//! The command line as php's CLI reads it (`Z/sapi/cli/php_cli.c`):
//! `main`'s pass over `argv` through `php_getopt`, which takes the ini
//! switches, picks the host (`-S` is the built-in server) and ends early
//! on the usage, and `do_cli`'s two passes, the first answering `-i`,
//! `-v` and `-m` and the second settling [`Mode`], php's
//! `PHP_CLI_MODE_*`, once. What the line asks for is then one [`Ask`],
//! decoded here and nowhere else.

use std::ffi::OsString;
use std::path::PathBuf;

use php_sapi::c_number::atoi;
use php_sapi::getopt::{Getopt, Opt, Taken, getopt_line, opt, os};
use php_sapi::ini::{Search, define};

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

/// `PHP_CLI_MODE_PROCESS_STDIN`'s four switches: `-B`'s code before the
/// first line of stdin, `-R`'s code or `-F`'s file for each line, and
/// `-E`'s code after the last.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lines {
    pub begin: Option<Vec<u8>>,
    pub each: Option<Each>,
    pub end: Option<Vec<u8>>,
}

/// What runs for each line of stdin: `-R`'s code, or `-F`'s file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Each {
    Code(Vec<u8>),
    File(PathBuf),
}

/// What `do_cli` prints in a request of its own with no program:
/// php's `PHP_CLI_MODE_REFLECTION_*` and `PHP_CLI_MODE_SHOW_INI_*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// `--rf`: `ReflectionFunction`, or `ReflectionMethod` for a name
    /// with `::` in it.
    Function(Vec<u8>),
    /// `--rc`: `ReflectionClass`.
    Class(Vec<u8>),
    /// `--re`: `ReflectionExtension`.
    Extension(Vec<u8>),
    /// `--rz`: `ReflectionZendExtension`.
    ZendExtension(Vec<u8>),
    /// `--ri`: `php_info_print_module`.
    ExtensionInfo(Vec<u8>),
    /// `--ini`: the configuration files read.
    IniFiles,
}

/// php's `PHP_CLI_MODE_*` (`Z/sapi/cli/cli.h`), what `do_cli`'s second
/// pass settles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Standard,
    Highlight,
    Lint,
    Strip,
    Direct,
    ProcessStdin,
    Report,
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
    /// `-B`, `-R`, `-F` and `-E` over stdin's lines, with the words after
    /// the switches as `$argv[1..]`.
    Lines(Lines, Vec<OsString>),
    /// A report printed in a request of its own; the file a bare word
    /// named is opened first, as `do_repeat` opens it, and not served.
    Report(Report, Option<PathBuf>),
    /// `-S`: the built-in web server.
    Server(Server),
    /// `-a`: the interactive shell, the file a bare word or `-f` named
    /// opened first and not served, as `do_repeat` opens it, and the
    /// words after it as `$argv[1..]`.
    Shell(Option<PathBuf>, Vec<OsString>),
}

/// What `do_cli_server` reads off the line: `-S`'s address, `-t`'s
/// document root, the router after the switches, and how many `-q`
/// lowered the log level from 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub address: Vec<u8>,
    pub document_root: Option<PathBuf>,
    pub router: Option<PathBuf>,
    pub quiet: u8,
}

/// php's modules started for the CLI under the line's [`Startup`], as
/// `main` starts them between its own pass and `do_cli`'s
/// (`Z/sapi/cli/php_cli.c:1341`), answering whether the start filled
/// `cli_shell_callbacks.cli_shell_run`, which `do_cli` asks of `-a`.
pub type Start<'a> = dyn Fn(&Startup) -> bool + 'a;

/// What `main`'s pass reads off the line for php's modules to start
/// under, before `do_cli` reads the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Startup {
    /// `-c` and `-n`, which steer the search for php.ini.
    pub ini_search: Search,
    /// Each `-d name=value` split at its first `=`, in the order given;
    /// a `-d name` is `name=1`, as `php_ini_builder_define` writes it.
    /// The value is the text the engine's builder writes as the define's
    /// line and reads with the INI grammar (`EngineBuilder::ini`), so
    /// `UTC;` stands here and is `UTC` once read; both are the
    /// argument's bytes, as php's argv is.
    pub ini: Vec<(Vec<u8>, Vec<u8>)>,
    /// `-e` taken, `main`'s `use_extended_info`: the engine compiles
    /// with extended info (`Z/sapi/cli/php_cli.c:1317`, `:1366`).
    pub extended_info: bool,
}

