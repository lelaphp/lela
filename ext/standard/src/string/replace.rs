//! The replacements of `string.c`: `str_replace` and `str_ireplace`
//! over `php_str_replace_in_subject`, `strtr` over `php_strtr_ex` and
//! `php_strtr_array`, and `substr_replace`.

use std::ops::ControlFlow;

use php_ext::abi::{Raw, Ref, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::{coerce_param, to_long, to_string, with_string};
use php_ext::decl::Type;
use php_ext::raising::{Level, Thrown, arg_type_error, argument_error, arity, docref};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value};

use super::{KeyBytes, long, lower, memnstr};
use crate::ffi::memchr;

/// `str_replace(array|string $search, array|string $replace,
/// string|array $subject, &$count = null): string|array`: each search
/// string, in order, replaced in the subject by its replacement (the
/// replacement string, the replacement array's next element, or the
/// empty string past its end), in each subject of an array subject
/// under its key, and the number of replacements written through
/// `$count`. An empty search is skipped.
///
/// ```php
/// var_dump(str_replace("l", "L", "hello", $c), $c, str_replace(["a", "b"], ["b", "c"], "ab"), str_replace(["a", "b", "c"], ["x"], "abc"), str_replace(["a"], "z", ["k" => "aa", 5 => "ba"], $n), $n, str_replace("", "x", "abc"), str_replace("abc", "", "abc"), str_replace("ll", "LLL", "hello llama"), str_replace(1, 2, 3141));
/// var_dump(str_ireplace("L", "x", "HelLo", $i), $i, str_ireplace(["HE", "o"], ["a", "0"], "hello"), str_ireplace("ab", "--", "xAbyaB"));
/// try { str_replace("a", ["b"], "a"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(5) "heLLo"
/// int(2)
/// string(2) "cc"
/// string(1) "x"
/// array(2) {
///   ["k"]=>
///   string(2) "zz"
///   [5]=>
///   string(2) "bz"
/// }
/// int(3)
/// string(3) "abc"
/// string(0) ""
/// string(13) "heLLLo LLLama"
/// string(4) "3242"
/// string(5) "Hexxo"
/// int(2)
/// string(4) "all0"
/// string(6) "x--y--"
/// str_replace(): Argument #2 ($replace) must be of type string when argument #1 ($search) is a string
/// ```
///
/// A subject no search matches is handed back as it stands, so nothing
/// is allocated; a replacement as long as its search, a shorter one and
/// a longer one each write the new string once, and a subject no longer
/// than the search is the replacement where the two are equal. A search
/// that leaves the subject empty ends the walk of the searches.
///
/// ```php
/// $s = str_repeat("ab", 20); $m = memory_get_usage(); $r = str_replace("x", "y", $s); $t = str_replace("xy", "z", $s); $u = str_ireplace(["Q", "QR"], "z", $s); var_dump(memory_get_usage() - $m);
/// var_dump(str_replace("ab", "xy", "abcab"), str_replace("ab", "x", "abcab"), str_replace("ab", "xyz", "abcab", $n), $n, str_replace("abc", "Z", "abc"), str_replace("abcd", "Z", "abc"), str_replace("aa", "b", "aaa"));
/// var_dump(str_replace(["a", "b"], "", "abab", $e), $e, str_ireplace("A", "xx", "aAb"), str_ireplace("AB", "c", "xaBAbab"), str_ireplace("aBc", "q", "ABC"), str_ireplace(["B", "bc"], ["c", "D"], "aBc"));
/// ```
/// ```expect
/// int(0)
/// string(5) "xycxy"
/// string(3) "xcx"
/// string(7) "xyzcxyz"
/// int(2)
/// string(1) "Z"
/// string(3) "abc"
/// string(2) "ba"
/// string(0) ""
/// int(4)
/// string(5) "xxxxb"
/// string(4) "xccc"
/// string(1) "q"
/// string(3) "acc"
/// ```
///
/// Declared frameless for three arguments
/// (`frameless_function_infos_str_replace`), so a call without `$count`
/// is a `FRAMELESS_ICALL_3` over this body, with the `OP_DATA` after it
/// carrying the subject, and one with it the framed call.
///
/// ```php
/// var_dump(str_replace("a", "b", "aaa"), str_replace(["a"], ["b"], ["x" => "a"]), str_replace(search: "a", replace: "b", subject: "a"), call_user_func('str_replace', "a", "b", "ab"), str_replace(1, 2, 121));
/// try { str_replace("a", [], "a"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "bbb"
/// array(1) {
///   ["x"]=>
///   string(1) "b"
/// }
/// string(1) "b"
/// string(2) "bb"
/// string(3) "222"
/// str_replace(): Argument #2 ($replace) must be of type string when argument #1 ($search) is a string
/// ```
#[php_function(frameless(3))]
pub(super) fn str_replace(
    req: &mut Request,
    search: Union<PhpArray, PhpString>,
    replace: Union<PhpArray, PhpString>,
    subject: Union<PhpArray, PhpString>,
    #[php(default = null)] count: Ref<Untyped<Value>>,
) -> Result<Union<Owner<PhpArray>, Owner<PhpString>>, Thrown> {
    replace_common(req, (search, replace, subject, count), true)
}

