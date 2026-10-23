//! `DOMElement` (`element.c`): construction, the attribute methods, the
//! ID switches and the adjacent insertions.

use php_ext::abi::{Instance, Path, Raw, This, Union, Untyped, Variadic};
use php_ext::attr::php_class;
use php_ext::raising::{Thrown, arg_null_deprecated, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value};
use php_ext_libxml::ffi::{Kind, Name, Node, Ns, Quirks, XML_ATTRIBUTE_ID};

use crate::document::XMLNS_NS;
use crate::map::MapKind;
use crate::modern::follows_spec;
use crate::node::DOMNode;
use crate::object::{Code, Extra};
use crate::private::{HTML_NS, html_in_html_document};
use crate::{
    check_qname, dom, dom_error, dom_mut, dom_throw, empty_string, fetch, get_ns, intern,
    invalidate, mark_ids_modified, node_list_unlink, nullable_object, object_for,
    resolve_prefix_conflict, returned, set_attribute_id, strict, string, validate_and_extract,
};

/// What a qualified name finds on an element for the legacy classes: an
/// attribute, or a namespace declaration `xmlns[:p]` names.
#[derive(Clone, Copy)]
enum Found {
    Attr(Node),
    Decl(Ns),
}

/// `dom_get_attribute_or_nsdecl`: for the modern classes the attribute
/// whose qualified name is `name` (`php_dom_get_attribute_node`), for the
/// legacy ones an attribute or a namespace declaration.
fn attribute_or_decl(modern: bool, elem: Node, name: &[u8]) -> Option<Found> {
    if modern {
        return crate::map::attribute_node(elem, name).map(Found::Attr);
    }
    if let Some(at) = Name::split3(name) {
        let local = &name[at..];
        if name.starts_with(b"xmlns:") && at == 6 {
            let mut ns = elem.ns_def();
            while let Some(n) = ns {
                if n.prefix().as_deref() == Some(local) {
                    return Some(Found::Decl(n));
                }
                ns = n.next();
            }
            return None;
        }
        let prefix = &name[..at - 1];
        if let Some(ns) = elem.search_ns(Some(prefix))
            && let Some(href) = ns.href()
        {
            return elem.has_ns_prop(local, Some(&href)).map(Found::Attr);
        }
    } else if name == b"xmlns" {
        let mut ns = elem.ns_def();
        while let Some(n) = ns {
            if n.prefix().is_none() {
                return Some(Found::Decl(n));
            }
            ns = n.next();
        }
        return None;
    }
    elem.has_ns_prop(name, None).map(Found::Attr)
}

/// `dom_get_attribute_ns`: the value, and for the legacy classes a
/// namespace declaration's href where the namespace is the XMLNS one; the
/// modern classes take an empty namespace as none.
fn attribute_ns(modern: bool, elem: Node, uri: Option<&[u8]>, name: &[u8]) -> Option<Vec<u8>> {
    let uri = uri.filter(|u| !modern || !u.is_empty());
    if let Some(v) = elem.get_ns_prop(name, uri) {
        return Some(v);
    }
    if !modern && uri == Some(XMLNS_NS) {
        return decl_named(elem, Some(name)).and_then(Ns::href);
    }
    None
}

/// `dom_get_nsdecl`: the declaration of `prefix` (the default one for
/// none or empty) on the element itself.
fn decl_named(elem: Node, prefix: Option<&[u8]>) -> Option<Ns> {
    let prefix = prefix.filter(|p| !p.is_empty());
    let mut ns = elem.ns_def();
    while let Some(n) = ns {
        let hit = match prefix {
            None => n.prefix().is_none() && n.href().is_some(),
            Some(p) => n.prefix().as_deref() == Some(p),
        };
        if hit {
            return Some(n);
        }
        ns = n.next();
    }
    None
}

/// A namespace declaration handed out as a `DOMNameSpaceNode`, the
/// element kept alive by it.
fn namespace_node(
    req: &mut Request,
    this: PhpObject,
    elem: Node,
    ns: Ns,
) -> Result<Owner<Value>, Thrown> {
    let Some(fake) = Node::fake_namespace_decl(elem, ns.href().as_deref(), ns.prefix().as_deref())
    else {
        return Err(dom_throw(req, Code::InvalidState));
    };
    let value = object_for(req, fake, Some(this))?;
    if let Some(object) = value.as_object() {
        let parent = Owner::take(req, this);
        match dom_mut(req, object) {
            Some(d) => d.extra = Extra::NamespaceParent(parent),
            None => parent.give_up(req),
        }
    }
    Ok(value)
}

/// The modern branch of `DOMElement::setAttribute`: the attribute the
/// qualified name finds takes the value as its one text child, or a new
/// one with no namespace is made, its name lowercased for an HTML element
/// of an HTML document.
fn set_attribute_modern(req: &mut Request, this: PhpObject, elem: Node, name: &[u8], value: &[u8]) {
    if let Some(attr) = crate::map::attribute_node(elem, name) {
        crate::attr_value_will_change(req, this, attr);
        crate::remove_all_children(req, attr);
        if let Some(text) = Node::new_text(attr.doc(), value) {
            attr.add_child(text);
        }
        return;
    }
    let processed = if html_in_html_document(elem) {
        name.to_ascii_lowercase()
    } else {
        name.to_vec()
    };
    if let Some(attr) = elem.set_ns_prop(None, &processed, value) {
        register_attribute_id(req, this, attr);
    }
}

/// `dom_check_register_attribute_id`: the IDs marked changed, and an
/// `id` attribute of no namespace made an ID in an HTML document, as
/// XML's own ID rules are left to the DTD.
fn register_attribute_id(req: &mut Request, this: PhpObject, attr: Node) {
    mark_ids_modified(req, this);
    if attr.atype() != XML_ATTRIBUTE_ID
        && attr.doc().is_some_and(|d| d.kind() == Kind::HtmlDocument)
        && attr.ns().is_none()
        && attr.name() == b"id"
    {
        attr.set_atype(XML_ATTRIBUTE_ID);
    }
}

/// `php_dom_libxml_ns_mapper_get_ns_raw_prefix_string` over the mapper
/// of `object`'s document: the one declaration of `uri` under `prefix`,
/// none where both are empty.
///
/// ```php
/// $d = Dom\XMLDocument::createEmpty();
/// $e = $d->createElementNS('urn:p', 'p:e');
/// $e->setAttributeNS('urn:p', 'p:a', '1');
/// $e->setAttributeNS('urn:p', 'q:b', '2');
/// $a = $e->getAttributeNodeNS('urn:p', 'a');
/// $b = $e->getAttributeNodeNS('urn:p', 'b');
/// var_dump($e->prefix, $a->prefix, $b->prefix, $b->namespaceURI, $d->createElementNS(null, 'x')->namespaceURI);
/// ```
/// ```expect
/// string(1) "p"
/// string(1) "p"
/// string(1) "q"
/// string(5) "urn:p"
/// NULL
/// ```
pub(crate) fn mapped_ns(
    req: &mut Request,
    object: PhpObject,
    doc: Option<Node>,
    prefix: Option<&[u8]>,
    uri: Option<PhpString>,
) -> Result<Option<Ns>, Thrown> {
    let uri = crate::private::Handed::argument(uri);
    let ns = crate::private::with_mapper(req, object, |mapper, req| {
        mapper.get_raw_prefix(req, doc, prefix.unwrap_or_default(), uri)
    })?;
    Ok(ns.flatten())
}

/// `dom_remove_attribute`.
fn remove_found(elem: Node, found: Found) {
    match found {
        Found::Attr(a) => {
            if a.object().is_none() {
                node_list_unlink(a.children());
                a.unlink();
                a.free();
            } else {
                a.unlink();
            }
        }
        Found::Decl(ns) => {
            let Some(first) = elem.ns_def() else {
                return;
            };
            if first == ns {
                elem.set_ns_def(ns.next());
            } else {
                let mut prev = first;
                while let Some(cur) = prev.next() {
                    if cur == ns {
                        prev.set_next(cur.next());
                        break;
                    }
                    prev = cur;
                }
            }
            ns.set_next(None);
            ns.move_to_old(elem.doc());
            deep_ns_redef(elem, ns);
        }
    }
}

/// `dom_deep_ns_redef`: every use of `removed` under `node` given a
/// declaration closest to it.
fn deep_ns_redef(node: Node, removed: Ns) {
    let mut work: Vec<(Node, Option<Ns>)> = vec![(node, None)];
    while let Some((current, mut defined)) = work.pop() {
        if current.ns() == Some(removed) {
            if defined.is_none() {
                defined = current.new_ns(
                    &removed.href().unwrap_or_default(),
                    removed.prefix().as_deref(),
                );
            }
            current.set_ns(defined);
        }
        let mut attr = current.properties();
        while let Some(a) = attr {
            if a.ns() == Some(removed) {
                if defined.is_none() {
                    defined = current.new_ns(
                        &removed.href().unwrap_or_default(),
                        removed.prefix().as_deref(),
                    );
                }
                a.set_ns(defined);
            }
            attr = a.next();
        }
        let mut child = current.children();
        while let Some(c) = child {
            if c.kind() == Kind::Element {
                work.push((c, defined));
            }
            child = c.next();
        }
    }
}

