//! The binding to OpenSSL's libcrypto (design §7.2, `unsafe.homes`'
//! `ext/*/src/ffi/`): the platform's OpenSSL 3, the major version the
//! oracle links, found by `openssl-sys` through `pkg-config`, and the one
//! home of this crate's `unsafe`. What `openssl-sys` does not declare is
//! declared below from the library's own headers.
//!
//! One owned type per OpenSSL object, each freeing what it owns on drop:
//! [`PKey`] (`EVP_PKEY`, one reference of its count), [`Bio`] (a memory or
//! file `BIO`), [`Certificate`] (`X509`), [`Digest`] (`EVP_MD`),
//! [`Cipher`] (`EVP_CIPHER`), [`DigestContext`] (`EVP_MD_CTX` with the
//! `EVP_PKEY_CTX` it owns), [`KeyContext`] (`EVP_PKEY_CTX`) and [`Conf`]
//! (`CONF`). Every call is made on the default `OSSL_LIB_CTX` with no
//! property query; the oracle's `openssl.libctx=custom` `OSSL_LIB_CTX` is one
//! loaded from the same configuration and has the same providers, so what
//! the natives answer does not follow from which one is used.
//!
//! The library's error queue is per thread, and a request runs on one
//! thread, so [`next_error`] drains what the calls of the native that asks
//! left behind, as `php_openssl_store_errors` does.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uchar, c_ulong, c_void};
use std::marker::PhantomData;
use std::ptr::{self, NonNull};

use openssl_sys as sys;

// SAFETY: libcrypto's own declarations, from `openssl/err.h`,
// `openssl/conf.h`, `openssl/evp.h`, `openssl/objects.h`,
// `openssl/asn1.h`, `openssl/x509.h`, `openssl/pem.h`, `openssl/bio.h`
// and `openssl/rand.h` of OpenSSL 3, which `openssl-sys` does not
// declare; the library `openssl-sys` links defines each.
unsafe extern "C" {
    fn ERR_set_mark() -> c_int;
    fn ERR_pop_to_mark() -> c_int;
    fn ERR_error_string_n(e: c_ulong, buf: *mut c_char, len: usize);
    fn NCONF_load(conf: *mut sys::CONF, file: *const c_char, eline: *mut c_long) -> c_int;
    fn NCONF_get_string(
        conf: *const sys::CONF,
        group: *const c_char,
        name: *const c_char,
    ) -> *mut c_char;
    fn NCONF_get_number_e(
        conf: *const sys::CONF,
        group: *const c_char,
        name: *const c_char,
        result: *mut c_long,
    ) -> c_int;
    fn EVP_PKEY_get_base_id(pkey: *const sys::EVP_PKEY) -> c_int;
    fn EVP_PKEY_get0_type_name(pkey: *const sys::EVP_PKEY) -> *const c_char;
    fn EVP_PKEY_type(nid: c_int) -> c_int;
    fn OBJ_txt2nid(s: *const c_char) -> c_int;
    fn EVP_PKEY_CTX_new_from_pkey(
        libctx: *mut sys::OSSL_LIB_CTX,
        pkey: *mut sys::EVP_PKEY,
        propquery: *const c_char,
    ) -> *mut sys::EVP_PKEY_CTX;
    fn EVP_PKEY_CTX_set_ec_param_enc(ctx: *mut sys::EVP_PKEY_CTX, param_enc: c_int) -> c_int;
    fn ASN1_STRING_set_default_mask_asc(p: *const c_char) -> c_int;
    fn X509_get_default_cert_area() -> *const c_char;
    fn BIO_new_file(filename: *const c_char, mode: *const c_char) -> *mut sys::BIO;
    fn BIO_free(a: *mut sys::BIO) -> c_int;
    // `PEM_ASN1_read_bio` takes a `d2i_of_void *`; every `d2i_*` has the
    // same ABI, and the one this crate hands it is `d2i_X509`'s type.
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
    fn RAND_file_name(buf: *mut c_char, num: usize) -> *const c_char;
    fn RAND_load_file(file: *const c_char, max_bytes: c_long) -> c_int;
    fn RAND_write_file(file: *const c_char) -> c_int;
}

/// `EVP_PKEY_KEYMGMT`: the id of a key a provider holds with no legacy
/// method, whose type is read off its name.
const EVP_PKEY_KEYMGMT: c_int = -1;

/// `OPENSSL_EC_NAMED_CURVE`: an EC key's parameters as the curve's name.
const OPENSSL_EC_NAMED_CURVE: c_int = 1;

/// `ERR_get_error()`: the oldest code on this thread's queue, taken off
/// it, or `None` once the queue is empty.
pub(crate) fn next_error() -> Option<c_ulong> {
    // SAFETY: ERR_get_error reads and pops this thread's queue; it takes
    // nothing.
    let code = unsafe { sys::ERR_get_error() };
    (code != 0).then_some(code)
}

