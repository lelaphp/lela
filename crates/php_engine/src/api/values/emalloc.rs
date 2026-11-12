//! `emalloc` (`Z/Zend/zend_alloc.h`): the bytes a native keeps where
//! php keeps them in one `emalloc`'d allocation, on the request's heap
//! and counted there (design §5.13).

use php_heap::Block;

use crate::api::abi::EngineState;
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

    /// `ferealloc(ptr, len, true)` and its kin, as php 8.5.12 grows a
    /// buffer it keeps where the growth is refused: [`Emalloc::resize`],
    /// with the allocation handed back as it stood beside the `Err`, the
    /// refusal [`Memory::fallible`] carries out of its scope or the memory
    /// limit's fatal.
    ///
    /// [`Memory::fallible`]: crate::api::request::Memory::fallible
    pub(crate) fn try_resize(
        self,
        req: &mut Request,
        len: usize,
    ) -> Result<Emalloc, (Emalloc, Thrown)> {
        match req.heap_mut().block_try_resize(self.block, len) {
            Ok(block) => Ok(Emalloc { block }),
            Err((block, fatal)) => {
                let thrown = req.fatal_of::<()>(Err(fatal)).err();
                Err((Emalloc { block }, thrown.unwrap_or(Thrown::of(None))))
            }
        }
    }

    /// `efree(ptr)`: the allocation back to the heap.
    pub fn free(self, req: &mut Request) {
        req.heap_mut().free_block(self.block);
    }
}

/// `pemalloc(len, 1)`: `len` bytes of the system allocator, as php's
/// `__zend_malloc` holds a persistent allocation, which no request
/// counts or frees. Engine state by type, since no request's heap is
/// reachable from it, so what holds it outlives the request that made
/// it: a filter on a persistent stream's chain.
#[derive(Debug, EngineState)]
pub struct Malloc(Box<[u8]>);

/// One `pemalloc(len, persistent)` whose persistence is its type: an
/// [`Emalloc`] on the request's heap, or a [`Malloc`] of the system
/// allocator where the allocation is persistent. A native that php hands
/// `persistent` is generic over the one it holds and decides it once,
/// where it is handed it. What php `pemalloc`s for a stream's filter
/// takes the stream's persistence, so a filter on a `pfsockopen` stream
/// is counted nowhere.
///
/// ```php
/// $s = stream_socket_server('tcp://127.0.0.1:0');
/// [$host, $port] = explode(':', stream_socket_get_name($s, false));
/// $a = pfsockopen($host, $port);
/// $f = fopen('php://memory', 'w+');
/// foreach ([$f, $a] as $stream) {
///     $m = memory_get_usage(); $h = stream_filter_append($stream, 'bzip2.compress', STREAM_FILTER_READ); echo memory_get_usage() - $m, "\n";
/// }
/// ```
/// ```expect
/// 3531152
/// 32
/// ```
pub trait Allocation: Sized + Send + 'static {
    /// `pecalloc(1, len, persistent)`: `len` zero bytes; the `Err` is the
    /// memory limit's fatal.
    fn new(req: &mut Request, len: usize) -> Result<Self, Thrown>;

    /// The bytes, as written.
    fn bytes(&self) -> &[u8];

    /// The bytes, to write.
    fn bytes_mut(&mut self) -> &mut [u8];

    /// `pefree(ptr, persistent)`: the allocation back to where it came
    /// from.
    fn free(self, req: &mut Request);
}

impl Allocation for Emalloc {
    fn new(req: &mut Request, len: usize) -> Result<Emalloc, Thrown> {
        Emalloc::new(req, len)
    }

    fn bytes(&self) -> &[u8] {
        Emalloc::bytes(self)
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        Emalloc::bytes_mut(self)
    }

    fn free(self, req: &mut Request) {
        Emalloc::free(self, req);
    }
}

impl Allocation for Malloc {
    fn new(_req: &mut Request, len: usize) -> Result<Malloc, Thrown> {
        Ok(Malloc(vec![0; len].into_boxed_slice()))
    }

    fn bytes(&self) -> &[u8] {
        &self.0
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }

    /// What the system allocator holds goes back as it is dropped.
    fn free(self, _req: &mut Request) {}
}
