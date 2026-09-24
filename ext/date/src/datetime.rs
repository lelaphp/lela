//! `DateTime`, `DateTimeImmutable` and the procedural natives over
//! them: `php_date_initialize` and every body `php_date.c` gives both
//! a function and a method. A function reads its object as argument 1;
//! the method of the same body reads `$this` and its arguments one
//! place earlier, as `zend_parse_method_parameters` shifts them.
//! `DateTime`'s methods change the object and answer it; the immutable
//! class's clone it first (`date_clone_immutable`) and answer the clone.

use std::time::{SystemTime, UNIX_EPOCH};

use php_ext::abi::ObjectHandlers as _;
use php_ext::call::new_object;
use php_ext::raising::{Level, Thrown, docref, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, Value, ValueRef};

use crate::State;
use crate::args::{
    array, count_between, long, nullable_object_of, object_of, optional_bool, optional_long,
    optional_string, string, string_value,
};
use crate::ffi::{
    self, NO_CLONE, OVERRIDE_TIME, RelTime, Time, UNSET, ZONETYPE_ABBR, ZONETYPE_ID,
    ZONETYPE_OFFSET,
};
use crate::format::date_format;
use crate::obj::{
    CIVIL, DateObj, TzKind, WALL, class_id, clone_date, date_fields, interval_of, new_date,
    new_interval, new_timezone, rows_array, set_time, std_rows, this, this_again, time_of,
    tz_from_time, tz_of,
};
use crate::parse::{keep_errors, path_string};
use crate::zone::{default_zone, parse_tzfile};

/// `php_date_get_current_time_with_fraction`: `gettimeofday`.
fn now_with_fraction() -> (i64, i64) {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or((0, 0), |d| {
            (
                i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                i64::from(d.subsec_micros()),
            )
        })
}

/// The first parse error as php's messages quote it: its position, its
/// character or a space for a NUL, and its text.
fn first_error(errors: &ffi::Errors) -> (i32, char, String) {
    errors.errors.first().map_or((0, ' ', String::new()), |e| {
        let c = if e.character == 0 {
            ' '
        } else {
            char::from(e.character)
        };
        (e.position, c, String::from_utf8_lossy(&e.text).into_owned())
    })
}

/// How `php_date_initialize` was called: from a constructor, which
/// throws the first parse error, and with a format, which lets the
/// format's fields override the current time's.
#[derive(Clone, Copy)]
pub(super) struct Init {
    pub ctor: bool,
    pub format: bool,
}

/// `php_date_initialize(dateobj, time_str, len, format, timezone_object,
/// flags)`: the parse, the errors kept for `date_get_last_errors`, the
/// zone chosen (the object given, the one the string named, or the
/// default), the holes filled from the current time, and the time set
/// on the object. `false` where the parse failed.
pub(super) fn initialize(
    req: &mut Request,
    object: PhpObject,
    text: &[u8],
    format: Option<&[u8]>,
    timezone: Option<PhpObject>,
    how: Init,
) -> Result<bool, Thrown> {
    set_time(req, object, None);
    let text: &[u8] = if format.is_none() && text.is_empty() {
        b"now"
    } else {
        text
    };
    let (mut parsed, errors) = match format {
        Some(format) => Time::parse_from_format(format, text),
        None => Time::strtotime(text),
    };
    keep_errors(req, &errors);
    if !errors.errors.is_empty() {
        if how.ctor {
            let (at, c, message) = first_error(&errors);
            return Err(throw(
                req,
                "DateMalformedStringException",
                &format!(
                    "Failed to parse time string ({}) at position {at} ({c}): {message}",
                    String::from_utf8_lossy(text)
                ),
            ));
        }
        return Ok(false);
    }
    let mut now = Time::new();
    let zone = match timezone {
        Some(tz) => {
            let Some(kind) = tz
                .native::<crate::obj::TzObj>(req)
                .and_then(|t| t.tz.clone())
            else {
                return Err(throw(
                    req,
                    "Error",
                    "The DateTimeZone object has not been correctly initialized by its constructor",
                ));
            };
            match kind {
                TzKind::Id(zone) => Some(zone),
                TzKind::Offset(offset) => {
                    now.zone_type = ZONETYPE_OFFSET;
                    #[allow(clippy::cast_possible_truncation)]
                    let z = offset as i32;
                    now.z = z;
                    None
                }
                TzKind::Abbr {
                    utc_offset,
                    abbr,
                    dst,
                } => {
                    #[allow(clippy::cast_possible_truncation)]
                    let z = utc_offset as i32;
                    now.z = z;
                    now.dst = dst;
                    now.zone_type = ZONETYPE_ABBR;
                    now.set_abbr(&abbr);
                    None
                }
            }
        }
        None => match parsed.zone() {
            Some(zone) => Some(zone.clone()),
            None => Some(default_zone(req)?),
        },
    };
    if let Some(zone) = &zone {
        now.set_zone_id(zone.clone());
    }
    let (sec, usec) = now_with_fraction();
    now.unixtime2local(sec);
    now.us = usec;
    if format.is_none() && text == b"now" {
        set_time(req, object, Some(now));
        return Ok(true);
    }
    let options = NO_CLONE | if how.format { OVERRIDE_TIME } else { 0 };
    parsed.fill_holes(&mut now, options);
    parsed.update_ts(zone.as_ref());
    parsed.update_from_sse();
    parsed.have_relative = 0;
    set_time(req, object, Some(parsed));
    Ok(true)
}

/// `php_date_initialize_from_ts_long(dateobj, sec, usec)`.
fn from_timestamp(sec: i64, usec: i64) -> Time {
    let mut t = Time::new();
    t.zone_type = ZONETYPE_OFFSET;
    t.unixtime2gmt(sec);
    t.update_ts(None);
    t.us = usec;
    t
}

/// `php_date_initialize_from_ts_double`: the seconds and the rounded
/// microseconds, or the `DateRangeError` for a number no timestamp is.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn from_float(req: &mut Request, method: &str, ts: f64) -> Result<Time, Thrown> {
    let range = |req: &mut Request| {
        throw(
            req,
            "DateRangeError",
            &format!(
                "{method}(): Argument #1 ($timestamp) must be a finite number between {} and {}.999999, {} given",
                i64::MIN,
                i64::MAX,
                php_g(ts)
            ),
        )
    };
    let sec_f = ts.trunc();
    if sec_f.is_nan()
        || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&sec_f)
    {
        return Err(range(req));
    }
    let mut sec = sec_f as i64;
    let mut usec = ((ts % 1.0) * 1_000_000.0).round() as i64;
    if usec.abs() == 1_000_000 {
        sec += if usec > 0 { 1 } else { -1 };
        usec = 0;
    }
    if usec < 0 {
        if sec == i64::MIN {
            return Err(range(req));
        }
        sec -= 1;
        usec += 1_000_000;
    }
    Ok(from_timestamp(sec, usec))
}

/// `%g` of a double, what `zend_argument_error`'s messages print.
#[allow(clippy::cast_possible_truncation)]
fn php_g(f: f64) -> String {
    if f.is_nan() {
        return "NAN".to_owned();
    }
    if f.is_infinite() {
        return if f > 0.0 { "INF" } else { "-INF" }.to_owned();
    }
    let exp = if f == 0.0 {
        0
    } else {
        f.abs().log10().floor() as i32
    };
    if !(-5..6).contains(&exp) {
        let s = format!("{f:.5e}");
        let (mantissa, e) = s.split_once('e').unwrap_or((&s, "0"));
        let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
        let e: i32 = e.parse().unwrap_or(0);
        let sign = if e < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", e.abs());
    }
    let decimals = usize::try_from(5 - exp).unwrap_or(0);
    let s = format!("{f:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s
    }
}

/// `date_create`-shaped construction: an object of `class`, initialised,
/// or `false` where the parse failed.
fn create(
    req: &mut Request,
    class: ClassId,
    text: &[u8],
    format: Option<&[u8]>,
    timezone: Option<PhpObject>,
    how: Init,
) -> Result<Value, Thrown> {
    let object = new_object(req, class)?;
    match initialize(req, object, text, format, timezone, how) {
        Ok(true) => Ok(object.as_value()),
        Ok(false) => {
            req.release(object.as_value());
            Ok(Value::bool(false))
        }
        Err(thrown) => {
            req.release(object.as_value());
            Err(thrown)
        }
    }
}

