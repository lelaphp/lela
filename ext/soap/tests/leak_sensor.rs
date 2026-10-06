//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_soap::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// A fault the client raises while it goes on is kept in its
/// `__soap_fault` alone, `add_soap_fault`'s `zval_ptr_dtor(&fault)`
/// after `add_soap_fault_ex` copied it there: a `__doRequest` that
/// answers no string (#2100).
#[test]
fn a_fault_the_client_keeps_is_kept_once() {
    let stdout = armed(
        r#"<?php
class C extends SoapClient { function __doRequest($r, $l, $a, $v, $o = false, $u = null): ?string { return null; } }
$c = new C(null, ['location' => 'test://', 'uri' => 'urn:x', 'exceptions' => false]);
echo $c->hello(1, 'two')->faultstring, "\n";
"#,
    );
    assert_eq!(
        stdout,
        "SoapClient::__doRequest() returned non string value\n"
    );
}
