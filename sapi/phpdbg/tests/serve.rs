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

/// What the binary wrote on stdout and stderr, and its status, for
/// `flags` over a program of `source`, with `init` as the file `-i`
/// names where there is one and `stdin` on its stdin: killed after ten
/// seconds, where it has not ended, so a session that loops ends the
/// test; the program's real path.
struct Served {
    out: String,
    err: String,
    status: Option<i32>,
    file: String,
}

fn under_phpdbg(
    name: &str,
    flags: &[&str],
    init: Option<&str>,
    source: &str,
    stdin: &str,
) -> Served {
    under_phpdbg_with(name, flags, init, source, stdin, &Setup::default())
}

/// What a test sets up beside the program: files under its directory,
/// which is the working directory, by relative path; the environment
/// over the test's, `{dir}` in a value standing for the directory; and
/// how much of stdout is read before the binary is killed, for a
/// session that loops.
#[derive(Default)]
struct Setup<'a> {
    files: &'a [(&'a str, &'a str)],
    env: &'a [(&'a str, &'a str)],
    limit: Option<usize>,
}

fn under_phpdbg_with(
    name: &str,
    flags: &[&str],
    init: Option<&str>,
    source: &str,
    stdin: &str,
    setup: &Setup<'_>,
) -> Served {
    let dir = std::env::temp_dir().join(format!("sapi-phpdbg-{name}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let dir = std::fs::canonicalize(&dir).unwrap();
    let file = dir.join("t.php");
    std::fs::write(&file, source).unwrap();
    for (path, text) in setup.files {
        let path = dir.join(path);
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        std::fs::write(path, text).unwrap();
    }
    let init_file = dir.join("init");
    let mut command = Command::new(env!("CARGO_BIN_EXE_phpdbg"));
    command.args(flags);
    for (name, value) in setup.env {
        command.env(name, value.replace("{dir}", &dir.display().to_string()));
    }
    if let Some(init) = init {
        std::fs::write(&init_file, init).unwrap();
        command.arg(format!("-i{}", init_file.display()));
    }
    let mut child = command
        .arg(&file)
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let stdin = stdin.to_owned();
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(stdin.as_bytes());
    });
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let limit = setup.limit.unwrap_or(usize::MAX);
    let read = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = std::sync::Arc::clone(&read);
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        while bytes.len() < limit {
            match std::io::Read::read(&mut stdout, &mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            }
            counted.store(bytes.len(), std::sync::atomic::Ordering::Relaxed);
        }
        bytes.truncate(limit);
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stderr, &mut bytes);
        bytes
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if std::time::Instant::now() > deadline
            || read.load(std::sync::atomic::Ordering::Relaxed) >= limit
        {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let _ = writer.join();
    let out = out.join().unwrap();
    let err = err.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    Served {
        out: String::from_utf8_lossy(&out).into_owned(),
        err: String::from_utf8_lossy(&err).into_owned(),
        status,
        file: file.display().to_string(),
    }
}

