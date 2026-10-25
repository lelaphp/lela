//! `zend_dtoa` (`Z/Zend/zend_strtod.c:3703`), David Gay's correctly
//! rounded double-to-decimal conversion as php builds it: IEEE doubles
//! with `IEEE_8087`, 32-bit words multiplied through 64-bit products,
//! neither `ROUND_BIASED` nor `Honor_FLT_ROUNDS` nor `Sudden_Underflow`
//! defined. The two kinds of digits `zend_gcvt` asks for are ported,
//! the shortest and a count of significant ones, with each of their
//! paths: the quick estimate in doubles for at most
//! [`QUICK_MAX`] digits, the exact division of a small integer, and the
//! big-integer quotients otherwise, with Steele and White's stopping rule
//! for the shortest digits. The digits are written once, into the
//! caller's [`DoubleText`], with no text formatted and read back.
//!
//! php keeps its big integers on a free list every thread shares; here each is
//! a fixed array on the stack, [`WORDS`] words, which no double's
//! conversion outgrows (the bound is at the constant).

// Gay's code moves between `int`, `Long`, `ULong` and `ULLong` as its
// arithmetic wants, and the casts here are those conversions: a 64-bit
// product's low word kept by truncation, a quotient digit under ten, an
// exponent count that the IEEE format bounds.
#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "zend_dtoa's own conversions between its word, long and int types"
)]

use super::DoubleText;

/// Which digits `zend_dtoa` is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Digits {
    /// `zend_dtoa(d, 0, ...)`: the shortest digits that read back as the
    /// same double, the closest of them to it.
    Shortest,
    /// `zend_dtoa(d, 2, ndigits, ...)`: this many significant digits,
    /// correctly rounded, a count under one read as one, trailing zeros
    /// suppressed but on the exact division of a small integer, which
    /// stops at the count with its zeros, as `1005` at three digits is
    /// `100` and prints `1.00E+3`.
    Significant(i32),
}

impl Digits {
    /// What `zend_gcvt` asks for at `ndigit`: the shortest for a negative
    /// one, `ndigit` significant digits otherwise.
    pub(super) fn at(ndigit: i32) -> Self {
        if ndigit < 0 {
            Self::Shortest
        } else {
            Self::Significant(ndigit)
        }
    }
}

/// `P`, the significand's bits.
const P: i32 = 53;
/// `Bias`, the exponent's.
const BIAS: i32 = 1023;
/// `Exp_msk1`, the exponent's lowest bit in the high word.
const EXP_MSK1: u32 = 0x10_0000;
/// `Exp_mask`, the exponent's bits in the high word.
const EXP_MASK: u32 = 0x7ff0_0000;
/// `Exp_11`, the high word of `1.0`'s exponent.
const EXP_11: u32 = 0x3ff0_0000;
/// `Frac_mask`, `Frac_mask1` and `Bndry_mask`: the significand's bits in
/// the high word.
const FRAC_MASK: u32 = 0xf_ffff;
/// `Ten_pmax`, the largest power of ten a double holds exactly.
const TEN_PMAX: i32 = 22;
/// `Bletch`, the bit of `k >> 4` that needs `1e256` at once.
const BLETCH: i32 = 0x10;
/// `Quick_max`, the most digits the estimate in doubles is tried for.
const QUICK_MAX: i32 = 14;
/// `Int_max`, the largest power of ten an integer is divided by exactly.
const INT_MAX: i32 = 14;
/// `Log2P`.
const LOG2P: i32 = 1;

/// `tens[]`.
const TENS: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];
/// `bigtens[]`.
const BIGTENS: [f64; 5] = [1e16, 1e32, 1e64, 1e128, 1e256];

/// The words a big integer holds. The widest any conversion lays out
/// is `mult`'s product for the largest subnormal at the shortest digits,
/// laid at the sum of its factors' words before it is trimmed: 25 words,
/// which `the_digits_are_the_correctly_rounded_expansion` below reaches
/// over every power of two and the extremes (24 words fail it). 40 is
/// half again.
const WORDS: usize = 40;

/// Gay's `Bigint` with its `wds` words in use, least significant first.
#[derive(Clone)]
struct Big {
    x: [u32; WORDS],
    wds: usize,
}

impl Big {
    /// `i2b`.
    fn small(i: u32) -> Self {
        let mut x = [0; WORDS];
        x[0] = i;
        Big { x, wds: 1 }
    }

