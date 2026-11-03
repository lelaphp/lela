//! The escapes of `string.c`: `addslashes` and `stripslashes`, and the
//! C-style pair `addcslashes` and `stripcslashes`.

use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{FreshString, Owner, PhpString, Sizing};

use super::{Fill, charmask, shortened};

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
///
/// Its string is asked for as php asks for it, so `memory_get_usage()`
/// moves by php's bin:
///
/// ```php
/// $m = memory_get_usage(); $v0 = addslashes("a'b"); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $v1 = addslashes(str_repeat("x", 20) . "\"" . str_repeat("y", 20)); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 40
/// 80
/// ```
#[php_function]
pub(super) fn addslashes(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let s = string.as_bytes();
    let slash = |c: &u8| matches!(c, 0 | b'\'' | b'"' | b'\\');
    // `php_addslashes` as php builds it on aarch64 and with SSE 4.2:
    // sixteen bytes at a time while more than fifteen are left, so the
    // bytes copied as they stand end at the block the first byte to
    // escape is in, and then byte by byte.
    let mut offset = 0;
    while s.len() - offset > 15 && !s[offset..offset + 16].iter().any(slash) {
        offset += 16;
    }
    let Some(found) = s[offset..].iter().position(slash) else {
        return Ok(Owner::take(req, string));
    };
    if s.len() - offset <= 15 {
        offset += found;
    }
    // `zend_string_safe_alloc(2, ZSTR_LEN(str) - offset, offset, 0)`,
    // and `zend_string_truncate` where more than 16 bytes are left over,
    // `ZSTR_LEN(new_str) = ...` otherwise.
    let rest = s.len() - offset;
    let mut fresh = FreshString::sized(
        req,
        rest.saturating_mul(2).saturating_add(offset),
        Sizing::SafeAlloc { n: 2, m: rest },
    )?;
    let mut out = Fill::new(fresh.bytes_mut());
    out.put(&s[..offset]);
    for &c in &s[offset..] {
        match c {
            0 => out.put(b"\\0"),
            b'\'' | b'"' | b'\\' => {
                out.byte(b'\\');
                out.byte(c);
            }
            _ => out.byte(c),
        }
    }
    let written = out.written();
    if fresh.len() - written > 16 {
        fresh = fresh.truncate(req, written)?;
    } else {
        fresh.set_len(written);
    }
    Ok(fresh.finish())
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
///
/// Its string is asked for as php asks for it, so `memory_get_usage()`
/// moves by php's bin:
///
/// ```php
/// $m = memory_get_usage(); $v0 = stripslashes("a\\\\b"); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $v1 = stripslashes(str_repeat("a\\\\b", 14)); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 32
/// 96
/// ```
#[php_function]
pub(super) fn stripslashes(
    req: &mut Request,
    string: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    let s = string.as_bytes();
    // `zend_string_init` of the argument, `php_stripslashes` in place and
    // `ZSTR_LEN(str) = ...`, the allocation kept.
    shortened(req, s.len(), |out| {
        let mut i = 0;
        while i < s.len() {
            if s[i] == b'\\' {
                i += 1;
                if let Some(&next) = s.get(i) {
                    out.byte(if next == b'0' { 0 } else { next });
                    i += 1;
                }
            } else {
                out.byte(s[i]);
                i += 1;
            }
        }
    })
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
///
/// Its string is asked for as php asks for it, so `memory_get_usage()`
/// moves by php's bin:
///
/// ```php
/// $m = memory_get_usage(); $v0 = addcslashes("zz", "a..c"); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $v1 = addcslashes(str_repeat("a\nb", 10), "a..c\n"); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 40
/// 96
/// ```
#[php_function]
pub(super) fn addcslashes(
    req: &mut Request,
    string: PhpString,
    characters: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    let what = characters;
    if string.is_empty() {
        return PhpString::new(req, b"");
    }
    if what.is_empty() {
        return Ok(Owner::take(req, string));
    }
    let flags = charmask(req, what.as_bytes());
    let len = string.len();
    // `php_addcslashes_str`: `zend_string_safe_alloc(4, len, 0, 0)`, and
    // `zend_string_truncate` where fewer bytes came out.
    let mut fresh = FreshString::sized(
        req,
        len.saturating_mul(4),
        Sizing::SafeAlloc { n: 4, m: len },
    )?;
    let mut out = Fill::new(fresh.bytes_mut());
    for &c in string.as_bytes() {
        if flags[usize::from(c)] {
            if !(32..=126).contains(&c) {
                out.byte(b'\\');
                match c {
                    b'\n' => out.byte(b'n'),
                    b'\t' => out.byte(b't'),
                    b'\r' => out.byte(b'r'),
                    0x07 => out.byte(b'a'),
                    0x0b => out.byte(b'v'),
                    0x08 => out.byte(b'b'),
                    0x0c => out.byte(b'f'),
                    _ => out.put(&[b'0' + (c >> 6), b'0' + ((c >> 3) & 7), b'0' + (c & 7)]),
                }
                continue;
            }
            out.byte(b'\\');
        }
        out.byte(c);
    }
    let written = out.written();
    if written < len.saturating_mul(4) {
        fresh = fresh.truncate(req, written)?;
    }
    Ok(fresh.finish())
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
///
/// Its string is asked for as php asks for it, so `memory_get_usage()`
/// moves by php's bin:
///
/// ```php
/// $m = memory_get_usage(); $v0 = stripcslashes(str_repeat("a\\\\x41", 10)); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 96
/// ```
#[php_function]
pub(super) fn stripcslashes(
    req: &mut Request,
    string: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    let s = string.as_bytes();
    // `ZVAL_STRINGL` of the argument, `php_stripcslashes` in place and
    // `ZSTR_LEN(str) = nlen`, the allocation kept.
    shortened(req, s.len(), |out| {
        let mut i = 0;
        while i < s.len() {
            if s[i] != b'\\' || i + 1 >= s.len() {
                out.byte(s[i]);
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
                out.byte(byte);
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
                out.byte(u8::try_from(value & 0xff).unwrap_or(0));
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
                out.byte(u8::try_from(value & 0xff).unwrap_or(0));
            } else {
                out.byte(c);
                i += 1;
            }
        }
    })
}
