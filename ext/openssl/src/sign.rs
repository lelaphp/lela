//! `openssl_sign` and `openssl_verify` (`openssl.c`): a digest found by
//! name or by `OPENSSL_ALGO_*`, the key read as
//! `php_openssl_pkey_from_zval` reads it, and `EVP_DigestSign` or
//! `EVP_DigestVerify` under the padding `php_openssl_setup_rsa_padding`
//! allows; and the raw RSA operations `openssl_private_encrypt`,
//! `openssl_private_decrypt`, `openssl_public_encrypt` and
//! `openssl_public_decrypt` over `EVP_PKEY_CTX`.

use std::ffi::c_int;

use php_ext::abi::{OrFalse, Ref, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, argument_error, docref};
use php_ext::request::Request;
use php_ext::values::{PhpString, Value};

use crate::constants::{
    OPENSSL_PKCS1_OAEP_PADDING, OPENSSL_PKCS1_PADDING, OPENSSL_PKCS1_PSS_PADDING,
};
use crate::decode::{Arg, c_string, finish, raised};
use crate::ffi::key::{DigestContext, Direction, KeyContext, Operation};
use crate::ffi::{Digest, KeyKind, PKey};
use crate::key::{Half, key_from};
use crate::store_errors;
use crate::throwables::Throwables;

/// `php_openssl_get_evp_md_from_algo`: the digest an `OPENSSL_ALGO_*`
/// names, of those the library carries (no MD2 in OpenSSL 3).
pub(crate) fn digest_of(algo: i64) -> Option<Digest> {
    let name = match algo {
        1 => c"SHA1",
        2 => c"MD5",
        3 => c"MD4",
        6 => c"SHA224",
        7 => c"SHA256",
        8 => c"SHA384",
        9 => c"SHA512",
        10 => c"RIPEMD160",
        _ => return None,
    };
    Digest::by_name(name)
}

/// The `$algorithm` a signing native was handed: a digest by name, or an
/// `OPENSSL_ALGO_*`. `Ok(None)` is OpenSSL 3's default digest, which
/// `0` asks for; `Err(())` is php's `Unknown digest algorithm`.
fn digest_for(algorithm: &Union<PhpString, i64>) -> Result<Option<Digest>, ()> {
    match algorithm {
        Union::A(name) => Digest::by_name(&c_string(name.as_bytes()))
            .map(Some)
            .ok_or(()),
        Union::B(algo) => match digest_of(*algo) {
            Some(digest) => Ok(Some(digest)),
            None if *algo == 0 => Ok(None),
            None => Err(()),
        },
    }
}

/// `php_openssl_setup_rsa_padding`: `0` leaves the library's default;
/// any other asks for an RSA key, and then `OPENSSL_PKCS1_PSS_PADDING`
/// sets PSS and `OPENSSL_PKCS1_PADDING` the default again. False after
/// php's warning or with the library's reasons stored.
fn setup_padding(req: &mut Request, ctx: &mut DigestContext, key: &PKey, padding: i64) -> bool {
    if padding == 0 {
        return true;
    }
    if key.legacy_kind() != KeyKind::Rsa {
        docref(
            req,
            Level::Warning,
            "Padding parameter is only supported for RSA keys",
        );
        return false;
    }
    if padding == OPENSSL_PKCS1_PSS_PADDING {
        if !ctx.set_pss_padding() {
            store_errors(req);
            return false;
        }
        return true;
    }
    if padding != OPENSSL_PKCS1_PADDING {
        docref(req, Level::Warning, "Unknown padding type");
        return false;
    }
    true
}

