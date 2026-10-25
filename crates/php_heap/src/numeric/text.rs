//! A number's text laid in a buffer on the stack, as php lays it before
//! one `zend_string_init` copies it into a request's string:
//! `zend_long_to_str` prints a long backwards from the end of a
//! `char buf[MAX_LENGTH_OF_LONG + 1]` (`Z/Zend/zend_operators.c:3533`),
//! and `zend_double_to_str` and `smart_str_append_double` hand
//! `zend_gcvt` a `char buf[ZEND_DOUBLE_MAX_LENGTH]`
//! (`Z/Zend/zend_operators.c:3618`, `Z/Zend/zend_smart_str.c:158`). No
//! heap allocation is made for the text, and none is read back.

use std::fmt;
use std::mem::MaybeUninit;
use std::ops::Deref;

/// `ZEND_DOUBLE_MAX_LENGTH` (`Z/Zend/zend_strtod.h:46`), `3 +
/// DBL_MANT_DIG - DBL_MIN_EXP`: the longest text `zend_gcvt` writes, a
/// double's whole decimal expansion with its sign, point and zeros.
pub(crate) const DOUBLE_MAX_LENGTH: usize = 3 + 53 + 1021;

/// The text of a number, `N` bytes of room on the stack of which the
/// bytes between `start` and `end` are written, ASCII each: a
/// [`LongText`] or a [`DoubleText`]. It reads as the `str` it holds.
pub struct NumberText<const N: usize> {
    buf: [MaybeUninit<u8>; N],
    start: usize,
    end: usize,
}

/// A long's text, `zend_print_long_to_buf`'s: `i64::MIN`'s sign and
/// nineteen digits fill it.
pub type LongText = NumberText<20>;

/// A double's text, what `zend_gcvt` writes into php's
/// `ZEND_DOUBLE_MAX_LENGTH` bytes.
pub type DoubleText = NumberText<DOUBLE_MAX_LENGTH>;

/// `"00"` to `"99"`, the two digits of each number below a hundred.
const PAIRS: &[u8; 200] = b"\
    0001020304050607080910111213141516171819\
    2021222324252627282930313233343536373839\
    4041424344454647484950515253545556575859\
    6061626364656667686970717273747576777879\
    8081828384858687888990919293949596979899";

impl<const N: usize> NumberText<N> {
    /// No text yet, to be written forward from `at` and backward before
    /// it.
    pub(crate) fn at(at: usize) -> Self {
        let at = at.min(N);
        NumberText {
            buf: [MaybeUninit::uninit(); N],
            start: at,
            end: at,
        }
    }

    /// `bytes` as the whole text, cut to the room.
    pub(crate) fn of(bytes: &[u8]) -> Self {
        let mut text = Self::at(0);
        for &byte in bytes {
            text.push(byte);
        }
        text
    }

    /// The text's bytes.
    #[must_use]
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.as_str().as_bytes()
    }

    /// The text.
    #[must_use]
    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: every byte between `start` and `end` was written by
        // `push`, `push_front`, `set_last` or `open_point`, so the range is
        // initialised.
        let written = unsafe { self.buf[self.start..self.end].assume_init_ref() };
        // SAFETY: each byte written is ASCII: the writers are this
        // crate's, and each hands them a digit, a sign, a point, an
        // exponent's letter or a letter of `INF` or `NAN`.
        unsafe { str::from_utf8_unchecked(written) }
    }

    /// How many bytes are written.
    pub(crate) fn len(&self) -> usize {
        self.end - self.start
    }

    /// Where the text ends, which [`NumberText::truncate`] goes back to.
    pub(crate) fn end(&self) -> usize {
        self.end
    }

    /// `byte` after the text. The room is sized so that no text php
    /// writes outgrows it; a byte past it is dropped.
    pub(crate) fn push(&mut self, byte: u8) {
        debug_assert!(byte.is_ascii() && self.end < N, "{byte} at {}", self.end);
        if let Some(slot) = self.buf.get_mut(self.end) {
            *slot = MaybeUninit::new(byte);
            self.end += 1;
        }
    }

    /// `byte` before the text, as `*--buf = byte` writes it.
    pub(crate) fn push_front(&mut self, byte: u8) {
        debug_assert!(
            byte.is_ascii() && self.start > 0,
            "{byte} at {}",
            self.start
        );
        if let Some(at) = self.start.checked_sub(1) {
            self.buf[at] = MaybeUninit::new(byte);
            self.start = at;
        }
    }

    /// The last byte, where there is one.
    pub(crate) fn last(&self) -> Option<u8> {
        if self.end == self.start {
            return None;
        }
        // SAFETY: `end - 1` is inside the written range, so it is
        // initialised.
        Some(unsafe { self.buf[self.end - 1].assume_init() })
    }

    /// The last byte rewritten as `byte`; nothing where the text is
    /// empty.
    pub(crate) fn set_last(&mut self, byte: u8) {
        debug_assert!(byte.is_ascii(), "{byte}");
        if self.end > self.start {
            self.buf[self.end - 1] = MaybeUninit::new(byte);
        }
    }

    /// The last byte taken off.
    pub(crate) fn pop(&mut self) {
        if self.end > self.start {
            self.end -= 1;
        }
    }

    /// The text cut back to end at `end`, an [`NumberText::end`] it had.
    pub(crate) fn truncate(&mut self, end: usize) {
        self.end = end.clamp(self.start, self.end);
    }

    /// A point after the first `at` bytes, which move back one place
    /// to make room for it: `zend_gcvt`'s `*dst++ = dec_point` among the
    /// digits. Room before the text is the caller's to leave.
    pub(crate) fn open_point(&mut self, at: usize) {
        debug_assert!(self.start > 0 && at <= self.len());
        let Some(to) = self.start.checked_sub(1) else {
            return;
        };
        let at = at.min(self.len());
        self.buf.copy_within(self.start..self.start + at, to);
        self.buf[to + at] = MaybeUninit::new(b'.');
        self.start = to;
    }
}

