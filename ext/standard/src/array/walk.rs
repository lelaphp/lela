//! The natives of `array.c` that call back through the engine's call
//! seam, `api::call::call`: `array_walk` and `array_walk_recursive`,
//! which hand each element to the callback by reference, `array_map`,
//! `array_filter`, `array_reduce`, and the four searches 8.4 added,
//! `array_find`, `array_find_key`, `array_any` and `array_all`. A
//! callback that throws ends the walk where it threw and the native
//! raises what was thrown, as php's loops stop on `EG(exception)`.

use std::ops::ControlFlow;

use php_ext::abi::{Callable, Passed, Raw, Ref, True, Variadic};
use php_ext::attr::php_function;
use php_ext::call::call;
use php_ext::coerce::{truthy, value_name};
use php_ext::raising::{Thrown, arg_type_error, call_stack_end, call_stack_limit, throw_error};
use php_ext::request::Request;
use php_ext::values::walk::Hold;
use php_ext::values::{Emalloc, Key, Layout, Owner, PhpArray, PhpRef, Value, ValueRef, walk};

use super::constants::{ARRAY_FILTER_USE_BOTH, ARRAY_FILTER_USE_KEY};
use super::copied;
use super::pointer::ArrayOrObject;
use super::sort::key_value;
use crate::throwables::Throwables;

/// `array_walk(array|object &$array, callable $callback, mixed $arg = UNKNOWN): true`,
/// php's `php_array_walk` (`Z/ext/standard/array.c:1438`):
/// each element of the array the reference holds made a reference in
/// place (`ZVAL_MAKE_REF`) and handed to the callback with its key, and
/// `$arg` third where it was given, so a callback that takes the value
/// by reference writes the caller's array. The walk stands at a bucket
/// of the table, a hash iterator the table's writes move, and the
/// variable is read again after every call, since the callback may
/// change it: an element it removed is not visited, one it appended
/// is, and a key it removed and wrote again is visited again at the
/// end, where it now stands.
///
/// ```php
/// $a = ["x" => 1, "y" => 2, 3];
/// var_dump(array_walk($a, function (&$v, $k, $p) { echo "$k "; $v = $p . $v; }, "n"), $a);
/// ```
/// ```expect
/// x y 0 bool(true)
/// array(3) {
///   ["x"]=>
///   string(2) "n1"
///   ["y"]=>
///   string(2) "n2"
///   [0]=>
///   string(2) "n3"
/// }
/// ```
///
/// ```php
/// $a = [1, 2, 3]; array_walk($a, function ($v, $k) use (&$a) { echo $v; if ($k == 0) { unset($a[1]); $a[] = 9; } }); echo "\n";
/// $b = [1]; try { array_walk($b, function () { throw new Exception("stop"); }); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// $c = 5; try { array_walk($c, 'strlen'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 139
/// stop
/// array_walk(): Argument #1 ($array) must be of type array, int given
/// ```
///
/// The visit order under a callback that removes and appends: an
/// element removed ahead, two keys removed and written again, appends
/// that grow the table, and appends that turn a list into a hash and
/// rehash it, which move the walk with the bucket it stood at.
///
/// ```php
/// $a = ['a' => 1, 'b' => 2, 'c' => 3, 'd' => 4];
/// array_walk($a, function ($v, $k) use (&$a) { echo $k; if ($k === 'a') { unset($a['c']); } }); echo "\n";
/// $a = ['a' => 1, 'b' => 2, 'c' => 3];
/// array_walk($a, function ($v, $k) use (&$a) { echo $k; if ($v === 1) { unset($a['a'], $a['b']); $a['a'] = 9; $a['b'] = 8; } }); echo "\n";
/// $a = [1, 2];
/// array_walk($a, function ($v, $k) use (&$a) { echo " $k=$v"; if ($v < 5) { $a[] = $v + 3; } }); echo "\n";
/// $a = [1, 2, 3];
/// array_walk($a, function ($v, $k) use (&$a) { echo " $k"; if ($k === 1) { unset($a[0]); for ($i = 0; $i < 9; $i++) { $a["k$i"] = $i; } } }); echo "\n";
/// ```
/// ```expect
/// abd
/// acab
///  0=1 1=2 2=4 3=5 4=7
///  0 1 2 k0 k1 k2 k3 k4 k5 k6 k7 k8
/// ```
///
/// The declarations of the natives that call back.
///
/// ```php
/// foreach (['array_walk', 'array_map', 'array_filter', 'array_reduce', 'array_find', 'array_all'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function array_walk ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> object|array &$array ]
///     Parameter #1 [ <required> callable $callback ]
///     Parameter #2 [ <optional> mixed $arg = <default> ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function array_map ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> ?callable $callback ]
///     Parameter #1 [ <required> array $array ]
///     Parameter #2 [ <optional> array ...$arrays ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function array_filter ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <optional> ?callable $callback = null ]
///     Parameter #2 [ <optional> int $mode = 0 ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function array_reduce ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <required> callable $callback ]
///     Parameter #2 [ <optional> mixed $initial = null ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function array_find ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <required> callable $callback ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function array_all ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <required> callable $callback ]
///   }
///   - Return [ bool ]
/// }
/// ```
///
/// After each callback the walk reads the variable again: another array
/// there is walked from its own pointer, a copy of its own, and what is
/// no array or object is php's `TypeError` (GH-69068, bug #70713).
///
/// ```php
/// $a = [1, 2, 3]; array_walk($a, function (&$v) { global $a; echo $v, " "; if ($v == 2) { $a = [7, 8]; } $v *= 10; }); var_dump($a);
/// $c = [1, 2, 3]; array_walk($c, function ($v, $k) { global $c; echo " $k=$v"; if ($k === 0) { $d = [5, 6, 7, 8]; next($d); next($d); $c = $d; } }); echo "\n";
/// $b = [1, 2]; try { array_walk($b, function () { global $b; $b = 1; }); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 1 2 7 8 array(2) {
///   [0]=>
///   int(70)
///   [1]=>
///   int(80)
/// }
///  0=1 2=7 3=8
/// Iterated value is no longer an array or object
/// ```
#[php_function]
fn array_walk(
    req: &mut Request,
    array: Ref<ArrayOrObject>,
    callback: Raw<Callable>,
    arg: Passed<Value>,
) -> Result<True, Thrown> {
    walk(req, &array, callback, arg, false)
}

