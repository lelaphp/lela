//! `dateformat/`: `IntlDateFormatter` and the `datefmt_*` functions its
//! methods alias, over ICU's `udat_*` where php calls the C API and the
//! shim ([`crate::ffi::dateformat`]) where php reads the format as an
//! `icu::DateFormat`. Each method shares one body with the function it
//! aliases, handed the object.

use std::ffi::CString;

use php_ext::abi::{
    CloneObj, ExtensionBuilder, Instance, ObjectHandlers, OrFalse, Raw, Ref, This, Union, Untyped,
};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{clone_with, lookup_class, new_object};
use php_ext::coerce::{self, to_long, value_name, with_string};
use php_ext::raising::{Thrown, argument_error, arity, fatal, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value, ValueRef, walk};

use crate::calendar::{self, IntlCalendar};
use crate::common_date::{
    ZoneArg, datetime_decompose, is_a, process_timezone_argument, zone_held, zone_sorted,
    zval_to_millis,
};
use crate::error::{self, Carrier, IntlError, Outside, or_false};
use crate::ffi::calendar::{self as cal, Cal};
use crate::ffi::cpp::CName;
use crate::ffi::dateformat::{self as df, DateFormat};
use crate::ffi::formatter::iso3_language;
use crate::ffi::timezone as tz;
use crate::ffi::{self, Code, c_string};
use crate::timezone::{self, IntlTimeZone};

/// A native's answer where php may `RETURN_FALSE`.
type Answer<T> = Result<OrFalse<T>, Thrown>;
/// `IntlCalendar|int|null $calendar`.
type CalArg = Option<Union<Instance<IntlCalendar>, i64>>;
/// The formatter a function takes first.
type Fmt = Instance<IntlDateFormatter>;

/// `UDAT_PATTERN`.
const PATTERN: i64 = -2;
/// `UCAL_GREGORIAN`.
const GREGORIAN: i64 = 1;
/// `UCAL_TRADITIONAL`.
const TRADITIONAL: i64 = 0;

/// `IntlDateFormatter_object` past its `zend_object`: the error, the
/// format, the styles and the calendar it was made with (`-1` for an
/// `IntlCalendar`), and the locale it was asked for, which keeps the
/// modifiers the format's own locale drops.
pub(crate) struct DateFormatObject {
    error: IntlError,
    udatf: Option<DateFormat>,
    date_type: i32,
    time_type: i32,
    calendar: i64,
    requested_locale: Option<Box<CString>>,
}

impl Default for DateFormatObject {
    /// `IntlDateFormatter_object_create`.
    fn default() -> Self {
        DateFormatObject {
            error: IntlError::default(),
            udatf: None,
            date_type: 0,
            time_type: 0,
            calendar: -1,
            requested_locale: None,
        }
    }
}

impl Carrier for DateFormatObject {
    fn error(&mut self) -> &mut IntlError {
        &mut self.error
    }
}

/// `IntlDateFormatter_handlers`: `std_object_handlers` with php's clone.
pub(crate) struct Handlers;
pub(crate) static HANDLERS: Handlers = Handlers;

impl ObjectHandlers for Handlers {
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_formatter)
    }
}

/// `IntlDateFormatter_object_clone`: a fresh object's members, the
/// styles and the calendar type left at their defaults as php leaves
/// them, and `udat_clone`; php's `Error` once the copy is made.
fn clone_formatter(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let mut status = Code::ZERO;
    let copy = object
        .native::<DateFormatObject>(req)
        .and_then(|o| o.udatf.as_ref())
        .map(|udatf| udatf.try_clone(&mut status));
    let held = copy.is_some();
    let payload = DateFormatObject {
        udatf: copy.flatten(),
        ..DateFormatObject::default()
    };
    let new = clone_with(req, &object, payload)?;
    let message = match held {
        false => "Cannot clone uninitialized IntlDateFormatter",
        true if status.is_failure() => "Failed to clone IntlDateFormatter",
        true => return Ok(new),
    };
    new.give_up(req);
    Err(throw_error(req, "Error", message))
}

/// `DATE_FORMAT_METHOD_FETCH_OBJECT`: the object's error reset, the
/// request's having been reset before the arguments
/// (`DATE_FORMAT_METHOD_INIT_VARS`, `before_args`), and php's `Error`
/// for an object no constructor finished.
fn fetch(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    error::reset_object::<DateFormatObject>(req, object);
    let held = object
        .native::<DateFormatObject>(req)
        .is_some_and(|o| o.udatf.is_some());
    if !held {
        let message = "Found unconstructed IntlDateFormatter";
        return Err(throw_error(req, "Error", message));
    }
    Ok(())
}

/// [`fetch`] and `body` over the format, taken out of the object for
/// the call and put back after.
fn with<T>(
    req: &mut Request,
    object: PhpObject,
    body: impl FnOnce(&mut Request, &mut DateFormat) -> Result<T, Thrown>,
) -> Result<T, Thrown> {
    fetch(req, object)?;
    let taken = object
        .native_mut::<DateFormatObject>(req)
        .and_then(|o| o.udatf.take());
    let Some(mut udatf) = taken else {
        let message = "Found unconstructed IntlDateFormatter";
        return Err(throw_error(req, "Error", message));
    };
    let answer = body(req, &mut udatf);
    if let Some(o) = object.native_mut::<DateFormatObject>(req) {
        o.udatf = Some(udatf);
    }
    answer
}

/// The object's payload, read.
fn payload(req: &Request, object: PhpObject) -> Option<&DateFormatObject> {
    object.native::<DateFormatObject>(req)
}

/// `INTL_METHOD_CHECK_STATUS` after ICU wrote `code` into the object's
/// error.
fn check(
    req: &mut Request,
    object: PhpObject,
    code: Code,
    message: &str,
) -> Result<(), Option<Thrown>> {
    error::errors_set_code::<DateFormatObject>(req, object, code);
    error::check_object::<DateFormatObject>(req, object, message)
}

/// `INTL_METHOD_RETVAL_UTF8`: the UTF-16 converted, a failure answered
/// `false`.
fn utf8(req: &mut Request, object: PhpObject, units: &[u16]) -> Answer<Owner<PhpString>> {
    let converted = ffi::to_utf8(units);
    let code = converted.as_ref().err().copied().unwrap_or(Code::ZERO);
    if let Err(stop) = check(req, object, code, "Error converting value to UTF-8") {
        return or_false(Err(stop));
    }
    let bytes = converted.unwrap_or_default();
    PhpString::new(req, &bytes).map(OrFalse::Value)
}

/// `INTL_UDATE_FMT_OK`.
fn style_ok(style: i64) -> bool {
    matches!(style, -2..=3 | 128..=131)
}

/// The `$timezone` argument as `Z_PARAM_OBJ_OR_STR_OR_NULL` holds it.
type ZoneRaw = Raw<ZoneArg>;

/// What `datefmt_ctor`'s ZPP holds: the locale, the two styles, the
/// zone as given, the calendar and the pattern.
#[derive(Clone, Copy)]
struct Args {
    locale: Option<PhpString>,
    date_type: i64,
    time_type: i64,
    zone: Value,
    calendar: CalArg,
    pattern: Option<PhpString>,
}

/// The constructor's six parameters, as each entry declares them.
struct RawArgs<C> {
    locale: Raw<Option<PhpString>>,
    date_type: Raw<i64>,
    time_type: Raw<i64>,
    zone: ZoneRaw,
    calendar: Raw<C>,
    pattern: Raw<Option<PhpString>>,
}

impl<C: php_ext::abi::FromArgument> RawArgs<C> {
    /// `ZEND_PARSE_PARAMETERS_START(1, 6)`, the request's error reset
    /// before it (`before_args`).
    fn hold(&self, req: &mut Request, calendar: impl FnOnce(C) -> CalArg) -> Result<Args, Thrown> {
        arity(req, 1, Some(6))?;
        let locale = self.locale.hold(req)?;
        let date_type = self.date_type.hold(req)?;
        let time_type = self.time_type.hold(req)?;
        let zone = zone_held(req, self.zone.value(), 4)?;
        let calendar = calendar(self.calendar.hold(req)?);
        let pattern = self.pattern.hold(req)?;
        Ok(Args {
            locale,
            date_type,
            time_type,
            zone,
            calendar,
            pattern,
        })
    }
}