/// `dom_set_attribute_ns_legacy`.
fn set_attribute_ns_legacy(
    req: &mut Request,
    this: PhpObject,
    elem: Node,
    uri: Option<&[u8]>,
    name: &[u8],
    value: &[u8],
) -> Result<(), Thrown> {
    if name.is_empty() {
        return Err(arg_value_error(req, 2, "not be empty"));
    }
    let mut strict = strict(req, this);
    let uri_len = uri.map_or(0, <[u8]>::len);
    let code = match check_qname(name, uri_len) {
        Err(code) => Some(code),
        Ok((local, prefix)) => {
            mark_ids_modified(req, this);
            match uri.filter(|u| !u.is_empty()) {
                Some(uri) => {
                    if let Some(existing) = elem
                        .has_ns_prop(&local, Some(uri))
                        .filter(|a| a.kind() != Kind::AttributeDecl)
                    {
                        node_list_unlink(existing.children());
                    }
                    let is_xmlns = (prefix.as_deref() == Some(b"xmlns")
                        || (prefix.is_none() && local == b"xmlns"))
                        && uri == XMLNS_NS;
                    let mut ns = if is_xmlns {
                        decl_named(elem, if prefix.is_none() { None } else { Some(&local) })
                    } else {
                        let mut found = elem.search_ns_by_href(elem.doc(), uri);
                        if let Some(f) = found.filter(|f| f.prefix().is_none()) {
                            let mut next = f.next();
                            let mut better = None;
                            while let Some(n) = next {
                                if n.prefix().is_some() && n.href().as_deref() == Some(uri) {
                                    better = Some(n);
                                    break;
                                }
                                next = n.next();
                            }
                            found = better.or_else(|| {
                                resolve_prefix_conflict(elem, &f.href().unwrap_or_default())
                            });
                        }
                        found
                    };
                    let mut code = None;
                    match ns {
                        None => {
                            if is_xmlns {
                                elem.new_ns(
                                    value,
                                    if prefix.is_none() { None } else { Some(&local) },
                                );
                            } else {
                                match get_ns(elem, uri, prefix.as_deref()) {
                                    Ok(n) => ns = Some(n),
                                    Err(c) => code = Some(c),
                                }
                            }
                            elem.reconciliate_ns(elem.doc());
                        }
                        Some(n) if is_xmlns => n.set_href(value),
                        Some(_) => {}
                    }
                    if code.is_none() && !is_xmlns {
                        elem.set_ns_prop(ns, &local, value);
                    }
                    code
                }
                None => {
                    if Name::valid(&local) {
                        if let Some(existing) = elem
                            .has_prop(&local)
                            .filter(|a| a.kind() != Kind::AttributeDecl)
                        {
                            node_list_unlink(existing.children());
                        }
                        elem.set_prop(&local, value);
                        None
                    } else {
                        strict = true;
                        Some(Code::InvalidCharacter)
                    }
                }
            }
        }
    };
    match code {
        Some(code) => dom_error(req, code, strict),
        None => Ok(()),
    }
}

/// `dom_set_attribute_ns_modern`: the name validated and split, its
/// namespace the mapper's one declaration.
fn set_attribute_ns_modern(
    req: &mut Request,
    this: PhpObject,
    elem: Node,
    namespace: Option<PhpString>,
    name: &[u8],
    value: &[u8],
) -> Result<(), Thrown> {
    let uri = namespace.as_ref().map(PhpString::as_bytes);
    let (local, prefix) = match validate_and_extract(uri, name) {
        Ok(parts) => parts,
        Err(code) => return Err(dom_throw(req, code)),
    };
    let ns = mapped_ns(req, this, elem.doc(), prefix.as_deref(), namespace)?;
    match elem.set_ns_prop(ns, &local, value) {
        Some(attr) => {
            register_attribute_id(req, this, attr);
            Ok(())
        }
        None => Err(dom_throw(req, Code::InvalidState)),
    }
}

/// `dom_element_get_in_scope_namespace_info`: the `xmlns` attributes of
/// the element and its ancestors, the nearest per prefix, as
/// `Dom\NamespaceInfo` objects appended to `out`, the outermost first; a
/// default declared empty is left out.
fn in_scope_namespace_info(
    req: &mut Request,
    this: PhpObject,
    node: Node,
    out: &mut Owner<PhpArray>,
) -> Result<(), Thrown> {
    let doc = node.doc();
    let mut table: Vec<(Vec<u8>, Option<Ns>)> = Vec::new();
    let mut at = Some(node);
    while let Some(cur) = at {
        if cur.kind() == Kind::Element {
            let mut attrs = Vec::new();
            let mut attr = cur.properties();
            while let Some(a) = attr {
                attrs.push(a);
                attr = a.next();
            }
            for a in attrs.into_iter().rev() {
                let Some(ns) = a.ns().filter(|ns| ns.href().as_deref() == Some(XMLNS_NS)) else {
                    continue;
                };
                let Some(content) = a.children().and_then(Node::content) else {
                    continue;
                };
                let prefix = ns.prefix().map(|_| a.name());
                let key = prefix.clone().unwrap_or_default();
                if table.iter().any(|(k, _)| *k == key) {
                    continue;
                }
                let found = crate::private::with_mapper(req, this, |mapper, req| {
                    mapper.get_raw(req, doc, prefix.as_deref(), Some(&content))
                })?
                .flatten();
                table.push((key, found));
            }
        }
        at = cur.parent();
    }
    let class = crate::class(req, "Dom\\NamespaceInfo")?;
    for (prefix, ns) in table.into_iter().rev() {
        let href = ns.and_then(Ns::href).filter(|h| !h.is_empty());
        if prefix.is_empty() && href.is_none() {
            continue;
        }
        let info = php_ext::call::new_object(req, class)?;
        let prefix = if prefix.is_empty() {
            Owner::null()
        } else {
            string(req, &prefix)?
        };
        info.set_slot(req, 0, prefix)?;
        let uri = crate::nullable_string(req, href.as_deref())?;
        info.set_slot(req, 1, uri)?;
        let element = match object_for(req, node, Some(this)) {
            Ok(element) => element,
            Err(thrown) => {
                info.give_up(req);
                return Err(thrown);
            }
        };
        info.set_slot(req, 2, element.into_value())?;
        out.append(req, info.into_value())?;
    }
    Ok(())
}

/// `enum Dom\AdjacentPosition: string`: where an adjacent insertion goes,
/// `dom_insert_adjacent`'s four places. The legacy classes' `$where` is a
/// string read as one of them ([`AdjacentPosition::of`]).
#[php_class(namespace(Dom), backed = string)]
pub(crate) enum AdjacentPosition {
    #[php(value = "beforebegin")]
    BeforeBegin,
    #[php(value = "afterbegin")]
    AfterBegin,
    #[php(value = "beforeend")]
    BeforeEnd,
    #[php(value = "afterend")]
    AfterEnd,
}

#[php_class]
impl AdjacentPosition {}

impl AdjacentPosition {
    /// A legacy `$where`, compared case-blind with the cases' values as
    /// `dom_insert_adjacent` compares it.
    fn of(name: &[u8]) -> Option<AdjacentPosition> {
        Some(match name.to_ascii_lowercase().as_slice() {
            b"beforebegin" => AdjacentPosition::BeforeBegin,
            b"afterbegin" => AdjacentPosition::AfterBegin,
            b"beforeend" => AdjacentPosition::BeforeEnd,
            b"afterend" => AdjacentPosition::AfterEnd,
            _ => return None,
        })
    }
}

/// What `dom_insert_adjacent` answered.
enum Inserted {
    Done,
    NoParent,
    Failed,
}

/// `dom_insert_adjacent`: `place` none for a legacy `$where` that names no
/// place, php's syntax error.
fn insert_adjacent(
    req: &mut Request,
    this: PhpObject,
    elem: Node,
    place: Option<AdjacentPosition>,
    other: Node,
) -> Result<Inserted, Thrown> {
    let Some(place) = place else {
        let strict = strict(req, this);
        dom_error(req, Code::Syntax, strict)?;
        return Ok(Inserted::Failed);
    };
    let (parent, before) = match place {
        AdjacentPosition::BeforeBegin => match elem.parent() {
            Some(p) => (p, Some(elem)),
            None => return Ok(Inserted::NoParent),
        },
        AdjacentPosition::AfterBegin => (elem, elem.children()),
        AdjacentPosition::BeforeEnd => (elem, None),
        AdjacentPosition::AfterEnd => match elem.parent() {
            Some(p) => (p, elem.next()),
            None => return Ok(Inserted::NoParent),
        },
    };
    let Some(doc) = elem.doc() else {
        return Ok(Inserted::Failed);
    };
    if !crate::adopt(req, this, other, doc)? {
        return Ok(Inserted::Failed);
    }
    if !crate::parent::pre_insert(req, this, other, parent, before)? {
        return Ok(Inserted::Failed);
    }
    Ok(Inserted::Done)
}

/// `dom_element_insert_adjacent_element`.
fn insert_adjacent_element(
    req: &mut Request,
    this: PhpObject,
    place: Option<AdjacentPosition>,
    other_obj: PhpObject,
) -> Result<Owner<Value>, Thrown> {
    let elem = fetch(req, this)?;
    let other = fetch(req, other_obj)?;
    match insert_adjacent(req, this, elem, place, other)? {
        Inserted::Done => object_for(req, other, Some(other_obj)),
        Inserted::NoParent | Inserted::Failed => Ok(Owner::null()),
    }
}

/// `dom_element_insert_adjacent_text`.
fn insert_adjacent_text(
    req: &mut Request,
    this: PhpObject,
    place: Option<AdjacentPosition>,
    data: &[u8],
) -> Result<(), Thrown> {
    let elem = fetch(req, this)?;
    let Some(text) = Node::new_text_len(elem.doc(), data) else {
        return Ok(());
    };
    match insert_adjacent(req, this, elem, place, text)? {
        Inserted::Done => {}
        Inserted::NoParent => text.free(),
        Inserted::Failed => {
            if text.parent().is_none() {
                text.free();
            }
        }
    }
    Ok(())
}

/// `DOMElement::setAttribute`'s body: the modern branch where the
/// document follows the standard.
fn set_attribute(
    req: &mut Request,
    this: PhpObject,
    name: &[u8],
    value: &[u8],
) -> Result<Owner<Value>, Thrown> {
    if name.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if !Name::valid(name) {
        return Err(dom_throw(req, Code::InvalidCharacter));
    }
    let elem = fetch(req, this)?;
    if follows_spec(req, this) {
        set_attribute_modern(req, this, elem, name, value);
        return Ok(Owner::null());
    }
    match attribute_or_decl(false, elem, name) {
        Some(Found::Attr(a)) if a.kind() == Kind::Attribute => {
            crate::attr_value_will_change(req, this, a);
            node_list_unlink(a.children());
        }
        Some(Found::Decl(_)) => return Ok(Owner::bool(false)),
        _ => {}
    }
    if name == b"xmlns" {
        return Ok(match elem.new_ns(value, None) {
            Some(_) => Owner::bool(true),
            None => return Err(arg_value_error(req, 1, "be a valid XML attribute")),
        });
    }
    match elem.set_prop(name, value) {
        Some(a) => object_for(req, a, Some(this)),
        None => Err(arg_value_error(req, 1, "be a valid XML attribute")),
    }
}

