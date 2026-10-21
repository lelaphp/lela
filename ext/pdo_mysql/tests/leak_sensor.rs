//! The request leak sensor (#1992) over PDO through this connector: each
//! request below gives up everything the natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
//! Against the server of `ci/mysql/compose.yaml`; with none listening
//! each test says so and passes.
#![cfg(debug_assertions)]

#[path = "../../mysqlnd/tests/support/mysql.rs"]
mod mysql;
mod support;

use mysql::MysqlServer;
use php_ext::test::Capture;

/// What one request of an armed engine with PDO, this connector and
/// standard wrote for `body`, served after `$db` is connected under
/// `PDO::ERRMODE_EXCEPTION`.
fn armed(body: &str) -> String {
    let engine = support::engine();
    let source = format!(
        "<?php $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]);\n{body}"
    );
    let served = engine.serve_capture(&source, Capture::default());
    served.stdout_text()
}

/// An emulated prepare quotes each bound value into the query as its
/// string, `zval_try_get_string`, and gives that string up once quoted:
/// a number bound as a string, converted in place where it is bound,
/// and a string bound as one (#2117, #2116).
#[test]
fn an_emulated_prepare_gives_up_the_strings_it_quotes() {
    MysqlServer::with(|_| {
        let stdout = armed(
            r#"$s = $db->prepare('SELECT ?, ?'); $s->execute([15, 'a' . 'bc']); echo implode(',', $s->fetch(PDO::FETCH_NUM)), "\n";
$s->execute([16, str_repeat('x', 3)]); echo implode(',', $s->fetch(PDO::FETCH_NUM)), "\n";
"#,
        );
        assert_eq!(stdout, "15,abc\n16,xxx\n");
    });
}

/// A fetch into a class whose constructor takes no arguments calls it
/// with none, and the empty array that stood for them is given up
/// (#2117).
#[test]
fn a_constructor_called_without_arguments_keeps_no_array() {
    MysqlServer::with(|_| {
        let stdout = armed(
            r#"class R { public $a; function __construct() { echo "made\n"; } }
$o = $db->query('SELECT 1 AS a')->fetchObject('R'); echo $o->a, "\n";
"#,
        );
        assert_eq!(stdout, "made\n1\n");
    });
}

/// `fetchAll` gives up the rows it gathered where a fetch throws
/// partway, as the VM frees the return value of a native that threw
/// (#2117).
#[test]
fn fetch_all_gives_up_its_rows_where_a_fetch_throws() {
    MysqlServer::with(|_| {
        let stdout = armed(
            r#"function f($a) { if ($a == 2) throw new Exception('two'); return $a; }
try { $db->query('SELECT 1 UNION SELECT 2')->fetchAll(PDO::FETCH_FUNC, 'f'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
"#,
        );
        assert_eq!(stdout, "two\n");
    });
}

/// `fetchAll` under `FETCH_GROUP` or `FETCH_UNIQUE` reads each row's
/// group key as a string and gives it up once the row is placed, the key
/// a decimal names included, and gives up the rows it grouped where a
/// fetch throws partway (#2117).
#[test]
fn a_grouped_fetch_all_keeps_no_key_and_no_row() {
    MysqlServer::with(|_| {
        let stdout = armed(
            r#"$q = "SELECT 'aa' AS a, 'bb' AS b UNION SELECT 'aa', 'cc' UNION SELECT '12', 'dd'";
$g = $db->query($q)->fetchAll(PDO::FETCH_COLUMN | PDO::FETCH_GROUP); $u = $db->query($q)->fetchAll(PDO::FETCH_UNIQUE);
echo implode(',', $g['aa']), ' ', $g[12][0], ' ', $u['aa']['b'], "\n";
try { $db->query($q)->fetchAll(PDO::FETCH_FUNC | PDO::FETCH_GROUP, function ($b) { if ($b == 'dd') throw new Exception('dd'); return $b; }); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
"#,
        );
        assert_eq!(stdout, "bb,cc dd cc\ndd\n");
    });
}
