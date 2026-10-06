//! php-cgi reads its own `cgi.*` entries from the engine's ini, a
//! php.ini's value included, as `INI_BOOL` and `CGIG()` read them in php:
//! a `-c` file's `cgi.redirect_status_env` names the variable the
//! redirect check asks for, and its `cgi.check_shebang_line=0` leaves a
//! `#!` line in the output. The witness lane serves a fence under the
//! recipe's `-n` with no ini of its own (#1634), so the oracle's bytes are
//! held here, from `php-cgi -c php.ini` (#1693).

use std::process::Command;

#[test]
fn a_php_ini_s_cgi_entries_are_the_ones_php_cgi_reads() {
    let dir = std::env::temp_dir().join(format!("sapi-cgi-ini-file-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("scan")).unwrap();
    std::fs::write(
        dir.join("php.ini"),
        "cgi.redirect_status_env=MY_STATUS\ncgi.check_shebang_line=0\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("sb.php"),
        "#!/usr/bin/php\n<?php var_dump($_GET);\n",
    )
    .unwrap();
    let php_cgi = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_php-cgi"));
        command
            .env_clear()
            .env("PHP_INI_SCAN_DIR", dir.join("scan"))
            .current_dir(&dir)
            .arg("-c")
            .arg(dir.join("php.ini"));
        command
    };
    let shell = php_cgi().args(["sb.php", "a=1"]).output().unwrap();
    let server = php_cgi()
        .env("REDIRECT_STATUS", "200")
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("REQUEST_METHOD", "GET")
        .env("SCRIPT_FILENAME", dir.join("sb.php"))
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        String::from_utf8_lossy(&shell.stdout),
        "X-Powered-By: PHP/8.5.9\r\nContent-type: text/html; charset=UTF-8\r\n\r\n\
         #!/usr/bin/php\narray(2) {\n  [\"sb_php\"]=>\n  string(0) \"\"\n  [\"a\"]=>\n  string(1) \"1\"\n}\n"
    );
    let server = String::from_utf8_lossy(&server.stdout);
    assert!(
        server.starts_with("<b>Security Alert!</b> The PHP CGI cannot be accessed directly."),
        "{server:?}"
    );
}
