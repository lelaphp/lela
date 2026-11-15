//! The process's identity, its real and effective user and group and its
//! supplementary groups (design §4.1, §4.4, revision 41; `unsafe.homes`):
//! `getuid(2)`, `geteuid(2)`, `getgid(2)`, `getegid(2)` and
//! `getgroups(2)`, bound once for the engine and read by every extension
//! through `api::process`. php calls them from `<unistd.h>` wherever it
//! asks, `php_stat`'s and `phar_fancy_stat`'s permission masks,
//! `ps_files_open`'s ownership rule and `posix_getuid` and its kin among
//! them; the
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

/// `geteuid()`: the process's effective user id.
#[must_use]
pub fn euid() -> u32 {
    // SAFETY: `geteuid` takes nothing, cannot fail and writes nothing.
    unsafe { libc::geteuid() }
}

/// `getgid()`: the process's real group id.
#[must_use]
pub fn gid() -> u32 {
    // SAFETY: `getgid` takes nothing, cannot fail and writes nothing.
    unsafe { libc::getgid() }
}

/// `getegid()`: the process's effective group id.
#[must_use]
pub fn egid() -> u32 {
    // SAFETY: `getegid` takes nothing, cannot fail and writes nothing.
    unsafe { libc::getegid() }
}

/// `getgroups(0, NULL)` and then `getgroups(n, list)`: the process's
/// supplementary group ids, asked for their count first and then into
/// room for that many, as php asks twice because macOS may answer more
/// than `NGROUPS_MAX`. Either call's failure is the `errno` it left,
/// which `posix_getgroups` keeps; the list may or may not hold the
/// effective group, as the platform answers.
///
/// On macOS the call is the C library's `getgroups$DARWIN_EXTSN`, which
/// php gets by building with `_DARWIN_C_SOURCE`: the user's
/// group access list from Open Directory, `getgrouplist` of
/// `getpwuid(getuid())`, uncapped and blind to a `setgroups(2)`, and
/// `EINVAL` where Open Directory cannot be reached; not the
/// kernel's list of at most 16 that the plain symbol answers.
///
/// # Errors
///
/// The OS error either call failed with, `EINVAL` where the groups grew
/// between the two.
pub fn groups() -> io::Result<Vec<u32>> {
    // SAFETY: a count of zero asks the number alone; the null pointer is
    // not written through.
    let count = unsafe { getgroups(0, std::ptr::null_mut()) };
    let Ok(len) = usize::try_from(count) else {
        return Err(io::Error::last_os_error());
    };
    let mut list: Vec<libc::gid_t> = vec![0; len];
    // SAFETY: `list` has room for `count` ids, which is the most the call
    // writes.
    let got = unsafe { getgroups(count, list.as_mut_ptr()) };
    let Ok(got) = usize::try_from(got) else {
        return Err(io::Error::last_os_error());
    };
    list.truncate(got);
    Ok(list)
}

// php's `getgroups` on macOS, the variant `_DARWIN_C_SOURCE` selects in
// `<unistd.h>`, which the `libc` crate does not bind.
#[cfg(target_vendor = "apple")]
unsafe extern "C" {
    #[link_name = "getgroups$DARWIN_EXTSN"]
    fn getgroups(size: libc::c_int, list: *mut libc::gid_t) -> libc::c_int;
}
#[cfg(not(target_vendor = "apple"))]
use libc::getgroups;
