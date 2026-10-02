//! The symmetric families of `libsodium.c` that are not AEAD: the keyed
//! hashes (`crypto_auth`, `crypto_shorthash`, `crypto_generichash`), the
//! key derivation, the password hashes, the two stream ciphers, the
//! secret stream and the ristretto255 group. A `generichash` or a secret
//! stream state is a string the caller holds by reference between calls,
//! as php keeps it.

use php_ext::abi::{OrFalse, Raw, Ref, True};
use php_ext::attr::php_function;
use php_ext::raising::{Thrown, warning};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, Value};

use crate::constants::SODIUM_CRYPTO_PWHASH_ALG_DEFAULT;
use crate::ffi::{self, GenericHash, Stream};
use crate::{
    ZSTR_MAX_LEN, assign, assign_bytes, bad_arg, fail, internal, made, parsed, random_key, referent,
};

/// `crypto_auth_BYTES` and `crypto_auth_KEYBYTES`.
const AUTH: usize = 32;
/// `crypto_shorthash_KEYBYTES`.
const SHORTHASH_KEY: usize = 16;
/// `crypto_shorthash_BYTES`.
const SHORTHASH: usize = 8;
/// `crypto_generichash_BYTES_MIN`, `_MAX`.
const GENERICHASH_MIN: i64 = 16;
const GENERICHASH_MAX: i64 = 64;
/// `crypto_generichash_KEYBYTES_MIN`, `_MAX`, `crypto_generichash_KEYBYTES`.
const GENERICHASH_KEY_MIN: usize = 16;
const GENERICHASH_KEY_MAX: usize = 64;
const GENERICHASH_KEY: usize = 32;
/// `crypto_kdf_BYTES_MIN`, `_MAX`, `_CONTEXTBYTES`, `_KEYBYTES`.
const KDF_MIN: i64 = 16;
const KDF_MAX: i64 = 64;
const KDF_CONTEXT: usize = 8;
const KDF_KEY: usize = 32;
/// `crypto_pwhash_ALG_ARGON2I13`, `_ARGON2ID13`, `_SALTBYTES`.
const ARGON2I13: i64 = 1;
const ARGON2ID13: i64 = 2;
const PWHASH_SALT: usize = 16;
/// `crypto_pwhash_OPSLIMIT_MIN` and `crypto_pwhash_MEMLIMIT_MIN`.
const PWHASH_OPSLIMIT_MIN: i64 = 1;
const PWHASH_MEMLIMIT_MIN: i64 = 8192;
/// `crypto_pwhash_scryptsalsa208sha256_SALTBYTES` and the interactive
/// limits php holds a scrypt call to.
const SCRYPT_SALT: usize = 32;
const SCRYPT_OPSLIMIT_INTERACTIVE: i64 = 524_288;
const SCRYPT_MEMLIMIT_INTERACTIVE: i64 = 16_777_216;
/// `crypto_stream_KEYBYTES`, `_NONCEBYTES`, the same for `XChaCha20`.
const STREAM_KEY: usize = 32;
const STREAM_NONCE: usize = 24;
/// `crypto_secretstream_xchacha20poly1305_KEYBYTES`, `_HEADERBYTES`,
/// `_MESSAGEBYTES_MAX`.
const SECRETSTREAM_KEY: usize = 32;
const SECRETSTREAM_HEADER: usize = 24;
const SECRETSTREAM_MESSAGE_MAX: usize = 274_877_906_816;
/// `crypto_core_ristretto255_BYTES`, `_HASHBYTES`, `_SCALARBYTES`,
/// `_NONREDUCEDSCALARBYTES`.
const RISTRETTO: usize = 32;
const RISTRETTO_HASH: usize = 64;
const RISTRETTO_NONREDUCED: usize = 64;

/// A length php has held to a range inside `size_t`'s.
fn small(n: i64) -> usize {
    usize::try_from(n).unwrap_or(0)
}

/// A ristretto255 operation of two operands.
type Binary = fn(&mut [u8], &[u8], &[u8]) -> Result<(), ffi::Failed>;

/// `s`, one string argument.
fn one(req: &mut Request, a: Raw<PhpString>) -> Result<PhpString, Thrown> {
    parsed(req, 1, 1, |req| a.hold(req))
}

/// `ss`, two string arguments.
fn two(
    req: &mut Request,
    a: Raw<PhpString>,
    b: Raw<PhpString>,
) -> Result<(PhpString, PhpString), Thrown> {
    parsed(req, 2, 2, |req| Ok((a.hold(req)?, b.hold(req)?)))
}

/// `must be <constant> bytes long` for argument `position` where `bytes`
/// is not `len` long.
fn sized(
    req: &mut Request,
    bytes: PhpString,
    len: usize,
    position: u32,
    constant: &str,
) -> Result<(), Thrown> {
    if bytes.len() == len {
        return Ok(());
    }
    Err(bad_arg(
        req,
        position,
        &format!("must be {constant} bytes long"),
    ))
}

/// A string of `len` bytes `write` fills in place, the memory limit met
/// before a byte is written as `zend_string_alloc` meets it; `internal
/// error` where `write` fails.
fn filled(
    req: &mut Request,
    len: usize,
    write: impl FnOnce(&mut [u8]) -> Result<(), ffi::Failed>,
) -> Result<PhpString, Thrown> {
    let mut done = Err(ffi::Failed);
    let string = PhpString::filled(req, len, |out| done = write(out))?;
    if done.is_err() {
        req.release(Value::string(string));
        return Err(internal(req));
    }
    Ok(string)
}

/// `sodium_crypto_auth(string $message, string $key): string`:
/// HMAC-SHA-512-256.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_auth('msg', str_repeat("\x01", 32))));
/// try { sodium_crypto_auth('msg', 'k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "c6ecbd89d46f700627599de3ec1f106b0f0f5a37b20350318536ce9f0010267d"
/// sodium_crypto_auth(): Argument #2 ($key) must be SODIUM_CRYPTO_AUTH_KEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_auth(
    req: &mut Request,
    message: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    let (msg, key) = two(req, message, key)?;
    sized(req, key, AUTH, 2, "SODIUM_CRYPTO_AUTH_KEYBYTES")?;
    filled(req, AUTH, |out| {
        ffi::auth(out, msg.as_bytes(), key.as_bytes())
    })
}

/// `sodium_crypto_auth_keygen(): string`: 32 random bytes.
///
/// ```php
/// var_dump(strlen(sodium_crypto_auth_keygen()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_auth_keygen(req: &mut Request) -> Result<PhpString, Thrown> {
    random_key(req, AUTH)
}

/// `sodium_crypto_auth_verify(string $mac, string $message, string
/// $key): bool`, the key's length checked before the tag's.
///
/// ```php
/// $k = str_repeat("\x01", 32); $mac = sodium_crypto_auth('msg', $k);
/// var_dump(sodium_crypto_auth_verify($mac, 'msg', $k), sodium_crypto_auth_verify($mac, 'msh', $k));
/// try { sodium_crypto_auth_verify('mac', 'msg', 'k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_auth_verify('mac', 'msg', $k); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// sodium_crypto_auth_verify(): Argument #3 ($key) must be SODIUM_CRYPTO_AUTH_KEYBYTES bytes long
/// sodium_crypto_auth_verify(): Argument #1 ($mac) must be SODIUM_CRYPTO_AUTH_BYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_auth_verify(
    req: &mut Request,
    mac: Raw<PhpString>,
    message: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<bool, Thrown> {
    let (mac, msg, key) = parsed(req, 3, 3, |req| {
        Ok((mac.hold(req)?, message.hold(req)?, key.hold(req)?))
    })?;
    sized(req, key, AUTH, 3, "SODIUM_CRYPTO_AUTH_KEYBYTES")?;
    sized(req, mac, AUTH, 1, "SODIUM_CRYPTO_AUTH_BYTES")?;
    Ok(ffi::auth_verify(mac.as_bytes(), msg.as_bytes(), key.as_bytes()).unwrap_or(false))
}

/// A ristretto255 operation over one string of `in_len` bytes into one of
/// 32, `internal error` where libsodium refuses.
fn ristretto_unary(
    req: &mut Request,
    s: Raw<PhpString>,
    in_len: usize,
    constant: &str,
    op: fn(&mut [u8], &[u8]) -> Result<(), ffi::Failed>,
) -> Result<PhpString, Thrown> {
    let s = one(req, s)?;
    sized(req, s, in_len, 1, constant)?;
    let mut r = [0u8; RISTRETTO];
    if op(&mut r, s.as_bytes()).is_err() {
        return Err(internal(req));
    }
    made(req, &r)
}

