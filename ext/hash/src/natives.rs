//! `hash.c`: the natives, `HashContext` and the `mhash*` compatibility
//! layer, with `php_hashcontext_object` as the class's payload.
//
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ops::ControlFlow;

use php_ext::abi::{CloneObj, Instance, ObjectHandlers, OrFalse, Path, Raw, This, True};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::value_name;
use php_ext::raising::{
    Thrown, arg_type_error, arg_value_error, throw, throw_error, while_pending,
};
use php_ext::request::{OwnedStream as _, Request, Stream, StreamCtx, Streams as _};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{
    Emalloc, FreshString, Key, Owner, PhpArray, PhpObject, PhpString, Resource, Value, ValueRef,
};

use crate::algos::{self, ALGOS, Algo, Options, Refusal, State};

/// `hash_init`'s `$flags`, php's `context->options`: kept whole, as
/// `__serialize` writes it back as given, and read through [`Flags::hmac`].
#[derive(Debug, Default, Clone, Copy)]
struct Flags(i64);

impl Flags {
    /// `options & PHP_HASH_HMAC`.
    fn hmac(self) -> bool {
        self.0 & constants::HASH_HMAC != 0
    }
}

/// `mhash_to_hash`: the mhash name and the table's for each of
/// php's 42 numbers, `None` for the four it leaves empty.
#[rustfmt::skip]
const MHASH: [Option<(&str, &str)>; 42] = [
    Some(("CRC32", "crc32")), Some(("MD5", "md5")), Some(("SHA1", "sha1")), Some(("HAVAL256", "haval256,3")),
    None, Some(("RIPEMD160", "ripemd160")), None, Some(("TIGER", "tiger192,3")), Some(("GOST", "gost")),
    Some(("CRC32B", "crc32b")), Some(("HAVAL224", "haval224,3")), Some(("HAVAL192", "haval192,3")),
    Some(("HAVAL160", "haval160,3")), Some(("HAVAL128", "haval128,3")), Some(("TIGER128", "tiger128,3")),
    Some(("TIGER160", "tiger160,3")), Some(("MD4", "md4")), Some(("SHA256", "sha256")), Some(("ADLER32", "adler32")),
    Some(("SHA224", "sha224")), Some(("SHA512", "sha512")), Some(("SHA384", "sha384")), Some(("WHIRLPOOL", "whirlpool")),
    Some(("RIPEMD128", "ripemd128")), Some(("RIPEMD256", "ripemd256")), Some(("RIPEMD320", "ripemd320")), None,
    Some(("SNEFRU256", "snefru256")), Some(("MD2", "md2")), Some(("FNV132", "fnv132")), Some(("FNV1A32", "fnv1a32")),
    Some(("FNV164", "fnv164")), Some(("FNV1A64", "fnv1a64")), Some(("JOAAT", "joaat")), Some(("CRC32C", "crc32c")),
    Some(("MURMUR3A", "murmur3a")), Some(("MURMUR3C", "murmur3c")), Some(("MURMUR3F", "murmur3f")),
    Some(("XXH32", "xxh32")), Some(("XXH64", "xxh64")), Some(("XXH3", "xxh3")), Some(("XXH128", "xxh128")),
];

/// `HASH_HMAC`, then `mhash_init`'s `MHASH_*`, each deprecated since
/// 8.5.
///
/// ```php
/// var_dump(MHASH_SHA1, MHASH_XXH128);
/// echo (new ReflectionExtension('hash'))->getConstants()['MHASH_GOST'], "\n";
/// ```
/// ```expect
///
/// Deprecated: Constant MHASH_SHA1 is deprecated since 8.5, as the mhash*() functions were deprecated in %s on line 2
///
/// Deprecated: Constant MHASH_XXH128 is deprecated since 8.5, as the mhash*() functions were deprecated in %s on line 2
/// int(2)
/// int(41)
/// 8
/// ```
#[php_const]
pub(crate) mod constants {
    /// `PHP_HASH_HMAC`, `hash_init`'s `$flags` bit.
    pub(crate) const HASH_HMAC: i64 = 1;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_CRC32: i64 = 0;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_MD5: i64 = 1;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_SHA1: i64 = 2;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_HAVAL256: i64 = 3;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_RIPEMD160: i64 = 5;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_TIGER: i64 = 7;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_GOST: i64 = 8;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_CRC32B: i64 = 9;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_HAVAL224: i64 = 10;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_HAVAL192: i64 = 11;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_HAVAL160: i64 = 12;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_HAVAL128: i64 = 13;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_TIGER128: i64 = 14;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_TIGER160: i64 = 15;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_MD4: i64 = 16;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_SHA256: i64 = 17;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_ADLER32: i64 = 18;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_SHA224: i64 = 19;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_SHA512: i64 = 20;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_SHA384: i64 = 21;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_WHIRLPOOL: i64 = 22;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_RIPEMD128: i64 = 23;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_RIPEMD256: i64 = 24;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_RIPEMD320: i64 = 25;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_SNEFRU256: i64 = 27;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_MD2: i64 = 28;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_FNV132: i64 = 29;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_FNV1A32: i64 = 30;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_FNV164: i64 = 31;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_FNV1A64: i64 = 32;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_JOAAT: i64 = 33;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_CRC32C: i64 = 34;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_MURMUR3A: i64 = 35;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_MURMUR3C: i64 = 36;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_MURMUR3F: i64 = 37;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_XXH32: i64 = 38;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_XXH64: i64 = 39;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_XXH3: i64 = 40;
    #[php(deprecated(since = "8.5", message = "as the mhash*() functions were deprecated"))]
    const MHASH_XXH128: i64 = 41;
}

/// `PHP_MINFO_FUNCTION(hash)`.
///
/// ```php
/// $e = new ReflectionExtension('hash'); ob_start(); $e->info(); $i = ob_get_clean();
/// echo substr_count($i, 'sha3-512 '), ' ', str_contains($i, 'MHASH API Version => Emulated Support') ? 'yes' : 'no', "\n";
/// ```
/// ```expect
/// 1 yes
/// ```
pub(crate) fn info(req: &mut Request) {
    let engines: String = ALGOS.iter().flat_map(|a| [a.name, " "]).collect();
    req.extensions_mut()
        .info_table(&[&["hash support", "enabled"], &["Hashing Engines", &engines]]);
    req.extensions_mut().info_table(&[
        &["MHASH support", "Enabled"],
        &["MHASH API Version", "Emulated Support"],
    ]);
}

// ------------------------------------------------------------ helpers

/// `PHP_HASHCONTEXT_VERIFY`'s `TypeError`.
fn finalized(req: &mut Request, position: u32) -> Thrown {
    arg_type_error(req, position, "be a valid, non-finalized HashContext")
}

/// `PHP_HASHCONTEXT_VERIFY`: a state not yet finalised.
fn verify(req: &mut Request, object: PhpObject, position: u32) -> Result<PhpObject, Thrown> {
    if object
        .native::<Context>(req)
        .is_some_and(|c| c.running.is_some())
    {
        Ok(object)
    } else {
        Err(finalized(req, position))
    }
}

/// `php_hash_fetch_ops`, and the `ValueError` for a name it does not
/// know, or one HMAC cannot take.
fn fetch(req: &mut Request, name: &[u8], crypto: bool) -> Result<&'static Algo, Thrown> {
    match algos::fetch(name) {
        Some(algo) if !crypto || algo.crypto => Ok(algo),
        _ if crypto => Err(arg_value_error(
            req,
            1,
            "be a valid cryptographic hashing algorithm",
        )),
        _ => Err(arg_value_error(req, 1, "be a valid hashing algorithm")),
    }
}

/// `php_hash_bin2hex`.
fn hex(digest: &[u8]) -> Vec<u8> {
    const HEXITS: &[u8; 16] = b"0123456789abcdef";
    digest
        .iter()
        .flat_map(|b| [HEXITS[usize::from(b >> 4)], HEXITS[usize::from(b & 15)]])
        .collect()
}

