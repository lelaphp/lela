//! `sanitizing_filters.c` and `callback_filter.c`: the sanitizers, each
//! the string with bytes stripped, encoded or kept by a map, which
//! never fail, and `FILTER_CALLBACK`, the string through a callable.

use php_ext::call::call;
use php_ext::coerce::{is_callable, to_callable};
use php_ext::raising::{Thrown, type_error};
use php_ext::request::Request;
use php_ext::values::{FreshString, Owner, SmartStr};

use crate::constants::{
    FILTER_FLAG_ALLOW_FRACTION, FILTER_FLAG_ALLOW_SCIENTIFIC, FILTER_FLAG_ALLOW_THOUSAND,
    FILTER_FLAG_EMPTY_STRING_NULL, FILTER_FLAG_ENCODE_AMP, FILTER_FLAG_ENCODE_HIGH,
    FILTER_FLAG_ENCODE_LOW, FILTER_FLAG_NO_ENCODE_QUOTES, FILTER_FLAG_STRIP_BACKTICK,
    FILTER_FLAG_STRIP_HIGH, FILTER_FLAG_STRIP_LOW,
};
use crate::{Outcome, Run, string_value};

const LOWALPHA: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
const HIALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGIT: &[u8] = b"0123456789";

/// A sanitized string as the outcome.
fn value(req: &mut Request, bytes: &[u8]) -> Result<Outcome, Thrown> {
    Ok(Outcome::Value(string_value(req, bytes)?))
}

/// `php_filter_strip`: bytes from 127 up under `FILTER_FLAG_STRIP_HIGH`,
/// below 32 under `FILTER_FLAG_STRIP_LOW`, and the backtick under
/// `FILTER_FLAG_STRIP_BACKTICK`, dropped.
fn strip(s: &[u8], flags: i64) -> Vec<u8> {
    s.iter()
        .copied()
        .filter(|&c| {
            !((c >= 127 && flags & FILTER_FLAG_STRIP_HIGH != 0)
                || (c < 32 && flags & FILTER_FLAG_STRIP_LOW != 0)
                || (c == b'`' && flags & FILTER_FLAG_STRIP_BACKTICK != 0))
        })
        .collect()
}

/// The encoding map `enc[256]` the HTML sanitizers build: the quotes
/// unless `FILTER_FLAG_NO_ENCODE_QUOTES` (with `quotes`), `&` under
/// `FILTER_FLAG_ENCODE_AMP`, below 32 under `FILTER_FLAG_ENCODE_LOW`, and
/// from 127 up under `FILTER_FLAG_ENCODE_HIGH`.
fn html_map(flags: i64, quotes: bool) -> [bool; 256] {
    let mut map = [false; 256];
    if quotes && flags & FILTER_FLAG_NO_ENCODE_QUOTES == 0 {
        map[usize::from(b'\'')] = true;
        map[usize::from(b'"')] = true;
    }
    if flags & FILTER_FLAG_ENCODE_AMP != 0 {
        map[usize::from(b'&')] = true;
    }
    if flags & FILTER_FLAG_ENCODE_LOW != 0 {
        map[..32].fill(true);
    }
    if flags & FILTER_FLAG_ENCODE_HIGH != 0 {
        map[127..].fill(true);
    }
    map
}

/// `php_filter_encode_html`: each byte the map names as `&#N;`, onto a
/// `smart_str` on the request's heap in php's appends, the bytes before
/// it, `&#`, the number and `;`; and since 8.5.12 `None` where the memory
/// limit was met after a byte (`ZEND_HAS_OOM()`), what was written given
/// up, for the caller's `ZVAL_EMPTY_STRING`. Nothing is allocated for an
/// empty string.
fn encode_html(req: &mut Request, s: &[u8], map: &[bool; 256]) -> Result<Option<SmartStr>, Thrown> {
    let mut out = SmartStr::default();
    if s.is_empty() {
        return Ok(Some(out));
    }
    let mut last = 0;
    for (at, &c) in s.iter().enumerate() {
        if map[usize::from(c)] {
            out.append(req, &s[last..at])?;
            out.append(req, b"&#")?;
            out.append(req, c.to_string().as_bytes())?;
            out.append(req, b";")?;
            last = at + 1;
        }
        if req.memory().has_oom() {
            out.free(req);
            return Ok(None);
        }
    }
    out.append(req, &s[last..])?;
    Ok(Some(out))
}

