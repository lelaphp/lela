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
    let _ = std::fs::remove_file(&*path);
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
    let _ = std::fs::remove_file(&path);
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
    let _ = std::fs::remove_file(path.to_string());
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "string(6) \"a|i:1;\"\n"
    );
}
