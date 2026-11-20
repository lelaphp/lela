//! The requests `do_cli` serves (`Z/sapi/cli/php_cli.c:840-1180`): the
//! request description `do_repeat` builds for a program, the sink on the
//! process's two streams, and each `behavior` served on it, a program, `-r`'s
//! code and stdin's lines, a lint, a highlight or a strip, and the
//! reports `do_cli` prints in a request of their own.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;

use php_sapi::{
    Aborted, CommandLine, EachLine, Engine, FileHandles, HostServes, Observables, OutputSink,
    ProgramFile, RequestDescription, ResponseHead,
};

use crate::NAME;
use crate::options::{Each, Lines, Listing, Program};

/// The sink on the process's streams, php's CLI: body bytes on stdout,
/// the header block nowhere, since `sapi_cli_send_headers` sends none,
/// and both the log and the error stream on stderr, each log entry with
/// `\n` after it (`sapi_cli_log_message`). The request owns it and each
/// call reaches the fd as it is made, as `sapi_cli_ub_write` writes each
/// chunk to fd 1, so what a child process the request starts writes to
/// the stdout it inherits stands where php's would. A stream that
/// refuses a write is left refusing: php's `sapi_cli_single_write` reads
/// no error either.
#[derive(Debug, Default)]
pub struct Process;

impl OutputSink for Process {
    /// `sapi_cli_ub_write` (`Z/sapi/cli/php_cli.c:263`): each chunk
    /// `write(2)` to fd 1 until all of it went; one the fd refuses, a
    /// closed fd or a pipe whose reader left, sets `EG(exit_status)` to
    /// 255 and is `php_handle_aborted_connection`.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
        std::io::stdout().lock().flush().ok();
        crate::ffi::write_stdout(bytes).map_err(|_| Aborted {
            exit_status: Some(255),
        })
    }

    fn flush(&mut self) {
        std::io::stdout().lock().flush().ok();
    }

    fn send_headers(&mut self, _head: &ResponseHead) -> Result<(), Aborted> {
        Ok(())
    }

    fn log(&mut self, entry: &[u8]) {
        let mut stderr = std::io::stderr().lock();
        stderr.write_all(entry).ok();
        stderr.write_all(b"\n").ok();
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        std::io::stderr().lock().write_all(bytes).ok();
    }
}

/// `php_self` for a request with no file.
const STANDARD_INPUT: &[u8] = b"Standard input code";

/// The request description `do_repeat` builds (`php_cli.c:878-916`):
/// `argv` with `php_self` first, which is the file's name or `Standard
/// input code`; the process's environment and its working directory; the
/// file named on the line, which the engine reads by name and holds open
/// while it runs, as `cli_seek_file_begin`'s `FILE *` is held
/// ([`ProgramFile::Opened`]), or stdin read whole and served as `Standard input code` where `stdin` asks it; and
/// the process's standard handles ([`FileHandles`]), which the engine
/// registers as `STDIN`, `STDOUT` and `STDERR` over copies of fds 0, 1
/// and 2. A file that cannot be opened is `cli_seek_file_begin`'s line on
/// stderr and exit 1, before a request begins.
pub fn describe(
    file: Option<&Path>,
    arguments: &[OsString],
    stdin: bool,
) -> Result<RequestDescription, i32> {
    let (php_self, script_filename, source) = match file {
        Some(path) => {
            if std::fs::File::open(path).is_err() {
                let mut stderr = std::io::stderr().lock();
                stderr.write_all(b"Could not open input file: ").ok();
                stderr.write_all(path.as_os_str().as_encoded_bytes()).ok();
                stderr.write_all(b"\n").ok();
                return Err(1);
            }
            let name = path.as_os_str().as_encoded_bytes().to_vec();
            (name.clone(), name, None)
        }
        None if stdin => {
            let mut source = Vec::new();
            std::io::stdin().lock().read_to_end(&mut source).ok();
            (STANDARD_INPUT.to_vec(), Vec::new(), Some(source))
        }
        None => (STANDARD_INPUT.to_vec(), Vec::new(), None),
    };
    let server = server_variables(&php_self, &script_filename);
    let script_name = (file.is_some() || stdin).then(|| php_self.clone());
    let mut argv = vec![php_self];
    argv.extend(arguments.iter().map(|a| a.as_encoded_bytes().to_vec()));
    Ok(RequestDescription {
        argv,
        env: std::env::vars_os()
            .map(|(n, v)| (n.as_encoded_bytes().to_vec(), v.as_encoded_bytes().to_vec()))
            .collect(),
        cwd: std::env::current_dir().unwrap_or_default(),
        server,
        sapi_name: String::from(NAME),
        script_name,
        source,
        file_handles: Some(process_handles()),
        process_title: crate::title_constant(),
        program_file: ProgramFile::Opened,
        ..RequestDescription::default()
    })
}

