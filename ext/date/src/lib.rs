//! The `date` extension: `php-src/ext/date` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/date/tests`, through `xtask phpt --subset`.
//!
//! timelib is the library (design §7.2): compiled by `build.rs` from the
//! sources vendored at `lib/`, php-src's `ext/date/lib` at the pinned tag
//! with its bundled timezone database, and bound in [`ffi`], the crate's
//! one home of `unsafe`. Over it, `php_date.c`'s natives, declared as
//! `php_date.stub.php` declares them.
//!
//! What php keeps in `DATEG` is the request state: the zone
//! `date_default_timezone_set` chose, the zones parsed so far
//! (`DATEG(tzcache)`), and the messages of the last parse
//! (`DATEG(last_errors)`). The database itself is data compiled into the
//! binary, shared by every request and written by none.
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
// The binding's items the later natives of #656 read, before they land.
#![allow(dead_code)]

mod args;
mod ffi;
mod zone;

use std::collections::HashMap;
use std::sync::Arc;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::decl::{FunctionDecl, IniDecl, Name, Type};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Value;

use crate::args::{function, p};
use crate::ffi::{Errors, Tzdb, Zone};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised.
#[derive(Default)]
struct State {
    /// `DATEG(timezone)`, what `date_default_timezone_set` set.
    timezone: Option<Vec<u8>>,
    /// `DATEG(tzcache)`: every zone the request parsed, by the name it
    /// was asked for.
    zones: HashMap<Vec<u8>, Arc<Zone>>,
    /// `DATEG(last_errors)`: the messages of the last parse that had any.
    last_errors: Option<Errors>,
}

/// A native's entry, the shape `ExtensionBuilder::functions` takes.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// The extension value (design §7.1): declarations paired with entries,
/// and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('date'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .ini(ini())
        .functions(functions())
        .request_state::<State>()
        .info(date_info)
        .build("date")
}

/// `PHP_INI_BEGIN()` of `php_date.c`, every entry `PHP_INI_ALL`.
fn ini() -> Vec<IniDecl> {
    [
        ("date.timezone", "UTC"),
        ("date.default_latitude", "31.7667"),
        ("date.default_longitude", "35.2333"),
        ("date.sunset_zenith", "90.833333"),
        ("date.sunrise_zenith", "90.833333"),
    ]
    .into_iter()
    .map(|(name, default)| IniDecl {
        name: Name(name.as_bytes().into()),
        default: default.as_bytes().into(),
        user: true,
        per_dir: true,
        system: true,
    })
    .collect()
}

/// `PHP_MINFO_FUNCTION(date)`.
fn date_info(req: &mut Request) {
    let version = String::from_utf8_lossy(Tzdb::builtin().version()).into_owned();
    let default = String::from_utf8_lossy(&zone::guess_timezone(req)).into_owned();
    req.info_table(&[
        &["date/time support", "enabled"],
        &["timelib version", "2022.17"],
        &["\"Olson\" Timezone Database Version", &version],
        &["Timezone Database", "internal"],
        &["Default timezone", &default],
    ]);
}

/// The functions, in `php_date.stub.php`'s order.
fn functions() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            function(
                "date_default_timezone_set",
                vec![p("timezoneId", Type::String)],
                Some(Type::Bool),
            ),
            zone::date_default_timezone_set as Entry,
        ),
        (
            function("date_default_timezone_get", vec![], Some(Type::String)),
            zone::date_default_timezone_get,
        ),
    ]
}