/// `dom_element_set_attribute_node_common`: the modern classes refuse an
/// attribute another element owns and adopt one of another document,
/// where the legacy ones refuse it.
fn set_attribute_node(
    req: &mut Request,
    this: PhpObject,
    attr: PhpObject,
    (use_ns, modern): (bool, bool),
) -> Result<Owner<Value>, Thrown> {
    let elem = fetch(req, this)?;
    let attr = fetch(req, attr)?;
    if modern {
        if attr.parent().is_some_and(|p| p != elem) {
            return Err(dom_throw(req, Code::InuseAttribute));
        }
        if let (Some(from), Some(doc)) = (attr.doc(), elem.doc())
            && from != doc
        {
            crate::adopt(req, this, attr, doc)?;
        }
    } else if attr.doc().is_some() && attr.doc() != elem.doc() {
        let strict = strict(req, this);
        dom_error(req, Code::WrongDocument, strict)?;
        return Ok(Owner::bool(false));
    }
    let existing = match attr.ns().and_then(Ns::href).filter(|_| use_ns) {
        Some(href) => elem.has_ns_prop(&attr.name(), Some(&href)),
        None => elem.has_prop(&attr.name()),
    }
    .filter(|a| a.kind() != Kind::AttributeDecl);
    if let Some(old) = existing {
        if old == attr {
            return Ok(Owner::null());
        }
        old.unlink();
    }
    if attr.parent().is_some() {
        attr.unlink();
    }
    if attr.doc().is_none()
        && elem.doc().is_some()
        && let Some(shared) = crate::shared_doc(req, this)
    {
        crate::set_document_ref_pointers_attr(req, attr, shared);
    }
    elem.add_child(attr);
    if modern {
        register_attribute_id(req, this, attr);
    } else {
        mark_ids_modified(req, this);
        crate::reconcile_attribute_namespace(attr);
    }
    nullable_object(req, existing, Some(this))
}

/// `DOMElement::setIdAttributeNS`'s body, which `Dom\Element`'s shares.
fn set_id_attribute_ns(
    req: &mut Request,
    this: PhpObject,
    uri: &[u8],
    name: &[u8],
    is_id: bool,
) -> Result<(), Thrown> {
    let elem = fetch(req, this)?;
    if let Some(a) = elem
        .has_ns_prop(name, Some(uri))
        .filter(|a| a.kind() != Kind::AttributeDecl)
    {
        set_attribute_id(req, this, a, is_id);
    } else {
        let strict = strict(req, this);
        dom_error(req, Code::NotFound, strict)?;
    }
    Ok(())
}

/// `DOMElement::getAttributeNodeNS`'s body.
fn get_attribute_node_ns(
    req: &mut Request,
    this: PhpObject,
    uri: Option<&[u8]>,
    name: &[u8],
) -> Result<Owner<Value>, Thrown> {
    let elem = fetch(req, this)?;
    let modern = follows_spec(req, this);
    let uri = uri.filter(|u| !modern || !u.is_empty());
    match elem.has_ns_prop(name, uri) {
        Some(a) => object_for(req, a, Some(this)),
        None if !modern && uri == Some(XMLNS_NS) => match decl_named(elem, Some(name)) {
            Some(ns) => namespace_node(req, this, elem, ns),
            None => Ok(Owner::null()),
        },
        None => Ok(Owner::null()),
    }
}

/// `DOMElement::getAttributeNode`'s body.
fn get_attribute_node(
    req: &mut Request,
    this: PhpObject,
    name: &[u8],
) -> Result<Owner<Value>, Thrown> {
    let elem = fetch(req, this)?;
    let modern = follows_spec(req, this);
    match attribute_or_decl(modern, elem, name) {
        None if modern => Ok(Owner::null()),
        None => Ok(Owner::bool(false)),
        Some(Found::Decl(ns)) => namespace_node(req, this, elem, ns),
        Some(Found::Attr(a)) => object_for(req, a, Some(this)),
    }
}

/// `DOMElement::removeAttributeNode`'s body, `Dom\Element`'s too.
fn remove_attribute_node(
    req: &mut Request,
    this: PhpObject,
    attr: PhpObject,
) -> Result<Owner<Value>, Thrown> {
    let elem = fetch(req, this)?;
    let attr = fetch(req, attr)?;
    if attr.parent() != Some(elem) {
        let strict = strict(req, this);
        dom_error(req, Code::NotFound, strict)?;
        return Ok(Owner::bool(false));
    }
    attr.unlink();
    object_for(req, attr, Some(this))
}

/// `class DOMElement extends DOMNode implements DOMParentNode,
/// DOMChildNode`: its nine properties `@virtual`, the attribute methods,
/// the ID switches, the adjacent insertions and the parent and child
/// methods, which the other classes alias.
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
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r xmlns:p="urn:p" a="1" p:b="2"><c/></r>');
/// $r = $d->documentElement;
/// print_r($r->getAttributeNames());
/// var_dump($r->hasAttribute('a'), $r->hasAttributeNS('urn:p', 'b'), $r->getAttributeNodeNS('urn:p', 'b')->value);
/// var_dump($r->getElementsByTagNameNS('*', 'c')->length);
/// var_dump($r->setAttributeNodeNS(new DOMAttr('x', 'v')), $r->removeAttributeNode($r->getAttributeNode('x'))->value);
/// $r->setIdAttributeNS('urn:p', 'b', true);
/// $r->setIdAttributeNode($r->getAttributeNode('a'), true);
/// var_dump($d->getElementById('2') === $r, $d->getElementById('1') === $r);
/// $r->firstChild->insertAdjacentText('beforebegin', 't');
/// echo $d->saveXML($r), "\n";
/// ```
/// ```expect
/// Array
/// (
///     [0] => xmlns:p
///     [1] => a
///     [2] => p:b
/// )
/// bool(true)
/// bool(true)
/// string(1) "2"
/// int(1)
/// NULL
/// string(1) "v"
/// bool(true)
/// bool(true)
/// <r xmlns:p="urn:p" a="1" p:b="2">t<c/></r>
/// ```
#[php_class(
    extends(DOMNode),
    implements(crate::parent::DOMParentNode, crate::parent::DOMChildNode)
)]
pub(crate) struct DOMElement;

#[php_class]
impl DOMElement {
    #[php(property, virtual)]
    const tagName: PhpString;
    #[php(property, virtual)]
    const className: PhpString;
    #[php(property, virtual)]
    const id: PhpString;
    #[php(property, virtual)]
    const schemaTypeInfo: Value;
    #[php(property, virtual)]
    const firstElementChild: Option<Instance<DOMElement>>;
    #[php(property, virtual)]
    const lastElementChild: Option<Instance<DOMElement>>;
    #[php(property, virtual)]
    const childElementCount: i64;
    #[php(property, virtual)]
    const previousElementSibling: Option<Instance<DOMElement>>;
    #[php(property, virtual)]
    const nextElementSibling: Option<Instance<DOMElement>>;

