//! `DOMParentNode` and `DOMChildNode` (`parentnode/tree.c`): `append`,
//! `prepend`, `replaceChildren`, `before`, `after`, `replaceWith` and
//! `remove` over a list of nodes and strings, and the pre-insertion
//! validity both the legacy classes and the `Dom\` classes check, the
//! latter held to the DOM standard's document rules where the document
//! follows it.

use php_ext::abi::{Instance, Union, Untyped, Variadic};
use php_ext::attr::php_class;
use php_ext::raising::{Thrown, arg_type_error, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{PhpObject, PhpString, Value, ValueRef};
use php_ext_libxml::ffi::{Kind, Node};

use crate::modern::follows_spec;
use crate::object::Code;
use crate::{
    class_name, dom_error, dom_error_message, dom_throw, fetch, hierarchy_ok, instance_of, intern,
    invalidate, node_of, strict,
};

/// One argument of the list, checked: a node's object, or the bytes of a
/// text node to make.
enum Piece {
    Node(PhpObject),
    Text(Vec<u8>),
}

/// `dom_sanity_check_node_list_types` over the call's arguments, the node
/// class the one the receiver's document hands out.
fn pieces(
    req: &mut Request,
    this: PhpObject,
    nodes: &[Untyped<Value>],
) -> Result<Vec<Piece>, Thrown> {
    let class = if follows_spec(req, this) {
        "Dom\\Node"
    } else {
        "DOMNode"
    };
    let mut out = Vec::with_capacity(nodes.len());
    for (i, value) in nodes.iter().enumerate() {
        let position = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let v = value.0.deref();
        match v.as_ref() {
            ValueRef::Object(o) if instance_of(req, o, class) => out.push(Piece::Node(o)),
            ValueRef::Str(s) if i32::try_from(s.len()).is_err() => {
                return Err(arg_value_error(
                    req,
                    position,
                    &format!("be less than or equal to {} bytes long", i32::MAX),
                ));
            }
            ValueRef::Str(s) => out.push(Piece::Text(s.as_bytes().to_vec())),
            ValueRef::Object(o) => {
                let name = class_name(req, o.class());
                return Err(arg_type_error(
                    req,
                    position,
                    &format!("be of type {class}|string, {name} given"),
                ));
            }
            _ => {
                let name = v.type_name();
                return Err(arg_type_error(
                    req,
                    position,
                    &format!("be of type {class}|string, {name} given"),
                ));
            }
        }
    }
    Ok(out)
}

/// `dom_is_node_in_list`.
fn in_list(req: &Request, pieces: &[Piece], node: Node) -> bool {
    pieces
        .iter()
        .any(|p| matches!(p, Piece::Node(o) if node_of(req, *o) == Some(node)))
}

/// What `php_dom_throw_error_with_message(HIERARCHY_REQUEST_ERR, ...)`
/// says where the standard's document rules refuse an insertion.
const TEXT_IN_DOCUMENT: &str = "Cannot insert text as a child of a document";
const DOCTYPE_OUTSIDE_DOCUMENT: &str =
    "Cannot insert a document type into anything other than a document";
const ONE_ELEMENT: &str = "Cannot have more than one element child in a document";
const DOCTYPE_FIRST: &str = "Document types must be the first child in a document";
const ONE_DOCTYPE: &str = "Cannot have more than one document type";

/// `php_dom_pre_insert_is_parent_invalid`: whether `parent` is no document,
/// fragment or element, which the standard's insertions refuse.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r>t<!--c--></r>');
/// $r = $d->documentElement;
/// $t = $r->firstChild;
/// $c = $r->lastChild;
/// $e = $r->cloneNode();
/// foreach ([fn() => $t->appendChild($e), fn() => $c->insertBefore($e, null), fn() => $r->appendChild($e)] as $g) {
///     try { $g(); echo "ok\n"; } catch (DOMException $x) { echo $x->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// Hierarchy Request Error
/// Hierarchy Request Error
/// ok
/// ```
pub(crate) fn is_parent_invalid(parent: Node) -> bool {
    !matches!(
        parent.kind(),
        Kind::Document | Kind::HtmlDocument | Kind::Element | Kind::Fragment
    )
}

/// `php_dom_has_child_of_type`.
fn has_child_of_type(node: Node, kind: Kind) -> bool {
    let mut at = node.children();
    while let Some(n) = at {
        if n.kind() == kind {
            return true;
        }
        at = n.next();
    }
    false
}

/// `php_dom_has_sibling_following_node`.
fn has_sibling_following(node: Node, kind: Kind) -> bool {
    let mut at = node.next();
    while let Some(n) = at {
        if n.kind() == kind {
            return true;
        }
        at = n.next();
    }
    false
}

/// `php_dom_has_sibling_preceding_node`.
fn has_sibling_preceding(node: Node, kind: Kind) -> bool {
    let mut at = node.prev();
    while let Some(n) = at {
        if n.kind() == kind {
            return true;
        }
        at = n.prev();
    }
    false
}

/// `dom_fragment_common_hierarchy_check_part`: the refusal a fragment's
/// children earn under a document, else whether one of them is an element.
fn fragment_common(node: Node) -> Result<bool, &'static str> {
    let mut seen_element = false;
    let mut at = node.children();
    while let Some(n) = at {
        match n.kind() {
            Kind::Element if seen_element => return Err(ONE_ELEMENT),
            Kind::Element => seen_element = true,
            Kind::Text | Kind::CData => return Err(TEXT_IN_DOCUMENT),
            _ => {}
        }
        at = n.next();
    }
    Ok(seen_element)
}

