//! `openssl_sign` and `openssl_verify` (`openssl.c`): a digest found by
//! name or by `OPENSSL_ALGO_*`, the key read as
//! `php_openssl_pkey_from_zval` reads it, and `EVP_DigestSign` or
//! `EVP_DigestVerify` under the padding `php_openssl_setup_rsa_padding`
//! allows.

use php_ext::abi::{Ref, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::{PhpString, Value};

use crate::config::c_string;
use crate::constants::{OPENSSL_PKCS1_PADDING, OPENSSL_PKCS1_PSS_PADDING};
use crate::ffi::{Digest, DigestContext, Direction, KeyKind, PKey};
use crate::key::{Arg, Half, key_from};
use crate::store_errors;

/// `php_openssl_get_evp_md_from_algo`: the digest an `OPENSSL_ALGO_*`
/// names, of those the library carries (no MD2 in OpenSSL 3).
fn digest_of(algo: i64) -> Option<Digest> {
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

/// `openssl_sign(string $data, &$signature, $private_key, string|int
/// $algorithm = OPENSSL_ALGO_SHA1, int $padding = 0): bool`: the
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
    let arg = Arg {
        position: 3,
        name: "private_key",
    };
    let Some(key) = key_from(req, &private_key.0, Half::Private, Some(b""), arg)? else {
        docref(
            req,
            Level::Warning,
            "Supplied key param cannot be coerced into a private key",
        );
        return Ok(false);
    };
    let Ok(digest) = digest_for(&algorithm) else {
        docref(req, Level::Warning, "Unknown digest algorithm");
        return Ok(false);
    };
    let signed = DigestContext::new(Direction::Sign, digest.as_ref(), &key).and_then(|mut ctx| {
        setup_padding(req, &mut ctx, &key, padding)
            .then(|| ctx.sign(data.as_bytes()))
            .flatten()
    });
    let Some(signed) = signed else {
        store_errors(req);
        return Ok(false);
    };
    let signed = PhpString::new(req, &signed)?;
    signature.set(req, Untyped(Value::string(signed)))?;
    Ok(true)
}

/// `openssl_verify(string $data, string $signature, $public_key,
/// string|int $algorithm = OPENSSL_ALGO_SHA1, int $padding = 0):
/// int|false`: 1 where the signature is the key's over the data, 0
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
) -> Result<php_ext::abi::OrFalse<i64>, Thrown> {
    if u32::try_from(signature.as_bytes().len()).is_err() {
        return Err(arg_value_error(req, 2, "is too long"));
    }
    let Ok(digest) = digest_for(&algorithm) else {
        docref(req, Level::Warning, "Unknown digest algorithm");
        return Ok(php_ext::abi::OrFalse::False);
    };
    let arg = Arg {
        position: 3,
        name: "public_key",
    };
    let Some(key) = key_from(req, &public_key.0, Half::Public, None, arg)? else {
        docref(
            req,
            Level::Warning,
            "Supplied key param cannot be coerced into a public key",
        );
        return Ok(php_ext::abi::OrFalse::False);
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
    Ok(php_ext::abi::OrFalse::Value(i64::from(verdict)))
}
