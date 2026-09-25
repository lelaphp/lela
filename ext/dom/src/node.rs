//! `DOMNode` (`node.c`): its methods, and the insertion checks of
//! `php_dom.c` the other classes share.

use php_ext::coerce::value_name;
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, Value};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi::{C14nOut, C14nSelect, C14nTarget, Kind, Node, Ns, Object, Save, XPath};

use crate::object::Code;
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
    if new_child.doc().is_none() && parent.doc().is_some() {
        new_child.set_tree_doc(parent.doc());
        set_document_ref_pointers(new_child, shared_doc(req, this));
    }
    if new_child.kind() == Kind::Fragment {
        let prev = old_child.prev();
        let next = old_child.next();
        old_child.unlink();
        let last = new_child.last();
        if let (Some(first), Some(last)) = (insert_fragment(parent, prev, next, new_child), last) {
            reconcile_ns_list(parent.doc(), first, last);
        }
    } else if old_child != new_child {
        let doc = parent.doc();
        let replaced_doctype = doc.and_then(Node::int_subset) == Some(old_child);
        old_child.replace_with(new_child);
        reconcile_ns(doc, new_child);
        if replaced_doctype && let Some(d) = doc {
            d.set_int_subset(Some(new_child));
        }
    }
    invalidate(req, this);
    object_for(req, old_child, Some(this))
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

/// `DOMNode::cloneNode(bool $deep = false): DOMNode|false`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r a="1"><c/></r>');
/// $r = $d->documentElement;
/// echo $d->saveXML($r->cloneNode()), ' ', $d->saveXML($r->cloneNode(true)), "\n";
/// $copy = $d->cloneNode(true);
/// var_dump($copy === $d, $copy->saveXML() === $d->saveXML());
/// ```
/// ```expect
/// <r a="1"/> <r a="1"><c/></r>
/// bool(false)
/// bool(true)
/// ```
fn clone_node_method(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 1)?;
    let recursive = bool_arg(req, 1, false)?;
    let (this, n) = this_node(req)?;
    let Some(node) = clone_node(n, n.doc(), recursive) else {
        return Ok(Value::bool(false));
    };
    if n.kind().is_document() {
        let value = object_for(req, node, None)?;
        if let Some(copy) = value.as_object().copied() {
            copy_document_ref(req, this, copy);
        }
        return Ok(value);
    }
    if node.kind() == Kind::Attribute && n.ns().is_some() && node.ns().is_none() {
        node.set_ns(n.ns());
    }
    object_for(req, node, Some(this))
}

/// `DOMNode::normalize(): void`.
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
/// ```
/// ```expect
/// int(2)
/// string(2) "ab"
/// ```
fn normalize_method(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (_, node) = this_node(req)?;
    normalize(node);
    Ok(Value::null())
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
    arity(req, 1, 1)?;
    let Some(other) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let other = fetch(req, other)?;
    let (_, node) = this_node(req)?;
    if node == other {
        return Ok(Value::bool(true));
    }
    Ok(Value::bool(is_equal_node(node, other)))
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
    arity(req, 1, 1)?;
    let uri = bytes_arg(req, 1)?;
    let (_, node) = this_node(req)?;
    if uri.is_empty() {
        return Ok(Value::null());
    }
    let lookup = match node.kind() {
        Kind::Element => Some(node),
        Kind::Document | Kind::HtmlDocument => node.root(),
        Kind::Entity | Kind::Notation | Kind::Fragment | Kind::DocumentType | Kind::Dtd => {
            return Ok(Value::null());
        }
        _ => node.parent(),
    };
    if let Some(l) = lookup
        && let Some(ns) = l.search_ns_by_href(l.doc(), &uri)
        && let Some(prefix) = ns.prefix()
    {
        return string(req, &prefix);
    }
    Ok(Value::null())
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

/// `DOMNode::lookupNamespaceURI(?string $prefix): ?string`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r xmlns="urn:d" xmlns:p="urn:p"><c/></r>');
/// $c = $d->documentElement->firstChild;
/// var_dump($c->lookupNamespaceURI('p'), $c->lookupNamespaceURI(null), $d->lookupNamespaceURI('q'));
/// ```
/// ```expect
/// string(5) "urn:p"
/// string(5) "urn:d"
/// NULL
/// ```
fn lookup_namespace_uri(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let prefix = nullable_bytes_arg(req, 1)?;
    let (_, mut node) = this_node(req)?;
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
    arity(req, 0, 0)?;
    let (_, node) = this_node(req)?;
    match node.path() {
        Some(p) => string(req, &p),
        None => Ok(Value::null()),
    }
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
        return Err(arg_type_error(
            req,
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
    let (_, node) = this_node(req)?;
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
    let (out, raised) = Save::c14n(
        doc,
        &select,
        exclusive,
        inclusive.as_deref(),
        with_comments,
        &target,
    );
    drop(query_holder);
    Errors::route(req, raised);
    match out {
        C14nOut::Bytes(bytes) => string(req, &bytes),
        C14nOut::Written(n) => Ok(Value::int(n)),
        C14nOut::Failed => Ok(Value::bool(false)),
    }
}

/// `DOMNode::C14N(bool $exclusive = false, bool $withComments = false, ?array $xpath = null, ?array $nsPrefixes = null): string|false`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r b="2" a="1"><!-- c --><x/></r>');
/// echo $d->C14N(), "\n", $d->C14N(false, true), "\n", $d->documentElement->firstChild->nextSibling->C14N(), "\n";
/// ```
/// ```expect
/// <r a="1" b="2"><x></x></r>
/// <r a="1" b="2"><!-- c --><x></x></r>
/// <x></x>
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
    let mut at = Some(other);
    while let Some(n) = at {
        if n == node {
            return Ok(Value::bool(true));
        }
        at = n.parent();
    }
    Ok(Value::bool(false))
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

/// The methods `DOMNode` declares, and `DOMNameSpaceNode`'s, which are
/// `DOMNode`'s own (`@implementation-alias`).
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
];
