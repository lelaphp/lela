//! `php_pcre_split_impl` and `php_pcre_grep_impl`
//! (`Z/ext/pcre/php_pcre.c:2567`, `:2928`): a subject cut at each match,
//! and the entries of an array that match or, inverted, do not.

use php_ext::coerce::{coerce_param, to_string};
use php_ext::decl::Type;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, Value};

use crate::ffi::{self, Subject};
use crate::matching::{Scratch, offset_pair, piece, unit_length};
use crate::pattern::{Pattern, compiled};
use crate::{
    GREP_INVERT, NO_ERROR, SPLIT_DELIM_CAPTURE, SPLIT_NO_EMPTY, SPLIT_OFFSET_CAPTURE,
    arguments_between, error_code, exec_error, long_argument, set_error, string_argument,
};

/// `preg_split(string $pattern, string $subject, int $limit = -1, int
/// $flags = 0): array|false`: the pieces between matches, at most
/// `$limit` of them where it is above zero, the empty ones left out
/// under `PREG_SPLIT_NO_EMPTY`, the groups of each match between them
/// under `PREG_SPLIT_DELIM_CAPTURE`, and each as `[piece, offset]` under
/// `PREG_SPLIT_OFFSET_CAPTURE`.
///
/// ```php
/// var_dump(preg_split('/[\s,]+/', "a, b  c,d"), preg_split('/,/', 'a,b,c', 2), preg_split('//', 'abc', -1, PREG_SPLIT_NO_EMPTY), preg_split('/(-)/', 'a-b', -1, PREG_SPLIT_DELIM_CAPTURE | PREG_SPLIT_OFFSET_CAPTURE));
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
///   [2]=>
///   string(1) "c"
///   [3]=>
///   string(1) "d"
/// }
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(3) "b,c"
/// }
/// array(3) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
///   [2]=>
///   string(1) "c"
/// }
/// array(3) {
///   [0]=>
///   array(2) {
///     [0]=>
///     string(1) "a"
///     [1]=>
///     int(0)
///   }
///   [1]=>
///   array(2) {
///     [0]=>
///     string(1) "-"
///     [1]=>
///     int(1)
///   }
///   [2]=>
///   array(2) {
///     [0]=>
///     string(1) "b"
///     [1]=>
///     int(2)
///   }
/// }
/// ```
///
/// ```php
/// var_dump(preg_split('/x/', 'abc'), preg_split('/,/', ',a,', 0), preg_split('/,/', ',a,', -1, PREG_SPLIT_NO_EMPTY), preg_split('/,/', 'a,b', 1), preg_split('//u', "\xc3\xa9a", -1, PREG_SPLIT_NO_EMPTY), preg_split('/(a)|b/', 'xbyaz', -1, PREG_SPLIT_DELIM_CAPTURE | PREG_SPLIT_NO_EMPTY), preg_split('/(/', 'a'), preg_split('/./u', "\xff"));
/// ```
/// ```expect
///
/// Warning: preg_split(): Compilation failed: missing closing parenthesis at offset 1 in %s on line 2
/// array(1) {
///   [0]=>
///   string(3) "abc"
/// }
/// array(3) {
///   [0]=>
///   string(0) ""
///   [1]=>
///   string(1) "a"
///   [2]=>
///   string(0) ""
/// }
/// array(1) {
///   [0]=>
///   string(1) "a"
/// }
/// array(1) {
///   [0]=>
///   string(3) "a,b"
/// }
/// array(2) {
///   [0]=>
///   string(2) "é"
///   [1]=>
///   string(1) "a"
/// }
/// array(4) {
///   [0]=>
///   string(1) "x"
///   [1]=>
///   string(1) "y"
///   [2]=>
///   string(1) "a"
///   [3]=>
///   string(1) "z"
/// }
/// bool(false)
/// bool(false)
/// ```
pub(crate) fn preg_split(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 2, 4)?;
    let regex = string_argument(req, 1)?;
    let subject = string_argument(req, 2)?;
    let limit = long_argument(req, 3, -1)?;
    let flags = long_argument(req, 4, 0)?;
    let Some(pattern) = compiled(req, regex.as_bytes()) else {
        return Ok(Value::bool(false));
    };
    split_impl(req, &pattern, subject.as_bytes(), limit, flags)
}

