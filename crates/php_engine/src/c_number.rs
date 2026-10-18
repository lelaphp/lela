//! C's `isspace`, `strtol` and `atoi` as the C library php links reads
//! them, for the places php-src calls them on a value a program can set:
//! the ini entries `ZEND_ATOL` reads, the status line
//! `sapi_extract_response_code` reads with `atoi`, and the ini syntax's
//! `get_int_val`; and, through `coerce` on the native face and `php_sapi`
//! for the hosts, every extension's and host's call of them (#2634). One
//! reading of each, so the edges agree: `strtol` saturates at the long's
//! ends, and `atoi` is that truncated to `int`, as glibc's and the BSDs'
//! `atoi` is `(int) strtol(s, NULL, 10)`.
//!
//! [`is_space`] is the C library's own, under the thread's `LC_CTYPE`, as
//! php's `isspace` and the white space `strtol` skips are; php's
//! locale-free spelling of the six bytes, `zend_is_whitespace` and
//! `_is_numeric_string_ex`'s, is [`is_whitespace`].
//!
//! [`c_str`] is the string all of them read: a `char *` as `strlen` reads
//! it, up to its first NUL, which is what php hands C, or prints with
//! `%s`, of a string that may hold one (#2763).

/// C's `isspace` under the thread's `LC_CTYPE`, the request's locale
/// where `setlocale()` installed one: the six bytes [`is_whitespace`]
/// names in the `C` locale, and under php's default `C.UTF-8` whatever
/// the C library counts besides: `0xA0` on macOS and nothing on glibc,
/// which `php_process`'s own test holds, so no witness pins it (#2796).
pub use php_process::is_space;

/// php's own spelling of the white space C's `isspace` reads in the `C`
/// locale, whatever locale a request set: space, `\t`, `\n`, `\v`, `\f`
/// and `\r`, as `zend_is_whitespace` (`Z/Zend/zend_ini.c:35`) and
/// `_is_numeric_string_ex` (`Z/Zend/zend_operators.c:3642`) write them.
pub(crate) fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// C's `strtol(s, NULL, 10)`, which `ZEND_ATOL` and `ZEND_STRTOL` with a
/// base of 10 are on a 64-bit host (`Z/Zend/zend_long.h:77`): the
/// [`is_space`] bytes skipped, one sign, the digits up to the first byte
/// that is not one, zero where there are none, and `LONG_MAX` or
/// `LONG_MIN` by the sign where the digits spell a number past the long.
///
/// `precision` is `ZEND_ATOL` of its text, which skips the white space
/// [`is_space`] reads, and under `C` not `0xA0`; the long cut to `int`
/// is [`atoi`]'s.
///
/// ```php
/// ini_set('precision', " \t\v4"); echo 1/3, "\n";
/// ini_set('precision', "4294967301"); echo 1/3, "\n";
/// setlocale(LC_CTYPE, "C");
/// ini_set('precision', "\xa04"); echo 1/3, "\n";
/// ```
/// ```expect
/// 0.3333
/// 0.33333
/// 0.3
/// ```
#[must_use]
pub fn strtol(bytes: &[u8]) -> i64 {
    strtol_in(bytes, Some(10))
}

/// C's `strtol(s, NULL, 0)`, `zend_ini_long`'s `ZEND_STRTOL(value, NULL,
/// 0)` (`Z/Zend/zend_ini.c:469`): [`strtol`] with the base read from the
/// digits, `0x` or `0X` before a hex digit sixteen, a leading `0` eight,
/// and ten otherwise.
pub(crate) fn strtol_prefixed(bytes: &[u8]) -> i64 {
    strtol_in(bytes, None)
}

/// C's `strtol` in `base`, `None` for base zero.
fn strtol_in(bytes: &[u8], base: Option<u32>) -> i64 {
    let bytes = &bytes[bytes.iter().take_while(|b| is_space(**b)).count()..];
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let (radix, digits) = match (base, digits) {
        (Some(radix), _) => (radix, digits),
        (None, [b'0', b'x' | b'X', hex, ..]) if hex.is_ascii_hexdigit() => (16, &digits[2..]),
        (None, [b'0', ..]) => (8, digits),
        (None, _) => (10, digits),
    };
    let mut n: i64 = 0;
    for digit in digits.iter().map_while(|b| char::from(*b).to_digit(radix)) {
        let digit = i64::from(digit);
        let next = n.checked_mul(i64::from(radix)).and_then(|n| {
            if negative {
                n.checked_sub(digit)
            } else {
                n.checked_add(digit)
            }
        });
        let Some(next) = next else {
            return if negative { i64::MIN } else { i64::MAX };
        };
        n = next;
    }
    n
}

