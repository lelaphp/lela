//! The raising group (design §4.4): how a native reports what php
//! reports. The engine knows the active native's name and parameters
//! before the body, so the argument raisers take the position and the
//! tail of the message and the engine renders `fn(): Argument #N ($name)
//! must ...` itself. [`Thrown`] is the error every native returns. In
//! this skeleton nothing is rendered or dispatched to a user handler: a
//! diagnostic is kept on the request as its last error, and a `Thrown`
//! carries the throwable it would have made pending.

use crate::api::request::Request;
use crate::api::values::{PhpObject, PhpString, Value};

/// php's error levels, `E_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    Error,
    Warning,
    Parse,
    Notice,
    CoreError,
    CoreWarning,
    CompileError,
    CompileWarning,
    UserError,
    UserWarning,
    UserNotice,
    Strict,
    RecoverableError,
    Deprecated,
    UserDeprecated,
}

impl Level {
    /// php's number for the level, what `error_reporting` masks.
    #[must_use]
    pub(crate) const fn bit(self) -> i64 {
        match self {
            Level::Error => 1,
            Level::Warning => 2,
            Level::Parse => 4,
            Level::Notice => 8,
            Level::CoreError => 16,
            Level::CoreWarning => 32,
            Level::CompileError => 64,
            Level::CompileWarning => 128,
            Level::UserError => 256,
            Level::UserWarning => 512,
            Level::UserNotice => 1024,
            Level::Strict => 2048,
            Level::RecoverableError => 4096,
            Level::Deprecated => 8192,
            Level::UserDeprecated => 16384,
        }
    }
}

/// What a native returns when it does not return a value: a throwable is
/// pending on the request, or the request is ending in a fatal or an
/// `exit`. A `Thrown` in hand *is* the pending state; there is no second
/// channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Thrown {
    pending: Option<PhpObject>,
}

impl Thrown {
    pub(crate) const fn of(pending: Option<PhpObject>) -> Thrown {
        Thrown { pending }
    }

    /// The throwable that is pending, if one is: `None` for a fatal or
    /// an `exit`.
    #[must_use]
    pub fn exception(&self) -> Option<&PhpObject> {
        self.pending.as_ref()
    }
}

/// Raise a diagnostic at `level` with php's message text.
pub fn raise(req: &mut Request, level: Level, message: &str) {
    req.note_error(level, message);
}

/// `E_WARNING`.
pub fn warning(req: &mut Request, message: &str) {
    raise(req, Level::Warning, message);
}

/// `E_NOTICE`.
pub fn notice(req: &mut Request, message: &str) {
    raise(req, Level::Notice, message);
}

/// `E_DEPRECATED`.
pub fn deprecated(req: &mut Request, message: &str) {
    raise(req, Level::Deprecated, message);
}

/// `E_ERROR`: the request ends after this; the `Thrown` says so.
pub fn fatal(req: &mut Request, message: &str) -> Thrown {
    raise(req, Level::Error, message);
    req.end_in_fatal(message);
    Thrown::of(None)
}

/// Throw a new instance of `class` with `message`, `zend_throw_error`.
pub fn throw(req: &mut Request, class: &str, message: &str) -> Thrown {
    let throwable = new_throwable(req, class, message);
    throw_value(req, &throwable)
}

/// Make a throwable pending, `zend_throw_exception_object`; a value that
/// is not an object becomes an `Error`.
pub fn throw_value(req: &mut Request, throwable: &Value) -> Thrown {
    if let Some(object) = throwable.as_object() {
        return Thrown::of(Some(object.clone()));
    }
    let error = new_throwable(
        req,
        "Error",
        "Cannot throw objects that do not implement Throwable",
    );
    Thrown::of(error.as_object().cloned())
}

/// A new instance of the throwable class `class` with `message`, not yet
/// pending.
pub fn new_throwable(req: &mut Request, class: &str, message: &str) -> Value {
    let mut object = req.new_object_of(class);
    object.set_prop(
        b"message",
        Value::string(PhpString::from_bytes(message.as_bytes())),
    );
    object.as_value()
}

/// `TypeError` for the argument at `position`: the engine renders
/// `fn(): Argument #N ($name) must <tail>`.
pub fn arg_type_error(req: &mut Request, position: u32, tail: &str) -> Thrown {
    let message = req.argument_message(position, tail);
    throw(req, "TypeError", &message)
}

/// `ValueError` for the argument at `position`, rendered the same way.
pub fn arg_value_error(req: &mut Request, position: u32, tail: &str) -> Thrown {
    let message = req.argument_message(position, tail);
    throw(req, "ValueError", &message)
}

/// `ArgumentCountError`: `fn() expects <tail>`.
pub fn arg_count_error(req: &mut Request, tail: &str) -> Thrown {
    let message = format!("{}() expects {tail}", req.active_native());
    throw(req, "ArgumentCountError", &message)
}

/// `ArgumentCountError` for a call with fewer arguments than the
/// declaration requires, `zend_wrong_parameters_count_error`.
pub fn too_few_arguments(req: &mut Request, expected: u32, given: u32) -> Thrown {
    let word = if expected == 1 {
        "argument"
    } else {
        "arguments"
    };
    let message = format!(
        "{}() expects at least {expected} {word}, {given} given",
        req.active_native()
    );
    throw(req, "ArgumentCountError", &message)
}

/// The 8.1 deprecation for `null` passed to a non-nullable scalar
/// parameter of a native.
pub fn arg_null_deprecated(req: &mut Request, position: u32, type_name: &str) {
    let message = req.argument_message(position, &format!("be of type {type_name}, null given"));
    let text = format!("Passing null to parameter #{position} is deprecated: {message}");
    deprecated(req, &text);
}

/// `exit`: end the request with `status`, an `int` code or a `string`
/// printed first (design §5.9).
pub fn exit(req: &mut Request, status: &Value) -> Thrown {
    if let Some(text) = status.as_str_bytes() {
        req.write(text);
        req.end_in_exit(0);
    } else {
        let code = status
            .as_int()
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or(0);
        req.end_in_exit(code);
    }
    Thrown::of(None)
}
