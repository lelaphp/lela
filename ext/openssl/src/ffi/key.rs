//! Asymmetric keys and what is done with them: [`PKey`] (`EVP_PKEY`, one
//! reference of its count), [`KeyContext`] (`EVP_PKEY_CTX`), the
//! `EVP_MD_CTX` a signature is made or checked in ([`DigestContext`]),
//! the parameters a key is made from ([`Params`], [`ParamSet`]) and the
//! numbers and curve points they are built of ([`Bn`], [`EcGroup`],
//! [`EcPoint`]).

use std::ffi::{CStr, c_char, c_int, c_uchar, c_void};
use std::marker::PhantomData;
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
    fn EVP_PKEY_CTX_set_rsa_oaep_md(ctx: *mut sys::EVP_PKEY_CTX, md: *const sys::EVP_MD) -> c_int;
    fn EVP_PKEY_public_check_quick(ctx: *mut sys::EVP_PKEY_CTX) -> c_int;
    fn EVP_PKEY_check(ctx: *mut sys::EVP_PKEY_CTX) -> c_int;
    fn EVP_PKEY_CTX_set_params(
        ctx: *mut sys::EVP_PKEY_CTX,
        params: *const sys::OSSL_PARAM,
    ) -> c_int;
    fn EVP_PKEY_set1_encoded_public_key(
        pkey: *mut sys::EVP_PKEY,
        pb: *const c_uchar,
        n: usize,
    ) -> c_int;
    fn EC_GROUP_set_seed(group: *mut sys::EC_GROUP, seed: *const c_uchar, len: usize) -> usize;
    fn EC_GROUP_check_named_curve(
        group: *const sys::EC_GROUP,
        nist_only: c_int,
        ctx: *mut sys::BN_CTX,
    ) -> c_int;
}

/// `EVP_PKEY_KEYMGMT`: the id of a key a provider holds with no legacy
/// method, whose type is read off its name.
const EVP_PKEY_KEYMGMT: c_int = -1;

/// `OPENSSL_EC_NAMED_CURVE`: an EC key's parameters as the curve's name.
const OPENSSL_EC_NAMED_CURVE: c_int = 1;

