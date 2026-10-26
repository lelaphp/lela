//! `grapheme/`: the `grapheme_*` functions over ICU's character break
//! iterator, string search and collator (`grapheme_string.c`,
//! `grapheme_util.c`). php's one global iterator is cloned for every
//! call, so each call here opens its own, which nothing can tell apart.
//! Nothing here resets the request's error first, as php resets none.

use php_ext::abi::{ExtensionBuilder, OrFalse, Path, Ref, Untyped};
use php_ext::attr::php_function;
use php_ext::raising::{Thrown, arg_value_error, argument_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use crate::error;
use crate::ffi::collator::Collator;
use crate::ffi::grapheme::{Breaks, DONE, Search};
use crate::ffi::heap::Block;
use crate::ffi::{self, Code};
use crate::utf8;

/// `grapheme_*` in the stub's order.
pub(crate) fn functions(b: ExtensionBuilder) -> ExtensionBuilder {
    b.functions(vec![
        grapheme_strlen::declared(),
        grapheme_strpos::declared(),
        grapheme_stripos::declared(),
        grapheme_strrpos::declared(),
        grapheme_strripos::declared(),
        grapheme_substr::declared(),
        grapheme_strstr::declared(),
        grapheme_stristr::declared(),
        grapheme_str_split::declared(),
        grapheme_levenshtein::declared(),
        grapheme_extract::declared(),
    ])
}

/// `UINT_MAX / 4`, the bound php puts on a length and a cost.
const QUARTER: i64 = 1_073_741_823;

/// A failure's code and message set on the request; the caller answers
/// `false`.
fn fail(req: &mut Request, code: Code, message: &str) -> Result<(), Thrown> {
    error::set(req, code, message)
}

/// `intl_convert_utf8_to_utf16`, a failure set with `message`.
fn utf16(req: &mut Request, text: &[u8], message: &str) -> Result<Option<Vec<u16>>, Thrown> {
    match ffi::to_utf16(text) {
        Ok(text) => Ok(Some(text)),
        Err(code) => fail(req, code, message).map(|()| None),
    }
}

/// `intl_convert_utf16_to_utf8` of what php answers, its conversion
/// error set where it fails.
fn utf8_out(req: &mut Request, text: &[u16]) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    match ffi::to_utf8(text) {
        Ok(bytes) => Ok(OrFalse::Value(PhpString::new(req, &bytes)?)),
        Err(code) => {
            fail(req, code, "Error converting output string to UTF-8")?;
            Ok(OrFalse::False)
        }
    }
}

/// A byte string answered.
fn bytes(req: &mut Request, text: &[u8]) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    PhpString::new(req, text).map(OrFalse::Value)
}

/// `grapheme_ascii_check` over the first `len` bytes of `s`: no byte above
/// 0x7f and no CR LF pair, the byte after the last read as php reads it,
/// past the slice or not.
fn ascii(s: &[u8], len: usize) -> bool {
    (0..len).all(|i| {
        let next = s.get(i + 1).copied().unwrap_or(0);
        s[i] <= 0x7f && !(s[i] == b'\r' && next == b'\n')
    })
}

/// `OUTSIDE_STRING(offset, len)`.
fn outside(offset: i64, len: usize) -> bool {
    let len = i64::try_from(len).unwrap_or(i64::MAX);
    offset <= i64::from(i32::MIN)
        || offset > i64::from(i32::MAX)
        || if offset < 0 {
            -offset > len
        } else {
            offset > len
        }
}

/// php's `ValueError` for an offset outside the haystack.
fn offset_error(req: &mut Request) -> Thrown {
    arg_value_error(req, 3, "be contained in argument #1 ($haystack)")
}

/// `php_memnstr`: the first occurrence at or after `from`.
fn memnstr(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let rest = haystack.get(from..)?;
    if needle.is_empty() {
        return Some(from);
    }
    rest.windows(needle.len())
        .position(|w| w == needle)
        .map(|at| from + at)
}

/// `grapheme_strrpos_ascii`.
fn strrpos_ascii(haystack: &[u8], needle: &[u8], offset: i32) -> i64 {
    let (h, n, off) = (len_of(haystack), len_of(needle), i64::from(offset));
    let (p, mut e) = if off >= 0 {
        (off, h - n)
    } else if n > -off {
        (0, h - n)
    } else {
        (0, h + off)
    };
    while e >= p {
        let start = usize::try_from(e).unwrap_or(0);
        if haystack.get(start..start + needle.len()) == Some(needle) {
            return e - p + off.max(0);
        }
        e -= 1;
    }
    -1
}

/// A length as php's signed arithmetic reads it.
fn len_of(s: &[u8]) -> i64 {
    i64::try_from(s.len()).unwrap_or(i64::MAX)
}

