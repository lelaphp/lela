//! `-v` and `-S` start php's modules and serve no request: php answers
//! `-v` after `php_module_startup` alone (`Z/sapi/cli/php_cli.c:638-641`)
//! and its built-in server starts none before its first client, so no
//! extension's `rinit` runs and `session.auto_start` opens no session.
//! `-m` lists the modules in a request of its own (`php_cli.c:643-655`),
//! whose `rinit` does (#1967).

mod server;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use server::Server;

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
    let save_path = format!("session.save_path={}", dir.display());
    let server = Server::start_on(
        std::iter::repeat_with(server::free_port),
        "start-up",
        &[],
        &[],
        &["-d", "session.auto_start=1", "-d", &save_path],
    );
    let first = server.log().into_iter().next().unwrap_or_default();
    drop(server);
    assert!(first.ends_with(") started"), "{first}");
    assert_eq!(sessions(&dir), Vec::<String>::new());
}

/// A port another process holds when php -S binds it is given up for a
/// fresh one (#2653): php says it is in use and exits, and the server is
/// started again on the next port offered.
#[test]
fn a_server_whose_port_is_taken_starts_on_a_fresh_one() {
    let held = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
    let taken = held.local_addr().map_or(0, |bound| bound.port());
    let ports = std::iter::once(taken).chain(std::iter::repeat_with(server::free_port));
    let server = Server::start_on(
        ports,
        "taken",
        &[("i.php", "<?php echo 'served';")],
        &[],
        &[],
    );
    assert_ne!(server.address, format!("127.0.0.1:{taken}"));
    assert_eq!(server.get("/i.php"), "served");
}
