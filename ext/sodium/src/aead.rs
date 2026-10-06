//! The six AEAD constructions of `libsodium.c`: AES-256-GCM,
//! AEGIS-128L, AEGIS-256, ChaCha20-Poly1305 in its original and its
//! IETF form, and XChaCha20-Poly1305, each an `_encrypt`, a `_decrypt`
//! and a `_keygen`. The six differ only in their lengths and in the
//! bounds php holds a message to, which [`Guard`] names. The two AEGIS
//! constructions are `aead::aegis`, which exists only where the linked
//! libsodium has them.

use php_ext::abi::{OrFalse, Raw};
use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString};

use crate::ffi::{self, Aead};
use crate::{ZSTR_MAX_LEN, bad_arg, fail, internal, made, parsed, random_key};

#[cfg(libsodium_aegis)]
pub(crate) mod aegis;

/// The bounds `libsodium.c` holds one construction's message and
/// ciphertext to beyond its lengths, and the order it checks them in.
#[derive(Clone, Copy)]
enum Guard {
    /// AES-256-GCM: `16 * (2^32 - 2)` bytes under one key.
    Aes,
    /// AEGIS and the original ChaCha20-Poly1305: none.
    Plain,
    /// The IETF ChaCha20-Poly1305: `64 * 2^32 - 64` bytes under one key.
    Ietf,
    /// XChaCha20-Poly1305: the IETF bound, refused as an overflow.
    XChaCha,
}

/// `16ULL * ((1ULL << 32) - 2ULL)`.
const AES_MAX: u64 = 16 * ((1 << 32) - 2);

/// `64ULL * (1ULL << 32) - 64ULL`.
const IETF_MAX: u64 = 64 * (1 << 32) - 64;

/// One construction as php names it: the cipher and the infix of its
/// constants, `SODIUM_CRYPTO_AEAD_<name>_NPUBBYTES`.
struct Construction {
    aead: Aead,
    name: &'static str,
    guard: Guard,
}

const AES256GCM: Construction = Construction {
    aead: ffi::AES256GCM,
    name: "AES256GCM",
    guard: Guard::Aes,
};

const CHACHA20POLY1305: Construction = Construction {
    aead: ffi::CHACHA20POLY1305,
    name: "CHACHA20POLY1305",
    guard: Guard::Plain,
};

const CHACHA20POLY1305_IETF: Construction = Construction {
    aead: ffi::CHACHA20POLY1305_IETF,
    name: "CHACHA20POLY1305_IETF",
    guard: Guard::Ietf,
};

const XCHACHA20POLY1305_IETF: Construction = Construction {
    aead: ffi::XCHACHA20POLY1305_IETF,
    name: "XCHACHA20POLY1305_IETF",
    guard: Guard::XChaCha,
};

/// The four arguments every `_encrypt` and `_decrypt` takes.
type Four = (
    Raw<PhpString>,
    Raw<PhpString>,
    Raw<PhpString>,
    Raw<PhpString>,
);

/// `ssss`, then the nonce and the key held to the construction's
/// lengths: arguments 3 and 4.
fn held(
    req: &mut Request,
    of: &Construction,
    args: Four,
) -> Result<(PhpString, PhpString, PhpString, PhpString), Thrown> {
    let (text, ad, nonce, key) = args;
    let held = parsed(req, 4, 4, |req| {
        Ok((
            text.hold(req)?,
            ad.hold(req)?,
            nonce.hold(req)?,
            key.hold(req)?,
        ))
    })?;
    if held.2.len() != of.aead.npub {
        let tail = format!(
            "must be SODIUM_CRYPTO_AEAD_{}_NPUBBYTES bytes long",
            of.name
        );
        return Err(bad_arg(req, 3, &tail));
    }
    if held.3.len() != of.aead.key {
        let tail = format!("must be SODIUM_CRYPTO_AEAD_{}_KEYBYTES bytes long", of.name);
        return Err(bad_arg(req, 4, &tail));
    }
    Ok(held)
}

