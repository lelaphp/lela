//! `emalloc` (`Z/Zend/zend_alloc.h`): the bytes a native keeps where
//! php keeps them in one `emalloc`'d allocation, on the request's heap
//! and counted there (design §5.13).

use php_heap::Block;

use crate::api::raising::Thrown;
use crate::api::request::Request;

/// One `emalloc(len)` on the request's heap, with one owner: what php's
/// `create_object`, constructors and a C library's allocator callbacks
/// (`php_zlib_alloc`, uriparser's memory manager) ask `emalloc` for,
/// held where php holds the pointer, so `memory_get_usage()` and
/// `memory_limit` see what php's see. The bytes are zero until written,
/// as `ecalloc`'s are. They go back through [`Emalloc::free`] where php
/// calls `efree`; one the holder never frees is the request's end's, as
/// the heap's mass release takes it.
///
/// ```php
/// $c = hash_init('md5');
/// $m = memory_get_usage();
/// $d = hash_init('md5');
/// $n = memory_get_usage();
/// unset($d);
/// var_dump($n - $m, memory_get_usage() - $n);
/// ```
/// ```expect
/// int(256)
/// int(-256)
/// ```
#[derive(Debug)]
pub struct Emalloc {
    block: Block,
}

impl Emalloc {
    /// `ecalloc(1, len)`: `len` zero bytes; the `Err` is the memory
    /// limit's fatal.
    pub fn new(req: &mut Request, len: usize) -> Result<Emalloc, Thrown> {
        let block = req.heap_mut().block(len);
        Ok(Emalloc {
            block: req.fatal_of(block)?,
        })
    }

    /// The `char *` `spprintf` and `zend_vspprintf` answer for `len`
    /// bytes of text: a `smart_string` laid down at its 255-byte start,
    /// or grown to the page past the text and its terminator, the
    /// buffer [`crate::api::raising::throw_error`] holds. A native holds
    /// it where php holds a sentence it printed, such as the
    /// `warning_message` a callee hands back, writes the text into its
    /// first `len` bytes and frees it where php calls `efree`. The `Err`
    /// is the memory limit's fatal.
    pub fn printf_buffer(req: &mut Request, len: usize) -> Result<Emalloc, Thrown> {
        crate::api::raising::printf_buffer(req, len).map(|block| Emalloc { block })
    }

    /// The bytes asked for.
    #[must_use]
    pub fn len(&self) -> usize {
        self.block.len()
    }

    /// Whether none were asked for.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.block.is_empty()
    }

    /// The bytes, as written.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.block.bytes()
    }

    /// The bytes, to write.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.block.bytes_mut()
    }

    /// The first byte's address, what php's `emalloc` answers, for a C
    /// library the allocation is made for (`php_zlib_alloc`'s zlib,
    /// uriparser's memory manager), which writes it through the pointer
    /// and hands it back to be freed; making it makes no reference to
    /// the bytes.
    #[must_use]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.block.as_mut_ptr()
    }

    /// `erealloc(ptr, len)`: the bytes kept to the shorter of the two
    /// lengths and zero past them; the `Err` is the memory limit's
    /// fatal, and what was held is the request's end's.
    pub fn resize(self, req: &mut Request, len: usize) -> Result<Emalloc, Thrown> {
        let block = req.heap_mut().block_resize(self.block, len);
        Ok(Emalloc {
            block: req.fatal_of(block)?,
        })
    }

    /// `efree(ptr)`: the allocation back to the heap.
    pub fn free(self, req: &mut Request) {
        req.heap_mut().free_block(self.block);
    }
}
