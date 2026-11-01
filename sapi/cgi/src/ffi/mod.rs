//! php-cgi's signals under `-b`: `fcgi_setup_signals`
//! (`Z/main/fastcgi.c:423-443`), which `fcgi_listen` runs, so a `SIGTERM`
//! or a `SIGUSR1` ends the accept loop and the process with it, a
//! `SIGPIPE` too where nothing took it; and under `PHP_FCGI_CHILDREN`,
//! `fastcgi_cleanup` (`Z/sapi/cgi/cgi_main.c:1458-1474`), the parent's
//! `SIGTERM`, `SIGINT` and `SIGQUIT`, which put `SIGTERM` back to
//! `fcgi_setup_signals`'s handler and send it to the whole process group,
//! the parent among it, so the children and the parent end together. And
//! `accept` itself, which a signal interrupts here, where std's retries,
//! and which a stop handled after the loop's check fails at once
//! ([`accepting_on`]).
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
/// The number the accept loop accepts on, which [`on_term`] takes from
/// the listening socket; -1 where none is named or it was taken.
static LISTENING: AtomicI32 = AtomicI32::new(-1);

/// `fcgi_signal_handler`, and php-fpm's `sig_soft_quit` after it
/// ([`stop_accepting`]).
extern "C" fn on_term(signo: libc::c_int) {
    if signo == libc::SIGTERM || signo == libc::SIGUSR1 {
        SHUTDOWN.store(true, Ordering::Relaxed);
        stop_accepting();
    }
}

/// php-fpm's `sig_soft_quit` (`Z/sapi/fpm/fpm/fpm_signals.c:142-153`):
/// the number the accept loop accepts on made to name a socket that
/// listens on nothing, so an `accept` the loop enters after its
/// `in_shutdown` check fails at once where it would wait for the next
/// connection. php-cgi's handler sets `in_shutdown` alone, and a stop handled
/// between the check and `accept` is lost until a connection comes
/// (#3869). The socket itself is kept open under another number until the
/// process ends, as php-cgi keeps it, so a web server's connection is
/// queued and reset at the exit as php-cgi's is, and a sibling child's
/// `accept` is untouched. errno is left as the handler found it.
fn stop_accepting() {
    let fd = LISTENING.swap(-1, Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let errno = nix::errno::Errno::last_raw();
    // SAFETY: `fcntl(F_DUPFD_CLOEXEC)` is async-signal-safe; the number it
    // opens is never closed, which keeps the socket listening until the
    // process ends.
    unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    // SAFETY: `socket` is async-signal-safe and touches no memory.
    let unbound = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if unbound >= 0 {
        // SAFETY: `dup2` is async-signal-safe; `fd` is the number the
        // listening value owns, which now names the unbound socket and
        // which that value closes as its own.
        unsafe { libc::dup2(unbound, fd) };
        // SAFETY: `close` of the number `socket` just opened, which nothing
        // else names.
        unsafe { libc::close(unbound) };
    }
    nix::errno::Errno::set_raw(errno);
}

/// The listening socket the accept loop accepts on, from its first
/// `in_shutdown` check on, which [`on_term`] takes the number from.
pub(crate) fn accepting_on(listening: BorrowedFd<'_>) {
    LISTENING.store(listening.as_raw_fd(), Ordering::Relaxed);
}

/// The listening socket no longer accepted on, its number about to be
/// closed: [`on_term`] leaves it alone.
pub(crate) fn not_accepting_on(listening: BorrowedFd<'_>) {
    LISTENING
        .compare_exchange(
            listening.as_raw_fd(),
            -1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        )
        .ok();
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
