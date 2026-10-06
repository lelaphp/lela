//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_openssl::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// A scratch directory of this process's own, by its path.
fn scratch(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("openssl-leak-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    dir.to_string_lossy().into_owned()
}

/// What an array hands the extension as text, a CA path of `ca_info`, a
/// subject entry, an extra attribute and a PKCS#7 header, is converted
/// with `zval_try_get_string` and given up once read,
/// `zend_string_release` (#2093). Each value here is an integer, so each
/// conversion makes a string of the request's.
#[test]
fn the_strings_an_array_s_values_convert_to_are_given_up() {
    let dir = scratch("convert");
    std::fs::write(format!("{dir}/in.txt"), "hello\n").ok();
    let stdout = armed(&format!(
        r"<?php
$k = openssl_pkey_new(['private_key_bits' => 1024]);
$x = openssl_csr_sign(openssl_csr_new(['commonName' => 12345], $k), null, $k, 1);
var_dump(@openssl_x509_checkpurpose($x, X509_PURPOSE_ANY, [77]));
var_dump(@openssl_csr_new(['commonName' => 'a'], $k, null, ['unstructuredName' => 1234]) !== false);
var_dump(openssl_pkcs7_sign('{dir}/in.txt', '{dir}/out.txt', $x, $k, ['X' => 42]));
"
    ));
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(stdout, "bool(false)\nbool(false)\nbool(true)\n");
}
