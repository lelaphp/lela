//! The classes this extension raises, each resolved once as the engine
//! starts, as php's `MINIT` keeps `zend_ce_value_error` and
//! `php_filter_failed_exception_ce` (ADR-0005: engine state, immutable):
//! a raise names its class by the identity, never by a name looked up as
//! it throws (#3403).

use php_ext::abi::{EngineState, Refusal, Start};
use php_ext::attr::php_class;
use php_ext::raising::{self, Thrown};
use php_ext::request::Request;
use php_ext::values::ClassId;

use crate::FilterFailedException;

/// Core's `ValueError`.
#[php_class(extern)]
pub(crate) struct ValueError;

/// The classes the natives raise, by their identity in this engine.
#[derive(Clone, Copy, EngineState)]
pub(crate) struct Throwables {
    pub(crate) value_error: ClassId,
    pub(crate) filter_failed_exception: ClassId,
}

impl Throwables {
    /// Each class looked up once, at the engine start.
    pub(crate) fn start(start: &Start) -> Result<Throwables, Refusal> {
        Ok(Throwables {
            value_error: start.class::<ValueError>()?,
            filter_failed_exception: start.class::<FilterFailedException>()?,
        })
    }

    /// What the engine start resolved, or the `InternalError` fatal
    /// where this engine started no `filter`.
    pub(crate) fn of(req: &mut Request) -> Result<Throwables, Thrown> {
        match req.engine::<Throwables>() {
            Some(throwables) => Ok(*throwables),
            None => Err(raising::fatal(
                req,
                "InternalError: filter's classes were not resolved at its engine start",
            )),
        }
    }
}
