//! `OpenSSLCertificate` and the `openssl_x509_*` natives of openssl.c:
//! read, export, fingerprint, match against a key, verify, parse, and
//! check for a purpose against a store (`php_openssl_setup_verify`,
//! `php_openssl_check_cert`), with `openssl_get_cert_locations`.

use std::ffi::{CStr, OsStr};
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;

use php_ext::abi::{Instance, OrFalse, Ref, Refusals, Union, Untyped};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::{atoi, with_string};
use php_ext::raising::{Level, Thrown, docref, fatal};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef};

use crate::decode::{
    Arg, CertParam, Origin, c_string, cert_from_param, certs_from_file, check_path, finish,
};
use crate::ffi::x509::{Asn1String, Checked, Name, Store};
use crate::ffi::{self, Bio, Certificate, Digest};
use crate::key::{Half, key_from};
use crate::{hex, put, put_bytes, store_errors};

/// A `php_openssl_certificate_object`: the certificate it holds.
#[derive(Default)]
pub(crate) struct Cert {
    pub(crate) cert: Option<Certificate>,
}

/// `final class OpenSSLCertificate`, `@strict-properties`,
/// `@not-serializable`, under [`HANDLERS`].
///
/// ```php
/// try { new OpenSSLCertificate; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// try { clone $x; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $x->y = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($x == openssl_x509_read($x), $x);
/// ```
/// ```expect
/// Cannot directly construct OpenSSLCertificate, use openssl_x509_read() instead
/// Trying to clone an uncloneable object of class OpenSSLCertificate
/// Cannot create dynamic property OpenSSLCertificate::$y
/// bool(false)
/// object(OpenSSLCertificate)#4 (0) {
/// }
/// ```
///
/// `php_openssl_certificate_create_object` allocates a
/// `php_openssl_certificate_object`, which `memory_get_usage()` counts
/// (design §5.13):
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024, 'private_key_type' => OPENSSL_KEYTYPE_RSA]);
/// openssl_x509_export(openssl_csr_sign(openssl_csr_new(['commonName' => 'x'], $k), null, $k, 1), $pem);
/// openssl_x509_read($pem);
/// $m = memory_get_usage();
/// $c = openssl_x509_read($pem);
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 48
/// ```
#[php_class(
    final,
    not_serializable,
    size = 64, payload = Cert,
    handlers = HANDLERS,
    dynamic_properties = forbidden
)]
pub(crate) struct OpenSSLCertificate;

#[php_class]
impl OpenSSLCertificate {}

/// `php_openssl_certificate_object_handlers`.
pub(crate) const HANDLERS: Refusals = crate::refusals("openssl_x509_read()");

/// `object_init_ex(return_value, php_openssl_certificate_ce)` holding
/// the certificate.
pub(crate) fn object_of(
    req: &mut Request,
    cert: Certificate,
) -> Result<Owner<Instance<OpenSSLCertificate>>, Thrown> {
    let Some(class) = lookup_class(req, b"OpenSSLCertificate")? else {
        return Err(fatal(
            req,
            "InternalError: OpenSSLCertificate is not declared",
        ));
    };
    let object = new_object(req, class)?;
    if let Some(payload) = object.native_mut::<Cert>(req) {
        payload.cert = Some(cert);
    }
    Ok(object.into_instance())
}

/// The warning every native raises for a certificate it cannot read.
fn no_cert(req: &mut Request) {
    docref(req, Level::Warning, "X.509 Certificate cannot be retrieved");
}

/// `openssl_x509_read(OpenSSLCertificate|string $certificate):`
/// `OpenSSLCertificate|false`: a new object holding the certificate.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// openssl_x509_export($x, $pem);
/// var_dump(openssl_x509_read($pem) instanceof OpenSSLCertificate, openssl_x509_read($x) !== $x);
/// var_dump(openssl_x509_read('nope'));
/// try { openssl_x509_read([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
///
/// Warning: openssl_x509_read(): X.509 Certificate cannot be retrieved in %s on line 6
/// bool(false)
/// openssl_x509_read(): Argument #1 ($certificate) must be of type OpenSSLCertificate|string, array given
/// ```
#[php_function]
fn openssl_x509_read(
    req: &mut Request,
    certificate: CertParam,
) -> Result<OrFalse<Owner<Instance<OpenSSLCertificate>>>, Thrown> {
    finish(req, move |req| {
        let Some(cert) = cert_from_param(req, &certificate, Arg::new(1, "certificate"))? else {
            no_cert(req);
            return Ok(OrFalse::False);
        };
        Ok(OrFalse::Value(object_of(req, cert)?))
    })
}

