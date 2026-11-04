//! The `openssl` extension: `php-src/ext/openssl` as one crate under
//! `ext/` (ADR-0004), over the platform's libcrypto bound in [`ffi`] alone
//! (design §7.2). Its state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/openssl/tests`, through `xtask phpt --subset`.
//!
//! Declared by the attributes as `openssl.stub.php` at the tag declares it:
//! the ini entries, the constants, the three classes, and the natives by
//! area: certificates ([`x509`]), signing requests ([`csr`]), asymmetric
//! keys ([`key`], [`keydata`]) and the configuration they are made under
//! ([`config`]), signatures and raw RSA ([`sign`]), ciphers, digests and
//! envelopes ([`cipher`]), PKCS#7 ([`pkcs7`]), CMS ([`cms`]), PKCS#12
//! ([`pkcs12`]), and SPKAC ([`spki`]), with the arguments they share
//! decoded once ([`decode`]) and the ring of library errors
//! `openssl_error_string` reads ([`Errors`]). ext/phar's OpenSSL signatures
//! call `openssl_sign` and `openssl_verify` through the function table, as
//! php's do.
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod cipher;
mod cms;
mod config;
mod csr;
mod decode;
mod ffi;
mod key;
mod keydata;
mod pkcs12;
mod pkcs7;
mod sign;
mod spki;
mod throwables;
mod x509;
mod xp_ssl;

/// The values php compiles in from `<openssl/opensslv.h>`, read by
/// `build.rs` off the headers openssl-sys found, as php's `@cvalue` reads
/// the macro at its build (#1974): after the library is upgraded without
/// a rebuild, these keep the headers' version as php's do.
mod header {
    include!(concat!(env!("OUT_DIR"), "/header.rs"));
}

use std::ffi::{c_int, c_ulong};

use php_ext::abi::{Extension, ExtensionBuilder, IniStage, OrFalse, Refusals};
use php_ext::attr::{php_const, php_function};
use php_ext::decl::IniDecl;
use php_ext::raising::Thrown;
use php_ext::request::{Request, Streams as _};
use php_ext::values::{Owner, PhpArray, PhpString, Value};

