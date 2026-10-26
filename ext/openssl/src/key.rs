//! `OpenSSLAsymmetricKey` and the natives that make, read, write and
//! combine one: `php_openssl_pkey_object`, `php_openssl_pkey_from_zval`
//! (a key, a certificate, a PEM string, a `file://` path, or an array of
//! one and its passphrase) and the `openssl_pkey_*`,
//! `openssl_dh_compute_key` and `openssl_get_curve_names` natives of
//! `openssl.c`.

use std::ffi::CString;

use php_ext::abi::{Instance, OrFalse, Passed, Path, Ref, Refusals, Untyped};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::with_string;
use php_ext::raising::{Level, Thrown, arg_value_error, docref, fatal, pending, value_error};
use php_ext::request::Request;
use php_ext::values::{Key as ArrayKey, Owner, PhpArray, PhpString, Value, ValueRef};

use crate::constants::{
    OPENSSL_KEYTYPE_DH, OPENSSL_KEYTYPE_DSA, OPENSSL_KEYTYPE_EC, OPENSSL_KEYTYPE_ED448,
    OPENSSL_KEYTYPE_ED25519, OPENSSL_KEYTYPE_RSA, OPENSSL_KEYTYPE_X448, OPENSSL_KEYTYPE_X25519,
};
use crate::decode::{Arg, Origin, c_string, cert_from_str, check_path, defer, finish, raised};
use crate::ffi::key::{KeyContext, Operation};
use crate::ffi::{self, Bio, Cipher, KeyKind, PKey};
use crate::throwables::Throwables;
use crate::{config, errors_restore_mark, errors_set_mark, keydata, put, put_bytes, store_errors};

/// A `php_openssl_pkey_object`: the key, and whether it was made as a
/// private one, which is what the natives that take a key check.
#[derive(Default)]
pub(crate) struct Key {
    pkey: Option<PKey>,
    private: bool,
}

/// `final class OpenSSLAsymmetricKey`, `@strict-properties`,
/// `@not-serializable`, under [`HANDLERS`].
///
/// ```php
/// try { new OpenSSLAsymmetricKey; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $k = openssl_pkey_new(['private_key_bits' => 512]);
/// try { clone $k; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($k); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { $k->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($k == openssl_pkey_new(['private_key_bits' => 512]));
/// var_dump($k);
/// ```
/// ```expect
/// Cannot directly construct OpenSSLAsymmetricKey, use openssl_pkey_new() instead
/// Trying to clone an uncloneable object of class OpenSSLAsymmetricKey
/// Serialization of 'OpenSSLAsymmetricKey' is not allowed
/// Cannot create dynamic property OpenSSLAsymmetricKey::$x
/// bool(false)
/// object(OpenSSLAsymmetricKey)#1 (0) {
/// }
/// ```
///
/// `php_openssl_pkey_create_object` allocates a `php_openssl_pkey_object`,
/// which `memory_get_usage()` counts (design §5.13):
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024, 'private_key_type' => OPENSSL_KEYTYPE_RSA]);
/// openssl_pkey_get_private($k);
/// $m = memory_get_usage();
/// $p = openssl_pkey_get_private($k);
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 56
/// ```
#[php_class(
    final,
    not_serializable,
    size = 72, payload = Key,
    handlers = HANDLERS,
    dynamic_properties = forbidden
)]
pub(crate) struct OpenSSLAsymmetricKey;

#[php_class]
impl OpenSSLAsymmetricKey {}

/// `php_openssl_pkey_object_handlers`.
pub(crate) const HANDLERS: Refusals = crate::refusals("openssl_pkey_new()");

/// `php_openssl_pkey_object_init`: a new `OpenSSLAsymmetricKey` holding
/// the key.
pub(crate) fn object_of(
    req: &mut Request,
    pkey: PKey,
    private: bool,
) -> Result<Owner<Instance<OpenSSLAsymmetricKey>>, Thrown> {
    let Some(class) = lookup_class(req, b"OpenSSLAsymmetricKey")? else {
        return Err(fatal(
            req,
            "InternalError: OpenSSLAsymmetricKey is not declared",
        ));
    };
    let object = new_object(req, class)?;
    if let Some(payload) = object.native_mut::<Key>(req) {
        payload.pkey = Some(pkey);
        payload.private = private;
    }
    Ok(object.into_instance())
}