/// `array_walk_recursive(array|object &$array, callable $callback, mixed $arg = UNKNOWN): true`,
/// php's `php_array_walk` with `recursive`
/// (`Z/ext/standard/array.c:1595`): `array_walk`, descending into every
/// element that is an array, separated in place, where php guards the
/// descent against an array that holds itself.
///
/// ```php
/// $a = ["a" => 1, "b" => [2, [3]], "c" => 4];
/// var_dump(array_walk_recursive($a, function (&$v, $k) { echo "$k=$v "; $v *= 10; }), $a);
/// ```
/// ```expect
/// a=1 0=2 0=3 c=4 bool(true)
/// array(3) {
///   ["a"]=>
///   int(10)
///   ["b"]=>
///   array(2) {
///     [0]=>
///     int(20)
///     [1]=>
///     array(1) {
///       [0]=>
///       int(30)
///     }
///   }
///   ["c"]=>
///   int(40)
/// }
/// ```
///
/// ```php
/// $a = [1]; $a[] = &$a; try { array_walk_recursive($a, function ($v) { echo $v; }); } catch (Error $e) { echo "\n", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 11
/// Recursion detected
/// ```
#[php_function]
fn array_walk_recursive(
    req: &mut Request,
    array: Ref<ArrayOrObject>,
    callback: Raw<Callable>,
    arg: Passed<Value>,
) -> Result<True, Thrown> {
    walk(req, &array, callback, arg, true)
}

fn walk(
    req: &mut Request,
    given: &Ref<ArrayOrObject>,
    callback: Raw<Callable>,
    arg: Passed<Value>,
    recursive: bool,
) -> Result<True, Thrown> {
    let target = given.get().map_or_else(Value::null, Raw::value);
    if !matches!(target.as_ref(), ValueRef::Array(_) | ValueRef::Object(_)) {
        let name = value_name(req, &target);
        return Err(arg_type_error(
            req,
            1,
            &[b"be of type array, ".as_slice(), &name, b" given"].concat(),
        ));
    }
    // `Z_PARAM_FUNC` after `Z_PARAM_ARRAY_OR_OBJECT_EX`, in php's order.
    let callback = callback.hold(req)?;
    let walking = Walk {
        callback: &callback,
        userdata: match arg {
            Passed::Given(userdata) => Some(userdata),
            Passed::Omitted => None,
        },
        recursive,
    };
    // A value where a reference was asked for, past the notice: the
    // walk is over a cell of its own, which nobody reads after.
    let (holder, owned) = if let Some(reference) = given.reference() {
        (reference, None)
    } else {
        let target = Owner::take(req, target);
        let cell = PhpRef::new(req, target)?;
        (*cell, Some(cell))
    };
    let walked = walk_level(req, holder, &walking);
    if let Some(cell) = owned {
        cell.give_up(req);
    }
    walked?;
    Ok(True)
}

/// What every level of a walk hands on, php's `php_array_walk_context`
/// with the two arguments `php_array_walk` passes beside it: the
/// callback, the `$arg` it is given third, and whether an element that
/// is an array is descended into.
struct Walk<'a> {
    callback: &'a Callable,
    userdata: Option<Value>,
    recursive: bool,
}

