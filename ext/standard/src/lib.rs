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
//! calls itself (`count`, `chr`, `ord`, the `is_*` checks, #219) and
//! emits a frameless call for the natives that declare one (#254); what
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
/// base name of the file it reports (#1330).
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
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::{EngineWrapper, Request, STREAM_HOOKS, Streams};

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
pub fn extension() -> Extension {
    let builder = ExtensionBuilder::default()
        .functions(vec![
            basic_functions::header_register_callback::declared(),
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
            url_scanner::output_reset_rewrite_vars::declared(),
            url_scanner::output_add_rewrite_var::declared(),
            userspace::stream_wrapper_register::declared(),
            userspace::stream_register_wrapper::declared(),
            userspace::stream_wrapper_unregister::declared(),
            userspace::stream_wrapper_restore::declared(),
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
            r#type::array_is_list::declared(),
            base64::base64_encode::declared(),
            base64::base64_decode::declared(),
            basic_functions::constant::declared(),
            basic_functions::ip2long::declared(),
            basic_functions::long2ip::declared(),
            basic_functions::getenv::declared(),
            basic_functions::putenv::declared(),
            basic_functions::getopt::declared(),
            basic_functions::flush::declared(),
            basic_functions::sleep::declared(),
            basic_functions::usleep::declared(),
            basic_functions::time_nanosleep::declared(),
            basic_functions::time_sleep_until::declared(),
            basic_functions::get_current_user::declared(),
            basic_functions::get_cfg_var::declared(),
            basic_functions::error_log::declared(),
            basic_functions::error_get_last::declared(),
            basic_functions::error_clear_last::declared(),
            basic_functions::call_user_func::declared(),
            basic_functions::call_user_func_array::declared(),
            basic_functions::forward_static_call::declared(),
            basic_functions::forward_static_call_array::declared(),
            basic_functions::highlight_file::declared(),
            basic_functions::show_source::declared(),
            basic_functions::php_strip_whitespace::declared(),
            basic_functions::highlight_string::declared(),
            basic_functions::ini_get_all::declared(),
            basic_functions::ini_restore::declared(),
            basic_functions::ini_parse_quantity::declared(),
            basic_functions::set_include_path::declared(),
            basic_functions::get_include_path::declared(),
            var::print_r::declared(),
            basic_functions::connection_aborted::declared(),
            basic_functions::connection_status::declared(),
            basic_functions::ignore_user_abort::declared(),
            basic_functions::getservbyname::declared(),
            basic_functions::getservbyport::declared(),
            basic_functions::getprotobyname::declared(),
            basic_functions::getprotobynumber::declared(),
            basic_functions::register_tick_function::declared(),
            basic_functions::unregister_tick_function::declared(),
            basic_functions::is_uploaded_file::declared(),
            basic_functions::move_uploaded_file::declared(),
            basic_functions::parse_ini_file::declared(),
            basic_functions::parse_ini_string::declared(),
            basic_functions::sys_getloadavg::declared(),
            browscap::get_browser::declared(),
            crc32::crc32::declared(),
            crypt::crypt::declared(),
            datetime::strptime::declared(),
            dns::gethostname::declared(),
            dns::gethostbyaddr::declared(),
            dns::gethostbyname::declared(),
            dns::gethostbynamel::declared(),
            dns::dns_check_record::declared(),
            dns::checkdnsrr::declared(),
            dns::dns_get_record::declared(),
            dns::dns_get_mx::declared(),
            dns::getmxrr::declared(),
            net::net_get_interfaces::declared(),
            basic_functions::ftok::declared(),
            hrtime::hrtime::declared(),
            md5::md5::declared(),
            md5::md5_file::declared(),
            pageinfo::getmyuid::declared(),
            pageinfo::getmygid::declared(),
            pageinfo::getmypid::declared(),
            pageinfo::getmyinode::declared(),
            pageinfo::getlastmod::declared(),
            sha1::sha1::declared(),
            sha1::sha1_file::declared(),
            syslog::openlog::declared(),
            syslog::closelog::declared(),
            syslog::syslog::declared(),
            basic_functions::inet_ntop::declared(),
            basic_functions::inet_pton::declared(),
            metaphone::metaphone::declared(),
            head::header::declared(),
            head::header_remove::declared(),
            head::setrawcookie::declared(),
            head::setcookie::declared(),
            head::http_response_code::declared(),
            head::headers_sent::declared(),
            head::headers_list::declared(),
            html::htmlspecialchars::declared(),
            html::htmlspecialchars_decode::declared(),
            html::html_entity_decode::declared(),
            html::htmlentities::declared(),
            html::get_html_translation_table::declared(),
            assert::assert::declared(),
            assert::assert_options::declared(),
            string::bin2hex::declared(),
            string::hex2bin::declared(),
            string::strspn::declared(),
            string::strcspn::declared(),
            string::locale::nl_langinfo::declared(),
            string::locale::strcoll::declared(),
            string::trim::declared(),
            string::rtrim::declared(),
            string::chop::declared(),
            string::ltrim::declared(),
            string::wordwrap::declared(),
            string::explode::declared(),
            string::implode::declared(),
            string::join::declared(),
            string::strtok::declared(),
            string::strtoupper::declared(),
            string::strtolower::declared(),
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
            string::str_contains::declared(),
            string::find::str_starts_with::declared(),
            string::find::str_ends_with::declared(),
            string::chunk_split::declared(),
            string::substr::declared(),
            string::replace::substr_replace::declared(),
            string::quotemeta::declared(),
            string::ord::declared(),
            string::chr::declared(),
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
            string::str_repeat::declared(),
            string::count_chars::declared(),
            strnatcmp::strnatcmp::declared(),
            string::locale::localeconv::declared(),
            strnatcmp::strnatcasecmp::declared(),
            string::substr_count::declared(),
            string::str_pad::declared(),
            scanf::sscanf::declared(),
            string::str_rot13::declared(),
            string::str_shuffle::declared(),
            string::str_word_count::declared(),
            string::str_split::declared(),
            string::strpbrk::declared(),
            string::substr_compare::declared(),
            string::utf8_encode::declared(),
            string::utf8_decode::declared(),
            file::opendir::declared(),
            dir::dir::declared(),
            file::closedir::declared(),
            dir::chdir::declared(),
            dir::getcwd::declared(),
            file::rewinddir::declared(),
            file::readdir::declared(),
            dir::scandir::declared(),
            dir::glob::declared(),
            exec::exec::declared(),
            exec::system::declared(),
            exec::passthru::declared(),
            exec::escapeshellcmd::declared(),
            exec::escapeshellarg::declared(),
            exec::shell_exec::declared(),
            exec::proc_nice::declared(),
            file::flock::declared(),
            file::get_meta_tags::declared(),
            proc_open::pclose::declared(),
            proc_open::popen::declared(),
            file::readfile::declared(),
            file::rewind::declared(),
            file::rmdir::declared(),
            file::umask::declared(),
            file::fclose::declared(),
            file::feof::declared(),
            file::fgetc::declared(),
            file::fgets::declared(),
            file::fread::declared(),
            file::fopen::declared(),
            file::fscanf::declared(),
            file::fpassthru::declared(),
            file::ftruncate::declared(),
            file::fstat::declared(),
            file::fseek::declared(),
            file::ftell::declared(),
            file::fflush::declared(),
            file::fsync::declared(),
            file::fdatasync::declared(),
            file::fwrite::declared(),
            file::fputs::declared(),
            file::mkdir::declared(),
            file::rename::declared(),
            file::copy::declared(),
            file::tempnam::declared(),
            file::tmpfile::declared(),
            file::file::declared(),
            file::file_get_contents::declared(),
            file::unlink::declared(),
            file::file_put_contents::declared(),
            file::fputcsv::declared(),
            file::fgetcsv::declared(),
            file::realpath::declared(),
            file::fnmatch::declared(),
            file::sys_get_temp_dir::declared(),
            filestat::fileatime::declared(),
            filestat::filectime::declared(),
            filestat::filegroup::declared(),
            filestat::fileinode::declared(),
            filestat::filemtime::declared(),
            filestat::fileowner::declared(),
            filestat::fileperms::declared(),
            filestat::filesize::declared(),
            filestat::filetype::declared(),
            filestat::file_exists::declared(),
            filestat::is_writable::declared(),
            filestat::is_writeable::declared(),
            filestat::is_readable::declared(),
            filestat::is_executable::declared(),
            filestat::is_file::declared(),
            filestat::is_dir::declared(),
            filestat::is_link::declared(),
            filestat::stat::declared(),
            filestat::lstat::declared(),
            filestat::chown::declared(),
            filestat::chgrp::declared(),
            filestat::lchown::declared(),
            filestat::lchgrp::declared(),
            filestat::chmod::declared(),
            filestat::touch::declared(),
            filestat::clearstatcache::declared(),
            filestat::disk_total_space::declared(),
            filestat::disk_free_space::declared(),
            filestat::diskfreespace::declared(),
            filestat::realpath_cache_get::declared(),
            filestat::realpath_cache_size::declared(),
            formatted_print::sprintf::declared(),
            formatted_print::printf::declared(),
            formatted_print::vprintf::declared(),
            formatted_print::vsprintf::declared(),
            formatted_print::fprintf::declared(),
            formatted_print::vfprintf::declared(),
            sockets::fsockopen::declared(),
            sockets::pfsockopen::declared(),
            http::http_build_query::declared(),
            http::http_get_last_response_headers::declared(),
            http::http_clear_last_response_headers::declared(),
            http::request_parse_body::declared(),
            image::image_type_to_mime_type::declared(),
            image::image_type_to_extension::declared(),
            image::getimagesize::declared(),
            image::getimagesizefromstring::declared(),
            info::phpinfo::declared(),
            info::phpversion::declared(),
            info::phpcredits::declared(),
            info::php_sapi_name::declared(),
            info::php_uname::declared(),
            info::php_ini_scanned_files::declared(),
            info::php_ini_loaded_file::declared(),
            iptc::iptcembed::declared(),
            iptc::iptcparse::declared(),
            levenshtein::levenshtein::declared(),
            link::readlink::declared(),
            link::linkinfo::declared(),
            link::symlink::declared(),
            link::link::declared(),
            mail::mail::declared(),
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
            math::atanh::declared(),
            math::atan2::declared(),
            math::sinh::declared(),
            math::cosh::declared(),
            math::tanh::declared(),
            math::asinh::declared(),
            math::acosh::declared(),
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
            microtime::microtime::declared(),
            microtime::gettimeofday::declared(),
            microtime::getrusage::declared(),
            pack::pack::declared(),
            pack::unpack::declared(),
            password::password_get_info::declared(),
            password::password_hash::declared(),
            password::password_needs_rehash::declared(),
            password::password_verify::declared(),
            password::password_algos::declared(),
            proc_open::proc_open::declared(),
            proc_open::proc_close::declared(),
            proc_open::proc_terminate::declared(),
            proc_open::proc_get_status::declared(),
            quot_print::quoted_printable_decode::declared(),
            quot_print::quoted_printable_encode::declared(),
            soundex::soundex::declared(),
            select::stream_select::declared(),
            streamsfuncs::stream_context_create::declared(),
            streamsfuncs::stream_context_set_params::declared(),
            streamsfuncs::stream_context_get_params::declared(),
            streamsfuncs::stream_context_set_option::declared(),
            streamsfuncs::stream_context_set_options::declared(),
            streamsfuncs::stream_context_get_options::declared(),
            streamsfuncs::stream_context_get_default::declared(),
            streamsfuncs::stream_context_set_default::declared(),
            user_filters::stream_filter_prepend::declared(),
            user_filters::stream_filter_append::declared(),
            user_filters::stream_filter_remove::declared(),
            sockets::stream_socket_client::declared(),
            sockets::stream_socket_server::declared(),
            sockets::stream_socket_accept::declared(),
            sockets::stream_socket_get_name::declared(),
            sockets::stream_socket_recvfrom::declared(),
            sockets::stream_socket_sendto::declared(),
            sockets::stream_socket_enable_crypto::declared(),
            sockets::stream_socket_shutdown::declared(),
            sockets::stream_socket_pair::declared(),
            streamsfuncs::stream_copy_to_stream::declared(),
            streamsfuncs::stream_get_contents::declared(),
            streamsfuncs::stream_supports_lock::declared(),
            streamsfuncs::stream_set_write_buffer::declared(),
            streamsfuncs::set_file_buffer::declared(),
            streamsfuncs::stream_set_read_buffer::declared(),
            streamsfuncs::stream_set_blocking::declared(),
            streamsfuncs::socket_set_blocking::declared(),
            file::stream_get_meta_data::declared(),
            file::socket_get_status::declared(),
            streamsfuncs::stream_get_line::declared(),
            streamsfuncs::stream_resolve_include_path::declared(),
            streamsfuncs::stream_get_wrappers::declared(),
            sockets::stream_get_transports::declared(),
            streamsfuncs::stream_is_local::declared(),
            select::stream_isatty::declared(),
            streamsfuncs::stream_set_chunk_size::declared(),
            streamsfuncs::stream_set_timeout::declared(),
            streamsfuncs::socket_set_timeout::declared(),
            r#type::gettype::declared(),
            r#type::get_debug_type::declared(),
            r#type::settype::declared(),
            r#type::intval::declared(),
            r#type::floatval::declared(),
            r#type::doubleval::declared(),
            r#type::boolval::declared(),
            r#type::strval::declared(),
            r#type::is_null::declared(),
            r#type::is_resource::declared(),
            r#type::is_bool::declared(),
            r#type::is_int::declared(),
            r#type::is_integer::declared(),
            r#type::is_long::declared(),
            r#type::is_float::declared(),
            r#type::is_double::declared(),
            r#type::is_numeric::declared(),
            r#type::is_string::declared(),
            r#type::is_array::declared(),
            r#type::is_object::declared(),
            r#type::is_scalar::declared(),
            r#type::is_callable::declared(),
            r#type::is_iterable::declared(),
            r#type::is_countable::declared(),
            uniqid::uniqid::declared(),
            url::parse_url::declared(),
            url::urlencode::declared(),
            url::urldecode::declared(),
            url::rawurlencode::declared(),
            url::rawurldecode::declared(),
            url::get_headers::declared(),
            user_filters::stream_bucket_make_writeable::declared(),
            user_filters::stream_bucket_prepend::declared(),
            user_filters::stream_bucket_append::declared(),
            user_filters::stream_bucket_new::declared(),
            user_filters::stream_get_filters::declared(),
            user_filters::stream_filter_register::declared(),
            uuencode::convert_uuencode::declared(),
            uuencode::convert_uudecode::declared(),
            var::var_export::declared(),
            var::debug::debug_zval_dump::declared(),
            var::serialize::serialize::declared(),
            var::unserialize::unserialize::declared(),
            versioning::version_compare::declared(),
        ])
        // `PHP_MINIT(basic)`'s order (`Z/ext/standard/basic_functions.c:292`):
        // `register_basic_functions_symbols`, the stub's constants from
        // `EXTR_*` to `CODESET` over the modules that read them, then the
        // submodules' own, `file`, `user_filters`, `password`, `dir` and
        // `user_streams`; the image handlers' after all, where
        // `php_image_register_handler` adds them.
        .constants(
            [
                array::constants::declared(),
                assert::constants::declared(),
                basic_functions::constants::declared(),
                url::constants::declared(),
                math::constants::declared(),
                crypt::constants::declared(),
                dns::constants::declared(),
                html::constants::declared(),
                image::constants::declared(),
                info::constants::declared(),
                syslog::constants::declared(),
                string::constants::declared(),
                file::constants::declared(),
                user_filters::constants::declared(),
                password::constants::declared(),
                dir::constants::declared(),
                userspace::constants::declared(),
            ]
            .into_iter()
            .flatten()
            .collect(),
        )
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
        .stream_hooks(STREAM_HOOKS)
        .info(basic_functions::minfo)
        .rshutdown(rshutdown)
        .rinit(rinit)
        .build("standard")
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
