//! `uloc.h` and `ures_openAvailableLocales`: the C API
//! `locale/locale_methods.cpp` calls.

use std::ffi::{CStr, c_char};

use super::enumeration::{Enumeration, UEnumeration};
use super::{Code, fill_chars, fill_utf16, length};

/// A `uloc_get*` that writes a part of a locale ID into a `char` buffer.
type PartFn = unsafe extern "C" fn(*const c_char, *mut c_char, i32, &mut Code) -> i32;
/// A `uloc_getDisplay*` that writes a name into a `UChar` buffer.
type DisplayFn =
    unsafe extern "C" fn(*const c_char, *const c_char, *mut u16, i32, &mut Code) -> i32;

mod sys {
    use std::ffi::c_char;

    use super::{Code, UEnumeration};

    icu! {
        fn uloc_getLanguage(locale: *const c_char, buf: *mut c_char, cap: i32, status: &mut Code) -> i32;
        fn uloc_getScript(locale: *const c_char, buf: *mut c_char, cap: i32, status: &mut Code) -> i32;
        fn uloc_getCountry(locale: *const c_char, buf: *mut c_char, cap: i32, status: &mut Code) -> i32;
        fn uloc_getVariant(locale: *const c_char, buf: *mut c_char, cap: i32, status: &mut Code) -> i32;
        fn uloc_addLikelySubtags(locale: *const c_char, buf: *mut c_char, cap: i32, status: &mut Code) -> i32;
        fn uloc_minimizeSubtags(locale: *const c_char, buf: *mut c_char, cap: i32, status: &mut Code) -> i32;
        fn uloc_getDisplayLanguage(locale: *const c_char, display: *const c_char, buf: *mut u16, cap: i32, status: &mut Code) -> i32;
        fn uloc_getDisplayScript(locale: *const c_char, display: *const c_char, buf: *mut u16, cap: i32, status: &mut Code) -> i32;
        fn uloc_getDisplayCountry(locale: *const c_char, display: *const c_char, buf: *mut u16, cap: i32, status: &mut Code) -> i32;
        fn uloc_getDisplayVariant(locale: *const c_char, display: *const c_char, buf: *mut u16, cap: i32, status: &mut Code) -> i32;
        fn uloc_getDisplayName(locale: *const c_char, display: *const c_char, buf: *mut u16, cap: i32, status: &mut Code) -> i32;
        fn uloc_openKeywords(locale: *const c_char, status: &mut Code) -> *mut UEnumeration;
        fn uloc_getKeywordValue(locale: *const c_char, keyword: *const c_char, buf: *mut c_char, cap: i32, status: &mut Code) -> i32;
        fn uloc_isRightToLeft(locale: *const c_char) -> i8;
        fn uloc_acceptLanguageFromHTTP(
            buf: *mut c_char,
            cap: i32,
            outcome: *mut i32,
            header: *const c_char,
            available: *mut UEnumeration,
            status: &mut Code,
        ) -> i32;
        fn ures_openAvailableLocales(package: *const c_char, status: &mut Code) -> *mut UEnumeration;
    }
}

/// The part of a locale ID a `uloc_get*` call answers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    Language,
    Script,
    Country,
    Variant,
    Canonical,
    Likely,
    Minimal,
}

/// `uloc_getLanguage` and its siblings, grown to fit by ICU's preflight
/// as `get_icu_value_internal` loops: the bytes and the last code.
pub(crate) fn part(part: Part, locale: &CStr) -> (Vec<u8>, Code) {
    let call: PartFn = match part {
        Part::Language => sys::uloc_getLanguage,
        Part::Script => sys::uloc_getScript,
        Part::Country => sys::uloc_getCountry,
        Part::Variant => sys::uloc_getVariant,
        Part::Canonical => super::sys::uloc_canonicalize,
        Part::Likely => sys::uloc_addLikelySubtags,
        Part::Minimal => sys::uloc_minimizeSubtags,
    };
    // SAFETY: the locale is NUL-terminated, the buffer holds capacity bytes.
    fill_chars(|buf, capacity, status| unsafe { call(locale.as_ptr(), buf, capacity, status) })
}

