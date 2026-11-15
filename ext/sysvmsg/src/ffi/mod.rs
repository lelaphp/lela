//! System V message queues, `<sys/msg.h>` from the C library (design
//! §7.2; `unsafe.homes`' `ext/*/src/ffi/`), since nix binds no System V
//! IPC: `msgget`, `msgsnd`, `msgrcv` and `msgctl` behind safe
//! functions. The `libc` crate declares them for Linux and not for
//! Apple's targets, whose `struct msqid_ds` is `<sys/msg.h>`'s own,
//! packed to four bytes; both are read through [`Stat`]. The one home of
//! this crate's `unsafe`.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{c_int, c_long};

#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::{c_int, c_long, c_ulong, c_void};

    /// `struct __msqid_ds_new` under `#pragma pack(4)`.
    #[repr(C, packed(4))]
    #[allow(non_camel_case_types, clippy::struct_field_names)]
    pub(super) struct msqid_ds {
        pub(super) msg_perm: libc::ipc_perm,
        msg_first: i32,
        msg_last: i32,
        msg_cbytes: c_ulong,
        pub(super) msg_qnum: c_ulong,
        pub(super) msg_qbytes: c_ulong,
        pub(super) msg_lspid: libc::pid_t,
        pub(super) msg_lrpid: libc::pid_t,
        pub(super) msg_stime: libc::time_t,
        msg_pad1: i32,
        pub(super) msg_rtime: libc::time_t,
        msg_pad2: i32,
        pub(super) msg_ctime: libc::time_t,
        msg_pad3: i32,
        msg_pad4: [i32; 4],
    }

    unsafe extern "C" {
        pub(super) fn msgget(key: libc::key_t, flags: c_int) -> c_int;
        pub(super) fn msgsnd(id: c_int, msgp: *const c_void, size: usize, flags: c_int) -> c_int;
        pub(super) fn msgrcv(
            id: c_int,
            msgp: *mut c_void,
            size: usize,
            kind: c_long,
            flags: c_int,
        ) -> isize;
        pub(super) fn msgctl(id: c_int, cmd: c_int, buf: *mut msqid_ds) -> c_int;
    }

    /// `MSG_NOERROR`.
    pub(super) const MSG_NOERROR: c_int = 0o10000;
    /// `MSG_EXCEPT`, which Apple's targets do not have.
    pub(super) const MSG_EXCEPT: Option<c_int> = None;
}

#[cfg(not(target_os = "macos"))]
mod sys {
    pub(super) use libc::{MSG_NOERROR, msgctl, msgget, msgrcv, msgsnd, msqid_ds};

    /// `MSG_EXCEPT`.
    pub(super) const MSG_EXCEPT: Option<std::ffi::c_int> = Some(libc::MSG_EXCEPT);
}

/// Whether the system can receive the first message of another type.
pub(crate) const HAS_EXCEPT: bool = sys::MSG_EXCEPT.is_some();

/// The `errno` of the call that just failed.
fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `msgget(key, flags)`: the queue's id, or the errno.
pub(crate) fn get(key: i32, flags: i32) -> Result<i32, i32> {
    // SAFETY: `msgget` takes two values and touches no memory of ours.
    let id = unsafe { sys::msgget(key, flags) };
    if id < 0 { Err(errno()) } else { Ok(id) }
}

/// `msgctl(id, IPC_STAT, &ds)`.
fn stat_ds(id: i32) -> Option<sys::msqid_ds> {
    // SAFETY: `msqid_ds` is integers and a permission struct of integers,
    // for which all zeroes is a value.
    let mut ds: sys::msqid_ds = unsafe { std::mem::zeroed() };
    // SAFETY: `ds` is a `msqid_ds` this frame owns, which `IPC_STAT`
    // writes and nothing else.
    let done = unsafe { sys::msgctl(id, libc::IPC_STAT, &raw mut ds) };
    (done == 0).then_some(ds)
}

/// What `msg_stat_queue` reports of a queue, `IPC_STAT`'s `[XSI]` fields.
pub(crate) struct Stat {
    pub(crate) uid: i64,
    pub(crate) gid: i64,
    pub(crate) mode: i64,
    pub(crate) stime: i64,
    pub(crate) rtime: i64,
    pub(crate) ctime: i64,
    pub(crate) qnum: i64,
    pub(crate) qbytes: i64,
    pub(crate) lspid: i64,
    pub(crate) lrpid: i64,
}

/// A C `unsigned long` read as php's `add_assoc_long` reads it.
#[allow(clippy::cast_possible_wrap, clippy::useless_conversion)]
const fn long(n: u64) -> i64 {
    n as i64
}

