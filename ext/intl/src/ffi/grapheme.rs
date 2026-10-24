//! `ubrk.h`, `utext.h`, `usearch.h` and `ucol.h` as `grapheme_*` calls
//! them: a character break iterator over UTF-16 or UTF-8 text, a string
//! search over it, and a collator for `grapheme_levenshtein`.

use std::ffi::{CStr, c_char};
use std::marker::PhantomData;
use std::ptr;

use super::{Code, length};

mod sys {
    use std::ffi::c_char;

    use super::super::Code;
    use super::super::collator::sys::UCollator;

    /// `UBreakIterator`, opaque.
    pub(crate) enum UBreakIterator {}
    /// `UText`, opaque.
    pub(crate) enum UText {}
    /// `UStringSearch`, opaque.
    pub(crate) enum UStringSearch {}

    icu! {
        fn ubrk_open(
            kind: i32,
            locale: *const c_char,
            text: *const u16,
            len: i32,
            status: &mut Code,
        ) -> *mut UBreakIterator;
        fn ubrk_close(bi: *mut UBreakIterator);
        fn ubrk_setUText(bi: *mut UBreakIterator, text: *mut UText, status: &mut Code);
        fn ubrk_next(bi: *mut UBreakIterator) -> i32;
        fn ubrk_previous(bi: *mut UBreakIterator) -> i32;
        fn ubrk_first(bi: *mut UBreakIterator) -> i32;
        fn ubrk_last(bi: *mut UBreakIterator) -> i32;
        fn ubrk_current(bi: *const UBreakIterator) -> i32;
        fn ubrk_following(bi: *mut UBreakIterator, offset: i32) -> i32;
        fn ubrk_isBoundary(bi: *mut UBreakIterator, offset: i32) -> i8;
        fn utext_openUTF8(ut: *mut UText, s: *const c_char, len: i64, status: &mut Code) -> *mut UText;
        fn utext_close(ut: *mut UText) -> *mut UText;
        fn usearch_open(
            pattern: *const u16,
            pattern_len: i32,
            text: *const u16,
            text_len: i32,
            locale: *const c_char,
            bi: *mut UBreakIterator,
            status: &mut Code,
        ) -> *mut UStringSearch;
        fn usearch_close(search: *mut UStringSearch);
        fn usearch_getCollator(search: *const UStringSearch) -> *mut UCollator;
        fn usearch_reset(search: *mut UStringSearch);
        fn usearch_setOffset(search: *mut UStringSearch, position: i32, status: &mut Code);
        fn usearch_next(search: *mut UStringSearch, status: &mut Code) -> i32;
        fn usearch_last(search: *mut UStringSearch, status: &mut Code) -> i32;
    }
}

/// `UBRK_DONE` and `USEARCH_DONE`.
pub(crate) const DONE: i32 = -1;

/// A character break iterator over text it borrows, the `UText` it reads
/// UTF-8 through beside it.
pub(crate) struct Breaks<'t> {
    bi: *mut sys::UBreakIterator,
    text: *mut sys::UText,
    held: PhantomData<&'t [u8]>,
}

