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
    fn send_headers(&mut self, head: &ResponseHead) -> Result<(), crate::engine::Aborted> {
        for line in &head.lines {
            self.headers.extend_from_slice(line);
            self.headers.extend_from_slice(b"\r\n");
        }
        Ok(())
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

/// A sink whose bytes are the process's own, as php's CLI writes them:
/// each body write straight to the process's fd 1 and each
/// error-stream write and log entry to its fd 2, unbuffered
/// (`sapi_cli_single_write`), so what a request wrote stands written
/// whatever ends the process after it.
#[derive(Debug, Default)]
struct Through;

impl OutputSink for Through {
    fn write(&mut self, bytes: &[u8]) -> Result<(), crate::engine::Aborted> {
        use std::io::Write as _;
        let mut stdout = std::io::stdout().lock();
        // php's CLI writes what it can and drops the rest; a closed
        // stream is nothing a request is told of here either.
        stdout.write_all(bytes).and_then(|()| stdout.flush()).ok();
        Ok(())
    }

    fn flush(&mut self) {
        use std::io::Write as _;
        std::io::stdout().lock().flush().ok();
    }

    /// The CLI sends no head.
    fn send_headers(&mut self, _: &ResponseHead) -> Result<(), crate::engine::Aborted> {
        Ok(())
    }

    fn log(&mut self, entry: &[u8]) {
        use std::io::Write as _;
        let mut stderr = std::io::stderr().lock();
        stderr
            .write_all(entry)
            .and_then(|()| stderr.write_all(b"\n"))
            .ok();
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        use std::io::Write as _;
        std::io::stderr().lock().write_all(bytes).ok();
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

/// A tree of files for one test, at `<temp>/dax-<pid>-<name>`, under the
/// system's temporary directory and named for the process and the test:
/// laid down fresh by [`Files::new`], each step asserted, so a tree that
/// could not be written stops the test where it failed rather than as
/// a request that reads nothing, and removed when it is dropped.
#[derive(Debug)]
pub struct Files {
    dir: std::path::PathBuf,
}

impl Files {
    /// The tree `name` holding each `(file, text)` of `files`, a file's
    /// directories made as its path names them, over whatever an earlier
    /// process of the same id left there.
    ///
    /// # Panics
    ///
    /// Where the tree cannot be removed, made or written, the io error
    /// in the message.
    #[must_use]
    pub fn new(name: &str, files: &[(&str, &str)]) -> Files {
        let dir = std::env::temp_dir().join(format!("dax-{}-{name}", std::process::id()));
        let removed = std::fs::remove_dir_all(&dir);
        assert!(
            !matches!(&removed, Err(error) if error.kind() != std::io::ErrorKind::NotFound),
            "what stands at {} is removed first: {removed:?}",
            dir.display()
        );
        let made = std::fs::create_dir_all(&dir);
        assert!(made.is_ok(), "{} is made: {made:?}", dir.display());
        let tree = Files { dir };
        for (file, text) in files {
            tree.write(file, text);
        }
        tree
    }

    /// The tree's directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// `contents` written to `file` in the tree, a file there replaced,
    /// and the directories its path names made.
    ///
    /// # Panics
    ///
    /// Where the file or its directories cannot be written, the io error
    /// in the message.
    pub fn write(&self, file: &str, contents: impl AsRef<[u8]>) {
        let path = self.dir.join(file);
        if let Some(parent) = path.parent() {
            let made = std::fs::create_dir_all(parent);
            assert!(made.is_ok(), "{} is made: {made:?}", parent.display());
        }
        let written = std::fs::write(&path, contents);
        assert!(
            written.is_ok(),
            "{} is written: {written:?}",
            path.display()
        );
    }

    /// `php <dir>/<file>` in the tree's directory, the path as
    /// `run-tests.php` hands it: the file compiled as a file is and served as
    /// [`Engine::serve_capture`] serves a source, what the request wrote
    /// and how it ended.
    pub fn serve(&self, engine: &Engine, file: &str) -> Served {
        let path = self.dir.join(file);
        engine.serve_capture_with(
            &self.dir,
            path.as_os_str().as_encoded_bytes(),
            None,
            Vec::new(),
            Capture::default(),
            false,
        )
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        // A tree something else removed first is gone all the same.
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

impl Served {
    /// What the request wrote to its body, as text, a byte that is no
    /// UTF-8 replaced.
    #[must_use]
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// The request wrote `expected`, ended with status 0 and raised no
    /// fatal, held in one comparison whose failure prints both streams
    /// as text.
    ///
    /// # Panics
    ///
    /// Where any of the three differs, with stdout and stderr whole.
    #[track_caller]
    pub fn assert_clean(&self, expected: &str) {
        let stdout = self.stdout_text();
        assert_eq!(
            (stdout.as_str(), self.exit_code, self.fatal.as_deref()),
            (expected, 0, None),
            "\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&self.stderr)
        );
    }
}

impl Engine {
    /// Serve `source` as a CLI request into `sink` and hand back what it
    /// wrote, the shape every extension's capture test reads. The request
    /// is named `Standard input code`, the CLI's name for a piped source
    /// (`php -r`, stdin); a test that pins `__FILE__` or the `in %s on
    /// line %d` of a diagnostic to another name serves through
    /// [`Engine::serve_capture_in`].
    pub fn serve_capture(&self, source: &str, sink: Capture) -> Served {
        let cwd = std::env::current_dir().unwrap_or_default();
        self.serve_capture_in(&cwd, b"Standard input code", source, sink)
    }

    /// Serve `source` as [`Engine::serve_capture`] does, with what it
    /// writes the process's own fd 1 and fd 2 as it is
    /// written, unbuffered as php's CLI writes them, rather than kept:
    /// for a test that reads the request from outside the process, one
    /// whose request ends the process among them (`zend_test_crash`'s
    /// signal). The [`Served`] it hands back holds the observables and no
    /// bytes.
    pub fn serve_through(&self, source: &str) -> Served {
        let cwd = std::env::current_dir().unwrap_or_default();
        let (description, dumped) = self.capture_description(
            &cwd,
            b"Standard input code",
            Some(source),
            Vec::new(),
            Capture::default(),
        );
        let mut through = Through;
        through.write_stderr(&dumped.stderr);
        let (observables, _) = self.serve(description, Box::new(through), |_| HostServes::Program);
        served(observables, Vec::new(), Box::new(Capture::default()))
    }

    /// [`Engine::serve_capture`] with the request named `name` and its
    /// working directory `dir`. The name is what
    /// `$_SERVER['SCRIPT_NAME']` and `$argv[0]` read, and, expanded
    /// against `dir` as php opens its primary file, what `__FILE__` and
    /// every diagnostic's `in %s` read (#1569), which under the CLI is
    /// `argv` of that one word; the directory is what `__DIR__`, a
    /// relative `include` and `getcwd()` read.
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
        fatal: match observables.ending {
            Some(crate::api::request::Ending::Fatal(logged)) => Some(logged),
            _ => None,
        },
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
pub(crate) fn request() -> Box<crate::api::request::Request> {
    request_on(&Engine::builder().build())
}

/// A request on `engine`'s shared state under the CLI's name, nothing
/// served: for a unit test that builds the engine with the ini it holds
/// the request to, and hands the request a state no snippet reaches, a
/// level no raiser raises or a throwable pending where php's release
/// would be.
#[cfg(test)]
pub(crate) fn request_on(engine: &Engine) -> Box<crate::api::request::Request> {
    crate::api::request::Request::new(
        std::sync::Arc::clone(&engine.shared),
        RequestDescription {
            sapi_name: String::from("cli"),
            ..RequestDescription::default()
        },
    )
}

/// What `memory_get_usage()` reads of a request before it serves a line
/// (design §5.4, §5.6, §5.13), the three parts `php_heap` lays down
/// when the request's heap is made: `CG(arena)`'s first block,
/// `init_compiler`'s `zend_arena_create(64 * 1024)`
/// (`php_heap::zend_arena::FIRST_BLOCK`); `EG(objects_store)`, the 1024
/// buckets of a `zend_object *` each that `init_executor` gives
/// `zend_objects_store_init` (`php_heap::object::INIT_SIZE`); and the VM
/// stack's first page, `ZEND_VM_STACK_PAGE_SLOTS`, sixteen thousand
/// zvals (`php_heap::stack::PAGE_SIZE`). `php_heap` keeps the three
/// figures to itself, so they are restated here once, over its own
/// [`php_heap::Zval`].
#[cfg(test)]
pub(crate) const STARTED: usize =
    64 * 1024 + 1024 * size_of::<usize>() + 16 * 1024 * size_of::<php_heap::Zval>();

#[cfg(test)]
mod tests {
    use super::{Engine, Files, Served};

    /// A request that missed its expectation prints what it wrote and
    /// what it logged as text, the bytes of neither as an array.
    #[test]
    #[should_panic(expected = "stdout:\ngot\n\nstderr:\nPHP Warning:  logged\n")]
    fn a_failing_assert_clean_prints_both_streams_as_text() {
        let served = Served {
            stdout: b"got\n".to_vec(),
            stderr: b"PHP Warning:  logged\n".to_vec(),
            ..Served::default()
        };
        served.assert_clean("wanted\n");
    }

    /// A clean request meets `assert_clean`; one that ended with another
    /// status does not, however it wrote what was expected.
    #[test]
    fn assert_clean_holds_the_status_and_the_fatal_with_the_body() {
        let served = Served {
            stdout: b"ok\n".to_vec(),
            ..Served::default()
        };
        served.assert_clean("ok\n");
        let exited = Served {
            exit_code: 1,
            ..served
        };
        let missed = std::panic::catch_unwind(|| exited.assert_clean("ok\n"));
        assert!(missed.is_err(), "status 1 is not clean");
    }

    /// A tree is laid down with its directories, served from, written
    /// again between requests, and gone once dropped.
    #[test]
    fn a_tree_is_served_from_rewritten_and_removed_on_drop() {
        let files = Files::new(
            "test-files",
            &[
                ("main.php", "<?php include 'sub/inc.php';"),
                ("sub/inc.php", "<?php echo \"one\\n\";"),
            ],
        );
        let engine = Engine::cli_builder().build();
        files.serve(&engine, "main.php").assert_clean("one\n");
        files.write("sub/inc.php", "<?php echo \"two\\n\";");
        files.serve(&engine, "main.php").assert_clean("two\n");
        let dir = files.path().to_path_buf();
        drop(files);
        assert!(!dir.exists(), "{} is removed", dir.display());
    }
}
