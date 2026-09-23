//! The natives of `type.c` beside the `is_*` checks the crate root
//! declares: the type names (`gettype`, `get_debug_type`), the
//! conversions (`settype`, `intval`, `floatval` and its alias
//! `doubleval`, `boolval`, `strval`), and the remaining predicates
//! (`array_is_list`, `is_numeric`, `is_callable`, `is_iterable`,
//! `is_countable`).

use php_ext::call::lookup_class;
use php_ext::coerce::{
    is_callable as callable, numeric_string, to_array, to_double, to_long, to_object, to_string,
    truthy,
};
use php_ext::decl::{ConstExpr, FunctionDecl, Parameter, Type};
use php_ext::raising::{Thrown, arg_value_error, throw};
use php_ext::request::Request;
use php_ext::values::{PhpString, Value, ValueRef};

use crate::output::Entry;
use crate::{
    argument_count_between, bool_argument, declared, frameless, long_argument, parameter,
    string_argument,
};

/// The declarations, as `basic_functions.stub.php` has them.
///
/// ```php
/// foreach (['gettype', 'get_debug_type', 'settype', 'intval', 'floatval', 'doubleval', 'boolval', 'strval', 'array_is_list', 'is_numeric', 'is_callable', 'is_iterable', 'is_countable'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function gettype ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> mixed $value ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function get_debug_type ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> mixed $value ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function settype ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> mixed &$var ]
///     Parameter #1 [ <required> string $type ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function intval ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> mixed $value ]
///     Parameter #1 [ <optional> int $base = 10 ]
///   }
///   - Return [ int ]
/// }
/// Function [ <internal:standard> function floatval ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> mixed $value ]
///   }
///   - Return [ float ]
/// }
/// Function [ <internal:standard> function doubleval ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> mixed $value ]
///   }
///   - Return [ float ]
/// }
/// Function [ <internal:standard> function boolval ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> mixed $value ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function strval ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> mixed $value ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function array_is_list ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> array $array ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function is_numeric ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> mixed $value ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function is_callable ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> mixed $value ]
///     Parameter #1 [ <optional> bool $syntax_only = false ]
///     Parameter #2 [ <optional> &$callable_name = null ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function is_iterable ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> mixed $value ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function is_countable ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> mixed $value ]
///   }
///   - Return [ bool ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let value = || parameter(b"value", Type::Mixed);
    let one =
        |name: &[u8], returns: Type, entry: Entry| (declared(name, &[value()], returns), entry);
    vec![
        one(b"gettype", Type::String, gettype),
        one(b"get_debug_type", Type::String, get_debug_type),
        (
            declared(
                b"settype",
                &[
                    Parameter {
                        by_ref: true,
                        ..parameter(b"var", Type::Mixed)
                    },
                    parameter(b"type", Type::String),
                ],
                Type::Bool,
            ),
            settype,
        ),
        (
            declared(
                b"intval",
                &[
                    value(),
                    Parameter {
                        default: Some(ConstExpr::Long(10)),
                        ..parameter(b"base", Type::Int)
                    },
                ],
                Type::Int,
            ),
            intval,
        ),
        one(b"floatval", Type::Float, floatval),
        one(b"doubleval", Type::Float, floatval),
        one(b"boolval", Type::Bool, boolval),
        one(b"strval", Type::String, strval),
        (
            declared(
                b"array_is_list",
                &[parameter(b"array", Type::Array)],
                Type::Bool,
            ),
            array_is_list,
        ),
        (
            frameless(declared(b"is_numeric", &[value()], Type::Bool), &[1]),
            is_numeric,
        ),
        (
            declared(
                b"is_callable",
                &[
                    value(),
                    Parameter {
                        default: Some(ConstExpr::Bool(false)),
                        ..parameter(b"syntax_only", Type::Bool)
                    },
                    Parameter {
                        by_ref: true,
                        default: Some(ConstExpr::Null),
                        ty: None,
                        ..parameter(b"callable_name", Type::Mixed)
                    },
                ],
                Type::Bool,
            ),
            is_callable,
        ),
        one(b"is_iterable", Type::Bool, is_iterable),
        one(b"is_countable", Type::Bool, is_countable),
    ]
}

