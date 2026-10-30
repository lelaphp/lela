//! libzstd, the keg's library the oracle's libzip loads, found through
//! `pkg-config libzstd` (`build.rs`), for `zip_algorithm_zstd.c`'s
//! streaming compressor and decompressor. The declarations are
//! `zstd.h`'s, written out.

use std::ffi::{c_int, c_uint, c_void};
use std::ops::RangeInclusive;
use std::ptr::NonNull;

/// `ZSTD_CStream` and `ZSTD_DStream`, which libzstd alone reads.
#[repr(C)]
struct CStream {
    _opaque: [u8; 0],
}

#[repr(C)]
struct DStream {
    _opaque: [u8; 0],
}

/// `ZSTD_inBuffer`.
#[repr(C)]
struct InBuffer {
    src: *const c_void,
    size: usize,
    pos: usize,
}

/// `ZSTD_outBuffer`.
#[repr(C)]
struct OutBuffer {
    dst: *mut c_void,
    size: usize,
    pos: usize,
}

unsafe extern "C" {
    fn ZSTD_createCStream() -> *mut CStream;
    fn ZSTD_freeCStream(zcs: *mut CStream) -> usize;
    fn ZSTD_initCStream(zcs: *mut CStream, level: c_int) -> usize;
    fn ZSTD_compressStream(zcs: *mut CStream, out: *mut OutBuffer, input: *mut InBuffer) -> usize;
    fn ZSTD_endStream(zcs: *mut CStream, out: *mut OutBuffer) -> usize;
    fn ZSTD_createDStream() -> *mut DStream;
    fn ZSTD_freeDStream(zds: *mut DStream) -> usize;
    fn ZSTD_decompressStream(zds: *mut DStream, out: *mut OutBuffer, input: *mut InBuffer)
    -> usize;
    fn ZSTD_isError(code: usize) -> c_uint;
    fn ZSTD_minCLevel() -> c_int;
    fn ZSTD_maxCLevel() -> c_int;
    fn ZSTD_compressBound(size: usize) -> usize;
}

/// A `size_t` libzstd answered that `ZSTD_isError` calls an error. libzip
/// hands the `size_t` itself to its `map_error`, whose cases are
/// `ZSTD_ErrorCode`s it never equals, so no reader asks which.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ZstdError;

/// `code`, or an error where `ZSTD_isError` says it is one.
fn checked(code: usize) -> Result<usize, ZstdError> {
    // SAFETY: `ZSTD_isError` reads its argument alone.
    if unsafe { ZSTD_isError(code) } == 0 {
        Ok(code)
    } else {
        Err(ZstdError)
    }
}

/// `ZSTD_minCLevel()..=ZSTD_maxCLevel()`.
pub(crate) fn levels() -> RangeInclusive<c_int> {
    // SAFETY: a constant of the library.
    let min = unsafe { ZSTD_minCLevel() };
    // SAFETY: as above.
    let max = unsafe { ZSTD_maxCLevel() };
    min..=max
}

/// `ZSTD_compressBound(size)`, an error code among its answers as
/// libzip returns it.
pub(crate) fn compress_bound(size: u64) -> u64 {
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    // SAFETY: a computation on its argument alone.
    let bound = unsafe { ZSTD_compressBound(size) };
    bound as u64
}

/// `input` and `out` as libzstd's buffers, one call of `call` over them,
/// and what it answered, took and made.
fn step(
    input: &[u8],
    out: &mut [u8],
    call: impl FnOnce(*mut OutBuffer, *mut InBuffer) -> usize,
) -> (Result<usize, ZstdError>, usize, usize) {
    let mut src = InBuffer {
        src: input.as_ptr().cast(),
        size: input.len(),
        pos: 0,
    };
    let mut dst = OutBuffer {
        dst: out.as_mut_ptr().cast(),
        size: out.len(),
        pos: 0,
    };
    let code = call(&raw mut dst, &raw mut src);
    (checked(code), src.pos, dst.pos)
}