/// `EVP_PKEY_KEYPAIR`, what `EVP_PKEY_fromdata` is asked to take.
const EVP_PKEY_KEYPAIR: c_int = 0x87;

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

    /// The private key as PEM, as [`PKey::write_private`] writes it.
    pub(crate) fn private_pem(&self, encryption: Option<(&Cipher, &[u8])>) -> Option<Vec<u8>> {
        let out = Bio::memory()?;
        self.write_private(&out, encryption).then(|| out.contents())
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

    /// `EVP_PKEY_size(key)`: the most bytes a signature or an encrypted
    /// key of it takes.
    pub(crate) fn size(&self) -> c_int {
        // SAFETY: the key is live.
        unsafe { sys::EVP_PKEY_get_size(self.as_ptr()) }
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

    /// `EVP_PKEY_base_id(key)` is `EVP_PKEY_DH`.
    pub(crate) fn is_dh(&self) -> bool {
        // SAFETY: the key is live.
        unsafe { EVP_PKEY_get_base_id(self.as_ptr()) == sys::EVP_PKEY_DH }
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

    /// `php_openssl_dh_compute_key`'s peer: `EVP_PKEY_new`, this key's
    /// parameters copied in and `public` set as its encoded public key.
    pub(crate) fn peer_of(&self, public: &[u8]) -> Option<PKey> {
        // SAFETY: EVP_PKEY_new takes nothing and answers a key or null.
        let peer = PKey::owned(unsafe { sys::EVP_PKEY_new() })?;
        // SAFETY: both keys are live.
        let copied = unsafe { sys::EVP_PKEY_copy_parameters(peer.as_ptr(), self.as_ptr()) };
        if copied <= 0 {
            return None;
        }
        // SAFETY: the key is live and `public` is read for the call alone.
        let set = unsafe {
            EVP_PKEY_set1_encoded_public_key(peer.as_ptr(), public.as_ptr(), public.len())
        };
        (set > 0).then_some(peer)
    }
}

owned!(
    /// An `EVP_PKEY_CTX`: a key being made, or an operation on one.
    KeyContext,
    sys::EVP_PKEY_CTX,
    sys::EVP_PKEY_CTX_free
);

/// What a [`KeyContext`] over a key is set up to do, each with its init
/// and its two-call operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Sign,
    Encrypt,
    Decrypt,
    VerifyRecover,
    Derive,
}

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

    /// `EVP_PKEY_CTX_new(key, NULL)`: one that works with `key`.
    pub(crate) fn of_key(key: &PKey) -> Option<KeyContext> {
        // SAFETY: the key is live; the context takes its own reference.
        KeyContext::owned(unsafe { sys::EVP_PKEY_CTX_new(key.as_ptr(), ptr::null_mut()) })
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

    /// `EVP_PKEY_fromdata_init(ctx) > 0` and `EVP_PKEY_fromdata(ctx,`
    /// `&key, EVP_PKEY_KEYPAIR, params) > 0`.
    pub(crate) fn fromdata(&mut self, params: &ParamSet) -> Option<PKey> {
        // SAFETY: the context is live.
        if unsafe { sys::EVP_PKEY_fromdata_init(self.as_ptr()) } <= 0 {
            return None;
        }
        let mut key: *mut sys::EVP_PKEY = ptr::null_mut();
        // SAFETY: the context and the parameters are live; on success the
        // library stores a new key, which `PKey` takes.
        let ok = unsafe {
            sys::EVP_PKEY_fromdata(
                self.as_ptr(),
                &raw mut key,
                EVP_PKEY_KEYPAIR,
                params.0.as_ptr(),
            )
        };
        let key = PKey::owned(key)?;
        (ok > 0).then_some(key)
    }

    /// `EVP_PKEY_check(ctx) || EVP_PKEY_public_check_quick(ctx)`, C's
    /// truth: `-2`, "not supported", counts as passing.
    pub(crate) fn checks(&mut self) -> bool {
        // SAFETY: the context is live.
        let full = unsafe { EVP_PKEY_check(self.as_ptr()) != 0 };
        // SAFETY: as above.
        full || unsafe { EVP_PKEY_public_check_quick(self.as_ptr()) != 0 }
    }

    /// `EVP_PKEY_CTX_set_params(ctx, params) == 1`.
    pub(crate) fn set_params(&mut self, params: &ParamSet) -> bool {
        // SAFETY: the context and the parameters are live.
        unsafe { EVP_PKEY_CTX_set_params(self.as_ptr(), params.0.as_ptr()) == 1 }
    }

    /// The init of `operation`, `> 0`.
    pub(crate) fn init(&mut self, operation: Operation) -> bool {
        let init: unsafe extern "C" fn(*mut sys::EVP_PKEY_CTX) -> c_int = match operation {
            Operation::Sign => sys::EVP_PKEY_sign_init,
            Operation::Encrypt => sys::EVP_PKEY_encrypt_init,
            Operation::Decrypt => sys::EVP_PKEY_decrypt_init,
            Operation::VerifyRecover => sys::EVP_PKEY_verify_recover_init,
            Operation::Derive => sys::EVP_PKEY_derive_init,
        };
        // SAFETY: the context is live, and each init takes it alone.
        unsafe { init(self.as_ptr()) > 0 }
    }

    /// `EVP_PKEY_CTX_set_rsa_padding(ctx, padding) > 0`.
    pub(crate) fn set_rsa_padding(&mut self, padding: c_int) -> bool {
        // SAFETY: the context is live.
        unsafe { sys::EVP_PKEY_CTX_set_rsa_padding(self.as_ptr(), padding) > 0 }
    }

    /// `EVP_PKEY_CTX_set_rsa_oaep_md(ctx, md) > 0`.
    pub(crate) fn set_oaep_md(&mut self, md: &Digest) -> bool {
        // SAFETY: the context and the digest are live.
        unsafe { EVP_PKEY_CTX_set_rsa_oaep_md(self.as_ptr(), md.as_ptr()) > 0 }
    }

    /// `EVP_PKEY_derive_set_peer(ctx, peer) > 0`.
    pub(crate) fn set_peer(&mut self, peer: &PKey) -> bool {
        // SAFETY: both are live; the context takes its own reference.
        unsafe { sys::EVP_PKEY_derive_set_peer(self.as_ptr(), peer.as_ptr()) > 0 }
    }

    /// The operation over `data`, what it writes sized first where `size` is
    /// 0 and taken as `size` bytes otherwise; `None` where either call
    /// answers `<= 0`.
    pub(crate) fn apply(
        &mut self,
        operation: Operation,
        data: &[u8],
        size: usize,
    ) -> Option<Vec<u8>> {
        type Op = unsafe extern "C" fn(
            *mut sys::EVP_PKEY_CTX,
            *mut c_uchar,
            *mut usize,
            *const c_uchar,
            usize,
        ) -> c_int;
        let op: Op = match operation {
            Operation::Sign => sys::EVP_PKEY_sign,
            Operation::Encrypt => sys::EVP_PKEY_encrypt,
            Operation::Decrypt => sys::EVP_PKEY_decrypt,
            Operation::VerifyRecover => sys::EVP_PKEY_verify_recover,
            Operation::Derive => derive,
        };
        let ctx = self.as_ptr();
        let call = |out: *mut c_uchar, len: &mut usize| {
            // SAFETY: the context is set up for the operation; a null
            // output asks for the length, and a non-null one holds `len`
            // bytes; the input is read for the call alone.
            unsafe { op(ctx, out, len, data.as_ptr(), data.len()) }
        };
        if size == 0 {
            return two_calls(call);
        }
        let mut out = vec![0u8; size];
        let mut len = size;
        (call(out.as_mut_ptr(), &mut len) > 0).then(|| {
            out.truncate(len);
            out
        })
    }
}

/// `EVP_PKEY_derive` in the shape of the other operations, the input
/// unused.
unsafe extern "C" fn derive(
    ctx: *mut sys::EVP_PKEY_CTX,
    out: *mut c_uchar,
    len: *mut usize,
    _data: *const c_uchar,
    _n: usize,
) -> c_int {
    // SAFETY: the caller's context, output and length, as `apply` passes
    // them.
    unsafe { sys::EVP_PKEY_derive(ctx, out, len) }
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
    /// `BN_bin2bn(bytes, len, NULL)`: big-endian bytes as a number.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Bn> {
        let len = int_len(bytes.len())?;
        // SAFETY: the slice is `len` bytes; the answer is a new number.
        Bn::owned(unsafe { sys::BN_bin2bn(bytes.as_ptr(), len, ptr::null_mut()) })
    }

    /// `BN_bn2bin`: the number's big-endian bytes, `BN_num_bytes` of them.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        // SAFETY: the number is live.
        let bits = unsafe { sys::BN_num_bits(self.as_ptr()) };
        let mut out = vec![0u8; usize::try_from(bits).unwrap_or(0).div_ceil(8)];
        // SAFETY: the buffer holds BN_num_bytes bytes, all BN_bn2bin writes.
        unsafe { sys::BN_bn2bin(self.as_ptr(), out.as_mut_ptr()) };
        out
    }

    /// `php_openssl_dh_pub_from_priv`: `g` to the power of this number
    /// modulo `p`, the public half of a DH private key.
    pub(crate) fn dh_public(&self, g: &Bn, p: &Bn) -> Option<Bn> {
        // SAFETY: BN_new and BN_CTX_new take nothing.
        let out = Bn::owned(unsafe { sys::BN_new() })?;
        // SAFETY: as above.
        let ctx = NonNull::new(unsafe { sys::BN_CTX_new() })?;
        // SAFETY: every number and the scratch are live.
        let done = unsafe {
            sys::BN_mod_exp(
                out.as_ptr(),
                g.as_ptr(),
                self.as_ptr(),
                p.as_ptr(),
                ctx.as_ptr(),
            )
        };
        // SAFETY: the scratch is owned here and freed once.
        unsafe { sys::BN_CTX_free(ctx.as_ptr()) };
        (done > 0).then_some(out)
    }
}

