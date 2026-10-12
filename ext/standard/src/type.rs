//! The natives of `type.c`: the type names (`gettype`, `get_debug_type`), the
//! conversions (`settype`, `intval`, `floatval` and its alias
//! `doubleval`, `boolval`, `strval`), and the remaining predicates
//! (`array_is_list`, the `is_*` checks with their aliases,
//! `is_numeric`, `is_callable`, `is_iterable`, `is_countable`).

use php_ext::abi::{Ref, Untyped};
use php_ext::attr::php_function;
use php_ext::call::lookup_class;
use php_ext::coerce::{
    callable_name, is_callable as callable, is_space, is_type, numeric_string, to_array, to_double,
    to_long, to_object, to_string, truthy,
};
use php_ext::raising::{Thrown, arg_value_error, throw_error, warning};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, TypeTag, Value, ValueRef, walk};

/// `gettype(mixed $value): string`, php's `PHP_FUNCTION(gettype)` over
/// `zend_zval_get_legacy_type`: the names php has kept since PHP 4,
/// `integer`, `double`, `boolean`, `NULL`, and `resource (closed)` for a
/// closed resource. php's compiler serves a call of one argument with
/// `ZEND_GET_TYPE` (`zend_compile_func_gettype`); what reaches the
/// native is a call by a name the program holds, a named or an unpacked
/// argument, and a callback.
///
/// ```php
/// $f = fopen('php://memory', 'r'); fclose($f); $g = 'gettype';
/// var_dump($g(1), $g(1.0), $g("s"), $g(null), $g([]), $g(new stdClass), $g(true), $g($f), $g(fopen('php://memory', 'r')));
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
///
/// The declarations of `type.c`, as `basic_functions.stub.php` has them.
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
#[php_function]
fn gettype(req: &mut Request, value: Value) -> Result<Owner<PhpString>, Thrown> {
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
    PhpString::new(req, name)
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
#[php_function]
fn get_debug_type(req: &mut Request, value: Value) -> Result<Owner<PhpString>, Thrown> {
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
    PhpString::new(req, &name)
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
/// $s = new stdClass; try { settype($s, "string"); } catch (Error $e) { echo $e->getMessage(), "\n"; } var_dump($s);
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
/// Object of class stdClass could not be converted to string
/// string(0) ""
/// ```
///
/// A NAN made `null`, an array or an object is php's warning first,
/// `zend_nan_coerced_to_type_warning`, and the conversion reads the
/// variable again after it, so what an error handler wrote there is
/// what the array or the object holds (`convert_scalar_to_array`,
/// `convert_to_object`):
///
/// ```php
/// set_error_handler(function ($no, $str) { global $n; $n = "h"; echo $str, "\n"; });
/// foreach (['null', 'array', 'object'] as $t) { $n = NAN; settype($n, $t); var_dump($n); }
/// ```
/// ```expect
/// unexpected NAN value was coerced to null
/// NULL
/// unexpected NAN value was coerced to array
/// array(1) {
///   [0]=>
///   string(1) "h"
/// }
/// unexpected NAN value was coerced to object
/// object(stdClass)#2 (1) {
///   ["scalar"]=>
///   string(1) "h"
/// }
/// ```
#[php_function]
fn settype(req: &mut Request, var: Ref<Value>, r#type: PhpString) -> Result<bool, Thrown> {
    let name = r#type.as_bytes().to_ascii_lowercase();
    let current = var.get().copied().unwrap_or_default();
    if current.as_float().is_some_and(f64::is_nan)
        && let Some(target) = match name.as_slice() {
            b"null" => Some("null"),
            b"array" => Some("array"),
            b"object" => Some("object"),
            _ => None,
        }
    {
        warning(
            req,
            &format!("unexpected NAN value was coerced to {target}"),
        );
        let now = var.reference().map_or(current, |cell| cell.get());
        let now = Owner::take(req, now);
        let converted = match target {
            "null" => {
                now.give_up(req);
                Owner::null()
            }
            "array" => {
                let mut array = PhpArray::new(req)?;
                array.append(req, now)?;
                array.into_value()
            }
            // `object_init` and `scalar` added to its table, which is
            // the object an array of that one row casts to.
            _ => {
                let mut row = PhpArray::new(req)?;
                row.set_str(req, b"scalar", now)?;
                let object = to_object(req, &Value::array(*row));
                row.give_up(req);
                object?.into_value()
            }
        };
        var.set(req, converted)?;
        return Ok(true);
    }
    let converted = match name.as_slice() {
        b"integer" | b"int" => {
            let n = to_long(req, &current)?;
            Owner::int(n)
        }
        b"float" | b"double" => {
            let f = to_double(req, &current)?;
            Owner::float(f)
        }
        b"string" => match to_string(req, &current) {
            Ok(string) => string.into_value(),
            // `convert_to_string` leaves the empty string behind a
            // conversion that threw.
            Err(thrown) => {
                let empty = PhpString::new(req, b"")?;
                var.set(req, empty.into_value())?;
                return Err(thrown);
            }
        },
        b"array" => {
            if current.as_array().is_some() {
                Owner::take(req, current)
            } else if matches!(current.as_ref(), ValueRef::Null | ValueRef::Undef) {
                php_ext::values::PhpArray::new(req)?.into_value()
            } else {
                to_array(req, &current)?.into_value()
            }
        }
        b"object" => {
            if current.as_object().is_some() {
                Owner::take(req, current)
            } else {
                to_object(req, &current)?.into_value()
            }
        }
        b"bool" | b"boolean" => {
            let truth = truthy(req, &current)?;
            Owner::bool(truth)
        }
        b"null" => Owner::null(),
        b"resource" => {
            return Err(throw_error(
                req,
                "ValueError",
                "Cannot convert to resource type",
            ));
        }
        _ => return Err(arg_value_error(req, 2, "be a valid type")),
    };
    var.set(req, converted)?;
    Ok(true)
}

/// `intval(mixed $value, int $base = 10): int`, php's
/// `PHP_FUNCTION(intval)` (`Z/ext/standard/type.c:150`): anything but a
/// string, or any value under base 10, as `zval_get_long` reads it; a
/// string under another base read by `strtol`, with `0x`, `0` and, for
/// bases 0 and 2, `0b` prefixes, and clamped to the long's range. php's
/// compiler serves a call of one argument with `ZEND_CAST`
/// (`zend_compile_func_cast`); what reaches the native is a call of two,
/// a call by a name the program holds, a named or an unpacked argument,
/// and a callback.
///
/// ```php
/// $i = 'intval'; var_dump($i("0x1A", 16), $i("0x1A", 0), $i("012", 0), $i("0b11", 0), $i("-0b11", 2), $i(" 42abc"), $i("z", 36), $i("9999999999999999999999"), $i("-9999999999999999999999", 10), $i("777", 8), $i(42.9), $i("12", 5), $i(" +0x1f", 16), $i("1", 1), $i(null), $i([0]));
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
///
/// The white space `strtol` skips is C's `isspace` under the request's
/// `LC_CTYPE`, `\v` and `\f` among it, and under `C` not 0xA0.
///
/// ```php
/// var_dump(intval("\v 12", 16), intval("\f12", 0));
/// setlocale(LC_CTYPE, "C"); var_dump(intval("\xa012", 16));
/// ```
/// ```expect
/// int(18)
/// int(12)
/// int(0)
/// ```
#[php_function]
fn intval(req: &mut Request, value: Value, #[php(default = 10)] base: i64) -> Result<i64, Thrown> {
    let Some(text) = value.as_str_bytes().map(<[u8]>::to_vec) else {
        return to_long(req, &value);
    };
    if base == 10 {
        return to_long(req, &value);
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
                return Ok(strtol(&digits, 2));
            }
        }
    }
    Ok(strtol(&text, base))
}

