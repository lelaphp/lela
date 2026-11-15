//! `fpm_signals.c`'s handlers: the master's, which write one character
//! naming the signal into the pipe its event loop reads, and a child's
//! for `SIGQUIT`, which closes the listening socket so the child stops
//! accepting and ends once its request is served.

use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal};

/// The write end of the master's signal pipe, `sp[1]`.
static PIPE: AtomicI32 = AtomicI32::new(-1);
/// `fpm_globals.parent_pid`, which the master's handler checks.
static MASTER: AtomicI32 = AtomicI32::new(0);
/// A child's listening socket, `fpm_globals.listening_socket`: owned by
/// this static alone, closed by [`soft_quit`] and by nothing else, and -1
/// once closed.
static LISTENING: AtomicI32 = AtomicI32::new(-1);
/// `in_shutdown`: a child was told to finish (`fcgi_terminate`).
pub(crate) static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// `sig_handler`: the signal's character into the pipe, where this is
/// the master; a forked child that has not yet set its own handlers
/// passes it over.
extern "C" fn master_handler(signo: libc::c_int) {
    // SAFETY: `getpid` reads the process's id and is async-signal-safe.
    if unsafe { libc::getpid() } != MASTER.load(Ordering::Relaxed) {
        return;
    }
    let c: u8 = match signo {
        libc::SIGTERM => b'T',
        libc::SIGINT => b'I',
        libc::SIGUSR1 => b'1',
        libc::SIGUSR2 => b'2',
        libc::SIGQUIT => b'Q',
        libc::SIGCHLD => b'C',
        _ => return,
    };
    let fd = PIPE.load(Ordering::Relaxed);
    // SAFETY: `write` of one byte from a live local to the pipe's write
    // end is async-signal-safe; errno is left as the handler found it
    // because nothing reads it between the handler's return and the
    // interrupted call's own result.
    let _ = unsafe { libc::write(fd, (&raw const c).cast(), 1) };
}

/// `sig_soft_quit`: the listening socket closed so `accept` fails, a
/// socket opened to take its number, and the child told to finish.
///
/// The number is taken out of [`LISTENING`] before it is closed, so a
/// second `SIGQUIT` closes nothing; php's closes the number again, which by
/// then names the socket opened here or whatever took the number since.
extern "C" fn soft_quit(_signo: libc::c_int) {
    let fd = LISTENING.swap(-1, Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: `close` of the child's listening socket, which
        // `LISTENING` alone owns and no Rust value holds, is
        // async-signal-safe and touches no memory.
        unsafe { libc::close(fd) };
        // SAFETY: `socket` is async-signal-safe; the socket it opens takes
        // the lowest free number, the one just closed where no lower one
        // is free, as php's handler does, and is never used or closed.
        unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    }
    SHUTDOWN.store(true, Ordering::Relaxed);
}

/// A child's listening socket, which it accepts from by number: owned by
/// the `SIGQUIT` handler, which closes it while a Rust value may still
/// name it, so no Rust value owns it and nothing drops it. The
/// number it was handed over as, which [`LISTENING`] holds until then.
pub(crate) struct Listening(RawFd);

impl Listening {
    /// `accept(2)` on the listening socket, the connection owned by the
    /// caller and closed on `exec`; `EBADF` once the handler closed it.
    pub(crate) fn accept(&self) -> nix::Result<OwnedFd> {
        if LISTENING.load(Ordering::Relaxed) != self.0 {
            return Err(nix::errno::Errno::EBADF);
        }
        let accepted = nix::sys::socket::accept(self.0)?;
        // SAFETY: `accept` answered a descriptor it opened, which nothing
        // else in the process names.
        let accepted = unsafe { OwnedFd::from_raw_fd(accepted) };
        nix::fcntl::fcntl(
            &accepted,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        )?;
        Ok(accepted)
    }
}

/// `sigaction` of `handler` for each signal, the mask full while it runs.
fn install(signals: &[Signal], handler: SigHandler, flags: SaFlags) -> nix::Result<()> {
    let action = SigAction::new(handler, flags, SigSet::all());
    for signal in signals {
        // SAFETY: the handlers installed are the two above, which only
        // make async-signal-safe calls and store to atomics, or `SIG_DFL`.
        unsafe { nix::sys::signal::sigaction(*signal, &action) }?;
    }
    Ok(())
}

/// `fpm_signals_init_main`'s `sigaction` calls: the master's six signals
/// written into the pipe whose write end is `pipe`, in the master `pid`.
pub(crate) fn master_signals(pipe: RawFd, pid: i32) -> nix::Result<()> {
    PIPE.store(pipe, Ordering::Relaxed);
    MASTER.store(pid, Ordering::Relaxed);
    install(
        &[
            Signal::SIGTERM,
            Signal::SIGINT,
            Signal::SIGUSR1,
            Signal::SIGUSR2,
            Signal::SIGCHLD,
            Signal::SIGQUIT,
        ],
        SigHandler::Handler(master_handler),
        SaFlags::empty(),
    )
}

/// `fpm_signals_init_child`'s `sigaction` calls: every master signal back
/// to its default but `SIGQUIT`, which closes `listening` and restarts
/// the call it interrupted; `listening` handed to the handler, and the
/// child accepting from it by number.
pub(crate) fn child_soft_quit(listening: OwnedFd) -> nix::Result<Listening> {
    let fd = listening.into_raw_fd();
    LISTENING.store(fd, Ordering::Relaxed);
    restore_defaults()?;
    install(
        &[Signal::SIGQUIT],
        SigHandler::Handler(soft_quit),
        SaFlags::SA_RESTART,
    )?;
    Ok(Listening(fd))
}

/// The master's signals back to `SIG_DFL`, as a child and an exec take
/// them.
pub(crate) fn restore_defaults() -> nix::Result<()> {
    install(
        &[
            Signal::SIGTERM,
            Signal::SIGINT,
            Signal::SIGUSR1,
            Signal::SIGUSR2,
            Signal::SIGCHLD,
        ],
        SigHandler::SigDfl,
        SaFlags::empty(),
    )
}
