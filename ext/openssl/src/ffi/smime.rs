//! Signed and enveloped messages: [`Pkcs7`] (`PKCS7`), [`Cms`]
//! (`CMS_ContentInfo`) and the key store [`Pkcs12`] (`PKCS12`).

use std::ffi::{CStr, c_int, c_uint, c_void};
use std::ptr;

use openssl_sys as sys;

use super::cipher::Cipher;
use super::key::PKey;
use super::x509::{CertStack, Certificate, Pems, Store, pems_of};
use super::{Bio, stack_items, written};

// SAFETY: libcrypto's own declarations, from `openssl/cms.h` and
// `openssl/pkcs12.h` of OpenSSL 3, which `openssl-sys` does not declare;
// the library `openssl-sys` links defines each.
unsafe extern "C" {
    fn SMIME_read_CMS_ex(
        bio: *mut sys::BIO,
        flags: c_int,
        bcont: *mut *mut sys::BIO,
        ci: *mut *mut sys::CMS_ContentInfo,
    ) -> *mut sys::CMS_ContentInfo;
    fn d2i_CMS_bio(
        bp: *mut sys::BIO,
        cms: *mut *mut sys::CMS_ContentInfo,
    ) -> *mut sys::CMS_ContentInfo;
    fn CMS_get0_signers(cms: *mut sys::CMS_ContentInfo) -> *mut sys::OPENSSL_STACK;
    fn CMS_final(
        cms: *mut sys::CMS_ContentInfo,
        data: *mut sys::BIO,
        dcont: *mut sys::BIO,
        flags: c_uint,
    ) -> c_int;
    fn i2d_CMS_bio(bp: *mut sys::BIO, cms: *mut sys::CMS_ContentInfo) -> c_int;
    fn PEM_write_bio_CMS_stream(
        out: *mut sys::BIO,
        cms: *mut sys::CMS_ContentInfo,
        input: *mut sys::BIO,
        flags: c_int,
    ) -> c_int;
    fn CMS_get0_type(cms: *const sys::CMS_ContentInfo) -> *const sys::ASN1_OBJECT;
    fn CMS_get1_certs(cms: *mut sys::CMS_ContentInfo) -> *mut sys::OPENSSL_STACK;
    fn CMS_get1_crls(cms: *mut sys::CMS_ContentInfo) -> *mut sys::OPENSSL_STACK;
    fn SMIME_write_CMS(
        bio: *mut sys::BIO,
        cms: *mut sys::CMS_ContentInfo,
        data: *mut sys::BIO,
        flags: c_int,
    ) -> c_int;
    fn d2i_PKCS12_bio(bp: *mut sys::BIO, p12: *mut *mut sys::PKCS12) -> *mut sys::PKCS12;
}

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

