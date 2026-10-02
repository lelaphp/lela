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

use php_ext::raising::{Level, Thrown, docref, throw};
use php_ext::request::Request;
use php_ext::values::PhpObject;

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
fn prefixed(req: &Request, message: &str) -> Vec<u8> {
    format!("{}(): {message}", req.active_function_name()).into_bytes()
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
pub(crate) fn set_custom_msg(req: &mut Request, message: &str) -> Result<(), Thrown> {
    let text = prefixed(req, message);
    if let Some(level) = error_level(req) {
        docref(req, level, message);
    }
    let thrown = use_exceptions(req).then(|| {
        let text = String::from_utf8_lossy(&text).into_owned();
        throw(req, "IntlException", &text)
    });
    global(req).message = Some(Box::new(text));
    thrown.map_or(Ok(()), Err)
}

/// `intl_error_set(NULL, code, msg)`.
pub(crate) fn set(req: &mut Request, code: Code, message: &str) -> Result<(), Thrown> {
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
    let text = prefixed(req, message);
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
        let _ = write!(out, "on line {line}");
        any = true;
    }
    if offset >= 0 {
        out.push_str(if any { ", " } else { "at " });
        let _ = write!(out, "offset {offset}");
        any = true;
    }
    if !pre.is_empty() {
        if any {
            out.push_str(", ");
        }
        let _ = write!(out, "after \"{}\"", String::from_utf8_lossy(pre));
        any = true;
    }
    if !post.is_empty() {
        if any {
            out.push_str(", ");
        }
        let _ = write!(out, "before or at \"{}\"", String::from_utf8_lossy(post));
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
