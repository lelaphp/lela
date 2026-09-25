//! The virtual properties of the legacy classes: each class's handler
//! table in `PHP_MINIT_FUNCTION(dom)`'s registration order, a class's own
//! handlers first and its parent's merged behind them
//! (`zend_hash_merge(..., false)`), and the readers and writers each
//! handler names.

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{PhpObject, Value};

use crate::map::MapKind;
use crate::object::Code;
use crate::{
    Read, attr_value_will_change, children_valid, content_value, deprecated_property, dom_error,
    invalidate, is_connected, nullable_object, nullable_string, object_for, prop_node,
    qualified_name, remove_all_children, string, sync_entity_reference,
};
use php_ext_libxml::ffi::{Kind, Node, Ns, Save, Utf8, XML_EXTERNAL_GENERAL_UNPARSED_ENTITY};

/// One handled property: `dom_prop_handler`.
pub(crate) struct Prop {
    pub(crate) name: &'static [u8],
    pub(crate) read: Reader,
    pub(crate) write: Option<Writer>,
}

/// `dom_read_t`.
pub(crate) type Reader = fn(&mut Request, PhpObject) -> Read;

/// `dom_write_t`.
pub(crate) type Writer = fn(&mut Request, PhpObject, &Value) -> Result<(), Thrown>;

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

pub(crate) const NODE: &[Prop] = &[
    ro(b"nodeName", node_name),
    rw(b"nodeValue", node_value, set_node_value),
    ro(b"nodeType", node_type),
    ro(b"parentNode", parent_node),
    ro(b"parentElement", parent_element),
    ro(b"childNodes", child_nodes),
    ro(b"firstChild", first_child),
    ro(b"lastChild", last_child),
    ro(b"previousSibling", previous_sibling),
    ro(b"nextSibling", next_sibling),
    ro(b"attributes", attributes),
    ro(b"isConnected", is_connected_read),
    ro(b"ownerDocument", owner_document),
    ro(b"namespaceURI", namespace_uri),
    rw(b"prefix", prefix, set_prefix),
    ro(b"localName", local_name),
    ro(b"baseURI", base_uri),
    rw(b"textContent", text_content, set_text_content),
];

pub(crate) const NAMESPACE_NODE: &[Prop] = &[
    ro(b"nodeName", node_name),
    ro(b"nodeValue", node_value),
    ro(b"nodeType", node_type),
    ro(b"prefix", prefix),
    ro(b"localName", local_name),
    ro(b"namespaceURI", namespace_uri),
    ro(b"isConnected", is_connected_read),
    ro(b"ownerDocument", owner_document),
    ro(b"parentNode", parent_node),
    ro(b"parentElement", parent_element),
];

pub(crate) const PARENT: &[Prop] = &[
    ro(b"firstElementChild", first_element_child),
    ro(b"lastElementChild", last_element_child),
    ro(b"childElementCount", child_element_count),
];

pub(crate) const CHARACTER_DATA: &[Prop] = &[
    rw(b"data", data, set_data),
    ro(b"length", data_length),
    ro(b"previousElementSibling", previous_element_sibling),
    ro(b"nextElementSibling", next_element_sibling),
];

pub(crate) const ATTR: &[Prop] = &[
    ro(b"name", attr_name),
    ro(b"specified", attr_specified),
    rw(b"value", attr_value, set_attr_value),
    ro(b"ownerElement", owner_element),
    ro(b"schemaTypeInfo", null_read),
];

pub(crate) const ELEMENT: &[Prop] = &[
    ro(b"tagName", tag_name),
    rw(b"className", class_name, set_class_name),
    rw(b"id", id, set_id),
    ro(b"schemaTypeInfo", null_read),
    ro(b"firstElementChild", first_element_child),
    ro(b"lastElementChild", last_element_child),
    ro(b"childElementCount", child_element_count),
    ro(b"previousElementSibling", previous_element_sibling),
    ro(b"nextElementSibling", next_element_sibling),
];

pub(crate) const TEXT: &[Prop] = &[ro(b"wholeText", whole_text)];

pub(crate) const DOCUMENT_TYPE: &[Prop] = &[
    ro(b"name", doctype_name),
    ro(b"entities", doctype_entities),
    ro(b"notations", doctype_notations),
    ro(b"publicId", doctype_public_id),
    ro(b"systemId", doctype_system_id),
    ro(b"internalSubset", internal_subset),
];

