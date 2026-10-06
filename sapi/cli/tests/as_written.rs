//! What the binary writes, as php's CLI writes it, held to what the oracle at
//! 8.5.9 prints under `php -n`: `sapi_cli_ub_write` writes each chunk to
//! fd 1 as it comes and the error stream reaches fd 2 as it is written,
//! so a child process writing to the stdout it inherits stands between
//! the request's own writes, and one pipe under both streams carries them
//! in the order written (#741). The witness lane serves each snippet in
//! its own process over the `Capture` sink and never crosses the process
//! boundary, so these are held here.

use std::io::Read;
use std::process::{Command, Stdio};

/// What `php -n -r <code>` writes to stdout, stdin empty.
fn stdout_of(code: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
        .args(["-n", "-r", code])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("running php: {e}"));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// What `php -n -r <code>` writes with stdout and stderr on one pipe,
/// as `2>&1` puts them.
fn merged(code: &str) -> String {
    let (mut reader, writer) = std::io::pipe().unwrap_or_else(|e| panic!("a pipe: {e}"));
    let second = writer
        .try_clone()
        .unwrap_or_else(|e| panic!("cloning the pipe: {e}"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_php"))
        .args(["-n", "-r", code])
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(second)
        .spawn()
        .unwrap_or_else(|e| panic!("running php: {e}"));
    let mut text = String::new();
    reader.read_to_string(&mut text).ok();
    child.wait().ok();
    text
}

#[test]
fn a_popen_child_writes_between_the_echoes_around_it() {
    assert_eq!(
        stdout_of(
            r#"echo "a\n"; $p = popen('cat', 'w'); fwrite($p, "b\n"); pclose($p); echo "c\n";"#
        ),
        "a\nb\nc\n"
    );
}

#[test]
fn a_proc_open_child_on_the_inherited_stdout_writes_between_the_echoes() {
    assert_eq!(
        stdout_of(
            r#"echo "a\n"; $p = proc_open('echo child', [], $pipes); proc_close($p); echo "c\n";"#
        ),
        "a\nchild\nc\n"
    );
}

/// What a handler holds has not left the `php_output` layer, so the
/// child's write stands before it, and the handler's flush writes it.
#[test]
fn what_a_buffer_holds_reaches_the_stdout_only_when_it_leaves() {
    assert_eq!(
        stdout_of(
            r#"ob_start(); echo "a\n"; $p = popen('cat', 'w'); fwrite($p, "b\n"); pclose($p); ob_end_flush(); echo "c\n";"#
        ),
        "b\na\nc\n"
    );
}

/// The error stream is written as it comes, beside the body rather than
/// before it: `STDERR` and the `display_errors=stderr` form of a warning,
/// which carries no leading newline.
#[test]
fn the_error_stream_and_the_body_interleave_on_one_pipe() {
    assert_eq!(
        merged(
            r#"echo "a\n"; fwrite(STDERR, "b\n"); echo "c\n"; ini_set('display_errors', 'stderr'); echo $u; echo "d\n";"#
        ),
        "a\nb\nc\nWarning: Undefined variable $u in Command line code on line 1\nd\n"
    );
}
