//! X.509: [`Certificate`] (`X509`), the names in it ([`Name`]), the
//! signing request [`Csr`] (`X509_REQ`), the trust [`Store`]
//! (`X509_STORE`) a certificate is checked against, [`CertStack`]
//! (`STACK_OF(X509)`) and the extensions a configuration section adds.

use std::ffi::{CStr, c_char, c_int, c_long, c_uchar, c_ulong, c_void};
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::ptr::{self, NonNull};

use openssl_sys as sys;

use super::cipher::Digest;
use super::key::PKey;
use super::{Bio, Conf, bytes_at, copied, int_len, object_oid, stack_items, taken, written};

// SAFETY: libcrypto's own declarations, from `openssl/x509.h`,
// `openssl/x509v3.h`, `openssl/x509_vfy.h`, `openssl/asn1.h` and
// `openssl/pem.h` of OpenSSL 3, which `openssl-sys` does not declare; the
// library `openssl-sys` links defines each.
unsafe extern "C" {
    fn X509_get0_notBefore(x: *const sys::X509) -> *const sys::ASN1_TIME;
    fn X509_get0_notAfter(x: *const sys::X509) -> *const sys::ASN1_TIME;
    fn X509_check_private_key(cert: *const sys::X509, pkey: *const sys::EVP_PKEY) -> c_int;
    fn X509_NAME_oneline(a: *const sys::X509_NAME, buf: *mut c_char, size: c_int) -> *mut c_char;
    fn X509_NAME_get_index_by_NID(name: *const sys::X509_NAME, nid: c_int, last: c_int) -> c_int;
    fn i2s_ASN1_INTEGER(meth: *mut c_void, aint: *const sys::ASN1_INTEGER) -> *mut c_char;
    fn X509_PURPOSE_get_count() -> c_int;
    fn X509_PURPOSE_get0(idx: c_int) -> *mut c_void;
    fn X509_PURPOSE_get_id(xp: *const c_void) -> c_int;
    fn X509_PURPOSE_get0_sname(xp: *const c_void) -> *mut c_char;
    fn X509_PURPOSE_get0_name(xp: *const c_void) -> *mut c_char;
    fn X509_check_purpose(x: *mut sys::X509, id: c_int, ca: c_int) -> c_int;
    fn X509_alias_get0(x: *mut sys::X509, len: *mut c_int) -> *mut c_uchar;
    fn X509_subject_name_hash(x: *mut sys::X509) -> c_ulong;
    fn X509V3_EXT_print(
        out: *mut sys::BIO,
        ext: *mut sys::X509_EXTENSION,
        flag: c_ulong,
        indent: c_int,
    ) -> c_int;
    fn GENERAL_NAME_print(out: *mut sys::BIO, name: *mut sys::GENERAL_NAME) -> c_int;
    fn X509_STORE_CTX_set_purpose(ctx: *mut sys::X509_STORE_CTX, purpose: c_int) -> c_int;
    fn a2i_ASN1_INTEGER(
        bp: *mut sys::BIO,
        bs: *mut sys::ASN1_INTEGER,
        buf: *mut c_char,
        size: c_int,
    ) -> c_int;
    fn ASN1_INTEGER_set_int64(a: *mut sys::ASN1_INTEGER, r: i64) -> c_int;
    fn X509V3_EXT_add_nconf(
        conf: *mut sys::CONF,
        ctx: *mut sys::X509V3_CTX,
        section: *const c_char,
        cert: *mut sys::X509,
    ) -> c_int;
    fn X509V3_EXT_REQ_add_nconf(
        conf: *mut sys::CONF,
        ctx: *mut sys::X509V3_CTX,
        section: *const c_char,
        req: *mut sys::X509_REQ,
    ) -> c_int;
    fn X509_REQ_add1_attr_by_NID(
        req: *mut sys::X509_REQ,
        nid: c_int,
        kind: c_int,
        bytes: *const c_uchar,
        len: c_int,
    ) -> c_int;
    fn X509_REQ_add1_attr_by_txt(
        req: *mut sys::X509_REQ,
        name: *const c_char,
        kind: c_int,
        bytes: *const c_uchar,
        len: c_int,
    ) -> c_int;
    fn X509_REQ_get_attr_by_NID(req: *const sys::X509_REQ, nid: c_int, last: c_int) -> c_int;
    fn PEM_X509_INFO_read_bio_ex(
        bp: *mut sys::BIO,
        sk: *mut sys::OPENSSL_STACK,
        cb: *mut c_void,
        u: *mut c_void,
        libctx: *mut sys::OSSL_LIB_CTX,
        propq: *const c_char,
    ) -> *mut sys::OPENSSL_STACK;
    fn X509_INFO_free(info: *mut c_void);
}

/// `MBSTRING_UTF8`, how an entry's or attribute's text is given.
const MBSTRING_UTF8: c_int = 0x1000;

/// `X509V3_CTX_TEST`: extensions checked for syntax and not added.
const X509V3_CTX_TEST: c_int = 1;

/// `X509_L_FILE_LOAD`, `X509_L_ADD_DIR` and the file types they take.
const X509_L_FILE_LOAD: c_int = 1;
const X509_L_ADD_DIR: c_int = 2;
const X509_FILETYPE_PEM: c_long = 1;
const X509_FILETYPE_DEFAULT: c_long = 3;

owned!(
    /// An `X509` certificate: one reference of its count.
    Certificate,
    sys::X509,
    sys::X509_free
);

