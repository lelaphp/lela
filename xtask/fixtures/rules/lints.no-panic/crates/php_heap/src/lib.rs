//! Denies the rule's lints at the crate root, in the shape `new-ext`
//! writes: every source under it passes.
// seams.toml rule lints.no-panic: (fixture)
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

pub mod deep;
