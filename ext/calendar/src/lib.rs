//! The `calendar` extension: `php-src/ext/calendar` as one crate under
//! `ext/` (ADR-0004), php's own C written again in Rust with no library
//! (design §7.2). One dependency, the facade; one entry, `extension()`;
//! no state (ADR-0005). The phpt subset is `php-src/ext/calendar/tests`,
//! through `xtask phpt --subset`.
//!
//! Every conversion goes through [`JulianDay`], the serial day number
//! php calls `sdn`: [`sdn`] is `gregor.c`, `julian.c`, `french.c` and
//! `dow.c`, [`jewish`] is `jewish.c`, and this file is `calendar.c`'s
//! natives over them, with the calendar ID decoded once into
//! [`Calendar`]. [`easter`] and [`unix`] are `easter.c` and
//! `cal_unix.c`, which read the platform's local time through the
//! request's [`Zone`](php_ext::request::Zone).
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod easter;
mod jewish;
mod sdn;
mod unix;

use php_ext::abi::{Extension, ExtensionBuilder, Union};
use php_ext::attr::{php_const, php_function};
use php_ext::raising::{Thrown, arg_value_error, value_error};
use php_ext::request::Request;
use php_ext::values::{Layout, Owner, PhpArray, PhpString, Value};

use constants::{
    CAL_DOW_LONG, CAL_DOW_SHORT, CAL_FRENCH, CAL_GREGORIAN, CAL_JEWISH, CAL_JULIAN,
    CAL_MONTH_FRENCH, CAL_MONTH_GREGORIAN_LONG, CAL_MONTH_JEWISH, CAL_MONTH_JULIAN_LONG,
    CAL_MONTH_JULIAN_SHORT,
};
use jewish::HebrewFlags;
use sdn::{Date, JulianDay};

/// The constants `register_calendar_symbols` registers, in
/// `calendar.stub.php`'s order.
///
/// ```php
/// var_dump(CAL_GREGORIAN, CAL_FRENCH, CAL_NUM_CALS, CAL_DOW_SHORT, CAL_MONTH_FRENCH, CAL_EASTER_ALWAYS_JULIAN, CAL_JEWISH_ADD_GERESHAYIM);
/// ```
/// ```expect
/// int(0)
/// int(3)
/// int(4)
/// int(2)
/// int(5)
/// int(3)
/// int(8)
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const CAL_GREGORIAN: i64 = 0;
    pub(crate) const CAL_JULIAN: i64 = 1;
    pub(crate) const CAL_JEWISH: i64 = 2;
    pub(crate) const CAL_FRENCH: i64 = 3;
    pub(crate) const CAL_NUM_CALS: i64 = 4;
    pub(crate) const CAL_DOW_DAYNO: i64 = 0;
    pub(crate) const CAL_DOW_SHORT: i64 = 2;
    pub(crate) const CAL_DOW_LONG: i64 = 1;
    pub(crate) const CAL_MONTH_GREGORIAN_SHORT: i64 = 0;
    pub(crate) const CAL_MONTH_GREGORIAN_LONG: i64 = 1;
    pub(crate) const CAL_MONTH_JULIAN_SHORT: i64 = 2;
    pub(crate) const CAL_MONTH_JULIAN_LONG: i64 = 3;
    pub(crate) const CAL_MONTH_JEWISH: i64 = 4;
    pub(crate) const CAL_MONTH_FRENCH: i64 = 5;
    pub(crate) const CAL_EASTER_DEFAULT: i64 = 0;
    pub(crate) const CAL_EASTER_ROMAN: i64 = 1;
    pub(crate) const CAL_EASTER_ALWAYS_GREGORIAN: i64 = 2;
    pub(crate) const CAL_EASTER_ALWAYS_JULIAN: i64 = 3;
    pub(crate) const CAL_JEWISH_ADD_ALAFIM_GERESH: i64 = 0x2;
    pub(crate) const CAL_JEWISH_ADD_ALAFIM: i64 = 0x4;
    pub(crate) const CAL_JEWISH_ADD_GERESHAYIM: i64 = 0x8;
}

