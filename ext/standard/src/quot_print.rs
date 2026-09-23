//! `Z/ext/standard/quot_print.c`: `quoted_printable_decode`'s own loop
//! over the C string, and `quoted_printable_encode` over
//! `php_quot_print_encode`, RFC 2045's encoding with php's 75-byte soft
//! line limit and its lookahead for a multi-byte UTF-8 lead.
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

/// The hex digits `=XX` writes.
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// `PHP_QPRINT_MAXL`.
const MAXL: u64 = 75;

/// The two, in the stub's order.
///
/// ```php
/// echo new ReflectionFunction('quoted_printable_decode'), new ReflectionFunction('quoted_printable_encode');
/// ```
/// ```expect
/// Function [ <internal:standard> function quoted_printable_decode ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function quoted_printable_encode ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ string ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            stub("quoted_printable_decode(string $string): string"),
            quoted_printable_decode as Entry,
        ),
        (
            stub("quoted_printable_encode(string $string): string"),
            quoted_printable_encode,
        ),
    ]
}

/// `php_hex2int`.
fn hex2int(c: u8) -> u8 {
    u8::try_from((c as char).to_digit(16).unwrap_or(0)).unwrap_or(0)
}

/// `quoted_printable_decode(string $string): string`: `=XX` as its
/// byte, a soft line break (`=` then blanks and a line end) dropped,
/// and every other byte kept; the string ends at its first NUL.
///
/// ```php
/// var_dump(quoted_printable_decode("a=3Db=20c"), quoted_printable_decode("soft=\r\nbreak= \t\nhere=\nx"), quoted_printable_decode("=zz=4"), quoted_printable_decode("end="), bin2hex(quoted_printable_decode("=C3=A9")), quoted_printable_decode(""), quoted_printable_decode("a\0b"));
/// ```
/// ```expect
/// string(5) "a=b c"
/// string(14) "softbreakherex"
/// string(5) "=zz=4"
/// string(3) "end"
/// string(4) "c3a9"
/// string(0) ""
/// string(1) "a"
/// ```
fn quoted_printable_decode(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    let s = string.as_bytes();
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while at(i) != 0 {
        if at(i) != b'=' {
            out.push(at(i));
            i += 1;
            continue;
        }
        if at(i + 1) != 0
            && at(i + 2) != 0
            && at(i + 1).is_ascii_hexdigit()
            && at(i + 2).is_ascii_hexdigit()
        {
            out.push((hex2int(at(i + 1)) << 4) + hex2int(at(i + 2)));
            i += 3;
            continue;
        }
        let mut k = 1;
        while at(i + k) != 0 && (at(i + k) == b' ' || at(i + k) == b'\t') {
            k += 1;
        }
        if at(i + k) == 0 {
            i += k;
        } else if at(i + k) == b'\r' && at(i + k + 1) == b'\n' {
            i += k + 2;
        } else if at(i + k) == b'\r' || at(i + k) == b'\n' {
            i += k + 1;
        } else {
            out.push(at(i));
            i += 1;
        }
    }
    string_value(req, &out)
}

/// `quoted_printable_encode(string $string): string`: control bytes,
/// `=`, bytes past 0x7f and a space before a CR as `=XX`, a CRLF kept,
/// and a soft break `=\r\n` wherever a line would pass 75 bytes, early
/// enough before a UTF-8 lead that its whole character stays on one
/// line.
///
/// ```php
/// $show = fn($s) => strtr($s, ["\r" => '\r', "\n" => '\n']);
/// var_dump(quoted_printable_encode("a=b\tc"), $show(quoted_printable_encode("line \r\nnext\r\n")), $show(quoted_printable_encode(str_repeat("x", 80))), $show(quoted_printable_encode(str_repeat("é", 30))), quoted_printable_encode(""));
/// ```
/// ```expect
/// string(9) "a=3Db=09c"
/// string(19) "line=20\r\nnext\r\n"
/// string(85) "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx=\r\nxxxxx"
/// string(190) "=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=\r\n=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=\r\n=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9=C3=A9"
/// string(0) ""
/// ```
fn quoted_printable_encode(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    let s = string.as_bytes();
    if s.is_empty() {
        return string_value(req, b"");
    }
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut out = Vec::with_capacity(s.len() * 3);
    let mut lp: u64 = 0;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        i += 1;
        let left = s.len() - i;
        if c == b'\r' && at(i) == b'\n' && left > 0 {
            out.push(b'\r');
            out.push(b'\n');
            i += 1;
            lp = 0;
            continue;
        }
        if c < 32 || c == 0x7f || c & 0x80 != 0 || c == b'=' || (c == b' ' && at(i) == b'\r') {
            lp += 3;
            if (lp > MAXL && c <= 0x7f)
                || (c > 0x7f && c <= 0xdf && lp + 3 > MAXL)
                || (c > 0xdf && c <= 0xef && lp + 6 > MAXL)
                || (c > 0xef && c <= 0xf4 && lp + 9 > MAXL)
            {
                out.extend_from_slice(b"=\r\n");
                lp = 3;
            }
            out.extend_from_slice(&[b'=', HEX[usize::from(c >> 4)], HEX[usize::from(c & 0xf)]]);
        } else {
            lp += 1;
            if lp > MAXL {
                out.extend_from_slice(b"=\r\n");
                lp = 1;
            }
            out.push(c);
        }
    }
    string_value(req, &out)
}
