//! `util.c`'s signatures: `phar_verify_signature` over the bytes before
//! the signature, and `phar_create_signature` over a new archive, the
//! hashes through ext/hash's [`Digest`] as php's through `php_hash.h`,
//! and an OpenSSL signature through `openssl_verify()` and
//! `openssl_sign()` as `phar_call_openssl_signverify` calls them where
//! php is built without its own OpenSSL.

use php_ext::call::{call, lookup_function};
use php_ext::coerce::callable_checked;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{PhpRef, Value};
use php_ext_hash::Digest;

use crate::archive::{
    self, SIG_MD5, SIG_OPENSSL, SIG_OPENSSL_SHA256, SIG_OPENSSL_SHA512, SIG_SHA1, SIG_SHA256,
    SIG_SHA512, string,
};

/// The algorithm ext/hash names for a signature type, with the length
/// of its digest.
fn algo(sig_flags: u32) -> Option<(&'static [u8], usize)> {
    match sig_flags {
        SIG_SHA512 => Some((b"sha512", 64)),
        SIG_SHA256 => Some((b"sha256", 32)),
        SIG_SHA1 => Some((b"sha1", 20)),
        SIG_MD5 => Some((b"md5", 16)),
        _ => None,
    }
}

/// The digest of `data` by `name`.
fn digest(req: &mut Request, name: &[u8], data: &[u8]) -> Result<Vec<u8>, Thrown> {
    let Some(mut d) = Digest::fetch(req, name)? else {
        return Ok(Vec::new());
    };
    d.update(data);
    Ok(d.finish())
}

/// `phar_hex_str`: upper-case hex.
pub(crate) fn hex(digest: &[u8]) -> Vec<u8> {
    const HEX: &[u8] = b"0123456789ABCDEF";
    digest
        .iter()
        .flat_map(|b| [HEX[usize::from(b >> 4)], HEX[usize::from(b & 0xF)]])
        .collect()
}

/// `phar_verify_signature`: the signature of `fp`'s first `end` bytes
/// held to `sig`; its hex, or php's message.
pub(crate) fn verify(
    req: &mut Request,
    fp: &[u8],
    end: usize,
    sig_flags: u32,
    sig: &[u8],
    fname: &[u8],
) -> Result<Result<Vec<u8>, String>, Thrown> {
    if let Some((name, len)) = algo(sig_flags) {
        if sig.len() < len {
            return Ok(Err("broken signature".into()));
        }
        let made = digest(req, name, &fp[..end])?;
        if made != sig[..len] {
            return Ok(Err("broken signature".into()));
        }
        return Ok(Ok(hex(&made)));
    }
    if !matches!(
        sig_flags,
        SIG_OPENSSL | SIG_OPENSSL_SHA256 | SIG_OPENSSL_SHA512
    ) {
        return Ok(Err("broken or unsupported signature".into()));
    }
    if req.extensions().named(b"openssl").is_none() {
        return Ok(Err("openssl not loaded".into()));
    }
    let mut pfile = fname.to_vec();
    pfile.extend_from_slice(b".pubkey");
    let pubkey = archive::read_file(req, &pfile, false)?.filter(|k| !k.is_empty());
    let Some(pubkey) = pubkey else {
        return Ok(Err("openssl public key could not be read".into()));
    };
    if openssl_call(req, false, &fp[..end], &pubkey, sig, sig_flags)?.is_none() {
        return Ok(Err("openssl signature could not be verified".into()));
    }
    Ok(Ok(hex(sig)))
}

/// `phar_call_openssl_signverify`: `openssl_verify($data, $sig, $key,
/// $algo)`, or `openssl_sign($data, &$sig, $key, $algo)`, found in the
/// function table; the signature a sign made, the one verified, or
/// `None` where the function is not there or said no.
fn openssl_call(
    req: &mut Request,
    is_sign: bool,
    data: &[u8],
    key: &[u8],
    sig: &[u8],
    sig_flags: u32,
) -> Result<Option<Vec<u8>>, Thrown> {
    let name: &[u8] = if is_sign {
        b"openssl_sign"
    } else {
        b"openssl_verify"
    };
    if lookup_function(req, name).is_none() {
        return Ok(None);
    }
    let named = string(req, name)?;
    let function = callable_checked(req, &named);
    named.give_up(req);
    let Ok(function) = function else {
        return Ok(None);
    };
    let algo = match sig_flags {
        SIG_OPENSSL_SHA512 => 9,
        SIG_OPENSSL_SHA256 => 7,
        _ => 1,
    };
    let data = string(req, data)?;
    let signature = string(req, sig)?;
    let signature = if is_sign {
        PhpRef::new(req, signature)?.into_value()
    } else {
        signature
    };
    let key = string(req, key)?;
    let args = [*data, *signature, *key, Value::int(algo)];
    let answered = call(req, &function, &args);
    let out = match &answered {
        Ok(v) if v.as_int() == Some(1) => Some(sig.to_vec()),
        Ok(v) if v.as_bool() == Some(true) && is_sign => signature
            .as_php_ref()
            .and_then(|r| r.get().as_str_bytes().map(<[u8]>::to_vec)),
        _ => None,
    };
    for arg in [data, signature, key] {
        arg.give_up(req);
    }
    answered?.give_up(req);
    Ok(out)
}

/// `phar_create_signature`: the signature of `data` by the archive's
/// signature type, SHA-256 where it names none; the raw signature, or
/// php's message.
pub(crate) fn create(
    req: &mut Request,
    sig_flags: &mut u32,
    data: &[u8],
    fname: &[u8],
) -> Result<Result<Vec<u8>, String>, Thrown> {
    if let Some((name, _)) = algo(*sig_flags) {
        return Ok(Ok(digest(req, name, data)?));
    }
    if matches!(
        *sig_flags,
        SIG_OPENSSL | SIG_OPENSSL_SHA256 | SIG_OPENSSL_SHA512
    ) {
        let key = req
            .state::<crate::State>()
            .privatekey
            .clone()
            .unwrap_or_default();
        return Ok(
            match openssl_call(req, true, data, &key, b"", *sig_flags)? {
                Some(sig) => Ok(sig),
                None => Err(format!(
                    "unable to write phar \"{}\" with requested openssl signature",
                    archive::lossy(fname)
                )),
            },
        );
    }
    *sig_flags = SIG_SHA256;
    Ok(Ok(digest(req, b"sha256", data)?))
}