/// The extension value (design §7.1): `calendar_module_entry`, its
/// constants and its eighteen functions in the stub's order.
///
/// ```php
/// var_dump(extension_loaded('calendar'), count(get_extension_funcs('calendar')));
/// ```
/// ```expect
/// bool(true)
/// int(18)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .constants(constants::declared())
        .functions(&[
            cal_days_in_month::DECLARED,
            cal_from_jd::DECLARED,
            cal_info::DECLARED,
            cal_to_jd::DECLARED,
            easter::easter_date::DECLARED,
            easter::easter_days::DECLARED,
            frenchtojd::DECLARED,
            gregoriantojd::DECLARED,
            jddayofweek::DECLARED,
            jdmonthname::DECLARED,
            jdtofrench::DECLARED,
            jdtogregorian::DECLARED,
            jdtojewish::DECLARED,
            jdtojulian::DECLARED,
            unix::jdtounix::DECLARED,
            jewishtojd::DECLARED,
            juliantojd::DECLARED,
            unix::unixtojd::DECLARED,
        ])
        .info(calendar_info)
        .build("calendar")
}

/// `PHP_MINFO_FUNCTION(calendar)`.
///
/// ```php
/// (new ReflectionExtension('calendar'))->info();
/// ```
/// ```expect
/// calendar
///
/// Calendar support => enabled
/// ```
fn calendar_info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["Calendar support", "enabled"]]);
}

/// A calendar `calendar.c`'s conversion table has a row for, decoded
/// once from its `CAL_*` ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Calendar {
    Gregorian,
    Julian,
    Jewish,
    French,
}

impl Calendar {
    /// The table in ID order, which `cal_info()` lists.
    const ALL: [Calendar; 4] = [
        Calendar::Gregorian,
        Calendar::Julian,
        Calendar::Jewish,
        Calendar::French,
    ];

    /// The calendar `id` names, `None` outside `0..CAL_NUM_CALS`.
    const fn decode(id: i64) -> Option<Calendar> {
        match id {
            CAL_GREGORIAN => Some(Calendar::Gregorian),
            CAL_JULIAN => Some(Calendar::Julian),
            CAL_JEWISH => Some(Calendar::Jewish),
            CAL_FRENCH => Some(Calendar::French),
            _ => None,
        }
    }

    /// `id` decoded, or php's `ValueError` for the argument at `position`.
    fn argument(req: &mut Request, id: i64, position: u32) -> Result<Calendar, Thrown> {
        Calendar::decode(id).ok_or_else(|| arg_value_error(req, position, "be a valid calendar ID"))
    }

    /// The table's `name` and `symbol`.
    const fn names(self) -> (&'static str, &'static str) {
        match self {
            Calendar::Gregorian => ("Gregorian", "CAL_GREGORIAN"),
            Calendar::Julian => ("Julian", "CAL_JULIAN"),
            Calendar::Jewish => ("Jewish", "CAL_JEWISH"),
            Calendar::French => ("French", "CAL_FRENCH"),
        }
    }

    /// The table's `num_months`, `max_days_in_month`, `month_name_short`
    /// and `month_name_long`.
    const fn months(self) -> (usize, i64, &'static [&'static str], &'static [&'static str]) {
        match self {
            Calendar::Gregorian | Calendar::Julian => {
                (12, 31, &sdn::MONTH_NAME_SHORT, &sdn::MONTH_NAME_LONG)
            }
            Calendar::Jewish => (13, 30, &jewish::MONTH_NAME_LEAP, &jewish::MONTH_NAME_LEAP),
            Calendar::French => (13, 30, &sdn::FRENCH_MONTH_NAME, &sdn::FRENCH_MONTH_NAME),
        }
    }

    /// The table's `to_jd`, over php's `int` arguments.
    fn to_jd(self, year: i32, month: i32, day: i32) -> Option<JulianDay> {
        match self {
            Calendar::Gregorian => sdn::gregorian_to_sdn(year, month, day),
            Calendar::Julian => sdn::julian_to_sdn(year, month, day),
            Calendar::Jewish => jewish::jewish_to_sdn(year, month, day),
            Calendar::French => sdn::french_to_sdn(year, month, day),
        }
    }

    /// The table's `from_jd`.
    fn date(self, jd: JulianDay) -> Option<Date> {
        match self {
            Calendar::Gregorian => sdn::sdn_to_gregorian(jd),
            Calendar::Julian => sdn::sdn_to_julian(jd),
            Calendar::Jewish => jewish::sdn_to_jewish(jd),
            Calendar::French => sdn::sdn_to_french(jd),
        }
    }
}

