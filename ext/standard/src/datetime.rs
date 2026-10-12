//! `Z/ext/standard/datetime.c`: `strptime`, deprecated since 8.2, over
//! the host's `strptime(3)`.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::OrFalse;
use php_ext::attr::php_function;
use php_ext::coerce::c_str;
use php_ext::raising::Thrown;
use php_ext::request::{Request, Zone};
use php_ext::values::{Owner, PhpArray, PhpString};

/// `strptime(string $timestamp, string $format): array|false`,
/// `PHP_FUNCTION(strptime)` (`Z/ext/standard/datetime.c:34`): the
/// deprecation, then `strptime(3)` over a zeroed `struct tm`, its eight
/// fields and what was left unparsed, or `false` where it failed; both
/// strings are read as the C strings they are.
///
/// ```php
/// var_dump(strptime("2024-03-05 10:20:30 junk", "%Y-%m-%d %H:%M:%S"), strptime("x", "%Y"));
/// ```
/// ```expect
/// Deprecated: Function strptime() is deprecated since 8.2, use date_parse_from_format() (for locale-independent parsing), or IntlDateFormatter::parse() (for locale-dependent parsing) instead in %s on line 2
///
/// Deprecated: Function strptime() is deprecated since 8.2, use date_parse_from_format() (for locale-independent parsing), or IntlDateFormatter::parse() (for locale-dependent parsing) instead in %s on line 2
/// array(9) {
///   ["tm_sec"]=>
///   int(30)
///   ["tm_min"]=>
///   int(20)
///   ["tm_hour"]=>
///   int(10)
///   ["tm_mday"]=>
///   int(5)
///   ["tm_mon"]=>
///   int(2)
///   ["tm_year"]=>
///   int(124)
///   ["tm_wday"]=>
///   int(2)
///   ["tm_yday"]=>
///   int(64)
///   ["unparsed"]=>
///   string(5) " junk"
/// }
/// bool(false)
/// ```
///
/// A `%Z` of GMT, and a `%z`, give a GMT time, which the C library
/// answers in its local time: the zone of the request's `TZ`, which
/// `putenv` moves as php's `tzset` does, and never the process's.
///
/// ```php
/// putenv("TZ=Asia/Tokyo");
/// $a = @strptime("1963-07-02 10:01:20 GMT", "%Y-%m-%d %H:%M:%S %Z");
/// $b = @strptime("1963-07-02 10:01:20 +0200", "%Y-%m-%d %H:%M:%S %z");
/// $c = @strptime("1963-07-02 10:01:20", "%Y-%m-%d %H:%M:%S");
/// echo $a["tm_hour"], " ", $a["tm_mday"], " ", $b["tm_hour"], " ", $c["tm_hour"], "\n";
/// ```
/// ```expect
/// 19 2 17 10
/// ```
///
/// The declaration, under the stub's `#[\Deprecated(since: '8.2', ...)]`.
///
/// ```php
/// echo new ReflectionFunction('strptime');
/// ```
/// ```expect
/// Function [ <internal, deprecated:standard> function strptime ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $timestamp ]
///     Parameter #1 [ <required> string $format ]
///   }
///   - Return [ array|false ]
/// }
/// ```
#[php_function(deprecated(
    since = "8.2",
    message = "use date_parse_from_format() (for locale-independent parsing), or IntlDateFormatter::parse() (for locale-dependent parsing) instead"
))]
fn strptime(
    req: &mut Request,
    timestamp: PhpString,
    format: PhpString,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let c_string = |bytes: &[u8]| c_str(bytes).to_vec();
    let Some(parsed) = crate::ffi::strptime(
        &c_string(timestamp.as_bytes()),
        &c_string(format.as_bytes()),
    ) else {
        return Ok(OrFalse::False);
    };
    // Where the C library turned a GMT time into its local time, the
    // local time is the request's, as php's is after `putenv('TZ=…')`,
    // which calls `tzset`; the process's `TZ` is not the request's.
    let fields = parsed
        .moment
        .and_then(|at| crate::ffi::civil(at.saturating_add(Zone::of(req).offset(at))))
        .unwrap_or(parsed.fields);
    let unparsed = parsed.rest;
    let mut array = PhpArray::new(req)?;
    let names = [
        "tm_sec", "tm_min", "tm_hour", "tm_mday", "tm_mon", "tm_year", "tm_wday", "tm_yday",
    ];
    for (name, value) in names.iter().zip(fields) {
        array.set_str(req, name.as_bytes(), Owner::int(i64::from(value)))?;
    }
    let unparsed = PhpString::new(req, &unparsed)?.into_value();
    array.set_str(req, b"unparsed", unparsed)?;
    Ok(OrFalse::Value(array))
}
