//! The request's zones as the parsers' wrapper reads them (#1899).

use std::sync::Arc;

use super::{Time, Zone, parse_zone};
use crate::State;
use crate::lent::Lent;

#[path = "storage_proof.rs"]
mod storage_proof;

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
    let mut state = State::default();
    let mut lent = Lent::detached(&mut state);
    let first = Time::strtotime(&mut lent, b"2024-07-01 12:00 Europe/Amsterdam");
    let again = Time::strtotime(&mut lent, b"2024-07-02 Europe/Amsterdam");
    let formatted = Time::parse_from_format(&mut lent, b"!Y-m-d e", b"2024-01-01 Europe/Amsterdam");
    let read = parse_zone(&mut lent, b"Europe/Amsterdam");
    let cached = lent.zone(b"Europe/Amsterdam");
    assert!(cached.is_some());
    for time in [&first.time, &again.time, &formatted.time] {
        assert!(cached.as_ref().is_some_and(|c| same(time.zone(), c)));
    }
    assert!(cached.as_ref().is_some_and(|c| same(read.zone.as_ref(), c)));
    assert_eq!(lent.state().zones.len(), 1);
}

/// A name the database has not is answered none each time it is asked
/// for and kept nowhere, as `php_date_parse_tzfile` adds only a zone.
#[test]
fn a_name_the_database_has_not_is_not_kept() {
    let mut state = State::default();
    let mut lent = Lent::detached(&mut state);
    let parsed = Time::strtotime(&mut lent, b"2024-01-01 Mars/Olympus");
    assert!(parsed.time.zone().is_none());
    let errors = parsed.errors.map(|e| e.messages()).unwrap_or_default();
    assert_eq!(errors.errors.len(), 1);
    assert!(lent.zone(b"Mars/Olympus").is_none());
    assert_eq!(lent.state().zones.len(), 0);
}
