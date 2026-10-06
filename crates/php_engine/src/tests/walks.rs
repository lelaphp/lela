//! The callback walks as a native meets them (#1980 candidate 3): what a
//! walk took is given back on every way out, past the last row, at a
//! row the body broke at and at one it threw from through `?`, so the
//! table is freed where the program lets it go, and a guarded walk
//! leaves the table unmarked; a live walk's row stands for its step
//! whatever the step's code does to the table.

use std::ops::ControlFlow;

use php_macros::php_function;

use crate::api::abi::{Callable, Extension, ExtensionBuilder, Ref};
use crate::api::call::call;
use crate::api::raising::{Thrown, throw_error};
use crate::api::request::Request;
use crate::api::values::walk::{self, Guarded, Reached};
use crate::api::values::{Key, Owner, PhpArray, PhpString, Value};
use crate::engine::Engine;
use crate::test::Capture;

/// How the by-reference argument `array` reaches its table: through
/// the reference where the call passed one.
fn reached(req: &mut Request, array: &Ref<PhpArray>) -> Result<Reached, Thrown> {
    let Some(table) = array.get().copied() else {
        return Err(throw_error(req, "Exception", "no array"));
    };
    Ok(array
        .reference()
        .map_or(Reached::Held(table), |cell| Reached::Through(cell, table)))
}

/// `held_rows(array &$array, int $stop): int`: the rows a held walk
/// visits, or a hundred times `$stop` where the walk breaks at the row
/// keyed `$stop`.
#[php_function]
fn held_rows(req: &mut Request, array: Ref<PhpArray>, stop: i64) -> Result<i64, Thrown> {
    let mut visited = 0;
    let reached = reached(req, &array)?;
    let broke = walk::held(req, reached, |_, key, _| {
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
    let reached = reached(req, &array)?;
    walk::held(req, reached, |req, _, _| -> walk::Step {
        Err(throw_error(req, "Exception", "thrown at the first row"))
    })?;
    Ok(())
}

/// `guarded_throws(array &$array): void`: a guarded walk whose body
/// throws, after asking whether a second guarded walk over the same
/// table is inside it already, which it is.
#[php_function]
fn guarded_throws(req: &mut Request, array: Ref<PhpArray>) -> Result<(), Thrown> {
    let reached = reached(req, &array)?;
    let walked = walk::held_guarded(req, reached, |req, _, _| -> walk::Step {
        let inner = walk::held_guarded(req, reached, |_, _, _| -> walk::Step {
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
    let reached = reached(req, &array)?;
    let walked = walk::held_guarded(req, reached, |_, _, _| -> walk::Step {
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(walked == Guarded::Recursive)
}

/// `live_reads(array &$array, callable $f): string`: a live walk that
/// calls `$f` with each row's key and then reads the row's value, which
/// `$f` may have unset.
#[php_function]
fn live_reads(
    req: &mut Request,
    array: Ref<PhpArray>,
    f: Callable,
) -> Result<Owner<PhpString>, Thrown> {
    let Some(holder) = array.reference() else {
        return Err(throw_error(req, "Exception", "no reference"));
    };
    let mut read = String::new();
    walk::live(req, holder, |req, row| -> walk::Step {
        let key = match row.key() {
            Key::Int(n) => Value::int(*n),
            Key::Str(s) => Value::string(*s),
        };
        let answer = call(req, &f, &[key])?;
        answer.give_up(req);
        let bytes = row.value().as_str_bytes().unwrap_or_default();
        read.push_str(&String::from_utf8_lossy(bytes));
        read.push(' ');
        Ok(ControlFlow::Continue(()))
    })?;
    PhpString::new(req, read.as_bytes())
}

fn walks() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            held_rows::declared(),
            held_throws::declared(),
            guarded_throws::declared(),
            guarded::declared(),
            live_reads::declared(),
        ])
        .build("walks")
}

/// A held walk over a table reached through a reference holds one owner
/// of it, and gives it back past the last row, at the row the body broke
/// at and at the row it threw from, so the array is freed, and the
/// object in it destroyed, where the program lets the variable go; a
/// debug build would also raise an owner the walk dropped. The
/// throwable is let go first, since its trace holds the argument. A
/// guarded walk marks the table while it runs, so a second one inside
/// it walks nothing, and unmarks it when the body throws.
#[test]
fn a_held_walk_gives_back_what_it_took_on_every_way_out() {
    let engine = Engine::cli_builder().extensions(&[walks()]).build();
    let served = engine.serve_capture(
        "<?php class D { function __destruct() { echo 'gone '; } }\n\
         function s($c) { return $c . PHP_INT_MAX; }\n\
         $a = [new D, s('y')];\n\
         echo held_rows($a, 9), ' ', held_rows($a, 1), \"\\n\"; $a = null; echo \"after\\n\";\n\
         $a = [new D];\n\
         try { held_throws($a); } catch (Exception $e) { echo $e->getMessage(), ' '; }\n\
         unset($e); $a = null; echo \"after\\n\";\n\
         $a = [new D];\n\
         try { guarded_throws($a); } catch (Exception $e) { echo $e->getMessage(), ' '; }\n\
         unset($e); echo guarded($a) ? 'recursive ' : 'walked '; $a = null; echo \"after\\n\";\n",
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "2 100\ngone after\nthrown at the first row gone after\ninside walked gone after\n"
    );
    assert_eq!(served.exit_code, 0);
}

/// A live walk owns each row's key and value for its step: a callback
/// that unsets the row and allocates a string of its size leaves the
/// native reading the value the row held, not the slot the string took.
#[test]
fn a_live_walk_s_row_stands_after_the_callback_unsets_it() {
    let engine = Engine::cli_builder().extensions(&[walks()]).build();
    let served = engine.serve_capture(
        "<?php function s($c) { return $c . PHP_INT_MAX; }\n\
         $a = ['k' => s('v'), 'l' => s('w')];\n\
         echo live_reads($a, function ($k) { global $a; unset($a[$k]); $GLOBALS['x' . $k] = s('z'); }), $a === [] ? 'emptied' : 'not emptied', \"\\n\";\n",
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "v9223372036854775807 w9223372036854775807 emptied\n"
    );
    assert_eq!(served.exit_code, 0);
}