/// `PHP_FUNCTION(sodium_crypto_aead_*_encrypt)`.
fn encrypt(req: &mut Request, of: &Construction, args: Four) -> Result<Owner<PhpString>, Thrown> {
    let (msg, ad, npub, key) = held(req, of, args)?;
    let abytes = of.aead.abytes;
    if ZSTR_MAX_LEN - msg.len() <= abytes {
        return Err(fail(req, "arithmetic overflow"));
    }
    let single_key = match of.guard {
        Guard::Aes => Some(AES_MAX - abytes as u64),
        Guard::Ietf => Some(IETF_MAX),
        Guard::Plain | Guard::XChaCha => None,
    };
    if single_key.is_some_and(|max| msg.len() as u64 > max) {
        return Err(fail(req, "message too long for a single key"));
    }
    let mut ciphertext = vec![0u8; msg.len() + abytes];
    let sealed = of.aead.seal(
        &mut ciphertext,
        msg.as_bytes(),
        ad.as_bytes(),
        npub.as_bytes(),
        key.as_bytes(),
    );
    let Ok(len) = sealed else {
        return Err(internal(req));
    };
    match usize::try_from(len) {
        Ok(len) if len > 0 && len < ZSTR_MAX_LEN && len <= ciphertext.len() => {
            made(req, &ciphertext[..len])
        }
        _ => Err(fail(req, "arithmetic overflow")),
    }
}

/// `PHP_FUNCTION(sodium_crypto_aead_*_decrypt)`: `false` for a
/// ciphertext too short to hold the tag or one that does not verify.
fn decrypt(
    req: &mut Request,
    of: &Construction,
    args: Four,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let (ciphertext, ad, npub, key) = held(req, of, args)?;
    let len = ciphertext.len();
    let abytes = of.aead.abytes;
    let body = len.saturating_sub(abytes) as u64;
    match of.guard {
        Guard::Aes => {
            if len < abytes {
                return Ok(OrFalse::False);
            }
            if body > AES_MAX {
                return Err(bad_arg(req, 1, "is too long"));
            }
            if len >= ZSTR_MAX_LEN {
                return Err(fail(req, "arithmetic overflow"));
            }
        }
        Guard::Plain => {
            if len < abytes {
                return Ok(OrFalse::False);
            }
            if len >= ZSTR_MAX_LEN {
                return Err(fail(req, "arithmetic overflow"));
            }
        }
        Guard::Ietf => {
            if len >= ZSTR_MAX_LEN {
                return Err(fail(req, "arithmetic overflow"));
            }
            if len < abytes {
                return Ok(OrFalse::False);
            }
            if body > IETF_MAX {
                return Err(fail(req, "message too long for a single key"));
            }
        }
        Guard::XChaCha => {
            if len < abytes {
                return Ok(OrFalse::False);
            }
            if len - abytes >= ZSTR_MAX_LEN || body > IETF_MAX {
                return Err(fail(req, "arithmetic overflow"));
            }
        }
    }
    let mut msg = vec![0u8; len];
    let opened = of.aead.open(
        &mut msg,
        ciphertext.as_bytes(),
        ad.as_bytes(),
        npub.as_bytes(),
        key.as_bytes(),
    );
    let Ok(real) = opened else {
        return Ok(OrFalse::False);
    };
    match usize::try_from(real) {
        Ok(real) if real < ZSTR_MAX_LEN && real <= len => {
            let made = made(req, &msg[..real]);
            msg.fill(0);
            Ok(OrFalse::Value(made?))
        }
        _ => Err(fail(req, "arithmetic overflow")),
    }
}

/// `sodium_crypto_aead_aes256gcm_is_available(): bool`: whether the CPU
/// has the AES instructions libsodium's AES-256-GCM needs.
///
/// ```php
/// var_dump(sodium_crypto_aead_aes256gcm_is_available());
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_aead_aes256gcm_is_available() -> bool {
    ffi::aes256gcm_is_available()
}

/// `sodium_crypto_aead_aes256gcm_decrypt(string $ciphertext, string
/// $additional_data, string $nonce, string $key): string|false`.
///
/// ```php
/// $k = str_repeat("\x01", 32); $n = str_repeat("\x02", 12);
/// $c = sodium_crypto_aead_aes256gcm_encrypt('msg', 'ad', $n, $k);
/// var_dump(sodium_crypto_aead_aes256gcm_decrypt($c, 'ad', $n, $k), sodium_crypto_aead_aes256gcm_decrypt($c, 'xx', $n, $k), sodium_crypto_aead_aes256gcm_decrypt('short', 'ad', $n, $k));
/// try { sodium_crypto_aead_aes256gcm_decrypt($c, 'ad', 'n', $k); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "msg"
/// bool(false)
/// bool(false)
/// sodium_crypto_aead_aes256gcm_decrypt(): Argument #3 ($nonce) must be SODIUM_CRYPTO_AEAD_AES256GCM_NPUBBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_aead_aes256gcm_decrypt(
    req: &mut Request,
    ciphertext: Raw<PhpString>,
    additional_data: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    decrypt(req, &AES256GCM, (ciphertext, additional_data, nonce, key))
}