pub(crate) const NOTATION: &[Prop] = &[
    ro(b"publicId", notation_public_id),
    ro(b"systemId", notation_system_id),
];

pub(crate) const ENTITY: &[Prop] = &[
    ro(b"publicId", entity_public_id),
    ro(b"systemId", entity_system_id),
    ro(b"notationName", entity_notation_name),
    ro(b"actualEncoding", entity_actual_encoding),
    ro(b"encoding", entity_encoding),
    ro(b"version", entity_version),
];

/// `dom_entity_reference_prop_handlers`: the node's table with four
/// handlers overwritten in place.
pub(crate) const ENTITY_REFERENCE: &[Prop] = &[
    ro(b"nodeName", node_name),
    rw(b"nodeValue", node_value, set_node_value),
    ro(b"nodeType", node_type),
    ro(b"parentNode", parent_node),
    ro(b"parentElement", parent_element),
    ro(b"childNodes", entity_reference_child_nodes),
    ro(b"firstChild", entity_reference_child),
    ro(b"lastChild", entity_reference_child),
    ro(b"previousSibling", previous_sibling),
    ro(b"nextSibling", next_sibling),
    ro(b"attributes", attributes),
    ro(b"isConnected", is_connected_read),
    ro(b"ownerDocument", owner_document),
    ro(b"namespaceURI", namespace_uri),
    rw(b"prefix", prefix, set_prefix),
    ro(b"localName", local_name),
    ro(b"baseURI", base_uri),
    ro(b"textContent", entity_reference_text_content),
];

pub(crate) const PROCESSING_INSTRUCTION: &[Prop] =
    &[ro(b"target", pi_target), rw(b"data", data, set_data)];

// ---- the helpers every reader shares

#[allow(clippy::unnecessary_wraps)] // the reader shape every row of a property table has
fn some(value: Value) -> Read {
    Ok(Some(value))
}

fn string_or_null(req: &mut Request, bytes: Option<&[u8]>) -> Read {
    some(nullable_string(req, bytes)?)
}

fn string_or_empty(req: &mut Request, bytes: Option<Vec<u8>>) -> Read {
    some(string(req, &bytes.unwrap_or_default())?)
}

fn null_read(_req: &mut Request, _object: PhpObject) -> Read {
    some(Value::null())
}

/// A written value as bytes, `null` as the empty string: the property's
/// type already held it to `?string`.
fn written_bytes(value: &Value) -> Vec<u8> {
    value.as_str_bytes().map(<[u8]>::to_vec).unwrap_or_default()
}

// ---- DOMNode

/// `dom_node_node_name_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<?xml-stylesheet x?><p:r xmlns:p="urn:p" a="1"><![CDATA[c]]><!--k-->t</p:r>');
/// $r = $d->documentElement;
/// echo $d->nodeName, ' ', $d->firstChild->nodeName, ' ', $r->nodeName, ' ', $r->attributes->item(0)->nodeName, "\n";
/// foreach ($r->childNodes as $c) echo $c->nodeName, ' ';
/// echo $d->createDocumentFragment()->nodeName, "\n";
/// ```
/// ```expect
/// #document xml-stylesheet p:r a
/// #cdata-section #comment #text #document-fragment
/// ```
fn node_name(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let name: Vec<u8> = match node.kind() {
        Kind::Element | Kind::Attribute => qualified_name(node),
        Kind::NamespaceDecl => match node.ns().and_then(Ns::prefix) {
            Some(prefix) => [b"xmlns:".as_slice(), &prefix].concat(),
            None => node.name(),
        },
        Kind::DocumentType
        | Kind::Dtd
        | Kind::Pi
        | Kind::EntityDecl
        | Kind::EntityRef
        | Kind::Notation => node.name(),
        Kind::CData => b"#cdata-section".to_vec(),
        Kind::Comment => b"#comment".to_vec(),
        Kind::HtmlDocument | Kind::Document => b"#document".to_vec(),
        Kind::Fragment => b"#document-fragment".to_vec(),
        Kind::Text => b"#text".to_vec(),
        _ => Vec::new(),
    };
    some(string(req, &name)?)
}

