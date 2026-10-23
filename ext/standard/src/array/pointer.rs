//! The internal-pointer natives of `array.c` and the two that pick an
//! extreme: `end`, `prev`, `next` and `reset` move the pointer of the
//! array a reference holds and answer what it lands on, `current`, its
//! alias `pos`, and `key` read it, each over the heap's
//! `nInternalPointer` through the face's cursor; `min` and `max` walk
//! their arguments, or the one array they are given, with
//! `zend_compare`.
//!
//! An object is php's 8.1 deprecation and then its property table
//! (`get_ht_for_iap`), whose pointer stays on the table between calls
//! and steps over a declared slot nothing holds as
//! `php_array_iter_seek_current` does. A class whose `get_properties`
//! answers rows of its own keeps no such table here, so over one what
//! is read is a fresh table per call.

use std::cmp::Ordering;

use php_ext::abi::{Raw, Ref, Union, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::{coerce_param, compare, value_name};
use php_ext::decl::Type;
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, PointerMove, Value, ValueRef, walk};

use super::keys::key_return;

/// `array|object`, held by the body: `Z_PARAM_ARRAY_OR_OBJECT_EX`
/// refuses anything else as "must be of type array".
pub(super) type ArrayOrObject = Raw<Union<PhpArray, PhpObject>>;

