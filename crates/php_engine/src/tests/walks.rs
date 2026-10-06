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
use crate::api::values::walk::{self, Guarded, Hold};
use crate::api::values::{Key, Owner, PhpArray, PhpString, Value};
use crate::engine::{Engine, LeakSensor};
use crate::test::Capture;

/// The table the by-reference argument `array` holds.
fn table(req: &mut Request, array: &Ref<PhpArray>) -> Result<PhpArray, Thrown> {
    array
        .get()
        .copied()
        .ok_or_else(|| throw_error(req, "Exception", "no array"))
}

/// The hold the test natives' `$hold` names.
fn hold(owner: bool) -> Hold {
    if owner { Hold::Owner } else { Hold::None }
}

/// `foreach_rows(array &$array, int $stop, bool $hold): int`: the rows a
/// foreach walk visits, or a hundred times `$stop` where the walk breaks
/// at the row keyed `$stop`.
#[php_function]
fn foreach_rows(
    req: &mut Request,
    array: Ref<PhpArray>,
    stop: i64,
    owner: bool,
) -> Result<i64, Thrown> {
    let mut visited = 0;
    let table = table(req, &array)?;
    let broke = walk::foreach(req, table, hold(owner), |_, row| {
        visited += 1;
        Ok(if *row.key() == Key::Int(stop) {
            ControlFlow::Break(stop * 100)
        } else {
            ControlFlow::Continue(())
        })
    })?;
    Ok(broke.unwrap_or(visited))
}

/// `foreach_throws(array &$array, bool $hold): void`: a foreach walk
/// whose body throws at its first row, handed on with `?`.
#[php_function]
fn foreach_throws(req: &mut Request, array: Ref<PhpArray>, owner: bool) -> Result<(), Thrown> {
    let table = table(req, &array)?;
    walk::foreach(req, table, hold(owner), |req, _| -> walk::Step {
        Err(throw_error(req, "Exception", "thrown at the first row"))
    })?;
    Ok(())
}

