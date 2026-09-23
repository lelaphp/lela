//! The natives of `array.c` that call back through the engine's call
//! seam, `api::call::call`: `array_walk` and `array_walk_recursive`,
//! which hand each element to the callback by reference, `array_map`,
//! `array_filter`, `array_reduce`, and the four searches 8.4 added,
//! `array_find`, `array_find_key`, `array_any` and `array_all`. A
//! callback that throws ends the walk where it threw and the native
//! raises what was thrown, as php's loops stop on `EG(exception)`.

use std::collections::HashSet;

use php_ext::abi::Callable;
use php_ext::call::call;
use php_ext::coerce::{coerce_param, truthy, value_name};
use php_ext::decl::{ConstExpr, FunctionDecl, Name, Parameter, Type};
use php_ext::raising::{Thrown, arg_type_error, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpRef, Value, ValueRef};

use super::sort::key_value;
use super::{copied, key_copy, unknown};
use crate::output::Entry;
use crate::{argument_count_between, callable_argument, declared, long_argument, parameter};

/// `ARRAY_FILTER_USE_BOTH` and `ARRAY_FILTER_USE_KEY`.
pub(super) const ARRAY_FILTER_USE_BOTH: i64 = 1;
pub(super) const ARRAY_FILTER_USE_KEY: i64 = 2;

/// The declarations, as `basic_functions.stub.php` has them, the walks'
/// `$arg` optional with the default php leaves unknown.
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
pub(super) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let walk = |name: &[u8], entry: Entry| {
        (
            declared(
                name,
                &[
                    Parameter {
                        by_ref: true,
                        ..parameter(b"array", Type::Union(vec![Type::Array, Type::Object]))
                    },
                    parameter(b"callback", Type::Callable),
                    Parameter {
                        default: Some(unknown()),
                        ..parameter(b"arg", Type::Mixed)
                    },
                ],
                Type::True,
            ),
            entry,
        )
    };
    let search = |name: &[u8], returns: Type, entry: Entry| {
        (
            declared(
                name,
                &[
                    parameter(b"array", Type::Array),
                    parameter(b"callback", Type::Callable),
                ],
                returns,
            ),
            entry,
        )
    };
    vec![
        walk(b"array_walk", array_walk),
        walk(b"array_walk_recursive", array_walk_recursive),
        (
            declared(
                b"array_reduce",
                &[
                    parameter(b"array", Type::Array),
                    parameter(b"callback", Type::Callable),
                    Parameter {
                        default: Some(ConstExpr::Null),
                        ..parameter(b"initial", Type::Mixed)
                    },
                ],
                Type::Mixed,
            ),
            array_reduce,
        ),
        (
            declared(
                b"array_filter",
                &[
                    parameter(b"array", Type::Array),
                    Parameter {
                        default: Some(ConstExpr::Null),
                        ..parameter(b"callback", Type::Nullable(Box::new(Type::Callable)))
                    },
                    Parameter {
                        default: Some(ConstExpr::Long(0)),
                        ..parameter(b"mode", Type::Int)
                    },
                ],
                Type::Array,
            ),
            array_filter,
        ),
        search(b"array_find", Type::Mixed, array_find),
        search(b"array_find_key", Type::Mixed, array_find_key),
        search(b"array_any", Type::Bool, array_any),
        search(b"array_all", Type::Bool, array_all),
        (
            declared(
                b"array_map",
                &[
                    parameter(b"callback", Type::Nullable(Box::new(Type::Callable))),
                    parameter(b"array", Type::Array),
                    Parameter {
                        variadic: true,
                        ..parameter(b"arrays", Type::Array)
                    },
                ],
                Type::Array,
            ),
            array_map,
        ),
    ]
}

