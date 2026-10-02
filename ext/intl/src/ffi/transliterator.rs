//! `UTransliterator` (`utrans.h`), the C API php's transliterator calls.

use std::ptr::NonNull;

use super::enumeration::{Enumeration, UEnumeration};
use super::{Code, ParseError, length};

mod sys {
    use super::super::enumeration::UEnumeration;
    use super::super::{Code, ParseError};

    /// `UTransliterator`, opaque.
    pub(crate) enum UTransliterator {}

    icu! {
        fn utrans_openU(id: *const u16, id_len: i32, dir: i32, rules: *const u16, rules_len: i32, pe: *mut ParseError, status: &mut Code) -> *mut UTransliterator;
        fn utrans_openInverse(trans: *const UTransliterator, status: &mut Code) -> *mut UTransliterator;
        fn utrans_clone(trans: *const UTransliterator, status: &mut Code) -> *mut UTransliterator;
        fn utrans_close(trans: *mut UTransliterator);
        fn utrans_getUnicodeID(trans: *const UTransliterator, len: &mut i32) -> *const u16;
        fn utrans_openIDs(status: &mut Code) -> *mut UEnumeration;
        fn utrans_transUChars(trans: *const UTransliterator, text: *mut u16, len: &mut i32, cap: i32, start: i32, limit: &mut i32, status: &mut Code);
    }
}

/// One `UTransliterator`, from its opening to `utrans_close`.
pub(crate) struct Trans(NonNull<sys::UTransliterator>);

// SAFETY: a transliterator is used by the one request that holds it,
// never shared; ICU's are thread-compatible.
unsafe impl Send for Trans {}

fn owned(raw: *mut sys::UTransliterator, status: Code) -> Result<Trans, Code> {
    status.check()?;
    NonNull::new(raw).map(Trans).ok_or(Code::MEMORY_ALLOCATION)
}

impl Trans {
    /// `utrans_openU` by id, or over `rules` named by it.
    pub(crate) fn open(
        id: &[u16],
        dir: i32,
        rules: Option<&[u16]>,
        pe: &mut ParseError,
    ) -> Result<Trans, Code> {
        let mut status = Code::ZERO;
        let (rules_ptr, rules_len) = match rules {
            Some(r) => (r.as_ptr(), length(r.len())?),
            None => (std::ptr::null(), -1),
        };
        let id_len = length(id.len())?;
        // SAFETY: id and rules are live slices of the lengths given.
        let raw = unsafe {
            sys::utrans_openU(
                id.as_ptr(),
                id_len,
                dir,
                rules_ptr,
                rules_len,
                pe,
                &mut status,
            )
        };
        owned(raw, status)
    }

    /// `utrans_openInverse`.
    pub(crate) fn inverse(&self) -> Result<Trans, Code> {
        let mut status = Code::ZERO;
        // SAFETY: the transliterator is open.
        let raw = unsafe { sys::utrans_openInverse(self.0.as_ptr(), &mut status) };
        owned(raw, status)
    }

    /// `utrans_clone`.
    pub(crate) fn try_clone(&self) -> Result<Trans, Code> {
        let mut status = Code::ZERO;
        // SAFETY: the transliterator is open.
        let raw = unsafe { sys::utrans_clone(self.0.as_ptr(), &mut status) };
        owned(raw, status)
    }

    /// `utrans_getUnicodeID`.
    pub(crate) fn id(&self) -> Vec<u16> {
        let mut len = 0;
        // SAFETY: the transliterator is open; ICU answers len UChars it owns.
        let id = unsafe { sys::utrans_getUnicodeID(self.0.as_ptr(), &mut len) };
        if id.is_null() {
            return Vec::new();
        }
        // SAFETY: id holds len UChars, valid while the transliterator is.
        unsafe { std::slice::from_raw_parts(id, usize::try_from(len).unwrap_or(0)) }.to_vec()
    }

    /// `utrans_transUChars` in place over `buffer`, whose first `len`
    /// `UChar`s are the text; `len` becomes the answer's length.
    pub(crate) fn trans_uchars(
        &self,
        buffer: &mut [u16],
        len: &mut i32,
        start: i32,
        limit: i32,
    ) -> Code {
        let (mut limit, mut status) = (limit, Code::ZERO);
        let Ok(cap) = length(buffer.len()) else {
            return Code::INDEX_OUTOFBOUNDS;
        };
        let (trans, text) = (self.0.as_ptr(), buffer.as_mut_ptr());
        // SAFETY: the buffer holds cap UChars, len of them the text.
        unsafe { sys::utrans_transUChars(trans, text, len, cap, start, &mut limit, &mut status) };
        status
    }
}

impl Drop for Trans {
    fn drop(&mut self) {
        // SAFETY: the transliterator is open and closed once, here.
        unsafe { sys::utrans_close(self.0.as_ptr()) };
    }
}

/// `utrans_openIDs`.
pub(crate) fn ids() -> Result<Enumeration, Code> {
    let mut status = Code::ZERO;
    // SAFETY: the call takes a status and answers an enumeration or null.
    let raw: *mut UEnumeration = unsafe { sys::utrans_openIDs(&mut status) };
    status.check()?;
    Enumeration::from_raw(raw).ok_or(Code::MEMORY_ALLOCATION)
}
