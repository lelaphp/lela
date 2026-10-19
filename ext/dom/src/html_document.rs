//! `Dom\Document`'s properties and `Dom\HTMLDocument` (`html_document.c`,
//! with the `Dom\Document` rows `PHP_MINIT_FUNCTION(dom)` registers):
//! `implementation`, `URL`, the encoding names, and `body`, `head` and
//! `title` as the HTML standard defines them over the tree.

use php_ext::abi::{Instance, OrFalse, Path, Raw, This};
use php_ext::attr::php_class;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString, Value};
use php_ext_libxml::ffi::{Kind, Node};

use crate::object::Code;
use crate::private::{HTML_NS, SVG_NS, ns_is};
use crate::props::Prop;
use crate::token_list::strip_and_collapse;
use crate::{Read, dom_error_message, empty_string, nullable_object, prop_node, string};

/// `dom_modern_document_implementation_read`: one `Dom\Implementation` per
/// document, kept in the declared `$implementation` slot.
///
/// ```php
/// $d = Dom\XMLDocument::createEmpty();
/// var_dump(get_class($d->implementation), $d->implementation === $d->implementation);
/// ```
/// ```expect
/// string(18) "Dom\Implementation"
/// bool(true)
/// ```
fn implementation(req: &mut Request, object: PhpObject) -> Read {
    if let Some(kept) = crate::modern::cached(req, object, b"implementation") {
        return Ok(Some(Owner::take(req, kept)));
    }
    let class = crate::class(req, "Dom\\Implementation")?;
    let made = php_ext::call::new_object(req, class)?.into_value();
    if let Err(thrown) = crate::modern::keep(req, object, b"implementation", &made) {
        made.give_up(req);
        return Err(thrown);
    }
    Ok(Some(made))
}

/// `dom_html_document_element_read_raw`: the first child of the `html`
/// document element in the HTML namespace that `accept` takes.
fn html_child(doc: Node, accept: fn(&[u8]) -> bool) -> Option<Node> {
    let root = doc.root()?;
    if !(ns_is(root, HTML_NS) && root.name() == b"html") {
        return None;
    }
    let mut at = root.children();
    while let Some(n) = at {
        if n.kind() == Kind::Element && ns_is(n, HTML_NS) && accept(&n.name()) {
            return Some(n);
        }
        at = n.next();
    }
    None
}

fn is_body(name: &[u8]) -> bool {
    name == b"body" || name == b"frameset"
}

fn is_head(name: &[u8]) -> bool {
    name == b"head"
}

/// `dom_html_document_body_read`.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<html xmlns="http://www.w3.org/1999/xhtml"><head/><body/></html>');
/// var_dump($d->body->localName, $d->head->localName);
/// $e = Dom\XMLDocument::createFromString('<r/>');
/// var_dump($e->body, $e->head);
/// ```
/// ```expect
/// string(4) "body"
/// string(4) "head"
/// NULL
/// NULL
/// ```
fn body(req: &mut Request, object: PhpObject) -> Read {
    let doc = prop_node(req, object)?;
    Ok(Some(nullable_object(
        req,
        html_child(doc, is_body),
        Some(object),
    )?))
}

/// `dom_html_document_head_read`.
fn head(req: &mut Request, object: PhpObject) -> Read {
    let doc = prop_node(req, object)?;
    Ok(Some(nullable_object(
        req,
        html_child(doc, is_head),
        Some(object),
    )?))
}