/// The digest as a native answers it: raw, or its hex.
/// The one-shot hashes' ending, `php_hash_do_hash`'s and
/// `php_hash_do_hash_hmac`'s: `digest`, laid into
/// `zend_string_alloc(ops->digest_size)` (`raw`, or a fresh one where
/// the caller allocated none) while the state's allocation stands, it and
/// what `held` names given back, then the digest itself, or its hex
/// string with the digest given back (`Z/ext/hash/hash.c`).
fn rendered(
    req: &mut Request,
    raw: Option<FreshString>,
    held: [Option<Emalloc>; 2],
    digest: &[u8],
    binary: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let raw = match raw {
        Some(raw) => Ok(raw),
        None => FreshString::new(req, digest.len()),
    };
    let raw = raw.map(|mut raw| {
        raw.bytes_mut().copy_from_slice(digest);
        raw
    });
    for held in held.into_iter().flatten() {
        held.free(req);
    }
    let raw = raw?;
    if binary {
        return Ok(raw.finish());
    }
    let hexed = PhpString::new(req, &hex(digest));
    raw.free(req);
    hexed
}

/// `php_hash_alloc_context(ops)`, the state's allocation a one-shot
/// hash holds while it runs.
fn context(req: &mut Request, algo: &Algo) -> Result<Emalloc, Thrown> {
    Emalloc::new(req, algo.context)
}

fn digest_value(
    req: &mut Request,
    digest: &[u8],
    binary: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let bytes = if binary { digest.to_vec() } else { hex(digest) };
    PhpString::new(req, &bytes)
}

/// `hash_init(context, NULL)`, the pieces, `hash_final`.
fn digest_of(req: &mut Request, algo: &Algo, pieces: &[&[u8]]) -> Result<Vec<u8>, Thrown> {
    let mut state = (algo.init)(req, Options::default())?;
    for piece in pieces {
        state.update(piece);
    }
    Ok(state.finish())
}

/// `php_hash_hmac_prep_key`: the key, reduced first if it is longer
/// than a block, zero-padded to one, the inner pad applied.
fn hmac_key(req: &mut Request, algo: &Algo, key: &[u8]) -> Result<Vec<u8>, Thrown> {
    let mut k = if key.len() > algo.block {
        digest_of(req, algo, &[key])?
    } else {
        key.to_vec()
    };
    k.resize(algo.block, 0);
    Ok(k.iter().map(|b| b ^ 0x36).collect())
}

/// `K ^ 0x6A`: the outer pad from the inner.
fn outer(key: &[u8]) -> Vec<u8> {
    key.iter().map(|b| b ^ 0x6A).collect()
}

/// `php_hash_hmac_round` twice: the inner digest, then the outer.
fn hmac(req: &mut Request, algo: &Algo, key: &[u8], data: &[u8]) -> Result<Vec<u8>, Thrown> {
    let inner = digest_of(req, algo, &[key, data])?;
    digest_of(req, algo, &[&outer(key), &inner])
}

/// `CHECK_NULL_PATH` naming `position`, then
/// `php_stream_open_wrapper_ex(path, "rb", REPORT_ERRORS)`: `None` after
/// the warning the open left.
fn open(req: &mut Request, path: &[u8], position: u32) -> Result<Option<Owner<Stream>>, Thrown> {
    if path.contains(&0) {
        return Err(arg_value_error(req, position, "not contain any null bytes"));
    }
    req.open_stream(path, b"rb", false)
}

/// The loop of 1024-byte `php_stream_read`s into `state`, and the
/// close: `false` for the read that failed, php's `n < 0`.
fn pump(req: &mut Request, stream: Owner<Stream>, state: &mut dyn State) -> bool {
    // php's loop reads into one `char buf[1024]` on its stack.
    let mut buf = [0u8; 1024];
    let read = loop {
        match stream.read_into(req, &mut buf) {
            Some(0) => break true,
            Some(n) => state.update(&buf[..n]),
            None => break false,
        }
    };
    stream.free(req);
    read
}

// ------------------------------------------------------------ natives

/// `hash(string $algo, string $data, bool $binary = false, array $options = []): string`.
///
/// ```php
/// var_dump(hash('md5', 'abc'), bin2hex(hash('sha256', 'abc', true)), hash('SHA1', ''), hash('ripemd160', 'abc', false, ['seed' => 5]));
/// try { hash('nope', 'x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { hash('md5'); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(32) "900150983cd24fb0d6963f7d28e17f72"
/// string(64) "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
/// string(40) "da39a3ee5e6b4b0d3255bfef95601890afd80709"
/// string(40) "8eb208f7e05d987a9b044a8e98c6b087f15a0bfc"
/// hash(): Argument #1 ($algo) must be a valid hashing algorithm
/// hash() expects at least 2 arguments, 1 given
/// ```
///
/// The state `php_hash_alloc_context` allocates stands while the data is
/// hashed and the digest's string is laid down, then the state goes
/// and the hex string is made of the digest:
///
/// ```php
/// memory_reset_peak_usage(); $m = memory_get_usage(); $h = hash('md5', 'abc');
/// var_dump(memory_get_peak_usage() - $m, memory_get_usage() - $m);
/// memory_reset_peak_usage(); $m = memory_get_usage(); $b = hash('sha256', 'abc', true);
/// var_dump(memory_get_peak_usage() - $m, memory_get_usage() - $m);
/// ```
/// ```expect
/// int(208)
/// int(64)
/// int(176)
/// int(64)
/// ```
#[php_function]
fn hash(
    req: &mut Request,
    algo: PhpString,
    data: PhpString,
    #[php(default = false)] binary: bool,
    #[php(default = [])] options: PhpArray,
) -> Result<Owner<PhpString>, Thrown> {
    let algo = fetch(req, algo.as_bytes(), false)?;
    let held = context(req, algo)?;
    let mut state = match (algo.init)(req, Options(Some(options))) {
        Ok(state) => state,
        Err(thrown) => {
            held.free(req);
            return Err(thrown);
        }
    };
    state.update(data.as_bytes());
    let digest = state.finish();
    rendered(req, None, [Some(held), None], &digest, binary)
}

/// `hash_file(string $algo, string $filename, bool $binary = false, array $options = []): string|false`.
///
/// ```php
/// $p = __DIR__ . '/hash_file.txt'; file_put_contents($p, str_repeat('abc', 1000));
/// var_dump(hash_file('sha256', $p), hash_file('md5', $p) === md5_file($p), hash_file('md5', __DIR__ . '/nope'));
/// try { hash_file('md5', "a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// unlink($p);
/// ```
/// ```expect
///
/// Warning: hash_file(%s/nope): Failed to open stream: No such file or directory in %s on line 3
/// string(64) "328de8f1895f8bb09f6e6b4c2012ef2b2a6f067cd002794b750aa040a6f6d8bd"
/// bool(true)
/// bool(false)
/// hash_file(): Argument #1 ($algo) must not contain any null bytes
/// ```
///
/// Each read is given back once the state has it, as php reads into one
/// `char buf[1024]`, so hashing a 2 MiB file raises the peak by a
/// stream's buffers and no more:
///
/// ```php
/// $p = __DIR__ . '/peak.bin'; file_put_contents($p, str_repeat('x', 2 << 20));
/// memory_reset_peak_usage(); $m = memory_get_usage();
/// hash_file('md5', $p);
/// $rise = memory_get_peak_usage() - $m;
/// unlink($p);
/// var_dump($rise < 16384);
/// ```
/// ```expect
/// bool(true)
/// ```
///
/// With the stream php opens, the peak is its allocations and the
/// state's:
///
/// ```php
/// chdir(__DIR__); file_put_contents('peak.bin', str_repeat('x', 2 << 20)); hash_file('md5', 'peak.bin');
/// memory_reset_peak_usage(); $m = memory_get_usage(); hash_file('md5', 'peak.bin');
/// var_dump(memory_get_peak_usage() - $m);
/// unlink('peak.bin');
/// ```
/// ```expect
/// int(8848)
/// ```
#[php_function]
fn hash_file(
    req: &mut Request,
    algo: PhpString,
    filename: PhpString,
    #[php(default = false)] binary: bool,
    #[php(default = [])] options: PhpArray,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let algo = fetch(req, algo.as_bytes(), false)?;
    let Some(stream) = open(req, filename.as_bytes(), 1)? else {
        return Ok(OrFalse::False);
    };
    let held = match context(req, algo) {
        Ok(held) => held,
        Err(thrown) => {
            stream.free(req);
            return Err(thrown);
        }
    };
    let mut state = match (algo.init)(req, Options(Some(options))) {
        Ok(state) => state,
        Err(thrown) => {
            stream.free(req);
            held.free(req);
            return Err(thrown);
        }
    };
    if !pump(req, stream, state.as_mut()) {
        held.free(req);
        return Ok(OrFalse::False);
    }
    let digest = state.finish();
    rendered(req, None, [Some(held), None], &digest, binary).map(OrFalse::Value)
}

