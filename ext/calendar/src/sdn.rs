//! The serial day number and the three plain calendars over it:
//! `sdncal.h`, `gregor.c`, `julian.c`, `french.c` and `dow.c`, php's
//! arithmetic kept operation for operation, `int` where php computes in
//! `int` (wrapping, as the C compiles) and `zend_long` where it computes
//! in `zend_long`, because the out-of-range answers are observable.

/// A julian day count, php's `zend_long sdn`: the one representation
/// every conversion goes through. Any integer is one on the way in; a
/// conversion that finds no day answers `None` where php answers `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JulianDay(pub(crate) i64);

impl JulianDay {
    /// php's answer from a `*ToSdn`: zero is the invalid date.
    pub(crate) const fn counted(n: i64) -> Option<JulianDay> {
        if n == 0 { None } else { Some(JulianDay(n)) }
    }

    /// `DayOfWeek`: 0 for Sunday to 6 for Saturday. php's `(int)` is of
    /// a value in `2..=14`, so it keeps every bit.
    pub(crate) fn day_of_week(self) -> usize {
        usize::try_from((self.0 % 7 + 8) % 7).unwrap_or(0)
    }
}

/// A date in one of the calendars, as the `SdnTo*` functions write it
/// through their three `int *` out-parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Date {
    pub(crate) year: i32,
    pub(crate) month: i32,
    pub(crate) day: i32,
}

/// The `month/day/year` php's `"%i/%i/%i"` prints, `0/0/0` for no date.
pub(crate) fn render(date: Option<Date>) -> String {
    let Date { year, month, day } = date.unwrap_or(Date {
        year: 0,
        month: 0,
        day: 0,
    });
    format!("{month}/{day}/{year}")
}

/// The name `names` holds for `month`, `""` for no date, as php
/// indexes its tables at zero for `0/0/0`.
pub(crate) fn month_name(names: &'static [&'static str], date: Option<Date>) -> &'static str {
    date.and_then(|d| usize::try_from(d.month).ok())
        .and_then(|m| names.get(m))
        .copied()
        .unwrap_or("")
}

pub(crate) const DAY_NAME_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
pub(crate) const DAY_NAME_LONG: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

pub(crate) const MONTH_NAME_SHORT: [&str; 13] = [
    "", "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
pub(crate) const MONTH_NAME_LONG: [&str; 13] = [
    "",
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
pub(crate) const FRENCH_MONTH_NAME: [&str; 14] = [
    "",
    "Vendemiaire",
    "Brumaire",
    "Frimaire",
    "Nivose",
    "Pluviose",
    "Ventose",
    "Germinal",
    "Floreal",
    "Prairial",
    "Messidor",
    "Thermidor",
    "Fructidor",
    "Extra",
];

const GREGOR_SDN_OFFSET: i64 = 32045;
const JULIAN_SDN_OFFSET: i64 = 32083;
const DAYS_PER_5_MONTHS: i64 = 153;
const DAYS_PER_4_YEARS: i64 = 1461;
const DAYS_PER_400_YEARS: i64 = 146_097;
const INT_MAX: i64 = i32::MAX as i64;

/// The month and day of a `dayOfYear` counted from March, and the year
/// moved on for January and February, then numbered B.C./A.D. as php
/// numbers it (no year zero): the tail `SdnToGregorian` and
/// `SdnToJulian` share.
#[expect(
    clippy::cast_possible_truncation,
    reason = "php's `int` of a value under 400"
)]
fn from_march(year: i32, day_of_year: i64) -> Date {
    let temp = day_of_year * 5 - 3;
    let mut month = (temp / DAYS_PER_5_MONTHS) as i32;
    let day = ((temp % DAYS_PER_5_MONTHS) / 5 + 1) as i32;
    let mut year = year;
    if month < 10 {
        month += 3;
    } else {
        year = year.wrapping_add(1);
        month -= 9;
    }
    year = year.wrapping_sub(4800);
    if year <= 0 {
        year = year.wrapping_sub(1);
    }
    Date { year, month, day }
}

/// `SdnToGregorian`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "php's `int` of values it has bounded"
)]
pub(crate) fn sdn_to_gregorian(sdn: JulianDay) -> Option<Date> {
    let sdn = sdn.0;
    if sdn <= 0 || sdn > (i64::MAX - 4 * GREGOR_SDN_OFFSET) / 4 {
        return None;
    }
    let temp = (sdn + GREGOR_SDN_OFFSET) * 4 - 1;
    if temp < 0 || temp / DAYS_PER_400_YEARS > INT_MAX {
        return None;
    }
    let century = (temp / DAYS_PER_400_YEARS) as i32;
    let temp = ((temp % DAYS_PER_400_YEARS) / 4) * 4 + 3;
    if i64::from(century) > (INT_MAX / 100) - temp / DAYS_PER_4_YEARS {
        return None;
    }
    let year = century * 100 + (temp / DAYS_PER_4_YEARS) as i32;
    Some(from_march(year, (temp % DAYS_PER_4_YEARS) / 4 + 1))
}

