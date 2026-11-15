//! The request leak sensor over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_hash::extension()])
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// XXH3's `secret` option is read as its string, `zval_try_get_string`,
/// and given up once its bytes are copied.
#[test]
fn an_xxh3_secret_read_is_given_up() {
    let stdout = armed(
        r#"<?php
$secret = ''; for ($i = 0; $i < 14; $i++) $secret .= '0123456789';
echo hash('xxh3', 'abc', false, ['secret' => $secret]), ' ', hash('xxh128', 'abc', false, ['secret' => $secret]), "\n";
"#,
    );
    assert_eq!(
        stdout,
        "c2316825a070030d bf3b0d0f5eb93fd5c2316825a070030d\n"
    );
}
