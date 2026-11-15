//! The request leak sensor over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine};

/// `src` served by an armed engine with this extension alone, what it
/// wrote held to `expected`.
fn served(src: &str, expected: &str) {
    let engine = Engine::builder()
        .extensions(&[php_ext_sqlite3::extension()])
        .build();
    let served = engine.serve_capture(src, Capture::default());
    served.assert_clean(expected);
}

/// `SQLite3::setAuthorizer` on a connection that is not open throws the
/// one `Error` `SQLITE3_CHECK_INITIALIZED` throws, not a second over the
/// first.
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
/// `zval_get_string` made, which is given up after.
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

/// The authorizer a reopen forgets, which php leaks, is given up
/// by the extension's `rshutdown`, after the store's walk has destructed
/// what it holds, so the request ends holding none of it.
#[test]
fn an_authorizer_a_reopen_forgets_is_given_up_at_the_request_s_end() {
    served(
        r#"<?php
class A { function __destruct() { echo "dtor\n"; } }
$db = new SQLite3(':memory:');
$a = new A;
$db->setAuthorizer(function () use ($a) { return SQLite3::OK; });
unset($a);
$db->close();
$db->open(':memory:');
$db->setAuthorizer(fn () => SQLite3::OK);
$db->close();
$db->open(':memory:');
unset($db);
echo "end\n";
"#,
        "end\ndtor\n",
    );
}
