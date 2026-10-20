//! `zend_ini_parse_quantity` (`Z/Zend/zend_ini.c:613-841`), the one
//! reading of a whole-number ini value: what `OnUpdateLong` stores,
//! what `ini_parse_quantity()` answers, and the sentence php's `errstr`
//! holds for a shape it interprets for backwards compatibility.
//! [`Ini::parse_quantity`](crate::api::request::Ini::parse_quantity)
//! is its face, and the witnesses stand there.

use crate::c_number::is_whitespace;

/// The value, and the sentence where php writes one: `zend_ini_parse_quantity`.
pub(crate) fn parse(value: &[u8]) -> (i64, Option<String>) {
    let (bits, sentence) = quantity(value, Signedness::Signed);
    (i64::from_ne_bytes(bits.to_ne_bytes()), sentence)
}

/// `zend_ini_parse_uquantity`: the value read as unsigned, `-1` alone
/// among the negatives taken, as the maximum, since `memory_limit=-1`
/// spells it so.
pub(crate) fn parse_unsigned(value: &[u8]) -> (u64, Option<String>) {
    quantity(value, Signedness::Unsigned)
}

/// `zend_ini_parse_quantity_signed_result_t`: what range the value is
/// held to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Signedness {
    Signed,
    Unsigned,
}

/// `zend_ini_parse_quantity_internal` (`Z/Zend/zend_ini.c:613-841`).
#[allow(clippy::too_many_lines)] // one C function, kept whole
fn quantity(value: &[u8], signedness: Signedness) -> (u64, Option<String>) {
    let invalid = || escaped(value);
    let mut start = 0;
    let mut end = value.len();
    while start < end && is_whitespace(value[start]) {
        start += 1;
    }
    while start < end && is_whitespace(value[end - 1]) {
        end -= 1;
    }
    if start == end {
        return (0, None);
    }
    let at = |i: usize| if i < end { value[i] } else { 0 };
    let negative = match value[start] {
        b'+' => {
            start += 1;
            false
        }
        b'-' => {
            start += 1;
            true
        }
        _ => false,
    };
    let no_digits = || {
        format!(
            "Invalid quantity \"{}\": no valid leading digits, interpreting as \"0\" for backwards compatibility",
            invalid()
        )
    };
    if !at(start).is_ascii_digit() {
        return (0, Some(no_digits()));
    }
    let mut base = 0;
    if at(start) == b'0' && !at(start + 1).is_ascii_digit() {
        if start + 1 == end {
            return (0, None);
        }
        match at(start + 1) {
            b'g' | b'G' | b'm' | b'M' | b'k' | b'K' => {}
            prefix => {
                base = match prefix {
                    b'x' | b'X' => 16,
                    b'o' | b'O' => 8,
                    b'b' | b'B' => 2,
                    other => {
                        return (
                            0,
                            Some(format!(
                                "Invalid prefix \"0{}\", interpreting as \"0\" for backwards compatibility",
                                char::from(other)
                            )),
                        );
                    }
                };
                start += 2;
                if start == end || start != quantity_prefix(value, start, end, base) {
                    return (
                        0,
                        Some(format!(
                            "Invalid quantity \"{}\": no digits after base prefix, interpreting as \"0\" for backwards compatibility",
                            invalid()
                        )),
                    );
                }
            }
        }
    }
    let (magnitude, digits_end, range) = strtoull(&value[..end], start, base);
    let mut overflow = range;
    let mut result = magnitude;
    if !overflow {
        match signedness {
            // "-1" is the one negative taken, `memory_limit=-1`'s maximum.
            Signedness::Unsigned if negative => {
                if magnitude == 1 && digits_end == end {
                    result = u64::MAX;
                } else {
                    overflow = true;
                }
            }
            Signedness::Unsigned => {}
            Signedness::Signed => {
                if negative && magnitude == 1u64 << 63 {
                    result = magnitude.wrapping_neg();
                } else if i64::try_from(magnitude).is_err() {
                    overflow = true;
                } else if negative {
                    result = magnitude.wrapping_neg();
                }
            }
        }
    }
    if digits_end == start {
        return (0, Some(no_digits()));
    }
    let mut digits_end = digits_end;
    while digits_end < end && is_whitespace(value[digits_end]) {
        digits_end += 1;
    }
    let signed = |n: u64| i64::from_ne_bytes(n.to_ne_bytes());
    if digits_end < end {
        let last = value[end - 1];
        // php's `str`, the value from its first byte, its leading
        // whitespace with it.
        let interpreted = escaped(&value[..digits_end]);
        let chr = escaped(&[last]);
        let factor: u64 = match last {
            b'g' | b'G' => 1 << 30,
            b'm' | b'M' => 1 << 20,
            b'k' | b'K' => 1 << 10,
            _ => {
                return (
                    result,
                    Some(format!(
                        "Invalid quantity \"{}\": unknown multiplier \"{chr}\", interpreting as \"{interpreted}\" for backwards compatibility",
                        invalid()
                    )),
                );
            }
        };
        if !overflow {
            overflow = match signedness {
                Signedness::Signed => {
                    let (n, f) = (signed(result), signed(factor));
                    if n > 0 {
                        n > i64::MAX / f
                    } else {
                        n < i64::MIN / f
                    }
                }
                Signedness::Unsigned => result > u64::MAX / factor,
            };
        }
        result = result.wrapping_mul(factor);
        if digits_end != end - 1 {
            return (
                result,
                Some(format!(
                    "Invalid quantity \"{}\", interpreting as \"{interpreted}{chr}\" for backwards compatibility",
                    invalid()
                )),
            );
        }
    }
    if overflow {
        return (
            result,
            Some(format!(
                "Invalid quantity \"{}\": value is out of range, using overflow result for backwards compatibility",
                invalid()
            )),
        );
    }
    (result, None)
}

