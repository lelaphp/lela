//! The internal-pointer natives of `array.c` and the two that pick an
//! extreme: `end`, `prev`, `next` and `reset` move the pointer of the
//! array a reference holds and answer what it lands on, `current`, its
//! alias `pos`, and `key` read it, each over the heap's
//! `nInternalPointer` through the face's cursor; `min` and `max` walk
//! their arguments, or the one array they are given, with
//! `zend_compare`.
//!
//! An object is php's 8.1 deprecation and then its property table
//! (`get_ht_for_iap`). The face has no pointer on that table, so what is
//! read is a fresh table per call: `current` and `key` read its first
//! row, and a move answers from that row, which is php's answer for an
//! object whose pointer has not moved.

use std::cmp::Ordering;

use php_ext::coerce::{compare, to_array, value_name};
use php_ext::decl::{FunctionDecl, Parameter, Type};
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref, too_few_arguments};
use php_ext::request::Request;
use php_ext::values::{PhpArray, Value, ValueRef};

use super::sort::key_value;
use crate::output::Entry;
use crate::{argument_count_between, declared, frameless, parameter};

/// The declarations, as `basic_functions.stub.php` has them: the four
/// that move take the array by reference, the three that read take it
/// by value, and `min` and `max` declare the frameless arity 2.
///
/// ```php
/// foreach (['end', 'prev', 'next', 'reset', 'current', 'pos', 'key', 'min', 'max'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function end ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> object|array &$array ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function prev ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> object|array &$array ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function next ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> object|array &$array ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function reset ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> object|array &$array ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function current ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> object|array $array ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function pos ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> object|array $array ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function key ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> object|array $array ]
///   }
///   - Return [ string|int|null ]
/// }
/// Function [ <internal:standard> function min ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> mixed $value ]
///     Parameter #1 [ <optional> mixed ...$values ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function max ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> mixed $value ]
///     Parameter #1 [ <optional> mixed ...$values ]
///   }
///   - Return [ mixed ]
/// }
/// ```
pub(super) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let array_or_object = || Type::Union(vec![Type::Array, Type::Object]);
    let moved = |name: &[u8], entry: Entry| {
        (
            declared(
                name,
                &[Parameter {
                    by_ref: true,
                    ..parameter(b"array", array_or_object())
                }],
                Type::Mixed,
            ),
            entry,
        )
    };
    let read = |name: &[u8], returns: Type, entry: Entry| {
        (
            declared(name, &[parameter(b"array", array_or_object())], returns),
            entry,
        )
    };
    let extreme = |name: &[u8], entry: Entry| {
        (
            frameless(
                declared(
                    name,
                    &[
                        parameter(b"value", Type::Mixed),
                        Parameter {
                            variadic: true,
                            ..parameter(b"values", Type::Mixed)
                        },
                    ],
                    Type::Mixed,
                ),
                &[2],
            ),
            entry,
        )
    };
    vec![
        moved(b"end", end),
        moved(b"prev", prev),
        moved(b"next", next),
        moved(b"reset", reset),
        read(b"current", Type::Mixed, current),
        read(b"pos", Type::Mixed, pos),
        read(
            b"key",
            Type::Union(vec![Type::String, Type::Int, Type::Null]),
            key,
        ),
        extreme(b"min", min),
        extreme(b"max", max),
    ]
}

/// One move of the pointer: `reset`, `end`, one on or one back.
#[derive(Clone, Copy)]
enum Move {
    Reset,
    End,
    Next,
    Prev,
}

impl Move {
    /// The native that makes the move.
    fn name(self) -> &'static str {
        match self {
            Move::Reset => "reset",
            Move::End => "end",
            Move::Next => "next",
            Move::Prev => "prev",
        }
    }
}

