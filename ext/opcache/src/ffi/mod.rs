//! The calls that give the calling thread alone another user's ids, the
//! one home of this crate's `unsafe` (`unsafe.homes`' `ext/*/src/ffi/`,
//! as #1258's ruling of 2026-10-01 has it), each one call behind
//! [`take`]. php's preload child calls `setgid`, `initgroups` and
//! `setuid` (`Z/ext/opcache/ZendAccelerator.c:5028-5039`), which change
//! a whole process; this engine's preload is a thread of a process that
//! goes on serving as it was, so the calls are the per-thread ones.
//!
//! - **Linux:** the raw `setresgid`, `setgroups` and `setresuid` system
//!   calls, each of which changes the calling thread's credentials
//!   alone. glibc's wrappers of the same names broadcast the change to
//!   every thread of the process (POSIX's per-process ids, by the
//!   setxid signal), so they are not called. The groups are
//!   `getgrouplist`'s for the user and its group, as glibc's
//!   `initgroups` resolves them.
//! - **macOS:** `pthread_setugid_np(uid, gid)` (`<unistd.h>`), xnu's
//!   `settid`, which gives the calling thread a credential of its own
//!   with the real, effective and saved user and group ids set and the
//!   group list cut to `gid` alone, membership resolved for `uid` by the
//!   directory (`kauth_cred_model_setuidgid`,
//!   `bsd/kern/kern_credential.c`). `initgroups` after it sets the
//!   group list and that user on the thread's credential, not the
//!   process's, permitted by the process's credential being root
//!   (`setgroups_internal`, `bsd/kern/kern_prot.c`). A process the
//!   thread forks or spawns takes the thread's credential as its own
//!   (`forkproc`'s `kauth_cred_get()`, `bsd/kern/kern_fork.c`), as a
//!   child of php's child has the user's ids.
//!
//! Nothing undoes what [`take`] took: the thread ends with the preload.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::CStr;

/// Which of php's three calls the step that stands for it refused, the
/// one php's warning names. macOS takes the user and its group in one
/// call, which is php's `setgid` where it refuses, so no step there
/// stands for `setuid` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    Setgid,
    Initgroups,
    #[cfg(target_os = "linux")]
    Setuid,
}

#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86", target_arch = "arm"))
))]
use libc::{SYS_setgroups as SETGROUPS, SYS_setresgid as SETRESGID, SYS_setresuid as SETRESUID};
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "arm")))]
use libc::{
    SYS_setgroups32 as SETGROUPS, SYS_setresgid32 as SETRESGID, SYS_setresuid32 as SETRESUID,
};

/// The calling thread given `name`'s group `gid`, the groups the system
/// lists for `name` and `gid`, and the user `uid`, in php's order, for
/// this thread alone: the step that refused where one did, after which
/// the thread holds what the steps before it took.
#[cfg(target_os = "linux")]
pub(crate) fn take(name: &CStr, uid: u32, gid: u32) -> Result<(), Refused> {
    let (uid, gid_id) = (libc::c_long::from(uid), libc::c_long::from(gid));
    // SAFETY: `setresgid` takes three ids by value, reads no memory and
    // changes the calling thread's credentials alone.
    if unsafe { libc::syscall(SETRESGID, gid_id, gid_id, gid_id) } != 0 {
        return Err(Refused::Setgid);
    }
    let groups = nix::unistd::getgrouplist(name, nix::unistd::Gid::from_raw(gid))
        .map_err(|_| Refused::Initgroups)?;
    let groups: Vec<libc::gid_t> = groups.iter().map(|group| group.as_raw()).collect();
    let count = libc::c_long::try_from(groups.len()).map_err(|_| Refused::Initgroups)?;
    // SAFETY: `groups` holds `count` ids, which `setgroups` reads and
    // does not keep; it changes the calling thread's list alone.
    if unsafe { libc::syscall(SETGROUPS, count, groups.as_ptr()) } != 0 {
        return Err(Refused::Initgroups);
    }
    // SAFETY: `setresuid` takes three ids by value, reads no memory and
    // changes the calling thread's credentials alone.
    if unsafe { libc::syscall(SETRESUID, uid, uid, uid) } != 0 {
        return Err(Refused::Setuid);
    }
    Ok(())
}

/// `prctl(PR_GET_DUMPABLE)`: the process's dumpable setting, which a
/// thread's change of ids resets for every thread to
/// `fs.suid_dumpable`, 0 by default (`commit_creds`, kernel/cred.c), so
/// a process whose preload took a user's ids would dump no core and
/// read as undumpable after; `None` where the call fails.
#[cfg(target_os = "linux")]
pub(crate) fn dumpable() -> Option<libc::c_int> {
    let zero: libc::c_ulong = 0;
    // SAFETY: `PR_GET_DUMPABLE` reads the four further arguments as
    // unused numbers and writes no memory.
    let flag = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, zero, zero, zero, zero) };
    (flag >= 0).then_some(flag)
}

/// `prctl(PR_SET_DUMPABLE, flag)`: the setting [`dumpable`] read put back,
/// once no thread of the process holds other ids, since a dumpable
/// process lets a thread's user trace it.
#[cfg(target_os = "linux")]
pub(crate) fn set_dumpable(flag: libc::c_int) {
    let (flag, zero) = (libc::c_ulong::try_from(flag).unwrap_or_default(), 0);
    // SAFETY: `PR_SET_DUMPABLE` takes the flag by value, reads the rest
    // as unused numbers and writes no memory.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, flag, zero, zero, zero);
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `<unistd.h>`'s `pthread_setugid_np`, which the `libc` crate does
    /// not declare.
    fn pthread_setugid_np(uid: libc::uid_t, gid: libc::gid_t) -> libc::c_int;
}

/// The calling thread given the user `uid` and its group `gid` by one
/// call, which stands for php's `setgid` and `setuid` both and is
/// php's `setgid` where it refuses, the first of the two php makes;
/// then `name`'s groups, by `initgroups` on the thread's own credential.
#[cfg(target_os = "macos")]
pub(crate) fn take(name: &CStr, uid: u32, gid: u32) -> Result<(), Refused> {
    // SAFETY: `pthread_setugid_np` takes two ids by value, reads no
    // memory and changes the calling thread's credential alone.
    if unsafe { pthread_setugid_np(uid, gid) } != 0 {
        return Err(Refused::Setgid);
    }
    // `initgroups(const char *, int)`: the id's bits, as C converts it.
    let base = libc::c_int::from_ne_bytes(gid.to_ne_bytes());
    // SAFETY: `name` is a NUL-terminated string that outlives the call,
    // which reads it and does not keep it.
    if unsafe { libc::initgroups(name.as_ptr(), base) } != 0 {
        return Err(Refused::Initgroups);
    }
    Ok(())
}