/// `dom_html_document_body_write`: the body replaced, or appended to the
/// document element, the new one adopted into this document.
fn set_body(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let doc = prop_node(req, object)?;
    let fresh = value
        .as_object()
        .and_then(|o| crate::node_of(req, *o))
        .filter(|n| is_body(&n.name()));
    let Some(fresh) = fresh else {
        return dom_error_message(
            req,
            Code::HierarchyRequest,
            "The new body must either be a body or a frameset tag",
            true,
        );
    };
    let current = html_child(doc, is_body);
    if current == Some(fresh) {
        return Ok(());
    }
    if let Some(current) = current {
        crate::adopt(req, object, fresh, doc)?;
        current.replace_with(fresh);
        if current.object().is_none() {
            current.free_resource(req);
        }
        return Ok(());
    }
    let Some(root) = doc.root() else {
        return dom_error_message(
            req,
            Code::HierarchyRequest,
            "A body can only be set if there is a document element",
            true,
        );
    };
    crate::adopt(req, object, fresh, doc)?;
    root.add_child(fresh);
    Ok(())
}

/// `dom_get_child_text_content`: the text and CDATA children's content,
/// joined.
fn child_text_content(node: Node) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = node.children();
    while let Some(n) = at {
        if matches!(n.kind(), Kind::Text | Kind::CData)
            && let Some(content) = n.content()
        {
            out.extend_from_slice(&content);
        }
        at = n.next();
    }
    out
}

/// `dom_get_title_element`: the first `title` element of the HTML
/// namespace in tree order.
fn title_element(doc: Node) -> Option<Node> {
    let mut at = doc.children();
    while let Some(n) = at {
        if n.kind() == Kind::Element && ns_is(n, HTML_NS) && n.name() == b"title" {
            return Some(n);
        }
        at = crate::next_in_tree_order_from(n, None);
    }
    None
}

/// `dom_get_svg_title_element`: the first `title` child of the SVG
/// namespace.
fn svg_title_element(svg: Node) -> Option<Node> {
    let mut at = svg.children();
    while let Some(n) = at {
        if n.kind() == Kind::Element && ns_is(n, SVG_NS) && n.name() == b"title" {
            return Some(n);
        }
        at = n.next();
    }
    None
}

/// Whether `root` is the SVG `svg` element.
fn is_svg_root(root: Node) -> bool {
    ns_is(root, SVG_NS) && root.name() == b"svg"
}

/// `dom_html_document_title_read`.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<html xmlns="http://www.w3.org/1999/xhtml"><head><title>  a
///  b  </title></head></html>');
/// var_dump($d->title);
/// $s = Dom\XMLDocument::createFromString('<svg xmlns="http://www.w3.org/2000/svg"><title>t</title></svg>');
/// var_dump($s->title);
/// ```
/// ```expect
/// string(3) "a b"
/// string(1) "t"
/// ```
fn title(req: &mut Request, object: PhpObject) -> Read {
    let doc = prop_node(req, object)?;
    let Some(root) = doc.root() else {
        return Ok(Some(empty_string(req)?));
    };
    let title = if is_svg_root(root) {
        svg_title_element(root)
    } else {
        title_element(doc)
    };
    // `dom_get_child_text_content` is a `smart_str_extract`, the
    // interned empty string where the title has no text, which
    // `dom_strip_and_collapse_ascii_whitespace` hands back as it stands,
    // and a new string it collapses in place otherwise.
    let value = title.map(child_text_content).unwrap_or_default();
    if value.is_empty() {
        return Ok(Some(empty_string(req)?));
    }
    Ok(Some(string(req, &strip_and_collapse(&value))?))
}

/// `dom_string_replace_all`.
fn replace_all(req: &mut Request, doc: Node, element: Node, value: &[u8]) {
    crate::remove_all_children(req, element);
    if let Some(text) = Node::new_text(Some(doc), value) {
        element.add_child(text);
    }
}

