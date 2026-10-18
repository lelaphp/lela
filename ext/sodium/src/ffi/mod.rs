//! libsodium, the library the pinned php links (design §7.2, revision
//! 72; #1540): the calls `libsodium.c` makes, declared by hand from
//! `sodium.h`, no bindgen, behind safe functions over slices. Each
//! function holds the lengths the C signature reads to the slices it
//! is given and answers [`Failed`] where one is short, so no caller can
//! hand libsodium a buffer shorter than it reads or writes; the callers
//! hold the lengths to php's sentences before they get here, so a
//! [`Failed`] past them is libsodium's own refusal. The one home of
//! this crate's `unsafe` (`unsafe.homes`' `ext/*/src/ffi/`).
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, c_char, c_int, c_ulonglong};

mod sys {
    use std::ffi::{c_char, c_int, c_ulonglong, c_void};

    /// `crypto_generichash_blake2b_state`: 384 opaque bytes on a 64-byte
    /// boundary (`CRYPTO_ALIGN(64)`).
    #[repr(C, align(64))]
    pub(super) struct GenericHashState(pub(super) [u8; 384]);

    pub(super) type Ull = c_ulonglong;

    /// `crypto_aead_*_encrypt`, one shape for the six constructions.
    pub(super) type AeadEncrypt = unsafe extern "C" fn(
        c: *mut u8,
        clen_p: *mut Ull,
        m: *const u8,
        mlen: Ull,
        ad: *const u8,
        adlen: Ull,
        nsec: *const u8,
        npub: *const u8,
        k: *const u8,
    ) -> c_int;

    /// `crypto_aead_*_decrypt`.
    pub(super) type AeadDecrypt = unsafe extern "C" fn(
        m: *mut u8,
        mlen_p: *mut Ull,
        nsec: *mut u8,
        c: *const u8,
        clen: Ull,
        ad: *const u8,
        adlen: Ull,
        npub: *const u8,
        k: *const u8,
    ) -> c_int;