/// `-v` clears quietness, so a step writes each opline the loop reaches
/// to stderr; `set break` disables one breakpoint and `set breaks` all,
/// and a restart starts the cycle over with the line's flags, the
/// breakpoints on and quietness as `-v` left it, the one disabled
/// carried enabled (the pinned phpdbg's bytes for `phpdbg -qvb -n`).
#[test]
fn set_breaks_and_quiet_read_as_php_writes_them_across_a_cycle() {
    let served = under_phpdbg(
        "set-quiet",
        &["-qvb", "-n"],
        None,
        "<?php\nfunction f($x) {\n  $y = $x + 1;\n  return $y;\n}\necho f(1), \"\\n\";\necho \"end\\n\";\n",
        "b f\nr\ns\nn\nset break 0 off\nset quiet\nset breaks off\nset breaks\nr\ny\nset breaks\nset quiet\ninfo break\nq\n",
    );
    let file = &served.file;
    assert_eq!(
        (served.out.as_str(), served.err.as_str(), served.status),
        (
            format!(
                "[Successful compilation of {file}]\n\
                 prompt> [Breakpoint #0 added at f]\n\
                 prompt> [Breakpoint #0 in f() at {file}:3, hits: 1]\n\
                 >00003:   $y = $x + 1;\n \
                 00004:   return $y;\n \
                 00005: }}\n\
                 prompt> >00004:   return $y;\n \
                 00005: }}\n \
                 00006: echo f(1), \"\\n\";\n\
                 prompt> >00006: echo f(1), \"\\n\";\n \
                 00007: echo \"end\\n\";\n \
                 00008: \n\
                 prompt> prompt> Quietness off\n\
                 prompt> prompt> Breakpoints off\n\
                 prompt> Do you really want to restart execution? (type y or n): \
                 [Breakpoint #0 in f() at {file}:3, hits: 1]\n\
                 >00003:   $y = $x + 1;\n \
                 00004:   return $y;\n \
                 00005: }}\n\
                 prompt> Breakpoints on\n\
                 prompt> Quietness off\n\
                 prompt> ------------------------------------------------\n\
                 Function Breakpoints:\n\
                 #0\t\tf\n\
                 prompt> "
            )
            .as_str(),
            "L0003 0002 ASSIGN CV1($y) T2\n\
             L0004 0003 RETURN CV1($y)\n\
             L0006 0003 ECHO V0\n",
            Some(0)
        )
    );
}

const BLOCK_PROGRAM: &str = "<?php\necho $x ?? \"unset\", \"\\n\";\nf();\n";

/// An init file's code block runs where its `:>` stands, its lines
/// joined with nothing between them, a comment and a blank line among
/// them skipped, as `phpdbginit code`: what it defines the program
/// finds, and what it raises names that file (the pinned phpdbg's bytes
/// for `phpdbg -qb -n -i<file>`, `-qrb` and `-qrrb`).
#[test]
fn an_init_code_block_runs_as_phpdbginit_code() {
    let init = "<:\n$x = 5;\nfunction f() { echo \"f\\n\"; }\n  echo \"a\";\n# comment\n\n echo $undef, \"b\\n\";\n:>\nev $x\n";
    let block = "a\n[PHP Warning:  Undefined variable $undef in phpdbginit code on line 1]\nb\n5\n";
    let served = under_phpdbg("block-q", &["-qb", "-n"], Some(init), BLOCK_PROGRAM, "q\n");
    let file = &served.file;
    assert_eq!(
        (served.out.as_str(), served.err.as_str(), served.status),
        (
            format!("{block}\n[Successful compilation of {file}]\nprompt> ").as_str(),
            "",
            Some(0)
        )
    );
    let served = under_phpdbg("block-r", &["-qrb", "-n"], Some(init), BLOCK_PROGRAM, "q\n");
    assert_eq!(
        (served.out.as_str(), served.err.as_str(), served.status),
        (
            format!("{block}5\nf\n[Script ended normally]\n{block}prompt> ").as_str(),
            "",
            Some(0)
        )
    );
    let served = under_phpdbg("block-rr", &["-qrrb", "-n"], Some(init), BLOCK_PROGRAM, "");
    assert_eq!(
        (served.out.as_str(), served.err.as_str(), served.status),
        (format!("{block}5\nf\n").as_str(), "", Some(0))
    );
}

