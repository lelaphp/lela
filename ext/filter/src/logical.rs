//! `logical_filters.c`: the validators. Each reads the string and
//! answers the value it denotes or [`Outcome::Failed`]; the options are
//! read as `FETCH_LONG_OPTION`, `FETCH_DOUBLE_OPTION` and
//! `FETCH_STRING_OPTION` read them, and a malformed option is php's
//! `ValueError` naming the native.

use php_ext::call::call;
use php_ext::coerce::{callable_checked, numeric_string, to_double, to_long};
use php_ext::raising::{Thrown, fatal, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, Value, ValueRef};
use php_ext_pcre::Regex;
use php_ext_uri::{Parser, ReadAs, UriStruct};

use crate::constants::{
    FILTER_FLAG_ALLOW_HEX, FILTER_FLAG_ALLOW_OCTAL, FILTER_FLAG_ALLOW_THOUSAND,
    FILTER_FLAG_EMAIL_UNICODE, FILTER_FLAG_GLOBAL_RANGE, FILTER_FLAG_HOSTNAME, FILTER_FLAG_IPV4,
    FILTER_FLAG_IPV6, FILTER_FLAG_NO_PRIV_RANGE, FILTER_FLAG_NO_RES_RANGE,
    FILTER_FLAG_PATH_REQUIRED, FILTER_FLAG_QUERY_REQUIRED,
};
use crate::{Outcome, Run, find, sanitizing, string_value};

/// `FETCH_LONG_OPTION`: `zval_get_long` of the option where it is set.
fn long_option(req: &mut Request, run: &Run, name: &[u8]) -> Result<Option<i64>, Thrown> {
    match find(run.options.as_ref(), name) {
        Some(value) => Ok(Some(to_long(req, &value.deref())?)),
        None => Ok(None),
    }
}

/// `FETCH_DOUBLE_OPTION`: `zval_get_double` of the option where it is set.
fn double_option(req: &mut Request, run: &Run, name: &[u8]) -> Result<Option<f64>, Thrown> {
    match find(run.options.as_ref(), name) {
        Some(value) => Ok(Some(to_double(req, &value.deref())?)),
        None => Ok(None),
    }
}

/// `FETCH_STRING_OPTION`: the option where it is set and a string.
fn string_option(run: &Run, name: &[u8]) -> Option<Vec<u8>> {
    find(run.options.as_ref(), name)?
        .deref()
        .as_str_bytes()
        .map(<[u8]>::to_vec)
}

/// `zend_value_error("%s(): ...", get_active_function_name())`.
fn value_error(req: &mut Request, run: &Run, message: &str) -> Thrown {
    throw_error(req, "ValueError", &format!("{}(): {message}", run.native))
}

/// The string unchanged, as a validator that passed leaves it.
fn same(req: &mut Request, s: &[u8]) -> Result<Outcome, Thrown> {
    Ok(Outcome::Value(string_value(req, s)?))
}

/// `PHP_FILTER_TRIM_DEFAULT_EX`: space, tab, CR, VT and LF off both ends.
fn trim(s: &[u8]) -> &[u8] {
    let blank = |c: &u8| matches!(c, b' ' | b'\t' | b'\r' | 0x0b | b'\n');
    let start = s.iter().position(|c| !blank(c)).unwrap_or(s.len());
    let end = s.iter().rposition(|c| !blank(c)).map_or(start, |at| at + 1);
    &s[start..end.max(start)]
}

/// `php_filter_parse_int`: an optional sign, then `1`-`9` and digits,
/// no more than `MAX_LENGTH_OF_LONG - 1` after the first, in range; a
/// lone `0` after the sign is zero.
fn parse_int(s: &[u8]) -> Option<i64> {
    let (negative, mut at) = match s.first() {
        Some(b'-') => (true, 1),
        Some(b'+') => (false, 1),
        _ => (false, 0),
    };
    if s.get(at) == Some(&b'0') && at + 1 == s.len() {
        return Some(0);
    }
    let first = *s.get(at).filter(|c| (b'1'..=b'9').contains(c))?;
    let mut value = i64::from(first - b'0');
    if negative {
        value = -value;
    }
    at += 1;
    if s.len() - at > 19 {
        return None;
    }
    for &c in &s[at..] {
        if !c.is_ascii_digit() {
            return None;
        }
        let digit = i64::from(c - b'0');
        if !negative && value <= (i64::MAX - digit) / 10 {
            value = value * 10 + digit;
        } else if negative && value >= (i64::MIN + digit) / 10 {
            value = value * 10 - digit;
        } else {
            return None;
        }
    }
    Some(value)
}

/// `php_filter_parse_octal` and `php_filter_parse_hex`: the digits of
/// the base as a `zend_ulong` that may not overflow, read back as a
/// `zend_long`.
fn parse_base(s: &[u8], base: u64) -> Option<i64> {
    let mut value: u64 = 0;
    for &c in s {
        let digit = u64::from(match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' if base == 16 => c - b'a' + 10,
            b'A'..=b'F' if base == 16 => c - b'A' + 10,
            _ => return None,
        });
        if digit >= base || value > u64::MAX / base || value * base > u64::MAX - digit {
            return None;
        }
        value = value * base + digit;
    }
    Some(value.cast_signed())
}