/// `uloc_addLikelySubtags` or `uloc_minimizeSubtags` into php's fixed
/// `ULOC_FULLNAME_CAPACITY` buffer, which overflows rather than grows:
/// the bytes, the length ICU answered and the code.
pub(crate) fn fixed(part: Part, locale: &CStr) -> (Vec<u8>, i32, Code) {
    let call: PartFn = if part == Part::Likely {
        sys::uloc_addLikelySubtags
    } else {
        sys::uloc_minimizeSubtags
    };
    let mut buf = vec![0u8; super::FULLNAME_CAPACITY];
    let mut status = Code::ZERO;
    let room = length(buf.len()).unwrap_or(0);
    // SAFETY: the locale is NUL-terminated, the buffer holds room bytes.
    let len = unsafe { call(locale.as_ptr(), buf.as_mut_ptr().cast(), room, &mut status) };
    buf.truncate(usize::try_from(len).unwrap_or(0).min(buf.len()));
    (buf, len, status)
}

/// The name a `uloc_getDisplay*` call answers.
#[derive(Clone, Copy)]
pub(crate) enum Shown {
    Language,
    Script,
    Country,
    Variant,
    Name,
}

/// `uloc_getDisplayLanguage` and its siblings, in `display`'s language.
pub(crate) fn display(shown: Shown, locale: &CStr, display: &CStr) -> (Vec<u16>, Code) {
    let call: DisplayFn = match shown {
        Shown::Language => sys::uloc_getDisplayLanguage,
        Shown::Script => sys::uloc_getDisplayScript,
        Shown::Country => sys::uloc_getDisplayCountry,
        Shown::Variant => sys::uloc_getDisplayVariant,
        Shown::Name => sys::uloc_getDisplayName,
    };
    fill_utf16(|buf, capacity, status| {
        // SAFETY: both names are NUL-terminated, the buffer holds capacity UChars.
        unsafe { call(locale.as_ptr(), display.as_ptr(), buf, capacity, status) }
    })
}

/// `uloc_getKeywordValue`, grown to fit.
pub(crate) fn keyword_value(locale: &CStr, keyword: &CStr) -> (Vec<u8>, Code) {
    fill_chars(|buf, capacity, status| {
        // SAFETY: both names are NUL-terminated, the buffer holds capacity bytes.
        unsafe {
            sys::uloc_getKeywordValue(locale.as_ptr(), keyword.as_ptr(), buf, capacity, status)
        }
    })
}

/// `uloc_isRightToLeft`.
pub(crate) fn is_right_to_left(locale: &CStr) -> bool {
    // SAFETY: the locale is NUL-terminated.
    unsafe { sys::uloc_isRightToLeft(locale.as_ptr()) != 0 }
}

/// `uloc_openKeywords`: `None` where the locale has no keywords.
pub(crate) fn keywords(locale: &CStr, status: &mut Code) -> Option<Enumeration> {
    // SAFETY: the locale is NUL-terminated.
    Enumeration::from_raw(unsafe { sys::uloc_openKeywords(locale.as_ptr(), status) })
}

/// `ures_openAvailableLocales(NULL)`, or of a bundle's package.
pub(crate) fn available(package: Option<&CStr>, status: &mut Code) -> Option<Enumeration> {
    let package = package.map_or(std::ptr::null(), CStr::as_ptr);
    // SAFETY: the package is null or NUL-terminated.
    Enumeration::from_raw(unsafe { sys::ures_openAvailableLocales(package, status) })
}

/// `uloc_acceptLanguageFromHTTP` over `available` into a buffer of
/// `capacity`: the bytes, the length ICU answered, whether it found one
/// (`UAcceptResult` other than `ULOC_ACCEPT_FAILED`) and the code.
pub(crate) fn accept(
    available: &mut Enumeration,
    header: &CStr,
    capacity: usize,
) -> (Vec<u8>, i32, bool, Code) {
    let mut buf = vec![0u8; capacity + 1];
    let mut outcome = 0;
    let mut status = Code::ZERO;
    let room = length(capacity).unwrap_or(0);
    // SAFETY: the buffer holds room bytes and one more, the header is
    // NUL-terminated, the enumeration is open.
    let len = unsafe {
        sys::uloc_acceptLanguageFromHTTP(
            buf.as_mut_ptr().cast(),
            room,
            &raw mut outcome,
            header.as_ptr(),
            available.raw(),
            &mut status,
        )
    };
    buf.truncate(usize::try_from(len).unwrap_or(0).min(capacity));
    (buf, len, outcome != 0, status)
}
