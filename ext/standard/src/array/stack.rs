//! The natives of `array.c` that treat an array as a stack or a queue,
//! and the two that cut one: `array_push` and `array_pop` at its end,
//! `array_shift` and `array_unshift` at its start, `array_splice`,
//! which cuts the array a reference holds, and `array_slice`, which cuts
//! a copy. The four that write take the array by reference and write
//! the one the reference holds, separated first; where php rebuilds the
//! table in place (`array_shift`, `array_unshift`, `php_splice`), the
//! rebuilt table is written into the reference.

use php_ext::coerce::{coerce_param, to_array};
use php_ext::decl::{ConstExpr, FunctionDecl, Parameter, Type};
use php_ext::raising::{Thrown, throw, too_few_arguments};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpRef, Value, ValueRef};

use super::walk::array_argument;
use super::{copied, key_copy};
use crate::output::Entry;
use crate::{argument_count_between, bool_argument, declared, long_argument, parameter};

/// The declarations, as `basic_functions.stub.php` has them.
///
/// ```php
/// foreach (['array_push', 'array_pop', 'array_shift', 'array_unshift', 'array_splice', 'array_slice'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function array_push ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <optional> mixed ...$values ]
///   }
///   - Return [ int ]
/// }
/// Function [ <internal:standard> function array_pop ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> array &$array ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function array_shift ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> array &$array ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function array_unshift ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <optional> mixed ...$values ]
///   }
///   - Return [ int ]
/// }
/// Function [ <internal:standard> function array_splice ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <required> int $offset ]
///     Parameter #2 [ <optional> ?int $length = null ]
///     Parameter #3 [ <optional> mixed $replacement = [] ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function array_slice ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <required> int $offset ]
///     Parameter #2 [ <optional> ?int $length = null ]
///     Parameter #3 [ <optional> bool $preserve_keys = false ]
///   }
///   - Return [ array ]
/// }
/// ```
pub(super) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let by_ref_array = || Parameter {
        by_ref: true,
        ..parameter(b"array", Type::Array)
    };
    let values = || Parameter {
        variadic: true,
        ..parameter(b"values", Type::Mixed)
    };
    let length = || Parameter {
        default: Some(ConstExpr::Null),
        ..parameter(b"length", Type::Nullable(Box::new(Type::Int)))
    };
    vec![
        (
            declared(b"array_push", &[by_ref_array(), values()], Type::Int),
            array_push,
        ),
        (
            declared(b"array_pop", &[by_ref_array()], Type::Mixed),
            array_pop,
        ),
        (
            declared(b"array_shift", &[by_ref_array()], Type::Mixed),
            array_shift,
        ),
        (
            declared(b"array_unshift", &[by_ref_array(), values()], Type::Int),
            array_unshift,
        ),
        (
            declared(
                b"array_splice",
                &[
                    by_ref_array(),
                    parameter(b"offset", Type::Int),
                    length(),
                    Parameter {
                        default: Some(ConstExpr::Array(Vec::new())),
                        ..parameter(b"replacement", Type::Mixed)
                    },
                ],
                Type::Array,
            ),
            array_splice,
        ),
        (
            declared(
                b"array_slice",
                &[
                    parameter(b"array", Type::Array),
                    parameter(b"offset", Type::Int),
                    length(),
                    Parameter {
                        default: Some(ConstExpr::Bool(false)),
                        ..parameter(b"preserve_keys", Type::Bool)
                    },
                ],
                Type::Array,
            ),
            array_slice,
        ),
    ]
}

/// The array a by-reference argument holds, as `Z_PARAM_ARRAY_EX(array,
/// 0, 1)` separates it, with the cell to write a rebuilt table into; an
/// argument that came by value, past its notice, is a cell of its own
/// nobody reads after.
struct Stack {
    cell: PhpRef,
    array: PhpArray,
    owned: bool,
}

