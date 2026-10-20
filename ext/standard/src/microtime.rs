//! `Z/ext/standard/microtime.c`: `microtime` and `gettimeofday` over
//! the wall clock, and `getrusage` over the C library's `getrusage(2)`
//! (`crate::ffi::host`).
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{OrFalse, Union};
use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString};

use crate::ffi::{Who, rusage};
use crate::uniqid::now;

/// `MICRO_IN_SEC`.
const MICRO_IN_SEC: f64 = 1_000_000.0;

/// `getrusage(int $mode = 0): array|false`, php's
/// `PHP_FUNCTION(getrusage)`: the process's resource usage, or its
/// children's for a `$mode` of `1`, as the seventeen fields php adds under
/// their C names; `false` where the call fails. The numbers are the
/// host's, so the witness pins the shape.
///
/// ```php
/// $u = getrusage(); var_dump(array_keys($u), count(array_filter($u, "is_int")), array_keys(getrusage(1)) === array_keys($u), gettype(getrusage(-1)));
/// ```
/// ```expect
/// array(17) {
///   [0]=>
///   string(10) "ru_oublock"
///   [1]=>
///   string(10) "ru_inblock"
///   [2]=>
///   string(9) "ru_msgsnd"
///   [3]=>
///   string(9) "ru_msgrcv"
///   [4]=>
///   string(9) "ru_maxrss"
///   [5]=>
///   string(8) "ru_ixrss"
///   [6]=>
///   string(8) "ru_idrss"
///   [7]=>
///   string(9) "ru_minflt"
///   [8]=>
///   string(9) "ru_majflt"
///   [9]=>
///   string(11) "ru_nsignals"
///   [10]=>
///   string(8) "ru_nvcsw"
///   [11]=>
///   string(9) "ru_nivcsw"
///   [12]=>
///   string(8) "ru_nswap"
///   [13]=>
///   string(16) "ru_utime.tv_usec"
///   [14]=>
///   string(15) "ru_utime.tv_sec"
///   [15]=>
///   string(16) "ru_stime.tv_usec"
///   [16]=>
///   string(15) "ru_stime.tv_sec"
/// }
/// int(17)
/// bool(true)
/// string(5) "array"
/// ```
#[php_function]
fn getrusage(
    req: &mut Request,
    #[php(default = 0)] mode: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let who = match mode {
        1 => Who::Children,
        _ => Who::Process,
    };
    let Some(fields) = rusage(who) else {
        return Ok(OrFalse::False);
    };
    let mut usage = PhpArray::new(req)?;
    for (name, value) in fields {
        usage.set_str(req, name.as_bytes(), Owner::int(value))?;
    }
    Ok(OrFalse::Value(usage))
}

/// The time as a double, `tv_sec + tv_usec / MICRO_IN_SEC`.
fn as_float(sec: u64, usec: u32) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let sec = sec as f64;
    sec + f64::from(usec) / MICRO_IN_SEC
}

/// `microtime(bool $as_float = false): string|float`: `"%.8F %ld"` of
/// the microseconds as a fraction and the seconds, or the time as a
/// float.
///
/// ```php
/// $s = microtime(); $f = microtime(true); $parts = explode(" ", $s);
/// var_dump(count($parts), strlen($parts[0]), substr($parts[0], 0, 2), is_float($f), $f >= (float) $parts[1], $f - (float) $parts[1] < 5);
/// ```
/// ```expect
/// int(2)
/// int(10)
/// string(2) "0."
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
///
/// ```php
/// echo new ReflectionFunction('microtime');
/// ```
/// ```expect
/// Function [ <internal:standard> function microtime ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <optional> bool $as_float = false ]
///   }
///   - Return [ string|float ]
/// }
/// ```
#[php_function]
fn microtime(
    req: &mut Request,
    #[php(default = false)] as_float: bool,
) -> Result<Union<Owner<PhpString>, f64>, Thrown> {
    let (sec, usec) = now();
    if as_float {
        return Ok(Union::B(self::as_float(sec, usec)));
    }
    let text = format!("{:.8} {sec}", f64::from(usec) / MICRO_IN_SEC);
    PhpString::new(req, text.as_bytes()).map(Union::A)
}

/// `gettimeofday(bool $as_float = false): array|float`: `sec`, `usec`,
/// and `minuteswest` and `dsttime` of the default zone's offset at `sec`,
/// read through ext/date's `get_timezone_info()` and
/// `timelib_get_time_zone_info` as `_php_gettimeofday` reads them, or the
/// time as a float.
///
/// ```php
/// $t = gettimeofday(); $f = gettimeofday(true);
/// foreach ($t as $k => $v) { echo $k, " ", is_int($v) ? "int" : "other", "\n"; }
/// var_dump($t["minuteswest"], $t["dsttime"], $t["usec"] < 1000000, is_float($f), $f - $t["sec"] < 5);
/// date_default_timezone_set('Asia/Kolkata'); $t = gettimeofday(); var_dump($t["minuteswest"], $t["dsttime"]);
/// date_default_timezone_set('America/Sao_Paulo'); $t = gettimeofday(); var_dump($t["minuteswest"]);
/// ```
/// ```expect
/// sec int
/// usec int
/// minuteswest int
/// dsttime int
/// int(0)
/// int(0)
/// bool(true)
/// bool(true)
/// bool(true)
/// int(-330)
/// int(0)
/// int(180)
/// ```
///
/// ```php
/// echo new ReflectionFunction('gettimeofday');
/// ```
/// ```expect
/// Function [ <internal:standard> function gettimeofday ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <optional> bool $as_float = false ]
///   }
///   - Return [ array|float ]
/// }
/// ```
///
/// Its peak is php's: timelib's offset, and its abbreviation, stand
/// while the array is built.
///
/// ```php
/// date_default_timezone_set('Europe/Amsterdam');
/// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
/// pk(fn () => gettimeofday());
/// ```
/// ```expect capture
/// 560 544
/// ```
#[php_function]
fn gettimeofday(
    req: &mut Request,
    #[php(default = false)] as_float: bool,
) -> Result<Union<Owner<PhpArray>, f64>, Thrown> {
    let (sec, usec) = now();
    if as_float {
        return Ok(Union::B(self::as_float(sec, usec)));
    }
    let sec = i64::try_from(sec).unwrap_or(i64::MAX);
    // timelib's offset stands while the array is built, as php frees it
    // after the last `add_assoc_long`.
    let zone = php_ext_date::get_timezone_info(req)?;
    let array = zone.offset_at(req, sec, |req, offset| {
        PhpArray::filled(req, |req, array| {
            let rows: [(&[u8], i64); 4] = [
                (b"sec", sec),
                (b"usec", i64::from(usec)),
                (b"minuteswest", i64::from(-offset.offset() / 60)),
                (b"dsttime", i64::from(offset.is_dst())),
            ];
            for (name, value) in rows {
                array.set_str(req, name, Owner::int(value))?;
            }
            Ok(())
        })
    })?;
    Ok(Union::A(array))
}
