//! Asymmetric keys and what is done with them: [`PKey`] (`EVP_PKEY`, one
//! reference of its count), [`KeyContext`] (`EVP_PKEY_CTX`), the
//! `EVP_MD_CTX` a signature is made or checked in ([`DigestContext`]),
//! and the numbers keys are read as ([`Bn`]).

use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr::{self, NonNull};

use openssl_sys as sys;

use super::cipher::{Cipher, Digest};
use super::{Bio, int_len, two_calls, written};

// SAFETY: libcrypto's own declarations, from `openssl/evp.h`,
// `openssl/objects.h`, `openssl/ec.h` and `openssl/rsa.h` of OpenSSL 3,
// which `openssl-sys` does not declare; the library `openssl-sys` links
// defines each.
unsafe extern "C" {
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
}

/// `EVP_PKEY_KEYMGMT`: the id of a key a provider holds with no legacy
/// method, whose type is read off its name.
const EVP_PKEY_KEYMGMT: c_int = -1;

/// `OPENSSL_EC_NAMED_CURVE`: an EC key's parameters as the curve's name.
const OPENSSL_EC_NAMED_CURVE: c_int = 1;

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

owned!(
    /// An `EVP_PKEY`: one reference of the library's count.
    PKey,
    sys::EVP_PKEY,
    sys::EVP_PKEY_free
);

// SAFETY: an EVP_PKEY's reference count is atomic, and a key is not
// changed after it is made; OpenSSL 3 shares keys across threads.
unsafe impl Send for PKey {}