/// `hash_hmac(string $algo, string $data, string $key, bool $binary = false): string`.
///
/// ```php
/// var_dump(hash_hmac('sha256', 'data', 'key'), hash_hmac('md5', 'data', str_repeat('k', 100)));
/// try { hash_hmac('nope', 'data', 'key'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "5031fe3d989c6d1537a013fa6e739da23463fdaec3b70137d828e36ace221bd0"
/// string(32) "38a034384b6cbf08275f34dc859e6d6b"
/// hash_hmac(): Argument #1 ($algo) must be a valid cryptographic hashing algorithm
/// ```
///
/// ```php
/// echo new ReflectionFunction('hash_hmac');
/// ```
/// ```expect
/// Function [ <internal:hash> function hash_hmac ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> string $algo ]
///     Parameter #1 [ <required> string $data ]
///     Parameter #2 [ <required> string $key ]
///     Parameter #3 [ <optional> bool $binary = false ]
///   }
///   - Return [ string ]
/// }
/// ```
#[php_function]
fn hash_hmac(
    req: &mut Request,
    algo: PhpString,
    data: PhpString,
    #[php(sensitive)] key: PhpString,
    #[php(default = false)] binary: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let algo = fetch(req, algo.as_bytes(), true)?;
    let (held, raw) = hmac_held(req, algo)?;
    let k = hmac_key(req, algo, key.as_bytes())?;
    let digest = hmac(req, algo, &k, data.as_bytes())?;
    rendered(req, Some(raw), held, &digest, binary)
}

/// `php_hash_do_hash_hmac`'s allocations before the key is prepared: the
/// state's allocation, `K = emalloc(ops->block_size)` and the digest's string.
fn hmac_held(
    req: &mut Request,
    algo: &Algo,
) -> Result<([Option<Emalloc>; 2], FreshString), Thrown> {
    let held = context(req, algo)?;
    let k = match Emalloc::new(req, algo.block) {
        Ok(k) => k,
        Err(thrown) => {
            held.free(req);
            return Err(thrown);
        }
    };
    match FreshString::new(req, algo.digest) {
        Ok(raw) => Ok(([Some(held), Some(k)], raw)),
        Err(thrown) => {
            held.free(req);
            k.free(req);
            Err(thrown)
        }
    }
}

/// `hash_hmac_file(string $algo, string $filename, string $key, bool $binary = false): string|false`.
///
/// ```php
/// $p = __DIR__ . '/hmac_file.txt'; file_put_contents($p, 'data');
/// var_dump(hash_hmac_file('sha256', $p, 'key'), hash_hmac_file('md5', __DIR__ . '/nope', 'k'));
/// try { hash_hmac_file('md5', "a\0", 'k'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// unlink($p);
/// ```
/// ```expect
///
/// Warning: hash_hmac_file(%s/nope): Failed to open stream: No such file or directory in %s on line 3
/// string(64) "5031fe3d989c6d1537a013fa6e739da23463fdaec3b70137d828e36ace221bd0"
/// bool(false)
/// hash_hmac_file(): Argument #2 ($filename) must not contain any null bytes
/// ```
///
/// Each read is given back once the state has it, as php reads into one
/// `char buf[1024]`, so hashing a 2 MiB file raises the peak by a
/// stream's buffers and no more:
///
/// ```php
/// $p = __DIR__ . '/peak.bin'; file_put_contents($p, str_repeat('x', 2 << 20));
/// memory_reset_peak_usage(); $m = memory_get_usage();
/// hash_hmac_file('md5', $p, 'k');
/// $rise = memory_get_peak_usage() - $m;
/// unlink($p);
/// var_dump($rise < 16384);
/// ```
/// ```expect
/// bool(true)
/// ```
///
/// The stream, then the state, the key's block and the digest's
/// string, as `php_hash_do_hash_hmac` allocates them:
///
/// ```php
/// chdir(__DIR__); file_put_contents('peak.bin', str_repeat('x', 2 << 20)); hash_hmac_file('md5', 'peak.bin', 'key');
/// memory_reset_peak_usage(); $m = memory_get_usage(); hash_hmac_file('md5', 'peak.bin', 'key');
/// var_dump(memory_get_peak_usage() - $m);
/// unlink('peak.bin');
/// ```
/// ```expect
/// int(8960)
/// ```
#[php_function]
fn hash_hmac_file(
    req: &mut Request,
    algo: PhpString,
    filename: PhpString,
    #[php(sensitive)] key: PhpString,
    #[php(default = false)] binary: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let algo = fetch(req, algo.as_bytes(), true)?;
    let Some(stream) = open(req, filename.as_bytes(), 2)? else {
        return Ok(OrFalse::False);
    };
    let (held, raw) = match hmac_held(req, algo) {
        Ok(made) => made,
        Err(thrown) => {
            stream.free(req);
            return Err(thrown);
        }
    };
    let prepared = hmac_key(req, algo, key.as_bytes())
        .and_then(|k| Ok((algo.init)(req, Options::default())?).map(|state| (k, state)));
    let (k, mut state) = match prepared {
        Ok(prepared) => prepared,
        Err(thrown) => {
            stream.free(req);
            for held in held.into_iter().flatten() {
                held.free(req);
            }
            raw.free(req);
            return Err(thrown);
        }
    };
    state.update(&k);
    if !pump(req, stream, state.as_mut()) {
        for held in held.into_iter().flatten() {
            held.free(req);
        }
        raw.free(req);
        return Ok(OrFalse::False);
    }
    let inner = state.finish();
    let digest = digest_of(req, algo, &[&outer(&k), &inner])?;
    rendered(req, Some(raw), held, &digest, binary).map(OrFalse::Value)
}

/// php's `php_hashcontext_object` beside the object: the algorithm,
/// the running state (`None` once finalised), the options and the
/// HMAC key, `ecalloc(1, ops->block_size)` as php holds it.
#[derive(Default)]
pub(crate) struct ContextState {
    algo: Option<&'static Algo>,
    running: Option<Running>,
    flags: Flags,
    key: Option<Emalloc>,
}

impl ContextState {
    /// `php_hashcontext_dtor`: the state's allocation and the key given
    /// back, the key zeroed first as `ZEND_SECURE_ZERO` zeroes it.
    fn free(&mut self, req: &mut Request) {
        if let Some(running) = self.running.take() {
            running.allocation.free(req);
        }
        if let Some(mut key) = self.key.take() {
            key.bytes_mut().fill(0);
            key.free(req);
        }
    }
}

/// `hash->context`: the algorithm's state, in the allocation
/// `php_hash_alloc_context` makes for it, `ops->context_size` and the
/// alignment php leaves room for, so a `HashContext` counts what php's does
/// (design §5.13).
pub(crate) struct Running {
    state: Box<dyn State>,
    allocation: Emalloc,
}

impl Running {
    /// `php_hash_alloc_context(ops)` for `state`.
    fn new(req: &mut Request, algo: &Algo, state: Box<dyn State>) -> Result<Running, Thrown> {
        Ok(Running {
            state,
            allocation: Emalloc::new(req, algo.context)?,
        })
    }
}

/// The payload: `php_hashcontext_object`'s fields, boxed, inside the `sizeof` php
/// allocates (design §5.13).
pub(crate) type Context = Box<ContextState>;