/// `ERR_error_string_n(code, buf, 256)`: the library's text for a code,
/// `error:0480006C:PEM routines::no start line` and its kin, cut at 255
/// bytes as php's buffer cuts it.
pub(crate) fn error_text(code: c_ulong) -> Vec<u8> {
    let mut buf = [0 as c_char; 256];
    // SAFETY: the buffer is 256 bytes and the length says so; the library
    // writes at most that many, NUL included.
    unsafe { ERR_error_string_n(code, buf.as_mut_ptr(), buf.len()) };
    // SAFETY: ERR_error_string_n NUL-terminates within the length given.
    let text = unsafe { CStr::from_ptr(buf.as_ptr()) };
    text.to_bytes().to_vec()
}

/// `ERR_set_mark()` and `ERR_pop_to_mark()` around `run`: what the calls
/// in it queue is dropped, as php's optional configuration reads drop it.
fn quietly<T>(run: impl FnOnce() -> T) -> T {
    // SAFETY: ERR_set_mark marks this thread's queue; it takes nothing.
    unsafe { ERR_set_mark() };
    let out = run();
    // SAFETY: ERR_pop_to_mark pops this thread's queue to the mark set
    // above; it takes nothing.
    unsafe { ERR_pop_to_mark() };
    out
}

/// A C string the library hands out and keeps, copied.
///
/// # Safety
///
/// `text` is null or a NUL-terminated string that lives for the call.
unsafe fn copied(text: *const c_char) -> Option<Vec<u8>> {
    if text.is_null() {
        return None;
    }
    // SAFETY: the caller's contract: non-null and NUL-terminated.
    let text = unsafe { CStr::from_ptr(text) };
    Some(text.to_bytes().to_vec())
}

/// `OpenSSL_version(OPENSSL_VERSION)`: `OpenSSL 3.6.4 25 Aug 2026`.
pub(crate) fn version_text() -> Vec<u8> {
    // SAFETY: OpenSSL_version answers a pointer into a static string.
    let text = unsafe { sys::OpenSSL_version(sys::OPENSSL_VERSION) };
    // SAFETY: the static string is NUL-terminated and never freed.
    unsafe { copied(text) }.unwrap_or_default()
}

/// `OpenSSL_version_num()`: `0xMNN00PP0L` of the linked library.
pub(crate) fn version_number() -> i64 {
    // SAFETY: OpenSSL_version_num takes nothing and answers a number.
    let number = unsafe { sys::OpenSSL_version_num() };
    i64::try_from(number).unwrap_or(i64::MAX)
}

/// `X509_get_default_cert_area()`: the directory the library was
/// configured with, where `openssl.cnf` is looked for by default.
pub(crate) fn default_cert_area() -> Vec<u8> {
    // SAFETY: X509_get_default_cert_area answers a static string.
    let text = unsafe { X509_get_default_cert_area() };
    // SAFETY: the static string is NUL-terminated and never freed.
    unsafe { copied(text) }.unwrap_or_default()
}

/// `OBJ_sn2nid(name)`: the NID of a short name, `NID_undef` (0) for one
/// the library does not know.
pub(crate) fn short_name_nid(name: &CStr) -> c_int {
    // SAFETY: the name is a NUL-terminated string for the call.
    unsafe { sys::OBJ_sn2nid(name.as_ptr()) }
}

/// `OBJ_obj2txt(buf, 80, OBJ_nid2obj(nid), 1)`: the dotted OID of a NID,
/// in the 80 bytes OpenSSL recommends.
pub(crate) fn nid_oid(nid: c_int) -> Option<Vec<u8>> {
    // SAFETY: OBJ_nid2obj takes any NID and answers a new object or null.
    let object = unsafe { sys::OBJ_nid2obj(nid) };
    if object.is_null() {
        return None;
    }
    let mut buf = [0 as c_char; 80];
    let size = c_int::try_from(buf.len()).unwrap_or(0);
    // SAFETY: the object is live, and the buffer is 80 bytes as the size
    // says; the library writes at most that many, NUL included.
    let len = unsafe { sys::OBJ_obj2txt(buf.as_mut_ptr(), size, object, 1) };
    // SAFETY: the object came from OBJ_nid2obj above and is freed once.
    unsafe { sys::ASN1_OBJECT_free(object) };
    let len = usize::try_from(len).unwrap_or(0).min(buf.len());
    Some(buf[..len].iter().map(|&c| c.to_ne_bytes()[0]).collect())
}

/// `ASN1_STRING_set_default_mask_asc(mask)`: the process's default string
/// mask, which a configuration's `string_mask` sets; false for a mask
/// the library refuses.
pub(crate) fn set_default_string_mask(mask: &CStr) -> bool {
    // SAFETY: the mask is a NUL-terminated string for the call.
    unsafe { ASN1_STRING_set_default_mask_asc(mask.as_ptr()) != 0 }
}

/// `RAND_file_name(buf, MAXPATHLEN)`: the seed file the library names,
/// `$RANDFILE` or `$HOME/.rnd`.
pub(crate) fn rand_file_name() -> Option<CString> {
    let mut buf = [0 as c_char; 1024];
    // SAFETY: the buffer is 1024 bytes and the length says so; the
    // library answers the buffer or null.
    let name = unsafe { RAND_file_name(buf.as_mut_ptr(), buf.len()) };
    // SAFETY: a non-null answer is the buffer, NUL-terminated.
    unsafe { copied(name) }.and_then(|name| CString::new(name).ok())
}