/// `end(array|object &$array): mixed`, php's `PHP_FUNCTION(end)`
/// (`Z/ext/standard/array.c:1053`): the pointer of the array the
/// reference holds, separated first, moved to the last element, and
/// that element's value, a reference followed; `false` for an empty
/// array, whose pointer is left where it stands.
///
/// ```php
/// $a = [1, 2, 3]; var_dump(end($a), key($a));
/// $e = []; var_dump(end($e));
/// $b = ["x" => 1, "y" => 2]; $c = $b; end($c); var_dump(key($b), key($c));
/// function pair() { return ["a", "b"]; } var_dump(end(pair()));
/// ```
/// ```expect
/// int(3)
/// int(2)
/// bool(false)
/// string(1) "x"
/// string(1) "y"
///
/// Notice: Only variables should be passed by reference in %s on line 5
/// string(1) "b"
/// ```
///
/// Anything but an array or an object is `Z_PARAM_ARRAY_OR_OBJECT_EX`'s
/// `TypeError`, which names the array alone.
///
/// ```php
/// $x = 1; try { end($x); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// end(): Argument #1 ($array) must be of type array, int given
/// ```
fn end(req: &mut Request) -> Result<Value, Thrown> {
    moved(req, Move::End)
}

/// `prev(array|object &$array): mixed`, php's `PHP_FUNCTION(prev)`
/// (`Z/ext/standard/array.c:1075`): the pointer one element back and
/// what it lands on; from the first element it goes past the end and
/// the answer is `false`, and from past the end it stays.
///
/// ```php
/// $a = [1, 2, 3]; end($a); var_dump(prev($a), prev($a), prev($a), prev($a), current($a));
/// ```
/// ```expect
/// int(2)
/// int(1)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn prev(req: &mut Request) -> Result<Value, Thrown> {
    moved(req, Move::Prev)
}

/// `next(array|object &$array): mixed`, php's `PHP_FUNCTION(next)`
/// (`Z/ext/standard/array.c:1097`): the pointer one element on and what
/// it lands on, `false` past the end. The move writes the array the
/// reference holds, so another holder of the same array keeps its own
/// pointer.
///
/// ```php
/// $a = [1, 2, 3]; var_dump(next($a), next($a), next($a), next($a), key($a));
/// $b = ["x" => 1, "y" => 2]; $c = $b; next($c); var_dump(current($b), current($c));
/// $d = [1, 2]; $r = &$d; next($r); var_dump(current($d));
/// ```
/// ```expect
/// int(2)
/// int(3)
/// bool(false)
/// bool(false)
/// NULL
/// int(1)
/// int(2)
/// int(2)
/// ```
fn next(req: &mut Request) -> Result<Value, Thrown> {
    moved(req, Move::Next)
}

/// `reset(array|object &$array): mixed`, php's `PHP_FUNCTION(reset)`
/// (`Z/ext/standard/array.c:1119`): the pointer to the first element and
/// its value, `false` for an empty array.
///
/// ```php
/// $a = [1, 2, 3]; end($a); var_dump(reset($a), key($a));
/// $e = []; var_dump(reset($e));
/// $r = ["k" => [9]]; var_dump(reset($r));
/// ```
/// ```expect
/// int(1)
/// int(0)
/// bool(false)
/// array(1) {
///   [0]=>
///   int(9)
/// }
/// ```
fn reset(req: &mut Request) -> Result<Value, Thrown> {
    moved(req, Move::Reset)
}

/// `current(array|object $array): mixed` and its alias `pos`, php's
/// `PHP_FUNCTION(current)` (`Z/ext/standard/array.c:1141`): the value at
/// the pointer, a reference followed, or `false` where the pointer is
/// past the end. The array is taken by value, so a call over a
/// temporary reads its first element.
///
/// ```php
/// $a = [1, 2]; var_dump(current($a), pos($a)); next($a); next($a); var_dump(current($a));
/// function f() { return [5, 6]; } var_dump(current(f()), current([]));
/// $x = 1; $b = []; $b[0] = &$x; var_dump(current($b));
/// try { current(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// int(1)
/// bool(false)
/// int(5)
/// bool(false)
/// int(1)
/// current(): Argument #1 ($array) must be of type array, int given
/// ```
///
/// An object is the deprecation, and then its properties.
///
/// ```php
/// $o = new stdClass; $o->a = 1; $o->b = 2; var_dump(current($o), key($o), reset($o));
/// ```
/// ```expect
/// Deprecated: current(): Calling current() on an object is deprecated in %s on line 2
///
/// Deprecated: key(): Calling key() on an object is deprecated in %s on line 2
///
/// Deprecated: reset(): Calling reset() on an object is deprecated in %s on line 2
/// int(1)
/// string(1) "a"
/// int(1)
/// ```
fn current(req: &mut Request) -> Result<Value, Thrown> {
    read_current(req, "current")
}

