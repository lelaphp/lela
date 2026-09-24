//! The C library calls `filestat.c` makes that Rust's `std` has no
//! answer for (design §4.1, `unsafe.homes`' `ext/*/src/ffi/`):
//! `access(2)`, which is what `file_exists`, `is_readable`,
//! `is_writable` and `is_executable` ask of a plain file; `getuid(2)`,
//! `getgid(2)` and `getgroups(2)`, which choose the permission bits those
//! read of any other wrapper's file; `getpwnam_r(3)` and `getgrnam_r(3)`
//! for `chown` and `chgrp` by name; and `utime(2)`, which `touch` sets
//! the times with. Each is one call, and a path reaches it as the bytes
//! the host has.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// `F_OK`, `R_OK`, `W_OK` and `X_OK`: what `access(2)` is asked.
pub(super) const F_OK: i32 = libc::F_OK;
pub(super) const R_OK: i32 = libc::R_OK;
pub(super) const W_OK: i32 = libc::W_OK;
pub(super) const X_OK: i32 = libc::X_OK;

/// The path as a C string; a NUL inside it is a path no call can be
/// handed, which `Z_PARAM_PATH` refused before it got here.
fn c_path(path: &Path) -> Option<CString> {
    CString::new(path.as_os_str().as_bytes()).ok()
}

/// `VCWD_ACCESS(path, mode) == 0`.
pub(super) fn access(path: &Path, mode: i32) -> bool {
    let Some(path) = c_path(path) else {
        return false;
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call,
    // which reads it and nothing else.
    unsafe { libc::access(path.as_ptr(), mode) == 0 }
}

/// `getuid()`.
pub(super) fn uid() -> u32 {
    // SAFETY: `getuid` takes nothing and cannot fail.
    unsafe { libc::getuid() }
}

/// `getgid()`.
pub(super) fn gid() -> u32 {
    // SAFETY: `getgid` takes nothing and cannot fail.
    unsafe { libc::getgid() }
}

/// `getgroups()`: the supplementary groups, asked for their count first
/// and then into room for that many, as `php_stat` does.
pub(super) fn groups() -> Vec<u32> {
    // SAFETY: a count of zero asks only for the number of groups; the
    // null pointer is not written through.
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    let Ok(len) = usize::try_from(count) else {
        return Vec::new();
    };
    if len == 0 {
        return Vec::new();
    }
    let mut gids = vec![0; len];
    // SAFETY: `gids` has room for `count` entries, which is the most the
    // call writes.
    let written = unsafe { libc::getgroups(count, gids.as_mut_ptr()) };
    gids.truncate(usize::try_from(written).unwrap_or(0));
    gids
}

/// `php_get_uid_by_name` (`Z/ext/standard/filestat.c:416`) as ZTS builds
/// it, over `getpwnam_r(3)` with its buffer grown while it is too small.
pub(super) fn uid_by_name(name: &[u8]) -> Option<u32> {
    let name = CString::new(name).ok()?;
    let mut buf: Vec<libc::c_char> = vec![0; 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is to live storage of the size given,
        // `name` is NUL-terminated, and the call writes only `entry`,
        // `buf` up to its length and `found`.
        let err = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                entry.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut found,
            )
        };
        if err == libc::ERANGE || err == libc::EAGAIN {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if err != 0 || found.is_null() {
            return None;
        }
        // SAFETY: the call returned 0 with `found` set, so it filled
        // `entry`.
        let entry = unsafe { entry.assume_init() };
        return Some(entry.pw_uid);
    }
}

