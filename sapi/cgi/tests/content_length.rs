//! php-cgi reads `CONTENT_LENGTH` with C's `strtol` under the process's
//! `LC_CTYPE`, `C.UTF-8`, whose `isspace` on macOS counts `0xA0`: a length
//! opening with that byte is read past it, as the pinned php-cgi reads it.
//! An environment value may hold a byte no `lines.toml` string
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

/// A negative `CONTENT_LENGTH` reads stdin to its end, as the pinned
/// php-cgi's wrapped `size_t` does, but the host hands the body out only
/// as the request pulls it, so 256 MiB on stdin past a `post_max_size` of
/// 64K never stand in the process's memory: the form reader stops
/// a block past the limit with php's warning and the request reads no
/// more of it. The process's peak resident set stays under half the body,
/// where a host that read the body before the request would hold all of
/// it.
#[test]
fn a_negative_content_length_is_read_no_further_than_the_request_pulls() {
    use std::io::Write as _;

    const SENT: usize = 256 << 20;
    let dir = std::env::temp_dir().join(format!("sapi-cgi-negative-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.php");
    std::fs::write(&file, "<?php echo $_POST === [] ? 0 : 1, \"\\n\";\n").unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .env_clear()
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("REDIRECT_STATUS", "200")
        .env("REQUEST_METHOD", "POST")
        .env("CONTENT_TYPE", "application/x-www-form-urlencoded")
        .env("CONTENT_LENGTH", "-1")
        .env("SCRIPT_FILENAME", &file)
        .args(["-n", "-d", "post_max_size=64K", "-d", "html_errors=0"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // The client, a thread of the test's own process, which forks nothing.
    #[allow(clippy::disallowed_methods)]
    let writer = std::thread::spawn(move || {
        // The request stops reading once its reader stops; a pipe it
        // closed refuses the rest.
        let chunk = vec![b'x'; 1 << 16];
        if stdin.write_all(b"a=").is_err() {
            return;
        }
        for _ in 0..SENT / chunk.len() {
            if stdin.write_all(&chunk).is_err() {
                return;
            }
        }
    });
    writer.join().unwrap();
    let out = child.wait_with_output().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.ends_with(
            "\n\nWarning: PHP Request Startup: Actual POST length does not match Content-Length, and exceeds 65536 bytes in Unknown on line 0\n0\n"
        ),
        "{stdout:?}"
    );
    let usage =
        nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_CHILDREN).unwrap();
    // `ru_maxrss` counts bytes on macOS and kilobytes on Linux.
    let peak = if cfg!(target_os = "macos") {
        usage.max_rss()
    } else {
        usage.max_rss() * 1024
    };
    assert!(
        usize::try_from(peak).unwrap() < SENT / 2,
        "php-cgi's peak resident set was {peak} bytes"
    );
}