/// `hash_init(string $algo, int $flags = 0, string $key = "", array $options = []): HashContext`.
///
/// ```php
/// echo new ReflectionFunction('hash_init');
/// ```
/// ```expect
/// Function [ <internal:hash> function hash_init ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> string $algo ]
///     Parameter #1 [ <optional> int $flags = 0 ]
///     Parameter #2 [ <optional> string $key = "" ]
///     Parameter #3 [ <optional> array $options = [] ]
///   }
///   - Return [ HashContext ]
/// }
/// ```
///
/// ```php
/// $c = hash_init('sha1'); hash_update($c, 'ab'); hash_update($c, 'c'); var_dump(hash_final($c));
/// $h = hash_init('md5', HASH_HMAC, str_repeat('key', 30)); hash_update($h, 'data');
/// var_dump(hash_final($h) === hash_hmac('md5', 'data', str_repeat('key', 30)));
/// try { hash_init('md5', HASH_HMAC, ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { hash_init('nope'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(40) "a9993e364706816aba3e25717850c26c9cd0d89d"
/// bool(true)
/// hash_init(): Argument #3 ($key) must not be empty when HMAC is requested
/// hash_init(): Argument #1 ($algo) must be a valid hashing algorithm
/// ```
///
/// A `HashContext` is `php_hashcontext_object`, the state in the
/// allocation `php_hash_alloc_context` makes for the algorithm, and an
/// HMAC key `ecalloc`ed at the block's size; a copy takes a key of its
/// own whether or not HMAC is asked for, and the final gives both back:
///
/// ```php
/// $warm = hash_init('md5');
/// $m = memory_get_usage(); $c = hash_init('md5'); echo 'md5 ', memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $s = hash_init('sha3-512'); echo 'sha3-512 ', memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $x = hash_init('xxh3'); echo 'xxh3 ', memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $h = hash_init('md5', HASH_HMAC, 'k'); echo 'hmac ', memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $d = hash_copy($warm); echo 'copy ', memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); hash_final($d); hash_final($h); echo 'final ', memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// md5 288
/// sha3-512 320
/// xxh3 992
/// hmac 320
/// copy 320
/// final -384
/// ```
///
/// The first `HashContext` of a request costs what every later one does, its
/// omitted `$options` no table:
///
/// ```php
/// $m = memory_get_usage(); $a = hash_init('md5'); $b = memory_get_usage();
/// $c = hash_init('md5');
/// var_dump($b - $m, memory_get_usage() - $b);
/// ```
/// ```expect
/// int(256)
/// int(256)
/// ```
#[php_function]
fn hash_init(
    req: &mut Request,
    algo: PhpString,
    #[php(default = 0)] flags: i64,
    #[php(sensitive, default = "")] key: PhpString,
    #[php(default = [])] options: PhpArray,
) -> Result<Owner<Instance<HashContext>>, Thrown> {
    let flags = Flags(flags);
    let key = key.as_bytes().to_vec();
    let options = Options(Some(options));
    let algo = fetch(req, algo.as_bytes(), false)?;
    if flags.hmac() {
        if !algo.crypto {
            return Err(arg_value_error(
                req,
                1,
                "be a cryptographic hashing algorithm if HMAC is requested",
            ));
        }
        if key.is_empty() {
            return Err(arg_value_error(
                req,
                3,
                "not be empty when HMAC is requested",
            ));
        }
    }
    let mut state = (algo.init)(req, options)?;
    let Some(class) = lookup_class(req, b"HashContext")? else {
        return Err(throw_error(req, "Error", "Class \"HashContext\" not found"));
    };
    let object = new_object(req, class)?;
    let allocation = Emalloc::new(req, algo.context)?;
    if flags.hmac() {
        let mut k = if key.len() > algo.block {
            state.update(&key);
            let reduced = state.finish();
            state = (algo.init)(req, options)?;
            reduced
        } else {
            key
        };
        k.resize(algo.block, 0);
        let mut kept = Emalloc::new(req, algo.block)?;
        for (at, b) in kept.bytes_mut().iter_mut().zip(&k) {
            *at = b ^ 0x36;
        }
        state.update(kept.bytes());
        if let Some(context) = object.native_mut::<Context>(req) {
            context.key = Some(kept);
        }
    }
    if let Some(context) = object.native_mut::<Context>(req) {
        context.algo = Some(algo);
        context.running = Some(Running { state, allocation });
        context.flags = flags;
    }
    Ok(object.into_instance())
}

/// The running state, taken out for the length of a call, and put
/// back.
fn take(req: &mut Request, object: PhpObject) -> Option<Running> {
    object.native_mut::<Context>(req)?.running.take()
}

fn put(req: &mut Request, object: PhpObject, running: Running) {
    if let Some(context) = object.native_mut::<Context>(req) {
        context.running = Some(running);
    }
}

/// `hash_update(HashContext $context, string $data): true`.
///
/// ```php
/// $c = hash_init('sha256'); var_dump(hash_update($c, 'a'), hash_update($c, 'bc'), hash_final($c));
/// try { hash_update($c, 'x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(64) "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
/// hash_update(): Argument #1 ($context) must be a valid, non-finalized HashContext
/// ```
#[php_function]
fn hash_update(
    req: &mut Request,
    context: Instance<HashContext>,
    data: PhpString,
) -> Result<True, Thrown> {
    let object = verify(req, context.object(), 1)?;
    let Some(mut running) = take(req, object) else {
        return Err(finalized(req, 1));
    };
    running.state.update(data.as_bytes());
    put(req, object, running);
    Ok(True)
}

/// `hash_update_stream(HashContext $context, $stream, int $length = -1): int`.
///
/// ```php
/// $f = fopen('php://memory', 'w+'); fwrite($f, str_repeat('xy', 1500)); rewind($f);
/// $c = hash_init('md5'); var_dump(hash_update_stream($c, $f, 10), hash_update_stream($c, $f), hash_final($c) === md5(str_repeat('xy', 1500)));
/// try { hash_update_stream(hash_init('md5'), 'x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(10)
/// int(2990)
/// bool(true)
/// hash_update_stream(): Argument #2 ($stream) must be of type resource, string given
/// ```
///
/// `PHP_Z_PARAM_STREAM`: a resource that is no stream is refused at its
/// position, as the prologue holds it.
///
/// ```php
/// try { hash_update_stream(hash_init('md5'), stream_context_create()); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// hash_update_stream(): Argument #2 ($stream) must be an open stream resource
/// ```
///
/// ```php
/// echo new ReflectionFunction('hash_update_stream');
/// ```
/// ```expect
/// Function [ <internal:hash> function hash_update_stream ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> HashContext $context ]
///     Parameter #1 [ <required> $stream ]
///     Parameter #2 [ <optional> int $length = -1 ]
///   }
///   - Return [ int ]
/// }
/// ```
///
/// Each read is given back once the state has it, as php reads into one
/// `char buf[1024]`, so hashing a 2 MiB file raises the peak by a
/// stream's buffers and no more:
///
/// ```php
/// $p = __DIR__ . '/peak.bin'; file_put_contents($p, str_repeat('x', 2 << 20)); $c = hash_init('md5'); $f = fopen($p, 'rb');
/// memory_reset_peak_usage(); $m = memory_get_usage();
/// hash_update_stream($c, $f);
/// $rise = memory_get_peak_usage() - $m;
/// unlink($p);
/// var_dump($rise < 16384);
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn hash_update_stream(
    req: &mut Request,
    context: Instance<HashContext>,
    stream: Stream,
    #[php(default = -1)] length: i64,
) -> Result<i64, Thrown> {
    // `$length`: a negative one reads to the end, as php's loop does.
    let mut left = usize::try_from(length).ok();
    let object = verify(req, context.object(), 1)?;
    let Some(mut running) = take(req, object) else {
        return Err(finalized(req, 1));
    };
    let mut read = 0usize;
    // php's loop reads into one `char buf[1024]` on its stack.
    let mut buf = [0u8; 1024];
    while left != Some(0) {
        let want = left.map_or(1024, |left| left.min(1024));
        let Some(n @ 1..) = stream.read_into(req, &mut buf[..want]) else {
            break;
        };
        running.state.update(&buf[..n]);
        left = left.map(|left| left.saturating_sub(n));
        read += n;
    }
    put(req, object, running);
    Ok(i64::try_from(read).unwrap_or(i64::MAX))
}