/// `datefmt_process_calendar_arg`: the calendar a formatter takes, its
/// type, and whether it is the formatter's own (`calendar_owned`); none
/// where it cannot be had, with the object's error set.
fn calendar_arg(
    req: &mut Request,
    object: PhpObject,
    calendar: CalArg,
    locale: &CString,
) -> Result<Option<(Cal, i64, bool)>, Thrown> {
    let mut status = Code::ZERO;
    let (made, kind, owned) = match calendar {
        None => (
            new_calendar(GREGORIAN, locale, &mut status),
            GREGORIAN,
            true,
        ),
        Some(Union::B(kind)) if kind == TRADITIONAL || kind == GREGORIAN => {
            (new_calendar(kind, locale, &mut status), kind, true)
        }
        Some(Union::B(_)) => {
            let message = "Invalid value for calendar type; it must be one of IntlDateFormatter::TRADITIONAL (locale's default calendar) or IntlDateFormatter::GREGORIAN. Alternatively, it can be an IntlCalendar object";
            error::errors_set::<DateFormatObject>(req, object, Code::ILLEGAL_ARGUMENT, message)?;
            return Ok(None);
        }
        Some(Union::A(given)) => {
            let Some(held) = calendar::held(req, given.object()) else {
                let message = "Found unconstructed IntlCalendar object";
                error::errors_set::<DateFormatObject>(
                    req,
                    object,
                    Code::ILLEGAL_ARGUMENT,
                    message,
                )?;
                return Ok(None);
            };
            (cal::php_intl_cal_clone(held), -1, false)
        }
    };
    match made {
        Some(made) if !status.is_failure() => Ok(Some((made, kind, owned))),
        _ => {
            let message = "Failure instantiating calendar";
            error::errors_set::<DateFormatObject>(req, object, Code::ILLEGAL_ARGUMENT, message)?;
            Ok(None)
        }
    }
}

/// `Calendar::createInstance(locale)` for `TRADITIONAL` and `new
/// GregorianCalendar(locale)` otherwise.
fn new_calendar(kind: i64, locale: &CString, status: &mut Code) -> Option<Cal> {
    let traditional = i32::from(kind == TRADITIONAL);
    df::php_intl_datefmt_calendar_new(traditional, CName::from(locale.as_c_str()), status)
}

/// `datefmt_ctor`'s checks of the styles and the locale: the locale
/// canonicalized, or none where the request's error is set.
fn styles_and_locale(req: &mut Request, a: &Args) -> Result<Option<CString>, Thrown> {
    let refused = if !style_ok(a.date_type) {
        "invalid date format style"
    } else if !style_ok(a.time_type) {
        "invalid time format style"
    } else if a.date_type == PATTERN && a.time_type != PATTERN {
        "datefmt_create: time format must be IntlDateFormatter::PATTERN if date format is IntlDateFormatter::PATTERN"
    } else {
        ""
    };
    if !refused.is_empty() {
        error::set(req, Code::ILLEGAL_ARGUMENT, refused)?;
        return Ok(None);
    }
    let given = a.locale.as_ref().map_or(&b""[..], PhpString::as_bytes);
    if crate::formatter::locale_too_long(req, given.len())? {
        return Ok(None);
    }
    let asked = if given.is_empty() {
        crate::default_locale(req)
    } else {
        given.to_vec()
    };
    let asked = c_string(&asked);
    let locale =
        ffi::canonicalize(asked.to_bytes()).map_or_else(|| asked.clone(), |c| c_string(&c));
    let short = given.len() == 1 && given[0] != b'C';
    if short || (given.len() > 1 && iso3_language(&locale).is_empty()) {
        let tail = format!(
            "\"{}\" is invalid",
            String::from_utf8_lossy(asked.to_bytes())
        );
        return Err(argument_error(req, "ValueError", 1, &tail));
    }
    Ok(Some(locale))
}

/// `datefmt_ctor`: `Ok(false)` for php's `FAILURE` where nothing was
/// thrown, which is where the request's code is a failure.
fn construct(req: &mut Request, object: PhpObject, a: Args) -> Result<bool, Thrown> {
    if payload(req, object).is_some_and(|o| o.udatf.is_some()) {
        let message = "cannot call constructor twice";
        error::errors_set::<DateFormatObject>(req, object, Code::ILLEGAL_ARGUMENT, message)?;
        return Ok(false);
    }
    let Some(locale) = styles_and_locale(req, &a)? else {
        return Ok(false);
    };
    let Some((calendar, kind, owned)) = calendar_arg(req, object, a.calendar, &locale)? else {
        return Ok(failed(req));
    };
    let mut zone = None;
    if !matches!(a.zone.as_ref(), ValueRef::Null) || owned {
        zone = zone_sorted(req, a.zone, |req, arg| {
            process_timezone_argument(req, arg, &mut Outside::of::<DateFormatObject>(object))
        })?;
        if zone.is_none() {
            return Ok(failed(req));
        }
    }
    let mut units = None;
    if let Some(pattern) = a.pattern.as_ref().filter(|p| !p.is_empty()) {
        match ffi::to_utf16(pattern.as_bytes()) {
            Ok(u) => units = Some(u),
            Err(code) => {
                set_code(req, object, code);
                error::set(req, code, "error converting pattern to UTF-16")?;
                return Ok(failed(req));
            }
        }
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "php's (UDateFormatStyle) cast"
    )]
    let (time_style, date_style) = (a.time_type as i32, a.date_type as i32);
    let mut status = Code::ZERO;
    let mut udatf = df::open(
        time_style,
        date_style,
        &locale,
        units.as_deref(),
        &mut status,
    );
    set_code(req, object, status);
    if let (Some(units), Some(udatf)) = (&units, udatf.as_mut()) {
        udatf.apply_pattern(true, units);
    }
    if status.is_failure() || udatf.is_none() {
        let message = if units.is_some() {
            "error applying pattern"
        } else {
            "date formatter creation failed"
        };
        error::set(req, status, message)?;
        return Ok(failed(req));
    }
    if let Some(udatf) = udatf.as_mut() {
        df::php_intl_datefmt_adopt_calendar(udatf, calendar);
        if let Some(zone) = zone {
            df::php_intl_datefmt_adopt_zone(udatf, zone);
        }
    }
    if let Some(o) = object.native_mut::<DateFormatObject>(req) {
        #[allow(clippy::cast_possible_truncation, reason = "php's int members")]
        {
            o.date_type = a.date_type as i32;
            o.time_type = a.time_type as i32;
        }
        o.calendar = kind;
        o.requested_locale = Some(Box::new(locale));
        o.udatf = udatf;
    }
    Ok(failed(req))
}

/// `return U_FAILURE(intl_error_get_code(NULL)) ? FAILURE : SUCCESS;`.
fn failed(req: &mut Request) -> bool {
    !error::code(req).is_failure()
}

/// The object's code alone, as php writes ICU's status into it.
fn set_code(req: &mut Request, object: PhpObject, code: Code) {
    if let Some(o) = object.native_mut::<DateFormatObject>(req) {
        o.error.code = code;
    }
}

/// `datefmt_create`: a fresh `IntlDateFormatter`, or null where its
/// constructor failed.
fn create(req: &mut Request, a: Args) -> Result<Option<Owner<Fmt>>, Thrown> {
    let Some(class) = lookup_class(req, b"IntlDateFormatter")? else {
        return Err(fatal(req, "InternalError: IntlDateFormatter is not linked"));
    };
    let object = new_object(req, class)?;
    match construct(req, *object, a) {
        Ok(true) => Ok(Some(object.into_instance())),
        Ok(false) => {
            object.give_up(req);
            Ok(None)
        }
        Err(thrown) => {
            object.give_up(req);
            Err(thrown)
        }
    }
}

/// `datefmt_get_datetype` (`date`) and `datefmt_get_timetype`.
fn get_type(req: &mut Request, object: PhpObject, date: bool) -> Result<i64, Thrown> {
    fetch(req, object)?;
    let o = payload(req, object);
    let style = o.map_or(0, |o| if date { o.date_type } else { o.time_type });
    Ok(i64::from(style))
}

/// `datefmt_get_calendar`: the type the formatter was made with, `false`
/// for an `IntlCalendar`.
fn get_calendar(req: &mut Request, object: PhpObject) -> Answer<i64> {
    fetch(req, object)?;
    Ok(match payload(req, object).map_or(-1, |o| o.calendar) {
        -1 => OrFalse::False,
        kind => OrFalse::Value(kind),
    })
}

/// `datefmt_set_calendar`: a calendar of the requested locale's, the
/// formatter's zone kept; or a clone of the `IntlCalendar` given.
fn set_calendar(req: &mut Request, object: PhpObject, calendar: CalArg) -> Result<bool, Thrown> {
    with(req, object, |req, udatf| {
        let locale = payload(req, object)
            .and_then(|o| o.requested_locale.as_deref().cloned())
            .unwrap_or_else(|| c_string(ffi::default_locale().as_bytes()));
        let Some((mut made, kind, owned)) = calendar_arg(req, object, calendar, &locale)? else {
            return Ok(false);
        };
        if owned {
            let Some(zone) = df::php_intl_datefmt_zone(udatf) else {
                let message = "Out of memory when cloning calendar";
                error::errors_set::<DateFormatObject>(
                    req,
                    object,
                    Code::MEMORY_ALLOCATION,
                    message,
                )?;
                return Ok(false);
            };
            cal::php_intl_cal_adopt_zone(&mut made, zone);
        }
        df::php_intl_datefmt_adopt_calendar(udatf, made);
        if let Some(o) = object.native_mut::<DateFormatObject>(req) {
            o.calendar = kind;
        }
        Ok(true)
    })
}

