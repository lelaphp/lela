//! `date()` and its neighbours: `date_format`, the body `date()`,
//! `gmdate()` and `DateTimeInterface::format` share, `php_idate`, and the
//! natives that answer the time as numbers, `time`, `mktime`,
//! `gmmktime`, `checkdate`, `localtime`, `getdate` and the two
//! `strftime`s.

use std::time::{SystemTime, UNIX_EPOCH};

use php_ext::abi::OrFalse;
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{FreshString, Layout, Owner, PhpArray, PhpString, SmartStr};

use crate::ffi::{self, Time, TimeOffset, ZONETYPE_ABBR, ZONETYPE_ID, ZONETYPE_OFFSET};
use crate::lent::{Lent, kept, lent, release};
use crate::obj::{set_str, set_string};
use crate::zone::default_zone;

pub(crate) const MON_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
pub(crate) const MON_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
pub(crate) const DAY_FULL: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
pub(crate) const DAY_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// `php_time()`: the seconds of `gettimeofday`.
pub(super) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `timelib_is_leap(y)`.
pub(super) fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn month_name(names: &[&'static str; 12], m: i64) -> &'static str {
    usize::try_from(m - 1)
        .ok()
        .and_then(|i| names.get(i))
        .copied()
        .unwrap_or("")
}

pub(super) fn full_month(m: i64) -> &'static str {
    month_name(&MON_FULL, m)
}

pub(super) fn short_month(m: i64) -> &'static str {
    month_name(&MON_SHORT, m)
}

/// `php_date_full_day_name` and `php_date_short_day_name`.
fn day_name(names: &[&'static str; 7], y: i64, m: i64, d: i64) -> &'static str {
    usize::try_from(ffi::day_of_week(y, m, d))
        .ok()
        .and_then(|i| names.get(i))
        .copied()
        .unwrap_or("Unknown")
}

pub(super) fn full_day(y: i64, m: i64, d: i64) -> &'static str {
    day_name(&DAY_FULL, y, m, d)
}

/// `english_suffix`.
fn english_suffix(n: i64) -> &'static str {
    if (10..=19).contains(&n) {
        return "th";
    }
    match n % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    }
}

/// C's `(int)` of a `timelib_sll`: the low 32 bits.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn c_int(n: i64) -> i64 {
    i64::from(n as i32)
}

/// The offset `date_format` makes for a local time, timelib's own and
/// held until the string is made: the abbreviation's, the fixed
/// offset's under `GMT±hhmm`, or the zone's at the time.
fn local_offset(lent: &Lent<'_>, t: &Time) -> TimeOffset {
    match t.zone_type {
        ZONETYPE_ABBR => TimeOffset::of_abbr(lent, t.z + t.dst * 3600, t.dst != 0, t.tz_abbr()),
        ZONETYPE_OFFSET => TimeOffset::of_fixed(lent, t.z),
        ZONETYPE_ID => match t.zone() {
            Some(zone) => zone.offset_in(lent, t.sse),
            None => TimeOffset::new(lent),
        },
        // "Shouldn't happen, but code defensively."
        _ => TimeOffset::new(lent),
    }
}

/// `date_format(format, len, t, localtime)`: the format read byte by
/// byte, each letter replaced by what `php_date.c` prints for it, built
/// in a `smart_str` on the request's heap and handed over as php's
/// `return string.s` does, untrimmed, so the string stands in the
/// buffer's first block, 256 bytes, or the page it grew to; the empty
/// format is `ZSTR_EMPTY_ALLOC()`, which allocates nothing. The `Err` is
/// the memory limit's fatal.
pub(super) fn date_format(
    req: &mut Request,
    format: &[u8],
    t: &Time,
    localtime: bool,
) -> Result<Owner<PhpString>, Thrown> {
    if format.is_empty() {
        return PhpString::new(req, b"");
    }
    if !localtime {
        return format_built(req, format, t, None);
    }
    // php frees the offset after `smart_str_0`, so it stands beside the
    // buffer while the buffer grows.
    let offset = lent(req, |l| local_offset(l, t))?;
    kept(req, offset, |req, offset| {
        format_built(req, format, t, Some(offset))
    })
}

/// [`date_format`]'s `smart_str`, built and handed over, or freed where
/// the memory limit stopped it.
fn format_built(
    req: &mut Request,
    format: &[u8],
    t: &Time,
    offset: Option<&TimeOffset>,
) -> Result<Owner<PhpString>, Thrown> {
    let mut out = SmartStr::default();
    match format_into(req, &mut out, format, t, offset) {
        Ok(()) => out.finish(req),
        Err(thrown) => {
            out.free(req);
            Err(thrown)
        }
    }
}