/// `hash_update_file(HashContext $context, string $filename, $stream_context = null): bool`.
///
/// ```php
/// $p = __DIR__ . '/update_file.txt'; file_put_contents($p, 'abc');
/// $c = hash_init('sha1'); var_dump(hash_update_file($c, $p), hash_final($c), hash_update_file(hash_init('md5'), __DIR__ . '/nope'));
/// unlink($p);
/// ```
/// ```expect
///
/// Warning: hash_update_file(%s/nope): Failed to open stream: No such file or directory in %s on line 3
/// bool(true)
/// string(40) "a9993e364706816aba3e25717850c26c9cd0d89d"
/// bool(false)
/// ```
///
/// Each read is given back once the state has it, as php reads into one
/// `char buf[1024]`, so hashing a 2 MiB file raises the peak by a
/// stream's buffers and no more:
///
/// ```php
/// $p = __DIR__ . '/peak.bin'; file_put_contents($p, str_repeat('x', 2 << 20)); $c = hash_init('md5');
/// memory_reset_peak_usage(); $m = memory_get_usage();
/// hash_update_file($c, $p);
/// $rise = memory_get_peak_usage() - $m;
/// unlink($p);
/// var_dump($rise < 16384);
/// ```
/// ```expect
/// bool(true)
/// ```
///
/// No `$stream_context` is `php_stream_context_from_zval(NULL, 0)`, which
/// lays `FG(default_context)` down where the call is the request's first
/// opener, and it stays the request's:
///
/// ```php
/// $c = hash_init('md5'); memory_reset_peak_usage(); $m = memory_get_usage(); hash_update_file($c, '/dev/null'); echo memory_get_peak_usage() - $m, ' ', memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); hash_update_file($c, '/dev/null'); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 8808 152
/// 0
/// ```
///
/// `php_stream_context_from_zval(zcontext, 0)` over a given resource is
/// `zend_fetch_resource_ex`, whose `TypeError` refuses a resource of any
/// type but `stream-context`; php's body does not stop for it, so the
/// file is opened with none and hashed while the `TypeError` is pending,
/// and a file that does not open raises no warning beside it.
///
/// ```php
/// $c = hash_init('md5');
/// try { var_dump(hash_update_file($c, __FILE__, STDIN)); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(hash_final($c) === md5_file(__FILE__));
/// try { var_dump(hash_update_file(hash_init('md5'), __DIR__ . '/nope', STDIN)); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(hash_update_file(hash_init('md5'), __FILE__, stream_context_create()));
/// ```
/// ```expect
/// hash_update_file(): supplied resource is not a valid Stream-Context resource
/// bool(true)
/// hash_update_file(): supplied resource is not a valid Stream-Context resource
/// bool(true)
/// ```
#[php_function]
fn hash_update_file(
    req: &mut Request,
    context: Instance<HashContext>,
    filename: Path,
    #[php(default = null)] stream_context: Option<Resource>,
) -> Result<bool, Thrown> {
    let object = verify(req, context.object(), 1)?;
    // `php_stream_context_from_zval(zcontext, 0)`, after the context is
    // verified as php's body does: one given is the open's, and none
    // lays `FG(default_context)` down where no opener of the request has
    // yet.
    let given = match stream_context.map(|res| StreamCtx::fetch(req, res)) {
        None => {
            StreamCtx::default_ctx(req)?;
            None
        }
        Some(Ok(context)) => Some(context),
        Some(Err(refused)) => {
            let path = filename.0.as_bytes();
            let carried =
                while_pending(req, &refused, |req| hash_file_into(req, object, path, None));
            if let Some(Err(fatal)) = carried {
                return Err(fatal);
            }
            return Err(refused);
        }
    };
    hash_file_into(req, object, filename.0.as_bytes(), given)
}

/// `hash_update_file`'s open and its read loop under the `stream-context`
/// resource the body settled on, `None` where php's is `NULL` or none was
/// given.
fn hash_file_into(
    req: &mut Request,
    object: PhpObject,
    filename: &[u8],
    stream_context: Option<StreamCtx>,
) -> Result<bool, Thrown> {
    let opened = match stream_context {
        Some(ctx) => req.with_stream_ctx(Some(ctx), |req| req.open_stream(filename, b"rb", false)),
        None => req.open_stream(filename, b"rb", false),
    };
    let Some(stream) = opened? else {
        return Ok(false);
    };
    let Some(mut running) = take(req, object) else {
        stream.free(req);
        return Err(finalized(req, 1));
    };
    let read = pump(req, stream, running.state.as_mut());
    put(req, object, running);
    Ok(read)
}

/// `hash_final(HashContext $context, bool $binary = false): string`:
/// the state is spent, and an HMAC's outer round taken over its digest.
///
/// ```php
/// $c = hash_init('sha256', HASH_HMAC, 'key'); hash_update($c, 'data');
/// var_dump(hash_final($c), bin2hex(hash_final(hash_init('md4'), true)));
/// try { hash_final($c); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "5031fe3d989c6d1537a013fa6e739da23463fdaec3b70137d828e36ace221bd0"
/// string(32) "31d6cfe0d16ae931b73c59d7e0c089c0"
/// hash_final(): Argument #1 ($context) must be a valid, non-finalized HashContext
/// ```
#[php_function]
fn hash_final(
    req: &mut Request,
    context: Instance<HashContext>,
    #[php(default = false)] binary: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let object = verify(req, context.object(), 1)?;
    // The key is php's to give back here only under HMAC; a copy's
    // key without it stays with the object until it is freed.
    let taken = object.native_mut::<Context>(req).map(|c| {
        let key = if c.flags.hmac() { c.key.take() } else { None };
        (c.algo, c.running.take(), key)
    });
    let Some((Some(algo), Some(mut running), key)) = taken else {
        return Err(finalized(req, 1));
    };
    let mut digest = running.state.finish();
    running.allocation.free(req);
    if let Some(mut key) = key {
        digest = digest_of(req, algo, &[&outer(key.bytes()), &digest])?;
        key.bytes_mut().fill(0);
        key.free(req);
    }
    digest_value(req, &digest, binary)
}

/// `php_hashcontext_clone`: a fresh object with the members copied and
/// the state forked into an allocation of its own, and a key `ecalloc`ed
/// whether or not the original holds one, as php's clone does, the
/// original's copied in where it does; a finalised state is php's
/// `ValueError`, refused once the copy is made, which stands across it.
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
/// pk(function () { $c = hash_init('md5'); hash_final($c); try { $d = clone $c; } catch (ValueError $e) {} });
/// ```
/// ```expect
/// 1976 32
/// ```
fn clone_context(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let forked = object.native::<Context>(req).map(|c| {
        (
            c.algo,
            c.running.as_ref().map(|r| r.state.fork()),
            c.flags,
            c.key.as_ref().map(|key| key.bytes().to_vec()),
        )
    });
    // `php_hashcontext_create` before the state is read, so the copy
    // stands across the refusal and is released after it, as the
    // `CLONE` handler frees the object php's clone returns.
    let copy = new_object(req, object.class())?;
    let Some((Some(algo), Some(state), flags, key)) = forked else {
        let thrown = throw(req, "ValueError", "Cannot clone a finalized HashContext");
        copy.give_up(req);
        return Err(thrown);
    };
    let filled = (|| {
        for (key, value) in object.props(req)? {
            match key {
                Key::Str(name) => {
                    let value = Owner::take(req, value);
                    let name = Owner::take(req, name);
                    copy.set_prop(req, name, value)?;
                }
                Key::Int(_) => {
                    let value = Owner::take(req, value);
                    copy.set_row(req, &key, value)?;
                }
            }
        }
        let running = Running::new(req, algo, state)?;
        let mut kept = Emalloc::new(req, algo.block)?;
        if let Some(key) = key {
            kept.bytes_mut().copy_from_slice(&key);
        }
        if let Some(context) = copy.native_mut::<Context>(req) {
            context.algo = Some(algo);
            context.running = Some(running);
            context.flags = flags;
            context.key = Some(kept);
        }
        Ok(())
    })();
    // The `clone_obj` slot's return, the one owner the copy was made
    // with, given up where the copy could not be filled.
    if let Err(thrown) = filled {
        copy.give_up(req);
        return Err(thrown);
    }
    Ok(copy)
}

/// `hash_copy(HashContext $context): HashContext`.
///
/// ```php
/// $c = hash_init('md5'); hash_update($c, 'ab'); $d = hash_copy($c); hash_update($d, 'c');
/// var_dump(hash_final($c) === md5('ab'), hash_final($d) === md5('abc'));
/// $h = hash_init('sha1', HASH_HMAC, 'k'); $i = clone $h; var_dump(hash_final($h) === hash_final($i));
/// $e = clone $d;
/// ```
/// ```expect exit=255
/// bool(true)
/// bool(true)
/// bool(true)
///
/// Fatal error: Uncaught ValueError: Cannot clone a finalized HashContext in %s:5
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 5
/// ```
#[php_function]
fn hash_copy(
    req: &mut Request,
    context: Instance<HashContext>,
) -> Result<Owner<Instance<HashContext>>, Thrown> {
    let object = verify(req, context.object(), 1)?;
    // The slot's copy is the one owner it was made with.
    Ok(clone_context(req, object)?.into_instance())
}

