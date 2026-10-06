//! The leaks the request leak sensor found in the handlers and the
//! constant evaluator (#2057), each a program served with the sensor
//! armed (`EngineBuilder::sense_leaks`): what the program wrote, and no
//! `InternalError` naming a block the heap still held after it.

use crate::engine::{Engine, LeakSensor};
use crate::test::Capture;

/// `source` served by an engine with the sensor armed: what it wrote,
/// and its exit.
fn armed(source: &str) -> (String, i32) {
    let engine = Engine::cli_builder().sense_leaks(LeakSensor::Armed).build();
    let served = engine.serve_capture(source, Capture::default());
    (
        String::from_utf8_lossy(&served.stdout).into_owned(),
        served.exit_code,
    )
}

/// A constant expression that throws while it builds an array gives up
/// the array, whether the element that threw was a key with no room
/// past the last, a constant nothing defined or a spread, and one that
/// throws while it evaluates a `new`'s arguments gives up those it
/// evaluated before (#2060).
#[test]
fn a_constant_array_that_throws_gives_up_the_array() {
    let (stdout, exit) = armed(
        r#"<?php
function test($x = [PHP_INT_MAX => 42, 'foo']) {}
try { test(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
class D { const Z = [NOPE]; const U = [PHP_INT_MAX => 1, ...[2]]; const S = [1 => 1, 'c' . 'd' => NOPE]; }
try { D::Z; } catch (Error $e) { echo $e->getMessage(), "\n"; }
try { D::U; } catch (Error $e) { echo $e->getMessage(), "\n"; }
try { D::S; } catch (Error $e) { echo $e->getMessage(), "\n"; }
define('Q', 'x');
class P { function __construct($a, $b = null) {} }
function f($x = new P([Q], NOPE)) {}
try { f(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
function g($x = new P(b: [Q], a: NOPE)) {}
try { g(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
"#,
    );
    assert_eq!(
        stdout,
        "Cannot add element to the array as the next element is already occupied\n\
         Undefined constant \"NOPE\"\n\
         Cannot add element to the array as the next element is already occupied\n\
         Undefined constant \"NOPE\"\n\
         Undefined constant \"NOPE\"\n\
         Undefined constant \"NOPE\"\n"
    );
    assert_eq!(exit, 0);
}

/// A constant expression that evaluates cleanly gives up the key it
/// evaluated once the array holds its own owner of it (#2061).
#[test]
fn a_constant_array_gives_up_the_keys_it_evaluated() {
    let (stdout, exit) = armed(
        r#"<?php
const A = "ok";
function f7($x = array('aaa' => A)) { echo $x['aaa'], "\n"; }
f7();
const T = ['fo' . 'o' => 1, 'b' => 'a' . 'r'];
foreach (T as $k => $v) { echo $k, '=', $v, "\n"; }
"#,
    );
    assert_eq!(stdout, "ok\nfoo=1\nb=ar\n");
    assert_eq!(exit, 0);
}