/// [`date_format`]'s loop: each letter's piece appended to `out` as
/// `smart_str_appendl` appends it, `offset` the local time's where the
/// format is local.
#[allow(clippy::too_many_lines)]
fn format_into(
    req: &mut Request,
    out: &mut SmartStr,
    format: &[u8],
    t: &Time,
    offset: Option<&TimeOffset>,
) -> Result<(), Thrown> {
    let localtime = offset.is_some();
    let off = offset.map_or(0, TimeOffset::offset);
    let sign = if localtime && off < 0 { '-' } else { '+' };
    let oh = (off / 3600).abs();
    let om = ((off % 3600) / 60).abs();
    let mut week: Option<(i64, i64)> = None;
    let mut i = 0;
    while i < format.len() {
        let c = format[i];
        let piece: String = match c {
            b'd' => format!("{:02}", c_int(t.d)),
            b'D' => day_name(&DAY_SHORT, t.y, t.m, t.d).to_owned(),
            b'j' => c_int(t.d).to_string(),
            b'l' => full_day(t.y, t.m, t.d).to_owned(),
            b'S' => english_suffix(t.d).to_owned(),
            b'w' => c_int(ffi::day_of_week(t.y, t.m, t.d)).to_string(),
            b'N' => c_int(ffi::iso_day_of_week(t.y, t.m, t.d)).to_string(),
            b'z' => c_int(ffi::day_of_year(t.y, t.m, t.d)).to_string(),
            b'W' => {
                let (w, _) = *week.get_or_insert_with(|| ffi::isoweek_from_date(t.y, t.m, t.d));
                format!("{:02}", c_int(w))
            }
            b'o' => {
                let (_, y) = *week.get_or_insert_with(|| ffi::isoweek_from_date(t.y, t.m, t.d));
                y.to_string()
            }
            b'F' => full_month(t.m).to_owned(),
            b'm' => format!("{:02}", c_int(t.m)),
            b'M' => short_month(t.m).to_owned(),
            b'n' => c_int(t.m).to_string(),
            b't' => c_int(ffi::days_in_month(t.y, t.m)).to_string(),
            b'L' => i32::from(is_leap(c_int(t.y))).to_string(),
            b'y' => format!("{:02}", c_int(t.y % 100)),
            b'Y' => format!(
                "{}{:04}",
                if t.y < 0 { "-" } else { "" },
                t.y.unsigned_abs()
            ),
            b'x' => format!(
                "{}{:04}",
                if t.y < 0 {
                    "-"
                } else if t.y >= 10000 {
                    "+"
                } else {
                    ""
                },
                t.y.unsigned_abs()
            ),
            b'X' => format!(
                "{}{:04}",
                if t.y < 0 { "-" } else { "+" },
                t.y.unsigned_abs()
            ),
            b'a' => (if t.h >= 12 { "pm" } else { "am" }).to_owned(),
            b'A' => (if t.h >= 12 { "PM" } else { "AM" }).to_owned(),
            b'B' => format!("{:03}", swatch(t.sse)),
            b'g' => twelve(t.h).to_string(),
            b'G' => c_int(t.h).to_string(),
            b'h' => format!("{:02}", twelve(t.h)),
            b'H' => format!("{:02}", c_int(t.h)),
            b'i' => format!("{:02}", c_int(t.i)),
            b's' => format!("{:02}", c_int(t.s)),
            b'u' => format!("{:06}", c_int(t.us)),
            b'v' => format!("{:03}", c_int(t.us / 1000)),
            b'I' => i32::from(offset.is_some_and(TimeOffset::is_dst)).to_string(),
            b'p' | b'P' | b'O' => {
                let abbr = offset.map_or(&[][..], TimeOffset::abbr);
                if c == b'p' && (!localtime || matches!(abbr, b"UTC" | b"Z" | b"GMT+0000")) {
                    "Z".to_owned()
                } else {
                    let colon = if c == b'O' { "" } else { ":" };
                    format!("{sign}{oh:02}{colon}{om:02}")
                }
            }
            b'T' => match offset {
                Some(o) => String::from_utf8_lossy(o.abbr()).into_owned(),
                None => "GMT".to_owned(),
            },
            b'e' => {
                if localtime {
                    match t.zone_type {
                        ZONETYPE_ID => t
                            .zone()
                            .map(|z| String::from_utf8_lossy(z.name()).into_owned())
                            .unwrap_or_default(),
                        ZONETYPE_ABBR => offset
                            .map(|o| String::from_utf8_lossy(o.abbr()).into_owned())
                            .unwrap_or_default(),
                        ZONETYPE_OFFSET => {
                            let secs = off % 60;
                            if secs == 0 {
                                format!("{}{oh:02}:{om:02}", if off < 0 { '-' } else { '+' })
                            } else {
                                format!(
                                    "{}{oh:02}:{om:02}:{:02}",
                                    if off < 0 { '-' } else { '+' },
                                    secs.abs()
                                )
                            }
                        }
                        _ => String::new(),
                    }
                } else {
                    "UTC".to_owned()
                }
            }
            b'Z' => off.to_string(),
            b'c' => format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{sign}{oh:02}:{om:02}",
                t.y,
                c_int(t.m),
                c_int(t.d),
                c_int(t.h),
                c_int(t.i),
                c_int(t.s)
            ),
            b'r' => format!(
                "{:>3}, {:02} {:>3} {:04} {:02}:{:02}:{:02} {sign}{oh:02}{om:02}",
                day_name(&DAY_SHORT, t.y, t.m, t.d),
                c_int(t.d),
                short_month(t.m),
                t.y,
                c_int(t.h),
                c_int(t.i),
                c_int(t.s)
            ),
            b'U' => t.sse.to_string(),
            b'\\' => {
                i += 1;
                // php reads the byte after the backslash, and past the
                // last byte the string's terminating NUL.
                out.append(req, &[format.get(i).copied().unwrap_or(0)])?;
                i += 1;
                continue;
            }
            other => {
                out.append(req, &[other])?;
                i += 1;
                continue;
            }
        };
        out.append(req, piece.as_bytes())?;
        i += 1;
    }
    Ok(())
}

