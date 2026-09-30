//! php's CLI, `php-src/sapi/cli`, as one crate under `sapi/` (ADR-0008):
//! the binary `php`, reaching the engine through `php_sapi` alone. The
//! host is `sapi/cli/php_cli.c`: the command line read as `main` and
//! `do_cli` read it through `php_getopt`, one request description built
//! from it, the sink on the process's two streams, and the exit code as
//! `serve` ends the request (design §4.1, §4.3 seam 4, §5.14).
//!
//! What the command line asks for is one of eight things, as `do_cli`
//! settles it: the usage, the version, `phpinfo()` as text, the
//! extension listing, a request over one program, `-l`, the lint of
//! each program named, or `-s` and `-w`, the program highlighted or
//! stripped as `zend_highlight` and `zend_strip` write it. A program is a file named on the line or after
//! `-f`, the code after `-r`, or stdin. `-n` and every `-d` are the
//! engine's to lay over its defaults before it is built, so the binary
//! reads them off [`Options`] and builds the engine; the host then
//! serves [`Options::ask`] on it. Which of the three programs is named
//! more than once, or with another, is refused with php's sentence
//! before anything is built.
//!
//! The switches taken are `php_cli.c`'s `OPTIONS` table as `main` and
//! `do_cli` read it: `-s` and `-w`, each php's sentence where `-r`
//! came before it; `-a`, which is `do_cli`'s sentence for a build
//! without the readline extension, since this engine has none; `-c`,
//! `-e`, `-H` and `-t`, each taken and changing nothing, as `-n` is: no
//! `php.ini` is looked for anywhere, the compiler emits no extended
//! info, no process title is rewritten (so the served name is never
//! blanked, as php's `-H` blanks it through the `argv` memory it
//! zeroed), and `-t` is the dev server's; `-C` and `-q`, php's own
//! no-ops; `-d`, `-f`, `-h`, `-?`, `-i`, `-l`, `-m`, `-n`, `-r`, `-v`,
//! `--version` and `--`, and each long spelling `OPTIONS` gives a
//! switch taken.
//!
//! What php's CLI takes that this host refuses, each with php's usage on
//! stdout and exit 1: `-B`, `-R`, `-F` and `-E` over stdin's lines, `-S` the dev server, `--rf`, `--rc`,
//! `--re`, `--rz` and `--ri` the reflection printers, `--ini` and
//! `--ini=diff`, and `--repeat`. A switch php does not know is
//! `php_getopt`'s line on stderr before the usage.
//!
//! Where `php_getopt` and `do_cli` read stdin lazily, this host does not:
//! nothing in the engine reads the request's body yet (design §4.3 seam
//! 4, the body stream), so stdin is read whole where it is the program.

#![forbid(unsafe_code)]
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use php_sapi::getopt::{Getopt, Opt, Taken, getopt_line, opt, os};
use php_sapi::ini::define;
use php_sapi::{
    Engine, FileHandles, Observables, OutputSink, PHP_VERSION, RequestDescription, ResponseHead,
};

/// `sapi_module.name` for the CLI: what `php_sapi_name()` answers and
/// what the version line carries.
pub const NAME: &str = "cli";

/// The program one request serves, as `do_cli` settles it: a file named
/// on the line or after `-f`, the code after `-r`, served as
/// `zend_eval_string` serves it under the name `Command line code`, or
/// stdin read as the file named `Standard input code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Program {
    File(PathBuf),
    Direct(Vec<u8>),
    Stdin,
}

/// What the command line asks the binary for once the engine's own
/// switches are taken off it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// `-h`, `-?`: `php_cli_usage`, and exit 0.
    Usage,
    /// `-v`, `--version`: `php_print_version`.
    Version,
    /// `-i`: `php_print_info` as text.
    Info,
    /// `-m`: the loaded extensions, `print_modules`.
    Extensions,
    /// A request over one program, with what follows it as `$argv[1..]`.
    Serve(Program, Vec<OsString>),
    /// `-l`: each program linted in turn, one request each, as the
    /// repeat at `do_cli`'s `out:` label walks the words left on the
    /// line until `--`. `refused` is the sentence `do_cli` `PUTS` on
    /// stdout before `goto err`, which under `-l` sets the status to 1
    /// and then lints the bare words all the same.
    Lint {
        programs: Vec<Program>,
        refused: Option<String>,
    },
    /// `-s`: the program as `zend_highlight` writes it, stdin where no
    /// file is named.
    Highlight(Program, Vec<OsString>),
    /// `-w`: the program as `zend_strip` writes it.
    Strip(Program, Vec<OsString>),
}

/// The command line, read: the engine's switches and what is asked of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// `-n`: `php.ini` is not looked for. The engine reads no file yet, so
    /// the switch is taken and changes nothing (design §11, row 15–21).
    pub ini_ignore: bool,
    /// Each `-d name=value` split at its first `=`, in the order given;
    /// a `-d name` is `name=1`, as `php_ini_builder_define` writes it.
    /// The value is the text the engine's builder writes as the define's
    /// line and reads with the INI grammar (`EngineBuilder::ini`), so
    /// `UTC;` stands here and is `UTC` once read.
    pub ini: Vec<(String, String)>,
    pub ask: Ask,
}

