//! The binary tests its configuration as `-t` asks, and serves a request
//! handed to it as `FastCGI` records under php-fpm's name.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A directory of the test's own.
fn tree(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sapi-fpm-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A configuration of one static child listening at `listen`.
fn conf(dir: &Path, listen: &str) -> PathBuf {
    let path = dir.join("php-fpm.conf");
    std::fs::write(
        &path,
        format!(
            "[global]\nerror_log = {}\n[www]\nlisten = {listen}\npm = static\npm.max_children = 1\n",
            dir.join("fpm.log").display()
        ),
    )
    .unwrap();
    path
}

/// Where a pool in `dir` listens: a socket in the test's own directory,
/// which no other process names, so nothing else can listen there before
/// the master does (#1474). A loopback port found free and let go was
/// anyone's until the master bound it, and a request sent there reached
/// whatever took it.
fn socket(dir: &Path) -> PathBuf {
    dir.join("fpm.sock")
}

#[test]
fn a_configuration_test_says_the_file_is_sound() {
    let dir = tree("test");
    let path = conf(&dir, "127.0.0.1:9");
    let out = Command::new(env!("CARGO_BIN_EXE_php-fpm"))
        .args(["-n", "-t", "-y"])
        .arg(&path)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let said = String::from_utf8_lossy(&out.stderr);
    let expected = format!(
        "NOTICE: configuration file {} test is successful",
        path.display()
    );
    assert!(said.contains(&expected), "{out:?}");
    assert!(out.status.success(), "{out:?}");
}

/// One `FastCGI` record of request 1.
fn record(kind: u8, content: &[u8]) -> Vec<u8> {
    let len = u16::try_from(content.len()).unwrap().to_be_bytes();
    let mut out = vec![1, kind, 0, 1, len[0], len[1], 0, 0];
    out.extend_from_slice(content);
    out
}

/// What request 1 writes on `FCGI_STDOUT`, read off `stream` to its end.
fn response(stream: UnixStream, script: &Path) -> Vec<u8> {
    let script = script.display().to_string();
    request(
        stream,
        &[
            ("SCRIPT_FILENAME", script.as_str()),
            ("REQUEST_METHOD", "GET"),
            ("QUERY_STRING", "q=1"),
        ],
    )
}

/// What request 1 with `pairs` as its parameters writes on
/// `FCGI_STDOUT`, read off `stream` to its end.
fn request(mut stream: UnixStream, pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut params = Vec::new();
    for &(name, value) in pairs {
        params.push(u8::try_from(name.len()).unwrap());
        params.push(u8::try_from(value.len()).unwrap());
        params.extend_from_slice(name.as_bytes());
        params.extend_from_slice(value.as_bytes());
    }
    let mut out = record(1, &[0, 1, 0, 0, 0, 0, 0, 0]);
    out.extend(record(4, &params));
    out.extend(record(4, &[]));
    out.extend(record(5, &[]));
    stream.write_all(&out).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut body = Vec::new();
    loop {
        let mut header = [0u8; 8];
        if stream.read_exact(&mut header).is_err() {
            return body;
        }
        let len = usize::from(u16::from_be_bytes([header[4], header[5]]));
        let mut content = vec![0u8; len + usize::from(header[6])];
        stream.read_exact(&mut content).unwrap();
        content.truncate(len);
        match header[1] {
            6 => body.extend_from_slice(&content),
            3 => return body,
            _ => {}
        }
    }
}

/// A connection to the pool listening at `socket`, tried for ten seconds
/// from `started`.
fn connect(socket: &Path, started: Instant) -> Option<UnixStream> {
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Some(stream),
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

#[test]
fn a_request_is_served_by_a_child_under_the_host_s_name() {
    let dir = tree("serve");
    let socket = socket(&dir);
    let path = conf(&dir, &socket.display().to_string());
    let script = dir.join("index.php");
    std::fs::write(&script, "<?php echo PHP_SAPI, ' ', $_GET['q'];").unwrap();
    let mut master = Command::new(env!("CARGO_BIN_EXE_php-fpm"))
        .args(["-n", "-F", "-y"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let body = connect(&socket, started)
        .map(|s| response(s, &script))
        .unwrap_or_default();
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(master.id().to_string())
        .status();
    let _ = master.wait();
    let _ = std::fs::remove_dir_all(&dir);
    let body = String::from_utf8_lossy(&body);
    assert!(body.ends_with("\r\n\r\nfpm-fcgi 1"), "{body:?}");
}

/// A log line with its time, the pids and a child's lifetime masked: the
/// number after `pid`, `child` and `after` is `N`.
fn masked(line: &str) -> String {
    let line = line.split_once("] ").map_or(line, |(_, rest)| rest);
    let mut out: Vec<String> = Vec::new();
    for word in line.split(' ') {
        let numbered = out
            .last()
            .is_some_and(|w| matches!(w.as_str(), "pid" | "child" | "after"));
        let rest = word.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
        if numbered && rest.len() < word.len() {
            out.push(format!("N{rest}"));
        } else {
            out.push(word.to_owned());
        }
    }
    out.join(" ")
}

/// The lines the oracle's php-fpm writes for a static pool of one child
/// at `log_level = debug`, started and sent `SIGQUIT`, times and pids
/// masked (#1382).
const QUIT_AT_DEBUG: &str = "\
DEBUG: pid N, fpm_scoreboard_init_main(), line 38: got clock tick '100'
DEBUG: pid N, fpm_signals_init_main(), line 219: Unblocking all signals
DEBUG: pid N, fpm_socket_af_inet_socket_by_addr(), line 325: Found address for 127.0.0.1, socket opened on 127.0.0.1
DEBUG: pid N, fpm_event_init_main(), line 345: event module is kqueue and 1 fds have been reserved
NOTICE: pid N, fpm_init(), line 83: fpm is running, pid N
DEBUG: pid N, fpm_children_make(), line 449: blocking signals before child birth
DEBUG: pid N, fpm_children_make(), line 473: unblocking signals, child born
DEBUG: pid N, fpm_children_make(), line 479: [pool www] child N started
DEBUG: pid N, fpm_event_loop(), line 373: 3488 bytes have been reserved in SHM
NOTICE: pid N, fpm_event_loop(), line 374: ready to handle connections
DEBUG: pid N, fpm_got_signal(), line 100: received SIGQUIT
NOTICE: pid N, fpm_got_signal(), line 101: Finishing ...
DEBUG: pid N, fpm_pctl(), line 239: switching to 'finishing' state
DEBUG: pid N, fpm_pctl_kill_all(), line 165: [pool www] sending signal 3 SIGQUIT to child N
DEBUG: pid N, fpm_pctl_kill_all(), line 174: 1 child(ren) still alive
DEBUG: pid N, fpm_event_loop(), line 426: event module triggered 1 events
DEBUG: pid N, fpm_pctl_kill_all(), line 165: [pool www] sending signal 15 SIGTERM to child N
DEBUG: pid N, fpm_pctl_kill_all(), line 174: 1 child(ren) still alive
DEBUG: pid N, fpm_got_signal(), line 81: received SIGCHLD
DEBUG: pid N, fpm_event_loop(), line 426: event module triggered 1 events
DEBUG: pid N, fpm_children_bury(), line 300: [pool www] child N exited on signal 15 (SIGTERM) after N seconds from start
NOTICE: pid N, fpm_pctl_exit(), line 69: exiting, bye-bye!
";

#[cfg(target_os = "macos")]
#[test]
fn a_static_pool_at_debug_sent_sigquit_writes_the_oracle_s_lines() {
    let dir = tree("debug");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let path = dir.join("php-fpm.conf");
    let log = dir.join("fpm.log");
    std::fs::write(
        &path,
        format!(
            "[global]\nerror_log = {}\nlog_level = debug\n[www]\nlisten = 127.0.0.1:{port}\npm = static\npm.max_children = 1\n",
            log.display()
        ),
    )
    .unwrap();
    let mut master = Command::new(env!("CARGO_BIN_EXE_php-fpm"))
        .args(["-n", "-F", "-y"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !std::fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("ready to handle connections")
        && started.elapsed() < Duration::from_secs(10)
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = Command::new("kill")
        .arg("-QUIT")
        .arg(master.id().to_string())
        .status();
    let _ = master.wait();
    let written = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    let lines: String = written.lines().map(|l| masked(l) + "\n").collect();
    assert_eq!(lines, QUIT_AT_DEBUG);
}

#[test]
fn a_request_body_file_that_does_not_open_is_a_warning_the_pool_logs() {
    let dir = tree("body-file");
    let socket = socket(&dir);
    let path = dir.join("php-fpm.conf");
    let log = dir.join("fpm.log");
    std::fs::write(
        &path,
        format!(
            "[global]\nerror_log = {}\n[www]\nlisten = {}\npm = static\npm.max_children = 1\ncatch_workers_output = yes\nphp_admin_flag[log_errors] = on\n",
            log.display(),
            socket.display()
        ),
    )
    .unwrap();
    let script = dir.join("index.php");
    std::fs::write(&script, "<?php echo 'ran';").unwrap();
    let mut master = Command::new(env!("CARGO_BIN_EXE_php-fpm"))
        .args(["-n", "-F", "-y"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let script = script.display().to_string();
    let missing = dir.join("missing").display().to_string();
    let body = connect(&socket, Instant::now())
        .map(|s| {
            request(
                s,
                &[
                    ("SCRIPT_FILENAME", script.as_str()),
                    ("REQUEST_METHOD", "POST"),
                    ("CONTENT_TYPE", "application/x-www-form-urlencoded"),
                    ("CONTENT_LENGTH", "3"),
                    ("REQUEST_BODY_FILE", missing.as_str()),
                ],
            )
        })
        .unwrap_or_default();
    let _ = Command::new("kill")
        .arg("-QUIT")
        .arg(master.id().to_string())
        .status();
    let _ = master.wait();
    let written = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    let expected = format!(
        "said into stderr: \"NOTICE: PHP message: PHP Warning:  REQUEST_BODY_FILE: open('{missing}') failed: No such file or directory (2) in Unknown on line 0\""
    );
    assert!(written.contains(&expected), "{written}");
    assert!(String::from_utf8_lossy(&body).ends_with("ran"), "{body:?}");
}
