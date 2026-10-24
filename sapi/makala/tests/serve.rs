//! The binary over a real socket (#2339): started on a port the system
//! picks, sent HTTP/1.1 requests as bytes, and held to what a client
//! reads back, the status, the header lines and the body, and to what
//! the PHP program saw.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use common::{Request, Server, read_response, root, unchunked};

/// A `GET` is PHP's response over HTTP/1.1: the code and the header lines
/// the program set, php's own `X-Powered-By` and `Content-type`, the
/// server's `Date`, and the body the program wrote, chunked, since it
/// set no length; the program served under the host's name, with its
/// query string and `$_SERVER` as the request described them, with
/// neither `pcntl` nor `posix` loaded, and `PHP_ZTS` and
/// `ZEND_THREAD_SAFE` false, as #2335 ruled.
#[test]
fn a_get_is_answered_with_php_s_status_headers_and_body() {
    let root = root(
        "get",
        &[(
            "index.php",
            "<?php http_response_code(201); header('X-Test: yes'); \
             echo PHP_SAPI, ' ', $_GET['x'], ' ', $_SERVER['SERVER_SOFTWARE'], ' ', \
             $_SERVER['REQUEST_METHOD'], ' ', $_SERVER['SCRIPT_NAME'], ' ', \
             var_export(extension_loaded('pcntl') || extension_loaded('posix'), true), ' ', \
             var_export([PHP_ZTS, ZEND_THREAD_SAFE, php_sapi_name()], true);",
        )],
    );
    let server = Server::start(&root);
    let mut stream = server.send(&Request::get("/?x=1").host("localhost").close());
    let response = read_response(&mut stream);
    assert_eq!(response.status, "HTTP/1.1 201 Created");
    assert_eq!(response.header("X-Powered-By"), Some("PHP/8.5.12"));
    assert_eq!(response.header("X-Test"), Some("yes"));
    assert_eq!(
        response.header("Content-type"),
        Some("text/html; charset=UTF-8")
    );
    assert!(response.header("Date").is_some(), "{response:?}");
    assert_eq!(
        String::from_utf8_lossy(&response.body),
        "makala 1 makala/0.0.0 GET /index.php false array (\n  0 => false,\n  1 => false,\n  2 => 'makala',\n)"
    );
}

/// A path that names nothing under the root is a 404: a `.php` one is
/// php-fpm's `File not found.`, as one that climbs out of the root to a
/// file beside it is, and any other has no body; a file that is not PHP
/// is served as itself, never run.
#[test]
fn a_path_with_no_php_file_is_not_served_as_a_program() {
    let root = root("missing", &[("notes.txt", "<?php echo 'ran';")]);
    let outside = Path::new(env!("CARGO_TARGET_TMPDIR")).join("makala-outside.php");
    std::fs::write(&outside, "<?php echo 'escaped';").unwrap_or_else(|e| panic!("{e}"));
    let server = Server::start(&root);
    for (path, status, body) in [
        (
            "/nothing.php",
            "HTTP/1.1 404 Not Found",
            "File not found.\n",
        ),
        ("/nothing", "HTTP/1.1 404 Not Found", ""),
        ("/notes.txt", "HTTP/1.1 200 OK", "<?php echo 'ran';"),
        (
            "/../makala-outside.php",
            "HTTP/1.1 404 Not Found",
            "File not found.\n",
        ),
    ] {
        let response = common::parse_response(&get(&server, path));
        assert_eq!(response.status, status, "{path}");
        assert_eq!(String::from_utf8_lossy(&response.body), body, "{path}");
    }
}

