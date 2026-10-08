//! `-i` is php-cgi's `php_print_info` in a request of its own
//! (`Z/sapi/cgi/cgi_main.c`, `case 'i'`), printed before `main` would open
//! a primary script or read stdin: whether the line names a file that
//! does not exist or none at all, the page is phpinfo's under the head,
//! and the status 0, as the oracle's php-cgi 8.5.9 answers both.

use std::process::{Command, Stdio};

/// stdout and the status of `php-cgi -n -i <words>` with stdin empty.
fn info(words: &[&str]) -> (String, Option<i32>) {
    let out = Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .env_clear()
        .args(["-n", "-i"])
        .args(words)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code(),
    )
}

#[test]
fn info_opens_no_program_and_reads_no_stdin() {
    let missing = std::env::temp_dir().join(format!("sapi-cgi-info-{}.php", std::process::id()));
    for words in [&[][..], &[missing.to_str().unwrap()]] {
        let (stdout, status) = info(words);
        assert!(
            stdout.starts_with(
                "X-Powered-By: PHP/8.5.9\r\nContent-type: text/html; charset=UTF-8\r\n\r\n<!DOCTYPE html"
            ),
            "{words:?}: {stdout}"
        );
        assert_eq!(status, Some(0), "{words:?}");
    }
}
