//! The `phar` extension: `php-src/ext/phar` as one crate under `ext/`
//! (ADR-0004), php's own C carried over (design §7.2, revision 36). Its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/phar/tests`, through `xtask phpt --subset`.
//!
//! The archive model and the table of open archives ([`archive`],
//! [`table`]), the paths into them ([`path`]), the phar format's reader
//! with its signatures ([`open`], [`signature`]), the entries' contents
//! ([`entry`]) and the `phar://` wrapper ([`stream`]). Compression rides
//! on ext/zlib's stream filters and signatures on ext/hash, as php's do;
//! bz2 is not in the bundle, so what needs it is refused as php refuses
//! it without bz2.
#![forbid(unsafe_code)]
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
// The formats are php's C: sizes and offsets are the 32-bit fields the
// archives carry.
#![allow(clippy::cast_possible_truncation)]

mod archive;
mod build;
mod convert;
mod entry;
mod extract;
mod fileinfo;
mod intercept;
mod object;
mod open;
mod path;
mod signature;
mod stream;
mod stub;
mod table;
mod tar;
mod web;
mod write;
mod zip;

use php_ext::abi::{Dependency, DependencyKind, Extension, ExtensionBuilder};
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::Request;

use crate::archive::Archives;

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised. The request's half of `zend_phar_globals`.
#[derive(Default)]
struct State {
    archives: Archives,
    /// `PHAR_G(cwd)`: the directory, inside the archive, of the file the
    /// first include out of an archive opened; `None` for its root.
    cwd: Option<Vec<u8>>,
    /// `PHAR_G(cwd_init)`.
    cwd_init: bool,
    /// The private key `setSignatureAlgorithm()` was given, which an
    /// OpenSSL signature is made with (`PHAR_G(openssl_privatekey)`).
    privatekey: Option<Vec<u8>>,
    /// `PHAR_G(phar_SERVER_mung_list)`, the `web::MUNG_*` bits.
    mung: u8,
    /// `PHAR_G(intercepted)`: `Phar::interceptFileFuncs()` was called.
    intercepted: bool,
}

/// The two boolean entries: `phar.readonly` and `phar.require_hash`.
const READONLY: &str = "phar.readonly";
const REQUIRE_HASH: &str = "phar.require_hash";
const CACHE_LIST: &str = "phar.cache_list";

/// The extension value (design §7.1): the ini entries, the wrapper its
/// `MINIT` registers, and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('phar'), in_array('phar', stream_get_wrappers()), ini_get('phar.readonly'), ini_get('phar.require_hash'));
/// var_dump(ini_set('phar.readonly', '0'), ini_set('phar.require_hash', 0), ini_set('phar.cache_list', 'x'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(1) "1"
/// string(1) "1"
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
pub fn extension() -> Extension {
    let ini = |name: &str, default: &str, user: bool| IniDecl {
        name: Name(name.as_bytes().into()),
        default: default.as_bytes().into(),
        user,
        per_dir: user,
        system: true,
    };
    let dep = |name: &str, kind: DependencyKind| Dependency {
        name: name.into(),
        kind,
        rel: None,
        version: None,
    };
    ExtensionBuilder::default()
        .dependencies(vec![
            dep("apc", DependencyKind::Optional),
            dep("bz2", DependencyKind::Optional),
            dep("openssl", DependencyKind::Optional),
            dep("zlib", DependencyKind::Optional),
            dep("standard", DependencyKind::Optional),
            dep("hash", DependencyKind::Required),
            dep("spl", DependencyKind::Required),
        ])
        .ini(vec![
            ini(READONLY, "1", true),
            ini(REQUIRE_HASH, "1", true),
            ini(CACHE_LIST, "", false),
        ])
        .on_modify(READONLY.as_bytes(), on_readonly)
        .on_modify(REQUIRE_HASH.as_bytes(), on_require_hash)
        .class::<object::PharException>()
        .class::<object::Phar>()
        .class::<object::PharData>()
        .class::<fileinfo::PharFileInfo>()
        .request_state::<State>()
        .rinit(rinit)
        .rshutdown(rshutdown)
        .info(info)
        .build("Phar")
}

/// The entry's startup value, php's `readonly_orig` and
/// `require_hash_orig`.
fn master(req: &Request, name: &str) -> bool {
    let Some(id) = req.extension_named(b"phar") else {
        return true;
    };
    req.extension_ini(id)
        .into_iter()
        .find(|e| e.decl.name.0.as_ref() == name.as_bytes())
        .is_none_or(|e| Request::ini_parse_bool(e.original.as_deref().unwrap_or(&e.value)))
}