/// The table's names, all or the cryptographic ones.
fn names(req: &mut Request, crypto: bool) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for algo in ALGOS.iter().filter(|a| !crypto || a.crypto) {
        let name = PhpString::new(req, algo.name.as_bytes())?;
        out.append(req, name.into_value())?;
    }
    Ok(out)
}

/// `hash_algos(): array`, the table's names in its order.
///
/// ```php
/// echo count(hash_algos()), ' ', implode(' ', hash_algos()), "\n";
/// ```
/// ```expect
/// 60 md2 md4 md5 sha1 sha224 sha256 sha384 sha512/224 sha512/256 sha512 sha3-224 sha3-256 sha3-384 sha3-512 ripemd128 ripemd160 ripemd256 ripemd320 whirlpool tiger128,3 tiger160,3 tiger192,3 tiger128,4 tiger160,4 tiger192,4 snefru snefru256 gost gost-crypto adler32 crc32 crc32b crc32c fnv132 fnv1a32 fnv164 fnv1a64 joaat murmur3a murmur3c murmur3f xxh32 xxh64 xxh3 xxh128 haval128,3 haval160,3 haval192,3 haval224,3 haval256,3 haval128,4 haval160,4 haval192,4 haval224,4 haval256,4 haval128,5 haval160,5 haval192,5 haval224,5 haval256,5
/// ```
#[php_function]
fn hash_algos(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    names(req, false)
}

/// `hash_hmac_algos(): array`, the cryptographic ones.
///
/// ```php
/// $h = hash_hmac_algos(); echo count($h), ' ', implode(' ', array_slice($h, 20)), ' ', in_array('crc32', $h) ? 'crc32' : '-', "\n";
/// ```
/// ```expect
/// 44 tiger160,3 tiger192,3 tiger128,4 tiger160,4 tiger192,4 snefru snefru256 gost gost-crypto haval128,3 haval160,3 haval192,3 haval224,3 haval256,3 haval128,4 haval160,4 haval192,4 haval224,4 haval256,4 haval128,5 haval160,5 haval192,5 haval224,5 haval256,5 -
/// ```
#[php_function]
fn hash_hmac_algos(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    names(req, true)
}

/// `hash_pbkdf2(string $algo, string $password, string $salt, int $iterations, int $length = 0, bool $binary = false, array $options = []): string`.
/// The lengths are php's single-precision `ceil`s.
///
/// ```php
/// var_dump(hash_pbkdf2('sha256', 'password', 'salt', 1000), hash_pbkdf2('sha1', 'password', 'salt', 1, 20), bin2hex(hash_pbkdf2('md5', 'p', 's', 2, 5, true)), hash_pbkdf2('sha1', 'p', 's', 3, 7));
/// try { hash_pbkdf2('md5', 'p', 's', 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { hash_pbkdf2('md5', 'p', 's', 1, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(64) "632c2812e46d4604102ba7618e9d6d7d2f8128f6266b4a03264d2a0460b7dcb3"
/// string(20) "0c60c80f961f0e71f3a9"
/// string(10) "c9641e374b"
/// string(7) "00e3ddf"
/// hash_pbkdf2(): Argument #4 ($iterations) must be greater than 0
/// hash_pbkdf2(): Argument #5 ($length) must be greater than or equal to 0
/// ```
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
#[php_function]
fn hash_pbkdf2(
    req: &mut Request,
    algo: PhpString,
    #[php(sensitive)] password: PhpString,
    salt: PhpString,
    iterations: i64,
    #[php(default = 0)] length: i64,
    #[php(default = false)] binary: bool,
    #[php(default = [])] options: PhpArray,
) -> Result<Owner<PhpString>, Thrown> {
    let mut length = length;
    let (password, salt) = (password.as_bytes(), salt.as_bytes().to_vec());
    let algo = fetch(req, algo.as_bytes(), true)?;
    if iterations <= 0 {
        return Err(arg_value_error(req, 4, "be greater than 0"));
    }
    if length < 0 {
        return Err(arg_value_error(req, 5, "be greater than or equal to 0"));
    }
    // `ops->hash_init(context, args)`: what the options may raise, raised.
    drop((algo.init)(req, Options(Some(options)))?);
    let k1 = hmac_key(req, algo, password)?;
    let size = algo.digest;
    if length == 0 {
        length = size as i64 * if binary { 1 } else { 2 };
    }
    let digest_length = if binary {
        length
    } else {
        ((length as f32) / 2.0).ceil() as i64
    };
    let loops = ((digest_length as f32) / (size as f32)).ceil() as i64;
    let mut result = Vec::new();
    let mut salted = salt.clone();
    for i in 1..=loops {
        salted.truncate(salt.len());
        salted.extend_from_slice(&(i as u32).to_be_bytes());
        let mut digest = hmac(req, algo, &k1, &salted)?;
        let mut temp = digest.clone();
        for _ in 1..iterations {
            digest = hmac(req, algo, &k1, &digest)?;
            for (t, d) in temp.iter_mut().zip(&digest) {
                *t ^= d;
            }
        }
        result.extend_from_slice(&temp);
    }
    let mut bytes = if binary {
        result
    } else {
        result.truncate(digest_length as usize);
        hex(&result)
    };
    bytes.truncate(length as usize);
    PhpString::new(req, &bytes)
}

/// `hash_equals(string $known_string, string $user_string): bool`: two
/// strings and nothing else, compared in time their length alone sets.
///
/// ```php
/// var_dump(hash_equals('abc', 'abc'), hash_equals('abc', 'abd'), hash_equals('abc', 'ab'));
/// try { hash_equals(1, 'x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { hash_equals('x', null); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// hash_equals(): Argument #1 ($known_string) must be of type string, int given
/// hash_equals(): Argument #2 ($user_string) must be of type string, null given
/// ```
#[php_function]
fn hash_equals(
    req: &mut Request,
    #[php(sensitive)] known_string: Raw<PhpString>,
    #[php(sensitive)] user_string: Raw<PhpString>,
) -> Result<bool, Thrown> {
    let mut strings = Vec::new();
    for (position, given) in [(1, known_string.value()), (2, user_string.value())] {
        let Some(bytes) = given.as_str_bytes() else {
            let name = value_name(req, &given);
            return Err(arg_type_error(
                req,
                position,
                &format!("be of type string, {name} given"),
            ));
        };
        strings.push(bytes.to_vec());
    }
    let (known, user) = (&strings[0], &strings[1]);
    let same = known.len() == user.len()
        && known
            .iter()
            .zip(user)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
    Ok(same)
}

/// `hash_hkdf(string $algo, string $key, int $length = 0, string $info = "", string $salt = ""): string`,
/// RFC 5869.
///
/// ```php
/// var_dump(bin2hex(hash_hkdf('sha256', 'key')), bin2hex(hash_hkdf('sha1', 'key', 50, 'info', 'salt')));
/// foreach ([['md5', '', 0], ['md5', 'k', -1], ['md5', 'k', 5000], ['nope', 'k', 0]] as $c) {
///     try { hash_hkdf($c[0], $c[1], $c[2]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// string(64) "80c724c4a65d79568fc3606753c192adcc24bc263286767d4410269d8093c372"
/// string(100) "a0bc90fe7a3fef57087f96ceeaccea0241f6e00a3fe35a789ced78dfbc7c95f5fb56cf6b88b72a532bbb9d2a257983e122a0"
/// hash_hkdf(): Argument #2 ($key) must not be empty
/// hash_hkdf(): Argument #3 ($length) must be greater than or equal to 0
/// hash_hkdf(): Argument #3 ($length) must be less than or equal to 4080
/// hash_hkdf(): Argument #1 ($algo) must be a valid cryptographic hashing algorithm
/// ```
#[php_function]
fn hash_hkdf(
    req: &mut Request,
    algo: PhpString,
    #[php(sensitive)] key: PhpString,
    #[php(default = 0)] length: i64,
    #[php(default = "")] info: PhpString,
    #[php(default = "")] salt: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    let (ikm, info, salt) = (key.as_bytes(), info.as_bytes(), salt.as_bytes());
    let algo = fetch(req, algo.as_bytes(), true)?;
    if ikm.is_empty() {
        return Err(arg_value_error(req, 2, "not be empty"));
    }
    let size = algo.digest;
    let max = i64::try_from(size * 255).unwrap_or(i64::MAX);
    let length = match length {
        l if l < 0 => {
            return Err(arg_value_error(req, 3, "be greater than or equal to 0"));
        }
        0 => size,
        l if l > max => {
            return Err(arg_value_error(
                req,
                3,
                &format!("be less than or equal to {max}"),
            ));
        }
        l => usize::try_from(l).unwrap_or(size),
    };
    let k = hmac_key(req, algo, salt)?;
    let prk = hmac(req, algo, &k, ikm)?;
    let k = hmac_key(req, algo, &prk)?;
    let mut out = Vec::with_capacity(length);
    let mut digest: Vec<u8> = Vec::new();
    let mut i = 1u8;
    while out.len() < length {
        let inner = digest_of(req, algo, &[&k, &digest, info, &[i]])?;
        digest = digest_of(req, algo, &[&outer(&k), &inner])?;
        let take = (length - out.len()).min(size);
        out.extend_from_slice(&digest[..take]);
        i = i.wrapping_add(1);
    }
    PhpString::new(req, &out)
}

