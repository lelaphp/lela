//! `unum.h`: the C number format php's `NumberFormatter` holds, one
//! `UNumberFormat` per object, each call the unsafe line and no more.

use std::ffi::CStr;
use std::ptr::{NonNull, null, null_mut};

use super::{Code, length, static_str};

mod sys {
    use std::ffi::c_char;

    use super::super::Code;
    use super::{ParseError, UNumberFormat};

    icu! {
        fn unum_open(
            style: i32,
            pattern: *const u16,
            pattern_length: i32,
            locale: *const c_char,
            parse_error: *mut ParseError,
            status: &mut Code,
        ) -> *mut UNumberFormat;
        fn unum_close(fmt: *mut UNumberFormat);
        fn unum_clone(fmt: *const UNumberFormat, status: &mut Code) -> *mut UNumberFormat;
        fn unum_format(fmt: *const UNumberFormat, n: i32, out: *mut u16, cap: i32, pos: *mut u8, status: &mut Code) -> i32;
        fn unum_formatInt64(fmt: *const UNumberFormat, n: i64, out: *mut u16, cap: i32, pos: *mut u8, status: &mut Code) -> i32;
        fn unum_formatDouble(fmt: *const UNumberFormat, n: f64, out: *mut u16, cap: i32, pos: *mut u8, status: &mut Code) -> i32;
        fn unum_formatDoubleCurrency(fmt: *const UNumberFormat, n: f64, currency: *const u16, out: *mut u16, cap: i32, pos: *mut u8, status: &mut Code) -> i32;
        fn unum_parse(fmt: *const UNumberFormat, text: *const u16, len: i32, pos: *mut i32, status: &mut Code) -> i32;
        fn unum_parseInt64(fmt: *const UNumberFormat, text: *const u16, len: i32, pos: *mut i32, status: &mut Code) -> i64;
        fn unum_parseDouble(fmt: *const UNumberFormat, text: *const u16, len: i32, pos: *mut i32, status: &mut Code) -> f64;
        fn unum_parseDoubleCurrency(fmt: *const UNumberFormat, text: *const u16, len: i32, pos: *mut i32, currency: *mut u16, status: &mut Code) -> f64;
        fn unum_getAttribute(fmt: *const UNumberFormat, attr: i32) -> i32;
        fn unum_setAttribute(fmt: *mut UNumberFormat, attr: i32, value: i32);
        fn unum_getDoubleAttribute(fmt: *const UNumberFormat, attr: i32) -> f64;
        fn unum_setDoubleAttribute(fmt: *mut UNumberFormat, attr: i32, value: f64);
        fn unum_getTextAttribute(fmt: *const UNumberFormat, attr: i32, out: *mut u16, cap: i32, status: &mut Code) -> i32;
        fn unum_setTextAttribute(fmt: *mut UNumberFormat, attr: i32, text: *const u16, len: i32, status: &mut Code);
        fn unum_getSymbol(fmt: *const UNumberFormat, symbol: i32, out: *mut u16, cap: i32, status: &mut Code) -> i32;
        fn unum_setSymbol(fmt: *mut UNumberFormat, symbol: i32, text: *const u16, len: i32, status: &mut Code);
        fn unum_toPattern(fmt: *const UNumberFormat, localized: i8, out: *mut u16, cap: i32, status: &mut Code) -> i32;
        fn unum_applyPattern(fmt: *mut UNumberFormat, localized: i8, text: *const u16, len: i32, parse_error: *mut ParseError, status: &mut Code);
        fn unum_getLocaleByType(fmt: *const UNumberFormat, kind: i32, status: &mut Code) -> *const c_char;
        fn uloc_getISO3Language(locale: *const c_char) -> *const c_char;
    }
}

/// `UNumberFormat`, opaque.
pub(crate) enum UNumberFormat {}

/// `UParseError`: where a pattern stopped parsing, and the text around.
#[repr(C)]
#[derive(Default)]
pub(crate) struct ParseError {
    pub(crate) line: i32,
    pub(crate) offset: i32,
    pub(crate) pre: [u16; 16],
    pub(crate) post: [u16; 16],
}