/// `dom_html_document_title_write`.
fn set_title(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let doc = prop_node(req, object)?;
    let Some(root) = doc.root() else {
        return Ok(());
    };
    let bytes = value.as_str_bytes().map(<[u8]>::to_vec).unwrap_or_default();
    if is_svg_root(root) {
        let element = if let Some(e) = svg_title_element(root) {
            e
        } else {
            {
                let ns = match root.ns() {
                    Some(ns) if ns.prefix().is_some() => {
                        crate::private::mapped(req, object, |mapper, req| {
                            let svg = crate::private::Handed::Made(SVG_NS);
                            mapper.get_ns(req, Some(doc), crate::private::Handed::Empty, svg)
                        })?
                    }
                    other => other,
                };
                let Some(element) = Node::new_element(Some(doc), ns, b"title", None) else {
                    return Err(crate::dom_throw(req, Code::InvalidState));
                };
                match root.children() {
                    Some(first) => {
                        first.add_prev_sibling(element);
                    }
                    None => {
                        root.add_child(element);
                    }
                }
                element
            }
        };
        replace_all(req, doc, element, &bytes);
    } else if ns_is(root, HTML_NS) {
        let title = title_element(doc);
        let head = html_child(doc, is_head);
        let element = match (title, head) {
            (Some(t), _) => t,
            (None, None) => return Ok(()),
            (None, Some(head)) => {
                let ns =
                    crate::private::mapped(req, object, |mapper, req| mapper.html(req, Some(doc)))?;
                let Some(element) = Node::new_element(Some(doc), ns, b"title", None) else {
                    return Err(crate::dom_throw(req, Code::InvalidState));
                };
                head.add_child(element);
                element
            }
        };
        replace_all(req, doc, element, &bytes);
    }
    Ok(())
}

const fn ro(name: &'static [u8], read: fn(&mut Request, PhpObject) -> Read) -> Prop {
    Prop {
        name,
        read,
        write: None,
    }
}

const fn rw(
    name: &'static [u8],
    read: fn(&mut Request, PhpObject) -> Read,
    write: fn(&mut Request, PhpObject, &Value) -> Result<(), Thrown>,
) -> Prop {
    Prop {
        name,
        read,
        write: Some(write),
    }
}

/// `dom_abstract_base_document_prop_handlers`' own rows, the node's merged
/// behind them.
pub(crate) const PROPS: &[Prop] = &[
    ro(b"implementation", implementation),
    crate::document::MODERN_URL,
    crate::document::MODERN_DOCUMENT_URI,
    rw(
        b"characterSet",
        crate::document::encoding,
        set_character_set,
    ),
    rw(b"charset", crate::document::encoding, set_character_set),
    rw(
        b"inputEncoding",
        crate::document::encoding,
        set_character_set,
    ),
    ro(b"doctype", crate::document::doctype),
    ro(b"documentElement", crate::document::document_element),
    ro(b"children", crate::props::children),
    ro(b"firstElementChild", crate::props::first_element_child),
    ro(b"lastElementChild", crate::props::last_element_child),
    ro(b"childElementCount", crate::props::child_element_count),
    rw(b"body", body, set_body),
    ro(b"head", head),
    rw(b"title", title, set_title),
];

/// `dom_html_document_encoding_write`: an encoding lexbor knows, kept by
/// its canonical name, php's `ValueError` for another.
fn set_character_set(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let doc = prop_node(req, object)?;
    let given = value.as_str_bytes().unwrap_or_default();
    match php_ext_lexbor::ffi::encoding::Encoding::by_name(given) {
        Some(found) => {
            doc.set_doc_string(php_ext_libxml::ffi::DocString::Encoding, Some(found.name()));
            Ok(())
        }
        None => Err(php_ext::raising::throw_error(
            req,
            "ValueError",
            "Invalid document encoding",
        )),
    }
}

/// `final class Dom\HTMLDocument extends Dom\Document`: the HTML5
/// parser's factories of `html_document.c`, php's HTML serialiser, and
/// `DOMDocument`'s XML savers.
///
/// A document costs what php's does, its `php_dom_private_data` among it,
/// and its unset gives all of it back:
///
/// ```php
/// $w = Dom\HTMLDocument::createEmpty(); unset($w);
/// $m = memory_get_usage(); $d = Dom\HTMLDocument::createEmpty(); var_dump(memory_get_usage() - $m);
/// $m = memory_get_usage(); unset($d); var_dump(memory_get_usage() - $m);
/// ```
/// ```expect
/// int(264)
/// int(-264)
/// ```
#[php_class(
    namespace(Dom),
    final,
    extends(crate::document::Document),
    handlers = crate::object::NODE_HANDLERS
)]
pub(crate) struct HTMLDocument;

