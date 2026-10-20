//! `OpenSSLCertificateSigningRequest` and the `openssl_csr_*` natives of
//! openssl.c: make a request from a distinguished name and a key
//! (`php_openssl_csr_make`), export it, read its subject and key, and
//! sign it into a certificate.

use std::ffi::c_long;
use std::ops::ControlFlow;

use php_ext::abi::{Instance, OrFalse, Path, Ref, Refusals, Union, Untyped};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::with_string;
use php_ext::raising::{Level, Thrown, docref, fatal};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value, ValueRef};

use crate::config::{Settings, parse};
use crate::decode::{
    Arg, CertParam, CsrParam, Origin, c_string, cert_from_param, check_path, csr_from_param,
    finish, raised,
};
use crate::ffi::x509::{Csr, Extended, add_extensions};
use crate::ffi::{self, Bio, Certificate, PKey};
use crate::key::{Half, OpenSSLAsymmetricKey, key_from, object_of as key_object};
use crate::store_errors;
use crate::x509::{OpenSSLCertificate, name_entries, object_of as cert_object};

/// A `php_openssl_request_object`: the request it holds.
#[derive(Default)]
pub(crate) struct Req {
    pub(crate) csr: Option<Csr>,
}

/// `final class OpenSSLCertificateSigningRequest`, `@strict-properties`,
/// `@not-serializable`, under [`HANDLERS`].
///
/// ```php
/// try { new OpenSSLCertificateSigningRequest; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $r = openssl_csr_new(['commonName' => 'a'], $k);
/// try { clone $r; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($r); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// var_dump($r);
/// ```
/// ```expect
/// Cannot directly construct OpenSSLCertificateSigningRequest, use openssl_csr_new() instead
/// Trying to clone an uncloneable object of class OpenSSLCertificateSigningRequest
/// Serialization of 'OpenSSLCertificateSigningRequest' is not allowed
/// object(OpenSSLCertificateSigningRequest)#1 (0) {
/// }
/// ```
#[php_class(
    final,
    not_serializable,
    size = 64, payload = Req,
    handlers = HANDLERS,
    dynamic_properties = forbidden
)]
pub(crate) struct OpenSSLCertificateSigningRequest;

#[php_class]
impl OpenSSLCertificateSigningRequest {}

/// `php_openssl_request_object_handlers`.
pub(crate) const HANDLERS: Refusals = crate::refusals("openssl_csr_new()");

/// A new `OpenSSLCertificateSigningRequest` holding the request.
fn object_of(
    req: &mut Request,
    csr: Csr,
) -> Result<Owner<Instance<OpenSSLCertificateSigningRequest>>, Thrown> {
    let Some(class) = lookup_class(req, b"OpenSSLCertificateSigningRequest")? else {
        return Err(fatal(
            req,
            "InternalError: OpenSSLCertificateSigningRequest is not declared",
        ));
    };
    let object = new_object(req, class)?;
    if let Some(payload) = object.native_mut::<Req>(req) {
        payload.csr = Some(csr);
    }
    Ok(object.into_instance())
}

/// The warning every native raises for a request it cannot read.
fn no_csr(req: &mut Request) {
    docref(
        req,
        Level::Warning,
        "X.509 Certificate Signing Request cannot be retrieved",
    );
}

/// `php_openssl_csr_add_subj_entry`: one entry of the subject, its text
/// read as C reads it.
fn add_subject_entry(req: &mut Request, csr: &Csr, nid: i32, item: &Value) -> Result<bool, Thrown> {
    let text = with_string(req, item, c_string)?;
    if csr.subject().add_by_nid(nid, &text) {
        return Ok(true);
    }
    store_errors(req);
    let message = format!(
        "dn: add_entry_by_NID {nid} -> {} (failed; check error queue and value of string_mask OpenSSL option if illegal characters are reported)",
        String::from_utf8_lossy(text.as_bytes())
    );
    docref(req, Level::Warning, &message);
    Ok(false)
}

