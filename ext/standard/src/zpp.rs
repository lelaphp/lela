//! The `Z_PARAM_*` readers the natives of `string.c` and its neighbours
//! share beyond the three the crate root has (`Z/Zend/zend_API.h`): an
//! optional slot read only when the call gave it, the `_OR_NULL` forms,
//! and the two unions `Z_PARAM_ARRAY_HT_OR_STR` and
//! `Z_PARAM_ARRAY_HT_OR_LONG`, each raising what php raises for a value
//! it does not take.

use php_ext::coerce::{coerce_param, value_name};
use php_ext::decl::Type;
use php_ext::raising::{Thrown, arg_type_error};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, Value, ValueRef};

use crate::{bool_argument, long_argument, string_argument};

/// The argument at `position` when the call gave one: `None` past the
/// count and for a slot a named call skipped, which is where
/// `Z_PARAM_OPTIONAL` leaves the C default standing.
pub(super) fn given(req: &Request, position: u32) -> Option<Value> {
    req.argument(position)
        .filter(|value| !matches!(value.as_ref(), ValueRef::Undef))
}

/// `Z_PARAM_LONG` after `Z_PARAM_OPTIONAL`.
pub(super) fn optional_long(req: &mut Request, position: u32, default: i64) -> Result<i64, Thrown> {
    match given(req, position) {
        Some(_) => long_argument(req, position),
        None => Ok(default),
    }
}

/// `Z_PARAM_BOOL` after `Z_PARAM_OPTIONAL`.
pub(super) fn optional_bool(
    req: &mut Request,
    position: u32,
    default: bool,
) -> Result<bool, Thrown> {
    match given(req, position) {
        Some(_) => bool_argument(req, position),
        None => Ok(default),
    }
}

/// `Z_PARAM_STR` after `Z_PARAM_OPTIONAL`.
pub(super) fn optional_string(
    req: &mut Request,
    position: u32,
) -> Result<Option<PhpString>, Thrown> {
    match given(req, position) {
        Some(_) => string_argument(req, position).map(Some),
        None => Ok(None),
    }
}

/// `Z_PARAM_LONG_OR_NULL`: `None` for `null` or no argument.
pub(super) fn nullable_long(req: &mut Request, position: u32) -> Result<Option<i64>, Thrown> {
    match given(req, position) {
        Some(value) if !matches!(value.deref().as_ref(), ValueRef::Null) => {
            let coerced =
                coerce_param(req, &value, &Type::Nullable(Box::new(Type::Int)), position)?;
            Ok(coerced.as_int())
        }
        _ => Ok(None),
    }
}

/// `Z_PARAM_STR_OR_NULL`: `None` for `null` or no argument.
pub(super) fn nullable_string(
    req: &mut Request,
    position: u32,
) -> Result<Option<PhpString>, Thrown> {
    match given(req, position) {
        Some(value) if !matches!(value.deref().as_ref(), ValueRef::Null) => {
            let coerced = coerce_param(
                req,
                &value,
                &Type::Nullable(Box::new(Type::String)),
                position,
            )?;
            Ok(coerced.as_php_string().copied())
        }
        _ => Ok(None),
    }
}

/// `Z_PARAM_ARRAY_HT`: the array, or the `TypeError` naming `array`.
pub(super) fn array_argument(req: &mut Request, position: u32) -> Result<PhpArray, Thrown> {
    let value = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &value, &Type::Array, position)?;
    match coerced.deref().as_array() {
        Some(array) => Ok(*array),
        None => PhpArray::new(req),
    }
}

/// `Z_PARAM_ARRAY_HT_OR_NULL`: `None` for `null` or no argument.
pub(super) fn nullable_array(req: &mut Request, position: u32) -> Result<Option<PhpArray>, Thrown> {
    let Some(value) = given(req, position) else {
        return Ok(None);
    };
    match value.deref().as_ref() {
        ValueRef::Null => Ok(None),
        ValueRef::Array(array) => Ok(Some(*array)),
        _ => {
            let name = value_name(req, &value.deref());
            Err(arg_type_error(
                req,
                position,
                &format!("be of type ?array, {name} given"),
            ))
        }
    }
}

