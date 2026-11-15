//! System V semaphores, `<sys/sem.h>` from the C library (design §7.2;
//! `unsafe.homes`' `ext/*/src/ffi/`), since nix binds no System V IPC:
//! `semget`, `semop` and `semctl` behind safe functions. The one
//! home of this crate's `unsafe`.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::c_int;

/// `union semun`, which `semctl`'s fourth argument is and no header
/// declares: the value `SETVAL` writes, or the buffer `IPC_STAT` fills.
#[repr(C)]
#[derive(Clone, Copy)]
union Semun {
    val: c_int,
    buf: *mut SemidDs,
}

/// `struct semid_ds`, the buffer `IPC_STAT` fills, as the C library
/// declares it.
#[cfg(not(target_env = "musl"))]
use libc::semid_ds as SemidDs;

/// musl's `struct semid_ds`, which the libc crate does not declare.
/// Nothing here reads a member: it is the buffer `IPC_STAT`
/// fills, so its size and alignment are what count, and 128 bytes holds
/// every architecture's in musl's `bits/sem.h` (`x86_64`'s is 104).
#[cfg(target_env = "musl")]
#[repr(C, align(8))]
struct SemidDs([u8; 128]);

/// The `errno` of the call that just failed.
fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// A C answer of `-1` as the errno.
fn checked(answer: c_int) -> Result<c_int, i32> {
    if answer == -1 {
        Err(errno())
    } else {
        Ok(answer)
    }
}

/// `semget(key, nsems, flags)`: the set's id, or the errno.
pub(crate) fn get(key: i32, nsems: i32, flags: i32) -> Result<i32, i32> {
    // SAFETY: `semget` takes three values and touches no memory of ours.
    checked(unsafe { libc::semget(key, nsems, flags) })
}

/// One `struct sembuf`: the semaphore of the set, what is added to it
/// (zero waits for it to be zero), and `SEM_UNDO` and `IPC_NOWAIT`.
#[derive(Clone, Copy)]
pub(crate) struct Op {
    pub(crate) num: u16,
    pub(crate) delta: i16,
    pub(crate) flags: i16,
}

/// `semop(id, ops, n)`, the operations done together or not at all, or
/// the errno.
pub(crate) fn op(id: i32, ops: &[Op]) -> Result<(), i32> {
    let mut buf: Vec<libc::sembuf> = ops
        .iter()
        .map(|o| libc::sembuf {
            sem_num: o.num,
            sem_op: o.delta,
            sem_flg: o.flags,
        })
        .collect();
    // SAFETY: `buf` is `buf.len()` `sembuf`s this frame owns, which
    // `semop` reads.
    checked(unsafe { libc::semop(id, buf.as_mut_ptr(), buf.len()) }).map(drop)
}

/// `semctl(id, num, GETVAL, NULL)`: the semaphore's value, or the errno.
pub(crate) fn value(id: i32, num: i32) -> Result<i32, i32> {
    let arg = Semun {
        buf: std::ptr::null_mut(),
    };
    // SAFETY: `GETVAL` reads nothing through its argument.
    checked(unsafe { libc::semctl(id, num, libc::GETVAL, arg) })
}

/// `semctl(id, num, SETVAL, semarg)`: the semaphore set to `val`, or
/// the errno.
pub(crate) fn set_value(id: i32, num: i32, val: i32) -> Result<(), i32> {
    let arg = Semun { val };
    // SAFETY: `SETVAL` reads the value out of its argument.
    checked(unsafe { libc::semctl(id, num, libc::SETVAL, arg) }).map(drop)
}

/// `semctl(id, 0, IPC_STAT, buf)`: whether the set still exists.
pub(crate) fn exists(id: i32) -> bool {
    // SAFETY: `semid_ds` is integers and a permission struct of integers,
    // for which all zeroes is a value.
    let mut ds: SemidDs = unsafe { std::mem::zeroed() };
    let arg = Semun { buf: &raw mut ds };
    // SAFETY: `ds` is a `semid_ds` this frame owns, which `IPC_STAT`
    // writes and nothing else.
    unsafe { libc::semctl(id, 0, libc::IPC_STAT, arg) >= 0 }
}

/// `semctl(id, 0, IPC_RMID)`: the set removed, or the errno.
pub(crate) fn remove(id: i32) -> Result<(), i32> {
    let arg = Semun {
        buf: std::ptr::null_mut(),
    };
    // SAFETY: `IPC_RMID` reads nothing through its argument.
    checked(unsafe { libc::semctl(id, 0, libc::IPC_RMID, arg) }).map(drop)
}
