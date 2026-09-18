//! The CLI host, php's `sapi/cli/php_cli.c`: the command line read as
//! `main` and `do_cli` read it through `php_getopt`, one request
//! description built from it, the sink on the process's two streams, and
//! the exit code as `serve` ends the request (design §4.1, §4.3 seam 4,
//! §5.14).
//!
//! What the command line asks for is one of six things, as `do_cli`
//! settles it: the usage, the version, `phpinfo()` as text, the
//! extension listing, a request over one program, or `-l`, the lint of
//! each program named. A program is a file named on the line or after
//! `-f`, the code after `-r`, or stdin. `-n` and every `-d` are the
//! engine's to lay over its defaults before it is built, so the binary
//! reads them off [`Options`] and builds the engine; the host then
//! serves [`Options::ask`] on it. Which of the three programs is named
//! more than once, or with another, is refused with php's sentence
//! before anything is built.
//!
//! The switches taken are `php_cli.c`'s `OPTIONS` table as `main` and
//! `do_cli` read it: `-a`, which is `do_cli`'s sentence for a build
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
//! stdout and exit 1: `-s` and `-w` over the program, `-B`, `-R`, `-F`
//! and `-E` over stdin's lines, `-S` the dev server, `--rf`, `--rc`,
//! `--re`, `--rz` and `--ri` the reflection printers, `--ini` and
//! `--ini=diff`, and `--repeat`. A switch php does not know is
//! `php_getopt`'s line on stderr before the usage.
//!
//! Where `php_getopt` and `do_cli` read stdin lazily, this host does not:
//! nothing in the engine reads the request's body yet (design §4.3 seam
//! 4, the body stream), so stdin is read whole where it is the program.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;

use php_engine::{Engine, OutputSink, PHP_VERSION, RequestDescription};

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
}

/// The command line, read: the engine's switches and what is asked of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// `-n`: `php.ini` is not looked for. The engine reads no file yet, so
    /// the switch is taken and changes nothing (design §11, row 15–21).
    pub ini_ignore: bool,
    /// Each `-d name=value`, in the order given; a `-d name` is `name=1`,
    /// as `php_ini_builder_define` writes it.
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
}

/// One row of `php_cli.c`'s `OPTIONS`: the code `php_getopt` returns, a
/// letter or php's number for a long switch, whether it takes a value
/// (`1`), may (`2`) or does not (`0`), and its long name.
struct Opt {
    code: u8,
    need: u8,
    name: &'static str,
}

const fn opt(code: u8, need: u8, name: &'static str) -> Opt {
    Opt { code, need, name }
}

/// `OPTIONS`, `php_cli.c:124-165`, row for row.
const OPTIONS: [Opt; 38] = [
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

/// `php_opt_error`'s three reasons, `main/getopt.c:20-24`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptError {
    Colon,
    NotFound,
    NoArgument,
}

/// Where `php_getopt` stopped and why: the argument and the character
/// its line names, and the reason.
type Stopped = (usize, usize, OptError);

/// `php_getopt` (`Z/main/getopt.c:55-198`) over `argv[1..]`, with the
/// state it keeps in statics, `optchr` and `dash`, kept here: `optchr`
/// is where in a clustered word the next letter is and outlives the
/// word, which is why the line php prints for a switch it does not know
/// counts from the previous cluster's end. Copied as written, since that
/// line is observable.
struct Getopt {
    optind: usize,
    optchr: usize,
    dash: bool,
}

/// One thing `php_getopt` returned: a switch with its value, or the end
/// of the switches.
enum Taken {
    Switch(u8, Option<Vec<u8>>),
    End,
}

impl Getopt {
    fn new() -> Getopt {
        Getopt {
            optind: 0,
            optchr: 0,
            dash: false,
        }
    }

    /// The next switch, `php_getopt` with `arg_start` 2.
    fn next(&mut self, argv: &[Vec<u8>]) -> Result<Taken, Stopped> {
        let Some(word) = argv.get(self.optind) else {
            return Ok(Taken::End);
        };
        if !self.dash && (word.first() != Some(&b'-') || word.len() == 1) {
            return Ok(Taken::End);
        }
        let (index, arg_start) = if word.starts_with(b"--") {
            if word.len() == 2 {
                self.optind += 1;
                return Ok(Taken::End);
            }
            self.long(word)?
        } else {
            if !self.dash {
                self.dash = true;
                self.optchr = 1;
            }
            if word.get(self.optchr) == Some(&b':') {
                self.dash = false;
                self.optind += 1;
                return Err((self.optind - 1, self.optchr, OptError::Colon));
            }
            (self.short(word)?, 1 + self.optchr)
        };
        let row = &OPTIONS[index];
        if row.need != 0 {
            return self.value(argv, word, row, arg_start);
        }
        if arg_start >= 2 && !word.starts_with(b"--") {
            if word.get(self.optchr + 1).is_none() {
                self.dash = false;
                self.optind += 1;
            } else {
                self.optchr += 1;
            }
        } else {
            self.optind += 1;
        }
        Ok(Taken::Switch(row.code, None))
    }