/// The name of a `_default` line of the distinguished name section, its
/// suffix taken off and cut at 200 bytes, past a leading `N.`, `N:` or
/// `N,`; `None` for a line that is no default.
fn default_field(name: &[u8]) -> Option<&[u8]> {
    let stem = name.strip_suffix(b"_default").filter(|_| name.len() >= 9)?;
    let stem = &stem[..stem.len().min(200)];
    match stem.iter().position(|b| matches!(b, b':' | b',' | b'.')) {
        Some(at) if at + 1 < stem.len() => Some(&stem[at + 1..]),
        _ => Some(stem),
    }
}

/// The subject's entries from the `$distinguished_names` array: each
/// string key a name the library knows, its value or each value of its
/// list an entry; php's warning for a name it does not know.
fn subject_from_dn(req: &mut Request, csr: &Csr, dn: PhpArray) -> Result<bool, Thrown> {
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(Z_ARRVAL_P(dn), strindex, item)` and,
    // for a list, `ZEND_HASH_FOREACH_NUM_KEY_VAL(Z_ARRVAL_P(item), i,
    // subitem)` (`Z/ext/openssl/openssl_backend_common.c:983`, `:988`), no
    // owner of either table taken: the argument holds them while
    // `zval_try_get_string` runs `__toString`.
    let walked = walk::foreach(req, dn, Hold::None, |req, row| -> walk::Step {
        let Key::Str(name) = row.key() else {
            return Ok(ControlFlow::Continue(()));
        };
        let nid = ffi::text_nid(&c_string(name.as_bytes()));
        if nid == 0 {
            let text = format!(
                "dn: {} is not a recognized name",
                String::from_utf8_lossy(name.as_bytes())
            );
            docref(req, Level::Warning, &text);
            return Ok(ControlFlow::Continue(()));
        }
        let item = row.value().deref();
        let Some(list) = item.as_array() else {
            let added = add_subject_entry(req, csr, nid, &item)?;
            return Ok(if added {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(())
            });
        };
        let refused = walk::foreach(req, list, Hold::None, |req, row| -> walk::Step {
            let added = add_subject_entry(req, csr, nid, &row.value().deref())?;
            Ok(if added {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(())
            })
        })?;
        Ok(if refused.is_some() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    })?;
    Ok(walked.is_none())
}

/// The distinguished name section's `_default` lines, each added where
/// the subject has no entry of its name yet.
fn subject_defaults(req: &mut Request, csr: &Csr, lines: &[(Vec<u8>, Vec<u8>)]) -> bool {
    for (name, value) in lines {
        let Some(field) = default_field(name) else {
            continue;
        };
        let field = c_string(field);
        let subject = csr.subject();
        if subject.has(ffi::text_nid(&field)) {
            continue;
        }
        if !subject.add_by_text(&field, &c_string(value)) {
            store_errors(req);
            let text = format!(
                "add_entry_by_txt {} -> {} (failed)",
                String::from_utf8_lossy(field.as_bytes()),
                String::from_utf8_lossy(value)
            );
            docref(req, Level::Warning, &text);
            return false;
        }
        if subject.len() == 0 {
            docref(req, Level::Warning, "No objects specified in config file");
            return false;
        }
    }
    true
}

/// The `$extra_attributes`, each under a name the library knows, then
/// the configuration's attributes section's lines the request lacks.
fn attributes(
    req: &mut Request,
    csr: &mut Csr,
    attribs: PhpArray,
    lines: &[(Vec<u8>, Vec<u8>)],
) -> Result<bool, Thrown> {
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(Z_ARRVAL_P(attribs), strindex, item)`
    // (`Z/ext/openssl/openssl_backend_common.c:1052`), no owner of the
    // table taken: the argument holds it while `__toString` runs.
    let walked = walk::foreach(req, attribs, Hold::None, |req, row| -> walk::Step<bool> {
        let key = row.key().clone();
        let item = row.value().deref();
        let Key::Str(name) = key else {
            docref(
                req,
                Level::Warning,
                "attributes: numeric fild names are not supported",
            );
            return Ok(ControlFlow::Continue(()));
        };
        let nid = ffi::text_nid(&c_string(name.as_bytes()));
        if nid == 0 {
            let text = format!(
                "attributes: {} is not a recognized attribute name",
                String::from_utf8_lossy(name.as_bytes())
            );
            docref(req, Level::Warning, &text);
            return Ok(ControlFlow::Continue(()));
        }
        let text = with_string(req, &item, <[u8]>::to_vec)?;
        if !csr.add_attribute(nid, &text) {
            store_errors(req);
            let message = format!(
                "attributes: add_attr_by_NID {nid} -> {} (failed)",
                String::from_utf8_lossy(c_string(&text).as_bytes())
            );
            docref(req, Level::Warning, &message);
            return Ok(ControlFlow::Break(false));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    if let Some(answer) = walked {
        return Ok(answer);
    }
    for (name, value) in lines {
        let name = c_string(name);
        if csr.has_attribute(ffi::text_nid(&name)) {
            continue;
        }
        if !csr.add_attribute_by_text(&name, &c_string(value)) {
            store_errors(req);
            let text = format!(
                "add1_attr_by_txt {} -> {} (failed; check error queue and value of string_mask OpenSSL option if illegal characters are reported)",
                String::from_utf8_lossy(name.as_bytes()),
                String::from_utf8_lossy(value)
            );
            docref(req, Level::Warning, &text);
            return Ok(false);
        }
    }
    Ok(true)
}

/// `php_openssl_csr_make`: the request's version, subject and attributes
/// from the arrays and the configuration's defaults, and its public key;
/// false after php's warning or with the library's reasons stored.
fn csr_make(
    req: &mut Request,
    settings: &Settings,
    csr: &mut Csr,
    key: &PKey,
    dn: PhpArray,
    attribs: Option<PhpArray>,
) -> Result<bool, Thrown> {
    let Some(dn_section) = settings
        .conf
        .string_loud(Some(&settings.section), c"distinguished_name")
    else {
        store_errors(req);
        return Ok(false);
    };
    let Some(dn_lines) = settings.conf.section(&c_string(&dn_section)) else {
        store_errors(req);
        return Ok(false);
    };
    let attr_name = settings.conf.string(Some(&settings.section), c"attributes");
    let attr_lines = match attr_name.map(|name| settings.conf.section(&c_string(&name))) {
        Some(Some(lines)) => lines,
        Some(None) => {
            store_errors(req);
            return Ok(false);
        }
        None => Vec::new(),
    };
    if !csr.set_version_1() {
        store_errors(req);
        return Ok(false);
    }
    if !subject_from_dn(req, csr, dn)? || !subject_defaults(req, csr, &dn_lines) {
        return Ok(false);
    }
    if let Some(attribs) = attribs
        && !attributes(req, csr, attribs, &attr_lines)?
    {
        return Ok(false);
    }
    if !csr.set_public_key(key) {
        store_errors(req);
    }
    Ok(true)
}

/// `openssl_csr_new(array $distinguished_names, &$private_key, ?array`
/// `$options = null, ?array $extra_attributes = null):`
/// `OpenSSLCertificateSigningRequest|bool`: a request for the name,
/// signed with the key `$private_key` holds or a new one it is given.
///
/// ```php
/// $r = openssl_csr_new(['countryName' => 'NL', 'commonName' => 'a'], $k, ['digest_alg' => 'sha256']);
/// var_dump($r instanceof OpenSSLCertificateSigningRequest, $k instanceof OpenSSLAsymmetricKey, openssl_csr_get_subject($r));
/// $k2 = openssl_pkey_new(['private_key_bits' => 1024]);
/// var_dump(openssl_csr_new(['commonName' => 'b', 'nope' => 'x'], $k2) instanceof OpenSSLCertificateSigningRequest);
/// var_dump(openssl_csr_new(['commonName' => 'b'], $k2, ['private_key_bits' => 128]));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// array(4) {
///   ["C"]=>
///   string(2) "NL"
///   ["CN"]=>
///   string(1) "a"
///   ["ST"]=>
///   string(10) "Some-State"
///   ["O"]=>
///   string(24) "Internet Widgits Pty Ltd"
/// }
///
/// Warning: openssl_csr_new(): dn: nope is not a recognized name in %s on line 5
/// bool(true)
/// object(OpenSSLCertificateSigningRequest)#4 (0) {
/// }
/// ```
#[php_function]
fn openssl_csr_new(
    req: &mut Request,
    distinguished_names: PhpArray,
    #[php(sensitive)] private_key: Ref<Untyped<Value>>,
    #[php(default = null)] options: Option<PhpArray>,
    #[php(default = null)] extra_attributes: Option<PhpArray>,
) -> Result<Union<Owner<Instance<OpenSSLCertificateSigningRequest>>, bool>, Thrown> {
    finish(req, move |req| {
        let Some(settings) = parse(req, options) else {
            return Ok(Union::B(false));
        };
        let given = private_key
            .get()
            .map(|v| v.0.deref())
            .filter(|v| !matches!(v.as_ref(), ValueRef::Null));
        let mut key = match given {
            Some(value) => key_from(req, &value, Half::Private, None, Arg::new(2, "private_key"))?,
            None => None,
        };
        let made = key.is_none();
        if made {
            key = settings.generate(req);
        }
        let Some(key) = key else {
            docref(req, Level::Warning, "Unable to generate a private key");
            return Ok(Union::B(false));
        };
        let Some(mut csr) = Csr::new() else {
            store_errors(req);
            return Ok(Union::B(false));
        };
        if !csr_make(
            req,
            &settings,
            &mut csr,
            &key,
            distinguished_names,
            extra_attributes,
        )? {
            return Ok(Union::B(false));
        }
        if let Some(section) = &settings.request_extensions
            && !add_extensions(&settings.conf, section, &Extended::Request(&csr))
        {
            store_errors(req);
            let text = format!(
                "Error loading extension section {}",
                String::from_utf8_lossy(section.as_bytes())
            );
            docref(req, Level::Warning, &text);
            return Ok(Union::B(false));
        }
        let signed = settings
            .digest
            .as_ref()
            .is_some_and(|md| csr.sign(&key, md));
        let out = if signed {
            Union::A(object_of(req, csr)?)
        } else {
            store_errors(req);
            docref(req, Level::Warning, "Error signing request");
            Union::B(true)
        };
        if made {
            let written =
                key_object(req, key, true).and_then(|object| private_key.set(req, object));
            if let Err(thrown) = written {
                if let Union::A(object) = out {
                    object.give_up(req);
                }
                return Err(thrown);
            }
        }
        Ok(out)
    })
}

/// The request as PEM, `X509_REQ_print`'s text first unless `no_text`.
fn export_into(req: &mut Request, csr: &Csr, out: &Bio<'_>, no_text: bool) -> Option<bool> {
    if !no_text && !csr.print(out) {
        store_errors(req);
        return None;
    }
    let written = csr.write_pem(out);
    Some(written)
}

/// `openssl_csr_export_to_file(OpenSSLCertificateSigningRequest|string`
/// `$csr, string $output_filename, bool $no_text = true): bool`.
///
/// ```php
/// $r = openssl_csr_new(['commonName' => 'a'], $k);
/// $f = __DIR__ . '/r.pem';
/// var_dump(openssl_csr_export_to_file($r, $f), substr(file_get_contents($f), 0, 35));
/// unlink($f);
/// var_dump(openssl_csr_export_to_file('nope', $f));
/// ```
/// ```expect
/// bool(true)
/// string(35) "-----BEGIN CERTIFICATE REQUEST-----"
///
/// Warning: openssl_csr_export_to_file(): X.509 Certificate Signing Request cannot be retrieved in %s on line 6
/// bool(false)
/// ```
#[php_function]
fn openssl_csr_export_to_file(
    req: &mut Request,
    csr: CsrParam,
    output_filename: Path,
    #[php(default = true)] no_text: bool,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let Some(csr) = csr_from_param(req, &csr, Arg::new(1, "csr"))? else {
            no_csr(req);
            return Ok(false);
        };
        let origin = Origin::Arg(Arg::new(2, "output_filename"));
        let Some(path) = check_path(req, output_filename.0.as_bytes(), origin, false) else {
            return Ok(false);
        };
        let shown = String::from_utf8_lossy(path.as_bytes()).into_owned();
        let Some(out) = Bio::file(&path, c"w") else {
            store_errors(req);
            docref(req, Level::Warning, &format!("Error opening file {shown}"));
            return Ok(false);
        };
        match export_into(req, &csr, &out, no_text) {
            Some(true) => Ok(true),
            Some(false) => {
                docref(
                    req,
                    Level::Warning,
                    &format!("Error writing PEM to file {shown}"),
                );
                store_errors(req);
                Ok(false)
            }
            None => Ok(false),
        }
    })
}