    /// `DOMElement::__construct(string $qualifiedName, ?string $value = null, string $namespace = "")`.
    ///
    /// ```php
    /// $e = new DOMElement('p:e', 'v', 'urn:p');
    /// var_dump($e->nodeName, $e->namespaceURI, $e->textContent, $e->ownerDocument);
    /// try { new DOMElement('p:e'); } catch (DOMException $x) { echo $x->getMessage(), "\n"; }
    /// try { new DOMElement('1'); } catch (DOMException $x) { echo $x->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(3) "p:e"
    /// string(5) "urn:p"
    /// string(1) "v"
    /// NULL
    /// Namespace Error
    /// Invalid Character Error
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: PhpString,
        #[php(default = null)] value: Option<PhpString>,
        #[php(default = "")] namespace: PhpString,
    ) -> Result<(), Thrown> {
        let name = qualifiedName.as_bytes();
        let uri = namespace.as_bytes();
        let this = this.object();
        if !Name::valid(name) {
            return Err(dom_throw(req, Code::InvalidCharacter));
        }
        let node = if uri.is_empty() {
            if Name::split2(name).is_some() {
                return Err(dom_throw(req, Code::Namespace));
            }
            Node::new_element(None, None, name, None)
        } else {
            let made = check_qname(name, uri.len()).and_then(|(local, prefix)| {
                let node = Node::new_element(None, None, &local, None).ok_or(Code::InvalidState)?;
                match get_ns(node, uri, prefix.as_deref()) {
                    Ok(ns) => {
                        node.set_ns_call(Some(ns));
                        Ok(node)
                    }
                    Err(code) => {
                        node.free();
                        Err(code)
                    }
                }
            });
            match made {
                Ok(n) => Some(n),
                Err(code) => return Err(dom_throw(req, code)),
            }
        };
        let Some(node) = node else {
            return Err(dom_throw(req, Code::InvalidState));
        };
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            node.set_content_len(v.as_bytes());
        }
        crate::rebind(req, this, node);
        Ok(())
    }

    /// `DOMElement::getAttribute(string $qualifiedName): string`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r xmlns:p="urn:p" a="1" p:b="2"/>');
    /// $r = $d->documentElement;
    /// var_dump($r->getAttribute('a'), $r->getAttribute('p:b'), $r->getAttribute('xmlns:p'), $r->getAttribute('z'), $r->hasAttribute('p:b'), $r->hasAttribute('z'));
    /// print_r($r->getAttributeNames());
    /// ```
    /// ```expect
    /// string(1) "1"
    /// string(1) "2"
    /// string(5) "urn:p"
    /// string(0) ""
    /// bool(true)
    /// bool(false)
    /// Array
    /// (
    ///     [0] => xmlns:p
    ///     [1] => a
    ///     [2] => p:b
    /// )
    /// ```
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p" a="" p:b="2"/>');
    /// $r = $d->documentElement;
    /// $r->toggleAttribute('c');
    /// var_dump($r->getAttribute('a'), $r->getAttribute('p:b'), $r->getAttribute('xmlns:p'), $r->getAttribute('c'), $r->getAttribute('z'));
    /// print_r($r->getAttributeNames());
    /// ```
    /// ```expect
    /// string(0) ""
    /// string(1) "2"
    /// string(5) "urn:p"
    /// NULL
    /// NULL
    /// Array
    /// (
    ///     [0] => xmlns:p
    ///     [1] => a
    ///     [2] => p:b
    ///     [3] => c
    /// )
    /// ```
    #[php(tentative)]
    fn getAttribute(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: PhpString,
    ) -> Result<Owner<Raw<PhpString>>, Thrown> {
        let this = this.object();
        let elem = fetch(req, this)?;
        let modern = follows_spec(req, this);
        let value = match attribute_or_decl(modern, elem, qualifiedName.as_bytes()) {
            Some(Found::Attr(a)) if a.kind() == Kind::Attribute => a.list_string(),
            Some(Found::Decl(ns)) => ns.href(),
            _ => None,
        };
        let value = match value {
            Some(v) => string(req, &v)?,
            None if modern => Owner::null(),
            None => empty_string(req)?,
        };
        returned(value)
    }

    /// `DOMElement::getAttributeNames(): array`.
    fn getAttributeNames(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let this = this.object();
        let elem = fetch(req, this)?;
        let modern = follows_spec(req, this);
        let mut out = PhpArray::new(req)?;
        let mut ns = if modern { None } else { elem.ns_def() };
        while let Some(n) = ns {
            let name = match n.prefix() {
                Some(p) => [b"xmlns:".as_slice(), &p].concat(),
                None => b"xmlns".to_vec(),
            };
            let v = string(req, &name)?;
            out.append(req, v)?;
            ns = n.next();
        }
        let mut attr = elem.properties();
        while let Some(a) = attr {
            let v = string(req, &crate::qualified_name(a))?;
            out.append(req, v)?;
            attr = a.next();
        }
        Ok(out)
    }

    /// `DOMElement::getAttributeNS(?string $namespace, string $localName): string`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r xmlns:p="urn:p" p:b="2"/>');
    /// $r = $d->documentElement;
    /// var_dump($r->getAttributeNS('urn:p', 'b'), $r->getAttributeNS('http://www.w3.org/2000/xmlns/', 'p'), $r->hasAttributeNS('urn:p', 'b'), $r->hasAttributeNS(null, 'b'));
    /// ```
    /// ```expect
    /// string(1) "2"
    /// string(5) "urn:p"
    /// bool(true)
    /// bool(false)
    /// ```
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p" p:b="2" c="3"/>');
    /// $r = $d->documentElement;
    /// var_dump($r->getAttributeNS('urn:p', 'b'), $r->getAttributeNS('', 'c'), $r->getAttributeNS('http://www.w3.org/2000/xmlns/', 'p'), $r->getAttributeNS('urn:p', 'z'), $r->hasAttributeNS(null, 'c'), $r->hasAttributeNS('urn:q', 'b'));
    /// ```
    /// ```expect
    /// string(1) "2"
    /// string(1) "3"
    /// string(5) "urn:p"
    /// NULL
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getAttributeNS(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<PhpString>,
        localName: PhpString,
    ) -> Result<Owner<Raw<PhpString>>, Thrown> {
        let this = this.object();
        let elem = fetch(req, this)?;
        let modern = follows_spec(req, this);
        let uri = namespace.as_ref().map(PhpString::as_bytes);
        let value = match attribute_ns(modern, elem, uri, localName.as_bytes()) {
            Some(v) => string(req, &v)?,
            None if modern => Owner::null(),
            None => empty_string(req)?,
        };
        returned(value)
    }

    /// `DOMElement::getAttributeNode(string $qualifiedName)`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r xmlns:p="urn:p" a="1"/>');
    /// $r = $d->documentElement;
    /// var_dump(get_class($r->getAttributeNode('a')), $r->getAttributeNode('z'));
    /// $n = $r->getAttributeNode('xmlns:p');
    /// var_dump(get_class($n), $n->nodeName, $n->nodeValue, $n->prefix, $n->localName, $n->namespaceURI, $n->parentNode === $r);
    /// ```
    /// ```expect
    /// string(7) "DOMAttr"
    /// bool(false)
    /// string(16) "DOMNameSpaceNode"
    /// string(7) "xmlns:p"
    /// string(5) "urn:p"
    /// string(1) "p"
    /// string(1) "p"
    /// string(5) "urn:p"
    /// bool(true)
    /// ```
    ///
    /// ```php
    /// $d = Dom\HTMLDocument::createEmpty();
    /// $e = $d->createElement('p');
    /// $e->setAttribute('Data-X', '1');
    /// var_dump($e->getAttributeNode('DATA-x')->localName, $e->getAttributeNode('xmlns:p'), $e->hasAttribute('data-X'));
    /// $x = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p" p:a="1"/>')->documentElement;
    /// $a = $x->getAttributeNodeNS('urn:p', 'a');
    /// var_dump(get_class($x->getAttributeNode('xmlns:p')), $x->getAttributeNode('p:a')->value, $a->prefix, $a->localName, $x->getAttributeNodeNS('', 'a'));
    /// ```
    /// ```expect
    /// string(6) "data-x"
    /// NULL
    /// bool(true)
    /// string(8) "Dom\Attr"
    /// string(1) "1"
    /// string(1) "p"
    /// string(1) "a"
    /// NULL
    /// ```
    fn getAttributeNode(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: PhpString,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        get_attribute_node(req, this.object(), qualifiedName.as_bytes()).map(Untyped)
    }

    /// `DOMElement::getAttributeNodeNS(?string $namespace, string $localName)`.
    fn getAttributeNodeNS(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<PhpString>,
        localName: PhpString,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        let uri = namespace.as_ref().map(PhpString::as_bytes);
        get_attribute_node_ns(req, this.object(), uri, localName.as_bytes()).map(Untyped)
    }

    /// `DOMDocument::getElementsByTagName(string $qualifiedName): DOMNodeList`,
    /// shared with `DOMElement`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r xmlns:p="urn:p"><a/><p:a/><b><a/></b></r>');
    /// var_dump($d->getElementsByTagName('a')->length, $d->getElementsByTagName('*')->length, $d->getElementsByTagNameNS('urn:p', '*')->length, $d->getElementsByTagNameNS(null, 'a')->length, $d->documentElement->getElementsByTagName('a')->item(2)->parentNode->nodeName);
    /// ```
    /// ```expect
    /// int(3)
    /// int(5)
    /// int(1)
    /// int(2)
    /// string(1) "b"
    /// ```
    #[php(tentative)]
    fn getElementsByTagName(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: Path,
    ) -> Result<Owner<Raw<Instance<crate::map::DOMNodeList>>>, Thrown> {
        let this = intern(req, this.object())?;
        let list = crate::create(
            req,
            this,
            "DOMNodeList",
            MapKind::ByTagName {
                local: qualifiedName.0.as_bytes().to_vec(),
                ns: None,
                qname: false,
            },
        )?;
        returned(list)
    }

    /// `getElementsByTagNameNS(?string $namespace, string $localName): DOMNodeList`.
    #[php(tentative)]
    fn getElementsByTagNameNS(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<Path>,
        localName: Path,
    ) -> Result<Owner<Raw<Instance<crate::map::DOMNodeList>>>, Thrown> {
        let list = crate::document::elements_by_tag_name_ns(
            req,
            this.object(),
            namespace,
            localName,
            "DOMNodeList",
        )?;
        returned(list)
    }

    /// `DOMElement::hasAttribute(string $qualifiedName): bool`.
    #[php(tentative)]
    fn hasAttribute(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: PhpString,
    ) -> Result<bool, Thrown> {
        let this = this.object();
        let elem = fetch(req, this)?;
        let modern = follows_spec(req, this);
        Ok(attribute_or_decl(modern, elem, qualifiedName.as_bytes()).is_some())
    }

    /// `DOMElement::hasAttributeNS(?string $namespace, string $localName): bool`.
    #[php(tentative)]
    fn hasAttributeNS(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<PhpString>,
        localName: PhpString,
    ) -> Result<bool, Thrown> {
        let this = this.object();
        let elem = fetch(req, this)?;
        let modern = follows_spec(req, this);
        let uri = namespace.as_ref().map(PhpString::as_bytes);
        Ok(attribute_ns(modern, elem, uri, localName.as_bytes()).is_some())
    }

    /// `DOMElement::removeAttribute(string $qualifiedName): bool`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r xmlns:p="urn:p" a="1"><p:c/></r>');
    /// $r = $d->documentElement;
    /// var_dump($r->removeAttribute('a'), $r->removeAttribute('z'), $r->removeAttribute('xmlns:p'));
    /// echo $d->saveXML($r), "\n";
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// <r><p:c xmlns:p="urn:p"/></r>
    /// ```
    #[php(tentative)]
    fn removeAttribute(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: PhpString,
    ) -> Result<bool, Thrown> {
        let this = this.object();
        let elem = fetch(req, this)?;
        let modern = follows_spec(req, this);
        match attribute_or_decl(modern, elem, qualifiedName.as_bytes()) {
            None => Ok(false),
            Some(found) => {
                remove_found(elem, found);
                Ok(true)
            }
        }
    }

    /// `DOMElement::removeAttributeNS(?string $namespace, string $localName): void`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p" a="1" p:b="2"/>');
    /// $r = $d->documentElement;
    /// $r->removeAttributeNS('', 'a');
    /// $r->removeAttributeNS('urn:p', 'b');
    /// $r->removeAttributeNS('http://www.w3.org/2000/xmlns/', 'p');
    /// var_dump($r->attributes->length);
    /// ```
    /// ```expect
    /// int(0)
    /// ```
    #[php(tentative)]
    fn removeAttributeNS(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<PhpString>,
        localName: PhpString,
    ) -> Result<(), Thrown> {
        let name = localName.as_bytes();
        let this = this.object();
        let elem = fetch(req, this)?;
        let modern = follows_spec(req, this);
        let uri = namespace
            .as_ref()
            .map(PhpString::as_bytes)
            .filter(|u| !modern || !u.is_empty());
        let attr = elem.has_ns_prop(name, uri);
        if let Some(ns) = decl_named(elem, Some(name)).filter(|_| !modern) {
            if uri == ns.href().as_deref() {
                ns.clear();
                let first = elem.ns_def();
                if first == Some(ns) {
                    elem.set_ns_def(ns.next());
                } else {
                    let mut at = first;
                    while let Some(cur) = at {
                        if cur.next() == Some(ns) {
                            cur.set_next(ns.next());
                            break;
                        }
                        at = cur.next();
                    }
                }
                ns.set_next(None);
                ns.move_to_old(elem.doc());
                let mut at = Some(elem);
                while let Some(n) = at {
                    if n.kind() == Kind::Element {
                        if n.ns() == Some(ns) {
                            n.set_ns(None);
                        }
                        let mut a = n.properties();
                        while let Some(x) = a {
                            if x.ns() == Some(ns) {
                                x.set_ns(None);
                            }
                            a = x.next();
                        }
                    }
                    at = if n == elem {
                        n.children()
                    } else {
                        crate::next_in_tree_order(n, elem)
                    };
                }
            } else {
                return Ok(());
            }
        }
        if let Some(a) = attr.filter(|a| a.kind() != Kind::AttributeDecl) {
            remove_found(elem, Found::Attr(a));
        }
        Ok(())
    }

    /// `DOMElement::removeAttributeNode(DOMAttr $attr): DOMAttr|false`.
    fn removeAttributeNode(
        req: &mut Request,
        this: This<Self>,
        attr: Instance<crate::classes::DOMAttr>,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        remove_attribute_node(req, this.object(), attr.object()).map(Untyped)
    }

    /// `DOMElement::setAttribute(string $qualifiedName, string $value)`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r a="1"/>');
    /// $r = $d->documentElement;
    /// $a = $r->setAttribute('a', '2');
    /// var_dump(get_class($a), $r->setAttribute('xmlns', 'urn:d'));
    /// $r->setAttribute('b', 'x&y');
    /// echo $d->saveXML($r), "\n";
    /// try { $r->setAttribute('', 'v'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(7) "DOMAttr"
    /// bool(true)
    /// <r xmlns="urn:d" a="2" b="x&amp;y"/>
    /// DOMElement::setAttribute(): Argument #1 ($qualifiedName) must not be empty
    /// ```
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p" p:a="1"/>');
    /// $r = $d->documentElement;
    /// var_dump($r->setAttribute('p:a', 'x&y'), $r->setAttribute('B', '2'));
    /// var_dump($r->getAttributeNames(), $r->getAttribute('p:a'), $r->getAttributeNode('B')->namespaceURI);
    /// $h = Dom\HTMLDocument::createEmpty();
    /// $e = $h->createElement('p');
    /// $e->setAttribute('ID', 'k');
    /// $h->append($e);
    /// var_dump($e->getAttributeNames(), $h->getElementById('k') === $e);
    /// try { $r->setAttribute('1', 'v'); } catch (DOMException $x) { echo $x->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// NULL
    /// NULL
    /// array(3) {
    ///   [0]=>
    ///   string(7) "xmlns:p"
    ///   [1]=>
    ///   string(3) "p:a"
    ///   [2]=>
    ///   string(1) "B"
    /// }
    /// string(3) "x&y"
    /// NULL
    /// array(1) {
    ///   [0]=>
    ///   string(2) "id"
    /// }
    /// bool(true)
    /// Invalid Character Error
    /// ```
    fn setAttribute(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: PhpString,
        value: PhpString,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        set_attribute(
            req,
            this.object(),
            qualifiedName.as_bytes(),
            value.as_bytes(),
        )
        .map(Untyped)
    }

    /// `DOMElement::setAttributeNS(?string $namespace, string $qualifiedName, string $value): void`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r/>');
    /// $r = $d->documentElement;
    /// $r->setAttributeNS('urn:p', 'p:a', '1');
    /// $r->setAttributeNS('http://www.w3.org/2000/xmlns/', 'xmlns:q', 'urn:q');
    /// $r->setAttributeNS(null, 'b', '2');
    /// echo $d->saveXML($r), "\n";
    /// try { $r->setAttributeNS(null, 'p:c', '3'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// <r xmlns:p="urn:p" xmlns:q="urn:q" p:a="1" b="2"/>
    /// Namespace Error
    /// ```
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r/>');
    /// $r = $d->documentElement;
    /// $r->setAttributeNS('urn:p', 'p:a', '1');
    /// $r->setAttributeNS('urn:p', 'q:a', '2');
    /// $r->setAttributeNS('http://www.w3.org/2000/xmlns/', 'xmlns:q', 'urn:q');
    /// $r->setAttributeNS(null, 'b', '3');
    /// var_dump($r->getAttributeNames(), $r->getAttributeNS('urn:p', 'a'));
    /// $a = $r->getAttributeNodeNS('urn:p', 'a');
    /// var_dump($a->prefix, $r->getAttributeNodeNS('http://www.w3.org/2000/xmlns/', 'q')->value);
    /// try { $r->setAttributeNS(null, 'p:c', 'v'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// try { $r->setAttributeNS('urn:p', '1', 'v'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// try { $r->setAttributeNS('urn:p', 'xmlns', 'v'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// array(3) {
    ///   [0]=>
    ///   string(3) "q:a"
    ///   [1]=>
    ///   string(7) "xmlns:q"
    ///   [2]=>
    ///   string(1) "b"
    /// }
    /// string(1) "2"
    /// string(1) "q"
    /// string(5) "urn:q"
    /// Namespace Error
    /// Invalid Character Error
    /// Namespace Error
    /// ```
    #[php(tentative)]
    fn setAttributeNS(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<PhpString>,
        qualifiedName: PhpString,
        value: PhpString,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let elem = fetch(req, this)?;
        let (name, value) = (qualifiedName.as_bytes(), value.as_bytes());
        if follows_spec(req, this) {
            set_attribute_ns_modern(req, this, elem, namespace, name, value)
        } else {
            let uri = namespace.as_ref().map(PhpString::as_bytes);
            set_attribute_ns_legacy(req, this, elem, uri, name, value)
        }
    }

    /// `DOMElement::setAttributeNode(DOMAttr $attr)`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r a="1"/>');
    /// $r = $d->documentElement;
    /// $old = $r->setAttributeNode(new DOMAttr('a', '2'));
    /// var_dump($old->value, $r->getAttribute('a'), $r->setAttributeNode($r->getAttributeNode('a')));
    /// ```
    /// ```expect
    /// string(1) "1"
    /// string(1) "2"
    /// NULL
    /// ```
    fn setAttributeNode(
        req: &mut Request,
        this: This<Self>,
        attr: Instance<crate::classes::DOMAttr>,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        set_attribute_node(req, this.object(), attr.object(), (false, false)).map(Untyped)
    }

    /// `DOMElement::setAttributeNodeNS(DOMAttr $attr)`.
    fn setAttributeNodeNS(
        req: &mut Request,
        this: This<Self>,
        attr: Instance<crate::classes::DOMAttr>,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        set_attribute_node(req, this.object(), attr.object(), (true, false)).map(Untyped)
    }

    /// `DOMElement::setIdAttribute(string $qualifiedName, bool $isId): void`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r a="x"/>');
    /// $r = $d->documentElement;
    /// $r->setIdAttribute('a', true);
    /// var_dump($r->getAttributeNode('a')->isId(), $d->getElementById('x') === $r);
    /// try { $r->setIdAttribute('z', true); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// Not Found Error
    /// ```
    #[php(tentative)]
    fn setIdAttribute(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: PhpString,
        isId: bool,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let elem = fetch(req, this)?;
        if let Some(a) = elem
            .has_ns_prop(qualifiedName.as_bytes(), None)
            .filter(|a| a.kind() != Kind::AttributeDecl)
        {
            set_attribute_id(req, this, a, isId);
        } else {
            let strict = strict(req, this);
            dom_error(req, Code::NotFound, strict)?;
        }
        Ok(())
    }

    /// `DOMElement::setIdAttributeNS(string $namespace, string $qualifiedName, bool $isId): void`.
    #[php(tentative)]
    fn setIdAttributeNS(
        req: &mut Request,
        this: This<Self>,
        namespace: PhpString,
        qualifiedName: PhpString,
        isId: bool,
    ) -> Result<(), Thrown> {
        set_id_attribute_ns(
            req,
            this.object(),
            namespace.as_bytes(),
            qualifiedName.as_bytes(),
            isId,
        )
    }

    /// `DOMElement::setIdAttributeNode(DOMAttr $attr, bool $isId): void`.
    #[php(tentative)]
    fn setIdAttributeNode(
        req: &mut Request,
        this: This<Self>,
        attr: Instance<crate::classes::DOMAttr>,
        isId: bool,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let elem = fetch(req, this)?;
        let attr = fetch(req, attr.object())?;
        if attr.parent() == Some(elem) {
            set_attribute_id(req, this, attr, isId);
        } else {
            let strict = strict(req, this);
            dom_error(req, Code::NotFound, strict)?;
        }
        Ok(())
    }

    /// `DOMElement::toggleAttribute(string $qualifiedName, ?bool $force = null): bool`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r a=""/>');
    /// $r = $d->documentElement;
    /// var_dump($r->toggleAttribute('a'), $r->toggleAttribute('b'), $r->toggleAttribute('b', true), $r->toggleAttribute('c', false));
    /// echo $d->saveXML($r), "\n";
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// <r b=""/>
    /// ```
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r a=""/>');
    /// $r = $d->documentElement;
    /// var_dump($r->toggleAttribute('a'), $r->toggleAttribute('B'), $r->getAttribute('B'), $r->toggleAttribute('xmlns:p'), $r->getAttributeNames());
    /// $h = Dom\HTMLDocument::createEmpty()->createElement('p');
    /// var_dump($h->toggleAttribute('HIDDEN'), $h->getAttributeNames());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// NULL
    /// bool(true)
    /// array(2) {
    ///   [0]=>
    ///   string(1) "B"
    ///   [1]=>
    ///   string(7) "xmlns:p"
    /// }
    /// bool(true)
    /// array(1) {
    ///   [0]=>
    ///   string(6) "hidden"
    /// }
    /// ```
    fn toggleAttribute(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: PhpString,
        #[php(default = null)] force: Option<bool>,
    ) -> Result<bool, Thrown> {
        let mut name = qualifiedName.as_bytes().to_vec();
        let this = this.object();
        let elem = fetch(req, this)?;
        if !Name::valid(&name) {
            return Err(dom_throw(req, Code::InvalidCharacter));
        }
        let modern = follows_spec(req, this);
        if elem.doc().is_some_and(|d| d.kind() == Kind::HtmlDocument)
            && ((!modern && elem.ns().is_none()) || crate::private::ns_is(elem, HTML_NS))
        {
            name.make_ascii_lowercase();
        }
        match attribute_or_decl(modern, elem, &name) {
            None => {
                if force != Some(false) {
                    if modern {
                        elem.set_ns_prop_bare(None, &name);
                        return Ok(true);
                    }
                    match Name::split3(&name) {
                        Some(at) if name.starts_with(b"xmlns:") && at == 6 => {
                            elem.new_ns(b"", Some(&name[at..]));
                        }
                        _ => {
                            if name == b"xmlns" {
                                elem.new_ns(b"", None);
                            } else {
                                elem.set_prop(&name, b"");
                            }
                        }
                    }
                    return Ok(true);
                }
                Ok(false)
            }
            Some(found) => {
                if force != Some(true) {
                    remove_found(elem, found);
                    return Ok(false);
                }
                Ok(true)
            }
        }
    }

    /// `remove(): void`, `dom_child_node_remove`.
    fn remove(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        crate::parent::remove(req, this.object())
    }

    /// `before(DOMNode|string ...$nodes): void`.
    fn before(
        req: &mut Request,
        this: This<Self>,
        nodes: Variadic<Untyped<Value>>,
    ) -> Result<(), Thrown> {
        crate::parent::before(req, this.object(), &nodes)
    }

    /// `after(DOMNode|string ...$nodes): void`.
    fn after(
        req: &mut Request,
        this: This<Self>,
        nodes: Variadic<Untyped<Value>>,
    ) -> Result<(), Thrown> {
        crate::parent::after(req, this.object(), &nodes)
    }

    /// `replaceWith(DOMNode|string ...$nodes): void`.
    fn replaceWith(
        req: &mut Request,
        this: This<Self>,
        nodes: Variadic<Untyped<Value>>,
    ) -> Result<(), Thrown> {
        crate::parent::replace_with(req, this.object(), &nodes)
    }

    /// `append(DOMNode|string ...$nodes): void`.
    fn append(
        req: &mut Request,
        this: This<Self>,
        nodes: Variadic<Untyped<Value>>,
    ) -> Result<(), Thrown> {
        crate::parent::append(req, this.object(), &nodes)
    }

    /// `prepend(DOMNode|string ...$nodes): void`.
    fn prepend(
        req: &mut Request,
        this: This<Self>,
        nodes: Variadic<Untyped<Value>>,
    ) -> Result<(), Thrown> {
        crate::parent::prepend(req, this.object(), &nodes)
    }

    /// `replaceChildren(DOMNode|string ...$nodes): void`.
    fn replaceChildren(
        req: &mut Request,
        this: This<Self>,
        nodes: Variadic<Untyped<Value>>,
    ) -> Result<(), Thrown> {
        crate::parent::replace_children(req, this.object(), &nodes)
    }

    /// `DOMElement::insertAdjacentElement(string $where, DOMElement $element): ?DOMElement`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r><m/></r>');
    /// $m = $d->documentElement->firstChild;
    /// foreach (['beforebegin', 'afterbegin', 'beforeend', 'afterend'] as $w) $m->insertAdjacentElement($w, $d->createElement($w[0] . $w[6]));
    /// $m->insertAdjacentText('afterbegin', 't');
    /// $root = $d->documentElement;
    /// echo $d->saveXML($root), "\n";
    /// try { $m->insertAdjacentElement('nowhere', $d->createElement('x')); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// <r><bb/><m>t<ae/><be/></m><an/></r>
    /// Syntax Error
    /// ```
    fn insertAdjacentElement(
        req: &mut Request,
        this: This<Self>,
        r#where: PhpString,
        element: Instance<DOMElement>,
    ) -> Result<Owner<Raw<Option<Instance<DOMElement>>>>, Thrown> {
        let place = AdjacentPosition::of(r#where.as_bytes());
        let inserted = insert_adjacent_element(req, this.object(), place, element.object())?;
        returned(inserted)
    }

    /// `DOMElement::insertAdjacentText(string $where, string $data): void`.
    fn insertAdjacentText(
        req: &mut Request,
        this: This<Self>,
        r#where: PhpString,
        data: PhpString,
    ) -> Result<(), Thrown> {
        let place = AdjacentPosition::of(r#where.as_bytes());
        insert_adjacent_text(req, this.object(), place, data.as_bytes())
    }
}

/// `class Dom\Element extends Dom\Node implements Dom\ParentNode,
/// Dom\ChildNode`: the standard's element, whose clone drops the cached
/// `$classList` and `$children` ([`crate::object::ELEMENT_HANDLERS`]).
/// Most of its methods are `DOMElement`'s (`@implementation-alias`); its
/// own are the collections, the adjacent insertions over
/// [`AdjacentPosition`], the selectors, the namespace lists and `rename`.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p" p:a="x"><c xmlns:q="urn:q"/></r>');
/// $r = $d->documentElement;
/// $a = $r->getAttributeNodeNS('urn:p', 'a');
/// $r->setIdAttributeNode($a, true);
/// var_dump($d->getElementById('x') === $r, $r->removeAttributeNode($a) === $a);
/// $r->setAttributeNS('urn:p', 'p:i', 'k');
/// $r->setIdAttributeNS('urn:p', 'i', true);
/// var_dump($d->getElementById('k') === $r, $r->getElementsByTagNameNS('*', 'c')->length);
/// $r->firstChild->insertAdjacentText(Dom\AdjacentPosition::BeforeBegin, 't');
/// foreach ($r->getDescendantNamespaces() as $i) echo $i->prefix, ' ', $i->namespaceURI, ' ', $i->element->localName, "\n";
/// echo $d->saveXml($r), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// int(1)
/// p urn:p r
/// p urn:p c
/// q urn:q c
/// <r xmlns:p="urn:p" p:i="k">t<c xmlns:q="urn:q"/></r>
/// ```
///
/// An element's object is `dom_objects_new`'s, with the
/// `php_libxml_node_ptr` `php_libxml_increment_node_ptr` lays beside it
/// for its node, given back with it:
///
/// ```php
/// $d = Dom\HTMLDocument::createFromString('<p>a</p><div>b</div>', LIBXML_NOERROR);
/// $w = $d->body->firstChild;
/// $m = memory_get_usage(); $t = $d->body->lastChild; $n = memory_get_usage(); unset($t); $o = memory_get_usage();
/// var_dump($n - $m, $o - $n);
/// $x = new DOMDocument; $x->loadXML('<r><a/><b/></r>'); $w = $x->documentElement->firstChild;
/// $m = memory_get_usage(); $u = $x->documentElement->lastChild; var_dump(memory_get_usage() - $m);
/// ```
/// ```expect
/// int(120)
/// int(-120)
/// int(88)
/// ```
#[php_class(
    namespace(Dom),
    extends(crate::modern::Node),
    implements(crate::parent::ParentNode, crate::parent::ChildNode),
    handlers = crate::object::ELEMENT_HANDLERS
)]
pub(crate) struct Element;

