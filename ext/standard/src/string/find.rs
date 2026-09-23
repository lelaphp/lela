//! The searches of `string.c`: `strpos` and its three kin, `strstr`
//! with its alias and `stristr`, `strrchr`, and `str_starts_with` and
//! `str_ends_with`. Case folding is `zend_tolower_ascii`.

use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::Value;

use super::{long, lower, memnstr};
use crate::zpp::{optional_bool, optional_long, string_value};
use crate::{argument_count_between, string_argument};

/// `zend_memnrstr`: the last offset of `needle` inside
/// `haystack[from..to]`, the empty needle found at `to`.
fn memnrstr(haystack: &[u8], needle: &[u8], from: usize, to: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(to);
    }
    let window = haystack.get(from..to)?;
    if needle.len() > window.len() {
        return None;
    }
    window
        .windows(needle.len())
        .rposition(|w| w == needle)
        .map(|at| at + from)
}

/// The two strings of a search and its optional third argument.
fn search_arguments(
    req: &mut Request,
) -> Result<(php_ext::values::PhpString, php_ext::values::PhpString), Thrown> {
    argument_count_between(req, 2, 3)?;
    let haystack = string_argument(req, 1)?;
    let needle = string_argument(req, 2)?;
    Ok((haystack, needle))
}

/// `zend_argument_value_error(3, "must be contained in argument #1
/// ($haystack)")`.
fn outside(req: &mut Request) -> Thrown {
    arg_value_error(req, 3, "be contained in argument #1 ($haystack)")
}

/// `strpos(string $haystack, string $needle, int $offset = 0):
/// int|false`, `_zend_strpos`: the first offset of the needle at or
/// after `$offset`, a negative one counted from the end, and the
/// `ValueError` for an offset outside the haystack.
///
/// ```php
/// var_dump(strpos("hello", "l"), strpos("hello", "l", 3), strpos("hello", "l", -2), strpos("hello", "x"), strpos("hello", ""), strpos("hello", "", 5), strpos("abcabc", "bc", -3));
/// var_dump(stripos("HeLLo", "ll"), stripos("abc", "C", 1), stripos("abc", "d"));
/// foreach ([6, -6] as $o) { try { strpos("hello", "l", $o); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } }
/// try { stripos("a", "a", 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// int(3)
/// int(3)
/// bool(false)
/// int(0)
/// int(5)
/// int(4)
/// int(2)
/// int(2)
/// bool(false)
/// strpos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// strpos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// stripos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// ```
pub(super) fn strpos(req: &mut Request) -> Result<Value, Thrown> {
    positioned(req, false)
}

/// `stripos(...)`, `php_memnistr` over the same window.
///
/// ```php
/// var_dump(stripos("HeLLo", "ll"), stripos("abc", "C", 1), stripos("abc", "d"), stripos("ABC", "", 3), stripos("aBc", "B", -2));
/// try { stripos("a", "a", 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// int(2)
/// bool(false)
/// int(3)
/// int(1)
/// stripos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// ```
pub(super) fn stripos(req: &mut Request) -> Result<Value, Thrown> {
    positioned(req, true)
}

fn positioned(req: &mut Request, fold: bool) -> Result<Value, Thrown> {
    let (haystack, needle) = search_arguments(req)?;
    let mut offset = optional_long(req, 3, 0)?;
    let len = long(haystack.len());
    if offset < 0 {
        offset += len;
    }
    if offset < 0 || offset > len {
        return Err(outside(req));
    }
    let from = usize::try_from(offset).unwrap_or(0);
    let found = if fold {
        let h: Vec<u8> = haystack.as_bytes().iter().map(|c| lower(*c)).collect();
        let n: Vec<u8> = needle.as_bytes().iter().map(|c| lower(*c)).collect();
        memnstr(&h, &n, from)
    } else {
        memnstr(haystack.as_bytes(), needle.as_bytes(), from)
    };
    Ok(found.map_or(Value::bool(false), |at| Value::int(long(at))))
}

