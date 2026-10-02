//! The test face (design §4.3 seam 4, ADR-0002, ADR-0004): the `Capture`
//! sink and `Engine::serve_capture`, behind the `test` feature, so an
//! extension's tests build a fresh engine with that crate alone and read
//! what it wrote. Re-exported by `php_ext::test`; its items are `api.lock`
//! lines in the `test` group.

use std::path::Path;

pub use crate::engine::Engine;
use crate::engine::{FileHandles, OutputSink, RequestDescription, ResponseHead};

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
    fn write(&mut self, bytes: &[u8]) -> Result<(), crate::engine::Aborted> {
        self.stdout.extend_from_slice(bytes);
        Ok(())
    }

    fn flush(&mut self) {}

    /// The head's lines, each with `\r\n` after it.
    fn send_headers(&mut self, head: &ResponseHead) {
        for line in &head.lines {
            self.headers.extend_from_slice(line);
            self.headers.extend_from_slice(b"\r\n");
        }
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
/// observables, and under `witness-trace` the natives it entered, which
/// [`Engine::serve_capture_traced_in`] alone records.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Served {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub headers: Vec<u8>,
    pub log: Vec<u8>,
    pub exit_code: i32,
    pub fatal: Option<String>,
    #[cfg(feature = "witness-trace")]
    pub entered: Vec<Entered>,
}

/// One native a traced request entered, named as the macro expansion
/// that built its entry names it (harness §4.5 condition 2, revision
/// 58): the crate it is declared in, `CARGO_CRATE_NAME`; the Rust type
/// whose `#[php_class]` member impl declares it, `None` for a
/// `#[php_function]`; and its Rust identifier, `r#` stripped. These are
/// the names `xtask witness` reads off the source, so a subject and what
/// a fence entered meet without a translation between them.
#[cfg(feature = "witness-trace")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Entered {
    pub krate: &'static str,
    pub owner: Option<&'static str>,
    pub name: &'static str,
}

#[cfg(feature = "witness-trace")]
impl Entered {
    /// The line a traced engine appends for it to the file a host named
    /// ([`EngineBuilder::trace_natives_to`](crate::EngineBuilder::trace_natives_to)):
    /// the crate, the owning type or nothing, and the identifier, a tab
    /// between each and a newline after. The names are Rust identifiers,
    /// so neither character is in one.
    #[must_use]
    pub(crate) fn line(&self) -> String {
        format!(
            "{}\t{}\t{}\n",
            self.krate,
            self.owner.unwrap_or_default(),
            self.name
        )
    }
}

/// `trace` appended to `path` in one write, so a forked child's request
/// and its parent's land whole beside each other. A file that cannot be
/// written loses the lines, and the harness that reads it reports the
/// natives as not entered.
#[cfg(feature = "witness-trace")]
pub(crate) fn append_trace(path: &Path, trace: &[Entered]) {
    use std::io::Write;
    let lines: String = trace.iter().map(Entered::line).collect();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(lines.as_bytes());
    }
}

impl Engine {
    /// Serve `source` as a CLI request into `sink` and hand back what it
    /// wrote, the shape every extension's capture test reads. The request
    /// is named `Standard input code`, the CLI's name for a piped source
    /// (`php -r`, stdin); a test that pins `__FILE__` or the `in %s on
    /// line %d` of a diagnostic to another name serves through
    /// [`Engine::serve_capture_as`].
    pub fn serve_capture(&self, source: &str, sink: Capture) -> Served {
        self.serve_capture_as(b"Standard input code", source, sink)
    }

    /// [`Engine::serve_capture`] with the request named `name`: what
    /// `$_SERVER['SCRIPT_NAME']` and `$argv[0]` read, and, expanded
    /// against the working directory as php opens its primary file,
    /// what `__FILE__` and every diagnostic's `in %s` read (#1569). The
    /// witness lane's linked adapter serves
    /// each snippet under the recipe's `script_name` this way, as the
    /// fresh-process adapter runs it as that file (#293), which under the
    /// CLI is `argv` of that one word.
    pub fn serve_capture_as(&self, name: &[u8], source: &str, sink: Capture) -> Served {
        let cwd = std::env::current_dir().unwrap_or_default();
        self.serve_capture_in(&cwd, name, source, sink)
    }

