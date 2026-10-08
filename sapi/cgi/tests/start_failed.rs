//! A request whose start fails: an extension's `rinit` raises, which is
//! php's fatal inside `RINIT` (#1775). Session's, under
//! `session.auto_start`, over a session file whose `DateTime` does not
//! unserialize: the throwable is rendered as the uncaught fatal with no
//! frame standing, session's warning that the data did not decode follows
//! it, and php-cgi's `main` returns `FAILURE` where `php_request_startup`
//! failed (`Z/sapi/cgi/cgi_main.c:2460-2467`), the program never run and no
//! request-end phase served, so the session's file, destroyed as the data
//! failed, is not written again. The bytes and the status are the oracle's
//! php-cgi under the same line and environment (#1879); a fence carries
//! neither the ini nor the session file (#1634).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The session's data: a `DateTime` whose `date` is no string.
const DATA: &str = "x|O:8:\"DateTime\":1:{s:4:\"date\";i:5;}";

/// What the failed start writes after the head.
const FAILED: &str = "<br />\n<b>Fatal error</b>:  Uncaught Error: Invalid serialization data for DateTime object in [no active file]:0\nStack trace:\n#0 [internal function]: DateTime-&gt;__unserialize(Array)\n#1 {main}\n  thrown in <b>[no active file]</b> on line <b>0</b><br />\n<br />\n<b>Warning</b>:  PHP Request Startup: Failed to decode session object. Session has been destroyed in <b>Unknown</b> on line <b>0</b><br />\n";

/// The head the failure's output commits.
const HEAD: &str = "X-Powered-By: PHP/8.5.9\r\nContent-type: text/html; charset=UTF-8\r\n\r\n";

/// A directory of the test's own holding the program and the session's
/// file under the id the cookie names.
fn tree(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sapi-cgi-start-failed-{name}-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("t.php"), "<?php echo \"ran\\n\";").unwrap();
    std::fs::write(dir.join("sess_t1879"), DATA).unwrap();
    dir
}

/// php-cgi over `dir`'s program with the session started automatically
/// from `dir`, the cookie naming the file, and `flags` before the file.
fn served(dir: &Path, flags: &[&str]) -> Output {
    served_file(dir, "t.php", flags)
}

/// [`served`] over `dir`'s file `name`, which need not exist.
fn served_file(dir: &Path, name: &str, flags: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .env_clear()
        .env("HTTP_COOKIE", "PHPSESSID=t1879")
        .args(["-n", "-d", "session.auto_start=1", "-d"])
        .arg(format!("session.save_path={}", dir.display()))
        .args(flags)
        .arg(dir.join(name))
        .output()
        .unwrap()
}

/// The head, the fatal and session's warning, no program, status 255, and
/// the session's file gone, as the failed decode destroyed it.
#[test]
fn a_failed_start_ends_php_cgi_before_the_program() {
    let dir = tree("serve");
    let out = served(&dir, &[]);
    let file_left = dir.join("sess_t1879").exists();
    let others = std::fs::read_dir(&dir).unwrap().count();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{HEAD}{FAILED}")
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(out.status.code(), Some(255));
    assert!(!file_left);
    assert_eq!(others, 1, "no session file is written after the failure");
}

/// `-q` and `-l` set `no_headers` only once `php_request_startup`
/// returned (`Z/sapi/cgi/cgi_main.c:2468-2471`), so the failure's output
/// commits the head under them too; `-l` writes no verdict.
#[test]
fn the_head_is_committed_under_q_and_l_since_the_start_never_returned() {
    for flag in ["-q", "-l"] {
        let dir = tree(&flag[1..]);
        let out = served(&dir, &[flag]);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("{HEAD}{FAILED}"),
            "{flag}"
        );
        assert_eq!(out.status.code(), Some(255), "{flag}");
    }
}

/// What a buffer `output_buffering` started still holds is never flushed,
/// since no `php_request_shutdown` ends the buffers: nothing at all is
/// written, not even the head.
#[test]
fn a_buffered_failure_writes_nothing() {
    let dir = tree("buffered");
    let out = served(&dir, &["-d", "output_buffering=4096"]);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(out.status.code(), Some(255));
}

/// php-cgi opens the primary script only once `php_request_startup` has
/// returned (`Z/sapi/cgi/cgi_main.c:2460-2487`), so a start that fails
/// over a missing script writes what the failure wrote under a head with
/// no `Status:` line, and never `No input file specified.`; a server's
/// request with no path translated carries the 404 `init_request_info`
/// set before the start, and nothing more (#2272).
#[test]
fn a_failed_start_over_a_missing_script_writes_no_refusal() {
    let dir = tree("missing");
    let out = served_file(&dir, "missing.php", &[]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{HEAD}{FAILED}")
    );
    assert_eq!(out.status.code(), Some(255));
    std::fs::write(dir.join("sess_t1879"), DATA).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .env_clear()
        .env("HTTP_COOKIE", "PHPSESSID=t1879")
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("REDIRECT_STATUS", "200")
        .env("REQUEST_METHOD", "GET")
        .env("SCRIPT_FILENAME", dir.join("missing.php"))
        .args([
            "-n",
            "-d",
            "cgi.fix_pathinfo=0",
            "-d",
            "session.auto_start=1",
            "-d",
        ])
        .arg(format!("session.save_path={}", dir.display()))
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{HEAD}{FAILED}")
    );
    assert_eq!(out.status.code(), Some(255));
}
