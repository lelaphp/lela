//! An extension whose engine start refuses, php's `MINIT` returning
//! `FAILURE` (ADR-0010): `ffi.preload` naming a header that is not there.
//! php ends the process from `php_error_cb`, `exit(-2)` for the
//! `E_CORE_ERROR` of a failed `MINIT` (`Z/main/main.c:1505-1507`), before
//! the host does anything of its own. The bytes and the status are the
//! oracle's phpdbg under the same line (#1802); a fence cannot carry an
//! ini (#1634).

use std::process::Command;

const MISSING: &str = "/nonexistent/startup-refused.h";

/// phpdbg displays nothing under its `display_errors` and logs each
/// diagnostic as its log function does before it is booted, a line on
/// stdout; the file named on the line is never run, and status 254.
#[test]
fn a_refused_start_ends_the_process_before_the_file_is_run() {
    let out = Command::new(env!("CARGO_BIN_EXE_phpdbg"))
        .args([
            "-qrr",
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
            "PHP Warning:  FFI: failed pre-loading '{MISSING}', file doesn't exist in Unknown on line 0\nPHP Fatal error:  Unable to start FFI module in Unknown on line 0\nStack trace:\n#0 {{main}}\n"
        )
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(out.status.code(), Some(254));
}

/// Under `display_errors=1` each diagnostic is logged, then displayed,
/// before the next, in the order `php_error_cb` writes them.
#[test]
fn a_refused_start_logs_and_displays_each_diagnostic_in_turn() {
    let out = Command::new(env!("CARGO_BIN_EXE_phpdbg"))
        .args([
            "-qrr",
            "-n",
            "-d",
            "display_errors=1",
            "-d",
            "fatal_error_backtraces=0",
        ])
        .args([
            "-d",
            &format!("ffi.preload={MISSING}"),
            "/nonexistent/t.php",
        ])
        .output()
        .unwrap();
    let warning =
        format!("FFI: failed pre-loading '{MISSING}', file doesn't exist in Unknown on line 0");
    let fatal = "Unable to start FFI module in Unknown on line 0";
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "PHP Warning:  {warning}\n\nWarning: {warning}\nPHP Fatal error:  {fatal}\n\nFatal error: {fatal}\n"
        )
    );
    assert_eq!(out.status.code(), Some(254));
}

/// Under `display_errors=stderr` the display goes to stderr, as
/// `php_error_cb` writes it for a host named `phpdbg`, and the log stays
/// on stdout.
#[test]
fn a_refused_start_displays_on_stderr_where_display_errors_says_so() {
    let out = Command::new(env!("CARGO_BIN_EXE_phpdbg"))
        .args([
            "-qrr",
            "-n",
            "-d",
            "display_errors=stderr",
            "-d",
            "fatal_error_backtraces=0",
        ])
        .args([
            "-d",
            &format!("ffi.preload={MISSING}"),
            "/nonexistent/t.php",
        ])
        .output()
        .unwrap();
    let warning =
        format!("FFI: failed pre-loading '{MISSING}', file doesn't exist in Unknown on line 0");
    let fatal = "Unable to start FFI module in Unknown on line 0";
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("PHP Warning:  {warning}\nPHP Fatal error:  {fatal}\n")
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!("Warning: {warning}\nFatal error: {fatal}\n")
    );
    assert_eq!(out.status.code(), Some(254));
}
