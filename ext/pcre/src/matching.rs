//! `php_pcre_match_impl` (`Z/ext/pcre/php_pcre.c:1150`) and what it
//! shares with the rest of the family: the scratch a call borrows from
//! the request, the loop's empty-match step, and `populate_subpat_array`,
//! the shape of one match's array, which `preg_replace_callback` hands
//! its callback too.

use php_ext::abi::{OrFalse, Ref, Untyped};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use crate::constants::{
    PREG_INTERNAL_ERROR, PREG_NO_ERROR, PREG_OFFSET_CAPTURE, PREG_PATTERN_ORDER, PREG_SET_ORDER,
    PREG_UNMATCHED_AS_NULL,
};
use crate::ffi::{self, Context, MatchData, Subject};
use crate::pattern::{Pattern, compiled};
use crate::{State, error_code, exec_error, limits, set_error};

/// `PHP_PCRE_PREALLOC_MDATA_SIZE`.
const PREALLOC_PAIRS: u32 = 32;

/// What one call matches with: the match data, the request's own while
/// no other call holds it and the pattern fits it, and `mctx`
/// with the limits the ini entries hold now.
pub(crate) struct Scratch {
    pub(crate) data: MatchData,
    pub(crate) context: Context,
    pooled: bool,
}

impl Scratch {
    /// php's `mdata_used` test and `pcre2_match_data_create_from_pattern`;
    /// `None`, with `PREG_INTERNAL_ERROR`, where the library had no memory.
    pub(crate) fn take(req: &mut Request, pattern: &Pattern) -> Option<Scratch> {
        let limits = limits(req);
        let state = req.state::<State>();
        let context = state.context.take().or_else(Context::new);
        let fits = pattern.groups <= PREALLOC_PAIRS as usize;
        let (data, pooled) = if fits {
            (
                state
                    .data
                    .take()
                    .or_else(|| MatchData::with_pairs(PREALLOC_PAIRS)),
                true,
            )
        } else {
            (MatchData::for_code(&pattern.code), false)
        };
        let (Some(mut context), Some(data)) = (context, data) else {
            set_error(req, PREG_INTERNAL_ERROR);
            return None;
        };
        context.configure(limits.backtrack, limits.recursion, limits.jit);
        Some(Scratch {
            data,
            context,
            pooled,
        })
    }

    /// The scratch handed back for the next call.
    pub(crate) fn give_back(self, req: &mut Request) {
        let state = req.state::<State>();
        if state.context.is_none() {
            state.context = Some(self.context);
        }
        if self.pooled && state.data.is_none() {
            state.data = Some(self.data);
        }
    }

    /// One match of `pattern` over `subject` from `start`.
    pub(crate) fn run(
        &mut self,
        pattern: &Pattern,
        subject: &mut Subject<'_>,
        start: usize,
        options: u32,
        jit: bool,
    ) -> i32 {
        ffi::run(
            &pattern.code,
            subject,
            start,
            options,
            &mut self.data,
            &self.context,
            jit,
        )
    }

    /// `pcre2_match` again at the end of an empty match, anchored and
    /// not empty, which is how php steps past one, never `pcre2_jit_match`.
    pub(crate) fn retry(&mut self, pattern: &Pattern, subject: &mut Subject<'_>, at: usize) -> i32 {
        self.run(
            pattern,
            subject,
            at,
            ffi::PCRE2_NOTEMPTY_ATSTART | ffi::PCRE2_ANCHORED,
            false,
        )
    }

    /// The offsets of group `group`, `PCRE2_UNSET` twice for one that did
    /// not take part.
    pub(crate) fn group(&self, group: usize) -> (usize, usize) {
        let ovector = self.data.ovector();
        (
            ovector.get(2 * group).copied().unwrap_or(ffi::PCRE2_UNSET),
            ovector
                .get(2 * group + 1)
                .copied()
                .unwrap_or(ffi::PCRE2_UNSET),
        )
    }
}

