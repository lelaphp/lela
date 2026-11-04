//! The `date` extension: `php-src/ext/date` as one crate under `ext/`
//! (ADR-0004). Two dependencies, the facade and timelib's binding; one
//! entry, `extension()`; its state on the value (ADR-0005). The phpt
//! subset is `php-src/ext/date/tests`, through `xtask phpt --subset`.
//!
//! timelib is the library (design §7.2): `timelib_sys` vendors php-src's
//! `ext/date/lib` at the pinned tag with its bundled timezone database,
//! compiles it and declares it, and [`ffi`], the crate's one home of
//! `unsafe`, wraps it. Over it, `php_date.c`'s natives, declared as
//! `php_date.stub.php` declares them.
//!
//! What php keeps in `DATEG` is the request state: the zone
//! `date_default_timezone_set` chose, the zones parsed so far
//! (`DATEG(tzcache)`), and the container of the last parse that had
//! messages (`DATEG(last_errors)`). timelib allocates on the request's
//! heap as php's build has it, under [`lent`]'s lend (ADR-0014), and
//! `rshutdown` frees the cache and the container under one, as php's
//! `RSHUTDOWN` frees them. The database itself is data compiled into the
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
mod interval;
mod lent;
mod obj;
mod parse;
mod period;
mod sun;
mod throwables;
mod timezone;
mod tzdb;
mod zone;

use std::sync::Arc;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::php_const;
use php_ext::decl::{Bytes, IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject};

use crate::classes::{
    DateError, DateException, DateInvalidOperationException, DateInvalidTimeZoneException,
    DateMalformedIntervalStringException, DateMalformedPeriodStringException,
    DateMalformedStringException, DateObjectError, DateRangeError, DateTimeInterface,
};
use crate::datetime::{DateTime, DateTimeImmutable};
use crate::ffi::{ErrorContainer, Tzdb, Zones};
pub use crate::ffi::{Offset, Zone};
use crate::interval::DateInterval;
pub use crate::obj::{AbbrZone, TzKind};
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
    /// was asked for, the parsers' wrapper's among them.
    zones: Zones,
    /// `DATEG(last_errors)`: the container of the last parse that had
    /// messages, on the request's heap as php keeps it.
    last_errors: Option<ErrorContainer>,
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

/// `php_format_date_obj()`, what `php_date.h` declares `PHPAPI` (design
/// §4.10): a `DateTimeInterface` object's time in `date()`'s `format`,
/// none where no constructor set it. `ext/soap`'s `to_xml_datetime_ex`
/// writes a date with it, past a subclass's `format()`.
pub fn php_format_date_obj(req: &mut Request, format: &[u8], object: PhpObject) -> Option<Vec<u8>> {
    let text = datetime::format_obj(req, format, object)?.ok()?;
    let bytes = text.as_bytes().to_vec();
    text.give_up(req);
    Some(bytes)
}

/// What ext/intl reads of a `DateTimeInterface`'s `php_date_obj`
/// (`php_date.h`, which `common_date.cpp` includes): the time's
/// microseconds, and its zone, none where the time is UTC
/// (`!time->is_localtime`).
pub struct DateParts {
    pub us: i64,
    pub zone: Option<ZoneParts>,
}

/// A zone as ext/intl's `timezone_convert_datetimezone` reads it, off a
/// time (`datetime->time`) or a `DateTimeZone` (`tzobj->tzi`): its zone
/// of the database, its offset from UTC in seconds, or its abbreviation,
/// `time->tz_abbr` or `tzi.z.abbr`, read where ext/date keeps it and
/// copied out for the reader, as an offset is.
pub enum ZoneParts {
    Id(Arc<Zone>),
    Offset(i64),
    Abbr(Vec<u8>),
}

/// `php_date_obj_from_obj(obj)->time`, as ext/intl's
/// `intl_datetime_decompose` reads it; none where no constructor set the
/// time (design §4.10).
#[must_use]
pub fn date_obj(req: &Request, object: PhpObject) -> Option<DateParts> {
    let time = object.native::<obj::DateObj>(req)?.time.as_ref()?;
    let zone = if time.is_localtime == 0 {
        None
    } else {
        match time.zone_type {
            ffi::ZONETYPE_ID => time.zone().map(|zone| ZoneParts::Id(Arc::clone(zone))),
            ffi::ZONETYPE_OFFSET => Some(ZoneParts::Offset(i64::from(time.z))),
            ffi::ZONETYPE_ABBR => Some(ZoneParts::Abbr(time.tz_abbr().to_vec())),
            _ => None,
        }
    };
    Some(DateParts { us: time.us, zone })
}

