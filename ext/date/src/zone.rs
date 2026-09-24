//! The default timezone and the request's zone cache: `guess_timezone`,
//! `php_date_parse_tzfile` over `DATEG(tzcache)`, `get_timezone_info`,
//! and the two natives that set and read the default.

use std::sync::Arc;

use php_ext::raising::{Level, Thrown, docref, throw};
use php_ext::request::Request;
use php_ext::values::Value;

use crate::State;
use crate::args::{count_between, string, string_value};
use crate::ffi::{Tzdb, Zone};

/// `php_date_parse_tzfile(name, tzdb)`: the zone out of the request's
/// cache, parsed from the bundled database the first time it is asked
/// for; none for a name the database has not.
pub(super) fn parse_tzfile(req: &mut Request, name: &[u8]) -> Option<Arc<Zone>> {
    let state = req.state::<State>();
    if let Some(zone) = state.zones.get(name) {
        return Some(Arc::clone(zone));
    }
    let zone = Arc::new(Zone::parse(name)?);
    state.zones.insert(name.to_vec(), Arc::clone(&zone));
    Some(zone)
}

/// `guess_timezone(tzdb)`: what `date_default_timezone_set` set, else
/// `date.timezone` where it names a zone, else `UTC`. php checks the ini
/// value when it is set (`OnUpdate_date_timezone`) and keeps the one
/// before; here the value is checked where it is read.
pub(super) fn guess_timezone(req: &mut Request) -> Vec<u8> {
    if let Some(tz) = &req.state::<State>().timezone
        && !tz.is_empty()
    {
        return tz.clone();
    }
    if let Some(ini) = req.ini_get(b"date.timezone")
        && !ini.is_empty()
        && Tzdb::builtin().is_valid(ini)
    {
        return ini.to_vec();
    }
    b"UTC".to_vec()
}

/// `get_timezone_info()`: the default zone, or the `DateError` php
/// throws when the database cannot give it.
pub(super) fn default_zone(req: &mut Request) -> Result<Arc<Zone>, Thrown> {
    let name = guess_timezone(req);
    match parse_tzfile(req, &name) {
        Some(zone) => Ok(zone),
        None => Err(throw(
            req,
            "DateError",
            "Timezone database is corrupt. Please file a bug report as this should never happen",
        )),
    }
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
pub(crate) fn date_default_timezone_set(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 1)?;
    let zone = string(req, 1)?.as_bytes().to_vec();
    if !Tzdb::builtin().is_valid(&zone) {
        let shown = zone.split(|b| *b == 0).next().unwrap_or_default();
        let message = format!(
            "Timezone ID '{}' is invalid",
            String::from_utf8_lossy(shown)
        );
        docref(req, Level::Notice, &message);
        return Ok(Value::bool(false));
    }
    req.state::<State>().timezone = Some(zone);
    Ok(Value::bool(true))
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
pub(crate) fn date_default_timezone_get(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    let zone = default_zone(req)?;
    string_value(req, zone.name())
}