/// A ristretto255 operation over two 32-byte strings.
fn ristretto_binary(
    req: &mut Request,
    (p, q): (Raw<PhpString>, Raw<PhpString>),
    constant: &str,
    op: Binary,
) -> Result<PhpString, Thrown> {
    let (p, q) = two(req, p, q)?;
    sized(req, p, RISTRETTO, 1, constant)?;
    sized(req, q, RISTRETTO, 2, constant)?;
    let mut r = [0u8; RISTRETTO];
    if op(&mut r, p.as_bytes(), q.as_bytes()).is_err() {
        return Err(internal(req));
    }
    made(req, &r)
}

/// `sodium_crypto_core_ristretto255_add(string $p, string $q): string`.
///
/// ```php
/// $p = sodium_crypto_scalarmult_ristretto255_base(str_repeat("\x01", 32)); $q = sodium_crypto_scalarmult_ristretto255_base(str_repeat("\x02", 32));
/// var_dump(bin2hex(sodium_crypto_core_ristretto255_add($p, $q)));
/// try { sodium_crypto_core_ristretto255_add($p, str_repeat("\xff", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_core_ristretto255_add('p', $q); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "3417ece792a4d8416b6ee445ef9420d1b6e4db370d38ab04872fcbb0fed96f6f"
/// internal error
/// sodium_crypto_core_ristretto255_add(): Argument #1 ($p) must be SODIUM_CRYPTO_CORE_RISTRETTO255_BYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_core_ristretto255_add(
    req: &mut Request,
    p: Raw<PhpString>,
    q: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    ristretto_binary(
        req,
        (p, q),
        "SODIUM_CRYPTO_CORE_RISTRETTO255_BYTES",
        ffi::ristretto255_add,
    )
}

/// `sodium_crypto_core_ristretto255_from_hash(string $s): string`.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_core_ristretto255_from_hash(str_repeat("\x01", 64))));
/// try { sodium_crypto_core_ristretto255_from_hash('s'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "bcdb3f2f6eee9dfe105a81d3243a4d76e0ffff39700418952364b8a93c9d401d"
/// sodium_crypto_core_ristretto255_from_hash(): Argument #1 ($s) must be SODIUM_CRYPTO_CORE_RISTRETTO255_HASHBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_core_ristretto255_from_hash(
    req: &mut Request,
    s: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    ristretto_unary(
        req,
        s,
        RISTRETTO_HASH,
        "SODIUM_CRYPTO_CORE_RISTRETTO255_HASHBYTES",
        ffi::ristretto255_from_hash,
    )
}

/// `sodium_crypto_core_ristretto255_is_valid_point(string $s): bool`.
///
/// ```php
/// var_dump(sodium_crypto_core_ristretto255_is_valid_point(sodium_crypto_core_ristretto255_random()), sodium_crypto_core_ristretto255_is_valid_point(str_repeat("\xff", 32)));
/// try { sodium_crypto_core_ristretto255_is_valid_point('s'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// sodium_crypto_core_ristretto255_is_valid_point(): Argument #1 ($s) must be SODIUM_CRYPTO_CORE_RISTRETTO255_BYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_core_ristretto255_is_valid_point(
    req: &mut Request,
    s: Raw<PhpString>,
) -> Result<bool, Thrown> {
    let s = one(req, s)?;
    sized(
        req,
        s,
        RISTRETTO,
        1,
        "SODIUM_CRYPTO_CORE_RISTRETTO255_BYTES",
    )?;
    Ok(ffi::ristretto255_is_valid_point(s.as_bytes()).unwrap_or(false))
}

/// `sodium_crypto_core_ristretto255_random(): string`: a random point.
///
/// ```php
/// var_dump(strlen(sodium_crypto_core_ristretto255_random()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_core_ristretto255_random(
    req: &mut Request,
) -> Result<PhpString, Thrown> {
    filled(req, RISTRETTO, |out| ffi::ristretto255_random(out, false))
}

/// `sodium_crypto_core_ristretto255_scalar_add(string $x, string $y):
/// string`.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_core_ristretto255_scalar_add(str_repeat("\x01", 32), str_repeat("\x02", 32))));
/// try { sodium_crypto_core_ristretto255_scalar_add('x', 'y'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "0303030303030303030303030303030303030303030303030303030303030303"
/// sodium_crypto_core_ristretto255_scalar_add(): Argument #1 ($x) must be SODIUM_CRYPTO_CORE_RISTRETTO255_SCALARBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_core_ristretto255_scalar_add(
    req: &mut Request,
    x: Raw<PhpString>,
    y: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    ristretto_binary(
        req,
        (x, y),
        "SODIUM_CRYPTO_CORE_RISTRETTO255_SCALARBYTES",
        ffi::ristretto255_scalar_add,
    )
}

/// `sodium_crypto_core_ristretto255_scalar_complement(string $s):
/// string`.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_core_ristretto255_scalar_complement(str_repeat("\x01", 32))));
/// try { sodium_crypto_core_ristretto255_scalar_complement('s'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "edd2f45b19621157d59bf6a1ddf8dd13fffefefefefefefefefefefefefefe0e"
/// sodium_crypto_core_ristretto255_scalar_complement(): Argument #1 ($s) must be SODIUM_CRYPTO_CORE_RISTRETTO255_SCALARBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_core_ristretto255_scalar_complement(
    req: &mut Request,
    s: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    ristretto_unary(
        req,
        s,
        RISTRETTO,
        "SODIUM_CRYPTO_CORE_RISTRETTO255_SCALARBYTES",
        ffi::ristretto255_scalar_complement,
    )
}

/// `sodium_crypto_core_ristretto255_scalar_invert(string $s): string`.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_core_ristretto255_scalar_invert(str_repeat("\x01", 32))));
/// try { sodium_crypto_core_ristretto255_scalar_invert(str_repeat("\x00", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "442a1d24f485545ed8390cea70cafdce5a7856fac6515af9fb97b28c0c966704"
/// internal error
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_core_ristretto255_scalar_invert(
    req: &mut Request,
    s: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    ristretto_unary(
        req,
        s,
        RISTRETTO,
        "SODIUM_CRYPTO_CORE_RISTRETTO255_SCALARBYTES",
        ffi::ristretto255_scalar_invert,
    )
}

/// `sodium_crypto_core_ristretto255_scalar_mul(string $x, string $y):
/// string`.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_core_ristretto255_scalar_mul(str_repeat("\x01", 32), str_repeat("\x02", 32))));
/// try { sodium_crypto_core_ristretto255_scalar_mul(str_repeat("\x01", 32), 'y'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "7d808bf1fafea25f3ee660ef3c1793985190ba1413f9b714edf967ce6b8bdd06"
/// sodium_crypto_core_ristretto255_scalar_mul(): Argument #2 ($y) must be SODIUM_CRYPTO_CORE_RISTRETTO255_SCALARBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_core_ristretto255_scalar_mul(
    req: &mut Request,
    x: Raw<PhpString>,
    y: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    ristretto_binary(
        req,
        (x, y),
        "SODIUM_CRYPTO_CORE_RISTRETTO255_SCALARBYTES",
        ffi::ristretto255_scalar_mul,
    )
}

/// `sodium_crypto_core_ristretto255_scalar_negate(string $s): string`.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_core_ristretto255_scalar_negate(str_repeat("\x01", 32))));
/// try { sodium_crypto_core_ristretto255_scalar_negate('s'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "ecd2f45b19621157d59bf6a1ddf8dd13fffefefefefefefefefefefefefefe0e"
/// sodium_crypto_core_ristretto255_scalar_negate(): Argument #1 ($s) must be SODIUM_CRYPTO_CORE_RISTRETTO255_SCALARBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_core_ristretto255_scalar_negate(
    req: &mut Request,
    s: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    ristretto_unary(
        req,
        s,
        RISTRETTO,
        "SODIUM_CRYPTO_CORE_RISTRETTO255_SCALARBYTES",
        ffi::ristretto255_scalar_negate,
    )
}