/// `str_ireplace(...)`, the search folded by `zend_tolower_ascii`.
///
/// ```php
/// var_dump(str_ireplace("L", "x", "HelLo", $i), $i, str_ireplace(["HE", "o"], ["a", "0"], "hello"), str_ireplace("ab", "--", "xAbyaB"), str_ireplace(["A"], "b", ["Aa", "k" => "xa"]));
/// ```
/// ```expect
/// string(5) "Hexxo"
/// int(2)
/// string(4) "all0"
/// string(6) "x--y--"
/// array(2) {
///   [0]=>
///   string(2) "bb"
///   ["k"]=>
///   string(2) "xb"
/// }
/// ```
///
/// A search of more than one byte folds the subject once, on the
/// request's heap as `zend_string_tolower` does, and gives the copy up
/// before the call returns, so the peak moves by its size where the
/// subject has an uppercase byte to fold.
///
/// ```php
/// $s = str_repeat("Ab", 500000); $p = memory_get_peak_usage(); $r = str_ireplace("XY", "z", $s); $q = memory_get_peak_usage(); $t = str_ireplace(["x", "QQ", "yy"], "z", strtolower($s)); var_dump($q - $p, memory_get_peak_usage() - $q, $r === $s);
/// ```
/// ```expect
/// int(1003552)
/// int(0)
/// bool(true)
/// ```
#[php_function]
pub(super) fn str_ireplace(
    req: &mut Request,
    search: Union<PhpArray, PhpString>,
    replace: Union<PhpArray, PhpString>,
    subject: Union<PhpArray, PhpString>,
    #[php(default = null)] count: Ref<Untyped<Value>>,
) -> Result<Union<Owner<PhpArray>, Owner<PhpString>>, Thrown> {
    replace_common(req, (search, replace, subject, count), false)
}

/// `php_str_replace_common`.
/// What `str_replace` and `str_ireplace` were called with: the search,
/// the replacement, the subject and the `&$count` written back.
type Replacing = (
    Union<PhpArray, PhpString>,
    Union<PhpArray, PhpString>,
    Union<PhpArray, PhpString>,
    Ref<Untyped<Value>>,
);

fn replace_common(
    req: &mut Request,
    (search, replace, subject, counted): Replacing,
    case_sensitive: bool,
) -> Result<Union<Owner<PhpArray>, Owner<PhpString>>, Thrown> {
    if matches!(search, Union::B(_)) && matches!(replace, Union::A(_)) {
        return Err(arg_type_error(
            req,
            2,
            "be of type string when argument #1 ($search) is a string",
        ));
    }
    let mut count = 0i64;
    let result = match subject {
        Union::A(subjects) => {
            let mut out = PhpArray::new(req)?;
            // `ZEND_HASH_FOREACH_KEY_VAL(subject_ht, num_key, string_key,
            // subject_entry)` (`Z/ext/standard/string.c:4501`), no owner of the
            // table taken: the argument holds it while `__toString` runs.
            let walked = walk::foreach(req, subjects, Hold::None, |req, row| -> walk::Step {
                let subject = to_string(req, &row.value().deref())?;
                let replaced = in_subject(req, &search, &replace, *subject, case_sensitive);
                subject.give_up(req);
                let (value, n) = replaced?;
                count += n;
                let key = Owner::take(req, row.key().clone());
                out.set(req, key, value.into_value())?;
                Ok(ControlFlow::Continue(()))
            });
            if let Err(thrown) = walked {
                out.give_up(req);
                return Err(thrown);
            }
            Union::A(out)
        }
        Union::B(subject) => {
            let (value, n) = in_subject(req, &search, &replace, subject, case_sensitive)?;
            count = n;
            Union::B(value)
        }
    };
    if let Err(thrown) = counted.set(req, Untyped(Value::int(count))) {
        match result {
            Union::A(array) => array.give_up(req),
            Union::B(string) => string.give_up(req),
        }
        return Err(thrown);
    }
    Ok(result)
}

