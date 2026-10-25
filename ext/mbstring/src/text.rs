//! `mbstring.c`'s character-wise natives over one encoding: length,
//! search, substrings, width, splitting, trimming, padding, `mb_ord`,
//! `mb_chr` and `mb_scrub`. Each decodes with the encoding's own
//! `to_wchar`, 128 codepoints at a time as php's loops do, and a search
//! runs over UTF-8, converted with libmbfl's own error marker so no
//! marker ever matches text.

use crate::case::{self, CaseMode};
use crate::convert::convert;
use crate::ffi::mbfl::{Converted, Encoder, Encoding, fast_convert, fast_convert_held, strcut};
use crate::settings::{Substitute, encoding, settings};
use crate::tables::{
    ENCODING_2022JPMS, ENCODING_CP50220, ENCODING_CP50222, ENCODING_INVALID, ENCODING_JIS,
    ENCODING_QPRINT, ENCODING_SJIS_MAC, ENCODING_UTF7, ENCODING_UTF7IMAP, ENCODING_UTF8,
    ENCODING_UTF8_SB, FIRST_DOUBLEWIDTH_CODEPOINT, MBFL_BAD_INPUT, MBFL_EAW_TABLE,
    MBFL_ENCTYPE_SBCS, MBFL_ENCTYPE_WCS2, MBFL_ENCTYPE_WCS4,
    MBFL_OUTPUTFILTER_ILLEGAL_MODE_BADUTF8,
};
use php_ext::abi::OrFalse;
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, argument_error, docref, throw_error};
use php_ext::request::Request;
use php_ext::values::{Layout, Owner, PhpArray, PhpString};

/// `MBFL_SUBSTR_UNTIL_END`.
pub(crate) const UNTIL_END: usize = usize::MAX;

/// The bytes a fixed-width encoding spends a codepoint, `SBCS`, `WCS2`
/// or `WCS4`; zero for the rest.
pub(crate) fn char_len(enc: Encoding) -> usize {
    (enc.flag() & (MBFL_ENCTYPE_SBCS | MBFL_ENCTYPE_WCS2 | MBFL_ENCTYPE_WCS4)) as usize
}

/// `php_mb_is_no_encoding_utf8`: UTF-8 and its mobile variants.
pub(crate) fn is_utf8(enc: Encoding) -> bool {
    (ENCODING_UTF8..=ENCODING_UTF8_SB).contains(&enc.no())
}

/// `php_mb_is_unsupported_no_encoding`: what `mb_ord` and `mb_chr` refuse.
fn is_unsupported(enc: Encoding) -> bool {
    let no = enc.no();
    (ENCODING_INVALID..=ENCODING_QPRINT).contains(&no)
        || (ENCODING_UTF7..=ENCODING_UTF7IMAP).contains(&no)
        || (ENCODING_JIS..=ENCODING_2022JPMS).contains(&no)
        || (ENCODING_CP50220..=ENCODING_CP50222).contains(&no)
}

/// Every codepoint `input` decodes to, as `to_wchar` hands them over in
/// batches of `size`, each batch to `each`, which stops the walk by answering
/// `false`.
pub(crate) fn decode_runs(
    enc: Encoding,
    mut input: &[u8],
    size: usize,
    mut each: impl FnMut(&[u32], &[u8]) -> bool,
) {
    let mut buf = [0u32; 128];
    let size = size.min(buf.len());
    let mut state = 0;
    while !input.is_empty() {
        let n = enc.decode(&mut input, &mut buf[..size], &mut state);
        if !each(&buf[..n], input) {
            return;
        }
    }
}

/// `mb_fast_strlen_utf8`: the bytes less the ones after a lead byte.
fn fast_strlen_utf8(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b.cast_signed() >= -64).count()
}

/// `mb_get_strlen`.
pub(crate) fn strlen(string: PhpString, enc: Encoding) -> usize {
    let bytes = string.as_bytes();
    if let Some(n) = bytes.len().checked_div(char_len(enc)) {
        return n;
    }
    if is_utf8(enc) && (string.is_valid_utf8() || std::str::from_utf8(bytes).is_ok()) {
        return fast_strlen_utf8(bytes);
    }
    count(bytes, enc)
}

/// `mb_get_strlen` over bytes that carry no validity mark.
pub(crate) fn count(bytes: &[u8], enc: Encoding) -> usize {
    let mut len = 0;
    decode_runs(enc, bytes, 128, |w, _| {
        len += w.len();
        true
    });
    len
}

/// `mb_get_substr_slow`: the codepoints from `from`, `len` of them,
/// encoded back.
pub(crate) fn substr_slow(
    input: &[u8],
    from: usize,
    len: usize,
    enc: Encoding,
    sub: Substitute,
) -> Vec<u8> {
    let mut out = Encoder::new(
        enc,
        len.min(input.len().saturating_sub(from)),
        sub.char,
        sub.raw_mode(),
    );
    let (mut from, mut len) = (from, len);
    decode_runs(enc, input, 128, |w, rest| {
        if from >= w.len() {
            from -= w.len();
        } else {
            let needed = (w.len() - from).min(len);
            out.write(&w[from..from + needed], rest.is_empty() || w.len() >= len);
            from = 0;
            len -= needed;
        }
        len > 0
    });
    out.into_vec()
}

/// What `mb_get_substr` answers, as php makes it: bytes cut from the
/// input, `zend_empty_string` or `zend_string_init_fast`, so nothing and
/// one byte are the process's interned strings; or the codepoints
/// decoded and encoded back into the string `mb_convert_buf` allocates.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Substr {
    Cut(Vec<u8>),
    Encoded(Vec<u8>),
}

impl Substr {
    /// The bytes, whichever way php made them.
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        match self {
            Substr::Cut(b) | Substr::Encoded(b) => b,
        }
    }

    /// The string php hands out: a cut through [`cut_string`], an
    /// encoding through [`new_string`].
    pub(crate) fn into_string(self, req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
        match self {
            Substr::Cut(b) => cut_string(req, &b),
            Substr::Encoded(b) => new_string(req, &b),
        }
    }
}

/// `mb_get_substr`: a fixed-width encoding cut by bytes, the rest
/// decoded.
pub(crate) fn substr(
    input: &[u8],
    from: usize,
    len: usize,
    enc: Encoding,
    sub: Substitute,
) -> Substr {
    if len == 0 || (from >= input.len() && enc.no() != ENCODING_SJIS_MAC) {
        return Substr::Cut(Vec::new());
    }
    let width = char_len(enc);
    if width != 0 {
        let from = from.saturating_mul(width);
        let len = len.saturating_mul(width);
        if from >= input.len() {
            return Substr::Cut(Vec::new());
        }
        let rest = &input[from..];
        return Substr::Cut(rest[..len.min(rest.len())].to_vec());
    }
    Substr::Encoded(substr_slow(input, from, len, enc, sub))
}

/// What a search can miss by, `MBFL_ERROR_NOT_FOUND` and
/// `MBFL_ERROR_OFFSET`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Missed {
    NotFound,
    Offset,
}

