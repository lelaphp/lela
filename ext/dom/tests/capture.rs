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