#[php_class]
impl Element {
    #[php(property, virtual)]
    const namespaceURI: Option<PhpString>;
    #[php(property, virtual)]
    const prefix: Option<PhpString>;
    #[php(property, virtual)]
    const localName: PhpString;
    #[php(property, virtual)]
    const tagName: PhpString;
    #[php(property)]
    const children: Instance<crate::map::HTMLCollection>;
    #[php(property, virtual)]
    const firstElementChild: Option<Instance<Element>>;
    #[php(property, virtual)]
    const lastElementChild: Option<Instance<Element>>;
    #[php(property, virtual)]
    const childElementCount: i64;
    #[php(property, virtual)]
    const previousElementSibling: Option<Instance<Element>>;
    #[php(property, virtual)]
    const nextElementSibling: Option<Instance<Element>>;
    #[php(property, virtual)]
    const id: PhpString;
    #[php(property, virtual)]
    const className: PhpString;
    #[php(property)]
    const classList: Instance<crate::token_list::TokenList>;
    #[php(property, virtual)]
    const attributes: Instance<crate::map::NamedNodeMap>;
    #[php(property, virtual)]
    const innerHTML: PhpString;
    #[php(property, virtual)]
    const outerHTML: PhpString;
    #[php(property, virtual)]
    const substitutedNodeValue: PhpString;

