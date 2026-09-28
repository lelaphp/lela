//! `OpenSSLAsymmetricKey` and the natives that make, read and write one:
//! `php_openssl_pkey_object`, `php_openssl_pkey_from_zval` (a key, a
//! PEM string, a `file://` path, or an array of one and its passphrase)
//! and the `openssl_pkey_*` natives of `openssl.c`.

use std::cmp::Ordering;
use std::ffi::CString;

use php_ext::abi::{CloneObj, Instance, ObjectHandlers, OrFalse, Passed, Ref, Untyped};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{Function, lookup_class, new_object};
use php_ext::coerce::to_string;
use php_ext::raising::{Level, Thrown, arg_value_error, docref, fatal, raise, throw};
use php_ext::request::{Request, Resolve};
use php_ext::values::{Key as ArrayKey, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::config::{self, field};
use crate::constants::{
    OPENSSL_KEYTYPE_DH, OPENSSL_KEYTYPE_DSA, OPENSSL_KEYTYPE_EC, OPENSSL_KEYTYPE_ED448,
    OPENSSL_KEYTYPE_ED25519, OPENSSL_KEYTYPE_RSA, OPENSSL_KEYTYPE_X448, OPENSSL_KEYTYPE_X25519,
};
use crate::ffi::{Bio, Certificate, Cipher, KeyKind, PKey};
use crate::{errors_restore_mark, errors_set_mark, store_errors};

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
#[php_class(
    final,
    not_serializable,
    payload = Key,
    handlers = HANDLERS,
    dynamic_properties = forbidden
)]
pub(crate) struct OpenSSLAsymmetricKey;

#[php_class]
impl OpenSSLAsymmetricKey {}

/// `php_openssl_pkey_object_handlers`: `std_object_handlers` with
/// `get_constructor` refusing, `clone_obj` php's null slot and `compare`
/// `zend_objects_not_comparable`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

impl ObjectHandlers for Handlers {
    fn get_constructor(
        &self,
        req: &mut Request,
        _object: PhpObject,
    ) -> Result<Option<Function>, Thrown> {
        Err(throw(
            req,
            "Error",
            "Cannot directly construct OpenSSLAsymmetricKey, use openssl_pkey_new() instead",
        ))
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }

    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let _ = (req, left, right);
        Ok(None)
    }
}

/// `php_openssl_pkey_object_init`: a new `OpenSSLAsymmetricKey` holding
/// the key.
fn object_of(
    req: &mut Request,
    pkey: PKey,
    private: bool,
) -> Result<Instance<OpenSSLAsymmetricKey>, Thrown> {
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
    Ok(Instance::of(object))
}

/// The key an `OpenSSLAsymmetricKey` holds, and whether it is private.
fn held(req: &mut Request, key: Instance<OpenSSLAsymmetricKey>) -> Result<(PKey, bool), Thrown> {
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

/// A parameter a key or a path is read from, as its messages name it:
/// one-based position and declared name.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Arg {
    pub(crate) position: u32,
    pub(crate) name: &'static str,
}

/// Which half of a key pair a native asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Half {
    Private,
    Public,
}

/// `PHP_OPENSSL_CHECK_SIZE_T_TO_INT`: a length past `INT_MAX` refused.
pub(crate) fn check_int_len(req: &mut Request, len: usize, arg: u32) -> Result<(), Thrown> {
    if i32::try_from(len).is_err() {
        return Err(arg_value_error(req, arg, "is too long"));
    }
    Ok(())
}

/// `php_openssl_check_path_ex` for a path a parameter names, `file://`
/// taken off: the path made absolute; `None` where it cannot be, after
/// the warning php raises, and a `ValueError` for a NUL in it.
pub(crate) fn check_path(
    req: &mut Request,
    path: &[u8],
    arg: Arg,
) -> Result<Option<CString>, Thrown> {
    let Some(path) = path
        .strip_prefix(b"file://")
        .filter(|rest| !rest.is_empty())
    else {
        return Ok(None);
    };
    if path.contains(&0) {
        return Err(arg_value_error(
            req,
            arg.position,
            "not contain any null bytes",
        ));
    }
    let cwd = req.cwd().0.into_os_string().into_encoded_bytes();
    if let Some(real) = req.virtual_file_ex(&cwd, path, Resolve::FilePath) {
        return Ok(CString::new(real).ok());
    }
    // `php_verror(NULL, arg_name, E_WARNING, ...)`: the parameter's name
    // between the parentheses.
    let text = format!(
        "{}({}): must be a valid file path",
        req.active_function_name(),
        arg.name
    );
    raise(req, Level::Warning, &text);
    Ok(None)
}

