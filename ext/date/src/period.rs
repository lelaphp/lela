//! `DatePeriod`: a start, an interval, and an end or a count of
//! recurrences, read from objects or from an ISO 8601 string, with its
//! getters and its serialised form.

use php_ext::call::new_object;
use php_ext::raising::{Thrown, deprecated, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::args::{array, count_between, long, optional_long, string};
use crate::datetime::restore_properties;
use crate::ffi::{self, Time};
use crate::obj::{
    DateObj, IntervalObj, PERIOD_FIELDS, PeriodObj, class_id, new_date, new_interval,
    period_fields, rows_array, std_rows, this, uninitialized,
};

/// `PHP_DATE_PERIOD_EXCLUDE_START_DATE` and `PHP_DATE_PERIOD_INCLUDE_END_DATE`.
pub(super) const EXCLUDE_START_DATE: i64 = 0x0001;
pub(super) const INCLUDE_END_DATE: i64 = 0x0002;

/// `INT_MAX - 8`, the most recurrences a period may have.
const MAX_RECURRENCES: i64 = 2_147_483_647 - 8;

/// What an ISO 8601 period string gives: its start, end, interval and
/// recurrence count.
struct Iso {
    start: Option<Time>,
    end: Option<Time>,
    interval: Option<ffi::RelTime>,
    recurrences: i64,
}

/// `date_period_init_iso8601_string`: the string read, and php's
/// `DateMalformedPeriodStringException` for a part it lacks.
fn from_iso(req: &mut Request, func: &str, text: &[u8]) -> Result<Iso, Thrown> {
    let parsed = ffi::strtointerval(text);
    let shown = String::from_utf8_lossy(text).into_owned();
    if !parsed.errors.errors.is_empty() {
        return Err(throw(
            req,
            "DateMalformedPeriodStringException",
            &format!("Unknown or bad format ({shown})"),
        ));
    }
    let malformed = |req: &mut Request, what: &str| {
        throw(
            req,
            "DateMalformedPeriodStringException",
            &format!("{func}(): ISO interval must contain {what}, \"{shown}\" given"),
        )
    };
    let Some(mut start) = parsed.begin else {
        return Err(malformed(req, "a start date"));
    };
    if parsed.period.is_none() {
        return Err(malformed(req, "an interval"));
    }
    if parsed.end.is_none() && parsed.recurrences == 0 {
        return Err(malformed(req, "an end date or a recurrence count"));
    }
    start.update_ts(None);
    let end = parsed.end.map(|mut e| {
        e.update_ts(None);
        e
    });
    Ok(Iso {
        start: Some(start),
        end,
        interval: parsed.period,
        recurrences: i64::from(parsed.recurrences),
    })
}

/// `date_period_init_finish`: the options and the recurrence count,
/// held to php's bounds.
fn finish(
    req: &mut Request,
    func: &str,
    period: &mut PeriodObj,
    options: i64,
    recurrences: i64,
) -> Result<(), Thrown> {
    if period.end.is_none() && !(1..=MAX_RECURRENCES).contains(&recurrences) {
        return Err(throw(
            req,
            "DateMalformedPeriodStringException",
            &format!(
                "{func}(): Recurrence count must be greater or equal to 1 and lower than {}",
                MAX_RECURRENCES + 1
            ),
        ));
    }
    period.include_start_date = options & EXCLUDE_START_DATE == 0;
    period.include_end_date = options & INCLUDE_END_DATE != 0;
    let total =
        recurrences + i64::from(period.include_start_date) + i64::from(period.include_end_date);
    if total > MAX_RECURRENCES {
        return Err(throw(
            req,
            "DateMalformedStringException",
            &format!(
                "{func}(): Recurrence count must be greater or equal to 1 and lower than {} (including options)",
                MAX_RECURRENCES + 1
            ),
        ));
    }
    period.recurrences = total;
    period.initialized = true;
    Ok(())
}

/// The period's payload set.
fn store(req: &mut Request, object: PhpObject, period: PeriodObj) {
    if let Some(payload) = object.native_mut::<PeriodObj>(req) {
        *payload = period;
    }
}

/// `DatePeriod::createFromISO8601String`, of the called class, over
/// `string $specification` and `int $options = 0`: its dates are
/// `DateTimeImmutable`s.
///
/// ```php
/// $p = DatePeriod::createFromISO8601String('R4/2012-07-01T00:00:00Z/P7D');
/// var_dump($p->getStartDate(), $p->getRecurrences(), $p->getEndDate(), $p->include_start_date);
/// try { DatePeriod::createFromISO8601String('R4/P7D'); } catch (DateMalformedPeriodStringException $e) { echo $e->getMessage(), "\n"; }
/// try { DatePeriod::createFromISO8601String('2012-07-01T00:00:00Z/P7D'); } catch (DateMalformedPeriodStringException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(DateTimeImmutable)#2 (3) {
///   ["date"]=>
///   string(26) "2012-07-01 00:00:00.000000"
///   ["timezone_type"]=>
///   int(1)
///   ["timezone"]=>
///   string(6) "+00:00"
/// }
/// int(4)
/// NULL
/// bool(true)
/// DatePeriod::createFromISO8601String(): ISO interval must contain a start date, "R4/P7D" given
/// DatePeriod::createFromISO8601String(): ISO interval must contain an end date or a recurrence count, "2012-07-01T00:00:00Z/P7D" given
/// ```
pub(crate) fn create_from_iso8601_string(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 2)?;
    let text = string(req, 1)?.as_bytes().to_vec();
    let options = optional_long(req, 2, 0)?;
    let class = match req.called_scope() {
        Some(class) => class,
        None => class_id(req, "DatePeriod")?,
    };
    let object = new_object(req, class)?;
    let built = (|| {
        let func = "DatePeriod::createFromISO8601String";
        let iso = from_iso(req, func, &text)?;
        let base = class_id(req, "DateTimeImmutable")?;
        let mut period = PeriodObj::default();
        period.start = iso.start;
        period.start_ce = Some(base);
        period.end = iso.end;
        period.interval = iso.interval;
        finish(req, func, &mut period, options, iso.recurrences)?;
        store(req, object, period);
        Ok(())
    })();
    match built {
        Ok(()) => Ok(object.as_value()),
        Err(thrown) => {
            req.release(object.as_value());
            Err(thrown)
        }
    }
}

