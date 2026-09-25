//! `Z/ext/standard/string.c`: the string natives the crate root does
//! not already declare, in the order `basic_functions.stub.php` gives
//! them, each over php's own algorithm. The crate root keeps the eight
//! it declared first (`trim`, `str_contains`, `substr`, `strtoupper`,
//! `strtolower`, `str_pad`, `str_repeat`, `ord` and `chr`); the rest of
//! the file is here and in its children: the searches in `find`, the
//! path natives in `path`, the replacements in `replace`, the escapes in
//! `slashes`, `strip_tags`'s state machine in `strip_tags`, the locale
//! natives in `locale` and `parse_str` in `parse_str`. The C locale is
//! the only one this engine has (#640), so `isalpha` and the case maps
//! are ASCII's, as php's are under it.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod find;
mod locale;
mod parse_str;
pub(super) mod path;
mod replace;
mod slashes;
mod strip_tags;

pub use strip_tags::strip_tags_ex;

use php_ext::coerce::to_string;
use php_ext::decl::FunctionDecl;
use php_ext::raising::{Level, Thrown, arg_value_error, deprecated, docref, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value};
use php_ext_random::Rng;

/// The request state `string.c` keeps: `BG(strtok_string)` and its
/// cursor, and each locale category's name.
#[derive(Default)]
pub(super) struct State {
    strtok: Option<(Vec<u8>, usize)>,
    locale: locale::Locale,
}

/// The constants the natives of `string.c` read: the locale's, then
/// `pathinfo`'s flags.
///
/// ```php
/// var_dump(CHAR_MAX, LC_CTYPE, LC_NUMERIC, LC_TIME, LC_COLLATE, LC_MONETARY, LC_ALL, LC_MESSAGES, CODESET, ABDAY_1, MON_1, CRNCYSTR, PATHINFO_DIRNAME, PATHINFO_BASENAME, PATHINFO_EXTENSION, PATHINFO_FILENAME, PATHINFO_ALL);
/// ```
/// ```expect
/// int(127)
/// int(2)
/// int(4)
/// int(5)
/// int(1)
/// int(3)
/// int(0)
/// int(6)
/// int(0)
/// int(14)
/// int(21)
/// int(56)
/// int(1)
/// int(2)
/// int(4)
/// int(8)
/// int(15)
/// ```
pub(crate) fn constants() -> Vec<php_ext::decl::ConstDecl> {
    let mut out = locale::constants();
    for (name, value) in [
        (&b"PATHINFO_DIRNAME"[..], path::PATHINFO_DIRNAME),
        (b"PATHINFO_BASENAME", path::PATHINFO_BASENAME),
        (b"PATHINFO_EXTENSION", path::PATHINFO_EXTENSION),
        (b"PATHINFO_FILENAME", path::PATHINFO_FILENAME),
        (b"PATHINFO_ALL", path::PATHINFO_ALL),
    ] {
        out.push(crate::constant(name, value));
    }
    out
}

use crate::output::Entry;
use crate::stub::{deprecated_since, stub};
use crate::zpp::{
    ArrayOr, array_or_string, long_of, nullable_array, nullable_long, nullable_string,
    optional_bool, optional_long, optional_string, same, string_value,
};
use crate::{argument_count_between, charmask, long_argument, string_argument};

/// The declarations, in the stub's order, less the crate root's.
#[allow(clippy::too_many_lines)]
///
/// ```php
/// foreach (['bin2hex', 'strspn', 'implode', 'strtok', 'dirname', 'strrchr', 'substr_replace', 'similar_text', 'str_replace', 'setlocale', 'sscanf', 'substr_compare', 'utf8_encode'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function bin2hex ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function strspn ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <required> string $characters ]
///     Parameter #2 [ <optional> int $offset = 0 ]
///     Parameter #3 [ <optional> ?int $length = null ]
///   }
///   - Return [ int ]
/// }
/// Function [ <internal:standard> function implode ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array|string $separator ]
///     Parameter #1 [ <optional> ?array $array = null ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function strtok ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <optional> ?string $token = null ]
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:standard> function dirname ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $path ]
///     Parameter #1 [ <optional> int $levels = 1 ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function strrchr ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $haystack ]
///     Parameter #1 [ <required> string $needle ]
///     Parameter #2 [ <optional> bool $before_needle = false ]
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:standard> function substr_replace ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> array|string $string ]
///     Parameter #1 [ <required> array|string $replace ]
///     Parameter #2 [ <required> array|int $offset ]
///     Parameter #3 [ <optional> array|int|null $length = null ]
///   }
///   - Return [ array|string ]
/// }
/// Function [ <internal:standard> function similar_text ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $string1 ]
///     Parameter #1 [ <required> string $string2 ]
///     Parameter #2 [ <optional> &$percent = null ]
///   }
///   - Return [ int ]
/// }
/// Function [ <internal:standard> function str_replace ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> array|string $search ]
///     Parameter #1 [ <required> array|string $replace ]
///     Parameter #2 [ <required> array|string $subject ]
///     Parameter #3 [ <optional> &$count = null ]
///   }
///   - Return [ array|string ]
/// }
/// Function [ <internal:standard> function setlocale ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> int $category ]
///     Parameter #1 [ <required> $locales ]
///     Parameter #2 [ <optional> ...$rest ]
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:standard> function sscanf ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <required> string $format ]
///     Parameter #2 [ <optional> mixed &...$vars ]
///   }
///   - Return [ array|int|null ]
/// }
/// Function [ <internal:standard> function substr_compare ] {
///
///   - Parameters [5] {
///     Parameter #0 [ <required> string $haystack ]
///     Parameter #1 [ <required> string $needle ]
///     Parameter #2 [ <required> int $offset ]
///     Parameter #3 [ <optional> ?int $length = null ]
///     Parameter #4 [ <optional> bool $case_insensitive = false ]
///   }
///   - Return [ int ]
/// }
/// Function [ <internal, deprecated:standard> function utf8_encode ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ string ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let trim_default = " \\n\\r\\t\\v\\0";
    vec![
        (stub("bin2hex(string $string): string"), bin2hex as Entry),
        (stub("hex2bin(string $string): string|false"), hex2bin),
        (
            stub(
                "strspn(string $string, string $characters, int $offset = 0, ?int $length = null): int",
            ),
            strspn,
        ),
        (
            stub(
                "strcspn(string $string, string $characters, int $offset = 0, ?int $length = null): int",
            ),
            strcspn,
        ),
        (
            stub("nl_langinfo(int $item): string|false"),
            locale::nl_langinfo,
        ),
        (
            stub("strcoll(string $string1, string $string2): int"),
            locale::strcoll,
        ),
        (
            stub(&format!(
                "rtrim(string $string, string $characters = \"{trim_default}\"): string"
            )),
            rtrim,
        ),
        (
            stub(&format!(
                "chop(string $string, string $characters = \"{trim_default}\"): string"
            )),
            rtrim,
        ),
        (
            stub(&format!(
                "ltrim(string $string, string $characters = \"{trim_default}\"): string"
            )),
            ltrim,
        ),
        (
            stub(
                "wordwrap(string $string, int $width = 75, string $break = \"\\n\", bool $cut_long_words = false): string",
            ),
            wordwrap,
        ),
        (
            stub("explode(string $separator, string $string, int $limit = PHP_INT_MAX): array"),
            explode,
        ),
        (
            stub("implode(array|string $separator, ?array $array = null): string"),
            implode,
        ),
        (
            stub("join(array|string $separator, ?array $array = null): string"),
            join,
        ),
        (
            stub("strtok(string $string, ?string $token = null): string|false"),
            strtok,
        ),
        (stub("str_increment(string $string): string"), str_increment),
        (stub("str_decrement(string $string): string"), str_decrement),
        (
            stub("basename(string $path, string $suffix = \"\"): string"),
            path::basename,
        ),
        (
            stub("dirname(string $path, int $levels = 1): string"),
            path::dirname,
        ),
        (
            stub("pathinfo(string $path, int $flags = PATHINFO_ALL): array|string"),
            path::pathinfo,
        ),
        (
            stub(
                "stristr(string $haystack, string $needle, bool $before_needle = false): string|false",
            ),
            find::stristr,
        ),
        (
            stub(
                "strstr(string $haystack, string $needle, bool $before_needle = false): string|false",
            ),
            find::strstr,
        ),
        (
            stub(
                "strchr(string $haystack, string $needle, bool $before_needle = false): string|false",
            ),
            find::strstr,
        ),
        (
            stub("strpos(string $haystack, string $needle, int $offset = 0): int|false"),
            find::strpos,
        ),
        (
            stub("stripos(string $haystack, string $needle, int $offset = 0): int|false"),
            find::stripos,
        ),
        (
            stub("strrpos(string $haystack, string $needle, int $offset = 0): int|false"),
            find::strrpos,
        ),
        (
            stub("strripos(string $haystack, string $needle, int $offset = 0): int|false"),
            find::strripos,
        ),
        (
            stub(
                "strrchr(string $haystack, string $needle, bool $before_needle = false): string|false",
            ),
            find::strrchr,
        ),
        (
            stub("str_starts_with(string $haystack, string $needle): bool"),
            find::str_starts_with,
        ),
        (
            stub("str_ends_with(string $haystack, string $needle): bool"),
            find::str_ends_with,
        ),
        (
            stub(
                "chunk_split(string $string, int $length = 76, string $separator = \"\\r\\n\"): string",
            ),
            chunk_split,
        ),
        (
            stub(
                "substr_replace(array|string $string, array|string $replace, array|int $offset, array|int|null $length = null): array|string",
            ),
            replace::substr_replace,
        ),
        (stub("quotemeta(string $string): string"), quotemeta),
        (stub("ucfirst(string $string): string"), ucfirst),
        (stub("lcfirst(string $string): string"), lcfirst),
        (
            stub("ucwords(string $string, string $separators = \" \\t\\r\\n\\f\\v\"): string"),
            ucwords,
        ),
        (
            stub("strtr(string $string, array|string $from, ?string $to = null): string"),
            replace::strtr,
        ),
        (stub("strrev(string $string): string"), strrev),
        (
            stub("similar_text(string $string1, string $string2, &$percent = null): int"),
            similar_text,
        ),
        (
            stub("addcslashes(string $string, string $characters): string"),
            slashes::addcslashes,
        ),
        (
            stub("addslashes(string $string): string"),
            slashes::addslashes,
        ),
        (
            stub("stripcslashes(string $string): string"),
            slashes::stripcslashes,
        ),
        (
            stub("stripslashes(string $string): string"),
            slashes::stripslashes,
        ),
        (
            stub(
                "str_replace(array|string $search, array|string $replace, array|string $subject, &$count = null): array|string",
            ),
            replace::str_replace,
        ),
        (
            stub(
                "str_ireplace(array|string $search, array|string $replace, array|string $subject, &$count = null): array|string",
            ),
            replace::str_ireplace,
        ),
        (
            stub("hebrev(string $string, int $max_chars_per_line = 0): string"),
            hebrev,
        ),
        (
            stub("nl2br(string $string, bool $use_xhtml = true): string"),
            nl2br,
        ),
        (
            stub("strip_tags(string $string, array|string|null $allowed_tags = null): string"),
            strip_tags::strip_tags,
        ),
        (
            stub("setlocale(int $category, $locales, ...$rest): string|false"),
            locale::setlocale,
        ),
        (
            stub("parse_str(string $string, &$result): void"),
            parse_str::parse_str,
        ),
        (
            stub(
                "str_getcsv(string $string, string $separator = \",\", string $enclosure = \"\\\"\", string $escape = \"\\\\\"): array",
            ),
            crate::csv::str_getcsv,
        ),
        (
            stub("count_chars(string $string, int $mode = 0): array|string"),
            count_chars,
        ),
        (
            stub("strnatcmp(string $string1, string $string2): int"),
            crate::strnatcmp::strnatcmp,
        ),
        (stub("localeconv(): array"), locale::localeconv),
        (
            stub("strnatcasecmp(string $string1, string $string2): int"),
            crate::strnatcmp::strnatcasecmp,
        ),
        (
            stub(
                "substr_count(string $haystack, string $needle, int $offset = 0, ?int $length = null): int",
            ),
            substr_count,
        ),
        (
            stub("sscanf(string $string, string $format, mixed &...$vars): array|int|null"),
            crate::scanf::sscanf,
        ),
        (stub("str_rot13(string $string): string"), str_rot13),
        (stub("str_shuffle(string $string): string"), str_shuffle),
        (
            stub(
                "str_word_count(string $string, int $format = 0, ?string $characters = null): array|int",
            ),
            str_word_count,
        ),
        (
            stub("str_split(string $string, int $length = 1): array"),
            str_split,
        ),
        (
            stub("strpbrk(string $string, string $characters): string|false"),
            strpbrk,
        ),
        (
            stub(
                "substr_compare(string $haystack, string $needle, int $offset, ?int $length = null, bool $case_insensitive = false): int",
            ),
            substr_compare,
        ),
        (
            deprecated_since(stub("utf8_encode(string $string): string"), "8.2"),
            utf8_encode,
        ),
        (
            deprecated_since(stub("utf8_decode(string $string): string"), "8.2"),
            utf8_decode,
        ),
    ]
}