/// The one argument of a type native, as `Z_PARAM_ZVAL` reads it: the
/// value, a reference followed.
fn the_value(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    Ok(req.argument(1).unwrap_or_default().deref())
}

fn string_value(req: &mut Request, text: &[u8]) -> Result<Value, Thrown> {
    Ok(Value::string(PhpString::new(req, text)?))
}

/// `gettype(mixed $value): string`, php's `PHP_FUNCTION(gettype)` over
/// `zend_zval_get_legacy_type`: the names php has kept since PHP 4,
/// `integer`, `double`, `boolean`, `NULL`, and `resource (closed)` for a
/// closed resource.
///
/// ```php
/// $f = fopen('php://memory', 'r'); fclose($f);
/// var_dump(gettype(1), gettype(1.0), gettype("s"), gettype(null), gettype([]), gettype(new stdClass), gettype(true), gettype($f), gettype(fopen('php://memory', 'r')));
/// ```
/// ```expect
/// string(7) "integer"
/// string(6) "double"
/// string(6) "string"
/// string(4) "NULL"
/// string(5) "array"
/// string(6) "object"
/// string(7) "boolean"
/// string(17) "resource (closed)"
/// string(8) "resource"
/// ```
fn gettype(req: &mut Request) -> Result<Value, Thrown> {
    let value = the_value(req)?;
    let name: &[u8] = match value.as_ref() {
        ValueRef::Null | ValueRef::Undef => b"NULL",
        ValueRef::Bool(_) => b"boolean",
        ValueRef::Int(_) => b"integer",
        ValueRef::Float(_) => b"double",
        ValueRef::Str(_) => b"string",
        ValueRef::Array(_) => b"array",
        ValueRef::Object(_) => b"object",
        ValueRef::Resource(resource) if resource.is_closed() => b"resource (closed)",
        ValueRef::Resource(_) => b"resource",
        ValueRef::Reference(_) => b"unknown type",
    };
    string_value(req, name)
}

/// `get_debug_type(mixed $value): string`, php's
/// `PHP_FUNCTION(get_debug_type)` (`Z/ext/standard/type.c:44`): the
/// type's name as a declaration writes it, a class's name for an
/// object, an anonymous class's name up to its NUL, and a resource's
/// type in parentheses.
///
/// ```php
/// $f = fopen('php://memory', 'r'); fclose($f);
/// class Named {}
/// var_dump(get_debug_type(1), get_debug_type(1.5), get_debug_type("x"), get_debug_type([]), get_debug_type(new Named), get_debug_type(new stdClass), get_debug_type($f), get_debug_type(fopen('php://memory', 'r')), get_debug_type(null), get_debug_type(false));
/// ```
/// ```expect
/// string(3) "int"
/// string(5) "float"
/// string(6) "string"
/// string(5) "array"
/// string(5) "Named"
/// string(8) "stdClass"
/// string(17) "resource (closed)"
/// string(17) "resource (stream)"
/// string(4) "null"
/// string(4) "bool"
/// ```
fn get_debug_type(req: &mut Request) -> Result<Value, Thrown> {
    let value = the_value(req)?;
    let name: Vec<u8> = match value.as_ref() {
        ValueRef::Null | ValueRef::Undef => b"null".to_vec(),
        ValueRef::Bool(_) => b"bool".to_vec(),
        ValueRef::Int(_) => b"int".to_vec(),
        ValueRef::Float(_) => b"float".to_vec(),
        ValueRef::Str(_) => b"string".to_vec(),
        ValueRef::Array(_) => b"array".to_vec(),
        ValueRef::Object(object) => {
            let name = req
                .class_decl(object.class())
                .map(|decl| decl.name.0.to_vec())
                .unwrap_or_default();
            match name.iter().position(|b| *b == 0) {
                Some(end) => name[..end].to_vec(),
                None => name,
            }
        }
        ValueRef::Resource(resource) => match resource.type_name(req) {
            Some(kind) => {
                let mut name = b"resource (".to_vec();
                name.extend_from_slice(kind);
                name.push(b')');
                name
            }
            None => b"resource (closed)".to_vec(),
        },
        ValueRef::Reference(_) => b"unknown".to_vec(),
    };
    string_value(req, &name)
}