    /// [`Engine::serve_capture_as`] with the request's working directory
    /// `dir` rather than the process's: what `__DIR__`, a relative
    /// `include` and `getcwd()` read. The witness lane's linked adapter
    /// serves each snippet in the scratch tree the file stands in, as the
    /// fresh-process adapter runs it there, so an `include` of the
    /// witness's own file reads the same file under both.
    pub fn serve_capture_in(&self, dir: &Path, name: &[u8], source: &str, sink: Capture) -> Served {
        self.serve_capture_with(dir, name, source, sink, false)
    }

    /// [`Engine::serve_capture_in`] with every native the request enters
    /// recorded in [`Served::entered`]: what the witness lane's linked
    /// adapter serves a fence through, so coverage is what the fence ran
    /// (harness §4.5 condition 2, revision 58).
    #[cfg(feature = "witness-trace")]
    pub fn serve_capture_traced_in(
        &self,
        dir: &Path,
        name: &[u8],
        source: &str,
        sink: Capture,
    ) -> Served {
        self.serve_capture_with(dir, name, source, sink, true)
    }

    fn serve_capture_with(
        &self,
        dir: &Path,
        name: &[u8],
        source: &str,
        mut sink: Capture,
        traced: bool,
    ) -> Served {
        // `sapi_cli_register_variables`' rows, as the CLI host describes
        // them: a piped source has no file behind it.
        let file: &[u8] = if name == b"Standard input code" {
            b""
        } else {
            name
        };
        let server = [
            (b"PHP_SELF".as_slice(), name),
            (b"SCRIPT_NAME", name),
            (b"SCRIPT_FILENAME", file),
            (b"PATH_TRANSLATED", file),
            (b"DOCUMENT_ROOT", b""),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_vec(), value.to_vec()))
        .collect();
        let description = RequestDescription {
            argv: vec![name.to_vec()],
            server,
            sapi_name: String::from("cli"),
            script_name: Some(name.to_vec()),
            source: Some(source.as_bytes().to_vec()),
            cwd: dir.to_path_buf(),
            file_handles: Some(piped()),
            process_title: crate::engine::ProcessTitle::Unsaved,
            ..RequestDescription::default()
        };
        // The CLI host's dump of the op arrays where
        // `opcache.opt_debug_level` asks for them, on the second stream
        // before the program runs (`php_sapi::cli`'s `dump`).
        if let Some(arrays) = self.dump_before_optimizer(&description) {
            sink.write_stderr(arrays.as_bytes());
        }
        #[cfg(feature = "witness-trace")]
        let (observables, entered) = if traced {
            self.serve_traced(description, &mut sink)
        } else {
            (self.serve(description, &mut sink), Vec::new())
        };
        #[cfg(not(feature = "witness-trace"))]
        let observables = {
            let _ = traced;
            self.serve(description, &mut sink)
        };
        Served {
            stdout: sink.stdout,
            stderr: sink.stderr,
            headers: sink.headers,
            log: sink.log,
            exit_code: observables.exit_code,
            fatal: observables.fatal,
            #[cfg(feature = "witness-trace")]
            entered,
        }
    }
}

/// The CLI's standard handles as a test runner hands php its fds, one
/// pipe's end under each, so `fstat` and `ftruncate` on `STDIN`, `STDOUT`
/// and `STDERR` answer what php answers under `run-tests.php` and the
/// witness lane's oracle, a pipe's `fstat(2)` and the `EINVAL` its
/// `ftruncate(2)` is. What the request writes is the sink's all the same;
/// the pipes carry nothing. A pipe the system refuses is no fd, which
/// answers as a closed one does.
fn piped() -> FileHandles {
    let stdin = std::io::pipe().ok().map(|(reader, _)| reader.into());
    let stdout = std::io::pipe().ok().map(|(_, writer)| writer.into());
    let stderr = std::io::pipe().ok().map(|(_, writer)| writer.into());
    FileHandles::new(stdin, stdout, stderr)
}

/// A request on a fresh engine, for a unit test of the face that needs
/// one to allocate through: the CLI description and nothing served.
#[cfg(test)]
pub(crate) fn request() -> crate::api::request::Request {
    let engine = Engine::builder().build();
    crate::api::request::Request::new(
        std::sync::Arc::clone(&engine.shared),
        RequestDescription::default(),
    )
}
