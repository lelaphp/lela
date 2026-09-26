//! `DatePeriod`: a start, an interval, and an end or a count of
//! recurrences, read from objects or from an ISO 8601 string, with its
//! getters and its serialised form.

use php_ext::call::{internal_iterator, new_object};
use php_ext::raising::{Thrown, deprecated, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::args::{array, count_between, long, optional_long, string};
use crate::datetime::restore_properties;
use crate::ffi::{self, RelTime, Time};
use crate::obj::{
    DateObj, IntervalObj, PERIOD_FIELDS, PeriodObj, class_id, new_date, new_interval, parent_of,
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
        payload.replace(period);
    }
}

/// The period's state as it stands, to be changed and stored back.
fn copied(req: &Request, object: PhpObject) -> PeriodObj {
    object
        .native::<PeriodObj>(req)
        .map(PeriodObj::copied)
        .unwrap_or_default()
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

/// `DatePeriod::getIterator(): Iterator`, php's
/// `zend_create_internal_iterator_zval` over the period: an
/// `InternalIterator` walking it by `date_period_it`'s rules
/// ([`iterator_element`]), which `foreach` reaches through the same
/// method, so a subclass that overrides it is walked by its own.
///
/// ```php
/// $p = DatePeriod::createFromISO8601String('R2/2012-07-01T00:00:00Z/P7D');
/// $it = $p->getIterator();
/// var_dump(get_class($it));
/// for (; $it->valid(); $it->next()) echo $it->key(), ': ', $it->current()->format('Y-m-d'), "\n";
/// $it->rewind(); echo $it->current()->format('Y-m-d'), "\n";
/// class P extends DatePeriod { function getIterator(): Iterator { return new ArrayIterator([7]); } }
/// foreach (new P(new DateTime('2000-01-01'), new DateInterval('P1D'), 1) as $k => $v) echo "$k=$v\n";
/// try { foreach ($p as &$d) {} } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(16) "InternalIterator"
/// 0: 2012-07-01
/// 1: 2012-07-08
/// 2: 2012-07-15
/// 2012-07-01
/// 0=7
/// An iterator cannot be used with foreach by reference
/// ```
pub(crate) fn get_iterator(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    let object = this(req)?;
    Ok(internal_iterator(req, &object)?.as_value())
}

/// `date_period_advance`: the interval added to the time as its relative
/// part, and the time read back from the seconds.
fn advance(time: &mut Time, interval: &RelTime) {
    time.have_relative = 1;
    time.relative = *interval;
    time.sse_uptodate = 0;
    time.update_ts(None);
    time.update_from_sse();
}

/// `date_period_it_has_more`: before the end, or at it to the microsecond
/// under `INCLUDE_END_DATE`; with no end, fewer steps than the
/// recurrences.
fn has_more(period: &PeriodObj, index: usize) -> bool {
    let Some(current) = &period.current else {
        return false;
    };
    match &period.end {
        Some(end) if current.sse == end.sse => {
            if period.include_end_date {
                current.us <= end.us
            } else {
                current.us < end.us
            }
        }
        Some(end) => current.sse < end.sse,
        None => i64::try_from(index).is_ok_and(|index| index < period.recurrences),
    }
}

/// `get_base_date_class(start_ce)`: the class itself or the first
/// ancestor that is `DateTime` or `DateTimeImmutable`, or the root.
fn base_date_class(req: &mut Request, class: ClassId) -> Result<ClassId, Thrown> {
    let mutable = class_id(req, "DateTime")?;
    let immutable = class_id(req, "DateTimeImmutable")?;
    let mut at = class;
    while at != mutable && at != immutable {
        match parent_of(req, at) {
            Some(parent) => at = parent,
            None => break,
        }
    }
    Ok(at)
}

/// The element a walk of the period stands at `index` steps past its
/// rewind, `date_period_it`'s functions over the period's `current`:
/// `rewind` sets it to a copy of the start, moved once where the start
/// is excluded; each step is `move_forward`'s `date_period_advance`;
/// past the last `has_more` answers none; `current_data` is a new date
/// of the start's base class over it, and `current_key` the index. The
/// period keeps where `current` stands, so asking for the same index
/// again moves nothing and the next one moves once, and an index behind
/// it rewinds. A period whose constructor never ran is php's
/// `DateObjectError`, raised where `rewind` raises it.
///
/// ```php
/// $s = new DateTimeImmutable('2022-01-01 00:00:00.5 UTC'); $i = new DateInterval('P1D');
/// foreach ([0, DatePeriod::EXCLUDE_START_DATE, DatePeriod::INCLUDE_END_DATE, 3] as $o) {
///     foreach (new DatePeriod($s, $i, new DateTime('2022-01-03 00:00:00.5 UTC'), $o) as $k => $d) echo $k, '=', $d->format('md'), ',';
///     foreach (new DatePeriod($s, $i, 2, $o) as $k => $d) echo $k, ':', $d->format('md'), ',';
///     echo "\n";
/// }
/// $p = new DatePeriod($s, $i, 1);
/// foreach ($p as $d) {}
/// var_dump(get_class($d), $p->current->format('Y-m-d'));
/// class NoCtor extends DatePeriod { function __construct() {} }
/// try { foreach (new NoCtor as $d) {} } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 0=0101,1=0102,0:0101,1:0102,2:0103,
/// 0=0102,0:0102,1:0103,
/// 0=0101,1=0102,2=0103,0:0101,1:0102,2:0103,3:0104,
/// 0=0102,1=0103,0:0102,1:0103,2:0104,
/// string(17) "DateTimeImmutable"
/// string(10) "2022-01-03"
/// DateObjectError: Object of type DatePeriod has not been correctly initialized by calling parent::__construct() in its constructor
/// ```
pub(super) fn iterator_element(
    req: &mut Request,
    object: PhpObject,
    index: usize,
) -> Result<Option<(Value, Value)>, Thrown> {
    let start_ce = object.native::<PeriodObj>(req).and_then(|p| p.start_ce);
    let base = match start_ce {
        Some(class) => Some(base_date_class(req, class)?),
        None => None,
    };
    let Some(period) = object.native_mut::<PeriodObj>(req) else {
        return Ok(None);
    };
    let Some(start) = period.start.clone() else {
        let class = class_id(req, "DatePeriod")?;
        return Err(uninitialized(req, class));
    };
    let interval = period.interval.unwrap_or_default();
    let steps = match period.cursor {
        Some(at) if at <= index && period.current.is_some() => index - at,
        _ => {
            let mut current = start;
            if !period.include_start_date {
                advance(&mut current, &interval);
            }
            period.current = Some(current);
            index
        }
    };
    if let Some(current) = period.current.as_mut() {
        for _ in 0..steps {
            advance(current, &interval);
        }
    }
    period.cursor = Some(index);
    if !has_more(period, index) {
        return Ok(None);
    }
    let (Some(current), Some(base)) = (period.current.clone(), base) else {
        return Ok(None);
    };
    let date = new_date(req, base, current)?;
    let key = Value::int(i64::try_from(index).unwrap_or(i64::MAX));
    Ok(Some((key, date.as_value())))
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
/// type php holds it to, written into `period` in php's order as each is
/// read, a `null` date leaving the field as it was, and nothing rolled
/// back where a later one is refused; `false` for the refusal.
fn from_hash(req: &mut Request, hash: PhpArray, period: &mut PeriodObj) -> Result<bool, Thrown> {
    let find = |req: &mut Request, name: &[u8]| -> Result<Option<Value>, Thrown> {
        let key = Key::Str(PhpString::new(req, name)?);
        Ok(hash.get(&key).map(|v| v.deref()))
    };
    // A date field: `Ok(None)` for `null`, `Err(())` for a refusal.
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
        return Ok(false);
    };
    match date_of(req, &start)? {
        Ok(Some((t, class))) => {
            period.start = Some(t);
            period.start_ce = Some(class);
        }
        Ok(None) => {}
        Err(()) => return Ok(false),
    }
    for (name, current) in [(&b"end"[..], false), (b"current", true)] {
        let Some(value) = find(req, name)? else {
            return Ok(false);
        };
        match date_of(req, &value)? {
            Ok(Some(_)) if period.start_ce.is_none() => return Ok(false),
            Ok(Some((t, _))) if current => period.current = Some(t),
            Ok(Some((t, _))) => period.end = Some(t),
            Ok(None) => {}
            Err(()) => return Ok(false),
        }
    }
    let Some(interval) = find(req, b"interval")? else {
        return Ok(false);
    };
    let interval_class = class_id(req, "DateInterval")?;
    let Some(diff) = interval
        .as_object()
        .filter(|o| o.class() == interval_class)
        .and_then(|o| o.native::<IntervalObj>(req))
        .and_then(|i| i.diff)
    else {
        return Ok(false);
    };
    period.interval = Some(diff);
    let Some(recurrences) = find(req, b"recurrences")?
        .and_then(|v| v.as_int())
        .filter(|n| (0..=i64::from(i32::MAX)).contains(n))
    else {
        return Ok(false);
    };
    period.recurrences = recurrences;
    let Some(include_start) = find(req, b"include_start_date")?.and_then(|v| v.as_bool()) else {
        return Ok(false);
    };
    period.include_start_date = include_start;
    let Some(include_end) = find(req, b"include_end_date")?.and_then(|v| v.as_bool()) else {
        return Ok(false);
    };
    period.include_end_date = include_end;
    period.initialized = true;
    Ok(true)
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
    let mut period = PeriodObj::default();
    match from_hash(req, hash, &mut period) {
        Ok(true) => {
            store(req, object, period);
            Ok(object.as_value())
        }
        Ok(false) => {
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
    let mut period = copied(req, object);
    let taken = from_hash(req, hash, &mut period);
    store(req, object, period);
    if !taken? {
        return Err(throw(
            req,
            "Error",
            "Invalid serialization data for DatePeriod object",
        ));
    }
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
    deprecated(
        req,
        "Method DatePeriod::__wakeup() is deprecated since 8.5, this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize()",
    );
    count_between(req, 0, 0)?;
    let object = this(req)?;
    let own = std_rows(req, object)?;
    let hash = rows_array(req, &own, Vec::new(), false)?;
    let mut period = copied(req, object);
    let taken = from_hash(req, hash, &mut period);
    req.release(Value::array(hash));
    store(req, object, period);
    if !taken? {
        return Err(throw(
            req,
            "Error",
            "Invalid serialization data for DatePeriod object",
        ));
    }
    Ok(Value::null())
}
