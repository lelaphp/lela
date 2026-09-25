//! The `standard` extension: `php-src/ext/standard` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/standard/tests`, through `xtask phpt --subset`.
//!
//! The natives so far, declared and paired with their entries as
//! `php_engine`'s own are, since the attributes under `php_ext::attr` hand
//! an item back unchanged until their expansion lands, in the order
//! `basic_functions.stub.php` declares them: `count` and its alias
//! `sizeof`, `ord`, `chr`, `str_repeat`, and the `is_*` family with its
//! aliases; then `trim`, `str_contains` and `substr`, the first three to
//! declare a frameless entry (`ZEND_FRAMELESS_FUNCTION`, #254), whose
//! call of a covered arity the compiler emits as `FRAMELESS_ICALL_<n>`
//! over the engine's kernel. The compiler serves a call of `count`,
//! `sizeof`, `chr`, `ord` or an `is_*` name itself where it reads the
//! name, as a constant or as `ZEND_COUNT` and `ZEND_TYPE_CHECK` over the
//! engine's kernels (`zend_try_compile_special_func`, #219), and reads it only
//! because the bundle declares the native; what reaches the native is a
//! call the compiler could not shape, which for the type checks is a
//! call of the wrong count of arguments until dynamic calls land, for
//! `count` the two-argument call, and for `chr` and `ord` an argument
//! not written as a value. The `ob_*` natives are `output`'s and the
//! header natives `head`'s, each over the engine's layer; `sleep` and
//! `constant`, the second over the engine's `zend_get_constant_ex`, are
//! `basic_functions.c`'s, `strtoupper` and `strtolower` are `string.c`'s
//! beside `trim`, `array_unique` with the `SORT_*` constants is
//! `array`'s, `print_r` and `var_export` are `var`'s, the first over the
//! renderer 8.5 keeps in Zend, `sprintf`, `printf` and `vsprintf` are
//! `formatted_print`'s over `php_formatted_print`, `base64_encode`
//! and `base64_decode` are `base64`'s, `shell_exec` is `exec`'s, the
//! one native the backtick operator compiles to, and the natives of
//! `file.c`, the directory natives of `dir.c` and `stream_get_meta_data`
//! are `file`'s, over the engine's streams facet.
// `unsafe` is `src/ffi/`'s alone, the C library calls `filestat.c` makes.
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

mod array;
mod assert;
pub mod base64;
mod basic_functions;
mod browscap;
pub mod crc32;
mod crypt;
mod csv;
mod datetime;
mod dir;
mod dl;
mod dns;
mod exec;
mod ffi;
mod file;
mod filestat;
mod filters;
mod formatted_print;
mod getopt;
mod head;
mod hrtime;
mod html;
mod http;
mod image;
mod info;
mod ini;
mod iptc;
mod levenshtein;
mod link;
mod mail;
mod math;
pub mod md5;
mod metaphone;
mod microtime;
mod net;
mod output;
mod pack;
mod pageinfo;
mod password;
mod proc_open;
pub mod quot_print;
mod scanf;
mod select;
pub mod sha1;
mod sockets;
mod soundex;
mod streamsfuncs;
mod string;
mod strnatcmp;
mod stub;
mod syslog;
mod r#type;
mod uniqid;
mod url;
mod user_filters;
mod userspace;
mod uuencode;
mod var;
mod versioning;
mod virtual_cwd;
mod zpp;

/// What `php_string.h`, `html.h` and `php_var.h` declare `PHPAPI` for the
/// extensions that include them, ext/filter's sanitizers and ext/session's
/// and ext/spl's serializers (ADR-0004; design §4.10).
pub use html::escape as escape_html_entities;
pub use string::strip_tags_ex;
pub use var::{Serializer, Unserializer};

/// What `file.h`, `php_filestat.h`, `scanf.h` and `main/php_glob.h`
/// declare `PHPAPI` for `spl_directory.c`, which calls them directly
/// rather than through the function table (ADR-0004; design §4.10).
pub use csv::fgetcsv as php_fgetcsv;
pub use dir::glob::php_glob;
pub use file::{php_fputcsv, php_fstat};
pub use filestat::{Fs, php_stat};
pub use scanf::php_sscanf_internal;

use php_ext::abi::{Callable, Dependency, DependencyKind, Extension, ExtensionBuilder};
use php_ext::call::{call, call_array, call_named, fetch_constant};
use php_ext::coerce::{coerce_param, is_type};
use php_ext::decl::{ConstDecl, ConstExpr, FunctionDecl, Name, Parameter, ParameterDefault, Type};
use php_ext::raising::{
    Level, Thrown, arg_count_error, arg_value_error, docref, too_few_arguments,
};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, TypeTag, Value};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {
    /// `DIRG(default_dir)` (`Z/ext/standard/dir.c:62`): the directory
    /// `opendir` opened last, with the owner php's `GC_ADDREF` takes,
    /// which `readdir`, `rewinddir` and `closedir` read without an
    /// argument.
    default_dir: Option<php_ext::values::Resource>,
    string: string::State,
    uniqid: uniqid::State,
    hrtime: hrtime::State,
    contexts: streamsfuncs::Contexts,
    page: Option<pageinfo::Page>,
    syslog: syslog::Log,
    browscap: browscap::Browscap,
    proc_open: proc_open::ProcState,
    user_filters: user_filters::FilterState,
}

