//! `common/common_date.cpp` and the conversions `timezone_class.cpp`
//! keeps beside it: ext/date's zones made ICU's and back, and the time
//! zone argument every date class takes. ext/date's objects are read through its `php_date.h` surface
//! (design §4.10).

use php_ext::abi::{Instance, Union};
use php_ext::attr::php_class;
use php_ext::call::{instantiate, lookup_class};
use php_ext::raising::{Thrown, fatal, throw_chained, throw_error};
use php_ext::request::Request;
use php_ext::values::{PhpObject, PhpString, Value};
use php_ext_date::TzKind;

use crate::error::Outside;
use crate::ffi::cpp::Text;
use crate::ffi::timezone::{self as tz, TimeZone, Zone};
use crate::ffi::{self, Code, c_string, to_utf16};
use crate::timezone::{IntlTimeZone, TimeZoneObject};

/// ext/date's `DateTimeZone`, named for a parameter's type.
#[php_class(extern)]
pub(crate) struct DateTimeZone;

/// ext/date's `DateTime`, named for a parameter's and a return's type.
#[php_class(extern)]
pub(crate) struct DateTime;

/// `IntlTimeZone|DateTimeZone|string|null $timezone`, what
/// `Z_PARAM_OBJ_OR_STR_OR_NULL` takes for a time zone.
pub(crate) type ZoneArg =
    Option<Union<Instance<IntlTimeZone>, Union<Instance<DateTimeZone>, PhpString>>>;

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

/// `TimeZone::getGMT()->clone()`.
pub(crate) fn gmt() -> Option<Zone> {
    tz::php_intl_tz_special(1)
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

/// `timezone_throw_exception_with_call_location`: an `IntlException`
/// thrown as an error, its message prefixed with the native.
fn throw_located(req: &mut Request, message: &str) -> Thrown {
    let text = format!("{}(): {message}", req.active_function_name());
    throw_error(req, "IntlException", &text)
}

/// `timezone_process_timezone_argument`: the zone a time zone argument
/// names, the request's default zone for none; an `IntlTimeZone`
/// cloned, a `DateTimeZone` converted and a string looked up, or php's
/// `IntlException` where it names nothing.
pub(crate) fn process_timezone_argument(
    req: &mut Request,
    zone: ZoneArg,
    outside: &mut Outside,
) -> Result<Option<Zone>, Thrown> {
    let object = match zone {
        Some(Union::A(intl)) => Some(intl.object()),
        Some(Union::B(Union::A(date))) => Some(date.object()),
        _ => None,
    };
    // The union's object is sorted by what it holds, as php sorts it by
    // `instanceof_function`: an `IntlTimeZone` carries its payload.
    if let Some(object) = object {
        let Some(intl) = object.native::<TimeZoneObject>(req) else {
            let kind = php_ext_date::timezone_obj(req, object);
            return convert_datetimezone(req, kind.as_ref(), outside);
        };
        return match intl.zone.as_deref().map(|z| tz::php_intl_tz_clone(z)) {
            None => Err(throw_located(
                req,
                "passed IntlTimeZone is not properly constructed",
            )),
            Some(None) => Err(throw_located(req, "could not clone TimeZone")),
            Some(zone) => Ok(zone),
        };
    }
    let id = match zone {
        Some(Union::B(Union::B(id))) => id.as_bytes().to_vec(),
        _ => php_ext_date::get_timezone_info(req)?.name().to_vec(),
    };
    let Ok(units) = to_utf16(&id) else {
        let message = "Time zone identifier given is not a valid UTF-8 string";
        return Err(throw_located(req, message));
    };
    let Some(zone) = tz::php_intl_tz_create(Text::from(units.as_slice())) else {
        return Err(throw_located(req, "Could not create time zone"));
    };
    if tz::php_intl_tz_is_unknown(&zone) {
        let message = format!("No such time zone: \"{}\"", String::from_utf8_lossy(&id));
        return Err(throw_located(req, &message));
    }
    Ok(Some(zone))
}

/// The instant `Calendar::getNow()` answers.
pub(crate) fn now() -> f64 {
    ffi::timezone::php_intl_now()
}
