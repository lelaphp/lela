//! C's `isspace`, `strtol` and `atoi` as the C library php links reads
//! them in the `C` locale, for the places php-src calls them on a value
//! a program can set: the ini entries `ZEND_ATOL` reads, the status line
//! `sapi_extract_response_code` reads with `atoi`, and the ini syntax's
//! `get_int_val`. One reading of each, so the edges agree: `strtol`
//! saturates at the long's ends, and `atoi` is that truncated to `int`,
//! as glibc's and the BSDs' `atoi` is `(int) strtol(s, NULL, 10)`.

/// C's `isspace` in the `C` locale, which php's `zend_is_whitespace`
/// (`Z/Zend/zend_ini.c:35`) spells out: space, `\t`, `\n`, `\v`, `\f`
/// and `\r`.
pub(crate) fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// C's `strtol(s, NULL, 10)`, which `ZEND_ATOL` and `ZEND_STRTOL` are
/// on a 64-bit host (`Z/Zend/zend_long.h:77`): the [`is_space`] bytes
/// skipped, one sign, the digits up to the first byte that is not one,
/// zero where there are none, and `LONG_MAX` or `LONG_MIN` by the sign
/// where the digits spell a number past the long.
pub(crate) fn strtol(bytes: &[u8]) -> i64 {
    let bytes = &bytes[bytes.iter().take_while(|b| is_space(**b)).count()..];
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let mut n: i64 = 0;
    for digit in digits.iter().take_while(|b| b.is_ascii_digit()) {
        let digit = i64::from(digit - b'0');
        let next = n.checked_mul(10).and_then(|n| {
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
pub(crate) fn atoi(bytes: &[u8]) -> i32 {
    strtol(bytes) as i32
}

#[cfg(test)]
mod tests {
    use super::{atoi, strtol};

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
}