/// `openssl_x509_free(OpenSSLCertificate $certificate): void`: nothing,
/// the object freeing its certificate itself.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// openssl_x509_free(openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1));
/// ```
/// ```expect
/// Deprecated: Function openssl_x509_free() is deprecated since 8.0, as OpenSSLCertificate objects are freed automatically in %s on line 3
/// ```
#[php_function(deprecated(
    since = "8.0",
    message = "as OpenSSLCertificate objects are freed automatically"
))]
fn openssl_x509_free(req: &mut Request, certificate: Instance<OpenSSLCertificate>) {
    let _ = (req, certificate);
}

/// The certificate as PEM, `X509_print`'s text first unless `no_text`,
/// into `out`; false with the library's reasons stored.
fn export_into(req: &mut Request, cert: &Certificate, out: &Bio<'_>, no_text: bool) -> bool {
    if (no_text || cert.print(out)) && cert.write_pem(out) {
        return true;
    }
    store_errors(req);
    false
}

/// `openssl_x509_export_to_file(OpenSSLCertificate|string $certificate,`
/// `string $output_filename, bool $no_text = true): bool`.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $f = __DIR__ . '/x.pem';
/// var_dump(openssl_x509_export_to_file($x, $f), substr(file_get_contents($f), 0, 27));
/// var_dump(openssl_x509_export_to_file($x, $f, false), strpos(file_get_contents($f), 'Certificate:') === 0);
/// unlink($f);
/// var_dump(openssl_x509_export_to_file('nope', $f));
/// ```
/// ```expect
/// bool(true)
/// string(27) "-----BEGIN CERTIFICATE-----"
/// bool(true)
/// bool(true)
///
/// Warning: openssl_x509_export_to_file(): X.509 Certificate cannot be retrieved in %s on line 8
/// bool(false)
/// ```
#[php_function]
fn openssl_x509_export_to_file(
    req: &mut Request,
    certificate: CertParam,
    output_filename: php_ext::abi::Path,
    #[php(default = true)] no_text: bool,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let Some(cert) = cert_from_param(req, &certificate, Arg::new(1, "certificate"))? else {
            no_cert(req);
            return Ok(false);
        };
        let origin = Origin::Arg(Arg::new(2, "output_filename"));
        let Some(path) = check_path(req, output_filename.0.as_bytes(), origin, false) else {
            return Ok(false);
        };
        let Some(out) = Bio::file(&path, c"w") else {
            store_errors(req);
            let text = format!(
                "Error opening file {}",
                String::from_utf8_lossy(path.as_bytes())
            );
            docref(req, Level::Warning, &text);
            return Ok(false);
        };
        Ok(export_into(req, &cert, &out, no_text))
    })
}

/// `openssl_x509_export(OpenSSLCertificate|string $certificate, &$output,`
/// `bool $no_text = true): bool`.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// var_dump(openssl_x509_export($x, $out), substr($out, 0, 27), openssl_x509_export($x, $text, false), strlen($text) > strlen($out));
/// var_dump(openssl_x509_export('nope', $out));
/// ```
/// ```expect
/// bool(true)
/// string(27) "-----BEGIN CERTIFICATE-----"
/// bool(true)
/// bool(true)
///
/// Warning: openssl_x509_export(): X.509 Certificate cannot be retrieved in %s on line 5
/// bool(false)
/// ```
#[php_function]
fn openssl_x509_export(
    req: &mut Request,
    certificate: CertParam,
    output: Ref<Untyped<Value>>,
    #[php(default = true)] no_text: bool,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let Some(cert) = cert_from_param(req, &certificate, Arg::new(1, "certificate"))? else {
            no_cert(req);
            return Ok(false);
        };
        let Some(out) = Bio::memory() else {
            store_errors(req);
            return Ok(false);
        };
        if !export_into(req, &cert, &out, no_text) {
            return Ok(false);
        }
        let pem = PhpString::new(req, &out.contents())?;
        output.set(req, Untyped(pem.into_value()))?;
        Ok(true)
    })
}