/// Why a command line was not read: what goes on each of the process's
/// streams, `php_getopt`'s line on stderr and `PUTS(param_error)` or
/// `php_cli_usage` on stdout, and the exit code `main` ends with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// `param_mode_conflict`, `php_cli.c:563`.
const CONFLICT: &str = "Either execute direct code, process stdin or use a file.\n";

/// `do_cli`'s sentence for `-a` when `cli_shell_callbacks.cli_shell_run`
/// is unset, which it is with no readline extension linked.
const NO_SHELL: &str = "Interactive shell (-a) requires the readline extension.\n";

/// `php_cli.c`'s `PHP_CLI_MODE_*` that this host reaches: `do_cli`'s
/// `PROCESS_STDIN` is not among them, since `-B`, `-R`, `-F` and `-E`
/// are refused before it is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Standard,
    Direct,
    Lint,
    Highlight,
    Strip,
}

/// `OPTIONS`, `php_cli.c:124-165`, row for row.
const OPTIONS: &[Opt] = &[
    opt(b'a', 0, "interactive"),
    opt(b'B', 1, "process-begin"),
    opt(b'C', 0, "no-chdir"),
    opt(b'c', 1, "php-ini"),
    opt(b'd', 1, "define"),
    opt(b'E', 1, "process-end"),
    opt(b'e', 0, "profile-info"),
    opt(b'F', 1, "process-file"),
    opt(b'f', 1, "file"),
    opt(b'h', 0, "help"),
    opt(b'i', 0, "info"),
    opt(b'l', 0, "syntax-check"),
    opt(b'm', 0, "modules"),
    opt(b'n', 0, "no-php-ini"),
    opt(b'q', 0, "no-header"),
    opt(b'R', 1, "process-code"),
    opt(b'H', 0, "hide-args"),
    opt(b'r', 1, "run"),
    opt(b's', 0, "syntax-highlight"),
    opt(b's', 0, "syntax-highlighting"),
    opt(b'S', 1, "server"),
    opt(b't', 1, "docroot"),
    opt(b'w', 0, "strip"),
    opt(b'?', 0, "usage"),
    opt(b'v', 0, "version"),
    opt(10, 1, "rf"),
    opt(10, 1, "rfunction"),
    opt(11, 1, "rc"),
    opt(11, 1, "rclass"),
    opt(12, 1, "re"),
    opt(12, 1, "rextension"),
    opt(13, 1, "rz"),
    opt(13, 1, "rzendextension"),
    opt(14, 1, "ri"),
    opt(14, 1, "rextinfo"),
    opt(15, 2, "ini"),
    opt(16, 1, "repeat"),
    opt(b'-', 0, ""),
];

/// What `do_cli`'s second loop settled: what is being done with the
/// line, the program `-f` or
/// `-r` named, the first `param_error`, and where the switches ended.
struct Settled {
    mode: Mode,
    program: Option<Program>,
    param_error: Option<&'static str>,
    optind: usize,
}

impl Options {
    /// Read `argv[1..]` as php's `main` and `do_cli` read it: `main`'s
    /// pass takes the ini switches and ends on `-h`, on a switch php
    /// does not know, or at the first of `-i`, `-v` and `-m`, which
    /// `do_cli`'s first loop then answers; `do_cli`'s second loop settles
    /// what is being done with the line, then the program, then what is
    /// left as its arguments.
    /// `--` ends the switches; a word after it is the program unless one
    /// was named already.
    pub fn parse(argv: &[OsString]) -> Result<Options, Refused> {
        let argv: Vec<Vec<u8>> = argv
            .iter()
            .map(|word| word.as_encoded_bytes().to_vec())
            .collect();
        let mut options = Options {
            ini_ignore: false,
            ini: Vec::new(),
            ask: Ask::Serve(Program::Stdin, Vec::new()),
        };
        if let Some(ask) = options.main_pass(&argv)? {
            options.ask = ask;
            return Ok(options);
        }
        let settled = Options::mode_pass(&argv)?;
        let after_dashes = settled.optind > 0
            && argv.get(settled.optind - 1).map(Vec::as_slice) == Some(b"--".as_slice());
        let mut rest = argv[settled.optind.min(argv.len())..].iter();
        if let Some(error) = settled.param_error {
            // `PUTS(param_error)`, exit 1 and `goto err`, which under `-l`
            // falls through `out:` into the lint of the bare words.
            let programs = if settled.mode == Mode::Lint && !after_dashes {
                bare_words(&mut rest)
            } else {
                Vec::new()
            };
            if programs.is_empty() {
                return Err(Refused {
                    stdout: error.to_owned(),
                    stderr: String::new(),
                    exit_code: 1,
                });
            }
            options.ask = Ask::Lint {
                programs,
                refused: Some(error.to_owned()),
            };
            return Ok(options);
        }
        let mut program = settled.program;
        if program.is_none() && settled.mode != Mode::Direct && !after_dashes {
            program = rest
                .next()
                .map(|word| Program::File(PathBuf::from(os(word))));
        }
        options.ask = if settled.mode == Mode::Lint {
            // php lints stdin for each word left after `--`, without end;
            // this host lints it once.
            let programs = if after_dashes {
                vec![Program::Stdin]
            } else {
                lint_programs(program, &mut rest)
            };
            Ask::Lint {
                programs,
                refused: None,
            }
        } else {
            let program = program.unwrap_or(Program::Stdin);
            let arguments = rest.map(|word| os(word)).collect();
            match settled.mode {
                Mode::Highlight => Ask::Highlight(program, arguments),
                Mode::Strip => Ask::Strip(program, arguments),
                Mode::Standard | Mode::Direct | Mode::Lint => Ask::Serve(program, arguments),
            }
        };
        Ok(options)
    }