/// `settype(mixed &$var, string $type): bool`, php's
/// `PHP_FUNCTION(settype)` (`Z/ext/standard/type.c:88`): the variable
/// converted in place by `convert_to_*` for the type named, case
/// insensitively; `resource` and any unknown name are php's two
/// `ValueError`s.
///
/// ```php
/// $v = "12abc"; var_dump(settype($v, "integer"), $v);
/// $w = 5; settype($w, "Array"); var_dump($w);
/// $x = 1.5; settype($x, "string"); var_dump($x); settype($x, "bool"); var_dump($x); settype($x, "null"); var_dump($x);
/// $o = ["a" => 1]; settype($o, "object"); var_dump($o);
/// try { settype($w, "resource"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { settype($w, "x"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(12)
/// array(1) {
///   [0]=>
///   int(5)
/// }
/// string(3) "1.5"
/// bool(true)
/// NULL
/// object(stdClass)#1 (1) {
///   ["a"]=>
///   int(1)
/// }
/// Cannot convert to resource type
/// settype(): Argument #2 ($type) must be a valid type
/// ```
fn settype(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let given = req.argument(1).unwrap_or_default();
    let name = string_argument(req, 2)?.as_bytes().to_ascii_lowercase();
    let current = given.deref();
    let converted = match name.as_slice() {
        b"integer" | b"int" => Value::int(to_long(req, &current)?),
        b"float" | b"double" => Value::float(to_double(req, &current)?),
        b"string" => Value::string(to_string(req, &current)?),
        b"array" => {
            if current.as_array().is_some() {
                req.addref(&current);
                current
            } else if matches!(current.as_ref(), ValueRef::Null | ValueRef::Undef) {
                Value::array(php_ext::values::PhpArray::new(req)?)
            } else {
                Value::array(to_array(req, &current)?)
            }
        }
        b"object" => {
            if current.as_object().is_some() {
                req.addref(&current);
                current
            } else {
                Value::object(to_object(req, &current)?)
            }
        }
        b"bool" | b"boolean" => Value::bool(truthy(req, &current)),
        b"null" => Value::null(),
        b"resource" => {
            return Err(throw(req, "ValueError", "Cannot convert to resource type"));
        }
        _ => return Err(arg_value_error(req, 2, "be a valid type")),
    };
    match given.as_ref() {
        ValueRef::Reference(reference) => reference.set(req, converted),
        _ => req.release(converted),
    }
    Ok(Value::bool(true))
}

/// `intval(mixed $value, int $base = 10): int`, php's
/// `PHP_FUNCTION(intval)` (`Z/ext/standard/type.c:150`): anything but a
/// string, or any value under base 10, as `zval_get_long` reads it; a
/// string under another base read by `strtol`, with `0x`, `0` and, for
/// bases 0 and 2, `0b` prefixes, and clamped to the long's range.
///
/// ```php
/// var_dump(intval("0x1A", 16), intval("0x1A", 0), intval("012", 0), intval("0b11", 0), intval("-0b11", 2), intval(" 42abc"), intval("z", 36), intval("9999999999999999999999"), intval("-9999999999999999999999", 10), intval("777", 8), intval(42.9), intval("12", 5), intval(" +0x1f", 16), intval("1", 1), intval(null), intval([0]));
/// ```
/// ```expect
/// int(26)
/// int(26)
/// int(10)
/// int(3)
/// int(-3)
/// int(42)
/// int(35)
/// int(9223372036854775807)
/// int(-9223372036854775808)
/// int(511)
/// int(42)
/// int(7)
/// int(31)
/// int(0)
/// int(0)
/// int(1)
/// ```
fn intval(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let value = req.argument(1).unwrap_or_default().deref();
    let base = if req.argument_count() == 2 {
        long_argument(req, 2)?
    } else {
        10
    };
    let Some(text) = value.as_str_bytes().map(<[u8]>::to_vec) else {
        return Ok(Value::int(to_long(req, &value)?));
    };
    if base == 10 {
        return Ok(Value::int(to_long(req, &value)?));
    }
    if base == 0 || base == 2 {
        let start = text
            .iter()
            .position(|b| !is_space(*b))
            .unwrap_or(text.len());
        let rest = &text[start..];
        if rest.len() > 2 {
            let offset = usize::from(matches!(rest[0], b'-' | b'+'));
            if rest[offset] == b'0' && matches!(rest.get(offset + 1), Some(b'b' | b'B')) {
                let mut digits = rest[..offset].to_vec();
                digits.extend_from_slice(&rest[offset + 2..]);
                return Ok(Value::int(strtol(&digits, 2)));
            }
        }
    }
    Ok(Value::int(strtol(&text, base)))
}

