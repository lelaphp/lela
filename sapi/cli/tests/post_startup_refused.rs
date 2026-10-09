//! A refusal answered after every extension started, at
//! `zend_post_startup` inside `php_module_startup`: opcache's
//! `ACCEL_LOG_FATAL` in `accel_post_startup` for `opcache.file_cache_only`
//! with no `opcache.file_cache`, and a preload that fails. php ends the
//! process there, before `main` reads its line further, so the file named
//! on the line is never opened and `-i` never answered (#1943). The bytes
//! and the status are the oracle's php under the same line; a fence cannot
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

/// opcache's line on stderr, nothing on stdout, status 254, whatever the
/// line asked of the host: a file, `-i`, `-v` or `-m`.
#[test]
fn a_post_startup_refusal_ends_the_process_before_the_host_acts() {
    for ask in ["/nonexistent/t.php", "-i", "-v", "-m"] {
        let (stdout, stderr, status) = ran(Command::new(env!("CARGO_BIN_EXE_php"))
            .args(["-n", "-d", "opcache.enable_cli=1"])
            .args(["-d", "opcache.file_cache_only=1", ask]));
        assert_eq!(stdout, "", "{ask}");
        assert_eq!(stderr, format!("<asctime> (<pid>): {REFUSED}"), "{ask}");
        assert_eq!(status, Some(254), "{ask}");
    }
}

/// A preload that throws fails the startup: its fatal on stdout, as
/// `php_error_cb` displays it while php starts, and status 1, with `-i`
/// never answered.
#[test]
fn a_failed_preload_ends_the_process_before_the_host_acts() {
    let dir = std::env::temp_dir().join(format!("t1943-preload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("p.php");
    std::fs::write(&script, "<?php throw new Exception(\"refused\");\n").unwrap();
    let script = std::fs::canonicalize(&script).unwrap();
    let (stdout, stderr, status) = ran(Command::new(env!("CARGO_BIN_EXE_php"))
        .args(["-n", "-d", "opcache.enable_cli=1", "-d"])
        .arg(format!("opcache.preload={}", script.display()))
        .arg("-i"));
    std::fs::remove_dir_all(&dir).ok();
    let file = script.display();
    assert_eq!(
        stdout,
        format!(
            "\nFatal error: Uncaught Exception: refused in {file}:1\nStack trace:\n#0 {{main}}\n  thrown in {file} on line 1\n"
        )
    );
    assert_eq!(stderr, "");
    assert_eq!(status, Some(1));
}
