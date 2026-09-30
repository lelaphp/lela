//! The binary tests its configuration as `-t` asks, and serves a request
//! handed to it as `FastCGI` records under php-fpm's name.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
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

/// A configuration of one static child on `port`.
fn conf(dir: &Path, port: u16) -> PathBuf {
    let path = dir.join("php-fpm.conf");
    std::fs::write(
        &path,
        format!(
            "[global]\nerror_log = {}\n[www]\nlisten = 127.0.0.1:{port}\npm = static\npm.max_children = 1\n",
            dir.join("fpm.log").display()
        ),
    )
    .unwrap();
    path
}

#[test]
fn a_configuration_test_says_the_file_is_sound() {
    let dir = tree("test");
    let path = conf(&dir, 9);
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
fn response(mut stream: TcpStream, script: &Path) -> Vec<u8> {
    let mut params = Vec::new();
    let script = script.display().to_string();
    for (name, value) in [
        ("SCRIPT_FILENAME", script.as_str()),
        ("REQUEST_METHOD", "GET"),
        ("QUERY_STRING", "q=1"),
    ] {
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

#[test]
fn a_request_is_served_by_a_child_under_the_host_s_name() {
    let dir = tree("serve");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let path = conf(&dir, port);
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
    let stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => break Some(stream),
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break None,
        }
    };
    let body = stream.map(|s| response(s, &script)).unwrap_or_default();
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(master.id().to_string())
        .status();
    let _ = master.wait();
    let _ = std::fs::remove_dir_all(&dir);
    let body = String::from_utf8_lossy(&body);
    assert!(body.ends_with("\r\n\r\nfpm-fcgi 1"), "{body:?}");
}
