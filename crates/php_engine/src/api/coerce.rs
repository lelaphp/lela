//! The coercion and conversion group (design §4.4): php's conversions
//! through the engine's own kernels, honouring the caller file's
//! `strict_types`. The skeleton converts the cases that need no heap and
//! no user code (scalars, numeric strings) and reports the rest as the
//! `TypeError` php would raise, so a native written against these
//! signatures reads as php until the kernels land.

// php's `(double)` of a long and `(zend_long)` of a double are what these
// casts are; the kernel that copies `zend_dval_to_lval` replaces the latter.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::cmp::Ordering;

pub use php_heap::Exponent;

/// C's `strtol(s, NULL, 10)`, `atoi` and `isspace` as php's extensions
/// call them, `ZEND_STRTOL`, `ZEND_ATOL` and `zend_atoi`'s C library
/// (`Z/Zend/zend_long.h:77`): the engine's one reading, so an extension
/// reads a value past `int` or the long as php does (#2634); and
/// `c_str`, a string's bytes as C reads them, up to its first NUL (#2763).
pub use crate::c_number::{atoi, c_str, is_space, strtol};
use php_heap::ZvalType;
use php_unit::decl::{Bare, Type};

use crate::api::abi::Callable;
use crate::api::raising::{Thrown, arg_type_error};
use crate::api::request::{Deprecations, Request};
use crate::api::values::{
    ClassId, Key, Owner, PhpArray, PhpObject, PhpString, TypeTag, Value, ValueRef, adopted,
};

/// php's binary operators as `zend_binary_op` names them, and the one
/// unary operator an object's `do_operation` is asked for besides,
/// `ZEND_BW_NOT`, with no right operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Concat,
    BitwiseAnd,
    BitwiseOr,
    BitwiseXor,
    ShiftLeft,
    ShiftRight,
    BitwiseNot,
}

/// `zend_is_true`, the kernel [`crate::ops::is_true`] through the face:
/// a NAN warns as it is coerced, which is why the request is here. The
/// `Err` is the request leaving by `zend_bailout`, the fatal an object
/// that refuses `_IS_BOOL` raises among it, which longjmps out of the
/// native that asked; a throwable raised on the way stays pending, as
/// php's `EG(exception)` does, and the native goes on. `array_filter`
/// over a callback that answers such an object is the witness, in
/// `ext/zend_test`.
pub fn truthy(req: &mut Request, value: &Value) -> Result<bool, Thrown> {
    crate::ops::is_true(req, value)
}

/// `is_numeric_string_ex`: the `int` or `float` a string denotes, with
/// php's whitespace rules, or `None`: [`php_heap::numeric_prefix`], the
/// one reader of a numeric string, over the whole string.
#[must_use]
pub fn numeric_string(bytes: &[u8]) -> Option<Value> {
    Some(match numeric_whole(bytes)? {
        php_heap::Number::Long(n) => Value::int(n),
        php_heap::Number::Double(d) => Value::float(d),
    })
}

/// `zend_dval_to_lval`: saturating in 8.5's modular sense is the kernel's;
/// the skeleton truncates and maps non-finite to zero.
#[must_use]
pub fn float_to_long(f: f64) -> i64 {
    if f.is_finite() {
        // `as` saturates; php wraps modulo 2^64 since 7.0 (`zend_dval_to_lval` with
        // `ZEND_DOUBLE_WRAPS`) and 8.5 keeps that, which the kernel copies.
        f as i64
    } else {
        0
    }
}

/// `zend_long_to_str`: the digits as a string of the request's, which
/// for one digit is the process's interned one, as php's `ZSTR_CHAR` is.
#[cfg_attr(debug_assertions, track_caller)]
pub fn int_to_string(req: &mut Request, n: i64) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, crate::c_number::Long::of(n).as_bytes())
}

/// `zend_gcvt(f, precision ? precision : 1, '.', exponent, buf)`, as
/// `smart_str_append_double` and `zend_double_to_str` call it, the one
/// [`php_heap::double_to_string`] through the face: `precision`
/// significant digits, a `0` read as one, or with `-1` the shortest text that reads back as
/// the same double, in the form php chooses, and not yet a string of the
/// request's. The exponent character is the caller's, as it is at each
/// of php's call sites: `Exponent::Upper` for `var_export`, `%G` and
/// `%H`, `Exponent::Lower` for `json_encode`, `%g` and `%h`. A
/// `zero_fraction` such as `smart_str_append_double` takes is the
/// caller's too, since only `var_export` and `json_encode` ask for it.
///
/// ```php
/// echo var_export(1e25, true), "|", var_export(1e-5, true), "|", var_export(0.1, true), "|", json_encode([1e25, 1e-5, 0.1]), "|", sprintf('%.14G|%.14G|%.14G', 1e25, 1e-5, 0.1), "|", sprintf('%.14g', 1e25);
/// ```
/// ```expect
/// 1.0E+25|1.0E-5|0.1|[1.0e+25,1.0e-5,0.1]|1.0E+25|1.0E-5|0.1|1.0e+25
/// ```
///
/// ```php
/// ini_set('serialize_precision', '14');
/// ini_set('precision', '-1');
/// echo var_export(1/3, true), "|", json_encode(1/3), "|", sprintf('%.14G', 1/3), "|", 1/3, "|", 0.1 + 0.2;
/// ```
/// ```expect
/// 0.33333333333333|0.33333333333333|0.33333333333333|0.3333333333333333|0.30000000000000004
/// ```
///
/// This is the one rendering a float has in the engine; a `float` a
/// `string` parameter or `echo` takes is [`to_string`], which is
/// `zend_double_to_str` at the request's `precision` over the same
/// `zend_gcvt`, so `1.5` reads `1.5` on either path and never at a fixed
/// number of places. The shorter form, the exponent, the signed zero and
/// the two non-finite words, through each caller's own arm (#408):
///
/// ```php
/// function s(string $s): string { return $s; }
/// foreach ([1.5, 1e20, -0.0, INF, NAN] as $f) {
///     echo var_export($f, true), "|", sprintf('%.14G', $f), "|", s($f), "|", $f, "\n";
/// }
/// echo json_encode([1.5, 1e20, -0.0]), "\n";
/// ```
/// ```expect
/// 1.5|1.5|1.5|1.5
/// 1.0E+20|1.0E+20|1.0E+20|1.0E+20
/// -0.0|-0|-0|-0
/// INF|INF|INF|INF
/// NAN|NaN|
/// Warning: unexpected NAN value was coerced to string in %s on line 2
/// NAN|
/// Warning: unexpected NAN value was coerced to string in %s on line 4
/// NAN
/// [1.5,1.0e+20,-0]
/// ```
#[must_use]
pub fn float_format(f: f64, precision: i32, exponent: Exponent) -> String {
    php_heap::double_to_string(f, precision, exponent)
}

