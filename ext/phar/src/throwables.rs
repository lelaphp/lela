//! The classes this extension raises, each resolved once as the engine
//! starts, as php's `MINIT` keeps `phar_ce_PharException` and
//! the `spl_ce_*` it includes (ADR-0005: engine state, immutable):
//! a raise names its class by the identity, never by a name looked up as
//! it throws (#3403).

use php_ext::abi::{EngineState, Refusal, Start};
use php_ext::attr::php_class;
use php_ext::raising::{self, Thrown};
use php_ext::request::Request;
use php_ext::values::ClassId;

use crate::object::PharException;

/// ext/spl's `BadMethodCallException`.
#[php_class(extern)]
pub(crate) struct BadMethodCallException;

/// ext/spl's `UnexpectedValueException`.
#[php_class(extern)]
pub(crate) struct UnexpectedValueException;

/// ext/spl's `RuntimeException`.
#[php_class(extern)]
pub(crate) struct RuntimeException;

/// ext/spl's `InvalidArgumentException`.
#[php_class(extern)]
pub(crate) struct InvalidArgumentException;

/// The classes the natives raise, by their identity in this engine.
#[derive(Clone, Copy, EngineState)]
#[allow(clippy::struct_field_names)] // each field is its class's name, which ends in Exception
pub(crate) struct Throwables {
    pub(crate) phar_exception: ClassId,
    pub(crate) bad_method_call_exception: ClassId,
    pub(crate) unexpected_value_exception: ClassId,
    pub(crate) runtime_exception: ClassId,
    pub(crate) invalid_argument_exception: ClassId,
}

impl Throwables {
    /// Each class looked up once, at the engine start.
    pub(crate) fn start(start: &Start) -> Result<Throwables, Refusal> {
        Ok(Throwables {
            phar_exception: start.class::<PharException>()?,
            bad_method_call_exception: start.class::<BadMethodCallException>()?,
            unexpected_value_exception: start.class::<UnexpectedValueException>()?,
            runtime_exception: start.class::<RuntimeException>()?,
            invalid_argument_exception: start.class::<InvalidArgumentException>()?,
        })
    }

    /// What the engine start resolved, or the `InternalError` fatal
    /// where this engine started no `phar`.
    pub(crate) fn of(req: &mut Request) -> Result<Throwables, Thrown> {
        match req.engine::<Throwables>() {
            Some(throwables) => Ok(*throwables),
            None => Err(raising::fatal(
                req,
                "InternalError: phar's classes were not resolved at its engine start",
            )),
        }
    }
}
