//! `DateTimeZone` and the procedural natives over it: `timezone_initialize`
//! over `timelib_parse_zone`, the name, offset, transitions and location
//! of a zone, and its serialised form.

use std::sync::Arc;

use php_ext::abi::{Instance, OrFalse, Path, This};
use php_ext::attr::{php_class, php_function};
use php_ext::call::new_object;
use php_ext::raising::{Level, Thrown, docref, throw_error, throw_exception_ex};
use php_ext::request::Request;
use php_ext::values::{Emalloc, Owner, PhpArray, PhpObject, PhpString};

use crate::classes::DateTimeInterface;
use crate::datetime::restore_properties;
use crate::ffi::{self, ZONETYPE_ABBR, ZONETYPE_ID, ZONETYPE_OFFSET};
use crate::format::format_timestamp;
use crate::lent::{Lent, kept, lent};
use crate::obj::{
    AbbrZone, TIMEZONE, TzKind, TzObj, class_id, lent_time, rows_array, set_str, set_string,
    set_tz, std_rows, tz_fields, tz_of,
};
use crate::throwables::Throwables;

/// Why `timezone_initialize` refuses a zone: the sentence php prints for
/// it with `spprintf`, naming the zone as it was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// `Timezone offset is out of range (%s)`.
    OutOfRange,
    /// `Unknown or bad timezone (%s)`.
    Unknown,
}

impl Refusal {
    /// The sentence up to the zone.
    fn head(self) -> &'static [u8] {
        match self {
            Refusal::OutOfRange => b"Timezone offset is out of range (",
            Refusal::Unknown => b"Unknown or bad timezone (",
        }
    }

    /// `spprintf(warning_message, 0, "... (%s)", ZSTR_VAL(tz_zstr))`:
    /// the sentence naming `text`, printed into `spprintf`'s buffer on
    /// the request's heap. The `Err` is the memory limit's fatal.
    fn print(self, req: &mut Request, text: &[u8]) -> Result<Printed, Thrown> {
        let head = self.head();
        let len = head.len() + text.len() + 1;
        let mut buffer = Emalloc::printf_buffer(req, len)?;
        let sentence = head.iter().chain(text).chain(b")");
        for (slot, byte) in buffer.bytes_mut().iter_mut().zip(sentence) {
            *slot = *byte;
        }
        Ok(Printed { buffer, len })
    }
}

/// The `warning_message` `timezone_initialize` hands back: the sentence
/// in `spprintf`'s buffer, which stands until the caller has raised it
/// and frees it, as php's `efree(warning_message)`.
struct Printed {
    buffer: Emalloc,
    len: usize,
}

impl Printed {
    /// The sentence.
    fn text(&self) -> &[u8] {
        self.buffer.bytes().get(..self.len).unwrap_or_default()
    }

    /// `efree(warning_message)`.
    fn free(self, req: &mut Request) {
        self.buffer.free(req);
    }
}

/// What `timelib_parse_zone` reads of `text` as `timezone_initialize`
/// judges it: an identifier, an abbreviation or an offset, or why it
/// refuses the text. Nothing is printed here, so a zone it accepts costs
/// no sentence.
fn parse(lent: &mut Lent<'_>, text: &[u8]) -> Result<TzKind, Refusal> {
    let read = ffi::parse_zone(lent, text);
    #[allow(clippy::cast_possible_truncation)]
    let z = i64::from(read.offset as i32);
    if read.offset >= 100 * 60 * 60 || read.offset <= -100 * 60 * 60 {
        return Err(Refusal::OutOfRange);
    }
    if read.not_found || read.read < text.len() {
        return Err(Refusal::Unknown);
    }
    match read.zone_type {
        ZONETYPE_ID => read.zone.map(TzKind::Id).ok_or(Refusal::Unknown),
        ZONETYPE_OFFSET => Ok(TzKind::Offset(z)),
        // `set_timezone_from_timelib_time` copies the parse's abbreviation
        // before `timelib_free(dummy_t.tz_abbr)` frees it with `read`.
        ZONETYPE_ABBR => {
            let abbr = read.abbr.as_ref().map_or(&[][..], ffi::Abbr::bytes);
            Ok(TzKind::Abbr(Arc::new(AbbrZone::new(
                lent, z, read.dst, abbr,
            ))))
        }
        _ => Err(Refusal::Unknown),
    }
}