/// The extension value (design §7.1): declarations paired with entries,
/// the state declared on it, and the `deps` table, which is what
/// `ReflectionExtension::getDependencies` reads.
///
/// ```php
/// var_dump(extension_loaded('standard'), (new ReflectionExtension('standard'))->getDependencies());
/// ```
/// ```expect
/// bool(true)
/// array(2) {
///   ["uri"]=>
///   string(8) "Required"
///   ["session"]=>
///   string(8) "Optional"
/// }
/// ```
pub fn extension() -> Extension {
    let countable = || Type::Union(vec![Type::Named(Name::from("Countable")), Type::Array]);
    let normal = ParameterDefault::Expr(ConstExpr::Constant(Name::from("COUNT_NORMAL")));
    let mode = || Parameter {
        default: normal.clone(),
        ..parameter(b"mode", Type::Int)
    };
    let directory = dir::directory_class();
    ExtensionBuilder::default()
        .functions(vec![
            (
                declared(
                    b"count",
                    &[parameter(b"value", countable()), mode()],
                    Type::Int,
                ),
                count,
            ),
            (
                declared(
                    b"sizeof",
                    &[parameter(b"value", countable()), mode()],
                    Type::Int,
                ),
                count,
            ),
            (
                declared(b"ord", &[parameter(b"character", Type::String)], Type::Int),
                ord,
            ),
            (
                declared(b"chr", &[parameter(b"codepoint", Type::Int)], Type::String),
                chr,
            ),
            (
                declared(
                    b"str_repeat",
                    &[
                        parameter(b"string", Type::String),
                        parameter(b"times", Type::Int),
                    ],
                    Type::String,
                ),
                str_repeat,
            ),
            (type_check_declared(b"is_null"), is_null),
            (type_check_declared(b"is_resource"), is_resource),
            (type_check_declared(b"is_bool"), is_bool),
            (type_check_declared(b"is_int"), is_int),
            (type_check_declared(b"is_integer"), is_int),
            (type_check_declared(b"is_long"), is_int),
            (type_check_declared(b"is_float"), is_float),
            (type_check_declared(b"is_double"), is_float),
            (type_check_declared(b"is_string"), is_string),
            (type_check_declared(b"is_array"), is_array),
            (type_check_declared(b"is_object"), is_object),
            (type_check_declared(b"is_scalar"), is_scalar),
        ])
        .functions(string_natives())
        .functions(array::natives())
        .functions(output::natives())
        .functions(head::natives())
        .functions(var::natives())
        .functions(formatted_print::natives())
        .functions(base64::natives())
        .functions(exec::natives())
        .functions(file::natives())
        .functions(filestat::natives())
        .functions(basic_natives())
        .functions(file_natives())
        .functions(math::natives())
        .functions(r#type::natives())
        .constants(vec![
            constant(b"COUNT_NORMAL", COUNT_NORMAL),
            constant(b"COUNT_RECURSIVE", COUNT_RECURSIVE),
            constant(b"STR_PAD_LEFT", STR_PAD_LEFT),
            constant(b"STR_PAD_RIGHT", STR_PAD_RIGHT),
            constant(b"STR_PAD_BOTH", STR_PAD_BOTH),
        ])
        .constants(array::constants())
        .constants(file::constants())
        .constants(file_constants())
        .constants(math::constants())
        .classes(math::classes())
        .classes(var::classes())
        .classes(user_filters::classes())
        .ini(var::ini())
        .ini(sockets::ini())
        .dependencies(dependencies())
        .class_with(directory.0, directory.1, directory.2)
        .classes(vec![assert::assertion_error_class()])
        .ini(assert::ini())
        .ini(browscap::ini())
        .request_state::<State>()
        .rshutdown(syslog::close)
        .rinit(rinit)
        .build("standard")
}

/// `PHP_RINIT(basic)`'s share here: the string filters, which php
/// registers at `module` startup and this engine keeps in the request's
/// filter table (`filters::register`).
#[allow(clippy::unnecessary_wraps)] // `ExtensionBuilder::rinit`'s signature
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    filters::register(req);
    Ok(())
}

