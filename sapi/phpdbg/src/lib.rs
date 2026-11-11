//! php's `phpdbg` host, `php-src/sapi/phpdbg`, as one crate under `sapi/`
//! (ADR-0008): the binary `phpdbg`, reaching the engine through
//! `php_sapi` alone. The debugger is a `Tier` too (`jit-design.md` §2.4,
//! revision 8; #1308): the engine is built with [`debugger::Debugger`]
//! bound, whose `safepoint` is where it stops and whose loop is `step`, so a
//! program is served by the one dispatch loop and its kernels, and the prompt,
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
mod init;
mod input;
mod list;
mod natives;
mod opcodes;
mod out;
mod prompt;
mod read;
mod session;
mod shown;
mod watch;

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::process::ExitCode;

use php_ext::abi::Extension;
use php_sapi::Engine;
use php_sapi::getopt::{Getopt, Opt, Taken, opt};

use crate::cmd::{Command, Parameter};
use crate::session::{Session, Stop};

/// `sapi_module.name`: what `php_sapi_name()` answers.
pub const NAME: &str = "phpdbg";

/// `OPTIONS` (`Z/sapi/phpdbg/phpdbg.c:960-981`); the `-` row that ends
/// php's table is the slice's end.
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
#[allow(clippy::struct_excessive_bools)] // phpdbg's switches, each one fact of the line
#[derive(Debug, Default)]
struct Line {
    ini: Vec<(Vec<u8>, Vec<u8>)>,
    /// `-c` and `-n`.
    search: php_sapi::ini::Search,
    /// `-r` once runs at start, twice runs and quits.
    run: u32,
    stdin_delimiter: Option<Vec<u8>>,
    init_file: Option<Vec<u8>>,
    init_default: bool,
    /// `-b`: colours off.
    boring: bool,
    /// `-v`: quietness off.
    verbose: bool,
    exec: Option<Vec<u8>>,
    args: Vec<Vec<u8>>,
    /// `-e`, `ext_stmt`: the engine compiles with extended info
    /// (`Z/sapi/phpdbg/phpdbg.c:1250`, `:1498`).
    extended_info: bool,
}

/// The line read as `main` reads it (`phpdbg.c:1186-1286`).
fn read_line(argv: &[Vec<u8>]) -> Line {
    let mut line = Line {
        ini: HARDCODED
            .iter()
            .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
            .collect(),
        init_default: true,
        ..Line::default()
    };
    let mut getopt = Getopt::new(OPTIONS, 1);
    loop {
        let before = getopt.optind;
        match getopt.next(argv) {
            Ok(Taken::End) => break,
            Ok(Taken::Switch { code, value, .. }) => {
                let value = value.unwrap_or_default();
                match code {
                    b'r' => line.run += 1,
                    b'd' => line.ini.push(php_sapi::ini::define(&value)),
                    b's' => line.stdin_delimiter = Some(value),
                    b'I' => line.init_default = false,
                    b'c' => line.search.path = Some(value),
                    b'n' => line.search.ignore = true,
                    b'b' => line.boring = true,
                    b'v' => line.verbose = true,
                    b'e' => line.extended_info = true,
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

/// `php_sapi_phpdbg_log_message` before phpdbg is booted, as it stands
/// while php starts (`Z/sapi/phpdbg/phpdbg.c:767-768`): the entry and a
/// newline to C's `stdout`, an `fprintf`, which holds them past the
/// program's writes where stdout is not a terminal.
fn log_message(entry: &[u8]) {
    php_sapi::stdio::write(&[entry, b"\n"].concat());
}

/// The host's `main`.
pub fn main() -> ExitCode {
    let argv: Vec<Vec<u8>> = std::env::args_os()
        .map(|arg: OsString| arg.as_encoded_bytes().to_vec())
        .collect();
    main_with(&argv, &[])
}

/// phpdbg's engine: the bundle's enabled set, `beside` after it, the
/// debugger as its tier and phpdbg's own natives, over the php.ini files
/// the line found and its `-d`s, every file's shebang line passed over,
/// with extended info where the line carried `-e`.
///
/// A call of its own, as the CLI's engine is, so the builder, two
/// kilobytes moved through each step, stands in a frame that is gone
/// before the first request is served, and not under every frame of it
/// (#3209).
#[inline(never)]
fn engine(line: &Line, beside: &[Extension]) -> Engine {
    let mut extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    extensions.extend(beside.iter().cloned());
    let builder = Engine::builder()
        .ini_files(php_sapi::ini::search(NAME, &line.search, true))
        .sapi_name(NAME)
        .extensions(extensions)
        .tier::<debugger::Debugger>()
        .additional_extension(natives::extension())
        .skip_shebang()
        .extended_info(line.extended_info);
    php_sapi::start(builder, &line.ini, log_message)
}

/// The host's `main` over the line `argv`, with `beside` loaded after
/// the bundle's extensions, as php's phpdbg loads one a build carries
/// beside its own (`-z`, a Zend extension): what a test binary that
/// declares an extension of its own, an observer, runs to be phpdbg
/// with it.
pub fn main_with(argv: &[Vec<u8>], beside: &[Extension]) -> ExitCode {
    let line = read_line(argv);
    let engine = engine(&line, beside);

    let mut session = Session::new();
    session.line_args.clone_from(&line.args);
    session.args.clone_from(&line.args);
    if line.run > 1 {
        session.after_serve = session::AfterServe::Quit;
    }
    if let Some(exec) = &line.exec {
        session.exec = Some(host::expanded_path(exec));
    }
    session.init_files = init::Files {
        named: line.init_file.clone(),
        default: line.init_default,
        scan_dir: std::env::var_os("PHP_INI_SCAN_DIR").map(|dir| dir.as_bytes().to_vec()),
    };
    session.quiet = !line.verbose;
    session.line_quiet = session.quiet;
    if line.boring {
        session.line_colours = out::Colours::Off;
    }
    session.out.set_colours(session.line_colours);
    if let Some(delimiter) = &line.stdin_delimiter {
        session.queued.push(Command::Params(vec![
            Parameter::Str(b"stdin".to_vec()),
            Parameter::Str(delimiter.clone()),
        ]));
    }
    if line.run > 0 && session.exec.is_some() {
        session.stopped = Some(Stop::ServeWith(None));
        session.exec_compile = session::ExecCompile::Announced;
    }
    php_sapi::exit_code(host::serve(&engine, session))
}