/// `php_filter_int`, `FILTER_VALIDATE_INT`: a decimal integer, with
/// `FILTER_FLAG_ALLOW_HEX` a `0x` one and with `FILTER_FLAG_ALLOW_OCTAL`
/// a `0` or `0o` one, inside `min_range` and `max_range`.
///
/// ```php
/// foreach (["12", " -7\n", "+0", "-0", "0", "007", "1e3", "9223372036854775807", "9223372036854775808", "-9223372036854775808", "", " "] as $s) var_dump(filter_var($s, FILTER_VALIDATE_INT));
/// var_dump(filter_var("0x1F", FILTER_VALIDATE_INT, FILTER_FLAG_ALLOW_HEX), filter_var("0x", FILTER_VALIDATE_INT, FILTER_FLAG_ALLOW_HEX), filter_var("0755", FILTER_VALIDATE_INT, FILTER_FLAG_ALLOW_OCTAL), filter_var("0o17", FILTER_VALIDATE_INT, FILTER_FLAG_ALLOW_OCTAL), filter_var("0xffffffffffffffff", FILTER_VALIDATE_INT, FILTER_FLAG_ALLOW_HEX));
/// var_dump(filter_var("5", FILTER_VALIDATE_INT, ["options" => ["min_range" => 6]]), filter_var("5", FILTER_VALIDATE_INT, ["options" => ["min_range" => "1", "max_range" => 5]]));
/// ```
/// ```expect
/// int(12)
/// int(-7)
/// int(0)
/// int(0)
/// int(0)
/// bool(false)
/// bool(false)
/// int(9223372036854775807)
/// bool(false)
/// int(-9223372036854775808)
/// bool(false)
/// bool(false)
/// int(31)
/// bool(false)
/// int(493)
/// int(15)
/// int(-1)
/// bool(false)
/// int(5)
/// ```
pub(crate) fn int(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let min = long_option(req, run, b"min_range")?;
    let max = long_option(req, run, b"max_range")?;
    let p = trim(s);
    if p.is_empty() {
        return Ok(Outcome::Failed);
    }
    let parsed = if let Some(rest) = p.strip_prefix(b"0") {
        if run.flags & FILTER_FLAG_ALLOW_HEX != 0 && matches!(rest.first(), Some(b'x' | b'X')) {
            match &rest[1..] {
                [] => return Ok(Outcome::Failed),
                digits => parse_base(digits, 16),
            }
        } else if run.flags & FILTER_FLAG_ALLOW_OCTAL != 0 {
            match rest.first() {
                Some(b'o' | b'O') if rest.len() == 1 => return Ok(Outcome::Failed),
                Some(b'o' | b'O') => parse_base(&rest[1..], 8),
                _ => parse_base(rest, 8),
            }
        } else if rest.is_empty() {
            Some(0)
        } else {
            None
        }
    } else {
        parse_int(p)
    };
    match parsed {
        Some(n) if min.is_none_or(|m| n >= m) && max.is_none_or(|m| n <= m) => {
            Ok(Outcome::Value(Owner::int(n)))
        }
        _ => Ok(Outcome::Failed),
    }
}

/// `php_filter_boolean`, `FILTER_VALIDATE_BOOL`: `1`, `true`, `on` and
/// `yes` are true, `0`, `false`, `off`, `no` and the empty string false,
/// without case, and anything else fails.
///
/// ```php
/// foreach (["1", " Yes ", "ON", "true", "0", "no", "OFF", "False", "", "  ", "2", "maybe"] as $s) var_dump(filter_var($s, FILTER_VALIDATE_BOOL, FILTER_NULL_ON_FAILURE));
/// var_dump(filter_var("off", FILTER_VALIDATE_BOOLEAN), filter_var("x", FILTER_VALIDATE_BOOLEAN), filter_var(false, FILTER_VALIDATE_BOOLEAN, ["options" => ["default" => "d"]]));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// NULL
/// NULL
/// bool(false)
/// bool(false)
/// string(1) "d"
/// ```
#[allow(clippy::unnecessary_wraps)] // the shape every entry of `filter_list` has
pub(crate) fn boolean(_: &mut Request, _: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let p = trim(s);
    let is = |word: &[u8]| p.eq_ignore_ascii_case(word);
    Ok(
        if p.is_empty() || is(b"0") || is(b"no") || is(b"off") || is(b"false") {
            Outcome::Value(Owner::bool(false))
        } else if is(b"1") || is(b"on") || is(b"yes") || is(b"true") {
            Outcome::Value(Owner::bool(true))
        } else {
            Outcome::Failed
        },
    )
}

