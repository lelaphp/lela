//! Natives that walk a table while code of the program runs: where the
//! program frees the table under a walk that holds nothing, the walk
//! ends there. Each request runs on an armed engine, so a block read or
//! released after its free fails the test.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with this extension alone wrote
/// to its standard stream and to its error stream.
fn armed(source: &str) -> (String, String) {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
        ])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    (
        served.stdout_text(),
        String::from_utf8_lossy(&served.stderr).into_owned(),
    )
}

/// `phpinfo(INFO_VARIABLES)` walks a superglobal's rows holding nothing:
/// where the program frees the table under the walk, the walk ends
/// there, so the rows after it are not printed.
#[test]
fn phpinfo_ends_its_walk_where_the_superglobal_is_freed() {
    let (stdout, stderr) = armed(
        r#"<?php
class D { function __debugInfo() { $GLOBALS['_GET'] = ['x']; $s = str_repeat('z', 3); return []; } }
$_GET = ['a' => new D, 'b' => str_repeat('b', 3), 'c' => [1]];
phpinfo(INFO_VARIABLES);
echo "done\n";
"#,
    );
    assert!(stdout.contains("$_GET['a'] => "), "{stdout}{stderr}");
    assert!(!stdout.contains("$_GET['b']"), "{stdout}{stderr}");
    assert!(stdout.ends_with("done\n"), "{stdout}{stderr}");
    assert_eq!(stderr, "");
}

/// `var_export` holds an array it exports, `GC_ADDREF(myht)`, so an
/// error handler its circular warning reaches that assigns over the
/// array's only holder leaves it standing until the export is done, and
/// the rows after are written as php writes them.
#[test]
fn var_export_holds_the_array_it_exports() {
    let (stdout, stderr) = armed(
        r"<?php
$o = new stdClass; $o->self = $o;
set_error_handler(function () { global $arr; $arr = null; $s = str_repeat('z', 3); return true; });
$arr = [$o, str_repeat('b', 3), [str_repeat('c', 2)]];
$outer = [&$arr];
var_export($outer);
",
    );
    assert!(
        stdout.contains("1 => 'bbb',\n    2 => \n    array (\n      0 => 'cc',"),
        "{stdout}{stderr}"
    );
    assert_eq!(stderr, "");
}

/// `SEND_ARRAY`, what `call_user_func_array` compiles to, spreads the
/// array walking it holding nothing: where the program frees the array
/// under the walk, the walk ends there, and the call is made with the
/// arguments placed so far.
#[test]
fn send_array_ends_where_a_warning_frees_the_array() {
    let (stdout, stderr) = armed(
        r#"<?php
function f(&$a, $b = 'B', $c = 'C') { echo "$a $b $c\n"; }
set_error_handler(function ($n, $m) { global $args; echo "W: $m\n"; $args = null; return true; });
$args = [1, str_repeat('b', 2), str_repeat('c', 2)];
call_user_func_array('f', $args);
"#,
    );
    assert_eq!(
        stdout,
        "W: f(): Argument #1 ($a) must be passed by reference, value given\n1 B C\n"
    );
    assert_eq!(stderr, "");
}