/// One level of `php_array_walk` over what the cell `holder` holds, the
/// Live walk ([`walk::live`]): each row made a reference in place
/// (`ZVAL_MAKE_REF`) and visited, an array element and an object's
/// property alike. Over an object `HASH_OF` is the property table, a
/// lazy object initialised first, each declared row by its mangled
/// name, and a typed property's slot is made a reference the property
/// types (`ZEND_REF_ADD_TYPE_SOURCE`), so a write through the callback's
/// `&$value` reaches the object and is held to the property's type; a
/// declared slot nothing has written is no row. The walk stands at a
/// position of the table by a hash iterator and reads the cell again
/// after every call, so a row the callback removed ahead is not visited,
/// one it added is, one it removed and wrote again is visited again
/// where it now stands, and another table in the cell is walked from
/// its own pointer.
///
/// ```php
/// #[AllowDynamicProperties] class T { public int $n = 1; public array $list = [1, [2]]; protected $p = 3; }
/// $t = new T; $t->dyn = 4;
/// array_walk($t, function (&$v, $k) {
///     echo strtr($k, "\0", "*"), ";";
///     if ($k === "n") { try { $v = "x"; } catch (TypeError $e) { echo $e->getMessage(), ";"; } $v = "7"; }
///     if ($k === "dyn") { $v++; }
/// });
/// echo "\n";
/// array_walk_recursive($t, function (&$v) { if (is_int($v)) { $v *= 10; } });
/// var_dump($t);
/// ```
/// ```expect
/// n;Cannot assign string to reference held by property T::$n of type int;list;***p;dyn;
/// object(T)#1 (4) {
///   ["n"]=>
///   int(70)
///   ["list"]=>
///   array(2) {
///     [0]=>
///     int(10)
///     [1]=>
///     array(1) {
///       [0]=>
///       int(20)
///     }
///   }
///   ["p":protected]=>
///   int(30)
///   ["dyn"]=>
///   int(50)
/// }
/// ```
///
/// The walk holds an owner of each name it has read until it ends, so a
/// dynamic property the callback removes leaves its name to the walk,
/// and it holds the object only while the cell does: the cell is read
/// again after every call, an object the callback let go of is
/// destroyed there, and what the cell holds then is walked, another
/// object from its first property, an array from its own pointer, and
/// anything else php's `TypeError`.
///
/// ```php
/// $o = new stdClass; for ($i = 0; $i < 4; $i++) { $name = "p" . $i; $o->$name = $i; }
/// array_walk($o, function (&$v, $k) use ($o) { echo " $k=$v"; if ($v === 0) { unset($o->p1, $o->p2); $name = "q" . $v; $o->$name = 9; } $v = -$v; }); echo "\n";
/// var_dump($o);
/// class D { public $a = 1; public $b = 2; function __destruct() { echo " dtor"; } }
/// $o = new D;
/// try { array_walk($o, function ($v, $k) { global $o; echo " $k"; $o = null; }); } catch (TypeError $e) { echo " ", $e->getMessage(), "\n"; }
/// $o = new D;
/// array_walk($o, function ($v, $k) { global $o; echo " $k=$v"; if ($v === 1) { $o = new D; $o->a = 7; } }); echo "\n";
/// $o = new stdClass; $o->a = 1; $o->b = 2;
/// array_walk($o, function ($v, $k) { global $o; echo " $k=$v"; if ($k === 'a') { $p = [5, 6, 7]; next($p); $o = $p; } }); echo "\n";
/// ```
/// ```expect
///  p0=0 p3=3 q0=9
/// object(stdClass)#1 (3) {
///   ["p0"]=>
///   int(0)
///   ["p3"]=>
///   int(-3)
///   ["q0"]=>
///   int(-9)
/// }
///  a dtor Iterated value is no longer an array or object
///  a=1 dtor a=7 b=2
///  dtor a=1 1=6 2=7
/// ```
///
/// The holder's table is written in place, as php's `ZVAL_MAKE_REF`
/// writes it, so a copy the callback took of the array shares the
/// references the walk makes after it; a property removed and written
/// again behind the walk, or ahead of it, is visited where it now
/// stands; and an object a callback put in the cell is walked from its
/// table's pointer.
///
/// A callback that throws and leaves the variable holding no array or
/// object meets the walk's reading of it all the same, as php reads it
/// before it tests `EG(exception)`: the `TypeError` is raised with the
/// throwable as its `previous`. A walk a throw ended has given its hash
/// iterator back, so the object can be reset.
///
/// ```php
/// $a = [1, 2]; try { array_walk($a, function () use (&$a) { $a = 1; throw new Exception('mine'); }); } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), ' / ', $e->getPrevious()?->getMessage(), "\n"; }
/// class C { public $a = 1; }
/// $o = new C; try { array_walk($o, function () { throw new Exception('x'); }); } catch (Exception $e) {}
/// (new ReflectionClass($o))->resetAsLazyGhost($o, function () {}); echo "reset\n";
/// ```
/// ```expect
/// TypeError: Iterated value is no longer an array or object / mine
/// reset
/// ```
///
/// ```php
/// $a = [1, 2]; array_walk($a, function (&$v, $k) use (&$a, &$copy) { if ($k == 0) { $copy = $a; } $v = $v * 10; }); echo json_encode($copy), "\n";
/// $o = new stdClass; $o->a = 1; $o->b = 2; $o->c = 3; array_walk($o, function ($v, $k) { global $o; echo " $k"; if ($k === 'c') { unset($o->a); $o->a = 1; } }); echo "\n";
/// $o = new stdClass; $o->a = 1; $o->b = 2; $o->c = 3; array_walk($o, function ($v, $k) { global $o; echo $k; if ($k === 'a') { unset($o->b); $o->b = 22; } }); echo "\n";
/// class P { public $x = 1; public $y = 2; public $z = 3; }
/// $c = [1, 2]; array_walk($c, function ($v, $k) { global $c; echo " $k=$v"; if ($k === 0) { $p = new P; @next($p); $c = $p; } }); echo "\n";
/// class D { public $a = 1; public $b = 2; } $o = new D; array_walk($o, function ($v, $k) { global $o; echo " $k=$v"; if ($v === 1) { $o = null; $o = new D; $o->a = 7; } }); echo "\n";
/// ```
/// ```expect
/// [10,20]
///  a b c a
/// acb
///  0=1 y=2 z=3
///  a=1 a=7 b=2
/// ```
fn walk_level(req: &mut Request, holder: PhpRef, walk: &Walk) -> Result<(), Thrown> {
    walk::live(req, holder, |req, row| {
        let reference = row.make_ref(req)?;
        visit(req, reference, row.key(), walk)?;
        Ok(ControlFlow::<()>::Continue(()))
    })?;
    Ok(())
}