/// The key an `OpenSSLAsymmetricKey` holds, and whether it is private.
pub(crate) fn held(
    req: &mut Request,
    key: Instance<OpenSSLAsymmetricKey>,
) -> Result<(PKey, bool), Thrown> {
    let payload = key.payload_mut(req)?;
    let private = payload.private;
    match payload.pkey.clone() {
        Some(pkey) => Ok((pkey, private)),
        None => Err(fatal(
            req,
            "InternalError: an OpenSSLAsymmetricKey holds no key",
        )),
    }
}

/// Which half of a key pair a native asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Half {
    Private,
    Public,
}

/// `PHP_OPENSSL_CHECK_SIZE_T_TO_INT`: a length past `INT_MAX` refused
/// with php's `ValueError`, `Argument #N ($x) {what} is too long`.
pub(crate) fn check_int_len(
    req: &mut Request,
    len: usize,
    what: &str,
    arg: u32,
) -> Result<(), Thrown> {
    if i32::try_from(len).is_err() {
        let tail = format!("{what} is too long");
        let ce = Throwables::of(req)?;
        return Err(php_ext::raising::argument_error(
            req,
            ce.value_error,
            arg,
            &tail,
        ));
    }
    Ok(())
}

/// `php_openssl_pkey_from_zval`: the key a parameter gives. An array is
/// `[key, passphrase]`; an `OpenSSLAsymmetricKey` must be the half asked
/// for; an `OpenSSLCertificate` gives its public key; anything else is
/// read as a string, a `file://` path or the PEM itself, a certificate's
/// key where the public half is asked for and the string is one. `None`
/// where there is no key, after the warnings php raises and with the
/// library's reasons stored.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 512]);
/// openssl_pkey_export($k, $pem, 'secret');
/// var_dump(openssl_pkey_get_private($pem) === false, openssl_pkey_get_private([$pem, 'secret']) instanceof OpenSSLAsymmetricKey);
/// try { openssl_pkey_get_private([$pem]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// $pub = openssl_pkey_get_details($k)['key'];
/// var_dump(openssl_pkey_get_public($k));
/// var_dump(openssl_sign('x', $sig, openssl_pkey_get_public($pub)));
/// var_dump(openssl_pkey_get_public(42));
/// try { openssl_pkey_get_private("file://a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// var_dump(openssl_pkey_get_details(openssl_pkey_get_public($x))['key'] === $pub, openssl_pkey_get_private($x));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// Key array must be of the form array(0 => key, 1 => phrase)
///
/// Warning: openssl_pkey_get_public(): Don't know how to get public key from this private key in %s on line 7
/// bool(false)
///
/// Warning: openssl_sign(): Supplied key param is a public key in %s on line 8
///
/// Warning: openssl_sign(): Supplied key param cannot be coerced into a private key in %s on line 8
/// bool(false)
/// bool(false)
/// openssl_pkey_get_private(): Argument #1 ($private_key) must not contain any null bytes
/// bool(true)
/// bool(false)
/// ```
pub(crate) fn key_from(
    req: &mut Request,
    value: &Value,
    half: Half,
    passphrase: Option<&[u8]>,
    arg: Arg,
) -> Result<Option<PKey>, Thrown> {
    let mut phrase = passphrase.map(<[u8]>::to_vec);
    let mut value = *value;
    if let ValueRef::Array(array) = value.as_ref() {
        let form = "Key array must be of the form array(0 => key, 1 => phrase)";
        let Some(given) = array.get(&ArrayKey::Int(1)) else {
            let thrown = value_error(req, form);
            defer(req, thrown);
            return Ok(None);
        };
        phrase = Some(match given.as_ref() {
            ValueRef::Str(s) => s.as_bytes().to_vec(),
            _ => with_string(req, &given, <[u8]>::to_vec)?,
        });
        let Some(inner) = array.get(&ArrayKey::Int(0)) else {
            let thrown = value_error(req, form);
            defer(req, thrown);
            return Ok(None);
        };
        value = inner;
    }

    let mut cert = None;
    let mut key = None;
    if let ValueRef::Object(object) = value.as_ref()
        && let Some(held) = object.native::<Key>(req)
    {
        let private = held.private;
        let pkey = held.pkey.clone();
        if half == Half::Private && !private {
            docref(req, Level::Warning, "Supplied key param is a public key");
            return Ok(None);
        }
        if half == Half::Public && private {
            docref(
                req,
                Level::Warning,
                "Don't know how to get public key from this private key",
            );
            return Ok(None);
        }
        return Ok(pkey);
    } else if let ValueRef::Object(object) = value.as_ref()
        && let Some(held) = object.native::<crate::x509::Cert>(req)
    {
        cert.clone_from(&held.cert);
    } else {
        if !matches!(value.as_ref(), ValueRef::Str(_) | ValueRef::Object(_)) {
            return Ok(None);
        }
        let text = with_string(req, &value, <[u8]>::to_vec)?;
        let text = text.as_slice();
        let mut file: Option<CString> = None;
        if text.len() > 7 && text.starts_with(b"file://") {
            let Some(path) = check_path(req, text, Origin::Arg(arg), true) else {
                return Ok(None);
            };
            file = Some(path);
        }
        let bio = |file: &Option<CString>| match file {
            Some(path) => Bio::file(path, c"r"),
            None => Bio::reading(text),
        };
        match half {
            Half::Public => {
                errors_set_mark(req);
                cert = cert_from_str(req, text, Origin::Arg(arg));
                if cert.is_none() {
                    errors_restore_mark(req);
                    let Some(bio) = bio(&file) else {
                        store_errors(req);
                        return Ok(None);
                    };
                    key = PKey::read_public(&bio);
                }
            }
            Half::Private => {
                let Some(bio) = bio(&file) else {
                    return Ok(None);
                };
                key = PKey::read_private(&bio, phrase.as_deref());
            }
        }
    }
    if key.is_some() {
        return Ok(key);
    }
    store_errors(req);
    if half == Half::Public
        && let Some(cert) = &cert
    {
        key = cert.public_key();
        if key.is_none() {
            store_errors(req);
        }
    }
    Ok(key)
}

