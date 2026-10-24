//! The natives of `array.c` that read an array's keys and values into
//! a new one, or answer about them: the searches (`in_array`,
//! `array_search`, `array_key_exists` and its alias `key_exists`), the
//! readers of one end (`array_key_first`, `array_key_last`,
//! `array_first`, `array_last`), and the builders that keep, flip,
//! count, recase, pair, fill, pad, cut, pick a column out of, reverse,
//! sum or multiply what they are given.

use std::cmp::Ordering;
use std::ops::ControlFlow;

use php_ext::abi::{OrFalse, Passed, PropertyRead, PropertyTest, Raw, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::{
    BinaryOp, binary_op, float_to_long, identical, loose_eq, numeric_string, to_string,
};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, argument_error, deprecated, docref,
    throw_error, warning,
};
use php_ext::request::Request;
use php_ext::values::walk::Hold;
use php_ext::values::{Key, Layout, Owner, PhpArray, PhpString, TypeTag, Value, ValueRef, walk};

use super::sort::key_value;
use super::{copied, give_up_rows, returned};
use crate::string::KeyBytes;

/// The comparison a search makes: `fast_is_identical_function` under
/// `$strict`, `fast_equal_check_function` otherwise, the element's
/// reference followed.
fn matches(
    req: &mut Request,
    needle: &Value,
    element: &Value,
    strict: bool,
) -> Result<bool, Thrown> {
    let element = element.deref();
    if strict {
        return identical(req, needle, &element);
    }
    loose_eq(req, needle, &element)
}

/// `_php_search_array`: the key of the first element that matches.
fn search(
    req: &mut Request,
    needle: &Value,
    haystack: PhpArray,
    strict: bool,
) -> Result<Option<Key>, Thrown> {
    // `ZEND_HASH_FOREACH_KEY_VAL`, holding nothing: `==` may serve
    // `__toString`.
    walk::foreach(req, haystack, Hold::None, |req, row| -> walk::Step<Key> {
        if matches(req, needle, row.value(), strict)? {
            return Ok(ControlFlow::Break(row.key().clone()));
        }
        Ok(ControlFlow::Continue(()))
    })
}

/// `in_array(mixed $needle, array $haystack, bool $strict = false): bool`,
/// php's `PHP_FUNCTION(in_array)` (`Z/ext/standard/array.c:1726`) and its
/// frameless arities 2 and 3: whether an element equals the needle, `==`
/// or under `$strict` `===`.
///
/// ```php
/// var_dump(in_array("1e1", ["10"]), in_array("1e1", ["10"], true), in_array(0, ["a"]), in_array(null, [0]), in_array("abc", [0]), in_array([1], [[1]], true));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
///
/// `===` follows a reference inside an element as the operator does,
/// and walks an array that holds itself under php's protection, whose
/// `Error` ends the search.
///
/// ```php
/// $x = 1; $a = [&$a];
/// var_dump(in_array([1], [[&$x]], true));
/// try { in_array($a, [[[]]], true); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// Nesting level too deep - recursive dependency?
/// ```
///
/// The declarations Reflection is asked for most.
///
/// ```php
/// foreach (['in_array', 'array_search', 'array_keys', 'array_key_exists', 'key_exists', 'array_column', 'array_pad', 'array_fill'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function in_array ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> mixed $needle ]
///     Parameter #1 [ <required> array $haystack ]
///     Parameter #2 [ <optional> bool $strict = false ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function array_search ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> mixed $needle ]
///     Parameter #1 [ <required> array $haystack ]
///     Parameter #2 [ <optional> bool $strict = false ]
///   }
///   - Return [ string|int|false ]
/// }
/// Function [ <internal:standard> function array_keys ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <optional> mixed $filter_value = <default> ]
///     Parameter #2 [ <optional> bool $strict = false ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function array_key_exists ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> $key ]
///     Parameter #1 [ <required> array $array ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function key_exists ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> $key ]
///     Parameter #1 [ <required> array $array ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function array_column ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <required> string|int|null $column_key ]
///     Parameter #2 [ <optional> string|int|null $index_key = null ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function array_pad ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <required> int $length ]
///     Parameter #2 [ <required> mixed $value ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function array_fill ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> int $start_index ]
///     Parameter #1 [ <required> int $count ]
///     Parameter #2 [ <required> mixed $value ]
///   }
///   - Return [ array ]
/// }
/// ```
#[php_function(frameless(2, 3))]
fn in_array(
    req: &mut Request,
    needle: Value,
    haystack: PhpArray,
    #[php(default = false)] strict: bool,
) -> Result<bool, Thrown> {
    Ok(search(req, &needle, haystack, strict)?.is_some())
}

/// `array_search(mixed $needle, array $haystack, bool $strict = false):
/// int|string|false`, php's `PHP_FUNCTION(array_search)`
/// (`Z/ext/standard/array.c:1759`): the key of the first element that
/// matches, or `false`.
///
/// ```php
/// var_dump(array_search("green", ["blue", "red", "k" => "green", "green"]), array_search("1", [0, 1]), array_search(1, ["1"], true));
/// try { array_search(1, "x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "k"
/// int(1)
/// bool(false)
/// array_search(): Argument #2 ($haystack) must be of type array, string given
/// ```
#[php_function]
fn array_search(
    req: &mut Request,
    needle: Value,
    haystack: PhpArray,
    #[php(default = false)] strict: bool,
) -> Result<OrFalse<Union<Owner<PhpString>, i64>>, Thrown> {
    match search(req, &needle, haystack, strict)? {
        Some(key) => Ok(OrFalse::Value(key_return(req, &key))),
        None => Ok(OrFalse::False),
    }
}

