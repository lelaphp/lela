//! `php -S` stopped by `SIGINT`: php's handler only clears
//! `server.is_running` (`Z/sapi/cli/php_cli_server.c:2800-2804`), the
//! event loop ends after its wait, `do_cli_server` returns 0 and the CLI
//! ends the engine with `php_module_shutdown`. So the server ends with
//! status 0, not killed by the signal, with a connection that never sent
//! a request still open and after one that was served.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn a_sigint_stops_the_built_in_server_with_status_zero() {
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|probe| probe.local_addr())
        .map_or_else(|e| panic!("finding a free port: {e}"), |bound| bound.port());
    let root = std::env::temp_dir().join(format!("php-server-sigint-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("making {}: {e}", root.display()));
    std::fs::write(root.join("index.php"), "<?php echo 'served';")
        .unwrap_or_else(|e| panic!("writing index.php: {e}"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_php"))
        .arg("-n")
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
    let mut served = TcpStream::connect(&address).unwrap_or_else(|e| panic!("connecting: {e}"));
    let _ = served.write_all(b"GET /index.php HTTP/1.0\r\n\r\n");
    let mut response = Vec::new();
    let _ = served.read_to_end(&mut response);
    assert!(String::from_utf8_lossy(&response).ends_with("served"));
    let _idle = TcpStream::connect(&address).unwrap_or_else(|e| panic!("connecting: {e}"));
    std::thread::sleep(Duration::from_millis(200));
    let pid = child.id().to_string();
    let killed = Command::new("kill").args(["-INT", &pid]).status();
    assert!(killed.is_ok_and(|status| status.success()));
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break Some(status);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(status.and_then(|status| status.code()), Some(0));
}
