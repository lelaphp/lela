//! The binding to bzip2 (design §7.2, `unsafe.homes`' `ext/*/src/ffi/`):
//! `libbz2-rs-sys`, libbzip2 ported to Rust, and the one home of this
//! crate's `unsafe`. php's tests observe the compressed bytes, and the
//! library writes the oracle's libbz2 1.0.8 bytes at every block size and
//! work factor (#1116's measurement), so no C library is linked.
//!
//! Four safe types over it, one `bz_stream` each after
//! `BZ2_bzCompressInit` or `BZ2_bzDecompressInit`: [`Compress`] and
//! [`Decompress`] with the state on the library's own allocator, as
//! `bzcompress`, `bzdecompress` and the file layer leave `bzalloc` null,
//! and [`LentCompress`] and [`LentDecompress`] with the state on the
//! request's heap, as `php_bz2_alloc` keeps a filter's (design §5.13,
//! ADR-0014). A `bz_stream` is boxed, since the library's state names the
//! stream it was made for, and each call hands the library the caller's
//! slices for that call alone, which is all it reads of `next_in` and
//! `next_out`.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::convert::Infallible;
use std::ffi::{c_int, c_uint, c_void};

use libbz2_rs_sys as bz;
use php_ext::raising::Thrown;
use php_ext::request::{Request, efree, emalloc};

/// What a bzip2 call answers, `BZ_OK` through `BZ_CONFIG_ERROR`: the
/// library's own, and `BZ_IO_ERROR` and `BZ_UNEXPECTED_EOF`, which
/// `bzlib.c`'s file layer answers and [`crate::stream`] ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Code {
    Ok,
    RunOk,
    FlushOk,
    FinishOk,
    StreamEnd,
    SequenceError,
    ParamError,
    MemError,
    DataError,
    DataErrorMagic,
    IoError,
    UnexpectedEof,
    OutbuffFull,
    ConfigError,
}

impl Code {
    /// The code the library answered; it answers none but these, and
    /// anything else reads as `BZ_PARAM_ERROR`, what it says of a stream
    /// it cannot read.
    fn of(code: c_int) -> Code {
        match code {
            bz::BZ_OK => Code::Ok,
            bz::BZ_RUN_OK => Code::RunOk,
            bz::BZ_FLUSH_OK => Code::FlushOk,
            bz::BZ_FINISH_OK => Code::FinishOk,
            bz::BZ_STREAM_END => Code::StreamEnd,
            bz::BZ_SEQUENCE_ERROR => Code::SequenceError,
            bz::BZ_MEM_ERROR => Code::MemError,
            bz::BZ_DATA_ERROR => Code::DataError,
            bz::BZ_DATA_ERROR_MAGIC => Code::DataErrorMagic,
            bz::BZ_IO_ERROR => Code::IoError,
            bz::BZ_UNEXPECTED_EOF => Code::UnexpectedEof,
            bz::BZ_OUTBUFF_FULL => Code::OutbuffFull,
            bz::BZ_CONFIG_ERROR => Code::ConfigError,
            _ => Code::ParamError,
        }
    }

    /// The number, as `bzcompress`, `bzdecompress` and `bzerrno` answer
    /// it.
    pub(crate) fn number(self) -> i64 {
        i64::from(match self {
            Code::Ok => bz::BZ_OK,
            Code::RunOk => bz::BZ_RUN_OK,
            Code::FlushOk => bz::BZ_FLUSH_OK,
            Code::FinishOk => bz::BZ_FINISH_OK,
            Code::StreamEnd => bz::BZ_STREAM_END,
            Code::SequenceError => bz::BZ_SEQUENCE_ERROR,
            Code::ParamError => bz::BZ_PARAM_ERROR,
            Code::MemError => bz::BZ_MEM_ERROR,
            Code::DataError => bz::BZ_DATA_ERROR,
            Code::DataErrorMagic => bz::BZ_DATA_ERROR_MAGIC,
            Code::IoError => bz::BZ_IO_ERROR,
            Code::UnexpectedEof => bz::BZ_UNEXPECTED_EOF,
            Code::OutbuffFull => bz::BZ_OUTBUFF_FULL,
            Code::ConfigError => bz::BZ_CONFIG_ERROR,
        })
    }
}

/// The action a compress call is made with, `BZ_RUN`, `BZ_FLUSH` or
/// `BZ_FINISH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Run,
    Flush,
    Finish,
}

impl Action {
    fn code(self) -> c_int {
        match self {
            Action::Run => bz::BZ_RUN,
            Action::Flush => bz::BZ_FLUSH,
            Action::Finish => bz::BZ_FINISH,
        }
    }
}