/// `sodium_crypto_core_ristretto255_scalar_random(): string`.
///
/// ```php
/// var_dump(strlen(sodium_crypto_core_ristretto255_scalar_random()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_core_ristretto255_scalar_random(
    req: &mut Request,
) -> Result<PhpString, Thrown> {
    filled(req, RISTRETTO, |out| ffi::ristretto255_random(out, true))
}

/// `sodium_crypto_core_ristretto255_scalar_reduce(string $s): string`:
/// 64 bytes reduced modulo the group's order.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_core_ristretto255_scalar_reduce(str_repeat("\xff", 64))));
/// try { sodium_crypto_core_ristretto255_scalar_reduce(str_repeat("\xff", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "000f9c44e31106a447938568a71b0ed065bef517d273ecce3d9a307c1b419903"
/// sodium_crypto_core_ristretto255_scalar_reduce(): Argument #1 ($s) must be SODIUM_CRYPTO_CORE_RISTRETTO255_NONREDUCEDSCALARBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_core_ristretto255_scalar_reduce(
    req: &mut Request,
    s: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    ristretto_unary(
        req,
        s,
        RISTRETTO_NONREDUCED,
        "SODIUM_CRYPTO_CORE_RISTRETTO255_NONREDUCEDSCALARBYTES",
        ffi::ristretto255_scalar_reduce,
    )
}

/// `sodium_crypto_core_ristretto255_scalar_sub(string $x, string $y):
/// string`.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_core_ristretto255_scalar_sub(str_repeat("\x01", 32), str_repeat("\x02", 32))));
/// try { sodium_crypto_core_ristretto255_scalar_sub('x', str_repeat("\x02", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "ecd2f45b19621157d59bf6a1ddf8dd13fffefefefefefefefefefefefefefe0e"
/// sodium_crypto_core_ristretto255_scalar_sub(): Argument #1 ($x) must be SODIUM_CRYPTO_CORE_RISTRETTO255_SCALARBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_core_ristretto255_scalar_sub(
    req: &mut Request,
    x: Raw<PhpString>,
    y: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    ristretto_binary(
        req,
        (x, y),
        "SODIUM_CRYPTO_CORE_RISTRETTO255_SCALARBYTES",
        ffi::ristretto255_scalar_sub,
    )
}

/// `sodium_crypto_core_ristretto255_sub(string $p, string $q): string`.
///
/// ```php
/// $p = sodium_crypto_scalarmult_ristretto255_base(str_repeat("\x01", 32)); $q = sodium_crypto_scalarmult_ristretto255_base(str_repeat("\x02", 32));
/// var_dump(sodium_crypto_core_ristretto255_add(sodium_crypto_core_ristretto255_sub($p, $q), $q) === $p);
/// try { sodium_crypto_core_ristretto255_sub($p, str_repeat("\xff", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// internal error
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_core_ristretto255_sub(
    req: &mut Request,
    p: Raw<PhpString>,
    q: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    ristretto_binary(
        req,
        (p, q),
        "SODIUM_CRYPTO_CORE_RISTRETTO255_BYTES",
        ffi::ristretto255_sub,
    )
}

/// The digest length and the key `crypto_generichash` and its `_init`
/// take, held to php's bounds: the length first, then the key's.
fn generichash_bounds(req: &mut Request, length: i64, key: &[u8]) -> Result<usize, Thrown> {
    if !(GENERICHASH_MIN..=GENERICHASH_MAX).contains(&length) {
        return Err(fail(req, "unsupported output length"));
    }
    if !key.is_empty() && !(GENERICHASH_KEY_MIN..=GENERICHASH_KEY_MAX).contains(&key.len()) {
        return Err(fail(req, "unsupported key length"));
    }
    Ok(small(length))
}

/// `sodium_crypto_generichash(string $message, string $key = "", int
/// $length = SODIUM_CRYPTO_GENERICHASH_BYTES): string`: `BLAKE2b`.
///
/// ```php
/// var_dump(sodium_crypto_generichash('msg') === sodium_crypto_generichash('msg', '', 32), bin2hex(sodium_crypto_generichash('msg', str_repeat("\x01", 16), 16)));
/// try { sodium_crypto_generichash('msg', '', 8); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_generichash('msg', 'k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(32) "131ae970245887f698bb58199ca71bc5"
/// unsupported output length
/// unsupported key length
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_generichash(
    req: &mut Request,
    message: Raw<PhpString>,
    #[php(sensitive, default = "")] key: Raw<PhpString>,
    #[php(default = SODIUM_CRYPTO_GENERICHASH_BYTES)] length: Raw<i64>,
) -> Result<PhpString, Thrown> {
    let (msg, key, length) = parsed(req, 1, 3, |req| {
        Ok((message.hold(req)?, key.hold(req)?, length.hold(req)?))
    })?;
    let len = generichash_bounds(req, length, key.as_bytes())?;
    filled(req, len, |out| {
        ffi::generichash(out, msg.as_bytes(), key.as_bytes())
    })
}

/// `sodium_crypto_generichash_keygen(): string`: 32 random bytes.
///
/// ```php
/// var_dump(strlen(sodium_crypto_generichash_keygen()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_generichash_keygen(req: &mut Request) -> Result<PhpString, Thrown> {
    random_key(req, GENERICHASH_KEY)
}

/// `sodium_crypto_generichash_init(string $key = "", int $length =
/// SODIUM_CRYPTO_GENERICHASH_BYTES): string`: the state, as a string of
/// `sizeof(crypto_generichash_state)` bytes.
///
/// ```php
/// var_dump(strlen(sodium_crypto_generichash_init()), strlen(sodium_crypto_generichash_init(str_repeat("\x01", 32), 64)));
/// try { sodium_crypto_generichash_init('', 65); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(384)
/// int(384)
/// unsupported output length
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_generichash_init(
    req: &mut Request,
    #[php(sensitive, default = "")] key: Raw<PhpString>,
    #[php(default = SODIUM_CRYPTO_GENERICHASH_BYTES)] length: Raw<i64>,
) -> Result<PhpString, Thrown> {
    let (key, length) = parsed(req, 0, 2, |req| Ok((key.hold(req)?, length.hold(req)?)))?;
    let len = generichash_bounds(req, length, key.as_bytes())?;
    let Ok(state) = GenericHash::init(key.as_bytes(), len) else {
        return Err(internal(req));
    };
    made(req, state.bytes())
}

/// The state a `generichash` call holds by reference: a string, or
/// argument 1's refusal; one of the wrong length, `incorrect state
/// length`.
fn generichash_state(
    req: &mut Request,
    state: &Ref<Raw<PhpString>>,
) -> Result<GenericHash, Thrown> {
    let Some(bytes) = referent(state) else {
        return Err(bad_arg(req, 1, "must be a reference to a state"));
    };
    GenericHash::from_bytes(&bytes).map_err(|_| fail(req, "incorrect state length"))
}

/// `sodium_crypto_generichash_update(string &$state, string $message):
/// true`.
///
/// ```php
/// $s = sodium_crypto_generichash_init(); $t = $s;
/// var_dump(sodium_crypto_generichash_update($s, 'm'), $s === $t);
/// sodium_crypto_generichash_update($s, 'sg');
/// var_dump(sodium_crypto_generichash_final($s) === sodium_crypto_generichash('msg'));
/// $x = 'x'; try { sodium_crypto_generichash_update($x, 'm'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// $x = 1; try { sodium_crypto_generichash_update($x, 'm'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// incorrect state length
/// sodium_crypto_generichash_update(): Argument #1 ($state) must be a reference to a state
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_generichash_update(
    req: &mut Request,
    state: Ref<Raw<PhpString>>,
    message: Raw<PhpString>,
) -> Result<True, Thrown> {
    let msg = parsed(req, 2, 2, |req| message.hold(req))?;
    let mut hash = generichash_state(req, &state)?;
    if hash.update(msg.as_bytes()).is_err() {
        return Err(internal(req));
    }
    assign_bytes(req, &state, hash.bytes())?;
    Ok(True)
}