    /// A `--name` or `--name=value` word: its row and where its value
    /// starts; a name no row has is `OPTERRARG` at the `optchr` left
    /// over from the previous word.
    fn long(&mut self, word: &[u8]) -> Result<(usize, usize), Stopped> {
        let mut arg_start = 2;
        let mut arg_end = word.len() - 1;
        if let Some(at) = word[arg_start..arg_end].iter().position(|&b| b == b'=') {
            arg_end = at;
            arg_start += 1;
        } else {
            arg_end -= 1;
        }
        let found = OPTIONS.iter().position(|row| {
            row.code == b'-'
                || (row.name.len() == arg_end && word[2..].starts_with(row.name.as_bytes()))
        });
        let index = match found {
            Some(index) if OPTIONS[index].code != b'-' => index,
            _ => {
                self.optind += 1;
                return Err((self.optind - 1, self.optchr, OptError::NoArgument));
            }
        };
        self.optchr = 0;
        self.dash = false;
        Ok((index, arg_start + OPTIONS[index].name.len()))
    }

    /// The letter at `optchr` of a `-xyz` word: its row, or `OPTERRNF`
    /// with the cluster advanced past it.
    fn short(&mut self, word: &[u8]) -> Result<usize, Stopped> {
        let letter = word.get(self.optchr).copied();
        if let Some(index) = OPTIONS
            .iter()
            .position(|row| row.code != b'-' && Some(row.code) == letter)
        {
            return Ok(index);
        }
        let at = (self.optind, self.optchr);
        if word.get(self.optchr + 1).is_none() {
            self.dash = false;
            self.optind += 1;
        } else {
            self.optchr += 1;
        }
        Err((at.0, at.1, OptError::NotFound))
    }

