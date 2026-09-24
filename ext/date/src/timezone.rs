//! `DateTimeZone` and the procedural natives over it: `timezone_initialize`
//! over `timelib_parse_zone`, the name, offset, transitions and location
//! of a zone, and its serialised form.

use std::sync::Arc;

use php_ext::call::new_object;
use php_ext::raising::{Level, Thrown, docref, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, Value};

use crate::args::{
    array, count_between, object_of, optional_long, set_str, set_string, string_value,
};
use crate::datetime::restore_properties;
use crate::ffi::{Time, ZONETYPE_ABBR, ZONETYPE_ID, ZONETYPE_OFFSET};
use crate::format::format_timestamp;
use crate::obj::{TzKind, TzObj, class_id, rows_array, std_rows, this, time_of, tz_fields, tz_of};
use crate::parse::path_string;

/// `timezone_initialize(tzobj, tz, &warning)`: an identifier, an
/// abbreviation or an offset, as `timelib_parse_zone` reads it, or the
/// message php gives for one it cannot use.
pub(super) fn initialize(text: &[u8]) -> Result<TzKind, String> {
    let shown = String::from_utf8_lossy(text).into_owned();
    let mut dummy = Time::new();
    let (offset, dst, not_found, read) = dummy.parse_zone(text);
    #[allow(clippy::cast_possible_truncation)]
    let z = offset as i32;
    dummy.z = z;
    if offset >= 100 * 60 * 60 || offset <= -100 * 60 * 60 {
        return Err(format!("Timezone offset is out of range ({shown})"));
    }
    dummy.dst = dst;
    if not_found || read < text.len() {
        return Err(format!("Unknown or bad timezone ({shown})"));
    }
    match dummy.zone_type {
        ZONETYPE_ID => dummy
            .zone()
            .map(|zone| TzKind::Id(Arc::clone(zone)))
            .ok_or_else(|| format!("Unknown or bad timezone ({shown})")),
        ZONETYPE_OFFSET => Ok(TzKind::Offset(i64::from(dummy.z))),
        ZONETYPE_ABBR => Ok(TzKind::Abbr {
            utc_offset: i64::from(dummy.z),
            abbr: dummy.tz_abbr().to_vec(),
            dst: dummy.dst,
        }),
        _ => Err(format!("Unknown or bad timezone ({shown})")),
    }
}

/// The zone of a new object of the class's.
fn set_zone(req: &mut Request, object: PhpObject, kind: TzKind) {
    if let Some(payload) = object.native_mut::<TzObj>(req) {
        payload.tz = Some(kind);
    }
}

/// `timezone_open(string $timezone): DateTimeZone|false`: php's warning
/// and `false` for a zone it cannot read.
///
/// ```php
/// var_dump(timezone_open('Europe/Lisbon'), timezone_open('+0200'), timezone_open('cest'), timezone_open('Mars/Base'), timezone_open('+99:00'));
/// ```
/// ```expect
/// Warning: timezone_open(): Unknown or bad timezone (Mars/Base) in %s on line 2
/// object(DateTimeZone)#1 (2) {
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(13) "Europe/Lisbon"
/// }
/// object(DateTimeZone)#2 (2) {
///   ["timezone_type"]=>
///   int(1)
///   ["timezone"]=>
///   string(6) "+02:00"
/// }
/// object(DateTimeZone)#3 (2) {
///   ["timezone_type"]=>
///   int(2)
///   ["timezone"]=>
///   string(4) "CEST"
/// }
/// bool(false)
/// object(DateTimeZone)#4 (2) {
///   ["timezone_type"]=>
///   int(1)
///   ["timezone"]=>
///   string(6) "+99:00"
/// }
/// ```
pub(crate) fn timezone_open(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let text = path_string(req, 1)?.as_bytes().to_vec();
    match initialize(&text) {
        Ok(kind) => crate::obj::new_timezone(req, kind),
        Err(message) => {
            docref(req, Level::Warning, &message);
            Ok(Value::bool(false))
        }
    }
}

