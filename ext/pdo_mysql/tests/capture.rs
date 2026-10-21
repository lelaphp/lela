//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate and what a test needs beside it, never
//! the bundle. An engine that carries pdo carries spl, which php always
//! has and pdo requires: without it `PDOException` has no
//! `RuntimeException` to extend, and a connect failure writes `message`,
//! `file`, `line` and `trace` as dynamic properties, four deprecations
//! before anything the request prints (#1867).

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

#[path = "../../mysqlnd/tests/support/mysql.rs"]
mod mysql;

use mysql::MysqlServer;
use php_ext::abi::{Dependency, DependencyKind, ExtensionBuilder};
use php_ext::request::Request;
use php_ext::test::{Capture, Engine};
use php_ext::values::{PhpArray, Value};
use php_ext_pdo::connector::{Connection, Connector, Failure, Prepared, Source};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_json::extension(),
            php_ext_standard::extension(),
            php_ext_spl::extension(),
            php_ext_mysqlnd::extension(),
            php_ext_pdo::extension(),
            php_ext_uri::extension(),
            php_ext_pdo_mysql::extension(),
        ])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('pdo_mysql'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

/// A persistent connection given back to the engine's pool at the end of
/// one request is taken up by the next asking its key, the session's
/// variable still set on it; once the server has killed it, the ping the
/// next request sends finds it dead and a new connection replaces it.
/// Skipped where no server answers on `ci/mysql/compose.yaml`'s address.
#[test]
fn a_persistent_connection_serves_the_next_request_and_a_dead_one_is_replaced() {
    MysqlServer::with(|_| {
        let engine = Engine::builder()
            .extensions(&[
                php_ext_lexbor::extension(),
                php_ext_json::extension(),
                php_ext_standard::extension(),
                php_ext_uri::extension(),
                php_ext_spl::extension(),
                php_ext_pdo::extension(),
                php_ext_mysqlnd::extension(),
                php_ext_pdo_mysql::extension(),
            ])
            .build();
        let open = "<?php $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_PERSISTENT => true]);";
        let first = format!(
            "{open} $db->exec('SET @kept = 7'); echo $db->query('SELECT CONNECTION_ID()')->fetchColumn();"
        );
        let served = engine.serve_capture(&first, Capture::default());
        let id = served.stdout_text();
        assert!(id.parse::<u64>().is_ok(), "{id}");
        let second = format!(
            "{open} echo $db->query('SELECT CONNECTION_ID()')->fetchColumn(), ' ', $db->query('SELECT @kept')->fetchColumn();"
        );
        let served = engine.serve_capture(&second, Capture::default());
        assert_eq!(served.stdout_text(), format!("{id} 7"));
        let kill = format!(
            "<?php $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', ''); $db->exec('KILL {id}'); $db->query('SELECT SLEEP(0.2)'); echo 'killed';"
        );
        let served = engine.serve_capture(&kill, Capture::default());
        assert_eq!(served.stdout_text(), "killed");
        let third = format!(
            "{open} $now = $db->query('SELECT CONNECTION_ID()')->fetchColumn(); var_dump($now != '{id}', $db->query('SELECT @kept')->fetchColumn());"
        );
        let served = engine.serve_capture(&third, Capture::default());
        assert_eq!(served.stdout_text(), "bool(true)\nNULL\n");
    });
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
    MysqlServer::with(|_| {
        let engine = Engine::builder()
            .extensions(&[
                php_ext_lexbor::extension(),
                php_ext_json::extension(),
                php_ext_uri::extension(),
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
        assert_eq!(served.stdout_text(), "");
        let read = "<?php $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
            var_dump($db->query('SELECT v FROM t1695')->fetchAll(PDO::FETCH_COLUMN));
            $db->exec('DROP TABLE t1695');";
        let served = engine.serve_capture(read, Capture::default());
        assert_eq!(
            served.stdout_text(),
            "array(1) {\n  [0]=>\n  string(6) \"a|i:1;\"\n}\n"
        );
    });
}

/// A connector of the test's own, `filler:`, whose persistent connections
/// fill PDO's pool without a server.
struct Filler;

static FILLER: Filler = Filler;

impl Connector for Filler {
    fn name(&self) -> &'static str {
        "filler"
    }

    fn open(&self, req: &mut Request, source: &Source<'_>) -> Result<Box<dyn Connection>, Failure> {
        let _ = (req, source);
        Ok(Box::new(Filled))
    }
}

/// One connection of [`Filler`].
struct Filled;

impl Connection for Filled {
    fn prepare(
        &mut self,
        req: &mut Request,
        sql: &[u8],
        options: Option<PhpArray>,
    ) -> Result<Prepared, Failure> {
        let _ = (req, sql, options);
        Err(Failure::unsupported())
    }

    fn exec(&mut self, req: &mut Request, sql: &[u8]) -> Result<i64, Failure> {
        let _ = (req, sql);
        Ok(0)
    }

    /// Every attribute taken, `PDO::ATTR_PERSISTENT` among them.
    fn set_attribute(
        &mut self,
        req: &mut Request,
        attr: i64,
        value: &Value,
    ) -> Result<(), Failure> {
        let _ = (req, attr, value);
        Ok(())
    }
}

/// A persistent connection past the bound of PDO's pool is closed with the
/// request that gives it back, not dropped with its socket still open
/// (ADR-0010's amendment of ADR-0005, #1798): 1024 persistent connections
/// of a connector of the test's own fill the pool first, so the one the
/// request opened last, to the server, is the one past the bound, and the
/// server's process list, read in the next request, no longer lists its
/// thread. Against the server of `ci/mysql/compose.yaml`; with none
/// listening the test says so and passes.
#[test]
fn a_persistent_connection_past_the_pool_s_bound_is_closed() {
    MysqlServer::with(|_| {
        let filler = ExtensionBuilder::default()
            .dependencies(vec![Dependency {
                name: String::from("pdo"),
                kind: DependencyKind::Required,
                rel: None,
                version: None,
            }])
            .register::<&'static dyn Connector>(&FILLER)
            .build("pdo_filler");
        let engine = Engine::builder()
            .extensions(&[
                php_ext_lexbor::extension(),
                php_ext_json::extension(),
                php_ext_standard::extension(),
                php_ext_uri::extension(),
                php_ext_spl::extension(),
                php_ext_pdo::extension(),
                filler,
                php_ext_mysqlnd::extension(),
                php_ext_pdo_mysql::extension(),
            ])
            .build();
        let first = "<?php for ($i = 0; $i < 1024; $i++) {
                new PDO('filler:', null, null, [PDO::ATTR_PERSISTENT => \"n$i\"]);
            }
            $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_PERSISTENT => true]);
            echo $db->query('SELECT CONNECTION_ID()')->fetchColumn();";
        let served = engine.serve_capture(first, Capture::default());
        let thread = served.stdout_text();
        assert!(thread.parse::<u64>().is_ok(), "{thread}");
        let second = format!(
            "<?php $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
            for ($i = 0; $i < 50; $i++) {{
                $open = $db->query('SELECT COUNT(*) FROM information_schema.processlist WHERE id = {thread}')->fetchColumn() != 0;
                if (!$open) break;
                $db->exec('DO SLEEP(0.02)');
            }}
            echo $open ? 'open' : 'gone';"
        );
        let served = engine.serve_capture(&second, Capture::default());
        assert_eq!(served.stdout_text(), "gone");
    });
}

/// A persistent connection waits between requests in one list, PDO's,
/// with its socket, as php's `pdo_dbh_t` in `EG(persistent_list)` holds the
/// `MYSQLND` and mysqlnd deletes its stream from that list (#1805): the
/// first request fills the engine's pool of persistent streams to its
/// bound of 1024 with `pfsockopen` sockets before it connects, and the next
/// request still takes up the same server thread, where a connection whose
/// socket waited in that pool lost it past the bound and connected again.
/// Against the server of `ci/mysql/compose.yaml`; with none listening the
/// test says so and passes.
#[test]
fn a_persistent_connection_keeps_its_socket_however_full_the_stream_pool_is() {
    MysqlServer::with(|_| {
        let engine = Engine::builder()
            .extensions(&[
                php_ext_lexbor::extension(),
                php_ext_json::extension(),
                php_ext_uri::extension(),
                php_ext_standard::extension(),
                php_ext_spl::extension(),
                php_ext_pdo::extension(),
                php_ext_mysqlnd::extension(),
                php_ext_pdo_mysql::extension(),
            ])
            .build();
        let open = "$db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_PERSISTENT => true]);
            echo $db->query('SELECT CONNECTION_ID()')->fetchColumn();";
        let first = format!(
            "<?php $s = stream_socket_server('tcp://127.0.0.1:0');
            $port = explode(':', stream_socket_get_name($s, false))[1];
            for ($i = 0; $i < 1024; $i++) {{
                $kept[] = pfsockopen(\"127.0.0.1:$port/$i\", -1);
                $accepted[] = stream_socket_accept($s, 1);
            }}
            {open}"
        );
        let served = engine.serve_capture(&first, Capture::default());
        let thread = served.stdout_text();
        assert!(thread.parse::<u64>().is_ok(), "{thread}");
        let served = engine.serve_capture(&format!("<?php {open}"), Capture::default());
        assert_eq!(served.stdout_text(), thread);
    });
}

/// A persistent connection waiting in PDO's pool as the engine ends is
/// closed on the shutdown request through `mysql_handle_closer`, which
/// sends the server its `COM_QUIT`, as php's `zend_shutdown` destroys
/// `EG(persistent_list)` and `php_pdo_pdbh_dtor` reaches `dbh_free`
/// (#1961), where a dropped socket sends the server nothing. The
/// connection reaches the server through a relay of the test's own, which
/// reads each packet the server is sent: none after the connect while the
/// engine lives, the `COM_QUIT` last once its last clone is dropped.
/// Skipped where no server answers on `ci/mysql/compose.yaml`'s address.
#[test]
fn a_pooled_persistent_connection_is_quit_as_the_engine_ends() {
    use std::io::{Read as _, Write as _};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::time::Duration;

    MysqlServer::with(|server| {
        let at = server.at();
        let relay = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = relay.local_addr().unwrap().port();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut client, _) = relay.accept().unwrap();
            let mut upstream = TcpStream::connect(at).unwrap();
            let (mut from, mut to) = (upstream.try_clone().unwrap(), client.try_clone().unwrap());
            std::thread::spawn(move || std::io::copy(&mut from, &mut to));
            let mut seen = Vec::new();
            let mut chunk = [0; 4096];
            while let Ok(n @ 1..) = client.read(&mut chunk) {
                seen.extend_from_slice(&chunk[..n]);
                upstream.write_all(&chunk[..n]).ok();
            }
            upstream.shutdown(Shutdown::Write).ok();
            sent.send(seen).ok();
        });
        let engine = Engine::builder()
            .extensions(&[
                php_ext_lexbor::extension(),
                php_ext_json::extension(),
                php_ext_standard::extension(),
                php_ext_uri::extension(),
                php_ext_spl::extension(),
                php_ext_pdo::extension(),
                php_ext_mysqlnd::extension(),
                php_ext_pdo_mysql::extension(),
            ])
            .build();
        let script = format!(
            "<?php new PDO('mysql:host=127.0.0.1;port={port};dbname=test', 'root', '', [PDO::ATTR_PERSISTENT => true]); echo 'connected';"
        );
        let served = engine.serve_capture(&script, Capture::default());
        assert_eq!(served.stdout_text(), "connected");
        assert!(
            received.recv_timeout(Duration::from_millis(200)).is_err(),
            "the pooled connection was closed before the engine ended"
        );
        drop(engine);
        let seen = received.recv_timeout(Duration::from_secs(10)).unwrap();
        let mut last = &[][..];
        let mut rest = &seen[..];
        while let [a, b, c, _, ..] = *rest {
            let len = usize::from(a) | usize::from(b) << 8 | usize::from(c) << 16;
            last = &rest[..(4 + len).min(rest.len())];
            rest = &rest[last.len()..];
        }
        assert_eq!(
            last,
            [1, 0, 0, 0, 1],
            "the last packet the server was sent is no COM_QUIT"
        );
    });
}
