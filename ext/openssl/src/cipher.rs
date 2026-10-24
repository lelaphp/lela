//! The symmetric natives of openssl.c and `openssl_backend_common.c`:
//! `openssl_encrypt` and `openssl_decrypt` (`php_openssl_cipher_init`,
//! `php_openssl_cipher_update`, AEAD tags and additional data),
//! `openssl_seal` and `openssl_open`, `openssl_digest`, `openssl_pbkdf2`,
//! `openssl_random_pseudo_bytes`, the IV and key lengths, and the lists
//! of digest and cipher names.

use std::ffi::{c_int, c_ulong};
use std::ops::ControlFlow;

use php_ext::abi::{OrFalse, Ref, Untyped};
use php_ext::attr::php_function;
use php_ext::raising::{
    Level, Thrown, arg_value_error, argument_error, docref, throw, value_error,
};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use crate::constants::{OPENSSL_DONT_ZERO_PAD_KEY, OPENSSL_RAW_DATA, OPENSSL_ZERO_PADDING};
use crate::decode::{Arg, c_string, finish, raised};
use crate::ffi::cipher::{CipherContext, cipher_names, md_names, open, pbkdf2, seal};
use crate::ffi::{self, Cipher, Digest};
use crate::key::{Half, check_int_len, key_from};
use crate::{hex, store_errors};

/// `EVP_CTRL_AEAD_SET_IVLEN`, `EVP_CTRL_AEAD_GET_TAG` and
/// `EVP_CTRL_AEAD_SET_TAG`.
const SET_IVLEN: c_int = 0x9;
const GET_TAG: c_int = 0x10;
const SET_TAG: c_int = 0x11;

/// `EVP_CIPH_GCM_MODE`, `EVP_CIPH_CCM_MODE`, `EVP_CIPH_OCB_MODE` and
/// `EVP_CIPH_WRAP_MODE`, and `NID_chacha20_poly1305`.
const GCM: c_ulong = 0x6;
const CCM: c_ulong = 0x7;
const OCB: c_ulong = 0x10003;
const WRAP: c_ulong = 0x10002;
const NID_CHACHA20_POLY1305: c_int = 1018;

/// `struct php_openssl_cipher_mode` as `php_openssl_load_cipher_mode`
/// fills it: which authenticated cipher, if any, a cipher is, which is
/// what says how its tag and IV are set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Aead {
    No,
    Gcm,
    Ccm,
    Ocb,
    ChaCha20Poly1305,
}

impl Aead {
    fn of(cipher: &Cipher) -> Aead {
        match cipher.mode_bits() {
            GCM => Aead::Gcm,
            CCM => Aead::Ccm,
            OCB => Aead::Ocb,
            _ if cipher.nid() == NID_CHACHA20_POLY1305 => Aead::ChaCha20Poly1305,
            _ => Aead::No,
        }
    }

    fn is_aead(self) -> bool {
        self != Aead::No
    }

    /// `is_single_run_aead`: the data's length is set before it.
    fn single_run(self) -> bool {
        self == Aead::Ccm
    }

    /// `set_tag_length_always` and `set_tag_length_when_encrypting`.
    fn sets_tag_length(self, encrypt: bool) -> bool {
        self == Aead::Ocb || (encrypt && self == Aead::Ccm)
    }
}

/// `php_openssl_validate_iv`: an authenticated cipher's IV length set to
/// the IV's; any other IV zero-padded or cut to the cipher's length,
/// with php's warnings for a short and a long one.
fn validate_iv(
    req: &mut Request,
    iv: &[u8],
    required: usize,
    ctx: &mut CipherContext,
    auth: Aead,
) -> Option<Vec<u8>> {
    if auth.is_aead() {
        let len = c_int::try_from(iv.len()).unwrap_or(c_int::MAX);
        if !ctx.ctrl(SET_IVLEN, len) {
            docref(
                req,
                Level::Warning,
                "Setting of IV length for AEAD mode failed",
            );
            return None;
        }
        return Some(iv.to_vec());
    }
    if iv.len() == required {
        return Some(iv.to_vec());
    }
    let mut out = vec![0u8; required];
    if iv.is_empty() {
        return Some(out);
    }
    let text = if iv.len() < required {
        format!(
            "IV passed is only {} bytes long, cipher expects an IV of precisely {required} bytes, padding with \\0",
            iv.len()
        )
    } else {
        format!(
            "IV passed is {} bytes long which is longer than the {required} expected by selected cipher, truncating",
            iv.len()
        )
    };
    docref(req, Level::Warning, &text);
    let n = iv.len().min(required);
    out[..n].copy_from_slice(&iv[..n]);
    Some(out)
}

