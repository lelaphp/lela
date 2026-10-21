//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

mod support;

use php_ext::test::Capture;

/// `src` served by an armed engine with this extension alone, what it
/// wrote held to `expected`.
fn served(src: &str, expected: &str) {
    let engine = support::engine();
    let served = engine.serve_capture(src, Capture::default());
    served.assert_clean(expected);
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
/// as php's `zval_ptr_dtor(&callback_retval)` follows every arm (#2094),
/// and the callbacks a `registerPhpFunctions` array names before a
/// refused one stay in the table and are given up with the evaluator
/// (#2128).
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

/// `registerPhpFunctions()` with no argument after a set keeps the
/// callbacks the set held, which the evaluator gives up when it goes
/// (`ext/dom/tests/DOMXPath_callables.phpt`, #2128).
#[test]
fn registering_every_function_keeps_the_set() {
    served(
        r#"<?php
$doc = new DOMDocument(); $doc->loadXML('<a href="x">hello</a>');
$xpath = new DOMXPath($doc);
$xpath->registerPhpFunctions(["f" => fn($s) => $s]);
$xpath->registerPhpFunctions(null);
echo "end\n";
"#,
        "end\n",
    );
}

/// An element read as a temporary and the `children` collection it
/// caches name each other; the collection is offered to the collector
/// where the element's only other owner is that temporary, as php's
/// `dom_parent_node_children_read` does, so the cycle a discarded read
/// leaves is collected (#2241).
#[test]
fn a_discarded_children_read_is_collected() {
    served(
        "<?php $d = Dom\\XMLDocument::createFromString('<r><a/></r>');\n\
         $d->documentElement->children;\n\
         try { clone $d->documentElement->children; } catch (Error $e) { echo $e->getMessage(), \"\\n\"; }\n\
         echo (clone $d->documentElement)->children->length, \"\\n\";\n",
        "Trying to clone an uncloneable object of class Dom\\HTMLCollection\n1\n",
    );
}

/// A function an evaluation calls evaluates on the same evaluator and
/// registers on it, the evaluator staying on the object (#2318).
#[test]
fn an_xpath_callback_evaluates_and_registers_on_its_own_evaluator() {
    served(
        "<?php $d = new DOMDocument; $d->loadXML('<r><a>1</a><b>2</b></r>');\n\
         $x = new DOMXPath($d);\n\
         function inner($n) { global $x; var_dump($x->evaluate('count(//b)'), $x->registerNamespace('p', 'urn:p')); $x->registerPhpFunctions(['strtoupper']); return 'v'; }\n\
         $x->registerNamespace('php', 'http://php.net/xpath'); $x->registerPhpFunctions(['inner']);\n\
         var_dump($x->evaluate('string(php:function(\"inner\", //a))'), $x->evaluate('count(//p:z)'), $x->evaluate('php:function(\"strtoupper\", \"x\")'));\n",
        "float(1)\nbool(true)\nstring(1) \"v\"\nfloat(0)\nstring(1) \"X\"\n",
    );
}

/// `DOMXPath::__construct` from inside a function its own evaluation
/// calls: the evaluation runs to its end on the evaluator it started with,
/// which its hold keeps until then, and the object evaluates on the new
/// one after. php frees the `xmlXPathContext` libxml is evaluating with
/// (`xmlXPathFreeContext` in `dom_xpath_construct`) and reads it freed,
/// answering `function count not found` and `false`; the engine's answer
/// is the one an evaluator still open gives
/// (`trace:dom-xpath-constructed-in-callback` in `docs/divergence.toml`,
/// #2318, #2511).
#[test]
fn an_evaluator_constructed_anew_in_its_own_callback_evaluates_to_its_end() {
    served(
        "<?php $d = new DOMDocument; $d->loadXML('<r><a>1</a><b>2</b><b>3</b></r>');\n\
         $e = new DOMDocument; $e->loadXML('<s><b>9</b></s>');\n\
         $x = new DOMXPath($d);\n\
         function inner($n) { global $x, $e; $x->__construct($e); return 'v'; }\n\
         $x->registerNamespace('php', 'http://php.net/xpath'); $x->registerPhpFunctions(['inner']);\n\
         var_dump($x->evaluate('concat(php:function(\"inner\", //a), count(//b))'), $x->evaluate('count(//b)'));\n",
        "string(2) \"v2\"\nfloat(1)\n",
    );
}

/// lexbor allocates on the request's heap under its lend (ADR-0014,
/// #2925), and everything it made is given back by the request's end: an
/// HTML5 parse, a fragment, a selector list, and the request's URL pool,
/// parser and IDNA converter with every URL in it. One armed engine
/// serves many such requests, as a long-running host does, each ending
/// with no block of its heap held and each starting with the usage the
/// first started with, so nothing of one request's lexbor memory is
/// carried into the next; one ended by the memory limit mid-parse in
/// between leaves the next as it found it.
#[test]
fn what_lexbor_made_is_given_back_by_each_request_s_end() {
    let engine = support::engine();
    let src = "<?php $m = memory_get_usage();\n\
         $d = Dom\\HTMLDocument::createFromString(str_repeat('<div class=a><p>x <b>y</b></p></div>', 300), LIBXML_NOERROR);\n\
         $d->body->firstChild->innerHTML = '<i>z</i><template><u>t</u></template>';\n\
         echo count($d->querySelectorAll('div.a > p b')), ' ', $d->body->firstChild->innerHTML, \"\\n\";\n\
         $u = Uri\\WhatWg\\Url::parse('https://xn--mnchen-3ya.de:99999', null, $e);\n\
         $u = new Uri\\WhatWg\\Url('https://user@example.com/a?q#f');\n\
         $v = $u->withHost('xn--mnchen-3ya.de')->withPort(81)->withPath('/b c');\n\
         echo $v->toUnicodeString(), ' ', $v->getUnicodeHost(), ' ', count($e), \"\\n\";\n\
         echo $m, \"\\n\";\n";
    let mut first = None;
    for round in 0..64 {
        if round == 32 {
            let limited = engine.serve_capture(
                "<?php ini_set('memory_limit', '8M');\n\
                 Dom\\HTMLDocument::createFromString(str_repeat('<div class=a><p>hello <b>world</b></p></div>', 20000), LIBXML_NOERROR);\n",
                Capture::default(),
            );
            let fatal = limited.fatal.unwrap_or_default();
            assert!(fatal.contains("Allowed memory size"), "{fatal}");
        }
        let served = engine.serve_capture(src, Capture::default());
        assert_eq!(served.fatal, None, "round {round}");
        let stdout = served.stdout_text();
        let (shown, start) = stdout
            .rsplit_once('\n')
            .map_or((stdout.as_str(), ""), |(a, _)| {
                a.rsplit_once('\n').unwrap_or(("", a))
            });
        assert_eq!(
            shown,
            "299 <i>z</i><template><u>t</u></template>\n\
             https://user@münchen.de:81/b%20c?q#f münchen.de 1",
            "round {round}"
        );
        assert_eq!(
            *first.get_or_insert_with(|| start.to_owned()),
            start,
            "round {round}"
        );
    }
}
