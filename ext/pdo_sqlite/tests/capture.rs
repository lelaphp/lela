//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate and `pdo`, its one dependency, never the
//! bundle.

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[php_ext_pdo::extension(), php_ext_pdo_sqlite::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture(
        "<?php var_dump(extension_loaded('pdo_sqlite'), (new PDO('sqlite::memory:'))->query('SELECT 6 * 7')->fetchColumn());",
        sink,
    );
    assert_eq!(served.stdout, b"bool(true)\nint(42)\n");
}

/// A persistent connection given back to the engine's pool at the end of
/// one request is lent to the next asking its key: the second request
/// counts the row the first wrote into its `:memory:` database.
#[test]
fn a_persistent_connection_serves_the_next_request() {
    let engine = Engine::builder()
        .extensions(&[php_ext_pdo::extension(), php_ext_pdo_sqlite::extension()])
        .build();
    let open = "$db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => true]);";
    let first = format!(
        "<?php {open} @$db->sqliteCreateFunction('f', fn() => 1, 0); $db->exec('CREATE TABLE t (a)'); $db->exec('INSERT INTO t VALUES (1)'); echo 'made';"
    );
    let served = engine.serve_capture(&first, Capture::default());
    assert_eq!(served.stdout, b"made");
    let second = "<?php $db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => true, PDO::ATTR_ERRMODE => PDO::ERRMODE_SILENT]); var_dump($db->query('SELECT COUNT(*) FROM t')->fetchColumn(), $db->query('SELECT f()')); echo $db->errorInfo()[2];";
    let served = engine.serve_capture(second, Capture::default());
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "int(1)\nbool(false)\nno such function: f"
    );
}