/// `timezone_initialize(tzobj, tz, &warning_message)`: the zone `text`
/// names set on `object`, or the sentence printed for one it refuses,
/// which `timezone_open` and `DateTimeZone::__construct` raise each in
/// its own way. The `Err` is the memory limit's fatal.
fn initialize(
    req: &mut Request,
    object: PhpObject,
    text: &[u8],
) -> Result<Result<(), Printed>, Thrown> {
    match lent(req, |l| parse(l, text))? {
        Ok(kind) => set_zone(req, object, kind).map(Ok),
        Err(refusal) => refusal.print(req, text).map(Err),
    }
}

/// The zone of a new object of the class's.
fn set_zone(req: &mut Request, object: PhpObject, kind: TzKind) -> Result<(), Thrown> {
    set_tz(req, object, Some(kind))
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
///
/// The object is made before the zone is read, as `php_date_instantiate`
/// makes it, so timelib's transient copy of the name peaks over it; a
/// name it refuses has its warning printed into `spprintf`'s buffer,
/// which stands while the warning is raised and the object is released
/// after (#2481):
///
/// ```php
/// function m($f) { memory_reset_peak_usage(); $u = memory_get_usage(); $f(); echo memory_get_peak_usage() - $u, ' ', memory_get_usage() - $u, "\n"; }
/// $keep = [];
/// m(function () use (&$keep) { $keep[] = timezone_open('Europe/Amsterdam'); });
/// m(function () use (&$keep) { $keep[] = timezone_open('Europe/Amsterdam'); });
/// m(function () { @timezone_open('Mars/Olympus'); });
/// m(function () { @timezone_open('Mars/Olympus'); });
/// ```
/// ```expect
/// 2360 2392
/// 104 80
/// 1656 472
/// 1280 0
/// ```
#[php_function]
fn timezone_open(
    req: &mut Request,
    timezone: Path,
) -> Result<OrFalse<Owner<Instance<DateTimeZone>>>, Thrown> {
    let class = class_id(req, "DateTimeZone")?;
    let object = new_object(req, class)?;
    match initialize(req, *object, timezone.0.as_bytes()) {
        Ok(Ok(())) => Ok(OrFalse::Value(object.into_instance())),
        Ok(Err(printed)) => {
            docref(req, Level::Warning, printed.text());
            printed.free(req);
            object.give_up(req);
            Ok(OrFalse::False)
        }
        Err(thrown) => {
            object.give_up(req);
            Err(thrown)
        }
    }
}

/// `timezone_name_get` and `DateTimeZone::getName`: the name the zone
/// was made with.
fn name_of(req: &mut Request, object: PhpObject) -> Result<Owner<PhpString>, Thrown> {
    let kind = tz_of(req, object)?;
    PhpString::new(req, &kind.name())
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
#[php_function]
fn timezone_name_get(
    req: &mut Request,
    object: Instance<DateTimeZone>,
) -> Result<Owner<PhpString>, Thrown> {
    name_of(req, object.object())
}

/// `timezone_offset_get` and `DateTimeZone::getOffset`: the zone's
/// offset from UTC at the date's instant.
fn offset_of(req: &mut Request, object: PhpObject, date: PhpObject) -> Result<i64, Thrown> {
    let kind = tz_of(req, object)?;
    let offset = lent_time(req, date, |l, time| match &kind {
        TzKind::Id(zone) => i64::from(zone.offset_in(l, time.sse).offset()),
        TzKind::Offset(offset) => *offset,
        TzKind::Abbr(zone) => zone.utc_offset + i64::from(zone.dst) * 3600,
    });
    kind.release(req)?;
    offset
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
#[php_function]
fn timezone_offset_get(
    req: &mut Request,
    object: Instance<DateTimeZone>,
    datetime: Instance<DateTimeInterface>,
) -> Result<i64, Thrown> {
    offset_of(req, object.object(), datetime.object())
}

/// One row of `getTransitions`.
fn transition(
    req: &mut Request,
    out: &mut Owner<PhpArray>,
    ts: i64,
    offset: i64,
    is_dst: bool,
    abbr: &[u8],
) -> Result<(), Thrown> {
    let mut row = PhpArray::new(req)?;
    set_str(req, &mut row, b"ts", Owner::int(ts))?;
    let time = format_timestamp(req, b"x-m-d\\TH:i:sP", ts, false)?;
    set_str(req, &mut row, b"time", time.into_value())?;
    set_str(req, &mut row, b"offset", Owner::int(offset))?;
    set_str(req, &mut row, b"isdst", Owner::bool(is_dst))?;
    set_string(req, &mut row, b"abbr", abbr)?;
    out.append(req, row.into_value())
}

/// `timezone_transitions_get` and `DateTimeZone::getTransitions`.
#[allow(clippy::too_many_lines)]
fn transitions_of(
    req: &mut Request,
    object: PhpObject,
    begin_ts: i64,
    end_ts: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let kind = tz_of(req, object)?;
    let TzKind::Id(zone) = kind else {
        return Ok(OrFalse::False);
    };
    let types = zone.types();
    let trans = zone.transitions();
    let ty = |i: usize| types.get(i).cloned();
    let mut out = PhpArray::new(req)?;
    let nominal = |req: &mut Request, out: &mut Owner<PhpArray>, ts: i64| -> Result<(), Thrown> {
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
                return Ok(OrFalse::Value(out));
            }
            if let Some(t) = ty(i) {
                transition(req, &mut out, ts, i64::from(t.offset), t.is_dst, &t.abbr)?;
            }
            begin += 1;
        }
    } else if trans.is_empty() {
        nominal(req, &mut out, begin_ts)?;
    } else if posix {
        // `add_from_tto(tto, timestamp_begin)` reads timelib's offset,
        // which php frees after the row is added.
        let o = lent(req, |l| zone.offset_in(l, begin_ts))?;
        kept(req, o, |req, o| {
            let (offset, is_dst) = (i64::from(o.offset()), o.is_dst());
            transition(req, &mut out, begin_ts, offset, is_dst, o.abbr())
        })?;
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
                    return Ok(OrFalse::Value(out));
                }
                if let Some(t) = ty(i) {
                    transition(req, &mut out, ts, i64::from(t.offset), t.is_dst, &t.abbr)?;
                }
            }
        }
    }
    Ok(OrFalse::Value(out))
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
///
/// Each row's `time` is `php_format_date`'s string, untrimmed, and a row
/// read off a zone's POSIX rule is made while timelib's offset stands:
///
/// ```php
/// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
/// $u = new DateTimeZone('UTC'); $p = new DateTimeZone('EST5EDT');
/// pk(fn () => $u->getTransitions(0, 0));
/// pk(fn () => $p->getTransitions(2000000000, 2000000000));
/// ```
/// ```expect
/// 1040 1072
/// 1088 1040
/// ```
#[php_function]
fn timezone_transitions_get(
    req: &mut Request,
    object: Instance<DateTimeZone>,
    #[php(default = PHP_INT_MIN)] timestampBegin: i64,
    #[php(default = 2147483647)] timestampEnd: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    transitions_of(req, object.object(), timestampBegin, timestampEnd)
}

