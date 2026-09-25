//! The `pcre` extension: `php-src/ext/pcre` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade, and the library php links,
//! libpcre2-8 through `pcre2-sys` with `pcre2_jit_compile` on (design §7.2); one entry,
//! `extension()`; its state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/pcre/tests`, through `xtask phpt --subset`.
//!
//! Declared as `php_pcre.stub.php` at the tag declares it: the nineteen
//! constants `register_php_pcre_symbols` registers, in its order, the
//! eleven functions, and the three ini entries. The bodies are
//! `php_pcre.c`'s. The state is split where php splits it at
//! `RSHUTDOWN` (design §2.3): compiled patterns live in the engine's
//! cache of immutable values, [`pattern::Patterns`], and the match data,
//! `mctx` and `PCRE_G(error_code)` are the request's,
//! [`State`]. The `unsafe` the library needs is `src/ffi/`'s alone.
//! Beyond `extension()` the crate exports [`Regex`], the compiled-pattern
//! face ext/spl and ext/filter read (design §4.10).
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod ffi;
mod matching;
mod pattern;
mod regex;
mod replace;
mod split;

pub use regex::Regex;

use php_ext::abi::{Callable, Extension, ExtensionBuilder};
use php_ext::coerce::coerce_param;
use php_ext::decl::{ConstDecl, ConstExpr, FunctionDecl, IniDecl, Name, Parameter, Type};
use php_ext::raising::{Thrown, arg_count_error};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, Value, ValueRef};

/// `preg_match_all()` orders.
const PATTERN_ORDER: i64 = 1;
const SET_ORDER: i64 = 2;
/// The match flags.
const OFFSET_CAPTURE: i64 = 1 << 8;
const UNMATCHED_AS_NULL: i64 = 1 << 9;
/// `preg_split()` flags.
const SPLIT_NO_EMPTY: i64 = 1 << 0;
const SPLIT_DELIM_CAPTURE: i64 = 1 << 1;
const SPLIT_OFFSET_CAPTURE: i64 = 1 << 2;
/// `preg_grep()`'s option.
const GREP_INVERT: i64 = 1 << 0;

/// `php_pcre_error_code`, in its order.
const NO_ERROR: i64 = 0;
const INTERNAL_ERROR: i64 = 1;
const BACKTRACK_LIMIT_ERROR: i64 = 2;
const RECURSION_LIMIT_ERROR: i64 = 3;
const BAD_UTF8_ERROR: i64 = 4;
const BAD_UTF8_OFFSET_ERROR: i64 = 5;
const JIT_STACKLIMIT_ERROR: i64 = 6;

/// The three ini entries and their defaults, `PHP_INI_ALL` each.
const INI: [(&str, &str); 3] = [
    ("pcre.backtrack_limit", "1000000"),
    ("pcre.recursion_limit", "100000"),
    ("pcre.jit", "1"),
];

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised. `PCRE_G(error_code)`, and php's `mdata` and `mctx`:
/// the match data of 32 pairs a call borrows while no other call holds
/// it, and `mctx` with its `jit_stack`, each built on first
/// use and taken out while a call runs, so a call made from a callback
/// builds its own, as php's `mdata_used` has it.
#[derive(Default)]
struct State {
    error_code: i64,
    data: Option<ffi::MatchData>,
    context: Option<ffi::Context>,
}

/// The extension value (design §7.1): declarations paired with entries,
/// the constants, the ini entries and the two states.
///
/// ```php
/// var_dump(extension_loaded('pcre'), ini_get('pcre.backtrack_limit'), ini_get('pcre.recursion_limit'), ini_get('pcre.jit'));
/// ```
/// ```expect
/// bool(true)
/// string(7) "1000000"
/// string(6) "100000"
/// string(1) "1"
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .constants(constants())
        .functions(functions())
        .ini(
            INI.iter()
                .map(|(name, default)| IniDecl {
                    name: Name(name.as_bytes().into()),
                    default: default.as_bytes().into(),
                    user: true,
                    per_dir: true,
                    system: true,
                })
                .collect(),
        )
        .request_state::<State>()
        .engine_state(pattern::engine_state)
        .info(pcre_info)
        .build("pcre")
}