/// `sapi_cli_register_variables` (`Z/sapi/cli/php_cli.c:316`) past the
/// environment, which the engine imports first for every host:
/// `PHP_SELF` and `SCRIPT_NAME` are `php_self`, the file's name or
/// `Standard input code`; `SCRIPT_FILENAME` and `PATH_TRANSLATED` are the
/// file's name as given, empty for `-r` and stdin; `DOCUMENT_ROOT` is
/// empty.
fn server_variables(php_self: &[u8], file: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![
        (b"PHP_SELF".to_vec(), php_self.to_vec()),
        (b"SCRIPT_NAME".to_vec(), php_self.to_vec()),
        (b"SCRIPT_FILENAME".to_vec(), file.to_vec()),
        (b"PATH_TRANSLATED".to_vec(), file.to_vec()),
        (b"DOCUMENT_ROOT".to_vec(), Vec::new()),
    ]
}

/// The process's own fds 0, 1 and 2 as the engine is handed them, so the
/// request's `fstat` and `ftruncate` ask of the fds php's `STDIO` kind
/// asks of and its `fclose()` of a constant closes the fd itself; a
/// closed one is `None`, which both answer as `fstat(2)` answers `EBADF`.
fn process_handles() -> FileHandles {
    let [stdin, stdout, stderr] = crate::ffi::standard_fds();
    FileHandles::process(stdin, stdout, stderr)
}

/// The file a program names, where it names one.
fn file_of(program: &Program) -> Option<&Path> {
    match program {
        Program::File(path) => Some(path),
        Program::Direct(_) | Program::Stdin => None,
    }
}

/// `PHP_CLI_MODE_STANDARD` and `PHP_CLI_MODE_CLI_DIRECT`
/// (`php_cli.c:945-992`): a file or stdin served as
/// `php_execute_script` serves it, or `-r`'s code as
/// `zend_eval_string_ex` does; the exit code.
///
/// A file named relative to the working directory is served under its
/// expanded path, so `__FILE__` is whole and the page `getmyinode()` and
/// `getlastmod()` stat is found after a `chdir()`; `$argv[0]` keeps the
/// name as given (a process fence runs the program as `php witness.php`
/// in its own directory):
///
/// ```php
/// chdir('..');
/// var_dump(__FILE__ === getcwd() . '/' . basename(__DIR__) . '/witness.php', $argv[0], getmyinode() !== false, getlastmod() !== false);
/// ```
/// ```expect process
/// bool(true)
/// string(11) "witness.php"
/// bool(true)
/// bool(true)
/// ```
pub fn serve(engine: &Engine, program: &Program, arguments: &[OsString]) -> i32 {
    let description = match describe(file_of(program), arguments, *program == Program::Stdin) {
        Ok(description) => description,
        Err(code) => return code,
    };
    let mut sink: Box<dyn OutputSink + Send> = Box::new(Process);
    let (served, mut sink) = match program {
        Program::Direct(code) => {
            engine.command_line(description, CommandLine::Direct(code.clone()), sink)
        }
        Program::File(_) | Program::Stdin => {
            dump(engine, &description, sink.as_mut());
            engine.serve(description, sink, |_| HostServes::Program)
        }
    };
    ended(&served, sink.as_mut())
}

/// The status `do_cli` ends a request with: where `php_request_startup`
/// failed, an extension's `rinit` having raised, `Could not
/// startup.` written after what the failure rendered, and 1, which `err`
/// sets (`Z/sapi/cli/php_cli.c:919-922`, `:1188-1191`); otherwise the
/// request's own.
fn ended(served: &Observables, sink: &mut dyn OutputSink) -> i32 {
    php_sapi::abort_as_php(served);
    if served.start_failed {
        sink.write(b"Could not startup.\n").ok();
        return 1;
    }
    served.exit_code
}

