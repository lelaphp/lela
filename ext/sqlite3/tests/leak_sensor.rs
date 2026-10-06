//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_sqlite3::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// What a user function or an aggregate's final returns is handed to
/// the library as text, `sqlite3_result_text` over the string
/// `zval_get_string` made, which is given up after (#2099).
#[test]
fn a_user_function_s_answer_is_given_up() {
    let stdout = armed(
        r#"<?php
$db = new SQLite3(':memory:');
$db->exec("CREATE TABLE t (a); INSERT INTO t VALUES ('x'), ('y')");
$db->createFunction('twice', fn ($v) => $v . $v);
$db->createAggregate('cat', fn ($c, $row, $v) => "$c$v", fn ($c, $rows) => "$c/$rows");
var_dump($db->querySingle('SELECT twice(a) FROM t'), $db->querySingle('SELECT cat(a) FROM t'));
"#,
    );
    assert_eq!(stdout, "string(2) \"xx\"\nstring(4) \"xy/0\"\n");
}