/// One element of `php_array_walk`, made a reference in place: under
/// `recursive` an array it holds separated and walked in its turn,
/// guarded against an array that holds itself, and otherwise the cell
/// handed to the callback with its key, and `$arg` third where it was
/// given. The live walk owns the cell and the key for the step, as
/// php's `Z_ADDREF(ref)` before a descent and its `ZVAL_COPY(&args[0])`
/// and key copy before a call do, so a callback that removes the element
/// leaves both to the call. A descent asks the stack limit first, as
/// php 8.5.12's `php_array_walk` asks it at each level
/// (`Z/ext/standard/array.c:1469`, gh23111):
///
/// ```php
/// $a = [];
/// for ($i = 0; $i < 30000; $i++) { $a = [$a]; }
/// try { array_walk_recursive($a, function ($v) {}); } catch (Throwable $e) { echo $e::class, ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect regime=small-stack
/// Error: Maximum call stack size of %d bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?
/// ```
fn visit(req: &mut Request, reference: PhpRef, key: &Key, walk: &Walk) -> Result<(), Thrown> {
    if walk.recursive && reference.get().as_array().is_some() {
        call_stack_limit(req)?;
        // The floor stands under it, where `-1` sets no limit and php
        // would run to the thread's end.
        call_stack_end(req)?;
        let Some(nested) = reference.separate_array(req)? else {
            return Ok(());
        };
        if nested.is_recursive() {
            let ce = Throwables::of(req)?;
            return Err(throw_error(req, ce.error, "Recursion detected"));
        }
        nested.protect();
        let walked = walk_level(req, reference, walk);
        if reference.get().as_array() == Some(nested) {
            nested.unprotect();
        }
        return walked;
    }
    let mut arguments = vec![reference.as_value(), key_value(key)];
    if let Some(userdata) = walk.userdata {
        arguments.push(userdata);
    }
    let answer = call(req, walk.callback, &arguments)?;
    answer.give_up(req);
    Ok(())
}

/// `array_reduce(array $array, callable $callback, mixed $initial = null):
/// mixed`, php's `PHP_FUNCTION(array_reduce)`
/// (`Z/ext/standard/array.c:6510`): the carry, `$initial` or `null`,
/// handed to the callback with each value in turn and replaced by what
/// it answers, a reference answer unwrapped.
///
/// ```php
/// var_dump(array_reduce([1, 2, 3], fn($c, $v) => $c + $v), array_reduce([1, 2, 3], fn($c, $v) => $c . $v, "x"), array_reduce([], fn($c, $v) => 1, "empty"));
/// try { array_reduce([1], 'nope'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(6)
/// string(4) "x123"
/// string(5) "empty"
/// array_reduce(): Argument #2 ($callback) must be a valid callback, function "nope" not found or invalid function name
/// ```
#[php_function]
fn array_reduce(
    req: &mut Request,
    array: PhpArray,
    callback: Callable,
    #[php(default = null)] initial: Value,
) -> Result<Owner<Value>, Thrown> {
    let mut carry = Some(Owner::take(req, initial));
    // `ZEND_HASH_FOREACH_VAL(htbl, operand)` (`Z/ext/standard/array.c:6546`),
    // no owner of the table taken: the argument holds it while the
    // callback runs. The carry is the body's to take and give back.
    let walked = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let Some(held) = carry.take() else {
            return Ok(ControlFlow::Break(()));
        };
        let answer = call(req, &callback, &[*held, *row.value()]);
        held.give_up(req);
        let mut next = answer?;
        if let Some(reference) = next.as_php_ref() {
            let inner = Owner::take(req, reference.get());
            next.give_up(req);
            next = inner;
        }
        carry = Some(next);
        Ok(ControlFlow::Continue(()))
    });
    match (walked, carry) {
        (Ok(_), Some(carry)) => Ok(carry),
        (Ok(_), None) => Ok(Owner::null()),
        (Err(thrown), carry) => {
            if let Some(carry) = carry {
                carry.give_up(req);
            }
            Err(thrown)
        }
    }
}