impl Clone for PKey {
    /// `EVP_PKEY_up_ref`: a second reference to the same key.
    fn clone(&self) -> PKey {
        // SAFETY: the key is live; up_ref counts one more owner, the
        // value returned here.
        unsafe { sys::EVP_PKEY_up_ref(self.as_ptr()) };
        PKey(self.0)
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

impl PKey {
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

    /// `PEM_write_bio_PrivateKey(out, key, cipher, pass, len, NULL,`
    /// `NULL)` into `out`, encrypted under a cipher and a passphrase or
    /// not at all: false where the library refused.
    pub(crate) fn write_private(
        &self,
        out: &Bio<'_>,
        encryption: Option<(&Cipher, &[u8])>,
    ) -> bool {
        let (cipher, kstr, len) = match encryption {
            Some((cipher, phrase)) => match int_len(phrase.len()) {
                Some(len) => (cipher.as_ptr().cast_const(), phrase.as_ptr(), len),
                None => return false,
            },
            None => (ptr::null(), ptr::null(), 0),
        };
        // SAFETY: the sink, the key and the cipher are live; the
        // passphrase is `len` bytes read for the call alone.
        let done = unsafe {
            sys::PEM_write_bio_PrivateKey(
                out.as_ptr(),
                self.as_ptr(),
                cipher,
                kstr,
                len,
                None,
                ptr::null_mut(),
            )
        };
        done != 0
    }

    /// `PEM_write_bio_PUBKEY(out, key)` into a memory sink.
    pub(crate) fn public_pem(&self) -> Option<Vec<u8>> {
        // SAFETY: the sink and the key are live.
        written(|out| unsafe { sys::PEM_write_bio_PUBKEY(out, self.as_ptr()) })
    }

    /// `EVP_PKEY_bits(key)`.
    pub(crate) fn bits(&self) -> i64 {
        // SAFETY: the key is live.
        i64::from(unsafe { sys::EVP_PKEY_get_bits(self.as_ptr()) })
    }

    /// The key's kind as `php_openssl_pkey_get_details` reads it: the
    /// base id of a legacy key, or the kind its type name names for one a
    /// provider holds.
    pub(crate) fn kind(&self) -> KeyKind {
        // SAFETY: the key is live.
        let id = unsafe { sys::EVP_PKEY_get_id(self.as_ptr()) };
        if id != EVP_PKEY_KEYMGMT {
            // SAFETY: the key is live.
            return KeyKind::of(unsafe { EVP_PKEY_get_base_id(self.as_ptr()) });
        }
        // SAFETY: the key is live; the name, when there is one, is the
        // key's own static string.
        let name = unsafe { EVP_PKEY_get0_type_name(self.as_ptr()) };
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
        let id = unsafe { sys::EVP_PKEY_get_id(self.as_ptr()) };
        // SAFETY: EVP_PKEY_type takes any NID.
        KeyKind::of(unsafe { EVP_PKEY_type(id) })
    }

    /// `EVP_PKEY_get_bn_param(key, name, &bn)` and `BN_bn2bin`: the
    /// parameter's big-endian bytes, or `None` for one the key lacks.
    pub(crate) fn bn_param(&self, name: &CStr) -> Option<Vec<u8>> {
        let mut bn: *mut sys::BIGNUM = ptr::null_mut();
        // SAFETY: the key and the name are live; on success the library
        // stores a new BIGNUM, which `Bn` takes.
        let found =
            unsafe { sys::EVP_PKEY_get_bn_param(self.as_ptr(), name.as_ptr(), &raw mut bn) };
        let bn = Bn::owned(bn)?;
        (found > 0).then(|| bn.to_bytes())
    }

    /// `EVP_PKEY_get_utf8_string_param(key, name, buf, 64, &len)`.
    pub(crate) fn utf8_param(&self, name: &CStr) -> Option<Vec<u8>> {
        let mut buf = [0 as c_char; 64];
        let mut len = 0usize;
        // SAFETY: the key and the name are live; the buffer is 64 bytes
        // as the size says.
        let found = unsafe {
            sys::EVP_PKEY_get_utf8_string_param(
                self.as_ptr(),
                name.as_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut len,
            )
        };
        let bytes = buf[..len.min(buf.len())]
            .iter()
            .map(|&c| c.to_ne_bytes()[0]);
        (found > 0).then(|| bytes.collect())
    }

    /// `EVP_PKEY_get_octet_string_param(key, name, buf, 64, &len)`.
    pub(crate) fn octet_param(&self, name: &CStr) -> Option<Vec<u8>> {
        let mut buf = [0u8; 64];
        let mut len = 0usize;
        // SAFETY: the key and the name are live; the buffer is 64 bytes
        // as the size says.
        let found = unsafe {
            sys::EVP_PKEY_get_octet_string_param(
                self.as_ptr(),
                name.as_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut len,
            )
        };
        (found > 0).then(|| buf[..len.min(buf.len())].to_vec())
    }
}

owned!(
    /// An `EVP_PKEY_CTX`: a key being made, or an operation on one.
    KeyContext,
    sys::EVP_PKEY_CTX,
    sys::EVP_PKEY_CTX_free
);

impl KeyContext {
    /// `EVP_PKEY_CTX_new_from_name(NULL, name, NULL)`.
    pub(crate) fn of_kind(kind: KeyKind) -> Option<KeyContext> {
        // SAFETY: the name is NUL-terminated; the answer is a new context
        // or null.
        let ctx = unsafe {
            sys::EVP_PKEY_CTX_new_from_name(ptr::null_mut(), kind.name().as_ptr(), ptr::null())
        };
        KeyContext::owned(ctx)
    }

    /// `EVP_PKEY_CTX_new_from_pkey(NULL, key, NULL)`: one that makes keys
    /// over the parameters `key` holds.
    pub(crate) fn of_params(params: &PKey) -> Option<KeyContext> {
        // SAFETY: the parameters are live; the context takes its own
        // reference to them.
        let ctx =
            unsafe { EVP_PKEY_CTX_new_from_pkey(ptr::null_mut(), params.as_ptr(), ptr::null()) };
        KeyContext::owned(ctx)
    }

    /// `EVP_PKEY_paramgen_init(ctx) > 0`.
    pub(crate) fn paramgen_init(&mut self) -> bool {
        // SAFETY: the context is live.
        unsafe { sys::EVP_PKEY_paramgen_init(self.as_ptr()) > 0 }
    }

    /// `EVP_PKEY_CTX_set_dsa_paramgen_bits(ctx, bits) > 0`.
    pub(crate) fn set_dsa_bits(&mut self, bits: c_int) -> bool {
        // SAFETY: the context is live and set up for parameters.
        unsafe { sys::EVP_PKEY_CTX_set_dsa_paramgen_bits(self.as_ptr(), bits) > 0 }
    }

    /// `EVP_PKEY_CTX_set_dh_paramgen_prime_len(ctx, bits) > 0`.
    pub(crate) fn set_dh_prime_len(&mut self, bits: c_int) -> bool {
        // SAFETY: the context is live and set up for parameters.
        unsafe { sys::EVP_PKEY_CTX_set_dh_paramgen_prime_len(self.as_ptr(), bits) > 0 }
    }

    /// `EVP_PKEY_CTX_set_ec_paramgen_curve_nid(ctx, nid) > 0` and
    /// `EVP_PKEY_CTX_set_ec_param_enc(ctx, OPENSSL_EC_NAMED_CURVE) > 0`.
    pub(crate) fn set_ec_curve(&mut self, nid: c_int) -> bool {
        // SAFETY: the context is live and set up for parameters.
        let curve = unsafe { sys::EVP_PKEY_CTX_set_ec_paramgen_curve_nid(self.as_ptr(), nid) };
        // SAFETY: as above.
        curve > 0
            && unsafe { EVP_PKEY_CTX_set_ec_param_enc(self.as_ptr(), OPENSSL_EC_NAMED_CURVE) } > 0
    }

    /// `EVP_PKEY_paramgen(ctx, &params)`.
    pub(crate) fn paramgen(&mut self) -> Option<PKey> {
        let mut params: *mut sys::EVP_PKEY = ptr::null_mut();
        // SAFETY: the context is live; on success the library stores a
        // new key, which `PKey` takes.
        let ok = unsafe { sys::EVP_PKEY_paramgen(self.as_ptr(), &raw mut params) };
        let params = PKey::owned(params)?;
        (ok > 0).then_some(params)
    }

    /// `EVP_PKEY_keygen_init(ctx)`, its answer as the library gives it.
    pub(crate) fn keygen_init(&mut self) -> c_int {
        // SAFETY: the context is live.
        unsafe { sys::EVP_PKEY_keygen_init(self.as_ptr()) }
    }

    /// `EVP_PKEY_CTX_set_rsa_keygen_bits(ctx, bits) > 0`.
    pub(crate) fn set_rsa_bits(&mut self, bits: c_int) -> bool {
        // SAFETY: the context is live and set up for keys.
        unsafe { sys::EVP_PKEY_CTX_set_rsa_keygen_bits(self.as_ptr(), bits) > 0 }
    }

    /// `EVP_PKEY_keygen(ctx, &key) > 0`, or `EVP_PKEY_generate(ctx, &key)`
    /// `== 1` where `strict`, as php asks each.
    pub(crate) fn keygen(&mut self, strict: bool) -> Option<PKey> {
        let mut key: *mut sys::EVP_PKEY = ptr::null_mut();
        // SAFETY: the context is live; on success the library stores a
        // new key, which `PKey` takes (EVP_PKEY_keygen is EVP_PKEY_generate).
        let ok = unsafe { sys::EVP_PKEY_keygen(self.as_ptr(), &raw mut key) };
        let key = PKey::owned(key)?;
        (if strict { ok == 1 } else { ok > 0 }).then_some(key)
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
        let md = md.map_or(ptr::null(), |md| md.as_ptr().cast_const());
        let init = match direction {
            Direction::Sign => sys::EVP_DigestSignInit,
            Direction::Verify => sys::EVP_DigestVerifyInit,
        };
        // SAFETY: the context, the digest (or none) and the key are
        // live; the library takes its own reference to the key and stores
        // the key context it makes in `ctx.key`.
        let ok = unsafe {
            init(
                ctx.md.as_ptr(),
                &raw mut ctx.key,
                md,
                ptr::null_mut(),
                key.as_ptr(),
            )
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
        let md = self.md.as_ptr();
        two_calls(|out, len| {
            // SAFETY: the context is set up to sign; a null signature
            // buffer asks for the length, and a non-null one holds `len`
            // bytes.
            unsafe { sys::EVP_DigestSign(md, out, len, data.as_ptr(), data.len()) }
        })
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

owned!(
    /// A `BIGNUM`.
    Bn,
    sys::BIGNUM,
    sys::BN_free
);

impl Bn {
    /// `BN_bn2bin`: the number's big-endian bytes, `BN_num_bytes` of them.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        // SAFETY: the number is live.
        let bits = unsafe { sys::BN_num_bits(self.as_ptr()) };
        let mut out = vec![0u8; usize::try_from(bits).unwrap_or(0).div_ceil(8)];
        // SAFETY: the buffer holds BN_num_bytes bytes, all BN_bn2bin writes.
        unsafe { sys::BN_bn2bin(self.as_ptr(), out.as_mut_ptr()) };
        out
    }
}
