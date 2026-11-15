//! `fork(2)`, the one call in the workspace that makes a process
//! (`fork.home`), and nothing around it. Whether a fork may happen, which
//! of the engine's own threads are joined first, what the child restores
//! (php's `php_child_init`) and what the parent re-arms are `php_engine`'s;
//! the `process` seam makes the engine the one crate that imports
//! this one, so pcntl reaches a fork through the request and a host through
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
/// process the engine makes before it forks.
///
/// # Errors
///
/// The OS error `fork(2)` failed with, `EAGAIN` and `ENOMEM` among them,
/// which php's `pcntl_fork` renders as its warning; no process was made.
pub fn fork() -> io::Result<Forked> {
    #[expect(clippy::disallowed_methods, reason = "the home of fork(2)")]
    // SAFETY: `fork` takes no argument and writes no memory of this
    // process. In the child the calling thread alone runs, so nothing
    // this thread reaches there is written by another, and what another
    // held stays held, which can stall the child and cannot race it.
    // POSIX's async-signal-safe rule for the child of a process with
    // other threads is not one this function can see or enforce: no type
    // here witnesses that the process has one thread, so it rests on the
    // engine, the one importer the `process` seam admits, whose one home
    // for fork calls this only while it holds the type that says
    // every thread it started is joined and no other request is in
    // flight; a host's own threads are the host's to keep out of a
    // process that forks.
    let id = unsafe { libc::fork() };
    match id {
        -1 => Err(io::Error::last_os_error()),
        0 => {
            executable_jit_pages();
            Ok(Forked::Child)
        }
        child => Ok(Forked::Parent { child: Pid(child) }),
    }
}

/// Makes the child's thread see its `MAP_JIT` pages executable, as the
/// parent's thread did when it forked.
///
/// On Apple silicon whether such a page is writable or executable is a
/// state of each thread, so the child sets its one thread's state itself.
/// Whatever writes machine code in the process, libpcre2 and the compiled
/// tier, leaves its pages executable between writes, so executable is the
/// state the child is owed.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn executable_jit_pages() {
    // SAFETY: sets one bit of this thread's state and touches no memory;
    // it takes no lock, so the child of a process with other threads may
    // make it.
    unsafe { libc::pthread_jit_write_protect_np(1) };
}

/// Elsewhere a page's protection is the process's, which `fork` copies.
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
const fn executable_jit_pages() {}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
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

    /// Code the parent wrote to a `MAP_JIT` page just before it forked
    /// runs in the child, each of four thousand children exiting with
    /// what the code returns.
    #[test]
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[cfg_attr(miri, ignore = "Miri does not model fork(2)")]
    fn a_child_runs_the_jit_code_its_parent_wrote() -> std::io::Result<()> {
        unsafe extern "C" {
            fn sys_icache_invalidate(start: *mut libc::c_void, len: libc::size_t);
        }
        /// `mov w0, #42; ret`.
        const CODE: [u32; 2] = [0x5280_0540, 0xd65f_03c0];
        const PAGE: usize = 16 * 1024;
        let mut killed = 0;
        for _ in 0..4000 {
            // SAFETY: a fresh anonymous private mapping, no address hinted.
            let page = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    PAGE,
                    libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                    libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                    -1,
                    0,
                )
            };
            assert_ne!(page, libc::MAP_FAILED);
            // SAFETY: lets this thread write its MAP_JIT pages.
            unsafe { libc::pthread_jit_write_protect_np(0) };
            // SAFETY: the page is PAGE bytes, writable to this thread now.
            unsafe { std::ptr::copy_nonoverlapping(CODE.as_ptr(), page.cast::<u32>(), CODE.len()) };
            // SAFETY: makes this thread's MAP_JIT pages executable again.
            unsafe { libc::pthread_jit_write_protect_np(1) };
            // SAFETY: the range is the code just written, inside the page.
            unsafe { sys_icache_invalidate(page, size_of_val(&CODE)) };
            // SAFETY: the page holds a complete function of this signature.
            let run =
                unsafe { std::mem::transmute::<*mut libc::c_void, extern "C" fn() -> i32>(page) };
            assert_eq!(run(), 42);
            let child = match fork()? {
                // SAFETY: `_exit` is async-signal-safe; the call before it
                // runs the page's two instructions and touches nothing else.
                Forked::Child => unsafe { libc::_exit(run()) },
                Forked::Parent { child } => child,
            };
            let mut status = 0;
            // SAFETY: `status` is a live `c_int` that `waitpid` writes once.
            while unsafe { libc::waitpid(child.as_raw(), &raw mut status, 0) } == -1 {}
            if libc::WIFSIGNALED(status) {
                killed += 1;
            } else {
                assert_eq!(libc::WEXITSTATUS(status), 42);
            }
            // SAFETY: the mapping made above, unmapped once.
            unsafe { libc::munmap(page, PAGE) };
        }
        assert_eq!(killed, 0, "children killed running their parent's JIT code");
        Ok(())
    }
}