/// `date_create` and `date_create_immutable`.
fn date_create_of(req: &mut Request, class: &str) -> Result<Value, Thrown> {
    count_between(req, 0, 2)?;
    let text = optional_string(req, 1)?
        .map(|s| s.as_bytes().to_vec())
        .unwrap_or_default();
    let timezone = nullable_object_of(req, 2, "DateTimeZone")?;
    let class = class_id(req, class)?;
    create(
        req,
        class,
        &text,
        None,
        timezone,
        Init {
            ctor: false,
            format: false,
        },
    )
}

/// `date_create(string $datetime = "now", ?DateTimeZone $timezone =
/// null): DateTime|false`.
///
/// ```php
/// date_default_timezone_set('Europe/Amsterdam');
/// var_dump(date_create('2024-02-29 13:45:10.25'), date_create('2020-01-01', new DateTimeZone('Asia/Tokyo')), date_create('2020-01-01 10:00 EST'), date_create('@86400'), date_create('nonsense'));
/// ```
/// ```expect
/// object(DateTime)#1 (3) {
///   ["date"]=>
///   string(26) "2024-02-29 13:45:10.250000"
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(16) "Europe/Amsterdam"
/// }
/// object(DateTime)#3 (3) {
///   ["date"]=>
///   string(26) "2020-01-01 00:00:00.000000"
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(10) "Asia/Tokyo"
/// }
/// object(DateTime)#2 (3) {
///   ["date"]=>
///   string(26) "2020-01-01 10:00:00.000000"
///   ["timezone_type"]=>
///   int(2)
///   ["timezone"]=>
///   string(3) "EST"
/// }
/// object(DateTime)#4 (3) {
///   ["date"]=>
///   string(26) "1970-01-02 00:00:00.000000"
///   ["timezone_type"]=>
///   int(1)
///   ["timezone"]=>
///   string(6) "+00:00"
/// }
/// bool(false)
/// ```
pub(crate) fn date_create(req: &mut Request) -> Result<Value, Thrown> {
    date_create_of(req, "DateTime")
}

/// `date_create_immutable(...)`: `date_create` of the immutable class.
///
/// ```php
/// var_dump(date_create_immutable('2021-03-04 05:06:07 +03:00'), date_create_immutable('32nd of never'));
/// ```
/// ```expect
/// object(DateTimeImmutable)#1 (3) {
///   ["date"]=>
///   string(26) "2021-03-04 05:06:07.000000"
///   ["timezone_type"]=>
///   int(1)
///   ["timezone"]=>
///   string(6) "+03:00"
/// }
/// bool(false)
/// ```
pub(crate) fn date_create_immutable(req: &mut Request) -> Result<Value, Thrown> {
    date_create_of(req, "DateTimeImmutable")
}

/// `date_create_from_format` and its method: the class is the called
/// one where a call on the class names it.
fn from_format(req: &mut Request, class: ClassId) -> Result<Value, Thrown> {
    count_between(req, 2, 3)?;
    let format = string(req, 1)?.as_bytes().to_vec();
    let text = path_string(req, 2)?.as_bytes().to_vec();
    let timezone = nullable_object_of(req, 3, "DateTimeZone")?;
    create(
        req,
        class,
        &text,
        Some(&format),
        timezone,
        Init {
            ctor: false,
            format: true,
        },
    )
}

/// `date_create_from_format(string $format, string $datetime,
/// ?DateTimeZone $timezone = null): DateTime|false`.
///
/// ```php
/// date_default_timezone_set('UTC');
/// var_dump(date_create_from_format('Y-m-d H:i:s', '2009-02-15 15:16:17'), date_create_from_format('!d/m/Y', '15/02/2009', new DateTimeZone('Europe/Paris')), date_create_from_format('Y-m-d', 'x'));
/// var_dump(date_create_from_format('U.u', '1234567890.123456')->format('Y-m-d H:i:s.u T'));
/// ```
/// ```expect
/// object(DateTime)#1 (3) {
///   ["date"]=>
///   string(26) "2009-02-15 15:16:17.000000"
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(3) "UTC"
/// }
/// object(DateTime)#3 (3) {
///   ["date"]=>
///   string(26) "2009-02-15 00:00:00.000000"
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(12) "Europe/Paris"
/// }
/// bool(false)
/// string(35) "2009-02-13 23:31:30.123456 GMT+0000"
/// ```
pub(crate) fn date_create_from_format(req: &mut Request) -> Result<Value, Thrown> {
    let class = class_id(req, "DateTime")?;
    from_format(req, class)
}

/// `date_create_immutable_from_format(...)`.
///
/// ```php
/// date_default_timezone_set('UTC');
/// var_dump(date_create_immutable_from_format('j-M-Y', '15-Feb-2009')->format('Y-m-d'), date_create_immutable_from_format('H\h i\m', '23h 15m')->format('H:i:s'));
/// ```
/// ```expect
/// string(10) "2009-02-15"
/// string(8) "23:15:00"
/// ```
pub(crate) fn date_create_immutable_from_format(req: &mut Request) -> Result<Value, Thrown> {
    let class = class_id(req, "DateTimeImmutable")?;
    from_format(req, class)
}

/// The class a factory called on a class makes: the called one, else
/// `fallback`.
fn called_or(req: &mut Request, fallback: &str) -> Result<ClassId, Thrown> {
    match req.called_scope() {
        Some(class) => Ok(class),
        None => class_id(req, fallback),
    }
}

/// `DateTime::createFromFormat(...)`: `date_create_from_format` of the
/// called class.
///
/// ```php
/// date_default_timezone_set('UTC');
/// class My extends DateTime {}
/// var_dump(get_class(My::createFromFormat('Y-m-d', '2020-05-06')), DateTime::createFromFormat('Y-m-d', 'nope'), DateTime::getLastErrors()['errors']);
/// ```
/// ```expect
/// string(2) "My"
/// bool(false)
/// array(2) {
///   [0]=>
///   string(36) "A four digit year could not be found"
///   [4]=>
///   string(43) "Not enough data available to satisfy format"
/// }
/// ```
pub(crate) fn datetime_create_from_format(req: &mut Request) -> Result<Value, Thrown> {
    let class = called_or(req, "DateTime")?;
    from_format(req, class)
}

/// `DateTimeImmutable::createFromFormat(...)`.
///
/// ```php
/// date_default_timezone_set('UTC');
/// var_dump(DateTimeImmutable::createFromFormat('Y-m-d H:i', '2020-05-06 07:08')->format(DATE_ATOM));
/// ```
/// ```expect
/// string(25) "2020-05-06T07:08:00+00:00"
/// ```
pub(crate) fn immutable_create_from_format(req: &mut Request) -> Result<Value, Thrown> {
    let class = called_or(req, "DateTimeImmutable")?;
    from_format(req, class)
}

/// `DateTime::__construct(string $datetime = "now", ?DateTimeZone
/// $timezone = null)`: the parse error thrown as
/// `DateMalformedStringException`.
///
/// ```php
/// date_default_timezone_set('America/Chicago');
/// var_dump(new DateTime('2000-01-01 00:00:00'), new DateTime('2000-01-01', new DateTimeZone('+05:30')));
/// try { new DateTime('not a date'); } catch (DateMalformedStringException $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// class A extends DateTime { function __construct() {} }
/// try { (new A)->format('Y'); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(DateTime)#1 (3) {
///   ["date"]=>
///   string(26) "2000-01-01 00:00:00.000000"
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(15) "America/Chicago"
/// }
/// object(DateTime)#2 (3) {
///   ["date"]=>
///   string(26) "2000-01-01 00:00:00.000000"
///   ["timezone_type"]=>
///   int(1)
///   ["timezone"]=>
///   string(6) "+05:30"
/// }
/// DateMalformedStringException: Failed to parse time string (not a date) at position 0 (n): The timezone could not be found in the database
/// DateObjectError: Object of type A (inheriting DateTime) has not been correctly initialized by calling parent::__construct() in its constructor
/// ```
///
/// Its declaration, as `ReflectionMethod` renders it.
///
/// ```php
/// echo new ReflectionMethod('DateTime', '__construct');
/// echo new ReflectionMethod('DateTime', 'setTime');
/// ```
/// ```expect
/// Method [ <internal:date, ctor> public method __construct ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <optional> string $datetime = "now" ]
///     Parameter #1 [ <optional> ?DateTimeZone $timezone = null ]
///   }
/// }
/// Method [ <internal:date> public method setTime ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> int $hour ]
///     Parameter #1 [ <required> int $minute ]
///     Parameter #2 [ <optional> int $second = 0 ]
///     Parameter #3 [ <optional> int $microsecond = 0 ]
///   }
///   - Tentative return [ DateTime ]
/// }
/// ```
pub(crate) fn construct(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 2)?;
    let text = optional_string(req, 1)?
        .map(|s| s.as_bytes().to_vec())
        .unwrap_or_default();
    let timezone = nullable_object_of(req, 2, "DateTimeZone")?;
    let object = this(req)?;
    initialize(
        req,
        object,
        &text,
        None,
        timezone,
        Init {
            ctor: true,
            format: false,
        },
    )?;
    Ok(Value::null())
}