/// `array_filter(array $array, ?callable $callback = null, int $mode = 0):
/// array`, php's `PHP_FUNCTION(array_filter)`
/// (`Z/ext/standard/array.c:6581`): the elements the callback answers
/// truthy for, or with no callback the truthy elements, under their
/// keys. `ARRAY_FILTER_USE_KEY` hands the callback the key alone and
/// `ARRAY_FILTER_USE_BOTH` the value and the key.
///
/// ```php
/// var_dump(array_filter([1, 0, "a", "", null, [], "0", 2.5]), array_filter(["a" => 1, "b" => 2, "c" => 3], fn($k) => $k != "b", ARRAY_FILTER_USE_KEY), array_filter([5, 6, 7], fn($v, $k) => $v % 2 && $k, ARRAY_FILTER_USE_BOTH));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   int(1)
///   [2]=>
///   string(1) "a"
///   [7]=>
///   float(2.5)
/// }
/// array(2) {
///   ["a"]=>
///   int(1)
///   ["c"]=>
///   int(3)
/// }
/// array(1) {
///   [2]=>
///   int(7)
/// }
/// ```
///
/// ```php
/// var_dump(ARRAY_FILTER_USE_BOTH, ARRAY_FILTER_USE_KEY);
/// try { array_filter([1], function () { throw new Exception("stop"); }); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// int(2)
/// stop
/// ```
#[php_function]
fn array_filter(
    req: &mut Request,
    array: PhpArray,
    #[php(default = null)] callback: Option<Callable>,
    #[php(default = 0)] mode: i64,
) -> Result<Owner<PhpArray>, Thrown> {
    if array.is_empty() {
        return Ok(Owner::take(req, PhpArray::empty()));
    }
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(array), num_key, string_key,
    // operand)` (`Z/ext/standard/array.c:6620`), no owner of the table
    // taken: the argument holds it while the callback runs. A row kept is
    // taken as `zval_add_ref` takes it.
    let walked = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let operand = *row.value();
        let keep = match &callback {
            Some(callback) => {
                let arguments = match mode {
                    ARRAY_FILTER_USE_KEY => vec![key_value(row.key())],
                    ARRAY_FILTER_USE_BOTH => vec![operand, key_value(row.key())],
                    _ => vec![operand],
                };
                let answer = call(req, callback, &arguments)?;
                let truth = truthy(req, &answer);
                answer.give_up(req);
                truth?
            }
            None => truthy(req, &operand.deref())?,
        };
        if keep {
            let value = copied(req, *row.value());
            let key = Owner::take(req, row.key().clone());
            out.set(req, key, value)?;
        }
        Ok(ControlFlow::Continue(()))
    });
    if let Err(thrown) = walked {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(out)
}

/// The row [`find`] found: an owner of its key and of its value.
type Found = (Owner<Key>, Owner<Value>);

/// Whether [`find`] found a row, its owners given up.
fn given_up(req: &mut Request, found: Option<Found>) -> bool {
    let Some((key, value)) = found else {
        return false;
    };
    value.give_up(req);
    key.give_up(req);
    true
}

/// What `php_array_find` found: the value and the key of the first
/// element the callback answered truthy for (falsy, under `negate`).
fn find(
    req: &mut Request,
    array: PhpArray,
    callback: &Callable,
    negate: bool,
) -> Result<Option<Found>, Thrown> {
    // `ZEND_HASH_FOREACH_KEY_VAL(array, num_key, str_key, operand)`
    // (`Z/ext/standard/array.c:6684`), no owner of the table taken: the
    // argument holds it while the callback runs. The row found is taken an
    // owner of before the walk gives its own back, as php copies it out
    // inside its loop.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step<Found> {
        let answer = call(req, callback, &[*row.value(), key_value(row.key())])?;
        let truth = truthy(req, &answer);
        answer.give_up(req);
        let truth = truth?;
        if truth == negate {
            return Ok(ControlFlow::Continue(()));
        }
        let found = (
            Owner::take(req, row.key().clone()),
            Owner::take(req, *row.value()),
        );
        Ok(ControlFlow::Break(found))
    })
}