/// What one call did: its code, how much of the input it read and how
/// much it wrote.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Step {
    pub(crate) code: Code,
    pub(crate) read: usize,
    pub(crate) written: usize,
}

/// `sizeof(EState)` in the libbz2 1.0.8 php links, on its LP64 build:
/// what `BZ2_bzCompressInit` asks the allocator for first.
static ESTATE: usize = 55768;

/// `sizeof(DState)` in the same build, what `BZ2_bzDecompressInit` asks
/// for first. `libbz2-rs-sys` keeps `perm` and `mtfbase` as `u16`s where
/// C keeps `Int32`s, so its own asks a page less of the heap.
static DSTATE: usize = 64144;

/// The `opaque` a [`Lent`] stream is made with: the C size of the state its
/// init allocates, which [`alloc`] asks the heap for in place of the
/// Rust library's, so `memory_get_usage()` counts the pages php's libbz2 takes.
fn state_size(size: &'static usize) -> *mut c_void {
    std::ptr::from_ref(size).cast_mut().cast()
}

/// libbz2's `bzalloc` for a filter, `php_bz2_alloc`
/// (`Z/ext/bz2/bz2_filter.c:51-54`) of one that is not persistent:
/// `safe_pemalloc(items, size, 0, 0)`, `items * size` bytes of the heap
/// the call is lent, left as the heap finds them, as `safe_pemalloc`
/// leaves them. The library asks for no negative count, and two `int`s
/// multiply inside a 64-bit `size_t`, so php's overflow check never fires
/// for them. A single item is the stream's state, which the Rust library
/// asks for at its own size and C at the size `opaque` names
/// ([`state_size`]); the larger is asked of the heap.
unsafe extern "C" fn alloc(opaque: *mut c_void, items: c_int, size: c_int) -> *mut c_void {
    let count = |n: c_int| usize::try_from(n).unwrap_or(0);
    let mut len = count(items) * count(size);
    if items == 1 && !opaque.is_null() {
        // SAFETY: a lent stream's `opaque` is a `&'static usize`
        // ([`state_size`]), read only.
        len = len.max(unsafe { *opaque.cast::<usize>() });
    }
    emalloc(len)
}

/// libbz2's `bzfree` for a filter, `php_bz2_free`
/// (`Z/ext/bz2/bz2_filter.c:56-59`): `pefree(address, 0)`, under the lend
/// of the call the library is in.
unsafe extern "C" fn free(_opaque: *mut c_void, address: *mut c_void) {
    // SAFETY: the library frees a block once, and only one [`alloc`]
    // answered, so a block of the lent heap or, where the limit refused
    // one, of the system allocator.
    unsafe { efree(address) }
}

/// A `bz_stream` before its init, zeroed as php's `pecalloc` leaves one,
/// the library's own allocator in place; boxed, since the library's state
/// keeps the stream's address.
#[allow(clippy::unnecessary_box_returns)]
fn fresh() -> Box<bz::bz_stream> {
    Box::new(bz::bz_stream {
        next_in: std::ptr::null(),
        avail_in: 0,
        total_in_lo32: 0,
        total_in_hi32: 0,
        next_out: std::ptr::null_mut(),
        avail_out: 0,
        total_out_lo32: 0,
        total_out_hi32: 0,
        state: std::ptr::null_mut(),
        bzalloc: None,
        bzfree: None,
        opaque: std::ptr::null_mut(),
    })
}

/// A length the library's `unsigned int` holds, the rest left for the
/// next call.
fn avail(len: usize) -> c_uint {
    c_uint::try_from(len).unwrap_or(c_uint::MAX)
}

/// One call over `stream` with `input` and `output` for its length; the
/// `Err` is what `call` answered in place of a code.
fn step<E>(
    stream: &mut bz::bz_stream,
    input: &[u8],
    output: &mut [u8],
    call: impl FnOnce(*mut bz::bz_stream) -> Result<c_int, E>,
) -> Result<Step, E> {
    let avail_in = avail(input.len());
    let avail_out = avail(output.len());
    stream.next_in = input.as_ptr().cast();
    stream.avail_in = avail_in;
    stream.next_out = output.as_mut_ptr().cast();
    stream.avail_out = avail_out;
    let code = call(&raw mut *stream);
    let read = (avail_in - stream.avail_in) as usize;
    let written = (avail_out - stream.avail_out) as usize;
    stream.next_in = std::ptr::null();
    stream.avail_in = 0;
    stream.next_out = std::ptr::null_mut();
    stream.avail_out = 0;
    Ok(Step {
        code: Code::of(code?),
        read,
        written,
    })
}

