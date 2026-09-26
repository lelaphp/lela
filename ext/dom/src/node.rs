//! `DOMNode` and `Dom\Node` (`node.c`): their methods, the insertion
//! checks of `php_dom.c` the other classes share, and what the `Dom\`
//! classes do where php's code asks `php_dom_follow_spec_intern`: the
//! cloning through the namespace mapper, the standard's namespace lookups
//! and normalisation, and canonicalisation over `xmlns` attributes turned
//! back into declarations for the length of the call.

use php_ext::coerce::value_name;
use php_ext::raising::{Level, Thrown, arg_value_error, argument_error, docref, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, Value};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi::{C14nOut, C14nSelect, C14nTarget, Kind, Node, Ns, Object, Save, XPath};

use crate::document::{XML_NS, XMLNS_NS};
use crate::modern::follows_spec;
use crate::object::Code;
use crate::parent::{is_parent_invalid, replace_validity};
use crate::private::{Mapper, Private};
use crate::{
    Entry, arg, arity, array_get, bool_arg, bytes_arg, children_valid, class_name, clone_node,
    copy_document_ref, dom_error, dom_error_message, fetch, has_feature, hierarchy_ok, instance_of,
    invalidate, is_equal_attr, is_equal_node, is_read_only, normalize, nullable_bytes_arg,
    nullable_object, object_arg, object_for, path_arg, reconcile_attribute_namespace, reconcile_ns,
    reconcile_ns_list, set_document_ref_pointers, shared_doc, strict, string, this, this_node,
};

/// `dom_insert_fragment`: the fragment's children moved in between
/// `prev` and `next` under `parent`, the first of them answered.
fn insert_fragment(
    parent: Node,
    prev: Option<Node>,
    next: Option<Node>,
    fragment: Node,
) -> Option<Node> {
    let first = fragment.children()?;
    let last = fragment.last()?;
    match prev {
        None => parent.set_children(Some(first)),
        Some(p) => p.set_next(Some(first)),
    }
    first.set_prev(prev);
    match next {
        None => parent.set_last(Some(last)),
        Some(n) => {
            last.set_next(Some(n));
            n.set_prev(Some(last));
        }
    }
    let mut at = Some(first);
    while let Some(n) = at {
        n.set_parent(Some(parent));
        if n == last {
            break;
        }
        at = n.next();
    }
    fragment.set_children(None);
    fragment.set_last(None);
    Some(first)
}

/// `dom_node_check_legacy_insertion_validity`.
fn check_legacy_insertion(
    req: &mut Request,
    parent: Node,
    child: Node,
    strict: bool,
    warn_empty_fragment: bool,
) -> Result<bool, Thrown> {
    let code = if is_read_only(parent) || child.parent().is_some_and(is_read_only) {
        Some(Code::NoModificationAllowed)
    } else if !hierarchy_ok(parent, child) {
        Some(Code::HierarchyRequest)
    } else if child.doc() != parent.doc() && child.doc().is_some() {
        Some(Code::WrongDocument)
    } else {
        None
    };
    if let Some(code) = code {
        dom_error(req, code, strict)?;
        return Ok(false);
    }
    if warn_empty_fragment && child.kind() == Kind::Fragment && child.children().is_none() {
        docref(req, Level::Warning, "Document Fragment is empty");
        return Ok(false);
    }
    let bad = (parent.kind() == Kind::Attribute
        && !matches!(child.kind(), Kind::Text | Kind::EntityRef))
        || (child.kind() == Kind::Attribute && parent.kind() != Kind::Element)
        || child.kind().is_document();
    if bad {
        dom_error(req, Code::HierarchyRequest, strict)?;
        return Ok(false);
    }
    Ok(true)
}

/// An attribute of the same name `parent` already has, which `appendChild`
/// and `insertBefore` replace.
fn existing_attr(parent: Node, child: Node) -> Option<Node> {
    match child.ns().and_then(Ns::href) {
        None => parent.has_prop(&child.name()),
        Some(href) => parent.has_ns_prop(&child.name(), Some(&href)),
    }
    .filter(|a| a.kind() != Kind::AttributeDecl)
}

/// `dom_node_replace_child`: the standard's checks where `modern`, the
/// legacy ones otherwise, and the namespaces reconciled only for the
/// latter.
fn replace_child_common(req: &mut Request, modern: bool) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let (this, parent) = this_node(req)?;
    let (Some(new_obj), Some(old_obj)) = (object_arg(req, 1)?, object_arg(req, 2)?) else {
        return Ok(Value::bool(false));
    };
    let new_child = fetch(req, new_obj)?;
    let old_child = fetch(req, old_obj)?;
    let strict = strict(req, this);
    if new_child.doc() != parent.doc() && new_child.doc().is_some() {
        dom_error(req, Code::WrongDocument, strict)?;
        return Ok(Value::bool(false));
    }
    if modern {
        replace_validity(req, parent, new_child, old_child)?;
    } else {
        if !children_valid(parent) || parent.children().is_none() {
            return Ok(Value::bool(false));
        }
        if !check_legacy_insertion(req, parent, new_child, strict, false)? {
            return Ok(Value::bool(false));
        }
        if (old_child.kind() == Kind::Attribute) != (new_child.kind() == Kind::Attribute) {
            dom_error(req, Code::HierarchyRequest, strict)?;
            return Ok(Value::bool(false));
        }
        if old_child.parent() != Some(parent) {
            dom_error(req, Code::NotFound, strict)?;
            return Ok(Value::bool(false));
        }
    }
    if new_child.doc().is_none() && parent.doc().is_some() {
        new_child.set_tree_doc(parent.doc());
        set_document_ref_pointers(new_child, shared_doc(req, this));
    }
    if new_child.kind() == Kind::Fragment {
        let prev = old_child.prev();
        let next = old_child.next();
        old_child.unlink();
        let last = new_child.last();
        if let (Some(first), Some(last)) = (insert_fragment(parent, prev, next, new_child), last)
            && !modern
        {
            reconcile_ns_list(parent.doc(), first, last);
        }
    } else if old_child != new_child {
        let doc = parent.doc();
        let replaced_doctype = doc.and_then(Node::int_subset) == Some(old_child);
        old_child.replace_with(new_child);
        if !modern {
            reconcile_ns(doc, new_child);
        }
        if replaced_doctype && let Some(d) = doc {
            d.set_int_subset(Some(new_child));
        }
    }
    invalidate(req, this);
    object_for(req, old_child, Some(this))
}

/// `DOMNode::replaceChild(DOMNode $node, DOMNode $child): DOMNode|false`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/><b/></r>');
/// $r = $d->documentElement;
/// $first = $r->firstChild;
/// $old = $r->replaceChild($d->createElement('x'), $first);
/// echo $old->nodeName, ' ', $d->saveXML($r), "\n";
/// ```
/// ```expect
/// a <r><x/><b/></r>
/// ```
fn replace_child(req: &mut Request) -> Result<Value, Thrown> {
    replace_child_common(req, false)
}

/// `Dom\Node::replaceChild(Dom\Node $node, Dom\Node $child): Dom\Node`.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<!DOCTYPE r><r><a/><b/></r>');
/// $r = $d->documentElement;
/// $a = $r->firstChild;
/// $b = $r->lastChild;
/// $old = $r->replaceChild($b, $a);
/// echo $old->nodeName, ' ', $r->childNodes->length, ' ', $r->firstChild->nodeName, "\n";
/// $t = $d->createTextNode('t');
/// $c = $b->cloneNode();
/// $dt = $d->doctype;
/// foreach ([fn() => $d->replaceChild($t, $r), fn() => $d->replaceChild($c, $dt), fn() => $r->replaceChild($r, $b)] as $f) {
///     try { $f(); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// a 1 b
/// Cannot insert text as a child of a document
/// Cannot have more than one element child in a document
/// Hierarchy Request Error
/// ```
fn modern_replace_child(req: &mut Request) -> Result<Value, Thrown> {
    replace_child_common(req, true)
}

/// `DOMNode::hasChildNodes(): bool`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/></r>');
/// var_dump($d->documentElement->hasChildNodes(), $d->documentElement->firstChild->hasChildNodes());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
fn has_child_nodes(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (_, node) = this_node(req)?;
    Ok(Value::bool(
        children_valid(node) && node.children().is_some(),
    ))
}