/// `datefmt_get_timezone_id`.
fn get_timezone_id(req: &mut Request, object: PhpObject) -> Answer<Owner<PhpString>> {
    with(req, object, |req, udatf| {
        let id = df::php_intl_datefmt_zone(udatf).and_then(|zone| tz::php_intl_tz_id(&zone));
        let converted = id.map_or(Ok(Vec::new()), |id| id.to_utf8());
        let code = converted.as_ref().err().copied().unwrap_or(Code::ZERO);
        let message = "Could not convert time zone id to UTF-8";
        if let Err(stop) = check(req, object, code, message) {
            return or_false(Err(stop));
        }
        let bytes = converted.unwrap_or_default();
        PhpString::new(req, &bytes).map(OrFalse::Value)
    })
}

/// `datefmt_get_timezone`: a new `IntlTimeZone` holding a clone of the
/// formatter's zone.
fn get_timezone(req: &mut Request, object: PhpObject) -> Answer<Owner<Instance<IntlTimeZone>>> {
    let zone = with(req, object, |_, udatf| Ok(df::php_intl_datefmt_zone(udatf)))?;
    let Some(zone) = zone else {
        let message = "Out of memory when cloning time zone";
        error::errors_set::<DateFormatObject>(req, object, Code::MEMORY_ALLOCATION, message)?;
        return Ok(OrFalse::False);
    };
    timezone::construct(req, zone).map(OrFalse::Value)
}

/// `datefmt_set_timezone` and `IntlDateFormatter::setTimeZone`.
fn set_timezone(req: &mut Request, object: PhpObject, zone: Value) -> Result<bool, Thrown> {
    with(req, object, |req, udatf| {
        let mut outside = Outside::of::<DateFormatObject>(object);
        let processed = zone_sorted(req, zone, |req, arg| {
            process_timezone_argument(req, arg, &mut outside)
        });
        let Some(zone) = processed? else {
            return Ok(false);
        };
        df::php_intl_datefmt_adopt_zone(udatf, zone);
        Ok(true)
    })
}

/// `datefmt_get_calendar_object`: a new `IntlCalendar` holding a clone
/// of the formatter's calendar.
fn get_calendar_object(
    req: &mut Request,
    object: PhpObject,
) -> Result<Option<OrFalse<Owner<Instance<IntlCalendar>>>>, Thrown> {
    let made = with(req, object, |_, udatf| {
        Ok(df::php_intl_datefmt_calendar(udatf))
    })?;
    match made {
        Some(made) => calendar::made(req, made).map(|c| Some(OrFalse::Value(c))),
        None => Ok(None),
    }
}

/// `datefmt_set_pattern`: applied unlocalized.
fn set_pattern(req: &mut Request, object: PhpObject, pattern: PhpString) -> Answer<bool> {
    with(req, object, |req, udatf| {
        let converted = ffi::to_utf16(pattern.as_bytes());
        let code = converted.as_ref().err().copied().unwrap_or(Code::ZERO);
        if let Err(stop) = check(req, object, code, "Error converting pattern to UTF-16") {
            return or_false(Err(stop));
        }
        udatf.apply_pattern(false, &converted.unwrap_or_default());
        or_false(check(req, object, code, "Error setting symbol value").map(|()| true))
    })
}

/// `datefmt_get_pattern`: `udat_toPattern` into sixty-four units, and on
/// an overflow the full length and one more.
fn get_pattern(req: &mut Request, object: PhpObject) -> Answer<Owner<PhpString>> {
    with(req, object, |req, udatf| {
        let mut buffer = vec![0u16; 64];
        let mut status = Code::ZERO;
        let mut len = udatf.pattern(&mut buffer, &mut status);
        let needed = usize::try_from(len).unwrap_or(0);
        if status == Code::BUFFER_OVERFLOW && needed >= 64 {
            status = Code::ZERO;
            buffer = vec![0u16; needed + 1];
            len = udatf.pattern(&mut buffer, &mut status);
        }
        if let Err(stop) = check(req, object, status, "Error getting formatter pattern") {
            return or_false(Err(stop));
        }
        buffer.truncate(usize::try_from(len).unwrap_or(0));
        utf8(req, object, &buffer)
    })
}

/// `datefmt_get_locale`.
fn get_locale(req: &mut Request, object: PhpObject, kind: i64) -> Answer<Owner<PhpString>> {
    with(req, object, |req, udatf| {
        let mut status = Code::ZERO;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "php's (ULocDataLocaleType) cast"
        )]
        let locale = udatf.locale(kind as i32, &mut status).unwrap_or("");
        if let Err(stop) = check(req, object, status, "Error getting locale") {
            return or_false(Err(stop));
        }
        PhpString::new(req, locale.as_bytes()).map(OrFalse::Value)
    })
}

/// `datefmt_is_lenient`.
fn is_lenient(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    with(req, object, |_, udatf| Ok(udatf.is_lenient()))
}

/// `datefmt_set_lenient`.
fn set_lenient(req: &mut Request, object: PhpObject, lenient: bool) -> Result<(), Thrown> {
    with(req, object, |_, udatf| {
        udatf.set_lenient(lenient);
        Ok(())
    })
}

/// The keys `internal_get_timestamp` reads, in its order: the year since
/// 1900, the month, the hour, the minute, the second and the day.
const TM_KEYS: [&str; 6] = [
    "tm_year", "tm_mon", "tm_hour", "tm_min", "tm_sec", "tm_mday",
];

/// `internal_get_arr_ele`: the integer at `key`, zero where it is
/// absent; a value that is no integer, or one past 32 bits, sets the
/// object's error, and nothing is read once it is set. What the error
/// throws is left in `pending`, as php's C runs on past it.
fn array_field(
    req: &mut Request,
    object: PhpObject,
    array: PhpArray,
    key: &str,
    pending: &mut Option<Thrown>,
) -> i32 {
    if error::object_code::<DateFormatObject>(req, object).is_failure() {
        return 0;
    }
    let Some(value) = array.find(key.as_bytes()).map(|v| v.deref()) else {
        return 0;
    };
    let message = match value.as_ref() {
        ValueRef::Int(n) => match i32::try_from(n) {
            Ok(n) => return n,
            Err(_) => format!("value {n} is out of bounds for a 32-bit integer in key '{key}'"),
        },
        _ => format!("parameter array contains a non-integer element for key '{key}'"),
    };
    raise_over(req, object, pending, Code::ILLEGAL_ARGUMENT, &message);
    0
}

/// `intl_errors_set` on the object over what `pending` holds, its
/// `IntlException` chained to it, as `zend_throw_exception` chains a
/// throw to the one pending; what is pending after is left there.
///
/// ```php
/// ini_set('intl.use_exceptions', 1);
/// $f = new IntlDateFormatter('en_US', IntlDateFormatter::SHORT, IntlDateFormatter::SHORT, 'UTC');
/// try { $f->format(['tm_year' => 'x', 'tm_mon' => 1.5, 'tm_hour' => PHP_INT_MAX]); } catch (IntlException $e) {
///     for (; $e; $e = $e->getPrevious()) echo get_class($e), ': ', $e->getMessage(), "\n";
/// }
/// ini_set('intl.use_exceptions', 0);
/// var_dump($f->format(['tm_mday' => 2 ** 40]), intl_get_error_message(), $f->getErrorMessage());
/// var_dump($f->format(['tm_year' => 100, 'tm_mday' => 2]), intl_get_error_code());
/// ```
/// ```expect
/// IntlException: IntlDateFormatter::format(): date formatting failed
/// IntlException: IntlDateFormatter::format(): error cloning calendar
/// IntlException: IntlDateFormatter::format(): parameter array contains a non-integer element for key 'tm_year'
/// bool(false)
/// string(77) "IntlDateFormatter::format(): date formatting failed: U_ILLEGAL_ARGUMENT_ERROR"
/// string(77) "IntlDateFormatter::format(): date formatting failed: U_ILLEGAL_ARGUMENT_ERROR"
/// string(18) "1/2/00, 12:00 AM"
/// int(0)
/// ```
fn raise_over(
    req: &mut Request,
    object: PhpObject,
    pending: &mut Option<Thrown>,
    code: Code,
    message: &str,
) {
    let mut outside = Outside::of::<DateFormatObject>(object);
    *pending = outside.set_over(req, pending.take(), code, message).err();
}

/// `internal_get_timestamp`: the array's date and time, set on a clone
/// of the formatter's calendar, as milliseconds; the object's error set
/// where that fails, which a failure of an element's already is, so
/// `ucal_clone` refuses the code it is handed and php raises again.
fn array_millis(
    req: &mut Request,
    object: PhpObject,
    array: PhpArray,
    pending: &mut Option<Thrown>,
) -> f64 {
    let mut f = [0i32; 6];
    for (slot, key) in f.iter_mut().zip(TM_KEYS) {
        *slot = array_field(req, object, array, key, pending);
    }
    let fields = [f[0].wrapping_add(1900), f[1], f[5], f[2], f[3], f[4]];
    let mut code = error::object_code::<DateFormatObject>(req, object);
    let mut millis = None;
    if !code.is_failure() {
        let held = object
            .native::<DateFormatObject>(req)
            .and_then(|o| o.udatf.as_ref());
        millis = held.and_then(|udatf| udatf.timestamp(fields, &mut code));
    }
    let Some(millis) = millis else {
        raise_over(req, object, pending, code, "error cloning calendar");
        return 0.0;
    };
    set_code(req, object, code);
    millis
}

