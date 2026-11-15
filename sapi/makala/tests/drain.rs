//! The drain SIGTERM begins, Ranch's suspend, wait and stop and
//! Kestrel's (research/production-server.r2.md M8, M10), held over a real
//! socket against the binary: the listener closes, a request in flight
//! finishes, an idle keep-alive connection closes, and what is still in
//! flight at the shutdown timeout is aborted as the process ends.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{Ending, Request, Server, eventually, read_until_closed};

/// Long enough for any answer on a loaded machine.
const WAIT: Duration = Duration::from_secs(30);

/// A root whose `slow.php` says it has begun, then sleeps the `ms` its
/// query names before it says it is done, and whose `fast.php` answers
/// at once.
fn root(name: &str) -> PathBuf {
    common::root(
        name,
        &[
            (
                "slow.php",
                "<?php echo \"begun\\n\"; flush(); usleep((int) $_GET['ms'] * 1000); echo 'done';",
            ),
            ("fast.php", "<?php echo 'fast';"),
        ],
    )
}

/// What `stream` reads until `bytes` have come, which the test waits on.
fn read_until(stream: &mut TcpStream, bytes: &[u8]) -> Vec<u8> {
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    while !seen.windows(bytes.len()).any(|w| w == bytes) {
        let n = stream.read(&mut buf).unwrap_or_else(|e| panic!("{e}"));
        assert!(n > 0, "closed before {:?}", String::from_utf8_lossy(bytes));
        seen.extend_from_slice(&buf[..n]);
    }
    seen
}

/// A request for `slow.php` sleeping `ms`, on a connection left open
/// after it, read until the program has begun.
fn begun(server: &Server, ms: u32) -> TcpStream {
    let mut stream = server.send(&Request::get(&format!("/slow.php?ms={ms}")));
    read_until(&mut stream, b"begun\n");
    stream
}

/// A request in flight at SIGTERM is answered whole, its connection then
/// closed although it asked for none to be, and the process ends with
/// status 0 once it has.
#[test]
fn a_request_in_flight_at_sigterm_completes_and_the_process_then_exits() {
    let mut server = Server::start(&root("drain-finish"));
    let mut stream = begun(&server, 1500);
    server.terminate();
    let rest = read_until_closed(&mut stream, WAIT);
    assert_eq!(rest.ending, Ending::Closed, "{}", rest.text());
    assert!(
        rest.text().ends_with("done\r\n0\r\n\r\n"),
        "{:?}",
        rest.text()
    );
    let ended = server.ended_within(WAIT);
    assert!(ended.is_some_and(|status| status.success()), "{ended:?}");
}

/// Once SIGTERM has come the listener is closed, so a new client is
/// refused by the kernel while the server still drains, and the request
/// in flight still completes.
#[test]
fn no_connection_is_accepted_after_sigterm() {
    let mut server = Server::start(&root("drain-refuse"));
    let mut stream = begun(&server, 5000);
    server.terminate();
    assert!(
        eventually(Duration::from_secs(3), || TcpStream::connect(server.addr)
            .is_err()),
        "a connection was still accepted after SIGTERM"
    );
    assert_eq!(
        server.ended_within(Duration::ZERO),
        None,
        "the refusal came only with the process's end"
    );
    let rest = read_until_closed(&mut stream, WAIT);
    assert!(
        rest.text().ends_with("done\r\n0\r\n\r\n"),
        "{:?}",
        rest.text()
    );
}

/// A keep-alive connection idle at SIGTERM is closed then, with nothing
/// sent, while a request on another connection still runs.
#[test]
fn an_idle_keep_alive_connection_is_closed_at_sigterm() {
    let server = Server::start(&root("drain-idle"));
    let mut idle = server.connect();
    idle.write_all(&Request::get("/fast.php").bytes())
        .unwrap_or_else(|e| panic!("{e}"));
    read_until(&mut idle, b"fast\r\n0\r\n\r\n");
    let mut stream = begun(&server, 5000);
    server.terminate();
    let closed = read_until_closed(&mut idle, Duration::from_secs(3));
    assert_eq!(closed.ending, Ending::Closed, "{}", closed.text());
    assert!(closed.bytes.is_empty(), "{}", closed.text());
    let rest = read_until_closed(&mut stream, WAIT);
    assert!(
        rest.text().ends_with("done\r\n0\r\n\r\n"),
        "{:?}",
        rest.text()
    );
}

/// A request still running when the shutdown timeout has passed is
/// aborted: the process ends with status 0 at the deadline, not before
/// it and not when the request would have, and the request's connection
/// ends without the body's end.
#[test]
fn a_request_still_running_at_the_deadline_is_aborted_and_the_process_exits() {
    let mut server =
        Server::with_args(&root("drain-deadline"), &["--limit", "shutdown-timeout=1s"]);
    let mut stream = begun(&server, 20_000);
    let signalled = Instant::now();
    server.terminate();
    let ended = server.ended_within(Duration::from_secs(10));
    let took = signalled.elapsed();
    assert!(ended.is_some_and(|status| status.success()), "{ended:?}");
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_secs(10),
        "ended {took:?} after SIGTERM"
    );
    let rest = read_until_closed(&mut stream, Duration::from_secs(5));
    assert_ne!(rest.ending, Ending::Open, "{}", rest.text());
    assert!(!rest.text().contains("done"), "{:?}", rest.text());
}