/// A `bz_stream` after `BZ2_bzCompressInit`, ended by
/// `BZ2_bzCompressEnd` when dropped.
pub(crate) struct Compress(Box<bz::bz_stream>);

// SAFETY: the stream and the state the library hangs off it are reached
// only through this value, which one request owns and serves on one
// thread at a time (ADR-0005); the library keeps no thread-local state.
unsafe impl Send for Compress {}

impl Compress {
    /// `BZ2_bzCompressInit(&strm, block_size, 0, work_factor)`.
    pub(crate) fn new(block_size: i32, work_factor: i32) -> Result<Compress, Code> {
        let mut stream = fresh();
        // SAFETY: the stream is fresh, with no allocator of its own, which
        // the library reads as its default.
        let code = unsafe { bz::BZ2_bzCompressInit(&raw mut *stream, block_size, 0, work_factor) };
        match Code::of(code) {
            Code::Ok => Ok(Compress(stream)),
            refused => Err(refused),
        }
    }

    /// `BZ2_bzCompress(&strm, action)` over `input` into `output`.
    pub(crate) fn compress(&mut self, input: &[u8], output: &mut [u8], action: Action) -> Step {
        let action = action.code();
        let Ok(step) = step(&mut self.0, input, output, |s| {
            // SAFETY: the stream was initialised by BZ2_bzCompressInit,
            // and step points it at the two slices for this call alone.
            Ok::<_, Infallible>(unsafe { bz::BZ2_bzCompress(s, action) })
        });
        step
    }
}

impl Drop for Compress {
    fn drop(&mut self) {
        // SAFETY: the stream was initialised by BZ2_bzCompressInit and is
        // ended once, here.
        unsafe {
            bz::BZ2_bzCompressEnd(&raw mut *self.0);
        }
    }
}

/// A `bz_stream` after `BZ2_bzDecompressInit`, ended by
/// `BZ2_bzDecompressEnd` when dropped.
pub(crate) struct Decompress(Box<bz::bz_stream>);

// SAFETY: as for Compress: one owner, one thread at a time, no
// thread-local state in the library.
unsafe impl Send for Decompress {}

impl Decompress {
    /// `BZ2_bzDecompressInit(&strm, 0, small)`.
    pub(crate) fn new(small: bool) -> Result<Decompress, Code> {
        let mut stream = fresh();
        // SAFETY: the stream is fresh, with no allocator of its own, which
        // the library reads as its default.
        let code = unsafe { bz::BZ2_bzDecompressInit(&raw mut *stream, 0, c_int::from(small)) };
        match Code::of(code) {
            Code::Ok => Ok(Decompress(stream)),
            refused => Err(refused),
        }
    }

    /// `BZ2_bzDecompress(&strm)` over `input` into `output`.
    pub(crate) fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> Step {
        let Ok(step) = step(&mut self.0, input, output, |s| {
            // SAFETY: the stream was initialised by BZ2_bzDecompressInit,
            // and step points it at the two slices for this call alone.
            Ok::<_, Infallible>(unsafe { bz::BZ2_bzDecompress(s) })
        });
        step
    }
}

impl Drop for Decompress {
    fn drop(&mut self) {
        // SAFETY: the stream was initialised by BZ2_bzDecompressInit and
        // is ended once, here.
        unsafe {
            bz::BZ2_bzDecompressEnd(&raw mut *self.0);
        }
    }
}

/// A `bz_stream` whose state is on the request's heap, as a filter that
/// is not persistent keeps it (design §5.13): libbz2 allocates through
/// [`alloc`] and [`free`], and every call into it is made through
/// [`Lent::call`], which lends the heap for that call alone. It is ended
/// through [`LentCompress::end`] or [`LentDecompress::end`], which gives
/// its state back; one dropped without is left to the request's end,
/// whose heap takes what the library held, and the library is not called
/// again, since its state is in memory the heap may already have given
/// back.
struct Lent(Box<bz::bz_stream>);

// SAFETY: as for Compress: one owner, one thread at a time (ADR-0005).
unsafe impl Send for Lent {}

impl Lent {
    /// A stream before its init with the allocator a filter's is set to,
    /// `php_bz2_alloc` and `php_bz2_free`, and the C size of the state
    /// its init allocates as `opaque` ([`state_size`]).
    fn new(state: &'static usize) -> Lent {
        let mut stream = fresh();
        stream.bzalloc = Some(alloc);
        stream.bzfree = Some(free);
        stream.opaque = state_size(state);
        Lent(stream)
    }

