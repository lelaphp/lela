//! `php`, the binary (design §4.1, §4.10): one engine of the bundle's
//! enabled set with the bundle's tier bound, the command line read by
//! the host the binary's name chose, the CLI or, started as `php-cgi`, CGI,
//! and the process ending with the code the request ended with. The ini the line carries is laid over the engine's defaults
//! before it is built, as php's `-d` reaches `php_ini_builder` before
//! `php_module_startup`; the version needs no engine and the listing
//! needs one served nothing.
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

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::process::ExitCode;

use php_sapi::cgi::Ask as CgiAsk;
use php_sapi::cli::{Ask, Options};
use php_sapi::{Engine, Host, HostExtension};

pub(crate) fn main() -> ExitCode {
    let mut words = std::env::args_os();
    let argv0 = words.next().unwrap_or_default();
    let argv: Vec<OsString> = words.collect();
    match Host::of(&argv0) {
        Host::Cli => cli(&argv),
        Host::Cgi => cgi(&argv0, &argv),
    }
}

/// php-cgi's `main`: the line read in the process's environment, and the
/// request served, or what the line asked for instead, on an engine
/// with `cgi-fcgi` loaded.
fn cgi(argv0: &OsStr, argv: &[OsString]) -> ExitCode {
    let env = php_sapi::cgi::environment();
    let options = php_sapi::cgi::Options::parse(argv, &env);
    match &options.ask {
        CgiAsk::Usage { stderr, exit_code } => {
            let _ = std::io::stderr().lock().write_all(stderr.as_bytes());
            let _ = std::io::stdout()
                .lock()
                .write_all(php_sapi::cgi::usage_text(argv0).as_bytes());
            code(*exit_code)
        }
        CgiAsk::Version => {
            let _ = std::io::stdout()
                .lock()
                .write_all(php_sapi::cgi::version().as_bytes());
            code(0)
        }
        CgiAsk::Extensions => {
            let engine = engine(&options.ini, Some(php_sapi::cgi::EXTENSION));
            let _ = std::io::stdout()
                .lock()
                .write_all(php_sapi::cli::extensions(&engine).as_bytes());
            code(0)
        }
        CgiAsk::Serve(invocation) => {
            let engine = engine(&options.ini, Some(php_sapi::cgi::EXTENSION));
            code(php_sapi::cgi::serve(&engine, invocation, env, &options.ini))
        }
    }
}

/// php's CLI `main`.
fn cli(argv: &[OsString]) -> ExitCode {
    let options = match Options::parse(argv) {
        Ok(options) => options,
        Err(refused) => {
            let _ = std::io::stderr()
                .lock()
                .write_all(refused.stderr.as_bytes());
            let _ = std::io::stdout()
                .lock()
                .write_all(refused.stdout.as_bytes());
            return code(refused.exit_code);
        }
    };
    match &options.ask {
        Ask::Usage => {
            let _ = std::io::stdout()
                .lock()
                .write_all(php_sapi::cli::usage_text().as_bytes());
            code(0)
        }
        Ask::Version => {
            let _ = std::io::stdout()
                .lock()
                .write_all(php_sapi::cli::version().as_bytes());
            code(0)
        }
        Ask::Info => {
            let _ = std::io::stdout()
                .lock()
                .write_all(php_sapi::cli::info().as_bytes());
            code(0)
        }
        Ask::Extensions => {
            let engine = engine(&options.ini, None);
            let _ = std::io::stdout()
                .lock()
                .write_all(php_sapi::cli::extensions(&engine).as_bytes());
            code(0)
        }
        Ask::Serve(program, arguments) => {
            let engine = engine(&options.ini, None);
            code(php_sapi::cli::serve(&engine, program, arguments))
        }
        Ask::Highlight(program, arguments) => {
            let engine = engine(&options.ini, None);
            code(php_sapi::cli::highlight(&engine, program, arguments))
        }
        Ask::Strip(program, arguments) => {
            let engine = engine(&options.ini, None);
            code(php_sapi::cli::strip(&engine, program, arguments))
        }
        // `do_cli`'s lint loop: the status starts at 0, a lint that fails
        // sets 255 and a file that cannot be opened sets 1, each as it
        // comes, and one that passes changes nothing.
        Ask::Lint { programs, refused } => {
            let mut status = 0;
            if let Some(sentence) = refused {
                let _ = std::io::stdout().lock().write_all(sentence.as_bytes());
                status = 1;
            }
            let engine = engine(&options.ini, None);
            let mut sink = php_sapi::cli::Process;
            for program in programs {
                match php_sapi::cli::lint(&engine, program, &mut sink) {
                    0 => {}
                    ended => status = ended,
                }
            }
            code(status)
        }
    }
}

/// A fresh engine of `php_bundle::enabled()`, every extension in startup
/// order and `php_bundle::Tier` bound, the host's extension after them where
/// it names one, with the line's `-d` entries read by the engine's INI
/// grammar over its own defaults in the order given, and what that
/// reading wrote for `stderr` written there, as php's `ini_error` writes
/// it during `php_module_startup`.
fn engine(ini: &[(String, String)], host: Option<HostExtension>) -> Engine {
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    let mut builder = Engine::builder()
        .extensions(&extensions)
        .tier::<php_bundle::Tier>();
    if let Some(host) = host {
        builder = builder.host_extension(host);
    }
    #[cfg(feature = "witness-trace")]
    if let Some(path) = std::env::var_os(TRACE) {
        builder = builder.trace_natives_to(path.into());
    }
    for (name, value) in ini {
        builder = builder.ini(name, value);
    }
    let engine = builder.build();
    let _ = std::io::stderr()
        .lock()
        .write_all(engine.startup_errors().as_bytes());
    engine
}

/// The variable naming the file a `witness-trace` build appends the
/// natives each request entered to; the witness lane sets it for a fence
/// it serves through the binary (harness §8, revision 61).
#[cfg(feature = "witness-trace")]
const TRACE: &str = "PHP_WITNESS_TRACE";

/// php's exit status as the process's: `exit()` takes the low byte.
fn code(status: i32) -> ExitCode {
    ExitCode::from(u8::try_from(status & 0xff).unwrap_or(255))
}
