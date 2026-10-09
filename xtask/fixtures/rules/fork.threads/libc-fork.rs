//! `fork(2)` called from outside php_process, its one home.
pub fn forked() -> bool {
    // SAFETY: the fixture is linted, never run.
    unsafe { libc::fork() == 0 }
}
