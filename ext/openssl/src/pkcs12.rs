//! The `openssl_pkcs12_*` natives of openssl.c: a certificate, its
//! private key and extra certificates written to a PKCS#12 key store,
//! and one read back as PEM.

use php_ext::abi::{Path, Ref, Untyped};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};

use crate::config::field;
use crate::decode::{
    Arg, CertParam, Origin, c_string, cert_from_param, cert_stack, check_path, finish, raised,
};
use crate::ffi::smime::Pkcs12;
use crate::ffi::{Bio, Certificate, PKey};
use crate::key::{Half, check_int_len, key_from};
use crate::{put, put_bytes, store_errors};

/// What both exports read before they write: the certificate, its
/// private key, and the `friendly_name` and `extracerts` options, after
/// php's warnings; `None` where one cannot be had.
fn prepare(
    req: &mut Request,
    certificate: &CertParam,
    private_key: &Value,
    options: PhpArray,
    to_file: Option<&Path>,
) -> Result<Option<(Pkcs12Input, Option<std::ffi::CString>)>, Thrown> {
    let Some(cert) = cert_from_param(req, certificate, Arg::new(1, "certificate"))? else {
        docref(req, Level::Warning, "X.509 Certificate cannot be retrieved");
        return Ok(None);
    };
    let arg = Arg::new(3, "private_key");
    let Some(key) = key_from(req, private_key, Half::Private, Some(b""), arg)? else {
        if !raised(req) {
            docref(
                req,
                Level::Warning,
                "Cannot get private key from parameter 3",
            );
        }
        return Ok(None);
    };
    if !cert.matches_key(&key) {
        if to_file.is_some() {
            store_errors(req);
        }
        docref(
            req,
            Level::Warning,
            "Private key does not correspond to cert",
        );
        return Ok(None);
    }
    let path = match to_file {
        Some(file) => {
            let origin = Origin::Arg(Arg::new(2, "output_filename"));
            match check_path(req, file.0.as_bytes(), origin, false)? {
                Some(path) => Some(path),
                None => return Ok(None),
            }
        }
        None => None,
    };
    let name = field(req, options, b"friendly_name")?.and_then(|value| match value.as_ref() {
        ValueRef::Str(s) => Some(c_string(s.as_bytes())),
        _ => None,
    });
    let extra = match field(req, options, b"extracerts")? {
        Some(value) => match cert_stack(req, &value, Arg::new(5, "options"), Some("extracerts"))? {
            Some(stack) => Some(stack),
            None => return Ok(None),
        },
        None => None,
    };
    Ok(Some((
        Pkcs12Input {
            cert,
            key,
            name,
            extra,
        },
        path,
    )))
}

/// A key store's contents before `PKCS12_create`.
struct Pkcs12Input {
    cert: Certificate,
    key: PKey,
    name: Option<std::ffi::CString>,
    extra: Option<crate::ffi::x509::CertStack>,
}

impl Pkcs12Input {
    /// `PKCS12_create(pass, name, key, cert, ca, 0, 0, 0, 0, 0)`, the
    /// passphrase read as C reads it.
    fn create(&self, passphrase: PhpString) -> Option<Pkcs12> {
        let pass = c_string(passphrase.as_bytes());
        Pkcs12::create(
            &pass,
            self.name.as_deref(),
            &self.key,
            &self.cert,
            self.extra.as_ref(),
        )
    }
}

/// `openssl_pkcs12_export_to_file(OpenSSLCertificate|string`
/// `$certificate, string $output_filename, $private_key, string`
/// `$passphrase, array $options = []): bool`.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $f = __DIR__ . '/k.p12';
/// var_dump(openssl_pkcs12_export_to_file($x, $f, $k, 'pw', ['friendly_name' => 'me']), openssl_pkcs12_read(file_get_contents($f), $out, 'pw'), array_keys($out));
/// var_dump(openssl_pkcs12_export_to_file($x, $f, openssl_pkey_new(['private_key_bits' => 1024]), 'pw'));
/// unlink($f);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// array(2) {
///   [0]=>
///   string(4) "cert"
///   [1]=>
///   string(4) "pkey"
/// }
///
/// Warning: openssl_pkcs12_export_to_file(): Private key does not correspond to cert in %s on line 6
/// bool(false)
/// ```
#[php_function]
fn openssl_pkcs12_export_to_file(
    req: &mut Request,
    certificate: CertParam,
    output_filename: Path,
    #[php(sensitive)] private_key: Untyped<Value>,
    #[php(sensitive)] passphrase: PhpString,
    #[php(default = [])] options: PhpArray,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let Some((input, path)) = prepare(
            req,
            &certificate,
            &private_key.0,
            options,
            Some(&output_filename),
        )?
        else {
            return Ok(false);
        };
        let Some(path) = path else {
            return Ok(false);
        };
        let Some(p12) = input.create(passphrase) else {
            store_errors(req);
            return Ok(false);
        };
        let shown = String::from_utf8_lossy(path.as_bytes()).into_owned();
        let Some(out) = Bio::file(&path, c"w") else {
            store_errors(req);
            docref(req, Level::Warning, &format!("Error opening file {shown}"));
            return Ok(false);
        };
        if !p12.write_der(&out) {
            store_errors(req);
            docref(
                req,
                Level::Warning,
                &format!("Error writing to file {shown}"),
            );
            return Ok(false);
        }
        Ok(true)
    })
}

