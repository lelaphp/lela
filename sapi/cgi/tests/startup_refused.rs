//! An extension whose engine start refuses, php's `MINIT` returning
//! `FAILURE` (ADR-0010): `ffi.preload` naming a header that is not there.
//! php ends the process from `php_error_cb`, `exit(-2)` for the
//! `E_CORE_ERROR` of a failed `MINIT` (`Z/main/main.c:1505-1507`), before
//! the host does anything of its own. The bytes and the status are the
//! oracle's php-cgi under the same line (#1802); a fence cannot carry an
//! ini (#1634).

use std::process::Command;

const MISSING: &str = "/nonexistent/startup-refused.h";

/// The diagnostics on stdout in php-cgi's `html_errors`, no head before
/// them, the file named on the line never opened, and status 254.
#[test]
fn a_refused_start_ends_the_process_before_the_file_is_opened() {
    let out = Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .args([
            "-n",
            "-d",
            &format!("ffi.preload={MISSING}"),
            "/nonexistent/t.php",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "<br />\n<b>Warning</b>:  FFI: failed pre-loading '{MISSING}', file doesn't exist in <b>Unknown</b> on line <b>0</b><br />\n<br />\n<b>Fatal error</b>:  Unable to start FFI module in <b>Unknown</b> on line <b>0</b><br />\nStack trace:\n#0 {{main}}\n"
        )
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(out.status.code(), Some(254));
}
