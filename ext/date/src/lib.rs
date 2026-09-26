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

mod args;
mod classes;
mod datetime;
mod ffi;
mod format;
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
use php_ext::decl::{
    Attribute, AttributeArgument, ConstDecl, ConstExpr, FunctionDecl, IniDecl, Name, Parameter,
    Type,
};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Value;

use crate::args::{class, d, function, nullable, p, str_default, union};
use crate::ffi::{Errors, Tzdb};
pub use crate::ffi::{Offset, Zone};

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
    classes::register(ExtensionBuilder::default())
        .ini(ini())
        .constants(constants())
        .functions(functions())
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
            default: default.as_bytes().into(),
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
    // master, `no value` for an empty one.
    let shown = |value: &[u8]| {
        if value.is_empty() {
            "no value".to_owned()
        } else {
            String::from_utf8_lossy(value).into_owned()
        }
    };
    let entries = req.ini_entries();
    let rows: Vec<[String; 3]> = INI
        .iter()
        .filter_map(|(name, _)| {
            entries
                .iter()
                .find(|entry| entry.decl.name.0.as_ref() == name.as_bytes())
        })
        .map(|entry| {
            let master = entry.original.as_deref().unwrap_or(&entry.value);
            [
                String::from_utf8_lossy(&entry.decl.name.0).into_owned(),
                shown(&entry.value),
                shown(master),
            ]
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

/// `#[\Deprecated(since: ..., message: ...)]`.
fn deprecated(since: &str, message: &str) -> Vec<Attribute> {
    vec![Attribute {
        name: Name(b"Deprecated".as_slice().into()),
        arguments: vec![
            AttributeArgument {
                name: Some(Name(b"since".as_slice().into())),
                value: ConstExpr::Str(since.as_bytes().into()),
            },
            AttributeArgument {
                name: Some(Name(b"message".as_slice().into())),
                value: ConstExpr::Str(message.as_bytes().into()),
            },
        ],
    }]
}

/// The `DATE_*` formats, in `register_php_date_symbols`' order.
const FORMATS: [(&str, &str); 14] = [
    ("ATOM", "Y-m-d\\TH:i:sP"),
    ("COOKIE", "l, d-M-Y H:i:s T"),
    ("ISO8601", "Y-m-d\\TH:i:sO"),
    ("ISO8601_EXPANDED", "X-m-d\\TH:i:sP"),
    ("RFC822", "D, d M y H:i:s O"),
    ("RFC850", "l, d-M-y H:i:s T"),
    ("RFC1036", "D, d M y H:i:s O"),
    ("RFC1123", "D, d M Y H:i:s O"),
    ("RFC7231", "D, d M Y H:i:s \\G\\M\\T"),
    ("RFC2822", "D, d M Y H:i:s O"),
    ("RFC3339", "Y-m-d\\TH:i:sP"),
    ("RFC3339_EXTENDED", "Y-m-d\\TH:i:s.vP"),
    ("RSS", "D, d M Y H:i:s O"),
    ("W3C", "Y-m-d\\TH:i:sP"),
];

/// `RFC7231`'s `#[\Deprecated]`, on the constant and the class constant.
fn rfc7231_deprecation() -> Vec<Attribute> {
    deprecated(
        "8.5",
        "as this format ignores the associated timezone and always uses GMT",
    )
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
fn constants() -> Vec<ConstDecl> {
    let mut out: Vec<ConstDecl> = FORMATS
        .iter()
        .map(|(name, value)| ConstDecl {
            name: Name(format!("DATE_{name}").into_bytes().into()),
            value: ConstExpr::Str(value.as_bytes().into()),
            attributes: if *name == "RFC7231" {
                rfc7231_deprecation()
            } else {
                Vec::new()
            },
        })
        .collect();
    for (name, value) in [("TIMESTAMP", 0), ("STRING", 1), ("DOUBLE", 2)] {
        out.push(ConstDecl {
            name: Name(format!("SUNFUNCS_RET_{name}").into_bytes().into()),
            value: ConstExpr::long(value),
            attributes: deprecated(
                "8.4",
                "as date_sunrise() and date_sunset() were deprecated in 8.1",
            ),
        });
    }
    out
}

/// `?int $timestamp = null` of the `date`-shaped natives.
fn null_ts() -> Parameter {
    null_int("timestamp")
}

/// `?int $name = null`.
fn null_int(name: &str) -> Parameter {
    d(name, nullable(Type::Int), ConstExpr::Null)
}

/// `?DateTimeZone $timezone = null`.
fn null_tz() -> Parameter {
    d("timezone", nullable(class("DateTimeZone")), ConstExpr::Null)
}

/// `$object` of a class, the first parameter of the procedural twins.
fn obj(class_name: &str) -> Parameter {
    p("object", class(class_name))
}

/// `date_sunrise` and `date_sunset`, deprecated since 8.1.
fn sunrise_decl(name: &str) -> FunctionDecl {
    let null_float = |name: &str| d(name, nullable(Type::Float), ConstExpr::Null);
    FunctionDecl {
        attributes: deprecated("8.1", "use date_sun_info() instead"),
        ..function(
            name,
            vec![
                p("timestamp", Type::Int),
                d(
                    "returnFormat",
                    Type::Int,
                    ConstExpr::Constant(Name(b"SUNFUNCS_RET_STRING".as_slice().into())),
                ),
                null_float("latitude"),
                null_float("longitude"),
                null_float("zenith"),
                null_float("utcOffset"),
            ],
            Some(union(vec![
                Type::String,
                Type::Int,
                Type::Float,
                Type::False,
            ])),
        )
    }
}

/// The functions, in `php_date.stub.php`'s order.
#[allow(clippy::too_many_lines)] // the stub's function table, one row per native
fn functions() -> Vec<(FunctionDecl, Entry)> {
    let int_or_false = || union(vec![Type::Int, Type::False]);
    let string_or_false = || union(vec![Type::String, Type::False]);
    let mktime_params = || {
        vec![
            p("hour", Type::Int),
            null_int("minute"),
            null_int("second"),
            null_int("month"),
            null_int("day"),
            null_int("year"),
        ]
    };
    let strftime_decl = |name: &str| FunctionDecl {
        attributes: deprecated("8.1", "use IntlDateFormatter::format() instead"),
        ..function(
            name,
            vec![p("format", Type::String), null_ts()],
            Some(string_or_false()),
        )
    };
    let format_ts = |name: &str, returns: Type| {
        function(
            name,
            vec![p("format", Type::String), null_ts()],
            Some(returns),
        )
    };
    vec![
        (
            function(
                "strtotime",
                vec![p("datetime", Type::String), null_int("baseTimestamp")],
                Some(int_or_false()),
            ),
            parse::strtotime as Entry,
        ),
        (format_ts("date", Type::String), format::date),
        (format_ts("idate", int_or_false()), format::idate),
        (format_ts("gmdate", Type::String), format::gmdate),
        (
            function("mktime", mktime_params(), Some(int_or_false())),
            format::mktime,
        ),
        (
            function("gmmktime", mktime_params(), Some(int_or_false())),
            format::gmmktime,
        ),
        (
            function(
                "checkdate",
                vec![
                    p("month", Type::Int),
                    p("day", Type::Int),
                    p("year", Type::Int),
                ],
                Some(Type::Bool),
            ),
            format::checkdate,
        ),
        (strftime_decl("strftime"), format::strftime),
        (strftime_decl("gmstrftime"), format::gmstrftime),
        (function("time", vec![], Some(Type::Int)), format::time),
        (
            function(
                "localtime",
                vec![
                    null_ts(),
                    d("associative", Type::Bool, ConstExpr::Bool(false)),
                ],
                Some(Type::Array),
            ),
            format::localtime,
        ),
        (
            function("getdate", vec![null_ts()], Some(Type::Array)),
            format::getdate,
        ),
        (
            function(
                "date_create",
                vec![d("datetime", Type::String, str_default("now")), null_tz()],
                Some(union(vec![class("DateTime"), Type::False])),
            ),
            datetime::date_create,
        ),
        (
            function(
                "date_create_immutable",
                vec![d("datetime", Type::String, str_default("now")), null_tz()],
                Some(union(vec![class("DateTimeImmutable"), Type::False])),
            ),
            datetime::date_create_immutable,
        ),
        (
            function(
                "date_create_from_format",
                vec![
                    p("format", Type::String),
                    p("datetime", Type::String),
                    null_tz(),
                ],
                Some(union(vec![class("DateTime"), Type::False])),
            ),
            datetime::date_create_from_format,
        ),
        (
            function(
                "date_create_immutable_from_format",
                vec![
                    p("format", Type::String),
                    p("datetime", Type::String),
                    null_tz(),
                ],
                Some(union(vec![class("DateTimeImmutable"), Type::False])),
            ),
            datetime::date_create_immutable_from_format,
        ),
        (
            function(
                "date_parse",
                vec![p("datetime", Type::String)],
                Some(Type::Array),
            ),
            parse::date_parse,
        ),
        (
            function(
                "date_parse_from_format",
                vec![p("format", Type::String), p("datetime", Type::String)],
                Some(Type::Array),
            ),
            parse::date_parse_from_format,
        ),
        (
            function(
                "date_get_last_errors",
                vec![],
                Some(union(vec![Type::Array, Type::False])),
            ),
            datetime::date_get_last_errors,
        ),
        (
            function(
                "date_format",
                vec![obj("DateTimeInterface"), p("format", Type::String)],
                Some(Type::String),
            ),
            datetime::date_format_fn,
        ),
        (
            function(
                "date_modify",
                vec![obj("DateTime"), p("modifier", Type::String)],
                Some(union(vec![class("DateTime"), Type::False])),
            ),
            datetime::date_modify,
        ),
        (
            function(
                "date_add",
                vec![obj("DateTime"), p("interval", class("DateInterval"))],
                Some(class("DateTime")),
            ),
            datetime::date_add,
        ),
        (
            function(
                "date_sub",
                vec![obj("DateTime"), p("interval", class("DateInterval"))],
                Some(class("DateTime")),
            ),
            datetime::date_sub,
        ),
        (
            function(
                "date_timezone_get",
                vec![obj("DateTimeInterface")],
                Some(union(vec![class("DateTimeZone"), Type::False])),
            ),
            datetime::date_timezone_get,
        ),
        (
            function(
                "date_timezone_set",
                vec![obj("DateTime"), p("timezone", class("DateTimeZone"))],
                Some(class("DateTime")),
            ),
            datetime::date_timezone_set,
        ),
        (
            function(
                "date_offset_get",
                vec![obj("DateTimeInterface")],
                Some(Type::Int),
            ),
            datetime::date_offset_get,
        ),
        (
            function(
                "date_diff",
                vec![
                    p("baseObject", class("DateTimeInterface")),
                    p("targetObject", class("DateTimeInterface")),
                    d("absolute", Type::Bool, ConstExpr::Bool(false)),
                ],
                Some(class("DateInterval")),
            ),
            datetime::date_diff,
        ),
        (
            function(
                "date_time_set",
                vec![
                    obj("DateTime"),
                    p("hour", Type::Int),
                    p("minute", Type::Int),
                    d("second", Type::Int, ConstExpr::long(0)),
                    d("microsecond", Type::Int, ConstExpr::long(0)),
                ],
                Some(class("DateTime")),
            ),
            datetime::date_time_set,
        ),
        (
            function(
                "date_date_set",
                vec![
                    obj("DateTime"),
                    p("year", Type::Int),
                    p("month", Type::Int),
                    p("day", Type::Int),
                ],
                Some(class("DateTime")),
            ),
            datetime::date_date_set,
        ),
        (
            function(
                "date_isodate_set",
                vec![
                    obj("DateTime"),
                    p("year", Type::Int),
                    p("week", Type::Int),
                    d("dayOfWeek", Type::Int, ConstExpr::long(1)),
                ],
                Some(class("DateTime")),
            ),
            datetime::date_isodate_set,
        ),
        (
            function(
                "date_timestamp_set",
                vec![obj("DateTime"), p("timestamp", Type::Int)],
                Some(class("DateTime")),
            ),
            datetime::date_timestamp_set,
        ),
        (
            function(
                "date_timestamp_get",
                vec![obj("DateTimeInterface")],
                Some(Type::Int),
            ),
            datetime::date_timestamp_get,
        ),
        (
            function(
                "timezone_open",
                vec![p("timezone", Type::String)],
                Some(union(vec![class("DateTimeZone"), Type::False])),
            ),
            timezone::timezone_open,
        ),
        (
            function(
                "timezone_name_get",
                vec![obj("DateTimeZone")],
                Some(Type::String),
            ),
            timezone::timezone_name_get,
        ),
        (
            function(
                "timezone_name_from_abbr",
                vec![
                    p("abbr", Type::String),
                    d("utcOffset", Type::Int, ConstExpr::long(-1)),
                    d("isDST", Type::Int, ConstExpr::long(-1)),
                ],
                Some(union(vec![Type::String, Type::False])),
            ),
            tzdb::timezone_name_from_abbr,
        ),
        (
            function(
                "timezone_offset_get",
                vec![
                    obj("DateTimeZone"),
                    p("datetime", class("DateTimeInterface")),
                ],
                Some(Type::Int),
            ),
            timezone::timezone_offset_get,
        ),
        (
            function(
                "timezone_transitions_get",
                vec![
                    obj("DateTimeZone"),
                    d(
                        "timestampBegin",
                        Type::Int,
                        ConstExpr::Constant(Name(b"PHP_INT_MIN".as_slice().into())),
                    ),
                    d("timestampEnd", Type::Int, ConstExpr::long(2_147_483_647)),
                ],
                Some(union(vec![Type::Array, Type::False])),
            ),
            timezone::timezone_transitions_get,
        ),
        (
            function(
                "timezone_location_get",
                vec![obj("DateTimeZone")],
                Some(union(vec![Type::Array, Type::False])),
            ),
            timezone::timezone_location_get,
        ),
        (
            function(
                "timezone_identifiers_list",
                vec![
                    d(
                        "timezoneGroup",
                        Type::Int,
                        ConstExpr::ClassConstant {
                            class: Name(b"DateTimeZone".as_slice().into()),
                            name: Name(b"ALL".as_slice().into()),
                        },
                    ),
                    d("countryCode", nullable(Type::String), ConstExpr::Null),
                ],
                Some(Type::Array),
            ),
            tzdb::timezone_identifiers_list,
        ),
        (
            function("timezone_abbreviations_list", vec![], Some(Type::Array)),
            tzdb::timezone_abbreviations_list,
        ),
        (
            function("timezone_version_get", vec![], Some(Type::String)),
            tzdb::timezone_version_get,
        ),
        (
            function(
                "date_interval_create_from_date_string",
                vec![p("datetime", Type::String)],
                Some(union(vec![class("DateInterval"), Type::False])),
            ),
            interval::date_interval_create_from_date_string,
        ),
        (
            function(
                "date_interval_format",
                vec![obj("DateInterval"), p("format", Type::String)],
                Some(Type::String),
            ),
            interval::date_interval_format,
        ),
        (
            function(
                "date_default_timezone_set",
                vec![p("timezoneId", Type::String)],
                Some(Type::Bool),
            ),
            zone::date_default_timezone_set,
        ),
        (
            function("date_default_timezone_get", vec![], Some(Type::String)),
            zone::date_default_timezone_get,
        ),
        (sunrise_decl("date_sunrise"), sun::date_sunrise),
        (sunrise_decl("date_sunset"), sun::date_sunset),
        (
            function(
                "date_sun_info",
                vec![
                    p("timestamp", Type::Int),
                    p("latitude", Type::Float),
                    p("longitude", Type::Float),
                ],
                Some(Type::Array),
            ),
            sun::date_sun_info,
        ),
    ]
}
