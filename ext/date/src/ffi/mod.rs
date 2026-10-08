//! The binding to timelib (design §7.2, `unsafe.homes`' `ext/*/src/ffi/`):
//! the C library `timelib_sys` vendors, compiles and declares, wrapped
//! here, the one home of this crate's `unsafe`. What it hands the rest of
//! the crate is owned values that free themselves:
//!
//! - [`Zone`], a parsed `timelib_tzinfo`, immutable once parsed and so
//!   shared by `Arc` between the request's zone cache, every date that
//!   carries it and every `DateTimeZone` made from it, as php shares one
//!   pointer out of `DATEG(tzcache)`;
//! - [`Time`], an owned `timelib_time` beside the `Arc` of the zone its
//!   `tz_info` points at, so the zone outlives every date that reads it;
//!   its numbers are [`TimeFields`], read and written as plain fields;
//! - [`RelTime`], `timelib_rel_time` by value;
//! - [`Tzdb`], the bundled database, the data compiled into the binary, never written;
//! - [`Zones`], `DATEG(tzcache)`: the zones a request parsed, by the name
//!   asked for, which the parsers' zone wrapper reads as php's
//!   `php_date_parse_tzfile_wrapper` reads the cache.
//!
//! Beside timelib is the one call into the platform's C library,
//! [`strftime`], which reads the calling thread's `LC_TIME`, the
//! request's own once ext/standard's `setlocale()` has installed one
//! (design §5.14, revision 40).
//!
//! Every function timelib may point a date's `tz_info` elsewhere through
//! is followed by [`Time::sync`], which keeps the `Arc` and the pointer
//! the same zone: none where timelib cleared it, the zone it came from
//! where it copied one, and the request's where the parser asked for one.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int};
use std::ptr::{self, NonNull};
use std::sync::Arc;

use timelib_sys as sys;

#[cfg(test)]
mod tests;

pub(crate) use sys::{RelTime, TimeFields};

/// `TIMELIB_ZONETYPE_*`.
pub(crate) const ZONETYPE_OFFSET: u32 = 1;
pub(crate) const ZONETYPE_ABBR: u32 = 2;
pub(crate) const ZONETYPE_ID: u32 = 3;
/// `TIMELIB_UNSET`.
pub(crate) const UNSET: i64 = -9_999_999;
/// `TIMELIB_NO_CLONE`, `TIMELIB_OVERRIDE_TIME`.
pub(crate) const NO_CLONE: i32 = 0x02;
pub(crate) const OVERRIDE_TIME: i32 = 0x01;

/// The bytes of a C string timelib owns, empty for a null pointer.
fn bytes_of<'a>(p: *const c_char) -> &'a [u8] {
    if p.is_null() {
        return &[];
    }
    // SAFETY: timelib's strings are NUL-terminated and live at least as
    // long as the structure they were read off, which the caller holds.
    unsafe { CStr::from_ptr(p) }.to_bytes()
}

/// The bytes as a C string: timelib reads up to the first NUL, as php's
/// `ZSTR_VAL` hands it the same bytes.
fn c_string(bytes: &[u8]) -> CString {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    CString::new(&bytes[..end]).unwrap_or_default()
}

/// The bytes with the NUL after them that a `zend_string` always has:
/// timelib is handed a length, but its format parser and its interval
/// scanner also stop at the terminator.
fn terminated(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.extend_from_slice(bytes);
    out.push(0);
    out
}

/// `timelib_day_of_week`.
pub(super) fn day_of_week(y: i64, m: i64, d: i64) -> i64 {
    sys::timelib_day_of_week(y, m, d)
}

/// `timelib_iso_day_of_week`.
pub(super) fn iso_day_of_week(y: i64, m: i64, d: i64) -> i64 {
    sys::timelib_iso_day_of_week(y, m, d)
}

/// `timelib_day_of_year`.
pub(super) fn day_of_year(y: i64, m: i64, d: i64) -> i64 {
    sys::timelib_day_of_year(y, m, d)
}

/// `timelib_daynr_from_weeknr`.
pub(super) fn daynr_from_weeknr(iy: i64, iw: i64, id: i64) -> i64 {
    sys::timelib_daynr_from_weeknr(iy, iw, id)
}

/// `timelib_days_in_month`.
pub(super) fn days_in_month(y: i64, m: i64) -> i64 {
    sys::timelib_days_in_month(y, m)
}

/// `timelib_valid_date`.
pub(super) fn valid_date(y: i64, m: i64, d: i64) -> bool {
    sys::timelib_valid_date(y, m, d) != 0
}

/// `timelib_isoweek_from_date`: the ISO week and its year.
pub(super) fn isoweek_from_date(y: i64, m: i64, d: i64) -> (i64, i64) {
    let (mut iw, mut iy) = (0, 0);
    // SAFETY: both out-pointers are locals the call writes and nothing else.
    unsafe { sys::timelib_isoweek_from_date(y, m, d, &raw mut iw, &raw mut iy) };
    (iw, iy)
}

