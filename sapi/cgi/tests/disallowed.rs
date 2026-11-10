//! A web server `FCGI_WEB_SERVER_ADDRS` does not allow: php-cgi `-b`
//! closes its connection and says so on stderr as `fcgi_log` says it, with
//! no newline after it (`Z/main/fastcgi.c:1402`), after the list's own
//! word for each entry that is no address (#4398). `lines.toml` cannot
//! hold a connection the host drops, so this half is held here, to the
//! bytes the pinned php-cgi 8.5.12 (md5 f4f783bf54d2bfa24a558693f7c12188)
//! wrote on 2026-11-10 for this list and one connection from 127.0.0.1
//! that sent nothing.

use std::io::Read as _;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// What the pinned php-cgi wrote on stderr, whole.
const PINNED: &[u8] = b"Wrong IP address 'bogus' in listen.allowed_clientsConnection disallowed: IP address '127.0.0.1' has been dropped.";

#[test]
fn a_peer_the_list_does_not_allow_is_dropped_and_said_with_php_s_bytes() {
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let host = std::process::Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .args(["-n", "-b", &format!("127.0.0.1:{port}")])
        .env("FCGI_WEB_SERVER_ADDRS", "bogus,192.0.2.1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let since = Instant::now();
    let mut conn = loop {
        if let Ok(conn) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
            break conn;
        }
        assert!(since.elapsed() < Duration::from_secs(30), "timed out");
        std::thread::sleep(Duration::from_millis(10));
    };
    conn.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    // The host closes the connection unread, so the read ends at once.
    let read = conn.read(&mut [0u8; 8]);
    assert!(
        matches!(&read, Ok(0))
            || matches!(&read, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset),
        "{read:?}"
    );
    let pid = nix::unistd::Pid::from_raw(i32::try_from(host.id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    let out = host.wait_with_output().unwrap();
    assert_eq!(
        (&out.stdout[..], String::from_utf8_lossy(&out.stderr)),
        (&b""[..], String::from_utf8_lossy(PINNED))
    );
    assert_eq!(out.status.code(), Some(0));
}