/// `php_timezone_obj_from_obj(obj)`'s `type` and `tzi`, as ext/intl's
/// `timezone_convert_datetimezone` reads them; none where the object is
/// not `initialized` (design §4.10).
#[must_use]
pub fn timezone_obj(req: &Request, object: PhpObject) -> Option<ZoneParts> {
    Some(match object.native::<obj::TzObj>(req)?.tz.as_ref()? {
        TzKind::Id(zone) => ZoneParts::Id(Arc::clone(zone)),
        TzKind::Offset(offset) => ZoneParts::Offset(*offset),
        TzKind::Abbr(zone) => ZoneParts::Abbr(zone.abbr().to_vec()),
    })
}

/// `object_init_ex(php_date_get_timezone_ce())` with its `type` and `tzi`
/// written, as ext/intl's `timezone_convert_to_datetimezone` lays down an
/// offset zone the constructor cannot (design §4.10): one owner, the
/// caller's.
///
/// # Errors
///
/// What making the object threw.
pub fn new_timezone(req: &mut Request, kind: TzKind) -> Result<Owner<PhpObject>, Thrown> {
    obj::new_timezone(req, kind)
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
        .register(php_ext::request::FormatDate(format::format_date))
        .constants(constants::DECLARED)
        .functions(&[
            parse::strtotime::DECLARED,
            format::date::DECLARED,
            format::idate::DECLARED,
            format::gmdate::DECLARED,
            format::mktime::DECLARED,
            format::gmmktime::DECLARED,
            format::checkdate::DECLARED,
            format::strftime::DECLARED,
            format::gmstrftime::DECLARED,
            format::time::DECLARED,
            format::localtime::DECLARED,
            format::getdate::DECLARED,
            datetime::date_create::DECLARED,
            datetime::date_create_immutable::DECLARED,
            datetime::date_create_from_format::DECLARED,
            datetime::date_create_immutable_from_format::DECLARED,
            parse::date_parse::DECLARED,
            parse::date_parse_from_format::DECLARED,
            datetime::date_get_last_errors::DECLARED,
            datetime::date_format::DECLARED,
            datetime::date_modify::DECLARED,
            datetime::date_add::DECLARED,
            datetime::date_sub::DECLARED,
            datetime::date_timezone_get::DECLARED,
            datetime::date_timezone_set::DECLARED,
            datetime::date_offset_get::DECLARED,
            datetime::date_diff::DECLARED,
            datetime::date_time_set::DECLARED,
            datetime::date_date_set::DECLARED,
            datetime::date_isodate_set::DECLARED,
            datetime::date_timestamp_set::DECLARED,
            datetime::date_timestamp_get::DECLARED,
            timezone::timezone_open::DECLARED,
            timezone::timezone_name_get::DECLARED,
            tzdb::timezone_name_from_abbr::DECLARED,
            timezone::timezone_offset_get::DECLARED,
            timezone::timezone_transitions_get::DECLARED,
            timezone::timezone_location_get::DECLARED,
            tzdb::timezone_identifiers_list::DECLARED,
            tzdb::timezone_abbreviations_list::DECLARED,
            tzdb::timezone_version_get::DECLARED,
            interval::date_interval_create_from_date_string::DECLARED,
            interval::date_interval_format::DECLARED,
            zone::date_default_timezone_set::DECLARED,
            zone::date_default_timezone_get::DECLARED,
            sun::date_sunrise::DECLARED,
            sun::date_sunset::DECLARED,
            sun::date_sun_info::DECLARED,
        ])
        .on_modify(b"date.timezone", zone::on_update_timezone)
        .request_state::<State>()
        .engine_state(throwables::Throwables::start)
        .rshutdown(rshutdown)
        .info(date_info)
        .build("date")
}

/// `PHP_RSHUTDOWN_FUNCTION(date)`: `DATEG(tzcache)` destroyed and
/// `DATEG(last_errors)` freed, under a lend, so their blocks go back to
/// the request's heap where php's go back to it. A zone a date or a
/// `DateTimeZone` still holds goes back with the last of them.
fn rshutdown(req: &mut Request) {
    // Freeing allocates nothing, so the limit refuses nothing.
    let _freed = lent::lent(req, |l| {
        let state = l.state();
        state.zones = Zones::default();
        state.last_errors = None;
    });
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
            name: Name(Bytes::borrowed(name.as_bytes())),
            default: Some(Bytes::borrowed(default.as_bytes())),
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
///
/// date
///
/// date/time support => enabled
/// timelib version => 2022.17
/// "Olson" Timezone Database Version => 2026.5
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
    req.extensions_mut().info_table(&[
        &["date/time support", "enabled"],
        &["timelib version", "2022.17"],
        &["\"Olson\" Timezone Database Version", &version],
        &["Timezone Database", "internal"],
        &["Default timezone", &default],
    ]);
    // `DISPLAY_INI_ENTRIES()`: each entry's request value beside its
    // master.
    let shown = |value: &[u8]| String::from_utf8_lossy(value).into_owned();
    let entries = req.ini().entries();
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
    req.ini_mut().info_ini_table(&cells);
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