/// `zend_tolower_ascii`.
const fn lower(byte: u8) -> u8 {
    byte.to_ascii_lowercase()
}

/// `zend_memnstr`: the first offset of `needle` in `haystack` at or
/// after `from`, the empty needle found where the search starts.
fn memnstr(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let rest = haystack.get(from..)?;
    if needle.is_empty() {
        return Some(from);
    }
    if needle.len() > rest.len() {
        return None;
    }
    rest.windows(needle.len())
        .position(|window| window == needle)
        .map(|at| at + from)
}

/// The `zend_long` php returns for a length or an offset.
fn long(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `bin2hex(string $string): string`, `php_bin2hex`: each byte as two
/// lowercase hex digits.
///
/// ```php
/// var_dump(bin2hex(""), bin2hex("abc"), bin2hex("\x00\xff\x10"), bin2hex(123));
/// ```
/// ```expect
/// string(0) ""
/// string(6) "616263"
/// string(6) "00ff10"
/// string(6) "313233"
/// ```
fn bin2hex(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let data = string_argument(req, 1)?;
    let input = data.as_bytes();
    let hex = PhpString::product(req, input.len(), 2, 0, |out| {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        for (i, byte) in input.iter().enumerate() {
            out[2 * i] = DIGITS[usize::from(byte >> 4)];
            out[2 * i + 1] = DIGITS[usize::from(byte & 15)];
        }
    })?;
    Ok(Value::string(hex))
}

/// `hex2bin(string $string): string|false`, `php_hex2bin`: pairs of hex
/// digits in either case as bytes, and php's two warnings and `false`
/// for an odd length and for a byte that is no digit.
///
/// ```php
/// var_dump(hex2bin("616263"), hex2bin("4A4b"), hex2bin(""), hex2bin("abc"), hex2bin("zz"));
/// ```
/// ```expect
/// Warning: hex2bin(): Hexadecimal input string must have an even length in %s on line 2
///
/// Warning: hex2bin(): Input string must be hexadecimal string in %s on line 2
/// string(3) "abc"
/// string(2) "JK"
/// string(0) ""
/// bool(false)
/// bool(false)
/// ```
fn hex2bin(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let data = string_argument(req, 1)?;
    let input = data.as_bytes();
    if input.len() % 2 != 0 {
        docref(
            req,
            Level::Warning,
            "Hexadecimal input string must have an even length",
        );
        return Ok(Value::bool(false));
    }
    let digit = |c: u8| (c as char).to_digit(16).and_then(|d| u8::try_from(d).ok());
    let mut out = Vec::with_capacity(input.len() / 2);
    for pair in input.chunks_exact(2) {
        let (Some(high), Some(low)) = (digit(pair[0]), digit(pair[1])) else {
            docref(
                req,
                Level::Warning,
                "Input string must be hexadecimal string",
            );
            return Ok(Value::bool(false));
        };
        out.push((high << 4) | low);
    }
    string_value(req, &out)
}

/// `strspn(string $string, string $characters, int $offset = 0, ?int
/// $length = null): int`, `php_spn_common_handler`: the length of the
/// leading span of the window `$offset`, `$length` whose bytes are all
/// in `$characters`, the window clamped into the string as `substr`'s
/// is.
///
/// ```php
/// var_dump(strspn("42 is the answer", "1234567890"), strspn("foo", "o", 1, 2), strspn("foo", "o", -2), strspn("abc", "abc", 1, -1), strspn("abc", ""), strspn("abc", "a", 10), strspn("abc", "ab", -10, 100));
/// var_dump(strcspn("abcd", "cd"), strcspn("hello", "l", -4), strcspn("abcd", "x", 1, 2), strcspn("", "a"), strcspn("abc", ""), strcspn("abc", "c", 0, -1));
/// ```
/// ```expect
/// int(2)
/// int(2)
/// int(2)
/// int(1)
/// int(0)
/// int(0)
/// int(2)
/// int(2)
/// int(1)
/// int(2)
/// int(0)
/// int(3)
/// int(2)
/// ```
fn strspn(req: &mut Request) -> Result<Value, Thrown> {
    spn(req, true)
}

/// `strcspn(...)`: the length of the leading span of the window whose
/// bytes are none of `$characters`.
///
/// ```php
/// var_dump(strcspn("abcd", "cd"), strcspn("hello", "l", -4), strcspn("abcd", "x", 1, 2), strcspn("", "a"), strcspn("abc", ""), strcspn("abc", "c", 0, -1), strcspn("abc", "a", 5));
/// ```
/// ```expect
/// int(2)
/// int(1)
/// int(2)
/// int(0)
/// int(3)
/// int(2)
/// int(0)
/// ```
fn strcspn(req: &mut Request) -> Result<Value, Thrown> {
    spn(req, false)
}

/// `php_spn_common_handler(..., is_strspn)`.
fn spn(req: &mut Request, must_match: bool) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 4)?;
    let string = string_argument(req, 1)?;
    let characters = string_argument(req, 2)?;
    let mut start = optional_long(req, 3, 0)?;
    let length = nullable_long(req, 4)?;
    let bytes = string.as_bytes();
    let remain = long(bytes.len());
    if start < 0 {
        start = (start + remain).max(0);
    } else if start > remain {
        start = remain;
    }
    let remain = remain - start;
    let len = match length {
        Some(len) if len < 0 => (len + remain).max(0),
        Some(len) => len.min(remain),
        None => remain,
    };
    if len == 0 {
        return Ok(Value::int(0));
    }
    let from = usize::try_from(start).unwrap_or(0);
    let to = from + usize::try_from(len).unwrap_or(0);
    let mut table = [false; 256];
    for &c in characters.as_bytes() {
        table[usize::from(c)] = true;
    }
    let run = bytes[from..to]
        .iter()
        .take_while(|c| table[usize::from(**c)] == must_match)
        .count();
    Ok(long_of(run))
}