/// `timelib_unixtime2date(ts, &y, &m, &d)`.
fn year_of(ts: i64) -> (i64, i64, i64) {
    ffi::unixtime2date(ts)
}

/// `timezone_location_get` and `DateTimeZone::getLocation`.
fn location_of(req: &mut Request, object: PhpObject) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let kind = tz_of(req, object)?;
    let TzKind::Id(zone) = kind else {
        return Ok(OrFalse::False);
    };
    let loc = zone.location();
    let mut out = PhpArray::new(req)?;
    set_string(req, &mut out, b"country_code", &loc.country_code)?;
    set_str(req, &mut out, b"latitude", Owner::float(loc.latitude))?;
    set_str(req, &mut out, b"longitude", Owner::float(loc.longitude))?;
    set_string(req, &mut out, b"comments", &loc.comments)?;
    Ok(OrFalse::Value(out))
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
#[php_function]
fn timezone_location_get(
    req: &mut Request,
    object: Instance<DateTimeZone>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    location_of(req, object.object())
}

/// `php_date_timezone_initialize_from_hash`.
fn from_hash(req: &mut Request, hash: PhpArray) -> Result<Option<TzKind>, Thrown> {
    let Some(text) = hash_zone(hash) else {
        return Ok(None);
    };
    lent(req, |l| parse(l, &text).ok())
}