/// `array_keys(array $array, mixed $filter_value = UNKNOWN, bool $strict
/// = false): array`, php's `PHP_FUNCTION(array_keys)`
/// (`Z/ext/standard/array.c:4424`): every key in order, or with a filter
/// value the keys of the elements that match it.
///
/// ```php
/// var_dump(array_keys(["a" => 1, 2 => "x", "b" => "1"]), array_keys(["a" => 1, 2 => "x", "b" => "1"], 1), array_keys(["a" => 1, "b" => "1"], "1", true), array_keys([]));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   int(2)
///   [2]=>
///   string(1) "b"
/// }
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
/// }
/// array(1) {
///   [0]=>
///   string(1) "b"
/// }
/// array(0) {
/// }
/// ```
#[php_function]
fn array_keys(
    req: &mut Request,
    array: PhpArray,
    filter_value: Passed<Value>,
    #[php(default = false)] strict: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let filter = match filter_value {
        Passed::Given(filter) => Some(filter),
        Passed::Omitted => None,
    };
    if array.is_empty() {
        return Ok(Owner::take(req, array));
    }
    let mut out = PhpArray::new(req)?;
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        if let Some(filter) = &filter
            && !matches(req, filter, row.value(), strict)?
        {
            return Ok(ControlFlow::Continue(()));
        }
        let value = key_value(row.key());
        out.append(req, Owner::take(req, value))?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(out)
}

/// `array_key_first(array $array): int|string|null`, php's
/// `PHP_FUNCTION(array_key_first)` (`Z/ext/standard/array.c:4507`): the
/// first key, `null` for an empty array; the internal pointer is not
/// read.
///
/// ```php
/// $a = ["x" => 1, 3 => 2]; next($a); var_dump(array_key_first($a), array_key_first([]));
/// ```
/// ```expect
/// string(1) "x"
/// NULL
/// ```
#[php_function]
fn array_key_first(req: &mut Request, array: PhpArray) -> Option<Union<Owner<PhpString>, i64>> {
    let first = walk::rows(req, array).next();
    key_or_null(req, first.map(|(key, _)| key))
}

/// `array_key_last(array $array): int|string|null`, php's
/// `PHP_FUNCTION(array_key_last)` (`Z/ext/standard/array.c:4522`): the
/// last key, `null` for an empty array.
///
/// ```php
/// var_dump(array_key_last(["x" => 1, 3 => 2]), array_key_last([]));
/// ```
/// ```expect
/// int(3)
/// NULL
/// ```
#[php_function]
fn array_key_last(req: &mut Request, array: PhpArray) -> Option<Union<Owner<PhpString>, i64>> {
    let last = walk::rows(req, array).last();
    key_or_null(req, last.map(|(key, _)| key))
}

fn key_or_null(req: &mut Request, key: Option<Key>) -> Option<Union<Owner<PhpString>, i64>> {
    key.map(|key| key_return(req, &key))
}

/// A key as the return, `string|int`, a string one more owner.
pub(super) fn key_return(req: &mut Request, key: &Key) -> Union<Owner<PhpString>, i64> {
    match key {
        Key::Str(s) => Union::A(Owner::take(req, *s)),
        Key::Int(n) => Union::B(*n),
    }
}

/// `array_first(array $array): mixed`, php's `PHP_FUNCTION(array_first)`
/// (`Z/ext/standard/array.c:4537`): the first value, a reference
/// followed, `null` for an empty array.
///
/// ```php
/// var_dump(array_first(["x" => 1, 3 => 2]), array_first([]));
/// ```
/// ```expect
/// int(1)
/// NULL
/// ```
#[php_function]
fn array_first(req: &mut Request, array: PhpArray) -> Value {
    let first = walk::rows(req, array).next();
    value_or_null(first.map(|(_, value)| value))
}

/// `array_last(array $array): mixed`, php's `PHP_FUNCTION(array_last)`
/// (`Z/ext/standard/array.c:4550`): the last value, `null` for an empty
/// array.
///
/// ```php
/// var_dump(array_last(["x" => 1, 3 => [2]]), array_last([]));
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   int(2)
/// }
/// NULL
/// ```
#[php_function]
fn array_last(req: &mut Request, array: PhpArray) -> Value {
    let last = walk::rows(req, array).last();
    value_or_null(last.map(|(_, value)| value))
}

fn value_or_null(value: Option<Value>) -> Value {
    value.map_or_else(Value::null, |value| value.deref())
}

/// `array_values(array $array): array`, php's
/// `PHP_FUNCTION(array_values)` (`Z/ext/standard/array.c:4564`): the
/// values as a list, a list handed back as it is and a reference with
/// one owner followed (`zend_array_to_list`).
///
/// ```php
/// var_dump(array_values(["x" => "a", 5 => "b"]), array_values([]), array_values([1, 2]));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
/// }
/// array(0) {
/// }
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// ```
#[php_function]
fn array_values(req: &mut Request, array: PhpArray) -> Result<Owner<PhpArray>, Thrown> {
    if array.is_empty() {
        return Ok(Owner::take(req, PhpArray::empty()));
    }
    if array.is_list() && array.next_free() == i64::try_from(array.len()).unwrap_or(i64::MAX) {
        return Ok(Owner::take(req, array));
    }
    let mut out = PhpArray::new(req)?;
    // `zend_array_to_list`'s `ZEND_HASH_FOREACH_VAL` (`Z/Zend/zend_hash.c:2543`),
    // no owner of the table taken: the argument holds it.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let value = copied(req, *row.value());
        out.append(req, value)?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(out)
}