/// A native's entry, the shape `ExtensionBuilder::functions` takes.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// The constants `register_php_pcre_symbols` registers, in its order.
/// `PCRE_VERSION` and its two numbers are the linked library's.
///
/// ```php
/// var_dump(PREG_PATTERN_ORDER, PREG_SET_ORDER, PREG_OFFSET_CAPTURE, PREG_UNMATCHED_AS_NULL, PREG_SPLIT_NO_EMPTY, PREG_SPLIT_DELIM_CAPTURE, PREG_SPLIT_OFFSET_CAPTURE, PREG_GREP_INVERT, PREG_NO_ERROR, PREG_INTERNAL_ERROR, PREG_BACKTRACK_LIMIT_ERROR, PREG_RECURSION_LIMIT_ERROR, PREG_BAD_UTF8_ERROR, PREG_BAD_UTF8_OFFSET_ERROR, PREG_JIT_STACKLIMIT_ERROR, PCRE_VERSION_MAJOR, PCRE_JIT_SUPPORT, is_string(PCRE_VERSION));
/// ```
/// ```expect
/// int(1)
/// int(2)
/// int(256)
/// int(512)
/// int(1)
/// int(2)
/// int(4)
/// int(1)
/// int(0)
/// int(1)
/// int(2)
/// int(3)
/// int(4)
/// int(5)
/// int(6)
/// int(10)
/// bool(true)
/// bool(true)
/// ```
fn constants() -> Vec<ConstDecl> {
    let version = ffi::version();
    let number = |at: usize| -> i64 {
        version
            .split([' ', '.'])
            .nth(at)
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    let ints: [(&str, i64); 15] = [
        ("PREG_PATTERN_ORDER", PATTERN_ORDER),
        ("PREG_SET_ORDER", SET_ORDER),
        ("PREG_OFFSET_CAPTURE", OFFSET_CAPTURE),
        ("PREG_UNMATCHED_AS_NULL", UNMATCHED_AS_NULL),
        ("PREG_SPLIT_NO_EMPTY", SPLIT_NO_EMPTY),
        ("PREG_SPLIT_DELIM_CAPTURE", SPLIT_DELIM_CAPTURE),
        ("PREG_SPLIT_OFFSET_CAPTURE", SPLIT_OFFSET_CAPTURE),
        ("PREG_GREP_INVERT", GREP_INVERT),
        ("PREG_NO_ERROR", NO_ERROR),
        ("PREG_INTERNAL_ERROR", INTERNAL_ERROR),
        ("PREG_BACKTRACK_LIMIT_ERROR", BACKTRACK_LIMIT_ERROR),
        ("PREG_RECURSION_LIMIT_ERROR", RECURSION_LIMIT_ERROR),
        ("PREG_BAD_UTF8_ERROR", BAD_UTF8_ERROR),
        ("PREG_BAD_UTF8_OFFSET_ERROR", BAD_UTF8_OFFSET_ERROR),
        ("PREG_JIT_STACKLIMIT_ERROR", JIT_STACKLIMIT_ERROR),
    ];
    let constant = |name: &str, value: ConstExpr| ConstDecl {
        name: Name(name.as_bytes().into()),
        value,
        attributes: Vec::new(),
    };
    let mut all: Vec<ConstDecl> = ints
        .into_iter()
        .map(|(name, value)| constant(name, ConstExpr::Long(value)))
        .collect();
    all.extend([
        constant("PCRE_VERSION", ConstExpr::Str(version.as_bytes().into())),
        constant("PCRE_VERSION_MAJOR", ConstExpr::Long(number(0))),
        constant("PCRE_VERSION_MINOR", ConstExpr::Long(number(1))),
        constant("PCRE_JIT_SUPPORT", ConstExpr::Bool(ffi::jit_supported())),
    ]);
    all
}

/// The eleven functions, in the stub's order, `preg_match` frameless
/// for two arguments and `preg_replace` for three.
///
/// ```php
/// echo new ReflectionFunction('preg_match'), new ReflectionFunction('preg_replace_callback'), new ReflectionFunction('preg_quote');
/// ```
/// ```expect
/// Function [ <internal:pcre> function preg_match ] {
///
///   - Parameters [5] {
///     Parameter #0 [ <required> string $pattern ]
///     Parameter #1 [ <required> string $subject ]
///     Parameter #2 [ <optional> &$matches = null ]
///     Parameter #3 [ <optional> int $flags = 0 ]
///     Parameter #4 [ <optional> int $offset = 0 ]
///   }
///   - Return [ int|false ]
/// }
/// Function [ <internal:pcre> function preg_replace_callback ] {
///
///   - Parameters [6] {
///     Parameter #0 [ <required> array|string $pattern ]
///     Parameter #1 [ <required> callable $callback ]
///     Parameter #2 [ <required> array|string $subject ]
///     Parameter #3 [ <optional> int $limit = -1 ]
///     Parameter #4 [ <optional> &$count = null ]
///     Parameter #5 [ <optional> int $flags = 0 ]
///   }
///   - Return [ array|string|null ]
/// }
/// Function [ <internal:pcre> function preg_quote ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $str ]
///     Parameter #1 [ <optional> ?string $delimiter = null ]
///   }
///   - Return [ string ]
/// }
/// ```
#[allow(clippy::too_many_lines)] // one table, in the stub's order
fn functions() -> Vec<(FunctionDecl, Entry)> {
    let string = |name: &[u8]| parameter(name, Some(Type::String));
    let either = |name: &[u8]| parameter(name, Some(Type::Union(vec![Type::Array, Type::String])));
    let int = |name: &[u8], default: i64| Parameter {
        default: Some(ConstExpr::Long(default)),
        ..parameter(name, Some(Type::Int))
    };
    let by_ref = |name: &[u8]| Parameter {
        by_ref: true,
        default: Some(ConstExpr::Null),
        ..parameter(name, None)
    };
    let matcher = |name: &[u8], frameless: Vec<u32>| FunctionDecl {
        frameless,
        ..declared(
            name,
            vec![
                string(b"pattern"),
                string(b"subject"),
                by_ref(b"matches"),
                int(b"flags", 0),
                int(b"offset", 0),
            ],
            Type::Union(vec![Type::Int, Type::False]),
        )
    };
    let string_array_null = || Type::Union(vec![Type::String, Type::Array, Type::Null]);
    let replacer = |name: &[u8], frameless: Vec<u32>| FunctionDecl {
        frameless,
        ..declared(
            name,
            vec![
                either(b"pattern"),
                either(b"replacement"),
                either(b"subject"),
                int(b"limit", -1),
                by_ref(b"count"),
            ],
            string_array_null(),
        )
    };
    let array_or_false = || Type::Union(vec![Type::Array, Type::False]);
    vec![
        (
            matcher(b"preg_match", vec![2]),
            matching::preg_match as Entry,
        ),
        (
            matcher(b"preg_match_all", Vec::new()),
            matching::preg_match_all,
        ),
        (replacer(b"preg_replace", vec![3]), replace::preg_replace),
        (replacer(b"preg_filter", Vec::new()), replace::preg_filter),
        (
            declared(
                b"preg_replace_callback",
                vec![
                    either(b"pattern"),
                    parameter(b"callback", Some(Type::Callable)),
                    either(b"subject"),
                    int(b"limit", -1),
                    by_ref(b"count"),
                    int(b"flags", 0),
                ],
                string_array_null(),
            ),
            replace::preg_replace_callback,
        ),
        (
            declared(
                b"preg_replace_callback_array",
                vec![
                    parameter(b"pattern", Some(Type::Array)),
                    either(b"subject"),
                    int(b"limit", -1),
                    by_ref(b"count"),
                    int(b"flags", 0),
                ],
                string_array_null(),
            ),
            replace::preg_replace_callback_array,
        ),
        (
            declared(
                b"preg_split",
                vec![
                    string(b"pattern"),
                    string(b"subject"),
                    int(b"limit", -1),
                    int(b"flags", 0),
                ],
                array_or_false(),
            ),
            split::preg_split,
        ),
        (
            declared(
                b"preg_quote",
                vec![
                    string(b"str"),
                    Parameter {
                        default: Some(ConstExpr::Null),
                        ..parameter(b"delimiter", Some(Type::Nullable(Box::new(Type::String))))
                    },
                ],
                Type::String,
            ),
            preg_quote,
        ),
        (
            declared(
                b"preg_grep",
                vec![
                    string(b"pattern"),
                    parameter(b"array", Some(Type::Array)),
                    int(b"flags", 0),
                ],
                array_or_false(),
            ),
            split::preg_grep,
        ),
        (
            declared(b"preg_last_error", Vec::new(), Type::Int),
            preg_last_error,
        ),
        (
            declared(b"preg_last_error_msg", Vec::new(), Type::String),
            preg_last_error_msg,
        ),
    ]
}

/// One function as the stub declares it.
fn declared(name: &[u8], parameters: Vec<Parameter>, returns: Type) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.into()),
        parameters,
        return_type: Some(returns),
        returns_by_ref: false,
        is_generator: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
        frameless: Vec::new(),
    }
}

