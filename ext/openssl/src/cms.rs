//! The `openssl_cms_*` natives of openssl.c, the CMS kin of the PKCS#7
//! ones: sign, verify, encrypt and decrypt files as S/MIME, PEM or DER,
//! and the certificates and CRLs a PEM CMS structure carries.

use php_ext::abi::{Path, Ref, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, Value, ValueRef};

use crate::config::{as_int, cipher_of};
use crate::constants::{OPENSSL_ENCODING_DER, OPENSSL_ENCODING_PEM, OPENSSL_ENCODING_SMIME};
use crate::decode::{Arg, CertParam, c_string, certs_from_file, finish, open_file};
use crate::ffi::smime::Cms;
use crate::ffi::{Bio, Cipher};
use crate::key::check_int_len;
use crate::pkcs7::{
    Output, decrypting, output, pem_list, recipients, signers_file, signing, signing_files,
    write_headers,
};
use crate::store_errors;
use crate::x509::setup_verify;

/// `CMS_DETACHED`, `CMS_STREAM` and `CMS_PARTIAL`.
const CMS_DETACHED: i64 = 0x40;
const CMS_STREAM: i64 = 0x1000;
const CMS_PARTIAL: i64 = 0x4000;

/// The encoding a CMS native reads or writes, `OPENSSL_ENCODING_*`
/// decoded once; `None` for any other number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Der,
    Smime,
    Pem,
}

impl Encoding {
    fn of(n: i64) -> Option<Encoding> {
        match n {
            OPENSSL_ENCODING_DER => Some(Encoding::Der),
            OPENSSL_ENCODING_SMIME => Some(Encoding::Smime),
            OPENSSL_ENCODING_PEM => Some(Encoding::Pem),
            _ => None,
        }
    }
}

/// A structure as the encoding writes it, `CMS_final` first where the
/// flags ask for a partial structure that is not streamed; false with
/// the library's reasons stored, or after php's warning for an encoding
/// it does not know.
fn write(
    req: &mut Request,
    cms: &mut Cms,
    out: &Bio<'_>,
    input: &Bio<'_>,
    flags: i64,
    encoding: i64,
) -> bool {
    let bits = as_int(flags);
    let finish = flags & CMS_PARTIAL != 0 && flags & CMS_STREAM == 0;
    let unsigned = u32::from_ne_bytes(bits.to_ne_bytes());
    let written = match Encoding::of(encoding) {
        Some(Encoding::Smime) => cms.write_smime(out, input, bits),
        Some(Encoding::Der) => (!finish || cms.finish(input, unsigned)) && cms.write_der(out),
        Some(Encoding::Pem) => {
            (!finish || cms.finish(input, unsigned))
                && if flags & CMS_STREAM != 0 {
                    cms.write_pem_stream(out, input, bits)
                } else {
                    cms.write_pem(out)
                }
        }
        None => {
            docref(req, Level::Warning, "Unknown OPENSSL encoding");
            return false;
        }
    };
    if !written {
        store_errors(req);
    }
    written
}