/// `openssl_x509_fingerprint(OpenSSLCertificate|string $certificate,`
/// `string $digest_algo = "sha1", bool $binary = false): string|false`:
/// `php_openssl_x509_fingerprint`, the digest of the DER form in hex or
/// raw.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// var_dump(strlen(openssl_x509_fingerprint($x)), strlen(openssl_x509_fingerprint($x, 'sha256', true)));
/// var_dump(openssl_x509_fingerprint($x, 'nope'), openssl_x509_fingerprint('nope'));
/// ```
/// ```expect
/// int(40)
/// int(32)
///
/// Warning: openssl_x509_fingerprint(): Unknown digest algorithm in %s on line 5
///
/// Warning: openssl_x509_fingerprint(): X.509 Certificate cannot be retrieved in %s on line 5
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_x509_fingerprint(
    req: &mut Request,
    certificate: CertParam,
    #[php(default = "sha1")] digest_algo: PhpString,
    #[php(default = false)] binary: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    finish(req, move |req| {
        let Some(cert) = cert_from_param(req, &certificate, Arg::new(1, "certificate"))? else {
            no_cert(req);
            return Ok(OrFalse::False);
        };
        let Some(md) = Digest::by_name(&c_string(digest_algo.as_bytes())) else {
            docref(req, Level::Warning, "Unknown digest algorithm");
            return Ok(OrFalse::False);
        };
        let Some(digest) = cert.digest(&md) else {
            store_errors(req);
            let text = format!(
                "{}(): Could not generate signature",
                req.frames().active_function_name()
            );
            return Err(fatal(req, &text));
        };
        let out = if binary { digest } else { hex(&digest) };
        Ok(OrFalse::Value(PhpString::new(req, &out)?))
    })
}

/// `openssl_x509_check_private_key(OpenSSLCertificate|string`
/// `$certificate, $private_key): bool`: whether the key is the private
/// half of the certificate's.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// var_dump(openssl_x509_check_private_key($x, $k), openssl_x509_check_private_key($x, openssl_pkey_new(['private_key_bits' => 1024])), openssl_x509_check_private_key('nope', $k));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_x509_check_private_key(
    req: &mut Request,
    certificate: CertParam,
    #[php(sensitive)] private_key: Untyped<Value>,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let Some(cert) = cert_from_param(req, &certificate, Arg::new(1, "certificate"))? else {
            return Ok(false);
        };
        let arg = Arg::new(2, "private_key");
        let Some(key) = key_from(req, &private_key.0, Half::Private, Some(b""), arg)? else {
            return Ok(false);
        };
        Ok(cert.matches_key(&key))
    })
}

/// `openssl_x509_verify(OpenSSLCertificate|string $certificate,`
/// `$public_key): int`: 1 where the key signed the certificate, 0 where
/// it did not, -1 for an error.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $pub = openssl_pkey_get_details($k)['key'];
/// $other = openssl_pkey_get_details(openssl_pkey_new(['private_key_bits' => 1024]))['key'];
/// var_dump(openssl_x509_verify($x, $pub), openssl_x509_verify($x, $other), openssl_x509_verify('nope', $pub), openssl_x509_verify($x, 'nope'));
/// ```
/// ```expect
/// int(1)
/// int(0)
/// int(-1)
/// int(-1)
/// ```
#[php_function]
fn openssl_x509_verify(
    req: &mut Request,
    certificate: CertParam,
    public_key: Untyped<Value>,
) -> Result<i64, Thrown> {
    finish(req, move |req| {
        let Some(cert) = cert_from_param(req, &certificate, Arg::new(1, "certificate"))? else {
            return Ok(-1);
        };
        let arg = Arg::new(2, "public_key");
        let Some(key) = key_from(req, &public_key.0, Half::Public, None, arg)? else {
            return Ok(-1);
        };
        let verdict = cert.verify(&key);
        if verdict < 0 {
            store_errors(req);
        }
        Ok(i64::from(verdict))
    })
}

/// `php_openssl_add_assoc_name_entry`: a name's entries under their
/// short or long names, a name met twice becoming a list, into `array`
/// itself or into a new array under `key`.
pub(crate) fn name_entries(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    key: Option<&[u8]>,
    name: &Name<'_>,
    short: bool,
) -> Result<(), Thrown> {
    let mut fresh = match key {
        Some(_) => Some(PhpArray::new(req)?),
        None => None,
    };
    let inner = fresh.as_mut().unwrap_or(array);
    for entry in name.entries() {
        let label = if short {
            ffi::nid_short_name(entry.nid)
        } else {
            ffi::nid_long_name(entry.nid)
        }
        .unwrap_or_default();
        let Some(text) = entry.text else {
            store_errors(req);
            continue;
        };
        let text = PhpString::new(req, &text)?.into_value();
        let found = inner.find(&label);
        match found.as_ref().map(Value::as_ref) {
            Some(ValueRef::Array(list)) => {
                let mut list = list;
                list.append(req, text)?;
            }
            Some(ValueRef::Str(_)) => {
                let Some(first) = found else {
                    text.give_up(req);
                    continue;
                };
                // `zend_string_copy`: the list holds the string as well,
                // before the entry that held it alone is replaced.
                let first = Owner::take(req, first);
                let mut list = PhpArray::new(req)?;
                list.append(req, first)?;
                list.append(req, text)?;
                inner.set_str(req, &label, list.into_value())?;
            }
            Some(_) => text.give_up(req),
            None => {
                inner.set_symtable_str(req, &label, text)?;
            }
        }
    }
    if let (Some(key), Some(fresh)) = (key, fresh) {
        array.set_str(req, key, fresh.into_value())?;
    }
    Ok(())
}

