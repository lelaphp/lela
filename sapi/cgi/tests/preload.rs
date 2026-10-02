//! The request `opcache.preload`'s file is served in sends no head:
//! `accel_finish_startup_preload` sets `SG(headers_sent)` and
//! `SG(request_info).no_headers` once it started, so what the file echoes
//! reaches stdout bare and its `header()` reaches no one. The witness lane
//! serves a fence under the recipe's ini, which preloads nothing, so it is
//! held here, to the oracle's whole stdout under `php-cgi -n [-q] -d
//! opcache.enable=1 -d opcache.preload=...` (#1633).

fn served(quiet: bool) -> String {
    let dir = std::env::temp_dir().join(format!("sapi-cgi-preload-{}-{quiet}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let preload = dir.join("preload.php");
    std::fs::write(
        &preload,
        "<?php header('X-A: 1'); echo \"pre\\n\"; function pf() { return 1; }\n",
    )
    .unwrap();
    let main = dir.join("main.php");
    std::fs::write(&main, "<?php echo pf(), \"\\n\";\n").unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_php-cgi"));
    command.env_clear().arg("-n");
    if quiet {
        command.arg("-q");
    }
    let out = command
        .args(["-d", "opcache.enable=1", "-d"])
        .arg(format!("opcache.preload={}", preload.display()))
        .arg(&main)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_preload_request_sends_no_head() {
    assert_eq!(served(true), "pre\n1\n");
    assert_eq!(
        served(false),
        "pre\nX-Powered-By: PHP/8.5.9\r\nContent-type: text/html; charset=UTF-8\r\n\r\n1\n"
    );
}
