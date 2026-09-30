//! `smart_str` (`Z/Zend/zend_smart_str.c`): the bytes a native keeps
//! where php keeps them in a `smart_str`, on the request's heap and
//! counted there (design §5.13).

use std::ops::Range;

use php_heap::Block;

use crate::api::raising::Thrown;
use crate::api::request::Request;

/// `_ZSTR_HEADER_SIZE`: a `zend_string`'s header, which each of the
/// allocations lays before the bytes.
const HEADER: usize = 24;
/// `SMART_STR_OVERHEAD`: the header and the terminator.
const OVERHEAD: usize = HEADER + 1;
/// `SMART_STR_START_LEN`: what the first allocation holds at least.
const START_LEN: usize = 256 - OVERHEAD;
/// `SMART_STR_PAGE`: what a larger allocation grows to a multiple of.
const PAGE: usize = 4096;

/// A `smart_str`: bytes on the request's heap, grown as
/// `smart_str_erealloc` grows them, to 231 bytes first and then to the
/// page past what is asked, each allocation a `zend_string`'s header and
/// terminator beside the bytes, so `memory_get_usage()` and
/// `memory_limit` see what php's see. Empty holds nothing. The bytes go
/// back to the heap through [`SmartStr::free`], where php calls
/// `smart_str_free`; what a holder leaves is the request's end's.
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
    block: Option<Block>,
    len: usize,
}

impl SmartStr {
    /// `smart_str_appendl`: `bytes` after what is held, the allocation
    /// grown first where they do not fit before its terminator; the
    /// `Err` is the memory limit's fatal.
    pub fn append(&mut self, req: &mut Request, bytes: &[u8]) -> Result<(), Thrown> {
        let len = self.len.saturating_add(bytes.len());
        let room = self
            .block
            .as_ref()
            .map_or(0, |block| block.len() - OVERHEAD);
        if self.block.is_none() || len >= room {
            let grown = if self.block.is_none() && len <= START_LEN {
                START_LEN
            } else {
                len.saturating_add(OVERHEAD).next_multiple_of(PAGE) - OVERHEAD
            };
            let size = HEADER + grown + 1;
            let block = match self.block.take() {
                None => req.heap_mut().block(size),
                Some(block) => req.heap_mut().block_resize(block, size),
            };
            self.block = Some(req.fatal_of(block)?);
        }
        if let Some(block) = &mut self.block {
            block.bytes_mut()[HEADER + self.len..HEADER + len].copy_from_slice(bytes);
        }
        self.len = len;
        Ok(())
    }

    /// The bytes held.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.block
            .as_ref()
            .map_or(&[][..], |block| &block.bytes()[HEADER..HEADER + self.len])
    }

    /// The bytes of `range` taken out and those after moved down, the
    /// `memmove` php makes over `ZSTR_VAL(s)`; the allocation stays.
    pub fn remove(&mut self, range: Range<usize>) {
        let end = range.end.min(self.len);
        let start = range.start.min(end);
        if let Some(block) = &mut self.block {
            block
                .bytes_mut()
                .copy_within(HEADER + end..HEADER + self.len, HEADER + start);
        }
        self.len -= end - start;
    }

    /// `ZSTR_LEN(s) = 0`: nothing held, the allocation kept.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// `smart_str_free`: the allocation back to the heap, nothing held.
    pub fn free(&mut self, req: &mut Request) {
        if let Some(block) = self.block.take() {
            req.heap_mut().free_block(block);
        }
        self.len = 0;
    }
}