/// The natives of the php-src files ported one file each (#655), in
/// the order the modules landed: `string.c`, `url.c`, `html.c`, the
/// digests, `pack.c`, `http.c`, the small files and `basic_functions.c`'s
/// environment and ini natives.
fn file_natives() -> Vec<(FunctionDecl, output::Entry)> {
    [
        string::natives(),
        url::natives(),
        html::natives(),
        md5::natives(),
        sha1::natives(),
        crc32::natives(),
        pack::natives(),
        http::natives(),
        quot_print::natives(),
        uuencode::natives(),
        metaphone::natives(),
        soundex::natives(),
        levenshtein::natives(),
        uniqid::natives(),
        microtime::natives(),
        hrtime::natives(),
        versioning::natives(),
        basic_functions::natives(),
        dir::natives(),
        link::natives(),
        dl::natives(),
        streamsfuncs::natives(),
        crypt::natives(),
        password::natives(),
        assert::natives(),
        pageinfo::natives(),
        syslog::natives(),
        mail::natives(),
        image::natives(),
        browscap::natives(),
        datetime::natives(),
        net::natives(),
        iptc::natives(),
        dns::natives(),
        info::natives(),
        select::natives(),
        proc_open::natives(),
        sockets::natives(),
        user_filters::natives(),
        userspace::natives(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Their constants.
fn file_constants() -> Vec<ConstDecl> {
    [
        string::constants(),
        url::constants(),
        html::constants(),
        basic_functions::constants(),
        dir::constants(),
        streamsfuncs::constants(),
        crypt::constants(),
        password::constants(),
        assert::constants(),
        syslog::constants(),
        image::constants(),
        dns::constants(),
        info::constants(),
        sockets::constants(),
        user_filters::constants(),
        userspace::constants(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// `standard_deps` (`Z/ext/standard/basic_functions.c:143`): `uri`
/// required and `session` optional, in php's order and with no `rel`
/// or `version`.
fn dependencies() -> Vec<Dependency> {
    vec![
        Dependency {
            name: String::from("uri"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        },
        Dependency {
            name: String::from("session"),
            kind: DependencyKind::Optional,
            rel: None,
            version: None,
        },
    ]
}

/// The natives of `basic_functions.c` itself declared so far, in the
/// stub's order.
fn basic_natives() -> Vec<(FunctionDecl, output::Entry)> {
    vec![
        (
            declared(
                b"constant",
                &[parameter(b"name", Type::String)],
                Type::Mixed,
            ),
            constant_of,
        ),
        (
            declared(b"sleep", &[parameter(b"seconds", Type::Int)], Type::Int),
            sleep,
        ),
        (
            declared(
                b"call_user_func",
                &[
                    parameter(b"callback", Type::Callable),
                    Parameter {
                        variadic: true,
                        ..parameter(b"args", Type::Mixed)
                    },
                ],
                Type::Mixed,
            ),
            call_user_func,
        ),
        (
            declared(
                b"call_user_func_array",
                &[
                    parameter(b"callback", Type::Callable),
                    parameter(b"args", Type::Array),
                ],
                Type::Mixed,
            ),
            call_user_func_array,
        ),
        (
            declared(
                b"forward_static_call",
                &[
                    parameter(b"callback", Type::Callable),
                    Parameter {
                        variadic: true,
                        ..parameter(b"args", Type::Mixed)
                    },
                ],
                Type::Mixed,
            ),
            forward_static_call,
        ),
        (
            declared(
                b"forward_static_call_array",
                &[
                    parameter(b"callback", Type::Callable),
                    parameter(b"args", Type::Array),
                ],
                Type::Mixed,
            ),
            forward_static_call_array,
        ),
    ]
}

/// The natives of `string.c` declared so far, each with the frameless
/// entries `basic_functions_arginfo.h` gives it.
fn string_natives() -> Vec<(FunctionDecl, output::Entry)> {
    vec![
        (
            frameless(
                declared(
                    b"trim",
                    &[
                        parameter(b"string", Type::String),
                        Parameter {
                            default: ParameterDefault::Expr(ConstExpr::Str(TRIM_DEFAULT.into())),
                            ..parameter(b"characters", Type::String)
                        },
                    ],
                    Type::String,
                ),
                &[1, 2],
            ),
            trim,
        ),
        (
            frameless(
                declared(
                    b"str_contains",
                    &[
                        parameter(b"haystack", Type::String),
                        parameter(b"needle", Type::String),
                    ],
                    Type::Bool,
                ),
                &[2],
            ),
            str_contains,
        ),
        (
            declared(
                b"strtoupper",
                &[parameter(b"string", Type::String)],
                Type::String,
            ),
            strtoupper,
        ),
        (
            declared(
                b"strtolower",
                &[parameter(b"string", Type::String)],
                Type::String,
            ),
            strtolower,
        ),
        (
            declared(
                b"str_pad",
                &[
                    parameter(b"string", Type::String),
                    parameter(b"length", Type::Int),
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Str(b" ".as_slice().into())),
                        ..parameter(b"pad_string", Type::String)
                    },
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Constant(Name(
                            b"STR_PAD_RIGHT".as_slice().into(),
                        ))),
                        ..parameter(b"pad_type", Type::Int)
                    },
                ],
                Type::String,
            ),
            str_pad,
        ),
        (
            frameless(
                declared(
                    b"substr",
                    &[
                        parameter(b"string", Type::String),
                        parameter(b"offset", Type::Int),
                        Parameter {
                            default: ParameterDefault::Expr(ConstExpr::Null),
                            ..parameter(b"length", Type::Nullable(Box::new(Type::Int)))
                        },
                    ],
                    Type::String,
                ),
                &[2, 3],
            ),
            substr,
        ),
    ]
}

/// `PHP_COUNT_NORMAL` and `PHP_COUNT_RECURSIVE`, what `count`'s
/// `$mode` is one of.
const COUNT_NORMAL: i64 = 0;
const COUNT_RECURSIVE: i64 = 1;

/// `count(Countable|array $value, int $mode = COUNT_NORMAL): int` and
/// its alias `sizeof`, php's `PHP_FUNCTION(count)`
/// (`Z/ext/standard/array.c:648`): the one-argument call is `ZEND_COUNT`
/// over the engine's kernel and never reaches here, so what does is the
/// call with a `$mode`. One that is neither constant is php's
/// `ValueError`, raised before the value is looked at; then the kernel's
/// arm through the face, `php_ext::coerce::count`, which is the element
/// count or the `TypeError`, and under `COUNT_RECURSIVE` the count of
/// every nested array added in (`php_count_recursive`), a reference
/// followed to what it holds. The `TypeError` names the function as it
/// was called, `sizeof` for the alias, since php reads the name off the
/// active function.
///
/// ```php
/// var_dump(count(token_get_all("<?php 1;"), COUNT_RECURSIVE), sizeof(token_get_all("<?php 1;"), 1), count(token_get_all("<?php 1;"), 0), count(token_get_all("<?php 1;"), true), count(token_get_all("<?php 1;"), "1"), count(token_get_all("<?php 1;"), null));
/// ```
/// ```expect
/// Deprecated: count(): Passing null to parameter #2 ($mode) of type int is deprecated in %s on line 2
/// int(9)
/// int(9)
/// int(3)
/// int(9)
/// int(9)
/// int(3)
/// ```
///
/// ```php
/// var_dump(count("x", 2));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ValueError: count(): Argument #2 ($mode) must be either COUNT_NORMAL or COUNT_RECURSIVE in %s:2
/// Stack trace:
/// #0 %s(2): count('x', 2)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// var_dump(sizeof(false, 0));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: sizeof(): Argument #1 ($value) must be of type Countable|array, false given in %s:2
/// Stack trace:
/// #0 %s(2): sizeof(false, 0)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn count(req: &mut Request) -> Result<Value, Thrown> {
    let given = req.argument_count();
    if !(1..=2).contains(&given) {
        return Err(arg_count_error(
            req,
            &format!(
                "{} {} given",
                if given < 1 {
                    "at least 1 argument,"
                } else {
                    "at most 2 arguments,"
                },
                given
            ),
        ));
    }
    let value = req.argument(1).unwrap_or_default();
    let mode = if given == 2 {
        long_argument(req, 2)?
    } else {
        COUNT_NORMAL
    };
    if mode != COUNT_NORMAL && mode != COUNT_RECURSIVE {
        return Err(arg_value_error(
            req,
            2,
            "be either COUNT_NORMAL or COUNT_RECURSIVE",
        ));
    }
    match value.deref().as_array() {
        Some(array) if mode == COUNT_RECURSIVE => Ok(Value::int(count_recursive(req, *array))),
        _ => php_ext::coerce::count(req, &value).map(Value::int),
    }
}

/// `php_count_recursive`: the elements, and every nested array's
/// elements counted the same way, a reference followed to what it
/// holds, and the walk guarded as php guards it, so an array that
/// holds itself is the warning `Recursion detected` and a zero where
/// the walk meets itself again.
fn count_recursive(req: &mut Request, array: PhpArray) -> i64 {
    if array.is_recursive() {
        docref(req, Level::Warning, "Recursion detected");
        return 0;
    }
    array.protect();
    let mut count = i64::try_from(array.len()).unwrap_or(i64::MAX);
    for (_, element) in &array {
        if let Some(nested) = element.deref().as_array() {
            count = count.saturating_add(count_recursive(req, *nested));
        }
    }
    array.unprotect();
    count
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
fn ord(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, count));
    }
    let string = string_argument(req, 1)?;
    let bytes = string.as_bytes();
    if bytes.len() != 1 {
        let what = if bytes.is_empty() {
            "Providing an empty string is deprecated"
        } else {
            "Providing a string that is not one byte long is deprecated. Use ord($str[0]) instead"
        };
        docref_deprecated(req, what);
    }
    Ok(Value::int(i64::from(bytes.first().copied().unwrap_or(0))))
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
fn chr(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, count));
    }
    let codepoint = long_argument(req, 1)?;
    if !(0..=255).contains(&codepoint) {
        docref_deprecated(
            req,
            "Providing a value not in-between 0 and 255 is deprecated, this is because a byte value must be in the [0, 255] interval. The value used will be constrained using % 256",
        );
    }
    // `c &= 0xff`, then `RETURN_CHAR(c)`.
    let byte = (codepoint & 0xff).to_le_bytes()[0];
    Ok(Value::string(PhpString::new(req, &[byte])?))
}

