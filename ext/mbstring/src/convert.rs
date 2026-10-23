//! `mbstring.c`'s conversions between encodings: `mb_convert_encoding`
//! and its recursive and by-reference forms, detection by demerits
//! (`mb_guess_encoding_for_strings`), validity, the encoding lists, kana
//! conversion and numeric entities.

use std::ops::ControlFlow;

use crate::ffi::mbfl::{Encoder, Encoding, fast_convert, kana};
use crate::settings::{State, Substitute, encoding, encodings, settings, text_encodings};
use crate::tables::{
    MBFL_BAD_INPUT, MBFL_HAN2ZEN_ALL, MBFL_HAN2ZEN_ALPHA, MBFL_HAN2ZEN_HIRAGANA,
    MBFL_HAN2ZEN_KATAKANA, MBFL_HAN2ZEN_NUMERIC, MBFL_ZEN2HAN_ALL, MBFL_ZEN2HAN_ALPHA,
    MBFL_ZEN2HAN_HIRAGANA, MBFL_ZEN2HAN_KATAKANA, MBFL_ZEN2HAN_NUMERIC, MBFL_ZENKAKU_HIRA2KATA,
    MBFL_ZENKAKU_KATA2HIRA, RARE_CODEPOINT_BITVEC,
};
use crate::text::{decode_runs, new_string, substitute, value_error};
use php_ext::abi::{OrFalse, Passed, Ref, Union, Variadic};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, call_stack_end, docref, fatal, pending};
use php_ext::request::Request;
use php_ext::values::walk::{Guarded, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value, ValueRef, walk};

/// `mb_fast_convert` under a substitute, with the errors met.
pub(crate) fn convert(
    input: &[u8],
    from: Encoding,
    to: Encoding,
    sub: Substitute,
) -> (Vec<u8>, u32) {
    fast_convert(input, from, to, sub.char, sub.raw_mode())
}

/// `php_mb_convert_encoding_ex`: the request's substitute, its errors
/// counted into `illegalchars`.
pub(crate) fn convert_counted(
    req: &mut Request,
    input: &[u8],
    from: Encoding,
    to: Encoding,
) -> Vec<u8> {
    let sub = substitute(req);
    let (out, errors) = convert(input, from, to, sub);
    req.state::<State>().illegal_chars += u64::from(errors);
    out
}

/// `php_mb_convert_encoding`: one source, or the one detection picks,
/// `None` after php's warning where none is picked.
fn convert_from(
    req: &mut Request,
    input: &[u8],
    to: Encoding,
    from: &[Encoding],
) -> Option<Vec<u8>> {
    let from = if let [one] = from {
        *one
    } else {
        let strict = settings(req).strict_detection;
        let Some(from) = guess(&[input], from, strict, true) else {
            docref(req, Level::Warning, "Unable to detect character encoding");
            return None;
        };
        from
    };
    Some(convert_counted(req, input, from, to))
}