/// `openssl_sign(string $data, &$signature, $private_key, string|int`
/// `$algorithm = OPENSSL_ALGO_SHA1, int $padding = 0): bool`: the
/// signature over the data, written to `$signature`.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $pub = openssl_pkey_get_details($k)['key'];
/// foreach ([OPENSSL_ALGO_SHA1, OPENSSL_ALGO_SHA256, OPENSSL_ALGO_SHA512, 'sha384', 0] as $algo) {
///     var_dump(openssl_sign('data', $sig, $k, $algo), strlen($sig), openssl_verify('data', $sig, $pub, $algo));
/// }
/// var_dump(openssl_sign('data', $sig, $k, OPENSSL_ALGO_SHA256, OPENSSL_PKCS1_PSS_PADDING), openssl_verify('data', $sig, $pub, OPENSSL_ALGO_SHA256, OPENSSL_PKCS1_PSS_PADDING), openssl_verify('data', $sig, $pub, OPENSSL_ALGO_SHA256));
/// var_dump(openssl_sign('data', $sig, $k, 42));
/// var_dump(openssl_sign('data', $sig, $k, 'nope'));
/// var_dump(openssl_sign('data', $sig, $k, OPENSSL_ALGO_SHA1, 42));
/// var_dump(openssl_sign('data', $sig, 'nope'));
/// ```
/// ```expect
/// bool(true)
/// int(128)
/// int(1)
/// bool(true)
/// int(128)
/// int(1)
/// bool(true)
/// int(128)
/// int(1)
/// bool(true)
/// int(128)
/// int(1)
/// bool(true)
/// int(128)
/// int(1)
/// bool(true)
/// int(1)
/// int(0)
///
/// Warning: openssl_sign(): Unknown digest algorithm in %s on line 8
/// bool(false)
///
/// Warning: openssl_sign(): Unknown digest algorithm in %s on line 9
/// bool(false)
///
/// Warning: openssl_sign(): Unknown padding type in %s on line 10
/// bool(false)
///
/// Warning: openssl_sign(): Supplied key param cannot be coerced into a private key in %s on line 11
/// bool(false)
/// ```
#[php_function]
fn openssl_sign(
    req: &mut Request,
    data: PhpString,
    signature: Ref<Untyped<Value>>,
    #[php(sensitive)] private_key: Untyped<Value>,
    #[php(default = OPENSSL_ALGO_SHA1)] algorithm: Union<PhpString, i64>,
    #[php(default = 0)] padding: i64,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let arg = Arg::new(3, "private_key");
        let Some(key) = key_from(req, &private_key.0, Half::Private, Some(b""), arg)? else {
            if !raised(req) {
                docref(
                    req,
                    Level::Warning,
                    "Supplied key param cannot be coerced into a private key",
                );
            }
            return Ok(false);
        };
        let Ok(digest) = digest_for(&algorithm) else {
            docref(req, Level::Warning, "Unknown digest algorithm");
            return Ok(false);
        };
        let signed =
            DigestContext::new(Direction::Sign, digest.as_ref(), &key).and_then(|mut ctx| {
                setup_padding(req, &mut ctx, &key, padding)
                    .then(|| ctx.sign(data.as_bytes()))
                    .flatten()
            });
        let Some(signed) = signed else {
            store_errors(req);
            return Ok(false);
        };
        let signed = PhpString::new(req, &signed)?;
        signature.set(req, Untyped(signed.into_value()))?;
        Ok(true)
    })
}

/// `openssl_verify(string $data, string $signature, $public_key,`
/// `string|int $algorithm = OPENSSL_ALGO_SHA1, int $padding = 0):`
/// `int|false`: 1 where the signature is the key's over the data, 0
/// where it is not, -1 for an error, false for a digest or key that
/// cannot be had.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $pub = openssl_pkey_get_details($k)['key'];
/// openssl_sign('data', $sig, $k);
/// var_dump(openssl_verify('data', $sig, $pub), openssl_verify('other', $sig, $pub), openssl_verify('data', 'short', $pub));
/// var_dump(openssl_verify('data', $sig, openssl_pkey_get_public($pub)), openssl_verify('data', $sig, $k));
/// var_dump(openssl_verify('data', $sig, $pub, 42));
/// var_dump(openssl_verify('data', $sig, 'nope'));
/// var_dump(openssl_verify('data', $sig, $pub, OPENSSL_ALGO_SHA1, 42));
/// ```
/// ```expect
/// int(1)
/// int(0)
/// int(0)
///
/// Warning: openssl_verify(): Don't know how to get public key from this private key in %s on line 6
///
/// Warning: openssl_verify(): Supplied key param cannot be coerced into a public key in %s on line 6
/// int(1)
/// bool(false)
///
/// Warning: openssl_verify(): Unknown digest algorithm in %s on line 7
/// bool(false)
///
/// Warning: openssl_verify(): Supplied key param cannot be coerced into a public key in %s on line 8
/// bool(false)
///
/// Warning: openssl_verify(): Unknown padding type in %s on line 9
/// int(-1)
/// ```
#[php_function]
fn openssl_verify(
    req: &mut Request,
    data: PhpString,
    signature: PhpString,
    public_key: Untyped<Value>,
    #[php(default = OPENSSL_ALGO_SHA1)] algorithm: Union<PhpString, i64>,
    #[php(default = 0)] padding: i64,
) -> Result<OrFalse<i64>, Thrown> {
    finish(req, move |req| {
        if u32::try_from(signature.as_bytes().len()).is_err() {
            let ce = Throwables::of(req)?;
            return Err(argument_error(
                req,
                ce.value_error,
                2,
                "signature is too long",
            ));
        }
        let Ok(digest) = digest_for(&algorithm) else {
            docref(req, Level::Warning, "Unknown digest algorithm");
            return Ok(OrFalse::False);
        };
        let arg = Arg::new(3, "public_key");
        let Some(key) = key_from(req, &public_key.0, Half::Public, None, arg)? else {
            if !raised(req) {
                docref(
                    req,
                    Level::Warning,
                    "Supplied key param cannot be coerced into a public key",
                );
            }
            return Ok(OrFalse::False);
        };
        let ready = DigestContext::new(Direction::Verify, digest.as_ref(), &key)
            .and_then(|mut ctx| setup_padding(req, &mut ctx, &key, padding).then_some(ctx));
        let verdict = if let Some(mut ctx) = ready {
            ctx.verify(signature.as_bytes(), data.as_bytes())
        } else {
            -1
        };
        if verdict < 0 {
            store_errors(req);
        }
        Ok(OrFalse::Value(i64::from(verdict)))
    })
}

