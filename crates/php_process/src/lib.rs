//! The process-wide mechanisms (design §4.1, #1779): what a process has
//! one of, whichever request or engine asks. Each is a mechanism that
//! decides nothing of when or why it is used:
//!
//! - [`fork`], the one call in the workspace that makes a process
//!   (`fork.home`); `php_engine` decides around it, the threads it joins
//!   first and what the child restores (#1807);
//! - [`post`], the pool a signal handler writes and safe code reads, with
//!   the wake set it raises (design §5.8, revision 38), which `php_engine`
//!   holds the one instance of;
//! - [`reset_lc_ctype_locale`], the process's `LC_CTYPE`, set once before
//!   any extension starts, and [`is_space`], C's `isspace` under the
//!   thread's;
//! - [`stand_aside`], a host's main thread out of every signal's way once
//!   the thread `php_engine` moved the process's work onto stands for it
//!   (design §5.6, #3067), and [`started_aside`], a thread the engine
//!   starts for itself out of it from its first instruction (#3861);
//! - [`process`], the process's identity, its real user and group and its
//!   supplementary groups (revision 41);
//! - the calls on the process's file descriptors that `std` has no answer
//!   for, [`poll_fd`], [`select_fds`] and the socket calls, which the
//!   streams layer reaches through `php_engine`'s face (#1874).
//!
//! Revision 38 kept the pool in `php_heap` until more than signals and
//! `LC_CTYPE` arrived; `fork` is what arrived, and the two moved here
//! unchanged. The process's identity and its file descriptors followed by
//! the rule that `php_heap` holds memory and stacks and this crate the
//! process and its file descriptors (#1935), moved as they stood. The
//! crate is an `unsafe` home of its own, every block one operation with
//! its `SAFETY` comment.
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]
// The denials seams.toml's lints.no-panic holds php_heap to, declared here as
// php_heap declares them; the rule's scope does not name this crate yet.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

mod aside;
mod fd;
mod fork;
mod locale;
pub mod post;
pub mod process;

pub use aside::{stand_aside, started_aside};
pub use fd::{
    BUFSIZ, Readiness, SockOpt, bind_fd, connect_fd, dup_fd, fd_table_size, inherit_fd, listen_fd,
    new_socket, peek_fd, poll_fd, select_fds, set_fd_blocking, set_sockopt, sync_fd,
};
pub use fork::{Forked, Pid, fork};
pub use locale::{is_space, reset_lc_ctype_locale};