/// `call_user_func(callable $callback, mixed ...$args): mixed`, php's
/// `PHP_FUNCTION(call_user_func)` (`Z/ext/standard/basic_functions.c:1473`):
/// `Z_PARAM_FUNC` and then `Z_PARAM_VARIADIC_WITH_NAMED`, the
/// positional arguments and the extra named ones off this call's own
/// frame, handed to `zend_call_function` as `fci.params` and
/// `fci.named_params`, which is the engine's `call_named`. The callee's
/// return is this call's. php's compiler serves the spelling
/// `call_user_func(...)` as a dynamic call of the callback itself
/// (`zend_compile_func_cuf`) and this native is what the spelling
/// `$f('call_user_func')` and every other reaches; this compiler serves
/// every spelling through the native, and what differs is the frame a
/// trace prints for a raise at the boundary, so the witnesses that pin
/// a trace spell the call the way the oracle serves through the native
/// too.
///
/// ```php
/// function greet($who, $punct = '!') { return "hi $who$punct"; }
/// class W { static function s($x) { return "s:$x"; } function i($x) { return "i:$x"; } }
/// var_dump(call_user_func('strtoupper', 'abc'), call_user_func('greet', 'bob'), call_user_func(['W', 's'], 1), call_user_func([new W, 'i'], 2), call_user_func(fn($n) => $n * 2, 21), call_user_func('W::s', 3));
/// ```
/// ```expect
/// string(3) "ABC"
/// string(7) "hi bob!"
/// string(3) "s:1"
/// string(3) "i:2"
/// int(42)
/// string(3) "s:3"
/// ```
///
/// The declarations, as `basic_functions.stub.php` has them.
///
/// ```php
/// echo new ReflectionFunction('call_user_func'), new ReflectionFunction('call_user_func_array');
/// ```
/// ```expect
/// Function [ <internal:standard> function call_user_func ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> callable $callback ]
///     Parameter #1 [ <optional> mixed ...$args ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function call_user_func_array ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> callable $callback ]
///     Parameter #1 [ <required> array $args ]
///   }
///   - Return [ mixed ]
/// }
/// ```
///
/// A callback that is none is `Z_PARAM_FUNC`'s `TypeError`, with the
/// sentence `zend_is_callable_ex` wrote for the miss; a call with no
/// callback at all is the arity's `ArgumentCountError`.
///
/// ```php
/// $f = 'call_user_func'; $f('nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: call_user_func(): Argument #1 ($callback) must be a valid callback, function "nope" not found or invalid function name in %s:2
/// Stack trace:
/// #0 %s(2): call_user_func('nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// call_user_func();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: call_user_func() expects at least 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): call_user_func()
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// The extra named arguments this call carries are the callee's named
/// ones: `call_user_func_array` places `length` on this native's frame,
/// since `call_user_func` declares no parameter of that name and
/// collects into a variadic, and this native hands it on to `str_pad`,
/// where it lands at the parameter of that name. A trace prints them
/// after the positional arguments, `name: value`, on the frame that
/// holds them.
///
/// ```php
/// $f = 'call_user_func_array';
/// var_dump($f('call_user_func', ['str_pad', 'abc', 'length' => 6, 'pad_string' => '-']));
/// $f('call_user_func', ['str_pad', 'abc', 'pad_type' => 0, 'pad_string' => ' ', 'length' => 'x']);
/// ```
/// ```expect exit=255
/// string(6) "abc---"
///
/// Fatal error: Uncaught TypeError: str_pad(): Argument #2 ($length) must be of type int, string given in %s:4
/// Stack trace:
/// #0 [internal function]: str_pad('abc', 'x', ' ', 0)
/// #1 [internal function]: call_user_func('str_pad', 'abc', pad_type: 0, pad_string: ' ', length: 'x')
/// #2 %s(4): call_user_func_array('call_user_func', Array)
/// #3 {main}
///   thrown in %s on line 4
/// ```
fn call_user_func(req: &mut Request) -> Result<Value, Thrown> {
    if req.argument_count() < 1 {
        return Err(too_few_arguments(req, 1, 0));
    }
    let callback = callable_argument(req, 1)?;
    let arguments: Vec<Value> = (2..=req.argument_count())
        .filter_map(|position| u32::try_from(position).ok())
        .filter_map(|position| req.argument(position))
        .collect();
    let named = req.named_arguments();
    call_named(req, &callback, &arguments, &named)
}