/// Whether `value` is an object of the class named.
fn is_a(req: &mut Request, value: &Value, class: &str) -> Result<Option<PhpObject>, Thrown> {
    let Some(object) = value.deref().as_object().copied() else {
        return Ok(None);
    };
    let class = class_id(req, class)?;
    Ok(object.instance_of(req, class).then_some(object))
}

/// Whether `value` is one `l` takes quietly: an int, or a scalar that
/// reads as one.
fn is_long(value: &Value) -> bool {
    matches!(
        value.deref().as_ref(),
        ValueRef::Int(_) | ValueRef::Bool(_) | ValueRef::Null
    ) || value
        .deref()
        .as_float()
        .is_some_and(|f| f.fract() == 0.0 && f.is_finite())
        || value.deref().as_str_bytes().is_some_and(|s| {
            !s.is_empty() && std::str::from_utf8(s).is_ok_and(|t| t.trim().parse::<i64>().is_ok())
        })
}

/// `DatePeriod::__construct($start, $interval = UNKNOWN, $end = UNKNOWN,
/// $options = UNKNOWN)`: its three signatures tried in php's order, the
/// ISO string one deprecated since 8.3.
///
/// ```php
/// $s = new DateTime('2012-07-01 UTC'); $i = new DateInterval('P7D');
/// $p = new DatePeriod($s, $i, 4, DatePeriod::EXCLUDE_START_DATE);
/// var_dump($p->getRecurrences(), $p->recurrences, $p->include_start_date, get_class($p->getStartDate()));
/// $q = new DatePeriod($s, $i, new DateTime('2012-08-01 UTC'), DatePeriod::INCLUDE_END_DATE);
/// var_dump($q->getEndDate()->format('c'), $q->include_end_date, $q->getRecurrences());
/// $r = new DatePeriod('R2/2012-07-01T00:00:00Z/P1D');
/// var_dump(get_class($r->getStartDate()));
/// try { new DatePeriod([], 2); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// echo (new ReflectionMethod('DatePeriod', '__construct'))->getNumberOfRequiredParameters(), "\n";
/// try { new DatePeriod($s, $i, 0); } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(4)
/// int(4)
/// bool(false)
/// string(8) "DateTime"
/// string(25) "2012-08-01T00:00:00+00:00"
/// bool(true)
/// NULL
///
/// Deprecated: Calling DatePeriod::__construct(string $isostr, int $options = 0) is deprecated, use DatePeriod::createFromISO8601String() instead in %s on line 7
/// string(8) "DateTime"
/// DatePeriod::__construct() accepts (DateTimeInterface, DateInterval, int [, int]), or (DateTimeInterface, DateInterval, DateTime [, int]), or (string [, int]) as arguments
/// 1
/// DateMalformedPeriodStringException: DatePeriod::__construct(): Recurrence count must be greater or equal to 1 and lower than 2147483640
/// ```
pub(crate) fn construct(req: &mut Request) -> Result<Value, Thrown> {
    let func = "DatePeriod::__construct";
    let count = req.argument_count();
    let arg = |req: &Request, at: u32| req.argument(at).unwrap_or_default();
    let (a, b, c, d) = (arg(req, 1), arg(req, 2), arg(req, 3), arg(req, 4));
    let start = is_a(req, &a, "DateTimeInterface")?;
    let interval = is_a(req, &b, "DateInterval")?;
    let end = is_a(req, &c, "DateTimeInterface")?;
    let options_ok = count < 4 || is_long(&d);
    let object = this(req)?;
    let mut period = PeriodObj::default();
    let (options, recurrences) = if let (Some(start), Some(interval), true, true, true) = (
        start,
        interval,
        (3..=4).contains(&count),
        options_ok,
        end.is_none() && is_long(&c),
    ) {
        let recurrences = long(req, 3)?;
        let options = optional_long(req, 4, 0)?;
        from_objects(req, &mut period, start, interval, None)?;
        (options, recurrences)
    } else if let (Some(start), Some(interval), Some(end), true, true) =
        (start, interval, end, (3..=4).contains(&count), options_ok)
    {
        let options = optional_long(req, 4, 0)?;
        from_objects(req, &mut period, start, interval, Some(end))?;
        (options, 0)
    } else if (1..=2).contains(&count)
        && (count < 2 || is_long(&b))
        && !matches!(a.deref().as_ref(), ValueRef::Object(_) | ValueRef::Array(_))
    {
        let text = string(req, 1)?.as_bytes().to_vec();
        let options = optional_long(req, 2, 0)?;
        deprecated(
            req,
            "Calling DatePeriod::__construct(string $isostr, int $options = 0) is deprecated, use DatePeriod::createFromISO8601String() instead",
        );
        let iso = from_iso(req, func, &text)?;
        period.start = iso.start;
        period.end = iso.end;
        period.interval = iso.interval;
        period.start_ce = Some(class_id(req, "DateTime")?);
        (options, iso.recurrences)
    } else {
        return Err(throw(
            req,
            "TypeError",
            "DatePeriod::__construct() accepts (DateTimeInterface, DateInterval, int [, int]), or (DateTimeInterface, DateInterval, DateTime [, int]), or (string [, int]) as arguments",
        ));
    };
    finish(req, func, &mut period, options, recurrences)?;
    store(req, object, period);
    Ok(Value::null())
}

