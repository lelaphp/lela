//! The `zlib` extension: `php-src/ext/zlib` as one crate under `ext/`
//! (ADR-0004), over the platform's zlib bound in [`ffi`] alone (design
//! §7.2, revision 34). Its state on the value (ADR-0005). The phpt subset
//! is `php-src/ext/zlib/tests`, through `xtask phpt --subset`.
//!
//! Declared by the attributes as `zlib.stub.php` at the tag declares it
//! and `php -n --re zlib` renders it: the constants, the one-shot
//! encoders and decoders ([`encode`]), the incremental `DeflateContext`
//! and `InflateContext` ([`context`]), the `gz*` file natives over the
//! `compress.zlib://` wrapper's stream ([`gz`]), the `zlib.*` stream
//! filters ([`filter`]) and the `zlib output compression` buffer handler with
//! `ob_gzhandler` ([`output`]).
//!
//! The library is the platform's zlib and not a pure-Rust deflate: php's
//! tests observe the compressed bytes, which follow the linked zlib's
//! algorithm, and on #950's measurement over eight inputs, every level,
//! strategy, window and memory level `deflate_init` takes (31,680 rows),
//! the platform's zlib 1.2.12 wrote the oracle's bytes on every row,
//! zlib-rs on 22,459 (its level 0 and `ZLIB_HUFFMAN_ONLY` alone whole)
//! and `miniz_oxide` on 7,684.
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

mod context;
mod encode;
mod ffi;
mod filter;
mod gz;
mod output;
mod throwables;

/// The values php compiles in from `<zlib.h>`, read by `build.rs` off the
/// header a C build finds, as php's `@cvalue` reads the macro at its build
/// (#1974): after the library is upgraded without a rebuild, these keep
/// the header's version as php's do.
mod header {
    include!(concat!(env!("OUT_DIR"), "/header.rs"));
}

use php_ext::abi::{Extension, ExtensionBuilder, LongHandler};
use php_ext::attr::php_const;
use php_ext::decl::IniDecl;
use php_ext::raising::Thrown;
use php_ext::request::{Request, Streams as _};

/// `register_zlib_symbols`: the constants in `zlib.stub.php`'s order,
/// `ZLIB_VERSION` and `ZLIB_VERNUM` the header's (see [`header`]).
///
/// ```php
/// var_dump(FORCE_GZIP, ZLIB_ENCODING_RAW, ZLIB_BLOCK, ZLIB_FINISH, ZLIB_FIXED, ZLIB_VERSION, ZLIB_VERNUM, ZLIB_VERSION_ERROR);
/// ```
/// ```expect
/// int(31)
/// int(-15)
/// int(5)
/// int(4)
/// int(4)
/// string(6) "1.2.12"
/// int(4800)
/// int(-6)
/// ```
#[php_const]
#[allow(non_snake_case)]
pub(crate) mod constants {
    pub(crate) const FORCE_GZIP: i64 = 0x1f;
    pub(crate) const FORCE_DEFLATE: i64 = 0x0f;
    pub(crate) const ZLIB_ENCODING_RAW: i64 = -0xf;
    pub(crate) const ZLIB_ENCODING_GZIP: i64 = 0x1f;
    pub(crate) const ZLIB_ENCODING_DEFLATE: i64 = 0x0f;
    pub(crate) const ZLIB_NO_FLUSH: i64 = 0;
    pub(crate) const ZLIB_PARTIAL_FLUSH: i64 = 1;
    pub(crate) const ZLIB_SYNC_FLUSH: i64 = 2;
    pub(crate) const ZLIB_FULL_FLUSH: i64 = 3;
    pub(crate) const ZLIB_BLOCK: i64 = 5;
    pub(crate) const ZLIB_FINISH: i64 = 4;
    pub(crate) const ZLIB_FILTERED: i64 = 1;
    pub(crate) const ZLIB_HUFFMAN_ONLY: i64 = 2;
    pub(crate) const ZLIB_RLE: i64 = 3;
    pub(crate) const ZLIB_FIXED: i64 = 4;
    pub(crate) const ZLIB_DEFAULT_STRATEGY: i64 = 0;
    /// `ZLIB_VERSION`, `@cvalue` of the header macro: the version of the
    /// `zlib.h` this build compiled against, not the zlib it loads
    /// (`zlibVersion`, `--ri`'s "Linked Version").
    pub(crate) const ZLIB_VERSION: &str = super::header::ZLIB_VERSION;
    /// `ZLIB_VERNUM`, `@cvalue` of the header macro, `0xMmrs`, of the
    /// same header.
    pub(crate) const ZLIB_VERNUM: i64 = super::header::ZLIB_VERNUM;
    pub(crate) const ZLIB_OK: i64 = 0;
    pub(crate) const ZLIB_STREAM_END: i64 = 1;
    pub(crate) const ZLIB_NEED_DICT: i64 = 2;
    pub(crate) const ZLIB_ERRNO: i64 = -1;
    pub(crate) const ZLIB_STREAM_ERROR: i64 = -2;
    pub(crate) const ZLIB_DATA_ERROR: i64 = -3;
    pub(crate) const ZLIB_MEM_ERROR: i64 = -4;
    pub(crate) const ZLIB_BUF_ERROR: i64 = -5;
    pub(crate) const ZLIB_VERSION_ERROR: i64 = -6;
}

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised. The request's half of `zend_zlib_globals`.
#[derive(Default)]
struct State {
    output: output::Globals,
}

