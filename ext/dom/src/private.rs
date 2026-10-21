//! `private_data.c` and the namespace mapper of `namespace_compat.c`: what
//! a document of the `Dom\` classes keeps beside libxml's tree, as
//! `php_dom_private_data` rides on `php_libxml_ref_obj::private_data`.
//!
//! The modern classes keep no namespace declaration on an element
//! (`nsDef`): an element's or an attribute's namespace is one shared
//! declaration per prefix and URI, handed out by the [`Mapper`], and an
//! `xmlns` attribute is an attribute like any other. The mapper's tables,
//! the tables its walks keep, the template table, the strings they key
//! by and the declarations the mapper makes are on the request's heap at
//! php's sizes, made and freed where php's are (design §5.13, [`Table`]).
//! Where php's declaration is its `emalloc`, whose strings are the tables'
//! keys, a declaration here is libxml's, charged its `emalloc` and put on
//! the document's old list, which `xmlFreeDoc` frees after the private
//! data is gone; nothing reads the list but libxml's lookup of the `xml`
//! prefix, which finds the same declaration either way.
//!
//! A `template` element's contents are a fragment no tree holds, found
//! through the element ([`Private::template`]); the element's namespace
//! carries the tag that frees the fragment with it
//! (`dom_add_element_ns_hook`).

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Emalloc, Owner, PhpArray, PhpObject, PhpString, Value};
use php_ext_libxml::ffi::{Kind, Node, NodeObject, Ns, PrivateData};
use php_ext_libxml::table::Table;

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

/// `sizeof(xmlNsPtr)`, a slot of the in-scope list.
const NS_PTR: usize = 8;

/// `sizeof(xmlNs)` on the pinned 64-bit build, what `dom_create_owned_ns`
/// `emalloc`s for a declaration.
const XML_NS: usize = 48;

/// `php_dom_private_data`.
pub(crate) struct Private {
    pub(crate) mapper: Mapper,
    /// `template_fragments`: each template element's contents, in the
    /// order they were made, the table made at the first.
    templates: Option<Table<Template>>,
    /// The struct's `emalloc` on the request's heap, given back by the
    /// header's `dtor` ([`PrivateData::destroy`]).
    charge: Option<Emalloc>,
}

/// One row of `template_fragments`: the fragment the element's mangled
/// pointer keys.
struct Template {
    element: Node,
    fragment: Node,
}

impl PrivateData for Private {
    /// `php_dom_libxml_private_data_ns_hook`: the row deleted, which frees
    /// nothing of the request's heap, so the hook needs no request.
    fn element_freed(&mut self, element: Node) {
        self.remove_template(element);
    }

    /// `php_dom_private_data_destroy`: the mapper's table destroyed,
    /// every template's contents freed and the table with them, then
    /// `efree(data)`.
    fn destroy(&mut self, req: &mut Request) {
        self.mapper.destroy(req);
        self.free_templates();
        if let Some(mut table) = self.templates.take() {
            table.free(req);
        }
        if let Some(charge) = self.charge.take() {
            charge.free(req);
        }
    }
}

/// `php_dom_private_data_destroy` with no request at hand: every
/// template's contents freed, what the tables hold of the request's heap
/// the request's end's.
impl Drop for Private {
    fn drop(&mut self) {
        self.free_templates();
    }
}

impl Private {
    /// `php_dom_private_data_create`: `emalloc(sizeof(*private_data))`,
    /// an empty mapper and no templates; the `Err` is the memory limit.
    pub(crate) fn new(req: &mut Request) -> Result<Private, Thrown> {
        Ok(Private {
            mapper: Mapper::default(),
            templates: None,
            charge: Some(Emalloc::new(req, PRIVATE_DATA)?),
        })
    }

    /// `php_dom_private_data_destroy` with the request at hand, for data
    /// no document took.
    pub(crate) fn free(mut self, req: &mut Request) {
        self.destroy(req);
    }

