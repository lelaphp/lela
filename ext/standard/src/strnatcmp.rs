//! `Z/ext/standard/strnatcmp.c`: Martin Pool's natural-order
//! comparison as php carries it, `strnatcmp_ex` with `compare_left`
//! for digits that start with a zero and `compare_right` for one that
//! does not, behind `strnatcmp` and `strnatcasecmp` here and
//! `SORT_NATURAL` once the sorts read it; spaces, digits and capitals
//! are `isspace(3)`'s, `isdigit(3)`'s and `toupper(3)`'s under the
//! thread's `LC_CTYPE`.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::cmp::Ordering;

use php_ext::attr::php_function;
use php_ext::coerce::is_space;
use php_ext::values::PhpString;

use crate::ffi::locale::{self as c, Class};

/// `isdigit(3)` under the thread's `LC_CTYPE`.
fn is_digit(byte: u8) -> bool {
    c::is(Class::Digit, byte)
}

/// `compare_right`: the longer span of digits wins, else the first
/// difference, remembered as the bias.
fn compare_right(a: &[u8], ap: &mut usize, b: &[u8], bp: &mut usize) -> i64 {
    let mut bias = 0;
    loop {
        let da = *ap < a.len() && is_digit(a[*ap]);
        let db = *bp < b.len() && is_digit(b[*bp]);
        match (da, db) {
            (false, false) => return bias,
            (false, true) => return -1,
            (true, false) => return 1,
            (true, true) => {
                if bias == 0 {
                    bias = match a[*ap].cmp(&b[*bp]) {
                        Ordering::Less => -1,
                        Ordering::Greater => 1,
                        Ordering::Equal => 0,
                    };
                }
            }
        }
        *ap += 1;
        *bp += 1;
    }
}

/// `compare_left`: left-aligned digits, the first difference wins.
fn compare_left(a: &[u8], ap: &mut usize, b: &[u8], bp: &mut usize) -> i64 {
    loop {
        let da = *ap < a.len() && is_digit(a[*ap]);
        let db = *bp < b.len() && is_digit(b[*bp]);
        match (da, db) {
            (false, false) => return 0,
            (false, true) => return -1,
            (true, false) => return 1,
            (true, true) => match a[*ap].cmp(&b[*bp]) {
                Ordering::Less => return -1,
                Ordering::Greater => return 1,
                Ordering::Equal => {}
            },
        }
        *ap += 1;
        *bp += 1;
    }
}

/// `strnatcmp_ex(a, a_len, b, b_len, is_case_insensitive)`: -1, 0 or 1.
pub(super) fn strnatcmp_ex(a: &[u8], b: &[u8], fold: bool) -> i64 {
    if a.is_empty() || b.is_empty() {
        return match a.len().cmp(&b.len()) {
            Ordering::Equal => 0,
            Ordering::Greater => 1,
            Ordering::Less => -1,
        };
    }
    // Past the end reads the terminator, as php's pointers do.
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let (mut ap, mut bp) = (0usize, 0usize);
    let (mut ca, mut cb) = (a[0], b[0]);
    while ca == b'0' && ap + 1 < a.len() && is_digit(a[ap + 1]) {
        ap += 1;
        ca = a[ap];
    }
    while cb == b'0' && bp + 1 < b.len() && is_digit(b[bp + 1]) {
        bp += 1;
        cb = b[bp];
    }
    loop {
        while is_space(ca) {
            ap += 1;
            ca = at(a, ap);
        }
        while is_space(cb) {
            bp += 1;
            cb = at(b, bp);
        }
        if is_digit(ca) && is_digit(cb) {
            let result = if ca == b'0' || cb == b'0' {
                compare_left(a, &mut ap, b, &mut bp)
            } else {
                compare_right(a, &mut ap, b, &mut bp)
            };
            if result != 0 {
                return result;
            }
            match (ap == a.len(), bp == b.len()) {
                (true, true) => return 0,
                (true, false) => return -1,
                (false, true) => return 1,
                (false, false) => {
                    ca = a[ap];
                    cb = b[bp];
                }
            }
        }
        if fold {
            ca = c::to_upper(ca);
            cb = c::to_upper(cb);
        }
        match ca.cmp(&cb) {
            Ordering::Less => return -1,
            Ordering::Greater => return 1,
            Ordering::Equal => {}
        }
        ap += 1;
        bp += 1;
        match (ap >= a.len(), bp >= b.len()) {
            (true, true) => return 0,
            (true, false) => return -1,
            (false, true) => return 1,
            (false, false) => {
                ca = a[ap];
                cb = b[bp];
            }
        }
    }
}

/// `strnatcmp(string $string1, string $string2): int`.
///
/// ```php
/// var_dump(strnatcmp("img12.png", "img10.png"), strnatcmp("img2", "img10"), strnatcmp("a", "A"), strnatcmp("0.5", "0.25"), strnatcmp("x 1", "x   1"), strnatcmp("", "a"), strnatcmp("007", "7"), strnatcmp("a01", "a1"), strnatcmp("a", "a"), strnatcmp("1.10", "1.9"));
/// ```
/// ```expect
/// int(1)
/// int(-1)
/// int(1)
/// int(-1)
/// int(0)
/// int(-1)
/// int(0)
/// int(-1)
/// int(0)
/// int(1)
/// ```
#[php_function]
pub(super) fn strnatcmp(string1: PhpString, string2: PhpString) -> i64 {
    strnatcmp_ex(string1.as_bytes(), string2.as_bytes(), false)
}

/// `strnatcasecmp(...)`.
///
/// ```php
/// var_dump(strnatcasecmp("a", "A"), strnatcasecmp("IMG2", "img10"), strnatcasecmp("Hello10", "hello9"), strnatcasecmp("", ""));
/// ```
/// ```expect
/// int(0)
/// int(-1)
/// int(1)
/// int(0)
/// ```
///
/// The capitals and the spaces are the request's `LC_CTYPE`'s: `\xe9`
/// folds to `\xc9` under an ISO-8859-1 locale, and macOS's `C.UTF-8`
/// skips `\xa0` as a space.
///
/// ```php
/// foreach (['C', 'fr_FR.ISO8859-1'] as $l) { setlocale(LC_CTYPE, $l); var_dump(strnatcasecmp("\xe9t\xe92", "\xc9T\xc910"), strnatcmp("a\xa01", "a1")); }
/// setlocale(LC_CTYPE, 'C.UTF-8'); var_dump(strnatcmp("a\xa01", "a1"));
/// ```
/// ```expect locale=fr_FR.ISO8859-1
/// int(1)
/// int(1)
/// int(-1)
/// int(1)
/// int(0)
/// ```
#[php_function]
pub(super) fn strnatcasecmp(string1: PhpString, string2: PhpString) -> i64 {
    strnatcmp_ex(string1.as_bytes(), string2.as_bytes(), true)
}
