//! `cgi.redirect_status_env` set to the empty string, by a php.ini or by
//! `-d`, names a variable no environment has, so a server's request is
//! refused with the Security Alert, as `CGIG(redirect_status_env)` set to
//! `""` refuses it; unset, the entry is `NULL` and `REDIRECT_STATUS` lets
//! the request through. The bytes are the oracle's php-cgi serving the
//! same request (#1698); a fence cannot carry an ini (#1634).

use std::path::Path;
use std::process::Command;

/// php-cgi's exit and stdout serving `t.php` in `dir` with `REDIRECT_STATUS`
/// set, under `args`.
fn served(dir: &Path, args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .env_clear()
        .env("PHP_INI_SCAN_DIR", dir.join("scan"))
        .env("REDIRECT_STATUS", "200")
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("REQUEST_METHOD", "GET")
        .env("SCRIPT_FILENAME", dir.join("t.php"))
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

const ALERT: &str = "<b>Security Alert!</b> The PHP CGI cannot be accessed directly.\n\n\
<p>This PHP CGI binary was compiled with force-cgi-redirect enabled.  This\n\
means that a page will only be served up if the REDIRECT_STATUS CGI variable is\n\
set, e.g. via an Apache Action directive.</p>\n\
<p>For more information as to <i>why</i> this behaviour exists, see the <a href=\"https://www.php.net/security.cgi-bin\">\
manual page for CGI security</a>.</p>\n\
<p>For more information about changing this behaviour or re-enabling this webserver,\n\
consult the installation file that came with this distribution, or visit \n\
<a href=\"https://www.php.net/install.windows\">the manual page</a>.</p>\n";

#[test]
fn an_empty_redirect_status_env_refuses_and_an_unset_one_falls_back() {
    let dir = std::env::temp_dir().join(format!("sapi-cgi-redirect-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("scan")).unwrap();
    std::fs::write(dir.join("t.php"), "<?php echo \"ok\\n\";\n").unwrap();
    std::fs::write(dir.join("php.ini"), "cgi.redirect_status_env=\n").unwrap();
    let ini = dir.join("php.ini").display().to_string();
    let from_ini = served(&dir, &["-c", &ini]);
    let from_d = served(&dir, &["-n", "-d", "cgi.redirect_status_env="]);
    let unset = served(&dir, &["-n"]);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(from_ini, (Some(255), ALERT.to_string()));
    assert_eq!(from_d, (Some(255), ALERT.to_string()));
    let head = "X-Powered-By: PHP/8.5.9\r\nContent-type: text/html; charset=UTF-8\r\n\r\n";
    assert_eq!(unset, (Some(0), format!("{head}ok\n")));
}
