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
//! leaves out too; and `-c`, since no php.ini is read (#1176). A slow
//! request's trace is php's: the child stopped and its memory asked for
//! as php asks, and where it opens, the request's frames read out of it
//! from the chain the engine lays and the child publishes in its
//! scoreboard slot ([`slowlog`]; design §5.6, revision 59).

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
mod natives;
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
use std::os::fd::AsRawFd as _;
use std::process::ExitCode;

use php_sapi::getopt::{Getopt, Opt, Taken, opt};
use php_sapi::ini::{Search, define};
use php_sapi::{Engine, HostServes, IniFiles, RequestDescription};

use crate::master::{Args, Became, EXIT_CONFIG, EXIT_OK, EXIT_USAGE, Fpm, Init};

/// One `-d` of the line, its name and value as the argument's bytes
/// ([`define`]).
type Define = (Vec<u8>, Vec<u8>);

/// `OPTIONS`, `fpm_main.c:121-140`, row for row; the `-` row that ends
/// php's table is the slice's end.
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
        php_sapi::PHP_PREFIX
    )
}

/// What `main`'s line set that php-fpm's engine is built under, the one
/// the master starts and each child keeps: the php.ini files found, and
/// `-e`, `use_extended_info`, which compiles with extended info
/// (`Z/sapi/fpm/fpm/fpm_main.c:1651`, `:1775`).
struct Startup {
    files: IniFiles,
    extended_info: bool,
}