/// The path past the program is its `PATH_INFO`, split at the first
/// segment that ends in `.php` as the recipe's nginx splits it (#2637,
/// #2693): `SCRIPT_NAME` and `SCRIPT_FILENAME` the program's, `PHP_SELF`
/// the two, an empty `PATH_INFO` where nothing is past it, and
/// `PATH_TRANSLATED` the root and `PATH_INFO`, none where that is empty
/// (RFC 3875 §4.1.6, #2650). What is split meets php-fpm's own checks,
/// and is answered as the pinned php-fpm answers it under the recipe: a
/// program named past a file that is not one is refused and runs
/// nothing, a directory named `*.php` is no program's and its index never
/// runs, and a name that ends in `.PHP` is not split and is refused.
#[test]
fn the_path_past_the_program_is_its_path_info() {
    let root = root(
        "path-info",
        &[
            (
                "front.php",
                "<?php foreach (['SCRIPT_NAME', 'PATH_INFO', 'PHP_SELF', 'SCRIPT_FILENAME', 'PATH_TRANSLATED'] as $n) \
                 echo $n, '=', $_SERVER[$n] ?? '(none)', \"\\n\";",
            ),
            ("x.PHP", "<?php echo 'ran';"),
        ],
    );
    for dir in ["upload", "d.php"] {
        std::fs::create_dir_all(root.join(dir)).unwrap_or_else(|e| panic!("{e}"));
    }
    for file in ["upload/x.jpg", "d.php/index.php"] {
        std::fs::write(root.join(file), "<?php echo 'ran';").unwrap_or_else(|e| panic!("{e}"));
    }
    let root = root.canonicalize().unwrap_or_else(|e| panic!("{e}"));
    let filename = root.join("front.php");
    let (root_shown, filename) = (root.display(), filename.display());
    let server = Server::start(&root);
    let answer = |code, body: &str| (code, body.to_owned());
    for (target, (status, body)) in [
        (
            "/front.php/index.php?x=1",
            answer(
                "HTTP/1.1 200 OK",
                &format!(
                    "SCRIPT_NAME=/front.php\nPATH_INFO=/index.php\nPHP_SELF=/front.php/index.php\nSCRIPT_FILENAME={filename}\nPATH_TRANSLATED={root_shown}/index.php\n"
                ),
            ),
        ),
        (
            "/front.php",
            answer(
                "HTTP/1.1 200 OK",
                &format!(
                    "SCRIPT_NAME=/front.php\nPATH_INFO=\nPHP_SELF=/front.php\nSCRIPT_FILENAME={filename}\nPATH_TRANSLATED=(none)\n"
                ),
            ),
        ),
        (
            "/upload/x.jpg/y.php",
            answer("HTTP/1.1 403 Forbidden", "Access denied.\n"),
        ),
        ("/upload/x.jpg/", answer("HTTP/1.1 404 Not Found", "")),
        (
            "/d.php/index.php",
            answer("HTTP/1.1 404 Not Found", "No input file specified.\n"),
        ),
        (
            "/d.php/",
            answer("HTTP/1.1 404 Not Found", "No input file specified.\n"),
        ),
        (
            "/d.php",
            answer("HTTP/1.1 404 Not Found", "No input file specified.\n"),
        ),
        (
            "/x.PHP/a",
            answer("HTTP/1.1 403 Forbidden", "Access denied.\n"),
        ),
        (
            "/x.PHP",
            answer("HTTP/1.1 403 Forbidden", "Access denied.\n"),
        ),
    ] {
        let response = common::parse_response(&get(&server, target));
        assert_eq!(response.status, status, "{target}");
        assert_eq!(String::from_utf8_lossy(&response.body), body, "{target}");
    }
}

/// A body reaches `php://input` through the stream while the program
/// runs (#2338): the program answers before it reads the input, and the
/// client sends the second half of the body only once it has that answer,
/// so a server that read the body before running the program would wait
/// on the client forever, which the read's timeout fails. The request is
/// a `PUT`, since php's default reader swallows a `POST`'s body of any
/// type at request startup (`php_default_post_reader`,
/// `Z/main/php_content_types.c:35`).
#[test]
fn a_post_body_reaches_php_input_through_the_stream() {
    let root = root(
        "post",
        &[(
            "post.php",
            "<?php echo \"ready\\n\"; flush(); $in = file_get_contents('php://input'); \
             echo strlen($in), ' ', $in === str_repeat('y', 40000) ? 'same' : 'differs', ' ', count($_POST);",
        )],
    );
    let server = Server::start(&root);
    let body = vec![b'y'; 40_000];
    let mut stream = server.send(
        &Request::new("PUT", "/post.php")
            .host("localhost")
            .close()
            .field("Content-Type", "application/octet-stream")
            .field("Content-Length", &body.len().to_string()),
    );
    let (first, rest) = body.split_at(10_000);
    stream.write_all(first).unwrap_or_else(|e| panic!("{e}"));
    let mut seen = Vec::new();
    let mut buf = [0u8; 1024];
    while !seen.windows(6).any(|w| w == b"ready\n") {
        let n = stream.read(&mut buf).unwrap_or_else(|e| panic!("{e}"));
        assert!(n > 0, "closed before the program answered");
        seen.extend_from_slice(&buf[..n]);
    }
    stream.write_all(rest).unwrap_or_else(|e| panic!("{e}"));
    stream
        .read_to_end(&mut seen)
        .unwrap_or_else(|e| panic!("{e}"));
    let mut whole = seen.as_slice();
    let response = {
        let end = whole
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or_else(|| panic!("no head"));
        let chunked = String::from_utf8_lossy(&whole[..end])
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked");
        whole = &whole[end + 4..];
        if chunked {
            unchunked(whole)
        } else {
            whole.to_vec()
        }
    };
    assert_eq!(String::from_utf8_lossy(&response), "ready\n40000 same 0");
}