/// `call_user_func_array(callable $callback, array $args): mixed`, php's
/// `PHP_FUNCTION(call_user_func_array)`
/// (`Z/ext/standard/basic_functions.c:1497`): `Z_PARAM_FUNC` and
/// `Z_PARAM_ARRAY_HT`, then `zend_call_function` with the array as
/// `fci.named_params` and nothing in `fci.params`, which is the
/// engine's `call_array`: an integer-keyed element is the next
/// positional argument whatever its key, and a string-keyed one is
/// placed at the parameter of that name by `zend_handle_named_arg`,
/// the slots between left for the callee. php's compiler serves the
/// spelling `call_user_func_array(...)` as `SEND_ARRAY` on a dynamic
/// call (`zend_compile_func_cufa`), so a raise at the boundary prints
/// one frame fewer there than through this native; the witnesses that
/// pin a trace spell the call through a variable, which the oracle
/// serves through the native too.
///
/// ```php
/// function greet($who, $punct = '!') { return "hi $who$punct"; }
/// var_dump(call_user_func_array('str_pad', ['abc', 'length' => 6]), call_user_func_array('str_pad', [1 => 'abc', 5 => 6, 'pad_string' => '-', 'pad_type' => STR_PAD_LEFT]), call_user_func_array('greet', ['punct' => '?', 'who' => 'ann']), call_user_func_array('strtoupper', ['abc']));
/// ```
/// ```expect
/// string(6) "abc   "
/// string(6) "---abc"
/// string(7) "hi ann?"
/// string(3) "ABC"
/// ```
///
/// A name the callee declares no parameter for, and a name whose slot
/// a positional argument filled, are `zend_handle_named_arg`'s two
/// `Error`s, raised before the callee is entered, so the trace ends at
/// this native's frame; a positional element after a named one is
/// `zend_call_function`'s own, raised where the walk meets it, after
/// an earlier name's.
///
/// ```php
/// try { call_user_func_array('str_pad', ['abc', 'nope' => 6]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { call_user_func_array('str_pad', ['abc', 6, 'length' => 7]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { call_user_func_array('str_pad', ['abc', 'length' => 6, 7]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { call_user_func_array('str_pad', ['x' => 1, 2]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// function f($a) {} try { call_user_func_array('f', ['b' => 1]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Error: Unknown named parameter $nope
/// Error: Named parameter $length overwrites previous argument
/// Error: Cannot use positional argument after named argument
/// Error: Unknown named parameter $x
/// Error: Unknown named parameter $b
/// ```
///
/// ```php
/// $f = 'call_user_func_array'; $f('str_pad', ['abc', 'nope' => 6]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Unknown named parameter $nope in %s:2
/// Stack trace:
/// #0 %s(2): call_user_func_array('str_pad', Array)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// $f = 'call_user_func_array'; $f('str_pad', ['abc', 6, 'length' => 7]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Named parameter $length overwrites previous argument in %s:2
/// Stack trace:
/// #0 %s(2): call_user_func_array('str_pad', Array)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// A name a variadic callee does not declare rides on the callee's
/// frame as an extra named argument (`ZEND_CALL_HAS_EXTRA_NAMED_PARAMS`):
/// a user function's `RECV_VARIADIC` takes it into the variadic's array
/// under its name, after the positional extras, held to the declared
/// type under the number after the last positional argument; and a
/// trace prints it `name: value` after the positional arguments.
///
/// ```php
/// function v($a, int ...$rest) { var_dump($rest); }
/// call_user_func_array('v', [1, 2, 'x' => "3", 'y' => 4]);
/// call_user_func_array('v', ['y' => 4, 'a' => 1]);
/// function w(...$rest) { throw new Exception('x'); }
/// $f = 'call_user_func_array'; $f('w', [1, 'x' => 5, 'y' => 'str']);
/// ```
/// ```expect exit=255
/// array(3) {
///   [0]=>
///   int(2)
///   ["x"]=>
///   int(3)
///   ["y"]=>
///   int(4)
/// }
/// array(1) {
///   ["y"]=>
///   int(4)
/// }
///
/// Fatal error: Uncaught Exception: x in %s:5
/// Stack trace:
/// #0 [internal function]: w(1, x: 5, y: 'str')
/// #1 %s(6): call_user_func_array('w', Array)
/// #2 {main}
///   thrown in %s on line 5
/// ```
///
/// ```php
/// function v(int ...$rest) {}
/// $f = 'call_user_func_array';
/// try { $f('v', ['x' => 1, 'y' => 'a']); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $f('v', [1, 'x' => 'a']);
/// ```
/// ```expect exit=255
/// v(): Argument #1 must be of type int, string given
///
/// Fatal error: Uncaught TypeError: v(): Argument #2 must be of type int, string given in %s:2
/// Stack trace:
/// #0 [internal function]: v(1, x: 'a')
/// #1 %s(5): call_user_func_array('v', Array)
/// #2 {main}
///   thrown in %s on line 2
/// ```
///
/// A variadic native meets them where its parameter parsing reads the
/// variadic, `Z_PARAM_VARIADIC`, and that is
/// `zend_unexpected_extra_named_error`'s `ArgumentCountError`, raised
/// inside the callee's frame after the checks before it: the arity's
/// own error comes first.
///
/// ```php
/// try { call_user_func_array('array_merge', ['a' => [1]]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { call_user_func_array('sprintf', ['%d', 'a' => 1]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { call_user_func_array('sprintf', ['a' => 1]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// ArgumentCountError: array_merge() does not accept unknown named parameters
/// ArgumentCountError: sprintf() does not accept unknown named parameters
/// ArgumentCountError: sprintf() expects at least 1 argument, 0 given
/// ```
///
/// ```php
/// $f = 'call_user_func_array'; $f('array_merge', [[1], 'a' => [1]]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: array_merge() does not accept unknown named parameters in %s:2
/// Stack trace:
/// #0 [internal function]: array_merge(Array, a: Array)
/// #1 %s(2): call_user_func_array('array_merge', Array)
/// #2 {main}
///   thrown in %s on line 2
/// ```
///
/// The two parameters' refusals.
///
/// ```php
/// $f = 'call_user_func_array'; $f('strlen', 'x');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: call_user_func_array(): Argument #2 ($args) must be of type array, string given in %s:2
/// Stack trace:
/// #0 %s(2): call_user_func_array('strlen', 'x')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// $f = 'call_user_func_array'; $f('strlen');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: call_user_func_array() expects exactly 2 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): call_user_func_array('strlen')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn call_user_func_array(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let callback = callable_argument(req, 1)?;
    let given = req.argument(2).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::Array, 2)?;
    let array = match coerced.deref().as_array().copied() {
        Some(array) => array,
        None => PhpArray::new(req)?,
    };
    call_array(req, &callback, array)
}

/// `forward_static_call(callable $callback, mixed ...$args): mixed`,
/// php's `PHP_FUNCTION(forward_static_call)`
/// (`Z/ext/standard/basic_functions.c:1522`): `call_user_func` from
/// inside a class, the callee served with the caller's called scope as
/// its own where that scope is the callee's class or extends it
/// ([`Callable::with_called_scope`]). Outside a class scope it is
/// php's `Error`, raised before the callee is entered.
///
/// ```php
/// class A { const NAME = 'A'; public static function test() { echo static::NAME, ' ', implode(',', func_get_args()), "\n"; } }
/// class B extends A { const NAME = 'B'; public static function test() { forward_static_call(['A', 'test'], 'more', 'args'); forward_static_call('printf', "%s\n", 'fn'); } }
/// B::test();
/// function outside() { forward_static_call(['A', 'test']); }
/// try { outside(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// B more,args
/// fn
/// Cannot call forward_static_call() when no class scope is active
/// ```
fn forward_static_call(req: &mut Request) -> Result<Value, Thrown> {
    if req.argument_count() < 1 {
        return Err(too_few_arguments(req, 1, 0));
    }
    let callback = callable_argument(req, 1)?;
    let arguments = req.variadic_arguments(2)?;
    if req.scope().is_none() {
        return Err(php_ext::raising::throw(
            req,
            "Error",
            "Cannot call forward_static_call() when no class scope is active",
        ));
    }
    let callback = forwarded(req, &callback);
    call(req, &callback, &arguments)
}