/// `pos(array|object $array): mixed`, the alias of `current`, which
/// names itself where it raises.
///
/// ```php
/// $a = [3, 4]; next($a); var_dump(pos($a));
/// $o = new stdClass; $o->p = 7; var_dump(pos($o));
/// ```
/// ```expect
/// int(4)
///
/// Deprecated: pos(): Calling pos() on an object is deprecated in %s on line 3
/// int(7)
/// ```
fn pos(req: &mut Request) -> Result<Value, Thrown> {
    read_current(req, "pos")
}

fn read_current(req: &mut Request, name: &str) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let Some(array) = read_table(req, name)? else {
        return Ok(Value::bool(false));
    };
    match array.cursor_current() {
        Some(value) => {
            let value = value.deref();
            req.addref(&value);
            Ok(value)
        }
        None => Ok(Value::bool(false)),
    }
}

/// `key(array|object $array): int|string|null`, php's
/// `PHP_FUNCTION(key)` (`Z/ext/standard/array.c:1155`): the key at the
/// pointer, `null` past the end.
///
/// ```php
/// $a = ["x" => 1, 5 => 2]; var_dump(key($a)); next($a); var_dump(key($a)); next($a); var_dump(key($a), key([]));
/// try { key("s"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "x"
/// int(5)
/// NULL
/// NULL
/// key(): Argument #1 ($array) must be of type array, string given
/// ```
fn key(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let Some(array) = read_table(req, "key")? else {
        return Ok(Value::null());
    };
    match array.cursor_key() {
        Some(key) => {
            let value = key_value(&key);
            req.addref(&value);
            Ok(value)
        }
        None => Ok(Value::null()),
    }
}

/// `Z_PARAM_ARRAY_OR_OBJECT` for the readers: the array, or an object's
/// property table after the deprecation; the `TypeError` otherwise.
fn read_table(req: &mut Request, name: &str) -> Result<Option<PhpArray>, Thrown> {
    let given = req.argument(1).unwrap_or_default().deref();
    match given.as_ref() {
        ValueRef::Array(array) => Ok(Some(*array)),
        ValueRef::Object(_) => object_table(req, name, &given).map(Some),
        _ => Err(not_an_array(req, &given)),
    }
}

/// `get_ht_for_iap` over an object: the deprecation, and the table the
/// `(array)` cast builds, which is the property table as php keys it.
fn object_table(req: &mut Request, name: &str, object: &Value) -> Result<PhpArray, Thrown> {
    docref(
        req,
        Level::Deprecated,
        &format!("Calling {name}() on an object is deprecated"),
    );
    to_array(req, object)
}

/// The `TypeError` `Z_PARAM_ARRAY_OR_OBJECT` raises, naming the array.
fn not_an_array(req: &mut Request, given: &Value) -> Thrown {
    let name = value_name(req, given);
    arg_type_error(req, 1, &format!("be of type array, {name} given"))
}

