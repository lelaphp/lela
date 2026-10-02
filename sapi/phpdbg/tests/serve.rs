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

/// Without `-b`, notices and errors are coloured whether or not stdout
/// is a terminal, the prompt never (the pinned phpdbg's bytes for
/// `phpdbg -qrr -n` and `phpdbg -qr -n`).
#[test]
fn notices_and_errors_are_coloured_unless_boring() {
    let (out, file, init) = initialized("colours-rr", &["-qrr", "-n"], INIT, INIT_PROGRAM, "");
    assert_eq!(
        out,
        format!(
            "\x1b[0;32m[Breakpoint #0 added at {file}:3]\x1b[0m\n\
             7\n\
             \n\
             \x1b[1;31m[Unrecognized command in {init}:3: nope, The command \"nope\" could not be found!]\x1b[0m\n\
             7\n"
        )
    );
    let (out, err, status) = ran(
        "colours-throw",
        &["-qrr", "-n"],
        "<?php throw new Exception(\"boo\");\n",
    );
    assert!(
        out.starts_with("\x1b[1;31m[Uncaught Exception in ")
            && out.contains(" on line 1]\x1b[0m\n"),
        "{out}"
    );
    assert_eq!((err.as_str(), status), ("", Some(255)));
    let (out, file) = debugged("colours-b", "<?php\necho \"x\\n\";\n", "r\nq\n");
    assert_eq!(
        out,
        format!(
            "[Successful compilation of {file}]\n\
             prompt> x\n\
             [Script ended normally]\n\
             prompt> "
        )
    );
}

/// A cycle's end exports the breakpoints as `break` lines, and the next
/// cycle starts its tables empty and its count at zero: the init file
/// numbers its line #0 again, the lines run again after it with nothing
/// written, and the breakpoints still stop; a deleted one is not
/// carried, and the numbers close up (the pinned phpdbg's bytes for
/// `phpdbg -qrb -n -i<file>` and `phpdbg -qIb -n`). The file's lines are
/// past the eighth: under it php lists a file's breakpoints off a packed
/// table it reads as a hash, which drops rows or crashes.
#[test]
fn a_cycle_starts_the_breakpoints_over_and_carries_them() {
    let program = "<?php\n\n\n\n\n\n\n$a = 1;\n$b = 2;\n$c = 3;\necho \"x\\n\";\n";
    let (out, file, _) = initialized(
        "cycle-init",
        &["-qrb", "-n"],
        "b 9\n",
        program,
        "c\nb 9\nb 10\ninfo break\nq\n",
    );
    assert_eq!(
        out,
        format!(
            "[Breakpoint #0 added at {file}:9]\n\
             [Breakpoint #0 at {file}:9, hits: 1]\n\
             >00009: $b = 2;\n \
             00010: $c = 3;\n \
             00011: echo \"x\\n\";\n\
             prompt> x\n\
             [Script ended normally]\n\
             [Breakpoint #0 added at {file}:9]\n\
             prompt> [Breakpoint at {file}:9 exists]\n\
             prompt> [Breakpoint #1 added at {file}:10]\n\
             prompt> ------------------------------------------------\n\
             File Breakpoints:\n\
             #0\t\t{file}:9\n\
             #1\t\t{file}:10\n\
             prompt> "
        )
    );
    let (out, file) = debugged(
        "cycle-numbers",
        "<?php\nfunction foo() { echo \"foo\\n\"; }\nfunction bar() { echo \"bar\\n\"; }\nfunction baz() { echo \"baz\\n\"; }\nfoo();\nbar();\nbaz();\n",
        "b foo\nb bar\nb del 0\nr\nc\nb baz\ninfo break\nr\nq\n",
    );
    assert_eq!(
        out,
        format!(
            "[Successful compilation of {file}]\n\
             prompt> [Breakpoint #0 added at foo]\n\
             prompt> [Breakpoint #1 added at bar]\n\
             prompt> [Deleted breakpoint #0]\n\
             prompt> foo\n\
             [Breakpoint #1 in bar() at {file}:3, hits: 1]\n\
             >00003: function bar() {{ echo \"bar\\n\"; }}\n \
             00004: function baz() {{ echo \"baz\\n\"; }}\n \
             00005: foo();\n\
             prompt> bar\n\
             baz\n\
             [Script ended normally]\n\
             prompt> [Breakpoint #1 added at baz]\n\
             prompt> ------------------------------------------------\n\
             Function Breakpoints:\n\
             #0\t\tbar\n\
             #1\t\tbaz\n\
             prompt> foo\n\
             [Breakpoint #0 in bar() at {file}:3, hits: 1]\n\
             >00003: function bar() {{ echo \"bar\\n\"; }}\n \
             00004: function baz() {{ echo \"baz\\n\"; }}\n \
             00005: foo();\n\
             prompt> "
        )
    );
}

/// The overview with each table's size cut to `N`: the engine's tables
/// hold what its bundle loads, which is not the oracle's build.
fn counts_cut(out: &str) -> String {
    out.lines()
        .map(|line| {
            match ["Classes    ", "Functions  ", "Constants  "]
                .iter()
                .find(|label| line.starts_with(**label))
            {
                Some(label) => format!("{label}N"),
                None => line.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `info` with no sub-command writes the execution context's overview:
/// in the init file, before the context is compiled; at the prompt the
/// compile left, with the main op array's sizes; and at a breakpoint,
/// executing, with what the last handler answered (the pinned phpdbg's
/// bytes for `phpdbg -qb -n -i<file>`, the table sizes aside).
#[test]
fn info_writes_the_execution_context_overview() {
    let overview = |file: &str, sizes: Option<&str>, executing: &str| {
        let compiled = if sizes.is_some() { "yes" } else { "no" };
        let sizes = sizes.unwrap_or_default();
        format!(
            "Execution Context Information\n\
             \n\
             Readline   no\n\
             Libedit    yes\n\
             Exec       {file}\n\
             Compiled   {compiled}\n\
             Stepping   off\n\
             Quietness  on\n\
             {sizes}\
             {executing}\
             Classes    N\n\
             Functions  N\n\
             Constants  N\n\
             Included   0"
        )
    };
    let (out, file, _) = initialized(
        "info",
        &["-qb", "-n"],
        "info\n",
        "<?php\n$a = 1;\nfunction f($x) { $y = $x; return $y; }\nf(1);\n",
        "info\nb f\nr\ninfo\nq\n",
    );
    let sizes = Some("Opcodes    5\nVariables  0\n");
    assert_eq!(
        counts_cut(&out),
        format!(
            "{}\n\
             [Successful compilation of {file}]\n\
             prompt> {}\n\
             prompt> [Breakpoint #0 added at f]\n\
             prompt> [Breakpoint #0 in f() at {file}:3, hits: 1]\n\
             >00003: function f($x) {{ $y = $x; return $y; }}\n \
             00004: f(1);\n \
             00005: \n\
             prompt> {}\n\
             prompt> ",
            overview(&file, None, "Executing  no\n"),
            overview(&file, sizes, "Executing  no\n"),
            overview(&file, sizes, "Executing  yes\nVM Return  1\n"),
        )
    );
}
