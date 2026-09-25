//! `DOMParentNode` and `DOMChildNode` (`parentnode/tree.c`): `append`,
//! `prepend`, `replaceChildren`, `before`, `after`, `replaceWith` and
//! `remove` over a list of nodes and strings, with the pre-insertion
//! checks the legacy classes make.

use php_ext::raising::{Thrown, arg_type_error};
use php_ext::request::Request;
use php_ext::values::{PhpObject, Value, ValueRef};
use php_ext_libxml::ffi::{Kind, Node};

use crate::object::Code;
use crate::{
    Entry, class_name, dom_error, dom_throw, fetch, hierarchy_ok, instance_of, invalidate, node_of,
    strict, this_intern,
};

/// One argument of the list, checked: a node's object, or the bytes of a
/// text node to make.
enum Piece {
    Node(PhpObject),
    Text(Vec<u8>),
}

/// `dom_sanity_check_node_list_types` over the call's arguments.
fn pieces(req: &mut Request) -> Result<Vec<Piece>, Thrown> {
    let values = req.variadic_arguments(1)?;
    let mut out = Vec::with_capacity(values.len());
    for (i, value) in values.iter().enumerate() {
        let position = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let v = value.deref();
        match v.as_ref() {
            ValueRef::Object(o) if instance_of(req, *o, "DOMNode") => out.push(Piece::Node(*o)),
            ValueRef::Str(s) => out.push(Piece::Text(s.as_bytes().to_vec())),
            ValueRef::Object(o) => {
                let name = class_name(req, o.class());
                return Err(arg_type_error(
                    req,
                    position,
                    &format!("be of type DOMNode|string, {name} given"),
                ));
            }
            _ => {
                let name = v.type_name();
                return Err(arg_type_error(
                    req,
                    position,
                    &format!("be of type DOMNode|string, {name} given"),
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

/// `dom_is_pre_insert_valid_without_step_1` for the legacy classes.
fn pre_insert_valid(
    req: &mut Request,
    this: PhpObject,
    parent: Node,
    node: Node,
    child: Option<Node>,
    doc: Option<Node>,
) -> Result<bool, Thrown> {
    let strict = strict(req, this);
    let code = if !matches!(
        parent.kind(),
        Kind::Document | Kind::HtmlDocument | Kind::Element | Kind::Fragment
    ) {
        Some(Code::HierarchyRequest)
    } else if node.doc() != doc {
        Some(Code::WrongDocument)
    } else if child.is_some_and(|c| c.parent() != Some(parent)) {
        Some(Code::NotFound)
    } else if !hierarchy_ok(parent, node) || node.kind() == Kind::Attribute {
        Some(Code::HierarchyRequest)
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

/// `dom_insert_node_list_unchecked` for the legacy classes.
fn insert_unchecked(node: Node, parent: Node, point: Option<Node>) {
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
            crate::reconcile_ns_list(parent.doc(), first, last);
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
            doc => crate::reconcile_ns(doc, node),
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
        insert_unchecked(node, parent, point);
        Ok(true)
    } else {
        cleanup(node);
        Ok(false)
    }
}

/// `DOMElement::append(DOMNode|string ...$nodes): void`, shared with the
/// document and the fragment.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><m/></r>');
/// $r = $d->documentElement;
/// $r->append('a', $d->createElement('b'));
/// $r->prepend('p');
/// $r->firstChild->nextSibling->before('x');
/// $r->firstChild->nextSibling->nextSibling->after('y');
/// echo $d->saveXML($r), "\n";
/// $r->lastChild->replaceWith('z');
/// $d->getElementsByTagName('m')->item(0)->remove();
/// echo $d->saveXML($r), "\n";
/// $r->replaceChildren('only');
/// echo $d->saveXML($r), "\n";
/// try { $r->append(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// <r>px<m/>ya<b/></r>
/// <r>pxyaz</r>
/// <r>only</r>
/// DOMElement::append(): Argument #1 must be of type DOMNode|string, int given
/// ```
fn append(req: &mut Request) -> Result<Value, Thrown> {
    let list = pieces(req)?;
    let this = this_intern(req)?;
    let parent = fetch(req, this)?;
    invalidate(req, this);
    if let Some(node) = single_node(req, this, parent, &list)? {
        pre_insert(req, this, node, parent, None)?;
    }
    Ok(Value::null())
}

/// `prepend(DOMNode|string ...$nodes): void`.
fn prepend(req: &mut Request) -> Result<Value, Thrown> {
    let list = pieces(req)?;
    let this = this_intern(req)?;
    let parent = fetch(req, this)?;
    let Some(first) = parent.children() else {
        return append(req);
    };
    invalidate(req, this);
    if let Some(node) = single_node(req, this, parent, &list)? {
        pre_insert(req, this, node, parent, Some(first))?;
    }
    Ok(Value::null())
}

/// `after(DOMNode|string ...$nodes): void`.
fn after(req: &mut Request) -> Result<Value, Thrown> {
    let list = pieces(req)?;
    let this = this_intern(req)?;
    let node = fetch(req, this)?;
    let Some(parent) = node.parent() else {
        return Ok(Value::null());
    };
    let mut next = node.next();
    while let Some(n) = next.filter(|n| in_list(req, &list, *n)) {
        next = n.next();
    }
    invalidate(req, this);
    if let Some(fragment) = single_node(req, this, parent, &list)? {
        pre_insert(req, this, fragment, parent, next)?;
    }
    Ok(Value::null())
}

/// `before(DOMNode|string ...$nodes): void`.
fn before(req: &mut Request) -> Result<Value, Thrown> {
    let list = pieces(req)?;
    let this = this_intern(req)?;
    let node = fetch(req, this)?;
    let Some(parent) = node.parent() else {
        return Ok(Value::null());
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
    Ok(Value::null())
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
fn remove(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_intern(req)?;
    let child = fetch(req, this)?;
    if removable(req, this, child)? {
        invalidate(req, this);
        child.unlink();
    }
    Ok(Value::null())
}

/// `replaceWith(DOMNode|string ...$nodes): void`.
fn replace_with(req: &mut Request) -> Result<Value, Thrown> {
    let list = pieces(req)?;
    let this = this_intern(req)?;
    let child = fetch(req, this)?;
    let Some(parent) = child.parent() else {
        return Ok(Value::null());
    };
    let mut next = child.next();
    while let Some(n) = next.filter(|n| in_list(req, &list, *n)) {
        next = n.next();
    }
    if !removable(req, this, child)? {
        return Ok(Value::null());
    }
    invalidate(req, this);
    let Some(node) = single_node(req, this, parent, &list)? else {
        return Ok(Value::null());
    };
    if pre_insert_valid(req, this, parent, node, next, parent.doc())? {
        if child.parent() != Some(node) {
            child.unlink();
        }
        insert_unchecked(node, parent, next);
    } else {
        cleanup(node);
    }
    Ok(Value::null())
}

/// `replaceChildren(DOMNode|string ...$nodes): void`.
fn replace_children(req: &mut Request) -> Result<Value, Thrown> {
    let list = pieces(req)?;
    let this = this_intern(req)?;
    let parent = fetch(req, this)?;
    invalidate(req, this);
    let Some(node) = single_node(req, this, parent, &list)? else {
        return Ok(Value::null());
    };
    if pre_insert_valid(req, this, parent, node, None, parent.doc())? {
        crate::remove_all_children(parent);
        pre_insert(req, this, node, parent, None)?;
    } else {
        cleanup(node);
    }
    Ok(Value::null())
}

/// The natives each class that is a parent or a child lists.
pub(crate) const APPEND: Entry = append;
pub(crate) const PREPEND: Entry = prepend;
pub(crate) const AFTER: Entry = after;
pub(crate) const BEFORE: Entry = before;
pub(crate) const REMOVE: Entry = remove;
pub(crate) const REPLACE_WITH: Entry = replace_with;
pub(crate) const REPLACE_CHILDREN: Entry = replace_children;