/// `dom_node_node_value_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r a="v">t<x/></r>');
/// var_dump($d->nodeValue, $d->documentElement->nodeValue, $d->documentElement->getAttributeNode('a')->nodeValue);
/// ```
/// ```expect
/// NULL
/// string(1) "t"
/// string(1) "v"
/// ```
fn node_value(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    match node.kind() {
        Kind::Element | Kind::Attribute | Kind::Text | Kind::Comment | Kind::CData | Kind::Pi => {
            some(content_value(req, node, true)?)
        }
        Kind::NamespaceDecl => {
            let content = node.children().and_then(Node::get_content);
            string_or_null(req, content.as_deref())
        }
        _ => some(Value::null()),
    }
}

/// `dom_node_node_value_write`.
fn set_node_value(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let node = prop_node(req, object)?;
    let bytes = written_bytes(value);
    match node.kind() {
        Kind::Attribute => {
            attr_value_will_change(req, object, node);
            remove_all_children(node);
            node.set_content_len(&bytes);
        }
        Kind::Element => {
            remove_all_children(node);
            node.set_content_len(&bytes);
        }
        Kind::Text | Kind::Comment | Kind::CData | Kind::Pi => node.set_content_len(&bytes),
        _ => {}
    }
    invalidate(req, object);
    Ok(())
}

/// `dom_node_node_type_read`: a DTD answers as a document type.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<!DOCTYPE r><r/>');
/// var_dump($d->nodeType, $d->doctype->nodeType, $d->documentElement->nodeType);
/// ```
/// ```expect
/// int(9)
/// int(10)
/// int(1)
/// ```
fn node_type(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let kind = if node.kind() == Kind::Dtd {
        Kind::DocumentType
    } else {
        node.kind()
    };
    some(Value::int(kind.number()))
}

fn parent_of(req: &mut Request, object: PhpObject, only_element: bool) -> Read {
    let node = prop_node(req, object)?;
    let parent = node
        .parent()
        .filter(|p| !only_element || p.kind() == Kind::Element);
    some(nullable_object(req, parent, Some(object))?)
}

/// `dom_node_parent_node_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/></r>');
/// var_dump($d->documentElement->firstChild->parentNode->nodeName, $d->documentElement->parentNode === $d, $d->parentNode);
/// ```
/// ```expect
/// string(1) "r"
/// bool(true)
/// NULL
/// ```
fn parent_node(req: &mut Request, object: PhpObject) -> Read {
    parent_of(req, object, false)
}

/// `dom_node_parent_element_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/></r>');
/// var_dump($d->documentElement->firstChild->parentElement->nodeName, $d->documentElement->parentElement);
/// ```
/// ```expect
/// string(1) "r"
/// NULL
/// ```
fn parent_element(req: &mut Request, object: PhpObject) -> Read {
    parent_of(req, object, true)
}

/// `dom_node_child_nodes_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/>t<b/></r>');
/// $l = $d->documentElement->childNodes;
/// var_dump(get_class($l), $l->length, $l->item(2)->nodeName);
/// ```
/// ```expect
/// string(11) "DOMNodeList"
/// int(3)
/// string(1) "b"
/// ```
fn child_nodes(req: &mut Request, object: PhpObject) -> Read {
    prop_node(req, object)?;
    some(crate::create(
        req,
        object,
        "DOMNodeList",
        MapKind::ChildNodes,
    )?)
}

/// `dom_node_first_child_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/><b/></r>');
/// var_dump($d->documentElement->firstChild->nodeName, $d->documentElement->lastChild->nodeName, $d->documentElement->firstChild->firstChild);
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "b"
/// NULL
/// ```
fn first_child(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let first = if children_valid(node) {
        node.children()
    } else {
        None
    };
    some(nullable_object(req, first, Some(object))?)
}

/// `dom_node_last_child_read`.
fn last_child(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let last = if children_valid(node) {
        node.last()
    } else {
        None
    };
    some(nullable_object(req, last, Some(object))?)
}

/// `dom_node_previous_sibling_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/><b/></r>');
/// $a = $d->documentElement->firstChild;
/// var_dump($a->nextSibling->nodeName, $a->nextSibling->previousSibling === $a, $a->previousSibling);
/// ```
/// ```expect
/// string(1) "b"
/// bool(true)
/// NULL
/// ```
fn previous_sibling(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    some(nullable_object(req, node.prev(), Some(object))?)
}