/// `php_get_gid_by_name` (`Z/ext/standard/filestat.c:275`) as ZTS builds
/// it, over `getgrnam_r(3)` with its buffer grown while it is too small.
pub(super) fn gid_by_name(name: &[u8]) -> Option<u32> {
    let name = CString::new(name).ok()?;
    let mut buf: Vec<libc::c_char> = vec![0; 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::group>::uninit();
        let mut found: *mut libc::group = std::ptr::null_mut();
        // SAFETY: as `uid_by_name`'s call, over a `group`.
        let err = unsafe {
            libc::getgrnam_r(
                name.as_ptr(),
                entry.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut found,
            )
        };
        if err == libc::ERANGE {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if err != 0 || found.is_null() {
            return None;
        }
        // SAFETY: the call returned 0 with `found` set, so it filled
        // `entry`.
        let entry = unsafe { entry.assume_init() };
        return Some(entry.gr_gid);
    }
}

/// `VCWD_UTIME(path, newtime)`: the modification and access times, or
/// the current time for `None`, which `utime(2)` checks against write
/// access where a time given is checked against ownership.
pub(super) fn utime(path: &Path, times: Option<(i64, i64)>) -> std::io::Result<()> {
    let Some(path) = c_path(path) else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    let buf = times.map(|(modtime, actime)| libc::utimbuf { actime, modtime });
    let at = buf.as_ref().map_or(std::ptr::null(), std::ptr::from_ref);
    // SAFETY: `path` is NUL-terminated and `at` is null or points at a
    // `utimbuf` that outlives the call, which only reads them.
    let ret = unsafe { libc::utime(path.as_ptr(), at) };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `fnmatch(pattern, string, flags) == 0`, the host's `fnmatch(3)`,
/// which `fnmatch()` calls as it is; a NUL inside either is refused by
/// `Z_PARAM_PATH` before it gets here.
pub(super) fn fnmatch(pattern: &[u8], string: &[u8], flags: i32) -> bool {
    let (Ok(pattern), Ok(string)) = (CString::new(pattern), CString::new(string)) else {
        return false;
    };
    // SAFETY: both are NUL-terminated strings that outlive the call,
    // which reads them and nothing else.
    unsafe { libc::fnmatch(pattern.as_ptr(), string.as_ptr(), flags) == 0 }
}

/// `arc4random_buf(3)`, the host's CSPRNG, which is what
/// `php_random_bytes` reads on this host: `buf` filled with random
/// bytes. It cannot fail.
pub(super) fn random_bytes(buf: &mut [u8]) {
    // SAFETY: `buf` is live, writable storage of exactly the length
    // given, which is all the call writes.
    unsafe { libc::arc4random_buf(buf.as_mut_ptr().cast(), buf.len()) }
}

/// `sysconf(_SC_ARG_MAX)`, what `PHP_MINIT_FUNCTION(exec)` sets
/// `cmd_max_len` to, and `_POSIX_ARG_MAX`'s 4096 where it answers `-1`.
pub(super) fn arg_max() -> usize {
    // SAFETY: `sysconf` reads a name and writes nothing.
    let max = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
    usize::try_from(max).unwrap_or(4096)
}

/// `errno = 0; nice(increment); errno == 0`: whether `nice(2)` took the
/// increment, which is how `proc_nice` tells, since `-1` is also a
/// niceness.
pub(super) fn nice(increment: i32) -> bool {
    // SAFETY: `__error` answers the calling thread's `errno` location
    // and nothing else.
    let errno = unsafe { libc::__error() };
    // SAFETY: that location is live and the thread's own for its life;
    // writing zero to it is what C's `errno = 0` does.
    unsafe { *errno = 0 };
    // SAFETY: `nice` takes a number and changes the process's priority,
    // nothing in this program's memory.
    unsafe { libc::nice(increment) };
    std::io::Error::last_os_error().raw_os_error() == Some(0)
}

/// `openlog(3)` with `ident`, which the caller keeps alive until the
/// matching [`closelog`], since the C library keeps the pointer.
pub(super) fn openlog(ident: &CString, option: i32, facility: i32) {
    // SAFETY: `ident` is NUL-terminated and outlives the log it names,
    // which is closed before the caller lets it go.
    unsafe { libc::openlog(ident.as_ptr(), option, facility) };
}

/// `closelog(3)`.
pub(super) fn closelog() {
    // SAFETY: `closelog` takes nothing and closes the log if one is open.
    unsafe { libc::closelog() };
}

/// `syslog(priority, "%.*s", len, message)`: one message, its bytes as
/// they are, NUL ones included in the length.
pub(super) fn syslog(priority: i32, message: &[u8]) {
    let len = libc::c_int::try_from(message.len()).unwrap_or(libc::c_int::MAX);
    // SAFETY: the format is NUL-terminated and asks for one `int` and one
    // pointer to at most `len` bytes, which `message` holds and outlives
    // the call.
    unsafe { libc::syslog(priority, c"%.*s".as_ptr(), len, message.as_ptr()) };
}

/// `strsignal(3)`: the host's name for a signal.
pub(super) fn strsignal(signal: i32) -> String {
    // SAFETY: `strsignal` answers a pointer to a NUL-terminated string
    // the C library owns, valid until the next call on this thread.
    let text = unsafe { libc::strsignal(signal) };
    if text.is_null() {
        return String::new();
    }
    // SAFETY: `text` is the non-null, NUL-terminated string just
    // answered, read before any other call could replace it.
    let text = unsafe { std::ffi::CStr::from_ptr(text) };
    text.to_string_lossy().into_owned()
}

/// A C string the C library owns, copied out, or `None` for null.
fn owned(text: *const libc::c_char) -> Option<Vec<u8>> {
    if text.is_null() {
        return None;
    }
    // SAFETY: `text` is a non-null, NUL-terminated string the call just
    // answered, read before any other call could replace it.
    let text = unsafe { std::ffi::CStr::from_ptr(text) };
    Some(text.to_bytes().to_vec())
}

/// `ntohs(getservbyname(name, proto)->s_port)`.
pub(super) fn service_port(name: &[u8], proto: &[u8]) -> Option<u16> {
    let (name, proto) = (CString::new(name).ok()?, CString::new(proto).ok()?);
    // SAFETY: both strings are NUL-terminated and outlive the call, which
    // answers null or a pointer to the C library's own `servent`.
    let entry = unsafe { libc::getservbyname(name.as_ptr(), proto.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: `entry` is the non-null `servent` just answered.
    let port = unsafe { (*entry).s_port };
    let port = u16::from_ne_bytes([port.to_ne_bytes()[0], port.to_ne_bytes()[1]]);
    Some(u16::from_be(port))
}

/// `getservbyport(htons(port), proto)->s_name`.
pub(super) fn service_name(port: u16, proto: &[u8]) -> Option<Vec<u8>> {
    let proto = CString::new(proto).ok()?;
    let port = i32::from(port.to_be());
    // SAFETY: `proto` is NUL-terminated and outlives the call, which
    // answers null or a pointer to the C library's own `servent`.
    let entry = unsafe { libc::getservbyport(port, proto.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: `entry` is the non-null `servent` just answered.
    owned(unsafe { (*entry).s_name })
}

/// `getprotobyname(name)->p_proto`.
pub(super) fn protocol_number(name: &[u8]) -> Option<i32> {
    let name = CString::new(name).ok()?;
    // SAFETY: `name` is NUL-terminated and outlives the call, which
    // answers null or a pointer to the C library's own `protoent`.
    let entry = unsafe { libc::getprotobyname(name.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: `entry` is the non-null `protoent` just answered.
    Some(unsafe { (*entry).p_proto })
}

/// `getprotobynumber(number)->p_name`.
pub(super) fn protocol_name(number: i32) -> Option<Vec<u8>> {
    // SAFETY: the call takes a number and answers null or a pointer to
    // the C library's own `protoent`.
    let entry = unsafe { libc::getprotobynumber(number) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: `entry` is the non-null `protoent` just answered.
    owned(unsafe { (*entry).p_name })
}

/// `strptime(s, format, &tm)` over a zeroed `tm`: the eight fields
/// `strptime()` answers, `tm_sec` to `tm_yday`, and what was left of the
/// string, or `None` where the call answered null.
pub(super) fn strptime(s: &[u8], format: &[u8]) -> Option<([i32; 8], Vec<u8>)> {
    let (text, format) = (CString::new(s).ok()?, CString::new(format).ok()?);
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: both strings are NUL-terminated and outlive the call, and
    // `tm` is live, zeroed storage the call writes; it answers null or a
    // pointer into `text`.
    let rest = unsafe { libc::strptime(text.as_ptr(), format.as_ptr(), tm.as_mut_ptr()) };
    if rest.is_null() {
        return None;
    }
    // SAFETY: a zeroed `tm` is a valid one, and the call wrote only fields.
    let tm = unsafe { tm.assume_init() };
    let rest = owned(rest)?;
    Some((
        [
            tm.tm_sec, tm.tm_min, tm.tm_hour, tm.tm_mday, tm.tm_mon, tm.tm_year, tm.tm_wday,
            tm.tm_yday,
        ],
        rest,
    ))
}