/// `grapheme_get_haystack_offset`: the code unit `offset` graphemes from
/// the start (from the end where negative), `-1` past either end.
fn haystack_offset(breaks: &mut Breaks<'_>, mut offset: i32) -> i32 {
    if offset == 0 {
        return 0;
    }
    let backward = offset < 0;
    if backward {
        breaks.last();
    }
    let mut pos = 0;
    while pos != DONE && offset != 0 {
        pos = if backward {
            breaks.previous()
        } else {
            breaks.next()
        };
        if pos != DONE {
            offset += if backward { 1 } else { -1 };
        }
    }
    if offset == 0 { pos } else { -1 }
}

/// `grapheme_count_graphemes` (and `grapheme_split_string` counting):
/// the boundaries after the start, `-1` where no iterator opens.
fn count_graphemes(text: &[u16]) -> i64 {
    let Ok(mut breaks) = Breaks::utf16(text) else {
        return -1;
    };
    let mut count = 0;
    while breaks.next() != DONE {
        count += 1;
    }
    count
}

/// A search's two strings and the switches `grapheme_strpos_utf16`
/// takes.
struct Find<'a> {
    haystack: &'a [u8],
    needle: &'a [u8],
    offset: i32,
    ignore_case: bool,
    last: bool,
    locale: &'a [u8],
}

/// Where `grapheme_strpos_utf16` found the needle: the graphemes before
/// it, what it answers, and the code points before it, its `puchar_pos`,
/// which `strstr_common_handler` steps the UTF-8 haystack by (none for
/// an empty needle, whose `-1` steps nothing).
#[derive(Clone, Copy)]
struct Found {
    graphemes: i64,
    chars: usize,
}

/// `grapheme_strpos_utf16`: where the match starts, `None` where there
/// is none or an error was set. An empty haystack holds no needle that
/// is not empty, and nothing is converted for it.
fn strpos_utf16(req: &mut Request, find: &Find<'_>) -> Result<Option<Found>, Thrown> {
    if find.haystack.is_empty() && !find.needle.is_empty() {
        return Ok(None);
    }
    let Some(haystack) = utf16(
        req,
        find.haystack,
        "Error converting input string to UTF-16",
    )?
    else {
        return Ok(None);
    };
    let Some(needle) = utf16(req, find.needle, "Error converting needle string to UTF-16")? else {
        return Ok(None);
    };
    let mut breaks = match Breaks::utf16(&haystack) {
        Ok(breaks) => breaks,
        Err(code) => return fail(req, code, "Failed to get iterator").map(|()| None),
    };
    let offset = find.offset;
    if needle.is_empty() {
        let at = haystack_offset(&mut breaks, offset);
        if at == -1 {
            return Err(offset_error(req));
        }
        let end = if find.last && offset >= 0 {
            haystack.len()
        } else {
            usize::try_from(at).unwrap_or(0)
        };
        let graphemes = count_graphemes(haystack.get(..end).unwrap_or(&haystack));
        return Ok(Some(Found {
            graphemes,
            chars: 0,
        }));
    }
    let locale = ffi::c_string(find.locale);
    let mut search = match Search::open(&needle, &haystack, &locale, breaks) {
        Ok(search) => search,
        Err(code) => return fail(req, code, "Error creating search object").map(|()| None),
    };
    if find.ignore_case
        && let Err(code) = search.ignore_case()
    {
        return fail(req, code, "Error setting collation strength").map(|()| None);
    }
    let mut at = 0;
    if offset != 0 {
        at = haystack_offset(search.breaks(), offset);
        if at == -1 {
            return Err(offset_error(req));
        }
        if let Err(code) = search.set_offset(if find.last { 0 } else { at }) {
            return fail(req, code, "Invalid search offset").map(|()| None);
        }
    }
    let mut status = Code::ZERO;
    let found = if !find.last {
        search.next(&mut status)
    } else if offset >= 0 {
        Some(search.last(&mut status))
            .filter(|&p| p >= at)
            .unwrap_or(DONE)
    } else {
        // php searches forwards, backwards being broken, each search
        // from the boundary after the last match, so matches overlap.
        let mut previous = DONE;
        loop {
            let p = search.next(&mut status);
            if p == DONE || p > at {
                break previous;
            }
            previous = p;
            let next = search.breaks().following(p);
            if next == DONE {
                break previous;
            }
            if let Err(code) = search.set_offset(next) {
                return fail(req, code, "Invalid search offset").map(|()| None);
            }
        }
    };
    if status.is_failure() {
        return fail(req, status, "Error looking up string").map(|()| None);
    }
    if found == DONE || !search.breaks().is_boundary(found) {
        return Ok(None);
    }
    let prefix = haystack
        .get(..usize::try_from(found).unwrap_or(0))
        .unwrap_or(&[]);
    Ok(Some(Found {
        graphemes: count_graphemes(prefix),
        chars: char::decode_utf16(prefix.iter().copied()).count(),
    }))
}

/// A position answered, `false` for none.
fn position(found: Option<Found>) -> OrFalse<i64> {
    found.map_or(OrFalse::False, |found| OrFalse::Value(found.graphemes))
}