/// `php_str_replace_in_subject` (`Z/ext/standard/string.c:4313`): the
/// subject after every search, and the count of replacements. The
/// subject stands as it is until a search finds something, so a subject
/// nothing matches is handed back with one more owner and no byte
/// copied; each search that does match writes the next subject once, at
/// its length.
fn in_subject(
    req: &mut Request,
    search: &Union<PhpArray, PhpString>,
    replace: &Union<PhpArray, PhpString>,
    subject: PhpString,
    case_sensitive: bool,
) -> Result<(Owner<PhpString>, i64), Thrown> {
    if subject.is_empty() {
        return Ok((PhpString::new(req, b"")?, 0));
    }
    let mut count = 0i64;
    let mut subject = Subject {
        given: subject,
        current: None,
        folded: None,
    };
    let searches = match search {
        Union::A(searches) => *searches,
        Union::B(needle) => {
            // A string search with an array replacement was refused before
            // any subject was read.
            let with: &[u8] = match replace {
                Union::B(s) => s.as_bytes(),
                Union::A(_) => b"",
            };
            let searched = subject.search(req, *needle, with, case_sensitive, &mut count);
            let value = subject.into_current(req);
            if let Err(thrown) = searched {
                value.give_up(req);
                return Err(thrown);
            }
            return Ok((value, count));
        }
    };
    let mut replace_at = 0;
    // `ZEND_HASH_FOREACH_VAL(search_ht, search_entry)`
    // (`Z/ext/standard/string.c:4344`), no owner of the searches taken:
    // the argument holds them while `zval_get_tmp_string` runs
    // `__toString`. The replacement is read at the next bucket of its
    // table that holds one, as it stands at the step, before the search
    // is looked at.
    let walked = walk::foreach(req, searches, Hold::None, |req, row| -> walk::Step {
        let needle = to_string(req, &row.value().deref())?;
        let with = match replace {
            Union::A(all) => match all.live_from(replace_at) {
                Some((at, _, value)) => {
                    replace_at = at.saturating_add(1);
                    match to_string(req, &value) {
                        Ok(with) => Some(with),
                        Err(thrown) => {
                            needle.give_up(req);
                            return Err(thrown);
                        }
                    }
                }
                None => None,
            },
            Union::B(_) => None,
        };
        let bytes: &[u8] = match (&with, replace) {
            (Some(with), _) => with.as_bytes(),
            (None, Union::B(s)) => s.as_bytes(),
            (None, Union::A(_)) => b"",
        };
        let replaced = subject.search(req, *needle, bytes, case_sensitive, &mut count);
        needle.give_up(req);
        if let Some(with) = with {
            with.give_up(req);
        }
        // A search that left nothing ends the walk.
        Ok(if replaced? && subject.now().is_empty() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    });
    let value = subject.into_current(req);
    match walked {
        Err(thrown) => {
            value.give_up(req);
            Err(thrown)
        }
        // php's empty string, where a search left nothing.
        Ok(Some(())) => {
            value.give_up(req);
            Ok((PhpString::new(req, b"")?, count))
        }
        Ok(None) => Ok((value, count)),
    }
}

/// The subject of `php_str_replace_in_subject` as the searches so far
/// left it: the argument until a search replaces anything and the new
/// string after, and its folded copy (`lc_subject_str`), made by the
/// first folded search of more than one byte and given up when any
/// search replaces, since it then folds a subject no longer current.
struct Subject {
    given: PhpString,
    current: Option<Owner<PhpString>>,
    folded: Option<Owner<PhpString>>,
}

impl Subject {
    fn now(&self) -> PhpString {
        self.current.as_deref().copied().unwrap_or(self.given)
    }

    /// One search of the subject as it stands, and whether it replaced
    /// anything, what it replaced added to `count`: an empty search is
    /// skipped, a one-byte search is `php_char_to_str_ex`, a longer one
    /// `php_str_to_str_ex`, or `php_str_to_str_i_ex` over the folded
    /// subject and the needle folded for the search, both on the
    /// request's heap as `zend_string_tolower` makes them.
    fn search(
        &mut self,
        req: &mut Request,
        needle: PhpString,
        with: &[u8],
        case_sensitive: bool,
        count: &mut i64,
    ) -> Result<bool, Thrown> {
        let now = self.now();
        let step = match needle.as_bytes() {
            [] => None,
            &[byte] => char_to_str(req, now.as_bytes(), (byte, with), case_sensitive, count)?,
            bytes if case_sensitive => {
                str_to_str(req, (now.as_bytes(), now.as_bytes()), (bytes, with), count)?
            }
            bytes => {
                let folded = if let Some(folded) = &self.folded {
                    **folded
                } else {
                    let folded = lowered(req, now)?;
                    let view = *folded;
                    self.folded = Some(folded);
                    view
                };
                // `php_str_to_str_i_ex` folds the needle only where it
                // can match.
                if bytes.len() > now.len() {
                    None
                } else {
                    let needle = lowered(req, needle)?;
                    let step = str_to_str(
                        req,
                        (now.as_bytes(), folded.as_bytes()),
                        (needle.as_bytes(), with),
                        count,
                    );
                    needle.give_up(req);
                    step?
                }
            }
        };
        let Some(replaced) = step else {
            return Ok(false);
        };
        if let Some(folded) = self.folded.take() {
            folded.give_up(req);
        }
        if let Some(old) = self.current.replace(replaced) {
            old.give_up(req);
        }
        Ok(true)
    }

    /// The subject as it stands, one owner of it, its folded copy given
    /// up.
    fn into_current(self, req: &mut Request) -> Owner<PhpString> {
        if let Some(folded) = self.folded {
            folded.give_up(req);
        }
        match self.current {
            Some(current) => current,
            None => Owner::take(req, self.given),
        }
    }
}

/// `zend_string_tolower`: `s` itself, one more owner, where it has no
/// uppercase byte, and otherwise its bytes folded by
/// `zend_tolower_ascii` into a new string of the request's.
fn lowered(req: &mut Request, s: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let bytes = s.as_bytes();
    if !bytes.iter().any(u8::is_ascii_uppercase) {
        return Ok(Owner::take(req, s));
    }
    PhpString::filled(req, bytes.len(), |out| {
        for (to, from) in out.iter_mut().zip(bytes) {
            *to = lower(*from);
        }
    })
}

/// `php_char_to_str_ex` (`Z/ext/standard/string.c:3103`): every `from`
/// in `subject` replaced by `to`, folded by `zend_tolower_ascii` where
/// not `case_sensitive`. The bytes are counted first (`count_chars`), so
/// a subject without one is `None` and allocates nothing, and the new string
/// is allocated once at its length, `zend_string_safe_alloc` where it
/// grows.
#[allow(clippy::naive_bytecount)] // `count_chars` is one loop, not a crate
fn char_to_str(
    req: &mut Request,
    subject: &[u8],
    (from, to): (u8, &[u8]),
    case_sensitive: bool,
    count: &mut i64,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let folded = lower(from);
    let n = if case_sensitive {
        // `count_chars`' SSE2 count, which the compiler vectorises as it is.
        subject.iter().filter(|&&c| c == from).count()
    } else {
        subject.iter().filter(|&&c| lower(c) == folded).count()
    };
    if n == 0 {
        return Ok(None);
    }
    *count += long(n);
    let fill = |out: &mut [u8]| {
        let mut pen = Pen { out, at: 0 };
        if case_sensitive {
            let mut rest = subject;
            while let Some(at) = memchr(rest, from) {
                pen.put(&rest[..at]);
                pen.put(to);
                rest = &rest[at + 1..];
            }
            pen.put(rest);
        } else {
            for &c in subject {
                if lower(c) == folded {
                    pen.put(to);
                } else {
                    pen.put(&[c]);
                }
            }
        }
    };
    let replaced = match to.len().checked_sub(1) {
        Some(grows) => PhpString::product(req, n, grows, subject.len(), fill)?,
        None => PhpString::filled(req, subject.len() - n, fill)?,
    };
    Ok(Some(replaced))
}

/// `php_str_to_str_ex` (`Z/ext/standard/string.c:3169`) where `searched`
/// is `subject`, and `php_str_to_str_i_ex` where it is the subject
/// folded and `needle` is folded too: every non-overlapping `needle` in
/// `searched`, left to right, replaced by `with`, the bytes between
/// copied from `subject`. A replacement as long as the search copies
/// the subject at its first match and writes each match over it; any
/// other counts the matches first and writes the new string once at its
/// length, `zend_string_safe_alloc` where it grows. A subject no longer
/// than the search is the replacement where the two are equal. `None`
/// where nothing matched.
fn str_to_str(
    req: &mut Request,
    (subject, searched): (&[u8], &[u8]),
    (needle, with): (&[u8], &[u8]),
    count: &mut i64,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let len = subject.len();
    if needle.len() >= len {
        if searched != needle {
            return Ok(None);
        }
        *count += 1;
        // `zend_string_init_fast` (`Z/ext/standard/string.c:3307`).
        return PhpString::fast(req, with).map(Some);
    }
    if needle.len() == with.len() {
        let Some(first) = memnstr(searched, needle, 0) else {
            return Ok(None);
        };
        let mut n = 0;
        let replaced = PhpString::filled(req, len, |out| {
            out.copy_from_slice(subject);
            for at in found(searched, needle, first) {
                if let Some(place) = out.get_mut(at..at + with.len()) {
                    place.copy_from_slice(with);
                }
                n += 1;
            }
        })?;
        *count += n;
        return Ok(Some(replaced));
    }
    let Some(first) = memnstr(searched, needle, 0) else {
        return Ok(None);
    };
    let n = found(searched, needle, first).count();
    *count += long(n);
    let fill = |out: &mut [u8]| {
        let mut pen = Pen { out, at: 0 };
        let mut copied = 0;
        for at in found(searched, needle, first) {
            pen.put(&subject[copied..at]);
            pen.put(with);
            copied = at + needle.len();
        }
        pen.put(&subject[copied..]);
    };
    let replaced = if with.len() > needle.len() {
        PhpString::product(req, n, with.len() - needle.len(), len, fill)?
    } else {
        PhpString::filled(req, len - n * (needle.len() - with.len()), fill)?
    };
    Ok(Some(replaced))
}

/// Each offset of a non-empty `needle` in `haystack` from the match at
/// `first` on, the next search starting past the last match, as
/// `php_str_to_str_ex`'s loop over `php_memnstr` steps.
fn found<'a>(
    haystack: &'a [u8],
    needle: &'a [u8],
    first: usize,
) -> impl Iterator<Item = usize> + 'a {
    let mut next = Some(first);
    std::iter::from_fn(move || {
        let at = next?;
        next = memnstr(haystack, needle, at + needle.len().max(1));
        Some(at)
    })
}