/// The `timezone` a serialised zone's `timezone_type` admits, none for
/// a hash `php_date_timezone_initialize_from_hash` refuses.
fn hash_zone(hash: PhpArray) -> Option<Vec<u8>> {
    let find = |name: &[u8]| hash.find(name).map(|v| v.deref());
    let zone_type = find(b"timezone_type")?;
    let zone = find(b"timezone")?;
    let zone_type = zone_type.as_int()?;
    if !(i64::from(ZONETYPE_OFFSET)..=i64::from(ZONETYPE_ID)).contains(&zone_type) {
        return None;
    }
    let text = zone.as_str_bytes()?;
    if text.contains(&0) {
        return None;
    }
    Some(text.to_vec())
}

/// `class DateTimeZone`: its fourteen groups, the values of
/// `PHP_DATE_TIMEZONE_GROUP_*` and `PHP_DATE_TIMEZONE_PER_COUNTRY`, and
/// its eleven methods, over a `php_timezone_obj` payload.
///
/// ```php
/// var_dump(DateTimeZone::AFRICA, DateTimeZone::UTC, DateTimeZone::ALL, DateTimeZone::ALL_WITH_BC, DateTimeZone::PER_COUNTRY);
/// ```
/// ```expect
/// int(1)
/// int(1024)
/// int(2047)
/// int(4095)
/// int(4096)
/// ```
#[php_class(size = 88, payload = TzObj, handlers = TIMEZONE)]
pub(crate) struct DateTimeZone;

