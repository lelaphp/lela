//! The escapes of `string.c`: `addslashes` and `stripslashes`, and the
//! C-style pair `addcslashes` and `stripcslashes`.

use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::PhpString;

use super::str_copy;
use crate::charmask;

/// `addslashes(string $string): string`, `php_addslashes`: a backslash
/// before `'`, `"` and `\`, and a NUL as `\0`.
///
/// ```php
/// var_dump(addslashes("O'Re\"il\\ly"), addslashes("a\0b"), addslashes(""), addslashes("plain"));
/// var_dump(stripslashes("O\\'Re\\\\il\\ly"), stripslashes("a\\0b") === "a\0b", stripslashes("end\\"), stripslashes("\\\\\\"));
/// ```
/// ```expect
/// string(13) "O\'Re\"il\\ly"
/// string(4) "a\0b"
/// string(0) ""
/// string(5) "plain"
/// string(9) "O'Re\illy"
/// bool(true)
/// string(3) "end"
/// string(1) "\"
/// ```
#[php_function]
pub(super) fn addslashes(req: &mut Request, string: PhpString) -> Result<PhpString, Thrown> {
    let s = string.as_bytes();
    if !s.iter().any(|c| matches!(c, 0 | b'\'' | b'"' | b'\\')) {
        return Ok(str_copy(req, string));
    }
    let mut out = Vec::with_capacity(s.len() * 2);
    for &c in s {
        match c {
            0 => out.extend_from_slice(b"\\0"),
            b'\'' | b'"' | b'\\' => {
                out.push(b'\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    PhpString::new(req, &out)
}

/// `stripslashes(string $string): string`, `php_stripslashes`: each
/// backslash dropped and the byte after it kept, `\0` read as a NUL,
/// and a backslash that ends the string dropped.
///
/// ```php
/// var_dump(stripslashes("O\\'Re\\\\il\\ly"), bin2hex(stripslashes("a\\0b")), stripslashes("end\\"), stripslashes("\\\\\\"), stripslashes(""));
/// ```
/// ```expect
/// string(9) "O'Re\illy"
/// string(6) "610062"
/// string(3) "end"
/// string(1) "\"
/// string(0) ""
/// ```
#[php_function]
pub(super) fn stripslashes(req: &mut Request, string: PhpString) -> Result<PhpString, Thrown> {
    let s = string.as_bytes();
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' {
            i += 1;
            if let Some(&next) = s.get(i) {
                out.push(if next == b'0' { 0 } else { next });
                i += 1;
            }
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    PhpString::new(req, &out)
}

/// `addcslashes(string $string, string $characters): string`,
/// `php_addcslashes_str`: a backslash before each byte the
/// `php_charmask` set names, a control or high byte written as its C
/// escape or as three octal digits.
///
/// ```php
/// var_dump(addcslashes("foo[bar]", "A..Z"), addcslashes("zoo['.']", "z..A"), addcslashes("a\n\t\x01\xff", "\0..\37\177..\377"), addcslashes("", "a"), addcslashes("abc", ""));
/// var_dump(stripcslashes('a\nb\x41\101\q\\'), stripcslashes('\x4g'), stripcslashes('\\'), bin2hex(stripcslashes('\a\b\f\v\t\r\0')));
/// ```
/// ```expect
/// Warning: addcslashes(): Invalid '..'-range, '..'-range needs to be incrementing in %s on line 2
/// string(8) "foo[bar]"
/// string(10) "\zoo['\.']"
/// string(13) "a\n\t\001\377"
/// string(0) ""
/// string(3) "abc"
/// string(7) "a
/// bAAq\"
/// string(2) "g"
/// string(1) "\"
/// string(14) "07080c0b090d00"
/// ```
#[php_function]
pub(super) fn addcslashes(
    req: &mut Request,
    string: PhpString,
    characters: PhpString,
) -> Result<PhpString, Thrown> {
    let what = characters;
    if string.is_empty() {
        return PhpString::new(req, b"");
    }
    if what.is_empty() {
        return Ok(str_copy(req, string));
    }
    let flags = charmask(req, what.as_bytes());
    let mut out = Vec::with_capacity(string.len() * 4);
    for &c in string.as_bytes() {
        if flags[usize::from(c)] {
            if !(32..=126).contains(&c) {
                out.push(b'\\');
                match c {
                    b'\n' => out.push(b'n'),
                    b'\t' => out.push(b't'),
                    b'\r' => out.push(b'r'),
                    0x07 => out.push(b'a'),
                    0x0b => out.push(b'v'),
                    0x08 => out.push(b'b'),
                    0x0c => out.push(b'f'),
                    _ => out.extend_from_slice(format!("{c:03o}").as_bytes()),
                }
                continue;
            }
            out.push(b'\\');
        }
        out.push(c);
    }
    PhpString::new(req, &out)
}

/// `stripcslashes(string $string): string`, `php_stripcslashes`: the C
/// escapes read back, `\x` with one or two hex digits, one to three
/// octal digits, and any other escaped byte kept as itself; a backslash
/// that ends the string stays.
///
/// ```php
/// var_dump(bin2hex(stripcslashes('a\tb\x41\101\q')), stripcslashes('\x4g'), stripcslashes('\\'), bin2hex(stripcslashes('\a\b\f\v\r\0\400')));
/// ```
/// ```expect
/// string(12) "610962414171"
/// string(2) "g"
/// string(1) "\"
/// string(14) "07080c0b0d0000"
/// ```
#[php_function]
pub(super) fn stripcslashes(req: &mut Request, string: PhpString) -> Result<PhpString, Thrown> {
    let s = string.as_bytes();
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] != b'\\' || i + 1 >= s.len() {
            out.push(s[i]);
            i += 1;
            continue;
        }
        i += 1;
        let c = s[i];
        let simple = match c {
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b'a' => Some(0x07),
            b't' => Some(b'\t'),
            b'v' => Some(0x0b),
            b'b' => Some(0x08),
            b'f' => Some(0x0c),
            b'\\' => Some(b'\\'),
            _ => None,
        };
        if let Some(byte) = simple {
            out.push(byte);
            i += 1;
            continue;
        }
        if c == b'x' && s.get(i + 1).is_some_and(u8::is_ascii_hexdigit) {
            let mut value = 0u32;
            let mut digits = 0;
            while digits < 2 && s.get(i + 1).is_some_and(u8::is_ascii_hexdigit) {
                i += 1;
                value = value * 16 + (s[i] as char).to_digit(16).unwrap_or(0);
                digits += 1;
            }
            out.push(u8::try_from(value & 0xff).unwrap_or(0));
            i += 1;
            continue;
        }
        let mut value = 0u32;
        let mut digits = 0;
        while digits < 3 && i < s.len() && (b'0'..=b'7').contains(&s[i]) {
            value = value * 8 + u32::from(s[i] - b'0');
            i += 1;
            digits += 1;
        }
        if digits > 0 {
            out.push(u8::try_from(value & 0xff).unwrap_or(0));
        } else {
            out.push(c);
            i += 1;
        }
    }
    PhpString::new(req, &out)
}
