//! `easter.c`: the day of Easter by Simon Kershaw's reckoning, Julian
//! or Gregorian as the year and the method say, and `easter_date`'s
//! midnight of that day in the platform's local time ([`Zone`]).

use php_ext::attr::php_function;
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;

use crate::constants::{CAL_EASTER_ALWAYS_GREGORIAN, CAL_EASTER_ALWAYS_JULIAN, CAL_EASTER_ROMAN};
use crate::zone::Zone;

/// The largest year either function takes, `(ZEND_LONG_MAX / 5) * 4`,
/// under which php's reckoning stays in a `zend_long`.
const MAX_YEAR: i64 = (i64::MAX / 5) * 4;

/// `$mode`, the `CAL_EASTER_*` method, decoded once; any other value
/// reckons as `CAL_EASTER_DEFAULT` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    /// `CAL_EASTER_DEFAULT`: Julian through 1752, when Britain changed.
    Default,
    /// `CAL_EASTER_ROMAN`: Julian through 1582, when Rome changed.
    Roman,
    /// `CAL_EASTER_ALWAYS_GREGORIAN`.
    AlwaysGregorian,
    /// `CAL_EASTER_ALWAYS_JULIAN`.
    AlwaysJulian,
}

impl Method {
    const fn decode(mode: i64) -> Method {
        match mode {
            CAL_EASTER_ROMAN => Method::Roman,
            CAL_EASTER_ALWAYS_GREGORIAN => Method::AlwaysGregorian,
            CAL_EASTER_ALWAYS_JULIAN => Method::AlwaysJulian,
            _ => Method::Default,
        }
    }

    /// Whether `year` is reckoned in the Julian calendar.
    const fn julian(self, year: i64) -> bool {
        match self {
            Method::Default => year <= 1752,
            Method::Roman => year <= 1582,
            Method::AlwaysGregorian => false,
            Method::AlwaysJulian => true,
        }
    }
}

/// Easter as the days after March 21, php's arithmetic, which wraps
/// in a `zend_long` for the largest years as the C compiles.
const fn days_after_march_21(year: i64, method: Method) -> i64 {
    let golden = (year % 19) + 1;
    let (dom, pfm) = if method.julian(year) {
        // The "Dominical number", then the uncorrected Paschal full moon.
        let dom = year.wrapping_add(year / 4).wrapping_add(5) % 7;
        (dom, (3 - (11 * golden) - 7) % 30)
    } else {
        let dom = year
            .wrapping_add(year / 4)
            .wrapping_sub(year / 100)
            .wrapping_add(year / 400)
            % 7;
        let solar = (year - 1600) / 100 - (year - 1600) / 400;
        let lunar = (((year - 1400) / 100) * 8) / 25;
        (dom, (3 - (11 * golden) + solar - lunar) % 30)
    };
    let dom = if dom < 0 { dom + 7 } else { dom };
    let mut pfm = if pfm < 0 { pfm + 30 } else { pfm };
    // The corrected Paschal full moon.
    if pfm == 29 || (pfm == 28 && golden > 11) {
        pfm -= 1;
    }
    let tmp = (4 - pfm - dom) % 7;
    let tmp = if tmp < 0 { tmp + 7 } else { tmp };
    pfm + tmp + 1
}

/// `$year`: the one given, or the current local year (1900 where
/// `localtime` fails), held to `1..=MAX_YEAR`.
fn year_argument(req: &mut Request, year: Option<i64>) -> Result<i64, Thrown> {
    let year = year.unwrap_or_else(|| {
        let (year, _, _) = Zone::of(req).date(crate::unix::now());
        if i32::try_from(year - 1900).is_ok() {
            year
        } else {
            1900
        }
    });
    if year <= 0 || year > MAX_YEAR {
        let tail = format!("be between 1 and {MAX_YEAR}");
        return Err(arg_value_error(req, 1, &tail));
    }
    Ok(year)
}

