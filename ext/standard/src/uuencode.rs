//! `Z/ext/standard/uuencode.c`: `convert_uuencode` over `php_uuencode`
//! and `convert_uudecode` over `php_uudecode`, the classic
//! 45-bytes-a-line encoding with `` ` `` for zero, read past a short
//! tail as php reads its NUL-terminated buffer.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::OrFalse;
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::PhpString;

/// `PHP_UU_ENC(c)`.
const fn enc(c: u8) -> u8 {
    if c == 0 { b'`' } else { (c & 0o77) + b' ' }
}

/// `PHP_UU_DEC(c)`.
const fn dec(c: u8) -> u8 {
    c.wrapping_sub(b' ') & 0o77
}

/// `php_uuencode(src, src_len)`.
fn uuencode(src: &[u8]) -> Vec<u8> {
    let at = |i: usize| src.get(i).copied().unwrap_or(0);
    let c2 = |s: usize| enc(((at(s) << 4) & 0o60) | ((at(s + 1) >> 4) & 0o17));
    let c3 = |s: usize| enc(((at(s + 1) << 2) & 0o74) | ((at(s + 2) >> 6) & 0o3));
    let mut out = Vec::with_capacity(src.len() / 2 * 3 + 46);
    let e = src.len();
    let mut s = 0usize;
    let mut len = 45usize;
    while s + 3 < e {
        let mut ee = s + len;
        if ee > e {
            ee = e;
            len = ee - s;
            if !len.is_multiple_of(3) {
                ee = s + len / 3 * 3;
            }
        }
        out.push(enc(u8::try_from(len).unwrap_or(0)));
        while s < ee {
            out.push(enc(at(s) >> 2));
            out.push(c2(s));
            out.push(c3(s));
            out.push(enc(at(s + 2) & 0o77));
            s += 3;
        }
        if len == 45 {
            out.push(b'\n');
        }
    }
    if s < e {
        if len == 45 {
            out.push(enc(u8::try_from(e - s).unwrap_or(0)));
            len = 0;
        }
        out.push(enc(at(s) >> 2));
        out.push(c2(s));
        out.push(if e - s > 1 { c3(s) } else { enc(0) });
        out.push(if e - s > 2 {
            enc(at(s + 2) & 0o77)
        } else {
            enc(0)
        });
    }
    if len < 45 {
        out.push(b'\n');
    }
    out.push(enc(0));
    out.push(b'\n');
    out
}

/// `php_uudecode(src, src_len)`: the bytes, or `None` for input php
/// refuses.
fn uudecode(src: &[u8]) -> Option<Vec<u8>> {
    if src.is_empty() {
        return None;
    }
    let at = |i: usize| src.get(i).copied().unwrap_or(0);
    let e = src.len();
    let mut out = Vec::with_capacity(src.len());
    let mut s = 0usize;
    let mut total = 0usize;
    while s < e {
        let len = usize::from(dec(at(s)));
        s += 1;
        if len == 0 {
            break;
        }
        if len > src.len() {
            return None;
        }
        total += len;
        // `(int) floor(len * 1.33)`
        let span = if len == 45 { 60 } else { len * 133 / 100 };
        let ee = s + span;
        if ee > e {
            return None;
        }
        while s < ee {
            if s + 4 > e {
                return None;
            }
            out.push((dec(at(s)) << 2) | (dec(at(s + 1)) >> 4));
            out.push((dec(at(s + 1)) << 4) | (dec(at(s + 2)) >> 2));
            out.push((dec(at(s + 2)) << 6) | dec(at(s + 3)));
            s += 4;
        }
        if len < 45 {
            break;
        }
        s += 1;
    }
    if total > out.len() {
        out.push((dec(at(s)) << 2) | (dec(at(s + 1)) >> 4));
        if total > 1 {
            out.push((dec(at(s + 1)) << 4) | (dec(at(s + 2)) >> 2));
            if total > 2 {
                out.push((dec(at(s + 2)) << 6) | dec(at(s + 3)));
            }
        }
    }
    out.resize(total, 0);
    Some(out)
}

/// `convert_uuencode(string $string): string`.
///
/// ```php
/// var_dump(convert_uuencode("test\ntext text text\r\n"), convert_uuencode(""), convert_uuencode("a"), convert_uuencode(str_repeat("x", 50)));
/// ```
/// ```expect
/// string(32) "5=&5S=`IT97AT('1E>'0@=&5X=`T*
/// `
/// "
/// string(2) "`
/// "
/// string(8) "!80``
/// `
/// "
/// string(74) "M>'AX>'AX>'AX>'AX>'AX>'AX>'AX>'AX>'AX>'AX>'AX>'AX>'AX>'AX>'AX
/// %>'AX>'@`
/// `
/// "
/// ```
///
/// The declarations as Reflection renders them.
///
/// ```php
/// echo new ReflectionFunction('convert_uuencode'), new ReflectionFunction('convert_uudecode');
/// ```
/// ```expect
/// Function [ <internal:standard> function convert_uuencode ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function convert_uudecode ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ string|false ]
/// }
/// ```
#[php_function]
fn convert_uuencode(req: &mut Request, string: PhpString) -> Result<PhpString, Thrown> {
    let out = uuencode(string.as_bytes());
    PhpString::new(req, &out)
}

/// `convert_uudecode(string $string): string|false`, and php's warning
/// and `false` for what is no uuencoded string.
///
/// ```php
/// var_dump(convert_uudecode(convert_uuencode("test\ntext text text")), convert_uudecode(convert_uuencode(str_repeat("xyz", 30))) === str_repeat("xyz", 30), convert_uudecode("!80``\n`\n"), convert_uudecode(""), convert_uudecode("M"));
/// ```
/// ```expect
/// Warning: convert_uudecode(): Argument #1 ($data) is not a valid uuencoded string in %s on line 2
///
/// Warning: convert_uudecode(): Argument #1 ($data) is not a valid uuencoded string in %s on line 2
/// string(19) "test
/// text text text"
/// bool(true)
/// string(1) "a"
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn convert_uudecode(req: &mut Request, string: PhpString) -> Result<OrFalse<PhpString>, Thrown> {
    let Some(out) = uudecode(string.as_bytes()) else {
        docref(
            req,
            Level::Warning,
            "Argument #1 ($data) is not a valid uuencoded string",
        );
        return Ok(OrFalse::False);
    };
    PhpString::new(req, &out).map(OrFalse::Value)
}
