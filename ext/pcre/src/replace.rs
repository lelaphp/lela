//! `php_pcre_replace_impl` and `php_pcre_replace_func_impl`
//! (`Z/ext/pcre/php_pcre.c:1616`, `:1860`) and the four natives over
//! them: every match of a pattern, up to a limit, replaced by a string
//! with `$n`, `${n}` and `\n` references or by what a callback returns,
//! over a string subject or each string of an array one, a pattern array
//! applied in turn.

use std::ops::ControlFlow;

use php_ext::abi::{Callable, Ref, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::call::call;
use php_ext::coerce::{callable_checked, to_string, with_string};
use php_ext::raising::{Level, Thrown, arg_type_error, docref, raise_while_pending};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{FreshString, Key, Owner, PhpArray, PhpString, Value, ValueRef};

use crate::Either;
use crate::constants::{PREG_INTERNAL_ERROR, PREG_NO_ERROR};
use crate::ffi::{self, Subject};
use crate::matching::{Scratch, populate, unit_length};
use crate::pattern::{Pattern, compiled, compiled_under};
use crate::{exec_error, set_error};

/// What a replacing native answers, `string|array|null`.
type Replacement = Option<Union<Owner<PhpString>, Owner<PhpArray>>>;

/// A replacing native's answer given up, where the `$count` written
/// after it threw.
fn give_up(req: &mut Request, value: Replacement) {
    match value {
        Some(Union::A(string)) => string.give_up(req),
        Some(Union::B(array)) => array.give_up(req),
        None => {}
    }
}

/// What one subject came to.
enum Replaced {
    /// Nothing matched: the subject itself, `zend_string_copy`.
    Same,
    Built(Owner<PhpString>),
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
/// An `each` that answers `false` has no replacement, php's `NULL`
/// `eval_result`: the subject fails at that match with
/// `PREG_INTERNAL_ERROR`, as a throw from it does.
fn replace_loop(
    req: &mut Request,
    pattern: &Pattern,
    subject: &[u8],
    mut limit: usize,
    count: &mut usize,
    mut each: impl FnMut(&mut Request, &Scratch, usize, &mut Vec<u8>) -> Result<bool, Thrown>,
) -> Result<Replaced, Thrown> {
    set_error(req, PREG_NO_ERROR);
    let Some(mut scratch) = Scratch::take(req, pattern) else {
        return Ok(Replaced::Failed);
    };
    let len = subject.len();
    let mut searched = Subject::new(subject);
    let mut out: Option<Built> = None;
    let mut piece = Vec::new();
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
                    set_error(req, PREG_INTERNAL_ERROR);
                    break 'search Ok(Replaced::Failed);
                }
                *count += 1;
                let n = usize::try_from(rc).unwrap_or(0);
                piece.clear();
                match each(req, &scratch, n, &mut piece) {
                    Ok(true) => {}
                    Ok(false) => {
                        set_error(req, PREG_INTERNAL_ERROR);
                        break 'search Ok(Replaced::Failed);
                    }
                    Err(thrown) => {
                        set_error(req, PREG_INTERNAL_ERROR);
                        scratch.give_back(req);
                        if let Some(out) = out {
                            out.free(req);
                        }
                        return Err(thrown);
                    }
                }
                // The part before the match and the replacement, in a
                // string grown to twice what it comes to where it reaches
                // its size, fallibly since 8.5.12: `goto error` where the
                // memory limit refused it.
                let before = subject.get(last_end..match_start).unwrap_or_default();
                let built = out.get_or_insert_with(Built::default);
                match built.room(req, before.len() + piece.len()) {
                    Ok(true) => {}
                    Ok(false) => {
                        exec_error(req, rc);
                        break 'search Ok(Replaced::Failed);
                    }
                    Err(thrown) => {
                        scratch.give_back(req);
                        if let Some(out) = out {
                            out.free(req);
                        }
                        return Err(thrown);
                    }
                }
                built.push(before);
                built.push(&piece);
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
                            break 'search finished(req, out.take(), subject, last_end);
                        }
                    } else {
                        exec_error(req, rc);
                        break 'search Ok(Replaced::Failed);
                    }
                }
                break;
            }
        } else if rc == ffi::PCRE2_ERROR_NOMATCH || limit == 0 {
            break finished(req, out.take(), subject, last_end);
        } else {
            exec_error(req, rc);
            break Ok(Replaced::Failed);
        }
        rc = scratch.run(pattern, &mut searched, start, 0, true);
    };
    scratch.give_back(req);
    // `zend_string_release_ex(result, 0)` where the subject failed.
    if let Some(out) = out {
        out.free(req);
    }
    replaced
}