/// `dom_node_next_sibling_read`.
fn next_sibling(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    some(nullable_object(req, node.next(), Some(object))?)
}

/// `dom_node_previous_element_sibling_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/>t<b/>u</r>');
/// $t = $d->documentElement->childNodes->item(1);
/// var_dump($t->previousElementSibling->nodeName, $t->nextElementSibling->nodeName, $d->documentElement->lastChild->nextElementSibling);
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "b"
/// NULL
/// ```
fn previous_element_sibling(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let mut at = node.prev();
    while let Some(n) = at {
        if n.kind() == Kind::Element {
            break;
        }
        at = n.prev();
    }
    some(nullable_object(req, at, Some(object))?)
}

/// `dom_node_next_element_sibling_read`.
fn next_element_sibling(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let mut at = node.next();
    while let Some(n) = at {
        if n.kind() == Kind::Element {
            break;
        }
        at = n.next();
    }
    some(nullable_object(req, at, Some(object))?)
}

/// `dom_node_attributes_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r a="1" b="2">t</r>');
/// var_dump($d->documentElement->attributes->length, $d->documentElement->firstChild->attributes);
/// ```
/// ```expect
/// int(2)
/// NULL
/// ```
fn attributes(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    if node.kind() != Kind::Element {
        return some(Value::null());
    }
    some(crate::create(
        req,
        object,
        "DOMNamedNodeMap",
        MapKind::Attributes,
    )?)
}

/// `dom_node_is_connected_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r/>');
/// var_dump($d->documentElement->isConnected, $d->createElement('x')->isConnected);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
fn is_connected_read(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    some(Value::bool(is_connected(node)))
}

/// `dom_node_owner_document_read`: `null` for a document, nothing for a
/// node with none.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r/>');
/// var_dump($d->documentElement->ownerDocument === $d, $d->ownerDocument, (new DOMElement('x'))->ownerDocument);
/// ```
/// ```expect
/// bool(true)
/// NULL
/// NULL
/// ```
fn owner_document(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    if node.kind().is_document() {
        return some(Value::null());
    }
    let Some(doc) = node.doc() else {
        return Ok(None);
    };
    some(object_for(req, doc, Some(object))?)
}

/// `dom_node_namespace_uri_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r xmlns="urn:d"><c/></r>');
/// var_dump($d->documentElement->firstChild->namespaceURI, $d->namespaceURI);
/// ```
/// ```expect
/// string(5) "urn:d"
/// NULL
/// ```
fn namespace_uri(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let href = match node.kind() {
        Kind::Element | Kind::Attribute | Kind::NamespaceDecl => node.ns().and_then(Ns::href),
        _ => None,
    };
    string_or_null(req, href.as_deref())
}

/// `dom_node_prefix_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<p:r xmlns:p="urn:p"><c/></p:r>');
/// var_dump($d->documentElement->prefix, $d->documentElement->firstChild->prefix);
/// ```
/// ```expect
/// string(1) "p"
/// string(0) ""
/// ```
fn prefix(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let prefix = match node.kind() {
        Kind::Element | Kind::Attribute | Kind::NamespaceDecl => node.ns().and_then(Ns::prefix),
        _ => None,
    };
    string_or_empty(req, prefix)
}

/// `dom_node_prefix_write`.
fn set_prefix(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let node = prop_node(req, object)?;
    let holder = match node.kind() {
        Kind::Element => Some(node),
        Kind::Attribute => node.parent().or_else(|| node.doc().and_then(Node::root)),
        _ => return Ok(()),
    };
    let given = written_bytes(value);
    let prefix: Option<&[u8]> = if given.is_empty() { None } else { Some(&given) };
    let (Some(holder), Some(current)) = (holder, node.ns()) else {
        return Ok(());
    };
    if current.prefix().as_deref() == prefix {
        return Ok(());
    }
    let href = current.href();
    let invalid = match &href {
        None => true,
        Some(h) => {
            (given == b"xml" && h.as_slice() != crate::document::XML_NS)
                || (node.kind() == Kind::Attribute
                    && given == b"xmlns"
                    && h.as_slice() != crate::document::XMLNS_NS)
                || (node.kind() == Kind::Attribute && node.name() == b"xmlns")
        }
    };
    let Some(href) = href.filter(|_| !invalid) else {
        let strict = crate::strict(req, object);
        return dom_error(req, Code::Namespace, strict);
    };
    let mut found = None;
    let mut cur = holder.ns_def();
    while let Some(ns) = cur {
        if ns.prefix().as_deref() == prefix && ns.href().as_deref() == Some(href.as_slice()) {
            found = Some(ns);
            break;
        }
        cur = ns.next();
    }
    let ns = match found {
        Some(ns) => ns,
        None => match holder.new_ns(&href, prefix) {
            Some(ns) => ns,
            None => return dom_error(req, Code::Namespace, true),
        },
    };
    node.set_ns_call(Some(ns));
    Ok(())
}