/// The start, interval and end of the objects form: copies of each.
fn from_objects(
    req: &mut Request,
    period: &mut PeriodObj,
    start: PhpObject,
    interval: PhpObject,
    end: Option<PhpObject>,
) -> Result<(), Thrown> {
    let interface = class_id(req, "DateTimeInterface")?;
    let start_time = start.native::<DateObj>(req).and_then(|d| d.time.clone());
    let Some(start_time) = start_time else {
        return Err(uninitialized(req, interface));
    };
    let end_time = match end {
        Some(end) => match end.native::<DateObj>(req).and_then(|d| d.time.clone()) {
            Some(t) => Some(t),
            None => return Err(uninitialized(req, interface)),
        },
        None => None,
    };
    period.start = Some(start_time);
    period.start_ce = Some(start.class());
    period.interval = interval.native::<IntervalObj>(req).and_then(|i| i.diff);
    period.end = end_time;
    Ok(())
}

/// `DatePeriod::getStartDate(): DateTimeInterface`.
///
/// ```php
/// $p = new DatePeriod(new DateTimeImmutable('2000-01-01 Europe/Oslo'), new DateInterval('P1D'), 2);
/// var_dump($p->getStartDate());
/// ```
/// ```expect
/// object(DateTimeImmutable)#3 (3) {
///   ["date"]=>
///   string(26) "2000-01-01 00:00:00.000000"
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(11) "Europe/Oslo"
/// }
/// ```
pub(crate) fn get_start_date(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    let object = this(req)?;
    let state = object
        .native::<PeriodObj>(req)
        .and_then(|p| p.start.clone().zip(p.start_ce));
    let Some((start, class)) = state else {
        return Err(uninitialized(req, object.class()));
    };
    Ok(new_date(req, class, start)?.as_value())
}

