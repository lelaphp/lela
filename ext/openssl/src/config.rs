//! `struct php_x509_request` as the natives read it:
//! `php_openssl_parse_config` over the `$options` array and the
//! configuration file it names, decoded once into [`Settings`], and
//! `php_openssl_generate_private_key`, which makes a key under it
//! (`openssl_backend_common.c`).

use std::ffi::{CStr, CString, c_int};

use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};

use crate::constants::{
    OPENSSL_KEYTYPE_DH, OPENSSL_KEYTYPE_DSA, OPENSSL_KEYTYPE_EC, OPENSSL_KEYTYPE_ED448,
    OPENSSL_KEYTYPE_ED25519, OPENSSL_KEYTYPE_RSA, OPENSSL_KEYTYPE_X448, OPENSSL_KEYTYPE_X25519,
};
use crate::decode::{Origin, c_string, check_path};
use crate::ffi::key::KeyContext;
use crate::ffi::x509::{Extended, add_extensions};
use crate::ffi::{self, Bio, Cipher, Conf, Digest, KeyKind, PKey};
use crate::store_errors;

/// `MIN_KEY_LENGTH`: the fewest bits an RSA, DSA or DH key is made with.
const MIN_KEY_LENGTH: c_int = 384;

/// `default_ssl_conf_filename`: `$OPENSSL_CONF`, else `$SSLEAY_CONF`,
/// else `openssl.cnf` in the library's certificate area.
pub(crate) fn default_file() -> Vec<u8> {
    for name in ["OPENSSL_CONF", "SSLEAY_CONF"] {
        if let Some(value) = std::env::var_os(name) {
            return value.into_encoded_bytes();
        }
    }
    let mut path = ffi::default_cert_area();
    path.extend_from_slice(b"/openssl.cnf");
    path
}

