//! The searches of `string.c`: `strpos` and its three kin, `strstr`
//! with its alias and `stristr`, `strrchr`, and `str_starts_with` and
//! `str_ends_with`. Case folding is `zend_tolower_ascii`.

use php_ext::abi::OrFalse;
use php_ext::attr::php_function;
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString};

use super::{long, lower, memnstr};

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
///
/// Declared frameless for two arguments and for three
/// (`frameless_function_infos_strpos`), so those calls are a
/// `FRAMELESS_ICALL_2` or `_3` over this body, and a call by name or
/// through a callable the framed call; the two agree.
///
/// ```php
/// var_dump(strpos("abcabc", "c"), strpos("abcabc", "c", 3), strpos(haystack: "abc", needle: "c"), strpos("abc", "x", 1), call_user_func('strpos', "abc", "c", -1), strpos(123, 2));
/// try { strpos("abc", "a", 9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// int(5)
/// int(2)
/// bool(false)
/// int(2)
/// int(1)
/// strpos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// ```
#[php_function(frameless(2, 3))]
pub(super) fn strpos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] offset: i64,
) -> Result<OrFalse<i64>, Thrown> {
    positioned(req, (haystack, needle, offset), false)
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
#[php_function]
pub(super) fn stripos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] offset: i64,
) -> Result<OrFalse<i64>, Thrown> {
    positioned(req, (haystack, needle, offset), true)
}

fn positioned(
    req: &mut Request,
    (haystack, needle, mut offset): (PhpString, PhpString, i64),
    fold: bool,
) -> Result<OrFalse<i64>, Thrown> {
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
    Ok(found.map_or(OrFalse::False, |at| OrFalse::Value(long(at))))
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
#[php_function]
pub(super) fn strrpos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] offset: i64,
) -> Result<OrFalse<i64>, Thrown> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    let Some((from, to)) = reverse_window(h.len(), n.len(), offset) else {
        return Err(outside(req));
    };
    Ok(memnrstr(h, n, from, to).map_or(OrFalse::False, |at| OrFalse::Value(long(at))))
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
#[php_function]
pub(super) fn strripos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] offset: i64,
) -> Result<OrFalse<i64>, Thrown> {
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
        return Ok(found.map_or(OrFalse::False, |at| OrFalse::Value(long(at + p))));
    }
    let Some((from, to)) = reverse_window(h.len(), n.len(), offset) else {
        return Err(outside(req));
    };
    Ok(memnrstr(&h, &n, from, to).map_or(OrFalse::False, |at| OrFalse::Value(long(at))))
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
///
/// Declared frameless for two arguments and for three
/// (`frameless_function_infos_strstr`), so those calls are a
/// `FRAMELESS_ICALL_2` or `_3` over this body; `strchr`, the alias,
/// declares none, as php's `ZEND_RAW_FENTRY` passes none for it.
///
/// ```php
/// var_dump(strstr("user@host", "@"), strstr("user@host", "@", true), strstr("abc", "b", before_needle: true), strstr("abc", "x"), strchr("abc", "b"), call_user_func('strstr', "abc", "b", true));
/// ```
/// ```expect
/// string(5) "@host"
/// string(4) "user"
/// string(1) "a"
/// bool(false)
/// string(2) "bc"
/// string(1) "a"
/// ```
#[php_function(frameless(2, 3), aliases(strchr))]
pub(super) fn strstr(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = false)] before_needle: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    found_part(req, (haystack, needle, before_needle), false)
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
#[php_function]
pub(super) fn stristr(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = false)] before_needle: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    found_part(req, (haystack, needle, before_needle), true)
}

fn found_part(
    req: &mut Request,
    (haystack, needle, part): (PhpString, PhpString, bool),
    fold: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let h = haystack.as_bytes();
    let found = if fold {
        let lh: Vec<u8> = h.iter().map(|c| lower(*c)).collect();
        let ln: Vec<u8> = needle.as_bytes().iter().map(|c| lower(*c)).collect();
        memnstr(&lh, &ln, 0)
    } else {
        memnstr(h, needle.as_bytes(), 0)
    };
    match found {
        None => Ok(OrFalse::False),
        Some(at) if part => PhpString::new(req, &h[..at]).map(OrFalse::Value),
        Some(at) => PhpString::new(req, &h[at..]).map(OrFalse::Value),
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
#[php_function]
pub(super) fn strrchr(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = false)] before_needle: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let part = before_needle;
    let h = haystack.as_bytes();
    let c = needle.as_bytes().first().copied().unwrap_or(0);
    match h.iter().rposition(|b| *b == c) {
        None => Ok(OrFalse::False),
        Some(at) if part => PhpString::new(req, &h[..at]).map(OrFalse::Value),
        Some(at) => PhpString::new(req, &h[at..]).map(OrFalse::Value),
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
///
/// Declared frameless for its two arguments
/// (`frameless_function_infos_str_starts_with`), so such a call is a
/// `FRAMELESS_ICALL_2` over this body, and a call by name or through a
/// callable the framed call; the two agree.
///
/// ```php
/// var_dump(str_starts_with("abc", "ab"), str_starts_with("abc", ""), str_starts_with(haystack: "abc", needle: "b"), str_starts_with(12, 1), call_user_func('str_starts_with', "abc", "abcd"));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
#[php_function(frameless(2))]
pub(super) fn str_starts_with(haystack: PhpString, needle: PhpString) -> bool {
    haystack.as_bytes().starts_with(needle.as_bytes())
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
#[php_function]
pub(super) fn str_ends_with(haystack: PhpString, needle: PhpString) -> bool {
    haystack.as_bytes().ends_with(needle.as_bytes())
}
