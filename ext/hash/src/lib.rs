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
mod tiger;
mod whirlpool;
mod xxhash;

use php_ext::abi::{Extension, ExtensionBuilder};

use crate::natives::{
    HashContext, hash, hash_algos, hash_copy, hash_equals, hash_file, hash_final, hash_hkdf,
    hash_hmac, hash_hmac_algos, hash_hmac_file, hash_init, hash_pbkdf2, hash_update,
    hash_update_file, hash_update_stream, mhash, mhash_count, mhash_get_block_size,
    mhash_get_hash_name, mhash_keygen_s2k,
};

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
        .functions(vec![
            hash::declared(),
            hash_file::declared(),
            hash_hmac::declared(),
            hash_hmac_file::declared(),
            hash_init::declared(),
            hash_update::declared(),
            hash_update_stream::declared(),
            hash_update_file::declared(),
            hash_final::declared(),
            hash_copy::declared(),
            hash_algos::declared(),
            hash_hmac_algos::declared(),
            hash_pbkdf2::declared(),
            hash_equals::declared(),
            hash_hkdf::declared(),
            mhash_get_block_size::declared(),
            mhash_get_hash_name::declared(),
            mhash_keygen_s2k::declared(),
            mhash_count::declared(),
            mhash::declared(),
        ])
        .class::<HashContext>()
        .constants(natives::constants::declared())
        .info(natives::info)
        .request_state::<State>()
        .build("hash")
}
