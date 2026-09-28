//! The platform's local time, which `easter_date` reaches through
//! `mktime` and `unixtojd` through `php_localtime_r`: not
//! `date.timezone` but the process's `TZ`, which PHP code moves with
//! `putenv('TZ=…')` (php calls `tzset` there). This engine serves
//! requests on threads, so the zone is read from the request's own
//! environment, and resolved as the C library resolves it: an unset
//! `TZ` is `/etc/localtime`; a set one names a `TZif` file, absolute or
//! under `/usr/share/zoneinfo`, with a leading `:` dropped; one that
//! names no file is read as a POSIX TZ string; and one that is neither,
//! or empty, is UTC. Leap-second (`right/`) files are read as if they
//! carried none.

use php_ext::request::Request;

/// Seconds in a day.
const DAY: i64 = 86_400;

/// The days from 1970-01-01 to `year`-`month`-`day` in the proleptic
/// Gregorian calendar (`days_from_civil`), any year an `i64` holds a
/// timestamp of.
pub(crate) const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of the day `days` after 1970-01-01 (`civil_from_days`).
pub(crate) const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if month <= 2 { 1 } else { 0 }, month, day)
}

const fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// A day a POSIX rule names in its year.
#[derive(Clone, Copy)]
enum RuleDay {
    /// `Jn`: day 1 to 365, February 29 never counted.
    Julian(i64),
    /// `n`: day 0 to 365, February 29 counted.
    Zero(i64),
    /// `Mm.w.d`: day `d` (0 Sunday) of week `w` (5 the last) of month `m`.
    Week { month: i64, week: i64, weekday: i64 },
}

impl RuleDay {
    /// The day, as days since the epoch, in `year`.
    const fn in_year(self, year: i64) -> i64 {
        let jan1 = days_from_civil(year, 1, 1);
        match self {
            RuleDay::Julian(n) => jan1 + n - 1 + if is_leap(year) && n >= 60 { 1 } else { 0 },
            RuleDay::Zero(n) => jan1 + n,
            RuleDay::Week {
                month,
                week,
                weekday,
            } => {
                let first = days_from_civil(year, month, 1);
                // 1970-01-01 was a Thursday.
                let first_weekday = (first + 4).rem_euclid(7);
                let mut day = first + (weekday - first_weekday).rem_euclid(7) + (week - 1) * 7;
                let next = if month == 12 {
                    days_from_civil(year + 1, 1, 1)
                } else {
                    days_from_civil(year, month + 1, 1)
                };
                while day >= next {
                    day -= 7;
                }
                day
            }
        }
    }
}

/// A POSIX TZ string: the standard offset, and daylight time with the
/// two moments it starts and ends at, local time of day in seconds.
#[derive(Clone, Copy)]
struct Rule {
    std: i64,
    dst: Option<Daylight>,
}

/// Daylight time: its offset, and the day and local time of day it
/// starts and ends on.
#[derive(Clone, Copy)]
struct Daylight {
    offset: i64,
    start: (RuleDay, i64),
    end: (RuleDay, i64),
}

/// The rule the C library assumes for a daylight name with no dates,
/// the United States' since 2007.
const DEFAULT_DST: ((RuleDay, i64), (RuleDay, i64)) = (
    (
        RuleDay::Week {
            month: 3,
            week: 2,
            weekday: 0,
        },
        7200,
    ),
    (
        RuleDay::Week {
            month: 11,
            week: 1,
            weekday: 0,
        },
        7200,
    ),
);

