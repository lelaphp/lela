//! nix's `fork`, through the grouped import #1883's pattern missed.
use nix::unistd::{ForkResult, fork};

pub fn forked() -> bool {
    // SAFETY: the fixture is linted, never run.
    matches!(unsafe { fork() }, Ok(ForkResult::Child))
}