/// `createFromImmutable`, `createFromMutable` and both
/// `createFromInterface`s: a date of the called class over a copy of
/// the given one's time.
fn create_from(req: &mut Request, source: &str, fallback: &str) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let from = object_of(req, 1, source)?;
    let time = time_of(req, from)?;
    let class = called_or(req, fallback)?;
    Ok(new_date(req, class, time)?.as_value())
}

/// `DateTime::createFromImmutable(DateTimeImmutable $object): static`.
///
/// ```php
/// $i = new DateTimeImmutable('2001-02-03 04:05:06', new DateTimeZone('Europe/Oslo'));
/// var_dump(DateTime::createFromImmutable($i));
/// ```
/// ```expect
/// object(DateTime)#2 (3) {
///   ["date"]=>
///   string(26) "2001-02-03 04:05:06.000000"
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(11) "Europe/Oslo"
/// }
/// ```
pub(crate) fn create_from_immutable(req: &mut Request) -> Result<Value, Thrown> {
    create_from(req, "DateTimeImmutable", "DateTime")
}

/// `DateTime::createFromInterface(DateTimeInterface $object): DateTime`.
///
/// ```php
/// var_dump(DateTime::createFromInterface(new DateTimeImmutable('@1000000000'))->format('c'), DateTime::createFromInterface(new DateTime('2020-01-01 UTC'))->format('c'));
/// ```
/// ```expect
/// string(25) "2001-09-09T01:46:40+00:00"
/// string(25) "2020-01-01T00:00:00+00:00"
/// ```
pub(crate) fn datetime_create_from_interface(req: &mut Request) -> Result<Value, Thrown> {
    create_from(req, "DateTimeInterface", "DateTime")
}

/// `DateTimeImmutable::createFromMutable(DateTime $object): static`.
///
/// ```php
/// var_dump(DateTimeImmutable::createFromMutable(new DateTime('2010-10-10 10:10:10.5 UTC')));
/// ```
/// ```expect
/// object(DateTimeImmutable)#2 (3) {
///   ["date"]=>
///   string(26) "2010-10-10 10:10:10.500000"
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(3) "UTC"
/// }
/// ```
pub(crate) fn create_from_mutable(req: &mut Request) -> Result<Value, Thrown> {
    create_from(req, "DateTime", "DateTimeImmutable")
}

/// `DateTimeImmutable::createFromInterface(DateTimeInterface $object):
/// DateTimeImmutable`.
///
/// ```php
/// var_dump(get_class(DateTimeImmutable::createFromInterface(new DateTime('@0'))), DateTimeImmutable::createFromInterface(new DateTime('@0'))->getTimestamp());
/// ```
/// ```expect
/// string(17) "DateTimeImmutable"
/// int(0)
/// ```
pub(crate) fn immutable_create_from_interface(req: &mut Request) -> Result<Value, Thrown> {
    create_from(req, "DateTimeInterface", "DateTimeImmutable")
}

/// `createFromTimestamp(int|float $timestamp): static` of either class.
fn create_from_timestamp(req: &mut Request, method: &str, fallback: &str) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let value = req.argument(1).unwrap_or_default().deref();
    let time = match value.as_ref() {
        ValueRef::Int(n) => from_timestamp(n, 0),
        ValueRef::Float(f) => from_float(req, method, f)?,
        _ => {
            let number = php_ext::coerce::coerce_param(
                req,
                &value,
                &crate::args::union(vec![php_ext::decl::Type::Int, php_ext::decl::Type::Float]),
                1,
            )?;
            match number.as_ref() {
                ValueRef::Int(n) => from_timestamp(n, 0),
                ValueRef::Float(f) => from_float(req, method, f)?,
                _ => from_timestamp(0, 0),
            }
        }
    };
    let class = called_or(req, fallback)?;
    Ok(new_date(req, class, time)?.as_value())
}

/// `DateTime::createFromTimestamp(int|float $timestamp): static`.
///
/// ```php
/// var_dump(DateTime::createFromTimestamp(1700000000), DateTime::createFromTimestamp(-1.5)->format('Y-m-d H:i:s.u'), DateTime::createFromTimestamp(1.9999999)->format('U.u'));
/// try { DateTime::createFromTimestamp(INF); } catch (DateRangeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(DateTime)#1 (3) {
///   ["date"]=>
///   string(26) "2023-11-14 22:13:20.000000"
///   ["timezone_type"]=>
///   int(1)
///   ["timezone"]=>
///   string(6) "+00:00"
/// }
/// string(26) "1969-12-31 23:59:58.500000"
/// string(8) "2.000000"
/// DateTime::createFromTimestamp(): Argument #1 ($timestamp) must be a finite number between -9223372036854775808 and 9223372036854775807.999999, INF given
/// ```
pub(crate) fn datetime_create_from_timestamp(req: &mut Request) -> Result<Value, Thrown> {
    create_from_timestamp(req, "DateTime::createFromTimestamp", "DateTime")
}

/// `DateTimeImmutable::createFromTimestamp(int|float $timestamp): static`.
///
/// ```php
/// var_dump(DateTimeImmutable::createFromTimestamp(0.5)->format(DATE_RFC3339_EXTENDED));
/// ```
/// ```expect
/// string(29) "1970-01-01T00:00:00.500+00:00"
/// ```
pub(crate) fn immutable_create_from_timestamp(req: &mut Request) -> Result<Value, Thrown> {
    create_from_timestamp(
        req,
        "DateTimeImmutable::createFromTimestamp",
        "DateTimeImmutable",
    )
}

/// `date_get_last_errors(): array|false`, and `getLastErrors` of both
/// classes: the messages of the last parse that had any.
///
/// ```php
/// var_dump(date_get_last_errors());
/// date_create('2009-02-30 xx');
/// var_dump(date_get_last_errors());
/// date_create('2009-02-03');
/// var_dump(date_get_last_errors(), DateTimeImmutable::getLastErrors());
/// ```
/// ```expect
/// bool(false)
/// array(4) {
///   ["warning_count"]=>
///   int(1)
///   ["warnings"]=>
///   array(1) {
///     [14]=>
///     string(27) "The parsed date was invalid"
///   }
///   ["error_count"]=>
///   int(1)
///   ["errors"]=>
///   array(1) {
///     [11]=>
///     string(47) "The timezone could not be found in the database"
///   }
/// }
/// bool(false)
/// bool(false)
/// ```
pub(crate) fn date_get_last_errors(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    let Some(errors) = req.state::<State>().last_errors.clone() else {
        return Ok(Value::bool(false));
    };
    crate::parse::errors_value(req, &errors)
}

/// The object a body works on: `$this` for a method, argument 1 of
/// `class` for a function, and the position its first own argument
/// stands at.
fn receiver(req: &mut Request, method: bool, class: &str) -> Result<(PhpObject, u32), Thrown> {
    if method {
        Ok((this(req)?, 1))
    } else {
        Ok((object_of(req, 1, class)?, 2))
    }
}

/// `date_format(DateTimeInterface $object, string $format): string` and
/// `format` of both classes.
fn format_body(req: &mut Request, method: bool) -> Result<Value, Thrown> {
    let shift = usize::from(!method);
    count_between(req, 1 + shift, 1 + shift)?;
    let (object, at) = receiver(req, method, "DateTimeInterface")?;
    let format = string(req, at)?.as_bytes().to_vec();
    let text = {
        let Some(t) = object.native::<DateObj>(req).and_then(|d| d.time.as_ref()) else {
            return Err(crate::obj::uninitialized(req, object.class()));
        };
        date_format(&format, t, t.is_localtime != 0)
    };
    string_value(req, &text)
}

