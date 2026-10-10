//! `udat.h`: the date format php's `IntlDateFormatter` holds, one
//! `UDateFormat` per object, each C call the unsafe line and no more;
//! and `shim/dateformat.cpp`, where php's C++ reads the same format as
//! an `icu::DateFormat`. The calendar `udat_getCalendar` answers is a
//! [`Calendar`], as ICU's `UCalendar` is. php's `IntlDatePatternGenerator`
//! holds an `icu::DateTimePatternGenerator`, [`Dtpg`] here.

use std::ptr::NonNull;

use super::calendar::{Cal, Calendar};
use super::cpp::{CName, Text, UStr, opaque, owner};
use super::timezone::Zone;
use super::{Code, length, static_str};

opaque!(
    /// `UDateFormat`, an `icu::DateFormat`.
    UDateFormat,
    /// `icu::DateTimePatternGenerator`.
    DateTimePatternGenerator
);

owner!(
    /// A `DateTimePatternGenerator` of php's own.
    Dtpg(DateTimePatternGenerator),
    php_intl_dtpg_delete
);

owner!(
    /// One `UDateFormat`, from `udat_open` to `udat_close`.
    DateFormat(UDateFormat),
    udat_close
);

use sys::udat_close;

mod sys {
    use std::ffi::c_char;
    use std::ptr::NonNull;

    use super::super::Code;
    use super::super::calendar::{Cal, Calendar};
    use super::{DateFormat, UDateFormat};

    icu! {
        fn udat_open(
            time_style: i32,
            date_style: i32,
            locale: *const c_char,
            zone: *const u16,
            zone_len: i32,
            pattern: *const u16,
            pattern_len: i32,
            status: &mut Code,
        ) -> Option<DateFormat>;
        fn udat_close(fmt: NonNull<UDateFormat>);
        fn udat_clone(fmt: &UDateFormat, status: &mut Code) -> Option<DateFormat>;
        fn udat_format(fmt: &UDateFormat, date: f64, out: *mut u16, cap: i32, pos: *mut u8, status: &mut Code) -> i32;
        fn udat_parse(fmt: &UDateFormat, text: *const u16, len: i32, pos: Option<&mut i32>, status: &mut Code) -> f64;
        fn udat_parseCalendar(fmt: &UDateFormat, cal: NonNull<Calendar>, text: *const u16, len: i32, pos: Option<&mut i32>, status: &mut Code);
        fn udat_getCalendar(fmt: &UDateFormat) -> Option<NonNull<Calendar>>;
        fn udat_toPattern(fmt: &UDateFormat, localized: i8, out: *mut u16, cap: i32, status: &mut Code) -> i32;
        fn udat_applyPattern(fmt: &mut UDateFormat, localized: i8, pattern: *const u16, len: i32);
        fn udat_getLocaleByType(fmt: &UDateFormat, kind: i32, status: &mut Code) -> *const c_char;
        fn udat_isLenient(fmt: &UDateFormat) -> i8;
        fn udat_setLenient(fmt: &mut UDateFormat, lenient: i8);
        fn ucal_clone(cal: NonNull<Calendar>, status: &mut Code) -> Option<Cal>;
        fn ucal_setDateTime(cal: &mut Calendar, y: i32, m: i32, d: i32, h: i32, i: i32, s: i32, status: &mut Code);
        fn ucal_getMillis(cal: *const Calendar, status: &mut Code) -> f64;
        fn ucal_get(cal: NonNull<Calendar>, field: i32, status: &mut Code) -> i32;
        fn ucal_inDaylightTime(cal: NonNull<Calendar>, status: &mut Code) -> i8;
    }
}

unsafe extern "C" {
    pub(crate) safe fn php_intl_datefmt_calendar_new(
        traditional: i32,
        locale: CName,
        status: &mut Code,
    ) -> Option<Cal>;
    pub(crate) safe fn php_intl_datefmt_adopt_calendar(df: &mut UDateFormat, cal: Cal);
    pub(crate) safe fn php_intl_datefmt_adopt_zone(df: &mut UDateFormat, zone: Zone);
    pub(crate) safe fn php_intl_datefmt_zone(df: &UDateFormat) -> Option<Zone>;
    pub(crate) safe fn php_intl_datefmt_calendar(df: &UDateFormat) -> Option<Cal>;
    pub(crate) safe fn php_intl_datefmt_format_object(
        cal: Option<Cal>,
        date: f64,
        zone: Zone,
        pattern: Option<CName>,
        date_style: i32,
        time_style: i32,
        locale: CName,
        stage: &mut i32,
        status: &mut Code,
    ) -> Option<UStr>;
    pub(crate) safe fn php_intl_dtpg_create(locale: CName, status: &mut Code) -> Option<Dtpg>;
    pub(crate) safe fn php_intl_dtpg_clone(dtpg: &DateTimePatternGenerator) -> Option<Dtpg>;
    fn php_intl_dtpg_delete(dtpg: NonNull<DateTimePatternGenerator>);
    pub(crate) safe fn php_intl_dtpg_best_pattern(
        dtpg: &mut DateTimePatternGenerator,
        skeleton: Text,
        stage: &mut i32,
        status: &mut Code,
    ) -> Option<UStr>;
}

/// A text of `len` units, or none and zero.
fn text(units: Option<&[u16]>) -> (*const u16, i32) {
    units.map_or((std::ptr::null(), 0), |u| {
        (u.as_ptr(), length(u.len()).unwrap_or(0))
    })
}

