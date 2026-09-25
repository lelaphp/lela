//! The `Z_PARAM_*` readers the natives and methods of this crate share
//! (`Z/Zend/zend_API.h`): the count checked as
//! `zend_wrong_parameters_count_error` words it, and each argument
//! coerced to the type its declaration names, which is what
//! `zend_parse_arg_*` does under the caller's `strict_types`.

use php_ext::coerce::argument;
use php_ext::raising::{Thrown, arg_count_error, fatal};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpObject, Value};

/// `ZEND_PARSE_PARAMETERS_START(min, max)`: the count, or php's
/// `ArgumentCountError`.
pub(super) fn arity(req: &mut Request, min: usize, max: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if (min..=max).contains(&given) {
        return Ok(());
    }
    let (how, n) = if min == max {
        ("exactly", min)
    } else if given < min {
        ("at least", min)
    } else {
        ("at most", max)
    };
    let word = if n == 1 { "argument" } else { "arguments" };
    Err(arg_count_error(
        req,
        &format!("{how} {n} {word}, {given} given"),
    ))
}

/// The argument at `position` coerced to its declared type, `None`
/// where the call gave none.
pub(super) fn given(req: &mut Request, position: u32) -> Result<Option<Value>, Thrown> {
    argument(req, position)
}

/// `Z_PARAM_LONG`.
pub(super) fn long(req: &mut Request, position: u32) -> Result<i64, Thrown> {
    Ok(given(req, position)?.and_then(|v| v.as_int()).unwrap_or(0))
}

/// `Z_PARAM_LONG_OR_NULL`: `None` for `null` and for no argument.
pub(super) fn nullable_long(req: &mut Request, position: u32) -> Result<Option<i64>, Thrown> {
    Ok(given(req, position)?.and_then(|v| v.as_int()))
}

/// `Z_PARAM_ARRAY`: the array, a view.
pub(super) fn array(req: &mut Request, position: u32) -> Result<PhpArray, Thrown> {
    match given(req, position)?
        .map(|v| v.deref())
        .and_then(|v| v.as_array().copied())
    {
        Some(a) => Ok(a),
        None => PhpArray::new(req),
    }
}

/// `ZEND_THIS`: the receiver of the method being served.
pub(super) fn this(req: &mut Request) -> Result<PhpObject, Thrown> {
    match req.this() {
        Some(this) => Ok(this),
        None => Err(fatal(
            req,
            "InternalError: a random method served without its receiver",
        )),
    }
}
