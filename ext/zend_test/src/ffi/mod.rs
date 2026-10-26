//! The one act of `test.c` this crate does in C's terms (`unsafe.homes`'
//! `ext/*/src/ffi/`): `zend_test_crash`'s end of the process, the
//! `SIGSEGV` php's `php_printf("%s", (char *) 1)` takes when it reads
//! the byte at address one (ruled on #2976).
//!
//! No invalid pointer is read here. The process is sent the signal that
//! read delivers, under the disposition that read meets in php's CLI,
//! the default one, so the process ends as php's does and a shell
//! reports the same status.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

/// End the process with `SIGSEGV`, as php's read of `(char *) 1` does.
///
/// The Rust runtime's own `SIGSEGV` handler, which tells a stack
/// overflow from any other fault, would take a raised signal for no
/// overflow and return, and a thread that has the signal blocked would
/// leave it pending; php's fault meets neither. So the disposition is
/// set back to the default and the signal unblocked on this thread
/// first, and then the signal is raised, which ends the process before
/// `raise` returns.
pub(crate) fn segfault() -> ! {
    // SAFETY: `SIG_DFL` is a disposition `signal(2)` takes for any
    // signal; nothing of this process is left to run once the default
    // action of the signal set here is taken.
    unsafe { libc::signal(libc::SIGSEGV, libc::SIG_DFL) };
    let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigemptyset` writes the whole set it is pointed at, which
    // is this local's room.
    unsafe { libc::sigemptyset(set.as_mut_ptr()) };
    // SAFETY: the set is the room `sigemptyset` initialised, and
    // `SIGSEGV` is a valid signal to add.
    unsafe { libc::sigaddset(set.as_mut_ptr(), libc::SIGSEGV) };
    // SAFETY: the set is initialised, and a null old set asks for no
    // write back; unblocking one signal on this thread is all it does.
    unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, set.as_ptr(), std::ptr::null_mut()) };
    // SAFETY: `raise(3)` sends a valid signal to this thread; its
    // default action, set above, ends the process.
    unsafe { libc::raise(libc::SIGSEGV) };
    // The default action of an unblocked `SIGSEGV` does not return; were
    // it to, the process still ends rather than run on past the fault.
    std::process::abort()
}

/// `raise(3)` of `signal` to this thread: whether it was sent, which a
/// number that names no signal is not.
pub(crate) fn raise(signal: libc::c_int) -> bool {
    // SAFETY: `raise(3)` takes any number and refuses one that names no
    // signal; a signal sent meets the disposition the process set.
    unsafe { libc::raise(signal) == 0 }
}