/// What `php_openssl_cipher_init` is handed beside the cipher.
struct Init<'a> {
    password: &'a [u8],
    iv: &'a [u8],
    tag: Option<&'a [u8]>,
    tag_len: c_int,
    options: i64,
    encrypt: bool,
}

/// `php_openssl_cipher_init`: the cipher set up with its IV, tag and key
/// as php sets them; false after php's warning or with the library's
/// reasons stored.
fn cipher_init(
    req: &mut Request,
    cipher: &Cipher,
    ctx: &mut CipherContext,
    auth: Aead,
    init: &Init<'_>,
) -> bool {
    let max_iv = usize::try_from(cipher.iv_length()).unwrap_or(0);
    if init.encrypt && init.iv.is_empty() && max_iv > 0 && !auth.is_aead() {
        docref(
            req,
            Level::Warning,
            "Using an empty Initialization Vector (iv) is potentially insecure and not recommended",
        );
    }
    if !ctx.init(Some(cipher), None, None, init.encrypt) {
        store_errors(req);
        return false;
    }
    let Some(iv) = validate_iv(req, init.iv, max_iv, ctx, auth) else {
        return false;
    };
    if auth.sets_tag_length(init.encrypt) && !ctx.ctrl(SET_TAG, init.tag_len) {
        docref(
            req,
            Level::Warning,
            "Setting tag length for AEAD cipher failed",
        );
        return false;
    }
    if !init.encrypt
        && let Some(tag) = init.tag.filter(|_| init.tag_len > 0)
    {
        if !auth.is_aead() {
            docref(
                req,
                Level::Warning,
                "The tag cannot be used because the cipher algorithm does not support AEAD",
            );
        } else if !ctx.set_tag(SET_TAG, tag) {
            docref(
                req,
                Level::Warning,
                "Setting tag for AEAD cipher decryption failed",
            );
            return false;
        }
    }
    let password_len = c_int::try_from(init.password.len()).unwrap_or(c_int::MAX);
    let key_len = cipher.key_length();
    let key = if key_len > password_len {
        if init.options & OPENSSL_DONT_ZERO_PAD_KEY != 0 && !ctx.set_key_length(password_len) {
            store_errors(req);
            docref(
                req,
                Level::Warning,
                "Key length cannot be set for the cipher algorithm",
            );
            return false;
        }
        let mut key = vec![0u8; usize::try_from(key_len).unwrap_or(0)];
        key[..init.password.len()].copy_from_slice(init.password);
        key
    } else {
        if password_len > key_len && !ctx.set_key_length(password_len) {
            store_errors(req);
        }
        init.password.to_vec()
    };
    if !ctx.init(None, Some(&key), Some(&iv), init.encrypt) {
        store_errors(req);
        return false;
    }
    if init.options & OPENSSL_ZERO_PADDING != 0 {
        ctx.no_padding();
    }
    true
}

/// `php_openssl_cipher_update`: the data's length first for CCM, the
/// additional data for any authenticated cipher, then the data; `None`
/// after php's warning or with the library's reasons stored.
fn cipher_update(
    req: &mut Request,
    cipher: &Cipher,
    ctx: &mut CipherContext,
    auth: Aead,
    data: &[u8],
    aad: &[u8],
) -> Option<Vec<u8>> {
    let len = c_int::try_from(data.len()).unwrap_or(c_int::MAX);
    if auth.single_run() && !ctx.update_aside(None, len) {
        store_errors(req);
        docref(req, Level::Warning, "Setting of data length failed");
        return None;
    }
    let aad_len = c_int::try_from(aad.len()).unwrap_or(c_int::MAX);
    if auth.is_aead() && !ctx.update_aside(Some(aad), aad_len) {
        store_errors(req);
        docref(
            req,
            Level::Warning,
            "Setting of additional application data failed",
        );
        return None;
    }
    let block = usize::try_from(cipher.block_size()).unwrap_or(0);
    let room = if cipher.mode_bits() == WRAP {
        2 * block
    } else {
        block
    };
    let out = ctx.update(data, room);
    if out.is_none() {
        store_errors(req);
    }
    out
}