/// `openssl_cms_verify(string $input_filename, int $flags = 0, ?string`
/// `$certificates = null, array $ca_info = [], ?string`
/// `$untrusted_certificates_filename = null, ?string $content = null,`
/// `?string $pk7 = null, ?string $sigfile = null, int $encoding =`
/// `OPENSSL_ENCODING_SMIME): bool`: whether the signed message verifies
/// against the store.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $d = __DIR__; file_put_contents("$d/in.txt", "hello\n"); openssl_x509_export_to_file($x, "$d/ca.pem");
/// var_dump(openssl_cms_sign("$d/in.txt", "$d/out.txt", $x, $k, null));
/// var_dump(openssl_cms_verify("$d/out.txt", 0, "$d/signers.pem", ["$d/ca.pem"]), substr(file_get_contents("$d/signers.pem"), 0, 27));
/// var_dump(openssl_cms_verify("$d/in.txt", 0), openssl_cms_verify("$d/out.txt", OPENSSL_CMS_DETACHED, null, [], null, null, null, "$d/x"));
/// try { openssl_cms_verify("$d/out.txt"); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// foreach (['in.txt', 'out.txt', 'ca.pem', 'signers.pem'] as $f) unlink("$d/$f");
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(27) "-----BEGIN CERTIFICATE-----"
///
/// Warning: openssl_cms_verify(): Detached signatures not possible with S/MIME encoding in %s on line 7
/// bool(false)
/// bool(false)
/// openssl_cms_verify() expects at least 2 arguments, 1 given
/// ```
#[php_function]
fn openssl_cms_verify(
    req: &mut Request,
    input_filename: Path,
    #[php(default = 0)] flags: i64,
    #[php(default = null)] certificates: Option<Path>,
    #[php(default = [])] ca_info: PhpArray,
    #[php(default = null)] untrusted_certificates_filename: Option<Path>,
    #[php(default = null)] content: Option<Path>,
    #[php(default = null)] pk7: Option<Path>,
    #[php(default = null)] sigfile: Option<Path>,
    #[php(default = OPENSSL_ENCODING_SMIME)] encoding: i64,
) -> Result<bool, Thrown> {
    // `"pl|p!ap!p!p!p!l"`: the flags are required where the stub gives
    // them a default.
    php_ext::raising::arity(req, 2, Some(9))?;
    finish(req, move |req| {
        let Some(input) = open_file(
            req,
            input_filename.0.as_bytes(),
            Arg::new(1, "input_filename"),
            c"r",
        ) else {
            return Ok(false);
        };
        let detached = flags & CMS_DETACHED != 0;
        let sig = match &sigfile {
            Some(file) if detached => {
                if encoding == OPENSSL_ENCODING_SMIME {
                    docref(
                        req,
                        Level::Warning,
                        "Detached signatures not possible with S/MIME encoding",
                    );
                    return Ok(false);
                }
                match open_file(req, file.0.as_bytes(), Arg::new(1, "input_filename"), c"r") {
                    Some(bio) => Some(bio),
                    None => return Ok(false),
                }
            }
            _ => None,
        };
        let sig_in = sig.as_ref().unwrap_or(&input);
        let (cms, content_in) = match Encoding::of(encoding) {
            Some(Encoding::Pem) => (Cms::read_pem(sig_in), None),
            Some(Encoding::Der) => (Cms::read_der(sig_in), None),
            Some(Encoding::Smime) => match Cms::read_smime(sig_in) {
                Some((cms, content)) => (Some(cms), content),
                None => (None, None),
            },
            None => {
                docref(req, Level::Warning, "Unknown encoding");
                return Ok(false);
            }
        };
        let Some(cms) = cms else {
            store_errors(req);
            return Ok(false);
        };
        let data_in = match Encoding::of(encoding) {
            Some(Encoding::Smime) => content_in.as_ref(),
            _ => detached.then_some(&input),
        };
        let others = match &untrusted_certificates_filename {
            Some(file) => {
                let arg = Arg::new(5, "untrusted_certificates_filename");
                match certs_from_file(req, file.0.as_bytes(), arg) {
                    Some(stack) => Some(stack),
                    None => return Ok(false),
                }
            }
            None => None,
        };
        let Some(store) = setup_verify(req, Some(ca_info), Arg::new(4, "ca_info"))? else {
            return Ok(false);
        };
        let data_out = output(req, content, Arg::new(6, "content"));
        if matches!(data_out, Output::Unopened) {
            return Ok(false);
        }
        let p7b_out = output(req, pk7, Arg::new(7, "pk7"));
        if matches!(p7b_out, Output::Unopened) {
            return Ok(false);
        }
        let unsigned = u32::from_ne_bytes(as_int(flags).to_ne_bytes());
        if !cms.verify(others.as_ref(), &store, data_in, data_out.bio(), unsigned) {
            store_errors(req);
            return Ok(false);
        }
        let mut verdict = true;
        if let Some(file) = certificates {
            verdict = signers_file(req, file, Arg::new(3, "certificates"), || cms.signers());
            if let Some(out) = p7b_out.bio()
                && !cms.write_pem(out)
            {
                docref(req, Level::Warning, "Failed to write CMS to file");
                store_errors(req);
                verdict = false;
            }
        }
        Ok(verdict)
    })
}

