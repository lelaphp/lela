//! Signed and enveloped messages: [`Pkcs7`] (`PKCS7`).

use std::ffi::{c_int, c_void};
use std::ptr;

use openssl_sys as sys;

use super::cipher::Cipher;
use super::key::PKey;
use super::x509::{CertStack, Certificate, Pems, Store, pems_of};
use super::{Bio, stack_items, written};

/// `NID_pkcs7_signed` and `NID_pkcs7_signedAndEnveloped`.
const NID_PKCS7_SIGNED: c_int = 22;
const NID_PKCS7_SIGNED_AND_ENVELOPED: c_int = 24;

/// A stack pointer, or null for `None`.
fn stack_or_null(stack: Option<&CertStack>) -> *mut sys::stack_st_X509 {
    stack.map_or(ptr::null_mut(), |s| s.as_ptr().cast())
}

/// A BIO pointer, or null for `None`.
fn bio_or_null(bio: Option<&Bio<'_>>) -> *mut sys::BIO {
    bio.map_or(ptr::null_mut(), Bio::as_ptr)
}

/// Each CRL of a stack as PEM, `None` where one would not write.
///
/// # Safety
///
/// `stack` is null or a live `STACK_OF(X509_CRL)`.
unsafe fn crl_pems(stack: *const sys::OPENSSL_STACK) -> Pems {
    if stack.is_null() {
        return Vec::new();
    }
    // SAFETY: the caller's live stack.
    let items = unsafe { stack_items(stack) };
    let pem = |crl: *mut c_void| {
        // SAFETY: each item is a live CRL.
        written(|out| unsafe { sys::PEM_write_bio_X509_CRL(out, crl.cast()) })
    };
    items.into_iter().map(pem).collect()
}

/// The certificates of a borrowed signer stack as PEM, the stack freed
/// and not its certificates; `None` for a null stack.
///
/// # Safety
///
/// `signers` is null or a stack the caller owns and whose certificates
/// it does not.
unsafe fn signer_pems(signers: *mut sys::OPENSSL_STACK) -> Option<Pems> {
    if signers.is_null() {
        return None;
    }
    // SAFETY: the caller's live stack.
    let pems = unsafe { pems_of(signers) };
    // SAFETY: the stack is the caller's, freed once; the certificates are
    // not.
    unsafe { sys::OPENSSL_sk_free(signers) };
    Some(pems)
}

owned!(
    /// A `PKCS7` structure.
    Pkcs7,
    sys::PKCS7,
    sys::PKCS7_free
);

impl Pkcs7 {
    /// `SMIME_read_PKCS7(in, &datain)`: the structure and, for a detached
    /// signature, the content beside it.
    pub(crate) fn read_smime(input: &Bio<'_>) -> Option<(Pkcs7, Option<Bio<'static>>)> {
        let mut content = ptr::null_mut();
        // SAFETY: the BIO is live; a content BIO made is handed over.
        let p7 = unsafe { sys::SMIME_read_PKCS7(input.as_ptr(), &raw mut content) };
        let content = Bio::adopt(content);
        Some((Pkcs7::owned(p7)?, content))
    }

    /// `PEM_read_bio_PKCS7(in, NULL, NULL, NULL)`.
    pub(crate) fn read_pem(input: &Bio<'_>) -> Option<Pkcs7> {
        // SAFETY: the BIO is live; no callback and no user data.
        let p7 = unsafe {
            sys::PEM_read_bio_PKCS7(input.as_ptr(), ptr::null_mut(), None, ptr::null_mut())
        };
        Pkcs7::owned(p7)
    }

    /// `PKCS7_verify(p7, others, store, data, out, flags)`, nonzero.
    pub(crate) fn verify(
        &self,
        others: Option<&CertStack>,
        store: &Store,
        data: Option<&Bio<'_>>,
        out: Option<&Bio<'_>>,
        flags: c_int,
    ) -> bool {
        // SAFETY: the structure and store are live; the rest live or null.
        unsafe {
            sys::PKCS7_verify(
                self.as_ptr(),
                stack_or_null(others),
                store.as_ptr(),
                bio_or_null(data),
                bio_or_null(out),
                flags,
            ) != 0
        }
    }