/// `PHP_CLI_MODE_STANDARD` with `interactive` (`php_cli.c:945-950`):
/// the file a bare word or `-f` named opened first and not served, as
/// `do_repeat` opens it, then `cli_shell_callbacks.cli_shell_run()` in
/// the request ([`CommandLine::Shell`]); the exit code.
pub fn shell(engine: &Engine, file: Option<&Path>, arguments: &[OsString]) -> i32 {
    let description = match describe(file, arguments, false) {
        Ok(description) => description,
        Err(code) => return code,
    };
    let (served, mut sink) =
        engine.command_line(description, CommandLine::Shell, Box::new(Process));
    ended(&served, sink.as_mut())
}

/// `PHP_CLI_MODE_PROCESS_STDIN` (`php_cli.c:996-1034`): `-B`, `-R` or
/// `-F`, and `-E` over stdin's lines, the file `-F` named opened first,
/// as `do_repeat` opens it; the exit code.
pub fn process_stdin(engine: &Engine, lines: &Lines, arguments: &[OsString]) -> i32 {
    let file = match &lines.each {
        Some(Each::File(path)) => Some(path.as_path()),
        Some(Each::Code(_)) | None => None,
    };
    let description = match describe(file, arguments, false) {
        Ok(description) => description,
        Err(code) => return code,
    };
    let command = CommandLine::Lines {
        begin: lines.begin.clone(),
        each: lines.each.as_ref().map(|each| match each {
            Each::Code(code) => EachLine::Code(code.clone()),
            Each::File(_) => EachLine::File,
        }),
        end: lines.end.clone(),
    };
    let (served, mut sink) = engine.command_line(description, command, Box::new(Process));
    ended(&served, sink.as_mut())
}

/// php's op arrays as the compiler left them, on the second stream,
/// where `opcache.opt_debug_level` asks for them
/// ([`Engine::dump_before_optimizer`], design §6.1): opcache dumps them
/// as it persists the unit, which is before the program runs and
/// before `-l`'s own sentence, and this host asks for them in the one
/// moment php has both. `-r`'s code is the program php dumps nothing
/// for, its op array being `ZEND_EVAL_CODE`
/// (`Z/Zend/Optimizer/zend_optimizer.c:1056`), and it never comes here; a
/// file and what the host read from stdin are dumped alike.
fn dump(engine: &Engine, description: &RequestDescription, sink: &mut dyn OutputSink) {
    if let Some(arrays) = engine.dump_before_optimizer(description) {
        sink.write_stderr(&arrays);
    }
}

/// `-s` over one program, `do_cli`'s `PHP_CLI_MODE_HIGHLIGHT` arm
/// (`php_cli.c:969-977`): the program as [`Engine::highlight`] writes it,
/// and the exit code; a file that cannot be opened is exit 1 before a
/// request begins. The arm registers no standard handles.
pub fn highlight(engine: &Engine, program: &Program, arguments: &[OsString]) -> i32 {
    view(program, arguments, |description, sink| {
        engine.highlight(description, sink)
    })
}

/// `-w` over one program, `do_cli`'s `PHP_CLI_MODE_STRIP` arm
/// (`php_cli.c:963-968`), as [`highlight`] with [`Engine::strip`].
pub fn strip(engine: &Engine, program: &Program, arguments: &[OsString]) -> i32 {
    view(program, arguments, |description, sink| {
        engine.strip(description, sink)
    })
}

/// The request `-s` and `-w` describe, handed to what writes it.
fn view(
    program: &Program,
    arguments: &[OsString],
    write: impl FnOnce(
        RequestDescription,
        Box<dyn OutputSink + Send>,
    ) -> (Observables, Box<dyn OutputSink + Send>),
) -> i32 {
    match describe(file_of(program), arguments, *program == Program::Stdin) {
        Ok(mut description) => {
            description.file_handles = None;
            let (served, mut sink) = write(description, Box::new(Process));
            ended(&served, sink.as_mut())
        }
        Err(code) => code,
    }
}

