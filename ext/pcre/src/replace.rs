//! `php_pcre_replace_impl` and `php_pcre_replace_func_impl`
//! (`Z/ext/pcre/php_pcre.c:1616`, `:1860`) and the four natives over
//! them: every match of a pattern, up to a limit, replaced by a string
//! with `$n`, `${n}` and `\n` references or by what a callback returns,
//! over a string subject or each string of an array one, a pattern array
//! applied in turn.

use php_ext::abi::Callable;
use php_ext::call::call;
use php_ext::coerce::{callable_checked, coerce_param, to_string};
use php_ext::decl::Type;
use php_ext::raising::{Level, Thrown, arg_type_error, docref};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value};

use crate::ffi::{self, Subject};
use crate::matching::{Scratch, populate, unit_length};
use crate::pattern::{Pattern, compiled};
use crate::{
    Either, INTERNAL_ERROR, NO_ERROR, arguments_between, assign_ref, callable_argument,
    either_argument, exec_error, long_argument, set_error,
};

/// What one subject came to.
enum Replaced {
    /// Nothing matched: the subject itself, `zend_string_copy`.
    Same,
    Built(Vec<u8>),
    /// The error code is set; the native answers `null` for it.
    Failed,
}

/// `size_t limit` from a `zend_long`: `-1`, and every other negative,
/// is no limit.
pub(super) fn limit_of(limit: i64) -> usize {
    usize::try_from(limit).unwrap_or(usize::MAX)
}

/// `preg_get_backref`: at a `$` or `\`, the group a `$n`, `${n}` or `\n`
/// of one or two digits names and where the reference ends. The
/// replacement is read as php's NUL-terminated buffer is.
fn backref(replacement: &[u8], at: usize) -> Option<(usize, usize)> {
    let byte = |i: usize| replacement.get(i).copied().unwrap_or(0);
    if byte(at + 1) == 0 {
        return None;
    }
    let brace = byte(at) == b'$' && byte(at + 1) == b'{';
    let mut walk = at + 1 + usize::from(brace);
    if !byte(walk).is_ascii_digit() {
        return None;
    }
    let mut group = usize::from(byte(walk) - b'0');
    walk += 1;
    if byte(walk).is_ascii_digit() {
        group = group * 10 + usize::from(byte(walk) - b'0');
        walk += 1;
    }
    if brace {
        if byte(walk) != b'}' {
            return None;
        }
        walk += 1;
    }
    Some((group, walk))
}

/// The replacement with its references filled from this match: a
/// reference to a group past the match's count, or to one that did not
/// take part, is empty, and a `\` before `$` or `\` writes that byte as it is.
fn expand(replacement: &[u8], subject: &[u8], scratch: &Scratch, count: usize, out: &mut Vec<u8>) {
    let mut at = 0;
    let mut last = 0u8;
    while at < replacement.len() {
        let byte = replacement[at];
        if byte == b'\\' || byte == b'$' {
            if last == b'\\' {
                out.pop();
                out.push(byte);
                at += 1;
                last = 0;
                continue;
            }
            if let Some((group, next)) = backref(replacement, at) {
                if group < count {
                    let (start, end) = scratch.group(group);
                    if start != ffi::PCRE2_UNSET {
                        out.extend_from_slice(subject.get(start..end).unwrap_or_default());
                    }
                }
                at = next;
                continue;
            }
        }
        out.push(byte);
        at += 1;
        last = byte;
    }
}