/// `php_do_trim(..., mode)` for `rtrim` and `ltrim`: `trim`'s masks,
/// one side taken.
fn trim_side(req: &mut Request, left: bool) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let string = string_argument(req, 1)?;
    let what = optional_string(req, 2)?;
    let bytes = string.as_bytes();
    let strip: Box<dyn Fn(u8) -> bool> = match what {
        Some(what) if what.len() == 1 => {
            let p = what.as_bytes()[0];
            Box::new(move |c| c == p)
        }
        Some(what) => {
            let mask = charmask(req, what.as_bytes());
            Box::new(move |c| mask[usize::from(c)])
        }
        None => Box::new(|c| c <= b' ' && matches!(c, b' ' | b'\n' | b'\r' | b'\t' | 0x0B | 0)),
    };
    let (start, end) = if left {
        let start = bytes.iter().take_while(|c| strip(**c)).count();
        (start, bytes.len())
    } else {
        let kept = bytes.len() - bytes.iter().rev().take_while(|c| strip(**c)).count();
        (0, kept)
    };
    if end - start == bytes.len() {
        return Ok(same(req, string));
    }
    string_value(req, &bytes[start..end])
}

/// `rtrim(string $string, string $characters = " \n\r\t\v\0"): string`
/// and its alias `chop`: `php_trim_int` with its second bit, the bytes the mask
/// names taken off the end.
///
/// ```php
/// var_dump(rtrim("  ab  "), chop("xxaxx", "x"), rtrim("abc", "a..c"), rtrim("a\0\n"), rtrim("", "x"), ltrim("  ab  "), ltrim("xxaxx", "x"), ltrim("0012", "0..1"), ltrim("abc", "z..a"));
/// ```
/// ```expect
/// Warning: ltrim(): Invalid '..'-range, '..'-range needs to be incrementing in %s on line 2
/// string(4) "  ab"
/// string(3) "xxa"
/// string(0) ""
/// string(1) "a"
/// string(0) ""
/// string(4) "ab  "
/// string(3) "axx"
/// string(1) "2"
/// string(2) "bc"
/// ```
fn rtrim(req: &mut Request) -> Result<Value, Thrown> {
    trim_side(req, false)
}

/// `ltrim(...)`: `php_trim_int` with its first bit, the start.
///
/// ```php
/// var_dump(ltrim("  ab  "), ltrim("xxaxx", "x"), ltrim("0012", "0..1"), ltrim("\t\n a"), ltrim("abc", "z..a"));
/// ```
/// ```expect
/// Warning: ltrim(): Invalid '..'-range, '..'-range needs to be incrementing in %s on line 2
/// string(4) "ab  "
/// string(3) "axx"
/// string(1) "2"
/// string(1) "a"
/// string(2) "bc"
/// ```
fn ltrim(req: &mut Request) -> Result<Value, Thrown> {
    trim_side(req, true)
}

/// `wordwrap(string $string, int $width = 75, string $break = "\n",
/// bool $cut_long_words = false): string`, php's two loops: the
/// in-place one for a one-byte break without cutting, and the copying
/// one for every other shape, each breaking at the last space before
/// the width and, when cutting, inside a word that is longer.
///
/// ```php
/// var_dump(wordwrap("The quick brown fox sat over the lazy dog", 15, "<br />\n"), wordwrap("A very long woooooooooooord.", 8, "\n", true), wordwrap("A very long woooooooooooooooooord. and something", 8, "\n", false), wordwrap("short", 10), wordwrap("", 5), wordwrap("abc def", 0), wordwrap("aaaa bbbb", 2, "--", true));
/// try { wordwrap("x", 1, ""); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { wordwrap("x", 0, "\n", true); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(53) "The quick brown<br />
/// fox sat over<br />
/// the lazy dog"
/// string(29) "A very
/// long
/// wooooooo
/// ooooord."
/// string(48) "A very
/// long
/// woooooooooooooooooord.
/// and
/// something"
/// string(5) "short"
/// string(0) ""
/// string(7) "abc
/// def"
/// string(14) "aa--aa--bb--bb"
/// wordwrap(): Argument #3 ($break) must not be empty
/// wordwrap(): Argument #4 ($cut_long_words) cannot be true when argument #2 ($width) is 0
/// ```
fn wordwrap(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 4)?;
    let text = string_argument(req, 1)?;
    let linelength = optional_long(req, 2, 75)?;
    let breakchar = optional_string(req, 3)?;
    let docut = optional_bool(req, 4, false)?;
    let text = text.as_bytes();
    if text.is_empty() {
        return string_value(req, b"");
    }
    let brk: &[u8] = breakchar.as_ref().map_or(b"\n", |b| b.as_bytes());
    if brk.is_empty() {
        return Err(arg_value_error(req, 3, "not be empty"));
    }
    if linelength == 0 && docut {
        return Err(throw(
            req,
            "ValueError",
            "wordwrap(): Argument #4 ($cut_long_words) cannot be true when argument #2 ($width) is 0",
        ));
    }
    // `current - laststart` as php's signed `zend_long`s compare it.
    let gap = |a: usize, b: usize| long(a) - long(b);
    if brk.len() == 1 && !docut {
        let mut out = text.to_vec();
        let (mut laststart, mut lastspace) = (0usize, 0usize);
        for (current, &c) in text.iter().enumerate() {
            if c == brk[0] {
                laststart = current + 1;
                lastspace = current + 1;
            } else if c == b' ' {
                if gap(current, laststart) >= linelength {
                    out[current] = brk[0];
                    laststart = current + 1;
                }
                lastspace = current;
            } else if gap(current, laststart) >= linelength && laststart != lastspace {
                out[lastspace] = brk[0];
                laststart = lastspace + 1;
            }
        }
        return string_value(req, &out);
    }
    let mut out: Vec<u8> = Vec::with_capacity(text.len() + brk.len());
    let (mut laststart, mut lastspace) = (0usize, 0usize);
    let mut current = 0usize;
    while current < text.len() {
        let c = text[current];
        if c == brk[0] && current + brk.len() < text.len() && text[current..].starts_with(brk) {
            out.extend_from_slice(&text[laststart..current + brk.len()]);
            current += brk.len() - 1;
            laststart = current + 1;
            lastspace = current + 1;
        } else if c == b' ' {
            if gap(current, laststart) >= linelength {
                out.extend_from_slice(&text[laststart..current]);
                out.extend_from_slice(brk);
                laststart = current + 1;
            }
            lastspace = current;
        } else if gap(current, laststart) >= linelength && docut && laststart >= lastspace {
            out.extend_from_slice(&text[laststart..current]);
            out.extend_from_slice(brk);
            laststart = current;
            lastspace = current;
        } else if gap(current, laststart) >= linelength && laststart < lastspace {
            out.extend_from_slice(&text[laststart..lastspace]);
            out.extend_from_slice(brk);
            lastspace += 1;
            laststart = lastspace;
        }
        current += 1;
    }
    if laststart != current {
        out.extend_from_slice(&text[laststart.min(current)..current]);
    }
    string_value(req, &out)
}

