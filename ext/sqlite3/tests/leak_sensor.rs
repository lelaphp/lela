//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// `src` served by an armed engine with this extension alone, what it
/// wrote held to `expected`.
fn served(src: &str, expected: &str) {
    let engine = Engine::builder()
        .extensions(&[php_ext_sqlite3::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(src, Capture::default());
    assert_eq!(served.fatal, None);
    assert_eq!(String::from_utf8_lossy(&served.stdout), expected);
}

/// `SQLite3::setAuthorizer` on a connection that is not open throws the
/// one `Error` `SQLITE3_CHECK_INITIALIZED` throws, not a second over the
/// first (#2075).
#[test]
fn set_authorizer_on_a_closed_connection_throws_once() {
    served(
        "<?php $db = new SQLite3(':memory:'); $db->close();\n\
         try { $db->setAuthorizer(null); } catch (Error $e) { echo $e->getMessage(), $e->getPrevious() === null ? \" alone\" : \" chained\", \"\\n\"; }\n",
        "The SQLite3 object has not been correctly initialised or is already closed alone\n",
    );
}

/// What a user function or an aggregate's final returns is handed to
/// the library as text, `sqlite3_result_text` over the string
/// `zval_get_string` made, which is given up after (#2099).
#[test]
fn a_user_function_s_answer_is_given_up() {
    served(
        r#"<?php
$db = new SQLite3(':memory:');
$db->exec("CREATE TABLE t (a); INSERT INTO t VALUES ('x'), ('y')");
$db->createFunction('twice', fn ($v) => $v . $v);
$db->createAggregate('cat', fn ($c, $row, $v) => "$c$v", fn ($c, $rows) => "$c/$rows");
var_dump($db->querySingle('SELECT twice(a) FROM t'), $db->querySingle('SELECT cat(a) FROM t'));
"#,
        "string(2) \"xx\"\nstring(4) \"xy/0\"\n",
    );
}
