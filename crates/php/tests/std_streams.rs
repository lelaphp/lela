//! The binary's standard handles, held to what the oracle at 8.5.9 prints
//! under `php -n`: `do_cli` registers `STDIN`, `STDOUT` and `STDERR` as
//! resources 1 to 3 for a file and for `-r`'s code.

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn scratch(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("php-std-streams-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(name);
    let _ = std::fs::write(&path, source);
    path
}

/// stdout, stderr and the exit code of `php -n <words>`, stdin empty
/// and the two outputs pipes.
fn php(words: &[&str]) -> (String, String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
        .arg("-n")
        .args(words)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("running php: {e}"));
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn a_file_and_direct_code_see_the_three_constants() {
    let expected = "resource(1) of type (stream)\nresource(2) of type (stream)\nresource(3) of type (stream)\n";
    let code = "var_dump(STDIN, STDOUT, STDERR);";
    let file = scratch("ids.php", &format!("<?php\n{code}\n"));
    let file = file.to_string_lossy().into_owned();
    assert_eq!(php(&[&file]), (expected.to_owned(), String::new(), 0));
    assert_eq!(php(&["-r", code]), (expected.to_owned(), String::new(), 0));
}

#[test]
fn fwrite_on_stderr_reaches_the_process_s_second_stream() {
    assert_eq!(
        php(&["-r", "var_dump(fwrite(STDERR, 'x'));"]),
        ("int(1)\n".to_owned(), "x".to_owned(), 0)
    );
}

/// `-l` registers no stream, as `do_cli`'s lint arm calls no
/// `cli_register_file_handles`; nothing it prints can show the constants,
/// so this holds only that the lint is untouched.
#[test]
fn lint_is_untouched_by_the_handles() {
    let file = scratch("lint.php", "<?php\nvar_dump(STDIN);\n");
    let file = file.to_string_lossy().into_owned();
    assert_eq!(
        php(&["-l", &file]),
        (
            format!("No syntax errors detected in {file}\n"),
            String::new(),
            0
        )
    );
}