    /// `ZEND_HASH_MAP_FOREACH_PTR(template_fragments)`'s `xmlFreeNode`.
    fn free_templates(&mut self) {
        if let Some(table) = self.templates.as_mut() {
            for row in table.take_rows() {
                row.fragment.free();
            }
        }
    }

    /// `php_dom_retrieve_templated_content`.
    pub(crate) fn template(&self, element: Node) -> Option<Node> {
        self.templates
            .as_ref()?
            .rows()
            .find(|row| row.element == element)
            .map(|row| row.fragment)
    }

    /// `php_dom_add_templated_content`: the table `ALLOC_HASHTABLE`d and
    /// laid down at the first, then the row; the `Err` is the memory
    /// limit, the fragment left to the caller.
    ///
    /// So a document with a template costs the request what php's does,
    /// and nothing of it is given back before the document is:
    ///
    /// ```php
    /// $f = fn () => Dom\HTMLDocument::createFromString('<template><p>x</p></template>', LIBXML_NOERROR);
    /// $d = $f(); unset($d);
    /// $m = memory_get_usage();
    /// $d = $f();
    /// echo memory_get_usage() - $m, "\n";
    /// $t = $d->getElementsByTagName('template')->item(0);
    /// $t->innerHTML = '<i>y</i>';
    /// $t->remove(); unset($t);
    /// echo memory_get_usage() - $m, "\n";
    /// unset($d);
    /// echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 1440
    /// 1472
    /// 32
    /// ```
    pub(crate) fn add_template(
        &mut self,
        req: &mut Request,
        element: Node,
        fragment: Node,
    ) -> Result<(), Thrown> {
        let table = match &mut self.templates {
            Some(table) => table,
            None => self.templates.insert(Table::allocated(req)?),
        };
        table.add(req, Template { element, fragment })
    }

    /// `php_dom_remove_templated_content`: the row deleted first, so the
    /// nested contents it frees find the table whole.
    pub(crate) fn remove_template(&mut self, element: Node) {
        let Some(table) = self.templates.as_mut() else {
            return;
        };
        let Some(at) = table.rows().position(|row| row.element == element) else {
            return;
        };
        let row = table.remove(at);
        self.free_templated_content(row.fragment);
    }

    /// `php_dom_free_templated_content`: no object can stand for a node of
    /// a template's contents, so the fragment is freed whole after the
    /// contents nested in it.
    fn free_templated_content(&mut self, base: Node) {
        if self.template_count() > 0 {
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
        self.templates.as_ref().map_or(0, Table::len)
    }
}

/// A string a mapper entry point is handed, as php's holds it, which
/// says what the call allocates and what a table keeps of it.
#[derive(Clone, Copy)]
pub(crate) enum Handed<'a> {
    /// `zend_empty_string`, or the `NULL` an entry point reads as it:
    /// interned, so nothing is allocated or kept.
    Empty,
    /// The `zend_string_init` the entry point makes of these bytes and
    /// releases when it returns, standing past it only where a table took
    /// it as a key.
    Made(&'a [u8]),
    /// The caller's own string, a PHP argument: a table that takes it as
    /// a key counts it once more.
    Given(PhpString),
}

impl Handed<'_> {
    /// A PHP argument that may be null, which an entry point reads as the
    /// empty string.
    pub(crate) fn argument(argument: Option<PhpString>) -> Handed<'static> {
        argument.map_or(Handed::Empty, Handed::Given)
    }

    /// The string the call holds; the `Err` is the memory limit.
    fn hold(self, req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
        match self {
            Handed::Empty => PhpString::new(req, b""),
            Handed::Made(bytes) => {
                PhpString::filled(req, bytes.len(), |to| to.copy_from_slice(bytes))
            }
            Handed::Given(given) => Ok(Owner::take(req, given)),
        }
    }
}

