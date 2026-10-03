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

mod classes;
mod datetime;
mod ffi;
mod format;
mod held;
mod interval;
mod obj;
mod parse;
mod period;
mod sun;
mod timezone;
mod tzdb;
mod zone;

use std::collections::HashMap;
use std::sync::Arc;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::php_const;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::Request;

use crate::classes::{
    DateError, DateException, DateInvalidOperationException, DateInvalidTimeZoneException,
    DateMalformedIntervalStringException, DateMalformedPeriodStringException,
    DateMalformedStringException, DateObjectError, DateRangeError, DateTimeInterface,
};
use crate::datetime::{DateTime, DateTimeImmutable};
use crate::ffi::{Errors, Tzdb};
pub use crate::ffi::{Offset, Zone};
use crate::interval::DateInterval;
use crate::period::DatePeriod;
use crate::timezone::DateTimeZone;

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

/// `get_timezone_info()`, what `php_date.h` declares `PHPAPI` (design
/// §4.10): the request's default zone, `guess_timezone` parsed through
/// the request's cache, or php's `DateError` where the database cannot
/// give it. `ext/standard`'s `gettimeofday` reads its offset.
///
/// ```php
/// date_default_timezone_set('Asia/Kathmandu');
/// $t = gettimeofday(); var_dump($t['minuteswest'], $t['dsttime']);
/// ```
/// ```expect
/// int(-345)
/// int(0)
/// ```
///
/// # Errors
///
/// The thrown `DateError`.
pub fn get_timezone_info(req: &mut Request) -> Result<Arc<Zone>, Thrown> {
    zone::default_zone(req)
}

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
        .class::<DateTimeInterface>()
        .class::<DateTime>()
        .class::<DateTimeImmutable>()
        .class::<DateTimeZone>()
        .class::<DateInterval>()
        .class::<DatePeriod>()
        .class::<DateError>()
        .class::<DateObjectError>()
        .class::<DateRangeError>()
        .class::<DateException>()
        .class::<DateInvalidTimeZoneException>()
        .class::<DateInvalidOperationException>()
        .class::<DateMalformedStringException>()
        .class::<DateMalformedIntervalStringException>()
        .class::<DateMalformedPeriodStringException>()
        .ini(ini())
        // `php_format_date`, what php's `main` and ext/standard call
        // into ext/date for (`php_date.h`).
        .register(php_ext::request::FormatDate(format::format_timestamp))
        .constants(constants::declared())
        .functions(vec![
            parse::strtotime::declared(),
            format::date::declared(),
            format::idate::declared(),
            format::gmdate::declared(),
            format::mktime::declared(),
            format::gmmktime::declared(),
            format::checkdate::declared(),
            format::strftime::declared(),
            format::gmstrftime::declared(),
            format::time::declared(),
            format::localtime::declared(),
            format::getdate::declared(),
            datetime::date_create::declared(),
            datetime::date_create_immutable::declared(),
            datetime::date_create_from_format::declared(),
            datetime::date_create_immutable_from_format::declared(),
            parse::date_parse::declared(),
            parse::date_parse_from_format::declared(),
            datetime::date_get_last_errors::declared(),
            datetime::date_format::declared(),
            datetime::date_modify::declared(),
            datetime::date_add::declared(),
            datetime::date_sub::declared(),
            datetime::date_timezone_get::declared(),
            datetime::date_timezone_set::declared(),
            datetime::date_offset_get::declared(),
            datetime::date_diff::declared(),
            datetime::date_time_set::declared(),
            datetime::date_date_set::declared(),
            datetime::date_isodate_set::declared(),
            datetime::date_timestamp_set::declared(),
            datetime::date_timestamp_get::declared(),
            timezone::timezone_open::declared(),
            timezone::timezone_name_get::declared(),
            tzdb::timezone_name_from_abbr::declared(),
            timezone::timezone_offset_get::declared(),
            timezone::timezone_transitions_get::declared(),
            timezone::timezone_location_get::declared(),
            tzdb::timezone_identifiers_list::declared(),
            tzdb::timezone_abbreviations_list::declared(),
            tzdb::timezone_version_get::declared(),
            interval::date_interval_create_from_date_string::declared(),
            interval::date_interval_format::declared(),
            zone::date_default_timezone_set::declared(),
            zone::date_default_timezone_get::declared(),
            sun::date_sunrise::declared(),
            sun::date_sunset::declared(),
            sun::date_sun_info::declared(),
        ])
        .on_modify(b"date.timezone", zone::on_update_timezone)
        .request_state::<State>()
        .info(date_info)
        .build("date")
}

/// `PHP_INI_BEGIN()` of `php_date.c`: each entry and its default.
const INI: [(&str, &str); 5] = [
    ("date.timezone", "UTC"),
    ("date.default_latitude", "31.7667"),
    ("date.default_longitude", "35.2333"),
    ("date.sunset_zenith", "90.833333"),
    ("date.sunrise_zenith", "90.833333"),
];

/// The ini entries, every one `PHP_INI_ALL`.
fn ini() -> Vec<IniDecl> {
    INI.into_iter()
        .map(|(name, default)| IniDecl {
            name: Name(name.as_bytes().into()),
            default: Some(default.as_bytes().into()),
            user: true,
            per_dir: true,
            system: true,
        })
        .collect()
}