    /// `!b->x[0] && b->wds <= 1`.
    fn is_zero(&self) -> bool {
        self.x[0] == 0 && self.wds <= 1
    }

    /// `multadd(b, m, a)`: multiply by `m` and add `a`, in place.
    fn multadd(&mut self, m: u32, a: u32) {
        let mut carry = u64::from(a);
        for word in &mut self.x[..self.wds] {
            let y = u64::from(*word) * u64::from(m) + carry;
            carry = y >> 32;
            *word = y as u32;
        }
        if carry != 0 {
            self.x[self.wds] = carry as u32;
            self.wds += 1;
        }
    }

    /// `lshift(b, k)`, in place.
    fn lshift(&mut self, k: i32) {
        let n = (k >> 5) as usize;
        let bits = k & 0x1f;
        let wds = self.wds;
        if bits == 0 {
            self.x.copy_within(0..wds, n);
            self.wds = wds + n;
        } else {
            let back = 32 - bits;
            let top = self.x[wds - 1] >> back;
            self.wds = wds + n;
            if top != 0 {
                self.x[wds + n] = top;
                self.wds += 1;
            }
            for i in (1..wds).rev() {
                self.x[i + n] = self.x[i] << bits | self.x[i - 1] >> back;
            }
            self.x[n] = self.x[0] << bits;
        }
        self.x[..n].fill(0);
    }

    /// `quorem(b, S)`: the digit `b / S`, under ten by the scaling, with
    /// `b` left as the remainder.
    fn quorem(&mut self, s: &Big) -> u32 {
        let n = s.wds;
        if self.wds < n {
            return 0;
        }
        let top = n - 1;
        let mut q = self.x[top] / (s.x[top] + 1);
        if q != 0 {
            self.subtract_times(s, q);
        }
        if cmp(self, s) >= 0 {
            q += 1;
            self.subtract_times(s, 1);
        }
        q
    }

    /// `b -= q * S` over `S`'s words, and `b`'s count trimmed as
    /// `quorem` trims it.
    fn subtract_times(&mut self, s: &Big, q: u32) {
        let n = s.wds;
        let mut borrow = 0_u64;
        let mut carry = 0_u64;
        for (word, &si) in self.x[..n].iter_mut().zip(&s.x[..n]) {
            let ys = u64::from(si) * u64::from(q) + carry;
            carry = ys >> 32;
            let y = u64::from(*word)
                .wrapping_sub(ys & 0xffff_ffff)
                .wrapping_sub(borrow);
            borrow = y >> 32 & 1;
            *word = y as u32;
        }
        let mut top = n - 1;
        if self.x[top] == 0 {
            while top > 0 && self.x[top] == 0 {
                top -= 1;
            }
            self.wds = top + 1;
        }
    }
}

/// `cmp(a, b)`: the sign of `a - b`.
fn cmp(a: &Big, b: &Big) -> i32 {
    if a.wds != b.wds {
        return if a.wds > b.wds { 1 } else { -1 };
    }
    for i in (0..a.wds).rev() {
        if a.x[i] != b.x[i] {
            return if a.x[i] < b.x[i] { -1 } else { 1 };
        }
    }
    0
}

/// `mult(a, b)`.
fn mult(a: &Big, b: &Big) -> Big {
    let (a, b) = if a.wds < b.wds { (b, a) } else { (a, b) };
    let mut c = Big {
        x: [0; WORDS],
        wds: 0,
    };
    for (j, &y) in b.x[..b.wds].iter().enumerate() {
        if y == 0 {
            continue;
        }
        let mut carry = 0_u64;
        for (i, &xa) in a.x[..a.wds].iter().enumerate() {
            let z = u64::from(xa) * u64::from(y) + u64::from(c.x[i + j]) + carry;
            carry = z >> 32;
            c.x[i + j] = z as u32;
        }
        c.x[j + a.wds] = carry as u32;
    }
    let mut wc = a.wds + b.wds;
    while wc > 0 && c.x[wc - 1] == 0 {
        wc -= 1;
    }
    c.wds = wc;
    c
}

