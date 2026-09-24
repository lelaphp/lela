//! timelib's C interface as `lib/timelib.h` and `lib/timelib_private.h`
//! declare it at the tag: the structs this crate reads laid out as C lays
//! them out, and the functions it calls. Nothing here is called outside
//! [`super`]; the arithmetic that touches no memory is declared `safe`.
#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_double, c_float, c_int, c_uint};

/// `timelib_rel_time`: a relative time, the body of a `DateInterval`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelTime {
    pub y: i64,
    pub m: i64,
    pub d: i64,
    pub h: i64,
    pub i: i64,
    pub s: i64,
    pub us: i64,
    pub weekday: c_int,
    pub weekday_behavior: c_int,
    pub first_last_day_of: c_int,
    pub invert: c_int,
    pub days: i64,
    pub special_type: c_uint,
    pub special_amount: i64,
    pub have_weekday_relative: c_uint,
    pub have_special_relative: c_uint,
}

/// `timelib_time`. The two pointers are timelib's to manage and this
/// binding's to read, so they are private to it; every other field is a
/// number any code may read and write.
#[repr(C)]
#[derive(Debug)]
pub struct TimeFields {
    pub y: i64,
    pub m: i64,
    pub d: i64,
    pub h: i64,
    pub i: i64,
    pub s: i64,
    pub us: i64,
    pub z: c_int,
    pub(super) tz_abbr: *mut c_char,
    pub(super) tz_info: *mut TzInfo,
    pub dst: c_int,
    pub relative: RelTime,
    pub sse: i64,
    pub have_time: c_uint,
    pub have_date: c_uint,
    pub have_zone: c_uint,
    pub have_relative: c_uint,
    pub have_weeknr_day: c_uint,
    pub sse_uptodate: c_uint,
    pub tim_uptodate: c_uint,
    pub is_localtime: c_uint,
    pub zone_type: c_uint,
}

#[repr(C)]
pub struct Bit32 {
    pub ttisgmtcnt: u32,
    pub ttisstdcnt: u32,
    pub leapcnt: u32,
    pub timecnt: u32,
    pub typecnt: u32,
    pub charcnt: u32,
}

#[repr(C)]
pub struct Bit64 {
    pub ttisgmtcnt: u64,
    pub ttisstdcnt: u64,
    pub leapcnt: u64,
    pub timecnt: u64,
    pub typecnt: u64,
    pub charcnt: u64,
}

/// `ttinfo`, one local time type of a zone.
#[repr(C)]
pub struct TtInfo {
    pub offset: i32,
    pub isdst: c_int,
    pub abbr_idx: c_uint,
    pub isstdcnt: c_uint,
    pub isgmtcnt: c_uint,
}

/// `tlocinfo`.
#[repr(C)]
pub struct LocInfo {
    pub country_code: [c_char; 3],
    pub latitude: c_double,
    pub longitude: c_double,
    pub comments: *mut c_char,
}

/// `timelib_tzinfo`, a parsed zone.
#[repr(C)]
pub struct TzInfo {
    pub name: *mut c_char,
    pub bit32: Bit32,
    pub bit64: Bit64,
    pub trans: *mut i64,
    pub trans_idx: *mut u8,
    pub types: *mut TtInfo,
    pub timezone_abbr: *mut c_char,
    pub leap_times: *mut u8,
    pub bc: u8,
    pub location: LocInfo,
    pub posix_string: *mut c_char,
    pub posix_info: *mut PosixStr,
}

/// `timelib_posix_str`, of which the binding reads whether it names a
/// daylight-saving rule.
#[repr(C)]
pub struct PosixStr {
    pub std: *mut c_char,
    pub std_offset: i64,
    pub dst: *mut c_char,
    pub dst_offset: i64,
    pub dst_begin: *mut u8,
    pub dst_end: *mut u8,
    pub type_index_std_type: c_int,
    pub type_index_dst_type: c_int,
}

