//! `private_data.c` and the namespace mapper of `namespace_compat.c`: what
//! a document of the `Dom\` classes keeps beside libxml's tree, as
//! `php_dom_private_data` rides on `php_libxml_ref_obj::private_data`.
//!
//! The modern classes keep no namespace declaration on an element
//! (`nsDef`): an element's or an attribute's namespace is one shared
//! declaration per prefix and URI, handed out by the [`Mapper`], and an
//! `xmlns` attribute is an attribute like any other. Where php allocates
//! the mapper's declarations itself and frees them with the private data,
//! these go onto the document's old list, which `xmlFreeDoc` frees after
//! the private data is gone; nothing reads the list but libxml's lookup
//! of the `xml` prefix, which finds the same declaration either way.
//!
//! A `template` element's contents are a fragment no tree holds, found
//! through the element ([`Private::template`]); the element's namespace
//! carries the tag that frees the fragment with it
//! (`dom_add_element_ns_hook`).

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Emalloc;
use php_ext_libxml::ffi::{Kind, Node, NodeObject, Ns, PrivateData};

use crate::document::XMLNS_NS;

/// `DOM_XHTML_NS_URI`.
pub(crate) const HTML_NS: &[u8] = b"http://www.w3.org/1999/xhtml";
/// `DOM_SVG_NS_URI`.
pub(crate) const SVG_NS: &[u8] = b"http://www.w3.org/2000/svg";
/// `DOM_MATHML_NS_URI`.
pub(crate) const MATHML_NS: &[u8] = b"http://www.w3.org/1998/Math/MathML";
/// `DOM_XLINK_NS_URI`.
pub(crate) const XLINK_NS: &[u8] = b"http://www.w3.org/1999/xlink";

/// `sizeof(php_dom_private_data)` on the pinned 64-bit build: the
/// header's two pointers, the namespace mapper's table and two
/// declarations, and `template_fragments` (`Z/ext/dom/private_data.h`).
const PRIVATE_DATA: usize = 96;

/// `php_dom_private_data`.
pub(crate) struct Private {
    pub(crate) mapper: Mapper,
    /// `template_fragments`: a template element and its contents, in the
    /// order they were made.
    templates: Vec<(Node, Node)>,
    /// The struct's `emalloc` on the request's heap, given back by the
    /// header's `dtor` ([`PrivateData::destroy`]).
    charge: Option<Emalloc>,
}

impl PrivateData for Private {
    /// `php_dom_libxml_private_data_ns_hook`.
    fn element_freed(&mut self, element: Node) {
        self.remove_template(element);
    }

    /// `php_dom_private_data_destroy`'s `efree(data)`; the templates'
    /// contents go with the drop.
    fn destroy(&mut self, req: &mut Request) {
        if let Some(charge) = self.charge.take() {
            charge.free(req);
        }
    }
}

/// `php_dom_private_data_destroy`: every template's contents freed.
impl Drop for Private {
    fn drop(&mut self) {
        for (_, fragment) in self.templates.drain(..) {
            fragment.free();
        }
    }
}

impl Private {
    /// `php_dom_private_data_create`: `emalloc(sizeof(*private_data))`,
    /// an empty mapper and no templates; the `Err` is the memory limit.
    pub(crate) fn new(req: &mut Request) -> Result<Private, Thrown> {
        Ok(Private {
            mapper: Mapper::default(),
            templates: Vec::new(),
            charge: Some(Emalloc::new(req, PRIVATE_DATA)?),
        })
    }

    /// `php_dom_private_data_destroy` with the request at hand, for data
    /// no document took: the struct's `efree`, the templates' contents
    /// with the drop.
    pub(crate) fn free(mut self, req: &mut Request) {
        self.destroy(req);
    }

    /// `php_dom_retrieve_templated_content`.
    pub(crate) fn template(&self, element: Node) -> Option<Node> {
        self.templates
            .iter()
            .find(|(e, _)| *e == element)
            .map(|(_, f)| *f)
    }

    /// `php_dom_add_templated_content`.
    pub(crate) fn add_template(&mut self, element: Node, fragment: Node) {
        self.templates.push((element, fragment));
    }

    /// `php_dom_remove_templated_content`: the entry taken out first, so
    /// the nested contents it frees find the table whole.
    pub(crate) fn remove_template(&mut self, element: Node) {
        let Some(at) = self.templates.iter().position(|(e, _)| *e == element) else {
            return;
        };
        let (_, fragment) = self.templates.remove(at);
        self.free_templated_content(fragment);
    }