/// `openssl_pkey_new(?array $options = null): OpenSSLAsymmetricKey|false`:
/// a new private key of the type and size the options and the
/// configuration say, RSA of the configuration's `default_bits` unless
/// told otherwise; or, where the options hold an `rsa`, `dsa`, `dh`,
/// `ec`, `x25519`, `ed25519`, `x448` or `ed448` array, the key its
/// parameters make.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 512]);
/// var_dump($k instanceof OpenSSLAsymmetricKey, openssl_pkey_get_details($k)['bits']);
/// $d = openssl_pkey_get_details(openssl_pkey_new(['private_key_type' => OPENSSL_KEYTYPE_EC, 'curve_name' => 'prime256v1']));
/// var_dump($d['type'] === OPENSSL_KEYTYPE_EC, $d['ec']['curve_name'], $d['ec']['curve_oid']);
/// var_dump(openssl_pkey_new(['private_key_bits' => 128]));
/// var_dump(openssl_pkey_new(['private_key_type' => 42]));
/// var_dump(openssl_pkey_new(['private_key_type' => OPENSSL_KEYTYPE_EC]));
/// $r = openssl_pkey_get_details($k)['rsa'];
/// var_dump(openssl_pkey_get_details(openssl_pkey_new(['rsa' => ['n' => $r['n'], 'e' => $r['e'], 'd' => $r['d']]]))['key'] === openssl_pkey_get_details($k)['key']);
/// var_dump(openssl_pkey_new(['rsa' => ['n' => $r['n']]]));
/// $e = openssl_pkey_get_details(openssl_pkey_new(['ec' => ['curve_name' => 'prime256v1']]));
/// var_dump($e['bits'], strlen($e['ec']['d']));
/// var_dump(openssl_pkey_new(['ec' => ['curve_name' => 'nope']]), openssl_pkey_new(['ec' => []]));
/// $x = openssl_pkey_get_details(openssl_pkey_new(['x25519' => []]));
/// var_dump($x['type'] === OPENSSL_KEYTYPE_X25519, strlen($x['x25519']['pub_key']));
/// var_dump(openssl_pkey_get_details(openssl_pkey_new(['dh' => ['p' => "\xff\xff\xff\xff\xff\xff\xff\xc5", 'g' => "\x02", 'priv_key' => "\x03"]]))['dh']['pub_key'] === "\x08");
/// ```
/// ```expect
/// bool(true)
/// int(512)
/// bool(true)
/// string(10) "prime256v1"
/// string(19) "1.2.840.10045.3.1.7"
///
/// Warning: openssl_pkey_new(): Private key length must be at least 384 bits, configured to 128 in %s on line 6
/// bool(false)
///
/// Warning: openssl_pkey_new(): Unsupported private key type in %s on line 7
/// bool(false)
///
/// Warning: openssl_pkey_new(): Missing configuration value: "curve_name" not set in %s on line 8
/// bool(false)
/// bool(true)
/// bool(false)
/// int(256)
/// int(32)
///
/// Warning: openssl_pkey_new(): Unknown elliptic curve (short) name nope in %s on line 14
///
/// Warning: openssl_pkey_new(): Missing params: curve_name in %s on line 14
/// bool(false)
/// bool(false)
/// bool(true)
/// int(32)
/// bool(true)
/// ```
#[php_function]
fn openssl_pkey_new(
    req: &mut Request,
    #[php(default = null)] options: Option<PhpArray>,
) -> Result<OrFalse<Owner<Instance<OpenSSLAsymmetricKey>>>, Thrown> {
    if let Some(array) = options
        && let Some((kind, data)) = keydata::entry(array)
    {
        let made = match kind {
            KeyKind::Rsa => keydata::rsa(req, data).map(|key| (key, true)),
            KeyKind::Dsa | KeyKind::Dh => keydata::ffc(req, kind, data),
            KeyKind::Ec => keydata::ec(req, data),
            _ => keydata::curve(req, kind, data),
        };
        return Ok(match made {
            Some((key, private)) => OrFalse::Value(object_of(req, key, private)?),
            None => OrFalse::False,
        });
    }
    let Some(settings) = config::parse(req, options) else {
        return Ok(OrFalse::False);
    };
    let Some(key) = settings.generate(req) else {
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(object_of(req, key, true)?))
}

