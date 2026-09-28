//! The binding to bzip2 (design §7.2, `unsafe.homes`' `ext/*/src/ffi/`):
//! `libbz2-rs-sys`, libbzip2 ported to Rust, and the one home of this
//! crate's `unsafe`. php's tests observe the compressed bytes, and the
//! library writes the oracle's libbz2 1.0.8 bytes at every block size and
//! work factor (#1116's measurement), so no C library is linked.
//!
//! Two safe types over it, [`Compress`] and [`Decompress`], one
//! `bz_stream` each after `BZ2_bzCompressInit` or `BZ2_bzDecompressInit`.
//! A `bz_stream` is boxed, since the library's state names the stream it
//! was made for, and each call hands the library the caller's slices for
//! that call alone, which is all it reads of `next_in` and `next_out`.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{c_int, c_uint};

use libbz2_rs_sys as bz;

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

/// One call over `stream` with `input` and `output` for its length.
fn step(
    stream: &mut bz::bz_stream,
    input: &[u8],
    output: &mut [u8],
    call: impl FnOnce(&mut bz::bz_stream) -> c_int,
) -> Step {
    let avail_in = avail(input.len());
    let avail_out = avail(output.len());
    stream.next_in = input.as_ptr().cast();
    stream.avail_in = avail_in;
    stream.next_out = output.as_mut_ptr().cast();
    stream.avail_out = avail_out;
    let code = Code::of(call(stream));
    let read = (avail_in - stream.avail_in) as usize;
    let written = (avail_out - stream.avail_out) as usize;
    stream.next_in = std::ptr::null();
    stream.avail_in = 0;
    stream.next_out = std::ptr::null_mut();
    stream.avail_out = 0;
    Step {
        code,
        read,
        written,
    }
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
        // SAFETY: the stream was initialised by BZ2_bzCompressInit, and
        // step points it at the two slices for this call alone.
        step(&mut self.0, input, output, |s| unsafe {
            bz::BZ2_bzCompress(s, action)
        })
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
        // SAFETY: the stream was initialised by BZ2_bzDecompressInit, and
        // step points it at the two slices for this call alone.
        step(&mut self.0, input, output, |s| unsafe {
            bz::BZ2_bzDecompress(s)
        })
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