/// `-l` over one program, `do_cli`'s `PHP_CLI_MODE_LINT` arm
/// (`php_cli.c:954-961`): the program compiled by [`Engine::lint`] and
/// not served, then `No syntax errors detected in %s` and 0, or `Errors
/// parsing %s` and 255, on the sink after what the compiler rendered.
/// A file that cannot be opened is exit 1 before a request begins. What
/// [`dump`] prints stands on the second stream before the sentence, as
/// php's does. `sink` is handed back.
pub fn lint(
    engine: &Engine,
    program: &Program,
    mut sink: Box<dyn OutputSink + Send>,
) -> (i32, Box<dyn OutputSink + Send>) {
    let mut description = match describe(file_of(program), &[], *program == Program::Stdin) {
        Ok(description) => description,
        Err(code) => return (code, sink),
    };
    // `do_cli`'s `PHP_CLI_MODE_LINT` arm calls no
    // `cli_register_file_handles`.
    description.file_handles = None;
    let name = description.argv.first().cloned().unwrap_or_default();
    dump(engine, &description, sink.as_mut());
    let (served, mut sink) = engine.lint(description, sink);
    if served.start_failed {
        return (ended(&served, sink.as_mut()), sink);
    }
    let mut line = if served.exit_code == 0 {
        b"No syntax errors detected in ".to_vec()
    } else {
        b"Errors parsing ".to_vec()
    };
    line.extend_from_slice(&name);
    line.push(b'\n');
    sink.write(&line).ok();
    sink.flush();
    (if served.exit_code == 0 { 0 } else { 255 }, sink)
}

/// A listing `do_cli` prints in a request of its own
/// (`php_cli.c:1036-1131`), the file a bare word named opened first as
/// `do_repeat` opens it: php's C there constructs the reflector, prints
/// it or the `ReflectionException`'s message, or prints `php_info_print_module` or
/// the configuration files; served here as the same calls in PHP, which
/// is what `xtask crosscheck` holds `--re` and `--ri` to the oracle's
/// bytes through; the exit code.
pub fn listing(engine: &Engine, listing: &Listing, file: Option<&Path>) -> i32 {
    let code: Vec<u8> = match listing {
        Listing::Function(name) if name.windows(2).any(|pair| pair == b"::") => {
            reflect(b"\\ReflectionMethod::createFromMethodName(", name)
        }
        Listing::Function(name) => reflect(b"new \\ReflectionFunction(", name),
        Listing::Class(name) => reflect(b"new \\ReflectionClass(", name),
        Listing::Extension(name) => reflect(b"new \\ReflectionExtension(", name),
        Listing::ZendExtension(name) => reflect(b"new \\ReflectionZendExtension(", name),
        Listing::ExtensionInfo(name) => {
            let name = quoted(name);
            [
                &b"if (\\extension_loaded("[..],
                &name,
                b")) { (new \\ReflectionExtension(",
                &name,
                b"))->info(); } else { echo \"Extension '\", ",
                &name,
                b", \"' not present.\\n\"; exit(1); }",
            ]
            .concat()
        }
        Listing::IniFiles => ini_files(engine),
    };
    match describe(file, &[], false) {
        Ok(mut description) => {
            description.file_handles = None;
            let served = in_request(engine, description, code);
            ended(&served, &mut Process)
        }
        Err(code) => code,
    }
}

/// `PHP_CLI_MODE_SHOW_INI_CONFIG` (`Z/sapi/cli/php_cli.c:1127-1142`):
/// the paths quoted where present, `(none)` only for NULL, and the
/// scanned files joined with comma-newlines and their final newline,
/// before the listing's own newline. Serve the bytes in the listing's
/// request so its startup, `ob_` buffering and shutdown are the CLI's.
fn ini_files(engine: &Engine) -> Vec<u8> {
    let files = engine.ini_files();
    let path = |path: Option<&[u8]>| {
        path.map_or_else(
            || b"(none)".to_vec(),
            |path| [b"\"".as_slice(), path, b"\""].concat(),
        )
    };
    let scanned: Vec<_> = files.scanned().collect();
    let scanned = if scanned.is_empty() {
        b"(none)".to_vec()
    } else {
        [scanned.join(b",\n".as_slice()), b"\n".to_vec()].concat()
    };
    let body = [
        b"Configuration File (php.ini) Path: \"".as_slice(),
        files.ini_path(),
        b"\"\nLoaded Configuration File:         ",
        &path(files.loaded()),
        b"\nScan for additional .ini files in: ",
        &path(files.scan_dir()),
        b"\nAdditional .ini files parsed:      ",
        &scanned,
        b"\n",
    ]
    .concat();
    [b"echo ".as_slice(), &quoted(&body), b";"].concat()
}

