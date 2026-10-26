//! `php-linked`, the engine the witness and crosscheck lanes serve a
//! snippet through (harness-design.md §8, revision 84; #2166). The harness
//! once linked this engine into itself, so every edit under `crates/` or
//! `ext/` relinked the binary every hook runs; now it starts this binary
//! and hands it snippets.
//!
//! `php-linked --serve` reads one directory per line on stdin. Each holds
//! a request as files: `script_name`, the program's file, which the
//! harness has written in `cwd`, the directory it is served in, and
//! which is served as `php <script_name>` serves it there, compiled as a
//! file (#2985); `source`, the bytes it holds, which the harness's own
//! stand-ins read; `ini`, one
//! `name=value` per line, laid over the CLI's own defaults as `-d` lays
//! them; and `extended`, `1` where it is compiled with extended info as
//! `-e` builds it. It serves the request under this process's
//! environment, as the CLI does, with a fresh engine of the
//! bundle's `linked` set, the isolation a process of its own gave, and
//! writes `stdout`, `stderr`, `exit` and `entered`, the natives the
//! request entered one per line as the trace file holds them, into the
//! same directory, and `abort` beside them where php's process would
//! have ended on `abort()`, which this one, serving the next, does not
//! (`php_sapi::abort_as_php`, #3515); then it answers `served` on a line of its own and
//! reads the next. A request that panics leaves `panicked` instead, and
//! the binary goes on to the next. End of input ends it. Before the
//! first line it closes every fd beyond its three streams, so it
//! holds no pipe of the harness's but its own two (#2550).
//!
//! A request that forks leaves a child that returns here after its
//! request ends; the child ends then, with its request's status, and
//! never reads a line meant for its parent.
#![deny(unsafe_code)]

use std::fs;
use std::io::{self, BufRead, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::process::ExitCode;

use php_bundle::test::{Capture, engine};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().map(String::as_str).ne(["--serve"]) {
        eprintln!(
            "php-linked: serves the harness; run as `php-linked --serve`, a request directory per line on stdin"
        );
        return ExitCode::from(2);
    }
    // An fd beyond its three streams is one the harness's other
    // threads leaked into it, which it must not hold while idle (#2550).
    php_sapi_cli::close_inherited_fds();
    // Served on the stack the CLI serves on, as `php` serves them.
    php_sapi::main_on_request_stack(served)
}

/// Each line's request served, until the input ends: 0, or 1 where a line
/// could not be read or answered.
fn served() -> i32 {
    let pid = std::process::id();
    let stdout = io::stdout();
    for line in io::stdin().lock().lines() {
        let Ok(dir) = line else {
            return 1;
        };
        let exit = serve(Path::new(&dir));
        if std::process::id() != pid {
            // A child the request forked: its request has ended.
            std::process::exit(exit);
        }
        let mut out = stdout.lock();
        if writeln!(out, "served").and_then(|()| out.flush()).is_err() {
            return 1;
        }
    }
    0
}

/// The CLI's environment, the process's: what the harness started this
/// binary under, the recipe's (#1077).
fn environ() -> Vec<(Vec<u8>, Vec<u8>)> {
    std::env::vars_os()
        .map(|(name, value)| (name.into_encoded_bytes(), value.into_encoded_bytes()))
        .collect()
}

/// The request `dir` holds, served, and what it did written beside it;
/// the status it ended on.
fn serve(dir: &Path) -> i32 {
    let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap_or_default();
    let name = read("script_name");
    let cwd = read("cwd");
    let ini_text = read("ini");
    let extended = read("extended").trim() == "1";
    let ini: Vec<(&str, &str)> = php_sapi::ini::CLI_DEFAULTS
        .iter()
        .chain(&php_sapi::ini::CLI_HARDCODED)
        .map(|&(name, value)| (name, value))
        .chain(ini_text.lines().filter_map(|line| line.split_once('=')))
        .collect();
    let served = panic::catch_unwind(AssertUnwindSafe(|| {
        engine(&ini, extended).serve_capture_traced_in(
            Path::new(cwd.trim_end_matches('\n')),
            name.as_bytes(),
            environ(),
            Capture::default(),
        )
    }));
    let Ok(served) = served else {
        fs::write(dir.join("panicked"), "").ok();
        return 255;
    };
    let entered: String = served
        .entered
        .iter()
        .flat_map(|e| {
            [
                e.krate,
                "\t",
                e.owner.unwrap_or_default(),
                "\t",
                e.name,
                "\n",
            ]
        })
        .collect();
    fs::write(dir.join("stdout"), &served.stdout).ok();
    fs::write(dir.join("stderr"), &served.stderr).ok();
    fs::write(dir.join("entered"), entered).ok();
    fs::write(dir.join("exit"), served.exit_code.to_string()).ok();
    if served.terminated == Some(php_sapi::ProcessEnd::Abort) {
        fs::write(dir.join("abort"), "").ok();
    }
    served.exit_code
}