/// `php_filter_float`, `FILTER_VALIDATE_FLOAT`: an optional sign,
/// digits, the `decimal` option's separator and a fraction, an exponent;
/// with `FILTER_FLAG_ALLOW_THOUSAND`, groups of three after the
/// `thousand` option's separators; the number `is_numeric_string` reads
/// it as, finite, inside `min_range` and `max_range`.
///
/// ```php
/// foreach (["1.5", " -2 ", "1e3", "1.", ".5", "1,000", "abc", "1e", "1e999", "1e-999", "0.0", "+.5e-2", ""] as $s) var_dump(filter_var($s, FILTER_VALIDATE_FLOAT));
/// var_dump(filter_var("1,000,000.5", FILTER_VALIDATE_FLOAT, FILTER_FLAG_ALLOW_THOUSAND), filter_var("1,00", FILTER_VALIDATE_FLOAT, FILTER_FLAG_ALLOW_THOUSAND), filter_var("1 000,5", FILTER_VALIDATE_FLOAT, ["options" => ["decimal" => ",", "thousand" => " "], "flags" => FILTER_FLAG_ALLOW_THOUSAND]));
/// var_dump(filter_var("5.5", FILTER_VALIDATE_FLOAT, ["options" => ["min_range" => 1, "max_range" => 5]]), filter_var("3", FILTER_VALIDATE_FLOAT, ["options" => ["min_range" => 1, "max_range" => 5]]));
/// try { filter_var("1", FILTER_VALIDATE_FLOAT, ["options" => ["decimal" => ".."]]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { filter_var("1", FILTER_VALIDATE_FLOAT, ["options" => ["thousand" => ""]]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// float(1.5)
/// float(-2)
/// float(1000)
/// float(1)
/// float(0.5)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// float(0)
/// float(0.005)
/// bool(false)
/// float(1000000.5)
/// bool(false)
/// float(1000.5)
/// bool(false)
/// float(3)
/// filter_var(): "decimal" option must be one character long
/// filter_var(): "thousand" option must not be empty
/// ```
pub(crate) fn float(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let text = trim(s);
    if text.is_empty() {
        return Ok(Outcome::Failed);
    }
    let mut decimal = b'.';
    if let Some(given) = string_option(run, b"decimal") {
        match given.as_slice() {
            [one] => decimal = *one,
            _ => {
                return Err(value_error(
                    req,
                    run,
                    "\"decimal\" option must be one character long",
                ));
            }
        }
    }
    let thousand = match string_option(run, b"thousand") {
        Some(given) if given.is_empty() => {
            return Err(value_error(
                req,
                run,
                "\"thousand\" option must not be empty",
            ));
        }
        Some(given) => php_ext::coerce::c_str(&given).to_vec(),
        None => b"',.".to_vec(),
    };
    let min = double_option(req, run, b"min_range")?;
    let max = double_option(req, run, b"max_range")?;
    let at = |pos: usize| text.get(pos).copied().unwrap_or(0);
    let end = text.len();
    let mut num = Vec::with_capacity(end);
    let mut pos = 0;
    if matches!(at(0), b'+' | b'-') {
        num.push(text[0]);
        pos = 1;
    }
    let digits = |num: &mut Vec<u8>, pos: &mut usize| {
        let mut n = 0;
        while *pos < end && text[*pos].is_ascii_digit() {
            num.push(text[*pos]);
            *pos += 1;
            n += 1;
        }
        n
    };
    let mut first = true;
    loop {
        let n = digits(&mut num, &mut pos);
        let byte = at(pos);
        if pos == end || byte == decimal || byte == b'e' || byte == b'E' {
            if !first && n != 3 {
                return Ok(Outcome::Failed);
            }
            if at(pos) == decimal {
                num.push(b'.');
                pos += 1;
                digits(&mut num, &mut pos);
            }
            if matches!(at(pos), b'e' | b'E') {
                num.push(at(pos));
                pos += 1;
                if pos < end && matches!(text[pos], b'+' | b'-') {
                    num.push(text[pos]);
                    pos += 1;
                }
                digits(&mut num, &mut pos);
            }
            break;
        }
        // `strchr(tsd_sep, *str)` finds the terminator for a NUL byte.
        if run.flags & FILTER_FLAG_ALLOW_THOUSAND != 0 && (byte == 0 || thousand.contains(&byte)) {
            if if first { !(1..=3).contains(&n) } else { n != 3 } {
                return Ok(Outcome::Failed);
            }
            first = false;
            pos += 1;
        } else {
            return Ok(Outcome::Failed);
        }
    }
    if pos != end {
        return Ok(Outcome::Failed);
    }
    Ok(numeric(&num, min, max).map_or(Outcome::Failed, |f| Outcome::Value(Owner::float(f))))
}

/// The tail of `php_filter_float`: what `is_numeric_string` reads the
/// copied number as, a double that underflowed to zero or is not finite
/// refused, held to the range.
fn numeric(num: &[u8], min: Option<f64>, max: Option<f64>) -> Option<f64> {
    let value = match numeric_string(num).as_ref().map(Value::as_ref)? {
        // `(double)lval`, php's own conversion.
        #[allow(clippy::cast_precision_loss)]
        ValueRef::Int(n) => n as f64,
        ValueRef::Float(d) => {
            let underflowed =
                d == 0.0 && num.len() > 1 && num.iter().any(|byte| (b'1'..=b'9').contains(byte));
            if underflowed || !d.is_finite() {
                return None;
            }
            d
        }
        _ => return None,
    };
    // A NaN bound refuses nothing, as C's `<` and `>` compare it.
    let outside = min.is_some_and(|m| value < m) || max.is_some_and(|m| value > m);
    (!outside).then_some(value)
}

/// `pcre_get_compiled_regex` over the pattern, then `pcre2_match` from
/// offset zero: the string unchanged where it matched, and a refused
/// pattern, whose warning ext/pcre raises under the active native, or a
/// match that errs, failed, as `php_filter_validate_regexp` and
/// `php_filter_validate_email` share it.
fn matched(req: &mut Request, pattern: &[u8], s: &[u8]) -> Result<Outcome, Thrown> {
    match Regex::compile(req, pattern) {
        Some(regex) if regex.is_match(req, s) => same(req, s),
        _ => Ok(Outcome::Failed),
    }
}

/// `php_filter_validate_regexp`, `FILTER_VALIDATE_REGEXP`: the `regexp`
/// option, a string, is required, and the string passes where it
/// matches.
///
/// ```php
/// try { filter_var("a", FILTER_VALIDATE_REGEXP); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { filter_var("a", FILTER_VALIDATE_REGEXP, ["options" => ["regexp" => 5]]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// filter_var(): "regexp" option is missing
/// filter_var(): "regexp" option is missing
/// ```
///
/// A match from offset zero, unanchored unless the pattern anchors it,
/// with the pattern's modifiers; a miss is the flags' failure.
///
/// ```php
/// foreach (["abc", "xabc", "ABC", ""] as $s) var_dump(filter_var($s, FILTER_VALIDATE_REGEXP, ["options" => ["regexp" => "/^a/"]]), filter_var($s, FILTER_VALIDATE_REGEXP, ["options" => ["regexp" => "/b/i"]]));
/// var_dump(filter_var("x", FILTER_VALIDATE_REGEXP, ["options" => ["regexp" => "/^a/"], "flags" => FILTER_NULL_ON_FAILURE]));
/// ```
/// ```expect
/// string(3) "abc"
/// string(3) "abc"
/// bool(false)
/// string(4) "xabc"
/// bool(false)
/// string(3) "ABC"
/// bool(false)
/// bool(false)
/// NULL
/// ```
///
/// A pattern php's syntax refuses warns as `pcre_get_compiled_regex`
/// words it, under `filter_var()`, sets `PREG_INTERNAL_ERROR` and
/// fails; the refusal is cached as the warning, so the second use warns
/// again.
///
/// ```php
/// var_dump(filter_var("a", FILTER_VALIDATE_REGEXP, ["options" => ["regexp" => "/(/"]]));
/// var_dump(filter_var("a", FILTER_VALIDATE_REGEXP, ["options" => ["regexp" => "abc"]]));
/// var_dump(filter_var("a", FILTER_VALIDATE_REGEXP, ["options" => ["regexp" => "/(/"]]));
/// var_dump(preg_last_error_msg());
/// ```
/// ```expect
///
/// Warning: filter_var(): Compilation failed: missing closing parenthesis at offset 1 in %s on line 2
/// bool(false)
///
/// Warning: filter_var(): Delimiter must not be alphanumeric, backslash, or NUL byte in %s on line 3
/// bool(false)
///
/// Warning: filter_var(): Compilation failed: missing closing parenthesis at offset 1 in %s on line 4
/// bool(false)
/// string(14) "Internal error"
/// ```
///
/// A match that errs, here a subject that is not UTF-8 under `u`, fails
/// and leaves `preg_last_error()` as it was, where `preg_match` sets it.
///
/// ```php
/// preg_match('/a/', 'a');
/// var_dump(filter_var("\xff", FILTER_VALIDATE_REGEXP, ["options" => ["regexp" => "/./u"]]), preg_last_error_msg());
/// var_dump(preg_match("/./u", "\xff"), preg_last_error_msg());
/// ```
/// ```expect
/// bool(false)
/// string(8) "No error"
/// bool(false)
/// string(56) "Malformed UTF-8 characters, possibly incorrectly encoded"
/// ```
pub(crate) fn regexp(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let Some(regexp) = string_option(run, b"regexp") else {
        return Err(value_error(req, run, "\"regexp\" option is missing"));
    };
    matched(req, &regexp, s)
}