    /// `main`'s loop over the line (`php_cli.c:1290-1325`), `show_err`
    /// set: the ini switches onto `self`, and what ends it early.
    fn main_pass(&mut self, argv: &[Vec<u8>]) -> Result<Option<Ask>, Refused> {
        let mut getopt = Getopt::new(OPTIONS);
        loop {
            let ask = match getopt.next(argv) {
                Ok(Taken::End) => return Ok(None),
                Err((argument, at, error)) => {
                    return Err(Refused {
                        stderr: getopt_line(argv, argument, at, error),
                        ..usage(1)
                    });
                }
                Ok(Taken::Switch(code, value)) => match code {
                    b'n' => {
                        self.ini_ignore = true;
                        continue;
                    }
                    b'd' => {
                        self.ini
                            .push(define(&String::from_utf8_lossy(&value.unwrap_or_default())));
                        continue;
                    }
                    b'h' | b'?' => Ask::Usage,
                    b'S' => return Err(usage(1)),
                    b'i' => Ask::Info,
                    b'v' => Ask::Version,
                    b'm' => Ask::Extensions,
                    _ => continue,
                },
            };
            return Ok(Some(ask));
        }
    }

    /// `do_cli`'s second loop (`php_cli.c:666-832`): what is being done
    /// with the line, and the program.
    fn mode_pass(argv: &[Vec<u8>]) -> Result<Settled, Refused> {
        let mut settled = Settled {
            mode: Mode::Standard,
            program: None,
            param_error: None,
            optind: 0,
        };
        let mut getopt = Getopt::new(OPTIONS);
        while let Ok(Taken::Switch(code, value)) = getopt.next(argv) {
            let value = value.unwrap_or_default();
            match code {
                b'a' => {
                    settled.param_error.get_or_insert(NO_SHELL);
                }
                b'f' if settled.mode == Mode::Direct => {
                    settled.param_error.get_or_insert(CONFLICT);
                }
                b'f' if settled.program.is_some() => {
                    settled
                        .param_error
                        .get_or_insert("You can use -f only once.\n");
                }
                b'f' => settled.program = Some(Program::File(PathBuf::from(os(&value)))),
                b'l' if settled.mode == Mode::Standard => settled.mode = Mode::Lint,
                b'r' if settled.mode == Mode::Direct => {
                    settled
                        .param_error
                        .get_or_insert("You can use -r only once.\n");
                }
                b'r' if settled.mode != Mode::Standard => {
                    settled.param_error.get_or_insert(CONFLICT);
                }
                b'r' => {
                    settled.mode = Mode::Direct;
                    settled.program = Some(Program::Direct(value));
                }
                b's' if settled.mode == Mode::Direct => {
                    settled
                        .param_error
                        .get_or_insert("Source highlighting only works for files.\n");
                }
                b's' => settled.mode = Mode::Highlight,
                b'w' if settled.mode == Mode::Direct => {
                    settled
                        .param_error
                        .get_or_insert("Source stripping only works for files.\n");
                }
                b'w' => settled.mode = Mode::Strip,
                b'B' | b'E' | b'F' | b'R' | 10..=16 => return Err(usage(1)),
                _ => {}
            }
        }
        settled.optind = getopt.optind;
        Ok(settled)
    }
}

/// The programs `-l` lints: the one named, or stdin where none was, and
/// then each word left on the line up to `--`, as the repeat at `out:`
/// takes them (`php_cli.c:1176-1179`).
fn lint_programs(first: Option<Program>, rest: &mut std::slice::Iter<'_, Vec<u8>>) -> Vec<Program> {
    let mut programs = vec![first.unwrap_or(Program::Stdin)];
    programs.extend(bare_words(rest));
    programs
}

/// Each word left on the line up to `--`, as a file to lint.
fn bare_words(rest: &mut std::slice::Iter<'_, Vec<u8>>) -> Vec<Program> {
    rest.take_while(|word| word.as_slice() != b"--")
        .map(|word| Program::File(PathBuf::from(os(word))))
        .collect()
}