/// `date_format(DateTimeInterface $object, string $format): string`.
///
/// ```php
/// $d = new DateTime('2010-01-02 03:04:05.678901', new DateTimeZone('America/Sao_Paulo'));
/// var_dump(date_format($d, 'Y-m-d H:i:s.u v e T P p O Z U'), date_format(new DateTime('2000-01-01 12:00 +0530'), 'e T P'), date_format(new DateTime('2000-01-01 CEST'), 'e T P I Z'));
/// ```
/// ```expect
/// string(89) "2010-01-02 03:04:05.678901 678 America/Sao_Paulo -02 -02:00 -02:00 -0200 -7200 1262408645"
/// string(22) "+05:30 GMT+0530 +05:30"
/// string(23) "CEST CEST +02:00 1 7200"
/// ```
pub(crate) fn date_format_fn(req: &mut Request) -> Result<Value, Thrown> {
    format_body(req, false)
}

/// `DateTimeInterface::format(string $format): string`.
///
/// ```php
/// $d = new DateTimeImmutable('-0044-03-15 12:00:00 UTC');
/// var_dump($d->format('Y y x X c'), (new DateTime('10000-01-01 UTC'))->format('Y x X'), (new DateTime('@0'))->format(DATE_COOKIE));
/// ```
/// ```expect
/// string(47) "-0044 -44 -0044 -0044 -044-03-15T12:00:00+00:00"
/// string(15) "2000 2000 +2000"
/// string(39) "Thursday, 01-Jan-1970 00:00:00 GMT+0000"
/// ```
pub(crate) fn format_method(req: &mut Request) -> Result<Value, Thrown> {
    format_body(req, true)
}

/// `php_date_modify(object, modify, len)`: the parse's fields written
/// over the object's time, its relative part applied, and the zone set
/// to UTC by a `@<ts>`. `method` names the method whose parse error is
/// thrown as `DateMalformedStringException`; without one it is php's
/// warning.
fn modify(
    req: &mut Request,
    object: PhpObject,
    text: &[u8],
    method: Option<&str>,
) -> Result<bool, Thrown> {
    let mut time = time_of(req, object)?;
    let (tmp, errors) = Time::strtotime(text);
    keep_errors(req, &errors);
    if !errors.errors.is_empty() {
        let (at, c, message) = first_error(&errors);
        let message = format!(
            "Failed to parse time string ({}) at position {at} ({c}): {message}",
            String::from_utf8_lossy(text)
        );
        if let Some(name) = method {
            return Err(throw(
                req,
                "DateMalformedStringException",
                &format!("{name}(): {message}"),
            ));
        }
        docref(req, Level::Warning, &message);
        return Ok(false);
    }
    time.relative = tmp.relative;
    time.have_relative = tmp.have_relative;
    time.sse_uptodate = 0;
    if tmp.y != UNSET {
        time.y = tmp.y;
    }
    if tmp.m != UNSET {
        time.m = tmp.m;
    }
    if tmp.d != UNSET {
        time.d = tmp.d;
    }
    if tmp.h != UNSET {
        time.h = tmp.h;
        if tmp.i == UNSET {
            time.i = 0;
            time.s = 0;
        } else {
            time.i = tmp.i;
            time.s = if tmp.s == UNSET { 0 } else { tmp.s };
        }
    }
    if tmp.us != UNSET {
        time.us = tmp.us;
    }
    if tmp.y == 1970
        && tmp.m == 1
        && tmp.d == 1
        && tmp.h == 0
        && tmp.i == 0
        && tmp.s == 0
        && tmp.us == 0
        && tmp.have_zone != 0
        && tmp.zone_type == ZONETYPE_OFFSET
        && tmp.z == 0
        && tmp.dst == 0
    {
        time.set_timezone_from_offset(0);
    }
    time.update_ts(None);
    time.update_from_sse();
    time.have_relative = 0;
    time.relative = RelTime::default();
    set_time(req, object, Some(time));
    Ok(true)
}

/// `date_modify(DateTime $object, string $modifier): DateTime|false`.
///
/// ```php
/// date_default_timezone_set('UTC');
/// $d = new DateTime('2006-12-12 10:00:00');
/// var_dump(date_modify($d, '+1 day')->format('c'), date_modify($d, 'last day of next month noon')->format('c'), date_modify($d, 'garbage'), date_modify($d, '@1234567890')->format('c e'));
/// ```
/// ```expect
/// Warning: date_modify(): Failed to parse time string (garbage) at position 0 (g): The timezone could not be found in the database in %s on line 4
/// string(25) "2006-12-13T10:00:00+00:00"
/// string(25) "2007-01-31T12:00:00+00:00"
/// bool(false)
/// string(32) "2009-02-13T23:31:30+00:00 +00:00"
/// ```
pub(crate) fn date_modify(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 2, 2)?;
    let object = object_of(req, 1, "DateTime")?;
    let text = string(req, 2)?.as_bytes().to_vec();
    if !modify(req, object, &text, None)? {
        return Ok(Value::bool(false));
    }
    Ok(this_again(req, object))
}

/// `DateTime::modify(string $modifier): DateTime`.
///
/// ```php
/// date_default_timezone_set('Europe/London');
/// $d = new DateTime('2021-03-27 01:30:00');
/// var_dump($d->modify('+1 day')->format('c T'), $d->modify('first monday of january 2022')->format('c'), $d->modify('+90 minutes 15 seconds')->format('c'));
/// try { $d->modify('foo'); } catch (DateMalformedStringException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(29) "2021-03-28T02:30:00+01:00 BST"
/// string(25) "2022-01-03T00:00:00+00:00"
/// string(25) "2022-01-03T01:30:15+00:00"
/// DateTime::modify(): Failed to parse time string (foo) at position 0 (f): The timezone could not be found in the database
/// ```
pub(crate) fn datetime_modify(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let text = string(req, 1)?.as_bytes().to_vec();
    let object = this(req)?;
    modify(req, object, &text, Some("DateTime::modify"))?;
    Ok(this_again(req, object))
}

/// `DateTimeImmutable::modify(string $modifier): DateTimeImmutable`.
///
/// ```php
/// $d = new DateTimeImmutable('2020-02-29 12:00 UTC');
/// $e = $d->modify('+1 year');
/// var_dump($d->format('c'), $e->format('c'));
/// try { $x = $d->modify('+1 fortnights'); } catch (DateMalformedStringException $x) { echo $x->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(25) "2020-02-29T12:00:00+00:00"
/// string(25) "2021-03-01T12:00:00+00:00"
/// ```
pub(crate) fn immutable_modify(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let text = string(req, 1)?.as_bytes().to_vec();
    let object = this(req)?;
    let copy = clone_date(req, object)?;
    if let Err(thrown) = modify(req, copy, &text, Some("DateTimeImmutable::modify")) {
        req.release(copy.as_value());
        return Err(thrown);
    }
    Ok(copy.as_value())
}

/// `php_date_add`: `timelib_add_wall` for an interval a constructor
/// made, `timelib_add` for a computed one.
fn add(req: &mut Request, object: PhpObject, interval: PhpObject) -> Result<(), Thrown> {
    let time = time_of(req, object)?;
    let (diff, civil) = interval_of(req, interval)?;
    let new = if civil == WALL {
        time.add_wall(&diff)
    } else {
        time.add(&diff)
    };
    set_time(req, object, Some(new));
    Ok(())
}

/// `php_date_sub`: as `add`, but a weekday or special relative part is
/// php's warning, or under `method` the `DateInvalidOperationException`.
fn sub(
    req: &mut Request,
    object: PhpObject,
    interval: PhpObject,
    method: Option<&str>,
) -> Result<(), Thrown> {
    let time = time_of(req, object)?;
    let (diff, civil) = interval_of(req, interval)?;
    if diff.have_weekday_relative != 0 || diff.have_special_relative != 0 {
        let message = "Only non-special relative time specifications are supported for subtraction";
        if let Some(name) = method {
            return Err(throw(
                req,
                "DateInvalidOperationException",
                &format!("{name}(): {message}"),
            ));
        }
        docref(req, Level::Warning, message);
        return Ok(());
    }
    let new = if civil == WALL {
        time.sub_wall(&diff)
    } else {
        time.sub(&diff)
    };
    set_time(req, object, Some(new));
    Ok(())
}