/// `openssl_pkcs12_export(OpenSSLCertificate|string $certificate,`
/// `&$output, $private_key, string $passphrase, array $options = []):`
/// `bool`: the key store as DER.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// var_dump(openssl_pkcs12_export($x, $p12, $k, 'pw', ['extracerts' => [$x]]), openssl_pkcs12_read($p12, $out, 'pw'), count($out['extracerts']));
/// var_dump(openssl_pkcs12_export('nope', $p12, $k, 'pw'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// int(1)
///
/// Warning: openssl_pkcs12_export(): X.509 Certificate cannot be retrieved in %s on line 5
/// bool(false)
/// ```
#[php_function]
fn openssl_pkcs12_export(
    req: &mut Request,
    certificate: CertParam,
    output: Ref<Untyped<Value>>,
    #[php(sensitive)] private_key: Untyped<Value>,
    #[php(sensitive)] passphrase: PhpString,
    #[php(default = [])] options: PhpArray,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let Some((input, _)) = prepare(req, &certificate, &private_key.0, options, None)? else {
            return Ok(false);
        };
        let Some(p12) = input.create(passphrase) else {
            store_errors(req);
            return Ok(false);
        };
        let Some(out) = Bio::memory().filter(|out| p12.write_der(out)) else {
            store_errors(req);
            return Ok(false);
        };
        let der = PhpString::new(req, &out.contents())?;
        output.set(req, Untyped(Value::string(der)))?;
        Ok(true)
    })
}

/// `openssl_pkcs12_read(string $pkcs12, &$certificates, string`
/// `$passphrase): bool`: the key store's certificate as `cert`, its key as
/// `pkey` and its extra certificates as `extracerts`, each as PEM.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// openssl_pkcs12_export($x, $p12, $k, 'pw');
/// var_dump(openssl_pkcs12_read($p12, $out, 'pw'), array_keys($out), substr($out['pkey'], 0, 27));
/// var_dump(openssl_pkcs12_read($p12, $out, 'wrong'), openssl_pkcs12_read('', $out, 'pw'));
/// ```
/// ```expect
/// bool(true)
/// array(2) {
///   [0]=>
///   string(4) "cert"
///   [1]=>
///   string(4) "pkey"
/// }
/// string(27) "-----BEGIN PRIVATE KEY-----"
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_pkcs12_read(
    req: &mut Request,
    pkcs12: PhpString,
    certificates: Ref<Untyped<Value>>,
    #[php(sensitive)] passphrase: PhpString,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        check_int_len(req, pkcs12.as_bytes().len(), "pkcs12", 1)?;
        let parsed = Bio::reading(pkcs12.as_bytes())
            .filter(|_| !pkcs12.is_empty())
            .and_then(|bio| Pkcs12::read_der(&bio))
            .and_then(|p12| p12.parse(&c_string(passphrase.as_bytes())));
        let Some(parsed) = parsed else {
            store_errors(req);
            return Ok(false);
        };
        let mut out = PhpArray::new(req)?;
        if let Some(cert) = &parsed.cert {
            match cert.to_pem() {
                Some(pem) => put_bytes(req, &mut out, b"cert", &pem)?,
                None => store_errors(req),
            }
        }
        if let Some(key) = &parsed.key {
            match key.private_pem(None) {
                Some(pem) => put_bytes(req, &mut out, b"pkey", &pem)?,
                None => store_errors(req),
            }
        }
        if let Some(extra) = parsed.extra.as_ref().filter(|stack| stack.len() > 0) {
            let mut list = PhpArray::new(req)?;
            for (i, pem) in extra.pems().into_iter().rev().enumerate() {
                if let Some(pem) = pem {
                    let pem = Value::string(PhpString::new(req, &pem)?);
                    list.set(req, Key::Int(i64::try_from(i).unwrap_or(0)), pem)?;
                }
            }
            put(req, &mut out, b"extracerts", Value::array(list))?;
        }
        certificates.set(req, Untyped(Value::array(out)))?;
        Ok(true)
    })
}