/// What a code block leaves pending, a parse error or a throwable
/// nothing catches, is rendered by the next `ev`, which writes nothing
/// else; a throwable nothing catches stops the loop first, at a prompt
/// with no listing of `phpdbginit code` to show. A fatal ends the
/// reading of the init files, its log listing nothing, and stops at no
/// prompt in the first cycle (the pinned phpdbg's bytes for
/// `phpdbg -qb -n -i<file>`).
#[test]
fn what_an_init_code_block_leaves_is_the_next_evs() {
    let compiled = |file: &str| format!("[Successful compilation of {file}]\nprompt> ");
    let served = under_phpdbg(
        "block-parse",
        &["-qb", "-n"],
        Some("<:\n$y =\n:>\nev 2\nev 3\n"),
        BLOCK_PROGRAM,
        "q\n",
    );
    assert_eq!(
        (served.out.as_str(), served.status),
        (
            format!(
                "[PHP Parse error:  syntax error, unexpected end of file in phpdbginit code on line 1]\n\
                 3\n\n{}",
                compiled(&served.file)
            )
            .as_str(),
            Some(255)
        )
    );
    let served = under_phpdbg(
        "block-throw",
        &["-qb", "-n"],
        Some("<:\necho \"a\\n\";\nthrow new Exception(\"boo\");\n:>\nev 1\nev 2\n"),
        BLOCK_PROGRAM,
        "c\nq\n",
    );
    assert_eq!(
        (served.out.as_str(), served.status),
        (
            format!(
                "a\n\
                 [Uncaught Exception in phpdbginit code on line 1: boo]\n\
                 [Could not find information about included file...]\n\
                 prompt> [PHP Fatal error:  Uncaught Exception: boo in phpdbginit code:1\n\
                 Stack trace:\n\
                 #0 {{main}}\n  \
                 thrown in phpdbginit code on line 1]\n\
                 2\n\n{}",
                compiled(&served.file)
            )
            .as_str(),
            Some(255)
        )
    );
    let served = under_phpdbg(
        "block-fatal",
        &["-qb", "-n"],
        Some("<:\nini_set(\"memory_limit\", \"2M\");\nstr_repeat(\"x\", 4 << 20);\n:>\nev 2\n"),
        BLOCK_PROGRAM,
        "c\nq\n",
    );
    assert_eq!(
        (served.out.as_str(), served.err.as_str(), served.status),
        (
            format!(
                "[PHP Fatal error:  Allowed memory size of 2097152 bytes exhausted (tried to allocate 4194336 bytes) in phpdbginit code on line 1\n\
                 Stack trace:\n\
                 #0 phpdbginit code(1): str_repeat('x', 4194304)\n\
                 #1 {{main}}]\n\
                 [Could not find information about included file...]\n\
                 {}prompt> ",
                compiled(&served.file)
            )
            .as_str(),
            "",
            Some(255)
        )
    );
}

/// php runs the init file before it compiles the context, so where the
/// context does not compile, what the file writes comes first, the
/// compiler's log after it, and the prompt stands in the request the
/// file ran in, with what it left; a `print exec` there compiles the
/// context again and writes the log again (the pinned phpdbg's bytes for
/// `phpdbg -qb -n -i<file>`).
#[test]
fn an_init_file_runs_before_a_context_that_does_not_compile() {
    let served = under_phpdbg(
        "init-refused",
        &["-qb", "-n"],
        Some("b 3\nev $x = 3\nprint exec\n"),
        "<?php\n$a = 1;\necho \"prog \", $x ?? \"unset\", \"\\n\";\n$b =\n",
        "ev $x\nprint exec\nc\nr\nq\n",
    );
    let file = &served.file;
    let refused = format!(
        "[PHP Parse error:  syntax error, unexpected end of file in {file} on line 5]\n\
         [Could not find information about included file...]\n"
    );
    assert_eq!(
        (served.out.as_str(), served.status),
        (
            format!(
                "[Breakpoint #0 added at {file}:3]\n3\n\n\
                 {refused}{refused}prompt> 3\n\
                 prompt> {refused}prompt> [Not running]\n\
                 prompt> {refused}prompt> "
            )
            .as_str(),
            Some(255)
        )
    );
}

/// `print exec` in the init file compiles the context there, with its
/// notice, once a cycle, and the context's own compile after the file
/// writes it again (the pinned phpdbg's bytes for `phpdbg -qb -n
/// -i<file>`).
#[test]
fn print_exec_in_the_init_file_compiles_the_context_first() {
    let served = under_phpdbg(
        "init-print",
        &["-qb", "-n"],
        Some("print exec\nev 5\nprint exec\n"),
        "<?php\n$a = 1;\necho \"prog \", $x ?? \"unset\", \"\\n\";\n",
        "q\n",
    );
    let file = &served.file;
    assert_eq!(
        (
            served.out.as_str(),
            served.err.matches("$_main:").count(),
            served.status
        ),
        (
            format!(
                "[Successful compilation of {file}]\n\
                 [Context {file} (7 ops)]\n\
                 5\n\n\
                 [Context {file} (7 ops)]\n\
                 [Successful compilation of {file}]\n\
                 prompt> "
            )
            .as_str(),
            2,
            Some(0)
        )
    );
}