/// `grapheme_strlen(string $string): int|false|null`.
///
/// ```php
/// var_dump(grapheme_strlen("abc"), grapheme_strlen("a\r\nb"), grapheme_strlen("e\u{301}\u{1F1EB}\u{1F1F7}"), grapheme_strlen("\xff"), intl_get_error_code());
/// ```
/// ```expect
/// int(3)
/// int(3)
/// int(2)
/// NULL
/// int(10)
/// ```
#[php_function]
fn grapheme_strlen(req: &mut Request, string: PhpString) -> Result<Option<OrFalse<i64>>, Thrown> {
    let text = string.as_bytes();
    if ascii(text, text.len()) {
        return Ok(Some(OrFalse::Value(len_of(text))));
    }
    let Some(text) = utf16(req, text, "Error converting input string to UTF-16")? else {
        return Ok(None);
    };
    let count = count_graphemes(&text);
    Ok(Some(if count >= 0 {
        OrFalse::Value(count)
    } else {
        OrFalse::False
    }))
}

/// `grapheme_strpos(string $haystack, string $needle, int $offset = 0, string $locale = ""): int|false`.
///
/// ```php
/// var_dump(grapheme_strpos("abcabc", "c", 3), grapheme_strpos("a\u{301}bc", "b"), grapheme_strpos("a\u{301}bc", "a"), grapheme_strpos("ébc", "c", -1));
/// try { grapheme_strpos("abc", "a", 4); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(5)
/// int(1)
/// bool(false)
/// int(2)
/// grapheme_strpos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// ```
///
/// An ASCII haystack that misses a needle by bytes is searched again,
/// canonically, where the needle is not ASCII, so `K` matches the Kelvin
/// sign; an empty haystack holds no needle, with no error set; and an
/// empty needle answers the grapheme its offset names, not the code unit
/// (php 8.5.12).
///
/// ```php
/// var_dump(grapheme_strpos("aKbK", "\u{212A}", 2), grapheme_strpos("abc", "\u{E9}"), grapheme_strpos("", "\u{E9}"), intl_get_error_code(), grapheme_strpos("😀x", "", 1));
/// ```
/// ```expect
/// int(3)
/// bool(false)
/// bool(false)
/// int(0)
/// int(1)
/// ```
#[php_function]
fn grapheme_strpos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] offset: i64,
    #[php(default = "")] locale: Path,
) -> Result<OrFalse<i64>, Thrown> {
    let (hay, nee) = (haystack.as_bytes(), needle.as_bytes());
    if outside(offset, hay.len()) {
        return Err(offset_error(req));
    }
    let offset = i32::try_from(offset).unwrap_or(0);
    if offset >= 0 && ascii(hay, hay.len()) {
        let from = usize::try_from(offset).unwrap_or(0);
        if let Some(at) = memnstr(hay, nee, from) {
            return Ok(OrFalse::Value(len_of(&hay[..at])));
        }
        // A needle that is not ASCII can still match canonically.
        if ascii(nee, nee.len()) {
            return Ok(OrFalse::False);
        }
    }
    let find = Find {
        haystack: hay,
        needle: nee,
        offset,
        ignore_case: false,
        last: false,
        locale: locale.0.as_bytes(),
    };
    strpos_utf16(req, &find).map(position)
}

/// `grapheme_stripos(string $haystack, string $needle, int $offset = 0, string $locale = ""): int|false`.
///
/// ```php
/// var_dump(grapheme_stripos("ABCabc", "c", 3), grapheme_stripos("ABC", "c", -1), grapheme_stripos("aÉb", "é"), grapheme_stripos("abc", "é"));
/// ```
/// ```expect
/// int(5)
/// int(2)
/// int(1)
/// bool(false)
/// ```
#[php_function]
fn grapheme_stripos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] offset: i64,
    #[php(default = "")] locale: Path,
) -> Result<OrFalse<i64>, Thrown> {
    let (hay, nee) = (haystack.as_bytes(), needle.as_bytes());
    if outside(offset, hay.len()) {
        return Err(offset_error(req));
    }
    let offset = i32::try_from(offset).unwrap_or(0);
    if ascii(hay, hay.len()) {
        let from = if offset >= 0 {
            i64::from(offset)
        } else {
            len_of(hay) + i64::from(offset)
        };
        let (hay_lower, nee_lower) = (hay.to_ascii_lowercase(), nee.to_ascii_lowercase());
        if let Some(at) = memnstr(&hay_lower, &nee_lower, usize::try_from(from).unwrap_or(0)) {
            return Ok(OrFalse::Value(len_of(&hay[..at])));
        }
        if ascii(nee, nee.len()) {
            return Ok(OrFalse::False);
        }
    }
    let find = Find {
        haystack: hay,
        needle: nee,
        offset,
        ignore_case: true,
        last: false,
        locale: locale.0.as_bytes(),
    };
    strpos_utf16(req, &find).map(position)
}