/// `DateTimeZone::__construct(string $timezone)`: the message thrown as
/// `DateInvalidTimeZoneException`.
///
/// ```php
/// var_dump(new DateTimeZone('America/Argentina/Buenos_Aires'), new DateTimeZone('-03:30'), new DateTimeZone('UTC'));
/// try { new DateTimeZone('Nowhere/Special'); } catch (DateInvalidTimeZoneException $e) { echo $e->getMessage(), "\n"; }
/// try { new DateTimeZone("UTC\0x"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(DateTimeZone)#1 (2) {
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(30) "America/Argentina/Buenos_Aires"
/// }
/// object(DateTimeZone)#2 (2) {
///   ["timezone_type"]=>
///   int(1)
///   ["timezone"]=>
///   string(6) "-03:30"
/// }
/// object(DateTimeZone)#3 (2) {
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(3) "UTC"
/// }
/// DateTimeZone::__construct(): Unknown or bad timezone (Nowhere/Special)
/// DateTimeZone::__construct(): Argument #1 ($timezone) must not contain any null bytes
/// ```
///
/// A subclass's `__debugInfo` is what `var_dump` shows.
///
/// ```php
/// class U extends DateTimeZone { public function __debugInfo(): array { return ['value' => 'zzz']; } }
/// var_dump(new U('Europe/Kyiv'));
/// ```
/// ```expect
/// object(U)#1 (1) {
///   ["value"]=>
///   string(3) "zzz"
/// }
/// ```
pub(crate) fn construct(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let text = path_string(req, 1)?.as_bytes().to_vec();
    let object = this(req)?;
    match initialize(&text) {
        Ok(kind) => {
            set_zone(req, object, kind);
            Ok(Value::null())
        }
        Err(message) => Err(throw(
            req,
            "DateInvalidTimeZoneException",
            &format!("DateTimeZone::__construct(): {message}"),
        )),
    }
}

/// The zone a body works on: `$this` for a method, argument 1 for a
/// function, and the position its first own argument stands at.
fn receiver(req: &mut Request) -> Result<(PhpObject, u32, usize), Thrown> {
    if req.this().is_some() {
        Ok((this(req)?, 1, 0))
    } else {
        Ok((object_of(req, 1, "DateTimeZone")?, 2, 1))
    }
}

/// `timezone_name_get(DateTimeZone $object): string`, and
/// `DateTimeZone::getName`.
///
/// ```php
/// var_dump(timezone_name_get(new DateTimeZone('europe/paris')), (new DateTimeZone('+01:30'))->getName(), (new DateTimeZone('EST'))->getName(), (new DateTimeZone('+01:30:45'))->getName());
/// ```
/// ```expect
/// string(12) "europe/paris"
/// string(6) "+01:30"
/// string(3) "EST"
/// string(9) "+01:30:45"
/// ```
pub(crate) fn timezone_name_get(req: &mut Request) -> Result<Value, Thrown> {
    let (object, _, shift) = receiver(req)?;
    count_between(req, shift, shift)?;
    let kind = tz_of(req, object)?;
    string_value(req, &kind.name())
}

/// `timezone_offset_get(DateTimeZone $object, DateTimeInterface
/// $datetime): int`, and `DateTimeZone::getOffset`.
///
/// ```php
/// $d = new DateTime('2020-07-01 UTC');
/// var_dump(timezone_offset_get(new DateTimeZone('America/Los_Angeles'), $d), (new DateTimeZone('+0430'))->getOffset($d), (new DateTimeZone('CEST'))->getOffset($d));
/// try { timezone_offset_get(new stdClass, $d); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { timezone_offset_get(new DateTimeZone('UTC'), new DateTimeZone('UTC')); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(-25200)
/// int(16200)
/// int(7200)
/// timezone_offset_get(): Argument #1 ($object) must be of type DateTimeZone, stdClass given
/// timezone_offset_get(): Argument #2 ($datetime) must be of type DateTimeInterface, DateTimeZone given
/// ```
pub(crate) fn timezone_offset_get(req: &mut Request) -> Result<Value, Thrown> {
    let (object, at, shift) = receiver(req)?;
    count_between(req, 1 + shift, 1 + shift)?;
    let date = object_of(req, at, "DateTimeInterface")?;
    let kind = tz_of(req, object)?;
    let time = time_of(req, date)?;
    Ok(Value::int(match kind {
        TzKind::Id(zone) => i64::from(zone.offset_at(time.sse).offset),
        TzKind::Offset(offset) => offset,
        TzKind::Abbr {
            utc_offset, dst, ..
        } => utc_offset + i64::from(dst) * 3600,
    }))
}

/// One row of `getTransitions`.
fn transition(
    req: &mut Request,
    out: &mut PhpArray,
    ts: i64,
    offset: i64,
    is_dst: bool,
    abbr: &[u8],
) -> Result<(), Thrown> {
    let mut row = PhpArray::new(req)?;
    set_str(req, &mut row, b"ts", Value::int(ts))?;
    let time = format_timestamp(req, b"x-m-d\\TH:i:sP", ts, false)?;
    set_string(req, &mut row, b"time", &time)?;
    set_str(req, &mut row, b"offset", Value::int(offset))?;
    set_str(req, &mut row, b"isdst", Value::bool(is_dst))?;
    set_string(req, &mut row, b"abbr", abbr)?;
    out.append(req, Value::array(row))
}