/// `forward_static_call_array(callable $callback, array $args): mixed`,
/// php's `PHP_FUNCTION(forward_static_call_array)`: the array handed
/// over as `call_user_func_array` hands it, `fci.named_params`, with the
/// called scope forwarded as [`forward_static_call`] forwards it and no
/// class scope asked for.
///
/// ```php
/// class A { public static function who($a, $b = 'b') { echo static::class, " $a $b\n"; } }
/// class B extends A { public static function test() { forward_static_call_array(['A', 'who'], ['x', 'b' => 'y']); } }
/// B::test();
/// forward_static_call_array(['A', 'who'], ['z']);
/// try { forward_static_call_array(['A', 'who'], ['a' => 1, 2]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// B x y
/// A z b
/// Cannot use positional argument after named argument
/// ```
fn forward_static_call_array(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let callback = callable_argument(req, 1)?;
    let array = zpp::array_argument(req, 2)?;
    let callback = forwarded(req, &callback);
    call_array(req, &callback, array)
}

/// The callable with the called scope of the native being served
/// forwarded to it, `zend_get_called_scope(execute_data)`.
fn forwarded(req: &Request, callback: &Callable) -> Callable {
    match req.called_scope() {
        Some(called) => callback.with_called_scope(req, called),
        None => callback.clone(),
    }
}

/// One `Z_PARAM_FUNC` of a native, php's `"F"`: the argument at
/// `position` resolved into the callee it names, `zend_parse_arg_func`
/// over `zend_is_callable_ex`, so a callback is held to being one where
/// it is taken and not where it is served. The sentence a miss takes is
/// php's `*error` for that miss, the one `zend_is_callable_ex` wrote.
fn callable_argument(req: &mut Request, position: u32) -> Result<Callable, Thrown> {
    match php_ext::coerce::callable_argument(req, position)? {
        Some(callable) => Ok(callable),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: a callable parameter read no callable",
        )),
    }
}

/// `php_error_docref(NULL, E_DEPRECATED, ...)`.
fn docref_deprecated(req: &mut Request, what: &str) {
    docref(req, Level::Deprecated, what);
}

/// `is_null(mixed $value): bool`, `php_is_type` under `IS_NULL`
/// (`Z/ext/standard/type.c:239`) through the face's `is_type`. The
/// one-argument call is `ZEND_TYPE_CHECK` over the engine's kernel and
/// never reaches here, as with every native of the family; what does is
/// the call `ZEND_PARSE_PARAMETERS_START(1, 1)` refuses.
///
/// ```php
/// var_dump(is_null());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_null() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_null()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_null(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Null)
}

/// `is_resource(mixed $value): bool`, `php_is_type` under `IS_RESOURCE`:
/// a closed resource is not one.
///
/// ```php
/// var_dump(is_resource());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_resource() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_resource()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_resource(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Resource)
}

/// `is_bool(mixed $value): bool`, `PHP_FUNCTION(is_bool)`: `IS_FALSE`
/// or `IS_TRUE`.
///
/// ```php
/// var_dump(is_bool());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_bool() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_bool()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_bool(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Bool)
}

/// `is_int(mixed $value): bool` and its aliases `is_integer` and
/// `is_long`, `php_is_type` under `IS_LONG`.
///
/// ```php
/// var_dump(is_int());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_int() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_int()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_int(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Int)
}

/// `is_float(mixed $value): bool` and its alias `is_double`,
/// `php_is_type` under `IS_DOUBLE`.
///
/// ```php
/// var_dump(is_float());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_float() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_float()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_float(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Float)
}

/// `is_string(mixed $value): bool`, `php_is_type` under `IS_STRING`.
///
/// ```php
/// var_dump(is_string());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_string() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_string()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_string(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::String)
}

/// `is_array(mixed $value): bool`, `php_is_type` under `IS_ARRAY`.
///
/// ```php
/// var_dump(is_array());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_array() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_array()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_array(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Array)
}

/// `is_object(mixed $value): bool`, `php_is_type` under `IS_OBJECT`.
///
/// ```php
/// var_dump(is_object());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_object() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_object()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_object(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Object)
}

/// `is_scalar(mixed $value): bool`, `PHP_FUNCTION(is_scalar)`
/// (`Z/ext/standard/type.c:389`): a bool, a long, a double or a string,
/// which is the face's `is_type` asked four times.
///
/// ```php
/// var_dump(is_scalar(1, 2));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_scalar() expects exactly 1 argument, 2 given in %s:2
/// Stack trace:
/// #0 %s(2): is_scalar(1, 2)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_scalar(req: &mut Request) -> Result<Value, Thrown> {
    let value = one_argument(req)?;
    Ok(Value::bool(
        [TypeTag::Bool, TypeTag::Int, TypeTag::Float, TypeTag::String]
            .into_iter()
            .any(|tag| is_type(&value, tag)),
    ))
}

/// `php_is_type`: the one `Z_PARAM_ZVAL` and whether it is of `tag`.
fn type_check(req: &mut Request, tag: TypeTag) -> Result<Value, Thrown> {
    let value = one_argument(req)?;
    Ok(Value::bool(is_type(&value, tag)))
}

/// `ZEND_PARSE_PARAMETERS_START(1, 1)` over one `Z_PARAM_ZVAL`: the
/// argument as it is, or the `ArgumentCountError` for any other count.
fn one_argument(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, count));
    }
    Ok(req.argument(1).unwrap_or_default())
}

