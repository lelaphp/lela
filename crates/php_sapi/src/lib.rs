//! The hosts (design §4.1, §4.3 seam 4, §5.14): each builds a request
//! description from what it was given and supplies a sink, and the engine
//! crosses `serve` for every one of them. The CLI is [`cli`]; CGI, the
//! `FastCGI` worker and the dev server land beside it, each reading its
//! wire through `php_proto`. The engine's host face is re-exported here
//! so the binary names one crate for it: what this crate imports from
//! `php_engine` is `sapi.lock`, and `api` is never among it.
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

pub mod cli;

pub use php_engine::{Engine, EngineBuilder, Observables, OutputSink, RequestDescription};