/// `php_openssl_x509_from_str`: a certificate from a `file://` path (any
/// PEM certificate label) or from the string itself (`CERTIFICATE`
/// alone), what the library said of a failure stored.
fn certificate_from(
    req: &mut Request,
    text: &[u8],
    arg: Arg,
) -> Result<Option<Certificate>, Thrown> {
    let cert = if text.len() > 7 && text.starts_with(b"file://") {
        let Some(path) = check_path(req, text, arg)? else {
            return Ok(None);
        };
        let Some(bio) = Bio::file(&path) else {
            store_errors(req);
            return Ok(None);
        };
        Certificate::read_pem(&bio)
    } else {
        let Some(bio) = Bio::reading(text) else {
            store_errors(req);
            return Ok(None);
        };
        Certificate::read_pem_certificate(&bio)
    };
    if cert.is_none() {
        store_errors(req);
    }
    Ok(cert)
}

/// `php_openssl_pkey_from_zval`: the key a parameter gives. An array is
/// `[key, passphrase]`; an `OpenSSLAsymmetricKey` must be the half asked
/// for; anything else is read as a string, a `file://` path or the PEM
/// itself, a certificate's key where the public half is asked for and
/// the string is one. `None` where there is no key, after the warnings
/// php raises and with the library's reasons stored.
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
            return Err(throw(req, "ValueError", form));
        };
        phrase = Some(match given.as_ref() {
            ValueRef::Str(s) => s.as_bytes().to_vec(),
            _ => to_string(req, &given)?.as_bytes().to_vec(),
        });
        let Some(inner) = array.get(&ArrayKey::Int(0)) else {
            return Err(throw(req, "ValueError", form));
        };
        value = inner;
    }

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
    }
    if !matches!(value.as_ref(), ValueRef::Str(_) | ValueRef::Object(_)) {
        return Ok(None);
    }
    let text = to_string(req, &value)?;
    let text = text.as_bytes();
    let mut file = None;
    if text.len() > 7 && text.starts_with(b"file://") {
        let Some(path) = check_path(req, text, arg)? else {
            return Ok(None);
        };
        file = Some(path);
    }
    let bio = |file: &Option<CString>| match file {
        Some(path) => Bio::file(path),
        None => Bio::reading(text),
    };

    let mut cert = None;
    let key = match half {
        Half::Public => {
            errors_set_mark(req);
            cert = certificate_from(req, text, arg)?;
            if cert.is_some() {
                None
            } else {
                errors_restore_mark(req);
                let Some(bio) = bio(&file) else {
                    store_errors(req);
                    return Ok(None);
                };
                PKey::read_public(&bio)
            }
        }
        Half::Private => {
            let Some(bio) = bio(&file) else {
                return Ok(None);
            };
            PKey::read_private(&bio, phrase.as_deref())
        }
    };
    if key.is_some() {
        return Ok(key);
    }
    store_errors(req);
    let key = cert.as_ref().and_then(Certificate::public_key);
    if cert.is_some() && key.is_none() {
        store_errors(req);
    }
    Ok(key)
}

/// `openssl_pkey_new(?array $options = null): OpenSSLAsymmetricKey|false`:
/// a new private key of the type and size the options and the
/// configuration say, RSA of the configuration's `default_bits` unless
/// told otherwise.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 512]);
/// var_dump($k instanceof OpenSSLAsymmetricKey, openssl_pkey_get_details($k)['bits']);
/// $d = openssl_pkey_get_details(openssl_pkey_new(['private_key_type' => OPENSSL_KEYTYPE_EC, 'curve_name' => 'prime256v1']));
/// var_dump($d['type'] === OPENSSL_KEYTYPE_EC, $d['ec']['curve_name'], $d['ec']['curve_oid']);
/// var_dump(openssl_pkey_new(['private_key_bits' => 128]));
/// var_dump(openssl_pkey_new(['private_key_type' => 42]));
/// var_dump(openssl_pkey_new(['private_key_type' => OPENSSL_KEYTYPE_EC]));
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
/// ```
#[php_function]
fn openssl_pkey_new(
    req: &mut Request,
    #[php(default = null)] options: Option<PhpArray>,
) -> Result<OrFalse<Instance<OpenSSLAsymmetricKey>>, Thrown> {
    if let Some(array) = &options {
        for form in [
            &b"rsa"[..],
            b"dsa",
            b"dh",
            b"ec",
            b"x25519",
            b"ed25519",
            b"x448",
            b"ed448",
        ] {
            if field(req, *array, form)?.is_some_and(|v| matches!(v.as_ref(), ValueRef::Array(_))) {
                let text = format!(
                    "InternalError: openssl_pkey_new() from the \"{}\" key's parameters is not ported",
                    String::from_utf8_lossy(form)
                );
                return Err(fatal(req, &text));
            }
        }
    }
    let Some(request) = config::parse(req, options)? else {
        return Ok(OrFalse::False);
    };
    let Some(key) = request.generate(req) else {
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(object_of(req, key, true)?))
}

