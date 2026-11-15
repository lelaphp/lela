//! The `sodium` extension: `php-src/ext/sodium` as one crate under `ext/`
//! (ADR-0004), `libsodium.c` and `sodium_pwhash.c` ported over the
//! libsodium the pinned php links, found through `pkg-config` and bound
//! in [`ffi`], never vendored (design §7.2, revision 72). This
//! file is the extension value, the constants, `SodiumException`, the
//! argument reading every native shares and the helpers over strings
//! (`sodium_add` to `sodium_base642bin`); [`aead`] is the six AEAD
//! constructions, [`keys`] the public-key boxes, signatures, key
//! exchange and scalar multiplication, and [`hash`] the hashes, the key
//! derivation, the password hashes, the stream ciphers, the secret
//! stream and ristretto255. The phpt subset is `php-src/ext/sodium/tests`,
//! through `xtask phpt --subset`.
//!
//! `sodium_pwhash.c` registers `argon2i` and `argon2id` with the password
//! API only where `php_password_algo_find("argon2i")` finds none: the
//! oracle's `ext/standard` is built with libargon2 and registers both
//! first, so its `PHP_MINIT_FUNCTION(sodium_password_hash)` takes the
//! "Nothing to do" branch and registers neither the algorithms nor the
//! `PASSWORD_ARGON2*` constants. `ext/standard` here carries both
//! algorithms the same way, so this crate takes that branch:
//! `PASSWORD_ARGON2_PROVIDER` stays `standard`, as the oracle prints.
//!
//! Every throwable `libsodium.c` raises leaves without the arguments in
//! its trace (`sodium_remove_param_values_from_backtrace`), a
//! `SodiumException` because its `create_object` strips them, one PHP
//! code makes with `new` among them ([`SodiumHandlers`]), and a
//! parameter's `TypeError` because the native strips it ([`parsed`]).
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod aead;
mod ffi;
mod hash;
mod keys;
mod throwables;

use php_ext::abi::{
    Dependency, DependencyKind, EngineState, Extension, ExtensionBuilder, ObjectHandlers, Raw, Ref,
    Refusal, Refusals,
};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::raising::{Level, Thrown, argument_error, arity, throw};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, Value, walk};

/// `ZSTR_MAX_LEN`: `SIZE_MAX` less a `zend_string`'s 24-byte header and
/// its NUL, the bound `libsodium.c`'s overflow checks are written over.
const ZSTR_MAX_LEN: usize = usize::MAX - 25;