    #[php(implementation = DOMNode::hasAttributes)]
    fn hasAttributes(this: This<Self>) -> bool;
    #[php(implementation = DOMElement::getAttributeNames)]
    fn getAttributeNames(this: This<Self>) -> PhpArray;
    #[php(implementation = DOMElement::getAttribute)]
    fn getAttribute(this: This<Self>, qualifiedName: PhpString) -> Option<PhpString>;
    #[php(implementation = DOMElement::getAttributeNS)]
    fn getAttributeNS(
        this: This<Self>,
        namespace: Option<PhpString>,
        localName: PhpString,
    ) -> Option<PhpString>;
    #[php(implementation = DOMElement::setAttribute)]
    fn setAttribute(this: This<Self>, qualifiedName: PhpString, value: PhpString);
    #[php(implementation = DOMElement::setAttributeNS)]
    fn setAttributeNS(
        this: This<Self>,
        namespace: Option<PhpString>,
        qualifiedName: PhpString,
        value: PhpString,
    );

    /// `Dom\Element::removeAttribute(string $qualifiedName): void`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p" a="1" p:b="2"><p:c/></r>');
    /// $r = $d->documentElement;
    /// var_dump($r->removeAttribute('a'), $r->removeAttribute('z'));
    /// $r->removeAttribute('xmlns:p');
    /// $r->removeAttribute('P:B');
    /// var_dump($r->getAttributeNames());
    /// $r->removeAttribute('p:b');
    /// $c = $r->firstChild;
    /// var_dump($r->getAttributeNames(), $c->namespaceURI);
    /// ```
    /// ```expect
    /// NULL
    /// NULL
    /// array(1) {
    ///   [0]=>
    ///   string(3) "p:b"
    /// }
    /// array(0) {
    /// }
    /// string(5) "urn:p"
    /// ```
    fn removeAttribute(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: PhpString,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let elem = fetch(req, this)?;
        let modern = follows_spec(req, this);
        if let Some(found) = attribute_or_decl(modern, elem, qualifiedName.as_bytes()) {
            remove_found(elem, found);
        }
        Ok(())
    }

    #[php(implementation = DOMElement::removeAttributeNS)]
    fn removeAttributeNS(this: This<Self>, namespace: Option<PhpString>, localName: PhpString);
    #[php(implementation = DOMElement::toggleAttribute)]
    fn toggleAttribute(
        this: This<Self>,
        qualifiedName: PhpString,
        #[php(default = null)] force: Option<bool>,
    ) -> bool;
    #[php(implementation = DOMElement::hasAttribute)]
    fn hasAttribute(this: This<Self>, qualifiedName: PhpString) -> bool;
    #[php(implementation = DOMElement::hasAttributeNS)]
    fn hasAttributeNS(this: This<Self>, namespace: Option<PhpString>, localName: PhpString)
    -> bool;
    #[php(implementation = DOMElement::getAttributeNode)]
    fn getAttributeNode(
        this: This<Self>,
        qualifiedName: PhpString,
    ) -> Option<Instance<crate::modern::Attr>>;
    #[php(implementation = DOMElement::getAttributeNodeNS)]
    fn getAttributeNodeNS(
        this: This<Self>,
        namespace: Option<PhpString>,
        localName: PhpString,
    ) -> Option<Instance<crate::modern::Attr>>;
    #[php(implementation = Element::setAttributeNodeNS)]
    fn setAttributeNode(
        this: This<Self>,
        attr: Instance<crate::modern::Attr>,
    ) -> Option<Instance<crate::modern::Attr>>;

    /// `Dom\Element::setAttributeNodeNS(Dom\Attr $attr): ?Dom\Attr`, which
    /// `setAttributeNode` aliases.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r a="1"/>');
    /// $r = $d->documentElement;
    /// $old = $r->setAttributeNode($d->createAttribute('a'));
    /// $a = $r->getAttributeNode('a');
    /// var_dump($old->value, $r->getAttribute('a'), $r->setAttributeNodeNS($a), $old->ownerElement);
    /// $o = Dom\XMLDocument::createEmpty();
    /// $b = $o->createAttributeNS('urn:x', 'x:b');
    /// var_dump($r->setAttributeNodeNS($b), $b->ownerDocument === $d, $b->ownerElement === $r, $b->namespaceURI);
    /// $c = $d->createElement('c');
    /// try { $c->setAttributeNode($b); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($r->getAttributeNames());
    /// ```
    /// ```expect
    /// string(1) "1"
    /// NULL
    /// NULL
    /// NULL
    /// NULL
    /// bool(true)
    /// bool(true)
    /// string(5) "urn:x"
    /// Inuse Attribute Error
    /// array(2) {
    ///   [0]=>
    ///   string(1) "a"
    ///   [1]=>
    ///   string(3) "x:b"
    /// }
    /// ```
    fn setAttributeNodeNS(
        req: &mut Request,
        this: This<Self>,
        attr: Instance<crate::modern::Attr>,
    ) -> Result<Owner<Raw<Option<Instance<crate::modern::Attr>>>>, Thrown> {
        let old = set_attribute_node(req, this.object(), attr.object(), (true, true))?;
        returned(old)
    }

