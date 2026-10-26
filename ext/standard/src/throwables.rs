//! The classes this extension raises, each resolved once as the engine
//! starts, as php's `MINIT` keeps `zend_ce_error`, `zend_ce_value_error`
//! and `assertion_error_ce` (ADR-0005: engine state, immutable): a raise
//! names its class by the identity, never by a name looked up as it throws
//! (#3403).

use php_ext::abi::{Refusal, Start};
use php_ext::attr::php_class;
use php_ext::raising::{self, Thrown};
use php_ext::request::Request;
use php_ext::values::ClassId;

use crate::assert::{AssertionError, Error};

/// Core's `ValueError`.
#[php_class(extern)]
pub(crate) struct ValueError;

/// Core's `TypeError`.
#[php_class(extern)]
pub(crate) struct TypeError;

/// Core's `Exception`.
#[php_class(extern)]
pub(crate) struct Exception;

/// Core's `ArithmeticError`.
#[php_class(extern)]
pub(crate) struct ArithmeticError;

/// Core's `DivisionByZeroError`.
#[php_class(extern)]
pub(crate) struct DivisionByZeroError;

/// ext/uri's `Uri\InvalidUriException`.
#[php_class(extern, namespace(Uri))]
pub(crate) struct InvalidUriException;

/// The classes the natives raise, by their identity in this engine.
#[derive(Clone, Copy)]
pub(crate) struct Throwables {
    pub(crate) error: ClassId,
    pub(crate) value_error: ClassId,
    pub(crate) type_error: ClassId,
    pub(crate) exception: ClassId,
    pub(crate) arithmetic_error: ClassId,
    pub(crate) division_by_zero_error: ClassId,
    pub(crate) assertion_error: ClassId,
    pub(crate) invalid_uri_exception: ClassId,
}

impl Throwables {
    /// Each class looked up once, at the engine start.
    pub(crate) fn start(start: &Start) -> Result<Throwables, Refusal> {
        Ok(Throwables {
            error: start.class::<Error>()?,
            value_error: start.class::<ValueError>()?,
            type_error: start.class::<TypeError>()?,
            exception: start.class::<Exception>()?,
            arithmetic_error: start.class::<ArithmeticError>()?,
            division_by_zero_error: start.class::<DivisionByZeroError>()?,
            assertion_error: start.class::<AssertionError>()?,
            invalid_uri_exception: start.class::<InvalidUriException>()?,
        })
    }

    /// What the engine start resolved, or the `InternalError` fatal
    /// where this engine started no `standard`.
    pub(crate) fn of(req: &mut Request) -> Result<Throwables, Thrown> {
        match req.engine::<Throwables>() {
            Some(throwables) => Ok(*throwables),
            None => Err(raising::fatal(
                req,
                "InternalError: standard's classes were not resolved at its engine start",
            )),
        }
    }
}