/// `offset_to_pointer_utf8`: the byte at codepoint `offset`, from the
/// end where it is negative.
fn utf8_position(s: &[u8], offset: isize) -> Option<usize> {
    if offset < 0 {
        let mut pos = s.len();
        let mut offset = offset;
        while offset < 0 {
            if pos == 0 {
                return None;
            }
            pos -= 1;
            let c = s[pos];
            if c < 0x80 || (c & 0xC0) != 0x80 {
                offset += 1;
            }
        }
        Some(pos)
    } else {
        let table = Encoding::utf8().mblen_table();
        let mut pos = 0;
        for _ in 0..offset {
            if pos >= s.len() {
                return None;
            }
            pos += table.map_or(1, |t| usize::from(t[usize::from(s[pos])]));
        }
        Some(pos.min(s.len()))
    }
}

/// `zend_memnstr`.
fn find_first(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `zend_memnrstr`.
fn find_last(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(haystack.len());
    }
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

/// UTF-8 as a search reads it, bad input as `0xFF`.
pub(crate) fn search_utf8(bytes: &[u8], enc: Encoding) -> Vec<u8> {
    fast_convert(
        bytes,
        enc,
        Encoding::utf8(),
        0,
        MBFL_OUTPUTFILTER_ILLEGAL_MODE_BADUTF8,
    )
    .0
}

/// `mb_find_strpos`: the codepoint at which `needle` is found.
pub(crate) fn find(
    haystack: &[u8],
    needle: &[u8],
    enc: Encoding,
    offset: isize,
    reverse: bool,
) -> Result<usize, Missed> {
    let (h, n);
    let (haystack_u8, needle_u8) = if is_utf8(enc) {
        (haystack, needle)
    } else {
        h = search_utf8(haystack, enc);
        n = search_utf8(needle, enc);
        (&h[..], &n[..])
    };
    find_utf8(haystack_u8, needle_u8, offset, reverse)
}

/// `mb_find_strpos` over its UTF-8: haystack and needle both as UTF-8,
/// the needle's codepoints what a backward search steps past.
fn find_utf8(
    haystack: &[u8],
    needle: &[u8],
    offset: isize,
    reverse: bool,
) -> Result<usize, Missed> {
    let start = utf8_position(haystack, offset).ok_or(Missed::Offset)?;
    if haystack.len() < needle.len() {
        return Err(Missed::NotFound);
    }
    let found = if !reverse {
        find_first(&haystack[start..], needle).map(|at| start + at)
    } else if offset >= 0 {
        find_last(&haystack[start..], needle).map(|at| start + at)
    } else {
        let needle_len = fast_strlen_utf8(needle);
        let end = utf8_position(
            &haystack[start..],
            isize::try_from(needle_len).unwrap_or(isize::MAX),
        )
        .map_or(haystack.len(), |at| start + at);
        find_last(&haystack[..end], needle)
    };
    found
        .map(|at| fast_strlen_utf8(&haystack[..at]))
        .ok_or(Missed::NotFound)
}

/// `php_mb_stripos`: both folded, simply, to UTF-8 and searched there.
pub(crate) fn find_folded(
    haystack: &[u8],
    needle: &[u8],
    enc: Encoding,
    offset: isize,
    reverse: bool,
) -> Result<usize, Missed> {
    let fold = |s: &[u8]| {
        case::convert(
            CaseMode::FoldSimple,
            s,
            enc,
            Encoding::utf8(),
            Substitute::BAD_UTF8,
        )
    };
    let (h, n) = (fold(haystack), fold(needle));
    find_utf8(&h, &n, offset, reverse)
}

/// `handle_strpos_error`, and `false`.
fn missed(req: &mut Request, missed: Missed) -> Result<OrFalse<i64>, Thrown> {
    match missed {
        Missed::NotFound => Ok(OrFalse::False),
        Missed::Offset => Err(value_error(
            req,
            3,
            "must be contained in argument #1 ($haystack)",
        )),
    }
}

fn found(req: &mut Request, at: Result<usize, Missed>) -> Result<OrFalse<i64>, Thrown> {
    match at {
        Ok(at) => Ok(OrFalse::Value(i64::try_from(at).unwrap_or(i64::MAX))),
        Err(m) => missed(req, m),
    }
}

fn offset(n: i64) -> isize {
    isize::try_from(n).unwrap_or(if n < 0 { isize::MIN } else { isize::MAX })
}

fn int(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `call` served with the request's heap lent to libmbfl (ADR-0014): what
/// libmbfl allocates while it runs, its conversion buffers and memory
/// devices, is the request's and counted as php counts it, so the memory
/// limit meets them as php's `zend_mm` meets php's, and since 8.5.12 a
/// buffer's growth past it is refused. `call` reaches nothing of the
/// request and gives back what libmbfl made before it returns; the `Err`
/// is the hard limit's end of the request.
pub(crate) fn on_heap<R>(req: &mut Request, call: impl FnOnce() -> R) -> Result<R, Thrown> {
    req.memory_mut().lend_heap(call)
}

/// A string of these bytes allocated as its own, `zend_string_init`,
/// `RETVAL_STRINGL` and what `mb_convert_buf` answers: nothing and one
/// byte are strings of the request's too, as every converter's are.
pub(crate) fn new_string(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    PhpString::filled(req, bytes.len(), |b| b.copy_from_slice(bytes))
}

/// A string of these bytes as `zend_string_init_fast` and
/// `ZVAL_STRINGL_FAST` make it ([`PhpString::fast`]): nothing and one
/// byte are the process's interned strings, what php's cuts and
/// `ZSTR_CHAR` answer, and a longer one php-8.5.12's
/// `zend_string_finit` (`Z/ext/mbstring/mbstring.c:2178`, `:2472`,
/// `:2495`, and the `cut` of `mbfilter_cjk.c`, `mbfilter_utf16.c` and
/// `mbfilter_utf8.c`).
pub(crate) fn cut_string(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    PhpString::fast(req, bytes)
}

/// An argument handed back as the return, one more owner taken.
pub(crate) fn same(req: &mut Request, s: PhpString) -> Owner<PhpString> {
    Owner::take(req, s)
}

/// The request's substitute character and its kind.
pub(crate) fn substitute(req: &mut Request) -> Substitute {
    settings(req).substitute
}

/// `mb_str_split(string $string, int $length = 1, ?string $encoding = null): array`.
///
/// ```php
/// var_dump(mb_str_split("añb€", 2), array_map("bin2hex", mb_str_split("\x00a\x00b\x00c", 2, "UTF-16BE")), array_map("bin2hex", mb_str_split("\x82\xa0\x82\xa2x", 1, "SJIS")), mb_str_split(""));
/// echo implode(",", array_map("bin2hex", mb_str_split("\x1b\$B\x30\x21\x30\x22\x1b(Bxy", 2, "JIS"))), "\n";
/// try { mb_str_split("a", 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(3) "añ"
///   [1]=>
///   string(4) "b€"
/// }
/// array(2) {
///   [0]=>
///   string(8) "00610062"
///   [1]=>
///   string(4) "0063"
/// }
/// array(3) {
///   [0]=>
///   string(4) "82a0"
///   [1]=>
///   string(4) "82a2"
///   [2]=>
///   string(2) "78"
/// }
/// array(0) {
/// }
/// 1b2442302130221b2842,7879
/// mb_str_split(): Argument #2 ($length) must be greater than 0
/// ```
///
/// Since 8.5.12 the table of every chunk is laid down first and fallibly, so
/// a split past the memory limit is refused there, and each chunk stops the
/// split once the limit was met; either way the limit's fatal is raised as
/// the call returns:
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(count(mb_str_split(str_repeat("a", 2 << 20))), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 33570816 bytes) in %s on line 3
/// ```
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(count(mb_str_split(str_repeat("a", 2 << 20), 1, "UTF-7")), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 33570816 bytes) in %s on line 3
/// ```
#[php_function]
fn mb_str_split(
    req: &mut Request,
    string: PhpString,
    #[php(default = 1)] length: i64,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpArray>, Thrown> {
    if length <= 0 {
        return Err(value_error(req, 2, "must be greater than 0"));
    }
    if length > i64::from(u32::MAX / 4) {
        return Err(value_error(req, 2, "is too large"));
    }
    let bytes = string.as_bytes();
    // Since 8.5.12 a length past the string's is its length.
    let split = usize::try_from(length).unwrap_or(1).min(bytes.len());
    let enc = self::encoding(req, encoding, 3)?;
    if string.is_empty() {
        return Ok(Owner::take(req, PhpArray::empty()));
    }
    // `array_finit_size_packed` since 8.5.12: the table for every chunk
    // laid down first and fallibly, `RETURN_OOM()` where the memory limit
    // refused it; then each chunk added, the split stopped once the
    // memory limit was met (`ZEND_HAS_OOM()`).
    let width = char_len(enc);
    let chunks = if width != 0 {
        (bytes.len() / (width * split)).div_ceil(split)
    } else {
        bytes.len().div_ceil(split)
    };
    let chunks = u32::try_from(chunks).unwrap_or(u32::MAX);
    let Some(mut out) = req
        .memory_mut()
        .fallible(|req| PhpArray::sized(req, chunks, Layout::Packed))?
    else {
        // RETURN_OOM()
        return Ok(Owner::take(req, PhpArray::empty()));
    };
    let add =
        |req: &mut Request, out: &mut Owner<PhpArray>, chunk: &[u8]| -> Result<bool, Thrown> {
            let s = new_string(req, chunk)?;
            out.append(req, s.into_value())?;
            Ok(req.memory().has_oom())
        };
    if width != 0 {
        for chunk in bytes.chunks(width * split) {
            if add(req, &mut out, chunk)? {
                return Ok(split_refused(req, out));
            }
        }
    } else if let Some(table) = enc.mblen_table() {
        let mut p = 0;
        while p < bytes.len() {
            let start = p;
            for _ in 0..split {
                if p >= bytes.len() {
                    break;
                }
                p += usize::from(table[usize::from(bytes[p])]);
            }
            p = p.min(bytes.len());
            if add(req, &mut out, &bytes[start..p])? {
                return Ok(split_refused(req, out));
            }
        }
    } else if split_filtered(req, &mut out, bytes, enc, split)? {
        return Ok(split_refused(req, out));
    }
    Ok(out)
}

/// `mb_str_split` through an encoding with neither a fixed width nor a
/// `mblen_table`: each chunk encoded afresh from 128 codepoints at a time,
/// a chunk left open carried into the next batch; `true` where the memory
/// limit was met and the split stopped.
fn split_filtered(
    req: &mut Request,
    out: &mut Owner<PhpArray>,
    bytes: &[u8],
    enc: Encoding,
    split: usize,
) -> Result<bool, Thrown> {
    let add = |req: &mut Request, out: &mut Owner<PhpArray>, chunk: &[u8]| -> Result<(), Thrown> {
        let s = new_string(req, chunk)?;
        out.append(req, s.into_value())
    };
    let sub = substitute(req);
    let (mut rest, mut state, mut wchars) = (bytes, 0, [0u32; 128]);
    let mut pending: Option<(Encoder, usize)> = None;
    while !rest.is_empty() {
        if req.memory().has_oom() {
            return Ok(true);
        }
        let n = enc.decode(&mut rest, &mut wchars, &mut state);
        let (w, end) = (&wchars[..n], rest.is_empty());
        let mut i = 0;
        if let Some((mut buf, count)) = pending.take() {
            if n >= split - count {
                let chunk = on_heap(req, || {
                    buf.write(&w[..split - count], true);
                    buf.into_vec()
                })?;
                i = split - count;
                add(req, out, &chunk)?;
            } else {
                on_heap(req, || buf.write(w, end))?;
                pending = Some((buf, count + n));
                continue;
            }
        }
        while i < n {
            if req.memory().has_oom() {
                return Ok(true);
            }
            let mut buf = on_heap(req, || Encoder::new(enc, split, sub.char, sub.raw_mode()))?;
            if n - i >= split {
                let chunk = on_heap(req, || {
                    buf.write(&w[i..i + split], true);
                    buf.into_vec()
                })?;
                i += split;
                add(req, out, &chunk)?;
            } else {
                on_heap(req, || buf.write(&w[i..], end))?;
                pending = Some((buf, n - i));
                break;
            }
        }
    }
    if let Some((buf, _)) = pending {
        let chunk = on_heap(req, || buf.into_vec())?;
        add(req, out, &chunk)?;
    }
    Ok(false)
}

/// `RETURN_OOM()` from `mb_str_split`: the chunks given up, and the
/// cheapest array, which the limit's fatal after the call leaves unseen.
fn split_refused(req: &mut Request, out: Owner<PhpArray>) -> Owner<PhpArray> {
    out.give_up(req);
    Owner::take(req, PhpArray::empty())
}

/// `mb_strlen(string $string, ?string $encoding = null): int`.
///
/// ```php
/// var_dump(mb_strlen("añb€"), mb_strlen("añb€", "8bit"), mb_strlen("\x00a\x00b", "UTF-16BE"), mb_strlen("\xff\xfea"), mb_strlen("\x1b\$B\x30\x21\x1b(B", "JIS"));
/// try { mb_strlen("a", "nope"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(4)
/// int(7)
/// int(2)
/// int(3)
/// int(1)
/// mb_strlen(): Argument #2 ($encoding) must be a valid encoding, "nope" given
/// ```
#[php_function]
fn mb_strlen(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<i64, Thrown> {
    let enc = self::encoding(req, encoding, 2)?;
    Ok(int(strlen(string, enc)))
}

/// `mb_strpos(string $haystack, string $needle, int $offset = 0, ?string $encoding = null): int|false`.
///
/// ```php
/// var_dump(mb_strpos("añbñc", "ñ"), mb_strpos("añbñc", "ñ", 2), mb_strpos("añbñc", "ñ", -2), mb_strpos("añb", "z"), mb_strpos("añb", ""), mb_strpos("\x00a\x00\xf1", "\x00\xf1", 0, "UTF-16BE"));
/// try { mb_strpos("abc", "a", 9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// int(3)
/// int(3)
/// bool(false)
/// int(0)
/// int(1)
/// mb_strpos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// ```
#[php_function]
fn mb_strpos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] offset: i64,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<i64>, Thrown> {
    let enc = self::encoding(req, encoding, 4)?;
    let at = on_heap(req, || {
        find(
            haystack.as_bytes(),
            needle.as_bytes(),
            enc,
            self::offset(offset),
            false,
        )
    })?;
    found(req, at)
}

/// `mb_strrpos(string $haystack, string $needle, int $offset = 0, ?string $encoding = null): int|false`.
///
/// ```php
/// var_dump(mb_strrpos("añbñc", "ñ"), mb_strrpos("añbñc", "ñ", -3), mb_strrpos("añbñc", "ñ", 4), mb_strrpos("añb", ""), mb_strrpos("añbñc", "ñc", -1));
/// try { mb_strrpos("abc", "a", -9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// int(1)
/// bool(false)
/// int(3)
/// int(3)
/// mb_strrpos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// ```
///
/// A negative offset steps back past the needle's codepoints as the
/// search's UTF-8 counts them, not its bytes in the encoding given
/// (GH-22779), so a one-byte needle in ISO-8859-1 or SJIS's two-byte
/// `あ` steps one, and UTF-16's two-byte `A` steps one.
///
/// ```php
/// var_dump(mb_strrpos("\xA9\xA9X", "\xA9", -2, 'ISO-8859-1'), mb_strrpos("\x82\xA0\x82\xA0A", "\x82\xA0", -2, 'SJIS'), mb_strrpos("\x00A\x00X\x00A", "\x00A", -2, 'UTF-16BE'));
/// ```
/// ```expect
/// int(1)
/// int(1)
/// int(0)
/// ```
#[php_function]
fn mb_strrpos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] offset: i64,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<i64>, Thrown> {
    let enc = self::encoding(req, encoding, 4)?;
    let at = on_heap(req, || {
        find(
            haystack.as_bytes(),
            needle.as_bytes(),
            enc,
            self::offset(offset),
            true,
        )
    })?;
    found(req, at)
}

/// `mb_stripos(string $haystack, string $needle, int $offset = 0, ?string $encoding = null): int|false`.
///
/// ```php
/// var_dump(mb_stripos("aÑbñc", "ñ"), mb_stripos("ΣΑΣ", "σ", 1), mb_stripos("ABC", "c"), mb_stripos("abc", "D"));
/// ```
/// ```expect
/// int(1)
/// int(2)
/// int(2)
/// bool(false)
/// ```
#[php_function]
fn mb_stripos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] offset: i64,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<i64>, Thrown> {
    let enc = self::encoding(req, encoding, 4)?;
    let at = on_heap(req, || {
        find_folded(
            haystack.as_bytes(),
            needle.as_bytes(),
            enc,
            self::offset(offset),
            false,
        )
    })?;
    found(req, at)
}