/// `regexp0` of `php_filter_validate_email`: Michael Rushton's pattern
/// as php changed it to admit routeable addresses alone, its local part
/// also admitting letters and digits of any alphabet (`\pL\pN`) under
/// `u`, for `FILTER_FLAG_EMAIL_UNICODE`. php's bytes.
const EMAIL_UNICODE: &[u8] = br"/^(?!(?:(?:\x22?\x5C[\x00-\x7E]\x22?)|(?:\x22?[^\x5C\x22]\x22?)){255,})(?!(?:(?:\x22?\x5C[\x00-\x7E]\x22?)|(?:\x22?[^\x5C\x22]\x22?)){65,}@)(?:(?:[\x21\x23-\x27\x2A\x2B\x2D\x2F-\x39\x3D\x3F\x5E-\x7E\pL\pN]+)|(?:\x22(?:[\x01-\x08\x0B\x0C\x0E-\x1F\x21\x23-\x5B\x5D-\x7F\pL\pN]|(?:\x5C[\x00-\x7F]))*\x22))(?:\.(?:(?:[\x21\x23-\x27\x2A\x2B\x2D\x2F-\x39\x3D\x3F\x5E-\x7E\pL\pN]+)|(?:\x22(?:[\x01-\x08\x0B\x0C\x0E-\x1F\x21\x23-\x5B\x5D-\x7F\pL\pN]|(?:\x5C[\x00-\x7F]))*\x22)))*@(?:(?:(?!.*[^.]{64,})(?:(?:(?:xn--)?[a-z0-9]+(?:-+[a-z0-9]+)*\.){1,126}){1,}(?:(?:[a-z][a-z0-9]*)|(?:(?:xn--)[a-z0-9]+))(?:-+[a-z0-9]+)*)|(?:\[(?:(?:IPv6:(?:(?:[a-f0-9]{1,4}(?::[a-f0-9]{1,4}){7})|(?:(?!(?:.*[a-f0-9][:\]]){7,})(?:[a-f0-9]{1,4}(?::[a-f0-9]{1,4}){0,5})?::(?:[a-f0-9]{1,4}(?::[a-f0-9]{1,4}){0,5})?)))|(?:(?:IPv6:(?:(?:[a-f0-9]{1,4}(?::[a-f0-9]{1,4}){5}:)|(?:(?!(?:.*[a-f0-9]:){5,})(?:[a-f0-9]{1,4}(?::[a-f0-9]{1,4}){0,3})?::(?:[a-f0-9]{1,4}(?::[a-f0-9]{1,4}){0,3}:)?)))?(?:(?:25[0-5])|(?:2[0-4][0-9])|(?:1[0-9]{2})|(?:[1-9]?[0-9]))(?:\.(?:(?:25[0-5])|(?:2[0-4][0-9])|(?:1[0-9]{2})|(?:[1-9]?[0-9]))){3}))\]))$/iDu";

/// `regexp1`: the same pattern with an ASCII local part and no `u`.
/// php's bytes.
const EMAIL: &[u8] = br"/^(?!(?:(?:\x22?\x5C[\x00-\x7E]\x22?)|(?:\x22?[^\x5C\x22]\x22?)){255,})(?!(?:(?:\x22?\x5C[\x00-\x7E]\x22?)|(?:\x22?[^\x5C\x22]\x22?)){65,}@)(?:(?:[\x21\x23-\x27\x2A\x2B\x2D\x2F-\x39\x3D\x3F\x5E-\x7E]+)|(?:\x22(?:[\x01-\x08\x0B\x0C\x0E-\x1F\x21\x23-\x5B\x5D-\x7F]|(?:\x5C[\x00-\x7F]))*\x22))(?:\.(?:(?:[\x21\x23-\x27\x2A\x2B\x2D\x2F-\x39\x3D\x3F\x5E-\x7E]+)|(?:\x22(?:[\x01-\x08\x0B\x0C\x0E-\x1F\x21\x23-\x5B\x5D-\x7F]|(?:\x5C[\x00-\x7F]))*\x22)))*@(?:(?:(?!.*[^.]{64,})(?:(?:(?:xn--)?[a-z0-9]+(?:-+[a-z0-9]+)*\.){1,126}){1,}(?:(?:[a-z][a-z0-9]*)|(?:(?:xn--)[a-z0-9]+))(?:-+[a-z0-9]+)*)|(?:\[(?:(?:IPv6:(?:(?:[a-f0-9]{1,4}(?::[a-f0-9]{1,4}){7})|(?:(?!(?:.*[a-f0-9][:\]]){7,})(?:[a-f0-9]{1,4}(?::[a-f0-9]{1,4}){0,5})?::(?:[a-f0-9]{1,4}(?::[a-f0-9]{1,4}){0,5})?)))|(?:(?:IPv6:(?:(?:[a-f0-9]{1,4}(?::[a-f0-9]{1,4}){5}:)|(?:(?!(?:.*[a-f0-9]:){5,})(?:[a-f0-9]{1,4}(?::[a-f0-9]{1,4}){0,3})?::(?:[a-f0-9]{1,4}(?::[a-f0-9]{1,4}){0,3}:)?)))?(?:(?:25[0-5])|(?:2[0-4][0-9])|(?:1[0-9]{2})|(?:[1-9]?[0-9]))(?:\.(?:(?:25[0-5])|(?:2[0-4][0-9])|(?:1[0-9]{2})|(?:[1-9]?[0-9]))){3}))\]))$/iD";

