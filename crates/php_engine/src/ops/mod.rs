//! The kernels (design §5.8): one semantic in one function, called by the
//! handlers, by `api` and, when the compiled tier lands, by the kernel
//! list `tier.lock` names. A handler above them does operand addressing
//! and nothing else, so every caller of a semantic gets the same one.
//!
//! Each kernel carries a witness, which is the manifest's
//! `witness.coverage` rule over this directory.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use crate::api::raising::{Thrown, throw, warning};
use crate::api::request::Request;
use crate::api::values::{PhpArray, PhpString, TypeTag, Value, ValueRef};

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

/// `zend_is_true` (`Z/Zend/zend_operators.h:409`, `i_zend_is_true`): a
/// value as a condition reads it, which is what `JMPNZ` and its kin
/// test and what a bool parameter coerces to. `null`, `false`, `0`,
/// `0.0`, the empty string, `"0"` and the empty array are false and
/// everything else is true; `"00"` and `" "` among the strings, and
/// `NAN` among the doubles, with the warning 8.5 raises for a NAN
/// coerced to bool. An object is true unless its handlers say
/// otherwise, and no class here has handlers of its own yet.
///
/// Each false value ends a loop before its body runs, and a `while`
/// tests it where php's compiler leaves it, at the `JMPNZ`; a true one
/// would go on until the execution timer, which is the witness beside
/// [`crate::interrupt::interrupted`].
///
/// ```php
/// while (0) echo 'a'; while (0.0) echo 'b'; while ('') echo 'c'; while ('0') echo 'd'; while (null) echo 'e'; while (false) echo 'f'; while (extension_loaded('nope')) echo 'g'; for (; 1, 0;) echo 'h';
/// echo 'none ran';
/// ```
/// ```expect
/// none ran
/// ```
///
/// A NAN is true, so `memory_get_usage(NAN)` reads the same number as
/// `memory_get_usage(true)`, and it warns on the way.
///
/// ```php
/// var_dump(memory_get_usage(NAN) - memory_get_usage(true));
/// ```
/// ```expect
/// Warning: unexpected NAN value was coerced to bool in %s on line 2
/// int(0)
/// ```
pub(crate) fn is_true(req: &mut Request, value: &Value) -> bool {
    match value.deref().as_ref() {
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false) => false,
        ValueRef::Bool(true) | ValueRef::Object(_) => true,
        ValueRef::Int(n) => n != 0,
        ValueRef::Float(f) => {
            if f.is_nan() {
                warning(req, "unexpected NAN value was coerced to bool");
            }
            f != 0.0
        }
        ValueRef::Str(s) => !(s.is_empty() || s.as_bytes() == b"0"),
        ValueRef::Array(a) => !a.is_empty(),
        ValueRef::Resource(r) => r.id != 0,
        // `deref` followed the reference already.
        ValueRef::Reference(r) => is_true(req, r.get()),
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

/// `add_function` (`Zend/zend_operators.c:1200`), its fast type pairs and
/// the slow path behind them: the sum of two longs, promoted to a double
/// where it overflowed; a double where either side is one; two arrays
/// merged with the left side's keys kept (`add_function_array`); and
/// `zend_binop_error`'s `TypeError` for an operand php will not read as a
/// number.
///
/// Each fence calls a native so that php's compiler leaves the opcode
/// standing: `zend_try_ct_eval_binary_op` folds arithmetic over two
/// constants where it is written, and what a call returned is constant to
/// no compiler.
///
/// ```php
/// echo 1 + (2 + extension_loaded('Core')), "|", 1.5 + (2.5 + extension_loaded('Core')), "|", "1" + (1 + extension_loaded('Core')), "|", 9223372036854775807 + (1 + extension_loaded('Core'));
/// ```
/// ```expect
/// 4|5|3|9.2233720368548E+18
/// ```
///
/// A string with a number in front of it and anything else behind it is
/// php's warning and the number it read (`_is_numeric_string_ex` with
/// `allow_errors`), which is why php leaves the opcode standing for one.
///
/// ```php
/// echo "n:", "1abc" + (1 + extension_loaded('Core'));
/// ```
/// ```expect
/// n:
/// Warning: A non-numeric value encountered in %s on line 2
/// 3
/// ```
pub(crate) fn add(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    if let (Some(a), Some(b)) = (left.deref().as_array(), right.deref().as_array()) {
        return Ok(Value::array(merged(a, b)));
    }
    let (a, b) = numbers(req, "+", left, right)?;
    Ok(match (a, b) {
        (Number::Long(a), Number::Long(b)) => promoted(a.checked_add(b), wide(a) + wide(b)),
        (a, b) => Value::float(a.double() + b.double()),
    })
}

/// `sub_function` (`Zend/zend_operators.c:1259`): the same type pairs as
/// [`add`] without the array one, which php refuses for every operator
/// but `+`.
///
/// ```php
/// echo 1 - (2 - extension_loaded('Core')), "|", 1.5 - (2.5 - extension_loaded('Core')), "|", "3" - (1 + extension_loaded('Core')), "|", -9223372036854775807 - (1 + extension_loaded('Core'));
/// ```
/// ```expect
/// 0|0|1|-9.2233720368548E+18
/// ```
pub(crate) fn sub(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    let (a, b) = numbers(req, "-", left, right)?;
    Ok(match (a, b) {
        (Number::Long(a), Number::Long(b)) => promoted(a.checked_sub(b), wide(a) - wide(b)),
        (a, b) => Value::float(a.double() - b.double()),
    })
}

/// `mul_function` (`Zend/zend_operators.c:1322`), whose long pair is
/// `ZEND_SIGNED_MULTIPLY_LONG`: the product, or the double it overflowed
/// into.
///
/// ```php
/// echo 2 * (3 * extension_loaded('Core')), "|", 1.5 * (2.5 * extension_loaded('Core')), "|", "3" * (1 + extension_loaded('Core')), "|", 4611686018427387904 * (1 + extension_loaded('Core'));
/// ```
/// ```expect
/// 6|3.75|6|9.2233720368548E+18
/// ```
pub(crate) fn mul(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    let (a, b) = numbers(req, "*", left, right)?;
    Ok(match (a, b) {
        (Number::Long(a), Number::Long(b)) => promoted(a.checked_mul(b), wide(a) * wide(b)),
        (a, b) => Value::float(a.double() * b.double()),
    })
}

/// `div_function` and `div_function_base`
/// (`Zend/zend_operators.c:1454`): a long where the division comes out
/// whole and a double where it does not, php's own guard against
/// `ZEND_LONG_MIN / -1`, and `DivisionByZeroError` thrown where the
/// opcode stands for a divisor of zero, `-0.0` among them.
///
/// ```php
/// echo 6 / (1 + extension_loaded('Core')), "|", 7.5 / (1.5 * extension_loaded('Core')), "|", "7" / (1 + extension_loaded('Core')), "|", (-9223372036854775807 - extension_loaded('Core')) / (0 - extension_loaded('Core'));
/// ```
/// ```expect
/// 3|5|3.5|9.2233720368548E+18
/// ```
pub(crate) fn div(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    let (a, b) = numbers(req, "/", left, right)?;
    match (a, b) {
        (Number::Long(a), Number::Long(b)) => {
            if b == 0 {
                return Err(throw(req, "DivisionByZeroError", "Division by zero"));
            }
            // php's own case, "prevent overflow error/crash".
            if b == -1 && a == i64::MIN {
                return Ok(Value::float(wide(a) / -1.0));
            }
            Ok(if a % b == 0 {
                Value::int(a / b)
            } else {
                Value::float(wide(a) / wide(b))
            })
        }
        (a, b) => {
            let (a, b) = (a.double(), b.double());
            if b == 0.0 {
                return Err(throw(req, "DivisionByZeroError", "Division by zero"));
            }
            Ok(Value::float(a / b))
        }
    }
}

/// One operand of php's arithmetic: the two number types the
/// `*_function_fast` type pairs switch on.
#[derive(Clone, Copy)]
enum Number {
    Long(i64),
    Double(f64),
}

impl Number {
    /// The operand as the double php's mixed type pairs convert it to.
    fn double(self) -> f64 {
        match self {
            Number::Long(n) => wide(n),
            Number::Double(d) => d,
        }
    }
}

/// php's `(double) lval`, whose rounding is the behaviour being copied.
#[expect(
    clippy::cast_precision_loss,
    reason = "php's own conversion, which this exists to reproduce"
)]
fn wide(n: i64) -> f64 {
    n as f64
}