/// `openssl_csr_export(OpenSSLCertificateSigningRequest|string $csr,`
/// `&$output, bool $no_text = true): bool`.
///
/// ```php
/// $r = openssl_csr_new(['commonName' => 'a'], $k);
/// var_dump(openssl_csr_export($r, $out), substr($out, 0, 35), openssl_csr_export($r, $text, false), strpos($text, 'Certificate Request:') === 0);
/// var_dump(openssl_csr_export('nope', $out));
/// ```
/// ```expect
/// bool(true)
/// string(35) "-----BEGIN CERTIFICATE REQUEST-----"
/// bool(true)
/// bool(true)
///
/// Warning: openssl_csr_export(): X.509 Certificate Signing Request cannot be retrieved in %s on line 4
/// bool(false)
/// ```
#[php_function]
fn openssl_csr_export(
    req: &mut Request,
    csr: CsrParam,
    output: Ref<Untyped<Value>>,
    #[php(default = true)] no_text: bool,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let Some(csr) = csr_from_param(req, &csr, Arg::new(1, "csr"))? else {
            no_csr(req);
            return Ok(false);
        };
        let Some(out) = Bio::memory() else {
            store_errors(req);
            return Ok(false);
        };
        match export_into(req, &csr, &out, no_text) {
            Some(true) => {
                let pem = PhpString::new(req, &out.contents())?;
                output.set(req, Untyped(pem.into_value()))?;
                Ok(true)
            }
            Some(false) => {
                store_errors(req);
                Ok(false)
            }
            None => Ok(false),
        }
    })
}

