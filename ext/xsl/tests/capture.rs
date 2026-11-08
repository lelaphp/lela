//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

mod support;

use php_ext::test::Capture;

#[test]
fn the_extension_is_loaded() {
    let engine = support::engine();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('xsl'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

/// A program of `prelude`'s declarations, a one-template stylesheet imported into `$p`
/// and `<i/>` loaded into `$in`, then `body`, which calls
/// `transformToDoc()`, and `end`.
fn transform_to_doc(prelude: &str, body: &str) -> php_ext::test::Served {
    let engine = support::engine();
    let source = format!(
        r#"<?php
{prelude}
$xsl = new DOMDocument;
$xsl->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><o/></xsl:template></xsl:stylesheet>');
$p = new XSLTProcessor;
$p->importStylesheet($xsl);
$in = new DOMDocument;
$in->loadXML('<i/>');
{body}
echo "end\n";
"#
    );
    engine.serve_capture(&source, Capture::default())
}

/// A `$returnClass` whose default property names a class that is not
/// there, and an abstract one: the engine throws the `Error` the
/// initialisation raised and goes on (#3912).
#[test]
fn a_return_class_that_cannot_be_instantiated_throws() {
    let call = r#"try {
    var_dump($p->transformToDoc($in, MyDoc::class));
} catch (Error $e) {
    echo get_class($e), ': ', $e->getMessage(), "\n";
}"#;
    let unresolved = transform_to_doc(
        "class MyDoc extends DOMDocument { public $x = Foo::BAR; }",
        call,
    );
    assert_eq!(
        unresolved.stdout_text(),
        "Error: Class \"Foo\" not found\nend\n"
    );
    assert_eq!(unresolved.exit_code, 0);
    let abstract_class = transform_to_doc("abstract class MyDoc extends DOMDocument {}", call);
    assert_eq!(
        abstract_class.stdout_text(),
        "Error: Cannot instantiate abstract class MyDoc\nend\n"
    );
    assert_eq!(abstract_class.exit_code, 0);
}

/// `DOMElement` as `$returnClass`: the engine accepts it, wraps the new
/// document in it, and reads an empty string for its `tagName` (#3918).
#[test]
fn a_non_document_return_class_reads_empty() {
    let served = transform_to_doc(
        "",
        r#"try {
    $r = $p->transformToDoc($in, DOMElement::class);
    var_dump(get_class($r));
    var_dump($r->nodeType);
    var_dump($r->tagName);
} catch (TypeError $e) {
    echo get_class($e), ': ', $e->getMessage(), "\n";
}"#,
    );
    assert_eq!(
        served.stdout_text(),
        "string(10) \"DOMElement\"\nint(9)\nstring(0) \"\"\nend\n"
    );
    assert_eq!(served.exit_code, 0);
}