/// `sodium_crypto_generichash_final(string &$state, int $length =
/// SODIUM_CRYPTO_GENERICHASH_BYTES): string`: the digest, the state
/// set to `null`.
///
/// ```php
/// $s = sodium_crypto_generichash_init('', 16); sodium_crypto_generichash_update($s, 'msg');
/// var_dump(bin2hex(sodium_crypto_generichash_final($s, 16)), $s);
/// $s = sodium_crypto_generichash_init(); try { sodium_crypto_generichash_final($s, 100); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(32) "d2af2662d96c7534929984fac7854973"
/// NULL
/// unsupported output length
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_generichash_final(
    req: &mut Request,
    state: Ref<Raw<PhpString>>,
    #[php(default = SODIUM_CRYPTO_GENERICHASH_BYTES)] length: Raw<i64>,
) -> Result<PhpString, Thrown> {
    let length = parsed(req, 1, 2, |req| length.hold(req))?;
    let mut hash = generichash_state(req, &state)?;
    if !(GENERICHASH_MIN..=GENERICHASH_MAX).contains(&length) {
        return Err(fail(req, "unsupported output length"));
    }
    let mut out = vec![0u8; small(length)];
    if hash.finish(&mut out).is_err() {
        return Err(internal(req));
    }
    assign(req, &state, Value::null())?;
    made(req, &out)
}

/// `sodium_crypto_kdf_derive_from_key(int $subkey_length, int $subkey_id, string $context, string $key): string`.
///
/// ```php
/// $k = str_repeat("\x01", 32);
/// var_dump(bin2hex(sodium_crypto_kdf_derive_from_key(16, 1, 'context!', $k)));
/// foreach ([[8, 1, 'context!', $k], [65, 1, 'context!', $k], [16, -1, 'context!', $k], [16, 1, 'ctx', $k], [16, 1, 'context!', 'k']] as $a) {
///     try { sodium_crypto_kdf_derive_from_key(...$a); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// string(32) "dde6ecde30ac1cc7333391c7b4f1188e"
/// sodium_crypto_kdf_derive_from_key(): Argument #1 ($subkey_length) must be greater than or equal to SODIUM_CRYPTO_KDF_BYTES_MIN
/// sodium_crypto_kdf_derive_from_key(): Argument #1 ($subkey_length) must be less than or equal to SODIUM_CRYPTO_KDF_BYTES_MAX
/// sodium_crypto_kdf_derive_from_key(): Argument #2 ($subkey_id) must be greater than or equal to 0
/// sodium_crypto_kdf_derive_from_key(): Argument #3 ($context) must be SODIUM_CRYPTO_KDF_CONTEXTBYTES bytes long
/// sodium_crypto_kdf_derive_from_key(): Argument #4 ($key) must be SODIUM_CRYPTO_KDF_KEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_kdf_derive_from_key(
    req: &mut Request,
    subkey_length: Raw<i64>,
    subkey_id: Raw<i64>,
    context: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    let (len, id, ctx, key) = parsed(req, 4, 4, |req| {
        Ok((
            subkey_length.hold(req)?,
            subkey_id.hold(req)?,
            context.hold(req)?,
            key.hold(req)?,
        ))
    })?;
    if len < KDF_MIN {
        return Err(bad_arg(
            req,
            1,
            "must be greater than or equal to SODIUM_CRYPTO_KDF_BYTES_MIN",
        ));
    }
    if len > KDF_MAX {
        return Err(bad_arg(
            req,
            1,
            "must be less than or equal to SODIUM_CRYPTO_KDF_BYTES_MAX",
        ));
    }
    let Ok(id) = u64::try_from(id) else {
        return Err(bad_arg(req, 2, "must be greater than or equal to 0"));
    };
    sized(req, ctx, KDF_CONTEXT, 3, "SODIUM_CRYPTO_KDF_CONTEXTBYTES")?;
    sized(req, key, KDF_KEY, 4, "SODIUM_CRYPTO_KDF_KEYBYTES")?;
    filled(req, small(len), |out| {
        ffi::kdf_derive(out, id, ctx.as_bytes(), key.as_bytes())
    })
}

/// `sodium_crypto_kdf_keygen(): string`: 32 random bytes.
///
/// ```php
/// var_dump(strlen(sodium_crypto_kdf_keygen()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_kdf_keygen(req: &mut Request) -> Result<PhpString, Thrown> {
    random_key(req, KDF_KEY)
}

/// The `memlimit` php hands libsodium, `size_t`, once it has held it to
/// greater than zero.
fn mem_limit(limit: i64) -> usize {
    usize::try_from(limit).unwrap_or(0)
}

/// `opslimit`, `unsigned long long`, once php has held it to greater
/// than zero.
fn ops_limit(limit: i64) -> u64 {
    u64::try_from(limit).unwrap_or(0)
}

/// `sodium_crypto_pwhash(int $length, string $password, string $salt,
/// int $opslimit, int $memlimit, int $algo =
/// SODIUM_CRYPTO_PWHASH_ALG_DEFAULT): string`: Argon2i or Argon2id.
///
/// ```php
/// $salt = str_repeat("\x01", SODIUM_CRYPTO_PWHASH_SALTBYTES);
/// var_dump(bin2hex(sodium_crypto_pwhash(16, 'password', $salt, 1, 8192)), bin2hex(sodium_crypto_pwhash(16, 'password', $salt, 3, 8192, SODIUM_CRYPTO_PWHASH_ALG_ARGON2I13)));
/// var_dump(strlen(@sodium_crypto_pwhash(16, '', $salt, 1, 8192)));
/// foreach ([[0, 'p', $salt, 1, 8192], [16, 'p', $salt, 0, 8192], [16, 'p', $salt, 1, 0], [16, 'p', $salt, 1, 8192, 7], [16, 'p', 's', 1, 8192], [16, 'p', $salt, 1, 100]] as $a) {
///     try { sodium_crypto_pwhash(...$a); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// string(32) "3094541c78d88f924b98999048b22002"
/// string(32) "7f763c1ef578db1296be1d7501853863"
/// int(16)
/// sodium_crypto_pwhash(): Argument #1 ($length) must be greater than 0
/// sodium_crypto_pwhash(): Argument #4 ($opslimit) must be greater than 0
/// sodium_crypto_pwhash(): Argument #5 ($memlimit) must be greater than 0
/// unsupported password hashing algorithm
/// sodium_crypto_pwhash(): Argument #3 ($salt) must be SODIUM_CRYPTO_PWHASH_SALTBYTES bytes long
/// sodium_crypto_pwhash(): Argument #5 ($memlimit) must be greater than or equal to 8192
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_pwhash(
    req: &mut Request,
    length: Raw<i64>,
    #[php(sensitive)] password: Raw<PhpString>,
    salt: Raw<PhpString>,
    opslimit: Raw<i64>,
    memlimit: Raw<i64>,
    #[php(default = SODIUM_CRYPTO_PWHASH_ALG_DEFAULT)] algo: Raw<i64>,
) -> Result<PhpString, Thrown> {
    let (len, passwd, salt, ops, mem, alg) = parsed(req, 5, 6, |req| {
        Ok((
            length.hold(req)?,
            password.hold(req)?,
            salt.hold(req)?,
            opslimit.hold(req)?,
            memlimit.hold(req)?,
            algo.hold(req)?,
        ))
    })?;
    if len <= 0 {
        return Err(bad_arg(req, 1, "must be greater than 0"));
    }
    if len >= 0xffff_ffff {
        return Err(bad_arg(req, 1, "is too large"));
    }
    if passwd.len() >= 0xffff_ffff {
        return Err(bad_arg(req, 2, "is too long"));
    }
    if ops <= 0 {
        return Err(bad_arg(req, 4, "must be greater than 0"));
    }
    if mem <= 0 {
        return Err(bad_arg(req, 5, "must be greater than 0"));
    }
    if alg != ARGON2I13 && alg != ARGON2ID13 && alg != SODIUM_CRYPTO_PWHASH_ALG_DEFAULT {
        return Err(fail(req, "unsupported password hashing algorithm"));
    }
    if passwd.is_empty() {
        warning(req, "empty password");
    }
    sized(req, salt, PWHASH_SALT, 3, "SODIUM_CRYPTO_PWHASH_SALTBYTES")?;
    if ops < PWHASH_OPSLIMIT_MIN {
        let tail = format!("must be greater than or equal to {PWHASH_OPSLIMIT_MIN}");
        return Err(bad_arg(req, 4, &tail));
    }
    if mem < PWHASH_MEMLIMIT_MIN {
        let tail = format!("must be greater than or equal to {PWHASH_MEMLIMIT_MIN}");
        return Err(bad_arg(req, 5, &tail));
    }
    let alg = std::ffi::c_int::try_from(alg).unwrap_or(0);
    filled(req, small(len), |out| {
        ffi::pwhash(
            out,
            passwd.as_bytes(),
            salt.as_bytes(),
            ops_limit(ops),
            mem_limit(mem),
            alg,
        )
    })
}

