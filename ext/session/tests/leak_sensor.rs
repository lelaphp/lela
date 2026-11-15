//! The request leak sensor over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

mod support;

use php_ext::test::{Capture, Engine};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_session::extension()])
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// A cookie parameter given twice in `session_set_cookie_params`'
/// array keeps the last, and the first's string stays held to the end
/// of the request, as php overwrites it unreleased and
/// `memory_get_usage()` reads it, where the request gives it up.
#[test]
fn a_cookie_parameter_given_twice_is_given_up_at_the_request_s_end() {
    let stdout = armed(
        r#"<?php
session_set_cookie_params(['path' => 123457]);
$m = memory_get_usage(); session_set_cookie_params(['path' => 123456, 'PATH' => 654321]); echo memory_get_usage() - $m, "\n";
"#,
    );
    assert_eq!(stdout, "32\n");
}

/// A read that throws inside a session decode makes none of the
/// `__wakeup()` calls queued before it, as php's `zend_call_function`
/// refuses each at `EG(exception)`, so the throwing `__wakeup()` makes
/// no second throwable to drop, under the `php` and the
/// `php_serialize` handlers alike.
#[test]
fn a_read_that_throws_in_a_session_decode_makes_no_delayed_call() {
    let engine = support::engine();
    let served = engine.serve_capture(
        r#"<?php
class A { function __wakeup() { echo "wakeup A\n"; throw new Exception("wake"); } }
class T { public int $p; }
$o = ["save_path" => sys_get_temp_dir(), "use_cookies" => 0, "cache_limiter" => ""];
ob_start(); session_start($o);
try { @session_decode('a|O:1:"A":0:{}b|O:1:"T":1:{s:1:"p";s:1:"x";}'); } catch (TypeError $e) { echo "caught\n"; }
ini_set("session.serialize_handler", "php_serialize"); session_start($o);
try { @session_decode('a:2:{i:0;O:1:"A":0:{}i:1;O:1:"T":1:{s:1:"p";s:1:"x";}}'); } catch (TypeError $e) { echo "caught\n"; }
ob_end_flush();
"#,
        Capture::default(),
    );
    served.assert_clean("caught\ncaught\n");
}

/// A `__wakeup()` that throws after a good `php_serialize` read leaves
/// the value bound as `$_SESSION` and its throwable pending, as php's
/// decode answers by the read alone; the armed engine ends the
/// request with no block held.
#[test]
fn a_delayed_throw_after_a_good_php_serialize_read_binds_the_value() {
    let engine = support::engine();
    let served = engine.serve_capture(
        r#"<?php
class A { function __wakeup() { echo "wakeup A\n"; throw new Exception("wake"); } }
ini_set("session.serialize_handler", "php_serialize");
ob_start(); session_start(["save_path" => sys_get_temp_dir(), "use_cookies" => 0, "cache_limiter" => ""]);
try { var_dump(session_decode('a:2:{s:1:"x";i:1;s:1:"a";O:1:"A":0:{}}')); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
echo implode(',', array_keys($_SESSION)), ' ', get_class($_SESSION['a']), "\n";
session_destroy(); ob_end_flush();
"#,
        Capture::default(),
    );
    served.assert_clean("wakeup A\nwake\nx,a A\n");
    assert_eq!(String::from_utf8_lossy(&served.stderr), "");
}