/// The loop both replacement bodies share: each match up to `limit`
/// replaced by what `each` writes, the pieces between copied, an empty
/// match stepped past as `/g` does, and `count` raised once per match.
fn replace_loop(
    req: &mut Request,
    pattern: &Pattern,
    subject: &[u8],
    mut limit: usize,
    count: &mut usize,
    mut each: impl FnMut(&mut Request, &Scratch, usize, &mut Vec<u8>) -> Result<(), Thrown>,
) -> Result<Replaced, Thrown> {
    set_error(req, NO_ERROR);
    let Some(mut scratch) = Scratch::take(req, pattern) else {
        return Ok(Replaced::Failed);
    };
    let len = subject.len();
    let mut searched = Subject::new(subject);
    let mut out: Option<Vec<u8>> = None;
    let mut start;
    let mut last_end = 0;
    let mut rc = scratch.run(pattern, &mut searched, 0, 0, true);
    let replaced = 'search: loop {
        if rc >= 0 && limit > 0 {
            if rc == 0 {
                docref(req, Level::Notice, "Matched, but too many substrings");
                rc = i32::try_from(pattern.groups).unwrap_or(i32::MAX);
            }
            loop {
                let (match_start, match_end) = scratch.group(0);
                if match_end < match_start {
                    set_error(req, INTERNAL_ERROR);
                    break 'search Replaced::Failed;
                }
                *count += 1;
                let buffer = out.get_or_insert_with(Vec::new);
                buffer.extend_from_slice(subject.get(last_end..match_start).unwrap_or_default());
                let n = usize::try_from(rc).unwrap_or(0);
                if let Err(thrown) = each(req, &scratch, n, buffer) {
                    set_error(req, INTERNAL_ERROR);
                    scratch.give_back(req);
                    return Err(thrown);
                }
                limit -= 1;
                start = match_end;
                last_end = match_end;
                if start == match_start {
                    rc = scratch.retry(pattern, &mut searched, start);
                    if rc >= 0 && limit > 0 {
                        continue;
                    } else if rc == ffi::PCRE2_ERROR_NOMATCH || limit == 0 {
                        if start < len {
                            start += unit_length(pattern, subject, start);
                        } else {
                            break 'search finished(out, subject, last_end);
                        }
                    } else {
                        exec_error(req, rc);
                        break 'search Replaced::Failed;
                    }
                }
                break;
            }
        } else if rc == ffi::PCRE2_ERROR_NOMATCH || limit == 0 {
            break finished(out, subject, last_end);
        } else {
            exec_error(req, rc);
            break Replaced::Failed;
        }
        rc = scratch.run(pattern, &mut searched, start, 0, true);
    };
    scratch.give_back(req);
    Ok(replaced)
}

/// `not_matched:` the subject itself where nothing was replaced, and
/// otherwise the bytes built with the rest of the subject after them.
fn finished(out: Option<Vec<u8>>, subject: &[u8], last_end: usize) -> Replaced {
    match out {
        None => Replaced::Same,
        Some(mut out) => {
            out.extend_from_slice(subject.get(last_end..).unwrap_or_default());
            Replaced::Built(out)
        }
    }
}

/// The subject a replacement came to, as a string the caller owns, or
/// `None` for php's `NULL`.
fn owned(
    req: &mut Request,
    subject: PhpString,
    replaced: Replaced,
) -> Result<Option<PhpString>, Thrown> {
    match replaced {
        Replaced::Same => {
            req.addref(&Value::string(subject));
            Ok(Some(subject))
        }
        Replaced::Built(bytes) => Ok(Some(PhpString::new(req, &bytes)?)),
        Replaced::Failed => Ok(None),
    }
}

/// `php_pcre_replace`: one pattern string over one subject.
fn replace_one(
    req: &mut Request,
    regex: &[u8],
    subject: PhpString,
    replacement: &[u8],
    limit: usize,
    count: &mut usize,
) -> Result<Option<PhpString>, Thrown> {
    let Some(pattern) = compiled(req, regex) else {
        return Ok(None);
    };
    replace_impl(req, &pattern, subject, replacement, limit, count)
}

/// `php_pcre_replace_impl`: one compiled pattern over one subject.
pub(super) fn replace_impl(
    req: &mut Request,
    pattern: &Pattern,
    subject: PhpString,
    replacement: &[u8],
    limit: usize,
    count: &mut usize,
) -> Result<Option<PhpString>, Thrown> {
    let bytes = subject.as_bytes();
    let replaced = replace_loop(req, pattern, bytes, limit, count, |_, scratch, n, out| {
        expand(replacement, bytes, scratch, n, out);
        Ok(())
    })?;
    owned(req, subject, replaced)
}

