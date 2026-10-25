//! `icu::BreakIterator` through `shim/breakiterator.cpp`: php's
//! breakiterator is C++, its code point iterator its own subclass.

use std::ffi::{CStr, c_void};
use std::ptr::NonNull;

use php_ext::request::Request;
use php_ext::values::{Owner, PhpString};

use super::{Code, ParseError, fill_chars, fill_utf16, length, static_str};

mod sys {
    use std::ffi::{c_char, c_int, c_void};

    use super::super::{Code, ParseError};

    unsafe extern "C" {
        pub(crate) fn php_intl_brk_open(
            kind: c_int,
            locale: *const c_char,
            status: &mut Code,
        ) -> *mut c_void;
        pub(crate) fn php_intl_brk_open_rules(
            rules: *const c_void,
            len: i32,
            pe: *mut ParseError,
            status: &mut Code,
        ) -> *mut c_void;
        pub(crate) fn php_intl_brk_clone(bi: *const c_void) -> *mut c_void;
        pub(crate) fn php_intl_brk_close(bi: *mut c_void);
        pub(crate) fn php_intl_brk_kind(bi: *const c_void) -> c_int;
        pub(crate) fn php_intl_brk_type(bi: *const c_void) -> *const c_char;
        pub(crate) fn php_intl_brk_equal(a: *const c_void, b: *const c_void) -> c_int;
        pub(crate) fn php_intl_brk_set_text(
            bi: *mut c_void,
            text: *const c_char,
            len: i64,
            status: &mut Code,
        ) -> c_int;
        pub(crate) fn php_intl_brk_step(bi: *mut c_void, op: c_int, arg: i32) -> i32;
        pub(crate) fn php_intl_brk_locale(
            bi: *const c_void,
            kind: c_int,
            dest: *mut c_char,
            cap: i32,
            status: &mut Code,
        ) -> i32;
        pub(crate) fn php_intl_brk_rules(
            bi: *mut c_void,
            dest: *mut u16,
            cap: i32,
            status: &mut Code,
        ) -> i32;
        pub(crate) fn php_intl_brk_binary_rules(bi: *mut c_void, len: &mut u32) -> *const u8;
        pub(crate) fn php_intl_brk_rule_status_vec(
            bi: *mut c_void,
            dest: *mut i32,
            cap: i32,
            status: &mut Code,
        ) -> i32;
    }
}

/// One `icu::BreakIterator`, from its creation to its `delete`.
struct Raw(NonNull<c_void>);

// SAFETY: an iterator is used by the one request that holds it, never
// shared; ICU's break iterators are thread-compatible.
unsafe impl Send for Raw {}

impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: the iterator is live and deleted once, here.
        unsafe { sys::php_intl_brk_close(self.0.as_ptr()) };
    }
}

/// One `icu::BreakIterator` and the strings it reads in place, which it
/// holds for as long as it lives: the text `setText()` handed it, which
/// ICU walks through a shallow `UText`, and the compiled rules it was
/// built over, which `RuleBasedBreakIterator` does not copy
/// (`BreakIterator_object`'s `text` and php 8.5.12's `compiled_rules`).
/// A clone holds them again, since ICU's clone reads the same bytes.
/// [`Breaker::close`] deletes the iterator before it gives them up, as
/// `BreakIterator_objects_free` does.
pub(crate) struct Breaker {
    raw: Raw,
    text: Option<Owner<PhpString>>,
    rules: Option<Owner<PhpString>>,
}

/// The integer calls of [`Breaker::step`], `php_intl_brk_step`'s ops.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    First,
    Last,
    Previous,
    Next,
    Current,
    Following,
    Preceding,
    IsBoundary,
    NextBy,
    RuleStatus,
    LastCodePoint,
}

impl Breaker {
    fn from_raw(raw: *mut c_void) -> Option<Breaker> {
        NonNull::new(raw).map(|raw| Breaker {
            raw: Raw(raw),
            text: None,
            rules: None,
        })
    }

