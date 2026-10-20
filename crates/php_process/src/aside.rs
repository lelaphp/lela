//! The thread a host's `main` started on, once another thread does the
//! process's work (design §5.6, #3067): out of every signal's way, so a
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

/// Every signal the system lets a thread block, blocked on the calling
/// thread; where the system refuses, the mask stays as it was.
fn block_every_signal() {
    let mut all = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigfillset(3)` writes the whole of the set it is handed,
    // live for the call.
    if unsafe { libc::sigfillset(all.as_mut_ptr()) } != 0 {
        return;
    }
    // SAFETY: `sigfillset` answered 0, which it does once the set is
    // written.
    let all = unsafe { all.assume_init() };
    // SAFETY: `pthread_sigmask(2)` reads the set it is handed, live for
    // the call, and is handed no old set to write.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &raw const all, std::ptr::null_mut()) };
}

/// XNU lands a signal every thread blocks on the oldest thread that has
/// not ended, so this one ends: `pthread_exit(3)`, which on Apple's
/// systems unwinds nothing, so no frame below is resumed or dropped and
/// what they hold stays as it is, and the main thread's stack, which the
/// system mapped and not the thread library, stays mapped with the
/// process's arguments and environment on it.
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