fn parameter(name: &[u8], ty: Option<Type>) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty,
        by_ref: false,
        prefer_ref: false,
        variadic: false,
        default: None,
        attributes: Vec::new(),
    }
}

/// `ZEND_PARSE_PARAMETERS_START(min, max)`'s count check.
fn arguments_between(req: &mut Request, min: usize, max: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if (min..=max).contains(&given) {
        return Ok(());
    }
    let (word, bound) = if min == max {
        ("exactly", min)
    } else if given < min {
        ("at least", min)
    } else {
        ("at most", max)
    };
    let plural = if bound == 1 { "" } else { "s" };
    Err(arg_count_error(
        req,
        &format!("{word} {bound} argument{plural}, {given} given"),
    ))
}

/// One `Z_PARAM_STR`.
fn string_argument(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::String, position)?;
    match coerced.deref().as_php_string() {
        Some(s) => Ok(*s),
        None => PhpString::new(req, b""),
    }
}

/// One `Z_PARAM_LONG` after `Z_PARAM_OPTIONAL`: `absent` where the call
/// gave none.
fn long_argument(req: &mut Request, position: u32, absent: i64) -> Result<i64, Thrown> {
    let Some(given) = req.argument(position) else {
        return Ok(absent);
    };
    let coerced = coerce_param(req, &given, &Type::Int, position)?;
    Ok(coerced.deref().as_int().unwrap_or(0))
}