/// `openssl_cms_encrypt(string $input_filename, string $output_filename,`
/// `$certificate, ?array $headers, int $flags = 0, int $encoding =`
/// `OPENSSL_ENCODING_SMIME, string|int $cipher_algo =`
/// `OPENSSL_CIPHER_AES_128_CBC): bool`: the message encrypted to the
/// recipients' certificates.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $d = __DIR__; file_put_contents("$d/in.txt", "hello\n");
/// foreach ([OPENSSL_ENCODING_SMIME, OPENSSL_ENCODING_PEM, OPENSSL_ENCODING_DER] as $e) {
///     var_dump(openssl_cms_encrypt("$d/in.txt", "$d/enc.txt", $x, null, 0, $e, 'aes-256-cbc'), openssl_cms_decrypt("$d/enc.txt", "$d/dec.txt", $x, $k, $e), file_get_contents("$d/dec.txt"));
/// }
/// var_dump(openssl_cms_encrypt("$d/in.txt", "$d/enc.txt", $x, null, 0, 7), openssl_cms_encrypt("$d/in.txt", "$d/enc.txt", $x, null, 0, 1, 'nope'));
/// foreach (['in.txt', 'enc.txt', 'dec.txt'] as $f) unlink("$d/$f");
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(7) "hello
/// "
/// bool(true)
/// bool(true)
/// string(7) "hello
/// "
/// bool(true)
/// bool(true)
/// string(7) "hello
/// "
///
/// Warning: openssl_cms_encrypt(): Unknown OPENSSL encoding in %s on line 8
///
/// Warning: openssl_cms_encrypt(): Failed to get cipher in %s on line 8
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_cms_encrypt(
    req: &mut Request,
    input_filename: Path,
    output_filename: Path,
    certificate: Untyped<Value>,
    headers: Option<PhpArray>,
    #[php(default = 0)] flags: i64,
    #[php(default = OPENSSL_ENCODING_SMIME)] encoding: i64,
    #[php(default = OPENSSL_CIPHER_AES_128_CBC)] cipher_algo: Union<PhpString, i64>,
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
        let cipher = match &cipher_algo {
            Union::A(name) => Cipher::by_name(&c_string(name.as_bytes())),
            Union::B(algo) => cipher_of(*algo),
        };
        let Some(cipher) = cipher else {
            docref(req, Level::Warning, "Failed to get cipher");
            return Ok(false);
        };
        let unsigned = u32::from_ne_bytes(as_int(flags).to_ne_bytes());
        let Some(mut cms) = Cms::encrypt(&certs, &input, &cipher, unsigned) else {
            store_errors(req);
            return Ok(false);
        };
        if encoding == OPENSSL_ENCODING_SMIME && !write_headers(req, &out, headers)? {
            return Ok(false);
        }
        input.reset();
        Ok(write(req, &mut cms, &out, &input, flags, encoding))
    })
}

/// `openssl_cms_read(string $input_filename, &$certificates): bool`: the
/// certificates and CRLs of a signed PEM CMS structure, as PEM.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $d = __DIR__; file_put_contents("$d/in.txt", "hello\n");
/// openssl_cms_sign("$d/in.txt", "$d/out.pem", $x, $k, null, 0, OPENSSL_ENCODING_PEM);
/// var_dump(openssl_cms_read(file_get_contents("$d/out.pem"), $certs), count($certs), substr($certs[0], 0, 27));
/// var_dump(openssl_cms_read('nope', $certs));
/// foreach (['in.txt', 'out.pem'] as $f) unlink("$d/$f");
/// ```
/// ```expect
/// bool(true)
/// int(1)
/// string(27) "-----BEGIN CERTIFICATE-----"
/// bool(false)
/// ```
#[php_function]
fn openssl_cms_read(
    req: &mut Request,
    input_filename: PhpString,
    certificates: Ref<Untyped<Value>>,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let data = input_filename.as_bytes();
        check_int_len(req, data.len(), "p7b", 1)?;
        let bio = Bio::reading(data).filter(|_| !data.is_empty());
        let Some(cms) = bio.and_then(|bio| Cms::read_pem(&bio)) else {
            store_errors(req);
            return Ok(false);
        };
        let (certs, crls) = cms.certs_and_crls();
        let list = pem_list(req, certs, crls)?;
        certificates.set(req, Untyped(list.into_value()))?;
        Ok(true)
    })
}