/// `array_count_values(array $array): array`, php's
/// `PHP_FUNCTION(array_count_values)` (`Z/ext/standard/array.c:4593`):
/// each int or string value as a key, in the order first seen, and how
/// often it stands; a numeric string counts with the int it is as a
/// key, and any other type is a warning and skipped.
///
/// ```php
/// var_dump(array_count_values([1, "hello", 1, "world", "hello", "1", 1.5, true]));
/// ```
/// ```expect
/// Warning: array_count_values(): Can only count string and integer values, entry skipped in %s on line 2
///
/// Warning: array_count_values(): Can only count string and integer values, entry skipped in %s on line 2
/// array(3) {
///   [1]=>
///   int(3)
///   ["hello"]=>
///   int(2)
///   ["world"]=>
///   int(1)
/// }
/// ```
#[php_function]
fn array_count_values(req: &mut Request, array: PhpArray) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_VAL(myht, entry)` (`Z/ext/standard/array.c:4609`),
    // no owner of the table taken: the argument holds it while the warning
    // reaches the program.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let element = *row.value();
        let element = element.deref();
        let key = match element.as_ref() {
            ValueRef::Int(n) => Key::Int(n),
            ValueRef::Str(s) => Key::lookup(s),
            _ => {
                docref(
                    req,
                    Level::Warning,
                    "Can only count string and integer values, entry skipped",
                );
                return Ok(ControlFlow::Continue(()));
            }
        };
        let count = out.get(&key).and_then(|v| v.as_int()).unwrap_or(0);
        let (key, count) = (Owner::take(req, key), Owner::int(count + 1));
        out.set(req, key, count)?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(out)
}

/// The key a value makes where php writes it with
/// `zend_symtable_update`: an int as itself, anything else as its
/// string, a canonical decimal read back as an int.
fn symtable_key(req: &mut Request, value: &Value) -> Result<Owner<Key>, Thrown> {
    if let Some(n) = value.as_int() {
        return Ok(Owner::index(n));
    }
    if let Some(s) = value.as_php_string() {
        return Ok(Owner::take(req, Key::lookup(s)));
    }
    let text = to_string(req, value)?;
    Ok(Key::from_string(req, text))
}

/// `array_flip(array $array): array`, php's `PHP_FUNCTION(array_flip)`
/// (`Z/ext/standard/array.c:4878`): each int or string value a key, the
/// key it stood under its value, a later duplicate writing over an
/// earlier; any other type is a warning and skipped.
///
/// ```php
/// var_dump(array_flip(["a", "b", "x" => "a", 5 => "10", 1.5]));
/// ```
/// ```expect
/// Warning: array_flip(): Can only flip string and integer values, entry skipped in %s on line 2
/// array(3) {
///   ["a"]=>
///   string(1) "x"
///   ["b"]=>
///   int(1)
///   [10]=>
///   int(5)
/// }
/// ```
#[php_function]
fn array_flip(req: &mut Request, array: PhpArray) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(array), …)`
    // (`Z/ext/standard/array.c:4890`), no owner of the table taken: the
    // argument holds it while the warning reaches the program.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let element = *row.value();
        let element = element.deref();
        if !matches!(element.as_ref(), ValueRef::Int(_) | ValueRef::Str(_)) {
            docref(
                req,
                Level::Warning,
                "Can only flip string and integer values, entry skipped",
            );
            return Ok(ControlFlow::Continue(()));
        }
        let new_key = symtable_key(req, &element)?;
        let data = Owner::take(req, key_value(&key));
        out.set(req, new_key, data)?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(out)
}

/// `array_change_key_case(array $array, int $case = CASE_LOWER): array`,
/// php's `PHP_FUNCTION(array_change_key_case)`
/// (`Z/ext/standard/array.c:4914`): each string key lowered, or raised
/// where `$case` is anything but zero, in ASCII, a later key writing
/// over an earlier one that folds the same.
///
/// ```php
/// var_dump(array_change_key_case(["FirSt" => 1, "SecOnd" => 4, 5 => "x", "first" => 2]), array_change_key_case(["a" => 1, "Ä" => 2], CASE_UPPER));
/// ```
/// ```expect
/// array(3) {
///   ["first"]=>
///   int(2)
///   ["second"]=>
///   int(4)
///   [5]=>
///   string(1) "x"
/// }
/// array(2) {
///   ["A"]=>
///   int(1)
///   ["Ä"]=>
///   int(2)
/// }
/// ```
#[php_function]
fn array_change_key_case(
    req: &mut Request,
    array: PhpArray,
    #[php(default = CASE_LOWER)] case: i64,
) -> Result<Owner<PhpArray>, Thrown> {
    let upper = case != 0;
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(array), …)`
    // (`Z/ext/standard/array.c:4930`), no owner of the table taken: the
    // argument holds it.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let key = match key {
            Key::Int(n) => Owner::index(n),
            Key::Str(s) => {
                let folded: Vec<u8> = s
                    .as_bytes()
                    .iter()
                    .map(|b| {
                        if upper {
                            b.to_ascii_uppercase()
                        } else {
                            b.to_ascii_lowercase()
                        }
                    })
                    .collect();
                PhpString::new(req, &folded)?.into_key()
            }
        };
        let value = copied(req, *row.value());
        out.set(req, key, value)?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(out)
}

/// `array_fill(int $start_index, int $count, mixed $value): array`, php's
/// `PHP_FUNCTION(array_fill)` (`Z/ext/standard/array.c:2754`): `$count`
/// copies of the value under the keys from `$start_index` on.
///
/// ```php
/// var_dump(array_fill(5, 3, 'x'), array_fill(-3, 2, 0), array_fill(0, 0, 1));
/// try { array_fill(0, -1, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { array_fill(0, 2147483648, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { array_fill(PHP_INT_MAX, 2, 1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(3) {
///   [5]=>
///   string(1) "x"
///   [6]=>
///   string(1) "x"
///   [7]=>
///   string(1) "x"
/// }
/// array(2) {
///   [-3]=>
///   int(0)
///   [-2]=>
///   int(0)
/// }
/// array(0) {
/// }
/// array_fill(): Argument #2 ($count) must be greater than or equal to 0
/// array_fill(): Argument #2 ($count) is too large
/// Cannot add element to the array as the next element is already occupied
/// ```
///
/// The count too large is `zend_argument_value_error`'s, so its two
/// printed buffers stand across the throw and it peaks where php's does:
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
/// pk(function () { try { array_fill(0, PHP_INT_MAX, 1); } catch (ValueError $e) {} });
/// ```
/// ```expect
/// 2904 32
/// ```
///
/// Since 8.5.12 the table is asked for fallibly: past the memory limit it
/// is refused, and the limit's fatal is raised as the call returns.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(count(array_fill(0, 1 << 20, 0)), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 16793600 bytes) in %s on line 3
/// ```
#[php_function]
fn array_fill(
    req: &mut Request,
    start_index: i64,
    count: i64,
    value: Value,
) -> Result<Owner<PhpArray>, Thrown> {
    let start = start_index;
    if count < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    if count == 0 {
        return Ok(Owner::take(req, PhpArray::empty()));
    }
    if count > i64::from(i32::MAX) {
        return Err(argument_error(req, "ValueError", 2, "is too large"));
    }
    if start > i64::MAX - count + 1 {
        return Err(throw_error(
            req,
            "Error",
            "Cannot add element to the array as the next element is already occupied",
        ));
    }
    // `array_init_size`: packed from zero for a start inside the count,
    // `start + count` slots, and a hash of `count` otherwise.
    let (size, layout) = if (0..count).contains(&start) {
        (start + count, Layout::Packed)
    } else {
        (count, Layout::Hash)
    };
    // `array_finit_size_packed` and `array_finit_size_mixed` since
    // 8.5.12: the table asked for fallibly, `RETURN_OOM()` where the
    // memory limit refused it.
    let size = u32::try_from(size).unwrap_or(u32::MAX);
    let Some(mut out) = req
        .memory_mut()
        .fallible(|req| PhpArray::sized(req, size, layout))?
    else {
        return Ok(Owner::take(req, PhpArray::empty()));
    };
    for at in 0..count {
        out.set(req, Owner::index(start + at), Owner::take(req, value))?;
    }
    Ok(out)
}