/// The command line, read: what the engine starts under and what is
/// asked of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub startup: Startup,
    /// `--repeat <count>`: the request served this many times, php's
    /// `num_repeats`, `atoi` of the value.
    pub repeat: i64,
    /// `-a` taken, which `do_cli` answers with `Interactive shell` and
    /// a blank line on stdout before it serves whatever the mode is.
    pub interactive: bool,
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
pub(crate) const CONFLICT: &str = "Either execute direct code, process stdin or use a file.\n";

/// `do_cli`'s sentence for `-a` when `cli_shell_callbacks.cli_shell_run`
/// is unset, which it is with no readline extension linked.
pub(crate) const NO_SHELL: &str = "Interactive shell (-a) requires the readline extension.\n";

/// `OPTIONS`, `php_cli.c:124-165`, row for row; the `-` row that ends
/// php's table is the slice's end.
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
];

/// What `do_cli`'s second pass settled: the mode, the program `-f` or
/// `-r` named, the process-stdin switches, the report, the last
/// `param_error` (each assignment overwrites the one before), the repeat
/// count and where the switches ended.
struct Settled {
    mode: Mode,
    script_file: Option<Vec<u8>>,
    direct: Option<Vec<u8>>,
    lines: Lines,
    report: Option<Report>,
    param_error: Option<&'static str>,
    /// `-a` taken, php's `interactive`.
    interactive: bool,
    repeat: i64,
    /// A switch this host refuses with the usage and exit 1.
    usage: bool,
    optind: usize,
}

/// What `main`'s pass ends on: the usage or an early answer, the server,
/// or `do_cli`.
enum Host {
    Early(Ask),
    Server,
    Cli,
}

impl Options {
    /// Read `argv[1..]` as php's `main` and `do_cli` read it: `main`'s
    /// pass takes the ini switches and ends on `-h`, on a switch php
    /// does not know, or at the first of `-i`, `-v` and `-m`, which
    /// `do_cli`'s first loop then answers; a `-S` before any of them
    /// makes the host the built-in server, which reads the line again
    /// for itself; `do_cli`'s second loop settles what is being done
    /// with the line, then the program, then what is left as its
    /// arguments. `--` ends the switches; a word after it is the program
    /// unless one was named already. Where `do_cli` reads the line, php's
    /// modules have started: `start` starts them under the line's
    /// [`Startup`] before the second loop, once, and its answer is what
    /// `-a` asks.
    pub fn parse(argv: &[OsString], start: &Start<'_>) -> Result<Options, Refused> {
        let argv: Vec<Vec<u8>> = argv
            .iter()
            .map(|word| word.as_encoded_bytes().to_vec())
            .collect();
        let mut options = Options {
            startup: Startup::default(),
            repeat: 1,
            interactive: false,
            ask: Ask::Serve(Program::Stdin, Vec::new()),
        };
        match options.main_pass(&argv)? {
            Host::Early(ask) => {
                options.ask = ask;
                return Ok(options);
            }
            Host::Server => {
                options.ask = Ask::Server(server_pass(&argv));
                return Ok(options);
            }
            Host::Cli => {}
        }
        let settled = mode_pass(&argv, start(&options.startup));
        if settled.usage {
            return Err(usage(1));
        }
        options.repeat = settled.repeat;
        options.interactive = settled.interactive && settled.param_error.is_none();
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
        // `do_repeat`: a bare word is the script where none was named,
        // the mode reads a file, and the switches did not end on `--`.
        let mut script_file = settled.script_file;
        if script_file.is_none()
            && !matches!(settled.mode, Mode::Direct | Mode::ProcessStdin)
            && !after_dashes
        {
            script_file = rest.next().cloned();
        }
        let file = script_file.map(|word| PathBuf::from(os(&word)));
        let program = match (&settled.direct, &file) {
            (Some(code), _) => Program::Direct(code.clone()),
            (None, Some(file)) => Program::File(file.clone()),
            (None, None) => Program::Stdin,
        };
        options.ask = match settled.mode {
            Mode::Lint => {
                // php lints stdin for each word left after `--`, without
                // end; this host lints it once.
                let mut programs = vec![program];
                if !after_dashes {
                    programs.extend(bare_words(&mut rest));
                }
                Ask::Lint {
                    programs,
                    refused: None,
                }
            }
            Mode::Report => Ask::Report(settled.report.unwrap_or(Report::IniFiles), file),
            Mode::ProcessStdin => {
                let mut lines = settled.lines;
                if lines.each.is_none()
                    && let Some(file) = file
                {
                    lines.each = Some(Each::File(file));
                }
                Ask::Lines(lines, rest.map(|word| os(word)).collect())
            }
            Mode::Highlight => Ask::Highlight(program, rest.map(|word| os(word)).collect()),
            Mode::Strip => Ask::Strip(program, rest.map(|word| os(word)).collect()),
            Mode::Standard if settled.interactive => {
                Ask::Shell(file, rest.map(|word| os(word)).collect())
            }
            Mode::Standard | Mode::Direct => {
                Ask::Serve(program, rest.map(|word| os(word)).collect())
            }
        };
        Ok(options)
    }

