//! The key-pair families of `libsodium.c`: `crypto_box` and its sealed
//! form, `crypto_sign`, `crypto_kx`, `crypto_secretbox` and the scalar
//! multiplications. A key pair is one string, the secret key first and
//! the public key after it, as php lays it.

use php_ext::abi::{OrFalse, Raw};
use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use crate::ffi;
use crate::{ZSTR_MAX_LEN, bad_arg, fail, internal, made, parsed, random_key};

/// `crypto_box_SECRETKEYBYTES` and `_PUBLICKEYBYTES`, as the scalar
/// multiplication's and `crypto_kx`'s.
const BOX_KEY: usize = 32;
/// `crypto_box_NONCEBYTES`.
const BOX_NONCE: usize = 24;
/// `crypto_box_MACBYTES`.
const BOX_MAC: usize = 16;
/// `crypto_box_SEALBYTES`.
const BOX_SEAL: usize = 48;
/// `crypto_sign_SECRETKEYBYTES`.
const SIGN_SECRET: usize = 64;
/// `crypto_sign_PUBLICKEYBYTES`.
const SIGN_PUBLIC: usize = 32;
/// `crypto_sign_BYTES`.
const SIGN_BYTES: usize = 64;
/// `crypto_sign_SEEDBYTES`, `crypto_box_SEEDBYTES`, `crypto_kx_SEEDBYTES`.
const SEED: usize = 32;
/// `crypto_secretbox_KEYBYTES`.
const SECRETBOX_KEY: usize = 32;

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

/// `sss`, three string arguments.
fn three(
    req: &mut Request,
    a: Raw<PhpString>,
    b: Raw<PhpString>,
    c: Raw<PhpString>,
) -> Result<(PhpString, PhpString, PhpString), Thrown> {
    parsed(req, 3, 3, |req| {
        Ok((a.hold(req)?, b.hold(req)?, c.hold(req)?))
    })
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

/// A string of `len` bytes `write` fills, `internal error` where it
/// fails, the scratch zeroed after.
fn written(
    req: &mut Request,
    len: usize,
    write: impl FnOnce(&mut [u8]) -> Result<(), ffi::Failed>,
) -> Result<Owner<PhpString>, Thrown> {
    let mut out = vec![0u8; len];
    if write(&mut out).is_err() {
        out.fill(0);
        return Err(internal(req));
    }
    let made = made(req, &out);
    out.fill(0);
    made
}

/// `sodium_crypto_box(string $message, string $nonce, string $key_pair):
/// string`.
///
/// ```php
/// $kp = sodium_crypto_box_seed_keypair(str_repeat("\x01", 32)); $n = str_repeat("\x02", 24);
/// var_dump(bin2hex(sodium_crypto_box('msg', $n, $kp)));
/// try { sodium_crypto_box('msg', 'n', $kp); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_box('msg', $n, 'kp'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(38) "7cdfb26202752951cb492cd58f5acd2db66fdc"
/// sodium_crypto_box(): Argument #2 ($nonce) must be SODIUM_CRYPTO_BOX_NONCEBYTES bytes long
/// sodium_crypto_box(): Argument #3 ($key_pair) must be SODIUM_CRYPTO_BOX_KEYPAIRBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_box(
    req: &mut Request,
    #[php(sensitive)] message: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key_pair: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let (msg, nonce, whole) = three(req, message, nonce, key_pair)?;
    sized(req, nonce, BOX_NONCE, 2, "SODIUM_CRYPTO_BOX_NONCEBYTES")?;
    sized(req, whole, 2 * BOX_KEY, 3, "SODIUM_CRYPTO_BOX_KEYPAIRBYTES")?;
    if ZSTR_MAX_LEN - msg.len() <= BOX_MAC {
        return Err(fail(req, "arithmetic overflow"));
    }
    written(req, msg.len() + BOX_MAC, |c| {
        ffi::box_easy(c, msg.as_bytes(), nonce.as_bytes(), whole.as_bytes())
    })
}

/// `sodium_crypto_box_keypair(): string`: a fresh secret key and its
/// public key, 64 bytes.
///
/// ```php
/// $kp = sodium_crypto_box_keypair();
/// var_dump(strlen($kp), sodium_crypto_box_publickey($kp) === sodium_crypto_box_publickey_from_secretkey(sodium_crypto_box_secretkey($kp)));
/// ```
/// ```expect
/// int(64)
/// bool(true)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_box_keypair(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    written(req, 2 * BOX_KEY, ffi::box_keypair)
}

/// `sodium_crypto_box_seed_keypair(string $seed): string`.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_box_seed_keypair(str_repeat("\x01", 32))));
/// try { sodium_crypto_box_seed_keypair('s'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(128) "5ce86efb75fa4e2c410f46e16de9f6acae1a1703528651b69bc176c088bef3ee1b1b58dd50ea14b60da17b790cd02754d970c9bab864ebb3c0f3016fe51d3f57"
/// sodium_crypto_box_seed_keypair(): Argument #1 ($seed) must be SODIUM_CRYPTO_BOX_SEEDBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_box_seed_keypair(
    req: &mut Request,
    #[php(sensitive)] seed: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let seed = one(req, seed)?;
    sized(req, seed, SEED, 1, "SODIUM_CRYPTO_BOX_SEEDBYTES")?;
    written(req, 2 * BOX_KEY, |kp| {
        ffi::box_seed_keypair(kp, seed.as_bytes())
    })
}