/// `explode(string $separator, string $string, int $limit =
/// PHP_INT_MAX): array`, `php_explode` and
/// `php_explode_negative_limit`: the pieces between the separators, at
/// most `$limit` of them with the last holding the rest, all but the
/// last `-$limit` for a negative limit, and the whole string for a
/// limit of zero or one.
///
/// ```php
/// var_dump(explode(",", "a,b,,c"), explode(",", "a,b,c", 2), explode(",", "a,b,c", -1), explode(",", "a,b,c", 0), explode(",", ""), explode(",", "", -1), explode(",", "abc", -1), explode("::", "a::b"));
/// try { explode("", "a"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
///   [2]=>
///   string(0) ""
///   [3]=>
///   string(1) "c"
/// }
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(3) "b,c"
/// }
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
/// }
/// array(1) {
///   [0]=>
///   string(5) "a,b,c"
/// }
/// array(1) {
///   [0]=>
///   string(0) ""
/// }
/// array(0) {
/// }
/// array(0) {
/// }
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
/// }
/// explode(): Argument #1 ($separator) must not be empty
/// ```
fn explode(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 3)?;
    let delim = string_argument(req, 1)?;
    let string = string_argument(req, 2)?;
    let limit = optional_long(req, 3, i64::MAX)?;
    if delim.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    let mut array = PhpArray::new(req)?;
    let (d, s) = (delim.as_bytes(), string.as_bytes());
    if s.is_empty() {
        if limit >= 0 {
            let empty = string_value(req, b"")?;
            array.append(req, empty)?;
        }
        return Ok(Value::array(array));
    }
    let mut starts = vec![0usize];
    let mut at = 0;
    while let Some(found) = memnstr(s, d, at) {
        at = found + d.len();
        starts.push(at);
        if limit > 1 && long(starts.len()) >= limit {
            break;
        }
    }
    // `starts[i]` opens piece `i`; the piece ends before the separator
    // that opens the next, or at the end.
    let piece = |i: usize| -> &[u8] {
        let from = starts[i];
        let to = starts.get(i + 1).map_or(s.len(), |next| next - d.len());
        &s[from..to]
    };
    let pieces = starts.len();
    let take = if limit > 1 {
        pieces
    } else if limit < 0 {
        if pieces == 1 {
            0
        } else {
            usize::try_from(long(pieces) + limit).unwrap_or(0)
        }
    } else {
        1
    };
    if limit == 0 || limit == 1 {
        let whole = same(req, string);
        array.append(req, whole)?;
        return Ok(Value::array(array));
    }
    for i in 0..take {
        let value = string_value(req, piece(i))?;
        array.append(req, value)?;
    }
    Ok(Value::array(array))
}

/// `implode(array|string $separator, ?array $array = null): string`,
/// `php_implode`: the elements as strings joined by the separator, an
/// int printed as `zend_print_long_to_buf` prints it and anything else
/// through `zval_get_string`. Called with one argument, that argument is
/// the array and the separator empty; the legacy order, array first,
/// is gone in 8, and its shapes are php's two `TypeError`s.
///
/// ```php
/// var_dump(implode(",", [1, "b", 2.5, true, null]), implode(["a", "b"]), join("-", []), join("-", ["x"]), implode(", ", ["k" => "v", 3 => -7]));
/// try { implode(",", "x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { implode("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { join([1], [2]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(implode(",", [[1]]));
/// ```
/// ```expect
/// string(10) "1,b,2.5,1,"
/// string(2) "ab"
/// string(0) ""
/// string(1) "x"
/// string(5) "v, -7"
/// implode(): Argument #2 ($array) must be of type ?array, string given
/// implode(): If argument #1 ($separator) is of type string, argument #2 ($array) must be of type array, null given
/// join(): Argument #1 ($separator) must be of type string, array given
///
/// Warning: Array to string conversion in %s on line 6
/// string(5) "Array"
/// ```
fn implode(req: &mut Request) -> Result<Value, Thrown> {
    imploded(req, "implode")
}

/// `join(...)`, `implode`'s alias.
///
/// ```php
/// var_dump(join(",", ["a", 1, null]), join(["x", "y"]));
/// try { join("-"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "a,1,"
/// string(2) "xy"
/// join(): If argument #1 ($separator) is of type string, argument #2 ($array) must be of type array, null given
/// ```
fn join(req: &mut Request) -> Result<Value, Thrown> {
    imploded(req, "join")
}

/// `PHP_FUNCTION(implode)` under the name it was called by, which
/// php's `get_active_function_name()` puts in the first `TypeError`.
fn imploded(req: &mut Request, name: &str) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let first = array_or_string(req, 1, false)?;
    let pieces = nullable_array(req, 2)?;
    let (glue, pieces) = match (first, pieces) {
        (Some(ArrayOr::Array(array)), None) => (PhpString::new(req, b"")?, array),
        (_, None) => {
            return Err(throw(
                req,
                "TypeError",
                &format!(
                    "{name}(): If argument #1 ($separator) is of type string, argument #2 ($array) must be of type array, null given"
                ),
            ));
        }
        (Some(ArrayOr::Other(glue)), Some(pieces)) => (glue, pieces),
        (_, Some(_)) => {
            return Err(php_ext::raising::arg_type_error(
                req,
                1,
                "be of type string, array given",
            ));
        }
    };
    let joined = implode_array(req, glue.as_bytes(), pieces)?;
    Ok(joined)
}

/// `php_implode(glue, pieces)`.
fn implode_array(req: &mut Request, glue: &[u8], pieces: PhpArray) -> Result<Value, Thrown> {
    let elements: Vec<Value> = pieces.iter().map(|(_, value)| value).collect();
    if elements.len() == 1
        && let Some(s) = elements[0].deref().as_php_string()
    {
        return Ok(same(req, *s));
    }
    let mut out = Vec::new();
    for (i, element) in elements.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(glue);
        }
        let element = element.deref();
        if let Some(s) = element.as_str_bytes() {
            out.extend_from_slice(s);
        } else if let Some(n) = element.as_int() {
            out.extend_from_slice(n.to_string().as_bytes());
        } else {
            let s = to_string(req, &element)?;
            out.extend_from_slice(s.as_bytes());
        }
    }
    string_value(req, &out)
}

/// `strtok(string $string, ?string $token = null): string|false`, over
/// `BG(strtok_string)`: with two arguments the string is taken and the
/// first token answered; with one, the argument is the token set and
/// the next token of the string taken before. A token is the span after
/// the leading bytes of the set, up to the next byte of it; `false` is
/// the end, which gives the string up.
///
/// ```php
/// $t = strtok("  a b  c ", " "); while ($t !== false) { var_dump($t); $t = strtok(" "); }
/// var_dump(strtok("a,b;c", ",;"), strtok(";"), strtok(","), strtok(","), strtok("x", "x"), strtok("abc", ""), strtok(""), strtok("x"));
/// var_dump(strtok("y"));
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "b"
/// string(1) "c"
/// string(1) "a"
/// string(1) "b"
/// string(1) "c"
/// bool(false)
/// bool(false)
/// string(3) "abc"
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn strtok(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let string = string_argument(req, 1)?;
    let token = nullable_string(req, 2)?;
    let tok: Vec<u8> = match token {
        None => string.as_bytes().to_vec(),
        Some(token) => {
            let state = &mut req.state::<crate::State>().string;
            state.strtok = Some((string.as_bytes().to_vec(), 0));
            token.as_bytes().to_vec()
        }
    };
    let Some((subject, last)) = req.state::<crate::State>().string.strtok.take() else {
        docref(
            req,
            Level::Warning,
            "Both arguments must be provided when starting tokenization",
        );
        return Ok(Value::bool(false));
    };
    let end = subject.len();
    if last >= end {
        // Past the end: `false`, and the string kept, as php keeps it.
        req.state::<crate::State>().string.strtok = Some((subject, last));
        return Ok(Value::bool(false));
    }
    let mut table = [false; 256];
    for c in &tok {
        table[usize::from(*c)] = true;
    }
    let mut p = last;
    while table[usize::from(subject[p])] {
        p += 1;
        if p >= end {
            return Ok(Value::bool(false));
        }
    }
    let skipped = p - last;
    p += 1;
    while p < end && !table[usize::from(subject[p])] {
        p += 1;
    }
    let token = string_value(req, &subject[last + skipped..p])?;
    req.state::<crate::State>().string.strtok = Some((subject, p + 1));
    Ok(token)
}

/// `str_increment(string $string): string`: the string's alphanumeric
/// successor, the carry running left through `z`, `Z` and `9` and a
/// digit, or the first byte's kind, prepended when it runs off the
/// start.
///
/// ```php
/// var_dump(str_increment("a"), str_increment("Az"), str_increment("zz"), str_increment("a9"), str_increment("Zz"), str_increment("9"));
/// foreach (["", "!", "a b"] as $s) { try { str_increment($s); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } }
/// ```
/// ```expect
/// string(1) "b"
/// string(2) "Ba"
/// string(3) "aaa"
/// string(2) "b0"
/// string(3) "AAa"
/// string(2) "10"
/// str_increment(): Argument #1 ($string) must not be empty
/// str_increment(): Argument #1 ($string) must be composed only of alphanumeric ASCII characters
/// str_increment(): Argument #1 ($string) must be composed only of alphanumeric ASCII characters
/// ```
fn str_increment(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    let mut bytes = alphanumeric_argument(req, string)?;
    let mut carry = true;
    for c in bytes.iter_mut().rev() {
        match *c {
            b'z' | b'Z' => *c -= 25,
            b'9' => *c = b'0',
            _ => {
                *c += 1;
                carry = false;
                break;
            }
        }
    }
    if carry {
        let first = if bytes[0] == b'0' { b'1' } else { bytes[0] };
        bytes.insert(0, first);
    }
    string_value(req, &bytes)
}

