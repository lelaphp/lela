//! The owner type as a program meets it: an owner a native
//! drops without handing it to a sink or giving it up is reported, in a
//! debug build, as the `InternalError` naming the line that made it,
//! whatever counted value it owns; one given up or returned is reported
//! nowhere, and
//! an owner returned from a native is the caller's.

use crate::api::values::ClassId;
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

/// `first(array $array): mixed`: the first element, returned as the
/// view it is, which the engine takes its own owner of.
#[php_function]
fn first(array: PhpArray) -> Value {
    array.get(&Key::Int(0)).unwrap_or_default()
}

/// `fresh(): string`: a string made by the native, returned as the
/// owner it was made with.
#[php_function]
fn fresh(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, b"made by the native")
}

/// `appended(array $array): array`: the array with one more element,
/// written through an owner taken of it, which the write separates from
/// the caller's.
#[php_function]
fn appended(req: &mut Request, array: PhpArray) -> Result<Owner<PhpArray>, Thrown> {
    let mut owner = Owner::take(req, array);
    owner.append(req, Owner::int(1))?;
    Ok(owner)
}

/// `held_past_the_limit(mixed $value, int $bytes): void`: an owner of
/// `$value` held while a string of `$bytes` is made, which meets the
/// memory limit, so the owner is dropped as the native leaves by its
/// fatal; given up otherwise.
#[php_function]
fn held_past_the_limit(req: &mut Request, value: Value, bytes: i64) -> Result<(), Thrown> {
    let owner = Owner::take(req, value);
    let made = PhpString::new(req, &vec![b'x'; usize::try_from(bytes).unwrap_or(0)])?;
    made.give_up(req);
    owner.give_up(req);
    Ok(())
}

/// `as_object(mixed $value): ?object`: the object `$value` is, returned
/// as the owner taken of `$value` recast; `null`, that owner given up,
/// where it is no object.
#[php_function]
fn as_object(req: &mut Request, value: Value) -> Owner<Value> {
    match Owner::take(req, value).into_object() {
        Ok(object) => object.into_value(),
        Err(other) => {
            other.give_up(req);
            Owner::null()
        }
    }
}

/// `filled_past_a_throw(mixed $value): array`: a table filled with
/// `$value` whose next step throws, the table given up by
/// [`PhpArray::filled`] as the throwable is handed on.
#[php_function]
fn filled_past_a_throw(req: &mut Request, value: Value) -> Result<Owner<PhpArray>, Thrown> {
    PhpArray::filled(req, |req, table| {
        table.append(req, Owner::take(req, value))?;
        Err(crate::api::raising::throw_error(
            req,
            ClassId::ERROR,
            "thrown",
        ))
    })
}

/// The line of `held_past_a_throw`'s [`Owner::take`].
const THROWN_AT: u32 = line!() + 6;

/// `held_past_a_throw(mixed $value): void`, faulty on purpose: an owner
/// of `$value` dropped unconsumed as the native leaves by an `Error`.
#[php_function]
fn held_past_a_throw(req: &mut Request, value: Value) -> Result<(), Thrown> {
    let _owner = Owner::take(req, value);
    Err(crate::api::raising::throw_error(
        req,
        ClassId::ERROR,
        "thrown",
    ))
}

/// `s($c)`: a string of the request's, counted, which the program builds.
const COUNTED: &str = "function s($c) { return $c . PHP_INT_MAX; }";

fn owners() -> Extension {
    ExtensionBuilder::default()
        .functions(&[
            drop_owner::DECLARED,
            give_up_owner::DECLARED,
            owned_first::DECLARED,
            first::DECLARED,
            fresh::DECLARED,
            appended::DECLARED,
            held_past_the_limit::DECLARED,
            held_past_a_throw::DECLARED,
            as_object::DECLARED,
            filled_past_a_throw::DECLARED,
        ])
        .build("owners")
}

