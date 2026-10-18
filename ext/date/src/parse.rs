//! What timelib's parsers answer, handed to PHP: `strtotime`,
//! `date_parse` and `date_parse_from_format` over
//! `php_date_do_return_parsed_time`, and `DATEG(last_errors)`.

use php_ext::abi::{OrFalse, Path};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use crate::ffi::{
    ErrorContainer, Errors, Parsed, RelFields, Time, UNSET, ZONETYPE_ABBR, ZONETYPE_ID,
    ZONETYPE_OFFSET,
};
use crate::format::now;
use crate::lent::{Lent, kept, lent, release};
use crate::obj::{set_str, set_string};
use crate::zone::default_zone;

/// `TIMELIB_SPECIAL_WEEKDAY` and `TIMELIB_SPECIAL_FIRST_DAY_OF_MONTH`.
const SPECIAL_WEEKDAY: u32 = 0x01;
const FIRST_DAY_OF_MONTH: i32 = 0x01;

/// `update_errors_warnings(&err)`: the last parse's container freed,
/// this one's kept as `DATEG(last_errors)` where it has messages and
/// freed where it has none; its messages answered, copied.
pub(super) fn keep_errors(lent: &mut Lent<'_>, container: Option<ErrorContainer>) -> Errors {
    let errors = container
        .as_ref()
        .map(ErrorContainer::messages)
        .unwrap_or_default();
    let state = lent.state();
    state.last_errors = None;
    if errors.any() {
        state.last_errors = container;
    }
    errors
}

/// `zval_from_error_container`: the counts and the messages by position.
fn add_errors(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    errors: &Errors,
) -> Result<(), Thrown> {
    for (count, list, messages) in [
        (&b"warning_count"[..], &b"warnings"[..], &errors.warnings),
        (b"error_count", b"errors", &errors.errors),
    ] {
        let n = i64::try_from(messages.len()).unwrap_or(i64::MAX);
        set_str(req, array, count, Owner::int(n))?;
        let mut element = PhpArray::new(req)?;
        for message in messages {
            let text = PhpString::new(req, &message.text)?.into_value();
            element.set(req, Owner::index(i64::from(message.position)), text)?;
        }
        set_str(req, array, list, element.into_value())?;
    }
    Ok(())
}

/// `date_get_last_errors`' array: `zval_from_error_container` alone.
pub(super) fn errors_value(req: &mut Request, errors: &Errors) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    add_errors(req, &mut array, errors)?;
    Ok(array)
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
#[php_function]
fn strtotime(
    req: &mut Request,
    datetime: PhpString,
    #[php(default = null)] baseTimestamp: Option<i64>,
) -> Result<OrFalse<i64>, Thrown> {
    let text = datetime.as_bytes().to_vec();
    let base = baseTimestamp;
    if text.is_empty() {
        return Ok(OrFalse::False);
    }
    let zone = default_zone(req)?;
    let ts = lent(req, |l| {
        let mut base_time = Time::new(l);
        base_time.set_zone_id(zone.clone());
        base_time.unixtime2local(l, base.unwrap_or_else(now));
        let Parsed {
            time: mut t,
            errors,
        } = Time::strtotime(l, &text);
        let failed = errors.is_some_and(|e| !e.messages().errors.is_empty());
        if failed {
            return None;
        }
        t.fill_holes(l, &mut base_time, crate::ffi::NO_CLONE);
        t.update_ts(l, Some(&zone));
        Some(t.date_to_int())
    })?;
    match ts {
        None => Ok(OrFalse::False),
        Some(None) => {
            docref(req, Level::Warning, "Epoch doesn't fit in a PHP integer");
            Ok(OrFalse::False)
        }
        Some(Some(ts)) => Ok(OrFalse::Value(ts)),
    }
}

/// `PHP_DATE_PARSE_DATE_SET_TIME_ELEMENT`: `false` for a field the parse
/// left unset, the number otherwise.
fn element(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    name: &[u8],
    n: i64,
) -> Result<(), Thrown> {
    let value = if n == UNSET {
        Value::bool(false)
    } else {
        Value::int(n)
    };
    set_str(req, array, name, Owner::take(req, value))
}