/// `zend_gcvt(f, ndigit, '.', exponent, buf)` with `ndigit` as its caller
/// holds it, [`php_heap::gcvt`] through the face: the call `serialize`,
/// `json_encode` and soap's `to_xml_double` make with the ini entry as
/// it stands, where [`float_format`] reads a `0` as one. At `0` the one
/// digit is written exponentially once the point stands past it, and a
/// non-finite double is no text at all:
///
/// ```php
/// ini_set('serialize_precision', 0); ini_set('precision', 0);
/// echo serialize([10/3, INF]), "|", json_encode(10/3), "|", 10/3, "|", INF;
/// ```
/// ```expect
/// a:2:{i:0;d:3.0E+0;i:1;d:;}|3.0e+0|3|I
/// ```
#[must_use]
pub fn gcvt(f: f64, ndigit: i32, exponent: Exponent) -> String {
    php_heap::gcvt(f, ndigit, exponent)
}

/// `zval_get_long`, the kernel [`crate::ops::get_long`] through the
/// face, what `php_get_long` reads a `usort` callback's answer with: the
/// number a string opens with, `is_numeric_string` with `allow_errors`,
/// so `"2abc"` is two and a string that opens with none is zero; a
/// float capped as `zend_dval_to_lval_cap` caps it; and an object
/// php's warning that it could not be converted, and one. Nothing here
/// throws; the `Result` is the face's shape.
pub fn to_long(req: &mut Request, value: &Value) -> Result<i64, Thrown> {
    Ok(crate::ops::get_long(req, value))
}

/// `zval_try_get_long`, the kernel [`crate::ops::to_long`]
/// (`zendi_try_get_long`) through the face, what a native reads an
/// argument it takes as `mixed` and holds to `int` with: the long, or
/// `None` where php sets `failed`, an array, a resource, an object whose
/// `cast_object` answers no int, and a string that opens with no
/// number, for which the native raises its own `TypeError`. A string
/// with more after its number is php's `A non-numeric value
/// encountered` warning, a float or a float-string that loses precision
/// its deprecation, and what a handler threw from either is the `Err`.
pub fn try_long(req: &mut Request, value: &Value) -> Result<Option<i64>, Thrown> {
    crate::ops::to_long(req, &value.deref())
}

/// `zval_get_double`, the kernel [`crate::ops::get_double`] through the
/// face, what `SORT_NUMERIC` reads each element with: `zend_strtod`
/// over a string, so `"9a"` is nine, and an object php's warning and
/// one. Nothing here throws; the `Result` is the face's shape.
pub fn to_double(req: &mut Request, value: &Value) -> Result<f64, Thrown> {
    Ok(crate::ops::get_double(req, value))
}

/// `zval_get_string`, the kernel [`crate::ops::to_string`] through the
/// face: an object goes through its `__toString`, and one with none is
/// php's `Error` for what could not be converted; a float is
/// `zend_double_to_str` at the request's `precision`, with php's
/// warning where it converted a NAN; an array is php's warning and the
/// word `Array`. The string handed back is the caller's to own, as
/// `zval_get_string`'s is: a string value's own with one more owner
/// (`zend_string_copy`), and a new string of the request's for
/// everything else.
#[cfg_attr(debug_assertions, track_caller)]
pub fn to_string(req: &mut Request, value: &Value) -> Result<Owner<PhpString>, Thrown> {
    adopted(crate::ops::to_string(req, value))
}

/// `zval_get_tmp_string` and `zend_tmp_string_release` around one read:
/// `value`'s text, as [`to_string`] converts it, lent to `read`, and the
/// string the conversion made given up whatever `read` answers. A
/// string, or a reference to one, is lent as it stands, with no owner
/// taken, as php's `tmp` is `NULL` for one: `read` is handed no request,
/// so nothing it does can call into the program or free what it reads.
/// What a native calls where it reads a value's text once, so no
/// conversion outlives the read; the conversion's own refusal, an
/// object with no `__toString` or what its `__toString` threw, is the
/// `Err`, and `read` is not called.
///
/// The bytes are lent for the call and no longer:
///
/// ```compile_fail
/// # use php_engine::api::coerce::with_string;
/// # use php_engine::api::raising::Thrown;
/// # use php_engine::api::request::Request;
/// # use php_engine::api::values::Value;
/// fn kept<'a>(req: &mut Request, value: &Value) -> Result<&'a [u8], Thrown> {
///     with_string(req, value, |bytes| bytes)
/// }
/// # fn main() {}
/// ```
pub fn with_string<R>(
    req: &mut Request,
    value: &Value,
    read: impl FnOnce(&[u8]) -> R,
) -> Result<R, Thrown> {
    let held = value.deref();
    if let Some(string) = held.as_php_string() {
        return Ok(read(string.as_bytes()));
    }
    let made = to_string(req, &held)?;
    let answer = read(made.as_bytes());
    made.give_up(req);
    Ok(answer)
}

/// `convert_scalar_to_number`: an `int` or a `float`, a string the
/// number it opens with (`is_numeric_string` with `allow_errors`,
/// silently) and zero for none, what a class's `cast_object` to
/// [`TypeTag::Number`] answers from its text.
pub fn to_number(req: &mut Request, value: &Value) -> Result<Value, Thrown> {
    match value.as_ref() {
        ValueRef::Float(f) => Ok(Value::float(f)),
        ValueRef::Str(s) => Ok(match php_heap::numeric_prefix(s.as_bytes()) {
            Some((php_heap::Number::Long(n), _)) => Value::int(n),
            Some((php_heap::Number::Double(d), _)) => Value::float(d),
            None => Value::int(0),
        }),
        ValueRef::Reference(r) => to_number(req, &r.get()),
        _ => to_long(req, value).map(Value::int),
    }
}

/// `(array)`, the kernel [`crate::ops::to_array`] through the face:
/// `null` the empty array, a scalar the one-element array, an object
/// its properties with php's mangled names, and an array itself. What
/// is handed back is the caller's to own, as `zval_get_string`'s is:
/// an array value's own with one more owner, a new one of the
/// request's otherwise.
#[cfg_attr(debug_assertions, track_caller)]
pub fn to_array(req: &mut Request, value: &Value) -> Result<Owner<PhpArray>, Thrown> {
    adopted(crate::ops::to_array(req, value))
}

/// `(object)`, the kernel [`crate::ops::to_object`] through the face:
/// `null` an empty `stdClass`, a scalar a `stdClass` with the property
/// `scalar`, an array a `stdClass` over its elements, and an object
/// itself. What is handed back is the caller's to own: an object
/// value's own with one more owner, a new one of the request's
/// otherwise.
#[cfg_attr(debug_assertions, track_caller)]
pub fn to_object(req: &mut Request, value: &Value) -> Result<Owner<PhpObject>, Thrown> {
    adopted(crate::ops::to_object(req, value))
}

/// `zend_is_callable` with the request's scope.
#[must_use]
pub fn is_callable(req: &mut Request, value: &Value) -> bool {
    req.is_callable_value(value, Deprecations::Raised)
}

/// `zend_is_callable_ex` with its `*error` (`Z/Zend/zend_API.c:4250`):
/// the callable a value names, or the sentence php words the refusal
/// with, one per shape. What `zend_fcall_info_init` hands its caller to
/// raise, which is `ob_start`'s warning and every other native that
/// prints php's own words rather than "must be a valid callback". The
/// request is taken mutably because the resolution raises php's
/// deprecations for `self`, `parent` and `static` as the class half.
pub fn callable_checked(req: &mut Request, value: &Value) -> Result<Callable, String> {
    req.callable_checked(value)
        .map_err(|refusal| refusal.to_string())
}