/// `php_dom_fragment_insertion_hierarchy_check_pre_insertion`.
fn fragment_pre_insertion(
    parent: Node,
    node: Node,
    child: Option<Node>,
) -> Result<(), &'static str> {
    if fragment_common(node)? {
        if has_child_of_type(parent, Kind::Element) {
            return Err(ONE_ELEMENT);
        }
        if child.is_some_and(|c| c.kind() == Kind::Dtd || has_sibling_following(c, Kind::Dtd)) {
            return Err(DOCTYPE_FIRST);
        }
    }
    Ok(())
}

/// `php_dom_fragment_insertion_hierarchy_check_replace`.
fn fragment_replace(parent: Node, node: Node, child: Node) -> Result<(), &'static str> {
    if fragment_common(node)? {
        let mut at = parent.children();
        while let Some(n) = at {
            if n.kind() == Kind::Element && n != child {
                return Err(ONE_ELEMENT);
            }
            at = n.next();
        }
        if has_sibling_following(child, Kind::Dtd) {
            return Err(DOCTYPE_FIRST);
        }
    }
    Ok(())
}

/// Steps 5 and 6 of the pre-insertion validity, which only a document
/// that follows the standard checks: what may go under a document, and
/// where.
fn document_rules(parent: Node, node: Node, child: Option<Node>) -> Result<(), &'static str> {
    let parent_is_document = parent.kind().is_document();
    if parent_is_document && matches!(node.kind(), Kind::Text | Kind::CData) {
        return Err(TEXT_IN_DOCUMENT);
    }
    if !parent_is_document && node.kind() == Kind::Dtd {
        return Err(DOCTYPE_OUTSIDE_DOCUMENT);
    }
    if !parent_is_document {
        return Ok(());
    }
    match node.kind() {
        Kind::Fragment => fragment_pre_insertion(parent, node, child),
        Kind::Element => {
            if has_child_of_type(parent, Kind::Element) {
                return Err(ONE_ELEMENT);
            }
            if child.is_some_and(|c| c.kind() == Kind::Dtd || has_sibling_following(c, Kind::Dtd)) {
                return Err(DOCTYPE_FIRST);
            }
            Ok(())
        }
        Kind::Dtd => {
            if has_child_of_type(parent, Kind::Dtd) {
                return Err(ONE_DOCTYPE);
            }
            let element_before = match child {
                Some(c) => has_sibling_preceding(c, Kind::Element),
                None => has_child_of_type(parent, Kind::Element),
            };
            if element_before {
                return Err(DOCTYPE_FIRST);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `dom_replace_node_validity_checks`: the standard's replace steps 1 to
/// 6, each refusal thrown.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<!DOCTYPE r><r><a/></r>');
/// $r = $d->documentElement;
/// $a = $r->firstChild;
/// $dt = $d->doctype;
/// $f = $d->createDocumentFragment();
/// $f->append('x');
/// $pi = $d->createProcessingInstruction('p', 'd');
/// foreach ([fn() => $a->replaceChild($pi, $a), fn() => $r->replaceChild($dt, $a), fn() => $d->replaceChild($f, $r), fn() => $d->replaceChild($r, $dt), fn() => $r->replaceChild($pi, $a)] as $g) {
///     try { $g(); echo "ok\n"; } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// Not Found Error
/// Cannot insert a document type into anything other than a document
/// Cannot insert text as a child of a document
/// Cannot have more than one element child in a document
/// ok
/// ```
pub(crate) fn replace_validity(
    req: &mut Request,
    parent: Node,
    node: Node,
    child: Node,
) -> Result<(), Thrown> {
    let refusal = if is_parent_invalid(parent) || !hierarchy_ok(parent, node) {
        Some((Code::HierarchyRequest, None))
    } else if child.parent() != Some(parent) {
        Some((Code::NotFound, None))
    } else if !matches!(
        node.kind(),
        Kind::Fragment
            | Kind::Dtd
            | Kind::Element
            | Kind::Text
            | Kind::CData
            | Kind::Comment
            | Kind::Pi
    ) {
        Some((Code::HierarchyRequest, None))
    } else {
        replace_document_rules(parent, node, child).map(|m| (Code::HierarchyRequest, Some(m)))
    };
    match refusal {
        None => Ok(()),
        Some((code, None)) => Err(dom_throw(req, code)),
        Some((code, Some(message))) => Err(crate::dom_throw_message(req, code, message)),
    }
}

/// Steps 5 and 6 of the standard's replace: what may take a child's place
/// under a document.
fn replace_document_rules(parent: Node, node: Node, child: Node) -> Option<&'static str> {
    let parent_is_document = parent.kind().is_document();
    if parent_is_document && matches!(node.kind(), Kind::Text | Kind::CData) {
        return Some(TEXT_IN_DOCUMENT);
    }
    if !parent_is_document && node.kind() == Kind::Dtd {
        return Some(DOCTYPE_OUTSIDE_DOCUMENT);
    }
    if !parent_is_document {
        return None;
    }
    match node.kind() {
        Kind::Fragment => fragment_replace(parent, node, child).err(),
        Kind::Element if parent.root() != Some(child) => Some(ONE_ELEMENT),
        Kind::Element if has_sibling_following(child, Kind::Dtd) => Some(DOCTYPE_FIRST),
        Kind::Dtd
            if parent.int_subset() != Some(child)
                || has_sibling_preceding(child, Kind::Element) =>
        {
            Some(DOCTYPE_FIRST)
        }
        _ => None,
    }
}

/// Step 4 for a document that follows the standard: a node that is no
/// fragment, doctype, element or character data.
fn refused_kind(node: Node) -> bool {
    matches!(
        node.kind(),
        Kind::EntityRef | Kind::Entity | Kind::Notation | Kind::Document | Kind::HtmlDocument
    ) || node.kind().number() >= Kind::ElementDecl.number()
}

/// `dom_is_pre_insert_valid_without_step_1`: whether `node` may go under
/// `parent` before `child`, the error raised as the receiver's document
/// says where not.
fn pre_insert_valid(
    req: &mut Request,
    this: PhpObject,
    parent: Node,
    node: Node,
    child: Option<Node>,
    doc: Option<Node>,
) -> Result<bool, Thrown> {
    let strict = strict(req, this);
    let modern = follows_spec(req, this);
    let code = if is_parent_invalid(parent) {
        Some(Code::HierarchyRequest)
    } else if node.doc() != doc {
        Some(Code::WrongDocument)
    } else if child.is_some_and(|c| c.parent() != Some(parent)) {
        Some(Code::NotFound)
    } else if !hierarchy_ok(parent, node)
        || node.kind() == Kind::Attribute
        || (modern && refused_kind(node))
    {
        Some(Code::HierarchyRequest)
    } else {
        None
    };
    if let Some(code) = code {
        dom_error(req, code, strict)?;
        return Ok(false);
    }
    if modern && let Err(message) = document_rules(parent, node, child) {
        dom_error_message(req, Code::HierarchyRequest, message, true)?;
        return Ok(false);
    }
    Ok(true)
}

/// `dom_add_child_without_merging`.
fn add_without_merging(parent: Node, child: Node) {
    match parent.last() {
        None => parent.set_children(Some(child)),
        Some(last) => {
            last.set_next(Some(child));
            child.set_prev(Some(last));
        }
    }
    parent.set_last(Some(child));
    child.set_parent(Some(parent));
}

/// `dom_free_node_after_zval_single_node_creation`, then the fragment's
/// shell.
fn discard_fragment(fragment: Node) {
    let mut at = fragment.children();
    while let Some(child) = at {
        at = child.next();
        child.unlink();
        if !child.is_named() {
            child.free();
        }
    }
    fragment.free_shell();
}

/// `dom_zvals_to_single_node`: the one node the list converts to.
fn single_node(
    req: &mut Request,
    this: PhpObject,
    context: Node,
    list: &[Piece],
) -> Result<Option<Node>, Thrown> {
    let doc = if context.kind().is_document() {
        Some(context)
    } else {
        context.doc()
    };
    if let [one] = list {
        return match one {
            Piece::Node(o) => Ok(node_of(req, *o)),
            Piece::Text(t) => match Node::new_text_len(doc, t) {
                Some(n) => Ok(Some(n)),
                None => Err(unable(req)),
            },
        };
    }
    let Some(fragment) = Node::new_fragment(doc) else {
        return Err(unable(req));
    };
    for piece in list {
        match piece {
            Piece::Node(o) => {
                let Some(node) = node_of(req, *o) else {
                    discard_fragment(fragment);
                    return Err(dom_throw(req, Code::InvalidState));
                };
                match pre_insert_valid(req, this, fragment, node, None, doc) {
                    Ok(true) => {}
                    other => {
                        discard_fragment(fragment);
                        return other.map(|_| None);
                    }
                }
                if node.parent().is_some() {
                    node.unlink();
                }
                if node.kind() == Kind::Fragment {
                    let mut at = node.children();
                    while let Some(child) = at {
                        at = child.next();
                        child.unlink();
                        add_without_merging(fragment, child);
                    }
                } else {
                    add_without_merging(fragment, node);
                }
            }
            Piece::Text(t) => {
                let Some(text) = Node::new_text_len(doc, t) else {
                    discard_fragment(fragment);
                    return Err(unable(req));
                };
                add_without_merging(fragment, text);
            }
        }
    }
    Ok(Some(fragment))
}

/// `dom_cannot_create_temp_nodes`.
fn unable(req: &mut Request) -> Thrown {
    match crate::dom_error_message(
        req,
        Code::InvalidModification,
        "Unable to allocate temporary nodes",
        true,
    ) {
        Err(thrown) => thrown,
        Ok(()) => {
            php_ext::raising::fatal(req, "InternalError: a strict DOMException was not thrown")
        }
    }
}

/// `php_dom_pre_insert_helper`.
fn splice(point: Option<Node>, parent: Node, first: Node, last: Node) {
    match point {
        None => {
            match parent.children() {
                Some(_) => {
                    first.set_prev(parent.last());
                    if let Some(l) = parent.last() {
                        l.set_next(Some(first));
                    }
                }
                None => parent.set_children(Some(first)),
            }
            parent.set_last(Some(last));
        }
        Some(point) => {
            last.set_next(Some(point));
            if let Some(prev) = point.prev() {
                prev.set_next(Some(first));
                first.set_prev(Some(prev));
            }
            point.set_prev(Some(last));
            if parent.children() == Some(point) {
                parent.set_children(Some(first));
            }
        }
    }
}

/// `dom_insert_node_list_unchecked`: the namespaces reconciled only where
/// the document does not follow the standard (`modern`), whose elements
/// carry no declarations of their own.
fn insert_unchecked(node: Node, parent: Node, point: Option<Node>, modern: bool) {
    if node.kind() == Kind::Fragment {
        if let (Some(first), Some(last)) = (node.children(), node.last()) {
            splice(point, parent, first, last);
            let mut at = Some(first);
            while let Some(n) = at {
                n.set_parent(Some(parent));
                if n == last {
                    break;
                }
                at = n.next();
            }
            if !modern {
                crate::reconcile_ns_list(parent.doc(), first, last);
            }
            if let Some(doc) = parent.doc()
                && first.kind() == Kind::Dtd
            {
                doc.set_int_subset(Some(first));
                first.set_parent(Some(doc));
            }
        }
        if node.is_named() {
            node.set_children(None);
            node.set_last(None);
        } else {
            node.free_shell();
        }
    } else {
        let point = if point == Some(node) {
            node.next()
        } else {
            point
        };
        node.unlink();
        splice(point, parent, node, node);
        node.set_parent(Some(parent));
        match parent.doc() {
            Some(doc) if node.kind() == Kind::Dtd => {
                doc.set_int_subset(Some(node));
                node.set_parent(Some(doc));
            }
            doc if !modern => crate::reconcile_ns(doc, node),
            _ => {}
        }
    }
}

/// `dom_insert_node_list_cleanup`.
fn cleanup(node: Node) {
    if node.is_named() {
        return;
    }
    match node.kind() {
        Kind::Fragment => discard_fragment(node),
        Kind::Text => node.free(),
        _ => {}
    }
}

/// `php_dom_pre_insert`: the pre-insertion validity held, then the node
/// put in before `point`, or last where there is none.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><m/></r>');
/// $r = $d->documentElement;
/// $m = $r->firstChild;
/// $r->prepend($d->createElement('a'));
/// $m->insertAdjacentElement('afterend', $d->createElement('z'));
/// echo $d->saveXML($r), "\n";
/// try { $r->append($r); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// <r><a/><m/><z/></r>
/// Hierarchy Request Error
/// ```
pub(crate) fn pre_insert(
    req: &mut Request,
    this: PhpObject,
    node: Node,
    parent: Node,
    point: Option<Node>,
) -> Result<bool, Thrown> {
    if pre_insert_valid(req, this, parent, node, point, parent.doc())? {
        let modern = follows_spec(req, this);
        insert_unchecked(node, parent, point, modern);
        Ok(true)
    } else {
        cleanup(node);
        Ok(false)
    }
}

/// `append(DOMNode|string ...$nodes): void`, `DOMElement`'s and the
/// classes' that alias it.
pub(crate) fn append(
    req: &mut Request,
    this: PhpObject,
    nodes: &[Untyped<Value>],
) -> Result<(), Thrown> {
    let this = intern(req, this)?;
    let list = pieces(req, this, nodes)?;
    let parent = fetch(req, this)?;
    invalidate(req, this);
    if let Some(node) = single_node(req, this, parent, &list)? {
        pre_insert(req, this, node, parent, None)?;
    }
    Ok(())
}

/// `prepend(DOMNode|string ...$nodes): void`.
pub(crate) fn prepend(
    req: &mut Request,
    this: PhpObject,
    nodes: &[Untyped<Value>],
) -> Result<(), Thrown> {
    let this = intern(req, this)?;
    let parent = fetch(req, this)?;
    let Some(first) = parent.children() else {
        return append(req, this, nodes);
    };
    let list = pieces(req, this, nodes)?;
    invalidate(req, this);
    if let Some(node) = single_node(req, this, parent, &list)? {
        pre_insert(req, this, node, parent, Some(first))?;
    }
    Ok(())
}

/// `after(DOMNode|string ...$nodes): void`.
pub(crate) fn after(
    req: &mut Request,
    this: PhpObject,
    nodes: &[Untyped<Value>],
) -> Result<(), Thrown> {
    let this = intern(req, this)?;
    let list = pieces(req, this, nodes)?;
    let node = fetch(req, this)?;
    let Some(parent) = node.parent() else {
        return Ok(());
    };
    let mut next = node.next();
    while let Some(n) = next.filter(|n| in_list(req, &list, *n)) {
        next = n.next();
    }
    invalidate(req, this);
    if let Some(fragment) = single_node(req, this, parent, &list)? {
        pre_insert(req, this, fragment, parent, next)?;
    }
    Ok(())
}

/// `before(DOMNode|string ...$nodes): void`.
pub(crate) fn before(
    req: &mut Request,
    this: PhpObject,
    nodes: &[Untyped<Value>],
) -> Result<(), Thrown> {
    let this = intern(req, this)?;
    let list = pieces(req, this, nodes)?;
    let node = fetch(req, this)?;
    let Some(parent) = node.parent() else {
        return Ok(());
    };
    let mut prev = node.prev();
    while let Some(p) = prev.filter(|p| in_list(req, &list, *p)) {
        prev = p.prev();
    }
    invalidate(req, this);
    let fragment = single_node(req, this, parent, &list)?;
    let point = match prev {
        None => parent.children(),
        Some(p) => p.next(),
    };
    if let Some(fragment) = fragment {
        pre_insert(req, this, fragment, parent, point)?;
    }
    Ok(())
}

/// `dom_child_removal_preconditions`.
fn removable(req: &mut Request, this: PhpObject, child: Node) -> Result<bool, Thrown> {
    let strict = strict(req, this);
    let code = if crate::is_read_only(child) || child.parent().is_some_and(crate::is_read_only) {
        Some(Code::NoModificationAllowed)
    } else if child.parent().is_none() {
        Some(Code::NotFound)
    } else {
        None
    };
    match code {
        Some(code) => {
            dom_error(req, code, strict)?;
            Ok(false)
        }
        None => Ok(true),
    }
}

/// `remove(): void`.
pub(crate) fn remove(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    let this = intern(req, this)?;
    let child = fetch(req, this)?;
    if removable(req, this, child)? {
        invalidate(req, this);
        child.unlink();
    }
    Ok(())
}

/// `replaceWith(DOMNode|string ...$nodes): void`.
pub(crate) fn replace_with(
    req: &mut Request,
    this: PhpObject,
    nodes: &[Untyped<Value>],
) -> Result<(), Thrown> {
    let this = intern(req, this)?;
    let list = pieces(req, this, nodes)?;
    let child = fetch(req, this)?;
    let Some(parent) = child.parent() else {
        return Ok(());
    };
    let mut next = child.next();
    while let Some(n) = next.filter(|n| in_list(req, &list, *n)) {
        next = n.next();
    }
    if !removable(req, this, child)? {
        return Ok(());
    }
    invalidate(req, this);
    let Some(node) = single_node(req, this, parent, &list)? else {
        return Ok(());
    };
    if pre_insert_valid(req, this, parent, node, next, parent.doc())? {
        if child.parent() != Some(node) {
            child.unlink();
        }
        let modern = follows_spec(req, this);
        insert_unchecked(node, parent, next, modern);
    } else {
        cleanup(node);
    }
    Ok(())
}

/// `replaceChildren(DOMNode|string ...$nodes): void`.
pub(crate) fn replace_children(
    req: &mut Request,
    this: PhpObject,
    nodes: &[Untyped<Value>],
) -> Result<(), Thrown> {
    let this = intern(req, this)?;
    let list = pieces(req, this, nodes)?;
    let parent = fetch(req, this)?;
    invalidate(req, this);
    let Some(node) = single_node(req, this, parent, &list)? else {
        return Ok(());
    };
    if pre_insert_valid(req, this, parent, node, None, parent.doc())? {
        crate::remove_all_children(req, parent);
        pre_insert(req, this, node, parent, None)?;
    } else {
        cleanup(node);
    }
    Ok(())
}

/// `interface DOMParentNode`: what a document, a fragment and an element
/// take children through.
#[php_class(interface)]
pub(crate) struct DOMParentNode;

#[php_class]
impl DOMParentNode {
    fn append(this: This<Self>, nodes: Variadic<Untyped<Value>>);
    fn prepend(this: This<Self>, nodes: Variadic<Untyped<Value>>);
    fn replaceChildren(this: This<Self>, nodes: Variadic<Untyped<Value>>);
}

/// `interface DOMChildNode`: what an element and character data leave or
/// take siblings through.
#[php_class(interface)]
pub(crate) struct DOMChildNode;

#[php_class]
impl DOMChildNode {
    fn remove(this: This<Self>);
    fn before(this: This<Self>, nodes: Variadic<Untyped<Value>>);
    fn after(this: This<Self>, nodes: Variadic<Untyped<Value>>);
    fn replaceWith(this: This<Self>, nodes: Variadic<Untyped<Value>>);
}

/// `interface Dom\ParentNode`, which adds the selectors.
#[php_class(namespace(Dom), interface)]
pub(crate) struct ParentNode;

#[php_class]
impl ParentNode {
    fn append(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    fn prepend(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    fn replaceChildren(
        this: This<Self>,
        nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>,
    );
    fn querySelector(
        this: This<Self>,
        selectors: PhpString,
    ) -> Option<Instance<crate::element::Element>>;
    fn querySelectorAll(this: This<Self>, selectors: PhpString) -> Instance<crate::map::NodeList>;
}

/// `interface Dom\ChildNode`.
#[php_class(namespace(Dom), interface)]
pub(crate) struct ChildNode;

#[php_class]
impl ChildNode {
    fn remove(this: This<Self>);
    fn before(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    fn after(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    fn replaceWith(
        this: This<Self>,
        nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>,
    );
}