    /// The iterator, live while `self` is.
    fn ptr(&self) -> *mut c_void {
        self.raw.0.as_ptr()
    }

    /// The iterator deleted, then the strings it read given up.
    pub(crate) fn close(mut self, req: &mut Request) {
        let held = [self.text.take(), self.rules.take()];
        drop(self);
        for owner in held.into_iter().flatten() {
            owner.give_up(req);
        }
    }

    /// The text `setText()` handed it, `None` before any.
    pub(crate) fn text(&self) -> Option<PhpString> {
        self.text.as_deref().copied()
    }

    /// A factory's iterator for `locale`: `kind` 0 to 4 in
    /// `createCharacterInstance`, word, line, sentence and title order,
    /// 5 php's code point iterator.
    pub(crate) fn open(kind: i32, locale: &CStr) -> Result<Breaker, Code> {
        let mut status = Code::ZERO;
        // SAFETY: the locale is NUL-terminated; the shim answers an
        // iterator the caller owns, or null with the status set.
        let raw = unsafe { sys::php_intl_brk_open(kind, locale.as_ptr(), &mut status) };
        status.check()?;
        Breaker::from_raw(raw).ok_or(Code::MEMORY_ALLOCATION)
    }

    /// `new RuleBasedBreakIterator` over source rules in UTF-16, the
    /// parse error filled where they fail.
    pub(crate) fn from_rules(rules: &[u16], pe: &mut ParseError) -> Result<Breaker, Code> {
        let mut status = Code::ZERO;
        let len = length(rules.len())?;
        // SAFETY: the rules are len UChars of a live slice; ICU copies them.
        let raw =
            unsafe { sys::php_intl_brk_open_rules(rules.as_ptr().cast(), len, pe, &mut status) };
        status.check()?;
        Breaker::from_raw(raw).ok_or(Code::MEMORY_ALLOCATION)
    }

    /// `new RuleBasedBreakIterator` over compiled rules, which ICU does not
    /// copy: the iterator holds the string, so the bytes it reads live as
    /// long as it does.
    pub(crate) fn from_binary(req: &Request, rules: PhpString) -> Result<Breaker, Code> {
        let mut status = Code::ZERO;
        let bytes = rules.as_bytes();
        let len = length(bytes.len())?;
        let none = std::ptr::null_mut();
        // SAFETY: the rules are len bytes of a string the iterator holds
        // below, before anything can release it, for as long as it lives.
        let raw =
            unsafe { sys::php_intl_brk_open_rules(bytes.as_ptr().cast(), len, none, &mut status) };
        status.check()?;
        let mut breaker = Breaker::from_raw(raw).ok_or(Code::MEMORY_ALLOCATION)?;
        breaker.rules = Some(Owner::take(req, rules));
        Ok(breaker)
    }

    /// `BreakIterator::clone`, `None` where ICU fails it: the copy reads
    /// the text and the compiled rules where the original does, so it
    /// holds both again.
    pub(crate) fn try_clone(&self, req: &Request) -> Option<Breaker> {
        // SAFETY: the iterator is live; the clone is the caller's.
        let mut copy = Breaker::from_raw(unsafe { sys::php_intl_brk_clone(self.ptr()) })?;
        copy.text = self.text().map(|text| Owner::take(req, text));
        copy.rules = self.rules.as_deref().map(|rules| Owner::take(req, *rules));
        Some(copy)
    }

    /// 1 for a rule-based iterator, 2 for php's code point one, 0 else.
    pub(crate) fn kind(&self) -> i32 {
        // SAFETY: the iterator is live.
        unsafe { sys::php_intl_brk_kind(self.ptr()) }
    }

