//! The binding to the vendored libmagic (design §7.2, #1451;
//! `unsafe.homes`' `ext/*/src/ffi/`): the one home of this crate's
//! `unsafe`, over what `build.rs` compiles. A [`Magic`] is one
//! `struct magic_set`, closed when it is dropped; what libmagic reports
//! through php's `php_error_docref` is kept by the shim and handed out by
//! [`take_warning`] once a call returns.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr::NonNull;

// libpcre2-8, which the shim's PCRE glue calls, linked through the crate
// that builds it.
use pcre2_sys as _;

/// `struct magic_set`, opaque.
#[repr(C)]
struct MagicSet {
    _private: [u8; 0],
}

/// The shim's `php_stream`, opaque.
#[repr(C)]
struct ShimStream {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn magic_open(flags: c_int) -> *mut MagicSet;
    fn magic_close(ms: *mut MagicSet);
    fn magic_load(ms: *mut MagicSet, file: *const c_char) -> c_int;
    fn magic_buffer(ms: *mut MagicSet, buf: *const c_void, len: usize) -> *const c_char;
    fn magic_stream(ms: *mut MagicSet, stream: *mut ShimStream) -> *const c_char;
    fn magic_error(ms: *mut MagicSet) -> *const c_char;
    fn magic_errno(ms: *mut MagicSet) -> c_int;
    fn magic_getflags(ms: *mut MagicSet) -> c_int;
    fn magic_setflags(ms: *mut MagicSet, flags: c_int) -> c_int;
    fn magic_getparam(ms: *mut MagicSet, param: c_int, value: *mut c_void) -> c_int;
    fn magic_version() -> c_int;
    fn fileinfo_shim_stream(
        bytes: *const u8,
        len: usize,
        fd: c_int,
        mode: u32,
        size: i64,
    ) -> *mut ShimStream;
    fn php_stream_close(stream: *mut ShimStream) -> c_int;
    fn fileinfo_shim_take_warning(
        param: *mut c_char,
        param_len: usize,
        message: *mut c_char,
        message_len: usize,
    ) -> c_int;
}

/// `MAGIC_PARAM_BYTES_MAX`.
const PARAM_BYTES_MAX: c_int = 6;

/// `magic_version()`: `546` for libmagic 5.46.
pub(crate) fn version() -> i32 {
    // SAFETY: `magic_version` reads a constant.
    unsafe { magic_version() }
}

/// The bytes of a C string libmagic answered, `None` for its NULL.
fn answered(text: *const c_char) -> Option<Vec<u8>> {
    if text.is_null() {
        return None;
    }
    // SAFETY: libmagic answers a NUL-terminated string it owns, alive
    // until the next call on the same set, which this copy precedes.
    let text = unsafe { CStr::from_ptr(text) };
    Some(text.to_bytes().to_vec())
}

/// What the engine's stream holds for `magic_stream`: the bytes read from
/// it, the fd under it where it has one, which libmagic reads the file's
/// end through, and its stat's `st_mode` and `st_size`.
pub(crate) struct Read<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) fd: Option<i32>,
    pub(crate) mode: u32,
    pub(crate) size: i64,
}

/// One libmagic set, `magic_open`'s, `magic_close`d when dropped.
pub(crate) struct Magic(NonNull<MagicSet>);

// SAFETY: a `magic_set` is owned by the one value that holds it and touched
// only through `&mut self`; libmagic keeps no thread's state in it.
unsafe impl Send for Magic {}

impl Drop for Magic {
    fn drop(&mut self) {
        // SAFETY: the set is this value's, opened by `magic_open` and not
        // closed before.
        unsafe { magic_close(self.0.as_ptr()) }
    }
}

impl Magic {
    /// `magic_open(flags)`, `None` where libmagic refuses the flags.
    pub(crate) fn open(flags: i32) -> Option<Magic> {
        // SAFETY: `magic_open` takes a value and allocates the set.
        let ms = unsafe { magic_open(flags) };
        NonNull::new(ms).map(Magic)
    }