/// `timegm`: seconds since the epoch of a broken-down UTC time, its
/// fields normalised as `mktime` normalises them.
fn timegm(year: i64, mon: i64, mday: i64, hour: i64, min: i64, sec: i64) -> i64 {
    let year = year + mon.div_euclid(12);
    let mon = mon.rem_euclid(12) + 1;
    // Days from the civil date, Howard Hinnant's `days_from_civil`.
    let y = if mon <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mon + 9) % 12;
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468 + mday - 1;
    days * 86_400 + hour * 3_600 + min * 60 + sec
}

/// `php_openssl_asn1_time_to_time_t`: a `UTCTime` or `GeneralizedTime`
/// read backwards two digits at a time, as php reads it, and taken as
/// UTC; -1 after php's warning for one it cannot read.
fn time_t(req: &mut Request, time: &Asn1String) -> i64 {
    const UTC: i32 = 23;
    const GENERALIZED: i32 = 24;
    if time.kind != UTC && time.kind != GENERALIZED {
        docref(req, Level::Warning, "Illegal ASN1 data type for timestamp");
        return -1;
    }
    let len = time.data.len();
    let shown = String::from_utf8_lossy(c_string(&time.data).as_bytes()).into_owned();
    if time.data.contains(&0) {
        docref(req, Level::Warning, "Illegal length in timestamp");
        return -1;
    }
    if len < 13 || (time.kind == GENERALIZED && len < 15) {
        let text = format!("Unable to parse time string {shown} correctly");
        docref(req, Level::Warning, &text);
        return -1;
    }
    let mut buf = time.data.clone();
    let mut at = len - 3;
    let mut take = |step: usize, buf: &mut Vec<u8>| {
        let n = i64::from(atoi(&buf[at..]));
        buf[at] = 0;
        at = at.saturating_sub(step);
        n
    };
    let sec = take(2, &mut buf);
    let min = take(2, &mut buf);
    let hour = take(2, &mut buf);
    let mday = take(2, &mut buf);
    let mon = take(if time.kind == UTC { 2 } else { 4 }, &mut buf) - 1;
    let year = if time.kind == UTC {
        let year = i64::from(atoi(&buf[len - 13..]));
        if year < 68 { year + 2000 } else { year + 1900 }
    } else {
        i64::from(atoi(&buf[len - 15..]))
    };
    timegm(year, mon, mday, hour, min, sec)
}

