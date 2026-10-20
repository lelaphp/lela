//! The PHP side of the callbacks `sqlite3.c` registers: a user function
//! and an aggregate's step and final (`sqlite3_do_callback`), a collation
//! (`php_sqlite3_callback_compare`) and the authorizer
//! (`php_sqlite3_authorizer`), served through the request the library's
//! call left reachable (`ffi`). The two values differ in what the
//! authorizer's errors raise, which `enableExceptions` chooses.

use php_ext::abi::Callable;
use php_ext::call::call;
use php_ext::coerce::with_string;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString, Value, ValueRef};
use php_ext_pdo_sqlite::ffi::{
    Aggregate, Argument, Callbacks, Invocation, SQLITE_ATTACH, SQLITE_DENY, SQLITE_IGNORE,
    SQLITE_OK, drop_aggregate,
};

use crate::conn::{fail, fail_after};

/// `sqlite3.c`'s callbacks for a connection whose errors warn, or throw.
pub(crate) struct Sqlite3 {
    exceptions: bool,
}

static WARNINGS: Sqlite3 = Sqlite3 { exceptions: false };
static EXCEPTIONS: Sqlite3 = Sqlite3 { exceptions: true };

/// What a connection serves with under `enableExceptions(exceptions)`.
pub(crate) fn serving(exceptions: bool) -> &'static Sqlite3 {
    if exceptions { &EXCEPTIONS } else { &WARNINGS }
}

/// The arguments as `sqlite3_do_callback` hands them: an integer as the
/// library's 64 bits, text and blobs alike as strings.
fn arguments(req: &mut Request, given: Vec<Argument>) -> Result<Vec<Owner<Value>>, Thrown> {
    let mut out = Vec::with_capacity(given.len());
    for one in given {
        out.push(match one {
            Argument::Int(n) => Owner::int(n),
            Argument::Float(f) => Owner::float(f),
            Argument::Null => Owner::null(),
            Argument::Text(bytes) => match PhpString::new(req, &bytes) {
                Ok(text) => text.into_value(),
                Err(thrown) => {
                    release_all(req, out);
                    return Err(thrown);
                }
            },
        });
    }
    Ok(out)
}

fn release_all(req: &mut Request, values: Vec<Owner<Value>>) {
    for value in values {
        value.give_up(req);
    }
}

/// The views of `values`, as a call is handed them.
fn views(values: &[Owner<Value>]) -> Vec<Value> {
    values.iter().map(|value| **value).collect()
}

