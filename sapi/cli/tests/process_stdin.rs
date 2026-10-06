//! `-B`, `-R`, `-F` and `-E` over stdin's lines, held to what the oracle
//! at 8.5.9 prints under `php -n` for the same stdin: each code string is
//! `zend_eval_string_ex` under its own name, so an `exit` with status 0
//! goes on to the next line, one that sets a status ends the lines, what
//! a line leaves uncaught is rendered and ends them with `-E` still
//! served, and a fatal ends everything.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn scratch(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("php-process-stdin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join(name);
    std::fs::write(&path, source).ok();
    path
}

/// stdout and stderr as one stream, and the exit code, of `php -n
/// <words>` with `stdin` on its stdin.
fn php(stdin: &str, words: &[&str]) -> (String, i32) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_php"))
        .arg("-n")
        .args(words)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("running php: {e}"));
    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(stdin.as_bytes()).ok();
    }
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("waiting for php: {e}"));
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.code().unwrap_or(-1))
}

#[test]
fn an_exit_with_0_goes_on_and_what_a_line_leaves_uncaught_ends_the_lines() {
    assert_eq!(
        php(
            "a\n\nb",
            &[
                "-B",
                "echo \"begin\\n\"; exit(0);",
                "-R",
                "var_dump($argn, $argi); if ($argi == 2) throw new Exception(\"x\");",
                "-E",
                "echo \"end\\n\";",
            ]
        ),
        (
            String::from(
                "begin\nstring(1) \"a\"\nint(1)\nstring(1) \"\0\"\nint(2)\n\nFatal error: Uncaught Exception: x in Command line run code:1\nStack trace:\n#0 {main}\n  thrown in Command line run code on line 1\nend\n"
            ),
            255
        )
    );
}

#[test]
fn an_exit_status_ends_the_lines_and_end_code_still_runs() {
    assert_eq!(
        php(
            "a\nb\n",
            &[
                "-R",
                "echo $argn, \"\\n\"; exit(3);",
                "-E",
                "echo \"end\\n\";"
            ]
        ),
        (String::from("a\nend\n"), 3)
    );
}

#[test]
fn a_fatal_ends_everything() {
    assert_eq!(
        php(
            "a\nb\n",
            &[
                "-R",
                "echo $argn; str_repeat(\"x\", PHP_INT_MAX);",
                "-E",
                "echo \"end\\n\";"
            ]
        ),
        (
            String::from(
                "a\nFatal error: Allowed memory size of 134217728 bytes exhausted (tried to allocate 9223372036854775839 bytes) in Command line run code on line 1\nStack trace:\n#0 Command line run code(1): str_repeat('x', 9223372036854775807)\n#1 {main}\n"
            ),
            255
        )
    );
}

/// `sapi/cli/tests/010.phpt`'s shape: the file `-F` names reads from
/// the same `STDIN` the lines are read from.
#[test]
fn a_file_for_each_line_reads_what_the_lines_left_on_stdin() {
    let file = scratch("f.php", "<?php var_dump($argn, fread(STDIN, 10));");
    let file = file.to_string_lossy().into_owned();
    assert_eq!(
        php("a\nb\nc\n", &["-F", &file]),
        (String::from("string(1) \"a\"\nstring(4) \"b\nc\n\"\n"), 0)
    );
    assert_eq!(
        php("a\n", &["-F", "nope.php"]),
        (String::from("Could not open input file: nope.php\n"), 1)
    );
}