/// `openssl_x509_parse(OpenSSLCertificate|string $certificate, bool`
/// `$short_names = true): array|false`: the certificate's fields, in
/// php's keys and order. The validity is pinned as whole days and as the
/// stamps read back from `validFrom` and `validTo`, never as the span in
/// seconds: `openssl_csr_sign` takes the two stamps one after the other,
/// so the span is a second longer when the clock ticks between them.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a', 'organizationName' => ['b', 'c']], $k), null, $k, 1, ['digest_alg' => 'sha256'], 7);
/// $p = openssl_x509_parse($x);
/// var_dump(array_keys($p), $p['name'], $p['subject'], $p['version'], $p['serialNumber'], $p['serialNumberHex'], $p['signatureTypeSN']);
/// var_dump(intdiv($p['validTo_time_t'] - $p['validFrom_time_t'], 86400), gmdate('ymdHis\Z', $p['validFrom_time_t']) === $p['validFrom'], gmdate('ymdHis\Z', $p['validTo_time_t']) === $p['validTo']);
/// var_dump($p['purposes'][1], array_keys($p['extensions']), openssl_x509_parse($x, false)['subject']['commonName'], openssl_x509_parse('nope'));
/// ```
/// ```expect
/// array(16) {
///   [0]=>
///   string(4) "name"
///   [1]=>
///   string(7) "subject"
///   [2]=>
///   string(4) "hash"
///   [3]=>
///   string(6) "issuer"
///   [4]=>
///   string(7) "version"
///   [5]=>
///   string(12) "serialNumber"
///   [6]=>
///   string(15) "serialNumberHex"
///   [7]=>
///   string(9) "validFrom"
///   [8]=>
///   string(7) "validTo"
///   [9]=>
///   string(16) "validFrom_time_t"
///   [10]=>
///   string(14) "validTo_time_t"
///   [11]=>
///   string(15) "signatureTypeSN"
///   [12]=>
///   string(15) "signatureTypeLN"
///   [13]=>
///   string(16) "signatureTypeNID"
///   [14]=>
///   string(8) "purposes"
///   [15]=>
///   string(10) "extensions"
/// }
/// string(32) "/CN=a/O=b/O=c/C=AU/ST=Some-State"
/// array(4) {
///   ["CN"]=>
///   string(1) "a"
///   ["O"]=>
///   array(2) {
///     [0]=>
///     string(1) "b"
///     [1]=>
///     string(1) "c"
///   }
///   ["C"]=>
///   string(2) "AU"
///   ["ST"]=>
///   string(10) "Some-State"
/// }
/// int(2)
/// string(1) "7"
/// string(2) "07"
/// string(10) "RSA-SHA256"
/// int(1)
/// bool(true)
/// bool(true)
/// array(3) {
///   [0]=>
///   bool(true)
///   [1]=>
///   bool(true)
///   [2]=>
///   string(9) "sslclient"
/// }
/// array(2) {
///   [0]=>
///   string(20) "subjectKeyIdentifier"
///   [1]=>
///   string(16) "basicConstraints"
/// }
/// string(1) "a"
/// bool(false)
/// ```
#[php_function]
fn openssl_x509_parse(
    req: &mut Request,
    certificate: CertParam,
    #[php(default = true)] short_names: bool,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    finish(req, move |req| {
        let Some(cert) = cert_from_param(req, &certificate, Arg::new(1, "certificate"))? else {
            return Ok(OrFalse::False);
        };
        let mut out = PhpArray::new(req)?;
        let subject = cert.subject();
        let Some(name) = subject.oneline() else {
            store_errors(req);
            return Ok(OrFalse::False);
        };
        put_bytes(req, &mut out, b"name", &name)?;
        name_entries(req, &mut out, Some(b"subject"), &subject, short_names)?;
        let hash = format!("{:08x}", cert.subject_hash());
        put_bytes(req, &mut out, b"hash", hash.as_bytes())?;
        name_entries(req, &mut out, Some(b"issuer"), &cert.issuer(), short_names)?;
        put(req, &mut out, b"version", Owner::int(cert.version()))?;
        let (Some(hex_serial), Some(serial)) = cert.serial() else {
            store_errors(req);
            return Ok(OrFalse::False);
        };
        put_bytes(req, &mut out, b"serialNumber", &serial)?;
        put_bytes(req, &mut out, b"serialNumberHex", &hex_serial)?;
        let (from, to) = cert.validity();
        put_bytes(req, &mut out, b"validFrom", &from.data)?;
        put_bytes(req, &mut out, b"validTo", &to.data)?;
        let from = time_t(req, &from);
        put(req, &mut out, b"validFrom_time_t", Owner::int(from))?;
        let to = time_t(req, &to);
        put(req, &mut out, b"validTo_time_t", Owner::int(to))?;
        if let Some(alias) = cert.alias() {
            put_bytes(req, &mut out, b"alias", &alias)?;
        }
        let nid = cert.signature_nid();
        let short = ffi::nid_short_name(nid).unwrap_or_default();
        put_bytes(req, &mut out, b"signatureTypeSN", &short)?;
        let long = ffi::nid_long_name(nid).unwrap_or_default();
        put_bytes(req, &mut out, b"signatureTypeLN", &long)?;
        put(
            req,
            &mut out,
            b"signatureTypeNID",
            Owner::int(i64::from(nid)),
        )?;

        let mut purposes = PhpArray::new(req)?;
        for purpose in cert.purposes() {
            let mut row = PhpArray::new(req)?;
            row.set(req, Owner::index(0), Owner::bool(purpose.fit != 0))?;
            row.set(req, Owner::index(1), Owner::bool(purpose.fit_ca != 0))?;
            let label = if short_names {
                purpose.short
            } else {
                purpose.long
            };
            let label = PhpString::new(req, &label)?.into_value();
            row.set(req, Owner::index(2), label)?;
            purposes.set(req, Owner::index(i64::from(purpose.id)), row.into_value())?;
        }
        put(req, &mut out, b"purposes", purposes.into_value())?;

        let Some(extensions) = cert.extensions() else {
            store_errors(req);
            return Ok(OrFalse::False);
        };
        let mut rows = PhpArray::new(req)?;
        for (name, value) in extensions {
            let value = PhpString::new(req, &value)?.into_value();
            rows.set_symtable_str(req, &name, value)?;
        }
        put(req, &mut out, b"extensions", rows.into_value())?;
        Ok(OrFalse::Value(out))
    })
}

/// The bundle of certificates the operating system keeps, where a client
/// stream or a verifying function falls back to it: macOS's
/// `/etc/ssl/cert.pem`, which a Mac has whether or not it has the OpenSSL
/// the library was built for, and none elsewhere.
pub(crate) const SYSTEM_BUNDLE: Option<&CStr> = if cfg!(target_os = "macos") {
    Some(c"/etc/ssl/cert.pem")
} else {
    None
};