// SAFETY: an X509's reference count is atomic and what it caches it
// caches under the library's own lock; OpenSSL 3 shares certificates
// across threads.
unsafe impl Send for Certificate {}

impl Clone for Certificate {
    /// `X509_up_ref`: a second reference to the same certificate.
    fn clone(&self) -> Certificate {
        // SAFETY: the certificate is live; up_ref counts one more owner.
        unsafe { sys::X509_up_ref(self.as_ptr()) };
        Certificate(self.0)
    }
}

/// An `ASN1_STRING` copied out: its type and its bytes.
pub(crate) struct Asn1String {
    pub(crate) kind: c_int,
    pub(crate) data: Vec<u8>,
}

impl Asn1String {
    /// # Safety
    ///
    /// `string` is a live `ASN1_STRING`.
    unsafe fn of(string: *const sys::ASN1_STRING) -> Asn1String {
        // SAFETY: the caller's live string.
        let kind = unsafe { sys::ASN1_STRING_type(string) };
        // SAFETY: as above.
        let len = unsafe { sys::ASN1_STRING_length(string) };
        // SAFETY: as above.
        let data = unsafe { sys::ASN1_STRING_get0_data(string) };
        // SAFETY: the string's own bytes, `len` of them.
        let data = unsafe { bytes_at(data, len) };
        Asn1String { kind, data }
    }
}

/// One purpose `openssl_x509_parse` lists: its id, whether the
/// certificate is fit for it and as a CA for it, and its names.
pub(crate) struct Purpose {
    pub(crate) id: c_int,
    pub(crate) fit: c_int,
    pub(crate) fit_ca: c_int,
    pub(crate) short: Vec<u8>,
    pub(crate) long: Vec<u8>,
}

impl Certificate {
    /// `PEM_read_bio_X509(in, NULL, NULL, NULL)`: any of the PEM
    /// certificate labels, read from a file.
    pub(crate) fn read_pem(bio: &Bio<'_>) -> Option<Certificate> {
        // SAFETY: the BIO is live; no callback and no user data.
        let cert =
            unsafe { sys::PEM_read_bio_X509(bio.as_ptr(), ptr::null_mut(), None, ptr::null_mut()) };
        Certificate::owned(cert)
    }

    /// `PEM_ASN1_read_bio(d2i_X509, PEM_STRING_X509, in, NULL, NULL,`
    /// `NULL)`: `CERTIFICATE` alone, read from a string.
    pub(crate) fn read_pem_certificate(bio: &Bio<'_>) -> Option<Certificate> {
        // SAFETY: libcrypto's own declaration, from `openssl/pem.h`; its
        // `d2i_of_void *` is given `d2i_X509`'s type, which has its ABI.
        unsafe extern "C" {
            fn PEM_ASN1_read_bio(
                d2i: unsafe extern "C" fn(
                    *mut *mut sys::X509,
                    *mut *const c_uchar,
                    c_long,
                ) -> *mut sys::X509,
                name: *const c_char,
                bp: *mut sys::BIO,
                x: *mut *mut c_void,
                cb: sys::pem_password_cb,
                u: *mut c_void,
            ) -> *mut c_void;
        }
        // SAFETY: the BIO is live, the label is NUL-terminated, and
        // d2i_X509 is the decoder the label's contents are for.
        let cert = unsafe {
            PEM_ASN1_read_bio(
                sys::d2i_X509,
                c"CERTIFICATE".as_ptr(),
                bio.as_ptr(),
                ptr::null_mut(),
                None,
                ptr::null_mut(),
            )
        };
        Certificate::owned(cert.cast())
    }

    /// `X509_new()`: an empty certificate, for `openssl_csr_sign` to fill.
    pub(crate) fn new() -> Option<Certificate> {
        // SAFETY: X509_new takes nothing.
        Certificate::owned(unsafe { sys::X509_new() })
    }

    /// `X509_get_pubkey(cert)`: a new reference to its key.
    pub(crate) fn public_key(&self) -> Option<PKey> {
        // SAFETY: the certificate is live; the key comes back with a
        // reference of its own, or null.
        PKey::owned(unsafe { sys::X509_get_pubkey(self.as_ptr()) })
    }

    /// `X509_print(out, cert)`, nonzero.
    pub(crate) fn print(&self, out: &Bio<'_>) -> bool {
        // SAFETY: the sink and the certificate are live.
        unsafe { sys::X509_print(out.as_ptr(), self.as_ptr()) != 0 }
    }

    /// `PEM_write_bio_X509(out, cert)`, nonzero.
    pub(crate) fn write_pem(&self, out: &Bio<'_>) -> bool {
        // SAFETY: the sink and the certificate are live.
        unsafe { sys::PEM_write_bio_X509(out.as_ptr(), self.as_ptr()) != 0 }
    }

    /// `X509_digest(cert, md, buf, &n)`.
    pub(crate) fn digest(&self, md: &Digest) -> Option<Vec<u8>> {
        let mut out = [0u8; 64];
        let mut len: u32 = 0;
        // SAFETY: the certificate and digest are live; the buffer holds
        // `EVP_MAX_MD_SIZE` bytes.
        let done =
            unsafe { sys::X509_digest(self.as_ptr(), md.as_ptr(), out.as_mut_ptr(), &raw mut len) };
        (done != 0).then(|| out[..usize::try_from(len).unwrap_or(0).min(64)].to_vec())
    }