/// `zend_get_callable_name` (`Z/Zend/zend_API.c:4075`): the name php
/// gives a value as a callable, whether or not it is one. A string is
/// itself, a pair `Class::method`, a closure its function's name, one
/// `zend_create_fake_closure` made of a method with its scope before
/// it, any other object `Class::__invoke`, and any other value its
/// string, as `zval_get_string` makes it.
#[cfg_attr(debug_assertions, track_caller)]
pub fn callable_name(req: &mut Request, value: &Value) -> Result<Owner<PhpString>, Thrown> {
    let held = value.deref();
    adopted(match held.as_ref() {
        ValueRef::Str(_) | ValueRef::Array(_) | ValueRef::Object(_) => {
            let named = req.callable_name(&held);
            req.string(named.as_bytes())
        }
        _ => crate::ops::to_string(req, &held),
    })
}

/// The callable a value names as `zend_call_function` resolves one it
/// was handed uncached, `call_user_function`'s way: the deprecations
/// raised, and a value that names none php's `Error` "Invalid callback
/// %s, %s" with the name `zend_get_callable_name_ex` gives it and the
/// resolver's sentence (`Z/Zend/zend_execute_API.c:838-846`).
///
/// ```php
/// ini_set("assert.callback", "nope");
/// try { assert(false); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), " / ", $e->getPrevious()->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: ini_set(): assert.callback INI setting is deprecated in %s on line 2
/// AssertionError: assert(false) / Invalid callback nope, function "nope" not found or invalid function name
/// ```
pub fn to_callable(req: &mut Request, value: &Value) -> Result<Callable, Thrown> {
    match req.callable_checked(value) {
        Ok(_) if req.is_pending() => Err(Thrown::of(req.take_pending_view())),
        Ok(callable) => Ok(callable),
        // `zend_get_callable_name_ex`'s string stands beside the
        // sentence, made for an array or an object and the string's own
        // for a string.
        Err(refusal) => Err(refusal.across(req, |req| {
            let name = req.callable_name(value);
            let message = format!("Invalid callback {name}, {refusal}");
            let made = match value.deref().as_ref() {
                ValueRef::Array(_) | ValueRef::Object(_) => {
                    match PhpString::new(req, name.as_bytes()) {
                        Ok(made) => Some(made),
                        Err(refused) => return refused,
                    }
                }
                _ => None,
            };
            let thrown = crate::api::raising::throw_over_pending(req, "Error", &message);
            if let Some(made) = made {
                made.give_up(req);
            }
            thrown
        })),
    }
}

/// The array key a value makes, `array_set_zval_key`'s offset rules
/// (`Z/Zend/zend_API.c:2256`), which are the write kernel's
/// ([`crate::ops::array_offset`]): `null` is `""` behind its deprecation,
/// a `bool` an `int`, a `float` an `int` behind the deprecation for a
/// fraction, a resource its id behind its warning, and an `array` or an
/// `object` the `TypeError` "Cannot access offset of type T on array". A
/// string key is the caller's, one owner, as [`PhpArray::set`] takes it,
/// and `value` keeps its own.
///
/// ```php
/// class It implements Iterator {
///     private $i = 0;
///     function current(): mixed { return $this->i; }
///     function key(): mixed { return str_repeat(chr(97 + $this->i), 2); }
///     function next(): void { $this->i++; }
///     function rewind(): void { $this->i = 0; }
///     function valid(): bool { return $this->i < 3; }
/// }
/// $a = iterator_to_array(new It);
/// $b = str_repeat('z', 64);
/// var_dump(array_keys($a));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   string(2) "aa"
///   [1]=>
///   string(2) "bb"
///   [2]=>
///   string(2) "cc"
/// }
/// ```
///
/// ```php
/// var_dump(array_column([["k" => 1.5, "v" => 1], ["k" => null, "v" => 2], ["k" => true, "v" => 3]], "v", "k"));
/// try { array_column([["k" => new stdClass, "v" => 1]], "v", "k"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Deprecated: Implicit conversion from float 1.5 to int loses precision in %s on line 2
///
/// Deprecated: Using null as an array offset is deprecated, use an empty string instead in %s on line 2
/// array(2) {
///   [1]=>
///   int(3)
///   [""]=>
///   int(2)
/// }
/// Cannot access offset of type stdClass on array
/// ```
#[cfg_attr(debug_assertions, track_caller)]
pub fn key_of(req: &mut Request, value: &Value) -> Result<Owner<Key>, Thrown> {
    let value = value.deref();
    let key = Key::of(crate::ops::array_offset(req, &value, false)?);
    // The kernel names a string offset's own string; a `null`'s `""` it
    // made is the caller's already.
    if let (Key::Str(s), ValueRef::Str(_)) = (&key, value.as_ref()) {
        req.addref(&Value::string(*s));
    }
    Ok(Owner::adopt(key))
}

/// `count()` of a countable value, the kernel [`crate::ops::count`]
/// through the face: an array's element count, and php's `TypeError`
/// for anything else, naming the native that is active, `sizeof` for
/// the alias, as `zend_argument_type_error` names it.
pub fn count(req: &mut Request, value: &Value) -> Result<i64, Thrown> {
    let sizeof = req.active_native() == "sizeof";
    crate::ops::count(req, value, sizeof)
}

/// `php_is_type` (`Z/ext/standard/type.c`), the kernel
/// [`crate::ops::type_check`] through the face: whether the value is of
/// `tag`, a reference followed to what it holds, both of a bool's types
/// for [`TypeTag::Bool`], and a closed resource not a resource
/// (`zend_rsrc_list_get_rsrc_type`). What `is_int`, `is_string` and
/// their kin answer where the compiler did not emit `ZEND_TYPE_CHECK`
/// for the call.
#[must_use]
pub fn is_type(value: &Value, tag: TypeTag) -> bool {
    let mask = match tag {
        TypeTag::Undef => 1 << (ZvalType::Undef as u32),
        TypeTag::Null => 1 << (ZvalType::Null as u32),
        TypeTag::Bool => (1 << (ZvalType::False as u32)) | (1 << (ZvalType::True as u32)),
        TypeTag::Int => 1 << (ZvalType::Long as u32),
        TypeTag::Float => 1 << (ZvalType::Double as u32),
        TypeTag::String => 1 << (ZvalType::String as u32),
        TypeTag::Array => 1 << (ZvalType::Array as u32),
        TypeTag::Object => 1 << (ZvalType::Object as u32),
        TypeTag::Resource => 1 << (ZvalType::Resource as u32),
        TypeTag::Reference => 1 << (ZvalType::Reference as u32),
        TypeTag::Number => (1 << (ZvalType::Long as u32)) | (1 << (ZvalType::Double as u32)),
    };
    crate::ops::type_check(value, mask)
}

/// `zend_zval_value_name` (`Z/Zend/zend_API.c:157`), the kernel
/// [`crate::ops::value_name`] through the face: what php's error text
/// names a value as, which is `true` or `false` for a bool, the class
/// for an object, and the type for the rest. It is what the tail of an
/// argument type error carries, the last `%s` of php's
/// `zend_argument_type_error(n, "must be of type %s, %s given", ...)`,
/// so a native that words one by hand words it with this and not with
/// [`type_name`], which says `bool` where php says `true`.
#[must_use]
pub fn value_name(req: &Request, value: &Value) -> String {
    crate::ops::value_name(req, value)
}