/// Where a peer verification finds the certificates it trusts, decided by
/// [`anchors`] once per stream's `SSL_CTX` and once per verifying
/// function's store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Anchors {
    /// A CA file or directory is named, by a stream's `ssl` options, a
    /// verifying function's `ca_info` or the ini: what php loads is
    /// loaded.
    Named,
    /// A server stream's, where nothing is named: none is loaded, as php
    /// loads none.
    Unnamed,
    /// A client stream's or a verifying function's, where nothing is
    /// named: the library's defaults, as `X509_STORE_set_default_paths`
    /// and `X509_LOOKUP_load_file(…, NULL, X509_FILETYPE_DEFAULT)` read
    /// them.
    Defaults,
    /// A client stream's or a verifying function's, where nothing is
    /// named and the library's defaults name no file it can read: the
    /// system's bundle where the default file would be. The bundled
    /// `libcrypto` of a macOS tarball names Homebrew's file, which a Mac
    /// without Homebrew's OpenSSL lacks.
    System(&'static CStr),
}

/// What the library's defaults name, as `X509_STORE_set_default_paths`
/// and the default file lookup read them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Defaults {
    /// The environment names a file or a directory, `SSL_CERT_FILE` or
    /// `SSL_CERT_DIR` as `X509_get_default_cert_file_env` and
    /// `X509_get_default_cert_dir_env` spell them, which the library reads
    /// in place of its compiled-in paths.
    pub(crate) env_names: bool,
    /// `X509_get_default_cert_file()` opens for reading.
    pub(crate) file_readable: bool,
}

impl Defaults {
    /// The environment and the file system now.
    pub(crate) fn read() -> Self {
        let [file, file_env, _, dir_env, ..] = ffi::cert_locations();
        let set = |name: &[u8]| std::env::var_os(OsStr::from_bytes(name)).is_some();
        Defaults {
            env_names: set(&file_env) || set(&dir_env),
            file_readable: std::fs::File::open(OsStr::from_bytes(&file)).is_ok(),
        }
    }
}

/// The anchors of one stream's `SSL_CTX` or one verifying function's
/// store: php's choice, what is named where anything is, else nothing for
/// a server stream and the library's defaults where `takes_defaults`, a
/// client stream or a verifying function, with one difference. Where the
/// platform has a `system` bundle and the defaults name nothing to read,
/// neither by the environment nor at the compiled-in file, the bundle
/// stands in for the default file. `defaults` is read only where that
/// question is asked.
pub(crate) fn anchors(
    named: bool,
    takes_defaults: bool,
    defaults: impl FnOnce() -> Defaults,
    system: Option<&'static CStr>,
) -> Anchors {
    if named {
        return Anchors::Named;
    }
    if !takes_defaults {
        return Anchors::Unnamed;
    }
    let Some(bundle) = system else {
        return Anchors::Defaults;
    };
    let defaults = defaults();
    if defaults.env_names || defaults.file_readable {
        Anchors::Defaults
    } else {
        Anchors::System(bundle)
    }
}

/// An ini entry naming a path, `openssl.cafile` or `openssl.capath`:
/// its value where it is set and not empty.
pub(crate) fn ini_path(req: &Request, name: &[u8]) -> Option<Vec<u8>> {
    req.ini()
        .get(name)
        .filter(|v| !v.is_empty())
        .map(<[u8]>::to_vec)
}

/// The file a verifying function's store loads where its `ca_info`
/// names no file: the library's default, `None`, as php loads it, or the
/// system's bundle where [`anchors`] stands it in. `named` is whether the
/// `ca_info` names a directory or the ini a CA file or directory.
fn verify_file(
    named: bool,
    defaults: impl FnOnce() -> Defaults,
    system: Option<&'static CStr>,
) -> Option<&'static CStr> {
    match anchors(named, true, defaults, system) {
        Anchors::System(bundle) => Some(bundle),
        Anchors::Named | Anchors::Unnamed | Anchors::Defaults => None,
    }
}

