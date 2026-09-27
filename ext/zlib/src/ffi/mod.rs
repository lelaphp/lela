//! The binding to zlib (design §7.2, `unsafe.homes`' `ext/*/src/ffi/`):
//! the platform's library, linked through `libz-sys`, and the one home of
//! this crate's `unsafe`. php's tests observe the compressed bytes, and
//! those follow the linked library's algorithm, so the library is the
//! one the oracle links: macOS's `/usr/lib/libz.1.dylib`, 1.2.12, where
//! every level, strategy, window and memory level php exposes writes the
//! oracle's bytes (#950's measurement; zlib-rs and `miniz_oxide` do not).
//!
//! Three safe types over it: [`Deflate`] and [`Inflate`], one `z_stream`
//! each after `deflateInit2` or `inflateInit2`, and [`GzFile`], the
//! `gzFile` `php_stream_gzopen` reads and writes through. A `z_stream`
//! is boxed, since zlib's state names the stream it was made for, and
//! each call hands zlib the caller's slices for that call alone, which is
//! all zlib reads of `next_in` and `next_out`. zlib allocates through
//! [`alloc`] and [`free`], over Rust's allocator.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::alloc::Layout;
use std::ffi::{CStr, c_int, c_uint};
use std::os::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;

use libz_sys as z;

// SAFETY: zlib's own `zError`, which libz-sys does not declare, from the
// library it links: an `int` in, a pointer into a static table out.
unsafe extern "C" {
    fn zError(err: c_int) -> *const std::ffi::c_char;
}

/// What a zlib call answers, `Z_OK` through `Z_VERSION_ERROR`, the codes
/// `ZLIB_OK` and its kin name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Ok,
    StreamEnd,
    NeedDict,
    Errno,
    StreamError,
    DataError,
    MemError,
    BufError,
    VersionError,
}

impl Status {
    /// The code zlib answered; zlib answers none but these nine, and
    /// anything else reads as `Z_STREAM_ERROR`, what zlib says of a stream
    /// it cannot read.
    fn of(code: c_int) -> Status {
        match code {
            z::Z_OK => Status::Ok,
            z::Z_STREAM_END => Status::StreamEnd,
            z::Z_NEED_DICT => Status::NeedDict,
            z::Z_ERRNO => Status::Errno,
            z::Z_DATA_ERROR => Status::DataError,
            z::Z_MEM_ERROR => Status::MemError,
            z::Z_BUF_ERROR => Status::BufError,
            z::Z_VERSION_ERROR => Status::VersionError,
            _ => Status::StreamError,
        }
    }

    /// The code, as `inflate_get_status` answers it.
    pub(crate) fn code(self) -> i64 {
        i64::from(match self {
            Status::Ok => z::Z_OK,
            Status::StreamEnd => z::Z_STREAM_END,
            Status::NeedDict => z::Z_NEED_DICT,
            Status::Errno => z::Z_ERRNO,
            Status::StreamError => z::Z_STREAM_ERROR,
            Status::DataError => z::Z_DATA_ERROR,
            Status::MemError => z::Z_MEM_ERROR,
            Status::BufError => z::Z_BUF_ERROR,
            Status::VersionError => z::Z_VERSION_ERROR,
        })
    }

    /// `zError(code)`: the library's own text, `data error` and its kin.
    pub(crate) fn message(self) -> String {
        let code = c_int::try_from(self.code()).unwrap_or(z::Z_STREAM_ERROR);
        // SAFETY: zError takes any code of the nine and answers a pointer
        // into the library's static `z_errmsg` table.
        let text = unsafe { zError(code) };
        if text.is_null() {
            return String::new();
        }
        // SAFETY: the table's entries are NUL-terminated C strings that
        // live as long as the library.
        let text = unsafe { CStr::from_ptr(text) };
        text.to_string_lossy().into_owned()
    }
}

