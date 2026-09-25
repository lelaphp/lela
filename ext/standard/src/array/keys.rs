//! The natives of `array.c` that read an array's keys and values into
//! a new one, or answer about them: the searches (`in_array`,
//! `array_search`, `array_key_exists` and its alias `key_exists`), the
//! readers of one end (`array_key_first`, `array_key_last`,
//! `array_first`, `array_last`), and the builders that keep, flip,
//! count, recase, pair, fill, pad, cut, pick a column out of, reverse,
//! sum or multiply what they are given.

use std::cmp::Ordering;

use php_ext::coerce::{
    BinaryOp, binary_op, float_to_long, identical, loose_eq, numeric_string, to_string, value_name,
};
use php_ext::decl::{ConstExpr, FunctionDecl, Name, Parameter, ParameterDefault, Type};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, deprecated, docref, throw, warning,
};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};

use super::sort::key_value;
use super::walk::array_argument;
use super::{copied, key_copy};
use crate::output::Entry;
use crate::{
    argument_count_between, bool_argument, constant, declared, frameless, long_argument, parameter,
};

/// `CASE_LOWER` and `CASE_UPPER`, what `array_change_key_case` reads.
const CASE_LOWER: i64 = 0;
const CASE_UPPER: i64 = 1;

/// `HT_MAX_SIZE` on a 64-bit build (`Z/Zend/zend_types.h`).
const HT_MAX_SIZE: i64 = 0x4000_0000;

/// The two case constants, in the stub's order.
///
/// ```php
/// var_dump(CASE_LOWER, CASE_UPPER);
/// ```
/// ```expect
/// int(0)
/// int(1)
/// ```
pub(super) fn constants() -> Vec<php_ext::decl::ConstDecl> {
    vec![
        constant(b"CASE_LOWER", CASE_LOWER),
        constant(b"CASE_UPPER", CASE_UPPER),
    ]
}

