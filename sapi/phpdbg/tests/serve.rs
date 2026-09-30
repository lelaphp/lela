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