impl<'t> Breaks<'t> {
    /// `ubrk_open(UBRK_CHARACTER, NULL, text, len)`.
    pub(crate) fn utf16(text: &'t [u16]) -> Result<Breaks<'t>, Code> {
        let len = length(text.len())?;
        let mut status = Code::ZERO;
        // SAFETY: the text is a live slice of len units the iterator
        // borrows for 't, which outlives it.
        let bi = unsafe { sys::ubrk_open(0, ptr::null(), text.as_ptr(), len, &mut status) };
        let breaks = Breaks {
            bi,
            text: ptr::null_mut(),
            held: PhantomData,
        };
        status.check().map(|_| breaks)
    }

    /// `utext_openUTF8` and `ubrk_setUText` over a character iterator.
    pub(crate) fn utf8(text: &'t [u8]) -> Result<Breaks<'t>, Code> {
        let mut status = Code::ZERO;
        let len = i64::try_from(text.len()).unwrap_or(i64::MAX);
        // SAFETY: the bytes are a live slice the UText borrows for 't.
        let ut =
            unsafe { sys::utext_openUTF8(ptr::null_mut(), text.as_ptr().cast(), len, &mut status) };
        status.check()?;
        // SAFETY: an iterator with no text yet.
        let bi = unsafe { sys::ubrk_open(0, ptr::null(), ptr::null(), 0, &mut status) };
        let breaks = Breaks {
            bi,
            text: ut,
            held: PhantomData,
        };
        status.check()?;
        // SAFETY: both are open; the iterator reads the UText it is given.
        unsafe { sys::ubrk_setUText(breaks.bi, breaks.text, &mut status) };
        status.check().map(|_| breaks)
    }

    /// `ubrk_next`.
    pub(crate) fn next(&mut self) -> i32 {
        // SAFETY: the iterator is open over text it borrows.
        unsafe { sys::ubrk_next(self.bi) }
    }

    /// `ubrk_previous`.
    pub(crate) fn previous(&mut self) -> i32 {
        // SAFETY: as `next`.
        unsafe { sys::ubrk_previous(self.bi) }
    }

    /// `ubrk_first`.
    pub(crate) fn first(&mut self) -> i32 {
        // SAFETY: as `next`.
        unsafe { sys::ubrk_first(self.bi) }
    }

    /// `ubrk_last`.
    pub(crate) fn last(&mut self) -> i32 {
        // SAFETY: as `next`.
        unsafe { sys::ubrk_last(self.bi) }
    }

    /// `ubrk_current`.
    pub(crate) fn current(&self) -> i32 {
        // SAFETY: as `next`.
        unsafe { sys::ubrk_current(self.bi) }
    }

    /// `ubrk_following`: the first boundary after `offset`.
    pub(crate) fn following(&mut self, offset: i32) -> i32 {
        // SAFETY: as `next`; ICU pins an offset outside the text.
        unsafe { sys::ubrk_following(self.bi, offset) }
    }

    /// `ubrk_isBoundary`.
    pub(crate) fn is_boundary(&mut self, offset: i32) -> bool {
        // SAFETY: as `next`; ICU pins an offset outside the text.
        unsafe { sys::ubrk_isBoundary(self.bi, offset) != 0 }
    }
}

impl Drop for Breaks<'_> {
    fn drop(&mut self) {
        // SAFETY: the iterator is closed once, here; ICU takes null.
        unsafe { sys::ubrk_close(self.bi) };
        if !self.text.is_null() {
            // SAFETY: the UText is closed once, after the iterator.
            unsafe { sys::utext_close(self.text) };
        }
    }
}

/// A `UStringSearch` over a needle and a haystack it borrows, matching at
/// the boundaries of the iterator it owns.
pub(crate) struct Search<'t> {
    search: *mut sys::UStringSearch,
    breaks: Breaks<'t>,
}

impl<'t> Search<'t> {
    /// `usearch_open(needle, haystack, locale, bi)`.
    pub(crate) fn open(
        needle: &'t [u16],
        haystack: &'t [u16],
        locale: &CStr,
        breaks: Breaks<'t>,
    ) -> Result<Search<'t>, Code> {
        let (needle_len, haystack_len) = (length(needle.len())?, length(haystack.len())?);
        let mut status = Code::ZERO;
        let locale: *const c_char = locale.as_ptr();
        // SAFETY: both texts live for 't and the iterator is owned beside
        // the search, which is closed first.
        let search = unsafe {
            sys::usearch_open(
                needle.as_ptr(),
                needle_len,
                haystack.as_ptr(),
                haystack_len,
                locale,
                breaks.bi,
                &mut status,
            )
        };
        let search = Search { search, breaks };
        status.check().map(|_| search)
    }

    /// The iterator the search matches at.
    pub(crate) fn breaks(&mut self) -> &mut Breaks<'t> {
        &mut self.breaks
    }

    /// `ucol_setAttribute(usearch_getCollator(s), UCOL_STRENGTH,
    /// UCOL_SECONDARY)` and `usearch_reset`.
    pub(crate) fn ignore_case(&mut self) -> Result<(), Code> {
        let mut status = Code::ZERO;
        // SAFETY: the search is open and answers the collator it owns.
        let coll = unsafe { sys::usearch_getCollator(self.search) };
        // SAFETY: the collator lives as long as the search.
        unsafe { super::collator::sys::ucol_setAttribute(coll, 5, 1, &mut status) };
        status.check()?;
        // SAFETY: the search is open.
        unsafe { sys::usearch_reset(self.search) };
        Ok(())
    }

    /// `usearch_setOffset`.
    pub(crate) fn set_offset(&mut self, position: i32) -> Result<(), Code> {
        let mut status = Code::ZERO;
        // SAFETY: the search is open; ICU refuses an offset outside it.
        unsafe { sys::usearch_setOffset(self.search, position, &mut status) };
        status.check().map(drop)
    }

    /// `usearch_next`.
    pub(crate) fn next(&mut self, status: &mut Code) -> i32 {
        // SAFETY: the search is open.
        unsafe { sys::usearch_next(self.search, status) }
    }

    /// `usearch_last`.
    pub(crate) fn last(&mut self, status: &mut Code) -> i32 {
        // SAFETY: the search is open.
        unsafe { sys::usearch_last(self.search, status) }
    }
}

impl Drop for Search<'_> {
    fn drop(&mut self) {
        // SAFETY: the search is closed once, here, before its iterator.
        unsafe { sys::usearch_close(self.search) };
    }
}