/// `timelib_time_offset`.
#[repr(C)]
pub struct TimeOffset {
    pub offset: i32,
    pub leap_secs: c_uint,
    pub is_dst: c_uint,
    pub abbr: *mut c_char,
    pub changed_at: i64,
}

/// `timelib_abbr_info`.
#[repr(C)]
pub struct AbbrInfo {
    pub utc_offset: i64,
    pub abbr: *mut c_char,
    pub dst: c_int,
}

/// `timelib_error_message`.
#[repr(C)]
pub struct ErrorMessage {
    pub error_code: c_int,
    pub position: c_int,
    pub character: c_char,
    pub message: *mut c_char,
}

/// `timelib_error_container`.
#[repr(C)]
pub struct ErrorContainer {
    pub error_messages: *mut ErrorMessage,
    pub warning_messages: *mut ErrorMessage,
    pub error_count: c_int,
    pub warning_count: c_int,
}

/// `timelib_tz_lookup_table`.
#[repr(C)]
pub struct LookupTable {
    pub name: *const c_char,
    pub kind: c_int,
    pub gmtoffset: c_float,
    pub full_tz_name: *const c_char,
}

/// `timelib_tzdb_index_entry`.
#[repr(C)]
pub struct IndexEntry {
    pub id: *mut c_char,
    pub pos: c_uint,
}

/// `timelib_tzdb`.
#[repr(C)]
pub struct Tzdb {
    pub version: *const c_char,
    pub index_size: c_int,
    pub index: *const IndexEntry,
    pub data: *const u8,
}

/// `timelib_posix_transitions`.
#[repr(C)]
pub struct PosixTransitions {
    pub count: usize,
    pub times: [i64; 6],
    pub types: [i64; 6],
}

/// `timelib_tz_get_wrapper`.
pub type TzGetWrapper = unsafe extern "C" fn(*const c_char, *const Tzdb, *mut c_int) -> *mut TzInfo;