/// One number as `unum_format*` takes it.
#[derive(Clone, Copy)]
pub(crate) enum Number {
    Int32(i32),
    Int64(i64),
    Double(f64),
    /// A double in the ISO currency, NUL-terminated UTF-16.
    Currency(f64, *const u16),
}

/// What `unum_parse*` answers.
#[derive(Clone, Copy)]
pub(crate) enum Parsed {
    Int32(i32),
    Int64(i64),
    Double(f64),
}

/// One `UNumberFormat`, from `unum_open` to `unum_close`.
pub(crate) struct NumberFormat(NonNull<UNumberFormat>);

// SAFETY: a format is used by the one request whose object holds it.
unsafe impl Send for NumberFormat {}

/// `uloc_getISO3Language`: empty for a locale ICU knows no language of.
pub(crate) fn iso3_language(locale: &CStr) -> &'static str {
    // SAFETY: the locale is NUL-terminated; ICU answers a static string.
    static_str(unsafe { sys::uloc_getISO3Language(locale.as_ptr()) }).unwrap_or("")
}

impl NumberFormat {
    /// `unum_open`.
    pub(crate) fn open(
        style: i32,
        pattern: Option<&[u16]>,
        locale: &CStr,
        status: &mut Code,
    ) -> Option<NumberFormat> {
        let (text, len) =
            pattern.map_or((null(), 0), |p| (p.as_ptr(), length(p.len()).unwrap_or(0)));
        // SAFETY: the pattern is len UChars or null, the locale NUL-terminated.
        let raw = unsafe { sys::unum_open(style, text, len, locale.as_ptr(), null_mut(), status) };
        NonNull::new(raw).map(NumberFormat)
    }

    /// `unum_clone`.
    pub(crate) fn try_clone(&self, status: &mut Code) -> Option<NumberFormat> {
        // SAFETY: the format is open.
        NonNull::new(unsafe { sys::unum_clone(self.0.as_ptr(), status) }).map(NumberFormat)
    }

    /// `unum_format`, `unum_formatInt64`, `unum_formatDouble` or
    /// `unum_formatDoubleCurrency` into `out`, the full length answered.
    pub(crate) fn format(&self, n: Number, out: &mut [u16], status: &mut Code) -> i32 {
        let (f, cap, at) = (
            self.0.as_ptr(),
            length(out.len()).unwrap_or(0),
            out.as_mut_ptr(),
        );
        match n {
            // SAFETY: out holds cap UChars; no field position.
            Number::Int32(n) => unsafe { sys::unum_format(f, n, at, cap, null_mut(), status) },
            // SAFETY: as above.
            Number::Int64(n) => unsafe { sys::unum_formatInt64(f, n, at, cap, null_mut(), status) },
            // SAFETY: as above.
            Number::Double(n) => unsafe {
                sys::unum_formatDouble(f, n, at, cap, null_mut(), status)
            },
            // SAFETY: as above, the currency NUL-terminated.
            Number::Currency(n, c) => unsafe {
                sys::unum_formatDoubleCurrency(f, n, c, at, cap, null_mut(), status)
            },
        }
    }

    /// `unum_parse*` as `kind` names (1 int32, 2 int64, else double),
    /// from `pos` where given.
    pub(crate) fn parse(
        &self,
        kind: i64,
        text: &[u16],
        pos: Option<&mut i32>,
        status: &mut Code,
    ) -> Parsed {
        let (f, t, len) = (
            self.0.as_ptr(),
            text.as_ptr(),
            length(text.len()).unwrap_or(0),
        );
        let pos = pos.map_or(null_mut(), std::ptr::from_mut);
        match kind {
            // SAFETY: the text is len UChars, pos null or a live int.
            1 => Parsed::Int32(unsafe { sys::unum_parse(f, t, len, pos, status) }),
            // SAFETY: as above.
            2 => Parsed::Int64(unsafe { sys::unum_parseInt64(f, t, len, pos, status) }),
            // SAFETY: as above.
            _ => Parsed::Double(unsafe { sys::unum_parseDouble(f, t, len, pos, status) }),
        }
    }