/// The extension value (design §7.1): the ini entries, the constants,
/// the natives and the two classes in `zlib.stub.php`'s order, the
/// wrapper and the filters `MINIT` registers, and the state declared on
/// it.
///
/// ```php
/// var_dump(extension_loaded('zlib'), in_array('compress.zlib', stream_get_wrappers()), in_array('zlib.*', stream_get_filters()));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
pub fn extension() -> Extension {
    let ini = |name: &'static str, default: &'static str| IniDecl {
        name: name.as_bytes(),
        default: Some(default.as_bytes()),
        user: true,
        per_dir: true,
        system: true,
    };
    ExtensionBuilder::default()
        .ini(vec![
            ini(output::COMPRESSION, "0"),
            ini(output::LEVEL, "-1"),
            ini(output::HANDLER, ""),
        ])
        .on_modify(output::COMPRESSION.as_bytes(), output::on_compression)
        .on_modify(output::HANDLER.as_bytes(), output::on_handler)
        .on_update_long(output::LEVEL.as_bytes(), LongHandler::Any)
        .constants(constants::DECLARED)
        .functions(&[
            output::ob_gzhandler::DECLARED,
            output::zlib_get_coding_type::DECLARED,
            gz::gzfile::DECLARED,
            gz::gzopen::DECLARED,
            gz::readgzfile::DECLARED,
            encode::zlib_encode::DECLARED,
            encode::zlib_decode::DECLARED,
            encode::gzdeflate::DECLARED,
            encode::gzencode::DECLARED,
            encode::gzcompress::DECLARED,
            encode::gzinflate::DECLARED,
            encode::gzdecode::DECLARED,
            encode::gzuncompress::DECLARED,
            gz::gzwrite::DECLARED,
            gz::gzputs::DECLARED,
            gz::gzrewind::DECLARED,
            gz::gzclose::DECLARED,
            gz::gzeof::DECLARED,
            gz::gzgetc::DECLARED,
            gz::gzpassthru::DECLARED,
            gz::gzseek::DECLARED,
            gz::gztell::DECLARED,
            gz::gzread::DECLARED,
            gz::gzgets::DECLARED,
            context::deflate_init::DECLARED,
            context::deflate_add::DECLARED,
            context::inflate_init::DECLARED,
            context::inflate_add::DECLARED,
            context::inflate_get_status::DECLARED,
            context::inflate_get_read_len::DECLARED,
        ])
        .class::<context::InflateContext>()
        .class::<context::DeflateContext>()
        .register(output::OB_GZHANDLER)
        .request_state::<State>()
        .engine_state(throwables::Throwables::start)
        .rinit(rinit)
        .rshutdown(rshutdown)
        .info(info)
        .build("zlib")
}

/// `PHP_MINIT_FUNCTION(zlib)`'s registrations, made per request as the
/// engine keeps its wrapper and filter tables, and `PHP_RINIT_FUNCTION`:
/// `zlib output compression` started where the ini asks for it.
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    req.register_wrapper(gz::wrapper());
    req.register_filter(filter::NAME, filter::factory);
    output::rinit(req)
}

/// `PHP_RSHUTDOWN_FUNCTION(zlib)`: `ob_gzhandler()`'s own stream given
/// back.
fn rshutdown(req: &mut Request) {
    req.state::<State>().output = output::Globals::default();
}

/// `PHP_MINFO_FUNCTION(zlib)`: the table, then `DISPLAY_INI_ENTRIES`,
/// `zlib.output_compression` shown as the boolean its displayer prints.
/// "Compiled Version" is `ZLIB_VERSION`, the header's ([`header`]);
/// "Linked Version" is the loaded zlib's, `zlibVersion`.
///
/// ```php
/// $e = new ReflectionExtension('zlib'); ob_start(); $e->info(); echo ob_get_clean();
/// ini_set('zlib.output_compression_level', '6');
/// $e = new ReflectionExtension('zlib'); ob_start(); $e->info(); echo strstr(ob_get_clean(), 'Directive');
/// ```
/// ```expect
/// zlib
///
/// ZLib Support => enabled
/// Stream Wrapper => compress.zlib://
/// Stream Filter => zlib.inflate, zlib.deflate
/// Compiled Version => 1.2.12
/// Linked Version => 1.2.12
///
/// Directive => Local Value => Master Value
/// zlib.output_compression => Off => Off
/// zlib.output_compression_level => -1 => -1
/// zlib.output_handler => no value => no value
/// Directive => Local Value => Master Value
/// zlib.output_compression => Off => Off
/// zlib.output_compression_level => 6 => -1
/// zlib.output_handler => no value => no value
/// ```
fn info(req: &mut Request) {
    let linked = ffi::version();
    req.extensions_mut().info_table(&[
        &["ZLib Support", "enabled"],
        &["Stream Wrapper", "compress.zlib://"],
        &["Stream Filter", "zlib.inflate, zlib.deflate"],
        &["Compiled Version", header::ZLIB_VERSION],
        &["Linked Version", &linked],
    ]);
    output::info_ini(req);
}