/// `php_openssl_setup_verify`: a store of the files and directories an
/// array names, each loaded as PEM, and the library's default file and
/// directory where it names none of either, the system's bundle standing
/// in for an absent default file where [`verify_file`] says so; `None`
/// where the store cannot be made or a name's conversion throws.
pub(crate) fn setup_verify(
    req: &mut Request,
    calist: Option<PhpArray>,
    arg: Arg,
) -> Result<Option<Store>, Thrown> {
    let Some(mut store) = Store::new() else {
        store_errors(req);
        return Ok(None);
    };
    let (mut files, mut dirs) = (0, 0);
    // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(calist), item)`
    // (`Z/ext/openssl/openssl_backend_common.c:792`), no owner of the table
    // taken: the argument holds it while `zval_try_get_string` runs
    // `__toString`.
    let calist = calist.unwrap_or_else(PhpArray::empty);
    walk::foreach(req, calist, Hold::None, |req, row| -> walk::Step {
        let text = with_string(req, row.value(), <[u8]>::to_vec)?;
        let Some(path) = check_path(req, &text, Origin::Item(arg, None), false) else {
            return Ok(ControlFlow::Continue(()));
        };
        let shown = String::from_utf8_lossy(path.as_bytes()).into_owned();
        let host = std::path::Path::new(std::ffi::OsStr::from_bytes(path.as_bytes()));
        let Ok(meta) = std::fs::metadata(host) else {
            docref(req, Level::Warning, &format!("Unable to stat {shown}"));
            return Ok(ControlFlow::Continue(()));
        };
        let dir = !meta.is_file();
        if store.load(dir, Some(&path)) {
            if dir {
                dirs += 1;
            } else {
                files += 1;
            }
        } else {
            store_errors(req);
            let what = if dir { "directory" } else { "file" };
            docref(
                req,
                Level::Warning,
                &format!("Error loading {what} {shown}"),
            );
        }
        Ok(ControlFlow::Continue(()))
    })?;
    if files == 0 {
        let named = dirs > 0
            || ini_path(req, b"openssl.cafile").is_some()
            || ini_path(req, b"openssl.capath").is_some();
        if !store.load(false, verify_file(named, Defaults::read, SYSTEM_BUNDLE)) {
            store_errors(req);
        }
    }
    if dirs == 0 && !store.load(true, None) {
        store_errors(req);
    }
    Ok(Some(store))
}

/// `php_openssl_check_cert`: `X509_verify_cert` of the certificate under
/// the store and the untrusted chain, for the purpose where it is one;
/// 0 after php's warning where the check cannot start.
pub(crate) fn check_cert(
    req: &mut Request,
    store: &Store,
    cert: &Certificate,
    untrusted: Option<&ffi::x509::CertStack>,
    purpose: i32,
) -> Result<i32, Thrown> {
    match store.check(cert, untrusted, purpose) {
        Checked::NoMemory => {
            store_errors(req);
            let text = format!(
                "{}(): Memory allocation failure",
                req.frames().active_function_name()
            );
            Err(fatal(req, &text))
        }
        Checked::InitFailed => {
            store_errors(req);
            docref(
                req,
                Level::Warning,
                "Certificate store initialization failed",
            );
            Ok(0)
        }
        Checked::Verified(verdict) => {
            store_errors(req);
            Ok(verdict)
        }
    }
}

/// `openssl_x509_checkpurpose(OpenSSLCertificate|string $certificate,`
/// `int $purpose, array $ca_info = [], ?string`
/// `$untrusted_certificates_file = null): bool|int`: whether the
/// certificate chains to the store for the purpose, -1 where it cannot
/// be checked.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $f = __DIR__ . '/ca.pem'; openssl_x509_export_to_file($x, $f);
/// var_dump(openssl_x509_checkpurpose($x, X509_PURPOSE_SSL_CLIENT), openssl_x509_checkpurpose($x, X509_PURPOSE_ANY, [$f]), openssl_x509_checkpurpose('nope', 1));
/// var_dump(openssl_x509_checkpurpose($x, 1, [], __DIR__ . '/none.pem'));
/// unlink($f);
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// int(-1)
///
/// Warning: openssl_x509_checkpurpose(): Error opening the file, %s/none.pem in %s on line 6
/// int(-1)
/// ```
#[php_function]
fn openssl_x509_checkpurpose(
    req: &mut Request,
    certificate: CertParam,
    purpose: i64,
    #[php(default = [])] ca_info: PhpArray,
    #[php(default = null)] untrusted_certificates_file: Option<PhpString>,
) -> Result<Union<bool, i64>, Thrown> {
    finish(req, move |req| {
        let untrusted = match untrusted_certificates_file {
            Some(file) => {
                let arg = Arg::new(4, "untrusted_certificates_file");
                match certs_from_file(req, file.as_bytes(), arg) {
                    Some(stack) => Some(stack),
                    None => return Ok(Union::B(-1)),
                }
            }
            None => None,
        };
        let Some(store) = setup_verify(req, Some(ca_info), Arg::new(3, "ca_info"))? else {
            return Ok(Union::B(-1));
        };
        let Some(cert) = cert_from_param(req, &certificate, Arg::new(1, "certificate"))? else {
            return Ok(Union::B(-1));
        };
        let purpose = crate::config::as_int(purpose);
        let verdict = check_cert(req, &store, &cert, untrusted.as_ref(), purpose)?;
        Ok(match verdict {
            0 | 1 => Union::A(verdict == 1),
            other => Union::B(i64::from(other)),
        })
    })
}