/// `not_matched:` the subject itself where nothing was replaced, and
/// otherwise the string built with the rest of the subject after it.
fn finished(
    req: &mut Request,
    out: Option<Built>,
    subject: &[u8],
    last_end: usize,
) -> Result<Replaced, Thrown> {
    match out {
        None => Ok(Replaced::Same),
        Some(out) => out
            .finish(req, subject.get(last_end..).unwrap_or_default())
            .map(Replaced::Built),
    }
}

/// php's `result`, `result_len` and `alloc_len`: the string a subject's
/// replacement is built in on the request's heap, laid down at the first
/// match and grown where what it would hold reaches its size.
#[derive(Default)]
struct Built {
    string: Option<FreshString>,
    len: usize,
}

impl Built {
    /// Room for `more` bytes after what is held: where the length it
    /// comes to reaches the allocation, `zend_string_falloc` or
    /// `zend_string_fextend` of twice that length (`alloc_len =
    /// zend_safe_address_guarded(2, new_len, ZSTR_MAX_OVERHEAD) -
    /// ZSTR_MAX_OVERHEAD`), asked for fallibly since 8.5.12; `false`
    /// where the memory limit refused it.
    fn room(&mut self, req: &mut Request, more: usize) -> Result<bool, Thrown> {
        let new_len = self.len.saturating_add(more);
        if let Some(string) = &self.string
            && new_len < string.len()
        {
            return Ok(true);
        }
        let alloc_len = new_len.saturating_mul(2);
        let held = self.string.take();
        let grown = req.memory_mut().fallible(|req| match held {
            None => FreshString::new(req, alloc_len),
            Some(string) => string.extend(req, alloc_len),
        })?;
        self.string = grown;
        Ok(self.string.is_some())
    }

    /// `bytes` copied in after what is held, in the room [`Built::room`]
    /// made.
    fn push(&mut self, bytes: &[u8]) {
        let end = self.len + bytes.len();
        if let Some(room) = self
            .string
            .as_mut()
            .and_then(|string| string.bytes_mut().get_mut(self.len..end))
        {
            room.copy_from_slice(bytes);
            self.len = end;
        }
    }

    /// "Now we know exactly how long it is": `zend_string_realloc` to
    /// what is held and `rest`, and `rest` copied in after it.
    fn finish(mut self, req: &mut Request, rest: &[u8]) -> Result<Owner<PhpString>, Thrown> {
        let exact = self.len + rest.len();
        let mut string = match self.string.take() {
            None => FreshString::new(req, exact)?,
            Some(string) if exact > string.len() => string.extend(req, exact)?,
            Some(string) if exact < string.len() => string.truncate(req, exact)?,
            Some(string) => string,
        };
        if let Some(room) = string.bytes_mut().get_mut(self.len..exact) {
            room.copy_from_slice(rest);
        }
        Ok(string.finish())
    }

    /// `zend_string_release_ex(result, 0)`.
    fn free(self, req: &mut Request) {
        if let Some(string) = self.string {
            string.free(req);
        }
    }
}

/// The subject a replacement came to, as a string the caller owns, or
/// `None` for php's `NULL`.
fn owned(req: &mut Request, subject: PhpString, replaced: Replaced) -> Option<Owner<PhpString>> {
    match replaced {
        Replaced::Same => Some(Owner::take(req, subject)),
        Replaced::Built(string) => Some(string),
        Replaced::Failed => None,
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
) -> Result<Option<Owner<PhpString>>, Thrown> {
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
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let bytes = subject.as_bytes();
    let replaced = replace_loop(req, pattern, bytes, limit, count, |_, scratch, n, out| {
        expand(replacement, bytes, scratch, n, out);
        Ok(true)
    })?;
    Ok(owned(req, subject, replaced))
}

/// What a callback replacement runs under, php's `fci` and `fcc` with
/// the arguments every subject shares: the callback, the limit per
/// subject and pattern, `$flags`, and what an earlier subject threw,
/// pending, which the callback is never entered under.
#[derive(Clone, Copy)]
struct Calling<'a> {
    callback: &'a Callable,
    limit: usize,
    flags: i64,
    pending: Option<&'a Thrown>,
}

