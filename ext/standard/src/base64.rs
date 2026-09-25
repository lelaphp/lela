//! `php_base64_encode_ex` and `php_base64_decode_ex`
//! (`Z/ext/standard/base64.c`), the scalar arms, behind `base64_encode`
//! and `base64_decode` (`basic_functions.stub.php:1906-1912`). php
//! reaches the same bytes through NEON and AVX arms where the machine
//! has them, and the scalar arm where it has not; the bytes are one
//! answer, so the scalar arm is the one copied.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::decl::{ConstExpr, FunctionDecl, Parameter, ParameterDefault, Type};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{PhpString, Value};

use crate::output::Entry;
use crate::{argument_count_between, bool_argument, declared, parameter, string_argument};

/// The two, in the stub's order.
///
/// ```php
/// echo new ReflectionFunction('base64_encode'), new ReflectionFunction('base64_decode');
/// ```
/// ```expect
/// Function [ <internal:standard> function base64_encode ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function base64_decode ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <optional> bool $strict = false ]
///   }
///   - Return [ string|false ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            declared(
                b"base64_encode",
                &[parameter(b"string", Type::String)],
                Type::String,
            ),
            base64_encode as Entry,
        ),
        (
            declared(
                b"base64_decode",
                &[
                    parameter(b"string", Type::String),
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Bool(false)),
                        ..parameter(b"strict", Type::Bool)
                    },
                ],
                Type::Union(vec![Type::String, Type::False]),
            ),
            base64_decode as Entry,
        ),
    ]
}

/// `base64_table`: the sixty-four digits, in order.
const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `base64_pad`.
const PAD: u8 = b'=';

/// `base64_encode(string $string): string`, php's
/// `PHP_FUNCTION(base64_encode)` over `php_base64_encode_impl`: every
/// three bytes as four digits, and the one or two left over as two or
/// three digits padded to four with `=`. The string is
/// `zend_string_safe_alloc((length + 2) / 3, 4, 0)`, the padded
/// length exactly, and `memory_get_usage()` moves by what that costs.
///
/// ```php
/// var_dump(base64_encode(""), base64_encode("f"), base64_encode("fo"), base64_encode("foo"), base64_encode("foob"), base64_encode("fooba"), base64_encode("foobar"), base64_encode("\xff\xfe\xfd\x00"), base64_encode(12));
/// $b = memory_get_usage(); $s = base64_encode("abcdefghij"); var_dump(memory_get_usage() - $b);
/// ```
/// ```expect
/// string(0) ""
/// string(4) "Zg=="
/// string(4) "Zm8="
/// string(4) "Zm9v"
/// string(8) "Zm9vYg=="
/// string(8) "Zm9vYmE="
/// string(8) "Zm9vYmFy"
/// string(8) "//79AA=="
/// string(4) "MTI="
/// int(48)
/// ```
///
/// ```php
/// try { base64_encode(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// try { base64_encode([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// base64_encode() expects exactly 1 argument, 0 given
/// base64_encode(): Argument #1 ($string) must be of type string, array given
/// ```
fn base64_encode(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    let input = string.as_bytes();
    let encoded = PhpString::product(req, input.len().div_ceil(3), 4, 0, |out| {
        encode(input, out);
    })?;
    Ok(Value::string(encoded))
}

/// `php_base64_encode_impl(in, inl, out, flags)` with padding on: the
/// groups of three, then the tail.
pub(super) fn encode(input: &[u8], out: &mut [u8]) {
    let mut chunks = input.chunks_exact(3);
    let mut at = 0;
    for group in &mut chunks {
        out[at] = TABLE[usize::from(group[0] >> 2)];
        out[at + 1] = TABLE[usize::from(((group[0] & 0x03) << 4) | (group[1] >> 4))];
        out[at + 2] = TABLE[usize::from(((group[1] & 0x0f) << 2) | (group[2] >> 6))];
        out[at + 3] = TABLE[usize::from(group[2] & 0x3f)];
        at += 4;
    }
    let rest = chunks.remainder();
    if let Some(first) = rest.first() {
        out[at] = TABLE[usize::from(first >> 2)];
        if let Some(second) = rest.get(1) {
            out[at + 1] = TABLE[usize::from(((first & 0x03) << 4) | (second >> 4))];
            out[at + 2] = TABLE[usize::from((second & 0x0f) << 2)];
            out[at + 3] = PAD;
        } else {
            out[at + 1] = TABLE[usize::from((first & 0x03) << 4)];
            out[at + 2] = PAD;
            out[at + 3] = PAD;
        }
    }
}

/// `base64_reverse_table`'s three answers for a byte: a digit's value,
/// whitespace, or a byte that is neither.
enum Digit {
    Value(u8),
    Space,
    Bad,
}

/// `base64_reverse_table[ch]`: `-1` for the five whitespace bytes,
/// `-2` for every other byte outside the table.
fn digit(byte: u8) -> Digit {
    match byte {
        b'A'..=b'Z' => Digit::Value(byte - b'A'),
        b'a'..=b'z' => Digit::Value(byte - b'a' + 26),
        b'0'..=b'9' => Digit::Value(byte - b'0' + 52),
        b'+' => Digit::Value(62),
        b'/' => Digit::Value(63),
        b'\t' | b'\n' | b'\r' | b' ' => Digit::Space,
        _ => Digit::Bad,
    }
}