/// `grapheme_strrpos(string $haystack, string $needle, int $offset = 0, string $locale = ""): int|false`.
///
/// ```php
/// var_dump(grapheme_strrpos("abcabc", "b"), grapheme_strrpos("abcabc", "b", -3), grapheme_strrpos("ébcébc", "é"), grapheme_strrpos("ébcébc", "é", -4), grapheme_strrpos("abc", ""));
/// ```
/// ```expect
/// int(4)
/// int(1)
/// int(3)
/// int(0)
/// int(3)
/// ```
///
/// A negative offset searches forwards from each match's next boundary,
/// so overlapping matches are found; an empty needle answers a count of
/// graphemes (php 8.5.12).
///
/// ```php
/// var_dump(grapheme_strrpos("\u{E9}\u{E9}\u{E9}", "\u{E9}\u{E9}", -1), grapheme_strripos("\u{C9}\u{E9}\u{C9}", "\u{E9}\u{E9}", -1), grapheme_strrpos("😀x", ""), grapheme_strrpos("😀x", "", -1));
/// ```
/// ```expect
/// int(1)
/// int(1)
/// int(2)
/// int(1)
/// ```
#[php_function]
fn grapheme_strrpos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] offset: i64,
    #[php(default = "")] locale: Path,
) -> Result<OrFalse<i64>, Thrown> {
    strrpos(req, haystack, needle, offset, false, locale)
}

/// `grapheme_strripos(string $haystack, string $needle, int $offset = 0, string $locale = ""): int|false`.
///
/// ```php
/// var_dump(grapheme_strripos("abcABC", "b"), grapheme_strripos("ÉbcÉbc", "é"), grapheme_strripos("abc", "x"));
/// ```
/// ```expect
/// int(4)
/// int(3)
/// bool(false)
/// ```
#[php_function]
fn grapheme_strripos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] offset: i64,
    #[php(default = "")] locale: Path,
) -> Result<OrFalse<i64>, Thrown> {
    strrpos(req, haystack, needle, offset, true, locale)
}

/// `grapheme_strrpos` and `grapheme_strripos`.
fn strrpos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    offset: i64,
    ignore_case: bool,
    locale: Path,
) -> Result<OrFalse<i64>, Thrown> {
    let (hay, nee) = (haystack.as_bytes(), needle.as_bytes());
    if outside(offset, hay.len()) {
        return Err(offset_error(req));
    }
    let offset = i32::try_from(offset).unwrap_or(0);
    if ascii(hay, hay.len()) {
        let found = if ignore_case {
            strrpos_ascii(&hay.to_ascii_lowercase(), &nee.to_ascii_lowercase(), offset)
        } else {
            strrpos_ascii(hay, nee, offset)
        };
        if found >= 0 {
            return Ok(OrFalse::Value(found));
        }
        if ascii(nee, nee.len()) {
            return Ok(OrFalse::False);
        }
    }
    let find = Find {
        haystack: hay,
        needle: nee,
        offset,
        ignore_case,
        last: true,
        locale: locale.0.as_bytes(),
    };
    strpos_utf16(req, &find).map(position)
}

/// `grapheme_substr_ascii`: the bytes `from` and `len` select, clamped.
fn substr_ascii(s: &[u8], from: i32, len: i32) -> &[u8] {
    let total = i32::try_from(s.len()).unwrap_or(i32::MAX);
    let f = if from < 0 {
        (total + from).max(0)
    } else {
        from.min(total)
    };
    let l = if len < 0 {
        ((total - f) + len).max(0)
    } else {
        len.min(total - f)
    };
    let (f, l) = (
        usize::try_from(f).unwrap_or(0),
        usize::try_from(l).unwrap_or(0),
    );
    s.get(f..f + l).unwrap_or(&[])
}

/// `grapheme_substr(string $string, int $offset, ?int $length = null, string $locale = ""): string|false`.
///
/// ```php
/// var_dump(grapheme_substr("abcdef", -3, 2), grapheme_substr("e\u{301}fg", 1), grapheme_substr("e\u{301}fg", 0, -1) === "e\u{301}f", grapheme_substr("e\u{301}fg", 5), grapheme_substr("e\u{301}fg", -9, 1) === "e\u{301}");
/// try { grapheme_substr("a", PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(2) "de"
/// string(2) "fg"
/// bool(true)
/// string(0) ""
/// bool(true)
/// grapheme_substr(): Argument #2 ($offset) is too large
/// ```
#[php_function]
fn grapheme_substr(
    req: &mut Request,
    string: PhpString,
    offset: i64,
    #[php(default = null)] length: Option<i64>,
    #[php(default = "")] locale: Path,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let _ = locale;
    let text = string.as_bytes();
    let Ok(mut start) = i32::try_from(offset) else {
        return Err(argument_error(req, ce.value_error, 2, "is too large"));
    };
    let Ok(mut length) = i32::try_from(length.unwrap_or(len_of(text))) else {
        return Err(argument_error(req, ce.value_error, 3, "is too large"));
    };
    if ascii(text, text.len()) {
        if i32::try_from(text.len()).is_err() {
            fail(req, Code::ILLEGAL_ARGUMENT, "invalid parameters")?;
            return Ok(OrFalse::False);
        }
        return bytes(req, substr_ascii(text, start, length));
    }
    let Some(units) = utf16(req, text, "Error converting input string to UTF-16")? else {
        return Ok(OrFalse::False);
    };
    let Ok(mut breaks) = Breaks::utf16(&units) else {
        return Ok(OrFalse::False);
    };
    let step = |breaks: &mut Breaks<'_>, backward: bool| {
        if backward {
            breaks.previous()
        } else {
            breaks.next()
        }
    };
    let backward = start < 0;
    if backward {
        breaks.last();
    }
    let mut from = 0;
    while start != 0 {
        from = step(&mut breaks, backward);
        if from == DONE {
            break;
        }
        start += if backward { 1 } else { -1 };
    }
    if start != 0 {
        if start > 0 {
            return bytes(req, b"");
        }
        from = 0;
        breaks.first();
    }
    let from_unit = usize::try_from(from).unwrap_or(0);
    if i64::from(length) >= i64::from(i32::try_from(text.len()).unwrap_or(i32::MAX)) {
        return utf8_out(req, units.get(from_unit..).unwrap_or(&[]));
    }
    if length == 0 {
        return bytes(req, b"");
    }
    let backward = length < 0;
    if backward {
        breaks.last();
    }
    let mut to = 0;
    while length != 0 {
        to = step(&mut breaks, backward);
        if to == DONE {
            break;
        }
        length += if backward { 1 } else { -1 };
    }
    drop(breaks);
    if to == DONE {
        if length < 0 {
            return bytes(req, b"");
        }
        to = ffi::length(units.len()).unwrap_or(i32::MAX);
    }
    if from > to {
        return bytes(req, b"");
    }
    let to_unit = usize::try_from(to).unwrap_or(0);
    utf8_out(req, units.get(from_unit..to_unit).unwrap_or(&[]))
}