/// `guarded_throws(array &$array): void`: a guarded walk whose body
/// throws, after asking whether a second guarded walk over the same
/// table is inside it already, which it is.
#[php_function]
fn guarded_throws(req: &mut Request, array: Ref<PhpArray>) -> Result<(), Thrown> {
    let table = table(req, &array)?;
    let walked = walk::foreach_guarded(req, table, Hold::Owner, |req, _| -> walk::Step {
        let inner = walk::foreach_guarded(req, table, Hold::Owner, |_, _| -> walk::Step {
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
    let table = table(req, &array)?;
    let walked = walk::foreach_guarded(req, table, Hold::Owner, |_, _| -> walk::Step {
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(walked == Guarded::Recursive)
}

/// `foreach_calls(array &$array, callable $f, bool $hold): string`: a
/// guarded foreach walk that calls `$f` with each row's key and then
/// reads the row's value, which `$f` may have unset, or freed with the
/// whole table.
#[php_function]
fn foreach_calls(
    req: &mut Request,
    array: Ref<PhpArray>,
    f: Callable,
    owner: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let table = table(req, &array)?;
    let mut read = String::new();
    let walked = walk::foreach_guarded(req, table, hold(owner), |req, row| -> walk::Step {
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
    if walked == Guarded::Recursive {
        return Err(throw_error(req, "Exception", "recursive at the start"));
    }
    PhpString::new(req, read.as_bytes())
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
            foreach_rows::declared(),
            foreach_throws::declared(),
            guarded_throws::declared(),
            guarded::declared(),
            foreach_calls::declared(),
            live_reads::declared(),
        ])
        .build("walks")
}

/// An engine of the test natives with the leak sensor armed, so a walk
/// that kept an owner, or gave up one it never took, fails the request.
fn armed() -> Engine {
    Engine::cli_builder()
        .extensions(&[walks()])
        .sense_leaks(LeakSensor::Armed)
        .build()
}

/// A foreach walk gives back what it took past the last row, at the row
/// the body broke at and at the row it threw from, holding the table or
/// not, so the array is freed, and the object in it destroyed, where the
/// program lets the variable go; a debug build would also raise an
/// owner the walk dropped, and the armed sensor an owner it kept. The
/// throwable is let go first, since its trace holds the argument. A
/// guarded walk marks the table while it runs, so a second one inside
/// it walks nothing, and unmarks it when the body throws.
#[test]
fn a_foreach_walk_gives_back_what_it_took_on_every_way_out() {
    let served = armed().serve_capture(
        "<?php class D { function __destruct() { echo 'gone '; } }\n\
         function s($c) { return $c . PHP_INT_MAX; }\n\
         $a = [new D, s('y')];\n\
         echo foreach_rows($a, 9, true), ' ', foreach_rows($a, 1, true), ' ', foreach_rows($a, 9, false), ' ', foreach_rows($a, 1, false), \"\\n\"; $a = null; echo \"after\\n\";\n\
         $a = [new D];\n\
         try { foreach_throws($a, true); } catch (Exception $e) { echo $e->getMessage(), ' '; }\n\
         try { foreach_throws($a, false); } catch (Exception $e) { echo $e->getMessage(), ' '; }\n\
         unset($e); $a = null; echo \"after\\n\";\n\
         $a = [new D];\n\
         try { guarded_throws($a); } catch (Exception $e) { echo $e->getMessage(), ' '; }\n\
         unset($e); echo guarded($a) ? 'recursive ' : 'walked '; $a = null; echo \"after\\n\";\n",
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "2 100 2 100\ngone after\nthrown at the first row thrown at the first row gone after\ninside walked gone after\n"
    );
    assert_eq!(served.fatal, None);
    assert_eq!(served.exit_code, 0);
}

/// A foreach walk reads the rows as they stand under [`Hold::None`]: a
/// row the callback unsets ahead is not visited and one it rewrites in
/// place is read as it now is, while under [`Hold::Owner`] the write
/// separates the table and the walk reads the rows it began with. Each
/// step's row is the walk's own, so the native reads the row it was
/// lent after the callback unsets it.
#[test]
fn a_foreach_walk_reads_the_rows_as_they_stand_where_it_holds_nothing() {
    let script = |owner: &str| {
        format!(
            "<?php function s($c) {{ return $c . PHP_INT_MAX; }}\n\
             $a = ['k' => s('v'), 'l' => s('w'), 'm' => s('x')];\n\
             echo foreach_calls($a, function ($k) {{ global $a; if ($k === 'k') {{ unset($a['k'], $a['l']); $a['m'] = 'new'; $GLOBALS['y'] = s('z'); }} }}, {owner}), \"\\n\";\n"
        )
    };
    let served = armed().serve_capture(&script("false"), Capture::default());
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "v9223372036854775807 new \n"
    );
    assert_eq!(served.fatal, None);
    let served = armed().serve_capture(&script("true"), Capture::default());
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "v9223372036854775807 w9223372036854775807 x9223372036854775807 \n"
    );
    assert_eq!(served.fatal, None);
}

/// A foreach walk holding nothing ends at the table's destruction: the
/// callback assigns over the variable that held the table's only owner,
/// which frees it. The walk reads the row it was lent, which it pins,
/// and no row after it. Holding the table, the walk reads every row the
/// table had.
#[test]
fn a_foreach_walk_holding_nothing_ends_where_the_table_is_destroyed() {
    let script = |owner: &str| {
        format!(
            "<?php function s($c) {{ return $c . PHP_INT_MAX; }}\n\
             $a = ['k' => s('v'), 'l' => s('w'), 'm' => s('x')];\n\
             echo foreach_calls($a, function ($k) {{ $GLOBALS['a'] = 0; $GLOBALS['y'] = s('z'); $GLOBALS['z'] = s('q'); }}, {owner}), \"\\n\";\n"
        )
    };
    let served = armed().serve_capture(&script("false"), Capture::default());
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "v9223372036854775807 \n"
    );
    assert_eq!(served.fatal, None);
    let served = armed().serve_capture(&script("true"), Capture::default());
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "v9223372036854775807 w9223372036854775807 x9223372036854775807 \n"
    );
    assert_eq!(served.fatal, None);
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
