//! A php.ini's `[HOST=]` table of the request's `SERVER_NAME`, lowered,
//! and its `[PATH=]` tables along the script's directory, set at
//! `sapi_cgi_activate` as `PHP_INI_SYSTEM`, so `ini_set` refuses them
//! after. The witness lane serves a fence under the recipe's `-n` with
//! no ini of its own (#1634), so the oracle's bytes are held here, from
//! `php-cgi -c php.ini` serving the same request (#1692).

use std::path::Path;
use std::process::Command;

/// What php-cgi writes serving `script` for `server_name` under a
/// php.ini of three tables in `dir`.
fn served(dir: &Path, script: &str, server_name: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .env_clear()
        .env("PHP_INI_SCAN_DIR", dir.join("scan"))
        .env("REDIRECT_STATUS", "200")
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("REQUEST_METHOD", "GET")
        .env("SCRIPT_FILENAME", dir.join(script))
        .env("SERVER_NAME", server_name)
        .arg("-c")
        .arg(dir.join("php.ini"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_host_s_table_and_each_directory_s_are_set_for_the_request() {
    let dir = std::env::temp_dir().join(format!("sapi-cgi-sections-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("www/sub")).unwrap();
    std::fs::create_dir_all(dir.join("scan")).unwrap();
    let www = dir.join("www").display().to_string();
    std::fs::write(
        dir.join("php.ini"),
        format!(
            "precision=10\n[PATH={www}]\nprecision=5\n[PATH={www}/sub/]\nprecision=3\n\
             [HOST=Example.COM]\nserialize_precision=7\n"
        ),
    )
    .unwrap();
    let program = "<?php var_dump(ini_get('precision'), ini_get('serialize_precision'), ini_set('precision', '9'));\n";
    std::fs::write(dir.join("www/sub/t.php"), program).unwrap();
    std::fs::write(dir.join("www/t.php"), program).unwrap();
    let sub = served(&dir, "www/sub/t.php", "Example.com");
    let top = served(&dir, "www/t.php", "other");
    std::fs::remove_dir_all(&dir).ok();
    let head = "X-Powered-By: PHP/8.5.9\r\nContent-type: text/html; charset=UTF-8\r\n\r\n";
    assert_eq!(
        sub,
        format!("{head}string(1) \"3\"\nstring(1) \"7\"\nbool(false)\n")
    );
    assert_eq!(
        top,
        format!("{head}string(1) \"5\"\nstring(2) \"-1\"\nbool(false)\n")
    );
}