/// `zend_zval_type_name` (`Z/Zend/zend_API.c:174`), the kernel
/// [`crate::ops::type_name`] through the face: the class for an object,
/// `null` for a slot nothing wrote, and `zend_get_type_by_const`'s word
/// for the rest, `bool` for either bool and `resource` for a closed
/// resource too, a reference followed to what it holds. Where
/// [`value_name`] renders `true`, this renders `bool`; where
/// [`Value::type_name`] renders `object`, this renders the class. It is
/// what `ctype_fallback`'s deprecation names its argument as.
///
/// ```php
/// class classA {} $f = fopen('php://memory', 'r'); fclose($f); var_dump(ctype_alpha(new classA), ctype_digit(new stdClass), ctype_alpha(true), ctype_alpha($f));
/// ```
/// ```expect
/// Deprecated: ctype_alpha(): Argument of type classA will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_digit(): Argument of type stdClass will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_alpha(): Argument of type bool will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_alpha(): Argument of type resource will be interpreted as string in the future in %s on line 2
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[must_use]
pub fn type_name(req: &Request, value: &Value) -> String {
    crate::ops::type_name(req, value)
}

/// `===`: `zend_is_identical`, the one the kernels read
/// ([`crate::ops::identical`]), for a native. Two arrays are walked
/// under php's protection, so one that holds itself is php's `Error`.
pub fn identical(req: &mut Request, a: &Value, b: &Value) -> Result<bool, Thrown> {
    crate::ops::identical(req, a, b)
}

/// `==`: [`compare`] finding the two sides equal.
pub fn loose_eq(req: &mut Request, a: &Value, b: &Value) -> Result<bool, Thrown> {
    compare(req, a, b).map(|o| o == Ordering::Equal)
}

/// `<=>`: `zend_compare`, the one ordering the kernels read
/// ([`crate::ops::compare`]), for a native.
pub fn compare(req: &mut Request, a: &Value, b: &Value) -> Result<Ordering, Thrown> {
    crate::ops::compare(req, a, b)
}

/// One binary operator as its opcode runs it: the kernel `ZEND_ADD`,
/// `ZEND_POW` or `ZEND_CONCAT` calls (`add_function`, `pow_function`,
/// `concat_function` of `Zend/zend_operators.c`), with its conversions,
/// its warnings and its throws, so a native that applies an operator
/// applies php's. [`BinaryOp::BitwiseNot`] reads `lhs` alone.
#[cfg_attr(debug_assertions, track_caller)]
pub fn binary_op(
    req: &mut Request,
    op: BinaryOp,
    lhs: &Value,
    rhs: &Value,
) -> Result<Owner<Value>, Thrown> {
    let kernel = match op {
        BinaryOp::Add => crate::ops::add,
        BinaryOp::Sub => crate::ops::sub,
        BinaryOp::Mul => crate::ops::mul,
        BinaryOp::Div => crate::ops::div,
        BinaryOp::Mod => crate::ops::r#mod,
        BinaryOp::Pow => crate::ops::pow,
        BinaryOp::Concat => crate::ops::concat,
        BinaryOp::BitwiseAnd => crate::ops::bitwise_and,
        BinaryOp::BitwiseOr => crate::ops::bitwise_or,
        BinaryOp::BitwiseXor => crate::ops::bitwise_xor,
        BinaryOp::ShiftLeft => crate::ops::shift_left,
        BinaryOp::ShiftRight => crate::ops::shift_right,
        BinaryOp::BitwiseNot => return adopted(crate::ops::bitwise_not(req, lhs)),
    };
    adopted(kernel(req, lhs, rhs))
}

/// `ZEND_ARG_USES_STRICT_TYPES()` (`Z/Zend/zend_compile.h:723`): whether
/// the call being served was made under `declare(strict_types=1)`, the
/// regime [`coerce_param`] holds an argument under, for a native that
/// parses an argument by hand as php's `gmp_zend_parse_arg_into_mpz`
/// does, with `zend_parse_arg_long_weak` only under the weak one.
#[must_use]
pub fn strict_types(req: &Request) -> bool {
    req.strict_types()
}

