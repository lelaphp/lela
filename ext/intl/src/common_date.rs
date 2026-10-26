//! `common/common_date.cpp` and the conversions `timezone_class.cpp`
//! keeps beside it: ext/date's zones made ICU's and back, a
//! `DateTimeInterface` taken apart into its instant and zone, a value
//! read as milliseconds, and the time zone argument every date class
//! takes. ext/date's objects are read through its `php_date.h` surface
//! (design §4.10).

use php_ext::abi::{Instance, ObjectHandlers, Std, Union};
use php_ext::attr::php_class;
use php_ext::call::{call_method, instantiate, lookup_class};
use php_ext::coerce::{coerce_param, numeric_string, to_string};
use php_ext::decl::Type;
use php_ext::raising::{Thrown, fatal, throw_chained, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString, Value, ValueRef};
use php_ext_date::{TzKind, ZoneParts};

use crate::calendar::CalendarObject;
use crate::error::Outside;
use crate::ffi::calendar as cal;
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
    kind: Option<&ZoneParts>,
    outside: &mut Outside,
) -> Result<Option<Zone>, Thrown> {
    let id = match kind {
        Some(ZoneParts::Id(zone)) => zone.name().to_vec(),
        Some(ZoneParts::Abbr(abbr)) => abbr.clone(),
        Some(ZoneParts::Offset(seconds)) => {
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

/// `U_MILLIS_PER_SECOND`.
const MILLIS: f64 = 1000.0;

/// `instanceof_function(ce, <class named name>)`.
pub(crate) fn is_a(req: &mut Request, object: PhpObject, name: &[u8]) -> Result<bool, Thrown> {
    Ok(lookup_class(req, name)?.is_some_and(|class| object.instance_of(req, class)))
}

/// `intl_datetime_decompose`: a `DateTimeInterface`'s instant in
/// milliseconds, through its own `getTimestamp()`, and its zone where
/// `want_zone`; none where either fails, with the error set on `outside`.
pub(crate) fn datetime_decompose(
    req: &mut Request,
    object: PhpObject,
    want_zone: bool,
    outside: &mut Outside,
) -> Result<Option<(f64, Option<Zone>)>, Thrown> {
    if outside.code(req).is_failure() {
        return Ok(None);
    }
    let class = String::from_utf8_lossy(&Std.get_class_name(req, object)).into_owned();
    let stamp = call_method(req, &object, b"getTimestamp", &[])?;
    let Some(seconds) = stamp.as_int() else {
        stamp.give_up(req);
        let message = format!("{class}::getTimestamp() did not return an int");
        outside.set(req, Code::INTERNAL_PROGRAM, &message)?;
        return Ok(None);
    };
    stamp.give_up(req);
    let parts = php_ext_date::date_obj(req, object);
    let us = parts.as_ref().map_or(0, |p| p.us);
    #[allow(clippy::cast_precision_loss, reason = "php's (double) cast")]
    let millis = MILLIS * seconds as f64 + (us / 1000) as f64;
    if !want_zone {
        return Ok(Some((millis, None)));
    }
    let Some(parts) = parts else {
        let message = format!("the {class} object is not properly initialized");
        outside.set(req, Code::ILLEGAL_ARGUMENT, &message)?;
        return Ok(None);
    };
    let zone = match &parts.zone {
        None => gmt(),
        Some(kind) => convert_datetimezone(req, Some(kind), &mut Outside::Request)?,
    };
    if zone.is_none() {
        let message = "could not convert DateTime's time zone";
        outside.set(req, Code::ILLEGAL_ARGUMENT, message)?;
        return Ok(None);
    }
    Ok(Some((millis, zone)))
}

/// `intl_zval_to_millis`: a number of seconds, a numeric string, a
/// `DateTimeInterface` or an `IntlCalendar` as milliseconds; `NAN` where
/// the value is none of these, with the error set on `outside`.
pub(crate) fn zval_to_millis(
    req: &mut Request,
    value: Value,
    outside: &mut Outside,
) -> Result<f64, Thrown> {
    if outside.code(req).is_failure() {
        return Ok(f64::NAN);
    }
    #[allow(clippy::cast_precision_loss, reason = "php's (double) cast")]
    match value.deref().as_ref() {
        ValueRef::Str(text) => match numeric_string(text.as_bytes()) {
            Some(n) if n.as_int().is_some() => Ok(MILLIS * n.as_int().unwrap_or(0) as f64),
            Some(n) => Ok(n.as_float().unwrap_or(f64::NAN) * MILLIS),
            None => {
                let message = format!(
                    "string '{}' is not numeric, which would be required for it to be a valid date",
                    String::from_utf8_lossy(c_string(text.as_bytes()).as_bytes())
                );
                outside.set(req, Code::ILLEGAL_ARGUMENT, &message)?;
                Ok(f64::NAN)
            }
        },
        ValueRef::Int(n) => Ok(MILLIS * n as f64),
        ValueRef::Float(d) => Ok(MILLIS * d),
        ValueRef::Object(object) => {
            if is_a(req, object, b"DateTimeInterface")? {
                let parts = datetime_decompose(req, object, false, outside)?;
                return Ok(parts.map_or(f64::NAN, |(millis, _)| millis));
            }
            if object.native::<CalendarObject>(req).is_some() {
                return calendar_millis(req, object, outside);
            }
            let message = "invalid object type for date/time (only IntlCalendar and DateTimeInterface permitted)";
            outside.set(req, Code::ILLEGAL_ARGUMENT, message)?;
            Ok(f64::NAN)
        }
        _ => {
            outside.set(req, Code::ILLEGAL_ARGUMENT, "invalid PHP type for date")?;
            Ok(f64::NAN)
        }
    }
}

/// An `IntlCalendar`'s `getTime()`, through a clone of its calendar.
fn calendar_millis(
    req: &mut Request,
    object: PhpObject,
    outside: &mut Outside,
) -> Result<f64, Thrown> {
    let Some(mut copy) =
        crate::calendar::held(req, object).and_then(|c| cal::php_intl_cal_clone(c))
    else {
        let message = "IntlCalendar object is not properly constructed";
        outside.set(req, Code::ILLEGAL_ARGUMENT, message)?;
        return Ok(f64::NAN);
    };
    let mut status = Code::ZERO;
    let millis = cal::php_intl_cal_time(&mut copy, 0, 0.0, &mut status);
    if status.is_failure() {
        let message = "call to internal Calendar::getTime() has failed";
        outside.set(req, status, message)?;
    }
    Ok(millis)
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
) -> Result<Option<Owner<PhpObject>>, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
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
    let argument = PhpString::new(req, &id)?;
    let made = instantiate(req, class, &[Value::string(*argument)], &[]);
    argument.give_up(req);
    match made {
        Ok(object) => Ok(Some(object)),
        // `zend_throw_exception(IntlException_ce_ptr, ...)` over a fixed
        // string.
        Err(thrown) => {
            let message = "DateTimeZone constructor threw exception";
            Err(throw_chained(req, thrown, ce.intl_exception, message))
        }
    }
}

/// The time zone argument as `Z_PARAM_OBJ_OR_STR_OR_NULL` holds it:
/// any object, a string or null, a scalar read as a string.
pub(crate) fn zone_held(req: &mut Request, value: Value, position: u32) -> Result<Value, Thrown> {
    let ty = Type::Union(vec![Type::Object, Type::String, Type::Null]);
    coerce_param(req, &value, &ty, position)
}

/// A held time zone argument sorted as `timezone_process_timezone_argument`
/// sorts it, and handed to `then`: an `IntlTimeZone`, a `DateTimeZone`,
/// or a string, another object by its string cast, which is given up
/// once `then` is done with it.
pub(crate) fn zone_sorted<R>(
    req: &mut Request,
    value: Value,
    then: impl FnOnce(&mut Request, ZoneArg) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let arg = match value.as_ref() {
        ValueRef::Object(object) if object.native::<TimeZoneObject>(req).is_some() => {
            Some(Union::A(Instance::of(object)))
        }
        ValueRef::Object(object) if is_a(req, object, b"DateTimeZone")? => {
            Some(Union::B(Union::A(Instance::of(object))))
        }
        ValueRef::Object(_) => {
            let cast = to_string(req, &value)?;
            let answer = then(req, Some(Union::B(Union::B(*cast))));
            cast.give_up(req);
            return answer;
        }
        ValueRef::Str(text) => Some(Union::B(Union::B(text))),
        _ => None,
    };
    then(req, arg)
}

/// `timezone_throw_exception_with_call_location`: an `IntlException`
/// thrown as an error, its message prefixed with the native.
fn throw_located(req: &mut Request, message: &str) -> Thrown {
    let ce = match crate::throwables::Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    let text = format!("{}(): {message}", req.frames().active_function_name());
    throw_error(req, ce.intl_exception, &text)
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
