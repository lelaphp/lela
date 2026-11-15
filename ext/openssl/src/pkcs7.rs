//! The `openssl_pkcs7_*` natives of openssl.c: S/MIME signing,
//! verification, encryption and decryption of files, and the
//! certificates and CRLs a PEM `PKCS7` carries.

use std::ops::ControlFlow;

use php_ext::abi::{Path, Ref, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::with_string;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value, ValueRef};

use crate::config::{as_int, cipher_of};
use crate::constants::PKCS7_DETACHED;
use crate::decode::{
    Arg, CertParam, Origin, c_string, cert_from_param, cert_from_value, cert_stack,
    certs_from_file, finish, open_file, raised,
};
use crate::ffi::smime::Pkcs7;
use crate::ffi::x509::{CertStack, Pems};
use crate::ffi::{Bio, Certificate, PKey};
use crate::key::{Half, check_int_len, key_from};
use crate::store_errors;
use crate::x509::setup_verify;

/// The extra headers `openssl_pkcs7_sign`, `_encrypt` and their CMS kin
/// write before the message: `Name: value` for a string key and the
/// value alone for an integer one, each as `BIO_printf`'s `%s` reads it,
/// up to a NUL. False where a value's conversion throws or a write fails,
/// the latter with the library's reasons stored.
pub(crate) fn write_headers(
    req: &mut Request,
    out: &Bio<'_>,
    headers: Option<PhpArray>,
) -> Result<bool, Thrown> {
    let Some(headers) = headers else {
        return Ok(true);
    };
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(Z_ARRVAL_P(zheaders), strindex, zcertval)`
    // (`Z/ext/openssl/openssl.c:2729`), no owner of the table taken: the
    // argument holds it while `zval_try_get_string` runs `__toString`.
    let refused = walk::foreach(req, headers, Hold::None, |req, row| -> walk::Step {
        let text = with_string(req, row.value(), c_string)?;
        let mut line = Vec::new();
        if let Key::Str(name) = row.key() {
            line.extend_from_slice(c_string(name.as_bytes()).as_bytes());
            line.extend_from_slice(b": ");
        }
        line.extend_from_slice(text.as_bytes());
        line.push(b'\n');
        if !out.write(&line) {
            store_errors(req);
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(refused.is_none())
}

/// The recipients of an encryption: `php_openssl_x509_from_zval` of each
/// member of an array, or of the one value, as a stack.
pub(crate) fn recipients(
    req: &mut Request,
    value: &Value,
    arg: Arg,
) -> Result<Option<CertStack>, Thrown> {
    cert_stack(req, value, arg, None)
}

/// A file a verification writes to, where a parameter names one.
pub(crate) enum Output {
    /// None is named.
    Unnamed,
    /// The one named, open.
    File(Bio<'static>),
    /// The one named does not open.
    Unopened,
}

impl Output {
    /// The file's BIO, where one is open.
    pub(crate) fn bio(&self) -> Option<&Bio<'static>> {
        match self {
            Output::File(bio) => Some(bio),
            Output::Unnamed | Output::Unopened => None,
        }
    }
}

/// The file a verification's parameter names, opened for writing.
pub(crate) fn output(req: &mut Request, file: Option<Path>, arg: Arg) -> Output {
    let Some(file) = file else {
        return Output::Unnamed;
    };
    match open_file(req, file.0.as_bytes(), arg, c"w") {
        Some(bio) => Output::File(bio),
        None => Output::Unopened,
    }
}

/// The signers of a verified message written to the file php's
/// `signers` parameter names, as PEM: false after php's warning where it
/// does not open, or where the signers cannot be had or one does not
/// write. `signers` is asked only once the file is open.
pub(crate) fn signers_file(
    req: &mut Request,
    file: Path,
    arg: Arg,
    signers: impl FnOnce() -> Option<Pems>,
) -> bool {
    let Some(out) = open_file(req, file.0.as_bytes(), arg, c"w") else {
        let text = format!(
            "Signature OK, but cannot open {} for writing",
            String::from_utf8_lossy(file.0.as_bytes())
        );
        docref(req, Level::Warning, &text);
        return false;
    };
    let Some(pems) = signers() else {
        store_errors(req);
        return false;
    };
    write_signers(req, &out, pems)
}

/// Each PEM of a list written to `certout` in turn, `Failed to write`
/// `signer N` for one that would not write; false where any failed.
pub(crate) fn write_signers(req: &mut Request, out: &Bio<'_>, pems: Pems) -> bool {
    let mut ok = true;
    for (i, pem) in pems.into_iter().enumerate() {
        if !pem.is_some_and(|pem| out.write(&pem)) {
            store_errors(req);
            ok = false;
            docref(req, Level::Warning, &format!("Failed to write signer {i}"));
        }
    }
    ok
}

/// The certificates, then the CRLs, of a structure as PEM into an array
/// under their positions, a CRL taking the key of the certificate at
/// the same position as php's loops give it.
pub(crate) fn pem_list(
    req: &mut Request,
    certs: Pems,
    crls: Pems,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for list in [certs, crls] {
        for (i, pem) in list.into_iter().enumerate() {
            if let Some(pem) = pem {
                let pem = PhpString::new(req, &pem)?.into_value();
                out.set(req, Owner::index(i64::try_from(i).unwrap_or(0)), pem)?;
            }
        }
    }
    Ok(out)
}

/// `openssl_pkcs7_verify(string $input_filename, int $flags, ?string`
/// `$signers_certificates_filename = null, array $ca_info = [], ?string`
/// `$untrusted_certificates_filename = null, ?string $content = null,`
/// `?string $output_filename = null): bool|int`: whether the signed
/// message verifies against the store; -1 where it cannot be checked.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $d = __DIR__; file_put_contents("$d/in.txt", "hello\n"); openssl_x509_export_to_file($x, "$d/ca.pem");
/// var_dump(openssl_pkcs7_sign("$d/in.txt", "$d/out.txt", $x, $k, ['To' => 'a@b']));
/// var_dump(openssl_pkcs7_verify("$d/out.txt", 0, "$d/signers.pem", ["$d/ca.pem"]), substr(file_get_contents("$d/signers.pem"), 0, 27));
/// var_dump(openssl_pkcs7_verify("$d/in.txt", 0), openssl_pkcs7_verify("$d/out.txt", 0, null, [], "$d/none.pem"));
/// foreach (['in.txt', 'out.txt', 'ca.pem', 'signers.pem'] as $f) unlink("$d/$f");
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(27) "-----BEGIN CERTIFICATE-----"
///
/// Warning: openssl_pkcs7_verify(): Error opening the file, %s/none.pem in %s on line 7
/// int(-1)
/// int(-1)
/// ```
#[php_function]
fn openssl_pkcs7_verify(
    req: &mut Request,
    input_filename: Path,
    flags: i64,
    #[php(default = null)] signers_certificates_filename: Option<Path>,
    #[php(default = [])] ca_info: PhpArray,
    #[php(default = null)] untrusted_certificates_filename: Option<Path>,
    #[php(default = null)] content: Option<Path>,
    #[php(default = null)] output_filename: Option<Path>,
) -> Result<Union<bool, i64>, Thrown> {
    finish(req, move |req| {
        let others = match &untrusted_certificates_filename {
            Some(file) => {
                let arg = Arg::new(5, "untrusted_certificates_filename");
                match certs_from_file(req, file.0.as_bytes(), arg) {
                    Some(stack) => Some(stack),
                    None => return Ok(Union::B(-1)),
                }
            }
            None => None,
        };
        let flags = as_int(flags) & !as_int(PKCS7_DETACHED);
        let Some(store) = setup_verify(req, Some(ca_info), Arg::new(4, "ca_info"))? else {
            return Ok(Union::B(-1));
        };
        let Some(input) = open_file(
            req,
            input_filename.0.as_bytes(),
            Arg::new(1, "input_filename"),
            c"r",
        ) else {
            return Ok(Union::B(-1));
        };
        let Some((p7, data)) = Pkcs7::read_smime(&input) else {
            store_errors(req);
            return Ok(Union::B(-1));
        };
        let data_out = output(req, content, Arg::new(6, "content"));
        if matches!(data_out, Output::Unopened) {
            return Ok(Union::B(-1));
        }
        let p7b_out = output(req, output_filename, Arg::new(7, "output_filename"));
        if matches!(p7b_out, Output::Unopened) {
            return Ok(Union::B(-1));
        }
        if !p7.verify(
            others.as_ref(),
            &store,
            data.as_ref(),
            data_out.bio(),
            flags,
        ) {
            store_errors(req);
            return Ok(Union::A(false));
        }
        let mut verdict = Union::A(true);
        if let Some(file) = signers_certificates_filename {
            let arg = Arg::new(3, "signers_certificates_filename");
            if !signers_file(req, file, arg, || p7.signers(others.as_ref(), flags)) {
                verdict = Union::B(-1);
            }
            if let Some(out) = p7b_out.bio()
                && !p7.write_pem(out)
            {
                docref(req, Level::Warning, "Failed to write PKCS7 to file");
                store_errors(req);
                verdict = Union::A(false);
            }
        }
        Ok(verdict)
    })
}

/// `openssl_pkcs7_encrypt(string $input_filename, string`
/// `$output_filename, $certificate, ?array $headers, int $flags = 0, int`
/// `$cipher_algo = OPENSSL_CIPHER_AES_128_CBC): bool`: the message
/// encrypted to the recipients' certificates, as S/MIME.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $d = __DIR__; file_put_contents("$d/in.txt", "hello\n");
/// var_dump(openssl_pkcs7_encrypt("$d/in.txt", "$d/enc.txt", $x, ['Subject' => 'hi', 'x']), strpos(file_get_contents("$d/enc.txt"), "Subject: hi\nx\n") === 0);
/// var_dump(openssl_pkcs7_decrypt("$d/enc.txt", "$d/dec.txt", $x, $k), file_get_contents("$d/dec.txt"));
/// var_dump(openssl_pkcs7_encrypt("$d/in.txt", "$d/enc.txt", $x, null, 0, 99), openssl_pkcs7_encrypt("$d/in.txt", "$d/enc.txt", 'nope', null));
/// foreach (['in.txt', 'enc.txt', 'dec.txt'] as $f) unlink("$d/$f");
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// string(7) "hello
/// "
///
/// Warning: openssl_pkcs7_encrypt(): Failed to get cipher in %s on line 7
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_pkcs7_encrypt(
    req: &mut Request,
    input_filename: Path,
    output_filename: Path,
    certificate: Untyped<Value>,
    headers: Option<PhpArray>,
    #[php(default = 0)] flags: i64,
    #[php(default = OPENSSL_CIPHER_AES_128_CBC)] cipher_algo: i64,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let Some(input) = open_file(
            req,
            input_filename.0.as_bytes(),
            Arg::new(1, "input_filename"),
            c"r",
        ) else {
            return Ok(false);
        };
        let Some(out) = open_file(
            req,
            output_filename.0.as_bytes(),
            Arg::new(2, "output_filename"),
            c"w",
        ) else {
            return Ok(false);
        };
        let Some(certs) = recipients(req, &certificate.0, Arg::new(3, "certificate"))? else {
            return Ok(false);
        };
        let Some(cipher) = cipher_of(cipher_algo) else {
            docref(req, Level::Warning, "Failed to get cipher");
            return Ok(false);
        };
        let flags = as_int(flags);
        let Some(p7) = Pkcs7::encrypt(&certs, &input, &cipher, flags) else {
            store_errors(req);
            return Ok(false);
        };
        if !write_headers(req, &out, headers)? {
            return Ok(false);
        }
        input.reset();
        if !p7.write_smime(&out, &input, flags) {
            store_errors(req);
            return Ok(false);
        }
        Ok(true)
    })
}