/// `php_date_do_return_parsed_time`.
fn parsed_time(
    req: &mut Request,
    t: &Time,
    container: Option<ErrorContainer>,
) -> Result<Owner<PhpArray>, Thrown> {
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
    set_str(req, &mut array, b"fraction", Owner::take(req, fraction))?;
    let errors = container
        .as_ref()
        .map(ErrorContainer::messages)
        .unwrap_or_default();
    add_errors(req, &mut array, &errors)?;
    // `timelib_error_container_dtor(error)` once its messages are read.
    release(req, container)?;
    set_str(
        req,
        &mut array,
        b"is_localtime",
        Owner::bool(t.is_localtime != 0),
    )?;
    if t.is_localtime != 0 {
        element(req, &mut array, b"zone_type", i64::from(t.zone_type))?;
        match t.zone_type {
            ZONETYPE_OFFSET => {
                element(req, &mut array, b"zone", i64::from(t.z))?;
                set_str(req, &mut array, b"is_dst", Owner::bool(t.dst != 0))?;
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
                set_str(req, &mut array, b"is_dst", Owner::bool(t.dst != 0))?;
                set_string(req, &mut array, b"tz_abbr", t.tz_abbr())?;
            }
            _ => {}
        }
    }
    if t.have_relative != 0 {
        let rel = relative_row(req, &t.relative)?;
        set_str(req, &mut array, b"relative", rel.into_value())?;
    }
    Ok(array)
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
///
/// Its peak is php's: each row goes in under a key made after the table
/// has room, so at the ninth row, where the table doubles, no key stands
/// beside the old data and the new ([`PhpArray::set_str`]).
///
/// ```php
/// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
/// pk(fn () => date_parse('now'));
/// pk(fn () => date_parse('2006-12-12 10:00:00.5'));
/// pk(fn () => date_parse('2006-12-12 Europe/Amsterdam'));
/// pk(fn () => date_parse_from_format('Y-m-d', '2006-12-12'));
/// pk(fn () => date_parse('foo'));
/// pk(fn () => date_parse('+1 day'));
/// ```
/// ```expect capture
/// 1624 1264
/// 1624 1232
/// 1624 1352
/// 1624 1232
/// 1784 1512
/// 2096 1840
/// ```
#[php_function]
fn date_parse(req: &mut Request, datetime: PhpString) -> Result<Owner<PhpArray>, Thrown> {
    let parsed = lent(req, |l| Time::strtotime(l, datetime.as_bytes()))?;
    parsed_kept(req, parsed)
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
#[php_function]
fn date_parse_from_format(
    req: &mut Request,
    format: PhpString,
    datetime: Path,
) -> Result<Owner<PhpArray>, Thrown> {
    let parsed = lent(req, |l| {
        Time::parse_from_format(l, format.as_bytes(), datetime.0.as_bytes())
    })?;
    parsed_kept(req, parsed)
}

/// `php_date_do_return_parsed_time(..., parsed_time, error)`: the array
/// built while the parse's time stands, the container freed once its
/// messages are read and the time after the array, as php frees them.
fn parsed_kept(req: &mut Request, parsed: Parsed) -> Result<Owner<PhpArray>, Thrown> {
    let Parsed { time, errors } = parsed;
    kept(req, time, |req, time| parsed_time(req, time, errors))
}

/// The `relative` row of a parse's answer: each field of its relative
/// part, and the weekday, weekdays and first or last day of the month
/// where it has them.
fn relative_row(req: &mut Request, r: &RelFields) -> Result<Owner<PhpArray>, Thrown> {
    let mut rel = PhpArray::new(req)?;
    for (name, n) in [
        (&b"year"[..], r.y),
        (b"month", r.m),
        (b"day", r.d),
        (b"hour", r.h),
        (b"minute", r.i),
        (b"second", r.s),
    ] {
        set_str(req, &mut rel, name, Owner::int(n))?;
    }
    if r.have_weekday_relative != 0 {
        set_str(req, &mut rel, b"weekday", Owner::int(i64::from(r.weekday)))?;
    }
    if r.have_special_relative != 0 && r.special_type == SPECIAL_WEEKDAY {
        set_str(req, &mut rel, b"weekdays", Owner::int(r.special_amount))?;
    }
    if r.first_last_day_of != 0 {
        let name: &[u8] = if r.first_last_day_of == FIRST_DAY_OF_MONTH {
            b"first_day_of_month"
        } else {
            b"last_day_of_month"
        };
        set_str(req, &mut rel, name, Owner::bool(true))?;
    }
    Ok(rel)
}