// ---- cloning through the namespace mapper

/// `dom_clone_container_helper`: the node alone, an element with its
/// declarations turned into `xmlns` attributes and its attributes copied,
/// every namespace the original's until reconciliation.
fn clone_container(mapper: &mut Mapper, src: Node, doc: Option<Node>) -> Option<Node> {
    let clone = src.copy(doc, 0)?;
    clone.set_ns(src.ns());
    if src.kind() == Kind::Element {
        let mut last_added = None;
        if let Some(first) = src.ns_def() {
            let mut at = Some(first);
            while let Some(ns) = at {
                mapper.mark_attribute(clone, ns);
                at = ns.next();
            }
            last_added = clone.properties();
            while let Some(next) = last_added.and_then(Node::next) {
                last_added = Some(next);
            }
        }
        let mut attr = src.properties();
        while let Some(a) = attr {
            let Some(copy) = a.copy(doc, 0) else {
                clone.free();
                return None;
            };
            match last_added {
                None => clone.set_properties(Some(copy)),
                Some(last) => {
                    copy.set_prev(Some(last));
                    last.set_next(Some(copy));
                }
            }
            copy.set_parent(Some(clone));
            last_added = Some(copy);
            copy.set_ns(a.ns());
            attr = a.next();
        }
    }
    Some(clone)
}

/// `dom_clone_helper`: the node, and with `recursive` what hangs from an
/// element, a fragment or a document, copied in tree order; a document's
/// internal subset copied once, as the subset it is.
fn clone_helper(
    mapper: &mut Mapper,
    src: Node,
    doc: Option<Node>,
    recursive: bool,
) -> Option<Node> {
    let outer = clone_container(mapper, src, doc)?;
    if !recursive
        || !matches!(
            src.kind(),
            Kind::Element | Kind::Fragment | Kind::Document | Kind::HtmlDocument
        )
    {
        return Some(outer);
    }
    let mut dst_doc = doc;
    if src.kind().is_document() {
        dst_doc = Some(outer);
        if let Some(subset) = src.int_subset() {
            let Some(copy) = subset.copy_dtd(Some(outer)) else {
                outer.free();
                return None;
            };
            outer.set_int_subset(Some(copy));
            copy.set_parent(Some(outer));
            outer.set_children(Some(copy));
            outer.set_last(Some(copy));
        }
    }
    let mut cloned_parent = outer;
    let mut at = src.children();
    while let Some(n) = at {
        let cloned = match n.kind() {
            Kind::Element => clone_container(mapper, n, dst_doc),
            Kind::Dtd => None,
            _ => n.copy(dst_doc, 1),
        };
        if let Some(c) = cloned {
            match cloned_parent.last() {
                None => cloned_parent.set_children(Some(c)),
                Some(last) => {
                    c.set_prev(Some(last));
                    last.set_next(Some(c));
                }
            }
            c.set_parent(Some(cloned_parent));
            cloned_parent.set_last(Some(c));
        }
        if n.kind() == Kind::Element
            && let (Some(child), Some(c)) = (n.children(), cloned)
        {
            cloned_parent = c;
            at = Some(child);
            continue;
        }
        if let Some(next) = n.next() {
            at = Some(next);
            continue;
        }
        let mut up = n;
        at = loop {
            let Some(p) = up.parent() else {
                return Some(outer);
            };
            if p == src {
                return Some(outer);
            }
            if let Some(cp) = cloned_parent.parent() {
                cloned_parent = cp;
            }
            if let Some(next) = p.next() {
                break Some(next);
            }
            up = p;
        };
    }
    Some(outer)
}

/// `dom_clone_node` with a namespace mapper, as a document that follows
/// the standard clones: a subtree that lands in another document (a
/// document's own copy) pointed at the mapper's declarations. The legacy
/// clone is [`crate::clone_node`].
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r xmlns="urn:r" xmlns:p="urn:p"><p:a p:x="1">t<b/></p:a></r>');
/// $a = $d->documentElement->firstChild;
/// $copy = clone $a;
/// var_dump($copy === $a, $copy->nodeName, $copy->namespaceURI, $copy->attributes->length, $copy->childNodes->length, $copy->lastChild->namespaceURI, $copy->parentNode, $copy->ownerDocument === $d);
/// $doc = clone $d;
/// $e = $doc->documentElement;
/// var_dump(get_class($doc), $doc === $d, $e->attributes->length, $e->firstChild->prefix, $e->firstChild->isEqualNode($a));
/// ```
/// ```expect
/// bool(false)
/// string(3) "p:a"
/// string(5) "urn:p"
/// int(1)
/// int(2)
/// string(5) "urn:r"
/// NULL
/// bool(true)
/// string(15) "Dom\XMLDocument"
/// bool(false)
/// int(2)
/// string(1) "p"
/// bool(true)
/// ```
pub(crate) fn clone_node_modern(
    mapper: &mut Mapper,
    node: Node,
    doc: Option<Node>,
    recursive: bool,
) -> Option<Node> {
    if node.kind() == Kind::Dtd {
        return node.copy_dtd(doc);
    }
    let clone = clone_helper(mapper, node, doc, recursive)?;
    if clone.doc() != node.doc() {
        let doc = clone.doc();
        if matches!(
            clone.kind(),
            Kind::Document | Kind::HtmlDocument | Kind::Fragment
        ) {
            let mut at = clone.children();
            while let Some(child) = at {
                mapper.reconcile(doc, child);
                at = child.next();
            }
        } else {
            mapper.reconcile(doc, clone);
        }
    }
    Some(clone)
}

/// The document of the `Dom\` classes a document node's copy is handed out
/// as.
fn modern_document_class(doc: Node) -> &'static str {
    if doc.kind() == Kind::HtmlDocument {
        "Dom\\HTMLDocument"
    } else {
        "Dom\\XMLDocument"
    }
}

/// `DOMNode::cloneNode(bool $deep = false): DOMNode|false`, and
/// `Dom\Node::cloneNode`, which clones through the document's namespace
/// mapper and gives a copied document private data of its own.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r a="1"><c/></r>');
/// $r = $d->documentElement;
/// echo $d->saveXML($r->cloneNode()), ' ', $d->saveXML($r->cloneNode(true)), "\n";
/// $copy = $d->cloneNode(true);
/// var_dump($copy === $d, $copy->saveXML() === $d->saveXML());
/// $m = Dom\XMLDocument::createFromString('<!DOCTYPE r><r xmlns:p="urn:p" p:a="1"><p:c>t</p:c></r>');
/// $copy = $m->cloneNode(true);
/// $e = $copy->documentElement;
/// var_dump(get_class($copy), $copy->doctype->name, $e->attributes->length, $e->firstChild->namespaceURI, $e->textContent);
/// $shallow = $m->documentElement->cloneNode();
/// var_dump(get_class($shallow), $shallow->attributes->length, $shallow->hasChildNodes(), $shallow->ownerDocument === $m);
/// var_dump(get_class($m->cloneNode()), $m->cloneNode()->hasChildNodes());
/// ```
/// ```expect
/// <r a="1"/> <r a="1"><c/></r>
/// bool(false)
/// bool(true)
/// string(15) "Dom\XMLDocument"
/// string(1) "r"
/// int(2)
/// string(5) "urn:p"
/// string(1) "t"
/// string(11) "Dom\Element"
/// int(2)
/// bool(false)
/// bool(true)
/// string(15) "Dom\XMLDocument"
/// bool(false)
/// ```
fn clone_node_method(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 1)?;
    let recursive = bool_arg(req, 1, false)?;
    let (this, n) = this_node(req)?;
    let is_document = n.kind().is_document();
    if follows_spec(req, this) {
        if is_document {
            let mut private = Private::default();
            let Some(node) = clone_node_modern(&mut private.mapper, n, n.doc(), recursive) else {
                return Ok(Value::bool(false));
            };
            let copy = crate::modern::document_object(req, modern_document_class(n), node)?;
            copy_document_ref(req, this, copy);
            if let Some(d) = crate::dom_mut(req, copy) {
                d.obj.set_private_data(Box::new(private));
            }
            return Ok(copy.as_value());
        }
        let cloned = match crate::dom_mut(req, this).and_then(|d| crate::private::of(&mut d.obj)) {
            Some(private) => clone_node_modern(&mut private.mapper, n, n.doc(), recursive),
            None => clone_node(n, n.doc(), recursive),
        };
        let Some(node) = cloned else {
            return Ok(Value::bool(false));
        };
        return cloned_object(req, this, n, node);
    }
    let Some(node) = clone_node(n, n.doc(), recursive) else {
        return Ok(Value::bool(false));
    };
    if is_document {
        let value = object_for(req, node, None)?;
        if let Some(copy) = value.as_object().copied() {
            copy_document_ref(req, this, copy);
        }
        return Ok(value);
    }
    cloned_object(req, this, n, node)
}