    /// `unum_parseDoubleCurrency`, the currency into `currency`.
    pub(crate) fn parse_currency(
        &self,
        text: &[u16],
        pos: Option<&mut i32>,
        currency: &mut [u16; 5],
        status: &mut Code,
    ) -> f64 {
        let pos = pos.map_or(null_mut(), std::ptr::from_mut);
        let len = length(text.len()).unwrap_or(0);
        // SAFETY: the text is len UChars, the currency four and a NUL.
        unsafe {
            sys::unum_parseDoubleCurrency(
                self.0.as_ptr(),
                text.as_ptr(),
                len,
                pos,
                currency.as_mut_ptr(),
                status,
            )
        }
    }

    /// `unum_getAttribute`.
    pub(crate) fn attribute(&self, attr: i32) -> i32 {
        // SAFETY: the format is open.
        unsafe { sys::unum_getAttribute(self.0.as_ptr(), attr) }
    }

    /// `unum_setAttribute`.
    pub(crate) fn set_attribute(&mut self, attr: i32, value: i32) {
        // SAFETY: the format is open.
        unsafe { sys::unum_setAttribute(self.0.as_ptr(), attr, value) };
    }

    /// `unum_getDoubleAttribute`.
    pub(crate) fn double_attribute(&self, attr: i32) -> f64 {
        // SAFETY: the format is open.
        unsafe { sys::unum_getDoubleAttribute(self.0.as_ptr(), attr) }
    }

    /// `unum_setDoubleAttribute`.
    pub(crate) fn set_double_attribute(&mut self, attr: i32, value: f64) {
        // SAFETY: the format is open.
        unsafe { sys::unum_setDoubleAttribute(self.0.as_ptr(), attr, value) };
    }

    /// `unum_getTextAttribute` (0), `unum_getSymbol` (1) or
    /// `unum_toPattern` (2) into `out`, the full length answered.
    pub(crate) fn text(&self, which: u8, key: i32, out: &mut [u16], status: &mut Code) -> i32 {
        let (f, cap, at) = (
            self.0.as_ptr(),
            length(out.len()).unwrap_or(0),
            out.as_mut_ptr(),
        );
        match which {
            // SAFETY: out holds cap UChars.
            0 => unsafe { sys::unum_getTextAttribute(f, key, at, cap, status) },
            // SAFETY: as above.
            1 => unsafe { sys::unum_getSymbol(f, key, at, cap, status) },
            // SAFETY: as above.
            _ => unsafe { sys::unum_toPattern(f, 0, at, cap, status) },
        }
    }

    /// `unum_setTextAttribute` (0), `unum_setSymbol` (1) or
    /// `unum_applyPattern` (2, where it stopped into `error`).
    pub(crate) fn set_text(
        &mut self,
        which: u8,
        key: i32,
        text: &[u16],
        error: &mut ParseError,
        status: &mut Code,
    ) {
        let (f, t, len) = (
            self.0.as_ptr(),
            text.as_ptr(),
            length(text.len()).unwrap_or(0),
        );
        match which {
            // SAFETY: the text is len UChars.
            0 => unsafe { sys::unum_setTextAttribute(f, key, t, len, status) },
            // SAFETY: as above.
            1 => unsafe { sys::unum_setSymbol(f, key, t, len, status) },
            // SAFETY: as above, the error a live UParseError.
            _ => unsafe { sys::unum_applyPattern(f, 0, t, len, error, status) },
        }
    }

    /// `unum_getLocaleByType`.
    pub(crate) fn locale(&self, kind: i32, status: &mut Code) -> Option<&'static str> {
        // SAFETY: the format is open; ICU answers a string it keeps.
        static_str(unsafe { sys::unum_getLocaleByType(self.0.as_ptr(), kind, status) })
    }
}

impl Drop for NumberFormat {
    fn drop(&mut self) {
        // SAFETY: the format is open and closed once, here.
        unsafe { sys::unum_close(self.0.as_ptr()) };
    }
}
