//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, Files, LeakSensor};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_openssl::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// What an array hands the extension as text, a CA path of `ca_info`, a
/// subject entry, an extra attribute and a PKCS#7 header, is converted
/// with `zval_try_get_string` and given up once read,
/// `zend_string_release` (#2093). Each value here is an integer, so each
/// conversion makes a string of the request's.
#[test]
fn the_strings_an_array_s_values_convert_to_are_given_up() {
    let files = Files::new("openssl-convert", &[("in.txt", "hello\n")]);
    let dir = files.path().display();
    let stdout = armed(&format!(
        r"<?php
$k = openssl_pkey_new(['private_key_bits' => 1024]);
$x = openssl_csr_sign(openssl_csr_new(['commonName' => 12345], $k), null, $k, 1);
var_dump(@openssl_x509_checkpurpose($x, X509_PURPOSE_ANY, [77]));
var_dump(@openssl_csr_new(['commonName' => 'a'], $k, null, ['unstructuredName' => 1234]) !== false);
var_dump(openssl_pkcs7_sign('{dir}/in.txt', '{dir}/out.txt', $x, $k, ['X' => 42]));
"
    ));
    assert_eq!(stdout, "bool(false)\nbool(false)\nbool(true)\n");
}

/// A native whose body made its answer while a deferred `ValueError`
/// stands answers the error, and what the body made is given up, as the
/// VM's `zval_ptr_dtor` of the return value frees it: here the
/// `OpenSSLCertificateSigningRequest` `openssl_csr_new` makes under a
/// refused key path (#2126, `ext/openssl/tests/bug81713.phpt`).
#[test]
fn an_answer_made_under_a_deferred_error_is_given_up() {
    let stdout = armed(
        r#"<?php
$k = "file:///nonexistent\0foo";
try { var_dump(openssl_csr_new(['commonName' => 'a'], $k)); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
"#,
    );
    assert_eq!(
        stdout,
        "openssl_csr_new(): Argument #2 ($private_key) must not contain any null bytes\n"
    );
}
