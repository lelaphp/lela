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
pub use image::{ImageHandler, ImageSize};
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
mod url_scanner;
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
pub use dir::php_glob::php_glob;
pub use file::{php_fputcsv, php_fstat};
pub use filestat::{Fs, php_stat};
pub use scanf::php_sscanf_internal;

use php_ext::abi::{Callable, Dependency, DependencyKind, Extension, ExtensionBuilder, OnModify};
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
    /// `BG(url_adapt_output_ex)`, the URL rewriter's state.
    url_rewriter: url_scanner::Rewriter,
    assert: assert::Stage,
}

/// The natives this file declares itself: `count` and `sizeof`, `ord`,
/// `chr`, `str_repeat` and the `is_*` type checks.
fn core_natives() -> Vec<(FunctionDecl, output::Entry)> {
    let countable = || Type::Union(vec![Type::Named(Name::from("Countable")), Type::Array]);
    let normal = ParameterDefault::Expr(ConstExpr::Constant(Name::from("COUNT_NORMAL")));
    let mode = || Parameter {
        default: normal.clone(),
        ..parameter(b"mode", Type::Int)
    };
    vec![
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
        string::ord::declared(),
        string::chr::declared(),
        string::str_repeat::declared(),
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
    ]
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
    let builder = ExtensionBuilder::default()
        .functions(core_natives())
        .functions(string_natives())
        .functions(array::natives())
        .functions(output::natives())
        .functions(head::natives())
        .functions(var::natives())
        .functions(vec![
            formatted_print::sprintf::declared(),
            formatted_print::printf::declared(),
            formatted_print::vsprintf::declared(),
            formatted_print::vprintf::declared(),
            formatted_print::fprintf::declared(),
            formatted_print::vfprintf::declared(),
        ])
        .functions(vec![
            base64::base64_encode::declared(),
            base64::base64_decode::declared(),
        ])
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
            string::STR_PAD_LEFT::declared(),
            string::STR_PAD_RIGHT::declared(),
            string::STR_PAD_BOTH::declared(),
        ])
        .constants(array::constants())
        .constants(file::constants::declared())
        .constants(file_constants())
        .constants(math::constants())
        .constants_with::<ImageHandler>(image::link_constants)
        .classes(math::classes())
        .classes(var::classes())
        .class::<user_filters::php_user_filter>()
        .class::<user_filters::StreamBucket>()
        .ini(var::ini())
        .ini(sockets::ini())
        .dependencies(dependencies())
        .class::<dir::Directory>()
        .classes(vec![assert::assertion_error_class()])
        .ini(assert::ini())
        .ini(browscap::ini())
        .ini(url_scanner::ini());
    ini_handlers()
        .into_iter()
        .fold(builder, |b, (name, handler)| b.on_modify(name, handler))
        .request_state::<State>()
        .rshutdown(syslog::close)
        .rinit(rinit)
        .build("standard")
}

/// The `on_modify` of each entry that has one, php's `PHP_INI_MH` at
/// runtime: the URL rewriter's two over session's entries, and the five
/// of `assert.c`.
fn ini_handlers() -> [(&'static [u8], OnModify); 7] {
    [
        (b"session.trans_sid_tags", url_scanner::on_session_tags),
        (b"session.trans_sid_hosts", url_scanner::on_session_hosts),
        (b"assert.active", assert::on_active),
        (b"assert.bail", assert::on_bail),
        (b"assert.warning", assert::on_warning),
        (b"assert.callback", assert::on_callback),
        (b"assert.exception", assert::on_exception),
    ]
}

/// `PHP_RINIT(basic)`'s share here: the `glob://` wrapper and the
/// filters, which php registers as the engine starts and this engine keeps
/// in the request's wrapper and filter tables (`dir::glob_wrapper`,
/// `filters::register`). The `assert.*` entries' startup deprecations are
/// their on-modify rules, which the engine serves over the host's values
/// as it starts.
#[allow(clippy::unnecessary_wraps)] // `ExtensionBuilder::rinit`'s signature
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    req.register_wrapper(dir::glob_wrapper());
    filters::register(req);
    Ok(())
}