/// `php_mb_convert_encoding_recursive`: keys and strings converted,
/// scalars kept, arrays walked, objects dropped after a warning.
///
/// php asks no stack limit here, as [`check_array`]'s walk asks none,
/// so this walk is held to the thread's floor and a nest php converts
/// whole under the small-stack regime is converted whole.
///
/// ```php
/// $a = ["\xe9"]; for ($i = 0; $i < 3000; $i++) { $a = [$a]; }
/// $out = mb_convert_encoding($a, 'UTF-8', 'ISO-8859-1');
/// for ($i = 0; $i < 3000; $i++) { $out = $out[0]; }
/// var_dump($out);
/// ```
/// ```expect regime=small-stack
/// array(1) {
///   [0]=>
///   string(2) "é"
/// }
/// ```
fn convert_array(
    req: &mut Request,
    input: PhpArray,
    to: Encoding,
    from: &[Encoding],
) -> Result<Option<Owner<PhpArray>>, Thrown> {
    call_stack_end(req)?;
    if input.is_recursive() {
        input.unprotect();
        docref(
            req,
            Level::Warning,
            "Cannot convert recursively referenced values",
        );
        return Ok(None);
    }
    input.protect();
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_KEY_VAL(input, idx, key, entry)`
    // (`Z/ext/mbstring/mbstring.c:2762`), no owner of the table taken: the
    // argument holds it while a warning reaches the program. A throwable
    // gives the array made so far up, and the table its mark, as every
    // other way out does.
    let walked = walk::foreach(req, input, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let value = row.value().deref();
        let key = match key {
            Key::Str(name) => match convert_from(req, name.as_bytes(), to, from) {
                Some(name) => new_string(req, &name)?.into_key(),
                None => return Ok(ControlFlow::Continue(())),
            },
            Key::Int(n) => Owner::index(n),
        };
        let value = value.deref();
        let converted = match value.as_ref() {
            ValueRef::Str(s) => match convert_from(req, s.as_bytes(), to, from) {
                Some(s) => new_string(req, &s).map(|s| Some(s.into_value())),
                None => Ok(None),
            },
            ValueRef::Null | ValueRef::Bool(_) | ValueRef::Int(_) | ValueRef::Float(_) => {
                Ok(Some(Owner::take(req, value)))
            }
            ValueRef::Array(inner) => {
                convert_array(req, inner, to, from).map(|array| {
                    Some(match array {
                        Some(array) => array.into_value(),
                        // `ZVAL_EMPTY_ARRAY` (`Z/ext/mbstring/mbstring.c:2799`).
                        None => Owner::take(req, Value::array(PhpArray::empty())),
                    })
                })
            }
            _ => {
                docref(req, Level::Warning, "Object is not supported");
                Ok(None)
            }
        };
        // The key made for an element left out is given up with it.
        let converted = match converted {
            Ok(Some(converted)) => converted,
            Ok(None) => {
                key.give_up(req);
                return Ok(ControlFlow::Continue(()));
            }
            Err(thrown) => {
                key.give_up(req);
                return Err(thrown);
            }
        };
        out.set(req, key, converted)?;
        Ok(ControlFlow::Continue(()))
    });
    input.unprotect();
    if let Err(thrown) = walked {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(Some(out))
}

/// `mb_convert_encoding(array|string $string, string $to_encoding, array|string|null $from_encoding = null): array|string|false`.
///
/// ```php
/// var_dump(bin2hex(mb_convert_encoding("añ", "UTF-16BE")), mb_convert_encoding("\xe1", "UTF-8", "ISO-8859-1"), mb_convert_encoding("€", "ISO-8859-1"), mb_convert_encoding(["k\xe1" => "\xe1", 1 => 2.5], "UTF-8", "ISO-8859-1"), mb_convert_encoding("\xa4\xa2", "UTF-8", "EUC-JP,SJIS"));
/// try { mb_convert_encoding("a", "UTF-8", "BASE64,QPrint"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(8) "006100f1"
/// string(2) "á"
/// string(1) "?"
/// array(2) {
///   ["ká"]=>
///   string(2) "á"
///   [1]=>
///   float(2.5)
/// }
/// string(3) "あ"
/// mb_convert_encoding(): Argument #3 ($from_encoding) must specify at least one encoding
/// ```
///
/// An array met again inside itself is converted to the process's empty
/// array after the warning.
///
/// ```php
/// $a = [1, [2]];
/// $a[1][] = &$a;
/// debug_zval_dump(mb_convert_encoding($a, 'UTF-8', 'UTF-8')[1]);
/// ```
/// ```expect
/// Warning: mb_convert_encoding(): Cannot convert recursively referenced values in %s on line 4
/// array(2) packed refcount(1){
///   [0]=>
///   int(2)
///   [1]=>
///   array(0) interned {
///   }
/// }
/// ```
///
/// The answer is the string `mb_fast_convert` allocates, so one byte
/// and nothing are strings of the request's, 32 bytes each, where a cut
/// (`mb_substr` over a fixed-width encoding) and `mb_chr` below 0x80 are
/// the process's interned ones.
///
/// ```php
/// $w = mb_convert_encoding('x', 'UTF-8', 'UTF-8'); unset($w);
/// $m = memory_get_usage(); $s = mb_convert_encoding('x', 'UTF-8', 'UTF-8'); $e = mb_convert_encoding('', 'UTF-16LE', 'UTF-8');
/// var_dump(memory_get_usage() - $m); debug_zval_dump(mb_convert_encoding(['x'], 'UTF-8', 'UTF-8'), $s, $e, mb_substr('xy', 1, 1, 'ASCII'), mb_chr(0x41));
/// ```
/// ```expect
/// int(64)
/// array(1) packed refcount(1){
///   [0]=>
///   string(1) "x" refcount(1)
/// }
/// string(1) "x" refcount(2)
/// string(0) "" refcount(2)
/// string(1) "y" interned
/// string(1) "A" interned
/// ```
///
/// Each malformed byte, and a sequence cut short, is one substitute,
/// counted in `mb_get_info('illegal_chars')`, as is a character the
/// target lacks, which a `long` substitute spells as its codepoint; a
/// stateful target ends in its initial shift state, and a stateful
/// source cut off inside a character ends in one substitute.
///
/// ```php
/// mb_internal_encoding('UTF-8');
/// mb_substitute_character(0x3f);
/// foreach (["\xf0\x9f", "\xc0\xaf", "\xed\xa0\x80", "\xe2\x28\xa1"] as $bytes) {
///     echo bin2hex(mb_convert_encoding($bytes, 'UTF-8', 'UTF-8')), ' ', mb_get_info('illegal_chars'), "\n";
/// }
/// echo bin2hex(mb_convert_encoding('あ', 'JIS', 'UTF-8')), "\n";
/// echo bin2hex(mb_convert_encoding("\x1b\x24\x42\x30", 'UTF-8', 'JIS')), ' ', mb_get_info('illegal_chars'), "\n";
/// echo bin2hex(mb_convert_encoding("\x81\x30\x81", 'UTF-8', 'GB18030-2022')), ' ', mb_get_info('illegal_chars'), "\n";
/// mb_substitute_character('long');
/// echo mb_convert_encoding('€', 'ASCII', 'UTF-8'), ' ', mb_get_info('illegal_chars'), "\n";
/// ```
/// ```expect
/// 3f 1
/// 3f3f 3
/// 3f3f3f 6
/// 3f283f 8
/// 1b244224221b2842
/// 3f 9
/// 3f 10
/// U+20AC 11
/// ```
///
/// An array's element the conversion refuses warns, and the handler the
/// warning calls may convert again and change the substitute, which the
/// elements after it then use.
///
/// ```php
/// mb_substitute_character(0x3f);
/// set_error_handler(function ($severity, $message) {
///     echo $message, "\n";
///     mb_substitute_character(0x23);
///     echo bin2hex(mb_convert_encoding("\xff", 'UTF-8', 'UTF-8')), "\n";
///     return true;
/// });
/// $out = mb_convert_encoding([new stdClass(), "\xff"], 'UTF-8', 'UTF-8');
/// restore_error_handler();
/// echo bin2hex($out[1]), ' ', mb_get_info('illegal_chars'), "\n";
/// ```
/// ```expect
/// mb_convert_encoding(): Object is not supported
/// 23
/// 23 2
/// ```
///
/// A handler that throws ends the array's conversion and gives up what
/// was converted before it, leaving the input as it was.
///
/// ```php
/// set_error_handler(function () { throw new Exception('reentered'); });
/// $input = ["\xe9" => "\xe9", new stdClass()];
/// try { mb_convert_encoding($input, 'UTF-8', 'ISO-8859-1'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// restore_error_handler();
/// echo count($input), "\n";
/// ```
/// ```expect
/// reentered
/// 2
/// ```
#[php_function]
// The declared `array|string|false`, each fresh value an owner.
#[allow(clippy::type_complexity)]
fn mb_convert_encoding(
    req: &mut Request,
    string: Union<PhpArray, PhpString>,
    to_encoding: PhpString,
    #[php(default = null)] from_encoding: Option<Union<PhpArray, PhpString>>,
) -> Result<OrFalse<Union<Owner<PhpArray>, Owner<PhpString>>>, Thrown> {
    let to = self::encoding(req, Some(to_encoding), 2)?;
    let mut from = match from_encoding {
        Some(given) => encodings(req, given, 3)?,
        None => vec![settings(req).current_internal],
    };
    if from.len() > 1 {
        text_encodings(&mut from);
    }
    if from.is_empty() {
        return Err(value_error(req, 3, "must specify at least one encoding"));
    }
    match string {
        Union::B(s) => match convert_from(req, s.as_bytes(), to, &from) {
            Some(out) => Ok(OrFalse::Value(Union::B(new_string(req, &out)?))),
            None => Ok(OrFalse::False),
        },
        Union::A(array) => match convert_array(req, array, to, &from)? {
            Some(out) => Ok(OrFalse::Value(Union::A(out))),
            None => Ok(OrFalse::False),
        },
    }
}

/// `estimate_demerits`: many for a rare codepoint, a few for ASCII
/// punctuation, one for the rest.
fn demerits(w: u32) -> u64 {
    if w > 0xFFFF {
        40
    } else if (0x21..=0x2F).contains(&w) {
        6
    } else if (RARE_CODEPOINT_BITVEC[(w >> 5) as usize] >> (w & 0x1F)) & 1 != 0 {
        30
    } else {
        1
    }
}

/// One candidate of `mb_guess_encoding_for_strings`.
struct Candidate<'a> {
    enc: Encoding,
    input: &'a [u8],
    state: u32,
    demerits: u64,
    multiplier: f32,
}

