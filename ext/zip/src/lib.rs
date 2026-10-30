//! The `zip` extension: `php-src/ext/zip` as one crate under `ext/`
//! (ADR-0004), behind the `zip` feature as php builds it `--with-zip`.
//! `php_zip.c` and `zip_stream.c` ported over libzip's behaviour,
//! re-derived in Rust where php's tests read it (design §7.2, revision
//! 70): the archive and its changes held until `close` ([`archive`],
//! [`write`]) by the object and its entries' streams together
//! ([`holder`]), libzip's codes and sentences ([`error`]), its strings
//! ([`strings`]) and DOS times ([`dostime`]); the central directory
//! found and read as libzip's `zip_open` reads it ([`cdir`]), every entry
//! read as its stream is read through libzip's layers ([`read`]), and
//! written through them ([`codec`]), over pure-Rust ciphers and bzip2,
//! and over the platform's libraries libzip links for deflate, LZMA, XZ
//! and ZSTD ([`ffi`], the crate's one home of `unsafe`; #1657, #3529),
//! driven as `zip_algorithm_xz.c` ([`xz`]) and `zip_algorithm_zstd.c`
//! ([`zstd`]) drive them. The phpt subset is `php-src/ext/zip/tests`,
//! through `xtask phpt --subset`.
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

mod archive;
mod cdir;
mod codec;
mod dostime;
mod error;
mod extract;
mod ffi;
mod holder;
mod object;
mod pattern;
mod pipe;
mod procedural;
mod read;
mod stream;
mod strings;
mod throwables;
mod write;
mod xz;
mod zstd;

use php_ext::abi::{Dependency, DependencyKind, Extension, ExtensionBuilder};
use php_ext::attr::php_class;
use php_ext::raising::Thrown;
use php_ext::request::{Request, Streams as _};
use php_ext::values::ResourceType;

/// php's C casts of a `zend_long` into libzip's types, which wrap as C's
/// conversions wrap, and of a libzip size back into a `zend_long`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "php_zip.c's own casts, `(zip_flags_t)`, `(int)` and `(zend_long)` among them"
)]
mod c {
    pub(crate) fn u32(v: i64) -> u32 {
        v as u32
    }
    pub(crate) fn i32(v: i64) -> i32 {
        v as i32
    }
    pub(crate) fn u16(v: i64) -> u16 {
        v as u16
    }
    pub(crate) fn u8(v: i64) -> u8 {
        v as u8
    }
    pub(crate) fn u64(v: i64) -> u64 {
        v as u64
    }
    pub(crate) fn long(v: u64) -> i64 {
        v as i64
    }
    pub(crate) fn float_long(v: f64) -> i64 {
        v as i64
    }
}

/// `PHP_ZIP_VERSION`.
const VERSION: &str = "1.22.8";

/// libzip's `LIBZIP_VERSION`, the `zipconf.h` the oracle's php was built
/// against. This crate binds no libzip and re-derives its behaviour, so
/// the two versions php reads are the oracle's, named here once each
/// beside each other.
const LIBZIP_HEADERS: &str = "1.12.1";

/// `zip_libzip_version()`, the libzip the oracle's php loads: the
/// closure frozen on 2026-10-24 carries 1.12.1 (oracle.toml `[closure]`),
/// which is what `ZipArchive::LIBZIP_VERSION` answers and what the
/// information table compares the headers with.
const LIBZIP_LIBRARY: &str = "1.12.1";

/// `zend_ce_countable`, which `ZipArchive` implements.
#[php_class(extern)]
pub(crate) struct Countable;

/// Request-scoped (ADR-0005): the two resource types, registered on
/// first use, and the archives `ZipArchive` opened, each held by its
/// object and its entries' streams ([`holder`]).
#[derive(Default)]
struct State {
    dir: Option<ResourceType>,
    entry: Option<ResourceType>,
    holders: holder::Holders,
}

/// The extension value (design §7.1): the ten procedural natives,
/// `ZipArchive`, the `zip://` wrapper and php's `ZEND_MOD_REQUIRED("pcre")`.
///
/// ```php
/// var_dump(extension_loaded('zip'), in_array('zip', stream_get_wrappers()), ZipArchive::LIBZIP_VERSION);
/// var_dump((new ReflectionExtension('zip'))->getDependencies(), phpversion('zip'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(6) "1.12.1"
/// array(1) {
///   ["pcre"]=>
///   string(8) "Required"
/// }
/// string(6) "1.22.8"
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            procedural::zip_open::declared(),
            procedural::zip_close::declared(),
            procedural::zip_read::declared(),
            procedural::zip_entry_open::declared(),
            procedural::zip_entry_close::declared(),
            procedural::zip_entry_read::declared(),
            procedural::zip_entry_name::declared(),
            procedural::zip_entry_compressedsize::declared(),
            procedural::zip_entry_filesize::declared(),
            procedural::zip_entry_compressionmethod::declared(),
        ])
        .class::<object::ZipArchive>()
        .dependencies(vec![Dependency {
            name: String::from("pcre"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .request_state::<State>()
        .engine_state(throwables::Throwables::start)
        .rinit(rinit)
        .info(info)
        .version(VERSION)
        .build("zip")
}

/// `PHP_MINIT_FUNCTION(zip)`'s wrapper, registered per request as the
/// engine keeps its wrapper table.
#[allow(clippy::unnecessary_wraps)] // the ABI's `rinit` hook
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    req.register_wrapper(stream::wrapper());
    Ok(())
}

/// `PHP_MINFO_FUNCTION(zip)`: the version and the methods this build
/// compresses and encrypts with. libzip's is one row where the headers
/// and the library agree, and a row for each where they differ, as php
/// compares `LIBZIP_VERSION` with `zip_libzip_version()`
/// (`Z/ext/zip/php_zip.c:3204-3211`).
///
/// ```php
/// (new ReflectionExtension('zip'))->info();
/// ```
/// ```expect
///
/// zip
///
/// Zip => enabled
/// Zip version => 1.22.8
/// Libzip version => 1.12.1
/// BZIP2 compression => Yes
/// XZ compression => Yes
/// ZSTD compression => Yes
/// AES-128 encryption => Yes
/// AES-192 encryption => Yes
/// AES-256 encryption => Yes
/// ```
fn info(req: &mut Request) {
    let yes = |supported: bool| if supported { "Yes" } else { "No" };
    let bzip2 = yes(archive::compression_supported(12));
    let xz = yes(archive::compression_supported(95));
    let zstd = yes(archive::compression_supported(93));
    let aes = yes(archive::encryption_supported(0x0101));
    let head = [["Zip", "enabled"], ["Zip version", VERSION]];
    let libzip: &[[&str; 2]] = if LIBZIP_HEADERS == LIBZIP_LIBRARY {
        &[["Libzip version", LIBZIP_HEADERS]]
    } else {
        &[
            ["Libzip headers version", LIBZIP_HEADERS],
            ["Libzip library version", LIBZIP_LIBRARY],
        ]
    };
    let methods = [
        ["BZIP2 compression", bzip2],
        ["XZ compression", xz],
        ["ZSTD compression", zstd],
        ["AES-128 encryption", aes],
        ["AES-192 encryption", aes],
        ["AES-256 encryption", aes],
    ];
    let rows: Vec<&[&str]> = head
        .iter()
        .chain(libzip)
        .chain(&methods)
        .map(|row| &row[..])
        .collect();
    req.extensions_mut().info_table(&rows);
}
