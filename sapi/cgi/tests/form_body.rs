//! A form body past what the request's temp stream keeps in memory spills
//! into `upload_tmp_dir`, and the form handler reads it back a `BUFSIZ`
//! block at a time, as `php_std_post_handler` reads
//! `SG(request_info).request_body`, so the body never stands in the
//! process's memory whole, mbstring linked and its `encoding_translation`
//! off; and one whose spill has nowhere to go spills where php's
//! does. Held in a test binary of its own, since
//! `getrusage(RUSAGE_CHILDREN)` is the peak of every child its process
//! waited for, and the spill's child peaks far under the large form's.

/// 251 MiB of form, 262144 variables of the same name, each a thousand
/// bytes: `$_POST` holds one, and the process's peak resident set stays
/// under half the body. The pinned php-cgi 8.5.12 writes `1 1000` for the
/// same body under the same switches, its peak resident set 26.5 MB on
/// arm64 macOS; the engine's debug build peaked at 315 MB while its form
/// handler copied the body out of the stream whole, and at 53.8 MB once
/// it read the body a block at a time, as it peaks for a body of one
/// variable.
#[test]
fn a_spilled_form_is_parsed_without_holding_it_whole() {
    use std::io::Write as _;

    const VARIABLES: usize = 1 << 18;
    let mut variable = b"a=".to_vec();
    variable.resize(1002, b'x');
    variable.push(b'&');
    let sent = variable.len() * VARIABLES;
    let dir = std::env::temp_dir().join(format!("sapi-cgi-form-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.php");
    std::fs::write(
        &file,
        "<?php echo count($_POST), ' ', strlen($_POST['a']), \"\\n\";\n",
    )
    .unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_lela-cgi"))
        .env_clear()
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("REDIRECT_STATUS", "200")
        .env("REQUEST_METHOD", "POST")
        .env("CONTENT_TYPE", "application/x-www-form-urlencoded")
        .env("CONTENT_LENGTH", sent.to_string())
        .env("SCRIPT_FILENAME", &file)
        .args([
            "-n",
            "-q",
            "-d",
            "post_max_size=0",
            "-d",
            "max_input_vars=1000000",
        ])
        .arg("-d")
        .arg(format!("upload_tmp_dir={}", dir.display()))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // The client, a thread of the test's own process, which forks nothing.
    #[allow(clippy::disallowed_methods)]
    let writer = std::thread::spawn(move || {
        for _ in 0..VARIABLES {
            stdin.write_all(&variable).unwrap();
        }
    });
    writer.join().unwrap();
    let out = child.wait_with_output().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.ends_with("\r\n\r\n1 1000\n"), "{stdout:?}");
    let usage =
        nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_CHILDREN).unwrap();
    // `ru_maxrss` counts bytes on macOS and kilobytes on Linux.
    let peak = if cfg!(target_os = "macos") {
        usage.max_rss()
    } else {
        usage.max_rss() * 1024
    };
    assert!(
        usize::try_from(peak).unwrap() < sent / 2,
        "php-cgi's peak resident set was {peak} bytes"
    );
}

/// A form body past the 16 KiB a temp stream keeps in memory, whose
/// `upload_tmp_dir` does not exist, spills into the system's temporary
/// directory after php's notice, `php_open_temporary_fd_ex`'s, and the
/// form handler then registers none of it, while `php://input` still
/// reads it whole. The pinned php-cgi 8.5.12 writes the same bytes for
/// the same body and switches (`-n -q -d html_errors=0`).
#[test]
fn a_form_whose_spill_directory_is_missing_spills_after_php_s_notice() {
    use std::io::Write as _;

    let dir = std::env::temp_dir().join(format!("sapi-cgi-spill-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.php");
    std::fs::write(
        &file,
        "<?php var_dump(count($_POST), strlen(file_get_contents('php://input')));\n",
    )
    .unwrap();
    let mut body = b"a=1&big=".to_vec();
    body.resize(body.len() + 20_000, b'x');
    body.extend_from_slice(b"&b=two");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_lela-cgi"))
        .env_clear()
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("REDIRECT_STATUS", "200")
        .env("REQUEST_METHOD", "POST")
        .env("CONTENT_TYPE", "application/x-www-form-urlencoded")
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("SCRIPT_FILENAME", &file)
        .args(["-n", "-q", "-d", "html_errors=0", "-d"])
        .arg(format!("upload_tmp_dir={}", dir.join("missing").display()))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&body).unwrap();
    let out = child.wait_with_output().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "Content-type: text/html; charset=UTF-8\r\n\r\n\nNotice: PHP Request Startup: file created in the system's temporary directory in Unknown on line 0\nint(0)\nint(20014)\n"
    );
}
