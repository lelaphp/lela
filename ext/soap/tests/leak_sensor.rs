//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

mod support;

use php_ext::test::Capture;

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = support::engine();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
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

/// A classmapped struct whose element names a property that holds an
/// array the class's default table shares appends to a copy the
/// property takes, `SEPARATE_ARRAY(prop)`, and the default keeps its
/// own: the class data's release at the request's end frees it once
/// (#2129, `ext/soap/tests/bugs/segfault_assertion_props.phpt`).
#[test]
fn a_classmapped_default_array_is_separated_once() {
    let stdout = armed(
        r#"<?php
class C extends SoapClient {
    function __doRequest($request, $location, $action, $version, $one_way = false, ?string $uriParserClass = null): string {
        return '<?xml version="1.0"?><E:Envelope xmlns:E="http://schemas.xmlsoap.org/soap/envelope/" xmlns:x="http://www.w3.org/2001/XMLSchema-instance" xmlns:enc="http://schemas.xmlsoap.org/soap/encoding/"><E:Body><r><res x:type="enc:Struct"><a>Hello</a></res></r></E:Body></E:Envelope>';
    }
}
trait A { public $a = [self::class . 'a']; }
class S { use A; }
$c = new C(null, ['location' => 'l', 'uri' => 'u', 'classmap' => ['Struct' => 'S']]);
$a = $c->f()->a; echo $a[0], ',', $a[1], "\n";
"#,
    );
    assert_eq!(stdout, "Sa,Hello\n");
}

/// A header the server refuses after it read another, here a
/// `mustUnderstand` that is no boolean, ends the call, and what the
/// headers read before it hold is given up (`soap12/T23.phpt`,
/// `soap12/T56.phpt`).
#[test]
fn a_refused_header_gives_up_the_headers_read_before_it() {
    let stdout = armed(
        r#"<?php
class S { function echoOk($x) { return $x; } }
$s = new SoapServer(null, ['uri' => 'http://example.org/ts-tests', 'soap_version' => SOAP_1_2]);
$s->setClass('S');
$s->handle('<?xml version="1.0"?><env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope"><env:Header><test:Unknown xmlns:test="http://example.org/ts-tests" env:mustUnderstand="1">foo</test:Unknown><test:echoOk xmlns:test="http://example.org/ts-tests" env:mustUnderstand="wrong">foo</test:echoOk></env:Header><env:Body><test:echoOk xmlns:test="http://example.org/ts-tests"><x>bar</x></test:echoOk></env:Body></env:Envelope>');
"#,
    );
    assert!(
        stdout.ends_with("mustUnderstand value is not boolean</env:Text></env:Reason></env:Fault></env:Body></env:Envelope>\n"),
        "{stdout}"
    );
}