/// `(t->h % 12) ? (int) t->h % 12 : 12`.
fn twelve(h: i64) -> i64 {
    if h % 12 == 0 { 12 } else { c_int(h) % 12 }
}

/// The Swatch beat `date_format` and `php_idate` compute over a C `int`.
#[allow(clippy::cast_possible_truncation)]
fn swatch(sse: i64) -> i32 {
    let mut beat = (((sse % 86400) + 3600) * 10) as i32;
    if beat < 0 {
        beat += 864_000;
    }
    (beat / 864) % 1000
}

/// `php_format_date(format, len, ts, localtime)`: the time `ts` in
/// `format`, in the default zone where `localtime`, as [`date_format`]
/// hands it over. The `Err` is the memory limit's fatal or the default
/// zone's error.
pub(super) fn format_timestamp(
    req: &mut Request,
    format: &[u8],
    ts: i64,
    localtime: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let zone = if localtime {
        Some(default_zone(req)?)
    } else {
        None
    };
    let t = lent(req, |l| {
        let mut t = Time::new(l);
        match zone {
            Some(zone) => {
                t.set_zone_id(zone);
                t.unixtime2local(l, ts);
            }
            None => t.unixtime2gmt(l, ts),
        }
        t
    })?;
    kept(req, t, |req, t| date_format(req, format, t, localtime))
}

/// [`format_timestamp`] as the bytes main's and ext/standard's callers of
/// `php_format_date` read before they free the string
/// ([`php_ext::request::FormatDate`]).
pub(super) fn format_date(
    req: &mut Request,
    format: &[u8],
    ts: i64,
    localtime: bool,
) -> Result<Vec<u8>, Thrown> {
    let text = format_timestamp(req, format, ts, localtime)?;
    let bytes = text.as_bytes().to_vec();
    text.give_up(req);
    Ok(bytes)
}

/// `php_date(INTERNAL_FUNCTION_PARAM_PASSTHRU, localtime)`.
fn php_date(
    req: &mut Request,
    format: PhpString,
    timestamp: Option<i64>,
    localtime: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let ts = timestamp.unwrap_or_else(now);
    format_timestamp(req, format.as_bytes(), ts, localtime)
}

/// `date(string $format, ?int $timestamp = null): string`, in the
/// default zone.
///
/// ```php
/// date_default_timezone_set('Europe/Amsterdam');
/// $t = 1700000000;
/// echo date('d D j l N S w z W F m M n t L o Y y a A B g G h H i s u v e I O P p T Z U', $t), "\n";
/// echo date('c | r | \Y\-\m | x X', $t), "\n";
/// echo date(DATE_ATOM, 1719835200), " ", date('I T', 1719835200), "\n";
/// echo date('jS jS jS jS', 86400 * 2), date('jS', 86400 * 11), date('jS', 86400 * 21), "\n";
/// var_dump(date(''), bin2hex(date('\\')), bin2hex(date('a\\', $t)));
/// ```
/// ```expect
/// 14 Tue 14 Tuesday 2 th 2 317 46 November 11 Nov 11 30 0 2023 2023 23 pm PM 967 11 23 11 23 13 20 000000 000 Europe/Amsterdam 0 +0100 +01:00 +01:00 CET 3600 1700000000
/// 2023-11-14T23:13:20+01:00 | Tue, 14 Nov 2023 23:13:20 +0100 | Y-m | 2023 +2023
/// 2024-07-01T14:00:00+02:00 1 CEST
/// 3rd 3rd 3rd 3rd12th22nd
/// string(0) ""
/// string(2) "00"
/// string(6) "706d00"
/// ```
///
/// The string is `date_format`'s `smart_str`, untrimmed, built while
/// timelib's offset for the local time stands, and the empty format's
/// is the interned empty string:
///
/// ```php
/// date('Y', 0);
/// $m = memory_get_usage(); $b = date('Y-m-d', 0); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $c = date(''); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); memory_reset_peak_usage(); $l = date('D, d M Y H:i:s T e', 0); echo memory_get_peak_usage() - $m, ' ', memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); memory_reset_peak_usage(); $n = idate('Z', 0); echo memory_get_peak_usage() - $m, ' ', memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 256
/// 0
/// 576 256
/// 320 0
/// ```
#[php_function]
fn date(
    req: &mut Request,
    format: PhpString,
    #[php(default = null)] timestamp: Option<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    php_date(req, format, timestamp, true)
}