/// The object for a node's copy: a copied attribute keeps the original's
/// namespace for reconciliation to settle, which libxml does not dare set.
fn cloned_object(
    req: &mut Request,
    this: php_ext::values::PhpObject,
    original: Node,
    node: Node,
) -> Result<Value, Thrown> {
    if node.kind() == Kind::Attribute && original.ns().is_some() && node.ns().is_none() {
        node.set_ns(original.ns());
    }
    object_for(req, node, Some(this))
}

/// `DOMNode::normalize(): void`: `php_dom_normalize_modern` for a
/// document that follows the standard, `php_dom_normalize_legacy`
/// otherwise, which also normalises attributes' children.
///
/// ```php
/// $d = new DOMDocument;
/// $r = $d->appendChild($d->createElement('r'));
/// $r->appendChild($d->createTextNode('a'));
/// $r->appendChild($d->createElement('x'));
/// $last = $r->lastChild;
/// $r->insertBefore($d->createTextNode('b'), $last);
/// $r->normalize();
/// var_dump($r->childNodes->length, $r->firstChild->data);
/// $m = Dom\XMLDocument::createFromString('<r>a<x/></r>');
/// $r = $m->documentElement;
/// $a = $r->firstChild;
/// $x = $r->lastChild;
/// $r->insertBefore($m->createTextNode(''), $a);
/// $r->insertBefore($m->createTextNode('b'), $x);
/// $r->insertBefore($m->createTextNode('c'), $x);
/// var_dump($r->childNodes->length);
/// $r->normalize();
/// var_dump($r->childNodes->length, $r->firstChild->data);
/// ```
/// ```expect
/// int(2)
/// string(2) "ab"
/// int(5)
/// int(2)
/// string(3) "abc"
/// ```
fn normalize_method(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (this, node) = this_node(req)?;
    if follows_spec(req, this) {
        normalize_modern(node);
    } else {
        normalize(node);
    }
    Ok(Value::null())
}

/// `php_dom_normalize_modern`: every descendant text node that is empty
/// removed, and the text nodes that follow one merged into it.
fn normalize_modern(this: Node) {
    let mut at = this.children();
    while let Some(node) = at {
        match node.kind() {
            Kind::Text => {
                if node.content().is_none_or(|c| c.is_empty()) {
                    let next = node.next();
                    node.unlink();
                    if !node.is_named() {
                        node.free();
                    }
                    at = next;
                    continue;
                }
                let mut next = node.next();
                while let Some(n) = next.filter(|n| n.kind() == Kind::Text) {
                    let after = n.next();
                    if let Some(content) = n.content() {
                        node.add_content(&content);
                    }
                    n.unlink();
                    if !n.is_named() {
                        n.free();
                    }
                    next = after;
                }
            }
            Kind::Element => normalize_modern(node),
            _ => {}
        }
        at = node.next();
    }
}

/// `DOMNode::isSupported(string $feature, string $version): bool`.
///
/// ```php
/// $d = new DOMDocument;
/// var_dump($d->isSupported('xml', '2.0'), $d->isSupported('Core', '2.0'), $d->isSupported('Events', '1.0'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
fn is_supported(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let feature = bytes_arg(req, 1)?;
    let version = bytes_arg(req, 2)?;
    Ok(Value::bool(has_feature(&feature, &version)))
}

/// `DOMNode::hasAttributes(): bool`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r a="1"><c/></r>');
/// var_dump($d->documentElement->hasAttributes(), $d->documentElement->firstChild->hasAttributes());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
fn has_attributes(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (_, node) = this_node(req)?;
    Ok(Value::bool(
        node.kind() == Kind::Element && node.properties().is_some(),
    ))
}

/// `DOMNode::isSameNode(DOMNode $otherNode): bool`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r/>');
/// $root = $d->documentElement;
/// $first = $d->firstChild;
/// var_dump($root->isSameNode($first), $d->isSameNode($root));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
fn is_same_node(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let (_, node) = this_node(req)?;
    let Some(other) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let other = fetch(req, other)?;
    Ok(Value::bool(node == other))
}

/// `dom_node_is_equal_node_common`: the standard's equality where
/// `modern`, which leaves namespace declarations out.
fn is_equal_node_common(req: &mut Request, modern: bool) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let Some(other) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let other = fetch(req, other)?;
    let (_, node) = this_node(req)?;
    if node == other {
        return Ok(Value::bool(true));
    }
    Ok(Value::bool(is_equal_node(node, other, modern)))
}

/// `DOMNode::isEqualNode(?DOMNode $otherNode): bool`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a x="1">t</a><a x="1">t</a><a x="2">t</a></r>');
/// $all = iterator_to_array($d->getElementsByTagName('a'));
/// $a = $all[0];
/// $b = $all[1];
/// $c = $all[2];
/// var_dump($a->isEqualNode($b), $a->isEqualNode($c), $a->isEqualNode(null));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
fn is_equal_node_method(req: &mut Request) -> Result<Value, Thrown> {
    is_equal_node_common(req, false)
}

/// `Dom\Node::isEqualNode(?Dom\Node $otherNode): bool`.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r><a xmlns:p="urn:p" x="1">t</a><a x="1">t</a><a x="1" xmlns:p="urn:p">t</a></r>');
/// $r = $d->documentElement;
/// $a = $r->childNodes[0];
/// $b = $r->childNodes[1];
/// $c = $r->childNodes[2];
/// $copy = $d->cloneNode(true);
/// var_dump($a->isEqualNode($b), $a->isEqualNode($c), $a->isEqualNode(null), $d->isEqualNode($copy));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
fn modern_is_equal_node(req: &mut Request) -> Result<Value, Thrown> {
    is_equal_node_common(req, true)
}

/// The node `lookupPrefix` starts from: an element itself, a document's
/// element, nothing for the kinds that have no element to ask, the
/// parent otherwise.
fn prefix_lookup_start(node: Node) -> Option<Node> {
    match node.kind() {
        Kind::Element => Some(node),
        Kind::Document | Kind::HtmlDocument => node.root(),
        Kind::Entity | Kind::Notation | Kind::Fragment | Kind::DocumentType | Kind::Dtd => None,
        _ => node.parent(),
    }
}

/// `dom_locate_a_namespace_prefix`: the prefix of `uri` on the element,
/// an `xmlns` attribute's local name, or the same asked of the parent
/// element.
fn locate_namespace_prefix(mut elem: Node, uri: &[u8]) -> Option<Vec<u8>> {
    loop {
        if let Some(ns) = elem.ns()
            && let Some(prefix) = ns.prefix()
            && ns.href().as_deref() == Some(uri)
        {
            return Some(prefix);
        }
        let mut attr = elem.properties();
        while let Some(a) = attr {
            if let Some(ns) = a.ns()
                && let Some(child) = a.children()
                && ns.prefix().as_deref() == Some(b"xmlns".as_slice())
                && child.content().as_deref() == Some(uri)
            {
                return Some(a.name());
            }
            attr = a.next();
        }
        match elem.parent() {
            Some(p) if p.kind() == Kind::Element => elem = p,
            _ => return None,
        }
    }
}

/// `dom_node_lookup_prefix`: libxml's search by href where the legacy
/// classes ask, the standard's lookup where `modern`.
fn lookup_prefix_common(req: &mut Request, modern: bool) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let uri = nullable_bytes_arg(req, 1)?.unwrap_or_default();
    let (_, node) = this_node(req)?;
    if uri.is_empty() {
        return Ok(Value::null());
    }
    let Some(lookup) = prefix_lookup_start(node) else {
        return Ok(Value::null());
    };
    let found = if modern {
        locate_namespace_prefix(lookup, &uri)
    } else {
        lookup
            .search_ns_by_href(lookup.doc(), &uri)
            .and_then(Ns::prefix)
    };
    match found {
        Some(prefix) => string(req, &prefix),
        None => Ok(Value::null()),
    }
}

