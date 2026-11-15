//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate and `pdo`, its one dependency, never the
//! bundle.

mod support;

use php_ext::test::{Capture, Engine, Files};

#[test]
fn the_extension_is_loaded() {
    let engine = support::engine();
    let sink = Capture::default();
    let served = engine.serve_capture(
        "<?php var_dump(extension_loaded('pdo_sqlite'), (new PDO('sqlite::memory:'))->query('SELECT 6 * 7')->fetchColumn());",
        sink,
    );
    assert_eq!(served.stdout_text(), "bool(true)\nint(42)\n");
}

/// A persistent connection given back to the engine's pool at the end of
/// one request is lent to the next asking its key: the second request
/// counts the row the first wrote into its `:memory:` database.
#[test]
fn a_persistent_connection_serves_the_next_request() {
    let engine = support::engine();
    let open = "$db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => true]);";
    let first = format!(
        "<?php {open} @$db->sqliteCreateFunction('f', fn() => 1, 0); $db->exec('CREATE TABLE t (a)'); $db->exec('INSERT INTO t VALUES (1)'); echo 'made';"
    );
    let served = engine.serve_capture(&first, Capture::default());
    assert_eq!(served.stdout_text(), "made");
    let second = "<?php $db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => true, PDO::ATTR_ERRMODE => PDO::ERRMODE_SILENT]); var_dump($db->query('SELECT COUNT(*) FROM t')->fetchColumn(), $db->query('SELECT f()')); echo $db->errorInfo()[2];";
    let served = engine.serve_capture(second, Capture::default());
    assert_eq!(
        served.stdout_text(),
        "int(1)\nbool(false)\nno such function: f"
    );
}

/// A session save handler whose `write` goes through a persistent
/// connection in a global, served in session's `RSHUTDOWN`, still has
/// that connection: php gives pdo's persistent connections back in
/// `shutdown_executor`, after every extension's `RSHUTDOWN`. The second
/// request prints what the write left, the oracle's bytes for the same
/// two requests.
#[test]
fn a_session_written_at_the_request_end_still_has_the_persistent_connection() {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_json::extension(),
            php_ext_spl::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
            php_ext_session::extension(),
            php_ext_pdo::extension(),
            php_ext_pdo_sqlite::extension(),
        ])
        .build();
    let files = Files::new("pdo-sqlite-session", &[]);
    let path = files.path().join("session.sqlite");
    let dsn = format!("sqlite:{}", path.display());
    let written = format!(
        "<?php ini_set('session.use_cookies', '0'); ini_set('session.cache_limiter', '');
        $db = new PDO('{dsn}', null, null, [PDO::ATTR_PERSISTENT => true]);
        $db->exec('CREATE TABLE t (v TEXT)');
        class H implements SessionHandlerInterface {{
            function __construct(public $db) {{}}
            function open(string $p, string $n): bool {{ return true; }}
            function close(): bool {{ return true; }}
            function read(string $id): string|false {{ return ''; }}
            function write(string $id, string $data): bool {{
                global $db;
                return $db->exec(\"INSERT INTO t VALUES ('$data')\") === 1;
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
    assert_eq!(served.stdout_text(), "");
    let read = format!(
        "<?php var_dump((new PDO('{dsn}'))->query('SELECT v FROM t')->fetchAll(PDO::FETCH_COLUMN));"
    );
    let served = engine.serve_capture(&read, Capture::default());
    assert_eq!(
        served.stdout_text(),
        "array(1) {\n  [0]=>\n  string(6) \"a|i:1;\"\n}\n"
    );
}

/// A persistent connection held in a static with its transaction open at
/// the end of one request is rolled back before the next is lent it:
/// php's `pdo_dbh_free_storage` rolls back what the object leaves open.
/// The second request prints the oracle's bytes for the same two
/// requests under its built-in web server, `bool(false)` and `int(0)`.
#[test]
fn a_persistent_connection_s_open_transaction_is_rolled_back_at_the_request_end() {
    let engine = support::engine();
    let open = "function db() { static $db; return $db ??= new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => true]); } $db = db();";
    let first = format!(
        "<?php {open} $db->exec('CREATE TABLE t (a)'); $db->beginTransaction(); $db->exec('INSERT INTO t VALUES (1)'); var_dump($db->inTransaction());"
    );
    let served = engine.serve_capture(&first, Capture::default());
    assert_eq!(served.stdout_text(), "bool(true)\n");
    let second = format!(
        "<?php {open} var_dump($db->inTransaction(), $db->query('SELECT COUNT(*) FROM t')->fetchColumn());"
    );
    let served = engine.serve_capture(&second, Capture::default());
    assert_eq!(served.stdout_text(), "bool(false)\nint(0)\n");
}
