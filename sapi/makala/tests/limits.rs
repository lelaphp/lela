//! The limits, timeouts and keep-alive over a real socket: each
//! limit crossed and held to its answer, most of them overridden small on
//! the line so a test crosses them quickly, and the HTTP semantics a
//! client relies on across requests.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::{Ending, Request, Server, eventually, read_until_closed};

/// Long enough for any answer on a loaded machine; a server that closes
/// ends the wait at once.
const WAIT: Duration = Duration::from_secs(20);

/// The document root: `echo.php` says which request it served and how
/// much body it read; `big.php` writes until a write finds the client
/// gone; `head.php` writes and records whether a write was refused.
fn limits_root() -> &'static PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        common::root(
            "limits",
            &[
                (
                    "echo.php",
                    "<?php echo 'served ', $_SERVER['REQUEST_URI'], ' ', \
                     strlen(file_get_contents('php://input')), \"\\n\";",
                ),
                (
                    "big.php",
                    "<?php $piece = str_repeat('x', 65536); \
                     for ($i = 0; $i < 20000 && !connection_aborted(); $i++) { echo $piece; flush(); }",
                ),
                (
                    "head.php",
                    "<?php ignore_user_abort(true); \
                     for ($i = 0; $i < 64; $i++) { echo str_repeat('h', 4096); flush(); } \
                     file_put_contents(__DIR__ . '/head.log', connection_aborted() ? 'aborted' : 'written');",
                ),
                (
                    "redirect.php",
                    "<?php header('Location: /echo.php', true, 302);",
                ),
                (
                    "sized.php",
                    "<?php header('Content-Length: 5'); echo 'sized';",
                ),
            ],
        )
    })
}

/// `count` header fields, `X-0: v` on.
fn fields(count: usize) -> String {
    (0..count).fold(String::new(), |mut fields, n| {
        fields += "X-";
        fields += &n.to_string();
        fields += ": v\r\n";
        fields
    })
}

fn server(args: &[&str]) -> Server {
    Server::with_args(limits_root(), args)
}

