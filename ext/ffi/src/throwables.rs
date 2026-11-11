//! The classes this extension raises, each resolved once as the engine
//! starts, as php's `MINIT` keeps `zend_ce_error`,
//! `zend_ffi_exception_ce` and `zend_ffi_parser_exception_ce`
//! (ADR-0005: engine state, immutable): a raise names its class by the
//! identity, never by a name looked up as it throws (#3403).

use php_ext::abi::{EngineState, Refusal, Start};
use php_ext::raising::{self, Thrown};
use php_ext::request::Request;
use php_ext::values::EngineClassId;

use crate::preload::Preloads;
use crate::{Error, Exception, ParserException};

/// The classes the natives raise, by their identity in this engine.
#[derive(Clone, Copy, EngineState)]
pub(crate) struct Throwables {
    pub(crate) error: EngineClassId,
    pub(crate) exception: EngineClassId,
    pub(crate) parser_exception: EngineClassId,
}

impl Throwables {
    /// Each class looked up once, at the engine start.
    pub(crate) fn start(start: &Start) -> Result<Throwables, Refusal> {
        Ok(Throwables {
            error: start.class::<Error>()?,
            exception: start.class::<Exception>()?,
            parser_exception: start.class::<ParserException>()?,
        })
    }

    /// What the engine start resolved, or the `InternalError` fatal
    /// where this engine started no `ffi`.
    pub(crate) fn of(req: &mut Request) -> Result<Throwables, Thrown> {
        match req.engine::<Preloads>().map(|state| state.throwables) {
            Some(throwables) => Ok(throwables),
            None => Err(raising::fatal(
                req,
                "InternalError: ffi's classes were not resolved at its engine start",
            )),
        }
    }
}
