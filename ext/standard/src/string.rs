//! `Z/ext/standard/string.c`: its natives, each declared by
//! `#[php_function]` over php's own algorithm, the nine the crate root
//! registers first (`ord`, `chr`, `str_repeat`, then `trim`,
//! `str_contains`, `substr` with their frameless entries, `strtoupper`,
//! `strtolower` and `str_pad`) and the rest in the order
//! `basic_functions.stub.php` gives them. They are here and in this
//! file's children: the searches in `find`, the
//! path natives in `path`, the replacements in `replace`, the escapes in
//! `slashes`, `strip_tags`'s state machine in `strip_tags`, the locale
//! natives in `locale` and `parse_str` in `parse_str`. The C locale is
//! the only one this engine has (#640), so `isalpha` and the case maps
//! are ASCII's, as php's are under it.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

pub(super) mod find;
pub(super) mod locale;
pub(super) mod parse_str;
pub(super) mod path;
pub(super) mod replace;
pub(super) mod slashes;
pub(super) mod strip_tags;

pub use strip_tags::strip_tags_ex;

use constants::{STR_PAD_BOTH, STR_PAD_LEFT, STR_PAD_RIGHT};
use php_ext::abi::{OrFalse, Passed, Ref, Union, Untyped};
use php_ext::attr::{php_const, php_function};
use php_ext::coerce::with_string;
use php_ext::raising::{Level, Thrown, arg_value_error, argument_error, docref, throw_error};
use php_ext::request::Request;
use php_ext::values::{Charset, Owner, PhpArray, PhpString, Value};
use php_ext_random::Rng;

use crate::ffi::locale::{self as c, Class};

/// The request state `string.c` keeps: `BG(strtok_string)` and its
/// cursor, and the request's `locale_t` once `setlocale()` built one.
#[derive(Default)]
pub(super) struct State {
    strtok: Option<(Vec<u8>, usize)>,
    locale: locale::Locale,
}

/// The constants the natives of `string.c` read, in the stub's order:
/// `PHP_STR_PAD_*` (`Z/ext/standard/php_string.h`), what `str_pad`'s
/// `$pad_type` is one of, `PHP_PATHINFO_*`, the bits of `pathinfo`'s
/// `$flags`, then `CHAR_MAX`, the `LC_*` categories and the
/// `nl_langinfo` items, which are the pinned build's `<langinfo.h>`
/// values.
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
#[php_const]
pub(crate) mod constants {
    pub(crate) const STR_PAD_LEFT: i64 = 0;
    pub(crate) const STR_PAD_RIGHT: i64 = 1;
    pub(crate) const STR_PAD_BOTH: i64 = 2;
    pub(crate) const PATHINFO_DIRNAME: i64 = 1;
    pub(crate) const PATHINFO_BASENAME: i64 = 2;
    pub(crate) const PATHINFO_EXTENSION: i64 = 4;
    pub(crate) const PATHINFO_FILENAME: i64 = 8;
    pub(crate) const PATHINFO_ALL: i64 = 15;
    pub(crate) const CHAR_MAX: i64 = 127;
    pub(crate) const LC_CTYPE: i64 = 2;
    pub(crate) const LC_NUMERIC: i64 = 4;
    pub(crate) const LC_TIME: i64 = 5;
    pub(crate) const LC_COLLATE: i64 = 1;
    pub(crate) const LC_MONETARY: i64 = 3;
    pub(crate) const LC_ALL: i64 = 0;
    pub(crate) const LC_MESSAGES: i64 = 6;
    pub(crate) const ABDAY_1: i64 = 14;
    pub(crate) const ABDAY_2: i64 = 15;
    pub(crate) const ABDAY_3: i64 = 16;
    pub(crate) const ABDAY_4: i64 = 17;
    pub(crate) const ABDAY_5: i64 = 18;
    pub(crate) const ABDAY_6: i64 = 19;
    pub(crate) const ABDAY_7: i64 = 20;
    pub(crate) const DAY_1: i64 = 7;
    pub(crate) const DAY_2: i64 = 8;
    pub(crate) const DAY_3: i64 = 9;
    pub(crate) const DAY_4: i64 = 10;
    pub(crate) const DAY_5: i64 = 11;
    pub(crate) const DAY_6: i64 = 12;
    pub(crate) const DAY_7: i64 = 13;
    pub(crate) const ABMON_1: i64 = 33;
    pub(crate) const ABMON_2: i64 = 34;
    pub(crate) const ABMON_3: i64 = 35;
    pub(crate) const ABMON_4: i64 = 36;
    pub(crate) const ABMON_5: i64 = 37;
    pub(crate) const ABMON_6: i64 = 38;
    pub(crate) const ABMON_7: i64 = 39;
    pub(crate) const ABMON_8: i64 = 40;
    pub(crate) const ABMON_9: i64 = 41;
    pub(crate) const ABMON_10: i64 = 42;
    pub(crate) const ABMON_11: i64 = 43;
    pub(crate) const ABMON_12: i64 = 44;
    pub(crate) const MON_1: i64 = 21;
    pub(crate) const MON_2: i64 = 22;
    pub(crate) const MON_3: i64 = 23;
    pub(crate) const MON_4: i64 = 24;
    pub(crate) const MON_5: i64 = 25;
    pub(crate) const MON_6: i64 = 26;
    pub(crate) const MON_7: i64 = 27;
    pub(crate) const MON_8: i64 = 28;
    pub(crate) const MON_9: i64 = 29;
    pub(crate) const MON_10: i64 = 30;
    pub(crate) const MON_11: i64 = 31;
    pub(crate) const MON_12: i64 = 32;
    pub(crate) const AM_STR: i64 = 5;
    pub(crate) const PM_STR: i64 = 6;
    pub(crate) const D_T_FMT: i64 = 1;
    pub(crate) const D_FMT: i64 = 2;
    pub(crate) const T_FMT: i64 = 3;
    pub(crate) const T_FMT_AMPM: i64 = 4;
    pub(crate) const ERA: i64 = 45;
    pub(crate) const ERA_D_T_FMT: i64 = 47;
    pub(crate) const ERA_D_FMT: i64 = 46;
    pub(crate) const ERA_T_FMT: i64 = 48;
    pub(crate) const ALT_DIGITS: i64 = 49;
    pub(crate) const CRNCYSTR: i64 = 56;
    pub(crate) const RADIXCHAR: i64 = 50;
    pub(crate) const THOUSEP: i64 = 51;
    pub(crate) const YESEXPR: i64 = 52;
    pub(crate) const NOEXPR: i64 = 53;
    pub(crate) const YESSTR: i64 = 54;
    pub(crate) const NOSTR: i64 = 55;
    pub(crate) const CODESET: i64 = 0;
}