/// `gmdate(string $format, ?int $timestamp = null): string`, in UTC.
///
/// ```php
/// date_default_timezone_set('Asia/Tokyo');
/// echo gmdate('Y-m-d H:i:s e T O P p Z I', 1700000000), "\n";
/// echo gmdate(DATE_RFC2822, -86400 * 365), "\n";
/// echo gmdate('Y-m-d', -62167219200), " ", gmdate('Y x X', 253402300800), "\n";
/// ```
/// ```expect
/// 2023-11-14 22:13:20 UTC GMT +0000 +00:00 Z 0 0
/// Wed, 01 Jan 1969 00:00:00 +0000
/// 0000-01-01 10000 +10000 +10000
/// ```
#[php_function]
fn gmdate(
    req: &mut Request,
    format: PhpString,
    #[php(default = null)] timestamp: Option<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    php_date(req, format, timestamp, false)
}

/// `php_idate(format, ts, localtime)`: one field of the local time as a
/// C `int`, `-1` for a letter it has no field for.
fn php_idate(req: &mut Request, format: u8, ts: i64) -> Result<i64, Thrown> {
    let zone = default_zone(req)?;
    lent(req, |l| {
        let mut t = Time::new(l);
        t.set_zone_id(zone);
        t.unixtime2local(l, ts);
        idate_field(l, format, &t)
    })
}

/// The field `php_idate` reads off the local time for `format`.
fn idate_field(lent: &Lent<'_>, format: u8, t: &Time) -> i64 {
    let offset = local_offset(lent, t);
    let (week, iso_year) = ffi::isoweek_from_date(t.y, t.m, t.d);
    match format {
        b'd' | b'j' => c_int(t.d),
        b'N' => c_int(ffi::iso_day_of_week(t.y, t.m, t.d)),
        b'w' => c_int(ffi::day_of_week(t.y, t.m, t.d)),
        b'z' => c_int(ffi::day_of_year(t.y, t.m, t.d)),
        b'W' => c_int(week),
        b'm' | b'n' => c_int(t.m),
        b't' => c_int(ffi::days_in_month(t.y, t.m)),
        b'L' => i64::from(is_leap(c_int(t.y))),
        b'y' => c_int(t.y % 100),
        b'Y' => c_int(t.y),
        b'o' => c_int(iso_year),
        b'B' => i64::from(swatch(t.sse)),
        b'g' | b'h' => twelve(t.h),
        b'H' | b'G' => c_int(t.h),
        b'i' => c_int(t.i),
        b's' => c_int(t.s),
        b'I' => i64::from(offset.is_dst()),
        b'Z' => i64::from(offset.offset()),
        b'U' => c_int(t.sse),
        _ => -1,
    }
}

/// `idate(string $format, ?int $timestamp = null): int|false`: one
/// letter, and its field as an integer.
///
/// ```php
/// date_default_timezone_set('America/New_York');
/// $t = 1700000000;
/// foreach (str_split('BdhHiILmstUwWyYzZNojnGgo') as $f) echo ' ', $f, '=', idate($f, $t);
/// echo "\n";
/// var_dump(idate('yy', $t), idate('x', $t), idate('y', mktime(0, 0, 0, 1, 1, -1)));
/// ```
/// ```expect
/// B=967 d=14 h=5 H=17 i=13 I=0 L=0 m=11 s=20 t=30 U=1700000000 w=2 W=46 y=23 Y=2023 z=317 Z=-18000 N=2 o=2023 j=14 n=11 G=17 g=5 o=2023
///
/// Warning: idate(): idate format is one char in %s on line 6
///
/// Warning: idate(): Unrecognized date format token in %s on line 6
///
/// Warning: idate(): Unrecognized date format token in %s on line 6
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn idate(
    req: &mut Request,
    format: PhpString,
    #[php(default = null)] timestamp: Option<i64>,
) -> Result<OrFalse<i64>, Thrown> {
    let format = format.as_bytes();
    let ts = timestamp;
    if format.len() != 1 {
        docref(req, Level::Warning, "idate format is one char");
        return Ok(OrFalse::False);
    }
    let ts = ts.unwrap_or_else(now);
    let n = php_idate(req, format[0], ts)?;
    if n == -1 {
        docref(req, Level::Warning, "Unrecognized date format token");
        return Ok(OrFalse::False);
    }
    Ok(OrFalse::Value(n))
}