    /// `call` over the stream with `req`'s heap lent for its length
    /// (ADR-0014), so what the library allocates and frees is the
    /// request's; the `Err` is the memory limit's fatal an allocation
    /// met, which ends the request as php's `emalloc` does.
    fn call(
        &mut self,
        req: &mut Request,
        call: impl FnOnce(*mut bz::bz_stream) -> c_int,
    ) -> Result<c_int, Thrown> {
        let stream = &raw mut *self.0;
        req.memory_mut().lend_heap(|| call(stream))
    }

    /// One step over `input` into `output` for their length alone.
    fn step(
        &mut self,
        req: &mut Request,
        input: &[u8],
        output: &mut [u8],
        call: impl FnOnce(*mut bz::bz_stream) -> c_int,
    ) -> Result<Step, Thrown> {
        step(&mut self.0, input, output, |s| {
            req.memory_mut().lend_heap(|| call(s))
        })
    }
}

/// A [`Lent`] stream after `BZ2_bzCompressInit`, as `php_bz2_filter_create`
/// makes a `bzip2.compress` filter's that is not persistent.
pub(crate) struct LentCompress(Lent);

impl LentCompress {
    /// `BZ2_bzCompressInit(&strm, block_size, 0, work_factor)`: the
    /// stream, or the library's refusal; the outer `Err` is the memory
    /// limit's fatal.
    pub(crate) fn new(
        req: &mut Request,
        block_size: i32,
        work_factor: i32,
    ) -> Result<Result<LentCompress, Code>, Thrown> {
        let mut stream = Lent::new(&ESTATE);
        // SAFETY: the stream is fresh with its allocator set.
        let code = stream.call(req, |s| unsafe {
            bz::BZ2_bzCompressInit(s, block_size, 0, work_factor)
        })?;
        Ok(match Code::of(code) {
            Code::Ok => Ok(LentCompress(stream)),
            refused => Err(refused),
        })
    }

    /// `BZ2_bzCompress(&strm, action)` over `input` into `output`.
    pub(crate) fn compress(
        &mut self,
        req: &mut Request,
        input: &[u8],
        output: &mut [u8],
        action: Action,
    ) -> Result<Step, Thrown> {
        let action = action.code();
        // SAFETY: the stream was initialised by BZ2_bzCompressInit, and
        // step points it at the two slices for this call alone.
        self.0.step(req, input, output, |s| unsafe {
            bz::BZ2_bzCompress(s, action)
        })
    }

    /// `BZ2_bzCompressEnd`: the state given back to the request's heap.
    pub(crate) fn end(mut self, req: &mut Request) {
        // SAFETY: an initialised stream, ended once, here, as `self` is
        // given up.
        self.0
            .call(req, |s| unsafe { bz::BZ2_bzCompressEnd(s) })
            .ok();
    }
}

/// A [`Lent`] stream after `BZ2_bzDecompressInit`, as a `bzip2.decompress`
/// filter that is not persistent makes one.
pub(crate) struct LentDecompress(Lent);

impl LentDecompress {
    /// `BZ2_bzDecompressInit(&strm, 0, small)`: the stream, or the
    /// library's refusal; the outer `Err` is the memory limit's fatal.
    pub(crate) fn new(
        req: &mut Request,
        small: bool,
    ) -> Result<Result<LentDecompress, Code>, Thrown> {
        let mut stream = Lent::new(&DSTATE);
        // SAFETY: the stream is fresh with its allocator set.
        let code = stream.call(req, |s| unsafe {
            bz::BZ2_bzDecompressInit(s, 0, c_int::from(small))
        })?;
        Ok(match Code::of(code) {
            Code::Ok => Ok(LentDecompress(stream)),
            refused => Err(refused),
        })
    }

    /// `BZ2_bzDecompress(&strm)` over `input` into `output`.
    pub(crate) fn decompress(
        &mut self,
        req: &mut Request,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<Step, Thrown> {
        // SAFETY: the stream was initialised by BZ2_bzDecompressInit, and
        // step points it at the two slices for this call alone.
        self.0
            .step(req, input, output, |s| unsafe { bz::BZ2_bzDecompress(s) })
    }

    /// `BZ2_bzDecompressEnd`: the state given back to the request's heap.
    pub(crate) fn end(mut self, req: &mut Request) {
        // SAFETY: an initialised stream, ended once, here, as `self` is
        // given up.
        self.0
            .call(req, |s| unsafe { bz::BZ2_bzDecompressEnd(s) })
            .ok();
    }
}
