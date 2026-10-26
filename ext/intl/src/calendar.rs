//! `calendar/`: `IntlCalendar` and `IntlGregorianCalendar` over an
//! `icu::Calendar` (`calendar_class.cpp`, `calendar_methods.cpp`,
//! `gregoriancalendar_methods.cpp`) and the `intlcal_*` and
//! `intlgregcal_*` functions their methods alias. The calendar is the
//! shim's ([`crate::ffi::calendar`]); what php does around it, its
//! arguments, its errors and the objects it hands back, is here.
//!
//! A method and the function it aliases share one body, which is told
//! which of the two it serves: php numbers an argument from the
//! calendar for the function and from the first argument for the
//! method (`hasThis() ? n - 1 : n`).

use php_ext::abi::{
    CloneObj, DebugInfo, ExtensionBuilder, Instance, ObjectHandlers, OrFalse, Passed, Raw, Static,
    This, True, Untyped,
};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{call_method, clone_with, instantiate, lookup_class, new_object};
use php_ext::coerce::{coerce_param, to_long};
use php_ext::decl::Type;
use php_ext::raising::{
    Level, Thrown, arg_value_error, argument_count_error, argument_error, arity, docref, fatal,
    pending, throw, throw_error,
};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::common_date::{
    DateTime, ZoneArg, convert_datetimezone, convert_to_datetimezone, gmt, now,
    process_timezone_argument, zone_held, zone_sorted,
};
use crate::error::{self, Carrier, IntlError, Outside, or_false};
use crate::ffi::calendar::{self as cal, Cal, Calendar, LOCALE_CAPACITY};
use crate::ffi::cpp::{CName, Text};
use crate::ffi::timezone as tz;
use crate::ffi::{Code, c_string, to_utf16};
use crate::iterator;
use crate::timezone::{self, set};

/// `Calendar_object`: the error and the calendar, none until a factory
/// or a constructor lays one down.
#[derive(Default)]
pub(crate) struct CalendarObject {
    error: IntlError,
    cal: Option<Cal>,
}

impl Carrier for CalendarObject {
    fn error(&mut self) -> &mut IntlError {
        &mut self.error
    }
}

/// `Calendar_handlers`: the clone and the debug table.
pub(crate) static HANDLERS: Handlers = Handlers;

/// The handlers of `IntlCalendar` and `IntlGregorianCalendar`.
pub(crate) struct Handlers;

/// A native's answer where php may `RETURN_FALSE`.
type Answer<T> = Result<OrFalse<T>, Thrown>;

/// `UCAL_FIELD_COUNT`.
const FIELD_COUNT: i64 = 24;

/// The field calls of `php_intl_cal_field`, by its selector.
const GET: i32 = 0;
const ACTUAL_MAXIMUM: i32 = 1;
const ACTUAL_MINIMUM: i32 = 2;
const GREATEST_MINIMUM: i32 = 3;
const LEAST_MAXIMUM: i32 = 4;
const MAXIMUM: i32 = 5;
const MINIMUM: i32 = 6;
const IS_SET: i32 = 7;
const CLEAR: i32 = 8;
const SET: i32 = 9;
const ADD: i32 = 10;
const ROLL: i32 = 11;
const DIFFERENCE: i32 = 12;

/// `debug_info_fields[]`: each field `Calendar_get_debug_info` reads, by
/// the name it prints.
const DEBUG_FIELDS: [(i32, &str); 23] = [
    (0, "era"),
    (1, "year"),
    (2, "month"),
    (3, "week of year"),
    (4, "week of month"),
    (6, "day of year"),
    (5, "day of month"),
    (7, "day of week"),
    (8, "day of week in month"),
    (9, "AM/PM"),
    (10, "hour"),
    (11, "hour of day"),
    (12, "minute"),
    (13, "second"),
    (14, "millisecond"),
    (15, "zone offset"),
    (16, "DST offset"),
    (17, "year for week of year"),
    (18, "localized day of week"),
    (19, "extended year"),
    (20, "julian day"),
    (21, "milliseconds in day"),
    (22, "is leap month"),
];

/// The calendar an object holds and whether a method serves it, which
/// decides how php numbers the arguments.
#[derive(Clone, Copy)]
struct Of {
    object: PhpObject,
    method: bool,
}

impl Of {
    fn function(calendar: Instance<IntlCalendar>) -> Of {
        Of {
            object: calendar.object(),
            method: false,
        }
    }

    fn method(this: This<IntlCalendar>) -> Of {
        Of {
            object: this.object(),
            method: true,
        }
    }

    fn gregorian(calendar: Instance<IntlGregorianCalendar>) -> Of {
        Of {
            object: calendar.object(),
            method: false,
        }
    }

    fn this_gregorian(this: This<IntlGregorianCalendar>) -> Of {
        Of {
            object: this.object(),
            method: true,
        }
    }

    /// `hasThis() ? n - 1 : n`.
    fn at(self, n: u32) -> u32 {
        if self.method { n - 1 } else { n }
    }
}

/// `CALENDAR_METHOD_FETCH_OBJECT`: the object's error reset, the
/// request's having been reset before the arguments
/// (`CALENDAR_METHOD_INIT_VARS`, `before_args`), and php's `Error` for an
/// object no factory made.
fn fetch(req: &mut Request, o: Of) -> Result<(), Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    error::reset_object::<CalendarObject>(req, o.object);
    if held(req, o.object).is_none() {
        return Err(throw_error(
            req,
            ce.error,
            "Found unconstructed IntlCalendar",
        ));
    }
    Ok(())
}

/// The calendar an object holds.
pub(crate) fn held(req: &Request, object: PhpObject) -> Option<&Calendar> {
    object.native::<CalendarObject>(req)?.cal.as_deref()
}

/// What `f` does with the calendar of a fetched object.
fn with<T>(req: &mut Request, o: Of, f: impl FnOnce(&mut Calendar) -> T) -> Result<T, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    fetch(req, o)?;
    match o
        .object
        .native_mut::<CalendarObject>(req)
        .and_then(|c| c.cal.as_deref_mut())
    {
        Some(calendar) => Ok(f(calendar)),
        None => Err(throw_error(
            req,
            ce.error,
            "Found unconstructed IntlCalendar",
        )),
    }
}

/// `INTL_METHOD_CHECK_STATUS(co, msg)` over a code ICU wrote into the
/// object's error.
fn check(req: &mut Request, o: Of, code: Code, message: &str) -> Result<(), Option<Thrown>> {
    error::errors_set_code::<CalendarObject>(req, o.object, code);
    error::check_object::<CalendarObject>(req, o.object, message)
}

/// `value`, or `false` where ICU's code is a failure.
fn checked<T>(req: &mut Request, o: Of, (value, code): (T, Code), message: &str) -> Answer<T> {
    or_false(check(req, o, code, message).map(|()| value))
}

/// `ZEND_VALUE_ERROR_INVALID_FIELD`.
fn field(req: &mut Request, value: i64, position: u32) -> Result<i32, Thrown> {
    match i32::try_from(value) {
        Ok(field) if (0..FIELD_COUNT).contains(&value) => Ok(field),
        _ => Err(arg_value_error(req, position, "be a valid field")),
    }
}

/// `ZEND_VALUE_ERROR_OUT_OF_BOUND_VALUE`.
fn int32(req: &mut Request, value: i64, position: u32) -> Result<i32, Thrown> {
    i32::try_from(value).map_err(|_| {
        let message = format!("be between {} and {}", i32::MIN, i32::MAX);
        arg_value_error(req, position, &message)
    })
}

/// `ZEND_VALUE_ERROR_INVALID_DAY_OF_WEEK`.
fn day_of_week(req: &mut Request, value: i64, position: u32) -> Result<i32, Thrown> {
    match i32::try_from(value) {
        Ok(day) if (1..=7).contains(&day) => Ok(day),
        _ => Err(arg_value_error(req, position, "be a valid day of the week")),
    }
}

/// `calendar_object_create`: a new `IntlGregorianCalendar` for a
/// Gregorian calendar and an `IntlCalendar` for any other, holding
/// `calendar`.
pub(crate) fn made(
    req: &mut Request,
    mut calendar: Cal,
) -> Result<Owner<Instance<IntlCalendar>>, Thrown> {
    let name: &[u8] = if cal::php_intl_cal_is_gregorian(&mut calendar) {
        b"IntlGregorianCalendar"
    } else {
        b"IntlCalendar"
    };
    store(req, name, calendar).map(Owner::into_instance)
}

/// A new object of the class `name` holding `calendar`.
fn store(req: &mut Request, name: &[u8], calendar: Cal) -> Result<Owner<PhpObject>, Thrown> {
    let Some(class) = lookup_class(req, name)? else {
        return Err(fatal(req, "InternalError: IntlCalendar is not linked"));
    };
    let object = new_object(req, class)?;
    if let Some(o) = object.native_mut::<CalendarObject>(req) {
        o.cal = Some(calendar);
    }
    Ok(object)
}

/// The locale's name: the one given, or `intl_locale_get_default()`.
fn locale_name(req: &Request, locale: Option<PhpString>) -> std::ffi::CString {
    match locale {
        Some(locale) => c_string(locale.as_bytes()),
        None => c_string(&crate::default_locale(req)),
    }
}

/// `Calendar_clone_obj` and `Calendar_get_debug_info`.
impl ObjectHandlers for Handlers {
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_calendar)
    }

    /// `Calendar_get_debug_info`: whether the object holds a calendar,
    /// its type, its zone's table, its valid locale and every field.
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<DebugInfo>, Thrown> {
        let mut table = PhpArray::new(req)?;
        let calendar = object
            .native_mut::<CalendarObject>(req)
            .and_then(|c| c.cal.as_deref_mut())
            .map(|calendar| {
                let zone = cal::php_intl_cal_zone(calendar).map(|z| timezone::debug_rows(&z));
                let (mut locale, mut status) = ([0u8; LOCALE_CAPACITY], Code::ZERO);
                cal::php_intl_cal_locale(calendar, 1, &mut locale, &mut status);
                let locale = if status.is_failure() {
                    status.name().as_bytes().to_vec()
                } else {
                    c_text(&locale)
                };
                let fields = DEBUG_FIELDS.map(|(f, name)| {
                    let mut status = Code::ZERO;
                    let value = cal::php_intl_cal_field(calendar, GET, f, 0, 0.0, &mut status);
                    (name, (!status.is_failure()).then_some(value).ok_or(status))
                });
                (cal::kind(calendar), zone, locale, fields)
            });
        set(req, &mut table, b"valid", Owner::bool(calendar.is_some()))?;
        let Some((kind, zone, locale, fields)) = calendar else {
            return Ok(Some(table.into()));
        };
        let kind = PhpString::new(req, kind.as_bytes())?;
        set(req, &mut table, b"type", kind.into_value())?;
        let zone = timezone::debug_table(req, zone)?;
        set(req, &mut table, b"timeZone", zone.into_value())?;
        let locale = PhpString::new(req, &locale)?;
        set(req, &mut table, b"locale", locale.into_value())?;
        let mut rows = PhpArray::new(req)?;
        for (name, value) in fields {
            let value = match value {
                Ok(n) => Owner::int(i64::from(n)),
                Err(code) => PhpString::new(req, code.name().as_bytes())?.into_value(),
            };
            set(req, &mut rows, name.as_bytes(), value)?;
        }
        set(req, &mut table, b"fields", rows.into_value())?;
        Ok(Some(table.into()))
    }
}

