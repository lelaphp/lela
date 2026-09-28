//! The process's identity, its real user and group and its supplementary
//! groups (design §4.1, §4.4, revision 41; `unsafe.homes`): `getuid(2)`,
//! `getgid(2)` and `getgroups(2)`, bound once for the engine and read by
//! every extension through `api::process`. php calls them from
//! `<unistd.h>` wherever it asks, `php_stat`'s and `phar_fancy_stat`'s
//! permission masks and `posix_getuid` and its kin among them; the
//! standard library does not read them, and `nix` has no `getgroups` on
//! Apple's targets. Each is a call that reads the process and writes
//! nothing of it, safe from any thread.

use std::io;

/// `getuid()`: the process's real user id.
#[must_use]
pub fn uid() -> u32 {
    // SAFETY: `getuid` takes nothing, cannot fail and writes nothing.
    unsafe { libc::getuid() }
}

/// `getgid()`: the process's real group id.
#[must_use]
pub fn gid() -> u32 {
    // SAFETY: `getgid` takes nothing, cannot fail and writes nothing.
    unsafe { libc::getgid() }
}

/// `getgroups(0, NULL)` and then `getgroups(n, list)`: the process's
/// supplementary group ids, asked for their count first and then into
/// room for that many, as php asks twice because macOS may answer more
/// than `NGROUPS_MAX`. Either call's failure is the `errno` it left,
/// which `posix_getgroups` keeps; the list may or may not hold the
/// effective group, as the platform answers.
///
/// # Errors
///
/// The OS error either call failed with, `EINVAL` where the groups grew
/// between the two.
pub fn groups() -> io::Result<Vec<u32>> {
    // SAFETY: a count of zero asks the number alone; the null pointer is
    // not written through.
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    let Ok(len) = usize::try_from(count) else {
        return Err(io::Error::last_os_error());
    };
    let mut list: Vec<libc::gid_t> = vec![0; len];
    // SAFETY: `list` has room for `count` ids, which is the most the call
    // writes.
    let got = unsafe { libc::getgroups(count, list.as_mut_ptr()) };
    let Ok(got) = usize::try_from(got) else {
        return Err(io::Error::last_os_error());
    };
    list.truncate(got);
    Ok(list)
}