    #[php(implementation = DOMElement::removeAttributeNode)]
    fn removeAttributeNode(
        this: This<Self>,
        attr: Instance<crate::modern::Attr>,
    ) -> Instance<crate::modern::Attr>;

    /// `Dom\Element::getElementsByTagName(string $qualifiedName): Dom\HTMLCollection`,
    /// which `Dom\Document` aliases: the name matched as a qualified name,
    /// lowercased against an HTML element of an HTML document.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p"><a/><p:a/><A/><b><a/></b></r>');
    /// $all = $d->getElementsByTagName('a');
    /// $r = $d->documentElement;
    /// var_dump(get_class($all), $all->length, $d->getElementsByTagName('p:a')->length, $r->getElementsByTagName('*')->length);
    /// var_dump($d->getElementsByTagNameNS('urn:p', 'a')->length, $d->getElementsByTagNameNS(null, 'a')->length, $d->getElementsByTagNameNS('*', 'a')->length);
    /// $h = Dom\HTMLDocument::createEmpty();
    /// $h->append($h->createElement('DIV'));
    /// var_dump($h->getElementsByTagName('Div')->length);
    /// ```
    /// ```expect
    /// string(18) "Dom\HTMLCollection"
    /// int(2)
    /// int(1)
    /// int(5)
    /// int(1)
    /// int(2)
    /// int(3)
    /// int(1)
    /// ```
    fn getElementsByTagName(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: Path,
    ) -> Result<Owner<Raw<Instance<crate::map::HTMLCollection>>>, Thrown> {
        let this = intern(req, this.object())?;
        let list = crate::create(
            req,
            this,
            "Dom\\HTMLCollection",
            MapKind::ByTagName {
                local: qualifiedName.0.as_bytes().to_vec(),
                ns: None,
                qname: true,
            },
        )?;
        returned(list)
    }

    /// `Dom\Element::getElementsByTagNameNS(?string $namespace, string $localName): Dom\HTMLCollection`.
    fn getElementsByTagNameNS(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<Path>,
        localName: Path,
    ) -> Result<Owner<Raw<Instance<crate::map::HTMLCollection>>>, Thrown> {
        let list = crate::document::elements_by_tag_name_ns(
            req,
            this.object(),
            namespace,
            localName,
            "Dom\\HTMLCollection",
        )?;
        returned(list)
    }

    /// `Dom\Element::getElementsByClassName(string $classNames): Dom\HTMLCollection`:
    /// the names an ordered set, lowercased for a quirks document; no name lists
    /// nothing.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r><a class="x y"/><b class=" y  x z"/><c class="X"/></r>');
    /// $r = $d->documentElement;
    /// foreach (['x', 'y x', 'X', '', '  '] as $n) {
    ///     $l = $r->getElementsByClassName($n);
    ///     echo get_class($l), ' ', $l->length, "\n";
    /// }
    /// echo $d->getElementsByClassName('z')->item(0)->tagName, "\n";
    /// ```
    /// ```expect
    /// Dom\HTMLCollection 2
    /// Dom\HTMLCollection 2
    /// Dom\HTMLCollection 1
    /// Dom\HTMLCollection 0
    /// Dom\HTMLCollection 0
    /// b
    /// ```
    fn getElementsByClassName(
        req: &mut Request,
        this: This<Self>,
        classNames: Path,
    ) -> Result<Owner<Raw<Instance<crate::map::HTMLCollection>>>, Thrown> {
        let this = intern(req, this.object())?;
        let quirks = dom(req, this).is_some_and(|d| d.obj.quirks() == Quirks::Quirks);
        let tokens = crate::token_list::ordered_set(classNames.0.as_bytes(), quirks);
        let kind = if tokens.is_empty() {
            MapKind::Noop
        } else {
            MapKind::ByClassName(tokens)
        };
        let list = crate::create(req, this, "Dom\\HTMLCollection", kind)?;
        returned(list)
    }