/// `DOMNode::lookupPrefix(string $namespace): ?string`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r xmlns:p="urn:p"><c/></r>');
/// var_dump($d->documentElement->firstChild->lookupPrefix('urn:p'), $d->lookupPrefix('urn:x'), $d->lookupPrefix(''));
/// ```
/// ```expect
/// string(1) "p"
/// NULL
/// NULL
/// ```
fn lookup_prefix(req: &mut Request) -> Result<Value, Thrown> {
    lookup_prefix_common(req, false)
}

/// `Dom\Node::lookupPrefix(?string $namespace): ?string`.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p" xmlns:q="urn:q"><q:c a="1"/></r>');
/// $c = $d->documentElement->firstChild;
/// $a = $c->attributes[0];
/// var_dump($c->lookupPrefix('urn:p'), $c->lookupPrefix('urn:q'), $a->lookupPrefix('urn:p'), $d->lookupPrefix(null), $d->lookupPrefix('urn:x'));
/// ```
/// ```expect
/// string(1) "p"
/// string(1) "q"
/// string(1) "p"
/// NULL
/// NULL
/// ```
fn modern_lookup_prefix(req: &mut Request) -> Result<Value, Thrown> {
    lookup_prefix_common(req, true)
}

/// `DOMNode::isDefaultNamespace(string $namespace): bool`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r xmlns="urn:d"><c/></r>');
/// var_dump($d->isDefaultNamespace('urn:d'), $d->documentElement->firstChild->isDefaultNamespace('urn:x'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
fn is_default_namespace(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let uri = bytes_arg(req, 1)?;
    let (_, mut node) = this_node(req)?;
    if uri.is_empty() {
        return Ok(Value::bool(false));
    }
    if node.kind().is_document() {
        match node.root() {
            Some(r) => node = r,
            None => return Ok(Value::bool(false)),
        }
    }
    let found = node.search_ns(None).and_then(Ns::href);
    let uri = &uri[..uri.iter().position(|b| *b == 0).unwrap_or(uri.len())];
    Ok(Value::bool(found.as_deref() == Some(uri)))
}

/// `Dom\Node::isDefaultNamespace(?string $namespace): bool`: the default
/// namespace the standard locates, null and the empty string alike.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r xmlns="urn:d"><c xmlns=""/></r>');
/// $c = $d->documentElement->firstChild;
/// var_dump($d->isDefaultNamespace('urn:d'), $c->isDefaultNamespace(null), $c->isDefaultNamespace(''), $c->isDefaultNamespace('urn:d'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
fn modern_is_default_namespace(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let uri = nullable_bytes_arg(req, 1)?.filter(|u| !u.is_empty());
    let (_, node) = this_node(req)?;
    let located = locate_namespace(node, None);
    let uri = uri
        .as_deref()
        .map(|u| &u[..u.iter().position(|b| *b == 0).unwrap_or(u.len())]);
    Ok(Value::bool(located.as_deref() == uri))
}

/// `dom_locate_a_namespace`: the namespace `prefix` stands for where the
/// node is, the standard's walk over elements and their `xmlns`
/// attributes.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r xmlns="urn:d" xmlns:p="urn:p"><c xmlns=""><?pi x?></c></r>');
/// $c = $d->documentElement->firstChild;
/// $pi = $c->firstChild;
/// $attr = $d->documentElement->attributes[1];
/// var_dump($c->lookupNamespaceURI(null), $pi->lookupNamespaceURI('p'), $attr->lookupNamespaceURI('p'), $d->lookupNamespaceURI(null), $c->lookupNamespaceURI('xmlns'));
/// ```
/// ```expect
/// NULL
/// string(5) "urn:p"
/// string(5) "urn:p"
/// string(5) "urn:d"
/// string(29) "http://www.w3.org/2000/xmlns/"
/// ```
pub(crate) fn locate_namespace(node: Node, prefix: Option<&[u8]>) -> Option<Vec<u8>> {
    match node.kind() {
        Kind::Element => {
            if let Some(p) = prefix {
                if p.eq_ignore_ascii_case(b"xml") {
                    return Some(XML_NS.to_vec());
                }
                if p.eq_ignore_ascii_case(b"xmlns") {
                    return Some(XMLNS_NS.to_vec());
                }
            }
            let mut at = node;
            loop {
                if let Some(ns) = at.ns()
                    && ns.prefix().as_deref() == prefix
                {
                    return ns.href();
                }
                let mut attr = at.properties();
                while let Some(a) = attr {
                    let declares = a.ns().filter(|ns| ns.href().as_deref() == Some(XMLNS_NS));
                    if let Some(ns) = declares {
                        let name = a.name();
                        let hit = match prefix {
                            Some(p) => {
                                ns.prefix().as_deref() == Some(b"xmlns".as_slice()) && name == p
                            }
                            None => ns.prefix().is_none() && name == b"xmlns",
                        };
                        if hit {
                            return a
                                .children()
                                .and_then(Node::content)
                                .filter(|c| !c.is_empty());
                        }
                    }
                    attr = a.next();
                }
                match at.parent() {
                    Some(p) if p.kind() == Kind::Element => at = p,
                    _ => return None,
                }
            }
        }
        Kind::Document | Kind::HtmlDocument => locate_namespace(node.root()?, prefix),
        Kind::Dtd | Kind::Fragment => None,
        _ => match node.parent() {
            Some(p) if p.kind() == Kind::Element => locate_namespace(p, prefix),
            _ => None,
        },
    }
}

/// `DOMNode::lookupNamespaceURI(?string $prefix): ?string`: libxml's
/// search for the legacy classes, the standard's locate for a document
/// that follows it, where the empty prefix is none.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r xmlns="urn:d" xmlns:p="urn:p"><c/></r>');
/// $c = $d->documentElement->firstChild;
/// var_dump($c->lookupNamespaceURI('p'), $c->lookupNamespaceURI(null), $d->lookupNamespaceURI('q'));
/// $m = Dom\XMLDocument::createFromString('<r xmlns="urn:d" xmlns:p="urn:p"><c/></r>');
/// $c = $m->documentElement->firstChild;
/// var_dump($c->lookupNamespaceURI('p'), $c->lookupNamespaceURI(''), $c->lookupNamespaceURI('q'), $c->lookupNamespaceURI('XML'), $m->lookupNamespaceURI('xmlns'));
/// ```
/// ```expect
/// string(5) "urn:p"
/// string(5) "urn:d"
/// NULL
/// string(5) "urn:p"
/// string(5) "urn:d"
/// NULL
/// string(36) "http://www.w3.org/XML/1998/namespace"
/// string(29) "http://www.w3.org/2000/xmlns/"
/// ```
fn lookup_namespace_uri(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let prefix = nullable_bytes_arg(req, 1)?;
    let (this, mut node) = this_node(req)?;
    if follows_spec(req, this) {
        let prefix = prefix.filter(|p| !p.is_empty());
        return match locate_namespace(node, prefix.as_deref()) {
            Some(uri) => string(req, &uri),
            None => Ok(Value::null()),
        };
    }
    if node.kind().is_document() {
        match node.root() {
            Some(r) => node = r,
            None => return Ok(Value::null()),
        }
    }
    match node.search_ns(prefix.as_deref()).and_then(Ns::href) {
        Some(href) => string(req, &href),
        None => Ok(Value::null()),
    }
}

/// `dom_node_get_node_path`: `null` where libxml answers none, thrown as
/// an invalid state where `throw`.
fn get_node_path_common(req: &mut Request, throw: bool) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (_, node) = this_node(req)?;
    match node.path() {
        Some(p) => string(req, &p),
        None if throw => Err(crate::dom_throw(req, Code::InvalidState)),
        None => Ok(Value::null()),
    }
}

/// `DOMNode::getNodePath(): ?string`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/><a><b/></a></r>');
/// echo $d->getElementsByTagName('b')->item(0)->getNodePath(), "\n";
/// ```
/// ```expect
/// /r/a[2]/b
/// ```
fn get_node_path(req: &mut Request) -> Result<Value, Thrown> {
    get_node_path_common(req, false)
}