/// The private key as PEM into `out`, encrypted under the passphrase
/// with the options' cipher (`DES-EDE3-CBC` unless named) where one is
/// given and the configuration encrypts keys.
fn write_private(
    req: &mut Request,
    pkey: &PKey,
    settings: &config::Settings,
    phrase: Option<&[u8]>,
    out: &Bio<'_>,
) -> bool {
    let fallback;
    let encryption = match phrase {
        Some(phrase) if settings.encrypt => {
            let cipher = if let Some(cipher) = &settings.cipher {
                Some(cipher)
            } else {
                fallback = Cipher::des_ede3_cbc();
                fallback.as_ref()
            };
            cipher.map(|cipher| (cipher, phrase))
        }
        _ => None,
    };
    if pkey.write_private(out, encryption) {
        return true;
    }
    store_errors(req);
    false
}

/// `openssl_pkey_export_to_file($key, string $output_filename, ?string`
/// `$passphrase = null, ?array $options = null): bool`: the private key
/// as PEM, written to the file.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 512]);
/// $f = __DIR__ . '/k.pem';
/// var_dump(openssl_pkey_export_to_file($k, $f), substr(file_get_contents($f), 0, 27));
/// var_dump(openssl_pkey_export_to_file($k, $f, 'pw'), openssl_pkey_get_private('file://' . $f, 'pw') instanceof OpenSSLAsymmetricKey);
/// unlink($f);
/// var_dump(openssl_pkey_export_to_file('nope', $f));
/// ```
/// ```expect
/// bool(true)
/// string(27) "-----BEGIN PRIVATE KEY-----"
/// bool(true)
/// bool(true)
///
/// Warning: openssl_pkey_export_to_file(): Cannot get key from parameter 1 in %s on line 7
/// bool(false)
/// ```
#[php_function]
fn openssl_pkey_export_to_file(
    req: &mut Request,
    #[php(sensitive)] key: Untyped<Value>,
    output_filename: Path,
    #[php(sensitive, default = null)] passphrase: Option<PhpString>,
    #[php(default = null)] options: Option<PhpArray>,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let phrase = passphrase.as_ref().map(|p| p.as_bytes().to_vec());
        if let Some(phrase) = &phrase {
            check_int_len(req, phrase.len(), "passphrase", 3)?;
        }
        let arg = Arg::new(1, "key");
        let Some(pkey) = key_from(req, &key.0, Half::Private, phrase.as_deref(), arg)? else {
            if !raised(req) {
                docref(req, Level::Warning, "Cannot get key from parameter 1");
            }
            return Ok(false);
        };
        let origin = Origin::Arg(Arg::new(2, "output_filename"));
        let Some(path) = check_path(req, output_filename.0.as_bytes(), origin, false) else {
            return Ok(false);
        };
        let Some(settings) = config::parse(req, options) else {
            return Ok(false);
        };
        let Some(out) = Bio::file(&path, c"w") else {
            store_errors(req);
            return Ok(false);
        };
        Ok(write_private(
            req,
            &pkey,
            &settings,
            phrase.as_deref(),
            &out,
        ))
    })
}

