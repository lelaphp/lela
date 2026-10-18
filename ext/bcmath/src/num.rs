//! `libbcmath`, php's copy of GNU bc's number package, in Rust: the one
//! arithmetic core the `bc*` natives and `BcMath\Number` share. A number
//! is laid out as php lays out a `bc_struct`, `n_len` digits before the
//! point and `n_scale` after, because the layout is observable: a
//! product's scale is chosen from its factors' scales, and a computed
//! number's scale is what `BcMath\Number::$scale` reports. The digits of
//! each are the digits php computes; how they are computed is free, so
//! the SIMD shims of `xsse.h` are left out and the vectors of
//! `BC_VECTOR` are kept only where php's own algorithm reads them
//! (`recmul.c`, `div.c`).

use std::cmp::Ordering;

/// `sign`: `PLUS` or `MINUS`. A zero may carry `Minus`, as php's does
/// after some operations; the renderer never prints the sign of a zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sign {
    Plus,
    Minus,
}

impl Sign {
    /// The sign of a product or a quotient of numbers signed `self` and
    /// `other`.
    fn times(self, other: Sign) -> Sign {
        if self == other {
            Sign::Plus
        } else {
            Sign::Minus
        }
    }

    fn flipped(self) -> Sign {
        match self {
            Sign::Plus => Sign::Minus,
            Sign::Minus => Sign::Plus,
        }
    }
}

/// `bc_struct`: `len` integer digits then `scale` fraction digits, each
/// `0..=9`, most significant first. `digits.len()` is `len + scale`
/// always; a narrowed scale drops the digits past it, which php leaves
/// in storage unread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Num {
    sign: Sign,
    len: usize,
    scale: usize,
    digits: Vec<u8>,
}

/// `BC_VECTOR` on a 64-bit build: eight decimal digits a limb.
type Limb = u64;
/// `BC_VECTOR_SIZE`.
const LIMB_DIGITS: usize = 8;
/// `BC_VECTOR_BOUNDARY_NUM`, `BASE ** BC_VECTOR_SIZE`.
const LIMB_BASE: Limb = 100_000_000;
/// `BC_POW_10_LUT`.
const POW10: [Limb; 9] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
];
/// `BC_VECTOR_NO_OVERFLOW_ADD_COUNT`: how many products a limb takes
/// before the carry is spread.
const NO_OVERFLOW_ADD_COUNT: usize = (Limb::MAX / (LIMB_BASE * LIMB_BASE)) as usize;

/// `bc_raise_status`'s failures, which `bc_pow_err` words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RaiseError {
    /// `BC_RAISE_STATUS_DIVIDE_BY_ZERO`: zero to a negative power.
    DivideByZero,
    /// `BC_RAISE_STATUS_LEN_IS_OVERFLOW`, `SCALE_IS_OVERFLOW` and
    /// `FULLLEN_IS_OVERFLOW`, which php words alike.
    Overflow,
}

/// `raise_mod_status`'s failures, in php's order of checking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RaiseModError {
    BaseHasFractional,
    ExpoHasFractional,
    ExpoIsNegative,
    ModHasFractional,
    ModIsZero,
}

/// php's `PHP_ROUND_*` modes, the eight `bc_round` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rounding {
    HalfUp,
    HalfDown,
    HalfEven,
    HalfOdd,
    Ceiling,
    Floor,
    TowardZero,
    AwayFromZero,
}

impl Num {
    /// What `_bc_new_num_nonzeroed_ex_internal` asks `pemalloc` for a
    /// number of this shape: `sizeof(bc_struct)`, thirty-two bytes, and
    /// the digits, rounded down to `ZEND_MM_ALIGNMENT` after seven are
    /// added (`libbcmath/src/init.c`); `None` for zero, which
    /// `bc_str2num` and `bc_long2num` answer with `BCG(_zero_)` and
    /// allocate nothing for.
    pub(crate) fn storage(&self) -> Option<usize> {
        const BC_STRUCT: usize = 32;
        if self.is_zero() && self.len == 1 && self.scale == 0 {
            return None;
        }
        Some((BC_STRUCT + 7 + self.len + self.scale) & !7)
    }

    /// `bc_new_num`: zeros, `len` of them before the point and `scale`
    /// after, signed `Plus`.
    fn zeros(len: usize, scale: usize) -> Num {
        Num {
            sign: Sign::Plus,
            len,
            scale,
            digits: vec![0; len + scale],
        }
    }

    /// `BCG(_zero_)`.
    pub(crate) fn zero() -> Num {
        Num::zeros(1, 0)
    }

    /// `BCG(_one_)`.
    pub(crate) fn one() -> Num {
        Num::small(1)
    }

    /// A one-digit integer, `BCG(_one_)` and `BCG(_two_)`.
    fn small(digit: u8) -> Num {
        Num {
            sign: Sign::Plus,
            len: 1,
            scale: 0,
            digits: vec![digit],
        }
    }

    /// `n_scale`.
    pub(crate) fn scale(&self) -> usize {
        self.scale
    }

    /// `bc_is_neg`.
    pub(crate) fn is_neg(&self) -> bool {
        self.sign == Sign::Minus
    }