    /// `X509_check_private_key(cert, key)`, nonzero.
    pub(crate) fn matches_key(&self, key: &PKey) -> bool {
        // SAFETY: the certificate and key are live.
        unsafe { X509_check_private_key(self.as_ptr(), key.as_ptr()) != 0 }
    }

    /// `X509_verify(cert, key)`: 1, 0, or negative for an error.
    pub(crate) fn verify(&self, key: &PKey) -> c_int {
        // SAFETY: the certificate and key are live.
        unsafe { sys::X509_verify(self.as_ptr(), key.as_ptr()) }
    }

    /// `X509_get_subject_name(cert)`.
    pub(crate) fn subject(&self) -> Name<'_> {
        // SAFETY: the certificate is live; the name is its own.
        Name::of(unsafe { sys::X509_get_subject_name(self.as_ptr()) })
    }

    /// `X509_get_issuer_name(cert)`.
    pub(crate) fn issuer(&self) -> Name<'_> {
        // SAFETY: the certificate is live; the name is its own.
        Name::of(unsafe { sys::X509_get_issuer_name(self.as_ptr()) })
    }

    /// `X509_subject_name_hash(cert)`.
    pub(crate) fn subject_hash(&self) -> c_ulong {
        // SAFETY: the certificate is live.
        unsafe { X509_subject_name_hash(self.as_ptr()) }
    }

    /// `X509_get_version(cert)`.
    pub(crate) fn version(&self) -> c_long {
        // SAFETY: the certificate is live.
        unsafe { sys::X509_get_version(self.as_ptr()) }
    }

    /// The serial number as `BN_bn2hex` of `ASN1_INTEGER_to_BN` and as
    /// `i2s_ASN1_INTEGER` write it, each `None` where the library failed.
    pub(crate) fn serial(&self) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        // SAFETY: the certificate is live; the serial is its own.
        let serial = unsafe { sys::X509_get_serialNumber(self.as_ptr()) };
        // SAFETY: the serial is live; the number made is freed by `Bn`.
        let bn = super::key::Bn::owned(unsafe { sys::ASN1_INTEGER_to_BN(serial, ptr::null_mut()) });
        let Some(bn) = bn else {
            return (None, None);
        };
        // SAFETY: the number is live; the text is the caller's to free.
        let hex = unsafe { sys::BN_bn2hex(bn.as_ptr()) };
        // SAFETY: as above.
        let hex = unsafe { taken(hex) };
        if hex.is_none() {
            return (None, None);
        }
        // SAFETY: the serial is live; the text is the caller's to free.
        let decimal = unsafe { i2s_ASN1_INTEGER(ptr::null_mut(), serial) };
        // SAFETY: as above.
        (hex, unsafe { taken(decimal) })
    }

    /// `X509_getm_notBefore(cert)` and `X509_getm_notAfter(cert)`.
    pub(crate) fn validity(&self) -> (Asn1String, Asn1String) {
        // SAFETY: the certificate is live; the time is its own.
        let from = unsafe { X509_get0_notBefore(self.as_ptr()) };
        // SAFETY: as above.
        let to = unsafe { X509_get0_notAfter(self.as_ptr()) };
        // SAFETY: the times are live.
        let from = unsafe { Asn1String::of(from.cast()) };
        // SAFETY: as above.
        (from, unsafe { Asn1String::of(to.cast()) })
    }

    /// `X509_alias_get0(cert, NULL)`, as the C string php reads it.
    pub(crate) fn alias(&self) -> Option<Vec<u8>> {
        // SAFETY: the certificate is live; the alias is its own.
        let alias = unsafe { X509_alias_get0(self.as_ptr(), ptr::null_mut()) };
        // SAFETY: a non-null alias is NUL-terminated.
        unsafe { copied(alias.cast()) }
    }

    /// `X509_get_signature_nid(cert)`.
    pub(crate) fn signature_nid(&self) -> c_int {
        // SAFETY: the certificate is live.
        unsafe { sys::X509_get_signature_nid(self.as_ptr()) }
    }

    /// Every purpose the library knows, in its order, with
    /// `X509_check_purpose(cert, id, 0)` and `(cert, id, 1)`.
    pub(crate) fn purposes(&self) -> Vec<Purpose> {
        // SAFETY: X509_PURPOSE_get_count takes nothing.
        let count = unsafe { X509_PURPOSE_get_count() };
        let purpose = |i| {
            // SAFETY: the index is below the count; the purpose is static.
            let purpose = unsafe { X509_PURPOSE_get0(i) };
            // SAFETY: as above.
            let id = unsafe { X509_PURPOSE_get_id(purpose) };
            // SAFETY: the certificate is live.
            let fit = unsafe { X509_check_purpose(self.as_ptr(), id, 0) };
            // SAFETY: as above.
            let fit_ca = unsafe { X509_check_purpose(self.as_ptr(), id, 1) };
            // SAFETY: the purpose is static and so are its names.
            let short = unsafe { X509_PURPOSE_get0_sname(purpose) };
            // SAFETY: as above.
            let names = [short, unsafe { X509_PURPOSE_get0_name(purpose) }];
            // SAFETY: the names are NUL-terminated.
            let [short, long] = names.map(|n| unsafe { copied(n) }.unwrap_or_default());
            Purpose {
                id,
                fit,
                fit_ca,
                short,
                long,
            }
        };
        (0..count).map(purpose).collect()
    }

    /// The extensions as `openssl_x509_parse` renders them: each one's
    /// short name, or its OID where it has none, beside
    /// `openssl_x509v3_subjectAltName`'s rendering for a subject
    /// alternative name, `X509V3_EXT_print`'s where it prints, and its
    /// raw bytes otherwise. `None` where a subject alternative name does
    /// not decode.
    pub(crate) fn extensions(&self) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        // SAFETY: the certificate is live.
        let count = unsafe { sys::X509_get_ext_count(self.as_ptr()) };
        let mut out = Vec::new();
        for i in 0..count {
            // SAFETY: the index is below the count; the extension is the
            // certificate's own.
            let ext = unsafe { sys::X509_get_ext(self.as_ptr(), i) };
            // SAFETY: the extension is live; its object is its own.
            let object = unsafe { sys::X509_EXTENSION_get_object(ext) };
            // SAFETY: the object is live.
            let nid = unsafe { sys::OBJ_obj2nid(object) };
            let name = match super::nid_short_name(nid).filter(|_| nid != sys::NID_undef) {
                Some(name) => name,
                // SAFETY: the object is live.
                None => unsafe { object_oid(object, 255) },
            };
            let bio = Bio::memory()?;
            let value = if nid == sys::NID_subject_alt_name {
                // SAFETY: the extension is live and the sink is.
                if !unsafe { alt_names(&bio, ext) } {
                    return None;
                }
                bio.contents()
            // SAFETY: the sink and the extension are live.
            } else if unsafe { X509V3_EXT_print(bio.as_ptr(), ext, 0, 0) } > 0 {
                bio.contents()
            } else {
                // SAFETY: the extension is live; its data is its own.
                let data = unsafe { sys::X509_EXTENSION_get_data(ext) };
                // SAFETY: the data is live.
                unsafe { Asn1String::of(data.cast()) }.data
            };
            out.push((name, value));
        }
        Some(out)
    }

    /// `ASN1_INTEGER_set_int64(X509_get_serialNumber(cert), n)`, nonzero.
    pub(crate) fn set_serial(&mut self, n: i64) -> bool {
        // SAFETY: the certificate is live; the serial is its own.
        let serial = unsafe { sys::X509_get_serialNumber(self.as_ptr()) };
        // SAFETY: the serial is live.
        unsafe { ASN1_INTEGER_set_int64(serial, n) != 0 }
    }

    /// `a2i_ASN1_INTEGER` of `hex` into the serial number, in the 256
    /// bytes of line php gives it, nonzero.
    pub(crate) fn set_serial_hex(&mut self, hex: &[u8]) -> Option<bool> {
        let bio = Bio::reading(hex)?;
        let mut buf = [0 as c_char; 256];
        // SAFETY: the certificate is live; the serial is its own.
        let serial = unsafe { sys::X509_get_serialNumber(self.as_ptr()) };
        // SAFETY: the BIO and serial are live; the line buffer is 256 bytes.
        Some(unsafe { a2i_ASN1_INTEGER(bio.as_ptr(), serial, buf.as_mut_ptr(), 256) } != 0)
    }

    /// `X509_set_version(cert, v)`, nonzero.
    pub(crate) fn set_version(&mut self, version: c_long) -> bool {
        // SAFETY: the certificate is live.
        unsafe { sys::X509_set_version(self.as_ptr(), version) != 0 }
    }

    /// `X509_set_subject_name(cert, name)`, nonzero.
    pub(crate) fn set_subject(&mut self, name: &Name<'_>) -> bool {
        // SAFETY: the certificate and name are live; the name is copied.
        unsafe { sys::X509_set_subject_name(self.as_ptr(), name.0.as_ptr()) != 0 }
    }

    /// `X509_set_issuer_name(cert, name)`, nonzero.
    pub(crate) fn set_issuer(&mut self, name: &Name<'_>) -> bool {
        // SAFETY: the certificate and name are live; the name is copied.
        unsafe { sys::X509_set_issuer_name(self.as_ptr(), name.0.as_ptr()) != 0 }
    }

    /// `X509_gmtime_adj(notBefore, 0)` and `X509_gmtime_adj(notAfter,`
    /// `seconds)`, both non-null.
    pub(crate) fn set_validity(&mut self, seconds: c_long) -> bool {
        // SAFETY: the certificate is live; the time is its own.
        let from = unsafe { sys::X509_getm_notBefore(self.as_ptr()) };
        // SAFETY: as above.
        let to = unsafe { sys::X509_getm_notAfter(self.as_ptr()) };
        // SAFETY: the time is live.
        let from = unsafe { sys::X509_gmtime_adj(from, 0) };
        // SAFETY: as above.
        !from.is_null() && !unsafe { sys::X509_gmtime_adj(to, seconds) }.is_null()
    }

    /// `X509_set_pubkey(cert, key)`, nonzero.
    pub(crate) fn set_public_key(&mut self, key: &PKey) -> bool {
        // SAFETY: the certificate and key are live.
        unsafe { sys::X509_set_pubkey(self.as_ptr(), key.as_ptr()) != 0 }
    }

    /// `X509_sign(cert, key, md)`, nonzero.
    pub(crate) fn sign(&mut self, key: &PKey, md: &Digest) -> bool {
        // SAFETY: the certificate, key and digest are live.
        unsafe { sys::X509_sign(self.as_ptr(), key.as_ptr(), md.as_ptr()) != 0 }
    }
}