/// `str_decrement(string $string): string`: the predecessor, the borrow
/// running left through `a`, `A` and `0`, and a leading `0` or a
/// borrow off the start dropped; `"0"` and a string starting with it
/// are out of range.
///
/// ```php
/// var_dump(str_decrement("b"), str_decrement("Ba"), str_decrement("aa"), str_decrement("a0"), str_decrement("10"), str_decrement("Aa"));
/// foreach (["", "a-b", "0", "a", "0ab"] as $s) { try { var_dump(str_decrement($s)); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } }
/// ```
/// ```expect
/// string(1) "a"
/// string(2) "Az"
/// string(1) "z"
/// string(1) "9"
/// string(1) "9"
/// string(1) "z"
/// str_decrement(): Argument #1 ($string) must not be empty
/// str_decrement(): Argument #1 ($string) must be composed only of alphanumeric ASCII characters
/// str_decrement(): Argument #1 ($string) "0" is out of decrement range
/// str_decrement(): Argument #1 ($string) "a" is out of decrement range
/// str_decrement(): Argument #1 ($string) "0ab" is out of decrement range
/// ```
fn str_decrement(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    let mut bytes = alphanumeric_argument(req, string)?;
    // `zend_argument_value_error(1, "\"%s\" is out of decrement
    // range", ...)`: a sentence without the `must` the face writes.
    let out_of_range = |req: &mut Request| {
        let text = String::from_utf8_lossy(string.as_bytes()).into_owned();
        Err(throw(
            req,
            "ValueError",
            &format!("str_decrement(): Argument #1 ($string) \"{text}\" is out of decrement range"),
        ))
    };
    if bytes[0] == b'0' {
        return out_of_range(req).map(|()| Value::null());
    }
    let mut carry = true;
    for c in bytes.iter_mut().rev() {
        match *c {
            b'a' | b'A' => *c += 25,
            b'0' => *c = b'9',
            _ => {
                *c -= 1;
                carry = false;
                break;
            }
        }
    }
    if carry || (bytes[0] == b'0' && bytes.len() > 1) {
        if bytes.len() == 1 {
            return out_of_range(req).map(|()| Value::null());
        }
        bytes.remove(0);
    }
    string_value(req, &bytes)
}

/// The shared refusals of `str_increment` and `str_decrement`: the
/// empty string and a byte that is no ASCII letter or digit.
fn alphanumeric_argument(req: &mut Request, string: PhpString) -> Result<Vec<u8>, Thrown> {
    if string.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if !string.as_bytes().iter().all(u8::is_ascii_alphanumeric) {
        return Err(arg_value_error(
            req,
            1,
            "be composed only of alphanumeric ASCII characters",
        ));
    }
    Ok(string.as_bytes().to_vec())
}

/// `chunk_split(string $string, int $length = 76, string $separator =
/// "\r\n"): string`, `php_chunk_split`: the separator after every
/// `$length` bytes and after the rest.
///
/// ```php
/// var_dump(chunk_split("abcdefg", 3, "|"), chunk_split("abc", 5, "|"), chunk_split("", 2, "|"), chunk_split("abcd", 2), chunk_split("ab", 1, ""));
/// try { chunk_split("a", 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(10) "abc|def|g|"
/// string(4) "abc|"
/// string(1) "|"
/// string(8) "ab
/// cd
/// "
/// string(2) "ab"
/// chunk_split(): Argument #2 ($length) must be greater than 0
/// ```
fn chunk_split(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 3)?;
    let string = string_argument(req, 1)?;
    let chunklen = optional_long(req, 2, 76)?;
    let end = optional_string(req, 3)?;
    let sep: &[u8] = end.as_ref().map_or(b"\r\n", |e| e.as_bytes());
    if chunklen <= 0 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    let bytes = string.as_bytes();
    let chunk = usize::try_from(chunklen).unwrap_or(usize::MAX);
    if chunk > bytes.len() {
        let mut out = bytes.to_vec();
        out.extend_from_slice(sep);
        return string_value(req, &out);
    }
    if bytes.is_empty() {
        return string_value(req, b"");
    }
    let mut out = Vec::with_capacity(bytes.len() + (bytes.len() / chunk + 1) * sep.len());
    for piece in bytes.chunks(chunk) {
        out.extend_from_slice(piece);
        out.extend_from_slice(sep);
    }
    string_value(req, &out)
}

/// `quotemeta(string $string): string`: a backslash before each of
/// `. \ + * ? [ ^ ] $ ( )`.
///
/// ```php
/// var_dump(quotemeta("1+1=2?"), quotemeta("[a](b)^c$.\\*"), quotemeta(""));
/// ```
/// ```expect
/// string(8) "1\+1=2\?"
/// string(21) "\[a\]\(b\)\^c\$\.\\\*"
/// string(0) ""
/// ```
fn quotemeta(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    let mut out = Vec::with_capacity(string.len() * 2);
    for &c in string.as_bytes() {
        if matches!(
            c,
            b'.' | b'\\' | b'+' | b'*' | b'?' | b'[' | b'^' | b']' | b'$' | b'(' | b')'
        ) {
            out.push(b'\\');
        }
        out.push(c);
    }
    string_value(req, &out)
}

/// `ucfirst(string $string): string`: the first byte through
/// `zend_toupper_ascii`, the string itself where that changes nothing.
///
/// ```php
/// var_dump(ucfirst("hello"), ucfirst("Hello"), ucfirst(""), bin2hex(ucfirst("\xe9t\xe9")), ucfirst("1a"));
/// ```
/// ```expect
/// string(5) "Hello"
/// string(5) "Hello"
/// string(0) ""
/// string(6) "e974e9"
/// string(2) "1a"
/// ```
fn ucfirst(req: &mut Request) -> Result<Value, Thrown> {
    first_cased(req, u8::to_ascii_uppercase)
}

/// `lcfirst(string $string): string`.
///
/// ```php
/// var_dump(lcfirst("ABC"), lcfirst("abc"), lcfirst(""), lcfirst("1A"));
/// ```
/// ```expect
/// string(3) "aBC"
/// string(3) "abc"
/// string(0) ""
/// string(2) "1A"
/// ```
fn lcfirst(req: &mut Request) -> Result<Value, Thrown> {
    first_cased(req, u8::to_ascii_lowercase)
}

/// `php_ucfirst` and `php_lcfirst`.
fn first_cased(req: &mut Request, map: fn(&u8) -> u8) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    let bytes = string.as_bytes();
    let Some(first) = bytes.first() else {
        return string_value(req, b"");
    };
    if map(first) == *first {
        return Ok(same(req, string));
    }
    let mut out = bytes.to_vec();
    out[0] = map(first);
    string_value(req, &out)
}

/// `ucwords(string $string, string $separators = " \t\r\n\f\v"):
/// string`: the first byte and each byte after one of the separators,
/// a `php_charmask` set, uppercased.
///
/// ```php
/// var_dump(ucwords("hello world-foo bar"), ucwords("hello world-foo", "-"), bin2hex(ucwords("a\tb\nc")), ucwords(""), ucwords("abc", "a..c"));
/// ```
/// ```expect
/// string(19) "Hello World-foo Bar"
/// string(15) "Hello world-Foo"
/// string(10) "4109420a43"
/// string(0) ""
/// string(3) "AbC"
/// ```
fn ucwords(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let string = string_argument(req, 1)?;
    let delims = optional_string(req, 2)?;
    if string.is_empty() {
        return string_value(req, b"");
    }
    let set: &[u8] = delims.as_ref().map_or(b" \t\r\n\x0c\x0b", |d| d.as_bytes());
    let mask = charmask(req, set);
    let mut out = string.as_bytes().to_vec();
    out[0] = out[0].to_ascii_uppercase();
    for i in 1..out.len() {
        if mask[usize::from(out[i - 1])] {
            out[i] = out[i].to_ascii_uppercase();
        }
    }
    string_value(req, &out)
}

/// `strrev(string $string): string`: the bytes in reverse.
///
/// ```php
/// var_dump(strrev("abc"), strrev(""), strrev("a\x00b") === "b\x00a");
/// ```
/// ```expect
/// string(3) "cba"
/// string(0) ""
/// bool(true)
/// ```
fn strrev(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    let out: Vec<u8> = string.as_bytes().iter().rev().copied().collect();
    string_value(req, &out)
}

/// `similar_text(string $string1, string $string2, &$percent = null):
/// int`, `php_similar_char`: the longest common run, first found
/// wins, plus the same count to its left and to its right, and the
/// percentage `sim * 200 / (len1 + len2)` written through the third
/// argument when the call gave one.
///
/// ```php
/// var_dump(similar_text("World", "Word"), similar_text("Hello", "World", $p), $p, similar_text("", "", $q), $q, similar_text("bafoobar", "barfoo", $r), $r);
/// ```
/// ```expect
/// int(4)
/// int(1)
/// float(20)
/// int(0)
/// float(0)
/// int(5)
/// float(71.42857142857143)
/// ```
fn similar_text(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 3)?;
    let t1 = string_argument(req, 1)?;
    let t2 = string_argument(req, 2)?;
    let percent = req.argument_count() >= 3;
    let (a, b) = (t1.as_bytes(), t2.as_bytes());
    if a.len() + b.len() == 0 {
        if percent {
            crate::zpp::assign_ref(req, 3, Value::float(0.0));
        }
        return Ok(Value::int(0));
    }
    let sim = similar_char(a, b);
    if percent {
        // `sim * 200.0 / (len1 + len2)`, both sides as C's doubles.
        let as_double = |n: usize| f64::from(u32::try_from(n).unwrap_or(u32::MAX));
        let p = as_double(sim) * 200.0 / as_double(a.len() + b.len());
        crate::zpp::assign_ref(req, 3, Value::float(p));
    }
    Ok(long_of(sim))
}

