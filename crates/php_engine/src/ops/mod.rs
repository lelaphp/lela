//! The kernels (design §5.8): one semantic in one function, called by the
//! handlers, by `api` and, when the compiled tier lands, by the kernel
//! list `tier.lock` names. A handler above them does operand addressing
//! and nothing else, so every caller of a semantic gets the same one.
//!
//! Each kernel carries a witness, which is the manifest's
//! `witness.coverage` rule over this directory.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_heap::{Number, double_to_string, numeric_prefix};

use crate::api::raising::{Thrown, deprecated, throw, warning};
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
        let text = PhpString::from_bytes(double_to_string(f, req.precision()).as_bytes());
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
///
/// The divisor of zero is the one pair php's compiler will not fold, so
/// two constants exercise the opcode in both engines; uncaught, the
/// throwable names the opline's line as its site.
///
/// ```php
/// echo 1/0;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught DivisionByZeroError: Division by zero in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
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

/// `mod_function` (`Zend/zend_operators.c:1541`): both operands as the
/// longs `convert_op1_op2_long` makes of them, the left one first,
/// `DivisionByZeroError` with php's own text for a divisor of zero, and
/// php's `% -1` case, which is zero where the division would overflow.
///
/// ```php
/// echo 7 % (2 * extension_loaded('Core')), '|', -7 % (2 * extension_loaded('Core')), '|', '7' % (2 * extension_loaded('Core')), '|', true % (2 * extension_loaded('Core')), '|', (-9223372036854775807 - extension_loaded('Core')) % (0 - extension_loaded('Core'));
/// ```
/// ```expect
/// 1|-1|1|1|0
/// ```
///
/// A double is what `zend_dval_to_lval_safe` makes of it: truncated,
/// with the deprecation where the truncation loses something; past a
/// long's range, php's warning and its modular reading
/// (`zend_dval_to_lval_slow`); and for NAN, which `ZEND_DOUBLE_FITS_LONG`
/// does not refuse, both.
///
/// ```php
/// echo 7.5 % (2 * extension_loaded('Core')), '|', 8.0 % (3 * extension_loaded('Core')), '|', 1e19 % (7 * extension_loaded('Core')), '|', NAN % (2 * extension_loaded('Core'));
/// ```
/// ```expect
///
/// Deprecated: Implicit conversion from float 7.5 to int loses precision in %s on line 2
/// 1|2|
/// Warning: The float 1.0E+19 is not representable as an int, cast occurred in %s on line 2
/// -6|
/// Warning: The float NAN is not representable as an int, cast occurred in %s on line 2
///
/// Deprecated: Implicit conversion from float NAN to int loses precision in %s on line 2
/// 0
/// ```
///
/// A string is read by [`numeric_prefix`] with the warning for what
/// follows the number, then a double among them is capped to the long's
/// range (`zend_dval_to_lval_cap`) with the float-string deprecation
/// where the cap or the truncation lost something.
///
/// ```php
/// echo '7abc' % (2 * extension_loaded('Core')), '|', '1.5' % (2 * extension_loaded('Core')), '|', '1e19' % (2 * extension_loaded('Core'));
/// ```
/// ```expect
///
/// Warning: A non-numeric value encountered in %s on line 2
/// 1|
/// Deprecated: Implicit conversion from float-string "1.5" to int loses precision in %s on line 2
/// 1|
/// Deprecated: Implicit conversion from float-string "1e19" to int loses precision in %s on line 2
/// 1
/// ```
///
/// A divisor of zero is the one pair php's compiler will not fold, so
/// two constants exercise the opcode in both engines.
///
/// ```php
/// echo 1 % 0;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught DivisionByZeroError: Modulo by zero in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn r#mod(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    let (a, b) = longs(req, "%", left, right)?;
    if b == 0 {
        return Err(throw(req, "DivisionByZeroError", "Modulo by zero"));
    }
    // php's own case, "prevent overflow error/crash if op1==LONG_MIN".
    if b == -1 {
        return Ok(Value::int(0));
    }
    Ok(Value::int(a % b))
}

