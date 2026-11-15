//! The `standard` extension: `php-src/ext/standard` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/standard/tests`, through `xtask phpt --subset`.
//!
//! Every native, constant and class is declared by the attributes beside
//! its body, in the file named for the php-src file it follows, and
//! `extension()` registers them in one list each, in php's order:
//! the functions as `basic_functions.stub.php` declares them, each alias
//! beside its target, and the constants as `PHP_MINIT(basic)` registers
//! them, so `get_defined_functions()`, `get_defined_constants()` and
//! `php --re standard` list them as php does. The compiler serves some
//! calls itself (`count`, `chr`, `ord`, the `is_*` checks) and
//! emits a frameless call for the natives that declare one; what
//! reaches a native is the call it could not shape.
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
mod dns;
mod exec;
mod ffi;
mod file;
mod filestat;
mod filters;
mod formatted_print;
mod ftp_fopen_wrapper;
mod getopt;
mod head;
mod hrtime;
mod html;
mod http;
mod http_fopen_wrapper;
mod image;
pub use image::{ImageHandler, ImageSize};
/// What `php_image.h` and `php_string.h` declare `PHPAPI` for ext/exif,
/// which includes both: an image's type, a type's MIME type, and the
/// base name of the file it reports.
pub use image::{php_getimagetype, php_image_type_to_mime_type};
pub use string::path::php_basename;
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
mod php_fopen_wrapper;
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
mod throwables;
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
pub use html::escape_string as escape_html_entities;
pub use string::strip_tags_ex;
// `html.h`'s `php_next_utf8_char`, which `xml.c` decodes a target
// encoding's characters with (design §4.10).
pub use string::next_utf8_char as php_next_utf8_char;
pub use var::{Serializer, Unserializer, php_unserialize_with_options};
// `php_var.h`'s `php_var_export_ex`, which zend_test's observer writes a
// return value with (`observer.c`'s `get_retval_info`; design §4.10).
pub use var::export as php_var_export_ex;

/// What `file.h`, `php_filestat.h`, `scanf.h` and `main/php_glob.h`
/// declare `PHPAPI` for `spl_directory.c`, which calls them directly
/// rather than through the function table (ADR-0004; design §4.10).
pub use csv::fgetcsv as php_fgetcsv;
pub use dir::php_glob::php_glob;
pub use file::{php_fputcsv, php_fstat};
pub use filestat::{Fs, php_stat};
pub use scanf::php_sscanf_internal;

/// What `url_scanner_ex.h` declares `PHPAPI` for `session.c`, which adds
/// the session's id to the URLs and forms of the session rewriter
/// (ADR-0004; design §4.10).
pub use url_scanner::{php_url_scanner_add_session_var, php_url_scanner_reset_session_var};

/// What `file.h` declares `PHPAPI PHP_FUNCTION` for `zlib.c`, whose
/// `gz*` file natives are `ZEND_FALIAS`es of these under declarations of
/// their own (ADR-0004; design §4.10).
pub use file::{fclose, feof, fgetc, fgets, fpassthru, fread, fseek, ftell, fwrite, rewind};

/// What `php_sdl.c` takes of `standard`: `php_get_current_user`, which
/// names its cache files (ADR-0004; design §4.10).
pub use basic_functions::php_get_current_user;

/// What `openssl.c` and `xp_ssl.c` take of `standard` (ADR-0004; design
/// §4.10): `php_fopen_wrappers.h`'s `php_stream_http_wrapper` and
/// `php_stream_ftp_wrapper`, which openssl's `MINIT` registers again as
/// `https` and `ftps`; and `php_stream_context_get_uri_parser` with the
/// parse it hands `php_uri.h`, which name the host a client verifies,
/// which php keeps in `main/` and this engine ports in `standard` beside
/// the wrappers.
pub use ftp_fopen_wrapper::wrapper as ftp_wrapper;
pub use http_fopen_wrapper::wrapper as http_wrapper;
pub use url::UriParser;

/// What `php_mail.h` and `exec.h` declare `PHPAPI` for `mbstring.c`,
/// whose `mb_send_mail` writes its headers and body and hands them to
/// `php_mail` (ADR-0004; design §4.10).
pub use exec::escape_shell_cmd as php_escape_shell_cmd;
pub use mail::{build_headers as php_mail_build_headers, php_mail};

