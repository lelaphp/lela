//! The classes this extension raises, each resolved once as the engine
//! starts, as php's `MINIT` keeps `sodium_exception_ce` (ADR-0005: engine state,
//! immutable): a raise names its class by the identity, never by a name
//! looked up as it throws (#3403).

use php_ext::abi::{EngineState, Refusal, Start};
use php_ext::raising::{self, Thrown};
use php_ext::request::Request;
use php_ext::values::EngineClassId;

use crate::SodiumException;

/// The classes the natives raise, by their identity in this engine.
#[derive(Clone, Copy, EngineState)]
pub(crate) struct Throwables {
    pub(crate) sodium_exception: EngineClassId,
}

impl Throwables {
    /// Each class looked up once, at the engine start.
    pub(crate) fn start(start: &Start) -> Result<Throwables, Refusal> {
        Ok(Throwables {
            sodium_exception: start.class::<SodiumException>()?,
        })
    }

    /// What the engine start resolved, or the `InternalError` fatal
    /// where this engine started no `sodium`.
    pub(crate) fn of(req: &mut Request) -> Result<Throwables, Thrown> {
        match req.engine::<Throwables>() {
            Some(throwables) => Ok(*throwables),
            None => Err(raising::fatal(
                req,
                "InternalError: sodium's classes were not resolved at its engine start",
            )),
        }
    }
}