/// The bundled timezone database, `timezonedb_builtin`: data compiled into
/// the binary, never written.
#[derive(Clone, Copy)]
pub(crate) struct Tzdb(&'static sys::Tzdb);

/// One zone of the database's index: its identifier and where its data
/// starts.
pub(crate) struct IndexEntry {
    pub id: &'static [u8],
    pub pos: usize,
}

impl Tzdb {
    /// `timelib_builtin_db()`.
    pub(crate) fn builtin() -> Tzdb {
        // SAFETY: the call takes nothing and answers the address of a
        // static, which lives as long as the program.
        let db = unsafe { sys::timelib_bundled_db() };
        // SAFETY: that address is of an initialised `timelib_tzdb` that is
        // never written, so a shared reference for the program's life holds.
        Tzdb(unsafe { &*db })
    }

    fn ptr(self) -> *const sys::Tzdb {
        self.0
    }

    /// `tzdb->version`, `2026.3`.
    pub(crate) fn version(self) -> &'static [u8] {
        bytes_of(self.0.version)
    }

    /// `timelib_timezone_identifiers_list`: the index in its order.
    pub(crate) fn identifiers(self) -> Vec<IndexEntry> {
        let mut count: c_int = 0;
        // SAFETY: the database is valid and `count` a local the call writes.
        let list = unsafe { sys::timelib_timezone_identifiers_list(self.ptr(), &raw mut count) };
        let len = usize::try_from(count).unwrap_or(0);
        if list.is_null() {
            return Vec::new();
        }
        // SAFETY: timelib answers its index, `count` entries long, static.
        let entries = unsafe { std::slice::from_raw_parts(list, len) };
        entries
            .iter()
            .map(|entry| IndexEntry {
                id: bytes_of(entry.id),
                pos: usize::try_from(entry.pos).unwrap_or(0),
            })
            .collect()
    }

    /// The byte at `at` of the database's data, which php reads at an
    /// entry's `pos + 4` (`bc`) and `pos + 5`, `+ 6` (country code).
    pub(crate) fn data_byte(self, at: usize) -> u8 {
        // SAFETY: `at` is an index entry's position plus at most six,
        // inside the zone's header, which every entry of the data has.
        let byte = unsafe { self.0.data.add(at) };
        // SAFETY: that address is inside the static data, as above.
        unsafe { *byte }
    }

    /// `timelib_timezone_id_is_valid`.
    pub(crate) fn is_valid(self, id: &[u8]) -> bool {
        let id = c_string(id);
        // SAFETY: a NUL-terminated string and the database, both read only.
        unsafe { sys::timelib_timezone_id_is_valid(id.as_ptr(), self.ptr()) != 0 }
    }
}

/// A parsed zone, `timelib_tzinfo`: freed with the last `Arc`. Nothing in
/// timelib writes a zone after `timelib_parse_tzfile` built it, so it is
/// shared as immutable data. `ext/standard` reads the default one
/// (design §4.10, `php_date.h`).
pub struct Zone(NonNull<sys::TzInfo>);

// SAFETY: the zone is owned here alone and never written after its
// parse, so moving it to another thread moves plain data.
unsafe impl Send for Zone {}
// SAFETY: no function this module calls writes a zone, so shared reads
// from several threads race with nothing.
unsafe impl Sync for Zone {}

impl Drop for Zone {
    fn drop(&mut self) {
        // SAFETY: the zone was made by `timelib_parse_tzfile` and this is
        // its last owner.
        unsafe { sys::timelib_tzinfo_dtor(self.0.as_ptr()) };
    }
}

/// One local time type of a zone, a `ttinfo`.
#[derive(Debug, Clone)]
pub(crate) struct ZoneType {
    pub offset: i32,
    pub is_dst: bool,
    pub abbr: Vec<u8>,
}

/// `tlocinfo`.
#[derive(Debug, Clone)]
pub(crate) struct Location {
    pub country_code: Vec<u8>,
    pub latitude: f64,
    pub longitude: f64,
    pub comments: Vec<u8>,
}

/// `timelib_time_offset`, copied out, with php's field names: the
/// offset from UTC in seconds, whether it is daylight saving time, and
/// the abbreviation.
#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)]
pub struct Offset {
    pub offset: i32,
    pub is_dst: bool,
    pub abbr: Vec<u8>,
}

impl Zone {
    /// `timelib_parse_tzfile(name, builtin)`: none for a name the
    /// database has not.
    pub(crate) fn parse(name: &[u8]) -> Option<Zone> {
        let name = c_string(name);
        let mut error: c_int = 0;
        // SAFETY: a NUL-terminated name, the static database and a local
        // for the error code; the zone answered is this caller's.
        let zone = unsafe {
            sys::timelib_parse_tzfile(name.as_ptr(), Tzdb::builtin().ptr(), &raw mut error)
        };
        NonNull::new(zone).map(Zone)
    }

    fn info(&self) -> &sys::TzInfo {
        // SAFETY: the pointer is a live zone owned by `self` and never
        // written, so a shared reference as long as `self` holds.
        unsafe { self.0.as_ref() }
    }

    fn ptr(&self) -> *mut sys::TzInfo {
        self.0.as_ptr()
    }