/// The bytes before the first NUL.
fn c_text(buffer: &[u8]) -> Vec<u8> {
    buffer.iter().take_while(|&&b| b != 0).copied().collect()
}

/// `Calendar_clone_obj`: the members copied, the calendar cloned, and
/// php's `Error` for an object that holds none.
///
/// ```php
/// $a = IntlCalendar::createInstance('Europe/Lisbon', 'pt_PT'); $a->setTime(1356998400000.0);
/// $b = clone $a; $b->add(IntlCalendar::FIELD_YEAR, 1);
/// var_dump($a->get(IntlCalendar::FIELD_YEAR), $b->get(IntlCalendar::FIELD_YEAR), get_class(clone new IntlGregorianCalendar));
/// try { clone (new ReflectionClass('IntlCalendar'))->newInstanceWithoutConstructor(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2013)
/// int(2014)
/// string(21) "IntlGregorianCalendar"
/// Cannot clone uninitialized IntlCalendar
/// ```
fn clone_calendar(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let calendar = held(req, object).map(|c| cal::php_intl_cal_clone(c));
    let constructed = calendar.is_some();
    let payload = CalendarObject {
        error: IntlError::default(),
        cal: calendar.flatten(),
    };
    let ok = payload.cal.is_some();
    let copy = clone_with(req, &object, payload)?;
    if ok {
        return Ok(copy);
    }
    copy.give_up(req);
    let message = if constructed {
        "Failed to clone IntlCalendar"
    } else {
        "Cannot clone uninitialized IntlCalendar"
    };
    Err(throw_error(req, ce.error, message))
}

/// `class IntlCalendar`: one ICU calendar, made by its static factories
/// and never by `new`, and its 39 field, day and wall time constants.
///
/// ```php
/// var_dump(IntlCalendar::FIELD_FIELD_COUNT, IntlCalendar::FIELD_DAY_OF_MONTH, IntlCalendar::DOW_SATURDAY, IntlCalendar::WALLTIME_NEXT_VALID);
/// try { new IntlCalendar; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump((new ReflectionClass('IntlCalendar'))->newInstanceWithoutConstructor());
/// $c = IntlCalendar::createInstance('UTC', 'en_US'); $c->setTime(0.0);
/// $d = (array) $c; unset($d['fields']); var_dump($d);
/// ```
/// ```expect
/// int(24)
/// int(5)
/// int(7)
/// int(2)
/// Call to private IntlCalendar::__construct() from global scope
/// object(IntlCalendar)#3 (1) {
///   ["valid"]=>
///   bool(false)
/// }
/// array(0) {
/// }
/// ```
#[php_class(not_serializable, size = 80, payload = CalendarObject, handlers = crate::calendar::HANDLERS)]
pub(crate) struct IntlCalendar;

/// The methods, each the `intlcal_*` function it aliases.
///
/// ```php
/// $c = IntlCalendar::createInstance('Europe/Lisbon', 'pt_PT'); $c->setTime(1356998400000.0);
/// $o = IntlCalendar::fromDateTime('2013-07-01 12:00:00 UTC');
/// var_dump($c->equals($o), $c->before($o), $c->after($o), $c->isEquivalentTo($o), $c->fieldDifference(1372680000000.0, IntlCalendar::FIELD_MONTH), $c->getTime());
/// var_dump($c->add(IntlCalendar::FIELD_DATE, 1), $c->get(IntlCalendar::FIELD_DATE), $c->roll(IntlCalendar::FIELD_HOUR_OF_DAY, -1), $c->get(IntlCalendar::FIELD_HOUR_OF_DAY), $c->clear(IntlCalendar::FIELD_MINUTE), $c->isSet(IntlCalendar::FIELD_MINUTE));
/// var_dump($c->getActualMaximum(IntlCalendar::FIELD_DATE), $c->getActualMinimum(IntlCalendar::FIELD_DATE), $c->getGreatestMinimum(IntlCalendar::FIELD_DATE), $c->getLeastMaximum(IntlCalendar::FIELD_DATE), $c->getMaximum(IntlCalendar::FIELD_DATE), $c->getMinimum(IntlCalendar::FIELD_DATE));
/// var_dump(in_array('pt_PT', IntlCalendar::getAvailableLocales()), $c->getDayOfWeekType(IntlCalendar::DOW_SUNDAY), $c->getErrorCode(), $c->getErrorMessage(), $c->getFirstDayOfWeek(), iterator_count(IntlCalendar::getKeywordValuesForLocale('calendar', 'pt_PT', true)) > 0);
/// var_dump($c->getLocale(1), $c->getMinimalDaysInFirstWeek(), $c->setMinimalDaysInFirstWeek(4), IntlCalendar::getNow() > 0, $c->getRepeatedWallTimeOption(), $c->getSkippedWallTimeOption(), $c->getTimeZone()->getID(), $c->getType());
/// var_dump($c->getWeekendTransition(IntlCalendar::DOW_SUNDAY), $c->inDaylightTime(), $c->isLenient(), $c->isWeekend(), $c->set(IntlCalendar::FIELD_YEAR, 2020), $c->setFirstDayOfWeek(IntlCalendar::DOW_MONDAY), $c->setLenient(false));
/// var_dump($c->setRepeatedWallTimeOption(IntlCalendar::WALLTIME_LAST), $c->setSkippedWallTimeOption(IntlCalendar::WALLTIME_NEXT_VALID), $c->setTime(0.0), $c->setTimeZone('Asia/Tokyo'), $c->toDateTime()->format('c e'));
/// $c->setDate(2001, 1, 2); $c->setDateTime(2001, 1, 2, 3, 4); var_dump($c->get(IntlCalendar::FIELD_HOUR_OF_DAY));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// int(6)
/// float(1372633200000)
/// bool(true)
/// int(2)
/// bool(true)
/// int(23)
/// bool(true)
/// bool(false)
/// int(31)
/// int(1)
/// int(1)
/// int(28)
/// int(31)
/// int(1)
/// bool(true)
/// int(1)
/// int(0)
/// string(12) "U_ZERO_ERROR"
/// int(1)
/// bool(true)
/// string(5) "pt_PT"
/// int(4)
/// bool(true)
/// bool(true)
/// int(0)
/// int(0)
/// string(13) "Europe/Lisbon"
/// string(9) "gregorian"
/// int(86400000)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// string(36) "1970-01-01T09:00:00+09:00 Asia/Tokyo"
/// int(3)
/// ```
#[php_class]
impl IntlCalendar {
    const FIELD_ERA: i64 = 0;
    const FIELD_YEAR: i64 = 1;
    const FIELD_MONTH: i64 = 2;
    const FIELD_WEEK_OF_YEAR: i64 = 3;
    const FIELD_WEEK_OF_MONTH: i64 = 4;
    const FIELD_DATE: i64 = 5;
    const FIELD_DAY_OF_YEAR: i64 = 6;
    const FIELD_DAY_OF_WEEK: i64 = 7;
    const FIELD_DAY_OF_WEEK_IN_MONTH: i64 = 8;
    const FIELD_AM_PM: i64 = 9;
    const FIELD_HOUR: i64 = 10;
    const FIELD_HOUR_OF_DAY: i64 = 11;
    const FIELD_MINUTE: i64 = 12;
    const FIELD_SECOND: i64 = 13;
    const FIELD_MILLISECOND: i64 = 14;
    const FIELD_ZONE_OFFSET: i64 = 15;
    const FIELD_DST_OFFSET: i64 = 16;
    const FIELD_YEAR_WOY: i64 = 17;
    const FIELD_DOW_LOCAL: i64 = 18;
    const FIELD_EXTENDED_YEAR: i64 = 19;
    const FIELD_JULIAN_DAY: i64 = 20;
    const FIELD_MILLISECONDS_IN_DAY: i64 = 21;
    const FIELD_IS_LEAP_MONTH: i64 = 22;
    const FIELD_FIELD_COUNT: i64 = 24;
    const FIELD_DAY_OF_MONTH: i64 = 5;
    const DOW_SUNDAY: i64 = 1;
    const DOW_MONDAY: i64 = 2;
    const DOW_TUESDAY: i64 = 3;
    const DOW_WEDNESDAY: i64 = 4;
    const DOW_THURSDAY: i64 = 5;
    const DOW_FRIDAY: i64 = 6;
    const DOW_SATURDAY: i64 = 7;
    const DOW_TYPE_WEEKDAY: i64 = 0;
    const DOW_TYPE_WEEKEND: i64 = 1;
    const DOW_TYPE_WEEKEND_OFFSET: i64 = 2;
    const DOW_TYPE_WEEKEND_CEASE: i64 = 3;
    const WALLTIME_FIRST: i64 = 1;
    const WALLTIME_LAST: i64 = 0;
    const WALLTIME_NEXT_VALID: i64 = 2;

    /// `__construct()`: private, and php's `Exception` where reflection
    /// reaches it.
    ///
    /// ```php
    /// $m = new ReflectionMethod('IntlCalendar', '__construct');
    /// try { $m->invoke((new ReflectionClass('IntlCalendar'))->newInstanceWithoutConstructor()); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// An object of this type cannot be created with the new operator
    /// ```
    #[php(private)]
    fn __construct(req: &mut Request, _this: This<Self>) -> Result<(), Thrown> {
        let ce = crate::throwables::Throwables::of(req)?;
        Err(throw(
            req,
            ce.exception,
            "An object of this type cannot be created with the new operator",
        ))
    }

    #[php(tentative, before_args = error::reset)]
    fn createInstance(
        req: &mut Request,
        #[php(default = null)] timezone: Raw<ZoneArg>,
        #[php(default = null)] locale: Option<PhpString>,
    ) -> Result<Option<Owner<Instance<IntlCalendar>>>, Thrown> {
        zone_raw(req, &timezone, 1, |req, timezone| {
            create_instance(req, timezone, locale)
        })
    }