/// The flush a `deflate` or `inflate` call is made with, `Z_NO_FLUSH`
/// through `Z_BLOCK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flush {
    None,
    Partial,
    Sync,
    Full,
    Finish,
    Block,
}

impl Flush {
    /// The flush a program named by its `ZLIB_*` value, `None` for a
    /// number that is none of the six.
    pub(crate) fn of(mode: i64) -> Option<Flush> {
        Some(match c_int::try_from(mode).ok()? {
            z::Z_NO_FLUSH => Flush::None,
            z::Z_PARTIAL_FLUSH => Flush::Partial,
            z::Z_SYNC_FLUSH => Flush::Sync,
            z::Z_FULL_FLUSH => Flush::Full,
            z::Z_FINISH => Flush::Finish,
            z::Z_BLOCK => Flush::Block,
            _ => return None,
        })
    }

    fn code(self) -> c_int {
        match self {
            Flush::None => z::Z_NO_FLUSH,
            Flush::Partial => z::Z_PARTIAL_FLUSH,
            Flush::Sync => z::Z_SYNC_FLUSH,
            Flush::Full => z::Z_FULL_FLUSH,
            Flush::Finish => z::Z_FINISH,
            Flush::Block => z::Z_BLOCK,
        }
    }
}

/// What one call did: its status, how much of the input it read and how
/// much it wrote.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Step {
    pub(crate) status: Status,
    pub(crate) read: usize,
    pub(crate) written: usize,
}

/// `zlibVersion()`: the linked library's version, `1.2.12` on the
/// oracle's machine.
pub(crate) fn version() -> String {
    // SAFETY: zlibVersion answers a pointer to the library's static
    // ZLIB_VERSION string.
    let text = unsafe { z::zlibVersion() };
    if text.is_null() {
        return String::new();
    }
    // SAFETY: that string is NUL-terminated and lives as long as the
    // library.
    let text = unsafe { CStr::from_ptr(text) };
    text.to_string_lossy().into_owned()
}

/// The size of the header [`alloc`] keeps before each block, where the
/// block's length lives for [`free`].
const HEADER: usize = 16;

/// zlib's `zalloc`: `items * size` zeroed bytes from Rust's allocator,
/// as `php_zlib_alloc` takes them from php's, null where they cannot be.
extern "C" fn alloc(_opaque: z::voidpf, items: z::uInt, size: z::uInt) -> z::voidpf {
    let Some(len) = (items as usize)
        .checked_mul(size as usize)
        .and_then(|n| n.checked_add(HEADER))
    else {
        return std::ptr::null_mut();
    };
    let Ok(layout) = Layout::from_size_align(len, HEADER) else {
        return std::ptr::null_mut();
    };
    // SAFETY: the layout has a non-zero size, HEADER at least.
    let base = unsafe { std::alloc::alloc_zeroed(layout) };
    if base.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the block holds HEADER bytes, aligned to HEADER, before
    // what zlib is handed; HEADER's alignment is usize's and more.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe {
        base.cast::<usize>().write(len);
    }
    // SAFETY: HEADER is within the block just allocated.
    unsafe { base.add(HEADER).cast() }
}

/// zlib's `zfree`: gives back a block [`alloc`] handed out.
extern "C" fn free(_opaque: z::voidpf, address: z::voidpf) {
    if address.is_null() {
        return;
    }
    // SAFETY: every address zlib frees is one alloc answered, HEADER bytes
    // past the start of its block.
    let base = unsafe { address.cast::<u8>().sub(HEADER) };
    // SAFETY: alloc wrote the block's length at its start, aligned to
    // HEADER.
    #[allow(clippy::cast_ptr_alignment)]
    let len = unsafe { base.cast::<usize>().read() };
    if let Ok(layout) = Layout::from_size_align(len, HEADER) {
        // SAFETY: the block was allocated with this layout by alloc.
        unsafe { std::alloc::dealloc(base, layout) };
    }
}

