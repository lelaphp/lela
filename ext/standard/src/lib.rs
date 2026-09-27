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

use php_ext::abi::{Dependency, DependencyKind, Extension, ExtensionBuilder, OnModify};
use php_ext::decl::{ConstDecl, FunctionDecl};
use php_ext::raising::{Thrown, arg_count_error, arg_value_error};
use php_ext::request::Request;

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
    let builder = ExtensionBuilder::default()
        // `string.c`'s three this file declares, then `type.c`'s checks,
        // where they have always been registered.
        .functions(vec![
            string::ord::declared(),
            string::chr::declared(),
            string::str_repeat::declared(),
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
        ])
        // `string.c`'s six this file declares.
        .functions(vec![
            string::trim::declared(),
            string::str_contains::declared(),
            string::strtoupper::declared(),
            string::strtolower::declared(),
            string::str_pad::declared(),
            string::substr::declared(),
        ])
        // `array.c`, in the stub's order.
        .functions(vec![
            array::stack::array_push::declared(),
            array::sort::krsort::declared(),
            array::sort::ksort::declared(),
            array::count::declared(),
            array::sizeof::declared(),
            array::sort::natsort::declared(),
            array::sort::natcasesort::declared(),
            array::sort::asort::declared(),
            array::sort::arsort::declared(),
            array::sort::sort::declared(),
            array::sort::rsort::declared(),
            array::sort::usort::declared(),
            array::sort::uasort::declared(),
            array::sort::uksort::declared(),
            array::pointer::end::declared(),
            array::pointer::prev::declared(),
            array::pointer::next::declared(),
            array::pointer::reset::declared(),
            array::pointer::current::declared(),
            array::pointer::pos::declared(),
            array::pointer::key::declared(),
            array::pointer::min::declared(),
            array::pointer::max::declared(),
            array::walk::array_walk::declared(),
            array::walk::array_walk_recursive::declared(),
            array::keys::in_array::declared(),
            array::keys::array_search::declared(),
            array::symbols::extract::declared(),
            array::symbols::compact::declared(),
            array::keys::array_fill::declared(),
            array::keys::array_fill_keys::declared(),
            array::make::range::declared(),
            array::make::shuffle::declared(),
            array::stack::array_pop::declared(),
            array::stack::array_shift::declared(),
            array::stack::array_unshift::declared(),
            array::stack::array_splice::declared(),
            array::stack::array_slice::declared(),
            array::array_merge::declared(),
            array::make::array_merge_recursive::declared(),
            array::make::array_replace::declared(),
            array::make::array_replace_recursive::declared(),
            array::keys::array_keys::declared(),
            array::keys::array_key_first::declared(),
            array::keys::array_key_last::declared(),
            array::keys::array_first::declared(),
            array::keys::array_last::declared(),
            array::keys::array_values::declared(),
            array::keys::array_count_values::declared(),
            array::keys::array_column::declared(),
            array::keys::array_reverse::declared(),
            array::keys::array_pad::declared(),
            array::keys::array_flip::declared(),
            array::keys::array_change_key_case::declared(),
            array::array_unique::declared(),
            array::set::array_intersect_key::declared(),
            array::set::array_intersect_ukey::declared(),
            array::set::array_intersect::declared(),
            array::set::array_uintersect::declared(),
            array::set::array_intersect_assoc::declared(),
            array::set::array_uintersect_assoc::declared(),
            array::set::array_intersect_uassoc::declared(),
            array::set::array_uintersect_uassoc::declared(),
            array::set::array_diff_key::declared(),
            array::set::array_diff_ukey::declared(),
            array::set::array_diff::declared(),
            array::set::array_udiff::declared(),
            array::set::array_diff_assoc::declared(),
            array::set::array_diff_uassoc::declared(),
            array::set::array_udiff_assoc::declared(),
            array::set::array_udiff_uassoc::declared(),
            array::sort::array_multisort::declared(),
            array::make::array_rand::declared(),
            array::keys::array_sum::declared(),
            array::keys::array_product::declared(),
            array::walk::array_reduce::declared(),
            array::walk::array_filter::declared(),
            array::walk::array_find::declared(),
            array::walk::array_find_key::declared(),
            array::walk::array_any::declared(),
            array::walk::array_all::declared(),
            array::walk::array_map::declared(),
            array::keys::array_key_exists::declared(),
            array::keys::key_exists::declared(),
            array::keys::array_chunk::declared(),
            array::keys::array_combine::declared(),
        ])
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
        .functions(vec![
            var::print_r::declared(),
            var::var_export::declared(),
            var::debug::debug_zval_dump::declared(),
            var::serialize::serialize::declared(),
            var::unserialize::unserialize::declared(),
        ])
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
        .constants(array::constants::declared())
        .constants(vec![
            string::STR_PAD_LEFT::declared(),
            string::STR_PAD_RIGHT::declared(),
            string::STR_PAD_BOTH::declared(),
        ])
        .constants(file::constants::declared())
        .constants(file_constants())
        .constants(math::constants::declared())
        .constants_with::<ImageHandler>(image::link_constants)
        .class::<math::RoundingMode>()
        .class::<var::__PHP_Incomplete_Class>()
        .class::<user_filters::php_user_filter>()
        .class::<user_filters::StreamBucket>()
        .ini(var::ini())
        .ini(sockets::ini())
        .dependencies(dependencies())
        .class::<dir::Directory>()
        .class::<assert::AssertionError>()
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
        vec![
            microtime::microtime::declared(),
            microtime::gettimeofday::declared(),
            microtime::getrusage::declared(),
        ],
        vec![hrtime::hrtime::declared()],
        vec![versioning::version_compare::declared()],
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
        vec![crypt::crypt::declared()],
        vec![
            password::password_get_info::declared(),
            password::password_hash::declared(),
            password::password_needs_rehash::declared(),
            password::password_verify::declared(),
            password::password_algos::declared(),
        ],
        vec![
            assert::assert::declared(),
            assert::assert_options::declared(),
        ],
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
        vec![mail::mail::declared()],
        vec![
            image::image_type_to_mime_type::declared(),
            image::image_type_to_extension::declared(),
            image::getimagesize::declared(),
            image::getimagesizefromstring::declared(),
        ],
        vec![browscap::get_browser::declared()],
        vec![datetime::strptime::declared()],
        vec![net::net_get_interfaces::declared()],
        vec![iptc::iptcembed::declared(), iptc::iptcparse::declared()],
        dns::natives(),
        vec![
            info::phpinfo::declared(),
            info::phpversion::declared(),
            info::phpcredits::declared(),
            info::php_sapi_name::declared(),
            info::php_uname::declared(),
            info::php_ini_scanned_files::declared(),
            info::php_ini_loaded_file::declared(),
        ],
        select::natives(),
        proc_open::natives(),
        sockets::natives(),
        user_filters::natives(),
        userspace::natives(),
        vec![
            url_scanner::output_reset_rewrite_vars::declared(),
            url_scanner::output_add_rewrite_var::declared(),
        ],
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
        basic_functions::constants::declared(),
        dir::constants::declared(),
        crypt::constants::declared(),
        password::constants::declared(),
        assert::constants::declared(),
        syslog::constants::declared(),
        image::constants::declared(),
        dns::constants::declared(),
        info::constants::declared(),
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