/// `php_filter_validate_email`, `FILTER_VALIDATE_EMAIL`: at most 320
/// bytes (RFC 2821), then [`EMAIL`], or [`EMAIL_UNICODE`] under
/// `FILTER_FLAG_EMAIL_UNICODE`. No comments or folding whitespace, and
/// the domain a fully-qualified name or an address in brackets.
///
/// ```php
/// foreach (["user@example.com", "first.last+tag@sub.example.co.uk", "a@b", "\"quoted str\"@example.com", "user@[IPv6:::1]", "user@[127.0.0.1]", "user@[256.0.0.1]", "a.@example.com", ".a@example.com", "a..b@example.com", "a@-example.com", "a@example.123", "a@xn--bcher-kva.example", "", "@example.com"] as $s) var_dump(filter_var($s, FILTER_VALIDATE_EMAIL));
/// ```
/// ```expect
/// string(16) "user@example.com"
/// string(32) "first.last+tag@sub.example.co.uk"
/// bool(false)
/// bool(false)
/// string(15) "user@[IPv6:::1]"
/// string(16) "user@[127.0.0.1]"
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// string(23) "a@xn--bcher-kva.example"
/// bool(false)
/// bool(false)
/// ```
///
/// `FILTER_FLAG_EMAIL_UNICODE` admits letters and digits beyond ASCII
/// in the local part alone; the domain stays ASCII, and a string that
/// is not UTF-8 fails under it.
///
/// ```php
/// foreach (["jöhn@example.com", "用户@example.com", "a@exämple.com", "\xff@example.com", "user@example.com"] as $s) var_dump(filter_var($s, FILTER_VALIDATE_EMAIL), filter_var($s, FILTER_VALIDATE_EMAIL, FILTER_FLAG_EMAIL_UNICODE));
/// ```
/// ```expect
/// bool(false)
/// string(17) "jöhn@example.com"
/// bool(false)
/// string(18) "用户@example.com"
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// string(16) "user@example.com"
/// string(16) "user@example.com"
/// ```
///
/// The lengths: 254 bytes in all, since the pattern's first lookahead
/// refuses 255 of the address's characters, 64 in the local part and 63
/// in a label; past 320 bytes the pattern is not matched at all.
///
/// ```php
/// $local = str_repeat("a", 64);
/// $label = str_repeat("b", 63);
/// foreach ([61, 62] as $n) { $s = "$local@$label.$label." . str_repeat("c", $n); var_dump(strlen($s), filter_var($s, FILTER_VALIDATE_EMAIL) === $s); }
/// var_dump(filter_var(str_repeat("a", 65) . "@example.com", FILTER_VALIDATE_EMAIL));
/// var_dump(filter_var("a@" . str_repeat("b", 64) . ".com", FILTER_VALIDATE_EMAIL));
/// var_dump(filter_var(str_repeat("a", 321), FILTER_VALIDATE_EMAIL));
/// ```
/// ```expect
/// int(254)
/// bool(true)
/// int(255)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
pub(crate) fn email(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let pattern = if run.flags & FILTER_FLAG_EMAIL_UNICODE != 0 {
        EMAIL_UNICODE
    } else {
        EMAIL
    };
    if s.len() > 320 {
        return Ok(Outcome::Failed);
    }
    matched(req, pattern, s)
}

/// `php_filter_validate_domain_ex`: at most 253 bytes past a trailing
/// dot, labels of at most 63, no empty label, and under
/// `FILTER_FLAG_HOSTNAME` alphanumerics and inner hyphens alone.
fn domain_ok(s: &[u8], flags: i64) -> bool {
    let hostname = flags & FILTER_FLAG_HOSTNAME != 0;
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut end = s.len();
    if end > 0 && s[end - 1] == b'.' {
        end -= 1;
    }
    if end > 253 || at(0) == b'.' || (hostname && !at(0).is_ascii_alphanumeric()) {
        return false;
    }
    let mut label = 1;
    for (k, &c) in s[..end].iter().enumerate() {
        if c == b'.' {
            let edges =
                k > 0 && at(k - 1).is_ascii_alphanumeric() && at(k + 1).is_ascii_alphanumeric();
            if at(k + 1) == b'.' || (hostname && !edges) {
                return false;
            }
            label = 1;
        } else {
            if label > 63
                || (hostname && (c != b'-' || at(k + 1) == 0) && !c.is_ascii_alphanumeric())
            {
                return false;
            }
            label += 1;
        }
    }
    true
}

/// `php_filter_validate_domain`, `FILTER_VALIDATE_DOMAIN`.
///
/// ```php
/// foreach (["example.com", "example.com.", "a..b", ".a", "-a.com", "a-.com", "a_b.com", str_repeat("a", 64) . ".com", ""] as $s) var_dump(filter_var($s, FILTER_VALIDATE_DOMAIN), filter_var($s, FILTER_VALIDATE_DOMAIN, FILTER_FLAG_HOSTNAME));
/// ```
/// ```expect
/// string(11) "example.com"
/// string(11) "example.com"
/// string(12) "example.com."
/// string(12) "example.com."
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// string(6) "-a.com"
/// bool(false)
/// string(6) "a-.com"
/// bool(false)
/// string(7) "a_b.com"
/// bool(false)
/// bool(false)
/// bool(false)
/// string(0) ""
/// bool(false)
/// ```
pub(crate) fn domain(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    if domain_ok(s, run.flags) {
        same(req, s)
    } else {
        Ok(Outcome::Failed)
    }
}