/// C's `atoi`, `(int) strtol(s, NULL, 10)`: [`strtol`]'s long cut to its
/// low 32 bits, so `4294967497` is `201` and a number past the long is
/// `LONG_MAX`'s `-1`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "C's `(int)` of `strtol`'s long, whose cut is the behaviour being copied"
)]
#[must_use]
pub fn atoi(bytes: &[u8]) -> i32 {
    strtol(bytes) as i32
}

/// `bytes` as C reads a `char *` through `strlen`: the bytes before the
/// first NUL, every byte where there is none. php reads a string so
/// wherever it hands its bytes to a C function that takes no length, or
/// prints them with `%s`.
#[must_use]
pub fn c_str(bytes: &[u8]) -> &[u8] {
    bytes.split(|&b| b == 0).next().unwrap_or(bytes)
}

/// A `zend_long`'s decimal text, as `zend_long_to_str` and
/// `smart_str_append_long` write it, its sign before its digits, laid in
/// a buffer of its own: `i64::MIN`'s sign and nineteen digits fill it.
pub(crate) struct Long {
    buf: [u8; 20],
    start: usize,
}

impl Long {
    pub(crate) fn of(n: i64) -> Long {
        let mut buf = [0u8; 20];
        let mut start = buf.len();
        let mut magnitude = n.unsigned_abs();
        loop {
            start -= 1;
            // A digit, below ten.
            #[allow(clippy::cast_possible_truncation)]
            let digit = (magnitude % 10) as u8;
            buf[start] = b'0' + digit;
            magnitude /= 10;
            if magnitude == 0 {
                break;
            }
        }
        if n < 0 {
            start -= 1;
            buf[start] = b'-';
        }
        Long { buf, start }
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.buf[self.start..]
    }
}

#[cfg(test)]
mod tests {
    use super::{Long, atoi, c_str, strtol, strtol_prefixed};

    /// A long's text is Rust's `to_string`'s at the edges and between.
    #[test]
    fn a_long_is_written_as_its_decimal() {
        for n in [
            0,
            7,
            -7,
            10,
            -10,
            1_234_567,
            i64::MAX,
            i64::MIN,
            i64::MIN + 1,
        ] {
            assert_eq!(Long::of(n).as_bytes(), n.to_string().as_bytes());
        }
    }

    /// A C string ends at its first NUL, is empty where it leads, and is
    /// every byte where there is none.
    #[test]
    fn c_str_reads_up_to_the_first_nul() {
        assert_eq!(c_str(b"a.txt\0.php\0x"), b"a.txt");
        assert_eq!(c_str(b"\0a"), b"");
        assert_eq!(c_str(b"abc"), b"abc");
        assert_eq!(c_str(b""), b"");
    }

    /// The edges C's library reads: white space and one sign before the
    /// digits, nothing after the first byte that is not one, a long's
    /// saturation by the sign, and `atoi`'s cut of it to `int`.
    #[test]
    fn strtol_and_atoi_read_the_edges_c_reads() {
        assert_eq!(strtol(b" \t\x0b\x0c\r\n-12x"), -12);
        assert_eq!(strtol(b"+7"), 7);
        assert_eq!(strtol(b"--1"), 0);
        assert_eq!(strtol(b"abc"), 0);
        assert_eq!(strtol(b"9223372036854775807"), i64::MAX);
        assert_eq!(strtol(b"9223372036854775808"), i64::MAX);
        assert_eq!(strtol(b"-9223372036854775808"), i64::MIN);
        assert_eq!(strtol(b"-99999999999999999999"), i64::MIN);
        assert_eq!(atoi(b"4294967497"), 201);
        assert_eq!(atoi(b"4294967296"), 0);
        assert_eq!(atoi(b"99999999999999999999"), -1);
        assert_eq!(atoi(b"-2147483649"), 2_147_483_647);
    }

    /// Base zero reads its base from the digits: `0x` before a hex digit,
    /// a leading `0`, or ten; a `0x` with no hex digit after it is the
    /// zero before the `x`.
    #[test]
    fn strtol_prefixed_reads_the_base_c_reads() {
        assert_eq!(strtol_prefixed(b" 0x1f"), 31);
        assert_eq!(strtol_prefixed(b"-0X10"), -16);
        assert_eq!(strtol_prefixed(b"010"), 8);
        assert_eq!(strtol_prefixed(b"09"), 0);
        assert_eq!(strtol_prefixed(b"0xg"), 0);
        assert_eq!(strtol_prefixed(b"20k"), 20);
        assert_eq!(strtol_prefixed(b"0x8000000000000000"), i64::MAX);
        assert_eq!(strtol_prefixed(b"-0x8000000000000000"), i64::MIN);
        assert_eq!(strtol(b"0x10"), 0);
    }
}
