//! The `Z_PARAM_*` readers `php_date.c`'s natives parse their arguments
//! with (`Z/Zend/zend_API.h`), each raising what php raises for a value
//! it does not take, and the declarations the natives are made of.

use php_ext::coerce::coerce_param;
use php_ext::decl::{ConstExpr, FunctionDecl, MethodDecl, Name, Parameter, Type, Visibility};
use php_ext::raising::{Thrown, arg_count_error};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

/// `ZEND_PARSE_PARAMETERS_START(min, max)`'s count check.
pub(super) fn count_between(req: &mut Request, min: usize, max: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if (min..=max).contains(&given) {
        return Ok(());
    }
    let (word, bound) = if min == max {
        ("exactly", min)
    } else if given < min {
        ("at least", min)
    } else {
        ("at most", max)
    };
    let plural = if bound == 1 { "" } else { "s" };
    Err(arg_count_error(
        req,
        &format!("{word} {bound} argument{plural}, {given} given"),
    ))
}

/// The argument at `position` when the call gave one.
pub(super) fn given(req: &Request, position: u32) -> Option<Value> {
    req.argument(position)
        .filter(|value| !matches!(value.as_ref(), ValueRef::Undef))
}

/// `Z_PARAM_STR`.
pub(super) fn string(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let value = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &value, &Type::String, position)?;
    match coerced.as_php_string() {
        Some(s) => Ok(*s),
        None => PhpString::new(req, b""),
    }
}

/// `Z_PARAM_STR` after `Z_PARAM_OPTIONAL`, `None` where the call gave none.
pub(super) fn optional_string(
    req: &mut Request,
    position: u32,
) -> Result<Option<PhpString>, Thrown> {
    match given(req, position) {
        Some(_) => string(req, position).map(Some),
        None => Ok(None),
    }
}

/// `Z_PARAM_STR_OR_NULL`.
pub(super) fn nullable_string(
    req: &mut Request,
    position: u32,
) -> Result<Option<PhpString>, Thrown> {
    match given(req, position) {
        Some(value) if !matches!(value.deref().as_ref(), ValueRef::Null) => {
            let ty = Type::Nullable(Box::new(Type::String));
            let coerced = coerce_param(req, &value, &ty, position)?;
            Ok(coerced.as_php_string().copied())
        }
        _ => Ok(None),
    }
}

/// `Z_PARAM_LONG`.
pub(super) fn long(req: &mut Request, position: u32) -> Result<i64, Thrown> {
    let value = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &value, &Type::Int, position)?;
    Ok(coerced.as_int().unwrap_or(0))
}

/// `Z_PARAM_LONG` after `Z_PARAM_OPTIONAL`.
pub(super) fn optional_long(req: &mut Request, position: u32, default: i64) -> Result<i64, Thrown> {
    match given(req, position) {
        Some(_) => long(req, position),
        None => Ok(default),
    }
}

/// `Z_PARAM_LONG_OR_NULL`.
pub(super) fn nullable_long(req: &mut Request, position: u32) -> Result<Option<i64>, Thrown> {
    match given(req, position) {
        Some(value) if !matches!(value.deref().as_ref(), ValueRef::Null) => {
            let ty = Type::Nullable(Box::new(Type::Int));
            let coerced = coerce_param(req, &value, &ty, position)?;
            Ok(coerced.as_int())
        }
        _ => Ok(None),
    }
}

/// `Z_PARAM_DOUBLE`.
pub(super) fn double(req: &mut Request, position: u32) -> Result<f64, Thrown> {
    let value = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &value, &Type::Float, position)?;
    Ok(coerced.as_float().unwrap_or(0.0))
}

/// `Z_PARAM_DOUBLE_OR_NULL`.
pub(super) fn nullable_double(req: &mut Request, position: u32) -> Result<Option<f64>, Thrown> {
    match given(req, position) {
        Some(value) if !matches!(value.deref().as_ref(), ValueRef::Null) => {
            let ty = Type::Nullable(Box::new(Type::Float));
            let coerced = coerce_param(req, &value, &ty, position)?;
            Ok(coerced.as_float())
        }
        _ => Ok(None),
    }
}

