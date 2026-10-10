//! `uspoof.h` and the `USet` php builds for `setAllowedChars`: the spoof
//! checker php's `Spoofchecker` holds, and the `USpoofCheckResult` ICU
//! 58 and later write a check's findings into.

use std::ffi::CStr;

use super::cpp::{opaque, owner};
use super::{Code, length};

opaque!(
    /// `USpoofChecker`.
    USpoofChecker,
    /// `USpoofCheckResult`.
    USpoofCheckResult,
    /// `USet`.
    USet
);

owner!(
    /// One `USpoofChecker`, from `uspoof_open` or `uspoof_clone` to
    /// `uspoof_close`.
    Checker(USpoofChecker),
    uspoof_close
);

owner!(
    /// One `USpoofCheckResult`, from `uspoof_openCheckResult` to
    /// `uspoof_closeCheckResult`.
    CheckResult(USpoofCheckResult),
    uspoof_closeCheckResult
);

owner!(
    /// One `USet`, from `uset_openEmpty` to `uset_close`.
    Set(USet),
    uset_close
);

use sys::{uset_close, uspoof_close, uspoof_closeCheckResult};

mod sys {
    use std::ffi::c_char;
    use std::ptr::NonNull;

    use super::super::Code;
    use super::{CheckResult, Checker, Set, USet, USpoofCheckResult, USpoofChecker};

    icu! {
        fn uspoof_open(status: &mut Code) -> Option<Checker>;
        fn uspoof_close(checker: NonNull<USpoofChecker>);
        fn uspoof_clone(checker: &USpoofChecker, status: &mut Code) -> Option<Checker>;
        fn uspoof_setRestrictionLevel(checker: &mut USpoofChecker, level: i32);
        fn uspoof_openCheckResult(status: &mut Code) -> Option<CheckResult>;
        fn uspoof_closeCheckResult(result: NonNull<USpoofCheckResult>);
        fn uspoof_getCheckResultChecks(result: Option<&USpoofCheckResult>, status: &mut Code) -> i32;
        fn uspoof_check2UTF8(
            checker: &USpoofChecker,
            id: *const c_char,
            length: i32,
            result: Option<&mut USpoofCheckResult>,
            status: &mut Code,
        ) -> i32;
        fn uspoof_areConfusableUTF8(
            checker: &USpoofChecker,
            id1: *const c_char,
            length1: i32,
            id2: *const c_char,
            length2: i32,
            status: &mut Code,
        ) -> i32;
        fn uspoof_setAllowedLocales(checker: &mut USpoofChecker, locales: *const c_char, status: &mut Code);
        fn uspoof_setChecks(checker: &mut USpoofChecker, checks: i32, status: &mut Code);
        fn uspoof_setAllowedChars(checker: &mut USpoofChecker, chars: &USet, status: &mut Code);
        fn uset_openEmpty() -> Option<Set>;
        fn uset_close(set: NonNull<USet>);
        fn uset_applyPattern(set: &mut USet, pattern: *const u16, length: i32, options: u32, status: &mut Code) -> i32;
        fn uset_compact(set: &mut USet);
    }
}

impl Checker {
    /// `uspoof_open`, none beside a failure ([`Code::built`]).
    pub(crate) fn open(status: &mut Code) -> Option<Checker> {
        // SAFETY: the call takes a status alone.
        let made = unsafe { sys::uspoof_open(status) };
        status.built(made)
    }

    /// `uspoof_clone`, none beside a failure ([`Code::built`]).
    pub(crate) fn try_clone(&self, status: &mut Code) -> Option<Checker> {
        // SAFETY: the checker is open.
        let made = unsafe { sys::uspoof_clone(self, status) };
        status.built(made)
    }

    /// `uspoof_setRestrictionLevel`, the level one of `URestrictionLevel`'s.
    pub(crate) fn set_restriction_level(&mut self, level: i32) {
        // SAFETY: the checker is open and held here alone.
        unsafe { sys::uspoof_setRestrictionLevel(self, level) };
    }

    /// `uspoof_check2UTF8`: the checks that failed, written into `result`
    /// where there is one.
    pub(crate) fn check(
        &self,
        id: &[u8],
        result: Option<&mut CheckResult>,
        status: &mut Code,
    ) -> i32 {
        let len = length(id.len()).unwrap_or(i32::MAX);
        let result = result.map(|r| &mut **r);
        // SAFETY: the checker is open, the text `len` bytes of a live
        // slice, the result open or none.
        unsafe { sys::uspoof_check2UTF8(self, id.as_ptr().cast(), len, result, status) }
    }

    /// `uspoof_areConfusableUTF8`: the confusability checks the two pass.
    pub(crate) fn confusable(&self, one: &[u8], two: &[u8], status: &mut Code) -> i32 {
        let (Ok(len1), Ok(len2)) = (length(one.len()), length(two.len())) else {
            *status = Code::BUFFER_OVERFLOW;
            return 0;
        };
        // SAFETY: the checker is open, both texts live slices of the
        // lengths given.
        unsafe {
            sys::uspoof_areConfusableUTF8(
                self,
                one.as_ptr().cast(),
                len1,
                two.as_ptr().cast(),
                len2,
                status,
            )
        }
    }

    /// `uspoof_setAllowedLocales`.
    pub(crate) fn set_allowed_locales(&mut self, locales: &CStr, status: &mut Code) {
        // SAFETY: the checker is open and held here alone; the list is
        // NUL-terminated.
        unsafe { sys::uspoof_setAllowedLocales(self, locales.as_ptr(), status) };
    }

    /// `uspoof_setChecks`.
    pub(crate) fn set_checks(&mut self, checks: i32, status: &mut Code) {
        // SAFETY: the checker is open and held here alone.
        unsafe { sys::uspoof_setChecks(self, checks, status) };
    }

    /// `uspoof_setAllowedChars`, which copies the set.
    pub(crate) fn set_allowed_chars(&mut self, chars: &Set, status: &mut Code) {
        // SAFETY: the checker is open and held here alone, the set open.
        unsafe { sys::uspoof_setAllowedChars(self, chars, status) };
    }
}

impl CheckResult {
    /// `uspoof_openCheckResult`, none beside a failure ([`Code::built`]).
    pub(crate) fn open(status: &mut Code) -> Option<CheckResult> {
        // SAFETY: the call takes a status alone.
        let made = unsafe { sys::uspoof_openCheckResult(status) };
        status.built(made)
    }
}

/// `uspoof_getCheckResultChecks` over a `USpoofCheckResult`, or none, which ICU
/// refuses as an illegal argument.
pub(crate) fn result_checks(result: Option<&CheckResult>, status: &mut Code) -> i32 {
    // SAFETY: the result is open or none.
    unsafe { sys::uspoof_getCheckResultChecks(result.map(|r| &**r), status) }
}

impl Set {
    /// `uset_openEmpty`.
    pub(crate) fn empty() -> Option<Set> {
        // SAFETY: the call takes nothing.
        unsafe { sys::uset_openEmpty() }
    }

    /// `uset_applyPattern`, then `uset_compact` where it took.
    pub(crate) fn apply_pattern(&mut self, pattern: &[u16], options: u32, status: &mut Code) {
        let len = length(pattern.len()).unwrap_or(i32::MAX);
        // SAFETY: the set is open and held here alone, the pattern `len`
        // UChars of a live slice.
        unsafe { sys::uset_applyPattern(self, pattern.as_ptr(), len, options, status) };
        if !status.is_failure() {
            // SAFETY: the set is open and held here alone.
            unsafe { sys::uset_compact(self) };
        }
    }
}
