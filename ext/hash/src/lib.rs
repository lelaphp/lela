//! The `hash` extension: `php-src/ext/hash` as one crate under `ext/`
//! (ADR-0004). One dependency beside the facade, ext/standard, whose
//! `md5.h` and `sha1.h` `php_hash_md.h` and `php_hash_sha.h` include; one
//! entry, `extension()`; its state on the value (ADR-0005). The phpt
//! subset is `php-src/ext/hash/tests`, through `xtask phpt --subset`.
//!
//! `hash.c`'s natives, `HashContext` over a payload of php's
//! `php_hashcontext_object`, the `mhash*` compatibility natives and the
//! `MHASH_*` constants. The algorithms are php's table in its order;
//! each state is php's C struct as fields, so `__serialize` writes what
//! php writes.
#![forbid(unsafe_code)]
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
// The contexts are php's C: its casts truncate and wrap where the ports
// cast, and the rounds keep the one-letter names of the papers.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]

mod algos;
mod checksum;
mod gost;
mod haval;
mod md;
mod murmur;
mod natives;
mod sha;
mod snefru;
mod throwables;
mod tiger;
mod whirlpool;
mod xxhash;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::raising::Thrown;
use php_ext::request::Request;

use crate::natives::{
    HashContext, hash, hash_algos, hash_copy, hash_equals, hash_file, hash_final, hash_hkdf,
    hash_hmac, hash_hmac_algos, hash_hmac_file, hash_init, hash_pbkdf2, hash_update,
    hash_update_file, hash_update_stream, mhash, mhash_count, mhash_get_block_size,
    mhash_get_hash_name, mhash_keygen_s2k,
};

/// One algorithm's running state, what `php_hash.h` hands an extension
/// that hashes through ext/hash (design §4.10, the `hash` → `phar` row):
/// [`Digest::fetch`] is `php_hash_fetch_ops` with the ops' `hash_init`,
/// [`Digest::update`] and [`Digest::finish`] its `hash_update` and
/// `hash_final`, as `phar_verify_signature` and `phar_create_signature`
/// hash an archive with `PHP_SHA256Init` and its kin.
///
/// ```php
/// $m = pack('VnVVV', 1, 0x1000, 0x10000, 0, 0) . pack('V', 1) . 'a' . pack('VVVVVV', 1, 0, 1, crc32('x'), 0x1b6, 0);
/// $data = "<?php __HALT_COMPILER(); ?>\r\n" . pack('V', strlen($m)) . $m . 'x';
/// foreach ([3 => 'sha256', 4 => 'sha512', 2 => 'sha1', 1 => 'md5'] as $flag => $algo) {
///     $p = __DIR__ . "/hash-digest-$algo.phar";
///     file_put_contents($p, $data . hash($algo, 'other', true) . pack('V', $flag) . 'GBMB');
///     echo str_replace($p, 'P', @file_get_contents("phar://$p/a") === false ? error_get_last()['message'] : ''), "\n";
///     $q = __DIR__ . "/hash-digest-$algo-ok.phar";
///     file_put_contents($q, $data . hash($algo, $data, true) . pack('V', $flag) . 'GBMB');
///     var_dump(file_get_contents("phar://$q/a"));
///     unlink($p); unlink($q);
/// }
/// ```
/// ```expect
/// file_get_contents(phar://P/a): Failed to open stream: phar "P" SHA256 signature could not be verified: broken signature
/// string(1) "x"
/// file_get_contents(phar://P/a): Failed to open stream: phar "P" SHA512 signature could not be verified: broken signature
/// string(1) "x"
/// file_get_contents(phar://P/a): Failed to open stream: phar "P" SHA1 signature could not be verified: broken signature
/// string(1) "x"
/// file_get_contents(phar://P/a): Failed to open stream: phar "P" MD5 signature could not be verified: broken signature
/// string(1) "x"
/// ```
pub struct Digest(Box<dyn algos::State>);

impl Digest {
    /// `php_hash_fetch_ops(algo)` and its `hash_init` with no options:
    /// `None` for a name the table has not.
    pub fn fetch(req: &mut Request, algo: &[u8]) -> Result<Option<Digest>, Thrown> {
        let Some(found) = algos::fetch(algo) else {
            return Ok(None);
        };
        Ok(Some(Digest((found.init)(req, algos::Options::default())?)))
    }

    /// `hash_update`.
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    /// `hash_final`: the digest's bytes.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        self.0.finish()
    }
}

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {}

/// The extension value (design §7.1): declarations paired with entries,
/// and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('hash'), HASH_HMAC, hash_algos()[28], (new ReflectionClass('HashContext'))->isFinal());
/// ```
/// ```expect
/// bool(true)
/// int(1)
/// string(11) "gost-crypto"
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(&[
            hash::DECLARED,
            hash_file::DECLARED,
            hash_hmac::DECLARED,
            hash_hmac_file::DECLARED,
            hash_init::DECLARED,
            hash_update::DECLARED,
            hash_update_stream::DECLARED,
            hash_update_file::DECLARED,
            hash_final::DECLARED,
            hash_copy::DECLARED,
            hash_algos::DECLARED,
            hash_hmac_algos::DECLARED,
            hash_pbkdf2::DECLARED,
            hash_equals::DECLARED,
            hash_hkdf::DECLARED,
            mhash_get_block_size::DECLARED,
            mhash_get_hash_name::DECLARED,
            mhash_keygen_s2k::DECLARED,
            mhash_count::DECLARED,
            mhash::DECLARED,
        ])
        .class::<HashContext>()
        .constants(natives::constants::declared())
        .info(natives::info)
        .request_state::<State>()
        .engine_state(throwables::Throwables::start)
        .build("hash")
}
