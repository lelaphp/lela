//! Digests and symmetric ciphers: [`Digest`] (`EVP_MD`), [`Cipher`]
//! (`EVP_CIPHER`) and the `EVP_CIPHER_CTX` data passes through
//! ([`CipherContext`]), the envelopes `openssl_seal` and `openssl_open`
//! make and take, PBKDF2, and the lists of names the library carries.

use std::ffi::{CStr, c_char, c_int, c_uchar, c_ulong, c_void};
use std::ptr::{self, NonNull};

use openssl_sys as sys;

use super::key::PKey;
use super::{copied, int_len, nid_short_name};

// SAFETY: libcrypto's own declarations, from `openssl/evp.h`,
// `openssl/objects.h` and `openssl/ec.h` of OpenSSL 3, which
// `openssl-sys` does not declare; the library `openssl-sys` links defines
// each.
unsafe extern "C" {
    fn OBJ_NAME_do_all_sorted(
        kind: c_int,
        f: unsafe extern "C" fn(*const ObjName, *mut c_void),
        arg: *mut c_void,
    );
    fn EVP_CIPHER_do_all_provided(
        libctx: *mut sys::OSSL_LIB_CTX,
        f: unsafe extern "C" fn(*mut sys::EVP_CIPHER, *mut c_void),
        arg: *mut c_void,
    );
    fn EVP_CIPHER_names_do_all(
        cipher: *const sys::EVP_CIPHER,
        f: unsafe extern "C" fn(*const c_char, *mut c_void),
        data: *mut c_void,
    ) -> c_int;
    fn EVP_CIPHER_get0_name(cipher: *const sys::EVP_CIPHER) -> *const c_char;
    fn EC_get_builtin_curves(r: *mut BuiltinCurve, nitems: usize) -> usize;
}

/// `OBJ_NAME`, one entry of the library's name table.
#[repr(C)]
struct ObjName {
    kind: c_int,
    alias: c_int,
    name: *const c_char,
    data: *const c_char,
}

/// `EC_builtin_curve`.
#[repr(C)]
#[derive(Clone, Copy)]
struct BuiltinCurve {
    nid: c_int,
    comment: *const c_char,
}

/// `OBJ_NAME_TYPE_MD_METH`.
const OBJ_NAME_TYPE_MD_METH: c_int = 1;

/// `EVP_CIPH_MODE`, the bits of a cipher's flags that say how it chains
/// blocks: `EVP_CIPH_GCM_MODE` and its kin.
const EVP_CIPH_MODE: c_ulong = 0xF0007;

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

    /// `EVP_MD_size(md)`.
    pub(crate) fn size(&self) -> c_int {
        // SAFETY: the digest is live.
        unsafe { sys::EVP_MD_get_size(self.as_ptr()) }
    }

    /// `EVP_DigestInit`, `EVP_DigestUpdate` and `EVP_DigestFinal` over
    /// `data`: the digest's bytes, `None` where a step refused.
    pub(crate) fn digest(&self, data: &[u8]) -> Option<Vec<u8>> {
        // SAFETY: EVP_MD_CTX_new takes nothing.
        let ctx = NonNull::new(unsafe { sys::EVP_MD_CTX_new() })?;
        let mut out = vec![0u8; usize::try_from(self.size()).unwrap_or(0).max(64)];
        let mut len: u32 = 0;
        // SAFETY: the context and the digest are live.
        let ok = unsafe { sys::EVP_DigestInit_ex(ctx.as_ptr(), self.as_ptr(), ptr::null_mut()) } > 0
            // SAFETY: the context is set up; the data is read for the call.
            && unsafe { sys::EVP_DigestUpdate(ctx.as_ptr(), data.as_ptr().cast(), data.len()) } > 0
            // SAFETY: the buffer holds at least the digest's size.
            && unsafe { sys::EVP_DigestFinal_ex(ctx.as_ptr(), out.as_mut_ptr(), &raw mut len) } > 0;
        // SAFETY: the context is owned here and freed once.
        unsafe { sys::EVP_MD_CTX_free(ctx.as_ptr()) };
        out.truncate(usize::try_from(len).unwrap_or(0));
        ok.then_some(out)
    }
}

