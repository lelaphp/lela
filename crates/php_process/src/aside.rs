//! The thread a host's `main` started on, once another thread does the
//! process's work (design §5.6): out of every signal's way, so a
//! signal reaches the thread that stands for it under that thread's own
//! mask, and one that thread blocks waits for it, as it waited for the
//! main thread when the main thread did the work.

/// The calling thread out of every signal's way, for good: every signal
/// blocked on it (`pthread_sigmask(2)`), and then, where the system lands
/// a signal that every thread blocks on the process's oldest thread
/// (XNU's `get_signalact`), the thread ended, so the oldest is the one
/// that stands for it and a signal it blocks is delivered to it when it
/// unblocks; where the system keeps such a signal pending for the
/// process (Linux), the thread is left parked. The process goes on, and
/// ends where another thread ends it.
///
/// A thread forked from the one that stands for it is the child's only
/// thread either way, as php's child is.
///
/// ```php
/// pcntl_signal(SIGUSR1, function ($signo) { echo "handled\n"; });
/// pcntl_sigprocmask(SIG_BLOCK, [SIGUSR1]);
/// posix_kill(getmypid(), SIGUSR1);
/// usleep(50000);
/// pcntl_signal_dispatch();
/// echo "blocked\n";
/// pcntl_sigprocmask(SIG_UNBLOCK, [SIGUSR1]);
/// pcntl_signal_dispatch();
/// echo "unblocked\n";
/// $pid = pcntl_fork();
/// if ($pid == 0) { exit(3); }
/// pcntl_waitpid($pid, $status);
/// echo pcntl_wexitstatus($status), "\n";
/// ```
/// ```expect process
/// blocked
/// handled
/// unblocked
/// 3
/// ```
pub fn stand_aside() -> ! {
    block_every_signal();
    leave()
}

/// What `start` answers, called with every signal blocked on the calling
/// thread and the thread's own mask given back after, as `start` returns
/// or unwinds: a thread `start` creates inherits the mask
/// (`pthread_create(3)`), so it is out of every signal's way from its
/// first instruction, and a signal sent to the process is delivered to a
/// thread that does not block it, as php's one thread takes each.
/// One sent meanwhile waits until the mask is given back, pending on the
/// process (Linux) or on the oldest thread (XNU, see [`stand_aside`]).
pub fn started_aside<T>(start: impl FnOnce() -> T) -> T {
    let _given_back = GivenBack(block_every_signal());
    start()
}

/// The mask a thread had before [`block_every_signal`], set again as this
/// drops; `None` where the system refused the block, which changed nothing.
struct GivenBack(Option<libc::sigset_t>);

impl Drop for GivenBack {
    fn drop(&mut self) {
        if let Some(mask) = &self.0 {
            // SAFETY: `pthread_sigmask(2)` reads the set it is handed, live
            // for the call, and is handed no old set to write.
            let _ = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, mask, std::ptr::null_mut()) };
        }
    }
}

/// Every signal the system lets a thread block, blocked on the calling
/// thread, and the mask it had before; where the system refuses, the mask
/// stays as it was and the answer is `None`.
fn block_every_signal() -> Option<libc::sigset_t> {
    let mut all = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigfillset(3)` writes the whole of the set it is handed,
    // live for the call.
    if unsafe { libc::sigfillset(all.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: `sigfillset` answered 0, which it does once the set is
    // written.
    let all = unsafe { all.assume_init() };
    let mut before = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `pthread_sigmask(2)` reads the set it is handed and writes
    // the whole of the old one, both live for the call. It fails only on a
    // `how` it does not know, and SIG_SETMASK is one it knows.
    let blocked =
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &raw const all, before.as_mut_ptr()) };
    // SAFETY: `pthread_sigmask` answered 0, which it does once the old
    // set is written.
    (blocked == 0).then(|| unsafe { before.assume_init() })
}

/// XNU lands a signal every thread blocks on the oldest thread that has
/// not ended, so this one ends: `pthread_exit(3)`, which on Apple's
/// systems unwinds nothing, so no frame below is resumed or dropped and
/// what they hold is never freed; only the destructors of the thread's
/// own per-thread keys are called. The main thread's stack, which the system mapped and
/// not the thread library, stays mapped with the process's arguments and
/// environment on it.
#[cfg(target_vendor = "apple")]
fn leave() -> ! {
    // SAFETY: `pthread_exit(3)` ends the calling thread, unwinding none
    // of its frames on Apple's systems; the value is never joined.
    unsafe { libc::pthread_exit(std::ptr::null_mut()) }
}

/// Linux keeps a signal every thread blocks pending for the process,
/// delivered to the first thread that unblocks it, so this one stays,
/// parked: its `pthread_exit` would unwind the Rust frames below it.
#[cfg(not(target_vendor = "apple"))]
fn leave() -> ! {
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod tests {
    use super::started_aside;

    /// The calling thread's mask, with whether it blocks `signal`.
    fn blocks(signal: libc::c_int) -> bool {
        let mut mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: SIG_BLOCK with no new set changes nothing and writes the
        // whole of the old one, live for the call.
        let read =
            unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), mask.as_mut_ptr()) };
        assert_eq!(read, 0, "pthread_sigmask read the mask");
        // SAFETY: `pthread_sigmask` answered 0, having written the set.
        let mask = unsafe { mask.assume_init() };
        // SAFETY: `sigismember(3)` reads the set it is handed, live for the call.
        let member = unsafe { libc::sigismember(&raw const mask, signal) };
        member == 1
    }

    /// A thread started aside blocks every signal a process is sent to
    /// stop or to call a handler, and the thread that started it blocks
    /// none of them again once the start returns.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not model a thread's signal mask")]
    fn a_thread_started_aside_blocks_every_signal_and_its_starter_none() {
        let signals = [
            libc::SIGTERM,
            libc::SIGINT,
            libc::SIGUSR1,
            libc::SIGALRM,
            libc::SIGCHLD,
        ];
        assert!(
            !signals.iter().any(|s| blocks(*s)),
            "the test thread starts unmasked"
        );
        let started = started_aside(|| std::thread::spawn(move || signals.map(blocks)));
        assert!(
            !signals.iter().any(|s| blocks(*s)),
            "the starter's mask is given back"
        );
        let on_it = started.join().expect("the thread answers");
        assert_eq!(on_it, [true; 5], "the thread blocks each");
    }
}