/// `dom_node_local_name_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<p:r xmlns:p="urn:p">t</p:r>');
/// var_dump($d->documentElement->localName, $d->documentElement->firstChild->localName);
/// ```
/// ```expect
/// string(1) "r"
/// NULL
/// ```
fn local_name(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    if matches!(
        node.kind(),
        Kind::Element | Kind::Attribute | Kind::NamespaceDecl
    ) {
        return some(string(req, &node.name())?);
    }
    some(Value::null())
}

/// `dom_node_base_uri_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r xml:base="http://e.org/b/"><c/></r>');
/// var_dump($d->documentElement->firstChild->baseURI, (new DOMElement('x'))->baseURI);
/// ```
/// ```expect
/// string(15) "http://e.org/b/"
/// NULL
/// ```
fn base_uri(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    string_or_null(req, node.base().as_deref())
}

/// `dom_node_text_content_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r>a<b>c</b></r>');
/// var_dump($d->textContent, $d->documentElement->textContent);
/// ```
/// ```expect
/// string(2) "ac"
/// string(2) "ac"
/// ```
fn text_content(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    some(content_value(req, node, false)?)
}

/// `dom_node_text_content_write`: a text node in place of the children of
/// a container, the content set on anything else.
fn set_text_content(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let node = prop_node(req, object)?;
    invalidate(req, object);
    let bytes = written_bytes(value);
    if matches!(
        node.kind(),
        Kind::Fragment | Kind::Element | Kind::Attribute
    ) {
        remove_all_children(node);
        if let Some(text) = Node::new_text_len(node.doc(), &bytes) {
            node.add_child(text);
        }
    } else {
        node.set_content(&bytes);
    }
    Ok(())
}

// ---- DOMParentNode

/// `dom_parent_node_first_element_child_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r>t<a/>u<b/>v</r>');
/// $r = $d->documentElement;
/// var_dump($r->firstElementChild->nodeName, $r->lastElementChild->nodeName, $r->childElementCount);
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "b"
/// int(2)
/// ```
fn first_element_child(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let mut at = node.children();
    while let Some(n) = at {
        if n.kind() == Kind::Element {
            break;
        }
        at = n.next();
    }
    some(nullable_object(req, at, Some(object))?)
}

/// `dom_parent_node_last_element_child_read`.
fn last_element_child(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let mut at = node.last();
    while let Some(n) = at {
        if n.kind() == Kind::Element {
            break;
        }
        at = n.prev();
    }
    some(nullable_object(req, at, Some(object))?)
}

/// `dom_parent_node_child_element_count`.
fn child_element_count(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let mut count = 0;
    let mut at = node.children();
    while let Some(n) = at {
        if n.kind() == Kind::Element {
            count += 1;
        }
        at = n.next();
    }
    some(Value::int(count))
}

// ---- DOMCharacterData, DOMText, DOMProcessingInstruction

/// `dom_characterdata_data_read`.
///
/// ```php
/// $t = new DOMText('héllo');
/// var_dump($t->data, $t->length);
/// ```
/// ```expect
/// string(6) "héllo"
/// int(5)
/// ```
fn data(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    some(content_value(req, node, false)?)
}

/// `dom_characterdata_data_write`.
fn set_data(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let node = prop_node(req, object)?;
    node.set_content_len(&written_bytes(value));
    Ok(())
}

/// `dom_characterdata_length_read`: the characters, as libxml counts
/// UTF-8.
fn data_length(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let len = node.content().map_or(0, |c| Utf8::len(&c));
    some(Value::int(len))
}