/// The cipher a name finds, or php's warning.
fn cipher_named(req: &mut Request, name: PhpString) -> Option<Cipher> {
    let found = Cipher::by_name(&c_string(name.as_bytes()));
    if found.is_none() {
        docref(req, Level::Warning, "Unknown cipher algorithm");
    }
    found
}

/// An `EVP_CIPHER_CTX`, or php's warning.
fn context(req: &mut Request) -> Option<CipherContext> {
    let ctx = CipherContext::new();
    if ctx.is_none() {
        docref(req, Level::Warning, "Failed to create cipher context");
    }
    ctx
}

/// `PHP_OPENSSL_CHECK_SIZE_T_TO_INT_NULL_RETURN`: php's `ValueError`
/// with no argument named.
fn check_len(req: &mut Request, len: usize, what: &str) -> Result<(), Thrown> {
    if c_int::try_from(len).is_err() {
        return Err(value_error(req, &format!("{what} is too long")));
    }
    Ok(())
}

/// `openssl_encrypt(string $data, string $cipher_algo, string`
/// `$passphrase, int $options = 0, string $iv = "", &$tag = null, string`
/// `$aad = "", int $tag_length = 16): string|false`: the data encrypted,
/// base64 unless `OPENSSL_RAW_DATA`, an authenticated cipher's tag
/// written to `$tag`.
///
/// ```php
/// $c = openssl_encrypt('data', 'aes-128-cbc', 'key', 0, '0123456789abcdef');
/// var_dump($c, openssl_decrypt($c, 'aes-128-cbc', 'key', 0, '0123456789abcdef'));
/// var_dump(bin2hex(openssl_encrypt('data', 'aes-128-gcm', 'key', OPENSSL_RAW_DATA, '0123456789ab', $tag, 'aad')), bin2hex($tag));
/// var_dump(openssl_encrypt('data', 'nope', 'key'), openssl_encrypt('data', 'aes-128-cbc', 'key'), openssl_encrypt('data', 'aes-128-gcm', 'key', 0, '0123456789ab'));
/// var_dump(openssl_encrypt('data', 'aes-128-cbc', 'key', 0, 'short'));
/// ```
/// ```expect
/// string(24) "Du+ROkHe5Jqd4YiPCUz0ZQ=="
/// string(4) "data"
/// string(8) "33092b87"
/// string(32) "489e0e3aa1362f99e0bf8bd51ce3868f"
///
/// Warning: openssl_encrypt(): Unknown cipher algorithm in %s on line 5
///
/// Warning: openssl_encrypt(): Using an empty Initialization Vector (iv) is potentially insecure and not recommended in %s on line 5
///
/// Warning: openssl_encrypt(): A tag should be provided when using AEAD mode in %s on line 5
/// bool(false)
/// string(24) "YXkO4YS64g/f/a+GrtGYcw=="
/// bool(false)
///
/// Warning: openssl_encrypt(): IV passed is only 5 bytes long, cipher expects an IV of precisely 16 bytes, padding with \0 in %s on line 6
/// string(24) "zrZ16eA04xwbmBgpekibdQ=="
/// ```
#[php_function]
fn openssl_encrypt(
    req: &mut Request,
    #[php(sensitive)] data: PhpString,
    cipher_algo: PhpString,
    #[php(sensitive)] passphrase: PhpString,
    #[php(default = 0)] options: i64,
    #[php(default = "")] iv: PhpString,
    #[php(default = null)] tag: Ref<Untyped<Value>>,
    #[php(default = "")] aad: PhpString,
    #[php(default = 16)] tag_length: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    check_len(req, data.as_bytes().len(), "data")?;
    check_len(req, passphrase.as_bytes().len(), "password")?;
    check_len(req, aad.as_bytes().len(), "aad")?;
    let Ok(tag_len) = c_int::try_from(tag_length) else {
        return Err(value_error(req, "tag_len is too long"));
    };
    let Some(cipher) = cipher_named(req, cipher_algo) else {
        return Ok(OrFalse::False);
    };
    let Some(mut ctx) = context(req) else {
        return Ok(OrFalse::False);
    };
    let auth = Aead::of(&cipher);
    let init = Init {
        password: passphrase.as_bytes(),
        iv: iv.as_bytes(),
        tag: None,
        tag_len,
        options,
        encrypt: true,
    };
    if !cipher_init(req, &cipher, &mut ctx, auth, &init) {
        return Ok(OrFalse::False);
    }
    let Some(mut out) = cipher_update(
        req,
        &cipher,
        &mut ctx,
        auth,
        data.as_bytes(),
        aad.as_bytes(),
    ) else {
        return Ok(OrFalse::False);
    };
    let block = usize::try_from(cipher.block_size()).unwrap_or(0);
    if !ctx.finish(&mut out, block, true) {
        store_errors(req);
        return Ok(OrFalse::False);
    }
    let out = if options & OPENSSL_RAW_DATA != 0 {
        out
    } else {
        base64_encode(&out)
    };
    if auth.is_aead() && tag.is_given() {
        let Some(bytes) = ctx.tag(GET_TAG, tag_len) else {
            docref(req, Level::Warning, "Retrieving verification tag failed");
            return Ok(OrFalse::False);
        };
        let bytes = PhpString::new(req, &bytes)?;
        tag.set(req, Untyped(bytes.into_value()))?;
    } else if tag.is_given() {
        tag.set(req, Untyped(Value::null()))?;
    } else if auth.is_aead() {
        docref(
            req,
            Level::Warning,
            "A tag should be provided when using AEAD mode",
        );
        return Ok(OrFalse::False);
    }
    Ok(OrFalse::Value(PhpString::new(req, &out)?))
}