/// `date_add(DateTime $object, DateInterval $interval): DateTime`, and
/// `DateTime::add`.
///
/// ```php
/// date_default_timezone_set('Europe/Amsterdam');
/// $d = new DateTime('2021-03-27 02:30:00');
/// var_dump(date_add($d, new DateInterval('P1D'))->format('c'), $d->add(new DateInterval('PT24H'))->format('c'), $d->add(DateInterval::createFromDateString('last day of next month'))->format('c'));
/// ```
/// ```expect
/// string(25) "2021-03-28T03:30:00+02:00"
/// string(25) "2021-03-29T03:30:00+02:00"
/// string(25) "2021-04-29T03:30:00+02:00"
/// ```
pub(crate) fn date_add(req: &mut Request) -> Result<Value, Thrown> {
    let method = req.this().is_some();
    let shift = usize::from(!method);
    count_between(req, 1 + shift, 1 + shift)?;
    let (object, at) = receiver(req, method, "DateTime")?;
    let interval = object_of(req, at, "DateInterval")?;
    add(req, object, interval)?;
    Ok(this_again(req, object))
}

/// `DateTimeImmutable::add(DateInterval $interval): DateTimeImmutable`.
///
/// ```php
/// $d = new DateTimeImmutable('2000-01-31 UTC');
/// var_dump($d->add(new DateInterval('P1M'))->format('Y-m-d'), $d->format('Y-m-d'), $d->add(new DateInterval('PT1S'))->format('H:i:s.u'));
/// ```
/// ```expect
/// string(10) "2000-03-02"
/// string(10) "2000-01-31"
/// string(15) "00:00:01.000000"
/// ```
pub(crate) fn immutable_add(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let interval = object_of(req, 1, "DateInterval")?;
    let object = this(req)?;
    let copy = clone_date(req, object)?;
    if let Err(thrown) = add(req, copy, interval) {
        req.release(copy.as_value());
        return Err(thrown);
    }
    Ok(copy.as_value())
}

/// `date_sub(DateTime $object, DateInterval $interval): DateTime`.
///
/// ```php
/// $d = new DateTime('2000-03-01 UTC');
/// var_dump(date_sub($d, new DateInterval('P1D'))->format('Y-m-d'), date_sub($d, DateInterval::createFromDateString('next weekday'))->format('Y-m-d'));
/// ```
/// ```expect
/// Warning: date_sub(): Only non-special relative time specifications are supported for subtraction in %s on line 3
/// string(10) "2000-02-29"
/// string(10) "2000-02-29"
/// ```
pub(crate) fn date_sub(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 2, 2)?;
    let object = object_of(req, 1, "DateTime")?;
    let interval = object_of(req, 2, "DateInterval")?;
    sub(req, object, interval, None)?;
    Ok(this_again(req, object))
}

/// `DateTime::sub(DateInterval $interval): DateTime`.
///
/// ```php
/// $d = new DateTime('2000-03-01 UTC');
/// var_dump($d->sub(new DateInterval('P1Y2M3DT4H5M6S'))->format('c'));
/// try { $d->sub(DateInterval::createFromDateString('next weekday')); } catch (DateInvalidOperationException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(25) "1998-12-28T19:54:54+00:00"
/// DateTime::sub(): Only non-special relative time specifications are supported for subtraction
/// ```
pub(crate) fn datetime_sub(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let interval = object_of(req, 1, "DateInterval")?;
    let object = this(req)?;
    sub(req, object, interval, Some("DateTime::sub"))?;
    Ok(this_again(req, object))
}

/// `DateTimeImmutable::sub(DateInterval $interval): DateTimeImmutable`.
///
/// ```php
/// $d = new DateTimeImmutable('2000-03-01 UTC');
/// var_dump($d->sub(new DateInterval('P1D'))->format('Y-m-d'), $d->format('Y-m-d'));
/// ```
/// ```expect
/// string(10) "2000-02-29"
/// string(10) "2000-03-01"
/// ```
pub(crate) fn immutable_sub(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let interval = object_of(req, 1, "DateInterval")?;
    let object = this(req)?;
    let copy = clone_date(req, object)?;
    if let Err(thrown) = sub(req, copy, interval, Some("DateTimeImmutable::sub")) {
        req.release(copy.as_value());
        return Err(thrown);
    }
    Ok(copy.as_value())
}

/// `date_timezone_get(DateTimeInterface $object): DateTimeZone|false`,
/// and `getTimezone` of both classes: `false` for a time with no zone.
///
/// ```php
/// var_dump(date_timezone_get(new DateTime('2000-01-01 Europe/Rome')), (new DateTimeImmutable('2000-01-01 -03:00'))->getTimezone(), (new DateTime('2000-01-01 PST'))->getTimezone()->getName());
/// ```
/// ```expect
/// object(DateTimeZone)#2 (2) {
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(11) "Europe/Rome"
/// }
/// object(DateTimeZone)#3 (2) {
///   ["timezone_type"]=>
///   int(1)
///   ["timezone"]=>
///   string(6) "-03:00"
/// }
/// string(3) "PST"
/// ```
pub(crate) fn date_timezone_get(req: &mut Request) -> Result<Value, Thrown> {
    let method = req.this().is_some();
    count_between(req, usize::from(!method), usize::from(!method))?;
    let (object, _) = receiver(req, method, "DateTimeInterface")?;
    let time = time_of(req, object)?;
    if time.is_localtime == 0 {
        return Ok(Value::bool(false));
    }
    match tz_from_time(&time) {
        Some(kind) => new_timezone(req, kind),
        None => Ok(Value::bool(false)),
    }
}

/// `php_date_timezone_set`: the zone set and the local time recomputed.
fn timezone_set(req: &mut Request, object: PhpObject, timezone: PhpObject) -> Result<(), Thrown> {
    let mut time = time_of(req, object)?;
    let kind = tz_of(req, timezone)?;
    match &kind {
        TzKind::Offset(offset) => time.set_timezone_from_offset(*offset),
        TzKind::Abbr {
            utc_offset,
            abbr,
            dst,
        } => time.set_timezone_from_abbr(*utc_offset, abbr, *dst),
        TzKind::Id(zone) => time.set_timezone(zone),
    }
    let sse = time.sse;
    time.unixtime2local(sse);
    set_time(req, object, Some(time));
    Ok(())
}

/// `date_timezone_set(DateTime $object, DateTimeZone $timezone):
/// DateTime`, and `DateTime::setTimezone`.
///
/// ```php
/// $d = new DateTime('2000-01-01 12:00:00 UTC');
/// var_dump(date_timezone_set($d, new DateTimeZone('Asia/Kolkata'))->format('c e'), $d->setTimezone(new DateTimeZone('-02:30'))->format('c e'), $d->setTimezone(new DateTimeZone('EDT'))->format('c e T'));
/// ```
/// ```expect
/// string(38) "2000-01-01T17:30:00+05:30 Asia/Kolkata"
/// string(32) "2000-01-01T09:30:00-02:30 -02:30"
/// string(33) "2000-01-01T08:00:00-04:00 EDT EDT"
/// ```
pub(crate) fn date_timezone_set(req: &mut Request) -> Result<Value, Thrown> {
    let method = req.this().is_some();
    let shift = usize::from(!method);
    count_between(req, 1 + shift, 1 + shift)?;
    let (object, at) = receiver(req, method, "DateTime")?;
    let timezone = object_of(req, at, "DateTimeZone")?;
    timezone_set(req, object, timezone)?;
    Ok(this_again(req, object))
}

/// `DateTimeImmutable::setTimezone(DateTimeZone $timezone): DateTimeImmutable`.
///
/// ```php
/// $d = new DateTimeImmutable('2000-06-01 12:00:00 UTC');
/// var_dump($d->setTimezone(new DateTimeZone('Europe/Berlin'))->format('c T'), $d->format('c T'));
/// ```
/// ```expect
/// string(30) "2000-06-01T14:00:00+02:00 CEST"
/// string(29) "2000-06-01T12:00:00+00:00 UTC"
/// ```
pub(crate) fn immutable_set_timezone(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let timezone = object_of(req, 1, "DateTimeZone")?;
    let object = this(req)?;
    let copy = clone_date(req, object)?;
    if let Err(thrown) = timezone_set(req, copy, timezone) {
        req.release(copy.as_value());
        return Err(thrown);
    }
    Ok(copy.as_value())
}

