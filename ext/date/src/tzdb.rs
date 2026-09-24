//! What the bundled database lists: `timezone_identifiers_list`,
//! `timezone_abbreviations_list`, `timezone_name_from_abbr` and
//! `timezone_version_get`.

use std::collections::HashMap;

use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value};

use crate::args::{count_between, nullable_string, optional_long, set_str, string, string_value};
use crate::ffi::{self, Tzdb};

/// `PHP_DATE_TIMEZONE_GROUP_*` and `PHP_DATE_TIMEZONE_PER_COUNTRY`, the
/// values of `DateTimeZone`'s constants.
pub(super) const GROUPS: [(&str, i64); 14] = [
    ("AFRICA", 0x0001),
    ("AMERICA", 0x0002),
    ("ANTARCTICA", 0x0004),
    ("ARCTIC", 0x0008),
    ("ASIA", 0x0010),
    ("ATLANTIC", 0x0020),
    ("AUSTRALIA", 0x0040),
    ("EUROPE", 0x0080),
    ("INDIAN", 0x0100),
    ("PACIFIC", 0x0200),
    ("UTC", 0x0400),
    ("ALL", 0x07FF),
    ("ALL_WITH_BC", 0x0FFF),
    ("PER_COUNTRY", 0x1000),
];
const ALL: i64 = 0x07FF;
const ALL_WITH_BC: i64 = 0x0FFF;
const PER_COUNTRY: i64 = 0x1000;

/// `check_id_allowed(id, what)`: the area prefixes, case-insensitively.
fn allowed(id: &[u8], what: i64) -> bool {
    const PREFIXES: [&[u8]; 11] = [
        b"Africa/",
        b"America/",
        b"Antarctica/",
        b"Arctic/",
        b"Asia/",
        b"Atlantic/",
        b"Australia/",
        b"Europe/",
        b"Indian/",
        b"Pacific/",
        b"UTC",
    ];
    PREFIXES.iter().enumerate().any(|(bit, prefix)| {
        what & (1 << bit) != 0
            && id.len() >= prefix.len()
            && id[..prefix.len()].eq_ignore_ascii_case(prefix)
    })
}

/// `timezone_identifiers_list(int $timezoneGroup = DateTimeZone::ALL,
/// ?string $countryCode = null): array`, `DateTimeZone::listIdentifiers`.
///
/// ```php
/// $all = timezone_identifiers_list();
/// var_dump(count($all), $all[0], $all[count($all) - 1], count(timezone_identifiers_list(DateTimeZone::ALL_WITH_BC)) > count($all));
/// var_dump(timezone_identifiers_list(DateTimeZone::PER_COUNTRY, 'NL'), timezone_identifiers_list(DateTimeZone::ANTARCTICA | DateTimeZone::UTC));
/// try { timezone_identifiers_list(DateTimeZone::PER_COUNTRY); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// echo new ReflectionFunction('timezone_identifiers_list');
/// ```
/// ```expect
/// int(419)
/// string(14) "Africa/Abidjan"
/// string(3) "UTC"
/// bool(true)
/// array(1) {
///   [0]=>
///   string(16) "Europe/Amsterdam"
/// }
/// array(12) {
///   [0]=>
///   string(16) "Antarctica/Casey"
///   [1]=>
///   string(16) "Antarctica/Davis"
///   [2]=>
///   string(25) "Antarctica/DumontDUrville"
///   [3]=>
///   string(20) "Antarctica/Macquarie"
///   [4]=>
///   string(17) "Antarctica/Mawson"
///   [5]=>
///   string(18) "Antarctica/McMurdo"
///   [6]=>
///   string(17) "Antarctica/Palmer"
///   [7]=>
///   string(18) "Antarctica/Rothera"
///   [8]=>
///   string(16) "Antarctica/Syowa"
///   [9]=>
///   string(16) "Antarctica/Troll"
///   [10]=>
///   string(17) "Antarctica/Vostok"
///   [11]=>
///   string(3) "UTC"
/// }
/// timezone_identifiers_list(): Argument #2 ($countryCode) must be a two-letter ISO 3166-1 compatible country code when argument #1 ($timezoneGroup) is DateTimeZone::PER_COUNTRY
/// Function [ <internal:date> function timezone_identifiers_list ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <optional> int $timezoneGroup = DateTimeZone::ALL ]
///     Parameter #1 [ <optional> ?string $countryCode = null ]
///   }
///   - Return [ array ]
/// }
/// ```
pub(crate) fn timezone_identifiers_list(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 2)?;
    let what = optional_long(req, 1, ALL)?;
    let option = nullable_string(req, 2)?.map(|s| s.as_bytes().to_vec());
    identifiers(req, what, option)
}

/// The body both entries share.
pub(super) fn identifiers(
    req: &mut Request,
    what: i64,
    option: Option<Vec<u8>>,
) -> Result<Value, Thrown> {
    let code = option.unwrap_or_default();
    if what == PER_COUNTRY && code.len() != 2 {
        return Err(arg_value_error(
            req,
            2,
            "be a two-letter ISO 3166-1 compatible country code when argument #1 ($timezoneGroup) is DateTimeZone::PER_COUNTRY",
        ));
    }
    let db = Tzdb::builtin();
    let mut array = PhpArray::new(req)?;
    for entry in db.identifiers() {
        let keep = if what == PER_COUNTRY {
            db.data_byte(entry.pos + 5) == code[0] && db.data_byte(entry.pos + 6) == code[1]
        } else {
            what == ALL_WITH_BC || (allowed(entry.id, what) && db.data_byte(entry.pos + 4) == 1)
        };
        if keep {
            let id = string_value(req, entry.id)?;
            array.append(req, id)?;
        }
    }
    Ok(Value::array(array))
}