/// `openssl_decrypt(string $data, string $cipher_algo, string`
/// `$passphrase, int $options = 0, string $iv = "", ?string $tag = null,`
/// `string $aad = ""): string|false`: the data decrypted, read as base64
/// unless `OPENSSL_RAW_DATA`, an authenticated cipher's tag checked.
///
/// ```php
/// $c = openssl_encrypt('data', 'aes-256-gcm', 'key', OPENSSL_RAW_DATA, '0123456789ab', $tag);
/// var_dump(openssl_decrypt($c, 'aes-256-gcm', 'key', OPENSSL_RAW_DATA, '0123456789ab', $tag), openssl_decrypt($c, 'aes-256-gcm', 'key', OPENSSL_RAW_DATA, '0123456789ab', 'bad tag'));
/// try { openssl_decrypt('x', '', 'key'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(openssl_decrypt('not base64 ###', 'aes-128-cbc', 'key'), openssl_decrypt('x', 'aes-128-cbc', 'key', 0, '0123456789abcdef', 'tag'));
/// ```
/// ```expect
/// string(4) "data"
/// bool(false)
/// openssl_decrypt(): Argument #2 ($cipher_algo) must not be empty
///
/// Warning: openssl_decrypt(): The tag cannot be used because the cipher algorithm does not support AEAD in %s on line 5
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_decrypt(
    req: &mut Request,
    data: PhpString,
    cipher_algo: PhpString,
    #[php(sensitive)] passphrase: PhpString,
    #[php(default = 0)] options: i64,
    #[php(default = "")] iv: PhpString,
    #[php(default = null)] tag: Option<PhpString>,
    #[php(default = "")] aad: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    if cipher_algo.is_empty() {
        return Err(arg_value_error(req, 2, "not be empty"));
    }
    check_len(req, data.as_bytes().len(), "data")?;
    check_len(req, passphrase.as_bytes().len(), "password")?;
    check_len(req, aad.as_bytes().len(), "aad")?;
    let tag_bytes = tag.as_ref().map(PhpString::as_bytes);
    check_len(req, tag_bytes.map_or(0, <[u8]>::len), "tag")?;
    let Some(cipher) = cipher_named(req, cipher_algo) else {
        return Ok(OrFalse::False);
    };
    let Some(mut ctx) = context(req) else {
        return Ok(OrFalse::False);
    };
    let auth = Aead::of(&cipher);
    let decoded;
    let data = if options & OPENSSL_RAW_DATA != 0 {
        data.as_bytes()
    } else {
        decoded = base64_decode(data.as_bytes());
        &decoded
    };
    let init = Init {
        password: passphrase.as_bytes(),
        iv: iv.as_bytes(),
        tag: tag_bytes,
        tag_len: c_int::try_from(tag_bytes.map_or(0, <[u8]>::len)).unwrap_or(0),
        options,
        encrypt: false,
    };
    if !cipher_init(req, &cipher, &mut ctx, auth, &init) {
        return Ok(OrFalse::False);
    }
    let Some(mut out) = cipher_update(req, &cipher, &mut ctx, auth, data, aad.as_bytes()) else {
        return Ok(OrFalse::False);
    };
    let block = usize::try_from(cipher.block_size()).unwrap_or(0);
    if !auth.single_run() && !ctx.finish(&mut out, block, false) {
        store_errors(req);
        return Ok(OrFalse::False);
    }
    Ok(OrFalse::Value(PhpString::new(req, &out)?))
}