/// An `OSSL_PARAM_BLD` a key's parameters are pushed onto; what it points
/// at lives as long as `'a`, since the library copies it only in
/// [`Params::build`].
pub(crate) struct Params<'a>(NonNull<sys::OSSL_PARAM_BLD>, PhantomData<&'a [u8]>);

impl Drop for Params<'_> {
    fn drop(&mut self) {
        // SAFETY: the builder is owned here and freed once.
        unsafe { sys::OSSL_PARAM_BLD_free(self.0.as_ptr()) };
    }
}

impl<'a> Params<'a> {
    /// `OSSL_PARAM_BLD_new()`.
    pub(crate) fn new() -> Option<Params<'a>> {
        // SAFETY: OSSL_PARAM_BLD_new takes nothing.
        NonNull::new(unsafe { sys::OSSL_PARAM_BLD_new() }).map(|p| Params(p, PhantomData))
    }

    /// `OSSL_PARAM_BLD_push_BN(bld, name, bn)`, a null number where none.
    pub(crate) fn push_bn(&mut self, name: &'static CStr, bn: Option<&'a Bn>) -> bool {
        let bn = bn.map_or(ptr::null(), |bn| bn.as_ptr().cast_const());
        // SAFETY: the builder is live; the name is static and the number
        // lives for `'a`, past the build.
        unsafe { sys::OSSL_PARAM_BLD_push_BN(self.0.as_ptr(), name.as_ptr(), bn) > 0 }
    }

    /// `OSSL_PARAM_BLD_push_octet_string(bld, name, bytes, len)`.
    pub(crate) fn push_octets(&mut self, name: &'static CStr, bytes: &'a [u8]) -> bool {
        // SAFETY: the builder is live; the name is static and the bytes
        // live for `'a`, past the build.
        unsafe {
            sys::OSSL_PARAM_BLD_push_octet_string(
                self.0.as_ptr(),
                name.as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
            ) > 0
        }
    }

    /// `OSSL_PARAM_BLD_push_utf8_string(bld, name, text, len)`.
    pub(crate) fn push_utf8(&mut self, name: &'static CStr, text: &'a [u8]) -> bool {
        // SAFETY: the builder is live; the name is static and the text
        // lives for `'a`, past the build.
        unsafe {
            sys::OSSL_PARAM_BLD_push_utf8_string(
                self.0.as_ptr(),
                name.as_ptr(),
                text.as_ptr().cast(),
                text.len(),
            ) > 0
        }
    }

    /// `OSSL_PARAM_BLD_to_param(bld)`.
    pub(crate) fn build(&mut self) -> Option<ParamSet> {
        // SAFETY: the builder is live; what it points at is live too.
        ParamSet::owned(unsafe { sys::OSSL_PARAM_BLD_to_param(self.0.as_ptr()) })
    }
}