/// `mb_strripos(string $haystack, string $needle, int $offset = 0, ?string $encoding = null): int|false`.
///
/// ```php
/// var_dump(mb_strripos("aÑbñc", "ñ"), mb_strripos("aÑbñc", "Ñ", -3), mb_strripos("ABC", "x"));
/// ```
/// ```expect
/// int(3)
/// int(1)
/// bool(false)
/// ```
#[php_function]
fn mb_strripos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] offset: i64,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<i64>, Thrown> {
    let enc = self::encoding(req, encoding, 4)?;
    let at = on_heap(req, || {
        find_folded(
            haystack.as_bytes(),
            needle.as_bytes(),
            enc,
            self::offset(offset),
            true,
        )
    })?;
    found(req, at)
}

/// `php_mb_strstr_variants`: the part before or from the match.
fn strstr_variant(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    before_needle: bool,
    encoding: Option<PhpString>,
    reverse: bool,
    folded: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let enc = self::encoding(req, encoding, 4)?;
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    let at = on_heap(req, || {
        if folded {
            find_folded(h, n, enc, 0, reverse)
        } else {
            find(h, n, enc, 0, reverse)
        }
    })?;
    let Ok(at) = at else {
        return Ok(OrFalse::False);
    };
    let sub = substitute(req);
    let part = on_heap(req, || {
        if before_needle {
            substr(h, 0, at, enc, sub)
        } else {
            substr(h, at, UNTIL_END, enc, sub)
        }
    })?;
    Ok(OrFalse::Value(part.into_string(req)?))
}

