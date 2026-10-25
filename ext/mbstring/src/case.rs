//! `php_unicode.c`: Unicode properties and case mapping over the tables
//! `ucgendat` generated into `unicode_data.h`, and the three natives that
//! convert case.

use crate::ffi::mbfl::{Encoder, Encoding};
use crate::settings::{Substitute, encoding};
use crate::tables::{
    ENCODING_8859_9, UCCASE_EXTRA_TABLE, UCCASE_FOLD_G, UCCASE_FOLD_TABLE, UCCASE_LOWER_G,
    UCCASE_LOWER_TABLE, UCCASE_TITLE_G, UCCASE_TITLE_TABLE, UCCASE_UPPER_G, UCCASE_UPPER_TABLE,
    UCPROP_OFFSETS, UCPROP_RANGES,
};
use crate::text::{new_string, on_heap, substitute, value_error};
use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString};

/// `php_case_mode`, in its order, which is `MB_CASE_*`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaseMode {
    Upper,
    Lower,
    Title,
    Fold,
    UpperSimple,
    LowerSimple,
    TitleSimple,
    FoldSimple,
}

impl CaseMode {
    const ALL: [CaseMode; 8] = [
        CaseMode::Upper,
        CaseMode::Lower,
        CaseMode::Title,
        CaseMode::Fold,
        CaseMode::UpperSimple,
        CaseMode::LowerSimple,
        CaseMode::TitleSimple,
        CaseMode::FoldSimple,
    ];
}

/// `UC_*` of `php_unicode.h`, the properties this crate asks.
#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum Prop {
    Cased = 35,
    CaseIgnorable = 36,
}

/// `prop_lookup`: whether `code` is in one of the property's ranges,
/// a binary search over range pairs.
pub(crate) fn has_prop(code: u32, prop: Prop) -> bool {
    let n = prop as usize;
    let mut l = i64::from(UCPROP_OFFSETS[n]);
    let mut r = i64::from(UCPROP_OFFSETS[n + 1]) - 1;
    while l <= r {
        let mut m = (l + r) >> 1;
        m -= m & 1;
        let at = usize::try_from(m).unwrap_or(0);
        if code > UCPROP_RANGES[at + 1] {
            l = m + 2;
        } else if code < UCPROP_RANGES[at] {
            r = m - 2;
        } else {
            return true;
        }
    }
    false
}

fn is_cased(c: u32) -> bool {
    has_prop(c, Prop::Cased)
}

fn is_case_ignorable(c: u32) -> bool {
    has_prop(c, Prop::CaseIgnorable)
}

/// `mph_hash`.
fn mph_hash(d: u32, x: u32) -> u32 {
    let x = x ^ d;
    ((x >> 16) ^ x).wrapping_mul(0x045d_9f3b)
}

/// `mph_lookup` over one pair of tables: the mapping, or `None`.
fn mph_lookup(code: u32, g: &[i16], table: &[u32]) -> Option<u32> {
    let size = u32::try_from(table.len() / 2).ok()?;
    let g = g[(mph_hash(0, code) % u32::try_from(g.len()).ok()?) as usize];
    let idx = if g <= 0 {
        u32::from(g.unsigned_abs())
    } else {
        mph_hash(u32::from(g.unsigned_abs()), code) % size
    } as usize;
    (table.get(2 * idx) == Some(&code)).then(|| table[2 * idx + 1])
}

fn is_turkish(enc: Encoding) -> bool {
    enc.no() == ENCODING_8859_9
}

/// `php_unicode_toupper_raw`: above `0xFFFFFF`, an index into the extra
/// table of a special casing.
fn toupper_raw(code: u32, enc: Encoding) -> u32 {
    if code < 0xB5 {
        if (0x61..=0x7A).contains(&code) {
            return if is_turkish(enc) && code == 0x69 {
                0x130
            } else {
                code - 0x20
            };
        }
        return code;
    }
    mph_lookup(code, &UCCASE_UPPER_G, &UCCASE_UPPER_TABLE).unwrap_or(code)
}

/// `php_unicode_tolower_raw`.
fn tolower_raw(code: u32, enc: Encoding) -> u32 {
    if code < 0xC0 {
        if (0x41..=0x5A).contains(&code) {
            return if is_turkish(enc) && code == 0x49 {
                0x131
            } else {
                code + 0x20
            };
        }
        return code;
    }
    match mph_lookup(code, &UCCASE_LOWER_G, &UCCASE_LOWER_TABLE) {
        Some(_) if is_turkish(enc) && code == 0x130 => 0x69,
        Some(new) => new,
        None => code,
    }
}