/// `phar_ini_modify_handler` for `phar.readonly`: at runtime an entry that
/// started on may not be turned off; a change reaches every open
/// executable archive's writable bit.
#[allow(clippy::unnecessary_wraps)] // the ABI's `OnModify`
fn on_readonly(req: &mut Request, value: &[u8]) -> Result<bool, Thrown> {
    let on = Request::ini_parse_bool(value);
    if req.at_startup() {
        return Ok(true);
    }
    if master(req, READONLY) && !on {
        return Ok(false);
    }
    for archive in req.state::<State>().archives.each_mut() {
        if !archive.is_data {
            archive.is_writeable = !on;
        }
    }
    Ok(true)
}

/// `phar_ini_modify_handler` for `phar.require_hash`.
#[allow(clippy::unnecessary_wraps)] // the ABI's `OnModify`
fn on_require_hash(req: &mut Request, value: &[u8]) -> Result<bool, Thrown> {
    let on = Request::ini_parse_bool(value);
    Ok(req.at_startup() || on || !master(req, REQUIRE_HASH))
}

/// `PHP_MINIT_FUNCTION(phar)`'s wrapper, made per request as the engine
/// keeps its wrapper table.
#[allow(clippy::unnecessary_wraps)] // the ABI's `rinit` hook
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    req.register_wrapper(stream::wrapper());
    Ok(())
}

/// `PHP_RSHUTDOWN_FUNCTION(phar)`: the name table destroyed, an archive
/// something still holds kept until it is let go, which after this is at
/// once (`request_done`).
fn rshutdown(req: &mut Request) {
    let names = req.state::<State>().archives.fname_map.keys();
    req.state::<State>().archives.alias_map.clear();
    for name in names {
        let table = &mut req.state::<State>().archives;
        let Some(id) = table.fname_map.remove(&name) else {
            continue;
        };
        let gone = table.get_mut(id).is_some_and(|a| {
            a.refcount -= 1;
            a.refcount < 0
        });
        if gone {
            archive::destroy(req, id);
        }
    }
    let state = req.state::<State>();
    state.archives.request_done = true;
    state.cwd = None;
    state.cwd_init = false;
}

/// `PHP_MINFO_FUNCTION(phar)`: the table, the credits box, then
/// `DISPLAY_INI_ENTRIES` with the booleans as their displayer prints
/// them.
///
/// ```php
/// $e = new ReflectionExtension('phar'); ob_start(); $e->info(); echo strstr(ob_get_clean(), 'Phar based');
/// ```
/// ```expect
/// Phar based on pear/PHP_Archive, original concept by Davey Shafik.
/// Phar fully realized by Gregory Beaver and Marcus Boerger.
/// Portions of tar implementation Copyright (c) 2003-2009 Tim Kientzle.
/// Directive => Local Value => Master Value
/// phar.readonly => On => On
/// phar.require_hash => On => On
/// phar.cache_list => no value => no value
/// ```
fn info(req: &mut Request) {
    let zlib = if open::has_zlib(req) {
        "enabled"
    } else {
        "disabled (install ext/zlib)"
    };
    let bz2 = if open::has_bz2(req) {
        "enabled"
    } else {
        "disabled (install ext/bz2)"
    };
    let openssl = if req.extension_named(b"openssl").is_some() {
        "enabled"
    } else {
        "disabled (install ext/openssl)"
    };
    req.info_table(&[
        &["Phar: PHP Archive support", "enabled"],
        &["Phar API version", archive::API_VERSION],
        &["Phar-based phar archives", "enabled"],
        &["Tar-based phar archives", "enabled"],
        &["ZIP-based phar archives", "enabled"],
        &["gzip compression", zlib],
        &["bzip2 compression", bz2],
        &["OpenSSL support", openssl],
    ]);
    req.write(b"\nPhar based on pear/PHP_Archive, original concept by Davey Shafik.\nPhar fully realized by Gregory Beaver and Marcus Boerger.\nPortions of tar implementation Copyright (c) 2003-2009 Tim Kientzle.");
    let entries = req.ini_entries();
    let shown = |name: &str, value: &[u8]| -> String {
        if name != CACHE_LIST {
            return if Request::ini_parse_bool(value) {
                "On"
            } else {
                "Off"
            }
            .into();
        }
        if value.is_empty() {
            "no value".into()
        } else {
            String::from_utf8_lossy(value).into_owned()
        }
    };
    let rows: Vec<[String; 3]> = [READONLY, REQUIRE_HASH, CACHE_LIST]
        .iter()
        .map(|name| {
            let entry = entries
                .iter()
                .find(|e| e.decl.name.0.as_ref() == name.as_bytes());
            let local = entry.map_or(&[][..], |e| &e.value[..]);
            let master = entry.map_or(&[][..], |e| e.original.as_deref().unwrap_or(&e.value[..]));
            [(*name).to_string(), shown(name, local), shown(name, master)]
        })
        .collect();
    let mut table: Vec<&[&str]> = vec![&["Directive", "Local Value", "Master Value"]];
    let cells: Vec<[&str; 3]> = rows
        .iter()
        .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
        .collect();
    table.extend(cells.iter().map(<[&str; 3]>::as_slice));
    req.info_table(&table);
}