/// The two halves of a key pair laid in one string, the secret key
/// first: `*_keypair_from_secretkey_and_publickey`.
fn joined(
    req: &mut Request,
    secret_key: Raw<PhpString>,
    public_key: Raw<PhpString>,
    lens: (usize, usize),
    names: (&str, &str),
) -> Result<Owner<PhpString>, Thrown> {
    let (sk, pk) = two(req, secret_key, public_key)?;
    sized(req, sk, lens.0, 1, names.0)?;
    sized(req, pk, lens.1, 2, names.1)?;
    let mut whole = Vec::with_capacity(lens.0 + lens.1);
    whole.extend_from_slice(sk.as_bytes());
    whole.extend_from_slice(pk.as_bytes());
    let made = made(req, &whole);
    whole.fill(0);
    made
}

/// `sodium_crypto_box_keypair_from_secretkey_and_publickey(string
/// $secret_key, string $public_key): string`.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_box_keypair_from_secretkey_and_publickey(str_repeat('a', 32), str_repeat('b', 32))));
/// try { sodium_crypto_box_keypair_from_secretkey_and_publickey('a', str_repeat('b', 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_box_keypair_from_secretkey_and_publickey(str_repeat('a', 32), 'b'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(128) "61616161616161616161616161616161616161616161616161616161616161616262626262626262626262626262626262626262626262626262626262626262"
/// sodium_crypto_box_keypair_from_secretkey_and_publickey(): Argument #1 ($secret_key) must be SODIUM_CRYPTO_BOX_SECRETKEYBYTES bytes long
/// sodium_crypto_box_keypair_from_secretkey_and_publickey(): Argument #2 ($public_key) must be SODIUM_CRYPTO_BOX_PUBLICKEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_box_keypair_from_secretkey_and_publickey(
    req: &mut Request,
    #[php(sensitive)] secret_key: Raw<PhpString>,
    public_key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    joined(
        req,
        secret_key,
        public_key,
        (BOX_KEY, BOX_KEY),
        (
            "SODIUM_CRYPTO_BOX_SECRETKEYBYTES",
            "SODIUM_CRYPTO_BOX_PUBLICKEYBYTES",
        ),
    )
}

/// `sodium_crypto_box_open(string $ciphertext, string $nonce, string
/// $key_pair): string|false`.
///
/// ```php
/// $kp = sodium_crypto_box_seed_keypair(str_repeat("\x01", 32)); $n = str_repeat("\x02", 24);
/// $c = sodium_crypto_box('msg', $n, $kp);
/// var_dump(sodium_crypto_box_open($c, $n, $kp), sodium_crypto_box_open('short', $n, $kp), sodium_crypto_box_open($c, str_repeat("\x03", 24), $kp));
/// ```
/// ```expect
/// string(3) "msg"
/// bool(false)
/// bool(false)
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_box_open(
    req: &mut Request,
    ciphertext: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key_pair: Raw<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let (c, nonce, whole) = three(req, ciphertext, nonce, key_pair)?;
    sized(req, nonce, BOX_NONCE, 2, "SODIUM_CRYPTO_BOX_NONCEBYTES")?;
    sized(req, whole, 2 * BOX_KEY, 3, "SODIUM_CRYPTO_BOX_KEYPAIRBYTES")?;
    let Some(len) = c.len().checked_sub(BOX_MAC) else {
        return Ok(OrFalse::False);
    };
    let mut msg = vec![0u8; len];
    if ffi::box_open(&mut msg, c.as_bytes(), nonce.as_bytes(), whole.as_bytes()).is_err() {
        return Ok(OrFalse::False);
    }
    let made = made(req, &msg);
    msg.fill(0);
    Ok(OrFalse::Value(made?))
}

/// One half of a key pair: `*_publickey` and `*_secretkey`.
fn half(
    req: &mut Request,
    key_pair: Raw<PhpString>,
    (secret, public): (usize, usize),
    want_public: bool,
    name: &str,
) -> Result<Owner<PhpString>, Thrown> {
    let whole = one(req, key_pair)?;
    sized(req, whole, secret + public, 1, name)?;
    let bytes = whole.as_bytes();
    let half = if want_public {
        &bytes[secret..]
    } else {
        &bytes[..secret]
    };
    made(req, half)
}

/// `sodium_crypto_box_publickey(string $key_pair): string`.
///
/// ```php
/// var_dump(sodium_crypto_box_publickey(str_repeat('s', 32) . str_repeat('p', 32)));
/// try { sodium_crypto_box_publickey('x'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(32) "pppppppppppppppppppppppppppppppp"
/// sodium_crypto_box_publickey(): Argument #1 ($key_pair) must be SODIUM_CRYPTO_BOX_KEYPAIRBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_box_publickey(
    req: &mut Request,
    #[php(sensitive)] key_pair: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    half(
        req,
        key_pair,
        (BOX_KEY, BOX_KEY),
        true,
        "SODIUM_CRYPTO_BOX_KEYPAIRBYTES",
    )
}

/// `sodium_crypto_box_publickey_from_secretkey(string $secret_key):
/// string`, `crypto_scalarmult_base`; `sodium_crypto_scalarmult_base` is
/// its alias.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_box_publickey_from_secretkey(str_repeat("\x01", 32))));
/// var_dump(sodium_crypto_scalarmult_base(str_repeat("\x01", 32)) === sodium_crypto_box_publickey_from_secretkey(str_repeat("\x01", 32)));
/// try { sodium_crypto_scalarmult_base('x'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "a4e09292b651c278b9772c569f5fa9bb13d906b46ab68c9df9dc2b4409f8a209"
/// bool(true)
/// sodium_crypto_scalarmult_base(): Argument #1 ($secret_key) must be SODIUM_CRYPTO_BOX_SECRETKEYBYTES bytes long
/// ```
#[php_function(arity = manual, aliases(sodium_crypto_scalarmult_base))]
pub(crate) fn sodium_crypto_box_publickey_from_secretkey(
    req: &mut Request,
    #[php(sensitive)] secret_key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let sk = one(req, secret_key)?;
    sized(req, sk, BOX_KEY, 1, "SODIUM_CRYPTO_BOX_SECRETKEYBYTES")?;
    let mut pk = [0u8; BOX_KEY];
    // php ignores `crypto_scalarmult_base`'s status here.
    ffi::scalarmult_base(&mut pk, sk.as_bytes()).ok();
    made(req, &pk)
}

