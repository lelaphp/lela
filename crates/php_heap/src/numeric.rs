//! The numeric string and the double's text (design §5.2): what php
//! reads a string as when an operator wants a number,
//! `_is_numeric_string_ex` (`Z/Zend/zend_operators.c:3620`), and what it
//! writes a double as when an operator wants a string, `zend_gcvt`
//! (`Z/Zend/zend_strtod.c:4509`), each ported once. The compiler and the
//! kernels both read them, because php's compiler folds an operator over
//! two constants through the same `add_function` and `concat_function`
//! the opcode calls (`zend_try_ct_eval_binary_op`), so one copy of each
//! is what keeps the op array and the value in step with php's.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::cmp::Ordering;

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

    /// `zend_compare`'s four numeric pairs as one match, in its order: two
    /// longs as longs, two doubles as they are, and a mixed pair with its
    /// long converted, the doubles under `ZEND_THREEWAY_COMPARE`, equal, less,
    /// else greater, which is where a NAN lands whichever side it is on.
    /// The one home of the rule: [`crate::compare`] takes it for its
    /// numeric pairs and the engine's handlers for their inline ones
    /// (design §5.8), so `<` over two numbers is one function.
    #[must_use]
    #[inline]
    #[expect(
        clippy::float_cmp,
        reason = "php's `(a) == (b)`, the exact equality being copied"
    )]
    pub fn compare(self, other: Number) -> Ordering {
        let (a, b) = match (self, other) {
            (Number::Long(a), Number::Long(b)) => return a.cmp(&b),
            (Number::Double(a), Number::Double(b)) => (a, b),
            _ => (self.double(), other.double()),
        };
        if a == b {
            Ordering::Equal
        } else if a < b {
            Ordering::Less
        } else {
            Ordering::Greater
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
    let read = numeric_read(bytes)?;
    Some((read.number, read.trailing))
}

/// What `_is_numeric_string_ex` reports about one string: the number, whether
/// anything but whitespace follows it, and `oflow_info`, the sign of an
/// integer too wide for a long, which [`crate::compare`]'s
/// `zendi_smart_strcmp` reads to keep two such integers apart.
pub(crate) struct NumericRead {
    pub(crate) number: Number,
    pub(crate) trailing: bool,
    /// php's `oflow_info`: -1 or 1 for an integer that overflowed a long
    /// on that side, and 0 for every other number, a written double
    /// among them.
    pub(crate) overflow: i8,
}

/// `_is_numeric_string_ex` itself, which [`numeric_prefix`] and the
/// comparisons read through.
pub(crate) fn numeric_read(bytes: &[u8]) -> Option<NumericRead> {
    let space = |b: &u8| matches!(*b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let from = bytes.iter().position(|b| !space(b))?;
    let rest = bytes.get(from..)?;
    let negative = rest.first() == Some(&b'-');
    let mut at = usize::from(matches!(rest.first(), Some(b'-' | b'+')));
    let digits = at;
    // php skips the leading zeros before it counts digits, so
    // `MAX_LENGTH_OF_LONG` is a count of the significant ones.
    while rest.get(at) == Some(&b'0') {
        at += 1;
    }
    let significant = at;
    while rest.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    let counted = at - significant;
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
    let sign = if negative { -1 } else { 1 };
    if floating {
        // php stops counting at `MAX_LENGTH_OF_LONG` digits and reports
        // the overflow before it reads the point or the exponent.
        let overflow = if counted >= MAX_LENGTH_OF_LONG {
            sign
        } else {
            0
        };
        return Some(NumericRead {
            number: Number::Double(text.parse().ok()?),
            trailing,
            overflow,
        });
    }
    // php reads an integer too wide for a long as a double.
    let (number, overflow) = match text.parse::<i64>() {
        Ok(n) => (Number::Long(n), 0),
        Err(_) => (Number::Double(text.parse().ok()?), sign),
    };
    Some(NumericRead {
        number,
        trailing,
        overflow,
    })
}

/// php's `MAX_LENGTH_OF_LONG` on a 64-bit long: the digit count at
/// which an integer is a double before it is read to the end.
const MAX_LENGTH_OF_LONG: usize = 20;

/// The exponent character `zend_gcvt` is handed, the fourth of its
/// arguments: Zend prints every float with `E` (`smart_str_append_double`,
/// `zend_double_to_str`, `var_export`, `%G` and `%H`), and `e` is what
/// `php_json_encode_double` and `printf`'s `%g` and `%h` pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exponent {
    /// `'E'`, Zend's own.
    Upper,
    /// `'e'`, json's and `%g`'s.
    Lower,
}

impl Exponent {
    /// The character itself.
    #[must_use]
    pub fn as_char(self) -> char {
        match self {
            Self::Upper => 'E',
            Self::Lower => 'e',
        }
    }
}

/// `zend_gcvt` (`Zend/zend_strtod.c:4509`), which every float php prints
/// goes through: `precision` significant digits when it is not negative,
/// and the shortest text that reads back as the same double when it is,
/// which is what `serialize_precision = -1` asks for. The form is php's:
/// exponential when the decimal point stands past `ndigit` digits or more
/// than three places before them, fixed otherwise, with the exponent
/// character the caller passes, as php's callers each pass their own.
///
/// ```php
/// echo 2.5, "|", 1.0, "|", 0.1 + 0.2, "|", 1e25, "|", 0.00001, "|", 1/3, "|", -0.0, "|", INF, "|", -INF, "|", 5e-324, "|", 0.001;
/// ```
/// ```expect
/// 2.5|1|0.3|1.0E+25|1.0E-5|0.33333333333333|-0|INF|-INF|4.9406564584125E-324|0.001
/// ```
///
/// ```php
/// var_dump(0.1, 1/3, 1e25, 0.1 + 0.2, 1.0, 5e-324);
/// ```
/// ```expect
/// float(0.1)
/// float(0.3333333333333333)
/// float(1.0E+25)
/// float(0.30000000000000004)
/// float(1)
/// float(5.0E-324)
/// ```
///
/// ```php
/// echo json_encode([1e25, 1e-5, 0.1, -2.5e-7]), "|", sprintf('%g', 1e25), "|", sprintf('%G', 1e25), "|", var_export(1e-5, true);
/// ```
/// ```expect
/// [1.0e+25,1.0e-5,0.1,-2.5e-7]|1.0e+25|1.0E+25|1.0E-5
/// ```
///
/// Past seventeen digits php leaves the shortest text behind:
/// `zend_dtoa` takes `ndigit` under no cap of its own, so it renders the
/// double's decimal expansion to as many digits as were asked for, and a
/// `precision` or a `serialize_precision` set past seventeen reads those
/// digits and not the short form. The boundary is the third line here:
/// sixteen digits still read `0.1`, seventeen read the shortest text that
/// reads back, and eighteen open the expansion.
///
/// ```php
/// ini_set('precision', 20); echo 0.1, "|", 0.1 + 0.2, "|", 1/3, "|", 1e25, "|", 5e-324, "|", 1.0, "|", 1e-5, "\n";
/// foreach ([16, 17, 18] as $p) { ini_set('precision', $p); echo $p, ": ", 0.1, "|", 0.1 + 0.2, "|", 1/3, "\n"; }
/// ini_set('serialize_precision', 20); var_dump(0.1, 0.1 + 0.2); echo json_encode([0.1, 1e25]), "|", var_export(0.1, true);
/// ```
/// ```expect
/// 0.10000000000000000555|0.30000000000000004441|0.33333333333333331483|1.0000000000000000906E+25|4.9406564584124654418E-324|1|1.0000000000000000818E-5
/// 16: 0.1|0.3|0.3333333333333333
/// 17: 0.10000000000000001|0.30000000000000004|0.33333333333333331
/// 18: 0.100000000000000006|0.300000000000000044|0.333333333333333315
/// float(0.10000000000000000555)
/// float(0.30000000000000004441)
/// [0.10000000000000000555,1.0000000000000000906e+25]|0.10000000000000000555
/// ```
#[must_use]
pub fn double_to_string(value: f64, precision: i32, exponent: Exponent) -> String {
    if value.is_nan() {
        return "NAN".to_owned();
    }
    if value.is_infinite() {
        return (if value > 0.0 { "INF" } else { "-INF" }).to_owned();
    }
    // `zend_gcvt` asks `zend_dtoa` for the shortest digits when `ndigit`
    // is negative, and reads `ndigit` as 17 for the form it then chooses,
    // whatever those digits came to.
    let ndigit = if precision < 0 { 17 } else { precision.max(1) };
    let (digits, point) = significant(value.abs(), precision);
    let mut out = String::new();
    if value.is_sign_negative() {
        out.push('-');
    }
    if (point >= 0 && point > ndigit) || point < -3 {
        exponential(&mut out, &digits, point - 1, exponent);
    } else {
        fixed(&mut out, &digits, point);
    }
    out
}

/// `zend_dtoa`'s two answers for a value that is finite and not negative:
/// the significant digits with their trailing zeros dropped, and where
/// the decimal point stands among them (php's `decpt`, one past the last
/// digit before it). A `precision` that is not negative asks for that
/// many digits, and a negative one for the shortest that reads back as
/// the same double; Rust's formatter gives both.
///
/// Asked for a digit count, `zend_dtoa` rounds the double's own decimal
/// expansion to it, under no cap of its own: past seventeen those digits
/// are no longer the shortest text that reads back, they are the
/// expansion itself, which is why `precision = 20` writes `0.1` as
/// `0.10000000000000000555`. Rust's formatter rounds the same expansion
/// by the same rule, half to even, which is `zend_dtoa`'s where
/// `ROUND_BIASED` is not defined, as php does not define it.
fn significant(value: f64, precision: i32) -> (String, i32) {
    let rendered = if precision < 0 {
        format!("{value:e}")
    } else {
        let ndigit = usize::try_from(precision.max(1))
            .unwrap_or(EXACT_DIGITS)
            .min(EXACT_DIGITS);
        format!("{value:.*e}", ndigit - 1)
    };
    let (mantissa, exponent) = rendered.split_once('e').unwrap_or((rendered.as_str(), "0"));
    let mut digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    (digits, exponent.parse().unwrap_or(0) + 1)
}

/// The most significant digits a double's decimal expansion carries,
/// reached at the largest subnormal, `2.2250738585072009E-308`. Every
/// digit a larger `ndigit` asks `zend_dtoa` for is a trailing zero it
/// suppresses and the loop above drops, so asking for more renders the
/// same text and only reads more of the formatter's time.
const EXACT_DIGITS: usize = 767;

/// `zend_gcvt`'s E-style: one digit, the point, the rest or a zero, the
/// exponent character, and the exponent with its sign and no padding.
fn exponential(out: &mut String, digits: &str, exponent: i32, character: Exponent) {
    let (first, rest) = digits.split_at(1.min(digits.len()));
    out.push_str(first);
    out.push('.');
    out.push_str(if rest.is_empty() { "0" } else { rest });
    out.push(character.as_char());
    out.push(if exponent < 0 { '-' } else { '+' });
    out.push_str(&exponent.abs().to_string());
}

/// `zend_gcvt`'s standard format, its two arms: a point before every
/// digit when it stands before them, and among them otherwise, with a
/// leading zero where php writes one.
fn fixed(out: &mut String, digits: &str, point: i32) {
    if point < 0 {
        out.push_str("0.");
        for _ in point..0 {
            out.push('0');
        }
        out.push_str(digits);
        return;
    }
    let at = usize::try_from(point).unwrap_or(0);
    for i in 0..at {
        out.push(digits.as_bytes().get(i).map_or('0', |b| char::from(*b)));
    }
    if at < digits.len() {
        if at == 0 {
            out.push('0');
        }
        out.push('.');
        out.push_str(digits.get(at..).unwrap_or_default());
    }
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

/// `shift_left_function`'s long arm for a count that is not negative
/// (`Z/Zend/zend_operators.c:1944`): the shift over the unsigned value,
/// "to get well-defined wrap behavior", and zero for a count at or past
/// the long's width, php's guard against "wrapping quirkiness on some
/// processors where << 64 + x == << x". A negative count is php's
/// `ArithmeticError` and stays with the caller. Read by the compiler's
/// fold and by the kernel, as [`power`] is.
///
/// ```php
/// echo 1 << 3, "|", 1 << 63, "|", 1 << 64, "|", -1 << 1, "|", 3 << 62;
/// ```
/// ```expect
/// 8|-9223372036854775808|0|-2|-4611686018427387904
/// ```
#[must_use]
pub fn shift_left(value: i64, by: u64) -> i64 {
    match u32::try_from(by) {
        Ok(by) if by < i64::BITS => value.wrapping_shl(by),
        _ => 0,
    }
}

/// `shift_right_function`'s long arm for a count that is not negative
/// (`Z/Zend/zend_operators.c:1981`): C's arithmetic shift, and for a
/// count at or past the long's width the sign the shift would have
/// filled with, -1 for a negative value and zero for the rest.
///
/// ```php
/// echo 8 >> 1, "|", -8 >> 1, "|", 1 >> 64, "|", -1 >> 64, "|", -1 >> 63;
/// ```
/// ```expect
/// 4|-4|0|-1|-1
/// ```
#[must_use]
pub fn shift_right(value: i64, by: u64) -> i64 {
    match u32::try_from(by) {
        Ok(by) if by < i64::BITS => value >> by,
        _ if value < 0 => -1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_double_is_rendered_as_zend_gcvt_renders_it() {
        // `precision`, php's mode 2, is what `echo` converts at.
        for (value, text) in [
            (2.5_f64, "2.5"),
            (1.0, "1"),
            (0.1 + 0.2, "0.3"),
            (1e25, "1.0E+25"),
            (0.00001, "1.0E-5"),
            (0.001, "0.001"),
            (1.0 / 3.0, "0.33333333333333"),
            (-0.0, "-0"),
            (0.0, "0"),
            (-2.5, "-2.5"),
            (5e-324, "4.9406564584125E-324"),
            (f64::INFINITY, "INF"),
            (f64::NEG_INFINITY, "-INF"),
        ] {
            assert_eq!(
                double_to_string(value, 14, Exponent::Upper),
                text,
                "{value} at precision 14"
            );
        }
        assert_eq!(double_to_string(f64::NAN, 14, Exponent::Upper), "NAN");

        // `serialize_precision` at its -1 default, php's mode 0: the
        // shortest text that reads back as the same double.
        for (value, text) in [
            (0.1_f64, "0.1"),
            (1.0 / 3.0, "0.3333333333333333"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1.0, "1"),
            (1e25, "1.0E+25"),
            (5e-324, "5.0E-324"),
        ] {
            assert_eq!(
                double_to_string(value, -1, Exponent::Upper),
                text,
                "{value} at serialize_precision -1"
            );
        }

        // json's exponent character, and `%g`'s.
        assert_eq!(double_to_string(1e25, -1, Exponent::Lower), "1.0e+25");
        assert_eq!(double_to_string(1e-5, -1, Exponent::Lower), "1.0e-5");
        assert_eq!(double_to_string(0.1, -1, Exponent::Lower), "0.1");
    }

    /// Every row read off the oracle at the pin: `zend_dtoa` takes the
    /// digit count it was asked for under no cap, so past
    /// seventeen the text is the double's decimal expansion and not the
    /// shortest that reads back. The boundary rows are `0.1` at sixteen,
    /// seventeen and eighteen digits.
    #[test]
    fn a_precision_past_seventeen_renders_the_expansion_php_renders() {
        for (value, precision, text) in [
            (0.1_f64, 16, "0.1"),
            (0.1, 17, "0.10000000000000001"),
            (0.1, 18, "0.100000000000000006"),
            (0.1, 19, "0.1000000000000000056"),
            (0.1, 20, "0.10000000000000000555"),
            (0.1, 25, "0.1000000000000000055511151"),
            (0.1, 40, "0.1000000000000000055511151231257827021182"),
            (
                0.1,
                53,
                "0.10000000000000000555111512312578270211815834045410156",
            ),
            // Past the expansion's own 767 digits php renders no more,
            // since `zend_dtoa` suppresses the trailing zeros.
            (
                0.1,
                2000,
                "0.1000000000000000055511151231257827021181583404541015625",
            ),
            (0.1 + 0.2, 20, "0.30000000000000004441"),
            (1.0 / 3.0, 20, "0.33333333333333331483"),
            (1e25, 20, "1.0000000000000000906E+25"),
            // `decpt` is 26 and `ndigit` 30, so php's form test leaves
            // the E-style behind at a precision this wide.
            (1e25, 30, "10000000000000000905969664"),
            (5e-324, 20, "4.9406564584124654418E-324"),
            (5e-324, 18, "4.94065645841246544E-324"),
            (1e-5, 20, "1.0000000000000000818E-5"),
            (1.0, 20, "1"),
            (2.5, 20, "2.5"),
            (-0.1, 20, "-0.10000000000000000555"),
            // `zend_dtoa` rounds a tie to even where `ROUND_BIASED` is
            // not defined, and php does not define it.
            (1.25, 2, "1.2"),
            (1.35, 2, "1.4"),
            (2.5, 1, "2"),
            (1.5, 1, "2"),
            (0.5, 1, "0.5"),
        ] {
            assert_eq!(
                double_to_string(value, precision, Exponent::Upper),
                text,
                "{value} at precision {precision}"
            );
        }
        // json's and `%g`'s exponent character on the same digits.
        assert_eq!(
            double_to_string(0.1, 20, Exponent::Lower),
            "0.10000000000000000555"
        );
        assert_eq!(
            double_to_string(1e25, 20, Exponent::Lower),
            "1.0000000000000000906e+25"
        );
    }

    /// `oflow_info`: the sign of an integer that overflowed a long, set
    /// where php sets it, at twenty significant digits before the point
    /// is read and at nineteen past `long_min_digits`, and nowhere else.
    #[test]
    fn an_integer_too_wide_for_a_long_reports_the_side_it_overflowed_on() {
        for (text, overflow) in [
            ("9223372036854775807", 0),
            ("9223372036854775808", 1),
            ("-9223372036854775808", 0),
            ("-9223372036854775809", -1),
            ("00000000000000000000000001", 0),
            ("12345678901234567890.5", 1),
            ("-12345678901234567890e1", -1),
            ("1234567890123456789.5", 0),
            ("1e1000", 0),
        ] {
            let read = numeric_read(text.as_bytes());
            assert_eq!(read.map(|r| r.overflow), Some(overflow), "{text:?}");
        }
    }

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