owned!(
    /// An `OSSL_PARAM` array, what [`Params::build`] makes.
    ParamSet,
    sys::OSSL_PARAM,
    sys::OSSL_PARAM_free
);

owned!(
    /// An `EC_GROUP`, a curve.
    EcGroup,
    sys::EC_GROUP,
    sys::EC_GROUP_free
);

impl EcGroup {
    /// `EC_GROUP_new_by_curve_name(nid)`.
    pub(crate) fn by_nid(nid: c_int) -> Option<EcGroup> {
        // SAFETY: any NID may be asked for.
        EcGroup::owned(unsafe { sys::EC_GROUP_new_by_curve_name(nid) })
    }

    /// `EC_GROUP_new_curve_GFp(p, a, b, NULL)`.
    pub(crate) fn prime(p: &Bn, a: &Bn, b: &Bn) -> Option<EcGroup> {
        // SAFETY: the three numbers are live; no scratch is passed.
        let group = unsafe {
            sys::EC_GROUP_new_curve_GFp(p.as_ptr(), a.as_ptr(), b.as_ptr(), ptr::null_mut())
        };
        EcGroup::owned(group)
    }

    /// `EC_GROUP_set_seed(group, seed, len)`, nonzero.
    pub(crate) fn set_seed(&mut self, seed: &[u8]) -> bool {
        // SAFETY: the group is live; the seed is copied.
        unsafe { EC_GROUP_set_seed(self.as_ptr(), seed.as_ptr(), seed.len()) != 0 }
    }

