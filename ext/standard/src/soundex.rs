//! `Z/ext/standard/soundex.c`: Knuth's soundex (TAOCP vol. 3) as php
//! computes it.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::decl::FunctionDecl;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Value;

use crate::output::Entry;
use crate::stub::stub;
use crate::zpp::string_value;
use crate::{argument_count_between, string_argument};

/// The one.
///
/// ```php
/// echo new ReflectionFunction('soundex');
/// ```
/// ```expect
/// Function [ <internal:standard> function soundex ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ string ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![(stub("soundex(string $string): string"), soundex as Entry)]
}

/// `soundex_table`: each letter's digit, zero for the vowels and H, W
/// and Y.
const TABLE: &[u8; 26] = b"\x00123\x0012\x00\x0022455\x0012623\x001\x002\x002";

/// `soundex(string $string): string`: the first letter and the digits
/// of the letters after it, a repeated digit written once and a
/// vowel between two keeping both, padded with `0` to four.
///
/// ```php
/// var_dump(soundex("Robert"), soundex("Rupert"), soundex("Tymczak"), soundex("Pfister"), soundex("Ashcraft"), soundex("lloyd"), soundex("a"), soundex(""), soundex("123"), soundex("Honeyman"));
/// ```
/// ```expect
/// string(4) "R163"
/// string(4) "R163"
/// string(4) "T522"
/// string(4) "P236"
/// string(4) "A226"
/// string(4) "L300"
/// string(4) "A000"
/// string(4) "0000"
/// string(4) "0000"
/// string(4) "H555"
/// ```
fn soundex(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    let mut out: Vec<u8> = Vec::with_capacity(4);
    let mut last: Option<u8> = None;
    for &c in string.as_bytes() {
        if out.len() >= 4 {
            break;
        }
        let c = c.to_ascii_uppercase();
        if !c.is_ascii_uppercase() {
            continue;
        }
        let code = TABLE[usize::from(c - b'A')];
        if out.is_empty() {
            out.push(c);
            last = Some(code);
        } else if Some(code) != last {
            if code != 0 {
                out.push(code);
            }
            last = Some(code);
        }
    }
    out.resize(4, b'0');
    string_value(req, &out)
}
