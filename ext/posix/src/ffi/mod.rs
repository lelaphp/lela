//! The calls `nix` does not wrap (design §7.2, revision 35; `unsafe.homes`'
//! `ext/*/src/ffi/`), each one `libc` call behind a safe function, and the
//! one home of this crate's `unsafe`. `times`, `getlogin` and `ctermid`
//! have no `nix` form at all; `getgroups` and `initgroups` have none on
//! Apple's targets; `sysconf`, `pathconf`, `fpathconf`, `getrlimit` and
//! `setrlimit` take the number a program passed, which `nix`'s enums cannot
//! carry for a number the platform does not name; `strerror_r` is the C
//! library's own text, which `posix_strerror` prints; `isatty` and
//! `ttyname_r` take a number a program passed, which need not be an open
//! fd `nix`'s `AsFd` could borrow; and `eaccess` is the C library's call
//! php makes, which `nix` reaches only through `faccessat`'s `AT_EACCESS`.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, c_char};
use std::os::fd::RawFd;

use nix::errno::Errno;

use crate::as_c_int;

/// `L_ctermid` in `<stdio.h>`: the room `ctermid` writes into.
#[cfg(target_os = "macos")]
const L_CTERMID: usize = 1024;
#[cfg(not(target_os = "macos"))]
const L_CTERMID: usize = 9;

unsafe extern "C" {
    /// `<stdio.h>`'s `ctermid`, which the `libc` crate does not declare.
    fn ctermid(s: *mut c_char) -> *mut c_char;
}

/// The five counts `times(2)` answers: the ticks since an arbitrary point,
/// then the user and system time of the process and of its waited-for
/// children.
pub(crate) struct Times {
    pub(crate) ticks: i64,
    pub(crate) utime: i64,
    pub(crate) stime: i64,
    pub(crate) cutime: i64,
    pub(crate) cstime: i64,
}

/// `times(&t)`: the counts, or the errno of the call that failed.
pub(crate) fn times() -> Result<Times, Errno> {
    let mut t = libc::tms {
        tms_utime: 0,
        tms_stime: 0,
        tms_cutime: 0,
        tms_cstime: 0,
    };
    // SAFETY: `t` is a `tms` this frame owns, and `times` writes it and
    // nothing else.
    let ticks = unsafe { libc::times(&raw mut t) };
    // `times` fails with `(clock_t) -1`, which is `clock_t::MAX` where the
    // platform's `clock_t` is unsigned (macOS) and `-1` where it is signed
    // (Linux); `as_clock` answers the one bit pattern either way.
    if ticks == as_clock(-1) {
        return Err(Errno::last());
    }
    Ok(Times {
        ticks: from_clock(ticks),
        utime: from_clock(t.tms_utime),
        stime: from_clock(t.tms_stime),
        cutime: from_clock(t.tms_cutime),
        cstime: from_clock(t.tms_cstime),
    })
}

/// A `clock_t` read as php's `zend_long` reads it: its bits as a signed
/// 64-bit count, whether the platform's `clock_t` is unsigned (macOS) or
/// already signed (Linux).
#[allow(clippy::cast_possible_wrap, clippy::unnecessary_cast)]
const fn from_clock(c: libc::clock_t) -> i64 {
    c as i64
}

/// `(clock_t) n`, as C writes the failure value `times` returns.
#[allow(clippy::cast_sign_loss, clippy::unnecessary_cast)]
const fn as_clock(n: i64) -> libc::clock_t {
    n as libc::clock_t
}

/// `getlogin()`: the login name, copied out of the C library's static
/// storage at once, or the errno of the call that found none.
pub(crate) fn getlogin() -> Result<Vec<u8>, Errno> {
    // SAFETY: `getlogin` takes nothing and answers a pointer into the C
    // library's storage or null.
    let name = unsafe { libc::getlogin() };
    if name.is_null() {
        return Err(Errno::last());
    }
    // SAFETY: a non-null answer is a NUL-terminated string that stands
    // until the next `getlogin`, and it is copied before this returns.
    Ok(unsafe { CStr::from_ptr(name) }.to_bytes().to_vec())
}

