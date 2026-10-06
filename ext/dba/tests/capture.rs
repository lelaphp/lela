//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[php_ext_dba::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('dba'));", sink);
    assert_eq!(served.stdout, b"bool(true)\n");
}

/// A `dba_popen` link the first request leaves open is given back to the
/// engine's pool with its fd and the `flock(2)` on it, as a php-fpm
/// worker's persistent list keeps it: the next request's `dba_open` of the
/// same database is refused its `flock(2)`, and its `dba_popen` by the same key takes
/// the link up, with what the first wrote.
#[test]
fn a_persistent_link_and_its_flock_outlive_the_request() {
    let engine = Engine::builder()
        .extensions(&[php_ext_dba::extension()])
        .ini("html_errors", "0")
        .build();
    let path = std::env::temp_dir().join(format!("dba_pool_{}.db", std::process::id()));
    let path = path.to_string_lossy();
    let first = format!(
        "<?php dba_close(dba_open('{path}', 'n', 'flatfile')); $p = dba_popen('{path}', 'wd', 'flatfile'); dba_insert('k', 'v', $p); echo 'held';"
    );
    let served = engine.serve_capture(&first, Capture::default());
    assert_eq!(String::from_utf8_lossy(&served.stdout), "held");
    let second = format!(
        "<?php var_dump(dba_open('{path}', 'rdt', 'flatfile')); var_dump(dba_fetch('k', dba_popen('{path}', 'wd', 'flatfile')));"
    );
    let served = engine.serve_capture(&second, Capture::default());
    std::fs::remove_file(&*path).ok();
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "\nWarning: dba_open(): Driver initialization failed for handler: flatfile: Unable to establish lock in Standard input code on line 1\nbool(false)\nstring(1) \"v\"\n"
    );
}

/// A session save handler whose `write` goes through a `dba_popen` link
/// in a global, served in session's `RSHUTDOWN`, still has that link:
/// php gives dba's persistent links back in `shutdown_executor`, after
/// every extension's `RSHUTDOWN`. The second request, lent the same link
/// and its flock, prints what the write left, the oracle's bytes for the
/// same two requests (#1695).
#[test]
fn a_session_written_at_the_request_end_still_has_the_persistent_link() {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_standard::extension(),
            php_ext_session::extension(),
            php_ext_dba::extension(),
        ])
        .build();
    let path = std::env::temp_dir().join(format!("t1695-{}.dba", std::process::id()));
    std::fs::remove_file(&path).ok();
    let path = path.display();
    let written = format!(
        "<?php ini_set('session.use_cookies', '0'); ini_set('session.cache_limiter', '');
        $db = dba_popen('{path}', 'c', 'flatfile');
        class H implements SessionHandlerInterface {{
            function __construct(public $db) {{}}
            function open(string $p, string $n): bool {{ return true; }}
            function close(): bool {{ return true; }}
            function read(string $id): string|false {{ return ''; }}
            function write(string $id, string $data): bool {{
                global $db;
                return dba_replace('t1695', $data, $db);
            }}
            function destroy(string $id): bool {{ return true; }}
            function gc(int $m): int|false {{ return 0; }}
        }}
        session_set_save_handler(new H($db), false);
        session_id('t1695');
        session_start();
        $_SESSION['a'] = 1;"
    );
    let served = engine.serve_capture(&written, Capture::default());
    assert_eq!(String::from_utf8_lossy(&served.stdout), "");
    let read = format!("<?php var_dump(dba_fetch('t1695', dba_popen('{path}', 'c', 'flatfile')));");
    let served = engine.serve_capture(&read, Capture::default());
    std::fs::remove_file(path.to_string()).ok();
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "string(6) \"a|i:1;\"\n"
    );
}

/// What one engine's request writes of a `dba_popen` `cdb_make` file at
/// `$f`, with `then` after the insert, and what a second engine reads of
/// it: php's `zend_shutdown` finishes the file when it destroys the
/// persistent list, its hash table written past the record and its
/// bucket heads over the first 2,048 bytes, so the next process reads the
/// value (#1804).
fn written_then_read(name: &str, then: &str) -> (String, String) {
    let path = std::env::temp_dir().join(format!("t1804-{name}-{}.cdb", std::process::id()));
    std::fs::remove_file(&path).ok();
    let path = path.display();
    let built = || {
        Engine::builder()
            .extensions(&[php_ext_standard::extension(), php_ext_dba::extension()])
            .ini("html_errors", "0")
            .build()
    };
    let engine = built();
    let write = format!(
        "<?php $f = '{path}'; $d = dba_popen($f, 'n', 'cdb_make'); dba_insert('key0', 'value0', $d); {then}"
    );
    let written = engine.serve_capture(&write, Capture::default());
    drop(engine);
    let read = format!(
        "<?php clearstatcache(); echo filesize('{path}'), \"\\n\"; var_dump(dba_fetch('key0', dba_open('{path}', 'r', 'cdb')));"
    );
    let served = built().serve_capture(&read, Capture::default());
    std::fs::remove_file(path.to_string()).ok();
    (
        String::from_utf8_lossy(&written.stdout).into_owned(),
        String::from_utf8_lossy(&served.stdout).into_owned(),
    )
}

/// The oracle's bytes for the two processes: 2,066 bytes once the record
/// is written, 2,082 once the first engine has ended, and the value read.
#[test]
fn a_persistent_cdb_make_file_is_finished_at_engine_end() {
    let (written, read) = written_then_read("end", "clearstatcache(); echo filesize($f);");
    assert_eq!(written, "2066");
    assert_eq!(read, "2082\nstring(6) \"value0\"\n");
}

/// `dba_close` leaves a persistent link open, so the file is finished at
/// engine end all the same.
#[test]
fn a_persistent_cdb_make_file_closed_is_finished_at_engine_end() {
    let (written, read) = written_then_read(
        "close",
        "dba_close($d); clearstatcache(); echo filesize($f);",
    );
    assert_eq!(written, "2066");
    assert_eq!(read, "2082\nstring(6) \"value0\"\n");
}

/// A fatal ends the request, and the engine's end finishes the file: an
/// uncaught `Error`, and the memory limit's `E_ERROR`.
#[test]
fn a_persistent_cdb_make_file_is_finished_at_engine_end_after_a_fatal() {
    let (written, read) = written_then_read(
        "memory",
        "ini_set('memory_limit', '2M'); $a = str_repeat('x', 4 * 1024 * 1024);",
    );
    assert!(
        written.contains("Fatal error: Allowed memory size of 2097152 bytes exhausted"),
        "{written}"
    );
    assert_eq!(read, "2082\nstring(6) \"value0\"\n");
    let (written, read) = written_then_read("fatal", "undefined_function();");
    assert!(
        written.contains(
            "Fatal error: Uncaught Error: Call to undefined function undefined_function()"
        ),
        "{written}"
    );
    assert_eq!(read, "2082\nstring(6) \"value0\"\n");
}