/// `datefmt_format`: an array of `localtime()`'s fields, or any value
/// `intl_zval_to_millis` reads, formatted.
fn format(req: &mut Request, object: PhpObject, datetime: Value) -> Answer<Owner<PhpString>> {
    fetch(req, object)?;
    let datetime = datetime.deref();
    let millis = if let ValueRef::Array(array) = datetime.as_ref() {
        if array.is_empty() {
            return Ok(OrFalse::False);
        }
        let mut pending = None;
        let millis = array_millis(req, object, array, &mut pending);
        // `INTL_METHOD_CHECK_STATUS`, raised over what the array threw.
        let code = error::object_code::<DateFormatObject>(req, object);
        error::set_code(req, code);
        if code.is_failure() {
            raise_over(req, object, &mut pending, code, "date formatting failed");
            return pending.map_or(Ok(OrFalse::False), Err);
        }
        if let Some(thrown) = pending {
            return Err(thrown);
        }
        millis
    } else {
        let millis = zval_to_millis(req, datetime, &mut Outside::of::<DateFormatObject>(object))?;
        if error::object_code::<DateFormatObject>(req, object).is_failure() {
            return Ok(OrFalse::False);
        }
        millis
    };
    with_held(req, object, |req, udatf| {
        let mut status = Code::ZERO;
        let needed = udatf.format(millis, &mut [], &mut status);
        let mut buffer = Vec::new();
        if status == Code::BUFFER_OVERFLOW {
            status = Code::ZERO;
            buffer = vec![0u16; usize::try_from(needed).unwrap_or(0)];
            udatf.format(millis, &mut buffer, &mut status);
        }
        if let Err(stop) = check(req, object, status, "Date formatting failed") {
            return or_false(Err(stop));
        }
        utf8(req, object, &buffer)
    })
}

/// `body` over the format of an object [`fetch`] already read, taken
/// out for the call and put back after.
fn with_held<T>(
    req: &mut Request,
    object: PhpObject,
    body: impl FnOnce(&mut Request, &mut DateFormat) -> Answer<T>,
) -> Answer<T> {
    let taken = object
        .native_mut::<DateFormatObject>(req)
        .and_then(|o| o.udatf.take());
    let Some(mut udatf) = taken else {
        let message = "Found unconstructed IntlDateFormatter";
        return Err(throw_error(req, "Error", message));
    };
    let answer = body(req, &mut udatf);
    if let Some(o) = object.native_mut::<DateFormatObject>(req) {
        o.udatf = Some(udatf);
    }
    answer
}

/// `valid_styles[]`: `DateFormat::EStyle`'s styles, the relative ones
/// among them.
fn valid_style(value: &Value) -> Option<i32> {
    match value.as_ref() {
        ValueRef::Int(n @ (-1..=3 | 128..=131)) => i32::try_from(n).ok(),
        _ => None,
    }
}

/// `DateFormat::kDefault`.
const DEFAULT_STYLE: i32 = 2;

/// `datefmt_format_object`'s `$format`: the date's and the time's
/// styles, the time's relative bit dropped, and the pattern; none where
/// the request's error is set.
#[allow(clippy::type_complexity)]
fn format_styles(
    req: &mut Request,
    format: Value,
) -> Result<Option<(i32, i32, Option<CString>)>, Thrown> {
    let (mut date_style, mut time_style) = (DEFAULT_STYLE, DEFAULT_STYLE);
    let mut pattern = None;
    match format.deref().as_ref() {
        ValueRef::Null => {}
        ValueRef::Array(array) => {
            if array.len() != 2 {
                let message = "bad format; if array, it must have two elements";
                error::set(req, Code::ILLEGAL_ARGUMENT, message)?;
                return Ok(None);
            }
            let styles: Vec<Option<i32>> = walk::rows(req, array)
                .map(|(_, v)| valid_style(&v.deref()))
                .collect();
            match (
                styles.first().copied().flatten(),
                styles.get(1).copied().flatten(),
            ) {
                (None, _) => {
                    let message =
                        "bad format; the date format (first element of the array) is not valid";
                    error::set(req, Code::ILLEGAL_ARGUMENT, message)?;
                    return Ok(None);
                }
                (_, None) => {
                    let message =
                        "bad format; the time format (second element of the array) is not valid";
                    error::set(req, Code::ILLEGAL_ARGUMENT, message)?;
                    return Ok(None);
                }
                (Some(d), Some(t)) => (date_style, time_style) = (d, t),
            }
        }
        ValueRef::Int(_) => {
            let Some(style) = valid_style(&format.deref()) else {
                let message = "the date/time format type is invalid";
                error::set(req, Code::ILLEGAL_ARGUMENT, message)?;
                return Ok(None);
            };
            (date_style, time_style) = (style, style);
        }
        _ => {
            let made = with_string(req, &format, |text| {
                (!text.is_empty()).then(|| c_string(text))
            })?;
            let Some(made) = made else {
                error::set(req, Code::ILLEGAL_ARGUMENT, "the format is empty")?;
                return Ok(None);
            };
            pattern = Some(made);
        }
    }
    if time_style != -1 {
        time_style &= !128;
    }
    Ok(Some((date_style, time_style, pattern)))
}

/// `datefmt_format_object`: an `IntlCalendar` or a `DateTimeInterface`
/// formatted once in the locale, by a pattern, a style for both parts or
/// an array of the date's and the time's.
fn format_object(
    req: &mut Request,
    object: PhpObject,
    format: Value,
    locale: Option<PhpString>,
) -> Answer<Owner<PhpString>> {
    let locale = match locale {
        Some(locale) => c_string(locale.as_bytes()),
        None => c_string(&crate::default_locale(req)),
    };
    let Some((date_style, time_style, pattern)) = format_styles(req, format)? else {
        return Ok(OrFalse::False);
    };
    let (calendar, zone, millis) = if object.native::<calendar::CalendarObject>(req).is_some() {
        let Some(held) = calendar::held(req, object) else {
            let message = "bad IntlCalendar instance: not initialized properly";
            return refuse(req, Code::ILLEGAL_ARGUMENT, message);
        };
        let zone = cal::php_intl_cal_zone(held);
        let Some(mut copy) = cal::php_intl_cal_clone(held) else {
            return refuse(
                req,
                Code::MEMORY_ALLOCATION,
                "error obtaining instant from IntlCalendar",
            );
        };
        let mut status = Code::ZERO;
        let millis = cal::php_intl_cal_time(&mut copy, 0, 0.0, &mut status);
        if status.is_failure() {
            return refuse(req, status, "error obtaining instant from IntlCalendar");
        }
        (Some(copy), zone, millis)
    } else if is_a(req, object, b"DateTimeInterface")? {
        let Some((millis, zone)) = datetime_decompose(req, object, true, &mut Outside::Request)?
        else {
            return Ok(OrFalse::False);
        };
        (None, zone, millis)
    } else {
        let message =
            "the passed object must be an instance of either IntlCalendar or DateTimeInterface";
        return refuse(req, Code::ZERO, message);
    };
    let Some(zone) = zone else {
        return refuse(req, Code::MEMORY_ALLOCATION, "could not clone TimeZone");
    };
    let (mut stage, mut status) = (0, Code::ZERO);
    let pattern = pattern.as_ref().map(|p| CName::from(p.as_c_str()));
    let made = df::php_intl_datefmt_format_object(
        calendar,
        millis,
        zone,
        pattern,
        date_style,
        time_style,
        CName::from(locale.as_c_str()),
        &mut stage,
        &mut status,
    );
    let message = match stage {
        1 => "could not create GregorianCalendar",
        2 => "could not create SimpleDateFormat",
        3 => "could not create DateFormat",
        _ => "",
    };
    if !message.is_empty() {
        return refuse(req, status, message);
    }
    match made.map_or(Ok(Vec::new()), |text| text.to_utf8()) {
        Ok(bytes) => PhpString::new(req, &bytes).map(OrFalse::Value),
        Err(code) => refuse(req, code, "error converting result to UTF-8"),
    }
}

/// `intl_error_set(NULL, code, msg); RETURN_FALSE;`.
fn refuse<T>(req: &mut Request, code: Code, message: &str) -> Answer<T> {
    error::set(req, code, message)?;
    Ok(OrFalse::False)
}

/// How a parse reads its text: to an instant, to an instant through the
/// formatter's own calendar, or to `localtime()`'s fields.
#[derive(Clone, Copy, PartialEq)]
enum Parse {
    Instant,
    Calendar,
    Fields,
}