    /// `main`'s loop over the line (`php_cli.c:1290-1325`), `show_err`
    /// set: the ini switches onto `self`, `-S` choosing the server's
    /// module, and what ends it early; `-i`, `-v` and `-m` are the CLI's
    /// whatever came before them.
    fn main_pass(&mut self, argv: &[Vec<u8>]) -> Result<Host, Refused> {
        let mut getopt = Getopt::new(OPTIONS, 0);
        let mut server = false;
        loop {
            let ask = match getopt.next(argv) {
                Ok(Taken::End) => {
                    return Ok(if server { Host::Server } else { Host::Cli });
                }
                Err((argument, at, error)) => {
                    return Err(Refused {
                        stderr: getopt_line(argv, argument, at, error),
                        ..usage(1)
                    });
                }
                Ok(Taken::Switch { code, value, .. }) => match code {
                    b'n' => {
                        self.startup.ini_search.ignore = true;
                        continue;
                    }
                    b'c' => {
                        self.startup.ini_search.path = value;
                        continue;
                    }
                    b'd' => {
                        let define = define(&value.unwrap_or_default());
                        self.startup.ini.push(define);
                        continue;
                    }
                    b'S' => {
                        server = true;
                        continue;
                    }
                    b'e' => {
                        self.startup.extended_info = true;
                        continue;
                    }
                    b'h' | b'?' => Ask::Usage,
                    b'i' => Ask::Info,
                    b'v' => Ask::Version,
                    b'm' => Ask::Extensions,
                    _ => continue,
                },
            };
            return Ok(Host::Early(ask));
        }
    }
}

impl Settled {
    /// `-B`, `-R`, `-F` and `-E` (`php_cli.c:700-790`): each makes the
    /// mode `PHP_CLI_MODE_PROCESS_STDIN` from the standard one, and one
    /// that repeats is php's sentence, `-R` and `-F` counting as one.
    fn process_stdin(&mut self, code: u8, value: Vec<u8>) {
        let taken = match code {
            b'B' => self.lines.begin.is_some(),
            b'E' => self.lines.end.is_some(),
            _ => self.lines.each.is_some() || self.script_file.is_some(),
        };
        if self.mode == Mode::ProcessStdin {
            if taken {
                self.param_error = Some(match code {
                    b'B' => "You can use -B only once.\n",
                    b'E' => "You can use -E only once.\n",
                    _ => "You can use -R or -F only once.\n",
                });
                return;
            }
        } else if self.mode != Mode::Standard {
            self.param_error = Some(CONFLICT);
            return;
        }
        self.mode = Mode::ProcessStdin;
        match code {
            b'B' => self.lines.begin = Some(value),
            b'E' => self.lines.end = Some(value),
            b'F' => self.script_file = Some(value),
            _ => self.lines.each = Some(Each::Code(value)),
        }
    }
}