/// php's `int` of a `zend_long` argument: the low 32 bits, which is
/// what the C conversion keeps.
#[expect(
    clippy::cast_possible_truncation,
    reason = "php passes a zend_long where the conversion declares an int"
)]
const fn int(n: i64) -> i32 {
    n as i32
}

/// A julian day count as php returns it, `0` for no day.
fn count(jd: Option<JulianDay>) -> i64 {
    jd.map_or(0, |jd| jd.0)
}

/// A string of the request's holding `bytes`.
fn string(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, bytes)
}

/// `add_assoc_*`: `value` under the string key `name`.
fn set(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    name: &str,
    value: Owner<Value>,
) -> Result<(), Thrown> {
    array.set_str(req, name.as_bytes(), value)
}

/// `add_assoc_string`.
fn set_string(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    name: &str,
    text: &str,
) -> Result<(), Thrown> {
    let value = string(req, text.as_bytes())?.into_value();
    set(req, array, name, value)
}

/// `_php_cal_info`: one calendar's month names, its longest month and
/// its names.
fn calendar_row(req: &mut Request, calendar: Calendar) -> Result<Owner<PhpArray>, Thrown> {
    let (months, max_days, short, long) = calendar.months();
    let mut row = PhpArray::new(req)?;
    for (name, names) in [("months", long), ("abbrevmonths", short)] {
        let size = u32::try_from(months + 1).unwrap_or(u32::MAX);
        let mut list = PhpArray::sized(req, size, Layout::Packed)?;
        for (at, month) in (1..).zip(names.iter().take(months + 1).skip(1)) {
            let value = string(req, month.as_bytes())?.into_value();
            list.set(req, Owner::index(at), value)?;
        }
        set(req, &mut row, name, list.into_value())?;
    }
    set(req, &mut row, "maxdaysinmonth", Owner::int(max_days))?;
    let (name, symbol) = calendar.names();
    set_string(req, &mut row, "calname", name)?;
    set_string(req, &mut row, "calsymbol", symbol)?;
    Ok(row)
}

/// `cal_info(int $calendar = -1): array`: one calendar's names, or
/// every calendar's under its ID.
///
/// ```php
/// print_r(cal_info(CAL_JEWISH));
/// var_dump(array_keys(cal_info()), cal_info(0)['abbrevmonths'][12]);
/// try { cal_info(4); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Array
/// (
///     [months] => Array
///         (
///             [1] => Tishri
///             [2] => Heshvan
///             [3] => Kislev
///             [4] => Tevet
///             [5] => Shevat
///             [6] => Adar I
///             [7] => Adar II
///             [8] => Nisan
///             [9] => Iyyar
///             [10] => Sivan
///             [11] => Tammuz
///             [12] => Av
///             [13] => Elul
///         )
///
///     [abbrevmonths] => Array
///         (
///             [1] => Tishri
///             [2] => Heshvan
///             [3] => Kislev
///             [4] => Tevet
///             [5] => Shevat
///             [6] => Adar I
///             [7] => Adar II
///             [8] => Nisan
///             [9] => Iyyar
///             [10] => Sivan
///             [11] => Tammuz
///             [12] => Av
///             [13] => Elul
///         )
///
///     [maxdaysinmonth] => 30
///     [calname] => Jewish
///     [calsymbol] => CAL_JEWISH
/// )
/// array(4) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(1)
///   [2]=>
///   int(2)
///   [3]=>
///   int(3)
/// }
/// string(3) "Dec"
/// cal_info(): Argument #1 ($calendar) must be a valid calendar ID
/// ```
#[php_function]
fn cal_info(
    req: &mut Request,
    #[php(default = -1)] calendar: i64,
) -> Result<Owner<PhpArray>, Thrown> {
    if calendar == -1 {
        let mut all = PhpArray::sized(req, 4, Layout::Packed)?;
        for (id, calendar) in (0..).zip(Calendar::ALL) {
            let row = calendar_row(req, calendar)?;
            all.set(req, Owner::index(id), row.into_value())?;
        }
        return Ok(all);
    }
    let calendar = Calendar::argument(req, calendar, 1)?;
    calendar_row(req, calendar)
}