/// `array_walk(array|object &$array, callable $callback, mixed $arg = UNKNOWN): true`,
/// php's `php_array_walk` (`Z/ext/standard/array.c:1438`):
/// each element of the array the reference holds made a reference in
/// place (`ZVAL_MAKE_REF`) and handed to the callback with its key, and
/// `$arg` third where it was given, so a callback that takes the value
/// by reference writes the caller's array. The array is read again
/// after every call, since the callback may change it: an element it
/// removed is not visited, and one it appended is.
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
fn array_walk(req: &mut Request) -> Result<Value, Thrown> {
    walk(req, false)
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
fn array_walk_recursive(req: &mut Request) -> Result<Value, Thrown> {
    walk(req, true)
}

fn walk(req: &mut Request, recursive: bool) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 3)?;
    let given = req.argument(1).unwrap_or_default();
    let target = given.deref();
    if !matches!(target.as_ref(), ValueRef::Array(_) | ValueRef::Object(_)) {
        let name = value_name(req, &target);
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type array, {name} given"),
        ));
    }
    let callback = callable_argument(req, 2)?;
    let userdata = if req.argument_count() == 3 {
        req.argument(3)
    } else {
        None
    };
    // A value where a reference was asked for, past the notice: the
    // walk is over a cell of its own, which nobody reads after.
    let holder = if let ValueRef::Reference(reference) = given.as_ref() {
        *reference
    } else {
        req.addref(&target);
        PhpRef::new(req, target)?
    };
    let walked = walk_holder(req, holder, &callback, userdata, recursive);
    if !matches!(given.as_ref(), ValueRef::Reference(_)) {
        req.release(holder.as_value());
    }
    walked?;
    Ok(Value::bool(true))
}

/// One level of `php_array_walk` over what the cell `holder` holds.
fn walk_holder(
    req: &mut Request,
    holder: PhpRef,
    callback: &Callable,
    userdata: Option<Value>,
    recursive: bool,
) -> Result<(), Thrown> {
    let start = holder.get();
    if let Some(object) = start.as_object().copied() {
        return walk_object(req, object.as_value(), callback, userdata);
    }
    let Some(array) = holder.separate_array(req)? else {
        return Ok(());
    };
    if array.is_empty() {
        return Ok(());
    }
    let mut visited: HashSet<Key> = HashSet::new();
    let mut pending: Vec<Key> = array.snapshot_refs().into_iter().map(|(k, _)| k).collect();
    loop {
        for key in std::mem::take(&mut pending) {
            if !visited.insert(key.clone()) {
                continue;
            }
            // "Reload array and position -- both may have changed".
            let now = holder.get();
            let Some(mut array) = now.as_array().copied() else {
                return Err(throw(
                    req,
                    "TypeError",
                    "Iterated value is no longer an array or object",
                ));
            };
            let Some(value) = array.get(&key) else {
                continue;
            };
            // `ZVAL_MAKE_REF(zv)`: the element a reference in place.
            let reference = if let Some(reference) = value.as_php_ref() {
                *reference
            } else {
                let Some(separated) = holder.separate_array(req)? else {
                    continue;
                };
                array = separated;
                req.addref(&value);
                let reference = PhpRef::new(req, value)?;
                let written = key_copy(req, &key);
                array.set(req, written, reference.as_value())?;
                reference
            };
            if recursive && reference.get().as_array().is_some() {
                let Some(nested) = reference.separate_array(req)? else {
                    continue;
                };
                if nested.is_recursive() {
                    return Err(throw(req, "Error", "Recursion detected"));
                }
                req.addref(&reference.as_value());
                nested.protect();
                let walked = walk_holder(req, reference, callback, userdata, true);
                if reference.get().as_array() == Some(&nested) {
                    nested.unprotect();
                }
                req.release(reference.as_value());
                walked?;
                continue;
            }
            let key_arg = key_value(&key);
            let mut arguments = vec![reference.as_value(), key_arg];
            if let Some(userdata) = userdata {
                arguments.push(userdata);
            }
            let answer = call(req, callback, &arguments)?;
            req.release(answer);
        }
        // What the callbacks appended is visited after, as php's position
        // walks on into it.
        let now = holder.get();
        let Some(array) = now.as_array().copied() else {
            return Ok(());
        };
        pending = array
            .snapshot_refs()
            .into_iter()
            .map(|(k, _)| k)
            .filter(|k| !visited.contains(k))
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
    }
}

/// The walk over an object: its properties as the `(array)` cast has
/// them, each handed to the callback by value, since the face has no
/// reference into a property slot.
fn walk_object(
    req: &mut Request,
    object: Value,
    callback: &Callable,
    userdata: Option<Value>,
) -> Result<(), Thrown> {
    let table = php_ext::coerce::to_array(req, &object)?;
    for (key, value) in table.snapshot_refs() {
        let mut arguments = vec![value, key_value(&key)];
        if let Some(userdata) = userdata {
            arguments.push(userdata);
        }
        let answer = call(req, callback, &arguments)?;
        req.release(answer);
    }
    req.release(table.to_value());
    Ok(())
}

