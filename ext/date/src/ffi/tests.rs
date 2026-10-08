//! The request's zones as the parsers' wrapper reads them (#1899).

use std::sync::Arc;

use super::{Time, Zone, Zones};

/// Whether `zone` is the very zone `cached` holds, not a second parse.
fn same(zone: Option<&Arc<Zone>>, cached: &Arc<Zone>) -> bool {
    zone.is_some_and(|zone| Arc::ptr_eq(zone, cached))
}

/// `php_date_parse_tzfile_wrapper`: `timelib_strtotime`,
/// `timelib_parse_from_format` and `timelib_parse_zone` each read the zone
/// a name names out of the request's cache, so the request parses it once
/// and every time shares it.
#[test]
fn every_parser_reads_a_zone_out_of_the_request_s_cache() {
    let mut zones = Zones::default();
    let (first, _) = Time::strtotime(&mut zones, b"2024-07-01 12:00 Europe/Amsterdam");
    let (again, _) = Time::strtotime(&mut zones, b"2024-07-02 Europe/Amsterdam");
    let (formatted, _) =
        Time::parse_from_format(&mut zones, b"!Y-m-d e", b"2024-01-01 Europe/Amsterdam");
    let mut dummy = Time::new();
    let _read = dummy.parse_zone(&mut zones, b"Europe/Amsterdam");
    let cached = zones.parse(b"Europe/Amsterdam");
    assert!(cached.is_some());
    for time in [&first, &again, &formatted, &dummy] {
        assert!(cached.as_ref().is_some_and(|c| same(time.zone(), c)));
    }
    assert_eq!(zones.len(), 1);
}

/// A name the database has not is answered none each time it is asked
/// for and kept nowhere, as `php_date_parse_tzfile` adds only a zone.
#[test]
fn a_name_the_database_has_not_is_not_kept() {
    let mut zones = Zones::default();
    let (t, errors) = Time::strtotime(&mut zones, b"2024-01-01 Mars/Olympus");
    assert!(t.zone().is_none());
    assert_eq!(errors.errors.len(), 1);
    assert!(zones.parse(b"Mars/Olympus").is_none());
    assert_eq!(zones.len(), 0);
}
