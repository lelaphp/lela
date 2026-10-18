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
//! - [`RelTime`], an owned `timelib_rel_time`, a `DateInterval`'s `diff`
//!   and a `DatePeriod`'s `interval`; its numbers are [`RelFields`];
//! - [`Tzdb`], the bundled database, the data compiled into the binary, never written;
//! - [`Zones`], `DATEG(tzcache)`: the zones a request parsed, by the name
//!   asked for, which the parsers' zone wrapper reads as php's
//!   `php_date_parse_tzfile_wrapper` reads the cache;
//! - [`ErrorContainer`], a parse's `timelib_error_container`.
//!
//! timelib allocates on the request's heap, as php's build has it
//! (ADR-0014): every call that can allocate or free takes a [`Lent`],
//! the proof the request's heap is lent for it, and each owner's drop
//! under a lend gives its memory back ([`crate::lent`]).
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
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr::{self, NonNull};
use std::sync::Arc;

use timelib_sys as sys;

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::NameHasher;

use crate::lent::{Lent, lent};

#[cfg(test)]
mod tests;

pub(crate) use sys::{RelFields, TimeFields};

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
    CString::new(php_ext::coerce::c_str(bytes)).unwrap_or_default()
}

/// `call` handed [`c_string`]'s string of `bytes`, laid on this stack
/// where it fits, as every zone name does, so a parse that runs once per
/// call allocates nothing to terminate it.
fn with_c_string<R>(bytes: &[u8], call: impl FnOnce(&CStr) -> R) -> R {
    const ROOM: usize = 64;
    let bytes = php_ext::coerce::c_str(bytes);
    let end = bytes.len();
    let mut room = [0u8; ROOM];
    if let Some(slot) = room.get_mut(..end)
        && end < ROOM
    {
        slot.copy_from_slice(&bytes[..end]);
        if let Ok(text) = CStr::from_bytes_until_nul(&room) {
            return call(text);
        }
    }
    call(&c_string(bytes))
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

/// `timelib_unixtime2date(ts, &y, &m, &d)`: the date of the Unix time
/// in UTC.
pub(super) fn unixtime2date(ts: i64) -> (i64, i64, i64) {
    let (mut y, mut m, mut d) = (0, 0, 0);
    // SAFETY: the three out-pointers are locals the call writes and
    // nothing else.
    unsafe { sys::timelib_unixtime2date(ts, &raw mut y, &raw mut m, &raw mut d) };
    (y, m, d)
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
    fn parse(_: &Lent<'_>, name: &[u8]) -> Option<Zone> {
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

    /// `timelib_get_time_zone_info(ts, tz)` with the request's heap lent
    /// for it, what ext/standard's `gettimeofday` asks of the default
    /// zone (design §4.10): the zone's offset in force at the Unix time
    /// `ts`, made and freed on the request's heap as php's is.
    ///
    /// # Errors
    ///
    /// The memory limit.
    pub fn offset_at(&self, req: &mut Request, ts: i64) -> Result<Offset, Thrown> {
        lent(req, |l| self.offset_in(l, ts))
    }

    /// `timelib_get_time_zone_info(ts, tz)`: the zone's offset in force
    /// at the Unix time `ts`.
    pub(crate) fn offset_in(&self, _: &Lent<'_>, ts: i64) -> Offset {
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
///
/// php's table is on the request's heap: `ALLOC_HASHTABLE` at the first
/// ask, its data at the first zone, doubled at the ninth, and a
/// `zend_string` key per zone. So the first zone a request parses counts
/// the table's 424 bytes beside the zone's own, and the ninth the growth
/// from 8 slots to 16:
///
/// ```php
/// $m = memory_get_usage(); $a = new DateTimeZone('Europe/Amsterdam'); var_dump(memory_get_usage() - $m);
/// $m = memory_get_usage(); $b = new DateTimeZone('America/New_York'); var_dump(memory_get_usage() - $m);
/// $c = new DateTimeZone('Asia/Tokyo'); $d = new DateTimeZone('Europe/London'); $e = new DateTimeZone('Australia/Sydney');
/// $f = new DateTimeZone('Africa/Cairo'); $g = new DateTimeZone('America/Chicago'); $h = new DateTimeZone('Europe/Paris');
/// $m = memory_get_usage(); $i = new DateTimeZone('Asia/Kolkata'); var_dump(memory_get_usage() - $m);
/// $m = memory_get_usage(); $j = new DateTimeZone('Pacific/Auckland'); var_dump(memory_get_usage() - $m);
/// $m = memory_get_usage(); $k = new DateTimeZone('Europe/Amsterdam'); var_dump(memory_get_usage() - $m);
/// ```
/// ```expect capture
/// int(2144)
/// int(2440)
/// int(1008)
/// int(1584)
/// int(80)
/// ```
#[derive(Default)]
pub(crate) struct Zones {
    /// The zones by the name each was asked for, each with its key.
    zones: HashMap<Vec<u8>, Cached, NameHasher>,
    /// The table's own blocks, made at the first ask.
    table: Option<Table>,
}

/// One entry of `DATEG(tzcache)`: the zone, and the block its
/// `zend_string` key stands in.
struct Cached {
    zone: Arc<Zone>,
    _key: Option<Block>,
}

/// The blocks of `DATEG(tzcache)`'s `HashTable` itself.
struct Table {
    /// `ALLOC_HASHTABLE`: the `HashTable` struct.
    _header: Option<Block>,
    /// The buckets and hash slots `zend_hash` allocates at the first
    /// insert and doubles when they are full; none before the first zone.
    data: Option<Block>,
}

impl Table {
    /// `sizeof(HashTable)`.
    const HEADER: usize = 56;

    /// `HT_SIZE_EX(n, HT_SIZE_TO_MASK(n))` for the table size that holds
    /// `entries`: a 32-byte bucket and two 4-byte hash slots per entry
    /// of a size `zend_hash_init(4)` starts at 8 and doubles.
    fn data_size(entries: usize) -> usize {
        entries.next_power_of_two().max(8) * 40
    }
}

impl Zones {
    /// How many names the request has parsed a zone for.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.zones.len()
    }
}

impl Lent<'_> {
    /// `php_date_parse_tzfile(name, tzdb)`: the cached zone, or the one
    /// the database parses the first time it is asked for, on the
    /// request's heap as php's `DATEG(tzcache)` entry is; none for a name
    /// the database has not, which is not kept. The table's blocks are
    /// made in php's order: the header before the first lookup, then the
    /// zone, then the data grown before the key is made.
    pub(crate) fn zone(&mut self, name: &[u8]) -> Option<Arc<Zone>> {
        if self.state().zones.table.is_none() {
            let header = Block::new(self, Table::HEADER);
            self.state().zones.table = Some(Table {
                _header: header,
                data: None,
            });
        }
        if let Some(cached) = self.state().zones.zones.get(name) {
            return Some(Arc::clone(&cached.zone));
        }
        let zone = Arc::new(Zone::parse(self, name)?);
        let size = Table::data_size(self.state().zones.zones.len() + 1);
        let grown = self
            .state()
            .zones
            .table
            .as_ref()
            .and_then(|t| t.data.as_ref())
            .map(Block::size);
        if grown != Some(size) {
            let data = Block::new(self, size);
            if let Some(table) = &mut self.state().zones.table {
                table.data = data;
            }
        }
        let key = Block::new(self, string_size(name.len()));
        let cached = Cached {
            zone: Arc::clone(&zone),
            _key: key,
        };
        self.state().zones.zones.insert(name.to_vec(), cached);
        Some(zone)
    }
}

/// `_ZSTR_STRUCT_SIZE(len)`: a `zend_string`'s 24-byte header, its bytes
/// and the NUL, aligned to 8.
fn string_size(len: usize) -> usize {
    (24 + len + 1).next_multiple_of(8)
}

/// One block timelib's allocator made on the request's heap for a struct
/// php keeps there and this crate keeps as a Rust value: freed by its
/// drop, which under a lend gives it back.
struct Block {
    at: NonNull<c_void>,
    size: usize,
}

// SAFETY: the block is owned here alone and never read or written; only
// its address travels, to the free.
unsafe impl Send for Block {}

impl Block {
    /// `timelib_malloc(size)`; none where the allocator answered none.
    fn new(_: &Lent<'_>, size: usize) -> Option<Block> {
        // SAFETY: an allocation of `size` bytes under the lend, this
        // block's alone.
        let at = unsafe { sys::timelib_malloc(size) };
        NonNull::new(at).map(|at| Block { at, size })
    }

    /// The size the block was asked for.
    fn size(&self) -> usize {
        self.size
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        // SAFETY: the block `timelib_malloc` answered, and this its one
        // owner.
        unsafe { sys::timelib_free(self.at.as_ptr()) };
    }
}

/// What a parse hands timelib in place of the database: the lend, whose
/// state holds the request's zones, and every zone the wrapper answered,
/// so [`Time::sync`] shares the `Arc` of the zone a parse pointed the
/// time at.
struct Lookup<'a, 'b> {
    lent: &'a mut Lent<'b>,
    answered: Answered,
}

/// The zones one parse's wrapper answered, in the order it answered
/// them: the first beside the rest, so a zone parse, which asks once,
/// allocates nothing of its own to keep it.
#[derive(Default)]
struct Answered {
    first: Option<Arc<Zone>>,
    rest: Vec<Arc<Zone>>,
}

impl Answered {
    fn push(&mut self, zone: Arc<Zone>) {
        if self.first.is_none() {
            self.first = Some(zone);
        } else {
            self.rest.push(zone);
        }
    }

    fn iter(&self) -> impl Iterator<Item = &Arc<Zone>> {
        self.first.iter().chain(&self.rest)
    }
}

impl Lookup<'_, '_> {
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
    let lookup = unsafe { &mut *lookup.cast::<Lookup<'_, '_>>().cast_mut() };
    let Some(zone) = lookup.lent.zone(bytes_of(name)) else {
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
    /// Whether the parse left any message, what decides whether php's
    /// `update_errors_warnings` keeps its container.
    pub(crate) fn any(&self) -> bool {
        !self.warnings.is_empty() || !self.errors.is_empty()
    }
}

/// A parse's `timelib_error_container`, as timelib allocated it on the
/// request's heap: freed by its drop, which under a lend gives it back.
/// `DATEG(last_errors)` keeps one for as long as php keeps it.
pub(crate) struct ErrorContainer(NonNull<sys::ErrorContainer>);

// SAFETY: the container is owned here alone, plain data timelib wrote
// once and reads nowhere after.
unsafe impl Send for ErrorContainer {}

impl Drop for ErrorContainer {
    fn drop(&mut self) {
        // SAFETY: the container timelib answered, and this its one owner.
        unsafe { sys::timelib_error_container_dtor(self.0.as_ptr()) };
    }
}

impl ErrorContainer {
    /// The container a parse answered, none for none.
    fn adopt(raw: *mut sys::ErrorContainer) -> Option<ErrorContainer> {
        NonNull::new(raw).map(ErrorContainer)
    }

    /// The messages, copied out.
    pub(crate) fn messages(&self) -> Errors {
        // SAFETY: a live container owned by `self`, read only.
        let c = unsafe { self.0.as_ref() };
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
        Errors {
            warnings: read(c.warning_messages, c.warning_count),
            errors: read(c.error_messages, c.error_count),
        }
    }
}

/// A parse's time, and its container of messages.
pub(crate) struct Parsed {
    pub time: Time,
    pub errors: Option<ErrorContainer>,
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

impl Time {
    /// `timelib_time_clone`: the abbreviation copied and the zone shared.
    pub(crate) fn copy(&self, _: &Lent<'_>) -> Time {
        // SAFETY: a live time; the copy answered is ours.
        let raw = unsafe { sys::timelib_time_clone(self.raw.as_ptr()) };
        Time::adopt(raw, self.zone.clone(), &Answered::default())
    }

    /// A time timelib allocated, with the zone its pointer is known to
    /// name or one of the zones a parse's wrapper `answered`; a null
    /// answer is an allocation failing, which the process does not
    /// outlive.
    fn adopt(raw: *mut TimeFields, zone: Option<Arc<Zone>>, answered: &Answered) -> Time {
        let raw = NonNull::new(raw).unwrap_or_else(|| std::process::abort());
        let mut time = Time { raw, zone };
        time.sync(answered.iter());
        time
    }

    /// `timelib_time_ctor()`: every field zero.
    pub(crate) fn new(_: &Lent<'_>) -> Time {
        // SAFETY: the call takes nothing; the time answered is ours.
        let raw = unsafe { sys::timelib_time_ctor() };
        Time::adopt(raw, None, &Answered::default())
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
    fn sync<'z>(&mut self, known: impl IntoIterator<Item = &'z Arc<Zone>>) {
        let now = self.tz_info();
        if now.is_null() {
            self.zone = None;
            return;
        }
        if self.zone.as_ref().is_some_and(|z| z.ptr() == now) {
            return;
        }
        match known.into_iter().find(|z| z.ptr() == now) {
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
    pub(crate) fn unixtime2gmt(&mut self, _: &Lent<'_>, ts: i64) {
        // SAFETY: a live time owned by `self`.
        unsafe { sys::timelib_unixtime2gmt(self.ptr(), ts) };
        self.sync([]);
    }

    /// `timelib_unixtime2local`: under the zone the time carries.
    pub(crate) fn unixtime2local(&mut self, _: &Lent<'_>, ts: i64) {
        // SAFETY: a live time whose zone, if any, `self.zone` keeps alive.
        unsafe { sys::timelib_unixtime2local(self.ptr(), ts) };
        self.sync([]);
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
    pub(crate) fn set_timezone(&mut self, _: &Lent<'_>, zone: &Arc<Zone>) {
        // SAFETY: a live time and a live zone the `Arc` keeps alive.
        unsafe { sys::timelib_set_timezone(self.ptr(), zone.ptr()) };
        self.sync([zone]);
    }

    /// `timelib_set_timezone_from_offset(t, offset)`.
    pub(crate) fn set_timezone_from_offset(&mut self, _: &Lent<'_>, offset: i64) {
        // SAFETY: a live time owned by `self`.
        unsafe { sys::timelib_set_timezone_from_offset(self.ptr(), offset) };
        self.sync([]);
    }

    /// `timelib_set_timezone_from_abbr(t, info)`: timelib copies `abbr`.
    pub(crate) fn set_timezone_from_abbr(
        &mut self,
        _: &Lent<'_>,
        utc_offset: i64,
        abbr: &[u8],
        dst: i32,
    ) {
        let abbr = c_string(abbr);
        let info = sys::AbbrInfo {
            utc_offset,
            abbr: abbr.as_ptr().cast_mut(),
            dst,
        };
        // SAFETY: a live time; the abbreviation outlives the call, which
        // copies it and writes nothing through it.
        unsafe { sys::timelib_set_timezone_from_abbr(self.ptr(), info) };
        self.sync([]);
    }

    /// `t->tz_abbr = timelib_strdup(abbr)` through the only door timelib
    /// has for it that keeps the zone: set from the abbreviation and the
    /// fields restored.
    pub(crate) fn set_abbr(&mut self, lent: &Lent<'_>, abbr: &[u8]) {
        let (z, dst, zone_type, have_zone) = (self.z, self.dst, self.zone_type, self.have_zone);
        let zone = self.zone.clone();
        let info = self.tz_info();
        self.set_timezone_from_abbr(lent, i64::from(z), abbr, dst);
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
    pub(crate) fn update_ts(&mut self, _: &Lent<'_>, zone: Option<&Arc<Zone>>) {
        let tzi = zone.map_or(ptr::null_mut(), |z| z.ptr());
        // SAFETY: a live time and, where given, a zone the `Arc` keeps alive.
        unsafe { sys::timelib_update_ts(self.ptr(), tzi) };
        self.sync(zone);
    }

    /// `timelib_update_from_sse`.
    pub(crate) fn update_from_sse(&mut self, _: &Lent<'_>) {
        // SAFETY: a live time whose zone `self.zone` keeps alive.
        unsafe { sys::timelib_update_from_sse(self.ptr()) };
        self.sync([]);
    }

    /// `timelib_fill_holes(t, now, options)`; `now`'s zone is shared, as
    /// `TIMELIB_NO_CLONE` asks, so the option is always given.
    pub(crate) fn fill_holes(&mut self, _: &Lent<'_>, now: &mut Time, options: i32) {
        // SAFETY: two live times; with `TIMELIB_NO_CLONE` the call copies
        // `now`'s zone pointer, which `now.zone` keeps alive and `sync`
        // then shares by its `Arc`.
        unsafe { sys::timelib_fill_holes(self.ptr(), now.ptr(), options | NO_CLONE) };
        self.sync(&now.zone);
    }

    /// `timelib_date_to_int`: none where the epoch does not fit.
    pub(crate) fn date_to_int(&mut self) -> Option<i64> {
        let mut error: c_int = 0;
        // SAFETY: a live time and a local for the error flag.
        let ts = unsafe { sys::timelib_date_to_int(self.ptr(), &raw mut error) };
        (error == 0).then_some(ts)
    }

    /// `timelib_get_current_offset`.
    pub(crate) fn current_offset(&self, _: &Lent<'_>) -> i64 {
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
    pub(crate) fn diff(&self, _: &Lent<'_>, other: &Time) -> RelTime {
        // SAFETY: two live times; the relative time answered is ours.
        RelTime::adopt(unsafe { sys::timelib_diff(self.ptr(), other.ptr()) })
    }

    fn arith(
        &self,
        _: &Lent<'_>,
        rel: &RelFields,
        f: unsafe extern "C" fn(*mut TimeFields, *mut RelFields) -> *mut TimeFields,
    ) -> Time {
        let mut rel = *rel;
        // SAFETY: a live time and a local copy of the interval; the time
        // answered is a clone of ours, sharing our zone.
        let raw = unsafe { f(self.ptr(), &raw mut rel) };
        let known = self.zone.as_ref();
        let mut out = Time {
            raw: NonNull::new(raw).unwrap_or_else(|| std::process::abort()),
            zone: self.zone.clone(),
        };
        out.sync(known);
        out
    }

    /// `timelib_add`.
    pub(crate) fn add(&self, lent: &Lent<'_>, rel: &RelFields) -> Time {
        self.arith(lent, rel, sys::timelib_add)
    }

    /// `timelib_add_wall`.
    pub(crate) fn add_wall(&self, lent: &Lent<'_>, rel: &RelFields) -> Time {
        self.arith(lent, rel, sys::timelib_add_wall)
    }

    /// `timelib_sub`.
    pub(crate) fn sub(&self, lent: &Lent<'_>, rel: &RelFields) -> Time {
        self.arith(lent, rel, sys::timelib_sub)
    }

    /// `timelib_sub_wall`.
    pub(crate) fn sub_wall(&self, lent: &Lent<'_>, rel: &RelFields) -> Time {
        self.arith(lent, rel, sys::timelib_sub_wall)
    }

    /// `timelib_astro_rise_set_altitude`: the status, the rise and set
    /// hours and the rise, set and transit timestamps.
    pub(crate) fn rise_set(
        &mut self,
        _: &Lent<'_>,
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
    /// zone it names read out of the request's zones.
    pub(crate) fn strtotime(lent: &mut Lent<'_>, s: &[u8]) -> Parsed {
        let text = terminated(s);
        let mut errors: *mut sys::ErrorContainer = ptr::null_mut();
        let mut lookup = Lookup {
            lent,
            answered: Answered::default(),
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
        Parsed {
            time: Time::adopt(raw, None, &lookup.answered),
            errors: ErrorContainer::adopt(errors),
        }
    }

    /// `timelib_parse_from_format(format, s, len, &errors, ...)`.
    pub(crate) fn parse_from_format(lent: &mut Lent<'_>, format: &[u8], s: &[u8]) -> Parsed {
        let format = c_string(format);
        let text = terminated(s);
        let mut errors: *mut sys::ErrorContainer = ptr::null_mut();
        let mut lookup = Lookup {
            lent,
            answered: Answered::default(),
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
        Parsed {
            time: Time::adopt(raw, None, &lookup.answered),
            errors: ErrorContainer::adopt(errors),
        }
    }
}

/// What `timezone_initialize` reads off the `timelib_time` it hands
/// `timelib_parse_zone`: the offset, whether no zone was found, how many
/// bytes were read, and the zone the dummy came to carry.
pub(crate) struct ZoneRead {
    pub offset: i64,
    pub dst: i32,
    pub not_found: bool,
    pub read: usize,
    pub zone_type: u32,
    /// The abbreviation an abbreviation zone keeps; empty for another.
    pub abbr: Vec<u8>,
    pub zone: Option<Arc<Zone>>,
}

/// `timelib_parse_zone(&ptr, &dst, &dummy_t, &not_found, ...)` over
/// `s`, into a `timelib_time` on the stack as php's `dummy_t = {0}` is,
/// whose `tz_abbr` copy is freed after, `timelib_free(dummy_t.tz_abbr)`.
pub(crate) fn parse_zone(lent: &mut Lent<'_>, s: &[u8]) -> ZoneRead {
    with_c_string(s, |text| parse_zone_in(lent, text))
}

/// [`parse_zone`] over the terminated text.
fn parse_zone_in(lent: &mut Lent<'_>, text: &CStr) -> ZoneRead {
    let start = text.as_ptr();
    let mut at = start;
    let (mut dst, mut not_found): (c_int, c_int) = (0, 0);
    // SAFETY: every field of `timelib_time` is a number or a pointer, for
    // which zero is a value, the `{0}` php initialises its dummy with.
    let mut dummy: TimeFields = unsafe { std::mem::zeroed() };
    let mut lookup = Lookup {
        lent,
        answered: Answered::default(),
    };
    let tzdb = lookup.as_tzdb();
    // SAFETY: a NUL-terminated string whose cursor is a local, two
    // locals for the flags, the dummy time on this stack, and the lookup
    // with the wrapper that reads it, borrowed for the call alone.
    let offset = unsafe {
        sys::timelib_parse_zone(
            &raw mut at,
            &raw mut dst,
            &raw mut dummy,
            &raw mut not_found,
            tzdb,
            cached_zone,
        )
    };
    let tz_info = dummy.tz_info();
    let zone = lookup
        .answered
        .iter()
        .find(|zone| !tz_info.is_null() && zone.ptr() == tz_info)
        .cloned();
    // Only an abbreviation zone keeps its abbreviation; an identifier's
    // copy is the transient php frees unread.
    let abbr = if dummy.zone_type == ZONETYPE_ABBR {
        bytes_of(dummy.tz_abbr()).to_vec()
    } else {
        Vec::new()
    };
    let abbr_copy = dummy.tz_abbr();
    if !abbr_copy.is_null() {
        // SAFETY: the copy `timelib_parse_zone` made with `timelib_strdup`
        // and the dummy's alone, which is dropped with this frame.
        unsafe { sys::timelib_free(abbr_copy.cast_mut().cast()) };
    }
    ZoneRead {
        offset,
        dst,
        not_found: not_found != 0,
        read: (at as usize).saturating_sub(start as usize),
        zone_type: dummy.zone_type,
        abbr,
        zone,
    }
}

/// An owned `timelib_rel_time`, made by timelib on the request's heap
/// where php's timelib makes the one a `DateInterval`'s `diff` or a
/// `DatePeriod`'s `interval` points at, and freed by its drop.
pub(crate) struct RelTime(NonNull<RelFields>);

// SAFETY: the relative time is owned here alone and points at nothing.
unsafe impl Send for RelTime {}

impl Drop for RelTime {
    fn drop(&mut self) {
        // SAFETY: the relative time timelib made, and this its one owner.
        unsafe { sys::timelib_rel_time_dtor(self.0.as_ptr()) };
    }
}

impl std::ops::Deref for RelTime {
    type Target = RelFields;
    fn deref(&self) -> &RelFields {
        // SAFETY: a live relative time owned by `self`.
        unsafe { self.0.as_ref() }
    }
}

impl std::ops::DerefMut for RelTime {
    fn deref_mut(&mut self) -> &mut RelFields {
        // SAFETY: a live relative time owned by `self`, borrowed uniquely.
        unsafe { self.0.as_mut() }
    }
}

impl RelTime {
    /// A relative time timelib allocated; a null answer is an allocation
    /// failing, which the process does not outlive.
    fn adopt(raw: *mut RelFields) -> RelTime {
        RelTime(NonNull::new(raw).unwrap_or_else(|| std::process::abort()))
    }

    /// `timelib_rel_time_ctor()`: every field zero.
    pub(crate) fn new(_: &Lent<'_>) -> RelTime {
        // SAFETY: the call takes nothing; the relative time answered is ours.
        RelTime::adopt(unsafe { sys::timelib_rel_time_ctor() })
    }

    /// `timelib_rel_time_clone(rt)`: a copy of `fields`, a relative time's
    /// or a parse's `relative`.
    pub(crate) fn of(_: &Lent<'_>, fields: &RelFields) -> RelTime {
        let rt = ptr::from_ref(fields).cast_mut();
        // SAFETY: the clone only reads the fields it copies; the relative
        // time answered is ours.
        RelTime::adopt(unsafe { sys::timelib_rel_time_clone(rt) })
    }
}

/// What `timelib_strtointerval` makes of an ISO 8601 interval.
pub(crate) struct IsoInterval {
    pub begin: Option<Time>,
    pub end: Option<Time>,
    pub period: Option<RelTime>,
    pub recurrences: i32,
    pub errors: Errors,
}

/// `timelib_strtointerval(s, len, &b, &e, &p, &r, &errors)`: the
/// container's messages copied and the container freed, as
/// `date_interval_initialize` frees it.
pub(super) fn strtointerval(_: &Lent<'_>, s: &[u8]) -> IsoInterval {
    let text = terminated(s);
    let mut begin: *mut TimeFields = ptr::null_mut();
    let mut end: *mut TimeFields = ptr::null_mut();
    let mut period: *mut RelFields = ptr::null_mut();
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
        begin: (!begin.is_null()).then(|| Time::adopt(begin, None, &Answered::default())),
        end: (!end.is_null()).then(|| Time::adopt(end, None, &Answered::default())),
        period: NonNull::new(period).map(RelTime),
        recurrences,
        errors: ErrorContainer::adopt(errors)
            .map(|container| container.messages())
            .unwrap_or_default(),
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