/// `dom_text_whole_text_read`: the adjacent text and CDATA joined.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r>a<![CDATA[b]]>c<x/>d</r>');
/// var_dump($d->documentElement->childNodes->item(1)->wholeText);
/// ```
/// ```expect
/// string(3) "abc"
/// ```
fn whole_text(req: &mut Request, object: PhpObject) -> Read {
    let mut node = prop_node(req, object)?;
    let is_text = |n: Node| matches!(n.kind(), Kind::Text | Kind::CData);
    while let Some(p) = node.prev().filter(|p| is_text(*p)) {
        node = p;
    }
    let mut out = Vec::new();
    let mut at = Some(node);
    while let Some(n) = at.filter(|n| is_text(*n)) {
        if let Some(c) = n.content() {
            out.extend_from_slice(&c);
        }
        at = n.next();
    }
    some(string(req, &out)?)
}

/// `dom_processinginstruction_target_read`.
///
/// ```php
/// $p = new DOMProcessingInstruction('t', 'd');
/// var_dump($p->target, $p->data);
/// ```
/// ```expect
/// string(1) "t"
/// string(1) "d"
/// ```
fn pi_target(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    some(string(req, &node.name())?)
}

// ---- DOMAttr

/// `dom_attr_name_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r xmlns:p="urn:p" p:a="1"/>');
/// $a = $d->documentElement->attributes->item(0);
/// var_dump($a->name, $a->specified, $a->value, $a->ownerElement->nodeName, $a->schemaTypeInfo);
/// ```
/// ```expect
/// string(1) "a"
/// bool(true)
/// string(1) "1"
/// string(1) "r"
/// NULL
/// ```
fn attr_name(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    some(string(req, &node.name())?)
}

fn attr_specified(_req: &mut Request, _object: PhpObject) -> Read {
    some(Value::bool(true))
}

fn attr_value(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    some(content_value(req, node, false)?)
}

/// `dom_attr_value_write`.
fn set_attr_value(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let node = prop_node(req, object)?;
    attr_value_will_change(req, object, node);
    remove_all_children(node);
    node.set_content_len(&written_bytes(value));
    Ok(())
}

/// `dom_attr_owner_element_read`.
fn owner_element(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    some(nullable_object(req, node.parent(), Some(object))?)
}

// ---- DOMElement

/// `dom_element_tag_name_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<p:r xmlns:p="urn:p" class="c" id="i"/>');
/// $r = $d->documentElement;
/// var_dump($r->tagName, $r->className, $r->id);
/// ```
/// ```expect
/// string(3) "p:r"
/// string(1) "c"
/// string(1) "i"
/// ```
fn tag_name(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    some(string(req, &qualified_name(node))?)
}

fn reflected_read(req: &mut Request, object: PhpObject, name: &[u8]) -> Read {
    let node = prop_node(req, object)?;
    string_or_empty(req, node.get_no_ns_prop(name))
}

fn reflected_write(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
    value: &Value,
) -> Result<Option<Node>, Thrown> {
    let Some(node) = crate::node_of(req, object) else {
        return Err(crate::dom_throw(req, Code::InvalidState));
    };
    Ok(node.set_ns_prop(None, name, &written_bytes(value)))
}

fn class_name(req: &mut Request, object: PhpObject) -> Read {
    reflected_read(req, object, b"class")
}

fn set_class_name(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    reflected_write(req, object, b"class", value).map(|_| ())
}

fn id(req: &mut Request, object: PhpObject) -> Read {
    reflected_read(req, object, b"id")
}

fn set_id(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    if let Some(attr) = reflected_write(req, object, b"id", value)? {
        crate::set_attribute_id(req, object, attr, true);
    }
    Ok(())
}

// ---- DOMDocumentType, DOMNotation, DOMEntity, DOMEntityReference

/// `dom_documenttype_name_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<!DOCTYPE r PUBLIC "-//P" "s.dtd" [<!ENTITY e "v"><!NOTATION n SYSTEM "x">]><r/>');
/// $t = $d->doctype;
/// var_dump($t->name, $t->publicId, $t->systemId, $t->entities->length, $t->notations->length, $t->internalSubset);
/// ```
/// ```expect
/// string(1) "r"
/// string(4) "-//P"
/// string(5) "s.dtd"
/// int(1)
/// int(1)
/// string(16) "<!ENTITY e "v">
/// "
/// ```
fn doctype_name(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    some(string(req, &node.name())?)
}