/// `RAND_load_file(file, -1)`: the bytes read into the generator's
/// state, negative for a file it could not read.
pub(crate) fn rand_load_file(file: &CStr) -> c_int {
    // SAFETY: the name is a NUL-terminated string for the call.
    unsafe { RAND_load_file(file.as_ptr(), -1) }
}

/// `RAND_write_file(file)`: negative for a file it could not write.
pub(crate) fn rand_write_file(file: &CStr) -> c_int {
    // SAFETY: the name is a NUL-terminated string for the call.
    unsafe { RAND_write_file(file.as_ptr()) }
}

/// `RAND_status()`: whether the generator is seeded.
pub(crate) fn rand_status() -> bool {
    // SAFETY: RAND_status takes nothing.
    unsafe { sys::RAND_status() == 1 }
}

/// A `BIO`: a read-only view of the caller's bytes, a memory sink, or a
/// file, freed on drop. The lifetime is the bytes a view reads.
pub(crate) struct Bio<'a>(NonNull<sys::BIO>, PhantomData<&'a [u8]>);

impl<'a> Bio<'a> {
    /// `BIO_new_mem_buf(bytes, len)`: the bytes, read in place; `None`
    /// past `INT_MAX` bytes, where php's `(int)` cast would ask for
    /// `strlen` of them instead.
    pub(crate) fn reading(bytes: &'a [u8]) -> Option<Bio<'a>> {
        let len = c_int::try_from(bytes.len()).ok()?;
        // SAFETY: the pointer and length are the slice's, and the BIO
        // reads them only while it lives, which the lifetime ties to them.
        let bio = unsafe { sys::BIO_new_mem_buf(bytes.as_ptr().cast(), len) };
        NonNull::new(bio).map(|bio| Bio(bio, PhantomData))
    }

    /// `BIO_new(BIO_s_mem())`: an empty memory sink.
    pub(crate) fn memory() -> Option<Bio<'static>> {
        // SAFETY: BIO_s_mem takes nothing and answers the static method
        // table.
        let method = unsafe { sys::BIO_s_mem() };
        // SAFETY: the method table is the library's own; BIO_new makes a
        // BIO of it or answers null.
        let bio = unsafe { sys::BIO_new(method) };
        NonNull::new(bio).map(|bio| Bio(bio, PhantomData))
    }

    /// `BIO_new_file(path, "rb")`: the file, opened by the C library.
    pub(crate) fn file(path: &CStr) -> Option<Bio<'static>> {
        // SAFETY: both strings are NUL-terminated for the call.
        let bio = unsafe { BIO_new_file(path.as_ptr(), c"rb".as_ptr()) };
        NonNull::new(bio).map(|bio| Bio(bio, PhantomData))
    }

    /// `BIO_get_mem_data`: what a memory sink holds.
    pub(crate) fn contents(&self) -> Vec<u8> {
        let mut data: *mut c_char = ptr::null_mut();
        // SAFETY: the BIO is live; for a memory BIO the control call
        // stores a pointer to its bytes and answers their length.
        let len = unsafe { sys::BIO_get_mem_data(self.0.as_ptr(), &raw mut data) };
        let Ok(len) = usize::try_from(len) else {
            return Vec::new();
        };
        if data.is_null() || len == 0 {
            return Vec::new();
        }
        // SAFETY: the BIO's own buffer, `len` bytes, live while the BIO
        // is and not written while this borrow reads it.
        let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
        bytes.to_vec()
    }

    fn as_ptr(&self) -> *mut sys::BIO {
        self.0.as_ptr()
    }
}

impl Drop for Bio<'_> {
    fn drop(&mut self) {
        // SAFETY: the BIO is owned here and freed once.
        unsafe { BIO_free(self.0.as_ptr()) };
    }
}

/// The passphrase `php_openssl_pem_password_cb` copies out, cut to the
/// buffer the library offers.
unsafe extern "C" fn password(
    buf: *mut c_char,
    size: c_int,
    _rwflag: c_int,
    userdata: *mut c_void,
) -> c_int {
    if userdata.is_null() {
        return -1;
    }
    // SAFETY: the library hands back the pointer `PKey::read_private`
    // gave it, a `&[u8]` live for the read.
    let phrase = unsafe { &*userdata.cast::<&[u8]>() };
    let len = usize::try_from(size).unwrap_or(0).min(phrase.len());
    // SAFETY: the library's buffer holds `size` bytes, and `len` is at
    // most that; the passphrase holds at least `len`.
    unsafe { ptr::copy_nonoverlapping(phrase.as_ptr(), buf.cast::<u8>(), len) };
    c_int::try_from(len).unwrap_or(0)
}

/// The kind of an asymmetric key, `EVP_PKEY_base_id` as
/// `php_openssl_pkey_get_details` reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyKind {
    Rsa,
    Dsa,
    Dh,
    Ec,
    X25519,
    Ed25519,
    X448,
    Ed448,
    Other,
}