/// `grapheme_strstr(string $haystack, string $needle, bool $beforeNeedle = false, string $locale = ""): string|false`.
///
/// ```php
/// var_dump(grapheme_strstr("user@example", "@"), grapheme_strstr("user@example", "@", true), grapheme_strstr("ééé@x", "@"), grapheme_strstr("abc", "x"));
/// ```
/// ```expect
/// string(8) "@example"
/// string(4) "user"
/// string(2) "@x"
/// bool(false)
/// ```
///
/// A haystack and needle both ASCII are searched by bytes; otherwise
/// the search is canonical, so `e` and a combining acute match `é`, and
/// the cut steps the UTF-8 haystack by the code points before the match,
/// a supplementary one counted once (php 8.5.12).
///
/// ```php
/// echo json_encode([grapheme_strstr("pe\u{301}x", "\u{E9}"), grapheme_strstr("pe\u{301}x", "\u{E9}", true), grapheme_strstr("😀abc", "a"), grapheme_stristr("😀e\u{301}abc", "A", true)]), "\n";
/// ```
/// ```expect
/// ["e\u0301x","p","abc","\ud83d\ude00e\u0301"]
/// ```
#[php_function]
#[allow(non_snake_case)]
fn grapheme_strstr(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = false)] beforeNeedle: bool,
    #[php(default = "")] locale: Path,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    strstr(req, haystack, needle, beforeNeedle, false, locale)
}

/// `grapheme_stristr(string $haystack, string $needle, bool $beforeNeedle = false, string $locale = ""): string|false`.
///
/// ```php
/// var_dump(grapheme_stristr("ABCdef", "cD"), grapheme_stristr("aÉb", "é", true), grapheme_stristr("abc", "x"));
/// ```
/// ```expect
/// string(4) "Cdef"
/// string(1) "a"
/// bool(false)
/// ```
#[php_function]
#[allow(non_snake_case)]
fn grapheme_stristr(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = false)] beforeNeedle: bool,
    #[php(default = "")] locale: Path,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    strstr(req, haystack, needle, beforeNeedle, true, locale)
}

/// `strstr_common_handler`.
fn strstr(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    before: bool,
    ignore_case: bool,
    locale: Path,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let (hay, nee) = (haystack.as_bytes(), needle.as_bytes());
    if !ignore_case && ascii(hay, hay.len()) && ascii(nee, nee.len()) {
        let Some(found) = memnstr(hay, nee, 0) else {
            return Ok(OrFalse::False);
        };
        return bytes(req, if before { &hay[..found] } else { &hay[found..] });
    }
    let find = Find {
        haystack: hay,
        needle: nee,
        offset: 0,
        ignore_case,
        last: false,
        locale: locale.0.as_bytes(),
    };
    let Some(found) = strpos_utf16(req, &find)? else {
        return Ok(OrFalse::False);
    };
    // `U8_FWD_N`: one code point at a time, the match's `puchar_pos`.
    let mut at = 0;
    for _ in 0..found.chars {
        if at >= hay.len() {
            break;
        }
        utf8::forward(hay, &mut at);
    }
    let at = at.min(hay.len());
    bytes(req, if before { &hay[..at] } else { &hay[at..] })
}