/// `timezone_abbreviations_list(): array`,
/// `DateTimeZone::listAbbreviations`: every row of timelib's table under
/// its abbreviation, in the table's order.
///
/// ```php
/// $list = timezone_abbreviations_list();
/// var_dump(count($list) > 100, $list["cest"][0], $list["utc"]);
/// foreach ($list as $k => $v) { var_dump($k); break; }
/// ```
/// ```expect
/// bool(true)
/// array(3) {
///   ["dst"]=>
///   bool(true)
///   ["offset"]=>
///   int(7200)
///   ["timezone_id"]=>
///   string(13) "Europe/Berlin"
/// }
/// array(5) {
///   [0]=>
///   array(3) {
///     ["dst"]=>
///     bool(false)
///     ["offset"]=>
///     int(0)
///     ["timezone_id"]=>
///     string(13) "Etc/Universal"
///   }
///   [1]=>
///   array(3) {
///     ["dst"]=>
///     bool(false)
///     ["offset"]=>
///     int(0)
///     ["timezone_id"]=>
///     string(7) "Etc/UTC"
///   }
///   [2]=>
///   array(3) {
///     ["dst"]=>
///     bool(false)
///     ["offset"]=>
///     int(0)
///     ["timezone_id"]=>
///     string(8) "Etc/Zulu"
///   }
///   [3]=>
///   array(3) {
///     ["dst"]=>
///     bool(false)
///     ["offset"]=>
///     int(0)
///     ["timezone_id"]=>
///     string(3) "UTC"
///   }
///   [4]=>
///   array(3) {
///     ["dst"]=>
///     bool(false)
///     ["offset"]=>
///     int(0)
///     ["timezone_id"]=>
///     string(3) "UTC"
///   }
/// }
/// string(4) "acdt"
/// ```
pub(crate) fn timezone_abbreviations_list(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    abbreviations(req)
}

/// The body both entries share.
pub(super) fn abbreviations(req: &mut Request) -> Result<Value, Thrown> {
    // The rows grouped under their abbreviation in the order each first
    // appears, which is the order php's `zend_hash_str_find` then
    // `add_assoc_zval` leaves the keys in.
    let mut groups: Vec<(&[u8], Vec<ffi::Abbreviation>)> = Vec::new();
    let mut seen: HashMap<&[u8], usize> = HashMap::new();
    for row in ffi::abbreviations() {
        if let Some(at) = seen.get(row.name) {
            groups[*at].1.push(row);
        } else {
            seen.insert(row.name, groups.len());
            groups.push((row.name, vec![row]));
        }
    }
    let mut array = PhpArray::new(req)?;
    for (name, rows) in groups {
        let mut list = PhpArray::new(req)?;
        for row in rows {
            let mut element = PhpArray::new(req)?;
            set_str(req, &mut element, b"dst", Value::bool(row.is_dst))?;
            #[allow(clippy::cast_possible_truncation)]
            let offset = row.gmtoffset as i64;
            set_str(req, &mut element, b"offset", Value::int(offset))?;
            let id = match row.full_tz_name {
                Some(id) => string_value(req, id)?,
                None => Value::null(),
            };
            set_str(req, &mut element, b"timezone_id", id)?;
            list.append(req, Value::array(element))?;
        }
        let key = Key::Str(PhpString::new(req, name)?);
        array.set(req, key, Value::array(list))?;
    }
    Ok(Value::array(array))
}

/// `timezone_name_from_abbr(string $abbr, int $utcOffset = -1, int
/// $isDST = -1): string|false`.
///
/// ```php
/// var_dump(timezone_name_from_abbr('CET'), timezone_name_from_abbr('', 3600, 0), timezone_name_from_abbr('', -18000, 1), timezone_name_from_abbr('XYZ'), timezone_name_from_abbr('', 1234, 0));
/// ```
/// ```expect
/// string(13) "Europe/Berlin"
/// string(12) "Europe/Paris"
/// string(15) "America/Chicago"
/// bool(false)
/// bool(false)
/// ```
pub(crate) fn timezone_name_from_abbr(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 1, 3)?;
    let abbr = string(req, 1)?.as_bytes().to_vec();
    let offset = optional_long(req, 2, -1)?;
    let dst = optional_long(req, 3, -1)?;
    #[allow(clippy::cast_possible_truncation)]
    let dst = dst as i32;
    match ffi::timezone_id_from_abbr(&abbr, offset, dst) {
        Some(id) => string_value(req, id),
        None => Ok(Value::bool(false)),
    }
}

/// `timezone_version_get(): string`: the bundled database's version.
///
/// ```php
/// var_dump(timezone_version_get());
/// ```
/// ```expect
/// string(6) "2026.3"
/// ```
pub(crate) fn timezone_version_get(req: &mut Request) -> Result<Value, Thrown> {
    count_between(req, 0, 0)?;
    string_value(req, Tzdb::builtin().version())
}
