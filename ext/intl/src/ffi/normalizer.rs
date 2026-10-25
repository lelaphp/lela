//! `unorm2.h`: the normalizers php's `Normalizer` asks ICU for, which
//! ICU owns for the process.

use super::{Code, fill_utf16, length};

mod sys {
    use super::super::Code;

    /// `UNormalizer2`, opaque.
    pub(crate) enum UNormalizer2 {}

    icu! {
        fn unorm2_getNFCInstance(status: &mut Code) -> *const UNormalizer2;
        fn unorm2_getNFDInstance(status: &mut Code) -> *const UNormalizer2;
        fn unorm2_getNFKCInstance(status: &mut Code) -> *const UNormalizer2;
        fn unorm2_getNFKDInstance(status: &mut Code) -> *const UNormalizer2;
        fn unorm2_getNFKCCasefoldInstance(status: &mut Code) -> *const UNormalizer2;
        fn unorm2_normalize(
            norm: *const UNormalizer2,
            src: *const u16,
            len: i32,
            dest: *mut u16,
            capacity: i32,
            status: &mut Code,
        ) -> i32;
        fn unorm2_isNormalized(
            norm: *const UNormalizer2,
            src: *const u16,
            len: i32,
            status: &mut Code,
        ) -> i8;
        fn unorm2_getRawDecomposition(
            norm: *const UNormalizer2,
            c: i32,
            dest: *mut u16,
            capacity: i32,
            status: &mut Code,
        ) -> i32;
    }
}

/// The normalization forms `intl_get_normalizer` knows.
#[derive(Clone, Copy)]
pub(crate) enum Form {
    C,
    D,
    Kc,
    Kd,
    KcCasefold,
}

/// One of ICU's normalizers, which ICU keeps for the process.
#[derive(Clone, Copy)]
pub(crate) struct Normalizer(*const sys::UNormalizer2);

impl Normalizer {
    /// `intl_get_normalizer`: the form's instance.
    pub(crate) fn of(form: Form) -> Result<Normalizer, Code> {
        let mut status = Code::ZERO;
        let get = match form {
            Form::C => sys::unorm2_getNFCInstance,
            Form::D => sys::unorm2_getNFDInstance,
            Form::Kc => sys::unorm2_getNFKCInstance,
            Form::Kd => sys::unorm2_getNFKDInstance,
            Form::KcCasefold => sys::unorm2_getNFKCCasefoldInstance,
        };
        // SAFETY: the getters take a status and answer ICU's own instance.
        let norm = unsafe { get(&mut status) };
        status.check()?;
        Ok(Normalizer(norm))
    }

    /// `unorm2_normalize`, preflighted.
    pub(crate) fn normalize(self, text: &[u16]) -> Result<Vec<u16>, Code> {
        let len = length(text.len())?;
        let (out, status) = fill_utf16(|dest, capacity, status| {
            // SAFETY: the source is a live slice of len units, dest holds
            // capacity units and ICU writes no further.
            unsafe { sys::unorm2_normalize(self.0, text.as_ptr(), len, dest, capacity, status) }
        });
        status.check().map(|_| out)
    }

    /// `unorm2_normalize` into `dest`, its last unit left for the
    /// terminator as php hands `uret_len` beside `uret_len + 1` units: the
    /// length the whole text needs, and the code.
    pub(crate) fn normalize_into(self, text: &[u16], dest: &mut [u16]) -> (i32, Code) {
        let mut status = Code::ZERO;
        let Ok(len) = length(text.len()) else {
            return (-1, Code::BUFFER_OVERFLOW);
        };
        let capacity = length(dest.len().saturating_sub(1)).unwrap_or(i32::MAX);
        // SAFETY: the source is a live slice of len units, dest holds more
        // than capacity units and ICU writes no further.
        let needed = unsafe {
            sys::unorm2_normalize(
                self.0,
                text.as_ptr(),
                len,
                dest.as_mut_ptr(),
                capacity,
                &mut status,
            )
        };
        (needed, status)
    }

    /// `unorm2_isNormalized`.
    pub(crate) fn is_normalized(self, text: &[u16]) -> Result<bool, Code> {
        let len = length(text.len())?;
        let mut status = Code::ZERO;
        // SAFETY: the source is a live slice of len units.
        let normalized =
            unsafe { sys::unorm2_isNormalized(self.0, text.as_ptr(), len, &mut status) };
        status.check().map(|_| normalized != 0)
    }

    /// `unorm2_getRawDecomposition` into php's 32 units, `None` for a code
    /// point with none.
    pub(crate) fn raw_decomposition(self, c: i32) -> Option<Vec<u16>> {
        let mut buffer = [0u16; 32];
        let mut status = Code::ZERO;
        // SAFETY: the buffer holds 32 units and ICU writes no further.
        let len = unsafe {
            sys::unorm2_getRawDecomposition(self.0, c, buffer.as_mut_ptr(), 32, &mut status)
        };
        let len = usize::try_from(len).ok()?;
        Some(buffer[..len.min(32)].to_vec())
    }
}
