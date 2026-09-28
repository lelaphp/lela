//! The `openssl` extension: `php-src/ext/openssl` as one crate under
//! `ext/` (ADR-0004), over the platform's libcrypto bound in [`ffi`] alone
//! (design §7.2). Its state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/openssl/tests`, through `xtask phpt --subset`.
//!
//! Declared by the attributes as `openssl.stub.php` at the tag declares
//! it: the ini entries, the constants, the asymmetric keys with
//! `OpenSSLAsymmetricKey` ([`key`]) and the configuration they are made
//! under ([`config`]), the signatures ([`sign`]) and the ring of library
//! errors `openssl_error_string` reads ([`Errors`]). ext/phar's OpenSSL
//! signatures call `openssl_sign` and `openssl_verify` through the
//! function table, as php's do.
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

mod config;
mod ffi;
mod key;
mod sign;

use std::ffi::{c_int, c_ulong};

use php_ext::abi::{Extension, ExtensionBuilder, OrFalse};
use php_ext::attr::{php_const, php_function};
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::PhpString;

/// `register_openssl_symbols`: the constants in `openssl.stub.php`'s
/// order, as the oracle's OpenSSL 3 build declares them (no `MD2`, no
/// `SSLV23_PADDING`). The two version constants are the linked
/// library's.
///
/// ```php
/// var_dump(OPENSSL_ALGO_SHA1, OPENSSL_ALGO_SHA256, OPENSSL_ALGO_SHA512, OPENSSL_ALGO_RMD160, OPENSSL_KEYTYPE_RSA, OPENSSL_KEYTYPE_EC, OPENSSL_PKCS1_PSS_PADDING, OPENSSL_CMS_NOSIGS, PKCS7_NO_DUAL_CONTENT, OPENSSL_ENCODING_PEM);
/// var_dump(defined('OPENSSL_ALGO_MD2'), is_string(OPENSSL_VERSION_TEXT), OPENSSL_VERSION_NUMBER >= 0x30000000);
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
/// ```
#[php_const]
#[allow(non_snake_case)]
pub(crate) mod constants {
    /// `OPENSSL_VERSION_TEXT`, the linked library's version line.
    pub(crate) fn OPENSSL_VERSION_TEXT() -> String {
        String::from_utf8_lossy(&super::ffi::version_text()).into_owned()
    }
    /// `OPENSSL_VERSION_NUMBER`, the linked library's.
    pub(crate) fn OPENSSL_VERSION_NUMBER() -> i64 {
        super::ffi::version_number()
    }
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
fn openssl_error_string(req: &mut Request) -> Result<OrFalse<PhpString>, Thrown> {
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
/// the class and the natives in `openssl.stub.php`'s order, and the
/// state declared on it.
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
    let ini = |name: &str, default: &str| IniDecl {
        name: Name(name.as_bytes().into()),
        default: default.as_bytes().into(),
        user: false,
        per_dir: true,
        system: true,
    };
    ExtensionBuilder::default()
        .ini(vec![
            ini("openssl.cafile", ""),
            ini("openssl.capath", ""),
            ini("openssl.libctx", "custom"),
        ])
        .constants(constants::declared())
        .class::<key::OpenSSLAsymmetricKey>()
        .functions(vec![
            key::openssl_pkey_new::declared(),
            key::openssl_pkey_export::declared(),
            key::openssl_pkey_get_public::declared(),
            key::openssl_get_publickey::declared(),
            key::openssl_pkey_free::declared(),
            key::openssl_free_key::declared(),
            key::openssl_pkey_get_private::declared(),
            key::openssl_get_privatekey::declared(),
            key::openssl_pkey_get_details::declared(),
            openssl_error_string::declared(),
            sign::openssl_sign::declared(),
            sign::openssl_verify::declared(),
        ])
        .request_state::<State>()
        .info(info)
        .build("openssl")
}

/// `PHP_MINFO_FUNCTION(openssl)`: the table, then `DISPLAY_INI_ENTRIES`.
/// The header's version is the library's: the crate is built against the
/// headers of the library it links.
///
/// ```php
/// $e = new ReflectionExtension('openssl'); ob_start(); $e->info(); $out = ob_get_clean();
/// echo preg_replace(['/Version => .*/', '/config => .*/'], ['Version => V', 'config => C'], $out);
/// ```
/// ```expect
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
    req.info_table(&[
        &["OpenSSL support", "enabled"],
        &["OpenSSL Library Version", &version],
        &["OpenSSL Header Version", &version],
        &["Openssl default config", &conf],
    ]);
    req.info_ini_entries(&[b"openssl.cafile", b"openssl.capath", b"openssl.libctx"]);
}
