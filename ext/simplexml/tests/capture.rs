//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

mod support;

use php_ext::test::Capture;

#[test]
fn the_extension_is_loaded() {
    let engine = support::engine();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('simplexml'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

/// `current()`, `key()` and the movers over an element read the table
/// the `(array)` cast builds of its children, as php reads the one
/// `sxe_get_properties` keeps on the object, and give it up once read:
/// armed, the request ends with no block of its heap held.
#[cfg(debug_assertions)]
#[test]
fn the_pointer_natives_over_an_element_give_up_the_table_they_read() {
    let engine = support::engine();
    let served = engine.serve_capture(
        r#"<?php $x = simplexml_load_string('<a><b>1</b><c>2</c></a>');
echo @current($x), @key($x), @next($x), @reset($x), @end($x), @prev($x), "\n";"#,
        Capture::default(),
    );
    served.assert_clean("1b212\n");
}

/// A tree libxml made past a throw, a program's entity loader's as
/// libxml loaded a DTD through it, is the object's as any other, as
/// php's is: the object counts it and frees it with itself, so no tree
/// is left uncounted. `simplexml_load_string` gives its object up with
/// the throw pending, the destructor reading the tree's root, and a
/// constructor called again stands for the new tree. The
/// request leak sensor does not see libxml2's allocations; the
/// destructor's read and the name are what count the tree.
#[test]
fn a_tree_made_past_a_throw_is_counted_by_its_object() {
    let engine = support::engine();
    let served = engine.serve_capture(
        r#"<?php
libxml_set_external_entity_loader(function () { throw new Exception('loader'); });
class X extends SimpleXMLElement { function __destruct() { echo 'destruct ', $this->getName(), "\n"; } }
$xml = '<!DOCTYPE e SYSTEM "e.dtd"><e/>';
try { simplexml_load_string($xml, 'X', LIBXML_DTDLOAD); } catch (Exception $e) { echo 'caught ', $e->getMessage(), "\n"; }
$s = new SimpleXMLElement('<old/>');
try { $s->__construct($xml, LIBXML_DTDLOAD); } catch (Exception $e) { echo 'caught ', $e->getMessage(), "\n"; }
echo $s->getName(), "\n";"#,
        Capture::default(),
    );
    assert_eq!(
        served.stdout_text(),
        "destruct e\ncaught loader\ncaught loader\ne\n"
    );
}
