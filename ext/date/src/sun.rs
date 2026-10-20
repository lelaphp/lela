//! `date_sunrise`, `date_sunset` and `date_sun_info` over timelib's
//! `astro.c`.

use php_ext::abi::{OrFalse, Passed, Union};
use php_ext::attr::php_function;
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use crate::constants::{
    SUNFUNCS_RET_DOUBLE as RET_DOUBLE, SUNFUNCS_RET_STRING as RET_STRING,
    SUNFUNCS_RET_TIMESTAMP as RET_TIMESTAMP,
};
use crate::ffi::Time;
use crate::lent::lent;
use crate::obj::set_str;
use crate::zone::default_zone;

/// `string|int|float|false`, what `date_sunrise` and `date_sunset` answer.
type SunTime = Union<Owner<PhpString>, Union<i64, OrFalse<f64>>>;

/// `INI_FLT(name)`: `zend_strtod` of the entry, the longest prefix that
/// reads as a number and zero where none does.
fn ini_float(req: &Request, name: &[u8]) -> f64 {
    let text = req.ini().get(name).unwrap_or_default();
    let text = String::from_utf8_lossy(text);
    let text = text.trim_start();
    let mut end = 0;
    let mut best = 0.0;
    for (at, _) in text.char_indices().skip(1).chain([(text.len(), ' ')]) {
        if let Ok(n) = text[..at].parse::<f64>() {
            best = n;
            end = at;
        }
    }
    if end == 0 { 0.0 } else { best }
}

/// `$returnFormat`, `SUNFUNCS_RET_STRING` where the call gave none: the
/// C default, so an omitted argument reads no deprecated constant.
fn format_or_string(given: Passed<i64>) -> i64 {
    match given {
        Passed::Given(format) => format,
        Passed::Omitted => RET_STRING,
    }
}

/// The arguments `date_sunrise` and `date_sunset` share, as the stub
/// names them: the time, the format, then the four `?float`s.
#[derive(Clone, Copy)]
struct SunArguments {
    time: i64,
    format: i64,
    latitude: Option<f64>,
    longitude: Option<f64>,
    zenith: Option<f64>,
    gmt_offset: Option<f64>,
}

/// `php_do_date_sunrise_sunset(INTERNAL_FUNCTION_PARAM_PASSTHRU, calc_sunset)`.
fn sunrise_sunset(req: &mut Request, sunset: bool, given: SunArguments) -> Result<SunTime, Thrown> {
    let SunArguments {
        time,
        format,
        latitude,
        longitude,
        zenith,
        gmt_offset,
    } = given;
    let latitude = latitude.unwrap_or_else(|| ini_float(req, b"date.default_latitude"));
    let longitude = longitude.unwrap_or_else(|| ini_float(req, b"date.default_longitude"));
    let zenith = zenith.unwrap_or_else(|| {
        if sunset {
            ini_float(req, b"date.sunset_zenith")
        } else {
            ini_float(req, b"date.sunrise_zenith")
        }
    });
    if ![RET_TIMESTAMP, RET_STRING, RET_DOUBLE].contains(&format) {
        return Err(arg_value_error(
            req,
            2,
            "be one of SUNFUNCS_RET_TIMESTAMP, SUNFUNCS_RET_STRING, or SUNFUNCS_RET_DOUBLE",
        ));
    }
    let altitude = 90.0 - zenith;
    if !latitude.is_finite() || !longitude.is_finite() {
        return Ok(Union::B(Union::B(OrFalse::False)));
    }
    let zone = default_zone(req)?;
    let (gmt_offset, (rs, h_rise, h_set, rise, set, _)) = lent(req, |l| {
        let mut t = Time::new(l);
        t.set_zone_id(zone);
        // php reads the offset before the time is set, so at the epoch.
        #[allow(clippy::cast_precision_loss)]
        let gmt_offset = gmt_offset.unwrap_or_else(|| t.current_offset(l) as f64 / 3600.0);
        t.unixtime2local(l, time);
        (
            gmt_offset,
            t.rise_set(l, longitude, latitude, altitude, true),
        )
    })?;
    if rs != 0 {
        return Ok(Union::B(Union::B(OrFalse::False)));
    }
    if format == RET_TIMESTAMP {
        return Ok(Union::B(Union::A(if sunset { set } else { rise })));
    }
    let mut n = if sunset { h_set } else { h_rise } + gmt_offset;
    if !(0.0..=24.0).contains(&n) {
        n -= (n / 24.0).floor() * 24.0;
    }
    if !(0.0..=24.0).contains(&n) {
        return Ok(Union::B(Union::B(OrFalse::False)));
    }
    if format == RET_STRING {
        #[allow(clippy::cast_possible_truncation)]
        let hours = n as i32;
        #[allow(clippy::cast_possible_truncation)]
        let minutes = (60.0 * (n - f64::from(hours))) as i32;
        return PhpString::new(req, format!("{hours:02}:{minutes:02}").as_bytes()).map(Union::A);
    }
    Ok(Union::B(Union::B(OrFalse::Value(n))))
}

