//! The natives of `array.c` that call back through the engine's call
//! seam, `api::call::call`: `array_walk` and `array_walk_recursive`,
//! which hand each element to the callback by reference, `array_map`,
//! `array_filter`, `array_reduce`, and the four searches 8.4 added,
//! `array_find`, `array_find_key`, `array_any` and `array_all`. A
//! callback that throws ends the walk where it threw and the native
//! raises what was thrown, as php's loops stop on `EG(exception)`.

use std::collections::HashSet;

use php_ext::abi::{Callable, FromArgument, Passed, Raw, Ref, True, Variadic};
use php_ext::attr::php_function;
use php_ext::call::call;
use php_ext::coerce::{truthy, value_name};
use php_ext::raising::{Thrown, arg_type_error, call_stack_end, fatal, throw_error};
use php_ext::request::Request;
use php_ext::values::{HashIterator, Key, PhpArray, PhpObject, PhpRef, Value, ValueRef};

use super::constants::{ARRAY_FILTER_USE_BOTH, ARRAY_FILTER_USE_KEY};
use super::pointer::ArrayOrObject;
use super::sort::key_value;
use super::{copied, key_copy};

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
            &format!("be of type array, {name} given"),
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
    let holder = if let Some(reference) = given.reference() {
        reference
    } else {
        req.addref(&target);
        PhpRef::new(req, target)?
    };
    let walked = walk_holder(req, holder, &walking);
    if given.reference().is_none() {
        req.release(holder.as_value());
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

/// One level of `php_array_walk` over what the cell `holder` holds: an
/// array walked by a hash iterator registered at its first element
/// (`zend_hash_iterator_add`), whatever the array's own pointer says,
/// and given up on every way out (`zend_hash_iterator_del`).
fn walk_holder(req: &mut Request, holder: PhpRef, walk: &Walk) -> Result<(), Thrown> {
    let start = holder.get();
    if let Some(object) = start.as_object().copied() {
        return walk_object(req, object, walk);
    }
    let Some(table) = holder.separate_array(req)? else {
        return Ok(());
    };
    // `zend_hash_internal_pointer_reset_ex`: the first live bucket.
    let Some((first, _, _)) = table.live_from(0) else {
        return Ok(());
    };
    let at = HashIterator::new(req, &table, first);
    let walked = walk_buckets(req, holder, table, at, walk);
    at.release(req);
    walked
}

/// `php_array_walk`'s loop over an array: the element at the bucket the
/// iterator `at` stands at, made a reference in place, the iterator
/// moved past it before the callback runs, and the cell `holder` read
/// again after: the same table goes on from where the table's writes
/// moved the iterator (`zend_hash_iterator_pos_ex`), so an element the
/// callback removed ahead is not visited, one it appended is, and a key
/// it removed and wrote again is visited again where it now stands.
/// Another array is walked from that array's own pointer, separated; an
/// object by its properties; anything else is php's `TypeError`. Nothing
/// of the table is kept across a callback but the iterator.
fn walk_buckets(
    req: &mut Request,
    holder: PhpRef,
    mut table: PhpArray,
    at: HashIterator,
    walk: &Walk,
) -> Result<(), Thrown> {
    let mut pos = at.position(req, &table);
    loop {
        let Some((found, key, value)) = table.live_from(pos) else {
            return Ok(());
        };
        // `ZVAL_MAKE_REF(zv)`: the element a reference in place. php
        // writes the cell into the table as it stands; the face writes
        // a table only after `SEPARATE_ARRAY`, so a table the callback
        // shared is separated first and walked on in the copy, which
        // carries the iterator (`zend_array_dup_ht_iterators`).
        let reference = if let Some(reference) = value.as_php_ref() {
            *reference
        } else {
            let separated = held_array(req, holder)?;
            if !separated.is_same(table) {
                table = separated;
                pos = at.position(req, &table);
                continue;
            }
            req.addref(&value);
            let reference = PhpRef::new(req, value)?;
            table.replace(req, &key, reference.as_value())?;
            reference
        };
        // `zend_hash_move_forward_ex`, before the call, "this mirrors the
        // approach used by foreach": the next live bucket, or the end.
        let next = found.saturating_add(1);
        pos = table.live_from(next).map_or(next, |(next, _, _)| next);
        at.set_position(req, pos);
        visit(req, reference, &key, walk)?;
        // "Reload array and position -- both may have changed".
        let now = holder.get();
        if let Some(now) = now.as_array().copied() {
            if !now.is_same(table) {
                table = held_array(req, holder)?;
            }
            pos = at.position(req, &table);
        } else if let Some(object) = now.as_object().copied() {
            return walk_object(req, object, walk);
        } else {
            return Err(throw_error(
                req,
                "TypeError",
                "Iterated value is no longer an array or object",
            ));
        }
    }
}

/// The array the cell `holder` holds, separated: what the walk writes
/// once it has read that the cell holds an array.
fn held_array(req: &mut Request, holder: PhpRef) -> Result<PhpArray, Thrown> {
    match holder.separate_array(req)? {
        Some(table) => Ok(table),
        None => Err(fatal(
            req,
            "InternalError: array_walk's variable holds no array",
        )),
    }
}

/// One element of `php_array_walk`, made a reference in place: under
/// `recursive` an array it holds separated and walked in its turn,
/// guarded against an array that holds itself, and otherwise the cell
/// handed to the callback with its key, and `$arg` third where it was
/// given.
fn visit(req: &mut Request, reference: PhpRef, key: &Key, walk: &Walk) -> Result<(), Thrown> {
    if walk.recursive && reference.get().as_array().is_some() {
        call_stack_end(req)?;
        let Some(nested) = reference.separate_array(req)? else {
            return Ok(());
        };
        if nested.is_recursive() {
            return Err(throw_error(req, "Error", "Recursion detected"));
        }
        req.addref(&reference.as_value());
        nested.protect();
        let walked = walk_holder(req, reference, walk);
        if reference.get().as_array() == Some(&nested) {
            nested.unprotect();
        }
        req.release(reference.as_value());
        return walked;
    }
    // `ZVAL_COPY(&args[0], zv)` and `zend_hash_get_current_key_zval_ex`:
    // the call's cell and key are owners of their own, so a callback that
    // removes the element leaves both to the call.
    let (cell, key) = (reference.as_value(), key_value(key));
    req.addref(&cell);
    req.addref(&key);
    let mut arguments = vec![cell, key];
    if let Some(userdata) = walk.userdata {
        arguments.push(userdata);
    }
    let answer = call(req, walk.callback, &arguments);
    req.release(key);
    req.release(cell);
    req.release(answer?);
    Ok(())
}

/// `php_array_walk` over an object: `HASH_OF` is the property table, a
/// lazy object initialised first, and each row a slot or a dynamic
/// property holds is made a reference in place, a typed property's slot
/// typed by it (`ZEND_REF_ADD_TYPE_SOURCE`,
/// [`PhpObject::typed_row_ref`]), so a write through the callback's
/// `&$value` reaches the object and is held to the property's type. A
/// declared slot nothing has written is no row and is not visited; the
/// rows are read again after every call, as an array's are, so a row
/// the callback removed is not visited and one it added is.
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
fn walk_object(req: &mut Request, object: PhpObject, walk: &Walk) -> Result<(), Thrown> {
    let mut visited: HashSet<Key> = HashSet::new();
    loop {
        let pending: Vec<Key> = object
            .entries(req)?
            .into_iter()
            .map(|(key, _)| key)
            .filter(|key| !visited.contains(key))
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        for key in pending {
            visited.insert(key.clone());
            // A row an earlier callback removed is not visited.
            if object.row(req, &key)?.is_none() {
                continue;
            }
            let cell = object.typed_row_ref(req, &key)?;
            let visited_row = visit(req, cell, &key, walk);
            req.release(cell.as_value());
            visited_row?;
        }
    }
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
) -> Result<Value, Thrown> {
    let mut carry = initial;
    req.addref(&carry);
    for (_, operand) in array.snapshot_refs() {
        let answer = call(req, &callback, &[carry, operand]);
        req.release(carry);
        carry = answer?;
        if let Some(reference) = carry.as_php_ref().copied() {
            let inner = reference.get();
            req.addref(&inner);
            req.release(carry);
            carry = inner;
        }
    }
    Ok(carry)
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
) -> Result<PhpArray, Thrown> {
    if array.is_empty() {
        return Ok(PhpArray::empty());
    }
    let mut out = PhpArray::new(req)?;
    for (key, operand) in array.snapshot_refs() {
        let keep = match &callback {
            Some(callback) => {
                let arguments = match mode {
                    ARRAY_FILTER_USE_KEY => vec![key_value(&key)],
                    ARRAY_FILTER_USE_BOTH => vec![operand, key_value(&key)],
                    _ => vec![operand],
                };
                let answer = match call(req, callback, &arguments) {
                    Ok(answer) => answer,
                    Err(thrown) => {
                        req.release(out.to_value());
                        return Err(thrown);
                    }
                };
                let truth = truthy(req, &answer);
                req.release(answer);
                truth
            }
            None => truthy(req, &operand.deref()),
        };
        if keep {
            let value = copied(req, operand);
            let key = key_copy(req, &key);
            out.set(req, key, value)?;
        }
    }
    Ok(out)
}

/// What `php_array_find` found: the value and the key of the first
/// element the callback answered truthy for (falsy, under `negate`).
fn find(
    req: &mut Request,
    array: PhpArray,
    callback: &Callable,
    negate: bool,
) -> Result<Option<(Key, Value)>, Thrown> {
    for (key, operand) in array.snapshot_refs() {
        let answer = call(req, callback, &[operand, key_value(&key)])?;
        let truth = truthy(req, &answer);
        req.release(answer);
        if truth != negate {
            return Ok(Some((key, operand)));
        }
    }
    Ok(None)
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
fn array_find(req: &mut Request, array: PhpArray, callback: Callable) -> Result<Value, Thrown> {
    match find(req, array, &callback, false)? {
        Some((_, value)) => {
            let value = value.deref();
            req.addref(&value);
            Ok(value)
        }
        None => Ok(Value::null()),
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
fn array_find_key(req: &mut Request, array: PhpArray, callback: Callable) -> Result<Value, Thrown> {
    match find(req, array, &callback, false)? {
        Some((key, _)) => {
            let value = key_value(&key);
            req.addref(&value);
            Ok(value)
        }
        None => Ok(Value::null()),
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
    Ok(find(req, array, &callback, false)?.is_some())
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
    Ok(find(req, array, &callback, true)?.is_none())
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
#[php_function]
fn array_map(
    req: &mut Request,
    callback: Option<Callable>,
    array: PhpArray,
    arrays: Variadic<PhpArray>,
) -> Result<Raw<PhpArray>, Thrown> {
    let mapped = mapped(req, callback, array, arrays)?;
    Raw::from_value(req, mapped.map_or(Value::null(), Value::array))
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
) -> Result<Option<PhpArray>, Thrown> {
    let arrays: Vec<PhpArray> = std::iter::once(array).chain(arrays.0).collect();
    if let [input] = arrays[..] {
        let Some(callback) = callback else {
            req.addref(&Value::array(input));
            return Ok(Some(input));
        };
        if input.is_empty() {
            req.addref(&Value::array(input));
            return Ok(Some(input));
        }
        let mut out = PhpArray::new(req)?;
        for (key, operand) in input.snapshot_refs() {
            let answer = match call(req, &callback, &[operand]) {
                Ok(answer) => answer,
                Err(thrown) => {
                    req.release(out.to_value());
                    return Err(thrown);
                }
            };
            if matches!(answer.as_ref(), ValueRef::Undef) {
                req.release(out.to_value());
                return Ok(None);
            }
            let key = key_copy(req, &key);
            out.set(req, key, answer)?;
        }
        return Ok(Some(out));
    }
    let columns: Vec<Vec<Value>> = arrays
        .iter()
        .map(|array| array.snapshot_refs().into_iter().map(|(_, v)| v).collect())
        .collect();
    let longest = columns.iter().map(Vec::len).max().unwrap_or(0);
    let mut out = PhpArray::new(req)?;
    for k in 0..longest {
        let row: Vec<Value> = columns
            .iter()
            .map(|column| column.get(k).copied().unwrap_or_default())
            .collect();
        let element = match &callback {
            None => {
                let mut tuple = PhpArray::new(req)?;
                for value in row {
                    req.addref(&value);
                    tuple.append(req, value)?;
                }
                Value::array(tuple)
            }
            Some(callback) => match call(req, callback, &row) {
                Ok(answer) if matches!(answer.as_ref(), ValueRef::Undef) => {
                    req.release(out.to_value());
                    return Ok(None);
                }
                Ok(answer) => answer,
                Err(thrown) => {
                    req.release(out.to_value());
                    return Err(thrown);
                }
            },
        };
        out.append(req, element)?;
    }
    Ok(Some(out))
}