/// The request's public key, where its signature is that key's; `None`
/// after php's warning otherwise.
fn signed_key(req: &mut Request, csr: &Csr) -> Option<PKey> {
    let Some(public) = csr.public_key() else {
        store_errors(req);
        docref(req, Level::Warning, "Error unpacking public key");
        return None;
    };
    match csr.verify(&public) {
        verdict if verdict < 0 => {
            store_errors(req);
            docref(req, Level::Warning, "Signature verification problems");
            None
        }
        0 => {
            let text = "Signature did not match the certificate request";
            docref(req, Level::Warning, text);
            None
        }
        _ => Some(public),
    }
}

/// The new certificate's serial number, from its hex where given (200
/// digits at most) and from the integer otherwise; false after php's
/// warning.
fn set_serial(
    req: &mut Request,
    cert: &mut Certificate,
    serial: i64,
    hex: Option<PhpString>,
) -> bool {
    let failure = match hex {
        Some(hex) if hex.as_bytes().len() > 200 => {
            docref(
                req,
                Level::Warning,
                "Error parsing serial number because it is too long",
            );
            return false;
        }
        Some(hex) => match cert.set_serial_hex(hex.as_bytes()) {
            Some(true) => return true,
            Some(false) => "Error parsing serial number",
            None => "Error parsing serial number because memory allocation failed",
        },
        None if cert.set_serial(serial) => return true,
        None => "Error setting serial number",
    };
    store_errors(req);
    docref(req, Level::Warning, failure);
    false
}

