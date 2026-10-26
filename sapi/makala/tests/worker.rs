//! One request thread, two requests (#3199, out of #3162's reading of
//! CVE-2026-109619): php keeps state in a worker's globals that points
//! into a request's freed heap, the session's upload progress and the
//! last error among it, and the worker's next request reads it before
//! its own start. makala serves every request of a request thread on
//! that thread, as a php-fpm child serves its requests, so a request that
//! ends in the memory limit while a multipart body is read, with the
//! session's upload progress under way, is followed on the same thread
//! by one that must start with nothing of it.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use common::{Request, Server, read_response, root};

/// A multipart body: the session's upload progress named `up`, one file
/// `file` holding `contents`, and after it a form field `big` of `big`
/// bytes.
fn body(file: &str, contents: &str, big: usize) -> Vec<u8> {
    let mut body = format!(
        "--b\r\nContent-Disposition: form-data; name=\"PHP_SESSION_UPLOAD_PROGRESS\"\r\n\r\nup\r\n\
         --b\r\nContent-Disposition: form-data; name=\"{file}\"; filename=\"{file}.txt\"\r\n\
         Content-Type: text/plain\r\n\r\n{contents}\r\n"
    )
    .into_bytes();
    if big > 0 {
        body.extend_from_slice(b"--b\r\nContent-Disposition: form-data; name=\"big\"\r\n\r\n");
        body.resize(body.len() + big, b'x');
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--b--\r\n");
    body
}

/// The first request reaches the memory limit while its body is read,
/// past a file the session's upload progress has written, and ends in
/// the limit's fatal with the thread still serving. The second, on the
/// same and only request thread, starts fresh, by three readings:
///
/// - the session's progress: its array, kept by
///   `session.upload_progress.cleanup=0`, holds the second request's
///   file alone and its length, where progress carried over from the
///   first would hold the first one's file before it;
/// - the diagnostics' carried error: `error_get_last()` reads `NULL` at
///   the program's first line, where a carried error would read the
///   first request's fatal;
/// - the heap's lend slot: the first request's end takes a lend (ext/date
///   frees its zone cache under one), and a debug build's heap, made as
///   the second request starts, senses a lend still standing there and
///   ends the request in an `InternalError` instead of its program.
#[test]
fn a_request_after_one_the_limit_ended_in_its_body_starts_fresh_on_the_same_thread() {
    let sessions = root("worker-sessions", &[]);
    let root = root(
        "worker",
        &[(
            "up.php",
            "<?php $last = error_get_last(); session_start(); \
             $p = $_SESSION['upload_progress_up'] ?? null; \
             var_dump($last); \
             echo json_encode([array_column($p['files'] ?? [], 'field_name'), \
             ($p['content_length'] ?? 0) === (int) $_SERVER['CONTENT_LENGTH'], $p['done'] ?? null]);",
        )],
    );
    let save_path = format!("session.save_path={}", sessions.display());
    let server = Server::with_args(
        &root,
        &[
            "--threads",
            "1",
            "-d",
            "memory_limit=4M",
            "-d",
            "display_errors=1",
            "-d",
            "session.upload_progress.cleanup=0",
            "-d",
            &save_path,
        ],
    );
    let post = |body: Vec<u8>| {
        let mut stream = server.send(
            &Request::post("/up.php")
                .host("localhost")
                .close()
                .field("Cookie", "PHPSESSID=worker3199")
                .field("Content-Type", "multipart/form-data; boundary=b")
                .body(body),
        );
        let response = read_response(&mut stream);
        (
            response.status.clone(),
            String::from_utf8_lossy(&response.body).into_owned(),
        )
    };

    // The limit's fatal, met as the read of `big`'s value grows its
    // buffer on the request's heap and before the program's first line:
    // `Unknown`, line 0, at the figure php-cgi 8.5.12 prints for this
    // body under memory_limit=4M.
    let (status, first) = post(body("first", "one", 5_000_000));
    assert!(
        first.contains(
            "Allowed memory size of 4194304 bytes exhausted (tried to allocate 2097152 bytes) in <b>Unknown</b> on line <b>0</b>"
        ),
        "{status}: {first}"
    );

    // What the first request's progress left: nothing, as php-cgi 8.5.12
    // leaves nothing for this body, the session destroyed at the body's
    // end once its encode under the limit failed. So the second request
    // has no progress of the first to carry over, and what it reads is its
    // own.
    assert!(
        !sessions.join("sess_worker3199").exists(),
        "the first request's session was stored"
    );

    let (status, second) = post(body("second", "two", 0));
    assert_eq!(
        second, "NULL\n[[\"second\"],true,true]",
        "{status}: what the second request's program read"
    );
}

/// A request php 8.5.12's hard memory limit ends sends no head, since a
/// php-fpm worker `_exit(137)`s there (`Z/Zend/zend_alloc.c:519`) before
/// its head or a buffered body is written, and nginx answers the request
/// it was serving with a 502. makala answers it so, read from the
/// request's ending, where a request with no head for any other reason
/// is its own 500; and the thread serves the next request.
#[test]
fn a_request_the_hard_memory_limit_ended_is_answered_502_and_its_thread_serves_the_next() {
    let root = root(
        "worker-hard-limit",
        &[
            (
                "hard.php",
                "<?php ob_start(); header('X-A: 1'); echo 'x'; \
                 hash_pbkdf2('md5', 'p', 's', 1, PHP_INT_MAX);",
            ),
            ("next.php", "<?php echo 'next';"),
        ],
    );
    let server = Server::with_args(&root, &["--threads", "1", "-d", "memory_limit=128M"]);
    let get = |target: &str| {
        let mut stream = server.send(&Request::get(target).host("localhost").close());
        read_response(&mut stream)
    };
    let hard = get("/hard.php");
    assert_eq!(hard.status, "HTTP/1.1 502 Bad Gateway", "{hard:?}");
    assert_eq!(hard.header("x-a"), None, "{hard:?}");
    let next = get("/next.php");
    assert_eq!(next.status, "HTTP/1.1 200 OK", "{next:?}");
    assert_eq!(next.body, b"next");
}
