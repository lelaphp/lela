//! `ucol.h`: the `UCollator` php's `Collator` holds, closed on drop, and
//! the calls `collator/` makes of it; `u_isspace` for
//! `collator_is_numeric`.

use std::ffi::CStr;
use std::ptr::NonNull;

use super::{Code, length, static_str};

/// The declarations, which `grapheme`'s search reaches its collator by.
pub(super) mod sys {
    use std::ffi::c_char;

    use super::Code;

    /// `UCollator`, opaque.
    pub(crate) enum UCollator {}

    icu! {
        fn ucol_open(locale: *const c_char, status: &mut Code) -> *mut UCollator;
        fn ucol_close(coll: *mut UCollator);
        fn ucol_strcoll(coll: *const UCollator, a: *const u16, a_len: i32, b: *const u16, b_len: i32) -> i32;
        fn ucol_getAttribute(coll: *const UCollator, attr: i32, status: &mut Code) -> i32;
        fn ucol_setAttribute(coll: *mut UCollator, attr: i32, value: i32, status: &mut Code);
        fn ucol_getStrength(coll: *const UCollator) -> i32;
        fn ucol_setStrength(coll: *mut UCollator, strength: i32);
        fn ucol_getLocaleByType(coll: *const UCollator, kind: i32, status: &mut Code) -> *const c_char;
        fn ucol_getSortKey(coll: *const UCollator, src: *const u16, len: i32, key: *mut u8, cap: i32) -> i32;
    }
}

/// `u_isspace`.
pub(crate) fn is_space(c: u16) -> bool {
    super::uchar::u_isspace(i32::from(c)) != 0
}

/// One open `UCollator`.
pub(crate) struct Collator(NonNull<sys::UCollator>);

// SAFETY: a collator is used by the one request whose object holds it,
// never shared; ICU's collators are thread-compatible.
unsafe impl Send for Collator {}

impl Drop for Collator {
    fn drop(&mut self) {
        // SAFETY: the collator is open and closed once, here.
        unsafe { sys::ucol_close(self.0.as_ptr()) };
    }
}

impl Collator {
    /// `ucol_open`: the collator, or `None` where ICU answered none; the
    /// code is written either way.
    pub(crate) fn open(locale: &CStr, status: &mut Code) -> Option<Collator> {
        // SAFETY: the locale is NUL-terminated.
        NonNull::new(unsafe { sys::ucol_open(locale.as_ptr(), status) }).map(Collator)
    }

    /// `ucol_strcoll`: -1, 0 or 1.
    pub(crate) fn strcoll(&self, a: &[u16], b: &[u16]) -> i32 {
        let (a_len, b_len) = (
            length(a.len()).unwrap_or(i32::MAX),
            length(b.len()).unwrap_or(i32::MAX),
        );
        // SAFETY: the collator is open, both slices live for the call.
        unsafe { sys::ucol_strcoll(self.0.as_ptr(), a.as_ptr(), a_len, b.as_ptr(), b_len) }
    }

    /// `ucol_getAttribute`.
    pub(crate) fn attribute(&self, attr: i32, status: &mut Code) -> i32 {
        // SAFETY: the collator is open.
        unsafe { sys::ucol_getAttribute(self.0.as_ptr(), attr, status) }
    }

    /// `ucol_setAttribute`.
    pub(crate) fn set_attribute(&mut self, attr: i32, value: i32, status: &mut Code) {
        // SAFETY: the collator is open and held here alone.
        unsafe { sys::ucol_setAttribute(self.0.as_ptr(), attr, value, status) };
    }

    /// `ucol_getStrength`.
    pub(crate) fn strength(&self) -> i32 {
        // SAFETY: the collator is open.
        unsafe { sys::ucol_getStrength(self.0.as_ptr()) }
    }

    /// `ucol_setStrength`.
    pub(crate) fn set_strength(&mut self, strength: i32) {
        // SAFETY: the collator is open and held here alone.
        unsafe { sys::ucol_setStrength(self.0.as_ptr(), strength) };
    }

    /// `ucol_getLocaleByType`.
    pub(crate) fn locale(&self, kind: i32, status: &mut Code) -> Option<&'static str> {
        // SAFETY: the collator is open; ICU answers a name it keeps.
        static_str(unsafe { sys::ucol_getLocaleByType(self.0.as_ptr(), kind, status) })
    }

    /// `ucol_getSortKey`, preflighted: the key with its terminating NUL,
    /// empty where ICU answered a length of zero.
    pub(crate) fn sort_key(&self, text: &[u16]) -> Vec<u8> {
        let len = length(text.len()).unwrap_or(i32::MAX);
        let mut key = Vec::new();
        loop {
            let room = length(key.len()).unwrap_or(0);
            // SAFETY: the collator is open, the text lives for the call, the
            // key holds room bytes.
            let needed = unsafe {
                sys::ucol_getSortKey(self.0.as_ptr(), text.as_ptr(), len, key.as_mut_ptr(), room)
            };
            let needed = usize::try_from(needed).unwrap_or(0);
            if needed <= key.len() {
                key.truncate(needed);
                return key;
            }
            key = vec![0; needed];
        }
    }
}
