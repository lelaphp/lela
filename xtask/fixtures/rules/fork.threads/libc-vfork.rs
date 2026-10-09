//! `vfork(2)`, which libc declares for Linux and not for Apple's targets.
#[cfg(target_os = "linux")]
pub fn forked() -> bool {
    // SAFETY: the fixture is linted, never run.
    unsafe { libc::vfork() == 0 }
}
