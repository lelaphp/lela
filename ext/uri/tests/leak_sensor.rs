//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_lexbor::extension(), php_ext_uri::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// A `UrlValidationError`'s `type` is the one owner of its case that the
/// property takes, whether the constructor writes it or the parser does
/// (#1997): the case `IntoReturn` answers is already an owner.
#[test]
fn a_validation_error_s_type_is_held_once() {
    let stdout = armed(
        r#"<?php
$e = new Uri\WhatWg\UrlValidationError("ctx", Uri\WhatWg\UrlValidationErrorType::DomainToAscii, true);
echo $e->type->name, "\n";
Uri\WhatWg\Url::parse("https://exa mple.com", null, $errors);
echo $errors[0]->type->name, "\n";
"#,
    );
    assert_eq!(stdout, "DomainToAscii\nDomainInvalidCodePoint\n");
}
