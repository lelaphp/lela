//! php's `FastCGI` process manager, `php-src/sapi/fpm`, as one crate under
//! `sapi/` (ADR-0008): the binary `php-fpm`, reaching the engine through
//! `php_sapi` alone. `main` reads the command line as `fpm_main.c`'s
//! `main` reads it through `php_getopt`, starts php, and hands the rest to
//! the process manager: the master reads `php-fpm.conf` ([`conf`]), opens
//! each pool's socket ([`sockets`]), maps each pool's scoreboard
//! ([`scoreboard`]) and forks the children each pool's `pm` asks for
//! ([`children`], [`pctl`]), then runs its event loop ([`master`]) until a
//! signal ends it; each child serves `FastCGI` requests on an engine of its
//! own ([`worker`]), its pool's `php_value`s laid over the defaults.
//!
//! What php-fpm has that this host leaves out: ACLs on a UNIX socket,
//! `listen.setfib`, `AppArmor` and systemd, which the oracle's macOS build
//! leaves out too; `-c`, since no php.ini is read (#1176); and the PHP
//! frames of a slow request's trace where the child's memory is readable,
//! which php reads out of it: the child is stopped and its memory asked
//! for as php asks ([`slowlog`]), and the entry carries the request's
//! time, pool, pid and program (#1301).

#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

mod access;
mod children;
mod conf;
mod env;
mod events;
#[allow(unsafe_code)]
mod ffi;
mod glob;
mod master;
mod pctl;
mod pools;
mod scoreboard;
mod slowlog;
mod sockets;
mod status;
mod stdio;
mod unix;
mod worker;
mod zlog;

use std::io::Write;
use std::process::ExitCode;

use php_sapi::getopt::{Getopt, Opt, Taken, opt};
use php_sapi::ini::define;
use php_sapi::{Engine, HostExtension, RequestDescription};

use crate::master::{Args, Became, EXIT_CONFIG, EXIT_OK, EXIT_USAGE, Fpm, Init};

/// `OPTIONS`, `fpm_main.c:121-140`, row for row.
const OPTIONS: &[Opt] = &[
    opt(b'c', 1, "php-ini"),
    opt(b'd', 1, "define"),
    opt(b'e', 0, "profile-info"),
    opt(b'h', 0, "help"),
    opt(b'i', 0, "info"),
    opt(b'm', 0, "modules"),
    opt(b'n', 0, "no-php-ini"),
    opt(b'?', 0, "usage"),
    opt(b'v', 0, "version"),
    opt(b'y', 1, "fpm-config"),
    opt(b't', 0, "test"),
    opt(b'p', 1, "prefix"),
    opt(b'g', 1, "pid"),
    opt(b'R', 0, "allow-to-run-as-root"),
    opt(b'D', 0, "daemonize"),
    opt(b'F', 0, "nodaemonize"),
    opt(b'O', 0, "force-stderr"),
    opt(b'-', 0, ""),
];

/// What the command line asks of php-fpm, `main`'s switch.
enum Ask {
    /// `-h`, `-?` or a switch `php_getopt` refuses: the usage, and the
    /// exit code.
    Usage(u8),
    /// `-m`.
    Modules,
    /// `-v`.
    Version,
    /// `-i`.
    Info,
    /// The process manager.
    Run(Args),
}

/// `php_cgi_usage`, the program named by what follows the last `/` of
/// `argv[0]`, or `php`.
fn usage(argv0: &[u8]) -> String {
    let prog = match argv0.iter().rposition(|b| *b == b'/') {
        Some(at) => String::from_utf8_lossy(&argv0[at + 1..]).into_owned(),
        None => String::from("php"),
    };
    format!(
        "Usage: {prog} [-n] [-e] [-h] [-i] [-m] [-v] [-t] [-p <prefix>] [-g <pid>] [-c <file>] [-d foo[=bar]] [-y <file>] [-D] [-F [-O]]\n\
         \x20 -c <path>|<file> Look for php.ini file in this directory\n\
         \x20 -n               No php.ini file will be used\n\
         \x20 -d foo[=bar]     Define INI entry foo with value 'bar'\n\
         \x20 -e               Generate extended information for debugger/profiler\n\
         \x20 -h               This help\n\
         \x20 -i               PHP information\n\
         \x20 -m               Show compiled in modules\n\
         \x20 -v               Version number\n\
         \x20 -p, --prefix <dir>\n\
         \x20                  Specify alternative prefix path to FastCGI process manager (default: {}).\n\
         \x20 -g, --pid <file>\n\
         \x20                  Specify the PID file location.\n\
         \x20 -y, --fpm-config <file>\n\
         \x20                  Specify alternative path to FastCGI process manager config file.\n\
         \x20 -t, --test       Test FPM configuration and exit\n\
         \x20 -D, --daemonize  force to run in background, and ignore daemonize option from config file\n\
         \x20 -F, --nodaemonize\n\
         \x20                  force to stay in foreground, and ignore daemonize option from config file\n\
         \x20 -O, --force-stderr\n\
         \x20                  force output to stderr in nodaemonize even if stderr is not a TTY\n\
         \x20 -R, --allow-to-run-as-root\n\
         \x20                  Allow pool to run as root (disabled by default)\n",
        conf::PHP_PREFIX
    )
}