/// `openssl_get_cert_locations(): array`: where the library looks for
/// certificates, and the two ini entries.
///
/// ```php
/// $l = openssl_get_cert_locations();
/// var_dump(array_keys($l), $l['default_cert_file_env'], $l['ini_cafile']);
/// ```
/// ```expect
/// array(8) {
///   [0]=>
///   string(17) "default_cert_file"
///   [1]=>
///   string(21) "default_cert_file_env"
///   [2]=>
///   string(16) "default_cert_dir"
///   [3]=>
///   string(20) "default_cert_dir_env"
///   [4]=>
///   string(19) "default_private_dir"
///   [5]=>
///   string(25) "default_default_cert_area"
///   [6]=>
///   string(10) "ini_cafile"
///   [7]=>
///   string(10) "ini_capath"
/// }
/// string(13) "SSL_CERT_FILE"
/// string(0) ""
/// ```
#[php_function]
fn openssl_get_cert_locations(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    let names: [&[u8]; 6] = [
        b"default_cert_file",
        b"default_cert_file_env",
        b"default_cert_dir",
        b"default_cert_dir_env",
        b"default_private_dir",
        b"default_default_cert_area",
    ];
    for (name, value) in names.into_iter().zip(ffi::cert_locations()) {
        put_bytes(req, &mut out, name, &value)?;
    }
    for (name, entry) in [
        (&b"ini_cafile"[..], &b"openssl.cafile"[..]),
        (b"ini_capath", b"openssl.capath"),
    ] {
        let value = req.ini().get(entry).map(<[u8]>::to_vec).unwrap_or_default();
        put_bytes(req, &mut out, name, &value)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{Anchors, Defaults, SYSTEM_BUNDLE, anchors, verify_file};

    /// The system's bundle is a client stream's only where nothing names a
    /// CA, neither the options, the ini nor the environment, and the
    /// library's compiled-in file cannot be read; everywhere else the
    /// anchors are php's.
    #[test]
    fn the_system_bundle_stands_in_only_for_an_absent_default_file() {
        let bundle = c"/etc/ssl/cert.pem";
        let seen = |env_names, file_readable| {
            move || Defaults {
                env_names,
                file_readable,
            }
        };
        let unread = || -> Defaults { panic!("the defaults are read only where they decide") };
        for (env_names, file_readable) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let want = if env_names || file_readable {
                Anchors::Defaults
            } else {
                Anchors::System(bundle)
            };
            let got = anchors(false, true, seen(env_names, file_readable), Some(bundle));
            assert_eq!(got, want, "env {env_names}, file {file_readable}");
        }
        assert_eq!(anchors(true, true, unread, Some(bundle)), Anchors::Named);
        assert_eq!(anchors(true, false, unread, Some(bundle)), Anchors::Named);
        assert_eq!(
            anchors(false, false, unread, Some(bundle)),
            Anchors::Unnamed
        );
        assert_eq!(anchors(false, true, unread, None), Anchors::Defaults);
        assert_eq!(SYSTEM_BUNDLE.is_some(), cfg!(target_os = "macos"));
    }

    /// A verifying function's store takes the system's bundle in place of
    /// the default file under the same condition as a client stream: its
    /// `ca_info` names no directory, the ini no CA, the environment
    /// nothing, and the compiled-in file cannot be read. Where anything is
    /// named, or the platform keeps no bundle, the file is php's default.
    #[test]
    fn a_verifying_function_takes_the_system_bundle_as_a_client_stream_does() {
        let bundle = c"/etc/ssl/cert.pem";
        let seen = |env_names, file_readable| {
            move || Defaults {
                env_names,
                file_readable,
            }
        };
        let unread = || -> Defaults { panic!("the defaults are read only where they decide") };
        for (env_names, file_readable) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let stream = anchors(false, true, seen(env_names, file_readable), Some(bundle));
            let want = (stream == Anchors::System(bundle)).then_some(bundle);
            let got = verify_file(false, seen(env_names, file_readable), Some(bundle));
            assert_eq!(got, want, "env {env_names}, file {file_readable}");
        }
        assert_eq!(
            verify_file(false, seen(false, false), Some(bundle)),
            Some(bundle)
        );
        assert_eq!(verify_file(true, unread, Some(bundle)), None);
        assert_eq!(verify_file(false, unread, None), None);
    }
}