/// The value `php_filter_encode_html` leaves: `smart_str_extract` of
/// what it wrote, or the empty string where the memory limit stopped it.
fn encoded_value(req: &mut Request, encoded: Option<SmartStr>) -> Result<Outcome, Thrown> {
    match encoded {
        Some(out) => Ok(Outcome::Value(out.extract(req)?.into_value())),
        None => value(req, b""),
    }
}

/// `filter_map_apply`: the bytes the allowed list names, kept.
fn keep(s: &[u8], allowed: &[&[u8]]) -> Vec<u8> {
    s.iter()
        .copied()
        .filter(|c| allowed.iter().any(|set| set.contains(c)))
        .collect()
}

/// `php_filter_string`, `FILTER_SANITIZE_STRING`: stripped, the quotes
/// and the flagged bytes encoded, then `php_strip_tags_ex` with tag
/// spaces allowed; nothing left is the empty string, or `null` under
/// `FILTER_FLAG_EMPTY_STRING_NULL`.
///
/// ```php
/// var_dump(filter_var("<b>It's</b> \"q\" & < b>x", 513), filter_var("a'b", 513, FILTER_FLAG_NO_ENCODE_QUOTES), filter_var("<br>", 513), filter_var("<br>", 513, FILTER_FLAG_EMPTY_STRING_NULL), filter_var("a&b\x01\xff", 513, FILTER_FLAG_ENCODE_AMP | FILTER_FLAG_ENCODE_LOW | FILTER_FLAG_ENCODE_HIGH));
/// ```
/// ```expect
/// string(24) "It&#39;s &#34;q&#34; & x"
/// string(3) "a'b"
/// string(0) ""
/// NULL
/// string(17) "a&#38;b&#1;&#255;"
/// ```
///
/// Since 8.5.12 the encoding stops once the memory limit was met, and the
/// limit's fatal is raised as the call returns.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(@filter_var(str_repeat("'", 2 << 20), 513), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 3
/// ```
pub(crate) fn string(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let stripped = strip(s, run.flags);
    // `return FAILURE` where the memory limit stopped the encoding, since
    // 8.5.12.
    let Some(mut encoded) = encode_html(req, &stripped, &html_map(run.flags, true))? else {
        return Ok(Outcome::Failed);
    };
    let out = php_ext_standard::strip_tags_ex(encoded.as_bytes(), None, true);
    encoded.free(req);
    if out.is_empty() && run.flags & FILTER_FLAG_EMPTY_STRING_NULL != 0 {
        return Ok(Outcome::Value(Owner::null()));
    }
    value(req, &out)
}