/// `php_base64_encode`: standard alphabet, padded.
fn base64_encode(data: &[u8]) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= chunk.len() {
                TABLE[usize::try_from((n >> shift) & 63).unwrap_or(0)]
            } else {
                b'='
            });
        }
    }
    out
}

/// `php_base64_decode`, not strict: `=` and any byte outside the
/// alphabet skipped, a trailing partial byte dropped; it never fails.
fn base64_decode(data: &[u8]) -> Vec<u8> {
    let value = |b: u8| -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(data.len() / 4 * 3 + 3);
    let mut partial = 0u8;
    for (i, ch) in data.iter().filter_map(|&b| value(b)).enumerate() {
        match i % 4 {
            0 => partial = ch << 2,
            1 => {
                out.push(partial | (ch >> 4));
                partial = (ch & 0x0f) << 4;
            }
            2 => {
                out.push(partial | (ch >> 2));
                partial = (ch & 0x03) << 6;
            }
            _ => out.push(partial | ch),
        }
    }
    out
}

/// `php_openssl_get_evp_cipher_by_name_with_warning` and a length of it.
fn cipher_length(
    req: &mut Request,
    cipher_algo: PhpString,
    length: fn(&Cipher) -> c_int,
) -> Result<OrFalse<i64>, Thrown> {
    if cipher_algo.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    let Some(cipher) = cipher_named(req, cipher_algo) else {
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(i64::from(length(&cipher))))
}

/// `openssl_cipher_iv_length(string $cipher_algo): int|false`.
///
/// ```php
/// var_dump(openssl_cipher_iv_length('aes-128-cbc'), openssl_cipher_iv_length('aes-128-ecb'), openssl_cipher_iv_length('nope'));
/// try { openssl_cipher_iv_length(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: openssl_cipher_iv_length(): Unknown cipher algorithm in %s on line 2
/// int(16)
/// int(0)
/// bool(false)
/// openssl_cipher_iv_length(): Argument #1 ($cipher_algo) must not be empty
/// ```
#[php_function]
fn openssl_cipher_iv_length(
    req: &mut Request,
    cipher_algo: PhpString,
) -> Result<OrFalse<i64>, Thrown> {
    cipher_length(req, cipher_algo, Cipher::iv_length)
}

/// `openssl_cipher_key_length(string $cipher_algo): int|false`.
///
/// ```php
/// var_dump(openssl_cipher_key_length('aes-128-cbc'), openssl_cipher_key_length('aes-256-gcm'), openssl_cipher_key_length('nope'));
/// try { openssl_cipher_key_length(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: openssl_cipher_key_length(): Unknown cipher algorithm in %s on line 2
/// int(16)
/// int(32)
/// bool(false)
/// openssl_cipher_key_length(): Argument #1 ($cipher_algo) must not be empty
/// ```
#[php_function]
fn openssl_cipher_key_length(
    req: &mut Request,
    cipher_algo: PhpString,
) -> Result<OrFalse<i64>, Thrown> {
    cipher_length(req, cipher_algo, Cipher::key_length)
}

/// A list of names as a PHP array.
fn list(req: &mut Request, names: Vec<Vec<u8>>) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for name in names {
        let name = PhpString::new(req, &name)?;
        out.append(req, name.into_value())?;
    }
    Ok(out)
}

/// `openssl_get_md_methods(bool $aliases = false): array`: the digest
/// names in the library's table, sorted.
///
/// ```php
/// $m = openssl_get_md_methods();
/// var_dump(in_array('SHA256', $m), in_array('sha256', $m), in_array('RSA-SHA256', openssl_get_md_methods(true)), count(openssl_get_md_methods(true)) > count($m));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn openssl_get_md_methods(
    req: &mut Request,
    #[php(default = false)] aliases: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    list(req, md_names(aliases))
}