/// The native that makes the move.
fn move_name(how: PointerMove) -> &'static str {
    match how {
        PointerMove::Reset => "reset",
        PointerMove::End => "end",
        PointerMove::Forward => "next",
        PointerMove::Backward => "prev",
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
#[php_function]
fn end(req: &mut Request, array: Ref<ArrayOrObject>) -> Result<Owner<Value>, Thrown> {
    moved(req, &array, PointerMove::End)
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
///
/// Over an object the pointer is its property table's and stays there
/// between calls: every declared property is a row to a move, and the
/// value read steps over one whose typed slot nothing wrote, back after
/// `end` and `prev` and on otherwise, moving the pointer as it goes, so
/// a move whose value nobody reads (`USED_RET()`) leaves the pointer on
/// the uninitialised row. A private property's key is its mangled name.
///
/// ```php
/// #[AllowDynamicProperties]
/// class T { public int $a; public int $b; private int $c = 3; public int $d; }
/// $x = new T; $x->b = 1; $x->e = 5;
/// var_dump(@reset($x), @next($x), @key($x), @next($x), @prev($x), @prev($x), @key($x));
/// @end($x); @prev($x); var_dump(@key($x));
/// @end($x); var_dump(@prev($x), @prev($x), @key($x));
/// class U { public int $a; }
/// $y = new U; var_dump(@key($y)); $y->a = 1; var_dump(@key($y)); @reset($y); var_dump(@key($y));
/// ```
/// ```expect
/// int(1)
/// int(3)
/// string(4) "%0T%0c"
/// int(5)
/// int(3)
/// int(1)
/// string(1) "b"
/// string(1) "e"
/// int(3)
/// int(1)
/// string(1) "b"
/// NULL
/// NULL
/// string(1) "a"
/// ```
#[php_function]
fn prev(req: &mut Request, array: Ref<ArrayOrObject>) -> Result<Owner<Value>, Thrown> {
    moved(req, &array, PointerMove::Backward)
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
#[php_function]
fn next(req: &mut Request, array: Ref<ArrayOrObject>) -> Result<Owner<Value>, Thrown> {
    moved(req, &array, PointerMove::Forward)
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
#[php_function]
fn reset(req: &mut Request, array: Ref<ArrayOrObject>) -> Result<Owner<Value>, Thrown> {
    moved(req, &array, PointerMove::Reset)
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
///
/// `pos(array|object $array): mixed` is its alias, which names itself
/// where it raises, as php reads the active function's name.
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
#[php_function(aliases(pos))]
fn current(req: &mut Request, array: ArrayOrObject) -> Result<Owner<Value>, Thrown> {
    let name = req.frames().active_function_name();
    read_current(req, array.value(), &name)
}

fn read_current(req: &mut Request, given: Value, name: &str) -> Result<Owner<Value>, Thrown> {
    let pointed = pointed(req, &given, name)?;
    let current = match &pointed {
        Pointed::Array(array) => array.cursor_current(),
        Pointed::Built(array) => array.cursor_current(),
        Pointed::Object(object) => object.pointer_row(req, None)?.map(|(_, value)| value),
    };
    let current = owned_current(req, current);
    pointed.read(req);
    Ok(current)
}

/// What `php_array_iter_return_current` answers: the value, a
/// reference followed, one more owner, or `false`.
fn owned_current(req: &mut Request, current: Option<Value>) -> Owner<Value> {
    let value = current.map_or(Value::bool(false), |value| value.deref());
    Owner::take(req, value)
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
#[php_function]
fn key(
    req: &mut Request,
    array: ArrayOrObject,
) -> Result<Option<Union<Owner<PhpString>, i64>>, Thrown> {
    let pointed = pointed(req, &array.value(), "key")?;
    let key = match &pointed {
        Pointed::Array(array) => array.cursor_key(),
        Pointed::Built(array) => array.cursor_key(),
        Pointed::Object(object) => object.pointer_row(req, None)?.map(|(key, _)| key),
    };
    let key = key.map(|key| key_return(req, &key));
    pointed.read(req);
    Ok(key)
}

/// What a pointer native reads: an array, a table made for the read,
/// or an object whose property table holds the pointer.
enum Pointed {
    /// The argument's array, or the table a class keeps in place of the
    /// object's (`sxe_get_properties`), a view.
    Array(PhpArray),
    /// A table made for this read, one owner the reader's, given up once
    /// it is read: a mover's copy of an array sent by value.
    Built(Owner<PhpArray>),
    Object(PhpObject),
}

impl Pointed {
    /// The read done: a table made for it given up, nothing else.
    fn read(self, req: &mut Request) {
        if let Pointed::Built(array) = self {
            array.give_up(req);
        }
    }
}

/// `Z_PARAM_ARRAY_OR_OBJECT`, then `get_ht_for_iap` over an object: the
/// deprecation, and the object, or the table its class's `get_properties`
/// keeps in place of the object's; the `TypeError` for anything else.
/// `SimpleXMLElement`'s table is built afresh on each call, so its
/// pointer stands at the first row at each:
///
/// ```php
/// $x = simplexml_load_string('<a><b>1</b><c>2</c></a>');
/// var_dump(@next($x), @key($x));
/// $f = new SplFixedArray(2); $f[0] = 5;
/// var_dump(@current($f), @current(new ArrayObject([7])));
/// ```
/// ```expect
/// string(1) "2"
/// string(1) "b"
/// bool(false)
/// bool(false)
/// ```
fn pointed(req: &mut Request, given: &Value, name: &str) -> Result<Pointed, Thrown> {
    match given.as_ref() {
        ValueRef::Array(array) => Ok(Pointed::Array(array)),
        ValueRef::Object(object) => {
            docref(
                req,
                Level::Deprecated,
                &format!("Calling {name}() on an object is deprecated"),
            );
            if let walk::Table::Array(array) = object.property_table(req)? {
                return Ok(Pointed::Array(array));
            }
            Ok(Pointed::Object(object))
        }
        _ => Err(not_an_array(req, given)),
    }
}

/// The `TypeError` `Z_PARAM_ARRAY_OR_OBJECT` raises, naming the array.
fn not_an_array(req: &mut Request, given: &Value) -> Thrown {
    let name = value_name(req, given);
    arg_type_error(req, 1, &format!("be of type array, {name} given"))
}

/// The four movers: the array the reference holds, separated as
/// `Z_PARAM_ARRAY_OR_OBJECT_EX(array_zv, 0, 1)` separates it, or the
/// object's table, which `get_ht_for_iap` separates; `false` where it
/// is empty, the pointer untouched; else the move, and the value it
/// lands on as `php_array_iter_return_current` reads it, which over an
/// object steps over a slot nothing holds and so moves the pointer
/// again, and which php reads only where the caller reads the return
/// (`USED_RET()`).
fn moved(
    req: &mut Request,
    given: &Ref<ArrayOrObject>,
    how: PointerMove,
) -> Result<Owner<Value>, Thrown> {
    let target = given.get().map_or_else(Value::null, Raw::value);
    let pointed = match target.as_ref() {
        ValueRef::Array(array) => match given.separated(req)? {
            Some(array) => Pointed::Array(array),
            // A value sent where a reference was asked for, past the
            // notice: php separates the argument slot, which nobody
            // reads after, so the move is over a copy.
            _ => Pointed::Built(array.copy(req)?),
        },
        _ => pointed(req, &target, move_name(how))?,
    };
    let mut array = match &pointed {
        Pointed::Array(array) => *array,
        Pointed::Built(array) => **array,
        Pointed::Object(object) => {
            if !object.pointer_move(req, how)? {
                return Ok(Owner::bool(false));
            }
            if !req.frames().return_value_used() {
                return Ok(Owner::null());
            }
            let current = object.pointer_row(req, Some(how))?.map(|(_, value)| value);
            return Ok(owned_current(req, current));
        }
    };
    let current = if array.is_empty() {
        Owner::bool(false)
    } else {
        match how {
            PointerMove::Reset => array.cursor_reset(),
            PointerMove::End => array.cursor_end(),
            PointerMove::Forward => array.cursor_next(),
            PointerMove::Backward => array.cursor_prev(),
        }
        owned_current(req, array.cursor_current())
    };
    pointed.read(req);
    Ok(current)
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
///
/// The declarations of the pointer natives and the extremes.
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
#[php_function(frameless(2))]
fn min(req: &mut Request, value: Value, values: Variadic<Value>) -> Result<Value, Thrown> {
    extreme(req, value, values, Ordering::Less)
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
///
/// The frameless form reads its two operands before anything else, the
/// second from a tail held in place, so it builds no `Vec`, and compares
/// the left with the right as `zend_compare(lhs, rhs)` does: a NaN on
/// either side answers neither way, so the right is kept, and equal
/// operands keep `max`'s left and `min`'s right.
///
/// ```php
/// var_dump(max(NAN, 1), max(1, NAN), min(NAN, 1), min(1, NAN));
/// var_dump(max(0, "a"), max("a", 0), min(null, false), min(false, null), max([1], 2), min("10", "9a"));
/// ```
/// ```expect
/// int(1)
/// float(NAN)
/// int(1)
/// float(NAN)
/// string(1) "a"
/// string(1) "a"
/// bool(false)
/// NULL
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// string(2) "10"
/// ```
#[php_function(frameless(2))]
fn max(req: &mut Request, value: Value, values: Variadic<Value>) -> Result<Value, Thrown> {
    extreme(req, value, values, Ordering::Greater)
}

/// The walk `min` and `max` share; `wanted` is the answer of
/// `compare(candidate, best)` that makes the candidate the new best.
fn extreme(
    req: &mut Request,
    value: Value,
    values: Variadic<Value>,
    wanted: Ordering,
) -> Result<Value, Thrown> {
    // The frameless arity's two operands, read before anything else.
    if let [other] = values[..]
        && req.frames().frameless()
    {
        return frameless_extreme(req, value, other, wanted);
    }
    if values.is_empty() {
        // One argument is the array to walk, held to `array` as php's
        // `IS_ARRAY` check past `Z_PARAM_VARIADIC` refuses it, where the
        // stub declares `mixed $value`.
        let only = coerce_param(req, &value, &Type::Array, 1)?;
        let Some(array) = only.as_array() else {
            return Ok(Value::null());
        };
        // `zend_hash_minmax`: the first element, replaced by a later one
        // only where it is strictly beyond.
        let mut best: Option<Value> = None;
        // Holding nothing: a comparison may serve `__toString`.
        walk::foreach(req, array, walk::Hold::None, |req, row| -> walk::Step {
            let value = row.value().deref();
            best = Some(match best {
                None => value,
                Some(held) if compare(req, &value, &held)? == wanted => value,
                Some(held) => held,
            });
            Ok(std::ops::ControlFlow::Continue(()))
        })?;
        let Some(best) = best else {
            return Err(arg_value_error(req, 1, "contain at least one element"));
        };
        return Ok(best);
    }
    let mut best = value;
    for value in values {
        if beyond(req, &value, &best, wanted)? {
            best = value;
        }
    }
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
    Ok(if left { lhs } else { rhs })
}