    /// `tz->name`.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        bytes_of(self.info().name)
    }

    /// `tz->location`.
    pub(crate) fn location(&self) -> Location {
        let loc = &self.info().location;
        let code: Vec<u8> = loc
            .country_code
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| c.to_ne_bytes()[0])
            .collect();
        Location {
            country_code: code,
            latitude: loc.latitude,
            longitude: loc.longitude,
            comments: bytes_of(loc.comments).to_vec(),
        }
    }

    /// Whether the zone carries a POSIX string with a daylight-saving
    /// rule for the years past its last change of offset, php's
    /// `posix_info && posix_info->dst_end`.
    pub(crate) fn has_posix(&self) -> bool {
        let posix = self.info().posix_info;
        if posix.is_null() {
            return false;
        }
        // SAFETY: a non-null `posix_info` is the string the parse built
        // for this zone, alive as long as the zone.
        let posix = unsafe { &*posix };
        !posix.dst_end.is_null()
    }

    /// `tz->bit64.timecnt` transitions: their times and the index of the
    /// type each switches to.
    pub(crate) fn transitions(&self) -> Vec<(i64, usize)> {
        let info = self.info();
        let count = usize::try_from(info.bit64.timecnt).unwrap_or(0);
        if count == 0 || info.trans.is_null() || info.trans_idx.is_null() {
            return Vec::new();
        }
        // SAFETY: `trans` holds `timecnt` entries, as the parse built it.
        let times = unsafe { std::slice::from_raw_parts(info.trans, count) };
        // SAFETY: `trans_idx` holds `timecnt` entries beside them.
        let idx = unsafe { std::slice::from_raw_parts(info.trans_idx, count) };
        times
            .iter()
            .zip(idx)
            .map(|(t, i)| (*t, usize::from(*i)))
            .collect()
    }

    /// `tz->type[i]` for every type of the zone.
    pub(crate) fn types(&self) -> Vec<ZoneType> {
        let info = self.info();
        let count = usize::try_from(info.bit64.typecnt).unwrap_or(0);
        if count == 0 || info.types.is_null() {
            return Vec::new();
        }
        // SAFETY: `type` holds `typecnt` entries, as the parse built it.
        let types = unsafe { std::slice::from_raw_parts(info.types, count) };
        types
            .iter()
            .map(|t| {
                let at = usize::try_from(t.abbr_idx).unwrap_or(0);
                // SAFETY: `abbr_idx` indexes the zone's NUL-separated
                // abbreviation table, which the parse checked.
                let abbr = unsafe { info.timezone_abbr.add(at) };
                ZoneType {
                    offset: t.offset,
                    is_dst: t.isdst != 0,
                    abbr: bytes_of(abbr).to_vec(),
                }
            })
            .collect()
    }

    /// `timelib_get_time_zone_info(ts, tz)`: the zone's offset in force
    /// at the Unix time `ts`.
    #[must_use]
    pub fn offset_at(&self, ts: i64) -> Offset {
        // SAFETY: a live zone, read only; the offset answered is ours.
        let raw = unsafe { sys::timelib_get_time_zone_info(ts, self.ptr()) };
        if raw.is_null() {
            return Offset {
                offset: 0,
                is_dst: false,
                abbr: Vec::new(),
            };
        }
        // SAFETY: timelib answered an initialised offset it allocated.
        let o = unsafe { &*raw };
        let out = Offset {
            offset: o.offset,
            is_dst: o.is_dst != 0,
            abbr: bytes_of(o.abbr).to_vec(),
        };
        // SAFETY: the offset is ours and read above; nothing holds it.
        unsafe { sys::timelib_time_offset_dtor(raw) };
        out
    }

    /// `timelib_get_transitions_for_year`: the POSIX string's transitions
    /// in `year`, each a time and a type index.
    pub(crate) fn posix_transitions(&self, year: i64) -> Vec<(i64, usize)> {
        let mut out = sys::PosixTransitions {
            count: 0,
            times: [0; 6],
            types: [0; 6],
        };
        // SAFETY: a live zone with its POSIX string, and a local the call
        // fills, at most six entries.
        unsafe { sys::timelib_get_transitions_for_year(self.ptr(), year, &raw mut out) };
        (0..out.count.min(6))
            .map(|i| (out.times[i], usize::try_from(out.types[i]).unwrap_or(0)))
            .collect()
    }
}

/// `DATEG(tzcache)`: every zone the request parsed, by the name it was
/// asked for, so each is parsed once a request and freed with the
/// request's state, as php's `php_date_parse_tzfile` keeps them
/// (`Z/ext/date/php_date.c:497-520`). Request state, unsynchronised
/// (ADR-0005).
/// One zone asked for again, in another spelling and through each
/// parser, and a name the database has not, which is not kept:
///
/// ```php
/// $a = new DateTimeZone('Europe/Amsterdam'); $b = new DateTimeZone('europe/amsterdam');
/// for ($i = 0; $i < 3; $i++) { $d = new DateTime('2024-07-01 12:00 Europe/Amsterdam'); echo $d->format('c e T'), "\n"; }
/// echo date_create_from_format('!Y-m-d e', '2024-01-01 Europe/Amsterdam')->format('c e'), ' ', $a->getName(), ' ', $b->getName(), ' ', $b->getOffset($d), "\n";
/// for ($i = 0; $i < 2; $i++) { try { new DateTimeZone('Mars/Olympus'); } catch (Exception $e) { echo $e->getMessage(), "\n"; } }
/// var_dump(strtotime('2024-01-01 Mars/Olympus'), date_parse('2024-01-01 Mars/Olympus')['error_count']);
/// ```
/// ```expect
/// 2024-07-01T12:00:00+02:00 Europe/Amsterdam CEST
/// 2024-07-01T12:00:00+02:00 Europe/Amsterdam CEST
/// 2024-07-01T12:00:00+02:00 Europe/Amsterdam CEST
/// 2024-01-01T00:00:00+01:00 Europe/Amsterdam Europe/Amsterdam europe/amsterdam 7200
/// DateTimeZone::__construct(): Unknown or bad timezone (Mars/Olympus)
/// DateTimeZone::__construct(): Unknown or bad timezone (Mars/Olympus)
/// bool(false)
/// int(1)
/// ```
#[derive(Default)]
pub(crate) struct Zones(HashMap<Vec<u8>, Arc<Zone>>);