/// `pow5mult(b, k)`: `b * 5^k`. php keeps the powers `5^(4 * 2^n)` it
/// squares on a list for the next call; here each call squares its own.
fn pow5mult(mut b: Big, mut k: i32) -> Big {
    const P05: [u32; 3] = [5, 25, 125];
    let low = (k & 3) as usize;
    if low != 0 {
        b.multadd(P05[low - 1], 0);
    }
    k >>= 2;
    if k == 0 {
        return b;
    }
    let mut p5 = Big::small(625);
    loop {
        if k & 1 != 0 {
            b = mult(&b, &p5);
        }
        k >>= 1;
        if k == 0 {
            return b;
        }
        p5 = mult(&p5, &p5);
    }
}

/// `diff(a, b)`: `|a - b|`, and whether `a < b`, which is `sign`.
fn diff(a: &Big, b: &Big) -> (Big, bool) {
    let order = cmp(a, b);
    if order == 0 {
        return (Big::small(0), false);
    }
    let (a, b, negative) = if order < 0 {
        (b, a, true)
    } else {
        (a, b, false)
    };
    let mut c = Big {
        x: [0; WORDS],
        wds: a.wds,
    };
    let mut borrow = 0_u64;
    for i in 0..a.wds {
        let sub = if i < b.wds { u64::from(b.x[i]) } else { 0 };
        let y = u64::from(a.x[i]).wrapping_sub(sub).wrapping_sub(borrow);
        borrow = y >> 32 & 1;
        c.x[i] = y as u32;
    }
    while c.wds > 1 && c.x[c.wds - 1] == 0 {
        c.wds -= 1;
    }
    (c, negative)
}

/// `d2b(d, &e, &bits)`: the double's significand as a big integer with
/// its trailing zero bits shifted out, its binary exponent and its
/// significant bits, for `d` finite, positive and not zero.
fn d2b(word0: u32, word1: u32) -> (Big, i32, i32) {
    let mut b = Big::small(0);
    let mut z = word0 & FRAC_MASK;
    let de = ((word0 & 0x7fff_ffff) >> 20) as i32;
    if de != 0 {
        z |= EXP_MSK1;
    }
    let (k, i) = if word1 == 0 {
        let k = z.trailing_zeros();
        b.x[0] = z >> k;
        b.wds = 1;
        (k as i32 + 32, 1)
    } else {
        let k = word1.trailing_zeros();
        if k == 0 {
            b.x[0] = word1;
        } else {
            b.x[0] = word1 >> k | z << (32 - k);
            z >>= k;
        }
        b.x[1] = z;
        b.wds = if z == 0 { 1 } else { 2 };
        (k as i32, b.wds)
    };
    if de == 0 {
        let bits = 32 * i as i32 - b.x[i - 1].leading_zeros() as i32;
        (b, de - BIAS - (P - 1) + 1 + k, bits)
    } else {
        (b, de - BIAS - (P - 1) + k, P - k)
    }
}

/// `dshift(S, p2)`: the shift that leaves `S`'s top word with four
/// leading zero bits once `p2` more are shifted in.
fn dshift(s: &Big, p2: i32) -> i32 {
    let mut rv = s.x[s.wds - 1].leading_zeros() as i32 - 4;
    if p2 > 0 {
        rv -= p2;
    }
    rv & 31
}

/// The `bump_up` and `roundoff` labels: add one to the last digit
/// written past `s0`, a string of nines carrying into the digit before it,
/// and all nines becoming a one with the point moved, `k++`.
fn round_up(s: &mut DoubleText, s0: usize, k: &mut i32) {
    loop {
        let first = s.end() == s0 + 1;
        let Some(digit) = s.last() else {
            return;
        };
        if digit != b'9' {
            s.set_last(digit + 1);
            return;
        }
        if first {
            *k += 1;
            s.set_last(b'1');
            return;
        }
        s.pop();
    }
}

/// The `round_9_up` label: a nine that rounds up is written and carried
/// into the digits before it; the answer is the point it leaves.
fn round_nine_up(s: &mut DoubleText, s0: usize, mut k: i32) -> i32 {
    s.push(b'9');
    round_up(s, s0, &mut k);
    k + 1
}

/// The trailing zeros `zend_dtoa` suppresses, past `s0`.
fn trim_zeros(s: &mut DoubleText, s0: usize) {
    while s.end() > s0 + 1 && s.last() == Some(b'0') {
        s.pop();
    }
}