/// `openssl_pkcs7_read(string $data, &$certificates): bool`: the
/// certificates and CRLs of a PEM `PKCS7`, as PEM.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $d = __DIR__; file_put_contents("$d/in.txt", "hello\n"); openssl_pkcs7_sign("$d/in.txt", "$d/out.txt", $x, $k, [], 0);
/// $p7 = "-----BEGIN PKCS7-----\n" . trim(explode("\n\n", file_get_contents("$d/out.txt"), 2)[1]) . "\n-----END PKCS7-----\n";
/// var_dump(openssl_pkcs7_read($p7, $certs), count($certs), substr($certs[0], 0, 27));
/// var_dump(openssl_pkcs7_read('', $certs), openssl_pkcs7_read('nope', $certs));
/// foreach (['in.txt', 'out.txt'] as $f) unlink("$d/$f");
/// ```
/// ```expect
/// bool(true)
/// int(1)
/// string(27) "-----BEGIN CERTIFICATE-----"
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_pkcs7_read(
    req: &mut Request,
    data: PhpString,
    certificates: Ref<Untyped<Value>>,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        check_int_len(req, data.as_bytes().len(), "p7b", 1)?;
        let bio = Bio::reading(data.as_bytes()).filter(|_| !data.is_empty());
        let Some(p7) = bio.and_then(|bio| Pkcs7::read_pem(&bio)) else {
            store_errors(req);
            return Ok(false);
        };
        let (certs, crls) = p7.certs_and_crls();
        let list = pem_list(req, certs, crls)?;
        certificates.set(req, Untyped(list.into_value()))?;
        Ok(true)
    })
}

