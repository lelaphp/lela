//! `icu::Calendar` through `shim/calendar.cpp`: php's `Calendar_object`
//! holds one by pointer, [`Cal`] here, and `calendar_methods.cpp`'s and
//! `gregoriancalendar_methods.cpp`'s calls are the shim's, each guarding
//! its own fields, days and selectors. A [`Zone`] handed in is adopted.

use std::ffi::c_char;
use std::ptr::NonNull;

use super::Code;
use super::cpp::{CName, opaque, owner};
use super::enumeration::{Enumeration, UEnumeration};
use super::timezone::Zone;

opaque!(
    /// `icu::Calendar`.
    Calendar
);

owner!(
    /// A `Calendar` of php's own.
    Cal(Calendar),
    php_intl_cal_delete
);

/// `ULOC_FULLNAME_CAPACITY`, the buffer a locale's name is written into.
pub(crate) const LOCALE_CAPACITY: usize = 157;

unsafe extern "C" {
    pub(crate) safe fn php_intl_cal_create(
        zone: Zone,
        locale: CName,
        status: &mut Code,
    ) -> Option<Cal>;
    pub(crate) safe fn php_intl_cal_greg_create(
        zone: Option<Zone>,
        locale: CName,
        n: i32,
        fields: &[i32; 6],
        status: &mut Code,
    ) -> Option<Cal>;
    pub(crate) safe fn php_intl_cal_clone(cal: &Calendar) -> Option<Cal>;
    fn php_intl_cal_delete(cal: NonNull<Calendar>);
    pub(crate) safe fn php_intl_cal_is_gregorian(cal: &mut Calendar) -> bool;
    safe fn php_intl_cal_type(cal: &Calendar) -> *const c_char;
    pub(crate) safe fn php_intl_cal_field(
        cal: &mut Calendar,
        which: i32,
        field: i32,
        value: i32,
        when: f64,
        status: &mut Code,
    ) -> i32;
    pub(crate) safe fn php_intl_cal_set_date(cal: &mut Calendar, n: i32, fields: &[i32; 6]);
    pub(crate) safe fn php_intl_cal_int(
        cal: &mut Calendar,
        which: i32,
        value: i32,
        status: &mut Code,
    ) -> i32;
    pub(crate) safe fn php_intl_cal_set_int(
        cal: &mut Calendar,
        which: i32,
        value: i32,
        status: &mut Code,
    );
    pub(crate) safe fn php_intl_cal_dow(
        cal: &Calendar,
        which: i32,
        dow: i32,
        status: &mut Code,
    ) -> i32;
    pub(crate) safe fn php_intl_cal_time(
        cal: &mut Calendar,
        which: i32,
        when: f64,
        status: &mut Code,
    ) -> f64;
    pub(crate) safe fn php_intl_cal_compare(
        a: &Calendar,
        b: &Calendar,
        which: i32,
        status: &mut Code,
    ) -> bool;
    pub(crate) safe fn php_intl_cal_locale(
        cal: &Calendar,
        which: i32,
        out: &mut [u8; LOCALE_CAPACITY],
        status: &mut Code,
    );
    pub(crate) safe fn php_intl_cal_zone(cal: &Calendar) -> Option<Zone>;
    pub(crate) safe fn php_intl_cal_adopt_zone(cal: &mut Calendar, zone: Zone);
    safe fn php_intl_cal_available(i: i32, count: &mut i32) -> *const c_char;
    safe fn php_intl_cal_keyword_values(
        key: CName,
        locale: CName,
        common: bool,
        status: &mut Code,
    ) -> *mut UEnumeration;
}

/// `getType()`.
pub(crate) fn kind(cal: &Calendar) -> &'static str {
    super::static_str(php_intl_cal_type(cal)).unwrap_or("")
}

/// `Calendar::getAvailableLocales`'s names.
pub(crate) fn available_locales() -> Vec<&'static str> {
    let mut count = 0;
    let mut names = Vec::new();
    let mut i = 0;
    while let Some(name) = super::static_str(php_intl_cal_available(i, &mut count)) {
        names.push(name);
        i += 1;
    }
    names
}

/// `Calendar::getKeywordValuesForLocale` as an enumeration, none where
/// ICU answers none.
pub(crate) fn keyword_values(
    key: CName,
    locale: CName,
    common: bool,
    status: &mut Code,
) -> Option<Enumeration> {
    Enumeration::from_raw(php_intl_cal_keyword_values(key, locale, common, status))
}