/// `strrpos(string $haystack, string $needle, int $offset = 0):
/// int|false`: the last offset of the needle; a non-negative offset is
/// where the search starts, a negative one how far from the end the
/// match may start at the latest.
///
/// ```php
/// var_dump(strrpos("hello", "l"), strrpos("hello", "l", -3), strrpos("hello", "l", 4), strrpos("hello", "ll", -3), strrpos("hello", ""), strrpos("hello", "", -1), strrpos("hello", "x"));
/// var_dump(strripos("HeLLo", "l"), strripos("HeLLo", "LL", -2), strripos("abcABC", "a", 1), strripos("abcABC", "a", -4), strripos("abc", "x"), strripos("abc", "", 1));
/// foreach ([6, -6] as $o) { try { strrpos("hello", "l", $o); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } try { strripos("hello", "l", $o); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } }
/// ```
/// ```expect
/// int(3)
/// int(2)
/// bool(false)
/// int(2)
/// int(5)
/// int(4)
/// bool(false)
/// int(3)
/// int(2)
/// int(3)
/// int(0)
/// bool(false)
/// int(3)
/// strrpos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// strripos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// strrpos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// strripos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// ```
pub(super) fn strrpos(req: &mut Request) -> Result<Value, Thrown> {
    let (haystack, needle) = search_arguments(req)?;
    let offset = optional_long(req, 3, 0)?;
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    let Some((from, to)) = reverse_window(h.len(), n.len(), offset) else {
        return Err(outside(req));
    };
    Ok(memnrstr(h, n, from, to).map_or(Value::bool(false), |at| Value::int(long(at))))
}

/// `strrpos`'s window, `p` and `e`: `None` for an offset outside the
/// haystack.
fn reverse_window(hlen: usize, nlen: usize, offset: i64) -> Option<(usize, usize)> {
    if offset >= 0 {
        let from = usize::try_from(offset).ok().filter(|o| *o <= hlen)?;
        Some((from, hlen))
    } else {
        let back = usize::try_from(offset.checked_neg()?)
            .ok()
            .filter(|b| *b <= hlen)?;
        let to = if back < nlen {
            hlen
        } else {
            hlen - back + nlen
        };
        Some((0, to))
    }
}

/// `strripos(...)`: `strrpos` over the lowercased strings, and php's
/// shortcut for a one-byte needle, which walks back from the end of
/// the window one byte further than the general path.
///
/// ```php
/// var_dump(strripos("HeLLo", "l"), strripos("HeLLo", "LL", -2), strripos("abcABC", "a", 1), strripos("abcABC", "a", -4), strripos("abc", "x"), strripos("abc", "", 1), strripos("aXa", "x", -1));
/// foreach ([6, -6] as $o) { try { strripos("hello", "l", $o); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } }
/// ```
/// ```expect
/// int(3)
/// int(2)
/// int(3)
/// int(0)
/// bool(false)
/// int(3)
/// int(1)
/// strripos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// strripos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// ```
pub(super) fn strripos(req: &mut Request) -> Result<Value, Thrown> {
    let (haystack, needle) = search_arguments(req)?;
    let offset = optional_long(req, 3, 0)?;
    let h: Vec<u8> = haystack.as_bytes().iter().map(|c| lower(*c)).collect();
    let n: Vec<u8> = needle.as_bytes().iter().map(|c| lower(*c)).collect();
    if n.len() == 1 {
        // php walks `e` back from the window's last byte to `p`, and
        // answers `e - p` plus a positive offset, which is `e` itself.
        let window = if offset >= 0 {
            usize::try_from(offset)
                .ok()
                .filter(|o| *o <= h.len())
                .map(|p| (p, h.len()))
        } else {
            offset
                .checked_neg()
                .and_then(|b| usize::try_from(b).ok())
                .filter(|b| *b <= h.len())
                .map(|back| (0, h.len() - back + 1))
        };
        let Some((p, end)) = window else {
            return Err(outside(req));
        };
        let found = h
            .get(p..end.min(h.len()))
            .and_then(|span| span.iter().rposition(|c| *c == n[0]));
        return Ok(found.map_or(Value::bool(false), |at| Value::int(long(at + p))));
    }
    let Some((from, to)) = reverse_window(h.len(), n.len(), offset) else {
        return Err(outside(req));
    };
    Ok(memnrstr(&h, &n, from, to).map_or(Value::bool(false), |at| Value::int(long(at))))
}