/// `php_filter_encoded`, `FILTER_SANITIZE_ENCODED`: stripped, then every
/// byte but the letters, the digits and `-._` as `%XX`.
///
/// ```php
/// var_dump(filter_var("a b&c/é-._~", FILTER_SANITIZE_ENCODED), filter_var("a\x01\xffb", FILTER_SANITIZE_ENCODED, FILTER_FLAG_STRIP_LOW | FILTER_FLAG_STRIP_HIGH));
/// ```
/// ```expect
/// string(24) "a%20b%26c%2F%C3%A9-._%7E"
/// string(2) "ab"
/// ```
///
/// The string of three bytes for each is asked for fallibly since 8.5.12:
/// past the memory limit it is refused, and the limit's fatal is raised
/// as the call returns.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(filter_var(str_repeat(' ', 3 << 20), FILTER_SANITIZE_ENCODED), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 9453568 bytes) in %s on line 3
/// ```
pub(crate) fn encoded(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let stripped = strip(s, run.flags);
    // `php_filter_encode_url`'s string of three bytes for each, asked for
    // fallibly since 8.5.12 (`zend_string_safe_falloc`), the value the
    // empty string where the memory limit refused it; the string keeps
    // that room, cut to what was written only in its length.
    let room = stripped.len().saturating_mul(3);
    let Some(mut out) = req
        .memory_mut()
        .fallible(|req| FreshString::new(req, room))?
    else {
        return value(req, b"");
    };
    let mut len = 0;
    for c in stripped {
        if c.is_ascii_alphanumeric() || b"-._".contains(&c) {
            out.bytes_mut()[len] = c;
            len += 1;
        } else {
            let hex = b"0123456789ABCDEF";
            out.bytes_mut()[len..len + 3].copy_from_slice(&[
                b'%',
                hex[usize::from(c >> 4)],
                hex[usize::from(c & 15)],
            ]);
            len += 3;
        }
    }
    out.set_len(len);
    Ok(Outcome::Value(out.finish().into_value()))
}

/// `php_filter_special_chars`, `FILTER_SANITIZE_SPECIAL_CHARS`: stripped,
/// then `'"<>&`, NUL and every byte below 32 as `&#N;`, and from 127 up
/// under `FILTER_FLAG_ENCODE_HIGH`.
///
/// ```php
/// var_dump(bin2hex(filter_var("<a href='x'>&\"\t\x7f\xe9", FILTER_SANITIZE_SPECIAL_CHARS)), filter_var("\xe9\x01", FILTER_SANITIZE_SPECIAL_CHARS, FILTER_FLAG_ENCODE_HIGH | FILTER_FLAG_STRIP_LOW));
/// ```
/// ```expect
/// string(88) "262336303b6120687265663d262333393b78262333393b262336323b262333383b262333343b2623393b7fe9"
/// string(6) "&#233;"
/// ```
///
/// Since 8.5.12 the encoding stops once the memory limit was met, and the
/// limit's fatal is raised as the call returns.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(filter_var(str_repeat("\xFF", 2 << 20), FILTER_SANITIZE_SPECIAL_CHARS, FILTER_FLAG_ENCODE_HIGH), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 3
/// ```
pub(crate) fn special_chars(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let mut map = html_map(run.flags & FILTER_FLAG_ENCODE_HIGH, false);
    for c in *b"'\"<>&" {
        map[usize::from(c)] = true;
    }
    map[..32].fill(true);
    let encoded = encode_html(req, &strip(s, run.flags), &map)?;
    encoded_value(req, encoded)
}

/// `php_filter_full_special_chars`, `FILTER_SANITIZE_FULL_SPECIAL_CHARS`:
/// `php_escape_html_entities_ex` with every entity, `ENT_QUOTES` unless
/// `FILTER_FLAG_NO_ENCODE_QUOTES`, the default charset, no double
/// encoding.
///
/// ```php
/// var_dump(filter_var("<é> & &amp; 'q' \"d\"", FILTER_SANITIZE_FULL_SPECIAL_CHARS), filter_var("'\"", FILTER_SANITIZE_FULL_SPECIAL_CHARS, FILTER_FLAG_NO_ENCODE_QUOTES), filter_var("a\x80b", FILTER_SANITIZE_FULL_SPECIAL_CHARS));
/// ```
/// ```expect
/// string(56) "&lt;&eacute;&gt; &amp; &amp; &#039;q&#039; &quot;d&quot;"
/// string(2) "'""
/// string(0) ""
/// ```
pub(crate) fn full_special_chars(
    req: &mut Request,
    run: &Run,
    s: &[u8],
) -> Result<Outcome, Thrown> {
    let quotes = if run.flags & FILTER_FLAG_NO_ENCODE_QUOTES == 0 {
        3
    } else {
        0
    };
    let out = php_ext_standard::escape_html_entities(req, s, true, quotes, None, false)?;
    Ok(Outcome::Value(out.into_value()))
}

