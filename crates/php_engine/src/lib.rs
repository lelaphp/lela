//! The engine (design §4.1, §5): the one loop over `php_unit`, the native
//! face [`api`], the tier seam [`tier`], the linker, and `serve`. Three
//! faces, each locked: the host's (`Engine`, `serve`, `sapi.lock`), the
//! extension's (`api`, measured on `php_ext` as `api.lock`) and the
//! tier's (`tier.lock`). The loop over `php_unit::Opcode` is [`tier`], its
//! dispatch generated from `ops.toml` in `build.rs`, its handlers in
//! `handlers` and its semantics in `ops`; the rest of the kernels and the
//! natives are Lane B and C of design §11. The interrupt the loop tests
//! at its safepoints, and the execution timer that sets it, are
//! [`interrupt`] (design §5.8).
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
mod classes;
mod engine;
mod exec;
mod handlers;
mod interrupt;
mod natives;
mod ops;
mod output;
#[cfg(test)]
mod probe;
mod streams;
// The test face is on for an extension's dev-dependency, and for this
// crate's own tests, which read a served request the same way.
#[cfg(any(feature = "test", test))]
pub mod test;
pub mod tier;

pub use api::request::PHP_VERSION;
pub use engine::{Engine, EngineBuilder, Observables, OutputSink, RequestDescription};