/// `openssl_x509v3_subjectAltName`: the names of a subject alternative
/// name extension into `bio`, `email:`, `DNS:` and `URI:` written from
/// their bytes and the rest by `GENERAL_NAME_print`, `, ` between; false
/// where the extension does not decode.
///
/// # Safety
///
/// `ext` is a live `X509_EXTENSION`.
unsafe fn alt_names(bio: &Bio<'_>, ext: *mut sys::X509_EXTENSION) -> bool {
    // SAFETY: the caller's live extension; the stack made is freed below.
    let names = unsafe { sys::X509V3_EXT_d2i(ext) }.cast::<sys::OPENSSL_STACK>();
    if names.is_null() {
        return false;
    }
    // SAFETY: the stack is live until it is freed below.
    let items = unsafe { stack_items(names) };
    for (i, item) in items.iter().enumerate() {
        let name = item.cast::<sys::GENERAL_NAME>();
        // SAFETY: each item is a live GENERAL_NAME.
        let general = unsafe { &*name };
        let (kind, d) = (general.type_, general.d);
        let prefix: Option<&[u8]> = match kind {
            sys::GEN_EMAIL => Some(b"email:"),
            sys::GEN_DNS => Some(b"DNS:"),
            sys::GEN_URI => Some(b"URI:"),
            _ => None,
        };
        match prefix {
            Some(prefix) => {
                bio.write(prefix);
                // SAFETY: these three kinds hold an IA5String.
                bio.write(&unsafe { Asn1String::of(d.cast()) }.data);
            }
            // SAFETY: the sink and the name are live.
            None => _ = unsafe { GENERAL_NAME_print(bio.as_ptr(), name) },
        }
        if i + 1 < items.len() {
            bio.write(b", ");
        }
    }
    // SAFETY: the stack and its names were made by the decode above and
    // are freed once.
    unsafe { sys::OPENSSL_sk_pop_free(names, Some(free_general_name)) };
    true
}