/// `ZEND_STRTOL(s, NULL, base)`, the C library's `strtoll`: whitespace,
/// a sign, a `0x` prefix for base 16 or 0 and a leading `0` for base 0,
/// the longest stretch of digits the base has, and the long's bound where it
/// overflows; zero for a base outside 2 to 36. Kept beside the face's
/// `strtol`, which reads base 10 alone; the white space it skips is the
/// face's `is_space`, C's under the thread's `LC_CTYPE`.
pub(crate) fn strtol(bytes: &[u8], base: i64) -> i64 {
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
/// `PHP_FUNCTION(floatval)`: `zval_get_double`. php's compiler serves a
/// call of one argument with `ZEND_CAST` (`zend_compile_func_cast`);
/// what reaches the native is a call by a name the program holds, a
/// named or an unpacked argument, and a callback.
///
/// ```php
/// $f = 'floatval'; $d = 'doubleval'; var_dump($f("1.5e3abc"), $f("abc"), $f(true), $d("-.5"), $f([1]));
/// ```
/// ```expect
/// float(1500)
/// float(0)
/// float(1)
/// float(-0.5)
/// float(1)
/// ```
#[php_function(aliases(doubleval))]
fn floatval(req: &mut Request, value: Value) -> Result<f64, Thrown> {
    to_double(req, &value)
}

/// `boolval(mixed $value): bool`, php's `PHP_FUNCTION(boolval)`:
/// `zend_is_true`. php's compiler serves a call of one argument with
/// `ZEND_BOOL` (`zend_compile_func_cast`); what reaches the native is a
/// call by a name the program holds, a named or an unpacked argument,
/// and a callback.
///
/// ```php
/// $b = 'boolval'; var_dump($b(0), $b("0"), $b("0.0"), $b([]), $b([0]), $b(new stdClass), $b(0.0), $b(null));
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
#[php_function]
fn boolval(req: &mut Request, value: Value) -> Result<bool, Thrown> {
    truthy(req, &value)
}

/// `strval(mixed $value): string`, php's `PHP_FUNCTION(strval)`:
/// `zval_get_string`, with its warning for an array and its `Error` for
/// an object that has no `__toString`. php's compiler serves a call of
/// one argument with `ZEND_CAST` (`zend_compile_func_cast`); what
/// reaches the native is a call by a name the program holds, a named or
/// an unpacked argument, and a callback.
///
/// ```php
/// $s = 'strval'; var_dump($s(1.0), $s(0.1 + 0.2), $s(true), $s(null), $s(-0.0), $s(1e25));
/// var_dump($s([1]));
/// try { $s(new stdClass); } catch (Error $e) { echo $e->getMessage(), "\n"; }
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
#[php_function]
fn strval(req: &mut Request, value: Value) -> Result<Owner<PhpString>, Thrown> {
    to_string(req, &value)
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
#[php_function]
fn array_is_list(array: PhpArray) -> bool {
    array.is_list()
}

/// `is_null(mixed $value): bool`, `php_is_type` under `IS_NULL`
/// (`Z/ext/standard/type.c:239`) through the face's `is_type`. The
/// one-argument call is `ZEND_TYPE_CHECK` over the engine's kernel and
/// never reaches here, as with every native of the family; what does is
/// the call `ZEND_PARSE_PARAMETERS_START(1, 1)` refuses.
///
/// ```php
/// var_dump(is_null());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_null() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_null()
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn is_null(value: Value) -> bool {
    is_type(&value, TypeTag::Null)
}

/// `is_resource(mixed $value): bool`, `php_is_type` under `IS_RESOURCE`:
/// a closed resource is not one.
///
/// ```php
/// var_dump(is_resource());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_resource() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_resource()
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn is_resource(value: Value) -> bool {
    is_type(&value, TypeTag::Resource)
}

/// `is_bool(mixed $value): bool`, `PHP_FUNCTION(is_bool)`: `IS_FALSE`
/// or `IS_TRUE`.
///
/// ```php
/// var_dump(is_bool());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_bool() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_bool()
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn is_bool(value: Value) -> bool {
    is_type(&value, TypeTag::Bool)
}

/// `is_int(mixed $value): bool` and its aliases `is_integer` and
/// `is_long`, `php_is_type` under `IS_LONG`.
///
/// ```php
/// var_dump(is_int());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_int() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_int()
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function(aliases(is_integer, is_long))]
fn is_int(value: Value) -> bool {
    is_type(&value, TypeTag::Int)
}

/// `is_float(mixed $value): bool` and its alias `is_double`,
/// `php_is_type` under `IS_DOUBLE`.
///
/// ```php
/// var_dump(is_float());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_float() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_float()
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function(aliases(is_double))]
fn is_float(value: Value) -> bool {
    is_type(&value, TypeTag::Float)
}