/// `sodium_crypto_box_seal(string $message, string $public_key):
/// string`: anonymous, so only its length is fixed.
///
/// ```php
/// $kp = sodium_crypto_box_seed_keypair(str_repeat("\x01", 32));
/// var_dump(strlen(sodium_crypto_box_seal('msg', sodium_crypto_box_publickey($kp))));
/// try { sodium_crypto_box_seal('msg', 'pk'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(51)
/// sodium_crypto_box_seal(): Argument #2 ($public_key) must be SODIUM_CRYPTO_BOX_PUBLICKEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_box_seal(
    req: &mut Request,
    #[php(sensitive)] message: Raw<PhpString>,
    public_key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let (msg, pk) = two(req, message, public_key)?;
    sized(req, pk, BOX_KEY, 2, "SODIUM_CRYPTO_BOX_PUBLICKEYBYTES")?;
    if ZSTR_MAX_LEN - msg.len() <= BOX_SEAL {
        return Err(fail(req, "arithmetic overflow"));
    }
    written(req, msg.len() + BOX_SEAL, |c| {
        ffi::box_seal(c, msg.as_bytes(), pk.as_bytes())
    })
}

/// `sodium_crypto_box_seal_open(string $ciphertext, string $key_pair):
/// string|false`.
///
/// ```php
/// $kp = sodium_crypto_box_seed_keypair(str_repeat("\x01", 32));
/// $c = sodium_crypto_box_seal('msg', sodium_crypto_box_publickey($kp));
/// var_dump(sodium_crypto_box_seal_open($c, $kp), sodium_crypto_box_seal_open('short', $kp), sodium_crypto_box_seal_open(str_repeat('x', 60), $kp));
/// try { sodium_crypto_box_seal_open($c, 'kp'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "msg"
/// bool(false)
/// bool(false)
/// sodium_crypto_box_seal_open(): Argument #2 ($key_pair) must be SODIUM_CRYPTO_BOX_KEYPAIRBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_box_seal_open(
    req: &mut Request,
    ciphertext: Raw<PhpString>,
    #[php(sensitive)] key_pair: Raw<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let (c, whole) = two(req, ciphertext, key_pair)?;
    sized(req, whole, 2 * BOX_KEY, 2, "SODIUM_CRYPTO_BOX_KEYPAIRBYTES")?;
    let Some(len) = c.len().checked_sub(BOX_SEAL) else {
        return Ok(OrFalse::False);
    };
    let mut msg = vec![0u8; len];
    if ffi::box_seal_open(&mut msg, c.as_bytes(), whole.as_bytes()).is_err() {
        return Ok(OrFalse::False);
    }
    let made = made(req, &msg);
    msg.fill(0);
    Ok(OrFalse::Value(made?))
}

/// `sodium_crypto_box_secretkey(string $key_pair): string`.
///
/// ```php
/// var_dump(sodium_crypto_box_secretkey(str_repeat('s', 32) . str_repeat('p', 32)));
/// try { sodium_crypto_box_secretkey('x'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(32) "ssssssssssssssssssssssssssssssss"
/// sodium_crypto_box_secretkey(): Argument #1 ($key_pair) must be SODIUM_CRYPTO_BOX_KEYPAIRBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_box_secretkey(
    req: &mut Request,
    #[php(sensitive)] key_pair: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    half(
        req,
        key_pair,
        (BOX_KEY, BOX_KEY),
        false,
        "SODIUM_CRYPTO_BOX_KEYPAIRBYTES",
    )
}

/// `sodium_crypto_kx_keypair(): string`: a random secret key and its
/// public key, `randombytes_buf` then `crypto_scalarmult_base`.
///
/// ```php
/// $kp = sodium_crypto_kx_keypair();
/// var_dump(strlen($kp), sodium_crypto_kx_publickey($kp) === sodium_crypto_scalarmult_base(sodium_crypto_kx_secretkey($kp)));
/// ```
/// ```expect
/// int(64)
/// bool(true)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_kx_keypair(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    written(req, 2 * BOX_KEY, |kp| {
        let (sk, pk) = kp.split_at_mut(BOX_KEY);
        ffi::random(sk);
        ffi::scalarmult_base(pk, sk)
    })
}

/// `sodium_crypto_kx_publickey(string $key_pair): string`.
///
/// ```php
/// var_dump(sodium_crypto_kx_publickey(str_repeat('s', 32) . str_repeat('p', 32)));
/// try { sodium_crypto_kx_publickey('x'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(32) "pppppppppppppppppppppppppppppppp"
/// sodium_crypto_kx_publickey(): Argument #1 ($key_pair) must be SODIUM_CRYPTO_KX_KEYPAIRBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_kx_publickey(
    req: &mut Request,
    #[php(sensitive)] key_pair: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    half(
        req,
        key_pair,
        (BOX_KEY, BOX_KEY),
        true,
        "SODIUM_CRYPTO_KX_KEYPAIRBYTES",
    )
}