/// `openssl_pkcs7_sign(string $input_filename, string $output_filename,`
/// `OpenSSLCertificate|string $certificate, $private_key, ?array`
/// `$headers, int $flags = PKCS7_DETACHED, ?string`
/// `$untrusted_certificates_filename = null): bool`: the message signed
/// with the certificate and its key, as S/MIME.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $d = __DIR__; file_put_contents("$d/in.txt", "hello\n");
/// var_dump(openssl_pkcs7_sign("$d/in.txt", "$d/out.txt", $x, $k, null), strpos(file_get_contents("$d/out.txt"), 'MIME-Version: 1.0') === 0);
/// var_dump(openssl_pkcs7_sign("$d/in.txt", "$d/out.txt", $x, 'nope', null), openssl_pkcs7_sign("$d/none.txt", "$d/out.txt", $x, $k, null));
/// foreach (['in.txt', 'out.txt'] as $f) unlink("$d/$f");
/// ```
/// ```expect
/// bool(true)
/// bool(true)
///
/// Warning: openssl_pkcs7_sign(): Error getting private key in %s on line 6
///
/// Warning: openssl_pkcs7_sign(): Error opening input file %s/none.txt! in %s on line 6
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_pkcs7_sign(
    req: &mut Request,
    input_filename: Path,
    output_filename: Path,
    certificate: CertParam,
    #[php(sensitive)] private_key: Untyped<Value>,
    headers: Option<PhpArray>,
    #[php(default = PKCS7_DETACHED)] flags: i64,
    #[php(default = null)] untrusted_certificates_filename: Option<Path>,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let Some((others, key, cert)) = signing(
            req,
            untrusted_certificates_filename,
            7,
            &private_key.0,
            &certificate,
        )?
        else {
            return Ok(false);
        };
        let Some((input, out)) = signing_files(req, input_filename, output_filename) else {
            return Ok(false);
        };
        let flags = as_int(flags);
        let Some(p7) = Pkcs7::sign(&cert, &key, others.as_ref(), &input, flags) else {
            store_errors(req);
            docref(req, Level::Warning, "Error creating PKCS7 structure!");
            return Ok(false);
        };
        input.reset();
        if !write_headers(req, &out, headers)? {
            return Ok(false);
        }
        if !p7.write_smime(&out, &input, flags) {
            store_errors(req);
            return Ok(false);
        }
        Ok(true)
    })
}