/// `sodium_crypto_pwhash_str(string $password, int $opslimit, int
/// $memlimit): string`: an Argon2id hash in its encoded form.
///
/// ```php
/// $h = sodium_crypto_pwhash_str('password', 1, 8192);
/// var_dump(substr($h, 0, 27), sodium_crypto_pwhash_str_verify($h, 'password'));
/// foreach ([['p', 0, 8192], ['p', 1, 0], ['p', 1, 100]] as $a) {
///     try { sodium_crypto_pwhash_str(...$a); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// string(27) "$argon2id$v=19$m=8,t=1,p=1$"
/// bool(true)
/// sodium_crypto_pwhash_str(): Argument #2 ($opslimit) must be greater than 0
/// sodium_crypto_pwhash_str(): Argument #3 ($memlimit) must be greater than 0
/// sodium_crypto_pwhash_str(): Argument #3 ($memlimit) must be greater than or equal to 8192
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_pwhash_str(
    req: &mut Request,
    #[php(sensitive)] password: Raw<PhpString>,
    opslimit: Raw<i64>,
    memlimit: Raw<i64>,
) -> Result<PhpString, Thrown> {
    let (passwd, ops, mem) = parsed(req, 3, 3, |req| {
        Ok((
            password.hold(req)?,
            opslimit.hold(req)?,
            memlimit.hold(req)?,
        ))
    })?;
    if ops <= 0 {
        return Err(bad_arg(req, 2, "must be greater than 0"));
    }
    if mem <= 0 {
        return Err(bad_arg(req, 3, "must be greater than 0"));
    }
    if passwd.len() >= 0xffff_ffff {
        return Err(bad_arg(req, 1, "is too long"));
    }
    if passwd.is_empty() {
        warning(req, "empty password");
    }
    if ops < PWHASH_OPSLIMIT_MIN {
        let tail = format!("must be greater than or equal to {PWHASH_OPSLIMIT_MIN}");
        return Err(bad_arg(req, 2, &tail));
    }
    if mem < PWHASH_MEMLIMIT_MIN {
        let tail = format!("must be greater than or equal to {PWHASH_MEMLIMIT_MIN}");
        return Err(bad_arg(req, 3, &tail));
    }
    let mut out = [0u8; ffi::PWHASH_STRBYTES];
    if ffi::pwhash_str(&mut out, passwd.as_bytes(), ops_limit(ops), mem_limit(mem)).is_err() {
        return Err(internal(req));
    }
    // `strlen` over the buffer whose last byte php sets to NUL.
    let end = out[..ffi::PWHASH_STRBYTES - 1]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(ffi::PWHASH_STRBYTES - 1);
    made(req, &out[..end])
}

/// `sodium_crypto_pwhash_str_verify(string $hash, string $password):
/// bool`.
///
/// ```php
/// $h = sodium_crypto_pwhash_str('password', 1, 8192);
/// var_dump(sodium_crypto_pwhash_str_verify($h, 'password'), sodium_crypto_pwhash_str_verify($h, 'passworD'), sodium_crypto_pwhash_str_verify('junk', 'password'));
/// var_dump(@sodium_crypto_pwhash_str_verify($h, ''));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_pwhash_str_verify(
    req: &mut Request,
    hash: Raw<PhpString>,
    #[php(sensitive)] password: Raw<PhpString>,
) -> Result<bool, Thrown> {
    let (hash, passwd) = two(req, hash, password)?;
    if passwd.len() >= 0xffff_ffff {
        return Err(bad_arg(req, 2, "is too long"));
    }
    if passwd.is_empty() {
        warning(req, "empty password");
    }
    Ok(ffi::pwhash_str_verify(hash.as_bytes(), passwd.as_bytes()))
}

/// `sodium_crypto_pwhash_str_needs_rehash(string $password, int
/// $opslimit, int $memlimit): bool`: whether the hash, the first
/// argument whatever its name, was made under other limits.
///
/// ```php
/// $h = sodium_crypto_pwhash_str('password', 1, 8192);
/// var_dump(sodium_crypto_pwhash_str_needs_rehash($h, 1, 8192), sodium_crypto_pwhash_str_needs_rehash($h, 2, 8192), sodium_crypto_pwhash_str_needs_rehash('junk', 1, 8192));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_pwhash_str_needs_rehash(
    req: &mut Request,
    password: Raw<PhpString>,
    opslimit: Raw<i64>,
    memlimit: Raw<i64>,
) -> Result<bool, Thrown> {
    let (hash, ops, mem) = parsed(req, 3, 3, |req| {
        Ok((
            password.hold(req)?,
            opslimit.hold(req)?,
            memlimit.hold(req)?,
        ))
    })?;
    // php hands the `zend_long`s to libsodium's unsigned types as C
    // converts them.
    let mem = usize::try_from(mem.cast_unsigned()).unwrap_or(usize::MAX);
    Ok(ffi::pwhash_str_needs_rehash(
        hash.as_bytes(),
        ops.cast_unsigned(),
        mem,
    ))
}

/// `sodium_crypto_pwhash_scryptsalsa208sha256(int $length, string
/// $password, string $salt, int $opslimit, int $memlimit): string`.
///
/// ```php
/// $salt = str_repeat("\x01", SODIUM_CRYPTO_PWHASH_SCRYPTSALSA208SHA256_SALTBYTES);
/// var_dump(bin2hex(sodium_crypto_pwhash_scryptsalsa208sha256(16, 'password', $salt, 524288, 16777216)));
/// foreach ([[0, 'p', $salt, 524288, 16777216], [16, 'p', $salt, 0, 16777216], [16, 'p', $salt, 524288, 0], [16, 'p', 's', 524288, 16777216], [16, 'p', $salt, 1, 16777216], [16, 'p', $salt, 524288, 1]] as $a) {
///     try { sodium_crypto_pwhash_scryptsalsa208sha256(...$a); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// string(32) "54829a32a400618c079489f9128293a8"
/// sodium_crypto_pwhash_scryptsalsa208sha256(): Argument #1 ($length) must be greater than 0
/// sodium_crypto_pwhash_scryptsalsa208sha256(): Argument #4 ($opslimit) must be greater than 0
/// sodium_crypto_pwhash_scryptsalsa208sha256(): Argument #5 ($memlimit) must be greater than 0
/// sodium_crypto_pwhash_scryptsalsa208sha256(): Argument #3 ($salt) must be SODIUM_CRYPTO_PWHASH_SCRYPTSALSA208SHA256_SALTBYTES bytes long
/// sodium_crypto_pwhash_scryptsalsa208sha256(): Argument #4 ($opslimit) must be greater than or equal to 524288
/// sodium_crypto_pwhash_scryptsalsa208sha256(): Argument #5 ($memlimit) must be greater than or equal to 16777216
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_pwhash_scryptsalsa208sha256(
    req: &mut Request,
    length: Raw<i64>,
    #[php(sensitive)] password: Raw<PhpString>,
    salt: Raw<PhpString>,
    opslimit: Raw<i64>,
    memlimit: Raw<i64>,
) -> Result<PhpString, Thrown> {
    let (len, passwd, salt, ops, mem) = parsed(req, 5, 5, |req| {
        Ok((
            length.hold(req)?,
            password.hold(req)?,
            salt.hold(req)?,
            opslimit.hold(req)?,
            memlimit.hold(req)?,
        ))
    })?;
    let too_long = usize::try_from(len).is_ok_and(|len| len >= ZSTR_MAX_LEN);
    if len <= 0 || too_long || len > 0x1f_ffff_ffe0 {
        return Err(bad_arg(req, 1, "must be greater than 0"));
    }
    if ops <= 0 {
        return Err(bad_arg(req, 4, "must be greater than 0"));
    }
    if mem <= 0 {
        return Err(bad_arg(req, 5, "must be greater than 0"));
    }
    if passwd.is_empty() {
        warning(req, "empty password");
    }
    sized(
        req,
        salt,
        SCRYPT_SALT,
        3,
        "SODIUM_CRYPTO_PWHASH_SCRYPTSALSA208SHA256_SALTBYTES",
    )?;
    if ops < SCRYPT_OPSLIMIT_INTERACTIVE {
        let tail = format!("must be greater than or equal to {SCRYPT_OPSLIMIT_INTERACTIVE}");
        return Err(bad_arg(req, 4, &tail));
    }
    if mem < SCRYPT_MEMLIMIT_INTERACTIVE {
        let tail = format!("must be greater than or equal to {SCRYPT_MEMLIMIT_INTERACTIVE}");
        return Err(bad_arg(req, 5, &tail));
    }
    filled(req, small(len), |out| {
        ffi::pwhash_scrypt(
            out,
            passwd.as_bytes(),
            salt.as_bytes(),
            ops_limit(ops),
            mem_limit(mem),
        )
    })
}