/// `PKCS5_PBKDF2_HMAC(password, salt, iterations, digest, length)`.
pub(crate) fn pbkdf2(
    password: &[u8],
    salt: &[u8],
    iterations: c_int,
    digest: &Digest,
    length: c_int,
) -> Option<Vec<u8>> {
    let mut out = vec![0u8; usize::try_from(length).ok()?];
    let (plen, slen) = (int_len(password.len())?, int_len(salt.len())?);
    // SAFETY: the password and salt are read for the call; the output is
    // `length` bytes; the digest is live.
    let done = unsafe {
        sys::PKCS5_PBKDF2_HMAC(
            password.as_ptr().cast(),
            plen,
            salt.as_ptr(),
            slen,
            iterations,
            digest.as_ptr(),
            length,
            out.as_mut_ptr(),
        )
    };
    (done == 1).then_some(out)
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

    /// `EVP_CIPHER_iv_length(cipher)`.
    pub(crate) fn iv_length(&self) -> c_int {
        // SAFETY: the cipher is live.
        unsafe { sys::EVP_CIPHER_get_iv_length(self.as_ptr()) }
    }

    /// `EVP_CIPHER_key_length(cipher)`.
    pub(crate) fn key_length(&self) -> c_int {
        // SAFETY: the cipher is live.
        unsafe { sys::EVP_CIPHER_get_key_length(self.as_ptr()) }
    }

    /// `EVP_CIPHER_block_size(cipher)`.
    pub(crate) fn block_size(&self) -> c_int {
        // SAFETY: the cipher is live.
        unsafe { sys::EVP_CIPHER_get_block_size(self.as_ptr()) }
    }

    /// `EVP_CIPHER_mode(cipher)`: `EVP_CIPH_GCM_MODE` and its kin.
    pub(crate) fn mode_bits(&self) -> c_ulong {
        // SAFETY: the cipher is live.
        unsafe { sys::EVP_CIPHER_get_flags(self.as_ptr()) & EVP_CIPH_MODE }
    }

    /// `EVP_CIPHER_nid(cipher)`.
    pub(crate) fn nid(&self) -> c_int {
        // SAFETY: the cipher is live.
        unsafe { sys::EVP_CIPHER_get_nid(self.as_ptr()) }
    }
}

owned!(
    /// An `EVP_CIPHER_CTX`, data on its way through a cipher.
    CipherContext,
    sys::EVP_CIPHER_CTX,
    sys::EVP_CIPHER_CTX_free
);

impl CipherContext {
    /// `EVP_CIPHER_CTX_new()`.
    pub(crate) fn new() -> Option<CipherContext> {
        // SAFETY: EVP_CIPHER_CTX_new takes nothing.
        CipherContext::owned(unsafe { sys::EVP_CIPHER_CTX_new() })
    }

    /// `EVP_CipherInit_ex(ctx, cipher, NULL, key, iv, enc)`, each of the
    /// three left as it is where it is `None`.
    pub(crate) fn init(
        &mut self,
        cipher: Option<&Cipher>,
        key: Option<&[u8]>,
        iv: Option<&[u8]>,
        encrypt: bool,
    ) -> bool {
        let cipher = cipher.map_or(ptr::null(), |c| c.as_ptr().cast_const());
        let key = key.map_or(ptr::null(), <[u8]>::as_ptr);
        let iv = iv.map_or(ptr::null(), <[u8]>::as_ptr);
        // SAFETY: the context is live; the cipher is live or null; the key
        // and IV are at least as long as the cipher, as the natives size
        // them, and read for the call.
        let done = unsafe {
            sys::EVP_CipherInit_ex(
                self.as_ptr(),
                cipher,
                ptr::null_mut(),
                key,
                iv,
                c_int::from(encrypt),
            )
        };
        done != 0
    }

    /// `EVP_CIPHER_CTX_ctrl(ctx, op, arg, NULL) > 0`.
    pub(crate) fn ctrl(&mut self, op: c_int, arg: c_int) -> bool {
        // SAFETY: the context is live; the operations this crate asks
        // with a null pointer read none.
        unsafe { sys::EVP_CIPHER_CTX_ctrl(self.as_ptr(), op, arg, ptr::null_mut()) > 0 }
    }

    /// `EVP_CIPHER_CTX_ctrl(ctx, EVP_CTRL_AEAD_SET_TAG, len, tag) > 0`.
    pub(crate) fn set_tag(&mut self, op: c_int, tag: &[u8]) -> bool {
        let Some(len) = int_len(tag.len()) else {
            return false;
        };
        // SAFETY: the context is live; the library copies `len` bytes of
        // the tag.
        unsafe {
            sys::EVP_CIPHER_CTX_ctrl(self.as_ptr(), op, len, tag.as_ptr().cast_mut().cast()) > 0
        }
    }