/// `openssl_pkey_export($key, &$output, ?string $passphrase = null,
/// ?array $options = null): bool`: the private key as PEM, encrypted
/// under the passphrase with the options' cipher (`DES-EDE3-CBC` unless
/// named) where one is given and the configuration encrypts keys.
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
    let phrase = passphrase.as_ref().map(|p| p.as_bytes().to_vec());
    if let Some(phrase) = &phrase {
        check_int_len(req, phrase.len(), 3)?;
    }
    let arg = Arg {
        position: 1,
        name: "key",
    };
    let Some(pkey) = key_from(req, &key.0, Half::Private, phrase.as_deref(), arg)? else {
        docref(req, Level::Warning, "Cannot get key from parameter 1");
        return Ok(false);
    };
    let Some(request) = config::parse(req, options)? else {
        return Ok(false);
    };
    let fallback;
    let encryption = match &phrase {
        Some(phrase) if request.encrypt => {
            let cipher = if let Some(cipher) = &request.cipher {
                Some(cipher)
            } else {
                fallback = Cipher::des_ede3_cbc();
                fallback.as_ref()
            };
            cipher.map(|cipher| (cipher, phrase.as_slice()))
        }
        _ => None,
    };
    let Some(pem) = pkey.private_pem(encryption) else {
        store_errors(req);
        return Ok(false);
    };
    let pem = PhpString::new(req, &pem)?;
    output.set(req, Untyped(Value::string(pem)))?;
    Ok(true)
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
) -> Result<OrFalse<Instance<OpenSSLAsymmetricKey>>, Thrown> {
    let arg = Arg {
        position: 1,
        name: "public_key",
    };
    let Some(pkey) = key_from(req, &public_key.0, Half::Public, None, arg)? else {
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(object_of(req, pkey, false)?))
}

/// `openssl_pkey_free(OpenSSLAsymmetricKey $key): void`, and its alias
/// `openssl_free_key`: nothing, the object freeing its key itself.
///
/// ```php
/// openssl_pkey_free(openssl_pkey_new(['private_key_bits' => 512]));
/// openssl_free_key(openssl_pkey_new(['private_key_bits' => 512]));
/// ```
/// ```expect
///
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

/// `openssl_pkey_get_private($private_key, ?string $passphrase = null):
/// OpenSSLAsymmetricKey|false`, and its alias `openssl_get_privatekey`:
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
) -> Result<OrFalse<Instance<OpenSSLAsymmetricKey>>, Thrown> {
    let phrase = match &passphrase {
        Passed::Omitted => Some(Vec::new()),
        Passed::Given(None) => None,
        Passed::Given(Some(phrase)) => {
            check_int_len(req, phrase.as_bytes().len(), 2)?;
            Some(phrase.as_bytes().to_vec())
        }
    };
    let arg = Arg {
        position: 1,
        name: "private_key",
    };
    let Some(pkey) = key_from(req, &private_key.0, Half::Private, phrase.as_deref(), arg)? else {
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(object_of(req, pkey, true)?))
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

/// A string entry of an array under construction.
fn put(req: &mut Request, array: &mut PhpArray, name: &[u8], bytes: &[u8]) -> Result<(), Thrown> {
    let key = ArrayKey::Str(PhpString::new(req, name)?);
    let value = Value::string(PhpString::new(req, bytes)?);
    array.set(req, key, value)
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
) -> Result<OrFalse<PhpArray>, Thrown> {
    let (pkey, _) = held(req, key)?;
    let Some(pem) = pkey.public_pem() else {
        store_errors(req);
        return Ok(OrFalse::False);
    };
    let mut out = PhpArray::new(req)?;
    let bits = ArrayKey::Str(PhpString::new(req, b"bits")?);
    out.set(req, bits, Value::int(pkey.bits()))?;
    put(req, &mut out, b"key", &pem)?;
    let kind = pkey.kind();
    if let Some((name, params)) = layout(kind) {
        let mut inner = PhpArray::new(req)?;
        if kind == KeyKind::Ec
            && let Some(curve) = pkey.utf8_param(c"group")
        {
            put(req, &mut inner, b"curve_name", &curve)?;
            let nid = crate::ffi::short_name_nid(&config::c_string(&curve));
            if nid != 0
                && let Some(oid) = crate::ffi::nid_oid(nid)
            {
                put(req, &mut inner, b"curve_oid", &oid)?;
            }
        }
        for (entry, param) in params {
            let found = match param {
                Param::Bn(name) => pkey.bn_param(name),
                Param::Octets(name) => pkey.octet_param(name),
            };
            if let Some(bytes) = found {
                put(req, &mut inner, entry, &bytes)?;
            }
        }
        let name = ArrayKey::Str(PhpString::new(req, name)?);
        out.set(req, name, Value::array(inner))?;
    }
    let type_key = ArrayKey::Str(PhpString::new(req, b"type")?);
    out.set(req, type_key, Value::int(key_type(kind)))?;
    Ok(OrFalse::Value(out))
}