/// `run` in the init file compiles the context and runs it there, the
/// breakpoints the file set not stopping it, and the file's next lines
/// run after it, an `ev` among them writing nothing; under `-rr` the
/// startup run then finds the program executing and asks, and the end
/// of the input quits. Without it php's cycle ends and starts again with
/// the init file, which runs the program again, for as long as the
/// session lasts (the pinned phpdbg's bytes for `phpdbg -qrrb -n
/// -i<file>`, and the first two cycles of `phpdbg -qb -n -i<file>`).
#[test]
fn run_in_the_init_file_runs_the_program_there() {
    let init = "ev 1\nb 3\nrun\nev 2\nb 3\n";
    let program = "<?php\n$a = 1;\necho \"prog \", $x ?? \"unset\", \"\\n\";\n";
    let cycle = |file: &str| {
        format!(
            "1\n\n[Breakpoint #0 added at {file}:3]\n\
             [Successful compilation of {file}]\n\
             prog unset\n\
             [Breakpoint at {file}:3 exists]\n"
        )
    };
    let served = under_phpdbg("init-run-rr", &["-qrrb", "-n"], Some(init), program, "q\n");
    assert_eq!(
        (served.out.as_str(), served.status),
        (
            format!(
                "{}Do you really want to restart execution? (type y or n): \
                 Please enter either y (yes) or n (no): ",
                cycle(&served.file)
            )
            .as_str(),
            Some(0)
        )
    );
    let served = under_phpdbg_with(
        "init-run",
        &["-qb", "-n"],
        Some(init),
        program,
        "q\n",
        &Setup {
            limit: Some(4096),
            ..Setup::default()
        },
    );
    let file = &served.file;
    let cycle = cycle(file);
    let expected = format!(
        "{cycle}[Successful compilation of {file}]\n\
         [Script ended normally]\n\
         {cycle}[Script ended normally]\n"
    );
    assert!(served.out.starts_with(&expected), "{}", served.out);
}

/// With no `-i` and no `-I`, the init files are `PHP_CONFIG_FILE_PATH`'s
/// `.phpdbginit`, one in each directory of `PHP_INI_SCAN_DIR`, and the
/// working directory's; php's walk of `PHP_INI_SCAN_DIR` cuts it at its
/// first `:` where it stands in the environment, so the program reads
/// the first directory alone, and so does every cycle after the first
/// (the pinned phpdbg's bytes for `phpdbg -qb -n`).
#[test]
fn the_default_init_files_are_read_where_php_reads_them() {
    let served = under_phpdbg_with(
        "init-default",
        &["-qb", "-n"],
        None,
        "<?php\necho getenv(\"PHP_INI_SCAN_DIR\"), \"\\n\";\n",
        "r\nr\nq\n",
        &Setup {
            files: &[
                ("a/.phpdbginit", "ev \"A\"\n"),
                ("b/.phpdbginit", "ev \"B\"\n"),
                (".phpdbginit", "ev \"W\"\n"),
            ],
            env: &[("PHP_INI_SCAN_DIR", "{dir}/a:{dir}/b")],
            limit: None,
        },
    );
    let file = &served.file;
    let dir = file.strip_suffix("/t.php").unwrap_or_default();
    assert_eq!(
        (served.out.as_str(), served.status),
        (
            format!(
                "A\nB\nW\n\n[Successful compilation of {file}]\n\
                 prompt> {dir}/a\n[Script ended normally]\nA\nW\n\
                 prompt> {dir}/a\n[Script ended normally]\nA\nW\n\
                 prompt> "
            )
            .as_str(),
            Some(0)
        )
    );
}