    /// Narrow `n_scale` to at most `scale`, the digits past it dropped:
    /// php's `num->n_scale = MIN(scale, num->n_scale)`.
    pub(crate) fn narrow(&mut self, scale: usize) {
        if scale < self.scale {
            self.scale = scale;
            self.digits.truncate(self.len + scale);
        }
    }

    /// Whether the integer part is the single digit zero, the shape of a
    /// number below one in magnitude.
    fn int_is_zero_digit(&self) -> bool {
        self.len == 1 && self.digits.first() == Some(&0)
    }

    /// `_bc_rm_leading_zeros`: the integer part's leading zeros dropped,
    /// one digit kept.
    fn rm_leading_zeros(&mut self) {
        let zeros = self.digits[..self.len - 1]
            .iter()
            .take_while(|&&d| d == 0)
            .count();
        if zeros > 0 {
            self.digits.drain(..zeros);
            self.len -= zeros;
        }
    }

    /// `bc_rm_trailing_zeros`: the fraction's trailing zeros dropped.
    pub(crate) fn rm_trailing_zeros(&mut self) {
        while self.scale > 0 && self.digits.last() == Some(&0) {
            self.digits.pop();
            self.scale -= 1;
        }
    }

    /// `bc_is_zero_for_scale`: whether the integer part and the first
    /// `scale` fraction digits are all zero. A scale past the number's
    /// reads only its own digits, those past it being zeros.
    fn is_zero_for_scale(&self, scale: usize) -> bool {
        let count = self.len + scale.min(self.scale);
        self.digits[..count].iter().all(|&d| d == 0)
    }

    /// `bc_is_zero`.
    pub(crate) fn is_zero(&self) -> bool {
        self.is_zero_for_scale(self.scale)
    }

    /// `bc_is_near_zero`: every digit up to `scale` zero but a last one.
    fn is_near_zero(&self, scale: usize) -> bool {
        let count = self.len + scale.min(self.scale);
        let digits = &self.digits[..count];
        match digits.iter().position(|&d| d != 0) {
            None => true,
            Some(at) => at + 1 == count && digits[at] == 1,
        }
    }

    /// `bc_str2num`: `text` read as php reads a numeric string for
    /// bcmath, up to its first NUL as C reads it: an optional sign,
    /// digits, and an optional point with digits after it, nothing else;
    /// the fraction's trailing zeros dropped, and with `auto_scale` unset
    /// the fraction cut to `scale` digits. The number and `full_scale`,
    /// the fraction's length as written, or `None` for a string that is
    /// no number.
    pub(crate) fn parse(text: &[u8], scale: usize, auto_scale: bool) -> Option<(Num, usize)> {
        let s = php_ext::coerce::c_str(text);
        let digit_run =
            |from: usize| from + s[from..].iter().take_while(|b| b.is_ascii_digit()).count();
        let mut at = usize::from(matches!(s.first(), Some(b'+' | b'-')));
        at += s[at..].iter().take_while(|&&b| b == b'0').count();
        let integer_at = at;
        at = digit_run(at);
        let mut digits = at - integer_at;
        let integer_end = at;
        let point = s.get(at) == Some(&b'.');
        if !point && at != s.len() {
            return None;
        }
        let mut full_scale = 0;
        let (fraction_at, mut fraction_end) = (at + 1, at + 1);
        let mut str_scale = 0;
        if point && fraction_at < s.len() {
            fraction_end = digit_run(fraction_at);
            if fraction_end != s.len() {
                return None;
            }
            full_scale = fraction_end - fraction_at;
            let skip_zeros = |mut end: usize| {
                while end > fraction_at && s[end - 1] == b'0' {
                    end -= 1;
                }
                end
            };
            fraction_end = skip_zeros(fraction_end);
            str_scale = fraction_end - fraction_at;
            if str_scale > scale && !auto_scale {
                fraction_end -= str_scale - scale;
                str_scale = scale;
                if str_scale > 0 {
                    fraction_end = skip_zeros(fraction_end);
                    str_scale = fraction_end - fraction_at;
                }
            }
        }
        if digits + str_scale == 0 {
            return Some((Num::zero(), full_scale));
        }
        let mut out = Vec::with_capacity(digits.max(1) + str_scale);
        if digits == 0 {
            digits = 1;
            out.push(0);
        } else {
            out.extend(s[integer_at..integer_end].iter().map(|b| b - b'0'));
        }
        if str_scale > 0 {
            out.extend(s[fraction_at..fraction_end].iter().map(|b| b - b'0'));
        }
        let sign = if s.first() == Some(&b'-') {
            Sign::Minus
        } else {
            Sign::Plus
        };
        let num = Num {
            sign,
            len: digits,
            scale: str_scale,
            digits: out,
        };
        Some((num, full_scale))
    }

    /// `bc_long2num`.
    pub(crate) fn from_long(value: i64) -> Num {
        if value == 0 {
            return Num::zero();
        }
        let digits: Vec<u8> = value
            .unsigned_abs()
            .to_string()
            .bytes()
            .map(|b| b - b'0')
            .collect();
        Num {
            sign: if value < 0 { Sign::Minus } else { Sign::Plus },
            len: digits.len(),
            scale: 0,
            digits,
        }
    }

