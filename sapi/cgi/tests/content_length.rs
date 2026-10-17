//! php-cgi reads `CONTENT_LENGTH` with C's `strtol` under the process's
//! `LC_CTYPE`, `C.UTF-8`, whose `isspace` on macOS counts `0xA0`: a length
//! opening with that byte is read past it, as the pinned php-cgi reads it
//! (#2712). An environment value may hold a byte no `lines.toml` string
//! carries, so the shell's half is held here.

#[cfg(target_os = "macos")]
#[test]
fn a_content_length_opening_with_0xa0_is_read_past_it() {
    use std::ffi::OsStr;
    use std::io::Write as _;
    use std::os::unix::ffi::OsStrExt as _;

    let dir = std::env::temp_dir().join(format!("sapi-cgi-length-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.php");
    std::fs::write(
        &file,
        "<?php echo strlen(file_get_contents('php://input')), \"\\n\";\n",
    )
    .unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .env_clear()
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("REDIRECT_STATUS", "200")
        .env("REQUEST_METHOD", "POST")
        .env("CONTENT_TYPE", "text/plain")
        .env("CONTENT_LENGTH", OsStr::from_bytes(b"\xa05"))
        .env("SCRIPT_FILENAME", &file)
        .args(["-n", "-q"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"hello world")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.ends_with("\r\n\r\n5\n"), "{stdout:?}");
}