/// `-i`, `do_cli`'s first loop (`php_cli.c:622-630`): `php_print_info`
/// with every section but the credits, in a request started before
/// `do_repeat` set `php_self` or an `argv`, and 0; 1 with nothing more
/// written where the request's start failed (`goto err`).
pub fn info(engine: &Engine) -> i32 {
    let code = b"phpinfo(INFO_ALL & ~INFO_CREDITS);".to_vec();
    i32::from(in_request(engine, unnamed(), code).start_failed)
}

/// `-m`'s `php_request_startup` (`php_cli.c:643-655`): an empty request,
/// which displays what the startup raised at its first moment, every
/// extension's `rinit` and `rshutdown` served as php serves them around
/// the listing; 1 with nothing more written where its start failed
/// (`goto err`), and otherwise the request's status.
pub(crate) fn started(engine: &Engine) -> i32 {
    let served = in_request(engine, unnamed(), Vec::new());
    if served.start_failed {
        1
    } else {
        served.exit_code
    }
}

/// A request before `do_repeat` set `php_self` or an `argv`.
fn unnamed() -> RequestDescription {
    RequestDescription {
        env: std::env::vars_os()
            .map(|(n, v)| (n.as_encoded_bytes().to_vec(), v.as_encoded_bytes().to_vec()))
            .collect(),
        cwd: std::env::current_dir().unwrap_or_default(),
        server: server_variables(b"", b""),
        sapi_name: String::from(NAME),
        ..RequestDescription::default()
    }
}

/// A reflector made by `made` over `name`, printed as `zend_print_zval`
/// prints it, or the message of what its construction threw after
/// `Exception: `, and exit 1.
fn reflect(made: &[u8], name: &[u8]) -> Vec<u8> {
    [
        &b"try { $r = "[..],
        made,
        &quoted(name),
        b"); } catch (\\Throwable $e) { echo 'Exception: ', $e->getMessage(), \"\\n\"; exit(1); } echo $r, \"\\n\";",
    ]
    .concat()
}

/// `bytes` as a PHP single-quoted string.
fn quoted(bytes: &[u8]) -> Vec<u8> {
    let mut quoted = vec![b'\''];
    for &byte in bytes {
        if byte == b'\\' || byte == b'\'' {
            quoted.push(b'\\');
        }
        quoted.push(byte);
    }
    quoted.push(b'\'');
    quoted
}