/// `grapheme_str_split(string $string, int $length = 1): array|false`.
///
/// ```php
/// var_dump(grapheme_str_split("ab\u{301}c", 2) === ["ab\u{301}", "c"], grapheme_str_split(""), count(grapheme_str_split("a\r\nb")));
/// try { grapheme_str_split("a", 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// array(0) {
/// }
/// int(3)
/// grapheme_str_split(): Argument #2 ($length) must be greater than 0 and less than or equal to 1073741823
/// ```
///
/// Since 8.5.12 a chunk the memory limit was met in stops the split, and
/// the limit's fatal is raised as the call returns:
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(count(grapheme_str_split(str_repeat("a", 4 << 20), 1 << 20)), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 3
/// ```
#[php_function]
fn grapheme_str_split(
    req: &mut Request,
    string: PhpString,
    #[php(default = 1)] length: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    if length <= 0 || length > QUARTER {
        return Err(arg_value_error(
            req,
            2,
            &format!("be greater than 0 and less than or equal to {QUARTER}"),
        ));
    }
    let text = string.as_bytes();
    let mut parts = PhpArray::new(req)?;
    if text.is_empty() {
        return Ok(OrFalse::Value(parts));
    }
    let mut breaks = match Breaks::utf8(text) {
        Ok(breaks) => breaks,
        Err(code) => {
            fail(req, code, "Error opening UTF-8 text")?;
            return Ok(OrFalse::False);
        }
    };
    let slice = |from: i64, len: i64| {
        let from = usize::try_from(from).unwrap_or(0);
        text.get(from..from + usize::try_from(len).unwrap_or(0))
            .unwrap_or(&[])
    };
    let (mut pos, mut start, mut end, mut i, mut current, mut end_len) =
        (0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
    while pos != i64::from(DONE) {
        end_len = pos - current;
        pos = i64::from(breaks.next());
        if i == length - 1 {
            if pos != i64::from(DONE) {
                let chunk = stringl(req, slice(start, pos - current))?;
                parts.append(req, chunk.into_value())?;
                // Since 8.5.12 a chunk the memory limit was met in stops
                // the split.
                if req.memory().has_oom() {
                    parts.give_up(req);
                    // RETURN_OOM()
                    return Ok(OrFalse::False);
                }
                end = start + pos - current;
                i = 0;
                start += pos - current;
                current = pos;
            }
        } else {
            i += 1;
        }
    }
    if i != 0 && end_len != 0 {
        let chunk = stringl(req, slice(end, end_len))?;
        parts.append(req, chunk.into_value())?;
    }
    drop(breaks);
    Ok(OrFalse::Value(parts))
}

/// `grapheme_levenshtein(string $string1, string $string2, int $insertion_cost = 1, int $replacement_cost = 1, int $deletion_cost = 1, string $locale = ""): int|false`.
///
/// ```php
/// var_dump(grapheme_levenshtein("kitten", "sitting"), grapheme_levenshtein("e\u{301}a", "\u{E9}b"), grapheme_levenshtein("", "abc", 2), grapheme_levenshtein("abc", "", 1, 1, 3));
/// try { grapheme_levenshtein("a", "b", 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// int(1)
/// int(6)
/// int(9)
/// grapheme_levenshtein(): Argument #3 ($insertion_cost) must be greater than 0 and less than or equal to 1073741823
/// ```
///
/// Since 8.5.12 the two rows are asked for fallibly, so rows past the
/// memory limit are refused and the limit's fatal is raised as the call
/// returns, the distance never computed:
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(grapheme_levenshtein("a", str_repeat("b", 1 << 20), 1, 2, 3), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 8404992 bytes) in %s on line 3
/// ```
///
/// and a UTF-16 copy the limit was met in stops the native before any
/// grapheme is compared:
///
/// ```php
/// ini_set('memory_limit', '8M');
/// $size = (int) (2.5 * 1024 * 1024);
/// var_dump(grapheme_levenshtein(str_repeat("a", $size), str_repeat("b", $size), 1, 2, 3), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 5259264 bytes) in %s on line 4
/// ```
#[php_function]
fn grapheme_levenshtein(
    req: &mut Request,
    string1: PhpString,
    string2: PhpString,
    #[php(default = 1)] insertion_cost: i64,
    #[php(default = 1)] replacement_cost: i64,
    #[php(default = 1)] deletion_cost: i64,
    #[php(default = "")] locale: Path,
) -> Result<OrFalse<i64>, Thrown> {
    let (ins, rep, del) = (insertion_cost, replacement_cost, deletion_cost);
    for (position, cost) in [(3, ins), (4, rep), (5, del)] {
        if cost <= 0 || cost > QUARTER {
            return Err(arg_value_error(
                req,
                position,
                &format!("be greater than 0 and less than or equal to {QUARTER}"),
            ));
        }
    }
    let (mut a, mut b) = (string1.as_bytes(), string2.as_bytes());
    if a.len() < b.len() && ins == rep && rep == del {
        (a, b) = (b, a);
    }
    let message = "Error converting input string to UTF-16";
    // `intl_convert_utf8_to_utf16` into `eumalloc`'s block, then, since
    // 8.5.12, `ZEND_HAS_OOM()`: a copy the memory limit was met in stops
    // the native (`RETURN_OOM()`'s value), before any row is asked for.
    let a = match ffi::to_utf16_block(req, a)? {
        Ok(a) if req.memory().has_oom() => {
            a.0.give_back(req);
            // RETURN_OOM()
            return Ok(OrFalse::False);
        }
        Ok(a) => a,
        Err(code) => {
            fail(req, code, message)?;
            return Ok(OrFalse::False);
        }
    };
    let b = match ffi::to_utf16_block(req, b)? {
        Ok(b) if req.memory().has_oom() => {
            b.0.give_back(req);
            a.0.give_back(req);
            // RETURN_OOM()
            return Ok(OrFalse::False);
        }
        Ok(b) => b,
        Err(code) => {
            a.0.give_back(req);
            fail(req, code, message)?;
            return Ok(OrFalse::False);
        }
    };
    let answered = {
        let (a_text, b_text) = (&a.0.as_slice()[..a.1], &b.0.as_slice()[..b.1]);
        distance(req, a_text, b_text, (ins, rep, del), locale.0.as_bytes())
    };
    b.0.give_back(req);
    a.0.give_back(req);
    answered
}

/// The distance `grapheme_levenshtein` answers over the two UTF-16
/// copies, as php walks them: two rows of `strlen_2 + 1` costs, asked
/// for fallibly since 8.5.12 (`safe_femalloc`), and each grapheme of one
/// compared with each of the other by the locale's collator, in place.
fn distance(
    req: &mut Request,
    a: &[u16],
    b: &[u16],
    (ins, rep, del): (i64, i64, i64),
    locale: &[u8],
) -> Result<OrFalse<i64>, Thrown> {
    let (len1, len2) = (count_graphemes(a), count_graphemes(b));
    if len1 < 0 || len2 < 0 {
        return Ok(OrFalse::False);
    }
    if len1 == 0 {
        return Ok(OrFalse::Value(len2 * ins));
    }
    if len2 == 0 {
        return Ok(OrFalse::Value(len1 * del));
    }
    let (Ok(mut bi1), Ok(mut bi2)) = (Breaks::utf16(a), Breaks::utf16(b)) else {
        return Ok(OrFalse::False);
    };
    let mut status = Code::ZERO;
    let opened = Collator::open(&ffi::c_string(locale), &mut status);
    let Some(collator) = opened else {
        fail(req, status, "Error on ucol_open")?;
        return Ok(OrFalse::False);
    };
    let width = usize::try_from(len2).unwrap_or(0) + 1;
    let Some(mut p1) = Block::<i64>::ask(req, width, true)? else {
        // RETURN_OOM()
        return Ok(OrFalse::False);
    };
    let Some(mut p2) = Block::<i64>::ask(req, width, true)? else {
        p1.give_back(req);
        // RETURN_OOM()
        return Ok(OrFalse::False);
    };
    for (i, cost) in p1.as_mut_slice().iter_mut().enumerate() {
        *cost = len_of_index(i) * ins;
    }
    loop {
        let current1 = bi1.current();
        let pos1 = bi1.next();
        if pos1 == DONE {
            break;
        }
        let g1 = unit(a, current1, pos1);
        let (row1, row2) = (p1.as_slice(), p2.as_mut_slice());
        row2[0] = row1[0] + del;
        let mut i2 = 0;
        loop {
            let current2 = bi2.current();
            let pos2 = bi2.next();
            if pos2 == DONE {
                break;
            }
            let g2 = unit(b, current2, pos2);
            let mut c0 = row1[i2]
                + if collator.strcoll(g1, g2) == 0 {
                    0
                } else {
                    rep
                };
            c0 = c0.min(row1[i2 + 1] + del).min(row2[i2] + ins);
            row2[i2 + 1] = c0;
            i2 += 1;
        }
        bi2.first();
        std::mem::swap(&mut p1, &mut p2);
    }
    let distance = p1.as_slice()[width - 1];
    p2.give_back(req);
    p1.give_back(req);
    Ok(OrFalse::Value(distance))
}

/// `add_next_index_stringl`'s string, `ZVAL_STRINGL`: `zend_string_init`,
/// the request's own whatever its length, one byte among them.
fn stringl(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    PhpString::filled(req, bytes.len(), |b| b.copy_from_slice(bytes))
}

/// The units of one grapheme, between two of the iterator's breaks.
fn unit(text: &[u16], from: i32, to: i32) -> &[u16] {
    let (from, to) = (
        usize::try_from(from).unwrap_or(0),
        usize::try_from(to).unwrap_or(0),
    );
    text.get(from..to).unwrap_or(&[])
}

/// An index as a count.
fn len_of_index(i: usize) -> i64 {
    i64::try_from(i).unwrap_or(i64::MAX)
}

/// `grapheme_extract(string $haystack, int $size, int $type = GRAPHEME_EXTR_COUNT, int $offset = 0, &$next = null): string|false`.
///
/// ```php
/// var_dump(grapheme_extract("abcdef", 2, GRAPHEME_EXTR_COUNT, 1, $next), $next);
/// var_dump(bin2hex(grapheme_extract("e\u{301}f\u{301}", 1, GRAPHEME_EXTR_COUNT, 0, $next)), $next);
/// var_dump(bin2hex(grapheme_extract("e\u{301}f\u{301}", 4, GRAPHEME_EXTR_MAXBYTES)), bin2hex(grapheme_extract("e\u{301}f\u{301}", 3, GRAPHEME_EXTR_MAXCHARS)));
/// var_dump(grapheme_extract("abc", 1, 0, 3), intl_get_error_message());
/// try { grapheme_extract("abc", 1, 7); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(2) "bc"
/// int(3)
/// string(6) "65cc81"
/// int(3)
/// string(6) "65cc81"
/// string(6) "65cc81"
/// bool(false)
/// string(75) "grapheme_extract(): start not contained in string: U_ILLEGAL_ARGUMENT_ERROR"
/// grapheme_extract(): Argument #3 ($type) must be one of GRAPHEME_EXTR_COUNT, GRAPHEME_EXTR_MAXBYTES, or GRAPHEME_EXTR_MAXCHARS
/// ```
#[php_function]
fn grapheme_extract(
    req: &mut Request,
    haystack: PhpString,
    size: i64,
    #[php(default = GRAPHEME_EXTR_COUNT)] r#type: i64,
    #[php(default = 0)] offset: i64,
    #[php(default = null)] next: Ref<Untyped<Value>>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let text = haystack.as_bytes();
    let mut start = offset;
    if start < 0 {
        start += len_of(text);
    }
    let tell = |req: &mut Request, at: i64| -> Result<(), Thrown> {
        if next.is_given() {
            next.set(req, Untyped(Value::int(at)))?;
        }
        Ok(())
    };
    tell(req, start)?;
    if !(0..=2).contains(&r#type) {
        return Err(arg_value_error(
            req,
            3,
            "be one of GRAPHEME_EXTR_COUNT, GRAPHEME_EXTR_MAXBYTES, or GRAPHEME_EXTR_MAXCHARS",
        ));
    }
    if start > i64::from(i32::MAX) || start < 0 || start >= len_of(text) {
        fail(req, Code::ILLEGAL_ARGUMENT, "start not contained in string")?;
        return Ok(OrFalse::False);
    }
    if size < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    if size > i64::from(i32::MAX) {
        return Err(argument_error(req, ce.value_error, 2, "is too large"));
    }
    if size == 0 {
        return bytes(req, b"");
    }
    let mut at = usize::try_from(start).unwrap_or(0);
    while !utf8::is_single(text[at]) && !utf8::is_lead(text[at]) {
        at += 1;
        if at >= text.len() {
            let message = "grapheme_extract: invalid input string";
            fail(req, Code::ILLEGAL_ARGUMENT, message)?;
            return Ok(OrFalse::False);
        }
    }
    let rest = &text[at..];
    let size_n = usize::try_from(size).unwrap_or(usize::MAX);
    if ascii(rest, rest.len().min(size_n.saturating_add(1))) {
        let n = size_n.min(rest.len());
        tell(req, len_of(&text[..at + n]))?;
        return bytes(req, &rest[..n]);
    }
    let mut breaks = match Breaks::utf8(rest) {
        Ok(breaks) => breaks,
        Err(code) => {
            fail(req, code, "Error opening UTF-8 text")?;
            return Ok(OrFalse::False);
        }
    };
    let end = match r#type {
        0 => extract_count(&mut breaks, size),
        1 => extract_bytes(&mut breaks, size),
        _ => extract_chars(&mut breaks, size, rest),
    };
    drop(breaks);
    let end = usize::try_from(end).unwrap_or(0).min(rest.len());
    tell(req, len_of(&text[..at + end]))?;
    bytes(req, &rest[..end])
}