/// A string being written front to back, `zend_mempcpy`'s target.
struct Pen<'a> {
    out: &'a mut [u8],
    at: usize,
}

impl Pen<'_> {
    fn put(&mut self, bytes: &[u8]) {
        let end = self.at + bytes.len();
        if let Some(place) = self.out.get_mut(self.at..end) {
            place.copy_from_slice(bytes);
        }
        self.at = end;
    }
}

/// `strtr(string $string, string|array $from, ?string $to = null):
/// string`: with three arguments, `php_strtr_ex`, each byte of `$from`
/// mapped to the byte of `$to` at its position over the shorter length;
/// with two, `php_strtr_array`, the longest key matching at each
/// position replaced by its value and the scan moving past it, so no
/// replacement is replaced again.
///
/// ```php
/// var_dump(strtr("Hi all, I said hello", ["Hi" => "Hello", "hello" => "hi", "said" => "say"]), strtr("abc", "ab", "xyz"), strtr("abc", "abc", "x"), strtr("abc", []), strtr("", ["a" => "b"]), strtr("aaa", ["a" => "b", "aa" => "c"]), strtr("a1b2", [1 => "one", 2 => "two"]), strtr("abc", ["b" => ""]), strtr("abc", "", "x"));
/// var_dump(strtr("abc", ["" => "x"]), strtr("abc", ["" => "x", "b" => "y"]));
/// try { strtr("abc", "a"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(19) "Hello all, I say hi"
/// string(3) "xyc"
/// string(3) "xbc"
/// string(3) "abc"
/// string(0) ""
/// string(2) "cb"
/// string(8) "aonebtwo"
/// string(2) "ac"
/// string(3) "abc"
///
/// Warning: strtr(): Ignoring replacement of empty string in %s on line 3
///
/// Warning: strtr(): Ignoring replacement of empty string in %s on line 3
/// string(3) "abc"
/// string(3) "ayc"
/// strtr(): Argument #2 ($from) must be of type array, string given
/// ```
///
/// The pairs are looked up as php's table holds them: an integer key as
/// its decimal, a longer key before a shorter one at one position, a key
/// longer than the string never, a value read as a string where it is
/// written (an object's `__toString` once per match, the single pair's
/// before its key is looked at), and the empty key warned of once, before
/// any value is read.
///
/// ```php
/// var_dump(strtr("a1b-2c10", [1 => "one", -2 => "m2", 10 => "ten", "b" => "B"]), strtr("01 1", ["01" => "x", 1 => "y"]), strtr("abcabc", ["abc" => "1", "ab" => "2", "bca" => "3"]), strtr("ab", ["abc" => "long", "a" => "A"]), strtr("aaa", ["a" => 1, "aa" => 2.5, "x" => null]), strtr("a", [1 => "x", 2 => "y"]), strtr("xyz", ["" => "e", "y" => "Y"]));
/// class S { function __toString(): string { echo "S\n"; return "s"; } }
/// var_dump(strtr("aXa", ["a" => new S, "Q" => "q"]), strtr("bb", ["" => new S]));
/// ```
/// ```expect
/// Warning: strtr(): Ignoring replacement of empty string in %s on line 2
/// string(11) "aoneBm2cten"
/// string(3) "x y"
/// string(2) "11"
/// string(2) "Ab"
/// string(4) "2.51"
/// string(1) "a"
/// string(3) "xYz"
/// S
/// S
/// S
///
/// Warning: strtr(): Ignoring replacement of empty string in %s on line 4
/// string(3) "sXs"
/// string(2) "bb"
/// ```
///
/// The parse is php's, chosen by the count, so a count it refuses is
/// refused before any argument is read.
///
/// ```php
/// foreach ([fn() => strtr("abc"), fn() => strtr([], "a"), fn() => strtr("abc", "a", "b", "c"), fn() => strtr("abc", 5), fn() => strtr("abc", ["a" => "x"], "q")] as $f) { try { $f(); } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; } }
/// ```
/// ```expect
/// ArgumentCountError: strtr() expects exactly 2 arguments, 1 given
/// TypeError: strtr(): Argument #1 ($string) must be of type string, array given
/// ArgumentCountError: strtr() expects exactly 3 arguments, 4 given
/// TypeError: strtr(): Argument #2 ($from) must be of type array, int given
/// TypeError: strtr(): Argument #2 ($from) must be of type string, array given
/// ```
///
/// Declared frameless for two arguments and for three
/// (`frameless_function_infos_strtr`), whose bodies parse as the count
/// chooses here, so a `FRAMELESS_ICALL_2` or `_3` and the framed call
/// agree, the deprecation for a `null` third argument included.
///
/// ```php
/// var_dump(strtr("abc", ["a" => "x"]), strtr("abc", "ab", "xy"), strtr(string: "abc", from: "a", to: "z"), strtr("", "a", "b"), call_user_func('strtr', "abc", "c", "C"), strtr("abc", "a", null));
/// ```
/// ```expect
/// Deprecated: strtr(): Passing null to parameter #3 ($to) of type ?string is deprecated in %s on line 2
/// string(3) "xbc"
/// string(3) "xyc"
/// string(3) "zbc"
/// string(0) ""
/// string(3) "abC"
/// string(3) "abc"
/// ```
#[php_function(arity = manual, frameless(2, 3))]
pub(super) fn strtr(
    req: &mut Request,
    string: Raw<PhpString>,
    from: Raw<Union<PhpArray, PhpString>>,
    #[php(default = null)] to: Raw<Option<PhpString>>,
) -> Result<Owner<PhpString>, Thrown> {
    // php's own parse, chosen by the count: `(string, array)` for two
    // arguments, `(string, string, string)` for three.
    let given = req.frames().argument_count();
    if given <= 2 {
        arity(req, 2, Some(2))?;
        let string = string.hold(req)?;
        // `Z_PARAM_ARRAY_HT`, where the stub declares `string|array $from`.
        let from = coerce_param(req, &from.value(), &Type::Array, 2)?;
        let Some(from) = from.as_array() else {
            return PhpString::new(req, b"");
        };
        if string.is_empty() {
            return PhpString::new(req, b"");
        }
        return strtr_array(req, string, from);
    }
    arity(req, 3, Some(3))?;
    let string = string.hold(req)?;
    // `Z_PARAM_STR`, where the stub declares `string|array $from`.
    let from = coerce_param(req, &from.value(), &Type::String, 2)?;
    // `Z_PARAM_STRING`: a `null` is the deprecation, which names the
    // declared `?string`, and the empty string.
    let to = coerce_param(req, &to.value(), &Type::String, 3)?;
    let (Some(from), Some(to)) = (from.as_php_string(), to.as_php_string()) else {
        return PhpString::new(req, b"");
    };
    if string.is_empty() {
        return PhpString::new(req, b"");
    }
    let n = from.len().min(to.len());
    if n == 0 {
        return Ok(Owner::take(req, string));
    }
    let mut xlat: [u8; 256] = std::array::from_fn(|i| u8::try_from(i).unwrap_or(0));
    for (f, t) in from.as_bytes()[..n].iter().zip(&to.as_bytes()[..n]) {
        xlat[usize::from(*f)] = *t;
    }
    let bytes = string.as_bytes();
    if bytes.iter().all(|c| xlat[usize::from(*c)] == *c) {
        return Ok(Owner::take(req, string));
    }
    let out: Vec<u8> = bytes.iter().map(|c| xlat[usize::from(*c)]).collect();
    PhpString::new(req, &out)
}