/// One raw RSA operation as the four natives make it: `EVP_PKEY_CTX_new`,
/// the operation's init, the padding (and for encryption and decryption
/// the OAEP digest `php_openssl_set_rsa_padding_and_digest` sets), then
/// the two-call operation; `None` with the library's reasons stored.
fn rsa_operation(
    req: &mut Request,
    key: &PKey,
    operation: Operation,
    padding: i64,
    digest_algo: Option<&PhpString>,
    data: &[u8],
) -> Option<Vec<u8>> {
    let padding = c_int::try_from(padding).unwrap_or(c_int::MAX);
    let out = (|| {
        let mut ctx = KeyContext::of_key(key)?;
        if !ctx.init(operation) || !ctx.set_rsa_padding(padding) {
            return None;
        }
        let digest = match digest_algo {
            Some(name) => {
                let Some(md) = Digest::by_name(&c_string(name.as_bytes())) else {
                    let text = format!(
                        "Unknown digest algorithm: {}",
                        String::from_utf8_lossy(c_string(name.as_bytes()).as_bytes())
                    );
                    docref(req, Level::Warning, &text);
                    return None;
                };
                Some(md)
            }
            None => None,
        };
        if let Some(md) = &digest
            && i64::from(padding) == OPENSSL_PKCS1_OAEP_PADDING
            && !ctx.set_oaep_md(md)
        {
            return None;
        }
        ctx.apply(operation, data, 0)
    })();
    if out.is_none() {
        store_errors(req);
    }
    out
}

/// The bytes of a raw RSA operation written to `$out`, or false.
fn written(
    req: &mut Request,
    out: &Ref<Untyped<Value>>,
    bytes: Option<Vec<u8>>,
) -> Result<bool, Thrown> {
    let Some(bytes) = bytes else {
        return Ok(false);
    };
    let bytes = PhpString::new(req, &bytes)?;
    out.set(req, Untyped(bytes.into_value()))?;
    Ok(true)
}

/// `openssl_private_encrypt(string $data, &$encrypted_data, $private_key,`
/// `int $padding = OPENSSL_PKCS1_PADDING): bool`: `EVP_PKEY_sign` of the
/// data itself, what `openssl_public_decrypt` recovers.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $pub = openssl_pkey_get_details($k)['key'];
/// var_dump(openssl_private_encrypt('data', $c, $k), strlen($c), openssl_public_decrypt($c, $d, $pub), $d);
/// var_dump(openssl_private_encrypt('data', $c, 'nope'), openssl_public_decrypt('bad', $d, $pub), openssl_public_decrypt($c, $d, 'nope'));
/// ```
/// ```expect
/// bool(true)
/// int(128)
/// bool(true)
/// string(4) "data"
///
/// Warning: openssl_private_encrypt(): key param is not a valid private key in %s on line 5
///
/// Warning: openssl_public_decrypt(): key parameter is not a valid public key in %s on line 5
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_private_encrypt(
    req: &mut Request,
    #[php(sensitive)] data: PhpString,
    encrypted_data: Ref<Untyped<Value>>,
    #[php(sensitive)] private_key: Untyped<Value>,
    #[php(default = OPENSSL_PKCS1_PADDING)] padding: i64,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let arg = Arg::new(3, "private_key");
        let Some(key) = key_from(req, &private_key.0, Half::Private, Some(b""), arg)? else {
            if !raised(req) {
                docref(req, Level::Warning, "key param is not a valid private key");
            }
            return Ok(false);
        };
        let out = rsa_operation(req, &key, Operation::Sign, padding, None, data.as_bytes());
        written(req, &encrypted_data, out)
    })
}