use php_ext::abi::{
    Dependency, DependencyKind, Extension, ExtensionBuilder, LongHandler, OnModify,
};
use php_ext::raising::{Thrown, arg_value_error, while_pending};
use php_ext::request::{EngineWrapper, Request, STREAM_HOOKS, Streams};
use php_ext::values::{Owner, View};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {
    /// `DIRG(default_dir)` (`Z/ext/standard/dir.c:62`): the directory
    /// `opendir` opened last, with the owner php's `GC_ADDREF` takes,
    /// which `readdir`, `rewinddir` and `closedir` read without an
    /// argument; given up at `rshutdown`, where php leaves it to the
    /// list's close, past every destructor either way.
    default_dir: Option<php_ext::values::Owner<php_ext::values::Resource>>,
    string: string::State,
    uniqid: uniqid::State,
    hrtime: hrtime::State,
    page: Option<pageinfo::Page>,
    syslog: syslog::Log,
    browscap: browscap::Browscap,
    proc_open: proc_open::ProcState,
    user_filters: user_filters::FilterState,
    /// `BG(url_adapt_output_ex)`, the URL rewriter's state.
    url_rewriter: url_scanner::Rewriter,
    /// `BG(url_adapt_session_ex)`, `session.use_trans_sid`'s rewriter.
    session_rewriter: url_scanner::Rewriter,
    assert: assert::Stage,
    var: var::Nesting,
    /// `BG(last_http_headers)`: the headers the http wrapper's last
    /// request read, one owner the state's, given back in `rshutdown`.
    last_http_headers: Option<php_ext::values::Owner<php_ext::values::PhpArray>>,
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
#[allow(clippy::large_stack_arrays)] // the entry list is a promoted constant, never on the stack
pub fn extension() -> Extension {
    let builder = ExtensionBuilder::default()
        .functions(&[
            basic_functions::header_register_callback::DECLARED,
            output::ob_start::DECLARED,
            output::ob_flush::DECLARED,
            output::ob_clean::DECLARED,
            output::ob_end_flush::DECLARED,
            output::ob_end_clean::DECLARED,
            output::ob_get_flush::DECLARED,
            output::ob_get_clean::DECLARED,
            output::ob_get_contents::DECLARED,
            output::ob_get_level::DECLARED,
            output::ob_get_length::DECLARED,
            output::ob_list_handlers::DECLARED,
            output::ob_get_status::DECLARED,
            output::ob_implicit_flush::DECLARED,
            url_scanner::output_reset_rewrite_vars::DECLARED,
            url_scanner::output_add_rewrite_var::DECLARED,
            userspace::stream_wrapper_register::DECLARED,
            userspace::stream_register_wrapper::DECLARED,
            userspace::stream_wrapper_unregister::DECLARED,
            userspace::stream_wrapper_restore::DECLARED,
            array::stack::array_push::DECLARED,
            array::sort::krsort::DECLARED,
            array::sort::ksort::DECLARED,
            array::count::DECLARED,
            array::sizeof::DECLARED,
            array::sort::natsort::DECLARED,
            array::sort::natcasesort::DECLARED,
            array::sort::asort::DECLARED,
            array::sort::arsort::DECLARED,
            array::sort::sort::DECLARED,
            array::sort::rsort::DECLARED,
            array::sort::usort::DECLARED,
            array::sort::uasort::DECLARED,
            array::sort::uksort::DECLARED,
            array::pointer::end::DECLARED,
            array::pointer::prev::DECLARED,
            array::pointer::next::DECLARED,
            array::pointer::reset::DECLARED,
            array::pointer::current::DECLARED,
            array::pointer::pos::DECLARED,
            array::pointer::key::DECLARED,
            array::pointer::min::DECLARED,
            array::pointer::max::DECLARED,
            array::walk::array_walk::DECLARED,
            array::walk::array_walk_recursive::DECLARED,
            array::keys::in_array::DECLARED,
            array::keys::array_search::DECLARED,
            array::symbols::extract::DECLARED,
            array::symbols::compact::DECLARED,
            array::keys::array_fill::DECLARED,
            array::keys::array_fill_keys::DECLARED,
            array::make::range::DECLARED,
            array::make::shuffle::DECLARED,
            array::stack::array_pop::DECLARED,
            array::stack::array_shift::DECLARED,
            array::stack::array_unshift::DECLARED,
            array::stack::array_splice::DECLARED,
            array::stack::array_slice::DECLARED,
            array::array_merge::DECLARED,
            array::make::array_merge_recursive::DECLARED,
            array::make::array_replace::DECLARED,
            array::make::array_replace_recursive::DECLARED,
            array::keys::array_keys::DECLARED,
            array::keys::array_key_first::DECLARED,
            array::keys::array_key_last::DECLARED,
            array::keys::array_first::DECLARED,
            array::keys::array_last::DECLARED,
            array::keys::array_values::DECLARED,
            array::keys::array_count_values::DECLARED,
            array::keys::array_column::DECLARED,
            array::keys::array_reverse::DECLARED,
            array::keys::array_pad::DECLARED,
            array::keys::array_flip::DECLARED,
            array::keys::array_change_key_case::DECLARED,
            array::array_unique::DECLARED,
            array::set::array_intersect_key::DECLARED,
            array::set::array_intersect_ukey::DECLARED,
            array::set::array_intersect::DECLARED,
            array::set::array_uintersect::DECLARED,
            array::set::array_intersect_assoc::DECLARED,
            array::set::array_uintersect_assoc::DECLARED,
            array::set::array_intersect_uassoc::DECLARED,
            array::set::array_uintersect_uassoc::DECLARED,
            array::set::array_diff_key::DECLARED,
            array::set::array_diff_ukey::DECLARED,
            array::set::array_diff::DECLARED,
            array::set::array_udiff::DECLARED,
            array::set::array_diff_assoc::DECLARED,
            array::set::array_diff_uassoc::DECLARED,
            array::set::array_udiff_assoc::DECLARED,
            array::set::array_udiff_uassoc::DECLARED,
            array::sort::array_multisort::DECLARED,
            array::make::array_rand::DECLARED,
            array::keys::array_sum::DECLARED,
            array::keys::array_product::DECLARED,
            array::walk::array_reduce::DECLARED,
            array::walk::array_filter::DECLARED,
            array::walk::array_find::DECLARED,
            array::walk::array_find_key::DECLARED,
            array::walk::array_any::DECLARED,
            array::walk::array_all::DECLARED,
            array::walk::array_map::DECLARED,
            array::keys::array_key_exists::DECLARED,
            array::keys::key_exists::DECLARED,
            array::keys::array_chunk::DECLARED,
            array::keys::array_combine::DECLARED,
            r#type::array_is_list::DECLARED,
            base64::base64_encode::DECLARED,
            base64::base64_decode::DECLARED,
            basic_functions::constant::DECLARED,
            basic_functions::ip2long::DECLARED,
            basic_functions::long2ip::DECLARED,
            basic_functions::getenv::DECLARED,
            basic_functions::putenv::DECLARED,
            basic_functions::getopt::DECLARED,
            basic_functions::flush::DECLARED,
            basic_functions::sleep::DECLARED,
            basic_functions::usleep::DECLARED,
            basic_functions::time_nanosleep::DECLARED,
            basic_functions::time_sleep_until::DECLARED,
            basic_functions::get_current_user::DECLARED,
            basic_functions::get_cfg_var::DECLARED,
            basic_functions::error_log::DECLARED,
            basic_functions::error_get_last::DECLARED,
            basic_functions::error_clear_last::DECLARED,
            basic_functions::call_user_func::DECLARED,
            basic_functions::call_user_func_array::DECLARED,
            basic_functions::forward_static_call::DECLARED,
            basic_functions::forward_static_call_array::DECLARED,
            basic_functions::highlight_file::DECLARED,
            basic_functions::show_source::DECLARED,
            basic_functions::php_strip_whitespace::DECLARED,
            basic_functions::highlight_string::DECLARED,
            basic_functions::ini_get_all::DECLARED,
            basic_functions::ini_restore::DECLARED,
            basic_functions::ini_parse_quantity::DECLARED,
            basic_functions::set_include_path::DECLARED,
            basic_functions::get_include_path::DECLARED,
            var::print_r::DECLARED,
            basic_functions::connection_aborted::DECLARED,
            basic_functions::connection_status::DECLARED,
            basic_functions::ignore_user_abort::DECLARED,
            basic_functions::getservbyname::DECLARED,
            basic_functions::getservbyport::DECLARED,
            basic_functions::getprotobyname::DECLARED,
            basic_functions::getprotobynumber::DECLARED,
            basic_functions::register_tick_function::DECLARED,
            basic_functions::unregister_tick_function::DECLARED,
            basic_functions::is_uploaded_file::DECLARED,
            basic_functions::move_uploaded_file::DECLARED,
            basic_functions::parse_ini_file::DECLARED,
            basic_functions::parse_ini_string::DECLARED,
            basic_functions::sys_getloadavg::DECLARED,
            browscap::get_browser::DECLARED,
            crc32::crc32::DECLARED,
            crypt::crypt::DECLARED,
            datetime::strptime::DECLARED,
            dns::gethostname::DECLARED,
            dns::gethostbyaddr::DECLARED,
            dns::gethostbyname::DECLARED,
            dns::gethostbynamel::DECLARED,
            dns::dns_check_record::DECLARED,
            dns::checkdnsrr::DECLARED,
            dns::dns_get_record::DECLARED,
            dns::dns_get_mx::DECLARED,
            dns::getmxrr::DECLARED,
            net::net_get_interfaces::DECLARED,
            basic_functions::ftok::DECLARED,
            hrtime::hrtime::DECLARED,
            md5::md5::DECLARED,
            md5::md5_file::DECLARED,
            pageinfo::getmyuid::DECLARED,
            pageinfo::getmygid::DECLARED,
            pageinfo::getmypid::DECLARED,
            pageinfo::getmyinode::DECLARED,
            pageinfo::getlastmod::DECLARED,
            sha1::sha1::DECLARED,
            sha1::sha1_file::DECLARED,
            syslog::openlog::DECLARED,
            syslog::closelog::DECLARED,
            syslog::syslog::DECLARED,
            basic_functions::inet_ntop::DECLARED,
            basic_functions::inet_pton::DECLARED,
            metaphone::metaphone::DECLARED,
            head::header::DECLARED,
            head::header_remove::DECLARED,
            head::setrawcookie::DECLARED,
            head::setcookie::DECLARED,
            head::http_response_code::DECLARED,
            head::headers_sent::DECLARED,
            head::headers_list::DECLARED,
            html::htmlspecialchars::DECLARED,
            html::htmlspecialchars_decode::DECLARED,
            html::html_entity_decode::DECLARED,
            html::htmlentities::DECLARED,
            html::get_html_translation_table::DECLARED,
            assert::assert::DECLARED,
            assert::assert_options::DECLARED,
            string::bin2hex::DECLARED,
            string::hex2bin::DECLARED,
            string::strspn::DECLARED,
            string::strcspn::DECLARED,
            string::locale::nl_langinfo::DECLARED,
            string::locale::strcoll::DECLARED,
            string::trim::DECLARED,
            string::rtrim::DECLARED,
            string::chop::DECLARED,
            string::ltrim::DECLARED,
            string::wordwrap::DECLARED,
            string::explode::DECLARED,
            string::implode::DECLARED,
            string::join::DECLARED,
            string::strtok::DECLARED,
            string::strtoupper::DECLARED,
            string::strtolower::DECLARED,
            string::str_increment::DECLARED,
            string::str_decrement::DECLARED,
            string::path::basename::DECLARED,
            string::path::dirname::DECLARED,
            string::path::pathinfo::DECLARED,
            string::find::stristr::DECLARED,
            string::find::strstr::DECLARED,
            string::find::strchr::DECLARED,
            string::find::strpos::DECLARED,
            string::find::stripos::DECLARED,
            string::find::strrpos::DECLARED,
            string::find::strripos::DECLARED,
            string::find::strrchr::DECLARED,
            string::str_contains::DECLARED,
            string::find::str_starts_with::DECLARED,
            string::find::str_ends_with::DECLARED,
            string::chunk_split::DECLARED,
            string::substr::DECLARED,
            string::replace::substr_replace::DECLARED,
            string::quotemeta::DECLARED,
            string::ord::DECLARED,
            string::chr::DECLARED,
            string::ucfirst::DECLARED,
            string::lcfirst::DECLARED,
            string::ucwords::DECLARED,
            string::replace::strtr::DECLARED,
            string::strrev::DECLARED,
            string::similar_text::DECLARED,
            string::slashes::addcslashes::DECLARED,
            string::slashes::addslashes::DECLARED,
            string::slashes::stripcslashes::DECLARED,
            string::slashes::stripslashes::DECLARED,
            string::replace::str_replace::DECLARED,
            string::replace::str_ireplace::DECLARED,
            string::hebrev::DECLARED,
            string::nl2br::DECLARED,
            string::strip_tags::strip_tags::DECLARED,
            string::locale::setlocale::DECLARED,
            string::parse_str::parse_str::DECLARED,
            csv::str_getcsv::DECLARED,
            string::str_repeat::DECLARED,
            string::count_chars::DECLARED,
            strnatcmp::strnatcmp::DECLARED,
            string::locale::localeconv::DECLARED,
            strnatcmp::strnatcasecmp::DECLARED,
            string::substr_count::DECLARED,
            string::str_pad::DECLARED,
            scanf::sscanf::DECLARED,
            string::str_rot13::DECLARED,
            string::str_shuffle::DECLARED,
            string::str_word_count::DECLARED,
            string::str_split::DECLARED,
            string::strpbrk::DECLARED,
            string::substr_compare::DECLARED,
            string::utf8_encode::DECLARED,
            string::utf8_decode::DECLARED,
            file::opendir::DECLARED,
            dir::dir::DECLARED,
            file::closedir::DECLARED,
            dir::chdir::DECLARED,
            dir::getcwd::DECLARED,
            file::rewinddir::DECLARED,
            file::readdir::DECLARED,
            dir::scandir::DECLARED,
            dir::glob::DECLARED,
            exec::exec::DECLARED,
            exec::system::DECLARED,
            exec::passthru::DECLARED,
            exec::escapeshellcmd::DECLARED,
            exec::escapeshellarg::DECLARED,
            exec::shell_exec::DECLARED,
            exec::proc_nice::DECLARED,
            file::flock::DECLARED,
            file::get_meta_tags::DECLARED,
            proc_open::pclose::DECLARED,
            proc_open::popen::DECLARED,
            file::readfile::DECLARED,
            file::rewind::DECLARED,
            file::rmdir::DECLARED,
            file::umask::DECLARED,
            file::fclose::DECLARED,
            file::feof::DECLARED,
            file::fgetc::DECLARED,
            file::fgets::DECLARED,
            file::fread::DECLARED,
            file::fopen::DECLARED,
            file::fscanf::DECLARED,
            file::fpassthru::DECLARED,
            file::ftruncate::DECLARED,
            file::fstat::DECLARED,
            file::fseek::DECLARED,
            file::ftell::DECLARED,
            file::fflush::DECLARED,
            file::fsync::DECLARED,
            file::fdatasync::DECLARED,
            file::fwrite::DECLARED,
            file::fputs::DECLARED,
            file::mkdir::DECLARED,
            file::rename::DECLARED,
            file::copy::DECLARED,
            file::tempnam::DECLARED,
            file::tmpfile::DECLARED,
            file::file::DECLARED,
            file::file_get_contents::DECLARED,
            file::unlink::DECLARED,
            file::file_put_contents::DECLARED,
            file::fputcsv::DECLARED,
            file::fgetcsv::DECLARED,
            file::realpath::DECLARED,
            file::fnmatch::DECLARED,
            file::sys_get_temp_dir::DECLARED,
            filestat::fileatime::DECLARED,
            filestat::filectime::DECLARED,
            filestat::filegroup::DECLARED,
            filestat::fileinode::DECLARED,
            filestat::filemtime::DECLARED,
            filestat::fileowner::DECLARED,
            filestat::fileperms::DECLARED,
            filestat::filesize::DECLARED,
            filestat::filetype::DECLARED,
            filestat::file_exists::DECLARED,
            filestat::is_writable::DECLARED,
            filestat::is_writeable::DECLARED,
            filestat::is_readable::DECLARED,
            filestat::is_executable::DECLARED,
            filestat::is_file::DECLARED,
            filestat::is_dir::DECLARED,
            filestat::is_link::DECLARED,
            filestat::stat::DECLARED,
            filestat::lstat::DECLARED,
            filestat::chown::DECLARED,
            filestat::chgrp::DECLARED,
            filestat::lchown::DECLARED,
            filestat::lchgrp::DECLARED,
            filestat::chmod::DECLARED,
            filestat::touch::DECLARED,
            filestat::clearstatcache::DECLARED,
            filestat::disk_total_space::DECLARED,
            filestat::disk_free_space::DECLARED,
            filestat::diskfreespace::DECLARED,
            filestat::realpath_cache_get::DECLARED,
            filestat::realpath_cache_size::DECLARED,
            formatted_print::sprintf::DECLARED,
            formatted_print::printf::DECLARED,
            formatted_print::vprintf::DECLARED,
            formatted_print::vsprintf::DECLARED,
            formatted_print::fprintf::DECLARED,
            formatted_print::vfprintf::DECLARED,
            sockets::fsockopen::DECLARED,
            sockets::pfsockopen::DECLARED,
            http::http_build_query::DECLARED,
            http::http_get_last_response_headers::DECLARED,
            http::http_clear_last_response_headers::DECLARED,
            http::request_parse_body::DECLARED,
            image::image_type_to_mime_type::DECLARED,
            image::image_type_to_extension::DECLARED,
            image::getimagesize::DECLARED,
            image::getimagesizefromstring::DECLARED,
            info::phpinfo::DECLARED,
            info::phpversion::DECLARED,
            info::phpcredits::DECLARED,
            info::php_sapi_name::DECLARED,
            info::php_uname::DECLARED,
            info::php_ini_scanned_files::DECLARED,
            info::php_ini_loaded_file::DECLARED,
            iptc::iptcembed::DECLARED,
            iptc::iptcparse::DECLARED,
            levenshtein::levenshtein::DECLARED,
            link::readlink::DECLARED,
            link::linkinfo::DECLARED,
            link::symlink::DECLARED,
            link::link::DECLARED,
            mail::mail::DECLARED,
            math::abs::DECLARED,
            math::ceil::DECLARED,
            math::floor::DECLARED,
            math::round::DECLARED,
            math::sin::DECLARED,
            math::cos::DECLARED,
            math::tan::DECLARED,
            math::asin::DECLARED,
            math::acos::DECLARED,
            math::atan::DECLARED,
            math::atanh::DECLARED,
            math::atan2::DECLARED,
            math::sinh::DECLARED,
            math::cosh::DECLARED,
            math::tanh::DECLARED,
            math::asinh::DECLARED,
            math::acosh::DECLARED,
            math::expm1::DECLARED,
            math::log1p::DECLARED,
            math::pi::DECLARED,
            math::is_finite::DECLARED,
            math::is_nan::DECLARED,
            math::intdiv::DECLARED,
            math::is_infinite::DECLARED,
            math::pow::DECLARED,
            math::exp::DECLARED,
            math::log::DECLARED,
            math::log10::DECLARED,
            math::sqrt::DECLARED,
            math::hypot::DECLARED,
            math::deg2rad::DECLARED,
            math::rad2deg::DECLARED,
            math::bindec::DECLARED,
            math::hexdec::DECLARED,
            math::octdec::DECLARED,
            math::decbin::DECLARED,
            math::decoct::DECLARED,
            math::dechex::DECLARED,
            math::base_convert::DECLARED,
            math::number_format::DECLARED,
            math::fmod::DECLARED,
            math::fdiv::DECLARED,
            math::fpow::DECLARED,
            microtime::microtime::DECLARED,
            microtime::gettimeofday::DECLARED,
            microtime::getrusage::DECLARED,
            pack::pack::DECLARED,
            pack::unpack::DECLARED,
            password::password_get_info::DECLARED,
            password::password_hash::DECLARED,
            password::password_needs_rehash::DECLARED,
            password::password_verify::DECLARED,
            password::password_algos::DECLARED,
            proc_open::proc_open::DECLARED,
            proc_open::proc_close::DECLARED,
            proc_open::proc_terminate::DECLARED,
            proc_open::proc_get_status::DECLARED,
            quot_print::quoted_printable_decode::DECLARED,
            quot_print::quoted_printable_encode::DECLARED,
            soundex::soundex::DECLARED,
            select::stream_select::DECLARED,
            streamsfuncs::stream_context_create::DECLARED,
            streamsfuncs::stream_context_set_params::DECLARED,
            streamsfuncs::stream_context_get_params::DECLARED,
            streamsfuncs::stream_context_set_option::DECLARED,
            streamsfuncs::stream_context_set_options::DECLARED,
            streamsfuncs::stream_context_get_options::DECLARED,
            streamsfuncs::stream_context_get_default::DECLARED,
            streamsfuncs::stream_context_set_default::DECLARED,
            user_filters::stream_filter_prepend::DECLARED,
            user_filters::stream_filter_append::DECLARED,
            user_filters::stream_filter_remove::DECLARED,
            sockets::stream_socket_client::DECLARED,
            sockets::stream_socket_server::DECLARED,
            sockets::stream_socket_accept::DECLARED,
            sockets::stream_socket_get_name::DECLARED,
            sockets::stream_socket_recvfrom::DECLARED,
            sockets::stream_socket_sendto::DECLARED,
            sockets::stream_socket_enable_crypto::DECLARED,
            sockets::stream_socket_shutdown::DECLARED,
            sockets::stream_socket_pair::DECLARED,
            streamsfuncs::stream_copy_to_stream::DECLARED,
            streamsfuncs::stream_get_contents::DECLARED,
            streamsfuncs::stream_supports_lock::DECLARED,
            streamsfuncs::stream_set_write_buffer::DECLARED,
            streamsfuncs::set_file_buffer::DECLARED,
            streamsfuncs::stream_set_read_buffer::DECLARED,
            streamsfuncs::stream_set_blocking::DECLARED,
            streamsfuncs::socket_set_blocking::DECLARED,
            file::stream_get_meta_data::DECLARED,
            file::socket_get_status::DECLARED,
            streamsfuncs::stream_get_line::DECLARED,
            streamsfuncs::stream_resolve_include_path::DECLARED,
            streamsfuncs::stream_get_wrappers::DECLARED,
            sockets::stream_get_transports::DECLARED,
            streamsfuncs::stream_is_local::DECLARED,
            select::stream_isatty::DECLARED,
            streamsfuncs::stream_set_chunk_size::DECLARED,
            streamsfuncs::stream_set_timeout::DECLARED,
            streamsfuncs::socket_set_timeout::DECLARED,
            r#type::gettype::DECLARED,
            r#type::get_debug_type::DECLARED,
            r#type::settype::DECLARED,
            r#type::intval::DECLARED,
            r#type::floatval::DECLARED,
            r#type::doubleval::DECLARED,
            r#type::boolval::DECLARED,
            r#type::strval::DECLARED,
            r#type::is_null::DECLARED,
            r#type::is_resource::DECLARED,
            r#type::is_bool::DECLARED,
            r#type::is_int::DECLARED,
            r#type::is_integer::DECLARED,
            r#type::is_long::DECLARED,
            r#type::is_float::DECLARED,
            r#type::is_double::DECLARED,
            r#type::is_numeric::DECLARED,
            r#type::is_string::DECLARED,
            r#type::is_array::DECLARED,
            r#type::is_object::DECLARED,
            r#type::is_scalar::DECLARED,
            r#type::is_callable::DECLARED,
            r#type::is_iterable::DECLARED,
            r#type::is_countable::DECLARED,
            uniqid::uniqid::DECLARED,
            url::parse_url::DECLARED,
            url::urlencode::DECLARED,
            url::urldecode::DECLARED,
            url::rawurlencode::DECLARED,
            url::rawurldecode::DECLARED,
            url::get_headers::DECLARED,
            user_filters::stream_bucket_make_writeable::DECLARED,
            user_filters::stream_bucket_prepend::DECLARED,
            user_filters::stream_bucket_append::DECLARED,
            user_filters::stream_bucket_new::DECLARED,
            user_filters::stream_get_filters::DECLARED,
            user_filters::stream_filter_register::DECLARED,
            uuencode::convert_uuencode::DECLARED,
            uuencode::convert_uudecode::DECLARED,
            var::var_export::DECLARED,
            var::debug::debug_zval_dump::DECLARED,
            var::serialize::serialize::DECLARED,
            var::unserialize::unserialize::DECLARED,
            versioning::version_compare::DECLARED,
        ])
        // `PHP_MINIT(basic)`'s order (`Z/ext/standard/basic_functions.c:292`):
        // `register_basic_functions_symbols`, the stub's constants from
        // `EXTR_*` to `CODESET` over the modules that read them, then the
        // submodules' own, `file`, `user_filters`, `password`, `dir` and
        // `user_streams`; the image handlers' after all, where
        // `php_image_register_handler` adds them.
        .constants(array::constants::DECLARED)
        .constants(assert::constants::DECLARED)
        .constants(basic_functions::constants::DECLARED)
        .constants(url::constants::DECLARED)
        .constants(math::constants::DECLARED)
        .constants(crypt::constants::DECLARED)
        .constants(dns::constants::DECLARED)
        .constants(html::constants::DECLARED)
        .constants(image::constants::DECLARED)
        .constants(info::constants::DECLARED)
        .constants(syslog::constants::DECLARED)
        .constants(string::constants::DECLARED)
        .constants(file::constants::DECLARED)
        .constants(user_filters::constants::DECLARED)
        .constants(password::constants::DECLARED)
        .constants(dir::constants::DECLARED)
        .constants(userspace::constants::DECLARED)
        .constants_with::<ImageHandler>(image::link_constants)
        // The classes as `PHP_MINIT(basic)` registers them.
        .class::<var::__PHP_Incomplete_Class>()
        .class::<assert::AssertionError>()
        .class::<math::RoundingMode>()
        .class::<user_filters::php_user_filter>()
        .class::<user_filters::StreamBucket>()
        .class::<dir::Directory>()
        .ini(var::ini())
        .ini(http_fopen_wrapper::ini())
        .ini(sockets::ini())
        .ini(file::ini())
        .dependencies(dependencies())
        .ini(assert::ini())
        .ini(url_scanner::ini())
        .register(php_ext::request::Syslog(syslog::php_syslog));
    ini_handlers()
        .into_iter()
        .fold(builder, |b, (name, handler)| b.on_modify(name, handler))
        .on_update_long(b"default_socket_timeout", LongHandler::Any)
        .on_update_long(b"unserialize_max_depth", LongHandler::Any)
        .request_state::<State>()
        .engine_state(throwables::Throwables::start)
        .stream_hooks(STREAM_HOOKS)
        .info(basic_functions::minfo)
        .rshutdown(rshutdown)
        .rinit(rinit)
        .build("standard")
}

/// `owner` given up past `thrown`, as php's `zval_ptr_dtor` and
/// `zend_array_destroy` after a `zend_call_function` that threw free it
/// with `EG(exception)` set: a destructor that throws there chains the
/// callback's throwable as its `previous`, and that is what is pending
/// after, `thrown` given up as the native's `Err` meets it. A `thrown`
/// that carries no throwable, a fatal, an `exit` or one the slot holds
/// already, gives `owner` up as it stands.
pub(crate) fn given_up_past<T: View>(req: &mut Request, thrown: &Thrown, owner: Owner<T>) {
    match thrown.exception() {
        Some(_) => {
            while_pending(req, thrown, |req| owner.give_up(req));
        }
        None => owner.give_up(req),
    }
}

/// The `on_modify` of each entry that has one, php's `PHP_INI_MH` at
/// runtime: `file.c`'s `auto_detect_line_endings`, the URL rewriter's
/// two over session's entries, and the five of `assert.c`.
fn ini_handlers() -> [(&'static [u8], OnModify); 8] {
    [
        (
            b"auto_detect_line_endings",
            file::on_auto_detect_line_endings,
        ),
        (b"session.trans_sid_tags", url_scanner::on_session_tags),
        (b"session.trans_sid_hosts", url_scanner::on_session_hosts),
        (b"assert.active", assert::on_active),
        (b"assert.bail", assert::on_bail),
        (b"assert.warning", assert::on_warning),
        (b"assert.callback", assert::on_callback),
        (b"assert.exception", assert::on_exception),
    ]
}

/// `PHP_RINIT(basic)`'s share here: the wrappers and the filters, which
/// php registers as the engine starts and this engine keeps in the
/// request's wrapper and filter tables (`filters::register`). The
/// wrappers go in as `PHP_MINIT(basic)` registers them
/// (`Z/ext/standard/basic_functions.c:341-346`): `php`, `file`, `glob`,
/// `data`, `http`, `ftp`, the stream layer keeping the kinds of `file`
/// and `data`. The `assert.*` entries' startup deprecations are their
/// on-modify rules, which the engine serves over the host's values as
/// it starts.
///
/// ```php
/// echo implode(' ', array_intersect(stream_get_wrappers(), ['php', 'file', 'glob', 'data', 'http', 'ftp'])), "\n";
/// ```
/// ```expect
/// php file glob data http ftp
/// ```
#[allow(clippy::unnecessary_wraps)] // `ExtensionBuilder::rinit`'s signature
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    req.register_wrapper(php_fopen_wrapper::wrapper());
    req.register_engine_wrapper(EngineWrapper::Plain);
    req.register_wrapper(dir::glob_wrapper());
    req.register_engine_wrapper(EngineWrapper::Data);
    req.register_wrapper(http_fopen_wrapper::wrapper());
    req.register_wrapper(ftp_fopen_wrapper::wrapper());
    filters::register(req);
    Ok(())
}

/// `PHP_RSHUTDOWN(basic)`'s share here, in its order: `DIRG(default_dir)`
/// given up, the syslog closed, `ASSERTG(callback)` given up, the URL
/// rewriter's buffers freed and `BG(last_http_headers)` given back.
fn rshutdown(req: &mut Request) {
    file::set_default_dir(req, None);
    syslog::close(req);
    assert::rshutdown(req);
    url_scanner::rshutdown(req);
    http_fopen_wrapper::clear_last_headers(req);
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
