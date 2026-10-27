//! The range arithmetic of `zend_inference.c`: what an integer operation
//! yields from the ranges of its operands, with php's overflow handling
//! and its bounds of `|` and `&` from Hacker's Delight, copied where the
//! outcome is observable in the SSA dump's `RANGE[..]`.

use php_unit::Opcode;

use crate::types::Range;

/// `zend_add_will_overflow`.
pub(crate) fn add_overflows(a: i64, b: i64) -> bool {
    (b > 0 && a > i64::MAX - b) || (b < 0 && a < i64::MIN - b)
}

/// `zend_sub_will_overflow`.
fn sub_overflows(a: i64, b: i64) -> bool {
    (b > 0 && a < i64::MIN + b) || (b < 0 && a > i64::MAX + b)
}

/// `minOR`, over the bits as php's `zend_ulong`.
#[allow(
    clippy::many_single_char_names,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn min_or(a: i64, b: i64, c: i64, d: i64) -> i64 {
    let (mut a, b, mut c, d) = (a as u64, b as u64, c as u64, d as u64);
    let mut m: u64 = 1 << 63;
    while m != 0 {
        if !a & c & m != 0 {
            let temp = (a | m) & m.wrapping_neg();
            if temp <= b {
                a = temp;
                break;
            }
        } else if a & !c & m != 0 {
            let temp = (c | m) & m.wrapping_neg();
            if temp <= d {
                c = temp;
                break;
            }
        }
        m >>= 1;
    }
    (a | c) as i64
}

/// `maxOR`.
#[allow(
    clippy::many_single_char_names,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn max_or(a: i64, b: i64, c: i64, d: i64) -> i64 {
    let (a, mut b, c, mut d) = (a as u64, b as u64, c as u64, d as u64);
    let mut m: u64 = 1 << 63;
    while m != 0 {
        if b & d & m != 0 {
            let temp = (b.wrapping_sub(m)) | (m - 1);
            if temp >= a {
                b = temp;
                break;
            }
            let temp = (d.wrapping_sub(m)) | (m - 1);
            if temp >= c {
                d = temp;
                break;
            }
        }
        m >>= 1;
    }
    (b | d) as i64
}

/// `minAND`.
#[allow(
    clippy::many_single_char_names,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn min_and(a: i64, b: i64, c: i64, d: i64) -> i64 {
    let (mut a, b, mut c, d) = (a as u64, b as u64, c as u64, d as u64);
    let mut m: u64 = 1 << 63;
    while m != 0 {
        if !a & !c & m != 0 {
            let temp = (a | m) & m.wrapping_neg();
            if temp <= b {
                a = temp;
                break;
            }
            let temp = (c | m) & m.wrapping_neg();
            if temp <= d {
                c = temp;
                break;
            }
        }
        m >>= 1;
    }
    (a & c) as i64
}

/// `maxAND`.
#[allow(
    clippy::many_single_char_names,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn max_and(a: i64, b: i64, c: i64, d: i64) -> i64 {
    let (a, mut b, c, mut d) = (a as u64, b as u64, c as u64, d as u64);
    let mut m: u64 = 1 << 63;
    while m != 0 {
        if b & !d & m != 0 {
            let temp = (b | !m) | (m - 1);
            if temp >= a {
                b = temp;
                break;
            }
        } else if !b & d & m != 0 {
            let temp = (d | !m) | (m - 1);
            if temp >= c {
                d = temp;
                break;
            }
        }
        m >>= 1;
    }
    (b & d) as i64
}

/// The sign pattern `zend_ssa_range_or` and `zend_ssa_range_and` switch
/// on: a bit per bound that is negative.
fn signs(a: i64, b: i64, c: i64, d: i64) -> u8 {
    (u8::from(a < 0) << 3) | (u8::from(b < 0) << 2) | (u8::from(c < 0) << 1) | u8::from(d < 0)
}

/// `zend_ssa_range_or`: the bounds of `[a..b] | [c..d]`, written into
/// `r` where the pattern is one php handles.
#[allow(clippy::many_single_char_names)]
fn range_or(a: i64, b: i64, c: i64, d: i64, r: &mut Range) {
    match signs(a, b, c, d) {
        0x0 | 0x3 | 0xc | 0xf => {
            r.min = min_or(a, b, c, d);
            r.max = max_or(a, b, c, d);
        }
        0x2 => {
            r.min = min_or(a, b, c, -1);
            r.max = max_or(a, b, 0, d);
        }
        0x8 => {
            r.min = min_or(a, -1, c, d);
            r.max = max_or(0, b, c, d);
        }
        0xa => {
            r.min = a.min(c);
            r.max = max_or(0, b, 0, d);
        }
        0xb => {
            r.min = c;
            r.max = -1;
        }
        0xe => {
            r.min = a;
            r.max = -1;
        }
        _ => {}
    }
}