    /// `php_dom_free_templated_content`: no object can stand for a node of
    /// a template's contents, so the fragment is freed whole after the
    /// contents nested in it.
    fn free_templated_content(&mut self, base: Node) {
        if !self.templates.is_empty() {
            let mut at = base.children();
            while let Some(node) = at {
                if node.kind() == Kind::Element {
                    self.remove_template(node);
                }
                at = crate::next_in_tree_order(node, base);
            }
        }
        base.free();
    }

    /// `php_dom_get_template_count`.
    pub(crate) fn template_count(&self) -> usize {
        self.templates.len()
    }
}

/// `php_dom_libxml_ns_mapper`: one declaration per prefix and URI.
#[derive(Default)]
pub(crate) struct Mapper {
    /// Cached, since an HTML document asks for it for every element.
    html: Option<Ns>,
    /// Cached, since every prefixless `xmlns` attribute of XML asks.
    prefixless_xmlns: Option<Ns>,
    /// `uri_to_prefix_map`: the URI, the prefix (empty for none), the
    /// declaration.
    map: Vec<(Vec<u8>, Vec<u8>, Ns)>,
}

impl Mapper {
    /// `php_dom_libxml_ns_mapper_get_ns`: the declaration of `uri` under
    /// `prefix`, made the first time; none where both are empty.
    pub(crate) fn get(&mut self, doc: Option<Node>, prefix: &[u8], uri: &[u8]) -> Option<Ns> {
        if prefix.is_empty() && uri.is_empty() {
            return None;
        }
        if let Some((_, _, ns)) = self.map.iter().find(|(u, p, _)| u == uri && p == prefix) {
            return Some(*ns);
        }
        let ns = Ns::owned(Some(uri), (!prefix.is_empty()).then_some(prefix))?;
        ns.move_to_old(doc);
        self.map.push((uri.to_vec(), prefix.to_vec(), ns));
        Some(ns)
    }

    /// `php_dom_libxml_ns_mapper_get_ns_raw_strings_nullsafe`.
    pub(crate) fn get_nullsafe(
        &mut self,
        doc: Option<Node>,
        prefix: Option<&[u8]>,
        uri: Option<&[u8]>,
    ) -> Option<Ns> {
        self.get(doc, prefix.unwrap_or_default(), uri.unwrap_or_default())
    }

    /// `php_dom_libxml_ns_mapper_ensure_html_ns`.
    pub(crate) fn html(&mut self, doc: Option<Node>) -> Option<Ns> {
        if self.html.is_none() {
            self.html = self.get(doc, b"", HTML_NS);
        }
        self.html
    }

    /// `php_dom_libxml_ns_mapper_ensure_prefixless_xmlns_ns`.
    pub(crate) fn prefixless_xmlns(&mut self, doc: Option<Node>) -> Option<Ns> {
        if self.prefixless_xmlns.is_none() {
            self.prefixless_xmlns = self.get(doc, b"", XMLNS_NS);
        }
        self.prefixless_xmlns
    }

    /// `php_dom_libxml_ns_mapper_store_and_normalize_parsed_ns`: a
    /// declaration the parser made, known under its prefix and URI unless
    /// one is already.
    fn store_parsed(&mut self, ns: Ns) {
        let uri = ns.href().unwrap_or_default();
        let prefix = ns.prefix().unwrap_or_default();
        if !self.map.iter().any(|(u, p, _)| *u == uri && *p == prefix) {
            self.map.push((uri, prefix, ns));
        }
    }

    /// `php_dom_ns_compat_mark_attribute`: the `xmlns` attribute that
    /// declares `ns` on `node`.
    pub(crate) fn mark_attribute(&mut self, node: Node, ns: Ns) -> Option<Node> {
        let doc = node.doc();
        let (xmlns, name) = match ns.prefix() {
            Some(prefix) => (self.get(doc, b"xmlns", XMLNS_NS), prefix),
            None => (self.prefixless_xmlns(doc), b"xmlns".to_vec()),
        };
        node.set_ns_prop(xmlns, &name, &ns.href().unwrap_or_default())
    }

    /// `php_dom_ns_compat_mark_attribute_list`: an element's declarations
    /// turned into `xmlns` attributes in front of its others, in their
    /// order, each declaration moved to the document's old list and known
    /// to the mapper.
    pub(crate) fn mark_attribute_list(&mut self, node: Node) {
        let Some(first) = node.ns_def() else {
            return;
        };
        let attrs = node.properties();
        node.set_properties(None);
        let mut last_added = None;
        let mut at = Some(first);
        while let Some(ns) = at {
            last_added = self.mark_attribute(node, ns);
            self.store_parsed(ns);
            at = ns.next();
            ns.set_next(None);
            ns.move_to_old(node.doc());
        }
        match last_added {
            Some(last) => {
                if let Some(rest) = attrs {
                    last.set_next(Some(rest));
                    rest.set_prev(Some(last));
                }
            }
            None => node.set_properties(attrs),
        }
        node.set_ns_def(None);
    }