/// `mb_strstr(string $haystack, string $needle, bool $before_needle = false, ?string $encoding = null): string|false`.
///
/// ```php
/// var_dump(mb_strstr("añbñc", "ñ"), mb_strstr("añbñc", "ñ", true), mb_strstr("añb", "z"), mb_strstr("abc", ""));
/// ```
/// ```expect
/// string(6) "ñbñc"
/// string(1) "a"
/// bool(false)
/// string(3) "abc"
/// ```
#[php_function]
fn mb_strstr(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = false)] before_needle: bool,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    strstr_variant(req, haystack, needle, before_needle, encoding, false, false)
}

/// `mb_strrchr(string $haystack, string $needle, bool $before_needle = false, ?string $encoding = null): string|false`.
///
/// ```php
/// var_dump(mb_strrchr("añbñc", "ñ"), mb_strrchr("añbñc", "ñ", true), mb_strrchr("añb", "z"));
/// ```
/// ```expect
/// string(3) "ñc"
/// string(4) "añb"
/// bool(false)
/// ```
#[php_function]
fn mb_strrchr(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = false)] before_needle: bool,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    strstr_variant(req, haystack, needle, before_needle, encoding, true, false)
}

/// `mb_stristr(string $haystack, string $needle, bool $before_needle = false, ?string $encoding = null): string|false`.
///
/// ```php
/// var_dump(mb_stristr("aÑbñc", "ñ"), mb_stristr("aÑbñc", "ñ", true), mb_stristr("abc", "Z"));
/// ```
/// ```expect
/// string(6) "Ñbñc"
/// string(1) "a"
/// bool(false)
/// ```
#[php_function]
fn mb_stristr(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = false)] before_needle: bool,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    strstr_variant(req, haystack, needle, before_needle, encoding, false, true)
}

/// `mb_strrichr(string $haystack, string $needle, bool $before_needle = false, ?string $encoding = null): string|false`.
///
/// ```php
/// var_dump(mb_strrichr("aÑbñc", "Ñ"), mb_strrichr("aÑbñc", "Ñ", true), mb_strrichr("abc", "Z"));
/// ```
/// ```expect
/// string(3) "ñc"
/// string(4) "aÑb"
/// bool(false)
/// ```
#[php_function]
fn mb_strrichr(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = false)] before_needle: bool,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    strstr_variant(req, haystack, needle, before_needle, encoding, true, true)
}

/// `zend_argument_value_error`: the sentence as php words it after
/// `Argument #N ($name) `.
pub(crate) fn value_error(req: &mut Request, arg: u32, text: &str) -> Thrown {
    argument_error(req, "ValueError", arg, text)
}

/// `zend_argument_must_not_be_empty_error`.
pub(crate) fn must_not_be_empty(req: &mut Request, arg: u32) -> Thrown {
    value_error(req, arg, "must not be empty")
}