/// `php_similar_str` and `php_similar_char`.
fn similar_char(a: &[u8], b: &[u8]) -> usize {
    let (mut max, mut count, mut pos1, mut pos2) = (0, 0, 0, 0);
    for p in 0..a.len() {
        for q in 0..b.len() {
            let l = a[p..]
                .iter()
                .zip(&b[q..])
                .take_while(|(x, y)| x == y)
                .count();
            if l > max {
                max = l;
                count += 1;
                pos1 = p;
                pos2 = q;
            }
        }
    }
    let mut sum = max;
    if sum > 0 {
        if pos1 > 0 && pos2 > 0 && count > 1 {
            sum += similar_char(&a[..pos1], &b[..pos2]);
        }
        if pos1 + max < a.len() && pos2 + max < b.len() {
            sum += similar_char(&a[pos1 + max..], &b[pos2 + max..]);
        }
    }
    sum
}

/// `hebrev(string $string, int $max_chars_per_line = 0): string`, php's
/// visual-order conversion of ISO-8859-8 Hebrew: the Hebrew and English
/// blocks laid out right to left with the bracket pairs mirrored, then
/// the lines broken at `$max_chars_per_line` from the end. A line-for-
/// line copy of php's two loops.
///
/// ```php
/// var_dump(hebrev("abc def"), bin2hex(hebrev("\xe0\xe1\xe2 abc (x)")), bin2hex(hebrev("\xe0\xe1 \xe2\xe3 \xe4\xe5", 3)), hebrev(""), hebrev("a\nb"), bin2hex(hebrev("ab \xe0\xe1, cd\n\xe2\xe3 [x]", 5)));
/// ```
/// ```expect
/// string(7) "abc def"
/// string(22) "2861626320287820e2e1e0"
/// string(16) "e1e00ae3e20ae5e4"
/// string(0) ""
/// string(3) "a
/// b"
/// string(32) "61620a2ce1e00a63640ae3e20a5b785d"
/// ```
fn hebrev(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let string = string_argument(req, 1)?;
    let max_chars = optional_long(req, 2, 0)?;
    let s = string.as_bytes();
    if s.is_empty() {
        return string_value(req, b"");
    }
    let out = hebrev_bytes(s, max_chars);
    string_value(req, &out)
}

/// php's two loops of `hebrev`, a line-for-line copy.
#[allow(clippy::too_many_lines)]
fn hebrev_bytes(s: &[u8], max_chars: i64) -> Vec<u8> {
    let isheb = |c: u8| (224..=250).contains(&c);
    let isblank = |c: u8| c == b' ' || c == b'\t';
    let isnewline = |c: u8| c == b'\n' || c == b'\r';
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let len = s.len();
    let mut heb = vec![0u8; len];
    // The next slot from the end `heb` is filled into, `target + 1`.
    let mut target = len;
    let mut tmp = 0usize;
    let (mut block_start, mut block_end) = (0usize, 0usize);
    let mut hebrew = isheb(s[0]);
    loop {
        if hebrew {
            while (isheb(at(tmp + 1))
                || isblank(at(tmp + 1))
                || at(tmp + 1).is_ascii_punctuation()
                || at(tmp + 1) == b'\n')
                && block_end < len - 1
            {
                tmp += 1;
                block_end += 1;
            }
            for i in block_start + 1..=block_end + 1 {
                let c = match s[i - 1] {
                    b'(' => b')',
                    b')' => b'(',
                    b'[' => b']',
                    b']' => b'[',
                    b'{' => b'}',
                    b'}' => b'{',
                    b'<' => b'>',
                    b'>' => b'<',
                    b'\\' => b'/',
                    b'/' => b'\\',
                    other => other,
                };
                if target > 0 {
                    target -= 1;
                    heb[target] = c;
                }
            }
            hebrew = false;
        } else {
            while !isheb(at(tmp + 1)) && at(tmp + 1) != b'\n' && block_end < len - 1 {
                tmp += 1;
                block_end += 1;
            }
            while (isblank(at(tmp)) || at(tmp).is_ascii_punctuation())
                && at(tmp) != b'/'
                && at(tmp) != b'-'
                && block_end > block_start
            {
                tmp -= 1;
                block_end -= 1;
            }
            let mut i = block_end + 1;
            while i > block_start {
                if target > 0 {
                    target -= 1;
                    heb[target] = s[i - 1];
                }
                i -= 1;
            }
            hebrew = true;
        }
        block_start = block_end + 1;
        if block_end >= len - 1 {
            break;
        }
    }
    let mut out = Vec::with_capacity(len);
    let (mut begin, mut end) = (len - 1, len - 1);
    loop {
        let mut char_count = 0i64;
        while (max_chars == 0 || (max_chars > 0 && char_count < max_chars)) && begin > 0 {
            char_count += 1;
            begin -= 1;
            if isnewline(heb[begin]) {
                while begin > 0 && isnewline(heb[begin - 1]) {
                    begin -= 1;
                    char_count += 1;
                }
                break;
            }
        }
        if max_chars >= 0 && char_count == max_chars {
            let (mut new_count, mut new_begin) = (char_count, begin);
            while new_count > 0 {
                if isblank(heb[new_begin]) || isnewline(heb[new_begin]) {
                    break;
                }
                new_begin += 1;
                new_count -= 1;
            }
            if new_count > 0 {
                begin = new_begin;
            }
        }
        let orig_begin = begin;
        if isblank(heb[begin]) {
            heb[begin] = b'\n';
        }
        while begin <= end && isnewline(heb[begin]) {
            begin += 1;
        }
        for &c in heb.get(begin..=end).unwrap_or(&[]) {
            out.push(c);
        }
        let mut i = orig_begin;
        while i <= end && isnewline(heb[i]) {
            out.push(heb[i]);
            i += 1;
        }
        begin = orig_begin;
        if begin == 0 {
            break;
        }
        begin -= 1;
        end = begin;
    }
    out
}