/// `sodium_crypto_pwhash_scryptsalsa208sha256_str(string $password, int
/// $opslimit, int $memlimit): string`: the 101 bytes of the encoded
/// hash.
///
/// ```php
/// $h = sodium_crypto_pwhash_scryptsalsa208sha256_str('password', 524288, 16777216);
/// var_dump(strlen($h), substr($h, 0, 3), sodium_crypto_pwhash_scryptsalsa208sha256_str_verify($h, 'password'));
/// foreach ([['p', 0, 16777216], ['p', 524288, 0], ['p', 1, 16777216], ['p', 524288, 1]] as $a) {
///     try { sodium_crypto_pwhash_scryptsalsa208sha256_str(...$a); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// int(101)
/// string(3) "$7$"
/// bool(true)
/// sodium_crypto_pwhash_scryptsalsa208sha256_str(): Argument #2 ($opslimit) must be greater than 0
/// sodium_crypto_pwhash_scryptsalsa208sha256_str(): Argument #3 ($memlimit) must be greater than 0
/// sodium_crypto_pwhash_scryptsalsa208sha256_str(): Argument #2 ($opslimit) must be greater than or equal to 524288
/// sodium_crypto_pwhash_scryptsalsa208sha256_str(): Argument #3 ($memlimit) must be greater than or equal to 16777216
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_pwhash_scryptsalsa208sha256_str(
    req: &mut Request,
    #[php(sensitive)] password: Raw<PhpString>,
    opslimit: Raw<i64>,
    memlimit: Raw<i64>,
) -> Result<PhpString, Thrown> {
    let (passwd, ops, mem) = parsed(req, 3, 3, |req| {
        Ok((
            password.hold(req)?,
            opslimit.hold(req)?,
            memlimit.hold(req)?,
        ))
    })?;
    if ops <= 0 {
        return Err(bad_arg(req, 2, "must be greater than 0"));
    }
    if mem <= 0 {
        return Err(bad_arg(req, 3, "must be greater than 0"));
    }
    if passwd.is_empty() {
        warning(req, "empty password");
    }
    if ops < SCRYPT_OPSLIMIT_INTERACTIVE {
        let tail = format!("must be greater than or equal to {SCRYPT_OPSLIMIT_INTERACTIVE}");
        return Err(bad_arg(req, 2, &tail));
    }
    if mem < SCRYPT_MEMLIMIT_INTERACTIVE {
        let tail = format!("must be greater than or equal to {SCRYPT_MEMLIMIT_INTERACTIVE}");
        return Err(bad_arg(req, 3, &tail));
    }
    let mut out = [0u8; ffi::SCRYPT_STRBYTES];
    if ffi::pwhash_scrypt_str(&mut out, passwd.as_bytes(), ops_limit(ops), mem_limit(mem)).is_err()
    {
        return Err(internal(req));
    }
    made(req, &out[..ffi::SCRYPT_STRBYTES - 1])
}

/// `sodium_crypto_pwhash_scryptsalsa208sha256_str_verify(string $hash,
/// string $password): bool`: a hash not 101 bytes long is a warning and
/// `false`.
///
/// ```php
/// $h = sodium_crypto_pwhash_scryptsalsa208sha256_str('password', 524288, 16777216);
/// var_dump(sodium_crypto_pwhash_scryptsalsa208sha256_str_verify($h, 'passworD'));
/// set_error_handler(function ($no, $msg) { echo $msg, "\n"; return true; });
/// var_dump(sodium_crypto_pwhash_scryptsalsa208sha256_str_verify('short', 'password'), sodium_crypto_pwhash_scryptsalsa208sha256_str_verify($h, ''));
/// ```
/// ```expect
/// bool(false)
/// wrong size for the hashed password
/// empty password
/// bool(false)
/// bool(false)
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_pwhash_scryptsalsa208sha256_str_verify(
    req: &mut Request,
    hash: Raw<PhpString>,
    #[php(sensitive)] password: Raw<PhpString>,
) -> Result<bool, Thrown> {
    let (hash, passwd) = two(req, hash, password)?;
    if passwd.is_empty() {
        warning(req, "empty password");
    }
    if hash.len() != ffi::SCRYPT_STRBYTES - 1 {
        warning(req, "wrong size for the hashed password");
        return Ok(false);
    }
    Ok(ffi::pwhash_scrypt_str_verify(hash.as_bytes(), passwd.as_bytes()).unwrap_or(false))
}

/// `sodium_crypto_secretstream_xchacha20poly1305_keygen(): string`: 32
/// random bytes.
///
/// ```php
/// var_dump(strlen(sodium_crypto_secretstream_xchacha20poly1305_keygen()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_secretstream_xchacha20poly1305_keygen(
    req: &mut Request,
) -> Result<PhpString, Thrown> {
    random_key(req, SECRETSTREAM_KEY)
}

/// `sodium_crypto_secretstream_xchacha20poly1305_init_push(string $key):
/// array`: the state and the header.
///
/// ```php
/// [$state, $header] = sodium_crypto_secretstream_xchacha20poly1305_init_push(str_repeat("\x01", 32));
/// var_dump(strlen($state), strlen($header));
/// try { sodium_crypto_secretstream_xchacha20poly1305_init_push('k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(52)
/// int(24)
/// sodium_crypto_secretstream_xchacha20poly1305_init_push(): Argument #1 ($key) must be SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_KEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_secretstream_xchacha20poly1305_init_push(
    req: &mut Request,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<PhpArray, Thrown> {
    let key = one(req, key)?;
    sized(
        req,
        key,
        SECRETSTREAM_KEY,
        1,
        "SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_KEYBYTES",
    )?;
    let mut state = [0u8; ffi::SECRETSTREAM_STATE];
    let mut header = [0u8; SECRETSTREAM_HEADER];
    if ffi::secretstream_init_push(&mut state, &mut header, key.as_bytes()).is_err() {
        return Err(internal(req));
    }
    let mut pair = PhpArray::new(req)?;
    for bytes in [&state[..], &header[..]] {
        let string = made(req, bytes)?;
        pair.append(req, Value::string(string))?;
    }
    state.fill(0);
    Ok(pair)
}

/// The state a secret stream call holds by reference: a string, or
/// argument 1's refusal; its length is the caller's to check.
fn stream_state(req: &mut Request, state: &Ref<Raw<PhpString>>) -> Result<Vec<u8>, Thrown> {
    referent(state).ok_or_else(|| bad_arg(req, 1, "must be a reference to a state"))
}