/// `openssl_csr_sign(OpenSSLCertificateSigningRequest|string $csr,`
/// `OpenSSLCertificate|string|null $ca_certificate, $private_key, int`
/// `$days, ?array $options = null, int $serial = 0, ?string $serial_hex =`
/// `null): OpenSSLCertificate|false`: a version 3 certificate for the
/// request, issued by the CA certificate or by the request itself, valid
/// from now for `$days`, signed with the key.
///
/// ```php
/// $r = openssl_csr_new(['commonName' => 'a'], $k);
/// $x = openssl_csr_sign($r, null, $k, 30, ['digest_alg' => 'sha256'], 0, 'DEADBEEF');
/// $p = openssl_x509_parse($x);
/// var_dump($p['serialNumberHex'], $p['issuer'], intdiv($p['validTo_time_t'] - $p['validFrom_time_t'], 86400));
/// var_dump(openssl_csr_sign($r, null, $k, -1));
/// var_dump(openssl_csr_sign($r, null, openssl_pkey_new(['private_key_bits' => 1024]), 1) instanceof OpenSSLCertificate);
/// var_dump(openssl_csr_sign($r, $x, openssl_pkey_new(['private_key_bits' => 1024]), 1));
/// ```
/// ```expect
/// string(8) "DEADBEEF"
/// array(4) {
///   ["CN"]=>
///   string(1) "a"
///   ["C"]=>
///   string(2) "AU"
///   ["ST"]=>
///   string(10) "Some-State"
///   ["O"]=>
///   string(24) "Internet Widgits Pty Ltd"
/// }
/// int(30)
///
/// Warning: openssl_csr_sign(): Days must be between 0 and 106751991167300 in %s on line 6
/// bool(false)
/// bool(true)
///
/// Warning: openssl_csr_sign(): Private key does not correspond to signing cert in %s on line 8
/// bool(false)
/// ```
#[php_function]
fn openssl_csr_sign(
    req: &mut Request,
    csr: CsrParam,
    ca_certificate: Option<CertParam>,
    #[php(sensitive)] private_key: Untyped<Value>,
    days: i64,
    #[php(default = null)] options: Option<PhpArray>,
    #[php(default = 0)] serial: i64,
    #[php(default = null)] serial_hex: Option<PhpString>,
) -> Result<OrFalse<Owner<Instance<OpenSSLCertificate>>>, Thrown> {
    finish(req, move |req| {
        let Some(csr) = csr_from_param(req, &csr, Arg::new(1, "csr"))? else {
            no_csr(req);
            return Ok(OrFalse::False);
        };
        let mut issuer = None;
        if let Some(param) = &ca_certificate {
            issuer = cert_from_param(req, param, Arg::new(2, "ca_certificate"))?;
            if issuer.is_none() {
                docref(req, Level::Warning, "X.509 Certificate cannot be retrieved");
                return Ok(OrFalse::False);
            }
        }
        let arg = Arg::new(3, "private_key");
        let Some(key) = key_from(req, &private_key.0, Half::Private, Some(b""), arg)? else {
            if !raised(req) {
                docref(
                    req,
                    Level::Warning,
                    "Cannot get private key from parameter 3",
                );
            }
            return Ok(OrFalse::False);
        };
        if issuer
            .as_ref()
            .is_some_and(|issuer| !issuer.matches_key(&key))
        {
            store_errors(req);
            docref(
                req,
                Level::Warning,
                "Private key does not correspond to signing cert",
            );
            return Ok(OrFalse::False);
        }
        let most = c_long::MAX / 86_400;
        if !(0..=most).contains(&days) {
            docref(
                req,
                Level::Warning,
                &format!("Days must be between 0 and {most}"),
            );
            return Ok(OrFalse::False);
        }
        let Some(settings) = parse(req, options) else {
            return Ok(OrFalse::False);
        };
        let Some(public) = signed_key(req, &csr) else {
            return Ok(OrFalse::False);
        };
        let Some(mut cert) = Certificate::new() else {
            store_errors(req);
            docref(req, Level::Warning, "No memory");
            return Ok(OrFalse::False);
        };
        if !cert.set_version(2) || !set_serial(req, &mut cert, serial, serial_hex) {
            return Ok(OrFalse::False);
        }
        let built = cert.set_subject(&csr.subject())
            && {
                let issuer_name = match &issuer {
                    Some(issuer) => issuer.subject(),
                    None => csr.subject(),
                };
                cert.set_issuer(&issuer_name)
            }
            && cert.set_validity(c_long::try_from(days * 86_400).unwrap_or(c_long::MAX))
            && cert.set_public_key(&public);
        if !built {
            store_errors(req);
            return Ok(OrFalse::False);
        }
        if let Some(section) = &settings.extensions {
            let to = Extended::Certificate {
                issuer: issuer.as_ref(),
                cert: &cert,
                csr: &csr,
            };
            if !add_extensions(&settings.conf, section, &to) {
                store_errors(req);
                return Ok(OrFalse::False);
            }
        }
        if !settings
            .digest
            .as_ref()
            .is_some_and(|md| cert.sign(&key, md))
        {
            store_errors(req);
            docref(req, Level::Warning, "Failed to sign it");
            return Ok(OrFalse::False);
        }
        Ok(OrFalse::Value(cert_object(req, cert)?))
    })
}

