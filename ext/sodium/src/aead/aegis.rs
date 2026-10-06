//! AEGIS-128L and AEGIS-256, which libsodium has from 1.0.19: compiled
//! only where `build.rs` finds them in the linked library, the cfg
//! `libsodium_aegis`, as `libsodium.c` and `libsodium.stub.php` hold
//! their functions and constants under `#ifdef
//! crypto_aead_aegis128l_KEYBYTES` and `crypto_aead_aegis256_KEYBYTES`.
//! A php built against 1.0.18 has none of them.

use php_ext::abi::{OrFalse, Raw};
use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString};

use super::{Construction, Guard, decrypt, encrypt};
use crate::ffi;
use crate::{internal, made};

const AEGIS128L: Construction = Construction {
    aead: ffi::AEGIS128L,
    name: "AEGIS128L",
    guard: Guard::Plain,
};

const AEGIS256: Construction = Construction {
    aead: ffi::AEGIS256,
    name: "AEGIS256",
    guard: Guard::Plain,
};

/// `sodium_crypto_aead_aegis128l_decrypt(string $ciphertext, string
/// $additional_data, string $nonce, string $key): string|false`.
///
/// ```php
/// $k = str_repeat("\x01", 16); $n = str_repeat("\x02", 16);
/// $c = sodium_crypto_aead_aegis128l_encrypt('msg', 'ad', $n, $k);
/// var_dump(sodium_crypto_aead_aegis128l_decrypt($c, 'ad', $n, $k), sodium_crypto_aead_aegis128l_decrypt($c, '', $n, $k));
/// try { sodium_crypto_aead_aegis128l_decrypt($c, 'ad', $n, 'k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "msg"
/// bool(false)
/// sodium_crypto_aead_aegis128l_decrypt(): Argument #4 ($key) must be SODIUM_CRYPTO_AEAD_AEGIS128L_KEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_aead_aegis128l_decrypt(
    req: &mut Request,
    ciphertext: Raw<PhpString>,
    additional_data: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    decrypt(req, &AEGIS128L, (ciphertext, additional_data, nonce, key))
}

/// `sodium_crypto_aead_aegis128l_encrypt(string $message, string
/// $additional_data, string $nonce, string $key): string`.
///
/// ```php
/// $k = str_repeat("\x01", 16); $n = str_repeat("\x02", 16);
/// var_dump(bin2hex(sodium_crypto_aead_aegis128l_encrypt('msg', 'ad', $n, $k)));
/// try { sodium_crypto_aead_aegis128l_encrypt('m', '', 'n', $k); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(70) "e91b3915b166bfddb6cf8274b441d1c07ff6ff6c5584f2b742e8a1e7533b8055b23d59"
/// sodium_crypto_aead_aegis128l_encrypt(): Argument #3 ($nonce) must be SODIUM_CRYPTO_AEAD_AEGIS128L_NPUBBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_aead_aegis128l_encrypt(
    req: &mut Request,
    #[php(sensitive)] message: Raw<PhpString>,
    additional_data: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    encrypt(req, &AEGIS128L, (message, additional_data, nonce, key))
}

/// `sodium_crypto_aead_aegis128l_keygen(): string`: 16 random bytes,
/// `crypto_aead_aegis128l_keygen`.
///
/// ```php
/// var_dump(strlen(sodium_crypto_aead_aegis128l_keygen()));
/// ```
/// ```expect
/// int(16)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_aead_aegis128l_keygen(
    req: &mut Request,
) -> Result<Owner<PhpString>, Thrown> {
    aegis_key(req, ffi::AEGIS128L.key)
}

/// `crypto_aead_aegis*_keygen` into a key of `len` bytes.
fn aegis_key(req: &mut Request, len: usize) -> Result<Owner<PhpString>, Thrown> {
    let mut key = vec![0u8; len];
    if ffi::aegis_keygen(&mut key).is_err() {
        return Err(internal(req));
    }
    let made = made(req, &key);
    key.fill(0);
    made
}

/// `sodium_crypto_aead_aegis256_decrypt(string $ciphertext, string
/// $additional_data, string $nonce, string $key): string|false`.
///
/// ```php
/// $k = str_repeat("\x01", 32); $n = str_repeat("\x02", 32);
/// $c = sodium_crypto_aead_aegis256_encrypt('msg', 'ad', $n, $k);
/// var_dump(sodium_crypto_aead_aegis256_decrypt($c, 'ad', $n, $k), sodium_crypto_aead_aegis256_decrypt('', 'ad', $n, $k));
/// ```
/// ```expect
/// string(3) "msg"
/// bool(false)
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_aead_aegis256_decrypt(
    req: &mut Request,
    ciphertext: Raw<PhpString>,
    additional_data: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    decrypt(req, &AEGIS256, (ciphertext, additional_data, nonce, key))
}

/// `sodium_crypto_aead_aegis256_encrypt(string $message, string
/// $additional_data, string $nonce, string $key): string`.
///
/// ```php
/// $k = str_repeat("\x01", 32); $n = str_repeat("\x02", 32);
/// var_dump(bin2hex(sodium_crypto_aead_aegis256_encrypt('msg', 'ad', $n, $k)));
/// try { sodium_crypto_aead_aegis256_encrypt('m', '', $n, 'k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(70) "2e03fc804b41134031c3ab3d6c069864eb0fab5e3110e2d1a9068436774e587a533caa"
/// sodium_crypto_aead_aegis256_encrypt(): Argument #4 ($key) must be SODIUM_CRYPTO_AEAD_AEGIS256_KEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_aead_aegis256_encrypt(
    req: &mut Request,
    #[php(sensitive)] message: Raw<PhpString>,
    additional_data: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    encrypt(req, &AEGIS256, (message, additional_data, nonce, key))
}

/// `sodium_crypto_aead_aegis256_keygen(): string`: 32 random bytes,
/// `crypto_aead_aegis256_keygen`.
///
/// ```php
/// var_dump(strlen(sodium_crypto_aead_aegis256_keygen()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_aead_aegis256_keygen(
    req: &mut Request,
) -> Result<Owner<PhpString>, Thrown> {
    aegis_key(req, ffi::AEGIS256.key)
}