#[php_class]
impl HTMLDocument {
    /// `Dom\HTMLDocument::createEmpty(string $encoding = "UTF-8"): Dom\HTMLDocument`.
    ///
    /// ```php
    /// $d = Dom\HTMLDocument::createEmpty();
    /// var_dump(get_class($d), $d->characterSet, $d->childNodes->length, $d->documentElement);
    /// var_dump(Dom\HTMLDocument::createEmpty('latin1')->charset);
    /// try { Dom\HTMLDocument::createEmpty('nope'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(16) "Dom\HTMLDocument"
    /// string(5) "UTF-8"
    /// int(0)
    /// NULL
    /// string(6) "latin1"
    /// Dom\HTMLDocument::createEmpty(): Argument #1 ($encoding) must be a valid document encoding
    /// ```
    fn createEmpty(
        req: &mut Request,
        #[php(default = "UTF-8")] encoding: Path,
    ) -> Result<Owner<Instance<HTMLDocument>>, Thrown> {
        crate::html5_parser::create_empty(req, encoding.0.as_bytes()).map(Owner::into_instance)
    }

    /// `Dom\HTMLDocument::createFromFile(string $path, int $options = 0, ?string $overrideEncoding = null): Dom\HTMLDocument`.
    ///
    /// ```php
    /// $f = tempnam(sys_get_temp_dir(), 'dom');
    /// file_put_contents($f, "<!doctype html>\n<title>t</title><p>x");
    /// $d = Dom\HTMLDocument::createFromFile($f);
    /// var_dump($d->title, $d->body->textContent, $d->URL === 'file://' . realpath($f));
    /// unlink($f);
    /// try { @Dom\HTMLDocument::createFromFile('/nonexistent/x.html'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "t"
    /// string(1) "x"
    /// bool(true)
    /// Cannot open file '/nonexistent/x.html'
    /// ```
    fn createFromFile(
        req: &mut Request,
        path: PhpString,
        #[php(default = 0)] options: i64,
        #[php(default = null)] overrideEncoding: Option<PhpString>,
    ) -> Result<Owner<Raw<Instance<HTMLDocument>>>, Thrown> {
        let made =
            crate::html5_parser::create_from_file(req, path.as_bytes(), options, overrideEncoding)?;
        crate::returned(made)
    }

    /// `Dom\HTMLDocument::createFromString(string $source, int $options = 0, ?string $overrideEncoding = null): Dom\HTMLDocument`.
    ///
    /// ```php
    /// $d = Dom\HTMLDocument::createFromString('<!doctype html><p class=a>x<b>y</p>');
    /// var_dump(get_class($d), $d->documentElement->tagName, $d->body->firstChild->className, $d->characterSet);
    /// $e = Dom\HTMLDocument::createFromString('<p>x', LIBXML_NOERROR);
    /// var_dump($e->body->textContent);
    /// $f = Dom\HTMLDocument::createFromString('<p>x</p>', LIBXML_HTML_NOIMPLIED);
    /// var_dump($f->documentElement->tagName);
    /// try { Dom\HTMLDocument::createFromString('', 1); } catch (ValueError $x) { echo $x->getMessage(), "\n"; }
    /// ```
    /// ```expect
    ///
    /// Warning: Dom\HTMLDocument::createFromString(): tree error unexpected-element-in-open-elements-stack in Entity, line: 1, column: 34 in %s on line 2
    /// string(16) "Dom\HTMLDocument"
    /// string(4) "HTML"
    /// string(1) "a"
    /// string(5) "UTF-8"
    /// string(1) "x"
    /// string(1) "P"
    /// Dom\HTMLDocument::createFromString(): Argument #2 ($options) contains invalid flags (allowed flags: LIBXML_NOERROR, LIBXML_COMPACT, LIBXML_HTML_NOIMPLIED, Dom\HTML_NO_DEFAULT_NS)
    /// ```
    fn createFromString(
        req: &mut Request,
        source: PhpString,
        #[php(default = 0)] options: i64,
        #[php(default = null)] overrideEncoding: Option<PhpString>,
    ) -> Result<Owner<Raw<Instance<HTMLDocument>>>, Thrown> {
        let made = crate::html5_parser::create_from_string(
            req,
            source.as_bytes(),
            options,
            overrideEncoding,
        )?;
        crate::returned(made)
    }

