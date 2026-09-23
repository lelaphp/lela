//! The natives of `math.c`, and the rounding under them that other
//! files call: `_php_math_round` and `php_round_helper`, copied
//! (`Z/ext/standard/math.c:62-252`), which `round`, `number_format` and
//! `range` round with.

/// `PHP_ROUND_HALF_UP` and the modes beside it (`Z/ext/standard/php_math_modes.h`).
pub(crate) const PHP_ROUND_HALF_UP: i32 = 0x01;
pub(crate) const PHP_ROUND_HALF_DOWN: i32 = 0x02;
pub(crate) const PHP_ROUND_HALF_EVEN: i32 = 0x03;
pub(crate) const PHP_ROUND_HALF_ODD: i32 = 0x04;
pub(crate) const PHP_ROUND_CEILING: i32 = 0x05;
pub(crate) const PHP_ROUND_FLOOR: i32 = 0x06;
pub(crate) const PHP_ROUND_TOWARD_ZERO: i32 = 0x07;
pub(crate) const PHP_ROUND_AWAY_FROM_ZERO: i32 = 0x08;

/// `php_intpow10`: an exact power of ten from the table where it has
/// one, `pow` otherwise.
fn intpow10(power: i32) -> f64 {
    const POWERS: [f64; 23] = [
        1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
        1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
    ];
    match usize::try_from(power) {
        Ok(at) if at < POWERS.len() => POWERS[at],
        _ => 10f64.powi(power),
    }
}

fn basic_edge(integral: f64, exponent: f64, places: i32) -> f64 {
    let half = integral + 0.5f64.copysign(integral);
    if places > 0 {
        (half / exponent).abs()
    } else {
        (half * exponent).abs()
    }
}

fn zero_edge(integral: f64, exponent: f64, places: i32) -> f64 {
    if places > 0 {
        (integral / exponent).abs()
    } else {
        (integral * exponent).abs()
    }
}

/// `php_round_helper`: `integral` moved one away from zero, or toward
/// the side `mode` names, where `value` passes its edge.
// php compares exactly, not within a margin.
#[allow(clippy::float_cmp)]
fn round_helper(integral: f64, value: f64, exponent: f64, places: i32, mode: i32) -> f64 {
    let away = integral + 1f64.copysign(integral);
    let value_abs = value.abs();
    match mode {
        PHP_ROUND_HALF_DOWN => {
            if value_abs > basic_edge(integral, exponent, places) {
                away
            } else {
                integral
            }
        }
        PHP_ROUND_CEILING => {
            if value > 0.0 && value_abs > zero_edge(integral, exponent, places) {
                integral + 1.0
            } else {
                integral
            }
        }
        PHP_ROUND_FLOOR => {
            if value < 0.0 && value_abs > zero_edge(integral, exponent, places) {
                integral - 1.0
            } else {
                integral
            }
        }
        PHP_ROUND_TOWARD_ZERO => integral,
        PHP_ROUND_AWAY_FROM_ZERO => {
            if value_abs > zero_edge(integral, exponent, places) {
                away
            } else {
                integral
            }
        }
        PHP_ROUND_HALF_EVEN | PHP_ROUND_HALF_ODD => {
            let edge = basic_edge(integral, exponent, places);
            if value_abs > edge {
                away
            } else if value_abs == edge {
                let even = integral % 2.0 == 0.0;
                if even == (mode == PHP_ROUND_HALF_ODD) {
                    away
                } else {
                    integral
                }
            } else {
                integral
            }
        }
        _ => {
            if value_abs >= basic_edge(integral, exponent, places) {
                away
            } else {
                integral
            }
        }
    }
}

/// `_php_math_round` (`Z/ext/standard/math.c:161`): `value` to `places`
/// decimal places in `mode`, the integral part corrected where the
/// scaling lost the last unit, a value past sixteen digits handed back
/// as it is, and the scaled value divided back, or for more than 22
/// places printed and read back with `zend_strtod`. What `range` sizes a
/// float range with, rounding the count of steps half up, so a count
/// that division leaves a hair under or over a whole number is that
/// number.
///
/// ```php
/// var_dump(count(range(0, 1, 0.1)), count(range(0.1, 0.5, 0.1)), count(range(0, 0.3, 0.1)), range(0, 1, 0.3));
/// ```
/// ```expect
/// int(11)
/// int(5)
/// int(3)
/// array(4) {
///   [0]=>
///   float(0)
///   [1]=>
///   float(0.3)
///   [2]=>
///   float(0.6)
///   [3]=>
///   float(0.8999999999999999)
/// }
/// ```
// php compares exactly, not within a margin.
#[allow(clippy::float_cmp)]
pub(crate) fn math_round(value: f64, places: i32, mode: i32) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return value;
    }
    let places = places.max(i32::MIN + 1);
    let exponent = intpow10(places.abs());
    let scaled = if places > 0 {
        value * exponent
    } else {
        value / exponent
    };
    let (mut tmp, tmp2) = if value >= 0.0 {
        let tmp = scaled.floor();
        (tmp, tmp + 1.0)
    } else {
        let tmp = scaled.ceil();
        (tmp, tmp - 1.0)
    };
    let back = if places > 0 {
        tmp2 / exponent
    } else {
        tmp2 * exponent
    };
    if back == value {
        tmp = tmp2;
    }
    if tmp.abs() >= 1e16 {
        return value;
    }
    tmp = round_helper(tmp, value, exponent, places, mode);
    if places.abs() < 23 {
        if places > 0 {
            tmp / exponent
        } else {
            tmp * exponent
        }
    } else {
        // `snprintf(buf, 39, "%15fe%d", tmp_value, -places)` read back.
        let text = format!("{tmp:15.6}e{}", -i64::from(places));
        match text.trim().parse::<f64>() {
            Ok(read) if read.is_finite() => read,
            _ => value,
        }
    }
}