/// `Z_PARAM_BOOL` after `Z_PARAM_OPTIONAL`.
pub(super) fn optional_bool(
    req: &mut Request,
    position: u32,
    default: bool,
) -> Result<bool, Thrown> {
    let Some(value) = given(req, position) else {
        return Ok(default);
    };
    let coerced = coerce_param(req, &value, &Type::Bool, position)?;
    Ok(coerced.as_bool().unwrap_or(false))
}

/// `Z_PARAM_ARRAY_HT`.
pub(super) fn array(req: &mut Request, position: u32) -> Result<PhpArray, Thrown> {
    let value = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &value, &Type::Array, position)?;
    match coerced.deref().as_array() {
        Some(array) => Ok(*array),
        None => PhpArray::new(req),
    }
}

/// `Z_PARAM_OBJECT_OF_CLASS(ce)`: the object, or the `TypeError` naming
/// the class, both `coerce_param`'s.
pub(super) fn object_of(
    req: &mut Request,
    position: u32,
    class: &str,
) -> Result<PhpObject, Thrown> {
    let value = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &value, &self::class(class), position)?;
    match coerced.as_object() {
        Some(object) => Ok(*object),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: coerce_param held a value that is no object to a class",
        )),
    }
}

/// `Z_PARAM_OBJECT_OF_CLASS_OR_NULL(ce)`: `None` for `null` or no
/// argument, and the refusal naming `?Class`.
pub(super) fn nullable_object_of(
    req: &mut Request,
    position: u32,
    class: &str,
) -> Result<Option<PhpObject>, Thrown> {
    let Some(value) = given(req, position) else {
        return Ok(None);
    };
    let coerced = coerce_param(req, &value, &nullable(self::class(class)), position)?;
    Ok(coerced.as_object().copied())
}

/// A new string of `bytes` as the return.
pub(super) fn string_value(req: &mut Request, bytes: &[u8]) -> Result<Value, Thrown> {
    Ok(Value::string(PhpString::new(req, bytes)?))
}

/// `add_assoc_*`: one row under a string key.
pub(super) fn set_str(
    req: &mut Request,
    array: &mut PhpArray,
    key: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    let key = Key::Str(PhpString::new(req, key)?);
    array.set(req, key, value)
}

/// `add_assoc_string`.
pub(super) fn set_string(
    req: &mut Request,
    array: &mut PhpArray,
    key: &[u8],
    value: &[u8],
) -> Result<(), Thrown> {
    let value = string_value(req, value)?;
    set_str(req, array, key, value)
}

/// One parameter by value with no default.
pub(super) fn p(name: &str, ty: Type) -> Parameter {
    Parameter {
        name: Name(name.as_bytes().into()),
        ty: Some(ty),
        by_ref: false,
        prefer_ref: false,
        variadic: false,
        default: None,
        attributes: Vec::new(),
    }
}

/// One parameter with its written default.
pub(super) fn d(name: &str, ty: Type, default: ConstExpr) -> Parameter {
    Parameter {
        default: Some(default),
        ..p(name, ty)
    }
}

/// An untyped parameter, `$start` of `DatePeriod::__construct`.
pub(super) fn untyped(name: &str) -> Parameter {
    Parameter {
        ty: None,
        ..p(name, Type::Mixed)
    }
}

/// `?T`.
pub(super) fn nullable(ty: Type) -> Type {
    Type::Nullable(Box::new(ty))
}

/// A class type.
pub(super) fn class(name: &str) -> Type {
    Type::Named(Name(name.as_bytes().into()))
}

/// `A|B`.
pub(super) fn union(types: Vec<Type>) -> Type {
    Type::Union(types)
}

/// A string default.
pub(super) fn str_default(s: &str) -> ConstExpr {
    ConstExpr::Str(s.as_bytes().into())
}

/// One function as `php_date.stub.php` declares it.
pub(super) fn function(
    name: &str,
    parameters: Vec<Parameter>,
    returns: Option<Type>,
) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.as_bytes().into()),
        parameters,
        return_type: returns,
        returns_by_ref: false,
        is_generator: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
        frameless: Vec::new(),
    }
}

/// One public method; `tentative` is the stub's `@tentative-return-type`.
pub(super) fn method(
    name: &str,
    parameters: Vec<Parameter>,
    returns: Option<Type>,
    is_instance: bool,
    tentative: bool,
) -> MethodDecl {
    MethodDecl {
        function: function(name, parameters, returns),
        visibility: Visibility::Public,
        is_instance,
        is_abstract: false,
        is_final: false,
        tentative_return: tentative,
    }
}