/// Hold the argument at `position` to `ty` as `zend_parse_parameters`
/// and its `Z_PARAM_*` macros hold it (`Z/Zend/zend_API.h`,
/// `Z/Zend/zend_API.c:533-820`), under the caller's `strict_types`
/// ([`Request::strict_types`]), with php's text for what it refuses.
/// One rule serves every declared shape: the value the type already
/// holds stands, [`holds`]; `null` to a scalar that is not nullable is
/// php 8.1's deprecation and then the scalar's zero under the weak rule,
/// and the `TypeError` under the strict one; what is left converts by
/// [`scalar_type_hint`], the one weak rule php's `RECV` walks too, which
/// under `strict_types` converts nothing but an int a float takes; and
/// the rest is `zend_argument_type_error`'s `must be of type %s, %s
/// given`, the type as `zend_type_to_string` writes it
/// ([`php_unit::type_to_string`]) and the value as `zend_zval_value_name`
/// names it. A `callable` is `zend_is_callable_ex`'s resolution, and
/// its refusal `zend_wrong_callback_error`'s sentence
/// ([`callable_argument`]).
///
/// An int parameter given a float or a string reads it through the
/// kernel that copies `zend_parse_arg_long_weak`,
/// [`crate::ops::arg_long_weak`]: a float in the long's range is
/// truncated behind the 8.1 deprecation where that loses something, and
/// one that is NAN or past the range is the `TypeError`; a numeric
/// string with a fraction is the same deprecation in its float-string
/// wording.
///
/// ```php
/// var_dump(str_repeat('x', 1.5), str_repeat('x', '1.5'));
/// str_repeat('x', 1e19);
/// ```
/// ```expect exit=255
/// Deprecated: Implicit conversion from float 1.5 to int loses precision in %s on line 2
///
/// Deprecated: Implicit conversion from float-string "1.5" to int loses precision in %s on line 2
/// string(1) "x"
/// string(1) "x"
///
/// Fatal error: Uncaught TypeError: str_repeat(): Argument #2 ($times) must be of type int, float given in %s:3
/// Stack trace:
/// #0 %s(3): str_repeat('x', 1.0E+19)
/// #1 {main}
///   thrown in %s on line 3
/// ```
///
/// What comes back is the frame's, as what `Z_PARAM_STR` hands a native
/// is: a string the coercion made replaces the argument in the call
/// frame, as `zend_parse_arg_str_weak` writes the string it made back
/// into the argument's zval, so the frame owns it and frees it with the
/// call, and a trace taken after the coercion prints the string and not
/// the number it was passed as, exactly as php's does. The native holds
/// a view and releases nothing.
///
/// ```php
/// str_repeat(12, -1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ValueError: str_repeat(): Argument #2 ($times) must be greater than or equal to 0 in %s:2
/// Stack trace:
/// #0 %s(2): str_repeat('12', -1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// A `string` takes an object through its `__toString` under the weak
/// rule, `zend_parse_arg_str_weak`'s object arm, and the string it made
/// replaces the object in the frame; an object with no `__toString` is
/// the `TypeError` naming its class, and what `__toString` throws is
/// what the call throws, with no `TypeError` after it.
///
/// ```php
/// class S { function __toString(): string { return "s"; } }
/// class T { function __toString(): string { throw new Exception("from __toString"); } }
/// var_dump(strtoupper(new S), str_repeat(new S, 2), strlen(new S));
/// try { strtoupper(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { strtoupper(new T); } catch (Exception $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "S"
/// string(2) "ss"
/// int(1)
/// strtoupper(): Argument #1 ($string) must be of type string, stdClass given
/// Exception: from __toString
/// ```
///
/// Under `strict_types` the caller's file refuses what the weak rule
/// converts, the object, the number and the numeric string alike, and
/// takes an int where a float is asked for, the one conversion php
/// makes there.
///
/// ```php
/// declare(strict_types=1);
/// class S { function __toString(): string { return "s"; } }
/// foreach ([fn () => strtoupper(new S), fn () => str_repeat(12, 1), fn () => str_repeat('x', '2'), fn () => fmod('7', 2), fn () => in_array(1, [], 1)] as $f) {
///     try { $f(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// }
/// var_dump(fmod(7, 2));
/// ```
/// ```expect
/// strtoupper(): Argument #1 ($string) must be of type string, S given
/// str_repeat(): Argument #1 ($string) must be of type string, int given
/// str_repeat(): Argument #2 ($times) must be of type int, string given
/// fmod(): Argument #1 ($num1) must be of type float, string given
/// float(1)
/// ```
///
/// The weak rule's scalar arms, each php's `zend_parse_arg_*_weak`: a
/// numeric string, leading and trailing whitespace allowed, to a float
/// and to an int; a bool to either; a number or a string to a bool.
///
/// ```php
/// var_dump(fmod(' 7 ', '2e0'), fmod(true, 2), str_repeat('ab', true), in_array(1, [1], '0'), in_array('1', [1], 1));
/// try { fmod('7x', 2); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// float(1)
/// float(1)
/// string(2) "ab"
/// bool(true)
/// bool(false)
/// fmod(): Argument #1 ($num1) must be of type float, string given
/// ```
///
/// A union holds a value one of its members holds, and otherwise
/// converts it by the members' order, int, then float, then string,
/// then bool, a string read by `is_numeric_str_function` where both
/// numbers are members; `null` to one with a scalar member is the
/// deprecation naming the whole union and the first member's zero. The
/// refusal names the union as `zend_type_to_string` orders it, classes
/// first, which is the `Z_EXPECTED_*` row `Z_PARAM_NUMBER`,
/// `Z_PARAM_ARRAY_HT_OR_STR` and their kin print.
///
/// ```php
/// var_dump(round('1.5'), round('2'), round(null), str_replace('a', 'b', 11));
/// try { round([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { str_replace('a', 'b', new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: round(): Passing null to parameter #1 ($num) of type int|float is deprecated in %s on line 2
/// float(2)
/// float(2)
/// float(0)
/// string(2) "11"
/// round(): Argument #1 ($num) must be of type int|float, array given
/// str_replace(): Argument #3 ($subject) must be of type array|string, stdClass given
/// ```
///
/// A class is `instanceof_function` over the object's class, its
/// parents and its interfaces, `Z_PARAM_OBJECT_OF_CLASS`, the refusal
/// naming the class and `?Class` where `null` is a member,
/// `zend_wrong_parameter_class_or_null_error`'s form.
///
/// ```php
/// var_dump(iterator_count(new ArrayIterator([1, 2])));
/// try { new CachingIterator(new ArrayObject([])); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { iterator_count(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { Closure::bind(fn () => 1, new stdClass, []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// CachingIterator::__construct(): Argument #1 ($iterator) must be of type Iterator, ArrayObject given
/// iterator_count(): Argument #1 ($iterator) must be of type Traversable|array, int given
/// Closure::bind(): Argument #3 ($newScope) must be of type object|string|null, array given
/// ```
pub fn coerce_param(
    req: &mut Request,
    value: &Value,
    ty: &Type,
    position: u32,
) -> Result<Value, Thrown> {
    hold(req, value, ty, position).map(|held| held.value)
}

/// [`coerce_param`] over `given`, the argument the call passed at
/// `position` as the entry read it off the call, and the type the
/// native being served declares for it, `arg_info->type`, the one
/// declaration Reflection reads too, so no native writes its parameter's
/// type a second time: the value as passed where the parameter declares
/// no type. A variadic
/// position is held to the variadic's type.
///
/// ```php
/// $m = new MultipleIterator(MultipleIterator::MIT_KEYS_ASSOC);
/// $m->attachIterator(new ArrayIterator([1]), 1.0);
/// $m->attachIterator(new ArrayIterator([2]), false);
/// foreach ($m as $v) { var_dump($v); }
/// try { $m->attachIterator(new ArrayIterator([3]), []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [1]=>
///   int(1)
///   [0]=>
///   int(2)
/// }
/// MultipleIterator::attachIterator(): Argument #2 ($info) must be of type string|int|null, array given
/// ```
///
/// A parameter whose type names one class holds its argument to the
/// class the engine resolved once as it started
/// (`Request::active_parameter_class`), `zend_parse_arg_object`'s
/// `instanceof_function` over a class entry, and an argument of another
/// class goes on to the declared type, which raises the `TypeError`:
///
/// ```php
/// class S extends SplObjectStorage {}
/// $a = new SplObjectStorage; $s = new S; $s[new stdClass] = 1;
/// $a->addAll($s); var_dump(count($a));
/// try { $a->addAll(new ArrayObject); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { Closure::bind(new stdClass, null); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// SplObjectStorage::addAll(): Argument #1 ($storage) must be of type SplObjectStorage, ArrayObject given
/// Closure::bind(): Argument #1 ($closure) must be of type Closure, stdClass given
/// ```
pub(crate) fn argument(req: &mut Request, given: Value, position: u32) -> Result<Value, Thrown> {
    // `Z_PARAM_OBJECT_OF_CLASS` over a class resolved once: an instance
    // is held by identity, and anything else goes on to the declared
    // type, which raises what it raises.
    if let Some(class) = req.active_parameter_class(position) {
        let value = given.deref();
        if value
            .as_object()
            .is_some_and(|object| req.symbols().instance_of(object.class(), class))
        {
            return Ok(value);
        }
    }
    match req.active_parameter_type(position) {
        Some(ty) => coerce_param(req, &given, &ty, position),
        None => Ok(given.deref()),
    }
}