/// `Dom\Node::getNodePath(): string`.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p"><a/><p:a><b x="1"/></p:a></r>');
/// $b = $d->documentElement->lastChild->firstChild;
/// echo $b->getNodePath(), ' ', $b->attributes[0]->getNodePath(), ' ', $d->getNodePath(), "\n";
/// ```
/// ```expect
/// /r/p:a/b /r/p:a/b/@x /
/// ```
fn modern_get_node_path(req: &mut Request) -> Result<Value, Thrown> {
    get_node_path_common(req, true)
}

/// `DOMNode::getLineNo(): int`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML("<r>\n<a/>\n</r>");
/// var_dump($d->getElementsByTagName('a')->item(0)->getLineNo());
/// ```
/// ```expect
/// int(2)
/// ```
fn get_line_no(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (_, node) = this_node(req)?;
    Ok(Value::int(node.line_no()))
}

/// `DOMNode::getRootNode(?array $options = null): DOMNode`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/></r>');
/// var_dump($d->documentElement->firstChild->getRootNode() === $d);
/// $e = $d->createElement('x');
/// var_dump($e->getRootNode() === $e);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
fn get_root_node(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 1)?;
    let _ = arg(req, 1)?;
    let (this, mut node) = this_node(req)?;
    while let Some(p) = node.parent() {
        node = p;
    }
    object_for(req, node, Some(this))
}

/// `DOMNode::compareDocumentPosition(DOMNode $other): int`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/><b/></r>');
/// $a = $d->documentElement->firstChild;
/// $b = $a->nextSibling;
/// var_dump($a->compareDocumentPosition($b), $b->compareDocumentPosition($a), $d->documentElement->compareDocumentPosition($a), $a->compareDocumentPosition($a));
/// ```
/// ```expect
/// int(4)
/// int(2)
/// int(20)
/// int(0)
/// ```
#[allow(clippy::too_many_lines)] // php's function, one step per clause of the specification
fn compare_document_position(req: &mut Request) -> Result<Value, Thrown> {
    const DISCONNECTED: i64 = 0x01;
    const PRECEDING: i64 = 0x02;
    const FOLLOWING: i64 = 0x04;
    const CONTAINS: i64 = 0x08;
    const CONTAINED_BY: i64 = 0x10;
    const IMPLEMENTATION_SPECIFIC: i64 = 0x20;
    arity(req, 1, 1)?;
    let (this_obj, this) = this_node(req)?;
    let Some(other_obj) = object_arg(req, 1)? else {
        return Ok(Value::int(0));
    };
    let other = fetch(req, other_obj)?;
    if this == other {
        return Ok(Value::int(0));
    }
    let mut node1 = Some(other);
    let mut node2 = Some(this);
    let mut attr1 = None;
    let mut attr2 = None;
    if other.kind() == Kind::Attribute {
        attr1 = Some(other);
        node1 = other.parent();
    }
    if this.kind() == Kind::Attribute {
        attr2 = Some(this);
        node2 = this.parent();
        if let (Some(a1), Some(n1)) = (attr1, node1)
            && node2 == Some(n1)
        {
            let mut attr = n1.properties();
            while let Some(a) = attr {
                if is_equal_attr(a, a1) {
                    return Ok(Value::int(IMPLEMENTATION_SPECIFIC | PRECEDING));
                } else if is_equal_attr(a, this) {
                    return Ok(Value::int(IMPLEMENTATION_SPECIFIC | FOLLOWING));
                }
                attr = a.next();
            }
        }
    }
    let disconnected = |req: &Request, node1: Option<Node>, node2: Option<Node>| -> i64 {
        let ordering = if let (Some(a), Some(b)) = (node1, node2) {
            if a < b { PRECEDING } else { FOLLOWING }
        } else {
            let _ = req;
            if other_obj.index() < this_obj.index() {
                PRECEDING
            } else {
                FOLLOWING
            }
        };
        DISCONNECTED | IMPLEMENTATION_SPECIFIC | ordering
    };
    let (Some(mut n1), Some(mut n2)) = (node1, node2) else {
        return Ok(Value::int(disconnected(req, node1, node2)));
    };
    let mut n2_ancestor_of_n1 = false;
    let mut depth1 = 0usize;
    let mut root1 = n1;
    while let Some(p) = root1.parent() {
        root1 = p;
        if p == n2 {
            n2_ancestor_of_n1 = true;
        }
        depth1 += 1;
    }
    let mut n1_ancestor_of_n2 = false;
    let mut depth2 = 0usize;
    let mut root2 = n2;
    while let Some(p) = root2.parent() {
        root2 = p;
        if p == n1 {
            n1_ancestor_of_n2 = true;
        }
        depth2 += 1;
    }
    if root1 != root2 {
        return Ok(Value::int(disconnected(req, Some(n1), Some(n2))));
    }
    if (n1_ancestor_of_n2 && attr1.is_none()) || (n1 == n2 && attr2.is_some()) {
        return Ok(Value::int(CONTAINS | PRECEDING));
    }
    if (n2_ancestor_of_n1 && attr2.is_none()) || (n1 == n2 && attr1.is_some()) {
        return Ok(Value::int(CONTAINED_BY | FOLLOWING));
    }
    if n1_ancestor_of_n2 {
        return Ok(Value::int(PRECEDING));
    } else if n2_ancestor_of_n1 {
        return Ok(Value::int(FOLLOWING));
    }
    while depth1 > depth2 {
        if let Some(p) = n1.parent() {
            n1 = p;
        }
        depth1 -= 1;
    }
    while depth2 > depth1 {
        if let Some(p) = n2.parent() {
            n2 = p;
        }
        depth2 -= 1;
    }
    while n1.parent() != n2.parent() {
        match (n1.parent(), n2.parent()) {
            (Some(a), Some(b)) => {
                n1 = a;
                n2 = b;
            }
            _ => break,
        }
    }
    let mut at = n1.next();
    while let Some(n) = at {
        if n == n2 {
            return Ok(Value::int(PRECEDING));
        }
        at = n.next();
    }
    Ok(Value::int(FOLLOWING))
}

// ---- C14N

/// The `xpath` option of `C14N`: its query evaluated from `node`, the
/// node set it answers kept alive with its evaluator.
fn c14n_query(
    req: &mut Request,
    doc: Node,
    node: Node,
    xpath: PhpArray,
    base: u32,
) -> Result<(XPath, Object), Thrown> {
    let Some(query) = array_get(req, xpath, b"query")? else {
        return Err(arg_value_error(req, 3 + base, "have a \"query\" key"));
    };
    let Some(q) = query.as_str_bytes().map(<[u8]>::to_vec) else {
        let given = value_name(req, &query);
        return Err(argument_error(
            req,
            "TypeError",
            3 + base,
            &format!("\"query\" option must be a string, {given} given"),
        ));
    };
    let mut namespaces = Vec::new();
    if let Some(table) = array_get(req, xpath, b"namespaces")?.and_then(|v| v.as_array().copied())
        && !table.is_list()
    {
        for (k, v) in &table {
            if let (Key::Str(prefix), Some(uri)) = (k, v.deref().as_str_bytes()) {
                namespaces.push((prefix.as_bytes().to_vec(), uri.to_vec()));
            }
        }
    }
    let (found, raised) = XPath::c14n_query(doc, node, &q, &namespaces);
    Errors::route(req, raised);
    match found {
        Some(pair) if pair.1.is_node_set() => Ok(pair),
        _ => Err(throw(req, "Error", "XPath query did not return a nodeset")),
    }
}

/// What `dom_relink_ns_decls` lent C14N, which reads namespace
/// declarations off `nsDef` where a document that follows the standard
/// keeps them as `xmlns` attributes; dropping it is `dom_unlink_ns_decls`,
/// every change put back in the reverse order it was made.
pub(crate) struct NsLinks(Vec<Link>);

/// One change [`NsLinks`] made.
enum Link {
    /// A declaration put in front of the element's `nsDef`, made from the
    /// `xmlns` attribute taken out of its attribute list, or made up for a
    /// namespace no attribute declares.
    Declared {
        element: Node,
        ns: Ns,
        attr: Option<Node>,
    },
    /// The element's default namespace, swapped for the declaration
    /// `xmlSearchNs` finds, as C14N looks the default up itself.
    Default { element: Node, ns: Ns },
}