/// `isspace` in the C locale.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

/// `ZEND_STRTOL(s, NULL, base)`, the C library's `strtoll`: whitespace,
/// a sign, a `0x` prefix for base 16 or 0 and a leading `0` for base 0,
/// the longest stretch of digits the base has, and the long's bound where it
/// overflows; zero for a base outside 2 to 36.
fn strtol(bytes: &[u8], base: i64) -> i64 {
    let mut at = bytes
        .iter()
        .position(|b| !is_space(*b))
        .unwrap_or(bytes.len());
    let mut negative = false;
    if at < bytes.len() && matches!(bytes[at], b'+' | b'-') {
        negative = bytes[at] == b'-';
        at += 1;
    }
    let mut base = base;
    let hex_prefix = at + 1 < bytes.len()
        && bytes[at] == b'0'
        && matches!(bytes[at + 1], b'x' | b'X')
        && bytes.get(at + 2).is_some_and(u8::is_ascii_hexdigit);
    if (base == 0 || base == 16) && hex_prefix {
        at += 2;
        base = 16;
    } else if base == 0 {
        base = if bytes.get(at) == Some(&b'0') { 8 } else { 10 };
    }
    if !(2..=36).contains(&base) {
        return 0;
    }
    let mut total: i128 = 0;
    let mut clamped = false;
    for &b in &bytes[at..] {
        let digit = match b {
            b'0'..=b'9' => i64::from(b - b'0'),
            b'a'..=b'z' => i64::from(b - b'a') + 10,
            b'A'..=b'Z' => i64::from(b - b'A') + 10,
            _ => break,
        };
        if digit >= base {
            break;
        }
        if !clamped {
            total = total * i128::from(base) + i128::from(digit);
            if total > i128::from(i64::MAX) + 1 {
                clamped = true;
            }
        }
    }
    if negative {
        i64::try_from(-total).unwrap_or(i64::MIN)
    } else {
        i64::try_from(total).unwrap_or(i64::MAX)
    }
}

/// `floatval(mixed $value): float` and its alias `doubleval`, php's
/// `PHP_FUNCTION(floatval)`: `zval_get_double`.
///
/// ```php
/// var_dump(floatval("1.5e3abc"), floatval("abc"), floatval(true), doubleval("-.5"), floatval([1]));
/// ```
/// ```expect
/// float(1500)
/// float(0)
/// float(1)
/// float(-0.5)
/// float(1)
/// ```
fn floatval(req: &mut Request) -> Result<Value, Thrown> {
    let value = the_value(req)?;
    Ok(Value::float(to_double(req, &value)?))
}

/// `boolval(mixed $value): bool`, php's `PHP_FUNCTION(boolval)`:
/// `zend_is_true`.
///
/// ```php
/// var_dump(boolval(0), boolval("0"), boolval("0.0"), boolval([]), boolval([0]), boolval(new stdClass), boolval(0.0), boolval(null));
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
fn boolval(req: &mut Request) -> Result<Value, Thrown> {
    let value = the_value(req)?;
    Ok(Value::bool(truthy(req, &value)))
}

