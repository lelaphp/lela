//! `zend_test_crash('x')` ends its process as php's does (ruled 2026-10-19):
//! the message on fd 1, then the end by `SIGSEGV`. The
//! process is this test's own binary started again as the request's host, its
//! bytes the process's own as php's CLI writes them
//! (`Engine::serve_through`), beside the zend-test variant `oracle.toml`
//! pins, and the two are held to each other's bytes and terminating
//! signal. The process is the test's `main`, the libtest harness being
//! no place for a request that ends its process.
//!
//! The pair php answers, `x` and `SIGSEGV`, is held as well, so a
//! machine without the oracle still holds the engine to it.

use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::Command;

use php_ext::test::Engine;

/// The variable that makes this binary the request's host.
const CHILD: &str = "ZEND_TEST_CRASH_CHILD";

/// The program both serve.
const SOURCE: &str = "zend_test_crash('x');";

/// What one process wrote on fd 1 and the signal it ended by.
#[derive(Debug, PartialEq, Eq)]
struct Ended {
    stdout: Vec<u8>,
    signal: Option<i32>,
}

fn ended(command: &mut Command) -> Result<Ended, String> {
    let output = command
        .output()
        .map_err(|e| format!("could not run {command:?}: {e}"))?;
    Ok(Ended {
        stdout: output.stdout,
        signal: output.status.signal(),
    })
}

/// The zend-test variant's binary for this platform, as `oracle.toml`'s
/// `[variant.zend-test]` names it.
fn oracle() -> Option<PathBuf> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        _ => return None,
    };
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../oracle.toml");
    let text = std::fs::read_to_string(manifest).ok()?;
    let mut lines = text
        .lines()
        .skip_while(|l| l.trim() != "[variant.zend-test]");
    lines.next()?;
    let row = lines
        .take_while(|l| !l.starts_with('['))
        .find(|l| l.trim_start().starts_with(&format!("{platform} =")))?;
    let path = row.split("path = \"").nth(1)?.split('"').next()?;
    Some(PathBuf::from(path)).filter(|path| path.is_file())
}

fn main() {
    if std::env::var_os(CHILD).is_some() {
        let engine = Engine::builder()
            .extensions(&[php_ext_zend_test::extension()])
            .build();
        engine.serve_through(&format!("<?php {SOURCE}"));
        // The request ended the process; reaching here is the failure.
        std::process::exit(3);
    }
    let failures = held();
    for failure in &failures {
        eprintln!("{failure}");
    }
    if !failures.is_empty() {
        std::process::exit(1);
    }
}

/// What the engine's end missed, against php's known pair and, where the
/// oracle is here, against the oracle's own end.
fn held() -> Vec<String> {
    let mut failures = Vec::new();
    let engine = std::env::current_exe()
        .map_err(|e| format!("this test's binary: {e}"))
        .and_then(|exe| ended(Command::new(exe).env(CHILD, "1")));
    let engine = match engine {
        Ok(engine) => engine,
        Err(failure) => return vec![failure],
    };
    let php = Ended {
        stdout: b"x".to_vec(),
        signal: Some(libc::SIGSEGV),
    };
    if engine != php {
        failures.push(format!("the engine ended {engine:?}, php ends {php:?}"));
    }
    match oracle() {
        Some(binary) => match ended(Command::new(binary).args(["-n", "-r", SOURCE])) {
            Ok(oracle) if oracle != engine => {
                failures.push(format!(
                    "the engine ended {engine:?}, the oracle {oracle:?}"
                ));
            }
            Ok(_) => {}
            Err(failure) => failures.push(failure),
        },
        None => {
            eprintln!("the zend-test oracle is not here; the engine is held to php's pair alone");
        }
    }
    failures
}