/// `constant(string $name): mixed`, php's `PHP_FUNCTION(constant)`
/// (`Z/ext/standard/basic_functions.c:501`): one `Z_PARAM_STR`, then
/// `zend_get_constant_ex` under `ZEND_FETCH_CLASS_EXCEPTION` with the
/// executed scope, which is the engine's `fetch_constant`, and its
/// answer one more owner (`ZVAL_COPY_OR_DUP`). Every arm of the name is
/// the engine's: a leading `\` dropped, a `::` read through the class
/// with `self`, `parent` and `static` against the scopes, a namespace
/// lowered, the table and then `true`, `false` and `null` in any case.
/// What `define()` wrote is what this reads back.
///
/// ```php
/// echo new ReflectionFunction('constant');
/// ```
/// ```expect
/// Function [ <internal:standard> function constant ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $name ]
///   }
///   - Return [ mixed ]
/// }
/// ```
///
/// ```php
/// define("DEMO", 1); class E { const C = 3; } class A { const X = 1; private const P = 2; }
/// var_dump(constant("DEMO"), constant("\\PHP_EOL"), constant("E::C"), constant("a::X"), constant("\\A::X"), constant("TRUE"), constant("nUlL"), constant("E_ALL"), constant("\\E_STRICT"));
/// ```
/// ```expect
/// Deprecated: Constant E_STRICT is deprecated since 8.4, the error level was removed in %s on line 3
/// int(1)
/// string(1) "
/// "
/// int(3)
/// int(1)
/// int(1)
/// bool(true)
/// NULL
/// int(30719)
/// int(2048)
/// ```
///
/// The refusals, each `zend_get_constant_ex`'s or `zend_fetch_class`'s
/// `Error`, the class named as the string spelled it.
///
/// ```php
/// class A { const X = 1; private const P = 2; }
/// foreach (["a::Y", "A::P", "Nope::X", "self::X", "parent::X", "static::X", "foo\\BAR", "", "::X", "A::", "\\"] as $n) { try { constant($n); } catch (Error $e) { echo $e->getMessage(), "\n"; } }
/// ```
/// ```expect
/// Undefined constant a::Y
/// Cannot access private constant A::P
/// Class "Nope" not found
/// Cannot access "self" when no class scope is active
/// Cannot access "parent" when no class scope is active
/// Cannot access "static" when no class scope is active
/// Undefined constant "foo\BAR"
/// Undefined constant ""
/// Class "" not found
/// Undefined constant A::
/// Undefined constant ""
/// ```
///
/// `self`, `parent` and `static` are read against the scope of the
/// method that called this native, `zend_get_executed_scope` walking
/// past the native's own frame, so a private constant of the scope is
/// reachable and `static` is the called class.
///
/// ```php
/// class M { const K = 7; private const P = 8; function f() { return constant("self::P"); } static function g() { return constant("static::K"); } }
/// class N extends M { const K = 9; function p() { return [constant("parent::K"), constant("self::K"), constant("static::K")]; } }
/// var_dump((new M)->f(), N::g(), (new N)->p());
/// ```
/// ```expect
/// int(8)
/// int(9)
/// array(3) {
///   [0]=>
///   int(7)
///   [1]=>
///   int(9)
///   [2]=>
///   int(9)
/// }
/// ```
///
/// ```php
/// constant("NOPE");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Undefined constant "NOPE" in %s:2
/// Stack trace:
/// #0 %s(2): constant('NOPE')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// var_dump(constant(null));
/// ```
/// ```expect exit=255
/// Deprecated: constant(): Passing null to parameter #1 ($name) of type string is deprecated in %s on line 2
///
/// Fatal error: Uncaught Error: Undefined constant "" in %s:2
/// Stack trace:
/// #0 %s(2): constant('')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// constant("A", "B");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: constant() expects exactly 1 argument, 2 given in %s:2
/// Stack trace:
/// #0 %s(2): constant('A', 'B')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn constant_of(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let name = string_argument(req, 1)?;
    fetch_constant(req, name.as_bytes())
}

/// `sleep(int $seconds): int`, php's `PHP_FUNCTION(sleep)`
/// (`Z/ext/standard/basic_functions.c:1122`): php's `ValueError` for a
/// negative count, then `php_sleep`, which is the CLI's `sleep(3)`, and
/// its answer, the seconds left unslept, zero when nothing woke it.
///
/// ```php
/// var_dump(sleep(0), sleep("0"), sleep(false));
/// ```
/// ```expect
/// int(0)
/// int(0)
/// int(0)
/// ```
///
/// ```php
/// sleep(-1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ValueError: sleep(): Argument #1 ($seconds) must be greater than or equal to 0 in %s:2
/// Stack trace:
/// #0 %s(2): sleep(-1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn sleep(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let seconds = long_argument(req, 1)?;
    let Ok(seconds) = u64::try_from(seconds) else {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    };
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    Ok(Value::int(0))
}

/// `is_*(mixed $value): bool`, the declaration every native of the
/// family shares.
fn type_check_declared(name: &[u8]) -> FunctionDecl {
    declared(name, &[parameter(b"value", Type::Mixed)], Type::Bool)
}