/// `php_unicode_totitle_raw`.
fn totitle_raw(code: u32, enc: Encoding) -> u32 {
    mph_lookup(code, &UCCASE_TITLE_G, &UCCASE_TITLE_TABLE).unwrap_or_else(|| toupper_raw(code, enc))
}

/// `php_unicode_tofold_raw`.
fn tofold_raw(code: u32, enc: Encoding) -> u32 {
    if code < 0x80 {
        if (0x41..=0x5A).contains(&code) {
            return if is_turkish(enc) && code == 0x49 {
                0x131
            } else {
                code + 0x20
            };
        }
        return code;
    }
    match mph_lookup(code, &UCCASE_FOLD_G, &UCCASE_FOLD_TABLE) {
        Some(_) if is_turkish(enc) && code == 0x130 => 0x69,
        Some(new) => new,
        None => code,
    }
}

/// A raw mapping made simple: a special casing's first codepoint.
fn simple(code: u32) -> u32 {
    if code > 0xFF_FFFF {
        UCCASE_EXTRA_TABLE
            .get((code & 0xFF_FFFF) as usize)
            .copied()
            .unwrap_or(code)
    } else {
        code
    }
}

/// `emit_special_casing_sequence`, or the codepoint itself.
fn emit(w: u32, out: &mut Vec<u32>) {
    if w > 0xFF_FFFF {
        let at = (w & 0xFF_FFFF) as usize;
        let len = (w >> 24) as usize;
        out.extend(UCCASE_EXTRA_TABLE.iter().skip(at + 1).take(len));
    } else {
        out.push(w);
    }
}

/// `scan_ahead_for_cased_letter`: from where decoding stands.
fn scan_ahead(rest: &[u8], state: u32, enc: Encoding) -> bool {
    let mut found = false;
    let mut state = state;
    let mut input = rest;
    let mut buf = [0u32; 64];
    while !input.is_empty() {
        let n = enc.decode(&mut input, &mut buf, &mut state);
        for &w in &buf[..n] {
            if is_cased(w) {
                found = true;
                return found;
            }
            if !is_case_ignorable(w) {
                return found;
            }
        }
    }
    found
}

/// `scan_back_for_cased_letter`: over what the last batch left in the
/// converted buffer past where this batch has written.
fn scan_back(converted: &[u32], from: usize, end: Option<usize>) -> bool {
    let Some(end) = end else { return false };
    for &w in converted.get(from..end).unwrap_or_default().iter().rev() {
        if is_cased(w) {
            return true;
        }
        if !is_case_ignorable(w) {
            return false;
        }
    }
    false
}

/// Whether a capital sigma at `i` of `run` ends a word: preceded by a
/// cased letter through case-ignorables, and not followed by one.
fn final_sigma(
    run: &[u32],
    i: usize,
    back: impl FnOnce() -> bool,
    ahead: impl FnOnce() -> bool,
) -> bool {
    let before = run[..i].iter().rev().find(|&&w| !is_case_ignorable(w));
    if !before.map_or_else(back, |&w| is_cased(w)) {
        return false;
    }
    match run[i + 1..].iter().find(|&&w| !is_case_ignorable(w)) {
        Some(&w) => !is_cased(w),
        None => !ahead(),
    }
}

