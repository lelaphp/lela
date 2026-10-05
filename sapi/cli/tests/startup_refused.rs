//! An extension whose engine start refuses, php's `MINIT` returning
//! `FAILURE` (ADR-0010): `ffi.preload` naming a header that is not there.
//! php ends the process from `php_error_cb`, `exit(-2)` for the
//! `E_CORE_ERROR` of a failed `MINIT` (`Z/main/main.c:1505-1507`), before
//! the host does anything of its own. The bytes and the status are the
//! oracle's php under the same line (#1802); a fence cannot carry an
//! ini (#1634).

use std::process::Command;

const MISSING: &str = "/nonexistent/startup-refused.h";

/// The diagnostics on stdout as the CLI displays them, the file named on
/// the line never opened, and status 254.
#[test]
fn a_refused_start_ends_the_process_before_the_file_is_opened() {
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
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
            "\nWarning: FFI: failed pre-loading '{MISSING}', file doesn't exist in Unknown on line 0\n\nFatal error: Unable to start FFI module in Unknown on line 0\nStack trace:\n#0 {{main}}\n"
        )
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(out.status.code(), Some(254));
}

/// What php raised before the failed start shows ahead of it, date's
/// warning over the host's `date.timezone`; what it raises only after
/// every extension started, `disable_functions` naming a construct, does
/// not show at all.
#[test]
fn a_refused_start_shows_only_what_php_raised_before_it() {
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
        .args(["-n", "-d", &format!("ffi.preload={MISSING}")])
        .args([
            "-d",
            "fatal_error_backtraces=0",
            "-d",
            "disable_functions=exit",
        ])
        .args(["-d", "date.timezone=Nowhere", "/nonexistent/t.php"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "\nWarning: PHP Startup: Invalid date.timezone value 'Nowhere', using 'UTC' instead in Unknown on line 0\n\nWarning: FFI: failed pre-loading '{MISSING}', file doesn't exist in Unknown on line 0\n\nFatal error: Unable to start FFI module in Unknown on line 0\n"
        )
    );
    assert_eq!(out.status.code(), Some(254));
}