/// `GENERAL_NAME_free` in the shape `OPENSSL_sk_pop_free` calls.
unsafe extern "C" fn free_general_name(name: *mut c_void) {
    // SAFETY: the stack's items are GENERAL_NAMEs it owns.
    unsafe { sys::GENERAL_NAME_free(name.cast()) };
}

/// `X509_free` in the shape `OPENSSL_sk_pop_free` calls.
unsafe extern "C" fn free_certificate(cert: *mut c_void) {
    // SAFETY: the stack's items are certificates it owns.
    unsafe { sys::X509_free(cert.cast()) };
}

/// An `X509_NAME` its certificate or request owns.
pub(crate) struct Name<'a>(NonNull<sys::X509_NAME>, PhantomData<&'a ()>);

/// One entry of a name as `php_openssl_add_assoc_name_entry` reads it:
/// its NID and its text as UTF-8, `None` where the conversion failed.
pub(crate) struct NameEntry {
    pub(crate) nid: c_int,
    pub(crate) text: Option<Vec<u8>>,
}

impl<'a> Name<'a> {
    fn of(name: *mut sys::X509_NAME) -> Name<'a> {
        Name(
            NonNull::new(name).unwrap_or(NonNull::dangling()),
            PhantomData,
        )
    }

    /// `X509_NAME_oneline(name, NULL, 0)`.
    pub(crate) fn oneline(&self) -> Option<Vec<u8>> {
        // SAFETY: the name is live; the text is the caller's to free.
        let text = unsafe { X509_NAME_oneline(self.0.as_ptr(), ptr::null_mut(), 0) };
        // SAFETY: as above.
        unsafe { taken(text) }
    }

    /// Each entry: its object's NID and its data, as UTF-8 already or
    /// converted by `ASN1_STRING_to_UTF8`.
    pub(crate) fn entries(&self) -> Vec<NameEntry> {
        // SAFETY: the name is live.
        let count = unsafe { sys::X509_NAME_entry_count(self.0.as_ptr()) };
        let entry = |i| {
            // SAFETY: the index is below the count; the entry is the name's.
            let entry = unsafe { sys::X509_NAME_get_entry(self.0.as_ptr(), i) };
            // SAFETY: the entry is live; its object is its own.
            let object = unsafe { sys::X509_NAME_ENTRY_get_object(entry) };
            // SAFETY: the object is live.
            let nid = unsafe { sys::OBJ_obj2nid(object) };
            // SAFETY: the entry is live; its data is its own.
            let data = unsafe { sys::X509_NAME_ENTRY_get_data(entry) };
            // SAFETY: the data is live.
            if unsafe { sys::ASN1_STRING_type(data) } == sys::V_ASN1_UTF8STRING {
                // SAFETY: as above.
                let text = Some(unsafe { Asn1String::of(data) }.data);
                return NameEntry { nid, text };
            }
            let mut out: *mut c_uchar = ptr::null_mut();
            // SAFETY: the data is live; a new buffer is stored in `out`.
            let len = unsafe { sys::ASN1_STRING_to_UTF8(&raw mut out, data) };
            // SAFETY: `len` bytes at `out`, or null.
            let text = (len != -1).then(|| unsafe { bytes_at(out, len) });
            if !out.is_null() {
                // SAFETY: the buffer is the caller's to free, once.
                unsafe { sys::OPENSSL_free(out.cast()) };
            }
            NameEntry { nid, text }
        };
        (0..count).map(entry).collect()
    }

    /// `X509_NAME_add_entry_by_NID(name, nid, MBSTRING_UTF8, text, -1,`
    /// `-1, 0)`, nonzero; the text is read up to its NUL, as C reads it.
    pub(crate) fn add_by_nid(&self, nid: c_int, text: &CStr) -> bool {
        // SAFETY: the name is live and the text NUL-terminated.
        unsafe {
            sys::X509_NAME_add_entry_by_NID(
                self.0.as_ptr(),
                nid,
                MBSTRING_UTF8,
                text.as_ptr().cast_mut().cast(),
                -1,
                -1,
                0,
            ) != 0
        }
    }

    /// `X509_NAME_add_entry_by_txt(name, field, MBSTRING_UTF8, text, -1,`
    /// `-1, 0)`, nonzero.
    pub(crate) fn add_by_text(&self, field: &CStr, text: &CStr) -> bool {
        // SAFETY: the name is live and both strings NUL-terminated.
        unsafe {
            sys::X509_NAME_add_entry_by_txt(
                self.0.as_ptr(),
                field.as_ptr(),
                MBSTRING_UTF8,
                text.as_ptr().cast(),
                -1,
                -1,
                0,
            ) != 0
        }
    }

    /// `X509_NAME_get_index_by_NID(name, nid, -1) >= 0`.
    pub(crate) fn has(&self, nid: c_int) -> bool {
        // SAFETY: the name is live.
        unsafe { X509_NAME_get_index_by_NID(self.0.as_ptr(), nid, -1) >= 0 }
    }

    /// `X509_NAME_entry_count(name)`.
    pub(crate) fn len(&self) -> c_int {
        // SAFETY: the name is live.
        unsafe { sys::X509_NAME_entry_count(self.0.as_ptr()) }
    }
}