/// The four movers: the array the reference holds, separated as
/// `Z_PARAM_ARRAY_OR_OBJECT_EX(array_zv, 0, 1)` separates it; `false`
/// where it is empty, the pointer untouched; else the move, and the
/// value it lands on as `php_array_iter_return_current` reads it.
fn moved(req: &mut Request, how: Move) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let given = req.argument(1).unwrap_or_default();
    let target = given.deref();
    let mut array = match target.as_ref() {
        ValueRef::Array(array) => match given.as_ref() {
            ValueRef::Reference(reference) => match reference.separate_array(req)? {
                Some(array) => array,
                None => *array,
            },
            // A value sent where a reference was asked for, past the
            // notice: php separates the argument slot, which nobody
            // reads after, so the move is over a copy.
            _ => array.copy(req)?,
        },
        ValueRef::Object(_) => object_table(req, how.name(), &target)?,
        _ => return Err(not_an_array(req, &target)),
    };
    if array.is_empty() {
        return Ok(Value::bool(false));
    }
    match how {
        Move::Reset => array.cursor_reset(),
        Move::End => array.cursor_end(),
        Move::Next => array.cursor_next(),
        Move::Prev => array.cursor_prev(),
    }
    match array.cursor_current() {
        Some(value) => {
            let value = value.deref();
            req.addref(&value);
            Ok(value)
        }
        None => Ok(Value::bool(false)),
    }
}

/// `min(mixed $value, mixed ...$values): mixed`, php's
/// `PHP_FUNCTION(min)` (`Z/ext/standard/array.c:1181`): of one argument,
/// the least element of the array it must be, `zend_hash_minmax` keeping
/// the first of equal ones; of more, the least argument, a stretch of ints
/// or of floats compared directly and anything else by `zend_compare`,
/// the first of equal ones kept.
///
/// ```php
/// var_dump(min(2, 1, 3), min([4, 2, 8]), min("apple", "banana"), min([0, -0.0]), min(1.5, 1), min(0, "a", -1));
/// ```
/// ```expect
/// int(1)
/// int(2)
/// string(5) "apple"
/// int(0)
/// int(1)
/// int(-1)
/// ```
///
/// A call of two arguments the compiler resolved is
/// `ZEND_FRAMELESS_FUNCTION(min, 2)`, which answers the right operand
/// where the two are equal; a call through a variable is the framed
/// body, which keeps the left.
///
/// ```php
/// $f = 'min'; var_dump(min(1, "1"), min("1", 1), $f(1, "1"), $f("1", 1));
/// ```
/// ```expect
/// string(1) "1"
/// int(1)
/// int(1)
/// string(1) "1"
/// ```
///
/// ```php
/// try { min([]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { min(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// min(): Argument #1 ($value) must contain at least one element
/// min(): Argument #1 ($value) must be of type array, int given
/// ```
fn min(req: &mut Request) -> Result<Value, Thrown> {
    extreme(req, Ordering::Less)
}

/// `max(mixed $value, mixed ...$values): mixed`, php's
/// `PHP_FUNCTION(max)` (`Z/ext/standard/array.c:1309`): `min` with the
/// comparison turned round, the frameless arity keeping the left of two
/// equal operands where `min`'s keeps the right.
///
/// ```php
/// var_dump(max([1, 5, 3]), max("10", 9), max(1, "1"), max("1", 1), max(0, -0.0), max(PHP_INT_MAX, (float)PHP_INT_MAX), max("abc", [0]), max([1, 2], [1, 3]));
/// $f = 'max'; var_dump($f(1, "1"), $f("1", 1));
/// ```
/// ```expect
/// int(5)
/// string(2) "10"
/// int(1)
/// string(1) "1"
/// int(0)
/// int(9223372036854775807)
/// array(1) {
///   [0]=>
///   int(0)
/// }
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(3)
/// }
/// int(1)
/// string(1) "1"
/// ```
fn max(req: &mut Request) -> Result<Value, Thrown> {
    extreme(req, Ordering::Greater)
}