/// `timezone_transitions_get(DateTimeZone $object, int $timestampBegin =
/// PHP_INT_MIN, int $timestampEnd = 2147483647): array|false`, and
/// `DateTimeZone::getTransitions`: the zone's transitions in the range,
/// the one in force at its start first, and past the last the POSIX
/// string's, year by year.
///
/// ```php
/// $tz = new DateTimeZone('Europe/Amsterdam');
/// var_dump(count($tz->getTransitions()) > 100, $tz->getTransitions(1700000000, 1735689600));
/// var_dump(timezone_transitions_get(new DateTimeZone('UTC')), (new DateTimeZone('+02:00'))->getTransitions());
/// ```
/// ```expect
/// bool(true)
/// array(3) {
///   [0]=>
///   array(5) {
///     ["ts"]=>
///     int(1700000000)
///     ["time"]=>
///     string(25) "2023-11-14T22:13:20+00:00"
///     ["offset"]=>
///     int(3600)
///     ["isdst"]=>
///     bool(false)
///     ["abbr"]=>
///     string(3) "CET"
///   }
///   [1]=>
///   array(5) {
///     ["ts"]=>
///     int(1711846800)
///     ["time"]=>
///     string(25) "2024-03-31T01:00:00+00:00"
///     ["offset"]=>
///     int(7200)
///     ["isdst"]=>
///     bool(true)
///     ["abbr"]=>
///     string(4) "CEST"
///   }
///   [2]=>
///   array(5) {
///     ["ts"]=>
///     int(1729990800)
///     ["time"]=>
///     string(25) "2024-10-27T01:00:00+00:00"
///     ["offset"]=>
///     int(3600)
///     ["isdst"]=>
///     bool(false)
///     ["abbr"]=>
///     string(3) "CET"
///   }
/// }
/// array(1) {
///   [0]=>
///   array(5) {
///     ["ts"]=>
///     int(-9223372036854775808)
///     ["time"]=>
///     string(34) "-292277022657-01-27T08:29:52+00:00"
///     ["offset"]=>
///     int(0)
///     ["isdst"]=>
///     bool(false)
///     ["abbr"]=>
///     string(3) "UTC"
///   }
/// }
/// bool(false)
/// ```
#[allow(clippy::too_many_lines)]
pub(crate) fn timezone_transitions_get(req: &mut Request) -> Result<Value, Thrown> {
    let (object, at, shift) = receiver(req)?;
    count_between(req, shift, 2 + shift)?;
    let begin_ts = optional_long(req, at, i64::MIN)?;
    let end_ts = optional_long(req, at + 1, i64::from(i32::MAX))?;
    let kind = tz_of(req, object)?;
    let TzKind::Id(zone) = kind else {
        return Ok(Value::bool(false));
    };
    let types = zone.types();
    let trans = zone.transitions();
    let ty = |i: usize| types.get(i).cloned();
    let mut out = PhpArray::new(req)?;
    let nominal = |req: &mut Request, out: &mut PhpArray, ts: i64| -> Result<(), Thrown> {
        match ty(0) {
            Some(t) => transition(req, out, ts, i64::from(t.offset), t.is_dst, &t.abbr),
            None => Ok(()),
        }
    };
    let (mut begin, found) = if begin_ts == i64::MIN {
        nominal(req, &mut out, begin_ts)?;
        (0, true)
    } else {
        let mut found = false;
        let mut at = 0;
        while at < trans.len() {
            if trans[at].0 > begin_ts {
                if at > 0 {
                    if let Some(t) = ty(trans[at - 1].1) {
                        transition(
                            req,
                            &mut out,
                            begin_ts,
                            i64::from(t.offset),
                            t.is_dst,
                            &t.abbr,
                        )?;
                    }
                } else {
                    nominal(req, &mut out, begin_ts)?;
                }
                found = true;
                break;
            }
            at += 1;
        }
        (at, found)
    };
    let posix = zone.has_posix();
    if found {
        while begin < trans.len() {
            let (ts, i) = trans[begin];
            if ts >= end_ts {
                return Ok(Value::array(out));
            }
            if let Some(t) = ty(i) {
                transition(req, &mut out, ts, i64::from(t.offset), t.is_dst, &t.abbr)?;
            }
            begin += 1;
        }
    } else if trans.is_empty() {
        nominal(req, &mut out, begin_ts)?;
    } else if posix {
        let o = zone.offset_at(begin_ts);
        transition(
            req,
            &mut out,
            begin_ts,
            i64::from(o.offset),
            o.is_dst,
            &o.abbr,
        )?;
    } else if let Some(t) = trans.last().and_then(|(_, i)| ty(*i)) {
        transition(
            req,
            &mut out,
            begin_ts,
            i64::from(t.offset),
            t.is_dst,
            &t.abbr,
        )?;
    }
    if posix && let Some(&(last, _)) = trans.last() {
        let (start_y, _, _) = year_of(last);
        let (end_y, _, _) = year_of(end_ts);
        for year in start_y..=end_y {
            for (ts, i) in zone.posix_transitions(year) {
                if ts <= last || ts < begin_ts {
                    continue;
                }
                if ts > end_ts {
                    return Ok(Value::array(out));
                }
                if let Some(t) = ty(i) {
                    transition(req, &mut out, ts, i64::from(t.offset), t.is_dst, &t.abbr)?;
                }
            }
        }
    }
    Ok(Value::array(out))
}