/// The methods the procedural natives alias, called as methods.
///
/// ```php
/// $tz = new DateTimeZone('Europe/Amsterdam');
/// var_dump($tz->getName(), $tz->getOffset(new DateTime('2020-07-01 UTC')), count($tz->getTransitions(1700000000, 1735689600)), $tz->getLocation()['country_code']);
/// ```
/// ```expect
/// string(16) "Europe/Amsterdam"
/// int(7200)
/// int(3)
/// string(2) "NL"
/// ```
#[php_class]
impl DateTimeZone {
    const AFRICA: i64 = 0x0001;
    const AMERICA: i64 = 0x0002;
    const ANTARCTICA: i64 = 0x0004;
    const ARCTIC: i64 = 0x0008;
    const ASIA: i64 = 0x0010;
    const ATLANTIC: i64 = 0x0020;
    const AUSTRALIA: i64 = 0x0040;
    const EUROPE: i64 = 0x0080;
    const INDIAN: i64 = 0x0100;
    const PACIFIC: i64 = 0x0200;
    const UTC: i64 = 0x0400;
    const ALL: i64 = 0x07FF;
    pub(crate) const ALL_WITH_BC: i64 = 0x0FFF;
    pub(crate) const PER_COUNTRY: i64 = 0x1000;

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
    ///
    /// An abbreviation zone holds the copy of its abbreviation on the
    /// request's heap, as `timelib_strdup` makes it; an identifier or an
    /// offset holds none there.
    ///
    /// ```php
    /// foreach (['EST', 'CEST', 'Europe/Amsterdam', '+01:00'] as $n) { $z = new DateTimeZone($n); $c = clone $z; unset($z, $c); }
    /// foreach (['EST', 'CEST', 'Europe/Amsterdam', '+01:00'] as $n) { $m = memory_get_usage(); $z = new DateTimeZone($n); $c = clone $z; echo $n, ' ', memory_get_usage() - $m, "\n"; unset($z, $c); }
    /// $d = new DateTime('2020-01-01 PST'); $m = memory_get_usage(); $z = $d->getTimezone(); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// EST 224
    /// CEST 192
    /// Europe/Amsterdam 160
    /// +01:00 160
    /// 96
    /// ```
    ///
    /// `timelib_parse_zone` reads into a time on the stack, as php's
    /// `dummy_t` is, and the zone out of the request's cache, so a second
    /// construction peaks where php's does (ADR-0014):
    ///
    /// ```php
    /// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
    /// pk(fn () => new DateTimeZone('Europe/Amsterdam'));
    /// ```
    /// ```expect
    /// 104 112
    /// ```
    ///
    /// A name it refuses has its sentence printed into `spprintf`'s
    /// buffer, which stands while the `DateInvalidTimeZoneException` is
    /// made:
    ///
    /// ```php
    /// function m($f) { memory_reset_peak_usage(); $u = memory_get_usage(); $f(); echo memory_get_peak_usage() - $u, ' ', memory_get_usage() - $u, "\n"; }
    /// m(function () { try { new DateTimeZone('Mars/Olympus'); } catch (DateInvalidTimeZoneException $e) {} });
    /// m(function () { try { new DateTimeZone('Mars/Olympus'); } catch (DateInvalidTimeZoneException $e) {} });
    /// ```
    /// ```expect
    /// 2816 88
    /// 2760 0
    /// ```
    ///
    /// What the request's heap counts for a zone (design §5.13): each name
    /// parsed once into the request's cache, a name again taking the object
    /// alone and its release giving that back, a name in another case
    /// taking storage of its own and keeping the name as written, an
    /// unknown name refused each time it is asked for, and an
    /// abbreviation's zone, and its clone, the object alone.
    ///
    /// ```php
    /// $names = ['Europe/Amsterdam', 'America/New_York', 'Asia/Tokyo', 'Europe/London', 'Australia/Sydney', 'Africa/Cairo', 'America/Chicago', 'Europe/Paris', 'Asia/Kolkata', 'Pacific/Auckland'];
    /// $zones = [];
    /// foreach ($names as $name) {
    ///     $before = memory_get_usage();
    ///     $zones[] = new DateTimeZone($name);
    ///     echo $name, ':', memory_get_usage() - $before, "\n";
    /// }
    /// $before = memory_get_usage();
    /// $again = new DateTimeZone('Europe/Amsterdam');
    /// echo 'repeat:', memory_get_usage() - $before, "\n";
    /// $before = memory_get_usage();
    /// unset($again);
    /// echo 'release:', memory_get_usage() - $before, "\n";
    /// $before = memory_get_usage();
    /// $alias = new DateTimeZone('europe/amsterdam');
    /// echo 'alias:', memory_get_usage() - $before, ':', $alias->getName(), "\n";
    /// for ($j = 0; $j < 2; $j++) {
    ///     try { new DateTimeZone('Mars/Olympus'); }
    ///     catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// $before = memory_get_usage();
    /// $abbreviation = new DateTimeZone('EST');
    /// $after = memory_get_usage();
    /// $cloned = clone $abbreviation;
    /// echo 'abbr:', $after - $before, ':clone:', memory_get_usage() - $after, "\n";
    /// $before = memory_get_usage();
    /// unset($abbreviation, $cloned);
    /// echo 'abbr-release:', memory_get_usage() - $before, "\n";
    /// ```
    /// ```expect
    /// Europe/Amsterdam:2392
    /// America/New_York:2440
    /// Asia/Tokyo:656
    /// Europe/London:2136
    /// Australia/Sydney:1536
    /// Africa/Cairo:1792
    /// America/Chicago:2424
    /// Europe/Paris:1744
    /// Asia/Kolkata:1168
    /// Pacific/Auckland:1584
    /// repeat:80
    /// release:-80
    /// alias:1768:europe/amsterdam
    /// DateTimeZone::__construct(): Unknown or bad timezone (Mars/Olympus)
    /// DateTimeZone::__construct(): Unknown or bad timezone (Mars/Olympus)
    /// abbr:96:clone:96
    /// abbr-release:-192
    /// ```
    fn __construct(req: &mut Request, this: This<Self>, timezone: Path) -> Result<(), Thrown> {
        let Err(printed) = initialize(req, this.object(), timezone.0.as_bytes())? else {
            return Ok(());
        };
        let message = format!(
            "DateTimeZone::__construct(): {}",
            String::from_utf8_lossy(printed.text())
        );
        let thrown = match Throwables::of(req) {
            Ok(ce) => throw_exception_ex(req, ce.date_invalid_time_zone_exception, &message),
            Err(fatal) => fatal,
        };
        printed.free(req);
        Err(thrown)
    }

