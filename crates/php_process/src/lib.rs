//! The process-wide mechanisms (design §4.1, #1779): what a process has
//! one of, whichever request or engine asks. Three of them, each a
//! mechanism that decides nothing of when or why it is used:
//!
//! - [`fork`], the one call in the workspace that makes a process
//!   (`fork.home`); `php_engine` decides around it, the threads it joins
//!   first and what the child restores (#1807);
//! - [`post`], the pool a signal handler writes and safe code reads, with
//!   the wake set it raises (design §5.8, revision 38), which `php_engine`
//!   holds the one instance of;
//! - [`reset_lc_ctype_locale`], the process's `LC_CTYPE`, set once before
//!   any extension starts.
//!
//! Revision 38 kept the pool in `php_heap` until more than signals and
//! `LC_CTYPE` arrived; `fork` is what arrived, and the two moved here
//! unchanged. The crate is an `unsafe` home of its own, every block one
//! operation with its `SAFETY` comment.
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

mod fork;
mod locale;
pub mod post;

pub use fork::{Forked, Pid, fork};
pub use locale::reset_lc_ctype_locale;