/// `php_mktime(INTERNAL_FUNCTION_PARAM_PASSTHRU, gmt)`: the current time
/// with each given field written over it, a two-digit year widened as
/// php widens it.
fn php_mktime(
    req: &mut Request,
    hour: i64,
    fields: [Option<i64>; 5],
    gmt: bool,
) -> Result<OrFalse<i64>, Thrown> {
    let zone = if gmt { None } else { Some(default_zone(req)?) };
    let ts = lent(req, |l| mktime_of(l, zone.as_ref(), hour, fields))?;
    let Some(ts) = ts else {
        docref(req, Level::Warning, "Epoch doesn't fit in a PHP integer");
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(ts))
}

/// `php_mktime`'s time: now, in `zone` or in UTC, with each given field
/// written over it, and its epoch where it fits.
fn mktime_of(
    lent: &Lent<'_>,
    zone: Option<&std::sync::Arc<ffi::Zone>>,
    hour: i64,
    fields: [Option<i64>; 5],
) -> Option<i64> {
    let mut now_t = Time::new(lent);
    match zone {
        None => now_t.unixtime2gmt(lent, now()),
        Some(zone) => {
            now_t.set_zone_id(zone.clone());
            now_t.unixtime2local(lent, now());
        }
    }
    now_t.h = hour;
    let [min, sec, mon, day, year] = fields;
    if let Some(v) = min {
        now_t.i = v;
    }
    if let Some(v) = sec {
        now_t.s = v;
    }
    if let Some(v) = mon {
        now_t.m = v;
    }
    if let Some(v) = day {
        now_t.d = v;
    }
    if let Some(mut y) = year {
        if (0..70).contains(&y) {
            y += 2000;
        } else if (70..=100).contains(&y) {
            y += 1900;
        }
        now_t.y = y;
    }
    now_t.update_ts(lent, zone);
    now_t.date_to_int()
}

/// `mktime(int $hour, ?int $minute = null, ?int $second = null, ?int
/// $month = null, ?int $day = null, ?int $year = null): int|false`.
///
/// ```php
/// date_default_timezone_set('Europe/London');
/// var_dump(mktime(12, 30, 0, 6, 15, 2023), mktime(0, 0, 0, 1, 1, 70), mktime(0, 0, 0, 1, 1, 69), mktime(0, 0, 0, 1, 1, 100), mktime(0, 0, 0, 13, 1, 2023), mktime(25, 0, 0, 2, 30, 2024), mktime(1, 30, 0, 3, 26, 2023));
/// var_dump(mktime(0, 0, 0, 1, 1, PHP_INT_MAX));
/// ```
/// ```expect
/// int(1686828600)
/// int(-3600)
/// int(3124224000)
/// int(946684800)
/// int(1704067200)
/// int(1709341200)
/// int(1679794200)
/// int(-62198793141)
/// ```
#[php_function]
fn mktime(
    req: &mut Request,
    hour: i64,
    #[php(default = null)] minute: Option<i64>,
    #[php(default = null)] second: Option<i64>,
    #[php(default = null)] month: Option<i64>,
    #[php(default = null)] day: Option<i64>,
    #[php(default = null)] year: Option<i64>,
) -> Result<OrFalse<i64>, Thrown> {
    php_mktime(req, hour, [minute, second, month, day, year], false)
}

/// `gmmktime(...)`: `mktime` in UTC.
///
/// ```php
/// date_default_timezone_set('Europe/London');
/// var_dump(gmmktime(12, 30, 0, 6, 15, 2023), gmmktime(0, 0, 0, 1, 1, 1970), gmmktime(0, 0, 0, 1, 1, 0), gmmktime(-1, 0, 0, 1, 1, 1970));
/// ```
/// ```expect
/// int(1686832200)
/// int(0)
/// int(946684800)
/// int(-3600)
/// ```
#[php_function]
fn gmmktime(
    req: &mut Request,
    hour: i64,
    #[php(default = null)] minute: Option<i64>,
    #[php(default = null)] second: Option<i64>,
    #[php(default = null)] month: Option<i64>,
    #[php(default = null)] day: Option<i64>,
    #[php(default = null)] year: Option<i64>,
) -> Result<OrFalse<i64>, Thrown> {
    php_mktime(req, hour, [minute, second, month, day, year], true)
}

