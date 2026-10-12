//! The hosts' facade (ADR-0008), stood in for here: the items the template
//! names, in the shapes it calls them, and nothing behind them, so `clippy`
//! checks the laid-down crate without building an engine (#2524). Each
//! public item is one of the real facade's, as `tests/new_sapi.rs` holds
//! (#2544), in its shape: a field one of the real item's `pub` fields, a
//! function's parameters as many as the real one's and each written as
//! the real one writes it where it names no generic (#2545).
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

/// The engine, which serves nothing here.
pub struct Engine;

/// The engine before it starts.
pub struct EngineBuilder;

/// What a request ends with.
pub struct Observables {
    /// The code the request ended with.
    pub exit_code: i32,
}

/// What a sink answers when the client went away.
pub struct Aborted;

/// The head a sink is handed before the body.
pub struct ResponseHead;

/// What a host gives the engine for one request.
#[derive(Default)]
pub struct RequestDescription {
    /// The request's argv.
    pub argv: Vec<Vec<u8>>,
    /// The request's environment, which the template leaves to its default.
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    /// The request's working directory.
    pub cwd: PathBuf,
    /// What `php_sapi_name()` answers.
    pub sapi_name: String,
    /// The script the request runs.
    pub script_name: Option<Vec<u8>>,
}

/// What a host serves in a request it has started.
pub enum HostServes {
    /// The program the description names.
    Program,
}

/// Where what the request writes goes.
pub trait OutputSink {
    /// The body.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted>;
    /// The body's flush.
    fn flush(&mut self);
    /// The head.
    fn send_headers(&mut self, head: &ResponseHead);
    /// One log entry.
    fn log(&mut self, entry: &[u8]);
    /// The error stream.
    fn write_stderr(&mut self, bytes: &[u8]);
}

impl Engine {
    /// The builder.
    pub fn builder() -> EngineBuilder {
        EngineBuilder
    }

    /// One request served, here none, and the sink handed back.
    pub fn serve(
        &self,
        _description: RequestDescription,
        sink: Box<dyn OutputSink + Send>,
        _look: impl FnOnce() -> HostServes,
    ) -> (Observables, Box<dyn OutputSink + Send>) {
        (Observables { exit_code: 0 }, sink)
    }
}

impl EngineBuilder {
    /// The host's name.
    #[must_use]
    pub fn sapi_name(self, _name: &'static str) -> Self {
        self
    }

    /// The enabled set.
    #[must_use]
    pub fn extensions<T>(self, _extensions: &[T]) -> Self {
        self
    }

    /// The `configure` switches.
    #[must_use]
    pub fn configure(self, _switches: &'static [&'static str]) -> Self {
        self
    }

    /// The compiled tier.
    #[must_use]
    pub fn tier<T>(self) -> Self {
        self
    }
}

/// The engine started.
pub fn start(_builder: EngineBuilder, _ini: &[(Vec<u8>, Vec<u8>)], _log: fn(&[u8])) -> Engine {
    Engine
}

/// The process's exit code.
pub fn exit_code(status: i32) -> ExitCode {
    ExitCode::from(u8::try_from(status).unwrap_or(255))
}