/// A declaration a prefix table names.
enum Decl {
    /// `DOM_Z_OWNED`: one the mapper made, charged the `emalloc` php's is.
    Owned { ns: Ns, charge: Emalloc },
    /// `DOM_Z_UNOWNED`: one a parser made, which the mapper only knows.
    Parsed(Ns),
}

impl Decl {
    fn ns(&self) -> Ns {
        match self {
            Decl::Owned { ns, .. } | Decl::Parsed(ns) => *ns,
        }
    }
}

/// Where a declaration stands in the mapper's list, what a prefix table's
/// row holds in the place of php's `xmlNsPtr`: written as the row's value
/// and read back from it here and nowhere else.
#[derive(Clone, Copy)]
struct DeclAt(usize);

impl DeclAt {
    /// The row's value. A list of declarations never outgrows `i64`.
    fn value(self) -> Owner<Value> {
        Owner::int(i64::try_from(self.0).unwrap_or(i64::MAX))
    }

    /// The place a row's value names.
    fn read(value: &Value) -> Option<DeclAt> {
        value
            .as_int()
            .and_then(|n| usize::try_from(n).ok())
            .map(DeclAt)
    }
}

/// One row of `uri_to_prefix_map`: the URI's string, which the bucket
/// keeps as its key, and the URI's table of prefixes, an `emalloc`'d
/// `HashTable` as php's is.
struct UriRow {
    key: Owner<PhpString>,
    prefixes: Owner<PhpArray>,
}

/// `uri_to_prefix_map`, the [`Table`] php embeds in the private data, so
/// only its data is an allocation of its own.
#[derive(Default)]
struct UriMap(Table<UriRow>);

impl UriMap {
    /// `zend_hash_find`: the prefix table `uri` keys.
    fn find(&self, uri: &[u8]) -> Option<PhpArray> {
        self.0
            .rows()
            .find(|row| row.key.as_bytes() == uri)
            .map(|row| *row.prefixes)
    }

    /// `zend_hash_add_new`: room made for the row, then the row; the
    /// `Err` is the memory limit, the row given up.
    fn add_uri(&mut self, req: &mut Request, row: UriRow) -> Result<PhpArray, Thrown> {
        if let Err(thrown) = self.0.make_room(req) {
            row.key.give_up(req);
            row.prefixes.give_up(req);
            return Err(thrown);
        }
        let prefixes = *row.prefixes;
        self.0.push(row);
        Ok(prefixes)
    }

    /// `zend_hash_destroy`: each prefix table destroyed and each key
    /// released, in order, then the data freed.
    fn destroy(&mut self, req: &mut Request) {
        for row in self.0.take_rows() {
            row.prefixes.give_up(req);
            row.key.give_up(req);
        }
        self.0.free(req);
    }
}

/// `php_dom_libxml_ns_mapper`: one declaration per prefix and URI.
#[derive(Default)]
pub(crate) struct Mapper {
    /// Cached, since an HTML document asks for it for every element.
    html: Option<Ns>,
    /// Cached, since every prefixless `xmlns` attribute of XML asks.
    prefixless_xmlns: Option<Ns>,
    /// `uri_to_prefix_map`: each URI's table, whose prefixes key the
    /// places of their declarations in `decls`.
    table: UriMap,
    /// The declarations the prefix tables name.
    decls: Vec<Decl>,
}

impl Mapper {
    /// `zend_hash_destroy(&uri_to_prefix_map)`: every URI's table with
    /// its keys, and each declaration the mapper made `efree`d.
    fn destroy(&mut self, req: &mut Request) {
        self.table.destroy(req);
        for decl in self.decls.drain(..) {
            if let Decl::Owned { charge, .. } = decl {
                charge.free(req);
            }
        }
        self.html = None;
        self.prefixless_xmlns = None;
    }