/// `msgctl(id, IPC_STAT, &stat)`, or `None` where it fails.
#[allow(clippy::useless_conversion)]
pub(crate) fn stat(id: i32) -> Option<Stat> {
    let ds = stat_ds(id)?;
    let perm = ds.msg_perm;
    Some(Stat {
        uid: i64::from(perm.uid),
        gid: i64::from(perm.gid),
        mode: i64::from(perm.mode),
        stime: i64::from(ds.msg_stime),
        rtime: i64::from(ds.msg_rtime),
        ctime: i64::from(ds.msg_ctime),
        qnum: long(u64::from(ds.msg_qnum)),
        qbytes: long(u64::from(ds.msg_qbytes)),
        lspid: i64::from(ds.msg_lspid),
        lrpid: i64::from(ds.msg_lrpid),
    })
}

/// The members `msg_set_queue` writes, each only where it was given, as
/// C truncates a `zend_long` into the member's type.
#[derive(Default)]
pub(crate) struct Changes {
    pub(crate) uid: Option<i64>,
    pub(crate) gid: Option<i64>,
    pub(crate) mode: Option<i64>,
    pub(crate) qbytes: Option<i64>,
}

/// `IPC_STAT`, the given members written over what it read, then
/// `IPC_SET`: whether both succeeded.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn set(id: i32, changes: &Changes) -> bool {
    let Some(mut ds) = stat_ds(id) else {
        return false;
    };
    let mut perm = ds.msg_perm;
    if let Some(uid) = changes.uid {
        perm.uid = uid as libc::uid_t;
    }
    if let Some(gid) = changes.gid {
        perm.gid = gid as libc::gid_t;
    }
    if let Some(mode) = changes.mode {
        // The member's own type, which is not `mode_t` everywhere: an
        // `unsigned short` on x86_64 glibc, an `unsigned int` on aarch64
        // glibc and a `mode_t` on macOS.
        perm.mode = mode as _;
    }
    ds.msg_perm = perm;
    if let Some(qbytes) = changes.qbytes {
        ds.msg_qbytes = qbytes as _;
    }
    // SAFETY: `ds` is a `msqid_ds` this frame owns, which `IPC_SET`
    // reads.
    unsafe { sys::msgctl(id, libc::IPC_SET, &raw mut ds) == 0 }
}

/// `msgctl(id, IPC_RMID, NULL)`: whether the queue was removed.
pub(crate) fn remove(id: i32) -> bool {
    // SAFETY: `IPC_RMID` reads no buffer, so the null one is allowed.
    unsafe { sys::msgctl(id, libc::IPC_RMID, std::ptr::null_mut()) == 0 }
}

/// The size of `struct php_msgbuf`'s `mtype`, which leads every message.
const TYPE: usize = size_of::<c_long>();

/// `msgsnd(id, {kind, text}, len, nowait ? IPC_NOWAIT : 0)`, or the
/// errno.
pub(crate) fn send(id: i32, kind: i64, text: &[u8], nowait: bool) -> Result<(), i32> {
    let mut buf = Vec::with_capacity(TYPE + text.len());
    buf.extend_from_slice(&c_long::from(kind).to_ne_bytes());
    buf.extend_from_slice(text);
    let flags = if nowait { libc::IPC_NOWAIT } else { 0 };
    // SAFETY: `buf` is the type and then `text.len()` bytes of text, the
    // `struct msgbuf` `msgsnd` reads.
    let done = unsafe { sys::msgsnd(id, buf.as_ptr().cast(), text.len(), flags) };
    if done == -1 { Err(errno()) } else { Ok(()) }
}

/// What `msg_receive`'s `$flags` ask of `msgrcv`.
#[derive(Clone, Copy)]
pub(crate) struct Receive {
    /// `MSG_EXCEPT`: the first message of any other type.
    pub(crate) except: bool,
    /// `MSG_NOERROR`: a longer message cut to the size.
    pub(crate) noerror: bool,
    /// `IPC_NOWAIT`: no waiting for one.
    pub(crate) nowait: bool,
}

/// `msgrcv(id, buf, max, kind, flags)`: the message's type and text, or
/// the errno.
pub(crate) fn receive(id: i32, max: usize, kind: i64, how: Receive) -> Result<(i64, Vec<u8>), i32> {
    let mut flags: c_int = 0;
    if how.except {
        flags |= sys::MSG_EXCEPT.unwrap_or(0);
    }
    if how.noerror {
        flags |= sys::MSG_NOERROR;
    }
    if how.nowait {
        flags |= libc::IPC_NOWAIT;
    }
    let mut buf = vec![0u8; TYPE + max];
    // SAFETY: `buf` is room for the type and `max` bytes of text, the
    // `struct msgbuf` `msgrcv` writes.
    let got = unsafe { sys::msgrcv(id, buf.as_mut_ptr().cast(), max, c_long::from(kind), flags) };
    let Ok(len) = usize::try_from(got) else {
        return Err(errno());
    };
    let mut head = [0u8; TYPE];
    head.copy_from_slice(&buf[..TYPE]);
    buf.truncate(TYPE + len);
    buf.drain(..TYPE);
    #[allow(clippy::useless_conversion)]
    Ok((i64::from(c_long::from_ne_bytes(head)), buf))
}
