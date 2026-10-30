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

mod boundary;
mod connection;
mod framing;
mod log;
mod natives;
pub mod options;
mod request;
mod route;
mod socket;
mod static_file;
mod tls;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use php_sapi::Engine;

use crate::options::Options;

/// `sapi_module.name`: what `php_sapi_name()` answers (#2335).
pub const NAME: &str = "makala";

/// `SERVER_SOFTWARE`, `makala/<version>` (#2335).
pub const SERVER_SOFTWARE: &str = concat!("makala/", env!("CARGO_PKG_VERSION"));

/// The host's `main`: the line read into its [`Options`], the TLS
/// certificates read where it serves TLS, one engine of the bundle's
/// enabled set started, the listening sockets bound, the
/// request threads started, and the connection side driven on this thread
/// for as long as the process serves, until the drain SIGTERM begins
/// has ended (#2344). The engine is the last to end, once the request
/// threads have: where one is still in a request at the drain's
/// deadline, the process ends around it, the engine's end never reached, as a
/// php-fpm child killed at its master's timeout ends.
pub fn main() -> ExitCode {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    let options = match Options::parse(&argv) {
        Ok(options) => options,
        Err(refusal) => {
            say(&format!("makala: {refusal}\n{}", options::usage()));
            return php_sapi::exit_code(1);
        }
    };
    // The certificates are read before the engine starts, so a line whose
    // TLS cannot serve ends the process before anything else.
    let tls = match options
        .tls
        .as_ref()
        .map(|tls| Ok::<_, String>((tls.listen, tls::acceptor(tls)?)))
        .transpose()
    {
        Ok(tls) => tls,
        Err(refusal) => {
            say(&format!("makala: {refusal}\n"));
            return php_sapi::exit_code(1);
        }
    };
    let engine = engine(&options);
    // The log thread once the engine has started, so what php logs while
    // it starts, and a startup it refuses, reach stderr before the process
    // ends. The thread is held, never joined, so a destination that
    // blocks holds only it.
    let _log = match log::start(
        options.access_log.clone(),
        options.error_log.clone(),
        options.limits.log_lines,
    ) {
        Ok(thread) => thread,
        Err(e) => {
            say(&format!("makala: cannot start the log thread: {e}\n"));
            return php_sapi::exit_code(1);
        }
    };
    let listeners = match listeners(&options, tls) {
        Ok(listeners) => listeners,
        Err(e) => {
            say(&format!("makala: {e}\n"));
            return php_sapi::exit_code(1);
        }
    };
    let side = match connection::ConnectionSide::new() {
        Ok(side) => side,
        Err(e) => {
            say(&format!("makala: cannot start the connection side: {e}\n"));
            return php_sapi::exit_code(1);
        }
    };
    let router = Arc::new(route::Router::new(&options));
    let (queue, pool) =
        match request::Pool::start(&options, request::serving(&engine, router.root())) {
            Ok(started) => started,
            Err(e) => {
                say(&format!("makala: cannot start a request thread: {e}\n"));
                return php_sapi::exit_code(1);
            }
        };
    if let Ok(urls) = listeners
        .iter()
        .map(connection::Listener::url)
        .collect::<std::io::Result<Vec<_>>>()
    {
        say(&format!(
            "{SERVER_SOFTWARE} listening on {}, document root {}\n",
            urls.join(", "),
            options.document_root.display()
        ));
    }
    let drained = match side.run(options.limits, router, listeners, queue) {
        Ok(drained) => drained,
        Err(e) => {
            say(&format!("makala: {e}\n"));
            return php_sapi::exit_code(1);
        }
    };
    if drained.whole && pool.join_by(drained.deadline) {
        log::error(b"makala: drained");
    } else {
        log::error(
            b"makala: the shutdown timeout passed with requests in flight; they were aborted",
        );
    }
    // The engine's end runs here, where `engine` is the last clone: once
    // every request thread is joined, and never while one is left in a
    // request, which holds a clone of its own. What it logs is written
    // before the process ends, as is every line queued before it.
    drop(engine);
    log::settle(log::SETTLE);
    php_sapi::exit_code(0)
}

/// One engine of the bundle's enabled set, under the host's name, with the
/// ini files the line's `-n` and `-c` leave and its `-d` entries over
/// them; a startup php refuses ends the process here, before the socket
/// is bound ([`php_sapi::start`]).
///
/// An entry no ini sets has the default of the build serving it, as under
/// php-fpm: `include_path` is the build's `PHP_INCLUDE_PATH`, which
/// `DEFAULT_INCLUDE_PATH` says, the engine's `.:` where no layout was
/// named at its build and the pinned php-fpm's keg's PEAR directory there
/// (#1371, #2704).
///
/// ```php
/// var_dump(get_include_path() === DEFAULT_INCLUDE_PATH, ini_get('include_path') === DEFAULT_INCLUDE_PATH);
/// ```
/// ```request makala
/// --GET--
/// q=1
/// ```
/// ```expect capture
/// 200
/// x-powered-by: PHP/8.5.12
/// content-type: text/html; charset=UTF-8
///
/// bool(true)
/// bool(true)
/// ```
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#[allow(rustdoc::invalid_codeblock_attributes)]
fn engine(options: &Options) -> Engine {
    // On threads: never pcntl nor posix, whatever features a build of the
    // workspace unified into php_bundle (design §7.2).
    let extensions: Vec<_> = php_bundle::enabled_for(php_bundle::Serving::Threads)
        .iter()
        .map(|start| start())
        .collect();
    let builder = Engine::builder()
        .ini_files(php_sapi::ini::search(NAME, &options.ini_search, true))
        .sapi_name(NAME)
        .extensions(&extensions)
        .configure(php_bundle::configure_for(php_bundle::Serving::Threads))
        .tier::<php_bundle::Tier>();
    let engine = php_sapi::start(natives::declare(builder), &options.ini, log::error);
    // What the startup displayed written before makala serves anything,
    // rather than held to its exit as php-fpm's buffered stdout holds it.
    php_sapi::stdio::flush();
    engine
}

/// The listening sockets the line asks for, bound: the clear one first,
/// then the TLS one at `tls`'s address with its handshakes.
fn listeners(
    options: &Options,
    tls: Option<(std::net::SocketAddr, tokio_rustls::TlsAcceptor)>,
) -> Result<Vec<connection::Listener>, String> {
    let bind = |addr| {
        std::net::TcpListener::bind(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))
    };
    let mut listeners = Vec::new();
    if let Some(addr) = options.listen {
        listeners.push(connection::Listener {
            socket: bind(addr)?,
            tls: None,
        });
    }
    if let Some((addr, tls)) = tls {
        listeners.push(connection::Listener {
            socket: bind(addr)?,
            tls: Some(tls),
        });
    }
    Ok(listeners)
}

/// A line of the server's own on stderr.
fn say(text: &str) {
    std::io::stderr().lock().write_all(text.as_bytes()).ok();
}