    /// `php_dom_get_in_scope_ns`: the declarations in scope at `node`, the
    /// nearest of each prefix, from an element's own namespace unless
    /// `ignore_elements`, and from its prefixed `xmlns` attributes.
    pub(crate) fn in_scope(&mut self, node: Node, ignore_elements: bool) -> Vec<Ns> {
        let doc = node.doc();
        let mut found: Vec<(Vec<u8>, Ns)> = Vec::new();
        let add = |found: &mut Vec<(Vec<u8>, Ns)>, prefix: Vec<u8>, ns: Ns| {
            if !found.iter().any(|(p, _)| *p == prefix) {
                found.push((prefix, ns));
            }
        };
        let mut at = Some(node);
        while let Some(cur) = at {
            if cur.kind() == Kind::Element {
                if !ignore_elements
                    && let Some(ns) = cur.ns()
                    && let Some(prefix) = ns.prefix()
                {
                    add(&mut found, prefix, ns);
                }
                let mut attr = cur.properties();
                while let Some(a) = attr {
                    if let Some(ans) = a.ns()
                        && ans.prefix().is_some()
                        && ans.href().as_deref() == Some(XMLNS_NS)
                        && let Some(value) = a.children().and_then(Node::content)
                    {
                        let prefix = a.name();
                        if let Some(ns) = self.get(doc, &prefix, &value) {
                            add(&mut found, prefix, ns);
                        }
                    }
                    attr = a.next();
                }
            }
            at = cur.parent();
        }
        found.into_iter().map(|(_, ns)| ns).collect()
    }

    /// `php_dom_libxml_reconcile_modern`: a subtree that came from another
    /// document pointed at this mapper's declarations.
    pub(crate) fn reconcile(&mut self, doc: Option<Node>, node: Node) {
        if node.kind() == Kind::Attribute {
            if let Some(ns) = node.ns() {
                let fresh = self.get_nullsafe(doc, ns.prefix().as_deref(), ns.href().as_deref());
                node.set_ns(fresh);
            }
            return;
        }
        if node.kind() != Kind::Element
            || (node.children().is_none()
                && node.properties().is_none()
                && node.ns() == node.ns_def())
        {
            return;
        }
        let mut seen: Vec<(Ns, Option<Ns>)> = Vec::new();
        self.reconcile_element(doc, node, &mut seen);
        let mut at = node.children();
        while let Some(n) = at {
            if n.kind() == Kind::Element {
                self.reconcile_element(doc, n, &mut seen);
            }
            at = crate::next_in_tree_order(n, node);
        }
    }

    fn reconcile_element(
        &mut self,
        doc: Option<Node>,
        element: Node,
        seen: &mut Vec<(Ns, Option<Ns>)>,
    ) {
        self.reconcile_single(doc, element, seen);
        let mut attr = element.properties();
        while let Some(a) = attr {
            self.reconcile_single(doc, a, seen);
            attr = a.next();
        }
    }

    /// `php_dom_libxml_reconcile_modern_single_node`.
    fn reconcile_single(
        &mut self,
        doc: Option<Node>,
        node: Node,
        seen: &mut Vec<(Ns, Option<Ns>)>,
    ) {
        let Some(ns) = node.ns() else {
            return;
        };
        if let Some((_, fresh)) = seen.iter().find(|(old, _)| *old == ns) {
            node.set_ns(*fresh);
            return;
        }
        let fresh = self.get_nullsafe(doc, ns.prefix().as_deref(), ns.href().as_deref());
        seen.push((ns, fresh));
        node.set_ns(fresh);
    }
}

/// The document's private data, laid down where a modern document has
/// none yet.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r xmlns="urn:a" xmlns:b="urn:b" c="1"><b:x/></r>');
/// foreach ($d->documentElement->attributes as $a) echo $a->name, '=', $a->value, ' ', var_export($a->namespaceURI, true), "\n";
/// ```
/// ```expect
/// xmlns=urn:a 'http://www.w3.org/2000/xmlns/'
/// xmlns:b=urn:b 'http://www.w3.org/2000/xmlns/'
/// c=1 NULL
/// ```
pub(crate) fn of(obj: &mut NodeObject) -> Option<&mut Private> {
    obj.private_data_mut::<Private>()
}

