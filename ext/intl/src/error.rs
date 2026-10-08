//! `intl_error.c`: the error state php keeps twice, once for the request
//! (`INTL_G(g_error)`, what `intl_get_error_code` and
//! `intl_get_error_message` answer) and once on each object (the
//! `intl_error` every `*_object` carries first, what its
//! `getErrorCode` and `getErrorMessage` answer). A message is prefixed
//! with the native that set it, `get_active_function_or_method_name`;
//! the request's is raised at `intl.error_level` and thrown as
//! `IntlException` under `intl.use_exceptions` when it is set.

use std::any::Any;
use std::fmt::Write as _;

use php_ext::abi::OrFalse;
use php_ext::call::write_prop;
use php_ext::raising::{Level, Thrown, docref, throw, throw_chained};
use php_ext::request::Request;
use php_ext::values::{PhpObject, PhpString};

use crate::State;
use crate::ffi::Code;

/// `intl_error`: the last code and the message a native wrote for it,
/// sixteen bytes as php's, the message behind one thin pointer as its
/// `zend_string *` is, so an object's payload fits php's struct.
#[derive(Debug, Default, Clone)]
pub(crate) struct IntlError {
    pub(crate) code: Code,
    #[allow(clippy::box_collection, reason = "a thin pointer, php's zend_string *")]
    message: Option<Box<Vec<u8>>>,
}

impl IntlError {
    /// `intl_error_reset`.
    pub(crate) fn reset(&mut self) {
        self.code = Code::ZERO;
        self.message = None;
    }

    /// `intl_error_get_message`: `<message>: <code name>`, or the code's
    /// name alone.
    pub(crate) fn message(&self) -> Vec<u8> {
        let name = self.code.name().as_bytes();
        match &self.message {
            Some(message) => [message.as_slice(), b": ", name].concat(),
            None => name.to_vec(),
        }
    }
}

/// An object's payload that carries php's `intl_error`, the first
/// member of each `*_object`.
pub(crate) trait Carrier: Any + Send {
    fn error(&mut self) -> &mut IntlError;
}

/// `INTL_G(g_error)`.
fn global(req: &mut Request) -> &mut IntlError {
    &mut req.state::<State>().error
}

/// `<native>(): <message>`, as `intl_error_set_custom_msg` prefixes it.
fn prefixed(req: &Request, message: &[u8]) -> Vec<u8> {
    [req.active_function_name().as_bytes(), b"(): ", message].concat()
}

/// `intl_error_reset(NULL)`.
pub(crate) fn reset(req: &mut Request) {
    global(req).reset();
}

/// `intl_error_set_code(NULL, code)`.
pub(crate) fn set_code(req: &mut Request, code: Code) {
    global(req).code = code;
}

/// `intl_error_get_code(NULL)`.
pub(crate) fn code(req: &mut Request) -> Code {
    global(req).code
}

/// `INTL_G(error_level)`: `intl.error_level` as a quantity.
fn error_level(req: &mut Request) -> Option<Level> {
    if req.state::<State>().constructing {
        return None;
    }
    let value = req.ini_get(b"intl.error_level")?;
    let (bits, _) = Request::ini_parse_quantity(value);
    [
        Level::Error,
        Level::Warning,
        Level::Parse,
        Level::Notice,
        Level::CoreError,
        Level::CoreWarning,
        Level::CompileError,
        Level::CompileWarning,
        Level::UserError,
        Level::UserWarning,
        Level::UserNotice,
        Level::Strict,
        Level::RecoverableError,
        Level::Deprecated,
        Level::UserDeprecated,
    ]
    .into_iter()
    .find(|level| level.bit() == bits)
}

/// `INTL_G(use_exceptions)`.
pub(crate) fn use_exceptions(req: &mut Request) -> bool {
    if req.state::<State>().constructing {
        return true;
    }
    req.ini_get(b"intl.use_exceptions")
        .is_some_and(Request::ini_parse_bool)
}

/// A constructor's body, as php's `__construct`s wrap theirs: errors
/// thrown as `IntlException` and raised at no level while it runs,
/// whatever the ini entries say, and the previous setting restored.
pub(crate) fn constructing<T>(req: &mut Request, f: impl FnOnce(&mut Request) -> T) -> T {
    let old = std::mem::replace(&mut req.state::<State>().constructing, true);
    let answer = f(req);
    req.state::<State>().constructing = old;
    answer
}

/// `intl_error_set_custom_msg(NULL, msg)`: raised at `intl.error_level`,
/// thrown as `IntlException` under `intl.use_exceptions`, and kept as
/// the request's message.
pub(crate) fn set_custom_msg(req: &mut Request, message: impl AsRef<[u8]>) -> Result<(), Thrown> {
    custom_msg(req, None, message.as_ref())
}

