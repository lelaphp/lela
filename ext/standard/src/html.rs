//! `Z/ext/standard/html.c`: `php_escape_html_entities_ex` behind
//! `htmlspecialchars` and `htmlentities`, `traverse_for_entities`
//! behind `htmlspecialchars_decode` and `html_entity_decode`, and
//! `get_html_translation_table` walking the same tables, which are
//! `html_tables.h`'s, in `tables`. The charsets are php's fourteen:
//! UTF-8, the eight single-byte ones with full tables, and the five
//! multi-byte ones php reads and passes through, basic entities alone.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod tables;

use php_ext::abi::Passed;
use php_ext::attr::{php_const, php_function};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Charset, FreshString, Owner, PhpArray, PhpString};

use tables::Row;

/// `ENT_HTML_QUOTE_SINGLE` and the rest of `html.h`'s `$flags` bits.
const QUOTE_SINGLE: i64 = 1;
const QUOTE_DOUBLE: i64 = 2;
const IGNORE_ERRORS: i64 = 4;
const SUBSTITUTE_ERRORS: i64 = 8;
const DOC_TYPE_MASK: i64 = 16 | 32;
const DOC_HTML401: i64 = 0;
const DOC_XML1: i64 = 16;
const DOC_XHTML: i64 = 32;
const DOC_HTML5: i64 = 16 | 32;
const SUBSTITUTE_DISALLOWED: i64 = 128;
/// `ENT_QUOTES | ENT_SUBSTITUTE`, the flags `url_scanner_ex.re` hands
/// `php_escape_html_entities_ex` for a rewrite variable's name and value,
/// and every native's default `$flags` beside `ENT_HTML401`, which is 0.
pub(super) const QUOTES_SUBSTITUTE: i64 = QUOTE_SINGLE | QUOTE_DOUBLE | SUBSTITUTE_ERRORS;
/// `ENT_QUOTES`, what `php_info_print_html_esc` hands the escaper.
pub(super) const QUOTES: i64 = QUOTE_SINGLE | QUOTE_DOUBLE;
/// `LONGEST_ENTITY_LENGTH`.
const LONGEST_ENTITY_LENGTH: usize = 31;

/// The constants of `html.c`, in the stub's order.
///
/// ```php
/// var_dump(HTML_SPECIALCHARS, HTML_ENTITIES, ENT_COMPAT, ENT_QUOTES, ENT_NOQUOTES, ENT_IGNORE, ENT_SUBSTITUTE, ENT_DISALLOWED, ENT_HTML401, ENT_XML1, ENT_XHTML, ENT_HTML5);
/// ```
/// ```expect
/// int(0)
/// int(1)
/// int(2)
/// int(3)
/// int(0)
/// int(4)
/// int(8)
/// int(128)
/// int(0)
/// int(16)
/// int(32)
/// int(48)
/// ```
#[php_const]
pub(crate) mod constants {
    use super::{
        DOC_HTML5, DOC_HTML401, DOC_XHTML, DOC_XML1, IGNORE_ERRORS, QUOTE_DOUBLE, QUOTE_SINGLE,
        SUBSTITUTE_DISALLOWED, SUBSTITUTE_ERRORS,
    };

    pub(crate) const HTML_SPECIALCHARS: i64 = 0;
    pub(crate) const HTML_ENTITIES: i64 = 1;
    pub(crate) const ENT_COMPAT: i64 = QUOTE_DOUBLE;
    pub(crate) const ENT_QUOTES: i64 = QUOTE_SINGLE | QUOTE_DOUBLE;
    pub(crate) const ENT_NOQUOTES: i64 = 0;
    pub(crate) const ENT_IGNORE: i64 = IGNORE_ERRORS;
    pub(crate) const ENT_SUBSTITUTE: i64 = SUBSTITUTE_ERRORS;
    pub(crate) const ENT_DISALLOWED: i64 = SUBSTITUTE_DISALLOWED;
    pub(crate) const ENT_HTML401: i64 = DOC_HTML401;
    pub(crate) const ENT_XML1: i64 = DOC_XML1;
    pub(crate) const ENT_XHTML: i64 = DOC_XHTML;
    pub(crate) const ENT_HTML5: i64 = DOC_HTML5;
}

/// `enc_to_uni_index[charset]` for a single-byte charset php maps to
/// Unicode.
fn to_unicode(charset: Charset) -> Option<&'static [u16; 256]> {
    Some(match charset {
        Charset::Cp1252 => &tables::TO_UNI_CP1252,
        Charset::Iso885915 => &tables::TO_UNI_ISO_8859_15,
        Charset::Cp1251 => &tables::TO_UNI_CP1251,
        Charset::Iso88595 => &tables::TO_UNI_ISO_8859_5,
        Charset::Cp866 => &tables::TO_UNI_CP866,
        Charset::MacRoman => &tables::TO_UNI_MACROMAN,
        Charset::Koi8r => &tables::TO_UNI_KOI8_R,
        _ => return None,
    })
}

