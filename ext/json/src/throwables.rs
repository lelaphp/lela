//! The classes this extension raises, each resolved once as the engine
//! starts, as php's `MINIT` keeps `php_json_exception_ce` (ADR-0005:
//! engine state, immutable): a raise names its class by the identity,
//! never by a name looked up as it throws (#3403).

use php_ext::abi::{Refusal, Start};
use php_ext::raising::{self, Thrown};
use php_ext::request::Request;
use php_ext::values::ClassId;

use crate::JsonException;

/// The classes the natives raise, by their identity in this engine.
#[derive(Clone, Copy)]
pub(crate) struct Throwables {
    pub(crate) json_exception: ClassId,
}

impl Throwables {
    /// Each class looked up once, at the engine start.
    pub(crate) fn start(start: &Start) -> Result<Throwables, Refusal> {
        Ok(Throwables {
            json_exception: start.class::<JsonException>()?,
        })
    }

    /// What the engine start resolved, or the `InternalError` fatal
    /// where this engine started no `json`.
    pub(crate) fn of(req: &mut Request) -> Result<Throwables, Thrown> {
        match req.engine::<Throwables>() {
            Some(throwables) => Ok(*throwables),
            None => Err(raising::fatal(
                req,
                "InternalError: json's classes were not resolved at its engine start",
            )),
        }
    }
}