/// `(int)Z_LVAL_P(item)`: the low 32 bits, as C's conversion keeps them.
pub(crate) fn as_int(n: i64) -> c_int {
    let bytes = n.to_le_bytes();
    i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// `zend_hash_str_find(Z_ARRVAL_P(args), name)`: the entry as it is, a
/// reference not followed.
pub(crate) fn field(
    req: &mut Request,
    array: PhpArray,
    name: &[u8],
) -> Result<Option<Value>, Thrown> {
    let key = PhpString::new(req, name)?;
    Ok(array.get(&Key::Str(key)))
}

/// The `$options` of a native that parses a configuration, `a!`: each
/// entry read as `SET_OPTIONAL_STRING_ARG` and `SET_OPTIONAL_LONG_ARG`
/// read it, of its own type or not at all.
struct Options(Option<PhpArray>);

impl Options {
    /// The entry under `name`, as it is.
    fn entry(&self, req: &mut Request, name: &[u8]) -> Result<Option<Value>, Thrown> {
        match self.0 {
            Some(array) => field(req, array, name),
            None => Ok(None),
        }
    }

    /// A string entry, as C reads it.
    fn string(&self, req: &mut Request, name: &[u8]) -> Result<Option<CString>, Thrown> {
        Ok(self
            .entry(req, name)?
            .and_then(|value| match value.as_ref() {
                ValueRef::Str(s) => Some(c_string(s.as_bytes())),
                _ => None,
            }))
    }

    /// An integer entry.
    fn long(&self, req: &mut Request, name: &[u8]) -> Result<Option<i64>, Thrown> {
        Ok(self
            .entry(req, name)?
            .and_then(|value| match value.as_ref() {
                ValueRef::Int(n) => Some(n),
                _ => None,
            }))
    }
}

/// `php_openssl_get_evp_cipher_from_algo`: the `OPENSSL_CIPHER_*` names.
pub(crate) fn cipher_of(algo: i64) -> Option<Cipher> {
    let name = match algo {
        0 => c"RC2-40-CBC",
        1 => c"RC2-CBC",
        2 => c"RC2-64-CBC",
        3 => c"DES-CBC",
        4 => c"DES-EDE3-CBC",
        5 => c"AES-128-CBC",
        6 => c"AES-192-CBC",
        7 => c"AES-256-CBC",
        _ => return None,
    };
    Cipher::by_name(name)
}

/// `php_openssl_get_evp_pkey_type`: the kind an `OPENSSL_KEYTYPE_*`
/// makes, `None` for any other number.
pub(crate) fn kind_of(key_type: i64) -> Option<KeyKind> {
    Some(match key_type {
        OPENSSL_KEYTYPE_RSA => KeyKind::Rsa,
        OPENSSL_KEYTYPE_DSA => KeyKind::Dsa,
        OPENSSL_KEYTYPE_DH => KeyKind::Dh,
        OPENSSL_KEYTYPE_EC => KeyKind::Ec,
        OPENSSL_KEYTYPE_X25519 => KeyKind::X25519,
        OPENSSL_KEYTYPE_ED25519 => KeyKind::Ed25519,
        OPENSSL_KEYTYPE_X448 => KeyKind::X448,
        OPENSSL_KEYTYPE_ED448 => KeyKind::Ed448,
        _ => return None,
    })
}

/// `struct php_x509_request` after a parse that succeeded: the
/// configuration and every setting a native reads of it.
pub(crate) struct Settings {
    pub(crate) conf: Conf,
    /// `section_name`.
    pub(crate) section: CString,
    /// `digest`: the one named, `EVP_md_null()` for `null`, or SHA-1.
    pub(crate) digest: Option<Digest>,
    /// `extensions_section`, `x509_extensions`.
    pub(crate) extensions: Option<CString>,
    /// `request_extensions_section`, `req_extensions`.
    pub(crate) request_extensions: Option<CString>,
    /// `priv_key_bits`.
    bits: c_int,
    /// `priv_key_type`: the kind it names, `None` for a number that names
    /// none.
    key_type: Option<KeyKind>,
    /// `priv_key_encrypt`.
    pub(crate) encrypt: bool,
    /// `priv_key_encrypt_cipher`.
    pub(crate) cipher: Option<Cipher>,
    /// `curve_name`, `NID_undef` (0) for none.
    curve: c_int,
}

/// `PHP_SSL_REQ_PARSE(&req, args)`: the configuration file `config`
/// names, or the default one, read, then each option over its entry in
/// the file's `config_section_name` section (`req` by default). `None`
/// for a parse php answers `FAILURE` to: a file the library could not
/// read, which says nothing, or an option or section refused with a
/// warning.
///
/// ```php
/// $k = openssl_pkey_new(['config' => '/nonexistent.cnf']);
/// var_dump($k, openssl_error_string() !== false);
/// $k = openssl_pkey_new(['private_key_bits' => 512, 'encrypt_key_cipher' => 99, 'encrypt_key' => true]);
/// var_dump($k);
/// $k = openssl_pkey_new(['private_key_type' => OPENSSL_KEYTYPE_EC, 'curve_name' => 'nope']);
/// var_dump($k);
/// var_dump(openssl_csr_new(['commonName' => 'a'], $key, ['x509_extensions' => 'nope']));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
///
/// Warning: openssl_pkey_new(): Unknown cipher algorithm for private key in %s on line 4
/// bool(false)
///
/// Warning: openssl_pkey_new(): Unknown elliptic curve (short) name nope in %s on line 6
/// bool(false)
///
/// Warning: openssl_csr_new(): Error loading extensions_section section nope of /opt/homebrew/etc/openssl@3/openssl.cnf in %s on line 8
/// bool(false)
/// ```
pub(crate) fn parse(
    req: &mut Request,
    options: Option<PhpArray>,
) -> Result<Option<Settings>, Thrown> {
    let options = Options(options);
    let Some((file, section, conf)) = load(req, &options)? else {
        return Ok(None);
    };
    let setting =
        |conf: &Conf, name: &CStr| conf.string(Some(&section), name).map(|v| c_string(&v));
    let digest_name = match options.string(req, b"digest_alg")? {
        Some(name) => Some(name),
        None => setting(&conf, c"default_md"),
    };
    let extensions = match options.string(req, b"x509_extensions")? {
        Some(name) => Some(name),
        None => setting(&conf, c"x509_extensions"),
    };
    let request_extensions = match options.string(req, b"req_extensions")? {
        Some(name) => Some(name),
        None => setting(&conf, c"req_extensions"),
    };
    let bits = match options.long(req, b"private_key_bits")? {
        Some(bits) => as_int(bits),
        None => c_int::try_from(conf.number(Some(&section), c"default_bits")).unwrap_or(0),
    };
    let key_type = options
        .long(req, b"private_key_type")?
        .map_or(Some(KeyKind::Rsa), |n| kind_of(i64::from(as_int(n))));

    let encrypt = if let Some(value) = options.entry(req, b"encrypt_key")? {
        matches!(value.as_ref(), ValueRef::Bool(true))
    } else {
        let said = conf
            .string(Some(&section), c"encrypt_rsa_key")
            .or_else(|| conf.string(Some(&section), c"encrypt_key"));
        said.as_deref() != Some(b"no")
    };

    let mut cipher = None;
    if encrypt && let Some(algo) = options.long(req, b"encrypt_key_cipher")? {
        let Some(found) = cipher_of(algo) else {
            docref(
                req,
                Level::Warning,
                "Unknown cipher algorithm for private key",
            );
            return Ok(None);
        };
        cipher = Some(found);
    }

    let digest = digest_named(req, digest_name.as_deref());
    if !syntax_check(
        req,
        &conf,
        &file,
        "extensions_section",
        extensions.as_deref(),
    ) {
        return Ok(None);
    }

    let mut curve = 0;
    if let Some(name) = options.string(req, b"curve_name")? {
        curve = ffi::short_name_nid(&name);
        if curve == 0 {
            let text = format!(
                "Unknown elliptic curve (short) name {}",
                String::from_utf8_lossy(name.as_bytes())
            );
            docref(req, Level::Warning, &text);
            return Ok(None);
        }
    }

    if !string_mask(req, &conf, &section) {
        return Ok(None);
    }

    let label = "request_extensions_section";
    if !syntax_check(req, &conf, &file, label, request_extensions.as_deref()) {
        return Ok(None);
    }

    Ok(Some(Settings {
        conf,
        section,
        digest,
        extensions,
        request_extensions,
        bits,
        key_type,
        encrypt,
        cipher,
        curve,
    }))
}

/// The configuration file `config` names, or the default one, read
/// after the default one is, under the section `config_section_name`
/// names, with its OIDs made; `None` where it does not read, or after
/// php's warning where an OID cannot be made.
fn load(req: &mut Request, options: &Options) -> Result<Option<(CString, CString, Conf)>, Thrown> {
    let file = match options.string(req, b"config")? {
        Some(file) => file,
        None => c_string(&default_file()),
    };
    let section = options
        .string(req, b"config_section_name")?
        .unwrap_or_else(|| c"req".to_owned());
    let default = c_string(&default_file());
    if !Conf::new().is_some_and(|mut global| global.load(&default)) {
        store_errors(req);
    }
    let Some(mut conf) = Conf::new() else {
        return Ok(None);
    };
    if !conf.load(&file) || !load_oids(req, &conf)? {
        return Ok(None);
    }
    Ok(Some((file, section, conf)))
}

/// The request's digest: `EVP_md_null()` for `null`, looked up nowhere;
/// the one named; or SHA-1, with the failed lookup's reasons stored, for
/// a name the library lacks or none.
fn digest_named(req: &mut Request, name: Option<&CStr>) -> Option<Digest> {
    match name {
        Some(name) if name.to_bytes() == b"null" => Digest::null(),
        Some(name) => match Digest::by_name(name) {
            Some(found) => Some(found),
            None => sha1(req),
        },
        None => sha1(req),
    }
}

/// The section's `string_mask` set as the library's default; false
/// after php's warning for one it refuses.
fn string_mask(req: &mut Request, conf: &Conf, section: &CStr) -> bool {
    let Some(mask) = conf.string(Some(section), c"string_mask") else {
        return true;
    };
    if ffi::set_default_string_mask(&c_string(&mask)) {
        return true;
    }
    let text = format!(
        "Invalid global string mask setting {}",
        String::from_utf8_lossy(&mask)
    );
    docref(req, Level::Warning, &text);
    false
}

/// `php_openssl_get_evp_md_by_name("sha1")`, the digest a request falls
/// back to, and what the lookup before it queued stored.
fn sha1(req: &mut Request) -> Option<Digest> {
    let found = Digest::by_name(c"sha1");
    store_errors(req);
    found
}

/// The configuration's `oid_file`, each line of it made an object, and
/// `php_openssl_add_oid_section`: each line of its `oid_section` made an
/// object where no object has its name; false after php's warning where
/// the section or an object cannot be.
fn load_oids(req: &mut Request, conf: &Conf) -> Result<bool, Thrown> {
    if let Some(file) = conf.string(None, c"oid_file") {
        let origin = Origin::Setting("oid_file");
        let path = check_path(req, c_string(&file).as_bytes(), origin, false)?;
        if let Some(bio) = path.and_then(|path| Bio::file(&path, c"r")) {
            ffi::create_objects(&bio);
            drop(bio);
            store_errors(req);
        }
    }
    let Some(section) = conf.string(None, c"oid_section") else {
        return Ok(true);
    };
    let Some(lines) = conf.section(&c_string(&section)) else {
        store_errors(req);
        let text = format!(
            "Problem loading oid section {}",
            String::from_utf8_lossy(&section)
        );
        docref(req, Level::Warning, &text);
        return Ok(false);
    };
    for (name, value) in lines {
        let (name_c, value_c) = (c_string(&name), c_string(&value));
        if ffi::short_name_nid(&name_c) == 0
            && ffi::long_name_nid(&name_c) == 0
            && ffi::create_object(&value_c, &name_c, &name_c) == 0
        {
            store_errors(req);
            let text = format!(
                "Problem creating object {}={}",
                String::from_utf8_lossy(&name),
                String::from_utf8_lossy(&value)
            );
            docref(req, Level::Warning, &text);
            return Ok(false);
        }
    }
    Ok(true)
}

/// `PHP_SSL_CONFIG_SYNTAX_CHECK`: a section of extensions added under
/// `X509V3_set_ctx_test`, which only checks it; false after php's warning
/// where it does not parse.
fn syntax_check(
    req: &mut Request,
    conf: &Conf,
    file: &CStr,
    label: &str,
    section: Option<&CStr>,
) -> bool {
    let Some(section) = section else {
        return true;
    };
    if add_extensions(conf, section, &Extended::Test) {
        return true;
    }
    store_errors(req);
    let text = format!(
        "Error loading {label} section {} of {}",
        String::from_utf8_lossy(section.to_bytes()),
        String::from_utf8_lossy(file.to_bytes())
    );
    docref(req, Level::Warning, &text);
    false
}

impl Settings {
    /// `php_openssl_generate_private_key`: a key of the configured type
    /// and size, its parameters generated first for every type but RSA,
    /// the seed file read before and written after as php does; `None`
    /// with a warning or the library's reasons stored.
    pub(crate) fn generate(&self, req: &mut Request) -> Option<PKey> {
        if matches!(
            self.key_type,
            Some(KeyKind::Rsa | KeyKind::Dh | KeyKind::Dsa)
        ) && self.bits < MIN_KEY_LENGTH
        {
            let text = format!(
                "Private key length must be at least {MIN_KEY_LENGTH} bits, configured to {}",
                self.bits
            );
            docref(req, Level::Warning, &text);
            return None;
        }
        let Some(kind) = self.key_type else {
            docref(req, Level::Warning, "Unsupported private key type");
            return None;
        };
        let randfile = self
            .conf
            .string(Some(&self.section), c"RANDFILE")
            .map(|name| c_string(&name));
        let seeded = load_rand_file(req, randfile.as_deref());
        let key = self.make(req, kind);
        if seeded {
            write_rand_file(req, randfile.as_deref());
        }
        key
    }

    /// The generation itself, between the seed file's read and write.
    fn make(&self, req: &mut Request, kind: KeyKind) -> Option<PKey> {
        let Some(mut ctx) = KeyContext::of_kind(kind) else {
            store_errors(req);
            return None;
        };
        if kind != KeyKind::Rsa {
            if !ctx.paramgen_init() {
                store_errors(req);
                return None;
            }
            let set = match kind {
                KeyKind::Dsa => ctx.set_dsa_bits(self.bits),
                KeyKind::Dh => ctx.set_dh_prime_len(self.bits),
                KeyKind::Ec => {
                    if self.curve == 0 {
                        docref(
                            req,
                            Level::Warning,
                            "Missing configuration value: \"curve_name\" not set",
                        );
                        return None;
                    }
                    ctx.set_ec_curve(self.curve)
                }
                _ => true,
            };
            if !set {
                store_errors(req);
                return None;
            }
            let Some(params) = ctx.paramgen() else {
                store_errors(req);
                return None;
            };
            let Some(next) = KeyContext::of_params(&params) else {
                store_errors(req);
                return None;
            };
            ctx = next;
        }
        if ctx.keygen_init() <= 0 || (kind == KeyKind::Rsa && !ctx.set_rsa_bits(self.bits)) {
            store_errors(req);
            return None;
        }
        let key = ctx.keygen(false);
        if key.is_none() {
            store_errors(req);
        }
        key
    }
}

/// `php_openssl_load_rand_file`: the seed file read into the generator,
/// the library's own name for it where the configuration gives none;
/// whether it was, which is whether it is written back.
fn load_rand_file(req: &mut Request, file: Option<&CStr>) -> bool {
    let named = match file {
        Some(file) => Some(file.to_owned()),
        None => ffi::rand_file_name(),
    };
    if named
        .as_deref()
        .is_some_and(|file| ffi::rand_load_file(file) >= 0)
    {
        return true;
    }
    if !ffi::rand_status() {
        store_errors(req);
        docref(
            req,
            Level::Warning,
            "Unable to load random state; not enough random data!",
        );
    }
    false
}

/// `php_openssl_write_rand_file`: the generator's state written back to
/// the seed file it was read from.
fn write_rand_file(req: &mut Request, file: Option<&CStr>) {
    let named = match file {
        Some(file) => Some(file.to_owned()),
        None => ffi::rand_file_name(),
    };
    if named
        .as_deref()
        .is_none_or(|file| ffi::rand_write_file(file) < 0)
    {
        store_errors(req);
        docref(req, Level::Warning, "Unable to write random state");
    }
}
