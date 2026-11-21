//! `struct php_x509_request` as the natives read it:
//! `php_openssl_parse_config` over the `$options` array and the
//! configuration file it names, decoded once into [`Settings`], and
//! `php_openssl_generate_private_key`, which makes a key under it
//! (`openssl_backend_common.c`).

use std::ffi::{CStr, CString, OsStr, OsString, c_int};
use std::os::unix::ffi::OsStrExt;

use php_ext::raising::{Level, docref};
use php_ext::request::Request;
use php_ext::values::{PhpArray, Value, ValueRef};

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
/// else `openssl.cnf` in the library's certificate area, as [`choose`]
/// picks among them, the macOS tarball's own file standing last.
///
/// The variables win, and one naming no file fails the call, as php's
/// does; one naming a file is read, here with a key size of its own.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'cnf');
/// file_put_contents($f, "[ req ]\ndefault_bits = 1024\n");
/// $child = '$k = openssl_pkey_new(); var_dump($k === false ? false : openssl_pkey_get_details($k)["bits"]);';
/// foreach ([['OPENSSL_CONF' => $f], ['SSLEAY_CONF' => $f], ['OPENSSL_CONF' => $f, 'SSLEAY_CONF' => '/nonexistent.cnf'],
///     ['OPENSSL_CONF' => '/nonexistent.cnf', 'SSLEAY_CONF' => $f], ['SSLEAY_CONF' => '/nonexistent.cnf']] as $env) {
///     $p = proc_open([PHP_BINARY, '-n', '-r', $child], [1 => ['pipe', 'w']], $pipes, null, $env);
///     echo stream_get_contents($pipes[1]);
///     proc_close($p);
/// }
/// unlink($f);
/// ```
/// ```expect process
/// int(1024)
/// int(1024)
/// int(1024)
/// bool(false)
/// bool(false)
/// ```
pub(crate) fn default_file() -> Vec<u8> {
    let named = ["OPENSSL_CONF", "SSLEAY_CONF"]
        .into_iter()
        .find_map(std::env::var_os)
        .map(OsString::into_encoded_bytes);
    let mut compiled_in = ffi::default_cert_area();
    compiled_in.extend_from_slice(b"/openssl.cnf");
    choose(named, compiled_in, readable, laid_file)
}

/// The file a variable names, as php reads it; else the compiled-in one,
/// where it can be read or `laid` finds no file of the tarball's; else the
/// tarball's. The bundled `libcrypto` of the macOS tarball compiles in
/// Homebrew's directory, which a Mac without Homebrew's OpenSSL lacks, and
/// the tarball lays OpenSSL's stock file beside the libraries for it.
/// `readable` and `laid` are asked only where they decide.
fn choose(
    named: Option<Vec<u8>>,
    compiled_in: Vec<u8>,
    readable: impl FnOnce(&[u8]) -> bool,
    laid: impl FnOnce() -> Option<Vec<u8>>,
) -> Vec<u8> {
    if let Some(named) = named {
        return named;
    }
    if readable(&compiled_in) {
        return compiled_in;
    }
    laid().unwrap_or(compiled_in)
}

/// Whether a file opens for reading, as `NCONF_load` opens it.
fn readable(path: &[u8]) -> bool {
    std::fs::File::open(OsStr::from_bytes(path)).is_ok()
}

/// Where the macOS tarball lays its `openssl.cnf`, from the tarball's
/// directory: beside the bundled libraries, as dist/bundle.py lays it.
const LAID: &str = "lib/openssl.cnf";

/// The tarball's `openssl.cnf`, where the running host is one of a macOS
/// tarball that carries it: [`LAID`] under the directory above the host's
/// real file's own, `bin/` or `sbin/`, where that file can be read. `None`
/// on any other platform, and for a host of a cargo build or an image,
/// none of which has the file there.
fn laid_file() -> Option<Vec<u8>> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let host = std::fs::canonicalize(std::env::current_exe().ok()?).ok()?;
    let file = host.parent()?.parent()?.join(LAID);
    let file = file.into_os_string().into_encoded_bytes();
    readable(&file).then_some(file)
}