/// `is_userinfo_valid`: unreserved and sub-delimiter bytes, `:`, and
/// `%` with two digits of which the first is decimal.
fn userinfo_ok(s: &[u8]) -> bool {
    let mut k = 0;
    while k < s.len() {
        let c = s[k];
        if c.is_ascii_alphanumeric() || c == 0 || b"-._~!$&'()*+,;=:".contains(&c) {
            k += 1;
        } else if c == b'%'
            && k + 3 <= s.len()
            && s[k + 1].is_ascii_digit()
            && s[k + 2].is_ascii_hexdigit()
        {
            k += 3;
        } else {
            return false;
        }
    }
    true
}

/// `php_filter_validate_url`, `FILTER_VALIDATE_URL`: nothing the URL
/// sanitizer would strip, then the components of the parser the
/// `uri_parser_class` option names (`php_uri_get_parser`): ext/uri's for
/// a class, `parse_url` where none is named, read raw as
/// `php_uri_parse_to_struct` reads them: a scheme, a host for all but `mailto`,
/// `news` and `file`, and, for `parse_url` alone, whose parse checks
/// less than the two standards', a hostname or a bracketed IPv6 address
/// for `http` and `https` and the user and password as
/// `is_userinfo_valid` holds them.
///
/// ```php
/// foreach (["http://example.com", "https://a.b/p?q=1#f", "http://-bad.com", "http://[::1]:80/", "mailto:a@b.c", "news:x", "foo", "http://ex ample.com", "http://u:p@h.com", "http://u%zz@h.com", "ftp://h", "//h.com"] as $s) var_dump(filter_var($s, FILTER_VALIDATE_URL));
/// var_dump(filter_var("http://h.com", FILTER_VALIDATE_URL, FILTER_FLAG_PATH_REQUIRED), filter_var("http://h.com/p", FILTER_VALIDATE_URL, FILTER_FLAG_QUERY_REQUIRED), filter_var("http://h.com/p?x", FILTER_VALIDATE_URL, FILTER_FLAG_PATH_REQUIRED | FILTER_FLAG_QUERY_REQUIRED));
/// try { filter_var("http://h.com", FILTER_VALIDATE_URL, ["options" => ["uri_parser_class" => "nope"]]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// foreach (["parse_url", Uri\Rfc3986\Uri::class, Uri\WhatWg\Url::class] as $p) {
///     foreach (["http:///example.com", "http://exa%23mple.org", "http://a_b.com/", "http://u%zz@h.com"] as $u) echo $p, " ", var_export(filter_var($u, FILTER_VALIDATE_URL, ["options" => ["uri_parser_class" => $p]]), true), "\n";
/// }
/// ```
/// ```expect
/// string(18) "http://example.com"
/// string(19) "https://a.b/p?q=1#f"
/// bool(false)
/// string(16) "http://[::1]:80/"
/// string(12) "mailto:a@b.c"
/// string(6) "news:x"
/// bool(false)
/// bool(false)
/// string(16) "http://u:p@h.com"
/// bool(false)
/// string(7) "ftp://h"
/// bool(false)
/// bool(false)
/// bool(false)
/// string(16) "http://h.com/p?x"
/// filter_var(): "uri_parser_class" option has invalid value
/// parse_url false
/// parse_url false
/// parse_url false
/// parse_url false
/// Uri\Rfc3986\Uri 'http:///example.com'
/// Uri\Rfc3986\Uri 'http://exa%23mple.org'
/// Uri\Rfc3986\Uri 'http://a_b.com/'
/// Uri\Rfc3986\Uri false
/// Uri\WhatWg\Url 'http:///example.com'
/// Uri\WhatWg\Url false
/// Uri\WhatWg\Url 'http://a_b.com/'
/// Uri\WhatWg\Url 'http://u%zz@h.com'
/// ```
pub(crate) fn url(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    if sanitizing::url_bytes(s).len() != s.len() {
        return Ok(Outcome::Failed);
    }
    let name = string_option(run, b"uri_parser_class");
    let parser = match name.as_deref() {
        None | Some(b"parse_url") => None,
        Some(name) => match Parser::named(name) {
            Some(parser) => Some(parser),
            None => {
                return Err(value_error(
                    req,
                    run,
                    "\"uri_parser_class\" option has invalid value",
                ));
            }
        },
    };
    let uri = match parser {
        Some(parser) => parser.parse_to_struct(req, s, ReadAs::Raw, true)?,
        None => parse_url(req, s)?,
    };
    let Some(uri) = uri else {
        return Ok(Outcome::Failed);
    };
    let parse_url = parser.is_none();
    let web = uri
        .scheme
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case(b"http") || s.eq_ignore_ascii_case(b"https"));
    let ipv6_host = |h: &[u8]| {
        h.len() >= 2 && h[0] == b'[' && h[h.len() - 1] == b']' && ipv6(&h[1..h.len() - 1]).is_some()
    };
    let host_ok = match uri.host.as_deref() {
        None => false,
        Some(h) => !parse_url || ipv6_host(h) || domain_ok(h, FILTER_FLAG_HOSTNAME),
    };
    let valid = (!web || host_ok)
        && uri.scheme.as_deref().is_some_and(|scheme| {
            uri.host.is_some() || matches!(scheme, b"mailto" | b"news" | b"file")
        })
        && (run.flags & FILTER_FLAG_PATH_REQUIRED == 0 || uri.path.is_some())
        && (run.flags & FILTER_FLAG_QUERY_REQUIRED == 0 || uri.query.is_some())
        && (!parse_url
            || (uri.user.as_deref().is_none_or(userinfo_ok)
                && uri.password.as_deref().is_none_or(userinfo_ok)));
    if valid {
        same(req, s)
    } else {
        Ok(Outcome::Failed)
    }
}

