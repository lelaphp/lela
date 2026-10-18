//! php-cgi's signals under `-b`: `fcgi_setup_signals`
//! (`Z/main/fastcgi.c:423-443`), which `fcgi_listen` runs, so a `SIGTERM`
//! or a `SIGUSR1` ends the accept loop and the process with it, a
//! `SIGPIPE` too where nothing took it; and under `PHP_FCGI_CHILDREN`,
//! `fastcgi_cleanup` (`Z/sapi/cgi/cgi_main.c:1458-1474`), the parent's
//! `SIGTERM`, `SIGINT` and `SIGQUIT`, which put `SIGTERM` back to
//! `fcgi_setup_signals`'s handler and send it to the whole process group,
//! the parent among it, so the children and the parent end together. And
//! `accept` itself, which a signal interrupts here, where std's retries.
// seams.toml rule unsafe.one-op: every unsafe block is one operation with
// a SAFETY comment.
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, kill, sigaction};
use nix::unistd::Pid;

/// `in_shutdown`: a `SIGTERM` or a `SIGUSR1` arrived.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
/// `pgroup`, the process group the parent made its own.
static GROUP: AtomicI32 = AtomicI32::new(0);
/// `exit_signal`: the parent was told to stop.
static EXIT: AtomicBool = AtomicBool::new(false);

/// `fcgi_signal_handler`.
extern "C" fn on_term(signo: libc::c_int) {
    if signo == libc::SIGTERM || signo == libc::SIGUSR1 {
        SHUTDOWN.store(true, Ordering::Relaxed);
    }
}

/// `fastcgi_cleanup`: `sigaction(SIGTERM, &old_term, 0)`, `old_term`
/// being [`on_term`], then `kill(-pgroup, SIGTERM)`, which tells the
/// parent too to finish.
extern "C" fn cleanup(_signo: libc::c_int) {
    let restored = SigAction::new(
        SigHandler::Handler(on_term),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: `sigaction` is async-signal-safe, and `on_term` stores one
    // atomic.
    unsafe { sigaction(Signal::SIGTERM, &restored) }.ok();
    let group = GROUP.load(Ordering::Relaxed);
    kill(Pid::from_raw(-group), Signal::SIGTERM).ok();
    EXIT.store(true, Ordering::Relaxed);
}

/// `exit_signal`: whether the parent was told to stop.
pub(crate) fn told_to_exit() -> bool {
    EXIT.load(Ordering::Relaxed)
}

/// `fcgi_setup_signals`: [`on_term`] for `SIGUSR1` and `SIGTERM`, and for
/// `SIGPIPE` where its action was the default.
pub(crate) fn shutdown_on_term() {
    let action = SigAction::new(
        SigHandler::Handler(on_term),
        SaFlags::empty(),
        SigSet::empty(),
    );
    for signal in [Signal::SIGUSR1, Signal::SIGTERM] {
        // SAFETY: `on_term` stores one atomic.
        unsafe { sigaction(signal, &action) }.ok();
    }
    // SAFETY: as above; what stood is read back.
    if let Ok(old) = unsafe { sigaction(Signal::SIGPIPE, &action) }
        && !matches!(old.handler(), SigHandler::SigDfl)
    {
        // SAFETY: what stood is put back.
        unsafe { sigaction(Signal::SIGPIPE, &old) }.ok();
    }
}

/// Whether a `SIGTERM` or a `SIGUSR1` arrived, `in_shutdown`.
pub(crate) fn shutting_down() -> bool {
    SHUTDOWN.load(Ordering::Relaxed)
}

/// The parent's stop signals made [`cleanup`], over `group`.
pub(crate) fn kill_group_on_stop(group: Pid) -> nix::Result<()> {
    GROUP.store(group.as_raw(), Ordering::Relaxed);
    let action = SigAction::new(
        SigHandler::Handler(cleanup),
        SaFlags::empty(),
        SigSet::empty(),
    );
    for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGQUIT] {
        // SAFETY: `cleanup` makes only async-signal-safe calls and reads
        // one atomic.
        unsafe { sigaction(signal, &action) }?;
    }
    Ok(())
}

/// A child's `sigaction(SIGTERM, &old_term, 0)` and its kin: `SIGTERM`
/// back to [`on_term`], `SIGINT` and `SIGQUIT` to their default action.
pub(crate) fn child_stops() {
    let term = SigAction::new(
        SigHandler::Handler(on_term),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: `on_term` stores one atomic.
    unsafe { sigaction(Signal::SIGTERM, &term) }.ok();
    let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    for signal in [Signal::SIGINT, Signal::SIGQUIT] {
        // SAFETY: the default action runs no code of this process.
        unsafe { sigaction(signal, &default) }.ok();
    }
}

/// `accept(listening)`, once: the connection, or the error, `EINTR`
/// where a signal came first.
pub(crate) fn accept(listening: BorrowedFd<'_>) -> nix::Result<OwnedFd> {
    let fd = nix::sys::socket::accept(listening.as_raw_fd())?;
    // SAFETY: `accept` just opened the descriptor and nothing else names
    // it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