/// An mhash number's row, `None` out of range or at one of the gaps.
fn mhash_entry(number: i64) -> Option<(&'static str, &'static str)> {
    usize::try_from(number)
        .ok()
        .and_then(|n| MHASH.get(n).copied().flatten())
}

/// `mhash(int $algo, string $data, ?string $key = null): string|false`:
/// `hash()` or `hash_hmac()`, raw.
///
/// ```php
/// echo new ReflectionFunction('mhash');
/// ```
/// ```expect
/// Function [ <internal, deprecated:hash> function mhash ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> int $algo ]
///     Parameter #1 [ <required> string $data ]
///     Parameter #2 [ <optional> ?string $key = null ]
///   }
///   - Return [ string|false ]
/// }
/// ```
///
/// ```php
/// var_dump(bin2hex(mhash(1, 'abc')), bin2hex(mhash(2, 'data', 'key')), mhash(4, 'x'));
/// ```
/// ```expect
///
/// Deprecated: Function mhash() is deprecated since 8.1 in %s on line 2
///
/// Deprecated: Function mhash() is deprecated since 8.1 in %s on line 2
///
/// Deprecated: Function mhash() is deprecated since 8.1 in %s on line 2
/// string(32) "900150983cd24fb0d6963f7d28e17f72"
/// string(40) "104152c5bfdca07bc633eebd46199f0255c9f49d"
/// bool(false)
/// ```
#[php_function(deprecated(since = "8.1"))]
fn mhash(
    req: &mut Request,
    algo: i64,
    data: PhpString,
    #[php(default = null)] key: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Some((_, name)) = mhash_entry(algo) else {
        return Ok(OrFalse::False);
    };
    let digest = if let Some(key) = key {
        let algo = fetch(req, name.as_bytes(), true)?;
        let k = hmac_key(req, algo, key.as_bytes())?;
        hmac(req, algo, &k, data.as_bytes())?
    } else {
        let algo = fetch(req, name.as_bytes(), false)?;
        digest_of(req, algo, &[data.as_bytes()])?
    };
    digest_value(req, &digest, true).map(OrFalse::Value)
}

/// `mhash_get_hash_name(int $algo): string|false`.
///
/// ```php
/// var_dump(@mhash_get_hash_name(7), @mhash_get_hash_name(26), @mhash_get_hash_name(99));
/// ```
/// ```expect
/// string(5) "TIGER"
/// bool(false)
/// bool(false)
/// ```
#[php_function(deprecated(since = "8.1"))]
fn mhash_get_hash_name(req: &mut Request, algo: i64) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    match mhash_entry(algo) {
        Some((name, _)) => PhpString::new(req, name.as_bytes()).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `mhash_count(): int`, the highest number.
///
/// ```php
/// var_dump(@mhash_count());
/// ```
/// ```expect
/// int(41)
/// ```
#[php_function(deprecated(since = "8.1"))]
fn mhash_count() -> i64 {
    41
}

/// `mhash_get_block_size(int $algo): int|false`, which answers the
/// digest's size.
///
/// ```php
/// var_dump(@mhash_get_block_size(20), @mhash_get_block_size(1), @mhash_get_block_size(4));
/// ```
/// ```expect
/// int(64)
/// int(16)
/// bool(false)
/// ```
#[php_function(deprecated(since = "8.1"))]
fn mhash_get_block_size(algo: i64) -> OrFalse<i64> {
    let size = mhash_entry(algo)
        .and_then(|(_, name)| algos::fetch(name.as_bytes()))
        .map(|a| a.digest);
    size.map_or(OrFalse::False, |s| {
        OrFalse::Value(i64::try_from(s).unwrap_or(0))
    })
}

/// `mhash_keygen_s2k(int $algo, string $password, string $salt, int $length): string|false`:
/// the salt cut or padded to eight bytes, and digests of it and the
/// password behind a growing row of zeros, concatenated.
///
/// ```php
/// var_dump(bin2hex(@mhash_keygen_s2k(1, 'password', 'saltsaltsalt', 20)), @mhash_keygen_s2k(4, 'p', 's', 4));
/// try { @mhash_keygen_s2k(1, 'p', 's', 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(40) "ae7cf6d3a33a2117fa4f008d66f6d26f4958de2d"
/// bool(false)
/// mhash_keygen_s2k(): Argument #4 ($length) must be a greater than 0
/// ```
#[php_function(deprecated(since = "8.1"))]
fn mhash_keygen_s2k(
    req: &mut Request,
    algo: i64,
    password: PhpString,
    salt: PhpString,
    length: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let (number, password, salt) = (algo, password.as_bytes(), salt.as_bytes());
    #[allow(clippy::cast_possible_truncation)]
    let bytes = length as i32;
    if bytes <= 0 {
        return Err(arg_value_error(req, 4, "be a greater than 0"));
    }
    let mut padded = [0u8; 8];
    let used = salt.len().min(8);
    padded[..used].copy_from_slice(&salt[..used]);
    let Some(algo) = mhash_entry(number).and_then(|(_, name)| algos::fetch(name.as_bytes())) else {
        return Ok(OrFalse::False);
    };
    let bytes = usize::try_from(bytes).unwrap_or(0);
    let mut key = Vec::new();
    let mut zeros = Vec::new();
    while key.len() < bytes {
        key.extend(digest_of(req, algo, &[&zeros, &padded, password])?);
        zeros.push(0);
    }
    key.truncate(bytes);
    PhpString::new(req, &key).map(OrFalse::Value)
}

/// `Z_TYPE_P(zv) == IS_LONG`.
fn int_of(value: Option<Value>) -> Option<i64> {
    match value?.as_ref() {
        ValueRef::Int(n) => Some(n),
        _ => None,
    }
}

// ------------------------------------------------------------ HashContext

/// `final class HashContext`.
///
/// ```php
/// $r = new ReflectionClass('HashContext');
/// var_dump($r->isFinal(), $r->isInstantiable());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_class(final, size = 88, payload = Context, handlers = HANDLERS)]
pub(crate) struct HashContext;

/// `php_hashcontext_handlers`: `clone_obj` is `php_hashcontext_clone`,
/// and `free_obj` `php_hashcontext_free`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

impl ObjectHandlers for Handlers {
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_context)
    }

    /// `php_hashcontext_free`: the state's allocation and the key given
    /// back.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let context = object
            .native_mut::<Context>(req)
            .map(|context| std::mem::take(&mut **context));
        if let Some(mut context) = context {
            context.free(req);
        }
    }
}