/// `date_offset_get(DateTimeInterface $object): int`, and `getOffset`.
///
/// ```php
/// var_dump(date_offset_get(new DateTime('2000-07-01 Europe/Amsterdam')), (new DateTime('2000-01-01 +0545'))->getOffset(), (new DateTimeImmutable('2000-01-01 EDT'))->getOffset(), (new DateTime('@0'))->getOffset());
/// ```
/// ```expect
/// int(7200)
/// int(20700)
/// int(-14400)
/// int(0)
/// ```
pub(crate) fn date_offset_get(req: &mut Request) -> Result<Value, Thrown> {
    let method = req.this().is_some();
    count_between(req, usize::from(!method), usize::from(!method))?;
    let (object, _) = receiver(req, method, "DateTimeInterface")?;
    let time = time_of(req, object)?;
    if time.is_localtime == 0 {
        return Ok(Value::int(0));
    }
    Ok(Value::int(match time.zone_type {
        ZONETYPE_ID => time
            .zone()
            .map_or(0, |zone| i64::from(zone.offset_at(time.sse).offset)),
        ZONETYPE_OFFSET => i64::from(time.z),
        ZONETYPE_ABBR => i64::from(time.z + 3600 * time.dst),
        _ => 0,
    }))
}

/// `php_date_time_set`.
fn time_set(req: &mut Request, object: PhpObject, at: u32, min: usize) -> Result<(), Thrown> {
    count_between(req, min, min + 2)?;
    let h = long(req, at)?;
    let i = long(req, at + 1)?;
    let s = optional_long(req, at + 2, 0)?;
    let us = optional_long(req, at + 3, 0)?;
    let mut time = time_of(req, object)?;
    time.h = h;
    time.i = i;
    time.s = s;
    time.us = us;
    time.update_ts(None);
    time.update_from_sse();
    set_time(req, object, Some(time));
    Ok(())
}

/// `php_date_date_set`.
fn date_set(req: &mut Request, object: PhpObject, at: u32, min: usize) -> Result<(), Thrown> {
    count_between(req, min, min)?;
    let y = long(req, at)?;
    let m = long(req, at + 1)?;
    let d = long(req, at + 2)?;
    let mut time = time_of(req, object)?;
    time.y = y;
    time.m = m;
    time.d = d;
    time.update_ts(None);
    set_time(req, object, Some(time));
    Ok(())
}

/// `php_date_isodate_set`.
fn isodate_set(req: &mut Request, object: PhpObject, at: u32, min: usize) -> Result<(), Thrown> {
    count_between(req, min, min + 1)?;
    let y = long(req, at)?;
    let w = long(req, at + 1)?;
    let d = optional_long(req, at + 2, 1)?;
    let mut time = time_of(req, object)?;
    time.y = y;
    time.m = 1;
    time.d = 1;
    time.relative = RelTime::default();
    time.relative.d = ffi::daynr_from_weeknr(y, w, d);
    time.have_relative = 1;
    time.update_ts(None);
    set_time(req, object, Some(time));
    Ok(())
}

/// `php_date_timestamp_set`.
fn timestamp_set(req: &mut Request, object: PhpObject, at: u32, min: usize) -> Result<(), Thrown> {
    count_between(req, min, min)?;
    let ts = long(req, at)?;
    let mut time = time_of(req, object)?;
    time.unixtime2local(ts);
    time.update_ts(None);
    time.us = 0;
    set_time(req, object, Some(time));
    Ok(())
}

/// One setter as its function (object at 1), its `DateTime` method, or
/// its `DateTimeImmutable` method (on a clone).
fn setter(
    req: &mut Request,
    body: fn(&mut Request, PhpObject, u32, usize) -> Result<(), Thrown>,
    min: usize,
    immutable: bool,
) -> Result<Value, Thrown> {
    let method = req.this().is_some();
    if !method {
        let object = object_of(req, 1, "DateTime")?;
        body(req, object, 2, min + 1)?;
        return Ok(this_again(req, object));
    }
    let object = this(req)?;
    if !immutable {
        body(req, object, 1, min)?;
        return Ok(this_again(req, object));
    }
    let copy = clone_date(req, object)?;
    if let Err(thrown) = body(req, copy, 1, min) {
        req.release(copy.as_value());
        return Err(thrown);
    }
    Ok(copy.as_value())
}

/// `date_time_set(DateTime $object, int $hour, int $minute, int $second
/// = 0, int $microsecond = 0): DateTime`, and `DateTime::setTime`.
///
/// ```php
/// $d = new DateTime('2000-01-01 UTC');
/// var_dump(date_time_set($d, 25, 61, 61)->format('c'), $d->setTime(1, 2, 3, 456789)->format('c u'), $d->setTime(-1, 0)->format('c'));
/// ```
/// ```expect
/// string(25) "2000-01-02T02:02:01+00:00"
/// string(32) "2000-01-02T01:02:03+00:00 456789"
/// string(25) "2000-01-01T23:00:00+00:00"
/// ```
pub(crate) fn date_time_set(req: &mut Request) -> Result<Value, Thrown> {
    setter(req, time_set, 2, false)
}

/// `DateTimeImmutable::setTime(...)`.
///
/// ```php
/// $d = new DateTimeImmutable('2000-01-01 UTC');
/// var_dump($d->setTime(12, 30)->format('c'), $d->format('c'));
/// ```
/// ```expect
/// string(25) "2000-01-01T12:30:00+00:00"
/// string(25) "2000-01-01T00:00:00+00:00"
/// ```
pub(crate) fn immutable_set_time(req: &mut Request) -> Result<Value, Thrown> {
    setter(req, time_set, 2, true)
}

/// `date_date_set(DateTime $object, int $year, int $month, int $day):
/// DateTime`, and `DateTime::setDate`.
///
/// ```php
/// $d = new DateTime('2000-01-01 10:00 UTC');
/// var_dump(date_date_set($d, 2004, 2, 30)->format('c'), $d->setDate(-5, 13, 0)->format('c'));
/// ```
/// ```expect
/// string(25) "2004-03-01T10:00:00+00:00"
/// string(25) "-005-12-31T10:00:00+00:00"
/// ```
pub(crate) fn date_date_set(req: &mut Request) -> Result<Value, Thrown> {
    setter(req, date_set, 3, false)
}

/// `DateTimeImmutable::setDate(...)`.
///
/// ```php
/// $d = new DateTimeImmutable('2000-01-01 UTC');
/// var_dump($d->setDate(2020, 12, 25)->format('D, d M Y'), $d->format('Y'));
/// ```
/// ```expect
/// string(16) "Fri, 25 Dec 2020"
/// string(4) "2000"
/// ```
pub(crate) fn immutable_set_date(req: &mut Request) -> Result<Value, Thrown> {
    setter(req, date_set, 3, true)
}

/// `date_isodate_set(DateTime $object, int $year, int $week, int
/// $dayOfWeek = 1): DateTime`, and `DateTime::setISODate`.
///
/// ```php
/// $d = new DateTime('2000-01-01 UTC');
/// var_dump(date_isodate_set($d, 2008, 1)->format('Y-m-d l'), $d->setISODate(2009, 53, 7)->format('Y-m-d l W o'));
/// ```
/// ```expect
/// string(17) "2007-12-31 Monday"
/// string(25) "2010-01-03 Sunday 53 2009"
/// ```
pub(crate) fn date_isodate_set(req: &mut Request) -> Result<Value, Thrown> {
    setter(req, isodate_set, 2, false)
}

/// `DateTimeImmutable::setISODate(...)`.
///
/// ```php
/// var_dump((new DateTimeImmutable('2000-01-01 UTC'))->setISODate(2015, 10, 3)->format('Y-m-d'));
/// ```
/// ```expect
/// string(10) "2015-03-04"
/// ```
pub(crate) fn immutable_set_isodate(req: &mut Request) -> Result<Value, Thrown> {
    setter(req, isodate_set, 2, true)
}