    #[php(implementation = crate::document::DOMDocument::saveXML)]
    fn saveXml(
        this: This<Self>,
        #[php(default = null)] node: Option<Instance<crate::modern::Node>>,
        #[php(default = 0)] options: i64,
    ) -> OrFalse<PhpString>;
    #[php(implementation = crate::document::DOMDocument::save)]
    fn saveXmlFile(
        this: This<Self>,
        filename: PhpString,
        #[php(default = 0)] options: i64,
    ) -> OrFalse<i64>;

    /// `Dom\HTMLDocument::saveHtml(?Dom\Node $node = null): string`.
    ///
    /// ```php
    /// $d = Dom\HTMLDocument::createEmpty();
    /// $html = $d->appendChild($d->createElement('html'));
    /// $p = $html->appendChild($d->createElement('p'));
    /// $p->setAttribute('title', "a\"b&\u{a0}<");
    /// $p->append("x<y> & \u{a0}", $d->createElement('br'), $d->createComment('c'));
    /// $s = $html->appendChild($d->createElement('script'));
    /// $s->append('if (a < b) {}');
    /// echo $d->saveHtml(), "\n", $d->saveHtml($p), "\n";
    /// $x = Dom\XMLDocument::createEmpty();
    /// try { $d->saveHtml($x->createElement('q')); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// <html><p title="a&quot;b&amp;&nbsp;<">x&lt;y&gt; &amp; &nbsp;<br><!--c--></p><script>if (a < b) {}</script></html>
    /// <p title="a&quot;b&amp;&nbsp;<">x&lt;y&gt; &amp; &nbsp;<br><!--c--></p>
    /// Wrong Document Error
    /// ```
    fn saveHtml(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] node: Option<Instance<crate::modern::Node>>,
    ) -> Result<Owner<PhpString>, Thrown> {
        crate::html5_serializer::save_html(req, this.object(), node.map(|n| n.object()))
    }

    /// `Dom\HTMLDocument::saveHtmlFile(string $filename): int|false`: the
    /// document written through php's streams, each piece as it is made.
    ///
    /// ```php
    /// $d = Dom\HTMLDocument::createEmpty();
    /// $d->appendChild($d->createElement('p'))->append("\u{e9}");
    /// $f = tempnam(sys_get_temp_dir(), 'dom');
    /// var_dump($d->saveHtmlFile($f), file_get_contents($f));
    /// unlink($f);
    /// try { $d->saveHtmlFile(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(9)
    /// string(9) "<p>é</p>"
    /// Dom\HTMLDocument::saveHtmlFile(): Argument #1 ($filename) must not be empty
    /// ```
    fn saveHtmlFile(
        req: &mut Request,
        this: This<Self>,
        filename: Path,
    ) -> Result<OrFalse<i64>, Thrown> {
        let written =
            crate::html5_serializer::save_html_file(req, this.object(), filename.0.as_bytes())?;
        Ok(written.map_or(OrFalse::False, OrFalse::Value))
    }
}