/// `openssl_pkey_export($key, &$output, ?string $passphrase = null,`
/// `?array $options = null): bool`: the private key as PEM.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 512]);
/// var_dump(openssl_pkey_export($k, $out), substr($out, 0, 27));
/// var_dump(openssl_pkey_export($k, $out, 'pw'), substr($out, 0, 37));
/// var_dump(openssl_pkey_export($k, $out, 'pw', ['encrypt_key' => false]), substr($out, 0, 27));
/// var_dump(openssl_pkey_export($k, $out, 'pw', ['encrypt_key_cipher' => OPENSSL_CIPHER_AES_128_CBC]), openssl_pkey_get_private($out, 'pw') !== false);
/// var_dump(openssl_pkey_export('nope', $out));
/// ```
/// ```expect
/// bool(true)
/// string(27) "-----BEGIN PRIVATE KEY-----"
/// bool(true)
/// string(37) "-----BEGIN ENCRYPTED PRIVATE KEY-----"
/// bool(true)
/// string(27) "-----BEGIN PRIVATE KEY-----"
/// bool(true)
/// bool(true)
///
/// Warning: openssl_pkey_export(): Cannot get key from parameter 1 in %s on line 7
/// bool(false)
/// ```
#[php_function]
fn openssl_pkey_export(
    req: &mut Request,
    #[php(sensitive)] key: Untyped<Value>,
    output: Ref<Untyped<Value>>,
    #[php(sensitive, default = null)] passphrase: Option<PhpString>,
    #[php(default = null)] options: Option<PhpArray>,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let phrase = passphrase.as_ref().map(|p| p.as_bytes().to_vec());
        if let Some(phrase) = &phrase {
            check_int_len(req, phrase.len(), "passphrase", 3)?;
        }
        let arg = Arg::new(1, "key");
        let Some(pkey) = key_from(req, &key.0, Half::Private, phrase.as_deref(), arg)? else {
            if !raised(req) {
                docref(req, Level::Warning, "Cannot get key from parameter 1");
            }
            return Ok(false);
        };
        let Some(settings) = config::parse(req, options) else {
            return Ok(false);
        };
        let Some(out) = Bio::memory() else {
            store_errors(req);
            return Ok(false);
        };
        if !write_private(req, &pkey, &settings, phrase.as_deref(), &out) {
            return Ok(false);
        }
        let pem = PhpString::new(req, &out.contents())?;
        output.set(req, Untyped(pem.into_value()))?;
        Ok(true)
    })
}

/// `openssl_pkey_get_public($public_key): OpenSSLAsymmetricKey|false`,
/// and its alias `openssl_get_publickey`: the public key a PEM public
/// key or certificate holds.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 512]);
/// $pub = openssl_pkey_get_details($k)['key'];
/// $p = openssl_get_publickey($pub);
/// var_dump($p instanceof OpenSSLAsymmetricKey, openssl_pkey_get_details($p)['key'] === $pub, isset(openssl_pkey_get_details($p)['rsa']['d']));
/// var_dump(openssl_pkey_get_public('nope'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function(aliases(openssl_get_publickey))]
fn openssl_pkey_get_public(
    req: &mut Request,
    public_key: Untyped<Value>,
) -> Result<OrFalse<Owner<Instance<OpenSSLAsymmetricKey>>>, Thrown> {
    finish(req, move |req| {
        let arg = Arg::new(1, "public_key");
        let Some(pkey) = key_from(req, &public_key.0, Half::Public, None, arg)? else {
            return Ok(OrFalse::False);
        };
        Ok(OrFalse::Value(object_of(req, pkey, false)?))
    })
}

/// `openssl_pkey_free(OpenSSLAsymmetricKey $key): void`, and its alias
/// `openssl_free_key`: nothing, the object freeing its key itself.
///
/// ```php
/// openssl_pkey_free(openssl_pkey_new(['private_key_bits' => 512]));
/// openssl_free_key(openssl_pkey_new(['private_key_bits' => 512]));
/// ```
/// ```expect
/// Deprecated: Function openssl_pkey_free() is deprecated since 8.0, as OpenSSLAsymmetricKey objects are freed automatically in %s on line 2
///
/// Deprecated: Function openssl_free_key() is deprecated since 8.0, as OpenSSLAsymmetricKey objects are freed automatically in %s on line 3
/// ```
#[php_function(
    aliases(openssl_free_key),
    deprecated(
        since = "8.0",
        message = "as OpenSSLAsymmetricKey objects are freed automatically"
    )
)]
fn openssl_pkey_free(req: &mut Request, key: Instance<OpenSSLAsymmetricKey>) {
    let _ = (req, key);
}