/// A reader over the bytes of a TZ string.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn eat(&mut self, c: u8) -> bool {
        if self.0.first() == Some(&c) {
            self.0 = &self.0[1..];
            true
        } else {
            false
        }
    }

    /// A zone abbreviation: three or more letters, or `<…>` quoted.
    fn name(&mut self) -> Option<()> {
        if self.eat(b'<') {
            let end = self.0.iter().position(|c| *c == b'>')?;
            self.0 = &self.0[end + 1..];
            return Some(());
        }
        let len = self
            .0
            .iter()
            .take_while(|c| c.is_ascii_alphabetic())
            .count();
        self.0 = &self.0[len..];
        (len >= 3).then_some(())
    }

    fn number(&mut self) -> Option<i64> {
        let len = self.0.iter().take_while(|c| c.is_ascii_digit()).count();
        let digits = std::str::from_utf8(&self.0[..len]).ok()?.parse().ok()?;
        self.0 = &self.0[len..];
        Some(digits)
    }

    /// `[+-]hh[:mm[:ss]]` in seconds, sign as written.
    fn time(&mut self) -> Option<i64> {
        let sign = if self.eat(b'-') {
            -1
        } else {
            self.eat(b'+');
            1
        };
        let mut seconds = self.number()? * 3600;
        if self.eat(b':') {
            seconds += self.number()? * 60;
            if self.eat(b':') {
                seconds += self.number()?;
            }
        }
        Some(sign * seconds)
    }

    /// One rule date, `Jn`, `n` or `Mm.w.d`, with its `/time`.
    fn date(&mut self) -> Option<(RuleDay, i64)> {
        let day = if self.eat(b'J') {
            RuleDay::Julian(self.number()?)
        } else if self.eat(b'M') {
            let month = self.number()?;
            self.eat(b'.').then_some(())?;
            let week = self.number()?;
            self.eat(b'.').then_some(())?;
            let weekday = self.number()?;
            RuleDay::Week {
                month,
                week,
                weekday,
            }
        } else {
            RuleDay::Zero(self.number()?)
        };
        let at = if self.eat(b'/') { self.time()? } else { 7200 };
        Some((day, at))
    }
}

impl Rule {
    /// `tzparse`: `std offset [dst [offset] [,start[/time],end[/time]]]`.
    fn parse(text: &[u8]) -> Option<Rule> {
        let mut at = Cursor(text);
        at.name()?;
        // POSIX offsets are west of Greenwich; ours are east.
        let std = -at.time()?;
        if at.0.is_empty() {
            return Some(Rule { std, dst: None });
        }
        at.name()?;
        let dst = if at.0.is_empty() || at.0[0] == b',' {
            std + 3600
        } else {
            -at.time()?
        };
        let dates = if at.eat(b',') {
            let start = at.date()?;
            at.eat(b',').then_some(())?;
            (start, at.date()?)
        } else {
            DEFAULT_DST
        };
        let daylight = Daylight {
            offset: dst,
            start: dates.0,
            end: dates.1,
        };
        at.0.is_empty().then_some(Rule {
            std,
            dst: Some(daylight),
        })
    }

    /// The offset east of UTC at `t`.
    fn offset(&self, t: i64) -> i64 {
        let Some(Daylight {
            offset: dst,
            start: (start, start_at),
            end: (end, end_at),
        }) = self.dst
        else {
            return self.std;
        };
        let (year, _, _) = civil_from_days((t + self.std).div_euclid(DAY));
        let begins = start.in_year(year) * DAY + start_at - self.std;
        let ends = end.in_year(year) * DAY + end_at - dst;
        let in_dst = if begins < ends {
            begins <= t && t < ends
        } else {
            !(ends <= t && t < begins)
        };
        if in_dst { dst } else { self.std }
    }
}

/// The zone the request's `TZ` names.
pub(crate) struct Zone {
    /// Each moment the offset changes, and the offset from it on, in order.
    transitions: Vec<(i64, i64)>,
    /// The offset before the first change.
    initial: i64,
    /// The `TZif` footer, or the whole of a `TZ` that names no file,
    /// for the moments after the last change.
    rule: Option<Rule>,
}

const UTC: Zone = Zone {
    transitions: Vec::new(),
    initial: 0,
    rule: None,
};

impl Zone {
    /// The zone of `req`'s environment, as `tzset` reads `TZ`.
    pub(crate) fn of(req: &Request) -> Zone {
        let Some(tz) = req.env(b"TZ") else {
            return Zone::file(b"/etc/localtime").unwrap_or(UTC);
        };
        let name = tz.strip_prefix(b":").unwrap_or(tz);
        if name.is_empty() {
            return UTC;
        }
        let path = if name.starts_with(b"/") {
            name.to_vec()
        } else {
            [b"/usr/share/zoneinfo/".as_slice(), name].concat()
        };
        let named =
            (!name.split(|c| *c == b'/').any(|part| part == b"..")).then(|| Zone::file(&path));
        named
            .flatten()
            .or_else(|| {
                Rule::parse(tz).map(|rule| Zone {
                    transitions: Vec::new(),
                    initial: rule.std,
                    rule: Some(rule),
                })
            })
            .unwrap_or(UTC)
    }