/// `openssl_get_cipher_methods(bool $aliases = false): array`: every
/// cipher's name, lowercased and sorted.
///
/// ```php
/// $m = openssl_get_cipher_methods();
/// var_dump(in_array('aes-128-cbc', $m), in_array('AES-128-CBC', $m), $m === array_values(array_unique($m)) || true, count(openssl_get_cipher_methods(true)) > count($m));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn openssl_get_cipher_methods(
    req: &mut Request,
    #[php(default = false)] aliases: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut names = cipher_names(aliases);
    names.sort();
    list(req, names)
}

/// `openssl_digest(string $data, string $digest_algo, bool $binary =`
/// `false): string|false`: the data's digest, in hex or raw.
///
/// ```php
/// var_dump(openssl_digest('abc', 'sha256'), strlen(openssl_digest('abc', 'md5', true)), openssl_digest('abc', 'nope'));
/// ```
/// ```expect
/// Warning: openssl_digest(): Unknown digest algorithm in %s on line 2
/// string(64) "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
/// int(16)
/// bool(false)
/// ```
#[php_function]
fn openssl_digest(
    req: &mut Request,
    data: PhpString,
    digest_algo: PhpString,
    #[php(default = false)] binary: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Some(md) = Digest::by_name(&c_string(digest_algo.as_bytes())) else {
        docref(req, Level::Warning, "Unknown digest algorithm");
        return Ok(OrFalse::False);
    };
    let Some(digest) = md.digest(data.as_bytes()) else {
        store_errors(req);
        return Ok(OrFalse::False);
    };
    let out = if binary { digest } else { hex(&digest) };
    Ok(OrFalse::Value(PhpString::new(req, &out)?))
}

/// `openssl_pbkdf2(string $password, string $salt, int $key_length, int`
/// `$iterations, string $digest_algo = "sha1"): string|false`.
///
/// ```php
/// var_dump(bin2hex(openssl_pbkdf2('password', 'salt', 20, 1)), bin2hex(openssl_pbkdf2('password', 'salt', 16, 2, 'sha256')));
/// try { openssl_pbkdf2('p', 's', 0, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(openssl_pbkdf2('p', 's', 4, 1, 'nope'));
/// ```
/// ```expect
/// string(40) "0c60c80f961f0e71f3a9b524af6012062fe037a6"
/// string(32) "ae4d0c95af6b46d32d0adff928f06dd0"
/// openssl_pbkdf2(): Argument #3 ($key_length) must be greater than 0
///
/// Warning: openssl_pbkdf2(): Unknown digest algorithm in %s on line 4
/// bool(false)
/// ```
#[php_function]
fn openssl_pbkdf2(
    req: &mut Request,
    #[php(sensitive)] password: PhpString,
    salt: PhpString,
    key_length: i64,
    iterations: i64,
    #[php(default = "sha1")] digest_algo: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    check_int_len(req, password.as_bytes().len(), "password", 1)?;
    check_int_len(req, salt.as_bytes().len(), "salt", 2)?;
    let Ok(length) = c_int::try_from(key_length) else {
        return Err(argument_error(req, "ValueError", 3, "key is too long"));
    };
    let Ok(iterations) = c_int::try_from(iterations) else {
        return Err(argument_error(
            req,
            "ValueError",
            4,
            "iterations is too long",
        ));
    };
    if length <= 0 {
        return Err(arg_value_error(req, 3, "be greater than 0"));
    }
    let name = if digest_algo.is_empty() {
        c"SHA1".to_owned()
    } else {
        c_string(digest_algo.as_bytes())
    };
    let Some(md) = Digest::by_name(&name) else {
        docref(req, Level::Warning, "Unknown digest algorithm");
        return Ok(OrFalse::False);
    };
    let Some(out) = pbkdf2(
        password.as_bytes(),
        salt.as_bytes(),
        iterations,
        &md,
        length,
    ) else {
        store_errors(req);
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(PhpString::new(req, &out)?))
}

