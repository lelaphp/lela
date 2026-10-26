//! The default timezone and the request's zone cache: `guess_timezone`,
//! `php_date_parse_tzfile` over `DATEG(tzcache)`, `get_timezone_info`,
//! and the two natives that set and read the default.

use std::sync::Arc;

use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, docref, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString};

use crate::State;
use crate::ffi::{Tzdb, Zone};
use crate::lent::lent;
use crate::throwables::Throwables;

/// `php_date_parse_tzfile(name, tzdb)`: the zone out of the request's
/// cache, parsed from the bundled database onto the request's heap the
/// first time it is asked for; none for a name the database has not.
/// The `Err` is the memory limit.
pub(super) fn parse_tzfile(req: &mut Request, name: &[u8]) -> Result<Option<Arc<Zone>>, Thrown> {
    lent(req, |l| l.zone(name))
}

/// `guess_timezone(tzdb)`: what `date_default_timezone_set` set, else
/// `date.timezone` where it is not empty, else `UTC`. The entry names a
/// zone, since [`on_update_timezone`] refuses any other value at startup
/// and at runtime alike.
pub(super) fn guess_timezone(req: &mut Request) -> Vec<u8> {
    if let Some(tz) = &req.state::<State>().timezone
        && !tz.is_empty()
    {
        return tz.clone();
    }
    if let Some(ini) = req.ini().get(b"date.timezone")
        && !ini.is_empty()
    {
        return ini.to_vec();
    }
    b"UTC".to_vec()
}

/// `OnUpdate_date_timezone`, `date.timezone`'s `on_modify`: a value
/// `timelib_timezone_id_is_valid` does not find is refused with php's
/// warning naming the zone kept, the entry's value before, `UTC` at
/// startup; `ini_set()` answers `false` and the entry keeps its value.
///
/// ```php
/// var_dump(ini_set('date.timezone', 'Europe/London'), ini_set('date.timezone', 'foo'), ini_get('date.timezone'), date_default_timezone_get());
/// ```
/// ```expect
/// Warning: ini_set(): Invalid date.timezone value 'foo', using 'Europe/London' instead in %s on line 2
/// string(3) "UTC"
/// bool(false)
/// string(13) "Europe/London"
/// string(13) "Europe/London"
/// ```
#[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature, which may throw
pub(super) fn on_update_timezone(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    if Tzdb::builtin().is_valid(value) {
        return Ok(true);
    }
    let kept = req
        .ini()
        .get(b"date.timezone")
        .map_or_else(|| b"UTC".to_vec(), <[u8]>::to_vec);
    let message = format!(
        "Invalid date.timezone value '{}', using '{}' instead",
        String::from_utf8_lossy(php_ext::coerce::c_str(value)),
        String::from_utf8_lossy(&kept)
    );
    docref(req, Level::Warning, &message);
    Ok(false)
}

/// `get_timezone_info()`: the default zone, or the `DateError` php
/// throws when the database cannot give it.
pub(super) fn default_zone(req: &mut Request) -> Result<Arc<Zone>, Thrown> {
    let name = guess_timezone(req);
    match parse_tzfile(req, &name)? {
        Some(zone) => Ok(zone),
        None => Err(corrupt(req)),
    }
}

/// The `DateError` `get_timezone_info()` throws where the database
/// cannot give the default zone.
pub(super) fn corrupt(req: &mut Request) -> Thrown {
    let ce = match Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    throw_error(
        req,
        ce.date_error,
        "Timezone database is corrupt. Please file a bug report as this should never happen",
    )
}

/// `date_default_timezone_set(string $timezoneId): bool`: the notice and
/// `false` for an identifier the database has not, as
/// `timelib_timezone_id_is_valid` reads it, up to the first NUL.
///
/// ```php
/// var_dump(date_default_timezone_set('Europe/Amsterdam'), date_default_timezone_get());
/// var_dump(date_default_timezone_set('Mars/Olympus'), date_default_timezone_get());
/// var_dump(date_default_timezone_set('utc'), date_default_timezone_get());
/// ```
/// ```expect
/// bool(true)
/// string(16) "Europe/Amsterdam"
///
/// Notice: date_default_timezone_set(): Timezone ID 'Mars/Olympus' is invalid in %s on line 3
/// bool(false)
/// string(16) "Europe/Amsterdam"
/// bool(true)
/// string(3) "utc"
/// ```
#[php_function]
fn date_default_timezone_set(req: &mut Request, timezoneId: PhpString) -> bool {
    let zone = timezoneId.as_bytes().to_vec();
    if !Tzdb::builtin().is_valid(&zone) {
        let shown = php_ext::coerce::c_str(&zone);
        let message = format!(
            "Timezone ID '{}' is invalid",
            String::from_utf8_lossy(shown)
        );
        docref(req, Level::Notice, &message);
        return false;
    }
    req.state::<State>().timezone = Some(zone);
    true
}

/// `date_default_timezone_get(): string`: the name of the zone the
/// default parses to, which is the database's spelling of it.
///
/// ```php
/// var_dump(date_default_timezone_get());
/// ini_set('date.timezone', 'America/New_York');
/// var_dump(date_default_timezone_get());
/// date_default_timezone_set('Asia/Tokyo');
/// var_dump(date_default_timezone_get());
/// ```
/// ```expect
/// string(3) "UTC"
/// string(16) "America/New_York"
/// string(10) "Asia/Tokyo"
/// ```
#[php_function]
fn date_default_timezone_get(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let zone = default_zone(req)?;
    PhpString::new(req, zone.name())
}