    /// `php_dom_libxml_ns_mapper_ensure_prefix_map`: `uri`'s table of
    /// prefixes, an `emalloc`'d `HashTable` keyed by `uri` the first
    /// time; the `Err` is the memory limit.
    fn prefix_map(&mut self, req: &mut Request, uri: PhpString) -> Result<PhpArray, Thrown> {
        if let Some(prefixes) = self.table.find(uri.as_bytes()) {
            return Ok(prefixes);
        }
        let prefixes = PhpArray::new(req)?;
        let key = Owner::take(req, uri);
        self.table.add_uri(req, UriRow { key, prefixes })
    }

    /// The declaration a prefix table names under `prefix`.
    fn named(&self, prefixes: PhpArray, prefix: &[u8]) -> Option<Ns> {
        let at = DeclAt::read(&prefixes.find(prefix)?)?;
        self.decls.get(at.0).map(Decl::ns)
    }

    /// A declaration put at the end of the list, and its place.
    fn push(&mut self, decl: Decl) -> DeclAt {
        self.decls.push(decl);
        DeclAt(self.decls.len() - 1)
    }

    /// `php_dom_libxml_ns_mapper_get_ns`: the declaration of `uri` under
    /// `prefix`, made the first time, `dom_create_owned_ns`'s `emalloc`
    /// before the prefix table keeps it; none where both are empty. The
    /// strings are held as the entry point holds them, the prefix's made
    /// first, and released when it returns. The `Err` is the memory
    /// limit.
    ///
    /// So what a document's declarations cost is php's: a parsed one's
    /// `xmlns` attributes, an element made in a namespace by a string
    /// the program holds, which the table keys by without a copy, and a
    /// copy pointed at the same declarations, all given back with the
    /// document but for 32 bytes php keeps of this program too:
    ///
    /// ```php
    /// $x = fn () => Dom\XMLDocument::createFromString('<r xmlns="urn:a" xmlns:b="urn:b"><b:x b:y="1"/></r>');
    /// $d = $x(); unset($d);
    /// $m = memory_get_usage();
    /// $d = $x();
    /// echo memory_get_usage() - $m, "\n";
    /// $e = $d->createElementNS('urn:c', 'c:e');
    /// echo memory_get_usage() - $m, "\n";
    /// $d->documentElement->appendChild($e);
    /// $c = $d->documentElement->cloneNode(true);
    /// echo memory_get_usage() - $m, "\n";
    /// unset($d, $e, $c);
    /// echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 2024
    /// 2632
    /// 2752
    /// 32
    /// ```
    pub(crate) fn get_ns(
        &mut self,
        req: &mut Request,
        doc: Option<Node>,
        prefix: Handed<'_>,
        uri: Handed<'_>,
    ) -> Result<Option<Ns>, Thrown> {
        let prefix = prefix.hold(req)?;
        let uri = match uri.hold(req) {
            Ok(uri) => uri,
            Err(thrown) => {
                prefix.give_up(req);
                return Err(thrown);
            }
        };
        let found = self.get_held(req, doc, *prefix, *uri);
        prefix.give_up(req);
        uri.give_up(req);
        found
    }

    fn get_held(
        &mut self,
        req: &mut Request,
        doc: Option<Node>,
        prefix: PhpString,
        uri: PhpString,
    ) -> Result<Option<Ns>, Thrown> {
        if prefix.is_empty() && uri.is_empty() {
            return Ok(None);
        }
        let mut prefixes = self.prefix_map(req, uri)?;
        if let Some(found) = self.named(prefixes, prefix.as_bytes()) {
            return Ok(Some(found));
        }
        let charge = Emalloc::new(req, XML_NS)?;
        let Some(ns) = Ns::owned(
            Some(uri.as_bytes()),
            (!prefix.is_empty()).then_some(prefix.as_bytes()),
        ) else {
            charge.free(req);
            return Ok(None);
        };
        ns.move_to_old(doc);
        let at = self.push(Decl::Owned { ns, charge });
        let key = Owner::take(req, prefix).into_key();
        prefixes.set(req, key, at.value())?;
        Ok(Some(ns))
    }

