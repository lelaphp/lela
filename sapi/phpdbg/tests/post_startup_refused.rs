//! A refusal answered after every extension started, at
//! `zend_post_startup` inside `php_module_startup`: opcache's
//! `ACCEL_LOG_FATAL` in `accel_post_startup` for `opcache.file_cache_only`
//! with no `opcache.file_cache`. php ends the process there, before
//! phpdbg reads its line further, so the file named on the line is never
//! run (#1943). The bytes and the status are the oracle's phpdbg under
//! the same line; a fence cannot carry an ini (#1634).

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

/// opcache's line on stderr, nothing on stdout, status 254.
#[test]
fn a_post_startup_refusal_ends_the_process_before_the_file_is_run() {
    let (stdout, stderr, status) = ran(Command::new(env!("CARGO_BIN_EXE_phpdbg"))
        .args(["-n", "-d", "opcache.enable_cli=1"])
        .args([
            "-d",
            "opcache.file_cache_only=1",
            "-qrr",
            "/nonexistent/t.php",
        ]));
    assert_eq!(stdout, "");
    assert_eq!(stderr, format!("<asctime> (<pid>): {REFUSED}"));
    assert_eq!(status, Some(254));
}