/// A month argument, `1..=INT32_MAX - 1`.
fn check_month(req: &mut Request, month: i64, position: u32) -> Result<i32, Thrown> {
    if month <= 0 || month > i64::from(i32::MAX - 1) {
        let tail = format!("be between 1 and {}", i32::MAX - 1);
        return Err(arg_value_error(req, position, &tail));
    }
    Ok(int(month))
}

/// A year argument, under `INT32_MAX - 1`; one below `INT_MIN` keeps
/// php's low 32 bits.
fn check_year(req: &mut Request, year: i64, position: u32) -> Result<i32, Thrown> {
    if year > i64::from(i32::MAX - 1) {
        let tail = format!("be less than {}", i32::MAX - 1);
        return Err(arg_value_error(req, position, &tail));
    }
    Ok(int(year))
}

/// `ZEND_LONG_EXCEEDS_INT`: an argument that must fit php's `int`.
fn check_int(req: &mut Request, n: i64, position: u32) -> Result<i32, Thrown> {
    i32::try_from(n).map_err(|_| {
        let tail = format!("be between {} and {}", i32::MIN, i32::MAX);
        arg_value_error(req, position, &tail)
    })
}

/// `cal_days_in_month(int $calendar, int $month, int $year): int`: the
/// days from the month's first to the next month's, or to the next
/// year's (1 A.D. after 1 B.C.; the French calendar's end after its
/// last year).
///
/// ```php
/// var_dump(cal_days_in_month(CAL_GREGORIAN, 2, 2024), cal_days_in_month(CAL_JULIAN, 2, 1900), cal_days_in_month(CAL_JEWISH, 6, 5784), cal_days_in_month(CAL_JEWISH, 6, 5783), cal_days_in_month(CAL_FRENCH, 13, 14), cal_days_in_month(CAL_GREGORIAN, 12, -1));
/// foreach ([[9, 1, 1], [0, 0, 1], [0, 13, 1], [0, 1, PHP_INT_MAX], [0, 1, 0]] as [$c, $m, $y]) {
///     try { cal_days_in_month($c, $m, $y); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// int(29)
/// int(29)
/// int(30)
/// int(0)
/// int(5)
/// int(31)
/// cal_days_in_month(): Argument #1 ($calendar) must be a valid calendar ID
/// cal_days_in_month(): Argument #2 ($month) must be between 1 and 2147483646
/// Invalid date
/// cal_days_in_month(): Argument #3 ($year) must be less than 2147483646
/// Invalid date
/// ```
#[php_function]
fn cal_days_in_month(
    req: &mut Request,
    calendar: i64,
    month: i64,
    year: i64,
) -> Result<i64, Thrown> {
    let cal = Calendar::argument(req, calendar, 1)?;
    let month = check_month(req, month, 2)?;
    let whole_year = year;
    let year = check_year(req, year, 3)?;
    let Some(start) = cal.to_jd(year, month, 1) else {
        return Err(value_error(req, "Invalid date"));
    };
    let next = match cal.to_jd(year, month + 1, 1) {
        Some(next) => Some(next),
        None if whole_year == -1 => cal.to_jd(1, 1, 1),
        None => match cal.to_jd(int(whole_year + 1), 1, 1) {
            None if cal == Calendar::French => Some(sdn::FRENCH_END),
            next => next,
        },
    };
    Ok(count(next) - start.0)
}

