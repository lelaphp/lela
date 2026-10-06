//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// `src` served by an armed engine with this extension alone, what it
/// wrote held to `expected`.
fn served(src: &str, expected: &str) {
    let engine = Engine::builder()
        .extensions(&[php_ext_dom::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(src, Capture::default());
    assert_eq!(served.fatal, None);
    assert_eq!(String::from_utf8_lossy(&served.stdout), expected);
}

/// A modern document made by `Dom\Implementation` or cloned keeps the
/// one private data it is given, as php's `php_dom_instantiate_object_helper`
/// makes none of its own, and one that throws before it is made gives
/// that data up, `php_dom_private_data_destroy` (#2094).
#[test]
fn a_modern_document_keeps_one_private_data() {
    served(
        "<?php $i = Dom\\XMLDocument::createEmpty()->implementation;\n\
         echo $i->createDocument('urn:r', 'x:r')->documentElement->nodeName, \"\\n\";\n\
         echo $i->createDocument(null, '')->childNodes->length, \"\\n\";\n\
         try { $i->createDocument(null, 'x:r'); } catch (DOMException $e) { echo $e->getMessage(), \"\\n\"; }\n\
         echo $i->createHTMLDocument('t')->title, \"\\n\";\n\
         echo get_class(Dom\\XMLDocument::createFromString('<r/>')->cloneNode(true)), \"\\n\";\n",
        "x:r\n0\nNamespace Error\nt\nDom\\XMLDocument\n",
    );
}

/// `Dom\HTMLDocument::createFromFile` gives up the empty read that ends
/// the stream, as `php_stream_read` into php's own buffer makes no
/// string of it (#2094).
#[test]
fn an_html_document_from_a_file_keeps_no_read() {
    served(
        "<?php echo Dom\\HTMLDocument::createFromFile('data:,<!doctype html><p>x')->body->textContent, \"\\n\";",
        "x\n",
    );
}

/// The string an `XPath` PHP function answers, and the name an int-keyed
/// `registerPhpFunctions` entry is read off, are given up once their
/// bytes are copied, as php's `zend_tmp_string_release` gives them up
/// (#2094).
#[test]
fn an_xpath_callback_s_answer_is_given_up() {
    served(
        "<?php $d = new DOMDocument; $d->loadXML('<r><a>x</a></r>');\n\
         $x = new DOMXPath($d);\n\
         $x->registerNamespace('php', 'http://php.net/xpath');\n\
         $x->registerPhpFunctions(['up' => fn($s) => $s . '!']);\n\
         echo $x->evaluate('php:functionString(\"up\", //a)'), \"\\n\";\n\
         function gh($s) { return $s . '?'; }\n\
         $g = 'g'; $h = 'h'; $x->registerPhpFunctions([$g . $h]);\n\
         echo $x->evaluate('php:functionString(\"gh\", //a)'), \"\\n\";\n",
        "x!\nx?\n",
    );
}

/// A callback answer whose conversion throws is given up all the same,
/// as php's `zval_ptr_dtor(&callback_retval)` follows every arm, and the
/// callbacks a `registerPhpFunctions` array names before a refused one
/// are given up with the refusal (#2094).
#[test]
fn an_xpath_callback_given_up_where_its_conversion_or_registration_fails() {
    served(
        "<?php $d = new DOMDocument; $d->loadXML('<r><a>x</a></r>');\n\
         $x = new DOMXPath($d);\n\
         $x->registerNamespace('php', 'http://php.net/xpath');\n\
         $x->registerPhpFunctions(['arr' => fn($s) => [$s]]);\n\
         set_error_handler(function ($n, $m) { throw new Exception($m); });\n\
         try { $x->evaluate('php:functionString(\"arr\", //a)'); } catch (Exception $e) { echo $e->getMessage(), \"\\n\"; }\n\
         restore_error_handler();\n\
         try { $x->registerPhpFunctions(['f' => fn() => 1, 'g' => 'no_such_function']); } catch (TypeError $e) { echo get_class($e), \"\\n\"; }\n",
        "Array to string conversion\nTypeError\n",
    );
}
