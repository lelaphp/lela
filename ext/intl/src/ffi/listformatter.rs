//! `ulistformatter.h`: the list format php's `IntlListFormatter` holds.

use std::ffi::CStr;
use std::ptr::NonNull;

use super::{Code, length};

mod sys {
    use std::ffi::c_char;

    use super::super::Code;
    use super::UListFormatter;

    icu! {
        fn ulistfmt_openForType(locale: *const c_char, kind: i32, width: i32, status: &mut Code) -> *mut UListFormatter;
        fn ulistfmt_close(fmt: *mut UListFormatter);
        fn ulistfmt_format(
            fmt: *const UListFormatter,
            strings: *const *const u16,
            lengths: *const i32,
            count: i32,
            out: *mut u16,
            cap: i32,
            status: &mut Code,
        ) -> i32;
    }
}

/// `UListFormatter`, opaque.
pub(crate) enum UListFormatter {}

/// One `UListFormatter`, from `ulistfmt_openForType` to `ulistfmt_close`.
pub(crate) struct ListFormat(NonNull<UListFormatter>);

// SAFETY: a format is used by the one request whose object holds it.
unsafe impl Send for ListFormat {}

impl ListFormat {
    /// `ulistfmt_openForType`, none beside a failure ([`Code::built`]).
    pub(crate) fn open(
        locale: &CStr,
        kind: i32,
        width: i32,
        status: &mut Code,
    ) -> Option<ListFormat> {
        // SAFETY: the locale is NUL-terminated.
        let raw = unsafe { sys::ulistfmt_openForType(locale.as_ptr(), kind, width, status) };
        status.built(NonNull::new(raw).map(ListFormat))
    }

    /// `ulistfmt_format` of `items` into `out`, the full length answered.
    pub(crate) fn format(&self, items: &[Vec<u16>], out: &mut [u16], status: &mut Code) -> i32 {
        let strings: Vec<*const u16> = items.iter().map(Vec::as_ptr).collect();
        let lengths: Vec<i32> = items.iter().map(|i| length(i.len()).unwrap_or(0)).collect();
        let (count, cap) = (
            length(items.len()).unwrap_or(0),
            length(out.len()).unwrap_or(0),
        );
        let at = if out.is_empty() {
            std::ptr::null_mut()
        } else {
            out.as_mut_ptr()
        };
        // SAFETY: count strings of the given lengths, out cap UChars or null at zero.
        unsafe {
            sys::ulistfmt_format(
                self.0.as_ptr(),
                strings.as_ptr(),
                lengths.as_ptr(),
                count,
                at,
                cap,
                status,
            )
        }
    }
}

impl Drop for ListFormat {
    fn drop(&mut self) {
        // SAFETY: the format is open and closed once, here.
        unsafe { sys::ulistfmt_close(self.0.as_ptr()) };
    }
}
