//! `-v` and `-S` start php's modules and serve no request: php answers
//! `-v` after `php_module_startup` alone (`Z/sapi/cli/php_cli.c:638-641`)
//! and its built-in server starts none before its first client, so no
//! extension's `rinit` runs and `session.auto_start` opens no session.
//! `-m` lists the modules in a request of its own (`php_cli.c:643-655`),
//! whose `rinit` does (#1967).

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A directory of its own for `name`, empty.
fn save_path(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("start-up-{name}"));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    dir
}

/// The session files under `dir`.
fn sessions(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("sess_"))
        .collect()
}

/// `php -n` under `session.auto_start` saving to `dir`, then `args`.
fn php(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_php"));
    command
        .arg("-n")
        .args(["-d", "session.auto_start=1", "-d"])
        .arg(format!("session.save_path={}", dir.display()))
        .args(args)
        .stdin(Stdio::null());
    command
}

#[test]
fn the_version_starts_no_session() {
    let dir = save_path("version");
    let out = php(&dir, &["-v"])
        .output()
        .unwrap_or_else(|e| panic!("running php -v: {e}"));
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("PHP "),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(sessions(&dir), Vec::<String>::new());
}

#[test]
fn the_module_listing_starts_its_request_s_session() {
    let dir = save_path("modules");
    let out = php(&dir, &["-m"])
        .output()
        .unwrap_or_else(|e| panic!("running php -m: {e}"));
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(sessions(&dir).len(), 1);
}

#[test]
fn the_server_starts_no_session_before_its_first_client() {
    let dir = save_path("server");
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|probe| probe.local_addr())
        .map_or_else(|e| panic!("finding a free port: {e}"), |bound| bound.port());
    let mut child = php(&dir, &["-S", &format!("127.0.0.1:{port}"), "-t"])
        .arg(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("running php -S: {e}"));
    let stderr = child.stderr.take().unwrap_or_else(|| panic!("no stderr"));
    let first = BufReader::new(stderr).lines().next();
    child.kill().ok();
    child.wait().ok();
    let first = first.and_then(Result::ok).unwrap_or_default();
    assert!(first.ends_with(") started"), "{first}");
    assert_eq!(sessions(&dir), Vec::<String>::new());
}