    /// `PKCS7_get0_signers(p7, others, flags)`: each signer as PEM.
    pub(crate) fn signers(&self, others: Option<&CertStack>, flags: c_int) -> Option<Pems> {
        // SAFETY: the structure is live, the others live or null.
        let signers =
            unsafe { sys::PKCS7_get0_signers(self.as_ptr(), stack_or_null(others), flags) };
        // SAFETY: the stack is ours to free, its certificates are not.
        unsafe { signer_pems(signers.cast()) }
    }

    /// `PEM_write_bio_PKCS7(out, p7)`, nonzero.
    pub(crate) fn write_pem(&self, out: &Bio<'_>) -> bool {
        // SAFETY: the sink and the structure are live.
        unsafe { sys::PEM_write_bio_PKCS7(out.as_ptr(), self.as_ptr()) != 0 }
    }

    /// `PKCS7_encrypt(certs, in, cipher, flags)`.
    pub(crate) fn encrypt(
        certs: &CertStack,
        input: &Bio<'_>,
        cipher: &Cipher,
        flags: c_int,
    ) -> Option<Pkcs7> {
        // SAFETY: every argument is live.
        let p7 = unsafe {
            sys::PKCS7_encrypt(
                certs.as_ptr().cast(),
                input.as_ptr(),
                cipher.as_ptr(),
                flags,
            )
        };
        Pkcs7::owned(p7)
    }

    /// `PKCS7_sign(cert, key, others, in, flags)`.
    pub(crate) fn sign(
        cert: &Certificate,
        key: &PKey,
        others: Option<&CertStack>,
        input: &Bio<'_>,
        flags: c_int,
    ) -> Option<Pkcs7> {
        // SAFETY: every argument is live or null.
        let p7 = unsafe {
            sys::PKCS7_sign(
                cert.as_ptr(),
                key.as_ptr(),
                stack_or_null(others),
                input.as_ptr(),
                flags,
            )
        };
        Pkcs7::owned(p7)
    }

    /// `SMIME_write_PKCS7(out, p7, in, flags)`, nonzero.
    pub(crate) fn write_smime(&self, out: &Bio<'_>, input: &Bio<'_>, flags: c_int) -> bool {
        // SAFETY: every argument is live.
        unsafe { sys::SMIME_write_PKCS7(out.as_ptr(), self.as_ptr(), input.as_ptr(), flags) != 0 }
    }

    /// `PKCS7_decrypt(p7, key, cert, out, flags)`, nonzero.
    pub(crate) fn decrypt(
        &self,
        key: &PKey,
        cert: &Certificate,
        out: &Bio<'_>,
        flags: c_int,
    ) -> bool {
        // SAFETY: every argument is live.
        unsafe {
            sys::PKCS7_decrypt(
                self.as_ptr(),
                key.as_ptr(),
                cert.as_ptr(),
                out.as_ptr(),
                flags,
            ) != 0
        }
    }

    /// The certificates and CRLs a signed, or signed and enveloped,
    /// structure carries, each as PEM; none for any other type.
    pub(crate) fn certs_and_crls(&self) -> (Pems, Pems) {
        // SAFETY: the structure is live and read, not changed.
        let p7 = unsafe { &*self.as_ptr() };
        // SAFETY: the type is the structure's own object.
        // SAFETY: the type is the structure's own object.
        let nid = unsafe { sys::OBJ_obj2nid(p7.type_) };
        // A signed and a signed-and-enveloped part both begin with
        // version, digests, certificates and CRLs.
        let signed = match nid {
            // SAFETY: the union's member is the one the type names.
            NID_PKCS7_SIGNED => unsafe { p7.d.sign },
            // SAFETY: as above.
            NID_PKCS7_SIGNED_AND_ENVELOPED => unsafe { p7.d.signed_and_enveloped }.cast(),
            _ => ptr::null_mut(),
        };
        // SAFETY: the part is live or null.
        let (certs, crls) = match unsafe { signed.as_ref() } {
            Some(signed) => (
                signed.cert.cast_const().cast(),
                signed.crl.cast_const().cast(),
            ),
            None => (ptr::null(), ptr::null()),
        };
        // SAFETY: the stacks are the structure's own, live or null.
        (unsafe { pems_of(certs) }, unsafe { crl_pems(crls) })
    }
}