impl LongText {
    /// `zend_print_long_to_buf` (`Z/Zend/zend_operators.h:999`): `n`'s
    /// decimal digits written back from the end of the room, two at a
    /// time, then its sign. The text php prints; the pairs are not
    /// observable.
    #[must_use]
    pub fn long(n: i64) -> Self {
        let mut buf = [MaybeUninit::uninit(); 20];
        let mut start = buf.len();
        let mut magnitude = n.unsigned_abs();
        let mut pair = |at: usize, two: u64| {
            let two = usize::try_from(two).unwrap_or(0) * 2;
            buf[at] = MaybeUninit::new(PAIRS[two]);
            buf[at + 1] = MaybeUninit::new(PAIRS[two + 1]);
        };
        // A magnitude has nineteen digits at most, so nine pairs and one
        // more digit or pair leave room for the sign.
        while magnitude >= 100 {
            start -= 2;
            pair(start, magnitude % 100);
            magnitude /= 100;
        }
        if magnitude >= 10 {
            start -= 2;
            pair(start, magnitude);
        } else {
            start -= 1;
            // A digit, below ten.
            buf[start] = MaybeUninit::new(b'0' + u8::try_from(magnitude).unwrap_or(0));
        }
        if n < 0 {
            start -= 1;
            buf[start] = MaybeUninit::new(b'-');
        }
        NumberText {
            buf,
            start,
            end: 20,
        }
    }
}

impl<const N: usize> Deref for NumberText<N> {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl<const N: usize> fmt::Display for NumberText<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const N: usize> fmt::Debug for NumberText<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const N: usize> PartialEq<&str> for NumberText<N> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A long's text is Rust's `to_string`'s at the edges and between.
    #[test]
    fn a_long_is_written_as_its_decimal() {
        let mut n: i64 = 1;
        let mut values = vec![0, i64::MIN, i64::MAX, i64::MIN + 1];
        for _ in 0..19 {
            values.extend([n - 1, n, n + 1, -n + 1, -n, -n - 1]);
            n = n.saturating_mul(10);
        }
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..if cfg!(miri) { 200 } else { 10_000 } {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            values.push(i64::from_ne_bytes(x.to_ne_bytes()) >> (x % 64));
        }
        for n in values {
            assert_eq!(LongText::long(n).as_str(), n.to_string(), "{n}");
        }
    }

    /// The point opens among the digits, before them and after them,
    /// and the bytes written before and after the text are its own.
    #[test]
    fn a_point_opens_where_it_is_asked_for() {
        for (at, text) in [(0, ".123"), (1, "1.23"), (3, "123.")] {
            let mut digits = DoubleText::at(2);
            for byte in *b"123" {
                digits.push(byte);
            }
            digits.open_point(at);
            assert_eq!(digits, text);
        }
        let mut digits = DoubleText::at(1);
        digits.push(b'5');
        digits.push_front(b'-');
        assert_eq!(digits.last(), Some(b'5'));
        digits.set_last(b'6');
        assert_eq!(digits, "-6");
        let end = digits.end();
        digits.push(b'7');
        digits.truncate(end);
        assert_eq!((digits.as_bytes(), digits.len()), (&b"-6"[..], 2));
    }
}