    /// `bc_num2long`: the integer part, or zero where it does not fit a
    /// long, which the caller tells from a true zero by the digits.
    pub(crate) fn to_long(&self) -> i64 {
        let mut value: i64 = 0;
        for &digit in &self.digits[..self.len] {
            let Some(next) = value
                .checked_mul(10)
                .and_then(|v| v.checked_add(i64::from(digit)))
            else {
                return 0;
            };
            value = next;
        }
        match self.sign {
            Sign::Plus => value,
            Sign::Minus => -value,
        }
    }

    /// Whether the integer part is anything but the single digit zero,
    /// what `bc_num2long`'s callers test a zero answer against.
    pub(crate) fn int_is_nonzero(&self) -> bool {
        !self.int_is_zero_digit()
    }

    /// `bc_num2str_ex`: the number with `scale` fraction digits, cut or
    /// padded with zeros, and a minus sign unless what is shown is zero.
    pub(crate) fn render(&self, scale: usize) -> Vec<u8> {
        let shown = self.scale.min(scale);
        let signed = self.sign == Sign::Minus && !self.is_zero_for_scale(shown);
        let mut out = Vec::with_capacity(self.len + scale + 2);
        if signed {
            out.push(b'-');
        }
        out.extend(self.digits[..self.len].iter().map(|d| d + b'0'));
        if scale > 0 {
            out.push(b'.');
            out.extend(
                self.digits[self.len..self.len + shown]
                    .iter()
                    .map(|d| d + b'0'),
            );
            out.resize(out.len() + (scale - shown), b'0');
        }
        out
    }

    /// The digit at `index` of a number laid out with `len` integer
    /// digits, this one's point aligned with it; zero outside it.
    fn aligned(&self, index: usize, len: usize) -> u8 {
        (index + self.len)
            .checked_sub(len)
            .and_then(|at| self.digits.get(at))
            .copied()
            .unwrap_or(0)
    }
}

