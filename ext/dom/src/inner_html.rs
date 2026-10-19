//! `inner_outer_html_mixin.c`: `Dom\Element`'s `innerHTML` and
//! `outerHTML` (the DOM Parsing and HTML standards' mixins, 2024-04-14).
//! An HTML document reads through the HTML serialiser and writes through
//! the HTML fragment parser; an XML document reads through the XML
//! serialiser, held to well-formedness, and writes through libxml's
//! parser over the markup put inside an element named like the one given,
//! carrying the namespaces in scope there.

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, Value};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi::{Fragment, Kind, Node, Ns, Parser};

use crate::document::XMLNS_NS;
use crate::element::AdjacentPosition;
use crate::object::Code;
use crate::private::{HTML_NS, html_in_html_document, ns_is};
use crate::props::Prop;
use crate::{Read, dom_mut, dom_throw, dom_throw_message, extracted, fetch, prop_node};

/// `dom_element_html_fragment_serialize` for `innerHTML` (`outer` false:
/// the element's children) and `outerHTML` (`outer`: the element alone).
fn fragment_serialize(
    req: &mut Request,
    object: PhpObject,
    node: Node,
    outer: bool,
) -> Result<Owner<Value>, Thrown> {
    if node.doc().is_some_and(|d| d.kind() == Kind::HtmlDocument) {
        let templates = crate::html5_serializer::templates_of(req, object, node);
        let mut bytes = Vec::new();
        if outer {
            crate::html5_serializer::outer(&mut bytes, &templates, node);
        } else {
            crate::html5_serializer::inner(&mut bytes, &templates, node);
        }
        return extracted(req, &bytes);
    }
    let nodes: Vec<Node> = if outer {
        vec![node]
    } else {
        crate::node_list(node.children())
    };
    match crate::xml_serializer::well_formed(req, object, &nodes)? {
        Some(bytes) => extracted(req, &bytes),
        None => Err(dom_throw_message(
            req,
            Code::Syntax,
            "The resulting XML serialization is not well-formed",
        )),
    }
}

/// `dom_element_inner_html_read`.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r xmlns="urn:a"><p:x xmlns:p="urn:p">t&amp;</p:x><y/></r>');
/// $r = $d->documentElement;
/// var_dump($r->innerHTML);
/// $r->append($d->createComment('a--b'));
/// try { var_dump($r->innerHTML); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// $h = Dom\HTMLDocument::createEmpty();
/// $div = $h->appendChild($h->createElement('div'));
/// $div->append('a<b', $h->createElement('br'));
/// var_dump($div->innerHTML, $h->createElement('img')->innerHTML);
/// ```
/// ```expect
/// string(51) "<p:x xmlns:p="urn:p">t&amp;</p:x><y xmlns="urn:a"/>"
/// The resulting XML serialization is not well-formed
/// string(10) "a&lt;b<br>"
/// string(0) ""
/// ```
fn inner_html(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    fragment_serialize(req, object, node, false).map(Some)
}

/// `dom_element_outer_html_read`.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r xmlns="urn:a"><p:x xmlns:p="urn:p" a="1"/></r>');
/// $x = $d->documentElement->firstChild;
/// var_dump($d->documentElement->outerHTML, $x->outerHTML);
/// $h = Dom\HTMLDocument::createEmpty();
/// $p = $h->appendChild($h->createElement('p'));
/// $p->setAttribute('class', 'c"');
/// $p->append($h->createElement('br'), 'x');
/// var_dump($p->outerHTML);
/// ```
/// ```expect
/// string(49) "<r xmlns="urn:a"><p:x xmlns:p="urn:p" a="1"/></r>"
/// string(28) "<p:x xmlns:p="urn:p" a="1"/>"
/// string(28) "<p class="c&quot;"><br>x</p>"
/// ```
fn outer_html(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    fragment_serialize(req, object, node, true).map(Some)
}

/// `dom_locate_a_namespace(node, NULL)` over elements: the default
/// namespace in scope at an element.
fn default_namespace(element: Node) -> Option<Vec<u8>> {
    let mut node = element;
    loop {
        if let Some(ns) = node.ns()
            && ns.prefix().is_none()
        {
            return ns.href();
        }
        let mut attr = node.properties();
        while let Some(a) = attr {
            attr = a.next();
            if ns_is(a, XMLNS_NS) && a.ns().and_then(Ns::prefix).is_none() && a.name() == b"xmlns" {
                return a
                    .children()
                    .and_then(Node::content)
                    .filter(|c| !c.is_empty());
            }
        }
        match node.parent() {
            Some(parent) if parent.kind() == Kind::Element => node = parent,
            _ => return None,
        }
    }
}