/// `php_pcre_replace_func`: one pattern string over one subject, each
/// match replaced by what `callback` returns for its array.
fn replace_one_func(
    req: &mut Request,
    regex: &[u8],
    subject: PhpString,
    callback: &Callable,
    limit: usize,
    count: &mut usize,
    flags: i64,
) -> Result<Option<PhpString>, Thrown> {
    let Some(pattern) = compiled(req, regex) else {
        return Ok(None);
    };
    let bytes = subject.as_bytes();
    let replaced = replace_loop(
        req,
        &pattern,
        bytes,
        limit,
        count,
        |req, scratch, n, out| {
            let mut groups = PhpArray::new(req)?;
            populate(req, &mut groups, &pattern, scratch, bytes, n, flags)?;
            let argument = groups.to_value();
            let returned = call(req, callback, &[argument]);
            req.release(argument);
            let returned = returned?;
            let text = to_string(req, &returned);
            req.release(returned);
            let text = text?;
            out.extend_from_slice(text.as_bytes());
            req.release(Value::string(text));
            Ok(())
        },
    )?;
    owned(req, subject, replaced)
}

/// `php_replace_in_subject`: a pattern string, or each of an array's
/// in turn over what the one before left, with the replacement or the
/// replacement array's entry at the same position, the empty string
/// past its end.
fn in_subject(
    req: &mut Request,
    regex: Either,
    replacement: Either,
    subject: PhpString,
    limit: usize,
    count: &mut usize,
) -> Result<Option<PhpString>, Thrown> {
    let patterns = match regex {
        Either::Str(regex) => {
            let Either::Str(replacement) = replacement else {
                return Ok(None);
            };
            return replace_one(
                req,
                regex.as_bytes(),
                subject,
                replacement.as_bytes(),
                limit,
                count,
            );
        }
        Either::Array(patterns) => patterns,
    };
    let replacements = match replacement {
        Either::Array(replacements) => Some(replacements.snapshot()),
        Either::Str(_) => None,
    };
    req.addref(&Value::string(subject));
    let mut current = subject;
    for (at, (_, entry)) in patterns.snapshot().into_iter().enumerate() {
        let regex = to_string(req, &entry)?;
        let replacement = match (&replacements, replacement) {
            (Some(all), _) => match all.get(at) {
                Some((_, value)) => to_string(req, value)?,
                None => PhpString::new(req, b"")?,
            },
            (None, Either::Str(text)) => {
                req.addref(&Value::string(text));
                text
            }
            (None, Either::Array(_)) => PhpString::new(req, b"")?,
        };
        let result = replace_one(
            req,
            regex.as_bytes(),
            current,
            replacement.as_bytes(),
            limit,
            count,
        );
        req.release(Value::string(replacement));
        req.release(Value::string(regex));
        req.release(Value::string(current));
        match result? {
            Some(next) => current = next,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

/// `php_replace_in_subject_func`: [`in_subject`] with a callback.
fn in_subject_func(
    req: &mut Request,
    regex: Either,
    callback: &Callable,
    subject: PhpString,
    limit: usize,
    count: &mut usize,
    flags: i64,
) -> Result<Option<PhpString>, Thrown> {
    let patterns = match regex {
        Either::Str(regex) => {
            return replace_one_func(
                req,
                regex.as_bytes(),
                subject,
                callback,
                limit,
                count,
                flags,
            );
        }
        Either::Array(patterns) => patterns,
    };
    req.addref(&Value::string(subject));
    let mut current = subject;
    for (_, entry) in patterns.snapshot() {
        let regex = match to_string(req, &entry) {
            Ok(regex) => regex,
            Err(thrown) => {
                req.release(Value::string(current));
                return Err(thrown);
            }
        };
        let result = replace_one_func(
            req,
            regex.as_bytes(),
            current,
            callback,
            limit,
            count,
            flags,
        );
        req.release(Value::string(regex));
        req.release(Value::string(current));
        match result? {
            Some(next) => current = next,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

/// Each string of an array subject through `each`, under its key, the
/// entries `each` answers `None` for, or `keep` refuses, left out.
fn over_array(
    req: &mut Request,
    subjects: PhpArray,
    count: &mut usize,
    mut each: impl FnMut(&mut Request, PhpString, &mut usize) -> Result<Option<PhpString>, Thrown>,
    keep: impl Fn(usize, usize) -> bool,
) -> Result<Value, Thrown> {
    let mut out = PhpArray::new(req)?;
    for (key, entry) in subjects.snapshot() {
        let before = *count;
        let subject = to_string(req, &entry)?;
        let result = each(req, subject, count);
        req.release(Value::string(subject));
        let Some(result) = result? else {
            continue;
        };
        if !keep(before, *count) {
            req.release(Value::string(result));
            continue;
        }
        if let Key::Str(s) = &key {
            req.addref(&Value::string(*s));
        }
        out.set(req, key, Value::string(result))?;
    }
    Ok(out.to_value())
}

/// `preg_replace(string|array $pattern, string|array $replacement,
/// string|array $subject, int $limit = -1, &$count = null):
/// string|array|null`: every match replaced, `$0` to `$99`, `${n}` and
/// `\n` naming groups, up to `$limit` per subject and pattern, the
/// replacements made counted in `$count`.
///
/// ```php
/// var_dump(preg_replace('/(\w+) (\w+)/', '$2 ${1}! \2 \\\\1 $9 \$1', 'hello world', -1, $c), $c);
/// var_dump(preg_replace('/a/', 'b', 'aaaa', 2, $c), $c, preg_replace('/x/', 'y', 'abc'), preg_replace('/a*/', '-', 'baac'), preg_replace('//u', '|', "\xc3\xa9"));
/// ```
/// ```expect
/// string(25) "world hello! world \1  $1"
/// int(1)
/// string(4) "bbaa"
/// int(2)
/// string(3) "abc"
/// string(6) "-b--c-"
/// string(4) "|é|"
/// ```
///
/// Pattern arrays apply in turn, each with the replacement at its
/// position, and an array subject keeps its keys.
///
/// ```php
/// var_dump(preg_replace(['/a/', '/b/', '/c/'], ['b', 'c'], 'abc'), preg_replace(['/a/', '/b/'], 'x', 'ab'), preg_replace('/\d/', '#', ['k' => 'a1', 5 => 'b22', 7]));
/// try { preg_replace('/a/', ['x'], 'a'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { preg_replace('/a/', new stdClass, 'a'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(preg_replace('/1/', 'x', 21), preg_replace('/1/', 'x', 1.5), preg_replace('/1/', 'x', true));
/// var_dump(preg_replace('/(/', 'x', 'a'), preg_replace(['/a/', '/(/'], 'x', 'a'), preg_last_error_msg());
/// ```
/// ```expect
/// string(0) ""
/// string(2) "xx"
/// array(3) {
///   ["k"]=>
///   string(2) "a#"
///   [5]=>
///   string(3) "b##"
///   [6]=>
///   string(1) "#"
/// }
/// preg_replace(): Argument #1 ($pattern) must be of type array when argument #2 ($replacement) is an array, string given
/// preg_replace(): Argument #2 ($replacement) must be of type array|string, stdClass given
/// string(2) "2x"
/// string(3) "x.5"
/// string(1) "x"
///
/// Warning: preg_replace(): Compilation failed: missing closing parenthesis at offset 1 in %s on line 6
///
/// Warning: preg_replace(): Compilation failed: missing closing parenthesis at offset 1 in %s on line 6
/// NULL
/// NULL
/// string(14) "Internal error"
/// ```
pub(crate) fn preg_replace(req: &mut Request) -> Result<Value, Thrown> {
    replace_common(req, false)
}

/// `preg_filter`: `preg_replace` that answers only the subjects in
/// which something was replaced, `null` for a string one that had none.
///
/// ```php
/// var_dump(preg_filter('/\d/', '#', ['a1', 'b', 'c2']), preg_filter('/\d/', '#', 'abc'), preg_filter(['/a/', '/\d/'], ['A', '#'], ['a1', 'b', 'x']));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(2) "a#"
///   [2]=>
///   string(2) "c#"
/// }
/// NULL
/// array(1) {
///   [0]=>
///   string(2) "A#"
/// }
/// ```
pub(crate) fn preg_filter(req: &mut Request) -> Result<Value, Thrown> {
    replace_common(req, true)
}

/// `preg_replace_common` and `_preg_replace_common`.
fn replace_common(req: &mut Request, filter: bool) -> Result<Value, Thrown> {
    arguments_between(req, 3, 5)?;
    let regex = either_argument(req, 1)?;
    let replacement = either_argument(req, 2)?;
    let subject = either_argument(req, 3)?;
    let limit = limit_of(long_argument(req, 4, -1)?);
    if matches!(replacement, Either::Array(_)) && matches!(regex, Either::Str(_)) {
        return Err(arg_type_error(
            req,
            1,
            "be of type array when argument #2 ($replacement) is an array, string given",
        ));
    }
    let mut count = 0;
    let keep = |before: usize, after: usize| !filter || after > before;
    let result = match subject {
        Either::Str(subject) => {
            match in_subject(req, regex, replacement, subject, limit, &mut count)? {
                Some(result) if keep(0, count) => Value::string(result),
                Some(result) => {
                    req.release(Value::string(result));
                    Value::null()
                }
                None => Value::null(),
            }
        }
        Either::Array(subjects) => over_array(
            req,
            subjects,
            &mut count,
            |req, subject, count| in_subject(req, regex, replacement, subject, limit, count),
            keep,
        )?,
    };
    if req.argument_count() >= 5 {
        assign_ref(req, 5, Value::int(i64::try_from(count).unwrap_or(i64::MAX)));
    }
    Ok(result)
}

/// `php_preg_replace_func_impl`: the subject replaced, or each of an
/// array's, and the count of replacements made.
fn func_impl(
    req: &mut Request,
    regex: Either,
    callback: &Callable,
    subject: Either,
    limit: usize,
    flags: i64,
) -> Result<(Value, usize), Thrown> {
    let mut count = 0;
    let value = match subject {
        Either::Str(subject) => {
            match in_subject_func(req, regex, callback, subject, limit, &mut count, flags)? {
                Some(result) => Value::string(result),
                None => Value::null(),
            }
        }
        Either::Array(subjects) => over_array(
            req,
            subjects,
            &mut count,
            |req, subject, count| {
                in_subject_func(req, regex, callback, subject, limit, count, flags)
            },
            |_, _| true,
        )?,
    };
    Ok((value, count))
}

/// `preg_replace_callback(string|array $pattern, callable $callback,
/// string|array $subject, int $limit = -1, &$count = null, int $flags =
/// 0): string|array|null`: each match replaced by what the callback
/// returns for the match's array, built as `preg_match` builds it under
/// `$flags`.
///
/// ```php
/// var_dump(preg_replace_callback('/(?<d>\d)/', fn($m) => $m['d'] * 2 . count($m), 'a1b2', -1, $c), $c);
/// var_dump(preg_replace_callback('/b(x)?/', function ($m) { var_dump($m); return 1.5; }, 'ab', 1, $c, PREG_UNMATCHED_AS_NULL | PREG_OFFSET_CAPTURE), $c);
/// var_dump(preg_replace_callback(['/a/', '/b/'], fn($m) => strtoupper($m[0]), ['x' => 'ab', 'cb']));
/// ```
/// ```expect
/// string(6) "a23b43"
/// int(2)
/// array(2) {
///   [0]=>
///   array(2) {
///     [0]=>
///     string(1) "b"
///     [1]=>
///     int(1)
///   }
///   [1]=>
///   array(2) {
///     [0]=>
///     NULL
///     [1]=>
///     int(-1)
///   }
/// }
/// string(4) "a1.5"
/// int(1)
/// array(2) {
///   ["x"]=>
///   string(2) "AB"
///   [0]=>
///   string(2) "cB"
/// }
/// ```
///
/// What the callback throws propagates, and a callback that is not one
/// is the `TypeError` before anything matches.
///
/// ```php
/// try { preg_replace_callback('/a/', function ($m) { throw new Exception('in callback'); }, 'a'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// var_dump(preg_last_error_msg());
/// try { preg_replace_callback('/a/', 'no_such_function', 'a'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(preg_replace_callback('/a/', fn($m) => preg_replace_callback('/./', fn($n) => '<' . $n[0] . '>', 'xy'), 'bab'));
/// ```
/// ```expect
/// in callback
/// string(14) "Internal error"
/// preg_replace_callback(): Argument #2 ($callback) must be a valid callback, function "no_such_function" not found or invalid function name
/// string(8) "b<x><y>b"
/// ```
pub(crate) fn preg_replace_callback(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 3, 6)?;
    let regex = either_argument(req, 1)?;
    let callback = callable_argument(req, 2)?;
    let subject = either_argument(req, 3)?;
    let limit = limit_of(long_argument(req, 4, -1)?);
    let flags = long_argument(req, 6, 0)?;
    let (value, count) = func_impl(req, regex, &callback, subject, limit, flags)?;
    if req.argument_count() >= 5 {
        assign_ref(req, 5, Value::int(i64::try_from(count).unwrap_or(i64::MAX)));
    }
    Ok(value)
}

/// `preg_replace_callback_array(array $pattern, string|array $subject,
/// int $limit = -1, &$count = null, int $flags = 0):
/// string|array|null`: each pattern key with its callback, in the
/// array's order, over what the one before left.
///
/// ```php
/// var_dump(preg_replace_callback_array(['/\d+/' => fn($m) => $m[0] * 2, '/[a-z]/' => fn($m) => strtoupper($m[0])], ['a1', 'b22'], -1, $c), $c);
/// try { preg_replace_callback_array(['x'], 'a'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { preg_replace_callback_array(['/a/' => 'no_such_function'], 'a'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(preg_replace_callback_array(['/(/' => fn($m) => ''], 'a', -1, $c), $c);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(2) "A2"
///   [1]=>
///   string(3) "B44"
/// }
/// int(4)
/// preg_replace_callback_array(): Argument #1 ($pattern) must contain only string patterns as keys
/// preg_replace_callback_array(): Argument #1 ($pattern) must contain only valid callbacks
///
/// Warning: preg_replace_callback_array(): Compilation failed: missing closing parenthesis at offset 1 in %s on line 5
/// NULL
/// int(4)
/// ```
pub(crate) fn preg_replace_callback_array(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 2, 5)?;
    let given = req.argument(1).unwrap_or_default();
    let patterns = coerce_param(req, &given, &Type::Array, 1)?.deref();
    let subject = either_argument(req, 2)?;
    let limit = limit_of(long_argument(req, 3, -1)?);
    let flags = long_argument(req, 5, 0)?;
    let Some(patterns) = patterns.as_array().copied() else {
        return Ok(Value::null());
    };
    let mut current = subject;
    let own = |req: &mut Request, value: Either| match value {
        Either::Str(s) => req.addref(&Value::string(s)),
        Either::Array(a) => req.addref(&a.to_value()),
    };
    let release = |req: &mut Request, value: Either| match value {
        Either::Str(s) => req.release(Value::string(s)),
        Either::Array(a) => req.release(a.to_value()),
    };
    own(req, current);
    let mut total = 0;
    for (key, callback) in patterns.snapshot() {
        let Key::Str(regex) = key else {
            release(req, current);
            return Err(arg_type_error(
                req,
                1,
                "contain only string patterns as keys",
            ));
        };
        let Ok(callable) = callable_checked(req, &callback) else {
            release(req, current);
            return Err(arg_type_error(req, 1, "contain only valid callbacks"));
        };
        let replaced = func_impl(req, Either::Str(regex), &callable, current, limit, flags);
        release(req, current);
        let (value, count) = replaced?;
        total += count;
        if let Some(array) = value.as_array() {
            current = Either::Array(*array);
        } else if let Some(string) = value.as_php_string() {
            current = Either::Str(*string);
        } else {
            return Ok(Value::null());
        }
    }
    if req.argument_count() >= 4 {
        assign_ref(req, 4, Value::int(i64::try_from(total).unwrap_or(i64::MAX)));
    }
    Ok(match current {
        Either::Str(s) => Value::string(s),
        Either::Array(a) => a.to_value(),
    })
}

#[cfg(test)]
mod tests {
    use super::backref;

    #[test]
    fn a_reference_is_one_or_two_digits_braced_or_not() {
        assert_eq!(backref(b"$1", 0), Some((1, 2)));
        assert_eq!(backref(b"$12x", 0), Some((12, 3)));
        assert_eq!(backref(b"${3}", 0), Some((3, 4)));
        assert_eq!(backref(b"${3", 0), None);
        assert_eq!(backref(b"\\9", 0), Some((9, 2)));
        assert_eq!(backref(b"$", 0), None);
        assert_eq!(backref(b"$a", 0), None);
    }
}
