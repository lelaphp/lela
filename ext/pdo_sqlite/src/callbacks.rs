//! The PHP side of the callbacks `sqlite_driver.c` registers: a user
//! function (`do_callback` for a scalar), an aggregate's step and final
//! (`do_callback` with `is_agg`), a collation
//! (`php_sqlite3_collation_callback`) and the authorizer (`authorizer`),
//! each served through the request the library's call left reachable
//! ([`crate::ffi`]) and answering as php answers, a throwable handed
//! back for the call to carry out.

use php_ext::abi::Callable;
use php_ext::call::call;
use php_ext::coerce::{value_name, with_string};
use php_ext::raising::{Thrown, type_error, value_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpRef, PhpString, Value, ValueRef};

use crate::ffi::{
    Aggregate, Argument, Callbacks, Invocation, SQLITE_ATTACH, SQLITE_DENY, SQLITE_IGNORE,
    SQLITE_OK,
};

/// `pdo_sqlite`'s callbacks, what every connection it opens serves with.
pub(crate) struct PdoSqlite;

pub(crate) static PDO_SQLITE: PdoSqlite = PdoSqlite;

impl Callbacks for PdoSqlite {
    fn scalar(
        &self,
        req: &mut Request,
        handler: &Callable,
        call: &mut Invocation<'_>,
    ) -> Result<(), Thrown> {
        scalar(req, handler, call)
    }

    fn step(
        &self,
        req: &mut Request,
        handler: &Callable,
        state: &mut Option<Box<Aggregate>>,
        args: Vec<Argument>,
    ) -> Result<(), Thrown> {
        step(req, handler, state, args)
    }

    fn finalize(
        &self,
        req: &mut Request,
        handler: &Callable,
        state: Option<Box<Aggregate>>,
        call: &mut Invocation<'_>,
    ) -> Result<(), Thrown> {
        finalize(req, handler, state, call)
    }

    fn collate(
        &self,
        req: &mut Request,
        handler: &Callable,
        left: &[u8],
        right: &[u8],
    ) -> (i32, Option<Thrown>) {
        collate(req, handler, left, right)
    }

    fn authorize(
        &self,
        req: &mut Request,
        handler: Option<&Callable>,
        action: i32,
        names: [Option<Vec<u8>>; 4],
        pending: bool,
    ) -> Result<i32, Thrown> {
        authorize(req, handler, action, names, pending)
    }
}

/// An argument as the callback is handed it.
fn argument(req: &mut Request, argument: Argument) -> Result<Owner<Value>, Thrown> {
    let value = match argument {
        // `sqlite3_value_int`, the library's 32 bits.
        #[allow(clippy::cast_possible_truncation)]
        Argument::Int(n) => Value::int(i64::from(n as i32)),
        Argument::Float(f) => Value::float(f),
        Argument::Null => Value::null(),
        Argument::Text(bytes) => return Ok(PhpString::new(req, &bytes)?.into_value()),
    };
    Ok(Owner::take(req, value))
}

/// The views of `values`, as a call is handed them.
fn views(values: &[Owner<Value>]) -> Vec<Value> {
    values.iter().map(|value| **value).collect()
}

fn arguments(req: &mut Request, given: Vec<Argument>) -> Result<Vec<Owner<Value>>, Thrown> {
    let mut out = Vec::with_capacity(given.len());
    for one in given {
        out.push(argument(req, one)?);
    }
    Ok(out)
}

fn release_all(req: &mut Request, values: Vec<Owner<Value>>) {
    for value in values {
        value.give_up(req);
    }
}

/// The callback's return as the function's answer: an integer to the
/// library's 32 bits, `null`, a float, and a string of anything else.
fn result(
    req: &mut Request,
    returned: Owner<Value>,
    call: &mut Invocation<'_>,
) -> Result<(), Thrown> {
    let answered = match returned.deref().as_ref() {
        // php hands `sqlite3_result_int` its `zend_long`.
        #[allow(clippy::cast_possible_truncation)]
        ValueRef::Int(n) => {
            call.return_int(n as i32);
            Ok(())
        }
        ValueRef::Null => {
            call.return_null();
            Ok(())
        }
        ValueRef::Float(f) => {
            call.return_double(f);
            Ok(())
        }
        _ => with_string(req, &returned, |text| call.return_text(text)),
    };
    returned.give_up(req);
    answered
}