/// `cal_to_jd(int $calendar, int $month, int $day, int $year): int`.
///
/// ```php
/// var_dump(cal_to_jd(CAL_GREGORIAN, 8, 26, 74), cal_to_jd(CAL_JULIAN, 8, 26, 74), cal_to_jd(CAL_JEWISH, 8, 26, 74), cal_to_jd(CAL_FRENCH, 8, 26, 74), cal_to_jd(CAL_FRENCH, 1, 1, 1));
/// foreach ([[-1, 1, 1, 1], [0, 0, 1, 1], [0, 1, PHP_INT_MIN, 1], [0, 1, 1, PHP_INT_MAX]] as [$c, $m, $d, $y]) {
///     try { cal_to_jd($c, $m, $d, $y); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// int(1748326)
/// int(1748324)
/// int(374867)
/// int(0)
/// int(2375840)
/// cal_to_jd(): Argument #1 ($calendar) must be a valid calendar ID
/// cal_to_jd(): Argument #2 ($month) must be between 1 and 2147483646
/// cal_to_jd(): Argument #3 ($day) must be between -2147483648 and 2147483647
/// cal_to_jd(): Argument #4 ($year) must be less than 2147483646
/// ```
#[php_function]
fn cal_to_jd(
    req: &mut Request,
    calendar: i64,
    month: i64,
    day: i64,
    year: i64,
) -> Result<i64, Thrown> {
    let cal = Calendar::argument(req, calendar, 1)?;
    let month = check_month(req, month, 2)?;
    let day = check_int(req, day, 3)?;
    let year = check_year(req, year, 4)?;
    Ok(count(cal.to_jd(year, month, day)))
}

/// `cal_from_jd(int $julian_day, int $calendar): array`: the date, its
/// parts, the day of the week and the month's names. A Jewish day
/// before the calendar's first has no day of the week.
///
/// ```php
/// var_dump(cal_from_jd(2453796, CAL_JEWISH));
/// print_r(cal_from_jd(2453796, CAL_GREGORIAN)); var_dump(cal_from_jd(0, CAL_FRENCH), cal_from_jd(1, CAL_JEWISH));
/// try { cal_from_jd(1, 7); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(9) {
///   ["date"]=>
///   string(8) "7/1/5766"
///   ["month"]=>
///   int(7)
///   ["day"]=>
///   int(1)
///   ["year"]=>
///   int(5766)
///   ["dow"]=>
///   int(3)
///   ["abbrevdayname"]=>
///   string(3) "Wed"
///   ["dayname"]=>
///   string(9) "Wednesday"
///   ["abbrevmonth"]=>
///   string(4) "Adar"
///   ["monthname"]=>
///   string(4) "Adar"
/// }
/// Array
/// (
///     [date] => 3/1/2006
///     [month] => 3
///     [day] => 1
///     [year] => 2006
///     [dow] => 3
///     [abbrevdayname] => Wed
///     [dayname] => Wednesday
///     [abbrevmonth] => Mar
///     [monthname] => March
/// )
/// array(9) {
///   ["date"]=>
///   string(5) "0/0/0"
///   ["month"]=>
///   int(0)
///   ["day"]=>
///   int(0)
///   ["year"]=>
///   int(0)
///   ["dow"]=>
///   int(1)
///   ["abbrevdayname"]=>
///   string(3) "Mon"
///   ["dayname"]=>
///   string(6) "Monday"
///   ["abbrevmonth"]=>
///   string(0) ""
///   ["monthname"]=>
///   string(0) ""
/// }
/// array(9) {
///   ["date"]=>
///   string(5) "0/0/0"
///   ["month"]=>
///   int(0)
///   ["day"]=>
///   int(0)
///   ["year"]=>
///   int(0)
///   ["dow"]=>
///   NULL
///   ["abbrevdayname"]=>
///   string(0) ""
///   ["dayname"]=>
///   string(0) ""
///   ["abbrevmonth"]=>
///   string(0) ""
///   ["monthname"]=>
///   string(0) ""
/// }
/// cal_from_jd(): Argument #2 ($calendar) must be a valid calendar ID
/// ```
#[php_function]
fn cal_from_jd(
    req: &mut Request,
    julian_day: i64,
    calendar: i64,
) -> Result<Owner<PhpArray>, Thrown> {
    let cal = Calendar::argument(req, calendar, 2)?;
    let jd = JulianDay(julian_day);
    let date = cal.date(jd);
    let mut array = PhpArray::new(req)?;
    set_string(req, &mut array, "date", &sdn::render(date))?;
    let Date { year, month, day } = date.unwrap_or(Date {
        year: 0,
        month: 0,
        day: 0,
    });
    for (name, n) in [("month", month), ("day", day), ("year", year)] {
        set(req, &mut array, name, Owner::int(i64::from(n)))?;
    }
    if cal == Calendar::Jewish && date.is_none() {
        set(req, &mut array, "dow", Owner::null())?;
        set_string(req, &mut array, "abbrevdayname", "")?;
        set_string(req, &mut array, "dayname", "")?;
    } else {
        let dow = jd.day_of_week();
        set(
            req,
            &mut array,
            "dow",
            Owner::int(i64::try_from(dow).unwrap_or(0)),
        )?;
        set_string(req, &mut array, "abbrevdayname", sdn::DAY_NAME_SHORT[dow])?;
        set_string(req, &mut array, "dayname", sdn::DAY_NAME_LONG[dow])?;
    }
    let (short, long) = match (cal, date) {
        (Calendar::Jewish, Some(date)) => {
            let names = jewish::month_names(date.year);
            (names, names)
        }
        (Calendar::Jewish, None) => (&[][..], &[][..]),
        _ => {
            let (_, _, short, long) = cal.months();
            (short, long)
        }
    };
    set_string(req, &mut array, "abbrevmonth", sdn::month_name(short, date))?;
    set_string(req, &mut array, "monthname", sdn::month_name(long, date))?;
    Ok(array)
}