/// `php_strtr_array` (`Z/ext/standard/string.c:3398`): no pair is the string as it stands, one pair is
/// `php_char_to_str_ex` or `php_str_to_str_ex` over its key, its value
/// read before the key is looked at, and more are
/// `php_strtr_array_ex`'s scan.
fn strtr_array(
    req: &mut Request,
    string: PhpString,
    from: PhpArray,
) -> Result<Owner<PhpString>, Thrown> {
    if from.len() > 1 {
        return strtr_pairs(req, string, from);
    }
    let Some((key, value)) = walk::rows(req, from).next() else {
        return Ok(Owner::take(req, string));
    };
    let k: &[u8] = &KeyBytes::of(&key);
    let s = string.as_bytes();
    let with = to_string(req, &value)?;
    let mut ignored = 0;
    let replaced = match k {
        [] => {
            docref(req, Level::Warning, "Ignoring replacement of empty string");
            Ok(None)
        }
        &[byte] => char_to_str(req, s, (byte, with.as_bytes()), true, &mut ignored),
        _ => str_to_str(req, (s, s), (k, with.as_bytes()), &mut ignored),
    };
    with.give_up(req);
    match replaced? {
        Some(replaced) => Ok(replaced),
        None => Ok(Owner::take(req, string)),
    }
}

