//! The owner type as a program meets it (#1995): an owner a native
//! drops without handing it to a sink or giving it up is reported, in a
//! debug build, as the `InternalError` naming the line that made it,
//! whatever it owns; one given up or returned is reported nowhere, and
//! an owner returned from a native is the caller's.

use php_macros::php_function;

use crate::api::abi::{Extension, ExtensionBuilder};
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Key, Owner, PhpArray, PhpString, Value};
use crate::engine::Engine;
use crate::test::Capture;

/// The line of `drop_owner`'s [`Owner::take`].
const TAKEN_AT: u32 = line!() + 6;

/// `drop_owner(mixed $value): void`, faulty on purpose: an owner of
/// `$value` taken and dropped unconsumed.
#[php_function]
fn drop_owner(req: &mut Request, value: Value) {
    let owner = Owner::take(req, value);
    let _view = owner.tag();
}

/// `give_up_owner(mixed $value): void`: an owner of `$value` taken and
/// given up.
#[php_function]
fn give_up_owner(req: &mut Request, value: Value) {
    Owner::take(req, value).give_up(req);
}

/// `owned_first(array $array): mixed`: the first element, returned as
/// an owner the native took of it.
#[php_function]
fn owned_first(req: &mut Request, array: PhpArray) -> Owner<Value> {
    Owner::take(req, array.get(&Key::Int(0)).unwrap_or_default())
}

/// `fresh(): string`: a string made by the native, returned as the view
/// of the one owner it was made with, handed on.
#[php_function]
fn fresh(req: &mut Request) -> Result<PhpString, Thrown> {
    PhpString::new(req, b"made by the native")
}

/// `s($c)`: a string of the request's, counted, which the program builds.
const COUNTED: &str = "function s($c) { return $c . PHP_INT_MAX; }";

fn owners() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            drop_owner::declared(),
            give_up_owner::declared(),
            owned_first::declared(),
            fresh::declared(),
        ])
        .build("owners")
}

/// An owner of each kind dropped unconsumed, a counted string, an
/// interned one, an integer, an array and an object among them, ends the
/// request at the next sensing point in the `InternalError` naming the
/// line that took it, and nothing after it runs.
#[test]
fn an_owner_dropped_unconsumed_is_reported_with_the_line_that_made_it() {
    let engine = Engine::cli_builder().extensions(&[owners()]).build();
    let site = format!(
        "Fatal error: InternalError: the owner made at crates/php_engine/src/tests/owners.rs:{TAKEN_AT}:"
    );
    for value in ["s('x')", "'interned'", "1", "[1, 2]", "new stdClass"] {
        let served = engine.serve_capture(
            &format!("<?php {COUNTED} echo 'a';\ndrop_owner({value});\necho 'went on';\n"),
            Capture::default(),
        );
        let stdout = String::from_utf8_lossy(&served.stdout);
        assert!(
            stdout.starts_with(&format!("a\n{site}")),
            "{value}: {stdout}"
        );
        assert!(
            stdout.contains(" was dropped without being handed to a sink or given up"),
            "{value}: {stdout}"
        );
        assert!(!stdout.contains("went on"), "{value}: {stdout}");
        assert_eq!(served.exit_code, 255, "{value}");
    }
}

/// An owner given up is consumed: nothing is reported and the value is
/// the caller's as before.
#[test]
fn an_owner_given_up_is_reported_nowhere() {
    let engine = Engine::cli_builder().extensions(&[owners()]).build();
    let served = engine.serve_capture(
        &format!("<?php {COUNTED} $s = s('x');\ngive_up_owner($s);\ngive_up_owner(new stdClass);\necho $s, \"\\n\";\n"),
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "x9223372036854775807\n"
    );
    assert_eq!(served.exit_code, 0);
}

/// An owner returned is moved to the caller: the element outlives the
/// array it was read from, so the slot it stands in is not handed to the
/// string made after the array is freed. A view the native made and
/// returns is the one owner it was made with, handed on.
#[test]
fn returning_an_owner_and_returning_a_fresh_view_are_both_correct() {
    let engine = Engine::cli_builder().extensions(&[owners()]).build();
    let served = engine.serve_capture(
        &format!("<?php {COUNTED} $a = [s('x')];\n$v = owned_first($a);\nunset($a);\n$b = s('y');\necho $v, \"\\n\", $b, \"\\n\";\nunset($v);\necho fresh(), \"\\n\";\n$f = fresh(); unset($f);\n"),
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "x9223372036854775807\ny9223372036854775807\nmade by the native\n"
    );
    assert_eq!(served.exit_code, 0);
}