    /// The value of a switch that takes one: attached, after `=`, or the
    /// next word; a switch that may take one (`--ini`) takes only an
    /// attached one.
    fn value(
        &mut self,
        argv: &[Vec<u8>],
        word: &[u8],
        row: &Opt,
        arg_start: usize,
    ) -> Result<Taken, Stopped> {
        self.dash = false;
        self.optind += 1;
        let value = match word.get(arg_start) {
            None if self.optind == argv.len() => {
                if row.need == 1 {
                    return Err((self.optind - 1, self.optchr, OptError::NoArgument));
                }
                None
            }
            None if row.need == 1 => {
                self.optind += 1;
                argv.get(self.optind - 1).cloned()
            }
            None => None,
            Some(b'=') => Some(word[arg_start + 1..].to_vec()),
            Some(_) => Some(word[arg_start..].to_vec()),
        };
        Ok(Taken::Switch(row.code, value))
    }
}

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
            let arguments = rest.map(|word| os(word)).collect();
            Ask::Serve(program.unwrap_or(Program::Stdin), arguments)
        };
        Ok(options)
    }

    /// `main`'s loop over the line (`php_cli.c:1290-1325`), `show_err`
    /// set: the ini switches onto `self`, and what ends it early.
    fn main_pass(&mut self, argv: &[Vec<u8>]) -> Result<Option<Ask>, Refused> {
        let mut getopt = Getopt::new();
        loop {
            let ask = match getopt.next(argv) {
                Ok(Taken::End) => return Ok(None),
                Err((argument, at, error)) => {
                    return Err(getopt_error(argv, argument, at, error));
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
        let mut getopt = Getopt::new();
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
                b'B' | b'E' | b'F' | b'R' | b's' | b'w' | 10..=16 => return Err(usage(1)),
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

fn os(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// `php_ini_builder_define`: `name=value`, or `name=1` for a bare name.
fn define(given: &str) -> (String, String) {
    match given.split_once('=') {
        Some((name, value)) => (name.to_owned(), value.to_owned()),
        None => (given.to_owned(), String::from("1")),
    }
}

/// `php_opt_error`'s line on stderr (`Z/main/getopt.c:28-50`), the
/// argument counted from the program's name and the character from
/// one, and then `main`'s `php_cli_usage` on stdout and exit 1.
fn getopt_error(argv: &[Vec<u8>], argument: usize, at: usize, error: OptError) -> Refused {
    let letter = argv
        .get(argument)
        .and_then(|word| word.get(at))
        .map_or(String::new(), |&b| {
            String::from_utf8_lossy(&[b]).into_owned()
        });
    let reason = match error {
        OptError::Colon => String::from(": in flags"),
        OptError::NotFound => format!("option not found {letter}"),
        OptError::NoArgument => format!("no argument for option {letter}"),
    };
    Refused {
        stderr: format!(
            "Error in argument {}, char {}: {reason}\n",
            argument + 1,
            at + 1
        ),
        ..usage(1)
    }
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
        env!("PHP_BUILD_DATE")
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

/// `print_modules` and `print_extensions`: the loaded extensions sorted
/// as `module_name_cmp` sorts them, without regard to case, under
/// `[PHP Modules]`, and the Zend extensions, of which this engine has
/// none, under `[Zend Modules]`.
#[must_use]
pub fn extensions(engine: &Engine) -> String {
    let mut names = engine.loaded_extensions();
    names.sort_by_key(|name| name.to_ascii_lowercase());
    let mut text = String::from("[PHP Modules]\n");
    for name in names {
        text.push_str(&name);
        text.push('\n');
    }
    text.push_str("\n[Zend Modules]\n\n");
    text
}

/// The sink on the process's streams, php's CLI: body bytes on stdout,
/// the header block nowhere, since `sapi_cli_send_headers` sends none,
/// and both the log and the error stream on stderr, each log entry with
/// `\n` after it (`sapi_cli_log_message`). A stream that refuses a write
/// is left refusing: php's `sapi_cli_single_write` reads no error either.
#[derive(Debug, Default)]
pub struct Process;

impl OutputSink for Process {
    fn write(&mut self, bytes: &[u8]) {
        let _ = std::io::stdout().lock().write_all(bytes);
    }

    fn flush(&mut self) {
        let _ = std::io::stdout().lock().flush();
    }

    fn send_headers(&mut self, _block: &[u8]) {}

    fn log(&mut self, entry: &[u8]) {
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(entry);
        let _ = stderr.write_all(b"\n");
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        let _ = std::io::stderr().lock().write_all(bytes);
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
/// request begins.
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
    let mut argv = vec![php_self];
    argv.extend(arguments.iter().map(|a| a.as_encoded_bytes().to_vec()));
    Ok(RequestDescription {
        argv,
        env: std::env::vars_os()
            .map(|(n, v)| (n.as_encoded_bytes().to_vec(), v.as_encoded_bytes().to_vec()))
            .collect(),
        cwd: std::env::current_dir().unwrap_or_default(),
        sapi_name: String::from(NAME),
        script_name: Some(script),
        source,
        ..RequestDescription::default()
    })
}

/// Serve `program` on `engine` as `do_cli` serves it and hand back the
/// exit code the process ends with; see [`describe`].
pub fn serve(engine: &Engine, program: &Program, arguments: &[OsString]) -> i32 {
    match describe(program, arguments) {
        Ok(description) => engine.serve(description, &mut Process).exit_code,
        Err(code) => code,
    }
}

/// `-l` over one program, `do_cli`'s `PHP_CLI_MODE_LINT` arm
/// (`php_cli.c:954-961`): the program compiled by [`Engine::lint`] and
/// not served, then `No syntax errors detected in %s` and 0, or `Errors
/// parsing %s` and 255, on the sink after what the compiler rendered.
/// A file that cannot be opened is exit 1 before a request begins.
pub fn lint(engine: &Engine, program: &Program, sink: &mut dyn OutputSink) -> i32 {
    let description = match describe(program, &[]) {
        Ok(description) => description,
        Err(code) => return code,
    };
    let name = description.script_name.clone().unwrap_or_default();
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
            &["-n", "-s", "a.php"][..],
            &["-n", "-w", "a.php"],
            &["-n", "-B", "x"],
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
        fn send_headers(&mut self, _block: &[u8]) {}
        fn log(&mut self, entry: &[u8]) {
            self.0.extend_from_slice(entry);
            self.0.push(b'\n');
        }
        fn write_stderr(&mut self, bytes: &[u8]) {
            self.0.extend_from_slice(bytes);
        }
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
