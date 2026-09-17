//! The CLI host, php's `sapi/cli/php_cli.c`: the command line read as
//! `do_cli` reads it, one request description built from it, the sink on
//! the process's two streams, and the exit code as `serve` ends the
//! request (design §4.1, §4.3 seam 4, §5.14).
//!
//! What the command line asks for is one of five things, as `do_cli`
//! settles it: the version, `phpinfo()` as text, the extension listing,
//! or a request over one program, the program being a file named on the
//! line or after `-f`, the code after `-r`, or stdin. `-n` and every `-d`
//! are the engine's to lay over its defaults before it is built, so the
//! binary reads them off [`Options`] and builds the engine; the host
//! then serves [`Options::ask`] on it. Which of the three programs is
//! named more than once, or with another, is refused with php's sentence
//! before anything is built.
//!
//! What php's CLI does that is not here yet: the `-a` shell, `-l`, `-s`
//! and `-w` over the program, `-B`, `-R`, `-F` and `-E` over stdin's
//! lines, `-c` and `-z`, `-S` the dev server, `-e`, `-H` and `--rf`,
//! `--rc`, `--re`, `--ri`. Each is refused as an unknown switch, with
//! php's usage on stdout and php's exit code for it.

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
    /// `-v`, `--version`: `php_print_version`.
    Version,
    /// `-i`: `php_print_info` as text.
    Info,
    /// `-m`: the loaded extensions, `print_modules`.
    Extensions,
    /// A request over one program, with what follows it as `$argv[1..]`.
    Serve(Program, Vec<OsString>),
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

/// Why a command line was not read: php's sentence for it, where it goes
/// and the exit code `do_cli` ends with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub message: String,
    /// `PUTS(param_error)` writes on stdout; `php_cli_usage` too.
    pub on_stderr: bool,
    pub exit_code: i32,
}

/// `param_mode_conflict`, `php_cli.c:563`.
const CONFLICT: &str = "Either execute direct code, process stdin or use a file.\n";

impl Options {
    /// Read `argv[1..]` as `do_cli` reads it: the switches php's
    /// `php_getopt` takes, each with its value attached or as the next
    /// word, then the program, then what is left as its arguments. `--`
    /// ends the switches; a word after it is the program unless one was
    /// named already.
    pub fn parse(argv: &[OsString]) -> Result<Options, Refused> {
        let mut options = Options {
            ini_ignore: false,
            ini: Vec::new(),
            ask: Ask::Serve(Program::Stdin, Vec::new()),
        };
        let mut program: Option<Program> = None;
        let mut words = argv.iter();
        let mut rest: Vec<OsString> = Vec::new();
        while let Some(word) = words.next() {
            let text = word.to_string_lossy();
            if text == "--" {
                rest.extend(words.cloned());
                break;
            }
            if text == "--version" {
                options.ask = Ask::Version;
                return Ok(options);
            }
            let Some(switches) = text.strip_prefix('-').filter(|s| !s.is_empty()) else {
                // The first bare word is the program; the rest are its arguments.
                if program.is_none() {
                    program = Some(Program::File(PathBuf::from(word)));
                } else {
                    rest.push(word.clone());
                }
                rest.extend(words.cloned());
                break;
            };
            let mut letters = switches.chars();
            while let Some(letter) = letters.next() {
                match letter {
                    'n' => options.ini_ignore = true,
                    'q' | 'C' => {}
                    'v' => {
                        options.ask = Ask::Version;
                        return Ok(options);
                    }
                    'i' => {
                        options.ask = Ask::Info;
                        return Ok(options);
                    }
                    'm' => {
                        options.ask = Ask::Extensions;
                        return Ok(options);
                    }
                    'd' | 'f' | 'r' => {
                        let attached: String = letters.collect();
                        let value = if attached.is_empty() {
                            match words.next() {
                                Some(next) => next.clone(),
                                None => return Err(usage(1)),
                            }
                        } else {
                            OsString::from(attached)
                        };
                        match letter {
                            'd' => options.ini.push(define(&value.to_string_lossy())),
                            'f' => {
                                if matches!(program, Some(Program::Direct(_))) {
                                    return Err(conflict());
                                }
                                if program.is_some() {
                                    return Err(refused("You can use -f only once.\n"));
                                }
                                program = Some(Program::File(PathBuf::from(value)));
                            }
                            _ => {
                                if program.is_some() {
                                    return Err(match program {
                                        Some(Program::Direct(_)) => {
                                            refused("You can use -r only once.\n")
                                        }
                                        _ => conflict(),
                                    });
                                }
                                program = Some(Program::Direct(value.as_encoded_bytes().to_vec()));
                            }
                        }
                        break;
                    }
                    _ => return Err(usage(1)),
                }
            }
        }
        // A file named by `-f` takes the first bare word as an argument;
        // `-r` leaves the bare words to `$argv` too.
        let program = program.unwrap_or(Program::Stdin);
        options.ask = Ask::Serve(program, rest);
        Ok(options)
    }
}

/// `php_ini_builder_define`: `name=value`, or `name=1` for a bare name.
fn define(given: &str) -> (String, String) {
    match given.split_once('=') {
        Some((name, value)) => (name.to_owned(), value.to_owned()),
        None => (given.to_owned(), String::from("1")),
    }
}

fn refused(message: &str) -> Refused {
    Refused {
        message: message.to_owned(),
        on_stderr: false,
        exit_code: 1,
    }
}

fn conflict() -> Refused {
    refused(CONFLICT)
}