/// `ord(string $character): int`, php's `PHP_FUNCTION(ord)`
/// (`Z/ext/standard/string.c:2650`): the first byte of the string, with
/// the 8.5 deprecation for a string that is not one byte long, and the
/// terminator's zero for the empty string behind its own. A one-byte
/// string written as such never reaches here, since the compiler folds
/// it (`zend_compile_func_ord`); what does is any other argument, read
/// as `Z_PARAM_STR` reads it.
///
/// ```php
/// var_dump(ord("ab"), ord(""), ord(65), ord("a" . extension_loaded('Core')));
/// ```
/// ```expect
/// Deprecated: ord(): Providing a string that is not one byte long is deprecated. Use ord($str[0]) instead in %s on line 2
///
/// Deprecated: ord(): Providing an empty string is deprecated in %s on line 2
///
/// Deprecated: ord(): Providing a string that is not one byte long is deprecated. Use ord($str[0]) instead in %s on line 2
///
/// Deprecated: ord(): Providing a string that is not one byte long is deprecated. Use ord($str[0]) instead in %s on line 2
/// int(97)
/// int(0)
/// int(54)
/// int(97)
/// ```
#[php_function]
fn ord(req: &mut Request, character: PhpString) -> i64 {
    let string = character;
    let bytes = string.as_bytes();
    if bytes.len() != 1 {
        let what = if bytes.is_empty() {
            "Providing an empty string is deprecated"
        } else {
            "Providing a string that is not one byte long is deprecated. Use ord($str[0]) instead"
        };
        docref(req, Level::Deprecated, what);
    }
    i64::from(bytes.first().copied().unwrap_or(0))
}

/// `chr(int $codepoint): string`, php's `PHP_FUNCTION(chr)`
/// (`Z/ext/standard/string.c:2673`): the one-byte string of the long's
/// low byte, with the 8.5 deprecation for a value outside 0 to 255. A
/// long in that range written as such never reaches here, since the
/// compiler folds it (`zend_compile_func_chr`); what does is any other
/// argument, read as `Z_PARAM_LONG` reads it. The byte of `chr(null)` is
/// read back through `ord`, since a NUL is no byte a fence pins.
///
/// ```php
/// var_dump(chr(256 + 65), chr(-191), chr(1 + 64), chr("65"), ord(chr(null)));
/// ```
/// ```expect
/// Deprecated: chr(): Providing a value not in-between 0 and 255 is deprecated, this is because a byte value must be in the [0, 255] interval. The value used will be constrained using % 256 in %s on line 2
///
/// Deprecated: chr(): Providing a value not in-between 0 and 255 is deprecated, this is because a byte value must be in the [0, 255] interval. The value used will be constrained using % 256 in %s on line 2
///
/// Deprecated: chr(): Passing null to parameter #1 ($codepoint) of type int is deprecated in %s on line 2
/// string(1) "A"
/// string(1) "A"
/// string(1) "A"
/// string(1) "A"
/// int(0)
/// ```
#[php_function]
fn chr(req: &mut Request, codepoint: i64) -> Result<Owner<PhpString>, Thrown> {
    if !(0..=255).contains(&codepoint) {
        docref(
            req,
            Level::Deprecated,
            "Providing a value not in-between 0 and 255 is deprecated, this is because a byte value must be in the [0, 255] interval. The value used will be constrained using % 256",
        );
    }
    // `c &= 0xff`, then `RETURN_CHAR(c)`.
    let byte = (codepoint & 0xff).to_le_bytes()[0];
    PhpString::new(req, &[byte])
}

/// `str_repeat(string $string, int $times): string`, php's
/// `PHP_FUNCTION(str_repeat)` (`Z/ext/standard/string.c:5489`): the
/// string laid end to end `$times` times, and the empty string for an
/// empty string or no times, before anything is allocated. The
/// arguments are read as `Z_PARAM_STR` and `Z_PARAM_LONG` read them, so
/// `12` and `true` are strings and `'2'` is a count.
///
/// ```php
/// var_dump(str_repeat("ab", 3), str_repeat("", 5), str_repeat("x", 0), str_repeat("x", 1), str_repeat(12, 2), str_repeat("x", "2"), str_repeat(true, 2));
/// ```
/// ```expect
/// string(6) "ababab"
/// string(0) ""
/// string(0) ""
/// string(1) "x"
/// string(4) "1212"
/// string(2) "xx"
/// string(2) "11"
/// ```
///
/// A count with a fraction, as a float or as a float-string, is read as
/// `zend_parse_arg_long_weak` reads it: truncated behind the 8.1
/// deprecation, so `1.5` and `'1.5'` are one repeat each; a float that
/// is NAN or past the long's range is the `TypeError` the parser refuses
/// it with (#253).
///
/// ```php
/// var_dump(str_repeat('x', 1.5), str_repeat('x', '1.5'));
/// str_repeat('x', 1e19);
/// ```
/// ```expect exit=255
/// Deprecated: Implicit conversion from float 1.5 to int loses precision in %s on line 2
///
/// Deprecated: Implicit conversion from float-string "1.5" to int loses precision in %s on line 2
/// string(1) "x"
/// string(1) "x"
///
/// Fatal error: Uncaught TypeError: str_repeat(): Argument #2 ($times) must be of type int, float given in %s:3
/// Stack trace:
/// #0 %s(3): str_repeat('x', 1.0E+19)
/// #1 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// str_repeat('x', NAN);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: str_repeat(): Argument #2 ($times) must be of type int, float given in %s:2
/// Stack trace:
/// #0 %s(2): str_repeat('x', NAN)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// A count below zero is php's `ValueError`, `Argument #2 ($times) must
/// be greater than or equal to 0`, thrown with both arguments in hand,
/// which the trace prints as php prints them: the string in `'`, cut at
/// `zend.exception_string_param_max_len` with `...` after it, and the
/// count as its digits. A call outside the arity of two is its
/// `ArgumentCountError`; `the_count_below_zero_and_the_arity_are_php_s`
/// in `tests/capture.rs` holds that without the oracle.
///
/// A `null` string is `zend_parse_arg_str_weak`'s: the 8.1 deprecation
/// and the empty string written back into the frame, which the trace
/// then prints; under `strict_types` it is the `TypeError`, and the
/// trace prints what was passed.
///
/// ```php
/// str_repeat(null, -1);
/// ```
/// ```expect exit=255
/// Deprecated: str_repeat(): Passing null to parameter #1 ($string) of type string is deprecated in %s on line 2
///
/// Fatal error: Uncaught ValueError: str_repeat(): Argument #2 ($times) must be greater than or equal to 0 in %s:2
/// Stack trace:
/// #0 %s(2): str_repeat('', -1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// declare(strict_types=1);
/// str_repeat(null, 1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: str_repeat(): Argument #1 ($string) must be of type string, null given in %s:3
/// Stack trace:
/// #0 %s(3): str_repeat(NULL, 1)
/// #1 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// str_repeat('a longer string than fifteen', -1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ValueError: str_repeat(): Argument #2 ($times) must be greater than or equal to 0 in %s:2
/// Stack trace:
/// #0 %s(2): str_repeat('a longer string...', -1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// The return's bytes are `zend_string_safe_alloc(len, times, 0)`, and
/// so is what they cost: the product plus the aligned header, so
/// `str_repeat("ab", 3)` moves `memory_get_usage()` by the 40-byte bin
/// and not the 32 a six-byte string takes, as php's does. A product that
/// overflows is php's `Possible integer overflow in memory allocation
/// (len * times + 32)` fatal, raised before a byte is asked for, and one
/// the request's `memory_limit` refuses is the allocator's own fatal,
/// raised where the allocation is asked for and not by this native.
///
/// ```php
/// $b = memory_get_usage(); $s = str_repeat("ab", 3); var_dump(memory_get_usage() - $b);
/// ```
/// ```expect
/// int(40)
/// ```
///
/// ```php
/// $s = str_repeat("x", 200000000);
/// ```
/// ```expect exit=255
/// Fatal error: Allowed memory size of 134217728 bytes exhausted (tried to allocate 200000032 bytes) in %s on line 2
/// ```
#[php_function]
fn str_repeat(
    req: &mut Request,
    string: PhpString,
    times: i64,
) -> Result<Owner<PhpString>, Thrown> {
    if times < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    if string.is_empty() || times == 0 {
        return PhpString::new(req, b"");
    }
    let times = usize::try_from(times).unwrap_or(usize::MAX);
    let pattern = string.as_bytes();
    // `zend_string_safe_alloc(ZSTR_LEN(input_str), mult, 0, 0)`, then
    // php's two fills: one byte `memset`, else `memcpy` of the pattern
    // and then of the result onto itself, doubling what is laid down.
    PhpString::product(req, pattern.len(), times, 0, |result| {
        if pattern.len() == 1 {
            result.fill(pattern[0]);
            return;
        }
        result[..pattern.len()].copy_from_slice(pattern);
        let mut laid = pattern.len();
        while laid < result.len() {
            let step = laid.min(result.len() - laid);
            let (done, rest) = result.split_at_mut(laid);
            rest[..step].copy_from_slice(&done[..step]);
            laid += step;
        }
    })
}