/// What `fast_long_add_function` and its kin leave: the long where the
/// operation stayed in range, and the double php computed again where it
/// did not.
fn promoted(stayed: Option<i64>, overflowed: f64) -> Value {
    stayed.map_or_else(|| Value::float(overflowed), Value::int)
}

/// `add_function_array`: the left array with every key of the right one
/// it does not already carry, which is `zend_hash_merge` with php's
/// `overwrite` off.
fn merged(left: &PhpArray, right: &PhpArray) -> PhpArray {
    let mut union = left.clone();
    for (key, value) in right.snapshot() {
        if !union.contains(&key) {
            union.set(key, value);
        }
    }
    union
}

/// Both operands as `zendi_try_convert_scalar_to_number` converts them,
/// or `zend_binop_error`'s `TypeError`, which names php's types around
/// the operator as written. php converts the left operand first and stops
/// there when it fails, so a right operand that would warn does not.
fn numbers(
    req: &mut Request,
    operator: &str,
    left: &Value,
    right: &Value,
) -> Result<(Number, Number), Thrown> {
    let (left, right) = (left.deref(), right.deref());
    let unsupported = |req: &mut Request| {
        throw(
            req,
            "TypeError",
            &format!(
                "Unsupported operand types: {} {operator} {}",
                left.type_name(),
                right.type_name()
            ),
        )
    };
    let Some(a) = to_number(req, left) else {
        return Err(unsupported(req));
    };
    let Some(b) = to_number(req, right) else {
        return Err(unsupported(req));
    };
    Ok((a, b))
}

