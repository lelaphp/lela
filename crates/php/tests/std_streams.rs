//! The binary's standard handles, held to what the oracle at 8.5.9 prints
//! under `php -n`: `do_cli` registers `STDIN`, `STDOUT` and `STDERR` over
//! the process's fds 0, 1 and 2 as resources 1 to 3 for a file and for
//! `-r`'s code, so a request's first open is resource 5 after
//! `FG(default_context)`; `fstat` and `ftruncate` on the three ask the
//! fds themselves.

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
fn a_file_and_direct_code_see_the_three_constants_and_the_first_open_is_resource_5() {
    let expected = "resource(1) of type (stream)\nresource(2) of type (stream)\nresource(3) of type (stream)\nresource(5) of type (stream)\n";
    let code = "var_dump(STDIN, STDOUT, STDERR, fopen('php://memory', 'r'));";
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

/// php's `STDIO` kind asks `fstat(2)` of the fd under the stream: 26
/// entries for a pipe as for any fd, and `ftruncate(2)` of a pipe is
/// `EINVAL`, `false`, where the numbers themselves are the pipe's.
#[test]
fn fstat_and_ftruncate_reach_the_process_s_fds() {
    assert_eq!(
        php(&[
            "-r",
            "var_dump(count(fstat(fopen('php://stdout', 'w'))), count(fstat(STDERR)), fstat(STDIN)['mode'] === fstat(fopen('php://stdin', 'r'))['mode'], ftruncate(STDOUT, 0));",
        ]),
        (
            "int(26)\nint(26)\nbool(true)\nbool(false)\n".to_owned(),
            String::new(),
            0
        )
    );
}

/// `ftruncate(2)` on a file fd 1 is redirected to cuts the file, as php's
/// does: what the request wrote before it is the sink's, and what stands
/// in the file once the process ends is what came after the cut.
#[test]
fn ftruncate_on_stdout_cuts_the_file_it_is_redirected_to() {
    let target = scratch("out.txt", "");
    let file = std::fs::File::create(&target).unwrap_or_else(|e| panic!("creating: {e}"));
    let status = Command::new(env!("CARGO_BIN_EXE_php"))
        .args([
            "-n",
            "-r",
            "var_dump(ftruncate(STDOUT, 0), fstat(STDOUT)['size']);",
        ])
        .stdin(Stdio::null())
        .stdout(file)
        .status()
        .unwrap_or_else(|e| panic!("running php: {e}"));
    assert!(status.success());
    let written = std::fs::read_to_string(&target).unwrap_or_default();
    assert_eq!(written, "bool(true)\nint(0)\n");
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