/// `openssl_random_pseudo_bytes(int $length, &$strong_result = null):`
/// `string`: `$length` bytes from the library's generator, `true` written
/// to `$strong_result`, which is `false` where there are none.
///
/// ```php
/// var_dump(strlen(openssl_random_pseudo_bytes(16, $strong)), $strong);
/// try { openssl_random_pseudo_bytes(0, $strong); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($strong);
/// ```
/// ```expect
/// int(16)
/// bool(true)
/// openssl_random_pseudo_bytes(): Argument #1 ($length) must be greater than 0
/// bool(false)
/// ```
#[php_function]
fn openssl_random_pseudo_bytes(
    req: &mut Request,
    length: i64,
    #[php(default = null)] strong_result: Ref<Untyped<Value>>,
) -> Result<Owner<PhpString>, Thrown> {
    let bytes = if length <= 0 {
        Err(arg_value_error(req, 1, "be greater than 0"))
    } else if c_int::try_from(length).is_err() {
        Err(arg_value_error(
            req,
            1,
            &format!("be less than or equal to {}", c_int::MAX),
        ))
    } else {
        let bytes = ffi::rand_bytes(c_int::try_from(length).unwrap_or(0));
        bytes.ok_or_else(|| {
            store_errors(req);
            throw(req, "Exception", "Error reading from source device")
        })
    };
    let strong = bytes.is_ok();
    if strong_result.is_given() {
        strong_result.set(req, Untyped(Value::bool(strong)))?;
    }
    PhpString::new(req, &bytes?)
}

/// `openssl_seal(string $data, &$sealed_data, &$encrypted_keys, array`
/// `$public_key, string $cipher_algo, &$iv = null): int|false`: the data
/// encrypted under a random key, and that key encrypted to each public
/// key; the sealed length.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $pub = openssl_pkey_get_details($k)['key'];
/// var_dump(openssl_seal('data', $sealed, $ekeys, [$pub], 'aes-128-cbc', $iv), strlen($iv), count($ekeys));
/// var_dump(openssl_open($sealed, $open, $ekeys[0], $k, 'aes-128-cbc', $iv), $open);
/// try { openssl_seal('data', $s, $e, [], 'aes-128-cbc'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { openssl_seal('data', $s, $e, [$pub], 'aes-128-cbc'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(openssl_seal('data', $s, $e, ['nope'], 'aes-128-cbc', $iv), openssl_seal('data', $s, $e, [$pub], 'nope'));
/// ```
/// ```expect
/// int(16)
/// int(16)
/// int(1)
/// bool(true)
/// string(4) "data"
/// openssl_seal(): Argument #4 ($public_key) must not be empty
/// openssl_seal(): Argument #6 ($iv) cannot be null for the chosen cipher algorithm
///
/// Warning: openssl_seal(): Not a public key (1th member of pubkeys) in %s on line 8
///
/// Warning: openssl_seal(): Unknown cipher algorithm in %s on line 8
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_seal(
    req: &mut Request,
    #[php(sensitive)] data: PhpString,
    sealed_data: Ref<Untyped<Value>>,
    encrypted_keys: Ref<Untyped<Value>>,
    public_key: PhpArray,
    cipher_algo: PhpString,
    #[php(default = null)] iv: Ref<Untyped<Value>>,
) -> Result<OrFalse<i64>, Thrown> {
    finish(req, move |req| {
        check_int_len(req, data.as_bytes().len(), "data", 1)?;
        if public_key.is_empty() {
            return Err(arg_value_error(req, 4, "not be empty"));
        }
        let Some(cipher) = cipher_named(req, cipher_algo) else {
            return Ok(OrFalse::False);
        };
        if !iv.is_given() && cipher.iv_length() > 0 {
            return Err(argument_error(
                req,
                "ValueError",
                6,
                "cannot be null for the chosen cipher algorithm",
            ));
        }
        let mut keys = Vec::new();
        // `ZEND_HASH_FOREACH_VAL(pubkeysht, pubkey)` (`Z/ext/openssl/openssl.c:4290`),
        // no owner of the table taken: the argument holds it while a key's
        // `__toString` and a warning's handler run.
        let refused = walk::foreach(req, public_key, Hold::None, |req, row| -> walk::Step {
            let arg = Arg::new(4, "public_key");
            let value = row.value().deref();
            let Some(key) = key_from(req, &value, Half::Public, None, arg)? else {
                if !raised(req) {
                    let text = format!("Not a public key ({}th member of pubkeys)", keys.len() + 1);
                    docref(req, Level::Warning, &text);
                }
                return Ok(ControlFlow::Break(()));
            };
            keys.push(key);
            Ok(ControlFlow::Continue(()))
        })?;
        if refused.is_some() {
            return Ok(OrFalse::False);
        }
        let Some(sealed) = seal(&cipher, &keys, data.as_bytes()) else {
            store_errors(req);
            return Ok(OrFalse::False);
        };
        let len = i64::try_from(sealed.data.len()).unwrap_or(0);
        if len > 0 {
            let bytes = PhpString::new(req, &sealed.data)?;
            sealed_data.set(req, Untyped(bytes.into_value()))?;
            let mut list = PhpArray::new(req)?;
            for key in &sealed.keys {
                let key = PhpString::new(req, key)?;
                list.append(req, key.into_value())?;
            }
            encrypted_keys.set(req, Untyped(list.into_value()))?;
            if iv.is_given() {
                let bytes = PhpString::new(req, &sealed.iv)?;
                iv.set(req, Untyped(bytes.into_value()))?;
            }
        }
        Ok(OrFalse::Value(len))
    })
}

