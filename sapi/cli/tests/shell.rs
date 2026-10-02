//! `-a`, the interactive shell the readline extension provides, fed
//! through a pipe and held to what the oracle at 8.5.9 prints under
//! `php -n -a` for the same stdin: libedit reads the lines and writes no
//! prompt where stdin is no terminal, each block of code is served as
//! `zend_eval_stringl` serves it, a fatal is rendered and the shell goes
//! on, what a block leaves uncaught is a warning, `#name=value` sets an
//! entry, the code of a prompt's backticks is served for its output, and
//! the status is what the request left.

use std::io::Write;
use std::process::{Command, Stdio};

/// stdout and stderr as one stream, and the exit code, of `php -n
/// <words> -a` with `stdin` on its stdin and the history kept apart.
fn shell(stdin: &str, words: &[&str]) -> (String, i32) {
    let history = std::env::temp_dir().join(format!("php-shell-history-{}", std::process::id()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_php"))
        .arg("-n")
        .args(words)
        .arg("-a")
        .env("PHP_HISTFILE", &history)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("running php: {e}"));
    if let Some(mut pipe) = child.stdin.take() {
        let _ = pipe.write_all(stdin.as_bytes());
    }
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("waiting for php: {e}"));
    let _ = std::fs::remove_file(&history);
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.code().unwrap_or(-1))
}

#[test]
fn a_fatal_and_an_uncaught_throwable_leave_the_shell_going() {
    assert_eq!(
        shell(
            "echo 1;\nfunction f() {}\nfunction f() {}\necho 2;\nthrow new Exception(\"x\");\nexit(3);\necho \"a\";\n",
            &[]
        ),
        (
            String::from(
                "Interactive shell\n\n1\n\nFatal error: Cannot redeclare function f() (previously declared in php shell code:1) in php shell code on line 1\nStack trace:\n#0 {main}\n2\n\nWarning: Uncaught Exception: x in php shell code:1\nStack trace:\n#0 {main}\n  thrown in php shell code on line 1\na\n"
            ),
            3
        )
    );
}

#[test]
fn blocks_gather_until_the_code_can_end_and_quit_leaves() {
    assert_eq!(
        shell(
            "$s = <<<EOT\n  one\n  EOT;\necho $s;\n#precision=3\necho 1/3;\nif (true) {\n  echo 'b';\n}\n'q\n';\nquit\necho 'never';\n",
            &[]
        ),
        (
            String::from(
                "Interactive shell\n\none\n0.333\nb\n\nParse error: syntax error, unexpected string content \"q\", expecting end of file in php shell code on line 1\n\nParse error: syntax error, unexpected string content \";\", expecting end of file in php shell code on line 1\n"
            ),
            255
        )
    );
}

#[test]
fn the_prompt_s_code_is_served_for_each_prompt() {
    assert_eq!(
        shell("echo 'p';\n", &["-d", "cli.prompt=`echo 'x';`> "]),
        (String::from("Interactive shell\n\nxxp\n"), 0)
    );
}