/// `libsodium.stub.php`'s constants in its order, the three
/// `SODIUM_LIBRARY_*` the linked library's own: `sodium_version_string`,
/// `sodium_library_version_major` and `sodium_library_version_minor`.
///
/// ```php
/// var_dump(SODIUM_LIBRARY_VERSION === phpversion('sodium') ? 'php' : 'lib', SODIUM_LIBRARY_MAJOR_VERSION >= 10, SODIUM_CRYPTO_BOX_KEYPAIRBYTES, SODIUM_CRYPTO_PWHASH_STRPREFIX, SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_MESSAGEBYTES_MAX);
/// $c = get_defined_constants(true)['sodium'];
/// var_dump(count($c), array_key_first($c), array_key_last($c));
/// ```
/// ```expect
/// string(3) "lib"
/// bool(true)
/// int(64)
/// string(10) "$argon2id$"
/// int(274877906816)
/// int(102)
/// string(22) "SODIUM_LIBRARY_VERSION"
/// string(53) "SODIUM_CRYPTO_CORE_RISTRETTO255_NONREDUCEDSCALARBYTES"
/// ```
#[php_const]
pub(crate) mod constants {
    fn SODIUM_LIBRARY_VERSION() -> String {
        super::ffi::version()
    }
    fn SODIUM_LIBRARY_MAJOR_VERSION() -> i64 {
        super::ffi::version_major()
    }
    fn SODIUM_LIBRARY_MINOR_VERSION() -> i64 {
        super::ffi::version_minor()
    }
    pub(crate) const SODIUM_CRYPTO_AEAD_AES256GCM_KEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_AEAD_AES256GCM_NSECBYTES: i64 = 0;
    pub(crate) const SODIUM_CRYPTO_AEAD_AES256GCM_NPUBBYTES: i64 = 12;
    pub(crate) const SODIUM_CRYPTO_AEAD_AES256GCM_ABYTES: i64 = 16;
    #[cfg(libsodium_aegis)]
    pub(crate) const SODIUM_CRYPTO_AEAD_AEGIS128L_KEYBYTES: i64 = 16;
    #[cfg(libsodium_aegis)]
    pub(crate) const SODIUM_CRYPTO_AEAD_AEGIS128L_NSECBYTES: i64 = 0;
    #[cfg(libsodium_aegis)]
    pub(crate) const SODIUM_CRYPTO_AEAD_AEGIS128L_NPUBBYTES: i64 = 16;
    #[cfg(libsodium_aegis)]
    pub(crate) const SODIUM_CRYPTO_AEAD_AEGIS128L_ABYTES: i64 = 32;
    #[cfg(libsodium_aegis)]
    pub(crate) const SODIUM_CRYPTO_AEAD_AEGIS256_KEYBYTES: i64 = 32;
    #[cfg(libsodium_aegis)]
    pub(crate) const SODIUM_CRYPTO_AEAD_AEGIS256_NSECBYTES: i64 = 0;
    #[cfg(libsodium_aegis)]
    pub(crate) const SODIUM_CRYPTO_AEAD_AEGIS256_NPUBBYTES: i64 = 32;
    #[cfg(libsodium_aegis)]
    pub(crate) const SODIUM_CRYPTO_AEAD_AEGIS256_ABYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_AEAD_CHACHA20POLY1305_KEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_AEAD_CHACHA20POLY1305_NSECBYTES: i64 = 0;
    pub(crate) const SODIUM_CRYPTO_AEAD_CHACHA20POLY1305_NPUBBYTES: i64 = 8;
    pub(crate) const SODIUM_CRYPTO_AEAD_CHACHA20POLY1305_ABYTES: i64 = 16;
    pub(crate) const SODIUM_CRYPTO_AEAD_CHACHA20POLY1305_IETF_KEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_AEAD_CHACHA20POLY1305_IETF_NSECBYTES: i64 = 0;
    pub(crate) const SODIUM_CRYPTO_AEAD_CHACHA20POLY1305_IETF_NPUBBYTES: i64 = 12;
    pub(crate) const SODIUM_CRYPTO_AEAD_CHACHA20POLY1305_IETF_ABYTES: i64 = 16;
    pub(crate) const SODIUM_CRYPTO_AEAD_XCHACHA20POLY1305_IETF_KEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_AEAD_XCHACHA20POLY1305_IETF_NSECBYTES: i64 = 0;
    pub(crate) const SODIUM_CRYPTO_AEAD_XCHACHA20POLY1305_IETF_NPUBBYTES: i64 = 24;
    pub(crate) const SODIUM_CRYPTO_AEAD_XCHACHA20POLY1305_IETF_ABYTES: i64 = 16;
    pub(crate) const SODIUM_CRYPTO_AUTH_BYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_AUTH_KEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_BOX_SEALBYTES: i64 = 48;
    pub(crate) const SODIUM_CRYPTO_BOX_SECRETKEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_BOX_PUBLICKEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_BOX_KEYPAIRBYTES: i64 = 64;
    pub(crate) const SODIUM_CRYPTO_BOX_MACBYTES: i64 = 16;
    pub(crate) const SODIUM_CRYPTO_BOX_NONCEBYTES: i64 = 24;
    pub(crate) const SODIUM_CRYPTO_BOX_SEEDBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_KDF_BYTES_MIN: i64 = 16;
    pub(crate) const SODIUM_CRYPTO_KDF_BYTES_MAX: i64 = 64;
    pub(crate) const SODIUM_CRYPTO_KDF_CONTEXTBYTES: i64 = 8;
    pub(crate) const SODIUM_CRYPTO_KDF_KEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_KX_SEEDBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_KX_SESSIONKEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_KX_PUBLICKEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_KX_SECRETKEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_KX_KEYPAIRBYTES: i64 = 64;
    pub(crate) const SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_ABYTES: i64 = 17;
    pub(crate) const SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_HEADERBYTES: i64 = 24;
    pub(crate) const SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_KEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_MESSAGEBYTES_MAX: i64 =
        274_877_906_816;
    pub(crate) const SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_TAG_MESSAGE: i64 = 0;
    pub(crate) const SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_TAG_PUSH: i64 = 1;
    pub(crate) const SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_TAG_REKEY: i64 = 2;
    pub(crate) const SODIUM_CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_TAG_FINAL: i64 = 3;
    pub(crate) const SODIUM_CRYPTO_GENERICHASH_BYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_GENERICHASH_BYTES_MIN: i64 = 16;
    pub(crate) const SODIUM_CRYPTO_GENERICHASH_BYTES_MAX: i64 = 64;
    pub(crate) const SODIUM_CRYPTO_GENERICHASH_KEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_GENERICHASH_KEYBYTES_MIN: i64 = 16;
    pub(crate) const SODIUM_CRYPTO_GENERICHASH_KEYBYTES_MAX: i64 = 64;
    pub(crate) const SODIUM_CRYPTO_PWHASH_ALG_ARGON2I13: i64 = 1;
    pub(crate) const SODIUM_CRYPTO_PWHASH_ALG_ARGON2ID13: i64 = 2;
    pub(crate) const SODIUM_CRYPTO_PWHASH_ALG_DEFAULT: i64 = 2;
    pub(crate) const SODIUM_CRYPTO_PWHASH_SALTBYTES: i64 = 16;
    pub(crate) const SODIUM_CRYPTO_PWHASH_STRPREFIX: &str = "$argon2id$";
    pub(crate) const SODIUM_CRYPTO_PWHASH_OPSLIMIT_INTERACTIVE: i64 = 2;
    pub(crate) const SODIUM_CRYPTO_PWHASH_MEMLIMIT_INTERACTIVE: i64 = 67_108_864;
    pub(crate) const SODIUM_CRYPTO_PWHASH_OPSLIMIT_MODERATE: i64 = 3;
    pub(crate) const SODIUM_CRYPTO_PWHASH_MEMLIMIT_MODERATE: i64 = 268_435_456;
    pub(crate) const SODIUM_CRYPTO_PWHASH_OPSLIMIT_SENSITIVE: i64 = 4;
    pub(crate) const SODIUM_CRYPTO_PWHASH_MEMLIMIT_SENSITIVE: i64 = 1_073_741_824;
    pub(crate) const SODIUM_CRYPTO_PWHASH_SCRYPTSALSA208SHA256_SALTBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_PWHASH_SCRYPTSALSA208SHA256_STRPREFIX: &str = "$7$";
    pub(crate) const SODIUM_CRYPTO_PWHASH_SCRYPTSALSA208SHA256_OPSLIMIT_INTERACTIVE: i64 = 524_288;
    pub(crate) const SODIUM_CRYPTO_PWHASH_SCRYPTSALSA208SHA256_MEMLIMIT_INTERACTIVE: i64 =
        16_777_216;
    pub(crate) const SODIUM_CRYPTO_PWHASH_SCRYPTSALSA208SHA256_OPSLIMIT_SENSITIVE: i64 = 33_554_432;
    pub(crate) const SODIUM_CRYPTO_PWHASH_SCRYPTSALSA208SHA256_MEMLIMIT_SENSITIVE: i64 =
        1_073_741_824;
    pub(crate) const SODIUM_CRYPTO_SCALARMULT_BYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_SCALARMULT_SCALARBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_SHORTHASH_BYTES: i64 = 8;
    pub(crate) const SODIUM_CRYPTO_SHORTHASH_KEYBYTES: i64 = 16;
    pub(crate) const SODIUM_CRYPTO_SECRETBOX_KEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_SECRETBOX_MACBYTES: i64 = 16;
    pub(crate) const SODIUM_CRYPTO_SECRETBOX_NONCEBYTES: i64 = 24;
    pub(crate) const SODIUM_CRYPTO_SIGN_BYTES: i64 = 64;
    pub(crate) const SODIUM_CRYPTO_SIGN_SEEDBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_SIGN_PUBLICKEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_SIGN_SECRETKEYBYTES: i64 = 64;
    pub(crate) const SODIUM_CRYPTO_SIGN_KEYPAIRBYTES: i64 = 96;
    pub(crate) const SODIUM_CRYPTO_STREAM_NONCEBYTES: i64 = 24;
    pub(crate) const SODIUM_CRYPTO_STREAM_KEYBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_STREAM_XCHACHA20_NONCEBYTES: i64 = 24;
    pub(crate) const SODIUM_CRYPTO_STREAM_XCHACHA20_KEYBYTES: i64 = 32;
    pub(crate) const SODIUM_BASE64_VARIANT_ORIGINAL: i64 = 1;
    pub(crate) const SODIUM_BASE64_VARIANT_ORIGINAL_NO_PADDING: i64 = 3;
    pub(crate) const SODIUM_BASE64_VARIANT_URLSAFE: i64 = 5;
    pub(crate) const SODIUM_BASE64_VARIANT_URLSAFE_NO_PADDING: i64 = 7;
    pub(crate) const SODIUM_CRYPTO_SCALARMULT_RISTRETTO255_BYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_SCALARMULT_RISTRETTO255_SCALARBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_CORE_RISTRETTO255_BYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_CORE_RISTRETTO255_HASHBYTES: i64 = 64;
    pub(crate) const SODIUM_CRYPTO_CORE_RISTRETTO255_SCALARBYTES: i64 = 32;
    pub(crate) const SODIUM_CRYPTO_CORE_RISTRETTO255_NONREDUCEDSCALARBYTES: i64 = 64;
}

