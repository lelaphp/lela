//! What crosses the shim when php's C++ holds an ICU object: the object
//! as an opaque type ([`opaque!`]), its owner ([`owner!`]), which hands
//! out the reference the shim's `safe` functions take and deletes the
//! object once, and the strings on either side of a call: [`Text`], a
//! `UChar` slice the shim reads, [`CName`], a NUL-terminated name, and
//! [`UStr`], the `icu::UnicodeString` a shim function answers.
//!
//! A shim function declared `safe` takes only these, references to
//! opaque objects, numbers and `&mut Code`: each guards its own
//! arguments (an out-of-range field, an unknown selector) before ICU
//! sees them, so no value safe Rust can make reaches undefined
//! behaviour.

use std::ffi::{CStr, c_char};
use std::marker::PhantomData;
use std::ptr::NonNull;

/// An ICU class php's C++ holds by pointer, never made or read in Rust:
/// the nomicon's opaque type, unsized for Rust's purposes and neither
/// `Send` nor `Sync` of itself.
macro_rules! opaque {
    ($($(#[$meta:meta])* $name:ident),* $(,)?) => { $(
        $(#[$meta])*
        #[repr(C)]
        pub(crate) struct $name {
            _private: [u8; 0],
            _pin: std::marker::PhantomData<(*mut u8, std::marker::PhantomPinned)>,
        }
    )* };
}
pub(crate) use opaque;

/// The owner of one heap object the shim or ICU's C API made, freed by
/// `$delete` on drop: a `NonNull` in a transparent struct, so an
/// `Option<$owner>` crosses the shim as the nullable pointer a factory
/// answers.
macro_rules! owner {
    ($(#[$meta:meta])* $owner:ident($object:ident), $delete:ident) => {
        $(#[$meta])*
        #[repr(transparent)]
        pub(crate) struct $owner(std::ptr::NonNull<$object>);

        // SAFETY: the object is reached through its one owner alone,
        // by the request that holds it; ICU's objects are
        // thread-compatible.
        unsafe impl Send for $owner {}

        impl Drop for $owner {
            fn drop(&mut self) {
                // SAFETY: the shim made the object and this owner is its
                // one owner, so it is deleted once, here.
                unsafe { $delete(self.0) };
            }
        }

        impl std::ops::Deref for $owner {
            type Target = $object;
            fn deref(&self) -> &$object {
                // SAFETY: a live object owned by `self`.
                unsafe { self.0.as_ref() }
            }
        }

        impl std::ops::DerefMut for $owner {
            fn deref_mut(&mut self) -> &mut $object {
                // SAFETY: a live object owned by `self`, borrowed
                // uniquely.
                unsafe { self.0.as_mut() }
            }
        }
    };
}
pub(crate) use owner;

/// A `UChar` slice the shim reads for the length of one call,
/// `php_intl_text`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Text<'a> {
    ptr: *const u16,
    len: i32,
    held: PhantomData<&'a [u16]>,
}

impl<'a> From<&'a [u16]> for Text<'a> {
    fn from(units: &'a [u16]) -> Text<'a> {
        Text {
            ptr: units.as_ptr(),
            len: i32::try_from(units.len()).unwrap_or(i32::MAX),
            held: PhantomData,
        }
    }
}

/// A NUL-terminated name the shim reads for the length of one call: a
/// locale, a country, a keyword.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub(crate) struct CName<'a>(NonNull<c_char>, PhantomData<&'a CStr>);

impl<'a> From<&'a CStr> for CName<'a> {
    fn from(name: &'a CStr) -> CName<'a> {
        CName(NonNull::from(name.to_bytes_with_nul()).cast(), PhantomData)
    }
}

opaque!(
    /// `icu::UnicodeString`.
    UnicodeString
);

owner!(
    /// A `UnicodeString` the shim answered, as php's C++ holds one before
    /// `intl_charFromString`.
    UStr(UnicodeString),
    php_intl_ustr_delete
);

unsafe extern "C" {
    safe fn php_intl_ustr_buffer(text: &UnicodeString) -> *const u16;
    safe fn php_intl_ustr_length(text: &UnicodeString) -> i32;
    fn php_intl_ustr_delete(text: NonNull<UnicodeString>);
}

impl UStr {
    /// `getBuffer()`, `length()` units; none for a bogus string.
    pub(crate) fn units(&self) -> &[u16] {
        let ptr = php_intl_ustr_buffer(self);
        let len = usize::try_from(php_intl_ustr_length(self)).unwrap_or(0);
        if ptr.is_null() || len == 0 {
            return &[];
        }
        // SAFETY: the string's buffer holds `length()` units and lives as
        // long as the string, which `self` borrows.
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }

    /// `intl_charFromString`: the string in UTF-8, or the conversion's
    /// failure.
    pub(crate) fn to_utf8(&self) -> Result<Vec<u8>, super::Code> {
        super::to_utf8(self.units())
    }
}