/// `date_sunrise(int $timestamp, int $returnFormat = SUNFUNCS_RET_STRING,
/// ?float $latitude = null, ?float $longitude = null, ?float $zenith =
/// null, ?float $utcOffset = null): string|int|float|false`, deprecated
/// since 8.1.
///
/// ```php
/// date_default_timezone_set('Europe/Amsterdam');
/// $t = 1700000000;
/// var_dump(@date_sunrise($t, SUNFUNCS_RET_STRING, 52.37, 4.89), @date_sunrise($t, SUNFUNCS_RET_TIMESTAMP, 52.37, 4.89), @date_sunrise($t, SUNFUNCS_RET_DOUBLE, 52.37, 4.89, 90, 1));
/// var_dump(date_sunrise($t, SUNFUNCS_RET_STRING, 89.9, 0));
/// try { @date_sunrise($t, 7); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(5) "07:57"
/// int(1699945056)
/// float(8.064960453025263)
///
/// Deprecated: Constant SUNFUNCS_RET_STRING is deprecated since 8.4, as date_sunrise() and date_sunset() were deprecated in 8.1 in %s on line 5
///
/// Deprecated: Function date_sunrise() is deprecated since 8.1, use date_sun_info() instead in %s on line 5
/// bool(false)
/// date_sunrise(): Argument #2 ($returnFormat) must be one of SUNFUNCS_RET_TIMESTAMP, SUNFUNCS_RET_STRING, or SUNFUNCS_RET_DOUBLE
/// ```
#[php_function(deprecated(since = "8.1", message = "use date_sun_info() instead"))]
fn date_sunrise(
    req: &mut Request,
    timestamp: i64,
    #[php(default = SUNFUNCS_RET_STRING)] returnFormat: Passed<i64>,
    #[php(default = null)] latitude: Option<f64>,
    #[php(default = null)] longitude: Option<f64>,
    #[php(default = null)] zenith: Option<f64>,
    #[php(default = null)] utcOffset: Option<f64>,
) -> Result<SunTime, Thrown> {
    let given = SunArguments {
        time: timestamp,
        format: format_or_string(returnFormat),
        latitude,
        longitude,
        zenith,
        gmt_offset: utcOffset,
    };
    sunrise_sunset(req, false, given)
}

/// `date_sunset(...)`, `date_sunrise`'s twin, deprecated since 8.1.
///
/// ```php
/// date_default_timezone_set('America/New_York');
/// var_dump(@date_sunset(1700000000, SUNFUNCS_RET_STRING, 40.71, -74.0), @date_sunset(1700000000, SUNFUNCS_RET_TIMESTAMP, 40.71, -74.0), @date_sunset(1700000000));
/// ```
/// ```expect
/// string(5) "16:40"
/// int(1699998043)
/// string(5) "09:42"
/// ```
#[php_function(deprecated(since = "8.1", message = "use date_sun_info() instead"))]
fn date_sunset(
    req: &mut Request,
    timestamp: i64,
    #[php(default = SUNFUNCS_RET_STRING)] returnFormat: Passed<i64>,
    #[php(default = null)] latitude: Option<f64>,
    #[php(default = null)] longitude: Option<f64>,
    #[php(default = null)] zenith: Option<f64>,
    #[php(default = null)] utcOffset: Option<f64>,
) -> Result<SunTime, Thrown> {
    let given = SunArguments {
        time: timestamp,
        format: format_or_string(returnFormat),
        latitude,
        longitude,
        zenith,
        gmt_offset: utcOffset,
    };
    sunrise_sunset(req, true, given)
}