/// `smart_str_append_escaped` over `bytes`, into a string of its own.
fn escaped(bytes: &[u8]) -> String {
    let mut out = String::new();
    crate::api::request::append_escaped(&mut out, bytes);
    out
}

/// `zend_ini_consume_quantity_prefix`: where `strtoull` would start
/// reading digits after whitespace, a sign and a base prefix of its
/// own, which `zend_ini_parse_quantity` requires to be where it already
/// stands. `0b` is a digit pair under base sixteen, not a prefix.
fn quantity_prefix(value: &[u8], start: usize, end: usize, base: u32) -> usize {
    let at = |i: usize| if i < end { value[i] } else { 0 };
    let mut i = start;
    while i < end && is_whitespace(value[i]) {
        i += 1;
    }
    if matches!(at(i), b'+' | b'-') {
        i += 1;
    }
    if at(i) == b'0' && !at(i + 1).is_ascii_digit() {
        if i + 1 == end {
            return i;
        }
        match at(i + 1) {
            b'x' | b'X' | b'o' | b'O' => i += 2,
            b'b' | b'B' if base != 16 => i += 2,
            _ => {}
        }
    }
    i
}

/// `strtoull(text + start, &end, base)` over digits alone, which is
/// all `zend_ini_parse_quantity` hands it: base zero reads `0x` as
/// hexadecimal where a hex digit follows and a leading `0` as octal.
/// The value, where the digits ended, and `ERANGE`: past `u64::MAX` the
/// value is `u64::MAX` and the digits are read to their end.
fn strtoull(text: &[u8], start: usize, base: u32) -> (u64, usize, bool) {
    let digit = |i: usize, radix: u32| {
        text.get(i)
            .and_then(|c| char::from(*c).to_digit(radix))
            .map(u64::from)
    };
    let (mut at, radix) = match base {
        0 if text.get(start) == Some(&b'0')
            && matches!(text.get(start + 1), Some(b'x' | b'X'))
            && digit(start + 2, 16).is_some() =>
        {
            (start + 2, 16)
        }
        0 if text.get(start) == Some(&b'0') => (start, 8),
        0 => (start, 10),
        base => (start, base),
    };
    let first = at;
    let mut value: u64 = 0;
    let mut range = false;
    while let Some(d) = digit(at, radix) {
        match value
            .checked_mul(u64::from(radix))
            .and_then(|v| v.checked_add(d))
        {
            Some(next) => value = next,
            None => range = true,
        }
        at += 1;
    }
    if range {
        value = u64::MAX;
    }
    if at == first {
        return (0, start, false);
    }
    (value, at, range)
}
