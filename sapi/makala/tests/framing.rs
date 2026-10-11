//! RFC 9112's framing over a real socket (#2340): each case sent as raw
//! bytes, and held to the responses that come back and to whether the
//! server closes the connection after them, so that no request can be
//! smuggled past the one the server framed.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use common::{Ending, Exchange, Server, eventually};

/// Long enough for any answer on a loaded machine; a server that closes
/// ends the wait at once.
const WAIT: Duration = Duration::from_secs(10);

/// What a short wait for an answer that should not come allows.
const SHORT: Duration = Duration::from_millis(1500);

/// The document root every case is served from: `echo.php` says which
/// request it served and how much body it read.
fn echo_root() -> &'static PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        common::root(
            "framing",
            &[(
                "echo.php",
                "<?php echo 'served ', $_SERVER['REQUEST_URI'], ' ', \
                 strlen(file_get_contents('php://input')), ' ', \
                 ($_SERVER['CONTENT_LENGTH'] ?? '') === '' ? '-' : $_SERVER['CONTENT_LENGTH'], \"\\n\";",
            )],
        )
    })
}

fn server() -> Server {
    Server::start(echo_root())
}

/// The case answered once with `status` and the connection closed after,
/// with no request served where none should be.
fn refused(case: &str, exchange: &Exchange, code: u16) {
    assert_eq!(exchange.codes(), [code], "{case}: {}", exchange.text());
    assert_eq!(
        exchange.ending,
        Ending::Closed,
        "{case}: {}",
        exchange.text()
    );
    assert!(
        !exchange.text().contains("served"),
        "{case}: {}",
        exchange.text()
    );
}