/// `array_find(array $array, callable $callback): mixed`, php's
/// `PHP_FUNCTION(array_find)` (`Z/ext/standard/array.c:6722`): the value
/// of the first element the callback answers truthy for, handed the
/// value and the key, or `null`.
///
/// ```php
/// var_dump(array_find([1, 2, 3, 4], fn($v) => $v > 2), array_find(["a" => 1, "b" => 2], fn($v, $k) => $k == "b"), array_find([1], fn() => false));
/// ```
/// ```expect
/// int(3)
/// int(2)
/// NULL
/// ```
#[php_function]
fn array_find(
    req: &mut Request,
    array: PhpArray,
    callback: Callable,
) -> Result<Owner<Value>, Thrown> {
    match find(req, array, &callback, false)? {
        Some((key, value)) => {
            key.give_up(req);
            let inner = Owner::take(req, value.deref());
            value.give_up(req);
            Ok(inner)
        }
        None => Ok(Owner::null()),
    }
}

/// `array_find_key(array $array, callable $callback): mixed`, php's
/// `PHP_FUNCTION(array_find_key)` (`Z/ext/standard/array.c:6738`): the
/// key of that element, or `null`.
///
/// ```php
/// var_dump(array_find_key(["a" => 1, "b" => 5], fn($v) => $v > 2), array_find_key([7, 8], fn($v) => $v == 8), array_find_key([], fn() => true));
/// ```
/// ```expect
/// string(1) "b"
/// int(1)
/// NULL
/// ```
#[php_function]
fn array_find_key(
    req: &mut Request,
    array: PhpArray,
    callback: Callable,
) -> Result<Owner<Value>, Thrown> {
    match find(req, array, &callback, false)? {
        Some((key, value)) => {
            value.give_up(req);
            let shown = Owner::take(req, key_value(&key));
            key.give_up(req);
            Ok(shown)
        }
        None => Ok(Owner::null()),
    }
}

/// `array_any(array $array, callable $callback): bool`, php's
/// `PHP_FUNCTION(array_any)` (`Z/ext/standard/array.c:6754`): whether
/// the callback answers truthy for some element, stopping at the first.
///
/// ```php
/// var_dump(array_any([1, 2, 3], function ($v) { echo $v; return $v == 2; }), array_any([], fn() => true));
/// ```
/// ```expect
/// 12bool(true)
/// bool(false)
/// ```
#[php_function]
fn array_any(req: &mut Request, array: PhpArray, callback: Callable) -> Result<bool, Thrown> {
    let found = find(req, array, &callback, false)?;
    Ok(given_up(req, found))
}

/// `array_all(array $array, callable $callback): bool`, php's
/// `PHP_FUNCTION(array_all)` (`Z/ext/standard/array.c:6770`): whether the
/// callback answers truthy for every element, stopping at the first
/// that is not.
///
/// ```php
/// var_dump(array_all([1, 2, 3], fn($v) => $v > 0), array_all([1, -2, 3], function ($v) { echo $v; return $v > 0; }), array_all([], fn() => false));
/// ```
/// ```expect
/// 1-2bool(true)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn array_all(req: &mut Request, array: PhpArray, callback: Callable) -> Result<bool, Thrown> {
    let found = find(req, array, &callback, true)?;
    Ok(!given_up(req, found))
}