/// `code` served in `description`'s request with no program, as `do_cli`
/// serves its reports after `php_request_startup`; what it observed.
fn in_request(engine: &Engine, description: RequestDescription, code: Vec<u8>) -> Observables {
    engine
        .command_line(description, CommandLine::Direct(code), Box::new(Process))
        .0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{Ask, Options, Refused};
    use std::path::PathBuf;

    fn parse(words: &[&str]) -> Result<Options, Refused> {
        let argv: Vec<OsString> = words.iter().map(OsString::from).collect();
        Options::parse(b"php", &argv, &|_| false)
    }

    /// One sink over both streams, in the order the request wrote them.
    #[derive(Default)]
    struct Capture(Vec<u8>);

    impl OutputSink for Capture {
        fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
            self.0.extend_from_slice(bytes);
            Ok(())
        }
        fn flush(&mut self) {}
        fn send_headers(&mut self, _head: &ResponseHead) -> Result<(), Aborted> {
            Ok(())
        }
        fn log(&mut self, entry: &[u8]) {
            self.0.extend_from_slice(entry);
            self.0.push(b'\n');
        }
        fn write_stderr(&mut self, bytes: &[u8]) {
            self.0.extend_from_slice(bytes);
        }
    }

    /// What `-r` prints under the `-d` switches given, served by an engine
    /// of the bundle built from them as the binary builds it, so an
    /// extension's entry, `date.timezone`, is one.
    fn served_under(switches: &[&str], code: &str) -> (String, String) {
        let mut words = vec!["-n"];
        for switch in switches {
            words.extend(["-d", switch]);
        }
        words.extend(["-r", code]);
        let Ok(options) = parse(&words) else {
            return (String::from("refused"), String::new());
        };
        let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
        let mut builder = Engine::builder()
            .extensions(extensions)
            .configure(php_bundle::CONFIGURE)
            .tier::<php_bundle::Tier>();
        for (name, value) in &options.startup.ini {
            builder = builder.ini(name, value);
        }
        let engine = builder.build();
        let Ask::Serve(Program::Direct(code), arguments) = &options.ask else {
            return (String::from("not served"), String::new());
        };
        let Ok(description) = describe(None, arguments, false) else {
            return (String::from("not described"), String::new());
        };
        let (_, sink) = engine.command_line(
            description,
            CommandLine::Direct(code.clone()),
            Box::new(Capture::default()),
        );
        let sink = captured(sink);
        (
            String::from_utf8_lossy(&sink.0).into_owned(),
            String::from_utf8_lossy(engine.startup_errors()).into_owned(),
        )
    }

    /// The pinned oracle at 8.5.9 under `-n` and these switches: a `-d`
    /// value is a line of INI text, so `;` opens a comment, an unquoted
    /// value is trimmed, a quoted one keeps its `;`, one that opens with
    /// a byte neither alphanumeric nor a quote is quoted by the define and
    /// stands as written, `off` is empty and a bare name `1`, a Zend
    /// constant and its operators are read, and `${…}` reads an entry set
    /// before it.
    #[test]
    fn a_define_is_read_by_the_ini_grammar() {
        let switches = [
            "date.timezone=UTC;",
            "precision=5 ; x",
            "error_log=${precision}x",
            "docref_root= 5",
            "docref_ext=\"a;b\" ; c",
            "display_errors=off",
            "log_errors",
            "error_reporting=E_ALL & ~E_NOTICE",
            "serialize_precision=PHP_INT_SIZE",
            "error_prepend_string=;x",
            "error_append_string=",
        ];
        let code = r#"foreach (["date.timezone","precision","error_log","docref_root","docref_ext","display_errors","log_errors","error_reporting","serialize_precision","error_prepend_string","error_append_string"] as $n) echo $n, "=[", ini_get($n), "]\n";"#;
        assert_eq!(
            served_under(&switches, code),
            (
                String::from(
                    "date.timezone=[UTC]\nprecision=[5]\nerror_log=[5x]\ndocref_root=[ 5]\n\
                     docref_ext=[a;b]\ndisplay_errors=[]\nlog_errors=[1]\nerror_reporting=[30711]\n\
                     serialize_precision=[8]\nerror_prepend_string=[;x]\nerror_append_string=[]\n"
                ),
                String::new()
            )
        );
    }

    /// A syntax error ends the reading where it stands: what came before
    /// it is set and nothing after, and `ini_error`'s line is kept for
    /// `stderr`. The oracle writes `on line 8` for the same two switches,
    /// its CLI having put five lines of its own before them, which this
    /// host does not.
    #[test]
    fn a_define_that_does_not_parse_keeps_what_came_before_it() {
        let code = r#"echo ini_get("date.timezone"), "|", ini_get("precision");"#;
        assert_eq!(
            served_under(&["date.timezone=Europe/Paris", "precision=bar\""], code),
            (
                String::from("Europe/Paris|14"),
                String::from(
                    "PHP:  syntax error, unexpected end of file, expecting TC_DOLLAR_CURLY or TC_QUOTED_STRING or '\"' in Unknown on line 3\n"
                )
            )
        );
    }

    /// What a [`Capture`] lent to a request was handed back as.
    fn captured(sink: Box<dyn OutputSink + Send>) -> Capture {
        let sink: Box<dyn std::any::Any> = sink;
        sink.downcast().map(|sink| *sink).unwrap_or_default()
    }

    /// [`lint`] into a fresh [`Capture`]: its status, and what it wrote.
    fn linted(engine: &Engine, program: &Program) -> (i32, Capture) {
        let (status, sink) = lint(engine, program, Box::new(Capture::default()));
        (status, captured(sink))
    }

    fn scratch(name: &str, source: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("php_sapi-lint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join(name);
        std::fs::write(&path, source).ok();
        path
    }

    /// The oracle at 8.5.9 under `-n -l`: `No syntax errors detected in
    /// %s` and 0, or the parse error and `Errors parsing %s` and 255, a
    /// deprecation rendered before the sentence and changing nothing.
    #[test]
    fn lint_prints_phps_sentence_and_ends_with_phps_status() {
        let engine = Engine::builder().ini("html_errors", "0").build();
        let ok = scratch("ok.php", "<?php\necho 1;\n");
        let bad = scratch("bad.php", "<?php\necho 1 +;\n");
        let deprecated = scratch("dep.php", "<?php\n(integer)1;\n");
        let (status, sink) = linted(&engine, &Program::File(ok.clone()));
        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8_lossy(&sink.0),
            format!("No syntax errors detected in {}\n", ok.display())
        );
        let (status, sink) = linted(&engine, &Program::File(bad.clone()));
        assert_eq!(status, 255);
        assert_eq!(
            String::from_utf8_lossy(&sink.0),
            format!(
                "\nParse error: syntax error, unexpected token \";\" in {0} on line 2\nErrors parsing {0}\n",
                bad.display()
            )
        );
        let (status, sink) = linted(&engine, &Program::File(deprecated.clone()));
        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8_lossy(&sink.0),
            format!(
                "\nDeprecated: Non-canonical cast (integer) is deprecated, use the (int) cast instead in {0} on line 2\nNo syntax errors detected in {0}\n",
                deprecated.display()
            )
        );
        let (status, sink) = linted(&engine, &Program::File(PathBuf::from("nope.php")));
        assert_eq!(status, 1);
        assert_eq!(sink.0, b"");
    }

    /// An extension whose `rinit` throws, as session's does where its data
    /// does not decode; the CLI reads no cookie, so no extension
    /// of the bundle fails a CLI request's start.
    fn failing_start() -> Engine {
        /// Core's `Error`.
        #[php_ext::attr::php_class(extern)]
        struct Error;
        /// `Error` as the extension's engine start resolved it.
        #[derive(php_ext::abi::EngineState)]
        struct Failing(php_ext::values::EngineClassId);
        fn throws(req: &mut php_ext::request::Request) -> Result<(), php_ext::raising::Thrown> {
            let Some(&Failing(error)) = req.engine::<Failing>() else {
                return Err(php_ext::raising::fatal(
                    req,
                    "InternalError: no engine state",
                ));
            };
            Err(php_ext::raising::throw_error(req, error, "rinit threw"))
        }
        let failing = php_ext::abi::ExtensionBuilder::default()
            .engine_state(|start| start.class::<Error>().map(Failing))
            .rinit(throws)
            .build("failing");
        Engine::builder()
            .ini("html_errors", "0")
            .extensions([failing])
            .build()
    }

    /// What the failed start displays under the CLI.
    const FAILED: &str = "\nFatal error: Uncaught Error: rinit threw in [no active file]:0\nStack trace:\n#0 {main}\n  thrown in [no active file] on line 0\n";

    /// A request whose start failed ends as `do_cli` ends it: what the
    /// failure displayed, `Could not startup.` after it, and 1, the program
    /// never started (`php_cli.c:919-922`, `:1188-1191`).
    #[test]
    fn a_failed_start_is_could_not_startup_and_status_one() {
        let engine = failing_start();
        let description =
            describe(None, &[], false).expect("a command line of no file describes a request");
        let (served, mut sink) = engine.command_line(
            description,
            CommandLine::Direct(b"echo 'ran';".to_vec()),
            Box::new(Capture::default()),
        );
        assert!(served.start_failed);
        assert_eq!(ended(&served, sink.as_mut()), 1);
        let sink = captured(sink);
        assert_eq!(
            String::from_utf8_lossy(&sink.0),
            format!("{FAILED}Could not startup.\n")
        );
    }

    /// `-l`'s request starts as any other's, so a failed start is the same
    /// ending in place of the verdict, its status the lint loop's.
    #[test]
    fn a_failed_start_under_lint_writes_no_verdict() {
        let engine = failing_start();
        let ok = scratch("start-failed.php", "<?php\necho 1;\n");
        let (status, sink) = linted(&engine, &Program::File(ok));
        assert_eq!(status, 1);
        assert_eq!(
            String::from_utf8_lossy(&sink.0),
            format!("{FAILED}Could not startup.\n")
        );
    }
}