/// `openssl_pkey_get_private($private_key, ?string $passphrase = null):`
/// `OpenSSLAsymmetricKey|false`, and its alias `openssl_get_privatekey`:
/// the private key a PEM holds, decrypted with the passphrase, which is
/// the empty one when none is passed and none at all when it is null.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 512]);
/// openssl_pkey_export($k, $pem);
/// $p = openssl_get_privatekey($pem);
/// var_dump($p instanceof OpenSSLAsymmetricKey, openssl_pkey_get_private($p) instanceof OpenSSLAsymmetricKey);
/// var_dump(openssl_pkey_get_private($pem, null) instanceof OpenSSLAsymmetricKey);
/// file_put_contents(__DIR__ . '/k.pem', $pem);
/// var_dump(openssl_pkey_get_private('file://' . __DIR__ . '/k.pem') instanceof OpenSSLAsymmetricKey);
/// var_dump(openssl_pkey_get_private('file://' . __DIR__ . '/none.pem'));
/// unlink(__DIR__ . '/k.pem');
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
#[php_function(aliases(openssl_get_privatekey))]
fn openssl_pkey_get_private(
    req: &mut Request,
    #[php(sensitive)] private_key: Untyped<Value>,
    #[php(sensitive, default = null)] passphrase: Passed<Option<PhpString>>,
) -> Result<OrFalse<Owner<Instance<OpenSSLAsymmetricKey>>>, Thrown> {
    finish(req, move |req| {
        let phrase = match &passphrase {
            Passed::Omitted => Some(Vec::new()),
            Passed::Given(None) => None,
            Passed::Given(Some(phrase)) => {
                check_int_len(req, phrase.as_bytes().len(), "passphrase", 2)?;
                Some(phrase.as_bytes().to_vec())
            }
        };
        let arg = Arg::new(1, "private_key");
        let Some(pkey) = key_from(req, &private_key.0, Half::Private, phrase.as_deref(), arg)?
        else {
            return Ok(OrFalse::False);
        };
        Ok(OrFalse::Value(object_of(req, pkey, true)?))
    })
}

/// The `OPENSSL_KEYTYPE_*` of a kind, -1 for one php names none of.
fn key_type(kind: KeyKind) -> i64 {
    match kind {
        KeyKind::Rsa => OPENSSL_KEYTYPE_RSA,
        KeyKind::Dsa => OPENSSL_KEYTYPE_DSA,
        KeyKind::Dh => OPENSSL_KEYTYPE_DH,
        KeyKind::Ec => OPENSSL_KEYTYPE_EC,
        KeyKind::X25519 => OPENSSL_KEYTYPE_X25519,
        KeyKind::Ed25519 => OPENSSL_KEYTYPE_ED25519,
        KeyKind::X448 => OPENSSL_KEYTYPE_X448,
        KeyKind::Ed448 => OPENSSL_KEYTYPE_ED448,
        KeyKind::Other => -1,
    }
}

