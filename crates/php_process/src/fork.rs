//! `fork(2)`, the one call in the workspace that makes a process (#1779;
//! `fork.home`), and nothing around it. Whether a fork may happen, which
//! of the engine's own threads are joined first, what the child restores
//! (php's `php_child_init`) and what the parent re-arms are `php_engine`'s
//! (#1807); the `process` seam makes the engine this crate's one consumer,
//! so pcntl reaches a fork through the request and a host through
//! `php_sapi`, never here.

use std::io;

/// Which side of a [`fork`] the caller stands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Forked {
    /// The process that called, with the id of the one it made, which
    /// php's `pcntl_fork` returns.
    Parent { child: Pid },
    /// The process made: a copy of the caller in which the calling thread
    /// alone goes on.
    Child,
}

/// A process id as `fork` hands it to the parent: positive, php's `pid_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pid(libc::pid_t);

impl Pid {
    /// The id as the kernel numbers it, which `waitpid(2)` and `kill(2)`
    /// take and php's `pcntl_fork` returns as an int.
    #[must_use]
    pub const fn as_raw(self) -> i32 {
        self.0
    }
}

/// `fork(2)`: the process copied, and this call returning in both copies,
/// as [`Forked::Parent`] in the caller and [`Forked::Child`] in the copy.
///
/// In the child the calling thread alone goes on. What another thread of
/// the parent held at the instant of the call stays held there for ever,
/// a mutex of the C library's or of the engine's shared tables among it,
/// so the child of a process with other threads may call only
/// async-signal-safe functions until it `exec`s or `_exit`s, as POSIX
/// states for `fork`. Returning [`Forked::Child`] allocates and waits on
/// nothing, so a caller that `exec`s or exits straight after stays within
/// that. A process whose calling thread is its only one holds nothing it
/// could lose, and its child goes on as the parent would: that is the
/// process the engine makes before it forks (#1807).
///
/// # Errors
///
/// The OS error `fork(2)` failed with, `EAGAIN` and `ENOMEM` among them,
/// which php's `pcntl_fork` renders as its warning; no process was made.
pub fn fork() -> io::Result<Forked> {
    // SAFETY: `fork` takes no argument and writes no memory of this
    // process. In the child the calling thread alone runs, so nothing
    // this thread reaches there is written by another, and what another
    // held stays held, which can stall the child and cannot race it.
    // POSIX's async-signal-safe rule for the child of a process with
    // other threads is not one this function can see or enforce: no type
    // here witnesses that the process has one thread, so it rests on the
    // engine, the one consumer the `process` seam admits, whose one home
    // for fork (#1807) calls this only while it holds the type that says
    // every thread it started is joined and no other request is in
    // flight.
    let id = unsafe { libc::fork() };
    match id {
        -1 => Err(io::Error::last_os_error()),
        0 => Ok(Forked::Child),
        child => Ok(Forked::Parent { child: Pid(child) }),
    }
}

#[cfg(test)]
mod tests {
    use super::{Forked, fork};

    /// The parent is handed the child's id, the child goes on in the same
    /// code, and the status the child exits with is the parent's to read.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not model fork(2)")]
    fn the_parent_reads_the_status_the_child_exits_with() -> std::io::Result<()> {
        let child = match fork()? {
            // SAFETY: `_exit` is async-signal-safe, the one call the child
            // of the test harness's several threads makes.
            Forked::Child => unsafe { libc::_exit(7) },
            Forked::Parent { child } => child,
        };
        let mut status = 0;
        let waited = loop {
            // SAFETY: `status` is a live `c_int` that `waitpid` writes once.
            let waited = unsafe { libc::waitpid(child.as_raw(), &raw mut status, 0) };
            let interrupted = std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR);
            if waited != -1 || !interrupted {
                break waited;
            }
        };
        assert_eq!(waited, child.as_raw());
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 7);
        Ok(())
    }
}