/// `checkdate(int $month, int $day, int $year): bool`.
///
/// ```php
/// var_dump(checkdate(2, 29, 2024), checkdate(2, 29, 2023), checkdate(12, 31, 32767), checkdate(1, 1, 32768), checkdate(1, 1, 0), checkdate(13, 1, 2000), checkdate(4, 31, 2000));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn checkdate(month: i64, day: i64, year: i64) -> bool {
    (1..=32767).contains(&year) && ffi::valid_date(year, month, day)
}

/// `time(): int`.
///
/// ```php
/// $t = time(); var_dump(is_int($t), $t > 1700000000, $t - (int) microtime(true) < 5);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn time() -> i64 {
    now()
}

/// The local time of `ts` in the default zone, what `localtime` and
/// `getdate` read.
fn local_time(req: &mut Request, ts: i64) -> Result<Time, Thrown> {
    let zone = default_zone(req)?;
    lent(req, |l| {
        let mut t = Time::new(l);
        t.set_zone_id(zone);
        t.unixtime2local(l, ts);
        t
    })
}

/// `localtime(?int $timestamp = null, bool $associative = false):
/// array`: C's `struct tm` as a list or under its field names.
///
/// ```php
/// date_default_timezone_set('Europe/Paris');
/// print_r(localtime(1690000000)); print_r(localtime(1700000000, true));
/// ```
/// ```expect
/// Array
/// (
///     [0] => 40
///     [1] => 26
///     [2] => 6
///     [3] => 22
///     [4] => 6
///     [5] => 123
///     [6] => 6
///     [7] => 202
///     [8] => 1
/// )
/// Array
/// (
///     [tm_sec] => 20
///     [tm_min] => 13
///     [tm_hour] => 23
///     [tm_mday] => 14
///     [tm_mon] => 10
///     [tm_year] => 123
///     [tm_wday] => 2
///     [tm_yday] => 317
///     [tm_isdst] => 0
/// )
/// ```
///
/// Its peak is php's: the list's table is laid down for nine rows at
/// once and the fields' at the first row, `array_init_size(9)`, so
/// neither doubles while the timelib time stands.
///
/// ```php
/// date_default_timezone_set('Europe/Amsterdam');
/// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
/// pk(fn () => localtime(0)); pk(fn () => localtime(0, true));
/// ```
/// ```expect capture
/// 648 408
/// 1264 992
/// ```
#[php_function]
fn localtime(
    req: &mut Request,
    #[php(default = null)] timestamp: Option<i64>,
    #[php(default = false)] associative: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let ts = timestamp.unwrap_or_else(now);
    let t = local_time(req, ts)?;
    kept(req, t, |req, t| localtime_rows(req, t, associative))
}

/// The rows of `localtime()` over the local time `t`.
fn localtime_rows(
    req: &mut Request,
    t: &Time,
    associative: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let rows: [(&[u8], i64); 9] = [
        (b"tm_sec", t.s),
        (b"tm_min", t.i),
        (b"tm_hour", t.h),
        (b"tm_mday", t.d),
        (b"tm_mon", t.m - 1),
        (b"tm_year", t.y - 1900),
        (b"tm_wday", ffi::day_of_week(t.y, t.m, t.d)),
        (b"tm_yday", ffi::day_of_year(t.y, t.m, t.d)),
        (b"tm_isdst", i64::from(t.dst)),
    ];
    // `array_init_size(return_value, 9)`, the list's table laid down at
    // once by `zend_hash_real_init_packed`, the fields' at the first
    // `add_assoc_long`.
    let mut array = if associative {
        PhpArray::init_size(req, 9)?
    } else {
        PhpArray::sized(req, 9, Layout::Packed)?
    };
    for (name, value) in rows {
        if associative {
            set_str(req, &mut array, name, Owner::int(value))?;
        } else {
            array.append(req, Owner::int(value))?;
        }
    }
    Ok(array)
}

/// `getdate(?int $timestamp = null): array`.
///
/// ```php
/// date_default_timezone_set('Australia/Sydney');
/// print_r(getdate(1700000000));
/// ```
/// ```expect
/// Array
/// (
///     [seconds] => 20
///     [minutes] => 13
///     [hours] => 9
///     [mday] => 15
///     [wday] => 3
///     [mon] => 11
///     [year] => 2023
///     [yday] => 318
///     [weekday] => Wednesday
///     [month] => November
///     [0] => 1700000000
/// )
/// ```
///
/// Its peak is php's, the table sized for its eleven rows,
/// `array_init_size(11)`, while the timelib time stands.
///
/// ```php
/// date_default_timezone_set('Europe/Amsterdam');
/// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
/// pk(fn () => getdate(0));
/// ```
/// ```expect capture
/// 1360 1120
/// ```
#[php_function]
fn getdate(
    req: &mut Request,
    #[php(default = null)] timestamp: Option<i64>,
) -> Result<Owner<PhpArray>, Thrown> {
    let ts = timestamp.unwrap_or_else(now);
    let t = local_time(req, ts)?;
    kept(req, t, |req, t| getdate_rows(req, t, ts))
}