/// A form `POST` that arrives in two writes with a pause between, past
/// `SAPI_POST_BLOCK_SIZE`, is `$_POST` and `php://input` whole, read
/// through the stream at request startup as php's form reader reads it.
#[test]
fn a_form_post_is_post_and_input_whole() {
    let root = root(
        "form",
        &[(
            "form.php",
            "<?php $in = file_get_contents('php://input'); \
             echo strlen($in), ' ', $_POST['a'], ' ', strlen($_POST['big']), ' ', \
             $in === 'a=1&big=' . str_repeat('y', 40000) ? 'same' : 'differs';",
        )],
    );
    let server = Server::start(&root);
    let mut body = b"a=1&big=".to_vec();
    body.resize(body.len() + 40_000, b'y');
    let mut stream = server.send(
        &Request::post("/form.php")
            .host("localhost")
            .close()
            .field("Content-Type", "application/x-www-form-urlencoded")
            .field("Content-Length", &body.len().to_string()),
    );
    let (first, rest) = body.split_at(10_000);
    stream.write_all(first).unwrap_or_else(|e| panic!("{e}"));
    std::thread::sleep(Duration::from_millis(200));
    stream.write_all(rest).unwrap_or_else(|e| panic!("{e}"));
    let response = read_response(&mut stream);
    assert_eq!(response.status, "HTTP/1.1 200 OK");
    assert_eq!(
        String::from_utf8_lossy(&response.body),
        "40008 1 40000 same"
    );
}

/// A relative `upload_tmp_dir` puts an upload under the process's working
/// directory, as php-fpm's rfc1867 resolves it at request startup in its
/// pool's (#2610), and not under the program's, which the program itself
/// is served in.
#[test]
fn a_relative_upload_tmp_dir_is_under_the_process_s_working_directory() {
    let root = root(
        "upload",
        &[(
            "up.php",
            "<?php echo dirname($_FILES['f']['tmp_name']), ' ', $_FILES['f']['error'], ' ', getcwd() === __DIR__ ? 'in its own' : 'elsewhere';",
        )],
    );
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("makala-upload-cwd");
    std::fs::create_dir_all(dir.join("spool")).unwrap_or_else(|e| panic!("{e}"));
    let dir = dir.canonicalize().unwrap_or_else(|e| panic!("{e}"));
    let server = Server::in_dir(
        &root,
        &dir,
        &["--threads", "2", "-d", "upload_tmp_dir=spool"],
    );
    let body = "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a.txt\"\r\n\
                Content-Type: text/plain\r\n\r\nhello\r\n--b--\r\n";
    let mut stream = server.send(
        &Request::post("/up.php")
            .host("localhost")
            .close()
            .field("Content-Type", "multipart/form-data; boundary=b")
            .body(body),
    );
    let response = read_response(&mut stream);
    assert_eq!(
        String::from_utf8_lossy(&response.body),
        format!("{}/spool 0 in its own", dir.display())
    );
}