/// `is_string(mixed $value): bool`, `php_is_type` under `IS_STRING`.
///
/// ```php
/// var_dump(is_string());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_string() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_string()
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn is_string(value: Value) -> bool {
    is_type(&value, TypeTag::String)
}

/// `is_array(mixed $value): bool`, `php_is_type` under `IS_ARRAY`.
///
/// ```php
/// var_dump(is_array());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_array() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_array()
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn is_array(value: Value) -> bool {
    is_type(&value, TypeTag::Array)
}

/// `is_object(mixed $value): bool`, `php_is_type` under `IS_OBJECT`.
///
/// ```php
/// var_dump(is_object());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_object() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_object()
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn is_object(value: Value) -> bool {
    is_type(&value, TypeTag::Object)
}

/// `is_scalar(mixed $value): bool`, `PHP_FUNCTION(is_scalar)`
/// (`Z/ext/standard/type.c:389`): a bool, a long, a double or a string,
/// which is the face's `is_type` asked four times.
///
/// ```php
/// var_dump(is_scalar(1, 2));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_scalar() expects exactly 1 argument, 2 given in %s:2
/// Stack trace:
/// #0 %s(2): is_scalar(1, 2)
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn is_scalar(value: Value) -> bool {
    [TypeTag::Bool, TypeTag::Int, TypeTag::Float, TypeTag::String]
        .into_iter()
        .any(|tag| is_type(&value, tag))
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
#[php_function(frameless(1))]
fn is_numeric(value: Value) -> bool {
    match value.as_ref() {
        ValueRef::Int(_) | ValueRef::Float(_) => true,
        ValueRef::Str(s) => numeric_string(s.as_bytes()).is_some(),
        _ => false,
    }
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
///
/// A closure is named by its function, one made of a method with the
/// method's class before it, and a value of no callable's shape by its
/// string:
///
/// ```php
/// class K { static function s() {} function __invoke() {} }
/// foreach ([K::s(...), (new K)(...), strlen(...), function () {}, 5, 1.5, null] as $v) {
///     is_callable($v, false, $name);
///     var_dump($name);
/// }
/// ```
/// ```expect
/// string(4) "K::s"
/// string(11) "K::__invoke"
/// string(6) "strlen"
/// string(%d) "{closure:%s:3}"
/// string(1) "5"
/// string(3) "1.5"
/// string(0) ""
/// ```
#[php_function]
fn is_callable(
    req: &mut Request,
    value: Value,
    #[php(default = false)] syntax_only: bool,
    #[php(default = null)] callable_name: Ref<Untyped<Value>>,
) -> Result<bool, Thrown> {
    let answer = if syntax_only {
        match value.as_ref() {
            ValueRef::Str(_) => true,
            ValueRef::Array(array) => {
                let mut rows = walk::rows(req, *array).map(|(_, value)| value.deref());
                array.len() == 2
                    && rows.next().is_some_and(|v| {
                        matches!(v.as_ref(), ValueRef::Str(_) | ValueRef::Object(_))
                    })
                    && rows
                        .next()
                        .is_some_and(|v| matches!(v.as_ref(), ValueRef::Str(_)))
            }
            ValueRef::Object(_) => callable(req, &value),
            _ => false,
        }
    } else {
        callable(req, &value)
    };
    if callable_name.is_given() {
        let name = self::callable_name(req, &value)?;
        callable_name.set(req, Untyped(name.into_value()))?;
    }
    Ok(answer)
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
#[php_function]
fn is_iterable(req: &mut Request, value: Value) -> Result<bool, Thrown> {
    if value.as_array().is_some() {
        return Ok(true);
    }
    implements(req, &value, b"Traversable")
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
#[php_function]
fn is_countable(req: &mut Request, value: Value) -> Result<bool, Thrown> {
    if value.as_array().is_some() {
        return Ok(true);
    }
    implements(req, &value, b"Countable")
}