impl Stack {
    fn of(req: &mut Request, position: u32) -> Result<Stack, Thrown> {
        let given = req.argument(position).unwrap_or_default();
        let coerced = coerce_param(req, &given, &Type::Array, position)?;
        if let ValueRef::Reference(reference) = given.as_ref()
            && let Some(array) = reference.separate_array(req)?
        {
            return Ok(Stack {
                cell: *reference,
                array,
                owned: false,
            });
        }
        let array = match coerced.deref().as_array() {
            Some(array) => array.copy(req)?,
            None => PhpArray::new(req)?,
        };
        let cell = PhpRef::new(req, array.to_value())?;
        Ok(Stack {
            cell,
            array,
            owned: true,
        })
    }

    /// The rebuilt table written into the cell, the old one given up.
    fn replace(&mut self, req: &mut Request, rebuilt: PhpArray) {
        self.cell.set(req, rebuilt.to_value());
        self.array = rebuilt;
    }

    fn finish(self, req: &mut Request) {
        if self.owned {
            req.release(self.cell.as_value());
        }
    }
}

/// `array_push(array &$array, mixed ...$values): int`, php's
/// `PHP_FUNCTION(array_push)` (`Z/ext/standard/array.c:3520`): each value
/// appended at `nNextFreeElement`, and the count after. An array whose
/// next index is taken is php's `Error`.
///
/// ```php
/// $f = [1]; var_dump(array_push($f, 2, [3]), $f, array_push($f));
/// $h = [PHP_INT_MAX => 1]; try { array_push($h, 2); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   [2]=>
///   array(1) {
///     [0]=>
///     int(3)
///   }
/// }
/// int(3)
/// Cannot add element to the array as the next element is already occupied
/// ```
fn array_push(req: &mut Request) -> Result<Value, Thrown> {
    if req.argument_count() < 1 {
        return Err(too_few_arguments(req, 1, 0));
    }
    let mut stack = Stack::of(req, 1)?;
    let values = req.variadic_arguments(2)?;
    for value in values {
        req.addref(&value);
        if let Err(thrown) = stack.array.append(req, value) {
            stack.finish(req);
            return Err(thrown);
        }
    }
    let count = i64::try_from(stack.array.len()).unwrap_or(i64::MAX);
    stack.finish(req);
    Ok(Value::int(count))
}

/// `array_pop(array &$array): mixed`, php's `PHP_FUNCTION(array_pop)`
/// (`Z/ext/standard/array.c:3549`): the last element taken out and
/// handed back, a reference unwrapped, `null` for an empty array;
/// `nNextFreeElement` moved back one where the element's key was the
/// last index appended, and the pointer reset.
///
/// ```php
/// $a = [1, 2, 3]; var_dump(array_pop($a)); $a[] = 'x'; var_dump($a);
/// $b = [0, 1, 2, 3]; unset($b[3]); var_dump(array_pop($b)); $b[] = 'y'; var_dump(array_keys($b));
/// $e = []; var_dump(array_pop($e));
/// $g = [5 => 'a', 6 => 'b']; next($g); array_pop($g); var_dump(current($g));
/// $x = 1; $r = []; $r[0] = &$x; $v = array_pop($r); $v = 5; var_dump($x);
/// ```
/// ```expect
/// int(3)
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   [2]=>
///   string(1) "x"
/// }
/// int(2)
/// array(3) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(1)
///   [2]=>
///   int(4)
/// }
/// NULL
/// string(1) "a"
/// int(1)
/// ```
fn array_pop(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let mut stack = Stack::of(req, 1)?;
    let Some((key, value)) = stack.array.snapshot_refs().pop() else {
        stack.finish(req);
        return Ok(Value::null());
    };
    let value = unwrapped(req, value);
    let popped = stack.array.unset(req, &key);
    if let Key::Int(index) = key
        && index == stack.array.next_free().wrapping_sub(1)
    {
        stack.array.set_next_free(req, index)?;
    }
    stack.array.cursor_reset();
    stack.finish(req);
    popped?;
    Ok(value)
}

