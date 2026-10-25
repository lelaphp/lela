//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

#[path = "../../mysqlnd/tests/support/mysql.rs"]
mod mysql;
mod support;

use mysql::{MysqlRelay, MysqlServer};
#[cfg(debug_assertions)]
use php_ext::test::Files;
use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = support::engine();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('mysqli'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

/// A `p:` link the first request leaves open is given back to the engine's
/// pool at its end and lent to the next request asking its `hash_key`,
/// taken up with `COM_CHANGE_USER`, so the server's thread id carries over,
/// and `mysqli_get_links_stats()` counts it cached between the two, as a
/// php-fpm worker's does. Against the server of `ci/mysql/compose.yaml`;
/// with none listening there is nothing to connect to, and the test says
/// so and passes.
#[test]
fn a_persistent_link_serves_the_next_request() {
    MysqlServer::with(|_| {
        let engine = support::engine();
        let connect = "mysqli_connect('p:127.0.0.1', 'root', '', 'test')";
        let first = format!("<?php $l = {connect}; echo mysqli_thread_id($l);");
        let served = engine.serve_capture(&first, Capture::default());
        let thread = served.stdout_text();
        assert!(thread.parse::<u64>().is_ok(), "{thread}");
        let stats = "foreach (mysqli_get_links_stats() as $k => $n) echo \"$k=$n \";";
        let second =
            format!("<?php {stats} $l = {connect}; echo mysqli_thread_id($l), ' '; {stats}");
        let served = engine.serve_capture(&second, Capture::default());
        assert_eq!(
            served.stdout_text(),
            format!(
                "total=0 active_plinks=0 cached_plinks=1 {thread} total=1 active_plinks=1 cached_plinks=0 "
            )
        );
    });
}

/// A `p:` link's connection waits between requests in one list, mysqli's
/// free links, with its socket, as php's `mysqli_plist_entry` holds the
/// `MYSQLND` and mysqlnd deletes its stream from `EG(persistent_list)`
/// (#1805): the first request fills the engine's pool of persistent
/// streams to its bound of 1024 with `pfsockopen` sockets before it
/// connects, and the next request still takes up the same server thread,
/// where a connection whose socket waited in that pool lost it past the
/// bound and connected again. Against the server of
/// `ci/mysql/compose.yaml`; with none listening the test says so and
/// passes.
#[test]
fn a_persistent_link_keeps_its_socket_however_full_the_stream_pool_is() {
    MysqlServer::with(|_| {
        let engine = support::engine();
        let connect = "mysqli_connect('p:127.0.0.1', 'root', '', 'test')";
        let first = format!(
            "<?php $s = stream_socket_server('tcp://127.0.0.1:0');
            $port = explode(':', stream_socket_get_name($s, false))[1];
            for ($i = 0; $i < 1024; $i++) {{
                $kept[] = pfsockopen(\"127.0.0.1:$port/$i\", -1);
                $accepted[] = stream_socket_accept($s, 1);
            }}
            $l = {connect}; echo mysqli_thread_id($l);"
        );
        let served = engine.serve_capture(&first, Capture::default());
        let thread = served.stdout_text();
        assert!(thread.parse::<u64>().is_ok(), "{thread}");
        let second = format!("<?php $l = {connect}; echo mysqli_thread_id($l);");
        let served = engine.serve_capture(&second, Capture::default());
        assert_eq!(served.stdout_text(), thread);
    });
}

/// A link `mysqli_init()` made and the first request connected without
/// `p:`, held by two variables so that no destructor pass frees it, is
/// closed as that request ends, as php's free of the link closes its
/// stream, and not kept in the engine's pool of persistent streams:
/// the server's `SHOW PROCESSLIST`, read in the second request, no longer
/// lists its thread. Against the server of `ci/mysql/compose.yaml`; with
/// none listening the test says so and passes.
#[test]
fn a_link_from_init_connected_without_p_is_closed_as_the_request_ends() {
    MysqlServer::with(|_| {
        let engine = support::engine();
        let first = "<?php $l = mysqli_init(); $kept = $l; $l->real_connect('127.0.0.1', 'root', '', 'test'); echo $l->thread_id;";
        let served = engine.serve_capture(first, Capture::default());
        let thread = served.stdout_text();
        assert!(thread.parse::<u64>().is_ok(), "{thread}");
        let second = format!(
            "<?php $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
            for ($i = 0; $i < 50; $i++) {{
                $open = false;
                $r = mysqli_query($l, 'SHOW PROCESSLIST');
                while ($row = mysqli_fetch_assoc($r)) $open = $open || $row['Id'] == {thread};
                if (!$open) break;
                mysqli_query($l, 'DO SLEEP(0.02)');
            }}
            echo $open ? 'open' : 'gone';"
        );
        let served = engine.serve_capture(&second, Capture::default());
        assert_eq!(served.stdout_text(), "gone");
    });
}

/// A session save handler whose `write` queries through a link in a
/// global, served in session's `RSHUTDOWN`, finds that link open, a
/// `mysqli_init` link without `p:` and a `p:` link alike: php frees
/// mysqli's links in `shutdown_executor`, after every extension's
/// `RSHUTDOWN`. The last request prints what each write left, the
/// oracle's bytes for the same requests (#1689). Against the server of `ci/mysql/compose.yaml`;
/// with none listening the test says so and passes.
#[test]
fn a_session_written_at_the_request_end_still_has_the_link() {
    MysqlServer::with(|_| {
        // Spelt whole: session starts before mysqli, so its RSHUTDOWN,
        // whose save handler writes through the link, runs after mysqli's,
        // the order the test holds the link across.
        let engine = Engine::builder()
            .extensions(&[
                php_ext_lexbor::extension(),
                php_ext_json::extension(),
                php_ext_spl::extension(),
                php_ext_uri::extension(),
                php_ext_standard::extension(),
                php_ext_session::extension(),
                php_ext_mysqlnd::extension(),
                php_ext_mysqli::extension(),
            ])
            .build();
        let table = "CREATE TABLE IF NOT EXISTS t1689 (k VARCHAR(8) PRIMARY KEY, v TEXT)";
        let setup = format!(
            "<?php $l = mysqli_connect('127.0.0.1', 'root', '', 'test'); mysqli_query($l, '{table}'); mysqli_query($l, 'DELETE FROM t1689');"
        );
        assert_eq!(
            engine
                .serve_capture(&setup, Capture::default())
                .stdout_text(),
            ""
        );
        for (key, connect) in [
            (
                "init",
                "$l = mysqli_init(); $l->real_connect('127.0.0.1', 'root', '', 'test');",
            ),
            (
                "p",
                "$l = mysqli_connect('p:127.0.0.1', 'root', '', 'test');",
            ),
        ] {
            let written = format!(
                "<?php ini_set('session.use_cookies', '0'); ini_set('session.cache_limiter', '');
                {connect}
                class H implements SessionHandlerInterface {{
                    function __construct(public $link) {{}}
                    function open(string $p, string $n): bool {{ return true; }}
                    function close(): bool {{ return true; }}
                    function read(string $id): string|false {{ return ''; }}
                    function write(string $id, string $data): bool {{
                        global $l;
                        return (bool) mysqli_query($l, \"INSERT INTO t1689 VALUES ('{key}', '$data')\");
                    }}
                    function destroy(string $id): bool {{ return true; }}
                    function gc(int $m): int|false {{ return 0; }}
                }}
                session_set_save_handler(new H($l), false);
                session_id('t1689');
                session_start();
                $_SESSION['{key}'] = 1;"
            );
            assert_eq!(
                engine
                    .serve_capture(&written, Capture::default())
                    .stdout_text(),
                ""
            );
        }
        let read = "<?php $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
            foreach (mysqli_query($l, 'SELECT k, v FROM t1689 ORDER BY k') as $r) echo $r['k'], ' ', $r['v'], \"\\n\";
            mysqli_query($l, 'DROP TABLE t1689');";
        let served = engine.serve_capture(read, Capture::default());
        assert_eq!(served.stdout_text(), "init init|i:1;\np p|i:1;\n");
    });
}

/// What mysqlnd reads off the wire is read into the buffer it parses, as
/// php's `mysqlnd_vio::network_read` reads into its caller's, and the
/// file a `LOAD DATA LOCAL INFILE` sends is read the same way
/// (`mysqlnd_local_infile_read`), so a request that connects, queries,
/// fetches, executes a prepared statement and loads a local file leaves
/// no block of its heap held: armed, the sensor ends a request that
/// kept a packet's string in an `InternalError` (#2092). Against the
/// server of `ci/mysql/compose.yaml`; with none listening the test says
/// so and passes.
#[cfg(debug_assertions)]
#[test]
fn an_armed_request_over_the_wire_leaves_none_of_its_packets_held() {
    MysqlServer::with(|_| {
        let engine = Engine::builder()
            .extensions(support::extensions())
            .ini("mysqli.allow_local_infile", "1")
            .build();
        let files = Files::new("mysqli-local-infile", &[("rows.csv", "1,a\n2,b\n")]);
        let file = files.path().join("rows.csv");
        let source = format!(
            "<?php $l = mysqli_init();
            mysqli_options($l, MYSQLI_OPT_LOCAL_INFILE, true);
            mysqli_real_connect($l, '127.0.0.1', 'root', '', 'test');
            mysqli_query($l, 'SET GLOBAL local_infile = 1');
            mysqli_query($l, 'CREATE TEMPORARY TABLE li (id INT, l CHAR(1))');
            var_dump(mysqli_query($l, \"LOAD DATA LOCAL INFILE '{}' INTO TABLE li FIELDS TERMINATED BY ','\"));
            mysqli_query($l, 'SET GLOBAL local_infile = 0');
            $r = mysqli_query($l, 'SELECT id, l FROM li ORDER BY id');
            while ($row = mysqli_fetch_row($r)) echo $row[0], ' ', $row[1], \"\\n\";
            mysqli_free_result($r);
            $s = mysqli_prepare($l, 'SELECT ? + 1, CAST(1.5 AS DOUBLE), ?');
            $n = 41; $t = 'text';
            mysqli_stmt_bind_param($s, 'is', $n, $t);
            mysqli_stmt_execute($s);
            mysqli_stmt_bind_result($s, $a, $b, $c);
            mysqli_stmt_fetch($s);
            var_dump($a, $b, $c);
            mysqli_stmt_close($s);
            mysqli_close($l);",
            file.display()
        );
        let served = engine.serve_capture(&source, Capture::default());
        assert_eq!(
            served.stdout_text(),
            "bool(true)\n1 a\n2 b\nint(42)\nfloat(1.5)\nstring(4) \"text\"\n"
        );
    });
}

/// `mysqli_get_warnings()` reads `SHOW WARNINGS` off the connection and
/// keeps each row's code and message, the message's string given back
/// once its bytes are copied, so an armed request that reads a warning
/// leaves no block of its heap held (#2092). Against the server of
/// `ci/mysql/compose.yaml`; with none listening the test says so and
/// passes.
#[cfg(debug_assertions)]
#[test]
fn an_armed_request_reading_its_warnings_leaves_none_held() {
    MysqlServer::with(|_| {
        let engine = support::engine();
        let source = "<?php $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
            mysqli_query($l, 'SELECT 1/0');
            $w = mysqli_get_warnings($l);
            var_dump($w->errno, $w->message);";
        let served = engine.serve_capture(source, Capture::default());
        assert_eq!(
            served.stdout_text(),
            "int(1365)\nstring(13) \"Division by 0\"\n"
        );
    });
}

/// A user the server does not know is answered over a plain socket by
/// `caching_sha2_password`'s full authentication, the password encrypted
/// under the server's public key through openssl's natives, and the
/// lookup of the key's size in `openssl_pkey_get_details()`'s answer
/// allocates nothing of its own, so an armed request whose connect is
/// refused leaves no block of its heap held (#2092). Against the server
/// of `ci/mysql/compose.yaml`; with none listening the test says so and
/// passes.
#[cfg(debug_assertions)]
#[test]
fn an_armed_request_refused_by_the_server_leaves_none_held() {
    MysqlServer::with(|_| {
        let engine = Engine::builder()
            .extensions(support::extensions())
            .extensions([php_ext_openssl::extension()])
            .build();
        let source = "<?php mysqli_report(MYSQLI_REPORT_OFF);
            var_dump(@mysqli_connect('127.0.0.1', 'nobody', 'x'), mysqli_connect_errno());";
        let served = engine.serve_capture(source, Capture::default());
        assert_eq!(served.stdout_text(), "bool(false)\nint(1045)\n");
    });
}

/// `mysqli_stmt_error_list()` answers the list it builds and allocates
/// no other array beside it, so an armed request that reads a failed
/// prepare's errors leaves no block of its heap held (#2092). Against
/// the server of `ci/mysql/compose.yaml`; with none listening the test
/// says so and passes.
#[cfg(debug_assertions)]
#[test]
fn an_armed_request_reading_a_statements_errors_leaves_none_held() {
    MysqlServer::with(|_| {
        let engine = support::engine();
        let source = "<?php mysqli_report(MYSQLI_REPORT_OFF);
            $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
            $s = mysqli_stmt_init($l);
            mysqli_stmt_prepare($s, 'SELECT nope');
            var_dump(mysqli_stmt_error_list($s)[0]['errno']);";
        let served = engine.serve_capture(source, Capture::default());
        assert_eq!(served.stdout_text(), "int(1054)\n");
    });
}

/// A text option of `mysqli_options()` given as a number, and the class
/// name `mysqli_result::fetch_object()` is given, are read through
/// `zval_get_string`, whose string is the caller's, and given back once
/// read, so an armed request that passes either leaves no block of its
/// heap held (#2092). Against the server of `ci/mysql/compose.yaml`;
/// with none listening the test says so and passes.
#[cfg(debug_assertions)]
#[test]
fn an_armed_request_whose_text_arguments_are_converted_leaves_none_held() {
    MysqlServer::with(|_| {
        let engine = support::engine();
        let source = "<?php var_dump(mysqli_options(mysqli_init(), MYSQLI_INIT_COMMAND, 5));
            $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
            $class = 'std'; $class .= 'Class';
            var_dump(get_class(mysqli_query($l, 'SELECT 1 AS a')->fetch_object($class)));";
        let served = engine.serve_capture(source, Capture::default());
        assert_eq!(served.stdout_text(), "bool(true)\nstring(8) \"stdClass\"\n");
    });
}

/// The server's public key read from the file `MYSQLI_SERVER_PUBLIC_KEY`
/// names, as `caching_sha2_password`'s full authentication reads it over
/// a plain socket, is copied out of the string the stream's
/// `copy_to_mem` answered and the string given back, so an armed
/// request whose connect reads the file leaves no block of its heap held
/// (#2092). Against the server of `ci/mysql/compose.yaml`; with none
/// listening the test says so and passes.
#[cfg(debug_assertions)]
#[test]
fn an_armed_request_reading_the_servers_key_from_a_file_leaves_none_held() {
    MysqlServer::with(|_| {
        let engine = support::engine();
        let files = Files::new("mysqli-public-key", &[("key.pem", "not a key\n")]);
        let file = files.path().join("key.pem");
        let source = format!(
            "<?php mysqli_report(MYSQLI_REPORT_OFF);
            $l = mysqli_init();
            mysqli_options($l, MYSQLI_SERVER_PUBLIC_KEY, '{}');
            var_dump(@mysqli_real_connect($l, '127.0.0.1', 'nobody', 'x'), mysqli_connect_errno());",
            file.display()
        );
        let served = engine.serve_capture(&source, Capture::default());
        assert_eq!(served.stdout_text(), "bool(false)\nint(1045)\n");
    });
}

/// A `p:` link waiting in mysqli's free links as the engine ends is
/// closed on the shutdown request with a `COM_QUIT`, as php's
/// `zend_shutdown` destroys `EG(persistent_list)` and
/// `php_mysqli_dtor_p_elements` closes each free link
/// (`mysqli_close(mysql, MYSQLI_CLOSE_IMPLICIT)`), where a dropped socket
/// sends the server nothing and counts in its `Aborted_clients` (#1961).
/// The link reaches the server through a relay of the test's own, which
/// reads each packet the server is sent: none after the connect while the
/// engine lives, the `COM_QUIT` last once its last clone is dropped.
/// Against the server of `ci/mysql/compose.yaml`; with none listening the
/// test says so and passes.
#[test]
fn a_free_persistent_link_is_quit_as_the_engine_ends() {
    use std::time::Duration;

    MysqlServer::with(|server| {
        let relay = MysqlRelay::start(server);
        let port = relay.at().port();
        let engine = support::engine();
        let script = format!(
            "<?php $l = mysqli_connect('p:127.0.0.1', 'root', '', 'test', {port}); echo $l ? 'connected' : 'refused';"
        );
        let served = engine.serve_capture(&script, Capture::default());
        assert_eq!(served.stdout_text(), "connected");
        assert!(
            relay.closed_within(Duration::from_millis(200)).is_none(),
            "the free link was closed before the engine ended"
        );
        drop(engine);
        relay
            .closed_within(Duration::from_secs(10))
            .expect("the client closes its side as the engine ends")
            .assert_quit();
    });
}