/// `mb_substr_count(string $haystack, string $needle, ?string $encoding = null): int`.
///
/// ```php
/// var_dump(mb_substr_count("ñañaña", "ña"), mb_substr_count("aaa", "aa"), mb_substr_count("\xffa\xffa", "\xff"), mb_substr_count("\x00a\x00a", "\x00a", "UTF-16BE"));
/// try { mb_substr_count("a", ""); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mb_substr_count("a", "\x1b(B", "JIS"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// int(1)
/// int(2)
/// int(2)
/// mb_substr_count(): Argument #2 ($needle) must not be empty
/// mb_substr_count(): Argument #2 ($needle) must not be empty
/// ```
///
/// Since 8.5.12 a conversion's buffer grows fallibly, so a needle whose UTF-8
/// is past the memory limit is refused its growth, and the limit's fatal is
/// raised as the call returns. The case crosses the limit where a new chunk
/// is asked for, whose figure holds in any layout (#3442):
///
/// ```php
/// ini_set('memory_limit', '2M');
/// var_dump(mb_substr_count("abc", str_repeat("\x80", 400000), "CP1252"), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 3
/// ```
#[php_function]
fn mb_substr_count(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<i64, Thrown> {
    if needle.is_empty() {
        return Err(must_not_be_empty(req, 2));
    }
    let enc = self::encoding(req, encoding, 3)?;
    // php's `haystack_u8` and `needle_u8`, held on the request's heap
    // while they are searched; the search itself is `zend_memnstr`'s.
    let counted = on_heap(req, || {
        let convert = |s: &PhpString| {
            if is_utf8(enc) && (s.is_valid_utf8() || std::str::from_utf8(s.as_bytes()).is_ok()) {
                None
            } else {
                Some(
                    fast_convert_held(
                        s.as_bytes(),
                        enc,
                        Encoding::utf8(),
                        0,
                        MBFL_OUTPUTFILTER_ILLEGAL_MODE_BADUTF8,
                    )
                    .0,
                )
            }
        };
        let (hc, nc) = (convert(&haystack), convert(&needle));
        let h = hc.as_ref().map_or(haystack.as_bytes(), Converted::bytes);
        let n = nc.as_ref().map_or(needle.as_bytes(), Converted::bytes);
        if !is_utf8(enc) && n.is_empty() {
            return None;
        }
        let mut count = 0;
        let mut at = 0;
        while h.len() >= n.len() && at + n.len() <= h.len() {
            match find_first(&h[at..], n) {
                Some(found) => {
                    at += found + n.len();
                    count += 1;
                }
                None => break,
            }
        }
        Some(count)
    })?;
    counted.ok_or_else(|| must_not_be_empty(req, 2))
}

/// `mb_substr(string $string, int $start, ?int $length = null, ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_substr("añbñc", 1, 3), mb_substr("añbñc", -2), mb_substr("añbñc", 1, -1), mb_substr("añb", 9), mb_substr("\x00a\x00b\x00c", 1, 1, "UTF-16BE") === "\x00b", mb_substr("\xffab", 0, 2));
/// try { mb_substr("a", PHP_INT_MIN); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(5) "ñbñ"
/// string(3) "ñc"
/// string(5) "ñbñ"
/// string(0) ""
/// bool(true)
/// string(2) "?a"
/// mb_substr(): Argument #2 ($start) must be between -9223372036854775807 and 9223372036854775807
/// ```
#[php_function]
fn mb_substr(
    req: &mut Request,
    string: PhpString,
    start: i64,
    #[php(default = null)] length: Option<i64>,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let range = format!("must be between {} and {}", i64::MIN + 1, i64::MAX);
    if start == i64::MIN {
        return Err(value_error(req, 2, &range));
    }
    if length == Some(i64::MIN) {
        return Err(value_error(req, 3, &range));
    }
    let enc = self::encoding(req, encoding, 4)?;
    let mblen = if start < 0 || length.is_some_and(|l| l < 0) {
        strlen(string, enc)
    } else {
        0
    };
    let from = if start >= 0 {
        usize::try_from(start).unwrap_or(usize::MAX)
    } else if let Some(back) = usize::try_from(start.unsigned_abs())
        .ok()
        .filter(|&b| b < mblen)
    {
        mblen - back
    } else {
        0
    };
    let len = match length {
        None => UNTIL_END,
        Some(l) if l >= 0 => usize::try_from(l).unwrap_or(usize::MAX),
        Some(l)
            if from < mblen
                && usize::try_from(l.unsigned_abs()).is_ok_and(|b| b < mblen - from) =>
        {
            mblen - from - usize::try_from(l.unsigned_abs()).unwrap_or(0)
        }
        Some(_) => 0,
    };
    let sub = substitute(req);
    on_heap(req, || substr(string.as_bytes(), from, len, enc, sub))?.into_string(req)
}

/// `mb_strcut(string $string, int $start, ?int $length = null, ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_strcut("añbñc", 2, 3), mb_strcut("añbñc", -3), mb_strcut("\x00a\x00b\x00c", 1, 3, "UTF-16BE") === "\x00a", bin2hex(mb_strcut("\x82\xa0\x82\xa2", 1, 3, "SJIS")), bin2hex(mb_strcut("\x1b\$B\x30\x21\x30\x22\x1b(B", 3, 4, "JIS")), mb_strcut("abc", 5));
/// ```
/// ```expect
/// string(3) "ñb"
/// string(3) "ñc"
/// bool(true)
/// string(4) "82a0"
/// string(0) ""
/// string(0) ""
/// ```
///
/// A cut of a stateful encoding takes whole characters, opened with the
/// escape that shifts to them and closed back to ASCII, and is empty
/// where the length cannot hold both escapes and a character, or where
/// it starts after the opening escape, as offset 3 does; a cut of a
/// fixed-width or multibyte encoding starts at the character its offset
/// falls in and keeps the whole characters that fit.
///
/// ```php
/// $jis = "\x1b\x24\x42\x30\x21\x30\x22\x1b\x28\x42";
/// foreach ([0, 3] as $start) {
///     foreach ([0, 1, 4, 7, 8, 9, 20, 40] as $length) {
///         echo $start, ' ', $length, ' [', bin2hex(mb_strcut($jis, $start, $length, 'JIS')), "]\n";
///     }
/// }
/// echo bin2hex(mb_strcut("\x00a\x00b\x00c", 1, 3, 'UTF-16BE')), "\n";
/// echo bin2hex(mb_strcut("\x82\xa0\x82\xa2", 1, 3, 'SJIS')), "\n";
/// echo bin2hex(mb_strcut('あいう', 1, 5, 'UTF-8')), "\n";
/// ```
/// ```expect
/// 0 0 []
/// 0 1 []
/// 0 4 []
/// 0 7 []
/// 0 8 [1b244230211b2842]
/// 0 9 [1b244230211b2842]
/// 0 20 [1b2442302130221b2842]
/// 0 40 [1b2442302130221b2842]
/// 3 0 []
/// 3 1 []
/// 3 4 []
/// 3 7 []
/// 3 8 []
/// 3 9 []
/// 3 20 []
/// 3 40 []
/// 0061
/// 82a0
/// e38182
/// ```
///
/// Since 8.5.12 the memory device of a cut through a filter grows fallibly,
/// so a cut past the memory limit is refused there and the limit's fatal is
/// raised as the call returns:
///
/// ```php
/// ini_set('memory_limit', '2M');
/// var_dump(strlen(mb_strcut(str_repeat("_", 500000), 0, 500000, "UTF-7")), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 3
/// ```
#[php_function]
fn mb_strcut(
    req: &mut Request,
    string: PhpString,
    start: i64,
    #[php(default = null)] length: Option<i64>,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let enc = self::encoding(req, encoding, 4)?;
    let bytes = string.as_bytes();
    let total = int(bytes.len());
    let mut len = length.unwrap_or(total);
    let mut from = start;
    if from < 0 {
        from = (total + from).max(0);
    }
    if len < 0 {
        len = ((total - from) + len).max(0);
    }
    if from > total || len == 0 {
        return cut_string(req, b"");
    }
    let (from, len) = (
        usize::try_from(from).unwrap_or(0),
        usize::try_from(len).unwrap_or(usize::MAX),
    );
    let out = if let Some(cut) = on_heap(req, || enc.cut(bytes, from, len))? {
        cut
    } else if char_len(enc) != 0 {
        let width = char_len(enc);
        let from = from & !(width - 1);
        let len = len.min(bytes.len() - from) & !(width - 1);
        bytes[from..from + len].to_vec()
    } else if let Some(table) = enc.mblen_table() {
        let step = |p: usize| usize::from(table[usize::from(bytes[p])]);
        let (mut p, mut m) = (0, 0);
        while p < from {
            m = step(p);
            p += m;
        }
        if p > from {
            p -= m;
        }
        let start = p;
        let end = if len >= bytes.len() - start {
            bytes.len()
        } else {
            let q = p + len;
            while p < q {
                m = step(p);
                p += m;
            }
            if p > q {
                p -= m;
            }
            p
        };
        bytes[start..end].to_vec()
    } else {
        // Since 8.5.12 a cut whose memory device was refused its growth
        // is `NULL`, and the native returns (`RETURN_OOM()`); the cut is
        // `RETVAL_FSTRINGL`, asked for fallibly.
        let Some(cut) = on_heap(req, || strcut(bytes, enc, from, len))? else {
            // RETURN_OOM()
            return cut_string(req, b"");
        };
        return match req
            .memory_mut()
            .fallible(|req| PhpString::finit(req, &cut))?
        {
            Some(made) => Ok(made),
            None => cut_string(req, b""),
        };
    };
    cut_string(req, &out)
}

/// `character_width`: two for an East Asian wide codepoint.
pub(crate) fn character_width(c: u32) -> usize {
    if c < FIRST_DOUBLEWIDTH_CODEPOINT {
        return 1;
    }
    let pairs = MBFL_EAW_TABLE.len() / 2;
    let (mut lo, mut hi) = (0, pairs);
    while lo < hi {
        let probe = usize::midpoint(lo, hi);
        if c < MBFL_EAW_TABLE[2 * probe] {
            hi = probe;
        } else if c > MBFL_EAW_TABLE[2 * probe + 1] {
            lo = probe + 1;
        } else {
            return 2;
        }
    }
    1
}

/// `mb_get_strwidth`.
pub(crate) fn strwidth(bytes: &[u8], enc: Encoding) -> usize {
    let mut width = 0;
    decode_runs(enc, bytes, 128, |w, _| {
        width += w.iter().map(|&c| character_width(c)).sum::<usize>();
        true
    });
    width
}

/// `mb_strwidth(string $string, ?string $encoding = null): int`.
///
/// ```php
/// var_dump(mb_strwidth("aä漢字"), mb_strwidth("\xff"), mb_strwidth("\x30\x42", "UTF-16BE"));
/// ```
/// ```expect
/// int(6)
/// int(1)
/// int(2)
/// ```
#[php_function]
fn mb_strwidth(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<i64, Thrown> {
    let enc = self::encoding(req, encoding, 2)?;
    Ok(int(strwidth(string.as_bytes(), enc)))
}

/// What `mb_trim_string` answers: the input or the marker itself,
/// `zend_string_copy`, or a part of the input made as [`Substr`] says.
enum Trimmed {
    Input,
    Marker,
    Part(Substr),
}

/// `mb_trim_string`: the codepoints from `from` that fit in `width`,
/// the marker appended where the string was cut.
fn trim_string(
    input: &[u8],
    marker: &[u8],
    enc: Encoding,
    from: usize,
    width: usize,
    sub: Substitute,
) -> Trimmed {
    let mut remaining = width;
    let mut to_skip = from;
    let mut bad = false;
    let mut too_wide = false;
    decode_runs(enc, input, 128, |w, _| {
        if w.len() <= to_skip {
            to_skip -= w.len();
            return true;
        }
        for &c in &w[to_skip..] {
            bad |= c == MBFL_BAD_INPUT;
            let cw = character_width(c);
            if remaining < cw {
                too_wide = true;
                return false;
            }
            remaining -= cw;
        }
        to_skip = 0;
        true
    });
    if !too_wide {
        return if bad {
            Trimmed::Part(Substr::Encoded(substr_slow(
                input, from, UNTIL_END, enc, sub,
            )))
        } else if from == 0 {
            Trimmed::Input
        } else {
            Trimmed::Part(substr(input, from, UNTIL_END, enc, sub))
        };
    }
    let marker_width = strwidth(marker, enc);
    if width <= marker_width {
        return Trimmed::Marker;
    }
    let mut width = width - marker_width;
    let mut out = Encoder::new(enc, width, sub.char, sub.raw_mode());
    let mut from = from;
    decode_runs(enc, input, 128, |w, _| {
        if w.len() <= from {
            from -= w.len();
            return true;
        }
        for (i, &c) in w.iter().enumerate().skip(from) {
            let cw = character_width(c);
            if width < cw {
                out.write(&w[from..i], true);
                return false;
            }
            width -= cw;
        }
        out.write(&w[from..], false);
        from = 0;
        true
    });
    out.append(marker);
    Trimmed::Part(Substr::Encoded(out.into_vec()))
}

/// `mb_strimwidth(string $string, int $start, int $width, string $trim_marker = "", ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_strimwidth("Hello World", 0, 10, "..."), mb_strimwidth("漢字漢字", 0, 5, "…"), mb_strimwidth("Hello", 1, 3), mb_strimwidth("Hello", -3, 9), mb_strimwidth("Hello World", 0, -3, "."));
/// try { mb_strimwidth("abc", 5, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Deprecated: mb_strimwidth(): passing a negative integer to argument #3 ($width) is deprecated in %s on line 2
/// string(10) "Hello W..."
/// string(9) "漢字…"
/// string(3) "ell"
/// string(3) "llo"
/// string(8) "Hello W."
/// mb_strimwidth(): Argument #2 ($start) is out of range
/// ```
///
/// Since 8.5.12 the trim marker's room is asked for fallibly, so a marker
/// past the memory limit is refused and the limit's fatal is raised as the
/// call returns:
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(strlen(mb_strimwidth(str_repeat("a", 3 << 20), 0, (3 << 20) - 1, str_repeat("b", 3 << 20))), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 3162112 bytes) in %s on line 3
/// ```
#[php_function]
fn mb_strimwidth(
    req: &mut Request,
    string: PhpString,
    start: i64,
    width: i64,
    #[php(default = "")] trim_marker: PhpString,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let enc = self::encoding(req, encoding, 5)?;
    let bytes = string.as_bytes();
    let mut from = start;
    if from != 0 {
        let len = int(strlen(string, enc));
        if from < 0 {
            from += len;
        }
        if from < 0 || from > len {
            return Err(value_error(req, 2, "is out of range"));
        }
    }
    let mut width = width;
    let sub = substitute(req);
    if width < 0 {
        docref(
            req,
            Level::Deprecated,
            "passing a negative integer to argument #3 ($width) is deprecated",
        );
        width += int(strwidth(bytes, enc));
        if from > 0 {
            let trimmed = on_heap(req, || {
                substr(bytes, 0, usize::try_from(from).unwrap_or(0), enc, sub)
            })?;
            width -= int(strwidth(&trimmed.into_bytes(), enc));
        }
        if width < 0 {
            return Err(value_error(req, 3, "is out of range"));
        }
    }
    let (from, width) = (
        usize::try_from(from).unwrap_or(0),
        usize::try_from(width).unwrap_or(0),
    );
    let marker = trim_marker.as_bytes();
    match on_heap(req, || trim_string(bytes, marker, enc, from, width, sub))? {
        Trimmed::Input => Ok(same(req, string)),
        Trimmed::Marker => Ok(same(req, trim_marker)),
        Trimmed::Part(part) => part.into_string(req),
    }
}

/// `php_mb_ulcfirst`: the first codepoint in `mode`, the rest as given.
fn ulcfirst(
    req: &mut Request,
    string: PhpString,
    encoding: Option<PhpString>,
    mode: CaseMode,
) -> Result<Owner<PhpString>, Thrown> {
    let enc = self::encoding(req, encoding, 2)?;
    let sub = substitute(req);
    let bytes = string.as_bytes();
    let (first, head) = on_heap(req, || {
        let first = substr(bytes, 0, 1, enc, sub).into_bytes();
        let head = case::convert(mode, &first, enc, enc, sub);
        (first, head)
    })?;
    if first == head {
        return Ok(same(req, string));
    }
    let mut out = head;
    out.extend(on_heap(req, || substr(bytes, 1, UNTIL_END, enc, sub))?.into_bytes());
    new_string(req, &out)
}

/// `mb_ucfirst(string $string, ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_ucfirst("ǆemal"), mb_ucfirst("élan"), mb_ucfirst(""), mb_ucfirst("ABC"));
/// ```
/// ```expect
/// string(6) "ǅemal"
/// string(5) "Élan"
/// string(0) ""
/// string(3) "ABC"
/// ```
#[php_function]
fn mb_ucfirst(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    ulcfirst(req, string, encoding, CaseMode::Title)
}

/// `mb_lcfirst(string $string, ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_lcfirst("ÉLAN"), mb_lcfirst("abc"));
/// ```
/// ```expect
/// string(5) "éLAN"
/// string(3) "abc"
/// ```
#[php_function]
fn mb_lcfirst(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    ulcfirst(req, string, encoding, CaseMode::Lower)
}

/// `mb_trim_mode`.
#[derive(Clone, Copy)]
enum Trim {
    Left,
    Right,
    Both,
}

impl Trim {
    fn left(self) -> bool {
        matches!(self, Trim::Left | Trim::Both)
    }

    fn right(self) -> bool {
        matches!(self, Trim::Right | Trim::Both)
    }
}

/// `mb_trim_default_chars`'s set.
const TRIM_DEFAULT: [u32; 27] = [
    0x20, 0x0C, 0x0A, 0x0D, 0x09, 0x0B, 0x00, 0xA0, 0x1680, 0x2000, 0x2001, 0x2002, 0x2003, 0x2004,
    0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200A, 0x2028, 0x2029, 0x202F, 0x205F, 0x3000, 0x85,
    0x180E,
];

/// `trim_each_wchar`: how many codepoints to drop from each end.
fn trim_each(input: &[u8], what: &[u32], mode: Trim, enc: Encoding) -> (usize, usize, usize) {
    let (mut left, mut right, mut total) = (0, 0, 0);
    let (mut trimming_left, trimming_right) = (mode.left(), mode.right());
    decode_runs(enc, input, 128, |w, _| {
        total += w.len();
        for c in w {
            if what.contains(c) {
                left += usize::from(trimming_left);
                right += usize::from(trimming_right);
            } else {
                trimming_left = false;
                if trimming_right {
                    right = 0;
                }
            }
        }
        true
    });
    (left, right, total)
}

/// `php_do_mb_trim`.
fn do_trim(
    req: &mut Request,
    string: PhpString,
    characters: Option<PhpString>,
    encoding: Option<PhpString>,
    mode: Trim,
) -> Result<Owner<PhpString>, Thrown> {
    let enc = self::encoding(req, encoding, 3)?;
    let what: Vec<u32> = match characters {
        None => TRIM_DEFAULT.to_vec(),
        Some(chars) => {
            // `mb_trim_what_chars`: a first run of four codepoints or fewer
            // is the whole set, whatever follows it.
            let mut all = Vec::new();
            decode_runs(enc, chars.as_bytes(), 128, |w, _| {
                let first = all.is_empty();
                all.extend_from_slice(w);
                !(first && w.len() <= 4)
            });
            all
        }
    };
    if what.is_empty() {
        return Ok(same(req, string));
    }
    let (left, right, total) = trim_each(string.as_bytes(), &what, mode, enc);
    if left == 0 && right == 0 {
        return Ok(same(req, string));
    }
    let sub = substitute(req);
    on_heap(req, || {
        substr(
            string.as_bytes(),
            left,
            total.saturating_sub(right + left),
            enc,
            sub,
        )
    })?
    .into_string(req)
}

/// `mb_trim(string $string, ?string $characters = null, ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_trim("\u{3000} abc \u{a0}\n"), mb_trim("ñañ", "ñ"), mb_trim("xyx", ""), mb_trim("abc", "cab"));
/// ```
/// ```expect
/// string(3) "abc"
/// string(1) "a"
/// string(3) "xyx"
/// string(0) ""
/// ```
#[php_function]
fn mb_trim(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] characters: Option<PhpString>,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    do_trim(req, string, characters, encoding, Trim::Both)
}

/// `mb_ltrim(string $string, ?string $characters = null, ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_ltrim("\u{3000} abc "), mb_ltrim("ñañ", "ñ"));
/// ```
/// ```expect
/// string(4) "abc "
/// string(3) "añ"
/// ```
#[php_function]
fn mb_ltrim(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] characters: Option<PhpString>,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    do_trim(req, string, characters, encoding, Trim::Left)
}