/// `register_openssl_symbols`: the constants in `openssl.stub.php`'s
/// order, as the oracle's OpenSSL 4 build declares them (no `MD2`, no
/// `SSLV23_PADDING`). The two version constants are the headers' (see
/// [`header`]), which agree with each other whatever libcrypto is loaded.
///
/// ```php
/// var_dump(OPENSSL_ALGO_SHA1, OPENSSL_ALGO_SHA256, OPENSSL_ALGO_SHA512, OPENSSL_ALGO_RMD160, OPENSSL_KEYTYPE_RSA, OPENSSL_KEYTYPE_EC, OPENSSL_PKCS1_PSS_PADDING, OPENSSL_CMS_NOSIGS, PKCS7_NO_DUAL_CONTENT, OPENSSL_ENCODING_PEM);
/// var_dump(defined('OPENSSL_ALGO_MD2'), is_string(OPENSSL_VERSION_TEXT), OPENSSL_VERSION_NUMBER >= 0x30000000);
/// $n = OPENSSL_VERSION_NUMBER;
/// var_dump(str_starts_with(OPENSSL_VERSION_TEXT, sprintf('OpenSSL %d.%d.%d ', $n >> 28, ($n >> 20) & 0xff, ($n >> 4) & 0xff)));
/// ```
/// ```expect
/// int(1)
/// int(7)
/// int(9)
/// int(10)
/// int(0)
/// int(3)
/// int(6)
/// int(12)
/// int(65536)
/// int(2)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
#[php_const]
#[allow(non_snake_case)]
pub(crate) mod constants {
    /// `OPENSSL_VERSION_TEXT`, `@cvalue` of the header macro: the
    /// version line of the headers this build compiled against, not the
    /// libcrypto it loads (`OpenSSL_version`, `--ri`'s "Library Version").
    pub(crate) const OPENSSL_VERSION_TEXT: &str = super::header::OPENSSL_VERSION_TEXT;
    /// `OPENSSL_VERSION_NUMBER`, `@cvalue` of the header macro, of the same
    /// headers.
    pub(crate) const OPENSSL_VERSION_NUMBER: i64 = super::header::OPENSSL_VERSION_NUMBER;
    pub(crate) const X509_PURPOSE_SSL_CLIENT: i64 = 1;
    pub(crate) const X509_PURPOSE_SSL_SERVER: i64 = 2;
    pub(crate) const X509_PURPOSE_NS_SSL_SERVER: i64 = 3;
    pub(crate) const X509_PURPOSE_SMIME_SIGN: i64 = 4;
    pub(crate) const X509_PURPOSE_SMIME_ENCRYPT: i64 = 5;
    pub(crate) const X509_PURPOSE_CRL_SIGN: i64 = 6;
    pub(crate) const X509_PURPOSE_ANY: i64 = 7;
    pub(crate) const X509_PURPOSE_OCSP_HELPER: i64 = 8;
    pub(crate) const X509_PURPOSE_TIMESTAMP_SIGN: i64 = 9;
    pub(crate) const OPENSSL_ALGO_SHA1: i64 = 1;
    pub(crate) const OPENSSL_ALGO_MD5: i64 = 2;
    pub(crate) const OPENSSL_ALGO_MD4: i64 = 3;
    pub(crate) const OPENSSL_ALGO_SHA224: i64 = 6;
    pub(crate) const OPENSSL_ALGO_SHA256: i64 = 7;
    pub(crate) const OPENSSL_ALGO_SHA384: i64 = 8;
    pub(crate) const OPENSSL_ALGO_SHA512: i64 = 9;
    pub(crate) const OPENSSL_ALGO_RMD160: i64 = 10;
    pub(crate) const PKCS7_DETACHED: i64 = 64;
    pub(crate) const PKCS7_TEXT: i64 = 1;
    pub(crate) const PKCS7_NOINTERN: i64 = 16;
    pub(crate) const PKCS7_NOVERIFY: i64 = 32;
    pub(crate) const PKCS7_NOCHAIN: i64 = 8;
    pub(crate) const PKCS7_NOCERTS: i64 = 2;
    pub(crate) const PKCS7_NOATTR: i64 = 256;
    pub(crate) const PKCS7_BINARY: i64 = 128;
    pub(crate) const PKCS7_NOSIGS: i64 = 4;
    pub(crate) const PKCS7_NOOLDMIMETYPE: i64 = 1024;
    pub(crate) const PKCS7_NOSMIMECAP: i64 = 512;
    pub(crate) const PKCS7_CRLFEOL: i64 = 2048;
    pub(crate) const PKCS7_NOCRL: i64 = 8192;
    pub(crate) const PKCS7_NO_DUAL_CONTENT: i64 = 65536;
    pub(crate) const OPENSSL_CMS_DETACHED: i64 = 64;
    pub(crate) const OPENSSL_CMS_TEXT: i64 = 1;
    pub(crate) const OPENSSL_CMS_NOINTERN: i64 = 16;
    pub(crate) const OPENSSL_CMS_NOVERIFY: i64 = 32;
    pub(crate) const OPENSSL_CMS_NOCERTS: i64 = 2;
    pub(crate) const OPENSSL_CMS_NOATTR: i64 = 256;
    pub(crate) const OPENSSL_CMS_BINARY: i64 = 128;
    pub(crate) const OPENSSL_CMS_NOSIGS: i64 = 12;
    pub(crate) const OPENSSL_CMS_OLDMIMETYPE: i64 = 1024;
    pub(crate) const OPENSSL_PKCS1_PADDING: i64 = 1;
    pub(crate) const OPENSSL_NO_PADDING: i64 = 3;
    pub(crate) const OPENSSL_PKCS1_OAEP_PADDING: i64 = 4;
    pub(crate) const OPENSSL_PKCS1_PSS_PADDING: i64 = 6;
    pub(crate) const OPENSSL_DEFAULT_STREAM_CIPHERS: &str = "ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:DHE-RSA-AES128-GCM-SHA256:DHE-DSS-AES128-GCM-SHA256:kEDH+AESGCM:ECDHE-RSA-AES128-SHA256:ECDHE-ECDSA-AES128-SHA256:ECDHE-RSA-AES128-SHA:ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES256-SHA384:ECDHE-ECDSA-AES256-SHA384:ECDHE-RSA-AES256-SHA:ECDHE-ECDSA-AES256-SHA:DHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA:DHE-DSS-AES128-SHA256:DHE-RSA-AES256-SHA256:DHE-DSS-AES256-SHA:DHE-RSA-AES256-SHA:AES128-GCM-SHA256:AES256-GCM-SHA384:AES128:AES256:HIGH:!SSLv2:!aNULL:!eNULL:!EXPORT:!DES:!MD5:!RC4:!ADH";
    pub(crate) const OPENSSL_CIPHER_RC2_40: i64 = 0;
    pub(crate) const OPENSSL_CIPHER_RC2_128: i64 = 1;
    pub(crate) const OPENSSL_CIPHER_RC2_64: i64 = 2;
    pub(crate) const OPENSSL_CIPHER_DES: i64 = 3;
    pub(crate) const OPENSSL_CIPHER_3DES: i64 = 4;
    pub(crate) const OPENSSL_CIPHER_AES_128_CBC: i64 = 5;
    pub(crate) const OPENSSL_CIPHER_AES_192_CBC: i64 = 6;
    pub(crate) const OPENSSL_CIPHER_AES_256_CBC: i64 = 7;
    pub(crate) const OPENSSL_KEYTYPE_RSA: i64 = 0;
    pub(crate) const OPENSSL_KEYTYPE_DSA: i64 = 1;
    pub(crate) const OPENSSL_KEYTYPE_DH: i64 = 2;
    pub(crate) const OPENSSL_KEYTYPE_EC: i64 = 3;
    pub(crate) const OPENSSL_KEYTYPE_X25519: i64 = 4;
    pub(crate) const OPENSSL_KEYTYPE_ED25519: i64 = 5;
    pub(crate) const OPENSSL_KEYTYPE_X448: i64 = 6;
    pub(crate) const OPENSSL_KEYTYPE_ED448: i64 = 7;
    pub(crate) const OPENSSL_RAW_DATA: i64 = 1;
    pub(crate) const OPENSSL_ZERO_PADDING: i64 = 2;
    pub(crate) const OPENSSL_DONT_ZERO_PAD_KEY: i64 = 4;
    pub(crate) const OPENSSL_TLSEXT_SERVER_NAME: i64 = 1;
    pub(crate) const OPENSSL_ENCODING_DER: i64 = 0;
    pub(crate) const OPENSSL_ENCODING_SMIME: i64 = 1;
    pub(crate) const OPENSSL_ENCODING_PEM: i64 = 2;
}