/// One pair of `date_sun_info`'s rows: `false` for a body always below
/// the altitude, `true` always above, the two times otherwise.
fn add_pair(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    rs: i32,
    names: (&[u8], &[u8]),
    times: (i64, i64),
) -> Result<(), Thrown> {
    let (a, b) = match rs {
        -1 => (Value::bool(false), Value::bool(false)),
        1 => (Value::bool(true), Value::bool(true)),
        _ => (Value::int(times.0), Value::int(times.1)),
    };
    set_str(req, array, names.0, Owner::take(req, a))?;
    set_str(req, array, names.1, Owner::take(req, b))
}

/// `date_sun_info(int $timestamp, float $latitude, float $longitude):
/// array`: sunrise, sunset, transit and the three twilights.
///
/// ```php
/// date_default_timezone_set('UTC');
/// var_dump(date_sun_info(1700000000, 52.37, 4.89));
/// var_dump(date_sun_info(1719835200, 78.2, 15.6));
/// try { date_sun_info(0, INF, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(9) {
///   ["sunrise"]=>
///   int(1699945169)
///   ["sunset"]=>
///   int(1699977009)
///   ["transit"]=>
///   int(1699961089)
///   ["civil_twilight_begin"]=>
///   int(1699942918)
///   ["civil_twilight_end"]=>
///   int(1699979259)
///   ["nautical_twilight_begin"]=>
///   int(1699940432)
///   ["nautical_twilight_end"]=>
///   int(1699981745)
///   ["astronomical_twilight_begin"]=>
///   int(1699938032)
///   ["astronomical_twilight_end"]=>
///   int(1699984146)
/// }
/// array(9) {
///   ["sunrise"]=>
///   bool(true)
///   ["sunset"]=>
///   bool(true)
///   ["transit"]=>
///   int(1719831695)
///   ["civil_twilight_begin"]=>
///   bool(true)
///   ["civil_twilight_end"]=>
///   bool(true)
///   ["nautical_twilight_begin"]=>
///   bool(true)
///   ["nautical_twilight_end"]=>
///   bool(true)
///   ["astronomical_twilight_begin"]=>
///   bool(true)
///   ["astronomical_twilight_end"]=>
///   bool(true)
/// }
/// date_sun_info(): Argument #2 ($latitude) must be finite
/// ```
#[php_function]
fn date_sun_info(
    req: &mut Request,
    timestamp: i64,
    latitude: f64,
    longitude: f64,
) -> Result<Owner<PhpArray>, Thrown> {
    let time = timestamp;
    if !latitude.is_finite() {
        return Err(arg_value_error(req, 2, "be finite"));
    }
    if !longitude.is_finite() {
        return Err(arg_value_error(req, 3, "be finite"));
    }
    let zone = default_zone(req)?;
    let [first, civil, nautical, astronomical] = lent(req, |l| {
        let mut t = Time::new(l);
        t.set_zone_id(zone);
        t.unixtime2local(l, time);
        [
            (-35.0 / 60.0, true),
            (-6.0, false),
            (-12.0, false),
            (-18.0, false),
        ]
        .map(|(altitude, upper)| t.rise_set(l, longitude, latitude, altitude, upper))
    })?;
    let mut array = PhpArray::new(req)?;
    let (rs, _, _, rise, set, transit) = first;
    add_pair(req, &mut array, rs, (b"sunrise", b"sunset"), (rise, set))?;
    set_str(req, &mut array, b"transit", Owner::int(transit))?;
    for ((rs, _, _, rise, set, _), begin, end) in [
        (
            civil,
            &b"civil_twilight_begin"[..],
            &b"civil_twilight_end"[..],
        ),
        (
            nautical,
            b"nautical_twilight_begin",
            b"nautical_twilight_end",
        ),
        (
            astronomical,
            b"astronomical_twilight_begin",
            b"astronomical_twilight_end",
        ),
    ] {
        add_pair(req, &mut array, rs, (begin, end), (rise, set))?;
    }
    Ok(array)
}