/// The rows of `getdate()` over the local time `t` of `ts`.
fn getdate_rows(req: &mut Request, t: &Time, ts: i64) -> Result<Owner<PhpArray>, Thrown> {
    // `array_init_size(return_value, 11)`.
    let mut array = PhpArray::init_size(req, 11)?;
    let rows: [(&[u8], i64); 8] = [
        (b"seconds", t.s),
        (b"minutes", t.i),
        (b"hours", t.h),
        (b"mday", t.d),
        (b"wday", ffi::day_of_week(t.y, t.m, t.d)),
        (b"mon", t.m),
        (b"year", t.y),
        (b"yday", ffi::day_of_year(t.y, t.m, t.d)),
    ];
    for (name, value) in rows {
        set_str(req, &mut array, name, Owner::int(value))?;
    }
    set_string(
        req,
        &mut array,
        b"weekday",
        full_day(t.y, t.m, t.d).as_bytes(),
    )?;
    set_string(req, &mut array, b"month", full_month(t.m).as_bytes())?;
    array.set(req, Owner::index(0), Owner::int(ts))?;
    Ok(array)
}

/// `php_strftime(INTERNAL_FUNCTION_PARAM_PASSTHRU, gmt)`
/// (`Z/ext/date/php_date.c:1264`): the `struct tm` php fills from the
/// time in the default zone, or in UTC for `gmstrftime`, rendered by
/// the C library's `strftime(3)` under the request's `LC_TIME` into a
/// string on the request's heap, with the time and the zone's offset
/// standing; `false` for an empty format and for text the C library
/// answered none of.
fn php_strftime(
    req: &mut Request,
    format: PhpString,
    timestamp: Option<i64>,
    gmt: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let format = format.as_bytes();
    if format.is_empty() {
        return Ok(OrFalse::False);
    }
    let ts = timestamp.unwrap_or_else(now);
    let zone = if gmt { None } else { Some(default_zone(req)?) };
    let (t, offset) = lent(req, |l| {
        let mut t = Time::new(l);
        let offset = match zone {
            None => {
                t.unixtime2gmt(l, ts);
                None
            }
            Some(zone) => {
                t.set_zone_id(zone.clone());
                t.unixtime2local(l, ts);
                Some(zone.offset_in(l, ts))
            }
        };
        (t, offset)
    })?;
    let tm = strftime_of(format, &t, offset.as_ref());
    let written = strftime_into(req, &tm);
    // `timelib_time_dtor(ts)`, then `timelib_time_offset_dtor(offset)`.
    release(req, t)?;
    release(req, offset)?;
    let (buf, len) = written?;
    if len != 0 && len != buf.len() {
        return Ok(OrFalse::Value(buf.truncate(req, len)?.finish()));
    }
    buf.free(req);
    Ok(OrFalse::False)
}

/// `php_strftime`'s loop: `zend_string_alloc(256)`, written by
/// `strftime(3)` and doubled by `zend_string_extend` while it answers
/// nothing or fills the string, five times at most, so the string is
/// 8192 bytes where the text is longer than 4095 bytes or empty. The
/// string and the length last written; the `Err` is the memory limit's
/// fatal.
fn strftime_into(req: &mut Request, tm: &ffi::Strftime) -> Result<(FreshString, usize), Thrown> {
    let mut buf = FreshString::new(req, 256)?;
    let mut len = 0;
    for _ in 0..5 {
        len = tm.write(buf.bytes_mut());
        if len != 0 && len != buf.len() {
            break;
        }
        let grown = buf.len() * 2;
        buf = buf.extend(req, grown)?;
    }
    Ok((buf, len))
}

/// The `struct tm` `php_strftime` fills from `t` and its offset, `GMT`
/// with none where the time is UTC, and the format, ready for
/// `strftime(3)`.
fn strftime_of(format: &[u8], t: &Time, offset: Option<&TimeOffset>) -> ffi::Strftime {
    let tm = ffi::BrokenDown {
        sec: t.s,
        min: t.i,
        hour: t.h,
        mday: t.d,
        mon: t.m - 1,
        year: t.y - 1900,
        wday: ffi::day_of_week(t.y, t.m, t.d),
        yday: ffi::day_of_year(t.y, t.m, t.d),
        isdst: offset.is_some_and(TimeOffset::is_dst),
        gmtoff: i64::from(offset.map_or(0, TimeOffset::offset)),
        zone: offset.map_or(b"GMT", TimeOffset::abbr),
    };
    ffi::Strftime::new(format, &tm)
}