/// `main`'s `php_getopt` loop over `argv[1..]`: the `-d` entries so far,
/// `-c` and `-n` onto `search`, `-e` onto `extended_info`, and what is
/// asked, `-m`, `-v`, `-h` and a refused switch answered where they
/// stand, as php answers them before it reads what follows.
fn parse(argv: &[Vec<u8>], search: &mut Search, extended_info: &mut bool) -> (Vec<Define>, Ask) {
    let mut ini = Vec::new();
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
    let mut getopt = Getopt::new(OPTIONS, 0);
    loop {
        match getopt.next(argv) {
            Ok(Taken::End) => break,
            Ok(Taken::Switch { code, value, .. }) => {
                let value = value.unwrap_or_default();
                match code {
                    b'd' => ini.push(define(&value)),
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
                    b'c' => search.path = Some(value),
                    b'n' => search.ignore = true,
                    b'e' => *extended_info = true,
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
/// and `cgi-fcgi` after them, over the files the line found and `-d`'s
/// entries.
///
/// A call of its own, so the builder, two kilobytes moved through each
/// step, stands in a frame that is gone before the first request is
/// served, and not under every frame of it, as the CLI's is (#3209).
#[inline(never)]
fn engine(ini: &[Define], startup: &Startup) -> Engine {
    php_sapi::start(builder(startup), ini, log_message)
}

/// `sapi_cgi_log_message` (`Z/sapi/fpm/fpm/fpm_main.c:629-632`): the entry
/// as a notice of the process's log, after `PHP message: `.
fn log_message(entry: &[u8]) {
    crate::zlog::msg(
        crate::zlog::Level::Notice,
        crate::zlog::Site::at("sapi_cgi_log_message", 631),
        b"PHP message: ",
        entry,
    );
}

/// php-fpm's engine before its entries: its name, the php.ini files the
/// master found, the bundle's enabled set, its tier, and its own
/// `cgi-fcgi` after them ([`natives`]); with extended info where the line
/// carried `-e`.
fn builder(startup: &Startup) -> php_sapi::EngineBuilder {
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    Engine::builder()
        .ini_files(startup.files.clone())
        .sapi_name(worker::NAME)
        .extensions(extensions)
        .configure(php_bundle::CONFIGURE)
        .tier::<php_bundle::Tier>()
        .extended_info(startup.extended_info)
        .additional_extension(natives::extension())
}

/// `text` on stdout.
fn print(text: &str) {
    std::io::stdout().lock().write_all(text.as_bytes()).ok();
}

/// php-fpm's `main`.
pub fn main() -> ExitCode {
    if nix::sys::signal::sigprocmask(
        nix::sys::signal::SigmaskHow::SIG_BLOCK,
        Some(&master::block_set(false)),
        None,
    )
    .is_err()
    {
        crate::zlog!(Warning @ main:1594, "Could die in the case of too early reload signal");
    }
    crate::zlog!(Debug @ main:1596, "Blocked some signals");
    // SIGPIPE is the standard library's ignored already, as php ignores it
    // in standalone mode so a closed socket does not kill the process.
    let words: Vec<Vec<u8>> = std::env::args_os()
        .map(std::ffi::OsString::into_encoded_bytes)
        .collect();
    let argv0 = words.first().cloned().unwrap_or_default();
    let mut search = Search::default();
    let mut extended_info = false;
    let (ini, ask) = parse(
        words.get(1..).unwrap_or_default(),
        &mut search,
        &mut extended_info,
    );
    let startup = Startup {
        files: php_sapi::ini::search(worker::NAME, &search, true),
        extended_info,
    };
    let engine = |ini: &[Define]| engine(ini, &startup);
    let status = match ask {
        // `main` starts php before it answers `-h`
        // (`Z/sapi/fpm/fpm/fpm_main.c:1694-1697`), and ends it after,
        // so what C's `stdout` held is written after the usage.
        Ask::Usage(code) => {
            let engine = engine(&ini);
            print(&usage(&argv0));
            drop(engine);
            code
        }
        Ask::Modules => {
            print(&php_sapi::modules(&engine(&ini)));
            EXIT_OK
        }
        Ask::Version => {
            print(&php_sapi::version(worker::NAME, &engine(&ini)));
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
            // A request whose start fails is `FPM_EXIT_SOFTWARE`
            // (`Z/sapi/fpm/fpm/fpm_main.c:1727-1734`).
            let (served, _) =
                engine(&ini).serve(description, Box::new(Stdout), |_| HostServes::Program);
            if served.start_failed {
                crate::master::EXIT_SOFTWARE
            } else {
                EXIT_OK
            }
        }
        Ask::Run(mut args) => {
            args.argv = words;
            run(&ini, &startup, args)
        }
    };
    php_sapi::exit_code(i32::from(status))
}

/// `fpm_init` and `fpm_run`, and a child's requests once this process
/// is one.
fn run(ini: &[Define], startup: &Startup, args: Args) -> u8 {
    let engine = engine(ini, startup);
    let mut fpm = match Fpm::init(&engine, args) {
        Init::Continue(fpm) => fpm,
        Init::ExitOk => return EXIT_OK,
        Init::Error(pipe) => {
            if let Some(pipe) = pipe {
                crate::zlog!(
                    Debug @ main:1793,
                    "Sending \"0\" (error) to parent via fd={}",
                    pipe.as_raw_fd()
                );
                nix::unistd::write(&pipe, &0i32.to_ne_bytes()).ok();
            }
            return EXIT_CONFIG;
        }
    };
    if let Some(pipe) = fpm.send_config_pipe.take() {
        crate::zlog!(
            Debug @ main:1806,
            "Sending \"1\" (OK) to parent via fd={}",
            pipe.as_raw_fd()
        );
        nix::unistd::write(&pipe, &1i32.to_ne_bytes()).ok();
    }
    fpm.engine = Some(engine);
    match fpm.run() {
        Err(Became::Child(pool, slot)) => {
            let worker = worker::init(&mut fpm, pool, slot);
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
        std::io::stdout().lock().write_all(bytes).ok();
        Ok(())
    }

    /// `sapi_cgibin_flush` with no request, `fflush(stdout)`: what C's
    /// `stdout` holds, a startup diagnostic among it, after the page.
    fn flush(&mut self) {
        php_sapi::stdio::flush();
    }

    fn send_headers(&mut self, _head: &php_sapi::ResponseHead) {}

    fn log(&mut self, entry: &[u8]) {
        let mut stderr = std::io::stderr().lock();
        stderr.write_all(entry).ok();
        stderr.write_all(b"\n").ok();
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        std::io::stderr().lock().write_all(bytes).ok();
    }
}