owned!(
    /// An `X509_REQ`, a certificate signing request.
    Csr,
    sys::X509_REQ,
    sys::X509_REQ_free
);

// SAFETY: a request is not shared with the library once made; moving
// the owner between threads moves the only handle.
unsafe impl Send for Csr {}

impl Csr {
    /// `X509_REQ_new()`.
    pub(crate) fn new() -> Option<Csr> {
        // SAFETY: X509_REQ_new takes nothing.
        Csr::owned(unsafe { sys::X509_REQ_new() })
    }

    /// `PEM_read_bio_X509_REQ(in, NULL, NULL, NULL)`.
    pub(crate) fn read_pem(bio: &Bio<'_>) -> Option<Csr> {
        // SAFETY: the BIO is live; no callback and no user data.
        let csr = unsafe {
            sys::PEM_read_bio_X509_REQ(bio.as_ptr(), ptr::null_mut(), None, ptr::null_mut())
        };
        Csr::owned(csr)
    }

    /// `X509_REQ_dup(csr)`: a copy a native owns while the object keeps
    /// its own.
    pub(crate) fn duplicate(&self) -> Option<Csr> {
        // SAFETY: the request is live.
        Csr::owned(unsafe { sys::X509_REQ_dup(self.as_ptr()) })
    }

    /// `X509_REQ_print(out, csr)`, nonzero.
    pub(crate) fn print(&self, out: &Bio<'_>) -> bool {
        // SAFETY: the sink and the request are live.
        unsafe { sys::X509_REQ_print(out.as_ptr(), self.as_ptr()) != 0 }
    }

    /// `PEM_write_bio_X509_REQ(out, csr)`, nonzero.
    pub(crate) fn write_pem(&self, out: &Bio<'_>) -> bool {
        // SAFETY: the sink and the request are live.
        unsafe { sys::PEM_write_bio_X509_REQ(out.as_ptr(), self.as_ptr()) != 0 }
    }

    /// `X509_REQ_get_subject_name(csr)`.
    pub(crate) fn subject(&self) -> Name<'_> {
        // SAFETY: the request is live; the name is its own.
        Name::of(unsafe { sys::X509_REQ_get_subject_name(self.as_ptr()) })
    }

    /// `X509_REQ_get_pubkey(csr)`: a new reference to its key.
    pub(crate) fn public_key(&self) -> Option<PKey> {
        // SAFETY: the request is live.
        PKey::owned(unsafe { sys::X509_REQ_get_pubkey(self.as_ptr()) })
    }

    /// `X509_REQ_verify(csr, key)`: 1, 0, or negative for an error.
    pub(crate) fn verify(&self, key: &PKey) -> c_int {
        // SAFETY: the request and key are live.
        unsafe { sys::X509_REQ_verify(self.as_ptr(), key.as_ptr()) }
    }

    /// `X509_REQ_set_version(csr, 0)`, nonzero.
    pub(crate) fn set_version_1(&mut self) -> bool {
        // SAFETY: the request is live.
        unsafe { sys::X509_REQ_set_version(self.as_ptr(), 0) != 0 }
    }

    /// `X509_REQ_set_pubkey(csr, key)`, nonzero.
    pub(crate) fn set_public_key(&mut self, key: &PKey) -> bool {
        // SAFETY: the request and key are live.
        unsafe { sys::X509_REQ_set_pubkey(self.as_ptr(), key.as_ptr()) != 0 }
    }

    /// `X509_REQ_add1_attr_by_NID(csr, nid, MBSTRING_UTF8, text, len)`.
    pub(crate) fn add_attribute(&mut self, nid: c_int, text: &[u8]) -> bool {
        let Some(len) = int_len(text.len()) else {
            return false;
        };
        // SAFETY: the request is live; the text is `len` bytes.
        unsafe {
            X509_REQ_add1_attr_by_NID(self.as_ptr(), nid, MBSTRING_UTF8, text.as_ptr(), len) != 0
        }
    }

    /// `X509_REQ_add1_attr_by_txt(csr, name, MBSTRING_UTF8, text, -1)`.
    pub(crate) fn add_attribute_by_text(&mut self, name: &CStr, text: &CStr) -> bool {
        // SAFETY: the request is live and both strings NUL-terminated.
        unsafe {
            X509_REQ_add1_attr_by_txt(
                self.as_ptr(),
                name.as_ptr(),
                MBSTRING_UTF8,
                text.as_ptr().cast(),
                -1,
            ) != 0
        }
    }

    /// `X509_REQ_get_attr_by_NID(csr, nid, -1) >= 0`.
    pub(crate) fn has_attribute(&self, nid: c_int) -> bool {
        // SAFETY: the request is live.
        unsafe { X509_REQ_get_attr_by_NID(self.as_ptr(), nid, -1) >= 0 }
    }

    /// `X509_REQ_sign(csr, key, md)`, nonzero.
    pub(crate) fn sign(&mut self, key: &PKey, md: &Digest) -> bool {
        // SAFETY: the request, key and digest are live.
        unsafe { sys::X509_REQ_sign(self.as_ptr(), key.as_ptr(), md.as_ptr()) != 0 }
    }
}

