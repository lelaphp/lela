//! An entry's MS-DOS date and time against the host's local zone, as
//! libzip's `_zip_u2d_time` and `_zip_d2u_time` reach it through
//! `localtime_r` and `mktime`: the zone `TZ` names, or `/etc/localtime`,
//! read from its `TZif` file; UTC where neither reads.

use std::path::PathBuf;

/// `zip_dostime_t`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct DosTime {
    pub(crate) time: u16,
    pub(crate) date: u16,
}

/// The torrentzip time, 1996-12-24 23:32:00.
pub(crate) const TORRENTZIP: DosTime = DosTime {
    time: 0xbc00,
    date: 0x2198,
};

/// A zone's transitions and the offset each starts, from its `TZif` file.
#[derive(Debug, Default)]
pub(crate) struct Zone {
    /// Each change's instant and the offset from it on.
    transitions: Vec<(i64, i64)>,
    /// The offset before the first change, or always where there is
    /// none.
    initial: i64,
}

impl Zone {
    /// `tzset()`'s zone: `TZ`, a `:`-prefixed or bare zone name, else
    /// `/etc/localtime`.
    pub(crate) fn local() -> Zone {
        let path = match std::env::var("TZ") {
            Ok(tz) if tz.is_empty() || tz.starts_with("UTC") || tz.starts_with("GMT") => {
                return Zone::default();
            }
            Ok(tz) => {
                let tz = tz.trim_start_matches(':');
                if tz.starts_with('/') {
                    PathBuf::from(tz)
                } else {
                    PathBuf::from("/usr/share/zoneinfo").join(tz)
                }
            }
            Err(_) => PathBuf::from("/etc/localtime"),
        };
        std::fs::read(path)
            .ok()
            .and_then(|data| parse(&data))
            .unwrap_or_default()
    }

    /// The offset in effect at `t`.
    fn offset_at(&self, t: i64) -> i64 {
        let at = self.transitions.partition_point(|(start, _)| *start <= t);
        match at.checked_sub(1) {
            Some(i) => self
                .transitions
                .get(i)
                .map_or(self.initial, |(_, off)| *off),
            None => self.initial,
        }
    }

    /// `_zip_u2d_time`: `localtime_r`, a year before 1980 made 1980.
    pub(crate) fn u2d(&self, t: i64) -> DosTime {
        let local = t + self.offset_at(t);
        let days = local.div_euclid(86_400);
        let secs = local.rem_euclid(86_400);
        let (mut year, month, day) = civil(days);
        if year < 1980 {
            year = 1980;
        }
        let date = ((year - 1980) << 9) + (month << 5) + day;
        let time = ((secs / 3600) << 11) + (((secs / 60) % 60) << 5) + ((secs % 60) >> 1);
        DosTime {
            time: u16::try_from(time & 0xffff).unwrap_or(0),
            date: u16::try_from(date & 0xffff).unwrap_or(0),
        }
    }

    /// `_zip_d2u_time`: `mktime` over the fields, normalised as it
    /// normalises them, with the offset that holds at that local time.
    pub(crate) fn d2u(&self, d: DosTime) -> i64 {
        let year = i64::from((d.date >> 9) & 127) + 1980;
        let month = i64::from((d.date >> 5) & 15) - 1;
        let day = i64::from(d.date & 31);
        let hour = i64::from((d.time >> 11) & 31);
        let min = i64::from((d.time >> 5) & 63);
        let sec = i64::from((d.time << 1) & 0x3e);
        let y = year + month.div_euclid(12);
        let m = month.rem_euclid(12) + 1;
        let local = (days_from_civil(y, m, 1) + day - 1) * 86_400 + hour * 3600 + min * 60 + sec;
        let guess = local - self.offset_at(local);
        local - self.offset_at(guess)
    }
}

/// A `TZif` file's transitions, the 64-bit block of version 2 and later.
fn parse(data: &[u8]) -> Option<Zone> {
    let header = |at: usize| -> Option<[usize; 6]> {
        if data.get(at..at + 4)? != b"TZif" {
            return None;
        }
        let mut counts = [0usize; 6];
        for (i, count) in counts.iter_mut().enumerate() {
            let bytes: [u8; 4] = data
                .get(at + 20 + i * 4..at + 24 + i * 4)?
                .try_into()
                .ok()?;
            *count = usize::try_from(u32::from_be_bytes(bytes)).ok()?;
        }
        Some(counts)
    };
    let [isut, isstd, leap, time, types, chars] = header(0)?;
    let v1 = time * 5 + types * 6 + chars + leap * 8 + isstd + isut;
    let (at, width, [_, _, _, time, types, _]) = if data.get(4).is_some_and(|v| *v >= b'2') {
        let at = 44 + v1;
        (at + 44, 8, header(at)?)
    } else {
        (44, 4, [isut, isstd, leap, time, types, chars])
    };
    let read = |at: usize, width: usize| -> Option<i64> {
        let bytes = data.get(at..at + width)?;
        Some(if width == 8 {
            i64::from_be_bytes(bytes.try_into().ok()?)
        } else {
            i64::from(i32::from_be_bytes(bytes.try_into().ok()?))
        })
    };
    let infos = at + time * width + time;
    let offset = |index: usize| read(infos + index * 6, 4);
    let mut transitions = Vec::with_capacity(time);
    for i in 0..time {
        let start = read(at + i * width, width)?;
        let index = usize::from(*data.get(at + time * width + i)?);
        transitions.push((start, offset(index)?));
    }
    let initial = if types > 0 { offset(0)? } else { 0 };
    Some(Zone {
        transitions,
        initial,
    })
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day since 1970-01-01.
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}
