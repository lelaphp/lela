//! `DateTime`, `DateTimeImmutable` and the procedural natives over
//! them: `php_date_initialize` and every body `php_date.c` gives both a
//! function and a method. The two classes share their payload,
//! [`DateObj`], and the table `date_object_handlers_date`; where a body
//! is the same on both, the immutable class names the mutable one's
//! entry. `DateTime`'s methods change the object and answer it; the
//! immutable class's clone it first (`date_clone_immutable`) and answer
//! the clone.

use std::time::{SystemTime, UNIX_EPOCH};

use php_ext::abi::{Class, Instance, ObjectHandlers as _, OrFalse, Path, Static, This, Union};
use php_ext::attr::{php_class, php_function};
use php_ext::call::new_object;
use php_ext::raising::{Level, Thrown, docref, throw, throw_error};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::State;
use crate::classes::DateTimeInterface;
use crate::ffi::{
    self, NO_CLONE, OVERRIDE_TIME, RelTime, Time, UNSET, ZONETYPE_ABBR, ZONETYPE_ID,
    ZONETYPE_OFFSET,
};
use crate::format::date_format as format_time;
use crate::interval::DateInterval;
use crate::obj::{
    CIVIL, DATE, DateObj, TzKind, WALL, again, class_id, clone_date, date_fields, interval_of,
    new_date, new_interval, new_timezone, rows_array, set_time, std_rows, time_of, tz_from_time,
    tz_of,
};
use crate::parse::keep_errors;
use crate::timezone::DateTimeZone;
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
                return Err(throw_error(
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
fn create<C: Class>(
    req: &mut Request,
    class: ClassId,
    text: &[u8],
    format: Option<&[u8]>,
    timezone: Option<Instance<DateTimeZone>>,
    how: Init,
) -> Result<OrFalse<Instance<C>>, Thrown> {
    let object = new_object(req, class)?;
    let timezone = timezone.map(|tz| tz.object());
    match initialize(req, object, text, format, timezone, how) {
        Ok(true) => Ok(OrFalse::Value(Instance::of(object))),
        Ok(false) => {
            req.release(object.as_value());
            Ok(OrFalse::False)
        }
        Err(thrown) => {
            req.release(object.as_value());
            Err(thrown)
        }
    }
}

/// `date_create` and `date_create_immutable`: an object of the class
/// `name` names, or `false`.
fn date_create_of<C: Class>(
    req: &mut Request,
    name: &str,
    datetime: PhpString,
    timezone: Option<Instance<DateTimeZone>>,
) -> Result<OrFalse<Instance<C>>, Thrown> {
    let class = class_id(req, name)?;
    let how = Init {
        ctor: false,
        format: false,
    };
    create(req, class, datetime.as_bytes(), None, timezone, how)
}

/// `date_create_from_format` and both `createFromFormat`s: an object of
/// `class`, or `false`.
fn from_format<C: Class>(
    req: &mut Request,
    class: ClassId,
    format: PhpString,
    datetime: Path,
    timezone: Option<Instance<DateTimeZone>>,
) -> Result<OrFalse<Instance<C>>, Thrown> {
    let how = Init {
        ctor: false,
        format: true,
    };
    let text = datetime.0.as_bytes();
    create(req, class, text, Some(format.as_bytes()), timezone, how)
}

/// The class a factory called on a class makes: the called one, else
/// `fallback`.
fn called_or(req: &mut Request, fallback: &str) -> Result<ClassId, Thrown> {
    match req.called_scope() {
        Some(class) => Ok(class),
        None => class_id(req, fallback),
    }
}

/// `createFromImmutable`, `createFromMutable` and both
/// `createFromInterface`s: a date of the called class over a copy of
/// the given one's time.
fn create_from(req: &mut Request, from: PhpObject, fallback: &str) -> Result<PhpObject, Thrown> {
    let time = time_of(req, from)?;
    let class = called_or(req, fallback)?;
    new_date(req, class, time)
}

/// `createFromTimestamp(int|float $timestamp): static` of either class.
fn create_from_timestamp(
    req: &mut Request,
    timestamp: Union<i64, f64>,
    method: &str,
    fallback: &str,
) -> Result<Static, Thrown> {
    let time = match timestamp {
        Union::A(n) => from_timestamp(n, 0),
        Union::B(f) => from_float(req, method, f)?,
    };
    let class = called_or(req, fallback)?;
    new_date(req, class, time).map(Static)
}

/// `date_get_last_errors` and `getLastErrors` of both classes.
fn last_errors(req: &mut Request) -> Result<OrFalse<PhpArray>, Thrown> {
    let Some(errors) = req.state::<State>().last_errors.clone() else {
        return Ok(OrFalse::False);
    };
    crate::parse::errors_value(req, &errors).map(OrFalse::Value)
}

/// `date_format` and `format` of both classes.
fn format_of(req: &mut Request, object: PhpObject, format: PhpString) -> Result<PhpString, Thrown> {
    let text = {
        let Some(t) = object.native::<DateObj>(req).and_then(|d| d.time.as_ref()) else {
            return Err(crate::obj::uninitialized(req, object.class()));
        };
        format_time(format.as_bytes(), t, t.is_localtime != 0)
    };
    PhpString::new(req, &text)
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

/// `date_timezone_get` and `getTimezone` of both classes: `false` for a
/// time with no zone.
fn timezone_of(
    req: &mut Request,
    object: PhpObject,
) -> Result<OrFalse<Instance<DateTimeZone>>, Thrown> {
    let time = time_of(req, object)?;
    if time.is_localtime == 0 {
        return Ok(OrFalse::False);
    }
    match tz_from_time(&time) {
        Some(kind) => new_timezone(req, kind).map(OrFalse::Value),
        None => Ok(OrFalse::False),
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

/// `date_offset_get` and `getOffset` of both classes.
fn offset_of(req: &mut Request, object: PhpObject) -> Result<i64, Thrown> {
    let time = time_of(req, object)?;
    if time.is_localtime == 0 {
        return Ok(0);
    }
    Ok(match time.zone_type {
        ZONETYPE_ID => time
            .zone()
            .map_or(0, |zone| i64::from(zone.offset_at(time.sse).offset)),
        ZONETYPE_OFFSET => i64::from(time.z),
        ZONETYPE_ABBR => i64::from(time.z + 3600 * time.dst),
        _ => 0,
    })
}

/// `php_date_time_set`.
fn time_set(
    req: &mut Request,
    object: PhpObject,
    hour: i64,
    minute: i64,
    second: i64,
    microsecond: i64,
) -> Result<(), Thrown> {
    let mut time = time_of(req, object)?;
    time.h = hour;
    time.i = minute;
    time.s = second;
    time.us = microsecond;
    time.update_ts(None);
    time.update_from_sse();
    set_time(req, object, Some(time));
    Ok(())
}

/// `php_date_date_set`.
fn date_set(
    req: &mut Request,
    object: PhpObject,
    year: i64,
    month: i64,
    day: i64,
) -> Result<(), Thrown> {
    let mut time = time_of(req, object)?;
    time.y = year;
    time.m = month;
    time.d = day;
    time.update_ts(None);
    set_time(req, object, Some(time));
    Ok(())
}

/// `php_date_isodate_set`.
fn isodate_set(
    req: &mut Request,
    object: PhpObject,
    year: i64,
    week: i64,
    day_of_week: i64,
) -> Result<(), Thrown> {
    let mut time = time_of(req, object)?;
    time.y = year;
    time.m = 1;
    time.d = 1;
    time.relative = RelTime::default();
    time.relative.d = ffi::daynr_from_weeknr(year, week, day_of_week);
    time.have_relative = 1;
    time.update_ts(None);
    set_time(req, object, Some(time));
    Ok(())
}

/// `php_date_timestamp_set`.
fn timestamp_set(req: &mut Request, object: PhpObject, timestamp: i64) -> Result<(), Thrown> {
    let mut time = time_of(req, object)?;
    time.unixtime2local(timestamp);
    time.update_ts(None);
    time.us = 0;
    set_time(req, object, Some(time));
    Ok(())
}

/// `setMicrosecond`'s range, php's `DateRangeError` outside it.
fn microsecond_in_range(req: &mut Request, method: &str, us: i64) -> Result<(), Thrown> {
    if (0..=999_999).contains(&us) {
        return Ok(());
    }
    Err(throw(
        req,
        "DateRangeError",
        &format!("{method}(): Argument #1 ($microsecond) must be between 0 and 999999, {us} given"),
    ))
}

/// `date_clone_immutable` and then the change on the clone: the clone,
/// or its release where the change threw.
fn changed_copy(
    req: &mut Request,
    object: PhpObject,
    change: impl FnOnce(&mut Request, PhpObject) -> Result<(), Thrown>,
) -> Result<PhpObject, Thrown> {
    let copy = clone_date(req, object)?;
    match change(req, copy) {
        Ok(()) => Ok(copy),
        Err(thrown) => {
            req.release(copy.as_value());
            Err(thrown)
        }
    }
}

/// `date_timestamp_get` and `getTimestamp` of both classes: the
/// `DateRangeError` where the epoch does not fit.
fn timestamp_of(req: &mut Request, object: PhpObject) -> Result<i64, Thrown> {
    let mut time = time_of(req, object)?;
    if time.sse_uptodate == 0 {
        time.update_ts(None);
    }
    match time.date_to_int() {
        Some(ts) => Ok(ts),
        None => Err(throw_error(
            req,
            "DateRangeError",
            "Epoch doesn't fit in a PHP integer",
        )),
    }
}

/// `date_diff` and `diff` of both classes.
fn diff_of(
    req: &mut Request,
    one: PhpObject,
    two: PhpObject,
    absolute: bool,
) -> Result<Instance<DateInterval>, Thrown> {
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
    let find = |name: &[u8]| hash.find(name).map(|v| v.deref());
    let Some(date) = find(b"date").and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec)) else {
        return Ok(false);
    };
    let Some(zone_type) = find(b"timezone_type").and_then(|v| v.as_int()) else {
        return Ok(false);
    };
    let Some(zone) = find(b"timezone").and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec)) else {
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
            let tz = new_timezone(req, TzKind::Id(tzi))?.object();
            let done = initialize(req, object, &date, None, Some(tz), init);
            req.release(tz.as_value());
            done
        }
        _ => Ok(false),
    }
}