    /// `Dom\Element::insertAdjacentElement(Dom\AdjacentPosition $where, Dom\Element $element): ?Dom\Element`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r><m/></r>');
    /// $r = $d->documentElement;
    /// $m = $r->firstChild;
    /// foreach (Dom\AdjacentPosition::cases() as $w) {
    ///     $name = $w->name;
    ///     $n = $m->insertAdjacentElement($w, $d->createElement($name));
    /// }
    /// var_dump($n->tagName);
    /// $m->insertAdjacentText(Dom\AdjacentPosition::AfterBegin, 't');
    /// $names = [];
    /// foreach ($r->childNodes as $c) $names[] = $c->nodeName;
    /// foreach ($m->childNodes as $c) $names[] = $c->nodeName;
    /// echo implode(',', $names), "\n";
    /// $lone = $d->createElement('lone');
    /// var_dump($lone->insertAdjacentElement(Dom\AdjacentPosition::BeforeBegin, $d->createElement('x')));
    /// $lone->insertAdjacentText(Dom\AdjacentPosition::AfterEnd, 'x');
    /// ```
    /// ```expect
    /// string(8) "AfterEnd"
    /// BeforeBegin,m,AfterEnd,#text,AfterBegin,BeforeEnd
    /// NULL
    /// ```
    fn insertAdjacentElement(
        req: &mut Request,
        this: This<Self>,
        r#where: AdjacentPosition,
        element: Instance<Element>,
    ) -> Result<Owner<Raw<Option<Instance<Element>>>>, Thrown> {
        let inserted =
            insert_adjacent_element(req, this.object(), Some(r#where), element.object())?;
        returned(inserted)
    }

    /// `Dom\Element::insertAdjacentText(Dom\AdjacentPosition $where, string $data): void`.
    fn insertAdjacentText(
        req: &mut Request,
        this: This<Self>,
        r#where: AdjacentPosition,
        data: PhpString,
    ) -> Result<(), Thrown> {
        insert_adjacent_text(req, this.object(), Some(r#where), data.as_bytes())
    }

    /// `Dom\Element::insertAdjacentHTML(Dom\AdjacentPosition $where, string $string): void`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p"><m/></r>');
    /// $m = $d->documentElement->firstChild;
    /// $m->insertAdjacentHTML(Dom\AdjacentPosition::BeforeBegin, '<a/>t');
    /// $m->insertAdjacentHTML(Dom\AdjacentPosition::AfterEnd, '<p:z/>');
    /// $m->insertAdjacentHTML(Dom\AdjacentPosition::AfterBegin, '<i/>');
    /// $m->insertAdjacentHTML(Dom\AdjacentPosition::BeforeEnd, '<j/>');
    /// echo $d->saveXml(), "\n";
    /// try { $m->insertAdjacentHTML(Dom\AdjacentPosition::BeforeEnd, '<x>'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// $root = $d->documentElement;
    /// try { $root->insertAdjacentHTML(Dom\AdjacentPosition::AfterEnd, '<x/>'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// <?xml version="1.0" encoding="UTF-8"?>
    /// <r xmlns:p="urn:p"><a/>t<m><i/><j/></m><p:z/></r>
    /// XML fragment is not well-formed
    /// No Modification Allowed Error
    /// ```
    fn insertAdjacentHTML(
        req: &mut Request,
        this: This<Self>,
        r#where: AdjacentPosition,
        string: PhpString,
    ) -> Result<(), Thrown> {
        crate::inner_html::insert_adjacent_html(req, this.object(), r#where, string.as_bytes())
    }

    #[php(implementation = DOMElement::setIdAttribute)]
    fn setIdAttribute(this: This<Self>, qualifiedName: PhpString, isId: bool);

    /// `DOMElement::setIdAttributeNS(string $namespace, string $qualifiedName, bool $isId): void`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p" p:a="x" b="y"/>');
    /// $r = $d->documentElement;
    /// $r->setIdAttributeNS('urn:p', 'a', true);
    /// var_dump($r->getAttributeNodeNS('urn:p', 'a')->isId(), $d->getElementById('x') === $r);
    /// try { $r->setIdAttributeNS(null, 'b', true); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    ///
    /// Deprecated: Dom\Element::setIdAttributeNS(): Passing null to parameter #1 ($namespace) of type ?string is deprecated in %s on line 6
    /// Not Found Error
    /// ```
    fn setIdAttributeNS(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<PhpString>,
        qualifiedName: PhpString,
        isId: bool,
    ) -> Result<(), Thrown> {
        // php parses the namespace `s` where `Dom\Element` declares it
        // `?string`, so `null` is the deprecation `zend_parse_arg_str_weak`
        // raises.
        if namespace.is_none() {
            arg_null_deprecated(req, 1, "?string");
        }
        let uri = namespace.as_ref().map_or(&[][..], PhpString::as_bytes);
        set_id_attribute_ns(req, this.object(), uri, qualifiedName.as_bytes(), isId)
    }

    #[php(implementation = DOMElement::setIdAttributeNode)]
    fn setIdAttributeNode(this: This<Self>, attr: Instance<crate::modern::Attr>, isId: bool);
    #[php(implementation = DOMElement::remove)]
    fn remove(this: This<Self>);
    #[php(implementation = DOMElement::before)]
    fn before(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    #[php(implementation = DOMElement::after)]
    fn after(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    #[php(implementation = DOMElement::replaceWith)]
    fn replaceWith(
        this: This<Self>,
        nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>,
    );
    #[php(implementation = DOMElement::append)]
    fn append(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    #[php(implementation = DOMElement::prepend)]
    fn prepend(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    #[php(implementation = DOMElement::replaceChildren)]
    fn replaceChildren(
        this: This<Self>,
        nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>,
    );

    /// `Dom\ParentNode::querySelector(string $selectors): ?Dom\Element`,
    /// `dom_parent_node_query_selector`: the first element below the node in
    /// tree order that the list matches.
    ///
    /// ```php
    /// $d = Dom\HTMLDocument::createFromString('<div id="a"><p class="x">1</p><P CLASS="y">2</P></div>', LIBXML_NOERROR);
    /// echo $d->querySelector('p.y')->textContent, ' ', $d->querySelector('DIV > P:last-child')->textContent, "\n";
    /// var_dump($d->querySelector('span'));
    /// echo $d->getElementById('a')->querySelector('#a p, .x')->textContent, "\n";
    /// $f = $d->createDocumentFragment();
    /// $f->append($d->createElement('b'));
    /// echo $f->querySelector(':root')->tagName, "\n";
    /// try { $d->querySelector('p >'); } catch (DOMException $e) { echo $e->getCode(), ' ', $e->getMessage(), "\n"; }
    /// try { $d->querySelector('p || p'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// 2 2
    /// NULL
    /// 1
    /// B
    /// 12 Invalid selector (Selectors. Unexpected token: END-OF-FILE)
    /// Dom\Document::querySelector(): Argument #1 ($selectors) contains an unsupported selector
    /// ```
    fn querySelector(
        req: &mut Request,
        this: This<Self>,
        selectors: PhpString,
    ) -> Result<Owner<Raw<Option<Instance<Element>>>>, Thrown> {
        let found = crate::css::query_selector(req, this.object(), selectors.as_bytes())?;
        returned(found)
    }

    /// `Dom\ParentNode::querySelectorAll(string $selectors): Dom\NodeList`,
    /// `dom_parent_node_query_selector_all`: every element below the node in
    /// tree order, once for each complex selector of the list it matches.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r><a class="x y"/><b id="i"><a/><c lang="en-GB"/></b><a class="Y"/></r>');
    /// foreach (['a', 'a, a.x', 'b > *', 'a ~ a', ':is(b) a', '[lang|=en]', '[class~=y i]', 'r :nth-child(2 of a)', ':not(a)', 'b:has(> c)', ':empty'] as $s) {
    ///     $l = $d->querySelectorAll($s);
    ///     echo get_class($l), ' ', $s, ':';
    ///     foreach ($l as $n) echo ' ', $n->tagName;
    ///     echo "\n";
    /// }
    /// ```
    /// ```expect
    /// Dom\NodeList a: a a a
    /// Dom\NodeList a, a.x: a a a a
    /// Dom\NodeList b > *: a c
    /// Dom\NodeList a ~ a: a
    /// Dom\NodeList :is(b) a: a
    /// Dom\NodeList [lang|=en]: c
    /// Dom\NodeList [class~=y i]: a a
    /// Dom\NodeList r :nth-child(2 of a): a
    /// Dom\NodeList :not(a): r b c
    /// Dom\NodeList b:has(> c): b
    /// Dom\NodeList :empty: a a c a
    /// ```
    fn querySelectorAll(
        req: &mut Request,
        this: This<Self>,
        selectors: PhpString,
    ) -> Result<Owner<Raw<Instance<crate::map::NodeList>>>, Thrown> {
        let list = crate::css::query_selector_all(req, this.object(), selectors.as_bytes())?;
        returned(list)
    }

    /// `Dom\Element::closest(string $selectors): ?Dom\Element`,
    /// `dom_element_closest`: the element itself or its nearest ancestor that
    /// the list matches.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r><s class="a"><t><u/></t></s></r>');
    /// $u = $d->getElementsByTagName('u')->item(0);
    /// echo $u->closest('u')->tagName, ' ', $u->closest('.a')->tagName, ' ', $u->closest('r > *')->tagName, "\n";
    /// var_dump($u->closest('v'));
    /// try { $u->closest('<'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// u s s
    /// NULL
    /// Invalid selector (Selectors. Unexpected token: <)
    /// ```
    fn closest(
        req: &mut Request,
        this: This<Self>,
        selectors: PhpString,
    ) -> Result<Owner<Raw<Option<Instance<Element>>>>, Thrown> {
        let found = crate::css::closest(req, this.object(), selectors.as_bytes())?;
        returned(found)
    }

    /// `Dom\Element::matches(string $selectors): bool`, `dom_element_matches`.
    ///
    /// ```php
    /// $d = Dom\HTMLDocument::createFromString('<!DOCTYPE html><form><input type=CHECKBOX checked><input disabled></form>', LIBXML_NOERROR);
    /// $c = $d->querySelector('input');
    /// $x = $d->querySelector('input + input');
    /// var_dump($c->matches(':checked'), $c->matches('form > input:first-of-type'), $x->matches(':enabled'), $x->matches(':disabled'), $x->matches('[DISABLED]'));
    /// try { $c->matches(':blank'); } catch (DOMException $e) { echo $e->getCode(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// 9
    /// ```
    fn matches(req: &mut Request, this: This<Self>, selectors: PhpString) -> Result<bool, Thrown> {
        crate::css::matches(req, this.object(), selectors.as_bytes())
    }

    /// `Dom\Element::getInScopeNamespaces(): array`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns="urn:d" xmlns:a="urn:a"><c xmlns:b="urn:b" xmlns:a="urn:a2"><e xmlns=""/></c></r>');
    /// $r = $d->documentElement;
    /// $e = $r->firstChild->firstChild;
    /// foreach ($e->getInScopeNamespaces() as $i) {
    ///     var_dump($i->prefix, $i->namespaceURI, $i->element === $e);
    /// }
    /// foreach ($r->getDescendantNamespaces() as $i) {
    ///     echo $i->prefix ?? '(none)', ' ', $i->namespaceURI, ' ', $i->element->localName, "\n";
    /// }
    /// try { $i->prefix = 'x'; } catch (Error $x) { echo $x->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "b"
    /// string(5) "urn:b"
    /// bool(true)
    /// string(1) "a"
    /// string(6) "urn:a2"
    /// bool(true)
    /// (none) urn:d r
    /// a urn:a r
    /// (none) urn:d c
    /// b urn:b c
    /// a urn:a2 c
    /// b urn:b e
    /// a urn:a2 e
    /// Cannot modify readonly property Dom\NamespaceInfo::$prefix
    /// ```
    fn getInScopeNamespaces(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let this = this.object();
        let node = fetch(req, this)?;
        let mut out = PhpArray::new(req)?;
        if let Err(thrown) = in_scope_namespace_info(req, this, node, &mut out) {
            out.give_up(req);
            return Err(thrown);
        }
        Ok(out)
    }

    /// `Dom\Element::getDescendantNamespaces(): array`: the element's, then
    /// every descendant element's in tree order.
    fn getDescendantNamespaces(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let this = this.object();
        let node = fetch(req, this)?;
        let walk = |req: &mut Request, out: &mut Owner<PhpArray>| -> Result<(), Thrown> {
            in_scope_namespace_info(req, this, node, out)?;
            let mut at = node.children();
            while let Some(cur) = at {
                if cur.kind() == Kind::Element {
                    in_scope_namespace_info(req, this, cur, out)?;
                }
                at = crate::next_in_tree_order(cur, node);
            }
            Ok(())
        };
        let mut out = PhpArray::new(req)?;
        if let Err(thrown) = walk(req, &mut out) {
            out.give_up(req);
            return Err(thrown);
        }
        Ok(out)
    }

    /// `Dom\Element::rename(?string $namespaceURI, string $qualifiedName): void`,
    /// which `Dom\Attr` aliases: the name validated and split, a clash with
    /// a sibling attribute or a move across the HTML namespace refused, then
    /// the namespace the mapper's and the local name replaced.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p" a="1" p:b="2"><c/></r>');
    /// $r = $d->documentElement;
    /// $c = $r->firstChild;
    /// $c->rename('urn:q', 'q:d');
    /// $a = $r->getAttributeNode('a');
    /// $a->rename('urn:p', 'x:a');
    /// var_dump($c->namespaceURI, $c->prefix, $c->localName, $c->tagName, $a->prefix, $a->localName, $a->namespaceURI, $r->getAttributeNames());
    /// try { $a->rename('urn:p', 'b'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// try { $r->rename('http://www.w3.org/1999/xhtml', 'r'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// try { $r->rename(null, 'p:r'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// $h = Dom\HTMLDocument::createEmpty()->createElement('template');
    /// try { $h->rename('http://www.w3.org/1999/xhtml', 'div'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// try { $h->rename(null, 'div'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(5) "urn:q"
    /// string(1) "q"
    /// string(1) "d"
    /// string(3) "q:d"
    /// string(1) "x"
    /// string(1) "a"
    /// string(5) "urn:p"
    /// array(3) {
    ///   [0]=>
    ///   string(7) "xmlns:p"
    ///   [1]=>
    ///   string(3) "x:a"
    ///   [2]=>
    ///   string(3) "p:b"
    /// }
    /// An attribute with the given name in the given namespace already exists
    /// It is not possible to move an element into the HTML namespace because the HTML namespace is tied to the HTMLElement class
    /// Namespace Error
    /// It is not possible to rename the template element because it hosts a document fragment
    /// It is not possible to move an element out of the HTML namespace because the HTML namespace is tied to the HTMLElement class
    /// ```
    fn rename(
        req: &mut Request,
        this: This<Self>,
        namespaceURI: Option<PhpString>,
        qualifiedName: PhpString,
    ) -> Result<(), Thrown> {
        let uri = namespaceURI.as_ref().map(PhpString::as_bytes);
        let this = this.object();
        let node = fetch(req, this)?;
        let (local, prefix) = match validate_and_extract(uri, qualifiedName.as_bytes()) {
            Ok(parts) => parts,
            Err(code) => return Err(dom_throw(req, code)),
        };
        if node.kind() == Kind::Attribute {
            let existing = node
                .parent()
                .and_then(|p| p.has_ns_prop(&local, uri.filter(|u| !u.is_empty())));
            if existing.is_some_and(|e| e != node) {
                return Err(crate::dom_throw_message(
                    req,
                    Code::InvalidModification,
                    "An attribute with the given name in the given namespace already exists",
                ));
            }
        } else {
            let is_html = crate::private::ns_is(node, HTML_NS);
            let will_be_html = uri == Some(HTML_NS);
            if is_html != will_be_html {
                let message = if is_html {
                    "It is not possible to move an element out of the HTML namespace because the HTML namespace is tied to the HTMLElement class"
                } else {
                    "It is not possible to move an element into the HTML namespace because the HTML namespace is tied to the HTMLElement class"
                };
                return Err(crate::dom_throw_message(
                    req,
                    Code::InvalidModification,
                    message,
                ));
            }
            if is_html && node.name() == b"template" && local != b"template" {
                return Err(crate::dom_throw_message(
                    req,
                    Code::InvalidModification,
                    "It is not possible to rename the template element because it hosts a document fragment",
                ));
            }
        }
        invalidate(req, this);
        let ns = mapped_ns(req, this, node.doc(), prefix.as_deref(), namespaceURI)?;
        node.set_ns(ns);
        node.set_name(&local);
        Ok(())
    }
}

/// `class Dom\HTMLElement extends Dom\Element`, an element of the HTML
/// namespace.
#[php_class(namespace(Dom), extends(Element))]
pub(crate) struct HTMLElement;

#[php_class]
impl HTMLElement {}
