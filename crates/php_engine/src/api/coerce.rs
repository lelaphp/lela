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

use php_heap::ZvalType;
use php_unit::decl::Type;

use crate::api::abi::Callable;
use crate::api::raising::{Thrown, arg_type_error, throw};
use crate::api::request::Request;
use crate::api::values::{Key, PhpArray, PhpObject, PhpString, TypeTag, Value, ValueRef};

/// php's binary operators as `zend_binary_op` names them.
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
}

/// `zend_is_true`, the kernel [`crate::ops::is_true`] through the face:
/// a NAN warns as it is coerced, which is why the request is here.
pub fn truthy(req: &mut Request, value: &Value) -> bool {
    crate::ops::is_true(req, value)
}

/// `is_numeric_string_ex`: the `int` or `float` a string denotes, with
/// php's whitespace rules, or `None`. The skeleton reads what Rust
/// parses after trimming php's whitespace set.
#[must_use]
pub fn numeric_string(bytes: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(bytes).ok()?;
    let trimmed = text.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'));
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(n) = trimmed.parse::<i64>() {
        return Some(Value::int(n));
    }
    let f: f64 = trimmed.parse().ok()?;
    if trimmed
        .chars()
        .any(|c| c.is_ascii_alphabetic() && !matches!(c, 'e' | 'E'))
    {
        return None;
    }
    Some(Value::float(f))
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
pub fn int_to_string(req: &mut Request, n: i64) -> Result<PhpString, Thrown> {
    PhpString::new(req, n.to_string().as_bytes())
}

/// `smart_str_append_double(buf, num, precision, false)`: the text php
/// appends to a `smart_str`, `zend_gcvt` at `precision` with `-1`
/// meaning the shortest text that reads back as the same double, and
/// not yet a string of the request's. The `zero_fraction` half of php's
/// call is the caller's, since only `var_export` asks for it.
#[must_use]
pub fn float_format(f: f64, precision: i32) -> String {
    php_heap::double_to_string(f, precision)
}

/// A `float` as php prints it under the request's `precision` ini, as a
/// string of the request's.
pub fn float_to_string(req: &mut Request, f: f64) -> Result<PhpString, Thrown> {
    let text = float_format(f, req.precision());
    PhpString::new(req, text.as_bytes())
}

/// `zval_get_long` with php's warnings and errors.
pub fn to_long(req: &mut Request, value: &Value) -> Result<i64, Thrown> {
    match value.as_ref() {
        ValueRef::Undef | ValueRef::Null => Ok(0),
        ValueRef::Bool(b) => Ok(i64::from(b)),
        ValueRef::Int(n) => Ok(n),
        ValueRef::Float(f) => Ok(float_to_long(f)),
        ValueRef::Str(s) => Ok(numeric_string(s.as_bytes())
            .and_then(|n| n.as_int().or_else(|| n.as_float().map(float_to_long)))
            .unwrap_or(0)),
        ValueRef::Array(a) => Ok(i64::from(!a.is_empty())),
        ValueRef::Resource(r) => Ok(r.id()),
        ValueRef::Object(_) => Err(throw(
            req,
            "TypeError",
            &format!("Cannot convert {} to int", value.type_name()),
        )),
        ValueRef::Reference(r) => to_long(req, &r.get()),
    }
}

/// `zval_get_double`.
pub fn to_double(req: &mut Request, value: &Value) -> Result<f64, Thrown> {
    match value.as_ref() {
        ValueRef::Float(f) => Ok(f),
        ValueRef::Str(s) => Ok(numeric_string(s.as_bytes())
            .and_then(|n| n.as_float().or_else(|| n.as_int().map(|i| i as f64)))
            .unwrap_or(0.0)),
        ValueRef::Reference(r) => to_double(req, &r.get()),
        _ => to_long(req, value).map(|n| n as f64),
    }
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
pub fn to_string(req: &mut Request, value: &Value) -> Result<PhpString, Thrown> {
    crate::ops::to_string(req, value)
}

/// `zval_get_number`: an `int` or a `float`.
pub fn to_number(req: &mut Request, value: &Value) -> Result<Value, Thrown> {
    match value.as_ref() {
        ValueRef::Float(f) => Ok(Value::float(f)),
        ValueRef::Str(s) => Ok(numeric_string(s.as_bytes()).unwrap_or(Value::int(0))),
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
pub fn to_array(req: &mut Request, value: &Value) -> Result<PhpArray, Thrown> {
    crate::ops::to_array(req, value)
}

/// `(object)`, the kernel [`crate::ops::to_object`] through the face:
/// `null` an empty `stdClass`, a scalar a `stdClass` with the property
/// `scalar`, an array a `stdClass` over its elements, and an object
/// itself. What is handed back is the caller's to own: an object
/// value's own with one more owner, a new one of the request's
/// otherwise.
pub fn to_object(req: &mut Request, value: &Value) -> Result<PhpObject, Thrown> {
    crate::ops::to_object(req, value)
}

/// `zend_is_callable` with the request's scope.
#[must_use]
pub fn is_callable(req: &Request, value: &Value) -> bool {
    req.callable_of(value).is_some()
}

/// The callable a value names, or php's `TypeError` for one that is not.
pub fn to_callable(req: &mut Request, value: &Value) -> Result<Callable, Thrown> {
    match req.callable_of(value) {
        Some(callable) => Ok(callable),
        None => Err(throw(req, "TypeError", "Argument must be a valid callback")),
    }
}

/// The array key a value makes, php's offset rules: `null` is `""`, a
/// `bool` and a `float` are an `int`, an `array` or an `object` is the
/// `TypeError` "Illegal offset type".
pub fn key_of(req: &mut Request, value: &Value) -> Result<Key, Thrown> {
    match value.as_ref() {
        ValueRef::Undef | ValueRef::Null => Ok(Key::Str(PhpString::new(req, b"")?)),
        ValueRef::Bool(b) => Ok(Key::Int(i64::from(b))),
        ValueRef::Int(n) => Ok(Key::Int(n)),
        ValueRef::Float(f) => Ok(Key::Int(float_to_long(f))),
        ValueRef::Str(s) => Ok(Key::from_string(*s)),
        ValueRef::Resource(r) => Ok(Key::Int(r.id())),
        ValueRef::Reference(r) => key_of(req, &r.get()),
        ValueRef::Array(_) | ValueRef::Object(_) => {
            Err(throw(req, "TypeError", "Illegal offset type"))
        }
    }
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
/// [`Value::type_name`], which is `zend_zval_type_name` and says `bool`
/// where php says `true`.
#[must_use]
pub fn value_name(req: &Request, value: &Value) -> String {
    crate::ops::value_name(req, value)
}

/// `===`.
#[must_use]
pub fn identical(a: &Value, b: &Value) -> bool {
    a.deref() == b.deref()
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

/// One binary operator over the types the skeleton knows: `int` and
/// `float` arithmetic, and string concatenation.
pub fn binary_op(
    req: &mut Request,
    op: BinaryOp,
    lhs: &Value,
    rhs: &Value,
) -> Result<Value, Thrown> {
    if op == BinaryOp::Concat {
        return crate::ops::concat(req, lhs, rhs);
    }
    if let (Some(left), Some(right)) = (lhs.as_int(), rhs.as_int()) {
        let folded = match op {
            BinaryOp::Add => left.checked_add(right),
            BinaryOp::Sub => left.checked_sub(right),
            BinaryOp::Mul => left.checked_mul(right),
            BinaryOp::Mod if right == 0 => {
                return Err(throw(req, "DivisionByZeroError", "Modulo by zero"));
            }
            BinaryOp::Mod => left.checked_rem(right),
            BinaryOp::BitwiseAnd => Some(left & right),
            BinaryOp::BitwiseOr => Some(left | right),
            BinaryOp::BitwiseXor => Some(left ^ right),
            BinaryOp::ShiftLeft => u32::try_from(right).ok().and_then(|s| left.checked_shl(s)),
            BinaryOp::ShiftRight => u32::try_from(right).ok().and_then(|s| left.checked_shr(s)),
            BinaryOp::Div | BinaryOp::Pow | BinaryOp::Concat => None,
        };
        if let Some(n) = folded {
            return Ok(Value::int(n));
        }
    }
    let left = to_double(req, lhs)?;
    let right = to_double(req, rhs)?;
    let folded = match op {
        BinaryOp::Add => left + right,
        BinaryOp::Sub => left - right,
        BinaryOp::Mul => left * right,
        BinaryOp::Div if right == 0.0 => {
            return Err(throw(req, "DivisionByZeroError", "Division by zero"));
        }
        BinaryOp::Div => left / right,
        BinaryOp::Pow => left.powf(right),
        BinaryOp::Mod
        | BinaryOp::BitwiseAnd
        | BinaryOp::BitwiseOr
        | BinaryOp::BitwiseXor
        | BinaryOp::ShiftLeft
        | BinaryOp::ShiftRight => {
            return Ok(Value::int(0));
        }
        BinaryOp::Concat => 0.0,
    };
    Ok(Value::float(folded))
}

/// Coerce the argument at `position` to `ty` as php's parameter parsing
/// does, honouring the caller's `strict_types`: the scalar cases here,
/// and a `TypeError` in php's wording for what does not coerce. An int
/// parameter given a float or a string reads it through the kernel that
/// copies `zend_parse_arg_long_weak`, [`crate::ops::arg_long_weak`]: a
/// float in the long's range is truncated behind the 8.1 deprecation
/// where that loses something, and one that is NAN or past the range is
/// the `TypeError`; a numeric string with a fraction is the same
/// deprecation in its float-string wording.
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
pub fn coerce_param(
    req: &mut Request,
    value: &Value,
    ty: &Type,
    position: u32,
) -> Result<Value, Thrown> {
    coerce_typed(req, value, ty, position, false)
}

/// `MAY_BE_FALSE`, `MAY_BE_TRUE`, `MAY_BE_LONG`, `MAY_BE_DOUBLE` and
/// `MAY_BE_STRING` (`Z/Zend/zend_type_info.h:26-32`): the bits of a
/// declared type's mask this coercion reads.
const MAY_BE_BOOL: u32 = (1 << 2) | (1 << 3);
const MAY_BE_LONG: u32 = 1 << 4;
const MAY_BE_DOUBLE: u32 = 1 << 5;
const MAY_BE_STRING: u32 = 1 << 6;

/// `zend_verify_scalar_type_hint` (`Z/Zend/zend_execute.c:817`) over a
/// declared type's mask, for an argument of a *user* function, which is
/// what a `RECV` checks: under `strict_types` nothing converts but a
/// long where the type accepts a float, and under the weak rule the
/// preference order `zend_verify_weak_scalar_type_hint` walks, int then
/// float then string then bool, with a string read by
/// `is_numeric_str_function` where the type accepts both int and float.
/// `null` never converts here, since php allows that for an internal
/// parameter alone; what does not convert is `None`, and the caller
/// raises the `TypeError`.
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
            match numeric_string(s.as_bytes()).as_ref().map(Value::as_ref) {
                Some(ValueRef::Int(n)) => return Ok(Some(Value::int(n))),
                Some(ValueRef::Float(d)) => return Ok(Some(Value::float(d))),
                _ => {}
            }
        } else if let ValueRef::Bool(b) = value.as_ref() {
            return Ok(Some(Value::int(i64::from(b))));
        } else if let Some(n) = crate::ops::arg_long_weak(req, &value) {
            return Ok(Some(Value::int(n)));
        }
    }
    if mask & MAY_BE_DOUBLE != 0 {
        match value.as_ref() {
            ValueRef::Int(n) => return Ok(Some(Value::float(n as f64))),
            ValueRef::Bool(b) => return Ok(Some(Value::float(f64::from(u8::from(b))))),
            ValueRef::Str(s) => {
                if let Some(number) = numeric_string(s.as_bytes()) {
                    return Ok(Some(Value::float(to_double(req, &number)?)));
                }
            }
            _ => {}
        }
    }
    if mask & MAY_BE_STRING != 0
        && matches!(
            value.as_ref(),
            ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Bool(_)
        )
    {
        // `crate::ops::to_string`, the kernel that copies
        // `zend_double_to_str`, and not this module's `float_format`,
        // which prints a float at a fixed number of places (#408).
        return Ok(Some(Value::string(crate::ops::to_string(req, &value)?)));
    }
    if mask & MAY_BE_BOOL == MAY_BE_BOOL
        && matches!(
            value.as_ref(),
            ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Str(_)
        )
    {
        return Ok(Some(Value::bool(truthy(req, &value))));
    }
    Ok(None)
}

/// The declared type as `zend_type_to_string` writes it, through the
/// face: what a `TypeError` for a parameter names the type as.
pub(crate) fn type_name(ty: &Type) -> String {
    declared_type_name(ty)
}

/// `zend_parse_arg_*_weak` on `null` for a scalar parameter that is
/// not nullable: the value `null` coerces to under the 8.1 rule,
/// `false`, `0`, `0.0` or the empty string, and none for a type php
/// does not coerce `null` into. The caller raises
/// `zend_null_arg_deprecated` beside it and writes the value back as
/// the argument, as the `Z_PARAM_*` macros do.
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
fn null_as_scalar(req: &mut Request, ty: &Type) -> Result<Option<Value>, Thrown> {
    Ok(match ty {
        Type::Bool => Some(Value::bool(false)),
        Type::Int => Some(Value::int(0)),
        Type::Float => Some(Value::float(0.0)),
        Type::String => Some(Value::string(PhpString::new(req, b"")?)),
        _ => None,
    })
}

/// The declared type as `zend_type_to_string` writes it into the null
/// deprecation, for the shapes a native parameter declares: a scalar,
/// a class, `?T`, and a union in its declared order.
fn declared_type_name(ty: &Type) -> String {
    match ty {
        Type::Mixed => String::from("mixed"),
        Type::Void => String::from("void"),
        Type::Never => String::from("never"),
        Type::Null => String::from("null"),
        Type::Bool => String::from("bool"),
        Type::False => String::from("false"),
        Type::True => String::from("true"),
        Type::Int => String::from("int"),
        Type::Float => String::from("float"),
        Type::String => String::from("string"),
        Type::Array => String::from("array"),
        Type::Object => String::from("object"),
        Type::Iterable => String::from("iterable"),
        Type::Callable => String::from("callable"),
        Type::CalledClass => String::from("static"),
        Type::SelfClass => String::from("self"),
        Type::ParentClass => String::from("parent"),
        Type::Named(name) => String::from_utf8_lossy(&name.0).into_owned(),
        Type::Nullable(inner) => format!("?{}", declared_type_name(inner)),
        Type::Union(members) | Type::Intersection(members) => {
            let glue = if matches!(ty, Type::Union(_)) {
                "|"
            } else {
                "&"
            };
            members
                .iter()
                .map(declared_type_name)
                .collect::<Vec<_>>()
                .join(glue)
        }
    }
}

/// [`coerce_param`] with the type named as the parameter declares it:
/// `nullable` is set inside a `?T`, so the `TypeError` for what does
/// not coerce says `?int` where php's `Z_EXPECTED_LONG_OR_NULL` does
/// (`zend_wrong_parameter_type_error` over the `_OR_NULL` row of
/// `Z_EXPECTED_TYPES`).
fn coerce_typed(
    req: &mut Request,
    value: &Value,
    ty: &Type,
    position: u32,
    nullable: bool,
) -> Result<Value, Thrown> {
    let strict = req.caller_strict_types();
    let value = &value.deref();
    if !strict
        && !nullable
        && matches!(value.as_ref(), ValueRef::Null | ValueRef::Undef)
        && let Some(coerced) = null_as_scalar(req, ty)?
    {
        let declared = req
            .active_parameter_type(position)
            .map_or_else(|| declared_type_name(ty), |ty| declared_type_name(&ty));
        crate::api::raising::arg_null_deprecated(req, position, &declared);
        req.replace_argument(position, coerced);
        return Ok(coerced);
    }
    let want = |req: &mut Request, name: &str| {
        let question = if nullable { "?" } else { "" };
        // `zend_argument_type_error(num, "must be of type %s, %s
        // given", need, zend_zval_value_name(arg))`: the tail is the
        // value's name and not its type's, so `true` stands where
        // `zend_zval_type_name` would read `bool`.
        let given = value_name(req, value);
        Err(arg_type_error(
            req,
            position,
            &format!("be of type {question}{name}, {given} given"),
        ))
    };
    match ty {
        Type::Null => match value.as_ref() {
            ValueRef::Null | ValueRef::Undef => Ok(Value::null()),
            _ => want(req, "null"),
        },
        Type::Bool => match value.as_ref() {
            ValueRef::Bool(b) => Ok(Value::bool(b)),
            ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Str(_) if !strict => {
                Ok(Value::bool(truthy(req, value)))
            }
            _ => want(req, "bool"),
        },
        Type::Int => match value.as_ref() {
            ValueRef::Int(n) => Ok(Value::int(n)),
            ValueRef::Bool(b) if !strict => Ok(Value::int(i64::from(b))),
            ValueRef::Float(_) | ValueRef::Str(_) if !strict => {
                match crate::ops::arg_long_weak(req, value) {
                    Some(n) => Ok(Value::int(n)),
                    None => want(req, "int"),
                }
            }
            _ => want(req, "int"),
        },
        Type::Float => match value.as_ref() {
            ValueRef::Float(f) => Ok(Value::float(f)),
            ValueRef::Int(n) => Ok(Value::float(n as f64)),
            ValueRef::Bool(b) if !strict => Ok(Value::float(f64::from(u8::from(b)))),
            ValueRef::Str(s) if !strict => match numeric_string(s.as_bytes()) {
                Some(n) => to_double(req, &n).map(Value::float),
                None => want(req, "float"),
            },
            _ => want(req, "float"),
        },
        Type::String => match value.as_ref() {
            ValueRef::Str(_) => Ok(*value),
            ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Bool(_) if !strict => {
                let coerced = Value::string(to_string(req, value)?);
                req.replace_argument(position, coerced);
                Ok(coerced)
            }
            _ => want(req, "string"),
        },
        Type::Array => match value.as_ref() {
            ValueRef::Array(_) => Ok(*value),
            _ => want(req, "array"),
        },
        Type::Object => match value.as_ref() {
            ValueRef::Object(_) => Ok(*value),
            _ => want(req, "object"),
        },
        Type::Nullable(inner) => match value.as_ref() {
            ValueRef::Null | ValueRef::Undef => Ok(Value::null()),
            _ => coerce_typed(req, value, inner, position, true),
        },
        _ => Ok(*value),
    }
}