    /// `EC_GROUP_set_generator(group, g, order, cofactor)`, nonzero.
    pub(crate) fn set_generator(&mut self, g: &EcPoint, order: &Bn, cofactor: Option<&Bn>) -> bool {
        let cofactor = cofactor.map_or(ptr::null(), |c| c.as_ptr().cast_const());
        // SAFETY: the group, the point and the numbers are live.
        unsafe {
            sys::EC_GROUP_set_generator(self.as_ptr(), g.as_ptr(), order.as_ptr(), cofactor) != 0
        }
    }

    /// `EC_GROUP_check_named_curve(group, 0, NULL)`: the NID of the named
    /// curve the parameters are, 0 for none.
    pub(crate) fn named_curve(&self) -> c_int {
        // SAFETY: the group is live; no scratch is passed.
        unsafe { EC_GROUP_check_named_curve(self.as_ptr(), 0, ptr::null_mut()) }
    }
}

owned!(
    /// An `EC_POINT` on some group.
    EcPoint,
    sys::EC_POINT,
    sys::EC_POINT_free
);

impl EcPoint {
    /// `EC_POINT_new(group)`.
    pub(crate) fn new(group: &EcGroup) -> Option<EcPoint> {
        // SAFETY: the group is live.
        EcPoint::owned(unsafe { sys::EC_POINT_new(group.as_ptr()) })
    }

    /// `EC_POINT_oct2point(group, point, bytes, len, NULL)`, nonzero.
    pub(crate) fn set_octets(&mut self, group: &EcGroup, bytes: &[u8]) -> bool {
        // SAFETY: the group and the point are live; the bytes are read
        // for the call alone.
        unsafe {
            sys::EC_POINT_oct2point(
                group.as_ptr(),
                self.as_ptr(),
                bytes.as_ptr(),
                bytes.len(),
                ptr::null_mut(),
            ) != 0
        }
    }

    /// `EC_POINT_set_affine_coordinates(group, point, x, y, NULL)`.
    pub(crate) fn set_affine(&mut self, group: &EcGroup, x: &Bn, y: &Bn) -> bool {
        // SAFETY: the group, the point and the numbers are live.
        unsafe {
            sys::EC_POINT_set_affine_coordinates(
                group.as_ptr(),
                self.as_ptr(),
                x.as_ptr(),
                y.as_ptr(),
                ptr::null_mut(),
            ) != 0
        }
    }

    /// `EC_POINT_mul(group, point, n, NULL, NULL, NULL)`: `n` times the
    /// generator.
    pub(crate) fn set_multiple(&mut self, group: &EcGroup, n: &Bn) -> bool {
        // SAFETY: the group, the point and the number are live.
        unsafe {
            sys::EC_POINT_mul(
                group.as_ptr(),
                self.as_ptr(),
                n.as_ptr(),
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
            ) != 0
        }
    }

    /// `EC_POINT_point2buf(group, point, POINT_CONVERSION_COMPRESSED)`:
    /// the point's compressed encoding; `None` for none.
    pub(crate) fn compressed(&self, group: &EcGroup) -> Option<Vec<u8>> {
        let form = sys::point_conversion_form_t::POINT_CONVERSION_COMPRESSED;
        let encoded = two_calls(|out, len| {
            let size = if out.is_null() { 0 } else { *len };
            // SAFETY: the group and the point are live; a null buffer asks
            // for the length, and a non-null one holds `size` bytes.
            let n = unsafe {
                sys::EC_POINT_point2oct(
                    group.as_ptr(),
                    self.as_ptr(),
                    form,
                    out,
                    size,
                    ptr::null_mut(),
                )
            };
            *len = n;
            c_int::from(n > 0)
        });
        encoded.filter(|bytes| !bytes.is_empty())
    }
}