/// The declarations, as `basic_functions.stub.php` has them.
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
// One row per native, in the stub's order: the table is the declaration.
#[allow(clippy::too_many_lines)]
pub(super) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let array = || parameter(b"array", Type::Array);
    let strict = || Parameter {
        default: ParameterDefault::Expr(ConstExpr::Bool(false)),
        ..parameter(b"strict", Type::Bool)
    };
    let preserve = || Parameter {
        default: ParameterDefault::Expr(ConstExpr::Bool(false)),
        ..parameter(b"preserve_keys", Type::Bool)
    };
    let one =
        |name: &[u8], returns: Type, entry: Entry| (declared(name, &[array()], returns), entry);
    let key_or_null = || Type::Union(vec![Type::String, Type::Int, Type::Null]);
    let search = |name: &[u8], returns: Type, entry: Entry| {
        (
            declared(
                name,
                &[
                    parameter(b"needle", Type::Mixed),
                    parameter(b"haystack", Type::Array),
                    strict(),
                ],
                returns,
            ),
            entry,
        )
    };
    let untyped_key = || Parameter {
        ty: None,
        ..parameter(b"key", Type::Mixed)
    };
    vec![
        (
            frameless(search(b"in_array", Type::Bool, in_array).0, &[2, 3]),
            in_array as Entry,
        ),
        search(
            b"array_search",
            Type::Union(vec![Type::String, Type::Int, Type::False]),
            array_search,
        ),
        (
            declared(
                b"array_fill",
                &[
                    parameter(b"start_index", Type::Int),
                    parameter(b"count", Type::Int),
                    parameter(b"value", Type::Mixed),
                ],
                Type::Array,
            ),
            array_fill,
        ),
        (
            declared(
                b"array_fill_keys",
                &[
                    parameter(b"keys", Type::Array),
                    parameter(b"value", Type::Mixed),
                ],
                Type::Array,
            ),
            array_fill_keys,
        ),
        (
            declared(
                b"array_keys",
                &[
                    array(),
                    Parameter {
                        default: ParameterDefault::Unshown,
                        ..parameter(b"filter_value", Type::Mixed)
                    },
                    strict(),
                ],
                Type::Array,
            ),
            array_keys,
        ),
        one(b"array_key_first", key_or_null(), array_key_first),
        one(b"array_key_last", key_or_null(), array_key_last),
        one(b"array_first", Type::Mixed, array_first),
        one(b"array_last", Type::Mixed, array_last),
        one(b"array_values", Type::Array, array_values),
        one(b"array_count_values", Type::Array, array_count_values),
        (
            declared(
                b"array_column",
                &[
                    array(),
                    parameter(b"column_key", key_or_null()),
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Null),
                        ..parameter(b"index_key", key_or_null())
                    },
                ],
                Type::Array,
            ),
            array_column,
        ),
        (
            declared(b"array_reverse", &[array(), preserve()], Type::Array),
            array_reverse,
        ),
        (
            declared(
                b"array_pad",
                &[
                    array(),
                    parameter(b"length", Type::Int),
                    parameter(b"value", Type::Mixed),
                ],
                Type::Array,
            ),
            array_pad,
        ),
        one(b"array_flip", Type::Array, array_flip),
        (
            declared(
                b"array_change_key_case",
                &[
                    array(),
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Constant(Name(
                            b"CASE_LOWER".as_slice().into(),
                        ))),
                        ..parameter(b"case", Type::Int)
                    },
                ],
                Type::Array,
            ),
            array_change_key_case,
        ),
        one(
            b"array_sum",
            Type::Union(vec![Type::Int, Type::Float]),
            array_sum,
        ),
        one(
            b"array_product",
            Type::Union(vec![Type::Int, Type::Float]),
            array_product,
        ),
        (
            declared(b"array_key_exists", &[untyped_key(), array()], Type::Bool),
            array_key_exists,
        ),
        (
            declared(b"key_exists", &[untyped_key(), array()], Type::Bool),
            key_exists,
        ),
        (
            declared(
                b"array_chunk",
                &[array(), parameter(b"length", Type::Int), preserve()],
                Type::Array,
            ),
            array_chunk,
        ),
        (
            declared(
                b"array_combine",
                &[
                    parameter(b"keys", Type::Array),
                    parameter(b"values", Type::Array),
                ],
                Type::Array,
            ),
            array_combine,
        ),
    ]
}

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
        return Ok(identical(needle, &element));
    }
    loose_eq(req, needle, &element)
}