/// `sodium_crypto_aead_aes256gcm_encrypt(string $message, string
/// $additional_data, string $nonce, string $key): string`.
///
/// ```php
/// $k = str_repeat("\x01", 32); $n = str_repeat("\x02", 12);
/// var_dump(bin2hex(sodium_crypto_aead_aes256gcm_encrypt('msg', 'ad', $n, $k)));
/// try { sodium_crypto_aead_aes256gcm_encrypt('m', '', $n, 'k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(38) "6aa5ae338303d353abddbc8f3722a9feb00361"
/// sodium_crypto_aead_aes256gcm_encrypt(): Argument #4 ($key) must be SODIUM_CRYPTO_AEAD_AES256GCM_KEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_aead_aes256gcm_encrypt(
    req: &mut Request,
    #[php(sensitive)] message: Raw<PhpString>,
    additional_data: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    encrypt(req, &AES256GCM, (message, additional_data, nonce, key))
}

/// `sodium_crypto_aead_aes256gcm_keygen(): string`: 32 random bytes.
///
/// ```php
/// var_dump(strlen(sodium_crypto_aead_aes256gcm_keygen()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_aead_aes256gcm_keygen(
    req: &mut Request,
) -> Result<Owner<PhpString>, Thrown> {
    random_key(req, ffi::AES256GCM.key)
}

/// `sodium_crypto_aead_chacha20poly1305_decrypt(string $ciphertext,
/// string $additional_data, string $nonce, string $key): string|false`.
///
/// ```php
/// $k = str_repeat("\x01", 32); $n = str_repeat("\x02", 8);
/// $c = sodium_crypto_aead_chacha20poly1305_encrypt('msg', 'ad', $n, $k);
/// var_dump(sodium_crypto_aead_chacha20poly1305_decrypt($c, 'ad', $n, $k), sodium_crypto_aead_chacha20poly1305_decrypt($c, 'ad', $n, str_repeat("\x03", 32)));
/// ```
/// ```expect
/// string(3) "msg"
/// bool(false)
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_aead_chacha20poly1305_decrypt(
    req: &mut Request,
    ciphertext: Raw<PhpString>,
    additional_data: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    decrypt(
        req,
        &CHACHA20POLY1305,
        (ciphertext, additional_data, nonce, key),
    )
}

/// `sodium_crypto_aead_chacha20poly1305_encrypt(string $message, string
/// $additional_data, string $nonce, string $key): string`.
///
/// ```php
/// $k = str_repeat("\x01", 32); $n = str_repeat("\x02", 8);
/// var_dump(bin2hex(sodium_crypto_aead_chacha20poly1305_encrypt('msg', 'ad', $n, $k)));
/// try { sodium_crypto_aead_chacha20poly1305_encrypt('m', '', 'n', $k); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(38) "cd1d59861e9649b1ac0bf43ea7fed7d51d5f74"
/// sodium_crypto_aead_chacha20poly1305_encrypt(): Argument #3 ($nonce) must be SODIUM_CRYPTO_AEAD_CHACHA20POLY1305_NPUBBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_aead_chacha20poly1305_encrypt(
    req: &mut Request,
    #[php(sensitive)] message: Raw<PhpString>,
    additional_data: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    encrypt(
        req,
        &CHACHA20POLY1305,
        (message, additional_data, nonce, key),
    )
}

/// `sodium_crypto_aead_chacha20poly1305_keygen(): string`: 32 random
/// bytes.
///
/// ```php
/// var_dump(strlen(sodium_crypto_aead_chacha20poly1305_keygen()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_aead_chacha20poly1305_keygen(
    req: &mut Request,
) -> Result<Owner<PhpString>, Thrown> {
    random_key(req, ffi::CHACHA20POLY1305.key)
}

/// `sodium_crypto_aead_chacha20poly1305_ietf_decrypt(string $ciphertext,
/// string $additional_data, string $nonce, string $key): string|false`.
///
/// ```php
/// $k = str_repeat("\x01", 32); $n = str_repeat("\x02", 12);
/// $c = sodium_crypto_aead_chacha20poly1305_ietf_encrypt('msg', 'ad', $n, $k);
/// var_dump(sodium_crypto_aead_chacha20poly1305_ietf_decrypt($c, 'ad', $n, $k), sodium_crypto_aead_chacha20poly1305_ietf_decrypt('x', 'ad', $n, $k));
/// ```
/// ```expect
/// string(3) "msg"
/// bool(false)
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_aead_chacha20poly1305_ietf_decrypt(
    req: &mut Request,
    ciphertext: Raw<PhpString>,
    additional_data: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    decrypt(
        req,
        &CHACHA20POLY1305_IETF,
        (ciphertext, additional_data, nonce, key),
    )
}