/// `X509_CRL_free` in the shape `OPENSSL_sk_pop_free` calls.
unsafe extern "C" fn free_crl(crl: *mut c_void) {
    // SAFETY: the stack's items are CRLs it owns.
    unsafe { sys::X509_CRL_free(crl.cast()) };
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

owned!(
    /// A `CMS_ContentInfo`.
    Cms,
    sys::CMS_ContentInfo,
    sys::CMS_ContentInfo_free
);

impl Cms {
    /// `SMIME_read_CMS_ex(in, 0, &datain, NULL)`: the structure and, for a
    /// detached signature, the content beside it.
    pub(crate) fn read_smime(input: &Bio<'_>) -> Option<(Cms, Option<Bio<'static>>)> {
        let mut content = ptr::null_mut();
        // SAFETY: the BIO is live; a content BIO made is handed over.
        let cms =
            unsafe { SMIME_read_CMS_ex(input.as_ptr(), 0, &raw mut content, ptr::null_mut()) };
        let content = Bio::adopt(content);
        Some((Cms::owned(cms)?, content))
    }

    /// `PEM_read_bio_CMS(in, NULL, NULL, NULL)`.
    pub(crate) fn read_pem(input: &Bio<'_>) -> Option<Cms> {
        // SAFETY: the BIO is live; no callback and no user data.
        let cms = unsafe {
            sys::PEM_read_bio_CMS(input.as_ptr(), ptr::null_mut(), None, ptr::null_mut())
        };
        Cms::owned(cms)
    }

    /// `d2i_CMS_bio(in, NULL)`.
    pub(crate) fn read_der(input: &Bio<'_>) -> Option<Cms> {
        // SAFETY: the BIO is live.
        Cms::owned(unsafe { d2i_CMS_bio(input.as_ptr(), ptr::null_mut()) })
    }

    /// `CMS_verify(cms, others, store, data, out, flags)`, nonzero.
    pub(crate) fn verify(
        &self,
        others: Option<&CertStack>,
        store: &Store,
        data: Option<&Bio<'_>>,
        out: Option<&Bio<'_>>,
        flags: c_uint,
    ) -> bool {
        // SAFETY: the structure and store are live; the rest live or null.
        unsafe {
            sys::CMS_verify(
                self.as_ptr(),
                stack_or_null(others),
                store.as_ptr(),
                bio_or_null(data),
                bio_or_null(out),
                flags,
            ) != 0
        }
    }

    /// `CMS_get0_signers(cms)`: each signer as PEM.
    pub(crate) fn signers(&self) -> Option<Pems> {
        // SAFETY: the structure is live.
        let signers = unsafe { CMS_get0_signers(self.as_ptr()) };
        // SAFETY: the stack is ours to free, its certificates are not.
        unsafe { signer_pems(signers) }
    }

    /// `PEM_write_bio_CMS(out, cms)`, nonzero.
    pub(crate) fn write_pem(&self, out: &Bio<'_>) -> bool {
        // SAFETY: the sink and the structure are live.
        unsafe { sys::PEM_write_bio_CMS(out.as_ptr(), self.as_ptr()) != 0 }
    }

    /// `PEM_write_bio_CMS_stream(out, cms, in, flags)`, nonzero.
    pub(crate) fn write_pem_stream(&self, out: &Bio<'_>, input: &Bio<'_>, flags: c_int) -> bool {
        // SAFETY: every argument is live.
        unsafe { PEM_write_bio_CMS_stream(out.as_ptr(), self.as_ptr(), input.as_ptr(), flags) != 0 }
    }

    /// `i2d_CMS_bio(out, cms) == 1`.
    pub(crate) fn write_der(&self, out: &Bio<'_>) -> bool {
        // SAFETY: the sink and the structure are live.
        unsafe { i2d_CMS_bio(out.as_ptr(), self.as_ptr()) == 1 }
    }

    /// `SMIME_write_CMS(out, cms, in, flags)`, nonzero.
    pub(crate) fn write_smime(&self, out: &Bio<'_>, input: &Bio<'_>, flags: c_int) -> bool {
        // SAFETY: every argument is live.
        unsafe { SMIME_write_CMS(out.as_ptr(), self.as_ptr(), input.as_ptr(), flags) != 0 }
    }

    /// `CMS_final(cms, in, NULL, flags) == 1`.
    pub(crate) fn finish(&mut self, input: &Bio<'_>, flags: c_uint) -> bool {
        // SAFETY: the structure and the input are live.
        unsafe { CMS_final(self.as_ptr(), input.as_ptr(), ptr::null_mut(), flags) == 1 }
    }

    /// `CMS_encrypt(certs, in, cipher, flags)`.
    pub(crate) fn encrypt(
        certs: &CertStack,
        input: &Bio<'_>,
        cipher: &Cipher,
        flags: c_uint,
    ) -> Option<Cms> {
        // SAFETY: every argument is live.
        let cms = unsafe {
            sys::CMS_encrypt(
                certs.as_ptr().cast(),
                input.as_ptr(),
                cipher.as_ptr(),
                flags,
            )
        };
        Cms::owned(cms)
    }

    /// `CMS_sign(cert, key, others, in, flags)`.
    pub(crate) fn sign(
        cert: &Certificate,
        key: &PKey,
        others: Option<&CertStack>,
        input: &Bio<'_>,
        flags: c_uint,
    ) -> Option<Cms> {
        // SAFETY: every argument is live or null.
        let cms = unsafe {
            sys::CMS_sign(
                cert.as_ptr(),
                key.as_ptr(),
                stack_or_null(others),
                input.as_ptr(),
                flags,
            )
        };
        Cms::owned(cms)
    }

    /// `CMS_decrypt(cms, key, cert, NULL, out, 0)`, nonzero.
    pub(crate) fn decrypt(&self, key: &PKey, cert: &Certificate, out: &Bio<'_>) -> bool {
        // SAFETY: every argument is live.
        unsafe {
            sys::CMS_decrypt(
                self.as_ptr(),
                key.as_ptr(),
                cert.as_ptr(),
                ptr::null_mut(),
                out.as_ptr(),
                0,
            ) != 0
        }
    }

    /// For a signed, or signed and enveloped, structure, `CMS_get1_certs`
    /// and `CMS_get1_crls` as PEM; none for any other type.
    pub(crate) fn certs_and_crls(&self) -> (Pems, Pems) {
        // SAFETY: the structure is live; the type is its own object.
        let kind = unsafe { CMS_get0_type(self.as_ptr()) };
        // SAFETY: the object is live.
        let nid = unsafe { sys::OBJ_obj2nid(kind) };
        if nid != NID_PKCS7_SIGNED && nid != NID_PKCS7_SIGNED_AND_ENVELOPED {
            return (Vec::new(), Vec::new());
        }
        // SAFETY: the structure is live; the stack made is ours.
        let certs = CertStack::adopt(unsafe { CMS_get1_certs(self.as_ptr()) });
        let certs = certs.map(|c| c.pems()).unwrap_or_default();
        // SAFETY: the structure is live; the stack made is ours.
        let crls = unsafe { CMS_get1_crls(self.as_ptr()) };
        // SAFETY: the stack is live or null.
        let pems = unsafe { crl_pems(crls) };
        if !crls.is_null() {
            // SAFETY: the stack and its CRLs are ours, freed once.
            unsafe { sys::OPENSSL_sk_pop_free(crls, Some(free_crl)) };
        }
        (certs, pems)
    }
}

owned!(
    /// A `PKCS12` key store.
    Pkcs12,
    sys::PKCS12,
    sys::PKCS12_free
);

/// What a key store holds once `PKCS12_parse` has opened it.
pub(crate) struct Parsed {
    pub(crate) key: Option<PKey>,
    pub(crate) cert: Option<Certificate>,
    pub(crate) extra: Option<CertStack>,
}

impl Pkcs12 {
    /// `PKCS12_create(pass, name, key, cert, ca, 0, 0, 0, 0, 0)`.
    pub(crate) fn create(
        pass: &CStr,
        name: Option<&CStr>,
        key: &PKey,
        cert: &Certificate,
        extra: Option<&CertStack>,
    ) -> Option<Pkcs12> {
        let name = name.map_or(ptr::null(), CStr::as_ptr);
        // SAFETY: every argument is live, NUL-terminated or null.
        let p12 = unsafe {
            sys::PKCS12_create(
                pass.as_ptr(),
                name,
                key.as_ptr(),
                cert.as_ptr(),
                stack_or_null(extra),
                0,
                0,
                0,
                0,
                0,
            )
        };
        Pkcs12::owned(p12)
    }

    /// `d2i_PKCS12_bio(in, NULL)`.
    pub(crate) fn read_der(input: &Bio<'_>) -> Option<Pkcs12> {
        // SAFETY: the BIO is live.
        Pkcs12::owned(unsafe { d2i_PKCS12_bio(input.as_ptr(), ptr::null_mut()) })
    }

    /// `i2d_PKCS12_bio(out, p12)`, nonzero.
    pub(crate) fn write_der(&self, out: &Bio<'_>) -> bool {
        // SAFETY: the sink and the store are live.
        unsafe { sys::i2d_PKCS12_bio(out.as_ptr(), self.as_ptr()) != 0 }
    }

    /// `PKCS12_parse(p12, pass, &key, &cert, &ca)`, `None` where it failed.
    pub(crate) fn parse(&self, pass: &CStr) -> Option<Parsed> {
        let (mut key, mut cert, mut ca) = (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
        // SAFETY: the store is live and the passphrase NUL-terminated; what
        // the library stores in the three is handed over.
        let done = unsafe {
            sys::PKCS12_parse(
                self.as_ptr(),
                pass.as_ptr(),
                &raw mut key,
                &raw mut cert,
                &raw mut ca,
            )
        };
        let parsed = Parsed {
            key: PKey::owned(key),
            cert: Certificate::owned(cert),
            extra: CertStack::adopt(ca.cast()),
        };
        (done == 1).then_some(parsed)
    }
}
