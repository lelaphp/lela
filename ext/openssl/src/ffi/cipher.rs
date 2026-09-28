//! Digests and symmetric ciphers as the key natives find them:
//! [`Digest`] (`EVP_MD`) and [`Cipher`] (`EVP_CIPHER`).

use std::ffi::CStr;
use std::ptr;

use openssl_sys as sys;

owned!(
    /// An `EVP_MD`, as `php_openssl_get_evp_md_by_name` finds it and
    /// `php_openssl_release_evp_md` gives it back: `EVP_MD_free` is a
    /// no-op on the library's static tables and frees a fetched one.
    Digest,
    sys::EVP_MD,
    sys::EVP_MD_free
);

impl Digest {
    /// `OBJ_NAME_get(name, OBJ_NAME_TYPE_MD_METH)`, then
    /// `EVP_MD_fetch(NULL, name, NULL)`.
    pub(crate) fn by_name(name: &CStr) -> Option<Digest> {
        // SAFETY: the name is NUL-terminated; the answer is a static table
        // or null.
        let found = unsafe { sys::EVP_get_digestbyname(name.as_ptr()) };
        if let Some(found) = Digest::owned(found.cast_mut()) {
            return Some(found);
        }
        // SAFETY: the name is NUL-terminated; the answer is a new
        // reference or null.
        Digest::owned(unsafe { sys::EVP_MD_fetch(ptr::null_mut(), name.as_ptr(), ptr::null()) })
    }

    /// `EVP_md_null()`, the digest a configuration's `null` names.
    pub(crate) fn null() -> Option<Digest> {
        // SAFETY: the answer is the library's static table.
        Digest::owned(unsafe { sys::EVP_md_null() }.cast_mut())
    }
}

owned!(
    /// An `EVP_CIPHER`, found as `php_openssl_get_evp_cipher_by_name`
    /// finds it and given back with `EVP_CIPHER_free`, a no-op on a
    /// static one.
    Cipher,
    sys::EVP_CIPHER,
    sys::EVP_CIPHER_free
);

impl Cipher {
    /// `OBJ_NAME_get(name, OBJ_NAME_TYPE_CIPHER_METH)`, then
    /// `EVP_CIPHER_fetch(NULL, name, NULL)`.
    pub(crate) fn by_name(name: &CStr) -> Option<Cipher> {
        // SAFETY: the name is NUL-terminated; the answer is a static table
        // or null.
        let found = unsafe { sys::EVP_get_cipherbyname(name.as_ptr()) };
        if let Some(found) = Cipher::owned(found.cast_mut()) {
            return Some(found);
        }
        // SAFETY: the name is NUL-terminated; the answer is a new
        // reference or null.
        Cipher::owned(unsafe { sys::EVP_CIPHER_fetch(ptr::null_mut(), name.as_ptr(), ptr::null()) })
    }

    /// `EVP_des_ede3_cbc()`, the cipher a passphrase encrypts a private
    /// key with when the options name none.
    pub(crate) fn des_ede3_cbc() -> Option<Cipher> {
        // SAFETY: the answer is the library's static table.
        Cipher::owned(unsafe { sys::EVP_des_ede3_cbc() }.cast_mut())
    }
}