/// `determine_charset(charset_hint, quiet)`: the hint, else
/// `internal_encoding`, else `default_charset`, looked up in
/// `charset_map`; a name it lacks is UTF-8 behind php's warning.
fn determine_charset(req: &mut Request, hint: Option<&[u8]>, quiet: bool) -> Charset {
    // The hint is a C string: it ends at its first NUL.
    let hint = hint.map(|h| php_ext::coerce::c_str(h).to_vec());
    let hint = match hint {
        Some(h) if !h.is_empty() => h,
        _ => ["internal_encoding", "default_charset"]
            .iter()
            .find_map(|name| {
                req.ini()
                    .get(name.as_bytes())
                    .filter(|v| !v.is_empty())
                    .map(<[u8]>::to_vec)
            })
            .unwrap_or_default(),
    };
    if hint.is_empty() {
        return Charset::Utf8;
    }
    if let Some(charset) = Charset::named(&hint) {
        return charset;
    }
    if !quiet {
        let text = String::from_utf8_lossy(&hint).into_owned();
        docref(
            req,
            Level::Warning,
            &format!("Charset \"{text}\" is not supported, assuming UTF-8"),
        );
    }
    Charset::Utf8
}

/// `unimap_bsearch(table, code, num)`: the byte `table` gives a BMP
/// code point, or none.
fn unimap(table: &[(u16, u8)], code: u32) -> Option<u32> {
    let code = u16::try_from(code).ok()?;
    table
        .binary_search_by_key(&code, |(cp, _)| *cp)
        .ok()
        .map(|at| u32::from(table[at].1))
}

/// `map_from_unicode(code, charset, &res)`: the code unit `charset`
/// writes a code point as, or none where it has none.
fn map_from_unicode(code: u32, charset: Charset) -> Option<u32> {
    match charset {
        Charset::Iso88591 => (code <= 0xFF).then_some(code),
        Charset::Iso88595 => {
            if code <= 0xA0 || code == 0xAD {
                Some(code)
            } else if code == 0x2116 {
                Some(0xF0)
            } else if code == 0xA7 {
                Some(0xFD)
            } else if (0x0401..=0x045F).contains(&code) {
                if code == 0x040D || code == 0x0450 || code == 0x045D {
                    None
                } else {
                    Some(code - 0x360)
                }
            } else {
                None
            }
        }
        Charset::Iso885915 => {
            if code < 0xA4 || (code > 0xBE && code <= 0xFF) {
                Some(code)
            } else {
                unimap(tables::FROM_UNI_ISO_8859_15, code)
            }
        }
        Charset::Cp1252 => {
            if code <= 0x7F || (0xA0..=0xFF).contains(&code) {
                Some(code)
            } else {
                unimap(tables::FROM_UNI_CP1252, code)
            }
        }
        Charset::MacRoman | Charset::Cp1251 | Charset::Koi8r | Charset::Cp866 => {
            if charset == Charset::MacRoman && code == 0x7F {
                return None;
            }
            if code <= 0x7F {
                return Some(code);
            }
            let table = match charset {
                Charset::MacRoman => tables::FROM_UNI_MACROMAN,
                Charset::Cp1251 => tables::FROM_UNI_CP1251,
                Charset::Koi8r => tables::FROM_UNI_KOI8_R,
                _ => tables::FROM_UNI_CP866,
            };
            unimap(table, code)
        }
        Charset::Sjis | Charset::EucJp => {
            ((0x20..=0x7D).contains(&code) && code != 0x5C).then_some(code)
        }
        Charset::Big5 | Charset::Big5Hkscs | Charset::Gb2312 => {
            (0x20..=0x7D).contains(&code).then_some(code)
        }
        Charset::Utf8 => None,
    }
}

/// `unicode_cp_is_allowed(uni_cp, document_type)`.
fn unicode_cp_is_allowed(cp: u32, doctype: i64) -> bool {
    match doctype {
        DOC_HTML401 => {
            (0x20..=0x7E).contains(&cp)
                || cp == 0x0A
                || cp == 0x09
                || cp == 0x0D
                || (0xA0..=0xD7FF).contains(&cp)
                || (0xE000..=0x0010_FFFF).contains(&cp)
        }
        DOC_HTML5 => {
            (0x20..=0x7E).contains(&cp)
                || ((0x09..=0x0D).contains(&cp) && cp != 0x0B)
                || (0xA0..=0xD7FF).contains(&cp)
                || ((0xE000..=0x0010_FFFF).contains(&cp)
                    && (cp & 0xFFFF) < 0xFFFE
                    && !(0xFDD0..=0xFDEF).contains(&cp))
        }
        _ => {
            (0x20..=0xD7FF).contains(&cp)
                || cp == 0x0A
                || cp == 0x09
                || cp == 0x0D
                || ((0xE000..=0x0010_FFFF).contains(&cp) && cp != 0xFFFE && cp != 0xFFFF)
        }
    }
}

/// `numeric_entity_is_allowed(uni_cp, document_type)`.
fn numeric_entity_is_allowed(cp: u32, doctype: i64) -> bool {
    match doctype {
        DOC_HTML401 => cp <= 0x0010_FFFF,
        DOC_HTML5 => {
            (0x20..=0x7E).contains(&cp)
                || ((0x09..=0x0C).contains(&cp) && cp != 0x0B)
                || ((0xA0..=0x0010_FFFF).contains(&cp)
                    && (cp & 0xFFFF) < 0xFFFE
                    && !(0xFDD0..=0xFDEF).contains(&cp))
        }
        _ => unicode_cp_is_allowed(cp, doctype),
    }
}