impl NsLinks {
    /// `dom_relink_ns_decls` over the root element and what it holds.
    pub(crate) fn relink(root: Option<Node>) -> NsLinks {
        let mut links = NsLinks(Vec::new());
        let Some(root) = root else {
            return links;
        };
        links.element(root);
        let mut at = root.children();
        while let Some(node) = at {
            links.element(node);
            at = crate::next_in_tree_order(node, root);
        }
        links
    }

    /// `dom_alloc_ns_decl`: a declaration at the front of `element`'s
    /// `nsDef`.
    fn declare(
        &mut self,
        element: Node,
        href: Option<&[u8]>,
        prefix: Option<&[u8]>,
        attr: Option<Node>,
    ) -> bool {
        let Some(ns) = Ns::owned(href, prefix) else {
            return false;
        };
        ns.set_next(element.ns_def());
        element.set_ns_def(Some(ns));
        self.0.push(Link::Declared { element, ns, attr });
        true
    }

    /// `dom_relink_ns_decls_element`.
    fn element(&mut self, node: Node) {
        if node.kind() != Kind::Element {
            return;
        }
        let mut attr = node.properties();
        while let Some(a) = attr {
            let next = a.next();
            if let Some(ns) = a.ns().filter(|ns| ns.href().as_deref() == Some(XMLNS_NS)) {
                let value = a.attr_value();
                let prefix = ns.prefix().map(|_| a.name());
                if !self.declare(node, Some(&value), prefix.as_deref(), Some(a)) {
                    return;
                }
                match a.prev() {
                    Some(prev) => prev.set_next(next),
                    None => node.set_properties(next),
                }
                if let Some(n) = next {
                    n.set_prev(a.prev());
                }
            }
            attr = next;
        }
        if let Some(ns) = node.ns() {
            if ns.prefix().is_none() {
                self.0.push(Link::Default { element: node, ns });
                node.set_ns(node.search_ns(None));
            } else {
                self.declare(node, ns.href().as_deref(), ns.prefix().as_deref(), None);
            }
        }
        let mut attr = node.properties();
        while let Some(a) = attr {
            if let Some(ns) = a.ns()
                && ns.href().as_deref() != Some(XMLNS_NS)
            {
                let prefix = ns.prefix();
                let mut declared = node.ns_def();
                let mut taken = false;
                while let Some(d) = declared {
                    if d.prefix() == prefix {
                        taken = true;
                        break;
                    }
                    declared = d.next();
                }
                if !taken {
                    self.declare(node, ns.href().as_deref(), prefix.as_deref(), None);
                }
            }
            attr = a.next();
        }
    }
}

impl Drop for NsLinks {
    fn drop(&mut self) {
        while let Some(link) = self.0.pop() {
            match link {
                Link::Default { element, ns } => element.set_ns(Some(ns)),
                Link::Declared { element, ns, attr } => {
                    element.set_ns_def(ns.next());
                    if let Some(a) = attr {
                        match a.prev() {
                            Some(prev) => prev.set_next(Some(a)),
                            None => element.set_properties(Some(a)),
                        }
                        if let Some(next) = a.next() {
                            next.set_prev(Some(a));
                        }
                    }
                    ns.free();
                }
            }
        }
    }
}

/// `dom_canonicalization`: `C14N` into a string, `C14NFile` into a file.
fn canonicalization(req: &mut Request, to_file: bool) -> Result<Value, Thrown> {
    let base = u32::from(to_file);
    arity(req, usize::from(to_file), 4 + usize::from(to_file))?;
    let file = if to_file {
        Some(path_arg(req, 1)?)
    } else {
        None
    };
    let exclusive = bool_arg(req, base + 1, false)?;
    let with_comments = bool_arg(req, base + 2, false)?;
    let xpath = arg(req, base + 3)?.and_then(|v| v.as_array().copied());
    let prefixes = arg(req, base + 4)?.and_then(|v| v.as_array().copied());
    let (this, node) = this_node(req)?;
    let links = if follows_spec(req, this) {
        let mut root = node;
        while let Some(p) = root.parent() {
            root = p;
        }
        if !root.kind().is_document() {
            return Err(crate::dom_throw_message(
                req,
                Code::HierarchyRequest,
                "Canonicalization can only happen on nodes attached to a document.",
            ));
        }
        Some(NsLinks::relink(node.doc().and_then(Node::root)))
    } else {
        None
    };
    let Some(doc) = node.doc() else {
        return Err(throw(
            req,
            "Error",
            "Node must be associated with a document",
        ));
    };
    let query_holder = match xpath {
        Some(xpath) => Some(c14n_query(req, doc, node, xpath, base)?),
        None => None,
    };
    let select = match &query_holder {
        Some((_, object)) => C14nSelect::NodeSet(object),
        None if node.kind().is_document() => C14nSelect::Document,
        None => C14nSelect::Subtree(node),
    };
    let mut inclusive: Option<Vec<Vec<u8>>> = None;
    if let Some(prefixes) = prefixes {
        if exclusive {
            let list: Vec<Vec<u8>> = prefixes
                .iter()
                .filter_map(|(_, v)| v.deref().as_str_bytes().map(<[u8]>::to_vec))
                .collect();
            inclusive = Some(list);
        } else {
            docref(
                req,
                Level::Notice,
                "Inclusive namespace prefixes only allowed in exclusive mode.",
            );
        }
    }
    let target = match &file {
        Some(f) => C14nTarget::File(f),
        None => C14nTarget::Memory,
    };
    let (out, raised, thrown) = Save::c14n(
        req,
        doc,
        &select,
        exclusive,
        inclusive.as_deref(),
        with_comments,
        &target,
    );
    drop(query_holder);
    drop(links);
    Errors::settle(req, raised, thrown)?;
    match out {
        C14nOut::Bytes(bytes) => string(req, &bytes),
        C14nOut::Written(n) => Ok(Value::int(n)),
        C14nOut::Failed => Ok(Value::bool(false)),
    }
}

/// `DOMNode::C14N(bool $exclusive = false, bool $withComments = false, ?array $xpath = null, ?array $nsPrefixes = null): string|false`,
/// which for a document that follows the standard lends C14N its `xmlns`
/// attributes as declarations ([`NsLinks`]) and refuses a node no document
/// holds.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r b="2" a="1"><!-- c --><x/></r>');
/// echo $d->C14N(), "\n", $d->C14N(false, true), "\n", $d->documentElement->firstChild->nextSibling->C14N(), "\n";
/// $m = Dom\XMLDocument::createFromString('<r xmlns="urn:r" xmlns:p="urn:p" p:b="2" a="1"><p:c/><c/></r>');
/// $r = $m->documentElement;
/// echo $m->C14N(), "\n", $r->firstChild->C14N(), "\n", $r->firstChild->C14N(true), "\n", $r->attributes->length, "\n";
/// $lone = $r->cloneNode();
/// try { $lone->C14N(); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// <r a="1" b="2"><x></x></r>
/// <r a="1" b="2"><!-- c --><x></x></r>
/// <x></x>
/// <r xmlns="urn:r" xmlns:p="urn:p" a="1" p:b="2"><p:c></p:c><c></c></r>
/// <p:c xmlns="urn:r" xmlns:p="urn:p"></p:c>
/// <p:c xmlns:p="urn:p"></p:c>
/// 4
/// Canonicalization can only happen on nodes attached to a document.
/// ```
fn c14n(req: &mut Request) -> Result<Value, Thrown> {
    canonicalization(req, false)
}

/// `DOMNode::C14NFile(string $uri, ...): int|false`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r/>');
/// $f = tempnam(sys_get_temp_dir(), 'c14n');
/// var_dump($d->C14NFile($f), file_get_contents($f));
/// unlink($f);
/// ```
/// ```expect
/// int(7)
/// string(7) "<r></r>"
/// ```
fn c14n_file(req: &mut Request) -> Result<Value, Thrown> {
    canonicalization(req, true)
}