/// `ctermid(buffer)`: the controlling terminal's path, or the errno.
pub(crate) fn controlling_terminal() -> Result<Vec<u8>, Errno> {
    let mut buffer = [0 as c_char; L_CTERMID];
    // SAFETY: `buffer` is `L_ctermid` bytes, the room `ctermid` writes a
    // NUL-terminated path into.
    let written = unsafe { ctermid(buffer.as_mut_ptr()) };
    if written.is_null() {
        return Err(Errno::last());
    }
    // SAFETY: `ctermid` left a NUL-terminated string in `buffer`.
    Ok(unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_bytes()
        .to_vec())
}

/// `getgroups(0, NULL)` and then `getgroups(n, list)`, as php asks twice
/// because macOS may answer more than `NGROUPS_MAX`.
pub(crate) fn getgroups() -> Result<Vec<u32>, Errno> {
    // SAFETY: a count of zero asks the number alone and writes nothing.
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    let Ok(len) = usize::try_from(count) else {
        return Err(Errno::last());
    };
    let mut list: Vec<libc::gid_t> = vec![0; len];
    // SAFETY: `list` has room for `count` ids, which is what is asked.
    let got = unsafe { libc::getgroups(count, list.as_mut_ptr()) };
    let Ok(got) = usize::try_from(got) else {
        return Err(Errno::last());
    };
    list.truncate(got);
    Ok(list)
}

/// `initgroups(name, basegid)`: whether it succeeded.
pub(crate) fn initgroups(name: &CStr, group: i64) -> bool {
    // SAFETY: `name` is NUL-terminated and outlives the call.
    unsafe { libc::initgroups(name.as_ptr(), initgroups_gid(group)) == 0 }
}

/// `initgroups`' group parameter as the platform declares it: a C `int`
/// on macOS and the BSDs, a `gid_t` on Linux, each taking the
/// `zend_long` as C converts it.
#[cfg(target_os = "linux")]
fn initgroups_gid(group: i64) -> libc::gid_t {
    crate::as_id(group)
}
#[cfg(not(target_os = "linux"))]
fn initgroups_gid(group: i64) -> libc::c_int {
    as_c_int(group)
}

/// `sysconf(name)`, whatever number the program passed.
pub(crate) fn sysconf(name: i64) -> i64 {
    // SAFETY: `sysconf` reads nothing through its argument, a number.
    unsafe { libc::sysconf(as_c_int(name)) }
}

/// `pathconf(path, name)` with php's reading of the answer: a value below
/// zero with `errno` set is the errno, and below zero with none is the
/// value, "no limit".
pub(crate) fn pathconf(path: &CStr, name: i64) -> Result<i64, Errno> {
    Errno::clear();
    // SAFETY: `path` is NUL-terminated and outlives the call.
    let answer = unsafe { libc::pathconf(path.as_ptr(), as_c_int(name)) };
    conf(answer)
}

/// `fpathconf(fd, name)`, read as [`pathconf`] is.
pub(crate) fn fpathconf(fd: RawFd, name: i64) -> Result<i64, Errno> {
    Errno::clear();
    // SAFETY: `fpathconf` reads nothing through its arguments, numbers.
    let answer = unsafe { libc::fpathconf(fd, as_c_int(name)) };
    conf(answer)
}

fn conf(answer: i64) -> Result<i64, Errno> {
    let errno = Errno::last();
    if answer < 0 && errno != Errno::UnknownErrno {
        return Err(errno);
    }
    Ok(answer)
}

/// One resource's two limits, `rlim_cur` and `rlim_max`; `None` is
/// `RLIM_INFINITY`.
pub(crate) struct Limits {
    pub(crate) soft: Option<i64>,
    pub(crate) hard: Option<i64>,
}

fn limit(value: libc::rlim_t) -> Option<i64> {
    (value != libc::RLIM_INFINITY).then_some(value.cast_signed())
}