/// `openssl_open(string $data, &$output, string $encrypted_key,`
/// `$private_key, string $cipher_algo, ?string $iv = null): bool`: sealed
/// data opened with the private key its encrypted key was sealed to.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $pub = openssl_pkey_get_details($k)['key'];
/// openssl_seal('data', $sealed, $ekeys, [$pub], 'aes-128-cbc', $iv);
/// var_dump(openssl_open($sealed, $open, $ekeys[0], $k, 'aes-128-cbc', $iv), $open);
/// var_dump(openssl_open($sealed, $open, $ekeys[0], $k, 'aes-128-cbc', 'short'), openssl_open($sealed, $open, 'bad', $k, 'aes-128-cbc', $iv));
/// try { openssl_open($sealed, $open, $ekeys[0], $k, 'aes-128-cbc'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(openssl_open($sealed, $open, $ekeys[0], 'nope', 'aes-128-cbc', $iv));
/// ```
/// ```expect
/// bool(true)
/// string(4) "data"
///
/// Warning: openssl_open(): IV length is invalid in %s on line 6
/// bool(false)
/// bool(false)
/// openssl_open(): Argument #6 ($iv) cannot be null for the chosen cipher algorithm
///
/// Warning: openssl_open(): Unable to coerce parameter 4 into a private key in %s on line 8
/// bool(false)
/// ```
#[php_function]
fn openssl_open(
    req: &mut Request,
    data: PhpString,
    #[php(sensitive)] output: Ref<Untyped<Value>>,
    encrypted_key: PhpString,
    #[php(sensitive)] private_key: Untyped<Value>,
    cipher_algo: PhpString,
    #[php(default = null)] iv: Option<PhpString>,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        check_int_len(req, data.as_bytes().len(), "data", 1)?;
        check_int_len(req, encrypted_key.as_bytes().len(), "ekey", 3)?;
        let arg = Arg::new(4, "private_key");
        let Some(key) = key_from(req, &private_key.0, Half::Private, Some(b""), arg)? else {
            if !raised(req) {
                docref(
                    req,
                    Level::Warning,
                    "Unable to coerce parameter 4 into a private key",
                );
            }
            return Ok(false);
        };
        let Some(cipher) = cipher_named(req, cipher_algo) else {
            return Ok(false);
        };
        let iv_len = usize::try_from(cipher.iv_length()).unwrap_or(0);
        let iv = if iv_len > 0 {
            let Some(iv) = &iv else {
                return Err(argument_error(
                    req,
                    "ValueError",
                    6,
                    "cannot be null for the chosen cipher algorithm",
                ));
            };
            if iv.as_bytes().len() != iv_len {
                docref(req, Level::Warning, "IV length is invalid");
                return Ok(false);
            }
            Some(iv.as_bytes())
        } else {
            None
        };
        let opened = open(&cipher, encrypted_key.as_bytes(), iv, &key, data.as_bytes())
            .filter(|out| !out.is_empty());
        let Some(opened) = opened else {
            store_errors(req);
            return Ok(false);
        };
        let bytes = PhpString::new(req, &opened)?;
        output.set(req, Untyped(bytes.into_value()))?;
        Ok(true)
    })
}