/// `timelib_unixtime2date(ts, &y, &m, &d)`.
fn year_of(ts: i64) -> (i64, i64, i64) {
    let mut t = Time::new();
    t.unixtime2gmt(ts);
    (t.y, t.m, t.d)
}

/// `timezone_location_get(DateTimeZone $object): array|false`, and
/// `DateTimeZone::getLocation`.
///
/// ```php
/// var_dump(timezone_location_get(new DateTimeZone('Europe/Amsterdam')), (new DateTimeZone('UTC'))->getLocation(), (new DateTimeZone('+01:00'))->getLocation());
/// ```
/// ```expect
/// array(4) {
///   ["country_code"]=>
///   string(2) "NL"
///   ["latitude"]=>
///   float(52.366659999999996)
///   ["longitude"]=>
///   float(4.900000000000006)
///   ["comments"]=>
///   string(0) ""
/// }
/// array(4) {
///   ["country_code"]=>
///   string(2) "??"
///   ["latitude"]=>
///   float(-90)
///   ["longitude"]=>
///   float(-180)
///   ["comments"]=>
///   string(0) ""
/// }
/// bool(false)
/// ```
pub(crate) fn timezone_location_get(req: &mut Request) -> Result<Value, Thrown> {
    let (object, _, shift) = receiver(req)?;
    count_between(req, shift, shift)?;
    let kind = tz_of(req, object)?;
    let TzKind::Id(zone) = kind else {
        return Ok(Value::bool(false));
    };
    let loc = zone.location();
    let mut out = PhpArray::new(req)?;
    set_string(req, &mut out, b"country_code", &loc.country_code)?;
    set_str(req, &mut out, b"latitude", Value::float(loc.latitude))?;
    set_str(req, &mut out, b"longitude", Value::float(loc.longitude))?;
    set_string(req, &mut out, b"comments", &loc.comments)?;
    Ok(Value::array(out))
}

/// `DateTimeZone::listAbbreviations(): array`.
///
/// ```php
/// var_dump(DateTimeZone::listAbbreviations()['acst'][0]);
/// ```
/// ```expect
/// array(3) {
///   ["dst"]=>
///   bool(false)
///   ["offset"]=>
///   int(34200)
///   ["timezone_id"]=>
///   string(18) "Australia/Adelaide"
/// }
/// ```
pub(crate) fn list_abbreviations(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    crate::tzdb::abbreviations(req)
}

/// `DateTimeZone::listIdentifiers(int $timezoneGroup = DateTimeZone::ALL,
/// ?string $countryCode = null): array`.
///
/// ```php
/// var_dump(DateTimeZone::listIdentifiers(DateTimeZone::PER_COUNTRY, 'BE'), count(DateTimeZone::listIdentifiers(DateTimeZone::EUROPE)) > 50);
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   string(15) "Europe/Brussels"
/// }
/// bool(true)
/// ```
pub(crate) fn list_identifiers(req: &mut Request) -> Result<Value, Thrown> {
    crate::tzdb::timezone_identifiers_list(req)
}

/// `php_date_timezone_initialize_from_hash`.
fn from_hash(req: &mut Request, hash: PhpArray) -> Result<Option<TzKind>, Thrown> {
    let find = |req: &mut Request, name: &[u8]| -> Result<Option<Value>, Thrown> {
        let key = Key::Str(PhpString::new(req, name)?);
        Ok(hash.get(&key).map(|v| v.deref()))
    };
    let Some(zone_type) = find(req, b"timezone_type")? else {
        return Ok(None);
    };
    let Some(zone) = find(req, b"timezone")? else {
        return Ok(None);
    };
    let Some(zone_type) = zone_type.as_int() else {
        return Ok(None);
    };
    if !(i64::from(ZONETYPE_OFFSET)..=i64::from(ZONETYPE_ID)).contains(&zone_type) {
        return Ok(None);
    }
    let Some(text) = zone.as_str_bytes() else {
        return Ok(None);
    };
    if text.contains(&0) {
        return Ok(None);
    }
    Ok(initialize(text).ok())
}