/// A `z_stream` before its init, with the allocator set and nothing
/// else, as `memset` then `Z.zalloc = php_zlib_alloc` leaves one; boxed,
/// since zlib's state keeps the stream's address.
#[allow(clippy::unnecessary_box_returns)]
fn fresh() -> Box<z::z_stream> {
    Box::new(z::z_stream {
        next_in: std::ptr::null_mut(),
        avail_in: 0,
        total_in: 0,
        next_out: std::ptr::null_mut(),
        avail_out: 0,
        total_out: 0,
        msg: std::ptr::null_mut(),
        state: std::ptr::null_mut(),
        zalloc: alloc,
        zfree: free,
        opaque: std::ptr::null_mut(),
        data_type: 0,
        adler: 0,
        reserved: 0,
    })
}

/// `sizeof(z_stream)`, which the init calls check against the library's.
fn stream_size() -> c_int {
    c_int::try_from(std::mem::size_of::<z::z_stream>()).unwrap_or(c_int::MAX)
}

/// A length zlib's `uInt` holds, the rest left for the next call.
fn avail(len: usize) -> c_uint {
    c_uint::try_from(len).unwrap_or(c_uint::MAX)
}

/// One call over `stream` with `input` and `output` for its length.
fn step(
    stream: &mut z::z_stream,
    input: &[u8],
    output: &mut [u8],
    call: impl FnOnce(&mut z::z_stream) -> c_int,
) -> Step {
    let avail_in = avail(input.len());
    let avail_out = avail(output.len());
    stream.next_in = input.as_ptr().cast_mut();
    stream.avail_in = avail_in;
    stream.next_out = output.as_mut_ptr();
    stream.avail_out = avail_out;
    let status = Status::of(call(stream));
    let read = (avail_in - stream.avail_in) as usize;
    let written = (avail_out - stream.avail_out) as usize;
    stream.next_in = std::ptr::null_mut();
    stream.avail_in = 0;
    stream.next_out = std::ptr::null_mut();
    stream.avail_out = 0;
    Step {
        status,
        read,
        written,
    }
}

/// A `z_stream` after `deflateInit2`, ended by `deflateEnd` when dropped.
pub(crate) struct Deflate(Box<z::z_stream>);

// SAFETY: the stream and the state zlib hangs off it are reached only
// through this value, which one request owns and serves on one thread at
// a time (ADR-0005); zlib keeps no thread-local state.
unsafe impl Send for Deflate {}

impl Deflate {
    /// `deflateInit2(&Z, level, Z_DEFLATED, window, memory, strategy)`.
    pub(crate) fn new(
        level: i64,
        window: i64,
        memory: i64,
        strategy: i64,
    ) -> Result<Deflate, Status> {
        let int = |n: i64| c_int::try_from(n).map_err(|_| Status::StreamError);
        let (level, window, memory, strategy) =
            (int(level)?, int(window)?, int(memory)?, int(strategy)?);
        let mut stream = fresh();
        // SAFETY: zlibVersion answers the library's static version string.
        let version = unsafe { z::zlibVersion() };
        // SAFETY: the stream is fresh with its allocator set, and the
        // version and size are the library's own and the struct's.
        let code = unsafe {
            z::deflateInit2_(
                &raw mut *stream,
                level,
                z::Z_DEFLATED,
                window,
                memory,
                strategy,
                version,
                stream_size(),
            )
        };
        match Status::of(code) {
            Status::Ok => Ok(Deflate(stream)),
            refused => Err(refused),
        }
    }

    /// `deflate(&Z, flush)` over `input` into `output`.
    pub(crate) fn deflate(&mut self, input: &[u8], output: &mut [u8], flush: Flush) -> Step {
        let flush = flush.code();
        // SAFETY: the stream was initialised by deflateInit2, and step
        // points it at the two slices for this call alone.
        step(&mut self.0, input, output, |s| unsafe {
            z::deflate(s, flush)
        })
    }