/// `strval(mixed $value): string`, php's `PHP_FUNCTION(strval)`:
/// `zval_get_string`, with its warning for an array and its `Error` for
/// an object that has no `__toString`.
///
/// ```php
/// var_dump(strval(1.0), strval(0.1 + 0.2), strval(true), strval(null), strval(-0.0), strval(1e25));
/// var_dump(strval([1]));
/// try { strval(new stdClass); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "1"
/// string(3) "0.3"
/// string(1) "1"
/// string(0) ""
/// string(2) "-0"
/// string(7) "1.0E+25"
///
/// Warning: Array to string conversion in %s on line 3
/// string(5) "Array"
/// Object of class stdClass could not be converted to string
/// ```
fn strval(req: &mut Request) -> Result<Value, Thrown> {
    let value = the_value(req)?;
    Ok(Value::string(to_string(req, &value)?))
}

/// `array_is_list(array $array): bool`, php's
/// `PHP_FUNCTION(array_is_list)` over `zend_array_is_list`: whether the
/// keys are `0` to `count - 1` in order.
///
/// ```php
/// var_dump(array_is_list([]), array_is_list([1, 2]), array_is_list([1 => 1]), array_is_list(["a" => 1]), array_is_list([1 => 'b', 0 => 'a']));
/// try { array_is_list("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// array_is_list(): Argument #1 ($array) must be of type array, string given
/// ```
fn array_is_list(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let given = req.argument(1).unwrap_or_default();
    let coerced = php_ext::coerce::coerce_param(req, &given, &Type::Array, 1)?;
    let Some(array) = coerced.deref().as_array().copied() else {
        return Ok(Value::bool(true));
    };
    Ok(Value::bool(array.is_list()))
}

/// `is_numeric(mixed $value): bool`, php's `_zend_is_numeric`, with the
/// frameless arity 1: an int or a float, or a string
/// `is_numeric_string` reads whole, leading and trailing whitespace
/// allowed.
///
/// ```php
/// var_dump(is_numeric("1e5"), is_numeric(" 1"), is_numeric("1 "), is_numeric("0x1A"), is_numeric("."), is_numeric(1.5), is_numeric(null), is_numeric("-.5"), is_numeric("1e"));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
fn is_numeric(req: &mut Request) -> Result<Value, Thrown> {
    let value = the_value(req)?;
    Ok(Value::bool(match value.as_ref() {
        ValueRef::Int(_) | ValueRef::Float(_) => true,
        ValueRef::Str(s) => numeric_string(s.as_bytes()).is_some(),
        _ => false,
    }))
}

/// `is_callable(mixed $value, bool $syntax_only = false, &$callable_name = null): bool`, php's `PHP_FUNCTION(is_callable)`
/// (`Z/ext/standard/type.c:406`): whether `zend_is_callable_ex` accepts
/// the value, or under `$syntax_only` whether it has a callable's shape,
/// a string or a pair of a class or object and a string; the name php
/// would call it by written into `$callable_name` where it is given.
///
/// ```php
/// class K { function m() {} static function s() {} function __invoke() {} }
/// var_dump(is_callable("strlen"), is_callable("nope"), is_callable([new K, "m"], false, $n), $n, is_callable("A::b", true, $m), $m, is_callable(["K", "s"], false, $s), $s, is_callable(new K, false, $i), $i, is_callable([1, 2], true));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// string(4) "K::m"
/// bool(true)
/// string(4) "A::b"
/// bool(true)
/// string(4) "K::s"
/// bool(true)
/// string(11) "K::__invoke"
/// bool(false)
/// ```
fn is_callable(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 3)?;
    let value = req.argument(1).unwrap_or_default().deref();
    let syntax_only = if req.argument_count() >= 2 {
        bool_argument(req, 2)?
    } else {
        false
    };
    let answer = if syntax_only {
        match value.as_ref() {
            ValueRef::Str(_) => true,
            ValueRef::Array(array) => {
                let rows = array.snapshot();
                rows.len() == 2
                    && matches!(rows[0].1.as_ref(), ValueRef::Str(_) | ValueRef::Object(_))
                    && matches!(rows[1].1.as_ref(), ValueRef::Str(_))
            }
            ValueRef::Object(_) => callable(req, &value),
            _ => false,
        }
    } else {
        callable(req, &value)
    };
    if req.argument_count() == 3
        && let Some(ValueRef::Reference(cell)) = req.argument(3).as_ref().map(Value::as_ref)
    {
        let cell = *cell;
        let name = callable_name(req, &value);
        let name = Value::string(PhpString::new(req, &name)?);
        cell.set(req, name);
    }
    Ok(Value::bool(answer))
}