/// `Z_PARAM_FUNC` and `Z_PARAM_FUNC_OR_NULL` over `given`, the argument
/// as it stands on the call at `position` (`UNDEF` where the call gave
/// none), held to the `callable` or `?callable` the native
/// declares there as [`argument`] holds any parameter, and to
/// `callable` where the declaration names no callable, which is the
/// macro's own form: the callee `zend_is_callable_ex` resolved it to,
/// its deprecations raised once, or `zend_wrong_callback_error`'s `must
/// be a valid callback, %s` ending with the resolver's own sentence, `or
/// null` where `null` is a member. `None` for the `null` a `?callable`
/// took and for no argument at all. Every form the resolver serves is
/// taken, a `Closure` first among them.
///
/// ```php
/// class C { function m($x) { return "m$x"; } static function s($x) { return "s$x"; } }
/// function f($x) { return "f$x"; }
/// var_dump(call_user_func(fn ($x) => "c$x", 1), call_user_func('f', 2), call_user_func([new C, 'm'], 3), call_user_func('C::s', 4), array_map(null, [5]));
/// try { call_user_func('nope'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { array_map([new C, 'nope'], []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $a = []; usort($a, null); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(2) "c1"
/// string(2) "f2"
/// string(2) "m3"
/// string(2) "s4"
/// array(1) {
///   [0]=>
///   int(5)
/// }
/// call_user_func(): Argument #1 ($callback) must be a valid callback, function "nope" not found or invalid function name
/// array_map(): Argument #1 ($callback) must be a valid callback or null, class C does not have a method "nope"
/// usort(): Argument #2 ($callback) must be a valid callback, no array or string given
/// ```
pub(crate) fn callable_argument(
    req: &mut Request,
    given: Value,
    position: u32,
) -> Result<Option<Callable>, Thrown> {
    if matches!(given.as_ref(), ValueRef::Undef) {
        return Ok(None);
    }
    let ty = req
        .active_parameter_type(position)
        .filter(|ty| callable_type(ty).is_some())
        .unwrap_or(Type::Callable);
    hold(req, &given, &ty, position).map(|held| held.callable)
}

/// What [`hold`] settled one argument to: the value the native reads,
/// and the callee a `callable` parameter resolved it to.
struct Held {
    value: Value,
    callable: Option<Callable>,
}

impl Held {
    fn plain(value: Value) -> Held {
        Held {
            value,
            callable: None,
        }
    }
}

/// The body of [`coerce_param`] and [`callable_argument`].
fn hold(req: &mut Request, value: &Value, ty: &Type, position: u32) -> Result<Held, Thrown> {
    let value = value.deref().undef_as_null();
    let null = matches!(value.as_ref(), ValueRef::Null);
    if let Some(nullable) = callable_type(ty) {
        if null && nullable {
            return Ok(Held::plain(Value::null()));
        }
        return match req.callable_checked(&value) {
            Ok(callable) => Ok(Held {
                value,
                callable: Some(callable),
            }),
            // A deprecation the resolution raised became a throw, which
            // stands as it is, as `zend_wrong_callback_error` raises
            // nothing over a pending exception.
            Err(_) if req.is_pending() => Err(Thrown::of(req.take_pending_view())),
            Err(why) => {
                let or_null = if nullable { " or null" } else { "" };
                let tail = format!("be a valid callback{or_null}, {why}");
                Err(why.across(req, |req| arg_type_error(req, position, &tail)))
            }
        };
    }
    let scopes = req.active_native_scopes();
    if holds(req, &value, ty, scopes) {
        return Ok(Held::plain(value));
    }
    let strict = req.strict_types();
    let mask = ty.mask();
    let coerced = if null {
        if strict {
            None
        } else {
            null_as_scalar(req, mask)?
        }
    } else {
        scalar_type_hint(req, &value, mask, strict)?
    };
    let Some(coerced) = coerced else {
        let given = value_name(req, &value);
        let declared = php_unit::type_to_string(ty);
        return Err(arg_type_error(
            req,
            position,
            &format!("be of type {declared}, {given} given"),
        ));
    };
    if null {
        // `zend_null_arg_deprecated` names the type the parameter
        // declares where it declares one, and the macro's where not.
        let declared = req.active_parameter_type(position).map_or_else(
            || php_unit::type_to_string(ty),
            |ty| php_unit::type_to_string(&ty),
        );
        crate::api::raising::arg_null_deprecated(req, position, &declared);
        if req.is_pending() {
            req.release(coerced);
            return Err(Thrown::of(req.take_pending_view()));
        }
    }
    // `zend_parse_arg_str_weak` converts the argument's own zval, and
    // so do `Z_PARAM_NUMBER`'s two slow paths; the long, double and bool
    // readers write their destination and leave the zval as passed.
    if coerced.as_php_string().is_some()
        || (mask & (MAY_BE_LONG | MAY_BE_DOUBLE)) == (MAY_BE_LONG | MAY_BE_DOUBLE)
    {
        req.replace_argument(position, coerced);
    }
    Ok(Held::plain(coerced))
}

/// Whether `ty` is `callable`, and whether `null` is a member beside
/// it: the two shapes `Z_PARAM_FUNC` and `Z_PARAM_FUNC_OR_NULL` read.
fn callable_type(ty: &Type) -> Option<bool> {
    match ty {
        Type::Callable => Some(false),
        Type::Nullable(inner) => callable_type(inner).map(|_| true),
        Type::Union(members) => match members.as_slice() {
            [Type::Callable, Type::Null] | [Type::Null, Type::Callable] => Some(true),
            _ => None,
        },
        _ => None,
    }
}

/// The classes a `self`, a `parent` and a `static` in a declared type
/// name where it is checked: the class the function is declared on,
/// php's `zf->common.scope`, and the class it was called on,
/// `Z_CE(EX(This))`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Scopes {
    pub(crate) own: Option<ClassId>,
    pub(crate) called: Option<ClassId>,
    /// Whether a `callable` is resolved with its deprecations, as
    /// `zend_check_type` resolves one for a user function's parameter
    /// or return, or without, `IS_CALLABLE_SUPPRESS_DEPRECATIONS`, for a
    /// native's.
    pub(crate) callables: Deprecations,
}

/// `zend_check_type` (`Z/Zend/zend_execute.c:1160`) and
/// `zend_check_type_slow`: whether the declared type holds the value as
/// it is, nothing converted and nothing raised. The mask is the whole
/// check for a type that names no class; a class holds an object whose class is it,
/// extends it or implements it (`instanceof_function`), an intersection
/// one every member holds, `iterable` an array or a `Traversable`,
/// `callable` what `zend_is_callable` resolves, and `self`, `parent`
/// and `static` the classes `scopes` names. The one check an argument
/// of a native, an argument of a user function, a return and a typed
/// property are held to.
///
/// A class a return and a property are held to is the same
/// `instanceof_function`:
///
/// ```php
/// class A {} class B extends A {}
/// function f(): A { return new B; }
/// class C { public A $a; }
/// $c = new C; $c->a = new B;
/// var_dump(f() instanceof B, $c->a instanceof B);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
///
/// php declares no native with an intersection, so the witness for that
/// arm is a user function's, which this same check serves:
///
/// ```php
/// class A { function f(self $x) { return "self "; } function g(): static { return $this; } }
/// function h(Countable&Traversable $x) { return "both "; }
/// function i(iterable $x) { return "iterable "; }
/// function k(?callable $c) { return "callable "; }
/// echo (new A)->f(new A), get_class((new A)->g()), " ", h(new ArrayIterator([])), i(new ArrayObject([])), i([]), k(fn () => 1), k('strlen'), k(null), "|\n";
/// foreach ([fn () => h(new EmptyIterator), fn () => i(1), fn () => k('nope')] as $f) {
///     try { $f(); } catch (TypeError $e) { echo preg_replace('/, called in.*/', '', $e->getMessage()), "\n"; }
/// }
/// ```
/// ```expect
/// self A both iterable iterable callable callable callable |
/// h(): Argument #1 ($x) must be of type Countable&Traversable, EmptyIterator given
/// i(): Argument #1 ($x) must be of type Traversable|array, int given
/// k(): Argument #1 ($c) must be of type ?callable, string given
/// ```
pub(crate) fn holds(req: &mut Request, value: &Value, ty: &Type, scopes: Scopes) -> bool {
    holding(req, value, ty, scopes).resolve(req, value, scopes)
}