/// `process_numeric_entity(&buf, &code_point)` with `buf` just past
/// `#`: the code point and the offset of the `;` ending it.
fn numeric_entity(s: &[u8], mut at: usize) -> Option<(u32, usize)> {
    let hex = matches!(s.get(at), Some(b'x' | b'X'));
    if hex {
        at += 1;
    }
    let digit = |c: &u8| {
        if hex {
            c.is_ascii_hexdigit()
        } else {
            c.is_ascii_digit()
        }
    };
    if !s.get(at).is_some_and(digit) {
        return None;
    }
    let start = at;
    while s.get(at).is_some_and(digit) {
        at += 1;
    }
    if s.get(at) != Some(&b';') {
        return None;
    }
    let text = std::str::from_utf8(&s[start..at]).ok()?;
    // `ZEND_STRTOL` saturates; anything past U+10FFFF is refused alike.
    let code = i64::from_str_radix(text, if hex { 16 } else { 10 }).unwrap_or(i64::MAX);
    if code > 0x0010_FFFF {
        return None;
    }
    Some((u32::try_from(code).ok()?, at))
}

/// `process_named_entity_html(&buf, &start, &length)`: the name's
/// length where `s[at..]` is letters and digits closed by `;`.
fn named_entity(s: &[u8], at: usize) -> Option<usize> {
    let len = s[at.min(s.len())..]
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric())
        .count();
    (len > 0 && s.get(at + len) == Some(&b';')).then_some(len)
}

/// Which entity-to-code-point table a decode reads,
/// `unescape_inverse_map(all, flags)`.
fn inverse_map(all: bool, flags: i64) -> &'static [(&'static str, u32, u32)] {
    let doctype = flags & DOC_TYPE_MASK;
    match (all, doctype) {
        (true, DOC_HTML401 | DOC_XHTML) => tables::DECODE_HTML4,
        (true, DOC_HTML5) => tables::DECODE_HTML5,
        (false, DOC_HTML401) => tables::DECODE_BASIC_NOAPOS,
        _ => tables::DECODE_BASIC_APOS,
    }
}

/// `resolve_named_entity_html(start, length, ht, &cp1, &cp2)`.
fn resolve_named(name: &[u8], map: &[(&str, u32, u32)]) -> Option<(u32, u32)> {
    map.binary_search_by(|(entity, _, _)| entity.as_bytes().cmp(name))
        .ok()
        .map(|at| (map[at].1, map[at].2))
}

/// `php_utf32_utf8` and the single byte, `write_octet_sequence`.
fn write_octets(out: &mut Vec<u8>, charset: Charset, code: u32) {
    if charset != Charset::Utf8 {
        out.push(u8::try_from(code & 0xFF).unwrap_or(0));
        return;
    }
    let mut buf = [0u8; 4];
    if let Some(ch) = char::from_u32(code) {
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    } else {
        // A surrogate: php writes its three bytes all the same.
        let b = |v: u32| u8::try_from(v & 0xFF).unwrap_or(0);
        out.extend_from_slice(&[
            b(0xE0 | (code >> 12)),
            b(0x80 | ((code >> 6) & 0x3F)),
            b(0x80 | (code & 0x3F)),
        ]);
    }
}

/// `traverse_for_entities(input, output, all, flags, inv_map,
/// charset)`.
fn traverse_for_entities(input: &[u8], all: bool, flags: i64, charset: Charset) -> Vec<u8> {
    let map = inverse_map(all, flags);
    let doctype = flags & DOC_TYPE_MASK;
    let mut out = Vec::with_capacity(input.len());
    let mut cur = 0usize;
    while cur < input.len() {
        let Some(amp) = input[cur..]
            .iter()
            .position(|c| *c == b'&')
            .map(|at| at + cur)
        else {
            out.extend_from_slice(&input[cur..]);
            break;
        };
        out.extend_from_slice(&input[cur..amp]);
        cur = amp;
        if input.len() - cur < 4 {
            out.extend_from_slice(&input[cur..]);
            break;
        }
        // The entity's code points and the offset of its `;`, or none
        // for what is no entity, where the `&` is copied alone.
        let parsed: Option<(u32, u32, usize)> = if input[cur + 1] == b'#' {
            numeric_entity(input, cur + 2)
                .filter(|(code, _)| {
                    let basic =
                        *code <= 63 && tables::ENCODE_BASIC_APOS.iter().any(|(cp, _)| cp == code);
                    (all || basic)
                        && unicode_cp_is_allowed(*code, doctype)
                        && !(doctype == DOC_HTML5 && *code == 0x0D)
                })
                .map(|(code, end)| (code, 0, end))
        } else {
            let name_start = cur + 1;
            let max = (LONGEST_ENTITY_LENGTH + 1).min(input.len() - name_start);
            match input[name_start..name_start + max]
                .iter()
                .position(|c| *c == b';')
            {
                None | Some(0) => None,
                Some(len) => {
                    let name = &input[name_start..name_start + len];
                    match resolve_named(name, map) {
                        Some((c1, c2)) => Some((c1, c2, name_start + len)),
                        None if doctype == DOC_XHTML && name == b"apos" => {
                            Some((u32::from(b'\''), 0, name_start + len))
                        }
                        None => None,
                    }
                }
            }
        };
        let Some((code, code2, end)) = parsed else {
            out.push(input[cur]);
            cur += 1;
            continue;
        };
        let refused_quote = (code == u32::from(b'\'') && flags & QUOTE_SINGLE == 0)
            || (code == u32::from(b'"') && flags & QUOTE_DOUBLE == 0);
        let code = if refused_quote {
            None
        } else if charset == Charset::Utf8 {
            Some(code)
        } else {
            map_from_unicode(code, charset).filter(|_| code2 == 0)
        };
        let Some(code) = code else {
            // `invalid_complete_entity`: the text up to the `;` kept,
            // the `;` read next as any byte is.
            out.extend_from_slice(&input[cur..end]);
            cur = end;
            continue;
        };
        write_octets(&mut out, charset, code);
        if code2 != 0 {
            write_octets(&mut out, charset, code2);
        }
        cur = end + 1;
    }
    out
}

