//! `ures.h`: the resource bundle php's `ResourceBundle` holds, one
//! `UResourceBundle` per object, and the reads `resourcebundle/` makes of
//! it. Each element is opened as a bundle of its own (php's `fillIn` is
//! a reuse of memory, never observed), which a table or an array element
//! hands on to the `ResourceBundle` that holds it.

use std::ffi::{CStr, c_char};

use super::Code;
use super::cpp::{opaque, owner};

opaque!(
    /// `UResourceBundle`.
    UResourceBundle
);

owner!(
    /// One open `UResourceBundle`, from `ures_open` or an element's
    /// lookup to `ures_close`.
    Bundle(UResourceBundle),
    ures_close
);

use sys::ures_close;

mod sys {
    use std::ffi::c_char;
    use std::ptr::NonNull;

    use super::super::Code;
    use super::{Bundle, UResourceBundle};

    icu! {
        fn ures_open(package: *const c_char, locale: *const c_char, status: &mut Code) -> Option<Bundle>;
        fn ures_openDirect(package: *const c_char, locale: *const c_char, status: &mut Code) -> Option<Bundle>;
        fn ures_close(bundle: NonNull<UResourceBundle>);
        fn ures_getByKey(
            bundle: &UResourceBundle,
            key: *const c_char,
            fill: Option<Bundle>,
            status: &mut Code,
        ) -> Option<Bundle>;
        fn ures_getByIndex(bundle: &UResourceBundle, index: i32, fill: Option<Bundle>, status: &mut Code) -> Option<Bundle>;
        fn ures_getType(bundle: &UResourceBundle) -> i32;
        fn ures_getString(bundle: &UResourceBundle, len: &mut i32, status: &mut Code) -> *const u16;
        fn ures_getBinary(bundle: &UResourceBundle, len: &mut i32, status: &mut Code) -> *const u8;
        fn ures_getInt(bundle: &UResourceBundle, status: &mut Code) -> i32;
        fn ures_getIntVector(bundle: &UResourceBundle, len: &mut i32, status: &mut Code) -> *const i32;
        fn ures_getSize(bundle: &UResourceBundle) -> i32;
        fn ures_getKey(bundle: &UResourceBundle) -> *const c_char;
        fn ures_getLocaleByType(bundle: &UResourceBundle, kind: i32, status: &mut Code) -> *const c_char;
    }
}

/// `UResType`, as `ures_getType` answers it for an element php reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    String,
    Binary,
    Table,
    Int,
    Array,
    IntVector,
    /// `URES_NONE` and `URES_ALIAS`, which a lookup resolves.
    Other,
}

/// The `n` items ICU answered at `ptr`, copied out; none for a null one.
fn copied<T: Copy>(ptr: *const T, n: i32) -> Vec<T> {
    let n = usize::try_from(n).unwrap_or(0);
    if ptr.is_null() || n == 0 {
        return Vec::new();
    }
    // SAFETY: ICU answered `n` items at `ptr`, held by the bundle the
    // caller borrows for the call.
    unsafe { std::slice::from_raw_parts(ptr, n) }.to_vec()
}

impl Bundle {
    /// `ures_open`, or `ures_openDirect` where no fallback is wanted: the
    /// bundle of `package` (ICU's own data for none) for `locale`.
    pub(crate) fn open(
        package: Option<&CStr>,
        locale: &CStr,
        fallback: bool,
        status: &mut Code,
    ) -> Option<Bundle> {
        let package = package.map_or(std::ptr::null(), CStr::as_ptr);
        if fallback {
            // SAFETY: both names are NUL-terminated or the package null.
            unsafe { sys::ures_open(package, locale.as_ptr(), status) }
        } else {
            // SAFETY: as above.
            unsafe { sys::ures_openDirect(package, locale.as_ptr(), status) }
        }
    }

    /// `ures_getByKey`.
    pub(crate) fn by_key(&self, key: &CStr, status: &mut Code) -> Option<Bundle> {
        // SAFETY: the bundle is open, the key NUL-terminated.
        unsafe { sys::ures_getByKey(self, key.as_ptr(), None, status) }
    }

    /// `ures_getByIndex`.
    pub(crate) fn by_index(&self, index: i32, status: &mut Code) -> Option<Bundle> {
        // SAFETY: the bundle is open; ICU bounds the index.
        unsafe { sys::ures_getByIndex(self, index, None, status) }
    }

    /// `ures_getType`.
    pub(crate) fn kind(&self) -> Kind {
        // SAFETY: the bundle is open.
        match unsafe { sys::ures_getType(self) } {
            0 => Kind::String,
            1 => Kind::Binary,
            2 => Kind::Table,
            7 => Kind::Int,
            8 => Kind::Array,
            14 => Kind::IntVector,
            _ => Kind::Other,
        }
    }

    /// `ures_getString`, copied out.
    pub(crate) fn string(&self, status: &mut Code) -> Vec<u16> {
        let mut len = 0;
        // SAFETY: the bundle is open; ICU writes the length.
        copied(unsafe { sys::ures_getString(self, &mut len, status) }, len)
    }

    /// `ures_getBinary`, copied out.
    pub(crate) fn binary(&self, status: &mut Code) -> Vec<u8> {
        let mut len = 0;
        // SAFETY: the bundle is open; ICU writes the length.
        copied(unsafe { sys::ures_getBinary(self, &mut len, status) }, len)
    }

    /// `ures_getInt`.
    pub(crate) fn int(&self, status: &mut Code) -> i32 {
        // SAFETY: the bundle is open.
        unsafe { sys::ures_getInt(self, status) }
    }

    /// `ures_getIntVector`, copied out.
    pub(crate) fn int_vector(&self, status: &mut Code) -> Vec<i32> {
        let mut len = 0;
        // SAFETY: the bundle is open; ICU writes the length.
        copied(
            unsafe { sys::ures_getIntVector(self, &mut len, status) },
            len,
        )
    }

    /// `ures_getSize`.
    pub(crate) fn size(&self) -> i32 {
        // SAFETY: the bundle is open.
        unsafe { sys::ures_getSize(self) }
    }

    /// `ures_getKey`: the element's key in its table, copied out.
    pub(crate) fn key(&self) -> Vec<u8> {
        // SAFETY: the bundle is open; the key is NUL-terminated, or null.
        let key: *const c_char = unsafe { sys::ures_getKey(self) };
        if key.is_null() {
            return Vec::new();
        }
        // SAFETY: a NUL-terminated key the bundle holds for the call.
        unsafe { CStr::from_ptr(key) }.to_bytes().to_vec()
    }

    /// `ures_getLocaleByType(ULOC_ACTUAL_LOCALE)`, copied out.
    pub(crate) fn actual_locale(&self, status: &mut Code) -> Option<Vec<u8>> {
        // SAFETY: the bundle is open; ICU answers a name it keeps, or null.
        let name = unsafe { sys::ures_getLocaleByType(self, 0, status) };
        (!name.is_null()).then(|| {
            // SAFETY: a NUL-terminated name the bundle holds for the call.
            unsafe { CStr::from_ptr(name) }.to_bytes().to_vec()
        })
    }
}