/// `jdtogregorian(int $julian_day): string`, `month/day/year`.
///
/// ```php
/// var_dump(jdtogregorian(2440588), jdtogregorian(1), jdtogregorian(0), jdtogregorian(536838866), jdtogregorian(536838867), jdtogregorian(PHP_INT_MAX));
/// ```
/// ```expect
/// string(8) "1/1/1970"
/// string(11) "11/25/-4714"
/// string(5) "0/0/0"
/// string(13) "10/17/1465102"
/// string(13) "10/18/1465102"
/// string(5) "0/0/0"
/// ```
#[php_function]
fn jdtogregorian(req: &mut Request, julian_day: i64) -> Result<Owner<PhpString>, Thrown> {
    let date = sdn::sdn_to_gregorian(JulianDay(julian_day));
    string(req, sdn::render(date).as_bytes())
}

/// `gregoriantojd(int $month, int $day, int $year): int`, each argument
/// php's `int`.
///
/// ```php
/// var_dump(gregoriantojd(1, 1, 1970), gregoriantojd(11, 25, -4714), gregoriantojd(11, 24, -4714), gregoriantojd(2, 30, 2023), gregoriantojd(1, 1, 0), gregoriantojd(5, 5, 6000000), gregoriantojd(5, 5, 2147483647), gregoriantojd(1, 1, 4294969266));
/// ```
/// ```expect
/// int(2440588)
/// int(1)
/// int(0)
/// int(2460006)
/// int(0)
/// int(2193176185)
/// int(0)
/// int(2440588)
/// ```
#[php_function]
fn gregoriantojd(month: i64, day: i64, year: i64) -> i64 {
    count(sdn::gregorian_to_sdn(int(year), int(month), int(day)))
}

/// `jdtojulian(int $julian_day): string`, `month/day/year`.
///
/// ```php
/// var_dump(jdtojulian(2440588), jdtojulian(1), jdtojulian(-5), jdtojulian(882858043), jdtojulian(784351576));
/// ```
/// ```expect
/// string(10) "12/19/1969"
/// string(9) "1/2/-4713"
/// string(5) "0/0/0"
/// string(11) "8/3/2412421"
/// string(12) "7/31/2142725"
/// ```
#[php_function]
fn jdtojulian(req: &mut Request, julian_day: i64) -> Result<Owner<PhpString>, Thrown> {
    let date = sdn::sdn_to_julian(JulianDay(julian_day));
    string(req, sdn::render(date).as_bytes())
}

/// `juliantojd(int $month, int $day, int $year): int`, each argument
/// php's `int`.
///
/// ```php
/// var_dump(juliantojd(1, 1, 1970), juliantojd(1, 2, -4713), juliantojd(1, 1, -4713), juliantojd(13, 1, 2000), juliantojd(5, 5, 6000000000), juliantojd(5, 5, -2147483647));
/// ```
/// ```expect
/// int(2440601)
/// int(1)
/// int(0)
/// int(0)
/// int(622764916319)
/// int(0)
/// ```
#[php_function]
fn juliantojd(month: i64, day: i64, year: i64) -> i64 {
    count(sdn::julian_to_sdn(int(year), int(month), int(day)))
}

