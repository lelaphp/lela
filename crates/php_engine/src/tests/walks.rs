//! The callback walks as a native meets them (#1980 candidate 3): what a
//! walk took is given back on every way out, past the last row, at a
//! row the body broke at and at one it threw from through `?`, so the
//! table is freed where the program lets it go, and a guarded walk
//! leaves the table unmarked; a live walk's row stands for its step
//! whatever the step's code does to the table, and so does a pinned
//! row of a foreach walk holding nothing, destroyed where the program
//! lets it go and its bytes kept to the step's end.

use std::ops::ControlFlow;

use php_macros::php_function;

use crate::api::abi::{Callable, Extension, ExtensionBuilder, Ref};
use crate::api::call::call;
use crate::api::raising::{Thrown, throw_error};
use crate::api::request::Request;
use crate::api::values::walk::{self, GiveUp, Guarded, Hold};
use crate::api::values::{Key, Owner, PhpArray, PhpObject, PhpString, Value};
use crate::engine::{Engine, LeakSensor};
use crate::test::Capture;

/// The table the by-reference argument `array` holds.
fn table(req: &mut Request, array: &Ref<PhpArray>) -> Result<PhpArray, Thrown> {
    array
        .get()
        .copied()
        .ok_or_else(|| throw_error(req, "Exception", "no array"))
}

/// The hold the test natives' `$owner` names: [`Hold::Owner`] where true.
fn hold(owner: bool) -> Hold {
    if owner {
        Hold::Owner(GiveUp::Dtor)
    } else {
        Hold::None
    }
}

/// `foreach_rows(array &$array, int $stop, bool $owner): int`: the rows a
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

/// `foreach_throws(array &$array, bool $owner): void`: a foreach walk
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
    let walked = walk::foreach_guarded(
        req,
        table,
        Hold::Owner(GiveUp::Dtor),
        |req, _| -> walk::Step {
            let inner = walk::foreach_guarded(
                req,
                table,
                Hold::Owner(GiveUp::Dtor),
                |_, _| -> walk::Step { Ok(ControlFlow::Continue(())) },
            )?;
            let said = if inner == Guarded::Recursive {
                "inside"
            } else {
                "not inside"
            };
            Err(throw_error(req, "Exception", said))
        },
    )?;
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
    let walked = walk::foreach_guarded(
        req,
        table,
        Hold::Owner(GiveUp::Dtor),
        |_, _| -> walk::Step { Ok(ControlFlow::Continue(())) },
    )?;
    Ok(walked == Guarded::Recursive)
}

/// `foreach_calls(array &$array, callable $f, bool $owner): string`: a
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
    let walked = walk::foreach_guarded(req, table, hold(owner), |req, row| {
        call_then_read(req, &f, row, &mut read)
    })?;
    if walked == Guarded::Recursive {
        return Err(throw_error(req, "Exception", "recursive at the start"));
    }
    PhpString::new(req, read.as_bytes())
}

/// `$f` called with `row`'s key, then the row's value, as the walk
/// reads it after the call, put in `read` before a space.
fn call_then_read(
    req: &mut Request,
    f: &Callable,
    row: &walk::Row,
    read: &mut String,
) -> walk::Step {
    let key = match row.key() {
        Key::Int(n) => Value::int(*n),
        Key::Str(s) => Value::string(*s),
    };
    let answer = call(req, f, &[key])?;
    answer.give_up(req);
    let bytes = row.value().as_str_bytes().unwrap_or_default();
    read.push_str(&String::from_utf8_lossy(bytes));
    read.push(' ');
    Ok(ControlFlow::Continue(()))
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
    walk::live(req, holder, |req, row| {
        call_then_read(req, &f, row, &mut read)
    })?;
    PhpString::new(req, read.as_bytes())
}

