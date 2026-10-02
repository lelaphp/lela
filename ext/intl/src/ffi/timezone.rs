//! `icu::TimeZone` through `shim/timezone.cpp`: php's `TimeZone_object`
//! holds one by pointer, [`Zone`] here, and `timezone_methods.cpp`'s
//! calls are the shim's, each guarding its own selectors.

use std::ptr::NonNull;

use super::Code;
use super::cpp::{CName, Text, UStr, opaque, owner};
use super::enumeration::{Enumeration, UEnumeration};

opaque!(
    /// `icu::TimeZone`.
    TimeZone
);

owner!(
    /// A `TimeZone` of php's own (`should_delete`).
    Zone(TimeZone),
    php_intl_tz_delete
);

unsafe extern "C" {
    pub(crate) safe fn php_intl_tz_create(id: Text) -> Option<Zone>;
    pub(crate) safe fn php_intl_tz_create_invariant(id: CName) -> Option<Zone>;
    pub(crate) safe fn php_intl_tz_special(which: i32) -> Option<Zone>;
    pub(crate) safe fn php_intl_tz_is_unknown(tz: &TimeZone) -> bool;
    pub(crate) safe fn php_intl_tz_clone(tz: &TimeZone) -> Option<Zone>;
    fn php_intl_tz_delete(tz: NonNull<TimeZone>);
    pub(crate) safe fn php_intl_tz_equals(a: &TimeZone, b: &TimeZone) -> bool;
    pub(crate) safe fn php_intl_tz_has_same_rules(a: &TimeZone, b: &TimeZone) -> bool;
    pub(crate) safe fn php_intl_tz_id(tz: &TimeZone) -> Option<UStr>;
    pub(crate) safe fn php_intl_tz_int(tz: &TimeZone, which: i32) -> i32;
    pub(crate) safe fn php_intl_tz_offset(
        tz: &TimeZone,
        date: f64,
        local: bool,
        raw: &mut i32,
        dst: &mut i32,
        status: &mut Code,
    );
    pub(crate) safe fn php_intl_tz_display_name(
        tz: &TimeZone,
        dst: bool,
        style: i32,
        locale: CName,
    ) -> Option<UStr>;
    pub(crate) safe fn php_intl_now() -> f64;
    pub(crate) safe fn php_intl_tz_lookup(
        which: i32,
        id: Text,
        n: i32,
        region: Option<CName>,
        flag: &mut bool,
        status: &mut Code,
    ) -> Option<UStr>;
    pub(crate) safe fn php_intl_tz_count_equivalent(id: Text) -> i32;
    pub(crate) safe fn php_intl_tz_region(id: Text, out: &mut [u8; 3], status: &mut Code) -> i32;
    safe fn php_intl_tz_enumeration(
        which: i32,
        name: Option<CName>,
        offset: i32,
        raw: Option<&i32>,
        status: &mut Code,
    ) -> *mut UEnumeration;
}

/// The zones as an enumeration (`php_intl_tz_enumeration`), none where
/// ICU answers none.
pub(crate) fn enumeration(
    which: i32,
    name: Option<CName>,
    offset: i32,
    raw: Option<&i32>,
    status: &mut Code,
) -> Option<Enumeration> {
    Enumeration::from_raw(php_intl_tz_enumeration(which, name, offset, raw, status))
}

unsafe extern "C" {
    safe fn php_intl_tz_data_version(status: &mut Code) -> *const std::ffi::c_char;
}

/// `TimeZone::getTZDataVersion`.
pub(crate) fn data_version(status: &mut Code) -> &'static str {
    super::static_str(php_intl_tz_data_version(status)).unwrap_or("")
}