/// `do_cli`'s second loop (`php_cli.c:666-832`): the mode, the program
/// and the rest of what the switches name, each switch read as php reads
/// it, `-a` under whether the started modules have a `shell`. A
/// `param_error` does not end the loop: the last one set is the one
/// `do_cli` puts.
fn mode_pass(argv: &[Vec<u8>], shell: bool) -> Settled {
    let mut settled = Settled {
        mode: Mode::Standard,
        script_file: None,
        direct: None,
        lines: Lines::default(),
        report: None,
        param_error: None,
        interactive: false,
        repeat: 1,
        usage: false,
        optind: 0,
    };
    let mut getopt = Getopt::new(OPTIONS, 0);
    while let Ok(Taken::Switch {
        code, value: given, ..
    }) = getopt.next(argv)
    {
        let value = given.clone().unwrap_or_default();
        let mode = settled.mode;
        let interactive = settled.interactive;
        let error = &mut settled.param_error;
        match code {
            b'a' if !shell => *error = Some(NO_SHELL),
            b'a' if !interactive => {
                if mode == Mode::Standard {
                    settled.interactive = true;
                } else {
                    *error = Some(CONFLICT);
                }
            }
            b'B' | b'E' if interactive && mode != Mode::ProcessStdin => *error = Some(CONFLICT),
            b'F' | b'R' | b'B' | b'E' => settled.process_stdin(code, value),
            b'f' => {
                if matches!(mode, Mode::Direct | Mode::ProcessStdin) {
                    *error = Some(CONFLICT);
                } else if settled.script_file.is_some() {
                    *error = Some("You can use -f only once.\n");
                } else {
                    settled.script_file = Some(value);
                }
            }
            b'l' if mode == Mode::Standard => settled.mode = Mode::Lint,
            b'r' => {
                if mode == Mode::Direct {
                    *error = Some("You can use -r only once.\n");
                } else if mode != Mode::Standard || interactive {
                    *error = Some(CONFLICT);
                } else {
                    settled.mode = Mode::Direct;
                    settled.direct = Some(value);
                }
            }
            b's' | b'w' => {
                if matches!(mode, Mode::Direct | Mode::ProcessStdin) {
                    *error = Some(if code == b's' {
                        "Source highlighting only works for files.\n"
                    } else {
                        "Source stripping only works for files.\n"
                    });
                } else {
                    settled.mode = if code == b's' {
                        Mode::Highlight
                    } else {
                        Mode::Strip
                    };
                }
            }
            10..=14 => {
                settled.mode = Mode::Report;
                settled.report = Some(match code {
                    10 => Report::Function(value),
                    11 => Report::Class(value),
                    12 => Report::Extension(value),
                    13 => Report::ZendExtension(value),
                    _ => Report::ExtensionInfo(value),
                });
            }
            15 => match given.as_deref() {
                None => {
                    settled.mode = Mode::Report;
                    settled.report = Some(Report::IniFiles);
                }
                // `PHP_CLI_MODE_SHOW_INI_DIFF`, the entries set away from
                // their built-in defaults: the engine keeps no default
                // apart from the value, so the host refuses it with the
                // usage.
                Some(b"diff") => settled.usage = true,
                Some(_) => *error = Some("Unknown argument for --ini\n"),
            },
            16 => settled.repeat = i64::from(atoi(&value)),
            _ => {}
        }
    }
    settled.optind = getopt.optind;
    settled
}

/// `do_cli_server`'s pass over the line (`php_cli_server.c:2808`): `-S`,
/// `-t` and each `-q`, the switches ending where `php_getopt` ends them,
/// and the word after them the router.
fn server_pass(argv: &[Vec<u8>]) -> Server {
    let mut server = Server {
        address: Vec::new(),
        document_root: None,
        router: None,
        quiet: 0,
    };
    let mut getopt = Getopt::new(OPTIONS, 0);
    while let Ok(Taken::Switch { code, value, .. }) = getopt.next(argv) {
        match code {
            b'S' => server.address = value.unwrap_or_default(),
            b't' => server.document_root = value.map(|value| PathBuf::from(os(&value))),
            b'q' => server.quiet = server.quiet.saturating_add(1),
            _ => {}
        }
    }
    server.router = argv.get(getopt.optind).map(|word| PathBuf::from(os(word)));
    server
}

