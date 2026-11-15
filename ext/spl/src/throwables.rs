//! The classes this extension raises, each resolved once as the engine
//! starts, as php's `MINIT` keeps `zend_ce_error`, `zend_ce_exception`
//! and its own `spl_ce_*` (ADR-0005: engine state, immutable):
//! a raise names its class by the identity, never by a name looked up as
//! it throws.

use php_ext::abi::{EngineState, Refusal, Start};
use php_ext::attr::php_class;
use php_ext::raising::{self, Thrown};
use php_ext::request::Request;
use php_ext::values::EngineClassId;

use crate::Exception;
use crate::exceptions::{
    BadMethodCallException, InvalidArgumentException, LogicException, OutOfBoundsException,
    OutOfRangeException, RuntimeException, UnexpectedValueException,
};

/// Core's `Error`.
#[php_class(extern)]
pub(crate) struct Error;

/// The classes the natives raise, by their identity in this engine.
#[derive(Clone, Copy, EngineState)]
pub(crate) struct Throwables {
    pub(crate) error: EngineClassId,
    pub(crate) exception: EngineClassId,
    pub(crate) logic_exception: EngineClassId,
    pub(crate) bad_method_call_exception: EngineClassId,
    pub(crate) invalid_argument_exception: EngineClassId,
    pub(crate) out_of_range_exception: EngineClassId,
    pub(crate) runtime_exception: EngineClassId,
    pub(crate) out_of_bounds_exception: EngineClassId,
    pub(crate) unexpected_value_exception: EngineClassId,
}

impl Throwables {
    /// Each class looked up once, at the engine start.
    pub(crate) fn start(start: &Start) -> Result<Throwables, Refusal> {
        Ok(Throwables {
            error: start.class::<Error>()?,
            exception: start.class::<Exception>()?,
            logic_exception: start.class::<LogicException>()?,
            bad_method_call_exception: start.class::<BadMethodCallException>()?,
            invalid_argument_exception: start.class::<InvalidArgumentException>()?,
            out_of_range_exception: start.class::<OutOfRangeException>()?,
            runtime_exception: start.class::<RuntimeException>()?,
            out_of_bounds_exception: start.class::<OutOfBoundsException>()?,
            unexpected_value_exception: start.class::<UnexpectedValueException>()?,
        })
    }

    /// What the engine start resolved, or the `InternalError` fatal
    /// where this engine started no `spl`.
    pub(crate) fn of(req: &mut Request) -> Result<Throwables, Thrown> {
        match req.engine::<Throwables>() {
            Some(throwables) => Ok(*throwables),
            None => Err(raising::fatal(
                req,
                "InternalError: spl's classes were not resolved at its engine start",
            )),
        }
    }
}