/// `php_filter_unsafe_raw`, `FILTER_UNSAFE_RAW` and `FILTER_DEFAULT`: the
/// string as it is, unless the flags strip or encode, and the empty
/// string `null` under `FILTER_FLAG_EMPTY_STRING_NULL`.
///
/// ```php
/// var_dump(filter_var("a&b\x01`"), filter_var("a&b\x01`", FILTER_UNSAFE_RAW, FILTER_FLAG_ENCODE_AMP | FILTER_FLAG_STRIP_LOW | FILTER_FLAG_STRIP_BACKTICK), filter_var("", FILTER_UNSAFE_RAW, FILTER_FLAG_EMPTY_STRING_NULL), filter_var("\xe9", FILTER_UNSAFE_RAW, FILTER_FLAG_ENCODE_HIGH));
/// ```
/// ```expect
/// string(5) "a&b`"
/// string(7) "a&#38;b"
/// NULL
/// string(6) "&#233;"
/// ```
///
/// Since 8.5.12 the encoding stops once the memory limit was met, and the
/// limit's fatal is raised as the call returns.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(filter_var(str_repeat("\xFF", 2 << 20), FILTER_UNSAFE_RAW, FILTER_FLAG_ENCODE_HIGH), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 3
/// ```
pub(crate) fn unsafe_raw(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    if run.flags != 0 && !s.is_empty() {
        let map = html_map(run.flags, false);
        let encoded = encode_html(req, &strip(s, run.flags), &map)?;
        return encoded_value(req, encoded);
    }
    if s.is_empty() && run.flags & FILTER_FLAG_EMPTY_STRING_NULL != 0 {
        return Ok(Outcome::Value(Owner::null()));
    }
    value(req, s)
}

/// `php_filter_email`, `FILTER_SANITIZE_EMAIL`: the letters, digits and
/// ``!#$%&'*+-=?^_`{|}~@.[]`` of RFC 822's section 6, kept.
///
/// ```php
/// var_dump(filter_var("(a b)@exa<m>ple.com\xe9", FILTER_SANITIZE_EMAIL));
/// ```
/// ```expect
/// string(14) "ab@example.com"
/// ```
pub(crate) fn email(req: &mut Request, _: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let out = keep(s, &[LOWALPHA, HIALPHA, DIGIT, b"!#$%&'*+-=?^_`{|}~@.[]"]);
    value(req, &out)
}

/// The bytes `php_filter_url` keeps, section 5 of RFC 1738, which
/// `FILTER_VALIDATE_URL` also holds a URL to: one it would strip fails.
///
/// ```php
/// var_dump(filter_var("http://a.b/\x7f", FILTER_VALIDATE_URL), filter_var("http://a.b/é", FILTER_VALIDATE_URL), filter_var("http://a.b/{}|^", FILTER_VALIDATE_URL));
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// string(15) "http://a.b/{}|^"
/// ```
pub(crate) fn url_bytes(s: &[u8]) -> Vec<u8> {
    keep(
        s,
        &[
            LOWALPHA,
            HIALPHA,
            DIGIT,
            b"$-_.+",
            b"!*'(),",
            b"{}|\\^~[]`",
            b"<>#%\"",
            b";/?:@&=",
        ],
    )
}

/// `php_filter_url`, `FILTER_SANITIZE_URL`.
///
/// ```php
/// var_dump(filter_var("http://ex ample.com/ä?q=1\t#x", FILTER_SANITIZE_URL));
/// ```
/// ```expect
/// string(25) "http://example.com/?q=1#x"
/// ```
pub(crate) fn url(req: &mut Request, _: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    value(req, &url_bytes(s))
}

/// `php_filter_number_int`, `FILTER_SANITIZE_NUMBER_INT`: the digits and
/// the signs, kept.
///
/// ```php
/// var_dump(filter_var("a-1b2.3e+4", FILTER_SANITIZE_NUMBER_INT));
/// ```
/// ```expect
/// string(6) "-123+4"
/// ```
pub(crate) fn number_int(req: &mut Request, _: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    value(req, &keep(s, &[b"+-", DIGIT]))
}

