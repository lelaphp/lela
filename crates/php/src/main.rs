//! `php`, the binary (design §4.1, §4.10): one engine of the bundle's
//! enabled set with the bundle's tier bound, the command line read by
//! the CLI host, and the process ending with the code the request ended
//! with. The ini the line carries is laid over the engine's defaults
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

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use php_sapi::Engine;
use php_sapi::cli::{Ask, Options};

fn main() -> ExitCode {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    let options = match Options::parse(&argv) {
        Ok(options) => options,
        Err(refused) => {
            if refused.on_stderr {
                let _ = std::io::stderr()
                    .lock()
                    .write_all(refused.message.as_bytes());
            } else {
                let _ = std::io::stdout()
                    .lock()
                    .write_all(refused.message.as_bytes());
            }
            return code(refused.exit_code);
        }
    };
    match &options.ask {
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
            let engine = engine(&options);
            let _ = std::io::stdout()
                .lock()
                .write_all(php_sapi::cli::extensions(&engine).as_bytes());
            code(0)
        }
        Ask::Serve(program, arguments) => {
            let engine = engine(&options);
            code(php_sapi::cli::serve(&engine, program, arguments))
        }
    }
}

/// A fresh engine of `php_bundle::enabled()`, every extension in startup
/// order and `php_bundle::Tier` bound, with the line's `-d` entries laid
/// over the engine's own defaults in the order given.
fn engine(options: &Options) -> Engine {
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    let mut builder = Engine::builder()
        .extensions(&extensions)
        .tier::<php_bundle::Tier>();
    for (name, value) in &options.ini {
        builder = builder.ini(name, value);
    }
    builder.build()
}

/// php's exit status as the process's: `exit()` takes the low byte.
fn code(status: i32) -> ExitCode {
    ExitCode::from(u8::try_from(status & 0xff).unwrap_or(255))
}