/// `pow_function` and `pow_function_base`
/// (`Zend/zend_operators.c:1358`): [`php_heap::power`] over a long pair
/// whose exponent is not negative, and `safe_pow` over two doubles for
/// every other pair, which is C's `pow` behind the deprecation php
/// raises for a zero base and a negative exponent.
///
/// ```php
/// echo 2 ** (3 * extension_loaded('Core')), '|', 2 ** (0.5 * extension_loaded('Core')), '|', 2.5 ** (2 * extension_loaded('Core')), '|', 2 ** (-1 * extension_loaded('Core')), '|', '3' ** (2 * extension_loaded('Core')), '|', 2 ** (63 * extension_loaded('Core')), '|', 3 ** (40 * extension_loaded('Core'));
/// ```
/// ```expect
/// 8|1.4142135623731|6.25|0.5|9|9.2233720368548E+18|1.2157665459057E+19
/// ```
///
/// The zero base under a negative exponent is the one pair php's
/// compiler will not fold, so two constants exercise the opcode in both
/// engines; at the pin the deprecation is what php raises there, and
/// `INF` is what `pow` computes.
///
/// ```php
/// echo 0 ** -1, '|', 0.0 ** (-1 * extension_loaded('Core'));
/// ```
/// ```expect
///
/// Deprecated: Power of base 0 and negative exponent is deprecated in %s on line 2
/// INF|
/// Deprecated: Power of base 0 and negative exponent is deprecated in %s on line 2
/// INF
/// ```
pub(crate) fn pow(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    let (a, b) = numbers(req, "**", left, right)?;
    if let (Number::Long(a), Number::Long(b)) = (a, b)
        && let Ok(exponent) = u64::try_from(b)
    {
        return Ok(match php_heap::power(a, exponent) {
            Number::Long(n) => Value::int(n),
            Number::Double(d) => Value::float(d),
        });
    }
    let (base, exponent) = (a.double(), b.double());
    if base == 0.0 && exponent < 0.0 {
        deprecated(req, "Power of base 0 and negative exponent is deprecated");
    }
    Ok(Value::float(base.powf(exponent)))
}

/// `shift_left_function` (`Zend/zend_operators.c:1944`):
/// [`php_heap::shift_left`] over the longs `convert_op1_op2_long` makes
/// of both sides, and `ArithmeticError` thrown where the opcode stands
/// for a negative count.
///
/// ```php
/// echo 1 << (3 * extension_loaded('Core')), "|", 1 << (64 * extension_loaded('Core')), "|", -1 << (1 * extension_loaded('Core')), "|", "2" << (1 * extension_loaded('Core')), "|", 1 << (2.0 * extension_loaded('Core')), "|", 1.5 << (1 * extension_loaded('Core'));
/// ```
/// ```expect
/// 8|0|-2|4|4|
/// Deprecated: Implicit conversion from float 1.5 to int loses precision in %s on line 2
/// 2
/// ```
///
/// The negative count is the one pair php's compiler will not fold, so
/// two constants exercise the opcode in both engines.
///
/// ```php
/// echo 1 << -1;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArithmeticError: Bit shift by negative number in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn shift_left(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    let (a, b) = longs(req, "<<", left, right)?;
    match u64::try_from(b) {
        Ok(by) => Ok(Value::int(php_heap::shift_left(a, by))),
        Err(_) => Err(throw(
            req,
            "ArithmeticError",
            "Bit shift by negative number",
        )),
    }
}