/// What `openssl_pkcs7_sign` and `openssl_cms_sign` read before their
/// files: the untrusted certificates, the private key and the
/// certificate, each after php's warning where it cannot be had.
pub(crate) fn signing(
    req: &mut Request,
    untrusted: Option<Path>,
    untrusted_position: u32,
    private_key: &Value,
    certificate: &CertParam,
) -> Result<Option<(Option<CertStack>, PKey, Certificate)>, Thrown> {
    let others = match untrusted {
        Some(file) => {
            let arg = Arg::new(untrusted_position, "untrusted_certificates_filename");
            match certs_from_file(req, file.0.as_bytes(), arg) {
                Some(stack) => Some(stack),
                None => return Ok(None),
            }
        }
        None => None,
    };
    let arg = Arg::new(4, "private_key");
    let Some(key) = key_from(req, private_key, Half::Private, Some(b""), arg)? else {
        if !raised(req) {
            docref(req, Level::Warning, "Error getting private key");
        }
        return Ok(None);
    };
    let Some(cert) = cert_from_param(req, certificate, Arg::new(3, "certificate"))? else {
        docref(req, Level::Warning, "X.509 Certificate cannot be retrieved");
        return Ok(None);
    };
    Ok(Some((others, key, cert)))
}

/// The files a signing native reads and writes, each opened after php's
/// `Error opening ... file` warning where it does not open.
pub(crate) fn signing_files(
    req: &mut Request,
    input: Path,
    output: Path,
) -> Option<(Bio<'static>, Bio<'static>)> {
    let files = [
        (input, Arg::new(1, "input_filename"), c"r", "input"),
        (output, Arg::new(2, "output_filename"), c"w", "output"),
    ];
    let mut opened = Vec::new();
    for (path, arg, mode, what) in files {
        let Some(bio) = open_file(req, path.0.as_bytes(), arg, mode) else {
            let text = format!(
                "Error opening {what} file {}!",
                String::from_utf8_lossy(path.0.as_bytes())
            );
            docref(req, Level::Warning, &text);
            return None;
        };
        opened.push(bio);
    }
    let out = opened.pop();
    opened.pop().zip(out)
}