/// `nl2br(string $string, bool $use_xhtml = true): string`: `<br />`,
/// or `<br>`, before each `\r\n`, `\n\r`, `\n` and `\r`.
///
/// ```php
/// var_dump(strtr(nl2br("a\nb\r\nc\n\rd\re"), ["\r" => '\r', "\n" => '\n']), nl2br("a\nb", false), nl2br("none"), bin2hex(nl2br("\n\n")));
/// ```
/// ```expect
/// string(41) "a<br />\nb<br />\r\nc<br />\n\rd<br />\re"
/// string(7) "a<br>
/// b"
/// string(4) "none"
/// string(28) "3c6272202f3e0a3c6272202f3e0a"
/// ```
fn nl2br(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let string = string_argument(req, 1)?;
    let xhtml = optional_bool(req, 2, true)?;
    let s = string.as_bytes();
    if !s.iter().any(|c| *c == b'\r' || *c == b'\n') {
        return Ok(same(req, string));
    }
    let tag: &[u8] = if xhtml { b"<br />" } else { b"<br>" };
    let mut out = Vec::with_capacity(s.len() * 2);
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\r' || c == b'\n' {
            out.extend_from_slice(tag);
            let next = s.get(i + 1).copied().unwrap_or(0);
            if (c == b'\r' && next == b'\n') || (c == b'\n' && next == b'\r') {
                out.push(c);
                i += 1;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    string_value(req, &out)
}

/// `count_chars(string $string, int $mode = 0): array|string`: the
/// count of every byte value, of those present, of those absent, or the
/// bytes present or absent as a string.
///
/// ```php
/// var_dump(count_chars("abca", 1), count_chars("abca", 3), strlen(count_chars("abc", 4)), count(count_chars("")), count(count_chars("a", 2)));
/// try { count_chars("a", 5); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(3) {
///   [97]=>
///   int(2)
///   [98]=>
///   int(1)
///   [99]=>
///   int(1)
/// }
/// string(3) "abc"
/// int(253)
/// int(256)
/// int(255)
/// count_chars(): Argument #2 ($mode) must be between 0 and 4 (inclusive)
/// ```
fn count_chars(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let input = string_argument(req, 1)?;
    let mode = optional_long(req, 2, 0)?;
    if !(0..=4).contains(&mode) {
        return Err(arg_value_error(req, 2, "be between 0 and 4 (inclusive)"));
    }
    let mut chars = [0usize; 256];
    for &c in input.as_bytes() {
        chars[usize::from(c)] += 1;
    }
    if mode >= 3 {
        let bytes: Vec<u8> = (0..=255u8)
            .filter(|c| (chars[usize::from(*c)] != 0) == (mode == 3))
            .collect();
        return string_value(req, &bytes);
    }
    let mut array = PhpArray::new(req)?;
    for (inx, count) in chars.iter().enumerate() {
        let keep = match mode {
            0 => true,
            1 => *count != 0,
            _ => *count == 0,
        };
        if keep {
            array.set(req, Key::Int(long(inx)), long_of(*count))?;
        }
    }
    Ok(Value::array(array))
}

/// `substr_count(string $haystack, string $needle, int $offset = 0,
/// ?int $length = null): int`: the non-overlapping occurrences in the
/// window, which must lie inside the haystack.
///
/// ```php
/// var_dump(substr_count("hello hello", "ll"), substr_count("aaa", "aa"), substr_count("hello", "l", 3), substr_count("hello", "l", -2), substr_count("hello", "l", 1, 2), substr_count("abc", "c", 0, -1));
/// foreach ([["a", ""], ["a", "a", 5], ["a", "a", 0, 5]] as $a) { try { call_user_func_array("substr_count", $a); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } }
/// ```
/// ```expect
/// int(2)
/// int(1)
/// int(1)
/// int(1)
/// int(1)
/// int(0)
/// substr_count(): Argument #2 ($needle) must not be empty
/// substr_count(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// substr_count(): Argument #4 ($length) must be contained in argument #1 ($haystack)
/// ```
fn substr_count(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 4)?;
    let haystack = string_argument(req, 1)?;
    let needle = string_argument(req, 2)?;
    let mut offset = optional_long(req, 3, 0)?;
    let length = nullable_long(req, 4)?;
    if needle.is_empty() {
        return Err(arg_value_error(req, 2, "not be empty"));
    }
    let mut len = long(haystack.len());
    if offset != 0 {
        if offset < 0 {
            offset += len;
        }
        if offset < 0 || offset > len {
            return Err(arg_value_error(
                req,
                3,
                "be contained in argument #1 ($haystack)",
            ));
        }
        len -= offset;
    }
    let length = match length {
        Some(mut length) => {
            if length < 0 {
                length += len;
            }
            if length < 0 || length > len {
                return Err(arg_value_error(
                    req,
                    4,
                    "be contained in argument #1 ($haystack)",
                ));
            }
            length
        }
        None => len,
    };
    let from = usize::try_from(offset).unwrap_or(0);
    let window = &haystack.as_bytes()[from..from + usize::try_from(length).unwrap_or(0)];
    let n = needle.as_bytes();
    let mut count = 0usize;
    let mut at = 0;
    while let Some(found) = memnstr(window, n, at) {
        count += 1;
        at = found + n.len();
    }
    Ok(long_of(count))
}

/// `str_rot13(string $string): string`: each ASCII letter moved
/// thirteen places round its case.
///
/// ```php
/// var_dump(str_rot13("Hello, World! xyz"), str_rot13(""));
/// ```
/// ```expect
/// string(17) "Uryyb, Jbeyq! klm"
/// string(0) ""
/// ```
fn str_rot13(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    let out: Vec<u8> = string
        .as_bytes()
        .iter()
        .map(|&c| match c {
            b'a'..=b'z' => b'a' + (c - b'a' + 13) % 26,
            b'A'..=b'Z' => b'A' + (c - b'A' + 13) % 26,
            _ => c,
        })
        .collect();
    string_value(req, &out)
}

/// `str_shuffle(string $string): string`, `php_binary_string_shuffle`
/// (`Z/ext/standard/string.c:5967`): the Fisher-Yates walk from the end,
/// each swap drawn from the request's default `Random\Engine\Mt19937`,
/// which `mt_srand` seeds.
///
/// ```php
/// $s = str_shuffle("aabbcc"); $c = count_chars($s, 1); var_dump(strlen($s), $c[97], $c[98], $c[99], str_shuffle(""), str_shuffle("x"));
/// mt_srand(42); var_dump(str_shuffle("abcdefghij"), str_shuffle("0123456789"));
/// ```
/// ```expect
/// int(6)
/// int(2)
/// int(2)
/// int(2)
/// string(0) ""
/// string(1) "x"
/// string(10) "bdjhgaiefc"
/// string(10) "9825107643"
/// ```
fn str_shuffle(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    let mut bytes = string.as_bytes().to_vec();
    Rng::default_engine().shuffle(req, &mut bytes)?;
    string_value(req, &bytes)
}

/// `str_word_count(string $string, int $format = 0, ?string $characters
/// = null): array|int`: the words, runs of letters, `'` and `-` and the
/// extra `$characters`, a word not starting with `'` or `-` and the
/// string's trailing `-` not counted; as a count, a list, or keyed by
/// offset.
///
/// ```php
/// $s = "Hello fri3nd, you're looking good today!-";
/// var_dump(str_word_count($s), str_word_count($s, 1), str_word_count($s, 2, "3"), str_word_count(""), str_word_count("", 1), str_word_count("-a- 'b'", 1));
/// try { str_word_count("a", 3); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(7)
/// array(7) {
///   [0]=>
///   string(5) "Hello"
///   [1]=>
///   string(3) "fri"
///   [2]=>
///   string(2) "nd"
///   [3]=>
///   string(6) "you're"
///   [4]=>
///   string(7) "looking"
///   [5]=>
///   string(4) "good"
///   [6]=>
///   string(5) "today"
/// }
/// array(6) {
///   [0]=>
///   string(5) "Hello"
///   [6]=>
///   string(6) "fri3nd"
///   [14]=>
///   string(6) "you're"
///   [21]=>
///   string(7) "looking"
///   [29]=>
///   string(4) "good"
///   [34]=>
///   string(5) "today"
/// }
/// int(0)
/// array(0) {
/// }
/// array(2) {
///   [0]=>
///   string(2) "a-"
///   [1]=>
///   string(3) "'b'"
/// }
/// str_word_count(): Argument #2 ($format) must be a valid format value
/// ```
fn str_word_count(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 3)?;
    let string = string_argument(req, 1)?;
    let format = optional_long(req, 2, 0)?;
    let list = nullable_string(req, 3)?;
    let s = string.as_bytes();
    let mut array = match format {
        1 | 2 => {
            let array = PhpArray::new(req)?;
            if s.is_empty() {
                return Ok(Value::array(array));
            }
            Some(array)
        }
        0 => {
            if s.is_empty() {
                return Ok(Value::int(0));
            }
            None
        }
        _ => return Err(arg_value_error(req, 2, "be a valid format value")),
    };
    let mask = list.as_ref().map(|list| charmask(req, list.as_bytes()));
    let extra = |c: u8| mask.as_ref().is_some_and(|m| m[usize::from(c)]);
    let (mut p, mut e) = (0usize, s.len());
    if (s[0] == b'\'' && !extra(b'\'')) || (s[0] == b'-' && !extra(b'-')) {
        p += 1;
    }
    if s[e - 1] == b'-' && !extra(b'-') {
        e -= 1;
    }
    let mut count = 0usize;
    while p < e {
        let start = p;
        while p < e && (s[p].is_ascii_alphabetic() || extra(s[p]) || s[p] == b'\'' || s[p] == b'-')
        {
            p += 1;
        }
        if p > start {
            match (&mut array, format) {
                (Some(array), 1) => {
                    let word = string_value(req, &s[start..p])?;
                    array.append(req, word)?;
                }
                (Some(array), _) => {
                    let word = string_value(req, &s[start..p])?;
                    array.set(req, Key::Int(long(start)), word)?;
                }
                (None, _) => count += 1,
            }
        }
        p += 1;
    }
    Ok(match array {
        Some(array) => Value::array(array),
        None => long_of(count),
    })
}

/// `str_split(string $string, int $length = 1): array`: the pieces of
/// `$length` bytes and the shorter rest; the empty string is the empty
/// array.
///
/// ```php
/// var_dump(str_split("abcde", 2), str_split(""), str_split("ab", 5));
/// try { str_split("a", 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   string(2) "ab"
///   [1]=>
///   string(2) "cd"
///   [2]=>
///   string(1) "e"
/// }
/// array(0) {
/// }
/// array(1) {
///   [0]=>
///   string(2) "ab"
/// }
/// str_split(): Argument #2 ($length) must be greater than 0
/// ```
fn str_split(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let string = string_argument(req, 1)?;
    let length = optional_long(req, 2, 1)?;
    if length <= 0 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    let mut array = PhpArray::new(req)?;
    let chunk = usize::try_from(length).unwrap_or(usize::MAX);
    for piece in string.as_bytes().chunks(chunk) {
        let value = string_value(req, piece)?;
        array.append(req, value)?;
    }
    Ok(Value::array(array))
}

