//! A refusal answered after every extension started, at
//! `zend_post_startup` inside `php_module_startup`: opcache's
//! `ACCEL_LOG_FATAL` in `accel_post_startup` for `opcache.file_cache_only`
//! with no `opcache.file_cache`. php-fpm's master ends there, before it
//! reads its configuration or binds a socket (#1943). The bytes and the
//! status are the oracle's php-fpm under the same line; a fence cannot
//! carry an ini (#1634).

use std::process::{Command, Stdio};

/// opcache's line under `opcache.file_cache_only` with no
/// `opcache.file_cache`, as `zend_accel_error` writes it to stderr: the
/// time `asctime` prints, the process's id and the message.
const REFUSED: &str =
    "Fatal Error opcache.file_cache_only is set without a proper setting of opcache.file_cache\n";

/// What the process `command` starts wrote and the status it ended
/// with, run under the recipe's `TZ`; its stderr with the time masked,
/// which `asctime` prints in the 24 columns before ` (<pid>): `.
fn ran(command: &mut Command) -> (String, String, Option<i32>) {
    let child = command
        .env("TZ", "UTC")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let tail = format!(" ({pid}): ");
    let masked = match stderr.find(&tail) {
        Some(24) => format!("<asctime>{}", &stderr[24..]),
        _ => stderr,
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    (
        stdout,
        masked.replace(&tail, " (<pid>): "),
        out.status.code(),
    )
}

/// opcache's line on stderr, nothing on stdout, status 254: under `-t`
/// with no configuration there, and in the foreground over a pool that
/// listens, which never says it is running.
#[test]
fn a_post_startup_refusal_ends_the_master_before_it_binds() {
    let dir = std::env::temp_dir().join(format!("t1943-fpm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conf = dir.join("php-fpm.conf");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .unwrap()
        .port();
    std::fs::write(
        &conf,
        format!(
            "[global]\nerror_log = /dev/stderr\n[www]\nlisten = 127.0.0.1:{port}\npm = static\npm.max_children = 1\n"
        ),
    )
    .unwrap();
    let lines: [&[&str]; 2] = [
        &["-t", "-y", "/nonexistent/php-fpm.conf"],
        &["-F", "-y", conf.to_str().unwrap()],
    ];
    for line in lines {
        let (stdout, stderr, status) = ran(Command::new(env!("CARGO_BIN_EXE_php-fpm"))
            .args(["-n", "-d", "opcache.file_cache_only=1"])
            .args(line));
        assert_eq!(stdout, "", "{line:?}");
        assert_eq!(stderr, format!("<asctime> (<pid>): {REFUSED}"), "{line:?}");
        assert_eq!(status, Some(254), "{line:?}");
    }
    std::fs::remove_dir_all(&dir).ok();
}
