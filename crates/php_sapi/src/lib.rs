//! The hosts (design §4.1, §4.3 seam 4, §5.14): each builds a request
//! description from what it was given and supplies a sink, and the engine
//! crosses `serve` for every one of them. The CLI is [`cli`] and CGI
//! [`cgi`]; the `FastCGI` worker and the dev server land beside them, each
//! reading its wire through `php_proto`. Which one a binary is, is chosen
//! once, by the name it was started under ([`Host::of`]). The engine's host
//! face is re-exported here so the binary names one crate for it: what
//! this crate imports from `php_engine` is `sapi.lock`, and `api` is never
//! among it.
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

pub mod cgi;
pub mod cli;
mod getopt;

pub use php_engine::{
    Engine, EngineBuilder, FileHandles, Method, Observables, OutputSink, RequestDescription,
    ResponseHead, WebRequest,
};

/// Which host a binary is, chosen once, by the name it was started under, as
/// php builds its CLI and its CGI as `php` and `php-cgi`: a name opening
/// with `php-cgi` is CGI, and any other the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Cli,
    Cgi,
}

impl Host {
    /// The host `argv[0]` names.
    #[must_use]
    pub fn of(argv0: &std::ffi::OsStr) -> Host {
        let path = std::path::Path::new(argv0);
        let named = path
            .file_name()
            .map(std::ffi::OsStr::as_encoded_bytes)
            .unwrap_or_default();
        if named.starts_with(b"php-cgi") {
            Host::Cgi
        } else {
            Host::Cli
        }
    }
}