    #[link(name = "sodium")]
    unsafe extern "C" {
        pub(super) fn sodium_init() -> c_int;
        pub(super) fn sodium_version_string() -> *const c_char;
        pub(super) fn sodium_library_version_major() -> c_int;
        pub(super) fn sodium_library_version_minor() -> c_int;
        pub(super) fn sodium_increment(n: *mut u8, nlen: usize);
        pub(super) fn sodium_add(a: *mut u8, b: *const u8, len: usize);
        pub(super) fn sodium_memcmp(b1: *const c_void, b2: *const c_void, len: usize) -> c_int;
        pub(super) fn sodium_compare(b1: *const u8, b2: *const u8, len: usize) -> c_int;
        pub(super) fn sodium_bin2hex(
            hex: *mut c_char,
            hex_maxlen: usize,
            bin: *const u8,
            bin_len: usize,
        ) -> *mut c_char;
        pub(super) fn sodium_hex2bin(
            bin: *mut u8,
            bin_maxlen: usize,
            hex: *const c_char,
            hex_len: usize,
            ignore: *const c_char,
            bin_len: *mut usize,
            hex_end: *mut *const c_char,
        ) -> c_int;
        pub(super) fn sodium_bin2base64(
            b64: *mut c_char,
            b64_maxlen: usize,
            bin: *const u8,
            bin_len: usize,
            variant: c_int,
        ) -> *mut c_char;
        pub(super) fn sodium_base642bin(
            bin: *mut u8,
            bin_maxlen: usize,
            b64: *const c_char,
            b64_len: usize,
            ignore: *const c_char,
            bin_len: *mut usize,
            b64_end: *mut *const c_char,
            variant: c_int,
        ) -> c_int;
        pub(super) fn sodium_pad(
            padded_buflen_p: *mut usize,
            buf: *mut u8,
            unpadded_buflen: usize,
            blocksize: usize,
            max_buflen: usize,
        ) -> c_int;
        pub(super) fn sodium_unpad(
            unpadded_buflen_p: *mut usize,
            buf: *const u8,
            padded_buflen: usize,
            blocksize: usize,
        ) -> c_int;
        pub(super) fn randombytes_buf(buf: *mut c_void, size: usize);

        pub(super) fn crypto_shorthash(
            out: *mut u8,
            m: *const u8,
            mlen: Ull,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_auth(out: *mut u8, m: *const u8, mlen: Ull, k: *const u8) -> c_int;
        pub(super) fn crypto_auth_verify(
            h: *const u8,
            m: *const u8,
            mlen: Ull,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_secretbox_easy(
            c: *mut u8,
            m: *const u8,
            mlen: Ull,
            n: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_secretbox_open_easy(
            m: *mut u8,
            c: *const u8,
            clen: Ull,
            n: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_generichash(
            out: *mut u8,
            outlen: usize,
            m: *const u8,
            mlen: Ull,
            key: *const u8,
            keylen: usize,
        ) -> c_int;
        pub(super) fn crypto_generichash_init(
            state: *mut GenericHashState,
            key: *const u8,
            keylen: usize,
            outlen: usize,
        ) -> c_int;
        pub(super) fn crypto_generichash_update(
            state: *mut GenericHashState,
            m: *const u8,
            mlen: Ull,
        ) -> c_int;
        pub(super) fn crypto_generichash_final(
            state: *mut GenericHashState,
            out: *mut u8,
            outlen: usize,
        ) -> c_int;
        pub(super) fn crypto_box_keypair(pk: *mut u8, sk: *mut u8) -> c_int;
        pub(super) fn crypto_box_seed_keypair(pk: *mut u8, sk: *mut u8, seed: *const u8) -> c_int;
        pub(super) fn crypto_box_easy(
            c: *mut u8,
            m: *const u8,
            mlen: Ull,
            n: *const u8,
            pk: *const u8,
            sk: *const u8,
        ) -> c_int;
        pub(super) fn crypto_box_open_easy(
            m: *mut u8,
            c: *const u8,
            clen: Ull,
            n: *const u8,
            pk: *const u8,
            sk: *const u8,
        ) -> c_int;
        pub(super) fn crypto_box_seal(c: *mut u8, m: *const u8, mlen: Ull, pk: *const u8) -> c_int;
        pub(super) fn crypto_box_seal_open(
            m: *mut u8,
            c: *const u8,
            clen: Ull,
            pk: *const u8,
            sk: *const u8,
        ) -> c_int;
        pub(super) fn crypto_sign_keypair(pk: *mut u8, sk: *mut u8) -> c_int;
        pub(super) fn crypto_sign_seed_keypair(pk: *mut u8, sk: *mut u8, seed: *const u8) -> c_int;
        pub(super) fn crypto_sign(
            sm: *mut u8,
            smlen_p: *mut Ull,
            m: *const u8,
            mlen: Ull,
            sk: *const u8,
        ) -> c_int;
        pub(super) fn crypto_sign_open(
            m: *mut u8,
            mlen_p: *mut Ull,
            sm: *const u8,
            smlen: Ull,
            pk: *const u8,
        ) -> c_int;
        pub(super) fn crypto_sign_detached(
            sig: *mut u8,
            siglen_p: *mut Ull,
            m: *const u8,
            mlen: Ull,
            sk: *const u8,
        ) -> c_int;
        pub(super) fn crypto_sign_verify_detached(
            sig: *const u8,
            m: *const u8,
            mlen: Ull,
            pk: *const u8,
        ) -> c_int;
        pub(super) fn crypto_sign_ed25519_sk_to_pk(pk: *mut u8, sk: *const u8) -> c_int;
        pub(super) fn crypto_sign_ed25519_sk_to_curve25519(out: *mut u8, sk: *const u8) -> c_int;
        pub(super) fn crypto_sign_ed25519_pk_to_curve25519(out: *mut u8, pk: *const u8) -> c_int;
        pub(super) fn crypto_scalarmult_base(q: *mut u8, n: *const u8) -> c_int;
        pub(super) fn crypto_scalarmult(q: *mut u8, n: *const u8, p: *const u8) -> c_int;
        pub(super) fn crypto_scalarmult_ristretto255(
            q: *mut u8,
            n: *const u8,
            p: *const u8,
        ) -> c_int;
        pub(super) fn crypto_scalarmult_ristretto255_base(q: *mut u8, n: *const u8) -> c_int;
        pub(super) fn crypto_stream(c: *mut u8, clen: Ull, n: *const u8, k: *const u8) -> c_int;
        pub(super) fn crypto_stream_xor(
            c: *mut u8,
            m: *const u8,
            mlen: Ull,
            n: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_stream_xchacha20(
            c: *mut u8,
            clen: Ull,
            n: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_stream_xchacha20_xor(
            c: *mut u8,
            m: *const u8,
            mlen: Ull,
            n: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_stream_xchacha20_xor_ic(
            c: *mut u8,
            m: *const u8,
            mlen: Ull,
            n: *const u8,
            ic: u64,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_pwhash(
            out: *mut u8,
            outlen: Ull,
            passwd: *const c_char,
            passwdlen: Ull,
            salt: *const u8,
            opslimit: Ull,
            memlimit: usize,
            alg: c_int,
        ) -> c_int;
        pub(super) fn crypto_pwhash_str(
            out: *mut c_char,
            passwd: *const c_char,
            passwdlen: Ull,
            opslimit: Ull,
            memlimit: usize,
        ) -> c_int;
        pub(super) fn crypto_pwhash_str_verify(
            str: *const c_char,
            passwd: *const c_char,
            passwdlen: Ull,
        ) -> c_int;
        pub(super) fn crypto_pwhash_str_needs_rehash(
            str: *const c_char,
            opslimit: Ull,
            memlimit: usize,
        ) -> c_int;
        pub(super) fn crypto_pwhash_scryptsalsa208sha256(
            out: *mut u8,
            outlen: Ull,
            passwd: *const c_char,
            passwdlen: Ull,
            salt: *const u8,
            opslimit: Ull,
            memlimit: usize,
        ) -> c_int;
        pub(super) fn crypto_pwhash_scryptsalsa208sha256_str(
            out: *mut c_char,
            passwd: *const c_char,
            passwdlen: Ull,
            opslimit: Ull,
            memlimit: usize,
        ) -> c_int;
        pub(super) fn crypto_pwhash_scryptsalsa208sha256_str_verify(
            str: *const c_char,
            passwd: *const c_char,
            passwdlen: Ull,
        ) -> c_int;
        pub(super) fn crypto_aead_aes256gcm_is_available() -> c_int;
        pub(super) fn crypto_aead_aes256gcm_encrypt(
            c: *mut u8,
            clen_p: *mut Ull,
            m: *const u8,
            mlen: Ull,
            ad: *const u8,
            adlen: Ull,
            nsec: *const u8,
            npub: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_aead_aes256gcm_decrypt(
            m: *mut u8,
            mlen_p: *mut Ull,
            nsec: *mut u8,
            c: *const u8,
            clen: Ull,
            ad: *const u8,
            adlen: Ull,
            npub: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_aead_chacha20poly1305_encrypt(
            c: *mut u8,
            clen_p: *mut Ull,
            m: *const u8,
            mlen: Ull,
            ad: *const u8,
            adlen: Ull,
            nsec: *const u8,
            npub: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_aead_chacha20poly1305_decrypt(
            m: *mut u8,
            mlen_p: *mut Ull,
            nsec: *mut u8,
            c: *const u8,
            clen: Ull,
            ad: *const u8,
            adlen: Ull,
            npub: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_aead_chacha20poly1305_ietf_encrypt(
            c: *mut u8,
            clen_p: *mut Ull,
            m: *const u8,
            mlen: Ull,
            ad: *const u8,
            adlen: Ull,
            nsec: *const u8,
            npub: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_aead_chacha20poly1305_ietf_decrypt(
            m: *mut u8,
            mlen_p: *mut Ull,
            nsec: *mut u8,
            c: *const u8,
            clen: Ull,
            ad: *const u8,
            adlen: Ull,
            npub: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_aead_xchacha20poly1305_ietf_encrypt(
            c: *mut u8,
            clen_p: *mut Ull,
            m: *const u8,
            mlen: Ull,
            ad: *const u8,
            adlen: Ull,
            nsec: *const u8,
            npub: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_aead_xchacha20poly1305_ietf_decrypt(
            m: *mut u8,
            mlen_p: *mut Ull,
            nsec: *mut u8,
            c: *const u8,
            clen: Ull,
            ad: *const u8,
            adlen: Ull,
            npub: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_kdf_derive_from_key(
            subkey: *mut u8,
            subkey_len: usize,
            subkey_id: u64,
            ctx: *const c_char,
            key: *const u8,
        ) -> c_int;
        pub(super) fn crypto_secretstream_xchacha20poly1305_init_push(
            state: *mut u8,
            header: *mut u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_secretstream_xchacha20poly1305_push(
            state: *mut u8,
            c: *mut u8,
            clen_p: *mut Ull,
            m: *const u8,
            mlen: Ull,
            ad: *const u8,
            adlen: Ull,
            tag: u8,
        ) -> c_int;
        pub(super) fn crypto_secretstream_xchacha20poly1305_init_pull(
            state: *mut u8,
            header: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_secretstream_xchacha20poly1305_pull(
            state: *mut u8,
            m: *mut u8,
            mlen_p: *mut Ull,
            tag_p: *mut u8,
            c: *const u8,
            clen: Ull,
            ad: *const u8,
            adlen: Ull,
        ) -> c_int;
        pub(super) fn crypto_secretstream_xchacha20poly1305_rekey(state: *mut u8);
        pub(super) fn crypto_core_ristretto255_is_valid_point(p: *const u8) -> c_int;
        pub(super) fn crypto_core_ristretto255_add(r: *mut u8, p: *const u8, q: *const u8)
        -> c_int;
        pub(super) fn crypto_core_ristretto255_sub(r: *mut u8, p: *const u8, q: *const u8)
        -> c_int;
        pub(super) fn crypto_core_ristretto255_from_hash(p: *mut u8, r: *const u8) -> c_int;
        pub(super) fn crypto_core_ristretto255_random(p: *mut u8);
        pub(super) fn crypto_core_ristretto255_scalar_random(r: *mut u8);
        pub(super) fn crypto_core_ristretto255_scalar_invert(recip: *mut u8, s: *const u8)
        -> c_int;
        pub(super) fn crypto_core_ristretto255_scalar_negate(neg: *mut u8, s: *const u8);
        pub(super) fn crypto_core_ristretto255_scalar_complement(comp: *mut u8, s: *const u8);
        pub(super) fn crypto_core_ristretto255_scalar_add(z: *mut u8, x: *const u8, y: *const u8);
        pub(super) fn crypto_core_ristretto255_scalar_sub(z: *mut u8, x: *const u8, y: *const u8);
        pub(super) fn crypto_core_ristretto255_scalar_mul(z: *mut u8, x: *const u8, y: *const u8);
        pub(super) fn crypto_core_ristretto255_scalar_reduce(r: *mut u8, s: *const u8);
    }

    // AEGIS-128L and AEGIS-256, which libsodium has from 1.0.19: declared
    // where `build.rs` finds them, the cfg `libsodium_aegis`, as php
    // compiles them under `#ifdef crypto_aead_aegis128l_KEYBYTES`.
    #[cfg(libsodium_aegis)]
    #[link(name = "sodium")]
    unsafe extern "C" {
        pub(super) fn crypto_aead_aegis128l_encrypt(
            c: *mut u8,
            clen_p: *mut Ull,
            m: *const u8,
            mlen: Ull,
            ad: *const u8,
            adlen: Ull,
            nsec: *const u8,
            npub: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_aead_aegis128l_decrypt(
            m: *mut u8,
            mlen_p: *mut Ull,
            nsec: *mut u8,
            c: *const u8,
            clen: Ull,
            ad: *const u8,
            adlen: Ull,
            npub: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_aead_aegis128l_keygen(k: *mut u8);
        pub(super) fn crypto_aead_aegis256_encrypt(
            c: *mut u8,
            clen_p: *mut Ull,
            m: *const u8,
            mlen: Ull,
            ad: *const u8,
            adlen: Ull,
            nsec: *const u8,
            npub: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_aead_aegis256_decrypt(
            m: *mut u8,
            mlen_p: *mut Ull,
            nsec: *mut u8,
            c: *const u8,
            clen: Ull,
            ad: *const u8,
            adlen: Ull,
            npub: *const u8,
            k: *const u8,
        ) -> c_int;
        pub(super) fn crypto_aead_aegis256_keygen(k: *mut u8);
    }
}

/// A call libsodium refused, or a slice shorter than the call reads.
#[derive(Debug)]
pub(crate) struct Failed;

/// Every slice at least as long as the C signature reads from it or
/// writes to it.
fn hold(lengths: &[(usize, usize)]) -> Result<(), Failed> {
    if lengths.iter().all(|(have, need)| have >= need) {
        Ok(())
    } else {
        Err(Failed)
    }
}

/// A libsodium status, `0` for success.
fn status(code: c_int) -> Result<(), Failed> {
    if code == 0 { Ok(()) } else { Err(Failed) }
}

/// A Rust length as libsodium's `unsigned long long`.
fn ull(len: usize) -> c_ulonglong {
    len as c_ulonglong
}

/// libsodium's `unsigned long long` length back as a Rust one, bounded
/// by the buffer it measured.
fn back(len: c_ulonglong, bound: usize) -> Result<usize, Failed> {
    match usize::try_from(len) {
        Ok(len) if len <= bound => Ok(len),
        _ => Err(Failed),
    }
}

/// `sodium_init`, `PHP_MINIT_FUNCTION(sodium)`'s first act: it picks
/// the implementations for the CPU and seeds `randombytes`, and is safe
/// to call from every thread, once or many times. `-1` is its failure.
pub(crate) fn init() -> bool {
    // SAFETY: sodium_init takes nothing and is thread-safe and idempotent.
    let code = unsafe { sys::sodium_init() };
    code >= 0
}

/// `sodium_version_string()`, `SODIUM_LIBRARY_VERSION()`.
pub(crate) fn version() -> String {
    // SAFETY: the library's static NUL-terminated version string.
    let ptr = unsafe { sys::sodium_version_string() };
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: a non-null pointer to a NUL-terminated string that lives
    // as long as the library.
    let text = unsafe { CStr::from_ptr(ptr) };
    text.to_string_lossy().into_owned()
}

/// `sodium_library_version_major()`.
pub(crate) fn version_major() -> i64 {
    // SAFETY: no arguments; answers a constant.
    i64::from(unsafe { sys::sodium_library_version_major() })
}

/// `sodium_library_version_minor()`.
pub(crate) fn version_minor() -> i64 {
    // SAFETY: no arguments; answers a constant.
    i64::from(unsafe { sys::sodium_library_version_minor() })
}

/// `sodium_increment`: `n` as a little-endian number, plus one.
pub(crate) fn increment(n: &mut [u8]) {
    // SAFETY: `n` is valid for `n.len()` writes.
    unsafe { sys::sodium_increment(n.as_mut_ptr(), n.len()) }
}

/// `sodium_add`: `a += b`, little-endian, over `a.len()` bytes.
pub(crate) fn add(a: &mut [u8], b: &[u8]) -> Result<(), Failed> {
    hold(&[(b.len(), a.len())])?;
    // SAFETY: both slices hold at least `a.len()` bytes.
    unsafe { sys::sodium_add(a.as_mut_ptr(), b.as_ptr(), a.len()) }
    Ok(())
}

/// `sodium_memcmp`: `0` where equal, `-1` otherwise, in constant time.
pub(crate) fn memcmp(a: &[u8], b: &[u8]) -> Result<i64, Failed> {
    hold(&[(b.len(), a.len())])?;
    // SAFETY: both slices hold at least `a.len()` bytes.
    let code = unsafe { sys::sodium_memcmp(a.as_ptr().cast(), b.as_ptr().cast(), a.len()) };
    Ok(i64::from(code))
}

/// `sodium_compare`: `-1`, `0` or `1`, little-endian, in constant time.
pub(crate) fn compare(a: &[u8], b: &[u8]) -> Result<i64, Failed> {
    hold(&[(b.len(), a.len())])?;
    // SAFETY: both slices hold at least `a.len()` bytes.
    let code = unsafe { sys::sodium_compare(a.as_ptr(), b.as_ptr(), a.len()) };
    Ok(i64::from(code))
}

/// `sodium_bin2hex` into `hex`, which holds `2 * bin.len() + 1` bytes
/// for the NUL it writes last.
pub(crate) fn bin2hex(hex: &mut [u8], bin: &[u8]) -> Result<(), Failed> {
    let need = bin.len().checked_mul(2).and_then(|n| n.checked_add(1));
    hold(&[(hex.len(), need.ok_or(Failed)?)])?;
    // SAFETY: `hex` holds `hex.len()` bytes, at least `2 * bin.len() + 1`.
    unsafe { sys::sodium_bin2hex(hex.as_mut_ptr().cast(), hex.len(), bin.as_ptr(), bin.len()) };
    Ok(())
}

/// `ignore` as the C string the decoders read: its bytes up to the
/// first NUL, as `strchr` sees a `char *`, with the NUL after them.
fn c_ignore(ignore: &[u8]) -> Vec<u8> {
    let ignore = php_ext::coerce::c_str(ignore);
    let mut owned = Vec::with_capacity(ignore.len() + 1);
    owned.extend_from_slice(ignore);
    owned.push(0);
    owned
}

/// `sodium_hex2bin` into `bin`: the bytes written, or [`Failed`] where
/// the decoder refused or stopped short of the end of `hex`.
pub(crate) fn hex2bin(bin: &mut [u8], hex: &[u8], ignore: &[u8]) -> Result<usize, Failed> {
    let ignore = c_ignore(ignore);
    let mut written = 0usize;
    let mut end: *const c_char = std::ptr::null();
    // SAFETY: `bin` is writable for its length, `hex` readable for its
    // length, `ignore` NUL-terminated, the two outs valid.
    let code = unsafe {
        sys::sodium_hex2bin(
            bin.as_mut_ptr(),
            bin.len(),
            hex.as_ptr().cast(),
            hex.len(),
            ignore.as_ptr().cast(),
            &raw mut written,
            &raw mut end,
        )
    };
    let finished = end == hex.as_ptr_range().end.cast();
    if code != 0 || !finished || written > bin.len() {
        return Err(Failed);
    }
    Ok(written)
}

/// `sodium_base64_ENCODED_LEN(bin_len, variant)`, the bytes the encoder
/// writes with its NUL, the macro's arithmetic as written.
pub(crate) fn base64_encoded_len(bin_len: usize, variant: usize) -> usize {
    let rem = bin_len.wrapping_sub((bin_len / 3).wrapping_mul(3));
    let partial = (rem | (rem >> 1)) & 1;
    let padded = (variant & 2) >> 1;
    let tail = 4usize.wrapping_sub(!padded.wrapping_sub(1) & 3usize.wrapping_sub(rem));
    (bin_len / 3)
        .wrapping_mul(4)
        .wrapping_add(partial.wrapping_mul(tail))
        .wrapping_add(1)
}

/// `sodium_bin2base64` into `b64`, which holds
/// [`base64_encoded_len`] bytes, the NUL last.
pub(crate) fn bin2base64(b64: &mut [u8], bin: &[u8], variant: c_int) -> Result<(), Failed> {
    let variant_bits = usize::try_from(variant).map_err(|_| Failed)?;
    hold(&[(b64.len(), base64_encoded_len(bin.len(), variant_bits))])?;
    // SAFETY: `b64` holds at least the encoded length with its NUL, which
    // is what the encoder writes for `bin` under `variant`.
    let out = unsafe {
        sys::sodium_bin2base64(
            b64.as_mut_ptr().cast(),
            b64.len(),
            bin.as_ptr(),
            bin.len(),
            variant,
        )
    };
    if out.is_null() { Err(Failed) } else { Ok(()) }
}

/// `sodium_base642bin` into `bin`: the bytes written, or [`Failed`]
/// where the decoder refused or stopped short of the end of `b64`.
pub(crate) fn base642bin(
    bin: &mut [u8],
    b64: &[u8],
    ignore: &[u8],
    variant: c_int,
) -> Result<usize, Failed> {
    let ignore = c_ignore(ignore);
    let mut written = 0usize;
    let mut end: *const c_char = std::ptr::null();
    // SAFETY: `bin` is writable for its length, `b64` readable for its
    // length, `ignore` NUL-terminated, the two outs valid.
    let code = unsafe {
        sys::sodium_base642bin(
            bin.as_mut_ptr(),
            bin.len(),
            b64.as_ptr().cast(),
            b64.len(),
            ignore.as_ptr().cast(),
            &raw mut written,
            &raw mut end,
            variant,
        )
    };
    let finished = end == b64.as_ptr_range().end.cast();
    if code != 0 || !finished || written > bin.len() {
        return Err(Failed);
    }
    Ok(written)
}

/// `sodium_pad` over `buf`, whose first `unpadded` bytes are the data:
/// the padded length, at most `buf.len()`.
pub(crate) fn pad(buf: &mut [u8], unpadded: usize, blocksize: usize) -> Result<usize, Failed> {
    hold(&[(buf.len(), unpadded)])?;
    let mut padded = 0usize;
    // SAFETY: `buf` is writable for `buf.len()`, the bound handed as
    // `max_buflen`, which libsodium checks before it writes.
    let code = unsafe {
        sys::sodium_pad(
            &raw mut padded,
            buf.as_mut_ptr(),
            unpadded,
            blocksize,
            buf.len(),
        )
    };
    status(code)?;
    if padded > buf.len() {
        return Err(Failed);
    }
    Ok(padded)
}

/// `sodium_unpad`: the unpadded length of `buf`, or [`Failed`] for a
/// padding it does not find.
pub(crate) fn unpad(buf: &[u8], blocksize: usize) -> Result<usize, Failed> {
    hold(&[(buf.len(), blocksize)])?;
    let mut unpadded = 0usize;
    // SAFETY: `buf` is readable for its length, at least `blocksize`,
    // which is the most libsodium reads back from its end.
    let code = unsafe { sys::sodium_unpad(&raw mut unpadded, buf.as_ptr(), buf.len(), blocksize) };
    status(code)?;
    Ok(unpadded)
}

/// `randombytes_buf`: `buf` filled from the system's generator.
pub(crate) fn random(buf: &mut [u8]) {
    // SAFETY: `buf` is writable for its length.
    unsafe { sys::randombytes_buf(buf.as_mut_ptr().cast(), buf.len()) }
}

/// A call of shape `f(out, m, mlen, k)`: the keyed hashes.
type Keyed = unsafe extern "C" fn(*mut u8, *const u8, c_ulonglong, *const u8) -> c_int;

fn keyed(
    f: Keyed,
    out: &mut [u8],
    outlen: usize,
    m: &[u8],
    k: &[u8],
    keylen: usize,
) -> Result<(), Failed> {
    hold(&[(out.len(), outlen), (k.len(), keylen)])?;
    // SAFETY: `out` holds the output length the primitive writes, `k`
    // the key length it reads, `m` its own length.
    let code = unsafe { f(out.as_mut_ptr(), m.as_ptr(), ull(m.len()), k.as_ptr()) };
    status(code)
}

/// `crypto_shorthash`: 8 bytes keyed by 16.
pub(crate) fn shorthash(out: &mut [u8], m: &[u8], k: &[u8]) -> Result<(), Failed> {
    keyed(sys::crypto_shorthash, out, 8, m, k, 16)
}

/// `crypto_auth`: 32 bytes keyed by 32.
pub(crate) fn auth(out: &mut [u8], m: &[u8], k: &[u8]) -> Result<(), Failed> {
    keyed(sys::crypto_auth, out, 32, m, k, 32)
}

/// `crypto_auth_verify`: whether `h` is `m`'s tag under `k`.
pub(crate) fn auth_verify(h: &[u8], m: &[u8], k: &[u8]) -> Result<bool, Failed> {
    hold(&[(h.len(), 32), (k.len(), 32)])?;
    // SAFETY: `h` and `k` hold the 32 bytes each the call reads.
    let code = unsafe { sys::crypto_auth_verify(h.as_ptr(), m.as_ptr(), ull(m.len()), k.as_ptr()) };
    Ok(code == 0)
}

/// `crypto_secretbox_easy`: `c` holds `m.len() + 16`.
pub(crate) fn secretbox(c: &mut [u8], m: &[u8], n: &[u8], k: &[u8]) -> Result<(), Failed> {
    hold(&[
        (c.len(), m.len().saturating_add(16)),
        (n.len(), 24),
        (k.len(), 32),
    ])?;
    // SAFETY: lengths held above: the ciphertext, the 24-byte nonce and
    // the 32-byte key.
    let code = unsafe {
        sys::crypto_secretbox_easy(
            c.as_mut_ptr(),
            m.as_ptr(),
            ull(m.len()),
            n.as_ptr(),
            k.as_ptr(),
        )
    };
    status(code)
}

/// `crypto_secretbox_open_easy`: `m` holds `c.len() - 16`.
pub(crate) fn secretbox_open(m: &mut [u8], c: &[u8], n: &[u8], k: &[u8]) -> Result<(), Failed> {
    let need = c.len().checked_sub(16).ok_or(Failed)?;
    hold(&[(m.len(), need), (n.len(), 24), (k.len(), 32)])?;
    // SAFETY: lengths held above.
    let code = unsafe {
        sys::crypto_secretbox_open_easy(
            m.as_mut_ptr(),
            c.as_ptr(),
            ull(c.len()),
            n.as_ptr(),
            k.as_ptr(),
        )
    };
    status(code)
}

/// `crypto_generichash` into all of `out`, keyed by `key` (empty for
/// none).
pub(crate) fn generichash(out: &mut [u8], m: &[u8], key: &[u8]) -> Result<(), Failed> {
    let key_ptr = if key.is_empty() {
        std::ptr::null()
    } else {
        key.as_ptr()
    };
    // SAFETY: `out` is writable for its length, `m` and `key` readable for
    // theirs; a null key with a zero length is libsodium's no-key form.
    let code = unsafe {
        sys::crypto_generichash(
            out.as_mut_ptr(),
            out.len(),
            m.as_ptr(),
            ull(m.len()),
            key_ptr,
            key.len(),
        )
    };
    status(code)
}

/// `sizeof(crypto_generichash_state)`, the length of the string php
/// keeps the state in.
pub(crate) const GENERICHASH_STATE: usize = 384;

/// A `crypto_generichash_state` on the 64-byte boundary the C type has,
/// copied in and out of the string php keeps it in.
pub(crate) struct GenericHash(Box<sys::GenericHashState>);

impl GenericHash {
    /// `crypto_generichash_init`.
    pub(crate) fn init(key: &[u8], outlen: usize) -> Result<GenericHash, Failed> {
        let mut state = Box::new(sys::GenericHashState([0; GENERICHASH_STATE]));
        let key_ptr = if key.is_empty() {
            std::ptr::null()
        } else {
            key.as_ptr()
        };
        // SAFETY: `state` is an aligned state; `key` readable for its
        // length, null with a zero length for none.
        let code =
            unsafe { sys::crypto_generichash_init(&raw mut *state, key_ptr, key.len(), outlen) };
        status(code)?;
        Ok(GenericHash(state))
    }

    /// A state read back from its bytes, `memcpy(&state_tmp, state, ..)`.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<GenericHash, Failed> {
        let bytes: &[u8; GENERICHASH_STATE] = bytes.try_into().map_err(|_| Failed)?;
        Ok(GenericHash(Box::new(sys::GenericHashState(*bytes))))
    }

    /// The state's bytes.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0.0
    }

    /// `crypto_generichash_update`.
    pub(crate) fn update(&mut self, m: &[u8]) -> Result<(), Failed> {
        // SAFETY: an aligned state, `m` readable for its length.
        let code =
            unsafe { sys::crypto_generichash_update(&raw mut *self.0, m.as_ptr(), ull(m.len())) };
        status(code)
    }

    /// `crypto_generichash_final` into all of `out`.
    pub(crate) fn finish(&mut self, out: &mut [u8]) -> Result<(), Failed> {
        // SAFETY: an aligned state, `out` writable for its length, which
        // libsodium holds to the state's own output length.
        let code =
            unsafe { sys::crypto_generichash_final(&raw mut *self.0, out.as_mut_ptr(), out.len()) };
        status(code)
    }
}

impl Drop for GenericHash {
    /// `sodium_memzero(&state_tmp, sizeof state_tmp)`.
    fn drop(&mut self) {
        self.0.0.fill(0);
    }
}

/// A key pair call `f(pk, sk)` into `keypair`, the secret key first and
/// the public key after it, as php lays them in one string.
type Pair = unsafe extern "C" fn(*mut u8, *mut u8) -> c_int;
type SeedPair = unsafe extern "C" fn(*mut u8, *mut u8, *const u8) -> c_int;

fn pair(f: Pair, keypair: &mut [u8], sk_len: usize, pk_len: usize) -> Result<(), Failed> {
    hold(&[(keypair.len(), sk_len + pk_len)])?;
    let (sk, pk) = keypair.split_at_mut(sk_len);
    // SAFETY: `sk` and `pk` hold the lengths the call writes.
    let code = unsafe { f(pk.as_mut_ptr(), sk.as_mut_ptr()) };
    status(code)
}

fn seed_pair(
    f: SeedPair,
    keypair: &mut [u8],
    sk_len: usize,
    pk_len: usize,
    seed: &[u8],
) -> Result<(), Failed> {
    hold(&[(keypair.len(), sk_len + pk_len), (seed.len(), 32)])?;
    let (sk, pk) = keypair.split_at_mut(sk_len);
    // SAFETY: `sk` and `pk` hold the lengths the call writes, `seed` the
    // 32 bytes it reads.
    let code = unsafe { f(pk.as_mut_ptr(), sk.as_mut_ptr(), seed.as_ptr()) };
    status(code)
}

/// `crypto_box_keypair` into 64 bytes, the secret key first.
pub(crate) fn box_keypair(keypair: &mut [u8]) -> Result<(), Failed> {
    pair(sys::crypto_box_keypair, keypair, 32, 32)
}

/// `crypto_box_seed_keypair` into 64 bytes, the secret key first.
pub(crate) fn box_seed_keypair(keypair: &mut [u8], seed: &[u8]) -> Result<(), Failed> {
    seed_pair(sys::crypto_box_seed_keypair, keypair, 32, 32, seed)
}

/// `crypto_sign_keypair` into 96 bytes, the secret key first.
pub(crate) fn sign_keypair(keypair: &mut [u8]) -> Result<(), Failed> {
    pair(sys::crypto_sign_keypair, keypair, 64, 32)
}

/// `crypto_sign_seed_keypair` into 96 bytes, the secret key first.
pub(crate) fn sign_seed_keypair(keypair: &mut [u8], seed: &[u8]) -> Result<(), Failed> {
    seed_pair(sys::crypto_sign_seed_keypair, keypair, 64, 32, seed)
}

/// A call `f(out, in)` over fixed lengths.
type Unary = unsafe extern "C" fn(*mut u8, *const u8) -> c_int;
type UnaryVoid = unsafe extern "C" fn(*mut u8, *const u8);
type Binary = unsafe extern "C" fn(*mut u8, *const u8, *const u8) -> c_int;
type BinaryVoid = unsafe extern "C" fn(*mut u8, *const u8, *const u8);

fn unary(f: Unary, out: &mut [u8], out_len: usize, s: &[u8], s_len: usize) -> Result<(), Failed> {
    hold(&[(out.len(), out_len), (s.len(), s_len)])?;
    // SAFETY: lengths held above.
    let code = unsafe { f(out.as_mut_ptr(), s.as_ptr()) };
    status(code)
}

fn unary_void(
    f: UnaryVoid,
    out: &mut [u8],
    out_len: usize,
    s: &[u8],
    s_len: usize,
) -> Result<(), Failed> {
    hold(&[(out.len(), out_len), (s.len(), s_len)])?;
    // SAFETY: lengths held above.
    unsafe { f(out.as_mut_ptr(), s.as_ptr()) };
    Ok(())
}

fn binary(f: Binary, out: &mut [u8], a: &[u8], b: &[u8]) -> Result<(), Failed> {
    hold(&[(out.len(), 32), (a.len(), 32), (b.len(), 32)])?;
    // SAFETY: the 32 bytes each the curve calls write and read.
    let code = unsafe { f(out.as_mut_ptr(), a.as_ptr(), b.as_ptr()) };
    status(code)
}

fn binary_void(f: BinaryVoid, out: &mut [u8], a: &[u8], b: &[u8]) -> Result<(), Failed> {
    hold(&[(out.len(), 32), (a.len(), 32), (b.len(), 32)])?;
    // SAFETY: the 32 bytes each the scalar calls write and read.
    unsafe { f(out.as_mut_ptr(), a.as_ptr(), b.as_ptr()) };
    Ok(())
}

/// `crypto_scalarmult_base`, 32 bytes from a 32-byte scalar.
pub(crate) fn scalarmult_base(q: &mut [u8], n: &[u8]) -> Result<(), Failed> {
    unary(sys::crypto_scalarmult_base, q, 32, n, 32)
}

/// `crypto_scalarmult`.
pub(crate) fn scalarmult(q: &mut [u8], n: &[u8], p: &[u8]) -> Result<(), Failed> {
    binary(sys::crypto_scalarmult, q, n, p)
}

/// `crypto_scalarmult_ristretto255`.
pub(crate) fn scalarmult_ristretto255(q: &mut [u8], n: &[u8], p: &[u8]) -> Result<(), Failed> {
    binary(sys::crypto_scalarmult_ristretto255, q, n, p)
}

/// `crypto_scalarmult_ristretto255_base`.
pub(crate) fn scalarmult_ristretto255_base(q: &mut [u8], n: &[u8]) -> Result<(), Failed> {
    unary(sys::crypto_scalarmult_ristretto255_base, q, 32, n, 32)
}

/// `crypto_sign_ed25519_sk_to_pk`: 32 bytes from the 64-byte secret key.
pub(crate) fn sign_sk_to_pk(pk: &mut [u8], sk: &[u8]) -> Result<(), Failed> {
    unary(sys::crypto_sign_ed25519_sk_to_pk, pk, 32, sk, 64)
}

/// `crypto_sign_ed25519_sk_to_curve25519`.
pub(crate) fn sign_sk_to_curve25519(out: &mut [u8], sk: &[u8]) -> Result<(), Failed> {
    unary(sys::crypto_sign_ed25519_sk_to_curve25519, out, 32, sk, 64)
}

/// `crypto_sign_ed25519_pk_to_curve25519`.
pub(crate) fn sign_pk_to_curve25519(out: &mut [u8], pk: &[u8]) -> Result<(), Failed> {
    unary(sys::crypto_sign_ed25519_pk_to_curve25519, out, 32, pk, 32)
}

/// `crypto_box_easy` with the key pair's secret key first: `c` holds
/// `m.len() + 16`.
pub(crate) fn box_easy(c: &mut [u8], m: &[u8], n: &[u8], keypair: &[u8]) -> Result<(), Failed> {
    hold(&[
        (c.len(), m.len().saturating_add(16)),
        (n.len(), 24),
        (keypair.len(), 64),
    ])?;
    let (sk, pk) = keypair.split_at(32);
    // SAFETY: lengths held above.
    let code = unsafe {
        sys::crypto_box_easy(
            c.as_mut_ptr(),
            m.as_ptr(),
            ull(m.len()),
            n.as_ptr(),
            pk.as_ptr(),
            sk.as_ptr(),
        )
    };
    status(code)
}

/// `crypto_box_open_easy`: `m` holds `c.len() - 16`.
pub(crate) fn box_open(m: &mut [u8], c: &[u8], n: &[u8], keypair: &[u8]) -> Result<(), Failed> {
    let need = c.len().checked_sub(16).ok_or(Failed)?;
    hold(&[(m.len(), need), (n.len(), 24), (keypair.len(), 64)])?;
    let (sk, pk) = keypair.split_at(32);
    // SAFETY: lengths held above.
    let code = unsafe {
        sys::crypto_box_open_easy(
            m.as_mut_ptr(),
            c.as_ptr(),
            ull(c.len()),
            n.as_ptr(),
            pk.as_ptr(),
            sk.as_ptr(),
        )
    };
    status(code)
}

/// `crypto_box_seal`: `c` holds `m.len() + 48`.
pub(crate) fn box_seal(c: &mut [u8], m: &[u8], pk: &[u8]) -> Result<(), Failed> {
    hold(&[(c.len(), m.len().saturating_add(48)), (pk.len(), 32)])?;
    // SAFETY: lengths held above.
    let code =
        unsafe { sys::crypto_box_seal(c.as_mut_ptr(), m.as_ptr(), ull(m.len()), pk.as_ptr()) };
    status(code)
}

/// `crypto_box_seal_open` with the key pair's secret key first: `m`
/// holds `c.len() - 48`.
pub(crate) fn box_seal_open(m: &mut [u8], c: &[u8], keypair: &[u8]) -> Result<(), Failed> {
    let need = c.len().checked_sub(48).ok_or(Failed)?;
    hold(&[(m.len(), need), (keypair.len(), 64)])?;
    let (sk, pk) = keypair.split_at(32);
    // SAFETY: lengths held above.
    let code = unsafe {
        sys::crypto_box_seal_open(
            m.as_mut_ptr(),
            c.as_ptr(),
            ull(c.len()),
            pk.as_ptr(),
            sk.as_ptr(),
        )
    };
    status(code)
}

/// `crypto_sign` into `sm`, which holds `m.len() + 64`: the signed
/// message's length.
pub(crate) fn sign(sm: &mut [u8], m: &[u8], sk: &[u8]) -> Result<usize, Failed> {
    hold(&[(sm.len(), m.len().saturating_add(64)), (sk.len(), 64)])?;
    let mut len: c_ulonglong = 0;
    // SAFETY: lengths held above, `len` a valid out.
    let code = unsafe {
        sys::crypto_sign(
            sm.as_mut_ptr(),
            &raw mut len,
            m.as_ptr(),
            ull(m.len()),
            sk.as_ptr(),
        )
    };
    status(code)?;
    back(len, sm.len())
}

/// `crypto_sign_open` into `m`, which holds `sm.len()`: the message's
/// length, or [`Failed`] for a signature that does not verify.
pub(crate) fn sign_open(m: &mut [u8], sm: &[u8], pk: &[u8]) -> Result<usize, Failed> {
    hold(&[(m.len(), sm.len()), (pk.len(), 32)])?;
    let mut len: c_ulonglong = 0;
    // SAFETY: lengths held above, `len` a valid out.
    let code = unsafe {
        sys::crypto_sign_open(
            m.as_mut_ptr(),
            &raw mut len,
            sm.as_ptr(),
            ull(sm.len()),
            pk.as_ptr(),
        )
    };
    status(code)?;
    back(len, m.len())
}

/// `crypto_sign_detached` into 64 bytes: the signature's length.
pub(crate) fn sign_detached(sig: &mut [u8], m: &[u8], sk: &[u8]) -> Result<u64, Failed> {
    hold(&[(sig.len(), 64), (sk.len(), 64)])?;
    let mut len: c_ulonglong = 0;
    // SAFETY: lengths held above, `len` a valid out.
    let code = unsafe {
        sys::crypto_sign_detached(
            sig.as_mut_ptr(),
            &raw mut len,
            m.as_ptr(),
            ull(m.len()),
            sk.as_ptr(),
        )
    };
    status(code)?;
    Ok(len)
}

/// `crypto_sign_verify_detached`.
pub(crate) fn sign_verify_detached(sig: &[u8], m: &[u8], pk: &[u8]) -> Result<bool, Failed> {
    hold(&[(sig.len(), 64), (pk.len(), 32)])?;
    // SAFETY: lengths held above.
    let code = unsafe {
        sys::crypto_sign_verify_detached(sig.as_ptr(), m.as_ptr(), ull(m.len()), pk.as_ptr())
    };
    Ok(code == 0)
}

/// The two stream ciphers php exposes.
#[derive(Clone, Copy)]
pub(crate) enum Stream {
    /// `crypto_stream`, `XSalsa20`.
    XSalsa20,
    /// `crypto_stream_xchacha20`.
    XChaCha20,
}

/// `crypto_stream` or `crypto_stream_xchacha20`: all of `c` filled with
/// the keystream; both take a 24-byte nonce and a 32-byte key.
pub(crate) fn stream(which: Stream, c: &mut [u8], n: &[u8], k: &[u8]) -> Result<(), Failed> {
    hold(&[(n.len(), 24), (k.len(), 32)])?;
    let f = match which {
        Stream::XSalsa20 => sys::crypto_stream,
        Stream::XChaCha20 => sys::crypto_stream_xchacha20,
    };
    // SAFETY: `c` writable for its length; nonce and key held above.
    let code = unsafe { f(c.as_mut_ptr(), ull(c.len()), n.as_ptr(), k.as_ptr()) };
    status(code)
}

/// `crypto_stream_xor` or `crypto_stream_xchacha20_xor`: `c` holds
/// `m.len()`.
pub(crate) fn stream_xor(
    which: Stream,
    out: &mut [u8],
    msg: &[u8],
    nonce: &[u8],
    key: &[u8],
) -> Result<(), Failed> {
    hold(&[(out.len(), msg.len()), (nonce.len(), 24), (key.len(), 32)])?;
    let xor = match which {
        Stream::XSalsa20 => sys::crypto_stream_xor,
        Stream::XChaCha20 => sys::crypto_stream_xchacha20_xor,
    };
    // SAFETY: lengths held above.
    let code = unsafe {
        xor(
            out.as_mut_ptr(),
            msg.as_ptr(),
            ull(msg.len()),
            nonce.as_ptr(),
            key.as_ptr(),
        )
    };
    status(code)
}

/// `crypto_stream_xchacha20_xor_ic`.
pub(crate) fn stream_xchacha20_xor_ic(
    c: &mut [u8],
    m: &[u8],
    n: &[u8],
    ic: u64,
    k: &[u8],
) -> Result<(), Failed> {
    hold(&[(c.len(), m.len()), (n.len(), 24), (k.len(), 32)])?;
    // SAFETY: lengths held above.
    let code = unsafe {
        sys::crypto_stream_xchacha20_xor_ic(
            c.as_mut_ptr(),
            m.as_ptr(),
            ull(m.len()),
            n.as_ptr(),
            ic,
            k.as_ptr(),
        )
    };
    status(code)
}

/// `crypto_pwhash` into all of `out`, a 16-byte salt.
pub(crate) fn pwhash(
    out: &mut [u8],
    passwd: &[u8],
    salt: &[u8],
    opslimit: u64,
    memlimit: usize,
    alg: c_int,
) -> Result<(), Failed> {
    hold(&[(salt.len(), 16)])?;
    // SAFETY: `out` writable for its length, `passwd` readable for its
    // length, the salt held above.
    let code = unsafe {
        sys::crypto_pwhash(
            out.as_mut_ptr(),
            ull(out.len()),
            passwd.as_ptr().cast(),
            ull(passwd.len()),
            salt.as_ptr(),
            opslimit,
            memlimit,
            alg,
        )
    };
    status(code)
}

/// `crypto_pwhash_scryptsalsa208sha256` into all of `out`, a 32-byte
/// salt.
pub(crate) fn pwhash_scrypt(
    out: &mut [u8],
    passwd: &[u8],
    salt: &[u8],
    opslimit: u64,
    memlimit: usize,
) -> Result<(), Failed> {
    hold(&[(salt.len(), 32)])?;
    // SAFETY: `out` writable for its length, `passwd` readable for its
    // length, the salt held above.
    let code = unsafe {
        sys::crypto_pwhash_scryptsalsa208sha256(
            out.as_mut_ptr(),
            ull(out.len()),
            passwd.as_ptr().cast(),
            ull(passwd.len()),
            salt.as_ptr(),
            opslimit,
            memlimit,
        )
    };
    status(code)
}

/// `crypto_pwhash_STRBYTES`.
pub(crate) const PWHASH_STRBYTES: usize = 128;

/// `crypto_pwhash_scryptsalsa208sha256_STRBYTES`.
pub(crate) const SCRYPT_STRBYTES: usize = 102;

/// `crypto_pwhash_str` into 128 bytes.
pub(crate) fn pwhash_str(
    out: &mut [u8],
    passwd: &[u8],
    opslimit: u64,
    memlimit: usize,
) -> Result<(), Failed> {
    hold(&[(out.len(), PWHASH_STRBYTES)])?;
    // SAFETY: `out` holds the 128 bytes the call writes.
    let code = unsafe {
        sys::crypto_pwhash_str(
            out.as_mut_ptr().cast(),
            passwd.as_ptr().cast(),
            ull(passwd.len()),
            opslimit,
            memlimit,
        )
    };
    status(code)
}

/// `crypto_pwhash_scryptsalsa208sha256_str` into 102 bytes.
pub(crate) fn pwhash_scrypt_str(
    out: &mut [u8],
    passwd: &[u8],
    opslimit: u64,
    memlimit: usize,
) -> Result<(), Failed> {
    hold(&[(out.len(), SCRYPT_STRBYTES)])?;
    // SAFETY: `out` holds the 102 bytes the call writes.
    let code = unsafe {
        sys::crypto_pwhash_scryptsalsa208sha256_str(
            out.as_mut_ptr().cast(),
            passwd.as_ptr().cast(),
            ull(passwd.len()),
            opslimit,
            memlimit,
        )
    };
    status(code)
}

/// A PHP string as the `char *` php hands libsodium: its bytes and the
/// NUL a `zend_string` keeps after them.
fn c_string(bytes: &[u8]) -> Vec<u8> {
    let mut owned = Vec::with_capacity(bytes.len() + 1);
    owned.extend_from_slice(bytes);
    owned.push(0);
    owned
}

/// `crypto_pwhash_str_verify`.
pub(crate) fn pwhash_str_verify(hash: &[u8], passwd: &[u8]) -> bool {
    let hash = c_string(hash);
    // SAFETY: `hash` is NUL-terminated, `passwd` readable for its length.
    let code = unsafe {
        sys::crypto_pwhash_str_verify(
            hash.as_ptr().cast(),
            passwd.as_ptr().cast(),
            ull(passwd.len()),
        )
    };
    code == 0
}

/// `crypto_pwhash_str_needs_rehash`: `0` where the hash meets the
/// limits.
pub(crate) fn pwhash_str_needs_rehash(hash: &[u8], opslimit: u64, memlimit: usize) -> bool {
    let hash = c_string(hash);
    // SAFETY: `hash` is NUL-terminated.
    let code =
        unsafe { sys::crypto_pwhash_str_needs_rehash(hash.as_ptr().cast(), opslimit, memlimit) };
    code != 0
}

/// `crypto_pwhash_scryptsalsa208sha256_str_verify` over a hash php has
/// held to 101 bytes.
pub(crate) fn pwhash_scrypt_str_verify(hash: &[u8], passwd: &[u8]) -> Result<bool, Failed> {
    hold(&[(hash.len(), SCRYPT_STRBYTES - 1)])?;
    let hash = c_string(hash);
    // SAFETY: `hash` holds the 102 bytes, NUL last, the call reads.
    let code = unsafe {
        sys::crypto_pwhash_scryptsalsa208sha256_str_verify(
            hash.as_ptr().cast(),
            passwd.as_ptr().cast(),
            ull(passwd.len()),
        )
    };
    Ok(code == 0)
}

/// `crypto_aead_aes256gcm_is_available`.
pub(crate) fn aes256gcm_is_available() -> bool {
    // SAFETY: no arguments; reads the CPU features `sodium_init` found.
    unsafe { sys::crypto_aead_aes256gcm_is_available() == 1 }
}

/// One AEAD construction: its two calls and its lengths.
pub(crate) struct Aead {
    encrypt: sys::AeadEncrypt,
    decrypt: sys::AeadDecrypt,
    /// `*_KEYBYTES`.
    pub(crate) key: usize,
    /// `*_NPUBBYTES`.
    pub(crate) npub: usize,
    /// `*_ABYTES`.
    pub(crate) abytes: usize,
}

pub(crate) const AES256GCM: Aead = Aead {
    encrypt: sys::crypto_aead_aes256gcm_encrypt,
    decrypt: sys::crypto_aead_aes256gcm_decrypt,
    key: 32,
    npub: 12,
    abytes: 16,
};

#[cfg(libsodium_aegis)]
pub(crate) const AEGIS128L: Aead = Aead {
    encrypt: sys::crypto_aead_aegis128l_encrypt,
    decrypt: sys::crypto_aead_aegis128l_decrypt,
    key: 16,
    npub: 16,
    abytes: 32,
};

#[cfg(libsodium_aegis)]
pub(crate) const AEGIS256: Aead = Aead {
    encrypt: sys::crypto_aead_aegis256_encrypt,
    decrypt: sys::crypto_aead_aegis256_decrypt,
    key: 32,
    npub: 32,
    abytes: 32,
};

pub(crate) const CHACHA20POLY1305: Aead = Aead {
    encrypt: sys::crypto_aead_chacha20poly1305_encrypt,
    decrypt: sys::crypto_aead_chacha20poly1305_decrypt,
    key: 32,
    npub: 8,
    abytes: 16,
};

pub(crate) const CHACHA20POLY1305_IETF: Aead = Aead {
    encrypt: sys::crypto_aead_chacha20poly1305_ietf_encrypt,
    decrypt: sys::crypto_aead_chacha20poly1305_ietf_decrypt,
    key: 32,
    npub: 12,
    abytes: 16,
};

pub(crate) const XCHACHA20POLY1305_IETF: Aead = Aead {
    encrypt: sys::crypto_aead_xchacha20poly1305_ietf_encrypt,
    decrypt: sys::crypto_aead_xchacha20poly1305_ietf_decrypt,
    key: 32,
    npub: 24,
    abytes: 16,
};

impl Aead {
    /// `*_encrypt` into `c`, which holds `m.len() + abytes`: the
    /// ciphertext's length.
    pub(crate) fn seal(
        &self,
        c: &mut [u8],
        m: &[u8],
        ad: &[u8],
        npub: &[u8],
        k: &[u8],
    ) -> Result<u64, Failed> {
        hold(&[
            (c.len(), m.len().saturating_add(self.abytes)),
            (npub.len(), self.npub),
            (k.len(), self.key),
        ])?;
        let mut len: c_ulonglong = 0;
        // SAFETY: lengths held above, `len` a valid out, no `nsec`.
        let code = unsafe {
            (self.encrypt)(
                c.as_mut_ptr(),
                &raw mut len,
                m.as_ptr(),
                ull(m.len()),
                ad.as_ptr(),
                ull(ad.len()),
                std::ptr::null(),
                npub.as_ptr(),
                k.as_ptr(),
            )
        };
        status(code)?;
        Ok(len)
    }

    /// `*_decrypt` into `m`, which holds `c.len()`: the message's length,
    /// or [`Failed`] for a ciphertext that does not verify.
    pub(crate) fn open(
        &self,
        m: &mut [u8],
        c: &[u8],
        ad: &[u8],
        npub: &[u8],
        k: &[u8],
    ) -> Result<u64, Failed> {
        hold(&[
            (m.len(), c.len()),
            (npub.len(), self.npub),
            (k.len(), self.key),
        ])?;
        let mut len: c_ulonglong = 0;
        // SAFETY: lengths held above, `len` a valid out, no `nsec`.
        let code = unsafe {
            (self.decrypt)(
                m.as_mut_ptr(),
                &raw mut len,
                std::ptr::null_mut(),
                c.as_ptr(),
                ull(c.len()),
                ad.as_ptr(),
                ull(ad.len()),
                npub.as_ptr(),
                k.as_ptr(),
            )
        };
        status(code)?;
        Ok(len)
    }
}

/// `crypto_aead_aegis128l_keygen` or `crypto_aead_aegis256_keygen` into
/// `k`, whose length names the construction.
#[cfg(libsodium_aegis)]
pub(crate) fn aegis_keygen(k: &mut [u8]) -> Result<(), Failed> {
    let f = match k.len() {
        16 => sys::crypto_aead_aegis128l_keygen,
        32 => sys::crypto_aead_aegis256_keygen,
        _ => return Err(Failed),
    };
    // SAFETY: `k` holds the key length of the construction chosen by it.
    unsafe { f(k.as_mut_ptr()) };
    Ok(())
}

/// `crypto_kdf_derive_from_key` into all of `subkey`: an 8-byte `ctx`
/// and a 32-byte key.
pub(crate) fn kdf_derive(subkey: &mut [u8], id: u64, ctx: &[u8], key: &[u8]) -> Result<(), Failed> {
    hold(&[(ctx.len(), 8), (key.len(), 32)])?;
    // SAFETY: lengths held above; libsodium holds `subkey.len()` to its
    // bounds and refuses outside them.
    let code = unsafe {
        sys::crypto_kdf_derive_from_key(
            subkey.as_mut_ptr(),
            subkey.len(),
            id,
            ctx.as_ptr().cast(),
            key.as_ptr(),
        )
    };
    status(code)
}

/// `sizeof(crypto_secretstream_xchacha20poly1305_state)`: a 32-byte
/// key, a 12-byte nonce and 8 bytes of padding, bytes only.
pub(crate) const SECRETSTREAM_STATE: usize = 52;

/// `crypto_secretstream_xchacha20poly1305_ABYTES`.
pub(crate) const SECRETSTREAM_ABYTES: usize = 17;

/// `crypto_secretstream_xchacha20poly1305_init_push`: the state and the
/// 24-byte header.
pub(crate) fn secretstream_init_push(
    state: &mut [u8],
    header: &mut [u8],
    k: &[u8],
) -> Result<(), Failed> {
    hold(&[
        (state.len(), SECRETSTREAM_STATE),
        (header.len(), 24),
        (k.len(), 32),
    ])?;
    // SAFETY: lengths held above; the state is bytes only, so any
    // alignment does.
    let code = unsafe {
        sys::crypto_secretstream_xchacha20poly1305_init_push(
            state.as_mut_ptr(),
            header.as_mut_ptr(),
            k.as_ptr(),
        )
    };
    status(code)
}

/// `crypto_secretstream_xchacha20poly1305_init_pull`.
pub(crate) fn secretstream_init_pull(
    state: &mut [u8],
    header: &[u8],
    k: &[u8],
) -> Result<(), Failed> {
    hold(&[
        (state.len(), SECRETSTREAM_STATE),
        (header.len(), 24),
        (k.len(), 32),
    ])?;
    // SAFETY: lengths held above.
    let code = unsafe {
        sys::crypto_secretstream_xchacha20poly1305_init_pull(
            state.as_mut_ptr(),
            header.as_ptr(),
            k.as_ptr(),
        )
    };
    status(code)
}

/// `crypto_secretstream_xchacha20poly1305_push` into `c`, which holds
/// `m.len() + 17`: the ciphertext's length.
pub(crate) fn secretstream_push(
    state: &mut [u8],
    c: &mut [u8],
    m: &[u8],
    ad: &[u8],
    tag: u8,
) -> Result<u64, Failed> {
    hold(&[
        (state.len(), SECRETSTREAM_STATE),
        (c.len(), m.len().saturating_add(SECRETSTREAM_ABYTES)),
    ])?;
    let mut len: c_ulonglong = 0;
    // SAFETY: lengths held above, `len` a valid out.
    let code = unsafe {
        sys::crypto_secretstream_xchacha20poly1305_push(
            state.as_mut_ptr(),
            c.as_mut_ptr(),
            &raw mut len,
            m.as_ptr(),
            ull(m.len()),
            ad.as_ptr(),
            ull(ad.len()),
            tag,
        )
    };
    status(code)?;
    Ok(len)
}

/// `crypto_secretstream_xchacha20poly1305_pull` into `m`, which holds
/// `c.len() - 17`: the message's length and its tag.
pub(crate) fn secretstream_pull(
    state: &mut [u8],
    m: &mut [u8],
    c: &[u8],
    ad: &[u8],
) -> Result<(u64, u8), Failed> {
    let need = c.len().checked_sub(SECRETSTREAM_ABYTES).ok_or(Failed)?;
    hold(&[(state.len(), SECRETSTREAM_STATE), (m.len(), need)])?;
    let mut len: c_ulonglong = 0;
    let mut tag = 0u8;
    // SAFETY: lengths held above, `len` and `tag` valid outs.
    let code = unsafe {
        sys::crypto_secretstream_xchacha20poly1305_pull(
            state.as_mut_ptr(),
            m.as_mut_ptr(),
            &raw mut len,
            &raw mut tag,
            c.as_ptr(),
            ull(c.len()),
            ad.as_ptr(),
            ull(ad.len()),
        )
    };
    status(code)?;
    Ok((len, tag))
}

/// `crypto_secretstream_xchacha20poly1305_rekey`.
pub(crate) fn secretstream_rekey(state: &mut [u8]) -> Result<(), Failed> {
    hold(&[(state.len(), SECRETSTREAM_STATE)])?;
    // SAFETY: the state's 52 bytes, held above.
    unsafe { sys::crypto_secretstream_xchacha20poly1305_rekey(state.as_mut_ptr()) };
    Ok(())
}

/// `crypto_core_ristretto255_is_valid_point`.
pub(crate) fn ristretto255_is_valid_point(p: &[u8]) -> Result<bool, Failed> {
    hold(&[(p.len(), 32)])?;
    // SAFETY: the 32 bytes the call reads, held above.
    let code = unsafe { sys::crypto_core_ristretto255_is_valid_point(p.as_ptr()) };
    Ok(code == 1)
}

/// `crypto_core_ristretto255_add`.
pub(crate) fn ristretto255_add(r: &mut [u8], p: &[u8], q: &[u8]) -> Result<(), Failed> {
    binary(sys::crypto_core_ristretto255_add, r, p, q)
}

/// `crypto_core_ristretto255_sub`.
pub(crate) fn ristretto255_sub(r: &mut [u8], p: &[u8], q: &[u8]) -> Result<(), Failed> {
    binary(sys::crypto_core_ristretto255_sub, r, p, q)
}

/// `crypto_core_ristretto255_from_hash`: 32 bytes from 64.
pub(crate) fn ristretto255_from_hash(p: &mut [u8], r: &[u8]) -> Result<(), Failed> {
    unary(sys::crypto_core_ristretto255_from_hash, p, 32, r, 64)
}

/// `crypto_core_ristretto255_random` or `_scalar_random` into 32 bytes.
pub(crate) fn ristretto255_random(out: &mut [u8], scalar: bool) -> Result<(), Failed> {
    hold(&[(out.len(), 32)])?;
    let f = if scalar {
        sys::crypto_core_ristretto255_scalar_random
    } else {
        sys::crypto_core_ristretto255_random
    };
    // SAFETY: the 32 bytes the call writes, held above.
    unsafe { f(out.as_mut_ptr()) };
    Ok(())
}

/// `crypto_core_ristretto255_scalar_invert`.
pub(crate) fn ristretto255_scalar_invert(out: &mut [u8], s: &[u8]) -> Result<(), Failed> {
    unary(sys::crypto_core_ristretto255_scalar_invert, out, 32, s, 32)
}

/// `crypto_core_ristretto255_scalar_negate`.
pub(crate) fn ristretto255_scalar_negate(out: &mut [u8], s: &[u8]) -> Result<(), Failed> {
    unary_void(sys::crypto_core_ristretto255_scalar_negate, out, 32, s, 32)
}

/// `crypto_core_ristretto255_scalar_complement`.
pub(crate) fn ristretto255_scalar_complement(out: &mut [u8], s: &[u8]) -> Result<(), Failed> {
    unary_void(
        sys::crypto_core_ristretto255_scalar_complement,
        out,
        32,
        s,
        32,
    )
}

/// `crypto_core_ristretto255_scalar_reduce`: 32 bytes from 64.
pub(crate) fn ristretto255_scalar_reduce(out: &mut [u8], s: &[u8]) -> Result<(), Failed> {
    unary_void(sys::crypto_core_ristretto255_scalar_reduce, out, 32, s, 64)
}

/// `crypto_core_ristretto255_scalar_add`.
pub(crate) fn ristretto255_scalar_add(z: &mut [u8], x: &[u8], y: &[u8]) -> Result<(), Failed> {
    binary_void(sys::crypto_core_ristretto255_scalar_add, z, x, y)
}

/// `crypto_core_ristretto255_scalar_sub`.
pub(crate) fn ristretto255_scalar_sub(z: &mut [u8], x: &[u8], y: &[u8]) -> Result<(), Failed> {
    binary_void(sys::crypto_core_ristretto255_scalar_sub, z, x, y)
}

/// `crypto_core_ristretto255_scalar_mul`.
pub(crate) fn ristretto255_scalar_mul(z: &mut [u8], x: &[u8], y: &[u8]) -> Result<(), Failed> {
    binary_void(sys::crypto_core_ristretto255_scalar_mul, z, x, y)
}