/// `zend_wrong_parameters_count_error` (`Z/Zend/zend_API.c:232`) for a
/// native that counts its own arguments, `arity = manual`, where php
/// reads the count after a step of its own: nothing where `min..=max`
/// holds the count, else `exactly`, `at least` or `at most` by where it
/// fell, `max` of `None` unbounded as a `"+"` spec is. The four such
/// natives' counts are `assert`'s after its active check, the `"+f"`
/// set natives' over their trailing callbacks, `strtr`'s by the parse
/// it chooses and `stream_context_set_option`'s after its deprecation.
///
/// ```php
/// foreach ([fn () => assert(), fn () => array_udiff([1]), fn () => array_uintersect_uassoc([1], 'strcmp'), fn () => strtr("abc", "a", "b", "c"), fn () => stream_context_set_option()] as $f) {
///     try { $f(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// assert() expects at least 1 argument, 0 given
/// array_udiff() expects at least 2 arguments, 1 given
/// array_uintersect_uassoc() expects at least 3 arguments, 2 given
/// strtr() expects exactly 3 arguments, 4 given
/// stream_context_set_option() expects at least 2 arguments, 0 given
/// ```
fn arity(req: &mut Request, min: usize, max: Option<usize>) -> Result<(), Thrown> {
    let given = req.argument_count();
    if given >= min && max.is_none_or(|max| given <= max) {
        return Ok(());
    }
    let (word, bound) = match max {
        Some(max) if max == min => ("exactly", min),
        _ if given < min => ("at least", min),
        Some(max) => ("at most", max),
        None => ("at least", min),
    };
    let plural = if bound == 1 { "" } else { "s" };
    Err(arg_count_error(
        req,
        &format!("{word} {bound} argument{plural}, {given} given"),
    ))
}