impl Zones {
    /// `php_date_parse_tzfile(name, tzdb)`: the cached zone, or the one
    /// the database parses the first time it is asked for; none for a
    /// name the database has not, which is not kept.
    pub(crate) fn parse(&mut self, name: &[u8]) -> Option<Arc<Zone>> {
        if let Some(zone) = self.0.get(name) {
            return Some(Arc::clone(zone));
        }
        let zone = Arc::new(Zone::parse(name)?);
        self.0.insert(name.to_vec(), Arc::clone(&zone));
        Some(zone)
    }

    /// How many names the request has parsed a zone for.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

/// What a parse hands timelib in place of the database: the request's
/// zones, and every zone the wrapper answered, so [`Time::sync`] shares
/// the `Arc` of the zone a parse pointed the time at.
struct Lookup<'a> {
    zones: &'a mut Zones,
    answered: Vec<Arc<Zone>>,
}

impl Lookup<'_> {
    /// The pointer timelib's parsers take as their `tzdb` and hand,
    /// untouched, to the wrapper: timelib's `parse_date.re` stores it in
    /// its scanner and passes it to `tz_wrapper` and reads it nowhere
    /// else (`timelib_parse_zone`, `timelib_strtotime`,
    /// `timelib_parse_from_format_with_map`).
    fn as_tzdb(&mut self) -> *const sys::Tzdb {
        (&raw mut *self).cast::<sys::Tzdb>().cast_const()
    }
}

