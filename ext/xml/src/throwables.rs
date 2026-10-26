//! The classes this extension raises, each resolved once as the engine
//! starts, as php's `MINIT` keeps `zend_ce_error` and
//! `zend_ce_value_error` (ADR-0005: engine state, immutable): a raise
//! names its class by the identity, never by a name looked up as it throws
//! (#3403).

use php_ext::abi::{Refusal, Start};
use php_ext::attr::php_class;
use php_ext::raising::{self, Thrown};
use php_ext::request::Request;
use php_ext::values::ClassId;

/// Core's `Error`.
#[php_class(extern)]
pub(crate) struct Error;

/// Core's `ValueError`.
#[php_class(extern)]
pub(crate) struct ValueError;

/// The classes the natives raise, by their identity in this engine.
#[derive(Clone, Copy)]
pub(crate) struct Throwables {
    pub(crate) error: ClassId,
    pub(crate) value_error: ClassId,
}

impl Throwables {
    /// Each class looked up once, at the engine start.
    pub(crate) fn start(start: &Start) -> Result<Throwables, Refusal> {
        Ok(Throwables {
            error: start.class::<Error>()?,
            value_error: start.class::<ValueError>()?,
        })
    }

    /// What the engine start resolved, or the `InternalError` fatal
    /// where this engine started no `xml`.
    pub(crate) fn of(req: &mut Request) -> Result<Throwables, Thrown> {
        match req.engine::<Throwables>() {
            Some(throwables) => Ok(*throwables),
            None => Err(raising::fatal(
                req,
                "InternalError: xml's classes were not resolved at its engine start",
            )),
        }
    }
}