/// `php_pcre_replace_func`: one pattern string over one subject, each
/// match replaced by what `callback` returns for its array. While what
/// an earlier subject threw is `pending`, the pattern is compiled and
/// matched as ever, but `zend_call_function` will not enter the callback,
/// so the first match has no replacement and the subject fails there.
fn replace_one_func(
    req: &mut Request,
    regex: &[u8],
    subject: PhpString,
    calling: Calling<'_>,
    count: &mut usize,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let Calling {
        callback,
        limit,
        flags,
        pending,
    } = calling;
    let Some(pattern) = compiled_under(req, regex, pending) else {
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
            if pending.is_some() {
                return Ok(false);
            }
            let mut groups = PhpArray::new(req)?;
            // `preg_do_repl_func`'s `NULL` where the memory limit was met
            // filling the groups, since 8.5.12.
            if !populate(req, &mut groups, &pattern, scratch, bytes, n, flags)? {
                groups.give_up(req);
                return Ok(false);
            }
            let argument = groups.into_value();
            let returned = call(req, callback, &[*argument]);
            argument.give_up(req);
            let returned = returned?;
            let read = with_string(req, &returned, |text| out.extend_from_slice(text));
            returned.give_up(req);
            read?;
            Ok(true)
        },
    )?;
    Ok(owned(req, subject, replaced))
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
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let patterns = match regex {
        Union::B(regex) => {
            let Union::B(replacement) = replacement else {
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
        Union::A(patterns) => patterns,
    };
    let replacements = match replacement {
        Union::A(replacements) => Some(replacements),
        Union::B(_) => None,
    };
    let mut current = Some(Owner::take(req, subject));
    let mut replace_at = 0;
    // `php_pcre_replace_array`'s `ZEND_HASH_FOREACH_VAL(regex, regex_entry)`
    // (`Z/ext/pcre/php_pcre.c:2093`), no owner of the patterns taken: the
    // argument holds them while `zval_get_tmp_string` runs `__toString`.
    // The replacement is read at the next bucket that holds one
    // (`ZEND_HASH_ELEMENT(replace_ht, replace_idx)`), as it stands then.
    let walked = walk::foreach(req, patterns, Hold::None, |req, row| -> walk::Step {
        let Some(subject) = current.take() else {
            return Ok(ControlFlow::Break(()));
        };
        let regex = match to_string(req, &row.value().deref()) {
            Ok(regex) => regex,
            Err(thrown) => {
                subject.give_up(req);
                return Err(thrown);
            }
        };
        let replacement = match (replacements, replacement) {
            (Some(all), _) => match all.live_from(replace_at) {
                Some((found, _, value)) => {
                    replace_at = found.saturating_add(1);
                    to_string(req, &value.deref())
                }
                None => PhpString::new(req, b""),
            },
            (None, Union::B(text)) => Ok(Owner::take(req, text)),
            (None, Union::A(_)) => PhpString::new(req, b""),
        };
        let replacement = match replacement {
            Ok(replacement) => replacement,
            Err(thrown) => {
                regex.give_up(req);
                subject.give_up(req);
                return Err(thrown);
            }
        };
        let result = replace_one(
            req,
            regex.as_bytes(),
            *subject,
            replacement.as_bytes(),
            limit,
            count,
        );
        replacement.give_up(req);
        regex.give_up(req);
        subject.give_up(req);
        current = result?;
        Ok(if current.is_some() {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        })
    });
    if let Err(thrown) = walked {
        if let Some(current) = current {
            current.give_up(req);
        }
        return Err(thrown);
    }
    Ok(current)
}

/// `php_replace_in_subject_func`: [`in_subject`] with a callback. While
/// what an earlier subject threw is `pending`, a pattern of an array is
/// converted as php converts it then ([`string_while_pending`]), one that
/// will not convert ending the patterns with the subject as it stands.
fn in_subject_func(
    req: &mut Request,
    regex: Either,
    subject: PhpString,
    calling: Calling<'_>,
    count: &mut usize,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let patterns = match regex {
        Union::B(regex) => {
            return replace_one_func(req, regex.as_bytes(), subject, calling, count);
        }
        Union::A(patterns) => patterns,
    };
    let mut current = Some(Owner::take(req, subject));
    // `ZEND_HASH_FOREACH_VAL(regex_ht, regex_entry)`
    // (`Z/ext/pcre/php_pcre.c:2194`), no owner of the patterns taken: the
    // argument holds them while the callback and `__toString` run.
    let walked = walk::foreach(req, patterns, Hold::None, |req, row| -> walk::Step {
        let entry = row.value().deref();
        let converted = match calling.pending {
            None => to_string(req, &entry).map(Some),
            Some(pending) => string_while_pending(req, pending, &entry),
        };
        let regex = match converted {
            Ok(Some(regex)) => regex,
            Ok(None) => return Ok(ControlFlow::Break(())),
            Err(thrown) => return Err(thrown),
        };
        let Some(subject) = current.take() else {
            regex.give_up(req);
            return Ok(ControlFlow::Break(()));
        };
        let result = replace_one_func(req, regex.as_bytes(), *subject, calling, count);
        regex.give_up(req);
        subject.give_up(req);
        current = result?;
        Ok(if current.is_some() {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        })
    });
    if let Err(thrown) = walked {
        if let Some(current) = current {
            current.give_up(req);
        }
        return Err(thrown);
    }
    Ok(current)
}

/// `zval_try_get_string` while a throwable is pending: an array raises
/// php's warning while the throwable carries and will not convert
/// then, and neither will an object, since `__toString` is a call
/// `zend_call_function` will not enter and no second throwable is
/// raised over the first; `None` for either.
fn string_while_pending(
    req: &mut Request,
    pending: &Thrown,
    value: &Value,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    match value.deref().as_ref() {
        ValueRef::Array(_) => {
            raise_while_pending(req, pending, Level::Warning, "Array to string conversion");
            Ok(None)
        }
        ValueRef::Object(_) => Ok(None),
        _ => to_string(req, value).map(Some),
    }
}

/// `php_preg_replace_func_impl` over an array subject: each entry
/// converted and replaced under its key. What one entry's patterns or
/// callback throw does not end php's loop, which stops only at an entry
/// that will not convert: the entries after it are converted and their
/// patterns compiled and matched while the throwable is pending, which is
/// what reaches the caller at the end.
///
/// ```php
/// $t = function ($m) { throw new Exception('x'); };
/// try { preg_replace_callback(['/a/', '/(/'], $t, ['a', 'Array', 'b']); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { preg_replace_callback(['/a/', '/(/'], $t, ['a', [1], 'b']); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { preg_replace_callback([new stdClass, '/b/'], fn($m) => 1, ['a', ['x'], 'b']); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Warning: preg_replace_callback(): Compilation failed: missing closing parenthesis at offset 1 in %s on line 3
/// x
///
/// Warning: Array to string conversion in %s on line 4
/// x
///
/// Warning: Array to string conversion in %s on line 5
/// Object of class stdClass could not be converted to string
/// ```
fn over_array_func(
    req: &mut Request,
    regex: Either,
    subjects: PhpArray,
    calling: Calling<'_>,
    count: &mut usize,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    let mut pending: Option<Thrown> = None;
    // `ZEND_HASH_FOREACH_KEY_VAL(subject_ht, num_key, string_key,
    // subject_entry)` (`Z/ext/pcre/php_pcre.c:2247`), no owner of the table
    // taken: the argument holds it while the callback runs.
    let walked = walk::foreach(req, subjects, Hold::None, |req, row| -> walk::Step {
        let entry = row.value().deref();
        let converted = match &pending {
            None => to_string(req, &entry).map(Some),
            Some(thrown) => string_while_pending(req, thrown, &entry),
        };
        let subject = match converted {
            Ok(Some(subject)) => subject,
            Ok(None) => return Ok(ControlFlow::Break(())),
            Err(thrown) => {
                pending = Some(thrown);
                return Ok(ControlFlow::Break(()));
            }
        };
        let calling = Calling {
            pending: pending.as_ref(),
            ..calling
        };
        let result = in_subject_func(req, regex, *subject, calling, count);
        subject.give_up(req);
        match result {
            Ok(Some(result)) => {
                let key = row.key().clone();
                out.set(req, Owner::take(req, key), result.into_value())?;
            }
            Ok(None) => {}
            Err(thrown) => pending = Some(thrown),
        }
        Ok(ControlFlow::Continue(()))
    });
    if let Err(thrown) = walked {
        out.give_up(req);
        return Err(match pending {
            Some(earlier) => earlier.chain(req, thrown),
            None => thrown,
        });
    }
    match pending {
        Some(thrown) => {
            out.give_up(req);
            Err(thrown)
        }
        None => Ok(out),
    }
}

/// Each string of an array subject through `each`, under its key, the
/// entries `each` answers `None` for, or `keep` refuses, left out.
fn over_array(
    req: &mut Request,
    subjects: PhpArray,
    count: &mut usize,
    mut each: impl FnMut(
        &mut Request,
        PhpString,
        &mut usize,
    ) -> Result<Option<Owner<PhpString>>, Thrown>,
    keep: impl Fn(usize, usize) -> bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_KEY_VAL(subject_ht, num_key, string_key,
    // subject_entry)` (`Z/ext/pcre/php_pcre.c:2318`), no owner of the table
    // taken: the argument holds it while a subject's `__toString` runs.
    let walked = walk::foreach(req, subjects, Hold::None, |req, row| -> walk::Step {
        let before = *count;
        let subject = to_string(req, &row.value().deref())?;
        let result = each(req, *subject, count);
        subject.give_up(req);
        let Some(result) = result? else {
            return Ok(ControlFlow::Continue(()));
        };
        if !keep(before, *count) {
            result.give_up(req);
            return Ok(ControlFlow::Continue(()));
        }
        let key = row.key().clone();
        out.set(req, Owner::take(req, key), result.into_value())?;
        Ok(ControlFlow::Continue(()))
    });
    if let Err(thrown) = walked {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(out)
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
///
/// Since 8.5.12 the replaced string is grown fallibly, to twice what it
/// comes to: a growth the memory limit refuses fails the subject, and the
/// limit's fatal is raised as the call returns.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// $s = preg_replace('/a/', 'bbb', str_repeat('a', 1 << 20));
/// var_dump("unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 3145728 bytes) in %s on line 3
/// ```
#[php_function(frameless(3))]
fn preg_replace(
    req: &mut Request,
    pattern: Either,
    replacement: Either,
    subject: Either,
    #[php(default = -1)] limit: i64,
    #[php(default = null)] count: Ref<Untyped<Value>>,
) -> Result<Replacement, Thrown> {
    replace_common(req, pattern, replacement, subject, limit, &count, false)
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
#[php_function]
fn preg_filter(
    req: &mut Request,
    pattern: Either,
    replacement: Either,
    subject: Either,
    #[php(default = -1)] limit: i64,
    #[php(default = null)] count: Ref<Untyped<Value>>,
) -> Result<Replacement, Thrown> {
    replace_common(req, pattern, replacement, subject, limit, &count, true)
}

/// `preg_replace_common` and `_preg_replace_common` past their ZPP.
fn replace_common(
    req: &mut Request,
    regex: Either,
    replacement: Either,
    subject: Either,
    limit: i64,
    count_out: &Ref<Untyped<Value>>,
    filter: bool,
) -> Result<Replacement, Thrown> {
    let limit = limit_of(limit);
    if matches!(replacement, Union::A(_)) && matches!(regex, Union::B(_)) {
        return Err(arg_type_error(
            req,
            1,
            "be of type array when argument #2 ($replacement) is an array, string given",
        ));
    }
    let mut count = 0;
    let keep = |before: usize, after: usize| !filter || after > before;
    let result = match subject {
        Union::B(subject) => {
            match in_subject(req, regex, replacement, subject, limit, &mut count)? {
                Some(result) if keep(0, count) => Some(Union::A(result)),
                Some(result) => {
                    result.give_up(req);
                    None
                }
                None => None,
            }
        }
        Union::A(subjects) => Some(Union::B(over_array(
            req,
            subjects,
            &mut count,
            |req, subject, count| in_subject(req, regex, replacement, subject, limit, count),
            keep,
        )?)),
    };
    let written = count_out.set(
        req,
        Untyped(Value::int(i64::try_from(count).unwrap_or(i64::MAX))),
    );
    if let Err(thrown) = written {
        give_up(req, result);
        return Err(thrown);
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
) -> Result<(Replacement, usize), Thrown> {
    let mut count = 0;
    let calling = Calling {
        callback,
        limit,
        flags,
        pending: None,
    };
    let value = match subject {
        Union::B(subject) => {
            in_subject_func(req, regex, subject, calling, &mut count)?.map(Union::A)
        }
        Union::A(subjects) => Some(Union::B(over_array_func(
            req, regex, subjects, calling, &mut count,
        )?)),
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
///
/// Since 8.5.12 the replaced string is grown fallibly, as
/// `preg_replace`'s is, and the groups handed to the callback stop once
/// the memory limit was met.
///
/// ```php
/// ini_set('memory_limit', '4M');
/// $s = preg_replace_callback('/./', fn ($m) => 'xyz', str_repeat('a', 1 << 20));
/// var_dump("unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 4194304 bytes exhausted (tried to allocate 3162112 bytes) in %s on line 3
/// ```
#[php_function]
fn preg_replace_callback(
    req: &mut Request,
    pattern: Either,
    callback: Callable,
    subject: Either,
    #[php(default = -1)] limit: i64,
    #[php(default = null)] count: Ref<Untyped<Value>>,
    #[php(default = 0)] flags: i64,
) -> Result<Replacement, Thrown> {
    let limit = limit_of(limit);
    let (value, replaced) = func_impl(req, pattern, &callback, subject, limit, flags)?;
    let written = count.set(
        req,
        Untyped(Value::int(i64::try_from(replaced).unwrap_or(i64::MAX))),
    );
    if let Err(thrown) = written {
        give_up(req, value);
        return Err(thrown);
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
#[php_function]
fn preg_replace_callback_array(
    req: &mut Request,
    pattern: PhpArray,
    subject: Either,
    #[php(default = -1)] limit: i64,
    #[php(default = null)] count: Ref<Untyped<Value>>,
    #[php(default = 0)] flags: i64,
) -> Result<Replacement, Thrown> {
    let limit = limit_of(limit);
    let current = match subject {
        Union::A(a) => Union::A(Owner::take(req, a)),
        Union::B(s) => Union::B(Owner::take(req, s)),
    };
    let view = |value: &Union<Owner<PhpArray>, Owner<PhpString>>| -> Either {
        match value {
            Union::A(a) => Union::A(**a),
            Union::B(s) => Union::B(**s),
        }
    };
    let release = |req: &mut Request, value: Union<Owner<PhpArray>, Owner<PhpString>>| match value {
        Union::B(s) => s.give_up(req),
        Union::A(a) => a.give_up(req),
    };
    let mut total = 0;
    let mut current = Some(current);
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(pattern, str_idx_regex, replace)`
    // (`Z/ext/pcre/php_pcre.c:2462`), no owner of the table taken: the
    // argument holds it while each callback runs.
    let walked = walk::foreach(req, pattern, Hold::None, |req, row| -> walk::Step {
        let Some(subject) = current.take() else {
            return Ok(ControlFlow::Break(()));
        };
        let Key::Str(regex) = row.key().clone() else {
            release(req, subject);
            return Err(arg_type_error(
                req,
                1,
                "contain only string patterns as keys",
            ));
        };
        let Ok(callable) = callable_checked(req, &row.value().deref()) else {
            release(req, subject);
            return Err(arg_type_error(req, 1, "contain only valid callbacks"));
        };
        let replaced = func_impl(
            req,
            Union::B(regex),
            &callable,
            view(&subject),
            limit,
            flags,
        );
        release(req, subject);
        let (value, replaced) = replaced?;
        total += replaced;
        current = match value {
            Some(Union::A(string)) => Some(Union::B(string)),
            Some(Union::B(array)) => Some(Union::A(array)),
            None => None,
        };
        Ok(if current.is_some() {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        })
    });
    if let Err(thrown) = walked {
        if let Some(current) = current {
            release(req, current);
        }
        return Err(thrown);
    }
    let Some(current) = current else {
        return Ok(None);
    };
    let written = count.set(
        req,
        Untyped(Value::int(i64::try_from(total).unwrap_or(i64::MAX))),
    );
    if let Err(thrown) = written {
        release(req, current);
        return Err(thrown);
    }
    Ok(Some(match current {
        Union::A(array) => Union::B(array),
        Union::B(string) => Union::A(string),
    }))
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
