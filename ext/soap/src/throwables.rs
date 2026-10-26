//! The classes this extension raises, each resolved once as the engine
//! starts, as php's `MINIT` keeps `zend_ce_error`, `zend_ce_type_error`
//! and `soap_fault_class_entry` (ADR-0005: engine state, immutable): a
//! raise names its class by the identity, never by a name looked up as it
//! throws (#3403).

use php_ext::abi::{Refusal, Start};
use php_ext::attr::php_class;
use php_ext::raising::{self, Thrown};
use php_ext::request::Request;
use php_ext::values::ClassId;

use crate::classes::SoapFault;

/// Core's `Error`.
#[php_class(extern)]
pub(crate) struct Error;

/// Core's `TypeError`.
#[php_class(extern)]
pub(crate) struct TypeError;

/// Core's `ValueError`.
#[php_class(extern)]
pub(crate) struct ValueError;

/// ext/random's `Random\RandomException`, which `php_random_bytes`
/// throws for the digest's nonce though soap does not depend on random.
#[php_class(extern, namespace(Random))]
pub(crate) struct RandomException;

/// The classes the natives raise, by their identity in this engine.
#[derive(Clone, Copy)]
pub(crate) struct Throwables {
    pub(crate) error: ClassId,
    pub(crate) type_error: ClassId,
    pub(crate) value_error: ClassId,
    pub(crate) soap_fault: ClassId,
    pub(crate) random_exception: ClassId,
}

impl Throwables {
    /// Each class looked up once, at the engine start.
    pub(crate) fn start(start: &Start) -> Result<Throwables, Refusal> {
        Ok(Throwables {
            error: start.class::<Error>()?,
            type_error: start.class::<TypeError>()?,
            value_error: start.class::<ValueError>()?,
            soap_fault: start.class::<SoapFault>()?,
            random_exception: start.class::<RandomException>()?,
        })
    }

    /// What the engine start resolved, or the `InternalError` fatal
    /// where this engine started no `soap`.
    pub(crate) fn of(req: &mut Request) -> Result<Throwables, Thrown> {
        match req.engine::<Throwables>() {
            Some(throwables) => Ok(*throwables),
            None => Err(raising::fatal(
                req,
                "InternalError: soap's classes were not resolved at its engine start",
            )),
        }
    }
}
