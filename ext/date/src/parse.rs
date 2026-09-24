//! What timelib's parsers answer, handed to PHP: `strtotime`,
//! `date_parse` and `date_parse_from_format` over
//! `php_date_do_return_parsed_time`, and `DATEG(last_errors)`.

use php_ext::raising::{Level, Thrown, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value};

use crate::State;
use crate::args::{count_between, nullable_long, set_str, set_string, string};
use crate::ffi::{Errors, Time, UNSET, ZONETYPE_ABBR, ZONETYPE_ID, ZONETYPE_OFFSET};
use crate::format::now;
use crate::zone::default_zone;

/// `TIMELIB_SPECIAL_WEEKDAY` and `TIMELIB_SPECIAL_FIRST_DAY_OF_MONTH`.
const SPECIAL_WEEKDAY: u32 = 0x01;
const FIRST_DAY_OF_MONTH: i32 = 0x01;

/// `Z_PARAM_PATH_STR`: the string, or php's `ValueError` for one with a
/// NUL inside it.
pub(super) fn path_string(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let s = string(req, position)?;
    if s.as_bytes().contains(&0) {
        return Err(arg_value_error(req, position, "not contain any null bytes"));
    }
    Ok(s)
}

/// `update_errors_warnings(&err)`: the messages kept as the request's
/// last where there are any, and none kept where there are none.
pub(super) fn keep_errors(req: &mut Request, errors: &Errors) {
    let state = req.state::<State>();
    state.last_errors =
        (!errors.warnings.is_empty() || !errors.errors.is_empty()).then(|| errors.clone());
}

/// `zval_from_error_container`: the counts and the messages by position.
fn add_errors(req: &mut Request, array: &mut PhpArray, errors: &Errors) -> Result<(), Thrown> {
    for (count, list, messages) in [
        (&b"warning_count"[..], &b"warnings"[..], &errors.warnings),
        (b"error_count", b"errors", &errors.errors),
    ] {
        let n = i64::try_from(messages.len()).unwrap_or(i64::MAX);
        set_str(req, array, count, Value::int(n))?;
        let mut element = PhpArray::new(req)?;
        for message in messages {
            let text = Value::string(PhpString::new(req, &message.text)?);
            element.set(req, Key::Int(i64::from(message.position)), text)?;
        }
        set_str(req, array, list, Value::array(element))?;
    }
    Ok(())
}

/// `strtotime(string $datetime, ?int $baseTimestamp = null): int|false`:
/// the parse filled from the base time in the default zone.
///
/// ```php
/// date_default_timezone_set('Europe/Amsterdam');
/// $base = 1700000000;
/// foreach (['now', 'tomorrow', '+1 week 2 days', 'last monday of next month', 'first day of this month midnight', '2024-02-29 13:45:10', '@86400', 'next friday 17:00', '2021-03-28 02:30:00', 'yesterday noon UTC', '10 September 2000 +0200', 'Sat, 01 Jan 2000 00:00:00 GMT'] as $s) {
///     echo str_pad($s, 34), strtotime($s, $base), "\n";
/// }
/// var_dump(strtotime(''), strtotime('garbage words'), strtotime('1 January 294277000000 UTC'));
/// ```
/// ```expect
/// now                               1700000000
/// tomorrow                          1700002800
/// +1 week 2 days                    1700777600
/// last monday of next month         1703458800
/// first day of this month midnight  1698793200
/// 2024-02-29 13:45:10               1709210710
/// @86400                            86400
/// next friday 17:00                 1700236800
/// 2021-03-28 02:30:00               1616895000
/// yesterday noon UTC                1699876800
/// 10 September 2000 +0200           968536800
/// Sat, 01 Jan 2000 00:00:00 GMT     946684800
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
pub(crate) fn strtotime(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 2)?;
    let text = string(req, 1)?.as_bytes().to_vec();
    let base = nullable_long(req, 2)?;
    if text.is_empty() {
        return Ok(Value::bool(false));
    }
    let zone = default_zone(req)?;
    let mut base_time = Time::new();
    base_time.set_zone_id(zone.clone());
    base_time.unixtime2local(base.unwrap_or_else(now));
    let (mut t, errors) = Time::strtotime(&text);
    if !errors.errors.is_empty() {
        return Ok(Value::bool(false));
    }
    t.fill_holes(&mut base_time, crate::ffi::NO_CLONE);
    t.update_ts(Some(&zone));
    let Some(ts) = t.date_to_int() else {
        docref(req, Level::Warning, "Epoch doesn't fit in a PHP integer");
        return Ok(Value::bool(false));
    };
    Ok(Value::int(ts))
}

/// `PHP_DATE_PARSE_DATE_SET_TIME_ELEMENT`: `false` for a field the parse
/// left unset, the number otherwise.
fn element(req: &mut Request, array: &mut PhpArray, name: &[u8], n: i64) -> Result<(), Thrown> {
    let value = if n == UNSET {
        Value::bool(false)
    } else {
        Value::int(n)
    };
    set_str(req, array, name, value)
}