/// `php_cli_usage` (`Z/sapi/cli/php_cli.c:457-515`) with `php` for the
/// program's name, on stdout.
#[must_use]
pub fn usage_text() -> String {
    String::from(
        "Usage: php [options] [-f] <file> [--] [args...]\n\
         \x20  php [options] -r <code> [--] [args...]\n\
         \x20  php [options] [-B <begin_code>] -R <code> [-E <end_code>] [--] [args...]\n\
         \x20  php [options] [-B <begin_code>] -F <file> [-E <end_code>] [--] [args...]\n\
         \x20  php [options] -S <addr>:<port> [-t docroot] [router]\n\
         \x20  php [options] -- [args...]\n\
         \x20  php [options] -a\n\
         \n\
         \x20 -a               Run as interactive shell (requires readline extension)\n\
         \x20 -c <path>|<file> Look for php.ini file in this directory\n\
         \x20 -n               No configuration (ini) files will be used\n\
         \x20 -d foo[=bar]     Define INI entry foo with value 'bar'\n\
         \x20 -e               Generate extended information for debugger/profiler\n\
         \x20 -f <file>        Parse and execute <file>.\n\
         \x20 -h               This help\n\
         \x20 -i               PHP information\n\
         \x20 -l               Syntax check only (lint)\n\
         \x20 -m               Show compiled in modules\n\
         \x20 -r <code>        Run PHP <code> without using script tags <?..?>\n\
         \x20 -B <begin_code>  Run PHP <begin_code> before processing input lines\n\
         \x20 -R <code>        Run PHP <code> for every input line\n\
         \x20 -F <file>        Parse and execute <file> for every input line\n\
         \x20 -E <end_code>    Run PHP <end_code> after processing all input lines\n\
         \x20 -H               Hide any passed arguments from external tools.\n\
         \x20 -S <addr>:<port> Run with built-in web server.\n\
         \x20 -t <docroot>     Specify document root <docroot> for built-in web server.\n\
         \x20 -s               Output HTML syntax highlighted source.\n\
         \x20 -v               Version number\n\
         \x20 -w               Output source with stripped comments and whitespace.\n\
         \n\
         \x20 args...          Arguments passed to script. Use -- args when first argument\n\
         \x20                  starts with - or script is read from stdin\n\
         \n\
         \x20 --ini            Show configuration file names\n\
         \x20 --ini=diff       Show INI entries that differ from the built-in default\n\
         \n\
         \x20 --rf <name>      Show information about function <name>.\n\
         \x20 --rc <name>      Show information about class <name>.\n\
         \x20 --re <name>      Show information about extension <name>.\n\
         \x20 --rz <name>      Show information about Zend extension <name>.\n\
         \x20 --ri <name>      Show configuration for extension <name>.\n\
         \n\
         \x20 --repeat <count> Repeat script execution <count> times.\n\
         \x20                  For internal purposes only.\n\
         \n",
    )
}

/// `php_cli_usage` on stdout with the exit code given: 1 for a switch
/// php does not know (`PHP_GETOPT_INVALID_ARG`) and for one this host
/// refuses.
fn usage(exit_code: i32) -> Refused {
    Refused {
        stdout: usage_text(),
        stderr: String::new(),
        exit_code,
    }
}

/// `php_get_version` for this host: `PHP <version> (cli) (built: <date>)
/// (NTS)`, the one line upstream's test runner and a person read, the
/// date stamped by this crate's `build.rs` as `__DATE__ " " __TIME__`
/// stamps php's. php follows it with its copyright lines and the Zend
/// engine's; this engine is not php and prints neither.
#[must_use]
pub fn version() -> String {
    format!(
        "PHP {PHP_VERSION} ({NAME}) (built: {}) (NTS)\n",
        php_sapi::build_date(env!("PHP_BUILD_EPOCH"))
    )
}

/// `php_print_info` as text, as far as `run-tests.php` reads it: the
/// head, the version, the host and that no ini file was read.
#[must_use]
pub fn info() -> String {
    format!(
        "phpinfo()\nPHP Version => {PHP_VERSION}\n\nServer API => Command Line Interface\nLoaded Configuration File => (none)\n"
    )
}

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
    fn write(&mut self, bytes: &[u8]) {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(bytes);
        let _ = stdout.flush();
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

/// The request description for `program` as `do_cli` builds it: `argv`
/// with `php_self` first, which is the file's name, or `Standard input
/// code` where no file was named, `-r` included; the process's
/// environment and its working directory; the file's bytes are the
/// engine's to read by name; `-r`'s code is served under the name
/// `Command line code` with php's open tag before it on the same line,
/// as `zend_eval_string` scans it in `ST_IN_SCRIPTING`; stdin is read
/// whole and served as `Standard input code`. A file that cannot be
/// opened is `cli_seek_file_begin`'s line on stderr and exit 1, before a
/// request begins. The request is handed the process's standard handles
/// ([`FileHandles`]), which the engine registers as `STDIN`, `STDOUT`
/// and `STDERR` over copies of fds 0, 1 and 2, as `do_cli` registers
/// them for a file, `-r`'s code and stdin alike.
///
/// What php's `-R`-family reads stdin lazily for is not here: nothing in
/// the engine reads the request's body yet (design §4.3 seam 4, the
/// body stream), so stdin is read only where it is the program.
pub fn describe(program: &Program, arguments: &[OsString]) -> Result<RequestDescription, i32> {
    let (php_self, script, source): (Vec<u8>, Vec<u8>, Option<Vec<u8>>) = match program {
        Program::File(path) => {
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
        Program::Direct(code) => {
            let mut source = b"<?php ".to_vec();
            source.extend_from_slice(code);
            (
                b"Standard input code".to_vec(),
                b"Command line code".to_vec(),
                Some(source),
            )
        }
        Program::Stdin => {
            let mut source = Vec::new();
            let _ = std::io::stdin().lock().read_to_end(&mut source);
            let name = b"Standard input code".to_vec();
            (name.clone(), name, Some(source))
        }
    };
    let file = match program {
        Program::File(_) => script.clone(),
        Program::Direct(_) | Program::Stdin => Vec::new(),
    };
    let server = server_variables(&php_self, &file);
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
        script_name: Some(script),
        source,
        file_handles: Some(process_handles()),
        ..RequestDescription::default()
    })
}