/// `strpbrk(string $string, string $characters): string|false`: the
/// string from the first byte that is one of `$characters`.
///
/// ```php
/// var_dump(strpbrk("This is a test", "st"), strpbrk("abc", "x"));
/// try { strpbrk("a", ""); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(11) "s is a test"
/// bool(false)
/// strpbrk(): Argument #2 ($characters) must be a non-empty string
/// ```
fn strpbrk(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let haystack = string_argument(req, 1)?;
    let list = string_argument(req, 2)?;
    if list.is_empty() {
        return Err(arg_value_error(req, 2, "be a non-empty string"));
    }
    let set = list.as_bytes();
    let bytes = haystack.as_bytes();
    match bytes.iter().position(|c| set.contains(c)) {
        Some(at) => string_value(req, &bytes[at..]),
        None => Ok(Value::bool(false)),
    }
}

/// `substr_compare(string $haystack, string $needle, int $offset, ?int
/// $length = null, bool $case_insensitive = false): int`:
/// `zend_binary_strncmp`, or its case-folding twin, of the haystack
/// from `$offset` against the needle over `$length` bytes, or the
/// longer of the two without one; php's answer is the byte difference
/// or the sign of the length difference.
///
/// ```php
/// var_dump(substr_compare("abcde", "bc", 1, 2), substr_compare("abcde", "de", -2, 2), substr_compare("abcde", "bcg", 1, 2), substr_compare("abcde", "BC", 1, 2, true), substr_compare("abcde", "bc", 1, 3), substr_compare("abcde", "cd", 1, 2), substr_compare("abcde", "abc", 0, 0), substr_compare("abcde", "x", 5), substr_compare("abc", "abcdef", 0));
/// foreach ([["a", "a", 2], ["a", "a", 0, -1]] as $a) { try { call_user_func_array("substr_compare", $a); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } }
/// ```
/// ```expect
/// int(0)
/// int(0)
/// int(0)
/// int(0)
/// int(1)
/// int(-1)
/// int(0)
/// int(-1)
/// int(-1)
/// substr_compare(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// substr_compare(): Argument #4 ($length) must be greater than or equal to 0
/// ```
fn substr_compare(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 3, 5)?;
    let s1 = string_argument(req, 1)?;
    let s2 = string_argument(req, 2)?;
    let mut offset = long_argument(req, 3)?;
    let length = nullable_long(req, 4)?;
    let case_insensitive = optional_bool(req, 5, false)?;
    if let Some(len) = length
        && len <= 0
    {
        if len == 0 {
            return Ok(Value::int(0));
        }
        return Err(arg_value_error(req, 4, "be greater than or equal to 0"));
    }
    let l1 = long(s1.len());
    if offset < 0 {
        offset = (l1 + offset).max(0);
    }
    if offset > l1 {
        return Err(arg_value_error(
            req,
            3,
            "be contained in argument #1 ($haystack)",
        ));
    }
    let a = &s1.as_bytes()[usize::try_from(offset).unwrap_or(0)..];
    let b = s2.as_bytes();
    let cmp_len = match length {
        Some(len) => usize::try_from(len).unwrap_or(usize::MAX),
        None => b.len().max(a.len()),
    };
    Ok(Value::int(binary_strncmp(a, b, cmp_len, case_insensitive)))
}

/// `zend_binary_strncmp` and `zend_binary_strncasecmp_l`: the first
/// differing byte's difference within `length`, as the pinned build's
/// `memcmp` answers it, else the sign of the difference of the two
/// lengths capped at `length`.
fn binary_strncmp(left: &[u8], right: &[u8], length: usize, fold: bool) -> i64 {
    let common = left.len().min(right.len()).min(length);
    for (x, y) in left[..common].iter().zip(&right[..common]) {
        let (x, y) = if fold {
            (lower(*x), lower(*y))
        } else {
            (*x, *y)
        };
        if x != y {
            return i64::from(x) - i64::from(y);
        }
    }
    // `ZEND_THREEWAY_COMPARE` of the two lengths: a sign, where the
    // bytes' difference is `memcmp`'s own.
    match left.len().min(length).cmp(&right.len().min(length)) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `utf8_encode(string $string): string`, deprecated since 8.2: each
/// Latin-1 byte as its UTF-8 sequence.
///
/// ```php
/// var_dump(bin2hex(utf8_encode("a\xe9\xff")), bin2hex(utf8_decode("a\xc3\xa9\xe2\x82\xac\xff")));
/// ```
/// ```expect
/// Deprecated: Function utf8_encode() is deprecated since 8.2, visit the php.net documentation for various alternatives in %s on line 2
///
/// Deprecated: Function utf8_decode() is deprecated since 8.2, visit the php.net documentation for various alternatives in %s on line 2
/// string(10) "61c3a9c3bf"
/// string(8) "61e93f3f"
/// ```
fn utf8_encode(req: &mut Request) -> Result<Value, Thrown> {
    deprecated(
        req,
        "Function utf8_encode() is deprecated since 8.2, visit the php.net documentation for various alternatives",
    );
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    let mut out = Vec::with_capacity(string.len() * 2);
    for &c in string.as_bytes() {
        if c < 0x80 {
            out.push(c);
        } else {
            out.push(0xc0 | (c >> 6));
            out.push(0x80 | (c & 0x3f));
        }
    }
    string_value(req, &out)
}

/// `utf8_decode(string $string): string`, deprecated since 8.2: each
/// UTF-8 character as its Latin-1 byte, `?` for one beyond U+00FF and
/// for each ill-formed sequence, `php_next_utf8_char`'s reading.
///
/// ```php
/// var_dump(bin2hex(utf8_decode("a\xc3\xa9\xe2\x82\xac\xff\xc3")), utf8_decode(""));
/// ```
/// ```expect
/// Deprecated: Function utf8_decode() is deprecated since 8.2, visit the php.net documentation for various alternatives in %s on line 2
///
/// Deprecated: Function utf8_decode() is deprecated since 8.2, visit the php.net documentation for various alternatives in %s on line 2
/// string(10) "61e93f3f3f"
/// string(0) ""
/// ```
fn utf8_decode(req: &mut Request) -> Result<Value, Thrown> {
    deprecated(
        req,
        "Function utf8_decode() is deprecated since 8.2, visit the php.net documentation for various alternatives",
    );
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    let s = string.as_bytes();
    let mut out = Vec::with_capacity(s.len());
    let mut pos = 0;
    while pos < s.len() {
        let (c, next) = next_utf8_char(s, pos);
        pos = next;
        out.push(match c {
            Some(c) if c <= 0xff => u8::try_from(c).unwrap_or(b'?'),
            _ => b'?',
        });
    }
    string_value(req, &out)
}

/// `get_next_char(cs_utf_8, ...)` (`Z/ext/standard/html.c:89`), what
/// `php_next_utf8_char` answers: the code point at `pos` and the offset
/// after it, or `None` and the offset past the bytes UTR #36's second
/// strategy takes as the ill-formed sequence: a non-initial byte that
/// is a lead or encodes a character of its own is left for the next
/// read.
pub(super) fn next_utf8_char(s: &[u8], pos: usize) -> (Option<u32>, usize) {
    let avail = s.len() - pos;
    let lead = |c: u8| c < 0x80 || (0xc2..=0xf4).contains(&c);
    let trail = |c: u8| (0x80..=0xbf).contains(&c);
    let b = |i: usize| s.get(pos + i).copied().unwrap_or(0);
    let c = b(0);
    if c < 0x80 {
        return (Some(u32::from(c)), pos + 1);
    }
    if c < 0xc2 {
        return (None, pos + 1);
    }
    if c < 0xe0 {
        if avail < 2 {
            return (None, pos + 1);
        }
        if !trail(b(1)) {
            return (None, pos + if lead(b(1)) { 1 } else { 2 });
        }
        let cp = ((u32::from(c) & 0x1f) << 6) | (u32::from(b(1)) & 0x3f);
        if cp < 0x80 {
            return (None, pos + 2);
        }
        return (Some(cp), pos + 2);
    }
    if c < 0xf0 {
        if avail < 3 || !trail(b(1)) || !trail(b(2)) {
            let skip = if avail < 2 || lead(b(1)) {
                1
            } else if avail < 3 || lead(b(2)) {
                2
            } else {
                3
            };
            return (None, pos + skip);
        }
        let cp = ((u32::from(c) & 0x0f) << 12)
            | ((u32::from(b(1)) & 0x3f) << 6)
            | (u32::from(b(2)) & 0x3f);
        if cp < 0x800 || (0xd800..=0xdfff).contains(&cp) {
            return (None, pos + 3);
        }
        return (Some(cp), pos + 3);
    }
    if c < 0xf5 {
        if avail < 4 || !trail(b(1)) || !trail(b(2)) || !trail(b(3)) {
            let skip = if avail < 2 || lead(b(1)) {
                1
            } else if avail < 3 || lead(b(2)) {
                2
            } else if avail < 4 || lead(b(3)) {
                3
            } else {
                4
            };
            return (None, pos + skip);
        }
        let cp = ((u32::from(c) & 0x07) << 18)
            | ((u32::from(b(1)) & 0x3f) << 12)
            | ((u32::from(b(2)) & 0x3f) << 6)
            | (u32::from(b(3)) & 0x3f);
        if !(0x10000..=0x0010_ffff).contains(&cp) {
            return (None, pos + 4);
        }
        return (Some(cp), pos + 4);
    }
    (None, pos + 1)
}
