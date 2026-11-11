//! The classes this extension raises, each resolved once as the engine
//! starts, as php's `MINIT` keeps `zend_ce_error`, `zend_ce_exception`
//! and `random_ce_Random_*` (ADR-0005: engine state, immutable): a raise
//! names its class by the identity, never by a name looked up as it throws
//! (#3403).

use php_ext::abi::{EngineState, Refusal, Start};
use php_ext::raising::{self, Thrown};
use php_ext::request::Request;
use php_ext::values::ClassId;

use crate::{BrokenRandomEngineError, Error, Exception, RandomException};

/// The classes the natives raise, by their identity in this engine.
#[derive(Clone, Copy, EngineState)]
pub(crate) struct Throwables {
    pub(crate) error: ClassId,
    pub(crate) exception: ClassId,
    pub(crate) random_exception: ClassId,
    pub(crate) broken_random_engine_error: ClassId,
}

impl Throwables {
    /// Each class looked up once, at the engine start.
    pub(crate) fn start(start: &Start) -> Result<Throwables, Refusal> {
        Ok(Throwables {
            error: start.class::<Error>()?,
            exception: start.class::<Exception>()?,
            random_exception: start.class::<RandomException>()?,
            broken_random_engine_error: start.class::<BrokenRandomEngineError>()?,
        })
    }

    /// What the engine start resolved, or the `InternalError` fatal
    /// where this engine started no `random`.
    pub(crate) fn of(req: &mut Request) -> Result<Throwables, Thrown> {
        match req.engine::<Throwables>() {
            Some(throwables) => Ok(*throwables),
            None => Err(raising::fatal(
                req,
                "InternalError: random's classes were not resolved at its engine start",
            )),
        }
    }
}