/// `sapi_cli_register_variables` (`Z/sapi/cli/php_cli.c:316`) past the
/// environment, which the engine imports first for every host:
/// `PHP_SELF` and `SCRIPT_NAME` are `php_self`, the file's name or
/// `Standard input code`; `SCRIPT_FILENAME` and `PATH_TRANSLATED` are the
/// file's name, empty for `-r` and stdin; `DOCUMENT_ROOT` is empty.
fn server_variables(php_self: &[u8], file: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![
        (b"PHP_SELF".to_vec(), php_self.to_vec()),
        (b"SCRIPT_NAME".to_vec(), php_self.to_vec()),
        (b"SCRIPT_FILENAME".to_vec(), file.to_vec()),
        (b"PATH_TRANSLATED".to_vec(), file.to_vec()),
        (b"DOCUMENT_ROOT".to_vec(), Vec::new()),
    ]
}

/// The process's fds 0, 1 and 2 as the engine is handed them, each a
/// `dup(2)` of the host's, so the request's `fstat` and `ftruncate` ask
/// of the fds php's `STDIO` kind asks of; a closed one is `None`, which
/// both answer as `fstat(2)` answers `EBADF`.
fn process_handles() -> FileHandles {
    use std::os::fd::AsFd;
    FileHandles::new(
        std::io::stdin().as_fd().try_clone_to_owned().ok(),
        std::io::stdout().as_fd().try_clone_to_owned().ok(),
        std::io::stderr().as_fd().try_clone_to_owned().ok(),
    )
}

/// Serve `program` on `engine` as `do_cli` serves it and hand back the
/// exit code the process ends with; see [`describe`].
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
    match describe(program, arguments) {
        Ok(description) => {
            let mut sink = Process;
            dump(engine, program, &description, &mut sink);
            engine.serve(description, &mut sink).exit_code
        }
        Err(code) => code,
    }
}

