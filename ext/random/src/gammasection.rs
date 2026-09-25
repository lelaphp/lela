//! `gammasection.c`: Goualard's γ-section, a float drawn uniformly from
//! the representable floats of an interval (Drawing Random Floating-Point
//! Numbers from an Interval, ACM TOMACS 32:3, 2022), one function per
//! pair of bounds as `Random\IntervalBoundary` names them. `NaN` is php's
//! answer for an interval with no float in it.

use crate::{Algo, Failure, range64};

/// `x - nextafter(x, -DBL_MAX)`, which is zero at `-DBL_MAX` itself.
#[allow(clippy::float_cmp)]
fn gamma_low(x: f64) -> f64 {
    if x == -f64::MAX {
        0.0
    } else {
        x - x.next_down()
    }
}

/// `nextafter(x, DBL_MAX) - x`, which is zero at `DBL_MAX` itself.
#[allow(clippy::float_cmp)]
fn gamma_high(x: f64) -> f64 {
    if x == f64::MAX { 0.0 } else { x.next_up() - x }
}

/// The step: the larger gap at the bound further from zero.
fn gamma_max(x: f64, y: f64) -> f64 {
    if x.abs() > y.abs() {
        gamma_high(x)
    } else {
        gamma_low(y)
    }
}

/// `splitint64`: `v` as `4 * hi + lo`, each exact in a double.
#[allow(clippy::cast_precision_loss)]
fn split(v: u64) -> (f64, f64) {
    ((v >> 2) as f64, (v & 3) as f64)
}

/// `ceilint`: the number of steps of `g` between `a` and `b`, rounded up
/// with the error term php carries.
// php's names, `a`, `b`, `g`, `s` and `e`, kept so the two read alike.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names
)]
fn ceilint(a: f64, b: f64, g: f64) -> u64 {
    let s = b / g - a / g;
    let e = if a.abs() <= b.abs() {
        -a / g - (s - b / g)
    } else {
        b / g - (s + a / g)
    };
    let si = s.ceil();
    if s == si {
        (si as u64) + u64::from(e > 0.0)
    } else {
        si as u64
    }
}

/// `max - k * g` for `k` split as [`split`] splits it.
fn down_from(max: f64, k: u64, g: f64) -> f64 {
    let (hi, lo) = split(k);
    4.0 * (max * 0.25 - hi * g) - lo * g
}

/// `min + k * g` for `k` split as [`split`] splits it.
fn up_from(min: f64, k: u64, g: f64) -> f64 {
    let (hi, lo) = split(k);
    4.0 * (min * 0.25 + hi * g) + lo * g
}

/// `php_random_gammasection_closed_open`: `[min, max)`.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\Mt19937(1));
/// var_dump($r->getFloat(0.0, 1.0), $r->getFloat(-10.5, 3.25), $r->getFloat(1e300, 1.7e308));
/// ```
/// ```expect
/// float(0.7655127444162375)
/// float(-4.960382687954031)
/// float(8.873517653218599E+307)
/// ```
pub(crate) fn closed_open(algo: &mut dyn Algo, min: f64, max: f64) -> Result<f64, Failure> {
    let g = gamma_max(min, max);
    let hi = ceilint(min, max, g);
    if max <= min || hi < 1 {
        return Ok(f64::NAN);
    }
    let k = 1 + range64(algo, hi - 1)?;
    Ok(if min.abs() <= max.abs() {
        if k == hi { min } else { down_from(max, k, g) }
    } else {
        up_from(min, k - 1, g)
    })
}

/// `php_random_gammasection_closed_closed`: `[min, max]`.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\Mt19937(1));
/// var_dump($r->getFloat(0.0, 1.0, Random\IntervalBoundary::ClosedClosed), $r->getFloat(2.0, 2.0, Random\IntervalBoundary::ClosedClosed));
/// ```
/// ```expect
/// float(0.7655127444164643)
/// float(2)
/// ```
pub(crate) fn closed_closed(algo: &mut dyn Algo, min: f64, max: f64) -> Result<f64, Failure> {
    let g = gamma_max(min, max);
    let hi = ceilint(min, max, g);
    if max < min {
        return Ok(f64::NAN);
    }
    let k = range64(algo, hi)?;
    Ok(if min.abs() <= max.abs() {
        if k == hi { min } else { down_from(max, k, g) }
    } else if k == hi {
        max
    } else {
        up_from(min, k, g)
    })
}

/// `php_random_gammasection_open_closed`: `(min, max]`.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\Mt19937(1));
/// var_dump($r->getFloat(0.0, 1.0, Random\IntervalBoundary::OpenClosed), $r->getFloat(-5.0, -1.0, Random\IntervalBoundary::OpenClosed));
/// ```
/// ```expect
/// float(0.7655127444162376)
/// float(-1.9801913439770145)
/// ```
pub(crate) fn open_closed(algo: &mut dyn Algo, min: f64, max: f64) -> Result<f64, Failure> {
    let g = gamma_max(min, max);
    let hi = ceilint(min, max, g);
    if max <= min || hi < 1 {
        return Ok(f64::NAN);
    }
    let k = range64(algo, hi - 1)?;
    Ok(if min.abs() <= max.abs() {
        down_from(max, k, g)
    } else if k == hi - 1 {
        max
    } else {
        up_from(min, k + 1, g)
    })
}

/// `php_random_gammasection_open_open`: `(min, max)`.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\Mt19937(1));
/// var_dump($r->getFloat(0.0, 1.0, Random\IntervalBoundary::OpenOpen), $r->getFloat(-5.0, -1.0, Random\IntervalBoundary::OpenOpen));
/// ```
/// ```expect
/// float(0.7655127444160108)
/// float(-1.9801913439736225)
/// ```
pub(crate) fn open_open(algo: &mut dyn Algo, min: f64, max: f64) -> Result<f64, Failure> {
    let g = gamma_max(min, max);
    let hi = ceilint(min, max, g);
    if max <= min || hi < 2 {
        return Ok(f64::NAN);
    }
    let k = 1 + range64(algo, hi - 2)?;
    Ok(if min.abs() <= max.abs() {
        down_from(max, k, g)
    } else {
        up_from(min, k, g)
    })
}