/// `php_date_do_return_parsed_time`.
fn parsed_time(req: &mut Request, t: &Time, errors: &Errors) -> Result<Value, Thrown> {
    let mut array = PhpArray::new(req)?;
    for (name, n) in [
        (&b"year"[..], t.y),
        (b"month", t.m),
        (b"day", t.d),
        (b"hour", t.h),
        (b"minute", t.i),
        (b"second", t.s),
    ] {
        element(req, &mut array, name, n)?;
    }
    #[allow(clippy::cast_precision_loss)]
    let fraction = if t.us == UNSET {
        Value::bool(false)
    } else {
        Value::float(t.us as f64 / 1_000_000.0)
    };
    set_str(req, &mut array, b"fraction", fraction)?;
    add_errors(req, &mut array, errors)?;
    set_str(
        req,
        &mut array,
        b"is_localtime",
        Value::bool(t.is_localtime != 0),
    )?;
    if t.is_localtime != 0 {
        element(req, &mut array, b"zone_type", i64::from(t.zone_type))?;
        match t.zone_type {
            ZONETYPE_OFFSET => {
                element(req, &mut array, b"zone", i64::from(t.z))?;
                set_str(req, &mut array, b"is_dst", Value::bool(t.dst != 0))?;
            }
            ZONETYPE_ID => {
                if !t.tz_abbr().is_empty() {
                    set_string(req, &mut array, b"tz_abbr", t.tz_abbr())?;
                }
                if let Some(zone) = t.zone() {
                    set_string(req, &mut array, b"tz_id", zone.name())?;
                }
            }
            ZONETYPE_ABBR => {
                element(req, &mut array, b"zone", i64::from(t.z))?;
                set_str(req, &mut array, b"is_dst", Value::bool(t.dst != 0))?;
                set_string(req, &mut array, b"tz_abbr", t.tz_abbr())?;
            }
            _ => {}
        }
    }
    if t.have_relative != 0 {
        let r = &t.relative;
        let mut rel = PhpArray::new(req)?;
        for (name, n) in [
            (&b"year"[..], r.y),
            (b"month", r.m),
            (b"day", r.d),
            (b"hour", r.h),
            (b"minute", r.i),
            (b"second", r.s),
        ] {
            set_str(req, &mut rel, name, Value::int(n))?;
        }
        if r.have_weekday_relative != 0 {
            set_str(req, &mut rel, b"weekday", Value::int(i64::from(r.weekday)))?;
        }
        if r.have_special_relative != 0 && r.special_type == SPECIAL_WEEKDAY {
            set_str(req, &mut rel, b"weekdays", Value::int(r.special_amount))?;
        }
        if r.first_last_day_of != 0 {
            let name: &[u8] = if r.first_last_day_of == FIRST_DAY_OF_MONTH {
                b"first_day_of_month"
            } else {
                b"last_day_of_month"
            };
            set_str(req, &mut rel, name, Value::bool(true))?;
        }
        set_str(req, &mut array, b"relative", Value::array(rel))?;
    }
    Ok(Value::array(array))
}

