//! php's `main/streams/` and `main/network.c` (design §4.1, §4.3 seam 5):
//! the stream layer, its wrappers, transports and filters, and the user
//! wrapper, over `php_engine`'s api and above it, as php's `Zend/` reaches
//! streams only through `zend_stream_open_function`. What an extension
//! reaches is [`api`], which `php_ext` re-exports into its request group
//! and `api.lock` counts; the engine reaches the layer through the open
//! hook [`api::request::STREAM_HOOKS`] and through the request state the layer keeps.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

pub mod api;
mod state;
mod streams;
mod wrapper;
mod zend;
