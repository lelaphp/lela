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

/// `zend_gcvt` at the `precision` ini entry, php's `%.*G`: `precision`
/// significant digits, the trailing zeros of the fraction dropped, and
/// the exponential form when the exponent is below -4 or at `precision`.
/// A `precision` of zero is one digit, as `zend_double_to_str` passes it.
///
/// ```php
/// echo 2.5, "|", 1.0, "|", 0.1 + 0.2, "|", 1e25, "|", 0.00001, "|", 1/3, "|", -0.0, "|", INF, "|", -INF;
/// ```
/// ```expect
/// 2.5|1|0.3|1.0E+25|1.0E-5|0.33333333333333|-0|INF|-INF
/// ```
pub(crate) fn double_to_string(value: f64, precision: i32) -> PhpString {
    if value.is_nan() {
        return PhpString::from_bytes(b"NAN");
    }
    if value.is_infinite() {
        return PhpString::from_bytes(if value > 0.0 { b"INF" } else { b"-INF" });
    }
    let digits = if precision == 0 { 1 } else { precision };
    let significant = usize::try_from(digits).unwrap_or(14).clamp(1, 17);
    if value == 0.0 {
        return PhpString::from_bytes(if value.is_sign_negative() {
            b"-0"
        } else {
            b"0"
        });
    }
    // php's `zend_gcvt` chooses the form from the decimal exponent of the
    // value rounded to `precision` digits, not of the value itself.
    let rounded: f64 = format!("{value:.*e}", significant - 1)
        .parse()
        .unwrap_or(value);
    let exponent = exponent_of(rounded);
    let text = if exponent < -4 || exponent >= digits {
        let mantissa = trimmed(&format!(
            "{:.*}",
            significant - 1,
            rounded / powers(exponent)
        ));
        let mantissa = if mantissa.contains('.') {
            mantissa
        } else {
            format!("{mantissa}.0")
        };
        format!(
            "{mantissa}E{}{}",
            if exponent < 0 { "-" } else { "+" },
            exponent.abs()
        )
    } else {
        let places = usize::try_from(digits - 1 - exponent).unwrap_or(0);
        trimmed(&format!("{rounded:.places$}"))
    };
    PhpString::from_bytes(text.as_bytes())
}

/// The decimal exponent of a value that is finite and not zero: the `E`
/// of its own shortest exponential rendering, read rather than computed,
/// so it is the one the formatter will use.
fn exponent_of(value: f64) -> i32 {
    format!("{value:e}")
        .split_once('e')
        .and_then(|(_, e)| e.parse().ok())
        .unwrap_or(0)
}

/// Ten to the power of `exponent`, through the text form so that it is
/// the nearest double to the decimal power rather than a product of
/// roundings.
fn powers(exponent: i32) -> f64 {
    format!("1e{exponent}").parse().unwrap_or(1.0)
}

/// A fixed-point rendering with the trailing zeros of its fraction, and
/// then a bare decimal point, dropped: `2.500` as `2.5`, `1.000` as `1`.
fn trimmed(text: &str) -> String {
    if !text.contains('.') {
        return text.to_owned();
    }
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_double_is_rendered_as_php_renders_it() {
        for (value, text) in [
            (2.5_f64, "2.5"),
            (1.0, "1"),
            (0.1 + 0.2, "0.3"),
            (1e25, "1.0E+25"),
            (0.00001, "1.0E-5"),
            (1.0 / 3.0, "0.33333333333333"),
            (-0.0, "-0"),
            (f64::INFINITY, "INF"),
            (f64::NEG_INFINITY, "-INF"),
        ] {
            assert_eq!(
                double_to_string(value, 14).as_bytes(),
                text.as_bytes(),
                "{value}"
            );
        }
        assert_eq!(double_to_string(f64::NAN, 14).as_bytes(), b"NAN");
    }
}
