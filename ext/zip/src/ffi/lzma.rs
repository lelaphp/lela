//! liblzma, the keg's library the oracle's libzip loads, found through
//! `pkg-config liblzma` and linked from its archive (`build.rs`), for
//! `zip_algorithm_xz.c`'s four coders: the `.lzma` ("alone") encoder and
//! decoder for LZMA entries, and the `.xz` stream encoder and decoder for
//! XZ entries. The declarations are `lzma/base.h`'s, `lzma/container.h`'s
//! and `lzma/lzma12.h`'s, written out; the two structs' sizes are held to
//! the headers' by a test.

use std::ffi::{c_int, c_void};

/// `lzma_stream`, as `LZMA_STREAM_INIT` leaves it until a coder is set
/// up on it.
#[repr(C)]
struct Stream {
    next_in: *const u8,
    avail_in: usize,
    total_in: u64,
    next_out: *mut u8,
    avail_out: usize,
    total_out: u64,
    allocator: *const c_void,
    internal: *mut c_void,
    reserved_ptr: [*mut c_void; 4],
    seek_pos: u64,
    reserved_int2: u64,
    reserved_int3: usize,
    reserved_int4: usize,
    reserved_enum: [c_int; 2],
}

/// `lzma_options_lzma`, which `lzma_lzma_preset` fills and the encoders
/// copy; this crate never reads a field of it.
#[repr(C)]
struct Options {
    dict_size: u32,
    preset_dict: *const u8,
    preset_dict_size: u32,
    lc: u32,
    lp: u32,
    pb: u32,
    mode: c_int,
    nice_len: u32,
    mf: c_int,
    depth: u32,
    ext_flags: u32,
    ext_size_low: u32,
    ext_size_high: u32,
    reserved_int: [u32; 5],
    reserved_enum: [c_int; 4],
    reserved_ptr: [*mut c_void; 2],
}

/// `lzma_filter`.
#[repr(C)]
struct Filter {
    id: u64,
    options: *mut c_void,
}

/// `LZMA_FILTER_LZMA2`.
const FILTER_LZMA2: u64 = 0x21;
/// `LZMA_VLI_UNKNOWN`, which ends a filter chain.
const VLI_UNKNOWN: u64 = u64::MAX;
/// `LZMA_CHECK_CRC64`.
const CHECK_CRC64: c_int = 4;
/// `LZMA_CONCATENATED`.
const CONCATENATED: u32 = 0x08;
/// `LZMA_RUN` and `LZMA_FINISH`.
const RUN: c_int = 0;
const FINISH: c_int = 3;
/// `LZMA_WINDOW_LIMIT` as libzip's `CMakeLists.txt` defines it when no
/// limit is configured, the keg's build: `UINT64_MAX`, liblzma's memory
/// limit for a decoder.
const WINDOW_LIMIT: u64 = u64::MAX;

unsafe extern "C" {
    fn lzma_code(strm: *mut Stream, action: c_int) -> c_int;
    fn lzma_end(strm: *mut Stream);
    fn lzma_lzma_preset(options: *mut Options, preset: u32) -> u8;
    fn lzma_alone_encoder(strm: *mut Stream, options: *const Options) -> c_int;
    fn lzma_stream_encoder(strm: *mut Stream, filters: *const Filter, check: c_int) -> c_int;
    fn lzma_alone_decoder(strm: *mut Stream, memlimit: u64) -> c_int;
    fn lzma_stream_decoder(strm: *mut Stream, memlimit: u64, flags: u32) -> c_int;
}

/// `lzma_ret`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Code {
    Ok,
    StreamEnd,
    NoCheck,
    UnsupportedCheck,
    GetCheck,
    MemError,
    MemlimitError,
    FormatError,
    OptionsError,
    DataError,
    BufError,
    ProgError,
    SeekNeeded,
}

impl Code {
    fn of(code: c_int) -> Code {
        match code {
            0 => Code::Ok,
            1 => Code::StreamEnd,
            2 => Code::NoCheck,
            3 => Code::UnsupportedCheck,
            4 => Code::GetCheck,
            5 => Code::MemError,
            6 => Code::MemlimitError,
            7 => Code::FormatError,
            8 => Code::OptionsError,
            9 => Code::DataError,
            10 => Code::BufError,
            12 => Code::SeekNeeded,
            _ => Code::ProgError,
        }
    }

    fn status(code: c_int) -> Result<(), Code> {
        match Code::of(code) {
            Code::Ok => Ok(()),
            refused => Err(refused),
        }
    }
}

/// An `lzma_stream` with one of the four coders set up on it, ended with
/// its owner.
pub(crate) struct Lzma(Box<Stream>);

// SAFETY: the stream and the coder liblzma hangs off it are reached only
// through this value, which one request owns and serves on one thread at
// a time (ADR-0005); liblzma keeps no thread-local state.
unsafe impl Send for Lzma {}

impl Drop for Lzma {
    fn drop(&mut self) {
        // SAFETY: the stream is `LZMA_STREAM_INIT`'s or a coder's, which
        // `lzma_end` frees once, here, as liblzma asks even after an
        // initialisation that failed.
        unsafe { lzma_end(&raw mut *self.0) };
    }
}