/// `php_unescape_html_entities(str, all, flags, hint_charset)`.
fn unescape(
    req: &mut Request,
    string: PhpString,
    all: bool,
    flags: i64,
    hint: Option<&[u8]>,
) -> Result<Owner<PhpString>, Thrown> {
    let s = string.as_bytes();
    if !s.contains(&b'&') {
        return Ok(Owner::take(req, string));
    }
    let charset = if all {
        determine_charset(req, hint, false)
    } else {
        Charset::Iso88591
    };
    let out = traverse_for_entities(s, all, flags, charset);
    PhpString::new(req, &out)
}

/// The encode table `determine_entity_table(all, doctype)` picks.
fn entity_table(all: bool, doctype: i64) -> &'static [(u32, Row)] {
    match (all, doctype) {
        (true, DOC_HTML5) => tables::ENCODE_HTML5,
        (true, _) => tables::ENCODE_HTML4,
        (false, DOC_HTML401) => tables::ENCODE_BASIC_NOAPOS,
        (false, _) => tables::ENCODE_BASIC_APOS,
    }
}

/// The row a table gives a code point.
fn row_of(table: &'static [(u32, Row)], cp: u32) -> Option<&'static Row> {
    table
        .binary_search_by_key(&cp, |(k, _)| *k)
        .ok()
        .map(|at| &table[at].1)
}

/// `php_escape_html_entities_ex(old, oldlen, all, flags, hint_charset,
/// double_encode, quiet)`: the escaped bytes, or the empty string for
/// input ill-formed in its charset where neither `ENT_IGNORE` nor
/// `ENT_SUBSTITUTE` says what to do; the bytes alone, as
/// `php_info_print_html_esc` prints them, where [`escape_string`] makes
/// php's string on the request's heap, as ext/filter's
/// `FILTER_SANITIZE_FULL_SPECIAL_CHARS` calls it with `all` set and
/// `double_encode` off.
///
/// ```php
/// var_dump(htmlentities("<é> &amp;", ENT_QUOTES, null, false), filter_var("<é> &amp; 'q'", FILTER_SANITIZE_FULL_SPECIAL_CHARS));
/// ```
/// ```expect
/// string(22) "&lt;&eacute;&gt; &amp;"
/// string(36) "&lt;&eacute;&gt; &amp; &#039;q&#039;"
/// ```
pub fn escape(
    req: &mut Request,
    old: &[u8],
    all: bool,
    flags: i64,
    hint: Option<&[u8]>,
    double_encode: bool,
) -> Vec<u8> {
    escape_grown(req, old, all, flags, hint, double_encode)
        .bytes
        .unwrap_or_default()
}

/// What `php_escape_html_entities_ex` asks of the request's heap besides
/// the bytes it writes: `replaced`'s room, its first estimate and each
/// growth as the loop meets it.
struct Escaped {
    /// The escaped bytes, `None` for input ill-formed in its charset
    /// that is refused, php's `zend_string_efree` and the empty string.
    bytes: Option<Vec<u8>>,
    /// The room php grows `replaced` to, in order, and whether it asks
    /// fallibly: the top of each character's turn where fewer than 40
    /// bytes are left (`zend_string_safe_frealloc(..., 128, fallible:
    /// true)` since 8.5.12), and infallibly before a kept entity longer
    /// than what is left.
    grown: Vec<(usize, bool)>,
}

/// `php_escape_html_entities_ex(old, oldlen, all, flags, hint_charset,
/// double_encode, quiet)` as php 8.5.12 lays it on the request's heap
/// (`Z/ext/standard/html.c:1173-1340`): `replaced` asked for fallibly at
/// 128 bytes or twice the input and grown fallibly by 128 whenever fewer
/// than 40 are left, the empty string where the memory limit refused
/// either, and the string returned the room with what was written, not
/// cut to it. `PHPAPI` in `html.h`, which ext/filter's
/// `FILTER_SANITIZE_FULL_SPECIAL_CHARS` calls.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(htmlspecialchars(str_repeat("'", 2 << 20)), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 4210688 bytes) in %s on line 3
/// ```
pub fn escape_string(
    req: &mut Request,
    old: &[u8],
    all: bool,
    flags: i64,
    hint: Option<&[u8]>,
    double_encode: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let escaped = escape_grown(req, old, all, flags, hint, double_encode);
    let maxlen = if old.len() < 64 {
        128
    } else {
        old.len().saturating_mul(2)
    };
    // `zend_string_falloc(maxlen, fallible: true)`, `ZSTR_EMPTY_ALLOC()`
    // where it is refused.
    let Some(mut room) = req
        .memory_mut()
        .fallible(|req| FreshString::new(req, maxlen))?
    else {
        return PhpString::new(req, b"");
    };
    for (len, fallibly) in escaped.grown {
        room = if fallibly {
            let Some(grown) = req.memory_mut().fallible(|req| room.extend(req, len))? else {
                return PhpString::new(req, b"");
            };
            grown
        } else {
            room.extend(req, len)?
        };
    }
    let Some(bytes) = escaped.bytes else {
        room.free(req);
        return PhpString::new(req, b"");
    };
    room.bytes_mut()[..bytes.len()].copy_from_slice(&bytes);
    room.set_len(bytes.len());
    Ok(room.finish())
}

/// The escape itself, its bytes and the growth php's `replaced` meets
/// on the way ([`Escaped`]).
#[allow(clippy::too_many_lines)]
fn escape_grown(
    req: &mut Request,
    old: &[u8],
    all: bool,
    flags: i64,
    hint: Option<&[u8]>,
    double_encode: bool,
) -> Escaped {
    let charset = determine_charset(req, hint, false);
    let doctype = flags & DOC_TYPE_MASK;
    let mut all = all;
    if all {
        if charset.partial() {
            docref(
                req,
                Level::Notice,
                "Only basic entities substitution is supported for multi-byte encodings other than UTF-8; functionality is equivalent to htmlspecialchars",
            );
        }
        all = !charset.partial() && doctype != DOC_XML1;
    }
    let table = entity_table(all, doctype);
    let to_uni = if all && !charset.unicode_compat() {
        to_unicode(charset)
    } else {
        None
    };
    let inv_map = inverse_map(true, flags);
    let replacement: &[u8] = if charset == Charset::Utf8 {
        "\u{FFFD}".as_bytes()
    } else {
        b"&#xFFFD;"
    };
    let mut out: Vec<u8> = Vec::with_capacity(old.len() + old.len() / 2);
    let mut maxlen = if old.len() < 64 {
        128
    } else {
        old.len().saturating_mul(2)
    };
    let mut grown = Vec::new();
    let mut cursor = 0usize;
    while cursor < old.len() {
        let before = cursor;
        let (this_char, next) = charset.next_char(old, cursor);
        cursor = next;
        // "guarantee we have at least 40 bytes to write".
        if out.len() > maxlen - 40 {
            maxlen += 128;
            grown.push((maxlen, true));
        }
        let Some(mut this_char) = this_char else {
            if flags & IGNORE_ERRORS != 0 {
                continue;
            } else if flags & SUBSTITUTE_ERRORS != 0 {
                out.extend_from_slice(replacement);
                continue;
            }
            return Escaped { bytes: None, grown };
        };
        let mut sequence: &[u8] = &old[before..cursor];
        if this_char != u32::from(b'&') {
            let quote_passes = (this_char == u32::from(b'\'') && flags & QUOTE_SINGLE == 0)
                || (this_char == u32::from(b'"') && flags & QUOTE_DOUBLE == 0);
            if quote_passes {
                out.extend_from_slice(sequence);
                continue;
            }
            let mut entity: Option<&str> = None;
            if all {
                if let Some(to_uni) = to_uni {
                    this_char = u32::from(to_uni[usize::try_from(this_char & 0xFF).unwrap_or(0)]);
                    if this_char == 0xFFFF {
                        out.extend_from_slice(sequence);
                        continue;
                    }
                }
                // `find_entity_for_char`: past plane 1's `0x1D` stage the
                // table has nothing; an ambiguous row peeks at the next
                // character and takes it where a pair names both.
                match row_of(table, this_char) {
                    Some(Row::One(name)) => entity = Some(name),
                    Some(Row::Many(default, pairs)) => {
                        let peek = if cursor < old.len() {
                            match charset.next_char(old, cursor) {
                                (Some(second), after) => Some((second, after)),
                                (None, _) => None,
                            }
                        } else {
                            None
                        };
                        match peek.and_then(|(second, after)| {
                            pairs
                                .iter()
                                .find(|(cp, _)| *cp == second)
                                .map(|(_, name)| (*name, after))
                        }) {
                            Some((name, after)) => {
                                entity = Some(name);
                                cursor = after;
                            }
                            None => entity = *default,
                        }
                    }
                    None => {}
                }
            } else if this_char < 64
                && let Some(Row::One(name)) = row_of(table, this_char)
            {
                entity = Some(name);
            }
            if let Some(name) = entity {
                out.push(b'&');
                out.extend_from_slice(name.as_bytes());
                out.push(b';');
                continue;
            }
            if flags & SUBSTITUTE_DISALLOWED != 0 {
                if charset.unicode_compat() {
                    if !unicode_cp_is_allowed(this_char, doctype) {
                        sequence = replacement;
                    }
                } else if to_uni.is_some() {
                    // Mapped to Unicode above, since a map is only in
                    // hand where every entity is.
                    if !unicode_cp_is_allowed(this_char, doctype) {
                        sequence = replacement;
                    }
                } else if this_char <= 0x7D && !unicode_cp_is_allowed(this_char, doctype) {
                    sequence = replacement;
                }
            }
            out.extend_from_slice(sequence);
            continue;
        }
        // `&`: encoded, or, without double encoding, kept where a
        // valid entity follows.
        if !double_encode {
            let valid = if old.get(cursor) == Some(&b'#') {
                numeric_entity(old, cursor + 1).and_then(|(code, end)| {
                    let refused = flags & SUBSTITUTE_DISALLOWED != 0
                        && !numeric_entity_is_allowed(code, doctype);
                    (!refused).then_some(end - cursor)
                })
            } else {
                named_entity(old, cursor).filter(|len| {
                    let name = &old[cursor..cursor + len];
                    resolve_named(name, inv_map).is_some()
                        || (doctype == DOC_XHTML && name == b"apos")
                })
            };
            if let Some(len) = valid {
                // "entity size is unbounded, we may need more memory".
                if maxlen - out.len() < len + 2 {
                    maxlen += len + 128;
                    grown.push((maxlen, false));
                }
                out.push(b'&');
                out.extend_from_slice(&old[cursor..cursor + len]);
                out.push(b';');
                cursor += len + 1;
                continue;
            }
        }
        out.extend_from_slice(b"&amp;");
    }
    Escaped {
        bytes: Some(out),
        grown,
    }
}

/// `php_html_entities(INTERNAL_FUNCTION_PARAM_PASSTHRU, all)`, over
/// the four arguments the entry held.
fn entities(
    req: &mut Request,
    (string, flags, hint, double_encode): (PhpString, i64, Option<PhpString>, bool),
    all: bool,
) -> Result<Owner<PhpString>, Thrown> {
    if string.is_empty() {
        return PhpString::new(req, b"");
    }
    // php 8.5.12 asks for the string and its growth fallibly, and
    // answers the empty string where the memory limit refused one, the
    // limit's fatal raised as the call returns ([`escape_string`]).
    escape_string(
        req,
        string.as_bytes(),
        all,
        flags,
        hint.as_ref().map(PhpString::as_bytes),
        double_encode,
    )
}

/// `htmlspecialchars(string $string, int $flags = ENT_QUOTES |
/// ENT_SUBSTITUTE | ENT_HTML401, ?string $encoding = null, bool
/// $double_encode = true): string`: `&`, `<`, `>` and the quotes the
/// flags name as the basic entities, ill-formed input as `$flags`
/// says.
///
/// ```php
/// var_dump(htmlspecialchars("<a href='x'>T&amp;C \"q\"</a>"), htmlspecialchars("'\"", ENT_NOQUOTES), htmlspecialchars("'\"", ENT_COMPAT), htmlspecialchars("'", ENT_QUOTES | ENT_HTML5), htmlspecialchars("&amp; &#38; &#x26; &bogus; &", ENT_QUOTES, null, false), bin2hex(htmlspecialchars("a\x80b")), htmlspecialchars("a\x80b", ENT_QUOTES), htmlspecialchars("a\x80b", ENT_IGNORE), htmlspecialchars(""));
/// var_dump(htmlspecialchars("\x01\x7f", ENT_QUOTES | ENT_DISALLOWED | ENT_HTML5), htmlspecialchars("x", ENT_QUOTES, "bogus"));
/// ```
/// ```expect
/// string(63) "&lt;a href=&#039;x&#039;&gt;T&amp;amp;C &quot;q&quot;&lt;/a&gt;"
/// string(2) "'""
/// string(7) "'&quot;"
/// string(6) "&apos;"
/// string(36) "&amp; &#38; &#x26; &amp;bogus; &amp;"
/// string(10) "61efbfbd62"
/// string(0) ""
/// string(2) "ab"
/// string(0) ""
///
/// Warning: htmlspecialchars(): Charset "bogus" is not supported, assuming UTF-8 in %s on line 3
/// string(6) "��"
/// string(1) "x"
/// ```
///
/// The declarations as Reflection renders them.
///
/// ```php
/// foreach (['htmlspecialchars', 'htmlspecialchars_decode', 'html_entity_decode', 'htmlentities', 'get_html_translation_table'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function htmlspecialchars ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <optional> int $flags = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401 ]
///     Parameter #2 [ <optional> ?string $encoding = null ]
///     Parameter #3 [ <optional> bool $double_encode = true ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function htmlspecialchars_decode ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <optional> int $flags = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401 ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function html_entity_decode ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <optional> int $flags = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401 ]
///     Parameter #2 [ <optional> ?string $encoding = null ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function htmlentities ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <optional> int $flags = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401 ]
///     Parameter #2 [ <optional> ?string $encoding = null ]
///     Parameter #3 [ <optional> bool $double_encode = true ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function get_html_translation_table ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <optional> int $table = HTML_SPECIALCHARS ]
///     Parameter #1 [ <optional> int $flags = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401 ]
///     Parameter #2 [ <optional> string $encoding = "UTF-8" ]
///   }
///   - Return [ array ]
/// }
/// ```
///
/// php 8.5.12 asks for its first estimate of the escaped string, twice
/// the input, fallibly: past the memory limit it is refused and the call
/// answers the empty string, the limit's fatal raised as it returns.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(htmlspecialchars(str_repeat('a', 3 << 20)), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 6307840 bytes) in %s on line 3
/// ```
#[php_function]
fn htmlspecialchars(
    req: &mut Request,
    string: PhpString,
    #[php(default = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401)] flags: i64,
    #[php(default = null)] encoding: Option<PhpString>,
    #[php(default = true)] double_encode: bool,
) -> Result<Owner<PhpString>, Thrown> {
    entities(req, (string, flags, encoding, double_encode), false)
}

/// `htmlentities(...)`: every character the document type names, as
/// its entity, through the charset's map to Unicode; the multi-byte
/// charsets but UTF-8 and XML 1.0 have the basic entities alone.
///
/// ```php
/// var_dump(htmlentities("café <b> ©€"), htmlentities("café", ENT_QUOTES | ENT_HTML5), htmlentities("\xe9\x80", ENT_QUOTES, "cp1252"), htmlentities("\xe9", ENT_QUOTES, "ISO-8859-1"), htmlentities("<é>", ENT_QUOTES | ENT_XML1), htmlentities("≂̸ fj <⃒", ENT_QUOTES | ENT_HTML5), htmlentities("'", ENT_QUOTES | ENT_XHTML));
/// var_dump(htmlentities("\xa4<", ENT_QUOTES, "BIG5"));
/// ```
/// ```expect
/// string(34) "caf&eacute; &lt;b&gt; &copy;&euro;"
/// string(11) "caf&eacute;"
/// string(14) "&eacute;&euro;"
/// string(8) "&eacute;"
/// string(10) "&lt;é&gt;"
/// string(22) "&nesim; &fjlig; &nvlt;"
/// string(6) "&#039;"
///
/// Notice: htmlentities(): Only basic entities substitution is supported for multi-byte encodings other than UTF-8; functionality is equivalent to htmlspecialchars in %s on line 3
/// string(0) ""
/// ```
#[php_function]
fn htmlentities(
    req: &mut Request,
    string: PhpString,
    #[php(default = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401)] flags: i64,
    #[php(default = null)] encoding: Option<PhpString>,
    #[php(default = true)] double_encode: bool,
) -> Result<Owner<PhpString>, Thrown> {
    entities(req, (string, flags, encoding, double_encode), true)
}

/// `htmlspecialchars_decode(string $string, int $flags = ENT_QUOTES |
/// ENT_SUBSTITUTE | ENT_HTML401): string`: the basic entities, named
/// or numeric, back to their bytes, the quotes as the flags allow.
///
/// ```php
/// var_dump(htmlspecialchars_decode("&lt;b&gt; &amp;amp; &quot;q&quot; &#039;s&#39; &apos; &#60;&#x3E; &eacute;"), htmlspecialchars_decode("&quot;&#039;", ENT_NOQUOTES), htmlspecialchars_decode("&apos;", ENT_QUOTES | ENT_HTML5), htmlspecialchars_decode("&amp", ENT_QUOTES), htmlspecialchars_decode("no entity"));
/// ```
/// ```expect
/// string(36) "<b> &amp; "q" 's' &apos; <> &eacute;"
/// string(12) "&quot;&#039;"
/// string(1) "'"
/// string(4) "&amp"
/// string(9) "no entity"
/// ```
#[php_function]
fn htmlspecialchars_decode(
    req: &mut Request,
    string: PhpString,
    #[php(default = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401)] flags: i64,
) -> Result<Owner<PhpString>, Thrown> {
    unescape(req, string, false, flags, None)
}

/// `html_entity_decode(string $string, int $flags = ENT_QUOTES |
/// ENT_SUBSTITUTE | ENT_HTML401, ?string $encoding = null): string`:
/// every entity the document type names and every numeric one it
/// allows, back to the charset's bytes; one the charset cannot write
/// stays as written.
///
/// ```php
/// var_dump(html_entity_decode("caf&eacute; &euro; &#8364; &#x20AC; &hellip; &nosuch; &#0; &#xD800; &apos;"), html_entity_decode("&apos; &NotEqualTilde; &fjlig;", ENT_QUOTES | ENT_HTML5), bin2hex(html_entity_decode("&euro;&eacute;&#x2603;", ENT_QUOTES, "cp1252")), html_entity_decode("&apos;", ENT_QUOTES | ENT_XHTML), html_entity_decode("&quot;&#39;", ENT_NOQUOTES), bin2hex(html_entity_decode("&#1025;", ENT_QUOTES, "ISO-8859-5")));
/// ```
/// ```expect
/// string(51) "café € € € … &nosuch; &#0; &#xD800; &apos;"
/// string(10) "' ≂̸ fj"
/// string(20) "80e9262378323630333b"
/// string(1) "'"
/// string(11) "&quot;&#39;"
/// string(2) "a1"
/// ```
#[php_function]
fn html_entity_decode(
    req: &mut Request,
    string: PhpString,
    #[php(default = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401)] flags: i64,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    unescape(
        req,
        string,
        true,
        flags,
        encoding.as_ref().map(PhpString::as_bytes),
    )
}

