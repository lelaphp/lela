//! `DatePeriod`: a start, an interval, and an end or a count of
//! recurrences, read from objects or from an ISO 8601 string, with its
//! getters and its serialised form.

use php_ext::abi::{Instance, IteratorElement, Passed, Raw, Static, This, Untyped};
use php_ext::attr::php_class;
use php_ext::call::{internal_iterator, new_object};
use php_ext::raising::{Thrown, deprecated, throw, throw_error, type_error};
use php_ext::request::Request;
use php_ext::values::{ClassId, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::classes::{DateTimeInterface, Iterator, IteratorAggregate};
use crate::datetime::restore_properties;
use crate::ffi::{self, RelFields, RelTime, Time};
use crate::interval::DateInterval;
use crate::lent::{Lent, lent, release};
use crate::obj::{
    IntervalObj, PERIOD, PERIOD_FIELDS, PeriodObj, PeriodState, class_id, new_date, period_fields,
    period_interval, rows_array, std_rows, uninitialized, with_period, with_time,
};

/// `INT_MAX - 8`, the most recurrences a period may have.
const MAX_RECURRENCES: i64 = 2_147_483_647 - 8;

/// What an ISO 8601 period string gives: its start, end, interval and
/// recurrence count.
struct Iso {
    start: Option<Time>,
    end: Option<Time>,
    interval: Option<RelTime>,
    recurrences: i64,
}

/// `date_period_init_iso8601_string`: the string read, and php's
/// `DateMalformedPeriodStringException` for a part it lacks.
fn from_iso(req: &mut Request, func: &str, text: &[u8]) -> Result<Iso, Thrown> {
    let read = lent(req, |l| {
        let parsed = ffi::strtointerval(l, text);
        if !parsed.errors.errors.is_empty() {
            return Err(None);
        }
        let Some(mut start) = parsed.begin else {
            return Err(Some("a start date"));
        };
        if parsed.period.is_none() {
            return Err(Some("an interval"));
        }
        if parsed.end.is_none() && parsed.recurrences == 0 {
            return Err(Some("an end date or a recurrence count"));
        }
        start.update_ts(l, None);
        let end = parsed.end.map(|mut e| {
            e.update_ts(l, None);
            e
        });
        Ok(Iso {
            start: Some(start),
            end,
            interval: parsed.period,
            recurrences: i64::from(parsed.recurrences),
        })
    })?;
    let shown = String::from_utf8_lossy(text).into_owned();
    match read {
        Ok(iso) => Ok(iso),
        Err(None) => Err(throw(
            req,
            "DateMalformedPeriodStringException",
            &format!("Unknown or bad format ({shown})"),
        )),
        Err(Some(what)) => Err(throw(
            req,
            "DateMalformedPeriodStringException",
            &format!("{func}(): ISO interval must contain {what}, \"{shown}\" given"),
        )),
    }
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
    period.include_start_date = options & DatePeriod::EXCLUDE_START_DATE == 0;
    period.include_end_date = options & DatePeriod::INCLUDE_END_DATE != 0;
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

/// The period's payload set, the one it had freed. The `Err` is the
/// memory limit.
fn store(req: &mut Request, object: PhpObject, period: PeriodObj) -> Result<(), Thrown> {
    let old = match object.native_mut::<PeriodObj>(req) {
        Some(payload) => std::mem::replace(payload, period),
        None => period,
    };
    release(req, old)
}

/// `period` stored where `made` built it whole, and released where
/// `made` threw, as php's partly built period is freed with its object.
fn store_made(
    req: &mut Request,
    object: PhpObject,
    period: PeriodObj,
    made: Result<(), Thrown>,
) -> Result<(), Thrown> {
    match made {
        Ok(()) => store(req, object, period),
        Err(thrown) => {
            release(req, period)?;
            Err(thrown)
        }
    }
}

/// The period's state as it stands, each time copied, to be changed and
/// stored back.
fn copied(req: &mut Request, object: PhpObject) -> Result<PeriodObj, Thrown> {
    with_period(req, object, |req, p| lent(req, |l| Box::new(p.copied(l))))
        .transpose()
        .map(Option::unwrap_or_default)
}

/// Whether `value` is an object of the class named.
fn is_a(req: &mut Request, value: &Value, class: &str) -> Result<Option<PhpObject>, Thrown> {
    let Some(object) = value.deref().as_object() else {
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

/// The start, interval and end of the objects form: copies of each.
fn from_objects(
    req: &mut Request,
    period: &mut PeriodObj,
    start: PhpObject,
    interval: PhpObject,
    end: Option<PhpObject>,
) -> Result<(), Thrown> {
    let interface = class_id(req, "DateTimeInterface")?;
    let Some(start_time) = copy_time(req, start)? else {
        return Err(uninitialized(req, interface));
    };
    period.start = Some(start_time);
    period.start_ce = Some(start.class());
    let diff = interval
        .native::<IntervalObj>(req)
        .and_then(|i| i.diff.as_deref().copied());
    if let Some(diff) = diff {
        period.interval = Some(lent(req, |l| RelTime::of(l, &diff))?);
    }
    if let Some(end) = end {
        let Some(end_time) = copy_time(req, end)? else {
            return Err(uninitialized(req, interface));
        };
        period.end = Some(end_time);
    }
    Ok(())
}

/// `timelib_time_clone(dateobj->time)`, none where no constructor set
/// the time.
fn copy_time(req: &mut Request, object: PhpObject) -> Result<Option<Time>, Thrown> {
    with_time(req, object, |req, t| lent(req, |l| t.copy(l))).transpose()
}

/// `date_period_advance`: the interval added to the time as its relative
/// part, and the time read back from the seconds.
fn advance(lent: &Lent<'_>, time: &mut Time, interval: &RelFields) {
    time.have_relative = 1;
    time.relative = *interval;
    time.sse_uptodate = 0;
    time.update_ts(lent, None);
    time.update_from_sse(lent);
}

/// `date_period_it_has_more`: before the end, or at it to the microsecond
/// under `INCLUDE_END_DATE`; with no end, fewer steps than the
/// recurrences.
fn has_more(period: &PeriodState, index: usize) -> bool {
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
    let found = req
        .symbols()
        .lineage(class)
        .find(|&at| at == mutable || at == immutable || req.symbols().class_parent(at).is_none());
    Ok(found.unwrap_or(class))
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
///
/// A step past the first element builds the period's property table,
/// as `move_forward`'s `zend_std_get_properties_ex` does, so a walk
/// leaves php's usage behind.
///
/// ```php
/// $p = new DatePeriod(new DateTime('2020-01-01'), new DateInterval('P1D'), new DateTime('2020-01-05'));
/// $q = new DatePeriod(new DateTime('2020-01-01'), new DateInterval('P1D'), new DateTime('2020-01-05'));
/// foreach ($q as $x) {} unset($x);
/// $m = memory_get_usage(); foreach ($p as $x) {} unset($x); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 328
/// ```
pub(super) fn iterator_element(
    req: &mut Request,
    object: PhpObject,
    index: usize,
) -> Result<Option<IteratorElement>, Thrown> {
    let start_ce = object.native::<PeriodObj>(req).and_then(|p| p.start_ce);
    let base = match start_ce {
        Some(class) => Some(base_date_class(req, class)?),
        None => None,
    };
    let Some(period) = object.native::<PeriodObj>(req) else {
        return Ok(None);
    };
    if period.start.is_none() {
        let class = class_id(req, "DatePeriod")?;
        return Err(uninitialized(req, class));
    }
    let walked = with_period(req, object, |req, period| {
        lent(req, |l| walk(l, period, index))
    });
    let current = walked.transpose()?.flatten();
    let kept = if index > 0 {
        object.rebuild_properties(req)
    } else {
        Ok(())
    };
    let (Ok(()), Some(base)) = (&kept, base) else {
        release(req, current)?;
        return kept.map(|()| None);
    };
    let Some(current) = current else {
        return Ok(None);
    };
    let date = new_date(req, base, current)?;
    let key = Value::int(i64::try_from(index).unwrap_or(i64::MAX));
    Ok(Some((Owner::take(req, key), date.into_value())))
}

/// `php_date_period_initialize_from_hash`: every field required, of the
/// type php holds it to, written into `period` in php's order as each is
/// read, a `null` date leaving the field as it was, and nothing rolled
/// back where a later one is refused; `false` for the refusal.
/// The period's `current` brought to `index` steps past its rewind, as
/// [`iterator_element`] says, and a copy of it where `has_more` answers
/// there is one.
fn walk(l: &Lent<'_>, period: &mut PeriodState, index: usize) -> Option<Time> {
    let interval = period.interval.as_deref().copied().unwrap_or_default();
    let steps = match period.cursor {
        Some(at) if at <= index && period.current.is_some() => index - at,
        _ => {
            let mut current = period.start.as_ref()?.copy(l);
            if !period.include_start_date {
                advance(l, &mut current, &interval);
            }
            period.current = Some(current);
            index
        }
    };
    if let Some(current) = period.current.as_mut() {
        for _ in 0..steps {
            advance(l, current, &interval);
        }
    }
    period.cursor = Some(index);
    if !has_more(period, index) {
        return None;
    }
    period.current.as_ref().map(|current| current.copy(l))
}

fn from_hash(req: &mut Request, hash: PhpArray, period: &mut PeriodObj) -> Result<bool, Thrown> {
    let find = |name: &[u8]| hash.find(name).map(|v| v.deref());
    // A date field: the object of a date class, `Ok(None)` for `null`,
    // `Err(())` for a refusal.
    let date_of = |req: &mut Request, v: &Value| -> Result<Result<Option<PhpObject>, ()>, Thrown> {
        if matches!(v.as_ref(), ValueRef::Null) {
            return Ok(Ok(None));
        }
        Ok(is_a(req, v, "DateTimeInterface")?.map(Some).ok_or(()))
    };
    let Some(start) = find(b"start") else {
        return Ok(false);
    };
    match date_of(req, &start)? {
        Ok(Some(object)) => {
            let Some(t) = copy_time(req, object)? else {
                return Ok(false);
            };
            let old = period.start.replace(t);
            release(req, old)?;
            period.start_ce = Some(object.class());
        }
        Ok(None) => {}
        Err(()) => return Ok(false),
    }
    for (name, current) in [(&b"end"[..], false), (b"current", true)] {
        let Some(value) = find(name) else {
            return Ok(false);
        };
        match date_of(req, &value)? {
            Ok(Some(object)) => {
                let initialized = object
                    .native::<crate::obj::DateObj>(req)
                    .is_some_and(|d| d.time.is_some());
                if !initialized || period.start_ce.is_none() {
                    return Ok(false);
                }
                let Some(t) = copy_time(req, object)? else {
                    return Ok(false);
                };
                let slot = if current {
                    &mut period.current
                } else {
                    &mut period.end
                };
                let old = slot.replace(t);
                release(req, old)?;
            }
            Ok(None) => {}
            Err(()) => return Ok(false),
        }
    }
    let Some(interval) = find(b"interval") else {
        return Ok(false);
    };
    let interval_class = class_id(req, "DateInterval")?;
    let Some(diff) = interval
        .as_object()
        .filter(|o| o.class() == interval_class)
        .and_then(|o| o.native::<IntervalObj>(req))
        .and_then(|i| i.diff.as_deref().copied())
    else {
        return Ok(false);
    };
    // The interval the period had freed before the clone is made.
    release(req, period.interval.take())?;
    period.interval = Some(lent(req, |l| RelTime::of(l, &diff))?);
    let Some(recurrences) = find(b"recurrences")
        .and_then(|v| v.as_int())
        .filter(|n| (0..=i64::from(i32::MAX)).contains(n))
    else {
        return Ok(false);
    };
    period.recurrences = recurrences;
    let Some(include_start) = find(b"include_start_date").and_then(|v| v.as_bool()) else {
        return Ok(false);
    };
    period.include_start_date = include_start;
    let Some(include_end) = find(b"include_end_date").and_then(|v| v.as_bool()) else {
        return Ok(false);
    };
    period.include_end_date = include_end;
    period.initialized = true;
    Ok(true)
}

/// php's refusal of a hash that is no period.
fn invalid(req: &mut Request) -> Thrown {
    throw_error(
        req,
        "Error",
        "Invalid serialization data for DatePeriod object",
    )
}

/// `class DatePeriod implements IteratorAggregate`, over [`PeriodObj`]:
/// its seven properties are `@virtual`, answered by its table in place
/// of a slot.
#[php_class(implements(IteratorAggregate), size = 104, payload = PeriodObj, handlers = PERIOD)]
pub(crate) struct DatePeriod;

#[php_class]
impl DatePeriod {
    const EXCLUDE_START_DATE: i64 = 0x0001;
    const INCLUDE_END_DATE: i64 = 0x0002;

    #[php(property, virtual)]
    const start: Option<Instance<DateTimeInterface>>;
    #[php(property, virtual)]
    const current: Option<Instance<DateTimeInterface>>;
    #[php(property, virtual)]
    const end: Option<Instance<DateTimeInterface>>;
    #[php(property, virtual)]
    const interval: Option<Instance<DateInterval>>;
    #[php(property, virtual)]
    const recurrences: i64;
    #[php(property, virtual)]
    const include_start_date: bool;
    #[php(property, virtual)]
    const include_end_date: bool;

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
    fn createFromISO8601String(
        req: &mut Request,
        specification: PhpString,
        #[php(default = 0)] options: i64,
    ) -> Result<Owner<Static>, Thrown> {
        let class = match req.frames().called_scope() {
            Some(class) => class,
            None => class_id(req, "DatePeriod")?,
        };
        let object = new_object(req, class)?;
        let view = *object;
        let built = (|| {
            let func = "DatePeriod::createFromISO8601String";
            let iso = from_iso(req, func, specification.as_bytes())?;
            let mut period = PeriodObj::default();
            period.start = iso.start;
            period.end = iso.end;
            period.interval = iso.interval;
            let made = class_id(req, "DateTimeImmutable").and_then(|base| {
                period.start_ce = Some(base);
                finish(req, func, &mut period, options, iso.recurrences)
            });
            store_made(req, view, period, made)
        })();
        match built {
            Ok(()) => Ok(object.into_static()),
            Err(thrown) => {
                object.give_up(req);
                Err(thrown)
            }
        }
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
    ///
    /// A period holds copies of its start and end, each a `timelib_time`
    /// with its abbreviation, and of its interval, a `timelib_rel_time`,
    /// on the request's heap, as php's timelib clones them; a walk adds
    /// its `current`.
    ///
    /// ```php
    /// $s = new DateTime('2020-01-01'); $i = new DateInterval('P1D'); $e = new DateTime('2020-01-03');
    /// $p = new DatePeriod($s, $i, 3); unset($p);
    /// $m = memory_get_usage(); $p = new DatePeriod($s, $i, 3); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $q = new DatePeriod($s, $i, $e); $c = clone $q; echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); unset($p, $q, $c); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 480
    /// 1504
    /// -1984
    /// ```
    ///
    /// The start, end and interval timelib copies, and what the start's
    /// and the interval's own constructions allocate, are the request's,
    /// so the peak is php's (ADR-0014):
    ///
    /// ```php
    /// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
    /// pk(fn () => new DatePeriod(new DateTime('2020-01-01'), new DateInterval('P1D'), 3));
    /// ```
    /// ```expect
    /// 1184 512
    /// ```
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        start: Raw<Untyped<PhpString>>,
        interval: Passed<Raw<Untyped<i64>>>,
        end: Passed<Raw<Untyped<i64>>>,
        options: Passed<Raw<Untyped<i64>>>,
    ) -> Result<(), Thrown> {
        let func = "DatePeriod::__construct";
        let count = req.frames().argument_count();
        let value = |given: Passed<Raw<Untyped<i64>>>| match given {
            Passed::Given(raw) => raw.value(),
            Passed::Omitted => Value::null(),
        };
        // `l` after the `|`: the argument held as an int, `0` where the
        // call gave none.
        let long = |req: &mut Request, given: Passed<Raw<Untyped<i64>>>| match given {
            Passed::Given(raw) => raw.hold(req).map(|held| held.0),
            Passed::Omitted => Ok(0),
        };
        let (a, b, c, d) = (start.value(), value(interval), value(end), value(options));
        let start_object = is_a(req, &a, "DateTimeInterface")?;
        let interval_object = is_a(req, &b, "DateInterval")?;
        let end_object = is_a(req, &c, "DateTimeInterface")?;
        let options_ok = count < 4 || is_long(&d);
        let object = this.object();
        let mut period = PeriodObj::default();
        let made = (|| {
            let (options, recurrences) = if let (Some(start), Some(interval), true, true, true) = (
                start_object,
                interval_object,
                (3..=4).contains(&count),
                options_ok,
                end_object.is_none() && is_long(&c),
            ) {
                let recurrences = long(req, end)?;
                let options = long(req, options)?;
                from_objects(req, &mut period, start, interval, None)?;
                (options, recurrences)
            } else if let (Some(start), Some(interval), Some(end), true, true) = (
                start_object,
                interval_object,
                end_object,
                (3..=4).contains(&count),
                options_ok,
            ) {
                let options = long(req, options)?;
                from_objects(req, &mut period, start, interval, Some(end))?;
                (options, 0)
            } else if (1..=2).contains(&count)
                && (count < 2 || is_long(&b))
                && !matches!(a.deref().as_ref(), ValueRef::Object(_) | ValueRef::Array(_))
            {
                let text = start.hold(req)?.0;
                let options = long(req, interval)?;
                deprecated(
                    req,
                    "Calling DatePeriod::__construct(string $isostr, int $options = 0) is deprecated, use DatePeriod::createFromISO8601String() instead",
                );
                let iso = from_iso(req, func, text.as_bytes())?;
                period.start = iso.start;
                period.end = iso.end;
                period.interval = iso.interval;
                period.start_ce = Some(class_id(req, "DateTime")?);
                (options, iso.recurrences)
            } else {
                return Err(type_error(
                    req,
                    "DatePeriod::__construct() accepts (DateTimeInterface, DateInterval, int [, int]), or (DateTimeInterface, DateInterval, DateTime [, int]), or (string [, int]) as arguments",
                ));
            };
            finish(req, func, &mut period, options, recurrences)
        })();
        store_made(req, object, period, made)
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
    #[php(tentative)]
    fn getStartDate(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<DateTimeInterface>>, Thrown> {
        let object = this.object();
        let state = with_period(req, object, |req, p| {
            let class = p.start_ce?;
            let start = p.start.as_ref()?;
            Some(lent(req, |l| (start.copy(l), class)))
        })
        .flatten()
        .transpose()?;
        let Some((start, class)) = state else {
            return Err(uninitialized(req, object.class()));
        };
        new_date(req, class, start).map(Owner::into_instance)
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
    #[php(tentative)]
    fn getEndDate(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<DateTimeInterface>>>, Thrown> {
        let state = with_period(req, this.object(), |req, p| {
            let class = p.start_ce?;
            let end = p.end.as_ref()?;
            Some(lent(req, |l| (end.copy(l), class)))
        })
        .flatten()
        .transpose()?;
        match state {
            Some((end, class)) => new_date(req, class, end).map(|date| Some(date.into_instance())),
            None => Ok(None),
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
    #[php(tentative)]
    fn getDateInterval(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<DateInterval>>, Thrown> {
        let object = this.object();
        let diff = object
            .native::<PeriodObj>(req)
            .and_then(|p| p.interval.as_deref().copied());
        match diff {
            Some(diff) => period_interval(req, &diff),
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
    #[php(tentative)]
    fn getRecurrences(req: &mut Request, this: This<Self>) -> Option<i64> {
        let n = this.object().native::<PeriodObj>(req).map_or(0, |p| {
            p.recurrences - i64::from(p.include_start_date) - i64::from(p.include_end_date)
        });
        (n != 0).then_some(n)
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
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let object = this.object();
        if object
            .native::<PeriodObj>(req)
            .is_none_or(|p| p.start.is_none())
        {
            return Err(uninitialized(req, object.class()));
        }
        let own = std_rows(req, object)?;
        let fields = period_fields(req, object)?;
        rows_array(req, &own, fields, true)
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
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        let object = this.object();
        let mut period = copied(req, object)?;
        let taken = from_hash(req, data, &mut period);
        store(req, object, period)?;
        if !taken? {
            return Err(invalid(req));
        }
        restore_properties(req, object, data, &PERIOD_FIELDS)
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
    #[php(
        tentative,
        deprecated(
            since = "8.5",
            message = "this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize()"
        )
    )]
    fn __wakeup(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let object = this.object();
        let own = std_rows(req, object)?;
        let hash = rows_array(req, &own, Vec::new(), false)?;
        let mut period = match copied(req, object) {
            Ok(period) => period,
            Err(thrown) => {
                hash.give_up(req);
                return Err(thrown);
            }
        };
        let taken = from_hash(req, *hash, &mut period);
        hash.give_up(req);
        store(req, object, period)?;
        if !taken? {
            return Err(invalid(req));
        }
        Ok(())
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
    #[php(tentative)]
    fn __set_state(
        req: &mut Request,
        array: PhpArray,
    ) -> Result<Owner<Instance<DatePeriod>>, Thrown> {
        let class = class_id(req, "DatePeriod")?;
        let object = new_object(req, class)?;
        let mut period = PeriodObj::default();
        let made = match from_hash(req, array, &mut period) {
            Ok(true) => Ok(()),
            Ok(false) => Err(None),
            Err(thrown) => Err(Some(thrown)),
        };
        if made.is_ok() {
            store(req, *object, period)?;
            return Ok(object.into_instance());
        }
        release(req, period)?;
        object.give_up(req);
        match made {
            Err(Some(thrown)) => Err(thrown),
            _ => Err(invalid(req)),
        }
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
    fn getIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<Iterator>>, Thrown> {
        internal_iterator(req, &this.object()).map(Owner::into_instance)
    }
}