/// `strtoupper(string $string): string`, php's `PHP_FUNCTION(strtoupper)`
/// (`Z/ext/standard/string.c:1207`) over `zend_string_toupper`
/// (`Z/Zend/zend_operators.c:3194`): ASCII only since 8.2, the bytes
/// walked to the first that `toupper` would change, and the string
/// handed back as it is, one more owner (`zend_string_copy`), when no
/// byte would; otherwise one new string of the request's, the bytes
/// before that one copied and the rest mapped. A byte past ASCII is
/// left as it is.
///
/// ```php
/// $s = str_repeat("AB1", 20); $b = memory_get_usage(); $t = strtoupper($s); var_dump($t === $s, memory_get_usage() - $b, strtoupper("abc Z1"), strtoupper("\xe9") === "\xe9", strtoupper(12), strtoupper(""));
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// string(6) "ABC Z1"
/// bool(true)
/// string(2) "12"
/// string(0) ""
/// ```
///
/// A string with a byte to change is a copy, `zend_string_alloc` of the
/// same length, and the request's heap reads the copy after the call.
///
/// ```php
/// $s = str_repeat("ab1", 20); $b = memory_get_usage(); $t = strtoupper($s); var_dump(memory_get_usage() - $b, $t);
/// ```
/// ```expect
/// int(96)
/// string(60) "AB1AB1AB1AB1AB1AB1AB1AB1AB1AB1AB1AB1AB1AB1AB1AB1AB1AB1AB1AB1"
/// ```
///
/// Declared, the name is one `ob_start()` accepts as a handler and
/// `ob_get_status()` reports under `name`. What php then does with it
/// is call it with the buffer and the `PHP_OUTPUT_HANDLER_*` bits, two
/// arguments to a native of one, so the oracle ends in `ArgumentCountError` at the first
/// handler call with the buffer lost; the `ob_*` layer's call of a
/// native handler is #288's, and the witness for it stands there.
#[php_function]
fn strtoupper(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    cased(req, string, u8::to_ascii_uppercase)
}

/// `strtolower(string $string): string`, php's `PHP_FUNCTION(strtolower)`
/// (`Z/ext/standard/string.c:1220`) over `zend_string_tolower`
/// (`Z/Zend/zend_operators.c:3162`), `strtoupper`'s mirror: the string
/// itself with one more owner when no byte is an ASCII capital, a copy
/// with each one lowered otherwise.
///
/// ```php
/// $s = str_repeat("ab1", 20); $b = memory_get_usage(); $t = strtolower($s); var_dump($t === $s, memory_get_usage() - $b, strtolower("ABC z1"), strtolower("\xc9") === "\xc9", strtolower(true), strtolower(""));
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// string(6) "abc z1"
/// bool(true)
/// string(1) "1"
/// string(0) ""
/// ```
///
/// ```php
/// $s = str_repeat("AB1", 20); $b = memory_get_usage(); $t = strtolower($s); var_dump(memory_get_usage() - $b, $t);
/// ```
/// ```expect
/// int(96)
/// string(60) "ab1ab1ab1ab1ab1ab1ab1ab1ab1ab1ab1ab1ab1ab1ab1ab1ab1ab1ab1ab1"
/// ```
#[php_function]
fn strtolower(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    cased(req, string, u8::to_ascii_lowercase)
}