/// `dom_mark_namespaces_as_attributes_too`: every declaration the parser
/// put on an element of `doc` turned into an `xmlns` attribute.
fn mark_namespaces(req: &mut Request, object: PhpObject, doc: Node) -> Result<(), Thrown> {
    crate::private::mapped(req, object, |mapper, req| {
        let mut at = doc.children();
        while let Some(node) = at {
            if node.kind() == Kind::Element {
                mapper.mark_attribute_list(req, node)?;
            }
            at = crate::next_in_tree_order_from(node, None);
        }
        Ok(Some(()))
    })
    .map(drop)
}

/// `dom_xml_fragment_parsing_algorithm`: `input` parsed by libxml inside
/// an element named like `context` that declares every namespace in scope
/// there, its children handed back as a fragment of `context`'s document;
/// `None` where libxml built nothing.
fn xml_fragment(
    req: &mut Request,
    object: PhpObject,
    context: Node,
    input: &[u8],
) -> Result<Option<Node>, Thrown> {
    let Some(doc) = context.doc() else {
        return Err(dom_throw(req, Code::InvalidState));
    };
    let in_scope: Vec<Ns> = crate::private::mapped(req, object, |mapper, req| {
        mapper.in_scope(req, context, true).map(Some)
    })?
    .unwrap_or_default();
    let prefix = context.ns().and_then(Ns::prefix);
    let name = context.name();
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let tag_name = |chunks: &mut Vec<Vec<u8>>| {
        if let Some(prefix) = &prefix {
            chunks.push(prefix.clone());
            chunks.push(b":".to_vec());
        }
        chunks.push(name.clone());
    };
    chunks.push(b"<".to_vec());
    tag_name(&mut chunks);
    for ns in in_scope {
        chunks.push(b" xmlns:".to_vec());
        chunks.push(ns.prefix().unwrap_or_default());
        chunks.push(b"=\"".to_vec());
        chunks.push(ns.href().unwrap_or_default());
        chunks.push(b"\"".to_vec());
    }
    if let Some(default) = default_namespace(context) {
        chunks.push(b" xmlns=\"".to_vec());
        chunks.push(default);
        chunks.push(b"\"".to_vec());
    }
    chunks.push(b">".to_vec());
    chunks.push(input.to_vec());
    chunks.push(b"</".to_vec());
    tag_name(&mut chunks);
    chunks.push(b">".to_vec());
    let pieces: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
    let (parsed, raised) = Parser::fragment(doc, &pieces);
    Errors::route(req, raised);
    let parsed = match parsed {
        Fragment::NoParser => return Err(dom_throw(req, Code::InvalidState)),
        Fragment::Malformed => {
            return Err(dom_throw_message(
                req,
                Code::Syntax,
                "XML fragment is not well-formed",
            ));
        }
        Fragment::Document(None) => return Ok(None),
        Fragment::Document(Some(parsed)) => parsed,
    };
    let root = match parsed.children() {
        Some(root) if root.next().is_none() => root,
        _ => {
            parsed.free_document();
            return Err(dom_throw_message(
                req,
                Code::Syntax,
                "XML fragment is not well-formed",
            ));
        }
    };
    let fragment = Node::new_fragment(Some(doc));
    if let Some(fragment) = fragment {
        let first = root.children();
        root.set_tree_doc(Some(doc));
        if let Some(first) = first {
            fragment.add_child_list(first);
        }
        let marked = mark_namespaces(req, object, parsed);
        root.set_children(None);
        root.set_last(None);
        if let Err(thrown) = marked {
            parsed.free_document();
            fragment.free();
            return Err(thrown);
        }
    }
    parsed.free_document();
    Ok(fragment)
}

/// `dom_parse_fragment`: libxml's parser for an XML document, lexbor's
/// for an HTML one.
fn parse_fragment(
    req: &mut Request,
    object: PhpObject,
    context: Node,
    input: &[u8],
) -> Result<Option<Node>, Thrown> {
    if context.doc().is_some_and(|d| d.kind() == Kind::Document) {
        xml_fragment(req, object, context, input)
    } else {
        crate::html5_parser::parse_fragment(req, object, context, input).map(Some)
    }
}

/// The string a handled property was given, coerced already by its
/// declared type.
fn given(value: &Value) -> Vec<u8> {
    value
        .deref()
        .as_str_bytes()
        .map(<[u8]>::to_vec)
        .unwrap_or_default()
}