/// `jdtojewish(int $julian_day, bool $hebrew = false, int $flags = 0):
/// string`: `month/day/year`, or the day, the month and the year in
/// Hebrew letters, ISO-8859-8, with the `CAL_JEWISH_ADD_*` marks.
///
/// ```php
/// var_dump(jdtojewish(2453796), jdtojewish(347998), jdtojewish(347997), jdtojewish(324542846), jdtojewish(324542847));
/// foreach ([0, CAL_JEWISH_ADD_ALAFIM_GERESH, CAL_JEWISH_ADD_ALAFIM, CAL_JEWISH_ADD_GERESHAYIM, 14] as $flags) {
///     echo bin2hex(jdtojewish(2453796, true, $flags)), "\n";
/// }
/// echo bin2hex(jdtojewish(gregoriantojd(9, 7, 2002), true, 14)), "\n", bin2hex(jdtojewish(jewishtojd(1, 15, 5016), true, 8)), "\n";
/// try { jdtojewish(1, true); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(8) "7/1/5766"
/// string(5) "1/1/1"
/// string(5) "0/0/0"
/// string(12) "12/13/887605"
/// string(5) "0/0/0"
/// e020e0e3f820e4faf9f1e5
/// e020e0e3f820e427faf9f1e5
/// e020e0e3f820e420e0ecf4e9ed20faf9f1e5
/// e02720e0e3f820e4faf9f122e5
/// e02720e0e3f820e42720e0ecf4e9ed20faf9f122e5
/// e02720faf9f8e920e42720e0ecf4e9ed20faf9f122e2
/// e822e520faf9f8e920e4e822e6
/// Year out of range (0-9999)
/// ```
#[php_function]
fn jdtojewish(
    req: &mut Request,
    julian_day: i64,
    #[php(default = false)] hebrew: bool,
    #[php(default = 0)] flags: i64,
) -> Result<Owner<PhpString>, Thrown> {
    let date = jewish::sdn_to_jewish(JulianDay(julian_day));
    if !hebrew {
        return string(req, sdn::render(date).as_bytes());
    }
    let Some(date) = date.filter(|d| d.year <= 9999) else {
        return Err(value_error(req, "Year out of range (0-9999)"));
    };
    let flags = HebrewFlags::decode(flags);
    let text = [
        jewish::hebrew_number(date.day, flags).as_slice(),
        b" ",
        jewish::hebrew_month_name(date),
        b" ",
        &jewish::hebrew_number(date.year, flags),
    ]
    .concat();
    string(req, &text)
}

/// `jewishtojd(int $month, int $day, int $year): int`: the year must
/// fit php's `int`; the month and the day keep their low 32 bits.
///
/// ```php
/// var_dump(jewishtojd(6, 14, 5766), jewishtojd(1, 1, 1), jewishtojd(3, 1, 5784), jewishtojd(5, 1, 5784), jewishtojd(14, 1, 5784), jewishtojd(1, 1, 6000), jewishtojd(10, 6, 2147483647), jewishtojd(1218182888, 1, 1218182888), jewishtojd(1, 1, 2000000000));
/// try { jewishtojd(1, 1, PHP_INT_MIN); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2453809)
/// int(347998)
/// int(2460263)
/// int(2460321)
/// int(0)
/// int(2539110)
/// int(0)
/// int(0)
/// int(349551698)
/// jewishtojd(): Argument #3 ($year) must be between -2147483648 and 2147483647
/// ```
#[php_function]
fn jewishtojd(req: &mut Request, month: i64, day: i64, year: i64) -> Result<i64, Thrown> {
    let year = check_int(req, year, 3)?;
    Ok(count(jewish::jewish_to_sdn(year, int(month), int(day))))
}

/// `jdtofrench(int $julian_day): string`, `month/day/year`.
///
/// ```php
/// var_dump(jdtofrench(2375840), jdtofrench(2380952), jdtofrench(2375839), jdtofrench(2380953));
/// ```
/// ```expect
/// string(5) "1/1/1"
/// string(7) "13/5/14"
/// string(5) "0/0/0"
/// string(5) "0/0/0"
/// ```
#[php_function]
fn jdtofrench(req: &mut Request, julian_day: i64) -> Result<Owner<PhpString>, Thrown> {
    let date = sdn::sdn_to_french(JulianDay(julian_day));
    string(req, sdn::render(date).as_bytes())
}

