//! `Dom\Document`'s properties and `Dom\HTMLDocument` (`html_document.c`,
//! with the `Dom\Document` rows `PHP_MINIT_FUNCTION(dom)` registers):
//! `implementation`, `URL`, the encoding names, and `body`, `head` and
//! `title` as the HTML standard defines them over the tree.

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{PhpObject, Value};
use php_ext_libxml::ffi::{Kind, Node};

use crate::object::Code;
use crate::private::{HTML_NS, SVG_NS, ns_is};
use crate::props::Prop;
use crate::token_list::strip_and_collapse;
use crate::{Read, dom_error_message, nullable_object, prop_node, string};

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
        req.addref(&kept);
        return Ok(Some(kept));
    }
    let class = crate::class(req, "Dom\\Implementation")?;
    let made = php_ext::call::new_object(req, class)?.as_value();
    crate::modern::keep(req, object, b"implementation", &made)?;
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
        crate::adopt(req, object, fresh, doc);
        current.replace_with(fresh);
        if current.object().is_none() {
            current.free_resource();
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
    crate::adopt(req, object, fresh, doc);
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
        return Ok(Some(string(req, b"")?));
    };
    let title = if is_svg_root(root) {
        svg_title_element(root)
    } else {
        title_element(doc)
    };
    let value = title.map(child_text_content).unwrap_or_default();
    Ok(Some(string(req, &strip_and_collapse(&value))?))
}

/// `dom_string_replace_all`.
fn replace_all(doc: Node, element: Node, value: &[u8]) {
    crate::remove_all_children(element);
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
                    Some(ns) if ns.prefix().is_some() => crate::dom_mut(req, object)
                        .and_then(|d| crate::private::of(&mut d.obj))
                        .and_then(|p| p.mapper.get(Some(doc), b"", SVG_NS)),
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
        replace_all(doc, element, &bytes);
    } else if ns_is(root, HTML_NS) {
        let title = title_element(doc);
        let head = html_child(doc, is_head);
        let element = match (title, head) {
            (Some(t), _) => t,
            (None, None) => return Ok(()),
            (None, Some(head)) => {
                let ns = crate::dom_mut(req, object)
                    .and_then(|d| crate::private::of(&mut d.obj))
                    .and_then(|p| p.mapper.html(Some(doc)));
                let Some(element) = Node::new_element(Some(doc), ns, b"title", None) else {
                    return Err(crate::dom_throw(req, Code::InvalidState));
                };
                head.add_child(element);
                element
            }
        };
        replace_all(doc, element, &bytes);
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
        None => Err(php_ext::raising::throw(
            req,
            "ValueError",
            "Invalid document encoding",
        )),
    }
}
