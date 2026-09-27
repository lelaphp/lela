//! The binding to the process calls `nix` leaves unsafe or does not
//! wrap (design §7.2, revision 35; `unsafe.homes`' `ext/*/src/ffi/`), the
//! one home of this crate's `unsafe`:
//!
//! - [`fork`] and [`install`], whose `nix` wrappers are `unsafe fn`;
//! - [`wait`] and [`waitid`], because php hands the program the status
//!   word and the `siginfo_t` as the kernel wrote them, which `nix`
//!   decodes into its own `WaitStatus` and does not give back;
//! - [`priority`] and [`set_priority`], `getpriority(2)` and
//!   `setpriority(2)`, which `nix` does not wrap;
//! - [`qos_class`] and [`set_qos_class`], Apple's
//!   `pthread_get_qos_class_np` and `pthread_set_qos_class_self_np`,
//!   which neither `nix` nor `libc` declares.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use nix::errno::Errno;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::unistd::ForkResult;
use php_ext::request::{Posted, signal_handler};

/// `fork(2)`. The child is a copy of the process with the calling thread
/// alone in it, as php's is: what the engine's other threads held (an
/// execution timer's wait, a mutex another request took in the engine's
/// shared tables) is gone or held for ever there, which is why the
/// extension is built for the `php` binary and no server host.
pub(crate) fn fork() -> nix::Result<ForkResult> {
    // SAFETY: `nix::unistd::fork` is `unsafe` because the child of a
    // process with several threads may call only async-signal-safe
    // functions until it `exec`s. The binary this extension is built for
    // serves its request on the one thread it has; the child goes on in
    // that request's own state, which is its copy alone.
    unsafe { nix::unistd::fork() }
}

/// What a signal's disposition becomes: php's `SIG_DFL`, `SIG_IGN`, or
/// the engine's signal post, which records it for the dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    Default,
    Ignore,
    Post,
}

/// `php_signal4(signo, func, restart, mask_all)` (`ext/pcntl/php_signal.c`):
/// `sigaction(2)` with `SA_ONSTACK | SA_SIGINFO`, `SA_RESTART` where
/// `restart`, and every signal blocked while the handler runs where
/// `mask_all`.
pub(crate) fn install(
    number: i32,
    disposition: Disposition,
    restart: bool,
    mask_all: bool,
) -> nix::Result<()> {
    let signal = Signal::try_from(number)?;
    let handler = match disposition {
        Disposition::Default => SigHandler::SigDfl,
        Disposition::Ignore => SigHandler::SigIgn,
        Disposition::Post => SigHandler::SigAction(signal_handler),
    };
    let mut flags = SaFlags::SA_ONSTACK | SaFlags::SA_SIGINFO;
    if restart {
        flags |= SaFlags::SA_RESTART;
    }
    let mask = if mask_all {
        SigSet::all()
    } else {
        SigSet::empty()
    };
    let action = SigAction::new(handler, flags, mask);
    // SAFETY: `nix::sys::signal::sigaction` is `unsafe` because the
    // handler installed runs asynchronously; the only handler installed
    // here is the engine's signal post, which performs atomic operations
    // on static storage and nothing else, so it is async-signal-safe.
    unsafe { nix::sys::signal::sigaction(signal, &action) }.map(|_| ())
}

/// A child's resource usage, as `wait4(2)` reports it.
pub(crate) type Usage = libc::rusage;

/// `waitpid(2)`, or `wait4(2)` where `usage` asks for the child's
/// resource usage: the child's id and the status word as the kernel
/// wrote it. `status` goes in as the caller's word, as php's goes in.
pub(crate) fn wait(
    pid: i32,
    status: &mut i32,
    options: i32,
    usage: Option<&mut Usage>,
) -> Result<i32, Errno> {
    let usage = usage.map_or(std::ptr::null_mut(), std::ptr::from_mut);
    // SAFETY: `status` is a live `i32` and `usage` is null or a live
    // `rusage`, the two out-parameters `wait4` writes.
    let child = unsafe { libc::wait4(pid, status, options, usage) };
    if child < 0 {
        return Err(Errno::last());
    }
    Ok(child)
}

/// `waitid(2)`: what the kernel wrote into `siginfo_t`, in the shape
/// the engine's signal post hands a dispatch.
pub(crate) fn waitid(idtype: i64, id: i64, options: i32) -> Result<Posted, Errno> {
    // SAFETY: `siginfo_t` is plain integers and pointers, for which all
    // zeroes is a value; php `memset`s it the same way.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (idtype, id) = (idtype as libc::idtype_t, id as libc::id_t);
    // SAFETY: `info` is a live `siginfo_t` the call writes.
    let status = unsafe { libc::waitid(idtype, id, &raw mut info, options) };
    if status == -1 {
        return Err(Errno::last());
    }
    Ok(read_siginfo(&info))
}

