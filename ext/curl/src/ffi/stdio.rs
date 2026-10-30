//! The `FILE *` `php_stream_cast(PHP_STREAM_AS_STDIO)` makes of a stream
//! for `CURLOPT_STDERR` (`Z/main/streams/cast.c:194`): `fdopen` over a
//! duplicate of an unfiltered plain file's fd, and for any other stream,
//! a `php://memory` or `php://temp` among them, the cookie `FILE *` php
//! opens with `fopencookie`, or `funopen` where the C library has that,
//! unbuffered as php's `funopen` makes it, whose writes are the stream's.
//!
//! **When a cookie writes.** libcurl writes its verbose lines with
//! `fwrite`, on the thread that called into it, inside the calls
//! [`Session::enter`] makes (`curl_easy_perform`, `curl_multi_perform`,
//! `curl_easy_cleanup` and the rest), which leave the request where the
//! cookie finds it: the write is `php_stream_write` through that request,
//! as php's `stream_cookie_writer` is. Outside such a call, or while a
//! callback's fatal is pending, the library reaches no request and the
//! cookie writes nothing, answering the failure libcurl ignores. PHP code
//! runs there only where the stream's own kind runs it, a user-space
//! wrapper's `stream_write` or a user filter, as it does in php; a memory
//! or temp stream runs none.
//!
//! **What the stream outlives.** The cookie names the stream without
//! owning it, as php's cookie names its `php_stream *`: the `CurlHandle`
//! whose `CURL *` holds the file holds the stream's value
//! (`handlers.std_err`) for as long as the library may write to it, and
//! every place that lets that value go points the library elsewhere first.
//! A file let go of during a call may be one the library is writing to,
//! whose write ran PHP code that set the option again, so it is kept to
//! the link's end ([`Session::let_file_go`]) and never closed under it.

use std::ffi::{c_char, c_int, c_void};
use std::os::fd::RawFd;
use std::ptr::{self, NonNull};

use php_ext::request::Stream;

use super::{Lent, Session, slice};

/// A `FILE *` for `CURLOPT_STDERR`, closed by its one owner.
pub(crate) struct CFile {
    file: NonNull<libc::FILE>,
    over: Over,
}

/// What a [`CFile`] writes to.
#[derive(Clone, Copy)]
enum Over {
    /// Its own duplicate of a plain file's fd.
    Fd,
    /// The stream, through a cookie.
    Stream(Stream),
}

/// A cookie `FILE *`'s argument: the request's link, where the request of
/// the call in progress is found, and the stream it writes to.
struct Cookie {
    link: Lent,
    stream: Stream,
}

// SAFETY: one request's, held by one `CURL *` or by the request's link,
// and written to only on the thread serving that request, which moves
// between threads only whole (ADR-0005).
unsafe impl Send for CFile {}

impl CFile {
    /// What php's cast makes of `stream`: `fdopen` over `fd`, the fd
    /// [`Cast::Stdio`](php_ext::request::Cast::Stdio) answered, or a
    /// cookie where it answered none or the fd would not open.
    pub(crate) fn of(session: &Session, stream: Stream, fd: Option<RawFd>) -> Option<CFile> {
        fd.and_then(CFile::open)
            .or_else(|| CFile::cookie(session.lent(), stream))
    }

    /// `fdopen(dup(fd), "a")`, `None` where either fails.
    fn open(fd: RawFd) -> Option<CFile> {
        // SAFETY: `dup` of a descriptor the stream holds open.
        let own = unsafe { libc::dup(fd) };
        if own < 0 {
            return None;
        }
        // SAFETY: a descriptor this call owns and a NUL-terminated mode.
        let file = unsafe { libc::fdopen(own, c"a".as_ptr()) };
        if let Some(file) = NonNull::new(file) {
            return Some(CFile {
                file,
                over: Over::Fd,
            });
        }
        // SAFETY: the descriptor `dup` made, which nothing else holds.
        unsafe { libc::close(own) };
        None
    }

    /// The cookie `FILE *` over `stream`, unbuffered: what the library
    /// writes reaches the stream at once, as php's `funopen` emulation
    /// of `fopencookie` sets it (GH-11078).
    fn cookie(link: Lent, stream: Stream) -> Option<CFile> {
        let cookie = Box::into_raw(Box::new(Cookie { link, stream }));
        let Some(file) = NonNull::new(open_cookie(cookie.cast())) else {
            // SAFETY: the box made above, which the library did not take.
            drop(unsafe { Box::from_raw(cookie) });
            return None;
        };
        // SAFETY: an open `FILE` nothing has written to yet.
        unsafe { libc::setvbuf(file.as_ptr(), ptr::null_mut(), libc::_IONBF, 0) };
        Some(CFile {
            file,
            over: Over::Stream(stream),
        })
    }

    pub(crate) fn as_ptr(&self) -> *mut libc::FILE {
        self.file.as_ptr()
    }

    /// `fflush`.
    pub(crate) fn flush(&self) {
        // SAFETY: an open `FILE` this value owns.
        unsafe { libc::fflush(self.file.as_ptr()) };
    }