/// What `Z_PARAM_ARRAY_HT_OR_STR` reads.
#[derive(Clone, Copy)]
enum Either {
    Array(PhpArray),
    Str(PhpString),
}

/// One `Z_PARAM_ARRAY_HT_OR_STR`: the array, or the argument read as
/// `Z_PARAM_STR` reads it, both `coerce_param`'s over `array|string`.
fn either_argument(req: &mut Request, position: u32) -> Result<Either, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let ty = Type::Union(vec![Type::Array, Type::String]);
    let coerced = coerce_param(req, &given, &ty, position)?;
    match coerced.as_ref() {
        ValueRef::Array(array) => Ok(Either::Array(*array)),
        ValueRef::Str(s) => Ok(Either::Str(*s)),
        _ => Ok(Either::Str(PhpString::new(req, b"")?)),
    }
}

/// One `Z_PARAM_FUNC`.
fn callable_argument(req: &mut Request, position: u32) -> Result<Callable, Thrown> {
    match php_ext::coerce::callable_argument(req, position)? {
        Some(callable) => Ok(callable),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: a callable parameter read no callable",
        )),
    }
}

/// `ZEND_TRY_ASSIGN_REF_*` on the argument at `position`: the value
/// written through the reference the argument is, and given up where it
/// is not one.
fn assign_ref(req: &mut Request, position: u32, value: Value) {
    if let Some(given) = req.argument(position)
        && let ValueRef::Reference(reference) = given.as_ref()
    {
        let reference = *reference;
        reference.set(req, value);
        return;
    }
    req.release(value);
}