/// `sodium_crypto_aead_chacha20poly1305_ietf_encrypt(string $message,
/// string $additional_data, string $nonce, string $key): string`.
///
/// ```php
/// $k = str_repeat("\x01", 32); $n = str_repeat("\x02", 12);
/// var_dump(bin2hex(sodium_crypto_aead_chacha20poly1305_ietf_encrypt('msg', 'ad', $n, $k)));
/// try { sodium_crypto_aead_chacha20poly1305_ietf_encrypt('m', '', $n, 'k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(38) "8b6f95948b0c7a9e3b59ba75d9a90f16b46221"
/// sodium_crypto_aead_chacha20poly1305_ietf_encrypt(): Argument #4 ($key) must be SODIUM_CRYPTO_AEAD_CHACHA20POLY1305_IETF_KEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_aead_chacha20poly1305_ietf_encrypt(
    req: &mut Request,
    #[php(sensitive)] message: Raw<PhpString>,
    additional_data: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    encrypt(
        req,
        &CHACHA20POLY1305_IETF,
        (message, additional_data, nonce, key),
    )
}

/// `sodium_crypto_aead_chacha20poly1305_ietf_keygen(): string`: 32
/// random bytes.
///
/// ```php
/// var_dump(strlen(sodium_crypto_aead_chacha20poly1305_ietf_keygen()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_aead_chacha20poly1305_ietf_keygen(
    req: &mut Request,
) -> Result<Owner<PhpString>, Thrown> {
    random_key(req, ffi::CHACHA20POLY1305_IETF.key)
}

/// `sodium_crypto_aead_xchacha20poly1305_ietf_decrypt(string
/// $ciphertext, string $additional_data, string $nonce, string $key):
/// string|false`.
///
/// ```php
/// $k = str_repeat("\x01", 32); $n = str_repeat("\x02", 24);
/// $c = sodium_crypto_aead_xchacha20poly1305_ietf_encrypt('msg', 'ad', $n, $k);
/// var_dump(sodium_crypto_aead_xchacha20poly1305_ietf_decrypt($c, 'ad', $n, $k), sodium_crypto_aead_xchacha20poly1305_ietf_decrypt($c . 'x', 'ad', $n, $k));
/// ```
/// ```expect
/// string(3) "msg"
/// bool(false)
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_aead_xchacha20poly1305_ietf_decrypt(
    req: &mut Request,
    ciphertext: Raw<PhpString>,
    additional_data: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    decrypt(
        req,
        &XCHACHA20POLY1305_IETF,
        (ciphertext, additional_data, nonce, key),
    )
}

/// `sodium_crypto_aead_xchacha20poly1305_ietf_keygen(): string`: 32
/// random bytes.
///
/// ```php
/// var_dump(strlen(sodium_crypto_aead_xchacha20poly1305_ietf_keygen()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_aead_xchacha20poly1305_ietf_keygen(
    req: &mut Request,
) -> Result<Owner<PhpString>, Thrown> {
    random_key(req, ffi::XCHACHA20POLY1305_IETF.key)
}

/// `sodium_crypto_aead_xchacha20poly1305_ietf_encrypt(string $message,
/// string $additional_data, string $nonce, string $key): string`.
///
/// ```php
/// $k = str_repeat("\x01", 32); $n = str_repeat("\x02", 24);
/// var_dump(bin2hex(sodium_crypto_aead_xchacha20poly1305_ietf_encrypt('msg', 'ad', $n, $k)));
/// try { sodium_crypto_aead_xchacha20poly1305_ietf_encrypt('m', '', 'n', $k); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(38) "a1155153b3e46e8106fede319b0b98278489db"
/// sodium_crypto_aead_xchacha20poly1305_ietf_encrypt(): Argument #3 ($nonce) must be SODIUM_CRYPTO_AEAD_XCHACHA20POLY1305_IETF_NPUBBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_aead_xchacha20poly1305_ietf_encrypt(
    req: &mut Request,
    #[php(sensitive)] message: Raw<PhpString>,
    additional_data: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    encrypt(
        req,
        &XCHACHA20POLY1305_IETF,
        (message, additional_data, nonce, key),
    )
}