/// The year counted from 4801 B.C. and the month from March, the head
/// `GregorianToSdn` and `JulianToSdn` share once the date is valid.
fn to_march(year: i32, month: i32) -> (i64, i64) {
    let mut year = i64::from(year) + if year < 0 { 4801 } else { 4800 };
    let month = if month > 2 {
        month - 3
    } else {
        year -= 1;
        month + 9
    };
    (year, i64::from(month))
}

/// `GregorianToSdn`.
pub(crate) fn gregorian_to_sdn(year: i32, month: i32, day: i32) -> Option<JulianDay> {
    if year == 0
        || !(-4714..=i32::MAX - 4800).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
    {
        return None;
    }
    if year == -4714 && (month < 11 || (month == 11 && day < 25)) {
        return None;
    }
    let (year, month) = to_march(year, month);
    JulianDay::counted(
        ((year / 100) * DAYS_PER_400_YEARS) / 4
            + ((year % 100) * DAYS_PER_4_YEARS) / 4
            + (month * DAYS_PER_5_MONTHS + 2) / 5
            + i64::from(day)
            - GREGOR_SDN_OFFSET,
    )
}

/// `SdnToJulian`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "php's `int` of a value it has bounded"
)]
pub(crate) fn sdn_to_julian(sdn: JulianDay) -> Option<Date> {
    let sdn = sdn.0;
    if sdn <= 0 || !(i64::MIN / 4..=(i64::MAX - JULIAN_SDN_OFFSET * 4 + 1) / 4).contains(&sdn) {
        return None;
    }
    let temp = sdn * 4 + (JULIAN_SDN_OFFSET * 4 - 1);
    let year = temp / DAYS_PER_4_YEARS;
    if year > INT_MAX || year < i64::from(i32::MIN) {
        return None;
    }
    Some(from_march(year as i32, (temp % DAYS_PER_4_YEARS) / 4 + 1))
}

/// `JulianToSdn`.
pub(crate) fn julian_to_sdn(year: i32, month: i32, day: i32) -> Option<JulianDay> {
    if year == 0
        || !(-4713..=i32::MAX - 4800).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
    {
        return None;
    }
    if year == -4713 && month == 1 && day == 1 {
        return None;
    }
    let (year, month) = to_march(year, month);
    JulianDay::counted(
        (year * DAYS_PER_4_YEARS) / 4 + (month * DAYS_PER_5_MONTHS + 2) / 5 + i64::from(day)
            - JULIAN_SDN_OFFSET,
    )
}

const FRENCH_SDN_OFFSET: i64 = 2_375_474;
const DAYS_PER_MONTH: i64 = 30;
const FIRST_VALID: i64 = 2_375_840;
const LAST_VALID: i64 = 2_380_952;

/// The day after the French republican calendar's last, 0014-13-05,
/// which `cal_days_in_month` counts to.
pub(crate) const FRENCH_END: JulianDay = JulianDay(LAST_VALID + 1);

/// `SdnToFrench`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "php's `int` of values under 5200"
)]
pub(crate) fn sdn_to_french(sdn: JulianDay) -> Option<Date> {
    if !(FIRST_VALID..=LAST_VALID).contains(&sdn.0) {
        return None;
    }
    let temp = (sdn.0 - FRENCH_SDN_OFFSET) * 4 - 1;
    let day_of_year = (temp % DAYS_PER_4_YEARS) / 4;
    Some(Date {
        year: (temp / DAYS_PER_4_YEARS) as i32,
        month: (day_of_year / DAYS_PER_MONTH + 1) as i32,
        day: (day_of_year % DAYS_PER_MONTH + 1) as i32,
    })
}

/// `FrenchToSdn`.
pub(crate) fn french_to_sdn(year: i32, month: i32, day: i32) -> Option<JulianDay> {
    if !(1..=14).contains(&year) || !(1..=13).contains(&month) || !(1..=30).contains(&day) {
        return None;
    }
    JulianDay::counted(
        (i64::from(year) * DAYS_PER_4_YEARS) / 4
            + i64::from(month - 1) * DAYS_PER_MONTH
            + i64::from(day)
            + FRENCH_SDN_OFFSET,
    )
}