/// `array_fill_keys(array $keys, mixed $value): array`, php's
/// `PHP_FUNCTION(array_fill_keys)` (`Z/ext/standard/array.c:2819`): each
/// element of `$keys` a key of the value, an int as itself and anything
/// else as its string.
///
/// ```php
/// var_dump(array_fill_keys(["a", 5, "10", 1.5, true], 0));
/// ```
/// ```expect
/// array(5) {
///   ["a"]=>
///   int(0)
///   [5]=>
///   int(0)
///   [10]=>
///   int(0)
///   ["1.5"]=>
///   int(0)
///   [1]=>
///   int(0)
/// }
/// ```
#[php_function]
fn array_fill_keys(
    req: &mut Request,
    keys: PhpArray,
    value: Value,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(keys), entry)`
    // (`Z/ext/standard/array.c:2831`), no owner of the table taken: the
    // argument holds it while a key's `__toString` runs.
    walk::foreach(req, keys, Hold::None, |req, row| -> walk::Step {
        let element = *row.value();
        let element = element.deref();
        let key = symtable_key(req, &element)?;
        out.set(req, key, Owner::take(req, value))?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(out)
}

/// `array_pad(array $array, int $length, mixed $value): array`, php's
/// `PHP_FUNCTION(array_pad)` (`Z/ext/standard/array.c:4788`): the array
/// padded with the value to `|$length|` elements, at the end where it is
/// positive and at the start where it is negative, integer keys
/// renumbered and string keys kept; an array already that long handed
/// back as it is.
///
/// ```php
/// var_dump(array_pad([5 => 1, "k" => 2], 4, 0), array_pad([1], -3, "p"), array_pad([1, 2], 1, 0));
/// try { array_pad([], 1073741825, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   int(1)
///   ["k"]=>
///   int(2)
///   [1]=>
///   int(0)
///   [2]=>
///   int(0)
/// }
/// array(3) {
///   [0]=>
///   string(1) "p"
///   [1]=>
///   string(1) "p"
///   [2]=>
///   int(1)
/// }
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// array_pad(): Argument #2 ($length) must not exceed the maximum allowed array size
/// ```
///
/// Since 8.5.12 a packed input's table is asked for fallibly, and each
/// insert into a hash: past the memory limit the ask is refused, and the
/// limit's fatal is raised as the call returns.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(count(array_pad([1], 1 << 20, 0)), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 16793600 bytes) in %s on line 3
/// ```
///
/// A hash, padded on the left:
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(count(array_pad(['a' => 1], -(1 << 20), 0)), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 16793600 bytes) in %s on line 3
/// ```
#[php_function]
fn array_pad(
    req: &mut Request,
    array: PhpArray,
    length: i64,
    value: Value,
) -> Result<Owner<PhpArray>, Thrown> {
    let (input, size, pad) = (array, length, value);
    let max = super::HT_MAX_SIZE.cast_signed();
    if !(-max..=max).contains(&size) {
        return Err(arg_value_error(
            req,
            2,
            "not exceed the maximum allowed array size",
        ));
    }
    let have = i64::try_from(input.len()).unwrap_or(i64::MAX);
    if have >= size.abs() {
        return Ok(Owner::take(req, input));
    }
    let pads = size.abs() - have;
    // `array_init_size(return_value, pad_size_abs)`, and since 8.5.12
    // its packed table, or each insert into a hash, asked for fallibly:
    // where the memory limit refuses one the native returns
    // (`RETURN_OOM()`).
    let abs = u32::try_from(size.abs()).unwrap_or(u32::MAX);
    let mut out = if input.is_packed() {
        let laid = req
            .memory_mut()
            .fallible(|req| PhpArray::sized(req, abs, Layout::Packed))?;
        let Some(out) = laid else {
            return Ok(Owner::take(req, PhpArray::empty()));
        };
        out
    } else {
        PhpArray::init_size(req, abs)?
    };
    let packed = input.is_packed();
    // Whether an insert was refused.
    let insert = |req: &mut Request,
                  out: &mut Owner<PhpArray>,
                  key: Option<Owner<Key>>,
                  value: Owner<Value>|
     -> Result<bool, Thrown> {
        let written = |req: &mut Request| match key {
            Some(key) => out.set(req, key, value),
            None => out.append(req, value),
        };
        if packed {
            written(req)?;
            return Ok(false);
        }
        Ok(req.memory_mut().fallible(written)?.is_none())
    };
    let fill = |req: &mut Request, out: &mut Owner<PhpArray>| -> Result<bool, Thrown> {
        for _ in 0..pads {
            if insert(req, out, None, Owner::take(req, pad))? {
                return Ok(true);
            }
        }
        Ok(false)
    };
    if size < 0 && fill(req, &mut out)? {
        // RETURN_OOM()
        return Ok(out);
    }
    let mut refused = false;
    // `ZEND_HASH_MAP_FOREACH_STR_KEY_VAL(Z_ARRVAL_P(input), key, value)`
    // (`Z/ext/standard/array.c:4859`), no owner of the table taken: the
    // argument holds it.
    walk::foreach(req, input, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let element = *row.value();
        let element = Owner::take(req, element);
        let key = match key {
            Key::Str(_) => Some(Owner::take(req, key.clone())),
            Key::Int(_) => None,
        };
        if insert(req, &mut out, key, element)? {
            refused = true;
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    if !refused && size > 0 {
        fill(req, &mut out)?;
    }
    Ok(out)
}

/// `array_reverse(array $array, bool $preserve_keys = false): array`,
/// php's `PHP_FUNCTION(array_reverse)` (`Z/ext/standard/array.c:4742`):
/// the elements in reverse order, string keys kept and integer keys
/// renumbered unless `$preserve_keys`.
///
/// ```php
/// var_dump(array_reverse(["php", 4.0, "k" => ["green", "red"]]), array_reverse([5 => "a", 9 => "b"], true));
/// ```
/// ```expect
/// array(3) {
///   ["k"]=>
///   array(2) {
///     [0]=>
///     string(5) "green"
///     [1]=>
///     string(3) "red"
///   }
///   [0]=>
///   float(4)
///   [1]=>
///   string(3) "php"
/// }
/// array(2) {
///   [9]=>
///   string(1) "b"
///   [5]=>
///   string(1) "a"
/// }
/// ```
#[php_function]
fn array_reverse(
    req: &mut Request,
    array: PhpArray,
    #[php(default = false)] preserve_keys: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let (input, preserve) = (array, preserve_keys);
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_REVERSE_FOREACH_KEY_VAL(Z_ARRVAL_P(input), …)`
    // (`Z/ext/standard/array.c:4771`), which no code of the program
    // reaches: the rows read under the borrowed request, each taken as
    // `zval_add_ref` takes it, then written last to first.
    let rows: Vec<(Key, Owner<Value>)> = walk::rows(req, input)
        .map(|(key, element)| (key, copied(req, element)))
        .collect();
    let mut rows = rows.into_iter().rev();
    while let Some((key, value)) = rows.next() {
        let written = match key {
            Key::Int(_) if !preserve => out.append(req, value),
            _ => {
                let key = Owner::take(req, key);
                out.set(req, key, value)
            }
        };
        if let Err(thrown) = written {
            give_up_rows(req, rows);
            out.give_up(req);
            return Err(thrown);
        }
    }
    Ok(out)
}

/// `array_chunk(array $array, int $length, bool $preserve_keys = false):
/// array`, php's `PHP_FUNCTION(array_chunk)` (`Z/ext/standard/array.c:7040`):
/// a list of arrays of `$length` elements each, the last holding what is
/// left, each renumbered unless `$preserve_keys`.
///
/// ```php
/// var_dump(array_chunk(["a" => 1, "b" => 2, 3], 2), array_chunk([1, 2], 5, true), array_chunk([], 3));
/// try { array_chunk([1], 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   array(2) {
///     [0]=>
///     int(1)
///     [1]=>
///     int(2)
///   }
///   [1]=>
///   array(1) {
///     [0]=>
///     int(3)
///   }
/// }
/// array(1) {
///   [0]=>
///   array(2) {
///     [0]=>
///     int(1)
///     [1]=>
///     int(2)
///   }
/// }
/// array(0) {
/// }
/// array_chunk(): Argument #2 ($length) must be greater than 0
/// ```
///
/// Since 8.5.12 the table of chunks is asked for fallibly, and a chunk is
/// opened only while the memory limit has not been met: past it the call
/// returns and the limit's fatal is raised.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// $input = array_fill(0, 250000, 0);
/// var_dump(count(array_chunk($input, 1)), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 4210688 bytes) in %s on line 4
/// ```
#[php_function]
fn array_chunk(
    req: &mut Request,
    array: PhpArray,
    length: i64,
    #[php(default = false)] preserve_keys: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let (input, size, preserve) = (array, length, preserve_keys);
    if size < 1 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    if input.is_empty() {
        return Ok(Owner::take(req, PhpArray::empty()));
    }
    // `if (size > num_in)`, the chunk's size at most the input's.
    let size = usize::try_from(size).unwrap_or(usize::MAX).min(input.len());
    // `array_finit_size_packed` since 8.5.12: the table for every chunk
    // asked for fallibly, `RETURN_OOM()` where the memory limit refused
    // it.
    let chunks = u32::try_from((input.len() - 1) / size + 1).unwrap_or(u32::MAX);
    let Some(mut out) = req
        .memory_mut()
        .fallible(|req| PhpArray::sized(req, chunks, Layout::Packed))?
    else {
        return Ok(Owner::take(req, PhpArray::empty()));
    };
    let mut chunk: Option<Owner<PhpArray>> = None;
    let mut current = 0;
    // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(input), …)`
    // (`Z/ext/standard/array.c:7079`), which no code of the program
    // reaches: the rows read under the borrowed request, each taken as
    // `zval_add_ref` takes it, then written into their chunks.
    let rows: Vec<(Key, Owner<Value>)> = walk::rows(req, input)
        .map(|(key, element)| (key, copied(req, element)))
        .collect();
    let mut rows = rows.into_iter();
    while let Some((key, value)) = rows.next() {
        // A chunk is opened only while the memory limit has not been met
        // (8.5.12), else the native returns (`RETURN_OOM()`).
        if chunk.is_none() && req.memory().has_oom() {
            value.give_up(req);
            give_up_rows(req, rows);
            return Ok(out);
        }
        let opened = match chunk.take() {
            Some(open) => Ok(open),
            None => PhpArray::init_size(req, u32::try_from(size).unwrap_or(u32::MAX)),
        };
        let mut open = match opened {
            Ok(open) => open,
            Err(thrown) => {
                value.give_up(req);
                give_up_rows(req, rows);
                out.give_up(req);
                return Err(thrown);
            }
        };
        let written = if preserve {
            let key = Owner::take(req, key);
            open.set(req, key, value)
        } else {
            open.append(req, value)
        };
        if let Err(thrown) = written {
            give_up_rows(req, rows);
            open.give_up(req);
            out.give_up(req);
            return Err(thrown);
        }
        current += 1;
        if current == size {
            out.append(req, open.into_value())?;
            chunk = None;
            current = 0;
        } else {
            chunk = Some(open);
        }
    }
    if let Some(open) = chunk {
        out.append(req, open.into_value())?;
    }
    Ok(out)
}

/// `array_combine(array $keys, array $values): array`, php's
/// `PHP_FUNCTION(array_combine)` (`Z/ext/standard/array.c:7114`): the
/// first array's values as keys of the second's, in order, an int as
/// itself and anything else as its string.
///
/// ```php
/// var_dump(array_combine(["green", "red", 7, "7"], ["avocado", "apple", "x", "y"]), array_combine([], []));
/// try { array_combine([1], []); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(3) {
///   ["green"]=>
///   string(7) "avocado"
///   ["red"]=>
///   string(5) "apple"
///   [7]=>
///   string(1) "y"
/// }
/// array(0) {
/// }
/// array_combine(): Argument #1 ($keys) and argument #2 ($values) must have the same number of elements
/// ```
#[php_function]
fn array_combine(
    req: &mut Request,
    keys: PhpArray,
    values: PhpArray,
) -> Result<Owner<PhpArray>, Thrown> {
    if keys.len() != values.len() {
        return Err(argument_error(
            req,
            "ValueError",
            1,
            "and argument #2 ($values) must have the same number of elements",
        ));
    }
    if keys.is_empty() {
        return Ok(Owner::take(req, PhpArray::empty()));
    }
    let mut out = PhpArray::new(req)?;
    let mut value_at = 0;
    // `ZEND_HASH_FOREACH_VAL(keys, entry_keys)` (`Z/ext/standard/array.c:7139`),
    // no owner of either table taken: the arguments hold them while a key's
    // `__toString` runs. Each value is read at the next bucket of `values`
    // that holds one, as it stands at the step.
    walk::foreach(req, keys, Hold::None, |req, row| -> walk::Step {
        let Some((found, _, value)) = values.live_from(value_at) else {
            return Ok(ControlFlow::Break(()));
        };
        value_at = found.saturating_add(1);
        let key = symtable_key(req, row.value())?;
        let value = copied(req, value);
        out.set(req, key, value)?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(out)
}

/// `array_key_exists($key, array $array): bool`, php's
/// `PHP_FUNCTION(array_key_exists)` (`Z/ext/standard/array.c:6998`):
/// whether the key the value makes is in the array, a string as
/// `zend_symtable_exists` reads it, an int, a float truncated behind the
/// deprecation where that loses something, a bool as 0 or 1, a resource
/// by its id with php's warning, and `null` as the empty string behind
/// the 8.5 deprecation; any other key is the function's `TypeError`.
/// php's compiler serves a call of two arguments with
/// `ZEND_ARRAY_KEY_EXISTS` (`zend_compile_func_array_key_exists`), whose
/// error for such a key is the offset's; what reaches the native is a
/// call by a name the program holds, one with a named argument or an
/// unpacked one, and a callback.
///
/// ```php
/// $f = 'array_key_exists';
/// $a = ["1" => 1, "" => 2, "x" => null, 0 => 3];
/// var_dump($f("1", $a), $f(1.9, $a), $f(false, $a), $f("x", $a), $f("y", $a), $f(null, $a), array_key_exists(key: "x", array: $a));
/// try { $f([], $a); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { array_key_exists([], $a); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $f(1, "x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Deprecated: Implicit conversion from float 1.9 to int loses precision in %s on line 4
///
/// Deprecated: Using null as the key parameter for array_key_exists() is deprecated, use an empty string instead in %s on line 4
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// array_key_exists(): Argument #1 ($key) must be a valid array offset type
/// Cannot access offset of type array on array
/// array_key_exists(): Argument #2 ($array) must be of type array, string given
/// ```
#[php_function]
fn array_key_exists(
    req: &mut Request,
    key: Untyped<Value>,
    array: PhpArray,
) -> Result<bool, Thrown> {
    key_exists_as(req, key.0, array)
}

/// `key_exists($key, array $array): bool`, the alias, which php serves
/// through `PHP_FUNCTION(array_key_exists)` itself
/// (`Z/ext/standard/array.c:6998`), no opcode standing for its name: the
/// same keys, and the function's `TypeError` for any other.
///
/// ```php
/// var_dump(key_exists("a", ["a" => 1]), key_exists(2, [1]));
/// try { key_exists([], []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// key_exists(): Argument #1 ($key) must be a valid array offset type
/// ```
#[php_function]
fn key_exists(req: &mut Request, key: Untyped<Value>, array: PhpArray) -> Result<bool, Thrown> {
    key_exists_as(req, key.0, array)
}

fn key_exists_as(req: &mut Request, key: Value, array: PhpArray) -> Result<bool, Thrown> {
    let mut made = None;
    let key = match key.as_ref() {
        ValueRef::Str(s) => Key::lookup(s),
        ValueRef::Int(n) => Key::Int(n),
        ValueRef::Null | ValueRef::Undef => {
            deprecated(
                req,
                "Using null as the key parameter for array_key_exists() is deprecated, use an empty string instead",
            );
            let empty = PhpString::new(req, b"")?.into_key();
            let key = Key::clone(&empty);
            made = Some(empty);
            key
        }
        ValueRef::Float(f) => Key::Int(lval_safe(req, f)),
        ValueRef::Bool(b) => Key::Int(i64::from(b)),
        ValueRef::Resource(resource) => {
            let id = resource.id();
            warning(
                req,
                &format!("Resource ID#{id} used as offset, casting to integer ({id})"),
            );
            Key::Int(id)
        }
        _ => {
            return Err(arg_type_error(req, 1, "be a valid array offset type"));
        }
    };
    let found = array.contains(&key);
    if let Some(made) = made {
        made.give_up(req);
    }
    Ok(found)
}

/// `zend_dval_to_lval_safe`: the float as a key, and the 8.1
/// deprecation where the conversion loses something.
fn lval_safe(req: &mut Request, f: f64) -> i64 {
    let n = float_to_long(f);
    // `zend_is_long_compatible`: exact equality, not a margin.
    #[allow(clippy::cast_precision_loss, clippy::float_cmp)]
    let lossy = n as f64 != f;
    if lossy {
        let text = php_ext::coerce::float_format(f, -1, php_ext::coerce::Exponent::Upper);
        deprecated(
            req,
            &format!("Implicit conversion from float {text} to int loses precision"),
        );
    }
    n
}

/// `array_column(array $array, int|string|null $column_key, int|string|null
/// $index_key = null): array`, php's `PHP_FUNCTION(array_column)`
/// (`Z/ext/standard/array.c:4679`): from each row, an array or an object,
/// the value under `$column_key`, or the whole row where it is `null`,
/// under the row's value at `$index_key` where that is given and the row
/// has one, appended otherwise; a row without the column is skipped. An
/// object's property is read where the executing scope may see it.
///
/// ```php
/// $rows = [["id" => 3, "v" => "x"], ["id" => 7, "v" => "y"], ["v" => "z"], ["id" => 9]];
/// var_dump(array_column($rows, "v"), array_column($rows, "v", "id"), array_column([[1, 2], [3, 4]], null, 0));
/// class P { public $a = 1; private $b = 2; }
/// var_dump(array_column([new P, ["a" => 9]], "a"), array_column([new P], "b"));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   string(1) "x"
///   [1]=>
///   string(1) "y"
///   [2]=>
///   string(1) "z"
/// }
/// array(3) {
///   [3]=>
///   string(1) "x"
///   [7]=>
///   string(1) "y"
///   [8]=>
///   string(1) "z"
/// }
/// array(2) {
///   [1]=>
///   array(2) {
///     [0]=>
///     int(1)
///     [1]=>
///     int(2)
///   }
///   [3]=>
///   array(2) {
///     [0]=>
///     int(3)
///     [1]=>
///     int(4)
///   }
/// }
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(9)
/// }
/// array(0) {
/// }
/// ```
///
/// An object's property is asked of its handlers, `has_property` with
/// `ZEND_PROPERTY_EXISTS` and then `ZEND_PROPERTY_ISSET`, and read through
/// `read_property`: a property the caller may not see is `__isset`'s and
/// `__get`'s, and one that is there holding `null` is taken.
///
/// ```php
/// class M { private $p = 1; function __isset($n) { echo "isset $n\n"; return $n != "q"; } function __get($n) { return "get $n"; } }
/// class N { public $x = null; }
/// var_dump(array_column([new M, new N], "p"), array_column([new M], "q"), array_column([new N], "x"));
/// ```
/// ```expect
/// isset p
/// isset q
/// array(1) {
///   [0]=>
///   string(5) "get p"
/// }
/// array(0) {
/// }
/// array(1) {
///   [0]=>
///   NULL
/// }
/// ```
#[php_function]
fn array_column(
    req: &mut Request,
    array: PhpArray,
    column_key: Option<Union<PhpString, i64>>,
    #[php(default = null)] index_key: Option<Union<PhpString, i64>>,
) -> Result<Owner<PhpArray>, Thrown> {
    let input = array;
    let column = column_key.map(column_of);
    let index = index_key.map(column_of);
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_VAL(input, data)` (`Z/ext/standard/array.c:4705`),
    // no owner of the table taken: the argument holds it while a property
    // read serves `__get`. A throwable gives the array built so far up.
    let walked = walk::foreach(req, input, Hold::None, |req, row| -> walk::Step {
        let row = row.value().deref();
        let value = match &column {
            None => Owner::take(req, row),
            Some(name) => match fetch(req, &row, name)? {
                Some(value) => value,
                None => return Ok(ControlFlow::Continue(())),
            },
        };
        let key = match &index {
            Some(name) => match fetch(req, &row, name) {
                Ok(key) => key,
                Err(thrown) => {
                    value.give_up(req);
                    return Err(thrown);
                }
            },
            None => None,
        };
        match key {
            Some(key) => {
                // `array_set_zval_key`: the key as an array offset.
                let key_of = php_ext::coerce::key_of(req, &key);
                key.give_up(req);
                match key_of {
                    Ok(key) => out.set(req, key, value)?,
                    Err(thrown) => {
                        value.give_up(req);
                        return Err(thrown);
                    }
                }
            }
            None => out.append(req, value)?,
        }
        Ok(ControlFlow::Continue(()))
    });
    if let Err(thrown) = walked {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(out)
}

/// A `string|int` column or index as the key it reads.
fn column_of(key: Union<PhpString, i64>) -> Key {
    match key {
        Union::A(s) => Key::Str(s),
        Union::B(n) => Key::Int(n),
    }
}

/// `array_column_fetch_prop` (`Z/ext/standard/array.c:4634`): the value
/// `name` names in a row, one more owner, a reference followed; an array
/// row read by key (`zend_symtable_find`), an object row read through its
/// `read_property` where its `has_property` finds the property there
/// (`ZEND_PROPERTY_EXISTS`) or set (`ZEND_PROPERTY_ISSET`, which a
/// `__isset` answers), in the scope that called, so a property it may not
/// see is its `__get`'s; anything else nothing.
fn fetch(req: &mut Request, row: &Value, name: &Key) -> Result<Option<Owner<Value>>, Thrown> {
    if let Some(array) = row.as_array() {
        let key = match name {
            Key::Str(s) => Key::lookup(*s),
            Key::Int(n) => Key::Int(*n),
        };
        return Ok(array.get(&key).map(|value| Owner::take(req, value.deref())));
    }
    if let Some(object) = row.as_object() {
        let text: &[u8] = &KeyBytes::of(name);
        let handlers = object.handlers(req);
        if handlers.has_property(req, object, text, PropertyTest::Exists)?
            || handlers.has_property(req, object, text, PropertyTest::Isset)?
        {
            let value = handlers.read_property(req, object, text, PropertyRead::Warning)?;
            return Ok(Some(value));
        }
        return Ok(None);
    }
    Ok(None)
}

/// `array_sum(array $array): int|float`, php's `php_array_binop` over
/// `add_function` (`Z/ext/standard/array.c:6443`): the elements added
/// from `0` in order; an object its class's `cast_object` to a number,
/// as `GMP`'s is; an array, any other object and a resource are php's
/// warning, the resource adding its id, and a string that is not numeric
/// adds zero behind the same warning.
///
/// ```php
/// var_dump(array_sum([gmp_init(PHP_INT_MAX - 1), 1]), array_product([gmp_init(3), 2.5]));
/// ```
/// ```expect
/// int(9223372036854775807)
/// float(7.5)
/// ```
///
///
/// ```php
/// var_dump(array_sum([1, 2.5, "3", true, null]), array_sum([]), array_sum([PHP_INT_MAX, 1]));
/// var_dump(array_sum(["x", [1], "5abc", new stdClass]));
/// ```
/// ```expect
/// float(7.5)
/// int(0)
/// float(9.223372036854776E+18)
///
/// Warning: array_sum(): Addition is not supported on type string in %s on line 3
///
/// Warning: array_sum(): Addition is not supported on type array in %s on line 3
///
/// Warning: A non-numeric value encountered in %s on line 3
///
/// Warning: array_sum(): Addition is not supported on type stdClass in %s on line 3
/// int(5)
/// ```
#[php_function]
fn array_sum(req: &mut Request, array: PhpArray) -> Result<Owner<Raw<Union<i64, f64>>>, Thrown> {
    let total = binop(req, array, BinaryOp::Add, "Addition", 0)?;
    Ok(returned(total))
}

/// `array_product(array $array): int|float`, php's `php_array_binop` over
/// `mul_function` (`Z/ext/standard/array.c:6503`): the elements
/// multiplied from `1`.
///
/// ```php
/// var_dump(array_product(["2", 3, 0.5]), array_product([]), array_product([2, "x"]));
/// ```
/// ```expect
/// Warning: array_product(): Multiplication is not supported on type string in %s on line 2
/// float(3)
/// int(1)
/// int(0)
/// ```
#[php_function]
fn array_product(
    req: &mut Request,
    array: PhpArray,
) -> Result<Owner<Raw<Union<i64, f64>>>, Thrown> {
    let total = binop(req, array, BinaryOp::Mul, "Multiplication", 1)?;
    Ok(returned(total))
}

/// `op(total, total, operand)`: the old total given up, as the new
/// total takes its place, and on a throw.
fn step(
    req: &mut Request,
    op: BinaryOp,
    total: Owner<Value>,
    operand: &Value,
) -> Result<Owner<Value>, Thrown> {
    let next = binary_op(req, op, &total, operand);
    total.give_up(req);
    next
}

fn binop(
    req: &mut Request,
    input: PhpArray,
    op: BinaryOp,
    name: &str,
    initial: i64,
) -> Result<Owner<Value>, Thrown> {
    let mut total = Some(Owner::int(initial));
    // `ZEND_HASH_FOREACH_VAL(input, entry)` (`Z/ext/standard/array.c:6457`),
    // no owner of the table taken: the argument holds it while an object's
    // `cast_object` and a warning's handler run. The running total is the
    // body's to take and give back.
    let walked = walk::foreach(req, input, Hold::None, |req, row| -> walk::Step {
        let Some(mut acc) = total.take() else {
            return Ok(ControlFlow::Break(()));
        };
        let element = *row.value();
        let element = element.deref();
        let operand = match element.as_ref() {
            ValueRef::Array(_) | ValueRef::Object(_) => {
                // "For objects we try to cast them to a numeric type":
                // the class's `cast_object` slot asked for `_IS_NUMBER`.
                if let Some(object) = element.as_object() {
                    let handlers = object.handlers(req);
                    let number = match handlers.cast_object(req, object, TypeTag::Number) {
                        Ok(number) => number,
                        Err(thrown) => {
                            acc.give_up(req);
                            return Err(thrown);
                        }
                    };
                    match number {
                        Some(number)
                            if number.as_int().is_some() || number.as_float().is_some() =>
                        {
                            let stepped = step(req, op, acc, &number);
                            number.give_up(req);
                            acc = stepped?;
                            total = Some(acc);
                            return Ok(ControlFlow::Continue(()));
                        }
                        Some(other) => other.give_up(req),
                        None => {}
                    }
                }
                let type_name = match element.as_object() {
                    Some(object) => req
                        .symbols()
                        .class_decl(object.class())
                        .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned())
                        .unwrap_or_default(),
                    None => String::from("array"),
                };
                docref(
                    req,
                    Level::Warning,
                    &format!("{name} is not supported on type {type_name}"),
                );
                total = Some(acc);
                return Ok(ControlFlow::Continue(()));
            }
            ValueRef::Resource(resource) => {
                let id = resource.id();
                docref(
                    req,
                    Level::Warning,
                    &format!("{name} is not supported on type resource"),
                );
                Value::int(id)
            }
            ValueRef::Str(s) => match numeric_string(s.as_bytes()) {
                Some(number) => number,
                None => {
                    if let Some(number) = leading_number(s.as_bytes()) {
                        warning(req, "A non-numeric value encountered");
                        number
                    } else {
                        acc = step(req, op, acc, &Value::int(0))?;
                        docref(
                            req,
                            Level::Warning,
                            &format!("{name} is not supported on type string"),
                        );
                        total = Some(acc);
                        return Ok(ControlFlow::Continue(()));
                    }
                }
            },
            ValueRef::Null | ValueRef::Undef => Value::int(0),
            ValueRef::Bool(b) => Value::int(i64::from(b)),
            _ => element,
        };
        acc = step(req, op, acc, &operand)?;
        total = Some(acc);
        Ok(ControlFlow::Continue(()))
    });
    match (walked, total) {
        (Ok(_), Some(total)) => Ok(total),
        (Ok(_), None) => Ok(Owner::int(initial)),
        (Err(thrown), total) => {
            if let Some(total) = total {
                total.give_up(req);
            }
            Err(thrown)
        }
    }
}

/// The number a leading-numeric string starts with, `"5abc"`'s 5: what
/// `zendi_smart_str` reads with `allow_errors`; none where no number
/// leads.
fn leading_number(bytes: &[u8]) -> Option<Value> {
    let mut end = 0;
    while end < bytes.len() && matches!(bytes[end], b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C) {
        end += 1;
    }
    let start = end;
    if end < bytes.len() && matches!(bytes[end], b'+' | b'-') {
        end += 1;
    }
    let digits = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let mut seen = end > digits;
    if end < bytes.len() && bytes[end] == b'.' {
        let mut fraction = end + 1;
        while fraction < bytes.len() && bytes[fraction].is_ascii_digit() {
            fraction += 1;
        }
        if seen || fraction > end + 1 {
            seen = true;
            end = fraction;
        }
    }
    if !seen {
        return None;
    }
    if end < bytes.len() && matches!(bytes[end], b'e' | b'E') {
        let mut exponent = end + 1;
        if exponent < bytes.len() && matches!(bytes[exponent], b'+' | b'-') {
            exponent += 1;
        }
        let first = exponent;
        while exponent < bytes.len() && bytes[exponent].is_ascii_digit() {
            exponent += 1;
        }
        if exponent > first {
            end = exponent;
        }
    }
    numeric_string(&bytes[start..end])
}

#[allow(dead_code)]
fn unused(_: Ordering) {}
