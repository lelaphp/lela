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

use super::{Decimal, long, lower, memnstr};

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
#[php_function]
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

/// `php_str_replace_in_subject`: the subject after every search, and
/// the count of replacements.
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
    let mut current: Option<Vec<u8>> = None;
    match search {
        Union::A(searches) => {
            let mut replace_at = 0;
            // `ZEND_HASH_FOREACH_VAL(search_ht, search_entry)`
            // (`Z/ext/standard/string.c:4344`), no owner of the searches
            // taken: the argument holds them while `zval_get_tmp_string`
            // runs `__toString`. The replacement is read at the next bucket
            // of its table that holds one, as it stands at the step.
            let emptied = walk::foreach(req, *searches, Hold::None, |req, row| -> walk::Step {
                let needle = with_string(req, row.value(), <[u8]>::to_vec)?;
                let with: Vec<u8> = match replace {
                    Union::A(all) => match all.live_from(replace_at) {
                        Some((found, _, value)) => {
                            replace_at = found.saturating_add(1);
                            with_string(req, &value, <[u8]>::to_vec)?
                        }
                        None => Vec::new(),
                    },
                    Union::B(s) => s.as_bytes().to_vec(),
                };
                if needle.is_empty() {
                    return Ok(ControlFlow::Continue(()));
                }
                let haystack = current.as_deref().unwrap_or(subject.as_bytes());
                let (replaced, n) = replace_bytes(haystack, &needle, &with, case_sensitive);
                count += n;
                if n > 0 {
                    current = Some(replaced);
                }
                if current.as_ref().is_some_and(Vec::is_empty) {
                    return Ok(ControlFlow::Break(()));
                }
                Ok(ControlFlow::Continue(()))
            })?;
            if emptied.is_some() {
                return Ok((PhpString::new(req, b"")?, count));
            }
        }
        Union::B(needle) => {
            let with: &[u8] = match replace {
                Union::B(s) => s.as_bytes(),
                Union::A(_) => b"",
            };
            if !needle.is_empty() {
                let (replaced, n) =
                    replace_bytes(subject.as_bytes(), needle.as_bytes(), with, case_sensitive);
                count = n;
                if n > 0 {
                    current = Some(replaced);
                }
            }
        }
    }
    let value = match current {
        Some(bytes) => PhpString::new(req, &bytes)?,
        None => Owner::take(req, subject),
    };
    Ok((value, count))
}

/// `php_char_to_str_ex`, `php_str_to_str_ex` and `php_str_to_str_i_ex`
/// as one: every non-overlapping occurrence of `needle` left to right
/// replaced by `with`, folding both sides when not `case_sensitive`.
fn replace_bytes(
    haystack: &[u8],
    needle: &[u8],
    with: &[u8],
    case_sensitive: bool,
) -> (Vec<u8>, i64) {
    let folded: Vec<u8>;
    let lowered_needle: Vec<u8>;
    let (h, n) = if case_sensitive {
        (haystack, needle)
    } else {
        folded = haystack.iter().map(|c| lower(*c)).collect();
        lowered_needle = needle.iter().map(|c| lower(*c)).collect();
        (folded.as_slice(), lowered_needle.as_slice())
    };
    let mut out = Vec::with_capacity(haystack.len());
    let mut count = 0;
    let mut at = 0;
    while let Some(found) = memnstr(h, n, at) {
        out.extend_from_slice(&haystack[at..found]);
        out.extend_from_slice(with);
        at = found + n.len();
        count += 1;
    }
    out.extend_from_slice(&haystack[at..]);
    (out, count)
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
#[php_function(arity = manual)]
pub(super) fn strtr(
    req: &mut Request,
    string: Raw<PhpString>,
    from: Raw<Union<PhpArray, PhpString>>,
    #[php(default = null)] to: Raw<Option<PhpString>>,
) -> Result<Owner<PhpString>, Thrown> {
    // php's own parse, chosen by the count: `(string, array)` for two
    // arguments, `(string, string, string)` for three.
    let given = req.argument_count();
    if given <= 2 {
        arity(req, 2, Some(2))?;
        let string = string.hold(req)?;
        // `Z_PARAM_ARRAY_HT`, where the stub declares `string|array $from`.
        let from = coerce_param(req, &from.value(), &Type::Array, 2)?;
        let Some(from) = from.as_array().copied() else {
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
    let (Some(from), Some(to)) = (from.as_php_string().copied(), to.as_php_string().copied())
    else {
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
    let digits;
    let k = match &key {
        Key::Int(n) => {
            digits = Decimal::of(*n);
            digits.as_bytes()
        }
        Key::Str(k) => k.as_bytes(),
    };
    let s = string.as_bytes();
    let replaced = with_string(req, &value, |with| {
        (!k.is_empty()).then(|| replace_bytes(s, k, with, true))
    })?;
    let Some((out, n)) = replaced else {
        docref(req, Level::Warning, "Ignoring replacement of empty string");
        return Ok(Owner::take(req, string));
    };
    if n == 0 {
        return Ok(Owner::take(req, string));
    }
    PhpString::new(req, &out)
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
        let digits;
        let k = match &key {
            Key::Int(n) => {
                numeric = true;
                digits = Decimal::of(*n);
                digits.as_bytes()
            }
            Key::Str(k) => k.as_bytes(),
        };
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