impl Lzma {
    /// `LZMA_STREAM_INIT`.
    fn fresh() -> Lzma {
        Lzma(Box::new(Stream {
            next_in: std::ptr::null(),
            avail_in: 0,
            total_in: 0,
            next_out: std::ptr::null_mut(),
            avail_out: 0,
            total_out: 0,
            allocator: std::ptr::null(),
            internal: std::ptr::null_mut(),
            reserved_ptr: [std::ptr::null_mut(); 4],
            seek_pos: 0,
            reserved_int2: 0,
            reserved_int3: 0,
            reserved_int4: 0,
            reserved_enum: [0; 2],
        }))
    }

    /// `start` compressing: `lzma_lzma_preset(&opt, preset)`, then the
    /// alone encoder over it for LZMA, or the stream encoder over LZMA2
    /// with a CRC-64 for XZ.
    pub(crate) fn encoder(alone: bool, preset: u32) -> Result<Lzma, Code> {
        let mut options = Options {
            dict_size: 0,
            preset_dict: std::ptr::null(),
            preset_dict_size: 0,
            lc: 0,
            lp: 0,
            pb: 0,
            mode: 0,
            nice_len: 0,
            mf: 0,
            depth: 0,
            ext_flags: 0,
            ext_size_low: 0,
            ext_size_high: 0,
            reserved_int: [0; 5],
            reserved_enum: [0; 4],
            reserved_ptr: [std::ptr::null_mut(); 2],
        };
        // SAFETY: `options` is a whole `lzma_options_lzma`, which the
        // preset overwrites; libzip ignores its answer, as here.
        unsafe { lzma_lzma_preset(&raw mut options, preset) };
        let mut lzma = Lzma::fresh();
        let code = if alone {
            // SAFETY: a fresh stream and the options above, which the
            // encoder copies.
            unsafe { lzma_alone_encoder(&raw mut *lzma.0, &raw const options) }
        } else {
            let filters = [
                Filter {
                    id: FILTER_LZMA2,
                    options: (&raw mut options).cast(),
                },
                Filter {
                    id: VLI_UNKNOWN,
                    options: std::ptr::null_mut(),
                },
            ];
            // SAFETY: a fresh stream and a chain the unknown id ends,
            // which the encoder copies with its options.
            unsafe { lzma_stream_encoder(&raw mut *lzma.0, filters.as_ptr(), CHECK_CRC64) }
        };
        Code::status(code).map(|()| lzma)
    }

    /// `start` decompressing: the alone decoder for LZMA, or the stream
    /// decoder reading concatenated streams for XZ, each without a
    /// memory limit.
    pub(crate) fn decoder(alone: bool) -> Result<Lzma, Code> {
        let mut lzma = Lzma::fresh();
        let code = if alone {
            // SAFETY: a fresh stream.
            unsafe { lzma_alone_decoder(&raw mut *lzma.0, WINDOW_LIMIT) }
        } else {
            // SAFETY: a fresh stream.
            unsafe { lzma_stream_decoder(&raw mut *lzma.0, WINDOW_LIMIT, CONCATENATED) }
        };
        Code::status(code).map(|()| lzma)
    }

    /// One `lzma_code`, `LZMA_FINISH` where `finish`, else `LZMA_RUN`,
    /// reading `input` and writing `out`: what it answered, and how much
    /// of each it took and made.
    pub(crate) fn code(
        &mut self,
        input: &[u8],
        out: &mut [u8],
        finish: bool,
    ) -> (Code, usize, usize) {
        let s = &mut *self.0;
        s.next_in = input.as_ptr();
        s.avail_in = input.len();
        s.next_out = out.as_mut_ptr();
        s.avail_out = out.len();
        let action = if finish { FINISH } else { RUN };
        // SAFETY: a stream a coder was set up on, reading `input` and
        // writing `out`, which outlive the call; the pointers are cleared
        // after it, so liblzma keeps none.
        let code = unsafe { lzma_code(&raw mut *s, action) };
        let used = input.len() - s.avail_in;
        let made = out.len() - s.avail_out;
        s.next_in = std::ptr::null();
        s.avail_in = 0;
        s.next_out = std::ptr::null_mut();
        s.avail_out = 0;
        (Code::of(code), used, made)
    }
}

#[cfg(test)]
mod tests {
    use super::{Options, Stream};

    unsafe extern "C" {
        /// `sizeof(lzma_stream)` and `sizeof(lzma_options_lzma)` as the
        /// headers `build.rs` compiles against have them.
        static php_zip_lzma_stream_size: usize;
        static php_zip_lzma_options_size: usize;
    }

    /// The two structs laid out here are the headers' size.
    #[test]
    fn structs_are_the_headers_size() {
        // SAFETY: a constant `build.rs` compiled, read once.
        let stream = unsafe { php_zip_lzma_stream_size };
        // SAFETY: as above.
        let options = unsafe { php_zip_lzma_options_size };
        assert_eq!(stream, size_of::<Stream>());
        assert_eq!(options, size_of::<Options>());
    }
}