/// `connection_aborted()` turns true on the write after the client has
/// gone, and not before (#2337, user story 23): the program writes once,
/// waits until the client has read that and closed, reads
/// `connection_aborted()` with no write since, and then writes until a
/// write finds the client gone, under `ignore_user_abort(true)` so it
/// runs on to say so.
#[test]
fn connection_aborted_turns_true_on_the_write_after_the_client_closes() {
    let root = root(
        "aborted",
        &[(
            "abort.php",
            "<?php ignore_user_abort(true); echo 'first'; flush(); \
             for ($i = 0; $i < 500 && !file_exists(__DIR__ . '/closed'); $i++) usleep(10000); \
             $before = connection_aborted(); \
             for ($w = 0; $w < 500 && !connection_aborted(); $w++) { echo str_repeat('x', 65536); flush(); usleep(1000); } \
             file_put_contents(__DIR__ . '/log', $before . ' ' . connection_aborted() . ' ' . ($w < 500 ? 'early' : 'late'));",
        )],
    );
    let server = Server::start(&root);
    let mut stream = server.send(&Request::get("/abort.php").host("localhost"));
    let mut seen = Vec::new();
    let mut buf = [0u8; 1024];
    while !seen.windows(5).any(|w| w == b"first") {
        let n = stream.read(&mut buf).unwrap_or_else(|e| panic!("{e}"));
        assert!(n > 0, "closed before the first write");
        seen.extend_from_slice(&buf[..n]);
    }
    drop(stream);
    std::fs::write(root.join("closed"), "").unwrap_or_else(|e| panic!("{e}"));
    let log = root.join("log");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !log.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let logged = std::fs::read_to_string(&log).unwrap_or_else(|e| panic!("the log: {e}"));
    assert_eq!(logged, "0 1 early");
    drop(server);
}

/// The header fields reach `$_SERVER` as nginx passes them to php-fpm
/// (#2347): a name with `_` in it left out, as nginx's
/// `underscores_in_headers off` ignores it, so `X_Forwarded_For` cannot
/// pose as `X-Forwarded-For`; a name sent twice joined with `, `, a
/// cookie's with `; `; and the fields first, newest first, as php-fpm
/// registers them.
#[test]
fn header_fields_reach_server_as_nginx_passes_them() {
    let root = root(
        "fields",
        &[(
            "fields.php",
            "<?php foreach ($_SERVER as $k => $v) if (str_starts_with($k, 'HTTP_')) echo \"$k=$v\\n\";",
        )],
    );
    let server = Server::start(&root);
    let mut stream = server.send(
        &Request::get("/fields.php")
            .field("X-Forwarded-For", "10.0.0.1")
            .field("X_Forwarded_For", "6.6.6.6")
            .field("Cookie", "a=1")
            .field("X-Two", "1")
            .field("Cookie", "b=2")
            .field("X-Two", "2")
            .close(),
    );
    let response = read_response(&mut stream);
    assert_eq!(
        String::from_utf8_lossy(&response.body),
        "HTTP_CONNECTION=close\nHTTP_X_TWO=1, 2\nHTTP_COOKIE=a=1; b=2\n\
         HTTP_X_FORWARDED_FOR=10.0.0.1\nHTTP_HOST=a\n"
    );
}

/// Forwarded fields are trusted from no proxy (research/production-server.r2.md
/// §6): a client's `X-Forwarded-For` and `Forwarded` reach `$_SERVER` as
/// fields alone, and `REMOTE_ADDR` stays the peer the socket saw.
#[test]
fn forwarded_fields_are_trusted_from_no_proxy() {
    let root = root(
        "forwarded",
        &[(
            "peer.php",
            "<?php echo $_SERVER['REMOTE_ADDR'], ' ', $_SERVER['HTTP_X_FORWARDED_FOR'], ' ', \
             $_SERVER['HTTP_FORWARDED'], \"\\n\";",
        )],
    );
    let server = Server::start(&root);
    let mut stream = server.send(
        &Request::get("/peer.php")
            .field("X-Forwarded-For", "10.9.9.9")
            .field("Forwarded", "for=10.8.8.8")
            .close(),
    );
    let response = read_response(&mut stream);
    assert_eq!(
        String::from_utf8_lossy(&response.body),
        "127.0.0.1 10.9.9.9 for=10.8.8.8\n"
    );
}

/// What one `GET` of `path` on a connection of its own reads back, whole.
fn get(server: &Server, path: &str) -> Vec<u8> {
    let request = Request::get(path).host("localhost").close();
    server
        .exchange(&request.bytes(), Duration::from_secs(30))
        .bytes
}