/// The certificate and private key a decryption uses: the certificate
/// the value is, and the key `$private_key` gives or the certificate
/// value itself does, each after php's warning where it cannot be had.
pub(crate) fn decrypting(
    req: &mut Request,
    certificate: &Value,
    private_key: Option<&Value>,
    key_position: u32,
) -> Result<Option<(Certificate, PKey)>, Thrown> {
    let origin = Origin::Arg(Arg::new(3, "certificate"));
    let Some(cert) = cert_from_value(req, certificate, origin)? else {
        docref(req, Level::Warning, "X.509 Certificate cannot be retrieved");
        return Ok(None);
    };
    let arg = Arg::new(key_position, "private_key");
    let source = private_key.unwrap_or(certificate);
    let Some(key) = key_from(req, source, Half::Private, Some(b""), arg)? else {
        if !raised(req) {
            docref(req, Level::Warning, "Unable to get private key");
        }
        return Ok(None);
    };
    Ok(Some((cert, key)))
}

/// `openssl_pkcs7_decrypt(string $input_filename, string`
/// `$output_filename, $certificate, $private_key = null): bool`: an
/// S/MIME message decrypted with a recipient's certificate and key.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $d = __DIR__; file_put_contents("$d/in.txt", "hello\n");
/// openssl_pkcs7_encrypt("$d/in.txt", "$d/enc.txt", [$x], null);
/// var_dump(openssl_pkcs7_decrypt("$d/enc.txt", "$d/dec.txt", $x, $k), file_get_contents("$d/dec.txt"));
/// var_dump(openssl_pkcs7_decrypt("$d/enc.txt", "$d/dec.txt", $x), openssl_pkcs7_decrypt("$d/in.txt", "$d/dec.txt", $x, $k));
/// foreach (['in.txt', 'enc.txt', 'dec.txt'] as $f) unlink("$d/$f");
/// ```
/// ```expect
/// bool(true)
/// string(7) "hello
/// "
///
/// Warning: openssl_pkcs7_decrypt(): Unable to get private key in %s on line 7
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_pkcs7_decrypt(
    req: &mut Request,
    input_filename: Path,
    output_filename: Path,
    #[php(sensitive)] certificate: Untyped<Value>,
    #[php(sensitive, default = null)] private_key: Untyped<Value>,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let key = Some(private_key.0).filter(|v| !matches!(v.as_ref(), ValueRef::Null));
        let Some((cert, key)) = decrypting(req, &certificate.0, key.as_ref(), 4)? else {
            return Ok(false);
        };
        let Some(input) = open_file(
            req,
            input_filename.0.as_bytes(),
            Arg::new(1, "input_filename"),
            c"r",
        ) else {
            return Ok(false);
        };
        let Some(out) = open_file(
            req,
            output_filename.0.as_bytes(),
            Arg::new(2, "output_filename"),
            c"w",
        ) else {
            return Ok(false);
        };
        let Some((p7, _data)) = Pkcs7::read_smime(&input) else {
            store_errors(req);
            return Ok(false);
        };
        if p7.decrypt(&key, &cert, &out, as_int(PKCS7_DETACHED)) {
            return Ok(true);
        }
        store_errors(req);
        Ok(false)
    })
}