fn doctype_entities(req: &mut Request, object: PhpObject) -> Read {
    prop_node(req, object)?;
    some(crate::create(
        req,
        object,
        "DOMNamedNodeMap",
        MapKind::Entities,
    )?)
}

fn doctype_notations(req: &mut Request, object: PhpObject) -> Read {
    prop_node(req, object)?;
    some(crate::create(
        req,
        object,
        "DOMNamedNodeMap",
        MapKind::Notations,
    )?)
}

fn doctype_public_id(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    string_or_empty(req, node.dtd_external_id())
}

fn doctype_system_id(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    string_or_empty(req, node.dtd_system_id())
}

/// `dom_documenttype_internal_subset_read`: each declaration of the
/// document's internal subset dumped bare.
fn internal_subset(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let Some(subset) = node.doc().and_then(Node::int_subset) else {
        return some(Value::null());
    };
    let mut out: Option<Vec<u8>> = None;
    let mut at = subset.children();
    while let Some(n) = at {
        if let Some(bytes) = Save::bare(n) {
            out.get_or_insert_with(Vec::new).extend_from_slice(&bytes);
        }
        at = n.next();
    }
    match out.filter(|o| !o.is_empty()) {
        Some(bytes) => some(string(req, &bytes)?),
        None => some(Value::null()),
    }
}

fn notation_public_id(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    string_or_empty(req, node.entity_external_id())
}

fn notation_system_id(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    string_or_empty(req, node.entity_system_id())
}

fn unparsed(node: Node) -> bool {
    node.entity_type() == XML_EXTERNAL_GENERAL_UNPARSED_ENTITY
}

/// `dom_entity_public_id_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<!DOCTYPE r [<!NOTATION n SYSTEM "x"><!ENTITY u PUBLIC "p" "s" NDATA n><!ENTITY e "v">]><r/>');
/// $u = $d->doctype->entities->getNamedItem('u');
/// $e = $d->doctype->entities->getNamedItem('e');
/// var_dump($u->publicId, $u->systemId, $u->notationName, $e->publicId, $e->notationName);
/// ```
/// ```expect
/// string(1) "p"
/// string(1) "s"
/// string(1) "n"
/// NULL
/// NULL
/// ```
fn entity_public_id(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let id = if unparsed(node) {
        node.entity_external_id()
    } else {
        None
    };
    string_or_null(req, id.as_deref())
}

fn entity_system_id(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    if !unparsed(node) {
        return some(Value::null());
    }
    string_or_empty(req, node.entity_system_id())
}

fn entity_notation_name(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    if !unparsed(node) {
        return some(Value::null());
    }
    string_or_empty(req, node.entity_content())
}

fn entity_actual_encoding(req: &mut Request, _object: PhpObject) -> Read {
    deprecated_property(req, "Property DOMEntity::$actualEncoding is deprecated");
    some(Value::null())
}

fn entity_encoding(req: &mut Request, _object: PhpObject) -> Read {
    deprecated_property(req, "Property DOMEntity::$encoding is deprecated");
    some(Value::null())
}

fn entity_version(req: &mut Request, _object: PhpObject) -> Read {
    deprecated_property(req, "Property DOMEntity::$version is deprecated");
    some(Value::null())
}

/// `dom_entity_reference_child_read`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<!DOCTYPE r [<!ENTITY e "v">]><r>&e;</r>');
/// $ref = $d->documentElement->firstChild;
/// var_dump(get_class($ref), $ref->firstChild->nodeName, $ref->textContent, $ref->childNodes->length);
/// ```
/// ```expect
/// string(18) "DOMEntityReference"
/// string(1) "e"
/// string(1) "v"
/// int(1)
/// ```
fn entity_reference_child(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    let entity = sync_entity_reference(node);
    some(nullable_object(req, entity, Some(object))?)
}

fn entity_reference_text_content(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    sync_entity_reference(node);
    text_content(req, object)
}

fn entity_reference_child_nodes(req: &mut Request, object: PhpObject) -> Read {
    let node = prop_node(req, object)?;
    sync_entity_reference(node);
    child_nodes(req, object)
}