/// The fields `internal_parse_to_localtime` reads, by `UCalendarDateFields`.
const LOCALTIME: [(i32, &str); 8] = [
    (13, "tm_sec"),
    (12, "tm_min"),
    (11, "tm_hour"),
    (1, "tm_year"),
    (5, "tm_mday"),
    (7, "tm_wday"),
    (6, "tm_yday"),
    (2, "tm_mon"),
];

/// A parse's answer: an instant in seconds, or `localtime()`'s fields.
type Parsed = Union<Union<i64, f64>, Owner<PhpArray>>;

/// `datefmt_parse`, `datefmt_localtime` and
/// `IntlDateFormatter::parseToCalendar`: the offset read, checked
/// against the text, the text parsed from it, and the offset written
/// back where it was given.
fn parse(
    req: &mut Request,
    object: PhpObject,
    text: PhpString,
    offset: &Ref<Untyped<Value>>,
    how: Parse,
) -> Answer<Parsed> {
    fetch(req, object)?;
    let mut position = None;
    if offset.is_given() {
        let given = offset.get().map_or(Value::null(), |Untyped(v)| v.deref());
        let read = match how {
            Parse::Calendar => try_long(req, &given)?,
            _ => to_long(req, &given)?,
        };
        // `ZEND_LONG_INT_OVFL`, which refuses past `INT_MAX` alone, then
        // C's `(int32_t)`: a long below `INT_MIN` keeps its low half.
        if read > i64::from(i32::MAX) {
            error::set(
                req,
                Code::ILLEGAL_ARGUMENT,
                "String index is out of valid range.",
            )?;
            return Ok(OrFalse::False);
        }
        let pos = crate::truncated(read);
        let past = usize::try_from(pos).map_or(true, |p| p > text.len());
        if past && !(how == Parse::Calendar && pos == -1) {
            return Ok(OrFalse::False);
        }
        position = Some(pos);
    }
    let answer = with_held(req, object, |req, udatf| {
        parse_text(req, object, udatf, text.as_bytes(), position.as_mut(), how)
    });
    if let Some(pos) = position {
        offset.set(req, Untyped(Value::int(i64::from(pos))))?;
    }
    answer
}

/// `zval_try_get_long`, as `parseToCalendar` reads its offset: php's
/// `TypeError` for a value no integer can be read from.
///
/// ```php
/// $f = new IntlDateFormatter('en_US', IntlDateFormatter::NONE, IntlDateFormatter::NONE, 'UTC', null, "'x'yyyy");
/// foreach (['3abc', 'abc', ' 1', "\v1", '1.5', '9999999999999999999', 2.5e30, PHP_INT_MIN, -5, null, []] as $offset) {
///     try { $at = $f->parseToCalendar('x2020', $offset); echo $at === false ? 'false' : 'parsed', ' ', var_export($offset, true), "\n"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// }
/// $offset = PHP_INT_MIN;
/// var_dump($f->parse('x2020', $offset), $offset);
/// ```
/// ```expect
/// Warning: A non-numeric value encountered in %s on line 4
/// parsed 5
/// IntlDateFormatter::parseToCalendar(): Argument #2 ($offset) must be of type int, string given
/// parsed 5
/// parsed 5
///
/// Deprecated: Implicit conversion from float-string "1.5" to int loses precision in %s on line 4
/// parsed 5
///
/// Deprecated: Implicit conversion from float-string "9999999999999999999" to int loses precision in %s on line 4
/// false '9999999999999999999'
///
/// Warning: The float 2.5E+30 is not representable as an int, cast occurred in %s on line 4
/// parsed 5
/// parsed 5
/// false -5
/// parsed 5
/// IntlDateFormatter::parseToCalendar(): Argument #2 ($offset) must be of type int, array given
/// int(1577836800)
/// int(5)
/// ```
fn try_long(req: &mut Request, value: &Value) -> Result<i64, Thrown> {
    if let Some(n) = coerce::try_long(req, value)? {
        return Ok(n);
    }
    let tail = format!("must be of type int, {} given", value_name(req, value));
    Err(argument_error(req, "TypeError", 2, &tail))
}

/// `internal_parse_to_timestamp` and `internal_parse_to_localtime`.
fn parse_text(
    req: &mut Request,
    object: PhpObject,
    udatf: &mut DateFormat,
    text: &[u8],
    position: Option<&mut i32>,
    how: Parse,
) -> Answer<Parsed> {
    let converted = ffi::to_utf16(text);
    let code = converted.as_ref().err().copied().unwrap_or(Code::ZERO);
    if let Err(stop) = check(req, object, code, "Error converting timezone to UTF-16") {
        return or_false(Err(stop));
    }
    let units = converted.unwrap_or_default();
    let mut status = Code::ZERO;
    let mut millis = udatf.parse(&units, position, how != Parse::Instant, &mut status);
    if how == Parse::Calendar {
        if let Err(stop) = check(req, object, status, "Calendar parsing failed") {
            return or_false(Err(stop));
        }
        millis = udatf.millis(&mut status);
    }
    if let Err(stop) = check(req, object, status, "Date parsing failed") {
        return or_false(Err(stop));
    }
    if how != Parse::Fields {
        let seconds = millis / 1000.0;
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            reason = "php's casts"
        )]
        let n = if seconds > i64::MAX as f64 || seconds < i64::MIN as f64 {
            Union::B(if seconds < 0.0 {
                seconds.ceil()
            } else {
                seconds.floor()
            })
        } else {
            Union::A(seconds as i64)
        };
        return Ok(OrFalse::Value(Union::A(n)));
    }
    let mut table = PhpArray::new(req)?;
    for (field, key) in LOCALTIME {
        let mut status = Code::ZERO;
        let value = i64::from(udatf.calendar_field(Some(field), &mut status));
        let message = "Date parsing - localtime failed : could not get a field from calendar";
        if let Err(stop) = check(req, object, status, message) {
            table.give_up(req);
            return or_false(Err(stop));
        }
        let value = match key {
            "tm_year" => value - 1900,
            "tm_wday" => value - 1,
            _ => value,
        };
        table.set_str(req, key.as_bytes(), Owner::int(value))?;
    }
    let mut status = Code::ZERO;
    let dst = udatf.calendar_field(None, &mut status);
    let message = "Date parsing - localtime failed : while checking if currently in DST.";
    if let Err(stop) = check(req, object, status, message) {
        table.give_up(req);
        return or_false(Err(stop));
    }
    table.set_str(req, b"tm_isdst", Owner::int(i64::from(dst == 1)))?;
    Ok(OrFalse::Value(Union::B(table)))
}

/// `datefmt_get_error_code`: the object's.
fn error_code(req: &mut Request, object: PhpObject) -> i64 {
    i64::from(error::object_code::<DateFormatObject>(req, object).0)
}

/// `datefmt_get_error_message`: the object's.
fn error_message(req: &mut Request, object: PhpObject) -> Result<Owner<PhpString>, Thrown> {
    let message = error::object_message::<DateFormatObject>(req, object);
    PhpString::new(req, &message)
}

/// `int|float|false`, of a parse's answer.
fn instant(req: &mut Request, parsed: OrFalse<Parsed>) -> OrFalse<Union<i64, f64>> {
    match parsed {
        OrFalse::Value(Union::A(n)) => OrFalse::Value(n),
        OrFalse::Value(Union::B(table)) => {
            table.give_up(req);
            OrFalse::False
        }
        OrFalse::False => OrFalse::False,
    }
}

/// `array|false`, of a parse's answer.
fn fields(parsed: OrFalse<Parsed>) -> OrFalse<Owner<PhpArray>> {
    match parsed {
        OrFalse::Value(Union::B(table)) => OrFalse::Value(table),
        _ => OrFalse::False,
    }
}

