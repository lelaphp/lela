//! The binary is php's debugger: the file its line names compiled, the
//! prompt read off stdin, a breakpoint stopping a run between two
//! handlers, `ev` evaluated in the frame stopped in, and the run ended
//! as phpdbg ends one.

use std::io::Write;
use std::process::{Command, Stdio};

/// What the binary wrote for `commands` over a program of `source`, and
/// the program's path as the binary names it.
fn debugged(name: &str, source: &str, commands: &str) -> (String, String) {
    let dir = std::env::temp_dir().join(format!("sapi-phpdbg-{name}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("t.php");
    std::fs::write(&file, source).unwrap();
    let file = std::fs::canonicalize(&file).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_phpdbg"))
        .args(["-qIb", "-n"])
        .arg(&file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(commands.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        file.display().to_string(),
    )
}

/// What the binary wrote on stdout and stderr, and its status, for
/// `flags` over a program of `source`, with nothing on stdin.
fn ran(name: &str, flags: &[&str], source: &str) -> (String, String, Option<i32>) {
    let dir = std::env::temp_dir().join(format!("sapi-phpdbg-{name}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("t.php");
    std::fs::write(&file, source).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_phpdbg"))
        .args(flags)
        .arg(&file)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

/// `-rr` runs the file and quits with php's quitting flag set, so a run
/// that ends normally writes no closing notice (the pinned phpdbg's
/// bytes for `phpdbg -qrr -n`).
#[test]
fn run_twice_on_the_line_quits_without_the_closing_notice() {
    let (out, err, status) = ran("rr", &["-qrr", "-n"], "<?php echo \"x\\n\";\n");
    assert_eq!((out.as_str(), err.as_str(), status), ("x\n", "", Some(0)));
    let (out, err, status) = ran(
        "rr-exit",
        &["-qrr", "-n"],
        "<?php echo \"x\\n\"; exit(3);\n",
    );
    assert_eq!((out.as_str(), err.as_str(), status), ("x\n", "", Some(3)));
}

/// Under `-rr` the loop checks nothing between handlers, so an uncaught
/// throwable stops at no prompt: the run's end reports it and the status
/// is its fatal's (the pinned phpdbg's bytes for `phpdbg -qrrb -n`).
#[test]
fn run_twice_on_the_line_reports_an_uncaught_throwable_at_the_end() {
    let (out, err, status) = ran(
        "rr-throw",
        &["-qrrb", "-n"],
        "<?php throw new Exception(\"boo\");\n",
    );
    let file = out
        .strip_prefix("[Uncaught Exception in ")
        .and_then(|rest| rest.split_once(" on line 1]\n"))
        .map(|(file, _)| file.to_owned())
        .unwrap_or_default();
    assert_eq!(
        (out.as_str(), err.as_str(), status),
        (
            format!(
                "[Uncaught Exception in {file} on line 1]\n\
                 Exception: boo in {file}:1\n\
                 Stack trace:\n\
                 #0 {{main}}\n"
            )
            .as_str(),
            "",
            Some(255)
        )
    );
}

#[test]
fn a_run_stops_at_a_breakpoint_and_evaluates_in_the_frame() {
    let (out, file) = debugged(
        "break",
        "<?php\n$i = 1;\necho PHP_SAPI, $i++, \"\\n\";\necho $i, \"\\n\";\n",
        "b 4\nr\nev $i * 10\nc\nq\n",
    );
    assert_eq!(
        out,
        format!(
            "[Successful compilation of {file}]\n\
             prompt> [Breakpoint #0 added at {file}:4]\n\
             prompt> phpdbg1\n\
             [Breakpoint #0 at {file}:4, hits: 1]\n\
             >00004: echo $i, \"\\n\";\n \
             00005: \n\
             prompt> 20\n\
             prompt> 2\n\
             [Script ended normally]\n\
             prompt> "
        )
    );
}

#[test]
fn a_step_stops_at_the_next_line_and_the_end_of_input_quits() {
    let (out, file) = debugged(
        "step",
        "<?php\nfunction f() {\n\treturn 1;\n}\nf();\n",
        "b f\nr\ns\n",
    );
    assert_eq!(
        out,
        format!(
            "[Successful compilation of {file}]\n\
             prompt> [Breakpoint #0 added at f]\n\
             prompt> [Breakpoint #0 in f() at {file}:3, hits: 1]\n\
             >00003: \treturn 1;\n \
             00004: }}\n \
             00005: f();\n\
             prompt> >00006: \n\
             prompt> "
        )
    );
}

/// A watchpoint's report writes the values through the program's
/// output, so its first byte sends the headers and the header callback
/// runs under the debugger, which stops in it on the same change before
/// the report goes on (`sapi/phpdbg/tests/gh15210_002.phpt`).
#[test]
fn a_watch_report_sends_the_headers_and_the_callback_is_debugged() {
    let (out, _) = debugged(
        "watch",
        "<?php\nheader_register_callback(function() { echo \"sent\";});\n$a = [0];\n$a[0] = 1;\n",
        "b 4\nr\nw $a[0]\nc\nc\nq\n",
    );
    let report = "prompt> [Breaking on watchpoint $a[0]]\n\
                  Old value: [Breaking on watchpoint $a[0]]\n\
                  Old value: 0\n\
                  New value: 1\n\
                  >00002: header_register_callback(function() { echo \"sent\";});\n \
                  00003: $a = [0];\n \
                  00004: $a[0] = 1;\n\
                  prompt> sent0\n\
                  New value: 1\n\
                  \n\
                  [$a[0] has been removed, removing watchpoint]\n";
    assert!(out.contains(report), "{out}");
}

#[test]
fn a_native_the_prompt_serves_reads_the_session() {
    let (out, file) = debugged(
        "lent",
        "<?php\nfunction f() {\n  echo \"in f\\n\";\n}\nf();\necho \"done\\n\";\n",
        "register phpdbg_prompt\nphpdbg_prompt y>\nev phpdbg_break_function(\"f\")\nr\nq\n",
    );
    assert_eq!(
        out,
        format!(
            "[Successful compilation of {file}]\n\
             prompt> [Registered phpdbg_prompt]\n\
             prompt> \n\
             y> [Breakpoint #0 added at f]\n\
             \n\
             y> [Breakpoint #0 in f() at {file}:3, hits: 1]\n\
             >00003:   echo \"in f\\n\";\n \
             00004: }}\n \
             00005: f();\n\
             y> "
        )
    );
}

/// What the binary wrote on stdout for `flags` and an init file of
/// `init` over a program of `source`, with `stdin` on its stdin; the
/// program's real path and the init file's path.
fn initialized(
    name: &str,
    flags: &[&str],
    init: &str,
    source: &str,
    stdin: &str,
) -> (String, String, String) {
    let dir = std::env::temp_dir().join(format!("sapi-phpdbg-{name}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("t.php");
    std::fs::write(&file, source).unwrap();
    let file = std::fs::canonicalize(&file).unwrap();
    let init_file = dir.join("init");
    std::fs::write(&init_file, init).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_phpdbg"))
        .args(flags)
        .arg(format!("-i{}", init_file.display()))
        .arg(&file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        file.display().to_string(),
        init_file.display().to_string(),
    )
}

const INIT_PROGRAM: &str = "<?php\n$i = 1;\necho $x ?? \"unset\", \"\\n\";\n";
const INIT: &str = "b 3\nev $x = 7\nnope\n";

/// The init file's commands run before the startup run, in its request,
/// a line at a time: a line that names no command writes its error with
/// the line and the next line still runs (the pinned phpdbg's bytes for
/// `phpdbg -qrrb -n -i<file>` and `phpdbg -qrb -n -i<file>`).
#[test]
fn the_init_file_runs_before_the_startup_run() {
    let (out, file, init) = initialized("init-rr", &["-qrrb", "-n"], INIT, INIT_PROGRAM, "");
    assert_eq!(
        out,
        format!(
            "[Breakpoint #0 added at {file}:3]\n\
             7\n\
             \n\
             [Unrecognized command in {init}:3: nope, The command \"nope\" could not be found!]\n\
             7\n"
        )
    );
    let (out, file, init) = initialized("init-r", &["-qrb", "-n"], INIT, INIT_PROGRAM, "q\n");
    assert_eq!(
        out,
        format!(
            "[Breakpoint #0 added at {file}:3]\n\
             7\n\
             \n\
             [Unrecognized command in {init}:3: nope, The command \"nope\" could not be found!]\n\
             [Breakpoint #0 at {file}:3, hits: 1]\n\
             >00003: echo $x ?? \"unset\", \"\\n\";\n \
             00004: \n\
             prompt> "
        )
    );
}
