//! `Z/ext/standard/crc32.c`: `crc32` over the IEEE 802.3 polynomial
//! php's `crc32tab` tabulates, reflected, with the initial and final
//! inversion, answered as an unsigned 32-bit value in a `zend_long`.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::decl::FunctionDecl;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Value;

use crate::output::Entry;
use crate::stub::stub;
use crate::{argument_count_between, string_argument};

/// The one.
///
/// ```php
/// echo new ReflectionFunction('crc32');
/// ```
/// ```expect
/// Function [ <internal:standard> function crc32 ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ int ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![(stub("crc32(string $string): int"), crc32 as Entry)]
}

/// `crc32tab`: the reflected table of `0xEDB88320`.
const fn table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n: u32 = 0;
    while n < 256 {
        let mut c = n;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n as usize] = c;
        n += 1;
    }
    table
}

/// The table, built at compile time.
const CRC32TAB: [u32; 256] = table();

/// `php_crc32_bulk_init`, `php_crc32_bulk_update` and
/// `php_crc32_bulk_end`.
fn crc(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in data {
        let low = crc.to_le_bytes()[0];
        crc = (crc >> 8) ^ CRC32TAB[usize::from(low ^ byte)];
    }
    !crc
}

/// `crc32(string $string): int`.
///
/// ```php
/// var_dump(crc32(""), crc32("The quick brown fox jumped over the lazy dog."), crc32("a"), crc32(str_repeat("\xff", 100)), sprintf("%u", crc32("hello")));
/// ```
/// ```expect
/// int(0)
/// int(2191738434)
/// int(3904355907)
/// int(64128641)
/// string(9) "907060870"
/// ```
fn crc32(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    Ok(Value::int(i64::from(crc(string.as_bytes()))))
}