/// `array_map(?callable $callback, array $array, array ...$arrays):
/// array`, php's `PHP_FUNCTION(array_map)` (`Z/ext/standard/array.c:6786`):
/// of one array, the callback's answer for each element under the
/// element's key, or with no callback the array itself; of several,
/// a list of the callback's answers over the elements at the same
/// position of each, `null` where an array has no more, or with no
/// callback a list of those elements as arrays.
///
/// ```php
/// var_dump(array_map(fn($v) => $v * 2, ["a" => 1, "b" => 2]), array_map(null, [1, 2]), array_map(fn($a, $b) => $a . $b, [1, 2, 3], ["x", "y"]), array_map(null, [1, 2], ["a"]));
/// ```
/// ```expect
/// array(2) {
///   ["a"]=>
///   int(2)
///   ["b"]=>
///   int(4)
/// }
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// array(3) {
///   [0]=>
///   string(2) "1x"
///   [1]=>
///   string(2) "2y"
///   [2]=>
///   string(1) "3"
/// }
/// array(2) {
///   [0]=>
///   array(2) {
///     [0]=>
///     int(1)
///     [1]=>
///     string(1) "a"
///   }
///   [1]=>
///   array(2) {
///     [0]=>
///     int(2)
///     [1]=>
///     NULL
///   }
/// }
/// ```
///
/// ```php
/// try { array_map(fn($v) => $v, 5); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { array_map(fn($v) => $v, [1], 5); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { array_map('nope', [1]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array_map(): Argument #2 ($array) must be of type array, int given
/// array_map(): Argument #3 must be of type array, int given
/// array_map(): Argument #1 ($callback) must be a valid callback or null, function "nope" not found or invalid function name
/// ```
///
/// The answer is laid down before the first call, packed at the input's
/// used slots or a hash at its count, and over several arrays sized for
/// the longest beside php's `array_pos` and `params`, so a callback's
/// own allocations peak on top of it as php's do.
///
/// ```php
/// $f = function () { $x = str_repeat('a', 1000); unset($x); return 1; };
/// $h = range(1, 9); unset($h[0]);
/// foreach ([[$f, [1]], [$f, ['k' => 1]], [fn ($v) => $v, $h], [fn ($a, $b) => 1, range(1, 9), [1]], [null, range(1, 9), [1]]] as $c) {
///   array_map(...$c);
///   $s = memory_get_usage(); memory_reset_peak_usage();
///   $r = array_map(...$c);
///   echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n";
///   unset($r);
/// }
/// ```
/// ```expect
/// 1496 248
/// 1656 376
/// 376 376
/// 424 376
/// 2336 2320
/// ```
///
/// Over several arrays the answer's table is laid down at once since
/// 8.5.12, asked for fallibly, and without a callback each tuple is made
/// only while the memory limit has not been met: past it the call returns
/// and the limit's fatal is raised.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// $input = array_fill(0, 250000, 0);
/// var_dump(count(array_map(null, $input, $input)), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 4210688 bytes) in %s on line 4
/// ```
///
/// With a callback, the table refused:
///
/// ```php
/// ini_set('memory_limit', '8M');
/// $a = range(1, 400000);
/// var_dump(count(array_map(fn ($x, $y) => 0, $a, $a)), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 8404992 bytes) in %s on line 3
/// ```
#[php_function]
fn array_map(
    req: &mut Request,
    callback: Option<Callable>,
    array: PhpArray,
    arrays: Variadic<PhpArray>,
) -> Result<Owner<Raw<PhpArray>>, Thrown> {
    let mapped = mapped(req, callback, array, arrays)?;
    let mapped = match mapped {
        Some(mapped) => mapped.into_value(),
        None => Owner::null(),
    };
    Ok(mapped.into_raw())
}

/// `array_map`'s arrays mapped, none where the callback returned nothing
/// (`Z_ISUNDEF(result)`), which php answers with `NULL` once the array it
/// was building is given up: a callback frame phpdbg left at
/// `EG(exception_op)` with nothing pending returns so.
///
///
/// ```php
/// function cb($x) {
///   $q = 1;
///   return 7;
/// }
/// var_dump(array_map("cb", [1]));
/// echo "end\n";
/// ```
/// ```request phpdbg
/// register intdiv
/// b 3
/// r
/// intdiv 1
/// ev 9
/// c
/// q
/// ```
/// ```expect exit=255
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Registered intdiv]
/// prompt> [Breakpoint #0 added at %SCRATCH%/witness.php:3]
/// prompt> [Breakpoint #0 at %SCRATCH%/witness.php:3, hits: 1]
/// >00003:   $q = 1;
///  00004:   return 7;
///  00005: }
/// prompt>%s
/// prompt>%s
/// Fatal error: Uncaught ArgumentCountError: intdiv() expects exactly 2 arguments, 1 given in %SCRATCH%/witness.php:3
/// Stack trace:
/// #0 %SCRATCH%/witness.php(3): intdiv(1)
/// #1 [internal function]: cb(1)
/// #2 %SCRATCH%/witness.php(6): array_map('cb', Array)
/// #3 {main}
///   thrown in %SCRATCH%/witness.php on line 3
/// prompt> NULL
/// end
/// [Script ended normally]
/// prompt>
/// ```
fn mapped(
    req: &mut Request,
    callback: Option<Callable>,
    array: PhpArray,
    arrays: Variadic<PhpArray>,
) -> Result<Option<Owner<PhpArray>>, Thrown> {
    if !arrays.is_empty() {
        let arrays: Vec<PhpArray> = std::iter::once(array).chain(arrays).collect();
        return mapped_together(req, callback.as_ref(), &arrays);
    }
    let Some(callback) = callback else {
        return Ok(Some(Owner::take(req, array)));
    };
    if array.is_empty() {
        return Ok(Some(Owner::take(req, array)));
    }
    // The answer laid down before the first call, as php's
    // `array_init_size` and `zend_hash_real_init_packed` (`_mixed` for a
    // map) are (`Z/ext/standard/array.c:6819`): packed at the input's
    // `nNumUsed`, its holes kept, or a hash at its count.
    let mut out = if array.is_packed() {
        let used = packed_used(req, array);
        PhpArray::sized(req, used, Layout::Packed)?
    } else {
        let len = u32::try_from(array.len()).unwrap_or(u32::MAX);
        PhpArray::sized(req, len, Layout::Hash)?
    };
    // `ZEND_HASH_MAP_FOREACH_KEY_VAL(input, num_key, str_key,
    // fci.params)` (`Z/ext/standard/array.c:6854`), no owner of the table taken:
    // the argument holds it while the callback runs. An answer of
    // nothing, a callback that threw past its frame, ends the walk with
    // no array.
    let walked = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let answer = call(req, &callback, &[*row.value()])?;
        if matches!(answer.as_ref(), ValueRef::Undef) {
            answer.give_up(req);
            return Ok(ControlFlow::Break(()));
        }
        let key = Owner::take(req, row.key().clone());
        out.set(req, key, answer)?;
        Ok(ControlFlow::Continue(()))
    });
    match walked {
        Ok(None) => {}
        Ok(Some(())) => {
            out.give_up(req);
            return Ok(None);
        }
        Err(thrown) => {
            out.give_up(req);
            return Err(thrown);
        }
    }
    Ok(Some(out))
}