/// `openssl_cms_sign(string $input_filename, string $output_filename,`
/// `OpenSSLCertificate|string $certificate, $private_key, ?array`
/// `$headers, int $flags = 0, int $encoding = OPENSSL_ENCODING_SMIME,`
/// `?string $untrusted_certificates_filename = null): bool`: the message
/// signed with the certificate and its key.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $d = __DIR__; file_put_contents("$d/in.txt", "hello\n");
/// var_dump(openssl_cms_sign("$d/in.txt", "$d/out.txt", $x, $k, ['To' => 'a@b']), strpos(file_get_contents("$d/out.txt"), "To: a@b\n") === 0);
/// var_dump(openssl_cms_sign("$d/in.txt", "$d/out.der", $x, $k, null, OPENSSL_CMS_DETACHED, OPENSSL_ENCODING_DER));
/// var_dump(openssl_cms_sign("$d/in.txt", "$d/out.txt", $x, $k, null, OPENSSL_CMS_DETACHED));
/// foreach (['in.txt', 'out.txt', 'out.der'] as $f) unlink("$d/$f");
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
///
/// Warning: openssl_cms_sign(): Detached signatures not possible with S/MIME encoding in %s on line 7
/// bool(false)
/// ```
#[php_function]
fn openssl_cms_sign(
    req: &mut Request,
    input_filename: Path,
    output_filename: Path,
    certificate: CertParam,
    #[php(sensitive)] private_key: Untyped<Value>,
    headers: Option<PhpArray>,
    #[php(default = 0)] flags: i64,
    #[php(default = OPENSSL_ENCODING_SMIME)] encoding: i64,
    #[php(default = null)] untrusted_certificates_filename: Option<Path>,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let Some((others, key, cert)) = signing(
            req,
            untrusted_certificates_filename,
            8,
            &private_key.0,
            &certificate,
        )?
        else {
            return Ok(false);
        };
        if encoding & OPENSSL_ENCODING_SMIME != 0 && flags & CMS_DETACHED != 0 {
            docref(
                req,
                Level::Warning,
                "Detached signatures not possible with S/MIME encoding",
            );
            return Ok(false);
        }
        let Some((input, out)) = signing_files(req, input_filename, output_filename) else {
            return Ok(false);
        };
        let unsigned = u32::from_ne_bytes(as_int(flags).to_ne_bytes());
        let Some(mut cms) = Cms::sign(&cert, &key, others.as_ref(), &input, unsigned) else {
            store_errors(req);
            docref(req, Level::Warning, "Error creating CMS structure!");
            return Ok(false);
        };
        if !input.reset() {
            store_errors(req);
            return Ok(false);
        }
        if encoding == OPENSSL_ENCODING_SMIME && !write_headers(req, &out, headers)? {
            return Ok(false);
        }
        Ok(write(req, &mut cms, &out, &input, flags, encoding))
    })
}

/// `openssl_cms_decrypt(string $input_filename, string $output_filename,`
/// `$certificate, $private_key = null, int $encoding =`
/// `OPENSSL_ENCODING_SMIME): bool`: a message decrypted with a
/// recipient's certificate and key.
///
/// ```php
/// $k = openssl_pkey_new(['private_key_bits' => 1024]);
/// $x = openssl_csr_sign(openssl_csr_new(['commonName' => 'a'], $k), null, $k, 1);
/// $d = __DIR__; file_put_contents("$d/in.txt", "hello\n");
/// openssl_cms_encrypt("$d/in.txt", "$d/enc.txt", [$x], null);
/// var_dump(openssl_cms_decrypt("$d/enc.txt", "$d/dec.txt", $x, $k), file_get_contents("$d/dec.txt"));
/// try { openssl_cms_decrypt("$d/enc.txt", "$d/dec.txt", $x, $k, 9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(openssl_cms_decrypt("$d/in.txt", "$d/dec.txt", $x, $k), openssl_cms_decrypt("$d/enc.txt", "$d/dec.txt", 'nope'));
/// foreach (['in.txt', 'enc.txt', 'dec.txt'] as $f) unlink("$d/$f");
/// ```
/// ```expect
/// bool(true)
/// string(7) "hello
/// "
/// openssl_cms_decrypt(): Argument #5 ($encoding) must be an OPENSSL_ENCODING_* constant
///
/// Warning: openssl_cms_decrypt(): X.509 Certificate cannot be retrieved in %s on line 8
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn openssl_cms_decrypt(
    req: &mut Request,
    input_filename: Path,
    output_filename: Path,
    #[php(sensitive)] certificate: Untyped<Value>,
    #[php(sensitive, default = null)] private_key: Untyped<Value>,
    #[php(default = OPENSSL_ENCODING_SMIME)] encoding: i64,
) -> Result<bool, Thrown> {
    finish(req, move |req| {
        let key = Some(private_key.0).filter(|v| !matches!(v.as_ref(), ValueRef::Null));
        let position = if key.is_some() { 4 } else { 3 };
        let Some((cert, key)) = decrypting(req, &certificate.0, key.as_ref(), position)? else {
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
        let cms = match Encoding::of(encoding) {
            Some(Encoding::Der) => Cms::read_der(&input),
            Some(Encoding::Pem) => Cms::read_pem(&input),
            Some(Encoding::Smime) => Cms::read_smime(&input).map(|(cms, _)| cms),
            None => return Err(arg_value_error(req, 5, "be an OPENSSL_ENCODING_* constant")),
        };
        let Some(cms) = cms else {
            store_errors(req);
            return Ok(false);
        };
        if cms.decrypt(&key, &cert, &out) {
            return Ok(true);
        }
        store_errors(req);
        Ok(false)
    })
}