/// `openssl_csr_get_subject(OpenSSLCertificateSigningRequest|string`
/// `$csr, bool $short_names = true): array|false`.
///
/// ```php
/// $r = openssl_csr_new(['commonName' => 'a', 'organizationName' => 'b'], $k);
/// var_dump(openssl_csr_get_subject($r), openssl_csr_get_subject($r, false), openssl_csr_get_subject('nope'));
/// ```
/// ```expect
/// array(4) {
///   ["CN"]=>
///   string(1) "a"
///   ["O"]=>
///   string(1) "b"
///   ["C"]=>
///   string(2) "AU"
///   ["ST"]=>
///   string(10) "Some-State"
/// }
/// array(4) {
///   ["commonName"]=>
///   string(1) "a"
///   ["organizationName"]=>
///   string(1) "b"
///   ["countryName"]=>
///   string(2) "AU"
///   ["stateOrProvinceName"]=>
///   string(10) "Some-State"
/// }
/// bool(false)
/// ```
#[php_function]
fn openssl_csr_get_subject(
    req: &mut Request,
    csr: CsrParam,
    #[php(default = true)] short_names: bool,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    finish(req, move |req| {
        let Some(csr) = csr_from_param(req, &csr, Arg::new(1, "csr"))? else {
            return Ok(OrFalse::False);
        };
        let mut out = PhpArray::new(req)?;
        name_entries(req, &mut out, None, &csr.subject(), short_names)?;
        Ok(OrFalse::Value(out))
    })
}

