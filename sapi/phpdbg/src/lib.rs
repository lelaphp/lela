//! php's `phpdbg` host, `php-src/sapi/phpdbg`, as one crate under `sapi/`
//! (ADR-0008): the binary `phpdbg`, reaching the engine through
//! `php_sapi` alone. The debugger is a `Tier` too (`jit-design.md` §2.4,
//! revision 8; #1308): the engine is built with [`debugger::Debugger`]
//! bound, whose `safepoint` is the gate and whose loop is `step`, so a
//! program runs under the one executor and its kernels, and the prompt,
//! the breakpoints and the watch list live here.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

mod bp;
mod cmd;
mod debugger;
mod host;
mod info;
mod input;
mod list;
mod natives;
mod out;
mod prompt;
mod session;
mod watch;

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::process::ExitCode;

use php_sapi::Engine;
use php_sapi::getopt::{Getopt, Opt, Taken, opt};

use crate::cmd::{Command, Param};
use crate::session::{Session, Stop};

/// `sapi_module.name`: what `php_sapi_name()` answers.
pub const NAME: &str = "phpdbg";

/// `OPTIONS` (`Z/sapi/phpdbg/phpdbg.c:960-981`).
static OPTIONS: &[Opt] = &[
    opt(b'c', 1, "ini path override"),
    opt(b'd', 1, "define ini entry on command line"),
    opt(b'n', 0, "no php.ini"),
    opt(b'z', 1, "load zend_extension"),
    opt(b'q', 0, "no banner"),
    opt(b'v', 0, "disable quietness"),
    opt(b'b', 0, "boring colours"),
    opt(b'i', 1, "specify init"),
    opt(b'I', 0, "ignore init"),
    opt(b'O', 1, "opline log"),
    opt(b'r', 0, "run"),
    opt(b'e', 0, "generate ext_stmt opcodes"),
    opt(b'E', 0, "step-through-eval"),
    opt(b's', 1, "script from stdin"),
    opt(b'S', 1, "sapi-name"),
    opt(b'p', 2, "show opcodes"),
    opt(b'h', 0, "help"),
    opt(b'V', 0, "version"),
    opt(b'-', 0, ""),
];

/// `phpdbg_ini_hardcoded`, the entries phpdbg sets under the line's.
const HARDCODED: [(&str, &str); 8] = [
    ("html_errors", "Off"),
    ("implicit_flush", "On"),
    ("display_errors", "Off"),
    ("log_errors", "On"),
    ("max_execution_time", "0"),
    ("max_input_time", "-1"),
    ("error_log", ""),
    ("output_buffering", "off"),
];

/// What the line asked for.
#[derive(Debug, Default)]
struct Line {
    ini: Vec<(String, String)>,
    /// `-r` once runs at start, twice runs and quits.
    run: u32,
    stdin_delimiter: Option<Vec<u8>>,
    init_file: Option<Vec<u8>>,
    init_default: bool,
    exec: Option<Vec<u8>>,
    args: Vec<Vec<u8>>,
}

/// The line read as `main` reads it (`phpdbg.c:1186-1286`).
fn read_line(argv: &[Vec<u8>]) -> Line {
    let mut line = Line {
        ini: HARDCODED
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        init_default: true,
        ..Line::default()
    };
    let mut getopt = Getopt::new(OPTIONS);
    getopt.optind = 1;
    loop {
        let before = getopt.optind;
        match getopt.next(argv) {
            Ok(Taken::End) => break,
            Ok(Taken::Switch(code, value)) => {
                let value = value.unwrap_or_default();
                match code {
                    b'r' => line.run += 1,
                    b'd' => line
                        .ini
                        .push(php_sapi::ini::define(&String::from_utf8_lossy(&value))),
                    b's' => line.stdin_delimiter = Some(value),
                    b'I' => line.init_default = false,
                    b'i' => line.init_file = (!value.is_empty()).then_some(value),
                    _ => {}
                }
            }
            Err(_) if getopt.optind == before => break,
            Err(_) => {}
        }
    }
    let mut rest = argv.iter().skip(getopt.optind);
    if line.stdin_delimiter.is_none()
        && let Some(exec) = rest.next()
    {
        line.exec = (!exec.is_empty()).then(|| exec.clone());
    }
    line.args = rest.cloned().collect();
    line
}

/// The host's `main`.
pub fn main() -> ExitCode {
    let argv: Vec<Vec<u8>> = std::env::args_os()
        .map(|arg: OsString| arg.as_encoded_bytes().to_vec())
        .collect();
    let line = read_line(&argv);
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    let builder = Engine::builder()
        .extensions(&extensions)
        .tier::<debugger::Debugger>()
        .additional_extension(natives::extension())
        .skip_shebang();
    let engine = php_sapi::start(builder, &line.ini);

    let mut session = Session::new();
    session.line_args.clone_from(&line.args);
    session.args.clone_from(&line.args);
    if line.run > 1 {
        session.after_run = session::AfterRun::Quit;
    }
    if let Some(exec) = &line.exec {
        let path = std::path::Path::new(std::ffi::OsStr::from_bytes(exec));
        session.exec = Some(std::fs::canonicalize(path).map_or_else(
            |_| exec.clone(),
            |path| path.as_os_str().as_bytes().to_vec(),
        ));
    }
    session.queued = init(&line);
    if let Some(delimiter) = &line.stdin_delimiter {
        session.queued.push(Command::Params(vec![
            Param::Str(b"stdin".to_vec()),
            Param::Str(delimiter.clone()),
        ]));
    }
    if line.run > 0 && session.exec.is_some() {
        session.stopped = Some(Stop::RunWith(None));
        session.context = session::Context::Announced;
    }
    session::install(session);
    php_sapi::exit_code(host::serve(&engine))
}

/// `phpdbg_init`: the commands of the init file the line names, or,
/// unless `-I`, of a `.phpdbginit` in each directory of
/// `PHP_INI_SCAN_DIR` and then the working directory's, a line each; run
/// before anything the prompt reads.
fn init(line: &Line) -> Vec<Command> {
    let mut files: Vec<Vec<u8>> = Vec::new();
    if let Some(file) = &line.init_file {
        files.push(file.clone());
    } else if line.init_default {
        if let Some(dirs) = std::env::var_os("PHP_INI_SCAN_DIR") {
            for dir in dirs.as_bytes().split(|b| *b == b':') {
                files.push([dir, b"/.phpdbginit"].concat());
            }
        }
        files.push(b".phpdbginit".to_vec());
    }
    let mut commands = Vec::new();
    for file in files {
        let path = std::path::Path::new(std::ffi::OsStr::from_bytes(&file));
        let Ok(text) = std::fs::read(path) else {
            continue;
        };
        for command in text.split(|b| *b == b'\n') {
            let command = command.trim_ascii_end();
            if command.is_empty() || command.first() == Some(&b'#') {
                continue;
            }
            if let Ok(parsed) = cmd::parse(command) {
                commands.extend(parsed);
            }
        }
    }
    commands
}