/// Core's `Exception`, named for `extends`.
#[php_class(extern)]
pub(crate) struct Exception;

/// `class SodiumException extends Exception`, what every refusal of
/// this extension throws.
///
/// ```php
/// function f() { throw new SodiumException('test'); }
/// try { f(); } catch (SodiumException $e) { var_dump(get_parent_class($e), $e->getMessage()); }
/// function g($s) { return sodium_bin2base64($s, 0); }
/// try { g('x'); } catch (SodiumException $e) { var_dump($e->getMessage(), $e->getTrace()[0]['args'], $e->getTrace()[1]['args']); }
/// function h($secret) { return new SodiumException('x'); }
/// var_dump(h('hunter2')->getTrace()[0]['args']);
/// class Mine extends SodiumException {}
/// function k($s) { return new Mine('y'); }
/// var_dump(k('pw')->getTrace()[0]['args']);
/// try { clone h(1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(9) "Exception"
/// string(4) "test"
/// string(80) "sodium_bin2base64(): Argument #2 ($id) must be a valid base64 variant identifier"
/// array(0) {
/// }
/// array(0) {
/// }
/// array(0) {
/// }
/// array(0) {
/// }
/// Trying to clone an uncloneable object of class SodiumException
/// ```
#[php_class(extends(Exception), handlers = SODIUM_EXCEPTION)]
pub(crate) struct SodiumException;

/// `SodiumException`'s handlers: the throwables' `clone_obj`, and
/// `sodium_exception_create_object`, which strips the arguments from
/// the trace `zend_default_exception_new` has written.
pub(crate) struct SodiumHandlers;

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for SodiumHandlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    fn create_object(&self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        scrub(req, object)
    }
}

static SODIUM_EXCEPTION: SodiumHandlers = SodiumHandlers;

#[php_class]
impl SodiumException {}

/// The process scope's proof that libsodium is started (ADR-0010).
#[derive(EngineState)]
struct Initialized;

/// `PHP_MINIT_FUNCTION(sodium)`'s first call, `sodium_init()`
/// (`Z/ext/sodium/libsodium.c:159-161`), the process start: called once
/// for the process, the first time an engine carrying sodium is built.
/// Only `-1` is a failure, which php answers with
/// `zend_error_noreturn(E_ERROR, "sodium_init()")`: the fatal is raised
/// before the refusal here, and the engine then says
/// `Unable to start sodium module`, where php's `zend_error_noreturn` aborts the process
/// after the fatal. The library fails only where it cannot read
/// randomness, so the difference is not met in practice.
fn initialize() -> Result<Initialized, Refusal> {
    if ffi::init() {
        Ok(Initialized)
    } else {
        Err(Refusal::new().raise(Level::Error, "sodium_init()"))
    }
}