/// `sodium_crypto_secretstream_xchacha20poly1305_push(string &$state,
/// string $message, string $additional_data = "", int $tag =
/// SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_TAG_MESSAGE): string`.
///
/// ```php
/// [$s, $h] = sodium_crypto_secretstream_xchacha20poly1305_init_push(str_repeat("\x01", 32)); $t = $s;
/// var_dump(strlen(sodium_crypto_secretstream_xchacha20poly1305_push($s, 'msg', 'ad', SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_TAG_FINAL)), $s === $t);
/// try { sodium_crypto_secretstream_xchacha20poly1305_push($s, 'msg', '', 256); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// $x = 'x'; try { sodium_crypto_secretstream_xchacha20poly1305_push($x, 'msg'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(20)
/// bool(false)
/// sodium_crypto_secretstream_xchacha20poly1305_push(): Argument #4 ($tag) must be in the range of 0-255
/// sodium_crypto_secretstream_xchacha20poly1305_push(): Argument #1 ($state) must have a correct length
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_secretstream_xchacha20poly1305_push(
    req: &mut Request,
    state: Ref<Raw<PhpString>>,
    #[php(sensitive)] message: Raw<PhpString>,
    #[php(default = "")] additional_data: Raw<PhpString>,
    #[php(default = SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_TAG_MESSAGE)] tag: Raw<i64>,
) -> Result<PhpString, Thrown> {
    let (msg, ad, tag) = parsed(req, 2, 4, |req| {
        Ok((
            message.hold(req)?,
            additional_data.hold(req)?,
            tag.hold(req)?,
        ))
    })?;
    let mut bytes = stream_state(req, &state)?;
    if bytes.len() != ffi::SECRETSTREAM_STATE {
        return Err(bad_arg(req, 1, "must have a correct length"));
    }
    if msg.len() > SECRETSTREAM_MESSAGE_MAX || msg.len() > ZSTR_MAX_LEN - ffi::SECRETSTREAM_ABYTES {
        return Err(bad_arg(
            req,
            2,
            "must be at most SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_MESSAGEBYTES_MAX bytes long",
        ));
    }
    let Ok(tag) = u8::try_from(tag) else {
        return Err(bad_arg(req, 4, "must be in the range of 0-255"));
    };
    let mut c = vec![0u8; msg.len() + ffi::SECRETSTREAM_ABYTES];
    let pushed = ffi::secretstream_push(&mut bytes, &mut c, msg.as_bytes(), ad.as_bytes(), tag);
    assign_bytes(req, &state, &bytes)?;
    bytes.fill(0);
    let Ok(len) = pushed else {
        return Err(internal(req));
    };
    match usize::try_from(len) {
        Ok(len) if len > 0 && len < ZSTR_MAX_LEN && len <= c.len() => made(req, &c[..len]),
        _ => Err(fail(req, "arithmetic overflow")),
    }
}

/// `sodium_crypto_secretstream_xchacha20poly1305_init_pull(string
/// $header, string $key): string`: the state.
///
/// ```php
/// var_dump(strlen(sodium_crypto_secretstream_xchacha20poly1305_init_pull(str_repeat("\x02", 24), str_repeat("\x01", 32))));
/// try { sodium_crypto_secretstream_xchacha20poly1305_init_pull('h', str_repeat("\x01", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_secretstream_xchacha20poly1305_init_pull(str_repeat("\x02", 24), 'k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(52)
/// sodium_crypto_secretstream_xchacha20poly1305_init_pull(): Argument #1 ($header) must be SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_HEADERBYTES bytes long
/// sodium_crypto_secretstream_xchacha20poly1305_init_pull(): Argument #2 ($key) must be SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_KEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_secretstream_xchacha20poly1305_init_pull(
    req: &mut Request,
    header: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    let (header, key) = two(req, header, key)?;
    sized(
        req,
        header,
        SECRETSTREAM_HEADER,
        1,
        "SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_HEADERBYTES",
    )?;
    sized(
        req,
        key,
        SECRETSTREAM_KEY,
        2,
        "SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_KEYBYTES",
    )?;
    let mut state = [0u8; ffi::SECRETSTREAM_STATE];
    if ffi::secretstream_init_pull(&mut state, header.as_bytes(), key.as_bytes()).is_err() {
        return Err(internal(req));
    }
    let made = made(req, &state);
    state.fill(0);
    made
}

/// `sodium_crypto_secretstream_xchacha20poly1305_pull(string &$state,
/// string $ciphertext, string $additional_data = ""): array|false`: the
/// message and its tag.
///
/// ```php
/// $k = str_repeat("\x01", 32);
/// [$s, $h] = sodium_crypto_secretstream_xchacha20poly1305_init_push($k);
/// $c1 = sodium_crypto_secretstream_xchacha20poly1305_push($s, 'one');
/// $c2 = sodium_crypto_secretstream_xchacha20poly1305_push($s, 'two', 'ad', SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_TAG_FINAL);
/// $p = sodium_crypto_secretstream_xchacha20poly1305_init_pull($h, $k);
/// var_dump(sodium_crypto_secretstream_xchacha20poly1305_pull($p, $c1), sodium_crypto_secretstream_xchacha20poly1305_pull($p, $c2), sodium_crypto_secretstream_xchacha20poly1305_pull($p, 'short'));
/// $x = 'x'; try { sodium_crypto_secretstream_xchacha20poly1305_pull($x, $c1); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(3) "one"
///   [1]=>
///   int(0)
/// }
/// bool(false)
/// bool(false)
/// incorrect state length
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_secretstream_xchacha20poly1305_pull(
    req: &mut Request,
    state: Ref<Raw<PhpString>>,
    ciphertext: Raw<PhpString>,
    #[php(default = "")] additional_data: Raw<PhpString>,
) -> Result<OrFalse<PhpArray>, Thrown> {
    let (c, ad) = parsed(req, 2, 3, |req| {
        Ok((ciphertext.hold(req)?, additional_data.hold(req)?))
    })?;
    let mut bytes = stream_state(req, &state)?;
    if bytes.len() != ffi::SECRETSTREAM_STATE {
        return Err(fail(req, "incorrect state length"));
    }
    let Some(len) = c.len().checked_sub(ffi::SECRETSTREAM_ABYTES) else {
        return Ok(OrFalse::False);
    };
    let mut msg = vec![0u8; len];
    let pulled = ffi::secretstream_pull(&mut bytes, &mut msg, c.as_bytes(), ad.as_bytes());
    assign_bytes(req, &state, &bytes)?;
    bytes.fill(0);
    let Ok((real, tag)) = pulled else {
        return Ok(OrFalse::False);
    };
    let Some(real) = usize::try_from(real)
        .ok()
        .filter(|&real| real < ZSTR_MAX_LEN && real <= len)
    else {
        return Err(fail(req, "arithmetic overflow"));
    };
    let mut pair = PhpArray::new(req)?;
    let string = made(req, &msg[..real])?;
    msg.fill(0);
    pair.append(req, Value::string(string))?;
    pair.append(req, Value::int(i64::from(tag)))?;
    Ok(OrFalse::Value(pair))
}

/// `sodium_crypto_secretstream_xchacha20poly1305_rekey(string &$state):
/// void`.
///
/// ```php
/// [$s, $h] = sodium_crypto_secretstream_xchacha20poly1305_init_push(str_repeat("\x01", 32)); $t = $s;
/// sodium_crypto_secretstream_xchacha20poly1305_rekey($s);
/// var_dump(strlen($s), $s === $t);
/// $x = 'x'; try { sodium_crypto_secretstream_xchacha20poly1305_rekey($x); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// $x = 1; try { sodium_crypto_secretstream_xchacha20poly1305_rekey($x); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(52)
/// bool(false)
/// incorrect state length
/// sodium_crypto_secretstream_xchacha20poly1305_rekey(): Argument #1 ($state) must be a reference to a state
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_secretstream_xchacha20poly1305_rekey(
    req: &mut Request,
    state: Ref<Raw<PhpString>>,
) -> Result<(), Thrown> {
    parsed(req, 1, 1, |_| Ok(()))?;
    let mut bytes = stream_state(req, &state)?;
    if bytes.len() != ffi::SECRETSTREAM_STATE || ffi::secretstream_rekey(&mut bytes).is_err() {
        return Err(fail(req, "incorrect state length"));
    }
    assign_bytes(req, &state, &bytes)?;
    bytes.fill(0);
    Ok(())
}

/// `sodium_crypto_shorthash(string $message, string $key): string`:
/// SipHash-2-4.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_shorthash('msg', str_repeat("\x01", 16))));
/// try { sodium_crypto_shorthash('msg', 'k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(16) "6b9a29a9d2b220f9"
/// sodium_crypto_shorthash(): Argument #2 ($key) must be SODIUM_CRYPTO_SHORTHASH_KEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_shorthash(
    req: &mut Request,
    message: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    let (msg, key) = two(req, message, key)?;
    sized(
        req,
        key,
        SHORTHASH_KEY,
        2,
        "SODIUM_CRYPTO_SHORTHASH_KEYBYTES",
    )?;
    filled(req, SHORTHASH, |out| {
        ffi::shorthash(out, msg.as_bytes(), key.as_bytes())
    })
}

/// `sodium_crypto_shorthash_keygen(): string`: 16 random bytes.
///
/// ```php
/// var_dump(strlen(sodium_crypto_shorthash_keygen()));
/// ```
/// ```expect
/// int(16)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_shorthash_keygen(req: &mut Request) -> Result<PhpString, Thrown> {
    random_key(req, SHORTHASH_KEY)
}