/// `DOMNode::appendChild(DOMNode $node): DOMNode|false`, `dom_node_append_child_legacy`.
///
/// ```php
/// $d = new DOMDocument;
/// $r = $d->appendChild($d->createElement('r'));
/// $r->appendChild($d->createTextNode('a'));
/// $t = $r->appendChild($d->createTextNode('b'));
/// var_dump($r->childNodes->length, $t->textContent, $d->saveXML($r));
/// try { $r->appendChild($r); } catch (DOMException $e) { echo $e->getMessage(), ' ', $e->getCode(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// string(1) "b"
/// string(9) "<r>ab</r>"
/// Hierarchy Request Error 3
/// ```
fn append_child(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let (this, parent) = this_node(req)?;
    let Some(child_obj) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let child = fetch(req, child_obj)?;
    if !children_valid(parent) {
        return Ok(Value::bool(false));
    }
    let strict = strict(req, this);
    if !check_legacy_insertion(req, parent, child, strict, true)? {
        return Ok(Value::bool(false));
    }
    if child.doc().is_none() && parent.doc().is_some() {
        child.set_tree_doc(parent.doc());
        set_document_ref_pointers(child, shared_doc(req, this));
    }
    if child.parent().is_some() {
        child.unlink();
    }
    let new_child = match child.kind() {
        Kind::Text if parent.last().is_some_and(|l| l.kind() == Kind::Text) => {
            child.set_parent(Some(parent));
            if parent.children().is_none() {
                parent.set_children(Some(child));
                parent.set_last(Some(child));
            } else {
                if let Some(last) = parent.last() {
                    last.set_next(Some(child));
                    child.set_prev(Some(last));
                }
                parent.set_last(Some(child));
            }
            Some(child)
        }
        Kind::Attribute => {
            if let Some(old) = existing_attr(parent, child)
                && old != child
            {
                old.unlink();
                old.free_resource();
            }
            let added = parent.add_child(child);
            if let Some(a) = added {
                reconcile_attribute_namespace(a);
            }
            added
        }
        Kind::Fragment => {
            let last = child.last();
            let first = insert_fragment(parent, parent.last(), None, child);
            if let (Some(f), Some(l)) = (first, last) {
                reconcile_ns_list(parent.doc(), f, l);
            }
            first
        }
        Kind::Dtd => {
            let doc = parent.doc();
            if doc.is_some_and(|d| d.int_subset().is_some()) {
                dom_error_message(
                    req,
                    Code::HierarchyRequest,
                    "A document may only contain one document type",
                    strict,
                )?;
                return Ok(Value::bool(false));
            }
            let added = parent.add_child(child);
            if let (Some(d), Some(a)) = (doc, added) {
                d.set_int_subset(Some(a));
            }
            added
        }
        _ => {
            let added = parent.add_child(child);
            if let Some(a) = added {
                reconcile_ns(parent.doc(), a);
            }
            added
        }
    };
    let Some(new_child) = new_child else {
        dom_error(req, Code::InvalidState, strict)?;
        return Ok(Value::bool(false));
    };
    invalidate(req, this);
    object_for(req, new_child, Some(this))
}

/// `DOMNode::insertBefore(DOMNode $node, ?DOMNode $child = null): DOMNode|false`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><b/></r>');
/// $r = $d->documentElement;
/// $first = $r->firstChild;
/// $r->insertBefore($d->createElement('a'), $first);
/// $r->insertBefore($d->createElement('c'));
/// echo $d->saveXML($r), "\n";
/// try { $r->insertBefore($d->createElement('x'), $d->createElement('y')); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// <r><a/><b/><c/></r>
/// Not Found Error
/// ```
#[allow(clippy::too_many_lines)] // php's function, one step per clause of the specification
fn insert_before(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 2)?;
    let (this, parent) = this_node(req)?;
    let Some(child_obj) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let child = fetch(req, child_obj)?;
    let reference = object_arg(req, 2)?;
    if !children_valid(parent) {
        return Ok(Value::bool(false));
    }
    let strict = strict(req, this);
    if !check_legacy_insertion(req, parent, child, strict, true)? {
        return Ok(Value::bool(false));
    }
    let refp = match reference {
        Some(r) => {
            let refp = fetch(req, r)?;
            if refp.parent() != Some(parent) {
                dom_error(req, Code::NotFound, strict)?;
                return Ok(Value::bool(false));
            }
            Some(refp)
        }
        None => None,
    };
    if child.doc().is_none() && parent.doc().is_some() {
        child.set_tree_doc(parent.doc());
        set_document_ref_pointers(child, shared_doc(req, this));
    }
    invalidate(req, this);
    let new_child = if let Some(refp) = refp {
        if child.parent().is_some() {
            child.unlink();
        }
        if child.kind() == Kind::Text
            && (refp.kind() == Kind::Text || refp.prev().is_some_and(|p| p.kind() == Kind::Text))
        {
            child.set_parent(refp.parent());
            child.set_next(Some(refp));
            child.set_prev(refp.prev());
            refp.set_prev(Some(child));
            if let Some(p) = child.prev() {
                p.set_next(Some(child));
            }
            if let Some(p) = child.parent()
                && p.children() == Some(refp)
            {
                p.set_children(Some(child));
            }
            Some(child)
        } else if child.kind() == Kind::Attribute {
            if let Some(owner) = refp.parent()
                && let Some(old) = existing_attr(owner, child)
            {
                if old == child {
                    return object_for(req, child, Some(this));
                }
                old.unlink();
                old.free_resource();
            }
            match refp.add_prev_sibling(child) {
                Some(n) => Some(n),
                None => {
                    return Err(throw(
                        req,
                        "Error",
                        "Cannot add newnode as the previous sibling of refnode",
                    ));
                }
            }
        } else if child.kind() == Kind::Fragment {
            let last = child.last();
            let first = insert_fragment(parent, refp.prev(), Some(refp), child);
            if let (Some(f), Some(l)) = (first, last) {
                reconcile_ns_list(parent.doc(), f, l);
            }
            first
        } else {
            match refp.add_prev_sibling(child) {
                Some(n) => {
                    reconcile_ns(parent.doc(), n);
                    Some(n)
                }
                None => {
                    return Err(throw(
                        req,
                        "Error",
                        "Cannot add newnode as the previous sibling of refnode",
                    ));
                }
            }
        }
    } else {
        if child.parent().is_some() {
            child.unlink();
        }
        if child.kind() == Kind::Text && parent.last().is_some_and(|l| l.kind() == Kind::Text) {
            child.set_parent(Some(parent));
            match parent.last() {
                None => {
                    parent.set_children(Some(child));
                    parent.set_last(Some(child));
                }
                Some(last) => {
                    last.set_next(Some(child));
                    child.set_prev(Some(last));
                    parent.set_last(Some(child));
                }
            }
            Some(child)
        } else if child.kind() == Kind::Attribute {
            if let Some(old) = existing_attr(parent, child) {
                if old == child {
                    return object_for(req, child, Some(this));
                }
                old.unlink();
                old.free_resource();
            }
            match parent.add_child(child) {
                Some(n) => Some(n),
                None => {
                    return Err(throw(
                        req,
                        "Error",
                        "Cannot add newnode as the previous sibling of refnode",
                    ));
                }
            }
        } else if child.kind() == Kind::Fragment {
            let last = child.last();
            let first = insert_fragment(parent, parent.last(), None, child);
            if let (Some(f), Some(l)) = (first, last) {
                reconcile_ns_list(parent.doc(), f, l);
            }
            first
        } else {
            match parent.add_child(child) {
                Some(n) => {
                    reconcile_ns(parent.doc(), n);
                    Some(n)
                }
                None => {
                    return Err(throw(
                        req,
                        "Error",
                        "Cannot add newnode as the previous sibling of refnode",
                    ));
                }
            }
        }
    };
    nullable_object(req, new_child, Some(this))
}

/// `Dom\Node::appendChild(Dom\Node $node): Dom\Node`: the standard's
/// append, which checks what a document may hold and merges no text.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r>a</r>');
/// $r = $d->documentElement;
/// $t = $r->appendChild($d->createTextNode('b'));
/// $first = $r->firstChild;
/// var_dump($r->childNodes->length, $t->data, $r->appendChild($first) === $first, $r->lastChild->data);
/// $text = $d->createTextNode('t');
/// $copy = $r->cloneNode();
/// $comment = $d->createComment('c');
/// foreach ([fn() => $d->appendChild($text), fn() => $d->appendChild($copy), fn() => $r->appendChild($r), fn() => $t->appendChild($comment)] as $f) {
///     try { $f(); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// int(2)
/// string(1) "b"
/// bool(true)
/// string(1) "a"
/// Cannot insert text as a child of a document
/// Cannot have more than one element child in a document
/// Hierarchy Request Error
/// Hierarchy Request Error
/// ```
fn modern_append_child(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let (this, parent) = this_node(req)?;
    let Some(child_obj) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let child = fetch(req, child_obj)?;
    if is_parent_invalid(parent) {
        return Err(crate::dom_throw(req, Code::HierarchyRequest));
    }
    invalidate(req, this);
    crate::parent::pre_insert(req, this, child, parent, None)?;
    object_for(req, child, Some(this))
}

