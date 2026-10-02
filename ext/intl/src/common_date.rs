//! `common/common_date.cpp` and the conversions `timezone_class.cpp`
//! keeps beside it: ext/date's zones made ICU's and back. ext/date's
//! objects are read through its `php_date.h` surface (design §4.10).

use php_ext::attr::php_class;
use php_ext::call::{instantiate, lookup_class};
use php_ext::raising::{Thrown, fatal, throw_chained};
use php_ext::request::Request;
use php_ext::values::{PhpObject, PhpString, Value};
use php_ext_date::TzKind;

use crate::error::Outside;
use crate::ffi::timezone::{self as tz, TimeZone, Zone};
use crate::ffi::{self, Code, c_string};

/// ext/date's `DateTimeZone`, named for a parameter's type.
#[php_class(extern)]
pub(crate) struct DateTimeZone;

/// `timezone_convert_datetimezone`: the ICU zone of ext/date's, by its
/// id, its offset as `GMT±hh:mm` or its abbreviation; none where ICU
/// knows no such zone, with the error set on `outside`.
pub(crate) fn convert_datetimezone(
    req: &mut Request,
    kind: Option<&TzKind>,
    outside: &mut Outside,
) -> Result<Option<Zone>, Thrown> {
    let id = match kind {
        Some(TzKind::Id(zone)) => zone.name().to_vec(),
        Some(TzKind::Abbr { abbr, .. }) => abbr.clone(),
        Some(TzKind::Offset(seconds)) => {
            #[allow(clippy::cast_possible_truncation, reason = "php's (int) cast")]
            let mins = (*seconds as i32) / 60;
            let hours = mins / 60;
            let minutes = (mins - hours * 60).abs();
            if mins <= -24 * 60 || mins >= 24 * 60 {
                let message = "object has an time zone offset that's too large";
                outside.set(req, Code::ILLEGAL_ARGUMENT, message)?;
                return Ok(None);
            }
            format!("GMT{hours:+03}:{minutes:02}").into_bytes()
        }
        None => b"(null)".to_vec(),
    };
    let name = c_string(if kind.is_some() { &id } else { b"" });
    let zone = tz::php_intl_tz_create_invariant(name.as_c_str().into());
    match zone {
        Some(zone) if !tz::php_intl_tz_is_unknown(&zone) => Ok(Some(zone)),
        _ => {
            let message = format!(
                "time zone id '{}' extracted from ext/date DateTimeZone not recognized",
                String::from_utf8_lossy(&id)
            );
            outside.set(req, Code::ILLEGAL_ARGUMENT, &message)?;
            Ok(None)
        }
    }
}

/// `timezone_convert_to_datetimezone`: ext/date's `DateTimeZone` for an
/// ICU zone, an offset zone laid down by hand for a `GMT` id, which the
/// constructor would refuse, and the id through the constructor
/// otherwise; none where the id cannot be had, with the error set on
/// `outside`.
pub(crate) fn convert_to_datetimezone(
    req: &mut Request,
    zone: &TimeZone,
    outside: &mut Outside,
) -> Result<Option<PhpObject>, Thrown> {
    let Some(id) = tz::php_intl_tz_id(zone) else {
        outside.set(req, Code::ILLEGAL_ARGUMENT, "could not obtain TimeZone id")?;
        return Ok(None);
    };
    if id.units().starts_with(&[0x47, 0x4D, 0x54]) {
        let raw = i64::from(tz::php_intl_tz_int(zone, 0) / 1000);
        return php_ext_date::new_timezone(req, TzKind::Offset(raw)).map(Some);
    }
    let id = match id.to_utf8() {
        Ok(id) => id,
        Err(code) => {
            outside.set(req, code, "could not convert id to UTF-8")?;
            return Ok(None);
        }
    };
    let Some(class) = lookup_class(req, b"DateTimeZone")? else {
        return Err(fatal(req, "InternalError: DateTimeZone is not linked"));
    };
    let argument = Value::string(PhpString::new(req, &id)?);
    let made = instantiate(req, class, &[argument]);
    req.release(argument);
    match made {
        Ok(object) => Ok(Some(object)),
        Err(thrown) => {
            let message = "DateTimeZone constructor threw exception";
            Err(throw_chained(req, thrown, "IntlException", message))
        }
    }
}

/// The instant `Calendar::getNow()` answers.
pub(crate) fn now() -> f64 {
    ffi::timezone::php_intl_now()
}