/// `DatePeriod::getEndDate(): ?DateTimeInterface`.
///
/// ```php
/// $s = new DateTime('2000-01-01 UTC'); $i = new DateInterval('P1D');
/// var_dump((new DatePeriod($s, $i, 2))->getEndDate(), (new DatePeriod($s, $i, new DateTime('2000-01-05 UTC')))->getEndDate()->format('Y-m-d'));
/// ```
/// ```expect
/// NULL
/// string(10) "2000-01-05"
/// ```
pub(crate) fn get_end_date(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    let object = this(req)?;
    let state = object
        .native::<PeriodObj>(req)
        .and_then(|p| p.end.clone().zip(p.start_ce));
    match state {
        Some((end, class)) => Ok(new_date(req, class, end)?.as_value()),
        None => Ok(Value::null()),
    }
}

/// `DatePeriod::getDateInterval(): DateInterval`.
///
/// ```php
/// var_dump((new DatePeriod(new DateTime('2000-01-01 UTC'), new DateInterval('PT12H'), 2))->getDateInterval());
/// ```
/// ```expect
/// object(DateInterval)#3 (10) {
///   ["y"]=>
///   int(0)
///   ["m"]=>
///   int(0)
///   ["d"]=>
///   int(0)
///   ["h"]=>
///   int(12)
///   ["i"]=>
///   int(0)
///   ["s"]=>
///   int(0)
///   ["f"]=>
///   float(0)
///   ["invert"]=>
///   int(0)
///   ["days"]=>
///   bool(false)
///   ["from_string"]=>
///   bool(false)
/// }
/// ```
pub(crate) fn get_date_interval(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    let object = this(req)?;
    let diff = object.native::<PeriodObj>(req).and_then(|p| p.interval);
    match diff {
        Some(diff) => new_interval(req, diff, 0, None),
        None => Err(uninitialized(req, object.class())),
    }
}

/// `DatePeriod::getRecurrences(): ?int`: the count given, `null` for a
/// period bounded by its end.
///
/// ```php
/// $s = new DateTime('2000-01-01 UTC'); $i = new DateInterval('P1D');
/// var_dump((new DatePeriod($s, $i, 5))->getRecurrences(), (new DatePeriod($s, $i, 5, DatePeriod::EXCLUDE_START_DATE | DatePeriod::INCLUDE_END_DATE))->getRecurrences(), (new DatePeriod($s, $i, new DateTime('2000-02-01 UTC')))->getRecurrences());
/// ```
/// ```expect
/// int(5)
/// int(5)
/// NULL
/// ```
pub(crate) fn get_recurrences(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    let object = this(req)?;
    let n = object.native::<PeriodObj>(req).map_or(0, |p| {
        p.recurrences - i64::from(p.include_start_date) - i64::from(p.include_end_date)
    });
    Ok(if n == 0 { Value::null() } else { Value::int(n) })
}