/// `date_timestamp_set(DateTime $object, int $timestamp): DateTime`,
/// and `DateTime::setTimestamp`.
///
/// ```php
/// $d = new DateTime('2000-01-01 10:00:00.5', new DateTimeZone('Pacific/Auckland'));
/// var_dump(date_timestamp_set($d, 1234567890)->format('c u'), $d->setTimestamp(-1)->format('c'));
/// ```
/// ```expect
/// string(32) "2009-02-14T12:31:30+13:00 000000"
/// string(25) "1970-01-01T11:59:59+12:00"
/// ```
pub(crate) fn date_timestamp_set(req: &mut Request) -> Result<Value, Thrown> {
    setter(req, timestamp_set, 1, false)
}

/// `DateTimeImmutable::setTimestamp(...)`.
///
/// ```php
/// var_dump((new DateTimeImmutable('now', new DateTimeZone('UTC')))->setTimestamp(86400 * 365)->format('c'));
/// ```
/// ```expect
/// string(25) "1971-01-01T00:00:00+00:00"
/// ```
pub(crate) fn immutable_set_timestamp(req: &mut Request) -> Result<Value, Thrown> {
    setter(req, timestamp_set, 1, true)
}

/// `setMicrosecond(int $microsecond): static` of either class.
fn set_microsecond(req: &mut Request, method: &str, immutable: bool) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let us = long(req, 1)?;
    if !(0..=999_999).contains(&us) {
        return Err(throw(
            req,
            "DateRangeError",
            &format!(
                "{method}(): Argument #1 ($microsecond) must be between 0 and 999999, {us} given"
            ),
        ));
    }
    let object = this(req)?;
    let mut time = time_of(req, object)?;
    time.us = us;
    let target = if immutable {
        clone_date(req, object)?
    } else {
        object
    };
    set_time(req, target, Some(time));
    Ok(if immutable {
        target.as_value()
    } else {
        this_again(req, object)
    })
}

/// `DateTime::setMicrosecond(int $microsecond): static`.
///
/// ```php
/// $d = new DateTime('2000-01-01 UTC');
/// var_dump($d->setMicrosecond(123456)->format('u'), $d->getMicrosecond());
/// try { $d->setMicrosecond(1000000); } catch (DateRangeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(6) "123456"
/// int(123456)
/// DateTime::setMicrosecond(): Argument #1 ($microsecond) must be between 0 and 999999, 1000000 given
/// ```
pub(crate) fn datetime_set_microsecond(req: &mut Request) -> Result<Value, Thrown> {
    set_microsecond(req, "DateTime::setMicrosecond", false)
}

/// `DateTimeImmutable::setMicrosecond(int $microsecond): static`.
///
/// ```php
/// $d = new DateTimeImmutable('2000-01-01 UTC');
/// var_dump($d->setMicrosecond(7)->format('u'), $d->format('u'));
/// try { $x = $d->setMicrosecond(-1); } catch (DateRangeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(6) "000007"
/// string(6) "000000"
/// DateTimeImmutable::setMicrosecond(): Argument #1 ($microsecond) must be between 0 and 999999, -1 given
/// ```
pub(crate) fn immutable_set_microsecond(req: &mut Request) -> Result<Value, Thrown> {
    set_microsecond(req, "DateTimeImmutable::setMicrosecond", true)
}

/// `date_timestamp_get(DateTimeInterface $object): int`, and
/// `getTimestamp`: the `DateRangeError` where the epoch does not fit.
///
/// ```php
/// var_dump(date_timestamp_get(new DateTime('2000-01-01 UTC')), (new DateTimeImmutable('1900-01-01 Europe/Paris'))->getTimestamp());
/// try { (new DateTime('292277026596-12-04 15:30:08 UTC'))->modify('+1 year')->getTimestamp(); } catch (DateRangeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(946684800)
/// int(-2208989361)
/// ```
pub(crate) fn date_timestamp_get(req: &mut Request) -> Result<Value, Thrown> {
    let method = req.this().is_some();
    count_between(req, usize::from(!method), usize::from(!method))?;
    let (object, _) = receiver(req, method, "DateTimeInterface")?;
    let mut time = time_of(req, object)?;
    if time.sse_uptodate == 0 {
        time.update_ts(None);
    }
    match time.date_to_int() {
        Some(ts) => Ok(Value::int(ts)),
        None => Err(throw(
            req,
            "DateRangeError",
            "Epoch doesn't fit in a PHP integer",
        )),
    }
}

/// `DateTimeInterface::getMicrosecond(): int`.
///
/// ```php
/// var_dump((new DateTime('2000-01-01 00:00:00.000321 UTC'))->getMicrosecond(), (new DateTimeImmutable('@5'))->getMicrosecond());
/// ```
/// ```expect
/// int(321)
/// int(0)
/// ```
pub(crate) fn get_microsecond(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    let object = this(req)?;
    Ok(Value::int(time_of(req, object)?.us))
}

/// `date_diff(DateTimeInterface $baseObject, DateTimeInterface
/// $targetObject, bool $absolute = false): DateInterval`, and `diff`.
///
/// ```php
/// $a = new DateTime('2000-02-28 10:00:00 UTC'); $b = new DateTimeImmutable('2001-03-01 09:30:15.5 UTC');
/// var_dump(date_diff($a, $b), $b->diff($a)->format('%R %y %m %d %h %i %s %f %a'), $b->diff($a, true)->invert);
/// $c = new DateTime('2021-03-27 12:00 Europe/Amsterdam'); $d = new DateTime('2021-03-28 12:00 Europe/Amsterdam');
/// var_dump($c->diff($d)->format('%d %h %a'));
/// ```
/// ```expect
/// object(DateInterval)#3 (10) {
///   ["y"]=>
///   int(1)
///   ["m"]=>
///   int(0)
///   ["d"]=>
///   int(0)
///   ["h"]=>
///   int(23)
///   ["i"]=>
///   int(30)
///   ["s"]=>
///   int(15)
///   ["f"]=>
///   float(0.5)
///   ["invert"]=>
///   int(0)
///   ["days"]=>
///   int(366)
///   ["from_string"]=>
///   bool(false)
/// }
/// string(27) "- 1 0 1 23 30 15 500000 366"
/// int(0)
/// string(5) "1 0 1"
/// ```
pub(crate) fn date_diff(req: &mut Request) -> Result<Value, Thrown> {
    let method = req.this().is_some();
    let shift = usize::from(!method);
    count_between(req, 1 + shift, 2 + shift)?;
    let (one, at) = receiver(req, method, "DateTimeInterface")?;
    let two = object_of(req, at, "DateTimeInterface")?;
    let absolute = optional_bool(req, at + 1, false)?;
    let a = time_of(req, one)?;
    let b = time_of(req, two)?;
    let mut diff = a.diff(&b);
    if absolute {
        diff.invert = 0;
    }
    new_interval(req, diff, CIVIL, None)
}

/// `php_date_initialize_from_hash`: `date`, `timezone_type` and
/// `timezone` of a serialised date, read back into the object.
pub(super) fn initialize_from_hash(
    req: &mut Request,
    object: PhpObject,
    hash: PhpArray,
) -> Result<bool, Thrown> {
    let find = |req: &mut Request, name: &[u8]| -> Result<Option<Value>, Thrown> {
        let key = Key::Str(php_ext::values::PhpString::new(req, name)?);
        Ok(hash.get(&key).map(|v| v.deref()))
    };
    let Some(date) = find(req, b"date")?.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec)) else {
        return Ok(false);
    };
    let Some(zone_type) = find(req, b"timezone_type")?.and_then(|v| v.as_int()) else {
        return Ok(false);
    };
    let Some(zone) = find(req, b"timezone")?.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Ok(false);
    };
    let init = Init {
        ctor: false,
        format: false,
    };
    match u32::try_from(zone_type).unwrap_or(0) {
        ZONETYPE_OFFSET | ZONETYPE_ABBR => {
            let text = [date.as_slice(), b" ", zone.as_slice()].concat();
            initialize(req, object, &text, None, None, init)
        }
        ZONETYPE_ID => {
            let Some(tzi) = parse_tzfile(req, &zone) else {
                return Ok(false);
            };
            let tz = new_timezone(req, TzKind::Id(tzi))?;
            let tz_object = tz.as_object().copied();
            let done = initialize(req, object, &date, None, tz_object, init);
            req.release(tz);
            done
        }
        _ => Ok(false),
    }
}