impl KeyKind {
    fn of(nid: c_int) -> KeyKind {
        match nid {
            sys::EVP_PKEY_RSA => KeyKind::Rsa,
            sys::EVP_PKEY_DSA => KeyKind::Dsa,
            sys::EVP_PKEY_DH => KeyKind::Dh,
            sys::EVP_PKEY_EC => KeyKind::Ec,
            sys::EVP_PKEY_X25519 => KeyKind::X25519,
            sys::EVP_PKEY_ED25519 => KeyKind::Ed25519,
            sys::EVP_PKEY_X448 => KeyKind::X448,
            sys::EVP_PKEY_ED448 => KeyKind::Ed448,
            _ => KeyKind::Other,
        }
    }

    /// The name `EVP_PKEY_CTX_new_from_name` makes a key of this kind by.
    fn name(self) -> &'static CStr {
        match self {
            KeyKind::Rsa => c"RSA",
            KeyKind::Dsa => c"DSA",
            KeyKind::Dh => c"DH",
            KeyKind::Ec => c"EC",
            KeyKind::X25519 => c"X25519",
            KeyKind::Ed25519 => c"ED25519",
            KeyKind::X448 => c"X448",
            KeyKind::Ed448 => c"ED448",
            KeyKind::Other => c"",
        }
    }
}

/// An `EVP_PKEY`: one reference of the library's count, dropped with it.
pub(crate) struct PKey(NonNull<sys::EVP_PKEY>);

// SAFETY: an EVP_PKEY's reference count is atomic, and a key is not
// changed after it is made; OpenSSL 3 shares keys across threads.
unsafe impl Send for PKey {}

impl Clone for PKey {
    /// `EVP_PKEY_up_ref`: a second reference to the same key.
    fn clone(&self) -> PKey {
        // SAFETY: the key is live; up_ref counts one more owner, the
        // value returned here.
        unsafe { sys::EVP_PKEY_up_ref(self.0.as_ptr()) };
        PKey(self.0)
    }
}

impl Drop for PKey {
    fn drop(&mut self) {
        // SAFETY: this value owns one reference and gives it back once.
        unsafe { sys::EVP_PKEY_free(self.0.as_ptr()) };
    }
}

impl PKey {
    fn owned(key: *mut sys::EVP_PKEY) -> Option<PKey> {
        NonNull::new(key).map(PKey)
    }

    /// `PEM_read_bio_PrivateKey(in, NULL, cb, &password)`, or with no
    /// callback where php hands none, a `NULL` passphrase.
    pub(crate) fn read_private(bio: &Bio<'_>, passphrase: Option<&[u8]>) -> Option<PKey> {
        let key = match passphrase {
            Some(mut phrase) => {
                let userdata = (&raw mut phrase).cast::<c_void>();
                // SAFETY: the BIO is live; the callback reads `userdata`,
                // a `&[u8]` that outlives the call.
                unsafe {
                    sys::PEM_read_bio_PrivateKey(
                        bio.as_ptr(),
                        ptr::null_mut(),
                        Some(password),
                        userdata,
                    )
                }
            }
            // SAFETY: the BIO is live; no callback and no user data are
            // the library's defaults.
            None => unsafe {
                sys::PEM_read_bio_PrivateKey(bio.as_ptr(), ptr::null_mut(), None, ptr::null_mut())
            },
        };
        PKey::owned(key)
    }

    /// `PEM_read_bio_PUBKEY(in, NULL, NULL, NULL)`.
    pub(crate) fn read_public(bio: &Bio<'_>) -> Option<PKey> {
        // SAFETY: the BIO is live; no callback and no user data.
        let key = unsafe {
            sys::PEM_read_bio_PUBKEY(bio.as_ptr(), ptr::null_mut(), None, ptr::null_mut())
        };
        PKey::owned(key)
    }

    /// `PEM_write_bio_PrivateKey(out, key, cipher, pass, len, NULL,
    /// NULL)` into a memory sink, encrypted under a cipher and a
    /// passphrase or not at all: the PEM, or `None` where the library
    /// refused.
    pub(crate) fn private_pem(&self, encryption: Option<(&Cipher, &[u8])>) -> Option<Vec<u8>> {
        let out = Bio::memory()?;
        let (cipher, kstr, len) = match encryption {
            Some((cipher, phrase)) => (
                cipher.0.as_ptr().cast_const(),
                phrase.as_ptr(),
                c_int::try_from(phrase.len()).ok()?,
            ),
            None => (ptr::null(), ptr::null(), 0),
        };
        // SAFETY: the sink, the key and the cipher are live; the
        // passphrase is `len` bytes read for the call alone.
        let written = unsafe {
            sys::PEM_write_bio_PrivateKey(
                out.as_ptr(),
                self.0.as_ptr(),
                cipher,
                kstr,
                len,
                None,
                ptr::null_mut(),
            )
        };
        (written != 0).then(|| out.contents())
    }

    /// `PEM_write_bio_PUBKEY(out, key)` into a memory sink.
    pub(crate) fn public_pem(&self) -> Option<Vec<u8>> {
        let out = Bio::memory()?;
        // SAFETY: the sink and the key are live.
        let written = unsafe { sys::PEM_write_bio_PUBKEY(out.as_ptr(), self.0.as_ptr()) };
        (written != 0).then(|| out.contents())
    }