/// `str_pad(string $string, int $length, string $pad_string = " ", int
/// $pad_type = STR_PAD_RIGHT): string`, php's `PHP_FUNCTION(str_pad)`
/// (`Z/ext/standard/string.c:5760`): the string handed back as it is,
/// one more owner, where `$length` is negative or no more than its
/// length, before either later parameter is looked at; then the empty
/// pad string's `ValueError` and the unknown type's; then the padding
/// laid down as the pad string repeated byte by byte, `pad_str[i %
/// pad_str_len]`, all of it on the right, all of it on the left, or the
/// floor of half on the left and the rest on the right.
///
/// ```php
/// var_dump(str_pad("abc", 6), str_pad("abc", 6, "-="), str_pad("abc", 6, "-=", STR_PAD_LEFT), str_pad("abc", 8, "-=", STR_PAD_BOTH), str_pad("abc", 7, "xy", STR_PAD_BOTH), str_pad("abc", 2), str_pad("abc", -1), str_pad("abc", 3), str_pad(12, 4, 0, STR_PAD_LEFT), str_pad("", 2, "ab"));
/// foreach ([fn() => str_pad("abc", 6, ""), fn() => str_pad("abc", 6, " ", 3), fn() => str_pad("abc", 6, " ", -1), fn() => str_pad("abc", 2, "")] as $f) { try { var_dump($f()); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; } }
/// var_dump(STR_PAD_LEFT, STR_PAD_RIGHT, STR_PAD_BOTH);
/// ```
/// ```expect
/// string(6) "abc   "
/// string(6) "abc-=-"
/// string(6) "-=-abc"
/// string(8) "-=abc-=-"
/// string(7) "xyabcxy"
/// string(3) "abc"
/// string(3) "abc"
/// string(3) "abc"
/// string(4) "0012"
/// string(2) "ab"
/// ValueError: str_pad(): Argument #3 ($pad_string) must not be empty
/// ValueError: str_pad(): Argument #4 ($pad_type) must be STR_PAD_LEFT, STR_PAD_RIGHT, or STR_PAD_BOTH
/// ValueError: str_pad(): Argument #4 ($pad_type) must be STR_PAD_LEFT, STR_PAD_RIGHT, or STR_PAD_BOTH
/// string(3) "abc"
/// int(0)
/// int(1)
/// int(2)
/// ```
///
/// The declaration, as `basic_functions.stub.php` has it, which is what
/// a named argument is placed against.
///
/// ```php
/// echo new ReflectionFunction('str_pad');
/// ```
/// ```expect
/// Function [ <internal:standard> function str_pad ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <required> int $length ]
///     Parameter #2 [ <optional> string $pad_string = " " ]
///     Parameter #3 [ <optional> int $pad_type = STR_PAD_RIGHT ]
///   }
///   - Return [ string ]
/// }
/// ```
///
/// ```php
/// str_pad("abc", "x");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: str_pad(): Argument #2 ($length) must be of type int, string given in %s:2
/// Stack trace:
/// #0 %s(2): str_pad('abc', 'x')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// str_pad("abc");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: str_pad() expects at least 2 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): str_pad('abc')
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn str_pad(
    req: &mut Request,
    string: PhpString,
    length: i64,
    #[php(default = " ")] pad_string: PhpString,
    #[php(default = STR_PAD_RIGHT)] pad_type: i64,
) -> Result<Owner<PhpString>, Thrown> {
    let (input, pad) = (string, pad_string.as_bytes());
    let bytes = input.as_bytes();
    if length < 0 || usize::try_from(length).is_ok_and(|length| length <= bytes.len()) {
        return Ok(Owner::take(req, input));
    }
    if pad.is_empty() {
        return Err(arg_value_error(req, 3, "not be empty"));
    }
    let num_pad_chars = usize::try_from(length)
        .unwrap_or(usize::MAX)
        .saturating_sub(bytes.len());
    let (left_pad, right_pad) = match pad_type {
        STR_PAD_RIGHT => (0, num_pad_chars),
        STR_PAD_LEFT => (num_pad_chars, 0),
        STR_PAD_BOTH => (num_pad_chars / 2, num_pad_chars - num_pad_chars / 2),
        _ => {
            return Err(arg_value_error(
                req,
                4,
                "be STR_PAD_LEFT, STR_PAD_RIGHT, or STR_PAD_BOTH",
            ));
        }
    };
    // `zend_string_safe_alloc(1, ZSTR_LEN(input), num_pad_chars, 0)`,
    // then the three fills in place.
    PhpString::product(req, 1, bytes.len(), num_pad_chars, |result| {
        let mut at = 0;
        for i in 0..left_pad {
            result[at] = pad[i % pad.len()];
            at += 1;
        }
        result[at..at + bytes.len()].copy_from_slice(bytes);
        at += bytes.len();
        for i in 0..right_pad {
            result[at] = pad[i % pad.len()];
            at += 1;
        }
    })
}

/// `zend_string_toupper_ex` and `zend_string_tolower_ex`'s one shape
/// over `map`: the argument with one more owner when `map` changes no
/// byte, else a new string of the request's with the bytes before the
/// first changed one copied and every byte from it mapped.
fn cased(
    req: &mut Request,
    string: PhpString,
    map: fn(&u8) -> u8,
) -> Result<Owner<PhpString>, Thrown> {
    let bytes = string.as_bytes();
    let Some(first) = bytes.iter().position(|b| map(b) != *b) else {
        return Ok(Owner::take(req, string));
    };
    PhpString::filled(req, bytes.len(), |out| {
        out[..first].copy_from_slice(&bytes[..first]);
        for (o, b) in out[first..].iter_mut().zip(&bytes[first..]) {
            *o = map(b);
        }
    })
}

/// `trim(string $string, string $characters = " \n\r\t\v\x00"): string`,
/// php's `PHP_FUNCTION(trim)` over `php_do_trim` with both sides
/// (`Z/ext/standard/string.c:641`, its third way, both ends): the string with every byte
/// of the set stripped from each end. The set is `php_charmask`'s: a
/// byte each, with `a..z` a range, and its four warnings for a `..`
/// with nothing before it, nothing after it, or a lesser byte after,
/// which strip what the rest of the set says. The string comes back as
/// it is, one more owner (`zend_string_copy`), when nothing was
/// stripped; empty, the interned empty string, when everything was;
/// and a new string of the request's otherwise (`zend_string_init`).
/// Declared frameless for one argument and for two
/// (`frameless_function_infos_trim`), so a call of either arity is a
/// `FRAMELESS_ICALL_1` or `_2` over this body, whose witnesses are the
/// kernels'.
///
/// ```php
/// var_dump(trim(" \t\n\r\0\x0B a \x0B\0\r\n\t "), trim("xxaxx", "x"), trim("abcxcba", "a..c"), trim("  "), trim("abc", ""));
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "a"
/// string(1) "x"
/// string(0) ""
/// string(3) "abc"
/// ```
///
/// ```php
/// var_dump(trim("..a..", ".."), trim("za", "z.."), trim("bab", "..b"), trim("cac", "c..a"), trim("dad", "a..d..e"));
/// ```
/// ```expect
/// Warning: trim(): Invalid '..'-range, no character to the left of '..' in %s on line 2
///
/// Warning: trim(): Invalid '..'-range, no character to the right of '..' in %s on line 2
///
/// Warning: trim(): Invalid '..'-range, no character to the left of '..' in %s on line 2
///
/// Warning: trim(): Invalid '..'-range, '..'-range needs to be incrementing in %s on line 2
///
/// Warning: trim(): Invalid '..'-range in %s on line 2
/// string(1) "a"
/// string(1) "a"
/// string(1) "a"
/// string(0) ""
/// string(0) ""
/// ```
///
/// A string nothing was stripped from is the argument's own, so the
/// heap reads the same after the call as before it.
///
/// ```php
/// $s = str_repeat("ab", 50); $b = memory_get_usage(); $t = trim($s); var_dump($t === $s, memory_get_usage() - $b);
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// ```
///
/// ```php
/// trim("a", "b", "c");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: trim() expects at most 2 arguments, 3 given in %s:2
/// Stack trace:
/// #0 %s(2): trim('a', 'b', 'c')
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function(frameless(1, 2))]
fn trim(
    req: &mut Request,
    string: PhpString,
    #[php(default = " \n\r\t\x0B\0")] characters: Passed<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let bytes = string.as_bytes();
    let (start, end) = match characters {
        Passed::Given(what) if what.len() == 1 => {
            let p = what.as_bytes()[0];
            trimmed(bytes, |c| c == p)
        }
        Passed::Given(what) => {
            let mask = charmask(req, what.as_bytes());
            trimmed(bytes, |c| mask[c as usize])
        }
        Passed::Omitted => trimmed(bytes, |c| {
            c <= b' ' && matches!(c, b' ' | b'\n' | b'\r' | b'\t' | 0x0B | 0)
        }),
    };
    if end - start == bytes.len() {
        return Ok(Owner::take(req, string));
    }
    PhpString::new(req, &bytes[start..end])
}