/// `mb_rtrim(string $string, ?string $characters = null, ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_rtrim(" abc \u{3000}"), mb_rtrim("ñañ", "ñ"));
/// ```
/// ```expect
/// string(4) " abc"
/// string(3) "ña"
/// ```
#[php_function]
fn mb_rtrim(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] characters: Option<PhpString>,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    do_trim(req, string, characters, encoding, Trim::Right)
}

/// `mb_ord(string $string, ?string $encoding = null): int|false`.
///
/// ```php
/// var_dump(mb_ord("€"), mb_ord("\x80\x00", "UTF-16BE"), mb_ord("\xff"), mb_ord("\xa4\xa2", "EUC-JP"));
/// try { mb_ord(""); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mb_ord("a", "JIS"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(8364)
/// int(32768)
/// bool(false)
/// int(12354)
/// mb_ord(): Argument #1 ($string) must not be empty
/// mb_ord() does not support the "JIS" encoding
/// ```
#[php_function]
fn mb_ord(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<i64>, Thrown> {
    if string.is_empty() {
        return Err(must_not_be_empty(req, 1));
    }
    let enc = self::encoding(req, encoding, 2)?;
    if is_unsupported(enc) {
        let name = String::from_utf8_lossy(enc.name());
        return Err(php_ext::raising::value_error(
            req,
            &format!("mb_ord() does not support the \"{name}\" encoding"),
        ));
    }
    let mut buf = [0u32; 5];
    let mut state = 0;
    let mut input = string.as_bytes();
    let n = enc.decode(&mut input, &mut buf, &mut state);
    if n == 0 || buf[0] == MBFL_BAD_INPUT {
        return Ok(OrFalse::False);
    }
    Ok(OrFalse::Value(i64::from(buf[0])))
}

/// `mb_chr(int $codepoint, ?string $encoding = null): string|false`.
///
/// ```php
/// var_dump(mb_chr(0x20AC), mb_chr(0xD800), mb_chr(0x110000), bin2hex(mb_chr(0x3042, "SJIS")), mb_chr(0x20AC, "ISO-8859-1"), mb_chr(65, "ASCII"));
/// try { mb_chr(65, "UTF-7"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "€"
/// bool(false)
/// bool(false)
/// string(4) "82a0"
/// bool(false)
/// string(1) "A"
/// mb_chr() does not support the "UTF-7" encoding
/// ```
#[php_function]
fn mb_chr(
    req: &mut Request,
    codepoint: i64,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let enc = self::encoding(req, encoding, 2)?;
    if is_unsupported(enc) {
        let name = String::from_utf8_lossy(enc.name());
        return Err(php_ext::raising::value_error(
            req,
            &format!("mb_chr() does not support the \"{name}\" encoding"),
        ));
    }
    let Some(cp) = u32::try_from(codepoint)
        .ok()
        .filter(|&cp| cp <= 0x0010_FFFF)
    else {
        return Ok(OrFalse::False);
    };
    if is_utf8(enc) {
        // `ZSTR_CHAR(cp)` below 0x80, `zend_string_alloc` above.
        return match char::from_u32(cp) {
            Some(c) => Ok(OrFalse::Value(PhpString::new(
                req,
                c.encode_utf8(&mut [0; 4]).as_bytes(),
            )?)),
            None => Ok(OrFalse::False),
        };
    }
    let sub = substitute(req);
    let (out, errors) = on_heap(req, || {
        convert(&cp.to_be_bytes(), Encoding::ucs4be(), enc, sub)
    })?;
    if errors != 0 {
        return Ok(OrFalse::False);
    }
    Ok(OrFalse::Value(new_string(req, &out)?))
}

/// `mb_str_pad(string $string, int $length, string $pad_string = " ", int $pad_type = STR_PAD_RIGHT, ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_str_pad("ñ", 5, "ab"), mb_str_pad("ñ", 4, "é", STR_PAD_LEFT), mb_str_pad("ñ", 4, "-", STR_PAD_BOTH), mb_str_pad("abc", 2));
/// try { mb_str_pad("a", 3, ""); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mb_str_pad("a", 3, " ", 7); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(6) "ñabab"
/// string(8) "éééñ"
/// string(5) "-ñ--"
/// string(3) "abc"
/// mb_str_pad(): Argument #3 ($pad_string) must not be empty
/// mb_str_pad(): Argument #4 ($pad_type) must be STR_PAD_LEFT, STR_PAD_RIGHT, or STR_PAD_BOTH
/// ```
///
/// Since 8.5.12 the string is asked for fallibly, so padding past the
/// memory limit is refused and the limit's fatal is raised as the call
/// returns:
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(strlen(mb_str_pad("a", 100 << 20)), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 104873984 bytes) in %s on line 3
/// ```
#[php_function]
fn mb_str_pad(
    req: &mut Request,
    string: PhpString,
    length: i64,
    #[php(default = " ")] pad_string: PhpString,
    #[php(default = STR_PAD_RIGHT)] pad_type: i64,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let enc = self::encoding(req, encoding, 5)?;
    let input_length = strlen(string, enc);
    let Some(target) = usize::try_from(length).ok().filter(|&l| l > input_length) else {
        return Ok(same(req, string));
    };
    if pad_string.is_empty() {
        return Err(must_not_be_empty(req, 3));
    }
    if !(0..=2).contains(&pad_type) {
        return Err(value_error(
            req,
            4,
            "must be STR_PAD_LEFT, STR_PAD_RIGHT, or STR_PAD_BOTH",
        ));
    }
    let pad_length = strlen(pad_string, enc);
    if pad_length == 0 {
        return Err(must_not_be_empty(req, 3));
    }
    let chars = target - input_length;
    let (left, right) = match pad_type {
        0 => (chars, 0),
        1 => (0, chars),
        _ => (chars / 2, chars - chars / 2),
    };
    let pad = pad_string.as_bytes();
    let sub = substitute(req);
    let (full_left, full_right) = (left / pad_length, right / pad_length);
    let (Some(full_left_bytes), Some(full_right_bytes)) = (
        full_left.checked_mul(pad.len()),
        full_right.checked_mul(pad.len()),
    ) else {
        return Err(throw_error(req, "Error", "String size overflow"));
    };
    let (tail_left, tail_right) = on_heap(req, || {
        (
            substr(pad, 0, left % pad_length, enc, sub).into_bytes(),
            substr(pad, 0, right % pad_length, enc, sub).into_bytes(),
        )
    })?;
    // `ZSTR_MAX_LEN`, `SIZE_MAX` less `ZSTR_MAX_OVERHEAD`, the aligned
    // header and terminator.
    let max = usize::MAX - 32;
    let bytes = full_left_bytes
        .checked_add(tail_left.len())
        .zip(full_right_bytes.checked_add(tail_right.len()))
        .and_then(|(l, r)| l.checked_add(r))
        .filter(|&pads| pads <= max && string.len() <= max - pads);
    let Some(pads) = bytes else {
        return Err(throw_error(req, "Error", "String size overflow"));
    };
    let input = string.as_bytes();
    // `zend_string_safe_falloc` since 8.5.12: the string asked for
    // fallibly, `RETURN_OOM()` where the memory limit refused it.
    let fill = |out: &mut [u8]| {
        let mut at = 0;
        let mut put = |bytes: &[u8]| {
            out[at..at + bytes.len()].copy_from_slice(bytes);
            at += bytes.len();
        };
        for _ in 0..full_left {
            put(pad);
        }
        put(&tail_left);
        put(input);
        for _ in 0..full_right {
            put(pad);
        }
        put(&tail_right);
    };
    match req
        .memory_mut()
        .fallible(|req| PhpString::product(req, 1, input.len(), pads, fill))?
    {
        Some(made) => Ok(made),
        // RETURN_OOM()
        None => cut_string(req, b""),
    }
}

/// `mb_scrub(string $string, ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_scrub("a\xffb"), mb_scrub("añb"), bin2hex(mb_scrub("\x82\xa0\x82", "SJIS")));
/// ```
/// ```expect
/// string(3) "a?b"
/// string(4) "añb"
/// string(6) "82a03f"
/// ```
#[php_function]
fn mb_scrub(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let enc = self::encoding(req, encoding, 2)?;
    if enc == Encoding::utf8()
        && (string.is_valid_utf8() || std::str::from_utf8(string.as_bytes()).is_ok())
    {
        return Ok(same(req, string));
    }
    let sub = substitute(req);
    let (out, errors) = on_heap(req, || convert(string.as_bytes(), enc, enc, sub))?;
    req.state::<crate::settings::State>().illegal_chars += u64::from(errors);
    new_string(req, &out)
}