/// The fields php reads out of a `siginfo_t`.
#[cfg(target_vendor = "apple")]
fn read_siginfo(info: &libc::siginfo_t) -> Posted {
    Posted {
        number: info.si_signo,
        errno: info.si_errno,
        code: info.si_code,
        pid: info.si_pid,
        uid: info.si_uid,
        status: info.si_status,
        addr: info.si_addr as usize,
    }
}

/// The fields php reads out of a `siginfo_t`, through the accessors
/// `libc` gives Linux's union.
#[cfg(not(target_vendor = "apple"))]
fn read_siginfo(info: &libc::siginfo_t) -> Posted {
    // SAFETY: the union is integers the kernel filled or zeroes.
    let pid = unsafe { info.si_pid() };
    // SAFETY: as above.
    let uid = unsafe { info.si_uid() };
    // SAFETY: as above.
    let status = unsafe { info.si_status() };
    // SAFETY: as above.
    let addr = unsafe { info.si_addr() } as usize;
    Posted {
        number: info.si_signo,
        errno: info.si_errno,
        code: info.si_code,
        pid,
        uid,
        status,
        addr,
    }
}

/// `getpriority(2)`, whose `-1` is a priority as well as the failure, so
/// `errno` is cleared first and read after, as php reads it.
pub(crate) fn priority(which: i64, who: i64) -> Result<i32, Errno> {
    Errno::clear();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (which, who) = (which as _, who as libc::id_t);
    // SAFETY: `getpriority` takes two integers and touches no memory of
    // the caller's.
    let priority = unsafe { libc::getpriority(which, who) };
    match Errno::last_raw() {
        0 => Ok(priority),
        _ => Err(Errno::last()),
    }
}

/// `setpriority(2)`.
pub(crate) fn set_priority(which: i64, who: i64, priority: i64) -> Result<(), Errno> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (which, who, priority) = (which as _, who as libc::id_t, priority as libc::c_int);
    // SAFETY: `setpriority` takes three integers and touches no memory of
    // the caller's.
    let failed = unsafe { libc::setpriority(which, who, priority) };
    if failed != 0 {
        return Err(Errno::last());
    }
    Ok(())
}

/// Apple's `qos_class_t` values (`<sys/qos.h>`).
#[cfg(target_vendor = "apple")]
pub(crate) mod qos {
    pub(crate) const USER_INTERACTIVE: u32 = 0x21;
    pub(crate) const USER_INITIATED: u32 = 0x19;
    pub(crate) const DEFAULT: u32 = 0x15;
    pub(crate) const UTILITY: u32 = 0x11;
    pub(crate) const BACKGROUND: u32 = 0x09;
}

#[cfg(target_vendor = "apple")]
unsafe extern "C" {
    fn pthread_get_qos_class_np(
        thread: libc::pthread_t,
        qos_class: *mut u32,
        relative_priority: *mut libc::c_int,
    ) -> libc::c_int;
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: libc::c_int)
    -> libc::c_int;
}

/// `pthread_get_qos_class_np(pthread_self(), &qos_class, NULL)`: the
/// calling thread's class, or the class read and the `errno` where the
/// call failed.
#[cfg(target_vendor = "apple")]
pub(crate) fn qos_class() -> Result<u32, (u32, Errno)> {
    let mut class = 0;
    // SAFETY: `pthread_self` takes nothing and cannot fail.
    let this = unsafe { libc::pthread_self() };
    // SAFETY: `class` is a live `u32` the call writes, and the relative
    // priority it may write too is asked for with a null pointer, which
    // the call reads as "not wanted".
    let failed = unsafe { pthread_get_qos_class_np(this, &raw mut class, std::ptr::null_mut()) };
    if failed != 0 {
        return Err((class, Errno::last()));
    }
    Ok(class)
}

/// `pthread_set_qos_class_self_np(class, 0)`.
#[cfg(target_vendor = "apple")]
pub(crate) fn set_qos_class(class: u32) -> Result<(), Errno> {
    // SAFETY: the call takes two integers and touches no memory of the
    // caller's.
    let failed = unsafe { pthread_set_qos_class_self_np(class, 0) };
    if failed != 0 {
        return Err(Errno::last());
    }
    Ok(())
}
