//! `smart_str` (`Z/Zend/zend_smart_str.c`): the bytes a native keeps
//! where php keeps them in a `smart_str`, on the request's heap and
//! counted there (design §5.13).

use std::ops::Range;

use php_heap::FreshStr;

use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Owner, PhpString};

/// `SMART_STR_OVERHEAD`: a `zend_string`'s header and its terminator.
const OVERHEAD: usize = 24 + 1;
/// `SMART_STR_START_LEN`: what the first allocation holds at least.
const START_LEN: usize = 256 - OVERHEAD;
/// `SMART_STR_PAGE`: what a larger allocation grows to a multiple of.
const PAGE: usize = 4096;

/// A `smart_str`: a `zend_string` on the request's heap whose
/// allocation, `a` bytes beside the header and terminator, runs ahead of
/// what it holds, grown as `smart_str_erealloc` grows it, to 231 bytes
/// first and then to the page past what is asked, so `memory_get_usage()`
/// and `memory_limit` see what php's see. Empty holds nothing. The bytes
/// go back to the heap through [`SmartStr::free`], where php calls
/// `smart_str_free`, or become the string a native returns through
/// [`SmartStr::finish`]; what a holder leaves is the request's end's.
///
/// ```php
/// $m = memory_get_usage();
/// output_add_rewrite_var('a', str_repeat('b', 300));
/// echo memory_get_usage() - $m >= 4096 ? "counted\n" : "not counted\n";
/// output_reset_rewrite_vars();
/// ```
/// ```expect
/// counted
/// ```
#[derive(Debug, Default)]
pub struct SmartStr {
    /// The string, as long as its allocation holds, `str->a`.
    buf: Option<FreshStr>,
    /// What it holds, `ZSTR_LEN(str->s)`.
    len: usize,
}

impl SmartStr {
    /// `smart_str_appendl`: `bytes` after what is held, the allocation
    /// laid down first where there is none and grown where what it would
    /// hold reaches its size (`smart_str_alloc`), even for no bytes; the
    /// `Err` is the memory limit's fatal.
    #[inline]
    pub fn append(&mut self, req: &mut Request, bytes: &[u8]) -> Result<(), Thrown> {
        let len = self.len.saturating_add(bytes.len());
        // What fits the allocation is copied in place, the common case.
        if let Some(buf) = &mut self.buf
            && len < buf.len()
            && let Some(room) = buf.bytes_mut().get_mut(self.len..len)
        {
            room.copy_from_slice(bytes);
            self.len = len;
            return Ok(());
        }
        self.append_grown(req, bytes)
    }

    /// `smart_str_alloc`: room for `more` bytes after what is held, the
    /// allocation laid down first where there is none and grown where
    /// what it would hold reaches its size, as `smart_str_erealloc`
    /// grows it, to 231 bytes first and then to the page past what is
    /// asked; nothing is appended. The `Err` is the memory limit's fatal.
    pub fn reserve(&mut self, req: &mut Request, more: usize) -> Result<(), Thrown> {
        let len = self.len.saturating_add(more);
        let room = self.buf.as_ref().map(FreshStr::len);
        if room.is_some_and(|room| len < room) {
            return Ok(());
        }
        let grown = if room.is_none() && len <= START_LEN {
            START_LEN
        } else {
            len.saturating_add(OVERHEAD).next_multiple_of(PAGE) - OVERHEAD
        };
        let buf = match self.buf.take() {
            None => req.heap_mut().string_fresh(grown, php_heap::Sizing::Alloc),
            Some(buf) => req.heap_mut().string_erealloc(buf, grown),
        };
        self.buf = Some(req.fatal_of(buf)?);
        Ok(())
    }

    /// [`SmartStr::append`] where the allocation is to be laid down or
    /// grown first.
    #[cold]
    fn append_grown(&mut self, req: &mut Request, bytes: &[u8]) -> Result<(), Thrown> {
        self.reserve(req, bytes.len())?;
        let len = self.len.saturating_add(bytes.len());
        if let Some(buf) = &mut self.buf {
            buf.bytes_mut()[self.len..len].copy_from_slice(bytes);
        }
        self.len = len;
        Ok(())
    }

    /// The bytes held.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.buf
            .as_ref()
            .map_or(&[][..], |buf| &buf.bytes()[..self.len])
    }

    /// The bytes of `range` taken out and those after moved down, the
    /// `memmove` php makes over `ZSTR_VAL(s)`; the allocation stays.
    pub fn remove(&mut self, range: Range<usize>) {
        let end = range.end.min(self.len);
        let start = range.start.min(end);
        if let Some(buf) = &mut self.buf {
            buf.bytes_mut().copy_within(end..self.len, start);
        }
        self.len -= end - start;
    }

    /// `ZSTR_LEN(s) = 0`: nothing held, the allocation kept.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// `smart_str_0(&buf)` and `buf.s` handed out, as
    /// `zend_print_zval_r_to_str` returns it: the string of what is held
    /// in the allocation as it stands, which `smart_str_extract` would
    /// trim and this does not, one owner the caller's; the empty string
    /// where nothing was ever appended.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn finish(mut self, req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
        let Some(mut buf) = self.buf.take() else {
            return PhpString::new(req, b"");
        };
        buf.set_len(self.len);
        Ok(Owner::adopt(PhpString::of(buf.finish())))
    }

    /// `smart_str_extract`: the string of what is held, trimmed first to
    /// its length as `smart_str_trim_to_size` reallocates it, so the
    /// string kept holds the bin of its bytes and not the buffer's; one
    /// owner the caller's, and `ZSTR_EMPTY_ALLOC()` where nothing was
    /// ever appended. The `Err` is the memory limit's fatal.
    ///
    /// ```php
    /// $a = [1, 'a' => 2];
    /// $m = memory_get_usage(); $s = var_export($a, true); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); memory_reset_peak_usage(); $s = var_export(range(1, 40), true); echo memory_get_peak_usage() - $m, ' ', memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 64
    /// 5688 448
    /// ```
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn extract(mut self, req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
        let Some(mut buf) = self.buf.take() else {
            return PhpString::new(req, b"");
        };
        if buf.len() > self.len {
            let cut = req.heap_mut().string_truncate(buf, self.len);
            buf = req.fatal_of(cut)?;
        }
        Ok(Owner::adopt(PhpString::of(buf.finish())))
    }

    /// `smart_str_free`: the allocation back to the heap, nothing held.
    pub fn free(&mut self, req: &mut Request) {
        if let Some(buf) = self.buf.take() {
            req.heap_mut().release_string(buf.finish());
        }
        self.len = 0;
    }
}
