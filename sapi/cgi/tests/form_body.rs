//! A form body past what the request's temp stream keeps in memory spills
//! into `upload_tmp_dir`, and the form handler reads it back a `BUFSIZ`
//! block at a time, as `php_std_post_handler` reads
//! `SG(request_info).request_body`, so the body never stands in the
//! process's memory whole, mbstring linked and its `encoding_translation`
//! off (#3651). Held in a test binary of its own, since
//! `getrusage(RUSAGE_CHILDREN)` is the peak of every child its process
//! waited for.

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
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_php-cgi"))
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
