//! `DOMElement` (`element.c`): construction, the attribute methods, the
//! ID switches and the adjacent insertions.

use php_ext::attr::php_class;
use php_ext::raising::{Thrown, arg_null_deprecated, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpObject, Value, ValueRef};
use php_ext_libxml::ffi::{Kind, Name, Node, Ns, Quirks, XML_ATTRIBUTE_ID};

use crate::document::XMLNS_NS;
use crate::map::MapKind;
use crate::modern::follows_spec;
use crate::object::{Code, Extra};
use crate::private::{HTML_NS, html_in_html_document};
use crate::{
    Entry, arity, bool_arg, bytes_arg, check_qname, dom, dom_error, dom_mut, dom_throw, fetch,
    get_ns, invalidate, mark_ids_modified, node_list_unlink, nullable_bool_arg, nullable_bytes_arg,
    nullable_object, object_arg, object_for, path_arg, resolve_prefix_conflict, set_attribute_id,
    strict, string, this, this_intern, this_node, validate_and_extract,
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
fn get_attribute(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = bytes_arg(req, 1)?;
    let (this, elem) = this_node(req)?;
    let modern = follows_spec(req, this);
    let value = match attribute_or_decl(modern, elem, &name) {
        Some(Found::Attr(a)) if a.kind() == Kind::Attribute => a.list_string(),
        Some(Found::Decl(ns)) => ns.href(),
        _ => None,
    };
    match value {
        Some(v) => string(req, &v),
        None if modern => Ok(Value::null()),
        None => string(req, b""),
    }
}

/// `DOMElement::getAttributeNames(): array`.
fn get_attribute_names(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (this, elem) = this_node(req)?;
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
    Ok(Value::array(out))
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
fn get_attribute_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let uri = nullable_bytes_arg(req, 1)?;
    let name = bytes_arg(req, 2)?;
    let (this, elem) = this_node(req)?;
    let modern = follows_spec(req, this);
    match attribute_ns(modern, elem, uri.as_deref(), &name) {
        Some(v) => string(req, &v),
        None if modern => Ok(Value::null()),
        None => string(req, b""),
    }
}

/// `DOMElement::hasAttribute(string $qualifiedName): bool`.
fn has_attribute(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = bytes_arg(req, 1)?;
    let (this, elem) = this_node(req)?;
    let modern = follows_spec(req, this);
    Ok(Value::bool(
        attribute_or_decl(modern, elem, &name).is_some(),
    ))
}

/// `DOMElement::hasAttributeNS(?string $namespace, string $localName): bool`.
fn has_attribute_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let uri = nullable_bytes_arg(req, 1)?;
    let name = bytes_arg(req, 2)?;
    let (this, elem) = this_node(req)?;
    let modern = follows_spec(req, this);
    Ok(Value::bool(
        attribute_ns(modern, elem, uri.as_deref(), &name).is_some(),
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
fn get_attribute_node(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = bytes_arg(req, 1)?;
    let (this, elem) = this_node(req)?;
    let modern = follows_spec(req, this);
    match attribute_or_decl(modern, elem, &name) {
        None if modern => Ok(Value::null()),
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
    let modern = follows_spec(req, this);
    let uri = uri.filter(|u| !modern || !u.is_empty());
    match elem.has_ns_prop(&name, uri.as_deref()) {
        Some(a) => object_for(req, a, Some(this)),
        None if !modern && uri.as_deref() == Some(XMLNS_NS) => {
            match decl_named(elem, Some(&name)) {
                Some(ns) => namespace_node(req, this, elem, ns),
                None => Ok(Value::null()),
            }
        }
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
    if follows_spec(req, this) {
        set_attribute_modern(req, this, elem, &name, &value);
        return Ok(Value::null());
    }
    match attribute_or_decl(false, elem, &name) {
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

/// The modern branch of `DOMElement::setAttribute`: the attribute the
/// qualified name finds takes the value as its one text child, or a new
/// one with no namespace is made, its name lowercased for an HTML element
/// of an HTML document.
fn set_attribute_modern(req: &mut Request, this: PhpObject, elem: Node, name: &[u8], value: &[u8]) {
    if let Some(attr) = crate::map::attribute_node(elem, name) {
        crate::attr_value_will_change(req, this, attr);
        crate::remove_all_children(attr);
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
    uri: Option<&[u8]>,
) -> Option<Ns> {
    let private = dom_mut(req, object).and_then(|d| crate::private::of(&mut d.obj))?;
    private.mapper.get_nullsafe(doc, prefix, uri)
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
    let (this, elem) = this_node(req)?;
    let modern = follows_spec(req, this);
    match attribute_or_decl(modern, elem, &name) {
        None => Ok(Value::bool(false)),
        Some(found) => {
            remove_found(elem, found);
            Ok(Value::bool(true))
        }
    }
}

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
fn remove_attribute_modern(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = bytes_arg(req, 1)?;
    let (this, elem) = this_node(req)?;
    let modern = follows_spec(req, this);
    if let Some(found) = attribute_or_decl(modern, elem, &name) {
        remove_found(elem, found);
    }
    Ok(Value::null())
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
fn remove_attribute_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let uri = nullable_bytes_arg(req, 1)?;
    let name = bytes_arg(req, 2)?;
    let (this, elem) = this_node(req)?;
    let modern = follows_spec(req, this);
    let uri = uri.filter(|u| !modern || !u.is_empty());
    let attr = elem.has_ns_prop(&name, uri.as_deref());
    if let Some(ns) = decl_named(elem, Some(&name)).filter(|_| !modern) {
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

/// `dom_element_set_attribute_node_common`: the modern classes refuse an
/// attribute another element owns and adopt one of another document,
/// where the legacy ones refuse it.
fn set_attribute_node_as(req: &mut Request, use_ns: bool, modern: bool) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let (this, elem) = this_node(req)?;
    let Some(attr_obj) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let attr = fetch(req, attr_obj)?;
    if modern {
        if attr.parent().is_some_and(|p| p != elem) {
            return Err(dom_throw(req, Code::InuseAttribute));
        }
        if let (Some(from), Some(doc)) = (attr.doc(), elem.doc())
            && from != doc
        {
            crate::adopt(req, this, attr, doc);
        }
    } else if attr.doc().is_some() && attr.doc() != elem.doc() {
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
    if modern {
        register_attribute_id(req, this, attr);
    } else {
        mark_ids_modified(req, this);
        crate::reconcile_attribute_namespace(attr);
    }
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
    set_attribute_node_as(req, false, false)
}

/// `DOMElement::setAttributeNodeNS(DOMAttr $attr)`.
fn set_attribute_node_ns(req: &mut Request) -> Result<Value, Thrown> {
    set_attribute_node_as(req, true, false)
}

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
fn set_attribute_node_ns_modern(req: &mut Request) -> Result<Value, Thrown> {
    set_attribute_node_as(req, true, true)
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
fn set_attribute_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 3, 3)?;
    let uri = nullable_bytes_arg(req, 1)?;
    let name = bytes_arg(req, 2)?;
    let value = bytes_arg(req, 3)?;
    let (this, elem) = this_node(req)?;
    if follows_spec(req, this) {
        set_attribute_ns_modern(req, this, elem, uri.as_deref(), &name, &value)?;
    } else {
        set_attribute_ns_legacy(req, this, elem, uri.as_deref(), &name, &value)?;
    }
    Ok(Value::null())
}

/// `dom_set_attribute_ns_modern`: the name validated and split, its
/// namespace the mapper's one declaration.
fn set_attribute_ns_modern(
    req: &mut Request,
    this: PhpObject,
    elem: Node,
    uri: Option<&[u8]>,
    name: &[u8],
    value: &[u8],
) -> Result<(), Thrown> {
    let (local, prefix) = match validate_and_extract(uri, name) {
        Ok(parts) => parts,
        Err(code) => return Err(dom_throw(req, code)),
    };
    let ns = mapped_ns(req, this, elem.doc(), prefix.as_deref(), uri);
    match elem.set_ns_prop(ns, &local, value) {
        Some(attr) => {
            register_attribute_id(req, this, attr);
            Ok(())
        }
        None => Err(dom_throw(req, Code::InvalidState)),
    }
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
fn set_id_attribute_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 3, 3)?;
    // `Dom\Element` declares the namespace `?string` where php parses it
    // `s`, so `null` is the deprecation `zend_parse_arg_str_weak` raises.
    if crate::arg(req, 1)?.is_some_and(|v| matches!(v.as_ref(), ValueRef::Null)) {
        arg_null_deprecated(req, 1, "?string");
    }
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
fn toggle_attribute(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 2)?;
    let mut name = bytes_arg(req, 1)?;
    let force = nullable_bool_arg(req, 2)?;
    let (this, elem) = this_node(req)?;
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
                    return Ok(Value::bool(true));
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
    insert_adjacent_element_at(req, &where_)
}

/// `dom_element_insert_adjacent_element`.
fn insert_adjacent_element_at(req: &mut Request, where_: &[u8]) -> Result<Value, Thrown> {
    let Some(other_obj) = object_arg(req, 2)? else {
        return Ok(Value::null());
    };
    let (this, elem) = this_node(req)?;
    let other = fetch(req, other_obj)?;
    match insert_adjacent(req, this, elem, where_, other)? {
        Inserted::Done => object_for(req, other, Some(other_obj)),
        Inserted::NoParent | Inserted::Failed => Ok(Value::null()),
    }
}

/// `DOMElement::insertAdjacentText(string $where, string $data): void`.
fn insert_adjacent_text(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let where_ = bytes_arg(req, 1)?;
    insert_adjacent_text_at(req, &where_)
}

/// `dom_element_insert_adjacent_text`.
fn insert_adjacent_text_at(req: &mut Request, where_: &[u8]) -> Result<Value, Thrown> {
    let data = bytes_arg(req, 2)?;
    let (this, elem) = this_node(req)?;
    let Some(text) = Node::new_text_len(elem.doc(), &data) else {
        return Ok(Value::null());
    };
    match insert_adjacent(req, this, elem, where_, text)? {
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
fn get_elements_by_tag_name(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = path_arg(req, 1)?;
    let this = this_intern(req)?;
    crate::create(
        req,
        this,
        "Dom\\HTMLCollection",
        MapKind::ByTagName {
            local: name,
            ns: None,
            qname: true,
        },
    )
}

/// `Dom\Element::getElementsByTagNameNS(?string $namespace, string $localName): Dom\HTMLCollection`.
fn get_elements_by_tag_name_ns(req: &mut Request) -> Result<Value, Thrown> {
    crate::document::elements_by_tag_name_ns(req, "Dom\\HTMLCollection")
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
fn get_elements_by_class_name(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let names = path_arg(req, 1)?;
    let this = this_intern(req)?;
    let quirks = dom(req, this).is_some_and(|d| d.obj.quirks() == Quirks::Quirks);
    let tokens = crate::token_list::ordered_set(&names, quirks);
    let kind = if tokens.is_empty() {
        MapKind::Noop
    } else {
        MapKind::ByClassName(tokens)
    };
    crate::create(req, this, "Dom\\HTMLCollection", kind)
}

/// `dom_element_get_in_scope_namespace_info`: the `xmlns` attributes of
/// the element and its ancestors, the nearest per prefix, as
/// `Dom\NamespaceInfo` objects appended to `out`, the outermost first; a
/// default declared empty is left out.
fn in_scope_namespace_info(
    req: &mut Request,
    this: PhpObject,
    node: Node,
    out: &mut PhpArray,
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
                let found = mapped_ns(req, this, doc, prefix.as_deref(), Some(&content));
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
            Value::null()
        } else {
            string(req, &prefix)?
        };
        info.set_slot(req, 0, prefix)?;
        let uri = crate::nullable_string(req, href.as_deref())?;
        info.set_slot(req, 1, uri)?;
        let element = object_for(req, node, Some(this))?;
        info.set_slot(req, 2, element)?;
        out.append(req, info.as_value())?;
    }
    Ok(())
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
fn get_in_scope_namespaces(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (this, node) = this_node(req)?;
    let mut out = PhpArray::new(req)?;
    in_scope_namespace_info(req, this, node, &mut out)?;
    Ok(Value::array(out))
}

/// `Dom\Element::getDescendantNamespaces(): array`: the element's, then
/// every descendant element's in tree order.
fn get_descendant_namespaces(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (this, node) = this_node(req)?;
    let mut out = PhpArray::new(req)?;
    in_scope_namespace_info(req, this, node, &mut out)?;
    let mut at = node.children();
    while let Some(cur) = at {
        if cur.kind() == Kind::Element {
            in_scope_namespace_info(req, this, cur, &mut out)?;
        }
        at = crate::next_in_tree_order(cur, node);
    }
    Ok(Value::array(out))
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
fn rename(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let uri = nullable_bytes_arg(req, 1)?;
    let name = bytes_arg(req, 2)?;
    let (this, node) = this_node(req)?;
    let (local, prefix) = match validate_and_extract(uri.as_deref(), &name) {
        Ok(parts) => parts,
        Err(code) => return Err(dom_throw(req, code)),
    };
    if node.kind() == Kind::Attribute {
        let existing = node
            .parent()
            .and_then(|p| p.has_ns_prop(&local, uri.as_deref().filter(|u| !u.is_empty())));
        if existing.is_some_and(|e| e != node) {
            return Err(crate::dom_throw_message(
                req,
                Code::InvalidModification,
                "An attribute with the given name in the given namespace already exists",
            ));
        }
    } else {
        let is_html = crate::private::ns_is(node, HTML_NS);
        let will_be_html = uri.as_deref() == Some(HTML_NS);
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
    let ns = mapped_ns(req, this, node.doc(), prefix.as_deref(), uri.as_deref());
    node.set_ns(ns);
    node.set_name(&local);
    Ok(Value::null())
}

/// The name a `Dom\AdjacentPosition` case stands for, its backing value,
/// which is its name lowercased as `zend_enum_fetch_case_name` gives it
/// to a case-blind comparison.
fn adjacent_position(req: &mut Request) -> Result<Vec<u8>, Thrown> {
    let Some(case) = object_arg(req, 1)? else {
        return Ok(Vec::new());
    };
    Ok(case
        .prop(req, b"value")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
        .unwrap_or_default())
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
fn insert_adjacent_element_modern(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let where_ = adjacent_position(req)?;
    insert_adjacent_element_at(req, &where_)
}

/// `Dom\Element::insertAdjacentText(Dom\AdjacentPosition $where, string $data): void`.
fn insert_adjacent_text_modern(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let where_ = adjacent_position(req)?;
    insert_adjacent_text_at(req, &where_)
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
    ("Dom\\Element::removeAttribute", remove_attribute_modern),
    ("Dom\\Element::removeAttributeNode", remove_attribute_node),
    (
        "Dom\\Element::setAttributeNodeNS",
        set_attribute_node_ns_modern,
    ),
    ("Dom\\Element::setIdAttributeNode", set_id_attribute_node),
    (
        "Dom\\Element::getElementsByTagName",
        get_elements_by_tag_name,
    ),
    (
        "Dom\\Element::getElementsByTagNameNS",
        get_elements_by_tag_name_ns,
    ),
    (
        "Dom\\Element::getElementsByClassName",
        get_elements_by_class_name,
    ),
    (
        "Dom\\Element::insertAdjacentElement",
        insert_adjacent_element_modern,
    ),
    (
        "Dom\\Element::insertAdjacentText",
        insert_adjacent_text_modern,
    ),
    (
        "Dom\\Element::getInScopeNamespaces",
        get_in_scope_namespaces,
    ),
    (
        "Dom\\Element::getDescendantNamespaces",
        get_descendant_namespaces,
    ),
    ("Dom\\Element::rename", rename),
];

/// `DOMElement` and `Dom\Element`, named for the types that hold them
/// until their headers are declared here.
#[php_class(extern)]
pub(crate) struct DOMElement;

#[php_class(extern, namespace(Dom))]
pub(crate) struct Element;