/// What a configuration section's extensions are added to.
pub(crate) enum Extended<'a> {
    /// `X509V3_set_ctx_test`: the section is checked and nothing changes.
    Test,
    /// A certificate `openssl_csr_sign` makes, from its request, under
    /// its issuer or itself.
    Certificate {
        issuer: Option<&'a Certificate>,
        cert: &'a Certificate,
        csr: &'a Csr,
    },
    /// A request `openssl_csr_new` makes.
    Request(&'a Csr),
}

/// `X509V3_set_ctx`, `X509V3_set_nconf` and `X509V3_EXT_add_nconf` or
/// `X509V3_EXT_REQ_add_nconf`: the section's extensions added, nonzero.
pub(crate) fn add_extensions(conf: &Conf, section: &CStr, to: &Extended<'_>) -> bool {
    let mut ctx = MaybeUninit::<sys::X509V3_CTX>::zeroed();
    let null = ptr::null_mut();
    let (issuer, subject, req, flags) = match to {
        Extended::Test => (null, null, ptr::null_mut(), X509V3_CTX_TEST),
        Extended::Certificate { issuer, cert, csr } => {
            let issuer = issuer.map_or(cert.as_ptr(), Certificate::as_ptr);
            (issuer, cert.as_ptr(), csr.as_ptr(), 0)
        }
        Extended::Request(csr) => (null, null, csr.as_ptr(), 0),
    };
    // SAFETY: the context is zeroed storage of its size, which set_ctx
    // fills; the objects are live or null.
    unsafe {
        sys::X509V3_set_ctx(
            ctx.as_mut_ptr(),
            issuer,
            subject,
            req,
            ptr::null_mut(),
            flags,
        );
    }
    // SAFETY: the context is set and the configuration live.
    unsafe { sys::X509V3_set_nconf(ctx.as_mut_ptr(), conf.as_ptr()) };
    let done = match to {
        Extended::Request(csr) => {
            // SAFETY: the configuration, context and request are live;
            // the section is NUL-terminated.
            unsafe {
                X509V3_EXT_REQ_add_nconf(
                    conf.as_ptr(),
                    ctx.as_mut_ptr(),
                    section.as_ptr(),
                    csr.as_ptr(),
                )
            }
        }
        // SAFETY: as above, with the certificate or none for the test.
        _ => unsafe {
            X509V3_EXT_add_nconf(conf.as_ptr(), ctx.as_mut_ptr(), section.as_ptr(), subject)
        },
    };
    done != 0
}

owned!(
    /// A `STACK_OF(X509)` that owns its certificates.
    CertStack,
    sys::OPENSSL_STACK,
    free_cert_stack
);

/// `sk_X509_pop_free(sk, X509_free)`.
unsafe fn free_cert_stack(stack: *mut sys::OPENSSL_STACK) {
    // SAFETY: the stack and its certificates are the caller's, freed once.
    unsafe { sys::OPENSSL_sk_pop_free(stack, Some(free_certificate)) };
}

impl CertStack {
    /// `sk_X509_new_null()`.
    pub(crate) fn new() -> Option<CertStack> {
        // SAFETY: OPENSSL_sk_new_null takes nothing.
        CertStack::owned(unsafe { sys::OPENSSL_sk_new_null() })
    }

    /// `sk_X509_push(sk, cert) > 0`, the stack taking the reference.
    pub(crate) fn push(&mut self, cert: Certificate) -> bool {
        let cert = ManuallyDrop::new(cert);
        // SAFETY: the stack is live; on success it owns the reference.
        let pushed = unsafe { sys::OPENSSL_sk_push(self.as_ptr(), cert.as_ptr().cast()) } > 0;
        if !pushed {
            drop(ManuallyDrop::into_inner(cert));
        }
        pushed
    }

    /// `sk_X509_num(sk)`.
    pub(crate) fn len(&self) -> usize {
        // SAFETY: the stack is live.
        usize::try_from(unsafe { sys::OPENSSL_sk_num(self.as_ptr()) }).unwrap_or(0)
    }
}