/// php's op arrays as the compiler left them, on the second stream,
/// where `opcache.opt_debug_level` asks for them
/// ([`Engine::dump_before_optimizer`], design §6.1): opcache dumps them
/// as it persists the unit, which is before the program runs and
/// before `-l`'s own sentence, and this host asks for them in the one
/// moment php has both. `-r`'s code is the program php dumps nothing
/// for, its op array being `ZEND_EVAL_CODE`
/// (`Z/Zend/Optimizer/zend_optimizer.c:1056`); a file and what the host
/// read from stdin are dumped alike.
fn dump(
    engine: &Engine,
    program: &Program,
    description: &RequestDescription,
    sink: &mut dyn OutputSink,
) {
    if matches!(program, Program::Direct(_)) {
        return;
    }
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
    match describe(program, arguments) {
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
    let mut description = match describe(program, &[]) {
        Ok(description) => description,
        Err(code) => return code,
    };
    // `do_cli`'s `PHP_CLI_MODE_LINT` arm calls no
    // `cli_register_file_handles`.
    description.file_handles = None;
    let name = description.script_name.clone().unwrap_or_default();
    dump(engine, program, &description, sink);
    let served = engine.lint(description, sink);
    let mut line = if served.exit_code == 0 {
        b"No syntax errors detected in ".to_vec()
    } else {
        b"Errors parsing ".to_vec()
    };
    line.extend_from_slice(&name);
    line.push(b'\n');
    sink.write(&line);
    sink.flush();
    if served.exit_code == 0 { 0 } else { 255 }
}

/// php's CLI `main`: the command line read, what it asks for served on
/// one engine of the bundle's enabled set with the bundle's tier bound,
/// and the process ending with the code the request ended with. The ini
/// the line carries is laid over the engine's defaults before it is
/// built, as php's `-d` reaches `php_ini_builder` before
/// `php_module_startup`; the version needs no engine, and the listing
/// one served nothing.
pub fn main() -> ExitCode {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    let options = match Options::parse(&argv) {
        Ok(options) => options,
        Err(refused) => {
            let _ = std::io::stderr()
                .lock()
                .write_all(refused.stderr.as_bytes());
            let _ = std::io::stdout()
                .lock()
                .write_all(refused.stdout.as_bytes());
            return php_sapi::exit_code(refused.exit_code);
        }
    };
    let status = match &options.ask {
        Ask::Usage => print(&usage_text()),
        Ask::Version => print(&version()),
        Ask::Info => print(&info()),
        Ask::Extensions => print(&php_sapi::modules(&engine(&options.ini))),
        Ask::Serve(program, arguments) => serve(&engine(&options.ini), program, arguments),
        Ask::Highlight(program, arguments) => highlight(&engine(&options.ini), program, arguments),
        Ask::Strip(program, arguments) => strip(&engine(&options.ini), program, arguments),
        // `do_cli`'s lint loop: the status starts at 0, a lint that fails
        // sets 255 and a file that cannot be opened sets 1, each as it
        // comes, and one that passes changes nothing.
        Ask::Lint { programs, refused } => {
            let mut status = 0;
            if let Some(sentence) = refused {
                let _ = std::io::stdout().lock().write_all(sentence.as_bytes());
                status = 1;
            }
            let engine = engine(&options.ini);
            let mut sink = Process;
            for program in programs {
                match lint(&engine, program, &mut sink) {
                    0 => {}
                    ended => status = ended,
                }
            }
            status
        }
    };
    php_sapi::exit_code(status)
}

/// `text` on stdout, and the status 0.
fn print(text: &str) -> i32 {
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
    0
}

/// A fresh engine of `php_bundle::enabled()`, every extension in startup
/// order and `php_bundle::Tier` bound, the shebang line of every file
/// passed over as `do_cli` sets `CG(skip_shebang)`, started over the
/// line's `ini` ([`php_sapi::start`]).
fn engine(ini: &[(String, String)]) -> Engine {
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    let builder = Engine::builder()
        .extensions(&extensions)
        .tier::<php_bundle::Tier>()
        .skip_shebang();
    php_sapi::start(builder, ini, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Result<Options, Refused> {
        let argv: Vec<OsString> = words.iter().map(OsString::from).collect();
        Options::parse(&argv)
    }

    fn ask(words: &[&str]) -> Result<Ask, (String, String, i32)> {
        parse(words)
            .map(|o| o.ask)
            .map_err(|r| (r.stdout, r.stderr, r.exit_code))
    }

    /// Whether the refusal is the usage on stdout, its stderr and its exit.
    fn refused(words: &[&str]) -> (bool, String, i32) {
        match parse(words) {
            Ok(options) => (false, format!("{:?}", options.ask), 0),
            Err(r) => (r.stdout == usage_text(), r.stderr, r.exit_code),
        }
    }

    fn file(name: &str) -> Program {
        Program::File(PathBuf::from(name))
    }

    fn direct(code: &str) -> Program {
        Program::Direct(code.as_bytes().to_vec())
    }

    #[test]
    fn a_bare_word_is_the_file_and_the_rest_its_arguments() {
        let options = parse(&["-n", "-d", "precision=10", "-dfoo", "a.php", "-x", "y"]);
        assert_eq!(
            options,
            Ok(Options {
                ini_ignore: true,
                ini: vec![
                    ("precision".to_owned(), "10".to_owned()),
                    ("foo".to_owned(), "1".to_owned())
                ],
                ask: Ask::Serve(
                    file("a.php"),
                    vec![OsString::from("-x"), OsString::from("y")]
                ),
            })
        );
    }

    #[test]
    fn stdin_is_the_program_when_none_is_named() {
        assert_eq!(ask(&["-n"]), Ok(Ask::Serve(Program::Stdin, Vec::new())));
    }

    #[test]
    fn direct_code_and_a_file_conflict_in_phps_words() {
        assert_eq!(
            parse(&["-r", "echo 1;", "-f", "a.php"]),
            Err(Refused {
                stdout: CONFLICT.to_owned(),
                stderr: String::new(),
                exit_code: 1
            })
        );
        assert_eq!(
            ask(&["-r", "echo 1;", "-r", "echo 2;"]),
            Err(("You can use -r only once.\n".to_owned(), String::new(), 1))
        );
    }

    #[test]
    fn a_word_after_the_dashes_is_an_argument_of_direct_code() {
        assert_eq!(
            ask(&["-r", "echo 1;", "--", "-n"]),
            Ok(Ask::Serve(direct("echo 1;"), vec![OsString::from("-n")]))
        );
    }

    #[test]
    fn the_version_and_the_listing_end_the_reading() {
        assert_eq!(ask(&["-v", "a.php"]), Ok(Ask::Version));
        assert_eq!(ask(&["--version"]), Ok(Ask::Version));
        assert_eq!(ask(&["-n", "-m", "x.php"]), Ok(Ask::Extensions));
        assert_eq!(ask(&["-i"]), Ok(Ask::Info));
        // `-rv` is `-r` with the code `v`.
        assert_eq!(ask(&["-rv"]), Ok(Ask::Serve(direct("v"), Vec::new())));
    }

    #[test]
    fn the_usage_and_a_switch_php_does_not_know_are_read_in_order() {
        assert_eq!(ask(&["-n", "-h", "-x"]), Ok(Ask::Usage));
        assert_eq!(ask(&["-n", "-?"]), Ok(Ask::Usage));
        assert_eq!(ask(&["-n", "-v", "-x"]), Ok(Ask::Version));
        assert_eq!(ask(&["-n", "--version", "-x"]), Ok(Ask::Version));
        assert_eq!(
            refused(&["-n", "-x", "-v"]),
            (
                true,
                "Error in argument 2, char 2: option not found x\n".to_owned(),
                1
            )
        );
    }

    /// `php_getopt`'s line, character for character as the oracle
    /// prints it at 8.5.9, the `optchr` it keeps across words and all.
    #[test]
    fn php_getopt_s_line_counts_as_php_counts() {
        let line = |words: &[&str]| refused(words).1;
        assert_eq!(
            line(&["-x"]),
            "Error in argument 1, char 2: option not found x\n"
        );
        assert_eq!(
            line(&["-nx"]),
            "Error in argument 1, char 3: option not found x\n"
        );
        assert_eq!(
            line(&["-n", "-nx"]),
            "Error in argument 2, char 3: option not found x\n"
        );
        assert_eq!(
            line(&["-d"]),
            "Error in argument 1, char 2: no argument for option d\n"
        );
        assert_eq!(
            line(&["-nd"]),
            "Error in argument 1, char 3: no argument for option d\n"
        );
        assert_eq!(
            line(&["-n", "-c"]),
            "Error in argument 2, char 2: no argument for option c\n"
        );
        assert_eq!(
            line(&["--foo"]),
            "Error in argument 1, char 1: no argument for option -\n"
        );
        assert_eq!(
            line(&["--foo=bar"]),
            "Error in argument 1, char 1: no argument for option -\n"
        );
        assert_eq!(
            line(&["-n", "--foo"]),
            "Error in argument 2, char 2: no argument for option -\n"
        );
        assert_eq!(
            line(&["-nn", "--foo"]),
            "Error in argument 2, char 3: no argument for option f\n"
        );
        assert_eq!(
            line(&["-n", "-dx", "--foo"]),
            "Error in argument 3, char 2: no argument for option -\n"
        );
        assert_eq!(line(&["-:"]), "Error in argument 1, char 2: : in flags\n");
        assert_eq!(
            line(&["-n", "-x", "-d"]),
            "Error in argument 2, char 2: option not found x\n"
        );
    }

    #[test]
    fn the_switches_taken_and_changing_nothing() {
        let served = Ok(Ask::Serve(direct("echo 1;"), Vec::new()));
        assert_eq!(ask(&["-n", "-c", "/tmp", "-r", "echo 1;"]), served);
        assert_eq!(ask(&["-n", "-e", "-r", "echo 1;"]), served);
        assert_eq!(ask(&["-n", "-H", "-r", "echo 1;"]), served);
        assert_eq!(ask(&["-n", "-t", "x", "-r", "echo 1;"]), served);
        assert_eq!(ask(&["-n", "-C", "-q", "-r", "echo 1;"]), served);
        assert_eq!(ask(&["-n", "--no-php-ini", "--run", "echo 1;"]), served);
        assert_eq!(ask(&["-n", "--run=echo 1;"]), served);
        assert_eq!(
            ask(&["-n", "-e"]),
            Ok(Ask::Serve(Program::Stdin, Vec::new()))
        );
    }

    #[test]
    fn the_shell_is_phps_sentence_for_a_build_without_readline() {
        assert_eq!(
            ask(&["-n", "-a"]),
            Err((NO_SHELL.to_owned(), String::new(), 1))
        );
        assert_eq!(ask(&["-n", "-a", "-v"]), Ok(Ask::Version));
        assert_eq!(
            ask(&["-n", "-r", "echo 1;", "-a"]),
            Err((NO_SHELL.to_owned(), String::new(), 1))
        );
    }

    #[test]
    fn the_switches_refused_are_the_usage_and_exit_1() {
        for words in [
            &["-n", "-B", "x"][..],
            &["-n", "-R", "x"],
            &["-n", "-F", "a.php"],
            &["-n", "-E", "x"],
            &["-n", "-S", "localhost:80"],
            &["-n", "--rf", "strlen"],
            &["-n", "--rc", "Foo"],
            &["-n", "--re", "json"],
            &["-n", "--rz", "x"],
            &["-n", "--ri", "json"],
            &["-n", "--ini"],
            &["-n", "--ini=diff"],
            &["-n", "--repeat", "2", "-r", "echo 1;"],
        ] {
            assert_eq!(refused(words), (true, String::new(), 1), "{words:?}");
        }
    }

    #[test]
    fn highlight_and_strip_take_a_file_or_stdin_and_the_last_of_the_two_wins() {
        let file = || Program::File(PathBuf::from("a.php"));
        assert_eq!(
            ask(&["-n", "-s", "a.php"]),
            Ok(Ask::Highlight(file(), Vec::new()))
        );
        assert_eq!(
            ask(&["-n", "-w", "-f", "a.php"]),
            Ok(Ask::Strip(file(), Vec::new()))
        );
        assert_eq!(
            ask(&["-n", "-s"]),
            Ok(Ask::Highlight(Program::Stdin, Vec::new()))
        );
        assert_eq!(
            ask(&["-n", "-s", "-w", "a.php"]),
            Ok(Ask::Strip(file(), Vec::new()))
        );
        assert_eq!(
            ask(&["-n", "-w", "-s", "a.php"]),
            Ok(Ask::Highlight(file(), Vec::new()))
        );
    }

    #[test]
    fn highlight_and_strip_after_r_are_phps_sentences_and_r_after_them_the_conflict() {
        let sentence = |text: &str| Err((text.to_owned(), String::new(), 1));
        assert_eq!(
            ask(&["-n", "-r", "echo 1;", "-s"]),
            sentence("Source highlighting only works for files.\n")
        );
        assert_eq!(
            ask(&["-n", "-r", "echo 1;", "-w"]),
            sentence("Source stripping only works for files.\n")
        );
        assert_eq!(ask(&["-n", "-s", "-r", "echo 1;"]), sentence(CONFLICT));
        assert_eq!(ask(&["-n", "-w", "-r", "echo 1;"]), sentence(CONFLICT));
    }

    #[test]
    fn lint_walks_the_words_left_on_the_line_up_to_the_dashes() {
        let lint = |programs: Vec<Program>| {
            Ok(Ask::Lint {
                programs,
                refused: None,
            })
        };
        assert_eq!(ask(&["-n", "-l", "a.php"]), lint(vec![file("a.php")]));
        assert_eq!(ask(&["-nl", "a.php"]), lint(vec![file("a.php")]));
        assert_eq!(ask(&["-n", "-l", "-l", "a.php"]), lint(vec![file("a.php")]));
        assert_eq!(ask(&["-n", "-l", "-f", "a.php"]), lint(vec![file("a.php")]));
        assert_eq!(ask(&["-n", "-f", "a.php", "-l"]), lint(vec![file("a.php")]));
        assert_eq!(ask(&["-n", "-l"]), lint(vec![Program::Stdin]));
        assert_eq!(
            ask(&["-n", "-l", "a.php", "b.php", "-x"]),
            lint(vec![file("a.php"), file("b.php"), file("-x")])
        );
        assert_eq!(
            ask(&["-n", "-l", "-f", "a.php", "b.php"]),
            lint(vec![file("a.php"), file("b.php")])
        );
        assert_eq!(
            ask(&["-n", "-l", "a.php", "b.php", "--", "c.php"]),
            lint(vec![file("a.php"), file("b.php")])
        );
        assert_eq!(
            ask(&["-n", "-l", "--", "b.php"]),
            lint(vec![Program::Stdin])
        );
        // `-l` after `-r` is not read; `-r` after `-l` is the conflict.
        assert_eq!(
            ask(&["-n", "-r", "echo 1;", "-l"]),
            Ok(Ask::Serve(direct("echo 1;"), Vec::new()))
        );
        assert_eq!(
            ask(&["-n", "-l", "-r", "echo 1;"]),
            Err((CONFLICT.to_owned(), String::new(), 1))
        );
    }

    #[test]
    fn a_refusal_under_lint_lints_the_bare_words_all_the_same() {
        assert_eq!(
            ask(&["-n", "-l", "-a", "a.php"]),
            Ok(Ask::Lint {
                programs: vec![file("a.php")],
                refused: Some(NO_SHELL.to_owned())
            })
        );
        assert_eq!(
            ask(&["-n", "-a", "-l", "a.php"]),
            Ok(Ask::Lint {
                programs: vec![file("a.php")],
                refused: Some(NO_SHELL.to_owned())
            })
        );
        // The file `-f` named is not among them: `goto err` skips it.
        assert_eq!(
            ask(&["-n", "-l", "-a", "-f", "a.php"]),
            Err((NO_SHELL.to_owned(), String::new(), 1))
        );
    }

    /// One sink over both streams, in the order the request wrote them.
    #[derive(Default)]
    struct Capture(Vec<u8>);

    impl OutputSink for Capture {
        fn write(&mut self, bytes: &[u8]) {
            self.0.extend_from_slice(bytes);
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
    /// built from them as the binary builds it.
    fn served_under(switches: &[&str], code: &str) -> (String, String) {
        let mut words = vec!["-n"];
        for switch in switches {
            words.extend(["-d", switch]);
        }
        words.extend(["-r", code]);
        let Ok(options) = parse(&words) else {
            return (String::from("refused"), String::new());
        };
        let mut builder = Engine::builder();
        for (name, value) in &options.ini {
            builder = builder.ini(name, value);
        }
        let engine = builder.build();
        let Ask::Serve(program, arguments) = &options.ask else {
            return (String::from("not served"), String::new());
        };
        let Ok(description) = describe(program, arguments) else {
            return (String::from("not described"), String::new());
        };
        let mut sink = Capture::default();
        engine.serve(description, &mut sink);
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
        let engine = Engine::builder().build();
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