/// `php_strtr_array_ex` (`Z/ext/standard/string.c:2919`): one walk of
/// the pattern table gathers the first bytes and the lengths its keys
/// come in, and the scan looks a slice of the subject up in the table
/// itself, by hash, only where its first byte begins a key and only at
/// the lengths a key has, longest first, so the longest key matching at
/// a position is replaced and the scan moves past it. php rebuilds the
/// table with its integer keys as strings where it has any; the
/// symtable lookup finds what that rebuilt table would, since no string
/// key of an array is a canonical decimal integer.
fn strtr_pairs(
    req: &mut Request,
    string: PhpString,
    from: PhpArray,
) -> Result<Owner<PhpString>, Thrown> {
    let s = string.as_bytes();
    let mut firsts = Bits::default();
    let mut lengths = Bits::default();
    let (mut minlen, mut maxlen) = (128 * 1024usize, 0usize);
    let (mut numeric, mut empty) = (false, false);
    for (key, _) in walk::rows(req, from) {
        numeric |= matches!(key, Key::Int(_));
        let k: &[u8] = &KeyBytes::of(&key);
        let Some(&first) = k.first() else {
            empty = true;
            continue;
        };
        if k.len() > s.len() {
            continue;
        }
        maxlen = maxlen.max(k.len());
        minlen = minlen.min(k.len());
        lengths.insert(k.len());
        firsts.insert(usize::from(first));
    }
    // An array holds one empty key at most, so php's warning from inside
    // its walk is this one, before any value is read.
    if empty {
        docref(req, Level::Warning, "Ignoring replacement of empty string");
    }
    if minlen > maxlen {
        return Ok(Owner::take(req, string));
    }
    let mut out: Option<Vec<u8>> = None;
    let (mut pos, mut old_pos) = (0usize, 0usize);
    while pos + minlen <= s.len() {
        if firsts.contains(usize::from(s[pos])) {
            let mut len = maxlen.min(s.len() - pos);
            while len >= minlen {
                if lengths.contains(len) {
                    let key = &s[pos..pos + len];
                    let found = if numeric {
                        from.find_symtable(key)
                    } else {
                        from.find(key)
                    };
                    if let Some(value) = found {
                        let buffer = out.get_or_insert_with(|| Vec::with_capacity(s.len()));
                        buffer.extend_from_slice(&s[old_pos..pos]);
                        with_string(req, &value, |with| buffer.extend_from_slice(with))?;
                        old_pos = pos + len;
                        pos = old_pos - 1;
                        break;
                    }
                }
                len -= 1;
            }
        }
        pos += 1;
    }
    match out {
        Some(mut buffer) => {
            buffer.extend_from_slice(&s[old_pos..]);
            PhpString::new(req, &buffer)
        }
        None => Ok(Owner::take(req, string)),
    }
}