/// The natives of the php-src files ported one file each (#655), in
/// the order the modules landed: `string.c`, `url.c`, `html.c`, the
/// digests, `pack.c`, `http.c`, the small files and `basic_functions.c`'s
/// environment and ini natives.
fn file_natives() -> Vec<(FunctionDecl, output::Entry)> {
    [
        string_c(),
        vec![
            url::parse_url::declared(),
            url::urlencode::declared(),
            url::urldecode::declared(),
            url::rawurlencode::declared(),
            url::rawurldecode::declared(),
            url::get_headers::declared(),
        ],
        vec![
            html::htmlspecialchars::declared(),
            html::htmlspecialchars_decode::declared(),
            html::html_entity_decode::declared(),
            html::htmlentities::declared(),
            html::get_html_translation_table::declared(),
        ],
        vec![md5::md5::declared(), md5::md5_file::declared()],
        vec![sha1::sha1::declared(), sha1::sha1_file::declared()],
        vec![crc32::crc32::declared()],
        vec![pack::pack::declared(), pack::unpack::declared()],
        http::natives(),
        vec![
            quot_print::quoted_printable_decode::declared(),
            quot_print::quoted_printable_encode::declared(),
        ],
        vec![
            uuencode::convert_uuencode::declared(),
            uuencode::convert_uudecode::declared(),
        ],
        vec![metaphone::metaphone::declared()],
        vec![soundex::soundex::declared()],
        vec![levenshtein::levenshtein::declared()],
        vec![uniqid::uniqid::declared()],
        microtime::natives(),
        hrtime::natives(),
        vec![versioning::version_compare::declared()],
        basic_functions::natives(),
        dir::natives(),
        link::natives(),
        dl::natives(),
        streamsfuncs::natives(),
        vec![crypt::crypt::declared()],
        vec![
            password::password_get_info::declared(),
            password::password_hash::declared(),
            password::password_needs_rehash::declared(),
            password::password_verify::declared(),
            password::password_algos::declared(),
        ],
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
        url_scanner::natives(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The natives of `string.c` in the order `basic_functions.stub.php`
/// declares them, less the ones [`string_natives`] and
/// [`core_natives`] declare first: `string`'s, its children's, and
/// `str_getcsv`, `strnatcmp`, `strnatcasecmp` and `sscanf`, whose bodies
/// live beside the code they share.
fn string_c() -> Vec<(FunctionDecl, output::Entry)> {
    vec![
        string::bin2hex::declared(),
        string::hex2bin::declared(),
        string::strspn::declared(),
        string::strcspn::declared(),
        string::locale::nl_langinfo::declared(),
        string::locale::strcoll::declared(),
        string::rtrim::declared(),
        string::chop::declared(),
        string::ltrim::declared(),
        string::wordwrap::declared(),
        string::explode::declared(),
        string::implode::declared(),
        string::join::declared(),
        string::strtok::declared(),
        string::str_increment::declared(),
        string::str_decrement::declared(),
        string::path::basename::declared(),
        string::path::dirname::declared(),
        string::path::pathinfo::declared(),
        string::find::stristr::declared(),
        string::find::strstr::declared(),
        string::find::strchr::declared(),
        string::find::strpos::declared(),
        string::find::stripos::declared(),
        string::find::strrpos::declared(),
        string::find::strripos::declared(),
        string::find::strrchr::declared(),
        string::find::str_starts_with::declared(),
        string::find::str_ends_with::declared(),
        string::chunk_split::declared(),
        string::replace::substr_replace::declared(),
        string::quotemeta::declared(),
        string::ucfirst::declared(),
        string::lcfirst::declared(),
        string::ucwords::declared(),
        string::replace::strtr::declared(),
        string::strrev::declared(),
        string::similar_text::declared(),
        string::slashes::addcslashes::declared(),
        string::slashes::addslashes::declared(),
        string::slashes::stripcslashes::declared(),
        string::slashes::stripslashes::declared(),
        string::replace::str_replace::declared(),
        string::replace::str_ireplace::declared(),
        string::hebrev::declared(),
        string::nl2br::declared(),
        string::strip_tags::strip_tags::declared(),
        string::locale::setlocale::declared(),
        string::parse_str::parse_str::declared(),
        csv::str_getcsv::declared(),
        string::count_chars::declared(),
        strnatcmp::strnatcmp::declared(),
        string::locale::localeconv::declared(),
        strnatcmp::strnatcasecmp::declared(),
        string::substr_count::declared(),
        scanf::sscanf::declared(),
        string::str_rot13::declared(),
        string::str_shuffle::declared(),
        string::str_word_count::declared(),
        string::str_split::declared(),
        string::strpbrk::declared(),
        string::substr_compare::declared(),
        string::utf8_encode::declared(),
        string::utf8_decode::declared(),
    ]
}

/// Their constants.
fn file_constants() -> Vec<ConstDecl> {
    [
        string::constants::declared(),
        url::constants::declared(),
        html::constants::declared(),
        basic_functions::constants(),
        dir::constants::declared(),
        crypt::constants::declared(),
        password::constants::declared(),
        assert::constants(),
        syslog::constants(),
        image::constants(),
        dns::constants::declared(),
        info::constants(),
        user_filters::constants::declared(),
        userspace::constants::declared(),
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

/// The natives of `string.c` the crate declared first, each with the
/// frameless entries `basic_functions_arginfo.h` gives it.
fn string_natives() -> Vec<(FunctionDecl, output::Entry)> {
    vec![
        string::trim::declared(),
        string::str_contains::declared(),
        string::strtoupper::declared(),
        string::strtolower::declared(),
        string::str_pad::declared(),
        string::substr::declared(),
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
        value: ConstExpr::long(value),
        attributes: Vec::new(),
    }
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
