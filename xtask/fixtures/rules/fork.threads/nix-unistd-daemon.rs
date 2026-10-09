//! nix's `daemon`, a safe function that forks, declared for Linux and the
//! BSDs and not for Apple's targets.
#[cfg(target_os = "linux")]
pub fn detached() -> bool {
    nix::unistd::daemon(false, false).is_ok()
}