    /// `EVP_PKEY_bits(key)`.
    pub(crate) fn bits(&self) -> i64 {
        // SAFETY: the key is live.
        i64::from(unsafe { sys::EVP_PKEY_get_bits(self.0.as_ptr()) })
    }

    /// The key's kind as `php_openssl_pkey_get_details` reads it: the
    /// base id of a legacy key, or the kind its type name names for one a
    /// provider holds.
    pub(crate) fn kind(&self) -> KeyKind {
        // SAFETY: the key is live.
        let id = unsafe { sys::EVP_PKEY_get_id(self.0.as_ptr()) };
        if id != EVP_PKEY_KEYMGMT {
            // SAFETY: the key is live.
            return KeyKind::of(unsafe { EVP_PKEY_get_base_id(self.0.as_ptr()) });
        }
        // SAFETY: the key is live; the name, when there is one, is the
        // key's own static string.
        let name = unsafe { EVP_PKEY_get0_type_name(self.0.as_ptr()) };
        if name.is_null() {
            return KeyKind::Other;
        }
        // SAFETY: a non-null name is NUL-terminated.
        let nid = unsafe { OBJ_txt2nid(name) };
        if nid == sys::NID_undef {
            return KeyKind::Other;
        }
        // SAFETY: EVP_PKEY_type takes any NID.
        KeyKind::of(unsafe { EVP_PKEY_type(nid) })
    }

    /// `EVP_PKEY_type(EVP_PKEY_id(key))`: the kind of the key's legacy
    /// id, [`KeyKind::Other`] for one a provider holds with none.
    pub(crate) fn legacy_kind(&self) -> KeyKind {
        // SAFETY: the key is live.
        let id = unsafe { sys::EVP_PKEY_get_id(self.0.as_ptr()) };
        // SAFETY: EVP_PKEY_type takes any NID.
        KeyKind::of(unsafe { EVP_PKEY_type(id) })
    }

    /// `EVP_PKEY_get_bn_param(key, name, &bn)` and `BN_bn2bin`: the
    /// parameter's big-endian bytes, or `None` for one the key lacks.
    pub(crate) fn bn_param(&self, name: &CStr) -> Option<Vec<u8>> {
        let mut bn: *mut sys::BIGNUM = ptr::null_mut();
        // SAFETY: the key and the name are live; on success the library
        // stores a new BIGNUM this function frees.
        let found =
            unsafe { sys::EVP_PKEY_get_bn_param(self.0.as_ptr(), name.as_ptr(), &raw mut bn) };
        if found <= 0 || bn.is_null() {
            return None;
        }
        // SAFETY: the BIGNUM is the one the library just made.
        let bits = unsafe { sys::BN_num_bits(bn) };
        let len = usize::try_from(bits).unwrap_or(0).div_ceil(8);
        let mut out = vec![0u8; len];
        // SAFETY: the buffer holds BN_num_bytes(bn) bytes, all BN_bn2bin
        // writes.
        unsafe { sys::BN_bn2bin(bn, out.as_mut_ptr()) };
        // SAFETY: the BIGNUM is owned here and freed once.
        unsafe { sys::BN_free(bn) };
        Some(out)
    }

    /// `EVP_PKEY_get_utf8_string_param(key, name, buf, 64, &len)`.
    pub(crate) fn utf8_param(&self, name: &CStr) -> Option<Vec<u8>> {
        let mut buf = [0 as c_char; 64];
        let mut len = 0usize;
        // SAFETY: the key and the name are live; the buffer is 64 bytes
        // as the size says.
        let found = unsafe {
            sys::EVP_PKEY_get_utf8_string_param(
                self.0.as_ptr(),
                name.as_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut len,
            )
        };
        (found > 0).then(|| {
            buf[..len.min(buf.len())]
                .iter()
                .map(|&c| c.to_ne_bytes()[0])
                .collect()
        })
    }

    /// `EVP_PKEY_get_octet_string_param(key, name, buf, 64, &len)`.
    pub(crate) fn octet_param(&self, name: &CStr) -> Option<Vec<u8>> {
        let mut buf = [0u8; 64];
        let mut len = 0usize;
        // SAFETY: the key and the name are live; the buffer is 64 bytes
        // as the size says.
        let found = unsafe {
            sys::EVP_PKEY_get_octet_string_param(
                self.0.as_ptr(),
                name.as_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut len,
            )
        };
        (found > 0).then(|| buf[..len.min(buf.len())].to_vec())
    }
}

/// An `X509` certificate, freed on drop.
pub(crate) struct Certificate(NonNull<sys::X509>);

impl Drop for Certificate {
    fn drop(&mut self) {
        // SAFETY: the certificate is owned here and freed once.
        unsafe { sys::X509_free(self.0.as_ptr()) };
    }
}

impl Certificate {
    /// `PEM_read_bio_X509(in, NULL, NULL, NULL)`: any of the PEM
    /// certificate labels, read from a file.
    pub(crate) fn read_pem(bio: &Bio<'_>) -> Option<Certificate> {
        // SAFETY: the BIO is live; no callback and no user data.
        let cert =
            unsafe { sys::PEM_read_bio_X509(bio.as_ptr(), ptr::null_mut(), None, ptr::null_mut()) };
        NonNull::new(cert).map(Certificate)
    }