/// The callback's return as the function's answer: an integer, `null`
/// and a float as they are, and anything else as its string, the answer
/// left unset where that conversion throws.
fn answer(
    req: &mut Request,
    returned: Owner<Value>,
    call: &mut Invocation<'_>,
) -> Result<(), Thrown> {
    let answered = match returned.deref().as_ref() {
        ValueRef::Int(n) => {
            call.return_int64(n);
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

impl Callbacks for Sqlite3 {
    /// `php_sqlite3_callback_func`.
    fn scalar(
        &self,
        req: &mut Request,
        handler: &Callable,
        invocation: &mut Invocation<'_>,
    ) -> Result<(), Thrown> {
        let values = arguments(req, invocation.arguments())?;
        let returned = call(req, handler, &views(&values));
        release_all(req, values);
        match returned {
            Ok(returned) => answer(req, returned, invocation),
            Err(thrown) => {
                invocation.failed();
                Err(thrown)
            }
        }
    }

    /// `php_sqlite3_callback_step`: the row counted first, then the
    /// aggregate's value, the count and the arguments handed over, and
    /// what the step returns kept as the value.
    fn step(
        &self,
        req: &mut Request,
        handler: &Callable,
        state: &mut Option<Box<Aggregate>>,
        args: Vec<Argument>,
    ) -> Result<(), Thrown> {
        let held = match state {
            Some(held) => held,
            None => state.insert(Box::new(Aggregate {
                value: Owner::null(),
                row: 0,
            })),
        };
        held.row += 1;
        let extra = arguments(req, args)?;
        let mut values = vec![*held.value, Value::int(held.row)];
        values.extend(views(&extra));
        let returned = call(req, handler, &values);
        release_all(req, extra);
        let (kept, thrown) = match returned {
            Ok(value) => (value, None),
            Err(thrown) => (Owner::null(), Some(thrown)),
        };
        std::mem::replace(&mut held.value, kept).give_up(req);
        thrown.map_or(Ok(()), Err)
    }

    /// `php_sqlite3_callback_final`: the value and a count of zero, the
    /// return as the answer, and the value given up after it.
    fn finalize(
        &self,
        req: &mut Request,
        handler: &Callable,
        state: Option<Box<Aggregate>>,
        invocation: &mut Invocation<'_>,
    ) -> Result<(), Thrown> {
        let value = state.as_ref().map_or(Value::null(), |held| *held.value);
        let returned = call(req, handler, &[value, Value::int(0)]);
        let answered = match returned {
            Ok(returned) => answer(req, returned, invocation),
            Err(thrown) => {
                invocation.failed();
                Err(thrown)
            }
        };
        drop_aggregate(req, state);
        answered
    }

    /// `php_sqlite3_callback_compare`: the callback's integer as the
    /// order, `0` where it threw, and php's warning and `0` for an answer
    /// of any other type.
    fn collate(
        &self,
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
        let order = returned.deref().as_int();
        returned.give_up(req);
        if let Some(n) = order {
            // php keeps the `zend_long` in an `int`.
            #[allow(clippy::cast_possible_truncation)]
            return (n as i32, None);
        }
        docref(
            req,
            Level::Warning,
            "An error occurred while invoking the compare callback (invalid return type).  Collation behaviour is undefined.",
        );
        (0, None)
    }

    /// `php_sqlite3_authorizer`: an `ATTACH` held to `open_basedir`
    /// first, then the handler's answer, which must be one of the three
    /// the library takes.
    fn authorize(
        &self,
        req: &mut Request,
        handler: Option<&Callable>,
        action: i32,
        names: [Option<Vec<u8>>; 4],
        pending: bool,
    ) -> Result<i32, Thrown> {
        if action == SQLITE_ATTACH
            && req
                .ini()
                .get(b"open_basedir")
                .is_some_and(|v| !v.is_empty())
        {
            match names[0].as_deref() {
                None => return Ok(SQLITE_DENY),
                Some(b":memory:" | b"") => {}
                Some(name) if name.starts_with(b"file:") => return Ok(SQLITE_DENY),
                Some(name) => {
                    if req.check_open_basedir(name, true).is_err() {
                        return Ok(SQLITE_DENY);
                    }
                }
            }
        }
        let Some(handler) = handler else {
            return Ok(SQLITE_OK);
        };
        if pending {
            return Ok(SQLITE_DENY);
        }
        let mut values = vec![Owner::int(i64::from(action))];
        for name in names {
            values.push(match name.map(|name| PhpString::new(req, &name)) {
                Some(Ok(name)) => name.into_value(),
                Some(Err(thrown)) => {
                    release_all(req, values);
                    return Err(thrown);
                }
                None => Owner::null(),
            });
        }
        let returned = call(req, handler, &views(&values));
        release_all(req, values);
        let exceptions = self.exceptions;
        let returned = match returned {
            Ok(returned) => returned,
            Err(thrown) => {
                let text = "An error occurred while invoking the authorizer callback";
                return Err(fail_after(req, exceptions, thrown, 0, text));
            }
        };
        let answer = returned.deref().as_int();
        returned.give_up(req);
        let Some(answer) = answer else {
            let text = "The authorizer callback returned an invalid type: expected int";
            fail(req, exceptions, None, 0, text)?;
            return Ok(SQLITE_DENY);
        };
        // php keeps the `zend_long` in an `int`.
        #[allow(clippy::cast_possible_truncation)]
        let answer = answer as i32;
        if matches!(answer, SQLITE_OK | SQLITE_IGNORE | SQLITE_DENY) {
            return Ok(answer);
        }
        let text = format!("The authorizer callback returned an invalid value: {answer}");
        fail(req, exceptions, None, 0, &text)?;
        Ok(SQLITE_DENY)
    }
}