/// `php_cli_usage` on a switch php does not know, exit 1
/// (`PHP_GETOPT_INVALID_ARG`); the usage is the head of php's, the
/// lines this host serves.
fn usage(exit_code: i32) -> Refused {
    Refused {
        message: String::from(
            "Usage: php [options] [-f] <file> [--] [args...]\n\
             \x20  php [options] -r <code> [--] [args...]\n\
             \x20  php [options] [--] [args...]\n\
             \n\
             \x20 -d foo[=bar]     Define INI entry foo with value 'bar'\n\
             \x20 -f <file>        Parse and execute <file>.\n\
             \x20 -i               PHP information\n\
             \x20 -m               Show compiled in modules\n\
             \x20 -n               No configuration (ini) files will be used\n\
             \x20 -r <code>        Run PHP <code> without using script tags <?..?>\n\
             \x20 -v               Version number\n",
        ),
        on_stderr: false,
        exit_code,
    }
}

/// `php_get_version` for this host: `PHP <version> (cli) (built: <date>)
/// (NTS)`, the one line upstream's test runner and a person read. php follows
/// it with its copyright lines and the Zend engine's; this engine is not
/// php and prints neither.
#[must_use]
pub fn version() -> String {
    format!(
        "PHP {PHP_VERSION} ({NAME}) (built: {}) (NTS)\n",
        option_env!("PHP_BUILD_DATE").unwrap_or("unknown")
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

/// Serve `program` on `engine` as `do_cli` serves it and hand back the
/// exit code the process ends with: the request description carries
/// `argv` with `php_self` first, the process's environment and its
/// working directory; the file's bytes are the engine's to read by name;
/// `-r`'s code is served under the name `Command line code` with php's
/// open tag before it on the same line, as `zend_eval_string` scans it
/// in `ST_IN_SCRIPTING`; stdin is read whole and served as `Standard
/// input code`. A file that cannot be opened is
/// `cli_seek_file_begin`'s line on stderr and exit 1, before a request
/// begins.
///
/// What php's `-R`-family reads stdin lazily for is not here: nothing in
/// the engine reads the request's body yet (design §4.3 seam 4, the
/// body stream), so stdin is read only where it is the program.
pub fn serve(engine: &Engine, program: &Program, arguments: &[OsString]) -> i32 {
    let (php_self, source): (Vec<u8>, Option<Vec<u8>>) = match program {
        Program::File(path) => {
            if std::fs::File::open(path).is_err() {
                let mut stderr = std::io::stderr().lock();
                let _ = stderr.write_all(b"Could not open input file: ");
                let _ = stderr.write_all(path.as_os_str().as_encoded_bytes());
                let _ = stderr.write_all(b"\n");
                return 1;
            }
            (path.as_os_str().as_encoded_bytes().to_vec(), None)
        }
        Program::Direct(code) => {
            let mut source = b"<?php ".to_vec();
            source.extend_from_slice(code);
            (b"Command line code".to_vec(), Some(source))
        }
        Program::Stdin => {
            let mut source = Vec::new();
            let _ = std::io::stdin().lock().read_to_end(&mut source);
            (b"Standard input code".to_vec(), Some(source))
        }
    };
    let mut argv = vec![php_self.clone()];
    argv.extend(arguments.iter().map(|a| a.as_encoded_bytes().to_vec()));
    let description = RequestDescription {
        argv,
        env: std::env::vars_os()
            .map(|(n, v)| (n.as_encoded_bytes().to_vec(), v.as_encoded_bytes().to_vec()))
            .collect(),
        cwd: std::env::current_dir().unwrap_or_default(),
        sapi_name: String::from(NAME),
        script_name: Some(php_self),
        source,
        ..RequestDescription::default()
    };
    let mut sink = Process;
    engine.serve(description, &mut sink).exit_code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Result<Options, Refused> {
        let argv: Vec<OsString> = words.iter().map(OsString::from).collect();
        Options::parse(&argv)
    }

    fn ask(words: &[&str]) -> Result<Ask, String> {
        parse(words).map(|o| o.ask).map_err(|r| r.message)
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
                    Program::File(PathBuf::from("a.php")),
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
                message: CONFLICT.to_owned(),
                on_stderr: false,
                exit_code: 1
            })
        );
        assert_eq!(
            ask(&["-r", "echo 1;", "-r", "echo 2;"]),
            Err("You can use -r only once.\n".to_owned())
        );
    }

    #[test]
    fn a_word_after_the_dashes_is_an_argument_of_direct_code() {
        assert_eq!(
            ask(&["-r", "echo 1;", "--", "-n"]),
            Ok(Ask::Serve(
                Program::Direct(b"echo 1;".to_vec()),
                vec![OsString::from("-n")]
            ))
        );
    }

    #[test]
    fn the_version_and_the_listing_end_the_reading() {
        assert_eq!(ask(&["-v", "a.php"]), Ok(Ask::Version));
        assert_eq!(ask(&["--version"]), Ok(Ask::Version));
        assert_eq!(ask(&["-n", "-m", "x.php"]), Ok(Ask::Extensions));
        assert_eq!(ask(&["-i"]), Ok(Ask::Info));
    }

    #[test]
    fn an_unknown_switch_is_the_usage_and_exit_1() {
        let refused = parse(&["-Z"]).err().map(|r| (r.exit_code, r.message));
        assert_eq!(
            refused
                .as_ref()
                .map(|(code, text)| (*code, text.starts_with("Usage: php"))),
            Some((1, true))
        );
    }
}