    /// `deflateSetDictionary`.
    pub(crate) fn set_dictionary(&mut self, dictionary: &[u8]) -> Status {
        let stream: *mut z::z_stream = &raw mut *self.0;
        // SAFETY: an initialised stream, and the dictionary read for the
        // length given during the call alone.
        Status::of(unsafe {
            z::deflateSetDictionary(stream, dictionary.as_ptr(), avail(dictionary.len()))
        })
    }

    /// `deflateReset`.
    pub(crate) fn reset(&mut self) {
        let stream: *mut z::z_stream = &raw mut *self.0;
        // SAFETY: an initialised stream.
        unsafe { z::deflateReset(stream) };
    }
}

impl Drop for Deflate {
    fn drop(&mut self) {
        let stream: *mut z::z_stream = &raw mut *self.0;
        // SAFETY: an initialised stream, ended once, here.
        unsafe { z::deflateEnd(stream) };
    }
}

/// A `z_stream` after `inflateInit2`, ended by `inflateEnd` when dropped.
pub(crate) struct Inflate(Box<z::z_stream>);

// SAFETY: as Deflate's: one owner, one thread at a time (ADR-0005).
unsafe impl Send for Inflate {}

impl Inflate {
    /// `inflateInit2(&Z, window)`.
    pub(crate) fn new(window: i64) -> Result<Inflate, Status> {
        let window = c_int::try_from(window).map_err(|_| Status::StreamError)?;
        let mut stream = fresh();
        // SAFETY: zlibVersion answers the library's static version string.
        let version = unsafe { z::zlibVersion() };
        // SAFETY: the stream is fresh with its allocator set, and the
        // version and size are the library's own and the struct's.
        let code = unsafe { z::inflateInit2_(&raw mut *stream, window, version, stream_size()) };
        match Status::of(code) {
            Status::Ok => Ok(Inflate(stream)),
            refused => Err(refused),
        }
    }

    /// `inflate(&Z, flush)` over `input` into `output`.
    pub(crate) fn inflate(&mut self, input: &[u8], output: &mut [u8], flush: Flush) -> Step {
        let flush = flush.code();
        // SAFETY: the stream was initialised by inflateInit2, and step
        // points it at the two slices for this call alone.
        step(&mut self.0, input, output, |s| unsafe {
            z::inflate(s, flush)
        })
    }

    /// `inflateSetDictionary`.
    pub(crate) fn set_dictionary(&mut self, dictionary: &[u8]) -> Status {
        let stream: *mut z::z_stream = &raw mut *self.0;
        // SAFETY: an initialised stream, and the dictionary read for the
        // length given during the call alone.
        Status::of(unsafe {
            z::inflateSetDictionary(stream, dictionary.as_ptr(), avail(dictionary.len()))
        })
    }

    /// `inflateReset`.
    pub(crate) fn reset(&mut self) {
        let stream: *mut z::z_stream = &raw mut *self.0;
        // SAFETY: an initialised stream.
        unsafe { z::inflateReset(stream) };
    }

    /// `Z.total_in`: every byte read since the init or the last reset.
    pub(crate) fn total_in(&self) -> u64 {
        #[allow(clippy::useless_conversion)] // `uLong` is `u64` on the 64-bit targets
        u64::from(self.0.total_in)
    }
}

impl Drop for Inflate {
    fn drop(&mut self) {
        let stream: *mut z::z_stream = &raw mut *self.0;
        // SAFETY: an initialised stream, ended once, here.
        unsafe { z::inflateEnd(stream) };
    }
}

/// A `gzFile` from `gzdopen`, closed by `gzclose` when dropped.
pub(crate) struct GzFile(NonNull<z::gzFile_s>);

// SAFETY: the gzFile and the descriptor it owns are reached only through
// this value, which one request owns (ADR-0005).
unsafe impl Send for GzFile {}