/// `start_string`: the input, past a byte order mark the encoding writes.
fn start<'a>(candidates: &mut [Candidate<'a>], input: &'a [u8]) {
    for c in candidates.iter_mut() {
        let bom: &[u8] = match c.enc {
            e if e == Encoding::utf8() => b"\xEF\xBB\xBF",
            e if e.no() == crate::tables::ENCODING_UTF16BE => b"\xFE\xFF",
            e if e.no() == crate::tables::ENCODING_UTF16LE => b"\xFF\xFE",
            _ => b"",
        };
        c.input = input
            .strip_prefix(bom)
            .filter(|_| !bom.is_empty())
            .unwrap_or(input);
    }
}

/// `count_demerits`: each candidate's input decoded, 128 codepoints a
/// turn, last candidate first; strictly, a candidate with bad input is
/// dropped. The count is php's `uint64_t`: weighed by the multiplier
/// once per string, it reaches `UINT64_MAX` over a few hundred strings,
/// and the next string's demerits wrap it round as C's unsigned
/// addition does (GH-17503).
// php weighs the demerits in `double` and clamps them back into a
// `uint64_t`, which these casts are.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn count_demerits(candidates: &mut Vec<Candidate<'_>>, strict: bool) {
    let mut buf = [0u32; 128];
    let mut finished = candidates.iter().filter(|c| c.input.is_empty()).count();
    while (strict || candidates.len() > 1) && finished < candidates.len() {
        let mut i = candidates.len();
        while i > 0 {
            i -= 1;
            let c = &mut candidates[i];
            if c.input.is_empty() {
                continue;
            }
            let n = c.enc.decode(&mut c.input, &mut buf, &mut c.state);
            let mut dropped = false;
            for &w in buf[..n].iter().rev() {
                if w == MBFL_BAD_INPUT {
                    if strict {
                        dropped = true;
                        break;
                    }
                    c.demerits = c.demerits.wrapping_add(1000);
                } else {
                    c.demerits = c.demerits.wrapping_add(demerits(w));
                }
            }
            if dropped {
                candidates.remove(i);
                continue;
            }
            if candidates[i].input.is_empty() {
                finished += 1;
            }
        }
    }
    for c in candidates.iter_mut() {
        let weighted = c.demerits as f64 * f64::from(c.multiplier);
        c.demerits = if weighted < u64::MAX as f64 {
            weighted as u64
        } else {
            u64::MAX
        };
    }
}

/// `mb_guess_encoding_for_strings`: the candidate with the fewest
/// demerits over every string, the earlier where `order_significant`
/// weighs them.
pub(crate) fn guess(
    strings: &[&[u8]],
    list: &[Encoding],
    strict: bool,
    order_significant: bool,
) -> Option<Encoding> {
    match list {
        [] => return None,
        [one] => {
            return (!strict || strings.iter().all(|s| check(s, *one))).then_some(*one);
        }
        _ => {}
    }
    if let [only] = strings
        && only.is_empty()
    {
        return list.first().copied();
    }
    let length = list.len();
    let mut candidates = Vec::new();
    for (i, &enc) in list.iter().enumerate() {
        let mut demerits = 0;
        if enc.has_check() {
            let mut skip = false;
            for s in strings {
                if enc.check(s) == Some(false) {
                    if strict {
                        skip = true;
                        break;
                    }
                    demerits += 500;
                }
            }
            if skip {
                continue;
            }
        }
        // `1.0 + ((0.3 * i) / length)` in `double`, stored in a `float`.
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let multiplier = if order_significant {
            (1.0 + (0.3 * i as f64) / length as f64) as f32
        } else {
            1.0
        };
        candidates.push(Candidate {
            enc,
            input: b"",
            state: 0,
            demerits,
            multiplier,
        });
    }
    for s in strings.iter().rev() {
        start(&mut candidates, s);
        count_demerits(&mut candidates, strict);
        if candidates.is_empty() {
            return None;
        }
    }
    let mut best = 0;
    for i in 1..candidates.len() {
        if candidates[i].demerits < candidates[best].demerits {
            best = i;
        }
    }
    Some(candidates[best].enc)
}

/// `mb_detect_encoding(string $string, array|string|null $encodings = null, bool $strict = false): string|false`.
///
/// ```php
/// var_dump(mb_detect_encoding("añ"), mb_detect_encoding("abc", "ASCII,UTF-8"), mb_detect_encoding("\xe1", "UTF-8,ISO-8859-1"), mb_detect_encoding("\xe1", "UTF-8", true), mb_detect_encoding("\xa4\xa2\xa4\xa4", ["SJIS", "EUC-JP"]), mb_detect_encoding("x", "BASE64"));
/// try { mb_detect_encoding("a", []); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(5) "UTF-8"
/// string(5) "ASCII"
/// string(10) "ISO-8859-1"
/// bool(false)
/// string(6) "EUC-JP"
/// bool(false)
/// mb_detect_encoding(): Argument #2 ($encodings) must specify at least one encoding
/// ```
#[php_function]
fn mb_detect_encoding(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] encodings: Option<Union<PhpArray, PhpString>>,
    #[php(default = false)] strict: Passed<bool>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let mut order_significant = true;
    let mut list = match encodings {
        Some(Union::A(array)) => {
            if req
                .state::<State>()
                .all_encodings
                .as_deref()
                .is_some_and(|all| all.is_same(array))
            {
                order_significant = false;
            }
            crate::settings::encoding_array(req, array, 2)?
        }
        Some(Union::B(names)) => {
            crate::settings::encoding_list(req, names.as_bytes(), Some(2))?.unwrap_or_default()
        }
        None => settings(req).detect_order.clone(),
    };
    if list.is_empty() {
        return Err(value_error(req, 2, "must specify at least one encoding"));
    }
    text_encodings(&mut list);
    if list.is_empty() {
        return Ok(OrFalse::False);
    }
    let strict = match strict {
        Passed::Given(strict) => strict,
        Passed::Omitted => settings(req).strict_detection,
    };
    let found = if list == [Encoding::utf8()]
        && (string.is_valid_utf8() || std::str::from_utf8(string.as_bytes()).is_ok())
    {
        Some(Encoding::utf8())
    } else {
        guess(&[string.as_bytes()], &list, strict, order_significant)
    };
    match found {
        Some(enc) => Ok(OrFalse::Value(new_string(req, enc.name())?)),
        None => Ok(OrFalse::False),
    }
}