/// `openssl_private_decrypt(string $data, &$decrypted_data,`
/// `$private_key, int $padding = OPENSSL_PKCS1_PADDING, ?string`
/// `$digest_algo = null): bool`: `EVP_PKEY_decrypt` of what
/// `openssl_public_encrypt` made, OAEP under the digest named.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $pub = openssl_pkey_get_details($k)['key'];
/// var_dump(openssl_public_encrypt('data', $c, $pub, OPENSSL_PKCS1_OAEP_PADDING, 'sha256'), openssl_private_decrypt($c, $d, $k, OPENSSL_PKCS1_OAEP_PADDING, 'sha256'), $d);
/// var_dump(openssl_private_decrypt($c, $d, $k, OPENSSL_PKCS1_OAEP_PADDING, 'nope'), openssl_private_decrypt($c, $d, 'nope'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(4) "data"
///
/// Warning: openssl_private_decrypt(): Unknown digest algorithm: nope in %s on line 5
///
/// Warning: openssl_private_decrypt(): key parameter is not a valid private key in %s on line 5
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_private_decrypt(
    req: &mut Request,
    data: PhpString,
    #[php(sensitive)] decrypted_data: Ref<Untyped<Value>>,
    #[php(sensitive)] private_key: Untyped<Value>,
    #[php(default = OPENSSL_PKCS1_PADDING)] padding: i64,
    #[php(default = null)] digest_algo: Option<php_ext::abi::Path>,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let arg = Arg::new(3, "private_key");
        let Some(key) = key_from(req, &private_key.0, Half::Private, Some(b""), arg)? else {
            if !raised(req) {
                docref(
                    req,
                    Level::Warning,
                    "key parameter is not a valid private key",
                );
            }
            return Ok(false);
        };
        let digest = digest_algo.as_ref().map(|p| &p.0);
        let out = rsa_operation(
            req,
            &key,
            Operation::Decrypt,
            padding,
            digest,
            data.as_bytes(),
        );
        written(req, &decrypted_data, out)
    })
}

/// `openssl_public_encrypt(string $data, &$encrypted_data, $public_key,`
/// `int $padding = OPENSSL_PKCS1_PADDING, ?string $digest_algo = null):`
/// `bool`: `EVP_PKEY_encrypt`, OAEP under the digest named.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $pub = openssl_pkey_get_details($k)['key'];
/// var_dump(openssl_public_encrypt('data', $c, $pub), strlen($c), openssl_private_decrypt($c, $d, $k), $d);
/// var_dump(openssl_public_encrypt('data', $c, 'nope'), openssl_public_encrypt(str_repeat('x', 200), $c, $pub));
/// ```
/// ```expect
/// bool(true)
/// int(128)
/// bool(true)
/// string(4) "data"
///
/// Warning: openssl_public_encrypt(): key parameter is not a valid public key in %s on line 5
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_public_encrypt(
    req: &mut Request,
    #[php(sensitive)] data: PhpString,
    encrypted_data: Ref<Untyped<Value>>,
    public_key: Untyped<Value>,
    #[php(default = OPENSSL_PKCS1_PADDING)] padding: i64,
    #[php(default = null)] digest_algo: Option<php_ext::abi::Path>,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let arg = Arg::new(3, "public_key");
        let Some(key) = key_from(req, &public_key.0, Half::Public, None, arg)? else {
            if !raised(req) {
                docref(
                    req,
                    Level::Warning,
                    "key parameter is not a valid public key",
                );
            }
            return Ok(false);
        };
        let digest = digest_algo.as_ref().map(|p| &p.0);
        let out = rsa_operation(
            req,
            &key,
            Operation::Encrypt,
            padding,
            digest,
            data.as_bytes(),
        );
        written(req, &encrypted_data, out)
    })
}

/// `openssl_public_decrypt(string $data, &$decrypted_data, $public_key,`
/// `int $padding = OPENSSL_PKCS1_PADDING): bool`:
/// `EVP_PKEY_verify_recover`, the data `openssl_private_encrypt` signed.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// openssl_private_encrypt('data', $c, $k);
/// var_dump(openssl_public_decrypt($c, $d, openssl_pkey_get_details($k)['key']), $d);
/// ```
/// ```expect
/// bool(true)
/// string(4) "data"
/// ```
#[php_function]
fn openssl_public_decrypt(
    req: &mut Request,
    data: PhpString,
    #[php(sensitive)] decrypted_data: Ref<Untyped<Value>>,
    public_key: Untyped<Value>,
    #[php(default = OPENSSL_PKCS1_PADDING)] padding: i64,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let arg = Arg::new(3, "public_key");
        let Some(key) = key_from(req, &public_key.0, Half::Public, None, arg)? else {
            if !raised(req) {
                docref(
                    req,
                    Level::Warning,
                    "key parameter is not a valid public key",
                );
            }
            return Ok(false);
        };
        let out = rsa_operation(
            req,
            &key,
            Operation::VerifyRecover,
            padding,
            None,
            data.as_bytes(),
        );
        written(req, &decrypted_data, out)
    })
}