/// `sodium_crypto_kx_secretkey(string $key_pair): string`.
///
/// ```php
/// var_dump(sodium_crypto_kx_secretkey(str_repeat('s', 32) . str_repeat('p', 32)));
/// try { sodium_crypto_kx_secretkey('x'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(32) "ssssssssssssssssssssssssssssssss"
/// sodium_crypto_kx_secretkey(): Argument #1 ($key_pair) must be SODIUM_CRYPTO_KX_KEYPAIRBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_kx_secretkey(
    req: &mut Request,
    #[php(sensitive)] key_pair: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    half(
        req,
        key_pair,
        (BOX_KEY, BOX_KEY),
        false,
        "SODIUM_CRYPTO_KX_KEYPAIRBYTES",
    )
}

/// `sodium_crypto_kx_seed_keypair(string $seed): string`: the secret key
/// the `BLAKE2b` hash of the seed, the public key its scalar
/// multiplication.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_kx_seed_keypair(str_repeat("\x01", 32))));
/// try { sodium_crypto_kx_seed_keypair('s'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(128) "f40ceaf86e5776923332b8d8fd3bef849cadb19c6996bc272af1f648d9566a4cab2fca32898322c208fb2dab5048bd43c355c6430f588897cb574961cfa9806f"
/// sodium_crypto_kx_seed_keypair(): Argument #1 ($seed) must be SODIUM_CRYPTO_KX_SEEDBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_kx_seed_keypair(
    req: &mut Request,
    #[php(sensitive)] seed: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let seed = one(req, seed)?;
    sized(req, seed, SEED, 1, "SODIUM_CRYPTO_KX_SEEDBYTES")?;
    written(req, 2 * BOX_KEY, |kp| {
        let (sk, pk) = kp.split_at_mut(BOX_KEY);
        ffi::generichash(sk, seed.as_bytes(), &[])?;
        ffi::scalarmult_base(pk, sk)
    })
}

/// `sodium_crypto_kx_client_session_keys` and `_server_session_keys`:
/// `BLAKE2b`-512 over the shared point, the client's public key and the
/// server's, split in two, the client's receive key first and the
/// server's transmit key first.
fn session_keys(
    req: &mut Request,
    key_pair: Raw<PhpString>,
    other_key: Raw<PhpString>,
    server: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let (whole, other) = two(req, key_pair, other_key)?;
    sized(req, whole, 2 * BOX_KEY, 1, "SODIUM_CRYPTO_KX_KEYPAIRBYTES")?;
    sized(req, other, BOX_KEY, 2, "SODIUM_CRYPTO_KX_PUBLICKEYBYTES")?;
    let (sk, pk) = whole.as_bytes().split_at(BOX_KEY);
    let mut q = [0u8; BOX_KEY];
    if ffi::scalarmult(&mut q, sk, other.as_bytes()).is_err() {
        return Err(internal(req));
    }
    let (client_pk, server_pk) = if server {
        (other.as_bytes(), pk)
    } else {
        (pk, other.as_bytes())
    };
    let mut keys = [0u8; 2 * BOX_KEY];
    let hashed = ffi::GenericHash::init(&[], keys.len()).and_then(|mut state| {
        state.update(&q)?;
        state.update(client_pk)?;
        state.update(server_pk)?;
        state.finish(&mut keys)
    });
    q.fill(0);
    if hashed.is_err() {
        return Err(internal(req));
    }
    let (first, second) = keys.split_at(BOX_KEY);
    let (rx, tx) = if server {
        (second, first)
    } else {
        (first, second)
    };
    let mut pair = PhpArray::new(req)?;
    for half in [rx, tx] {
        let string = made(req, half)?;
        pair.append(req, Value::string(string.hand_on_unmigrated()))?;
    }
    keys.fill(0);
    Ok(pair)
}

/// `sodium_crypto_kx_client_session_keys(string $client_key_pair, string
/// $server_key): array`: the client's receive key and its transmit key.
///
/// ```php
/// $c = sodium_crypto_kx_seed_keypair(str_repeat("\x01", 32)); $s = sodium_crypto_kx_seed_keypair(str_repeat("\x02", 32));
/// $ck = sodium_crypto_kx_client_session_keys($c, sodium_crypto_kx_publickey($s));
/// var_dump(array_map('bin2hex', $ck));
/// try { sodium_crypto_kx_client_session_keys($c, 'pk'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(64) "e0f480d42cd0d8a95993dd50976fb24ef81a6788b7b5bc722fc258410ac723ed"
///   [1]=>
///   string(64) "43eb74c847209a3af513e0fc824b791865665dcc727fc332f4ddfc614e0ab79a"
/// }
/// sodium_crypto_kx_client_session_keys(): Argument #2 ($server_key) must be SODIUM_CRYPTO_KX_PUBLICKEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_kx_client_session_keys(
    req: &mut Request,
    #[php(sensitive)] client_key_pair: Raw<PhpString>,
    server_key: Raw<PhpString>,
) -> Result<Owner<PhpArray>, Thrown> {
    session_keys(req, client_key_pair, server_key, false)
}

/// `sodium_crypto_kx_server_session_keys(string $server_key_pair, string
/// $client_key): array`: the server's receive key, the client's
/// transmit key, and its transmit key, the client's receive key.
///
/// ```php
/// $c = sodium_crypto_kx_seed_keypair(str_repeat("\x01", 32)); $s = sodium_crypto_kx_seed_keypair(str_repeat("\x02", 32));
/// $ck = sodium_crypto_kx_client_session_keys($c, sodium_crypto_kx_publickey($s));
/// $sk = sodium_crypto_kx_server_session_keys($s, sodium_crypto_kx_publickey($c));
/// var_dump($ck[0] === $sk[1], $ck[1] === $sk[0]);
/// try { sodium_crypto_kx_server_session_keys('kp', 'pk'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// sodium_crypto_kx_server_session_keys(): Argument #1 ($server_key_pair) must be SODIUM_CRYPTO_KX_KEYPAIRBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_kx_server_session_keys(
    req: &mut Request,
    #[php(sensitive)] server_key_pair: Raw<PhpString>,
    client_key: Raw<PhpString>,
) -> Result<Owner<PhpArray>, Thrown> {
    session_keys(req, server_key_pair, client_key, true)
}