/// `php_date_parse_tzfile_wrapper`, the `timelib_tz_get_wrapper` the
/// parsers are handed: the zone out of the request's [`Zones`], which
/// keeps it alive past the parse.
unsafe extern "C" fn cached_zone(
    name: *const c_char,
    lookup: *const sys::Tzdb,
    _error: *mut c_int,
) -> *mut sys::TzInfo {
    // SAFETY: `lookup` is the `Lookup::as_tzdb` of a `Lookup` the parse
    // that calls this borrows uniquely for the call and touches nothing
    // else of while timelib runs.
    let lookup = unsafe { &mut *lookup.cast::<Lookup<'_>>().cast_mut() };
    let Some(zone) = lookup.zones.parse(bytes_of(name)) else {
        return ptr::null_mut();
    };
    let at = zone.ptr();
    lookup.answered.push(zone);
    at
}

/// One message of a `timelib_error_container`, copied out.
#[derive(Debug, Clone)]
pub(crate) struct Message {
    pub position: i32,
    pub character: u8,
    pub text: Vec<u8>,
}

/// The warnings and errors of one parse.
#[derive(Debug, Clone, Default)]
pub(crate) struct Errors {
    pub warnings: Vec<Message>,
    pub errors: Vec<Message>,
}

impl Errors {
    /// The container's messages copied, and the container freed.
    fn take(raw: *mut sys::ErrorContainer) -> Errors {
        if raw.is_null() {
            return Errors::default();
        }
        // SAFETY: timelib answered an initialised container it allocated.
        let c = unsafe { &*raw };
        let read = |list: *mut sys::ErrorMessage, count: c_int| {
            let len = usize::try_from(count).unwrap_or(0);
            if list.is_null() || len == 0 {
                return Vec::new();
            }
            // SAFETY: the list holds `count` messages, as timelib counted.
            let messages = unsafe { std::slice::from_raw_parts(list, len) };
            messages
                .iter()
                .map(|m| Message {
                    position: m.position,
                    character: m.character.to_ne_bytes()[0],
                    text: bytes_of(m.message).to_vec(),
                })
                .collect()
        };
        let out = Errors {
            warnings: read(c.warning_messages, c.warning_count),
            errors: read(c.error_messages, c.error_count),
        };
        // SAFETY: the container is ours and copied above.
        unsafe { sys::timelib_error_container_dtor(raw) };
        out
    }
}

/// An owned `timelib_time`, and the zone its `tz_info` points at.
pub(crate) struct Time {
    raw: NonNull<TimeFields>,
    zone: Option<Arc<Zone>>,
}

// SAFETY: the time is owned here alone; the zone it points at is `Send`
// and `Sync` and held by the `Arc` beside it.
unsafe impl Send for Time {}

impl Drop for Time {
    fn drop(&mut self) {
        // SAFETY: the time was made by timelib and this is its one owner;
        // the dtor frees `tz_abbr` and the struct and never the zone.
        unsafe { sys::timelib_time_dtor(self.raw.as_ptr()) };
    }
}

impl std::ops::Deref for Time {
    type Target = TimeFields;
    fn deref(&self) -> &TimeFields {
        // SAFETY: a live time owned by `self`.
        unsafe { self.raw.as_ref() }
    }
}

impl std::ops::DerefMut for Time {
    fn deref_mut(&mut self) -> &mut TimeFields {
        // SAFETY: a live time owned by `self`, borrowed uniquely.
        unsafe { self.raw.as_mut() }
    }
}

impl Clone for Time {
    /// `timelib_time_clone`: the abbreviation copied and the zone shared.
    fn clone(&self) -> Time {
        // SAFETY: a live time; the copy answered is ours.
        let raw = unsafe { sys::timelib_time_clone(self.raw.as_ptr()) };
        Time::adopt(raw, self.zone.clone(), &[])
    }
}

impl Time {
    /// A time timelib allocated, with the zone its pointer is known to
    /// name or one of the zones a parse's wrapper `answered`; a null answer is timelib's allocation failing, which its
    /// `calloc` aborts on before it could return.
    fn adopt(raw: *mut TimeFields, zone: Option<Arc<Zone>>, answered: &[Arc<Zone>]) -> Time {
        let raw = NonNull::new(raw).unwrap_or_else(|| std::process::abort());
        let mut time = Time { raw, zone };
        let known: Vec<&Arc<Zone>> = answered.iter().collect();
        time.sync(&known);
        time
    }

    /// `timelib_time_ctor()`: every field zero.
    pub(crate) fn new() -> Time {
        // SAFETY: the call takes nothing; the time answered is ours.
        let raw = unsafe { sys::timelib_time_ctor() };
        Time::adopt(raw, None, &[])
    }

    fn ptr(&self) -> *mut TimeFields {
        self.raw.as_ptr()
    }

    /// The `Arc` and the pointer brought back to one zone after a call
    /// that may have moved `tz_info`: none where it is null, ours where
    /// it is still ours, and one of `known` where it names one of theirs.
    /// timelib makes no zone of its own on any path called from here:
    /// its parsers ask the wrapper, which answers out of [`Zones`], and
    /// `timelib_fill_holes` is always handed `TIMELIB_NO_CLONE`. A pointer
    /// none of them names is a zone nothing here owns, so the process
    /// aborts rather than read or free it, as on an allocation failure.
    fn sync(&mut self, known: &[&Arc<Zone>]) {
        let now = self.tz_info();
        if now.is_null() {
            self.zone = None;
            return;
        }
        if self.zone.as_ref().is_some_and(|z| z.ptr() == now) {
            return;
        }
        match known.iter().find(|z| z.ptr() == now) {
            Some(found) => self.zone = Some(Arc::clone(found)),
            None => std::process::abort(),
        }
    }

    /// The zone `tz_info` points at.
    pub(crate) fn zone(&self) -> Option<&Arc<Zone>> {
        self.zone.as_ref()
    }

    /// `t->tz_abbr`.
    pub(crate) fn tz_abbr(&self) -> &[u8] {
        bytes_of(TimeFields::tz_abbr(self))
    }

    /// `timelib_unixtime2gmt`.
    pub(crate) fn unixtime2gmt(&mut self, ts: i64) {
        // SAFETY: a live time owned by `self`.
        unsafe { sys::timelib_unixtime2gmt(self.ptr(), ts) };
        self.sync(&[]);
    }

    /// `timelib_unixtime2local`: under the zone the time carries.
    pub(crate) fn unixtime2local(&mut self, ts: i64) {
        // SAFETY: a live time whose zone, if any, `self.zone` keeps alive.
        unsafe { sys::timelib_unixtime2local(self.ptr(), ts) };
        self.sync(&[]);
    }

    /// `t->tz_info = tzi; t->zone_type = TIMELIB_ZONETYPE_ID;`, what php
    /// writes before `timelib_unixtime2local`.
    pub(crate) fn set_zone_id(&mut self, zone: Arc<Zone>) {
        // SAFETY: a zone timelib parsed, which the `Arc` stored beside the
        // pointer keeps alive as long as the time points at it.
        unsafe { self.set_tz_info(zone.ptr()) };
        self.zone = Some(zone);
        self.zone_type = ZONETYPE_ID;
    }

    /// `timelib_set_timezone(t, tz)`.
    pub(crate) fn set_timezone(&mut self, zone: &Arc<Zone>) {
        // SAFETY: a live time and a live zone the `Arc` keeps alive.
        unsafe { sys::timelib_set_timezone(self.ptr(), zone.ptr()) };
        self.sync(&[zone]);
    }

    /// `timelib_set_timezone_from_offset(t, offset)`.
    pub(crate) fn set_timezone_from_offset(&mut self, offset: i64) {
        // SAFETY: a live time owned by `self`.
        unsafe { sys::timelib_set_timezone_from_offset(self.ptr(), offset) };
        self.sync(&[]);
    }

    /// `timelib_set_timezone_from_abbr(t, info)`: timelib copies `abbr`.
    pub(crate) fn set_timezone_from_abbr(&mut self, utc_offset: i64, abbr: &[u8], dst: i32) {
        let abbr = c_string(abbr);
        let info = sys::AbbrInfo {
            utc_offset,
            abbr: abbr.as_ptr().cast_mut(),
            dst,
        };
        // SAFETY: a live time; the abbreviation outlives the call, which
        // copies it and writes nothing through it.
        unsafe { sys::timelib_set_timezone_from_abbr(self.ptr(), info) };
        self.sync(&[]);
    }

    /// `t->tz_abbr = timelib_strdup(abbr)` through the only door timelib
    /// has for it that keeps the zone: set from the abbreviation and the
    /// fields restored.
    pub(crate) fn set_abbr(&mut self, abbr: &[u8]) {
        let (z, dst, zone_type, have_zone) = (self.z, self.dst, self.zone_type, self.have_zone);
        let zone = self.zone.clone();
        let info = self.tz_info();
        self.set_timezone_from_abbr(i64::from(z), abbr, dst);
        self.z = z;
        self.dst = dst;
        self.zone_type = zone_type;
        self.have_zone = have_zone;
        // SAFETY: the pointer the time held before the call, and the `Arc`
        // restored beside it keeps that zone alive.
        unsafe { self.set_tz_info(info) };
        self.zone = zone;
    }

    /// `timelib_update_ts(t, tzi)`: under `zone`, or the time's own when
    /// none is given, as php passes `NULL`.
    pub(crate) fn update_ts(&mut self, zone: Option<&Arc<Zone>>) {
        let tzi = zone.map_or(ptr::null_mut(), |z| z.ptr());
        // SAFETY: a live time and, where given, a zone the `Arc` keeps alive.
        unsafe { sys::timelib_update_ts(self.ptr(), tzi) };
        let known: Vec<&Arc<Zone>> = zone.into_iter().collect();
        self.sync(&known);
    }

    /// `timelib_update_from_sse`.
    pub(crate) fn update_from_sse(&mut self) {
        // SAFETY: a live time whose zone `self.zone` keeps alive.
        unsafe { sys::timelib_update_from_sse(self.ptr()) };
        self.sync(&[]);
    }

    /// `timelib_fill_holes(t, now, options)`; `now`'s zone is shared, as
    /// `TIMELIB_NO_CLONE` asks, so the option is always given.
    pub(crate) fn fill_holes(&mut self, now: &mut Time, options: i32) {
        // SAFETY: two live times; with `TIMELIB_NO_CLONE` the call copies
        // `now`'s zone pointer, which `now.zone` keeps alive and `sync`
        // then shares by its `Arc`.
        unsafe { sys::timelib_fill_holes(self.ptr(), now.ptr(), options | NO_CLONE) };
        let zones: Vec<&Arc<Zone>> = now.zone.iter().collect();
        self.sync(&zones);
    }

    /// `timelib_date_to_int`: none where the epoch does not fit.
    pub(crate) fn date_to_int(&mut self) -> Option<i64> {
        let mut error: c_int = 0;
        // SAFETY: a live time and a local for the error flag.
        let ts = unsafe { sys::timelib_date_to_int(self.ptr(), &raw mut error) };
        (error == 0).then_some(ts)
    }

    /// `timelib_get_current_offset`.
    pub(crate) fn current_offset(&self) -> i64 {
        // SAFETY: a live time whose zone `self.zone` keeps alive; the
        // call reads it.
        unsafe { sys::timelib_get_current_offset(self.ptr()) }
    }

    /// `timelib_time_compare`.
    pub(crate) fn compare(&self, other: &Time) -> i32 {
        // SAFETY: two live times, read only.
        unsafe { sys::timelib_time_compare(self.ptr(), other.ptr()) }
    }

    /// `timelib_diff(one, two)`.
    pub(crate) fn diff(&self, other: &Time) -> RelTime {
        // SAFETY: two live times; the relative time answered is ours.
        let raw = unsafe { sys::timelib_diff(self.ptr(), other.ptr()) };
        take_rel(raw)
    }

    fn arith(
        &self,
        rel: &RelTime,
        f: unsafe extern "C" fn(*mut TimeFields, *mut RelTime) -> *mut TimeFields,
    ) -> Time {
        let mut rel = *rel;
        // SAFETY: a live time and a local copy of the interval; the time
        // answered is a clone of ours, sharing our zone.
        let raw = unsafe { f(self.ptr(), &raw mut rel) };
        let known: Vec<&Arc<Zone>> = self.zone.iter().collect();
        let mut out = Time {
            raw: NonNull::new(raw).unwrap_or_else(|| std::process::abort()),
            zone: self.zone.clone(),
        };
        out.sync(&known);
        out
    }

    /// `timelib_add`.
    pub(crate) fn add(&self, rel: &RelTime) -> Time {
        self.arith(rel, sys::timelib_add)
    }

    /// `timelib_add_wall`.
    pub(crate) fn add_wall(&self, rel: &RelTime) -> Time {
        self.arith(rel, sys::timelib_add_wall)
    }

    /// `timelib_sub`.
    pub(crate) fn sub(&self, rel: &RelTime) -> Time {
        self.arith(rel, sys::timelib_sub)
    }

    /// `timelib_sub_wall`.
    pub(crate) fn sub_wall(&self, rel: &RelTime) -> Time {
        self.arith(rel, sys::timelib_sub_wall)
    }

    /// `timelib_astro_rise_set_altitude`: the status, the rise and set
    /// hours and the rise, set and transit timestamps.
    pub(crate) fn rise_set(
        &mut self,
        lon: f64,
        lat: f64,
        altitude: f64,
        upper_limb: bool,
    ) -> (i32, f64, f64, i64, i64, i64) {
        let (mut h_rise, mut h_set) = (0.0, 0.0);
        let (mut rise, mut set, mut transit) = (0, 0, 0);
        // SAFETY: a live time and five locals the call writes.
        let rs = unsafe {
            sys::timelib_astro_rise_set_altitude(
                self.ptr(),
                lon,
                lat,
                altitude,
                c_int::from(upper_limb),
                &raw mut h_rise,
                &raw mut h_set,
                &raw mut rise,
                &raw mut set,
                &raw mut transit,
            )
        };
        (rs, h_rise, h_set, rise, set, transit)
    }

    /// `timelib_strtotime(s, len, &errors, DATE_TIMEZONEDB,
    /// php_date_parse_tzfile_wrapper)`: the parse and its messages, each
    /// zone it names read out of the request's `zones`.
    pub(crate) fn strtotime(zones: &mut Zones, s: &[u8]) -> (Time, Errors) {
        let text = terminated(s);
        let mut errors: *mut sys::ErrorContainer = ptr::null_mut();
        let mut lookup = Lookup {
            zones,
            answered: Vec::new(),
        };
        let tzdb = lookup.as_tzdb();
        // SAFETY: the bytes, NUL-terminated, and their length, a local
        // for the container, and the lookup with the wrapper that reads
        // it, borrowed for the call alone.
        let raw = unsafe {
            sys::timelib_strtotime(
                text.as_ptr().cast(),
                s.len(),
                &raw mut errors,
                tzdb,
                cached_zone,
            )
        };
        (
            Time::adopt(raw, None, &lookup.answered),
            Errors::take(errors),
        )
    }

    /// `timelib_parse_from_format(format, s, len, &errors, ...)`.
    pub(crate) fn parse_from_format(zones: &mut Zones, format: &[u8], s: &[u8]) -> (Time, Errors) {
        let format = c_string(format);
        let text = terminated(s);
        let mut errors: *mut sys::ErrorContainer = ptr::null_mut();
        let mut lookup = Lookup {
            zones,
            answered: Vec::new(),
        };
        let tzdb = lookup.as_tzdb();
        // SAFETY: a NUL-terminated format, the bytes, NUL-terminated, and
        // their length, a local for the container, and the lookup with
        // the wrapper that reads it, borrowed for the call alone.
        let raw = unsafe {
            sys::timelib_parse_from_format(
                format.as_ptr(),
                text.as_ptr().cast(),
                s.len(),
                &raw mut errors,
                tzdb,
                cached_zone,
            )
        };
        (
            Time::adopt(raw, None, &lookup.answered),
            Errors::take(errors),
        )
    }

    /// `timelib_parse_zone(&ptr, &dst, t, &not_found, ...)` over `s` into
    /// this time, what `timezone_initialize` asks of a dummy time: the
    /// offset, whether no zone was found, and how many bytes were read.
    pub(crate) fn parse_zone(&mut self, zones: &mut Zones, s: &[u8]) -> (i64, i32, bool, usize) {
        let text = c_string(s);
        let start = text.as_ptr();
        let mut at = start;
        let (mut dst, mut not_found): (c_int, c_int) = (0, 0);
        let mut lookup = Lookup {
            zones,
            answered: Vec::new(),
        };
        let tzdb = lookup.as_tzdb();
        // SAFETY: a NUL-terminated string whose cursor is a local, two
        // locals for the flags, a live time, and the lookup with the
        // wrapper that reads it, borrowed for the call alone.
        let offset = unsafe {
            sys::timelib_parse_zone(
                &raw mut at,
                &raw mut dst,
                self.ptr(),
                &raw mut not_found,
                tzdb,
                cached_zone,
            )
        };
        let answered: Vec<&Arc<Zone>> = lookup.answered.iter().collect();
        self.sync(&answered);
        let read = (at as usize).saturating_sub(start as usize);
        (offset, dst, not_found != 0, read)
    }
}

/// A relative time timelib allocated, copied and freed.
fn take_rel(raw: *mut RelTime) -> RelTime {
    if raw.is_null() {
        return RelTime::default();
    }
    // SAFETY: timelib answered an initialised relative time it allocated.
    let out = unsafe { *raw };
    // SAFETY: ours, and copied above.
    unsafe { sys::timelib_rel_time_dtor(raw) };
    out
}

/// What `timelib_strtointerval` makes of an ISO 8601 interval.
pub(crate) struct IsoInterval {
    pub begin: Option<Time>,
    pub end: Option<Time>,
    pub period: Option<RelTime>,
    pub recurrences: i32,
    pub errors: Errors,
}

/// `timelib_strtointerval(s, len, &b, &e, &p, &r, &errors)`.
pub(super) fn strtointerval(s: &[u8]) -> IsoInterval {
    let text = terminated(s);
    let mut begin: *mut TimeFields = ptr::null_mut();
    let mut end: *mut TimeFields = ptr::null_mut();
    let mut period: *mut RelTime = ptr::null_mut();
    let mut recurrences: c_int = 0;
    let mut errors: *mut sys::ErrorContainer = ptr::null_mut();
    // SAFETY: the bytes and their length and five locals the call writes;
    // every structure it answers is ours.
    unsafe {
        sys::timelib_strtointerval(
            text.as_ptr().cast(),
            s.len(),
            &raw mut begin,
            &raw mut end,
            &raw mut period,
            &raw mut recurrences,
            &raw mut errors,
        );
    }
    IsoInterval {
        begin: (!begin.is_null()).then(|| Time::adopt(begin, None, &[])),
        end: (!end.is_null()).then(|| Time::adopt(end, None, &[])),
        period: (!period.is_null()).then(|| take_rel(period)),
        recurrences,
        errors: Errors::take(errors),
    }
}

/// One row of `timelib_timezone_abbreviations_list()`.
pub(crate) struct Abbreviation {
    pub name: &'static [u8],
    pub is_dst: bool,
    pub gmtoffset: f32,
    pub full_tz_name: Option<&'static [u8]>,
}

/// `timelib_timezone_abbreviations_list()`, every row to the one whose
/// name is null.
pub(super) fn abbreviations() -> Vec<Abbreviation> {
    // SAFETY: the call takes nothing and answers a static table.
    let mut row = unsafe { sys::timelib_timezone_abbreviations_list() };
    let mut out = Vec::new();
    loop {
        // SAFETY: the table is terminated by a row whose name is null, and
        // `row` has not passed it.
        let entry = unsafe { &*row };
        if entry.name.is_null() {
            return out;
        }
        out.push(Abbreviation {
            name: bytes_of(entry.name),
            is_dst: entry.kind != 0,
            gmtoffset: entry.gmtoffset,
            full_tz_name: (!entry.full_tz_name.is_null()).then(|| bytes_of(entry.full_tz_name)),
        });
        // SAFETY: the row read is not the terminator, so the next one is
        // inside the table.
        row = unsafe { row.add(1) };
    }
}

/// `timelib_timezone_id_from_abbr(abbr, gmtoffset, isdst)`.
pub(super) fn timezone_id_from_abbr(
    abbr: &[u8],
    gmtoffset: i64,
    isdst: i32,
) -> Option<&'static [u8]> {
    let abbr = c_string(abbr);
    // SAFETY: a NUL-terminated string read only; the answer is static.
    let id = unsafe { sys::timelib_timezone_id_from_abbr(abbr.as_ptr(), gmtoffset, isdst) };
    (!id.is_null()).then(|| bytes_of(id))
}