/// `dom_element_inner_html_write`: the children replaced by the parsed
/// fragment, a template's contents in place of its children.
fn set_inner_html(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let mut context = prop_node(req, object)?;
    let Some(fragment) = parse_fragment(req, object, context, &given(value))? else {
        return Ok(());
    };
    if ns_is(context, HTML_NS) && context.name() == b"template" {
        let contents =
            dom_mut(req, object).and_then(|d| crate::private::ensure_template(&mut d.obj, context));
        let Some(contents) = contents else {
            fragment.free();
            return Ok(());
        };
        context = contents;
    }
    crate::invalidate(req, object);
    crate::remove_all_children(req, context);
    crate::parent::pre_insert(req, object, fragment, context, None)?;
    Ok(())
}

/// `dom_element_outer_html_write`: the element replaced by the fragment
/// parsed in its parent, a `body` element standing in for a fragment
/// parent.
fn set_outer_html(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let this = prop_node(req, object)?;
    let Some(parent) = this.parent() else {
        return Ok(());
    };
    if parent.kind().is_document() {
        return Err(dom_throw(req, Code::InvalidModification));
    }
    let mut created = None;
    let context = if parent.kind() == Kind::Fragment {
        let doc = parent.doc();
        let html = crate::private::mapped(req, object, |mapper, req| mapper.html(req, doc))?;
        let Some(body) = Node::new_element(doc, html, b"body", None) else {
            return Err(dom_throw(req, Code::InvalidState));
        };
        created = Some(body);
        body
    } else {
        parent
    };
    let inserted = match parse_fragment(req, object, context, &given(value)) {
        Ok(Some(fragment)) => {
            crate::invalidate(req, object);
            let inserted = match this.parent() {
                Some(parent) => {
                    crate::parent::pre_insert(req, object, fragment, parent, Some(this))
                }
                None => Ok(false),
            };
            if matches!(inserted, Ok(true)) {
                this.unlink();
            }
            inserted
        }
        Ok(None) => Ok(false),
        Err(thrown) => Err(thrown),
    };
    if let Some(body) = created {
        body.free();
    }
    inserted.map(|_| ())
}

/// `innerHTML`, the row of `Dom\Element` after `nextElementSibling`.
pub(crate) const INNER_HTML: Prop = Prop {
    name: b"innerHTML",
    read: inner_html,
    write: Some(set_inner_html),
};

/// `outerHTML`, the row after `innerHTML`.
pub(crate) const OUTER_HTML: Prop = Prop {
    name: b"outerHTML",
    read: outer_html,
    write: Some(set_outer_html),
};

/// `Dom\Element::insertAdjacentHTML`'s body: the markup parsed under the
/// element `position` names and inserted there.
pub(crate) fn insert_adjacent_html(
    req: &mut Request,
    this: PhpObject,
    position: AdjacentPosition,
    markup: &[u8],
) -> Result<(), Thrown> {
    let element = fetch(req, this)?;
    let mut context = match position {
        AdjacentPosition::BeforeBegin | AdjacentPosition::AfterEnd => match element.parent() {
            Some(parent) if !parent.kind().is_document() => parent,
            _ => return Err(dom_throw(req, Code::NoModificationAllowed)),
        },
        AdjacentPosition::AfterBegin | AdjacentPosition::BeforeEnd => element,
    };
    let mut created = None;
    if context.kind() != Kind::Element
        || (html_in_html_document(context) && context.name() == b"html")
    {
        let doc = element.doc();
        let html = crate::private::mapped(req, this, |mapper, req| mapper.html(req, doc))?;
        let Some(body) = Node::new_element(doc, html, b"body", None) else {
            return Err(dom_throw(req, Code::InvalidState));
        };
        created = Some(body);
        context = body;
    }
    let inserted = match parse_fragment(req, this, context, markup) {
        Ok(Some(fragment)) => {
            crate::invalidate(req, this);
            let (parent, point) = match position {
                AdjacentPosition::BeforeBegin => (element.parent(), Some(element)),
                AdjacentPosition::AfterEnd => (element.parent(), element.next()),
                AdjacentPosition::AfterBegin => (Some(element), element.children()),
                AdjacentPosition::BeforeEnd => (Some(element), None),
            };
            match parent {
                Some(parent) => crate::parent::pre_insert(req, this, fragment, parent, point),
                None => Ok(false),
            }
        }
        Ok(None) => Ok(false),
        Err(thrown) => Err(thrown),
    };
    if let Some(body) = created {
        body.free();
    }
    inserted.map(|_| ())
}