/// `mb_list_encodings(): array`: one array for the request, so that
/// detection can tell it was handed the whole list.
///
/// ```php
/// $l = mb_list_encodings(); var_dump(count($l), $l[0], in_array("UTF-8", $l), $l === mb_list_encodings());
/// ```
/// ```expect
/// int(79)
/// string(6) "BASE64"
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn mb_list_encodings(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    if let Some(all) = req.state::<State>().all_encodings.as_deref().copied() {
        return Ok(Owner::take(req, all));
    }
    let mut all = PhpArray::new(req)?;
    for enc in Encoding::all() {
        let name = new_string(req, enc.name())?;
        all.append(req, name.into_value())?;
    }
    let kept = Owner::take(req, *all);
    if let Some(old) = req.state::<State>().all_encodings.replace(kept) {
        old.give_up(req);
    }
    Ok(all)
}

/// `mb_encoding_aliases(string $encoding): array`.
///
/// ```php
/// var_dump(mb_encoding_aliases("ASCII"), mb_encoding_aliases("UTF-16BE"));
/// try { mb_encoding_aliases("nope"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(11) {
///   [0]=>
///   string(14) "ANSI_X3.4-1968"
///   [1]=>
///   string(8) "iso-ir-6"
///   [2]=>
///   string(14) "ANSI_X3.4-1986"
///   [3]=>
///   string(16) "ISO_646.irv:1991"
///   [4]=>
///   string(8) "US-ASCII"
///   [5]=>
///   string(9) "ISO646-US"
///   [6]=>
///   string(2) "us"
///   [7]=>
///   string(6) "IBM367"
///   [8]=>
///   string(7) "IBM-367"
///   [9]=>
///   string(5) "cp367"
///   [10]=>
///   string(7) "csASCII"
/// }
/// array(0) {
/// }
/// mb_encoding_aliases(): Argument #1 ($encoding) must be a valid encoding, "nope" given
/// ```
#[php_function]
fn mb_encoding_aliases(req: &mut Request, encoding: PhpString) -> Result<Owner<PhpArray>, Thrown> {
    let enc = self::encoding(req, Some(encoding), 1)?;
    let mut out = PhpArray::new(req)?;
    for alias in enc.aliases() {
        let alias = new_string(req, alias)?;
        out.append(req, alias.into_value())?;
    }
    Ok(out)
}

/// `php_mb_check_encoding`: the encoding's own check where it has one,
/// else no bad input decoded, the first eight codepoints tried first.
pub(crate) fn check(input: &[u8], enc: Encoding) -> bool {
    if let Some(valid) = enc.check(input) {
        return valid;
    }
    let mut valid = true;
    let mut first = true;
    let mut rest = input;
    let mut state = 0;
    let mut buf = [0u32; 128];
    while !rest.is_empty() || first {
        let size = if first { 8 } else { 128 };
        first = false;
        let n = enc.decode(&mut rest, &mut buf[..size], &mut state);
        if buf[..n].contains(&MBFL_BAD_INPUT) {
            valid = false;
            break;
        }
    }
    valid
}

/// `mb_check_str_encoding`.
fn check_string(s: PhpString, enc: Encoding) -> bool {
    if enc == Encoding::utf8() {
        return s.is_valid_utf8() || std::str::from_utf8(s.as_bytes()).is_ok();
    }
    check(s.as_bytes(), enc)
}

/// `php_mb_check_encoding_recursive`.
///
/// php asks no stack limit here and recurses to the thread's end, so a
/// nest deeper than a small `zend.max_allowed_stack_size` is checked
/// whole, and this walk is held to the thread's floor instead
/// ([`php_ext::raising::call_stack_end`]), which the small-stack
/// regime's 256K does not move; past the floor, where php faults, the
/// walk is that `Error` (`trace:stack-floor-refusal`).
///
/// ```php
/// $a = ["\xff"]; for ($i = 0; $i < 3000; $i++) { $a = [$a]; }
/// var_dump(mb_check_encoding($a, 'UTF-8'));
/// ```
/// ```expect regime=small-stack
/// bool(false)
/// ```
fn check_array(req: &mut Request, array: PhpArray, enc: Encoding) -> Result<bool, Thrown> {
    call_stack_end(req)?;
    // `GC_TRY_PROTECT_RECURSION(vars)` and `ZEND_HASH_FOREACH_KEY_VAL(vars,
    // …)` (`Z/ext/mbstring/mbstring.c:5643`): a guarded walk holding
    // nothing, the argument holding the table while a nested table's
    // circular warning reaches the program's handler.
    let walked = walk::foreach_guarded(req, array, Hold::None, |req, row| -> walk::Step {
        if let Key::Str(key) = row.key()
            && !check_string(*key, enc)
        {
            return Ok(ControlFlow::Break(()));
        }
        let value = row.value().deref();
        let ok = match value.as_ref() {
            ValueRef::Str(s) => check_string(s, enc),
            ValueRef::Array(inner) => check_array(req, inner, enc)?,
            ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Null | ValueRef::Bool(_) => true,
            _ => false,
        };
        Ok(if ok {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        })
    })?;
    Ok(match walked {
        Guarded::Walked(invalid) => invalid.is_none(),
        Guarded::Recursive => {
            docref(req, Level::Warning, "Cannot not handle circular references");
            false
        }
    })
}

/// `mb_check_encoding(array|string|null $value = null, ?string $encoding = null): bool`.
///
/// ```php
/// var_dump(mb_check_encoding("añ"), mb_check_encoding("a\xff"), mb_check_encoding("\x82\xa0", "SJIS"), mb_check_encoding(["a" => ["ñ"], 1 => 2]), mb_check_encoding(["\xff"]));
/// var_dump(mb_check_encoding());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
///
/// Deprecated: mb_check_encoding(): Calling mb_check_encoding() without argument is deprecated in %s on line 3
/// bool(true)
/// ```
#[php_function]
fn mb_check_encoding(
    req: &mut Request,
    #[php(default = null)] value: Option<Union<PhpArray, PhpString>>,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<bool, Thrown> {
    let enc = self::encoding(req, encoding, 2)?;
    match value {
        Some(Union::A(array)) => check_array(req, array, enc),
        Some(Union::B(s)) => Ok(check_string(s, enc)),
        None => {
            docref(
                req,
                Level::Deprecated,
                "Calling mb_check_encoding() without argument is deprecated",
            );
            Ok(req.state::<State>().illegal_chars == 0)
        }
    }
}

/// `mb_convert_kana_flags`, bit `i` the letter at `i`.
const KANA_FLAGS: [u8; 17] = *b"ARNSKHMCarnskhmcV";