/// `_php_search_array`: the key of the first element that matches.
fn search(req: &mut Request) -> Result<Option<Key>, Thrown> {
    argument_count_between(req, 2, 3)?;
    let needle = req.argument(1).unwrap_or_default().deref();
    let haystack = array_argument(req, 2)?;
    let strict = if req.argument_count() == 3 {
        bool_argument(req, 3)?
    } else {
        false
    };
    for (key, element) in &haystack {
        if matches(req, &needle, &element, strict)? {
            return Ok(Some(key));
        }
    }
    Ok(None)
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
fn in_array(req: &mut Request) -> Result<Value, Thrown> {
    Ok(Value::bool(search(req)?.is_some()))
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
fn array_search(req: &mut Request) -> Result<Value, Thrown> {
    match search(req)? {
        Some(key) => {
            let value = key_value(&key);
            req.addref(&value);
            Ok(value)
        }
        None => Ok(Value::bool(false)),
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
fn array_keys(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 3)?;
    let array = array_argument(req, 1)?;
    let filter = if req.argument_count() >= 2 {
        Some(req.argument(2).unwrap_or_default().deref())
    } else {
        None
    };
    let strict = if req.argument_count() == 3 {
        bool_argument(req, 3)?
    } else {
        false
    };
    if array.is_empty() {
        let value = Value::array(array);
        req.addref(&value);
        return Ok(value);
    }
    let mut out = PhpArray::new(req)?;
    for (key, element) in &array {
        if let Some(filter) = &filter
            && !matches(req, filter, &element, strict)?
        {
            continue;
        }
        let value = key_value(&key);
        req.addref(&value);
        out.append(req, value)?;
    }
    Ok(Value::array(out))
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
fn array_key_first(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let array = array_argument(req, 1)?;
    let first = array.iter().next();
    Ok(key_or_null(req, first.map(|(key, _)| key)))
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
fn array_key_last(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let array = array_argument(req, 1)?;
    let last = array.snapshot_refs().pop();
    Ok(key_or_null(req, last.map(|(key, _)| key)))
}

fn key_or_null(req: &mut Request, key: Option<Key>) -> Value {
    match key {
        Some(key) => {
            let value = key_value(&key);
            req.addref(&value);
            value
        }
        None => Value::null(),
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
fn array_first(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let array = array_argument(req, 1)?;
    let first = array.iter().next();
    Ok(value_or_null(req, first.map(|(_, value)| value)))
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
fn array_last(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let array = array_argument(req, 1)?;
    let last = array.snapshot_refs().pop();
    Ok(value_or_null(req, last.map(|(_, value)| value)))
}

fn value_or_null(req: &mut Request, value: Option<Value>) -> Value {
    match value {
        Some(value) => {
            let value = value.deref();
            req.addref(&value);
            value
        }
        None => Value::null(),
    }
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
fn array_values(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let array = array_argument(req, 1)?;
    if array.is_empty() {
        return Ok(Value::array(PhpArray::new(req)?));
    }
    if array.is_list() && array.next_free() == i64::try_from(array.len()).unwrap_or(i64::MAX) {
        let value = Value::array(array);
        req.addref(&value);
        return Ok(value);
    }
    let mut out = PhpArray::new(req)?;
    for (_, element) in array.snapshot_refs() {
        let value = copied(req, element);
        out.append(req, value)?;
    }
    Ok(Value::array(out))
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
fn array_count_values(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let array = array_argument(req, 1)?;
    let mut out = PhpArray::new(req)?;
    for (_, element) in array.snapshot_refs() {
        let element = element.deref();
        let key = match element.as_ref() {
            ValueRef::Int(n) => Key::Int(n),
            ValueRef::Str(s) => {
                req.addref(&element);
                Key::from_string(*s)
            }
            _ => {
                docref(
                    req,
                    Level::Warning,
                    "Can only count string and integer values, entry skipped",
                );
                continue;
            }
        };
        let count = out.get(&key).and_then(|v| v.as_int()).unwrap_or(0);
        out.set(req, key, Value::int(count + 1))?;
    }
    Ok(Value::array(out))
}

/// The key a value makes where php writes it with
/// `zend_symtable_update`: an int as itself, anything else as its
/// string, a canonical decimal read back as an int.
fn symtable_key(req: &mut Request, value: &Value) -> Result<Key, Thrown> {
    if let Some(n) = value.as_int() {
        return Ok(Key::Int(n));
    }
    if let Some(s) = value.as_php_string() {
        req.addref(value);
        return Ok(Key::from_string(*s));
    }
    Ok(Key::from_string(to_string(req, value)?))
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
fn array_flip(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let array = array_argument(req, 1)?;
    let mut out = PhpArray::new(req)?;
    for (key, element) in array.snapshot_refs() {
        let element = element.deref();
        if !matches!(element.as_ref(), ValueRef::Int(_) | ValueRef::Str(_)) {
            docref(
                req,
                Level::Warning,
                "Can only flip string and integer values, entry skipped",
            );
            continue;
        }
        let data = key_value(&key);
        req.addref(&data);
        let new_key = symtable_key(req, &element)?;
        out.set(req, new_key, data)?;
    }
    Ok(Value::array(out))
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
fn array_change_key_case(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let array = array_argument(req, 1)?;
    let upper = if req.argument_count() == 2 {
        long_argument(req, 2)? != 0
    } else {
        false
    };
    let mut out = PhpArray::new(req)?;
    for (key, element) in array.snapshot_refs() {
        let key = match key {
            Key::Int(n) => Key::Int(n),
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
                Key::Str(PhpString::new(req, &folded)?)
            }
        };
        let value = copied(req, element);
        out.set(req, key, value)?;
    }
    Ok(Value::array(out))
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
fn array_fill(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 3, 3)?;
    let start = long_argument(req, 1)?;
    let count = long_argument(req, 2)?;
    let value = req.argument(3).unwrap_or_default().deref();
    if count < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    if count == 0 {
        return Ok(Value::array(PhpArray::new(req)?));
    }
    if count > i64::from(i32::MAX) {
        return Err(throw(
            req,
            "ValueError",
            "array_fill(): Argument #2 ($count) is too large",
        ));
    }
    if start > i64::MAX - count + 1 {
        return Err(throw(
            req,
            "Error",
            "Cannot add element to the array as the next element is already occupied",
        ));
    }
    let mut out = PhpArray::new(req)?;
    for at in 0..count {
        req.addref(&value);
        out.set(req, Key::Int(start + at), value)?;
    }
    Ok(Value::array(out))
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
fn array_fill_keys(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let keys = array_argument(req, 1)?;
    let value = req.argument(2).unwrap_or_default().deref();
    let mut out = PhpArray::new(req)?;
    for (_, element) in keys.snapshot_refs() {
        let element = element.deref();
        let key = symtable_key(req, &element)?;
        req.addref(&value);
        out.set(req, key, value)?;
    }
    Ok(Value::array(out))
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
fn array_pad(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 3, 3)?;
    let input = array_argument(req, 1)?;
    let size = long_argument(req, 2)?;
    let pad = req.argument(3).unwrap_or_default().deref();
    if !(-HT_MAX_SIZE..=HT_MAX_SIZE).contains(&size) {
        return Err(arg_value_error(
            req,
            2,
            "not exceed the maximum allowed array size",
        ));
    }
    let have = i64::try_from(input.len()).unwrap_or(i64::MAX);
    if have >= size.abs() {
        let value = Value::array(input);
        req.addref(&value);
        return Ok(value);
    }
    let pads = size.abs() - have;
    let mut out = PhpArray::new(req)?;
    let fill = |req: &mut Request, out: &mut PhpArray| -> Result<(), Thrown> {
        for _ in 0..pads {
            req.addref(&pad);
            out.append(req, pad)?;
        }
        Ok(())
    };
    if size < 0 {
        fill(req, &mut out)?;
    }
    for (key, element) in input.snapshot_refs() {
        req.addref(&element);
        match key {
            Key::Str(_) => {
                let key = key_copy(req, &key);
                out.set(req, key, element)?;
            }
            Key::Int(_) => out.append(req, element)?,
        }
    }
    if size > 0 {
        fill(req, &mut out)?;
    }
    Ok(Value::array(out))
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
fn array_reverse(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let input = array_argument(req, 1)?;
    let preserve = if req.argument_count() == 2 {
        bool_argument(req, 2)?
    } else {
        false
    };
    let mut out = PhpArray::new(req)?;
    for (key, element) in input.snapshot_refs().into_iter().rev() {
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
fn array_chunk(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 3)?;
    let input = array_argument(req, 1)?;
    let size = long_argument(req, 2)?;
    let preserve = if req.argument_count() == 3 {
        bool_argument(req, 3)?
    } else {
        false
    };
    if size < 1 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    let mut out = PhpArray::new(req)?;
    let mut chunk: Option<PhpArray> = None;
    let mut current = 0;
    for (key, element) in input.snapshot_refs() {
        let mut open = match chunk {
            Some(open) => open,
            None => PhpArray::new(req)?,
        };
        let value = copied(req, element);
        if preserve {
            let key = key_copy(req, &key);
            open.set(req, key, value)?;
        } else {
            open.append(req, value)?;
        }
        current += 1;
        if current == size {
            out.append(req, Value::array(open))?;
            chunk = None;
            current = 0;
        } else {
            chunk = Some(open);
        }
    }
    if let Some(open) = chunk {
        out.append(req, Value::array(open))?;
    }
    Ok(Value::array(out))
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
fn array_combine(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let keys = array_argument(req, 1)?;
    let values = array_argument(req, 2)?;
    if keys.len() != values.len() {
        return Err(throw(
            req,
            "ValueError",
            "array_combine(): Argument #1 ($keys) and argument #2 ($values) must have the same number of elements",
        ));
    }
    let mut out = PhpArray::new(req)?;
    for ((_, key), (_, value)) in keys.snapshot_refs().into_iter().zip(values.snapshot_refs()) {
        let key = symtable_key(req, &key)?;
        let value = copied(req, value);
        out.set(req, key, value)?;
    }
    Ok(Value::array(out))
}

/// `array_key_exists($key, array $array): bool`, what php serves a call
/// of the name with, `ZEND_ARRAY_KEY_EXISTS` over
/// `zend_array_key_exists_fast` (`Z/Zend/zend_execute.c:3372`), since
/// `zend_compile_func_array_key_exists` compiles every two-argument call
/// of it to the opcode: whether the key the value makes is in the array,
/// a string as `ZEND_HANDLE_NUMERIC` reads it, an int, a float truncated
/// behind the deprecation where that loses something, a bool as 0 or 1,
/// a resource by its id with php's warning, and `null` as the empty
/// string behind the 8.5 deprecation; any other key is the opcode's
/// `TypeError`.
///
/// ```php
/// $a = ["1" => 1, "" => 2, "x" => null, 0 => 3];
/// var_dump(array_key_exists("1", $a), array_key_exists(1.9, $a), array_key_exists(false, $a), array_key_exists("x", $a), array_key_exists("y", $a), array_key_exists(null, $a));
/// try { array_key_exists([], $a); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { array_key_exists(1, "x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: Implicit conversion from float 1.9 to int loses precision in %s on line 3
///
/// Deprecated: Using null as the key parameter for array_key_exists() is deprecated, use an empty string instead in %s on line 3
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// Cannot access offset of type array on array
/// array_key_exists(): Argument #2 ($array) must be of type array, string given
/// ```
fn array_key_exists(req: &mut Request) -> Result<Value, Thrown> {
    key_exists_as(req, true)
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
fn key_exists(req: &mut Request) -> Result<Value, Thrown> {
    key_exists_as(req, false)
}

fn key_exists_as(req: &mut Request, opcode: bool) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let key = req.argument(1).unwrap_or_default().deref();
    let array = array_argument(req, 2)?;
    let key = match key.as_ref() {
        ValueRef::Str(s) => Key::from_string(*s),
        ValueRef::Int(n) => Key::Int(n),
        ValueRef::Null | ValueRef::Undef => {
            deprecated(
                req,
                "Using null as the key parameter for array_key_exists() is deprecated, use an empty string instead",
            );
            Key::Str(PhpString::new(req, b"")?)
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
        _ if opcode => {
            let name = value_name(req, &key);
            return Err(throw(
                req,
                "TypeError",
                &format!("Cannot access offset of type {name} on array"),
            ));
        }
        _ => {
            return Err(arg_type_error(req, 1, "be a valid array offset type"));
        }
    };
    Ok(Value::bool(array.contains(&key)))
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
fn array_column(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 3)?;
    let input = array_argument(req, 1)?;
    let column = column_key(req, 2)?;
    let index = if req.argument_count() == 3 {
        column_key(req, 3)?
    } else {
        None
    };
    let mut out = PhpArray::new(req)?;
    for (_, row) in input.snapshot_refs() {
        let row = row.deref();
        let value = match &column {
            None => {
                req.addref(&row);
                row
            }
            Some(name) => match fetch(req, &row, name)? {
                Some(value) => value,
                None => continue,
            },
        };
        let key = match &index {
            Some(name) => fetch(req, &row, name)?,
            None => None,
        };
        match key {
            Some(key) => {
                // `array_set_zval_key`: the key as an array offset.
                let key_of = php_ext::coerce::key_of(req, &key);
                req.release(key);
                match key_of {
                    Ok(key) => out.set(req, key, value)?,
                    Err(thrown) => {
                        req.release(value);
                        req.release(out.to_value());
                        return Err(thrown);
                    }
                }
            }
            None => out.append(req, value)?,
        }
    }
    Ok(Value::array(out))
}

/// `Z_PARAM_STR_OR_LONG_OR_NULL` at `position`, as the key it names.
fn column_key(req: &mut Request, position: u32) -> Result<Option<Key>, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let coerced = php_ext::coerce::coerce_param(
        req,
        &given,
        &Type::Union(vec![Type::String, Type::Int, Type::Null]),
        position,
    )?;
    Ok(match coerced.as_ref() {
        ValueRef::Int(n) => Some(Key::Int(n)),
        ValueRef::Str(s) => Some(Key::Str(*s)),
        _ => None,
    })
}

/// `array_column_fetch_prop`: the value `name` names in a row, one more
/// owner, a reference followed; an array row read by key
/// (`zend_symtable_find`), an object row read where its property is
/// public, and anything else nothing.
fn fetch(req: &mut Request, row: &Value, name: &Key) -> Result<Option<Value>, Thrown> {
    if let Some(array) = row.as_array() {
        let key = match name {
            Key::Str(s) => Key::from_string(*s),
            Key::Int(n) => Key::Int(*n),
        };
        return Ok(array.get(&key).map(|value| {
            let value = value.deref();
            req.addref(&value);
            value
        }));
    }
    if let Some(object) = row.as_object().copied() {
        let text = match name {
            Key::Int(n) => n.to_string().into_bytes(),
            Key::Str(s) => s.as_bytes().to_vec(),
        };
        let visible = object
            .props_for(req, php_ext::abi::PropertyPurpose::ArrayCast)?
            .into_iter()
            .find(|(prop, _)| prop.as_bytes() == text.as_slice())
            .map(|(_, value)| value);
        return Ok(visible.map(|value| {
            let value = value.deref();
            req.addref(&value);
            value
        }));
    }
    Ok(None)
}

/// `array_sum(array $array): int|float`, php's `php_array_binop` over
/// `add_function` (`Z/ext/standard/array.c:6443`): the elements added
/// from `0` in order; an array, an object and a resource are php's
/// warning, the resource adding its id, and a string that is not numeric
/// adds zero behind the same warning.
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
fn array_sum(req: &mut Request) -> Result<Value, Thrown> {
    binop(req, BinaryOp::Add, "Addition", 0)
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
fn array_product(req: &mut Request) -> Result<Value, Thrown> {
    binop(req, BinaryOp::Mul, "Multiplication", 1)
}

fn binop(req: &mut Request, op: BinaryOp, name: &str, initial: i64) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let input = array_argument(req, 1)?;
    let mut total = Value::int(initial);
    for (_, element) in input.snapshot_refs() {
        let element = element.deref();
        let operand = match element.as_ref() {
            ValueRef::Array(_) | ValueRef::Object(_) => {
                let type_name = match element.as_object() {
                    Some(object) => req
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
                continue;
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
                        total = binary_op(req, op, &total, &Value::int(0))?;
                        docref(
                            req,
                            Level::Warning,
                            &format!("{name} is not supported on type string"),
                        );
                        continue;
                    }
                }
            },
            ValueRef::Null | ValueRef::Undef => Value::int(0),
            ValueRef::Bool(b) => Value::int(i64::from(b)),
            _ => element,
        };
        total = binary_op(req, op, &total, &operand)?;
    }
    Ok(total)
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