    /// `PEM_ASN1_read_bio(d2i_X509, PEM_STRING_X509, in, NULL, NULL,
    /// NULL)`: `CERTIFICATE` alone, read from a string.
    pub(crate) fn read_pem_certificate(bio: &Bio<'_>) -> Option<Certificate> {
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
        NonNull::new(cert.cast::<sys::X509>()).map(Certificate)
    }

    /// `X509_get_pubkey(cert)`: a new reference to its key.
    pub(crate) fn public_key(&self) -> Option<PKey> {
        // SAFETY: the certificate is live; the key comes back with a
        // reference of its own, or null.
        PKey::owned(unsafe { sys::X509_get_pubkey(self.0.as_ptr()) })
    }
}

/// An `EVP_MD`, as `php_openssl_get_evp_md_by_name` finds it and
/// `php_openssl_release_evp_md` gives it back: `EVP_MD_free` is a no-op
/// on the library's static tables and frees a fetched one.
pub(crate) struct Digest(NonNull<sys::EVP_MD>);

impl Drop for Digest {
    fn drop(&mut self) {
        // SAFETY: the digest is owned here and given back once.
        unsafe { sys::EVP_MD_free(self.0.as_ptr()) };
    }
}

impl Digest {
    /// `OBJ_NAME_get(name, OBJ_NAME_TYPE_MD_METH)`, then
    /// `EVP_MD_fetch(NULL, name, NULL)`.
    pub(crate) fn by_name(name: &CStr) -> Option<Digest> {
        // SAFETY: the name is NUL-terminated; the answer is a static table
        // or null.
        let found = unsafe { sys::EVP_get_digestbyname(name.as_ptr()) };
        if let Some(found) = NonNull::new(found.cast_mut()) {
            return Some(Digest(found));
        }
        // SAFETY: the name is NUL-terminated; the answer is a new
        // reference or null.
        let fetched = unsafe { sys::EVP_MD_fetch(ptr::null_mut(), name.as_ptr(), ptr::null()) };
        NonNull::new(fetched).map(Digest)
    }
}

/// An `EVP_CIPHER`, found as `php_openssl_get_evp_cipher_by_name` finds
/// it and given back with `EVP_CIPHER_free`, a no-op on a static one.
pub(crate) struct Cipher(NonNull<sys::EVP_CIPHER>);

impl Drop for Cipher {
    fn drop(&mut self) {
        // SAFETY: the cipher is owned here and given back once.
        unsafe { sys::EVP_CIPHER_free(self.0.as_ptr()) };
    }
}

impl Cipher {
    /// `OBJ_NAME_get(name, OBJ_NAME_TYPE_CIPHER_METH)`, then
    /// `EVP_CIPHER_fetch(NULL, name, NULL)`.
    pub(crate) fn by_name(name: &CStr) -> Option<Cipher> {
        // SAFETY: the name is NUL-terminated; the answer is a static table
        // or null.
        let found = unsafe { sys::EVP_get_cipherbyname(name.as_ptr()) };
        if let Some(found) = NonNull::new(found.cast_mut()) {
            return Some(Cipher(found));
        }
        // SAFETY: the name is NUL-terminated; the answer is a new
        // reference or null.
        let fetched = unsafe { sys::EVP_CIPHER_fetch(ptr::null_mut(), name.as_ptr(), ptr::null()) };
        NonNull::new(fetched).map(Cipher)
    }

    /// `EVP_des_ede3_cbc()`, the cipher a passphrase encrypts a private
    /// key with when the options name none.
    pub(crate) fn des_ede3_cbc() -> Option<Cipher> {
        // SAFETY: the answer is the library's static table.
        let cipher = unsafe { sys::EVP_des_ede3_cbc() };
        NonNull::new(cipher.cast_mut()).map(Cipher)
    }
}

/// An `EVP_MD_CTX` set up to sign or verify with one key, and the
/// `EVP_PKEY_CTX` the set-up made, which the `EVP_MD_CTX` owns.
pub(crate) struct DigestContext {
    md: NonNull<sys::EVP_MD_CTX>,
    key: *mut sys::EVP_PKEY_CTX,
}

impl Drop for DigestContext {
    fn drop(&mut self) {
        // SAFETY: the context is owned here and freed once, and with it
        // the key context it made.
        unsafe { sys::EVP_MD_CTX_free(self.md.as_ptr()) };
    }
}

/// Which way a [`DigestContext`] is set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Sign,
    Verify,
}