/// `Z_PARAM_ARRAY` at `position`: the array, or php's `TypeError`.
pub(super) fn array_argument(req: &mut Request, position: u32) -> Result<PhpArray, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::Array, position)?;
    match coerced.deref().as_array() {
        Some(array) => Ok(*array),
        None => PhpArray::new(req),
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
fn array_reduce(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 3)?;
    let array = array_argument(req, 1)?;
    let callback = callable_argument(req, 2)?;
    let mut carry = if req.argument_count() == 3 {
        let initial = req.argument(3).unwrap_or_default().deref();
        req.addref(&initial);
        initial
    } else {
        Value::null()
    };
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
fn array_filter(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 3)?;
    let array = array_argument(req, 1)?;
    let callback = if req.argument_count() >= 2 {
        let given = req.argument(2).unwrap_or_default().deref();
        if matches!(given.as_ref(), ValueRef::Null) {
            None
        } else {
            Some(callable_argument(req, 2)?)
        }
    } else {
        None
    };
    let mode = if req.argument_count() == 3 {
        long_argument(req, 3)?
    } else {
        0
    };
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
    Ok(Value::array(out))
}

/// What `php_array_find` found: the value and the key of the first
/// element the callback answered truthy for (falsy, under `negate`).
fn find(req: &mut Request, negate: bool) -> Result<Option<(Key, Value)>, Thrown> {
    argument_count_between(req, 2, 2)?;
    let array = array_argument(req, 1)?;
    let callback = callable_argument(req, 2)?;
    for (key, operand) in array.snapshot_refs() {
        let answer = call(req, &callback, &[operand, key_value(&key)])?;
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
fn array_find(req: &mut Request) -> Result<Value, Thrown> {
    match find(req, false)? {
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
fn array_find_key(req: &mut Request) -> Result<Value, Thrown> {
    match find(req, false)? {
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
fn array_any(req: &mut Request) -> Result<Value, Thrown> {
    Ok(Value::bool(find(req, false)?.is_some()))
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
fn array_all(req: &mut Request) -> Result<Value, Thrown> {
    Ok(Value::bool(find(req, true)?.is_none()))
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
fn array_map(req: &mut Request) -> Result<Value, Thrown> {
    let given = req.argument_count();
    if given < 2 {
        return Err(php_ext::raising::arg_count_error(
            req,
            &format!("at least 2 arguments, {given} given"),
        ));
    }
    let callback_value = req.argument(1).unwrap_or_default().deref();
    let callback = if matches!(callback_value.as_ref(), ValueRef::Null) {
        None
    } else {
        match php_ext::coerce::callable_checked(req, &callback_value) {
            Ok(callable) => Some(callable),
            Err(why) => {
                return Err(arg_type_error(
                    req,
                    1,
                    &format!("be a valid callback or null, {why}"),
                ));
            }
        }
    };
    let values = req.variadic_arguments(2)?;
    let mut arrays = Vec::with_capacity(values.len());
    for (at, value) in values.iter().enumerate() {
        let value = value.deref();
        let Some(array) = value.as_array().copied() else {
            let position = u32::try_from(at + 2).unwrap_or(u32::MAX);
            let name = value_name(req, &value);
            return Err(arg_type_error(
                req,
                position,
                &format!("be of type array, {name} given"),
            ));
        };
        arrays.push(array);
    }
    if let [input] = arrays[..] {
        let Some(callback) = callback else {
            let value = Value::array(input);
            req.addref(&value);
            return Ok(value);
        };
        if input.is_empty() {
            let value = Value::array(input);
            req.addref(&value);
            return Ok(value);
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
            let key = key_copy(req, &key);
            out.set(req, key, answer)?;
        }
        return Ok(Value::array(out));
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
                Ok(answer) => answer,
                Err(thrown) => {
                    req.release(out.to_value());
                    return Err(thrown);
                }
            },
        };
        out.append(req, element)?;
    }
    Ok(Value::array(out))
}

#[allow(dead_code)]
fn unused(_: Name) {}
