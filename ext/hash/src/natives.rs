//! `hash.c`: the natives, `HashContext` and the `mhash*` compatibility
//! layer, with `php_hashcontext_object` as the class's payload.
//
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{CloneObj, ObjectHandlers};
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::{coerce_param, value_name};
use php_ext::decl::{
    Attribute, AttributeArgument, ClassDecl, ClassFlags, ClassKind, ConstDecl, ConstExpr,
    EnumBacking, FunctionDecl, MethodDecl, Name, Type, Visibility,
};
use php_ext::raising::{self, Thrown, arg_count_error, arg_type_error, arg_value_error, throw};
use php_ext::request::{Request, Stream};
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::algos::{self, ALGOS, Algo, Refusal, State};
use crate::stub::stub;

/// A native's entry.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// `hash_init`'s `$flags`, php's `context->options`: kept whole, as
/// `__serialize` writes it back as given, and read through [`Flags::hmac`].
#[derive(Debug, Default, Clone, Copy)]
struct Flags(i64);

impl Flags {
    /// `PHP_HASH_HMAC`.
    const HMAC: i64 = 1;

    /// `options & PHP_HASH_HMAC`.
    fn hmac(self) -> bool {
        self.0 & Flags::HMAC != 0
    }
}

/// The natives in `hash.stub.php`'s order.
///
/// ```php
/// echo new ReflectionFunction('hash_init'), new ReflectionFunction('mhash');
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
#[rustfmt::skip]
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let deprecated_stub = |line: &str| FunctionDecl { attributes: vec![deprecated("8.1", None)], ..stub(line) };
    vec![
        (stub("hash(string $algo, string $data, bool $binary = false, array $options = []): string"), hash as Entry),
        (stub("hash_file(string $algo, string $filename, bool $binary = false, array $options = []): string|false"), hash_file),
        (stub("hash_hmac(string $algo, string $data, #[\\SensitiveParameter] string $key, bool $binary = false): string"), hash_hmac),
        (stub("hash_hmac_file(string $algo, string $filename, #[\\SensitiveParameter] string $key, bool $binary = false): string|false"), hash_hmac_file),
        (stub("hash_init(string $algo, int $flags = 0, #[\\SensitiveParameter] string $key = \"\", array $options = []): HashContext"), hash_init),
        (stub("hash_update(HashContext $context, string $data): true"), hash_update),
        (stub("hash_update_stream(HashContext $context, $stream, int $length = -1): int"), hash_update_stream),
        (stub("hash_update_file(HashContext $context, string $filename, $stream_context = null): bool"), hash_update_file),
        (stub("hash_final(HashContext $context, bool $binary = false): string"), hash_final),
        (stub("hash_copy(HashContext $context): HashContext"), hash_copy),
        (stub("hash_algos(): array"), hash_algos),
        (stub("hash_hmac_algos(): array"), hash_hmac_algos),
        (stub("hash_pbkdf2(string $algo, #[\\SensitiveParameter] string $password, string $salt, int $iterations, int $length = 0, bool $binary = false, array $options = []): string"), hash_pbkdf2),
        (stub("hash_equals(#[\\SensitiveParameter] string $known_string, #[\\SensitiveParameter] string $user_string): bool"), hash_equals),
        (stub("hash_hkdf(string $algo, #[\\SensitiveParameter] string $key, int $length = 0, string $info = \"\", string $salt = \"\"): string"), hash_hkdf),
        (deprecated_stub("mhash_get_block_size(int $algo): int|false"), mhash_get_block_size),
        (deprecated_stub("mhash_get_hash_name(int $algo): string|false"), mhash_get_hash_name),
        (deprecated_stub("mhash_keygen_s2k(int $algo, string $password, string $salt, int $length): string|false"), mhash_keygen_s2k),
        (deprecated_stub("mhash_count(): int"), mhash_count),
        (deprecated_stub("mhash(int $algo, string $data, ?string $key = null): string|false"), mhash),
    ]
}

