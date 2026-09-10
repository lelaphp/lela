//! The engine (design §4.1, §5): the one loop over `php_unit`, the native
//! face [`api`], the tier seam [`tier`], the linker, and `serve`. Three
//! faces, each locked: the host's (`Engine`, `serve`, `sapi.lock`), the
//! extension's (`api`, measured on `php_ext` as `api.lock`) and the
//! tier's (`tier.lock`). This skeleton lays every face down as compilable
//! signatures over bodies that answer trivially or end the request in an
//! internal-error fatal; the kernels and the loop are Lane B and C of
//! design §11.
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
mod engine;
#[cfg(feature = "test")]
pub mod test;
pub mod tier;

pub use engine::{Engine, EngineBuilder, Observables, OutputSink, RequestDescription};
