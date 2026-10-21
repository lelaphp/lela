//! The request leak sensor (#1992) over a transformation whose callbacks
//! reach the processor transforming: each request below gives up
//! everything its natives took, so an armed engine ends it without the
//! `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// `src` served by an armed engine with this extension and what it
/// needs, what it wrote held to `expected`.
fn served(src: &str, expected: &str) {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_libxml::extension(),
            php_ext_lexbor::extension(),
            php_ext_dom::extension(),
            php_ext_standard::extension(),
            php_ext_uri::extension(),
            php_ext_xsl::extension(),
        ])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(src, Capture::default());
    served.assert_clean(expected);
}

/// The stylesheet `"<xsl:stylesheet ...>"` whose one template is `body`,
/// quoted for PHP source.
fn sheet(body: &str) -> String {
    format!(
        "'<xsl:stylesheet version=\"1.0\" xmlns:xsl=\"http://www.w3.org/1999/XSL/Transform\" xmlns:php=\"http://php.net/xsl\"><xsl:output method=\"text\"/><xsl:template match=\"/\">{body}</xsl:template></xsl:stylesheet>'"
    )
}

/// A callback transforms with the processor again and registers on it:
/// the nested transformation runs with the stylesheet, and the
/// registration holds for the rest of the outer one and after (#2319).
#[test]
fn a_callback_transforms_and_registers_on_its_own_processor() {
    let one = sheet(
        "<xsl:value-of select=\"php:function(\\'inner\\', string(/r))\"/>|<xsl:value-of select=\"php:function(\\'strrev\\', string(/r))\"/>",
    );
    served(
        &format!(
            "<?php $xsl = new DOMDocument; $xsl->loadXML({one});\n\
             $doc = new DOMDocument; $doc->loadXML('<r>xy</r>');\n\
             $p = new XSLTProcessor; $p->registerPHPFunctions(['inner']); $p->importStylesheet($xsl);\n\
             $depth = 0;\n\
             function inner($s) {{ global $p, $doc, $depth; if ($depth++) return 'deep'; $p->registerPHPFunctions(['strrev']); var_dump($p->transformToXml($doc)); return \"in:$s\"; }}\n\
             var_dump($p->transformToXml($doc), $p->transformToXml($doc));\n"
        ),
        "string(7) \"deep|yx\"\nstring(8) \"in:xy|yx\"\nstring(7) \"deep|yx\"\n",
    );
}

/// `importStylesheet()` from inside a callback of the transformation
/// using the stylesheet it replaces: the transformation runs to its end
/// with the old one, held until then, and the next uses the new one.
/// php frees the stylesheet libxslt is transforming with
/// (`xsltFreeStylesheet` in `XSLTProcessor::importStylesheet`) and dies
/// on SIGSEGV after the nested transformation's `nested: 'deep'`
/// (`trace:xsl-stylesheet-replaced-in-callback` in
/// `docs/divergence.toml`, #2319, #2511).
#[test]
fn a_stylesheet_replaced_in_its_own_callback_transforms_to_its_end() {
    let one = sheet("<xsl:value-of select=\"php:function(\\'inner\\', string(/r))\"/>");
    let two = sheet("two:<xsl:value-of select=\"/r\"/>");
    served(
        &format!(
            "<?php $xsl = new DOMDocument; $xsl->loadXML({one});\n\
             $xsl2 = new DOMDocument; $xsl2->loadXML({two});\n\
             $doc = new DOMDocument; $doc->loadXML('<r>x</r>');\n\
             $p = new XSLTProcessor; $p->registerPHPFunctions(); $p->importStylesheet($xsl);\n\
             $depth = 0;\n\
             function inner($s) {{ global $p, $doc, $xsl2, $depth; if ($depth++) return 'deep'; $r = $p->transformToXml($doc); echo 'nested: ', var_export($r, true), \"\\n\"; $p->importStylesheet($xsl2); return \"in:$s\"; }}\n\
             var_dump($p->transformToXml($doc), $p->transformToXml($doc));\n"
        ),
        "nested: 'deep'\nstring(4) \"in:x\"\nstring(5) \"two:x\"\n",
    );
}