/// `base64_decode(string $string, bool $strict = false): string|false`,
/// php's `PHP_FUNCTION(base64_decode)` over `php_base64_decode_impl`:
/// each digit's six bits laid into the bytes as they come, `=` counted
/// and otherwise skipped, and, without `$strict`, every byte outside
/// the table skipped too, so a stray byte or a missing pad changes
/// nothing. Under `$strict` a byte outside the table and the
/// whitespace is `false`, so is a digit after a `=`, one digit left
/// over in the last group, and padding that is more than two or leaves
/// the digits and the pads short of a multiple of four; no padding at
/// all is accepted, as RFC 4648 allows.
///
/// ```php
/// var_dump(base64_decode("Zm9vYmE="), base64_decode("Zm9vYmE"), base64_decode("Zm9v YmE=\n"), base64_decode("Zm9v*YmE="), base64_decode("Zm9vYmE==="), base64_decode("Zm9vYmE=x"), base64_decode("Z"), base64_decode(""), base64_decode("===="), base64_decode("Zg==\x00"), base64_decode(base64_encode("\xff\xfe\xfd\x00")) === "\xff\xfe\xfd\x00");
/// ```
/// ```expect
/// string(5) "fooba"
/// string(5) "fooba"
/// string(5) "fooba"
/// string(5) "fooba"
/// string(5) "fooba"
/// string(6) "fooba1"
/// string(0) ""
/// string(0) ""
/// string(0) ""
/// string(1) "f"
/// bool(true)
/// ```
///
/// ```php
/// var_dump(base64_decode("Zm9vYmE=", true), base64_decode("Zm9vYmE", true), base64_decode("Zm9v YmE=\n", true), base64_decode("Zm9v*YmE=", true), base64_decode("Zm9vYmE===", true), base64_decode("Zm9vYmE=x", true), base64_decode("Z", true), base64_decode("", true), base64_decode("====", true));
/// var_dump(base64_decode("Zg==", true), base64_decode("Zg=", true), base64_decode("Zg", true), base64_decode("Zm8=", true), base64_decode("Zm8==", true), base64_decode("Zm9v=", true), base64_decode("Zm9v", true), base64_decode("=Zm9v", true), base64_decode("Zg=\n=", true), base64_decode("Zg==\n", true), base64_decode("Zg==\x00", true), base64_decode("Zm9vYmFy", 1), base64_decode("Zm9vYmFy", "0"));
/// ```
/// ```expect
/// string(5) "fooba"
/// string(5) "fooba"
/// string(5) "fooba"
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// string(0) ""
/// bool(false)
/// string(1) "f"
/// bool(false)
/// string(1) "f"
/// string(2) "fo"
/// bool(false)
/// bool(false)
/// string(3) "foo"
/// bool(false)
/// string(1) "f"
/// string(1) "f"
/// bool(false)
/// string(6) "foobar"
/// string(6) "foobar"
/// ```
///
/// ```php
/// var_dump(base64_decode(null));
/// try { base64_decode("a", false, 1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: base64_decode(): Passing null to parameter #1 ($string) of type string is deprecated in %s on line 2
/// string(0) ""
/// base64_decode() expects at most 2 arguments, 3 given
/// ```
fn base64_decode(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let string = string_argument(req, 1)?;
    let strict = if req.argument_count() == 2 {
        bool_argument(req, 2)?
    } else {
        false
    };
    match decode(string.as_bytes(), strict) {
        Some(decoded) => Ok(Value::string(PhpString::new(req, &decoded)?)),
        None => Ok(Value::bool(false)),
    }
}

/// `php_base64_decode_impl(in, inl, out, outl, strict)`: the bytes, or
/// none where `strict` refuses the input. php lays the bytes into a
/// string of the input's length and shortens it afterwards; here they
/// are gathered first and the string made at its length, so
/// `memory_get_usage()` after a decode reads the shorter allocation.
fn decode(input: &[u8], strict: bool) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(input.len());
    let mut i = 0usize;
    let mut padding = 0usize;
    let mut pending: u8 = 0;
    for &byte in input {
        if byte == PAD {
            padding += 1;
            continue;
        }
        let ch = match digit(byte) {
            Digit::Value(ch) => ch,
            Digit::Bad if strict => return None,
            Digit::Space | Digit::Bad => continue,
        };
        if strict && padding > 0 {
            return None;
        }
        match i % 4 {
            0 => pending = ch << 2,
            1 => {
                out.push(pending | (ch >> 4));
                pending = (ch & 0x0f) << 4;
            }
            2 => {
                out.push(pending | (ch >> 2));
                pending = (ch & 0x03) << 6;
            }
            _ => out.push(pending | ch),
        }
        i += 1;
    }
    // "fail if the input is truncated (only one char in last group)"
    if strict && i % 4 == 1 {
        return None;
    }
    // "fail if the padding length is wrong (not VV==, VVV=), but accept
    // zero padding"
    if strict && padding > 0 && (padding > 2 || !(i + padding).is_multiple_of(4)) {
        return None;
    }
    Some(out)
}