/// `DateTimeZone::__set_state(array $array): DateTimeZone`.
///
/// ```php
/// var_dump(DateTimeZone::__set_state(['timezone_type' => 3, 'timezone' => 'Africa/Cairo']), DateTimeZone::__set_state(['timezone_type' => 1, 'timezone' => '+05:00'])->getName());
/// try { DateTimeZone::__set_state(['timezone_type' => 9, 'timezone' => 'UTC']); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_export(new DateTimeZone('Europe/Kyiv'));
/// ```
/// ```expect
/// object(DateTimeZone)#1 (2) {
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(12) "Africa/Cairo"
/// }
/// string(6) "+05:00"
/// Invalid serialization data for DateTimeZone object
/// \DateTimeZone::__set_state(array(
///    'timezone_type' => 3,
///    'timezone' => 'Europe/Kyiv',
/// ))
/// ```
pub(crate) fn set_state(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let hash = array(req, 1)?;
    let class = class_id(req, "DateTimeZone")?;
    let object = new_object(req, class)?;
    match from_hash(req, hash) {
        Ok(Some(kind)) => {
            set_zone(req, object, kind);
            Ok(object.as_value())
        }
        Ok(None) => {
            req.release(object.as_value());
            Err(throw(
                req,
                "Error",
                "Invalid serialization data for DateTimeZone object",
            ))
        }
        Err(thrown) => {
            req.release(object.as_value());
            Err(thrown)
        }
    }
}

/// `DateTimeZone::__serialize(): array`.
///
/// ```php
/// var_dump((new DateTimeZone('Asia/Singapore'))->__serialize(), (new DateTimeZone('-01:00'))->__serialize());
/// ```
/// ```expect
/// array(2) {
///   ["timezone_type"]=>
///   int(3)
///   ["timezone"]=>
///   string(14) "Asia/Singapore"
/// }
/// array(2) {
///   ["timezone_type"]=>
///   int(1)
///   ["timezone"]=>
///   string(6) "-01:00"
/// }
/// ```
pub(crate) fn serialize(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    let object = this(req)?;
    let kind = tz_of(req, object)?;
    let fields = tz_fields(req, &kind)?;
    let own = std_rows(req, object)?;
    Ok(Value::array(rows_array(req, &own, fields, true)?))
}

/// `DateTimeZone::__unserialize(array $data): void`.
///
/// ```php
/// $tz = new DateTimeZone('UTC');
/// $tz->__unserialize(['timezone_type' => 3, 'timezone' => 'America/Denver']);
/// var_dump($tz->getName());
/// try { $tz->__unserialize(['timezone_type' => 3]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(14) "America/Denver"
/// Invalid serialization data for DateTimeZone object
/// ```
pub(crate) fn unserialize(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let hash = array(req, 1)?;
    let object = this(req)?;
    let Some(kind) = from_hash(req, hash)? else {
        return Err(throw(
            req,
            "Error",
            "Invalid serialization data for DateTimeZone object",
        ));
    };
    set_zone(req, object, kind);
    restore_properties(req, object, hash, &[b"timezone_type", b"timezone"])?;
    Ok(Value::null())
}

/// `DateTimeZone::__wakeup(): void`, deprecated since 8.5.
///
/// ```php
/// $tz = new DateTimeZone('UTC');
/// try { $tz->__wakeup(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: Method DateTimeZone::__wakeup() is deprecated since 8.5, this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize() in %s on line 3
/// Invalid serialization data for DateTimeZone object
/// ```
pub(crate) fn wakeup(req: &mut Request) -> Result<Value, Thrown> {
    php_ext::raising::deprecated(
        req,
        "Method DateTimeZone::__wakeup() is deprecated since 8.5, this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize()",
    );
    count_between(req, 0, 0)?;
    let object = this(req)?;
    let mut hash = PhpArray::new(req)?;
    for (name, value) in object.props(req)? {
        req.addref(&value);
        hash.set(req, Key::Str(name), value)?;
    }
    let kind = from_hash(req, hash);
    req.release(Value::array(hash));
    match kind? {
        Some(kind) => {
            set_zone(req, object, kind);
            Ok(Value::null())
        }
        None => Err(throw(
            req,
            "Error",
            "Invalid serialization data for DateTimeZone object",
        )),
    }
}