/// What the three classes' handlers refuse: `get_constructor` in the
/// words of the class's own init, `clone_obj` php's null slot and
/// `compare` `zend_objects_not_comparable`.
pub(crate) const fn refusals(instead: &'static str) -> Refusals {
    Refusals::NONE
        .construction(Some(instead))
        .cloning()
        .comparison()
}

/// `add_assoc_*`: a value under a string key, a numeric one taken as
/// the integer, as `zend_symtable_str_update` takes it.
pub(crate) fn put(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    name: &[u8],
    value: Owner<Value>,
) -> Result<(), Thrown> {
    array.set_symtable_str(req, name, value)
}

/// `add_assoc_stringl`: bytes as a string under a string key.
pub(crate) fn put_bytes(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    name: &[u8],
    bytes: &[u8],
) -> Result<(), Thrown> {
    let value = PhpString::new(req, bytes)?.into_value();
    put(req, array, name, value)
}

/// `make_digest_ex`: bytes as lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> Vec<u8> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|&b| [DIGITS[usize::from(b >> 4)], DIGITS[usize::from(b & 15)]])
        .collect()
}

/// `PHP_OPENSSL_ERR_BUFFER_SIZE`: how many codes the ring keeps.
const ERR_BUFFER_SIZE: usize = 16;

/// `struct php_openssl_errors`: the ring `php_openssl_store_errors`
/// fills from the library's queue and `openssl_error_string` empties,
/// the oldest code overwritten once sixteen are held. A code is kept as
/// php keeps it, in an `int`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Errors {
    buffer: [c_int; ERR_BUFFER_SIZE],
    top: usize,
    bottom: usize,
}

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised. `OPENSSL_G(errors)` and `OPENSSL_G(errors_mark)`, each
/// absent until first needed, as php allocates them.
#[derive(Default)]
struct State {
    errors: Option<Errors>,
    mark: Option<Errors>,
    /// What a function below a native threw that php leaves standing in `EG(exception)`
    /// while the native goes on to its end ([`decode::defer`]).
    thrown: Option<Thrown>,
}