/// `ZSTD_CStream` after `ZSTD_initCStream`, freed with its owner.
pub(crate) struct ZstdCompress(NonNull<CStream>);

// SAFETY: the stream is reached only through this value, which one
// request owns and serves on one thread at a time (ADR-0005); libzstd
// keeps no thread-local state.
unsafe impl Send for ZstdCompress {}

impl Drop for ZstdCompress {
    fn drop(&mut self) {
        // SAFETY: a stream `ZSTD_createCStream` made, freed once, here.
        unsafe { ZSTD_freeCStream(self.0.as_ptr()) };
    }
}

/// Why a stream was not made: `ZSTD_createCStream`'s null, or
/// `ZSTD_initCStream`'s error.
pub(crate) enum Refused {
    Memory,
    Init(ZstdError),
}

impl ZstdCompress {
    /// `start` compressing: `ZSTD_createCStream` and
    /// `ZSTD_initCStream(level)`.
    pub(crate) fn new(level: c_int) -> Result<ZstdCompress, Refused> {
        // SAFETY: allocates a stream or answers null.
        let stream = unsafe { ZSTD_createCStream() };
        let stream = ZstdCompress(NonNull::new(stream).ok_or(Refused::Memory)?);
        // SAFETY: a stream `ZSTD_createCStream` made.
        let code = unsafe { ZSTD_initCStream(stream.0.as_ptr(), level) };
        checked(code).map_err(Refused::Init)?;
        Ok(stream)
    }

    /// `ZSTD_compressStream` of `input` into `out`: its answer, and how
    /// much it took and made.
    pub(crate) fn compress(
        &mut self,
        input: &[u8],
        out: &mut [u8],
    ) -> (Result<usize, ZstdError>, usize, usize) {
        let zcs = self.0.as_ptr();
        step(input, out, |dst, src| {
            // SAFETY: an initialised stream and two buffers over slices
            // that outlive the call.
            unsafe { ZSTD_compressStream(zcs, dst, src) }
        })
    }

    /// `ZSTD_endStream` into `out`: its answer, what is still to flush,
    /// and how much it made.
    pub(crate) fn end(&mut self, out: &mut [u8]) -> (Result<usize, ZstdError>, usize) {
        let zcs = self.0.as_ptr();
        let (code, _, made) = step(&[], out, |dst, _| {
            // SAFETY: an initialised stream and a buffer over a slice that
            // outlives the call.
            unsafe { ZSTD_endStream(zcs, dst) }
        });
        (code, made)
    }
}

/// `ZSTD_DStream` after `ZSTD_createDStream`, freed with its owner.
pub(crate) struct ZstdDecompress(NonNull<DStream>);

// SAFETY: as for `ZstdCompress`: one owner, one thread at a time, no
// thread-local state in the library.
unsafe impl Send for ZstdDecompress {}

impl Drop for ZstdDecompress {
    fn drop(&mut self) {
        // SAFETY: a stream `ZSTD_createDStream` made, freed once, here.
        unsafe { ZSTD_freeDStream(self.0.as_ptr()) };
    }
}

impl ZstdDecompress {
    /// `start` decompressing: `ZSTD_createDStream`, `None` where it
    /// answers null.
    pub(crate) fn new() -> Option<ZstdDecompress> {
        // SAFETY: allocates a stream or answers null.
        let stream = unsafe { ZSTD_createDStream() };
        NonNull::new(stream).map(ZstdDecompress)
    }

    /// `ZSTD_decompressStream` of `input` into `out`: its answer, and how
    /// much it took and made.
    pub(crate) fn decompress(
        &mut self,
        input: &[u8],
        out: &mut [u8],
    ) -> (Result<usize, ZstdError>, usize, usize) {
        let zds = self.0.as_ptr();
        step(input, out, |dst, src| {
            // SAFETY: a stream `ZSTD_createDStream` made, which the first
            // call initialises, and two buffers over slices that outlive
            // the call.
            unsafe { ZSTD_decompressStream(zds, dst, src) }
        })
    }
}
