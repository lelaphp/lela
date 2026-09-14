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

/// `smart_str_append_double` with `precision`: php's `%.*G` shape with
/// `-1` meaning the shortest round-trip repr (`zend_gcvt`), as the text
/// php appends to a `smart_str` and not yet a string of the request's.
#[must_use]
pub fn float_format(f: f64, precision: i32) -> String {
    if f.is_nan() {
        "NAN".to_owned()
    } else if f.is_infinite() {
        if f > 0.0 { "INF" } else { "-INF" }.to_owned()
    } else if precision < 0 {
        let s = format!("{f}");
        s.strip_suffix(".0").map_or(s.clone(), str::to_owned)
    } else {
        format!("{f:.*}", usize::try_from(precision).unwrap_or(0))
    }
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

/// `zval_get_string`: an object goes through `__toString`, which the
/// skeleton cannot serve and reports as php's "could not be converted".
/// The string handed back is the caller's to own, as `zval_get_string`'s
/// is: a string value's own with one more owner (`zend_string_copy`),
/// and a new string of the request's for everything else.
pub fn to_string(req: &mut Request, value: &Value) -> Result<PhpString, Thrown> {
    match value.as_ref() {
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false) => PhpString::new(req, b""),
        ValueRef::Bool(true) => PhpString::new(req, b"1"),
        ValueRef::Int(n) => int_to_string(req, n),
        ValueRef::Float(f) => float_to_string(req, f),
        ValueRef::Str(s) => {
            req.addref(value);
            Ok(*s)
        }
        ValueRef::Array(_) => {
            crate::api::raising::warning(req, "Array to string conversion");
            PhpString::new(req, b"Array")
        }
        ValueRef::Resource(r) => PhpString::new(req, format!("Resource id #{}", r.id()).as_bytes()),
        ValueRef::Object(_) => Err(throw(
            req,
            "Error",
            "Object of class could not be converted to string",
        )),
        ValueRef::Reference(r) => to_string(req, &r.get()),
    }
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

/// `(array)`: the array itself, named and not copied, as `ZVAL_COPY_VALUE`
/// names it; an object's properties as a new array; a scalar as a new
/// array of one. What is handed back is the caller's to own: an array
/// that already was one has no owner taken, and a new one has its one.
pub fn to_array(req: &mut Request, value: &Value) -> Result<PhpArray, Thrown> {
    match value.as_ref() {
        ValueRef::Undef | ValueRef::Null => PhpArray::new(req),
        ValueRef::Array(a) => Ok(*a),
        ValueRef::Object(o) => {
            let mut array = PhpArray::new(req)?;
            for (key, entry) in o.entries(req)? {
                req.addref(&entry);
                if let Key::Str(s) = &key {
                    req.addref(&Value::string(*s));
                }
                array.set(req, key, entry)?;
            }
            Ok(array)
        }
        ValueRef::Reference(r) => to_array(req, &r.get()),
        _ => {
            let mut array = PhpArray::new(req)?;
            let element = value.deref().clone();
            req.addref(&element);
            array.append(req, element)?;
            Ok(array)
        }
    }
}

/// `(object)`.
pub fn to_object(req: &mut Request, value: &Value) -> Result<PhpObject, Thrown> {
    match value.as_ref() {
        ValueRef::Object(o) => Ok(*o),
        ValueRef::Reference(r) => to_object(req, &r.get()),
        _ => {
            let object = req.new_object_of("stdClass")?;
            if let Some(array) = value.as_array() {
                for (key, entry) in array.snapshot() {
                    let name = key.to_prop_name(req)?;
                    req.addref(&entry);
                    object.set_prop(req, name, entry)?;
                }
            } else if !matches!(value.as_ref(), ValueRef::Null | ValueRef::Undef) {
                let name = PhpString::new(req, b"scalar")?;
                let scalar = value.deref().clone();
                req.addref(&scalar);
                object.set_prop(req, name, scalar)?;
            }
            Ok(object)
        }
    }
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

/// `===`.
#[must_use]
pub fn identical(a: &Value, b: &Value) -> bool {
    a.deref() == b.deref()
}

/// `==`, php 8's comparison.
pub fn loose_eq(req: &mut Request, a: &Value, b: &Value) -> Result<bool, Thrown> {
    compare(req, a, b).map(|o| o == Ordering::Equal)
}

/// `<=>`, php 8's comparison over the types the skeleton knows.
pub fn compare(req: &mut Request, a: &Value, b: &Value) -> Result<Ordering, Thrown> {
    match (a.deref().as_ref(), b.deref().as_ref()) {
        (ValueRef::Int(x), ValueRef::Int(y)) => Ok(x.cmp(&y)),
        (ValueRef::Str(x), ValueRef::Str(y)) => {
            match (numeric_string(x.as_bytes()), numeric_string(y.as_bytes())) {
                (Some(x), Some(y)) => compare(req, &x, &y),
                _ => Ok(x.as_bytes().cmp(y.as_bytes())),
            }
        }
        (ValueRef::Null | ValueRef::Undef, ValueRef::Null | ValueRef::Undef) => Ok(Ordering::Equal),
        (ValueRef::Bool(_) | ValueRef::Null, _) | (_, ValueRef::Bool(_) | ValueRef::Null) => {
            Ok(truthy(req, a).cmp(&truthy(req, b)))
        }
        (ValueRef::Array(x), ValueRef::Array(y)) => Ok(x.len().cmp(&y.len())),
        _ => {
            let x = to_double(req, a)?;
            let y = to_double(req, b)?;
            Ok(x.partial_cmp(&y).unwrap_or(Ordering::Less))
        }
    }
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
    let strict = req.caller_strict_types();
    let value = &value.deref();
    let want = |req: &mut Request, name: &str| {
        Err(arg_type_error(
            req,
            position,
            &format!("be of type {name}, {} given", value.type_name()),
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
            ValueRef::Str(_) => Ok(value.clone()),
            ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Bool(_) if !strict => {
                let coerced = Value::string(to_string(req, value)?);
                req.replace_argument(position, coerced.clone());
                Ok(coerced)
            }
            _ => want(req, "string"),
        },
        Type::Array => match value.as_ref() {
            ValueRef::Array(_) => Ok(value.clone()),
            _ => want(req, "array"),
        },
        Type::Nullable(inner) => match value.as_ref() {
            ValueRef::Null | ValueRef::Undef => Ok(Value::null()),
            _ => coerce_param(req, value, inner, position),
        },
        _ => Ok(value.clone()),
    }
}