/// One side of a `Z_PARAM_ARRAY_HT_OR_*` union.
pub(super) enum ArrayOr<T> {
    Array(PhpArray),
    Other(T),
}

/// `Z_PARAM_ARRAY_HT_OR_STR`: the array, or the argument read as
/// `Z_PARAM_STR` reads it, with `array|string` the type the null
/// deprecation and the `TypeError` name. `nullable` is the `_OR_NULL`
/// form, where `null` is `None` and neither is raised.
pub(super) fn array_or_string(
    req: &mut Request,
    position: u32,
    nullable: bool,
) -> Result<Option<ArrayOr<PhpString>>, Thrown> {
    let value = req.argument(position).unwrap_or_default().deref();
    match value.as_ref() {
        ValueRef::Array(array) => Ok(Some(ArrayOr::Array(*array))),
        ValueRef::Str(s) => Ok(Some(ArrayOr::Other(*s))),
        ValueRef::Null | ValueRef::Undef if nullable => Ok(None),
        ValueRef::Null
        | ValueRef::Undef
        | ValueRef::Int(_)
        | ValueRef::Float(_)
        | ValueRef::Bool(_) => {
            // The null deprecation names the declared type, `array|string`.
            let coerced = coerce_param(req, &value, &Type::String, position)?;
            match coerced.as_php_string() {
                Some(s) => Ok(Some(ArrayOr::Other(*s))),
                None => Ok(Some(ArrayOr::Other(PhpString::new(req, b"")?))),
            }
        }
        _ => {
            let name = value_name(req, &value);
            let question = if nullable {
                "array|string|null"
            } else {
                "array|string"
            };
            Err(arg_type_error(
                req,
                position,
                &format!("be of type {question}, {name} given"),
            ))
        }
    }
}

/// `Z_PARAM_ARRAY_HT_OR_LONG`, and with `nullable` its `_OR_NULL`
/// form: the array, or the argument read as `Z_PARAM_LONG` reads it.
pub(super) fn array_or_long(
    req: &mut Request,
    position: u32,
    nullable: bool,
) -> Result<Option<ArrayOr<i64>>, Thrown> {
    let value = req.argument(position).unwrap_or_default().deref();
    match value.as_ref() {
        ValueRef::Array(array) => Ok(Some(ArrayOr::Array(*array))),
        ValueRef::Int(n) => Ok(Some(ArrayOr::Other(n))),
        ValueRef::Null | ValueRef::Undef if nullable => Ok(None),
        ValueRef::Null
        | ValueRef::Undef
        | ValueRef::Float(_)
        | ValueRef::Bool(_)
        | ValueRef::Str(_) => {
            let coerced = coerce_param(req, &value, &Type::Int, position)?;
            Ok(Some(ArrayOr::Other(coerced.as_int().unwrap_or(0))))
        }
        _ => {
            let name = value_name(req, &value);
            let union = if nullable {
                "array|int|null"
            } else {
                "array|int"
            };
            Err(arg_type_error(
                req,
                position,
                &format!("be of type {union}, {name} given"),
            ))
        }
    }
}

/// A new string of `bytes` as the return.
pub(super) fn string_value(req: &mut Request, bytes: &[u8]) -> Result<Value, Thrown> {
    Ok(Value::string(PhpString::new(req, bytes)?))
}

/// `RETURN_STR_COPY(str)`: the argument's own string handed back, one
/// more reference to it.
pub(super) fn same(req: &mut Request, string: PhpString) -> Value {
    let value = Value::string(string);
    req.addref(&value);
    value
}

/// A count as php's `zend_long`.
pub(super) fn long_of(n: usize) -> Value {
    Value::int(i64::try_from(n).unwrap_or(i64::MAX))
}

/// `ZEND_TRY_ASSIGN_REF_*` on the argument at `position`: `value`
/// written through the reference the argument is, and given up where
/// it is not one.
pub(super) fn assign_ref(req: &mut Request, position: u32, value: Value) {
    let given = req.argument(position).unwrap_or_default();
    match given.as_ref() {
        ValueRef::Reference(reference) => reference.set(req, value),
        _ => req.release(value),
    }
}