/// A packed table's `nNumUsed`, the slot past its last element, which is
/// its key: the count where no key below `nNextFreeElement` is missing,
/// the last key's next where a hole stands.
fn packed_used(req: &Request, array: PhpArray) -> u32 {
    let len = u32::try_from(array.len()).unwrap_or(u32::MAX);
    if array.next_free() == i64::from(len) {
        return len;
    }
    match walk::rows(req, array).last() {
        Some((Key::Int(last), _)) => u32::try_from(last.saturating_add(1)).unwrap_or(len),
        _ => len,
    }
}

/// `array_map` over two arrays or more. A call of its own, so that what
/// it holds is not in the frame a recursion through a one-array
/// `array_map` stands on (#3022).
#[inline(never)]
fn mapped_together(
    req: &mut Request,
    callback: Option<&Callable>,
    arrays: &[PhpArray],
) -> Result<Option<Owner<PhpArray>>, Thrown> {
    // php's `ecalloc`'d `array_pos` and `safe_emalloc`'d `params` stand on
    // the request's heap beside the answer, `array_init_size`d at the
    // longest (`Z/ext/standard/array.c:6871`, `:6880`, `:6929`), whose
    // table its first write lays.
    let longest = arrays.iter().map(PhpArray::len).max().unwrap_or(0);
    let mut positions = vec![0u32; arrays.len()];
    let array_pos = Emalloc::new(req, arrays.len() * 4)?;
    // `array_finit_size_packed` since 8.5.12: the table laid down at
    // once and asked for fallibly, `RETURN_OOM()` where the memory limit
    // refused it.
    let longest_size = u32::try_from(longest).unwrap_or(u32::MAX);
    let laid = req
        .memory_mut()
        .fallible(|req| PhpArray::sized(req, longest_size, Layout::Packed))?;
    let Some(mut out) = laid else {
        array_pos.free(req);
        return Ok(None);
    };
    let params = match callback {
        Some(_) => Some(Emalloc::new(req, arrays.len() * 16)?),
        None => None,
    };
    let mapped = map_together(req, callback, arrays, longest, &mut positions, &mut out);
    if let Some(params) = params {
        params.free(req);
    }
    array_pos.free(req);
    match mapped {
        Ok(true) => Ok(Some(out)),
        Ok(false) => {
            out.give_up(req);
            Ok(None)
        }
        Err(thrown) => {
            out.give_up(req);
            Err(thrown)
        }
    }
}

/// `array_map`'s steps over several arrays into `out`, `false` where the
/// callback answered nothing.
fn map_together(
    req: &mut Request,
    callback: Option<&Callable>,
    arrays: &[PhpArray],
    longest: usize,
    positions: &mut [u32],
    out: &mut Owner<PhpArray>,
) -> Result<bool, Thrown> {
    // The arrays read at once by bucket position, as php reads each at its
    // `array_pos[i]` (`Z/ext/standard/array.c:6889`, `:6933`): the value at the next
    // bucket that holds one as it stands at the step, `null` past the end,
    // no owner of any table taken while the callback runs.
    for _ in 0..longest {
        let row: Vec<Value> = arrays
            .iter()
            .zip(&mut *positions)
            .map(|(array, at)| match array.live_from(*at) {
                Some((found, _, value)) => {
                    *at = found.saturating_add(1);
                    value
                }
                None => Value::null(),
            })
            .collect();
        let element = match callback {
            // A tuple is made only while the memory limit has not been
            // met (8.5.12), else the native returns (`RETURN_OOM()`).
            None if req.memory().has_oom() => return Ok(true),
            None => {
                let n = u32::try_from(arrays.len()).unwrap_or(u32::MAX);
                let mut tuple = PhpArray::init_size(req, n)?;
                for value in row {
                    tuple.append(req, Owner::take(req, value))?;
                }
                tuple.into_value()
            }
            Some(callback) => {
                let answer = call(req, callback, &row)?;
                if matches!(answer.as_ref(), ValueRef::Undef) {
                    answer.give_up(req);
                    return Ok(false);
                }
                answer
            }
        };
        out.append(req, element)?;
    }
    Ok(true)
}