    /// A `TZif` file (RFC 8536), its 64-bit block where it has one.
    fn file(path: &[u8]) -> Option<Zone> {
        let bytes = std::fs::read(std::str::from_utf8(path).ok()?).ok()?;
        let header = |at: usize| -> Option<[usize; 6]> {
            (bytes.get(at..at + 4)? == b"TZif").then_some(())?;
            let mut counts = [0; 6];
            for (i, count) in counts.iter_mut().enumerate() {
                let field = bytes.get(at + 20 + i * 4..at + 24 + i * 4)?;
                *count = usize::try_from(u32::from_be_bytes(field.try_into().ok()?)).ok()?;
            }
            Some(counts)
        };
        let [isut, isstd, leap, time, types, chars] = header(0)?;
        let v1_len = 44 + time * 5 + types * 6 + chars + leap * 8 + isstd + isut;
        let (start, width, counts) = if bytes.get(4).is_some_and(|v| *v >= b'2') {
            (v1_len, 8, header(v1_len)?)
        } else {
            (0, 4, [isut, isstd, leap, time, types, chars])
        };
        let [isut, isstd, leap, time, types, chars] = counts;
        let data = start + 44;
        let moment = |i: usize| -> Option<i64> {
            let field = bytes.get(data + i * width..data + (i + 1) * width)?;
            Some(if width == 8 {
                i64::from_be_bytes(field.try_into().ok()?)
            } else {
                i64::from(i32::from_be_bytes(field.try_into().ok()?))
            })
        };
        let index_at = data + time * width;
        let types_at = index_at + time;
        let offset = |t: usize| -> Option<i64> {
            let field = bytes.get(types_at + t * 6..types_at + t * 6 + 4)?;
            Some(i64::from(i32::from_be_bytes(field.try_into().ok()?)))
        };
        let mut transitions = Vec::with_capacity(time);
        for i in 0..time {
            let t = usize::from(*bytes.get(index_at + i)?);
            transitions.push((moment(i)?, offset(t)?));
        }
        let end = types_at + types * 6 + chars + leap * (width + 4) + isstd + isut;
        let rule = if width == 8 {
            let footer = bytes.get(end + 1..)?;
            let footer = &footer[..footer.iter().position(|c| *c == b'\n')?];
            Rule::parse(footer)
        } else {
            None
        };
        Some(Zone {
            transitions,
            initial: offset(0)?,
            rule,
        })
    }

    /// The offset east of UTC at `t`.
    pub(crate) fn offset(&self, t: i64) -> i64 {
        let passed = self.transitions.partition_point(|(at, _)| *at <= t);
        match (
            passed.checked_sub(1).map(|i| self.transitions[i]),
            &self.rule,
        ) {
            (_, Some(rule)) if passed == self.transitions.len() => rule.offset(t),
            (Some((_, offset)), _) => offset,
            (None, _) => self.initial,
        }
    }

    /// `localtime_r`'s date of `t`: the year, month and day.
    pub(crate) fn date(&self, t: i64) -> (i64, i64, i64) {
        civil_from_days(t.saturating_add(self.offset(t)).div_euclid(DAY))
    }

    /// `mktime` of midnight on `year`-`month`-`day`, `tm_isdst = -1`:
    /// the offset in force there, and where midnight falls in a
    /// spring-forward gap, the offset before it.
    pub(crate) fn midnight(&self, year: i64, month: i64, day: i64) -> i64 {
        let local = days_from_civil(year, month, day) * DAY;
        let guess = self.offset(local);
        let first = self.offset(local - guess);
        if self.offset(local - first) == first {
            return local - first;
        }
        let other = self.offset(local - first);
        if self.offset(local - other) == other {
            local - other
        } else {
            local - guess.min(first)
        }
    }
}
