//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with this extension and `pdo`
/// wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_json::extension(),
            php_ext_spl::extension(),
            php_ext_pdo::extension(),
            php_ext_pdo_sqlite::extension(),
        ])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// What a user function or an aggregate's final returns is handed to
/// the library as text, `sqlite3_result_text` over the string
/// `zval_get_string` made, which is given up after (#2099).
#[test]
fn a_user_function_s_answer_is_given_up() {
    let stdout = armed(
        r#"<?php
$db = new Pdo\Sqlite('sqlite::memory:');
$db->exec("CREATE TABLE t (a); INSERT INTO t VALUES ('x'), ('y')");
$db->createFunction('twice', fn ($v) => $v . $v);
$db->createAggregate('cat', function (&$c, $row, $v) { $c .= $v; return $c; }, fn ($c, $rows) => "$c/$rows");
var_dump($db->query('SELECT twice(a) FROM t')->fetchColumn(), $db->query('SELECT cat(a) FROM t')->fetchColumn());
"#,
    );
    assert_eq!(stdout, "string(2) \"xx\"\nstring(4) \"xy/3\"\n");
}

/// What `var_dump` shows of a `PDORow`, `row_get_properties_for`'s
/// table of the statement's properties and each column fetched, is the
/// dumper's to give up with every value fetched for it (#2117).
#[test]
fn a_dumped_lazy_row_s_values_are_given_up() {
    let stdout = armed(
        r#"<?php
$db = new PDO('sqlite::memory:');
$r = $db->query("SELECT 'ab' AS id", PDO::FETCH_LAZY); foreach ($r as $v) var_dump($v);
"#,
    );
    assert_eq!(
        stdout,
        "object(PDORow)#4 (2) {\n  [\"queryString\"]=>\n  string(17) \"SELECT 'ab' AS id\"\n  [\"id\"]=>\n  string(2) \"ab\"\n}\n"
    );
}

/// A column named by a canonical decimal is keyed by the integer,
/// `zend_symtable_update`, and the string made for its name is given up
/// (#2117, `ext/pdo/tests/bug_79106.phpt`).
#[test]
fn a_numeric_column_name_keeps_no_string() {
    let stdout = armed(
        r#"<?php
$db = new PDO('sqlite::memory:');
$row = $db->query('SELECT 0 AS "2007", 1 AS "2020"')->fetch(); echo $row[2007], $row[2020], "\n";
"#,
    );
    assert_eq!(stdout, "01\n");
}

/// A statement class given by `ATTR_STATEMENT_CLASS` without
/// constructor arguments is constructed with none, and the empty array
/// that stood for them is given up (#2117, `ext/pdo/tests/pdo_014.phpt`).
#[test]
fn a_statement_class_constructed_without_arguments_keeps_no_array() {
    let stdout = armed(
        r#"<?php
class S extends PDOStatement { protected function __construct() { echo "made\n"; } }
$db = new PDO('sqlite::memory:'); $db->setAttribute(PDO::ATTR_STATEMENT_CLASS, ['S']);
echo get_class($db->query('SELECT 1')), "\n";
"#,
    );
    assert_eq!(stdout, "made\nS\n");
}

/// `setAuthorizer` on an object with no connection, one whose
/// constructor never ran the parent's, throws before it
/// keeps the callback, as `PDO_CONSTRUCT_CHECK` comes before
/// `zend_fcc_dup` (#2133,
/// `ext/pdo_sqlite/tests/subclasses/pdosqlite_setauthorizer_trampoline_no_leak.phpt`).
#[test]
fn set_authorizer_with_no_connection_keeps_no_callback() {
    let stdout = armed(
        r#"<?php
class T { function __call($n, $a) { return Pdo\Sqlite::OK; } }
class S extends Pdo\Sqlite { function __construct() {} }
$obj = new S;
try { $obj->setAuthorizer([new T, 'authorizer']); } catch (Error $e) { echo $e->getMessage(), "\n"; }
"#,
    );
    assert_eq!(stdout, "S object is uninitialized\n");
}