/// `zend_ssa_range_and`.
#[allow(clippy::many_single_char_names)]
fn range_and(a: i64, b: i64, c: i64, d: i64, r: &mut Range) {
    match signs(a, b, c, d) {
        0x0 | 0x3 | 0xc | 0xf => {
            r.min = min_and(a, b, c, d);
            r.max = max_and(a, b, c, d);
        }
        0x2 => {
            r.min = 0;
            r.max = b;
        }
        0x8 => {
            r.min = 0;
            r.max = d;
        }
        0xa => {
            r.min = min_and(a, -1, c, -1);
            r.max = b.max(d);
        }
        0xb => {
            r.min = min_and(a, -1, c, d);
            r.max = max_and(0, b, c, d);
        }
        0xe => {
            r.min = min_and(a, b, c, -1);
            r.max = max_and(a, b, 0, d);
        }
        _ => {}
    }
}

/// `zend_abs_range`: none for a range from `ZEND_LONG_MIN`.
fn abs_range(min: i64, max: i64) -> Option<(i64, i64)> {
    if min == i64::MIN {
        return None;
    }
    Some(if min >= 0 {
        (min, max)
    } else if max <= 0 {
        (-max, -min)
    } else {
        (0, max.max(-min))
    })
}

/// `safe_shift_left`: the shift on the unsigned bits.
#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation
)]
fn shift_left(n: i64, s: i64) -> i64 {
    (n as u64).wrapping_shl(s as u32) as i64
}

/// `shift_left_overflows`, a shift into the sign bit among them.
fn shift_left_overflows(n: i64, s: i64) -> bool {
    if n >= 0 {
        s >= 63 || shift_left(n, s) < n
    } else {
        s >= 64 || shift_left(n, s) > n
    }
}

/// `float_div`: `a / b` truncated, and the integer past it where the
/// division is inexact.
fn float_div(a: i64, b: i64) -> (i64, i64) {
    let q = a.wrapping_div(b);
    let r2 = if a.wrapping_rem(b) != 0 {
        if q < 0 { q - 1 } else { q + 1 }
    } else {
        q
    };
    (q, r2)
}

/// An operand's range as `OP1_HAS_RANGE` and its kin read it: a long
/// constant's own, a variable's where it has one.
pub(crate) type Operand = Option<Range>;

fn min4(a: i64, b: i64, c: i64, d: i64) -> i64 {
    a.min(b).min(c.min(d))
}

fn max4(a: i64, b: i64, c: i64, d: i64) -> i64 {
    a.max(b).max(c.max(d))
}

