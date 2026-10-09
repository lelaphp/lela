//! Denies the rule's lints at the root, and two modules below lift one
//! each: `lifted` by an inner `allow`, `outer` by the `allow` on its `mod`
//! item here. Both are named, with the lints they lift.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

pub mod lifted;
#[allow(clippy::panic)]
pub mod outer;