/// `PHP_MINFO_FUNCTION(date)`: the extension's table, then its ini
/// entries as `DISPLAY_INI_ENTRIES` prints them.
///
/// ```php
/// date_default_timezone_set('Europe/Berlin');
/// ini_set('date.default_latitude', '52.5');
/// (new ReflectionExtension('date'))->info();
/// ```
/// ```expect
/// date
///
/// date/time support => enabled
/// timelib version => 2022.17
/// "Olson" Timezone Database Version => 2026.3
/// Timezone Database => internal
/// Default timezone => Europe/Berlin
///
/// Directive => Local Value => Master Value
/// date.timezone => UTC => UTC
/// date.default_latitude => 52.5 => 31.7667
/// date.default_longitude => 35.2333 => 35.2333
/// date.sunset_zenith => 90.833333 => 90.833333
/// date.sunrise_zenith => 90.833333 => 90.833333
/// ```
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
    // `DISPLAY_INI_ENTRIES()`: each entry's request value beside its
    // master.
    let shown = |value: &[u8]| String::from_utf8_lossy(value).into_owned();
    let entries = req.ini_entries();
    let rows: Vec<[String; 3]> = INI
        .iter()
        .filter_map(|(name, _)| {
            entries
                .iter()
                .find(|entry| entry.decl.name.0.as_ref() == name.as_bytes())
        })
        .map(|entry| {
            [
                String::from_utf8_lossy(&entry.decl.name.0).into_owned(),
                shown(entry.value.as_deref().unwrap_or_default()),
                shown(entry.master.as_deref().unwrap_or_default()),
            ]
        })
        .collect();
    let cells: Vec<[&str; 3]> = rows
        .iter()
        .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
        .collect();
    req.info_ini_table(&cells);
}

/// `register_php_date_symbols`: the formats, then the three deprecated
/// `SUNFUNCS_RET_*`.
///
/// ```php
/// var_dump(DATE_ATOM, DATE_COOKIE, DATE_RSS, DATE_W3C, DATE_ISO8601_EXPANDED, DATE_RFC3339_EXTENDED);
/// var_dump(DATE_RFC7231, SUNFUNCS_RET_DOUBLE);
/// ```
/// ```expect
/// string(13) "Y-m-d\TH:i:sP"
/// string(16) "l, d-M-Y H:i:s T"
/// string(16) "D, d M Y H:i:s O"
/// string(13) "Y-m-d\TH:i:sP"
/// string(13) "X-m-d\TH:i:sP"
/// string(15) "Y-m-d\TH:i:s.vP"
///
/// Deprecated: Constant DATE_RFC7231 is deprecated since 8.5, as this format ignores the associated timezone and always uses GMT in %s on line 3
///
/// Deprecated: Constant SUNFUNCS_RET_DOUBLE is deprecated since 8.4, as date_sunrise() and date_sunset() were deprecated in 8.1 in %s on line 3
/// string(21) "D, d M Y H:i:s \G\M\T"
/// int(2)
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const DATE_ATOM: &str = "Y-m-d\\TH:i:sP";
    pub(crate) const DATE_COOKIE: &str = "l, d-M-Y H:i:s T";
    pub(crate) const DATE_ISO8601: &str = "Y-m-d\\TH:i:sO";
    pub(crate) const DATE_ISO8601_EXPANDED: &str = "X-m-d\\TH:i:sP";
    pub(crate) const DATE_RFC822: &str = "D, d M y H:i:s O";
    pub(crate) const DATE_RFC850: &str = "l, d-M-y H:i:s T";
    pub(crate) const DATE_RFC1036: &str = "D, d M y H:i:s O";
    pub(crate) const DATE_RFC1123: &str = "D, d M Y H:i:s O";
    #[php(deprecated(
        since = "8.5",
        message = "as this format ignores the associated timezone and always uses GMT"
    ))]
    pub(crate) const DATE_RFC7231: &str = "D, d M Y H:i:s \\G\\M\\T";
    pub(crate) const DATE_RFC2822: &str = "D, d M Y H:i:s O";
    pub(crate) const DATE_RFC3339: &str = "Y-m-d\\TH:i:sP";
    pub(crate) const DATE_RFC3339_EXTENDED: &str = "Y-m-d\\TH:i:s.vP";
    pub(crate) const DATE_RSS: &str = DATE_RFC1123;
    pub(crate) const DATE_W3C: &str = DATE_RFC3339;
    #[php(deprecated(
        since = "8.4",
        message = "as date_sunrise() and date_sunset() were deprecated in 8.1"
    ))]
    pub(crate) const SUNFUNCS_RET_TIMESTAMP: i64 = 0;
    #[php(deprecated(
        since = "8.4",
        message = "as date_sunrise() and date_sunset() were deprecated in 8.1"
    ))]
    pub(crate) const SUNFUNCS_RET_STRING: i64 = 1;
    #[php(deprecated(
        since = "8.4",
        message = "as date_sunrise() and date_sunset() were deprecated in 8.1"
    ))]
    pub(crate) const SUNFUNCS_RET_DOUBLE: i64 = 2;
}