/// One `REGISTER_LONG_CONSTANT`.
fn constant(name: &[u8], value: i64) -> ConstDecl {
    ConstDecl {
        name: Name(name.into()),
        value: ConstExpr::Long(value),
        attributes: Vec::new(),
    }
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
fn str_repeat(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 2 {
        return Err(wrong_argument_count(req, 2, count));
    }
    let string = string_argument(req, 1)?;
    let times = long_argument(req, 2)?;
    if times < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    if string.is_empty() || times == 0 {
        return Ok(Value::string(PhpString::new(req, b"")?));
    }
    let times = usize::try_from(times).unwrap_or(usize::MAX);
    let pattern = string.as_bytes();
    // `zend_string_safe_alloc(ZSTR_LEN(input_str), mult, 0, 0)`, then
    // php's two fills: one byte `memset`, else `memcpy` of the pattern
    // and then of the result onto itself, doubling what is laid down.
    let repeated = PhpString::product(req, pattern.len(), times, 0, |result| {
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
    })?;
    Ok(Value::string(repeated))
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
fn strtoupper(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
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
fn strtolower(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let string = string_argument(req, 1)?;
    cased(req, string, u8::to_ascii_lowercase)
}

/// `PHP_STR_PAD_LEFT`, `PHP_STR_PAD_RIGHT` and `PHP_STR_PAD_BOTH`
/// (`Z/ext/standard/php_string.h`), what `str_pad`'s `$pad_type` is one
/// of.
const STR_PAD_LEFT: i64 = 0;
const STR_PAD_RIGHT: i64 = 1;
const STR_PAD_BOTH: i64 = 2;

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
fn str_pad(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 4)?;
    let given = req.argument_count();
    let input = string_argument(req, 1)?;
    let length = long_argument(req, 2)?;
    let pad = if given >= 3 {
        string_argument(req, 3)?.as_bytes().to_vec()
    } else {
        vec![b' ']
    };
    let pad_type = if given >= 4 {
        long_argument(req, 4)?
    } else {
        STR_PAD_RIGHT
    };
    let bytes = input.as_bytes();
    if length < 0 || usize::try_from(length).is_ok_and(|length| length <= bytes.len()) {
        let value = Value::string(input);
        req.addref(&value);
        return Ok(value);
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
    let padded = PhpString::product(req, 1, bytes.len(), num_pad_chars, |result| {
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
    })?;
    Ok(Value::string(padded))
}

/// `zend_string_toupper_ex` and `zend_string_tolower_ex`'s one shape
/// over `map`: the argument with one more owner when `map` changes no
/// byte, else a new string of the request's with the bytes before the
/// first changed one copied and every byte from it mapped.
fn cased(req: &mut Request, string: PhpString, map: fn(&u8) -> u8) -> Result<Value, Thrown> {
    let bytes = string.as_bytes();
    let Some(first) = bytes.iter().position(|b| map(b) != *b) else {
        let value = Value::string(string);
        req.addref(&value);
        return Ok(value);
    };
    let mapped = PhpString::filled(req, bytes.len(), |out| {
        out[..first].copy_from_slice(&bytes[..first]);
        for (o, b) in out[first..].iter_mut().zip(&bytes[first..]) {
            *o = map(b);
        }
    })?;
    Ok(Value::string(mapped))
}

/// The characters `trim` strips when given none, `php_trim_int`'s
/// default set as `basic_functions.stub.php` spells it.
const TRIM_DEFAULT: &[u8] = b" \n\r\t\x0B\x00";

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
fn trim(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let string = string_argument(req, 1)?;
    let what = if req.argument_count() == 2 {
        Some(string_argument(req, 2)?)
    } else {
        None
    };
    let bytes = string.as_bytes();
    let (start, end) = match what {
        Some(what) if what.len() == 1 => {
            let p = what.as_bytes()[0];
            trimmed(bytes, |c| c == p)
        }
        Some(what) => {
            let mask = charmask(req, what.as_bytes());
            trimmed(bytes, |c| mask[c as usize])
        }
        None => trimmed(bytes, |c| {
            c <= b' ' && matches!(c, b' ' | b'\n' | b'\r' | b'\t' | 0x0B | 0)
        }),
    };
    if end - start == bytes.len() {
        let value = Value::string(string);
        req.addref(&value);
        return Ok(value);
    }
    Ok(Value::string(PhpString::new(req, &bytes[start..end])?))
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
fn charmask(req: &mut Request, input: &[u8]) -> [bool; 256] {
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
fn str_contains(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let haystack = string_argument(req, 1)?;
    let needle = string_argument(req, 2)?;
    Ok(Value::bool(memnstr(haystack.as_bytes(), needle.as_bytes())))
}

/// `php_memnstr`, `zend_memnstr` (`Z/Zend/zend_operators.h`): whether
/// `needle` stands in `haystack`; an empty needle is found at once.
fn memnstr(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
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
fn substr(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 3)?;
    let string = string_argument(req, 1)?;
    let mut from = long_argument(req, 2)?;
    let length = if req.argument_count() == 3 {
        let given = req.argument(3).unwrap_or_default();
        let coerced = coerce_param(req, &given, &Type::Nullable(Box::new(Type::Int)), 3)?;
        coerced.as_int()
    } else {
        None
    };
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
        return Ok(Value::string(PhpString::new(req, b"")?));
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
        let value = Value::string(string);
        req.addref(&value);
        return Ok(value);
    }
    let start = usize::try_from(from).unwrap_or(0);
    let end = start + usize::try_from(taken).unwrap_or(0);
    Ok(Value::string(PhpString::new(
        req,
        &string.as_bytes()[start..end],
    )?))
}

/// `ZEND_PARSE_PARAMETERS_START(min, max)`'s count check: nothing, or
/// `zend_wrong_parameters_count_error`'s sentence for a count outside
/// the range, `exactly` where the two are one and `at least` or `at
/// most` by which side the count fell.
fn argument_count_between(req: &mut Request, min: usize, max: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if (min..=max).contains(&given) {
        return Ok(());
    }
    if min == max {
        return Err(wrong_argument_count(req, min, given));
    }
    let (word, bound) = if given < min {
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

/// `zend_wrong_parameters_count_error` for a native whose minimum is its
/// maximum: `exactly N arguments, M given`.
fn wrong_argument_count(req: &mut Request, exactly: usize, given: usize) -> Thrown {
    let word = if exactly == 1 {
        "argument"
    } else {
        "arguments"
    };
    arg_count_error(req, &format!("exactly {exactly} {word}, {given} given"))
}

/// One `Z_PARAM_STR` of a native, the argument at `position` read off
/// the call frame, coerced as php coerces a string parameter
/// (`zend_parse_arg_str_weak`): `null` is the 8.1 deprecation and then
/// the empty string written back into the frame, so a trace taken
/// after it prints `''` as php's does, and under `strict_types` the
/// `TypeError`, both `coerce_param`'s.
fn string_argument(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::String, position)?;
    match coerced.as_php_string() {
        Some(s) => Ok(*s),
        None => PhpString::new(req, b""),
    }
}

/// One `Z_PARAM_LONG` of a native, the argument at `position` read off
/// the call frame, `coerce_param` over `int`, whose `null` is the 8.1
/// deprecation and then zero under the weak rule.
fn long_argument(req: &mut Request, position: u32) -> Result<i64, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::Int, position)?;
    Ok(coerced.as_int().unwrap_or(0))
}

/// One `Z_PARAM_BOOL` of a native, the argument at `position` read off
/// the call frame, `coerce_param` over `bool`, whose `null` is the 8.1
/// deprecation and then `false` under the weak rule.
fn bool_argument(req: &mut Request, position: u32) -> Result<bool, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::Bool, position)?;
    Ok(coerced.as_bool().unwrap_or(false))
}

/// One declared parameter, by value, with no default.
fn parameter(name: &[u8], ty: Type) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty: Some(ty),
        by_ref: false,
        prefer_ref: false,
        promoted: false,
        variadic: false,
        default: ParameterDefault::None,
        attributes: Vec::new(),
    }
}

/// One native's declaration, as the attributes will derive it from the
/// Rust signature.
fn declared(name: &[u8], parameters: &[Parameter], returns: Type) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.into()),
        parameters: parameters.to_vec(),
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

/// The declaration with its frameless entries, php's
/// `frameless_function_infos_<name>` table: one arity per
/// `ZEND_FRAMELESS_FUNCTION(name, arity)` the native has, in php's
/// order.
fn frameless(decl: FunctionDecl, arities: &[u32]) -> FunctionDecl {
    FunctionDecl {
        frameless: arities.to_vec(),
        ..decl
    }
}
