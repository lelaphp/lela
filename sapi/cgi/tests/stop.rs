//! A `SIGTERM` php-cgi `-b` handles while a request runs: the request is
//! answered, the process then ends with 0, and its socket listens until
//! that end, so a connection made after the stop is queued and reset at
//! the exit, as the pinned php-cgi 8.5.12 queues and resets it. The
//! handler takes the accept loop's number from the socket so a stop
//! handled after the loop's check is not lost (#3869), and keeps the
//! socket itself open; `lines.toml` sends no stop while a request runs,
//! so this half is held here.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

/// One `FastCGI` record of `kind` for request 1.
fn record(kind: u8, content: &[u8]) -> Vec<u8> {
    let len = u16::try_from(content.len()).unwrap().to_be_bytes();
    let mut out = vec![1, kind, 0, 1, len[0], len[1], 0, 0];
    out.extend_from_slice(content);
    out
}

/// A responder's request for `script`, no body.
fn request(script: &Path) -> Vec<u8> {
    let mut params = Vec::new();
    let script = script.to_str().unwrap().as_bytes();
    for (name, value) in [
        (&b"SCRIPT_FILENAME"[..], script),
        (b"REQUEST_METHOD", b"GET"),
    ] {
        params.push(u8::try_from(name.len()).unwrap());
        params.push(u8::try_from(value.len()).unwrap());
        params.extend_from_slice(name);
        params.extend_from_slice(value);
    }
    let mut out = record(1, &[0, 1, 0, 0, 0, 0, 0, 0]);
    out.extend(record(4, &params));
    out.extend(record(4, b""));
    out.extend(record(5, b""));
    out
}

/// The `FCGI_STDOUT` bytes up to `FCGI_END_REQUEST`.
fn answer(conn: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut head = [0u8; 8];
        conn.read_exact(&mut head).unwrap();
        let n = usize::from(u16::from_be_bytes([head[4], head[5]])) + usize::from(head[6]);
        let mut body = vec![0u8; n];
        conn.read_exact(&mut body).unwrap();
        match head[1] {
            6 => out.extend_from_slice(&body[..n - usize::from(head[6])]),
            3 => return out,
            _ => {}
        }
    }
}

/// Waits for `ready` for up to 30 s.
fn until(mut ready: impl FnMut() -> bool) {
    let since = Instant::now();
    while !ready() {
        assert!(since.elapsed() < Duration::from_secs(30), "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_stop_while_a_request_runs_ends_the_process_after_it_with_its_socket_listening() {
    let dir = std::env::temp_dir().join(format!("sapi-cgi-stop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("t.php");
    std::fs::write(
        &script,
        "<?php touch(__DIR__ . '/started'); while (!file_exists(__DIR__ . '/go')) { clearstatcache(); usleep(1000); } echo \"done\\n\";",
    )
    .unwrap();
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut host = std::process::Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .args(["-n", "-b", &format!("127.0.0.1:{port}")])
        .current_dir(&dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = None;
    until(|| {
        first = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).ok();
        first.is_some()
    });
    let mut first = first.unwrap();
    first.write_all(&request(&script)).unwrap();
    until(|| dir.join("started").exists());
    let pid = nix::unistd::Pid::from_raw(i32::try_from(host.id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    // The handler has run once the signal is no longer pending; a
    // connection made then finds the socket listening.
    std::thread::sleep(Duration::from_millis(500));
    let second = TcpStream::connect((Ipv4Addr::LOCALHOST, port));
    std::fs::write(dir.join("go"), "").unwrap();
    let body = answer(&mut first);
    drop(first);
    let mut ended = None;
    until(|| {
        ended = host.try_wait().unwrap();
        ended.is_some()
    });
    let out = host.wait_with_output().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert!(body.ends_with(b"\r\n\r\ndone\n"), "{body:?}");
    assert_eq!(ended.unwrap().code(), Some(0));
    assert_eq!((&out.stdout[..], &out.stderr[..]), (&b""[..], &b""[..]));
    // The process has ended, so its reset has come and the read does not
    // wait.
    let mut second = second.expect("the socket listens until the process ends");
    let read = second.read(&mut [0u8; 8]);
    assert!(
        matches!(&read, Ok(0))
            || matches!(&read, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset),
        "{read:?}"
    );
}
