//! The access log and the error log (#2345) over a real socket against
//! the binary: a served request's access line, and a destination that
//! blocks costing lines, counted, and never a response's time. A request's
//! panic in the error log is held where a request can be made to panic,
//! by the containment tests of `src/request.rs`.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Server, eventually, read_response};

/// The access line of a served request: the time, the peer, the request
/// line, the status, the body's bytes in and out, and the time it took.
#[test]
fn a_served_request_has_its_access_line() {
    let root = common::root(
        "access",
        &[(
            "echo.php",
            "<?php echo strtoupper(file_get_contents('php://input'));",
        )],
    );
    let log = root.join("access.log");
    let log_arg = log.to_string_lossy().into_owned();
    let server = Server::with_args(&root, &["--threads", "2", "--access-log", &log_arg]);
    let mut stream = server.connect();
    let peer = stream.local_addr().unwrap_or_else(|e| panic!("{e}"));
    stream
        .write_all(
            b"PUT /echo.php?x=1 HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\
              Content-Length: 5\r\n\r\nhello",
        )
        .unwrap_or_else(|e| panic!("{e}"));
    let response = read_response(&mut stream);
    assert_eq!(response.body, b"HELLO");
    let read = || std::fs::read_to_string(&log).unwrap_or_default();
    assert!(eventually(Duration::from_secs(10), || read().ends_with('\n')));
    let logged = read();
    let words: Vec<&str> = logged.trim_end().split(' ').collect();
    let [
        time,
        peer_logged,
        method,
        target,
        protocol,
        status,
        body_in,
        body_out,
        took,
    ] = words[..]
    else {
        panic!("not an access line: {logged:?}");
    };
    assert!(time.len() == 24 && time.ends_with('Z'), "{time}");
    assert_eq!(peer_logged, peer.to_string());
    assert_eq!(
        [method, target, protocol],
        ["\"PUT", "/echo.php?x=1", "HTTP/1.1\""]
    );
    assert_eq!([status, body_in, body_out], ["200", "5", "5"]);
    let ms: f64 = took
        .strip_suffix("ms")
        .and_then(|ms| ms.parse().ok())
        .unwrap_or_else(|| panic!("not a duration: {took}"));
    assert!(ms > 0.0, "{took}");
}