/// `Transfer-Encoding` beside `Content-Length` is read by the transfer
/// coding alone and the connection closed after the response (§6.1, §6.3
/// rule 3): the program sees the chunked body and no `CONTENT_LENGTH`,
/// and what the length would have framed as a second request is never
/// served.
#[test]
fn te_with_cl_is_read_by_te_and_the_connection_closed() {
    let server = server();
    let exchange = server.exchange(
        b"POST /echo.php HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\
          Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n\
          GET /echo.php?second HTTP/1.1\r\nHost: a\r\n\r\n",
        WAIT,
    );
    assert_eq!(exchange.codes(), [200], "{}", exchange.text());
    assert_eq!(
        exchange.response().body,
        b"served /echo.php 5 -\n",
        "{}",
        exchange.text()
    );
    assert!(!exchange.text().contains("second"), "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Closed);
}

/// A transfer coding other than `chunked` last, a `Content-Length` that is
/// not a number, two that differ, and one past what a length can hold
/// each answer 400 and close, before any program runs (§6.3 rules 4, 5).
#[test]
fn chunked_not_final_and_a_bad_content_length_answer_400_and_close() {
    let server = server();
    for (case, head) in [
        ("chunked not final", "Transfer-Encoding: chunked, gzip\r\n"),
        ("identity alone", "Transfer-Encoding: identity\r\n"),
        (
            "chunked twice",
            "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
        ),
        ("signed length", "Content-Length: +5\r\n"),
        ("a list of lengths", "Content-Length: 5, 5\r\n"),
        (
            "differing lengths",
            "Content-Length: 5\r\nContent-Length: 6\r\n",
        ),
        (
            "overflowing length",
            "Content-Length: 99999999999999999999999\r\n",
        ),
    ] {
        let exchange = server.exchange(
            format!("POST /echo.php HTTP/1.1\r\nHost: a\r\n{head}\r\nhello\r\n0\r\n\r\n")
                .as_bytes(),
            WAIT,
        );
        refused(case, &exchange, 400);
    }
}

/// A transfer coding the server does not decode, ahead of `chunked`,
/// answers 501 and closes rather than handing the program a body still
/// coded (§6.1).
#[test]
fn a_transfer_coding_not_understood_answers_501_and_closes() {
    let server = server();
    let exchange = server.exchange(
        b"POST /echo.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\n",
        WAIT,
    );
    refused("gzip, chunked", &exchange, 501);
}

/// A chunked body whose framing breaks before the program answered, a
/// size line ended by a bare LF or a size past what a length can hold,
/// answers 400 and closes, not 200 over a body cut short (§7.1).
#[test]
fn a_broken_chunked_body_answers_400_and_closes() {
    let server = server();
    for (case, body) in [
        ("bare LF after the size", "3\nabc\r\n0\r\n\r\n"),
        ("size overflow", "fffffffffffffffffff\r\nabc\r\n0\r\n\r\n"),
    ] {
        let exchange = server.exchange(
            format!(
                "POST /echo.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n{body}"
            )
            .as_bytes(),
            WAIT,
        );
        refused(case, &exchange, 400);
    }
}

/// A chunked body that ends in a trailer section is refused with 400 and
/// a close: hyper ends the section only at CRLF CRLF and drops what
/// follows a bare LF's blank line inside it, so a request a front end
/// would read after that blank line is swallowed here, and the two would
/// disagree on how many requests the connection carried.
#[test]
fn a_trailer_section_answers_400_and_closes() {
    let server = server();
    for (case, trailers) in [
        (
            "a bare LF's blank line",
            "X: a\n\nGET /echo.php?smuggled HTTP/1.1\r\nHost: a\r\n\r\n",
        ),
        ("a trailer field", "X-Sum: 1\r\n\r\n"),
    ] {
        let exchange = server.exchange(
            format!(
                "POST /echo.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n\
                 1\r\nb\r\n0\r\n{trailers}"
            )
            .as_bytes(),
            WAIT,
        );
        refused(case, &exchange, 400);
    }
}

/// `Transfer-Encoding` on an HTTP/1.0 request is faulty framing, answered
/// 400 and closed (§6.1).
#[test]
fn te_on_http_1_0_answers_400_and_closes() {
    let server = server();
    let exchange = server.exchange(
        b"POST /echo.php HTTP/1.0\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        WAIT,
    );
    refused("TE on HTTP/1.0", &exchange, 400);
}

/// A field folded onto a second line, a bare CR inside a line or ending
/// one, and whitespace between a field's name and its colon are each
/// rejected with 400 and a close (§2.2, §5.1, §5.2).
#[test]
fn obs_fold_bare_cr_and_whitespace_before_the_colon_answer_400() {
    let server = server();
    for (case, head) in [
        ("obs-fold", "X-A: b\r\n c\r\n"),
        ("bare CR in a value", "X-A: b\rc\r\n"),
        ("bare CR ending a line", "X-A: b\r"),
        ("whitespace before the colon", "X-A : b\r\n"),
        ("a tab before the colon", "X-A\t: b\r\n"),
    ] {
        let exchange = server.exchange(
            format!("GET /echo.php HTTP/1.1\r\nHost: a\r\n{head}\r\n").as_bytes(),
            WAIT,
        );
        refused(case, &exchange, 400);
    }
}

/// An HTTP/1.1 request with no `Host`, with two, or with one that is not
/// a host and port answers 400 and closes; HTTP/1.0 needs none (§3.2).
#[test]
fn host_missing_repeated_or_invalid_on_http_1_1_answers_400() {
    let server = server();
    for (case, head) in [
        ("missing", ""),
        ("repeated", "Host: a\r\nHost: b\r\n"),
        ("repeated alike", "Host: a\r\nHost: a\r\n"),
        ("not a host", "Host: a b\r\n"),
        ("a port that is not one", "Host: a:x\r\n"),
        ("userinfo", "Host: u@a\r\n"),
    ] {
        let exchange = server.exchange(
            format!("GET /echo.php HTTP/1.1\r\n{head}\r\n").as_bytes(),
            WAIT,
        );
        refused(case, &exchange, 400);
    }
    for host in [
        "Host: a\r\n",
        "Host: a:8080\r\n",
        "Host: [::1]:80\r\n",
        "Host: \r\n",
    ] {
        let exchange = server.exchange(
            format!("GET /echo.php HTTP/1.1\r\n{host}Connection: close\r\n\r\n").as_bytes(),
            WAIT,
        );
        assert_eq!(exchange.codes(), [200], "{host:?}: {}", exchange.text());
    }
    let exchange = server.exchange(b"GET /echo.php HTTP/1.0\r\n\r\n", WAIT);
    assert_eq!(exchange.codes(), [200], "{}", exchange.text());
    let exchange = server.exchange(
        b"GET /echo.php HTTP/1.0\r\nHost: a\r\nHost: b\r\n\r\n",
        WAIT,
    );
    refused("two hosts on HTTP/1.0", &exchange, 400);
}

/// CL.TE: a front that frames by `Content-Length` would pass the second
/// request along inside the first one's body; this server frames by the
/// transfer coding, serves the first request alone and closes, so the
/// second is never served.
#[test]
fn cl_te_never_serves_the_smuggled_request() {
    let server = server();
    let smuggled = "GET /echo.php?smuggled HTTP/1.1\r\nHost: a\r\n\r\n";
    let body = format!("0\r\n\r\n{smuggled}");
    let exchange = server.exchange(
        format!(
            "POST /echo.php HTTP/1.1\r\nHost: a\r\nContent-Length: {}\r\n\
             Transfer-Encoding: chunked\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
        WAIT,
    );
    assert_eq!(exchange.codes(), [200], "{}", exchange.text());
    assert!(!exchange.text().contains("smuggled"), "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Closed);
}

/// TE.CL: a front that frames by the transfer coding would pass a body
/// that a back end framing by `Content-Length` reads as a second request;
/// this server reads the whole chunked body as the first request's, and
/// closes, so the second is never served.
#[test]
fn te_cl_never_serves_the_smuggled_request() {
    let server = server();
    let smuggled = "GET /echo.php?smuggled HTTP/1.1\r\nHost: a\r\n\r\n";
    let chunk = format!("x{smuggled}");
    let exchange = server.exchange(
        format!(
            "POST /echo.php HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\
             Content-Length: 4\r\n\r\n{:x}\r\n{chunk}\r\n0\r\n\r\n",
            chunk.len()
        )
        .as_bytes(),
        WAIT,
    );
    assert_eq!(exchange.codes(), [200], "{}", exchange.text());
    assert_eq!(
        String::from_utf8_lossy(&exchange.response().body),
        format!("served /echo.php {} -\n", chunk.len()),
    );
    assert!(
        !exchange.text().contains("served /echo.php?smuggled"),
        "{}",
        exchange.text()
    );
    assert_eq!(exchange.ending, Ending::Closed);
}

/// A request the server frames cleanly keeps its connection: the control
/// for every close above.
#[test]
fn a_clean_request_keeps_the_connection_open() {
    let server = server();
    let exchange = server.exchange(b"GET /echo.php HTTP/1.1\r\nHost: a\r\n\r\n", SHORT);
    assert_eq!(exchange.codes(), [200], "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Open);
}

/// A TLS handshake sent to the port that serves HTTP without TLS is
/// closed unanswered, no 400 sent, and the error log says so; the port
/// serves HTTP on (#2343, research/production-server.r2.md M19).
#[test]
fn a_tls_handshake_on_the_clear_port_is_closed_and_logged() {
    let log = echo_root().join("clear-port.log");
    std::fs::remove_file(&log).ok();
    let log_arg = log.to_string_lossy().into_owned();
    let server = Server::with_args(echo_root(), &["--threads", "2", "--error-log", &log_arg]);
    // The first record of a TLS 1.2 ClientHello: a handshake record, the
    // ClientHello's type and length, its version and the start of its
    // random.
    let mut hello = vec![
        0x16, 0x03, 0x01, 0x00, 0x31, 0x01, 0x00, 0x00, 0x2d, 0x03, 0x03,
    ];
    hello.extend([0x5a; 32]);
    let exchange = server.exchange(&hello, WAIT);
    assert_eq!(exchange.bytes, b"", "{}", exchange.text());
    assert_eq!(exchange.ending, Ending::Closed);
    let read = || std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        eventually(WAIT, || read()
            .contains("a TLS handshake came to the clear port")),
        "{}",
        read()
    );
    let next = server.exchange(
        b"GET /echo.php HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n",
        WAIT,
    );
    assert_eq!(next.codes(), [200], "{}", next.text());
}
