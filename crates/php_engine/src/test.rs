//! The test face (design §4.3 seam 4, ADR-0002, ADR-0004): the `Capture`
//! sink and `Engine::serve_capture`, behind the `test` feature, so an
//! extension's tests build a fresh engine with that crate alone and read
//! what it wrote. Re-exported by `php_ext::test`; its items are `api.lock`
//! lines in the `test` group.

use std::any::Any;
use std::path::Path;

pub use crate::engine::Engine;
use crate::engine::{
    FileHandles, HostServes, OutputSink, ProgramFile, RequestDescription, ResponseHead,
};

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
/// observables, and the natives it entered, which
/// [`Engine::serve_capture_traced_in`] alone records.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Served {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub headers: Vec<u8>,
    pub log: Vec<u8>,
    pub exit_code: i32,
    pub fatal: Option<String>,
    pub entered: Vec<Entered>,
}

pub use crate::trace::Entered;

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
    /// what `__FILE__` and every diagnostic's `in %s` read (#1569), which
    /// under the CLI is `argv` of that one word.
    pub fn serve_capture_as(&self, name: &[u8], source: &str, sink: Capture) -> Served {
        let cwd = std::env::current_dir().unwrap_or_default();
        self.serve_capture_in(&cwd, name, source, sink)
    }

    /// [`Engine::serve_capture_as`] with the request's working directory
    /// `dir` rather than the process's: what `__DIR__`, a relative
    /// `include` and `getcwd()` read.
    pub fn serve_capture_in(&self, dir: &Path, name: &[u8], source: &str, sink: Capture) -> Served {
        self.serve_capture_with(dir, name, Some(source), Vec::new(), sink, false)
    }

    /// The file `name` in `dir`, which the caller has written, served as
    /// php's CLI serves `php <name>` in `dir`: compiled as a file,
    /// through the unit cache, and held open while it is served
    /// ([`crate::engine::ProgramFile::Opened`]), so a name the program
    /// holds is in the table php compiles a file's names into, where
    /// `zend_string_init_existing_interned` finds it (#2985). It is served
    /// under the environment `env`, what `getenv()`, `$_ENV` and
    /// `$_SERVER` read, with every native the request enters recorded in
    /// [`Served::entered`]: what the witness lane's linked adapter serves
    /// a fence through, the file standing in the scratch tree the oracle
    /// serves it in, under the recipe's environment as the oracle has it
    /// (#1077), so coverage is what the fence reached (harness §4.5 condition
    /// 2, revision 58). The others serve a source handed over as a piped
    /// one is, under no environment, so what a test reads is the same on
    /// any shell.
    pub fn serve_capture_traced_in(
        &self,
        dir: &Path,
        name: &[u8],
        env: Vec<(Vec<u8>, Vec<u8>)>,
        sink: Capture,
    ) -> Served {
        self.serve_capture_with(dir, name, None, env, sink, true)
    }

    fn serve_capture_with(
        &self,
        dir: &Path,
        name: &[u8],
        source: Option<&str>,
        env: Vec<(Vec<u8>, Vec<u8>)>,
        sink: Capture,
        traced: bool,
    ) -> Served {
        let (description, sink) = self.capture_description(dir, name, source, env, sink);
        let sink = Box::new(sink);
        let (observables, entered, sink) = if traced {
            self.serve_traced(description, sink)
        } else {
            let (observables, sink) = self.serve(description, sink, |_| HostServes::Program);
            (observables, Vec::new(), sink)
        };
        served(observables, entered, sink)
    }

    /// [`Engine::serve_capture_in`], and the request state of type `T` as
    /// the request left it once it ended, where the request built one:
    /// what a test tier declared through `Tier::extension()` read back by
    /// the test that served the program (`tier::serve_kept`).
    pub(crate) fn serve_capture_keeping<T: Send + 'static>(
        &self,
        dir: &Path,
        name: &[u8],
        source: &str,
    ) -> (Served, Option<T>) {
        let (description, sink) =
            self.capture_description(dir, name, Some(source), Vec::new(), Capture::default());
        let program = description.source.clone();
        let mut req = match self.begin(description, Box::new(sink), false) {
            Ok(req) => req,
            Err((refused, sink)) => return (served(refused, Vec::new(), sink), None),
        };
        req.keep_states();
        self.serve_in(&mut req, program.as_deref(), HostServes::Program);
        let (observables, entered, sink) = self.shutdown(&mut req);
        let kept = req.take_kept_state::<T>();
        (served(observables, entered, sink), kept)
    }

    /// The CLI's description of a request serving `source` as `name` in
    /// `dir`, as a piped source is handed over, or where there is none the
    /// file `name` as the CLI opens it, and `sink` with the op arrays
    /// written to it first where `opcache.opt_debug_level` asks for them.
    fn capture_description(
        &self,
        dir: &Path,
        name: &[u8],
        source: Option<&str>,
        env: Vec<(Vec<u8>, Vec<u8>)>,
        mut sink: Capture,
    ) -> (RequestDescription, Capture) {
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
            env,
            server,
            sapi_name: String::from("cli"),
            script_name: Some(name.to_vec()),
            source: source.map(|source| source.as_bytes().to_vec()),
            cwd: dir.to_path_buf(),
            file_handles: Some(piped()),
            program_file: match source {
                Some(_) => ProgramFile::default(),
                None => ProgramFile::Opened,
            },
            process_title: crate::engine::ProcessTitle::Unsaved,
            ..RequestDescription::default()
        };
        // The CLI host's dump of the op arrays where
        // `opcache.opt_debug_level` asks for them, on the second stream
        // before the program runs (`php_sapi::cli`'s `dump`).
        if let Some(arrays) = self.dump_before_optimizer(&description) {
            sink.write_stderr(&arrays);
        }
        (description, sink)
    }
}