/// `main`'s `php_getopt` loop over `argv[1..]`: the `-d` entries so far
/// and what is asked, `-m`, `-v`, `-h` and a refused switch answered where
/// they stand, as php answers them before it reads what follows.
fn parse(argv: &[Vec<u8>]) -> (Vec<(String, String)>, Ask) {
    let mut ini: Vec<(String, String)> = php_sapi::cgi::HOST_INI
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    let mut asked = Args {
        argv: Vec::new(),
        config: None,
        prefix: None,
        pid: None,
        test: 0,
        run_as_root: false,
        daemonize: None,
        force_stderr: false,
    };
    let mut info = false;
    let mut getopt = Getopt::new(OPTIONS);
    loop {
        match getopt.next(argv) {
            Ok(Taken::End) => break,
            Ok(Taken::Switch(code, value)) => {
                let value = value.unwrap_or_default();
                match code {
                    b'd' => ini.push(define(&String::from_utf8_lossy(&value))),
                    b'y' => asked.config = Some(value),
                    b'p' => asked.prefix = Some(value),
                    b'g' => asked.pid = Some(value),
                    b't' => asked.test += 1,
                    b'm' => return (ini, Ask::Modules),
                    b'i' => info = true,
                    b'R' => asked.run_as_root = true,
                    b'D' => asked.daemonize = Some(true),
                    b'F' => asked.daemonize = Some(false),
                    b'O' => asked.force_stderr = true,
                    b'v' => return (ini, Ask::Version),
                    b'c' | b'n' | b'e' => {}
                    _ => return (ini, Ask::Usage(EXIT_OK)),
                }
            }
            Err(_) => return (ini, Ask::Usage(EXIT_USAGE)),
        }
    }
    if info {
        return (ini, Ask::Info);
    }
    if getopt.optind != argv.len() {
        return (ini, Ask::Usage(EXIT_USAGE));
    }
    (ini, Ask::Run(asked))
}

/// `php_module_startup` for php-fpm: the bundle's enabled set, its tier,
/// and `cgi-fcgi` after them, over `-d`'s entries.
fn engine(ini: &[(String, String)]) -> Engine {
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    let builder = Engine::builder()
        .extensions(&extensions)
        .tier::<php_bundle::Tier>();
    php_sapi::start(builder, ini, Some(HostExtension::FpmFcgi))
}

/// `text` on stdout.
fn print(text: &str) {
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
}

/// php-fpm's `main`.
pub fn main() -> ExitCode {
    let _ = nix::sys::signal::sigprocmask(
        nix::sys::signal::SigmaskHow::SIG_BLOCK,
        Some(&master::block_set(false)),
        None,
    );
    // SIGPIPE is the standard library's ignored already, as php ignores it
    // in standalone mode so a closed socket does not kill the process.
    let words: Vec<Vec<u8>> = std::env::args_os()
        .map(std::ffi::OsString::into_encoded_bytes)
        .collect();
    let argv0 = words.first().cloned().unwrap_or_default();
    let (ini, ask) = parse(words.get(1..).unwrap_or_default());
    let status = match ask {
        Ask::Usage(code) => {
            let _ = engine(&ini);
            print(&usage(&argv0));
            code
        }
        Ask::Modules => {
            print(&php_sapi::modules(&engine(&ini)));
            EXIT_OK
        }
        Ask::Version => {
            print(&php_sapi::version(
                worker::NAME,
                env!("PHP_BUILD_EPOCH"),
                &engine(&ini),
            ));
            EXIT_OK
        }
        Ask::Info => {
            let description = RequestDescription {
                sapi_name: String::from(worker::NAME),
                source: Some(b"<?php phpinfo();".to_vec()),
                script_name: Some(b"Standard input code".to_vec()),
                cwd: std::env::current_dir().unwrap_or_default(),
                ..RequestDescription::default()
            };
            let mut sink = Stdout;
            engine(&ini).serve(description, &mut sink);
            EXIT_OK
        }
        Ask::Run(mut args) => {
            args.argv = words;
            run(&ini, args)
        }
    };
    php_sapi::exit_code(i32::from(status))
}

/// `fpm_init` and `fpm_run`, and a child's requests once this process
/// is one.
fn run(ini: &[(String, String)], args: Args) -> u8 {
    let engine = engine(ini);
    let mut fpm = match Fpm::init(&engine, args) {
        Init::Continue(fpm) => fpm,
        Init::ExitOk => return EXIT_OK,
        Init::Error(pipe) => {
            if let Some(pipe) = pipe {
                let _ = nix::unistd::write(&pipe, &0i32.to_ne_bytes());
            }
            return EXIT_CONFIG;
        }
    };
    if let Some(pipe) = fpm.send_config_pipe.take() {
        let _ = nix::unistd::write(&pipe, &1i32.to_ne_bytes());
    }
    let started = engine.startup_errors().to_owned();
    drop(engine);
    match fpm.run() {
        Err(Became::Child(pool, slot)) => {
            let worker = worker::init(&mut fpm, pool, slot, ini, &started);
            drop(fpm);
            worker.run()
        }
        Ok(never) => match never {},
    }
}

/// The process's stdout, for `-i`.
struct Stdout;

impl php_sapi::OutputSink for Stdout {
    fn write(&mut self, bytes: &[u8]) -> Result<(), php_sapi::Aborted> {
        let _ = std::io::stdout().lock().write_all(bytes);
        Ok(())
    }

    fn flush(&mut self) {
        let _ = std::io::stdout().lock().flush();
    }

    fn send_headers(&mut self, _head: &php_sapi::ResponseHead) {}

    fn log(&mut self, entry: &[u8]) {
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(entry);
        let _ = stderr.write_all(b"\n");
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        let _ = std::io::stderr().lock().write_all(bytes);
    }
}