    #[php(tentative, before_args = error::reset)]
    fn equals(
        req: &mut Request,
        this: This<Self>,
        other: Instance<IntlCalendar>,
    ) -> Result<bool, Thrown> {
        compare(req, Of::method(this), other, 0)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn fieldDifference(
        req: &mut Request,
        this: This<Self>,
        timestamp: f64,
        field: i64,
    ) -> Answer<i64> {
        field_difference(req, Of::method(this), timestamp, field)
    }

    #[php(tentative, before_args = error::reset)]
    fn add(req: &mut Request, this: This<Self>, field: i64, value: i64) -> Result<bool, Thrown> {
        add(req, Of::method(this), field, value)
    }

    #[php(tentative, before_args = error::reset)]
    fn after(
        req: &mut Request,
        this: This<Self>,
        other: Instance<IntlCalendar>,
    ) -> Result<bool, Thrown> {
        compare(req, Of::method(this), other, 2)
    }

    #[php(tentative, before_args = error::reset)]
    fn before(
        req: &mut Request,
        this: This<Self>,
        other: Instance<IntlCalendar>,
    ) -> Result<bool, Thrown> {
        compare(req, Of::method(this), other, 1)
    }

    #[php(tentative, before_args = error::reset)]
    fn clear(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] field: Option<i64>,
    ) -> Result<True, Thrown> {
        clear(req, Of::method(this), field)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn fromDateTime(
        req: &mut Request,
        datetime: php_ext::abi::Union<Instance<DateTime>, PhpString>,
        #[php(default = null)] locale: Option<PhpString>,
    ) -> Result<Option<Owner<Instance<IntlCalendar>>>, Thrown> {
        from_date_time(req, datetime, locale)
    }

    #[php(tentative, before_args = error::reset)]
    fn get(req: &mut Request, this: This<Self>, field: i64) -> Answer<i64> {
        field_int(req, Of::method(this), GET, field)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getActualMaximum(req: &mut Request, this: This<Self>, field: i64) -> Answer<i64> {
        field_int(req, Of::method(this), ACTUAL_MAXIMUM, field)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getActualMinimum(req: &mut Request, this: This<Self>, field: i64) -> Answer<i64> {
        field_int(req, Of::method(this), ACTUAL_MINIMUM, field)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getAvailableLocales(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
        available_locales(req)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getDayOfWeekType(req: &mut Request, this: This<Self>, dayOfWeek: i64) -> Answer<i64> {
        day_of_week_call(req, Of::method(this), 0, dayOfWeek)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getErrorCode(req: &mut Request, this: This<Self>) -> OrFalse<i64> {
        error_code(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getErrorMessage(req: &mut Request, this: This<Self>) -> Answer<Owner<PhpString>> {
        error_message(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getFirstDayOfWeek(req: &mut Request, this: This<Self>) -> Answer<i64> {
        int_call(req, Of::method(this), 0, "Call to ICU method has failed")
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getGreatestMinimum(req: &mut Request, this: This<Self>, field: i64) -> Answer<i64> {
        field_int(req, Of::method(this), GREATEST_MINIMUM, field)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getKeywordValuesForLocale(
        req: &mut Request,
        keyword: PhpString,
        locale: PhpString,
        onlyCommon: bool,
    ) -> Answer<Owner<Instance<iterator::IntlIterator>>> {
        keyword_values(req, keyword, locale, onlyCommon)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getLeastMaximum(req: &mut Request, this: This<Self>, field: i64) -> Answer<i64> {
        field_int(req, Of::method(this), LEAST_MAXIMUM, field)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getLocale(req: &mut Request, this: This<Self>, r#type: i64) -> Answer<Owner<PhpString>> {
        get_locale(req, Of::method(this), r#type)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getMaximum(req: &mut Request, this: This<Self>, field: i64) -> Answer<i64> {
        field_int(req, Of::method(this), MAXIMUM, field)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getMinimalDaysInFirstWeek(req: &mut Request, this: This<Self>) -> Answer<i64> {
        int_call(req, Of::method(this), 1, "Call to ICU method has failed")
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn setMinimalDaysInFirstWeek(
        req: &mut Request,
        this: This<Self>,
        days: i64,
    ) -> Result<True, Thrown> {
        set_int(req, Of::method(this), 2, days)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getMinimum(req: &mut Request, this: This<Self>, field: i64) -> Answer<i64> {
        field_int(req, Of::method(this), MINIMUM, field)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getNow() -> f64 {
        now()
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getRepeatedWallTimeOption(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        plain_int(req, Of::method(this), 3)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getSkippedWallTimeOption(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        plain_int(req, Of::method(this), 4)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getTime(req: &mut Request, this: This<Self>) -> Answer<f64> {
        get_time(req, Of::method(this))
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getTimeZone(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<Owner<Instance<timezone::IntlTimeZone>>> {
        get_time_zone(req, Of::method(this))
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getType(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        get_type(req, Of::method(this))
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getWeekendTransition(req: &mut Request, this: This<Self>, dayOfWeek: i64) -> Answer<i64> {
        day_of_week_call(req, Of::method(this), 1, dayOfWeek)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn inDaylightTime(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        in_daylight_time(req, Of::method(this))
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn isEquivalentTo(
        req: &mut Request,
        this: This<Self>,
        other: Instance<IntlCalendar>,
    ) -> Result<bool, Thrown> {
        compare(req, Of::method(this), other, 3)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn isLenient(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        plain_int(req, Of::method(this), 2).map(|n| n != 0)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn isWeekend(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] timestamp: Option<f64>,
    ) -> Result<bool, Thrown> {
        is_weekend(req, Of::method(this), timestamp)
    }

    #[php(tentative, before_args = error::reset)]
    fn roll(
        req: &mut Request,
        this: This<Self>,
        field: i64,
        value: Untyped<Value>,
    ) -> Result<bool, Thrown> {
        roll(req, Of::method(this), field, &value.0)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn isSet(req: &mut Request, this: This<Self>, field: i64) -> Result<bool, Thrown> {
        is_set(req, Of::method(this), field)
    }

    /// `set()`: more than two arguments deprecated before the arguments
    /// are counted or read, as php raises it ahead of its parsing.
    ///
    /// ```php
    /// $c = IntlCalendar::createInstance('UTC');
    /// try { $c->set(1, 2, 3, 4, 5, 6, 7); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($c->set(IntlCalendar::FIELD_YEAR, 2001), $c->get(IntlCalendar::FIELD_YEAR));
    /// ```
    /// ```expect
    ///
    /// Deprecated: Calling IntlCalendar::set() with more than 2 arguments is deprecated, use either IntlCalendar::setDate() or IntlCalendar::setDateTime() instead in %s on line 3
    /// IntlCalendar::set() expects at most 6 arguments, 7 given
    /// bool(true)
    /// int(2001)
    /// ```
    #[php(tentative, arity = manual, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn set(
        req: &mut Request,
        this: This<Self>,
        year: Raw<i64>,
        month: Raw<i64>,
        dayOfMonth: Passed<Raw<i64>>,
        hour: Passed<Raw<i64>>,
        minute: Passed<Raw<i64>>,
        second: Passed<Raw<i64>>,
    ) -> Result<True, Thrown> {
        if req.frames().argument_count() > 2 {
            let message = "Calling IntlCalendar::set() with more than 2 arguments is deprecated, use either IntlCalendar::setDate() or IntlCalendar::setDateTime() instead";
            php_ext::raising::deprecated(req, message);
            pending(req)?;
        }
        arity(req, 2, Some(6))?;
        let first = [year.hold(req)?, month.hold(req)?];
        let mut rest = [Passed::Omitted; 4];
        for (slot, given) in rest.iter_mut().zip([dayOfMonth, hour, minute, second]) {
            if let Passed::Given(raw) = given {
                *slot = Passed::Given(raw.hold(req)?);
            }
        }
        set_fields(req, Of::method(this), first, rest)
    }

    /// `setDate()`: the date set, its arguments numbered from the
    /// calendar as php numbers them.
    ///
    /// ```php
    /// $c = IntlCalendar::createInstance('UTC'); $c->setDate(2012, 1, 29);
    /// var_dump($c->get(IntlCalendar::FIELD_DATE));
    /// try { $c->setDate(PHP_INT_MAX, 1, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(29)
    /// IntlCalendar::setDate(): Argument #1 ($year) must be between -2147483648 and 2147483647
    /// ```
    #[allow(non_snake_case)]
    #[php(before_args = error::reset)]
    fn setDate(
        req: &mut Request,
        this: This<Self>,
        year: i64,
        month: i64,
        dayOfMonth: i64,
    ) -> Result<(), Thrown> {
        let o = Of::method(this);
        let mut f = [0; 6];
        for (i, v) in [year, month, dayOfMonth].into_iter().enumerate() {
            f[i] = int32(req, v, o.at(2 + u32::try_from(i).unwrap_or(0)))?;
        }
        with(req, o, |c| cal::php_intl_cal_set_date(c, 3, &f))
    }

    /// `setDateTime()`: the date and time set, the seconds where given.
    ///
    /// ```php
    /// $c = IntlCalendar::createInstance('UTC'); $c->setDateTime(2012, 1, 29, 13, 14, 15);
    /// var_dump($c->get(IntlCalendar::FIELD_SECOND));
    /// try { $c->setDateTime(1, 1, 1, 1, 1, PHP_INT_MIN); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(15)
    /// IntlCalendar::setDateTime(): Argument #6 ($second) must be between -2147483648 and 2147483647
    /// ```
    #[allow(non_snake_case)]
    #[php(before_args = error::reset)]
    fn setDateTime(
        req: &mut Request,
        this: This<Self>,
        year: i64,
        month: i64,
        dayOfMonth: i64,
        hour: i64,
        minute: i64,
        #[php(default = null)] second: Option<i64>,
    ) -> Result<(), Thrown> {
        let o = Of::method(this);
        let mut f = [0; 6];
        for (i, v) in [year, month, dayOfMonth, hour, minute]
            .into_iter()
            .enumerate()
        {
            f[i] = int32(req, v, o.at(2 + u32::try_from(i).unwrap_or(0)))?;
        }
        fetch(req, o)?;
        let n = match second {
            None => 5,
            Some(s) => {
                f[5] = int32(req, s, o.at(7))?;
                6
            }
        };
        with(req, o, |c| cal::php_intl_cal_set_date(c, n, &f))
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn setFirstDayOfWeek(
        req: &mut Request,
        this: This<Self>,
        dayOfWeek: i64,
    ) -> Result<True, Thrown> {
        set_int(req, Of::method(this), 0, dayOfWeek)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn setLenient(req: &mut Request, this: This<Self>, lenient: bool) -> Result<True, Thrown> {
        set_int(req, Of::method(this), 1, i64::from(lenient))
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn setRepeatedWallTimeOption(
        req: &mut Request,
        this: This<Self>,
        option: i64,
    ) -> Result<True, Thrown> {
        set_int(req, Of::method(this), 3, option)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn setSkippedWallTimeOption(
        req: &mut Request,
        this: This<Self>,
        option: i64,
    ) -> Result<True, Thrown> {
        set_int(req, Of::method(this), 4, option)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn setTime(req: &mut Request, this: This<Self>, timestamp: f64) -> Result<bool, Thrown> {
        set_time(req, Of::method(this), timestamp)
    }

    /// `setTimeZone()`, which php declares apart from
    /// `intlcal_set_time_zone` with the same body: null leaves the zone
    /// as it is.
    ///
    /// ```php
    /// $c = IntlCalendar::createInstance('UTC');
    /// var_dump($c->setTimeZone(null), $c->getTimeZone()->getID(), $c->setTimeZone(new DateTimeZone('+01:00')), $c->getTimeZone()->getID());
    /// try { $c->setTimeZone([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { $c->setTimeZone(new stdClass); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// try { intlcal_create_instance([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// string(3) "UTC"
    /// bool(true)
    /// string(9) "GMT+01:00"
    /// IntlCalendar::setTimeZone(): Argument #1 ($timezone) must be of type object|string|null, array given
    /// Error: Object of class stdClass could not be converted to string
    /// intlcal_create_instance(): Argument #1 ($timezone) must be of type object|string|null, array given
    /// ```
    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn setTimeZone(
        req: &mut Request,
        this: This<Self>,
        timezone: Raw<ZoneArg>,
    ) -> Result<bool, Thrown> {
        zone_raw(req, &timezone, 1, |req, timezone| {
            set_time_zone(req, Of::method(this), timezone)
        })
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn toDateTime(req: &mut Request, this: This<Self>) -> Answer<Owner<Instance<DateTime>>> {
        to_date_time(req, Of::method(this))
    }
}

/// `intlcal_create_instance`: a calendar of the locale in the zone, null
/// where the zone cannot be had or ICU makes none.
fn create_instance(
    req: &mut Request,
    zone: ZoneArg,
    locale: Option<PhpString>,
) -> Result<Option<Owner<Instance<IntlCalendar>>>, Thrown> {
    let Some(zone) = process_timezone_argument(req, zone, &mut Outside::Request)? else {
        return Ok(None);
    };
    let locale = locale_name(req, locale);
    let mut status = Code::ZERO;
    if let Some(calendar) =
        cal::php_intl_cal_create(zone, CName::from(locale.as_c_str()), &mut status)
    {
        return made(req, calendar).map(Some);
    }
    error::set(req, status, "Error creating ICU Calendar object")?;
    Ok(None)
}

/// `intlcal_get_keyword_values_for_locale`.
fn keyword_values(
    req: &mut Request,
    key: PhpString,
    locale: PhpString,
    common: bool,
) -> Answer<Owner<Instance<iterator::IntlIterator>>> {
    let (key, locale) = (c_string(key.as_bytes()), c_string(locale.as_bytes()));
    let mut status = Code::ZERO;
    let values = cal::keyword_values(
        CName::from(key.as_c_str()),
        CName::from(locale.as_c_str()),
        common,
        &mut status,
    );
    match values {
        Some(values) => {
            iterator::from_enumeration(req, values).map(|o| OrFalse::Value(o.into_instance()))
        }
        None => or_false(Err(error::set(
            req,
            status,
            "error calling underlying method",
        )
        .err())),
    }
}

/// `intlcal_get_available_locales`.
fn available_locales(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let mut list = PhpArray::new(req)?;
    for name in cal::available_locales() {
        let name = PhpString::new(req, name.as_bytes())?.into_value();
        list.append(req, name)?;
    }
    Ok(list)
}

/// `_php_intlcal_field_uec_ret_in32t_method` and
/// `_php_intlcal_field_ret_in32t_method`: a field's value or bound.
fn field_int(req: &mut Request, o: Of, which: i32, value: i64) -> Answer<i64> {
    let f = field(req, value, o.at(2))?;
    let answer = with(req, o, |c| {
        let mut status = Code::ZERO;
        (
            i64::from(cal::php_intl_cal_field(c, which, f, 0, 0.0, &mut status)),
            status,
        )
    })?;
    checked(req, o, answer, "Call to ICU method has failed")
}

/// `intlcal_field_difference`.
fn field_difference(req: &mut Request, o: Of, when: f64, value: i64) -> Answer<i64> {
    let f = field(req, value, o.at(3))?;
    let answer = with(req, o, |c| {
        let mut status = Code::ZERO;
        (
            i64::from(cal::php_intl_cal_field(
                c,
                DIFFERENCE,
                f,
                0,
                when,
                &mut status,
            )),
            status,
        )
    })?;
    checked(req, o, answer, "Call to ICU method has failed")
}

/// `intlcal_add`.
fn add(req: &mut Request, o: Of, value: i64, amount: i64) -> Result<bool, Thrown> {
    let f = field(req, value, o.at(2))?;
    let amount = int32(req, amount, o.at(3))?;
    let status = with(req, o, |c| {
        let mut status = Code::ZERO;
        cal::php_intl_cal_field(c, ADD, f, amount, 0.0, &mut status);
        status
    })?;
    or_false(check(req, o, status, "Call to underlying method failed"))
        .map(|a| matches!(a, OrFalse::Value(())))
}

/// `intlcal_roll`: a bool deprecated and read as one step up or down.
fn roll(req: &mut Request, o: Of, value: i64, amount: &Value) -> Result<bool, Thrown> {
    fetch(req, o)?;
    let f = field(req, value, o.at(2))?;
    let amount = if let ValueRef::Bool(up) = amount.as_ref() {
        let message = "Passing bool is deprecated, use 1 or -1 instead";
        docref(req, Level::Deprecated, message);
        if up { 1 } else { -1 }
    } else {
        let n = to_long(req, amount)?;
        int32(req, n, o.at(3))?
    };
    let status = with(req, o, |c| {
        let mut status = Code::ZERO;
        cal::php_intl_cal_field(c, ROLL, f, amount, 0.0, &mut status);
        status
    })?;
    or_false(check(req, o, status, "Error calling ICU Calendar::roll"))
        .map(|a| matches!(a, OrFalse::Value(())))
}

/// `intlcal_clear`: every field, or one.
fn clear(req: &mut Request, o: Of, value: Option<i64>) -> Result<True, Thrown> {
    fetch(req, o)?;
    match value {
        None => with(req, o, |c| cal::php_intl_cal_int(c, 8, 0, &mut Code(0)))?,
        Some(value) => {
            let f = field(req, value, o.at(2))?;
            with(req, o, |c| {
                cal::php_intl_cal_field(c, CLEAR, f, 0, 0.0, &mut Code(0))
            })?
        }
    };
    Ok(True)
}

/// `intlcal_is_set`.
fn is_set(req: &mut Request, o: Of, value: i64) -> Result<bool, Thrown> {
    let f = field(req, value, o.at(2))?;
    with(req, o, |c| {
        cal::php_intl_cal_field(c, IS_SET, f, 0, 0.0, &mut Code(0)) != 0
    })
}

/// `intlcal_set`: a field and its value, or a date with or without its
/// time.
fn set_fields(
    req: &mut Request,
    o: Of,
    first: [i64; 2],
    rest: [Passed<i64>; 4],
) -> Result<True, Thrown> {
    let mut given = first.to_vec();
    given.extend(rest.iter().filter_map(|p| match p {
        Passed::Given(n) => Some(*n),
        Passed::Omitted => None,
    }));
    let mut f = [0; 6];
    for (i, v) in given.iter().enumerate() {
        f[i] = int32(req, *v, o.at(2 + u32::try_from(i).unwrap_or(0)))?;
    }
    fetch(req, o)?;
    match given.len() {
        2 => {
            let which = field(req, given[0], o.at(2))?;
            with(req, o, |c| {
                cal::php_intl_cal_field(c, SET, which, f[1], 0.0, &mut Code(0))
            })?;
        }
        4 => {
            let message = "IntlCalendar::set() has no variant with exactly 4 parameters";
            return Err(argument_count_error(req, message));
        }
        n => {
            let n = i32::try_from(n).unwrap_or(6);
            with(req, o, |c| cal::php_intl_cal_set_date(c, n, &f))?;
        }
    }
    Ok(True)
}

/// `intlcal_get_day_of_week_type` and `intlcal_get_weekend_transition`.
fn day_of_week_call(req: &mut Request, o: Of, which: i32, value: i64) -> Answer<i64> {
    let dow = day_of_week(req, value, o.at(2))?;
    let answer = with(req, o, |c| {
        let mut status = Code::ZERO;
        (
            i64::from(cal::php_intl_cal_dow(c, which, dow, &mut status)),
            status,
        )
    })?;
    let message = if which == 0 {
        "Call to ICU method has failed"
    } else {
        "Error calling ICU method"
    };
    checked(req, o, answer, message)
}

/// `intlcal_get_first_day_of_week` and
/// `intlcal_get_minimal_days_in_first_week`.
fn int_call(req: &mut Request, o: Of, which: i32, message: &str) -> Answer<i64> {
    let answer = with(req, o, |c| {
        let mut status = Code::ZERO;
        (
            i64::from(cal::php_intl_cal_int(c, which, 0, &mut status)),
            status,
        )
    })?;
    checked(req, o, answer, message)
}

/// A read php answers whole: `isLenient`, and the two wall time options.
fn plain_int(req: &mut Request, o: Of, which: i32) -> Result<i64, Thrown> {
    with(req, o, |c| {
        i64::from(cal::php_intl_cal_int(c, which, 0, &mut Code(0)))
    })
}

/// `intlcal_in_daylight_time`.
fn in_daylight_time(req: &mut Request, o: Of) -> Result<bool, Thrown> {
    let answer = int_call(req, o, 5, "Error calling ICU method")?;
    Ok(matches!(answer, OrFalse::Value(n) if n != 0))
}

/// `intlcal_is_weekend`: now, or at an instant.
fn is_weekend(req: &mut Request, o: Of, when: Option<f64>) -> Result<bool, Thrown> {
    let Some(when) = when else {
        return with(req, o, |c| {
            cal::php_intl_cal_int(c, 6, 0, &mut Code(0)) != 0
        });
    };
    let answer = with(req, o, |c| {
        let mut status = Code::ZERO;
        (
            cal::php_intl_cal_time(c, 2, when, &mut status) != 0.0,
            status,
        )
    })?;
    Ok(matches!(
        checked(req, o, answer, "Error calling ICU method")?,
        OrFalse::Value(true)
    ))
}

/// The writes of a setting php checks first: the first day of the week,
/// leniency, the minimal days in the first week and the wall time
/// options.
fn set_int(req: &mut Request, o: Of, which: i32, value: i64) -> Result<True, Thrown> {
    let refused = match which {
        0 => {
            let day = day_of_week(req, value, o.at(2))?;
            return set_checked(req, o, which, day);
        }
        2 => !(1..=7).contains(&value),
        3 => value != 0 && value != 1,
        4 => !(0..=2).contains(&value),
        _ => false,
    };
    if refused {
        let message = match which {
            2 => "be between 1 and 7",
            3 => "be either IntlCalendar::WALLTIME_FIRST or IntlCalendar::WALLTIME_LAST",
            _ => {
                "be one of IntlCalendar::WALLTIME_FIRST, IntlCalendar::WALLTIME_LAST, or IntlCalendar::WALLTIME_NEXT_VALID"
            }
        };
        return Err(arg_value_error(req, o.at(2), message));
    }
    set_checked(req, o, which, i32::try_from(value).unwrap_or(0))
}

/// `php_intl_cal_set_int` on a fetched calendar, the value checked.
fn set_checked(req: &mut Request, o: Of, which: i32, value: i32) -> Result<True, Thrown> {
    with(req, o, |c| {
        cal::php_intl_cal_set_int(c, which, value, &mut Code(0));
    })?;
    Ok(True)
}

/// `intlcal_get_time`.
fn get_time(req: &mut Request, o: Of) -> Answer<f64> {
    let answer = with(req, o, |c| {
        let mut status = Code::ZERO;
        (cal::php_intl_cal_time(c, 0, 0.0, &mut status), status)
    })?;
    checked(req, o, answer, "error calling ICU Calendar::getTime")
}

/// `intlcal_set_time`.
fn set_time(req: &mut Request, o: Of, when: f64) -> Result<bool, Thrown> {
    let answer = with(req, o, |c| {
        let mut status = Code::ZERO;
        cal::php_intl_cal_time(c, 1, when, &mut status);
        ((), status)
    })?;
    Ok(matches!(
        checked(req, o, answer, "Call to underlying method failed")?,
        OrFalse::Value(())
    ))
}

/// `intlcal_set_time_zone`: null leaves the zone; one that cannot be
/// had is `false`, with the error on the object.
fn set_time_zone(req: &mut Request, o: Of, zone: ZoneArg) -> Result<bool, Thrown> {
    fetch(req, o)?;
    if zone.is_none() {
        return Ok(true);
    }
    let Some(zone) =
        process_timezone_argument(req, zone, &mut Outside::of::<CalendarObject>(o.object))?
    else {
        return Ok(false);
    };
    with(req, o, |c| cal::php_intl_cal_adopt_zone(c, zone))?;
    Ok(true)
}

/// `_php_intlcal_before_after`, `intlcal_equals` and
/// `intlcal_is_equivalent_to`: php's `Error` for an unconstructed other
/// calendar, checked before the object for `isEquivalentTo` alone.
fn compare(
    req: &mut Request,
    o: Of,
    other: Instance<IntlCalendar>,
    which: i32,
) -> Result<bool, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let uninitialized =
        |req: &mut Request| argument_error(req, ce.error, o.at(2), "is uninitialized");
    if which == 3 && held(req, other.object()).is_none() {
        return Err(uninitialized(req));
    }
    fetch(req, o)?;
    let (Some(a), Some(b)) = (held(req, o.object), held(req, other.object())) else {
        return Err(uninitialized(req));
    };
    let mut status = Code::ZERO;
    let answer = cal::php_intl_cal_compare(a, b, which, &mut status);
    if which == 3 {
        return Ok(answer);
    }
    let message = if which == 0 {
        "error calling ICU Calendar::equals"
    } else {
        "Error calling ICU method"
    };
    Ok(matches!(
        checked(req, o, (answer, status), message)?,
        OrFalse::Value(true)
    ))
}

/// `intlcal_get_locale`: the actual or the valid locale.
fn get_locale(req: &mut Request, o: Of, kind: i64) -> Answer<Owner<PhpString>> {
    if kind != 0 && kind != 1 {
        let message = "be either Locale::ACTUAL_LOCALE or Locale::VALID_LOCALE";
        return Err(arg_value_error(req, o.at(2), message));
    }
    let answer = with(req, o, |c| {
        let (mut out, mut status) = ([0u8; LOCALE_CAPACITY], Code::ZERO);
        cal::php_intl_cal_locale(c, i32::from(kind == 1), &mut out, &mut status);
        (c_text(&out), status)
    })?;
    match checked(req, o, answer, "Call to ICU method has failed")? {
        OrFalse::Value(name) => PhpString::new(req, &name).map(OrFalse::Value),
        OrFalse::False => Ok(OrFalse::False),
    }
}

/// `intlcal_get_time_zone`: a clone of the calendar's zone.
fn get_time_zone(req: &mut Request, o: Of) -> Answer<Owner<Instance<timezone::IntlTimeZone>>> {
    if let Some(zone) = with(req, o, |c| cal::php_intl_cal_zone(c))? {
        return timezone::construct(req, zone).map(OrFalse::Value);
    }
    let message = "could not clone TimeZone";
    error::errors_set::<CalendarObject>(req, o.object, Code::MEMORY_ALLOCATION, message)?;
    Ok(OrFalse::False)
}

/// `intlcal_get_type`.
fn get_type(req: &mut Request, o: Of) -> Result<Owner<PhpString>, Thrown> {
    let kind = with(req, o, |c| cal::kind(c))?;
    PhpString::new(req, kind.as_bytes())
}

/// `intlcal_get_error_code`.
fn error_code(req: &mut Request, object: PhpObject) -> OrFalse<i64> {
    OrFalse::Value(i64::from(
        error::object_code::<CalendarObject>(req, object).0,
    ))
}

/// `intlcal_get_error_message`.
fn error_message(req: &mut Request, object: PhpObject) -> Answer<Owner<PhpString>> {
    let message = error::object_message::<CalendarObject>(req, object);
    PhpString::new(req, &message).map(OrFalse::Value)
}

/// `intlcal_from_date_time`: a calendar at a `DateTime`'s instant in its
/// zone, a string made a `DateTime` first; null with the request's
/// error where the time or the zone cannot be had.
fn from_date_time(
    req: &mut Request,
    datetime: php_ext::abi::Union<Instance<DateTime>, PhpString>,
    locale: Option<PhpString>,
) -> Result<Option<Owner<Instance<IntlCalendar>>>, Thrown> {
    let (object, owned) = match datetime {
        php_ext::abi::Union::A(date) => (date.object(), None),
        php_ext::abi::Union::B(text) => {
            let Some(class) = lookup_class(req, b"DateTime")? else {
                return Err(fatal(req, "InternalError: DateTime is not linked"));
            };
            let made = instantiate(req, class, &[Value::string(text)], &[])?;
            (*made, Some(made))
        }
    };
    let made = from_date_object(req, object, locale);
    if let Some(owned) = owned {
        owned.give_up(req);
    }
    made
}

/// `intlcal_from_date_time` over the `DateTime` object.
fn from_date_object(
    req: &mut Request,
    object: PhpObject,
    locale: Option<PhpString>,
) -> Result<Option<Owner<Instance<IntlCalendar>>>, Thrown> {
    let Some(parts) = php_ext_date::date_obj(req, object) else {
        error::set(
            req,
            Code::ILLEGAL_ARGUMENT,
            "DateTime object is unconstructed",
        )?;
        return Ok(None);
    };
    let stamp = call_method(req, &object, b"getTimestamp", &[])?;
    let Some(seconds) = stamp.as_int() else {
        stamp.give_up(req);
        let message = "bad DateTime; call to DateTime::getTimestamp() failed";
        error::set(req, Code::ILLEGAL_ARGUMENT, message)?;
        return Ok(None);
    };
    stamp.give_up(req);
    let zone = match &parts.zone {
        None => gmt(),
        Some(kind) => convert_datetimezone(req, Some(kind), &mut Outside::Request)?,
    };
    let Some(zone) = zone else {
        return Ok(None);
    };
    let locale = locale_name(req, locale);
    let mut status = Code::ZERO;
    let Some(mut calendar) =
        cal::php_intl_cal_create(zone, CName::from(locale.as_c_str()), &mut status)
    else {
        error::set(req, status, "error creating ICU Calendar object")?;
        return Ok(None);
    };
    #[allow(clippy::cast_precision_loss, reason = "php's (UDate) cast")]
    cal::php_intl_cal_time(&mut calendar, 1, seconds as f64 * 1000.0, &mut status);
    if status.is_failure() {
        error::set(req, status, "error creating ICU Calendar::setTime()")?;
        return Ok(None);
    }
    made(req, calendar).map(Some)
}

/// `intlcal_to_date_time`: a `DateTime` at the calendar's second in its
/// zone, through the constructor with `@<seconds>` and then
/// `setTimeZone()`, as php works around bug #40743.
fn to_date_time(req: &mut Request, o: Of) -> Answer<Owner<Instance<DateTime>>> {
    let (millis, status, zone) = with(req, o, |c| {
        let mut status = Code::ZERO;
        let millis = cal::php_intl_cal_time(c, 0, 0.0, &mut status);
        (millis, status, cal::php_intl_cal_zone(c))
    })?;
    if let Err(thrown) = check(req, o, status, "Call to ICU method has failed") {
        return or_false(Err(thrown));
    }
    let date = millis / 1000.0;
    #[allow(clippy::cast_precision_loss, reason = "php's (double) U_INT64_MAX")]
    if date > i64::MAX as f64 || date < i64::MIN as f64 {
        let message = "The calendar date is out of the range for a 64-bit integer";
        error::errors_set::<CalendarObject>(req, o.object, Code::ILLEGAL_ARGUMENT, message)?;
        return Ok(OrFalse::False);
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "php's (int64_t) cast, the range checked above"
    )]
    let seconds = date as i64;
    let Some(zone) = zone else {
        return Ok(OrFalse::False);
    };
    let Some(tz_object) =
        convert_to_datetimezone(req, &zone, &mut Outside::of::<CalendarObject>(o.object))?
    else {
        return Ok(OrFalse::False);
    };
    let made = date_time_at(req, o, seconds, *tz_object);
    tz_object.give_up(req);
    made
}

/// `new DateTime("@<seconds>", $zone)` and its `setTimeZone($zone)`.
fn date_time_at(
    req: &mut Request,
    o: Of,
    seconds: i64,
    zone: PhpObject,
) -> Answer<Owner<Instance<DateTime>>> {
    let Some(class) = lookup_class(req, b"DateTime")? else {
        return Err(fatal(req, "InternalError: DateTime is not linked"));
    };
    let stamp = PhpString::new(req, format!("@{seconds}").as_bytes())?;
    let made = instantiate(
        req,
        class,
        &[Value::string(*stamp), Value::object(zone)],
        &[],
    );
    stamp.give_up(req);
    let date = made?;
    let answer = call_method(req, &date, b"setTimeZone", &[Value::object(zone)]);
    let failed = match &answer {
        Ok(value) => matches!(value.as_ref(), ValueRef::Bool(false) | ValueRef::Undef),
        Err(_) => true,
    };
    if let Ok(value) = answer {
        value.give_up(req);
    } else if let Err(thrown) = answer {
        date.give_up(req);
        return Err(thrown);
    }
    if failed {
        date.give_up(req);
        let message = "call to DateTime::setTimeZone has failed";
        error::errors_set::<CalendarObject>(req, o.object, Code::ILLEGAL_ARGUMENT, message)?;
        return Ok(OrFalse::False);
    }
    Ok(OrFalse::Value(date.into_instance()))
}

/// `class IntlGregorianCalendar`: a Gregorian calendar, made by `new` or
/// by its two static factories.
///
/// ```php
/// ini_set('date.timezone', 'Europe/Lisbon');
/// $g = new IntlGregorianCalendar('UTC', 'en_US');
/// var_dump($g->getTimeZone()->getID(), $g->getLocale(1));
/// $d = IntlGregorianCalendar::createFromDate(2012, 1, 29);
/// $t = IntlGregorianCalendar::createFromDateTime(2012, 1, 29, 13, 14);
/// var_dump($d->get(IntlCalendar::FIELD_DATE), $t->get(IntlCalendar::FIELD_HOUR_OF_DAY), $t->getTimeZone()->getID());
/// var_dump($g->isLeapYear(2012), $g->getGregorianChange(), $g->setGregorianChange(0.0), $g->getGregorianChange());
/// try { new IntlGregorianCalendar(1, 2, 3, 4); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// try { $g->__construct(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "UTC"
/// string(5) "en_US"
/// int(29)
/// int(13)
/// string(13) "Europe/Lisbon"
/// bool(true)
/// float(-12219292800000)
/// bool(true)
/// float(0)
///
/// Deprecated: Calling IntlGregorianCalendar::__construct() with more than 2 arguments is deprecated, use either IntlGregorianCalendar::createFromDate() or IntlGregorianCalendar::createFromDateTime() instead in %s on line 9
/// No variant with 4 arguments (excluding trailing NULLs)
/// IntlGregorianCalendar object is already constructed
/// ```
#[php_class(not_serializable, extends(IntlCalendar))]
pub(crate) struct IntlGregorianCalendar;

#[php_class]
impl IntlGregorianCalendar {
    #[allow(non_snake_case)]
    #[php(before_args = error::reset)]
    fn createFromDate(
        req: &mut Request,
        year: i64,
        month: i64,
        dayOfMonth: i64,
    ) -> Result<Owner<Static>, Thrown> {
        create_from(req, &[year, month, dayOfMonth])
    }

    #[allow(non_snake_case)]
    #[php(before_args = error::reset)]
    fn createFromDateTime(
        req: &mut Request,
        year: i64,
        month: i64,
        dayOfMonth: i64,
        hour: i64,
        minute: i64,
        #[php(default = null)] second: Option<i64>,
    ) -> Result<Owner<Static>, Thrown> {
        match second {
            None => create_from(req, &[year, month, dayOfMonth, hour, minute]),
            Some(second) => create_from(req, &[year, month, dayOfMonth, hour, minute, second]),
        }
    }

    #[php(arity = manual, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        timezoneOrYear: Passed<Untyped<Value>>,
        localeOrMonth: Passed<Untyped<Value>>,
        day: Passed<Untyped<Value>>,
        hour: Passed<Untyped<Value>>,
        minute: Passed<Untyped<Value>>,
        second: Passed<Untyped<Value>>,
    ) -> Result<(), Thrown> {
        let _ = (timezoneOrYear, localeOrMonth, day, hour, minute, second);
        let object = this.object();
        error::constructing(req, |req| gregorian_body(req, object, true).map(|_| ()))
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn setGregorianChange(
        req: &mut Request,
        this: This<Self>,
        timestamp: f64,
    ) -> Result<bool, Thrown> {
        set_gregorian_change(req, Of::this_gregorian(this), timestamp)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getGregorianChange(req: &mut Request, this: This<Self>) -> Result<f64, Thrown> {
        get_gregorian_change(req, Of::this_gregorian(this))
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn isLeapYear(req: &mut Request, this: This<Self>, year: i64) -> Result<bool, Thrown> {
        is_leap_year(req, Of::this_gregorian(this), year)
    }
}

/// `_php_intlgregcal_constructor_body`: from a zone and a locale (none,
/// one or two arguments, trailing nulls dropped) or from three, five or
/// six date and time fields in the default zone; `false` where it made
/// none, with the error set.
fn gregorian_body(req: &mut Request, object: PhpObject, constructor: bool) -> Result<bool, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let count = req.frames().argument_count();
    if constructor && count > 2 {
        let message = "Calling IntlGregorianCalendar::__construct() with more than 2 arguments is deprecated, use either IntlGregorianCalendar::createFromDate() or IntlGregorianCalendar::createFromDateTime() instead";
        php_ext::raising::deprecated(req, message);
        pending(req)?;
    }
    if count > 6 {
        return Err(argument_count_error(req, "Too many arguments"));
    }
    let given: Vec<Value> = (1..=u32::try_from(count).unwrap_or(6))
        .map(|p| {
            req.frames()
                .argument(p)
                .map_or_else(Value::null, |v| v.deref())
        })
        .collect();
    let variant = given
        .iter()
        .rposition(|v| !matches!(v.as_ref(), ValueRef::Null))
        .map_or(0, |i| i + 1);
    if variant == 4 {
        let message = "No variant with 4 arguments (excluding trailing NULLs)";
        return Err(argument_count_error(req, message));
    }
    let calendar = if variant <= 2 {
        // The variant's parse, `Z_PARAM_OBJ_OR_STR_OR_NULL` and
        // `Z_PARAM_STRING_OR_NULL` for up to two arguments and `"lll|lll"`
        // past them, where the stub declares `$timezoneOrYear`,
        // `$localeOrMonth` and the rest untyped.
        let zone_type = Type::Union(vec![Type::Object, Type::String, Type::Null]);
        let zone = match given.first() {
            Some(v) => Some(coerce_param(req, v, &zone_type, 1)?),
            None => None,
        };
        let locale = match given.get(1) {
            Some(v) => coerce_param(req, v, &Type::Nullable(Box::new(Type::String)), 2)?,
            None => Value::null(),
        };
        if held(req, object).is_some() {
            return Err(throw_error(
                req,
                ce.error,
                "IntlGregorianCalendar object is already constructed",
            ));
        }
        let zone = zone_argument(req, zone)?;
        let Some(zone) = process_timezone_argument(req, zone, &mut Outside::Request)? else {
            return Err(throw(req, ce.intl_exception, "Constructor failed"));
        };
        let locale = locale_name(req, locale.as_php_string());
        let mut status = Code::ZERO;
        let made = cal::php_intl_cal_greg_create(
            Some(zone),
            CName::from(locale.as_c_str()),
            0,
            &[0; 6],
            &mut status,
        );
        let Some(calendar) = made else {
            let message = "error creating ICU GregorianCalendar from time zone and locale";
            error::set(req, status, message)?;
            return Ok(false);
        };
        calendar
    } else {
        let mut f = [0; 6];
        for (i, v) in given.iter().enumerate() {
            // `"lll|lll"`'s `l`, over the untyped parameters.
            let n = coerce_param(req, v, &Type::Int, u32::try_from(i + 1).unwrap_or(1))?;
            if i < variant {
                f[i] = int32(
                    req,
                    n.as_int().unwrap_or(0),
                    u32::try_from(i + 1).unwrap_or(1),
                )?;
            }
        }
        if held(req, object).is_some() {
            return Err(throw_error(
                req,
                ce.error,
                "IntlGregorianCalendar object is already constructed",
            ));
        }
        match gregorian_from_fields(req, i32::try_from(variant).unwrap_or(6), &f)? {
            Some(calendar) => calendar,
            None => return Ok(false),
        }
    };
    if let Some(o) = object.native_mut::<CalendarObject>(req) {
        o.cal = Some(calendar);
    }
    Ok(true)
}

/// A `$timezone` argument held as `Z_PARAM_OBJ_OR_STR_OR_NULL` holds it
/// and sorted as `timezone_process_timezone_argument` sorts it, then
/// handed to `then`.
fn zone_raw<R>(
    req: &mut Request,
    zone: &Raw<ZoneArg>,
    position: u32,
    then: impl FnOnce(&mut Request, ZoneArg) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let held = zone_held(req, zone.value(), position)?;
    zone_sorted(req, held, then)
}

/// The zone argument `Z_PARAM_OBJ_OR_STR_OR_NULL` took, sorted into an
/// `IntlTimeZone`, a `DateTimeZone` or a string, another object by its
/// string cast.
fn zone_argument(req: &mut Request, zone: Option<Value>) -> Result<ZoneArg, Thrown> {
    use php_ext::abi::Union;
    let Some(zone) = zone else {
        return Ok(None);
    };
    let zone_union = Type::Union(vec![
        Type::Named(php_ext::decl::Name(b"IntlTimeZone".as_slice().into())),
        Type::Named(php_ext::decl::Name(b"DateTimeZone".as_slice().into())),
        Type::String,
        Type::Null,
    ]);
    // The classes the constructor's `Z_PARAM_OBJ_OR_STR_OR_NULL` is sorted
    // into, where the stub declares `$timezoneOrYear` untyped.
    let held = coerce_param(req, &zone, &zone_union, 1)?;
    Ok(match held.as_ref() {
        ValueRef::Object(object)
            if object
                .native::<crate::timezone::TimeZoneObject>(req)
                .is_some() =>
        {
            Some(Union::A(Instance::of(object)))
        }
        ValueRef::Object(object) => Some(Union::B(Union::A(Instance::of(object)))),
        ValueRef::Str(text) => Some(Union::B(Union::B(text))),
        _ => None,
    })
}

/// `new GregorianCalendar(...)` from 3, 5 or 6 fields, its zone set to
/// php's default (`set_gregorian_calendar_time_zone`); none where ICU
/// fails, with the request's error set.
fn gregorian_from_fields(req: &mut Request, n: i32, f: &[i32; 6]) -> Result<Option<Cal>, Thrown> {
    let mut status = Code::ZERO;
    let empty = c_string(b"");
    let made =
        cal::php_intl_cal_greg_create(None, CName::from(empty.as_c_str()), n, f, &mut status);
    let Some(mut calendar) = made else {
        error::set(
            req,
            status,
            "Error creating ICU GregorianCalendar from date",
        )?;
        return Ok(None);
    };
    let name = php_ext_date::get_timezone_info(req)?.name().to_vec();
    let Ok(units) = to_utf16(&name) else {
        let message = "Could not create UTF-8 string from PHP's default timezone name (see date_default_timezone_get())";
        error::set(req, Code::ILLEGAL_ARGUMENT, message)?;
        return Ok(None);
    };
    if let Some(zone) = tz::php_intl_tz_create(Text::from(units.as_slice())) {
        cal::php_intl_cal_adopt_zone(&mut calendar, zone);
    }
    Ok(Some(calendar))
}

/// `IntlGregorianCalendar::createFromDate` and `createFromDateTime`.
fn create_from(req: &mut Request, fields: &[i64]) -> Result<Owner<Static>, Thrown> {
    let mut f = [0; 6];
    for (i, v) in fields.iter().enumerate() {
        f[i] = int32(req, *v, u32::try_from(i + 1).unwrap_or(1))?;
    }
    let n = i32::try_from(fields.len()).unwrap_or(6);
    let Some(calendar) = gregorian_from_fields(req, n, &f)? else {
        return Err(fatal(req, "InternalError: ICU made no GregorianCalendar"));
    };
    store(req, b"IntlGregorianCalendar", calendar).map(Owner::into_static)
}

/// `intlgregcal_set_gregorian_change`.
fn set_gregorian_change(req: &mut Request, o: Of, when: f64) -> Result<bool, Thrown> {
    let answer = with(req, o, |c| {
        let mut status = Code::ZERO;
        cal::php_intl_cal_time(c, 4, when, &mut status);
        ((), status)
    })?;
    Ok(matches!(
        checked(req, o, answer, "error calling ICU method")?,
        OrFalse::Value(())
    ))
}

/// `intlgregcal_get_gregorian_change`.
fn get_gregorian_change(req: &mut Request, o: Of) -> Result<f64, Thrown> {
    with(req, o, |c| cal::php_intl_cal_time(c, 3, 0.0, &mut Code(0)))
}

/// `intlgregcal_is_leap_year`.
fn is_leap_year(req: &mut Request, o: Of, year: i64) -> Result<bool, Thrown> {
    let year = int32(req, year, o.at(2))?;
    with(req, o, |c| {
        cal::php_intl_cal_int(c, 7, year, &mut Code(0)) != 0
    })
}

/// `intlgregcal_create_instance`: the constructor's body over a new
/// `IntlGregorianCalendar`, null where it made none.
fn gregorian_create_instance(
    req: &mut Request,
) -> Result<Option<Owner<Instance<IntlGregorianCalendar>>>, Thrown> {
    let Some(class) = lookup_class(req, b"IntlGregorianCalendar")? else {
        return Err(fatal(
            req,
            "InternalError: IntlGregorianCalendar is not linked",
        ));
    };
    let object = new_object(req, class)?;
    match gregorian_body(req, *object, false) {
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

/// `intlcal_create_instance`.
///
/// ```php
/// var_dump(get_class(intlcal_create_instance('UTC', 'en_US')), get_class(intlcal_create_instance(null, 'ja_JP@calendar=japanese')));
/// try { intlcal_create_instance('Nowhere'); } catch (IntlException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(21) "IntlGregorianCalendar"
/// string(12) "IntlCalendar"
/// intlcal_create_instance(): No such time zone: "Nowhere"
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_create_instance(
    req: &mut Request,
    #[php(default = null)] timezone: Raw<ZoneArg>,
    #[php(default = null)] locale: Option<PhpString>,
) -> Result<Option<Owner<Instance<IntlCalendar>>>, Thrown> {
    zone_raw(req, &timezone, 1, |req, timezone| {
        create_instance(req, timezone, locale)
    })
}

/// `intlcal_get_keyword_values_for_locale`.
///
/// ```php
/// var_dump(iterator_to_array(intlcal_get_keyword_values_for_locale('calendar', 'pt_PT', true)));
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   string(9) "gregorian"
/// }
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn intlcal_get_keyword_values_for_locale(
    req: &mut Request,
    keyword: PhpString,
    locale: PhpString,
    onlyCommon: bool,
) -> Answer<Owner<Instance<iterator::IntlIterator>>> {
    keyword_values(req, keyword, locale, onlyCommon)
}

/// `intlcal_get_now`.
///
/// ```php
/// var_dump(intlcal_get_now() > 1356998400000.0);
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_now() -> f64 {
    now()
}

/// `intlcal_get_available_locales`.
///
/// ```php
/// var_dump(in_array('en_US', intlcal_get_available_locales()));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_available_locales(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    available_locales(req)
}

/// `intlcal_get`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get($c, IntlCalendar::FIELD_YEAR), intlcal_get($c, IntlCalendar::FIELD_MONTH));
/// try { intlcal_get($c, 99); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2013)
/// int(0)
/// intlcal_get(): Argument #2 ($field) must be a valid field
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get(req: &mut Request, calendar: Instance<IntlCalendar>, field: i64) -> Answer<i64> {
    field_int(req, Of::function(calendar), GET, field)
}

/// `intlcal_get_time`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_time($c));
/// ```
/// ```expect
/// float(1356998400000)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_time(req: &mut Request, calendar: Instance<IntlCalendar>) -> Answer<f64> {
    get_time(req, Of::function(calendar))
}

/// `intlcal_set_time`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_set_time($c, 0.0), intlcal_get($c, IntlCalendar::FIELD_YEAR));
/// ```
/// ```expect
/// bool(true)
/// int(1970)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_set_time(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    timestamp: f64,
) -> Result<bool, Thrown> {
    set_time(req, Of::function(calendar), timestamp)
}

/// `intlcal_add`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_add($c, IntlCalendar::FIELD_MONTH, 13), intlcal_get($c, IntlCalendar::FIELD_YEAR));
/// try { intlcal_add($c, 1, PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(2014)
/// intlcal_add(): Argument #3 ($value) must be between -2147483648 and 2147483647
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_add(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    field: i64,
    value: i64,
) -> Result<bool, Thrown> {
    add(req, Of::function(calendar), field, value)
}

/// `intlcal_set_time_zone`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_set_time_zone($c, 'Asia/Tokyo'), intlcal_get($c, IntlCalendar::FIELD_HOUR_OF_DAY), intlcal_set_time_zone($c, IntlTimeZone::getGMT()), intlcal_get_time_zone($c)->getID());
/// ```
/// ```expect
/// bool(true)
/// int(9)
/// bool(true)
/// string(3) "GMT"
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_set_time_zone(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    timezone: Raw<ZoneArg>,
) -> Result<bool, Thrown> {
    zone_raw(req, &timezone, 2, |req, timezone| {
        set_time_zone(req, Of::function(calendar), timezone)
    })
}

/// `intlcal_after`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// $d = clone $c; intlcal_add($d, IntlCalendar::FIELD_SECOND, 1);
/// var_dump(intlcal_after($c, $d), intlcal_after($d, $c));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_after(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    other: Instance<IntlCalendar>,
) -> Result<bool, Thrown> {
    compare(req, Of::function(calendar), other, 2)
}

/// `intlcal_before`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// $d = clone $c; intlcal_add($d, IntlCalendar::FIELD_SECOND, 1);
/// var_dump(intlcal_before($c, $d), intlcal_before($d, $c));
/// try { intlcal_before($c, (new ReflectionClass('IntlCalendar'))->newInstanceWithoutConstructor()); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// intlcal_before(): Argument #2 ($other) is uninitialized
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_before(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    other: Instance<IntlCalendar>,
) -> Result<bool, Thrown> {
    compare(req, Of::function(calendar), other, 1)
}

/// `intlcal_set`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(@intlcal_set($c, 2012, 1, 29), intlcal_get($c, IntlCalendar::FIELD_MONTH), @intlcal_set($c, IntlCalendar::FIELD_DATE, 3), intlcal_get($c, IntlCalendar::FIELD_DATE));
/// try { @intlcal_set($c, 1, 2, 3, 4); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(1)
/// bool(true)
/// int(3)
/// IntlCalendar::set() has no variant with exactly 4 parameters
/// ```
#[php_function(deprecated(
    since = "8.4",
    message = "use IntlCalendar::set(), IntlCalendar::setDate(), or IntlCalendar::setDateTime() instead"
), before_args = error::reset)]
#[allow(non_snake_case)]
fn intlcal_set(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    year: i64,
    month: i64,
    dayOfMonth: Passed<i64>,
    hour: Passed<i64>,
    minute: Passed<i64>,
    second: Passed<i64>,
) -> Result<True, Thrown> {
    set_fields(
        req,
        Of::function(calendar),
        [year, month],
        [dayOfMonth, hour, minute, second],
    )
}

/// `intlcal_roll`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_roll($c, IntlCalendar::FIELD_MONTH, -1), intlcal_get($c, IntlCalendar::FIELD_YEAR), intlcal_get($c, IntlCalendar::FIELD_MONTH), intlcal_roll($c, IntlCalendar::FIELD_DATE, true));
/// ```
/// ```expect
///
/// Deprecated: intlcal_roll(): Passing bool is deprecated, use 1 or -1 instead in %s on line 3
/// bool(true)
/// int(2013)
/// int(11)
/// bool(true)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_roll(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    field: i64,
    value: Untyped<Value>,
) -> Result<bool, Thrown> {
    roll(req, Of::function(calendar), field, &value.0)
}

/// `intlcal_clear`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_clear($c, IntlCalendar::FIELD_YEAR), intlcal_is_set($c, IntlCalendar::FIELD_YEAR), intlcal_clear($c), intlcal_get_time($c));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// float(-3600000)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_clear(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    #[php(default = null)] field: Option<i64>,
) -> Result<True, Thrown> {
    clear(req, Of::function(calendar), field)
}

/// `intlcal_field_difference`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_field_difference($c, 1388534400000.0, IntlCalendar::FIELD_YEAR), intlcal_get($c, IntlCalendar::FIELD_YEAR));
/// ```
/// ```expect
/// int(1)
/// int(2014)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_field_difference(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    timestamp: f64,
    field: i64,
) -> Answer<i64> {
    field_difference(req, Of::function(calendar), timestamp, field)
}

/// `intlcal_get_actual_maximum`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// intlcal_set_time($c, 1328054400000.0); var_dump(intlcal_get_actual_maximum($c, IntlCalendar::FIELD_DATE));
/// ```
/// ```expect
/// int(29)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_actual_maximum(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    field: i64,
) -> Answer<i64> {
    field_int(req, Of::function(calendar), ACTUAL_MAXIMUM, field)
}

/// `intlcal_get_actual_minimum`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_actual_minimum($c, IntlCalendar::FIELD_DATE));
/// ```
/// ```expect
/// int(1)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_actual_minimum(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    field: i64,
) -> Answer<i64> {
    field_int(req, Of::function(calendar), ACTUAL_MINIMUM, field)
}

/// `intlcal_get_day_of_week_type`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_day_of_week_type($c, IntlCalendar::DOW_MONDAY), intlcal_get_day_of_week_type($c, IntlCalendar::DOW_SATURDAY));
/// try { intlcal_get_day_of_week_type($c, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(0)
/// int(1)
/// intlcal_get_day_of_week_type(): Argument #2 ($dayOfWeek) must be a valid day of the week
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn intlcal_get_day_of_week_type(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    dayOfWeek: i64,
) -> Answer<i64> {
    day_of_week_call(req, Of::function(calendar), 0, dayOfWeek)
}

/// `intlcal_get_first_day_of_week`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_first_day_of_week($c), intlcal_get_first_day_of_week(intlcal_create_instance('UTC', 'en_US')));
/// ```
/// ```expect
/// int(1)
/// int(1)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_first_day_of_week(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
) -> Answer<i64> {
    int_call(
        req,
        Of::function(calendar),
        0,
        "Call to ICU method has failed",
    )
}

/// `intlcal_get_least_maximum`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_least_maximum($c, IntlCalendar::FIELD_DATE));
/// ```
/// ```expect
/// int(28)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_least_maximum(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    field: i64,
) -> Answer<i64> {
    field_int(req, Of::function(calendar), LEAST_MAXIMUM, field)
}

/// `intlcal_get_greatest_minimum`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_greatest_minimum($c, IntlCalendar::FIELD_DATE));
/// ```
/// ```expect
/// int(1)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_greatest_minimum(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    field: i64,
) -> Answer<i64> {
    field_int(req, Of::function(calendar), GREATEST_MINIMUM, field)
}

/// `intlcal_get_locale`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_locale($c, 0), intlcal_get_locale($c, 1));
/// try { intlcal_get_locale($c, 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(2) "pt"
/// string(5) "pt_PT"
/// intlcal_get_locale(): Argument #2 ($type) must be either Locale::ACTUAL_LOCALE or Locale::VALID_LOCALE
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_locale(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    r#type: i64,
) -> Answer<Owner<PhpString>> {
    get_locale(req, Of::function(calendar), r#type)
}

/// `intlcal_get_maximum`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_maximum($c, IntlCalendar::FIELD_DATE));
/// ```
/// ```expect
/// int(31)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_maximum(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    field: i64,
) -> Answer<i64> {
    field_int(req, Of::function(calendar), MAXIMUM, field)
}

/// `intlcal_get_minimal_days_in_first_week`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_minimal_days_in_first_week($c));
/// ```
/// ```expect
/// int(4)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_minimal_days_in_first_week(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
) -> Answer<i64> {
    int_call(
        req,
        Of::function(calendar),
        1,
        "Call to ICU method has failed",
    )
}

/// `intlcal_set_minimal_days_in_first_week`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_set_minimal_days_in_first_week($c, 6), intlcal_get_minimal_days_in_first_week($c));
/// try { intlcal_set_minimal_days_in_first_week($c, 8); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(6)
/// intlcal_set_minimal_days_in_first_week(): Argument #2 ($days) must be between 1 and 7
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_set_minimal_days_in_first_week(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    days: i64,
) -> Result<True, Thrown> {
    set_int(req, Of::function(calendar), 2, days)
}

/// `intlcal_get_minimum`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_minimum($c, IntlCalendar::FIELD_YEAR));
/// ```
/// ```expect
/// int(1)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_minimum(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    field: i64,
) -> Answer<i64> {
    field_int(req, Of::function(calendar), MINIMUM, field)
}

/// `intlcal_get_time_zone`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_time_zone($c)->getID());
/// ```
/// ```expect
/// string(13) "Europe/Lisbon"
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_time_zone(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
) -> Answer<Owner<Instance<timezone::IntlTimeZone>>> {
    get_time_zone(req, Of::function(calendar))
}

/// `intlcal_get_type`.
///
/// ```php
/// var_dump(intlcal_get_type(intlcal_create_instance('UTC', 'en_US')), intlcal_get_type(intlcal_create_instance('UTC', 'th_TH@calendar=buddhist')));
/// ```
/// ```expect
/// string(9) "gregorian"
/// string(8) "buddhist"
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_type(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
) -> Result<Owner<PhpString>, Thrown> {
    get_type(req, Of::function(calendar))
}

/// `intlcal_get_weekend_transition`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_weekend_transition($c, IntlCalendar::DOW_SUNDAY));
/// ```
/// ```expect
/// int(86400000)
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn intlcal_get_weekend_transition(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    dayOfWeek: i64,
) -> Answer<i64> {
    day_of_week_call(req, Of::function(calendar), 1, dayOfWeek)
}

/// `intlcal_in_daylight_time`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_in_daylight_time($c)); intlcal_set_time($c, 1372636800000.0); var_dump(intlcal_in_daylight_time($c));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_in_daylight_time(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
) -> Result<bool, Thrown> {
    in_daylight_time(req, Of::function(calendar))
}

/// `intlcal_is_lenient`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_is_lenient($c));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_is_lenient(req: &mut Request, calendar: Instance<IntlCalendar>) -> Result<bool, Thrown> {
    plain_int(req, Of::function(calendar), 2).map(|n| n != 0)
}

/// `intlcal_is_set`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_is_set($c, IntlCalendar::FIELD_YEAR));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_is_set(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    field: i64,
) -> Result<bool, Thrown> {
    is_set(req, Of::function(calendar), field)
}

/// `intlcal_is_equivalent_to`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_is_equivalent_to($c, clone $c), intlcal_is_equivalent_to($c, intlcal_create_instance('UTC', 'pt_PT')));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_is_equivalent_to(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    other: Instance<IntlCalendar>,
) -> Result<bool, Thrown> {
    compare(req, Of::function(calendar), other, 3)
}

/// `intlcal_is_weekend`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_is_weekend($c, 1357387200000.0), intlcal_is_weekend($c, 1357473600000.0));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_is_weekend(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    #[php(default = null)] timestamp: Option<f64>,
) -> Result<bool, Thrown> {
    is_weekend(req, Of::function(calendar), timestamp)
}

/// `intlcal_set_first_day_of_week`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_set_first_day_of_week($c, IntlCalendar::DOW_WEDNESDAY), intlcal_get_first_day_of_week($c));
/// ```
/// ```expect
/// bool(true)
/// int(4)
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn intlcal_set_first_day_of_week(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    dayOfWeek: i64,
) -> Result<True, Thrown> {
    set_int(req, Of::function(calendar), 0, dayOfWeek)
}

/// `intlcal_set_lenient`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_set_lenient($c, false), intlcal_is_lenient($c));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_set_lenient(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    lenient: bool,
) -> Result<True, Thrown> {
    set_int(req, Of::function(calendar), 1, i64::from(lenient))
}

/// `intlcal_get_repeated_wall_time_option`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_repeated_wall_time_option($c));
/// ```
/// ```expect
/// int(0)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_repeated_wall_time_option(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
) -> Result<i64, Thrown> {
    plain_int(req, Of::function(calendar), 3)
}

/// `intlcal_equals`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_equals($c, clone $c));
/// try { intlcal_equals($c, (new ReflectionClass('IntlCalendar'))->newInstanceWithoutConstructor()); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// intlcal_equals(): Argument #2 ($other) is uninitialized
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_equals(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    other: Instance<IntlCalendar>,
) -> Result<bool, Thrown> {
    compare(req, Of::function(calendar), other, 0)
}

/// `intlcal_get_skipped_wall_time_option`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_skipped_wall_time_option($c));
/// ```
/// ```expect
/// int(0)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_skipped_wall_time_option(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
) -> Result<i64, Thrown> {
    plain_int(req, Of::function(calendar), 4)
}

/// `intlcal_set_repeated_wall_time_option`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_set_repeated_wall_time_option($c, IntlCalendar::WALLTIME_LAST), intlcal_get_repeated_wall_time_option($c));
/// try { intlcal_set_repeated_wall_time_option($c, 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// intlcal_set_repeated_wall_time_option(): Argument #2 ($option) must be either IntlCalendar::WALLTIME_FIRST or IntlCalendar::WALLTIME_LAST
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_set_repeated_wall_time_option(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    option: i64,
) -> Result<True, Thrown> {
    set_int(req, Of::function(calendar), 3, option)
}

/// `intlcal_set_skipped_wall_time_option`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_set_skipped_wall_time_option($c, IntlCalendar::WALLTIME_NEXT_VALID), intlcal_get_skipped_wall_time_option($c));
/// try { intlcal_set_skipped_wall_time_option($c, 3); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(2)
/// intlcal_set_skipped_wall_time_option(): Argument #2 ($option) must be one of IntlCalendar::WALLTIME_FIRST, IntlCalendar::WALLTIME_LAST, or IntlCalendar::WALLTIME_NEXT_VALID
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_set_skipped_wall_time_option(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
    option: i64,
) -> Result<True, Thrown> {
    set_int(req, Of::function(calendar), 4, option)
}

/// `intlcal_from_date_time`.
///
/// ```php
/// $c = intlcal_from_date_time(new DateTime('2013-01-01 12:00:00 +05:30'), 'en_US');
/// var_dump(intlcal_get_time($c), intlcal_get_time_zone($c)->getID(), intlcal_get_time_zone(intlcal_from_date_time('2013-01-01 Europe/Lisbon'))->getID());
/// ```
/// ```expect
/// float(1357021800000)
/// string(9) "GMT+05:30"
/// string(13) "Europe/Lisbon"
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_from_date_time(
    req: &mut Request,
    datetime: php_ext::abi::Union<Instance<DateTime>, PhpString>,
    #[php(default = null)] locale: Option<PhpString>,
) -> Result<Option<Owner<Instance<IntlCalendar>>>, Thrown> {
    from_date_time(req, datetime, locale)
}

/// `intlcal_to_date_time`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_to_date_time($c)->format('c e'));
/// ```
/// ```expect
/// string(39) "2013-01-01T00:00:00+00:00 Europe/Lisbon"
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_to_date_time(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
) -> Answer<Owner<Instance<DateTime>>> {
    to_date_time(req, Of::function(calendar))
}

/// `intlcal_get_error_code`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_error_code($c));
/// ```
/// ```expect
/// int(0)
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_error_code(req: &mut Request, calendar: Instance<IntlCalendar>) -> OrFalse<i64> {
    error_code(req, calendar.object())
}

/// `intlcal_get_error_message`.
///
/// ```php
/// $c = intlcal_create_instance('Europe/Lisbon', 'pt_PT'); intlcal_set_time($c, 1356998400000.0);
/// var_dump(intlcal_get_error_message($c));
/// ```
/// ```expect
/// string(12) "U_ZERO_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn intlcal_get_error_message(
    req: &mut Request,
    calendar: Instance<IntlCalendar>,
) -> Answer<Owner<PhpString>> {
    error_message(req, calendar.object())
}

/// `intlgregcal_create_instance`.
///
/// ```php
/// var_dump(get_class(@intlgregcal_create_instance('UTC', 'en_US')), @intlgregcal_create_instance(2012, 1, 29)->get(IntlCalendar::FIELD_DATE));
/// ```
/// ```expect
/// string(21) "IntlGregorianCalendar"
/// int(29)
/// ```
#[php_function(deprecated(since = "8.4", message = "use IntlGregorianCalendar::__construct(), IntlGregorianCalendar::createFromDate(), or IntlGregorianCalendar::createFromDateTime() instead"), arity = manual, before_args = error::reset)]
#[allow(non_snake_case)]
fn intlgregcal_create_instance(
    req: &mut Request,
    timezoneOrYear: Passed<Untyped<Value>>,
    localeOrMonth: Passed<Untyped<Value>>,
    day: Passed<Untyped<Value>>,
    hour: Passed<Untyped<Value>>,
    minute: Passed<Untyped<Value>>,
    second: Passed<Untyped<Value>>,
) -> Result<Option<Owner<Instance<IntlGregorianCalendar>>>, Thrown> {
    let _ = (timezoneOrYear, localeOrMonth, day, hour, minute, second);
    gregorian_create_instance(req)
}

/// `intlgregcal_set_gregorian_change`.
///
/// ```php
/// $g = new IntlGregorianCalendar('UTC'); var_dump(intlgregcal_set_gregorian_change($g, 0.0), intlgregcal_get_gregorian_change($g));
/// ```
/// ```expect
/// bool(true)
/// float(0)
/// ```
#[php_function(before_args = error::reset)]
fn intlgregcal_set_gregorian_change(
    req: &mut Request,
    calendar: Instance<IntlGregorianCalendar>,
    timestamp: f64,
) -> Result<bool, Thrown> {
    set_gregorian_change(req, Of::gregorian(calendar), timestamp)
}

/// `intlgregcal_get_gregorian_change`.
///
/// ```php
/// var_dump(intlgregcal_get_gregorian_change(new IntlGregorianCalendar('UTC')));
/// ```
/// ```expect
/// float(-12219292800000)
/// ```
#[php_function(before_args = error::reset)]
fn intlgregcal_get_gregorian_change(
    req: &mut Request,
    calendar: Instance<IntlGregorianCalendar>,
) -> Result<f64, Thrown> {
    get_gregorian_change(req, Of::gregorian(calendar))
}

/// `intlgregcal_is_leap_year`.
///
/// ```php
/// $g = new IntlGregorianCalendar('UTC'); var_dump(intlgregcal_is_leap_year($g, 2000), intlgregcal_is_leap_year($g, 1900));
/// try { intlgregcal_is_leap_year($g, PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// intlgregcal_is_leap_year(): Argument #2 ($year) must be between -2147483648 and 2147483647
/// ```
#[php_function(before_args = error::reset)]
fn intlgregcal_is_leap_year(
    req: &mut Request,
    calendar: Instance<IntlGregorianCalendar>,
    year: i64,
) -> Result<bool, Thrown> {
    is_leap_year(req, Of::gregorian(calendar), year)
}

/// The `intlcal_*` and `intlgregcal_*` block of `php_intl.stub.php`, in
/// its order.
pub(crate) fn functions(b: ExtensionBuilder) -> ExtensionBuilder {
    b.functions(vec![
        intlcal_create_instance::declared(),
        intlcal_get_keyword_values_for_locale::declared(),
        intlcal_get_now::declared(),
        intlcal_get_available_locales::declared(),
        intlcal_get::declared(),
        intlcal_get_time::declared(),
        intlcal_set_time::declared(),
        intlcal_add::declared(),
        intlcal_set_time_zone::declared(),
        intlcal_after::declared(),
        intlcal_before::declared(),
        intlcal_set::declared(),
        intlcal_roll::declared(),
        intlcal_clear::declared(),
        intlcal_field_difference::declared(),
        intlcal_get_actual_maximum::declared(),
        intlcal_get_actual_minimum::declared(),
        intlcal_get_day_of_week_type::declared(),
        intlcal_get_first_day_of_week::declared(),
        intlcal_get_least_maximum::declared(),
        intlcal_get_greatest_minimum::declared(),
        intlcal_get_locale::declared(),
        intlcal_get_maximum::declared(),
        intlcal_get_minimal_days_in_first_week::declared(),
        intlcal_set_minimal_days_in_first_week::declared(),
        intlcal_get_minimum::declared(),
        intlcal_get_time_zone::declared(),
        intlcal_get_type::declared(),
        intlcal_get_weekend_transition::declared(),
        intlcal_in_daylight_time::declared(),
        intlcal_is_lenient::declared(),
        intlcal_is_set::declared(),
        intlcal_is_equivalent_to::declared(),
        intlcal_is_weekend::declared(),
        intlcal_set_first_day_of_week::declared(),
        intlcal_set_lenient::declared(),
        intlcal_get_repeated_wall_time_option::declared(),
        intlcal_equals::declared(),
        intlcal_get_skipped_wall_time_option::declared(),
        intlcal_set_repeated_wall_time_option::declared(),
        intlcal_set_skipped_wall_time_option::declared(),
        intlcal_from_date_time::declared(),
        intlcal_to_date_time::declared(),
        intlcal_get_error_code::declared(),
        intlcal_get_error_message::declared(),
        intlgregcal_create_instance::declared(),
        intlgregcal_set_gregorian_change::declared(),
        intlgregcal_get_gregorian_change::declared(),
        intlgregcal_is_leap_year::declared(),
    ])
}
