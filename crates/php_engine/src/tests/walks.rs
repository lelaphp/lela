//! The callback walks as a native meets them (#1980 candidate 3): what a
//! walk took is given back on every way out, past the last row, at a
//! row the body broke at and at one it threw from through `?`, so the
//! table's owners after the walk are the ones it had before it, and a
//! guarded walk leaves the table unmarked.

use std::ops::ControlFlow;

use php_macros::php_function;

use crate::api::abi::{Extension, ExtensionBuilder, Ref};
use crate::api::raising::{Thrown, throw_error};
use crate::api::request::Request;
use crate::api::values::walk::{self, Guarded};
use crate::api::values::{Key, PhpArray, Value};
use crate::engine::Engine;
use crate::test::Capture;

/// The value the by-reference argument `array` names, the reference
/// itself where the call passed one.
fn reached(array: &Ref<PhpArray>) -> Value {
    array.reference().map_or_else(
        || array.get().copied().map_or_else(Value::null, Value::array),
        |cell| cell.as_value(),
    )
}

/// `owners(array &$array): int`: the owners of the array the variable
/// holds.
#[php_function]
fn owners(array: Ref<PhpArray>) -> i64 {
    array
        .get()
        .map_or(0, |table| i64::from(table.to_value().refcount()))
}

/// `held_rows(array &$array, int $stop): int`: the rows a held walk
/// visits, or a hundred times `$stop` where the walk breaks at the row
/// keyed `$stop`.
#[php_function]
fn held_rows(req: &mut Request, array: Ref<PhpArray>, stop: i64) -> Result<i64, Thrown> {
    let mut visited = 0;
    let broke = walk::held(req, &reached(&array), |_, key, _| {
        visited += 1;
        Ok(if *key == Key::Int(stop) {
            ControlFlow::Break(stop * 100)
        } else {
            ControlFlow::Continue(())
        })
    })?;
    Ok(broke.unwrap_or(visited))
}

/// `held_throws(array &$array): void`: a held walk whose body throws at
/// its first row, handed on with `?`.
#[php_function]
fn held_throws(req: &mut Request, array: Ref<PhpArray>) -> Result<(), Thrown> {
    walk::held(req, &reached(&array), |req, _, _| -> walk::Step {
        Err(throw_error(req, "Exception", "thrown at the first row"))
    })?;
    Ok(())
}

/// `guarded_throws(array &$array): bool`: a guarded walk whose body
/// throws, after asking whether a second guarded walk over the same
/// table is inside it already, which it is.
#[php_function]
fn guarded_throws(req: &mut Request, array: Ref<PhpArray>) -> Result<(), Thrown> {
    let walked = walk::held_guarded(req, &reached(&array), |req, _, _| -> walk::Step {
        let inner = walk::held_guarded(req, &reached(&array), |_, _, _| -> walk::Step {
            Ok(ControlFlow::Continue(()))
        })?;
        let said = if inner == Guarded::Recursive {
            "inside"
        } else {
            "not inside"
        };
        Err(throw_error(req, "Exception", said))
    })?;
    let Guarded::Walked(_) = walked else {
        return Err(throw_error(req, "Exception", "recursive at the start"));
    };
    Ok(())
}

/// `guarded(array &$array): bool`: whether a guarded walk finds the
/// table marked.
#[php_function]
fn guarded(req: &mut Request, array: Ref<PhpArray>) -> Result<bool, Thrown> {
    let walked = walk::held_guarded(req, &reached(&array), |_, _, _| -> walk::Step {
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(walked == Guarded::Recursive)
}

fn walks() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            owners::declared(),
            held_rows::declared(),
            held_throws::declared(),
            guarded_throws::declared(),
            guarded::declared(),
        ])
        .build("walks")
}

/// A held walk over a table reached through a reference holds one owner
/// of it, and gives it back past the last row, at the row the body broke
/// at and at the row it threw from, so the variable's array has the one
/// owner it had before each walk; a debug build would also raise an
/// owner the walk dropped. The throwable is let go before the count is
/// read, since its trace holds the argument. A guarded walk marks the table while it runs,
/// so a second one inside it walks nothing, and unmarks it when the body
/// throws.
#[test]
fn a_held_walk_gives_back_what_it_took_on_every_way_out() {
    let engine = Engine::cli_builder().extensions(&[walks()]).build();
    let served = engine.serve_capture(
        "<?php function s($c) { return $c . PHP_INT_MAX; }\n\
         $a = [s('x'), s('y'), s('z')];\n\
         echo owners($a), ' ', held_rows($a, 9), ' ', held_rows($a, 1), ' ', owners($a), \"\\n\";\n\
         try { held_throws($a); } catch (Exception $e) { echo $e->getMessage(); }\n\
         unset($e); echo ' ', owners($a), \"\\n\";\n\
         try { guarded_throws($a); } catch (Exception $e) { echo $e->getMessage(); }\n\
         unset($e); echo ' ', owners($a), \"\\n\";\n\
         echo guarded($a) ? 'recursive' : 'walked', \"\\n\";\n",
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "1 3 100 1\nthrown at the first row 1\ninside 1\nwalked\n"
    );
    assert_eq!(served.exit_code, 0);
}