/// The status line carries the reason phrase PHP set, as php-fpm hands it
/// on in `Status:` and the web server in front of it writes it (#2636):
/// phar's `403 Access Denied`, a code hyper has no phrase for with one,
/// and with none the code and a space, never hyper's `<none>`. A phrase
/// set on a 200 is no status php-fpm writes, so the line is `200 OK`.
#[test]
fn the_status_line_carries_the_reason_php_set() {
    let root = root(
        "reason",
        &[
            ("denied.php", "<?php header('HTTP/1.1 403 Access Denied');"),
            (
                "adventure.php",
                "<?php header('HTTP/1.1 399 Choose Your Own Adventure');",
            ),
            ("bare.php", "<?php http_response_code(399);"),
            ("fine.php", "<?php header('HTTP/1.1 200 Fine');"),
            ("plain.php", "<?php http_response_code(404);"),
        ],
    );
    let server = Server::start(&root);
    for (path, status) in [
        ("/denied.php", "HTTP/1.1 403 Access Denied"),
        ("/adventure.php", "HTTP/1.1 399 Choose Your Own Adventure"),
        ("/bare.php", "HTTP/1.1 399 "),
        ("/fine.php", "HTTP/1.1 200 OK"),
        ("/plain.php", "HTTP/1.1 404 Not Found"),
    ] {
        let bytes = get(&server, path);
        let line = bytes.split(|b| *b == b'\n').next().unwrap_or_default();
        assert_eq!(
            String::from_utf8_lossy(line),
            format!("{status}\r"),
            "{path}"
        );
    }
}

/// A 304 the program sets carries no `content-type`, as php-fpm's
/// `sapi_cgi_send_headers` drops PHP's under a 304 (#2608), and keeps the
/// rest of PHP's lines.
#[test]
fn a_304_carries_no_content_type() {
    let root = root(
        "not-modified",
        &[(
            "index.php",
            "<?php header('HTTP/1.1 304 Not Modified'); header('Location: /elsewhere');",
        )],
    );
    let server = Server::start(&root);
    let response = common::parse_response(&get(&server, "/index.php"));
    assert_eq!(response.status, "HTTP/1.1 304 Not Modified");
    assert_eq!(response.header("content-type"), None, "{response:?}");
    assert_eq!(response.header("location"), Some("/elsewhere"));
    assert_eq!(response.header("x-powered-by"), Some("PHP/8.5.12"));
}

/// A response's header names go out lowercase, in the order PHP set them,
/// with PHP's values (`trace:makala-lowercase-header-names`, #2600): the
/// head read as the bytes on the wire, where `Response::header` folds a
/// name's case.
#[test]
fn a_response_s_header_names_go_out_lowercase_in_php_s_order() {
    let root = root(
        "lowercase",
        &[("index.php", "<?php header('X-Test: yes'); echo 'body';")],
    );
    let server = Server::start(&root);
    let bytes = get(&server, "/index.php");
    let end = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no head in {:?}", String::from_utf8_lossy(&bytes)));
    let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
    let php: Vec<&str> = head
        .split("\r\n")
        .skip(1)
        .filter(|line| {
            let name = line.split(':').next().unwrap_or_default();
            ["x-powered-by", "x-test", "content-type"].contains(&name.to_ascii_lowercase().as_str())
        })
        .collect();
    assert_eq!(
        php,
        [
            "x-powered-by: PHP/8.5.12",
            "x-test: yes",
            "content-type: text/html; charset=UTF-8"
        ],
        "{head}"
    );
}

/// `include_path`'s default is the build's `PHP_INCLUDE_PATH`, the CLI's
/// too: `.:` and the PEAR directory the build was handed, where it was
/// handed one (`php_engine`'s `build_defs/layout.rs`, #1371), and never
/// another installed php's (#2704). The witness on makala's `engine`
/// holds it to `DEFAULT_INCLUDE_PATH` beside the pinned php-fpm.
#[test]
fn include_path_defaults_to_the_build_s() {
    let root = root(
        "include-path",
        &[("index.php", "<?php echo get_include_path();")],
    );
    let server = Server::start(&root);
    let response = common::parse_response(&get(&server, "/index.php"));
    let build = option_env!("PHP_INCLUDE_PATH").map_or_else(
        || format!(".:{}", option_env!("PEAR_INSTALL_DIR").unwrap_or("")),
        str::to_owned,
    );
    assert_eq!(String::from_utf8_lossy(&response.body), build);
}