    /// `EVP_CIPHER_CTX_ctrl(ctx, EVP_CTRL_AEAD_GET_TAG, len, buf)`: the
    /// tag, `len` bytes of it.
    pub(crate) fn tag(&mut self, op: c_int, len: c_int) -> Option<Vec<u8>> {
        let mut out = vec![0u8; usize::try_from(len).ok()?];
        // SAFETY: the context is live; the buffer holds `len` bytes.
        let done =
            unsafe { sys::EVP_CIPHER_CTX_ctrl(self.as_ptr(), op, len, out.as_mut_ptr().cast()) };
        (done > 0).then_some(out)
    }

    /// `EVP_CIPHER_CTX_set_key_length(ctx, len)`, nonzero.
    pub(crate) fn set_key_length(&mut self, len: c_int) -> bool {
        // SAFETY: the context is live.
        unsafe { sys::EVP_CIPHER_CTX_set_key_length(self.as_ptr(), len) != 0 }
    }

    /// `EVP_CIPHER_CTX_set_padding(ctx, 0)`.
    pub(crate) fn no_padding(&mut self) {
        // SAFETY: the context is live.
        unsafe { sys::EVP_CIPHER_CTX_set_padding(self.as_ptr(), 0) };
    }

    /// `EVP_CipherUpdate(ctx, NULL, &i, input, len)`: additional data, or
    /// with no input the length of the data to come.
    pub(crate) fn update_aside(&mut self, input: Option<&[u8]>, len: c_int) -> bool {
        let mut out: c_int = 0;
        let input = input.map_or(ptr::null(), <[u8]>::as_ptr);
        // SAFETY: the context is live; no output is written; the input is
        // null or `len` bytes read for the call.
        let done = unsafe {
            sys::EVP_CipherUpdate(self.as_ptr(), ptr::null_mut(), &raw mut out, input, len)
        };
        done != 0
    }

    /// `EVP_CipherUpdate(ctx, out, &n, input, len)` into a buffer of
    /// `input.len() + room` bytes: the bytes written, `None` where the
    /// library refused.
    pub(crate) fn update(&mut self, input: &[u8], room: usize) -> Option<Vec<u8>> {
        let len = int_len(input.len())?;
        let mut out = vec![0u8; input.len() + room];
        let mut written: c_int = 0;
        // SAFETY: the context is live; the output holds the input's length
        // and `room` more, the most a cipher of this block size writes.
        let done = unsafe {
            sys::EVP_CipherUpdate(
                self.as_ptr(),
                out.as_mut_ptr(),
                &raw mut written,
                input.as_ptr(),
                len,
            )
        };
        out.truncate(usize::try_from(written).unwrap_or(0));
        (done != 0).then_some(out)
    }

    /// `EVP_EncryptFinal` or `EVP_DecryptFinal` into `out`'s end, `room`
    /// bytes at most: false where the library refused.
    pub(crate) fn finish(&mut self, out: &mut Vec<u8>, room: usize, encrypt: bool) -> bool {
        let mut tail = vec![0u8; room.max(32)];
        let mut written: c_int = 0;
        let finish = if encrypt {
            sys::EVP_EncryptFinal_ex
        } else {
            sys::EVP_DecryptFinal_ex
        };
        // SAFETY: the context is live; the buffer holds a block and more.
        let done = unsafe { finish(self.as_ptr(), tail.as_mut_ptr(), &raw mut written) };
        tail.truncate(usize::try_from(written).unwrap_or(0));
        out.extend_from_slice(&tail);
        done != 0
    }
}

/// What `openssl_seal` makes: the sealed data, one encrypted key per
/// public key, and the IV.
pub(crate) struct Sealed {
    pub(crate) data: Vec<u8>,
    pub(crate) keys: Vec<Vec<u8>>,
    pub(crate) iv: Vec<u8>,
}