/// `openssl_csr_get_public_key(OpenSSLCertificateSigningRequest|string`
/// `$csr, bool $short_names = true): OpenSSLAsymmetricKey|false`: the
/// request's key, its public half alone (`php_openssl_extract_public_key`).
///
/// ```php
/// $r = openssl_csr_new(['commonName' => 'a'], $k);
/// $p = openssl_csr_get_public_key($r);
/// var_dump($p instanceof OpenSSLAsymmetricKey, openssl_pkey_get_details($p)['key'] === openssl_pkey_get_details($k)['key'], isset(openssl_pkey_get_details($p)['rsa']['d']));
/// var_dump(openssl_csr_get_public_key('nope'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_csr_get_public_key(
    req: &mut Request,
    csr: CsrParam,
    #[php(default = true)] short_names: bool,
) -> Result<OrFalse<Owner<Instance<OpenSSLAsymmetricKey>>>, Thrown> {
    finish(req, move |req| {
        let _ = short_names;
        let Some(csr) = csr_from_param(req, &csr, Arg::new(1, "csr"))? else {
            return Ok(OrFalse::False);
        };
        let public = csr
            .public_key()
            .and_then(|key| key.public_pem())
            .and_then(|pem| Bio::reading(&pem).and_then(|bio| PKey::read_public(&bio)));
        let Some(public) = public else {
            store_errors(req);
            return Ok(OrFalse::False);
        };
        Ok(OrFalse::Value(key_object(req, public, false)?))
    })
}