/// Each word left on the line up to `--`, as a file to lint, as the
/// repeat at `out:` takes them (`php_cli.c:1176-1179`).
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
/// php does not know (`PHP_GETOPT_INVALID_ARG`).
fn usage(exit_code: i32) -> Refused {
    Refused {
        stdout: usage_text(),
        stderr: String::new(),
        exit_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Result<Options, Refused> {
        let argv: Vec<OsString> = words.iter().map(OsString::from).collect();
        Options::parse(&argv, &|_| false)
    }

    /// What the line asks of a binary whose engine has a shell.
    fn ask_shell(words: &[&str]) -> Result<(bool, Ask), (String, String, i32)> {
        let argv: Vec<OsString> = words.iter().map(OsString::from).collect();
        Options::parse(&argv, &|_| true)
            .map(|o| (o.interactive, o.ask))
            .map_err(|r| (r.stdout, r.stderr, r.exit_code))
    }

    #[test]
    fn the_shell_is_asked_for_where_the_engine_has_one() {
        let shell = |words: &[&str]| ask_shell(words);
        assert_eq!(
            shell(&["-n", "-a"]),
            Ok((true, Ask::Shell(None, Vec::new())))
        );
        assert_eq!(
            shell(&["-n", "-a", "a.php", "x"]),
            Ok((
                true,
                Ask::Shell(Some(PathBuf::from("a.php")), vec![OsString::from("x")])
            ))
        );
        assert_eq!(
            shell(&["-n", "-a", "-a", "--", "x"]),
            Ok((true, Ask::Shell(None, vec![OsString::from("x")])))
        );
        for conflict in [
            &["-n", "-a", "-r", "echo 1;"][..],
            &["-n", "-r", "echo 1;", "-a"],
            &["-n", "-a", "-B", "x"],
            &["-n", "-a", "-E", "x"],
            &["-n", "-R", "x", "-a"],
        ] {
            assert_eq!(
                shell(conflict).map_err(|e| e.0).err().as_deref(),
                Some(CONFLICT)
            );
        }
        assert!(matches!(
            shell(&["-n", "-a", "-l", "a.php"]),
            Ok((true, Ask::Lint { refused: None, .. }))
        ));
        assert!(matches!(
            shell(&["-n", "-l", "-a", "a.php"]),
            Ok((false, Ask::Lint { refused: Some(sentence), .. })) if sentence == CONFLICT
        ));
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
                startup: Startup {
                    ini_search: Search {
                        path: None,
                        ignore: true,
                    },
                    ini: vec![
                        (b"precision".to_vec(), b"10".to_vec()),
                        (b"foo".to_vec(), b"1".to_vec())
                    ],
                    extended_info: false,
                },
                repeat: 1,
                interactive: false,
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
        assert_eq!(ask(&["-n", "-H", "-r", "echo 1;"]), served);
        assert_eq!(ask(&["-n", "-t", "x", "-r", "echo 1;"]), served);
        assert_eq!(ask(&["-n", "-C", "-q", "-r", "echo 1;"]), served);
        assert_eq!(ask(&["-n", "--no-php-ini", "--run", "echo 1;"]), served);
        assert_eq!(ask(&["-n", "--run=echo 1;"]), served);
    }

    /// `-e` asks for nothing of its own: it is `main`'s
    /// `use_extended_info`, read in the first loop over the line, and
    /// the program is served as it would be without it.
    #[test]
    fn extended_info_is_read_and_serves_what_the_line_asks() {
        let read = |words: &[&str]| parse(words).map(|o| (o.startup.extended_info, o.ask));
        assert_eq!(
            read(&["-n", "-e", "-r", "echo 1;"]),
            Ok((true, Ask::Serve(direct("echo 1;"), Vec::new())))
        );
        assert_eq!(
            read(&["-n", "-e"]),
            Ok((true, Ask::Serve(Program::Stdin, Vec::new())))
        );
        assert_eq!(
            read(&["-n", "-r", "echo 1;"]),
            Ok((false, Ask::Serve(direct("echo 1;"), Vec::new())))
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
    fn ini_diff_is_refused_with_the_usage_and_any_other_value_in_phps_words() {
        assert_eq!(refused(&["-n", "--ini=diff"]), (true, String::new(), 1));
        assert_eq!(
            ask(&["-n", "--ini=x"]),
            Err(("Unknown argument for --ini\n".to_owned(), String::new(), 1))
        );
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

    fn code(text: &str) -> Vec<u8> {
        text.as_bytes().to_vec()
    }

    #[test]
    fn begin_run_and_end_process_stdin_with_the_words_after_them_as_arguments() {
        let lines = Lines {
            begin: Some(code("b")),
            each: Some(Each::Code(code("r"))),
            end: Some(code("e")),
        };
        assert_eq!(
            ask(&["-n", "-B", "b", "-R", "r", "-E", "e", "x", "y"]),
            Ok(Ask::Lines(
                lines,
                vec![OsString::from("x"), OsString::from("y")]
            ))
        );
        assert_eq!(
            ask(&["-n", "-F", "a.php"]),
            Ok(Ask::Lines(
                Lines {
                    each: Some(Each::File(PathBuf::from("a.php"))),
                    ..Lines::default()
                },
                Vec::new()
            ))
        );
        assert_eq!(
            ask(&["-n", "-E", "e"]),
            Ok(Ask::Lines(
                Lines {
                    end: Some(code("e")),
                    ..Lines::default()
                },
                Vec::new()
            ))
        );
    }

    /// The oracle at 8.5.9 over `sapi/cli/tests/012.phpt`'s lines: the
    /// last sentence set is the one put.
    #[test]
    fn process_stdin_refuses_a_second_switch_in_phps_words() {
        let sentence = |text: &str| Err((text.to_owned(), String::new(), 1));
        let r_or_f = "You can use -R or -F only once.\n";
        assert_eq!(ask(&["-n", "-F", "a", "-F", "a"]), sentence(r_or_f));
        assert_eq!(ask(&["-n", "-F", "a", "-R", "a"]), sentence(r_or_f));
        assert_eq!(ask(&["-n", "-R", "a", "-F", "a"]), sentence(r_or_f));
        assert_eq!(ask(&["-n", "-R", "a", "-R", "a"]), sentence(r_or_f));
        assert_eq!(
            ask(&["-n", "-f", "a", "-f", "a"]),
            sentence("You can use -f only once.\n")
        );
        assert_eq!(
            ask(&["-n", "-B", "", "-B", ""]),
            sentence("You can use -B only once.\n")
        );
        assert_eq!(
            ask(&["-n", "-E", "", "-E", ""]),
            sentence("You can use -E only once.\n")
        );
        assert_eq!(
            ask(&["-n", "-r", "", "-r", ""]),
            sentence("You can use -r only once.\n")
        );
        assert_eq!(ask(&["-n", "-r", "", "-B", ""]), sentence(CONFLICT));
        assert_eq!(
            ask(&["-n", "-R", "", "-s"]),
            sentence("Source highlighting only works for files.\n")
        );
    }

    #[test]
    fn the_reports_take_their_name_and_a_bare_word_as_the_file_to_open() {
        assert_eq!(
            ask(&["-n", "--rf", "strlen"]),
            Ok(Ask::Report(Report::Function(code("strlen")), None))
        );
        assert_eq!(
            ask(&["-n", "--rc", "Foo", "x.php"]),
            Ok(Ask::Report(
                Report::Class(code("Foo")),
                Some(PathBuf::from("x.php"))
            ))
        );
        assert_eq!(
            ask(&["-n", "--rextension=json"]),
            Ok(Ask::Report(Report::Extension(code("json")), None))
        );
        assert_eq!(
            ask(&["-n", "--rz", "x"]),
            Ok(Ask::Report(Report::ZendExtension(code("x")), None))
        );
        assert_eq!(
            ask(&["-n", "--ri", "json"]),
            Ok(Ask::Report(Report::ExtensionInfo(code("json")), None))
        );
        // `--ini` takes no value from the word after it, `-d`'s among
        // them (`sapi/cli/tests/gh21901.phpt`).
        assert_eq!(
            ask(&["-n", "-d", "foo=bar", "--ini"]),
            Ok(Ask::Report(Report::IniFiles, None))
        );
    }

    #[test]
    fn repeat_is_atoi_of_its_value() {
        let repeat = |words: &[&str]| parse(words).map(|options| options.repeat);
        assert_eq!(repeat(&["-n", "--repeat", "3", "-r", ""]), Ok(3));
        assert_eq!(repeat(&["-n", "--repeat", "2x", "-r", ""]), Ok(2));
        assert_eq!(repeat(&["-n", "--repeat", "x", "-r", ""]), Ok(0));
        // `num_repeats` is an `int`: C's `atoi` keeps the long's low 32 bits.
        assert_eq!(repeat(&["-n", "--repeat", "4294967298", "-r", ""]), Ok(2));
        assert_eq!(repeat(&["-n", "-r", ""]), Ok(1));
    }

    #[test]
    fn s_makes_the_host_the_server_with_its_root_and_router() {
        assert_eq!(
            ask(&["-t", "/d", "-n", "-S", "localhost:0", "r.php"]),
            Ok(Ask::Server(Server {
                address: code("localhost:0"),
                document_root: Some(PathBuf::from("/d")),
                router: Some(PathBuf::from("r.php")),
                quiet: 0,
            }))
        );
        // `-v`, `-i` and `-m` are the CLI's whatever came before them.
        assert_eq!(ask(&["-S", "localhost:0", "-v"]), Ok(Ask::Version));
    }
}