/// php's `#[\Deprecated(since: ...)]`, as the stub marks the `mhash*`
/// natives, and with `message` as `mhash_init` hangs it on each
/// `MHASH_*` constant.
fn deprecated(since: &str, message: Option<&str>) -> Attribute {
    let argument = |name: &str, value: &str| AttributeArgument {
        name: Some(Name(name.as_bytes().into())),
        value: ConstExpr::Str(value.as_bytes().into()),
    };
    let mut arguments = vec![argument("since", since)];
    arguments.extend(message.map(|m| argument("message", m)));
    Attribute {
        name: Name(b"Deprecated".as_slice().into()),
        arguments,
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
pub(crate) fn constants() -> Vec<ConstDecl> {
    let mut out = vec![ConstDecl {
        name: Name(b"HASH_HMAC".as_slice().into()),
        value: ConstExpr::Long(Flags::HMAC),
        attributes: Vec::new(),
    }];
    for (number, entry) in (0i64..).zip(MHASH) {
        if let Some((name, _)) = entry {
            out.push(ConstDecl {
                name: Name(format!("MHASH_{name}").into_bytes().into()),
                value: ConstExpr::Long(number),
                attributes: vec![deprecated(
                    "8.5",
                    Some("as the mhash*() functions were deprecated"),
                )],
            });
        }
    }
    out
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
    req.info_table(&[&["hash support", "enabled"], &["Hashing Engines", &engines]]);
    req.info_table(&[
        &["MHASH support", "Enabled"],
        &["MHASH API Version", "Emulated Support"],
    ]);
}

// ------------------------------------------------------------ arguments

/// `ZEND_PARSE_PARAMETERS_START(min, max)`'s count check.
fn count(req: &mut Request, min: usize, max: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if (min..=max).contains(&given) {
        return Ok(());
    }
    let (word, bound) = if min == max {
        ("exactly", min)
    } else if given < min {
        ("at least", min)
    } else {
        ("at most", max)
    };
    let plural = if bound == 1 { "" } else { "s" };
    Err(arg_count_error(
        req,
        &format!("{word} {bound} argument{plural}, {given} given"),
    ))
}

/// The argument at `position` coerced to `ty`, or `None` past the count.
fn argument(req: &mut Request, position: u32, ty: &Type) -> Result<Option<Value>, Thrown> {
    match req.argument(position) {
        Some(value) if !matches!(value.as_ref(), ValueRef::Undef) => {
            coerce_param(req, &value, ty, position).map(Some)
        }
        _ => Ok(None),
    }
}

/// `Z_PARAM_STR`.
fn string(req: &mut Request, position: u32) -> Result<Vec<u8>, Thrown> {
    let value = argument(req, position, &Type::String)?;
    Ok(value
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
        .unwrap_or_default())
}

/// `Z_PARAM_BOOL`, `false` where the call gave none.
fn boolean(req: &mut Request, position: u32) -> Result<bool, Thrown> {
    let value = argument(req, position, &Type::Bool)?;
    Ok(value.and_then(|v| v.as_bool()).unwrap_or(false))
}

/// `Z_PARAM_LONG`, `default` where the call gave none.
fn long(req: &mut Request, position: u32, default: i64) -> Result<i64, Thrown> {
    let value = argument(req, position, &Type::Int)?;
    Ok(value.and_then(|v| v.as_int()).unwrap_or(default))
}

/// `Z_PARAM_ARRAY_HT`: php's `HashTable *args`, `NULL` where the call
/// gave none.
fn options(req: &mut Request, position: u32) -> Result<Option<PhpArray>, Thrown> {
    let value = argument(req, position, &Type::Array)?;
    Ok(value.and_then(|v| v.as_array().copied()))
}

/// `PHP_HASHCONTEXT_VERIFY`'s `TypeError`.
fn finalized(req: &mut Request, position: u32) -> Thrown {
    arg_type_error(req, position, "be a valid, non-finalized HashContext")
}

/// `Z_PARAM_OBJECT_OF_CLASS(hash_zv, php_hashcontext_ce)`.
fn object(req: &mut Request, position: u32) -> Result<PhpObject, Thrown> {
    let ty = Type::Named(Name(b"HashContext".as_slice().into()));
    let value = argument(req, position, &ty)?.unwrap_or_default();
    match value.as_object().copied() {
        Some(object) => Ok(object),
        None => Err(finalized(req, position)),
    }
}

/// `PHP_HASHCONTEXT_VERIFY`: a state not yet finalised.
fn verify(req: &mut Request, object: PhpObject, position: u32) -> Result<PhpObject, Thrown> {
    if object
        .native::<Context>(req)
        .is_some_and(|c| c.state.is_some())
    {
        Ok(object)
    } else {
        Err(finalized(req, position))
    }
}

/// [`object`] and [`verify`], for the natives with nothing between.
fn context(req: &mut Request, position: u32) -> Result<PhpObject, Thrown> {
    let object = object(req, position)?;
    verify(req, object, position)
}

/// `php_hash_fetch_ops`, and the `ValueError` for a name it does not
/// know, or one HMAC cannot take.
fn algo(req: &mut Request, name: &[u8], crypto: bool) -> Result<&'static Algo, Thrown> {
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
fn digest_value(req: &mut Request, digest: &[u8], binary: bool) -> Result<Value, Thrown> {
    let bytes = if binary { digest.to_vec() } else { hex(digest) };
    Ok(Value::string(PhpString::new(req, &bytes)?))
}

/// `hash_init(context, NULL)`, the pieces, `hash_final`.
fn digest_of(req: &mut Request, algo: &Algo, pieces: &[&[u8]]) -> Result<Vec<u8>, Thrown> {
    let mut state = (algo.init)(req, None)?;
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
fn open(req: &mut Request, path: &[u8], position: u32) -> Result<Option<Stream>, Thrown> {
    if path.contains(&0) {
        return Err(arg_value_error(req, position, "not contain any null bytes"));
    }
    req.open_stream(path, b"rb", false)
}

/// The loop of 1024-byte `php_stream_read`s into `state`, and the
/// close: `false` for the read that failed, php's `n < 0`.
fn pump(req: &mut Request, stream: Stream, state: &mut dyn State) -> Result<bool, Thrown> {
    let read = loop {
        match stream.read(req, 1024) {
            Ok(Some(chunk)) if !chunk.is_empty() => state.update(chunk.as_bytes()),
            Ok(Some(_)) => break Ok(true),
            Ok(None) => break Ok(false),
            Err(thrown) => break Err(thrown),
        }
    };
    stream.close(req);
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
fn hash(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 2, 4)?;
    let name = string(req, 1)?;
    let data = string(req, 2)?;
    let binary = boolean(req, 3)?;
    let options = options(req, 4)?;
    let algo = algo(req, &name, false)?;
    let mut state = (algo.init)(req, options)?;
    state.update(&data);
    let digest = state.finish();
    digest_value(req, &digest, binary)
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
fn hash_file(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 2, 4)?;
    let name = string(req, 1)?;
    let path = string(req, 2)?;
    let binary = boolean(req, 3)?;
    let options = options(req, 4)?;
    let algo = algo(req, &name, false)?;
    let Some(stream) = open(req, &path, 1)? else {
        return Ok(Value::bool(false));
    };
    let mut state = match (algo.init)(req, options) {
        Ok(state) => state,
        Err(thrown) => {
            stream.close(req);
            return Err(thrown);
        }
    };
    if !pump(req, stream, state.as_mut())? {
        return Ok(Value::bool(false));
    }
    let digest = state.finish();
    digest_value(req, &digest, binary)
}

/// `php_hash_do_hash_hmac` for `hash_hmac` and `hash_hmac_file`.
fn do_hmac(req: &mut Request, file: bool) -> Result<Value, Thrown> {
    count(req, 3, 4)?;
    let name = string(req, 1)?;
    let data = string(req, 2)?;
    let key = string(req, 3)?;
    let binary = boolean(req, 4)?;
    let algo = algo(req, &name, true)?;
    let k = if file {
        let Some(stream) = open(req, &data, 2)? else {
            return Ok(Value::bool(false));
        };
        let k = hmac_key(req, algo, &key)?;
        let mut state = (algo.init)(req, None)?;
        state.update(&k);
        if !pump(req, stream, state.as_mut())? {
            return Ok(Value::bool(false));
        }
        let inner = state.finish();
        let digest = digest_of(req, algo, &[&outer(&k), &inner])?;
        return digest_value(req, &digest, binary);
    } else {
        hmac_key(req, algo, &key)?
    };
    let digest = hmac(req, algo, &k, &data)?;
    digest_value(req, &digest, binary)
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
fn hash_hmac(req: &mut Request) -> Result<Value, Thrown> {
    do_hmac(req, false)
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
fn hash_hmac_file(req: &mut Request) -> Result<Value, Thrown> {
    do_hmac(req, true)
}

/// php's `php_hashcontext_object` beside the object: the algorithm,
/// the hash state (`None` once finalised), the options and the
/// HMAC key.
#[derive(Default)]
pub(crate) struct Context {
    algo: Option<&'static Algo>,
    state: Option<Box<dyn State>>,
    flags: Flags,
    key: Option<Vec<u8>>,
}

/// `hash_init(string $algo, int $flags = 0, string $key = "", array $options = []): HashContext`.
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
fn hash_init(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 1, 4)?;
    let name = string(req, 1)?;
    let flags = Flags(long(req, 2, 0)?);
    let key = string(req, 3)?;
    let options = options(req, 4)?;
    let algo = algo(req, &name, false)?;
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
    let mut kept = None;
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
        let k: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
        state.update(&k);
        kept = Some(k);
    }
    let Some(class) = lookup_class(req, b"HashContext")? else {
        return Err(throw(req, "Error", "Class \"HashContext\" not found"));
    };
    let object = new_object(req, class)?;
    if let Some(context) = object.native_mut::<Context>(req) {
        *context = Context {
            algo: Some(algo),
            state: Some(state),
            flags,
            key: kept,
        };
    }
    Ok(object.as_value())
}

/// The hash state, taken out for the length of a call, and put
/// back.
fn take(req: &mut Request, object: PhpObject) -> Option<Box<dyn State>> {
    object.native_mut::<Context>(req)?.state.take()
}

fn put(req: &mut Request, object: PhpObject, state: Box<dyn State>) {
    if let Some(context) = object.native_mut::<Context>(req) {
        context.state = Some(state);
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
fn hash_update(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 2, 2)?;
    let object = context(req, 1)?;
    let data = string(req, 2)?;
    let Some(mut state) = take(req, object) else {
        return Err(finalized(req, 1));
    };
    state.update(&data);
    put(req, object, state);
    Ok(Value::bool(true))
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
fn hash_update_stream(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 2, 3)?;
    let object = object(req, 1)?;
    let given = req.argument(2).unwrap_or_default().deref();
    let Some(stream) = Stream::from_value(req, &given) else {
        return Err(if given.as_resource().is_some() {
            throw(
                req,
                "TypeError",
                "hash_update_stream(): supplied resource is not a valid stream resource",
            )
        } else {
            let name = value_name(req, &given);
            arg_type_error(req, 2, &format!("be of type resource, {name} given"))
        });
    };
    // `$length`: a negative one reads to the end, as php's loop does.
    let mut left = usize::try_from(long(req, 3, -1)?).ok();
    verify(req, object, 1)?;
    let Some(mut state) = take(req, object) else {
        return Err(finalized(req, 1));
    };
    let mut read = 0usize;
    while left != Some(0) {
        let want = left.map_or(1024, |left| left.min(1024));
        let chunk = match stream.read(req, want) {
            Ok(Some(chunk)) if !chunk.is_empty() => chunk,
            Ok(_) => break,
            Err(thrown) => {
                put(req, object, state);
                return Err(thrown);
            }
        };
        state.update(chunk.as_bytes());
        left = left.map(|left| left.saturating_sub(chunk.len()));
        read += chunk.len();
    }
    put(req, object, state);
    Ok(Value::int(i64::try_from(read).unwrap_or(i64::MAX)))
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
fn hash_update_file(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 2, 3)?;
    let object = object(req, 1)?;
    let path = string(req, 2)?;
    if path.contains(&0) {
        return Err(arg_value_error(req, 2, "not contain any null bytes"));
    }
    if let Some(given) = req.argument(3).map(|v| v.deref())
        && !matches!(
            given.as_ref(),
            ValueRef::Null | ValueRef::Undef | ValueRef::Resource(_)
        )
    {
        let name = value_name(req, &given);
        return Err(arg_type_error(
            req,
            3,
            &format!("be of type resource or null, {name} given"),
        ));
    }
    verify(req, object, 1)?;
    let Some(stream) = req.open_stream(&path, b"rb", false)? else {
        return Ok(Value::bool(false));
    };
    let Some(mut state) = take(req, object) else {
        stream.close(req);
        return Err(finalized(req, 1));
    };
    let read = pump(req, stream, state.as_mut());
    put(req, object, state);
    Ok(Value::bool(read?))
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
fn hash_final(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 1, 2)?;
    let object = context(req, 1)?;
    let binary = boolean(req, 2)?;
    let taken = object
        .native_mut::<Context>(req)
        .map(|c| (c.algo, c.state.take(), c.key.take()));
    let Some((Some(algo), Some(mut state), key)) = taken else {
        return Err(finalized(req, 1));
    };
    let mut digest = state.finish();
    if let Some(key) = key {
        digest = digest_of(req, algo, &[&outer(&key), &digest])?;
    }
    digest_value(req, &digest, binary)
}

/// `php_hashcontext_clone`: a fresh object with the members copied and
/// the state and key forked; a finalised state is php's
/// `ValueError`.
fn clone_context(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let forked = object.native::<Context>(req).map(|c| {
        (
            c.algo,
            c.state.as_ref().map(|s| s.fork()),
            c.flags,
            c.key.clone(),
        )
    });
    let Some((algo, Some(state), flags, key)) = forked else {
        return Err(throw(
            req,
            "ValueError",
            "Cannot clone a finalized HashContext",
        ));
    };
    let copy = new_object(req, object.class())?;
    for (name, value) in object.props(req)? {
        req.addref(&Value::string(name));
        req.addref(&value);
        copy.set_prop(req, name, value)?;
    }
    if let Some(context) = copy.native_mut::<Context>(req) {
        *context = Context {
            algo,
            state: Some(state),
            flags,
            key,
        };
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
fn hash_copy(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 1, 1)?;
    let object = context(req, 1)?;
    let copy = clone_context(req, object)?;
    Ok(copy.as_value())
}

/// The table's names, all or the cryptographic ones.
fn names(req: &mut Request, crypto: bool) -> Result<Value, Thrown> {
    let mut out = PhpArray::new(req)?;
    for algo in ALGOS.iter().filter(|a| !crypto || a.crypto) {
        let name = PhpString::new(req, algo.name.as_bytes())?;
        out.append(req, Value::string(name))?;
    }
    Ok(Value::array(out))
}

/// `hash_algos(): array`, the table's names in its order.
///
/// ```php
/// echo implode(' ', array_slice(hash_algos(), 0, 29)), "\n";
/// ```
/// ```expect
/// md2 md4 md5 sha1 sha224 sha256 sha384 sha512/224 sha512/256 sha512 sha3-224 sha3-256 sha3-384 sha3-512 ripemd128 ripemd160 ripemd256 ripemd320 whirlpool tiger128,3 tiger160,3 tiger192,3 tiger128,4 tiger160,4 tiger192,4 snefru snefru256 gost gost-crypto
/// ```
fn hash_algos(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 0, 0)?;
    names(req, false)
}

/// `hash_hmac_algos(): array`, the cryptographic ones.
///
/// ```php
/// $h = hash_hmac_algos(); echo implode(' ', array_slice($h, 20, 9)), ' ', in_array('crc32', $h) ? 'crc32' : '-', "\n";
/// ```
/// ```expect
/// tiger160,3 tiger192,3 tiger128,4 tiger160,4 tiger192,4 snefru snefru256 gost gost-crypto -
/// ```
fn hash_hmac_algos(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 0, 0)?;
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
fn hash_pbkdf2(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 4, 7)?;
    let name = string(req, 1)?;
    let password = string(req, 2)?;
    let salt = string(req, 3)?;
    let iterations = long(req, 4, 0)?;
    let mut length = long(req, 5, 0)?;
    let binary = boolean(req, 6)?;
    let options = options(req, 7)?;
    let algo = algo(req, &name, true)?;
    if iterations <= 0 {
        return Err(arg_value_error(req, 4, "be greater than 0"));
    }
    if length < 0 {
        return Err(arg_value_error(req, 5, "be greater than or equal to 0"));
    }
    // `ops->hash_init(context, args)`: what the options may raise, raised.
    let _ = (algo.init)(req, options)?;
    let k1 = hmac_key(req, algo, &password)?;
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
    Ok(Value::string(PhpString::new(req, &bytes)?))
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
fn hash_equals(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 2, 2)?;
    let mut strings = Vec::new();
    for position in 1..=2 {
        let given = req.argument(position).unwrap_or_default().deref();
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
    Ok(Value::bool(same))
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
fn hash_hkdf(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 2, 5)?;
    let name = string(req, 1)?;
    let ikm = string(req, 2)?;
    let length = long(req, 3, 0)?;
    let info = string(req, 4)?;
    let salt = string(req, 5)?;
    let algo = algo(req, &name, true)?;
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
    let k = hmac_key(req, algo, &salt)?;
    let prk = hmac(req, algo, &k, &ikm)?;
    let k = hmac_key(req, algo, &prk)?;
    let mut out = Vec::with_capacity(length);
    let mut digest: Vec<u8> = Vec::new();
    let mut i = 1u8;
    while out.len() < length {
        let inner = digest_of(req, algo, &[&k, &digest, &info, &[i]])?;
        digest = digest_of(req, algo, &[&outer(&k), &inner])?;
        let take = (length - out.len()).min(size);
        out.extend_from_slice(&digest[..take]);
        i = i.wrapping_add(1);
    }
    Ok(Value::string(PhpString::new(req, &out)?))
}

/// `zend_deprecated_function`, which php raises before the handler of
/// a function its stub marks `#[\Deprecated]`; raised here, as
/// ext/standard's `utf8_encode` raises its own.
fn deprecated_call(req: &mut Request, name: &str) {
    raising::deprecated(req, &format!("Function {name}() is deprecated since 8.1"));
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
fn mhash(req: &mut Request) -> Result<Value, Thrown> {
    deprecated_call(req, "mhash");
    count(req, 2, 3)?;
    let number = long(req, 1, 0)?;
    let data = string(req, 2)?;
    let key = match req.argument(3).map(|v| v.deref()) {
        Some(v) if !matches!(v.as_ref(), ValueRef::Null | ValueRef::Undef) => Some(string(req, 3)?),
        _ => None,
    };
    let Some((_, name)) = mhash_entry(number) else {
        return Ok(Value::bool(false));
    };
    let digest = if let Some(key) = key {
        let algo = algo(req, name.as_bytes(), true)?;
        let k = hmac_key(req, algo, &key)?;
        hmac(req, algo, &k, &data)?
    } else {
        let algo = algo(req, name.as_bytes(), false)?;
        digest_of(req, algo, &[&data])?
    };
    digest_value(req, &digest, true)
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
fn mhash_get_hash_name(req: &mut Request) -> Result<Value, Thrown> {
    deprecated_call(req, "mhash_get_hash_name");
    count(req, 1, 1)?;
    let number = long(req, 1, 0)?;
    match mhash_entry(number) {
        Some((name, _)) => Ok(Value::string(PhpString::new(req, name.as_bytes())?)),
        None => Ok(Value::bool(false)),
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
fn mhash_count(req: &mut Request) -> Result<Value, Thrown> {
    deprecated_call(req, "mhash_count");
    count(req, 0, 0)?;
    Ok(Value::int(41))
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
fn mhash_get_block_size(req: &mut Request) -> Result<Value, Thrown> {
    deprecated_call(req, "mhash_get_block_size");
    count(req, 1, 1)?;
    let number = long(req, 1, 0)?;
    let size = mhash_entry(number)
        .and_then(|(_, name)| algos::fetch(name.as_bytes()))
        .map(|a| a.digest);
    Ok(size.map_or(Value::bool(false), |s| {
        Value::int(i64::try_from(s).unwrap_or(0))
    }))
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
fn mhash_keygen_s2k(req: &mut Request) -> Result<Value, Thrown> {
    deprecated_call(req, "mhash_keygen_s2k");
    count(req, 4, 4)?;
    let number = long(req, 1, 0)?;
    let password = string(req, 2)?;
    let salt = string(req, 3)?;
    #[allow(clippy::cast_possible_truncation)]
    let bytes = long(req, 4, 0)? as i32;
    if bytes <= 0 {
        return Err(arg_value_error(req, 4, "be a greater than 0"));
    }
    let mut padded = [0u8; 8];
    let used = salt.len().min(8);
    padded[..used].copy_from_slice(&salt[..used]);
    let Some(algo) = mhash_entry(number).and_then(|(_, name)| algos::fetch(name.as_bytes())) else {
        return Ok(Value::bool(false));
    };
    let bytes = usize::try_from(bytes).unwrap_or(0);
    let mut key = Vec::new();
    let mut zeros = Vec::new();
    while key.len() < bytes {
        key.extend(digest_of(req, algo, &[&zeros, &padded, &password])?);
        zeros.push(0);
    }
    key.truncate(bytes);
    Ok(Value::string(PhpString::new(req, &key)?))
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
pub(crate) fn class() -> ClassDecl {
    ClassDecl {
        name: Name(b"HashContext".as_slice().into()),
        kind: ClassKind::Class,
        flags: ClassFlags {
            is_final: true,
            ..ClassFlags::default()
        },
        parent: None,
        interfaces: Vec::new(),
        traits: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        properties: Vec::new(),
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    }
}

/// The stub's four methods, in its order.
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
pub(crate) fn methods() -> Vec<(MethodDecl, Entry)> {
    let method = |line: &str, visibility| MethodDecl {
        function: stub(line),
        visibility,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    };
    vec![
        (
            method("__construct()", Visibility::Private),
            construct as Entry,
        ),
        (
            method("__serialize(): array", Visibility::Public),
            serialize,
        ),
        (
            method("__unserialize(array $data): void", Visibility::Public),
            unserialize,
        ),
        (
            method("__debugInfo(): array", Visibility::Public),
            debug_info,
        ),
    ]
}

/// `php_hashcontext_handlers`: `clone_obj` is `php_hashcontext_clone`,
/// and `free_obj` has nothing of the heap's to give back.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

impl ObjectHandlers for Handlers {
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_context)
    }
}

/// `HashContext::__construct`, which a program cannot reach, since it
/// is private.
fn construct(req: &mut Request) -> Result<Value, Thrown> {
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
fn serialize(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 0, 0)?;
    let Some(this) = req.this() else {
        return Ok(Value::null());
    };
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
    let Some(mut state) = take(req, this) else {
        return Err(cannot(req));
    };
    let fields = algos::serialize(req, state.as_mut());
    put(req, this, state);
    let fields = fields?;
    let mut out = PhpArray::new(req)?;
    let algo_name = PhpString::new(req, name.as_bytes())?;
    out.append(req, Value::string(algo_name))?;
    out.append(req, Value::int(flags.0))?;
    out.append(req, Value::array(fields))?;
    out.append(req, Value::int(magic))?;
    let mut members = PhpArray::new(req)?;
    for (name, value) in this.props(req)? {
        req.addref(&value);
        req.addref(&Value::string(name));
        members.set(req, Key::from_string(name), value)?;
    }
    out.append(req, Value::array(members))?;
    Ok(Value::array(out))
}

/// `Z_TYPE_P(zv) == IS_LONG`.
fn int_of(value: Option<Value>) -> Option<i64> {
    match value?.as_ref() {
        ValueRef::Int(n) => Some(n),
        _ => None,
    }
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
fn unserialize(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 1, 1)?;
    let data = match argument(req, 1, &Type::Array)?.and_then(|v| v.as_array().copied()) {
        Some(data) => data,
        None => PhpArray::new(req)?,
    };
    let Some(this) = req.this() else {
        return Ok(Value::null());
    };
    if this
        .native::<Context>(req)
        .is_some_and(|c| c.state.is_some())
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
    let mut state = (algo.init)(req, None)?;
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
    if let Some(context) = this.native_mut::<Context>(req) {
        context.state = Some(state);
        context.flags = flags;
    }
    for (key, value) in members.snapshot() {
        let name = key.to_prop_name(req)?;
        req.addref(&value);
        this.set_prop(req, name, value)?;
    }
    Ok(Value::null())
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
fn debug_info(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 0, 0)?;
    let algo = req
        .this()
        .and_then(|this| this.native::<Context>(req).and_then(|c| c.algo));
    let mut out = PhpArray::new(req)?;
    let key = PhpString::new(req, b"algo")?;
    let name = PhpString::new(req, algo.map_or("", |a| a.label).as_bytes())?;
    out.set(req, Key::Str(key), Value::string(name))?;
    Ok(Value::array(out))
}