/// The `parse_url` parser of php's `uri_parsers`, `php_url_parse_ex2` read raw
/// into a `php_uri`: ext/standard's `parse_url`, called as the function
/// table has it, since ext/standard, which will need ext/uri, keeps it.
fn parse_url(req: &mut Request, s: &[u8]) -> Result<Option<UriStruct>, Thrown> {
    let name = string_value(req, b"parse_url")?;
    let parse_url = callable_checked(req, &name);
    name.give_up(req);
    let Ok(parse_url) = parse_url else {
        return Err(fatal(
            req,
            "InternalError: FILTER_VALIDATE_URL reads parse_url, which ext/standard declares",
        ));
    };
    let subject = string_value(req, s)?;
    let parts = call(req, &parse_url, &[*subject]);
    subject.give_up(req);
    let parts = parts?;
    let Some(array) = parts.as_array().copied() else {
        parts.give_up(req);
        return Ok(None);
    };
    let part = |name: &[u8]| {
        array
            .find(name)
            .and_then(|value| value.deref().as_str_bytes().map(<[u8]>::to_vec))
    };
    let uri = UriStruct {
        scheme: part(b"scheme"),
        user: part(b"user"),
        password: part(b"pass"),
        host: part(b"host"),
        port: array.find(b"port").and_then(|value| value.deref().as_int()),
        path: part(b"path"),
        query: part(b"query"),
        fragment: part(b"fragment"),
    };
    parts.give_up(req);
    Ok(Some(uri))
}

/// `_php_filter_validate_ipv4`: four decimal parts of at most three
/// digits, none above 255 and none with a leading zero.
fn ipv4(s: &[u8]) -> Option<[i64; 4]> {
    let mut ip = [0; 4];
    let (mut n, mut i) = (0, 0);
    while i < s.len() {
        if !s[i].is_ascii_digit() {
            return None;
        }
        let leading_zero = s[i] == b'0';
        let mut digits = 1;
        let mut num = i64::from(s[i] - b'0');
        i += 1;
        while i < s.len() && s[i].is_ascii_digit() {
            num = num * 10 + i64::from(s[i] - b'0');
            i += 1;
            digits += 1;
            if num > 255 || digits > 3 {
                return None;
            }
        }
        if leading_zero && (num != 0 || digits > 1) {
            return None;
        }
        ip[n] = num;
        n += 1;
        if n == 4 {
            return (i == s.len()).then_some(ip);
        }
        if i >= s.len() || s[i] != b'.' {
            return None;
        }
        i += 1;
    }
    None
}

/// `_php_filter_validate_ipv6`: blocks of one to four hex digits, one
/// `::` at most, an IPv4 address at the end in the place of two; the
/// eight blocks as php fills them, an address with a dotted tail read as
/// the IPv4-mapped one.
fn ipv6(s: &[u8]) -> Option<[i64; 8]> {
    if !s.contains(&b':') {
        return None;
    }
    let mut ip = [0i64; 8];
    let mut end = s.len();
    let mut blocks = 0;
    let mut compressed: Option<usize> = None;
    let mut mapped = None;
    if let Some(mut v) = s.iter().position(|&c| c == b'.') {
        while v > 0 && s[v - 1] != b':' {
            v -= 1;
        }
        mapped = Some(ipv4(&s[v..])?);
        if v < 2 {
            return None;
        }
        end = if s[v - 2] == b':' { v } else { v - 1 };
        blocks = 2;
    }
    let mut i = 0;
    while i < end {
        if s[i] == b':' {
            i += 1;
            if i >= end {
                return None;
            }
            if s[i] == b':' {
                if compressed.is_some() {
                    return None;
                }
                if blocks < 8 {
                    ip[blocks] = -1;
                }
                compressed = Some(blocks);
                blocks += 1;
                i += 1;
                if i == end {
                    if blocks > 8 {
                        return None;
                    }
                    break;
                }
            } else if i == 1 {
                return None;
            }
        }
        let (mut num, mut n) = (0u32, 0);
        while i < end {
            let Some(digit) = char::from(s[i]).to_digit(16) else {
                break;
            };
            num = num.wrapping_mul(16).wrapping_add(digit);
            n += 1;
            i += 1;
        }
        if blocks < 8 {
            ip[blocks] = i64::from(num);
        }
        if !(1..=4).contains(&n) {
            return None;
        }
        blocks += 1;
        if blocks > 8 {
            return None;
        }
    }
    if let Some(v4) = mapped {
        ip = [
            0,
            0,
            0,
            0,
            0,
            0xffff,
            256 * v4[0] + v4[1],
            256 * v4[2] + v4[3],
        ];
    } else if let Some(at) = compressed
        && blocks <= 8
    {
        let offset = 8 - blocks;
        for k in (at + offset + 1..8).rev() {
            ip[k] = ip[k - offset];
        }
        for slot in &mut ip[at..=at + offset] {
            *slot = 0;
        }
    }
    (compressed.is_some() && blocks <= 8 || blocks == 8).then_some(ip)
}

/// `ipv4_get_status_flags` over RFC 6890's table, as php orders the
/// arms: `(global, reserved, private)` for an address in a special
/// block, `None` outside every one.
fn ipv4_status(ip: &[i64]) -> Option<(bool, bool, bool)> {
    let (a, b, c) = (ip[0], ip[1], ip[2]);
    let reserved = matches!((a, b), (0 | 127, _) | (169, 254) | (240.., ..=255));
    let private = matches!((a, b), (10, _) | (172, 16..=31) | (192, 168));
    let global = (a, b, c) == (192, 88, 99);
    let listed = matches!(
        (a, b, c),
        (100, 64..=127, _) | (192, 0, 0 | 2) | (198, 18..=19, _) | (198, 51, 100) | (203, 0, 113)
    );
    (reserved || private || global || listed).then_some((global, reserved, private))
}

/// `ipv6_get_status_flags` over RFC 6890's table. RFC 4380, 2928, 5180
/// and 4843 are inside `2001::/23`, as php's chain reaches them.
fn ipv6_status(ip: &[i64; 8]) -> Option<(bool, bool, bool)> {
    let zeros = |n: usize| ip[..n].iter().all(|&b| b == 0);
    let reserved = zeros(8)
        || (zeros(7) && ip[7] == 1)
        || (zeros(5) && ip[5] == 0xffff)
        || (0xfe80..=0xfebf).contains(&ip[0]);
    let global = ip[0] == 0x64 && ip[1] == 0xff9b;
    let private = (0xfc00..=0xfdff).contains(&ip[0]);
    let listed = matches!(
        ip,
        [0x0100, 0, 0, 0, ..] | [0x2001, ..=0x01ff | 0x0db8, ..] | [0x2002, ..]
    );
    (reserved || private || global || listed).then_some((global, reserved, private))
}

