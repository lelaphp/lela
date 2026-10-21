//! The session's natives that walk the session's array while code of
//! the program runs (#2004), on an armed engine, so a block read or
//! released after its free fails the test as the `InternalError` it is.
#![cfg(debug_assertions)]

mod support;

use php_ext::test::Capture;

/// `session_unset` destroys the session's variables in order, serving
/// each destructor as it goes, and ends where a destructor replaces
/// `$_SESSION`: the array the destructor wrote and the tables it made
/// after are left as they were.
#[test]
fn session_unset_ends_where_a_destructor_replaces_the_session() {
    let engine = support::engine();
    let served = engine.serve_capture(
        r#"<?php
ob_start();
session_start(["save_path" => sys_get_temp_dir(), "use_cookies" => 0, "cache_limiter" => ""]);
class D { function __destruct() {
    $_SESSION = ['new' => str_repeat('n', 3)];
    for ($i = 0; $i < 40; $i++) { $GLOBALS['keep'][] = ['a' => 0, 'b' => 1, 'c' => 2]; }
} }
$_SESSION['a'] = new D; $_SESSION['b'] = str_repeat('b', 3); $_SESSION['c'] = [1];
session_unset();
echo implode(',', array_keys($_SESSION)), ' ', array_sum(array_map('count', $keep)), "\n";
session_destroy();
ob_end_flush();
"#,
        Capture::default(),
    );
    assert_eq!(served.stdout_text(), "new 120\n");
    assert_eq!(String::from_utf8_lossy(&served.stderr), "");
}