/// Each certificate of a list as PEM, `None` where one would not write.
pub(crate) type Pems = Vec<Option<Vec<u8>>>;

/// Each certificate of a stack as PEM, `None` where one would not write.
///
/// # Safety
///
/// `stack` is null or a live `STACK_OF(X509)`.
pub(in crate::ffi) unsafe fn pems_of(stack: *const sys::OPENSSL_STACK) -> Pems {
    if stack.is_null() {
        return Vec::new();
    }
    // SAFETY: the caller's live stack.
    let items = unsafe { stack_items(stack) };
    let pem = |cert: *mut c_void| {
        // SAFETY: each item is a live certificate.
        written(|out| unsafe { sys::PEM_write_bio_X509(out, cert.cast()) })
    };
    items.into_iter().map(pem).collect()
}

/// `php_openssl_load_all_certs_from_file` after the file is open:
/// `PEM_X509_INFO_read_bio_ex` and the certificates of what it read;
/// `None` where the file does not read.
pub(crate) fn read_all_certs(bio: &Bio<'_>) -> Option<CertStack> {
    /// `X509_INFO`, whose first member is the certificate.
    #[repr(C)]
    struct Info {
        x509: *mut sys::X509,
    }
    // SAFETY: the BIO is live; the stack made is freed below.
    let infos = unsafe {
        PEM_X509_INFO_read_bio_ex(
            bio.as_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        )
    };
    if infos.is_null() {
        return None;
    }
    let mut out = CertStack::new();
    // SAFETY: the stack is live.
    for info in unsafe { stack_items(infos) } {
        let info = info.cast::<Info>();
        // SAFETY: each item is a live X509_INFO; its certificate is taken
        // out of it before it is freed.
        let cert = unsafe { std::mem::replace(&mut (*info).x509, ptr::null_mut()) };
        if let (Some(stack), Some(cert)) = (out.as_mut(), Certificate::owned(cert)) {
            stack.push(cert);
        }
        // SAFETY: the info is freed once.
        unsafe { X509_INFO_free(info.cast()) };
    }
    // SAFETY: the stack's items are freed above; the stack is freed once.
    unsafe { sys::OPENSSL_sk_free(infos) };
    out
}

owned!(
    /// An `X509_STORE` of trusted certificates.
    Store,
    sys::X509_STORE,
    sys::X509_STORE_free
);

/// How [`Store::check`] ended.
pub(crate) enum Checked {
    /// `X509_STORE_CTX_new` failed.
    NoMemory,
    /// `X509_STORE_CTX_init` failed.
    InitFailed,
    /// `X509_verify_cert`'s answer.
    Verified(c_int),
}

impl Store {
    /// `X509_STORE_new()`.
    pub(crate) fn new() -> Option<Store> {
        // SAFETY: X509_STORE_new takes nothing.
        Store::owned(unsafe { sys::X509_STORE_new() })
    }

    /// `X509_STORE_add_lookup` of the file or directory method, then
    /// `X509_LOOKUP_load_file` or `X509_LOOKUP_add_dir` of `path` as PEM,
    /// or of the library's default where `path` is `None`.
    pub(crate) fn load(&mut self, dir: bool, path: Option<&CStr>) -> bool {
        let (method, cmd) = if dir {
            // SAFETY: the method table is static.
            (unsafe { sys::X509_LOOKUP_hash_dir() }, X509_L_ADD_DIR)
        } else {
            // SAFETY: as above.
            (unsafe { sys::X509_LOOKUP_file() }, X509_L_FILE_LOAD)
        };
        // SAFETY: the store is live; the lookup it answers is its own.
        let lookup = unsafe { sys::X509_STORE_add_lookup(self.as_ptr(), method) };
        if lookup.is_null() {
            return false;
        }
        let kind = if path.is_some() {
            X509_FILETYPE_PEM
        } else {
            X509_FILETYPE_DEFAULT
        };
        let path = path.map_or(ptr::null(), CStr::as_ptr);
        // SAFETY: the lookup is live; the path is null or NUL-terminated.
        unsafe { sys::X509_LOOKUP_ctrl(lookup, cmd, path, kind, ptr::null_mut()) != 0 }
    }

    /// `php_openssl_check_cert`'s library half.
    pub(crate) fn check(
        &self,
        cert: &Certificate,
        untrusted: Option<&CertStack>,
        purpose: c_int,
    ) -> Checked {
        // SAFETY: X509_STORE_CTX_new takes nothing.
        let Some(ctx) = NonNull::new(unsafe { sys::X509_STORE_CTX_new() }) else {
            return Checked::NoMemory;
        };
        let chain = untrusted.map_or(ptr::null_mut(), |s| s.as_ptr().cast());
        // SAFETY: the context, store and certificate are live; the chain is
        // live or null.
        let verdict = if unsafe {
            sys::X509_STORE_CTX_init(ctx.as_ptr(), self.as_ptr(), cert.as_ptr(), chain)
        } == 0
        {
            Checked::InitFailed
        } else {
            // SAFETY: the context is set up.
            if purpose >= 0 {
                // SAFETY: as above.
                unsafe { X509_STORE_CTX_set_purpose(ctx.as_ptr(), purpose) };
            }
            // SAFETY: as above.
            Checked::Verified(unsafe { sys::X509_verify_cert(ctx.as_ptr()) })
        };
        // SAFETY: the context is owned here and freed once.
        unsafe { sys::X509_STORE_CTX_free(ctx.as_ptr()) };
        verdict
    }
}