/// The extension value (design §7.1): `sodium_module_entry`, after
/// `standard` (`ZEND_MOD_REQUIRED("standard")`), its constants, its
/// functions in the stub's order and `SodiumException`, and
/// `sodium_init` as its process start (`initialize`).
///
/// ```php
/// var_dump(extension_loaded('sodium'), count(get_extension_funcs('sodium')), class_exists('SodiumException'));
/// echo (new ReflectionExtension('sodium'))->getDependencies()['standard'], "\n";
/// ```
/// ```expect
/// bool(true)
/// int(110)
/// bool(true)
/// Required
/// ```
#[allow(clippy::too_many_lines)] // one line per function of the stub
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .process_state(initialize)
        .dependencies(vec![Dependency {
            name: String::from("standard"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .constants(constants::DECLARED)
        .functions(&[
            aead::sodium_crypto_aead_aes256gcm_is_available::DECLARED,
            aead::sodium_crypto_aead_aes256gcm_decrypt::DECLARED,
            aead::sodium_crypto_aead_aes256gcm_encrypt::DECLARED,
            aead::sodium_crypto_aead_aes256gcm_keygen::DECLARED,
            #[cfg(libsodium_aegis)]
            aead::aegis::sodium_crypto_aead_aegis128l_decrypt::DECLARED,
            #[cfg(libsodium_aegis)]
            aead::aegis::sodium_crypto_aead_aegis128l_encrypt::DECLARED,
            #[cfg(libsodium_aegis)]
            aead::aegis::sodium_crypto_aead_aegis128l_keygen::DECLARED,
            #[cfg(libsodium_aegis)]
            aead::aegis::sodium_crypto_aead_aegis256_decrypt::DECLARED,
            #[cfg(libsodium_aegis)]
            aead::aegis::sodium_crypto_aead_aegis256_encrypt::DECLARED,
            #[cfg(libsodium_aegis)]
            aead::aegis::sodium_crypto_aead_aegis256_keygen::DECLARED,
            aead::sodium_crypto_aead_chacha20poly1305_decrypt::DECLARED,
            aead::sodium_crypto_aead_chacha20poly1305_encrypt::DECLARED,
            aead::sodium_crypto_aead_chacha20poly1305_keygen::DECLARED,
            aead::sodium_crypto_aead_chacha20poly1305_ietf_decrypt::DECLARED,
            aead::sodium_crypto_aead_chacha20poly1305_ietf_encrypt::DECLARED,
            aead::sodium_crypto_aead_chacha20poly1305_ietf_keygen::DECLARED,
            aead::sodium_crypto_aead_xchacha20poly1305_ietf_decrypt::DECLARED,
            aead::sodium_crypto_aead_xchacha20poly1305_ietf_keygen::DECLARED,
            aead::sodium_crypto_aead_xchacha20poly1305_ietf_encrypt::DECLARED,
            hash::sodium_crypto_auth::DECLARED,
            hash::sodium_crypto_auth_keygen::DECLARED,
            hash::sodium_crypto_auth_verify::DECLARED,
            keys::sodium_crypto_box::DECLARED,
            keys::sodium_crypto_box_keypair::DECLARED,
            keys::sodium_crypto_box_seed_keypair::DECLARED,
            keys::sodium_crypto_box_keypair_from_secretkey_and_publickey::DECLARED,
            keys::sodium_crypto_box_open::DECLARED,
            keys::sodium_crypto_box_publickey::DECLARED,
            keys::sodium_crypto_box_publickey_from_secretkey::DECLARED,
            keys::sodium_crypto_box_seal::DECLARED,
            keys::sodium_crypto_box_seal_open::DECLARED,
            keys::sodium_crypto_box_secretkey::DECLARED,
            hash::sodium_crypto_core_ristretto255_add::DECLARED,
            hash::sodium_crypto_core_ristretto255_from_hash::DECLARED,
            hash::sodium_crypto_core_ristretto255_is_valid_point::DECLARED,
            hash::sodium_crypto_core_ristretto255_random::DECLARED,
            hash::sodium_crypto_core_ristretto255_scalar_add::DECLARED,
            hash::sodium_crypto_core_ristretto255_scalar_complement::DECLARED,
            hash::sodium_crypto_core_ristretto255_scalar_invert::DECLARED,
            hash::sodium_crypto_core_ristretto255_scalar_mul::DECLARED,
            hash::sodium_crypto_core_ristretto255_scalar_negate::DECLARED,
            hash::sodium_crypto_core_ristretto255_scalar_random::DECLARED,
            hash::sodium_crypto_core_ristretto255_scalar_reduce::DECLARED,
            hash::sodium_crypto_core_ristretto255_scalar_sub::DECLARED,
            hash::sodium_crypto_core_ristretto255_sub::DECLARED,
            keys::sodium_crypto_kx_keypair::DECLARED,
            keys::sodium_crypto_kx_publickey::DECLARED,
            keys::sodium_crypto_kx_secretkey::DECLARED,
            keys::sodium_crypto_kx_seed_keypair::DECLARED,
            keys::sodium_crypto_kx_client_session_keys::DECLARED,
            keys::sodium_crypto_kx_server_session_keys::DECLARED,
            hash::sodium_crypto_generichash::DECLARED,
            hash::sodium_crypto_generichash_keygen::DECLARED,
            hash::sodium_crypto_generichash_init::DECLARED,
            hash::sodium_crypto_generichash_update::DECLARED,
            hash::sodium_crypto_generichash_final::DECLARED,
            hash::sodium_crypto_kdf_derive_from_key::DECLARED,
            hash::sodium_crypto_kdf_keygen::DECLARED,
            hash::sodium_crypto_pwhash::DECLARED,
            hash::sodium_crypto_pwhash_str::DECLARED,
            hash::sodium_crypto_pwhash_str_verify::DECLARED,
            hash::sodium_crypto_pwhash_str_needs_rehash::DECLARED,
            hash::sodium_crypto_pwhash_scryptsalsa208sha256::DECLARED,
            hash::sodium_crypto_pwhash_scryptsalsa208sha256_str::DECLARED,
            hash::sodium_crypto_pwhash_scryptsalsa208sha256_str_verify::DECLARED,
            keys::sodium_crypto_scalarmult::DECLARED,
            keys::sodium_crypto_scalarmult_ristretto255::DECLARED,
            keys::sodium_crypto_scalarmult_ristretto255_base::DECLARED,
            keys::sodium_crypto_secretbox::DECLARED,
            keys::sodium_crypto_secretbox_keygen::DECLARED,
            keys::sodium_crypto_secretbox_open::DECLARED,
            hash::sodium_crypto_secretstream_xchacha20poly1305_keygen::DECLARED,
            hash::sodium_crypto_secretstream_xchacha20poly1305_init_push::DECLARED,
            hash::sodium_crypto_secretstream_xchacha20poly1305_push::DECLARED,
            hash::sodium_crypto_secretstream_xchacha20poly1305_init_pull::DECLARED,
            hash::sodium_crypto_secretstream_xchacha20poly1305_pull::DECLARED,
            hash::sodium_crypto_secretstream_xchacha20poly1305_rekey::DECLARED,
            hash::sodium_crypto_shorthash::DECLARED,
            hash::sodium_crypto_shorthash_keygen::DECLARED,
            keys::sodium_crypto_sign::DECLARED,
            keys::sodium_crypto_sign_detached::DECLARED,
            keys::sodium_crypto_sign_ed25519_pk_to_curve25519::DECLARED,
            keys::sodium_crypto_sign_ed25519_sk_to_curve25519::DECLARED,
            keys::sodium_crypto_sign_keypair::DECLARED,
            keys::sodium_crypto_sign_keypair_from_secretkey_and_publickey::DECLARED,
            keys::sodium_crypto_sign_open::DECLARED,
            keys::sodium_crypto_sign_publickey::DECLARED,
            keys::sodium_crypto_sign_secretkey::DECLARED,
            keys::sodium_crypto_sign_publickey_from_secretkey::DECLARED,
            keys::sodium_crypto_sign_seed_keypair::DECLARED,
            keys::sodium_crypto_sign_verify_detached::DECLARED,
            hash::sodium_crypto_stream::DECLARED,
            hash::sodium_crypto_stream_keygen::DECLARED,
            hash::sodium_crypto_stream_xor::DECLARED,
            hash::sodium_crypto_stream_xchacha20::DECLARED,
            hash::sodium_crypto_stream_xchacha20_keygen::DECLARED,
            hash::sodium_crypto_stream_xchacha20_xor::DECLARED,
            hash::sodium_crypto_stream_xchacha20_xor_ic::DECLARED,
            sodium_add::DECLARED,
            sodium_compare::DECLARED,
            sodium_increment::DECLARED,
            sodium_memcmp::DECLARED,
            sodium_memzero::DECLARED,
            sodium_pad::DECLARED,
            sodium_unpad::DECLARED,
            sodium_bin2hex::DECLARED,
            sodium_hex2bin::DECLARED,
            sodium_bin2base64::DECLARED,
            sodium_base642bin::DECLARED,
            keys::sodium_crypto_scalarmult_base::DECLARED,
        ])
        .class::<SodiumException>()
        .engine_state(throwables::Throwables::start)
        .info(sodium_info)
        .build("sodium")
}

/// `PHP_MINFO_FUNCTION(sodium)`: the support row, the headers' version
/// (`pkg-config`'s, which `build.rs` read) and the linked library's.
///
/// ```php
/// ob_start(); (new ReflectionExtension('sodium'))->info(); $info = ob_get_clean();
/// echo str_replace(SODIUM_LIBRARY_VERSION, 'X', $info);
/// ```
/// ```expect
///
/// sodium
///
/// sodium support => enabled
/// libsodium headers version => X
/// libsodium library version => X
/// ```
fn sodium_info(req: &mut Request) {
    let library = ffi::version();
    req.extensions_mut().info_table(&[
        &["sodium support", "enabled"],
        &["libsodium headers version", env!("SODIUM_HEADERS_VERSION")],
        &["libsodium library version", &library],
    ]);
}

/// A native's `zend_parse_parameters`: the arity, then each argument
/// `read` holds, a refusal of either leaving with the arguments out of
/// its trace (`sodium_remove_param_values_from_backtrace(EG(exception))`).
pub(crate) fn parsed<T>(
    req: &mut Request,
    min: usize,
    max: usize,
    read: impl FnOnce(&mut Request) -> Result<T, Thrown>,
) -> Result<T, Thrown> {
    let held = arity(req, min, Some(max)).and_then(|()| read(req));
    held.map_err(|thrown| scrubbed(req, thrown))
}

/// `thrown` with its throwable's trace stripped of arguments.
fn scrubbed(req: &mut Request, thrown: Thrown) -> Thrown {
    let Some(object) = thrown.exception().copied() else {
        return thrown;
    };
    match scrub(req, object) {
        Ok(()) => thrown,
        Err(later) => thrown.chain(req, later),
    }
}

/// `sodium_remove_param_values_from_backtrace`: every frame of the
/// `trace` property that has `args` gets the empty array there.
fn scrub(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    let Some(trace) = object
        .prop(req, b"trace")
        .and_then(|value| value.as_array())
    else {
        return Ok(());
    };
    let framed: Vec<(Key, PhpArray)> = walk::rows(req, trace)
        .filter_map(|(key, frame)| {
            let frame = frame.as_array()?;
            frame.find(b"args").map(|_| (key, frame))
        })
        .collect();
    if framed.is_empty() {
        return Ok(());
    }
    let mut copy = trace.copy(req)?;
    for (key, frame) in framed {
        let mut frame = frame.copy(req)?;
        let args = PhpString::new(req, b"args")?;
        let empty = Owner::take(req, Value::array(PhpArray::empty()));
        let replaced = frame.replace(req, &Key::Str(*args), empty);
        args.give_up(req);
        let replaced = match replaced {
            Ok(_) => copy.replace(req, &key, frame.into_value()),
            Err(thrown) => {
                frame.give_up(req);
                Err(thrown)
            }
        };
        if let Err(thrown) = replaced {
            copy.give_up(req);
            return Err(thrown);
        }
    }
    php_ext::call::write_prop(req, &object, b"trace", copy.into_value())
}

/// `zend_throw_exception(sodium_exception_ce, message, 0)`.
pub(crate) fn fail(req: &mut Request, message: &str) -> Thrown {
    let ce = match throwables::Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    throw(req, ce.sodium_exception, message)
}

/// `zend_argument_error(sodium_exception_ce, position, tail)`.
pub(crate) fn bad_arg(req: &mut Request, position: u32, tail: &str) -> Thrown {
    let ce = match throwables::Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    argument_error(req, ce.sodium_exception, position, tail)
}

/// `"internal error"`, what a libsodium call that fails past php's own
/// checks throws.
pub(crate) fn internal(req: &mut Request) -> Thrown {
    fail(req, "internal error")
}

/// A string of these bytes.
pub(crate) fn made(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, bytes)
}

/// `n` random bytes, `randombytes_buf`, as every `*_keygen` makes its
/// key.
pub(crate) fn random_key(req: &mut Request, n: usize) -> Result<Owner<PhpString>, Thrown> {
    let mut key = vec![0u8; n];
    ffi::random(&mut key);
    let made = made(req, &key);
    key.fill(0);
    made
}

/// The bytes of a by-reference argument `ZVAL_DEREF` finds a string in,
/// or none.
pub(crate) fn referent(cell: &Ref<Raw<PhpString>>) -> Option<Vec<u8>> {
    let value = cell.get()?.value();
    value.as_str_bytes().map(<[u8]>::to_vec)
}

/// A new string of these bytes written through a by-reference argument,
/// the caller's variable where it passed one ([`Ref::set`]).
pub(crate) fn assign_bytes(
    req: &mut Request,
    cell: &Ref<Raw<PhpString>>,
    bytes: &[u8],
) -> Result<(), Thrown> {
    let string = made(req, bytes)?;
    cell.set(req, string)
}

/// `sodium_add(string &$string1, string $string2): void`: the two as
/// little-endian numbers, the sum written back into the first.
///
/// ```php
/// $a = "\xff\x00\x01"; $b = $a; sodium_add($a, "\x01\x00\x00"); var_dump(bin2hex($a), bin2hex($b));
/// $n = 1; try { sodium_add($n, "\x01"); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// $a = "ab"; try { sodium_add($a, "abc"); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(6) "000101"
/// string(6) "ff0001"
/// PHP strings are required
/// sodium_add(): Argument #1 ($string1) and argument #2 ($string_2) must have the same length
/// ```
#[php_function(arity = manual)]
fn sodium_add(
    req: &mut Request,
    string1: Ref<Raw<PhpString>>,
    string2: Raw<PhpString>,
) -> Result<(), Thrown> {
    let addend = parsed(req, 2, 2, |req| string2.hold(req))?;
    let Some(mut sum) = referent(&string1) else {
        return Err(fail(req, "PHP strings are required"));
    };
    if sum.len() != addend.len() {
        return Err(bad_arg(
            req,
            1,
            "and argument #2 ($string_2) must have the same length",
        ));
    }
    if ffi::add(&mut sum, addend.as_bytes()).is_err() {
        return Err(internal(req));
    }
    assign_bytes(req, &string1, &sum)
}

/// `sodium_compare(string $string1, string $string2): int`: `-1`, `0` or
/// `1`, the two as little-endian numbers, in constant time.
///
/// ```php
/// var_dump(sodium_compare("\x01\x02", "\x02\x01"), sodium_compare("ab", "ab"), sodium_compare("\x00\x02", "\x01\x01"));
/// try { sodium_compare("a", "ab"); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// int(0)
/// int(1)
/// sodium_compare(): Argument #1 ($string1) and argument #2 ($string_2) must have the same length
/// ```
#[php_function(arity = manual)]
fn sodium_compare(
    req: &mut Request,
    #[php(sensitive)] string1: Raw<PhpString>,
    #[php(sensitive)] string2: Raw<PhpString>,
) -> Result<i64, Thrown> {
    let (a, b) = parsed(req, 2, 2, |req| {
        Ok((string1.hold(req)?, string2.hold(req)?))
    })?;
    if a.len() != b.len() {
        return Err(bad_arg(
            req,
            1,
            "and argument #2 ($string_2) must have the same length",
        ));
    }
    ffi::compare(a.as_bytes(), b.as_bytes()).map_err(|_| internal(req))
}

/// `sodium_increment(string &$string): void`: the string as a
/// little-endian number, plus one, written back.
///
/// ```php
/// $s = "abc"; $t = $s; sodium_increment($s); var_dump($s, $t);
/// $s = "\xff\xff"; sodium_increment($s); var_dump(bin2hex($s));
/// $n = 123; try { sodium_increment($n); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "bbc"
/// string(3) "abc"
/// string(4) "0000"
/// a PHP string is required
/// ```
#[php_function(arity = manual)]
fn sodium_increment(req: &mut Request, string: Ref<Raw<PhpString>>) -> Result<(), Thrown> {
    parsed(req, 1, 1, |_| Ok(()))?;
    let Some(mut number) = referent(&string) else {
        return Err(fail(req, "a PHP string is required"));
    };
    ffi::increment(&mut number);
    assign_bytes(req, &string, &number)
}

/// `sodium_memcmp(string $string1, string $string2): int`: `0` where the
/// two are equal, `-1` otherwise, in constant time.
///
/// ```php
/// var_dump(sodium_memcmp("string", "string"), sodium_memcmp("string", "String"));
/// try { sodium_memcmp("a", ""); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(0)
/// int(-1)
/// sodium_memcmp(): Argument #1 ($string1) and argument #2 ($string_2) must have the same length
/// ```
#[php_function(arity = manual)]
fn sodium_memcmp(
    req: &mut Request,
    #[php(sensitive)] string1: Raw<PhpString>,
    #[php(sensitive)] string2: Raw<PhpString>,
) -> Result<i64, Thrown> {
    let (a, b) = parsed(req, 2, 2, |req| {
        Ok((string1.hold(req)?, string2.hold(req)?))
    })?;
    if a.len() != b.len() {
        return Err(bad_arg(
            req,
            1,
            "and argument #2 ($string_2) must have the same length",
        ));
    }
    ffi::memcmp(a.as_bytes(), b.as_bytes()).map_err(|_| internal(req))
}

/// `sodium_memzero(string &$string): void`: the variable set to `null`.
/// php zeroes the bytes first where the variable is their one owner;
/// no PHP code can read a string after its last owner lets go of it, so
/// this crate lets the engine free it.
///
/// ```php
/// $a = 'test'; sodium_memzero($a); var_dump($a);
/// $b = str_repeat('x', 3); $c = $b; sodium_memzero($b); var_dump($b, $c);
/// $n = 42; try { sodium_memzero($n); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// NULL
/// NULL
/// string(3) "xxx"
/// a PHP string is required
/// ```
#[php_function(arity = manual)]
fn sodium_memzero(
    req: &mut Request,
    #[php(sensitive)] string: Ref<Raw<PhpString>>,
) -> Result<(), Thrown> {
    parsed(req, 1, 1, |_| Ok(()))?;
    if referent(&string).is_none() {
        return Err(fail(req, "a PHP string is required"));
    }
    string.set(req, Value::null())
}

/// A `block_size` argument: `zend_long` greater than zero, as `size_t`.
fn positive_block(req: &mut Request, size: i64) -> Result<usize, Thrown> {
    match usize::try_from(size) {
        Ok(size) if size > 0 => Ok(size),
        _ => Err(bad_arg(req, 2, "must be greater than 0")),
    }
}

/// `sodium_pad(string $string, int $block_size): string`: ISO/IEC 7816-4
/// padding, `0x80` and zeros to the next multiple of the block size.
///
/// ```php
/// var_dump(bin2hex(sodium_pad('xyz', 16)), strlen(sodium_pad('', 4)), strlen(sodium_pad('abcd', 4)), bin2hex(sodium_pad('ab', 3)));
/// try { sodium_pad('x', 0); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(32) "78797a80000000000000000000000000"
/// int(4)
/// int(8)
/// string(6) "616280"
/// sodium_pad(): Argument #2 ($block_size) must be greater than 0
/// ```
#[php_function(arity = manual)]
fn sodium_pad(
    req: &mut Request,
    #[php(sensitive)] string: Raw<PhpString>,
    block_size: Raw<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let (unpadded, size) = parsed(req, 2, 2, |req| {
        Ok((string.hold(req)?, block_size.hold(req)?))
    })?;
    let block = positive_block(req, size)?;
    let len = unpadded.len();
    let mut xpadlen = block - 1;
    if block.is_power_of_two() {
        xpadlen -= len & (block - 1);
    } else {
        xpadlen -= len % block;
    }
    if usize::MAX - len <= xpadlen {
        return Err(fail(req, "input is too large"));
    }
    let xpadded = len + xpadlen;
    let mut padded = vec![0u8; xpadded + 1];
    padded[..len].copy_from_slice(unpadded.as_bytes());
    if ffi::pad(&mut padded, len, block).is_err() {
        return Err(internal(req));
    }
    made(req, &padded)
}

/// `sodium_unpad(string $string, int $block_size): string`: the padding
/// [`sodium_pad`] adds, taken off.
///
/// ```php
/// var_dump(sodium_unpad(sodium_pad('xyz', 16), 16));
/// try { sodium_unpad('abcd', 4); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_unpad('ab', 4); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_unpad('ab', -1); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "xyz"
/// invalid padding
/// sodium_unpad(): Argument #1 ($string) must be at least as long as the block size
/// sodium_unpad(): Argument #2 ($block_size) must be greater than 0
/// ```
#[php_function(arity = manual)]
fn sodium_unpad(
    req: &mut Request,
    #[php(sensitive)] string: Raw<PhpString>,
    block_size: Raw<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let (padded, size) = parsed(req, 2, 2, |req| {
        Ok((string.hold(req)?, block_size.hold(req)?))
    })?;
    let block = positive_block(req, size)?;
    if padded.len() < block {
        return Err(bad_arg(
            req,
            1,
            "must be at least as long as the block size",
        ));
    }
    match ffi::unpad(padded.as_bytes(), block) {
        Ok(len) if len <= padded.len() => made(req, &padded.as_bytes()[..len]),
        _ => Err(fail(req, "invalid padding")),
    }
}

/// `sodium_bin2hex(string $string): string`, in constant time.
///
/// ```php
/// var_dump(sodium_bin2hex("\x00\xffAz"), sodium_bin2hex(''));
/// try { sodium_bin2hex(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(8) "00ff417a"
/// string(0) ""
/// sodium_bin2hex() expects exactly 1 argument, 0 given
/// ```
#[php_function(arity = manual)]
fn sodium_bin2hex(
    req: &mut Request,
    #[php(sensitive)] string: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let bin = parsed(req, 1, 1, |req| string.hold(req))?;
    if bin.len() >= ZSTR_MAX_LEN / 2 {
        return Err(fail(req, "arithmetic overflow"));
    }
    let mut hex = vec![0u8; bin.len() * 2 + 1];
    if ffi::bin2hex(&mut hex, bin.as_bytes()).is_err() {
        return Err(internal(req));
    }
    made(req, &hex[..bin.len() * 2])
}

/// `sodium_hex2bin(string $string, string $ignore = ""): string`, in
/// constant time, skipping the bytes of `$ignore`.
///
/// ```php
/// var_dump(bin2hex(sodium_hex2bin('00ff417A')), bin2hex(sodium_hex2bin('00:ff', ':')));
/// try { sodium_hex2bin('0g'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_hex2bin('abc'); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(8) "00ff417a"
/// string(4) "00ff"
/// sodium_hex2bin(): Argument #1 ($string) must be a valid hexadecimal string
/// sodium_hex2bin(): Argument #1 ($string) must be a valid hexadecimal string
/// ```
#[php_function(arity = manual)]
fn sodium_hex2bin(
    req: &mut Request,
    #[php(sensitive)] string: Raw<PhpString>,
    #[php(default = "")] ignore: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let (hex, ignore) = parsed(req, 1, 2, |req| Ok((string.hold(req)?, ignore.hold(req)?)))?;
    let mut bin = vec![0u8; hex.len() / 2];
    match ffi::hex2bin(&mut bin, hex.as_bytes(), ignore.as_bytes()) {
        Ok(len) => made(req, &bin[..len]),
        Err(_) => Err(bad_arg(req, 1, "must be a valid hexadecimal string")),
    }
}

/// The variant check `sodium_bin2base64` and `sodium_base642bin` share,
/// over `(unsigned int) variant`: `1`, `3`, `5` or `7`.
fn base64_variant(req: &mut Request, id: i64) -> Result<std::ffi::c_int, Thrown> {
    let bits = id & 0xffff_ffff;
    if bits & !0x6 != 0x1 {
        return Err(bad_arg(req, 2, "must be a valid base64 variant identifier"));
    }
    Ok(std::ffi::c_int::try_from(bits).unwrap_or(0))
}

/// `sodium_bin2base64(string $string, int $id): string`, in constant
/// time, in the variant `$id` names.
///
/// ```php
/// var_dump(sodium_bin2base64("\xfb\xff", SODIUM_BASE64_VARIANT_ORIGINAL), sodium_bin2base64("\xfb\xff", SODIUM_BASE64_VARIANT_URLSAFE_NO_PADDING), sodium_bin2base64('', SODIUM_BASE64_VARIANT_URLSAFE));
/// try { sodium_bin2base64('x', 2); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "+/8="
/// string(3) "-_8"
/// string(0) ""
/// sodium_bin2base64(): Argument #2 ($id) must be a valid base64 variant identifier
/// ```
#[php_function(arity = manual)]
fn sodium_bin2base64(
    req: &mut Request,
    #[php(sensitive)] string: Raw<PhpString>,
    id: Raw<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let (bin, id) = parsed(req, 2, 2, |req| Ok((string.hold(req)?, id.hold(req)?)))?;
    let variant = base64_variant(req, id)?;
    if bin.len() >= ZSTR_MAX_LEN / 4 * 3 - 3 - 1 {
        return Err(fail(req, "arithmetic overflow"));
    }
    let encoded = ffi::base64_encoded_len(bin.len(), usize::try_from(variant).unwrap_or(0));
    let mut b64 = vec![0u8; encoded];
    if ffi::bin2base64(&mut b64, bin.as_bytes(), variant).is_err() {
        return Err(internal(req));
    }
    made(req, &b64[..encoded - 1])
}

/// `sodium_base642bin(string $string, int $id, string $ignore = ""):
/// string`, in constant time, in the variant `$id` names, skipping the
/// bytes of `$ignore`.
///
/// ```php
/// var_dump(sodium_base642bin('YWJjZA', SODIUM_BASE64_VARIANT_ORIGINAL_NO_PADDING), sodium_base642bin(" YWJjZA==\n", SODIUM_BASE64_VARIANT_ORIGINAL, " \n"));
/// try { sodium_base642bin('O1R', SODIUM_BASE64_VARIANT_ORIGINAL_NO_PADDING); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// try { sodium_base642bin('x', 0); } catch (SodiumException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "abcd"
/// string(4) "abcd"
/// sodium_base642bin(): Argument #1 ($string) must be a valid base64 string
/// sodium_base642bin(): Argument #2 ($id) must be a valid base64 variant identifier
/// ```
#[php_function(arity = manual)]
fn sodium_base642bin(
    req: &mut Request,
    #[php(sensitive)] string: Raw<PhpString>,
    id: Raw<i64>,
    #[php(default = "")] ignore: Raw<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let (b64, id, ignore) = parsed(req, 2, 3, |req| {
        Ok((string.hold(req)?, id.hold(req)?, ignore.hold(req)?))
    })?;
    let variant = base64_variant(req, id)?;
    let mut bin = vec![0u8; b64.len() / 4 * 3 + 2];
    match ffi::base642bin(&mut bin, b64.as_bytes(), ignore.as_bytes(), variant) {
        Ok(len) => made(req, &bin[..len]),
        Err(_) => Err(bad_arg(req, 1, "must be a valid base64 string")),
    }
}
