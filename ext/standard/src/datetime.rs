//! `Z/ext/standard/datetime.c`: `strptime`, deprecated since 8.2, over
//! the host's `strptime(3)`.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::decl::{Attribute, AttributeArgument, ConstExpr, FunctionDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value};

use crate::output::Entry;
use crate::stub::stub;
use crate::{argument_count_between, string_argument};

/// The message of the stub's `#[\Deprecated]`.
const MESSAGE: &str = "use date_parse_from_format() (for locale-independent parsing), or IntlDateFormatter::parse() (for locale-dependent parsing) instead";

/// The one native, under the stub's `#[\Deprecated(since: '8.2', ...)]`.
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
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let argument = |name: &[u8], value: &str| AttributeArgument {
        name: Some(Name(name.into())),
        value: ConstExpr::Str(value.as_bytes().into()),
    };
    let decl = FunctionDecl {
        attributes: vec![Attribute {
            name: Name(b"Deprecated".as_slice().into()),
            arguments: vec![argument(b"since", "8.2"), argument(b"message", MESSAGE)],
        }],
        ..stub("strptime(string $timestamp, string $format): array|false")
    };
    vec![(decl, strptime as Entry)]
}

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
fn strptime(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let timestamp = string_argument(req, 1)?;
    let format = string_argument(req, 2)?;
    let c_string = |bytes: &[u8]| bytes.split(|c| *c == 0).next().unwrap_or(&[]).to_vec();
    let Some((fields, unparsed)) = crate::ffi::strptime(
        &c_string(timestamp.as_bytes()),
        &c_string(format.as_bytes()),
    ) else {
        return Ok(Value::bool(false));
    };
    let mut array = PhpArray::new(req)?;
    let names = [
        "tm_sec", "tm_min", "tm_hour", "tm_mday", "tm_mon", "tm_year", "tm_wday", "tm_yday",
    ];
    for (name, value) in names.iter().zip(fields) {
        let key = Key::Str(PhpString::new(req, name.as_bytes())?);
        array.set(req, key, Value::int(i64::from(value)))?;
    }
    let key = Key::Str(PhpString::new(req, b"unparsed")?);
    let unparsed = Value::string(PhpString::new(req, &unparsed)?);
    array.set(req, key, unparsed)?;
    Ok(Value::array(array))
}