/// `udat_open` in the styles and locale, with `pattern` where given;
/// none beside a failure ([`Code::built`]).
pub(crate) fn open(
    time_style: i32,
    date_style: i32,
    locale: &std::ffi::CStr,
    pattern: Option<&[u16]>,
    status: &mut Code,
) -> Option<DateFormat> {
    let (pattern, len) = text(pattern);
    // SAFETY: the locale is NUL-terminated, the pattern `len` UChars or
    // null with zero, no zone id.
    let made = unsafe {
        sys::udat_open(
            time_style,
            date_style,
            locale.as_ptr(),
            std::ptr::null(),
            0,
            pattern,
            len,
            status,
        )
    };
    status.built(made)
}

impl DateFormat {
    /// `udat_clone`, none beside a failure ([`Code::built`]).
    pub(crate) fn try_clone(&self, status: &mut Code) -> Option<DateFormat> {
        // SAFETY: the format is open.
        let made = unsafe { sys::udat_clone(self, status) };
        status.built(made)
    }

    /// `udat_format` into `out`, the full length answered.
    pub(crate) fn format(&self, date: f64, out: &mut [u16], status: &mut Code) -> i32 {
        let (at, cap) = (out.as_mut_ptr(), length(out.len()).unwrap_or(0));
        // SAFETY: out holds cap UChars; no field position.
        unsafe { sys::udat_format(self, date, at, cap, std::ptr::null_mut(), status) }
    }

    /// The calendar the format owns, as `udat_getCalendar` hands it out.
    fn own_calendar(&self) -> Option<NonNull<Calendar>> {
        // SAFETY: the format is open; the calendar lives as long as it.
        unsafe { sys::udat_getCalendar(self) }
    }

    /// `udat_parse`, or with `own` `udat_parseCalendar` into the format's
    /// own calendar, whose instant [`DateFormat::millis`] then reads.
    pub(crate) fn parse(
        &mut self,
        units: &[u16],
        pos: Option<&mut i32>,
        own: bool,
        status: &mut Code,
    ) -> f64 {
        let (t, len) = text(Some(units));
        if !own {
            // SAFETY: the text is len UChars, pos a live int or none.
            return unsafe { sys::udat_parse(self, t, len, pos, status) };
        }
        let Some(cal) = self.own_calendar() else {
            *status = Code::ILLEGAL_ARGUMENT;
            return 0.0;
        };
        // SAFETY: as above; the calendar is the format's, which the
        // unique borrow of `self` lets ICU write.
        unsafe { sys::udat_parseCalendar(self, cal, t, len, pos, status) };
        0.0
    }

    /// `ucal_getMillis` of the format's own calendar.
    pub(crate) fn millis(&self, status: &mut Code) -> f64 {
        let Some(cal) = self.own_calendar() else {
            *status = Code::ILLEGAL_ARGUMENT;
            return 0.0;
        };
        // SAFETY: the format's live calendar.
        unsafe { sys::ucal_getMillis(cal.as_ptr(), status) }
    }

    /// `ucal_get` (a field) or, for `None`, `ucal_inDaylightTime` on the
    /// format's own calendar.
    pub(crate) fn calendar_field(&self, field: Option<i32>, status: &mut Code) -> i32 {
        let Some(cal) = self.own_calendar() else {
            *status = Code::ILLEGAL_ARGUMENT;
            return 0;
        };
        match field {
            // SAFETY: the format's live calendar; ICU checks the field.
            Some(f) => unsafe { sys::ucal_get(cal, f, status) },
            // SAFETY: as above.
            None => i32::from(unsafe { sys::ucal_inDaylightTime(cal, status) }),
        }
    }

    /// `internal_get_timestamp`'s calendar: a clone of the format's own
    /// set to the date and time, its milliseconds; none where the clone
    /// answers other than `U_ZERO_ERROR`.
    pub(crate) fn timestamp(&self, f: [i32; 6], status: &mut Code) -> Option<f64> {
        let own = self.own_calendar()?;
        // SAFETY: the format's live calendar.
        let mut cal = unsafe { sys::ucal_clone(own, status) }?;
        if *status != Code::ZERO {
            return None;
        }
        // SAFETY: a calendar of this call's own.
        unsafe { sys::ucal_setDateTime(&mut cal, f[0], f[1], f[2], f[3], f[4], f[5], status) };
        // SAFETY: as above.
        Some(unsafe { sys::ucal_getMillis(&raw const *cal, status) })
    }

    /// `udat_toPattern`, unlocalized, into `out`, the full length answered.
    pub(crate) fn pattern(&self, out: &mut [u16], status: &mut Code) -> i32 {
        let (at, cap) = (out.as_mut_ptr(), length(out.len()).unwrap_or(0));
        // SAFETY: out holds cap UChars.
        unsafe { sys::udat_toPattern(self, 0, at, cap, status) }
    }

    /// `udat_applyPattern`.
    pub(crate) fn apply_pattern(&mut self, localized: bool, units: &[u16]) {
        let (t, len) = text(Some(units));
        // SAFETY: the text is len UChars.
        unsafe { sys::udat_applyPattern(self, i8::from(localized), t, len) };
    }

    /// `udat_getLocaleByType`.
    pub(crate) fn locale(&self, kind: i32, status: &mut Code) -> Option<&'static str> {
        // SAFETY: the format is open; ICU answers a string it keeps.
        static_str(unsafe { sys::udat_getLocaleByType(self, kind, status) })
    }

    /// `udat_isLenient`.
    pub(crate) fn is_lenient(&self) -> bool {
        // SAFETY: the format is open.
        unsafe { sys::udat_isLenient(self) != 0 }
    }

    /// `udat_setLenient`.
    pub(crate) fn set_lenient(&mut self, lenient: bool) {
        // SAFETY: the format is open.
        unsafe { sys::udat_setLenient(self, i8::from(lenient)) };
    }
}