/// `strstr(string $haystack, string $needle, bool $before_needle =
/// false): string|false` and its alias `strchr`: the haystack from the
/// first needle, or before it.
///
/// ```php
/// var_dump(strstr("user@example.com", "@"), strstr("user@example.com", "@", true), strchr("abc", "x"), strstr("abc", ""), stristr("USER@EXAMPLE", "e"), stristr("USER@EXAMPLE", "e", true), stristr("abc", "X"));
/// ```
/// ```expect
/// string(12) "@example.com"
/// string(4) "user"
/// bool(false)
/// string(3) "abc"
/// string(10) "ER@EXAMPLE"
/// string(2) "US"
/// bool(false)
/// ```
pub(super) fn strstr(req: &mut Request) -> Result<Value, Thrown> {
    found_part(req, false)
}

/// `stristr(...)`.
///
/// ```php
/// var_dump(stristr("USER@EXAMPLE", "e"), stristr("USER@EXAMPLE", "e", true), stristr("abc", "X"), stristr("abc", ""));
/// ```
/// ```expect
/// string(10) "ER@EXAMPLE"
/// string(2) "US"
/// bool(false)
/// string(3) "abc"
/// ```
pub(super) fn stristr(req: &mut Request) -> Result<Value, Thrown> {
    found_part(req, true)
}

fn found_part(req: &mut Request, fold: bool) -> Result<Value, Thrown> {
    let (haystack, needle) = search_arguments(req)?;
    let part = optional_bool(req, 3, false)?;
    let h = haystack.as_bytes();
    let found = if fold {
        let lh: Vec<u8> = h.iter().map(|c| lower(*c)).collect();
        let ln: Vec<u8> = needle.as_bytes().iter().map(|c| lower(*c)).collect();
        memnstr(&lh, &ln, 0)
    } else {
        memnstr(h, needle.as_bytes(), 0)
    };
    match found {
        None => Ok(Value::bool(false)),
        Some(at) if part => string_value(req, &h[..at]),
        Some(at) => string_value(req, &h[at..]),
    }
}

/// `strrchr(string $haystack, string $needle, bool $before_needle =
/// false): string|false`: the haystack from the last byte equal to the
/// needle's first, the terminator's zero for an empty needle.
///
/// ```php
/// var_dump(strrchr("a/b/c", "/"), strrchr("a/b/c", "/x"), strrchr("a/b/c", "/", true), strrchr("abc", "x"), bin2hex(strrchr("a\0b", "")));
/// ```
/// ```expect
/// string(2) "/c"
/// string(2) "/c"
/// string(3) "a/b"
/// bool(false)
/// string(4) "0062"
/// ```
pub(super) fn strrchr(req: &mut Request) -> Result<Value, Thrown> {
    let (haystack, needle) = search_arguments(req)?;
    let part = optional_bool(req, 3, false)?;
    let h = haystack.as_bytes();
    let c = needle.as_bytes().first().copied().unwrap_or(0);
    match h.iter().rposition(|b| *b == c) {
        None => Ok(Value::bool(false)),
        Some(at) if part => string_value(req, &h[..at]),
        Some(at) => string_value(req, &h[at..]),
    }
}

/// `str_starts_with(string $haystack, string $needle): bool` and
/// `str_ends_with`: whether the needle's bytes open, or close, the
/// haystack.
///
/// ```php
/// var_dump(str_starts_with("abc", "ab"), str_starts_with("abc", ""), str_starts_with("abc", "b"), str_ends_with("abc", "bc"), str_ends_with("abc", ""), str_ends_with("a", "ab"));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
pub(super) fn str_starts_with(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let haystack = string_argument(req, 1)?;
    let needle = string_argument(req, 2)?;
    Ok(Value::bool(
        haystack.as_bytes().starts_with(needle.as_bytes()),
    ))
}

/// `str_ends_with(...)`.
///
/// ```php
/// var_dump(str_ends_with("abc", "bc"), str_ends_with("abc", ""), str_ends_with("a", "ab"), str_ends_with("", ""), str_ends_with("abc", "b"));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
pub(super) fn str_ends_with(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let haystack = string_argument(req, 1)?;
    let needle = string_argument(req, 2)?;
    Ok(Value::bool(
        haystack.as_bytes().ends_with(needle.as_bytes()),
    ))
}