/// `openssl_seal`'s library half: `EVP_EncryptInit(ctx, cipher, NULL,`
/// `NULL)` to learn the block size, then `EVP_SealInit`, `EVP_SealUpdate`
/// and `EVP_SealFinal`; `None` where a step refused.
pub(crate) fn seal(cipher: &Cipher, keys: &[PKey], data: &[u8]) -> Option<Sealed> {
    let len = int_len(data.len())?;
    let mut ctx = CipherContext::new()?;
    if !ctx.init(Some(cipher), None, None, true) {
        return None;
    }
    let mut ekeys: Vec<Vec<u8>> = keys
        .iter()
        .map(|k| vec![0u8; usize::try_from(k.size()).unwrap_or(0) + 1])
        .collect();
    let mut ekey_ptrs: Vec<*mut c_uchar> = ekeys.iter_mut().map(Vec::as_mut_ptr).collect();
    let mut ekey_lens = vec![0 as c_int; keys.len()];
    let mut pkeys: Vec<*mut sys::EVP_PKEY> = keys.iter().map(PKey::as_ptr).collect();
    let mut iv = vec![0u8; 16 + 1];
    let npubk = int_len(keys.len())?;
    // SAFETY: the context and cipher are live; each key buffer holds
    // `EVP_PKEY_size` bytes of its key; the IV buffer holds
    // `EVP_MAX_IV_LENGTH`.
    let init = unsafe {
        sys::EVP_SealInit(
            ctx.as_ptr(),
            cipher.as_ptr(),
            ekey_ptrs.as_mut_ptr(),
            ekey_lens.as_mut_ptr(),
            iv.as_mut_ptr(),
            pkeys.as_mut_ptr(),
            npubk,
        )
    };
    if init <= 0 {
        return None;
    }
    let block = usize::try_from(cipher.block_size()).unwrap_or(0);
    let mut out = vec![0u8; data.len() + block];
    let mut written: c_int = 0;
    // SAFETY: the context is set up; the output holds the input and a
    // block more.
    let updated = unsafe {
        sys::EVP_EncryptUpdate(
            ctx.as_ptr(),
            out.as_mut_ptr(),
            &raw mut written,
            data.as_ptr(),
            len,
        )
    };
    if updated == 0 {
        return None;
    }
    out.truncate(usize::try_from(written).unwrap_or(0));
    let mut tail = vec![0u8; block.max(32)];
    let mut more: c_int = 0;
    // SAFETY: the context is set up; the buffer holds a block and more.
    if unsafe { sys::EVP_SealFinal(ctx.as_ptr(), tail.as_mut_ptr(), &raw mut more) } == 0 {
        return None;
    }
    tail.truncate(usize::try_from(more).unwrap_or(0));
    out.extend_from_slice(&tail);
    for (key, len) in ekeys.iter_mut().zip(&ekey_lens) {
        key.truncate(usize::try_from(*len).unwrap_or(0));
    }
    iv.truncate(usize::try_from(cipher.iv_length()).unwrap_or(0));
    Some(Sealed {
        data: out,
        keys: ekeys,
        iv,
    })
}

/// `openssl_open`'s library half: `EVP_OpenInit`, `EVP_OpenUpdate` and
/// `EVP_OpenFinal`; `None` where a step refused.
pub(crate) fn open(
    cipher: &Cipher,
    ekey: &[u8],
    iv: Option<&[u8]>,
    key: &PKey,
    data: &[u8],
) -> Option<Vec<u8>> {
    let (len, ekey_len) = (int_len(data.len())?, int_len(ekey.len())?);
    let ctx = CipherContext::new()?;
    let iv = iv.map_or(ptr::null(), <[u8]>::as_ptr);
    // SAFETY: the context, cipher and key are live; the encrypted key is
    // `ekey_len` bytes and the IV the cipher's length, read for the call.
    let init = unsafe {
        sys::EVP_OpenInit(
            ctx.as_ptr(),
            cipher.as_ptr(),
            ekey.as_ptr(),
            ekey_len,
            iv,
            key.as_ptr(),
        )
    };
    if init == 0 {
        return None;
    }
    let block = usize::try_from(cipher.block_size()).unwrap_or(0);
    let mut out = vec![0u8; data.len() + block];
    let mut written: c_int = 0;
    // SAFETY: the context is set up; the output holds the input and a
    // block more.
    let updated = unsafe {
        sys::EVP_DecryptUpdate(
            ctx.as_ptr(),
            out.as_mut_ptr(),
            &raw mut written,
            data.as_ptr(),
            len,
        )
    };
    if updated == 0 {
        return None;
    }
    out.truncate(usize::try_from(written).unwrap_or(0));
    let mut tail = vec![0u8; block.max(32)];
    let mut more: c_int = 0;
    // SAFETY: the context is set up; the buffer holds a block and more.
    if unsafe { sys::EVP_OpenFinal(ctx.as_ptr(), tail.as_mut_ptr(), &raw mut more) } == 0 {
        return None;
    }
    tail.truncate(usize::try_from(more).unwrap_or(0));
    out.extend_from_slice(&tail);
    Some(out)
}

