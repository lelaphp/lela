//! A request whose start fails under the built-in server: session's
//! `rinit` raises under `session.auto_start` over a session file whose
//! `DateTime` does not unserialize, which is php's fatal inside `RINIT`
//! (#1775). `php_cli_server_dispatch` is the one host that shuts the
//! request down after the failure and closes the connection
//! (`Z/sapi/cli/php_cli_server.c:2307-2310`): the client is sent the head
//! and what the failure displayed, the program never runs, no response
//! line is logged for it, and the server goes on to serve the next
//! request. The body and the log's lines are the oracle's `php -S` under
//! the same line (#1879). The CLI's own ending, `Could not startup.`, has
//! no request a session can fail in, since the CLI reads no cookie, and is
//! held in `request`'s tests.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What the failed start displays, after the head.
const FAILED: &str = "<br />\n<b>Fatal error</b>:  Uncaught Error: Invalid serialization data for DateTime object in [no active file]:0\nStack trace:\n#0 [internal function]: DateTime-&gt;__unserialize(Array)\n#1 {main}\n  thrown in <b>[no active file]</b> on line <b>0</b><br />\n<br />\n<b>Warning</b>:  PHP Request Startup: Failed to decode session object. Session has been destroyed in <b>Unknown</b> on line <b>0</b><br />\n";

/// The response to `request` sent on a connection of its own.
fn response(address: &str, request: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap_or_else(|e| panic!("connecting: {e}"));
    stream.write_all(request.as_bytes()).ok();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok();
    String::from_utf8_lossy(&response).into_owned()
}

#[test]
fn the_built_in_server_shuts_a_failed_request_down_and_serves_the_next() {
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|probe| probe.local_addr())
        .map_or_else(|e| panic!("finding a free port: {e}"), |bound| bound.port());
    let root = std::env::temp_dir().join(format!("php-server-start-failed-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("making {}: {e}", root.display()));
    std::fs::write(root.join("index.php"), "<?php echo \"ran\\n\";")
        .unwrap_or_else(|e| panic!("writing index.php: {e}"));
    std::fs::write(
        root.join("sess_t1879"),
        "x|O:8:\"DateTime\":1:{s:4:\"date\";i:5;}",
    )
    .unwrap_or_else(|e| panic!("writing the session: {e}"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_php"))
        .args(["-n", "-d", "session.auto_start=1", "-d"])
        .arg(format!("session.save_path={}", root.display()))
        .arg("-S")
        .arg(format!("127.0.0.1:{port}"))
        .arg("-t")
        .arg(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("running php -S: {e}"));
    let stderr = child.stderr.take().unwrap_or_else(|| panic!("no stderr"));
    let mut lines = BufReader::new(stderr).lines();
    let started = lines
        .by_ref()
        .map_while(Result::ok)
        .any(|line| line.contains("Development Server") && line.ends_with("started"));
    assert!(started, "the server never said it started");
    let address = format!("127.0.0.1:{port}");
    let failed = response(
        &address,
        "GET /index.php HTTP/1.1\r\nHost: x\r\nCookie: PHPSESSID=t1879\r\n\r\n",
    );
    let next = response(&address, "GET /index.php HTTP/1.1\r\nHost: x\r\n\r\n");
    std::thread::sleep(Duration::from_millis(200));
    let pid = child.id().to_string();
    Command::new("kill").args(["-INT", &pid]).status().ok();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !matches!(child.try_wait(), Ok(Some(_))) {
        std::thread::sleep(Duration::from_millis(50));
    }
    child.kill().ok();
    let logged: Vec<String> = lines
        .map_while(Result::ok)
        .filter_map(|line| line.split_once("] ").map(|(_, rest)| rest.to_owned()))
        .map(|line| {
            line.split_once(' ')
                .map_or(line.clone(), |(_, rest)| rest.to_owned())
        })
        .collect();
    std::fs::remove_dir_all(&root).ok();
    let (head, body) = failed.split_once("\r\n\r\n").unwrap_or_default();
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{failed:?}");
    assert!(
        head.ends_with(
            "Connection: close\r\nX-Powered-By: PHP/8.5.9\r\nContent-type: text/html; charset=UTF-8"
        ),
        "{failed:?}"
    );
    assert_eq!(body, FAILED);
    assert!(next.ends_with("\r\n\r\nran\n"), "{next:?}");
    assert_eq!(
        logged,
        [
            "Accepted",
            "Closing",
            "Accepted",
            "[200]: GET /index.php",
            "Closing"
        ]
    );
}