    /// `typeid(*biter).name()`.
    pub(crate) fn type_name(&self) -> &'static str {
        // SAFETY: the iterator is live; the name is static.
        static_str(unsafe { sys::php_intl_brk_type(self.ptr()) }).unwrap_or("")
    }

    /// `BreakIterator::operator==`.
    pub(crate) fn equals(&self, other: &Breaker) -> bool {
        // SAFETY: both iterators are live.
        unsafe { sys::php_intl_brk_equal(self.ptr(), other.ptr()) != 0 }
    }

    /// `utext_openUTF8` over `text` and `setText`: `Err((1, code))` where
    /// the `UText` did not open, `Err((2, code))` where the iterator
    /// refused it. ICU keeps pointing into `text`, so the iterator holds
    /// it, and answers the text it held before, which the caller gives up
    /// (`zval_ptr_dtor(&bio->text)`).
    #[must_use = "the text held before is given up with the request"]
    pub(crate) fn set_text(
        &mut self,
        req: &Request,
        text: PhpString,
    ) -> Result<Option<Owner<PhpString>>, (i32, Code)> {
        let mut status = Code::ZERO;
        let bytes = text.as_bytes();
        let len = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        // SAFETY: the iterator is live; text is len bytes of a string the
        // iterator holds below, before anything can release it.
        let failed = unsafe {
            sys::php_intl_brk_set_text(self.ptr(), bytes.as_ptr().cast(), len, &mut status)
        };
        if failed != 0 {
            return Err((failed, status));
        }
        Ok(self.text.replace(Owner::take(req, text)))
    }

    /// One of the integer calls.
    pub(crate) fn step(&mut self, step: Step, arg: i32) -> i32 {
        // SAFETY: the iterator is live; every op is defined for any arg.
        unsafe { sys::php_intl_brk_step(self.ptr(), step as i32, arg) }
    }

    /// `getLocale(type).getName()`.
    pub(crate) fn locale(&self, kind: i32) -> (Vec<u8>, Code) {
        fill_chars(|dest, cap, status| {
            // SAFETY: the iterator is live; dest holds cap bytes.
            unsafe { sys::php_intl_brk_locale(self.ptr(), kind, dest, cap, status) }
        })
    }

    /// `RuleBasedBreakIterator::getRules`, in UTF-16.
    pub(crate) fn rules(&mut self) -> (Vec<u16>, Code) {
        let bi = self.ptr();
        // SAFETY: the iterator is live; dest holds cap UChars.
        fill_utf16(|dest, cap, status| unsafe { sys::php_intl_brk_rules(bi, dest, cap, status) })
    }

    /// `RuleBasedBreakIterator::getBinaryRules`, copied.
    pub(crate) fn binary_rules(&mut self) -> Vec<u8> {
        let mut len = 0;
        // SAFETY: the iterator is live; ICU answers len bytes it owns.
        let rules = unsafe { sys::php_intl_brk_binary_rules(self.ptr(), &mut len) };
        if rules.is_null() {
            return Vec::new();
        }
        // SAFETY: rules holds len bytes, valid while the iterator is.
        unsafe { std::slice::from_raw_parts(rules, len as usize) }.to_vec()
    }

    /// `getRuleStatusVec`, preflighted as php preflights it.
    pub(crate) fn rule_status_vec(&mut self) -> Result<Vec<i32>, Code> {
        let bi = self.ptr();
        let mut status = Code::ZERO;
        // SAFETY: the iterator is live; a zero capacity writes nothing.
        let n =
            unsafe { sys::php_intl_brk_rule_status_vec(bi, std::ptr::null_mut(), 0, &mut status) };
        let mut out = vec![0; usize::try_from(n).unwrap_or(0)];
        status = Code::ZERO;
        // SAFETY: out holds n ints.
        let n = unsafe { sys::php_intl_brk_rule_status_vec(bi, out.as_mut_ptr(), n, &mut status) };
        status.check()?;
        out.truncate(usize::try_from(n).unwrap_or(0));
        Ok(out)
    }
}
