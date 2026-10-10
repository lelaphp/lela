//! makala, the production web server beside `php -S` (#2337), one crate
//! under `sapi/` (ADR-0008 as amended by #2332): a host php does not ship,
//! named for the caves where the Trill symbionts live between hosts, as
//! the engine's compiled units live in one process while requests come
//! and go.
//!
//! One process, two sides (research/production-server.r2.md §4.10, M2 and
//! M4). The connection side ([`connection`]) is hyper's HTTP/1.1 server on
//! a one-thread tokio runtime: it owns every socket, the framing and the
//! timeouts, and never runs PHP. The request side ([`request`]) is a
//! bounded pool of request threads, each request through its own
//! `Engine::serve`, one engine shared by every thread (design §5.14).
//! Between them a bounded queue carries whole requests, and bounded
//! channels carry each request's body, which the request reads as it runs
//! (`Body::Stream`, #2338), and its response, which the host's sink writes
//! as the request writes it. Nothing stays booted across requests: each
//! starts fresh, as under php-fpm.
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

mod connection;
mod framing;
pub mod options;
mod request;
mod socket;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use php_sapi::Engine;

use crate::options::Options;

/// `sapi_module.name`: what `php_sapi_name()` answers (#2335).
pub const NAME: &str = "makala";

/// `SERVER_SOFTWARE`, `makala/<version>` (#2335).
pub const SERVER_SOFTWARE: &str = concat!("makala/", env!("CARGO_PKG_VERSION"));

/// The host's `main`: the line read into its [`Options`], one engine of
/// the bundle's enabled set started, the listening socket bound, the
/// request threads started, and the connection side run on this thread
/// for as long as the process serves (the drain that ends it is #2344's).
pub fn main() -> ExitCode {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    let options = match Options::parse(&argv) {
        Ok(options) => options,
        Err(refusal) => {
            say(&format!("makala: {refusal}\n{}", options::usage()));
            return php_sapi::exit_code(1);
        }
    };
    let engine = engine(&options);
    let listener = match std::net::TcpListener::bind(options.listen) {
        Ok(listener) => listener,
        Err(e) => {
            say(&format!(
                "makala: cannot listen on {}: {e}\n",
                options.listen
            ));
            return php_sapi::exit_code(1);
        }
    };
    let (queue, pool) = match request::Pool::start(&options, request::serving(&engine, &options)) {
        Ok(started) => started,
        Err(e) => {
            say(&format!("makala: cannot start a request thread: {e}\n"));
            return php_sapi::exit_code(1);
        }
    };
    if let Ok(local) = listener.local_addr() {
        say(&format!(
            "{SERVER_SOFTWARE} listening on http://{local}, document root {}\n",
            options.document_root.display()
        ));
    }
    let ended = connection::run(&options, listener, queue);
    pool.join();
    match ended {
        Ok(()) => php_sapi::exit_code(0),
        Err(e) => {
            say(&format!("makala: {e}\n"));
            php_sapi::exit_code(1)
        }
    }
}

/// One engine of the bundle's enabled set, under the host's name, with the
/// ini files the line's `-n` and `-c` leave and its `-d` entries over
/// them; a startup php refuses ends the process here, before the socket
/// is bound ([`php_sapi::start`]).
fn engine(options: &Options) -> Engine {
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    let builder = Engine::builder()
        .ini_files(php_sapi::ini::search(NAME, &options.ini_search, true))
        .sapi_name(NAME)
        .extensions(&extensions)
        .configure(php_bundle::CONFIGURE)
        .tier::<php_bundle::Tier>();
    php_sapi::start(builder, &options.ini, log)
}

/// The error log, until the server's own (#2345): one entry on the
/// process's stderr, whole, with `\n` after it.
pub(crate) fn log(entry: &[u8]) {
    let mut stderr = std::io::stderr().lock();
    stderr.write_all(entry).ok();
    stderr.write_all(b"\n").ok();
}

/// A line of the server's own on stderr.
fn say(text: &str) {
    std::io::stderr().lock().write_all(text.as_bytes()).ok();
}
