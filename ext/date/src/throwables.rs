//! The classes this extension raises, each resolved once as the engine
//! starts, as php's `MINIT` keeps `zend_ce_error` and `date_ce_*_error`
//! and `date_ce_*_exception` (ADR-0005: engine state, immutable): a raise
//! names its class by the identity, never by a name looked up as it throws
//! (#3403).

use php_ext::abi::{EngineState, Refusal, Start};
use php_ext::raising::{self, Thrown};
use php_ext::request::Request;
use php_ext::values::EngineClassId;

use crate::classes::{
    DateError, DateException, DateInvalidOperationException, DateInvalidTimeZoneException,
    DateMalformedIntervalStringException, DateMalformedPeriodStringException,
    DateMalformedStringException, DateObjectError, DateRangeError, Error,
};

/// The classes the natives raise, by their identity in this engine.
#[derive(Clone, Copy, EngineState)]
pub(crate) struct Throwables {
    pub(crate) error: EngineClassId,
    pub(crate) date_error: EngineClassId,
    pub(crate) date_object_error: EngineClassId,
    pub(crate) date_range_error: EngineClassId,
    pub(crate) date_exception: EngineClassId,
    pub(crate) date_invalid_time_zone_exception: EngineClassId,
    pub(crate) date_invalid_operation_exception: EngineClassId,
    pub(crate) date_malformed_string_exception: EngineClassId,
    pub(crate) date_malformed_interval_string_exception: EngineClassId,
    pub(crate) date_malformed_period_string_exception: EngineClassId,
}

impl Throwables {
    /// Each class looked up once, at the engine start.
    pub(crate) fn start(start: &Start) -> Result<Throwables, Refusal> {
        Ok(Throwables {
            error: start.class::<Error>()?,
            date_error: start.class::<DateError>()?,
            date_object_error: start.class::<DateObjectError>()?,
            date_range_error: start.class::<DateRangeError>()?,
            date_exception: start.class::<DateException>()?,
            date_invalid_time_zone_exception: start.class::<DateInvalidTimeZoneException>()?,
            date_invalid_operation_exception: start.class::<DateInvalidOperationException>()?,
            date_malformed_string_exception: start.class::<DateMalformedStringException>()?,
            date_malformed_interval_string_exception: start
                .class::<DateMalformedIntervalStringException>()?,
            date_malformed_period_string_exception: start
                .class::<DateMalformedPeriodStringException>()?,
        })
    }

    /// What the engine start resolved, or the `InternalError` fatal
    /// where this engine started no `date`.
    pub(crate) fn of(req: &mut Request) -> Result<Throwables, Thrown> {
        match req.engine::<Throwables>() {
            Some(throwables) => Ok(*throwables),
            None => Err(raising::fatal(
                req,
                "InternalError: date's classes were not resolved at its engine start",
            )),
        }
    }
}