/// `zend_inference_calc_binary_op_range`: the range of `op1 <opcode>
/// op2` written into `r`, whose bound flags the caller cleared; `false`
/// where php knows none.
#[allow(clippy::too_many_lines)]
pub(crate) fn binary(opcode: Opcode, op1: Operand, op2: Operand, r: &mut Range) -> bool {
    let (Some(a), Some(b)) = (op1, op2) else {
        return false;
    };
    let wide = a.underflow || b.underflow || a.overflow || b.overflow;
    match opcode {
        Opcode::Add => {
            if a.underflow || b.underflow || add_overflows(a.min, b.min) {
                r.underflow = true;
                r.min = i64::MIN;
            } else {
                r.min = a.min + b.min;
            }
            if a.overflow || b.overflow || add_overflows(a.max, b.max) {
                r.overflow = true;
                r.max = i64::MAX;
            } else {
                r.max = a.max + b.max;
            }
        }
        Opcode::Sub => {
            if a.underflow || b.overflow || sub_overflows(a.min, b.max) {
                r.underflow = true;
                r.min = i64::MIN;
            } else {
                r.min = a.min - b.max;
            }
            if a.overflow || b.underflow || sub_overflows(a.max, b.min) {
                r.overflow = true;
                r.max = i64::MAX;
            } else {
                r.max = a.max - b.min;
            }
        }
        Opcode::Mul => {
            let products = [
                a.min.checked_mul(b.min),
                a.min.checked_mul(b.max),
                a.max.checked_mul(b.min),
                a.max.checked_mul(b.max),
            ];
            match products {
                [Some(t1), Some(t2), Some(t3), Some(t4)] if !wide => {
                    r.min = min4(t1, t2, t3, t4);
                    r.max = max4(t1, t2, t3, t4);
                }
                _ => *r = Range::ANY,
            }
        }
        Opcode::Div => {
            if wide || (b.min <= 0 && b.max >= 0) || (a.min == i64::MIN && b.max == -1) {
                *r = Range::ANY;
            } else {
                let (t1, t1_) = float_div(a.min, b.min);
                let (t2, t2_) = float_div(a.min, b.max);
                let (t3, t3_) = float_div(a.max, b.min);
                let (t4, t4_) = float_div(a.max, b.max);
                r.min = min4(t1, t2, t3, t4).min(min4(t1_, t2_, t3_, t4_));
                r.max = max4(t1, t2, t3, t4).max(max4(t1_, t2_, t3_, t4_));
            }
        }
        Opcode::Mod => {
            if wide {
                r.min = i64::MIN;
                r.max = i64::MAX;
            } else {
                let Some((mut abs_min, abs_max)) = abs_range(b.min, b.max) else {
                    return false;
                };
                if abs_max == 0 {
                    return false;
                }
                if abs_min == 0 {
                    abs_min += 1;
                }
                if a.min >= 0 {
                    r.min = if a.max < abs_min { a.min } else { 0 };
                    r.max = a.max.min(abs_max - 1);
                } else if a.max <= 0 {
                    r.min = a.min.max(-abs_max + 1);
                    r.max = if a.min > -abs_min { a.max } else { 0 };
                } else {
                    r.min = a.min.max(-abs_max + 1);
                    r.max = a.max.min(abs_max - 1);
                }
            }
        }
        Opcode::Sl => {
            if wide {
                r.min = i64::MIN;
                r.max = i64::MAX;
            } else {
                let (s_min, s_max) = (b.min.max(0), b.max.max(0));
                if shift_left_overflows(a.min, s_max) || shift_left_overflows(a.max, s_max) {
                    r.min = i64::MIN;
                    r.max = i64::MAX;
                } else {
                    let t1 = shift_left(a.min, s_min);
                    let t2 = shift_left(a.min, s_max);
                    let t3 = shift_left(a.max, s_min);
                    let t4 = shift_left(a.max, s_max);
                    r.min = min4(t1, t2, t3, t4);
                    r.max = max4(t1, t2, t3, t4);
                }
            }
        }
        Opcode::Sr => {
            if wide {
                r.min = i64::MIN;
                r.max = i64::MAX;
            } else {
                let (s_min, s_max) = (b.min.clamp(0, 63), b.max.clamp(0, 63));
                let t1 = a.min >> s_min;
                let t2 = a.min >> s_max;
                let t3 = a.max >> s_min;
                let t4 = a.max >> s_max;
                r.min = min4(t1, t2, t3, t4);
                r.max = max4(t1, t2, t3, t4);
            }
        }
        Opcode::BwOr => {
            if wide {
                r.min = i64::MIN;
                r.max = i64::MAX;
            } else {
                range_or(a.min, a.max, b.min, b.max, r);
            }
        }
        Opcode::BwAnd => {
            if wide {
                r.min = i64::MIN;
                r.max = i64::MAX;
            } else {
                range_and(a.min, a.max, b.min, b.max, r);
            }
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{Range, binary};
    use php_unit::Opcode;

    #[allow(clippy::unnecessary_wraps)]
    fn of(min: i64, max: i64) -> Option<Range> {
        Some(Range {
            min,
            max,
            underflow: false,
            overflow: false,
        })
    }

    fn calc(opcode: Opcode, a: Option<Range>, b: Option<Range>) -> Option<Range> {
        let mut r = Range::default();
        binary(opcode, a, b, &mut r).then_some(r)
    }

    #[test]
    fn arithmetic_bounds() {
        assert_eq!(
            calc(Opcode::Sub, of(2, i64::MAX), of(1, 1)),
            of(1, i64::MAX - 1)
        );
        let wide = calc(Opcode::Add, of(0, i64::MAX), of(1, 1)).unwrap_or_default();
        assert!(wide.overflow && !wide.underflow);
        assert_eq!(calc(Opcode::Mod, of(0, 100), of(7, 7)), of(0, 6));
        assert_eq!(calc(Opcode::Div, of(7, 7), of(2, 2)), of(3, 4));
        assert_eq!(calc(Opcode::BwAnd, of(0, 100), of(15, 15)), of(0, 15));
        assert_eq!(calc(Opcode::BwOr, of(0, 3), of(4, 4)), of(4, 7));
        assert_eq!(calc(Opcode::Sl, of(1, 1), of(3, 3)), of(8, 8));
        assert_eq!(calc(Opcode::BwXor, of(1, 1), of(3, 3)), None);
    }
}
