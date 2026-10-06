//! `.user.ini` read in each directory from `DOCUMENT_ROOT` down to the
//! script's, or in the script's alone outside the root, and set after the
//! php.ini's `[PATH=]` tables as `PHP_INI_PERDIR` at the htaccess stage:
//! a deeper file's value wins, a `[PATH=]` entry is not overridden, a
//! `PHP_INI_SYSTEM` entry is refused, and a `PHP_INI_ALL` one still
//! changes at runtime. The bytes are the oracle's php-cgi serving the same
//! requests (#1699); a fence cannot carry an ini (#1634).

use std::path::Path;
use std::process::Command;

/// What php-cgi writes serving `script` in `dir` with `DOCUMENT_ROOT` its
/// `www/`.
fn served(dir: &Path, script: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .env_clear()
        .env("PHP_INI_SCAN_DIR", dir.join("scan"))
        .env("REDIRECT_STATUS", "200")
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("REQUEST_METHOD", "GET")
        .env("DOCUMENT_ROOT", format!("{}/", dir.join("www").display()))
        .env("SCRIPT_FILENAME", dir.join(script))
        .arg("-c")
        .arg(dir.join("php.ini"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn each_user_ini_from_the_document_root_down_is_set_per_dir() {
    let dir = std::env::temp_dir().join(format!("sapi-cgi-user-ini-{}", std::process::id()));
    for sub in ["scan", "www/sub", "out"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let www = dir.join("www").display().to_string();
    let files = [
        ("php.ini", format!("[PATH={www}/sub]\nprecision=3\n")),
        (
            ".user.ini",
            "precision=5\nserialize_precision=5\nmax_input_time=11\n".to_string(),
        ),
        (
            "www/.user.ini",
            "serialize_precision=6\ndefault_mimetype=\"text/plain\"\nallow_url_fopen=0\n"
                .to_string(),
        ),
        (
            "www/sub/.user.ini",
            "serialize_precision=7\nmax_input_time=12\nprecision=8\n".to_string(),
        ),
        ("out/.user.ini", "serialize_precision=4\n".to_string()),
    ];
    for (name, text) in files {
        std::fs::write(dir.join(name), text).unwrap();
    }
    let program = "<?php var_dump(ini_get(\"precision\"), ini_get(\"serialize_precision\"), \
        ini_get(\"max_input_time\"), ini_get(\"allow_url_fopen\"), \
        ini_set(\"serialize_precision\", \"9\"), ini_get(\"default_mimetype\"));\n";
    for script in ["www/sub/t.php", "www/t.php", "out/t.php"] {
        std::fs::write(dir.join(script), program).unwrap();
    }
    let sub = served(&dir, "www/sub/t.php");
    let top = served(&dir, "www/t.php");
    let out = served(&dir, "out/t.php");
    std::fs::remove_dir_all(&dir).ok();
    let plain = "X-Powered-By: PHP/8.5.9\r\nContent-type: text/plain; charset=UTF-8\r\n\r\n";
    let html = "X-Powered-By: PHP/8.5.9\r\nContent-type: text/html; charset=UTF-8\r\n\r\n";
    assert_eq!(
        sub,
        format!(
            "{plain}string(1) \"3\"\nstring(1) \"7\"\nstring(2) \"12\"\nstring(1) \"1\"\n\
             string(1) \"7\"\nstring(10) \"text/plain\"\n"
        )
    );
    assert_eq!(
        top,
        format!(
            "{plain}string(2) \"14\"\nstring(1) \"6\"\nstring(2) \"-1\"\nstring(1) \"1\"\n\
             string(1) \"6\"\nstring(10) \"text/plain\"\n"
        )
    );
    assert_eq!(
        out,
        format!(
            "{html}string(2) \"14\"\nstring(1) \"4\"\nstring(2) \"-1\"\nstring(1) \"1\"\n\
             string(1) \"4\"\nstring(9) \"text/html\"\n"
        )
    );
}
