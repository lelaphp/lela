//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

mod support;

use php_ext::test::Capture;

#[test]
fn the_extension_is_loaded() {
    let engine = support::engine();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('dom'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

/// A node of a node set made over a value the callbacks keep: the
/// evaluation holds the node's document while the node's object is made,
/// and the node counts it, so the node outlives the kept value.
#[test]
fn a_kept_parent_outlives_an_autoloader_that_constructs_the_evaluator_anew() {
    let engine = support::engine_with_spl();
    let sink = Capture::default();
    let served = engine.serve_capture(
        r#"<?php
class MyText extends DOMText { public $x = Foo::BAR; }
$xp = new DOMXPath(new DOMDocument);
$xp->registerNamespace("my", "my.ns");
$xp->registerPHPFunctionNS('my.ns', 'include', function () {
    $t = new DOMDocument;
    $t->registerNodeClass('DOMText', 'MyText');
    $t->loadXML('<root>a<x/>b</root>');
    return $t->documentElement;
});
spl_autoload_register(function ($c) {
    echo "autoload $c\n";
    $GLOBALS['xp']->__construct(new DOMDocument);
    class Foo { const BAR = 7; }
});
$l = $xp->query('my:include()/text()[1]');
foreach ($l as $a) echo get_class($a), ' ', $a->nodeValue, ' ', $a->x, ' ', $a->ownerDocument->saveXML($a->parentNode), "\n";
unset($a, $l);
echo "end\n";
"#,
        sink,
    );
    assert_eq!(
        served.stdout_text(),
        "autoload Foo\nMyText a 7 <root>a<x/>b</root>\nend\n"
    );
}

/// Each node of a node set is made counting its own node's document,
/// held by the evaluation. The class comes from the evaluator's re-read,
/// so the nodes made after the evaluator is constructed anew are
/// `DOMElement`; a parent made over one of them is mapped by `$d`, which
/// it counts, so it is `MyEl`.
#[test]
fn each_node_counts_its_own_document_after_an_autoloader_constructs_the_evaluator_anew() {
    let engine = support::engine_with_spl();
    let sink = Capture::default();
    let served = engine.serve_capture(
        r#"<?php
class MyEl extends DOMElement { public $x = Foo::BAR; }
$d = new DOMDocument;
$d->loadXML('<r><a id="1"/><a id="2"/><a id="3"/></r>');
$d->registerNodeClass('DOMElement', 'MyEl');
$xp = new DOMXPath($d);
$xp->registerNamespace('php', 'http://php.net/xpath');
$xp->registerPhpFunctions();
spl_autoload_register(function ($c) {
    echo "autoload $c\n";
    $GLOBALS['xp']->__construct(new DOMDocument);
    class Foo { const BAR = 7; }
});
$l = $xp->query('//a[php:function("is_array", @id)]');
foreach ($l as $a) echo get_class($a), ' ', $a->getAttribute('id'), ' ', get_class($a->parentNode), "\n";
"#,
        sink,
    );
    assert_eq!(
        served.stdout_text(),
        "autoload Foo\nMyEl 1 MyEl\nDOMElement 2 MyEl\nDOMElement 3 MyEl\n"
    );
    assert_eq!(served.exit_code, 0);
}

/// The node set of a document only the value a php function answered
/// holds: the evaluation holds the document, so each node is made over
/// it and counts it.
#[test]
fn a_document_only_a_kept_value_holds_outlives_an_autoloader_that_constructs_the_evaluator_anew() {
    let engine = support::engine_with_spl();
    let sink = Capture::default();
    let served = engine.serve_capture(
        r#"<?php
class MyText extends DOMText { public $x = Foo::BAR; }
$xp = new DOMXPath(new DOMDocument);
$xp->registerNamespace("my", "my.ns");
$xp->registerPHPFunctionNS('my.ns', 'include', function () {
    $t = new DOMDocument;
    $t->registerNodeClass('DOMText', 'MyText');
    $t->loadXML('<root>a<x/>b<x/>c</root>');
    return $t->documentElement;
});
spl_autoload_register(function ($c) {
    echo "autoload $c\n";
    $GLOBALS['xp']->__construct(new DOMDocument);
    class Foo { const BAR = 7; }
});
$l = $xp->query('my:include()/text()');
foreach ($l as $a) echo get_class($a), ' ', $a->nodeValue, ' ', $a->ownerDocument->saveXML($a->parentNode), "\n";
echo "end\n";
"#,
        sink,
    );
    assert_eq!(
        served.stdout_text(),
        "autoload Foo\nMyText a <root>a<x/>b<x/>c</root>\nDOMText b <root>a<x/>b<x/>c</root>\nDOMText c <root>a<x/>b<x/>c</root>\nend\n"
    );
    assert_eq!(served.exit_code, 0);
}

/// Serves a `wr://` wrapper whose three methods are given, as
/// `Dom\XMLDocument::C14NFile()`'s target, then `call`.
fn c14nfile_through_a_wrapper(methods: &str, call: &str) -> php_ext::test::Served {
    let engine = support::engine();
    let sink = Capture::default();
    let source = format!(
        r#"<?php
class W {{
    public $context;
    static ?Dom\XMLDocument $doc = null;
{methods}
}}
stream_wrapper_register('wr', 'W');
W::$doc = Dom\XMLDocument::createFromString('<root xmlns="urn:a" xmlns:p="urn:p" p:x="1"><c/></root>');
{call}
"#
    );
    engine.serve_capture(&source, sink)
}

/// A wrapper's methods see the tree with its `xmlns` attributes put back,
/// since the file is written once the tree is restored; php 8.5.12 writes
/// while the tree is relinked and its wrapper reads 1 attribute each
/// time, then 1 after the call, where this engine's reads 3 throughout
/// (#3961, the difference #3959 proposes).
#[test]
fn a_c14nfile_wrapper_sees_the_restored_tree() {
    let served = c14nfile_through_a_wrapper(
        r#"    function stream_open($p, $m, $o, &$op) { echo "open: ", self::$doc->documentElement->attributes->length, "\n"; return true; }
    function stream_write($d) { echo "write: ", strlen($d), " bytes, attributes ", self::$doc->documentElement->attributes->length, "\n"; return strlen($d); }
    function stream_close() { echo "close: ", self::$doc->documentElement->attributes->length, "\n"; }"#,
        r#"var_dump(W::$doc->C14NFile('wr://x'));
echo "after: ", W::$doc->documentElement->attributes->length, "\n";"#,
    );
    assert_eq!(
        served.stdout_text(),
        "open: 3\nwrite: 58 bytes, attributes 3\nclose: 3\nint(58)\nafter: 3\n"
    );
    assert_eq!(served.exit_code, 0);
}

/// A wrapper that removes and frees the root element in `stream_close`:
/// the tree was restored before the file opened, so nothing reads the
/// freed element; php 8.5.12 prints the same and exits 0 (#3961).
#[test]
fn a_c14nfile_wrapper_frees_the_root_in_stream_close() {
    let served = c14nfile_through_a_wrapper(
        r#"    function stream_open($p, $m, $o, &$op) { return true; }
    function stream_write($d) { return strlen($d); }
    function stream_close() { self::$doc->documentElement->remove(); gc_collect_cycles(); echo "removed\n"; }"#,
        r#"var_dump(W::$doc->C14NFile('wr://x'));
echo W::$doc->saveXml(), "\n";"#,
    );
    assert_eq!(
        served.stdout_text(),
        "removed\nint(58)\n<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\n"
    );
    assert_eq!(served.exit_code, 0);
}

/// A wrapper that frees the root element in `stream_write` and throws:
/// the file is written once the tree is restored, so the program catches
/// what was thrown and the document is left emptied.
#[test]
fn a_c14nfile_wrapper_frees_the_root_in_stream_write_and_throws() {
    let served = c14nfile_through_a_wrapper(
        r#"    function stream_open($p, $m, $o, &$op) { return true; }
    function stream_write($d) { self::$doc->documentElement->remove(); throw new Exception("removed in write"); }
    function stream_close() {}"#,
        r#"try {
    var_dump(W::$doc->C14NFile('wr://x'));
} catch (Exception $e) {
    echo get_class($e), ": ", $e->getMessage(), "\n";
}
echo W::$doc->saveXml(), "\n";"#,
    );
    assert_eq!(
        served.stdout_text(),
        "Exception: removed in write\n<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\n"
    );
    assert_eq!(served.exit_code, 0);
}

/// A wrapper that frees the root element in `stream_write` and writes
/// nothing: the one write the closing flush makes answers 0, so the call
/// answers `int(0)`; php 8.5.12 flushes twice, and its second
/// `stream_write` throws an `Error` (#3961).
#[test]
fn a_c14nfile_wrapper_frees_the_root_in_stream_write_and_writes_nothing() {
    let served = c14nfile_through_a_wrapper(
        r#"    function stream_open($p, $m, $o, &$op) { return true; }
    function stream_write($d) { self::$doc->documentElement->remove(); echo "removed in write\n"; return 0; }
    function stream_close() {}"#,
        r#"var_dump(W::$doc->C14NFile('wr://x'));
echo W::$doc->saveXml(), "\n";"#,
    );
    assert_eq!(
        served.stdout_text(),
        "removed in write\nint(0)\n<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\n"
    );
    assert_eq!(served.exit_code, 0);
}

/// Serves #3962's `handler-*.php`: an error handler that removes the root
/// element and collects while `call` canonicalizes a `Dom\XMLDocument`,
/// then the document as it stands.
fn c14n_under_a_handler_that_frees_the_root(call: &str) -> php_ext::test::Served {
    let engine = support::engine();
    let sink = Capture::default();
    let source = format!(
        r#"<?php
$doc = Dom\XMLDocument::createFromString('<root xmlns="urn:a" xmlns:p="urn:p" p:x="1"><c/></root>');
set_error_handler(function ($no, $str) use ($doc) {{ $doc->documentElement?->remove(); gc_collect_cycles(); echo "handler: $str\n"; return true; }});
{call}
echo $doc->saveXml(), "\n";
"#
    );
    engine.serve_capture(&source, sink)
}

/// The inclusive-prefixes notice is raised once the tree is put back, so
/// the handler that frees the root frees an element nothing writes into
/// after it: the handler's line, the canonical form of the whole tree,
/// then the emptied document.
#[test]
fn a_handler_that_frees_the_root_at_c14ns_notice_frees_a_restored_tree() {
    let served = c14n_under_a_handler_that_frees_the_root(
        "var_dump($doc->C14N(false, false, null, ['p']));",
    );
    assert_eq!(
        served.stdout_text(),
        "handler: Dom\\Node::C14N(): Inclusive namespace prefixes only allowed in exclusive mode.\n\
         string(58) \"<root xmlns=\"urn:a\" xmlns:p=\"urn:p\" p:x=\"1\"><c></c></root>\"\n\
         <?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\n"
    );
    assert_eq!(served.exit_code, 0);
}

/// The query's libxml error is raised once the tree is put back, then the
/// `Error` for what is no node set: the handler's line, the `Error`, the
/// emptied document.
#[test]
fn a_handler_that_frees_the_root_at_c14ns_query_error_frees_a_restored_tree() {
    let served = c14n_under_a_handler_that_frees_the_root(
        r#"try { var_dump($doc->C14N(false, false, ['query' => '//*['])); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }"#,
    );
    assert_eq!(
        served.stdout_text(),
        "handler: Dom\\Node::C14N(): Invalid expression\n\
         Error: XPath query did not return a nodeset\n\
         <?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\n"
    );
    assert_eq!(served.exit_code, 0);
}

/// A class `registerNodeClass()` maps elements to, whose default property
/// names a class that is not there: the engine throws the `Error` the
/// evaluation raised and goes on (#3912).
#[test]
fn a_registered_class_whose_default_cannot_be_evaluated_throws() {
    let engine = support::engine();
    let served = engine.serve_capture(
        r#"<?php
class MyEl extends DOMElement { public $x = Foo::BAR; }
$d = new DOMDocument;
$d->loadXML('<r/>');
$d->registerNodeClass('DOMElement', 'MyEl');
try {
    var_dump($d->documentElement);
} catch (Error $e) {
    echo get_class($e), ': ', $e->getMessage(), "\n";
}
echo "end\n";
"#,
        Capture::default(),
    );
    assert_eq!(
        served.stdout_text(),
        "Error: Class \"Foo\" not found\nend\n"
    );
    assert_eq!(served.exit_code, 0);
}