/// `strftime(string $format, ?int $timestamp = null): string|false`,
/// deprecated since 8.1, under the request's `LC_TIME`.
///
/// ```php
/// date_default_timezone_set('Europe/Amsterdam');
/// var_dump(strftime('%A %a %B %b %d %e %H %I %j %m %M %p %S %u %w %y %Y %Z %% %D %F %T %R %V %G %g %U %W %C', 1700000000));
/// var_dump(strftime('', 1700000000));
/// ```
/// ```expect
/// Deprecated: Function strftime() is deprecated since 8.1, use IntlDateFormatter::format() instead in %s on line 3
/// string(125) "Tuesday Tue November Nov 14 14 23 11 318 11 13 PM 20 2 2 23 2023 CET % 11/14/23 2023-11-14 23:13:20 23:13 46 2023 23 46 46 20"
///
/// Deprecated: Function strftime() is deprecated since 8.1, use IntlDateFormatter::format() instead in %s on line 4
/// bool(false)
/// ```
///
/// Under the request's `LC_TIME`, the names and the formats the C
/// library has for it.
///
/// ```php
/// date_default_timezone_set('UTC');
/// foreach (['fr_FR.UTF-8', 'de_DE.UTF-8', 'C'] as $l) { setlocale(LC_TIME, $l); var_dump(@strftime('%A %a %B %b|%c|%x|%p', 1700000000)); }
/// ```
/// ```expect locale=fr_FR.UTF-8 locale=de_DE.UTF-8
/// string(59) "Mardi Mar novembre nov|Mar 14 nov 22:13:20 2023|14.11.2023|"
/// string(62) "Dienstag Di November Nov|Di 14 Nov 22:13:20 2023|14.11.2023|pm"
/// string(61) "Tuesday Tue November Nov|Tue Nov 14 22:13:20 2023|11/14/23|PM"
/// ```
///
/// Its peak is php's: the text is written into a string of 256 bytes on
/// the request's heap, doubled while it does not fit, up to 8192 for a
/// text the C library answers none of, with the time and its offset
/// standing.
///
/// ```php
/// date_default_timezone_set('Europe/Amsterdam');
/// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
/// pk(fn () => @strftime('%Z %z', 0)); pk(fn () => @strftime(str_repeat('x', 300), 0)); pk(fn () => @strftime(str_repeat('x', 5000), 0));
/// ```
/// ```expect capture
/// 640 72
/// 1408 384
/// 20800 0
/// ```
#[php_function(deprecated(since = "8.1", message = "use IntlDateFormatter::format() instead"))]
fn strftime(
    req: &mut Request,
    format: PhpString,
    #[php(default = null)] timestamp: Option<i64>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    php_strftime(req, format, timestamp, false)
}

/// `gmstrftime(string $format, ?int $timestamp = null): string|false`,
/// deprecated since 8.1. `%z` is the C library's: glibc reads the
/// `tm_gmtoff` php sets to 0, and macOS's the process's `TZ`, whatever
/// the time's zone, in php as here; the recipe's `TZ` is `UTC` on both
/// sides of a witness (#1130).
///
/// ```php
/// date_default_timezone_set('Europe/Amsterdam');
/// var_dump(gmstrftime('%Y-%m-%d %H:%M:%S %Z %z %c', 1700000000), @gmstrftime('%z', 0));
/// ```
/// ```expect
/// Deprecated: Function gmstrftime() is deprecated since 8.1, use IntlDateFormatter::format() instead in %s on line 3
/// string(54) "2023-11-14 22:13:20 GMT +0000 Tue Nov 14 22:13:20 2023"
/// string(5) "+0000"
/// ```
///
/// Under the request's `LC_TIME`.
///
/// ```php
/// foreach (['fr_FR.UTF-8', 'de_DE.UTF-8', 'C'] as $l) { setlocale(LC_TIME, $l); var_dump(@gmstrftime('%A %B %x %X', 1700000000)); }
/// ```
/// ```expect locale=fr_FR.UTF-8 locale=de_DE.UTF-8
/// string(34) "Mardi novembre 14.11.2023 22:13:20"
/// string(37) "Dienstag November 14.11.2023 22:13:20"
/// string(34) "Tuesday November 11/14/23 22:13:20"
/// ```
///
/// Its peak is php's, the time standing beside the string it grows.
///
/// ```php
/// date_default_timezone_set('Europe/Amsterdam');
/// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
/// pk(fn () => @gmstrftime('%Z %z', 0)); pk(fn () => @gmstrftime(str_repeat('%Y', 200), 0));
/// ```
/// ```expect capture
/// 576 72
/// 2624 896
/// ```
#[php_function(deprecated(since = "8.1", message = "use IntlDateFormatter::format() instead"))]
fn gmstrftime(
    req: &mut Request,
    format: PhpString,
    #[php(default = null)] timestamp: Option<i64>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    php_strftime(req, format, timestamp, true)
}
