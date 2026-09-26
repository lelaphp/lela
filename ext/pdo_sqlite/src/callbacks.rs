//! The PHP side of the callbacks `sqlite_driver.c` registers: a user
//! function (`do_callback` for a scalar), an aggregate's step and final
//! (`do_callback` with `is_agg`), a collation
//! (`php_sqlite3_collation_callback`) and the authorizer (`authorizer`),
//! each served through the request the library's call left reachable
//! ([`crate::ffi`]) and answering as php answers, a throwable handed
//! back for the call to carry out.

use php_ext::abi::Callable;
use php_ext::call::call;
use php_ext::coerce::{to_string, value_name};
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{PhpRef, PhpString, Value, ValueRef};

use crate::ffi::{Argument, Invocation, SQLITE_ATTACH, SQLITE_DENY, SQLITE_IGNORE, SQLITE_OK};

/// A callable kept past the call it was given to, `zend_fcc_dup`: the
/// resolved callee and the value it came from, one owner of the value.
#[derive(Clone)]
pub(crate) struct Handler {
    pub(crate) callable: Callable,
    pub(crate) value: Value,
}

impl Handler {
    /// The callable and one more owner of `value`.
    pub(crate) fn keep(req: &Request, callable: Callable, value: Value) -> Handler {
        let value = value.deref();
        req.addref(&value);
        Handler { callable, value }
    }

    /// `zend_fcc_dtor`.
    pub(crate) fn release(self, req: &mut Request) {
        req.release(self.value);
    }
}

/// What a registered function serves.
pub(crate) enum Function {
    Scalar(Handler),
    Aggregate { step: Handler, finalize: Handler },
}

/// `aggregate_context`: the reference the step and the final are
/// handed, and the rows counted so far.
pub(crate) struct Aggregate {
    context: PhpRef,
    row: i64,
}

/// An argument as the callback is handed it.
fn argument(req: &mut Request, argument: Argument) -> Result<Value, Thrown> {
    Ok(match argument {
        Argument::Int(n) => Value::int(i64::from(n)),
        Argument::Float(f) => Value::float(f),
        Argument::Null => Value::null(),
        Argument::Text(bytes) => Value::string(PhpString::new(req, &bytes)?),
    })
}

fn arguments(req: &mut Request, given: Vec<Argument>) -> Result<Vec<Value>, Thrown> {
    let mut out = Vec::with_capacity(given.len());
    for one in given {
        out.push(argument(req, one)?);
    }
    Ok(out)
}

fn release_all(req: &mut Request, values: Vec<Value>) {
    for value in values {
        req.release(value);
    }
}

/// The callback's return as the function's answer: an integer to the
/// library's 32 bits, `null`, a float, and a string of anything else.
fn result(req: &mut Request, returned: Value, call: &mut Invocation<'_>) -> Result<(), Thrown> {
    let answered = match returned.deref().as_ref() {
        // php hands `sqlite3_result_int` its `zend_long`.
        #[allow(clippy::cast_possible_truncation)]
        ValueRef::Int(n) => {
            call.result_int(n as i32);
            Ok(())
        }
        ValueRef::Null => {
            call.result_null();
            Ok(())
        }
        ValueRef::Float(f) => {
            call.result_double(f);
            Ok(())
        }
        _ => to_string(req, &returned.deref()).map(|text| call.result_text(text.as_bytes())),
    };
    req.release(returned);
    answered
}

/// `do_callback` for a scalar function.
pub(super) fn scalar(
    req: &mut Request,
    handler: &Handler,
    invocation: &mut Invocation<'_>,
) -> Result<(), Thrown> {
    let values = arguments(req, invocation.arguments())?;
    let returned = call(req, &handler.callable, &values);
    release_all(req, values);
    match returned {
        Ok(returned) => result(req, returned, invocation),
        Err(thrown) => {
            invocation.failed();
            Err(thrown)
        }
    }
}

/// The aggregate's state, made on first use with the reference to
/// `null`.
fn state_of(
    req: &mut Request,
    state: &mut Option<Box<Aggregate>>,
) -> Result<(PhpRef, i64), Thrown> {
    if state.is_none() {
        let context = PhpRef::new(req, Value::null())?;
        *state = Some(Box::new(Aggregate { context, row: 0 }));
    }
    let Some(held) = state.as_mut() else {
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: an aggregate without its state",
        ));
    };
    held.row += 1;
    Ok((held.context, held.row))
}