/// `DatePeriod::__serialize(): array`.
///
/// ```php
/// var_dump((new DatePeriod(new DateTime('2000-01-01 UTC'), new DateInterval('P1D'), 1))->__serialize());
/// ```
/// ```expect
/// array(7) {
///   ["start"]=>
///   object(DateTime)#3 (3) {
///     ["date"]=>
///     string(26) "2000-01-01 00:00:00.000000"
///     ["timezone_type"]=>
///     int(3)
///     ["timezone"]=>
///     string(3) "UTC"
///   }
///   ["current"]=>
///   NULL
///   ["end"]=>
///   NULL
///   ["interval"]=>
///   object(DateInterval)#2 (10) {
///     ["y"]=>
///     int(0)
///     ["m"]=>
///     int(0)
///     ["d"]=>
///     int(1)
///     ["h"]=>
///     int(0)
///     ["i"]=>
///     int(0)
///     ["s"]=>
///     int(0)
///     ["f"]=>
///     float(0)
///     ["invert"]=>
///     int(0)
///     ["days"]=>
///     bool(false)
///     ["from_string"]=>
///     bool(false)
///   }
///   ["recurrences"]=>
///   int(2)
///   ["include_start_date"]=>
///   bool(true)
///   ["include_end_date"]=>
///   bool(false)
/// }
/// ```
pub(crate) fn serialize(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    let object = this(req)?;
    if object
        .native::<PeriodObj>(req)
        .is_none_or(|p| p.start.is_none())
    {
        return Err(uninitialized(req, object.class()));
    }
    let fields = period_fields(req, object)?;
    let own = std_rows(req, object)?;
    Ok(Value::array(rows_array(req, &own, fields, true)?))
}

/// `php_date_period_initialize_from_hash`: every field required, of the
/// type php holds it to.
fn from_hash(req: &mut Request, hash: PhpArray) -> Result<Option<PeriodObj>, Thrown> {
    let find = |req: &mut Request, name: &[u8]| -> Result<Option<Value>, Thrown> {
        let key = Key::Str(PhpString::new(req, name)?);
        Ok(hash.get(&key).map(|v| v.deref()))
    };
    let mut period = PeriodObj::default();
    let date_of =
        |req: &mut Request, v: &Value| -> Result<Result<Option<(Time, ClassId)>, ()>, Thrown> {
            if matches!(v.as_ref(), ValueRef::Null) {
                return Ok(Ok(None));
            }
            match is_a(req, v, "DateTimeInterface")? {
                Some(object) => Ok(object
                    .native::<DateObj>(req)
                    .and_then(|d| d.time.clone())
                    .map(|t| Some((t, object.class())))
                    .ok_or(())),
                None => Ok(Err(())),
            }
        };
    let Some(start) = find(req, b"start")? else {
        return Ok(None);
    };
    match date_of(req, &start)? {
        Ok(Some((t, class))) => {
            period.start = Some(t);
            period.start_ce = Some(class);
        }
        Ok(None) => {}
        Err(()) => return Ok(None),
    }
    for (name, current) in [(&b"end"[..], false), (b"current", true)] {
        let Some(value) = find(req, name)? else {
            return Ok(None);
        };
        match date_of(req, &value)? {
            Ok(Some((t, _))) => {
                if period.start_ce.is_none() {
                    return Ok(None);
                }
                if current {
                    period.current = Some(t);
                } else {
                    period.end = Some(t);
                }
            }
            Ok(None) => {}
            Err(()) => return Ok(None),
        }
    }
    let Some(interval) = find(req, b"interval")? else {
        return Ok(None);
    };
    let interval_class = class_id(req, "DateInterval")?;
    let Some(diff) = interval
        .as_object()
        .filter(|o| o.class() == interval_class)
        .and_then(|o| o.native::<IntervalObj>(req))
        .and_then(|i| i.diff)
    else {
        return Ok(None);
    };
    period.interval = Some(diff);
    let Some(recurrences) = find(req, b"recurrences")?
        .and_then(|v| v.as_int())
        .filter(|n| (0..=i64::from(i32::MAX)).contains(n))
    else {
        return Ok(None);
    };
    period.recurrences = recurrences;
    let Some(include_start) = find(req, b"include_start_date")?.and_then(|v| v.as_bool()) else {
        return Ok(None);
    };
    let Some(include_end) = find(req, b"include_end_date")?.and_then(|v| v.as_bool()) else {
        return Ok(None);
    };
    period.include_start_date = include_start;
    period.include_end_date = include_end;
    period.initialized = true;
    Ok(Some(period))
}