/// A set of small numbers, `php_strtr_array_ex`'s `bitset` of first
/// bytes and `num_bitset` of key lengths: the first 256 in place, so a byte or a
/// short key's length costs no allocation, and a longer length in words
/// grown as one is inserted.
#[derive(Default)]
struct Bits {
    short: [u64; 4],
    long: Vec<u64>,
}

impl Bits {
    #[inline]
    fn insert(&mut self, n: usize) {
        let (word, bit) = (n / 64, 1u64 << (n % 64));
        match self.short.get_mut(word) {
            Some(w) => *w |= bit,
            None => self.insert_long(word - self.short.len(), bit),
        }
    }

    #[cold]
    fn insert_long(&mut self, word: usize, bit: u64) {
        if self.long.len() <= word {
            self.long.resize(word + 1, 0);
        }
        self.long[word] |= bit;
    }

    #[inline]
    fn contains(&self, n: usize) -> bool {
        let (word, bit) = (n / 64, 1u64 << (n % 64));
        let w = match self.short.get(word) {
            Some(w) => *w,
            None => self.long.get(word - self.short.len()).copied().unwrap_or(0),
        };
        w & bit != 0
    }
}

/// `substr_replace(array|string $string, array|string $replace,
/// array|int $offset, array|int|null $length = null): string|array`:
/// the bytes from `$offset` over `$length` replaced, both clamped into
/// the string as php clamps them; over an array of strings, the offset,
/// length and replacement arrays each read in step, an exhausted one
/// standing at zero, the whole length and the empty string.
///
/// ```php
/// var_dump(substr_replace("Hello", "J", 0, 1), substr_replace("Hello", "!!", -1), substr_replace("Hello", "X", 2, 0), substr_replace("Hello", "X", 1, -2), substr_replace("Hello", "X", 10), substr_replace("Hello", ["a", "b"], 0, 1), substr_replace("Hello", [], 0, 1));
/// var_dump(substr_replace(["abc", "k" => "def"], "X", 1, 1), substr_replace(["abc", "def", "ghi"], ["1", "2"], [0, 1], [1]), substr_replace(["abc"], "X", -1, -1));
/// try { substr_replace("a", "b", [0]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { substr_replace("a", "b", 0, [0]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(5) "Jello"
/// string(6) "Hell!!"
/// string(6) "HeXllo"
/// string(4) "HXlo"
/// string(6) "HelloX"
/// string(5) "aello"
/// string(4) "ello"
/// array(2) {
///   [0]=>
///   string(3) "aXc"
///   ["k"]=>
///   string(3) "dXf"
/// }
/// array(3) {
///   [0]=>
///   string(3) "1bc"
///   [1]=>
///   string(2) "d2"
///   [2]=>
///   string(0) ""
/// }
/// array(1) {
///   [0]=>
///   string(4) "abXc"
/// }
/// substr_replace(): Argument #3 ($offset) cannot be an array when working on a single string
/// substr_replace(): Argument #4 ($length) cannot be an array when working on a single string
/// ```
#[allow(clippy::too_many_lines)]
#[php_function]
pub(super) fn substr_replace(
    req: &mut Request,
    string: Union<PhpArray, PhpString>,
    replace: Union<PhpArray, PhpString>,
    offset: Union<PhpArray, i64>,
    #[php(default = null)] length: Option<Union<PhpArray, i64>>,
) -> Result<Union<Owner<PhpArray>, Owner<PhpString>>, Thrown> {
    let from = offset;
    match string {
        Union::B(string) => {
            if matches!(from, Union::A(_)) {
                return Err(argument_error(
                    req,
                    "TypeError",
                    3,
                    "cannot be an array when working on a single string",
                ));
            }
            if matches!(length, Some(Union::A(_))) {
                return Err(argument_error(
                    req,
                    "TypeError",
                    4,
                    "cannot be an array when working on a single string",
                ));
            }
            let s = string.as_bytes();
            let slen = long(s.len());
            let mut f = match from {
                Union::B(f) => f,
                Union::A(_) => 0,
            };
            let mut l = match length {
                Some(Union::B(l)) => l,
                _ => slen,
            };
            if f < 0 {
                f = (slen + f).max(0);
            } else if f > slen {
                f = slen;
            }
            if l < 0 {
                l = ((slen - f) + l).max(0);
            }
            l = l.min(slen);
            if f.saturating_add(l) > slen {
                l = slen - f;
            }
            let (f, l) = (
                usize::try_from(f).unwrap_or(0),
                usize::try_from(l).unwrap_or(0),
            );
            let splice = |with: &[u8]| {
                let mut out = Vec::with_capacity(s.len() - l + with.len());
                out.extend_from_slice(&s[..f]);
                out.extend_from_slice(with);
                out.extend_from_slice(&s[f + l..]);
                out
            };
            let out = match replace {
                Union::B(r) => splice(r.as_bytes()),
                Union::A(array) => {
                    let first = walk::rows(req, array).next();
                    match first {
                        Some((_, value)) => with_string(req, &value, splice)?,
                        None => splice(b""),
                    }
                }
            };
            PhpString::new(req, &out).map(Union::B)
        }
        Union::A(strings) => {
            let mut out = PhpArray::new(req)?;
            let (mut from_at, mut length_at, mut replace_at) = (0, 0, 0);
            // The next row of a table walked by bucket position, as
            // `substr_replace` reads `from_ht`, `len_ht` and `repl_ht` beside
            // its walk (`tmp_from = &from_ht->arData[from_idx].val`): the
            // value at the next bucket that holds one, as it stands.
            let next = |table: PhpArray, at: &mut u32| {
                let (found, _, value) = table.live_from(*at)?;
                *at = found.saturating_add(1);
                Some(value.deref())
            };
            // `ZEND_HASH_FOREACH_KEY_VAL(str_ht, num_index, str_index,
            // tmp_str)` (`Z/ext/standard/string.c:2443`), no owner of the
            // strings taken: the argument holds them while `__toString`
            // runs.
            walk::foreach(req, strings, Hold::None, |req, row| -> walk::Step {
                let s = with_string(req, row.value(), <[u8]>::to_vec)?;
                let slen = long(s.len());
                let clamp_from = |f: i64| {
                    if f < 0 {
                        (slen + f).max(0)
                    } else {
                        f.min(slen)
                    }
                };
                let f = match from {
                    Union::A(all) => match next(all, &mut from_at) {
                        Some(value) => clamp_from(to_long(req, &value)?),
                        None => 0,
                    },
                    Union::B(f) => clamp_from(f),
                };
                let mut l = match length {
                    Some(Union::A(all)) => match next(all, &mut length_at) {
                        Some(value) => to_long(req, &value)?,
                        None => slen,
                    },
                    Some(Union::B(l)) => l,
                    None => slen,
                };
                if l < 0 {
                    l = ((slen - f) + l).max(0);
                }
                if f.saturating_add(l) > slen {
                    l = slen - f;
                }
                let (f, l) = (
                    usize::try_from(f).unwrap_or(0),
                    usize::try_from(l).unwrap_or(0),
                );
                let splice = |with: &[u8]| {
                    let mut bytes = Vec::with_capacity(s.len() - l + with.len());
                    bytes.extend_from_slice(&s[..f]);
                    bytes.extend_from_slice(with);
                    bytes.extend_from_slice(&s[f + l..]);
                    bytes
                };
                let bytes = match replace {
                    Union::A(all) => match next(all, &mut replace_at) {
                        Some(value) => with_string(req, &value, splice)?,
                        None => splice(b""),
                    },
                    Union::B(r) => splice(r.as_bytes()),
                };
                let value = PhpString::new(req, &bytes)?.into_value();
                let key = Owner::take(req, row.key().clone());
                out.set(req, key, value)?;
                Ok(ControlFlow::Continue(()))
            })?;
            Ok(Union::A(out))
        }
    }
}
