//! The numeric string (design §5.2): what php reads a string as when an
//! operator wants a number, `_is_numeric_string_ex`
//! (`Z/Zend/zend_operators.c:3620`), ported once. The compiler and the
//! kernels both read it, because php's compiler folds arithmetic over
//! two constants through the same `add_function` the opcode calls
//! (`zend_try_ct_eval_binary_op`), so one reader is what keeps the op
//! array and the value in step with php's.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

/// One operand of php's arithmetic: `IS_LONG` or `IS_DOUBLE`, the two
/// types `add_function` and its kin switch on, and the two a numeric
/// string reads as.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    Long(i64),
    Double(f64),
}

impl Number {
    /// The operand as the double php's mixed type pairs convert it to.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "php's `(double) lval`, whose rounding is the behaviour being copied"
    )]
    pub fn double(self) -> f64 {
        match self {
            Number::Long(n) => n as f64,
            Number::Double(d) => d,
        }
    }
}

/// `_is_numeric_string_ex` with php's `allow_errors` on, which is how
/// `zendi_try_convert_scalar_to_number` calls it: the number the string
/// opens with after any leading whitespace, and whether anything but
/// trailing whitespace follows it, which is what the kernel warns on and
/// what php's compiler refuses to fold. Nothing at all for a string that
/// opens with no number, which is what makes `"abc" + 1` php's
/// `TypeError`. An integer too wide for a long is a double, as php's
/// `MAX_LENGTH_OF_LONG` and `long_min_digits` arms leave it.
///
/// Read where php's compiler reads it, so that each sum is one folded
/// constant and no opline in both engines: whitespace on either side,
/// a leading sign, leading zeros, a fraction with digits on one side of
/// the point alone, an exponent, and the two integers a long's edge
/// splits.
///
/// ```php
/// echo " 1 " + 1, "|", "+2" + 1, "|", "007" + 1, "|", ".5" + 1, "|", "1." + 1, "|", "1e3" + 1, "|", "1.5e-1" + 1, "|", "9223372036854775807" + 0, "|", "9223372036854775808" + 0, "|", "-9223372036854775808" + 0, "|", "-9223372036854775809" + 0;
/// ```
/// ```expect exit=0
/// 2|3|8|1.5|2|1001|1.15|9223372036854775807|9.2233720368548E+18|-9223372036854775808|-9.2233720368548E+18
/// ```
///
/// And where the kernel reads it, the opcode left standing by a call
/// (design §8): the same numbers, and the warning for what a string
/// carries past its number, which the compiler leaves to the opcode.
///
/// ```php
/// $one = extension_loaded('Core');
/// echo " 1 " + $one, "|", "1e3" + $one, "|", "9223372036854775808" + $one, "|", "1e" + $one, "|", "0x1A" + $one;
/// ```
/// ```expect exit=0
/// 2|1001|9.2233720368548E+18|
/// Warning: A non-numeric value encountered in %s on line 3
/// 2|
/// Warning: A non-numeric value encountered in %s on line 3
/// 1
/// ```
#[must_use]
pub fn numeric_prefix(bytes: &[u8]) -> Option<(Number, bool)> {
    let space = |b: &u8| matches!(*b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let from = bytes.iter().position(|b| !space(b))?;
    let rest = bytes.get(from..)?;
    let mut at = usize::from(matches!(rest.first(), Some(b'-' | b'+')));
    let digits = at;
    while rest.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    let mut floating = false;
    // php's `process_double`, reached by a `.` after digits or before
    // them: `zend_strtod` then takes the whole number.
    if rest.get(at) == Some(&b'.')
        && (at > digits || rest.get(at + 1).is_some_and(u8::is_ascii_digit))
    {
        floating = true;
        at += 1;
        while rest.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
    }
    if at == digits {
        return None;
    }
    // An exponent counts only with a digit behind it, sign or no sign.
    let exponent = usize::from(matches!(rest.get(at + 1), Some(b'-' | b'+')));
    if matches!(rest.get(at), Some(b'e' | b'E'))
        && rest.get(at + 1 + exponent).is_some_and(u8::is_ascii_digit)
    {
        floating = true;
        at += 2 + exponent;
        while rest.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
    }
    let text = std::str::from_utf8(rest.get(..at)?).ok()?;
    let trailing = rest.get(at..).is_some_and(|tail| !tail.iter().all(space));
    if floating {
        return Some((Number::Double(text.parse().ok()?), trailing));
    }
    // php reads an integer too wide for a long as a double.
    let number = match text.parse::<i64>() {
        Ok(n) => Number::Long(n),
        Err(_) => Number::Double(text.parse().ok()?),
    };
    Some((number, trailing))
}

/// `pow_function_base`'s long arm for an exponent that is not negative
/// (`Z/Zend/zend_operators.c:1358`): php's square-and-multiply, a long
/// while every step fits and, once a step overflows, the double php
/// computes from that step's `pow()`. A negative exponent is php's
/// `pow()` over two doubles from the start, and stays with the caller,
/// since the deprecation php raises for a zero base there is the
/// engine's to raise. Read here by the compiler's fold and by the
/// kernel, because php's compiler folds `**` over two constants through
/// this very function (`zend_try_ct_eval_binary_op`), so one copy is
/// what keeps the folded constant and the opcode's value in step.
///
/// ```php
/// echo 2 ** 3, "|", 2 ** 3 ** 2, "|", 0 ** 0, "|", 0 ** 5, "|", (-2) ** 3, "|", 2 ** 62, "|", 2 ** 63, "|", 3 ** 40;
/// ```
/// ```expect
/// 8|512|1|0|-8|4611686018427387904|9.2233720368548E+18|1.2157665459057E+19
/// ```
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "php's `(double) lval` and `pow()` over one, whose rounding is the behaviour being copied"
)]
pub fn power(base: i64, exponent: u64) -> Number {
    if exponent == 0 {
        return Number::Long(1);
    }
    if base == 0 {
        return Number::Long(0);
    }
    let (mut l1, mut l2, mut i) = (1_i64, base, exponent);
    while i >= 1 {
        if i % 2 == 1 {
            i -= 1;
            let Some(product) = l1.checked_mul(l2) else {
                // php's `dval * pow(l2, i)`, `dval` being the product
                // that overflowed.
                return Number::Double(l1 as f64 * l2 as f64 * (l2 as f64).powf(i as f64));
            };
            l1 = product;
        } else {
            i /= 2;
            let Some(square) = l2.checked_mul(l2) else {
                // php's `(double) l1 * pow(dval, i)`, `dval` being the
                // square that overflowed.
                let squared = l2 as f64 * l2 as f64;
                return Number::Double(l1 as f64 * squared.powf(i as f64));
            };
            l2 = square;
        }
    }
    Number::Long(l1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row is php's own: what `"<string>" + 0` prints under the
    /// pin, and whether it warns, read off the oracle when the rows were
    /// written.
    #[test]
    fn a_string_reads_as_php_s_is_numeric_string_ex_reads_it() {
        for (text, number, trailing) in [
            ("1", Number::Long(1), false),
            (" \t\n\r\x0b\x0c1", Number::Long(1), false),
            ("1 \t\n\r\x0b\x0c", Number::Long(1), false),
            ("+2", Number::Long(2), false),
            ("-2", Number::Long(-2), false),
            ("007", Number::Long(7), false),
            ("1.5", Number::Double(1.5), false),
            (".5", Number::Double(0.5), false),
            ("1.", Number::Double(1.0), false),
            ("-.5", Number::Double(-0.5), false),
            ("1e3", Number::Double(1000.0), false),
            ("1E+3", Number::Double(1000.0), false),
            ("1.5e-1", Number::Double(0.15), false),
            ("1.e2", Number::Double(100.0), false),
            ("1e999", Number::Double(f64::INFINITY), false),
            ("9223372036854775807", Number::Long(i64::MAX), false),
            (
                "9223372036854775808",
                Number::Double(9_223_372_036_854_775_808.0),
                false,
            ),
            ("-9223372036854775808", Number::Long(i64::MIN), false),
            (
                "-9223372036854775809",
                Number::Double(-9_223_372_036_854_775_809.0),
                false,
            ),
            (
                "123456789012345678901234",
                Number::Double(1.234_567_890_123_456_8e23),
                false,
            ),
            ("1abc", Number::Long(1), true),
            ("1 abc", Number::Long(1), true),
            ("1e", Number::Long(1), true),
            ("1e+", Number::Long(1), true),
            ("1.5.5", Number::Double(1.5), true),
            ("1e5.5", Number::Double(100_000.0), true),
            ("0x1A", Number::Long(0), true),
            ("1\0", Number::Long(1), true),
        ] {
            assert_eq!(
                numeric_prefix(text.as_bytes()),
                Some((number, trailing)),
                "{text:?}"
            );
        }
        for text in [
            "", " ", "abc", ".", "-", "+", "-.", ".e5", "e5", "\x001", " abc 1",
        ] {
            assert_eq!(numeric_prefix(text.as_bytes()), None, "{text:?}");
        }
    }
}