/// `__set_state(array $array)` of either class: always of the class
/// itself, as `php_date_instantiate(date_ce_date)` makes it.
fn set_state(req: &mut Request, class: &str) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let hash = array(req, 1)?;
    let class_id = class_id(req, class)?;
    let object = new_object(req, class_id)?;
    match initialize_from_hash(req, object, hash) {
        Ok(true) => Ok(object.as_value()),
        Ok(false) => {
            req.release(object.as_value());
            Err(throw(
                req,
                "Error",
                &format!("Invalid serialization data for {class} object"),
            ))
        }
        Err(thrown) => {
            req.release(object.as_value());
            Err(thrown)
        }
    }
}

/// `DateTime::__set_state(array $array): DateTime`.
///
/// ```php
/// var_dump(DateTime::__set_state(['date' => '2000-01-01 00:00:00.000000', 'timezone_type' => 3, 'timezone' => 'Europe/Riga']), DateTime::__set_state(['date' => '2000-01-01 00:00:00', 'timezone_type' => 1, 'timezone' => '+01:00'])->format('c'));
/// try { DateTime::__set_state(['date' => 1]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_export(new DateTime('2000-01-01 12:00 UTC'));
/// ```
/// ```expect
/// object(DateTime)#1 (3) {
///   ["date"]=>
///   string(26) "2000-01-01 00:00:00.000000"
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(11) "Europe/Riga"
/// }
/// string(25) "2000-01-01T00:00:00+01:00"
/// Invalid serialization data for DateTime object
/// \DateTime::__set_state(array(
///    'date' => '2000-01-01 12:00:00.000000',
///    'timezone_type' => 3,
///    'timezone' => 'UTC',
/// ))
/// ```
pub(crate) fn datetime_set_state(req: &mut Request) -> Result<Value, Thrown> {
    set_state(req, "DateTime")
}

/// `DateTimeImmutable::__set_state(array $array): DateTimeImmutable`.
///
/// ```php
/// var_dump(DateTimeImmutable::__set_state(['date' => '2000-01-01 00:00:00.000000', 'timezone_type' => 2, 'timezone' => 'EST'])->format('c T'));
/// ```
/// ```expect
/// string(29) "2000-01-01T00:00:00-05:00 EST"
/// ```
pub(crate) fn immutable_set_state(req: &mut Request) -> Result<Value, Thrown> {
    set_state(req, "DateTimeImmutable")
}

/// `__serialize(): array` of either class: the three fields, then the
/// object's own properties.
///
/// ```php
/// class D extends DateTime { public $extra = 1; protected $p = 2; }
/// $s = (new D('2000-01-01 00:00 UTC'))->__serialize();
/// var_dump(json_encode($s), (new DateTimeImmutable('2000-01-01 +02:00'))->__serialize());
/// ```
/// ```expect
/// string(101) "{"date":"2000-01-01 00:00:00.000000","timezone_type":3,"timezone":"UTC","extra":1,"\u0000*\u0000p":2}"
/// array(3) {
///   ["date"]=>
///   string(26) "2000-01-01 00:00:00.000000"
///   ["timezone_type"]=>
///   int(1)
///   ["timezone"]=>
///   string(6) "+02:00"
/// }
/// ```
pub(crate) fn serialize(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    let object = this(req)?;
    let time = time_of(req, object)?;
    let fields = date_fields(req, &time)?;
    let own = std_rows(req, object)?;
    Ok(Value::array(rows_array(req, &own, fields, true)?))
}

/// `restore_custom_*_properties`: every row of the hash but the class's
/// own fields written back as a property.
pub(super) fn restore_properties(
    req: &mut Request,
    object: PhpObject,
    hash: PhpArray,
    internal: &[&[u8]],
) -> Result<(), Thrown> {
    for (key, value) in hash.snapshot() {
        let Key::Str(name) = key else {
            continue;
        };
        if matches!(value.as_ref(), ValueRef::Reference(_)) || internal.contains(&name.as_bytes()) {
            continue;
        }
        let bytes = name.as_bytes();
        let plain: &[u8] = if bytes.first() == Some(&0) {
            bytes
                .iter()
                .skip(1)
                .position(|b| *b == 0)
                .map_or(bytes, |at| &bytes[at + 2..])
        } else {
            bytes
        };
        let plain = php_ext::values::PhpString::new(req, plain)?;
        req.addref(&value);
        php_ext::abi::Std.write_property(req, object, plain, value)?;
    }
    Ok(())
}

/// `__unserialize(array $data): void` of either class.
fn unserialize(req: &mut Request, class: &str) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let hash = array(req, 1)?;
    let object = this(req)?;
    if !initialize_from_hash(req, object, hash)? {
        return Err(throw(
            req,
            "Error",
            &format!("Invalid serialization data for {class} object"),
        ));
    }
    restore_properties(req, object, hash, &[b"date", b"timezone_type", b"timezone"])?;
    Ok(Value::null())
}

/// `DateTime::__unserialize(array $data): void`.
///
/// ```php
/// $d = new DateTime('2000-01-01 UTC');
/// $d->__unserialize(['date' => '2011-11-11 11:11:11.000000', 'timezone_type' => 3, 'timezone' => 'Asia/Dubai', 'note' => 'kept']);
/// var_dump($d);
/// try { $d->__unserialize([]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: Creation of dynamic property DateTime::$note is deprecated in %s on line 3
/// object(DateTime)#1 (4) {
///   ["note"]=>
///   string(4) "kept"
///   ["date"]=>
///   string(26) "2011-11-11 11:11:11.000000"
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(10) "Asia/Dubai"
/// }
/// Invalid serialization data for DateTime object
/// ```
pub(crate) fn datetime_unserialize(req: &mut Request) -> Result<Value, Thrown> {
    unserialize(req, "DateTime")
}

/// `DateTimeImmutable::__unserialize(array $data): void`.
///
/// ```php
/// $d = new DateTimeImmutable('2000-01-01 UTC');
/// $d->__unserialize(['date' => '1999-12-31 23:59:59.000000', 'timezone_type' => 1, 'timezone' => '-05:00']);
/// var_dump($d->format('c'));
/// ```
/// ```expect
/// string(25) "1999-12-31T23:59:59-05:00"
/// ```
pub(crate) fn immutable_unserialize(req: &mut Request) -> Result<Value, Thrown> {
    unserialize(req, "DateTimeImmutable")
}

/// `php_do_date_time_wakeup`: the object's own properties read back as
/// its time.
fn wakeup(req: &mut Request, class: &str) -> Result<Value, Thrown> {
    php_ext::raising::deprecated(
        req,
        &format!(
            "Method {class}::__wakeup() is deprecated since 8.5, this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize()"
        ),
    );
    count_between(req, 0, 0)?;
    let object = this(req)?;
    let mut hash = PhpArray::new(req)?;
    for (name, value) in object.props(req)? {
        req.addref(&value);
        hash.set(req, Key::Str(name), value)?;
    }
    let done = initialize_from_hash(req, object, hash);
    req.release(Value::array(hash));
    if !done? {
        return Err(throw(
            req,
            "Error",
            &format!("Invalid serialization data for {class} object"),
        ));
    }
    Ok(Value::null())
}

/// `DateTime::__wakeup(): void`, deprecated since 8.5.
///
/// ```php
/// $d = new DateTime('2000-01-01 UTC');
/// try { $d->__wakeup(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: Method DateTime::__wakeup() is deprecated since 8.5, this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize() in %s on line 3
/// Invalid serialization data for DateTime object
/// ```
pub(crate) fn datetime_wakeup(req: &mut Request) -> Result<Value, Thrown> {
    wakeup(req, "DateTime")
}

/// `DateTimeImmutable::__wakeup(): void`, deprecated since 8.5.
///
/// ```php
/// $d = new DateTimeImmutable('2000-01-01 UTC');
/// try { $d->__wakeup(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: Method DateTimeImmutable::__wakeup() is deprecated since 8.5, this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize() in %s on line 3
/// Invalid serialization data for DateTimeImmutable object
/// ```
pub(crate) fn immutable_wakeup(req: &mut Request) -> Result<Value, Thrown> {
    wakeup(req, "DateTimeImmutable")
}