/// `php_openssl_store_errors`: every code on the library's queue moved
/// into the ring.
pub(crate) fn store_errors(req: &mut Request) {
    let Some(first) = ffi::next_error() else {
        return;
    };
    let errors = req
        .state::<State>()
        .errors
        .get_or_insert_with(Errors::default);
    let mut code = Some(first);
    while let Some(next) = code {
        errors.top = (errors.top + 1) % ERR_BUFFER_SIZE;
        if errors.top == errors.bottom {
            errors.bottom = (errors.bottom + 1) % ERR_BUFFER_SIZE;
        }
        errors.buffer[errors.top] = as_int(next);
        code = ffi::next_error();
    }
}

/// `int error_code = ERR_get_error()`: the low 32 bits of a library
/// code, as C's conversion keeps them.
fn as_int(code: c_ulong) -> c_int {
    let bytes = code.to_le_bytes();
    i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// `unsigned long val = buffer[bottom]`: the `int` widened with its
/// sign, as C widens it.
fn as_code(value: c_int) -> Option<c_ulong> {
    c_ulong::try_from(u64::from_ne_bytes(i64::from(value).to_ne_bytes())).ok()
}

/// `php_openssl_errors_set_mark`: the ring as it stands, kept to go back
/// to.
pub(crate) fn errors_set_mark(req: &mut Request) {
    let state = req.state::<State>();
    if let Some(errors) = state.errors {
        state.mark = Some(errors);
    }
}

/// `php_openssl_errors_restore_mark`: the ring put back to the mark, or
/// emptied where there is none.
pub(crate) fn errors_restore_mark(req: &mut Request) {
    let state = req.state::<State>();
    if state.errors.is_none() {
        return;
    }
    state.errors = Some(state.mark.unwrap_or_default());
}

/// `openssl_error_string(): string|false`: the oldest code the ring
/// holds, taken off it, in the library's words; false once it is empty.
///
/// ```php
/// var_dump(openssl_error_string());
/// var_dump(openssl_pkey_get_private('not a key'));
/// while (($e = openssl_error_string()) !== false) { echo $e, "\n"; }
/// var_dump(openssl_error_string());
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// error:1E08010C:DECODER routines::unsupported
/// bool(false)
/// ```
#[php_function]
fn openssl_error_string(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    store_errors(req);
    let Some(errors) = req.state::<State>().errors.as_mut() else {
        return Ok(OrFalse::False);
    };
    if errors.top == errors.bottom {
        return Ok(OrFalse::False);
    }
    errors.bottom = (errors.bottom + 1) % ERR_BUFFER_SIZE;
    let value = errors.buffer[errors.bottom];
    let Some(code) = as_code(value).filter(|&code| code != 0) else {
        return Ok(OrFalse::False);
    };
    let text = ffi::error_text(code);
    Ok(OrFalse::Value(PhpString::new(req, &text)?))
}

/// The extension value (design §7.1): the ini entries, the constants,
/// the class and the natives in `openssl.stub.php`'s order, the socket
/// `transport`s `PHP_MINIT(openssl)` registers ([`xp_ssl::transports`]),
/// and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('openssl'), ini_get('openssl.libctx'), ini_get('openssl.cafile'));
/// ```
/// ```expect
/// bool(true)
/// string(6) "custom"
/// string(0) ""
/// ```
pub fn extension() -> Extension {
    let ini = |name: &'static str, default: Option<&'static str>| IniDecl {
        name: name.as_bytes(),
        default: default.map(str::as_bytes),
        user: false,
        per_dir: true,
        system: false,
    };
    let builder = xp_ssl::transports()
        .into_iter()
        .fold(ExtensionBuilder::default(), ExtensionBuilder::register);
    builder
        .ini(vec![
            ini("openssl.cafile", None),
            ini("openssl.capath", None),
            ini("openssl.libctx", Some("custom")),
        ])
        .constants(constants::DECLARED)
        .class::<x509::OpenSSLCertificate>()
        .class::<csr::OpenSSLCertificateSigningRequest>()
        .class::<key::OpenSSLAsymmetricKey>()
        .functions(&[
            x509::openssl_x509_export_to_file::DECLARED,
            x509::openssl_x509_export::DECLARED,
            x509::openssl_x509_fingerprint::DECLARED,
            x509::openssl_x509_check_private_key::DECLARED,
            x509::openssl_x509_verify::DECLARED,
            x509::openssl_x509_parse::DECLARED,
            x509::openssl_x509_checkpurpose::DECLARED,
            x509::openssl_x509_read::DECLARED,
            x509::openssl_x509_free::DECLARED,
            pkcs12::openssl_pkcs12_export_to_file::DECLARED,
            pkcs12::openssl_pkcs12_export::DECLARED,
            pkcs12::openssl_pkcs12_read::DECLARED,
            csr::openssl_csr_export_to_file::DECLARED,
            csr::openssl_csr_export::DECLARED,
            csr::openssl_csr_sign::DECLARED,
            csr::openssl_csr_new::DECLARED,
            csr::openssl_csr_get_subject::DECLARED,
            csr::openssl_csr_get_public_key::DECLARED,
            key::openssl_pkey_new::DECLARED,
            key::openssl_pkey_export_to_file::DECLARED,
            key::openssl_pkey_export::DECLARED,
            key::openssl_pkey_get_public::DECLARED,
            key::openssl_get_publickey::DECLARED,
            key::openssl_pkey_free::DECLARED,
            key::openssl_free_key::DECLARED,
            key::openssl_pkey_get_private::DECLARED,
            key::openssl_get_privatekey::DECLARED,
            key::openssl_pkey_get_details::DECLARED,
            cipher::openssl_pbkdf2::DECLARED,
            pkcs7::openssl_pkcs7_verify::DECLARED,
            pkcs7::openssl_pkcs7_encrypt::DECLARED,
            pkcs7::openssl_pkcs7_sign::DECLARED,
            pkcs7::openssl_pkcs7_decrypt::DECLARED,
            pkcs7::openssl_pkcs7_read::DECLARED,
            cms::openssl_cms_verify::DECLARED,
            cms::openssl_cms_encrypt::DECLARED,
            cms::openssl_cms_sign::DECLARED,
            cms::openssl_cms_decrypt::DECLARED,
            cms::openssl_cms_read::DECLARED,
            sign::openssl_private_encrypt::DECLARED,
            sign::openssl_private_decrypt::DECLARED,
            sign::openssl_public_encrypt::DECLARED,
            sign::openssl_public_decrypt::DECLARED,
            openssl_error_string::DECLARED,
            sign::openssl_sign::DECLARED,
            sign::openssl_verify::DECLARED,
            cipher::openssl_seal::DECLARED,
            cipher::openssl_open::DECLARED,
            cipher::openssl_get_md_methods::DECLARED,
            cipher::openssl_get_cipher_methods::DECLARED,
            key::openssl_get_curve_names::DECLARED,
            cipher::openssl_digest::DECLARED,
            cipher::openssl_encrypt::DECLARED,
            cipher::openssl_decrypt::DECLARED,
            cipher::openssl_cipher_iv_length::DECLARED,
            cipher::openssl_cipher_key_length::DECLARED,
            key::openssl_dh_compute_key::DECLARED,
            key::openssl_pkey_derive::DECLARED,
            cipher::openssl_random_pseudo_bytes::DECLARED,
            spki::openssl_spki_new::DECLARED,
            spki::openssl_spki_verify::DECLARED,
            spki::openssl_spki_export::DECLARED,
            spki::openssl_spki_export_challenge::DECLARED,
            x509::openssl_get_cert_locations::DECLARED,
        ])
        .on_modify(b"openssl.libctx", on_update_libctx)
        .process_state(|| Ok(initialize()))
        .request_state::<State>()
        .engine_state(throwables::Throwables::start)
        .rinit(rinit)
        .info(info)
        .build("openssl")
}

/// `OnUpdateLibCtx`: `default` and `custom` name the two library
/// contexts php keeps; any other value is refused, fatally as the engine
/// starts and with a warning after. The entry is `PHP_INI_PERDIR`, so
/// `ini_set()` refuses it before the handler and a program meets the
/// value the host started with.
///
/// ```php
/// var_dump(ini_get('openssl.libctx'), ini_set('openssl.libctx', 'default'));
/// ```
/// ```expect
/// string(6) "custom"
/// bool(false)
/// ```
#[allow(clippy::unnecessary_wraps)] // the ABI's `OnModify`
fn on_update_libctx(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    if value == b"default" || value == b"custom" {
        return Ok(true);
    }
    let level = if req.ini().stage() == IniStage::Startup {
        php_ext::raising::Level::Error
    } else {
        php_ext::raising::Level::Warning
    };
    let text = format!(
        "OpenSSL libctx \"{}\" cannot be found",
        String::from_utf8_lossy(value)
    );
    php_ext::raising::docref(req, level, &text);
    Ok(false)
}

/// The process scope's proof that the library is started (ADR-0010).
struct Initialized;

/// `PHP_MINIT_FUNCTION(openssl)`'s `php_openssl_backend_init`
/// (`Z/ext/openssl/openssl.c:421`, `Z/ext/openssl/openssl_backend_v3.c:27-34`),
/// the process start: called once for the process, the first time an
/// engine carrying openssl is built. It cannot fail.
fn initialize() -> Initialized {
    ffi::start();
    Initialized
}

/// The library's error queue emptied as a request starts: it is the
/// thread's, and a thread may have served a request before, where php's
/// CLI starts each program in a fresh process with an empty queue. Then
/// the two wrappers `PHP_MINIT(openssl)` registers,
/// `php_stream_http_wrapper` again as `https` and `php_stream_ftp_wrapper`
/// as `ftps`, which this engine keeps in the request's table, after
/// `standard`'s and ahead of `zlib`'s.
///
/// ```php
/// $w = stream_get_wrappers();
/// var_dump(array_search('https', $w) < array_search('compress.zlib', $w), array_search('ftps', $w) === array_search('https', $w) + 1);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
#[allow(clippy::unnecessary_wraps)] // the ABI's `rinit` hook
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    ffi::clear_errors();
    let mut https = php_ext_standard::http_wrapper();
    https.scheme = String::from("https");
    req.register_wrapper(https);
    let mut ftps = php_ext_standard::ftp_wrapper();
    ftps.scheme = String::from("ftps");
    req.register_wrapper(ftps);
    Ok(())
}