impl DigestContext {
    /// `EVP_MD_CTX_create()` and `EVP_DigestSignInit` or
    /// `EVP_DigestVerifyInit(ctx, &pctx, md, NULL, key)`: `None` where the
    /// library refused, its reasons on the queue.
    pub(crate) fn new(
        direction: Direction,
        md: Option<&Digest>,
        key: &PKey,
    ) -> Option<DigestContext> {
        // SAFETY: EVP_MD_CTX_new takes nothing and answers a context or
        // null.
        let md_ctx = NonNull::new(unsafe { sys::EVP_MD_CTX_new() })?;
        let mut ctx = DigestContext {
            md: md_ctx,
            key: ptr::null_mut(),
        };
        let md = md.map_or(ptr::null(), |md| md.0.as_ptr().cast_const());
        let ok = match direction {
            // SAFETY: the context, the digest (or none) and the key are
            // live; the library takes its own reference to the key and
            // stores the key context it makes in `ctx.key`.
            Direction::Sign => unsafe {
                sys::EVP_DigestSignInit(
                    ctx.md.as_ptr(),
                    &raw mut ctx.key,
                    md,
                    ptr::null_mut(),
                    key.0.as_ptr(),
                )
            },
            // SAFETY: as for signing.
            Direction::Verify => unsafe {
                sys::EVP_DigestVerifyInit(
                    ctx.md.as_ptr(),
                    &raw mut ctx.key,
                    md,
                    ptr::null_mut(),
                    key.0.as_ptr(),
                )
            },
        };
        (ok > 0).then_some(ctx)
    }

    /// `EVP_PKEY_CTX_set_rsa_padding(pctx, RSA_PKCS1_PSS_PADDING)`.
    pub(crate) fn set_pss_padding(&mut self) -> bool {
        if self.key.is_null() {
            return false;
        }
        // SAFETY: the key context is the one the init made, owned by the
        // digest context, which is live.
        unsafe { sys::EVP_PKEY_CTX_set_rsa_padding(self.key, sys::RSA_PKCS1_PSS_PADDING) > 0 }
    }

    /// `EVP_DigestSign` twice, once for the length and once for the
    /// signature: `None` where the library refused.
    pub(crate) fn sign(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        let mut len = 0usize;
        // SAFETY: the context is set up to sign; a null signature buffer
        // asks for the length, which the library stores in `len`.
        let sized = unsafe {
            sys::EVP_DigestSign(
                self.md.as_ptr(),
                ptr::null_mut(),
                &raw mut len,
                data.as_ptr(),
                data.len(),
            )
        };
        if sized <= 0 {
            return None;
        }
        let mut out = vec![0u8; len];
        // SAFETY: the buffer holds `len` bytes, the most the library said
        // it writes; it stores what it wrote in `len`.
        let signed = unsafe {
            sys::EVP_DigestSign(
                self.md.as_ptr(),
                out.as_mut_ptr(),
                &raw mut len,
                data.as_ptr(),
                data.len(),
            )
        };
        if signed <= 0 {
            return None;
        }
        out.truncate(len);
        Some(out)
    }

    /// `EVP_DigestVerify(ctx, sig, siglen, data, len)`: 1 for a match, 0
    /// for none, negative for an error.
    pub(crate) fn verify(&mut self, signature: &[u8], data: &[u8]) -> c_int {
        // SAFETY: the context is set up to verify; both slices are read
        // for the call alone.
        unsafe {
            sys::EVP_DigestVerify(
                self.md.as_ptr(),
                signature.as_ptr(),
                signature.len(),
                data.as_ptr(),
                data.len(),
            )
        }
    }
}

/// An `EVP_PKEY_CTX` that generates parameters or keys, freed on drop.
pub(crate) struct KeyContext(NonNull<sys::EVP_PKEY_CTX>);

impl Drop for KeyContext {
    fn drop(&mut self) {
        // SAFETY: the context is owned here and freed once.
        unsafe { sys::EVP_PKEY_CTX_free(self.0.as_ptr()) };
    }
}

impl KeyContext {
    /// `EVP_PKEY_CTX_new_from_name(NULL, name, NULL)`.
    pub(crate) fn of_kind(kind: KeyKind) -> Option<KeyContext> {
        // SAFETY: the name is NUL-terminated; the answer is a new context
        // or null.
        let ctx = unsafe {
            sys::EVP_PKEY_CTX_new_from_name(ptr::null_mut(), kind.name().as_ptr(), ptr::null())
        };
        NonNull::new(ctx).map(KeyContext)
    }

    /// `EVP_PKEY_CTX_new_from_pkey(NULL, params, NULL)`: an `EVP_PKEY_CTX` that
    /// makes keys over generated parameters.
    pub(crate) fn of_params(params: &PKey) -> Option<KeyContext> {
        // SAFETY: the parameters are live; the context takes its own
        // reference to them.
        let ctx =
            unsafe { EVP_PKEY_CTX_new_from_pkey(ptr::null_mut(), params.0.as_ptr(), ptr::null()) };
        NonNull::new(ctx).map(KeyContext)
    }

    /// `EVP_PKEY_paramgen_init(ctx) > 0`.
    pub(crate) fn paramgen_init(&mut self) -> bool {
        // SAFETY: the context is live.
        unsafe { sys::EVP_PKEY_paramgen_init(self.0.as_ptr()) > 0 }
    }

    /// `EVP_PKEY_CTX_set_dsa_paramgen_bits(ctx, bits) > 0`.
    pub(crate) fn set_dsa_bits(&mut self, bits: c_int) -> bool {
        // SAFETY: the context is live and set up for parameters.
        unsafe { sys::EVP_PKEY_CTX_set_dsa_paramgen_bits(self.0.as_ptr(), bits) > 0 }
    }