    /// The same file opened again for a copy of the `CURL *`, which
    /// writes to it and flushes it as its own: what the original wrote
    /// flushed first, so the two keep their order.
    pub(crate) fn reopen(&self, session: &Session) -> Option<CFile> {
        self.flush();
        match self.over {
            Over::Fd => {
                // SAFETY: an open `FILE` this value owns.
                let fd = unsafe { libc::fileno(self.file.as_ptr()) };
                CFile::open(fd)
            }
            Over::Stream(stream) => CFile::cookie(session.lent(), stream),
        }
    }
}

impl Drop for CFile {
    fn drop(&mut self) {
        // SAFETY: the `FILE` `open` or `cookie` made, closed once by its
        // one owner; a cookie's is freed by `cookie_close`.
        unsafe { libc::fclose(self.file.as_ptr()) };
    }
}

/// `stream_cookie_writer`: `php_stream_write` through the request of the
/// call in progress, `None` for php's `-1` and where there is none.
fn cookie_write(cookie: *mut c_void, bytes: &[u8]) -> Option<usize> {
    // SAFETY: the box `CFile::cookie` made, freed only by `cookie_close`,
    // which no write is inside; both fields are copied out, so nothing
    // borrows the box across the write, which may run PHP code.
    let (link, stream) = unsafe {
        let cookie = &*cookie.cast::<Cookie>();
        (cookie.link, cookie.stream)
    };
    let req = link.request()?;
    if link.fatal(req) {
        return None;
    }
    stream.write(req, bytes)
}

/// `stream_cookie_closer`, less the stream's free: the stream is its
/// value's, and the cookie is let go of with its `FILE *`.
extern "C" fn cookie_close(cookie: *mut c_void) -> c_int {
    // SAFETY: the box `CFile::cookie` made, freed here once, by the
    // `fclose` of its `FILE`.
    drop(unsafe { Box::from_raw(cookie.cast::<Cookie>()) });
    0
}

/// The write function `funopen(3)` calls.
#[cfg(not(target_os = "linux"))]
extern "C" fn funopen_write(cookie: *mut c_void, data: *const c_char, len: c_int) -> c_int {
    let bytes = slice(data, usize::try_from(len).unwrap_or(0));
    cookie_write(cookie, bytes).map_or(-1, |n| c_int::try_from(n).unwrap_or(c_int::MAX))
}

/// The write function `fopencookie(3)` calls.
#[cfg(target_os = "linux")]
extern "C" fn fopencookie_write(cookie: *mut c_void, data: *const c_char, len: usize) -> isize {
    cookie_write(cookie, slice(data, len)).map_or(-1, |n| isize::try_from(n).unwrap_or(isize::MAX))
}

#[cfg(not(target_os = "linux"))]
unsafe extern "C" {
    /// The BSD C library's, `FILE *funopen(const void *, int (*)(void *,
    /// char *, int), int (*)(void *, const char *, int), fpos_t (*)(void *,
    /// fpos_t, int), int (*)(void *))`.
    fn funopen(
        cookie: *const c_void,
        read: Option<extern "C" fn(*mut c_void, *mut c_char, c_int) -> c_int>,
        write: Option<extern "C" fn(*mut c_void, *const c_char, c_int) -> c_int>,
        seek: Option<extern "C" fn(*mut c_void, i64, c_int) -> i64>,
        close: Option<extern "C" fn(*mut c_void) -> c_int>,
    ) -> *mut libc::FILE;
}

/// glibc's and musl's `cookie_io_functions_t`.
#[cfg(target_os = "linux")]
#[repr(C)]
struct CookieIo {
    read: Option<extern "C" fn(*mut c_void, *mut c_char, usize) -> isize>,
    write: Option<extern "C" fn(*mut c_void, *const c_char, usize) -> isize>,
    seek: Option<extern "C" fn(*mut c_void, *mut i64, c_int) -> c_int>,
    close: Option<extern "C" fn(*mut c_void) -> c_int>,
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn fopencookie(cookie: *mut c_void, mode: *const c_char, io: CookieIo) -> *mut libc::FILE;
}

/// A write-only cookie `FILE *` over `cookie`, null where none opened.
#[cfg(not(target_os = "linux"))]
fn open_cookie(cookie: *mut c_void) -> *mut libc::FILE {
    // SAFETY: a live box the `FILE` keeps until `cookie_close`, and
    // functions of the signatures `funopen` calls.
    unsafe { funopen(cookie, None, Some(funopen_write), None, Some(cookie_close)) }
}

/// A write-only cookie `FILE *` over `cookie`, null where none opened.
#[cfg(target_os = "linux")]
fn open_cookie(cookie: *mut c_void) -> *mut libc::FILE {
    let io = CookieIo {
        read: None,
        write: Some(fopencookie_write),
        seek: None,
        close: Some(cookie_close),
    };
    // SAFETY: a live box the `FILE` keeps until `cookie_close`, a
    // NUL-terminated mode and functions of the signatures `fopencookie`
    // calls.
    unsafe { fopencookie(cookie, c"w".as_ptr(), io) }
}
