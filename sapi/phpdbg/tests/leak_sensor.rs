//! The request leak sensor over phpdbg's own natives: a debug
//! binary is armed unless `PHP_SENSE_LEAKS=0` turns it off, so a program that
//! keeps a block ends in the `InternalError` naming it.
#![cfg(debug_assertions)]

use std::io::Write;
use std::process::{Command, Stdio};

/// What `phpdbg -qIb -n`, armed as a debug binary is by default, wrote
/// on stdout for a program of `source`, served and quit.
fn armed(name: &str, source: &str) -> String {
    let dir = std::env::temp_dir().join(format!("sapi-phpdbg-leak-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let file = dir.join("t.php");
    std::fs::write(&file, source).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lela-dbg"))
        .env_remove("PHP_SENSE_LEAKS")
        .args(["-qIb", "-n"])
        .arg(&file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"r\nq\n").unwrap();
    let out = child.wait_with_output().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `phpdbg_end_oplog` and `phpdbg_get_executable` look their options up
/// by name and read the `files` they are given as strings, and keep
/// nothing of either.
#[test]
fn the_options_and_files_the_natives_read_are_given_up() {
    let stdout = armed(
        "options",
        "<?php\nfunction f($a) { return $a + 1; }\nphpdbg_start_oplog(); f(1);\n$log = phpdbg_end_oplog(['functions' => true, 'opcodes' => true]);\n$e = phpdbg_get_executable(['files' => [__DIR__ . '/t.php'], 'functions' => true]);\necho count($log), ' ', count($e), \"\\n\";\n",
    );
    assert!(stdout.contains("2 1\n"), "{stdout}");
    assert!(!stdout.contains("InternalError"), "{stdout}");
}