unsafe extern "C" {
    pub safe fn timelib_day_of_week(y: i64, m: i64, d: i64) -> i64;
    pub safe fn timelib_iso_day_of_week(y: i64, m: i64, d: i64) -> i64;
    pub safe fn timelib_day_of_year(y: i64, m: i64, d: i64) -> i64;
    pub safe fn timelib_daynr_from_weeknr(iy: i64, iw: i64, id: i64) -> i64;
    pub safe fn timelib_days_in_month(y: i64, m: i64) -> i64;
    pub safe fn timelib_valid_date(y: i64, m: i64, d: i64) -> c_int;
    pub fn timelib_isoweek_from_date(y: i64, m: i64, d: i64, iw: *mut i64, iy: *mut i64);
    pub fn timelib_date_from_isodate(
        iy: i64,
        iw: i64,
        id: i64,
        y: *mut i64,
        m: *mut i64,
        d: *mut i64,
    );

    pub fn timelib_strtotime(
        s: *const c_char,
        len: usize,
        errors: *mut *mut ErrorContainer,
        tzdb: *const Tzdb,
        wrapper: TzGetWrapper,
    ) -> *mut TimeFields;
    pub fn timelib_parse_from_format(
        format: *const c_char,
        s: *const c_char,
        len: usize,
        errors: *mut *mut ErrorContainer,
        tzdb: *const Tzdb,
        wrapper: TzGetWrapper,
    ) -> *mut TimeFields;
    pub fn timelib_fill_holes(parsed: *mut TimeFields, now: *mut TimeFields, options: c_int);
    pub fn timelib_timezone_id_from_abbr(
        abbr: *const c_char,
        gmtoffset: i64,
        isdst: c_int,
    ) -> *const c_char;
    pub fn timelib_timezone_abbreviations_list() -> *const LookupTable;
    pub fn timelib_parse_zone(
        ptr: *mut *const c_char,
        dst: *mut c_int,
        t: *mut TimeFields,
        tz_not_found: *mut c_int,
        tzdb: *const Tzdb,
        wrapper: TzGetWrapper,
    ) -> i64;
    pub fn timelib_strtointerval(
        s: *const c_char,
        len: usize,
        begin: *mut *mut TimeFields,
        end: *mut *mut TimeFields,
        period: *mut *mut RelTime,
        recurrences: *mut c_int,
        errors: *mut *mut ErrorContainer,
    );

    pub fn timelib_update_ts(time: *mut TimeFields, tzi: *mut TzInfo);
    pub fn timelib_unixtime2gmt(tm: *mut TimeFields, ts: i64);
    pub fn timelib_unixtime2local(tm: *mut TimeFields, ts: i64);
    pub fn timelib_update_from_sse(tm: *mut TimeFields);
    pub fn timelib_set_timezone_from_offset(t: *mut TimeFields, utc_offset: i64);
    pub fn timelib_set_timezone_from_abbr(t: *mut TimeFields, abbr_info: AbbrInfo);
    pub fn timelib_set_timezone(t: *mut TimeFields, tz: *mut TzInfo);

    pub fn timelib_timezone_id_is_valid(timezone: *const c_char, tzdb: *const Tzdb) -> c_int;
    pub fn timelib_parse_tzfile(
        timezone: *const c_char,
        tzdb: *const Tzdb,
        error_code: *mut c_int,
    ) -> *mut TzInfo;
    pub fn timelib_tzinfo_dtor(tz: *mut TzInfo);
    pub fn timelib_get_time_zone_info(ts: i64, tz: *mut TzInfo) -> *mut TimeOffset;
    pub fn timelib_get_current_offset(t: *mut TimeFields) -> i64;
    #[link_name = "timelib_builtin_db"]
    pub fn timelib_bundled_db() -> *const Tzdb;
    pub fn timelib_timezone_identifiers_list(
        tzdb: *const Tzdb,
        count: *mut c_int,
    ) -> *const IndexEntry;
    pub fn timelib_get_transitions_for_year(
        tz: *mut TzInfo,
        year: i64,
        transitions: *mut PosixTransitions,
    );

    pub fn timelib_rel_time_dtor(t: *mut RelTime);
    pub fn timelib_time_ctor() -> *mut TimeFields;
    pub fn timelib_time_dtor(t: *mut TimeFields);
    pub fn timelib_time_clone(orig: *mut TimeFields) -> *mut TimeFields;
    pub fn timelib_time_compare(t1: *mut TimeFields, t2: *mut TimeFields) -> c_int;
    pub fn timelib_time_offset_dtor(t: *mut TimeOffset);
    pub fn timelib_error_container_dtor(errors: *mut ErrorContainer);
    pub fn timelib_date_to_int(d: *mut TimeFields, error: *mut c_int) -> i64;

    pub fn timelib_astro_rise_set_altitude(
        time: *mut TimeFields,
        lon: c_double,
        lat: c_double,
        altit: c_double,
        upper_limb: c_int,
        h_rise: *mut c_double,
        h_set: *mut c_double,
        ts_rise: *mut i64,
        ts_set: *mut i64,
        ts_transit: *mut i64,
    ) -> c_int;
    pub fn timelib_diff(one: *mut TimeFields, two: *mut TimeFields) -> *mut RelTime;
    pub fn timelib_diff_days(one: *mut TimeFields, two: *mut TimeFields) -> c_int;
    pub fn timelib_add(t: *mut TimeFields, interval: *mut RelTime) -> *mut TimeFields;
    pub fn timelib_add_wall(t: *mut TimeFields, interval: *mut RelTime) -> *mut TimeFields;
    pub fn timelib_sub(t: *mut TimeFields, interval: *mut RelTime) -> *mut TimeFields;
    pub fn timelib_sub_wall(t: *mut TimeFields, interval: *mut RelTime) -> *mut TimeFields;
}
