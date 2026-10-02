//! ICU's `utf8.h` macros as php's intl calls them over a `char *`: a
//! byte past the end reads as C's terminating NUL, which is how `U8_NEXT`
//! over an empty string reads one code point and moves past the end.

/// `U8_LEAD3_T1_BITS`: the second bytes valid after each three-byte lead.
const LEAD3_T1_BITS: &[u8; 16] =
    b"\x20\x30\x30\x30\x30\x30\x30\x30\x30\x30\x30\x30\x30\x10\x30\x30";
/// `U8_LEAD4_T1_BITS`: the four-byte leads valid before each second byte.
const LEAD4_T1_BITS: &[u8; 16] =
    b"\x00\x00\x00\x00\x00\x00\x00\x00\x1E\x0F\x0F\x0F\x00\x00\x00\x00";

/// The byte at `i`, the NUL a C string ends with past the end.
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// `U8_IS_SINGLE`.
pub(crate) fn is_single(b: u8) -> bool {
    b & 0x80 == 0
}

/// `U8_IS_LEAD`.
pub(crate) fn is_lead(b: u8) -> bool {
    b.wrapping_sub(0xc2) <= 0x32
}

/// `U8_IS_TRAIL`.
fn is_trail(b: u8) -> bool {
    b & 0xc0 == 0x80
}

/// `U8_IS_VALID_LEAD3_AND_T1`.
fn lead3_t1(lead: u8, t1: u8) -> bool {
    LEAD3_T1_BITS[usize::from(lead & 0xf)] & (1 << (t1 >> 5)) != 0
}

/// `U8_IS_VALID_LEAD4_AND_T1`.
fn lead4_t1(lead: u8, t1: u8) -> bool {
    LEAD4_T1_BITS[usize::from(t1 >> 4)] & (1 << (lead & 7)) != 0
}

/// `U8_NEXT(s, i, length, c)`: the code point at `*i`, `i` moved past
/// it, and `-1` (`U_SENTINEL`) for an ill-formed sequence, `i` past its
/// maximal subpart.
pub(crate) fn next(s: &[u8], i: &mut usize) -> i32 {
    let len = s.len();
    let lead = at(s, *i);
    *i += 1;
    if is_single(lead) {
        return i32::from(lead);
    }
    if *i == len {
        return -1;
    }
    let mut c = i32::from(lead);
    let t = at(s, *i);
    if lead >= 0xf0 {
        if lead > 0xf4 || !lead4_t1(lead, t) {
            return -1;
        }
        c = ((c - 0xf0) << 6) | i32::from(t & 0x3f);
        *i += 1;
        if *i == len || !is_trail(at(s, *i)) {
            return -1;
        }
        c = (c << 6) | i32::from(at(s, *i) & 0x3f);
        *i += 1;
    } else if lead >= 0xe0 {
        if !lead3_t1(lead, t) {
            return -1;
        }
        c = ((c & 0xf) << 6) | i32::from(t & 0x3f);
        *i += 1;
    } else if lead >= 0xc2 {
        c &= 0x1f;
    } else {
        return -1;
    }
    if *i == len || !is_trail(at(s, *i)) {
        return -1;
    }
    c = (c << 6) | i32::from(at(s, *i) & 0x3f);
    *i += 1;
    c
}

/// `U8_FWD_1(s, i, length)`: past one code point or one ill-formed
/// maximal subpart.
pub(crate) fn forward(s: &[u8], i: &mut usize) {
    let len = s.len();
    let lead = at(s, *i);
    *i += 1;
    if !is_lead(lead) || *i == len {
        return;
    }
    let t1 = at(s, *i);
    let trails = match lead {
        0xe0..=0xef if lead3_t1(lead, t1) => 2,
        0xf0.. if lead4_t1(lead, t1) => 3,
        ..0xe0 if is_trail(t1) => 1,
        _ => return,
    };
    *i += 1;
    for _ in 1..trails {
        if *i == len || !is_trail(at(s, *i)) {
            return;
        }
        *i += 1;
    }
}

/// `U8_APPEND_UNSAFE`: the code point's bytes, a surrogate's three as
/// ICU writes them.
pub(crate) fn encode(c: i32) -> Vec<u8> {
    let c = u32::try_from(c).unwrap_or(0);
    let byte = |v: u32| u8::try_from(v & 0xff).unwrap_or(0);
    match c {
        0..0x80 => vec![byte(c)],
        0x80..0x800 => vec![byte((c >> 6) | 0xc0), byte((c & 0x3f) | 0x80)],
        0x800..0x10000 => vec![
            byte((c >> 12) | 0xe0),
            byte(((c >> 6) & 0x3f) | 0x80),
            byte((c & 0x3f) | 0x80),
        ],
        _ => vec![
            byte((c >> 18) | 0xf0),
            byte(((c >> 12) & 0x3f) | 0x80),
            byte(((c >> 6) & 0x3f) | 0x80),
            byte((c & 0x3f) | 0x80),
        ],
    }
}