/// `sodium_crypto_scalarmult(string $n, string $p): string`: X25519.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_scalarmult(str_repeat("\x01", 32), sodium_crypto_scalarmult_base(str_repeat("\x02", 32)))));
/// try { sodium_crypto_scalarmult('n', str_repeat("\x01", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_scalarmult(str_repeat("\x01", 32), str_repeat("\x00", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "2ed76ab549b1e73c031eb49c9448f0798aea81b698279a0c3dc3e49fbfc4b953"
/// sodium_crypto_scalarmult(): Argument #1 ($n) must be SODIUM_CRYPTO_SCALARMULT_SCALARBYTES bytes long
/// internal error
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_scalarmult(
    req: &mut Request,
    n: Raw<PhpString>,
    p: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let (n, p) = two(req, n, p)?;
    sized(req, n, BOX_KEY, 1, "SODIUM_CRYPTO_SCALARMULT_SCALARBYTES")?;
    sized(req, p, BOX_KEY, 2, "SODIUM_CRYPTO_SCALARMULT_BYTES")?;
    written(req, BOX_KEY, |q| {
        ffi::scalarmult(q, n.as_bytes(), p.as_bytes())
    })
}

/// `sodium_crypto_scalarmult_ristretto255(string $n, string $p): string`.
///
/// ```php
/// $p = sodium_crypto_scalarmult_ristretto255_base(str_repeat("\x02", 32));
/// var_dump(bin2hex(sodium_crypto_scalarmult_ristretto255(str_repeat("\x01", 32), $p)));
/// try { sodium_crypto_scalarmult_ristretto255(str_repeat("\x00", 32), $p); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_scalarmult_ristretto255(str_repeat("\x01", 32), 'p'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "7080fd2643a8391157cbbb8445a8de84d15796a3c8fda632d73ce08133dc5933"
/// Result is identity element
/// sodium_crypto_scalarmult_ristretto255(): Argument #2 ($p) must be SODIUM_CRYPTO_SCALARMULT_RISTRETTO255_BYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_scalarmult_ristretto255(
    req: &mut Request,
    n: Raw<PhpString>,
    p: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let (n, p) = two(req, n, p)?;
    sized(
        req,
        n,
        BOX_KEY,
        1,
        "SODIUM_CRYPTO_SCALARMULT_RISTRETTO255_SCALARBYTES",
    )?;
    sized(
        req,
        p,
        BOX_KEY,
        2,
        "SODIUM_CRYPTO_SCALARMULT_RISTRETTO255_BYTES",
    )?;
    let mut q = [0u8; BOX_KEY];
    if ffi::scalarmult_ristretto255(&mut q, n.as_bytes(), p.as_bytes()).is_err() {
        return Err(fail(req, "Result is identity element"));
    }
    made(req, &q)
}

/// `sodium_crypto_scalarmult_ristretto255_base(string $n): string`.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_scalarmult_ristretto255_base(str_repeat("\x01", 32))));
/// try { sodium_crypto_scalarmult_ristretto255_base(str_repeat("\x00", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "3e440469a098036d89ffb2d77a4542928f2f74c2b5769da7480736ace829dc10"
/// sodium_crypto_scalarmult_ristretto255_base(): Argument #1 ($n) must not be zero
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_scalarmult_ristretto255_base(
    req: &mut Request,
    n: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let n = one(req, n)?;
    sized(
        req,
        n,
        BOX_KEY,
        1,
        "SODIUM_CRYPTO_SCALARMULT_RISTRETTO255_SCALARBYTES",
    )?;
    let mut q = [0u8; BOX_KEY];
    if ffi::scalarmult_ristretto255_base(&mut q, n.as_bytes()).is_err() {
        return Err(bad_arg(req, 1, "must not be zero"));
    }
    made(req, &q)
}

/// `sodium_crypto_secretbox(string $message, string $nonce, string
/// $key): string`.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_secretbox('msg', str_repeat("\x02", 24), str_repeat("\x01", 32))));
/// try { sodium_crypto_secretbox('msg', 'n', str_repeat("\x01", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_secretbox('msg', str_repeat("\x02", 24), 'k'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(38) "737ee3293c076a9bde9aae976f1023399016b9"
/// sodium_crypto_secretbox(): Argument #2 ($nonce) must be SODIUM_CRYPTO_SECRETBOX_NONCEBYTES bytes long
/// sodium_crypto_secretbox(): Argument #3 ($key) must be SODIUM_CRYPTO_SECRETBOX_KEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_secretbox(
    req: &mut Request,
    #[php(sensitive)] message: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let (msg, nonce, key) = three(req, message, nonce, key)?;
    sized(
        req,
        nonce,
        BOX_NONCE,
        2,
        "SODIUM_CRYPTO_SECRETBOX_NONCEBYTES",
    )?;
    sized(
        req,
        key,
        SECRETBOX_KEY,
        3,
        "SODIUM_CRYPTO_SECRETBOX_KEYBYTES",
    )?;
    if ZSTR_MAX_LEN - msg.len() <= BOX_MAC {
        return Err(fail(req, "arithmetic overflow"));
    }
    written(req, msg.len() + BOX_MAC, |c| {
        ffi::secretbox(c, msg.as_bytes(), nonce.as_bytes(), key.as_bytes())
    })
}

