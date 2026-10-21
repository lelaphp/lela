//! The binary's bundle and its start: the CLI's extensions without
//! `pcntl` and `posix`, which no host that serves requests on threads
//! carries (design §7.2), and a startup php refuses ending the process
//! before it binds its socket.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use std::process::{Command, Stdio};

use common::{Request, Server, read_response};

/// The engine makala starts carries neither `pcntl` nor `posix`, though
/// the build this test runs under turns both features on, as a build of
/// the workspace beside the CLI does (`[dev-dependencies]`): the host
/// leaves them out as a host that serves on threads
/// (`php_bundle::Serving::Threads`, design §7.2), and its configure line
/// says so.
#[test]
fn a_build_with_pcntl_and_posix_on_serves_without_them() {
    let root = common::root(
        "threaded",
        &[(
            "t.php",
            "<?php var_export([extension_loaded('pcntl'), extension_loaded('posix'), \
             function_exists('pcntl_fork'), function_exists('posix_getpid')]); \
             ob_start(); phpinfo(INFO_GENERAL); $info = ob_get_clean(); \
             echo ' ', str_contains($info, '--enable-pcntl') ? 'pcntl' : '-', \
             ' ', str_contains($info, '--disable-posix') ? 'no posix' : '-';",
        )],
    );
    let server = Server::start(&root);
    let mut stream = server.send(&Request::get("/t.php").close());
    let response = read_response(&mut stream);
    assert_eq!(
        String::from_utf8_lossy(&response.body),
        "array (\n  0 => false,\n  1 => false,\n  2 => false,\n  3 => false,\n) - no posix"
    );
}

/// opcache's `accel_post_startup` refuses `opcache.file_cache_only` with
/// no `opcache.file_cache`: the process ends with php's failure before
/// the server binds its socket or says it started.
#[test]
fn a_startup_php_refuses_ends_the_process_before_it_listens() {
    let out = Command::new(env!("CARGO_BIN_EXE_makala"))
        .args([
            "-n",
            "-d",
            "opcache.file_cache_only=1",
            "--listen",
            "127.0.0.1:0",
            "--root",
        ])
        .arg(env!("CARGO_TARGET_TMPDIR"))
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("running makala: {e}"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{out:?}");
    assert!(!stderr.contains("listening on"), "{stderr}");
}

/// A line without the server's address is refused with the usage.
#[test]
fn a_line_without_an_address_is_refused_with_the_usage() {
    let out = Command::new(env!("CARGO_BIN_EXE_makala"))
        .args(["--root", "/"])
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("running makala: {e}"));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "makala: no --listen nor --listen-tls address\n{}",
            php_sapi_makala::options::usage()
        )
    );
    assert!(php_sapi_makala::options::usage().contains("\n  body-data-rate=<rate>\n"));
}