/// A request line past the limit answers 414 and closes, at the default
/// of 8 KB and at one the line overrides; a line under it is served.
#[test]
fn a_request_line_past_its_limit_answers_414() {
    let server = server(&[]);
    let long = format!(
        "GET /echo.php?{} HTTP/1.1\r\nHost: a\r\n\r\n",
        "a".repeat(9000)
    );
    let exchange = server.exchange(long.as_bytes(), WAIT);
    assert_eq!(exchange.codes(), [414], "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Closed);

    let server = self::server(&["--limit", "request-line=100"]);
    let line = |n: usize| {
        let target = format!("/echo.php?{}", "a".repeat(n));
        let line = format!("GET {target} HTTP/1.1");
        (
            line.len(),
            format!("{line}\r\nHost: a\r\nConnection: close\r\n\r\n"),
        )
    };
    let (length, under) = line(70);
    assert!(length <= 100);
    assert_eq!(server.exchange(under.as_bytes(), WAIT).codes(), [200]);
    let (length, over) = line(95);
    assert!(length > 100);
    assert_eq!(server.exchange(over.as_bytes(), WAIT).codes(), [414]);
}

/// More header fields than the limit, or more bytes of them, answers 431
/// and closes: 100 fields and 32 KB by default, and a count the line
/// overrides.
#[test]
fn headers_past_their_limits_answer_431() {
    let server = server(&[]);
    let many = fields(101);
    let exchange = server.exchange(
        format!("GET /echo.php HTTP/1.1\r\nHost: a\r\n{many}\r\n").as_bytes(),
        WAIT,
    );
    assert_eq!(exchange.codes(), [431], "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Closed);
    let exchange = server.exchange(
        format!(
            "GET /echo.php HTTP/1.1\r\nHost: a\r\nX-Big: {}\r\n\r\n",
            "v".repeat(33 * 1024)
        )
        .as_bytes(),
        WAIT,
    );
    assert_eq!(exchange.codes(), [431], "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Closed);

    let server = self::server(&["--limit", "header-count=3"]);
    let exchange = server.exchange(
        b"GET /echo.php HTTP/1.1\r\nHost: a\r\nX-1: v\r\nX-2: v\r\nX-3: v\r\n\r\n",
        WAIT,
    );
    assert_eq!(exchange.codes(), [431], "{}", exchange.text());
    let exchange = server.exchange(
        b"GET /echo.php HTTP/1.1\r\nHost: a\r\nX-1: v\r\nConnection: close\r\n\r\n",
        WAIT,
    );
    assert_eq!(exchange.codes(), [200], "{}", exchange.text());
}

/// A body past the limit answers 413 and closes: by its length before a
/// byte of it is read, with no `100 Continue` asked for or not, and as a
/// chunked body crosses it while the program reads it; one at the limit
/// is read whole.
#[test]
fn a_body_past_its_limit_answers_413_before_it_is_read() {
    let server = server(&[]);
    let exchange = server.exchange(
        b"POST /echo.php HTTP/1.1\r\nHost: a\r\nContent-Length: 30000001\r\n\
          Expect: 100-continue\r\n\r\n",
        WAIT,
    );
    assert_eq!(exchange.codes(), [413], "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Closed);

    let server = self::server(&["--limit", "body-bytes=10"]);
    let mut stream = server.connect();
    stream
        .write_all(b"POST /echo.php HTTP/1.1\r\nHost: a\r\nContent-Length: 11\r\n\r\n")
        .unwrap_or_else(|e| panic!("{e}"));
    stream
        .write_all(&[b'b'; 11])
        .unwrap_or_else(|e| panic!("{e}"));
    let exchange = read_until_closed(&mut stream, WAIT);
    assert_eq!(exchange.codes(), [413], "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Closed);
    let exchange = server.exchange(
        b"POST /echo.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n\
          6\r\nbbbbbb\r\n6\r\nbbbbbb\r\n0\r\n\r\n",
        WAIT,
    );
    assert_eq!(exchange.codes(), [413], "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Closed);
    let exchange = server.exchange(
        b"POST /echo.php HTTP/1.1\r\nHost: a\r\nContent-Length: 10\r\nConnection: close\r\n\r\n\
          bbbbbbbbbb",
        WAIT,
    );
    assert_eq!(exchange.codes(), [200], "{}", exchange.text());
    assert!(
        exchange.response().body.ends_with(b" 10\n"),
        "{}",
        exchange.text()
    );
}

/// A head that has not arrived whole when the header timeout passes,
/// timed from its first byte, answers 408 and closes, however its bytes
/// trickle in.
#[test]
fn a_head_past_the_header_timeout_answers_408() {
    let server = server(&["--limit", "header-timeout=500ms"]);
    let started = Instant::now();
    let mut stream = server.connect();
    stream
        .write_all(b"GET /echo.php HTTP/1.1\r\n")
        .unwrap_or_else(|e| panic!("{e}"));
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(200));
        // A client that keeps sending does not keep the head alive.
        stream.write_all(b"X-A: b\r\n").ok();
    }
    let exchange = read_until_closed(&mut stream, WAIT);
    assert_eq!(exchange.codes(), [408], "{}", exchange.text());
    assert!(
        exchange.response().header("Date").is_some(),
        "{}",
        exchange.text()
    );
    assert_eq!(exchange.ending, Ending::Closed);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

/// A head whose start came in one write with the whole request before it,
/// as a pipelining client sends it, is timed by the header timeout from
/// that request's response's end, not by the keep-alive clock:
/// the first request is answered, the second 408 within a few seconds,
/// with a keep-alive limit of a minute.
#[test]
fn a_head_begun_with_the_request_before_it_is_timed_by_the_header_timeout() {
    let server = server(&[
        "--limit",
        "header-timeout=500ms",
        "--limit",
        "keep-alive-idle=60s",
    ]);
    let started = Instant::now();
    let mut stream = server.connect();
    stream
        .write_all(b"GET /echo.php HTTP/1.1\r\nHost: a\r\n\r\nGET /echo.php?second HTTP/1.1\r\nHo")
        .unwrap_or_else(|e| panic!("{e}"));
    let exchange = read_until_closed(&mut stream, WAIT);
    assert_eq!(exchange.codes(), [200, 408], "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Closed);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

/// A kept-alive connection idle past the keep-alive limit is closed with
/// nothing sent; one idle past the header timeout but within the
/// keep-alive limit still serves its next request, the two clocks apart.
#[test]
fn an_idle_connection_closes_at_the_keep_alive_limit_and_not_the_header_timeout() {
    let request = Request::get("/echo.php").bytes();
    let server = server(&["--limit", "keep-alive-idle=500ms"]);
    let mut stream = server.connect();
    stream.write_all(&request).expect("write the request");
    let exchange = read_until_closed(&mut stream, WAIT);
    assert_eq!(exchange.codes(), [200], "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Closed, "{}", exchange.text());

    let server = self::server(&[
        "--limit",
        "keep-alive-idle=20s",
        "--limit",
        "header-timeout=300ms",
    ]);
    let mut stream = server.connect();
    stream.write_all(&request).expect("write the request");
    let first = read_until_closed(&mut stream, Duration::from_millis(1200));
    assert_eq!(first.codes(), [200], "{}", first.text());
    assert_eq!(first.ending, Ending::Open);
    stream
        .write_all(&Request::get("/echo.php?again").close().bytes())
        .expect("write the request");
    let second = read_until_closed(&mut stream, WAIT);
    assert_eq!(second.codes(), [200], "{}", second.text());
    assert!(second.text().contains("again"), "{}", second.text());
}

/// A client that sends its body slower than the minimum rate, past the
/// grace, has its connection closed with a 408, and the request thread
/// that read the body is freed: with a pool of one thread, the next
/// request is served (a request blocked on the body channel is freed by
/// the connection dropping its end).
#[test]
fn a_body_slower_than_the_minimum_rate_is_cut_and_frees_its_thread() {
    let server = server(&["--threads", "1", "--limit", "body-data-rate=1000,1s"]);
    let mut slow = server.connect();
    slow.write_all(b"POST /echo.php HTTP/1.1\r\nHost: a\r\nContent-Length: 100000\r\n\r\n")
        .unwrap_or_else(|e| panic!("{e}"));
    slow.write_all(&[b'b'; 100])
        .unwrap_or_else(|e| panic!("{e}"));
    let exchange = read_until_closed(&mut slow, WAIT);
    assert_eq!(exchange.codes(), [408], "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Closed);
    let next = server.exchange(&Request::get("/echo.php?next").close().bytes(), WAIT);
    assert_eq!(next.codes(), [200], "{}", next.text());
}

/// A client that stops reading the response has its connection cut once
/// it takes less than the minimum rate past the grace, and the request
/// thread blocked writing is freed, its next write refused: with a pool
/// of one thread, the next request is served.
#[test]
fn a_reader_slower_than_the_minimum_rate_is_cut_and_frees_its_thread() {
    let server = server(&["--threads", "1", "--limit", "response-data-rate=4000000,1s"]);
    let stalled = server.send(&Request::get("/big.php"));
    let mut next = server.send(&Request::get("/echo.php?next").close());
    let exchange = read_until_closed(&mut next, Duration::from_secs(40));
    assert_eq!(exchange.codes(), [200], "{}", exchange.text());
    assert_eq!(exchange.response().body, b"served /echo.php?next 0\n");
    drop(stalled);
}

/// Keep-alive serves requests sent together on one connection in their
/// order, and stops at the per-connection cap the line sets: the last
/// request it serves says `Connection: close`, and the one after it is
/// never served.
#[test]
fn keep_alive_answers_in_order_and_stops_at_the_per_connection_cap() {
    let request = |n| format!("GET /echo.php?{n} HTTP/1.1\r\nHost: a\r\n\r\n");
    let requests = [request(1), request(2), request(3)].concat();
    let server = server(&[]);
    let exchange = server.exchange(requests.as_bytes(), Duration::from_millis(1500));
    assert_eq!(exchange.codes(), [200, 200, 200], "{}", exchange.text());
    let text = exchange.text();
    let order: Vec<usize> = (1..=3)
        .map(|n| text.find(&format!("?{n}")).unwrap_or(usize::MAX))
        .collect();
    assert!(order.is_sorted() && order[2] != usize::MAX, "{text}");
    assert_eq!(exchange.ending, Ending::Open);

    let server = self::server(&["--limit", "requests-per-connection=2"]);
    let exchange = server.exchange(requests.as_bytes(), WAIT);
    assert_eq!(exchange.codes(), [200, 200], "{}", exchange.text());
    assert!(exchange.text().contains("?2"), "{}", exchange.text());
    assert!(!exchange.text().contains("?3"), "{}", exchange.text());
    assert!(
        exchange
            .text()
            .to_ascii_lowercase()
            .contains("connection: close"),
        "{}",
        exchange.text()
    );
    assert_eq!(exchange.ending, Ending::Closed);
}

/// `Connection: close` from the client is answered and the connection
/// closed after the response.
#[test]
fn connection_close_is_honoured() {
    let server = server(&[]);
    let exchange = server.exchange(
        b"GET /echo.php HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n\
          GET /echo.php?after HTTP/1.1\r\nHost: a\r\n\r\n",
        WAIT,
    );
    assert_eq!(exchange.codes(), [200], "{}", exchange.text());
    assert!(!exchange.text().contains("after"), "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Closed);
}

/// `HEAD` sends `GET`'s fields and no content; the program runs to its
/// end, what it writes going nowhere, as php's `headers_only` has it, and is
/// never told its client has gone; the connection serves its next
/// request.
#[test]
fn head_sends_no_content_and_the_program_writes_on() {
    let server = server(&[]);
    let log = limits_root().join("head.log");
    std::fs::remove_file(&log).ok();
    let mut stream = server.send(&Request::new("HEAD", "/head.php"));
    let head = read_until_closed(&mut stream, Duration::from_millis(1500));
    assert_eq!(head.codes(), [200], "{}", head.text());
    assert!(head.text().ends_with("\r\n\r\n"), "{}", head.text());
    assert!(head.response().header("Date").is_some(), "{}", head.text());
    assert!(eventually(WAIT, || log.exists()));
    assert_eq!(
        std::fs::read_to_string(&log).unwrap_or_else(|e| panic!("{e}")),
        "written"
    );
    stream
        .write_all(&Request::get("/echo.php?after").close().bytes())
        .expect("write the request");
    let after = read_until_closed(&mut stream, WAIT);
    assert_eq!(after.codes(), [200], "{}", after.text());
}

/// `Expect: 100-continue` is answered with `100 Continue` before the
/// client sends the body, which the program then reads whole.
#[test]
fn expect_100_continue_is_answered_before_the_body_is_read() {
    let server = server(&[]);
    let mut stream = server.connect();
    stream
        .write_all(
            b"POST /echo.php HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\n\
              Expect: 100-continue\r\nConnection: close\r\n\r\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));
    let mut seen = Vec::new();
    let mut buf = [0u8; 256];
    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).unwrap_or_else(|e| panic!("{e}"));
        assert!(n > 0, "closed before 100 Continue");
        seen.extend_from_slice(&buf[..n]);
    }
    assert!(
        seen.starts_with(b"HTTP/1.1 100 Continue\r\n"),
        "{}",
        String::from_utf8_lossy(&seen)
    );
    stream.write_all(b"hello").unwrap_or_else(|e| panic!("{e}"));
    let rest = read_until_closed(&mut stream, WAIT);
    assert_eq!(rest.codes(), [200], "{}", rest.text());
    assert!(rest.response().body.ends_with(b" 5\n"), "{}", rest.text());
}

/// `Date` is on every response: PHP's 200, 302 and 404, the server's own
/// 400, and the parser's 431.
#[test]
fn date_is_on_every_response() {
    let server = server(&[]);
    for (case, request) in [
        ("200", Request::get("/echo.php").close().bytes()),
        ("404", Request::get("/none.php").close().bytes()),
        ("302", Request::get("/redirect.php").close().bytes()),
        ("400", b"GET /echo.php HTTP/1.1\r\n\r\n".to_vec()),
        (
            "431",
            format!("GET /echo.php HTTP/1.1\r\nHost: a\r\n{}\r\n", fields(101)).into_bytes(),
        ),
    ] {
        let exchange = server.exchange(&request, WAIT);
        assert_eq!(
            exchange.codes().first().map(u16::to_string).as_deref(),
            Some(case)
        );
        assert!(
            exchange.response().header("Date").is_some(),
            "{case}: {}",
            exchange.text()
        );
    }
}

/// A response with no length set is chunked on HTTP/1.1 and delimited by
/// the close on HTTP/1.0; one whose program set a length carries it.
#[test]
fn a_response_without_a_length_is_chunked_on_1_1_and_close_delimited_on_1_0() {
    let server = server(&[]);
    let exchange = server.exchange(&Request::get("/echo.php").close().bytes(), WAIT);
    let response = exchange.response();
    assert_eq!(response.header("Transfer-Encoding"), Some("chunked"));
    assert_eq!(response.body, b"served /echo.php 0\n");

    let exchange = server.exchange(b"GET /echo.php HTTP/1.0\r\n\r\n", WAIT);
    let response = exchange.response();
    assert_eq!(response.status, "HTTP/1.0 200 OK");
    assert_eq!(response.header("Transfer-Encoding"), None);
    assert_eq!(response.header("Content-Length"), None);
    assert_eq!(response.body, b"served /echo.php 0\n");
    assert_eq!(exchange.ending, Ending::Closed);

    let exchange = server.exchange(&Request::get("/sized.php").close().bytes(), WAIT);
    let response = exchange.response();
    assert_eq!(response.header("Content-Length"), Some("5"));
    assert_eq!(response.header("Transfer-Encoding"), None);
    assert_eq!(response.body, b"sized");
}