/// `zendi_try_convert_scalar_to_number`: the number php works in, or
/// nothing for an array, an object or a resource, which is the failure
/// `zend_binop_error` reports. A string is read by [`numeric_prefix`],
/// and one with anything past the number it opens with raises php's
/// warning and keeps the number.
fn to_number(req: &mut Request, value: &Value) -> Option<Number> {
    if let Some(n) = value.as_int() {
        return Some(Number::Long(n));
    }
    if let Some(f) = value.as_float() {
        return Some(Number::Double(f));
    }
    if let Some(s) = value.as_php_string() {
        let (number, trailing) = numeric_prefix(s.as_bytes())?;
        if trailing {
            warning(req, "A non-numeric value encountered");
        }
        return Some(number);
    }
    if let Some(b) = value.as_bool() {
        return Some(Number::Long(i64::from(b)));
    }
    match value.tag() {
        // php's `IS_NULL` arm; an uninitialised zval reads as null here.
        TypeTag::Null | TypeTag::Undef => Some(Number::Long(0)),
        _ => None,
    }
}

/// `_is_numeric_string_ex` with php's `allow_errors` on
/// (`Zend/zend_operators.c:3620`): the number the string opens with after
/// any leading whitespace, and whether anything but trailing whitespace
/// follows it. Nothing at all for a string that opens with no number,
/// which is what makes `"abc" + 1` php's `TypeError`. An integer too wide
/// for a long is a double, as php's `MAX_LENGTH_OF_LONG` arm leaves it.
fn numeric_prefix(bytes: &[u8]) -> Option<(Number, bool)> {
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