/// `jp_kana_convert`: each codepoint with the one after it, the last of
/// a batch carried over to be read with the next.
fn kana_convert(input: &[u8], enc: Encoding, mode: u32, sub: Substitute) -> Vec<u8> {
    let mut out = Encoder::new(enc, input.len(), sub.char, sub.raw_mode());
    let mut buf = [0u32; 64];
    let mut offset = 0;
    let mut rest = input;
    let mut state = 0;
    while !rest.is_empty() {
        let n = enc.decode(&mut rest, &mut buf[offset..], &mut state) + offset;
        if n == 0 {
            continue;
        }
        let mut converted = Vec::with_capacity(128);
        let mut i = 0;
        let mut carried = true;
        while i + 1 < n {
            let (mut second, mut consumed) = (0, false);
            converted.push(kana(
                buf[i],
                buf[i + 1],
                Some(&mut consumed),
                &mut second,
                mode,
            ));
            if second != 0 {
                converted.push(second);
            }
            if consumed {
                i += 1;
                if i == n - 1 {
                    carried = false;
                    offset = 0;
                    break;
                }
            }
            i += 1;
        }
        if carried {
            if rest.is_empty() {
                let mut second = 0;
                converted.push(kana(buf[n - 1], 0, None, &mut second, mode));
                if second != 0 {
                    converted.push(second);
                }
            } else {
                buf[0] = buf[n - 1];
                offset = 1;
            }
        }
        out.write(&converted, rest.is_empty());
    }
    out.into_vec()
}

/// `mb_convert_kana(string $string, string $mode = "KV", ?string $encoding = null): string`.
///
/// ```php
/// var_dump(mb_convert_kana("ｶﾞｷﾞ abc", "KVa"), mb_convert_kana("カキ ＡＢＣ", "kas"), mb_convert_kana("あい", "C"));
/// try { mb_convert_kana("a", "Aa"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mb_convert_kana("a", "HK"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mb_convert_kana("a", "x"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(10) "ガギ abc"
/// string(10) "ｶｷ ABC"
/// string(6) "アイ"
/// mb_convert_kana(): Argument #2 ($mode) must not combine 'A' and 'a' flags
/// mb_convert_kana(): Argument #2 ($mode) must not combine 'H' and 'K' flags
/// mb_convert_kana(): Argument #2 ($mode) contains invalid flag: 'x'
/// ```
#[php_function]
fn mb_convert_kana(
    req: &mut Request,
    string: PhpString,
    #[php(default = "KV")] mode: PhpString,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let mut opt = 0u32;
    for &c in mode.as_bytes() {
        if c == b'A' {
            opt |= MBFL_HAN2ZEN_ALL | MBFL_HAN2ZEN_ALPHA | MBFL_HAN2ZEN_NUMERIC;
        } else if c == b'a' {
            opt |= MBFL_ZEN2HAN_ALL | MBFL_ZEN2HAN_ALPHA | MBFL_ZEN2HAN_NUMERIC;
        } else if let Some(i) = KANA_FLAGS.iter().position(|&f| f == c) {
            opt |= 1 << i;
        } else {
            return Err(value_error(
                req,
                2,
                &format!("contains invalid flag: '{}'", char::from(c)),
            ));
        }
    }
    let bad = ((opt & 0xFF00) >> 8) & opt;
    if bad != 0 {
        let i = bad.trailing_zeros() as usize;
        let mut flag1 = char::from(KANA_FLAGS[i]);
        let mut flag2 = char::from(KANA_FLAGS[i + 8]);
        if (flag1 == 'R' || flag1 == 'N') && opt & MBFL_HAN2ZEN_ALL != 0 {
            flag1 = 'A';
        }
        if (flag2 == 'r' || flag2 == 'n') && opt & MBFL_ZEN2HAN_ALL != 0 {
            flag2 = 'a';
        }
        return Err(value_error(
            req,
            2,
            &format!("must not combine '{flag1}' and '{flag2}' flags"),
        ));
    }
    let refuse = |a: char, b: char| format!("must not combine '{a}' and '{b}' flags");
    let refused = if opt & MBFL_HAN2ZEN_HIRAGANA != 0 && opt & MBFL_HAN2ZEN_KATAKANA != 0 {
        Some(refuse('H', 'K'))
    } else if opt & MBFL_ZEN2HAN_HIRAGANA != 0 {
        (opt & MBFL_ZENKAKU_HIRA2KATA != 0)
            .then(|| refuse('h', 'C'))
            .or_else(|| (opt & MBFL_ZENKAKU_KATA2HIRA != 0).then(|| refuse('h', 'c')))
    } else if opt & MBFL_ZEN2HAN_KATAKANA != 0 {
        (opt & MBFL_ZENKAKU_HIRA2KATA != 0)
            .then(|| refuse('k', 'C'))
            .or_else(|| (opt & MBFL_ZENKAKU_KATA2HIRA != 0).then(|| refuse('k', 'c')))
    } else {
        None
    };
    if let Some(text) = refused {
        return Err(value_error(req, 2, &text));
    }
    let enc = self::encoding(req, encoding, 3)?;
    let sub = substitute(req);
    let out = kana_convert(string.as_bytes(), enc, opt, sub);
    new_string(req, &out)
}