/// `shift_right_function` (`Zend/zend_operators.c:1981`):
/// [`php_heap::shift_right`], the same way.
///
/// ```php
/// echo 8 >> (1 * extension_loaded('Core')), "|", -8 >> (1 * extension_loaded('Core')), "|", -1 >> (64 * extension_loaded('Core')), "|", 1 >> (64 * extension_loaded('Core')), "|", "8" >> (2 * extension_loaded('Core'));
/// ```
/// ```expect
/// 4|-4|-1|0|2
/// ```
///
/// ```php
/// echo 1 >> -1;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArithmeticError: Bit shift by negative number in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn shift_right(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    let (a, b) = longs(req, ">>", left, right)?;
    match u64::try_from(b) {
        Ok(by) => Ok(Value::int(php_heap::shift_right(a, by))),
        Err(_) => Err(throw(
            req,
            "ArithmeticError",
            "Bit shift by negative number",
        )),
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

/// Both operands as `convert_op1_op2_long` converts them, which is
/// [`to_long`] over the left one and then the right, or
/// `zend_binop_error`'s `TypeError` for the one that failed, which names
/// php's types around the operator as written.
fn longs(
    req: &mut Request,
    operator: &str,
    left: &Value,
    right: &Value,
) -> Result<(i64, i64), Thrown> {
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
    let Some(a) = to_long(req, left) else {
        return Err(unsupported(req));
    };
    let Some(b) = to_long(req, right) else {
        return Err(unsupported(req));
    };
    Ok((a, b))
}

/// `zendi_try_get_long`: the long php works in, or nothing for an array,
/// an object or a resource, which is the failure `zend_binop_error`
/// reports. A double goes through [`double_to_long`]; a string is read
/// by [`numeric_prefix`] with php's warning for what follows its number,
/// and a double among them is `zend_dval_to_lval_cap`'s saturated long,
/// with the float-string deprecation where that lost something.
fn to_long(req: &mut Request, value: &Value) -> Option<i64> {
    if let Some(n) = value.as_int() {
        return Some(n);
    }
    if let Some(d) = value.as_float() {
        return Some(double_to_long(req, d));
    }
    if let Some(s) = value.as_php_string() {
        let (number, trailing) = numeric_prefix(s.as_bytes())?;
        if trailing {
            warning(req, "A non-numeric value encountered");
        }
        return Some(match number {
            Number::Long(n) => n,
            Number::Double(d) => {
                // `zend_dval_to_lval_cap`: zero for what is not finite, and
                // the long's own edge past its range.
                let capped = if d.is_finite() { truncated(d) } else { 0 };
                if !long_compatible(d, capped) {
                    deprecated(
                        req,
                        &format!(
                            "Implicit conversion from float-string \"{}\" to int loses precision",
                            String::from_utf8_lossy(s.as_bytes())
                        ),
                    );
                }
                capped
            }
        });
    }
    if let Some(b) = value.as_bool() {
        return Some(i64::from(b));
    }
    match value.tag() {
        // php's `IS_NULL` arm; an uninitialised zval reads as null here.
        TypeTag::Null | TypeTag::Undef => Some(0),
        _ => None,
    }
}

/// `zend_dval_to_lval_safe`: `zend_dval_to_lval`, which is the truncated
/// long for a double in range and, with php's warning, zero for one that
/// is not finite and the modular reading of `zend_dval_to_lval_slow` for
/// one past the range; then the deprecation where the double fits and
/// the long is not it. `ZEND_DOUBLE_FITS_LONG` is two comparisons NAN
/// fails neither of, so NAN is warned on and then deprecated.
fn double_to_long(req: &mut Request, d: f64) -> i64 {
    let oob = |req: &mut Request| {
        let text = rendered(d);
        warning(
            req,
            &format!("The float {text} is not representable as an int, cast occurred"),
        );
    };
    let l = if !d.is_finite() {
        oob(req);
        0
    } else if !fits_long(d) {
        oob(req);
        modular(d)
    } else {
        truncated(d)
    };
    if !long_compatible(d, l) && fits_long(d) {
        let text = rendered(d);
        deprecated(
            req,
            &format!("Implicit conversion from float {text} to int loses precision"),
        );
    }
    l
}

/// A double as php's `%H` renders it in a message: `zend_gcvt` at the
/// shortest digits, which is [`double_to_string`] under
/// `serialize_precision`'s -1.
fn rendered(d: f64) -> String {
    String::from_utf8_lossy(double_to_string(d, -1).as_bytes()).into_owned()
}

/// `ZEND_DOUBLE_FITS_LONG` on a 64-bit long: `(double) ZEND_LONG_MAX`
/// rounds up out of the range, so the upper edge is exclusive. Written
/// as php writes it, two comparisons NAN fails neither of, which a range
/// test would turn into the one NAN fails.
#[expect(
    clippy::manual_range_contains,
    reason = "php's own two comparisons, whose answer for NAN is the behaviour being copied"
)]
fn fits_long(d: f64) -> bool {
    !(d >= 9_223_372_036_854_775_808.0 || d < -9_223_372_036_854_775_808.0)
}

/// `zend_is_long_compatible`: php's `(double) l == d`.
#[expect(
    clippy::float_cmp,
    reason = "php's `(double) l == d`, the exact equality being copied"
)]
fn long_compatible(d: f64, l: i64) -> bool {
    wide(l) == d
}

/// php's `(zend_long) d` over a double that fits, and past the range
/// the long's own edge, which is `zend_dval_to_lval_cap`'s answer and
/// the cast's.
#[expect(
    clippy::cast_possible_truncation,
    reason = "php's `(zend_long) d`, whose truncation is the behaviour being copied"
)]
fn truncated(d: f64) -> i64 {
    d as i64
}

/// `zend_dval_to_lval_slow` on a 64-bit long: the double modulo 2^64,
/// made positive, read as an unsigned long and then as the signed one
/// with the same bits. The double is past the long's range, so it is
/// whole and the remainder is too.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "php's `(zend_long)(zend_ulong) dmod`, the bit pattern being copied"
)]
fn modular(d: f64) -> i64 {
    let two_pow_64 = 18_446_744_073_709_551_616.0_f64;
    let mut dmod = d % two_pow_64;
    if dmod < 0.0 {
        dmod += two_pow_64;
    }
    (dmod as u64) as i64
}

/// `zendi_try_convert_scalar_to_number`: the number php works in, or
/// nothing for an array, an object or a resource, which is the failure
/// `zend_binop_error` reports. A string is read by [`numeric_prefix`],
/// the one reader php's compiler folds through as well, and one with
/// anything past the number it opens with raises php's warning and keeps
/// the number.
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
