//! Static files (#2346) over a real socket against the binary, by
//! `php -S`'s path rules (`php_sapi::vpath`): a file that is not PHP is
//! served with the type php's table gives its extension, a `.php` file is
//! run and never sent as its source, and no path, `..` or encoded, leaves
//! the document root.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use common::{Response, Server, read_response, read_until_closed};

/// `method target` on a connection of its own, its response read whole.
fn ask(server: &Server, method: &str, target: &str) -> Response {
    let mut stream = server.connect();
    write!(
        stream,
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap_or_else(|e| panic!("{e}"));
    read_response(&mut stream)
}

/// A static file is its bytes, its length and the type its extension
/// has, `; charset=UTF-8` after a `text/` one as `php -S` writes it; a
/// directory is its `index.html`; `HEAD` has the head alone; and a method
/// that writes is refused with 405 and the methods `php -S` allows.
#[test]
fn a_static_file_is_served_with_its_mime_type() {
    let root = common::root(
        "static",
        &[("style.css", "p { color: red }"), ("image.png", "\u{1}PNG")],
    );
    std::fs::create_dir_all(root.join("docs")).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(root.join("docs/index.html"), "<p>docs</p>").unwrap_or_else(|e| panic!("{e}"));
    let server = Server::start(&root);
    for (target, content_type, body) in [
        ("/style.css", "text/css; charset=UTF-8", "p { color: red }"),
        ("/image.png", "image/png", "\u{1}PNG"),
        ("/docs/", "text/html; charset=UTF-8", "<p>docs</p>"),
    ] {
        let response = ask(&server, "GET", target);
        assert_eq!(response.status, "HTTP/1.1 200 OK", "{target}");
        assert_eq!(
            response.header("Content-Type"),
            Some(content_type),
            "{target}"
        );
        let length = body.len().to_string();
        assert_eq!(
            response.header("Content-Length"),
            Some(length.as_str()),
            "{target}"
        );
        assert_eq!(String::from_utf8_lossy(&response.body), body, "{target}");
    }
    let mut stream = server.connect();
    stream
        .write_all(b"HEAD /style.css HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n")
        .unwrap_or_else(|e| panic!("{e}"));
    let head = read_until_closed(&mut stream, Duration::from_secs(30));
    assert!(
        head.text().starts_with("HTTP/1.1 200 OK\r\n"),
        "{}",
        head.text()
    );
    assert!(
        head.text().contains("content-length: 16\r\n"),
        "{}",
        head.text()
    );
    assert!(head.text().ends_with("\r\n\r\n"), "{}", head.text());
    let put = ask(&server, "PUT", "/style.css");
    assert_eq!(put.status, "HTTP/1.1 405 Method Not Allowed");
    assert_eq!(put.header("Allow"), Some("GET, HEAD, POST"));
}

/// A `.php` file is run, under a path's info after it or as a directory's
/// index, and never sent as its source: not in any case of its
/// extension, which on a file system that folds case names the same file
/// and is refused as php-fpm's `security.limit_extensions` refuses it,
/// and not past a NUL, which `stat(2)` finds nothing at.
#[test]
fn a_php_file_is_run_and_never_served_as_its_source() {
    let root = common::root("php-source", &[("run.php", "<?php echo 'ran ', 1 + 1;")]);
    std::fs::create_dir_all(root.join("app")).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(root.join("app/index.php"), "<?php echo 'index ran';")
        .unwrap_or_else(|e| panic!("{e}"));
    let server = Server::start(&root);
    for (target, body) in [
        ("/run.php", "ran 2"),
        ("/run.php/more/info", "ran 2"),
        ("/app/", "index ran"),
        ("/app", "index ran"),
    ] {
        let response = ask(&server, "GET", target);
        assert_eq!(response.status, "HTTP/1.1 200 OK", "{target}");
        assert_eq!(String::from_utf8_lossy(&response.body), body, "{target}");
    }
    let nul = ask(&server, "GET", "/run.php%00.txt");
    assert_eq!(nul.status, "HTTP/1.1 404 Not Found");
    assert!(nul.body.is_empty(), "{:?}", nul.body);
    for target in ["/RUN.PHP", "/Run.Php", "/run.php."] {
        let response = ask(&server, "GET", target);
        let body = String::from_utf8_lossy(&response.body);
        assert!(!body.contains("<?php"), "{target} sent the source: {body}");
        assert!(
            ["HTTP/1.1 403 Forbidden", "HTTP/1.1 404 Not Found"]
                .contains(&response.status.as_str()),
            "{target}: {}",
            response.status
        );
    }
}

/// No path leaves the document root, its `..` written plainly, encoded,
/// or with an encoded slash: each is held at the root, where the file
/// beside it is not, and is a 404.
#[test]
fn dot_dot_and_encoded_traversal_never_leave_the_document_root() {
    let root = common::root("contained", &[("in.txt", "inside")]);
    let outside = Path::new(env!("CARGO_TARGET_TMPDIR")).join("makala-secret.txt");
    std::fs::write(&outside, "secret").unwrap_or_else(|e| panic!("{e}"));
    let server = Server::start(&root);
    for target in [
        "/../makala-secret.txt",
        "/%2e%2e/makala-secret.txt",
        "/%2E%2E/%2e%2e/makala-secret.txt",
        "/in.txt/..%2f..%2fmakala-secret.txt",
        "/.%2e/makala-secret.txt",
        "//../makala-secret.txt",
    ] {
        let response = ask(&server, "GET", target);
        assert_eq!(response.status, "HTTP/1.1 404 Not Found", "{target}");
        assert!(response.body.is_empty(), "{target}: {:?}", response.body);
    }
    let inside = ask(&server, "GET", "/%2e/in.txt");
    assert_eq!(String::from_utf8_lossy(&inside.body), "inside");
}