/// What a declared type says of a value with the request only read:
/// [`holds`] up to its `callable`, the one test that can serve PHP code
/// or raise a deprecation (`zend_is_callable`, which may autoload a class
/// a string names) and so takes the request whole. A caller that holds
/// the type borrowed from the request reads this, lets the borrow go and
/// then resolves it ([`Holding::resolve`]), so it copies no type.
///
/// `callable` is tried after every other member of a union, where php
/// tries the classes, then `callable`, then `static`
/// (`zend_check_type_slow`): a value `static` holds is an object, which
/// `zend_is_callable` resolves with nothing raised or served, so the order
/// of the two is not observable.
pub(crate) fn holding(req: &Request, value: &Value, ty: &Type, scopes: Scopes) -> Holding {
    let value = value.deref();
    let object = value.as_object().map(|v| v.class());
    let is = |class: Option<ClassId>| {
        Holding::of(
            object
                .zip(class)
                .is_some_and(|(object, class)| req.symbols().instance_of(object, class)),
        )
    };
    match ty {
        Type::Mixed => Holding::Yes,
        Type::Nullable(_) if matches!(value.as_ref(), ValueRef::Null | ValueRef::Undef) => {
            Holding::Yes
        }
        Type::Nullable(inner) => holding(req, &value, inner, scopes),
        Type::Union(members) => {
            let mut verdict = Holding::No;
            for member in members {
                match holding(req, &value, member, scopes) {
                    Holding::Yes => return Holding::Yes,
                    Holding::IfCallable => verdict = Holding::IfCallable,
                    Holding::No => {}
                }
            }
            verdict
        }
        // Names alone, so never `callable`.
        Type::Intersection(members) => Holding::of(
            members
                .iter()
                .all(|member| holding(req, &value, member, scopes) == Holding::Yes),
        ),
        Type::Named(name) => is(req.class_named(name.into())),
        Type::Iterable if value.as_array().is_some() => Holding::Yes,
        Type::Iterable => is(req.class_named(Bare::resolved(b"Traversable"))),
        Type::Callable => Holding::IfCallable,
        Type::SelfClass => is(scopes.own),
        Type::CalledClass => is(scopes.called),
        Type::ParentClass => is(scopes
            .own
            .and_then(|class| req.symbols().class_decl(class)?.parent.as_ref())
            .and_then(|parent| req.class_named(parent.into()))),
        _ => Holding::of(ty.contains_code(value.type_code())),
    }
}

/// [`holding`]'s answer: the type holds the value, it does not, or it does
/// where the value is callable, which [`Holding::resolve`] asks of the
/// request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Holding {
    Yes,
    No,
    IfCallable,
}

impl Holding {
    fn of(holds: bool) -> Holding {
        if holds { Holding::Yes } else { Holding::No }
    }

    /// The answer whole: `zend_is_callable` over the value where only
    /// a `callable` could hold it, with the deprecations `scopes` asks
    /// for.
    pub(crate) fn resolve(self, req: &mut Request, value: &Value, scopes: Scopes) -> bool {
        match self {
            Holding::Yes => true,
            Holding::No => false,
            Holding::IfCallable => req.is_callable_value(&value.deref(), scopes.callables),
        }
    }
}

/// `MAY_BE_FALSE`, `MAY_BE_TRUE`, `MAY_BE_LONG`, `MAY_BE_DOUBLE` and
/// `MAY_BE_STRING` (`Z/Zend/zend_type_info.h:26-32`): the bits of a
/// declared type's mask this coercion reads.
const MAY_BE_BOOL: u32 = (1 << 2) | (1 << 3);
const MAY_BE_LONG: u32 = 1 << 4;
const MAY_BE_DOUBLE: u32 = 1 << 5;
const MAY_BE_STRING: u32 = 1 << 6;

/// `is_numeric_str_function`: the number a string is, whitespace
/// allowed on either side and nothing else past it.
fn numeric_whole(bytes: &[u8]) -> Option<php_heap::Number> {
    match php_heap::numeric_prefix(bytes)? {
        (number, false) => Some(number),
        (_, true) => None,
    }
}

/// `zend_verify_scalar_type_hint` (`Z/Zend/zend_execute.c:817`) over a
/// declared type's mask, the one weak rule php's `RECV` and a native's
/// parameter parsing both walk: under `strict_types` nothing converts
/// but a long where the type accepts a float, and under the weak rule
/// the preference order `zend_verify_weak_scalar_type_hint` walks over
/// the `zend_parse_arg_*_weak` readers, int then float then string then
/// bool, with a string read by `is_numeric_str_function` where the type
/// accepts both int and float, and an object taken as the string its
/// class's `cast_object` gives, `zend_parse_arg_str_weak`'s object arm,
/// whose throw is the `Err`. `null` never converts here, since php
/// allows that for an internal parameter alone ([`null_as_scalar`]); what does not
/// convert is `None`, and the caller raises the `TypeError`.
///
/// ```php
/// class S { function __toString(): string { return "s"; } }
/// function f(string $s) { return $s; }
/// function g(int|string $v) { return $v; }
/// class P { public string $p = ''; }
/// $p = new P; $p->p = new S;
/// var_dump(f(new S), g(new S), g(1.0), $p->p);
/// ```
/// ```expect
/// string(1) "s"
/// string(1) "s"
/// int(1)
/// string(1) "s"
/// ```
pub(crate) fn scalar_type_hint(
    req: &mut Request,
    value: &Value,
    mask: u32,
    strict: bool,
) -> Result<Option<Value>, Thrown> {
    let value = value.deref();
    if strict {
        // The one exception php makes under `strict_types`: an int
        // where a float is accepted, converted.
        return Ok(match (mask & MAY_BE_DOUBLE != 0, value.as_ref()) {
            (true, ValueRef::Int(n)) => Some(Value::float(n as f64)),
            _ => None,
        });
    }
    if matches!(value.as_ref(), ValueRef::Null | ValueRef::Undef) {
        return Ok(None);
    }
    if mask & MAY_BE_LONG != 0 {
        if mask & MAY_BE_DOUBLE != 0
            && let ValueRef::Str(s) = value.as_ref()
        {
            match numeric_whole(s.as_bytes()) {
                Some(php_heap::Number::Long(n)) => return Ok(Some(Value::int(n))),
                Some(php_heap::Number::Double(d)) => return Ok(Some(Value::float(d))),
                None => {}
            }
        } else if let ValueRef::Bool(b) = value.as_ref() {
            return Ok(Some(Value::int(i64::from(b))));
        } else if let Some(n) = crate::ops::arg_long_weak(req, &value) {
            return Ok(Some(Value::int(n)));
        }
        if req.is_pending() {
            return Ok(None);
        }
    }
    if mask & MAY_BE_DOUBLE != 0 {
        match value.as_ref() {
            ValueRef::Int(n) => return Ok(Some(Value::float(n as f64))),
            ValueRef::Bool(b) => return Ok(Some(Value::float(f64::from(u8::from(b))))),
            ValueRef::Str(s) => {
                if let Some(number) = numeric_whole(s.as_bytes()) {
                    return Ok(Some(Value::float(number.double())));
                }
            }
            _ => {}
        }
    }
    if mask & MAY_BE_STRING != 0 {
        match value.as_ref() {
            // `crate::ops::to_string`, the kernel that copies
            // `zend_double_to_str` at the request's `precision`; this
            // module's `float_format` is the same `zend_gcvt` at a
            // precision the caller names (#408).
            ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Bool(_) => {
                return Ok(Some(Value::string(crate::ops::to_string(req, &value)?)));
            }
            // `cast_object(zobj, &obj, IS_STRING)`, the class's own
            // slot, which at `Std` fails without a word for a class with
            // no `__toString`.
            ValueRef::Object(object) => {
                let table = req.object_handlers(object.heap());
                if let Some(text) = table.cast_object(req, object, TypeTag::String)? {
                    return Ok(Some(text.hand_on()));
                }
            }
            _ => {}
        }
    }
    if mask & MAY_BE_BOOL == MAY_BE_BOOL
        && matches!(
            value.as_ref(),
            ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Str(_)
        )
    {
        return Ok(Some(Value::bool(truthy(req, &value)?)));
    }
    Ok(None)
}