/// The `struct tm` `php_strftime` fills from a timelib time: every field
/// a number, the month from 0 and the year from 1900, and the zone's
/// offset and abbreviation, `GMT` and 0 for `gmstrftime`.
#[derive(Debug)]
pub(crate) struct BrokenDown<'a> {
    pub sec: i64,
    pub min: i64,
    pub hour: i64,
    pub mday: i64,
    pub mon: i64,
    pub year: i64,
    pub wday: i64,
    pub yday: i64,
    pub isdst: bool,
    pub gmtoff: i64,
    pub zone: &'a [u8],
}

/// `strftime(3)` under the thread's `LC_TIME`, as `php_strftime`
/// (`Z/ext/date/php_date.c:1335`) calls it: into 256 bytes, then twice
/// as many each time it answers nothing, up to 4096, so `None` both
/// where the text is longer than 4095 bytes and where it is empty.
/// The format and the zone are read to their first NUL.
pub(crate) fn strftime(format: &[u8], tm: &BrokenDown<'_>) -> Option<Vec<u8>> {
    let format = c_string(format);
    let zone = c_string(tm.zone);
    let int = |n: i64| c_int::try_from(n).unwrap_or(c_int::MAX);
    let fields = libc::tm {
        tm_sec: int(tm.sec),
        tm_min: int(tm.min),
        tm_hour: int(tm.hour),
        tm_mday: int(tm.mday),
        tm_mon: int(tm.mon),
        tm_year: int(tm.year),
        tm_wday: int(tm.wday),
        tm_yday: int(tm.yday),
        tm_isdst: c_int::from(tm.isdst),
        tm_gmtoff: libc::c_long::try_from(tm.gmtoff).unwrap_or(0),
        tm_zone: zone.as_ptr().cast_mut(),
    };
    let mut len = 256;
    for _ in 0..5 {
        let mut buf = vec![0u8; len];
        // SAFETY: `buf` has `len` writable bytes, `strftime` writes at
        // most that many, and the format and the `tm` it reads, whose
        // zone points at `zone`, outlive the call.
        let written = unsafe {
            libc::strftime(
                buf.as_mut_ptr().cast::<c_char>(),
                len,
                format.as_ptr(),
                &raw const fields,
            )
        };
        if written != 0 && written != len {
            buf.truncate(written);
            return Some(buf);
        }
        len *= 2;
    }
    None
}