impl GzFile {
    /// `gzdopen(dup(fd), mode)`, the `mode` read up to its first NUL as the
    /// C string php hands on; `None` where zlib refused it.
    pub(crate) fn dopen(fd: RawFd, mode: &[u8]) -> Option<GzFile> {
        let end = mode.iter().position(|b| *b == 0).unwrap_or(mode.len());
        let mut mode = mode[..end].to_vec();
        mode.push(0);
        // SAFETY: the descriptor belongs to the open stream the caller
        // holds, which outlives this call.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let raw = borrowed.try_clone_to_owned().ok()?.into_raw_fd();
        // SAFETY: a descriptor this call owns and a NUL-terminated mode.
        let file = unsafe { z::gzdopen(raw, mode.as_ptr().cast()) };
        if let Some(file) = NonNull::new(file) {
            return Some(GzFile(file));
        }
        // SAFETY: gzdopen took nothing on failure, so the duplicate is
        // still this call's to close.
        drop(unsafe { OwnedFd::from_raw_fd(raw) });
        None
    }

    /// `gzread`: how many bytes it gave, negative for an error.
    pub(crate) fn read(&mut self, buf: &mut [u8]) -> i64 {
        // SAFETY: an open gzFile, writing at most the length given.
        i64::from(unsafe { z::gzread(self.0.as_ptr(), buf.as_mut_ptr().cast(), avail(buf.len())) })
    }

    /// `gzwrite`: how many bytes it took, zero or less for an error.
    pub(crate) fn write(&mut self, buf: &[u8]) -> i64 {
        // SAFETY: an open gzFile, reading at most the length given.
        i64::from(unsafe { z::gzwrite(self.0.as_ptr(), buf.as_ptr().cast(), avail(buf.len())) })
    }

    /// `gzeof`.
    pub(crate) fn eof(&mut self) -> bool {
        // SAFETY: an open gzFile.
        unsafe { z::gzeof(self.0.as_ptr()) != 0 }
    }

    /// `gzseek`: the new offset, negative where it failed.
    #[allow(irrefutable_let_patterns, clippy::useless_conversion)] // `z_off_t` is `long`, `i64` on the 64-bit targets
    pub(crate) fn seek(&mut self, offset: i64, whence: i32) -> i64 {
        let Ok(offset) = z::z_off_t::try_from(offset) else {
            return -1;
        };
        // SAFETY: an open gzFile.
        i64::from(unsafe { z::gzseek(self.0.as_ptr(), offset, whence) })
    }

    /// `gzflush(file, Z_SYNC_FLUSH)`: `true` for `Z_OK`.
    pub(crate) fn flush(&mut self) -> bool {
        // SAFETY: an open gzFile.
        unsafe { z::gzflush(self.0.as_ptr(), z::Z_SYNC_FLUSH) == z::Z_OK }
    }

    /// `gzsetparams(file, level, Z_DEFAULT_STRATEGY)`: `true` for `Z_OK`.
    pub(crate) fn set_level(&mut self, level: i64) -> bool {
        let Ok(level) = c_int::try_from(level) else {
            return false;
        };
        // SAFETY: an open gzFile.
        unsafe { z::gzsetparams(self.0.as_ptr(), level, z::Z_DEFAULT_STRATEGY) == z::Z_OK }
    }

    /// `gzerror`'s number: whether the last error was `Z_ERRNO`, a
    /// system call's, which php reports with `errno`.
    pub(crate) fn failed_errno(&mut self) -> bool {
        let mut code: c_int = 0;
        // SAFETY: an open gzFile, and a number for gzerror to write.
        unsafe { z::gzerror(self.0.as_ptr(), &raw mut code) };
        code == z::Z_ERRNO
    }
}

impl Drop for GzFile {
    fn drop(&mut self) {
        // SAFETY: an open gzFile, closed once, here, with the duplicate
        // descriptor it owns.
        unsafe { z::gzclose(self.0.as_ptr()) };
    }
}