/// The span of `bytes` left once every byte `strip` names is taken off
/// both ends, `php_trim_int`'s two loops for `trim` itself.
fn trimmed(bytes: &[u8], strip: impl Fn(u8) -> bool) -> (usize, usize) {
    let mut start = 0;
    let mut end = bytes.len();
    while start != end && strip(bytes[start]) {
        start += 1;
    }
    while start != end && strip(bytes[end - 1]) {
        end -= 1;
    }
    (start, end)
}

/// `php_charmask` (`Z/ext/standard/string.c:475`): the 256-entry mask of
/// a character set, `a..z` a range from the byte before the `..` to the
/// byte after when the second is not the lesser, and php's four
/// warnings for a `..` that is no range, each leaving the mask as the
/// rest of the set builds it.
pub(crate) fn charmask(req: &mut Request, input: &[u8]) -> [bool; 256] {
    let mut mask = [false; 256];
    let mut at = 0;
    while at < input.len() {
        let c = input[at];
        if at + 3 < input.len()
            && input[at + 1] == b'.'
            && input[at + 2] == b'.'
            && input[at + 3] >= c
        {
            for byte in c..=input[at + 3] {
                mask[byte as usize] = true;
            }
            at += 4;
            continue;
        }
        if at + 1 < input.len() && input[at] == b'.' && input[at + 1] == b'.' {
            let message = if at == 0 {
                "Invalid '..'-range, no character to the left of '..'"
            } else if at + 2 >= input.len() {
                "Invalid '..'-range, no character to the right of '..'"
            } else if input[at - 1] > input[at + 2] {
                "Invalid '..'-range, '..'-range needs to be incrementing"
            } else {
                "Invalid '..'-range"
            };
            docref(req, Level::Warning, message);
            at += 1;
            continue;
        }
        mask[c as usize] = true;
        at += 1;
    }
    mask
}

/// `str_contains(string $haystack, string $needle): bool`, php's
/// `PHP_FUNCTION(str_contains)` (`Z/ext/standard/string.c:1815`):
/// whether the needle's bytes stand anywhere in the haystack's,
/// `php_memnstr`, which is `true` for the empty needle. Declared
/// frameless for its two arguments (`frameless_function_infos_str_contains`),
/// so every call of two is a `FRAMELESS_ICALL_2` over this body, whose
/// witnesses are the kernel's; what reaches the frame is a call of
/// another count, or one by name.
///
/// ```php
/// var_dump(str_contains("abc", "bc"), str_contains("abc", "cb"), str_contains("", ""), str_contains("abc", "abcd"), str_contains(true, "1"));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
///
/// ```php
/// str_contains("a");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: str_contains() expects exactly 2 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): str_contains('a')
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function(frameless(2))]
fn str_contains(haystack: PhpString, needle: PhpString) -> bool {
    memnstr(haystack.as_bytes(), needle.as_bytes(), 0).is_some()
}

/// `substr(string $string, int $offset, ?int $length = null): string`,
/// php's `PHP_FUNCTION(substr)` over `_zend_substr`
/// (`Z/ext/standard/string.c:2266`): the bytes from `offset`, counted
/// from the end when negative and clamped to the start when past it,
/// and the empty string for an offset past the end; of `length` bytes,
/// or to the end for a null length, with a negative length stopping
/// that many bytes short of the end and a length past the end clamped.
/// The whole string comes back as it is, one more owner
/// (`RETURN_STR_COPY`), and any other span as `ZVAL_STRINGL_FAST` makes
/// it: the interned empty string, the interned one-byte string, or a
/// new string of the request's. Declared frameless for two arguments
/// and for three (`frameless_function_infos_substr`), so those calls
/// are a `FRAMELESS_ICALL_2` or `_3` over this body, whose witnesses
/// are the kernels'.
///
/// ```php
/// $s = "abcdef";
/// var_dump(substr($s, 0), substr($s, 6), substr($s, 7), substr($s, -6), substr($s, -7), substr($s, 1, -5), substr($s, 1, -6), substr($s, 2, 10), substr("", 0), substr($s, 0, -9223372036854775807 - 1), substr($s, -9223372036854775807 - 1));
/// ```
/// ```expect
/// string(6) "abcdef"
/// string(0) ""
/// string(0) ""
/// string(6) "abcdef"
/// string(6) "abcdef"
/// string(0) ""
/// string(0) ""
/// string(4) "cdef"
/// string(0) ""
/// string(0) ""
/// string(6) "abcdef"
/// ```
///
/// A span that is the whole string is the argument's own, and the heap
/// reads the same after the call as before it.
///
/// ```php
/// $s = str_repeat("ab", 50); $b = memory_get_usage(); $t = substr($s, 0, 100); var_dump($t === $s, memory_get_usage() - $b, substr($s, "1", "2.0"));
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// string(2) "ba"
/// ```
///
/// ```php
/// substr("a", 1, 2, 3);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: substr() expects at most 3 arguments, 4 given in %s:2
/// Stack trace:
/// #0 %s(2): substr('a', 1, 2, 3)
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function(frameless(2, 3))]
fn substr(
    req: &mut Request,
    string: PhpString,
    offset: i64,
    #[php(default = null)] length: Option<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let mut from = offset;
    let len = i64::try_from(string.len()).unwrap_or(i64::MAX);
    if from < 0 {
        // `-(size_t)f > ZSTR_LEN(str)`: a negative offset further back
        // than the string is long starts at its first byte.
        from = if from.unsigned_abs() > u64::try_from(string.len()).unwrap_or(u64::MAX) {
            0
        } else {
            len + from
        };
    } else if from > len {
        return PhpString::new(req, b"");
    }
    let rest = len - from;
    let taken = match length {
        Some(l) if l < 0 => {
            if l.unsigned_abs() > u64::try_from(rest).unwrap_or(0) {
                0
            } else {
                rest + l
            }
        }
        Some(l) if l > rest => rest,
        Some(l) => l,
        None => rest,
    };
    if taken == len {
        return Ok(Owner::take(req, string));
    }
    let start = usize::try_from(from).unwrap_or(0);
    let end = start + usize::try_from(taken).unwrap_or(0);
    PhpString::new(req, &string.as_bytes()[start..end])
}

/// `zend_tolower_ascii`.
const fn lower(byte: u8) -> u8 {
    byte.to_ascii_lowercase()
}

/// `zend_memnstr` (`Z/Zend/zend_operators.h:221`): the first offset of
/// `needle` in `haystack` at or after `from`, the empty needle found
/// where the search starts. A one-byte needle is `memchr`; a longer one
/// is each `memchr` of its first byte that has its last byte where the
/// needle would end, compared whole. php hands a haystack of a kilobyte
/// or more and a needle of nine bytes or more to `zend_memnstr_ex`'s
/// Sunday search, which finds the same offset.
fn memnstr(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let rest = haystack.get(from..)?;
    let found = match needle {
        [] => Some(0),
        &[byte] => crate::ffi::memchr(rest, byte),
        &[first, .., last] => {
            // The last offset a match can start at.
            let end = rest.len().checked_sub(needle.len())?;
            let mut at = 0;
            loop {
                let candidate = at + crate::ffi::memchr(rest.get(at..=end)?, first)?;
                if rest.get(candidate + needle.len() - 1) == Some(&last)
                    && rest.get(candidate..candidate + needle.len()) == Some(needle)
                {
                    break Some(candidate);
                }
                at = candidate + 1;
            }
        }
    };
    found.map(|at| at + from)
}