/// What a served request handed back, as a test reads it.
fn served(
    observables: crate::engine::Observables,
    entered: Vec<Entered>,
    sink: Box<dyn OutputSink + Send>,
) -> Served {
    let sink: Capture = handed_back(sink).unwrap_or_default();
    Served {
        stdout: sink.stdout,
        stderr: sink.stderr,
        headers: sink.headers,
        log: sink.log,
        exit_code: observables.exit_code,
        fatal: observables.fatal,
        entered,
    }
}

impl Engine {
    /// [`Engine::serve`] of the program with `sink` lent and taken back
    /// into place, for a test that reads its own sink after.
    #[cfg(test)]
    pub(crate) fn serve_into<S: OutputSink + Send + Default>(
        &self,
        description: RequestDescription,
        sink: &mut S,
    ) -> crate::engine::Observables {
        let lent = Box::new(std::mem::take(sink));
        let (observed, back) = self.serve(description, lent, |_| HostServes::Program);
        if let Some(back) = handed_back(back) {
            *sink = back;
        }
        observed
    }
}

/// The sink a request handed back, as the type it was lent as; `None`
/// where it is another.
pub(crate) fn handed_back<S: OutputSink>(sink: Box<dyn OutputSink + Send>) -> Option<S> {
    let sink: Box<dyn Any> = sink;
    sink.downcast().ok().map(|sink| *sink)
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
/// one to allocate through: [`request_on`] a default engine.
#[cfg(test)]
pub(crate) fn request() -> crate::api::request::Request {
    request_on(&Engine::builder().build())
}

/// A request on `engine`'s shared state under the CLI's name, nothing
/// served: for a unit test that builds the engine with the ini it holds
/// the request to, and hands the request a state no snippet reaches, a
/// level no raiser raises or a throwable pending where php's release
/// would be.
#[cfg(test)]
pub(crate) fn request_on(engine: &Engine) -> crate::api::request::Request {
    crate::api::request::Request::new(
        std::sync::Arc::clone(&engine.shared),
        RequestDescription {
            sapi_name: String::from("cli"),
            ..RequestDescription::default()
        },
    )
}
