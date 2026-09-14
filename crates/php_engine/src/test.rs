//! The test face (design §4.3 seam 4, ADR-0002, ADR-0004): the `Capture`
//! sink and `Engine::serve_capture`, behind the `test` feature, so an
//! extension's tests build a fresh engine with that crate alone and read
//! what it wrote. Re-exported by `php_ext::test`; its items are `api.lock`
//! lines in the `test` group.

pub use crate::engine::Engine;
use crate::engine::{OutputSink, RequestDescription};

/// An in-process sink: everything the request wrote, kept, the way
/// php's CLI has it. `stderr` is the CLI's second stream whole: each
/// log entry with `\n` after it (`sapi_cli_log_message`) and what the
/// error stream was given, in the order the request wrote them, so an
/// extension's test reads it against the oracle's stderr; `log` is the
/// log's part of that alone, the same rendering (#216).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capture {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub headers: Vec<u8>,
    pub log: Vec<u8>,
}

impl OutputSink for Capture {
    fn write(&mut self, bytes: &[u8]) {
        self.stdout.extend_from_slice(bytes);
    }

    fn flush(&mut self) {}

    fn send_headers(&mut self, block: &[u8]) {
        self.headers.extend_from_slice(block);
    }

    fn log(&mut self, entry: &[u8]) {
        self.log.extend_from_slice(entry);
        self.log.push(b'\n');
        self.stderr.extend_from_slice(entry);
        self.stderr.push(b'\n');
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        self.stderr.extend_from_slice(bytes);
    }
}

/// What one captured request came to: the sink's bytes and the
/// observables.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Served {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub headers: Vec<u8>,
    pub log: Vec<u8>,
    pub exit_code: i32,
    pub fatal: Option<String>,
}

impl Engine {
    /// Serve `source` as a CLI request into `sink` and hand back what it
    /// wrote, the shape every extension's capture test reads.
    pub fn serve_capture(&self, source: &str, mut sink: Capture) -> Served {
        let description = RequestDescription {
            sapi_name: String::from("cli"),
            script_name: Some(b"Standard input code".to_vec()),
            source: Some(source.as_bytes().to_vec()),
            cwd: std::env::current_dir().unwrap_or_default(),
            ..RequestDescription::default()
        };
        let observables = self.serve(description, &mut sink);
        Served {
            stdout: sink.stdout,
            stderr: sink.stderr,
            headers: sink.headers,
            log: sink.log,
            exit_code: observables.exit_code,
            fatal: observables.fatal,
        }
    }
}