/// `calculate_unit_length`: one byte, or under `/u` the character that
/// starts at `at`.
pub(super) fn unit_length(pattern: &Pattern, subject: &[u8], at: usize) -> usize {
    if !pattern.utf {
        return 1;
    }
    1 + subject
        .get(at + 1..)
        .unwrap_or_default()
        .iter()
        .take_while(|b| *b & 0xc0 == 0x80)
        .count()
}

/// The bytes of `subject` between two offsets as a new string,
/// `ZVAL_STRINGL_FAST`.
pub(super) fn piece(
    req: &mut Request,
    subject: &[u8],
    start: usize,
    end: usize,
) -> Result<Owner<Value>, Thrown> {
    let bytes = subject.get(start..end).unwrap_or_default();
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// `populate_match_value`: the group's text, or for one that did not
/// take part `null` or the empty string.
fn match_value(
    req: &mut Request,
    subject: &[u8],
    (start, end): (usize, usize),
    unmatched_as_null: bool,
) -> Result<Owner<Value>, Thrown> {
    if start == ffi::PCRE2_UNSET {
        return if unmatched_as_null {
            Ok(Owner::null())
        } else {
            Ok(PhpString::new(req, b"")?.into_value())
        };
    }
    piece(req, subject, start, end)
}

/// `add_offset_pair`'s pair: `[text, offset]`, or for a group that did
/// not take part the request's one `[null or "", -1]`
/// ([`unmatched_pair`]).
pub(super) fn offset_pair(
    req: &mut Request,
    subject: &[u8],
    (start, end): (usize, usize),
    unmatched_as_null: bool,
) -> Result<Owner<Value>, Thrown> {
    if start == ffi::PCRE2_UNSET {
        return unmatched_pair(req, unmatched_as_null);
    }
    let mut pair = PhpArray::new(req)?;
    let text = piece(req, subject, start, end)?;
    pair.append(req, text)?;
    pair.append(req, Owner::int(i64::try_from(start).unwrap_or(i64::MAX)))?;
    Ok(pair.into_value())
}

/// `ZVAL_COPY(&match_pair, &PCRE_G(unmatched_empty_pair))`, or of
/// `unmatched_null_pair` under `PREG_UNMATCHED_AS_NULL`: one more owner
/// of the request's one pair, made on first use
/// (`init_unmatched_empty_pair`, `init_unmatched_null_pair`), so every
/// group that did not take part shares it and a `$matches` destroyed
/// with it gives up an array another owner holds, a possible root.
/// Once `RSHUTDOWN` gave the pairs up, each is made fresh.
///
/// ```php
/// function d($p) {
///     preg_match_all('#\{(!)?(\w+)\}#', $p, $m, PREG_OFFSET_CAPTURE | PREG_SET_ORDER);
///     gc_collect_cycles(); unset($m); $one = gc_status()['roots'];
///     preg_match('/(a)?(b)?c/', 'c', $n, PREG_OFFSET_CAPTURE | PREG_UNMATCHED_AS_NULL);
///     gc_collect_cycles(); unset($n); $two = gc_status()['roots'];
///     var_dump($one, $two);
/// }
/// d('/h/{name}');
/// preg_match('/(x)?(y)/', 'y', $o, PREG_OFFSET_CAPTURE);
/// var_dump($o);
/// ```
/// ```expect
/// int(1)
/// int(1)
/// array(3) {
///   [0]=>
///   array(2) {
///     [0]=>
///     string(1) "y"
///     [1]=>
///     int(0)
///   }
///   [1]=>
///   array(2) {
///     [0]=>
///     string(0) ""
///     [1]=>
///     int(-1)
///   }
///   [2]=>
///   array(2) {
///     [0]=>
///     string(1) "y"
///     [1]=>
///     int(0)
///   }
/// }
/// ```
pub(super) fn unmatched_pair(
    req: &mut Request,
    unmatched_as_null: bool,
) -> Result<Owner<Value>, Thrown> {
    let cached = {
        let unmatched = &req.state::<State>().unmatched;
        let slot = if unmatched_as_null {
            &unmatched.null
        } else {
            &unmatched.empty
        };
        (!unmatched.ended).then(|| slot.as_ref().map(|pair| **pair))
    };
    let pair = match cached {
        Some(Some(pair)) => pair,
        held => {
            let mut pair = PhpArray::new(req)?;
            let text = if unmatched_as_null {
                Owner::null()
            } else {
                PhpString::new(req, b"")?.into_value()
            };
            pair.append(req, text)?;
            pair.append(req, Owner::int(-1))?;
            // `EG_FLAGS_IN_SHUTDOWN`'s arm: a pair of its own.
            if held.is_none() {
                return Ok(pair.into_value());
            }
            let view = *pair;
            let unmatched = &mut req.state::<State>().unmatched;
            let slot = if unmatched_as_null {
                &mut unmatched.null
            } else {
                &mut unmatched.empty
            };
            *slot = Some(pair);
            view
        }
    };
    Ok(Owner::take(req, pair).into_value())
}

/// `add_named` and the numbered insert after it: the value under the
/// group's name, where it has one and, for a group that did not take
/// part, where no group of the name was written first (`J`), then
/// under the next number.
fn insert(
    req: &mut Request,
    into: &mut Owner<PhpArray>,
    name: Option<&[u8]>,
    value: Owner<Value>,
    unmatched: bool,
) -> Result<(), Thrown> {
    if let Some(name) = name
        && (!unmatched || into.find(name).is_none())
    {
        let named = Owner::take(req, *value);
        if let Err(thrown) = into.set_str(req, name, named) {
            value.give_up(req);
            return Err(thrown);
        }
    }
    into.append(req, value)
}

/// `populate_subpat_array`: one match's groups, each under its number
/// and its name, as text or as `[text, offset]` pairs, the groups past
/// the last that took part only under `PREG_UNMATCHED_AS_NULL`, and
/// `MARK` last where the match passed one.
pub(super) fn populate(
    req: &mut Request,
    into: &mut Owner<PhpArray>,
    pattern: &Pattern,
    scratch: &Scratch,
    subject: &[u8],
    count: usize,
    flags: i64,
) -> Result<(), Thrown> {
    let offsets = flags & PREG_OFFSET_CAPTURE != 0;
    let as_null = flags & PREG_UNMATCHED_AS_NULL != 0;
    let name = |group: usize| pattern.names.get(group).and_then(|n| n.as_deref());
    for group in 0..count {
        let at = scratch.group(group);
        let value = if offsets {
            offset_pair(req, subject, at, as_null)?
        } else {
            match_value(req, subject, at, as_null)?
        };
        let unset = at.0 == ffi::PCRE2_UNSET;
        insert(req, into, name(group), value, unset)?;
    }
    if as_null {
        for group in count..pattern.groups {
            let unset = (ffi::PCRE2_UNSET, ffi::PCRE2_UNSET);
            let value = if offsets {
                offset_pair(req, subject, unset, true)?
            } else {
                Owner::null()
            };
            insert(req, into, name(group), value, true)?;
        }
    }
    if let Some(mark) = scratch.data.mark() {
        let mark = PhpString::new(req, &mark)?.into_value();
        into.set_str(req, b"MARK", mark)?;
    }
    Ok(())
}

/// `preg_match(string $pattern, string $subject, &$matches = null, int
/// $flags = 0, int $offset = 0): int|false`: one match, its groups in
/// `$matches` under their numbers and names.
///
/// ```php
/// var_dump(preg_match('/(\d+)-(?<word>\w+)/', 'ab 12-cd', $m), $m, preg_match('/x/', 'abc', $m), $m);
/// ```
/// ```expect
/// int(1)
/// array(4) {
///   [0]=>
///   string(5) "12-cd"
///   [1]=>
///   string(2) "12"
///   ["word"]=>
///   string(2) "cd"
///   [2]=>
///   string(2) "cd"
/// }
/// int(0)
/// array(0) {
/// }
/// ```
///
/// `PREG_OFFSET_CAPTURE` pairs each group with its byte offset, and
/// `PREG_UNMATCHED_AS_NULL` reports a group that did not take part, the
/// trailing ones included, as `null`.
///
/// ```php
/// preg_match('/(a)(b)?(c)?/', 'xa', $m, PREG_OFFSET_CAPTURE); var_dump($m);
/// preg_match('/(a)(b)?(c)?/', 'xa', $m, PREG_UNMATCHED_AS_NULL); var_dump($m);
/// preg_match('/(a)(?<n>b)?(c)?/', 'xa', $m, PREG_UNMATCHED_AS_NULL | PREG_OFFSET_CAPTURE); var_dump($m);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   array(2) {
///     [0]=>
///     string(1) "a"
///     [1]=>
///     int(1)
///   }
///   [1]=>
///   array(2) {
///     [0]=>
///     string(1) "a"
///     [1]=>
///     int(1)
///   }
/// }
/// array(4) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "a"
///   [2]=>
///   NULL
///   [3]=>
///   NULL
/// }
/// array(5) {
///   [0]=>
///   array(2) {
///     [0]=>
///     string(1) "a"
///     [1]=>
///     int(1)
///   }
///   [1]=>
///   array(2) {
///     [0]=>
///     string(1) "a"
///     [1]=>
///     int(1)
///   }
///   ["n"]=>
///   array(2) {
///     [0]=>
///     NULL
///     [1]=>
///     int(-1)
///   }
///   [2]=>
///   array(2) {
///     [0]=>
///     NULL
///     [1]=>
///     int(-1)
///   }
///   [3]=>
///   array(2) {
///     [0]=>
///     NULL
///     [1]=>
///     int(-1)
///   }
/// }
/// ```
///
/// The offset, negative from the end, and past the end an internal
/// error; an order `preg_match` has none of is the `ValueError`.
///
/// ```php
/// var_dump(preg_match('/\w/', 'abc', $m, 0, 1), $m, preg_match('/\w/', 'abc', $m, 0, -1), $m, preg_match('/\w/', 'abc', $m, 0, 4), preg_last_error(), $m);
/// try { preg_match('/a/', 'a', $m, PREG_SET_ORDER); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// array(1) {
///   [0]=>
///   string(1) "b"
/// }
/// int(1)
/// array(1) {
///   [0]=>
///   string(1) "c"
/// }
/// bool(false)
/// int(1)
/// array(0) {
/// }
/// preg_match(): Argument #4 ($flags) must be a PREG_* constant
/// ```
///
/// `(*MARK)` names, and a duplicate name under `J`, where the group that
/// took part wins.
///
/// ```php
/// preg_match('/(*MARK:A)x|(*MARK:B)y/', 'y', $m); var_dump($m);
/// preg_match('/(?<n>a)|(?<n>b)/J', 'b', $m); var_dump($m);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(1) "y"
///   ["MARK"]=>
///   string(1) "B"
/// }
/// array(4) {
///   [0]=>
///   string(1) "b"
///   ["n"]=>
///   string(1) "b"
///   [1]=>
///   string(0) ""
///   [2]=>
///   string(1) "b"
/// }
/// ```
///
/// php's pattern syntax: bracket delimiters that nest, the modifiers,
/// and the warnings for what it refuses.
///
/// ```php
/// var_dump(preg_match('{a{2}}', 'aa'), preg_match('(a(b))', 'ab'), preg_match('[a]i', 'A'), preg_match('<a>', 'a'), preg_match('#^b$#m', "a\nb"), preg_match('/a.b/s', "a\nb"), preg_match('/a b/x', 'ab'), preg_match('/a+?/U', 'aa', $m), $m, preg_match('/b/A', 'ab'), preg_match('/a$/D', "a\n"), preg_match('/(a)(?:b)(c)/n', 'abc', $m), $m, preg_match('/\w/u', "\xc3\xa9"), preg_match("/a/\n", 'a'));
/// var_dump(preg_match('', 'a'), preg_match('abc', 'a'), preg_match('/abc', 'a'), preg_match('(abc', 'a'), preg_match('/a/Z', 'a'), preg_match("/a/\0", 'a'), preg_match('/a/e', 'a'), preg_match('/(/', 'a'), preg_last_error(), preg_last_error_msg());
/// ```
/// ```expect
/// int(1)
/// int(1)
/// int(1)
/// int(1)
/// int(1)
/// int(1)
/// int(1)
/// int(1)
/// array(1) {
///   [0]=>
///   string(2) "aa"
/// }
/// int(0)
/// int(0)
/// int(1)
/// array(1) {
///   [0]=>
///   string(3) "abc"
/// }
/// int(1)
/// int(1)
///
/// Warning: preg_match(): Empty regular expression in %s on line 3
///
/// Warning: preg_match(): Delimiter must not be alphanumeric, backslash, or NUL byte in %s on line 3
///
/// Warning: preg_match(): No ending delimiter '/' found in %s on line 3
///
/// Warning: preg_match(): No ending matching delimiter ')' found in %s on line 3
///
/// Warning: preg_match(): Unknown modifier 'Z' in %s on line 3
///
/// Warning: preg_match(): NUL byte is not a valid modifier in %s on line 3
///
/// Warning: preg_match(): Unknown modifier 'e' in %s on line 3
///
/// Warning: preg_match(): Compilation failed: missing closing parenthesis at offset 1 in %s on line 3
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// int(1)
/// string(14) "Internal error"
/// ```
///
/// `$matches` is written through the reference as `ZEND_TRY_ASSIGN_REF`
/// writes, held to the type of a typed property the reference is:
/// the empty array php lays first is refused before anything matches.
///
/// ```php
/// class T { public int $m = 0; public ?array $a = null; }
/// $t = new T;
/// try { preg_match('/a/', 'a', $t->m); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($t->m, preg_match('/(a)/', 'a', $t->a), $t->a);
/// ```
/// ```expect
/// Cannot assign array to reference held by property T::$m of type int
/// int(0)
/// int(1)
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "a"
/// }
/// ```
///
/// A `/u` match from offset zero that ends without an error records on
/// the string that its bytes are valid UTF-8, php's `IS_STR_VALID_UTF8`,
/// so the calls after it skip the library's check, which is what keeps
/// a `\G` walk through a long subject linear; a string written into
/// forgets it and is checked again.
///
/// ```php
/// $s = str_repeat("\u{e9}", 3); $n = 0; $pos = 0;
/// while (preg_match('/\G./u', $s, $m, 0, $pos)) { $pos += strlen($m[0]); $n++; }
/// $s .= "\xff";
/// var_dump($n, preg_match('/./u', $s), preg_last_error_msg(), preg_match('/./u', substr($s, 0, 1)), preg_last_error_msg());
/// ```
/// ```expect
/// int(3)
/// bool(false)
/// string(56) "Malformed UTF-8 characters, possibly incorrectly encoded"
/// bool(false)
/// string(56) "Malformed UTF-8 characters, possibly incorrectly encoded"
/// ```
#[php_function(frameless(2))]
fn preg_match(
    req: &mut Request,
    pattern: PhpString,
    subject: PhpString,
    #[php(default = null)] matches: Ref<Untyped<Value>>,
    #[php(default = 0)] flags: i64,
    #[php(default = 0)] offset: i64,
) -> Result<OrFalse<i64>, Thrown> {
    do_match(req, pattern, subject, &matches, flags, offset, false)
}

/// `preg_match_all(string $pattern, string $subject, &$matches = null,
/// int $flags = 0, int $offset = 0): int|false`: every match, grouped by
/// group under `PREG_PATTERN_ORDER` and by match under `PREG_SET_ORDER`,
/// an empty match stepped past as Perl's `/g` does.
///
/// ```php
/// var_dump(preg_match_all('/(\d)(?<l>[a-z])?/', '1a2 3c', $m), $m);
/// var_dump(preg_match_all('/(\d)(?<l>[a-z])?/', '1a2', $m, PREG_SET_ORDER), $m);
/// ```
/// ```expect
/// int(3)
/// array(4) {
///   [0]=>
///   array(3) {
///     [0]=>
///     string(2) "1a"
///     [1]=>
///     string(1) "2"
///     [2]=>
///     string(2) "3c"
///   }
///   [1]=>
///   array(3) {
///     [0]=>
///     string(1) "1"
///     [1]=>
///     string(1) "2"
///     [2]=>
///     string(1) "3"
///   }
///   ["l"]=>
///   array(3) {
///     [0]=>
///     string(1) "a"
///     [1]=>
///     string(0) ""
///     [2]=>
///     string(1) "c"
///   }
///   [2]=>
///   array(3) {
///     [0]=>
///     string(1) "a"
///     [1]=>
///     string(0) ""
///     [2]=>
///     string(1) "c"
///   }
/// }
/// int(2)
/// array(2) {
///   [0]=>
///   array(4) {
///     [0]=>
///     string(2) "1a"
///     [1]=>
///     string(1) "1"
///     ["l"]=>
///     string(1) "a"
///     [2]=>
///     string(1) "a"
///   }
///   [1]=>
///   array(2) {
///     [0]=>
///     string(1) "2"
///     [1]=>
///     string(1) "2"
///   }
/// }
/// ```
///
/// ```php
/// var_dump(preg_match_all('/a*/', 'baac', $m), $m, preg_match_all('//u', "\xc3\xa9x", $m, PREG_OFFSET_CAPTURE), $m, preg_match_all('/x/', 'abc'));
/// preg_match_all('/(*MARK:A)a|(*MARK:B)b/', 'ab', $m); var_dump($m);
/// preg_match_all('/(a)|(b)/', 'ab', $m, PREG_SET_ORDER | PREG_UNMATCHED_AS_NULL); var_dump($m);
/// try { preg_match_all('/a/', 'a', $m, 3); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(4)
/// array(1) {
///   [0]=>
///   array(4) {
///     [0]=>
///     string(0) ""
///     [1]=>
///     string(2) "aa"
///     [2]=>
///     string(0) ""
///     [3]=>
///     string(0) ""
///   }
/// }
/// int(3)
/// array(1) {
///   [0]=>
///   array(3) {
///     [0]=>
///     array(2) {
///       [0]=>
///       string(0) ""
///       [1]=>
///       int(0)
///     }
///     [1]=>
///     array(2) {
///       [0]=>
///       string(0) ""
///       [1]=>
///       int(2)
///     }
///     [2]=>
///     array(2) {
///       [0]=>
///       string(0) ""
///       [1]=>
///       int(3)
///     }
///   }
/// }
/// int(0)
/// array(2) {
///   [0]=>
///   array(2) {
///     [0]=>
///     string(1) "a"
///     [1]=>
///     string(1) "b"
///   }
///   ["MARK"]=>
///   array(2) {
///     [0]=>
///     string(1) "A"
///     [1]=>
///     string(1) "B"
///   }
/// }
/// array(2) {
///   [0]=>
///   array(3) {
///     [0]=>
///     string(1) "a"
///     [1]=>
///     string(1) "a"
///     [2]=>
///     NULL
///   }
///   [1]=>
///   array(3) {
///     [0]=>
///     string(1) "b"
///     [1]=>
///     NULL
///     [2]=>
///     string(1) "b"
///   }
/// }
/// preg_match_all(): Argument #4 ($flags) must be a PREG_* constant
/// ```
#[php_function]
fn preg_match_all(
    req: &mut Request,
    pattern: PhpString,
    subject: PhpString,
    #[php(default = null)] matches: Ref<Untyped<Value>>,
    #[php(default = 0)] flags: i64,
    #[php(default = 0)] offset: i64,
) -> Result<OrFalse<i64>, Thrown> {
    do_match(req, pattern, subject, &matches, flags, offset, true)
}

/// `php_do_pcre_match` past its ZPP: `$matches` is wanted where the call
/// passed it.
fn do_match(
    req: &mut Request,
    regex: PhpString,
    subject: PhpString,
    matches: &Ref<Untyped<Value>>,
    flags: i64,
    offset: i64,
    global: bool,
) -> Result<OrFalse<i64>, Thrown> {
    let wants = matches.is_given();
    let Some(pattern) = compiled(req, regex.as_bytes()) else {
        return Ok(OrFalse::False);
    };
    if offset == i64::MIN {
        return Err(arg_value_error(
            req,
            5,
            &format!("be greater than {}", i64::MIN),
        ));
    }
    // `zend_try_array_init`: `$matches` is `[]` before anything else.
    if wants {
        let empty = PhpArray::new(req)?;
        matches.set(req, Untyped(empty.into_value()))?;
    }
    let (value, found) = match_impl(req, &pattern, subject, wants, global, flags, offset)?;
    if let Some(found) = found {
        matches.set(req, Untyped(found.into_value()))?;
    }
    Ok(value)
}

/// `php_pcre_match_impl`: the count of matches, or `false` with the
/// error code set, and where `wants` asks for them the groups, as
/// `$matches` holds them.
pub(super) fn match_impl(
    req: &mut Request,
    pattern: &Pattern,
    subject_string: PhpString,
    wants: bool,
    global: bool,
    flags: i64,
    offset: i64,
) -> Result<(OrFalse<i64>, Option<Owner<PhpArray>>), Thrown> {
    let mut order = if global { PREG_PATTERN_ORDER } else { 0 };
    if flags & 0xff != 0 {
        order = flags & 0xff;
        if (global && !(PREG_PATTERN_ORDER..=PREG_SET_ORDER).contains(&order))
            || (!global && order != 0)
        {
            return Err(arg_value_error(req, 4, "be a PREG_* constant"));
        }
    }
    let bytes = subject_string.as_bytes();
    let len = bytes.len();
    let magnitude = usize::try_from(offset.unsigned_abs()).unwrap_or(usize::MAX);
    let mut start = if offset < 0 {
        len.saturating_sub(magnitude)
    } else {
        magnitude
    };
    if start > len {
        exec_error(req, ffi::PCRE2_ERROR_BADOFFSET);
        return Ok((OrFalse::False, None));
    }
    set_error(req, PREG_NO_ERROR);
    let Some(mut scratch) = Scratch::take(req, pattern) else {
        return Ok((OrFalse::False, None));
    };
    let from_start = start == 0;
    let outcome = matches_loop(
        req,
        pattern,
        &mut scratch,
        subject_string,
        &mut start,
        wants,
        global,
        order,
        flags,
    );
    scratch.give_back(req);
    let (count, out) = outcome?;
    let value = match count {
        Some(count) if error_code(req) == PREG_NO_ERROR => {
            // The library checked every byte from offset zero under `/u`,
            // or the string already said they were valid.
            if pattern.utf && from_start {
                subject_string.mark_valid_utf8();
            }
            OrFalse::Value(count)
        }
        _ => OrFalse::False,
    };
    Ok((value, out))
}

/// The loop of `php_pcre_match_impl`: how many matched, `None` for the
/// "Get subpatterns list failed" return, and the array for `$matches`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn matches_loop(
    req: &mut Request,
    pattern: &Pattern,
    scratch: &mut Scratch,
    string: PhpString,
    start: &mut usize,
    wants: bool,
    global: bool,
    order: i64,
    flags: i64,
) -> Result<(Option<i64>, Option<Owner<PhpArray>>), Thrown> {
    let bytes = string.as_bytes();
    let offsets = flags & PREG_OFFSET_CAPTURE != 0;
    let as_null = flags & PREG_UNMATCHED_AS_NULL != 0;
    let mut out = if wants {
        Some(PhpArray::new(req)?)
    } else {
        None
    };
    let mut sets = Vec::new();
    if global && wants && order == PREG_PATTERN_ORDER {
        for _ in 0..pattern.groups {
            sets.push(PhpArray::new(req)?);
        }
    }
    let mut marks: Option<Owner<PhpArray>> = None;
    let mut subject = Subject::of(&string);
    let mut matched: i64 = 0;
    let mut count = scratch.run(pattern, &mut subject, *start, 0, true);
    'search: loop {
        if count >= 0 {
            if count == 0 {
                docref(req, Level::Notice, "Matched, but too many substrings");
                count = i32::try_from(pattern.groups).unwrap_or(i32::MAX);
            }
            loop {
                matched += 1;
                let n = usize::try_from(count).unwrap_or(0);
                let (whole_start, whole_end) = scratch.group(0);
                if out.is_some() && whole_end < whole_start {
                    docref(req, Level::Warning, "Get subpatterns list failed");
                    for set in sets {
                        set.give_up(req);
                    }
                    if let Some(marks) = marks {
                        marks.give_up(req);
                    }
                    return Ok((None, out));
                }
                if let Some(out) = out.as_mut() {
                    if !global {
                        populate(req, out, pattern, scratch, bytes, n, flags)?;
                        break 'search;
                    }
                    if order == PREG_PATTERN_ORDER {
                        for (group, set) in sets.iter_mut().enumerate().take(n) {
                            let at = scratch.group(group);
                            let value = if offsets {
                                offset_pair(req, bytes, at, as_null)?
                            } else {
                                match_value(req, bytes, at, as_null)?
                            };
                            set.append(req, value)?;
                        }
                        if let Some(mark) = scratch.data.mark() {
                            let marks = match marks.as_mut() {
                                Some(marks) => marks,
                                None => marks.insert(PhpArray::new(req)?),
                            };
                            let mark = PhpString::new(req, &mark)?.into_value();
                            marks.set(req, Owner::index(matched - 1), mark)?;
                        }
                        let unset = (ffi::PCRE2_UNSET, ffi::PCRE2_UNSET);
                        for set in sets.iter_mut().skip(n) {
                            let value = if offsets {
                                offset_pair(req, bytes, unset, as_null)?
                            } else {
                                match_value(req, bytes, unset, as_null)?
                            };
                            set.append(req, value)?;
                        }
                    } else {
                        let mut set = PhpArray::new(req)?;
                        populate(req, &mut set, pattern, scratch, bytes, n, flags)?;
                        out.append(req, set.into_value())?;
                    }
                }
                let (whole_start, whole_end) = scratch.group(0);
                *start = whole_end;
                if *start == whole_start {
                    count = scratch.retry(pattern, &mut subject, *start);
                    if count >= 0 {
                        if global {
                            continue;
                        }
                        break 'search;
                    } else if count == ffi::PCRE2_ERROR_NOMATCH {
                        if *start < bytes.len() {
                            *start += unit_length(pattern, bytes, *start);
                        } else {
                            break 'search;
                        }
                    } else {
                        exec_error(req, count);
                        break 'search;
                    }
                }
                break;
            }
        } else if count == ffi::PCRE2_ERROR_NOMATCH {
            break;
        } else {
            exec_error(req, count);
            break;
        }
        if !global {
            break;
        }
        count = scratch.run(pattern, &mut subject, *start, 0, true);
    }
    if let Some(out) = out.as_mut()
        && !sets.is_empty()
    {
        for (group, set) in sets.into_iter().enumerate() {
            let value = set.into_value();
            if let Some(name) = pattern.names.get(group).and_then(|n| n.as_deref()) {
                out.set_str(req, name, Owner::take(req, *value))?;
            }
            out.append(req, value)?;
        }
        if let Some(marks) = marks {
            out.set_str(req, b"MARK", marks.into_value())?;
        }
    }
    Ok((Some(matched), out))
}