/// The stub's four methods, in its order, and `__construct`, which a
/// program reaches only through Reflection, since it is private.
///
/// ```php
/// echo new ReflectionClass('HashContext');
/// ```
/// ```expect
/// Class [ <internal:hash> final class HashContext ] {
///
///   - Constants [0] {
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [0] {
///   }
///
///   - Properties [0] {
///   }
///
///   - Methods [4] {
///     Method [ <internal:hash, ctor> private method __construct ] {
///
///       - Parameters [0] {
///       }
///     }
///
///     Method [ <internal:hash> public method __serialize ] {
///
///       - Parameters [0] {
///       }
///       - Return [ array ]
///     }
///
///     Method [ <internal:hash> public method __unserialize ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> array $data ]
///       }
///       - Return [ void ]
///     }
///
///     Method [ <internal:hash> public method __debugInfo ] {
///
///       - Parameters [0] {
///       }
///       - Return [ array ]
///     }
///   }
/// }
/// ```
///
/// ```php
/// try { (new ReflectionMethod('HashContext', '__construct'))->invoke(hash_init('md5')); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Illegal call to private/final constructor
/// ```
#[php_class]
impl HashContext {
    /// `HashContext::__construct`, which a program cannot reach but
    /// through Reflection, since it is private.
    #[php(private)]
    fn __construct(req: &mut Request, _this: This<Self>) -> Result<(), Thrown> {
        Err(throw(
            req,
            "Error",
            "Illegal call to private/final constructor",
        ))
    }

    /// `HashContext::__serialize(): array`: algorithm, options, the
    /// state as its spec writes it, the magic, and the members.
    ///
    /// ```php
    /// $c = hash_init('md5'); hash_update($c, "I can't remember anything");
    /// echo base64_encode(serialize($c)), "\n";
    /// $d = unserialize(serialize($c)); hash_update($d, 'x'); var_dump(hash_final($d) === md5("I can't remember anythingx"));
    /// try { serialize(hash_init('md5', HASH_HMAC, 'k')); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// TzoxMToiSGFzaENvbnRleHQiOjU6e2k6MDtzOjM6Im1kNSI7aToxO2k6MDtpOjI7YToyMzp7aTowO2k6MjU7aToxO2k6MDtpOjI7aToxNzMyNTg0MTkzO2k6MztpOi0yNzE3MzM4Nzk7aTo0O2k6LTE3MzI1ODQxOTQ7aTo1O2k6MjcxNzMzODc4O2k6NjtzOjY0OiJJIGNhbid0IHJlbWVtYmVyIGFueXRoaW5nAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAIjtpOjc7aTowO2k6ODtpOjA7aTo5O2k6MDtpOjEwO2k6MDtpOjExO2k6MDtpOjEyO2k6MDtpOjEzO2k6MDtpOjE0O2k6MDtpOjE1O2k6MDtpOjE2O2k6MDtpOjE3O2k6MDtpOjE4O2k6MDtpOjE5O2k6MDtpOjIwO2k6MDtpOjIxO2k6MDtpOjIyO2k6MDt9aTozO2k6MjtpOjQ7YTowOnt9fQ==
    /// bool(true)
    /// HashContext with HASH_HMAC option cannot be serialized
    /// ```
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let this = this.object();
        let (algo, flags) = this
            .native::<Context>(req)
            .map_or((None, Flags::default()), |c| (c.algo, c.flags));
        let name = algo.map_or("", |a| a.label);
        let cannot = |req: &mut Request| {
            throw(
                req,
                "Exception",
                &format!("HashContext for algorithm \"{name}\" cannot be serialized"),
            )
        };
        let Some(magic) = algo.and_then(|a| a.magic) else {
            return Err(cannot(req));
        };
        if flags.hmac() {
            return Err(throw(
                req,
                "Exception",
                "HashContext with HASH_HMAC option cannot be serialized",
            ));
        }
        let Some(mut running) = take(req, this) else {
            return Err(cannot(req));
        };
        let fields = algos::serialize(req, running.state.as_mut());
        put(req, this, running);
        let fields = fields?;
        let mut out = PhpArray::new(req)?;
        let algo_name = PhpString::new(req, name.as_bytes())?;
        out.append(req, algo_name.into_value())?;
        out.append(req, Owner::int(flags.0))?;
        out.append(req, fields.into_value())?;
        out.append(req, Owner::int(magic))?;
        let mut members = PhpArray::new(req)?;
        for (key, value) in this.props(req)? {
            let value = Owner::take(req, value);
            let key = match key {
                Key::Str(name) => Owner::take(req, Key::lookup(name)),
                Key::Int(_) => Owner::take(req, key),
            };
            members.set(req, key, value)?;
        }
        out.append(req, members.into_value())?;
        Ok(out)
    }

    /// `HashContext::__unserialize(array $data): void`.
    ///
    /// ```php
    /// $c = hash_init('sha256');
    /// try { $c->__unserialize($c->__serialize()); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// foreach (['O:11:"HashContext":0:{}', 'O:11:"HashContext":5:{i:0;s:3:"xyz";i:1;i:0;i:2;a:0:{}i:3;i:2;i:4;a:0:{}}',
    ///           'O:11:"HashContext":5:{i:0;s:4:"sha1";i:1;i:0;i:2;a:1:{i:0;s:1:"x";}i:3;i:2;i:4;a:0:{}}',
    ///           'O:11:"HashContext":5:{i:0;s:3:"md2";i:1;i:0;i:2;a:0:{}i:3;i:1;i:4;a:0:{}}',
    ///           'O:11:"HashContext":5:{i:0;s:3:"md5";i:1;i:1;i:2;a:0:{}i:3;i:2;i:4;a:0:{}}'] as $s) {
    ///     try { unserialize($s); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// HashContext::__unserialize called on initialized object
    /// Incomplete or ill-formed serialization data
    /// Unknown hash algorithm
    /// Incomplete or ill-formed serialization data ("sha1" code -1000)
    /// Incomplete or ill-formed serialization data ("md2" code -1)
    /// HashContext with HASH_HMAC option cannot be serialized
    /// ```
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        let this = this.object();
        if this
            .native::<Context>(req)
            .is_some_and(|c| c.running.is_some())
        {
            return Err(throw(
                req,
                "Exception",
                "HashContext::__unserialize called on initialized object",
            ));
        }
        let at = |n: i64| data.get(&Key::Int(n)).map(|v| v.deref());
        let name = at(0).and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
        let (flags, fields, magic) = (int_of(at(1)).map(Flags), at(2), int_of(at(3)));
        let members = at(4).and_then(|v| v.as_array().copied());
        let (Some(name), Some(flags), Some(fields), Some(magic), Some(members)) =
            (name, flags, fields, magic, members)
        else {
            return Err(throw(
                req,
                "Exception",
                "Incomplete or ill-formed serialization data",
            ));
        };
        if flags.hmac() {
            return Err(throw(
                req,
                "Exception",
                "HashContext with HASH_HMAC option cannot be serialized",
            ));
        }
        let Some(algo) = algos::fetch(&name) else {
            return Err(throw(req, "Exception", "Unknown hash algorithm"));
        };
        let Some(want) = algo.magic else {
            return Err(throw(
                req,
                "Exception",
                &format!("Hash algorithm \"{}\" cannot be unserialized", algo.label),
            ));
        };
        let mut state = (algo.init)(req, Options::default())?;
        let written = if magic == want {
            algos::unserialize(state.as_mut(), &fields)
        } else {
            Err(Refusal::Malformed)
        };
        if let Some(context) = this.native_mut::<Context>(req) {
            context.algo = Some(algo);
        }
        if let Err(refusal) = written {
            let code = refusal.code();
            return Err(throw(
                req,
                "Exception",
                &format!(
                    "Incomplete or ill-formed serialization data (\"{}\" code {code})",
                    algo.label
                ),
            ));
        }
        let running = Running::new(req, algo, state)?;
        if let Some(context) = this.native_mut::<Context>(req) {
            context.running = Some(running);
            context.flags = flags;
        }
        // `object_properties_load`'s walk (`Z/Zend/zend_API.c:1752`), no
        // owner of the table taken: the argument holds it.
        walk::foreach(req, members, Hold::None, |req, row| -> walk::Step {
            let key = row.key().clone();
            let value = row.value().deref();
            let name = key.to_prop_name(req)?;
            let value = Owner::take(req, value);
            this.set_prop(req, name, value)?;
            Ok(ControlFlow::Continue(()))
        })?;
        Ok(())
    }

    /// `HashContext::__debugInfo(): array`, `['algo' => ...]`, what
    /// `var_dump` shows.
    ///
    /// ```php
    /// var_dump(hash_init('snefru256'));
    /// ```
    /// ```expect
    /// object(HashContext)#1 (1) {
    ///   ["algo"]=>
    ///   string(6) "snefru"
    /// }
    /// ```
    fn __debugInfo(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let algo = this.object().native::<Context>(req).and_then(|c| c.algo);
        let mut out = PhpArray::new(req)?;
        let name = PhpString::new(req, algo.map_or("", |a| a.label).as_bytes())?;
        out.set_str(req, b"algo", name.into_value())?;
        Ok(out)
    }
}
