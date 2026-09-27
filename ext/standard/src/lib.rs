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
pub use dir::glob::php_glob;
pub use file::{php_fputcsv, php_fstat};
pub use filestat::{Fs, php_stat};
pub use scanf::php_sscanf_internal;

use php_ext::abi::{Callable, Dependency, DependencyKind, Extension, ExtensionBuilder, OnModify};
use php_ext::coerce::coerce_param;
use php_ext::decl::{ConstDecl, ConstExpr, FunctionDecl, Name, Parameter, ParameterDefault, Type};
use php_ext::raising::{Level, Thrown, arg_count_error, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, Value};

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

/// The natives this file declares itself, `count` and `sizeof`, `ord`,
/// `chr` and `str_repeat`, then the `is_*` type checks `type.rs`
/// declares, at the place they have always been registered.
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
        r#type::is_null::declared(),
        r#type::is_resource::declared(),
        r#type::is_bool::declared(),
        r#type::is_int::declared(),
        r#type::is_integer::declared(),
        r#type::is_long::declared(),
        r#type::is_float::declared(),
        r#type::is_double::declared(),
        r#type::is_string::declared(),
        r#type::is_array::declared(),
        r#type::is_object::declared(),
        r#type::is_scalar::declared(),
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
#[allow(clippy::too_many_lines)] // one line per native, in the stub's order
pub fn extension() -> Extension {
    let directory = dir::directory_class();
    let builder = ExtensionBuilder::default()
        .functions(core_natives())
        .functions(string_natives())
        .functions(array::natives())
        .functions(vec![
            output::ob_start::declared(),
            output::ob_flush::declared(),
            output::ob_clean::declared(),
            output::ob_end_flush::declared(),
            output::ob_end_clean::declared(),
            output::ob_get_flush::declared(),
            output::ob_get_clean::declared(),
            output::ob_get_contents::declared(),
            output::ob_get_level::declared(),
            output::ob_get_length::declared(),
            output::ob_list_handlers::declared(),
            output::ob_get_status::declared(),
            output::ob_implicit_flush::declared(),
        ])
        .functions(vec![
            head::header::declared(),
            head::header_remove::declared(),
            head::http_response_code::declared(),
            head::headers_sent::declared(),
            head::headers_list::declared(),
            head::setcookie::declared(),
            head::setrawcookie::declared(),
        ])
        .functions(var::natives())
        .functions(formatted_print::natives())
        .functions(base64::natives())
        .functions(exec::natives())
        .functions(file::natives())
        .functions(filestat::natives())
        .functions(vec![
            basic_functions::constant::declared(),
            basic_functions::sleep::declared(),
            basic_functions::call_user_func::declared(),
            basic_functions::call_user_func_array::declared(),
            basic_functions::forward_static_call::declared(),
            basic_functions::forward_static_call_array::declared(),
        ])
        .functions(file_natives())
        .functions(vec![
            math::abs::declared(),
            math::ceil::declared(),
            math::floor::declared(),
            math::round::declared(),
            math::sin::declared(),
            math::cos::declared(),
            math::tan::declared(),
            math::asin::declared(),
            math::acos::declared(),
            math::atan::declared(),
            math::atan2::declared(),
            math::sinh::declared(),
            math::cosh::declared(),
            math::tanh::declared(),
            math::asinh::declared(),
            math::acosh::declared(),
            math::atanh::declared(),
            math::expm1::declared(),
            math::log1p::declared(),
            math::pi::declared(),
            math::is_finite::declared(),
            math::is_nan::declared(),
            math::intdiv::declared(),
            math::is_infinite::declared(),
            math::pow::declared(),
            math::exp::declared(),
            math::log::declared(),
            math::log10::declared(),
            math::sqrt::declared(),
            math::hypot::declared(),
            math::deg2rad::declared(),
            math::rad2deg::declared(),
            math::bindec::declared(),
            math::hexdec::declared(),
            math::octdec::declared(),
            math::decbin::declared(),
            math::decoct::declared(),
            math::dechex::declared(),
            math::base_convert::declared(),
            math::number_format::declared(),
            math::fmod::declared(),
            math::fdiv::declared(),
            math::fpow::declared(),
        ])
        .functions(vec![
            r#type::gettype::declared(),
            r#type::get_debug_type::declared(),
            r#type::settype::declared(),
            r#type::intval::declared(),
            r#type::floatval::declared(),
            r#type::doubleval::declared(),
            r#type::boolval::declared(),
            r#type::strval::declared(),
            r#type::array_is_list::declared(),
            r#type::is_numeric::declared(),
            r#type::is_callable::declared(),
            r#type::is_iterable::declared(),
            r#type::is_countable::declared(),
        ])
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
        .constants(math::constants::declared())
        .constants_with::<ImageHandler>(image::link_constants)
        .class::<math::RoundingMode>()
        .classes(var::classes())
        .classes(user_filters::classes())
        .ini(var::ini())
        .ini(sockets::ini())
        .dependencies(dependencies())
        .class_with(directory.0, directory.1, directory.2)
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
#[allow(clippy::too_many_lines)] // one line per native, in the stub's order
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
        vec![
            microtime::microtime::declared(),
            microtime::gettimeofday::declared(),
            microtime::getrusage::declared(),
        ],
        vec![hrtime::hrtime::declared()],
        versioning::natives(),
        vec![
            basic_functions::ip2long::declared(),
            basic_functions::long2ip::declared(),
            basic_functions::getenv::declared(),
            basic_functions::putenv::declared(),
            basic_functions::getopt::declared(),
            basic_functions::time_nanosleep::declared(),
            basic_functions::error_get_last::declared(),
            basic_functions::error_clear_last::declared(),
            basic_functions::set_include_path::declared(),
            basic_functions::get_include_path::declared(),
            basic_functions::getservbyname::declared(),
            basic_functions::getservbyport::declared(),
            basic_functions::getprotobyname::declared(),
            basic_functions::getprotobynumber::declared(),
            basic_functions::parse_ini_file::declared(),
            basic_functions::parse_ini_string::declared(),
            basic_functions::flush::declared(),
            basic_functions::highlight_file::declared(),
            basic_functions::show_source::declared(),
            basic_functions::php_strip_whitespace::declared(),
            basic_functions::highlight_string::declared(),
            basic_functions::usleep::declared(),
            basic_functions::time_sleep_until::declared(),
            basic_functions::get_current_user::declared(),
            basic_functions::get_cfg_var::declared(),
            basic_functions::error_log::declared(),
            basic_functions::ini_get_all::declared(),
            basic_functions::ini_restore::declared(),
            basic_functions::ini_parse_quantity::declared(),
            basic_functions::connection_aborted::declared(),
            basic_functions::connection_status::declared(),
            basic_functions::ignore_user_abort::declared(),
            basic_functions::register_tick_function::declared(),
            basic_functions::unregister_tick_function::declared(),
            basic_functions::is_uploaded_file::declared(),
            basic_functions::move_uploaded_file::declared(),
            basic_functions::sys_getloadavg::declared(),
            basic_functions::ftok::declared(),
            basic_functions::inet_ntop::declared(),
            basic_functions::inet_pton::declared(),
            basic_functions::header_register_callback::declared(),
        ],
        dir::natives(),
        link::natives(),
        vec![dl::dl::declared()],
        streamsfuncs::natives(),
        crypt::natives(),
        password::natives(),
        assert::natives(),
        vec![
            pageinfo::getmyuid::declared(),
            pageinfo::getmygid::declared(),
            pageinfo::getmypid::declared(),
            pageinfo::getmyinode::declared(),
            pageinfo::getlastmod::declared(),
        ],
        vec![
            syslog::openlog::declared(),
            syslog::closelog::declared(),
            syslog::syslog::declared(),
        ],
        mail::natives(),
        image::natives(),
        vec![browscap::get_browser::declared()],
        vec![datetime::strptime::declared()],
        vec![net::net_get_interfaces::declared()],
        iptc::natives(),
        vec![iptc::iptcparse::declared()],
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

/// Their constants.
fn file_constants() -> Vec<ConstDecl> {
    [
        string::constants(),
        url::constants(),
        html::constants(),
        basic_functions::constants::declared(),
        dir::constants(),
        streamsfuncs::constants(),
        crypt::constants(),
        password::constants(),
        assert::constants(),
        syslog::constants::declared(),
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

/// One `REGISTER_LONG_CONSTANT`.
fn constant(name: &[u8], value: i64) -> ConstDecl {
    ConstDecl {
        name: Name(name.into()),
        value: ConstExpr::long(value),
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