/// `__set_state(array $array)` of either class: always of the class
/// itself, as `php_date_instantiate(date_ce_date)` makes it.
fn set_state(req: &mut Request, class: &str, hash: PhpArray) -> Result<PhpObject, Thrown> {
    let class_id = class_id(req, class)?;
    let object = new_object(req, class_id)?;
    match initialize_from_hash(req, object, hash) {
        Ok(true) => Ok(object),
        Ok(false) => {
            req.release(object.as_value());
            Err(throw_error(
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

/// `restore_custom_*_properties`: every row of the hash but the class's
/// own fields and a reference written back as a property, php's
/// `update_property` over each name: a private one only where the class
/// its mangled name names exists, which `zend_lookup_class` finds.
///
/// ```php
/// class I extends DateTimeImmutable { private $p = 0; }
/// $u = unserialize(str_replace('!', "\0", 'O:1:"I":4:{s:4:"date";s:26:"2023-03-09 17:03:11.000000";s:13:"timezone_type";i:3;s:8:"timezone";s:3:"UTC";s:4:"!K!p";i:2;}'));
/// var_dump((fn() => $this->p)->call($u));
/// var_dump(count(get_object_vars(unserialize('O:12:"DateInterval":2:{s:1:"a";s:1:"x";s:1:"b";R:2;}'))));
/// ```
/// ```expect
/// int(0)
/// int(10)
/// ```
pub(super) fn restore_properties(
    req: &mut Request,
    object: PhpObject,
    hash: PhpArray,
    internal: &[&[u8]],
) -> Result<(), Thrown> {
    for (key, value) in hash.snapshot_refs() {
        let Key::Str(name) = key else {
            continue;
        };
        if matches!(value.as_ref(), ValueRef::Reference(_)) || internal.contains(&name.as_bytes()) {
            continue;
        }
        let bytes = name.as_bytes();
        let plain: &[u8] = match bytes.strip_prefix(b"\0") {
            Some(rest) => {
                let Some(at) = rest.iter().position(|b| *b == 0) else {
                    continue;
                };
                let owner = &rest[..at];
                if owner != b"*" && php_ext::call::lookup_class(req, owner)?.is_none() {
                    continue;
                }
                &rest[at + 1..]
            }
            None => bytes,
        };
        let plain = php_ext::values::PhpString::new(req, plain)?;
        req.addref(&value);
        php_ext::abi::Std.write_property(req, object, plain, value)?;
    }
    Ok(())
}

/// `__unserialize(array $data): void` of either class.
fn unserialize(
    req: &mut Request,
    class: &str,
    object: PhpObject,
    hash: PhpArray,
) -> Result<(), Thrown> {
    if !initialize_from_hash(req, object, hash)? {
        return Err(throw_error(
            req,
            "Error",
            &format!("Invalid serialization data for {class} object"),
        ));
    }
    restore_properties(req, object, hash, &[b"date", b"timezone_type", b"timezone"])
}

/// `php_do_date_time_wakeup`: the object's own properties read back as
/// its time.
fn wakeup(req: &mut Request, class: &str, object: PhpObject) -> Result<(), Thrown> {
    let mut hash = PhpArray::new(req)?;
    for (name, value) in object.props(req)? {
        req.addref(&value);
        hash.set(req, Key::Str(name), value)?;
    }
    let done = initialize_from_hash(req, object, hash);
    req.release(Value::array(hash));
    if !done? {
        return Err(throw_error(
            req,
            "Error",
            &format!("Invalid serialization data for {class} object"),
        ));
    }
    Ok(())
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
#[php_function]
fn date_create(
    req: &mut Request,
    #[php(default = "now")] datetime: PhpString,
    #[php(default = null)] timezone: Option<Instance<DateTimeZone>>,
) -> Result<OrFalse<Instance<DateTime>>, Thrown> {
    date_create_of(req, "DateTime", datetime, timezone)
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
#[php_function]
fn date_create_immutable(
    req: &mut Request,
    #[php(default = "now")] datetime: PhpString,
    #[php(default = null)] timezone: Option<Instance<DateTimeZone>>,
) -> Result<OrFalse<Instance<DateTimeImmutable>>, Thrown> {
    date_create_of(req, "DateTimeImmutable", datetime, timezone)
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
#[php_function]
fn date_create_from_format(
    req: &mut Request,
    format: PhpString,
    datetime: Path,
    #[php(default = null)] timezone: Option<Instance<DateTimeZone>>,
) -> Result<OrFalse<Instance<DateTime>>, Thrown> {
    let class = class_id(req, "DateTime")?;
    from_format(req, class, format, datetime, timezone)
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
#[php_function]
fn date_create_immutable_from_format(
    req: &mut Request,
    format: PhpString,
    datetime: Path,
    #[php(default = null)] timezone: Option<Instance<DateTimeZone>>,
) -> Result<OrFalse<Instance<DateTimeImmutable>>, Thrown> {
    let class = class_id(req, "DateTimeImmutable")?;
    from_format(req, class, format, datetime, timezone)
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
#[php_function]
fn date_get_last_errors(req: &mut Request) -> Result<OrFalse<PhpArray>, Thrown> {
    last_errors(req)
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
#[php_function]
fn date_format(
    req: &mut Request,
    object: Instance<DateTimeInterface>,
    format: PhpString,
) -> Result<PhpString, Thrown> {
    format_of(req, object.object(), format)
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
#[php_function]
fn date_modify(
    req: &mut Request,
    object: Instance<DateTime>,
    modifier: PhpString,
) -> Result<OrFalse<Instance<DateTime>>, Thrown> {
    let object = object.object();
    if !modify(req, object, modifier.as_bytes(), None)? {
        return Ok(OrFalse::False);
    }
    Ok(OrFalse::Value(again(req, object)))
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
#[php_function]
fn date_add(
    req: &mut Request,
    object: Instance<DateTime>,
    interval: Instance<DateInterval>,
) -> Result<Instance<DateTime>, Thrown> {
    add(req, object.object(), interval.object())?;
    Ok(again(req, object.object()))
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
#[php_function]
fn date_sub(
    req: &mut Request,
    object: Instance<DateTime>,
    interval: Instance<DateInterval>,
) -> Result<Instance<DateTime>, Thrown> {
    sub(req, object.object(), interval.object(), None)?;
    Ok(again(req, object.object()))
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
#[php_function]
fn date_timezone_get(
    req: &mut Request,
    object: Instance<DateTimeInterface>,
) -> Result<OrFalse<Instance<DateTimeZone>>, Thrown> {
    timezone_of(req, object.object())
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
#[php_function]
fn date_timezone_set(
    req: &mut Request,
    object: Instance<DateTime>,
    timezone: Instance<DateTimeZone>,
) -> Result<Instance<DateTime>, Thrown> {
    timezone_set(req, object.object(), timezone.object())?;
    Ok(again(req, object.object()))
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
#[php_function]
fn date_offset_get(req: &mut Request, object: Instance<DateTimeInterface>) -> Result<i64, Thrown> {
    offset_of(req, object.object())
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
#[php_function]
fn date_diff(
    req: &mut Request,
    baseObject: Instance<DateTimeInterface>,
    targetObject: Instance<DateTimeInterface>,
    #[php(default = false)] absolute: bool,
) -> Result<Instance<DateInterval>, Thrown> {
    diff_of(req, baseObject.object(), targetObject.object(), absolute)
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
#[php_function]
fn date_time_set(
    req: &mut Request,
    object: Instance<DateTime>,
    hour: i64,
    minute: i64,
    #[php(default = 0)] second: i64,
    #[php(default = 0)] microsecond: i64,
) -> Result<Instance<DateTime>, Thrown> {
    time_set(req, object.object(), hour, minute, second, microsecond)?;
    Ok(again(req, object.object()))
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
#[php_function]
fn date_date_set(
    req: &mut Request,
    object: Instance<DateTime>,
    year: i64,
    month: i64,
    day: i64,
) -> Result<Instance<DateTime>, Thrown> {
    date_set(req, object.object(), year, month, day)?;
    Ok(again(req, object.object()))
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
#[php_function]
fn date_isodate_set(
    req: &mut Request,
    object: Instance<DateTime>,
    year: i64,
    week: i64,
    #[php(default = 1)] dayOfWeek: i64,
) -> Result<Instance<DateTime>, Thrown> {
    isodate_set(req, object.object(), year, week, dayOfWeek)?;
    Ok(again(req, object.object()))
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
#[php_function]
fn date_timestamp_set(
    req: &mut Request,
    object: Instance<DateTime>,
    timestamp: i64,
) -> Result<Instance<DateTime>, Thrown> {
    timestamp_set(req, object.object(), timestamp)?;
    Ok(again(req, object.object()))
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
#[php_function]
fn date_timestamp_get(
    req: &mut Request,
    object: Instance<DateTimeInterface>,
) -> Result<i64, Thrown> {
    timestamp_of(req, object.object())
}

/// `class DateTime implements DateTimeInterface`, over [`DateObj`]: each
/// method changes the object and answers it.
///
/// ```php
/// date_default_timezone_set('UTC');
/// $d = new DateTime('2000-01-31 10:00:00');
/// var_dump(DateTime::getLastErrors(), $d->add(new DateInterval('P1M'))->format('c'));
/// var_dump($d->setTimezone(new DateTimeZone('Asia/Tokyo'))->getTimezone()->getName(), $d->getOffset());
/// var_dump($d->setTime(1, 2)->setDate(2001, 2, 3)->format('c'), $d->setISODate(2001, 5)->format('c'), $d->setTimestamp(86400)->getTimestamp());
/// var_dump($d->diff(new DateTime('1970-01-03 12:00 UTC'))->format('%a %h'));
/// ```
/// ```expect
/// bool(false)
/// string(25) "2000-03-02T10:00:00+00:00"
/// string(10) "Asia/Tokyo"
/// int(32400)
/// string(25) "2001-02-03T01:02:00+09:00"
/// string(25) "2001-01-29T01:02:00+09:00"
/// int(86400)
/// string(4) "1 12"
/// ```
///
/// Each class's `create_object` allocates its `php_*_obj` by
/// `zend_object_alloc`, which `memory_get_usage()` counts (design §5.13):
///
/// ```php
/// foreach ([false, true] as $shown) foreach (['DateTime', 'DateTimeImmutable', 'DateTimeZone', 'DateInterval', 'DatePeriod'] as $c) {
///     $r = new ReflectionClass($c);
///     $m = memory_get_usage();
///     $o = $r->newInstanceWithoutConstructor();
///     $used = memory_get_usage() - $m;
///     $o = null;
///     if ($shown) echo $c, ' ', $used, "\n";
/// }
/// ```
/// ```expect
/// DateTime 48
/// DateTimeImmutable 48
/// DateTimeZone 80
/// DateInterval 64
/// DatePeriod 96
/// ```
#[php_class(implements(DateTimeInterface), size = 64, payload = DateObj, handlers = DATE)]
pub(crate) struct DateTime;

#[php_class]
impl DateTime {
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
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = "now")] datetime: PhpString,
        #[php(default = null)] timezone: Option<Instance<DateTimeZone>>,
    ) -> Result<(), Thrown> {
        let how = Init {
            ctor: true,
            format: false,
        };
        let timezone = timezone.map(|tz| tz.object());
        initialize(req, this.object(), datetime.as_bytes(), None, timezone, how)?;
        Ok(())
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
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<PhpArray, Thrown> {
        let object = this.object();
        let time = time_of(req, object)?;
        let fields = date_fields(req, &time)?;
        let own = std_rows(req, object)?;
        rows_array(req, &own, fields, true)
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
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        unserialize(req, "DateTime", this.object(), data)
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
    #[php(
        tentative,
        deprecated(
            since = "8.5",
            message = "this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize()"
        )
    )]
    fn __wakeup(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        wakeup(req, "DateTime", this.object())
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
    #[php(tentative)]
    fn __set_state(req: &mut Request, array: PhpArray) -> Result<Instance<DateTime>, Thrown> {
        set_state(req, "DateTime", array).map(Instance::of)
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
    #[php(tentative)]
    fn createFromImmutable(
        req: &mut Request,
        object: Instance<DateTimeImmutable>,
    ) -> Result<Static, Thrown> {
        create_from(req, object.object(), "DateTime").map(Static)
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
    fn createFromInterface(
        req: &mut Request,
        object: Instance<DateTimeInterface>,
    ) -> Result<Instance<DateTime>, Thrown> {
        create_from(req, object.object(), "DateTime").map(Instance::of)
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
    #[php(tentative)]
    fn createFromFormat(
        req: &mut Request,
        format: PhpString,
        datetime: Path,
        #[php(default = null)] timezone: Option<Instance<DateTimeZone>>,
    ) -> Result<OrFalse<Instance<DateTime>>, Thrown> {
        let class = called_or(req, "DateTime")?;
        from_format(req, class, format, datetime, timezone)
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
    #[php(tentative)]
    fn createFromTimestamp(
        req: &mut Request,
        timestamp: Union<i64, f64>,
    ) -> Result<Static, Thrown> {
        create_from_timestamp(req, timestamp, "DateTime::createFromTimestamp", "DateTime")
    }

    /// `DateTime::getLastErrors(): array|false`, `date_get_last_errors`'
    /// body.
    #[php(tentative)]
    fn getLastErrors(req: &mut Request) -> Result<OrFalse<PhpArray>, Thrown> {
        last_errors(req)
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
    #[php(tentative)]
    fn format(req: &mut Request, this: This<Self>, format: PhpString) -> Result<PhpString, Thrown> {
        format_of(req, this.object(), format)
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
    #[php(tentative)]
    fn modify(
        req: &mut Request,
        this: This<Self>,
        modifier: PhpString,
    ) -> Result<Instance<DateTime>, Thrown> {
        let object = this.object();
        modify(req, object, modifier.as_bytes(), Some("DateTime::modify"))?;
        Ok(again(req, object))
    }

    /// `DateTime::add(DateInterval $interval): DateTime`, `date_add`'s
    /// body.
    #[php(tentative)]
    fn add(
        req: &mut Request,
        this: This<Self>,
        interval: Instance<DateInterval>,
    ) -> Result<Instance<DateTime>, Thrown> {
        add(req, this.object(), interval.object())?;
        Ok(again(req, this.object()))
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
    #[php(tentative)]
    fn sub(
        req: &mut Request,
        this: This<Self>,
        interval: Instance<DateInterval>,
    ) -> Result<Instance<DateTime>, Thrown> {
        sub(req, this.object(), interval.object(), Some("DateTime::sub"))?;
        Ok(again(req, this.object()))
    }

    /// `DateTimeInterface::getTimezone(): DateTimeZone|false`,
    /// `date_timezone_get`'s body.
    #[php(tentative)]
    fn getTimezone(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Instance<DateTimeZone>>, Thrown> {
        timezone_of(req, this.object())
    }

    /// `DateTime::setTimezone(DateTimeZone $timezone): DateTime`,
    /// `date_timezone_set`'s body.
    #[php(tentative)]
    fn setTimezone(
        req: &mut Request,
        this: This<Self>,
        timezone: Instance<DateTimeZone>,
    ) -> Result<Instance<DateTime>, Thrown> {
        timezone_set(req, this.object(), timezone.object())?;
        Ok(again(req, this.object()))
    }

    /// `DateTimeInterface::getOffset(): int`, `date_offset_get`'s body.
    #[php(tentative)]
    fn getOffset(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        offset_of(req, this.object())
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
    fn getMicrosecond(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        Ok(time_of(req, this.object())?.us)
    }

    /// `DateTime::setTime(...)`, `date_time_set`'s body.
    #[php(tentative)]
    fn setTime(
        req: &mut Request,
        this: This<Self>,
        hour: i64,
        minute: i64,
        #[php(default = 0)] second: i64,
        #[php(default = 0)] microsecond: i64,
    ) -> Result<Instance<DateTime>, Thrown> {
        time_set(req, this.object(), hour, minute, second, microsecond)?;
        Ok(again(req, this.object()))
    }

    /// `DateTime::setDate(int $year, int $month, int $day): DateTime`,
    /// `date_date_set`'s body.
    #[php(tentative)]
    fn setDate(
        req: &mut Request,
        this: This<Self>,
        year: i64,
        month: i64,
        day: i64,
    ) -> Result<Instance<DateTime>, Thrown> {
        date_set(req, this.object(), year, month, day)?;
        Ok(again(req, this.object()))
    }

    /// `DateTime::setISODate(...)`, `date_isodate_set`'s body.
    #[php(tentative)]
    fn setISODate(
        req: &mut Request,
        this: This<Self>,
        year: i64,
        week: i64,
        #[php(default = 1)] dayOfWeek: i64,
    ) -> Result<Instance<DateTime>, Thrown> {
        isodate_set(req, this.object(), year, week, dayOfWeek)?;
        Ok(again(req, this.object()))
    }

    /// `DateTime::setTimestamp(int $timestamp): DateTime`,
    /// `date_timestamp_set`'s body.
    #[php(tentative)]
    fn setTimestamp(
        req: &mut Request,
        this: This<Self>,
        timestamp: i64,
    ) -> Result<Instance<DateTime>, Thrown> {
        timestamp_set(req, this.object(), timestamp)?;
        Ok(again(req, this.object()))
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
    fn setMicrosecond(
        req: &mut Request,
        this: This<Self>,
        microsecond: i64,
    ) -> Result<Static, Thrown> {
        microsecond_in_range(req, "DateTime::setMicrosecond", microsecond)?;
        let object = this.object();
        let mut time = time_of(req, object)?;
        time.us = microsecond;
        set_time(req, object, Some(time));
        // `RETURN_OBJ_COPY`, answered as `static`.
        req.addref(&object.as_value());
        Ok(Static(object))
    }

    /// `DateTimeInterface::getTimestamp(): int`, `date_timestamp_get`'s
    /// body.
    #[php(tentative)]
    fn getTimestamp(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        timestamp_of(req, this.object())
    }

    /// `DateTimeInterface::diff(...)`, `date_diff`'s body.
    #[php(tentative)]
    fn diff(
        req: &mut Request,
        this: This<Self>,
        targetObject: Instance<DateTimeInterface>,
        #[php(default = false)] absolute: bool,
    ) -> Result<Instance<DateInterval>, Thrown> {
        diff_of(req, this.object(), targetObject.object(), absolute)
    }
}

/// `class DateTimeImmutable implements DateTimeInterface`, over the
/// payload and the table `DateTime` has.
#[php_class(implements(DateTimeInterface), size = 64, payload = DateObj, handlers = DATE)]
pub(crate) struct DateTimeImmutable;

#[php_class]
impl DateTimeImmutable {
    #[php(implementation = DateTime::__construct)]
    fn __construct(
        this: This<Self>,
        #[php(default = "now")] datetime: PhpString,
        #[php(default = null)] timezone: Option<Instance<DateTimeZone>>,
    );

    #[php(implementation = DateTime::__serialize)]
    fn __serialize(this: This<Self>) -> PhpArray;

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
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        unserialize(req, "DateTimeImmutable", this.object(), data)
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
    #[php(
        tentative,
        deprecated(
            since = "8.5",
            message = "this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize()"
        )
    )]
    fn __wakeup(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        wakeup(req, "DateTimeImmutable", this.object())
    }

    /// `DateTimeImmutable::__set_state(array $array): DateTimeImmutable`.
    ///
    /// ```php
    /// var_dump(DateTimeImmutable::__set_state(['date' => '2000-01-01 00:00:00.000000', 'timezone_type' => 2, 'timezone' => 'EST'])->format('c T'));
    /// ```
    /// ```expect
    /// string(29) "2000-01-01T00:00:00-05:00 EST"
    /// ```
    #[php(tentative)]
    fn __set_state(
        req: &mut Request,
        array: PhpArray,
    ) -> Result<Instance<DateTimeImmutable>, Thrown> {
        set_state(req, "DateTimeImmutable", array).map(Instance::of)
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
    #[php(tentative)]
    fn createFromFormat(
        req: &mut Request,
        format: PhpString,
        datetime: Path,
        #[php(default = null)] timezone: Option<Instance<DateTimeZone>>,
    ) -> Result<OrFalse<Instance<DateTimeImmutable>>, Thrown> {
        let class = called_or(req, "DateTimeImmutable")?;
        from_format(req, class, format, datetime, timezone)
    }

    /// `DateTimeImmutable::createFromTimestamp(int|float $timestamp): static`.
    ///
    /// ```php
    /// var_dump(DateTimeImmutable::createFromTimestamp(0.5)->format(DATE_RFC3339_EXTENDED));
    /// ```
    /// ```expect
    /// string(29) "1970-01-01T00:00:00.500+00:00"
    /// ```
    #[php(tentative)]
    fn createFromTimestamp(
        req: &mut Request,
        timestamp: Union<i64, f64>,
    ) -> Result<Static, Thrown> {
        create_from_timestamp(
            req,
            timestamp,
            "DateTimeImmutable::createFromTimestamp",
            "DateTimeImmutable",
        )
    }

    #[php(tentative, implementation = DateTime::getLastErrors)]
    fn getLastErrors() -> OrFalse<PhpArray>;

    #[php(tentative, implementation = DateTime::format)]
    fn format(this: This<Self>, format: PhpString) -> PhpString;

    #[php(tentative, implementation = DateTime::getTimezone)]
    fn getTimezone(this: This<Self>) -> OrFalse<Instance<DateTimeZone>>;

    #[php(tentative, implementation = DateTime::getOffset)]
    fn getOffset(this: This<Self>) -> i64;

    #[php(tentative, implementation = DateTime::getTimestamp)]
    fn getTimestamp(this: This<Self>) -> i64;

    #[php(implementation = DateTime::getMicrosecond)]
    fn getMicrosecond(this: This<Self>) -> i64;

    #[php(tentative, implementation = DateTime::diff)]
    fn diff(
        this: This<Self>,
        targetObject: Instance<DateTimeInterface>,
        #[php(default = false)] absolute: bool,
    ) -> Instance<DateInterval>;

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
    #[php(
        tentative,
        no_discard(message = "as DateTimeImmutable::modify() does not modify the object itself")
    )]
    fn modify(
        req: &mut Request,
        this: This<Self>,
        modifier: PhpString,
    ) -> Result<Instance<DateTimeImmutable>, Thrown> {
        let text = modifier.as_bytes();
        changed_copy(req, this.object(), |req, copy| {
            modify(req, copy, text, Some("DateTimeImmutable::modify")).map(drop)
        })
        .map(Instance::of)
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
    #[php(
        tentative,
        no_discard(message = "as DateTimeImmutable::add() does not modify the object itself")
    )]
    fn add(
        req: &mut Request,
        this: This<Self>,
        interval: Instance<DateInterval>,
    ) -> Result<Instance<DateTimeImmutable>, Thrown> {
        changed_copy(req, this.object(), |req, copy| {
            add(req, copy, interval.object())
        })
        .map(Instance::of)
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
    #[php(
        tentative,
        no_discard(message = "as DateTimeImmutable::sub() does not modify the object itself")
    )]
    fn sub(
        req: &mut Request,
        this: This<Self>,
        interval: Instance<DateInterval>,
    ) -> Result<Instance<DateTimeImmutable>, Thrown> {
        changed_copy(req, this.object(), |req, copy| {
            sub(req, copy, interval.object(), Some("DateTimeImmutable::sub"))
        })
        .map(Instance::of)
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
    #[php(
        tentative,
        no_discard(
            message = "as DateTimeImmutable::setTimezone() does not modify the object itself"
        )
    )]
    fn setTimezone(
        req: &mut Request,
        this: This<Self>,
        timezone: Instance<DateTimeZone>,
    ) -> Result<Instance<DateTimeImmutable>, Thrown> {
        changed_copy(req, this.object(), |req, copy| {
            timezone_set(req, copy, timezone.object())
        })
        .map(Instance::of)
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
    #[php(
        tentative,
        no_discard(message = "as DateTimeImmutable::setTime() does not modify the object itself")
    )]
    fn setTime(
        req: &mut Request,
        this: This<Self>,
        hour: i64,
        minute: i64,
        #[php(default = 0)] second: i64,
        #[php(default = 0)] microsecond: i64,
    ) -> Result<Instance<DateTimeImmutable>, Thrown> {
        changed_copy(req, this.object(), |req, copy| {
            time_set(req, copy, hour, minute, second, microsecond)
        })
        .map(Instance::of)
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
    #[php(
        tentative,
        no_discard(message = "as DateTimeImmutable::setDate() does not modify the object itself")
    )]
    fn setDate(
        req: &mut Request,
        this: This<Self>,
        year: i64,
        month: i64,
        day: i64,
    ) -> Result<Instance<DateTimeImmutable>, Thrown> {
        changed_copy(req, this.object(), |req, copy| {
            date_set(req, copy, year, month, day)
        })
        .map(Instance::of)
    }

    /// `DateTimeImmutable::setISODate(...)`.
    ///
    /// ```php
    /// var_dump((new DateTimeImmutable('2000-01-01 UTC'))->setISODate(2015, 10, 3)->format('Y-m-d'));
    /// ```
    /// ```expect
    /// string(10) "2015-03-04"
    /// ```
    #[php(
        tentative,
        no_discard(
            message = "as DateTimeImmutable::setISODate() does not modify the object itself"
        )
    )]
    fn setISODate(
        req: &mut Request,
        this: This<Self>,
        year: i64,
        week: i64,
        #[php(default = 1)] dayOfWeek: i64,
    ) -> Result<Instance<DateTimeImmutable>, Thrown> {
        changed_copy(req, this.object(), |req, copy| {
            isodate_set(req, copy, year, week, dayOfWeek)
        })
        .map(Instance::of)
    }

    /// `DateTimeImmutable::setTimestamp(...)`.
    ///
    /// ```php
    /// var_dump((new DateTimeImmutable('now', new DateTimeZone('UTC')))->setTimestamp(86400 * 365)->format('c'));
    /// ```
    /// ```expect
    /// string(25) "1971-01-01T00:00:00+00:00"
    /// ```
    #[php(
        tentative,
        no_discard(
            message = "as DateTimeImmutable::setTimestamp() does not modify the object itself"
        )
    )]
    fn setTimestamp(
        req: &mut Request,
        this: This<Self>,
        timestamp: i64,
    ) -> Result<Instance<DateTimeImmutable>, Thrown> {
        changed_copy(req, this.object(), |req, copy| {
            timestamp_set(req, copy, timestamp)
        })
        .map(Instance::of)
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
    #[php(no_discard(
        message = "as DateTimeImmutable::setMicrosecond() does not modify the object itself"
    ))]
    fn setMicrosecond(
        req: &mut Request,
        this: This<Self>,
        microsecond: i64,
    ) -> Result<Static, Thrown> {
        microsecond_in_range(req, "DateTimeImmutable::setMicrosecond", microsecond)?;
        let object = this.object();
        let mut time = time_of(req, object)?;
        time.us = microsecond;
        let copy = clone_date(req, object)?;
        set_time(req, copy, Some(time));
        Ok(Static(copy))
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
    #[php(tentative)]
    fn createFromMutable(req: &mut Request, object: Instance<DateTime>) -> Result<Static, Thrown> {
        create_from(req, object.object(), "DateTimeImmutable").map(Static)
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
    fn createFromInterface(
        req: &mut Request,
        object: Instance<DateTimeInterface>,
    ) -> Result<Instance<DateTimeImmutable>, Thrown> {
        create_from(req, object.object(), "DateTimeImmutable").map(Instance::of)
    }
}