/// The `zend_long` php returns for a length or an offset.
fn long(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// A `zend_long` in decimal, the digits on the stack:
/// `zend_print_long_to_buf` (`Z/Zend/zend_operators.h:993`) into its
/// `char buf[MAX_LENGTH_OF_LONG + 1]`, what `zend_long_to_str` and
/// `smart_str_append_long` write without a string of their own.
pub(crate) struct Decimal {
    buf: [u8; 20],
    start: usize,
}

impl Decimal {
    pub(crate) fn of(n: i64) -> Decimal {
        let mut buf = [0u8; 20];
        let mut start = buf.len();
        let mut rest = n.unsigned_abs();
        loop {
            start -= 1;
            buf[start] = b'0' + u8::try_from(rest % 10).unwrap_or(0);
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        if n < 0 {
            start -= 1;
            buf[start] = b'-';
        }
        Decimal { buf, start }
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.buf[self.start..]
    }
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
///
/// The declarations as Reflection renders them.
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
#[php_function]
fn bin2hex(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let input = string.as_bytes();
    PhpString::product(req, input.len(), 2, 0, |out| {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        for (i, byte) in input.iter().enumerate() {
            out[2 * i] = DIGITS[usize::from(byte >> 4)];
            out[2 * i + 1] = DIGITS[usize::from(byte & 15)];
        }
    })
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
#[php_function]
fn hex2bin(req: &mut Request, string: PhpString) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let input = string.as_bytes();
    if !input.len().is_multiple_of(2) {
        docref(
            req,
            Level::Warning,
            "Hexadecimal input string must have an even length",
        );
        return Ok(OrFalse::False);
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
            return Ok(OrFalse::False);
        };
        out.push((high << 4) | low);
    }
    PhpString::new(req, &out).map(OrFalse::Value)
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
#[php_function]
fn strspn(
    string: PhpString,
    characters: PhpString,
    #[php(default = 0)] offset: i64,
    #[php(default = null)] length: Option<i64>,
) -> i64 {
    spn((string, characters, offset, length), true)
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
#[php_function]
fn strcspn(
    string: PhpString,
    characters: PhpString,
    #[php(default = 0)] offset: i64,
    #[php(default = null)] length: Option<i64>,
) -> i64 {
    spn((string, characters, offset, length), false)
}

/// `php_spn_common_handler(..., is_strspn)`.
fn spn(
    (string, characters, mut start, length): (PhpString, PhpString, i64, Option<i64>),
    must_match: bool,
) -> i64 {
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
        return 0;
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
    long(run)
}

/// `php_do_trim(..., mode)` for `rtrim` and `ltrim`: `trim`'s masks,
/// one side taken.
fn trim_side(
    req: &mut Request,
    string: PhpString,
    what: Passed<PhpString>,
    left: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let bytes = string.as_bytes();
    let strip: Box<dyn Fn(u8) -> bool> = match what {
        Passed::Given(what) if what.len() == 1 => {
            let p = what.as_bytes()[0];
            Box::new(move |c| c == p)
        }
        Passed::Given(what) => {
            let mask = charmask(req, what.as_bytes());
            Box::new(move |c| mask[usize::from(c)])
        }
        Passed::Omitted => {
            Box::new(|c| c <= b' ' && matches!(c, b' ' | b'\n' | b'\r' | b'\t' | 0x0B | 0))
        }
    };
    let (start, end) = if left {
        let start = bytes.iter().take_while(|c| strip(**c)).count();
        (start, bytes.len())
    } else {
        let kept = bytes.len() - bytes.iter().rev().take_while(|c| strip(**c)).count();
        (0, kept)
    };
    if end - start == bytes.len() {
        return Ok(Owner::take(req, string));
    }
    PhpString::new(req, &bytes[start..end])
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
#[php_function(aliases(chop))]
fn rtrim(
    req: &mut Request,
    string: PhpString,
    #[php(default = " \n\r\t\x0B\0")] characters: Passed<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    trim_side(req, string, characters, false)
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
#[php_function]
fn ltrim(
    req: &mut Request,
    string: PhpString,
    #[php(default = " \n\r\t\x0B\0")] characters: Passed<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    trim_side(req, string, characters, true)
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
///
/// Where the break is longer than a byte or long words are cut, php asks
/// for the longest the text can grow to before it scans a byte, and the
/// memory limit is met there, whatever the text turns out to be.
///
/// ```php
/// var_dump(wordwrap("ab cd", 1, "--"));
/// wordwrap(str_repeat("x", 65534), 1, str_repeat("x", 65535));
/// ```
/// ```expect exit=255
/// string(6) "ab--cd"
///
/// Fatal error: Allowed memory size of 134217728 bytes exhausted (tried to allocate 4294901785 bytes) in %s on line 3
/// ```
#[php_function]
fn wordwrap(
    req: &mut Request,
    string: PhpString,
    #[php(default = 75)] width: i64,
    #[php(default = "\n")] r#break: PhpString,
    #[php(default = false)] cut_long_words: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let (linelength, docut) = (width, cut_long_words);
    let text = string.as_bytes();
    if text.is_empty() {
        return PhpString::new(req, b"");
    }
    let brk: &[u8] = r#break.as_bytes();
    if brk.is_empty() {
        return Err(arg_value_error(req, 3, "not be empty"));
    }
    if linelength == 0 && docut {
        return Err(argument_error(
            req,
            "ValueError",
            4,
            "cannot be true when argument #2 ($width) is 0",
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
        return PhpString::new(req, &out);
    }
    // "Multiple character line break or forced cut": php asks for the
    // longest the text can grow to, `zend_string_safe_alloc`, before a
    // byte is scanned, so that is where the memory limit is met.
    let (chunks, each, over) = match usize::try_from(linelength) {
        Ok(width) if width > 0 => (text.len() / width + 1, brk.len(), text.len()),
        _ => (text.len(), brk.len() + 1, 0),
    };
    let charge = PhpString::product(req, chunks, each, over, |_| {})?;
    charge.give_up(req);
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
    PhpString::new(req, &out)
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
#[php_function]
fn explode(
    req: &mut Request,
    separator: PhpString,
    string: PhpString,
    #[php(default = PHP_INT_MAX)] limit: i64,
) -> Result<Owner<PhpArray>, Thrown> {
    let delim = separator;
    if delim.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    let mut array = PhpArray::new(req)?;
    let (d, s) = (delim.as_bytes(), string.as_bytes());
    if s.is_empty() {
        if limit >= 0 {
            let empty = PhpString::new(req, b"")?.into_value();
            array.append(req, empty)?;
        }
        return Ok(array);
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
        let whole = Owner::take(req, string).into_value();
        array.append(req, whole)?;
        return Ok(array);
    }
    for i in 0..take {
        let value = PhpString::new(req, piece(i))?.into_value();
        array.append(req, value)?;
    }
    Ok(array)
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
///
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
#[php_function(aliases(join))]
fn implode(
    req: &mut Request,
    separator: Union<PhpArray, PhpString>,
    #[php(default = null)] array: Option<PhpArray>,
) -> Result<Owner<PhpString>, Thrown> {
    imploded(req, separator, array)
}

/// `PHP_FUNCTION(implode)` under the name it was called by, which
/// php's `get_active_function_name()` puts in the first `TypeError`.
fn imploded(
    req: &mut Request,
    separator: Union<PhpArray, PhpString>,
    array: Option<PhpArray>,
) -> Result<Owner<PhpString>, Thrown> {
    let name = req.active_function_name();
    let (glue, pieces) = match (separator, array) {
        (Union::A(array), None) => (None, array),
        (_, None) => {
            return Err(throw_error(
                req,
                "TypeError",
                &format!(
                    "{name}(): If argument #1 ($separator) is of type string, argument #2 ($array) must be of type array, null given"
                ),
            ));
        }
        (Union::B(glue), Some(pieces)) => (Some(glue), pieces),
        (_, Some(_)) => {
            return Err(php_ext::raising::arg_type_error(
                req,
                1,
                "be of type string, array given",
            ));
        }
    };
    let glue = glue.as_ref().map_or(&b""[..], PhpString::as_bytes);
    implode_array(req, glue, pieces)
}

/// `php_implode(glue, pieces)`.
fn implode_array(
    req: &mut Request,
    glue: &[u8],
    pieces: PhpArray,
) -> Result<Owner<PhpString>, Thrown> {
    let elements: Vec<Value> = php_ext::values::walk::rows(req, pieces)
        .map(|(_, value)| value)
        .collect();
    if elements.len() == 1
        && let Some(s) = elements[0].deref().as_php_string()
    {
        return Ok(Owner::take(req, *s));
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
            with_string(req, &element, |s| out.extend_from_slice(s))?;
        }
    }
    PhpString::new(req, &out)
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
#[php_function]
fn strtok(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] token: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
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
        return Ok(OrFalse::False);
    };
    let end = subject.len();
    if last >= end {
        // Past the end: `false`, and the string kept, as php keeps it.
        req.state::<crate::State>().string.strtok = Some((subject, last));
        return Ok(OrFalse::False);
    }
    let mut table = [false; 256];
    for c in &tok {
        table[usize::from(*c)] = true;
    }
    let mut p = last;
    while table[usize::from(subject[p])] {
        p += 1;
        if p >= end {
            return Ok(OrFalse::False);
        }
    }
    let skipped = p - last;
    p += 1;
    while p < end && !table[usize::from(subject[p])] {
        p += 1;
    }
    let token = PhpString::new(req, &subject[last + skipped..p])?;
    req.state::<crate::State>().string.strtok = Some((subject, p + 1));
    Ok(OrFalse::Value(token))
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
#[php_function]
fn str_increment(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
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
    PhpString::new(req, &bytes)
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
#[php_function]
fn str_decrement(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let mut bytes = alphanumeric_argument(req, string)?;
    // `zend_argument_value_error(1, "\"%s\" is out of decrement
    // range", ...)`: a sentence without the `must` the face writes.
    let out_of_range = |req: &mut Request| {
        let text = String::from_utf8_lossy(string.as_bytes()).into_owned();
        argument_error(
            req,
            "ValueError",
            1,
            &format!("\"{text}\" is out of decrement range"),
        )
    };
    if bytes[0] == b'0' {
        return Err(out_of_range(req));
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
            return Err(out_of_range(req));
        }
        bytes.remove(0);
    }
    PhpString::new(req, &bytes)
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
#[php_function]
fn chunk_split(
    req: &mut Request,
    string: PhpString,
    #[php(default = 76)] length: i64,
    #[php(default = "\r\n")] separator: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    let chunklen = length;
    let sep: &[u8] = separator.as_bytes();
    if chunklen <= 0 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    let bytes = string.as_bytes();
    let chunk = usize::try_from(chunklen).unwrap_or(usize::MAX);
    if chunk > bytes.len() {
        let mut out = bytes.to_vec();
        out.extend_from_slice(sep);
        return PhpString::new(req, &out);
    }
    if bytes.is_empty() {
        return PhpString::new(req, b"");
    }
    let mut out = Vec::with_capacity(bytes.len() + (bytes.len() / chunk + 1) * sep.len());
    for piece in bytes.chunks(chunk) {
        out.extend_from_slice(piece);
        out.extend_from_slice(sep);
    }
    PhpString::new(req, &out)
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
#[php_function]
fn quotemeta(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
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
    PhpString::new(req, &out)
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
#[php_function]
fn ucfirst(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    first_cased(req, string, u8::to_ascii_uppercase)
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
#[php_function]
fn lcfirst(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    first_cased(req, string, u8::to_ascii_lowercase)
}

/// `php_ucfirst` and `php_lcfirst`.
fn first_cased(
    req: &mut Request,
    string: PhpString,
    map: fn(&u8) -> u8,
) -> Result<Owner<PhpString>, Thrown> {
    let bytes = string.as_bytes();
    let Some(first) = bytes.first() else {
        return PhpString::new(req, b"");
    };
    if map(first) == *first {
        return Ok(Owner::take(req, string));
    }
    let mut out = bytes.to_vec();
    out[0] = map(first);
    PhpString::new(req, &out)
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
#[php_function]
fn ucwords(
    req: &mut Request,
    string: PhpString,
    #[php(default = " \t\r\n\x0C\x0B")] separators: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    if string.is_empty() {
        return PhpString::new(req, b"");
    }
    let set: &[u8] = separators.as_bytes();
    let mask = charmask(req, set);
    let mut out = string.as_bytes().to_vec();
    out[0] = out[0].to_ascii_uppercase();
    for i in 1..out.len() {
        if mask[usize::from(out[i - 1])] {
            out[i] = out[i].to_ascii_uppercase();
        }
    }
    PhpString::new(req, &out)
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
#[php_function]
fn strrev(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let out: Vec<u8> = string.as_bytes().iter().rev().copied().collect();
    PhpString::new(req, &out)
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
#[php_function]
fn similar_text(
    req: &mut Request,
    string1: PhpString,
    string2: PhpString,
    #[php(default = null)] percent: Ref<Untyped<Value>>,
) -> Result<i64, Thrown> {
    let (t1, t2) = (string1, string2);
    let (a, b) = (t1.as_bytes(), t2.as_bytes());
    if a.len() + b.len() == 0 {
        percent.set(req, Untyped(Value::float(0.0)))?;
        return Ok(0);
    }
    let sim = similar_char(a, b);
    if percent.is_given() {
        // `sim * 200.0 / (len1 + len2)`, both sides as C's doubles.
        let as_double = |n: usize| f64::from(u32::try_from(n).unwrap_or(u32::MAX));
        let p = as_double(sim) * 200.0 / as_double(a.len() + b.len());
        percent.set(req, Untyped(Value::float(p)))?;
    }
    Ok(long(sim))
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
///
/// Punctuation is `ispunct(3)`'s under the request's `LC_CTYPE`: macOS's
/// `C.UTF-8` counts `\xab` in, so it joins the Hebrew block beside it.
///
/// ```php
/// foreach (['C', 'C.UTF-8'] as $l) { setlocale(LC_CTYPE, $l); var_dump(bin2hex(hebrev("\xe0\xab x")), bin2hex(hebrev("cd\xab\xe0"))); }
/// ```
/// ```expect
/// string(8) "ab2078e0"
/// string(8) "e06364ab"
/// string(8) "7820abe0"
/// string(8) "e0ab6364"
/// ```
#[php_function]
fn hebrev(
    req: &mut Request,
    string: PhpString,
    #[php(default = 0)] max_chars_per_line: i64,
) -> Result<Owner<PhpString>, Thrown> {
    let max_chars = max_chars_per_line;
    let s = string.as_bytes();
    if s.is_empty() {
        return PhpString::new(req, b"");
    }
    let out = hebrev_bytes(s, max_chars);
    PhpString::new(req, &out)
}

/// php's two loops of `hebrev`, a line-for-line copy; punctuation is
/// `ispunct(3)`'s under the thread's `LC_CTYPE`.
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
                || c::is(Class::Punct, at(tmp + 1))
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
            while (isblank(at(tmp)) || c::is(Class::Punct, at(tmp)))
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
#[php_function]
fn nl2br(
    req: &mut Request,
    string: PhpString,
    #[php(default = true)] use_xhtml: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let xhtml = use_xhtml;
    let s = string.as_bytes();
    if !s.iter().any(|c| *c == b'\r' || *c == b'\n') {
        return Ok(Owner::take(req, string));
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
    PhpString::new(req, &out)
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
#[php_function]
fn count_chars(
    req: &mut Request,
    string: PhpString,
    #[php(default = 0)] mode: i64,
) -> Result<Union<Owner<PhpArray>, Owner<PhpString>>, Thrown> {
    let input = string;
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
        return PhpString::new(req, &bytes).map(Union::B);
    }
    let mut array = PhpArray::new(req)?;
    for (inx, count) in chars.iter().enumerate() {
        let keep = match mode {
            0 => true,
            1 => *count != 0,
            _ => *count == 0,
        };
        if keep {
            array.set(req, Owner::index(long(inx)), Owner::int(long(*count)))?;
        }
    }
    Ok(Union::A(array))
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
#[php_function]
fn substr_count(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] mut offset: i64,
    #[php(default = null)] length: Option<i64>,
) -> Result<i64, Thrown> {
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
    Ok(long(count))
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
#[php_function]
fn str_rot13(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let out: Vec<u8> = string
        .as_bytes()
        .iter()
        .map(|&c| match c {
            b'a'..=b'z' => b'a' + (c - b'a' + 13) % 26,
            b'A'..=b'Z' => b'A' + (c - b'A' + 13) % 26,
            _ => c,
        })
        .collect();
    PhpString::new(req, &out)
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
#[php_function]
fn str_shuffle(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let mut bytes = string.as_bytes().to_vec();
    Rng::default_engine().shuffle(req, &mut bytes)?;
    PhpString::new(req, &bytes)
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
#[php_function]
fn str_word_count(
    req: &mut Request,
    string: PhpString,
    #[php(default = 0)] format: i64,
    #[php(default = null)] characters: Option<PhpString>,
) -> Result<Union<Owner<PhpArray>, i64>, Thrown> {
    let list = characters;
    let s = string.as_bytes();
    let mut array = match format {
        1 | 2 => {
            let array = PhpArray::new(req)?;
            if s.is_empty() {
                return Ok(Union::A(array));
            }
            Some(array)
        }
        0 => {
            if s.is_empty() {
                return Ok(Union::B(0));
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
                    let word = PhpString::new(req, &s[start..p])?.into_value();
                    array.append(req, word)?;
                }
                (Some(array), _) => {
                    let word = PhpString::new(req, &s[start..p])?.into_value();
                    array.set(req, Owner::index(long(start)), word)?;
                }
                (None, _) => count += 1,
            }
        }
        p += 1;
    }
    Ok(match array {
        Some(array) => Union::A(array),
        None => Union::B(long(count)),
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
#[php_function]
fn str_split(
    req: &mut Request,
    string: PhpString,
    #[php(default = 1)] length: i64,
) -> Result<Owner<PhpArray>, Thrown> {
    if length <= 0 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    if string.is_empty() {
        return Ok(Owner::take(req, PhpArray::empty()));
    }
    let mut array = PhpArray::new(req)?;
    let chunk = usize::try_from(length).unwrap_or(usize::MAX);
    for piece in string.as_bytes().chunks(chunk) {
        let value = PhpString::new(req, piece)?.into_value();
        array.append(req, value)?;
    }
    Ok(array)
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
#[php_function]
fn strpbrk(
    req: &mut Request,
    string: PhpString,
    characters: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let (haystack, list) = (string, characters);
    if list.is_empty() {
        return Err(arg_value_error(req, 2, "be a non-empty string"));
    }
    let set = list.as_bytes();
    let bytes = haystack.as_bytes();
    match bytes.iter().position(|c| set.contains(c)) {
        Some(at) => PhpString::new(req, &bytes[at..]).map(OrFalse::Value),
        None => Ok(OrFalse::False),
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
#[php_function]
fn substr_compare(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    mut offset: i64,
    #[php(default = null)] length: Option<i64>,
    #[php(default = false)] case_insensitive: bool,
) -> Result<i64, Thrown> {
    let (s1, s2) = (haystack, needle);
    if let Some(len) = length
        && len <= 0
    {
        if len == 0 {
            return Ok(0);
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
    Ok(binary_strncmp(a, b, cmp_len, case_insensitive))
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
#[php_function(deprecated(
    since = "8.2",
    message = "visit the php.net documentation for various alternatives"
))]
fn utf8_encode(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let mut out = Vec::with_capacity(string.len() * 2);
    for &c in string.as_bytes() {
        if c < 0x80 {
            out.push(c);
        } else {
            out.push(0xc0 | (c >> 6));
            out.push(0x80 | (c & 0x3f));
        }
    }
    PhpString::new(req, &out)
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
#[php_function(deprecated(
    since = "8.2",
    message = "visit the php.net documentation for various alternatives"
))]
fn utf8_decode(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
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
    PhpString::new(req, &out)
}

/// `get_next_char(cs_utf_8, ...)` (`Z/ext/standard/html.c:89`), what
/// `php_next_utf8_char` answers: the code point at `pos` and the offset
/// after it, or `None` and the offset past the ill-formed sequence
/// ([`Charset::next_char`]).
#[must_use]
pub fn next_utf8_char(s: &[u8], pos: usize) -> (Option<u32>, usize) {
    Charset::Utf8.next_char(s, pos)
}