/// `zend_parse_arg_*_weak` on `null` for a parameter with a scalar
/// member that is not nullable: the value `null` coerces to under the
/// 8.1 rule, the zero of the first member in the weak order, `0`,
/// `0.0`, the empty string or `false`, and none for a type with no
/// scalar member, which is the `TypeError`. The caller raises
/// `zend_null_arg_deprecated` beside it.
///
/// ```php
/// new ReflectionExtension(null);
/// ```
/// ```expect exit=255
/// Deprecated: ReflectionExtension::__construct(): Passing null to parameter #1 ($name) of type string is deprecated in %s on line 2
///
/// Fatal error: Uncaught ReflectionException: Extension "" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionExtension->__construct('')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// Under `strict_types` the same `null` is the `TypeError`, php's
/// `zend_parse_arg_*_slow` refusing before the weak reader is reached,
/// for a string, an int, a union and a bool alike.
///
/// ```php
/// declare(strict_types=1);
/// foreach ([fn () => define(null, 1), fn () => str_repeat('x', null), fn () => round(null), fn () => in_array(1, [], null), fn () => memory_get_usage(null)] as $f) {
///     try { $f(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// define(): Argument #1 ($constant_name) must be of type string, null given
/// str_repeat(): Argument #2 ($times) must be of type int, null given
/// round(): Argument #1 ($num) must be of type int|float, null given
/// memory_get_usage(): Argument #1 ($real_usage) must be of type bool, null given
/// ```
fn null_as_scalar(req: &mut Request, mask: u32) -> Result<Option<Value>, Thrown> {
    Ok(if mask & MAY_BE_LONG != 0 {
        Some(Value::int(0))
    } else if mask & MAY_BE_DOUBLE != 0 {
        Some(Value::float(0.0))
    } else if mask & MAY_BE_STRING != 0 {
        Some(Value::string(req.string(b"")?))
    } else if mask & MAY_BE_BOOL != 0 {
        Some(Value::bool(false))
    } else {
        None
    })
}

/// `zend_type_to_string_resolved` with a scope, what an argument's
/// and a return's `TypeError` name the type as
/// (`zend_verify_type_error_common`): `self` and `parent` the scope's
/// class and its parent, each up to a NUL as `resolve_class_name` cuts
/// an anonymous class's, and `static` the called scope. Without a
/// scope, an unbound closure's, the type is as written. Either way the
/// type is written as `zend_type_to_string` writes it
/// ([`php_unit::type_to_string`]), the class names first and the other
/// types in php's own order whatever order they were declared in:
///
/// ```php
/// function f(int|string|array|null $x) {}
/// function g(float|int $x) {}
/// foreach ([fn () => f(new stdClass), fn () => g('x')] as $c) {
///     try { $c(); } catch (TypeError $e) { echo preg_replace('/, called in.*/', '', $e->getMessage()), "\n"; }
/// }
/// ```
/// ```expect
/// f(): Argument #1 ($x) must be of type array|string|int|null, stdClass given
/// g(): Argument #1 ($x) must be of type int|float, string given
/// ```
///
/// A trait's `self` and `parent` are the using class's, which the
/// compiler could not write in:
///
/// ```php
/// trait T { function g(self|int $a): parent|array { return $a; } }
/// class P {}
/// class A extends P { use T; function f(): static { return new A; } }
/// class B extends A {}
/// try { (new B)->f(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { (new B)->g(""); } catch (TypeError $e) { echo preg_replace('/, called in.*/', '', $e->getMessage()), "\n"; }
/// try { (new B)->g(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $c = new class { function h(self|string $a) {} };
/// try { $c->h(null); } catch (TypeError $e) { echo preg_replace('/, called in.*/', '', $e->getMessage()), "\n"; }
/// ```
/// ```expect
/// A::f(): Return value must be of type B, A returned
/// A::g(): Argument #1 ($a) must be of type A|int, string given
/// A::g(): Return value must be of type P|array, int returned
/// class@anonymous(): Argument #1 ($a) must be of type class@anonymous|string, null given
/// ```
pub(crate) fn resolved_type_name(req: &Request, scopes: Scopes, ty: &Type) -> String {
    let Some(own) = scopes.own else {
        return php_unit::type_to_string(ty);
    };
    let own_name = req.symbols().class_name(own);
    let parent_name = req
        .symbols()
        .class_parent(own)
        .map(|parent| req.symbols().class_name(parent));
    let resolved = resolve_named(ty, &own_name, parent_name.as_deref());
    let called = scopes.called.map_or_else(
        || String::from("static"),
        |called| String::from_utf8_lossy(&req.class_name_bytes(called)).into_owned(),
    );
    php_unit::type_to_string_called(&resolved, &called)
}

/// `resolve_class_name` over each member: `self` the scope's name,
/// `parent` its parent's where it has one.
fn resolve_named(ty: &Type, own: &str, parent: Option<&str>) -> Type {
    let named = |name: &str| Type::Named(php_unit::decl::Name(name.as_bytes().into()));
    match ty {
        Type::SelfClass => named(own),
        Type::ParentClass => parent.map_or(Type::ParentClass, named),
        Type::Nullable(inner) => Type::Nullable(Box::new(resolve_named(inner, own, parent))),
        Type::Union(members) => Type::Union(
            members
                .iter()
                .map(|member| resolve_named(member, own, parent))
                .collect(),
        ),
        Type::Intersection(members) => Type::Intersection(
            members
                .iter()
                .map(|member| resolve_named(member, own, parent))
                .collect(),
        ),
        other => other.clone(),
    }
}