/// `RETVAL_COPY_VALUE` of an element about to be deleted, then
/// `zend_unwrap_reference`: the value with one more owner, which the
/// delete gives back, and a reference's value in place of the reference.
fn unwrapped(req: &mut Request, value: Value) -> Value {
    let value = value.deref();
    req.addref(&value);
    value
}

/// `array_shift(array &$array): mixed`, php's `PHP_FUNCTION(array_shift)`
/// (`Z/ext/standard/array.c:3620`): the first element taken out and
/// handed back, the integer keys renumbered from zero and string keys
/// kept, `nNextFreeElement` the count of integer keys, and the pointer
/// reset.
///
/// ```php
/// $c = ["a" => 1, 5 => 2, "b" => 3, 9 => 4]; var_dump(array_shift($c), $c); $c[] = 'z'; var_dump(array_keys($c));
/// $e = []; var_dump(array_shift($e));
/// $q = [1, 2]; $copy = $q; array_shift($q); var_dump(count($copy));
/// ```
/// ```expect
/// int(1)
/// array(3) {
///   [0]=>
///   int(2)
///   ["b"]=>
///   int(3)
///   [1]=>
///   int(4)
/// }
/// array(4) {
///   [0]=>
///   int(0)
///   [1]=>
///   string(1) "b"
///   [2]=>
///   int(1)
///   [3]=>
///   int(2)
/// }
/// NULL
/// int(2)
/// ```
fn array_shift(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let mut stack = Stack::of(req, 1)?;
    let rows = stack.array.snapshot_refs();
    let Some((_, first)) = rows.first().cloned() else {
        stack.finish(req);
        return Ok(Value::null());
    };
    let value = unwrapped(req, first);
    let mut rebuilt = PhpArray::new(req)?;
    for (key, element) in rows.into_iter().skip(1) {
        req.addref(&element);
        match key {
            Key::Str(_) => {
                let key = key_copy(req, &key);
                rebuilt.set(req, key, element)?;
            }
            Key::Int(_) => rebuilt.append(req, element)?,
        }
    }
    if rebuilt.next_free() == i64::MIN {
        rebuilt.set_next_free(req, 0)?;
    }
    stack.replace(req, rebuilt);
    stack.finish(req);
    Ok(value)
}

/// `array_unshift(array &$array, mixed ...$values): int`, php's
/// `PHP_FUNCTION(array_unshift)` (`Z/ext/standard/array.c:3738`): a new
/// table of the values and then the array's elements, integer keys
/// renumbered and string keys kept, written in place of the old, and
/// the count after.
///
/// ```php
/// $d = [3 => 'p', 'k' => 'q']; var_dump(array_unshift($d, 'u', 'v'), $d, array_unshift($d));
/// ```
/// ```expect
/// int(4)
/// array(4) {
///   [0]=>
///   string(1) "u"
///   [1]=>
///   string(1) "v"
///   [2]=>
///   string(1) "p"
///   ["k"]=>
///   string(1) "q"
/// }
/// int(4)
/// ```
fn array_unshift(req: &mut Request) -> Result<Value, Thrown> {
    if req.argument_count() < 1 {
        return Err(too_few_arguments(req, 1, 0));
    }
    let mut stack = Stack::of(req, 1)?;
    let values = req.variadic_arguments(2)?;
    let mut rebuilt = PhpArray::new(req)?;
    for value in values {
        req.addref(&value);
        rebuilt.append(req, value)?;
    }
    for (key, element) in stack.array.snapshot_refs() {
        req.addref(&element);
        match key {
            Key::Str(_) => {
                let key = key_copy(req, &key);
                rebuilt.set(req, key, element)?;
            }
            Key::Int(_) => rebuilt.append(req, element)?,
        }
    }
    let count = i64::try_from(rebuilt.len()).unwrap_or(i64::MAX);
    stack.replace(req, rebuilt);
    stack.finish(req);
    Ok(Value::int(count))
}