/// `php_unicode_convert_case`: `input` decoded from `from`, mapped in
/// `mode` and encoded to `to`, 64 codepoints at a time.
pub(crate) fn convert(
    mode: CaseMode,
    input: &[u8],
    from: Encoding,
    to: Encoding,
    sub: Substitute,
) -> Vec<u8> {
    let mut out = Encoder::new(to, input.len() + 1, sub.char, sub.raw_mode());
    let mut title = false;
    let mut converted: Vec<u32> = Vec::new();
    let mut converted_end: Option<usize> = None;
    let mut rest = input;
    let mut state = 0;
    let mut buf = [0u32; 64];
    while !rest.is_empty() {
        let n = from.decode(&mut rest, &mut buf, &mut state);
        let run = &buf[..n];
        let mut next: Vec<u32> = Vec::with_capacity(192);
        for (i, &w) in run.iter().enumerate() {
            if w > 0xFF_FFFF {
                next.push(w);
                continue;
            }
            let back = |p: usize| scan_back(&converted, p, converted_end);
            match mode {
                CaseMode::UpperSimple => next.push(simple(toupper_raw(w, from))),
                CaseMode::LowerSimple => next.push(simple(tolower_raw(w, from))),
                CaseMode::FoldSimple => next.push(simple(tofold_raw(w, from))),
                CaseMode::TitleSimple => {
                    next.push(simple(if title {
                        tolower_raw(w, from)
                    } else {
                        totitle_raw(w, from)
                    }));
                    if !is_case_ignorable(w) {
                        title = is_cased(w);
                    }
                }
                CaseMode::Upper => emit(toupper_raw(w, from), &mut next),
                CaseMode::Fold => emit(tofold_raw(w, from), &mut next),
                CaseMode::Lower => {
                    let p = next.len();
                    if w == 0x3A3
                        && final_sigma(run, i, || back(p), || scan_ahead(rest, state, from))
                    {
                        next.push(0x3C2);
                    } else {
                        emit(tolower_raw(w, from), &mut next);
                    }
                }
                CaseMode::Title => {
                    let p = next.len();
                    if title
                        && w == 0x3A3
                        && final_sigma(run, i, || back(p), || scan_ahead(rest, state, from))
                    {
                        next.push(0x3C2);
                    } else {
                        emit(
                            if title {
                                tolower_raw(w, from)
                            } else {
                                totitle_raw(w, from)
                            },
                            &mut next,
                        );
                    }
                    if !is_case_ignorable(w) {
                        title = is_cased(w);
                    }
                }
            }
        }
        out.write(&next, rest.is_empty());
        // php writes each run over the last in one buffer, so a backward
        // scan past where this run has written reads the last run's.
        converted_end = Some(next.len());
        converted = next;
    }
    out.into_vec()
}

/// `mb_convert_case(string $string, int $mode, ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_convert_case("hello wORLD", MB_CASE_TITLE), mb_convert_case("straße", MB_CASE_UPPER), mb_convert_case("straße", MB_CASE_UPPER_SIMPLE), mb_convert_case("ΣΑΣ ΣΑΣ", MB_CASE_LOWER), mb_convert_case("İI", MB_CASE_LOWER, "ISO-8859-9") === "i\xfd", mb_convert_case("ǆ", MB_CASE_FOLD));
/// try { mb_convert_case("a", 8); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(11) "Hello World"
/// string(7) "STRASSE"
/// string(7) "STRAßE"
/// string(13) "σας σας"
/// bool(false)
/// string(2) "ǆ"
/// mb_convert_case(): Argument #2 ($mode) must be one of the MB_CASE_* constants
/// ```
#[php_function]
fn mb_convert_case(
    req: &mut Request,
    string: PhpString,
    mode: i64,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let enc = self::encoding(req, encoding, 3)?;
    let Some(mode) = usize::try_from(mode)
        .ok()
        .and_then(|m| CaseMode::ALL.get(m))
    else {
        return Err(value_error(
            req,
            2,
            "must be one of the MB_CASE_* constants",
        ));
    };
    let sub = substitute(req);
    let out = on_heap(req, || convert(*mode, string.as_bytes(), enc, enc, sub))?;
    new_string(req, &out)
}

/// `mb_strtoupper(string $string, ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_strtoupper("añb ß"), bin2hex(mb_strtoupper("\xe1", "ISO-8859-1")), mb_strtoupper("a\xffb"));
/// ```
/// ```expect
/// string(7) "AÑB SS"
/// string(2) "c1"
/// string(3) "A?B"
/// ```
///
/// The conversion's buffer is the request's and grows fallibly since 8.5.12,
/// so a conversion past the memory limit is refused its growth and the
/// limit's fatal is raised as the call returns. The case crosses the limit
/// where a new chunk is asked for, whose figure holds in any layout (#3442):
///
/// ```php
/// ini_set('memory_limit', '2M');
/// var_dump(strlen(mb_strtoupper(str_repeat("\u{0390}", 400000), "UTF-8")), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 3
/// ```
#[php_function]
fn mb_strtoupper(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let enc = self::encoding(req, encoding, 2)?;
    let sub = substitute(req);
    let out = on_heap(req, || {
        convert(CaseMode::Upper, string.as_bytes(), enc, enc, sub)
    })?;
    new_string(req, &out)
}

/// `mb_strtolower(string $string, ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_strtolower("AÑB İ"), mb_strtolower("ΟΔΟΣ"));
/// ```
/// ```expect
/// string(8) "añb i̇"
/// string(8) "οδος"
/// ```
#[php_function]
fn mb_strtolower(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let enc = self::encoding(req, encoding, 2)?;
    let sub = substitute(req);
    let out = on_heap(req, || {
        convert(CaseMode::Lower, string.as_bytes(), enc, enc, sub)
    })?;
    new_string(req, &out)
}