/// An ini entry of the extension read as `OnUpdateLong` reads it.
fn ini_long(req: &Request, name: &str) -> i64 {
    req.ini_get(name.as_bytes())
        .map_or(0, Request::ini_parse_bytes)
}

/// An ini entry read as `OnUpdateBool` reads it.
fn ini_bool(req: &Request, name: &str) -> bool {
    req.ini_get(name.as_bytes())
        .is_some_and(Request::ini_parse_bool)
}

/// `PCRE_G(error_code) = code`.
fn set_error(req: &mut Request, code: i64) {
    req.state::<State>().error_code = code;
}

/// `PCRE_G(error_code)`.
fn error_code(req: &mut Request) -> i64 {
    req.state::<State>().error_code
}

/// `pcre_handle_exec_error`: the library's code as php's.
fn exec_error(req: &mut Request, rc: i32) {
    let code = match rc {
        ffi::PCRE2_ERROR_MATCHLIMIT => BACKTRACK_LIMIT_ERROR,
        ffi::PCRE2_ERROR_RECURSIONLIMIT => RECURSION_LIMIT_ERROR,
        ffi::PCRE2_ERROR_BADUTFOFFSET => BAD_UTF8_OFFSET_ERROR,
        ffi::PCRE2_ERROR_JIT_STACKLIMIT => JIT_STACKLIMIT_ERROR,
        rc if (ffi::PCRE2_ERROR_UTF8_ERR21..=ffi::PCRE2_ERROR_UTF8_ERR1).contains(&rc) => {
            BAD_UTF8_ERROR
        }
        _ => INTERNAL_ERROR,
    };
    set_error(req, code);
}

/// `php_pcre_get_error_msg`.
fn error_message(code: i64) -> &'static str {
    match code {
        NO_ERROR => "No error",
        INTERNAL_ERROR => "Internal error",
        BAD_UTF8_ERROR => "Malformed UTF-8 characters, possibly incorrectly encoded",
        BAD_UTF8_OFFSET_ERROR => {
            "The offset did not correspond to the beginning of a valid UTF-8 code point"
        }
        BACKTRACK_LIMIT_ERROR => "Backtrack limit exhausted",
        RECURSION_LIMIT_ERROR => "Recursion limit exhausted",
        JIT_STACKLIMIT_ERROR => "JIT stack limit exhausted",
        _ => "Unknown error",
    }
}