/// `Dom\Node::insertBefore(Dom\Node $node, ?Dom\Node $child): Dom\Node`:
/// the standard's pre-insert.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<!DOCTYPE r><r><b/></r>');
/// $r = $d->documentElement;
/// $b = $r->firstChild;
/// $r->insertBefore($d->createComment('a'), $b);
/// $r->insertBefore($d->createTextNode('c'), null);
/// $a = $r->firstChild;
/// $r->insertBefore($a, null);
/// foreach ($r->childNodes as $n) echo $n->nodeName, "\n";
/// $x = $d->createComment('x');
/// $dt = $d->doctype;
/// foreach ([fn() => $d->insertBefore($x, $b), fn() => $d->insertBefore($x, $dt), fn() => $d->insertBefore($dt, $r), fn() => $d->insertBefore($r->cloneNode(), $dt)] as $f) {
///     try { $f(); echo "ok\n"; } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// b
/// #text
/// #comment
/// Not Found Error
/// ok
/// Cannot have more than one document type
/// Cannot have more than one element child in a document
/// ```
fn modern_insert_before(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let (this, parent) = this_node(req)?;
    let Some(child_obj) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let child = fetch(req, child_obj)?;
    let reference = object_arg(req, 2)?;
    if is_parent_invalid(parent) {
        return Err(crate::dom_throw(req, Code::HierarchyRequest));
    }
    let point = match reference {
        Some(r) => Some(fetch(req, r)?),
        None => None,
    };
    invalidate(req, this);
    crate::parent::pre_insert(req, this, child, parent, point)?;
    object_for(req, child, Some(this))
}

/// `DOMNode::removeChild(DOMNode $child): DOMNode|false`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/><b/></r>');
/// $r = $d->documentElement;
/// $first = $r->firstChild;
/// $a = $r->removeChild($first);
/// var_dump($a->parentNode, $d->saveXML($r));
/// try { $r->removeChild($a); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// NULL
/// string(11) "<r><b/></r>"
/// Not Found Error
/// ```
fn remove_child(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let (this, parent) = this_node(req)?;
    let Some(child_obj) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let child = fetch(req, child_obj)?;
    let strict = strict(req, this);
    if parent.children().is_none() || child.parent() != Some(parent) {
        dom_error(req, Code::NotFound, strict)?;
        return Ok(Value::bool(false));
    }
    if is_read_only(parent) || child.parent().is_some_and(is_read_only) {
        dom_error(req, Code::NoModificationAllowed, strict)?;
        return Ok(Value::bool(false));
    }
    child.unlink();
    invalidate(req, this);
    object_for(req, child, Some(this))
}

/// `DOMNode::contains(DOMNode|DOMNameSpaceNode|null $other): bool`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/></r>');
/// $first = $d->documentElement->firstChild;
/// var_dump($d->contains($first), $first->contains($d), $d->contains(null));
/// try { $d->contains(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// DOMNode::contains(): Argument #1 ($other) must be of type DOMNode|DOMNameSpaceNode|null, stdClass given
/// ```
fn contains(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let Some(other) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    if !instance_of(req, other, "DOMNode") && !instance_of(req, other, "DOMNameSpaceNode") {
        let name = class_name(req, other.class());
        return Err(php_ext::raising::arg_type_error(
            req,
            1,
            &format!("be of type DOMNode|DOMNameSpaceNode|null, {name} given"),
        ));
    }
    let other = fetch(req, other)?;
    let (_, node) = this_node(req)?;
    Ok(Value::bool(contains_node(node, other)))
}

/// `dom_node_contains`: whether `node` is `other` or one of its ancestors.
fn contains_node(node: Node, other: Node) -> bool {
    let mut at = Some(other);
    while let Some(n) = at {
        if n == node {
            return true;
        }
        at = n.parent();
    }
    false
}

/// `Dom\Node::contains(?Dom\Node $other): bool`.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r><a/></r>');
/// $a = $d->documentElement->firstChild;
/// var_dump($d->contains($a), $a->contains($d), $a->contains($a), $d->contains(null));
/// try { $d->contains(new DOMDocument); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// Dom\Node::contains(): Argument #1 ($other) must be of type ?Dom\Node, DOMDocument given
/// ```
fn modern_contains(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let Some(other) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let other = fetch(req, other)?;
    let (_, node) = this_node(req)?;
    Ok(Value::bool(contains_node(node, other)))
}

/// `DOMNode::__sleep(): array`: php's refusal, unless a subclass
/// serialises itself.
///
/// ```php
/// try { serialize(new DOMDocument); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Serialization of 'DOMDocument' is not allowed, unless serialization methods are implemented in a subclass
/// ```
fn sleep(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let this = this(req)?;
    let name = class_name(req, this.class());
    Err(throw(
        req,
        "Exception",
        &format!(
            "Serialization of '{name}' is not allowed, unless serialization methods are implemented in a subclass"
        ),
    ))
}

/// `DOMNode::__wakeup(): void`: php's refusal.
///
/// ```php
/// try { unserialize('O:10:"DOMElement":0:{}'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Unserialization of 'DOMElement' is not allowed, unless unserialization methods are implemented in a subclass
/// ```
fn wakeup(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let this = this(req)?;
    let name = class_name(req, this.class());
    Err(throw(
        req,
        "Exception",
        &format!(
            "Unserialization of '{name}' is not allowed, unless unserialization methods are implemented in a subclass"
        ),
    ))
}

/// The methods `DOMNode` declares, `DOMNameSpaceNode`'s, which are
/// `DOMNode`'s own (`@implementation-alias`), and those `Dom\Node`
/// declares apart from the legacy class.
pub(crate) const METHODS: &[(&str, Entry)] = &[
    ("DOMNode::appendChild", append_child),
    ("DOMNode::C14N", c14n),
    ("DOMNode::C14NFile", c14n_file),
    ("DOMNode::cloneNode", clone_node_method),
    ("DOMNode::getLineNo", get_line_no),
    ("DOMNode::getNodePath", get_node_path),
    ("DOMNode::hasAttributes", has_attributes),
    ("DOMNode::hasChildNodes", has_child_nodes),
    ("DOMNode::insertBefore", insert_before),
    ("DOMNode::isDefaultNamespace", is_default_namespace),
    ("DOMNode::isSameNode", is_same_node),
    ("DOMNode::isEqualNode", is_equal_node_method),
    ("DOMNode::isSupported", is_supported),
    ("DOMNode::lookupNamespaceURI", lookup_namespace_uri),
    ("DOMNode::lookupPrefix", lookup_prefix),
    ("DOMNode::normalize", normalize_method),
    ("DOMNode::removeChild", remove_child),
    ("DOMNode::replaceChild", replace_child),
    ("DOMNode::contains", contains),
    ("DOMNode::getRootNode", get_root_node),
    (
        "DOMNode::compareDocumentPosition",
        compare_document_position,
    ),
    ("DOMNode::__sleep", sleep),
    ("DOMNode::__wakeup", wakeup),
    ("DOMNameSpaceNode::__sleep", sleep),
    ("DOMNameSpaceNode::__wakeup", wakeup),
    ("Dom\\Node::appendChild", modern_append_child),
    ("Dom\\Node::insertBefore", modern_insert_before),
    ("Dom\\Node::replaceChild", modern_replace_child),
    ("Dom\\Node::removeChild", remove_child),
    ("Dom\\Node::isEqualNode", modern_is_equal_node),
    ("Dom\\Node::isSameNode", is_same_node),
    ("Dom\\Node::lookupPrefix", modern_lookup_prefix),
    ("Dom\\Node::isDefaultNamespace", modern_is_default_namespace),
    ("Dom\\Node::getNodePath", modern_get_node_path),
    ("Dom\\Node::contains", modern_contains),
    (
        "Dom\\Node::compareDocumentPosition",
        compare_document_position,
    ),
];