/// `do_callback` for an aggregate's step: the aggregate's reference, the row number
/// and the arguments handed over, and what it returns kept in the
/// reference.
pub(super) fn step(
    req: &mut Request,
    handler: &Handler,
    state: &mut Option<Box<Aggregate>>,
    given: Vec<Argument>,
) -> Result<(), Thrown> {
    let (context, row) = state_of(req, state)?;
    let mut values = vec![context.as_value(), Value::int(row)];
    let extra = arguments(req, given)?;
    values.extend(extra.iter().copied());
    let returned = call(req, &handler.callable, &values);
    release_all(req, extra);
    let returned = returned?;
    context.set(req, returned)
}

/// `do_callback` for an aggregate's final: the aggregate's reference and
/// the row number, which counts this call too, and its return as the
/// answer; the reference given up.
pub(super) fn finalize(
    req: &mut Request,
    handler: &Handler,
    mut state: Option<Box<Aggregate>>,
    invocation: &mut Invocation<'_>,
) -> Result<(), Thrown> {
    let (context, row) = state_of(req, &mut state)?;
    let returned = call(
        req,
        &handler.callable,
        &[context.as_value(), Value::int(row)],
    );
    req.release(context.as_value());
    match returned {
        Ok(returned) => result(req, returned, invocation),
        Err(thrown) => {
            invocation.failed();
            Err(thrown)
        }
    }
}

/// An aggregate's state given up without its final served.
pub(super) fn drop_aggregate(req: &mut Request, state: Option<Box<Aggregate>>) {
    if let Some(state) = state {
        req.release(state.context.as_value());
    }
}

/// `php_sqlite3_collation_callback`: the two strings compared by the
/// callback, whose answer must be an integer.
pub(super) fn collate(
    req: &mut Request,
    handler: &Handler,
    left: &[u8],
    right: &[u8],
) -> Result<i32, Thrown> {
    let values = [
        Value::string(PhpString::new(req, left)?),
        Value::string(PhpString::new(req, right)?),
    ];
    let returned = call(req, &handler.callable, &values);
    release_all(req, values.to_vec());
    let returned = returned?;
    if let Some(n) = returned.deref().as_int() {
        return Ok(match n.signum() {
            -1 => -1,
            0 => 0,
            _ => 1,
        });
    }
    let name = req.active_function_name();
    let given = value_name(req, &returned);
    req.release(returned);
    Err(throw(
        req,
        "TypeError",
        &format!(
            "{name}(): Return value of the collation callback must be of type int, {given} returned"
        ),
    ))
}

/// `authorizer`'s user half: the action and its four names handed to
/// the callback, whose answer must be one of the three the library
/// takes.
pub(super) fn authorize(
    req: &mut Request,
    handler: Option<&Handler>,
    action: i32,
    names: [Option<Vec<u8>>; 4],
    pending: bool,
) -> Result<i32, Thrown> {
    if action == SQLITE_ATTACH
        && crate::sqlite::open_basedir(req)
        && crate::sqlite::safe_filename(req, names[0].as_deref()).is_none()
    {
        return Ok(SQLITE_DENY);
    }
    let Some(handler) = handler else {
        return Ok(SQLITE_OK);
    };
    if pending {
        return Ok(SQLITE_DENY);
    }
    let mut values = vec![Value::int(i64::from(action))];
    for name in names {
        values.push(match name {
            Some(name) => Value::string(PhpString::new(req, &name)?),
            None => Value::null(),
        });
    }
    let returned = call(req, &handler.callable, &values);
    release_all(req, values);
    let returned = returned?;
    let name = req.active_function_name();
    let Some(answer) = returned.deref().as_int() else {
        let given = value_name(req, &returned);
        req.release(returned);
        return Err(throw(
            req,
            "TypeError",
            &format!(
                "{name}(): Return value of the authorizer callback must be of type int, {given} returned"
            ),
        ));
    };
    match i32::try_from(answer) {
        Ok(answer @ (SQLITE_OK | SQLITE_IGNORE | SQLITE_DENY)) => Ok(answer),
        _ => Err(throw(
            req,
            "ValueError",
            &format!(
                "{name}(): Return value of the authorizer callback must be one of Pdo\\Sqlite::OK, Pdo\\Sqlite::DENY, or Pdo\\Sqlite::IGNORE"
            ),
        )),
    }
}