    /// `EVP_PKEY_CTX_set_dh_paramgen_prime_len(ctx, bits) > 0`.
    pub(crate) fn set_dh_prime_len(&mut self, bits: c_int) -> bool {
        // SAFETY: the context is live and set up for parameters.
        unsafe { sys::EVP_PKEY_CTX_set_dh_paramgen_prime_len(self.0.as_ptr(), bits) > 0 }
    }

    /// `EVP_PKEY_CTX_set_ec_paramgen_curve_nid(ctx, nid) > 0` and
    /// `EVP_PKEY_CTX_set_ec_param_enc(ctx, OPENSSL_EC_NAMED_CURVE) > 0`.
    pub(crate) fn set_ec_curve(&mut self, nid: c_int) -> bool {
        // SAFETY: the context is live and set up for parameters.
        let curve = unsafe { sys::EVP_PKEY_CTX_set_ec_paramgen_curve_nid(self.0.as_ptr(), nid) };
        // SAFETY: as above.
        curve > 0
            && unsafe { EVP_PKEY_CTX_set_ec_param_enc(self.0.as_ptr(), OPENSSL_EC_NAMED_CURVE) } > 0
    }

    /// `EVP_PKEY_paramgen(ctx, &params)`.
    pub(crate) fn paramgen(&mut self) -> Option<PKey> {
        let mut params: *mut sys::EVP_PKEY = ptr::null_mut();
        // SAFETY: the context is live; on success the library stores a
        // new key this value takes.
        let ok = unsafe { sys::EVP_PKEY_paramgen(self.0.as_ptr(), &raw mut params) };
        if ok <= 0 {
            return None;
        }
        PKey::owned(params)
    }

    /// `EVP_PKEY_keygen_init(ctx) > 0`.
    pub(crate) fn keygen_init(&mut self) -> bool {
        // SAFETY: the context is live.
        unsafe { sys::EVP_PKEY_keygen_init(self.0.as_ptr()) > 0 }
    }

    /// `EVP_PKEY_CTX_set_rsa_keygen_bits(ctx, bits) > 0`.
    pub(crate) fn set_rsa_bits(&mut self, bits: c_int) -> bool {
        // SAFETY: the context is live and set up for keys.
        unsafe { sys::EVP_PKEY_CTX_set_rsa_keygen_bits(self.0.as_ptr(), bits) > 0 }
    }

    /// `EVP_PKEY_keygen(ctx, &key)`.
    pub(crate) fn keygen(&mut self) -> Option<PKey> {
        let mut key: *mut sys::EVP_PKEY = ptr::null_mut();
        // SAFETY: the context is live; on success the library stores a
        // new key this value takes.
        let ok = unsafe { sys::EVP_PKEY_keygen(self.0.as_ptr(), &raw mut key) };
        if ok <= 0 {
            return None;
        }
        PKey::owned(key)
    }
}

/// A `CONF`, the parsed form of an `openssl.cnf`, freed on drop.
pub(crate) struct Conf(NonNull<sys::CONF>);

impl Drop for Conf {
    fn drop(&mut self) {
        // SAFETY: the configuration is owned here and freed once.
        unsafe { sys::NCONF_free(self.0.as_ptr()) };
    }
}

impl Conf {
    /// `NCONF_new(NULL)`: an empty configuration of the default method.
    pub(crate) fn new() -> Option<Conf> {
        // SAFETY: a null method asks for the default one.
        NonNull::new(unsafe { sys::NCONF_new(ptr::null_mut()) }).map(Conf)
    }

    /// `NCONF_load(conf, file, NULL)`: false for a file the library could
    /// not read or parse, its reasons on the queue.
    pub(crate) fn load(&mut self, file: &CStr) -> bool {
        // SAFETY: the configuration is live and the name NUL-terminated.
        unsafe { NCONF_load(self.0.as_ptr(), file.as_ptr(), ptr::null_mut()) > 0 }
    }

    /// `php_openssl_conf_get_string`: the value of `name` in `group` (or
    /// the default section), what the lookup queues dropped.
    pub(crate) fn string(&self, group: Option<&CStr>, name: &CStr) -> Option<Vec<u8>> {
        let group = group.map_or(ptr::null(), CStr::as_ptr);
        quietly(|| {
            // SAFETY: the configuration is live; the strings are
            // NUL-terminated; the answer is the configuration's own.
            let value = unsafe { NCONF_get_string(self.0.as_ptr(), group, name.as_ptr()) };
            // SAFETY: a non-null answer is NUL-terminated and lives with
            // the configuration.
            unsafe { copied(value) }
        })
    }

    /// `php_openssl_conf_get_number`: the number `name` in `group` holds,
    /// 0 where it holds none, what the lookup queues dropped.
    pub(crate) fn number(&self, group: Option<&CStr>, name: &CStr) -> c_long {
        let group = group.map_or(ptr::null(), CStr::as_ptr);
        quietly(|| {
            let mut out: c_long = 0;
            // SAFETY: the configuration is live; the strings are
            // NUL-terminated; the number is stored in `out` on success
            // and `out` is left alone otherwise.
            unsafe { NCONF_get_number_e(self.0.as_ptr(), group, name.as_ptr(), &raw mut out) };
            out
        })
    }
}