/// `write_s3row_data(r, orig_cp, charset, arr)`: the row's entities
/// under the character, and under the character and its second for
/// each pair the charset can write.
fn write_row(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    row: &Row,
    key: &[u8],
    charset: Charset,
) -> Result<(), Thrown> {
    let mut put = |req: &mut Request, key: &[u8], name: &str| -> Result<(), Thrown> {
        let mut entity = Vec::with_capacity(name.len() + 2);
        entity.push(b'&');
        entity.extend_from_slice(name.as_bytes());
        entity.push(b';');
        let value = PhpString::new(req, &entity)?.into_value();
        array.set_symtable_str(req, key, value)
    };
    match row {
        Row::One(name) => put(req, key, name),
        Row::Many(default, pairs) => {
            if let Some(name) = default {
                put(req, key, name)?;
            }
            for (second, name) in *pairs {
                let code = if charset.unicode_compat() {
                    Some(*second)
                } else {
                    map_from_unicode(*second, charset)
                };
                let Some(code) = code else {
                    continue;
                };
                let mut both = key.to_vec();
                write_octets(&mut both, charset, code);
                put(req, &both, name)?;
            }
            Ok(())
        }
    }
}

/// `get_html_translation_table(int $table = HTML_SPECIALCHARS, int
/// $flags = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401, string $encoding
/// = "UTF-8"): array`: the table the escapes read, character to entity,
/// in code point order, or, for a single-byte charset, byte order.
///
/// ```php
/// var_dump(get_html_translation_table(), get_html_translation_table(HTML_SPECIALCHARS, ENT_NOQUOTES | ENT_HTML5), count(get_html_translation_table(HTML_ENTITIES)), count(get_html_translation_table(HTML_ENTITIES, ENT_QUOTES | ENT_HTML5)), count(get_html_translation_table(HTML_ENTITIES, ENT_QUOTES, "cp1252")), count(get_html_translation_table(HTML_ENTITIES, ENT_QUOTES | ENT_XML1)), count(get_html_translation_table(HTML_ENTITIES, ENT_QUOTES | ENT_HTML5, "ISO-8859-1")));
/// foreach ([[ENT_QUOTES | ENT_HTML5, "UTF-8"], [ENT_QUOTES | ENT_HTML401, "UTF-8"], [ENT_QUOTES | ENT_HTML401, "KOI8-R"], [ENT_COMPAT | ENT_HTML5, "cp1252"], [ENT_QUOTES | ENT_XHTML, "MacRoman"], [ENT_QUOTES | ENT_HTML5, "ISO-8859-5"]] as $p) { $s = ""; foreach (get_html_translation_table(HTML_ENTITIES, $p[0], $p[1]) as $k => $v) { $s .= bin2hex($k) . "=" . $v . ","; } echo strlen($s), " ", md5($s), "\n"; }
/// ```
/// ```expect
/// array(5) {
///   ["""]=>
///   string(6) "&quot;"
///   ["&"]=>
///   string(5) "&amp;"
///   ["'"]=>
///   string(6) "&#039;"
///   ["<"]=>
///   string(4) "&lt;"
///   [">"]=>
///   string(4) "&gt;"
/// }
/// array(3) {
///   ["&"]=>
///   string(5) "&amp;"
///   ["<"]=>
///   string(4) "&lt;"
///   [">"]=>
///   string(4) "&gt;"
/// }
/// int(253)
/// int(1511)
/// int(126)
/// int(5)
/// int(132)
/// 23957 a24923ff13839c775201d7d99805084a
/// 3366 b8a7e231d7e307797575f157ed6555bd
/// 146 f8534ec3c87cb4084d31c3e7bdf3ede1
/// 1752 f4a2e7e2d09c5ae2fa4f45a57784af3d
/// 1318 5c3e4860155f13a6163e6e8f79e1ac59
/// 1289 0d3ed27bb00aa7cf7011fbaa31282b12
/// ```
#[php_function]
fn get_html_translation_table(
    req: &mut Request,
    #[php(default = HTML_SPECIALCHARS)] table: i64,
    #[php(default = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401)] flags: i64,
    #[php(default = "UTF-8")] encoding: Passed<PhpString>,
) -> Result<Owner<PhpArray>, Thrown> {
    let all = table;
    // php's `charset_hint` stays NULL where the call gave no `$encoding`,
    // so the ini's charset and not the shown default is what it reads.
    let hint = match encoding {
        Passed::Given(hint) => Some(hint),
        Passed::Omitted => None,
    };
    let charset = determine_charset(req, hint.as_ref().map(PhpString::as_bytes), false);
    let doctype = flags & DOC_TYPE_MASK;
    let all = all != 0 && !charset.partial() && doctype != DOC_XML1;
    let table = entity_table(all, doctype);
    let mut array = PhpArray::new(req)?;
    let quote_skipped = |cp: u32| {
        (cp == u32::from(b'\'') && flags & QUOTE_SINGLE == 0)
            || (cp == u32::from(b'"') && flags & QUOTE_DOUBLE == 0)
    };
    if all && charset.unicode_compat() {
        // ISO-8859-1 walks the first 256 code points, UTF-8 all of
        // them.
        let limit = if charset.single_byte() {
            0x100
        } else {
            u32::MAX
        };
        for (cp, row) in table {
            if *cp >= limit || quote_skipped(*cp) {
                continue;
            }
            let mut key = Vec::new();
            write_octets(&mut key, charset, *cp);
            write_row(req, &mut array, row, &key, charset)?;
        }
    } else if all {
        let to_uni = to_unicode(charset).unwrap_or(&tables::TO_UNI_CP1252);
        for byte in 0..=255u8 {
            let cp = u32::from(byte);
            if quote_skipped(cp) {
                continue;
            }
            let uni = u32::from(to_uni[usize::from(byte)]);
            if let Some(row) = row_of(table, uni) {
                write_row(req, &mut array, row, &[byte], charset)?;
            }
        }
    } else {
        for (cp, row) in table {
            if quote_skipped(*cp) {
                continue;
            }
            let key = [u8::try_from(*cp).unwrap_or(0)];
            write_row(req, &mut array, row, &key, Charset::Iso88591)?;
        }
    }
    Ok(array)
}