/// `php_filter_number_float`, `FILTER_SANITIZE_NUMBER_FLOAT`: the digits
/// and the signs, `.` under `FILTER_FLAG_ALLOW_FRACTION`, `,` under
/// `FILTER_FLAG_ALLOW_THOUSAND` and `eE` under
/// `FILTER_FLAG_ALLOW_SCIENTIFIC`, kept.
///
/// ```php
/// var_dump(filter_var("-1,234.5e6x", FILTER_SANITIZE_NUMBER_FLOAT), filter_var("-1,234.5e6x", FILTER_SANITIZE_NUMBER_FLOAT, FILTER_FLAG_ALLOW_FRACTION | FILTER_FLAG_ALLOW_THOUSAND | FILTER_FLAG_ALLOW_SCIENTIFIC));
/// ```
/// ```expect
/// string(7) "-123456"
/// string(10) "-1,234.5e6"
/// ```
pub(crate) fn number_float(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let mut allowed: Vec<&[u8]> = vec![b"+-", DIGIT];
    for (flag, set) in [
        (FILTER_FLAG_ALLOW_FRACTION, b".".as_slice()),
        (FILTER_FLAG_ALLOW_THOUSAND, b","),
        (FILTER_FLAG_ALLOW_SCIENTIFIC, b"eE"),
    ] {
        if run.flags & flag != 0 {
            allowed.push(set);
        }
    }
    value(req, &keep(s, &allowed))
}

/// `php_filter_add_slashes`, `FILTER_SANITIZE_ADD_SLASHES`:
/// `php_addslashes`, a backslash before `'`, `"` and `\`, and NUL as
/// `\0`.
///
/// ```php
/// var_dump(filter_var("O'Re\"il\\ly\0", FILTER_SANITIZE_ADD_SLASHES));
/// ```
/// ```expect
/// string(15) "O\'Re\"il\\ly\0"
/// ```
pub(crate) fn add_slashes(req: &mut Request, _: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let mut out = Vec::with_capacity(s.len() * 2);
    for &c in s {
        match c {
            0 => out.extend_from_slice(b"\\0"),
            b'\'' | b'"' | b'\\' => out.extend_from_slice(&[b'\\', c]),
            _ => out.push(c),
        }
    }
    value(req, &out)
}

/// `php_filter_callback`, `FILTER_CALLBACK`: the `options` a callable,
/// called with the string, its answer the value; anything else is php's
/// `TypeError`. The flags are cleared, so an array is walked and each
/// element called.
///
/// ```php
/// var_dump(filter_var("abc", FILTER_CALLBACK, ["options" => "strtoupper"]), filter_var(["a", ["b"]], FILTER_CALLBACK, ["options" => fn($s) => "<$s>"]), filter_var(5, FILTER_CALLBACK, ["options" => function ($s) { var_dump($s); }]));
/// try { filter_var("a", FILTER_CALLBACK, ["options" => "no_such_function"]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { filter_var("a", FILTER_CALLBACK); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "5"
/// string(3) "ABC"
/// array(2) {
///   [0]=>
///   string(3) "<a>"
///   [1]=>
///   array(1) {
///     [0]=>
///     string(3) "<b>"
///   }
/// }
/// NULL
/// filter_var(): Option must be a valid callback
/// filter_var(): Option must be a valid callback
/// ```
pub(crate) fn callback(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let Some(options) = run.options.filter(|o| is_callable(req, o)) else {
        return Err(type_error(
            req,
            &format!("{}(): Option must be a valid callback", run.native),
        ));
    };
    let callable = to_callable(req, &options)?;
    let argument = string_value(req, s)?;
    let answer = call(req, &callable, &[*argument]);
    argument.give_up(req);
    Ok(Outcome::Value(answer?))
}
