//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

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
            php_ext_mysqlnd::extension(),
            php_ext_pdo::extension(),
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