/// [`set_custom_msg`] where `pending` may already be thrown, as php's C
/// goes on past a throw: the `IntlException` chained to it, and it
/// handed on where nothing more is thrown.
fn custom_msg(req: &mut Request, pending: Option<Thrown>, message: &[u8]) -> Result<(), Thrown> {
    let text = prefixed(req, message);
    if let Some(level) = error_level(req) {
        docref(req, level, &String::from_utf8_lossy(message));
    }
    let thrown = if use_exceptions(req) {
        Some(intl_exception(req, pending, &text))
    } else {
        pending
    };
    global(req).message = Some(Box::new(text));
    thrown.map_or(Ok(()), Err)
}

/// `zend_throw_exception(IntlException, text)`, chained to what is
/// pending as `zend_throw_exception_internal` chains it. The message is
/// php's `char *`, bytes: where they are not UTF-8 they are written over
/// the empty one the throw laid.
fn intl_exception(req: &mut Request, pending: Option<Thrown>, text: &[u8]) -> Thrown {
    let utf8 = std::str::from_utf8(text);
    let message = utf8.unwrap_or_default();
    let thrown = match pending {
        Some(pending) => throw_chained(req, pending, "IntlException", message),
        None => throw(req, "IntlException", message),
    };
    let Some(object) = thrown.exception().copied().filter(|_| utf8.is_err()) else {
        return thrown;
    };
    let written = PhpString::new(req, text)
        .and_then(|text| write_prop(req, &object, b"message", text.into_value()));
    match written {
        Ok(()) => thrown,
        Err(refused) => thrown.chain(req, refused),
    }
}

/// `intl_error_set(NULL, code, msg)`.
pub(crate) fn set(req: &mut Request, code: Code, message: impl AsRef<[u8]>) -> Result<(), Thrown> {
    set_code(req, code);
    set_custom_msg(req, message)
}

/// `intl_error_get_message(NULL)`.
pub(crate) fn message(req: &mut Request) -> Vec<u8> {
    global(req).message()
}

/// The object's `intl_error`, or `None` for an object of another class.
fn carried<P: Carrier>(req: &mut Request, object: PhpObject) -> Option<&mut IntlError> {
    object.native_mut::<P>(req).map(Carrier::error)
}

/// `intl_error_reset(obj)`, as `INTL_METHOD_FETCH_OBJECT` resets it.
pub(crate) fn reset_object<P: Carrier>(req: &mut Request, object: PhpObject) {
    if let Some(error) = carried::<P>(req, object) {
        error.reset();
    }
}

/// `intl_errors_reset(obj)`: the object's and the request's.
pub(crate) fn errors_reset<P: Carrier>(req: &mut Request, object: PhpObject) {
    reset_object::<P>(req, object);
    reset(req);
}

/// `intl_errors_set_code(obj, code)`: the object's and the request's.
pub(crate) fn errors_set_code<P: Carrier>(req: &mut Request, object: PhpObject, code: Code) {
    if let Some(error) = carried::<P>(req, object) {
        error.code = code;
    }
    set_code(req, code);
}

/// `intl_errors_set_custom_msg(obj, msg)`: the object's message, then
/// the request's, which raises.
pub(crate) fn errors_set_custom_msg<P: Carrier>(
    req: &mut Request,
    object: PhpObject,
    message: &str,
) -> Result<(), Thrown> {
    let text = prefixed(req, message.as_bytes());
    if let Some(error) = carried::<P>(req, object) {
        error.message = Some(Box::new(text));
    }
    set_custom_msg(req, message)
}

/// `intl_errors_set(obj, code, msg)`.
pub(crate) fn errors_set<P: Carrier>(
    req: &mut Request,
    object: PhpObject,
    code: Code,
    message: &str,
) -> Result<(), Thrown> {
    errors_set_code::<P>(req, object, code);
    errors_set_custom_msg::<P>(req, object, message)
}

/// `intl_error_get_code(obj)`.
pub(crate) fn object_code<P: Carrier>(req: &mut Request, object: PhpObject) -> Code {
    carried::<P>(req, object).map_or(Code::ZERO, |error| error.code)
}

/// `intl_error_get_message(obj)`.
pub(crate) fn object_message<P: Carrier>(req: &mut Request, object: PhpObject) -> Vec<u8> {
    carried::<P>(req, object).map_or_else(|| Code::ZERO.name().as_bytes().to_vec(), |e| e.message())
}

/// `INTL_METHOD_CHECK_STATUS(obj, msg)`: the object's code copied to
/// the request; where it is a failure, the message set on both and
/// `Err(None)`, which the native answers `false` (or null) for.
pub(crate) fn check_object<P: Carrier>(
    req: &mut Request,
    object: PhpObject,
    message: &str,
) -> Result<(), Option<Thrown>> {
    let code = object_code::<P>(req, object);
    set_code(req, code);
    if code.is_failure() {
        return Err(errors_set_custom_msg::<P>(req, object, message).err());
    }
    Ok(())
}