/// The walk `min` and `max` share; `wanted` is the answer of
/// `compare(candidate, best)` that makes the candidate the new best.
fn extreme(req: &mut Request, wanted: Ordering) -> Result<Value, Thrown> {
    let given = req.argument_count();
    if given < 1 {
        return Err(too_few_arguments(req, 1, u32::try_from(given).unwrap_or(0)));
    }
    let arguments = req.variadic_arguments(1)?;
    if let [only] = arguments[..] {
        let only = php_ext::coerce::coerce_param(req, &only, &Type::Array, 1)?;
        let Some(array) = only.as_array() else {
            return Ok(Value::null());
        };
        // `zend_hash_minmax`: the first element, replaced by a later one
        // only where it is strictly beyond.
        let mut best: Option<Value> = None;
        for (_, value) in array {
            let value = value.deref();
            best = Some(match best {
                None => value,
                Some(held) if compare(req, &value, &held)? == wanted => value,
                Some(held) => held,
            });
        }
        let Some(best) = best else {
            return Err(arg_value_error(req, 1, "contain at least one element"));
        };
        req.addref(&best);
        return Ok(best);
    }
    let values: Vec<Value> = arguments.iter().map(Value::deref).collect();
    if values.len() == 2 && req.frameless() {
        return frameless_extreme(req, values[0], values[1], wanted);
    }
    let mut best = values[0];
    for value in &values[1..] {
        if beyond(req, value, &best, wanted)? {
            best = *value;
        }
    }
    req.addref(&best);
    Ok(best)
}

/// What php's dedicated numeric comparison answers where it applies.
#[derive(Clone, Copy, PartialEq)]
enum Numeric {
    /// Two ints, two floats, or an int beside a float where the int is
    /// exactly a double, compared directly.
    Ordered(Ordering),
    /// A NaN on either side, which answers neither way.
    Unordered,
    /// Anything else, which php hands to `zend_compare`.
    Generic,
}

/// php's dedicated numeric comparison where it applies.
fn dedicated(a: &Value, b: &Value) -> Numeric {
    #[allow(clippy::cast_precision_loss)]
    let ordering = match (a.as_ref(), b.as_ref()) {
        (ValueRef::Int(x), ValueRef::Int(y)) => Some(x.cmp(&y)),
        (ValueRef::Float(x), ValueRef::Float(y)) => x.partial_cmp(&y),
        (ValueRef::Float(x), ValueRef::Int(y)) if exact(y) => x.partial_cmp(&(y as f64)),
        (ValueRef::Int(x), ValueRef::Float(y)) if exact(x) => (x as f64).partial_cmp(&y),
        _ => return Numeric::Generic,
    };
    ordering.map_or(Numeric::Unordered, Numeric::Ordered)
}

/// Whether `candidate` is strictly beyond `best` the way `wanted`
/// points, as the framed loops ask it.
fn beyond(
    req: &mut Request,
    candidate: &Value,
    best: &Value,
    wanted: Ordering,
) -> Result<bool, Thrown> {
    match dedicated(candidate, best) {
        Numeric::Ordered(ordering) => Ok(ordering == wanted),
        Numeric::Unordered => Ok(false),
        Numeric::Generic => Ok(compare(req, candidate, best)? == wanted),
    }
}

/// `zend_dval_to_lval_silent((double) n) == n`: whether the int is
/// exactly a double; one that rounds to 2^63 is out of range and
/// converts to zero.
fn exact(n: i64) -> bool {
    #[allow(clippy::cast_precision_loss)]
    let back = n as f64;
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&back) {
        return n == 0;
    }
    #[allow(clippy::cast_possible_truncation)]
    let round = back as i64;
    round == n
}

/// `ZEND_FRAMELESS_FUNCTION(min, 2)` and `(max, 2)`: `lhs < rhs ? lhs :
/// rhs` and `lhs >= rhs ? lhs : rhs`, directly over numbers and through
/// `zend_compare` otherwise.
fn frameless_extreme(
    req: &mut Request,
    lhs: Value,
    rhs: Value,
    wanted: Ordering,
) -> Result<Value, Thrown> {
    let ordering = match dedicated(&lhs, &rhs) {
        Numeric::Ordered(ordering) => Some(ordering),
        Numeric::Unordered => None,
        Numeric::Generic => Some(compare(req, &lhs, &rhs)?),
    };
    let left = if wanted == Ordering::Less {
        ordering == Some(Ordering::Less)
    } else {
        matches!(ordering, Some(Ordering::Greater | Ordering::Equal))
    };
    let chosen = if left { lhs } else { rhs };
    req.addref(&chosen);
    Ok(chosen)
}
