//! `cal_unix.c`: a timestamp's julian day by its local date, and a
//! julian day's timestamp at UTC midnight.

use php_ext::abi::OrFalse;
use php_ext::attr::php_function;
use php_ext::raising::{Thrown, arg_value_error, value_error};
use php_ext::request::Request;

use crate::{count, int, sdn};
use php_ext::request::Zone;

/// `SECS_PER_DAY`.
const SECS_PER_DAY: i64 = 24 * 3600;

/// The julian day of 1970-01-01.
const EPOCH: i64 = 2_440_588;

/// `time(NULL)`.
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `unixtojd(?int $timestamp = null): int|false`: the julian day of the
/// timestamp's local date under the request's `TZ`, now where it is
/// `null`; `false` where `localtime` cannot hold the year.
///
/// ```php
/// putenv('TZ=UTC');
/// var_dump(unixtojd(0), unixtojd(1000000000), unixtojd(1152459009), unixtojd(PHP_INT_MAX), unixtojd(67767976233316800), is_int(unixtojd()), is_int(unixtojd(null)));
/// putenv('TZ=America/New_York'); var_dump(unixtojd(1000000000), unixtojd(0));
/// try { unixtojd(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2440588)
/// int(2452162)
/// int(2453926)
/// bool(false)
/// int(0)
/// bool(true)
/// bool(true)
/// int(2452161)
/// int(2440587)
/// unixtojd(): Argument #1 ($timestamp) must be greater than or equal to 0
/// ```
#[php_function]
pub(crate) fn unixtojd(
    req: &mut Request,
    #[php(default = null)] timestamp: Option<i64>,
) -> Result<OrFalse<i64>, Thrown> {
    let ts = match timestamp {
        None => now(),
        Some(ts) if ts >= 0 => ts,
        Some(_) => return Err(arg_value_error(req, 1, "be greater than or equal to 0")),
    };
    let (year, month, day) = Zone::of(req).date(ts);
    // `localtime_r` refuses a year its `int tm_year` cannot hold; php
    // adds 1900 back in `int`.
    let Ok(tm_year) = i32::try_from(year - 1900) else {
        return Ok(OrFalse::False);
    };
    let year = tm_year.wrapping_add(1900);
    Ok(OrFalse::Value(count(sdn::gregorian_to_sdn(
        year,
        int(month),
        int(day),
    ))))
}

/// `jdtounix(int $julian_day): int`: the timestamp of the day's UTC
/// midnight, for a day from 1970-01-01 to the last a `zend_long` holds.
///
/// ```php
/// var_dump(jdtounix(2440588), jdtounix(2465712), jdtounix(intdiv(PHP_INT_MAX, 86400) + 2440588));
/// foreach ([2440587, intdiv(PHP_INT_MAX, 86400) + 2440589, PHP_INT_MIN] as $jd) {
///     try { jdtounix($jd); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// int(0)
/// int(2170713600)
/// int(9223372036854720000)
/// jday must be between 2440588 and 106751993607888
/// jday must be between 2440588 and 106751993607888
/// jday must be between 2440588 and 106751993607888
/// ```
#[php_function]
pub(crate) fn jdtounix(req: &mut Request, julian_day: i64) -> Result<i64, Thrown> {
    if julian_day < EPOCH || julian_day - EPOCH > i64::MAX / SECS_PER_DAY {
        let message = format!(
            "jday must be between {EPOCH} and {}",
            i64::MAX / SECS_PER_DAY + EPOCH
        );
        return Err(value_error(req, &message));
    }
    Ok((julian_day - EPOCH) * SECS_PER_DAY)
}
