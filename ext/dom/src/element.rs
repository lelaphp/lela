//! `DOMElement` (`element.c`): construction, the attribute methods, the
//! ID switches and the adjacent insertions.

use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpObject, Value};
use php_ext_libxml::ffi::{Kind, Name, Node, Ns};

use crate::document::XMLNS_NS;
use crate::object::{Code, Extra};
use crate::{
    Entry, arity, bool_arg, bytes_arg, check_qname, dom_error, dom_mut, dom_throw, fetch, get_ns,
    mark_ids_modified, node_list_unlink, nullable_bool_arg, nullable_bytes_arg, nullable_object,
    object_arg, object_for, resolve_prefix_conflict, set_attribute_id, strict, string, this,
    this_node,
};

/// What a qualified name finds on an element for the legacy classes: an
/// attribute, or a namespace declaration `xmlns[:p]` names.
#[derive(Clone, Copy)]
enum Found {
    Attr(Node),
    Decl(Ns),
}

/// `dom_get_attribute_or_nsdecl` for the legacy classes.
fn attribute_or_decl(elem: Node, name: &[u8]) -> Option<Found> {
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
fn construct(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 3)?;
    let name = bytes_arg(req, 1)?;
    let value = nullable_bytes_arg(req, 2)?;
    let uri = bytes_arg(req, 3)?;
    let this = this(req)?;
    if !Name::valid(&name) {
        return Err(dom_throw(req, Code::InvalidCharacter));
    }
    let node = if uri.is_empty() {
        if Name::split2(&name).is_some() {
            return Err(dom_throw(req, Code::Namespace));
        }
        Node::new_element(None, None, &name, None)
    } else {
        let made = check_qname(&name, uri.len()).and_then(|(local, prefix)| {
            let node = Node::new_element(None, None, &local, None).ok_or(Code::InvalidState)?;
            match get_ns(node, &uri, prefix.as_deref()) {
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
        node.set_content_len(&v);
    }
    crate::rebind(req, this, node);
    Ok(Value::null())
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
fn get_attribute(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = bytes_arg(req, 1)?;
    let (_, elem) = this_node(req)?;
    let value = match attribute_or_decl(elem, &name) {
        Some(Found::Attr(a)) if a.kind() == Kind::Attribute => a.list_string(),
        Some(Found::Decl(ns)) => ns.href(),
        _ => None,
    };
    string(req, &value.unwrap_or_default())
}

/// `DOMElement::getAttributeNames(): array`.
fn get_attribute_names(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (_, elem) = this_node(req)?;
    let mut out = PhpArray::new(req)?;
    let mut ns = elem.ns_def();
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
    Ok(Value::array(out))
}

/// `dom_get_attribute_ns`: the value, a namespace declaration's href
/// where the namespace is the XMLNS one.
fn attribute_ns(elem: Node, uri: Option<&[u8]>, name: &[u8]) -> Option<Vec<u8>> {
    if let Some(v) = elem.get_ns_prop(name, uri) {
        return Some(v);
    }
    if uri == Some(XMLNS_NS) {
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
fn get_attribute_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let uri = nullable_bytes_arg(req, 1)?;
    let name = bytes_arg(req, 2)?;
    let (_, elem) = this_node(req)?;
    string(
        req,
        &attribute_ns(elem, uri.as_deref(), &name).unwrap_or_default(),
    )
}

/// `DOMElement::hasAttribute(string $qualifiedName): bool`.
fn has_attribute(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = bytes_arg(req, 1)?;
    let (_, elem) = this_node(req)?;
    Ok(Value::bool(attribute_or_decl(elem, &name).is_some()))
}

/// `DOMElement::hasAttributeNS(?string $namespace, string $localName): bool`.
fn has_attribute_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let uri = nullable_bytes_arg(req, 1)?;
    let name = bytes_arg(req, 2)?;
    let (_, elem) = this_node(req)?;
    Ok(Value::bool(
        attribute_ns(elem, uri.as_deref(), &name).is_some(),
    ))
}

/// A namespace declaration handed out as a `DOMNameSpaceNode`, the
/// element kept alive by it.
fn namespace_node(req: &mut Request, this: PhpObject, elem: Node, ns: Ns) -> Result<Value, Thrown> {
    let Some(fake) = Node::fake_namespace_decl(elem, ns.href().as_deref(), ns.prefix().as_deref())
    else {
        return Err(dom_throw(req, Code::InvalidState));
    };
    let value = object_for(req, fake, Some(this))?;
    if let Some(object) = value.as_object().copied() {
        req.addref(&this.as_value());
        if let Some(d) = dom_mut(req, object) {
            d.extra = Extra::NamespaceParent(this);
        }
    }
    Ok(value)
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
fn get_attribute_node(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = bytes_arg(req, 1)?;
    let (this, elem) = this_node(req)?;
    match attribute_or_decl(elem, &name) {
        None => Ok(Value::bool(false)),
        Some(Found::Decl(ns)) => namespace_node(req, this, elem, ns),
        Some(Found::Attr(a)) => object_for(req, a, Some(this)),
    }
}

/// `DOMElement::getAttributeNodeNS(?string $namespace, string $localName)`.
fn get_attribute_node_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let uri = nullable_bytes_arg(req, 1)?;
    let name = bytes_arg(req, 2)?;
    let (this, elem) = this_node(req)?;
    match elem.has_ns_prop(&name, uri.as_deref()) {
        Some(a) => object_for(req, a, Some(this)),
        None if uri.as_deref() == Some(XMLNS_NS) => match decl_named(elem, Some(&name)) {
            Some(ns) => namespace_node(req, this, elem, ns),
            None => Ok(Value::null()),
        },
        None => Ok(Value::null()),
    }
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
fn set_attribute(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let name = bytes_arg(req, 1)?;
    let value = bytes_arg(req, 2)?;
    if name.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if !Name::valid(&name) {
        return Err(dom_throw(req, Code::InvalidCharacter));
    }
    let (this, elem) = this_node(req)?;
    match attribute_or_decl(elem, &name) {
        Some(Found::Attr(a)) if a.kind() == Kind::Attribute => {
            crate::attr_value_will_change(req, this, a);
            node_list_unlink(a.children());
        }
        Some(Found::Decl(_)) => return Ok(Value::bool(false)),
        _ => {}
    }
    if name == b"xmlns" {
        return Ok(match elem.new_ns(&value, None) {
            Some(_) => Value::bool(true),
            None => return Err(arg_value_error(req, 1, "be a valid XML attribute")),
        });
    }
    match elem.set_prop(&name, &value) {
        Some(a) => object_for(req, a, Some(this)),
        None => Err(arg_value_error(req, 1, "be a valid XML attribute")),
    }
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
fn remove_attribute(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = bytes_arg(req, 1)?;
    let (_, elem) = this_node(req)?;
    match attribute_or_decl(elem, &name) {
        None => Ok(Value::bool(false)),
        Some(found) => {
            remove_found(elem, found);
            Ok(Value::bool(true))
        }
    }
}

/// `DOMElement::removeAttributeNS(?string $namespace, string $localName): void`.
fn remove_attribute_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let uri = nullable_bytes_arg(req, 1)?;
    let name = bytes_arg(req, 2)?;
    let (_, elem) = this_node(req)?;
    let attr = elem.has_ns_prop(&name, uri.as_deref());
    if let Some(ns) = decl_named(elem, Some(&name)) {
        if uri.as_deref() == ns.href().as_deref() {
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
            return Ok(Value::null());
        }
    }
    if let Some(a) = attr.filter(|a| a.kind() != Kind::AttributeDecl) {
        remove_found(elem, Found::Attr(a));
    }
    Ok(Value::null())
}

/// `DOMElement::removeAttributeNode(DOMAttr $attr): DOMAttr|false`.
fn remove_attribute_node(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let (this, elem) = this_node(req)?;
    let Some(attr_obj) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let attr = fetch(req, attr_obj)?;
    if attr.parent() != Some(elem) {
        let strict = strict(req, this);
        dom_error(req, Code::NotFound, strict)?;
        return Ok(Value::bool(false));
    }
    attr.unlink();
    object_for(req, attr, Some(this))
}

/// `dom_element_set_attribute_node_common` for the legacy classes.
fn set_attribute_node_as(req: &mut Request, use_ns: bool) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let (this, elem) = this_node(req)?;
    let Some(attr_obj) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let attr = fetch(req, attr_obj)?;
    if attr.doc().is_some() && attr.doc() != elem.doc() {
        let strict = strict(req, this);
        dom_error(req, Code::WrongDocument, strict)?;
        return Ok(Value::bool(false));
    }
    let existing = match attr.ns().and_then(Ns::href).filter(|_| use_ns) {
        Some(href) => elem.has_ns_prop(&attr.name(), Some(&href)),
        None => elem.has_prop(&attr.name()),
    }
    .filter(|a| a.kind() != Kind::AttributeDecl);
    if let Some(old) = existing {
        if old == attr {
            return Ok(Value::null());
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
        crate::set_document_ref_pointers_attr(attr, shared);
    }
    elem.add_child(attr);
    mark_ids_modified(req, this);
    crate::reconcile_attribute_namespace(attr);
    nullable_object(req, existing, Some(this))
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
fn set_attribute_node(req: &mut Request) -> Result<Value, Thrown> {
    set_attribute_node_as(req, false)
}

/// `DOMElement::setAttributeNodeNS(DOMAttr $attr)`.
fn set_attribute_node_ns(req: &mut Request) -> Result<Value, Thrown> {
    set_attribute_node_as(req, true)
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
fn set_attribute_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 3, 3)?;
    let uri = nullable_bytes_arg(req, 1)?;
    let name = bytes_arg(req, 2)?;
    let value = bytes_arg(req, 3)?;
    let (this, elem) = this_node(req)?;
    set_attribute_ns_legacy(req, this, elem, uri.as_deref(), &name, &value)?;
    Ok(Value::null())
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
fn set_id_attribute(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let name = bytes_arg(req, 1)?;
    let is_id = bool_arg(req, 2, false)?;
    let (this, elem) = this_node(req)?;
    if let Some(a) = elem
        .has_ns_prop(&name, None)
        .filter(|a| a.kind() != Kind::AttributeDecl)
    {
        set_attribute_id(req, this, a, is_id);
    } else {
        let strict = strict(req, this);
        dom_error(req, Code::NotFound, strict)?;
    }
    Ok(Value::null())
}

/// `DOMElement::setIdAttributeNS(string $namespace, string $qualifiedName, bool $isId): void`.
fn set_id_attribute_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 3, 3)?;
    let uri = bytes_arg(req, 1)?;
    let name = bytes_arg(req, 2)?;
    let is_id = bool_arg(req, 3, false)?;
    let (this, elem) = this_node(req)?;
    if let Some(a) = elem
        .has_ns_prop(&name, Some(&uri))
        .filter(|a| a.kind() != Kind::AttributeDecl)
    {
        set_attribute_id(req, this, a, is_id);
    } else {
        let strict = strict(req, this);
        dom_error(req, Code::NotFound, strict)?;
    }
    Ok(Value::null())
}

/// `DOMElement::setIdAttributeNode(DOMAttr $attr, bool $isId): void`.
fn set_id_attribute_node(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let (this, elem) = this_node(req)?;
    let Some(attr_obj) = object_arg(req, 1)? else {
        return Ok(Value::null());
    };
    let is_id = bool_arg(req, 2, false)?;
    let attr = fetch(req, attr_obj)?;
    if attr.parent() == Some(elem) {
        set_attribute_id(req, this, attr, is_id);
    } else {
        let strict = strict(req, this);
        dom_error(req, Code::NotFound, strict)?;
    }
    Ok(Value::null())
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
fn toggle_attribute(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 2)?;
    let mut name = bytes_arg(req, 1)?;
    let force = nullable_bool_arg(req, 2)?;
    let (_, elem) = this_node(req)?;
    if !Name::valid(&name) {
        return Err(dom_throw(req, Code::InvalidCharacter));
    }
    if elem.doc().is_some_and(|d| d.kind() == Kind::HtmlDocument) && elem.ns().is_none() {
        name.make_ascii_lowercase();
    }
    match attribute_or_decl(elem, &name) {
        None => {
            if force != Some(false) {
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
                return Ok(Value::bool(true));
            }
            Ok(Value::bool(false))
        }
        Some(found) => {
            if force != Some(true) {
                remove_found(elem, found);
                return Ok(Value::bool(false));
            }
            Ok(Value::bool(true))
        }
    }
}

/// Where an adjacent insertion goes, `dom_insert_adjacent`'s four names.
enum Where {
    BeforeBegin,
    AfterBegin,
    BeforeEnd,
    AfterEnd,
}

impl Where {
    fn of(name: &[u8]) -> Option<Where> {
        Some(match name.to_ascii_lowercase().as_slice() {
            b"beforebegin" => Where::BeforeBegin,
            b"afterbegin" => Where::AfterBegin,
            b"beforeend" => Where::BeforeEnd,
            b"afterend" => Where::AfterEnd,
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

/// `dom_insert_adjacent`.
fn insert_adjacent(
    req: &mut Request,
    this: PhpObject,
    elem: Node,
    where_: &[u8],
    other: Node,
) -> Result<Inserted, Thrown> {
    let Some(place) = Where::of(where_) else {
        let strict = strict(req, this);
        dom_error(req, Code::Syntax, strict)?;
        return Ok(Inserted::Failed);
    };
    let (parent, before) = match place {
        Where::BeforeBegin => match elem.parent() {
            Some(p) => (p, Some(elem)),
            None => return Ok(Inserted::NoParent),
        },
        Where::AfterBegin => (elem, elem.children()),
        Where::BeforeEnd => (elem, None),
        Where::AfterEnd => match elem.parent() {
            Some(p) => (p, elem.next()),
            None => return Ok(Inserted::NoParent),
        },
    };
    let Some(doc) = elem.doc() else {
        return Ok(Inserted::Failed);
    };
    if !crate::adopt(req, this, other, doc) {
        return Ok(Inserted::Failed);
    }
    if !crate::parent::pre_insert(req, this, other, parent, before)? {
        return Ok(Inserted::Failed);
    }
    Ok(Inserted::Done)
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
fn insert_adjacent_element(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let where_ = bytes_arg(req, 1)?;
    let Some(other_obj) = object_arg(req, 2)? else {
        return Ok(Value::null());
    };
    let (this, elem) = this_node(req)?;
    let other = fetch(req, other_obj)?;
    match insert_adjacent(req, this, elem, &where_, other)? {
        Inserted::Done => object_for(req, other, Some(other_obj)),
        Inserted::NoParent | Inserted::Failed => Ok(Value::null()),
    }
}

/// `DOMElement::insertAdjacentText(string $where, string $data): void`.
fn insert_adjacent_text(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let where_ = bytes_arg(req, 1)?;
    let data = bytes_arg(req, 2)?;
    let (this, elem) = this_node(req)?;
    let Some(text) = Node::new_text_len(elem.doc(), &data) else {
        return Ok(Value::null());
    };
    match insert_adjacent(req, this, elem, &where_, text)? {
        Inserted::Done => {}
        Inserted::NoParent => text.free(),
        Inserted::Failed => {
            if text.parent().is_none() {
                text.free();
            }
        }
    }
    Ok(Value::null())
}

/// The methods, `Class::method` to entry.
pub(crate) const METHODS: &[(&str, Entry)] = &[
    ("DOMElement::__construct", construct),
    ("DOMElement::getAttribute", get_attribute),
    ("DOMElement::getAttributeNames", get_attribute_names),
    ("DOMElement::getAttributeNS", get_attribute_ns),
    ("DOMElement::getAttributeNode", get_attribute_node),
    ("DOMElement::getAttributeNodeNS", get_attribute_node_ns),
    (
        "DOMElement::getElementsByTagName",
        crate::document::GET_ELEMENTS_BY_TAG_NAME,
    ),
    (
        "DOMElement::getElementsByTagNameNS",
        crate::document::GET_ELEMENTS_BY_TAG_NAME_NS,
    ),
    ("DOMElement::hasAttribute", has_attribute),
    ("DOMElement::hasAttributeNS", has_attribute_ns),
    ("DOMElement::removeAttribute", remove_attribute),
    ("DOMElement::removeAttributeNS", remove_attribute_ns),
    ("DOMElement::removeAttributeNode", remove_attribute_node),
    ("DOMElement::setAttribute", set_attribute),
    ("DOMElement::setAttributeNS", set_attribute_ns),
    ("DOMElement::setAttributeNode", set_attribute_node),
    ("DOMElement::setAttributeNodeNS", set_attribute_node_ns),
    ("DOMElement::setIdAttribute", set_id_attribute),
    ("DOMElement::setIdAttributeNS", set_id_attribute_ns),
    ("DOMElement::setIdAttributeNode", set_id_attribute_node),
    ("DOMElement::toggleAttribute", toggle_attribute),
    ("DOMElement::remove", crate::parent::REMOVE),
    ("DOMElement::before", crate::parent::BEFORE),
    ("DOMElement::after", crate::parent::AFTER),
    ("DOMElement::replaceWith", crate::parent::REPLACE_WITH),
    ("DOMElement::append", crate::parent::APPEND),
    ("DOMElement::prepend", crate::parent::PREPEND),
    (
        "DOMElement::replaceChildren",
        crate::parent::REPLACE_CHILDREN,
    ),
    ("DOMElement::insertAdjacentElement", insert_adjacent_element),
    ("DOMElement::insertAdjacentText", insert_adjacent_text),
];