/// `php_dom_ensure_templated_content`: a template element's contents,
/// made empty the first time, the element's namespace hooked so the
/// contents go with it.
///
/// ```php
/// $d = Dom\HTMLDocument::createFromString('<!doctype html><template id=t><b>x</b></template>');
/// $t = $d->getElementById('t');
/// var_dump($t->childNodes->length, $t->innerHTML);
/// $t->insertAdjacentHTML(Dom\AdjacentPosition::BeforeEnd, '<i>y</i>');
/// var_dump($t->innerHTML, $d->saveHtml($t));
/// ```
/// ```expect
/// int(0)
/// string(8) "<b>x</b>"
/// string(8) "<b>x</b>"
/// string(36) "<template id="t"><b>x</b></template>"
/// ```
pub(crate) fn ensure_template(obj: &mut NodeObject, element: Node) -> Option<Node> {
    if let Some(found) = obj
        .private_data::<Private>()
        .and_then(|p| p.template(element))
    {
        return Some(found);
    }
    let fragment = Node::new_fragment(element.doc())?;
    fragment.set_parent(Some(element));
    obj.add_ns_tag(element);
    of(obj)?.add_template(element, fragment);
    Some(fragment)
}

/// `php_dom_ns_is_fast(node, token)`: whether the node's namespace is
/// `uri`.
///
/// ```php
/// $h = Dom\HTMLDocument::createFromString('<!doctype html><p>x</p><svg><rect/></svg>', LIBXML_NOERROR);
/// foreach ($h->body->childNodes as $c) echo get_class($c), ' ', $c->nodeName, ' ', $c->tagName, "\n";
/// $x = Dom\XMLDocument::createFromString('<p xmlns="http://www.w3.org/1999/xhtml"/>');
/// echo get_class($x->documentElement), ' ', $x->documentElement->tagName, "\n";
/// ```
/// ```expect
/// Dom\HTMLElement P P
/// Dom\Element svg svg
/// Dom\HTMLElement p
/// ```
pub(crate) fn ns_is(node: Node, uri: &[u8]) -> bool {
    node.ns().and_then(Ns::href).is_some_and(|h| h == uri)
}

/// `php_dom_ns_is_html_and_document_is_html`.
///
/// ```php
/// $h = Dom\HTMLDocument::createFromString('<!doctype html><p>x</p><svg><rect/></svg>', LIBXML_NOERROR);
/// foreach ($h->body->childNodes as $c) echo get_class($c), ' ', $c->nodeName, ' ', $c->tagName, "\n";
/// $x = Dom\XMLDocument::createFromString('<p xmlns="http://www.w3.org/1999/xhtml"/>');
/// echo get_class($x->documentElement), ' ', $x->documentElement->tagName, "\n";
/// ```
/// ```expect
/// Dom\HTMLElement P P
/// Dom\Element svg svg
/// Dom\HTMLElement p
/// ```
pub(crate) fn html_in_html_document(node: Node) -> bool {
    node.doc().is_some_and(|d| d.kind() == Kind::HtmlDocument) && ns_is(node, HTML_NS)
}

/// Private data laid over the data a document holds gives the old data
/// up with the request at hand (#2132): served armed, a native that lays
/// a fresh `Private` over a modern document's leaves no block of the
/// first standing, where a replace that dropped it left its 96-byte
/// `php_dom_private_data`.
#[cfg(all(test, debug_assertions))]
mod tests {
    use php_ext::abi::{Extension, ExtensionBuilder};
    use php_ext::attr::php_function;
    use php_ext::raising::Thrown;
    use php_ext::request::Request;
    use php_ext::test::{Capture, Engine, LeakSensor};
    use php_ext::values::Value;

    use super::Private;

    /// `replace_private(object $document): void`: a fresh private data
    /// laid over the one `$document` holds.
    #[php_function]
    fn replace_private(req: &mut Request, document: Value) -> Result<(), Thrown> {
        let Some(object) = document.as_object().copied() else {
            return Ok(());
        };
        let private = Private::new(req)?;
        match crate::libxml_half(req, object) {
            Some(mut obj) => obj.set_private_data(req, Box::new(private)),
            None => private.free(req),
        }
        Ok(())
    }

    fn replacing() -> Extension {
        ExtensionBuilder::default()
            .functions(vec![replace_private::declared()])
            .build("replacing")
    }

    #[test]
    fn a_replaced_private_data_is_given_up() {
        let engine = Engine::builder()
            .extensions(&[
                php_ext_libxml::extension(),
                php_ext_lexbor::extension(),
                php_ext_standard::extension(),
                php_ext_uri::extension(),
                crate::extension(),
                replacing(),
            ])
            .sense_leaks(LeakSensor::Armed)
            .build();
        let served = engine.serve_capture(
            "<?php $d = Dom\\XMLDocument::createFromString('<r/>'); replace_private($d); replace_private($d); echo $d->documentElement->nodeName, \"\\n\";",
            Capture::default(),
        );
        assert_eq!(served.fatal, None);
        assert_eq!(String::from_utf8_lossy(&served.stdout), "r\n");
    }
}