/// `php_filter_validate_ip`, `FILTER_VALIDATE_IP`: an IPv6 address when
/// the string has a colon, IPv4 when it has a dot, held to
/// `FILTER_FLAG_IPV4` and `FILTER_FLAG_IPV6`, and to the three range
/// flags for an address in a block of RFC 6890's table.
///
/// ```php
/// foreach (["127.0.0.1", "256.1.1.1", "01.2.3.4", "1.2.3", "::1", "2001:db8::ff00:42:8329", "::ffff:192.0.2.1", "1:2:3:4:5:6:7:8:9", "1::2::3", ":1::", "fe80::1", "abc"] as $s) var_dump(filter_var($s, FILTER_VALIDATE_IP));
/// var_dump(filter_var("10.0.0.1", FILTER_VALIDATE_IP, FILTER_FLAG_NO_PRIV_RANGE), filter_var("127.0.0.1", FILTER_VALIDATE_IP, FILTER_FLAG_NO_RES_RANGE), filter_var("8.8.8.8", FILTER_VALIDATE_IP, FILTER_FLAG_NO_PRIV_RANGE | FILTER_FLAG_NO_RES_RANGE), filter_var("192.88.99.1", FILTER_VALIDATE_IP, FILTER_FLAG_GLOBAL_RANGE), filter_var("100.64.0.1", FILTER_VALIDATE_IP, FILTER_FLAG_GLOBAL_RANGE));
/// var_dump(filter_var("::1", FILTER_VALIDATE_IP, FILTER_FLAG_IPV4), filter_var("1.2.3.4", FILTER_VALIDATE_IP, FILTER_FLAG_IPV6), filter_var("fd00::1", FILTER_VALIDATE_IP, FILTER_FLAG_NO_PRIV_RANGE), filter_var("2001:db8::1", FILTER_VALIDATE_IP, FILTER_FLAG_IPV4 | FILTER_FLAG_IPV6));
/// ```
/// ```expect
/// string(9) "127.0.0.1"
/// bool(false)
/// bool(false)
/// bool(false)
/// string(3) "::1"
/// string(22) "2001:db8::ff00:42:8329"
/// string(16) "::ffff:192.0.2.1"
/// bool(false)
/// bool(false)
/// bool(false)
/// string(7) "fe80::1"
/// bool(false)
/// bool(false)
/// bool(false)
/// string(7) "8.8.8.8"
/// string(11) "192.88.99.1"
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// string(11) "2001:db8::1"
/// ```
pub(crate) fn ip(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let flags = run.flags;
    let v6 = s.contains(&b':');
    if !v6 && !s.contains(&b'.') {
        return Ok(Outcome::Failed);
    }
    let (four, six) = (flags & FILTER_FLAG_IPV4 != 0, flags & FILTER_FLAG_IPV6 != 0);
    if (four && !six && v6) || (six && !four && !v6) {
        return Ok(Outcome::Failed);
    }
    let status = if v6 {
        let Some(ip) = ipv6(s) else {
            return Ok(Outcome::Failed);
        };
        ipv6_status(&ip)
    } else {
        let Some(ip) = ipv4(s) else {
            return Ok(Outcome::Failed);
        };
        ipv4_status(&ip)
    };
    let Some((global, reserved, private)) = status else {
        return same(req, s);
    };
    if (flags & FILTER_FLAG_GLOBAL_RANGE != 0 && !global)
        || (flags & FILTER_FLAG_NO_PRIV_RANGE != 0 && private)
        || (flags & FILTER_FLAG_NO_RES_RANGE != 0 && reserved)
    {
        return Ok(Outcome::Failed);
    }
    same(req, s)
}

/// `php_filter_validate_mac`, `FILTER_VALIDATE_MAC`: three groups of
/// four hex digits between dots, or six of two between hyphens or
/// colons, the separator held to the `separator` option.
///
/// ```php
/// foreach (["01:23:45:67:89:ab", "01-23-45-67-89-AB", "0123.4567.89ab", "01:23:45-67:89:ab", "01:23:45:67:89:ag", "0123456789ab"] as $s) var_dump(filter_var($s, FILTER_VALIDATE_MAC));
/// var_dump(filter_var("01-23-45-67-89-ab", FILTER_VALIDATE_MAC, ["options" => ["separator" => ":"]]));
/// try { filter_var("01:23:45:67:89:ab", FILTER_VALIDATE_MAC, ["options" => ["separator" => "::"]]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(17) "01:23:45:67:89:ab"
/// string(17) "01-23-45-67-89-AB"
/// string(14) "0123.4567.89ab"
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// filter_var(): "separator" option must be one character long
/// ```
pub(crate) fn mac(req: &mut Request, run: &Run, s: &[u8]) -> Result<Outcome, Thrown> {
    let wanted = string_option(run, b"separator");
    if wanted.as_ref().is_some_and(|w| w.len() != 1) {
        return Err(value_error(
            req,
            run,
            "\"separator\" option must be one character long",
        ));
    }
    let (tokens, length, separator) = match (s.len(), s.get(2)) {
        (14, _) => (3, 4, b'.'),
        (17, Some(b'-')) => (6, 2, b'-'),
        (17, Some(b':')) => (6, 2, b':'),
        _ => return Ok(Outcome::Failed),
    };
    if wanted.is_some_and(|w| w[0] != separator) {
        return Ok(Outcome::Failed);
    }
    for i in 0..tokens {
        let offset = i * (length + 1);
        if (i < tokens - 1 && s[offset + length] != separator)
            || parse_base(&s[offset..offset + length], 16).is_none()
        {
            return Ok(Outcome::Failed);
        }
    }
    same(req, s)
}