/// `_bc_do_compare`: the order of `n1` and `n2` over their integer parts
/// and the first `scale` fraction digits, by magnitude alone where
/// `use_sign` is unset.
fn do_compare(n1: &Num, n2: &Num, scale: usize, use_sign: bool) -> Ordering {
    let signed = |left_greater: bool| {
        let greater = if left_greater {
            Ordering::Greater
        } else {
            Ordering::Less
        };
        if !use_sign || n1.sign == Sign::Plus {
            greater
        } else {
            greater.reverse()
        }
    };
    if use_sign && n1.sign != n2.sign {
        // `0.00 <=> -0.00` where a Number's scale is narrower than its
        // digits.
        if (n1.scale > scale || n2.scale > scale)
            && n1.int_is_zero_digit()
            && n2.int_is_zero_digit()
            && n1.is_zero_for_scale(scale)
            && n2.is_zero_for_scale(scale)
        {
            return Ordering::Equal;
        }
        return if n1.sign == Sign::Plus {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }
    if n1.len != n2.len {
        return signed(n1.len > n2.len);
    }
    let (s1, s2) = (n1.scale.min(scale), n2.scale.min(scale));
    let common = n1.len + s1.min(s2);
    if let Some((a, b)) = n1.digits[..common]
        .iter()
        .zip(&n2.digits[..common])
        .find(|(a, b)| a != b)
    {
        return signed(a > b);
    }
    if s1 > s2 && n1.digits[common..n1.len + s1].iter().any(|&d| d != 0) {
        return signed(true);
    }
    if s2 > s1 && n2.digits[common..n2.len + s2].iter().any(|&d| d != 0) {
        return signed(false);
    }
    Ordering::Equal
}

/// `bc_compare`.
pub(crate) fn compare(n1: &Num, n2: &Num, scale: usize) -> Ordering {
    do_compare(n1, n2, scale, true)
}

/// `_bc_do_add`: the magnitudes summed, one more integer digit than the
/// longer for the carry, the longer scale, the leading zeros dropped.
fn do_add(n1: &Num, n2: &Num) -> Num {
    let len = n1.len.max(n2.len) + 1;
    let scale = n1.scale.max(n2.scale);
    let mut sum = Num::zeros(len, scale);
    let mut carry = 0;
    for (index, slot) in sum.digits.iter_mut().enumerate().rev() {
        let total = n1.aligned(index, len) + n2.aligned(index, len) + carry;
        *slot = total % 10;
        carry = total / 10;
    }
    sum.rm_leading_zeros();
    sum
}

/// `_bc_do_sub`: the magnitude of `n2` taken from that of `n1`, which
/// is the larger, over the longer integer part and scale.
fn do_sub(n1: &Num, n2: &Num) -> Num {
    let len = n1.len.max(n2.len);
    let scale = n1.scale.max(n2.scale);
    let mut diff = Num::zeros(len, scale);
    let mut borrow = 0;
    for (index, slot) in diff.digits.iter_mut().enumerate().rev() {
        let (a, b) = (n1.aligned(index, len), n2.aligned(index, len) + borrow);
        (*slot, borrow) = if a >= b { (a - b, 0) } else { (a + 10 - b, 1) };
    }
    diff.rm_leading_zeros();
    diff
}

/// `bc_add`: the sum; where the signs differ the magnitudes are
/// compared over `scale_min` digits, and equal ones are a zero of the
/// widest scale.
pub(crate) fn add(n1: &Num, n2: &Num, scale_min: usize) -> Num {
    if n1.sign == n2.sign {
        let mut sum = do_add(n1, n2);
        sum.sign = n1.sign;
        return sum;
    }
    match do_compare(n1, n2, scale_min, false) {
        Ordering::Less => {
            let mut sum = do_sub(n2, n1);
            sum.sign = n2.sign;
            sum
        }
        Ordering::Equal => Num::zeros(1, scale_min.max(n1.scale).max(n2.scale)),
        Ordering::Greater => {
            let mut sum = do_sub(n1, n2);
            sum.sign = n1.sign;
            sum
        }
    }
}

/// `bc_sub`: `n1 - n2`, as [`add`] is.
pub(crate) fn sub(n1: &Num, n2: &Num, scale_min: usize) -> Num {
    if n1.sign != n2.sign {
        let mut diff = do_add(n1, n2);
        diff.sign = n1.sign;
        return diff;
    }
    match do_compare(n1, n2, scale_min, false) {
        Ordering::Less => {
            let mut diff = do_sub(n2, n1);
            diff.sign = n2.sign.flipped();
            diff
        }
        Ordering::Equal => Num::zeros(1, scale_min.max(n1.scale).max(n2.scale)),
        Ordering::Greater => {
            let mut diff = do_sub(n1, n2);
            diff.sign = n1.sign;
            diff
        }
    }
}

/// `bc_convert_to_vector`: `digits` as limbs, least significant first.
fn limbs(digits: &[u8]) -> Vec<Limb> {
    digits
        .rchunks(LIMB_DIGITS)
        .map(|chunk| chunk.iter().fold(0, |value, &d| value * 10 + Limb::from(d)))
        .collect()
}

/// `bc_convert_vector_to_char`: `limbs` written right-aligned into
/// `out`, its width; digits past the width dropped, the rest zeros.
fn write_limbs(limbs: &[Limb], out: &mut [u8]) {
    let mut digits = limbs.iter().flat_map(|&limb| {
        let mut limb = limb;
        (0..LIMB_DIGITS).map(move |_| {
            let digit = (limb % 10) as u8;
            limb /= 10;
            digit
        })
    });
    for slot in out.iter_mut().rev() {
        *slot = digits.next().unwrap_or(0);
    }
}

/// `bc_mul_carry_calc`: each limb's overflow carried into the next; the
/// top limb keeps what it holds.
fn carry_limbs(product: &mut [Limb]) {
    for i in 0..product.len().saturating_sub(1) {
        let carry = product[i] / LIMB_BASE;
        product[i + 1] = product[i + 1].wrapping_add(carry);
        product[i] %= LIMB_BASE;
    }
}

/// `bc_standard_vector_mul`: the product of two limb arrays into
/// `size` limbs, the carry spread as often as a limb could overflow.
fn multiply_limbs(n1: &[Limb], n2: &[Limb], size: usize) -> Vec<Limb> {
    let mut product = vec![0; size];
    let mut count = 0;
    for (i, &a) in n1.iter().enumerate() {
        if count >= NO_OVERFLOW_ADD_COUNT {
            carry_limbs(&mut product);
            count = 0;
        }
        count += 1;
        for (j, &b) in n2.iter().enumerate() {
            product[i + j] = product[i + j].wrapping_add(a.wrapping_mul(b));
        }
    }
    carry_limbs(&mut product);
    product
}

/// `bc_multiply`: the product, cut to the widest of `scale` and the
/// factors' scales and never past the exact scale, a zero signed `Plus`
/// with no fraction.
pub(crate) fn multiply(n1: &Num, n2: &Num, scale: usize) -> Num {
    let full_scale = n1.scale + n2.scale;
    let product_scale = full_scale.min(scale.max(n1.scale).max(n2.scale));
    let width = n1.digits.len() + n2.digits.len();
    let (a, b) = (limbs(&n1.digits), limbs(&n2.digits));
    let size = width.div_ceil(LIMB_DIGITS);
    let product = multiply_limbs(&a, &b, size);
    let mut prod = Num::zeros(width - full_scale, full_scale);
    write_limbs(&product, &mut prod.digits);
    prod.sign = n1.sign.times(n2.sign);
    prod.narrow(product_scale);
    prod.rm_leading_zeros();
    if prod.is_zero() {
        prod.sign = Sign::Plus;
        prod.narrow(0);
    }
    prod
}

/// `bc_fast_div`: the quotient limbs of a divisor of one limb.
fn fast_div(numerator: &mut [Limb], divisor: Limb, quot: &mut [Limb]) {
    let top = numerator.len() - 1;
    let quot_top = quot.len() - 1;
    for i in 0..quot_top {
        let n = numerator[top - i];
        if n < divisor {
            quot[quot_top - i] = 0;
            numerator[top - i - 1] = numerator[top - i - 1].wrapping_add(n.wrapping_mul(LIMB_BASE));
            continue;
        }
        quot[quot_top - i] = n / divisor;
        let rest = n - divisor * quot[quot_top - i];
        numerator[top - i - 1] = numerator[top - i - 1].wrapping_add(rest.wrapping_mul(LIMB_BASE));
        numerator[top - i] = 0;
    }
    quot[0] = numerator[0] / divisor;
}

/// `bc_standard_div`: php's restoring division over limbs, each
/// quotient limb guessed from `BC_VECTOR_SIZE + 1` leading digits of the
/// divisor and restored at most once.
fn standard_div(numerator: &mut [Limb], divisor: &[Limb], divisor_len: usize, quot: &mut [Limb]) {
    let top = numerator.len() - 1;
    let divisor_top = divisor.len() - 1;
    let quot_top = quot.len() - 1;
    let mut div_carry: Limb = 0;
    let top_digits = match divisor_len % LIMB_DIGITS {
        0 => LIMB_DIGITS,
        digits => digits,
    };
    let high_shift = POW10[LIMB_DIGITS - top_digits + 1];
    let low_shift = POW10[top_digits - 1];
    let divisor_high = divisor[divisor_top]
        .wrapping_mul(high_shift)
        .wrapping_add(divisor[divisor_top - 1] / low_shift);
    for i in 0..quot.len() {
        let mut numerator_high = numerator[top - i]
            .wrapping_mul(high_shift)
            .wrapping_add(numerator[top - i - 1] / low_shift);
        numerator_high =
            numerator_high.wrapping_add(div_carry.wrapping_mul(LIMB_BASE).wrapping_mul(high_shift));
        if numerator_high < divisor_high {
            quot[quot_top - i] = 0;
            div_carry = numerator[top - i];
            numerator[top - i] = 0;
            continue;
        }
        let mut guess = numerator_high / divisor_high;
        numerator[top - i] = numerator[top - i].wrapping_add(div_carry.wrapping_mul(LIMB_BASE));
        if guess == 0 {
            quot[quot_top - i] = 0;
            div_carry = numerator[top - i];
            numerator[top - i] = 0;
            continue;
        }
        let bottom = numerator.len() - divisor.len() - i;
        let mut borrow: Limb = 0;
        for (j, &d) in divisor[..divisor_top].iter().enumerate() {
            let sub = d.wrapping_mul(guess).wrapping_add(borrow);
            let sub_low = sub % LIMB_BASE;
            borrow = sub / LIMB_BASE;
            let slot = &mut numerator[bottom + j];
            if *slot >= sub_low {
                *slot -= sub_low;
            } else {
                *slot = slot.wrapping_add(LIMB_BASE - sub_low);
                borrow += 1;
            }
        }
        let sub = divisor[divisor_top]
            .wrapping_mul(guess)
            .wrapping_add(borrow);
        let last = bottom + divisor_top;
        let negative = numerator[last] < sub;
        numerator[last] = numerator[last].wrapping_sub(sub);
        if negative {
            guess -= 1;
            let mut carry: Limb = 0;
            for (j, &d) in divisor[..divisor_top].iter().enumerate() {
                let slot = &mut numerator[bottom + j];
                *slot = slot.wrapping_add(d).wrapping_add(carry);
                carry = *slot / LIMB_BASE;
                *slot %= LIMB_BASE;
            }
            numerator[last] = numerator[last]
                .wrapping_add(divisor[divisor_top])
                .wrapping_add(carry);
        }
        quot[quot_top - i] = guess;
        div_carry = numerator[top - i];
        numerator[top - i] = 0;
    }
}

/// `bc_divide`: the quotient cut to `scale` fraction digits, or `None`
/// for a divisor of zero. The shapes php gives a divisor of one and a
/// power of ten are kept, since their scales are observable.
pub(crate) fn divide(numerator: &Num, divisor: &Num, scale: usize) -> Option<Num> {
    if divisor.is_zero() {
        return None;
    }
    if numerator.is_zero() {
        return Some(Num::zero());
    }
    if do_compare(divisor, &Num::one(), divisor.scale, false) == Ordering::Equal {
        let kept = numerator.scale.min(scale);
        let mut quot = Num {
            sign: numerator.sign.times(divisor.sign),
            len: numerator.len,
            scale: kept,
            digits: numerator.digits[..numerator.len + kept].to_vec(),
        };
        quot.digits.shrink_to_fit();
        return Some(quot);
    }
    let readable_all = numerator.digits.len();
    let mut numerator_size = numerator.len + scale + divisor.scale;
    let numerator_digit = |i: usize| numerator.digits.get(i).copied().unwrap_or(0);
    let mut leading = 0;
    while numerator_digit(leading) == 0 {
        leading += 1;
        if leading == numerator_size {
            return Some(Num::zero());
        }
    }
    numerator_size -= leading;
    let divisor_leading = divisor.digits.iter().take_while(|&&d| d == 0).count();
    let mut divisor_digits = &divisor.digits[divisor_leading..];
    if divisor_digits.len() > numerator_size {
        return Some(Num::zero());
    }
    let trailing = divisor_digits[1..]
        .iter()
        .rev()
        .take_while(|&&d| d == 0)
        .count();
    divisor_digits = &divisor_digits[..divisor_digits.len() - trailing];
    numerator_size -= trailing;
    let divisor_size = divisor_digits.len();
    let quot_size = numerator_size - divisor_size + 1;
    let mut quot = if quot_size > scale {
        Num::zeros(quot_size - scale, scale)
    } else {
        Num::zeros(1, scale)
    };
    let readable = readable_all - leading;
    let sign = numerator.sign.times(divisor.sign);
    let from = &numerator.digits[leading..];
    if divisor_size == 1 && divisor_digits[0] == 1 {
        divide_by_pow_10(from, readable, &mut quot, quot_size, scale);
        quot.sign = sign;
        return Some(quot);
    }
    // The numerator's window, zeros past what it holds.
    let mut window: Vec<u8> = from[..readable.min(numerator_size)].to_vec();
    window.resize(numerator_size, 0);
    let mut numerator_limbs = limbs(&window);
    let divisor_limbs = limbs(divisor_digits);
    let quot_limbs_len = numerator_limbs.len() - divisor_limbs.len() + 1;
    let mut quot_limbs = vec![0; quot_limbs_len];
    if divisor_limbs.len() == 1 {
        fast_div(&mut numerator_limbs, divisor_limbs[0], &mut quot_limbs);
    } else {
        standard_div(
            &mut numerator_limbs,
            &divisor_limbs,
            divisor_size,
            &mut quot_limbs,
        );
    }
    let real = quot_limbs_len.min(quot_size.div_ceil(LIMB_DIGITS));
    write_limbs(&quot_limbs[..real], &mut quot.digits);
    quot.rm_leading_zeros();
    if quot.is_zero() {
        quot.sign = Sign::Plus;
        quot.narrow(0);
    } else {
        quot.sign = sign;
    }
    Some(quot)
}

/// `bc_divide_by_pow_10`: the numerator's digits moved past the point,
/// the scale narrowed to the digits the numerator had.
fn divide_by_pow_10(from: &[u8], readable: usize, quot: &mut Num, quot_size: usize, scale: usize) {
    let mut at = 0;
    for _ in quot_size..=scale {
        quot.digits[at] = 0;
        at += 1;
    }
    let used = quot_size.min(readable);
    quot.digits[at..at + used].copy_from_slice(&from[..used]);
    at += used;
    if used < quot.len {
        // e.g. 12.3 / 0.01 <=> 1230
        let len = quot.len;
        quot.digits[at..len].fill(0);
        quot.narrow(0);
    } else {
        let unwritten = quot.digits.len() - at;
        quot.scale -= unwritten;
        quot.digits.truncate(at);
    }
}

/// `bc_divmod`: the quotient truncated to an integer and the remainder
/// at `scale`, or `None` for a divisor of zero.
pub(crate) fn divmod(num1: &Num, num2: &Num, scale: usize) -> Option<(Num, Num)> {
    if num2.is_zero() {
        return None;
    }
    let rscale = num1.scale.max(num2.scale + scale);
    let quotient = divide(num1, num2, 0)?;
    let product = multiply(&quotient, num2, rscale);
    let mut rem = sub(num1, &product, rscale);
    rem.narrow(scale);
    if rem.is_zero() {
        rem.sign = Sign::Plus;
        rem.narrow(0);
    }
    Some((quotient, rem))
}

/// `bc_modulo`.
pub(crate) fn modulo(num1: &Num, num2: &Num, scale: usize) -> Option<Num> {
    divmod(num1, num2, scale).map(|(_, rem)| rem)
}

/// `bc_raise`: `base` to the integer power `exponent`, a negative one
/// through `1 / base ** -exponent` at `scale`.
pub(crate) fn raise(base: &Num, exponent: i64, scale: usize) -> Result<Num, RaiseError> {
    if exponent == 0 {
        return Ok(Num::one());
    }
    let is_neg = exponent < 0;
    let exponent = exponent.unsigned_abs();
    let rscale = if is_neg {
        scale
    } else {
        usize::try_from(exponent)
            .ok()
            .and_then(|e| base.scale.checked_mul(e))
            .unwrap_or(usize::MAX)
            .min(scale.max(base.scale))
    };
    if base.is_zero() {
        return if is_neg {
            Err(RaiseError::DivideByZero)
        } else {
            Ok(Num::zero())
        };
    }
    let Ok(times) = usize::try_from(exponent) else {
        return Err(RaiseError::Overflow);
    };
    let (Some(power_len), Some(power_scale)) =
        (base.len.checked_mul(times), base.scale.checked_mul(times))
    else {
        return Err(RaiseError::Overflow);
    };
    if power_len.checked_add(power_scale).is_none() {
        return Err(RaiseError::Overflow);
    }
    let sign = if base.sign == Sign::Minus && exponent & 1 == 1 {
        Sign::Minus
    } else {
        Sign::Plus
    };
    let leading = base.digits.iter().take_while(|&&d| d == 0).count();
    let value = power_limbs(limbs(&base.digits[leading..]), exponent);
    let mut power = from_limbs(&value, power_scale);
    power.sign = sign;
    if is_neg {
        return divide(&Num::one(), &power, rscale).ok_or(RaiseError::DivideByZero);
    }
    power.narrow(scale);
    Ok(power)
}

/// `base ** exponent` over limbs, by squaring as `bc_standard_raise`
/// does, each product stripped of its zero top limbs.
fn power_limbs(base: Vec<Limb>, mut exponent: u64) -> Vec<Limb> {
    let square = |limbs: &[Limb]| trimmed(multiply_limbs(limbs, limbs, limbs.len() * 2));
    let mut base = base;
    while exponent & 1 == 0 {
        base = square(&base);
        exponent >>= 1;
    }
    let mut power = base.clone();
    exponent >>= 1;
    while exponent > 0 {
        base = square(&base);
        if exponent & 1 == 1 {
            power = trimmed(multiply_limbs(&power, &base, power.len() + base.len()));
        }
        exponent >>= 1;
    }
    power
}

/// A limb array without its zero top limbs, one kept.
fn trimmed(mut limbs: Vec<Limb>) -> Vec<Limb> {
    while limbs.len() > 1 && limbs.last() == Some(&0) {
        limbs.pop();
    }
    limbs
}

/// The integer `value` read with `scale` fraction digits, laid out as
/// `bc_standard_raise` lays out a power: one integer digit at least, the
/// leading zeros dropped.
fn from_limbs(value: &[Limb], scale: usize) -> Num {
    let width = (value.len() * LIMB_DIGITS).max(scale + 1);
    let mut num = Num::zeros(width - scale, scale);
    write_limbs(value, &mut num.digits);
    num.rm_leading_zeros();
    if num.is_zero() {
        num.narrow(0);
    }
    num
}

/// `bc_raisemod`: `base ** expo % modulus` over integers, by the
/// binary method on the decimal exponent as php walks it.
pub(crate) fn raisemod(
    base: &Num,
    expo: &Num,
    modulus: &Num,
    scale: usize,
) -> Result<Num, RaiseModError> {
    if base.scale != 0 {
        return Err(RaiseModError::BaseHasFractional);
    }
    if expo.scale != 0 {
        return Err(RaiseModError::ExpoHasFractional);
    }
    if expo.is_neg() {
        return Err(RaiseModError::ExpoIsNegative);
    }
    if modulus.scale != 0 {
        return Err(RaiseModError::ModHasFractional);
    }
    if modulus.is_zero() {
        return Err(RaiseModError::ModIsZero);
    }
    if do_compare(modulus, &Num::one(), modulus.scale, false) == Ordering::Equal {
        return Ok(Num::zeros(1, scale));
    }
    let two = Num::small(2);
    let mut power = base.clone();
    let mut exponent = expo.clone();
    let mut temp = Num::one();
    while !exponent.is_zero() {
        let (half, parity) = divmod(&exponent, &two, 0).ok_or(RaiseModError::ModIsZero)?;
        exponent = half;
        if !parity.is_zero() {
            temp = multiply(&temp, &power, scale);
            temp = modulo(&temp, modulus, scale).ok_or(RaiseModError::ModIsZero)?;
        }
        power = multiply(&power, &power, scale);
        power = modulo(&power, modulus, scale).ok_or(RaiseModError::ModIsZero)?;
    }
    Ok(temp)
}

/// `bc_int2num`.
fn from_int(value: usize) -> Num {
    Num::from_long(i64::try_from(value).unwrap_or(i64::MAX))
}

/// `bc_sqrt`: the square root at the wider of `scale` and the number's
/// own, by php's Newton iteration, or `None` for a negative number.
pub(crate) fn sqrt(num: &Num, scale: usize) -> Option<Num> {
    if num.is_neg() {
        return None;
    }
    if num.is_zero() {
        return Some(Num::zero());
    }
    let against_one = compare(num, &Num::one(), num.scale);
    if against_one == Ordering::Equal {
        return Some(Num::one());
    }
    let rscale = scale.max(num.scale);
    let point5 = Num {
        sign: Sign::Plus,
        len: 1,
        scale: 1,
        digits: vec![0, 5],
    };
    let (mut guess, mut cscale) = if against_one == Ordering::Less {
        (Num::one(), num.scale)
    } else {
        let mut half = multiply(&from_int(num.len), &point5, 0);
        half.narrow(0);
        let exponent = half.to_long();
        let guess = raise(&from_int(10), exponent, 0).ok()?;
        (guess, 3)
    };
    loop {
        let previous = guess.clone();
        guess = divide(num, &guess, cscale)?;
        guess = add(&guess, &previous, 0);
        guess = multiply(&guess, &point5, cscale);
        let diff = sub(&guess, &previous, cscale + 1);
        if diff.is_near_zero(cscale) {
            if cscale < rscale + 1 {
                cscale = (cscale * 3).min(rscale + 1);
            } else {
                break;
            }
        }
    }
    divide(&guess, &Num::one(), rscale)
}

/// `bc_floor_or_ceil`: the integer part, one further from zero where a
/// fraction digit is set and the direction is away from it.
pub(crate) fn floor_or_ceil(num: &Num, is_floor: bool) -> Num {
    let mut result = Num {
        sign: num.sign,
        len: num.len,
        scale: 0,
        digits: num.digits[..num.len].to_vec(),
    };
    let toward_zero = if is_floor { Sign::Plus } else { Sign::Minus };
    let fraction_set = num.digits[num.len..].iter().any(|&d| d != 0);
    if num.scale != 0 && result.sign != toward_zero && fraction_set {
        let mut stepped = do_add(&result, &Num::one());
        stepped.sign = result.sign;
        result = stepped;
    }
    if result.is_zero() {
        result.sign = Sign::Plus;
    }
    result
}

/// `bc_round`: `num` rounded at `precision` places in `mode`, with the
/// scale php reports for it, which may be wider than the digits kept.
pub(crate) fn round(num: &Num, precision: i64, mode: Rounding) -> (Num, usize) {
    if precision < 0 {
        let places = precision.unsigned_abs();
        if (num.len as u64) < places {
            let to_zero = match mode {
                Rounding::HalfUp
                | Rounding::HalfDown
                | Rounding::HalfEven
                | Rounding::HalfOdd
                | Rounding::TowardZero => true,
                Rounding::Ceiling => num.sign == Sign::Minus,
                Rounding::Floor => num.sign == Sign::Plus,
                Rounding::AwayFromZero => false,
            };
            if to_zero || num.is_zero() {
                return (Num::zero(), 0);
            }
            let len = usize::try_from(places).map_or(usize::MAX, |p| p.saturating_add(1));
            let mut result = Num::zeros(len, 0);
            result.digits[0] = 1;
            result.sign = num.sign;
            return (result, 0);
        }
    }
    if let Ok(places) = usize::try_from(precision)
        && num.scale <= places
    {
        let mut result = num.clone();
        if num.scale < places {
            result.scale = places;
            result.digits.resize(num.len + places, 0);
        }
        return (result, places);
    }
    // `precision` is at least `-num.len` here, so the kept length is
    // not negative.
    let rounded_len = match usize::try_from(precision) {
        Ok(places) => num.len + places,
        Err(_) => num
            .len
            .saturating_sub(usize::try_from(precision.unsigned_abs()).unwrap_or(usize::MAX)),
    };
    let fraction = usize::try_from(precision).unwrap_or(0);
    let mut result = if rounded_len == 0 {
        Num::zeros(1, 0)
    } else {
        let mut result = Num::zeros(num.len, fraction);
        result.digits[..rounded_len].copy_from_slice(&num.digits[..rounded_len]);
        result
    };
    result.sign = num.sign;
    let first = num.digits[rounded_len];
    let rest_set = || num.digits[rounded_len + 1..].iter().any(|&d| d != 0);
    let up = match mode {
        Rounding::HalfUp => first >= 5,
        Rounding::HalfDown | Rounding::HalfEven | Rounding::HalfOdd if first != 5 => first > 5,
        Rounding::HalfDown => rest_set(),
        Rounding::HalfEven => {
            rest_set() || (rounded_len != 0 && num.digits[rounded_len - 1] % 2 == 1)
        }
        Rounding::HalfOdd => {
            rest_set() || rounded_len == 0 || num.digits[rounded_len - 1].is_multiple_of(2)
        }
        Rounding::Ceiling => num.sign == Sign::Plus && (first > 0 || rest_set()),
        Rounding::Floor => num.sign == Sign::Minus && (first > 0 || rest_set()),
        Rounding::TowardZero => false,
        Rounding::AwayFromZero => first > 0 || rest_set(),
    };
    if up {
        let stepped = if rounded_len == 0 {
            let mut one = Num::zeros(num.len + 1, 0);
            one.digits[0] = 1;
            one
        } else {
            let mut unit = Num::zeros(result.len, result.scale);
            unit.digits[rounded_len - 1] = 1;
            do_add(&result, &unit)
        };
        result = Num {
            sign: num.sign,
            ..stepped
        };
    }
    let scale = result.scale;
    if result.is_zero() {
        result.sign = Sign::Plus;
        result.narrow(0);
    }
    (result, scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(text: &str) -> Num {
        Num::parse(text.as_bytes(), 0, true).map_or_else(Num::zero, |(num, _)| num)
    }

    fn s(num: &Num, scale: usize) -> String {
        String::from_utf8_lossy(&num.render(scale)).into_owned()
    }

    #[test]
    fn the_four_operations_keep_php_s_digits() {
        assert_eq!(s(&add(&n("1.5"), &n("-2.25"), 3), 3), "-0.750");
        assert_eq!(s(&sub(&n("1"), &n("1.0001"), 2), 2), "0.00");
        assert_eq!(
            s(&multiply(&n("123456789.123"), &n("-987654321.987"), 5), 5),
            "-121932631355968601.34740"
        );
        assert_eq!(
            s(&divide(&n("1"), &n("3"), 20).unwrap_or_else(Num::zero), 20),
            "0.33333333333333333333"
        );
        assert_eq!(
            s(
                &divide(&n("12.3"), &n("0.01"), 2).unwrap_or_else(Num::zero),
                2
            ),
            "1230.00"
        );
        let big = divide(
            &n("123456789012345678901234567890"),
            &n("98765432109876543210"),
            10,
        );
        assert_eq!(
            s(&big.unwrap_or_else(Num::zero), 10),
            "1249999988.6093750001"
        );
    }

    #[test]
    fn powers_roots_and_rounding() {
        assert_eq!(
            s(&raise(&n("1.5"), 3, 2).unwrap_or_else(|_| Num::zero()), 2),
            "3.37"
        );
        assert_eq!(
            s(&raise(&n("2"), -2, 4).unwrap_or_else(|_| Num::zero()), 4),
            "0.2500"
        );
        assert_eq!(
            s(&sqrt(&n("2"), 10).unwrap_or_else(Num::zero), 10),
            "1.4142135623"
        );
        assert_eq!(
            s(
                &raisemod(&n("4"), &n("13"), &n("497"), 0).unwrap_or_else(|_| Num::zero()),
                0
            ),
            "445"
        );
        let (r, scale) = round(&n("-1.955"), 2, Rounding::HalfUp);
        assert_eq!(s(&r, scale), "-1.96");
        let (r, scale) = round(&n("2.5"), 0, Rounding::HalfEven);
        assert_eq!(s(&r, scale), "2");
    }
}