    /// `DateTimeZone::getName(): string`, `timezone_name_get`'s body.
    #[php(tentative)]
    fn getName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        name_of(req, this.object())
    }

    /// `DateTimeZone::getOffset(DateTimeInterface $datetime): int`,
    /// `timezone_offset_get`'s body.
    #[php(tentative)]
    fn getOffset(
        req: &mut Request,
        this: This<Self>,
        datetime: Instance<DateTimeInterface>,
    ) -> Result<i64, Thrown> {
        offset_of(req, this.object(), datetime.object())
    }

    /// `DateTimeZone::getTransitions(...)`, `timezone_transitions_get`'s
    /// body.
    #[php(tentative)]
    fn getTransitions(
        req: &mut Request,
        this: This<Self>,
        #[php(default = PHP_INT_MIN)] timestampBegin: i64,
        #[php(default = 2147483647)] timestampEnd: i64,
    ) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
        transitions_of(req, this.object(), timestampBegin, timestampEnd)
    }

    /// `DateTimeZone::getLocation(): array|false`,
    /// `timezone_location_get`'s body.
    #[php(tentative)]
    fn getLocation(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
        location_of(req, this.object())
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
    #[php(tentative)]
    fn listAbbreviations(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
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
    #[php(tentative)]
    fn listIdentifiers(
        req: &mut Request,
        #[php(default = DateTimeZone::ALL)] timezoneGroup: i64,
        #[php(default = null)] countryCode: Option<PhpString>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        crate::tzdb::identifiers(req, timezoneGroup, countryCode)
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
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let object = this.object();
        let kind = tz_of(req, object)?;
        let own = std_rows(req, object)?;
        let fields = tz_fields(req, &kind)?;
        rows_array(req, &own, fields, true)
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
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        let (object, hash) = (this.object(), data);
        let Some(kind) = from_hash(req, hash)? else {
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
                "Invalid serialization data for DateTimeZone object",
            ));
        };
        set_zone(req, object, kind)?;
        restore_properties(req, object, hash, &[b"timezone_type", b"timezone"])
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
    #[php(
        tentative,
        deprecated(
            since = "8.5",
            message = "this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize()"
        )
    )]
    fn __wakeup(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let object = this.object();
        let mut hash = PhpArray::new(req)?;
        for (key, value) in object.props(req)? {
            hash.set(req, Owner::take(req, key), Owner::take(req, value))?;
        }
        let kind = from_hash(req, *hash);
        hash.give_up(req);
        if let Some(kind) = kind? {
            set_zone(req, object, kind)
        } else {
            let ce = Throwables::of(req)?;
            Err(throw_error(
                req,
                ce.error,
                "Invalid serialization data for DateTimeZone object",
            ))
        }
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
    #[php(tentative)]
    fn __set_state(
        req: &mut Request,
        array: PhpArray,
    ) -> Result<Owner<Instance<DateTimeZone>>, Thrown> {
        let hash = array;
        let class = class_id(req, "DateTimeZone")?;
        let object = new_object(req, class)?;
        let Some(kind) = from_hash(req, hash)? else {
            object.give_up(req);
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
                "Invalid serialization data for DateTimeZone object",
            ));
        };
        set_zone(req, *object, kind)?;
        Ok(object.into_instance())
    }
}