/// `(int)Z_LVAL_P(item)`: the low 32 bits, as C's conversion keeps them.
pub(crate) fn as_int(n: i64) -> c_int {
    let bytes = n.to_le_bytes();
    i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// The `$options` of a native that parses a configuration, `a!`: each
/// entry read as `SET_OPTIONAL_STRING_ARG` and `SET_OPTIONAL_LONG_ARG`
/// read it, of its own type or not at all.
struct Options(Option<PhpArray>);

impl Options {
    /// The entry under `name`, as it is, a reference not followed:
    /// `zend_hash_str_find(Z_ARRVAL_P(args), name)`.
    fn entry(&self, name: &[u8]) -> Option<Value> {
        self.0.and_then(|array| array.find(name))
    }

    /// A string entry, as C reads it.
    fn string(&self, name: &[u8]) -> Option<CString> {
        self.entry(name).and_then(|value| match value.as_ref() {
            ValueRef::Str(s) => Some(c_string(s.as_bytes())),
            _ => None,
        })
    }

    /// An integer entry.
    fn long(&self, name: &[u8]) -> Option<i64> {
        self.entry(name).and_then(|value| match value.as_ref() {
            ValueRef::Int(n) => Some(n),
            _ => None,
        })
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
/// Warning: openssl_csr_new(): Error loading extensions_section section nope of %s/openssl.cnf in %s on line 8
/// bool(false)
/// ```
pub(crate) fn parse(req: &mut Request, options: Option<PhpArray>) -> Option<Settings> {
    let options = Options(options);
    let (file, section, conf) = load(req, &options)?;
    let setting =
        |conf: &Conf, name: &CStr| conf.string(Some(&section), name).map(|v| c_string(&v));
    let digest_name = match options.string(b"digest_alg") {
        Some(name) => Some(name),
        None => setting(&conf, c"default_md"),
    };
    let extensions = match options.string(b"x509_extensions") {
        Some(name) => Some(name),
        None => setting(&conf, c"x509_extensions"),
    };
    let request_extensions = match options.string(b"req_extensions") {
        Some(name) => Some(name),
        None => setting(&conf, c"req_extensions"),
    };
    let bits = match options.long(b"private_key_bits") {
        Some(bits) => as_int(bits),
        None => c_int::try_from(conf.number(Some(&section), c"default_bits")).unwrap_or(0),
    };
    let key_type = options
        .long(b"private_key_type")
        .map_or(Some(KeyKind::Rsa), |n| kind_of(i64::from(as_int(n))));

    let encrypt = if let Some(value) = options.entry(b"encrypt_key") {
        matches!(value.as_ref(), ValueRef::Bool(true))
    } else {
        let said = conf
            .string(Some(&section), c"encrypt_rsa_key")
            .or_else(|| conf.string(Some(&section), c"encrypt_key"));
        said.as_deref() != Some(b"no")
    };

    let mut cipher = None;
    if encrypt && let Some(algo) = options.long(b"encrypt_key_cipher") {
        let Some(found) = cipher_of(algo) else {
            docref(
                req,
                Level::Warning,
                "Unknown cipher algorithm for private key",
            );
            return None;
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
        return None;
    }

    let mut curve = 0;
    if let Some(name) = options.string(b"curve_name") {
        curve = ffi::short_name_nid(&name);
        if curve == 0 {
            let text = format!(
                "Unknown elliptic curve (short) name {}",
                String::from_utf8_lossy(name.as_bytes())
            );
            docref(req, Level::Warning, &text);
            return None;
        }
    }

    if !string_mask(req, &conf, &section) {
        return None;
    }

    let label = "request_extensions_section";
    if !syntax_check(req, &conf, &file, label, request_extensions.as_deref()) {
        return None;
    }

    Some(Settings {
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
    })
}

/// The configuration file `config` names, or the default one, read
/// after the default one is, under the section `config_section_name`
/// names, with its OIDs made; `None` where it does not read, or after
/// php's warning where an OID cannot be made.
fn load(req: &mut Request, options: &Options) -> Option<(CString, CString, Conf)> {
    let file = match options.string(b"config") {
        Some(file) => file,
        None => c_string(&default_file()),
    };
    let section = options
        .string(b"config_section_name")
        .unwrap_or_else(|| c"req".to_owned());
    let default = c_string(&default_file());
    if !Conf::new().is_some_and(|mut global| global.load(&default)) {
        store_errors(req);
    }
    let mut conf = Conf::new()?;
    if !conf.load(&file) || !load_oids(req, &conf) {
        return None;
    }
    Some((file, section, conf))
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
fn load_oids(req: &mut Request, conf: &Conf) -> bool {
    if let Some(file) = conf.string(None, c"oid_file") {
        let origin = Origin::Setting("oid_file");
        let path = check_path(req, c_string(&file).as_bytes(), origin, false);
        if let Some(bio) = path.and_then(|path| Bio::file(&path, c"r")) {
            ffi::create_objects(&bio);
            drop(bio);
            store_errors(req);
        }
    }
    let Some(section) = conf.string(None, c"oid_section") else {
        return true;
    };
    let Some(lines) = conf.section(&c_string(&section)) else {
        store_errors(req);
        let text = format!(
            "Problem loading oid section {}",
            String::from_utf8_lossy(&section)
        );
        docref(req, Level::Warning, &text);
        return false;
    };
    for (name, value) in lines {
        let (name_c, value_c) = (c_string(&name), c_string(&value));
        let named = |name: &CStr| ffi::short_name_nid(name) != 0 || ffi::long_name_nid(name) != 0;
        if named(&name_c) {
            continue;
        }
        if ffi::create_object(&value_c, &name_c, &name_c) == 0 {
            // The object table is the process's: a request on another
            // thread can create the name between the look-up and the
            // create, which then refuses it as `OBJ_R_OID_EXISTS`. The
            // name stands, so nothing failed; the refusal is the only
            // thing on this thread's queue, and it is dropped.
            if named(&name_c) {
                ffi::clear_errors();
                continue;
            }
            store_errors(req);
            let text = format!(
                "Problem creating object {}={}",
                String::from_utf8_lossy(&name),
                String::from_utf8_lossy(&value)
            );
            docref(req, Level::Warning, &text);
            return false;
        }
    }
    true
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

#[cfg(test)]
mod tests {
    use super::choose;

    /// A variable wins whether or not its file can be read, and the
    /// compiled-in file is read where it can be; the tarball's file is
    /// taken only where neither variable names one and the compiled-in file
    /// cannot be read, and php's name stands where no tarball's file is.
    #[test]
    fn the_tarballs_file_stands_in_only_for_an_unreadable_compiled_in_one() {
        let compiled_in = b"/opt/homebrew/etc/openssl@4/openssl.cnf".to_vec();
        let laid = b"/Users/a/lela/lib/openssl.cnf".to_vec();
        let unasked = |_: &[u8]| -> bool { panic!("a named file is not looked for") };
        let unlaid = || -> Option<Vec<u8>> { panic!("the tarball's file is asked only last") };

        let named = b"/nonexistent.cnf".to_vec();
        let got = choose(Some(named.clone()), compiled_in.clone(), unasked, unlaid);
        assert_eq!(got, named);

        let got = choose(None, compiled_in.clone(), |_| true, unlaid);
        assert_eq!(got, compiled_in);

        let looked = |path: &[u8]| {
            assert_eq!(path, b"/opt/homebrew/etc/openssl@4/openssl.cnf");
            false
        };
        let got = choose(None, compiled_in.clone(), looked, || Some(laid.clone()));
        assert_eq!(got, laid);

        let got = choose(None, compiled_in.clone(), |_| false, || None);
        assert_eq!(got, compiled_in);
    }

    /// A host of a cargo build lies under `target/<profile>/`, whose parent
    /// lays no `lib/openssl.cnf`, so its answer is php's.
    #[test]
    fn a_cargo_built_host_finds_no_tarballs_file() {
        assert_eq!(super::laid_file(), None);
    }
}