/// `INTL_CHECK_STATUS(err, msg)`: the code set on the request; where it
/// is a failure, the message too and `Err(None)`.
pub(crate) fn check(req: &mut Request, code: Code, message: &str) -> Result<(), Option<Thrown>> {
    set_code(req, code);
    if code.is_failure() {
        return Err(set_custom_msg(req, message).err());
    }
    Ok(())
}

/// `intl_parse_error_to_string`: where a pattern parse stopped, as php
/// words it.
pub(crate) fn parse_error_text(line: i32, offset: i32, pre: &[u8], post: &[u8]) -> String {
    let mut out = String::from("parse error ");
    let mut any = false;
    if line > 0 {
        write!(out, "on line {line}").ok();
        any = true;
    }
    if offset >= 0 {
        out.push_str(if any { ", " } else { "at " });
        write!(out, "offset {offset}").ok();
        any = true;
    }
    if !pre.is_empty() {
        if any {
            out.push_str(", ");
        }
        write!(out, "after \"{}\"", String::from_utf8_lossy(pre)).ok();
        any = true;
    }
    if !post.is_empty() {
        if any {
            out.push_str(", ");
        }
        write!(out, "before or at \"{}\"", String::from_utf8_lossy(post)).ok();
        any = true;
    }
    if !any {
        return "no parse error".to_owned();
    }
    out
}

/// `intl_parse_error_to_string` over ICU's `UParseError`, its contexts
/// read up to their NUL.
pub(crate) fn parse_error(pe: &crate::ffi::ParseError) -> String {
    let context = |c: &[u16; 16]| {
        let end = c.iter().position(|&u| u == 0).unwrap_or(c.len());
        crate::ffi::to_utf8(&c[..end]).unwrap_or_default()
    };
    parse_error_text(pe.line, pe.offset, &context(&pe.pre), &context(&pe.post))
}

/// The `intl_error *outside_error` php hands a function that may fail on
/// its caller's behalf (`common_date.cpp`, `timezone_class.cpp`): none,
/// so the request's error alone is set; a local one; or an object's.
pub(crate) enum Outside<'a> {
    Request,
    Local(&'a mut IntlError),
    Object(
        PhpObject,
        fn(&mut Request, PhpObject) -> Option<&mut IntlError>,
    ),
}

impl Outside<'_> {
    /// The error an object of payload `P` carries.
    pub(crate) fn of<P: Carrier>(object: PhpObject) -> Outside<'static> {
        Outside::Object(object, carried::<P>)
    }

    /// `intl_errors_set(err, code, msg)`: the outside error's code and
    /// message, then the request's, which raises.
    pub(crate) fn set(
        &mut self,
        req: &mut Request,
        code: Code,
        message: impl AsRef<[u8]>,
    ) -> Result<(), Thrown> {
        self.keep(req, code, message.as_ref());
        set(req, code, message)
    }

    /// [`Outside::set`] with `pending` already thrown: the
    /// `IntlException` chained to it, or it handed on; `Err` always.
    pub(crate) fn set_over(
        &mut self,
        req: &mut Request,
        pending: Thrown,
        code: Code,
        message: impl AsRef<[u8]>,
    ) -> Result<(), Thrown> {
        self.keep(req, code, message.as_ref());
        set_code(req, code);
        custom_msg(req, Some(pending), message.as_ref())
    }

    /// The outside error's code and message.
    fn keep(&mut self, req: &mut Request, code: Code, message: &[u8]) {
        let text = prefixed(req, message);
        let error = match self {
            Outside::Request => None,
            Outside::Local(error) => Some(&mut **error),
            Outside::Object(object, carried) => carried(req, *object),
        };
        if let Some(error) = error {
            error.code = code;
            error.message = Some(Box::new(text));
        }
    }

    /// `intl_error_get_code(err)`: the outside error's, zero for none.
    pub(crate) fn code(&mut self, req: &mut Request) -> Code {
        match self {
            Outside::Request => Code::ZERO,
            Outside::Local(error) => error.code,
            Outside::Object(object, carried) => {
                carried(req, *object).map_or(Code::ZERO, |e| e.code)
            }
        }
    }
}

/// What a native answers for a check that failed: `false` for
/// `Err(None)`, where php's `INTL_CHECK_STATUS` and its kin
/// `RETURN_FALSE`, and the throwable for `Err(Some(..))`.
pub(crate) fn or_false<T>(answer: Result<T, Option<Thrown>>) -> Result<OrFalse<T>, Thrown> {
    match answer {
        Ok(value) => Ok(OrFalse::Value(value)),
        Err(None) => Ok(OrFalse::False),
        Err(Some(thrown)) => Err(thrown),
    }
}