    /// `php_dom_libxml_ns_mapper_get_ns_raw_strings_nullsafe`, and
    /// `_raw_strings` where neither is null: both strings made, a null
    /// one as the empty string.
    pub(crate) fn get_raw(
        &mut self,
        req: &mut Request,
        doc: Option<Node>,
        prefix: Option<&[u8]>,
        uri: Option<&[u8]>,
    ) -> Result<Option<Ns>, Thrown> {
        let prefix = Handed::Made(prefix.unwrap_or_default());
        let uri = Handed::Made(uri.unwrap_or_default());
        self.get_ns(req, doc, prefix, uri)
    }

    /// `php_dom_libxml_ns_mapper_get_ns_raw_prefix_string`: an empty
    /// prefix the interned one, any other made.
    pub(crate) fn get_raw_prefix(
        &mut self,
        req: &mut Request,
        doc: Option<Node>,
        prefix: &[u8],
        uri: Handed<'_>,
    ) -> Result<Option<Ns>, Thrown> {
        let prefix = if prefix.is_empty() {
            Handed::Empty
        } else {
            Handed::Made(prefix)
        };
        self.get_ns(req, doc, prefix, uri)
    }

    /// `php_dom_libxml_ns_mapper_ensure_html_ns`.
    pub(crate) fn html(
        &mut self,
        req: &mut Request,
        doc: Option<Node>,
    ) -> Result<Option<Ns>, Thrown> {
        if self.html.is_none() {
            self.html = self.get_ns(req, doc, Handed::Empty, Handed::Made(HTML_NS))?;
        }
        Ok(self.html)
    }

    /// `php_dom_libxml_ns_mapper_ensure_prefixless_xmlns_ns`.
    pub(crate) fn prefixless_xmlns(
        &mut self,
        req: &mut Request,
        doc: Option<Node>,
    ) -> Result<Option<Ns>, Thrown> {
        if self.prefixless_xmlns.is_none() {
            self.prefixless_xmlns = self.get_ns(req, doc, Handed::Empty, Handed::Made(XMLNS_NS))?;
        }
        Ok(self.prefixless_xmlns)
    }

    /// `php_dom_libxml_ns_mapper_store_and_normalize_parsed_ns`: a
    /// declaration the parser made, known under its prefix and URI unless
    /// one is already, the URI's string released before the prefix's key
    /// is made.
    fn store_parsed(&mut self, req: &mut Request, ns: Ns) -> Result<(), Thrown> {
        let href = ns.href().unwrap_or_default();
        let held = Handed::Made(&href).hold(req)?;
        let prefixes = self.prefix_map(req, *held);
        held.give_up(req);
        let mut prefixes = prefixes?;
        let prefix = ns.prefix().unwrap_or_default();
        if prefixes.find(&prefix).is_none() {
            let at = self.push(Decl::Parsed(ns));
            prefixes.set_str(req, &prefix, at.value())?;
        }
        Ok(())
    }

    /// `php_dom_ns_compat_mark_attribute`: the `xmlns` attribute that
    /// declares `ns` on `node`.
    pub(crate) fn mark_attribute(
        &mut self,
        req: &mut Request,
        node: Node,
        ns: Ns,
    ) -> Result<Option<Node>, Thrown> {
        let doc = node.doc();
        let (xmlns, name) = match ns.prefix() {
            Some(prefix) => (
                self.get_raw(req, doc, Some(b"xmlns"), Some(XMLNS_NS))?,
                prefix,
            ),
            None => (self.prefixless_xmlns(req, doc)?, b"xmlns".to_vec()),
        };
        Ok(node.set_ns_prop(xmlns, &name, &ns.href().unwrap_or_default()))
    }