/// `php_openssl_pkey_get_details`' sub-array: its name, and each entry's
/// name beside the parameter it reads and how.
enum Param {
    Bn(&'static std::ffi::CStr),
    Octets(&'static std::ffi::CStr),
}

/// A sub-array's name and its entries, each beside the parameter it reads.
type Layout = (&'static [u8], &'static [(&'static [u8], Param)]);

/// The sub-array a kind of key is described by, as
/// `php_openssl_pkey_get_details` lays it (`openssl_backend_v3.c`).
fn layout(kind: KeyKind) -> Option<Layout> {
    use Param::{Bn, Octets};
    Some(match kind {
        KeyKind::Rsa => (
            b"rsa",
            &[
                (b"n", Bn(c"n")),
                (b"e", Bn(c"e")),
                (b"d", Bn(c"d")),
                (b"p", Bn(c"rsa-factor1")),
                (b"q", Bn(c"rsa-factor2")),
                (b"dmp1", Bn(c"rsa-exponent1")),
                (b"dmq1", Bn(c"rsa-exponent2")),
                (b"iqmp", Bn(c"rsa-coefficient1")),
            ],
        ),
        KeyKind::Dsa => (
            b"dsa",
            &[
                (b"p", Bn(c"p")),
                (b"q", Bn(c"q")),
                (b"g", Bn(c"g")),
                (b"priv_key", Bn(c"priv")),
                (b"pub_key", Bn(c"pub")),
            ],
        ),
        KeyKind::Dh => (
            b"dh",
            &[
                (b"p", Bn(c"p")),
                (b"g", Bn(c"g")),
                (b"priv_key", Bn(c"priv")),
                (b"pub_key", Bn(c"pub")),
            ],
        ),
        KeyKind::Ec => (
            b"ec",
            &[(b"x", Bn(c"qx")), (b"y", Bn(c"qy")), (b"d", Bn(c"priv"))],
        ),
        KeyKind::X25519 | KeyKind::Ed25519 | KeyKind::X448 | KeyKind::Ed448 => (
            match kind {
                KeyKind::X25519 => b"x25519",
                KeyKind::Ed25519 => b"ed25519",
                KeyKind::X448 => b"x448",
                _ => b"ed448",
            },
            &[(b"priv_key", Octets(c"priv")), (b"pub_key", Octets(c"pub"))],
        ),
        KeyKind::Other => return None,
    })
}

/// `openssl_pkey_get_details(OpenSSLAsymmetricKey $key): array|false`:
/// `bits`, the public half as PEM under `key`, the key's own parameters
/// under its kind's name, and `type`.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 512]);
/// $d = openssl_pkey_get_details($k);
/// var_dump(array_keys($d), array_keys($d['rsa']), $d['type'] === OPENSSL_KEYTYPE_RSA, strlen($d['rsa']['n']), bin2hex($d['rsa']['e']));
/// var_dump(substr($d['key'], 0, 26));
/// $x = openssl_pkey_get_details(openssl_pkey_new(['private_key_type' => OPENSSL_KEYTYPE_ED25519]));
/// var_dump($x['type'] === OPENSSL_KEYTYPE_ED25519, strlen($x['ed25519']['priv_key']), strlen($x['ed25519']['pub_key']));
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   string(4) "bits"
///   [1]=>
///   string(3) "key"
///   [2]=>
///   string(3) "rsa"
///   [3]=>
///   string(4) "type"
/// }
/// array(8) {
///   [0]=>
///   string(1) "n"
///   [1]=>
///   string(1) "e"
///   [2]=>
///   string(1) "d"
///   [3]=>
///   string(1) "p"
///   [4]=>
///   string(1) "q"
///   [5]=>
///   string(4) "dmp1"
///   [6]=>
///   string(4) "dmq1"
///   [7]=>
///   string(4) "iqmp"
/// }
/// bool(true)
/// int(64)
/// string(6) "010001"
/// string(26) "-----BEGIN PUBLIC KEY-----"
/// bool(true)
/// int(32)
/// int(32)
/// ```
#[php_function]
fn openssl_pkey_get_details(
    req: &mut Request,
    key: Instance<OpenSSLAsymmetricKey>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let (pkey, _) = held(req, key)?;
    let Some(pem) = pkey.public_pem() else {
        store_errors(req);
        return Ok(OrFalse::False);
    };
    let mut out = PhpArray::new(req)?;
    put(req, &mut out, b"bits", Owner::int(pkey.bits()))?;
    put_bytes(req, &mut out, b"key", &pem)?;
    let kind = pkey.kind();
    if let Some((name, params)) = layout(kind) {
        let mut inner = PhpArray::new(req)?;
        if kind == KeyKind::Ec
            && let Some(curve) = pkey.utf8_param(c"group")
        {
            put_bytes(req, &mut inner, b"curve_name", &curve)?;
            let nid = ffi::short_name_nid(&c_string(&curve));
            if nid != 0
                && let Some(oid) = ffi::nid_oid(nid)
            {
                put_bytes(req, &mut inner, b"curve_oid", &oid)?;
            }
        }
        for (entry, param) in params {
            let found = match param {
                Param::Bn(name) => pkey.bn_param(name),
                Param::Octets(name) => pkey.octet_param(name),
            };
            if let Some(bytes) = found {
                put_bytes(req, &mut inner, entry, &bytes)?;
            }
        }
        put(req, &mut out, name, inner.into_value())?;
    }
    put(req, &mut out, b"type", Owner::int(key_type(kind)))?;
    Ok(OrFalse::Value(out))
}

/// `php_openssl_pkey_derive`: the secret `key` and `peer` agree on,
/// `size` bytes of it, or all of it where `size` is 0; `None` with the
/// library's reasons stored.
fn derive(req: &mut Request, key: &PKey, peer: &PKey, size: usize) -> Option<Vec<u8>> {
    let mut ctx = KeyContext::of_key(key)?;
    let secret = (ctx.init(Operation::Derive) && ctx.set_peer(peer))
        .then(|| ctx.apply(Operation::Derive, &[], size))
        .flatten();
    if secret.is_none() {
        store_errors(req);
    }
    secret
}

/// `openssl_dh_compute_key(string $public_key, OpenSSLAsymmetricKey`
/// `$private_key): string|false`: the secret a DH key shares with the
/// peer whose public value is given.
///
/// ```php
/// $p = "\xff\xff\xff\xff\xff\xff\xff\xc5";
/// $a = openssl_pkey_new(['dh' => ['p' => $p, 'g' => "\x02", 'priv_key' => "\x03"]]);
/// $b = openssl_pkey_new(['dh' => ['p' => $p, 'g' => "\x02", 'priv_key' => "\x05"]]);
/// $pb = openssl_pkey_get_details($b)['dh']['pub_key'];
/// $pa = openssl_pkey_get_details($a)['dh']['pub_key'];
/// var_dump(bin2hex(openssl_dh_compute_key($pb, $a)), openssl_dh_compute_key($pb, $a) === openssl_dh_compute_key($pa, $b));
/// var_dump(openssl_dh_compute_key($pb, openssl_pkey_new(['private_key_bits' => 512])));
/// ```
/// ```expect
/// string(0) ""
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn openssl_dh_compute_key(
    req: &mut Request,
    public_key: PhpString,
    #[php(sensitive)] private_key: Instance<OpenSSLAsymmetricKey>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    check_int_len(req, public_key.as_bytes().len(), "pub_key", 1)?;
    let (pkey, _) = held(req, private_key)?;
    if !pkey.is_dh() {
        return Ok(OrFalse::False);
    }
    let Some(peer) = pkey.peer_of(public_key.as_bytes()) else {
        store_errors(req);
        return Ok(OrFalse::False);
    };
    let Some(secret) = derive(req, &pkey, &peer, 0) else {
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(PhpString::new(req, &secret)?))
}

/// `openssl_pkey_derive($public_key, $private_key, int $key_length = 0):`
/// `string|false`: the secret a DH, ECDH, X25519 or X448 private key
/// agrees on with the peer's public key.
///
/// ```php
/// $a = openssl_pkey_new(['x25519' => []]);
/// $b = openssl_pkey_new(['x25519' => []]);
/// $pa = openssl_pkey_get_details($a)['key'];
/// $pb = openssl_pkey_get_details($b)['key'];
/// var_dump(strlen(openssl_pkey_derive($pb, $a)), openssl_pkey_derive($pb, $a) === openssl_pkey_derive($pa, $b));
/// var_dump(strlen(openssl_pkey_derive($pb, $a, 16)));
/// try { openssl_pkey_derive($pb, $a, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(openssl_pkey_derive('nope', $a));
/// ```
/// ```expect
/// int(32)
/// bool(true)
///
/// Deprecated: openssl_pkey_derive(): the $key_length parameter is deprecated as it is either ignored or truncates the key in %s on line 7
/// int(0)
///
/// Deprecated: openssl_pkey_derive(): the $key_length parameter is deprecated as it is either ignored or truncates the key in %s on line 8
/// openssl_pkey_derive(): Argument #3 ($key_length) must be greater than or equal to 0
/// bool(false)
/// ```
#[php_function]
fn openssl_pkey_derive(
    req: &mut Request,
    public_key: Untyped<Value>,
    #[php(sensitive)] private_key: Untyped<Value>,
    #[php(default = 0)] key_length: Passed<i64>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    finish(req, move |req| {
        let length = match key_length {
            Passed::Omitted => 0,
            Passed::Given(length) => {
                docref(
                    req,
                    Level::Deprecated,
                    "the $key_length parameter is deprecated as it is either ignored or truncates the key",
                );
                pending(req)?;
                length
            }
        };
        let Ok(length) = usize::try_from(length) else {
            return Err(arg_value_error(req, 3, "be greater than or equal to 0"));
        };
        let arg = Arg::new(2, "private_key");
        let Some(key) = key_from(req, &private_key.0, Half::Private, Some(b""), arg)? else {
            return Ok(OrFalse::False);
        };
        let arg = Arg::new(1, "public_key");
        let Some(peer) = key_from(req, &public_key.0, Half::Public, None, arg)? else {
            return Ok(OrFalse::False);
        };
        let Some(secret) = derive(req, &key, &peer, length) else {
            return Ok(OrFalse::False);
        };
        Ok(OrFalse::Value(PhpString::new(req, &secret)?))
    })
}

/// `openssl_get_curve_names(): array|false`: the short name of every
/// curve the library carries.
///
/// ```php
/// $c = openssl_get_curve_names();
/// var_dump(in_array('prime256v1', $c), in_array('secp384r1', $c), count($c) > 50);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn openssl_get_curve_names(req: &mut Request) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Some(names) = ffi::cipher::curve_names() else {
        return Ok(OrFalse::False);
    };
    let mut out = PhpArray::new(req)?;
    for name in names {
        let name = PhpString::new(req, &name)?;
        out.append(req, name.into_value())?;
    }
    Ok(OrFalse::Value(out))
}