/// The keystream calls, `sodium_crypto_stream` and
/// `sodium_crypto_stream_xchacha20`: `lss`, the length greater than
/// zero, the nonce and the key held to the cipher's.
fn keystream(
    req: &mut Request,
    which: Stream,
    (length, nonce, key): (Raw<i64>, Raw<PhpString>, Raw<PhpString>),
) -> Result<PhpString, Thrown> {
    let (len, nonce, key) = parsed(req, 3, 3, |req| {
        Ok((length.hold(req)?, nonce.hold(req)?, key.hold(req)?))
    })?;
    let Some(len) = usize::try_from(len)
        .ok()
        .filter(|&len| len > 0 && len < ZSTR_MAX_LEN)
    else {
        return Err(bad_arg(req, 1, "must be greater than 0"));
    };
    let infix = match which {
        Stream::XSalsa20 => "",
        Stream::XChaCha20 => "_XCHACHA20",
    };
    sized(
        req,
        nonce,
        STREAM_NONCE,
        2,
        &format!("SODIUM_CRYPTO_STREAM{infix}_NONCEBYTES"),
    )?;
    sized(
        req,
        key,
        STREAM_KEY,
        3,
        &format!("SODIUM_CRYPTO_STREAM{infix}_KEYBYTES"),
    )?;
    filled(req, len, |out| {
        ffi::stream(which, out, nonce.as_bytes(), key.as_bytes())
    })
}

/// The XOR calls, `sodium_crypto_stream_xor`,
/// `sodium_crypto_stream_xchacha20_xor` and its `_ic` form.
fn xored(
    req: &mut Request,
    which: Stream,
    (msg, nonce, key): (PhpString, PhpString, PhpString),
    counter: Option<i64>,
) -> Result<PhpString, Thrown> {
    let infix = match which {
        Stream::XSalsa20 => "",
        Stream::XChaCha20 => "_XCHACHA20",
    };
    sized(
        req,
        nonce,
        STREAM_NONCE,
        2,
        &format!("SODIUM_CRYPTO_STREAM{infix}_NONCEBYTES"),
    )?;
    sized(
        req,
        key,
        STREAM_KEY,
        3,
        &format!("SODIUM_CRYPTO_STREAM{infix}_KEYBYTES"),
    )?;
    filled(req, msg.len(), |out| match counter {
        // `(uint64_t) ic`: the two's complement of a negative counter.
        Some(ic) => ffi::stream_xchacha20_xor_ic(
            out,
            msg.as_bytes(),
            nonce.as_bytes(),
            ic.cast_unsigned(),
            key.as_bytes(),
        ),
        None => ffi::stream_xor(which, out, msg.as_bytes(), nonce.as_bytes(), key.as_bytes()),
    })
}

/// `sodium_crypto_stream(int $length, string $nonce, string $key):
/// string`: the `XSalsa20` keystream.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_stream(8, str_repeat("\x02", 24), str_repeat("\x01", 32))));
/// try { sodium_crypto_stream(0, str_repeat("\x02", 24), str_repeat("\x01", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_stream(8, 'n', str_repeat("\x01", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(16) "63aaaf184d07dbb9"
/// sodium_crypto_stream(): Argument #1 ($length) must be greater than 0
/// sodium_crypto_stream(): Argument #2 ($nonce) must be SODIUM_CRYPTO_STREAM_NONCEBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_stream(
    req: &mut Request,
    length: Raw<i64>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    keystream(req, Stream::XSalsa20, (length, nonce, key))
}

/// `sodium_crypto_stream_keygen(): string`: 32 random bytes.
///
/// ```php
/// var_dump(strlen(sodium_crypto_stream_keygen()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_stream_keygen(req: &mut Request) -> Result<PhpString, Thrown> {
    random_key(req, STREAM_KEY)
}

/// `sodium_crypto_stream_xor(string $message, string $nonce, string
/// $key): string`.
///
/// ```php
/// $n = str_repeat("\x02", 24); $k = str_repeat("\x01", 32);
/// var_dump(bin2hex(sodium_crypto_stream_xor('msg', $n, $k)), sodium_crypto_stream_xor(sodium_crypto_stream_xor('msg', $n, $k), $n, $k));
/// try { sodium_crypto_stream_xor('msg', $n, 'k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(6) "0ed9c8"
/// string(3) "msg"
/// sodium_crypto_stream_xor(): Argument #3 ($key) must be SODIUM_CRYPTO_STREAM_KEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_stream_xor(
    req: &mut Request,
    #[php(sensitive)] message: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    let held = parsed(req, 3, 3, |req| {
        Ok((message.hold(req)?, nonce.hold(req)?, key.hold(req)?))
    })?;
    xored(req, Stream::XSalsa20, held, None)
}

/// `sodium_crypto_stream_xchacha20(int $length, string $nonce, string
/// $key): string`: the `XChaCha20` keystream.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_stream_xchacha20(8, str_repeat("\x02", 24), str_repeat("\x01", 32))));
/// try { sodium_crypto_stream_xchacha20(-1, str_repeat("\x02", 24), str_repeat("\x01", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_stream_xchacha20(8, str_repeat("\x02", 24), 'k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(16) "b6bb0ee1dfec94dd"
/// sodium_crypto_stream_xchacha20(): Argument #1 ($length) must be greater than 0
/// sodium_crypto_stream_xchacha20(): Argument #3 ($key) must be SODIUM_CRYPTO_STREAM_XCHACHA20_KEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_stream_xchacha20(
    req: &mut Request,
    length: Raw<i64>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    keystream(req, Stream::XChaCha20, (length, nonce, key))
}

/// `sodium_crypto_stream_xchacha20_keygen(): string`: 32 random bytes.
///
/// ```php
/// var_dump(strlen(sodium_crypto_stream_xchacha20_keygen()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_stream_xchacha20_keygen(
    req: &mut Request,
) -> Result<PhpString, Thrown> {
    random_key(req, STREAM_KEY)
}

/// `sodium_crypto_stream_xchacha20_xor(string $message, string $nonce,
/// string $key): string`.
///
/// ```php
/// $n = str_repeat("\x02", 24); $k = str_repeat("\x01", 32);
/// var_dump(bin2hex(sodium_crypto_stream_xchacha20_xor('msg', $n, $k)));
/// try { sodium_crypto_stream_xchacha20_xor('msg', 'n', $k); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(6) "dbc869"
/// sodium_crypto_stream_xchacha20_xor(): Argument #2 ($nonce) must be SODIUM_CRYPTO_STREAM_XCHACHA20_NONCEBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_stream_xchacha20_xor(
    req: &mut Request,
    #[php(sensitive)] message: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    let held = parsed(req, 3, 3, |req| {
        Ok((message.hold(req)?, nonce.hold(req)?, key.hold(req)?))
    })?;
    xored(req, Stream::XChaCha20, held, None)
}

/// `sodium_crypto_stream_xchacha20_xor_ic(string $message, string $nonce, int $counter, string $key): string`:
/// the message under the keystream from block `$counter` on.
///
/// ```php
/// $n = str_repeat("\x02", 24); $k = str_repeat("\x01", 32);
/// var_dump(sodium_crypto_stream_xchacha20_xor_ic('msg', $n, 0, $k) === sodium_crypto_stream_xchacha20_xor('msg', $n, $k), bin2hex(sodium_crypto_stream_xchacha20_xor_ic('msg', $n, 1, $k)));
/// try { sodium_crypto_stream_xchacha20_xor_ic('msg', $n, 0, 'k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(6) "a11551"
/// sodium_crypto_stream_xchacha20_xor_ic(): Argument #3 ($counter) must be SODIUM_CRYPTO_STREAM_XCHACHA20_KEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_stream_xchacha20_xor_ic(
    req: &mut Request,
    #[php(sensitive)] message: Raw<PhpString>,
    nonce: Raw<PhpString>,
    counter: Raw<i64>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<PhpString, Thrown> {
    let (msg, nonce, ic, key) = parsed(req, 4, 4, |req| {
        Ok((
            message.hold(req)?,
            nonce.hold(req)?,
            counter.hold(req)?,
            key.hold(req)?,
        ))
    })?;
    xored(req, Stream::XChaCha20, (msg, nonce, key), Some(ic))
}