/// `sodium_crypto_secretbox_keygen(): string`: 32 random bytes.
///
/// ```php
/// var_dump(strlen(sodium_crypto_secretbox_keygen()));
/// ```
/// ```expect
/// int(32)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_secretbox_keygen(
    req: &mut Request,
) -> Result<Owner<PhpString>, Thrown> {
    random_key(req, SECRETBOX_KEY)
}

/// `sodium_crypto_secretbox_open(string $ciphertext, string $nonce,
/// string $key): string|false`.
///
/// ```php
/// $n = str_repeat("\x02", 24); $k = str_repeat("\x01", 32);
/// $c = sodium_crypto_secretbox('msg', $n, $k);
/// var_dump(sodium_crypto_secretbox_open($c, $n, $k), sodium_crypto_secretbox_open('x', $n, $k), sodium_crypto_secretbox_open($c, $n, str_repeat("\x03", 32)));
/// ```
/// ```expect
/// string(3) "msg"
/// bool(false)
/// bool(false)
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_secretbox_open(
    req: &mut Request,
    ciphertext: Raw<PhpString>,
    nonce: Raw<PhpString>,
    #[php(sensitive)] key: Raw<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let (c, nonce, key) = three(req, ciphertext, nonce, key)?;
    sized(
        req,
        nonce,
        BOX_NONCE,
        2,
        "SODIUM_CRYPTO_SECRETBOX_NONCEBYTES",
    )?;
    sized(
        req,
        key,
        SECRETBOX_KEY,
        3,
        "SODIUM_CRYPTO_SECRETBOX_KEYBYTES",
    )?;
    let Some(len) = c.len().checked_sub(BOX_MAC) else {
        return Ok(OrFalse::False);
    };
    let mut msg = vec![0u8; len];
    if ffi::secretbox_open(&mut msg, c.as_bytes(), nonce.as_bytes(), key.as_bytes()).is_err() {
        return Ok(OrFalse::False);
    }
    let made = made(req, &msg);
    msg.fill(0);
    Ok(OrFalse::Value(made?))
}

/// `sodium_crypto_sign(string $message, string $secret_key): string`: the
/// signature, then the message.
///
/// ```php
/// $kp = sodium_crypto_sign_seed_keypair(str_repeat("\x01", 32));
/// var_dump(bin2hex(sodium_crypto_sign('msg', sodium_crypto_sign_secretkey($kp))));
/// try { sodium_crypto_sign('msg', 'sk'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(134) "1f7994b02892d629ed0bf1215d0700c11d58b4f116ad5c9ef0927605cd53b1f740984ed7f09239c3d81ca674f8c62aa4629408c6ac51f21f62d234973da17f026d7367"
/// sodium_crypto_sign(): Argument #2 ($secret_key) must be SODIUM_CRYPTO_SIGN_SECRETKEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_sign(
    req: &mut Request,
    message: Raw<PhpString>,
    #[php(sensitive)] secret_key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let (msg, sk) = two(req, message, secret_key)?;
    sized(req, sk, SIGN_SECRET, 2, "SODIUM_CRYPTO_SIGN_SECRETKEYBYTES")?;
    if ZSTR_MAX_LEN - msg.len() <= SIGN_BYTES {
        return Err(fail(req, "arithmetic overflow"));
    }
    let mut signed = vec![0u8; msg.len() + SIGN_BYTES];
    let Ok(len) = ffi::sign(&mut signed, msg.as_bytes(), sk.as_bytes()) else {
        return Err(internal(req));
    };
    if len >= ZSTR_MAX_LEN {
        return Err(fail(req, "arithmetic overflow"));
    }
    made(req, &signed[..len])
}

/// `sodium_crypto_sign_detached(string $message, string $secret_key):
/// string`.
///
/// ```php
/// $kp = sodium_crypto_sign_seed_keypair(str_repeat("\x01", 32));
/// var_dump(bin2hex(sodium_crypto_sign_detached('msg', sodium_crypto_sign_secretkey($kp))));
/// try { sodium_crypto_sign_detached('msg', 'sk'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(128) "1f7994b02892d629ed0bf1215d0700c11d58b4f116ad5c9ef0927605cd53b1f740984ed7f09239c3d81ca674f8c62aa4629408c6ac51f21f62d234973da17f02"
/// sodium_crypto_sign_detached(): Argument #2 ($secret_key) must be SODIUM_CRYPTO_SIGN_SECRETKEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_sign_detached(
    req: &mut Request,
    message: Raw<PhpString>,
    #[php(sensitive)] secret_key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let (msg, sk) = two(req, message, secret_key)?;
    sized(req, sk, SIGN_SECRET, 2, "SODIUM_CRYPTO_SIGN_SECRETKEYBYTES")?;
    let mut signature = [0u8; SIGN_BYTES];
    let Ok(len) = ffi::sign_detached(&mut signature, msg.as_bytes(), sk.as_bytes()) else {
        return Err(fail(req, "signature creation failed"));
    };
    match usize::try_from(len) {
        Ok(len) if len > 0 && len <= SIGN_BYTES => made(req, &signature[..len]),
        _ => Err(fail(req, "signature has a bogus size")),
    }
}