/// `foreach_takes(array &$array, callable $f): void`: a foreach walk
/// holding nothing that calls `$f` with each row's key, then takes an
/// owner of the row's value, prints `read` and what it holds, an object
/// as `object`, and gives the owner up.
#[php_function]
fn foreach_takes(req: &mut Request, array: Ref<PhpArray>, f: Callable) -> Result<(), Thrown> {
    let table = table(req, &array)?;
    walk::foreach(req, table, Hold::None, |req, row| -> walk::Step {
        let key = match row.key() {
            Key::Int(n) => Value::int(*n),
            Key::Str(s) => Value::string(*s),
        };
        let answer = call(req, &f, &[key])?;
        answer.give_up(req);
        let value = Owner::take(req, *row.value());
        let shown = match value.as_object() {
            Some(_) => "object".to_owned(),
            None => String::from_utf8_lossy(value.as_str_bytes().unwrap_or_default()).into_owned(),
        };
        req.response_mut()
            .write(format!("read {shown} ").as_bytes());
        value.give_up(req);
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// `object_rows(object &$o, callable $f, bool $owner): string`: a
/// foreach walk over the object's property table that calls `$f` with
/// each row's key and then reads the row's value, which `$f` may have
/// unset, or freed with the whole object; each row as `key=value`, a
/// `\0` of a mangled key spelt `\0`.
#[php_function]
fn object_rows(
    req: &mut Request,
    o: Ref<PhpObject>,
    f: Callable,
    owner: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let Some(object) = o.get().copied() else {
        return Err(throw_error(req, "Exception", "no object"));
    };
    let table = object.property_table(req)?;
    let mut read = String::new();
    walk::foreach(req, table, hold(owner), |req, row| -> walk::Step {
        let key = match row.key() {
            Key::Int(n) => Value::int(*n),
            Key::Str(s) => Value::string(*s),
        };
        let answer = call(req, &f, &[key])?;
        answer.give_up(req);
        let key = match row.key() {
            Key::Int(n) => n.to_string(),
            Key::Str(s) => String::from_utf8_lossy(s.as_bytes()).replace('\0', "\\0"),
        };
        let bytes = row.value().as_str_bytes().unwrap_or_default();
        read.push_str(&key);
        read.push('=');
        read.push_str(&String::from_utf8_lossy(bytes));
        read.push(' ');
        Ok(ControlFlow::Continue(()))
    })?;
    PhpString::new(req, read.as_bytes())
}

/// `object_guarded(object &$o): string`: a guarded walk over the
/// object's property table that asks, at its first row, whether a second
/// guarded walk over the same table is inside it already.
#[php_function]
fn object_guarded(req: &mut Request, o: Ref<PhpObject>) -> Result<Owner<PhpString>, Thrown> {
    let Some(object) = o.get().copied() else {
        return Err(throw_error(req, "Exception", "no object"));
    };
    let table = object.property_table(req)?;
    let inner = table.clone();
    let walked = walk::foreach_guarded(
        req,
        table,
        Hold::Owner(GiveUp::Dtor),
        |req, _| -> walk::Step<bool> {
            let again = walk::foreach_guarded(
                req,
                inner.clone(),
                Hold::Owner(GiveUp::Dtor),
                |_, _| -> walk::Step { Ok(ControlFlow::Continue(())) },
            )?;
            Ok(ControlFlow::Break(again == Guarded::Recursive))
        },
    )?;
    let said = match walked {
        Guarded::Walked(Some(true)) => "inside",
        Guarded::Walked(_) => "not inside",
        Guarded::Recursive => "recursive at the start",
    };
    PhpString::new(req, said.as_bytes())
}

fn walks() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            foreach_rows::declared(),
            foreach_throws::declared(),
            guarded_throws::declared(),
            guarded::declared(),
            foreach_calls::declared(),
            foreach_takes::declared(),
            live_reads::declared(),
            object_rows::declared(),
            object_guarded::declared(),
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
    served.assert_clean("2 100 2 100\ngone after\nthrown at the first row thrown at the first row gone after\ninside walked gone after\n");
}

/// A foreach walk reads the rows as they stand under [`Hold::None`]: a
/// row the callback unsets ahead is not visited and one it rewrites in
/// place is read as it now is, while under [`Hold::Owner`] the write
/// separates the table and the walk reads the rows it began with. Each
/// step's row is kept for the step, pinned or held with the table, so
/// the native reads the row it was lent after the callback unsets it.
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
    served.assert_clean("v9223372036854775807 new \n");
    let served = armed().serve_capture(&script("true"), Capture::default());
    served.assert_clean("v9223372036854775807 w9223372036854775807 x9223372036854775807 \n");
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
    served.assert_clean("v9223372036854775807 \n");
    let served = armed().serve_capture(&script("true"), Capture::default());
    served.assert_clean("v9223372036854775807 w9223372036854775807 x9223372036854775807 \n");
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
    served.assert_clean("v9223372036854775807 w9223372036854775807 emptied\n");
}

/// A foreach walk over an object's property table reads php's
/// positions: the declared slots in slot order, each keyed by its
/// mangled name, a typed one nothing has written stepped over, then the
/// dynamic rows.
#[test]
fn a_foreach_walk_reads_an_object_s_declared_rows_then_its_dynamic_ones() {
    let served = armed().serve_capture(
        "<?php #[AllowDynamicProperties] class P { private $a = 'a'; protected $b = 'b'; public int $u; public $c = 'c'; }\n\
         $o = new P; $o->d = 'd';\n\
         echo object_rows($o, function () {}, true), \"\\n\";\n",
        Capture::default(),
    );
    served.assert_clean("\\0P\\0a=a \\0*\\0b=b c=c d=d \n");
}

/// Holding the object's table, a foreach walk reads the dynamic rows it
/// began with when the callback writes them, as php's walk over the
/// table `zend_get_properties_for` holds reads them once the write
/// separated it, while a declared row is read in its slot as it stands:
/// one the callback unsets ahead is not visited. Holding nothing, the
/// walk reads the dynamic rows as they stand too, and a row appended
/// into the place the unset last row gave back, php's `nNumUsed` drawn
/// in by `zend_hash_del`, is below the end the walk fixed and visited.
#[test]
fn a_foreach_walk_over_an_object_reads_its_slots_as_they_stand() {
    let script = |owner: &str| {
        format!(
            "<?php function s($c) {{ return $c . PHP_INT_MAX; }}\n\
             #[AllowDynamicProperties] class P {{ public $a = 'a'; public $b = 'b'; }}\n\
             $o = new P; $o->d = s('d'); $o->e = s('e');\n\
             echo object_rows($o, function ($k) {{ global $o; if ($k === 'a') {{ unset($o->b, $o->e); $o->d = 'new'; $o->f = 'f'; $GLOBALS['y'] = s('z'); }} }}, {owner}), \"\\n\";\n"
        )
    };
    let served = armed().serve_capture(&script("true"), Capture::default());
    served.assert_clean("a=a d=d9223372036854775807 e=e9223372036854775807 \n");
    let served = armed().serve_capture(&script("false"), Capture::default());
    served.assert_clean("a=a d=new f=f \n");
}

/// A foreach walk over an object's table holding nothing ends at the
/// table's destruction. Holding the table, the walk reads its dynamic
/// rows to the end, the object kept from reuse until the walk ends.
#[test]
fn a_foreach_walk_over_an_object_holding_nothing_ends_where_it_is_destroyed() {
    let script = |owner: &str| {
        format!(
            "<?php function s($c) {{ return $c . PHP_INT_MAX; }}\n\
             #[AllowDynamicProperties] class P {{ public $a; public $b; function __destruct() {{ echo 'gone '; }} }}\n\
             $o = new P; $o->a = s('a'); $o->b = s('b'); $o->d = s('d'); $o->e = s('e');\n\
             echo object_rows($o, function ($k) {{ if ($k === 'a') {{ $GLOBALS['o'] = 0; $GLOBALS['y'] = s('z'); $GLOBALS['z'] = s('q'); }} }}, {owner}), \"\\n\";\n"
        )
    };
    let served = armed().serve_capture(&script("false"), Capture::default());
    served.assert_clean("gone a=a9223372036854775807 \n");
    let served = armed().serve_capture(&script("true"), Capture::default());
    served.assert_clean(
        "gone a=a9223372036854775807 d=d9223372036854775807 e=e9223372036854775807 \n",
    );
}

/// A guarded walk over an object's property table marks its dynamic
/// table, `zobj->properties`, as `json_encode`'s walk over the table
/// `zend_get_properties_for` hands it marks the table: a second guarded
/// walk inside it finds it marked, and the mark is cleared as it ends.
#[test]
fn a_guarded_walk_over_an_object_marks_its_table() {
    let served = armed().serve_capture(
        "<?php #[AllowDynamicProperties] class P { public $a = 1; }\n\
         $o = new P; $o->d = 2;\n\
         echo object_guarded($o), ' ', object_guarded($o), \"\\n\";\n",
        Capture::default(),
    );
    served.assert_clean("inside inside\n");
}

/// A row a foreach walk holding nothing lends is pinned for its step, so
/// one the callback frees by unsetting it is destroyed then, and only
/// its bytes wait for the step's end; nothing is freed twice and the
/// leak sensor finds nothing left.
#[test]
fn a_row_freed_during_its_own_step_is_destroyed_there_and_its_bytes_wait() {
    let served = armed().serve_capture(
        "<?php class D { function __destruct() { echo 'gone '; } }\n\
         function s($c) { return $c . PHP_INT_MAX; }\n\
         $a = [new D, s('v')];\n\
         foreach_takes($a, function ($k) { global $a; unset($a[$k]); echo 'unset '; $GLOBALS['y'] = s('z'); });\n\
         echo $a === [] ? 'emptied' : 'not emptied', \"\\n\";\n",
        Capture::default(),
    );
    served.assert_clean("gone unset read object unset read v9223372036854775807 emptied\n");
}

/// A row the callback appends to in place, the string's one owner the
/// table's, is not grown where it stands while the walk pins it: the
/// append makes the table a new string, as it would for a shared one,
/// and the native reads the bytes the row held, the old string freed as
/// the step ends. Grown where it stands, the string could move and the
/// pin name the block it left.
#[test]
fn a_pinned_row_appended_to_is_copied_not_moved() {
    let served = armed().serve_capture(
        "<?php function s($c) { return $c . PHP_INT_MAX; }\n\
         $a = ['k' => s('v')];\n\
         echo foreach_calls($a, function ($k) { global $a; $a['k'] .= s('x') . s('x') . s('x') . s('x'); $GLOBALS['y'] = s('z'); }, false), $a['k'] === s('v') . s('x') . s('x') . s('x') . s('x') ? 'appended' : 'not appended', \"\\n\";\n",
        Capture::default(),
    );
    served.assert_clean("v9223372036854775807 appended\n");
}

/// A row pinned by a walk outside a fiber and by a walk inside it that
/// suspends in its step is kept by whichever pin is let go last (#2291):
/// the walk outside ends its step first, the program then gives up the
/// row's last owner and allocates strings of its size, and the fiber's
/// walk, resumed, takes an owner of the row it was lent, the string as
/// it stood, destroyed and its bytes kept for the fiber's pin. With the
/// pin one bit, cleared by the first pin let go, the string was freed at
/// the program's write and the fiber's owner taken of freed memory, the
/// debug build's freed-use guard firing at it.
#[test]
fn a_row_pinned_across_a_fiber_suspend_stands_until_its_last_pin_goes() {
    let served = armed().serve_capture(
        "<?php function s($c) { return $c . PHP_INT_MAX; }\n\
         $a = ['k' => s('v')]; $fiber = null;\n\
         foreach_takes($a, function ($k) { global $fiber; if ($fiber === null) { $fiber = new Fiber(function () { global $a; foreach_takes($a, function ($k) { Fiber::suspend(); }); }); $fiber->start(); echo 'suspended '; } });\n\
         $a['k'] = 'other'; $GLOBALS['y'] = s('z'); $GLOBALS['z'] = s('q');\n\
         $fiber->resume(); echo \"\\n\";\n",
        Capture::default(),
    );
    served.assert_clean("suspended read v9223372036854775807 read v9223372036854775807 \n");
}