/// `array_splice(array &$array, int $offset, ?int $length = null, mixed
/// $replacement = []): array`, php's `PHP_FUNCTION(array_splice)` over
/// `php_splice` (`Z/ext/standard/array.c:3312`): the elements from
/// `$offset` for `$length` removed and handed back as a list, the
/// replacement's values put in their place, and the array rebuilt with
/// its integer keys renumbered and its string keys kept. A negative
/// offset counts from the end, a negative length stops that far from
/// it, and a replacement that is not an array is cast to one.
///
/// ```php
/// $a = ["a" => 1, 2, 3, 4, "b" => 5]; var_dump(array_splice($a, 1, 2, ["x", "y", "z"]), $a);
/// $b = [1, 2, 3, 4, 5]; var_dump(array_splice($b, -2), $b);
/// $c = [1, 2, 3, 4, 5]; array_splice($c, 1, -2, "only"); var_dump($c);
/// $d = [1, 2]; var_dump(array_splice($d, 5), array_splice($d, -9, 1));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(2)
///   [1]=>
///   int(3)
/// }
/// array(6) {
///   ["a"]=>
///   int(1)
///   [0]=>
///   string(1) "x"
///   [1]=>
///   string(1) "y"
///   [2]=>
///   string(1) "z"
///   [3]=>
///   int(4)
///   ["b"]=>
///   int(5)
/// }
/// array(2) {
///   [0]=>
///   int(4)
///   [1]=>
///   int(5)
/// }
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   [2]=>
///   int(3)
/// }
/// array(4) {
///   [0]=>
///   int(1)
///   [1]=>
///   string(4) "only"
///   [2]=>
///   int(4)
///   [3]=>
///   int(5)
/// }
/// array(0) {
/// }
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// ```
fn array_splice(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 4)?;
    let mut stack = Stack::of(req, 1)?;
    let offset = long_argument(req, 2)?;
    let length = if req.argument_count() >= 3 {
        let given = req.argument(3).unwrap_or_default();
        let coerced = coerce_param(req, &given, &Type::Nullable(Box::new(Type::Int)), 3)?;
        coerced.as_int()
    } else {
        None
    };
    let replacement = if req.argument_count() == 4 {
        let given = req.argument(4).unwrap_or_default().deref();
        Some(match given.as_array() {
            Some(array) => {
                req.addref(&given);
                *array
            }
            None => to_array(req, &given)?,
        })
    } else {
        None
    };
    let rows = stack.array.snapshot_refs();
    let num_in = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    let length = length.unwrap_or(num_in);
    let (start, taken) = clamp_splice(num_in, offset, length);
    let mut removed = PhpArray::new(req)?;
    let mut rebuilt = PhpArray::new(req)?;
    for (at, (key, element)) in rows.into_iter().enumerate() {
        let at = i64::try_from(at).unwrap_or(i64::MAX);
        if at == start
            && let Some(replacement) = replacement
        {
            for (_, value) in replacement.snapshot_refs() {
                req.addref(&value);
                rebuilt.append(req, value)?;
            }
        }
        if at >= start && at < start + taken {
            req.addref(&element);
            removed.append(req, element)?;
            continue;
        }
        req.addref(&element);
        match key {
            Key::Str(_) => {
                let key = key_copy(req, &key);
                rebuilt.set(req, key, element)?;
            }
            Key::Int(_) => rebuilt.append(req, element)?,
        }
    }
    if start >= num_in
        && let Some(replacement) = replacement
    {
        for (_, value) in replacement.snapshot_refs() {
            req.addref(&value);
            rebuilt.append(req, value)?;
        }
    }
    if let Some(replacement) = replacement {
        req.release(replacement.to_value());
    }
    if rebuilt.next_free() == i64::MIN {
        rebuilt.set_next_free(req, 0)?;
    }
    stack.replace(req, rebuilt);
    stack.finish(req);
    Ok(Value::array(removed))
}