    /// `magic_load`: the database at `file`, the compiled-in one for
    /// `None`; `false` for php's `-1`.
    pub(crate) fn load(&mut self, file: Option<&CStr>) -> bool {
        let file = file.map_or(std::ptr::null(), CStr::as_ptr);
        // SAFETY: the set is live, and `file` is NULL or a C string that
        // outlives the call.
        unsafe { magic_load(self.0.as_ptr(), file) != -1 }
    }

    /// `magic_getflags`.
    pub(crate) fn flags(&mut self) -> i32 {
        // SAFETY: the set is live.
        unsafe { magic_getflags(self.0.as_ptr()) }
    }

    /// `magic_setflags`.
    pub(crate) fn set_flags(&mut self, flags: i32) {
        // SAFETY: the set is live. What it answers fails only for
        // `MAGIC_PRESERVE_ATIME` without `utime(3)`, as php notes.
        unsafe { magic_setflags(self.0.as_ptr(), flags) };
    }

    /// `MAGIC_PARAM_BYTES_MAX`: how much of a file libmagic reads.
    pub(crate) fn bytes_max(&mut self) -> usize {
        let mut value: usize = 0;
        // SAFETY: the set is live, and the parameter is a `size_t`
        // written through the pointer.
        unsafe { magic_getparam(self.0.as_ptr(), PARAM_BYTES_MAX, (&raw mut value).cast()) };
        value
    }

    /// `magic_buffer`: the description of `bytes`.
    pub(crate) fn buffer(&mut self, bytes: &[u8]) -> Option<Vec<u8>> {
        // SAFETY: the set is live and `bytes` outlives the call, which
        // reads `bytes.len()` of them.
        let text = unsafe { magic_buffer(self.0.as_ptr(), bytes.as_ptr().cast(), bytes.len()) };
        answered(text)
    }

    /// `magic_stream` over a stream the shim makes of what the engine's
    /// stream held.
    pub(crate) fn stream(&mut self, read: &Read<'_>) -> Option<Vec<u8>> {
        let fd = read.fd.unwrap_or(-1);
        // SAFETY: the shim keeps the pointer to `read.bytes`, which
        // outlives the stream closed below.
        let stream = unsafe {
            fileinfo_shim_stream(
                read.bytes.as_ptr(),
                read.bytes.len(),
                fd,
                read.mode,
                read.size,
            )
        };
        // SAFETY: the set is live and the stream the shim's, open; the
        // descriptor is the engine's stream's, which the caller holds
        // open for the call.
        let text = unsafe { magic_stream(self.0.as_ptr(), stream) };
        let answer = answered(text);
        // SAFETY: the stream is the shim's, closed once.
        unsafe { php_stream_close(stream) };
        answer
    }

    /// `magic_errno` and `magic_error`, as php's `Failed identify data`
    /// prints them.
    pub(crate) fn error(&mut self) -> (i32, String) {
        // SAFETY: the set is live.
        let errno = unsafe { magic_errno(self.0.as_ptr()) };
        // SAFETY: the set is live.
        let text = unsafe { magic_error(self.0.as_ptr()) };
        let text = answered(text).unwrap_or_default();
        (errno, String::from_utf8_lossy(&text).into_owned())
    }
}

/// The oldest diagnostic libmagic raised through the shim since the last
/// was taken: the path `php_error_docref1` names, where it names one, and
/// the message.
pub(crate) fn take_warning() -> Option<(Option<Vec<u8>>, String)> {
    let mut param = vec![0u8; 4096];
    let mut message = vec![0u8; 8192];
    // SAFETY: both buffers are this frame's and their lengths are passed.
    let kind = unsafe {
        fileinfo_shim_take_warning(
            param.as_mut_ptr().cast(),
            param.len(),
            message.as_mut_ptr().cast(),
            message.len(),
        )
    };
    let cut = |bytes: &[u8]| php_ext::coerce::c_str(bytes).to_vec();
    let text = String::from_utf8_lossy(&cut(&message)).into_owned();
    match kind {
        0 => Some((None, text)),
        1 => Some((Some(cut(&param)), text)),
        _ => None,
    }
}
