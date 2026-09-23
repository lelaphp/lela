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
