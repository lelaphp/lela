//! The kernels (design §5.8): one semantic in one function, called by the
//! handlers, by `api` and, when the compiled tier lands, by the kernel
//! list `tier.lock` names. A handler above them does operand addressing
//! and nothing else, so every caller of a semantic gets the same one.
//!
//! Each kernel carries a witness, which is the manifest's
//! `witness.coverage` rule over this directory.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use crate::api::raising::warning;
use crate::api::request::Request;
use crate::api::values::{PhpString, Value};

/// `zval_get_string`: a value as the string php converts it to, which is
/// what `ECHO`, `print` and string interpolation write.
///
/// ```php
/// echo 1, "|", true, "|", false, "|", null, "|", "as written", "|", 2.5, "|", NAN;
/// ```
/// ```expect
/// 1|1|||as written|2.5|
/// Warning: unexpected NAN value was coerced to string in %s on line 2
/// NAN
/// ```
pub(crate) fn to_string(req: &mut Request, value: &Value) -> PhpString {
    let value = value.deref();
    if let Some(s) = value.as_php_string() {
        return s.clone();
    }
    if let Some(n) = value.as_int() {
        return PhpString::from_bytes(n.to_string().as_bytes());
    }
    if let Some(f) = value.as_float() {
        // `zend_double_to_str`: the text, then the warning php raises
        // when the value it converted was NAN.
        let text = double_to_string(f, req.precision());
        if f.is_nan() {
            warning(req, "unexpected NAN value was coerced to string");
        }
        return text;
    }
    match value.as_bool() {
        Some(true) => PhpString::from_bytes(b"1"),
        // `false` and `null` are both the empty string.
        _ => PhpString::default(),
    }
}

/// `zend_gcvt` (`Zend/zend_strtod.c:4509`), which every float php prints
/// goes through: `precision` significant digits when it is not negative,
/// and the shortest text that reads back as the same double when it is,
/// which is what `serialize_precision = -1` asks for. The form is php's:
/// exponential when the decimal point stands past `ndigit` digits or more
/// than three places before them, fixed otherwise.
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
pub(crate) fn double_to_string(value: f64, precision: i32) -> PhpString {
    if value.is_nan() {
        return PhpString::from_bytes(b"NAN");
    }
    if value.is_infinite() {
        return PhpString::from_bytes(if value > 0.0 { b"INF" } else { b"-INF" });
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
        exponential(&mut out, &digits, point - 1);
    } else {
        fixed(&mut out, &digits, point);
    }
    PhpString::from_bytes(out.as_bytes())
}

/// `zend_dtoa`'s two answers for a value that is finite and not negative:
/// the significant digits with their trailing zeros dropped, and where
/// the decimal point stands among them (php's `decpt`, one past the last
/// digit before it). A `precision` that is not negative asks for that
/// many digits, and a negative one for the shortest that reads back as
/// the same double; Rust's formatter gives both.
fn significant(value: f64, precision: i32) -> (String, i32) {
    let rendered = if precision < 0 {
        format!("{value:e}")
    } else {
        let ndigit = usize::try_from(precision.max(1)).unwrap_or(17).clamp(1, 17);
        format!("{value:.*e}", ndigit - 1)
    };
    let (mantissa, exponent) = rendered.split_once('e').unwrap_or((rendered.as_str(), "0"));
    let mut digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    (digits, exponent.parse().unwrap_or(0) + 1)
}

/// `zend_gcvt`'s E-style: one digit, the point, the rest or a zero, and
/// the exponent with its sign and no padding.
fn exponential(out: &mut String, digits: &str, exponent: i32) {
    let (first, rest) = digits.split_at(1.min(digits.len()));
    out.push_str(first);
    out.push('.');
    out.push_str(if rest.is_empty() { "0" } else { rest });
    out.push('E');
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
                double_to_string(value, 14).as_bytes(),
                text.as_bytes(),
                "{value} at precision 14"
            );
        }
        assert_eq!(double_to_string(f64::NAN, 14).as_bytes(), b"NAN");

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
                double_to_string(value, -1).as_bytes(),
                text.as_bytes(),
                "{value} at serialize_precision -1"
            );
        }
    }
}