/// `grapheme_extract_count_iter`.
fn extract_count(breaks: &mut Breaks<'_>, mut size: i64) -> i32 {
    let mut end = 0;
    while size != 0 {
        let next = breaks.next();
        if next == DONE {
            break;
        }
        end = next;
        size -= 1;
    }
    end
}

/// `grapheme_extract_bytecount_iter`.
fn extract_bytes(breaks: &mut Breaks<'_>, size: i64) -> i32 {
    let mut end = 0;
    loop {
        let pos = breaks.next();
        if pos == DONE || i64::from(pos) > size {
            break end;
        }
        end = pos;
    }
}

/// `grapheme_extract_charcount_iter`.
fn extract_chars(breaks: &mut Breaks<'_>, mut size: i64, text: &[u8]) -> i32 {
    let mut end = 0usize;
    let mut count = 0;
    loop {
        let pos = breaks.next();
        if pos == DONE {
            break;
        }
        let pos = usize::try_from(pos).unwrap_or(0);
        let mut at = end;
        while at < pos {
            count += 1;
            let before = at;
            utf8::forward(text, &mut at);
            if before == at {
                size = 0;
                break;
            }
        }
        if count > size {
            break;
        }
        end = at;
    }
    i32::try_from(end).unwrap_or(i32::MAX)
}