/// `do_callback` for a scalar function.
fn scalar(
    req: &mut Request,
    handler: &Callable,
    invocation: &mut Invocation<'_>,
) -> Result<(), Thrown> {
    let values = arguments(req, invocation.arguments())?;
    let returned = call(req, handler, &views(&values));
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
        let context = PhpRef::new(req, Owner::null())?.into_value();
        *state = Some(Box::new(Aggregate {
            value: context,
            row: 0,
        }));
    }
    let held = state.as_mut().and_then(|held| {
        held.row += 1;
        let ValueRef::Reference(context) = held.value.as_ref() else {
            return None;
        };
        Some((context, held.row))
    });
    held.ok_or_else(|| {
        php_ext::raising::fatal(req, "InternalError: an aggregate without its reference")
    })
}

/// `do_callback` for an aggregate's step: the aggregate's reference, the row number
/// and the arguments handed over, and what it returns kept in the
/// reference.
fn step(
    req: &mut Request,
    handler: &Callable,
    state: &mut Option<Box<Aggregate>>,
    given: Vec<Argument>,
) -> Result<(), Thrown> {
    let (context, row) = state_of(req, state)?;
    let mut values = vec![context.as_value(), Value::int(row)];
    let extra = arguments(req, given)?;
    values.extend(views(&extra));
    let returned = call(req, handler, &values);
    release_all(req, extra);
    let returned = returned?;
    context.set(req, returned)
}

/// `do_callback` for an aggregate's final: the aggregate's reference and
/// the row number, which counts this call too, and its return as the
/// answer; the reference given up.
fn finalize(
    req: &mut Request,
    handler: &Callable,
    mut state: Option<Box<Aggregate>>,
    invocation: &mut Invocation<'_>,
) -> Result<(), Thrown> {
    let held = state_of(req, &mut state);
    let returned =
        held.and_then(|(context, row)| call(req, handler, &[context.as_value(), Value::int(row)]));
    crate::ffi::drop_aggregate(req, state);
    match returned {
        Ok(returned) => result(req, returned, invocation),
        Err(thrown) => {
            invocation.failed();
            Err(thrown)
        }
    }
}

/// `php_sqlite3_collation_callback`: the two strings compared by the
/// callback, whose answer must be an integer; `0` where it threw, and
/// `FAILURE` beside the `TypeError` for an answer of another type.
fn collate(
    req: &mut Request,
    handler: &Callable,
    left: &[u8],
    right: &[u8],
) -> (i32, Option<Thrown>) {
    let values = match (PhpString::new(req, left), PhpString::new(req, right)) {
        (Ok(left), Ok(right)) => vec![left.into_value(), right.into_value()],
        (Err(thrown), _) | (_, Err(thrown)) => return (0, Some(thrown)),
    };
    let returned = call(req, handler, &views(&values));
    release_all(req, values);
    let returned = match returned {
        Ok(returned) => returned,
        Err(thrown) => return (0, Some(thrown)),
    };
    if let Some(n) = returned.deref().as_int() {
        let order = match n.signum() {
            -1 => -1,
            0 => 0,
            _ => 1,
        };
        returned.give_up(req);
        return (order, None);
    }
    let name = req.frames().active_function_name();
    let given = value_name(req, &returned);
    returned.give_up(req);
    let thrown = type_error(
        req,
        &format!(
            "{name}(): Return value of the collation callback must be of type int, {given} returned"
        ),
    );
    (-1, Some(thrown))
}

/// `authorizer`'s user half: the action and its four names handed to
/// the callback, whose answer must be one of the three the library
/// takes.
fn authorize(
    req: &mut Request,
    handler: Option<&Callable>,
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
    let mut values = vec![Owner::int(i64::from(action))];
    for name in names {
        values.push(match name {
            Some(name) => PhpString::new(req, &name)?.into_value(),
            None => Owner::null(),
        });
    }
    let returned = call(req, handler, &views(&values));
    release_all(req, values);
    let returned = returned?;
    let name = req.frames().active_function_name();
    let answer = returned.deref().as_int();
    let given = value_name(req, &returned);
    returned.give_up(req);
    let Some(answer) = answer else {
        return Err(type_error(
            req,
            &format!(
                "{name}(): Return value of the authorizer callback must be of type int, {given} returned"
            ),
        ));
    };
    match i32::try_from(answer) {
        Ok(answer @ (SQLITE_OK | SQLITE_IGNORE | SQLITE_DENY)) => Ok(answer),
        _ => Err(value_error(
            req,
            &format!(
                "{name}(): Return value of the authorizer callback must be one of Pdo\\Sqlite::OK, Pdo\\Sqlite::DENY, or Pdo\\Sqlite::IGNORE"
            ),
        )),
    }
}
