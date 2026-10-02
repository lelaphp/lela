//! `UEnumeration` (`uenum.h`): the string enumeration ICU answers for the
//! time zones, a calendar's keyword values and a locale's, which php
//! walks as an `IntlIterator` (`IntlIterator_from_StringEnumeration`).

use std::ffi::c_char;

use super::Code;

mod sys {
    use std::ffi::c_char;

    use super::super::Code;

    /// `UEnumeration`, opaque.
    pub(crate) enum UEnumeration {}

    icu! {
        fn uenum_close(en: *mut UEnumeration);
        fn uenum_next(en: *mut UEnumeration, len: *mut i32, status: &mut Code) -> *const c_char;
        fn uenum_reset(en: *mut UEnumeration, status: &mut Code);
    }
}

pub(crate) use sys::UEnumeration;

/// One `UEnumeration`, from its opening to `uenum_close`.
pub(crate) struct Enumeration(*mut UEnumeration);

// SAFETY: an enumeration is used by the one request that holds it, never
// shared; ICU's enumerations are thread-compatible.
unsafe impl Send for Enumeration {}

impl Enumeration {
    /// The enumeration an ICU call opened, `None` for a null one.
    pub(crate) fn from_raw(raw: *mut UEnumeration) -> Option<Enumeration> {
        (!raw.is_null()).then_some(Enumeration(raw))
    }

    /// `StringEnumeration::next`: the next string, `None` at the end.
    pub(crate) fn next(&mut self, status: &mut Code) -> Option<Vec<u8>> {
        let mut len = 0;
        // SAFETY: the enumeration is open; ICU writes the length.
        let text: *const c_char = unsafe { sys::uenum_next(self.0, &raw mut len, status) };
        if text.is_null() || status.is_failure() {
            return None;
        }
        let len = usize::try_from(len).unwrap_or(0);
        // SAFETY: ICU answers len bytes valid until the next call.
        let bytes = unsafe { std::slice::from_raw_parts(text.cast::<u8>(), len) };
        Some(bytes.to_vec())
    }

    /// `StringEnumeration::reset`.
    pub(crate) fn reset(&mut self, status: &mut Code) {
        // SAFETY: the enumeration is open.
        unsafe { sys::uenum_reset(self.0, status) };
    }
}

impl Drop for Enumeration {
    fn drop(&mut self) {
        // SAFETY: the enumeration is open and closed once, here.
        unsafe { sys::uenum_close(self.0) };
    }
}