/// The name `zend_get_callable_name_ex` gives a value: a string as it
/// is, a pair as `Class::method`, a closure by its function's name, an
/// object as `Class::__invoke`, and anything else as its string.
fn callable_name(req: &mut Request, value: &Value) -> Vec<u8> {
    match value.as_ref() {
        ValueRef::Str(s) => s.as_bytes().to_vec(),
        ValueRef::Array(array) => {
            let rows = array.snapshot();
            if rows.len() != 2 {
                return b"Array".to_vec();
            }
            let class: Vec<u8> = match rows[0].1.as_ref() {
                ValueRef::Object(object) => class_name(req, object.class()),
                ValueRef::Str(s) => s.as_bytes().to_vec(),
                _ => return b"Array".to_vec(),
            };
            let method = match rows[1].1.as_ref() {
                ValueRef::Str(s) => s.as_bytes().to_vec(),
                _ => return b"Array".to_vec(),
            };
            let mut name = class;
            name.extend_from_slice(b"::");
            name.extend_from_slice(&method);
            name
        }
        ValueRef::Object(object) => {
            if let Some(closure) = object.closure(req) {
                let function = closure.function;
                return req
                    .function_decl(function)
                    .map(|decl| decl.name.0.to_vec())
                    .unwrap_or_default();
            }
            let mut name = class_name(req, object.class());
            name.extend_from_slice(b"::__invoke");
            name
        }
        _ => to_string(req, value)
            .map(|s| s.as_bytes().to_vec())
            .unwrap_or_default(),
    }
}

fn class_name(req: &Request, class: php_ext::values::ClassId) -> Vec<u8> {
    req.class_decl(class)
        .map(|decl| decl.name.0.to_vec())
        .unwrap_or_default()
}

/// Whether the object is an instance of the interface `name`, where the
/// engine has linked one.
fn implements(req: &mut Request, value: &Value, name: &[u8]) -> Result<bool, Thrown> {
    let Some(object) = value.as_object().copied() else {
        return Ok(false);
    };
    let Some(class) = lookup_class(req, name)? else {
        return Ok(false);
    };
    Ok(object.instance_of(req, class))
}

/// `is_iterable(mixed $value): bool`, php's `zend_is_iterable`: an array,
/// or an object of a class implementing `Traversable`.
///
/// ```php
/// var_dump(is_iterable([1]), is_iterable([]), is_iterable(new stdClass), is_iterable("x"), is_iterable(null));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn is_iterable(req: &mut Request) -> Result<Value, Thrown> {
    let value = the_value(req)?;
    if value.as_array().is_some() {
        return Ok(Value::bool(true));
    }
    Ok(Value::bool(implements(req, &value, b"Traversable")?))
}

/// `is_countable(mixed $value): bool`, php's `zend_is_countable`: an
/// array, or an object of a class implementing `Countable`.
///
/// ```php
/// var_dump(is_countable([]), is_countable([1, 2]), is_countable(new stdClass), is_countable(1), is_countable("abc"));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn is_countable(req: &mut Request) -> Result<Value, Thrown> {
    let value = the_value(req)?;
    if value.as_array().is_some() {
        return Ok(Value::bool(true));
    }
    Ok(Value::bool(implements(req, &value, b"Countable")?))
}