/// `zend_dtoa(d, ..., &decpt, ...)` for `d` finite and not
/// negative: appends its digits to `s` and answers `decpt`, where the
/// decimal point stands among them. Zero is `"0"` with the point after
/// it, as php answers it.
#[expect(
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::approx_constant,
    clippy::float_cmp,
    reason = "zend_dtoa as php writes it: its names, its one body, its constants to the digits it \
              spells them with, and its exact comparisons of doubles it knows to be exact"
)]
pub(super) fn dtoa(d: f64, asked: Digits, s: &mut DoubleText) -> i32 {
    if d == 0.0 {
        s.push(b'0');
        return 1;
    }
    let s0 = s.end();
    let bits = d.to_bits();
    let word0 = (bits >> 32) as u32;
    let word1 = bits as u32;
    let (mut b, be, bbits) = d2b(word0, word1);

    // `k`, the estimate of `floor(log10(d))`, from the exponent and the
    // first-order series of the significand's logarithm.
    let exponent = ((word0 >> 20) & 0x7ff) as i32;
    let (d2, i, denorm) = if exponent == 0 {
        let i = bbits + be + (BIAS + (P - 1) - 1);
        let x = if i > 32 {
            word0 << (64 - i) | word1 >> (i - 32)
        } else {
            word1 << (32 - i)
        };
        let d2 = f64::from_bits(f64::from(x).to_bits() - (u64::from(31 * EXP_MSK1) << 32));
        (d2, i - (BIAS + (P - 1) - 1) - 1, true)
    } else {
        let d2 = f64::from_bits(u64::from((word0 & FRAC_MASK) | EXP_11) << 32 | u64::from(word1));
        (d2, exponent - BIAS, false)
    };
    let ds = (d2 - 1.5) * 0.289_529_654_602_168
        + 0.176_091_259_055_8
        + f64::from(i) * 0.301_029_995_663_981;
    let mut k = ds as i32;
    if ds < 0.0 && ds != f64::from(k) {
        k -= 1;
    }
    let mut k_check = true;
    if (0..=TEN_PMAX).contains(&k) {
        if d < TENS[k as usize] {
            k -= 1;
        }
        k_check = false;
    }
    let j = bbits - i - 1;
    let (mut b2, mut s2) = if j >= 0 { (0, j) } else { (-j, 0) };
    let (b5, s5) = if k >= 0 {
        s2 += k;
        (0, k)
    } else {
        b2 -= k;
        (-k, 0)
    };
    let (leftright, mut ilim, ilim1) = match asked {
        Digits::Shortest => (true, -1, -1),
        Digits::Significant(ndigits) => {
            let n = ndigits.max(1);
            (false, n, n)
        }
    };

    if (0..=QUICK_MAX).contains(&ilim) {
        // Try to get by with floating-point arithmetic.
        let mut u = d;
        let mut ieps = 2;
        let mut i = 0;
        if k > 0 {
            let mut ds = TENS[(k & 0xf) as usize];
            let mut j = k >> 4;
            if j & BLETCH != 0 {
                // Prevent overflows.
                j &= BLETCH - 1;
                u /= BIGTENS[BIGTENS.len() - 1];
                ieps += 1;
            }
            while j != 0 {
                if j & 1 != 0 {
                    ieps += 1;
                    ds *= BIGTENS[i];
                }
                j >>= 1;
                i += 1;
            }
            u /= ds;
        } else if k < 0 {
            let j1 = -k;
            u *= TENS[(j1 & 0xf) as usize];
            let mut j = j1 >> 4;
            while j != 0 {
                if j & 1 != 0 {
                    ieps += 1;
                    u *= BIGTENS[i];
                }
                j >>= 1;
                i += 1;
            }
        }
        let mut quick_k = k;
        let mut quick_ilim = ilim;
        if k_check && u < 1.0 && quick_ilim > 0 {
            quick_ilim = ilim1;
            quick_k -= 1;
            u *= 10.0;
            ieps += 1;
        }
        let mut eps = f64::from(ieps) * u + 7.0;
        eps = f64::from_bits(eps.to_bits() - (u64::from((P - 1) as u32 * EXP_MSK1) << 32));
        // Generate `ilim` digits, then fix them up.
        eps *= TENS[(quick_ilim - 1) as usize];
        let mut i = 1;
        loop {
            let l = u as i32;
            u -= f64::from(l);
            if u == 0.0 {
                quick_ilim = i;
            }
            s.push(b'0' + l as u8);
            if i == quick_ilim {
                if u > 0.5 + eps {
                    round_up(s, s0, &mut quick_k);
                    return quick_k + 1;
                } else if u < 0.5 - eps {
                    trim_zeros(s, s0);
                    return quick_k + 1;
                }
                break;
            }
            i += 1;
            u *= 10.0;
        }
        // fast_failed
        s.truncate(s0);
    }

    // Do we have a "small" integer?
    if be >= 0 && k <= INT_MAX {
        let ds = TENS[k as usize];
        let mut u = d;
        let mut i = 1;
        loop {
            let l = (u / ds) as i32;
            u -= f64::from(l) * ds;
            s.push(b'0' + l as u8);
            if u == 0.0 {
                break;
            }
            if i == ilim {
                u += u;
                if u > ds || (u == ds && l & 1 != 0) {
                    round_up(s, s0, &mut k);
                }
                break;
            }
            i += 1;
            u *= 10.0;
        }
        return k + 1;
    }

    let mut m2 = b2;
    let m5 = b5;
    let mut mhi = None;
    if leftright {
        let i = if denorm {
            be + (BIAS + (P - 1) - 1 + 1)
        } else {
            1 + P - bbits
        };
        b2 += i;
        s2 += i;
        mhi = Some(Big::small(1));
    }
    if m2 > 0 && s2 > 0 {
        let i = m2.min(s2);
        b2 -= i;
        m2 -= i;
        s2 -= i;
    }
    if b5 > 0 {
        if let Some(low) = mhi.take() {
            let mut low = low;
            if m5 > 0 {
                low = pow5mult(low, m5);
                b = mult(&low, &b);
            }
            if b5 - m5 != 0 {
                b = pow5mult(b, b5 - m5);
            }
            mhi = Some(low);
        } else {
            b = pow5mult(b, b5);
        }
    }
    let mut big_s = Big::small(1);
    if s5 > 0 {
        big_s = pow5mult(big_s, s5);
    }

    // The special case: `d` a normalised power of two, whose lower
    // neighbour is half as far away as its upper one.
    let mut spec_case = false;
    if leftright && word1 == 0 && word0 & FRAC_MASK == 0 && word0 & (EXP_MASK & !EXP_MSK1) != 0 {
        b2 += LOG2P;
        s2 += LOG2P;
        spec_case = true;
    }

    // Shift left so the divisor has four leading zero bits.
    let i = dshift(&big_s, s2);
    b2 += i;
    m2 += i;
    s2 += i;
    if b2 > 0 {
        b.lshift(b2);
    }
    if s2 > 0 {
        big_s.lshift(s2);
    }
    if k_check && cmp(&b, &big_s) < 0 {
        // We botched the k estimate.
        k -= 1;
        b.multadd(10, 0);
        if let Some(low) = mhi.as_mut() {
            low.multadd(10, 0);
        }
        ilim = ilim1;
    }

    let dig;
    if let Some(mut mlo) = mhi {
        if m2 > 0 {
            mlo.lshift(m2);
        }
        // `mhi` is `mlo` but in the special case, where it is twice it.
        let mut high = spec_case.then(|| {
            let mut high = mlo.clone();
            high.lshift(LOG2P);
            high
        });
        let mut i = 1;
        loop {
            let mut digit = b.quorem(&big_s) as u8 + b'0';
            // Do we yet have the shortest decimal string that will round
            // to d?
            let j = cmp(&b, &mlo);
            let (delta, negative) = diff(&big_s, high.as_ref().unwrap_or(&mlo));
            let j1 = if negative { 1 } else { cmp(&b, &delta) };
            if j1 == 0 && word1 & 1 == 0 {
                if digit == b'9' {
                    return round_nine_up(s, s0, k);
                }
                if j > 0 {
                    digit += 1;
                }
                s.push(digit);
                return k + 1;
            }
            if j < 0 || (j == 0 && word1 & 1 == 0) {
                if !b.is_zero() && j1 > 0 {
                    b.lshift(1);
                    let j1 = cmp(&b, &big_s);
                    if j1 > 0 || (j1 == 0 && digit & 1 != 0) {
                        if digit == b'9' {
                            return round_nine_up(s, s0, k);
                        }
                        digit += 1;
                    }
                }
                s.push(digit);
                return k + 1;
            }
            if j1 > 0 {
                if digit == b'9' {
                    return round_nine_up(s, s0, k);
                }
                s.push(digit + 1);
                return k + 1;
            }
            s.push(digit);
            if i == ilim {
                dig = digit;
                break;
            }
            b.multadd(10, 0);
            mlo.multadd(10, 0);
            if let Some(high) = high.as_mut() {
                high.multadd(10, 0);
            }
            i += 1;
        }
    } else {
        let mut i = 1;
        loop {
            let digit = b.quorem(&big_s) as u8 + b'0';
            s.push(digit);
            if b.is_zero() {
                return k + 1;
            }
            if i >= ilim {
                dig = digit;
                break;
            }
            b.multadd(10, 0);
            i += 1;
        }
    }

    // Round off the last digit.
    b.lshift(1);
    let j = cmp(&b, &big_s);
    if j > 0 || (j == 0 && dig & 1 != 0) {
        round_up(s, s0, &mut k);
    } else {
        trim_zeros(s, s0);
    }
    k + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `zend_dtoa`'s digits against the expansion Rust's formatter
    /// rounds, the same correctly rounded digits by another algorithm:
    /// half to even on a tie, as `zend_dtoa` rounds with `ROUND_BIASED`
    /// undefined, and the shortest that reads back for a negative
    /// `ndigit`, where the two part only between two strings equally
    /// near the double. The doubles are the extremes, every power of two
    /// and ten, and 20,000 bit patterns from a fixed seed.
    #[test]
    fn the_digits_are_the_correctly_rounded_expansion() {
        fn expansion(value: f64, ndigit: i32) -> (String, i32) {
            let rendered = if ndigit < 0 {
                format!("{value:e}")
            } else {
                format!("{value:.*e}", usize::try_from(ndigit.max(1) - 1).unwrap())
            };
            let (mantissa, power) = rendered.split_once('e').unwrap();
            let mut digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
            while digits.len() > 1 && digits.ends_with('0') {
                digits.pop();
            }
            (digits, power.parse::<i32>().unwrap() + 1)
        }
        let mut values = vec![
            f64::MIN_POSITIVE,
            f64::MAX,
            5e-324,
            1e-323,
            2.225_073_858_507_201e-308,
            1e23,
            9_007_199_254_740_993.0,
            0.1,
            0.5,
            1.0 / 3.0,
            123_456_789_012_345.0,
            999_999_999_999_999.9,
        ];
        // Miri interprets the whole sweep in hours; it takes every
        // 211th power, five patterns and the shorter counts.
        let step = if cfg!(miri) { 211 } else { 1 };
        for e in (-1074..1024).step_by(step) {
            values.push(2_f64.powi(e));
        }
        for e in (-323..309).step_by(step) {
            values.push(format!("1e{e}").parse().unwrap());
        }
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..(if cfg!(miri) { 5 } else { 20_000 }) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let value = f64::from_bits(state >> 1);
            if value.is_finite() && value != 0.0 {
                values.push(value);
            }
        }
        for value in values {
            let counts: &[i32] = if cfg!(miri) {
                &[-1, 0, 2, 14, 17]
            } else {
                &[-1, 0, 1, 2, 3, 6, 14, 15, 16, 17, 18, 25, 40]
            };
            for &ndigit in counts {
                let mut out = DoubleText::at(0);
                let point = dtoa(value, Digits::at(ndigit), &mut out);
                let mut digits = out.as_str().to_owned();
                while digits.len() > 1 && digits.ends_with('0') {
                    digits.pop();
                }
                let (expected, at) = expansion(value, ndigit);
                if ndigit < 0 && digits != expected {
                    // Two shortest strings equally near the double, as
                    // `2^-25`'s `...312` and `...313` are: Rust's formatter
                    // takes the upper, `zend_dtoa` the even one.
                    let read = |d: &str| format!("0.{d}e{point}").parse::<f64>().unwrap();
                    assert_eq!((digits.len(), point), (expected.len(), at), "{value:e}");
                    assert_eq!(
                        (read(&digits), read(&expected)),
                        (value, value),
                        "{value:e}"
                    );
                    continue;
                }
                assert_eq!((digits, point), (expected, at), "{value:e} at {ndigit}");
            }
        }
        // The widest digit runs: a subnormal's whole expansion, and the
        // largest double's.
        let widest: &[f64] = if cfg!(miri) {
            &[5e-324]
        } else {
            &[5e-324, 2.225_073_858_507_201e-308, f64::MAX]
        };
        for &value in widest {
            let mut out = DoubleText::at(0);
            let point = dtoa(value, Digits::Significant(800), &mut out);
            let digits = out.as_str().to_owned();
            assert_eq!((digits, point), expansion(value, 800), "{value:e} at 800");
        }
    }
}