/// `getrlimit(resource, &rl)`, whatever number the program passed.
// the resource is C's `int` on macOS and an unsigned `__rlimit_resource_t` on Linux; the cast is C's own conversion.
#[allow(clippy::cast_sign_loss)]
pub(crate) fn getrlimit(resource: i64) -> Result<Limits, Errno> {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rl` is an `rlimit` this frame owns, and `getrlimit` writes it
    // and nothing else.
    if unsafe { libc::getrlimit(as_c_int(resource) as _, &raw mut rl) } < 0 {
        return Err(Errno::last());
    }
    Ok(Limits {
        soft: limit(rl.rlim_cur),
        hard: limit(rl.rlim_max),
    })
}

/// `setrlimit(resource, &rl)` with the two numbers as php stores them,
/// `rl.rlim_cur = cur`, a `-1` included.
// the resource is C's `int` on macOS and an unsigned `__rlimit_resource_t` on Linux; the cast is C's own conversion.
#[allow(clippy::cast_sign_loss)]
pub(crate) fn setrlimit(resource: i64, soft: i64, hard: i64) -> Result<(), Errno> {
    let rl = libc::rlimit {
        rlim_cur: soft.cast_unsigned(),
        rlim_max: hard.cast_unsigned(),
    };
    // SAFETY: `rl` is read and not kept.
    if unsafe { libc::setrlimit(as_c_int(resource) as _, &raw const rl) } == -1 {
        return Err(Errno::last());
    }
    Ok(())
}

/// `strerror(error)`: the C library's text for the number, as php prints
/// it, an unknown number's text included.
pub(crate) fn strerror(error: i64) -> Vec<u8> {
    let mut buffer = [0 as c_char; 256];
    // SAFETY: `buffer` is 256 bytes and its length is what is passed;
    // `strerror_r` writes a NUL-terminated message within it.
    unsafe { libc::strerror_r(as_c_int(error), buffer.as_mut_ptr(), buffer.len()) };
    // SAFETY: `strerror_r` left a NUL-terminated string in `buffer`, and
    // the last byte, never written past, is the NUL when it truncated.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_bytes()
        .to_vec()
}

/// `eaccess(path, mode)`: `access(2)` by the effective ids, where the C
/// library has it (`HAVE_EACCESS`: Linux and FreeBSD); the errno where the
/// answer is no.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) fn eaccess(path: &CStr, mode: i64) -> Result<(), Errno> {
    // SAFETY: `path` is NUL-terminated and outlives the call.
    if unsafe { libc::eaccess(path.as_ptr(), as_c_int(mode)) } == 0 {
        return Ok(());
    }
    Err(Errno::last())
}

/// `isatty(fd)`: whether the fd is a terminal, and the errno where it is
/// not, `ENOTTY` or `EBADF`. A number a program passed is no fd `nix`'s
/// `AsFd` may borrow, since it need not be open.
pub(crate) fn isatty(fd: RawFd) -> Result<(), Errno> {
    // SAFETY: `isatty` reads nothing through its argument, a number.
    if unsafe { libc::isatty(fd) } == 1 {
        return Ok(());
    }
    Err(Errno::last())
}

/// `ttyname_r(fd, buffer, len)`: the terminal's path, the buffer grown on
/// `ERANGE` as php's ZTS arm grows it.
pub(crate) fn ttyname(fd: RawFd) -> Result<Vec<u8>, Errno> {
    let mut buffer: Vec<c_char> = vec![0; 128];
    loop {
        // SAFETY: `buffer` has `buffer.len()` bytes, which is what is
        // passed; `ttyname_r` writes a NUL-terminated path within it.
        let err = unsafe { libc::ttyname_r(fd, buffer.as_mut_ptr(), buffer.len()) };
        match err {
            0 => break,
            libc::ERANGE => buffer.resize(buffer.len() * 2, 0),
            err => return Err(Errno::from_raw(err)),
        }
    }
    // SAFETY: `ttyname_r` answered zero, so `buffer` holds a
    // NUL-terminated path.
    Ok(unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_bytes()
        .to_vec())
}