/// `sodium_crypto_sign_ed25519_pk_to_curve25519(string $public_key):
/// string`.
///
/// ```php
/// $kp = sodium_crypto_sign_seed_keypair(str_repeat("\x01", 32));
/// var_dump(bin2hex(sodium_crypto_sign_ed25519_pk_to_curve25519(sodium_crypto_sign_publickey($kp))));
/// try { sodium_crypto_sign_ed25519_pk_to_curve25519(str_repeat("\x00", 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_sign_ed25519_pk_to_curve25519('pk'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "1b1b58dd50ea14b60da17b790cd02754d970c9bab864ebb3c0f3016fe51d3f57"
/// conversion failed
/// sodium_crypto_sign_ed25519_pk_to_curve25519(): Argument #1 ($public_key) must be SODIUM_CRYPTO_SIGN_PUBLICKEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_sign_ed25519_pk_to_curve25519(
    req: &mut Request,
    public_key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let pk = one(req, public_key)?;
    sized(req, pk, SIGN_PUBLIC, 1, "SODIUM_CRYPTO_SIGN_PUBLICKEYBYTES")?;
    let mut out = [0u8; BOX_KEY];
    if ffi::sign_pk_to_curve25519(&mut out, pk.as_bytes()).is_err() {
        return Err(fail(req, "conversion failed"));
    }
    made(req, &out)
}

/// `sodium_crypto_sign_ed25519_sk_to_curve25519(string $secret_key):
/// string`.
///
/// ```php
/// $kp = sodium_crypto_sign_seed_keypair(str_repeat("\x01", 32));
/// var_dump(bin2hex(sodium_crypto_sign_ed25519_sk_to_curve25519(sodium_crypto_sign_secretkey($kp))));
/// try { sodium_crypto_sign_ed25519_sk_to_curve25519('sk'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "58e86efb75fa4e2c410f46e16de9f6acae1a1703528651b69bc176c088bef36e"
/// sodium_crypto_sign_ed25519_sk_to_curve25519(): Argument #1 ($secret_key) must be SODIUM_CRYPTO_SIGN_SECRETKEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_sign_ed25519_sk_to_curve25519(
    req: &mut Request,
    #[php(sensitive)] secret_key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let sk = one(req, secret_key)?;
    sized(req, sk, SIGN_SECRET, 1, "SODIUM_CRYPTO_SIGN_SECRETKEYBYTES")?;
    let mut out = [0u8; BOX_KEY];
    if ffi::sign_sk_to_curve25519(&mut out, sk.as_bytes()).is_err() {
        return Err(fail(req, "conversion failed"));
    }
    let made = made(req, &out);
    out.fill(0);
    made
}

/// `sodium_crypto_sign_keypair(): string`: a fresh Ed25519 secret key
/// and its public key, 96 bytes.
///
/// ```php
/// $kp = sodium_crypto_sign_keypair();
/// var_dump(strlen($kp), sodium_crypto_sign_publickey($kp) === sodium_crypto_sign_publickey_from_secretkey(sodium_crypto_sign_secretkey($kp)));
/// ```
/// ```expect
/// int(96)
/// bool(true)
/// ```
#[php_function]
pub(crate) fn sodium_crypto_sign_keypair(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    written(req, SIGN_SECRET + SIGN_PUBLIC, ffi::sign_keypair)
}

/// `sodium_crypto_sign_keypair_from_secretkey_and_publickey(string
/// $secret_key, string $public_key): string`.
///
/// ```php
/// var_dump(strlen(sodium_crypto_sign_keypair_from_secretkey_and_publickey(str_repeat('a', 64), str_repeat('b', 32))));
/// try { sodium_crypto_sign_keypair_from_secretkey_and_publickey(str_repeat('a', 32), str_repeat('b', 32)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_sign_keypair_from_secretkey_and_publickey(str_repeat('a', 64), 'b'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(96)
/// sodium_crypto_sign_keypair_from_secretkey_and_publickey(): Argument #1 ($secret_key) must be SODIUM_CRYPTO_SIGN_SECRETKEYBYTES bytes long
/// sodium_crypto_sign_keypair_from_secretkey_and_publickey(): Argument #2 ($public_key) must be SODIUM_CRYPTO_SIGN_PUBLICKEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_sign_keypair_from_secretkey_and_publickey(
    req: &mut Request,
    #[php(sensitive)] secret_key: Raw<PhpString>,
    public_key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    joined(
        req,
        secret_key,
        public_key,
        (SIGN_SECRET, SIGN_PUBLIC),
        (
            "SODIUM_CRYPTO_SIGN_SECRETKEYBYTES",
            "SODIUM_CRYPTO_SIGN_PUBLICKEYBYTES",
        ),
    )
}

/// `sodium_crypto_sign_open(string $signed_message, string $public_key):
/// string|false`.
///
/// ```php
/// $kp = sodium_crypto_sign_seed_keypair(str_repeat("\x01", 32));
/// $s = sodium_crypto_sign('msg', sodium_crypto_sign_secretkey($kp));
/// var_dump(sodium_crypto_sign_open($s, sodium_crypto_sign_publickey($kp)), sodium_crypto_sign_open('x' . $s, sodium_crypto_sign_publickey($kp)));
/// try { sodium_crypto_sign_open($s, 'pk'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "msg"
/// bool(false)
/// sodium_crypto_sign_open(): Argument #2 ($public_key) must be SODIUM_CRYPTO_SIGN_PUBLICKEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_sign_open(
    req: &mut Request,
    signed_message: Raw<PhpString>,
    public_key: Raw<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let (signed, pk) = two(req, signed_message, public_key)?;
    sized(req, pk, SIGN_PUBLIC, 2, "SODIUM_CRYPTO_SIGN_PUBLICKEYBYTES")?;
    if signed.len() >= ZSTR_MAX_LEN {
        return Err(fail(req, "arithmetic overflow"));
    }
    let mut msg = vec![0u8; signed.len()];
    let Ok(len) = ffi::sign_open(&mut msg, signed.as_bytes(), pk.as_bytes()) else {
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(made(req, &msg[..len])?))
}