/// `DatePeriod::__set_state(array $array): DatePeriod`.
///
/// ```php
/// $p = DatePeriod::__set_state(['start' => new DateTime('2000-01-01 UTC'), 'current' => null, 'end' => null, 'interval' => new DateInterval('P1D'), 'recurrences' => 3, 'include_start_date' => true, 'include_end_date' => false]);
/// var_dump($p->getRecurrences(), $p->getStartDate()->format('c'));
/// try { DatePeriod::__set_state([]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// string(25) "2000-01-01T00:00:00+00:00"
/// Invalid serialization data for DatePeriod object
/// ```
pub(crate) fn set_state(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let hash = array(req, 1)?;
    let class = class_id(req, "DatePeriod")?;
    let object = new_object(req, class)?;
    match from_hash(req, hash) {
        Ok(Some(period)) => {
            store(req, object, period);
            Ok(object.as_value())
        }
        Ok(None) => {
            req.release(object.as_value());
            Err(throw(
                req,
                "Error",
                "Invalid serialization data for DatePeriod object",
            ))
        }
        Err(thrown) => {
            req.release(object.as_value());
            Err(thrown)
        }
    }
}

/// `DatePeriod::__unserialize(array $data): void`.
///
/// ```php
/// $p = new DatePeriod(new DateTime('2000-01-01 UTC'), new DateInterval('P1D'), 1);
/// $data = $p->__serialize(); $data['recurrences'] = 9; $data['extra'] = true;
/// $p->__unserialize($data);
/// var_dump($p->getRecurrences(), $p->extra);
/// try { $p->__unserialize(['start' => 1]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: Creation of dynamic property DatePeriod::$extra is deprecated in %s on line 4
/// int(8)
/// bool(true)
/// Invalid serialization data for DatePeriod object
/// ```
pub(crate) fn unserialize(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let hash = array(req, 1)?;
    let object = this(req)?;
    let Some(period) = from_hash(req, hash)? else {
        return Err(throw(
            req,
            "Error",
            "Invalid serialization data for DatePeriod object",
        ));
    };
    store(req, object, period);
    restore_properties(req, object, hash, &PERIOD_FIELDS)?;
    Ok(Value::null())
}

/// `DatePeriod::__wakeup(): void`, deprecated since 8.5.
///
/// ```php
/// $p = new DatePeriod(new DateTime('2000-01-01 UTC'), new DateInterval('P1D'), 1);
/// try { @$p->__wakeup(); echo "ok\n"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Invalid serialization data for DatePeriod object
/// ```
pub(crate) fn wakeup(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    let object = this(req)?;
    let own = std_rows(req, object)?;
    let hash = rows_array(req, &own, Vec::new(), false)?;
    let found = from_hash(req, hash);
    req.release(Value::array(hash));
    let Some(period) = found? else {
        return Err(throw(
            req,
            "Error",
            "Invalid serialization data for DatePeriod object",
        ));
    };
    store(req, object, period);
    Ok(Value::null())
}