/// `preg_last_error(): int`, `PCRE_G(error_code)`: what the last call
/// of the family left, `PREG_NO_ERROR` while none ran. The backtrack
/// limit is the match limit `pcre2_jit_match` honours as `pcre2_match` does;
/// the recursion limit is the interpreter's depth limit.
///
/// ```php
/// var_dump(preg_last_error(), preg_last_error_msg());
/// var_dump(preg_match('/(?:\D+|<\d+>)*[!?]/', 'foobar foobar foobar'), preg_last_error(), preg_last_error_msg());
/// ini_set('pcre.jit', '0'); ini_set('pcre.recursion_limit', '2');
/// var_dump(preg_match('/(a)+b/', 'aaaab'), preg_last_error(), preg_last_error_msg());
/// ini_set('pcre.recursion_limit', '100000'); ini_set('pcre.backtrack_limit', '10');
/// var_dump(preg_match('/(a+)+$/', str_repeat('a', 20) . 'b'), preg_last_error(), preg_last_error_msg());
/// ```
/// ```expect
/// int(0)
/// string(8) "No error"
/// bool(false)
/// int(2)
/// string(25) "Backtrack limit exhausted"
/// bool(false)
/// int(3)
/// string(25) "Recursion limit exhausted"
/// bool(false)
/// int(2)
/// string(25) "Backtrack limit exhausted"
/// ```
///
/// Bad UTF-8 under `/u`, and an offset inside a character.
///
/// ```php
/// var_dump(preg_match('/./u', "\xff"), preg_last_error(), preg_last_error_msg());
/// var_dump(preg_match('/./u', "\xc3\xa9", $m, 0, 1), preg_last_error(), preg_last_error_msg(), preg_match('/a/', 'a'), preg_last_error());
/// ```
/// ```expect
/// bool(false)
/// int(4)
/// string(56) "Malformed UTF-8 characters, possibly incorrectly encoded"
/// bool(false)
/// int(5)
/// string(74) "The offset did not correspond to the beginning of a valid UTF-8 code point"
/// int(1)
/// int(0)
/// ```
fn preg_last_error(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    Ok(Value::int(error_code(req)))
}

/// `preg_last_error_msg(): string`, `php_pcre_get_error_msg` over
/// `PCRE_G(error_code)`, beside `preg_last_error`'s witnesses: a pattern
/// php refuses is an internal error, and the next call that runs clears it.
///
/// ```php
/// var_dump(@preg_match('/(/', 'a'), preg_last_error_msg(), preg_quote('x'), preg_last_error_msg(), preg_split('/,/', 'a'), preg_last_error_msg());
/// ```
/// ```expect
/// bool(false)
/// string(14) "Internal error"
/// string(1) "x"
/// string(14) "Internal error"
/// array(1) {
///   [0]=>
///   string(1) "a"
/// }
/// string(8) "No error"
/// ```
fn preg_last_error_msg(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    let code = error_code(req);
    Ok(Value::string(PhpString::new(
        req,
        error_message(code).as_bytes(),
    )?))
}

/// `preg_quote(string $str, ?string $delimiter = null): string`: each
/// byte of `. \ + * ? [ ^ ] $ ( ) { } = ! > < | : - #` and the
/// delimiter's first byte behind a backslash, and NUL as `\000`; the
/// string itself where nothing needed quoting.
///
/// ```php
/// var_dump(preg_quote('Hello.World?(1+1=2) #x'), preg_quote('a/b', '/'), preg_quote("a\0b"), preg_quote(''), preg_quote('abc', null), preg_quote('a-b:c', 'bc'));
/// ```
/// ```expect
/// string(29) "Hello\.World\?\(1\+1\=2\) \#x"
/// string(4) "a\/b"
/// string(6) "a\000b"
/// string(0) ""
/// string(3) "abc"
/// string(8) "a\-\b\:c"
/// ```
fn preg_quote(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 1, 2)?;
    let text = string_argument(req, 1)?;
    let delimiter = match req.argument(2) {
        None => None,
        Some(given) => {
            let ty = Type::Nullable(Box::new(Type::String));
            let coerced = coerce_param(req, &given, &ty, 2)?.deref();
            coerced
                .as_php_string()
                .map(|s| s.as_bytes().first().copied().unwrap_or(0))
        }
    };
    let mut quoted = Vec::with_capacity(text.len());
    for &byte in text.as_bytes() {
        match byte {
            b'.' | b'\\' | b'+' | b'*' | b'?' | b'[' | b'^' | b']' | b'$' | b'(' | b')' | b'{'
            | b'}' | b'=' | b'!' | b'>' | b'<' | b'|' | b':' | b'-' | b'#' => {
                quoted.extend_from_slice(&[b'\\', byte]);
            }
            0 => quoted.extend_from_slice(b"\\000"),
            _ => {
                if Some(byte) == delimiter {
                    quoted.push(b'\\');
                }
                quoted.push(byte);
            }
        }
    }
    if quoted.len() == text.len() {
        return Ok(Value::string(text));
    }
    Ok(Value::string(PhpString::new(req, &quoted)?))
}

