//! The replacements of `string.c`: `str_replace` and `str_ireplace`
//! over `php_str_replace_in_subject`, `strtr` over `php_strtr_ex` and
//! `php_strtr_array`, and `substr_replace`.

use php_ext::abi::{Raw, Ref, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::{coerce_param, to_long, to_string};
use php_ext::decl::Type;
use php_ext::raising::{Level, Thrown, arg_type_error, arity, docref, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value};

use super::{long, lower, memnstr, str_copy};

/// A key handed to a new array, one more reference to its string.
fn owned_key(req: &mut Request, key: &Key) -> Key {
    if let Key::Str(s) = key {
        req.addref(&Value::string(*s));
    }
    match key {
        Key::Int(n) => Key::Int(*n),
        Key::Str(s) => Key::Str(*s),
    }
}

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
) -> Result<Union<PhpArray, PhpString>, Thrown> {
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
) -> Result<Union<PhpArray, PhpString>, Thrown> {
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
) -> Result<Union<PhpArray, PhpString>, Thrown> {
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
            for (key, entry) in subjects.snapshot() {
                let subject = to_string(req, &entry.deref())?;
                let (value, n) = in_subject(req, &search, &replace, subject, case_sensitive)?;
                count += n;
                let key = owned_key(req, &key);
                out.set(req, key, Value::string(value))?;
            }
            Union::A(out)
        }
        Union::B(subject) => {
            let (value, n) = in_subject(req, &search, &replace, subject, case_sensitive)?;
            count = n;
            Union::B(value)
        }
    };
    counted.set(req, Untyped(Value::int(count)))?;
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
) -> Result<(PhpString, i64), Thrown> {
    if subject.is_empty() {
        return Ok((PhpString::new(req, b"")?, 0));
    }
    let mut count = 0i64;
    let mut current: Option<Vec<u8>> = None;
    match search {
        Union::A(searches) => {
            let mut replacements = match replace {
                Union::A(array) => Some(array.snapshot().into_iter()),
                Union::B(_) => None,
            };
            for (_, entry) in searches.snapshot() {
                let needle = to_string(req, &entry.deref())?;
                let with: Vec<u8> = match (&mut replacements, replace) {
                    (Some(iter), _) => match iter.next() {
                        Some((_, value)) => to_string(req, &value.deref())?.as_bytes().to_vec(),
                        None => Vec::new(),
                    },
                    (None, Union::B(s)) => s.as_bytes().to_vec(),
                    (None, Union::A(_)) => Vec::new(),
                };
                if needle.is_empty() {
                    continue;
                }
                let haystack = current.as_deref().unwrap_or(subject.as_bytes());
                let (replaced, n) =
                    replace_bytes(haystack, needle.as_bytes(), &with, case_sensitive);
                count += n;
                if n > 0 {
                    current = Some(replaced);
                }
                if current.as_ref().is_some_and(Vec::is_empty) {
                    return Ok((PhpString::new(req, b"")?, count));
                }
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
        None => str_copy(req, subject),
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
) -> Result<PhpString, Thrown> {
    // php's own parse, chosen by the count: `(string, array)` for two
    // arguments, `(string, string, string)` for three.
    let given = req.argument_count();
    if given <= 2 {
        arity(req, 2, Some(2))?;
        let string = string.hold(req)?;
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
        return Ok(str_copy(req, string));
    }
    let mut xlat: [u8; 256] = std::array::from_fn(|i| u8::try_from(i).unwrap_or(0));
    for (f, t) in from.as_bytes()[..n].iter().zip(&to.as_bytes()[..n]) {
        xlat[usize::from(*f)] = *t;
    }
    let bytes = string.as_bytes();
    if bytes.iter().all(|c| xlat[usize::from(*c)] == *c) {
        return Ok(str_copy(req, string));
    }
    let out: Vec<u8> = bytes.iter().map(|c| xlat[usize::from(*c)]).collect();
    PhpString::new(req, &out)
}

/// `php_strtr_array` and `php_strtr_array_ex`.
fn strtr_array(req: &mut Request, string: PhpString, from: PhpArray) -> Result<PhpString, Thrown> {
    let s = string.as_bytes();
    let entries = from.snapshot();
    if entries.is_empty() {
        return Ok(str_copy(req, string));
    }
    let key_bytes = |key: &Key| -> Vec<u8> {
        match key {
            Key::Int(n) => n.to_string().into_bytes(),
            Key::Str(k) => k.as_bytes().to_vec(),
        }
    };
    if entries.len() == 1 {
        let (key, value) = &entries[0];
        let k = key_bytes(key);
        let with = to_string(req, &value.deref())?;
        if k.is_empty() {
            docref(req, Level::Warning, "Ignoring replacement of empty string");
            return Ok(str_copy(req, string));
        }
        let (out, n) = replace_bytes(s, &k, with.as_bytes(), true);
        if n == 0 {
            return Ok(str_copy(req, string));
        }
        return PhpString::new(req, &out);
    }
    let mut patterns: Vec<(Vec<u8>, Value)> = Vec::with_capacity(entries.len());
    let (mut minlen, mut maxlen) = (128 * 1024usize, 0usize);
    for (key, value) in &entries {
        let k = key_bytes(key);
        if matches!(key, Key::Str(_)) && k.is_empty() {
            docref(req, Level::Warning, "Ignoring replacement of empty string");
            continue;
        }
        if k.len() > s.len() {
            continue;
        }
        maxlen = maxlen.max(k.len());
        minlen = minlen.min(k.len());
        patterns.push((k, *value));
    }
    if minlen > maxlen {
        return Ok(str_copy(req, string));
    }
    let mut out: Option<Vec<u8>> = None;
    let (mut pos, mut old_pos) = (0usize, 0usize);
    while pos + minlen <= s.len() {
        let mut len = maxlen.min(s.len() - pos);
        while len >= minlen {
            let key = &s[pos..pos + len];
            if let Some((_, value)) = patterns.iter().find(|(k, _)| k.as_slice() == key) {
                let with = to_string(req, &value.deref())?;
                let buffer = out.get_or_insert_with(Vec::new);
                buffer.extend_from_slice(&s[old_pos..pos]);
                buffer.extend_from_slice(with.as_bytes());
                old_pos = pos + len;
                pos = old_pos - 1;
                break;
            }
            len -= 1;
        }
        pos += 1;
    }
    match out {
        Some(mut buffer) => {
            buffer.extend_from_slice(&s[old_pos..]);
            PhpString::new(req, &buffer)
        }
        None => Ok(str_copy(req, string)),
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
) -> Result<Union<PhpArray, PhpString>, Thrown> {
    let from = offset;
    match string {
        Union::B(string) => {
            if matches!(from, Union::A(_)) {
                return Err(throw(
                    req,
                    "TypeError",
                    "substr_replace(): Argument #3 ($offset) cannot be an array when working on a single string",
                ));
            }
            if matches!(length, Some(Union::A(_))) {
                return Err(throw(
                    req,
                    "TypeError",
                    "substr_replace(): Argument #4 ($length) cannot be an array when working on a single string",
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
            let with: Vec<u8> = match replace {
                Union::B(r) => r.as_bytes().to_vec(),
                Union::A(array) => match array.iter().next() {
                    Some((_, value)) => to_string(req, &value.deref())?.as_bytes().to_vec(),
                    None => Vec::new(),
                },
            };
            let (f, l) = (
                usize::try_from(f).unwrap_or(0),
                usize::try_from(l).unwrap_or(0),
            );
            let mut out = Vec::with_capacity(s.len() - l + with.len());
            out.extend_from_slice(&s[..f]);
            out.extend_from_slice(&with);
            out.extend_from_slice(&s[f + l..]);
            PhpString::new(req, &out).map(Union::B)
        }
        Union::A(strings) => {
            let mut out = PhpArray::new(req)?;
            let mut froms = match from {
                Union::A(array) => Some(array.snapshot().into_iter()),
                Union::B(_) => None,
            };
            let mut lengths = match length {
                Some(Union::A(array)) => Some(array.snapshot().into_iter()),
                _ => None,
            };
            let mut replaces = match replace {
                Union::A(array) => Some(array.snapshot().into_iter()),
                Union::B(_) => None,
            };
            for (key, entry) in strings.snapshot() {
                let orig = to_string(req, &entry.deref())?;
                let s = orig.as_bytes().to_vec();
                let slen = long(s.len());
                let clamp_from = |f: i64| {
                    if f < 0 {
                        (slen + f).max(0)
                    } else {
                        f.min(slen)
                    }
                };
                let f = match (&mut froms, &from) {
                    (Some(iter), _) => match iter.next() {
                        Some((_, value)) => clamp_from(to_long(req, &value.deref())?),
                        None => 0,
                    },
                    (None, Union::B(f)) => clamp_from(*f),
                    (None, Union::A(_)) => 0,
                };
                let mut l = match (&mut lengths, &length) {
                    (Some(iter), _) => match iter.next() {
                        Some((_, value)) => to_long(req, &value.deref())?,
                        None => slen,
                    },
                    (None, Some(Union::B(l))) => *l,
                    (None, _) => slen,
                };
                if l < 0 {
                    l = ((slen - f) + l).max(0);
                }
                if f.saturating_add(l) > slen {
                    l = slen - f;
                }
                let with: Vec<u8> = match (&mut replaces, &replace) {
                    (Some(iter), _) => match iter.next() {
                        Some((_, value)) => to_string(req, &value.deref())?.as_bytes().to_vec(),
                        None => Vec::new(),
                    },
                    (None, Union::B(r)) => r.as_bytes().to_vec(),
                    (None, Union::A(_)) => Vec::new(),
                };
                let (f, l) = (
                    usize::try_from(f).unwrap_or(0),
                    usize::try_from(l).unwrap_or(0),
                );
                let mut bytes = Vec::with_capacity(s.len() - l + with.len());
                bytes.extend_from_slice(&s[..f]);
                bytes.extend_from_slice(&with);
                bytes.extend_from_slice(&s[f + l..]);
                let value = Value::string(PhpString::new(req, &bytes)?);
                let key = owned_key(req, &key);
                out.set(req, key, value)?;
            }
            Ok(Union::A(out))
        }
    }
}