/// `frenchtojd(int $month, int $day, int $year): int`.
///
/// ```php
/// var_dump(frenchtojd(1, 1, 1), frenchtojd(13, 5, 14), frenchtojd(1, 1, 15), frenchtojd(14, 1, 1), frenchtojd(1, 31, 1));
/// ```
/// ```expect
/// int(2375840)
/// int(2380952)
/// int(0)
/// int(0)
/// int(0)
/// ```
#[php_function]
fn frenchtojd(month: i64, day: i64, year: i64) -> i64 {
    count(sdn::french_to_sdn(int(year), int(month), int(day)))
}

/// `jddayofweek(int $julian_day, int $mode = CAL_DOW_DAYNO): int|string`:
/// the day's number, 0 for Sunday, or its name; any other `$mode` is
/// the number.
///
/// ```php
/// var_dump(jddayofweek(2440588), jddayofweek(2440588, CAL_DOW_LONG), jddayofweek(2440588, CAL_DOW_SHORT), jddayofweek(-3, 9), jddayofweek(PHP_INT_MAX, 1), jddayofweek(PHP_INT_MIN, 2));
/// ```
/// ```expect
/// int(4)
/// string(8) "Thursday"
/// string(3) "Thu"
/// int(5)
/// string(6) "Monday"
/// string(3) "Sun"
/// ```
#[php_function]
fn jddayofweek(
    req: &mut Request,
    julian_day: i64,
    #[php(default = CAL_DOW_DAYNO)] mode: i64,
) -> Result<Union<i64, Owner<PhpString>>, Thrown> {
    let day = JulianDay(julian_day).day_of_week();
    Ok(match mode {
        CAL_DOW_LONG => Union::B(string(req, sdn::DAY_NAME_LONG[day].as_bytes())?),
        CAL_DOW_SHORT => Union::B(string(req, sdn::DAY_NAME_SHORT[day].as_bytes())?),
        _ => Union::A(i64::try_from(day).unwrap_or(0)),
    })
}

/// `jdmonthname(int $julian_day, int $mode): string`: the month's name
/// in the calendar `$mode` names by its `CAL_MONTH_*`; any other
/// value is the Gregorian abbreviation.
///
/// ```php
/// foreach ([0, 1, 2, 3, 4, 5, 99] as $mode) { var_dump(jdmonthname(2453796, $mode)); }
/// var_dump(jdmonthname(0, CAL_MONTH_JEWISH), jdmonthname(0, CAL_MONTH_FRENCH));
/// ```
/// ```expect
/// string(3) "Mar"
/// string(5) "March"
/// string(3) "Feb"
/// string(8) "February"
/// string(4) "Adar"
/// string(0) ""
/// string(3) "Mar"
/// string(0) ""
/// string(0) ""
/// ```
#[php_function]
fn jdmonthname(req: &mut Request, julian_day: i64, mode: i64) -> Result<Owner<PhpString>, Thrown> {
    let jd = JulianDay(julian_day);
    let name = match mode {
        CAL_MONTH_GREGORIAN_LONG => {
            sdn::month_name(&sdn::MONTH_NAME_LONG, sdn::sdn_to_gregorian(jd))
        }
        CAL_MONTH_JULIAN_SHORT => sdn::month_name(&sdn::MONTH_NAME_SHORT, sdn::sdn_to_julian(jd)),
        CAL_MONTH_JULIAN_LONG => sdn::month_name(&sdn::MONTH_NAME_LONG, sdn::sdn_to_julian(jd)),
        CAL_MONTH_JEWISH => {
            let date = jewish::sdn_to_jewish(jd);
            date.map_or("", |d| sdn::month_name(jewish::month_names(d.year), date))
        }
        CAL_MONTH_FRENCH => sdn::month_name(&sdn::FRENCH_MONTH_NAME, sdn::sdn_to_french(jd)),
        _ => sdn::month_name(&sdn::MONTH_NAME_SHORT, sdn::sdn_to_gregorian(jd)),
    };
    string(req, name.as_bytes())
}
