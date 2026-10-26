//! The `sqlite3` extension: `php-src/ext/sqlite3` as one crate under `ext/`
//! (ADR-0004), `sqlite3.c` over the libsqlite3 that `ext/pdo_sqlite` binds,
//! reached through that binding's safe types ([`ffi`]) as design §4.10 and
//! §7.2 have a second extension reach a library another binds: this crate
//! keeps no `src/ffi/` and no `unsafe`. php's sqlite3 links the library
//! beside `pdo_sqlite` and requires no extension, so the crate edge is the
//! library's and `ReflectionExtension` names no dependency. The phpt subset
//! is `php-src/ext/sqlite3/tests`, through `xtask phpt --subset`.
//!
//! **What an object holds.** A `SQLite3` holds a [`conn::Conn`]: whether
//! it ever opened, is open or was closed ([`conn::Link`]), php's
//! `initialised` and its `db` pointer as one state, and, open, the
//! connection, the functions and collations registered on it and the
//! statements prepared on it, php's `free_list`, which `close()`
//! finalizes. A `SQLite3Stmt` names its statement in that list by a
//! [`conn::StmtId`] and holds its bound parameters as a typed table
//! ([`stmt::Bound`]); a `SQLite3Result` holds its statement's object and
//! the column names php caches. A method that steps a statement takes
//! it out of the list for the length of the call, so the callbacks the
//! step runs reach the objects and never a borrow the call holds.
//!
//! **Errors.** `php_sqlite3_error` is [`conn::fail`]: a `SQLite3Exception`
//! with the library's code under `enableExceptions(true)`, an
//! `E_WARNING` otherwise, a throwable a callback left pending chained
//! under the `SQLite3Exception` or carried past the warning, as php leaves it in
//! `EG(exception)`.
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

mod callbacks;
mod conn;
mod result;
mod stmt;
mod throwables;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::{php_class, php_const};
use php_ext::decl::{IniDecl, Name};
use php_ext::request::{Ini, Request};
use php_ext_pdo_sqlite::ffi;

/// The two `sqlite3.*` entries, `PHP_INI_BEGIN` in php's order: the name,
/// the default, and whether `ini_set` may set it (`PHP_INI_USER`) where
/// the system alone may (`PHP_INI_SYSTEM`).
const INI: [(&str, Option<&str>, bool); 2] = [
    ("sqlite3.extension_dir", None, false),
    ("sqlite3.defensive", Some("1"), true),
];

/// The constants of `sqlite3.stub.php`, in its order, with the values
/// `register_sqlite3_symbols` gives them.
#[php_const]
mod constants {
    pub(crate) const SQLITE3_ASSOC: i64 = 1;
    pub(crate) const SQLITE3_NUM: i64 = 2;
    pub(crate) const SQLITE3_BOTH: i64 = 3;
    pub(crate) const SQLITE3_INTEGER: i64 = 1;
    pub(crate) const SQLITE3_FLOAT: i64 = 2;
    pub(crate) const SQLITE3_TEXT: i64 = 3;
    pub(crate) const SQLITE3_BLOB: i64 = 4;
    pub(crate) const SQLITE3_NULL: i64 = 5;
    pub(crate) const SQLITE3_OPEN_READONLY: i64 = 1;
    pub(crate) const SQLITE3_OPEN_READWRITE: i64 = 2;
    pub(crate) const SQLITE3_OPEN_CREATE: i64 = 4;
    pub(crate) const SQLITE3_DETERMINISTIC: i64 = 2048;
}

/// The extension value (design §7.1): the constants, the INI entries,
/// the four classes and the `MINFO` table.
///
/// ```php
/// var_dump(extension_loaded('sqlite3'), SQLITE3_BOTH, SQLITE3_OPEN_CREATE, ini_get('sqlite3.defensive'));
/// var_dump((new ReflectionExtension('sqlite3'))->getDependencies(), SQLite3::RECURSIVE);
/// ```
/// ```expect
/// bool(true)
/// int(3)
/// int(4)
/// string(1) "1"
/// array(0) {
/// }
/// int(33)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .ini(
            INI.iter()
                .map(|(name, default, user)| IniDecl {
                    name: Name(name.as_bytes().into()),
                    default: default.map(|d| d.as_bytes().into()),
                    user: *user,
                    per_dir: false,
                    system: !*user,
                })
                .collect(),
        )
        .constants(constants::declared())
        .class::<SQLite3Exception>()
        .class::<conn::SQLite3>()
        .class::<stmt::SQLite3Stmt>()
        .class::<result::SQLite3Result>()
        .request_state::<conn::Forgotten>()
        .engine_state(throwables::Throwables::start)
        .rshutdown(conn::rshutdown)
        .info(sqlite3_info)
        .build("sqlite3")
}

/// `PHP_MINFO_FUNCTION(sqlite3)`: the two rows and `DISPLAY_INI_ENTRIES`,
/// `sqlite3.defensive` through the boolean displayer.
///
/// ```php
/// (new ReflectionExtension('sqlite3'))->info();
/// ```
/// ```expect
/// sqlite3
///
/// SQLite3 support => enabled
/// SQLite Library => %s
///
/// Directive => Local Value => Master Value
/// sqlite3.extension_dir => no value => no value
/// sqlite3.defensive => On => On
/// ```
fn sqlite3_info(req: &mut Request) {
    let version = ffi::version();
    req.extensions_mut().info_table(&[
        &["SQLite3 support", "enabled"],
        &["SQLite Library", &version],
    ]);
    let entries = req.ini().entries();
    let shown = |name: &str, value: &[u8]| -> String {
        if name == "sqlite3.defensive" {
            let on = Ini::parse_bool(value);
            String::from(if on { "On" } else { "Off" })
        } else {
            String::from_utf8_lossy(value).into_owned()
        }
    };
    let rows: Vec<[String; 3]> = INI
        .iter()
        .filter_map(|(name, _, _)| {
            let entry = entries
                .iter()
                .find(|entry| entry.decl.name.0.as_ref() == name.as_bytes())?;
            Some([
                (*name).to_owned(),
                shown(name, entry.value.as_deref().unwrap_or_default()),
                shown(name, entry.master.as_deref().unwrap_or_default()),
            ])
        })
        .collect();
    let cells: Vec<[&str; 3]> = rows
        .iter()
        .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
        .collect();
    req.ini_mut().info_ini_table(&cells);
}

/// `class Exception`, Core's.
#[php_class(extern)]
pub(crate) struct Exception;

/// `class SQLite3Exception extends Exception`, `@strict-properties`: what
/// every error raises under `enableExceptions(true)`, the library's code
/// as its own.
///
/// ```php
/// $db = new SQLite3(':memory:');
/// $db->enableExceptions(true);
/// try { $db->exec('NOPE'); } catch (SQLite3Exception $e) { var_dump(get_parent_class($e), $e->getMessage(), $e->getCode()); }
/// ```
/// ```expect
/// string(9) "Exception"
/// string(25) "near "NOPE": syntax error"
/// int(1)
/// ```
#[php_class(extends(Exception), dynamic_properties = forbidden)]
pub(crate) struct SQLite3Exception;

#[php_class]
impl SQLite3Exception {}