/// `easter_date(?int $year = null, int $mode = CAL_EASTER_DEFAULT):
/// int`: the timestamp of local midnight on Easter, `mktime`'s answer
/// under the request's `TZ`, for a year from 1970 to 2,000,000,000 (the
/// 64-bit build's range).
///
/// ```php
/// putenv('TZ=UTC');
/// var_dump(easter_date(2000), easter_date(2047), easter_date(1970, CAL_EASTER_ALWAYS_JULIAN), easter_date(2000000000), is_int(easter_date()));
/// putenv('TZ=America/New_York'); var_dump(easter_date(2024), easter_date(2500));
/// putenv('TZ=Australia/Sydney'); var_dump(easter_date(2024));
/// putenv('TZ=JST-9'); var_dump(easter_date(2024));
/// putenv('TZ=Foo/Bar'); var_dump(easter_date(2024));
/// foreach ([1969, 2000000001, 0, PHP_INT_MAX] as $year) {
///     try { easter_date($year); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// int(956448000)
/// int(2438812800)
/// int(8812800)
/// int(63113841842544000)
/// bool(true)
/// int(1711857600)
/// int(16734484800)
/// int(1711803600)
/// int(1711810800)
/// int(1711843200)
/// easter_date(): Argument #1 ($year) must be a year after 1970 (inclusive)
/// easter_date(): Argument #1 ($year) must be a year before 2.000.000.000 (inclusive)
/// easter_date(): Argument #1 ($year) must be between 1 and 7378697629483820644
/// easter_date(): Argument #1 ($year) must be between 1 and 7378697629483820644
/// ```
#[php_function]
pub(crate) fn easter_date(
    req: &mut Request,
    #[php(default = null)] year: Option<i64>,
    #[php(default = CAL_EASTER_DEFAULT)] mode: i64,
) -> Result<i64, Thrown> {
    let year = year_argument(req, year)?;
    if year < 1970 {
        return Err(arg_value_error(req, 1, "be a year after 1970 (inclusive)"));
    }
    if year > 2_000_000_000 {
        return Err(arg_value_error(
            req,
            1,
            "be a year before 2.000.000.000 (inclusive)",
        ));
    }
    let easter = days_after_march_21(year, Method::decode(mode));
    let (month, day) = if easter < 11 {
        (3, easter + 21)
    } else {
        (4, easter - 10)
    };
    Ok(Zone::of(req).midnight(year, month, day))
}

/// `easter_days(?int $year = null, int $mode = CAL_EASTER_DEFAULT):
/// int`: the days after March 21 that Easter falls, Julian through 1752
/// unless `$mode` says otherwise.
///
/// ```php
/// var_dump(easter_days(1999), easter_days(1492), easter_days(1600), easter_days(1600, CAL_EASTER_ROMAN), easter_days(2000, CAL_EASTER_ALWAYS_JULIAN), easter_days(1000, CAL_EASTER_ALWAYS_GREGORIAN), easter_days(2000, 42), easter_days(intdiv(PHP_INT_MAX, 5) * 4), is_int(easter_days()));
/// foreach ([0, -1, PHP_INT_MAX] as $year) {
///     try { easter_days($year); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// int(14)
/// int(32)
/// int(2)
/// int(12)
/// int(27)
/// int(9)
/// int(33)
/// int(3)
/// bool(true)
/// easter_days(): Argument #1 ($year) must be between 1 and 7378697629483820644
/// easter_days(): Argument #1 ($year) must be between 1 and 7378697629483820644
/// easter_days(): Argument #1 ($year) must be between 1 and 7378697629483820644
/// ```
#[php_function]
pub(crate) fn easter_days(
    req: &mut Request,
    #[php(default = null)] year: Option<i64>,
    #[php(default = CAL_EASTER_DEFAULT)] mode: i64,
) -> Result<i64, Thrown> {
    let year = year_argument(req, year)?;
    Ok(days_after_march_21(year, Method::decode(mode)))
}
