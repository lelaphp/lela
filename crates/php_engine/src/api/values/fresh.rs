//! `zend_string_alloc` and `zend_string_truncate` (`Z/Zend/zend_string.h`):
//! a string a native writes in place before it hands it out.

use php_heap::FreshStr;

use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Owner, PhpString};

/// `zend_string_alloc(len, 0)` before the string is handed out: `len`
/// zero bytes on the request's heap, the holder's alone, written in place
/// as php writes `ZSTR_VAL` (what a C library writes there among them) and cut down
/// as `zend_string_truncate` cuts them, then [`FreshString::finish`]ed into
/// the [`PhpString`] the native returns, the same allocation. So
/// `memory_get_usage()` and `memory_get_peak_usage()` see the size php
/// asks for first and the bin the cut leaves, where a buffer copied into a
/// string would show neither. One given up unfinished goes back through
/// [`FreshString::free`], `zend_string_efree`; one the holder drops is the
/// request's end's.
///
/// ```php
/// $in = str_repeat("abcdefghij", 205);
/// $w = gzcompress($in);
/// memory_reset_peak_usage(); $m = memory_get_usage(); $c = gzcompress($in);
/// var_dump(memory_get_peak_usage() - $m, memory_get_usage() - $m);
/// ```
/// ```expect
/// int(403968)
/// int(80)
/// ```
#[derive(Debug)]
pub struct FreshString(FreshStr);

impl FreshString {
    /// `zend_string_alloc(len, 0)`; the `Err` is the memory limit's
    /// fatal, met before the string exists.
    pub fn new(req: &mut Request, len: usize) -> Result<FreshString, Thrown> {
        let fresh = req.heap_mut().string_fresh(len);
        req.fatal_of(fresh).map(FreshString)
    }

    /// The length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the length is zero.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The bytes, to read.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.0.bytes()
    }

    /// The bytes, to write.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.0.bytes_mut()
    }

    /// `zend_string_truncate(s, len, 0)`: cut to `len` bytes, in place
    /// where the size stays in its bin and moved to the smaller one
    /// otherwise, as `erealloc` cuts it; the `Err` is the memory limit's
    /// fatal a move met, and the string is the request's end's.
    pub fn truncate(self, req: &mut Request, len: usize) -> Result<FreshString, Thrown> {
        let cut = req.heap_mut().string_truncate(self.0, len);
        req.fatal_of(cut).map(FreshString)
    }

    /// `zend_string_extend(s, len, 0)`: grown to `len` bytes, in place
    /// where the bin or the pages after it allow and moved otherwise, as
    /// `erealloc` grows it, the bytes written so far kept and the new
    /// ones zero; the `Err` is the memory limit's fatal, and the string
    /// is the request's end's.
    pub fn extend(self, req: &mut Request, len: usize) -> Result<FreshString, Thrown> {
        let grown = req.heap_mut().string_extend(self.0, len);
        req.fatal_of(grown).map(FreshString)
    }

    /// `ZSTR_LEN(s) = len` and its NUL: the length cut where the
    /// allocation stands, keeping its size, where php hands a string out
    /// without `zend_string_truncate`.
    pub fn set_len(&mut self, len: usize) {
        self.0.set_len(len);
    }

    /// The string handed out, `RETURN_STR(s)`: one owner, the caller's.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn finish(self) -> Owner<PhpString> {
        Owner::adopt(PhpString::of(self.0.finish()))
    }

    /// `zend_string_efree(s)`: the allocation back to the heap.
    pub fn free(self, req: &mut Request) {
        req.heap_mut().release_string(self.0.finish());
    }
}