/// `class IntlDateFormatter`: one `UDateFormat` each, cloned with the
/// object without the styles php leaves behind; one `new` without a
/// constructor has none.
///
/// ```php
/// $f = new IntlDateFormatter('en_US', IntlDateFormatter::LONG, IntlDateFormatter::SHORT, 'UTC');
/// var_dump($f->format(86400 * 365), $f->getDateType(), $f->getTimeType(), $f->getCalendar());
/// $g = clone $f; var_dump($g->format(0), $g->getDateType(), $g->getCalendar());
/// $u = (new ReflectionClass('IntlDateFormatter'))->newInstanceWithoutConstructor();
/// try { clone $u; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $u->format(0); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $f->__construct('en', 0, 0); } catch (IntlException $e) { echo $e->getMessage(), "\n"; }
/// try { new IntlDateFormatter('en', 9, 0); } catch (IntlException $e) { echo $e->getMessage(), "\n"; }
/// try { new IntlDateFormatter('x', 0, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(IntlDateFormatter::create('fr_FR', IntlDateFormatter::FULL, IntlDateFormatter::NONE, 'Europe/Paris')->format(0));
/// var_dump(IntlDateFormatter::RELATIVE_SHORT, IntlDateFormatter::PATTERN, IntlDateFormatter::TRADITIONAL);
/// $h = new IntlDateFormatter('en_US', 0, 0, 'UTC', null, 'yyyy-MM-dd'); $h->setLenient(false);
/// var_dump($h->setCalendar(IntlDateFormatter::GREGORIAN), $h->getTimeZoneId(), get_class($h->getCalendarObject()), $h->getTimeZone()->getID(), $h->setTimeZone('Europe/Oslo'), $h->setPattern('yyyy'), $h->getPattern(), $h->getLocale(), $h->isLenient());
/// var_dump(IntlDateFormatter::formatObject(new DateTime('@0'), 'yyyy', 'en'), $h->parse('2000'), is_int($h->parseToCalendar('2001')), $h->localtime('2002')['tm_year'], $h->getErrorCode(), $h->getErrorMessage());
/// ```
/// ```expect
/// string(29) "January 1, 1971 at 12:00 AM"
/// int(1)
/// int(3)
/// int(1)
/// string(29) "January 1, 1970 at 12:00 AM"
/// int(0)
/// bool(false)
/// Cannot clone uninitialized IntlDateFormatter
/// Found unconstructed IntlDateFormatter
/// IntlDateFormatter::__construct(): cannot call constructor twice
/// IntlDateFormatter::__construct(): invalid date format style
/// IntlDateFormatter::__construct(): Argument #1 ($locale) "x" is invalid
/// string(20) "jeudi 1 janvier 1970"
/// int(131)
/// int(-2)
/// int(0)
/// bool(true)
/// string(3) "UTC"
/// string(21) "IntlGregorianCalendar"
/// string(3) "UTC"
/// bool(true)
/// bool(true)
/// string(4) "yyyy"
/// string(2) "en"
/// bool(false)
/// string(4) "1970"
/// int(946681200)
/// bool(true)
/// int(102)
/// int(0)
/// string(12) "U_ZERO_ERROR"
/// ```
#[php_class(not_serializable, size = 104, payload = DateFormatObject, handlers = crate::dateformat::HANDLERS)]
pub(crate) struct IntlDateFormatter;

#[php_class]
#[allow(non_snake_case)]
impl IntlDateFormatter {
    const FULL: i64 = 0;
    const LONG: i64 = 1;
    const MEDIUM: i64 = 2;
    const SHORT: i64 = 3;
    const NONE: i64 = -1;
    const RELATIVE_FULL: i64 = 128;
    const RELATIVE_LONG: i64 = 129;
    const RELATIVE_MEDIUM: i64 = 130;
    const RELATIVE_SHORT: i64 = 131;
    const PATTERN: i64 = -2;
    const GREGORIAN: i64 = 1;
    const TRADITIONAL: i64 = 0;