/// `date_parse(string $datetime): array`: every field timelib's parser
/// found, and what it could not read.
///
/// ```php
/// var_dump(date_parse('2006-12-12 10:00:00.5 +1 week +1 hour'));
/// var_dump(date_parse('Europe/Amsterdam 2021-02-03 last day of next month'));
/// var_dump(date_parse('next monday EDT'));
/// var_dump(date_parse('2009-02-30 25:61 xx'));
/// ```
/// ```expect
/// array(13) {
///   ["year"]=>
///   int(2006)
///   ["month"]=>
///   int(12)
///   ["day"]=>
///   int(12)
///   ["hour"]=>
///   int(10)
///   ["minute"]=>
///   int(0)
///   ["second"]=>
///   int(0)
///   ["fraction"]=>
///   float(0.5)
///   ["warning_count"]=>
///   int(0)
///   ["warnings"]=>
///   array(0) {
///   }
///   ["error_count"]=>
///   int(0)
///   ["errors"]=>
///   array(0) {
///   }
///   ["is_localtime"]=>
///   bool(false)
///   ["relative"]=>
///   array(6) {
///     ["year"]=>
///     int(0)
///     ["month"]=>
///     int(0)
///     ["day"]=>
///     int(7)
///     ["hour"]=>
///     int(1)
///     ["minute"]=>
///     int(0)
///     ["second"]=>
///     int(0)
///   }
/// }
/// array(15) {
///   ["year"]=>
///   int(2021)
///   ["month"]=>
///   int(2)
///   ["day"]=>
///   int(3)
///   ["hour"]=>
///   bool(false)
///   ["minute"]=>
///   bool(false)
///   ["second"]=>
///   bool(false)
///   ["fraction"]=>
///   bool(false)
///   ["warning_count"]=>
///   int(0)
///   ["warnings"]=>
///   array(0) {
///   }
///   ["error_count"]=>
///   int(0)
///   ["errors"]=>
///   array(0) {
///   }
///   ["is_localtime"]=>
///   bool(true)
///   ["zone_type"]=>
///   int(3)
///   ["tz_id"]=>
///   string(16) "Europe/Amsterdam"
///   ["relative"]=>
///   array(7) {
///     ["year"]=>
///     int(0)
///     ["month"]=>
///     int(1)
///     ["day"]=>
///     int(0)
///     ["hour"]=>
///     int(0)
///     ["minute"]=>
///     int(0)
///     ["second"]=>
///     int(0)
///     ["last_day_of_month"]=>
///     bool(true)
///   }
/// }
/// array(17) {
///   ["year"]=>
///   bool(false)
///   ["month"]=>
///   bool(false)
///   ["day"]=>
///   bool(false)
///   ["hour"]=>
///   int(0)
///   ["minute"]=>
///   int(0)
///   ["second"]=>
///   int(0)
///   ["fraction"]=>
///   float(0)
///   ["warning_count"]=>
///   int(0)
///   ["warnings"]=>
///   array(0) {
///   }
///   ["error_count"]=>
///   int(0)
///   ["errors"]=>
///   array(0) {
///   }
///   ["is_localtime"]=>
///   bool(true)
///   ["zone_type"]=>
///   int(2)
///   ["zone"]=>
///   int(-18000)
///   ["is_dst"]=>
///   bool(true)
///   ["tz_abbr"]=>
///   string(3) "EDT"
///   ["relative"]=>
///   array(7) {
///     ["year"]=>
///     int(0)
///     ["month"]=>
///     int(0)
///     ["day"]=>
///     int(0)
///     ["hour"]=>
///     int(0)
///     ["minute"]=>
///     int(0)
///     ["second"]=>
///     int(0)
///     ["weekday"]=>
///     int(1)
///   }
/// }
/// array(13) {
///   ["year"]=>
///   int(2009)
///   ["month"]=>
///   int(2)
///   ["day"]=>
///   int(30)
///   ["hour"]=>
///   int(5)
///   ["minute"]=>
///   int(6)
///   ["second"]=>
///   int(0)
///   ["fraction"]=>
///   float(0)
///   ["warning_count"]=>
///   int(1)
///   ["warnings"]=>
///   array(1) {
///     [20]=>
///     string(27) "The parsed date was invalid"
///   }
///   ["error_count"]=>
///   int(3)
///   ["errors"]=>
///   array(3) {
///     [11]=>
///     string(20) "Unexpected character"
///     [15]=>
///     string(20) "Unexpected character"
///     [17]=>
///     string(47) "The timezone could not be found in the database"
///   }
///   ["is_localtime"]=>
///   bool(true)
///   ["zone_type"]=>
///   int(0)
/// }
/// ```
pub(crate) fn date_parse(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let text = string(req, 1)?.as_bytes().to_vec();
    let (t, errors) = Time::strtotime(&text);
    parsed_time(req, &t, &errors)
}

/// `date_parse_from_format(string $format, string $datetime): array`.
///
/// ```php
/// var_dump(date_parse_from_format('j.n.Y H:iP', '6.1.2009 13:00+01:00'));
/// var_dump(date_parse_from_format('Y-m-d', '2009-02-30 extra'));
/// try { date_parse_from_format('Y', "20\0"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(15) {
///   ["year"]=>
///   int(2009)
///   ["month"]=>
///   int(1)
///   ["day"]=>
///   int(6)
///   ["hour"]=>
///   int(13)
///   ["minute"]=>
///   int(0)
///   ["second"]=>
///   int(0)
///   ["fraction"]=>
///   float(0)
///   ["warning_count"]=>
///   int(0)
///   ["warnings"]=>
///   array(0) {
///   }
///   ["error_count"]=>
///   int(0)
///   ["errors"]=>
///   array(0) {
///   }
///   ["is_localtime"]=>
///   bool(true)
///   ["zone_type"]=>
///   int(1)
///   ["zone"]=>
///   int(3600)
///   ["is_dst"]=>
///   bool(false)
/// }
/// array(12) {
///   ["year"]=>
///   int(2009)
///   ["month"]=>
///   int(2)
///   ["day"]=>
///   int(30)
///   ["hour"]=>
///   bool(false)
///   ["minute"]=>
///   bool(false)
///   ["second"]=>
///   bool(false)
///   ["fraction"]=>
///   bool(false)
///   ["warning_count"]=>
///   int(1)
///   ["warnings"]=>
///   array(1) {
///     [10]=>
///     string(27) "The parsed date was invalid"
///   }
///   ["error_count"]=>
///   int(1)
///   ["errors"]=>
///   array(1) {
///     [10]=>
///     string(13) "Trailing data"
///   }
///   ["is_localtime"]=>
///   bool(false)
/// }
/// date_parse_from_format(): Argument #2 ($datetime) must not contain any null bytes
/// ```
pub(crate) fn date_parse_from_format(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 2, 2)?;
    let format = string(req, 1)?.as_bytes().to_vec();
    let text = path_string(req, 2)?.as_bytes().to_vec();
    let (t, errors) = Time::parse_from_format(&format, &text);
    parsed_time(req, &t, &errors)
}