/// A log destination that blocks, a FIFO nobody has opened to read,
/// costs no response its time: with one line of queue, twenty requests
/// on one connection are each answered at once while the log thread
/// waits on the FIFO, the lines the queue had no room for dropped. Once
/// a reader opens the FIFO the lines the queue held are written, and the
/// error log says how many were dropped.
#[test]
fn a_blocked_log_destination_does_not_delay_responses() {
    let root = common::root("blocked", &[("fast.php", "<?php echo 'fast';")]);
    let fifo = root.join("access.fifo");
    let made = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap_or_else(|e| panic!("running mkfifo: {e}"));
    assert!(made.success(), "mkfifo: {made}");
    let errors = root.join("error.log");
    let (fifo_arg, errors_arg) = (
        fifo.to_string_lossy().into_owned(),
        errors.to_string_lossy().into_owned(),
    );
    let server = Server::with_args(
        &root,
        &[
            "--threads",
            "2",
            "--access-log",
            &fifo_arg,
            "--error-log",
            &errors_arg,
            "--limit",
            "log-lines=1",
        ],
    );
    let mut stream = server.connect();
    let started = Instant::now();
    for n in 0..20 {
        stream
            .write_all(b"GET /fast.php HTTP/1.1\r\nHost: a\r\n\r\n")
            .unwrap_or_else(|e| panic!("{e}"));
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        while !seen.ends_with(b"fast\r\n0\r\n\r\n") {
            let read = stream.read(&mut buf).unwrap_or_else(|e| panic!("{e}"));
            assert!(read > 0, "closed before response {n}");
            seen.extend_from_slice(&buf[..read]);
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "twenty responses took {:?}",
        started.elapsed()
    );
    // The log thread holds one line, waiting on the FIFO's open, and the
    // queue the next; the others were dropped: eighteen, or seventeen
    // where the last response's line was queued only once the reader had
    // opened the FIFO.
    let reader = std::fs::File::open(&fifo).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = BufReader::new(reader).lines();
    for _ in 0..2 {
        let line = lines.next().and_then(Result::ok).unwrap_or_default();
        assert!(
            line.contains("\"GET /fast.php HTTP/1.1\" 200 0 4 "),
            "{line}"
        );
    }
    let said = || std::fs::read_to_string(&errors).unwrap_or_default();
    let dropped = || {
        said().lines().find_map(|line| {
            let (_, rest) = line.split_once("makala: ")?;
            let (n, _) = rest.split_once(" log lines were dropped")?;
            n.parse::<u32>().ok()
        })
    };
    assert!(
        eventually(Duration::from_secs(10), || dropped().is_some()),
        "{}",
        said()
    );
    assert!(matches!(dropped(), Some(17 | 18)), "{}", said());
}

/// Each request php-fpm's loop refuses before it runs a program logs, in
/// the error log, the line the pinned php-fpm writes for it on
/// `FCGI_STDERR`, worded as its `zlog` words it (#2767): a path that names
/// no script, a script past `security.limit_extensions`, a directory named
/// `*.php`, which php's opener leaves `ENOENT`, and a script that may not
/// be read, where the test's user is one the mode refuses.
#[test]
fn each_refused_request_logs_php_fpms_line_for_it() {
    use std::os::unix::fs::PermissionsExt;

    let root = common::root(
        "refusals",
        &[
            ("x.PHP", "<?php echo 'ran';"),
            ("secret.php", "<?php echo 'ran';"),
        ],
    );
    for dir in ["upload", "d.php"] {
        std::fs::create_dir_all(root.join(dir)).unwrap_or_else(|e| panic!("{e}"));
    }
    for file in ["upload/x.jpg", "d.php/index.php"] {
        std::fs::write(root.join(file), "<?php echo 'ran';").unwrap_or_else(|e| panic!("{e}"));
    }
    let secret = root.join("secret.php");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000))
        .unwrap_or_else(|e| panic!("{e}"));
    let refused_to_us = std::fs::File::open(&secret).is_err();
    let root = root.canonicalize().unwrap_or_else(|e| panic!("{e}"));
    let errors = root.join("error.log");
    let errors_arg = errors.to_string_lossy().into_owned();
    let server = Server::with_args(&root, &["--threads", "2", "--error-log", &errors_arg]);
    let shown = root.display();
    let mut refusals = vec![
        ("/nothing.php", "Primary script unknown".to_owned()),
        (
            "/nothing.php/witness.php",
            "Primary script unknown".to_owned(),
        ),
        (
            "/d.php/",
            format!("Unable to open primary script: {shown}/d.php (No such file or directory)"),
        ),
        (
            "/upload/x.jpg/y.php",
            format!(
                "Access to the script '{shown}/upload/x.jpg' has been denied (see security.limit_extensions)"
            ),
        ),
        (
            "/x.PHP",
            format!(
                "Access to the script '{shown}/x.PHP' has been denied (see security.limit_extensions)"
            ),
        ),
    ];
    if refused_to_us {
        refusals.push((
            "/secret.php",
            format!("Unable to open primary script: {shown}/secret.php (Permission denied)"),
        ));
    }
    let logged = || {
        let text = std::fs::read_to_string(&errors).unwrap_or_default();
        text.lines()
            .filter_map(|line| line.split_once(' ').map(|(_, entry)| entry.to_owned()))
            .filter(|entry| !entry.starts_with("makala: "))
            .collect::<Vec<_>>()
    };
    for (n, (target, line)) in refusals.iter().enumerate() {
        let request =
            format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        server.exchange(request.as_bytes(), Duration::from_secs(30));
        assert!(
            eventually(Duration::from_secs(10), || logged().len() > n),
            "{target}: {:?}",
            logged()
        );
        assert_eq!(logged().get(n), Some(line), "{target}");
    }
}
