//! The `openssl_spki_*` natives of openssl.c: a Netscape SPKAC made from
//! a private key and a challenge, and one read back, verified, and its
//! public key and challenge exported.

use php_ext::abi::{Instance, OrFalse};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString, Value};

use crate::decode::{Arg, raised};
use crate::ffi::x509::Spki;
use crate::key::{Half, OpenSSLAsymmetricKey, check_int_len, key_from};
use crate::sign::digest_of;
use crate::store_errors;

/// One step of making an SPKAC, beside the warning its failure raises.
type Step<'a> = (&'static str, &'a dyn Fn(&mut Spki) -> bool);

/// `php_openssl_spki_cleanup`: the string as C reads it, its CR and LF
/// taken out; `None` for one left empty, after php's warning.
fn cleaned(req: &mut Request, spki: PhpString) -> Option<Vec<u8>> {
    let text = spki.as_bytes();
    let text = php_ext::coerce::c_str(text);
    let out: Vec<u8> = text
        .iter()
        .copied()
        .filter(|&b| b != b'\n' && b != b'\r')
        .collect();
    if out.is_empty() {
        docref(req, Level::Warning, "Invalid SPKAC");
        return None;
    }
    Some(out)
}

/// `openssl_spki_new(OpenSSLAsymmetricKey $private_key, string`
/// `$challenge, int $digest_algo = OPENSSL_ALGO_MD5): string|false`: an
/// SPKAC of the key's public half and the challenge, signed with the key,
/// as `SPKAC=` and its base64.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $s = openssl_spki_new($k, 'challenge', OPENSSL_ALGO_SHA256);
/// var_dump(substr($s, 0, 6), openssl_spki_verify(substr($s, 6)), openssl_spki_export_challenge(substr($s, 6)));
/// var_dump(openssl_spki_new($k, 'c', 99), openssl_spki_new(openssl_pkey_get_public(openssl_pkey_get_details($k)['key']), 'c'));
/// ```
/// ```expect
/// string(6) "SPKAC="
/// bool(true)
/// string(9) "challenge"
///
/// Warning: openssl_spki_new(): Unknown digest algorithm in %s on line 5
///
/// Warning: openssl_spki_new(): Supplied key param is a public key in %s on line 5
///
/// Warning: openssl_spki_new(): Unable to use supplied private key in %s on line 5
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_spki_new(
    req: &mut Request,
    #[php(sensitive)] private_key: Instance<OpenSSLAsymmetricKey>,
    challenge: PhpString,
    #[php(default = OPENSSL_ALGO_MD5)] digest_algo: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    check_int_len(req, challenge.as_bytes().len(), "challenge", 2)?;
    let object = Value::object(private_key.object());
    let arg = Arg::new(1, "private_key");
    let Some(key) = key_from(req, &object, Half::Private, Some(challenge.as_bytes()), arg)? else {
        if !raised(req) {
            docref(req, Level::Warning, "Unable to use supplied private key");
        }
        return Ok(OrFalse::False);
    };
    let Some(md) = digest_of(digest_algo) else {
        docref(req, Level::Warning, "Unknown digest algorithm");
        return Ok(OrFalse::False);
    };
    let steps: [Step<'_>; 3] = [
        ("Unable to set challenge data", &|spki| {
            spki.set_challenge(challenge.as_bytes())
        }),
        ("Unable to embed public key", &|spki| {
            spki.set_public_key(&key)
        }),
        ("Unable to sign with specified digest algorithm", &|spki| {
            spki.sign(&key, &md)
        }),
    ];
    let Some(mut spki) = Spki::new() else {
        store_errors(req);
        docref(req, Level::Warning, "Unable to create new SPKAC");
        return Ok(OrFalse::False);
    };
    for (failure, step) in steps {
        if !step(&mut spki) {
            store_errors(req);
            docref(req, Level::Warning, failure);
            return Ok(OrFalse::False);
        }
    }
    let Some(encoded) = spki.encode() else {
        store_errors(req);
        docref(req, Level::Warning, "Unable to encode SPKAC");
        return Ok(OrFalse::False);
    };
    let mut out = b"SPKAC=".to_vec();
    out.extend_from_slice(&encoded);
    Ok(OrFalse::Value(PhpString::new(req, &out)?))
}

/// The SPKAC a string decodes to, after php's warnings for one that does
/// not, and its public key where `key` asks for it.
fn decoded(req: &mut Request, spki: PhpString, failure: &str) -> Option<Spki> {
    let text = cleaned(req, spki)?;
    let decoded = Spki::decode(&text);
    if decoded.is_none() {
        store_errors(req);
        docref(req, Level::Warning, failure);
    }
    decoded
}

/// The public key an SPKAC signs, after php's warning where there is
/// none.
fn signed_key(req: &mut Request, spki: &Spki) -> Option<crate::ffi::PKey> {
    let key = spki.public_key();
    if key.is_none() {
        store_errors(req);
        docref(req, Level::Warning, "Unable to acquire signed public key");
    }
    key
}

/// `openssl_spki_verify(string $spki): bool`: whether the SPKAC's
/// signature is its public key's.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $s = substr(openssl_spki_new($k, 'c'), 6);
/// var_dump(openssl_spki_verify($s), openssl_spki_verify(chunk_split($s, 20)), openssl_spki_verify(''), openssl_spki_verify('nope'));
/// ```
/// ```expect
/// Warning: openssl_spki_verify(): Invalid SPKAC in %s on line 4
///
/// Warning: openssl_spki_verify(): Unable to decode supplied SPKAC in %s on line 4
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_spki_verify(req: &mut Request, spki: PhpString) -> bool {
    let verified = decoded(req, spki, "Unable to decode supplied SPKAC")
        .and_then(|decoded| signed_key(req, &decoded).map(|key| decoded.verify(&key) > 0));
    if verified == Some(true) {
        return true;
    }
    store_errors(req);
    false
}

/// `openssl_spki_export(string $spki): string|false`: the SPKAC's public
/// key as PEM.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $s = substr(openssl_spki_new($k, 'c'), 6);
/// var_dump(openssl_spki_export($s) === openssl_pkey_get_details($k)['key'], openssl_spki_export('nope'));
/// ```
/// ```expect
/// Warning: openssl_spki_export(): Unable to decode supplied SPKAC in %s on line 4
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn openssl_spki_export(
    req: &mut Request,
    spki: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Some(decoded) = decoded(req, spki, "Unable to decode supplied SPKAC") else {
        return Ok(OrFalse::False);
    };
    let Some(key) = signed_key(req, &decoded) else {
        return Ok(OrFalse::False);
    };
    let Some(pem) = key.public_pem() else {
        store_errors(req);
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(PhpString::new(req, &pem)?))
}

/// `openssl_spki_export_challenge(string $spki): string|false`: the
/// SPKAC's challenge.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// var_dump(openssl_spki_export_challenge(substr(openssl_spki_new($k, 'my challenge'), 6)), openssl_spki_export_challenge('nope'), openssl_spki_export_challenge("\n"));
/// ```
/// ```expect
/// Warning: openssl_spki_export_challenge(): Unable to decode SPKAC in %s on line 3
///
/// Warning: openssl_spki_export_challenge(): Invalid SPKAC in %s on line 3
/// string(12) "my challenge"
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_spki_export_challenge(
    req: &mut Request,
    spki: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Some(decoded) = decoded(req, spki, "Unable to decode SPKAC") else {
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(PhpString::new(req, &decoded.challenge())?))
}