/// An owner of each counted kind dropped unconsumed, a counted string,
/// an array and an object, ends the request at the next sensing point in
/// the `InternalError` naming the line that took it, and nothing after it
/// runs. An owner of what no count owns, a string the engine's table
/// holds or an integer, leaks nothing and is reported nowhere: its line
/// is kept beside a block, and it has none. A constant string the
/// engine's table does not hold is the unit's own string, counted, as a
/// request-only unit's constants are under opcache.
#[test]
fn an_owner_dropped_unconsumed_is_reported_with_the_line_that_made_it() {
    let engine = Engine::cli_builder().extensions(&[owners()]).build();
    let site = format!(
        "Fatal error: InternalError: the owner made at crates/php_engine/src/tests/owners.rs:{TAKEN_AT}:"
    );
    for value in ["s('x')", "[1, s('x')]", "new stdClass"] {
        let served = engine.serve_capture(
            &format!("<?php {COUNTED} echo 'a';\ndrop_owner({value});\necho 'went on';\n"),
            Capture::default(),
        );
        let stdout = served.stdout_text();
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
    for value in ["'strlen'", "1"] {
        let served = engine.serve_capture(
            &format!("<?php echo 'a';\ndrop_owner({value});\necho 'went on';\n"),
            Capture::default(),
        );
        assert_eq!(served.stdout_text(), "awent on", "{value}");
        assert_eq!(served.exit_code, 0, "{value}");
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
    served.assert_clean("x9223372036854775807\n");
}

/// An owner returned is moved to the caller: the element outlives the
/// array it was read from, so the slot it stands in is not handed to the
/// string made after the array is freed. A string the native made is
/// returned as the owner it was made with.
#[test]
fn returning_an_owner_taken_and_an_owner_made_are_both_correct() {
    let engine = Engine::cli_builder().extensions(&[owners()]).build();
    let served = engine.serve_capture(
        &format!("<?php {COUNTED} $a = [s('x')];\n$v = owned_first($a);\nunset($a);\n$b = s('y');\necho $v, \"\\n\", $b, \"\\n\";\nunset($v);\necho fresh(), \"\\n\";\n$f = fresh(); unset($f);\n"),
        Capture::default(),
    );
    served.assert_clean("x9223372036854775807\ny9223372036854775807\nmade by the native\n");
}

/// A write through an owner that separates the array moves the owner to
/// the copy: the copy is what the native returns, and the caller's array
/// keeps its own owner and its elements.
#[test]
fn a_write_through_an_owner_moves_it_to_the_separated_copy() {
    let engine = Engine::cli_builder().extensions(&[owners()]).build();
    let served = engine.serve_capture(
        &format!("<?php {COUNTED} $a = [s('x')]; $b = appended($a); echo isset($a[1]) ? 'shared' : 'kept', $b[1], \"\\n\"; unset($a); $c = s('y'); echo $b[0], $b[1], \"\\n\";\n"),
        Capture::default(),
    );
    served.assert_clean("kept1\nx92233720368547758071\n");
}

/// A view returned is the engine's to own: the element outlives the
/// array it was read from, so the slot it stands in is not handed to the
/// string made after the array is freed, with no owner taken by the
/// native.
#[test]
fn returning_a_view_of_an_element_takes_the_engines_own_owner() {
    let engine = Engine::cli_builder().extensions(&[owners()]).build();
    let served = engine.serve_capture(
        &format!("<?php {COUNTED} $a = [s('x')];\n$v = first($a);\nunset($a);\n$b = s('y');\necho $v, \"\\n\", $b, \"\\n\";\n"),
        Capture::default(),
    );
    served.assert_clean("x9223372036854775807\ny9223372036854775807\n");
}

/// An owner a native holds as it meets the hard memory limit is dropped
/// on the path that ends the request, which php leaves by `_exit` with
/// the native's locals unfreed: the request writes php's line alone.
/// The same native given room gives its owner up.
#[test]
fn an_owner_dropped_on_the_path_to_a_fatal_is_not_reported() {
    let engine = Engine::cli_builder().extensions(&[owners()]).build();
    let served = engine.serve_capture(
        &format!("<?php {COUNTED} held_past_the_limit(s('x'), 8);\necho 'room';\nini_set('memory_limit', '2M');\nheld_past_the_limit(s('x'), 4 << 20);\necho 'went on';\n"),
        Capture::default(),
    );
    let stdout = served.stdout_text();
    let stderr = String::from_utf8_lossy(&served.stderr);
    assert_eq!(stdout, "room", "{stdout}");
    assert!(
        stderr.starts_with("Hard memory limit of 4194304 bytes exhausted"),
        "{stderr}"
    );
    assert!(!stderr.contains("InternalError"), "{stderr}");
    assert_eq!(served.exit_code, 137);
}

/// An owner dropped unconsumed as its native leaves by a throwable is on
/// an ordinary path, which the native was to clean up after: it is
/// reported with the line that made it.
#[test]
fn an_owner_dropped_on_an_exceptions_path_is_reported() {
    let engine = Engine::cli_builder().extensions(&[owners()]).build();
    let served = engine.serve_capture(
        &format!("<?php {COUNTED} try {{ held_past_a_throw(s('x')); }} catch (Error $e) {{ echo 'caught'; }}\necho 'went on';\n"),
        Capture::default(),
    );
    let stdout = served.stdout_text();
    assert!(
        stdout.contains(&format!("Fatal error: InternalError: the owner made at crates/php_engine/src/tests/owners.rs:{THROWN_AT}:")),
        "{stdout}"
    );
    assert!(!stdout.contains("went on"), "{stdout}");
    assert_eq!(served.exit_code, 255);
}

/// An owner of a value recast to an owner of the object it is keeps the
/// one count: the object is destroyed at its last PHP owner's unset, not
/// before and not at the request's end; anything else is handed back and
/// given up, a counted string freed with it.
#[test]
fn an_owner_of_an_object_value_is_recast_with_its_count() {
    let engine = Engine::cli_builder().extensions(&[owners()]).build();
    let served = engine.serve_capture(
        &format!("<?php {COUNTED} class D {{ function __destruct() {{ echo \"d\\n\"; }} }}\n$o = as_object(new D);\necho get_class($o), \"\\n\";\nunset($o);\necho 'end', \"\\n\";\nvar_dump(as_object(1), as_object(s('x')));\n"),
        Capture::default(),
    );
    served.assert_clean("D\nd\nend\nNULL\nNULL\n");
}

/// A table [`PhpArray::filled`] makes is given up where a step of its
/// fill throws: nothing is reported, and what it held is freed with it,
/// the object's destructor served once the throwable, whose trace holds
/// the argument, is let go.
#[test]
fn a_filled_table_is_given_up_where_its_fill_throws() {
    let engine = Engine::cli_builder().extensions(&[owners()]).build();
    let served = engine.serve_capture(
        "<?php class D { function __destruct() { echo \"d\\n\"; } }\ntry { filled_past_a_throw(new D); } catch (Error $e) { echo $e->getMessage(), \"\\n\"; unset($e); }\necho 'end', \"\\n\";\n",
        Capture::default(),
    );
    served.assert_clean("thrown\nd\nend\n");
}
