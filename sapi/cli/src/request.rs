//! The requests `do_cli` serves (`Z/sapi/cli/php_cli.c:840-1180`): the
//! request description `do_repeat` builds for a program, the sink on the
//! process's two streams, and each mode served on it, a program, `-r`'s
//! code and stdin's lines, a lint, a highlight or a strip, and the
//! reports `do_cli` prints in a request of their own.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;

use php_sapi::{
    Aborted, CommandLine, EachLine, Engine, FileHandles, Observables, OutputSink,
    RequestDescription, ResponseHead,
};

use crate::NAME;
use crate::options::{Each, Lines, Program, Report};

/// The sink on the process's streams, php's CLI: body bytes on stdout,
/// the header block nowhere, since `sapi_cli_send_headers` sends none,
/// and both the log and the error stream on stderr, each log entry with
/// `\n` after it (`sapi_cli_log_message`). The request holds it and each
/// call reaches the fd as it is made, as `sapi_cli_ub_write` writes each
/// chunk to fd 1, so what a child process the request starts writes to
/// the stdout it inherits stands where php's would (#741). A stream that
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
        let _ = std::io::stdout().lock().flush();
        crate::ffi::write_stdout(bytes).map_err(|_| Aborted {
            exit_status: Some(255),
        })
    }

    fn flush(&mut self) {
        let _ = std::io::stdout().lock().flush();
    }

    fn send_headers(&mut self, _head: &ResponseHead) {}

    fn log(&mut self, entry: &[u8]) {
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(entry);
        let _ = stderr.write_all(b"\n");
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        let _ = std::io::stderr().lock().write_all(bytes);
    }

    fn held(&mut self) -> Option<Box<dyn OutputSink + Send>> {
        Some(Box::new(Process))
    }
}

/// `php_self` for a request with no file.
const STANDARD_INPUT: &[u8] = b"Standard input code";