/// `PHP_MINFO_FUNCTION(pcre)`: the library's version, its Unicode
/// version and `PCRE2_CONFIG_JIT`, then `DISPLAY_INI_ENTRIES`, `pcre.jit` shown as
/// the boolean displayer shows it.
///
/// ```php
/// ob_start(); (new ReflectionExtension('pcre'))->info(); $info = ob_get_clean();
/// echo preg_replace('/(Version|Target) => .*/', '$1 => x', $info);
/// ```
/// ```expect
///
/// pcre
///
/// PCRE (Perl Compatible Regular Expressions) Support => enabled
/// PCRE Library Version => x
/// PCRE Unicode Version => x
/// PCRE JIT Support => enabled
/// PCRE JIT Target => x
///
/// Directive => Local Value => Master Value
/// pcre.backtrack_limit => 1000000 => 1000000
/// pcre.recursion_limit => 100000 => 100000
/// pcre.jit => On => On
/// ```
fn pcre_info(req: &mut Request) {
    let version = ffi::version();
    let unicode = ffi::unicode_version();
    let jit = if ffi::jit_supported() {
        "enabled"
    } else {
        "disabled"
    };
    let target = ffi::jit_target();
    let mut rows: Vec<[&str; 2]> = vec![
        [
            "PCRE (Perl Compatible Regular Expressions) Support",
            "enabled",
        ],
        ["PCRE Library Version", &version],
        ["PCRE Unicode Version", &unicode],
        ["PCRE JIT Support", jit],
    ];
    if let Some(target) = &target {
        rows.push(["PCRE JIT Target", target]);
    }
    let rows: Vec<&[&str]> = rows.iter().map(<[&str; 2]>::as_slice).collect();
    req.info_table(&rows);
    let master = req
        .engine::<pattern::Patterns>()
        .map(|patterns| patterns.master.clone());
    let cells: Vec<[String; 3]> = INI
        .iter()
        .enumerate()
        .map(|(at, (name, default))| {
            let local = req.ini_get(name.as_bytes()).unwrap_or(default.as_bytes());
            let master = master
                .as_ref()
                .map_or(default.as_bytes(), |m| m[at].as_slice());
            let boolean = *name == "pcre.jit";
            [
                (*name).to_owned(),
                shown(local, boolean),
                shown(master, boolean),
            ]
        })
        .collect();
    let mut table: Vec<&[&str]> = vec![&["Directive", "Local Value", "Master Value"]];
    let borrowed: Vec<[&str; 3]> = cells
        .iter()
        .map(|[a, b, c]| [a.as_str(), b.as_str(), c.as_str()])
        .collect();
    table.extend(borrowed.iter().map(<[&str; 3]>::as_slice));
    req.info_table(&table);
}

/// An ini value as `display_ini_entries` prints it: `no value` for
/// none, and `On` or `Off` through `zend_ini_boolean_displayer_cb`.
fn shown(value: &[u8], boolean: bool) -> String {
    if boolean {
        return if Request::ini_parse_bool(value) {
            "On"
        } else {
            "Off"
        }
        .to_owned();
    }
    if value.is_empty() {
        "no value".to_owned()
    } else {
        String::from_utf8_lossy(value).into_owned()
    }
}