/// The strings under a value, arrays and objects walked, a recursion
/// stopping the walk (`mb_recursive_find_strings`).
fn find_strings(req: &mut Request, value: &Value, out: &mut Vec<Vec<u8>>) -> Result<bool, Thrown> {
    if req.limits().stack_overflowed() {
        php_ext::raising::call_stack_limit(req)?;
        return Ok(true);
    }
    let value = value.deref();
    match value.as_ref() {
        ValueRef::Str(s) => out.push(s.as_bytes().to_vec()),
        ValueRef::Array(array) => {
            // `Z_PROTECT_RECURSION_P(var)` and `ZEND_HASH_FOREACH_VAL_IND(ht,
            // entry)` (`Z/ext/mbstring/mbstring.c:3774`): a guarded walk
            // holding nothing, the argument holding the table.
            let walked = walk::foreach_guarded(req, array, Hold::None, |req, row| -> walk::Step {
                Ok(if find_strings(req, row.value(), out)? {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
            })?;
            match walked {
                Guarded::Recursive | Guarded::Walked(Some(())) => return Ok(true),
                Guarded::Walked(None) => {}
            }
        }
        ValueRef::Object(object) => {
            // `Z_PROTECT_RECURSION_P(var)` and `ZEND_HASH_FOREACH_VAL_IND`
            // over `HASH_OF(var)` (`Z/ext/mbstring/mbstring.c:3771`), the
            // object's `Z_OBJPROP`: a walk holding nothing, the argument
            // holding the object.
            if object.is_recursive() {
                return Ok(true);
            }
            object.protect();
            let walked = object.property_table(req).and_then(|table| {
                walk::foreach(req, table, Hold::None, |req, row| -> walk::Step {
                    Ok(if find_strings(req, row.value(), out)? {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    })
                })
            });
            object.unprotect();
            if walked?.is_some() {
                return Ok(true);
            }
        }
        _ => {}
    }
    Ok(false)
}

/// What converting one slot's value leaves, `mb_recursive_convert_variable`.
enum Walk {
    /// A recursion stopped the walk.
    Stopped,
    /// Converted in place, or nothing to convert.
    Kept,
    /// The slot takes this value, a string in place of the reference
    /// that held one, as php's `ZVAL_STR(orig_var, ..)` puts it.
    Replace(Owner<Value>),
}

/// `mb_recursive_convert_variable` over a slot's value: a string
/// converted, an array separated and walked, a reference's array
/// separated in place, an object's properties written in place.
///
/// Each array's recursion mark is cleared on every way out, a refusal
/// at the stack limit among them, as php's `GC_TRY_UNPROTECT_RECURSION`
/// clears it before each `return true`, so the nest a refused walk
/// stood in is walked whole by the next.
///
/// ```php
/// $a = ["x"]; for ($i = 0; $i < 3000; $i++) { $a = [$a]; }
/// $b = [$a];
/// try { mb_convert_variables('UTF-8', 'UTF-8', $a); } catch (Error $e) { echo get_class($e), "\n"; }
/// try { mb_convert_variables('UTF-8', 'UTF-8', $b); } catch (Error $e) { echo get_class($e), "\n"; }
/// var_dump(mb_check_encoding($a, 'UTF-8'), count(mb_convert_encoding($b, 'UTF-8', 'UTF-8')));
/// ```
/// ```expect regime=small-stack
/// Error
/// Error
/// bool(true)
/// int(1)
/// ```
fn convert_value(
    req: &mut Request,
    value: &Value,
    from: Encoding,
    to: Encoding,
) -> Result<Walk, Thrown> {
    if req.limits().stack_overflowed() {
        php_ext::raising::call_stack_limit(req)?;
        return Ok(Walk::Stopped);
    }
    match value.as_ref() {
        ValueRef::Reference(r) => {
            let inner = r.get();
            match inner.as_ref() {
                ValueRef::Array(array) if !array.is_recursive() => {
                    let original = array;
                    original.protect();
                    let walked = match r.separate_array(req) {
                        Ok(Some(separated)) => convert_array_in(req, separated, from, to),
                        Ok(None) => Ok(false),
                        Err(thrown) => Err(thrown),
                    };
                    original.unprotect();
                    Ok(if walked? { Walk::Stopped } else { Walk::Kept })
                }
                ValueRef::Array(_) => Ok(Walk::Stopped),
                _ => convert_value(req, &inner, from, to),
            }
        }
        ValueRef::Str(s) => {
            let out = convert_counted(req, s.as_bytes(), from, to);
            Ok(Walk::Replace(new_string(req, &out)?.into_value()))
        }
        ValueRef::Array(array) => {
            let original = array;
            if original.is_recursive() {
                return Ok(Walk::Stopped);
            }
            original.protect();
            let separated = match original.copy(req) {
                Ok(separated) => separated,
                Err(thrown) => {
                    original.unprotect();
                    return Err(thrown);
                }
            };
            // The copy has the one owner, so the walk's writes never
            // separate it and the view walked is the array it owns.
            let stopped = match convert_array_in(req, *separated, from, to) {
                Ok(stopped) => stopped,
                Err(thrown) => {
                    original.unprotect();
                    separated.give_up(req);
                    return Err(thrown);
                }
            };
            original.unprotect();
            if stopped {
                separated.give_up(req);
                return Ok(Walk::Stopped);
            }
            Ok(Walk::Replace(separated.into_value()))
        }
        ValueRef::Object(object) => {
            // `GC_TRY_PROTECT_RECURSION(ht)` and `ZEND_HASH_FOREACH_VAL`
            // over `HASH_OF(var)` (`Z/ext/mbstring/mbstring.c:3808`), the
            // object's `Z_OBJPROP`, no owner of it taken: the argument
            // holds the object, and each row is written where it stands,
            // a declared one in its slot.
            let table = object.property_table(req)?;
            let walked = walk::foreach_guarded(req, table, Hold::None, |req, row| {
                match convert_value(req, row.value(), from, to)? {
                    Walk::Stopped => return Ok(ControlFlow::Break(())),
                    Walk::Kept => {}
                    Walk::Replace(new) => object.set_row(req, row.key(), new)?,
                }
                Ok(ControlFlow::Continue(()))
            })?;
            Ok(match walked {
                Guarded::Recursive | Guarded::Walked(Some(())) => Walk::Stopped,
                Guarded::Walked(None) => Walk::Kept,
            })
        }
        _ => Ok(Walk::Kept),
    }
}

/// Each element of an array the walk owns converted in its slot; `true`
/// where a recursion stopped it. A converted string is written back
/// under the key it stood at, which the array keeps, as php's
/// `ZEND_HASH_FOREACH_VAL` (`Z/ext/mbstring/mbstring.c:3828`) writes
/// through the bucket and leaves its key alone: the walk's keys are
/// views, so it replaces in place and
/// never hands one to a write that takes a key's owner. The array is
/// the walk's own and nothing it serves removes a key, so a key gone
/// is the `InternalError` of a broken invariant.
fn convert_array_in(
    req: &mut Request,
    mut array: PhpArray,
    from: Encoding,
    to: Encoding,
) -> Result<bool, Thrown> {
    array.protect();
    // `ZEND_HASH_FOREACH_VAL(ht, entry)` (`Z/ext/mbstring/mbstring.c:3828`)
    // over the separated table, no owner of it taken: the variable holds it
    // while what a nested object's conversion reaches runs, and each row is
    // written in place as php writes it.
    let walked = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step<bool> {
        match convert_value(req, row.value(), from, to)? {
            Walk::Stopped => return Ok(ControlFlow::Break(true)),
            Walk::Kept => {}
            Walk::Replace(new) => {
                if !array.replace(req, row.key(), new)? {
                    return Err(fatal(
                        req,
                        "InternalError: mb_convert_variables lost a key of the array it converts",
                    ));
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    });
    array.unprotect();
    Ok(walked?.unwrap_or(false))
}

/// `mb_convert_variables(string $to_encoding, array|string $from_encoding, mixed &$var, mixed &...$vars): string|false`.
///
/// ```php
/// $a = "\xe1"; $b = ["\xe9", ["x" => "\xed"]]; var_dump(mb_convert_variables("UTF-8", "ISO-8859-1", $a, $b), $a, $b);
/// $c = "\xa4\xa2"; var_dump(mb_convert_variables("UTF-8", "SJIS,EUC-JP", $c), $c);
/// ```
/// ```expect
/// string(10) "ISO-8859-1"
/// string(2) "á"
/// array(2) {
///   [0]=>
///   string(2) "é"
///   [1]=>
///   array(1) {
///     ["x"]=>
///     string(2) "í"
///   }
/// }
/// string(6) "EUC-JP"
/// string(3) "あ"
/// ```
///
/// A converted element stays under its key, and the array keeps a key
/// built by concatenation: a string of the key's size allocated after the
/// conversion is another block.
///
/// ```php
/// $k = 'kkkkk'; $z = 'ZZZZZ';
/// $a = [$k . $k => "\xe9", 'n' => ["\xed"]];
/// mb_convert_variables('UTF-8', 'ISO-8859-1', $a);
/// $x = $z . $z;
/// var_dump($a);
/// ```
/// ```expect
/// array(2) {
///   ["kkkkkkkkkk"]=>
///   string(2) "é"
///   ["n"]=>
///   array(1) {
///     [0]=>
///     string(2) "í"
///   }
/// }
/// ```
///
/// An object is read through `HASH_OF`, its class's `get_properties`
/// (`Z/ext/mbstring/mbstring.c:3771`, `:3808`), so a class whose
/// readers see rows of its own making converts the table it holds:
///
/// ```php
/// $dt = new DateTime('2020-01-01 UTC');
/// mb_convert_variables('UTF-8', 'ISO-8859-1', $dt);
/// var_dump(get_object_vars($dt));
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
///
/// Detection over many strings weighs each candidate's demerits once a
/// string, past what a `uint64_t` holds, and counts on from where php's
/// arithmetic leaves them (GH-17503):
///
/// ```php
/// $a = array_fill(0, 500, "<blah>");
/// var_dump(mb_convert_variables("ASCII", ["UTF-8", "UTF-16"], $a));
/// $c = array_merge(array_fill(0, 300, "<a>"), ["\xff\xfe"]);
/// var_dump(mb_convert_variables("UTF-8", ["UTF-16", "UTF-8", "ISO-8859-1"], $c));
/// ```
/// ```expect
/// string(5) "UTF-8"
/// string(6) "UTF-16"
/// ```
#[php_function]
fn mb_convert_variables(
    req: &mut Request,
    to_encoding: PhpString,
    from_encoding: Union<PhpArray, PhpString>,
    var: Ref<Value>,
    vars: Variadic<Ref<Value>>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let to = self::encoding(req, Some(to_encoding), 1)?;
    let mut order_significant = true;
    if let Union::A(array) = &from_encoding
        && req
            .state::<State>()
            .all_encodings
            .as_deref()
            .is_some_and(|all| all.is_same(*array))
    {
        order_significant = false;
    }
    let list = encodings(req, from_encoding, 2)?;
    if list.is_empty() {
        return Err(value_error(req, 2, "must specify at least one encoding"));
    }
    let mut refs = vec![var];
    refs.extend(vars);
    let from = if let [one] = list[..] {
        one
    } else {
        let mut strings = Vec::new();
        for r in &refs {
            let value = r.reference().map_or_else(Value::null, |c| c.get());
            if find_strings(req, &value, &mut strings)? {
                pending(req)?;
                docref(req, Level::Warning, "Cannot handle recursive references");
                return Ok(OrFalse::False);
            }
        }
        let strict = settings(req).strict_detection;
        let views: Vec<&[u8]> = strings.iter().map(Vec::as_slice).collect();
        let Some(from) = guess(&views, &list, strict, order_significant) else {
            docref(req, Level::Warning, "Unable to detect encoding");
            return Ok(OrFalse::False);
        };
        from
    };
    for r in &refs {
        let Some(cell) = r.reference() else { continue };
        match convert_value(req, &Value::reference(cell), from, to)? {
            Walk::Stopped => {
                pending(req)?;
                docref(req, Level::Warning, "Cannot handle recursive references");
                return Ok(OrFalse::False);
            }
            Walk::Kept => {}
            Walk::Replace(new) => cell.set(req, new)?,
        }
    }
    Ok(OrFalse::Value(new_string(req, from.name())?))
}

/// `make_conversion_map`: four integers a range, `lo`, `hi`, `offset`
/// and `mask`.
fn conversion_map(req: &mut Request, map: PhpArray) -> Result<Vec<u32>, Thrown> {
    if !map.len().is_multiple_of(4) {
        return Err(value_error(req, 2, "must have a multiple of 4 elements"));
    }
    let longs: Option<Vec<i64>> = walk::rows(req, map)
        .map(|(_, value)| try_long(&value.deref()))
        .collect();
    let Some(longs) = longs else {
        return Err(value_error(
            req,
            2,
            "must only be composed of values of type int",
        ));
    };
    // `*mapelm++ = tmp`: the `zend_long` cut to a `uint32_t`.
    Ok(longs
        .into_iter()
        .map(|n| u32::try_from(n & 0xFFFF_FFFF).unwrap_or(0))
        .collect())
}

/// `zval_try_get_long`: an integer, a float in range, a numeric string,
/// `null` and the booleans; anything else fails.
fn try_long(value: &Value) -> Option<i64> {
    match value.as_ref() {
        ValueRef::Int(n) => Some(n),
        ValueRef::Bool(b) => Some(i64::from(b)),
        ValueRef::Null => Some(0),
        ValueRef::Float(f) => Some(php_ext::coerce::float_to_long(f)),
        ValueRef::Str(s) => match php_ext::coerce::numeric_string(s.as_bytes())?.as_ref() {
            ValueRef::Int(n) => Some(n),
            ValueRef::Float(f) => Some(php_ext::coerce::float_to_long(f)),
            _ => None,
        },
        _ => None,
    }
}

/// `html_numeric_entity_encode`.
fn entity_encode(input: &[u8], enc: Encoding, map: &[u32], hex: bool, sub: Substitute) -> Vec<u8> {
    let mut out = Encoder::new(enc, input.len(), sub.char, sub.raw_mode());
    decode_runs(enc, input, 32, |w, rest| {
        let mut converted = Vec::with_capacity(w.len() * 13);
        for &c in w {
            let range = map.chunks_exact(4).find(|m| c >= m[0] && c <= m[1]);
            let Some(m) = range else {
                converted.push(c);
                continue;
            };
            let value = c.wrapping_add(m[2]) & m[3];
            let digits = if hex {
                format!("#x{value:X}")
            } else {
                format!("#{value}")
            };
            converted.push(u32::from(b'&'));
            converted.extend(digits.bytes().map(u32::from));
            converted.push(u32::from(b';'));
        }
        out.write(&converted, rest.is_empty());
        true
    });
    out.into_vec()
}

/// `mb_encode_numericentity(string $string, array $map, ?string $encoding = null, bool $hex = false): string`.
///
/// ```php
/// $map = [0x80, 0x10ffff, 0, 0x1fffff];
/// var_dump(mb_encode_numericentity("añ€", $map), mb_encode_numericentity("añ€", $map, "UTF-8", true));
/// try { mb_encode_numericentity("a", [1, 2, 3]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(14) "a&#241;&#8364;"
/// string(15) "a&#xF1;&#x20AC;"
/// mb_encode_numericentity(): Argument #2 ($map) must have a multiple of 4 elements
/// ```
#[php_function]
fn mb_encode_numericentity(
    req: &mut Request,
    string: PhpString,
    map: PhpArray,
    #[php(default = null)] encoding: Option<PhpString>,
    #[php(default = false)] hex: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let enc = self::encoding(req, encoding, 3)?;
    let map = conversion_map(req, map)?;
    let sub = substitute(req);
    let out = entity_encode(string.as_bytes(), enc, &map, hex, sub);
    new_string(req, &out)
}

/// `html_numeric_entity_deconvert`.
fn deconvert(number: u32, map: &[u32]) -> Option<u32> {
    map.chunks_exact(4).find_map(|m| {
        let c = number.wrapping_sub(m[2]);
        (c >= m[0] && c <= m[1]).then_some(c)
    })
}

/// `html_numeric_entity_decode`: an entity split across two runs of
/// codepoints carried to the next, as php's buffer carries it.
#[allow(clippy::too_many_lines)] // one C function, kept whole
fn entity_decode(input: &[u8], enc: Encoding, map: &[u32], sub: Substitute) -> Vec<u8> {
    const DEC_MIN: usize = 3;
    const HEX_MIN: usize = 4;
    const DEC_MAX: usize = 12;
    const HEX_MAX: usize = 11;
    let amp = u32::from(b'&');
    let mut out = Encoder::new(enc, input.len(), sub.char, sub.raw_mode());
    let mut buf = [0u32; 128];
    let mut offset = 0usize;
    let mut rest = input;
    let mut state = 0;
    let is_digit = |w: u32| (u32::from(b'0')..=u32::from(b'9')).contains(&w);
    let is_hex = |w: u32| {
        is_digit(w)
            || (u32::from(b'A')..=u32::from(b'F')).contains(&w)
            || (u32::from(b'a')..=u32::from(b'f')).contains(&w)
    };
    while !rest.is_empty() {
        let n = enc.decode(&mut rest, &mut buf[offset..127], &mut state) + offset;
        buf[n] = amp;
        let more = !rest.is_empty();
        let mut converted: Vec<u32> = Vec::with_capacity(128);
        let mut p = if offset == 0 {
            let first = buf[..=n].iter().position(|&w| w == amp).unwrap_or(n);
            if first == n {
                out.write(&buf[..n], !more);
                continue;
            }
            converted.extend_from_slice(&buf[..first]);
            first
        } else {
            0
        };
        offset = 0;
        loop {
            let mut p2 = p + 1;
            let mut carry = None;
            if buf[p2] == u32::from(b'#') {
                p2 += 1;
                if buf[p2] == u32::from(b'x') {
                    p2 += 1;
                    while is_hex(buf[p2]) {
                        p2 += 1;
                    }
                    let len = p2 - p;
                    if p2 == n && more && len <= HEX_MAX {
                        carry = Some(len);
                    } else if !(HEX_MIN..=HEX_MAX).contains(&len) {
                        converted.extend_from_slice(&buf[p..p2]);
                    } else {
                        let value = buf[p + 3..p2].iter().fold(0u32, |v, &w| {
                            let d = if w <= u32::from(b'9') {
                                w - u32::from(b'0')
                            } else if w >= u32::from(b'a') {
                                10 + w - u32::from(b'a')
                            } else {
                                10 + w - u32::from(b'A')
                            };
                            v.wrapping_mul(16).wrapping_add(d)
                        });
                        match deconvert(value, map) {
                            Some(c) => {
                                converted.push(c);
                                if buf[p2] == u32::from(b';') {
                                    p2 += 1;
                                }
                            }
                            None => converted.extend_from_slice(&buf[p..p2]),
                        }
                    }
                } else {
                    while is_digit(buf[p2]) {
                        p2 += 1;
                    }
                    let len = p2 - p;
                    if p2 == n && more && len <= DEC_MAX {
                        carry = Some(len);
                    } else if !(DEC_MIN..=DEC_MAX).contains(&len) {
                        converted.extend_from_slice(&buf[p..p2]);
                    } else {
                        let mut value = 0u32;
                        let mut too_big = false;
                        for &w in &buf[p + 2..p2] {
                            if value > 0x1999_9999 {
                                too_big = true;
                                break;
                            }
                            value = value.wrapping_mul(10).wrapping_add(w - u32::from(b'0'));
                        }
                        if too_big {
                            converted.extend_from_slice(&buf[p..p2]);
                        } else {
                            match deconvert(value, map) {
                                Some(c) => {
                                    converted.push(c);
                                    if buf[p2] == u32::from(b';') {
                                        p2 += 1;
                                    }
                                }
                                None => converted.extend_from_slice(&buf[p..p2]),
                            }
                        }
                    }
                }
            } else if p2 == n && more {
                carry = Some(1);
            } else {
                converted.push(amp);
            }
            if let Some(len) = carry {
                buf.copy_within(p..p + len, 0);
                offset = len;
                break;
            }
            p = p2;
            let next = buf[p..=n]
                .iter()
                .position(|&w| w == amp)
                .map_or(n, |at| p + at);
            converted.extend_from_slice(&buf[p..next]);
            p = next;
            if p >= n {
                break;
            }
        }
        out.write(&converted, !more);
    }
    out.into_vec()
}

/// `mb_decode_numericentity(string $string, array $map, ?string $encoding = null): string`.
///
/// ```php
/// $map = [0x0, 0x10ffff, 0, 0x1fffff];
/// var_dump(mb_decode_numericentity("a&#241;&#x20AC;&#;&#x;&amp;", $map), mb_decode_numericentity("&#65", [0x40, 0x50, 0, 0xff]));
/// ```
/// ```expect
/// string(18) "añ€&#;&#x;&amp;"
/// string(1) "A"
/// ```
#[php_function]
fn mb_decode_numericentity(
    req: &mut Request,
    string: PhpString,
    map: PhpArray,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let enc = self::encoding(req, encoding, 3)?;
    let map = conversion_map(req, map)?;
    let sub = substitute(req);
    let out = entity_decode(string.as_bytes(), enc, &map, sub);
    new_string(req, &out)
}