    /// `php_dom_ns_compat_mark_attribute_list`: an element's declarations
    /// turned into `xmlns` attributes in front of its others, in their
    /// order, each declaration moved to the document's old list and known
    /// to the mapper.
    pub(crate) fn mark_attribute_list(
        &mut self,
        req: &mut Request,
        node: Node,
    ) -> Result<(), Thrown> {
        let Some(first) = node.ns_def() else {
            return Ok(());
        };
        let attrs = node.properties();
        node.set_properties(None);
        let mut last_added = None;
        let mut at = Some(first);
        let mut marked = Ok(());
        while let Some(ns) = at {
            if marked.is_ok() {
                marked = self
                    .mark_attribute(req, node, ns)
                    .map(|added| last_added = added)
                    .and_then(|()| self.store_parsed(req, ns));
            }
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
        marked
    }

    /// `php_dom_get_in_scope_ns`: the declarations in scope at `node`, the
    /// nearest of each prefix, from an element's own namespace unless
    /// `ignore_elements`, and from its prefixed `xmlns` attributes. They
    /// are gathered in a table laid down at once and keyed by a copy of
    /// each prefix, which a prefix already in it is not, and listed in an
    /// `emalloc`'d array before the table is destroyed; the `Err` is the
    /// memory limit.
    ///
    /// So a query that registers the namespaces in scope at its node, and an
    /// XML fragment parsed in place, peak where php's do:
    ///
    /// ```php
    /// $x = '<r';
    /// foreach (range(1, 10) as $i) $x .= " xmlns:p$i=\"urn:$i\"";
    /// $d = Dom\XMLDocument::createFromString($x . '><p3:a xmlns:longer="urn:z"><p5:b/></p3:a></r>');
    /// $xp = new Dom\XPath($d);
    /// $b = $d->getElementsByTagNameNS('urn:5', 'b')->item(0);
    /// $xp->query('//p1:a', $b, true);
    /// memory_reset_peak_usage(); $m = memory_get_usage();
    /// $n = $xp->evaluate('count(//p3:a)', $b, true);
    /// echo $n, ' ', memory_get_usage() - $m, ' ', memory_get_peak_usage() - $m, "\n";
    /// memory_reset_peak_usage(); $m = memory_get_usage();
    /// $b->innerHTML = '<p7:c/><longer:q/>';
    /// echo memory_get_usage() - $m, ' ', memory_get_peak_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 1 32 1216
    /// 0 1216
    /// ```
    pub(crate) fn in_scope(
        &mut self,
        req: &mut Request,
        node: Node,
        ignore_elements: bool,
    ) -> Result<InScope, Thrown> {
        let mut table = Table::default();
        let gathered = table
            .real_init(req)
            .and_then(|()| self.gather_in_scope(req, &mut table, node, ignore_elements));
        let listed = gathered.and_then(|()| Emalloc::new(req, table.len() * NS_PTR));
        let mut list = Vec::with_capacity(table.len());
        for (key, ns) in table.take_rows() {
            key.give_up(req);
            list.push(ns);
        }
        table.free(req);
        Ok(InScope {
            list,
            charge: listed?,
        })
    }

    fn gather_in_scope(
        &mut self,
        req: &mut Request,
        table: &mut Table<(Owner<PhpString>, Ns)>,
        node: Node,
        ignore_elements: bool,
    ) -> Result<(), Thrown> {
        let doc = node.doc();
        let mut at = Some(node);
        while let Some(cur) = at {
            if cur.kind() == Kind::Element {
                if !ignore_elements
                    && let Some(ns) = cur.ns()
                    && let Some(prefix) = ns.prefix()
                {
                    add_in_scope(req, table, &prefix, ns)?;
                }
                let mut attr = cur.properties();
                while let Some(a) = attr {
                    if let Some(ans) = a.ns()
                        && ans.prefix().is_some()
                        && ans.href().as_deref() == Some(XMLNS_NS)
                        && let Some(value) = a.children().and_then(Node::content)
                    {
                        let prefix = a.name();
                        if let Some(ns) = self.get_raw(req, doc, Some(&prefix), Some(&value))? {
                            add_in_scope(req, table, &prefix, ns)?;
                        }
                    }
                    attr = a.next();
                }
            }
            at = cur.parent();
        }
        Ok(())
    }

    /// `php_dom_libxml_reconcile_modern`: a subtree that came from another
    /// document pointed at this mapper's declarations, each old one mapped
    /// to its new one in a table on the stack whose data is laid down at
    /// the first mapping and freed at the end; the `Err` is the memory
    /// limit.
    ///
    /// So an import peaks where php's does, the mapper's own tables and
    /// the walk's alike:
    ///
    /// ```php
    /// $src = Dom\XMLDocument::createFromString('<r xmlns="urn:a" xmlns:b="urn:b"><b:x b:y="1"><c xmlns="urn:c"/></b:x></r>');
    /// $dst = Dom\XMLDocument::createEmpty();
    /// foreach ([1, 2] as $i) {
    ///     memory_reset_peak_usage();
    ///     $m = memory_get_usage();
    ///     $n[] = $dst->importNode($src->documentElement, true);
    ///     echo memory_get_usage() - $m, ' ', memory_get_peak_usage() - $m, "\n";
    /// }
    /// $h = Dom\HTMLDocument::createEmpty();
    /// memory_reset_peak_usage();
    /// $m = memory_get_usage();
    /// $h->adoptNode($n[1]);
    /// echo memory_get_usage() - $m, ' ', memory_get_peak_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 2712 2816
    /// 120 528
    /// 2376 2696
    /// ```
    pub(crate) fn reconcile(
        &mut self,
        req: &mut Request,
        doc: Option<Node>,
        node: Node,
    ) -> Result<(), Thrown> {
        if node.kind() == Kind::Attribute {
            if let Some(ns) = node.ns() {
                let fresh = self.get_raw(req, doc, ns.prefix().as_deref(), ns.href().as_deref())?;
                node.set_ns(fresh);
            }
            return Ok(());
        }
        if node.kind() != Kind::Element
            || (node.children().is_none()
                && node.properties().is_none()
                && node.ns() == node.ns_def())
        {
            return Ok(());
        }
        let mut seen = Table::default();
        let walked = self.reconcile_walk(req, doc, node, &mut seen);
        seen.free(req);
        walked
    }

    fn reconcile_walk(
        &mut self,
        req: &mut Request,
        doc: Option<Node>,
        node: Node,
        seen: &mut Table<(Ns, Option<Ns>)>,
    ) -> Result<(), Thrown> {
        self.reconcile_element(req, doc, node, seen)?;
        let mut at = node.children();
        while let Some(n) = at {
            if n.kind() == Kind::Element {
                self.reconcile_element(req, doc, n, seen)?;
            }
            at = crate::next_in_tree_order(n, node);
        }
        Ok(())
    }

    fn reconcile_element(
        &mut self,
        req: &mut Request,
        doc: Option<Node>,
        element: Node,
        seen: &mut Table<(Ns, Option<Ns>)>,
    ) -> Result<(), Thrown> {
        self.reconcile_single(req, doc, element, seen)?;
        let mut attr = element.properties();
        while let Some(a) = attr {
            self.reconcile_single(req, doc, a, seen)?;
            attr = a.next();
        }
        Ok(())
    }

    /// `php_dom_libxml_reconcile_modern_single_node`: the mapping looked
    /// up, else the mapper asked and the mapping added after.
    fn reconcile_single(
        &mut self,
        req: &mut Request,
        doc: Option<Node>,
        node: Node,
        seen: &mut Table<(Ns, Option<Ns>)>,
    ) -> Result<(), Thrown> {
        let Some(ns) = node.ns() else {
            return Ok(());
        };
        if let Some((_, fresh)) = seen.rows().find(|(old, _)| *old == ns) {
            node.set_ns(*fresh);
            return Ok(());
        }
        let fresh = self.get_raw(req, doc, ns.prefix().as_deref(), ns.href().as_deref())?;
        seen.add(req, (ns, fresh))?;
        node.set_ns(fresh);
        Ok(())
    }
}

/// `zend_hash_str_add_ptr` into the in-scope table: nothing where the
/// prefix is in it, else room made and the key's `zend_string_init`; the
/// `Err` is the memory limit.
fn add_in_scope(
    req: &mut Request,
    table: &mut Table<(Owner<PhpString>, Ns)>,
    prefix: &[u8],
    ns: Ns,
) -> Result<(), Thrown> {
    if table.rows().any(|(key, _)| key.as_bytes() == prefix) {
        return Ok(());
    }
    table.make_room(req)?;
    let key = Handed::Made(prefix).hold(req)?;
    table.push((key, ns));
    Ok(())
}

/// `php_dom_in_scope_ns` from `php_dom_get_in_scope_ns`: the declarations
/// and the `safe_emalloc`'d array php lists them in, given back by
/// [`InScope::free`], `php_dom_in_scope_ns_destroy`.
pub(crate) struct InScope {
    pub(crate) list: Vec<Ns>,
    charge: Emalloc,
}

impl InScope {
    /// `php_dom_in_scope_ns_destroy`: the array's `efree`.
    pub(crate) fn free(self, req: &mut Request) {
        self.charge.free(req);
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

/// `php_dom_get_ns_mapper(intern)`: `call` served with the mapper of
/// `object`'s document and the request beside it; `None` where the object
/// keeps no private data. The mapper reaches no object, so the borrow of
/// the object's libxml half beside the request is never read through
/// while `call` runs.
pub(crate) fn with_mapper<R>(
    req: &mut Request,
    object: PhpObject,
    call: impl FnOnce(&mut Mapper, &mut Request) -> Result<R, Thrown>,
) -> Result<Option<R>, Thrown> {
    let Some(mut obj) = crate::dom_mut(req, object).map(|d| d.obj.borrow()) else {
        return Ok(None);
    };
    match of(&mut obj) {
        Some(private) => call(&mut private.mapper, req).map(Some),
        None => Ok(None),
    }
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
pub(crate) fn ensure_template(
    req: &mut Request,
    object: PhpObject,
    element: Node,
) -> Result<Option<Node>, Thrown> {
    let Some(mut obj) = crate::dom_mut(req, object).map(|d| d.obj.borrow()) else {
        return Ok(None);
    };
    if let Some(found) = obj
        .private_data::<Private>()
        .and_then(|p| p.template(element))
    {
        return Ok(Some(found));
    }
    let Some(fragment) = Node::new_fragment(element.doc()) else {
        return Ok(None);
    };
    fragment.set_parent(Some(element));
    obj.add_ns_tag(element);
    let added = of(&mut obj).map(|private| private.add_template(req, element, fragment));
    match added {
        Some(Ok(())) => Ok(Some(fragment)),
        Some(Err(thrown)) => {
            fragment.free();
            Err(thrown)
        }
        None => {
            fragment.free();
            Ok(None)
        }
    }
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
    use php_ext::test::{Capture, Engine};
    use php_ext::values::Value;

    use super::Private;

    /// `replace_private(object $document): void`: a fresh private data
    /// laid over the one `$document` holds.
    #[php_function]
    fn replace_private(req: &mut Request, document: Value) -> Result<(), Thrown> {
        let Some(object) = document.as_object() else {
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
            .build();
        let served = engine.serve_capture(
            "<?php $d = Dom\\XMLDocument::createFromString('<r/>'); replace_private($d); replace_private($d); echo $d->documentElement->nodeName, \"\\n\";",
            Capture::default(),
        );
        served.assert_clean("r\n");
    }
}