/// `php_pcre_split_impl`.
#[allow(clippy::too_many_lines)]
fn split_impl(
    req: &mut Request,
    pattern: &Pattern,
    bytes: &[u8],
    mut limit: i64,
    flags: i64,
) -> Result<Value, Thrown> {
    let no_empty = flags & SPLIT_NO_EMPTY != 0;
    let delimiters = flags & SPLIT_DELIM_CAPTURE != 0;
    let offsets = flags & SPLIT_OFFSET_CAPTURE != 0;
    let mut out = PhpArray::new(req)?;
    let len = bytes.len();
    let mut last = 0;
    set_error(req, NO_ERROR);
    let add = |req: &mut Request, out: &mut PhpArray, start: usize, end: usize| {
        let value = if offsets {
            offset_pair(req, bytes, (start, end), false)?
        } else {
            piece(req, bytes, start, end)?
        };
        out.append(req, value)
    };
    if limit == 0 {
        limit = -1;
    }
    if limit == -1 || limit > 1 {
        let Some(mut scratch) = Scratch::take(req, pattern) else {
            req.release(out.to_value());
            return Ok(Value::bool(false));
        };
        let mut subject = Subject::new(bytes);
        let mut start;
        let mut rc = scratch.run(pattern, &mut subject, 0, 0, true);
        let walked: Result<(), Thrown> = 'search: loop {
            if rc >= 0 {
                if rc == 0 {
                    docref(req, Level::Notice, "Matched, but too many substrings");
                    rc = i32::try_from(pattern.groups).unwrap_or(i32::MAX);
                }
                loop {
                    let (match_start, match_end) = scratch.group(0);
                    if match_end < match_start {
                        set_error(req, crate::INTERNAL_ERROR);
                        break 'search Ok(());
                    }
                    if !no_empty || match_start != last {
                        if let Err(thrown) = add(req, &mut out, last, match_start) {
                            break 'search Err(thrown);
                        }
                        if limit != -1 {
                            limit -= 1;
                        }
                    }
                    if delimiters {
                        let n = usize::try_from(rc).unwrap_or(0);
                        for group in 1..n {
                            let (s, e) = scratch.group(group);
                            if (!no_empty || s != e)
                                && let Err(thrown) = add(req, &mut out, s, e)
                            {
                                break 'search Err(thrown);
                            }
                        }
                    }
                    start = match_end;
                    last = match_end;
                    if start == match_start {
                        if limit != -1 && limit <= 1 {
                            break 'search Ok(());
                        }
                        rc = scratch.retry(pattern, &mut subject, start);
                        if rc >= 0 {
                            continue;
                        } else if rc == ffi::PCRE2_ERROR_NOMATCH {
                            if start < len {
                                start += unit_length(pattern, bytes, start);
                            } else {
                                break 'search Ok(());
                            }
                        } else {
                            exec_error(req, rc);
                            break 'search Ok(());
                        }
                    }
                    break;
                }
            } else if rc == ffi::PCRE2_ERROR_NOMATCH {
                break Ok(());
            } else {
                exec_error(req, rc);
                break Ok(());
            }
            if limit != -1 && limit <= 1 {
                break Ok(());
            }
            rc = scratch.run(pattern, &mut subject, start, 0, true);
        };
        scratch.give_back(req);
        walked?;
        if error_code(req) != NO_ERROR {
            req.release(out.to_value());
            return Ok(Value::bool(false));
        }
    }
    if !no_empty || last < len {
        add(req, &mut out, last, len)?;
    }
    Ok(out.to_value())
}

/// `preg_grep(string $pattern, array $array, int $flags = 0):
/// array|false`: the entries whose string matches, under their keys, or
/// with `PREG_GREP_INVERT` those whose string does not.
///
/// ```php
/// var_dump(preg_grep('/^\d+$/', ['1', 'a', 'k' => '23', 4.5, 7]), preg_grep('/^\d+$/', ['1', 'a', 'k' => '23'], PREG_GREP_INVERT), preg_grep('/(/', []), preg_grep('/./u', ['a', "\xff", 'b']), preg_last_error());
/// ```
/// ```expect
///
/// Warning: preg_grep(): Compilation failed: missing closing parenthesis at offset 1 in %s on line 2
/// array(3) {
///   [0]=>
///   string(1) "1"
///   ["k"]=>
///   string(2) "23"
///   [3]=>
///   int(7)
/// }
/// array(1) {
///   [1]=>
///   string(1) "a"
/// }
/// bool(false)
/// array(1) {
///   [0]=>
///   string(1) "a"
/// }
/// int(4)
/// ```
pub(crate) fn preg_grep(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 2, 3)?;
    let regex = string_argument(req, 1)?;
    let given = req.argument(2).unwrap_or_default();
    let input = coerce_param(req, &given, &Type::Array, 2)?.deref();
    let flags = long_argument(req, 3, 0)?;
    let Some(pattern) = compiled(req, regex.as_bytes()) else {
        return Ok(Value::bool(false));
    };
    let invert = flags & GREP_INVERT != 0;
    let mut out = PhpArray::new(req)?;
    set_error(req, NO_ERROR);
    let Some(entries) = input.as_array().map(PhpArray::snapshot_refs) else {
        return Ok(out.to_value());
    };
    let Some(mut scratch) = Scratch::take(req, &pattern) else {
        return Ok(out.to_value());
    };
    let mut kept = Ok(());
    for (key, entry) in entries {
        let text = match to_string(req, &entry) {
            Ok(text) => text,
            Err(thrown) => {
                kept = Err(thrown);
                break;
            }
        };
        let mut subject = Subject::new(text.as_bytes());
        let rc = scratch.run(&pattern, &mut subject, 0, 0, true);
        req.release(Value::string(text));
        let keep = if rc >= 0 {
            if rc == 0 {
                docref(req, Level::Notice, "Matched, but too many substrings");
            }
            !invert
        } else if rc == ffi::PCRE2_ERROR_NOMATCH {
            invert
        } else {
            exec_error(req, rc);
            break;
        };
        if keep {
            req.addref(&entry);
            if let Key::Str(s) = &key {
                req.addref(&Value::string(*s));
            }
            if let Err(thrown) = out.set(req, key, entry) {
                kept = Err(thrown);
                break;
            }
        }
    }
    scratch.give_back(req);
    kept?;
    Ok(out.to_value())
}
