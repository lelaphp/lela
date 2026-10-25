//! `php_pcre_split_impl` and `php_pcre_grep_impl`
//! (`Z/ext/pcre/php_pcre.c:2567`, `:2928`): a subject cut at each match,
//! and the entries of an array that match or, inverted, do not.

use std::ops::ControlFlow;

use php_ext::abi::OrFalse;
use php_ext::attr::php_function;
use php_ext::coerce::with_string;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpString};

use crate::constants::{
    PREG_GREP_INVERT, PREG_INTERNAL_ERROR, PREG_NO_ERROR, PREG_SPLIT_DELIM_CAPTURE,
    PREG_SPLIT_NO_EMPTY, PREG_SPLIT_OFFSET_CAPTURE,
};
use crate::ffi::{self, Subject};
use crate::matching::{Scratch, offset_pair, piece, unit_length};
use crate::pattern::{Pattern, compiled};
use crate::{error_code, exec_error, set_error};

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
///
/// Since 8.5.12 the pieces stop once the memory limit was met, and the
/// limit's fatal is raised as the call returns.
///
/// ```php
/// ini_set('memory_limit', '4M');
/// $p = preg_split('//', str_repeat('a', 1200000));
/// var_dump("unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 4194304 bytes exhausted (tried to allocate 2113536 bytes) in %s on line 3
/// ```
#[php_function]
fn preg_split(
    req: &mut Request,
    pattern: PhpString,
    subject: PhpString,
    #[php(default = -1)] limit: i64,
    #[php(default = 0)] flags: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Some(pattern) = compiled(req, pattern.as_bytes()) else {
        return Ok(OrFalse::False);
    };
    split_impl(req, &pattern, subject.as_bytes(), limit, flags)
}

/// `php_pcre_split_impl`.
#[allow(clippy::too_many_lines)]
pub(super) fn split_impl(
    req: &mut Request,
    pattern: &Pattern,
    bytes: &[u8],
    mut limit: i64,
    flags: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let no_empty = flags & PREG_SPLIT_NO_EMPTY != 0;
    let delimiters = flags & PREG_SPLIT_DELIM_CAPTURE != 0;
    let offsets = flags & PREG_SPLIT_OFFSET_CAPTURE != 0;
    let mut out = PhpArray::new(req)?;
    let len = bytes.len();
    let mut last = 0;
    set_error(req, PREG_NO_ERROR);
    let add = |req: &mut Request, out: &mut Owner<PhpArray>, start: usize, end: usize| {
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
            out.give_up(req);
            return Ok(OrFalse::False);
        };
        let mut subject = Subject::new(bytes);
        let mut start;
        let mut rc = scratch.run(pattern, &mut subject, 0, 0, true);
        let walked: Result<(), Thrown> = 'search: loop {
            // The pieces stop once the memory limit was met, with
            // `PREG_INTERNAL_ERROR`, since 8.5.12.
            if req.memory().has_oom() {
                set_error(req, PREG_INTERNAL_ERROR);
                break Ok(());
            }
            if rc >= 0 {
                if rc == 0 {
                    docref(req, Level::Notice, "Matched, but too many substrings");
                    rc = i32::try_from(pattern.groups).unwrap_or(i32::MAX);
                }
                loop {
                    let (match_start, match_end) = scratch.group(0);
                    if match_end < match_start {
                        set_error(req, PREG_INTERNAL_ERROR);
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
        if error_code(req) != PREG_NO_ERROR {
            out.give_up(req);
            return Ok(OrFalse::False);
        }
    }
    if !no_empty || last < len {
        add(req, &mut out, last, len)?;
    }
    Ok(OrFalse::Value(out))
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
#[php_function]
fn preg_grep(
    req: &mut Request,
    pattern: PhpString,
    array: PhpArray,
    #[php(default = 0)] flags: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Some(pattern) = compiled(req, pattern.as_bytes()) else {
        return Ok(OrFalse::False);
    };
    let invert = flags & PREG_GREP_INVERT != 0;
    let mut out = PhpArray::new(req)?;
    set_error(req, PREG_NO_ERROR);
    let Some(mut scratch) = Scratch::take(req, &pattern) else {
        return Ok(OrFalse::Value(out));
    };
    // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(input), num_key, string_key,
    // entry)` (`Z/ext/pcre/php_pcre.c:2966`), no owner of the table taken: the
    // argument holds it while `__toString` and a notice's handler run.
    let kept = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let entry = *row.value();
        let rc = with_string(req, &entry, |text| {
            let mut subject = Subject::new(text);
            scratch.run(&pattern, &mut subject, 0, 0, true)
        })?;
        let keep = if rc >= 0 {
            if rc == 0 {
                docref(req, Level::Notice, "Matched, but too many substrings");
            }
            !invert
        } else if rc == ffi::PCRE2_ERROR_NOMATCH {
            invert
        } else {
            exec_error(req, rc);
            return Ok(ControlFlow::Break(()));
        };
        if keep {
            let entry = Owner::take(req, entry);
            let key = row.key().clone();
            out.set(req, Owner::take(req, key), entry)?;
        }
        Ok(ControlFlow::Continue(()))
    })
    .map(|_| ());
    scratch.give_back(req);
    if let Err(thrown) = kept {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(OrFalse::Value(out))
}