/// The request description `do_repeat` builds (`php_cli.c:878-916`):
/// `argv` with `php_self` first, which is the file's name or `Standard
/// input code`; the process's environment and its working directory; the
/// file named on the line, which the engine reads by name, or stdin read
/// whole and served as `Standard input code` where `stdin` asks it; and
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
                let _ = stderr.write_all(b"Could not open input file: ");
                let _ = stderr.write_all(path.as_os_str().as_encoded_bytes());
                let _ = stderr.write_all(b"\n");
                return Err(1);
            }
            let name = path.as_os_str().as_encoded_bytes().to_vec();
            (name.clone(), name, None)
        }
        None if stdin => {
            let mut source = Vec::new();
            let _ = std::io::stdin().lock().read_to_end(&mut source);
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
    let mut sink = Process;
    match program {
        Program::Direct(code) => {
            engine
                .command_line(description, CommandLine::Direct(code.clone()), &mut sink)
                .exit_code
        }
        Program::File(_) | Program::Stdin => {
            dump(engine, &description, &mut sink);
            engine.serve(description, &mut sink).exit_code
        }
    }
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
    engine
        .command_line(description, CommandLine::Shell, &mut Process)
        .exit_code
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
    engine
        .command_line(description, command, &mut Process)
        .exit_code
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
        sink.write_stderr(arrays.as_bytes());
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
    write: impl FnOnce(RequestDescription, &mut dyn OutputSink) -> Observables,
) -> i32 {
    match describe(file_of(program), arguments, *program == Program::Stdin) {
        Ok(mut description) => {
            description.file_handles = None;
            write(description, &mut Process).exit_code
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
/// php's does.
pub fn lint(engine: &Engine, program: &Program, sink: &mut dyn OutputSink) -> i32 {
    let mut description = match describe(file_of(program), &[], *program == Program::Stdin) {
        Ok(description) => description,
        Err(code) => return code,
    };
    // `do_cli`'s `PHP_CLI_MODE_LINT` arm calls no
    // `cli_register_file_handles`.
    description.file_handles = None;
    let name = description.argv.first().cloned().unwrap_or_default();
    dump(engine, &description, sink);
    let served = engine.lint(description, sink);
    let mut line = if served.exit_code == 0 {
        b"No syntax errors detected in ".to_vec()
    } else {
        b"Errors parsing ".to_vec()
    };
    line.extend_from_slice(&name);
    line.push(b'\n');
    let _ = sink.write(&line);
    sink.flush();
    if served.exit_code == 0 { 0 } else { 255 }
}

/// A report `do_cli` prints in a request of its own
/// (`php_cli.c:1036-1131`), the file a bare word named opened first as
/// `do_repeat` opens it: php's C there constructs the reflector, prints
/// it or the exception's message, or prints `php_info_print_module` or
/// the configuration files; served here as the same calls in PHP, which
/// is what `xtask crosscheck` holds `--re` and `--ri` to the oracle's
/// bytes through; the exit code.
pub fn report(engine: &Engine, report: &Report, file: Option<&Path>) -> i32 {
    let code: Vec<u8> = match report {
        Report::Function(name) if name.windows(2).any(|pair| pair == b"::") => {
            reflect(b"\\ReflectionMethod::createFromMethodName(", name)
        }
        Report::Function(name) => reflect(b"new \\ReflectionFunction(", name),
        Report::Class(name) => reflect(b"new \\ReflectionClass(", name),
        Report::Extension(name) => reflect(b"new \\ReflectionExtension(", name),
        Report::ZendExtension(name) => reflect(b"new \\ReflectionZendExtension(", name),
        Report::ExtensionInfo(name) => {
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
        Report::IniFiles => {
            b"printf(\"Configuration File (php.ini) Path: \\\"%s\\\"\\n\", PHP_CONFIG_FILE_PATH); \
            echo \"Loaded Configuration File:         (none)\\n\", \
            \"Scan for additional .ini files in: (none)\\n\", \
            \"Additional .ini files parsed:      (none)\\n\";"
                .to_vec()
        }
    };
    match describe(file, &[], false) {
        Ok(mut description) => {
            description.file_handles = None;
            in_request(engine, description, code)
        }
        Err(code) => code,
    }
}

/// `-i`, `do_cli`'s first loop (`php_cli.c:622-630`): `php_print_info`
/// with every section but the credits, in a request started before
/// `do_repeat` set `php_self` or an `argv`, and 0.
pub fn info(engine: &Engine) -> i32 {
    let code = b"phpinfo(INFO_ALL & ~INFO_CREDITS);".to_vec();
    let _ = in_request(engine, unnamed(), code);
    0
}

/// `php_module_startup` as `main` runs it before `-v`, `-m` and `-S`
/// (`php_cli.c:1341`), which php serves with no request: what the
/// startup raised is displayed at a request's first moment here, so an
/// empty one under the host's `name` is served first, on `stdout`, and
/// its status 1 where the startup failed is the answer. The built-in
/// server's responses carry none of it, since php wrote it to the
/// process's `stdout` before any client connected; a startup that failed
/// leaves no server.
///
/// ```php
/// $dir = sys_get_temp_dir() . '/' . uniqid('srv');
/// mkdir($dir);
/// file_put_contents("$dir/index.php", '<?php echo "body\n";');
/// $port = 18000 + random_int(0, 999);
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'extension_dir=/nope', '-d', 'extension=x', '-d', 'disable_functions=exit', '-S', "127.0.0.1:$port", '-t', $dir], [1 => ['file', '/dev/null', 'w'], 2 => ['file', '/dev/null', 'w']], $pipes);
/// usleep(300000);
/// echo file_get_contents("http://127.0.0.1:$port/"), file_get_contents("http://127.0.0.1:$port/");
/// proc_terminate($p); proc_close($p);
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'magic_quotes_gpc=1', '-S', "127.0.0.1:$port", '-t', $dir], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]), proc_close($p), "\n";
/// unlink("$dir/index.php"); rmdir($dir);
/// ```
/// ```expect process
/// body
/// body
/// <br />
/// <b>Fatal error</b>:  Directive 'magic_quotes_gpc' is no longer available in PHP in <b>Unknown</b> on line <b>0</b><br />
/// Stack trace:
/// #0 {main}
/// 1
/// ```
pub(crate) fn started(engine: &Engine, name: &str) -> i32 {
    let description = RequestDescription {
        sapi_name: String::from(name),
        ..unnamed()
    };
    in_request(engine, description, Vec::new())
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
/// serves its reports after `php_request_startup`; the exit code.
fn in_request(engine: &Engine, description: RequestDescription, code: Vec<u8>) -> i32 {
    engine
        .command_line(description, CommandLine::Direct(code), &mut Process)
        .exit_code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{Ask, Options, Refused};
    use std::path::PathBuf;

    fn parse(words: &[&str]) -> Result<Options, Refused> {
        let argv: Vec<OsString> = words.iter().map(OsString::from).collect();
        Options::parse(&argv, &|_| false)
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
        fn send_headers(&mut self, _head: &ResponseHead) {}
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
            .extensions(&extensions)
            .configure(php_bundle::CONFIGURE)
            .tier::<php_bundle::Tier>();
        for (name, value) in &options.ini {
            builder = builder.ini(name, value);
        }
        let engine = builder.build();
        let Ask::Serve(Program::Direct(code), arguments) = &options.ask else {
            return (String::from("not served"), String::new());
        };
        let Ok(description) = describe(None, arguments, false) else {
            return (String::from("not described"), String::new());
        };
        let mut sink = Capture::default();
        engine.command_line(description, CommandLine::Direct(code.clone()), &mut sink);
        (
            String::from_utf8_lossy(&sink.0).into_owned(),
            engine.startup_errors().to_owned(),
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

    fn scratch(name: &str, source: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("php_sapi-lint-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        let _ = std::fs::write(&path, source);
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
        let mut sink = Capture::default();
        assert_eq!(lint(&engine, &Program::File(ok.clone()), &mut sink), 0);
        assert_eq!(
            String::from_utf8_lossy(&sink.0),
            format!("No syntax errors detected in {}\n", ok.display())
        );
        let mut sink = Capture::default();
        assert_eq!(lint(&engine, &Program::File(bad.clone()), &mut sink), 255);
        assert_eq!(
            String::from_utf8_lossy(&sink.0),
            format!(
                "\nParse error: syntax error, unexpected token \";\" in {0} on line 2\nErrors parsing {0}\n",
                bad.display()
            )
        );
        let mut sink = Capture::default();
        assert_eq!(
            lint(&engine, &Program::File(deprecated.clone()), &mut sink),
            0
        );
        assert_eq!(
            String::from_utf8_lossy(&sink.0),
            format!(
                "\nDeprecated: Non-canonical cast (integer) is deprecated, use the (int) cast instead in {0} on line 2\nNo syntax errors detected in {0}\n",
                deprecated.display()
            )
        );
        let mut sink = Capture::default();
        assert_eq!(
            lint(
                &engine,
                &Program::File(PathBuf::from("nope.php")),
                &mut sink
            ),
            1
        );
        assert_eq!(sink.0, b"");
    }
}
