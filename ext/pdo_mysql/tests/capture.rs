//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate and what a test needs beside it, never
//! the bundle. An engine that carries pdo carries spl, which php always
//! has and pdo requires: without it `PDOException` has no
//! `RuntimeException` to extend, and a connect failure writes `message`,
//! `file`, `line` and `trace` as dynamic properties, four deprecations
//! before anything the request prints (#1867).

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[php_ext_pdo_mysql::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('pdo_mysql'));", sink);
    assert_eq!(served.stdout, b"bool(true)\n");
}

/// A persistent connection given back to the engine's pool at the end of
/// one request is taken up by the next asking its key, the session's
/// variable still set on it; once the server has killed it, the ping the
/// next request sends finds it dead and a new connection replaces it.
/// Skipped where no server answers on `ci/mysql/compose.yaml`'s address.
#[test]
fn a_persistent_connection_serves_the_next_request_and_a_dead_one_is_replaced() {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_spl::extension(),
            php_ext_pdo::extension(),
            php_ext_mysqlnd::extension(),
            php_ext_pdo_mysql::extension(),
        ])
        .build();
    let open = "<?php try { $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_PERSISTENT => true]); } catch (PDOException $e) { exit('no server'); }";
    let first = format!(
        "{open} $db->exec('SET @kept = 7'); echo $db->query('SELECT CONNECTION_ID()')->fetchColumn();"
    );
    let served = engine.serve_capture(&first, Capture::default());
    let id = String::from_utf8_lossy(&served.stdout).into_owned();
    if id == "no server" {
        return;
    }
    let second = format!(
        "{open} echo $db->query('SELECT CONNECTION_ID()')->fetchColumn(), ' ', $db->query('SELECT @kept')->fetchColumn();"
    );
    let served = engine.serve_capture(&second, Capture::default());
    assert_eq!(String::from_utf8_lossy(&served.stdout), format!("{id} 7"));
    let kill = format!(
        "<?php $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', ''); $db->exec('KILL {id}'); $db->query('SELECT SLEEP(0.2)'); echo 'killed';"
    );
    let served = engine.serve_capture(&kill, Capture::default());
    assert_eq!(served.stdout, b"killed");
    let third = format!(
        "{open} $now = $db->query('SELECT CONNECTION_ID()')->fetchColumn(); var_dump($now != '{id}', $db->query('SELECT @kept')->fetchColumn());"
    );
    let served = engine.serve_capture(&third, Capture::default());
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "bool(true)\nNULL\n"
    );
}

/// A session save handler whose `write` goes through a persistent
/// connection in a global, served in session's `RSHUTDOWN`, still has
/// that connection: php gives pdo's persistent connections back in
/// `shutdown_executor`, after every extension's `RSHUTDOWN`. The second
/// request prints what the write left, the oracle's bytes for the same
/// two requests (#1695). Against the server of `ci/mysql/compose.yaml`;
/// with none listening the test says so and passes.
#[test]
fn a_session_written_at_the_request_end_still_has_the_persistent_connection() {
    if std::net::TcpStream::connect("127.0.0.1:3306").is_err() {
        eprintln!("no MySQL server on 127.0.0.1:3306; ci/mysql/compose.yaml starts one");
        return;
    }
    let engine = Engine::builder()
        .extensions(&[
            php_ext_standard::extension(),
            php_ext_spl::extension(),
            php_ext_session::extension(),
            php_ext_pdo::extension(),
            php_ext_mysqlnd::extension(),
            php_ext_pdo_mysql::extension(),
        ])
        .build();
    let written = "<?php ini_set('session.use_cookies', '0'); ini_set('session.cache_limiter', '');
        $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_PERSISTENT => true]);
        $db->exec('CREATE TABLE IF NOT EXISTS t1695 (v TEXT)');
        $db->exec('DELETE FROM t1695');
        class H implements SessionHandlerInterface {
            function __construct(public $db) {}
            function open(string $p, string $n): bool { return true; }
            function close(): bool { return true; }
            function read(string $id): string|false { return ''; }
            function write(string $id, string $data): bool {
                global $db;
                return $db->exec(\"INSERT INTO t1695 VALUES ('$data')\") === 1;
            }
            function destroy(string $id): bool { return true; }
            function gc(int $m): int|false { return 0; }
        }
        session_set_save_handler(new H($db), false);
        session_id('t1695');
        session_start();
        $_SESSION['a'] = 1;";
    let served = engine.serve_capture(written, Capture::default());
    assert_eq!(String::from_utf8_lossy(&served.stdout), "");
    let read = "<?php $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
        var_dump($db->query('SELECT v FROM t1695')->fetchAll(PDO::FETCH_COLUMN));
        $db->exec('DROP TABLE t1695');";
    let served = engine.serve_capture(read, Capture::default());
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "array(1) {\n  [0]=>\n  string(6) \"a|i:1;\"\n}\n"
    );
}