/// `PHP_MINFO_FUNCTION(openssl)`: the table, then `DISPLAY_INI_ENTRIES`.
/// "Library Version" is the libcrypto loaded, `OpenSSL_version`; "Header
/// Version" is `OPENSSL_VERSION_TEXT`, the headers this build compiled
/// against ([`header`]).
///
/// ```php
/// $e = new ReflectionExtension('openssl'); ob_start(); $e->info(); $out = ob_get_clean();
/// var_dump(str_contains($out, "OpenSSL Header Version => " . OPENSSL_VERSION_TEXT . "\n"));
/// echo preg_replace(['/Version => .*/', '/config => .*/'], ['Version => V', 'config => C'], $out);
/// ```
/// ```expect
/// bool(true)
///
/// openssl
///
/// OpenSSL support => enabled
/// OpenSSL Library Version => V
/// OpenSSL Header Version => V
/// Openssl default config => C
///
/// Directive => Local Value => Master Value
/// openssl.cafile => no value => no value
/// openssl.capath => no value => no value
/// openssl.libctx => custom => custom
/// ```
fn info(req: &mut Request) {
    let version = String::from_utf8_lossy(&ffi::version_text()).into_owned();
    let conf = String::from_utf8_lossy(&config::default_file()).into_owned();
    req.extensions_mut().info_table(&[
        &["OpenSSL support", "enabled"],
        &["OpenSSL Library Version", &version],
        &["OpenSSL Header Version", header::OPENSSL_VERSION_TEXT],
        &["Openssl default config", &conf],
    ]);
    req.ini_mut()
        .info_ini_entries(&[b"openssl.cafile", b"openssl.capath", b"openssl.libctx"]);
}
