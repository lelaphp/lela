//! A file outside `open_basedir` over a real socket against the binary
//! (#4351): a program is not opened, as the pinned php-fpm refuses one in
//! `php_fopen_primary_script` (`Z/main/fopen_wrappers.c:468-473`), with
//! its page and its line in the error log; and a static file is not sent,
//! answered 404 as a file makala may not serve is, which no oracle holds.
//! A file within the base directory is served either way.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use std::time::Duration;

use common::{Request, Response, Server, eventually, read_response};

/// `GET target` on a connection of its own, its response read whole.
fn ask(server: &Server, target: &str) -> Response {
    let mut stream = server.send(&Request::get(target).host("localhost").close());
    read_response(&mut stream)
}

/// The base directory holds `in.php` and `in.txt`, the root outside it
/// `out.php` and `out.txt`; makala runs under `-d open_basedir` naming the
/// base directory alone.
#[test]
fn a_file_outside_open_basedir_is_neither_run_nor_sent() {
    let root = common::root(
        "open-basedir",
        &[("out.php", "<?php echo 'ran';"), ("out.txt", "outside")],
    );
    std::fs::create_dir_all(root.join("base")).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(root.join("base/in.php"), "<?php echo 'ran';").unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(root.join("base/in.txt"), "inside").unwrap_or_else(|e| panic!("{e}"));
    let root = root.canonicalize().unwrap_or_else(|e| panic!("{e}"));
    let errors = root.join("error.log");
    let errors_arg = errors.to_string_lossy().into_owned();
    let basedir = format!("open_basedir={}", root.join("base").display());
    let server = Server::with_args(&root, &["-d", &basedir, "--error-log", &errors_arg]);

    let inside = ask(&server, "/base/in.php");
    assert!(inside.status.contains(" 200 "), "{inside:?}");
    assert_eq!(inside.body, b"ran");
    let sent = ask(&server, "/base/in.txt");
    assert!(sent.status.contains(" 200 "), "{sent:?}");
    assert_eq!(sent.body, b"inside");

    let program = ask(&server, "/out.php");
    assert!(program.status.contains(" 404 "), "{program:?}");
    assert_eq!(program.body, b"No input file specified.\n");
    let line = format!(
        "Unable to open primary script: {}/out.php (Operation not permitted)",
        root.display()
    );
    let logged = || std::fs::read_to_string(&errors).unwrap_or_default();
    assert!(
        eventually(Duration::from_secs(10), || logged().contains(&line)),
        "{}",
        logged()
    );

    let file = ask(&server, "/out.txt");
    assert!(file.status.contains(" 404 "), "{file:?}");
    assert!(file.body.is_empty(), "{file:?}");
}