    #[php(arity = manual, before_args = error::reset)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        locale: Raw<Option<PhpString>>,
        #[php(default = IntlDateFormatter::FULL)] dateType: Raw<i64>,
        #[php(default = IntlDateFormatter::FULL)] timeType: Raw<i64>,
        #[php(default = null)] timezone: ZoneRaw,
        #[php(default = null)] calendar: Raw<Untyped<CalArg>>,
        #[php(default = null)] pattern: Raw<Option<PhpString>>,
    ) -> Result<(), Thrown> {
        let object = this.object();
        let raw = RawArgs {
            locale,
            date_type: dateType,
            time_type: timeType,
            zone: timezone,
            calendar,
            pattern,
        };
        error::constructing(req, |req| {
            let args = raw.hold(req, |Untyped(c)| c)?;
            construct(req, object, args).map(|_| ())
        })
    }

    #[php(tentative, arity = manual, before_args = error::reset)]
    fn create(
        req: &mut Request,
        locale: Raw<Option<PhpString>>,
        #[php(default = IntlDateFormatter::FULL)] dateType: Raw<i64>,
        #[php(default = IntlDateFormatter::FULL)] timeType: Raw<i64>,
        #[php(default = null)] timezone: ZoneRaw,
        #[php(default = null)] calendar: Raw<CalArg>,
        #[php(default = null)] pattern: Raw<Option<PhpString>>,
    ) -> Result<Option<Owner<Fmt>>, Thrown> {
        let raw = RawArgs {
            locale,
            date_type: dateType,
            time_type: timeType,
            zone: timezone,
            calendar,
            pattern,
        };
        let args = raw.hold(req, |c| c)?;
        create(req, args)
    }

    #[php(tentative, before_args = error::reset)]
    fn getDateType(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        get_type(req, this.object(), true).map(OrFalse::Value)
    }

    #[php(tentative, before_args = error::reset)]
    fn getTimeType(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        get_type(req, this.object(), false).map(OrFalse::Value)
    }

    #[php(tentative, before_args = error::reset)]
    fn getCalendar(req: &mut Request, this: This<Self>) -> Answer<i64> {
        get_calendar(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn setCalendar(req: &mut Request, this: This<Self>, calendar: CalArg) -> Result<bool, Thrown> {
        set_calendar(req, this.object(), calendar)
    }

    #[php(tentative, before_args = error::reset)]
    fn getTimeZoneId(req: &mut Request, this: This<Self>) -> Answer<Owner<PhpString>> {
        get_timezone_id(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn getCalendarObject(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<OrFalse<Owner<Instance<IntlCalendar>>>>, Thrown> {
        get_calendar_object(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn getTimeZone(req: &mut Request, this: This<Self>) -> Answer<Owner<Instance<IntlTimeZone>>> {
        get_timezone(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn setTimeZone(req: &mut Request, this: This<Self>, timezone: ZoneRaw) -> Result<bool, Thrown> {
        let zone = zone_held(req, timezone.value(), 1)?;
        set_timezone(req, this.object(), zone)
    }

    #[php(tentative, before_args = error::reset)]
    fn setPattern(req: &mut Request, this: This<Self>, pattern: PhpString) -> Result<bool, Thrown> {
        set_pattern(req, this.object(), pattern).map(|a| matches!(a, OrFalse::Value(true)))
    }

    #[php(tentative, before_args = error::reset)]
    fn getPattern(req: &mut Request, this: This<Self>) -> Answer<Owner<PhpString>> {
        get_pattern(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn getLocale(
        req: &mut Request,
        this: This<Self>,
        #[php(default = ULOC_ACTUAL_LOCALE)] r#type: i64,
    ) -> Answer<Owner<PhpString>> {
        get_locale(req, this.object(), r#type)
    }

    #[php(tentative, before_args = error::reset)]
    fn setLenient(req: &mut Request, this: This<Self>, lenient: bool) -> Result<(), Thrown> {
        set_lenient(req, this.object(), lenient)
    }

    #[php(tentative, before_args = error::reset)]
    fn isLenient(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        is_lenient(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn format(
        req: &mut Request,
        this: This<Self>,
        datetime: Untyped<Value>,
    ) -> Answer<Owner<PhpString>> {
        format(req, this.object(), datetime.0)
    }

    #[php(tentative)]
    fn formatObject(
        req: &mut Request,
        datetime: Untyped<PhpObject>,
        #[php(default = null)] format: Untyped<Value>,
        #[php(default = null)] locale: Option<PhpString>,
    ) -> Answer<Owner<PhpString>> {
        format_object(req, datetime.0, format.0, locale)
    }

    #[php(tentative, before_args = error::reset)]
    fn parse(
        req: &mut Request,
        this: This<Self>,
        string: PhpString,
        #[php(default = null)] offset: Ref<Untyped<Value>>,
    ) -> Result<OrFalse<Union<i64, f64>>, Thrown> {
        parse(req, this.object(), string, &offset, Parse::Instant)
            .map(|parsed| instant(req, parsed))
    }

    #[php(before_args = error::reset)]
    fn parseToCalendar(
        req: &mut Request,
        this: This<Self>,
        string: PhpString,
        #[php(default = null)] offset: Ref<Untyped<Value>>,
    ) -> Result<OrFalse<Union<i64, f64>>, Thrown> {
        parse(req, this.object(), string, &offset, Parse::Calendar)
            .map(|parsed| instant(req, parsed))
    }

    #[php(tentative, before_args = error::reset)]
    fn localtime(
        req: &mut Request,
        this: This<Self>,
        string: PhpString,
        #[php(default = null)] offset: Ref<Untyped<Value>>,
    ) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
        parse(req, this.object(), string, &offset, Parse::Fields).map(fields)
    }

    #[php(tentative, before_args = error::reset)]
    fn getErrorCode(req: &mut Request, this: This<Self>) -> i64 {
        error_code(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn getErrorMessage(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        error_message(req, this.object())
    }
}

/// `datefmt_create`: null where the constructor fails.
///
/// ```php
/// var_dump(datefmt_create('en_US', 0, 0, 'UTC', null, 'yyyy-MM-dd') instanceof IntlDateFormatter, datefmt_create('en', 0, 0, 'UTC', 7), intl_get_error_message());
/// ```
/// ```expect
/// bool(true)
/// NULL
/// string(234) "datefmt_create(): Invalid value for calendar type; it must be one of IntlDateFormatter::TRADITIONAL (locale's default calendar) or IntlDateFormatter::GREGORIAN. Alternatively, it can be an IntlCalendar object: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(arity = manual, before_args = error::reset)]
#[allow(non_snake_case)]
fn datefmt_create(
    req: &mut Request,
    locale: Raw<Option<PhpString>>,
    #[php(default = IntlDateFormatter::FULL)] dateType: Raw<i64>,
    #[php(default = IntlDateFormatter::FULL)] timeType: Raw<i64>,
    #[php(default = null)] timezone: ZoneRaw,
    #[php(default = null)] calendar: Raw<CalArg>,
    #[php(default = null)] pattern: Raw<Option<PhpString>>,
) -> Result<Option<Owner<Fmt>>, Thrown> {
    let raw = RawArgs {
        locale,
        date_type: dateType,
        time_type: timeType,
        zone: timezone,
        calendar,
        pattern,
    };
    let args = raw.hold(req, |c| c)?;
    create(req, args)
}

/// `datefmt_get_datetype`.
///
/// ```php
/// var_dump(datefmt_get_datetype(datefmt_create('en', IntlDateFormatter::SHORT, IntlDateFormatter::NONE, 'UTC')));
/// ```
/// ```expect
/// int(3)
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_get_datetype(req: &mut Request, formatter: Fmt) -> Result<OrFalse<i64>, Thrown> {
    get_type(req, formatter.object(), true).map(OrFalse::Value)
}

/// `datefmt_get_timetype`.
///
/// ```php
/// var_dump(datefmt_get_timetype(datefmt_create('en', IntlDateFormatter::SHORT, IntlDateFormatter::NONE, 'UTC')));
/// ```
/// ```expect
/// int(-1)
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_get_timetype(req: &mut Request, formatter: Fmt) -> Result<OrFalse<i64>, Thrown> {
    get_type(req, formatter.object(), false).map(OrFalse::Value)
}

/// `datefmt_get_calendar`: `false` for a formatter made with an
/// `IntlCalendar`.
///
/// ```php
/// var_dump(datefmt_get_calendar(datefmt_create('en', 0, 0, 'UTC', IntlDateFormatter::TRADITIONAL)), datefmt_get_calendar(datefmt_create('en', 0, 0, 'UTC', IntlCalendar::createInstance('UTC'))));
/// ```
/// ```expect
/// int(0)
/// bool(false)
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_get_calendar(req: &mut Request, formatter: Fmt) -> Answer<i64> {
    get_calendar(req, formatter.object())
}

/// `datefmt_set_calendar`: a type keeps the formatter's zone, an
/// `IntlCalendar` brings its own.
///
/// ```php
/// $f = datefmt_create('en_US', 0, 0, 'America/New_York', null, 'yyyy-MM-dd HH:mm zzz');
/// var_dump(datefmt_set_calendar($f, IntlDateFormatter::TRADITIONAL), $f->format(0), $f->getCalendar());
/// var_dump($f->setCalendar(IntlCalendar::createInstance('Asia/Tokyo', 'ja_JP@calendar=japanese')), $f->format(0), $f->getCalendar());
/// var_dump($f->setCalendar(5), $f->getErrorMessage());
/// ```
/// ```expect
/// bool(true)
/// string(20) "1969-12-31 19:00 EST"
/// int(0)
/// bool(true)
/// string(22) "0045-01-01 09:00 GMT+9"
/// bool(false)
/// bool(false)
/// string(250) "IntlDateFormatter::setCalendar(): Invalid value for calendar type; it must be one of IntlDateFormatter::TRADITIONAL (locale's default calendar) or IntlDateFormatter::GREGORIAN. Alternatively, it can be an IntlCalendar object: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_set_calendar(
    req: &mut Request,
    formatter: Fmt,
    calendar: CalArg,
) -> Result<bool, Thrown> {
    set_calendar(req, formatter.object(), calendar)
}

/// `datefmt_get_timezone_id`.
///
/// ```php
/// var_dump(datefmt_get_timezone_id(datefmt_create('en', 0, 0, 'Europe/Lisbon')));
/// ```
/// ```expect
/// string(13) "Europe/Lisbon"
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_get_timezone_id(req: &mut Request, formatter: Fmt) -> Answer<Owner<PhpString>> {
    get_timezone_id(req, formatter.object())
}

/// `datefmt_get_calendar_object`: a clone of the formatter's calendar.
///
/// ```php
/// $c = datefmt_get_calendar_object(datefmt_create('en', 0, 0, 'Europe/Lisbon'));
/// var_dump(get_class($c), $c->getTimeZone()->getID(), $c->getType());
/// ```
/// ```expect
/// string(21) "IntlGregorianCalendar"
/// string(13) "Europe/Lisbon"
/// string(9) "gregorian"
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_get_calendar_object(
    req: &mut Request,
    formatter: Fmt,
) -> Result<Option<OrFalse<Owner<Instance<IntlCalendar>>>>, Thrown> {
    get_calendar_object(req, formatter.object())
}

/// `datefmt_get_timezone`: a clone of the formatter's zone.
///
/// ```php
/// var_dump(datefmt_get_timezone(datefmt_create('en', 0, 0, 'Asia/Tokyo'))->getID());
/// ```
/// ```expect
/// string(10) "Asia/Tokyo"
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_get_timezone(
    req: &mut Request,
    formatter: Fmt,
) -> Answer<Owner<Instance<IntlTimeZone>>> {
    get_timezone(req, formatter.object())
}

/// `datefmt_set_timezone`: an `IntlTimeZone`, a `DateTimeZone` or an id.
///
/// ```php
/// $f = datefmt_create('en_US', 0, 0, 'UTC', null, 'HH:mm VV');
/// var_dump(datefmt_set_timezone($f, 'Asia/Kolkata'), $f->format(0), $f->setTimeZone(new DateTimeZone('-03:30')), $f->format(0), $f->setTimeZone(IntlTimeZone::createTimeZone('Europe/Paris')), $f->format(0));
/// try { $f->setTimeZone('Nowhere/Else'); } catch (IntlException $e) { echo $e->getMessage(), "\n"; }
/// try { $f->setTimeZone([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(18) "05:30 Asia/Kolkata"
/// bool(true)
/// string(15) "20:30 GMT-03:30"
/// bool(true)
/// string(18) "01:00 Europe/Paris"
/// IntlDateFormatter::setTimeZone(): No such time zone: "Nowhere/Else"
/// IntlDateFormatter::setTimeZone(): Argument #1 ($timezone) must be of type object|string|null, array given
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_set_timezone(
    req: &mut Request,
    formatter: Fmt,
    timezone: ZoneRaw,
) -> Result<bool, Thrown> {
    let zone = zone_held(req, timezone.value(), 2)?;
    set_timezone(req, formatter.object(), zone)
}

/// `datefmt_set_pattern` and `datefmt_get_pattern`.
///
/// ```php
/// $f = datefmt_create('en_US', IntlDateFormatter::SHORT, IntlDateFormatter::SHORT, 'UTC');
/// var_dump(datefmt_get_pattern($f), datefmt_set_pattern($f, "yyyy'-'MM"), $f->getPattern(), $f->format(0));
/// ```
/// ```expect
/// string(16) "M/d/yy, h:mm a"
/// bool(true)
/// string(9) "yyyy'-'MM"
/// string(7) "1970-01"
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_set_pattern(
    req: &mut Request,
    formatter: Fmt,
    pattern: PhpString,
) -> Result<bool, Thrown> {
    set_pattern(req, formatter.object(), pattern).map(|a| matches!(a, OrFalse::Value(true)))
}

/// `datefmt_get_pattern`.
///
/// ```php
/// var_dump(datefmt_get_pattern(datefmt_create('de_DE', IntlDateFormatter::MEDIUM, IntlDateFormatter::NONE, 'UTC')));
/// ```
/// ```expect
/// string(7) "dd.MM.y"
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_get_pattern(req: &mut Request, formatter: Fmt) -> Answer<Owner<PhpString>> {
    get_pattern(req, formatter.object())
}

/// `datefmt_get_locale`.
///
/// ```php
/// $f = datefmt_create('de-AT', 0, 0, 'UTC');
/// var_dump(datefmt_get_locale($f), $f->getLocale(ULOC_VALID_LOCALE), $f->getLocale(7), intl_get_error_message());
/// ```
/// ```expect
/// string(2) "de"
/// string(5) "de_AT"
/// bool(false)
/// string(78) "IntlDateFormatter::getLocale(): Error getting locale: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_get_locale(
    req: &mut Request,
    formatter: Fmt,
    #[php(default = ULOC_ACTUAL_LOCALE)] r#type: i64,
) -> Answer<Owner<PhpString>> {
    get_locale(req, formatter.object(), r#type)
}

/// `datefmt_set_lenient` and `datefmt_is_lenient`.
///
/// ```php
/// $f = datefmt_create('en_US', 0, 0, 'UTC', null, 'yyyy-MM-dd');
/// var_dump(datefmt_is_lenient($f), $f->parse('2001-13-45'));
/// datefmt_set_lenient($f, false);
/// var_dump($f->isLenient(), $f->parse('2001-13-45'), $f->getErrorMessage());
/// ```
/// ```expect
/// bool(true)
/// int(1013644800)
/// bool(false)
/// bool(false)
/// string(62) "IntlDateFormatter::parse(): Date parsing failed: U_PARSE_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_set_lenient(req: &mut Request, formatter: Fmt, lenient: bool) -> Result<(), Thrown> {
    set_lenient(req, formatter.object(), lenient)
}

/// `datefmt_is_lenient`.
///
/// ```php
/// var_dump(datefmt_is_lenient(datefmt_create('en', 0, 0, 'UTC')));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_is_lenient(req: &mut Request, formatter: Fmt) -> Result<bool, Thrown> {
    is_lenient(req, formatter.object())
}

/// `datefmt_format`: a number of seconds, a numeric string, a
/// `DateTimeInterface`, an `IntlCalendar` or `localtime()`'s array.
///
/// ```php
/// $f = datefmt_create('en_US', 0, 0, 'UTC', null, 'yyyy-MM-dd HH:mm:ss.SSS');
/// var_dump(datefmt_format($f, 1.5), $f->format('86400'), $f->format(new DateTime('2020-02-29 12:00', new DateTimeZone('UTC'))));
/// $c = IntlCalendar::createInstance('UTC'); $c->setTime(1e12);
/// $g = datefmt_create('en_US', 0, 0, 'UTC', null, 'yyyy-MM-dd HH:mm');
/// var_dump($f->format($c), $g->format(['tm_year' => 99, 'tm_mon' => 11, 'tm_mday' => 31, 'tm_hour' => 23]), $f->format([]));
/// var_dump($f->format('x'), intl_get_error_message(), $f->format(['tm_sec' => 'a']), $f->getErrorMessage());
/// ```
/// ```expect
/// string(23) "1970-01-01 00:00:01.500"
/// string(23) "1970-01-02 00:00:00.000"
/// string(23) "2020-02-29 12:00:00.000"
/// string(23) "2001-09-09 01:46:40.000"
/// string(16) "1999-12-31 23:00"
/// bool(false)
/// bool(false)
/// string(131) "IntlDateFormatter::format(): string 'x' is not numeric, which would be required for it to be a valid date: U_ILLEGAL_ARGUMENT_ERROR"
/// bool(false)
/// string(77) "IntlDateFormatter::format(): date formatting failed: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_format(
    req: &mut Request,
    formatter: Fmt,
    datetime: Untyped<Value>,
) -> Answer<Owner<PhpString>> {
    format(req, formatter.object(), datetime.0)
}

/// `datefmt_format_object`.
///
/// ```php
/// $d = new DateTime('2012-01-01 13:14:15', new DateTimeZone('Europe/Lisbon'));
/// var_dump(datefmt_format_object($d, null, 'en_US'), IntlDateFormatter::formatObject($d, IntlDateFormatter::SHORT, 'de'), IntlDateFormatter::formatObject($d, [IntlDateFormatter::NONE, IntlDateFormatter::RELATIVE_FULL], 'en'), IntlDateFormatter::formatObject($d, "yyyy QQQ", 'en'));
/// $c = IntlCalendar::createInstance('Asia/Tokyo', 'en_US'); $c->setTime(0.0);
/// var_dump(IntlDateFormatter::formatObject($c, null, 'en_US'));
/// var_dump(datefmt_format_object($d, [1]), intl_get_error_message(), datefmt_format_object($d, 9), datefmt_format_object(new stdClass), intl_get_error_message());
/// ```
/// ```expect
/// string(25) "Jan 1, 2012, 1:14:15 PM"
/// string(15) "01.01.12, 13:14"
/// string(43) "1:14:15 PM Western European Standard Time"
/// string(7) "2012 Q1"
/// string(25) "Jan 1, 1970, 9:00:00 AM"
/// bool(false)
/// string(98) "datefmt_format_object(): bad format; if array, it must have two elements: U_ILLEGAL_ARGUMENT_ERROR"
/// bool(false)
/// bool(false)
/// string(120) "datefmt_format_object(): the passed object must be an instance of either IntlCalendar or DateTimeInterface: U_ZERO_ERROR"
/// ```
#[php_function]
fn datefmt_format_object(
    req: &mut Request,
    datetime: Untyped<PhpObject>,
    #[php(default = null)] format: Untyped<Value>,
    #[php(default = null)] locale: Option<PhpString>,
) -> Answer<Owner<PhpString>> {
    format_object(req, datetime.0, format.0, locale)
}

/// `datefmt_parse` and `IntlDateFormatter::parseToCalendar`, the offset
/// written back where given.
///
/// ```php
/// $f = datefmt_create('en_US', 0, 0, 'UTC', null, 'yyyy-MM-dd');
/// $p = 2; var_dump(datefmt_parse($f, 'x 2001-02-03 y', $p), $p, $f->parse('nope'), intl_get_error_message());
/// $q = 99; var_dump($f->parse('2001-02-03', $q), $q);
/// $f->setPattern('VV'); $r = null; var_dump(is_int($f->parseToCalendar('Asia/Tokyo', $r)), $r, $f->getTimeZoneId());
/// ```
/// ```expect
/// int(981158400)
/// int(12)
/// bool(false)
/// string(62) "IntlDateFormatter::parse(): Date parsing failed: U_PARSE_ERROR"
/// bool(false)
/// int(99)
/// bool(true)
/// int(10)
/// string(10) "Asia/Tokyo"
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_parse(
    req: &mut Request,
    formatter: Fmt,
    string: PhpString,
    #[php(default = null)] offset: Ref<Untyped<Value>>,
) -> Result<OrFalse<Union<i64, f64>>, Thrown> {
    parse(req, formatter.object(), string, &offset, Parse::Instant)
        .map(|parsed| instant(req, parsed))
}

/// `datefmt_localtime`: `localtime()`'s fields of what the text names.
///
/// ```php
/// $f = datefmt_create('en_US', 0, 0, 'America/New_York', null, 'yyyy-MM-dd HH:mm:ss');
/// var_dump(datefmt_localtime($f, '2010-07-04 12:30:45'), $f->localtime('junk'));
/// ```
/// ```expect
/// array(9) {
///   ["tm_sec"]=>
///   int(45)
///   ["tm_min"]=>
///   int(30)
///   ["tm_hour"]=>
///   int(12)
///   ["tm_year"]=>
///   int(110)
///   ["tm_mday"]=>
///   int(4)
///   ["tm_wday"]=>
///   int(0)
///   ["tm_yday"]=>
///   int(185)
///   ["tm_mon"]=>
///   int(6)
///   ["tm_isdst"]=>
///   int(1)
/// }
/// bool(false)
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_localtime(
    req: &mut Request,
    formatter: Fmt,
    string: PhpString,
    #[php(default = null)] offset: Ref<Untyped<Value>>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    parse(req, formatter.object(), string, &offset, Parse::Fields).map(fields)
}

/// `datefmt_get_error_code` and `datefmt_get_error_message`: the object's.
///
/// ```php
/// $f = datefmt_create('en', 0, 0, 'UTC'); $f->parse('x');
/// var_dump(datefmt_get_error_code($f), datefmt_get_error_message($f));
/// ```
/// ```expect
/// int(9)
/// string(62) "IntlDateFormatter::parse(): Date parsing failed: U_PARSE_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_get_error_code(req: &mut Request, formatter: Fmt) -> i64 {
    error_code(req, formatter.object())
}

/// `datefmt_get_error_message`.
///
/// ```php
/// var_dump(datefmt_get_error_message(datefmt_create('en', 0, 0, 'UTC')));
/// ```
/// ```expect
/// string(12) "U_ZERO_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn datefmt_get_error_message(
    req: &mut Request,
    formatter: Fmt,
) -> Result<Owner<PhpString>, Thrown> {
    error_message(req, formatter.object())
}

/// The `datefmt_*` block of `php_intl.stub.php`.
pub(crate) fn functions(b: ExtensionBuilder) -> ExtensionBuilder {
    b.functions(vec![
        datefmt_create::declared(),
        datefmt_get_datetype::declared(),
        datefmt_get_timetype::declared(),
        datefmt_get_calendar::declared(),
        datefmt_set_calendar::declared(),
        datefmt_get_timezone_id::declared(),
        datefmt_get_calendar_object::declared(),
        datefmt_get_timezone::declared(),
        datefmt_set_timezone::declared(),
        datefmt_set_pattern::declared(),
        datefmt_get_pattern::declared(),
        datefmt_get_locale::declared(),
        datefmt_set_lenient::declared(),
        datefmt_is_lenient::declared(),
        datefmt_format::declared(),
        datefmt_format_object::declared(),
        datefmt_parse::declared(),
        datefmt_localtime::declared(),
        datefmt_get_error_code::declared(),
        datefmt_get_error_message::declared(),
    ])
}