/// What a name-walking callback collects into, and whether aliases count.
struct Names {
    out: Vec<Vec<u8>>,
    aliases: bool,
}

/// `php_openssl_add_method` and `php_openssl_add_method_or_alias`.
unsafe extern "C" fn add_md_name(name: *const ObjName, arg: *mut c_void) {
    // SAFETY: the library hands back the `Names` `md_names` passed.
    let names = unsafe { &mut *arg.cast::<Names>() };
    // SAFETY: a live entry of the library's table.
    let name = unsafe { &*name };
    if names.aliases || name.alias == 0 {
        // SAFETY: the entry's name is NUL-terminated.
        names.out.extend(unsafe { copied(name.name) });
    }
}

/// `OBJ_NAME_do_all_sorted(OBJ_NAME_TYPE_MD_METH, ...)`: the digest names
/// in the library's table, sorted, aliases too where asked.
pub(crate) fn md_names(aliases: bool) -> Vec<Vec<u8>> {
    let mut names = Names {
        out: Vec::new(),
        aliases,
    };
    // SAFETY: the callback reads `names` for the walk alone.
    unsafe { OBJ_NAME_do_all_sorted(OBJ_NAME_TYPE_MD_METH, add_md_name, (&raw mut names).cast()) };
    names.out
}

/// `php_openssl_add_cipher_name`: one name, lowercased.
unsafe extern "C" fn add_cipher_name(name: *const c_char, arg: *mut c_void) {
    // SAFETY: the library hands back the `Names` `cipher_names` passed.
    let names = unsafe { &mut *arg.cast::<Names>() };
    // SAFETY: the name is NUL-terminated.
    if let Some(name) = unsafe { copied(name) } {
        names.out.push(name.to_ascii_lowercase());
    }
}

/// `php_openssl_add_cipher` and `php_openssl_add_cipher_or_alias`.
unsafe extern "C" fn add_cipher(cipher: *mut sys::EVP_CIPHER, arg: *mut c_void) {
    // SAFETY: the library hands back the `Names` `cipher_names` passed.
    let aliases = unsafe { (*arg.cast::<Names>()).aliases };
    if aliases {
        // SAFETY: the cipher is live for the walk; the callback reads
        // `arg` as above.
        unsafe { EVP_CIPHER_names_do_all(cipher, add_cipher_name, arg) };
    } else {
        // SAFETY: the cipher is live for the walk; the name is its own.
        let name = unsafe { EVP_CIPHER_get0_name(cipher) };
        // SAFETY: the callback reads `arg` as above.
        unsafe { add_cipher_name(name, arg) };
    }
}

/// `EVP_CIPHER_do_all_provided(NULL, ...)`: every cipher's name,
/// lowercased, in the order the providers list them, aliases too where
/// asked.
pub(crate) fn cipher_names(aliases: bool) -> Vec<Vec<u8>> {
    let mut names = Names {
        out: Vec::new(),
        aliases,
    };
    // SAFETY: the callback reads `names` for the walk alone.
    unsafe { EVP_CIPHER_do_all_provided(ptr::null_mut(), add_cipher, (&raw mut names).cast()) };
    names.out
}

/// `EC_get_builtin_curves` and `OBJ_nid2sn`: the short name of every
/// curve the library carries, `None` where it would not list them.
pub(crate) fn curve_names() -> Option<Vec<Vec<u8>>> {
    // SAFETY: a null buffer asks for the count.
    let len = unsafe { EC_get_builtin_curves(ptr::null_mut(), 0) };
    let empty = BuiltinCurve {
        nid: 0,
        comment: ptr::null(),
    };
    let mut curves = vec![empty; len];
    // SAFETY: the buffer holds `len` entries.
    if unsafe { EC_get_builtin_curves(curves.as_mut_ptr(), len) } == 0 {
        return None;
    }
    Some(
        curves
            .iter()
            .filter_map(|c| nid_short_name(c.nid))
            .collect(),
    )
}
