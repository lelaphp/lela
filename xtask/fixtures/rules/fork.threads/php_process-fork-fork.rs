//! A second caller of `php_process::fork`, by the name the crate re-exports
//! it under: the call #1807 left to `fork::Alone` alone.
use php_process::{Forked, fork};

pub fn forked() -> bool {
    matches!(fork(), Ok(Forked::Child))
}
