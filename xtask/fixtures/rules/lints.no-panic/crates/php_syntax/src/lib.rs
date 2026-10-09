//! The library denies the rule's lints; the binary beside it, a crate
//! root of its own, does not, and is named.
#![forbid(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

pub fn one() -> u8 {
    1
}