/// `php_splice`'s clamps: the offset into `0..=num_in`, from the end
/// where it is negative, and the length to what is left, stopping short
/// of the end where it is negative.
fn clamp_splice(num_in: i64, offset: i64, length: i64) -> (i64, i64) {
    let offset = if offset > num_in {
        num_in
    } else if offset < 0 {
        (num_in + offset).max(0)
    } else {
        offset
    };
    let length = if length < 0 {
        num_in - offset + length
    } else if offset.unsigned_abs().saturating_add(length.unsigned_abs()) > num_in.unsigned_abs() {
        num_in - offset
    } else {
        length
    };
    (offset, length.max(0))
}

/// `array_slice(array $array, int $offset, ?int $length = null, bool
/// $preserve_keys = false): array`, php's `PHP_FUNCTION(array_slice)`
/// (`Z/ext/standard/array.c:3909`): the elements from `$offset` for
/// `$length`, clamped as `array_splice` clamps, string keys kept and
/// integer keys renumbered unless `$preserve_keys`; an offset past the
/// end, or a length that leaves nothing, is the empty array.
///
/// ```php
/// $a = ["a" => 1, 5 => 2, 3, "b" => 4, 7 => 5];
/// var_dump(array_slice($a, 1, 3), array_slice($a, -2, 1, true), array_slice($a, 9), array_slice($a, 1, -3), array_slice([1, 2, 3], 0, null, true));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   int(2)
///   [1]=>
///   int(3)
///   ["b"]=>
///   int(4)
/// }
/// array(1) {
///   ["b"]=>
///   int(4)
/// }
/// array(0) {
/// }
/// array(1) {
///   [0]=>
///   int(2)
/// }
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   [2]=>
///   int(3)
/// }
/// ```
fn array_slice(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 4)?;
    let input = array_argument(req, 1)?;
    let offset = long_argument(req, 2)?;
    let length = if req.argument_count() >= 3 {
        let given = req.argument(3).unwrap_or_default();
        let coerced = coerce_param(req, &given, &Type::Nullable(Box::new(Type::Int)), 3)?;
        coerced.as_int()
    } else {
        None
    };
    let preserve = if req.argument_count() == 4 {
        bool_argument(req, 4)?
    } else {
        false
    };
    let num_in = i64::try_from(input.len()).unwrap_or(i64::MAX);
    if offset > num_in {
        return Ok(Value::array(PhpArray::new(req)?));
    }
    let (start, taken) = clamp_splice(num_in, offset, length.unwrap_or(num_in));
    if taken <= 0 {
        return Ok(Value::array(PhpArray::new(req)?));
    }
    let mut out = PhpArray::new(req)?;
    let rows = input.snapshot_refs();
    let from = usize::try_from(start).unwrap_or(usize::MAX);
    let count = usize::try_from(taken).unwrap_or(usize::MAX);
    for (key, element) in rows.into_iter().skip(from).take(count) {
        let value = copied(req, element);
        match key {
            Key::Int(_) if !preserve => out.append(req, value)?,
            _ => {
                let key = key_copy(req, &key);
                out.set(req, key, value)?;
            }
        }
    }
    Ok(Value::array(out))
}

/// php's `Error` for an append past `ZEND_LONG_MAX`.
#[allow(dead_code)]
fn occupied(req: &mut Request) -> Thrown {
    throw(
        req,
        "Error",
        "Cannot add element to the array as the next element is already occupied",
    )
}

/// The cell, the array it holds and whether the cell is this native's
/// own, for a native in another file of `array` that writes a rebuilt
/// table.
pub(super) fn cell_array(
    req: &mut Request,
    position: u32,
) -> Result<(PhpRef, PhpArray, bool), Thrown> {
    let stack = Stack::of(req, position)?;
    Ok((stack.cell, stack.array, stack.owned))
}
