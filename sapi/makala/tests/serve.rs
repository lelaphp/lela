//! The binary over a real socket (#2339): started on a port the system
//! picks, sent HTTP/1.1 requests as bytes, and held to what a client
//! reads back, the status, the header lines and the body, and to what
//! the PHP program saw.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use common::{Server, read_response, root, unchunked};

/// A `GET` is PHP's response over HTTP/1.1: the code and the header lines
/// the program set, php's own `X-Powered-By` and `Content-type`, the
/// server's `Date`, and the body the program wrote, chunked, since it
/// set no length; the program served under the host's name, with its
/// query string and `$_SERVER` as the request described them, with
/// neither `pcntl` nor `posix` loaded, and `PHP_ZTS` and
/// `ZEND_THREAD_SAFE` false, as #2335 ruled.
#[test]
fn a_get_is_answered_with_php_s_status_headers_and_body() {
    let root = root(
        "get",
        &[(
            "index.php",
            "<?php http_response_code(201); header('X-Test: yes'); \
             echo PHP_SAPI, ' ', $_GET['x'], ' ', $_SERVER['SERVER_SOFTWARE'], ' ', \
             $_SERVER['REQUEST_METHOD'], ' ', $_SERVER['SCRIPT_NAME'], ' ', \
             var_export(extension_loaded('pcntl') || extension_loaded('posix'), true), ' ', \
             var_export([PHP_ZTS, ZEND_THREAD_SAFE, php_sapi_name()], true);",
        )],
    );
    let server = Server::start(&root);
    let mut stream = server.connect();
    stream
        .write_all(b"GET /?x=1 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap_or_else(|e| panic!("{e}"));
    let response = read_response(&mut stream);
    assert_eq!(response.status, "HTTP/1.1 201 Created");
    assert_eq!(response.header("X-Powered-By"), Some("PHP/8.5.9"));
    assert_eq!(response.header("X-Test"), Some("yes"));
    assert_eq!(
        response.header("Content-type"),
        Some("text/html; charset=UTF-8")
    );
    assert!(response.header("Date").is_some(), "{response:?}");
    assert_eq!(
        String::from_utf8_lossy(&response.body),
        "makala 1 makala/0.0.0 GET /index.php false array (\n  0 => false,\n  1 => false,\n  2 => 'makala',\n)"
    );
}

/// A path that names nothing under the root is a 404, a `.php` one
/// among them, and so is one that climbs out of the root to a file beside
/// it; a file that is not PHP is served as itself, never run.
#[test]
fn a_path_with_no_php_file_is_not_served_as_a_program() {
    let root = root("missing", &[("notes.txt", "<?php echo 'ran';")]);
    let outside = Path::new(env!("CARGO_TARGET_TMPDIR")).join("makala-outside.php");
    std::fs::write(&outside, "<?php echo 'escaped';").unwrap_or_else(|e| panic!("{e}"));
    let server = Server::start(&root);
    for (path, status, body) in [
        ("/nothing.php", "HTTP/1.1 404 Not Found", ""),
        ("/notes.txt", "HTTP/1.1 200 OK", "<?php echo 'ran';"),
        ("/../makala-outside.php", "HTTP/1.1 404 Not Found", ""),
    ] {
        let mut stream = server.connect();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let response = read_response(&mut stream);
        assert_eq!(response.status, status, "{path}");
        assert_eq!(String::from_utf8_lossy(&response.body), body, "{path}");
    }
}

/// The path past the program is its `PATH_INFO`, split by `php -S`'s
/// walk (#2637): `SCRIPT_NAME` and `SCRIPT_FILENAME` the program's,
/// `PHP_SELF` the two, an empty `PATH_INFO` where nothing is past it, and
/// no `PATH_TRANSLATED`, which `php -S` sets none of. A program named
/// past a file that is not one is not found, and nothing runs: the walk
/// that routes is the walk that splits.
#[test]
fn the_path_past_the_program_is_its_path_info() {
    let root = root(
        "path-info",
        &[(
            "front.php",
            "<?php foreach (['SCRIPT_NAME', 'PATH_INFO', 'PHP_SELF', 'SCRIPT_FILENAME', 'PATH_TRANSLATED'] as $n) \
             echo $n, '=', $_SERVER[$n] ?? '(none)', \"\\n\";",
        )],
    );
    std::fs::create_dir_all(root.join("upload")).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(root.join("upload/x.jpg"), "<?php echo 'ran';")
        .unwrap_or_else(|e| panic!("{e}"));
    let filename = root
        .canonicalize()
        .unwrap_or_else(|e| panic!("{e}"))
        .join("front.php");
    let filename = filename.display();
    let server = Server::start(&root);
    for (target, status, body) in [
        (
            "/front.php/index.php?x=1",
            "HTTP/1.1 200 OK",
            format!(
                "SCRIPT_NAME=/front.php\nPATH_INFO=/index.php\nPHP_SELF=/front.php/index.php\nSCRIPT_FILENAME={filename}\nPATH_TRANSLATED=(none)\n"
            ),
        ),
        (
            "/front.php",
            "HTTP/1.1 200 OK",
            format!(
                "SCRIPT_NAME=/front.php\nPATH_INFO=\nPHP_SELF=/front.php\nSCRIPT_FILENAME={filename}\nPATH_TRANSLATED=(none)\n"
            ),
        ),
        (
            "/upload/x.jpg/y.php",
            "HTTP/1.1 404 Not Found",
            String::new(),
        ),
        ("/upload/x.jpg/", "HTTP/1.1 404 Not Found", String::new()),
    ] {
        let mut stream = server.connect();
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let response = read_response(&mut stream);
        assert_eq!(response.status, status, "{target}");
        assert_eq!(String::from_utf8_lossy(&response.body), body, "{target}");
    }
}

/// A body reaches `php://input` through the stream while the program
/// runs (#2338): the program answers before it reads the input, and the
/// client sends the second half of the body only once it has that answer,
/// so a server that read the body before running the program would wait
/// on the client forever, which the read's timeout fails. The request is
/// a `PUT`, since php's default reader swallows a `POST`'s body of any
/// type at request startup (`php_default_post_reader`,
/// `Z/main/php_content_types.c:35`).
#[test]
fn a_post_body_reaches_php_input_through_the_stream() {
    let root = root(
        "post",
        &[(
            "post.php",
            "<?php echo \"ready\\n\"; flush(); $in = file_get_contents('php://input'); \
             echo strlen($in), ' ', $in === str_repeat('y', 40000) ? 'same' : 'differs', ' ', count($_POST);",
        )],
    );
    let server = Server::start(&root);
    let body = vec![b'y'; 40_000];
    let mut stream = server.connect();
    write!(
        stream,
        "PUT /post.php HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Content-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let (first, rest) = body.split_at(10_000);
    stream.write_all(first).unwrap_or_else(|e| panic!("{e}"));
    let mut seen = Vec::new();
    let mut buf = [0u8; 1024];
    while !seen.windows(6).any(|w| w == b"ready\n") {
        let n = stream.read(&mut buf).unwrap_or_else(|e| panic!("{e}"));
        assert!(n > 0, "closed before the program answered");
        seen.extend_from_slice(&buf[..n]);
    }
    stream.write_all(rest).unwrap_or_else(|e| panic!("{e}"));
    stream
        .read_to_end(&mut seen)
        .unwrap_or_else(|e| panic!("{e}"));
    let mut whole = seen.as_slice();
    let response = {
        let end = whole
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or_else(|| panic!("no head"));
        let chunked = String::from_utf8_lossy(&whole[..end])
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked");
        whole = &whole[end + 4..];
        if chunked {
            unchunked(whole)
        } else {
            whole.to_vec()
        }
    };
    assert_eq!(String::from_utf8_lossy(&response), "ready\n40000 same 0");
}

/// A form `POST` that arrives in two writes with a pause between, past
/// `SAPI_POST_BLOCK_SIZE`, is `$_POST` and `php://input` whole, read
/// through the stream at request startup as php's form reader reads it.
#[test]
fn a_form_post_is_post_and_input_whole() {
    let root = root(
        "form",
        &[(
            "form.php",
            "<?php $in = file_get_contents('php://input'); \
             echo strlen($in), ' ', $_POST['a'], ' ', strlen($_POST['big']), ' ', \
             $in === 'a=1&big=' . str_repeat('y', 40000) ? 'same' : 'differs';",
        )],
    );
    let server = Server::start(&root);
    let mut body = b"a=1&big=".to_vec();
    body.resize(body.len() + 40_000, b'y');
    let mut stream = server.connect();
    write!(
        stream,
        "POST /form.php HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let (first, rest) = body.split_at(10_000);
    stream.write_all(first).unwrap_or_else(|e| panic!("{e}"));
    std::thread::sleep(Duration::from_millis(200));
    stream.write_all(rest).unwrap_or_else(|e| panic!("{e}"));
    let response = read_response(&mut stream);
    assert_eq!(response.status, "HTTP/1.1 200 OK");
    assert_eq!(
        String::from_utf8_lossy(&response.body),
        "40008 1 40000 same"
    );
}

/// `connection_aborted()` turns true on the write after the client has
/// gone, and not before (#2337, user story 23): the program writes once,
/// waits until the client has read that and closed, reads
/// `connection_aborted()` with no write since, and then writes until a
/// write finds the client gone, under `ignore_user_abort(true)` so it
/// runs on to say so.
#[test]
fn connection_aborted_turns_true_on_the_write_after_the_client_closes() {
    let root = root(
        "aborted",
        &[(
            "abort.php",
            "<?php ignore_user_abort(true); echo 'first'; flush(); \
             for ($i = 0; $i < 500 && !file_exists(__DIR__ . '/closed'); $i++) usleep(10000); \
             $before = connection_aborted(); \
             for ($w = 0; $w < 500 && !connection_aborted(); $w++) { echo str_repeat('x', 65536); flush(); usleep(1000); } \
             file_put_contents(__DIR__ . '/log', $before . ' ' . connection_aborted() . ' ' . ($w < 500 ? 'early' : 'late'));",
        )],
    );
    let server = Server::start(&root);
    let mut stream = server.connect();
    stream
        .write_all(b"GET /abort.php HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap_or_else(|e| panic!("{e}"));
    let mut seen = Vec::new();
    let mut buf = [0u8; 1024];
    while !seen.windows(5).any(|w| w == b"first") {
        let n = stream.read(&mut buf).unwrap_or_else(|e| panic!("{e}"));
        assert!(n > 0, "closed before the first write");
        seen.extend_from_slice(&buf[..n]);
    }
    drop(stream);
    std::fs::write(root.join("closed"), "").unwrap_or_else(|e| panic!("{e}"));
    let log = root.join("log");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !log.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let logged = std::fs::read_to_string(&log).unwrap_or_else(|e| panic!("the log: {e}"));
    assert_eq!(logged, "0 1 early");
    drop(server);
}

/// The header fields reach `$_SERVER` as nginx passes them to php-fpm
/// (#2347): a name with `_` in it left out, as nginx's
/// `underscores_in_headers off` ignores it, so `X_Forwarded_For` cannot
/// pose as `X-Forwarded-For`; a name sent twice joined with `, `, a
/// cookie's with `; `; and the fields first, newest first, as php-fpm
/// registers them.
#[test]
fn header_fields_reach_server_as_nginx_passes_them() {
    let root = root(
        "fields",
        &[(
            "fields.php",
            "<?php foreach ($_SERVER as $k => $v) if (str_starts_with($k, 'HTTP_')) echo \"$k=$v\\n\";",
        )],
    );
    let server = Server::start(&root);
    let mut stream = server.connect();
    stream
        .write_all(
            b"GET /fields.php HTTP/1.1\r\nHost: a\r\nX-Forwarded-For: 10.0.0.1\r\n\
              X_Forwarded_For: 6.6.6.6\r\nCookie: a=1\r\nX-Two: 1\r\nCookie: b=2\r\n\
              X-Two: 2\r\nConnection: close\r\n\r\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));
    let response = read_response(&mut stream);
    assert_eq!(
        String::from_utf8_lossy(&response.body),
        "HTTP_CONNECTION=close\nHTTP_X_TWO=1, 2\nHTTP_COOKIE=a=1; b=2\n\
         HTTP_X_FORWARDED_FOR=10.0.0.1\nHTTP_HOST=a\n"
    );
}

/// Forwarded fields are trusted from no proxy (research/production-server.r2.md
/// §6): a client's `X-Forwarded-For` and `Forwarded` reach `$_SERVER` as
/// fields alone, and `REMOTE_ADDR` stays the peer the socket saw.
#[test]
fn forwarded_fields_are_trusted_from_no_proxy() {
    let root = root(
        "forwarded",
        &[(
            "peer.php",
            "<?php echo $_SERVER['REMOTE_ADDR'], ' ', $_SERVER['HTTP_X_FORWARDED_FOR'], ' ', \
             $_SERVER['HTTP_FORWARDED'], \"\\n\";",
        )],
    );
    let server = Server::start(&root);
    let mut stream = server.connect();
    stream
        .write_all(
            b"GET /peer.php HTTP/1.1\r\nHost: a\r\nX-Forwarded-For: 10.9.9.9\r\n\
              Forwarded: for=10.8.8.8\r\nConnection: close\r\n\r\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));
    let response = read_response(&mut stream);
    assert_eq!(
        String::from_utf8_lossy(&response.body),
        "127.0.0.1 10.9.9.9 for=10.8.8.8\n"
    );
}