/// `sodium_crypto_sign_publickey(string $key_pair): string`.
///
/// ```php
/// var_dump(sodium_crypto_sign_publickey(str_repeat('s', 64) . str_repeat('p', 32)));
/// try { sodium_crypto_sign_publickey('x'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(32) "pppppppppppppppppppppppppppppppp"
/// sodium_crypto_sign_publickey(): Argument #1 ($key_pair) must be SODIUM_CRYPTO_SIGN_KEYPAIRBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_sign_publickey(
    req: &mut Request,
    #[php(sensitive)] key_pair: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    half(
        req,
        key_pair,
        (SIGN_SECRET, SIGN_PUBLIC),
        true,
        "SODIUM_CRYPTO_SIGN_KEYPAIRBYTES",
    )
}

/// `sodium_crypto_sign_secretkey(string $key_pair): string`.
///
/// ```php
/// var_dump(strlen(sodium_crypto_sign_secretkey(str_repeat('s', 64) . str_repeat('p', 32))));
/// try { sodium_crypto_sign_secretkey('x'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(64)
/// sodium_crypto_sign_secretkey(): Argument #1 ($key_pair) must be SODIUM_CRYPTO_SIGN_KEYPAIRBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_sign_secretkey(
    req: &mut Request,
    #[php(sensitive)] key_pair: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    half(
        req,
        key_pair,
        (SIGN_SECRET, SIGN_PUBLIC),
        false,
        "SODIUM_CRYPTO_SIGN_KEYPAIRBYTES",
    )
}

/// `sodium_crypto_sign_publickey_from_secretkey(string $secret_key):
/// string`, `crypto_sign_ed25519_sk_to_pk`.
///
/// ```php
/// $kp = sodium_crypto_sign_seed_keypair(str_repeat("\x01", 32));
/// var_dump(sodium_crypto_sign_publickey_from_secretkey(sodium_crypto_sign_secretkey($kp)) === sodium_crypto_sign_publickey($kp));
/// try { sodium_crypto_sign_publickey_from_secretkey('sk'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// sodium_crypto_sign_publickey_from_secretkey(): Argument #1 ($secret_key) must be SODIUM_CRYPTO_SIGN_SECRETKEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_sign_publickey_from_secretkey(
    req: &mut Request,
    #[php(sensitive)] secret_key: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let sk = one(req, secret_key)?;
    sized(req, sk, SIGN_SECRET, 1, "SODIUM_CRYPTO_SIGN_SECRETKEYBYTES")?;
    written(req, SIGN_PUBLIC, |pk| ffi::sign_sk_to_pk(pk, sk.as_bytes()))
}

/// `sodium_crypto_sign_seed_keypair(string $seed): string`.
///
/// ```php
/// var_dump(bin2hex(sodium_crypto_sign_seed_keypair(str_repeat("\x01", 32))));
/// try { sodium_crypto_sign_seed_keypair('s'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(192) "01010101010101010101010101010101010101010101010101010101010101018a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c"
/// sodium_crypto_sign_seed_keypair(): Argument #1 ($seed) must be SODIUM_CRYPTO_SIGN_SEEDBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_sign_seed_keypair(
    req: &mut Request,
    #[php(sensitive)] seed: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let seed = one(req, seed)?;
    sized(req, seed, SEED, 1, "SODIUM_CRYPTO_SIGN_SEEDBYTES")?;
    written(req, SIGN_SECRET + SIGN_PUBLIC, |kp| {
        ffi::sign_seed_keypair(kp, seed.as_bytes())
    })
}

/// `sodium_crypto_sign_verify_detached(string $signature, string
/// $message, string $public_key): bool`.
///
/// ```php
/// $kp = sodium_crypto_sign_seed_keypair(str_repeat("\x01", 32));
/// $sig = sodium_crypto_sign_detached('msg', sodium_crypto_sign_secretkey($kp));
/// var_dump(sodium_crypto_sign_verify_detached($sig, 'msg', sodium_crypto_sign_publickey($kp)), sodium_crypto_sign_verify_detached($sig, 'msh', sodium_crypto_sign_publickey($kp)));
/// try { sodium_crypto_sign_verify_detached('sig', 'msg', sodium_crypto_sign_publickey($kp)); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_crypto_sign_verify_detached($sig, 'msg', 'pk'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// sodium_crypto_sign_verify_detached(): Argument #1 ($signature) must be SODIUM_CRYPTO_SIGN_BYTES bytes long
/// sodium_crypto_sign_verify_detached(): Argument #3 ($public_key) must be SODIUM_CRYPTO_SIGN_PUBLICKEYBYTES bytes long
/// ```
#[php_function(arity = manual)]
pub(crate) fn sodium_crypto_sign_verify_detached(
    req: &mut Request,
    signature: Raw<PhpString>,
    message: Raw<PhpString>,
    public_key: Raw<PhpString>,
) -> Result<bool, Thrown> {
    let (sig, msg, pk) = three(req, signature, message, public_key)?;
    sized(req, sig, SIGN_BYTES, 1, "SODIUM_CRYPTO_SIGN_BYTES")?;
    sized(req, pk, SIGN_PUBLIC, 3, "SODIUM_CRYPTO_SIGN_PUBLICKEYBYTES")?;
    Ok(ffi::sign_verify_detached(sig.as_bytes(), msg.as_bytes(), pk.as_bytes()).unwrap_or(false))
}
