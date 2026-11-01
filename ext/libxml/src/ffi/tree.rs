//! libxml2's tree as handles: [`Node`] for `xmlNodePtr` and the structs
//! that share its prefix, [`Ns`] for `xmlNsPtr`, and the tree calls php's
//! dom makes over them. A `Node` names a node of a live tree; it is made
//! from a node object's proxy or from a walk inside one native call, and
//! the field a `Node` reads is the one its kind has, as php reads it after
//! the same test of `->type`.

use std::ffi::{c_char, c_int, c_void};
use std::marker::PhantomData;
use std::ptr::{self, NonNull};

use php_ext::request::Request;

use super::{Alloc, Exported, Library, NodeObject, SharedDoc, Tree, cstr, sys};

/// A NUL-terminated copy or null, for a C argument php passes as `NULL`.
fn opt_ptr(bytes: Option<&Vec<u8>>) -> *const u8 {
    bytes.map_or(ptr::null(), Vec::as_ptr)
}

/// The bytes of a string libxml owns, copied.
fn owned(p: *const u8) -> Vec<u8> {
    // SAFETY: libxml's strings are NUL-terminated and live while the
    // node that holds them does, which outlives this copy.
    unsafe { Alloc::bytes(p) }.to_vec()
}

/// A string libxml allocated for the caller, taken and freed.
fn taken(p: *mut u8) -> Option<Vec<u8>> {
    // SAFETY: an allocation libxml handed over, freed once here.
    unsafe { Alloc::take(p) }
}

/// `xmlElementType`, decoded once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Element,
    Attribute,
    Text,
    CData,
    EntityRef,
    Entity,
    Pi,
    Comment,
    Document,
    DocumentType,
    Fragment,
    Notation,
    HtmlDocument,
    Dtd,
    ElementDecl,
    AttributeDecl,
    EntityDecl,
    NamespaceDecl,
    XIncludeStart,
    XIncludeEnd,
}

impl Kind {
    fn of(raw: c_int) -> Kind {
        match raw {
            sys::XML_ELEMENT_NODE => Kind::Element,
            sys::XML_ATTRIBUTE_NODE => Kind::Attribute,
            sys::XML_TEXT_NODE => Kind::Text,
            sys::XML_CDATA_SECTION_NODE => Kind::CData,
            sys::XML_ENTITY_REF_NODE => Kind::EntityRef,
            sys::XML_ENTITY_NODE => Kind::Entity,
            sys::XML_PI_NODE => Kind::Pi,
            sys::XML_COMMENT_NODE => Kind::Comment,
            sys::XML_DOCUMENT_NODE => Kind::Document,
            sys::XML_DOCUMENT_TYPE_NODE => Kind::DocumentType,
            sys::XML_DOCUMENT_FRAG_NODE => Kind::Fragment,
            sys::XML_NOTATION_NODE => Kind::Notation,
            sys::XML_HTML_DOCUMENT_NODE => Kind::HtmlDocument,
            sys::XML_DTD_NODE => Kind::Dtd,
            sys::XML_ELEMENT_DECL => Kind::ElementDecl,
            sys::XML_ATTRIBUTE_DECL => Kind::AttributeDecl,
            sys::XML_ENTITY_DECL => Kind::EntityDecl,
            sys::XML_NAMESPACE_DECL => Kind::NamespaceDecl,
            sys::XML_XINCLUDE_START => Kind::XIncludeStart,
            _ => Kind::XIncludeEnd,
        }
    }

    /// The number `nodeType` and the `XML_*_NODE` constants carry.
    pub fn number(self) -> i64 {
        i64::from(match self {
            Kind::Element => sys::XML_ELEMENT_NODE,
            Kind::Attribute => sys::XML_ATTRIBUTE_NODE,
            Kind::Text => sys::XML_TEXT_NODE,
            Kind::CData => sys::XML_CDATA_SECTION_NODE,
            Kind::EntityRef => sys::XML_ENTITY_REF_NODE,
            Kind::Entity => sys::XML_ENTITY_NODE,
            Kind::Pi => sys::XML_PI_NODE,
            Kind::Comment => sys::XML_COMMENT_NODE,
            Kind::Document => sys::XML_DOCUMENT_NODE,
            Kind::DocumentType => sys::XML_DOCUMENT_TYPE_NODE,
            Kind::Fragment => sys::XML_DOCUMENT_FRAG_NODE,
            Kind::Notation => sys::XML_NOTATION_NODE,
            Kind::HtmlDocument => sys::XML_HTML_DOCUMENT_NODE,
            Kind::Dtd => sys::XML_DTD_NODE,
            Kind::ElementDecl => sys::XML_ELEMENT_DECL,
            Kind::AttributeDecl => sys::XML_ATTRIBUTE_DECL,
            Kind::EntityDecl => sys::XML_ENTITY_DECL,
            Kind::NamespaceDecl => sys::XML_NAMESPACE_DECL,
            Kind::XIncludeStart => sys::XML_XINCLUDE_START,
            Kind::XIncludeEnd => sys::XML_XINCLUDE_END,
        })
    }

    fn raw(self) -> c_int {
        c_int::try_from(self.number()).unwrap_or(sys::XML_ELEMENT_NODE)
    }

    /// Whether the node is a document, XML or HTML.
    pub fn is_document(self) -> bool {
        matches!(self, Kind::Document | Kind::HtmlDocument)
    }

    /// Whether the struct behind the node is an `xmlNode` in full, whose
    /// `ns`, `content`, `properties` and `nsDef` are there to read.
    fn has_node_tail(self) -> bool {
        matches!(
            self,
            Kind::Element
                | Kind::Text
                | Kind::CData
                | Kind::EntityRef
                | Kind::Pi
                | Kind::Comment
                | Kind::Fragment
                | Kind::NamespaceDecl
                | Kind::XIncludeStart
                | Kind::XIncludeEnd
        )
    }
}

/// `xmlNodePtr`, and an `xmlAttrPtr`, `xmlDocPtr`, `xmlDtdPtr` or
/// `xmlEntityPtr` read through the prefix they share with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Node(NonNull<sys::xmlNode>);

/// `xmlNsPtr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ns(NonNull<sys::xmlNs>);

impl Node {
    pub(crate) fn from_raw(p: *mut sys::xmlNode) -> Option<Node> {
        NonNull::new(p).map(Node)
    }

    pub(crate) fn raw(self) -> *mut sys::xmlNode {
        self.0.as_ptr()
    }

    /// The `xmlNodePtr` itself, as `php_libxml.h` hands its nodes to the
    /// extensions that include it and link a library over libxml2 of their
    /// own: ext/xsl's libxslt reads a document through it.
    #[must_use]
    pub fn as_ptr(self) -> *mut c_void {
        self.0.as_ptr().cast()
    }

    /// A `Node` for the `xmlNodePtr` a library over libxml2 answered, as
    /// libxslt answers the document a transformation made; `None` for its
    /// null.
    ///
    /// # Safety
    /// `p` is null or a live node of libxml2's, which stays live while the
    /// `Node` names it.
    #[must_use]
    pub unsafe fn from_ptr(p: *mut c_void) -> Option<Node> {
        Node::from_raw(p.cast())
    }

    fn read<T>(self, f: impl FnOnce(&sys::xmlNode) -> T) -> T {
        // SAFETY: a handle names a live node (the module's invariant); the
        // borrow ends with `f`, before anything else touches the node.
        f(unsafe { &*self.0.as_ptr() })
    }

    fn write(self, f: impl FnOnce(&mut sys::xmlNode)) {
        // SAFETY: as in `read`; no other reference into the node is held
        // across `f`.
        f(unsafe { &mut *self.0.as_ptr() });
    }

    fn attr<T>(self, f: impl FnOnce(&mut sys::xmlAttr) -> T) -> Option<T> {
        if self.kind() != Kind::Attribute {
            return None;
        }
        // SAFETY: an attribute node is an `xmlAttr`, live by the invariant.
        Some(f(unsafe { &mut *self.0.as_ptr().cast::<sys::xmlAttr>() }))
    }

    pub fn kind(self) -> Kind {
        self.read(|n| Kind::of(n.type_))
    }

    /// `node->type = kind`, the retyping php does around one libxml call.
    pub fn set_kind(self, kind: Kind) {
        self.write(|n| n.type_ = kind.raw());
    }

    /// `node->name`, empty where it is null.
    pub fn name(self) -> Vec<u8> {
        owned(self.read(|n| n.name))
    }

    pub fn has_name(self) -> bool {
        !self.read(|n| n.name).is_null()
    }

    pub fn children(self) -> Option<Node> {
        Node::from_raw(self.read(|n| n.children))
    }
    pub fn last(self) -> Option<Node> {
        Node::from_raw(self.read(|n| n.last))
    }
    pub fn parent(self) -> Option<Node> {
        Node::from_raw(self.read(|n| n.parent))
    }
    pub fn next(self) -> Option<Node> {
        Node::from_raw(self.read(|n| n.next))
    }
    pub fn prev(self) -> Option<Node> {
        Node::from_raw(self.read(|n| n.prev))
    }
    pub fn set_children(self, to: Option<Node>) {
        self.write(|n| n.children = to.map_or(ptr::null_mut(), Node::raw));
    }
    pub fn set_last(self, to: Option<Node>) {
        self.write(|n| n.last = to.map_or(ptr::null_mut(), Node::raw));
    }
    pub fn set_parent(self, to: Option<Node>) {
        self.write(|n| n.parent = to.map_or(ptr::null_mut(), Node::raw));
    }
    pub fn set_next(self, to: Option<Node>) {
        self.write(|n| n.next = to.map_or(ptr::null_mut(), Node::raw));
    }
    pub fn set_prev(self, to: Option<Node>) {
        self.write(|n| n.prev = to.map_or(ptr::null_mut(), Node::raw));
    }

    /// `node->doc`, as the document node.
    pub fn doc(self) -> Option<Node> {
        Node::from_raw(self.read(|n| n.doc).cast())
    }

    /// `node->ns`: an element's, an attribute's, a namespace node's.
    pub fn ns(self) -> Option<Ns> {
        let kind = self.kind();
        if !(kind.has_node_tail() || kind == Kind::Attribute) {
            return None;
        }
        Ns::from_raw(self.read(|n| n.ns))
    }

    /// `node->ns = ns` for an element or an attribute, no reconciliation.
    pub fn set_ns(self, ns: Option<Ns>) {
        let kind = self.kind();
        if kind.has_node_tail() || kind == Kind::Attribute {
            self.write(|n| n.ns = ns.map_or(ptr::null_mut(), Ns::raw));
        }
    }

    /// `node->content`: text, CDATA, comment, PI, entity reference and
    /// entity declaration content; `None` for null or another kind.
    pub fn content(self) -> Option<Vec<u8>> {
        let kind = self.kind();
        if !(kind.has_node_tail() || matches!(kind, Kind::EntityDecl | Kind::Notation)) {
            return None;
        }
        let p = self.read(|n| n.content);
        (!p.is_null()).then(|| owned(p))
    }

    /// `node->content = value`, as `dom_entity_reference_fetch_and_sync_declaration`
    /// points a reference at its declaration's content; nothing is freed.
    pub fn alias_content_of(self, entity: Option<Node>) {
        if self.kind() != Kind::EntityRef {
            return;
        }
        let content = entity.map_or(ptr::null_mut(), |e| e.read(|n| n.content));
        self.write(|n| n.content = content);
    }

    /// An element's attribute list.
    pub fn properties(self) -> Option<Node> {
        if self.kind() != Kind::Element {
            return None;
        }
        Node::from_raw(self.read(|n| n.properties).cast())
    }

    pub fn set_properties(self, to: Option<Node>) {
        if self.kind() == Kind::Element {
            self.write(|n| n.properties = to.map_or(ptr::null_mut(), |a| a.raw().cast()));
        }
    }

    /// An element's namespace declarations.
    pub fn ns_def(self) -> Option<Ns> {
        if self.kind() != Kind::Element {
            return None;
        }
        Ns::from_raw(self.read(|n| n.nsDef))
    }

    pub fn set_ns_def(self, to: Option<Ns>) {
        if self.kind() == Kind::Element {
            self.write(|n| n.nsDef = to.map_or(ptr::null_mut(), Ns::raw));
        }
    }

    /// `attr->atype`.
    pub fn atype(self) -> i32 {
        self.attr(|a| a.atype).unwrap_or(0)
    }

    pub fn set_atype(self, atype: i32) {
        let _ = self.attr(|a| a.atype = atype);
    }

    /// Whether an object stands for the node, `node->_private != NULL`.
    pub fn is_named(self) -> bool {
        !self.read(|n| n.private).is_null()
    }

    /// The object standing for the node, `php_dom_object_get_data`.
    pub fn object(self) -> Option<php_ext::values::PhpObject> {
        // SAFETY: a live node.
        unsafe { Tree::object_of(self.raw()) }
    }

    /// The libxml half of the object standing for the node.
    pub fn wrapper(self) -> Option<super::Borrowed> {
        // SAFETY: a live node; the borrow is used and dropped by the caller
        // before any other reaches the same object.
        unsafe { Tree::borrowed(self.raw()) }
    }

    // ---- xmlDoc, through the prefix a document node shares

    fn doc_read<T>(self, f: impl FnOnce(&sys::xmlDoc) -> T) -> Option<T> {
        if !self.kind().is_document() {
            return None;
        }
        // SAFETY: a document node is an `xmlDoc`, live by the invariant.
        Some(f(unsafe { &*self.0.as_ptr().cast::<sys::xmlDoc>() }))
    }

    fn doc_write(self, f: impl FnOnce(&mut sys::xmlDoc)) {
        if self.kind().is_document() {
            // SAFETY: as in `doc_read`; no other reference is held.
            f(unsafe { &mut *self.0.as_ptr().cast::<sys::xmlDoc>() });
        }
    }

    pub fn doc_encoding(self) -> Option<Vec<u8>> {
        self.doc_read(|d| d.encoding)
            .filter(|p| !p.is_null())
            .map(owned)
    }
    pub fn doc_version(self) -> Option<Vec<u8>> {
        self.doc_read(|d| d.version)
            .filter(|p| !p.is_null())
            .map(owned)
    }
    pub fn doc_url(self) -> Option<Vec<u8>> {
        self.doc_read(|d| d.URL).filter(|p| !p.is_null()).map(owned)
    }
    pub fn doc_standalone(self) -> i32 {
        self.doc_read(|d| d.standalone).unwrap_or(0)
    }
    pub fn set_doc_standalone(self, value: i32) {
        self.doc_write(|d| d.standalone = value);
    }

    /// Replace one of the document's strings, freeing the old one, as php
    /// does for `encoding`, `version` and `URL`.
    pub fn set_doc_string(self, field: DocString, value: Option<&[u8]>) {
        let fresh = value.map_or(ptr::null_mut(), |v| {
            let c = cstr(v);
            // SAFETY: a NUL-terminated copy libxml duplicates.
            unsafe { sys::xmlStrdup(c.as_ptr()) }
        });
        self.doc_write(|d| {
            let slot = match field {
                DocString::Encoding => &mut d.encoding,
                DocString::Version => &mut d.version,
                DocString::Url => &mut d.URL,
            };
            let old = std::mem::replace(slot, fresh);
            // SAFETY: the document owned the old string.
            unsafe { Alloc::free(old.cast_mut().cast()) };
        });
    }

    /// `doc->intSubset`, what `xmlGetIntSubset` answers.
    pub fn int_subset(self) -> Option<Node> {
        // SAFETY: a live document read as one.
        Node::from_raw(unsafe { sys::xmlGetIntSubset(self.raw().cast()) }.cast())
    }

    pub fn set_int_subset(self, dtd: Option<Node>) {
        self.doc_write(|d| d.intSubset = dtd.map_or(ptr::null_mut(), |n| n.raw().cast()));
    }

    /// `xmlDocGetRootElement`.
    pub fn root(self) -> Option<Node> {
        // SAFETY: a live node read as a document; libxml answers null for
        // one with no element child.
        Node::from_raw(unsafe { sys::xmlDocGetRootElement(self.raw().cast()) })
    }

    // ---- xmlDtd and xmlEntity, through the prefix they share

    fn dtd_read<T>(self, f: impl FnOnce(&sys::xmlDtd) -> T) -> Option<T> {
        if !matches!(self.kind(), Kind::Dtd | Kind::DocumentType) {
            return None;
        }
        // SAFETY: a DTD node is an `xmlDtd`, live by the invariant.
        Some(f(unsafe { &*self.0.as_ptr().cast::<sys::xmlDtd>() }))
    }

    pub fn dtd_external_id(self) -> Option<Vec<u8>> {
        self.dtd_read(|d| d.ExternalID)
            .filter(|p| !p.is_null())
            .map(owned)
    }
    pub fn dtd_system_id(self) -> Option<Vec<u8>> {
        self.dtd_read(|d| d.SystemID)
            .filter(|p| !p.is_null())
            .map(owned)
    }

    /// A DTD's entity or notation table.
    pub fn dtd_table(self, which: DtdTable) -> Option<HashTable> {
        self.dtd_read(|d| match which {
            DtdTable::Entities => d.entities,
            DtdTable::Notations => d.notations,
        })
        .and_then(|t| NonNull::new(t).map(HashTable))
    }

    fn entity_read<T>(self, f: impl FnOnce(&sys::xmlEntity) -> T) -> Option<T> {
        if !matches!(self.kind(), Kind::EntityDecl | Kind::Notation) {
            return None;
        }
        // SAFETY: an entity declaration, or `create_notation`'s entity in
        // disguise, is an `xmlEntity`, live by the invariant.
        Some(f(unsafe { &*self.0.as_ptr().cast::<sys::xmlEntity>() }))
    }

    pub fn entity_type(self) -> i32 {
        self.entity_read(|e| e.etype).unwrap_or(0)
    }
    pub fn entity_external_id(self) -> Option<Vec<u8>> {
        self.entity_read(|e| e.ExternalID)
            .filter(|p| !p.is_null())
            .map(owned)
    }
    pub fn entity_system_id(self) -> Option<Vec<u8>> {
        self.entity_read(|e| e.SystemID)
            .filter(|p| !p.is_null())
            .map(owned)
    }
    pub fn entity_content(self) -> Option<Vec<u8>> {
        self.entity_read(|e| e.content.cast_const())
            .filter(|p| !p.is_null())
            .map(owned)
    }

    /// `((xmlAttributePtr) node)->defaultValue`: the value an attribute's
    /// DTD declaration gives an element that does not carry it; none for
    /// any other node.
    pub fn attribute_decl_default(self) -> Option<Vec<u8>> {
        if self.kind() != Kind::AttributeDecl {
            return None;
        }
        // SAFETY: an attribute declaration is an `xmlAttribute`, live by
        // the invariant.
        let p = unsafe { &*self.0.as_ptr().cast::<sys::xmlAttribute>() }.defaultValue;
        (!p.is_null()).then(|| owned(p))
    }

    // ---- libxml2's tree calls

    /// `xmlUnlinkNode`.
    pub fn unlink(self) {
        // SAFETY: a live node.
        unsafe { sys::xmlUnlinkNode(self.raw()) };
    }

    /// `xmlAddChild`: the node added, or the text it merged into.
    pub fn add_child(self, child: Node) -> Option<Node> {
        // SAFETY: two live nodes.
        Node::from_raw(unsafe { sys::xmlAddChild(self.raw(), child.raw()) })
    }

    /// `xmlAddChildList`.
    pub fn add_child_list(self, list: Node) -> Option<Node> {
        // SAFETY: a live node and a live list.
        Node::from_raw(unsafe { sys::xmlAddChildList(self.raw(), list.raw()) })
    }

    /// `xmlAddPrevSibling`.
    pub fn add_prev_sibling(self, elem: Node) -> Option<Node> {
        // SAFETY: two live nodes.
        Node::from_raw(unsafe { sys::xmlAddPrevSibling(self.raw(), elem.raw()) })
    }

    /// `xmlAddNextSibling`.
    pub fn add_next_sibling(self, elem: Node) -> Option<Node> {
        // SAFETY: two live nodes.
        Node::from_raw(unsafe { sys::xmlAddNextSibling(self.raw(), elem.raw()) })
    }

    /// `xmlReplaceNode(old, cur)`.
    pub fn replace_with(self, cur: Node) {
        // SAFETY: two live nodes.
        unsafe { sys::xmlReplaceNode(self.raw(), cur.raw()) };
    }

    /// `xmlFreeNode`, of a node nothing names and no tree holds.
    pub fn free(self) {
        if self.kind() == Kind::Attribute {
            // SAFETY: an unlinked attribute, freed once.
            unsafe { sys::xmlFreeProp(self.raw().cast()) };
        } else {
            // SAFETY: an unlinked node, freed once.
            unsafe { sys::xmlFreeNode(self.raw()) };
        }
    }

    /// `xmlFree(node)`: the struct alone, as php frees an emptied
    /// temporary fragment whose children were moved away.
    pub fn free_shell(self) {
        // SAFETY: a node with no children, freed once.
        unsafe { Alloc::free(self.raw().cast()) };
    }

    /// `xmlFreeDoc(doc)`: a document nothing names, with its tree.
    pub fn free_document(self) {
        // SAFETY: a live document nothing else frees, freed once.
        unsafe { sys::xmlFreeDoc(self.raw().cast()) };
    }

    /// `php_libxml_node_free_resource`.
    pub fn free_resource(self, req: &mut Request) {
        // SAFETY: a live node nothing names any more.
        unsafe { Tree::free_resource(self.raw(), Some(req)) };
    }

    /// `php_libxml_node_free_list` over the node's children.
    pub fn free_list(req: &mut Request, first: Option<Node>) {
        // SAFETY: a live list nothing else frees.
        unsafe { Tree::free_list(first.map_or(ptr::null_mut(), Node::raw), Some(req)) };
    }

    /// `xmlReconciliateNs(doc, node)`.
    pub fn reconciliate_ns(self, doc: Option<Node>) {
        // SAFETY: a live tree and its document or none.
        unsafe {
            sys::xmlReconciliateNs(doc.map_or(ptr::null_mut(), |d| d.raw().cast()), self.raw())
        };
    }

    /// `xmlSetTreeDoc`.
    pub fn set_tree_doc(self, doc: Option<Node>) {
        // SAFETY: a live tree and a live document or none.
        unsafe { sys::xmlSetTreeDoc(self.raw(), doc.map_or(ptr::null_mut(), |d| d.raw().cast())) };
    }

    /// `xmlDocCopyNode(node, doc, recursive)`.
    pub fn copy(self, doc: Option<Node>, recursive: i32) -> Option<Node> {
        // SAFETY: a live node and document or none.
        Node::from_raw(unsafe {
            sys::xmlDocCopyNode(
                self.raw(),
                doc.map_or(ptr::null_mut(), |d| d.raw().cast()),
                recursive,
            )
        })
    }

    /// `xmlCopyDtd`, then `xmlSetTreeDoc` onto `doc`.
    pub fn copy_dtd(self, doc: Option<Node>) -> Option<Node> {
        // SAFETY: a live DTD.
        let copy = Node::from_raw(unsafe { sys::xmlCopyDtd(self.raw().cast()) }.cast())?;
        copy.set_tree_doc(doc);
        Some(copy)
    }

    /// `xmlNodeGetContent`.
    pub fn get_content(self) -> Option<Vec<u8>> {
        // SAFETY: a live node.
        taken(unsafe { sys::xmlNodeGetContent(self.raw()) })
    }

    /// `xmlNodeSetContent`.
    pub fn set_content(self, content: &[u8]) {
        let c = cstr(content);
        // SAFETY: a live node and a NUL-terminated string.
        unsafe { sys::xmlNodeSetContent(self.raw(), c.as_ptr()) };
    }

    /// `xmlNodeSetContentLen`, the bytes as they are.
    pub fn set_content_len(self, content: &[u8]) {
        let len = c_int::try_from(content.len()).unwrap_or(c_int::MAX);
        // SAFETY: a live node and `len` readable bytes.
        unsafe { sys::xmlNodeSetContentLen(self.raw(), content.as_ptr(), len) };
    }

    /// `xmlNodeAddContent`.
    pub fn add_content(self, content: &[u8]) {
        let c = cstr(content);
        // SAFETY: a live node and a NUL-terminated string.
        unsafe { sys::xmlNodeAddContent(self.raw(), c.as_ptr()) };
    }

    /// `xmlTextConcat`.
    pub fn text_concat(self, content: &[u8]) {
        let len = c_int::try_from(content.len()).unwrap_or(c_int::MAX);
        // SAFETY: a live node and `len` readable bytes.
        unsafe { sys::xmlTextConcat(self.raw(), content.as_ptr(), len) };
    }

    /// `xmlNodeListGetString(doc, list, 1)` over the node's children.
    pub fn list_string(self) -> Option<Vec<u8>> {
        let doc = self.doc().map_or(ptr::null_mut(), |d| d.raw().cast());
        let list = self.children().map_or(ptr::null_mut(), Node::raw);
        // SAFETY: a live list and its document.
        taken(unsafe { sys::xmlNodeListGetString(doc, list, 1) })
    }

    /// `xmlNodeGetBase(node->doc, node)`.
    pub fn base(self) -> Option<Vec<u8>> {
        let doc = self.doc().map_or(ptr::null_mut(), |d| d.raw().cast());
        // SAFETY: a live node and its document.
        taken(unsafe { sys::xmlNodeGetBase(doc, self.raw()) })
    }

    /// `xmlGetNodePath`.
    pub fn path(self) -> Option<Vec<u8>> {
        // SAFETY: a live node.
        taken(unsafe { sys::xmlGetNodePath(self.raw()) })
    }

    /// `xmlGetLineNo`.
    pub fn line_no(self) -> i64 {
        // SAFETY: a live node.
        unsafe { sys::xmlGetLineNo(self.raw()) }
    }

    /// `xmlIsBlankNode`.
    pub fn is_blank(self) -> bool {
        // SAFETY: a live node.
        unsafe { sys::xmlIsBlankNode(self.raw()) != 0 }
    }

    /// `xmlHasProp`.
    pub fn has_prop(self, name: &[u8]) -> Option<Node> {
        let n = cstr(name);
        // SAFETY: a live node and a NUL-terminated name.
        Node::from_raw(unsafe { sys::xmlHasProp(self.raw(), n.as_ptr()) }.cast())
    }

    /// `xmlHasNsProp`, `ns` null for "no namespace".
    pub fn has_ns_prop(self, name: &[u8], ns: Option<&[u8]>) -> Option<Node> {
        let n = cstr(name);
        let u = ns.map(cstr);
        // SAFETY: a live node and NUL-terminated strings or null.
        Node::from_raw(
            unsafe { sys::xmlHasNsProp(self.raw(), n.as_ptr(), opt_ptr(u.as_ref())) }.cast(),
        )
    }

    /// `xmlGetNsProp`.
    pub fn get_ns_prop(self, name: &[u8], ns: Option<&[u8]>) -> Option<Vec<u8>> {
        let n = cstr(name);
        let u = ns.map(cstr);
        // SAFETY: as above.
        taken(unsafe { sys::xmlGetNsProp(self.raw(), n.as_ptr(), opt_ptr(u.as_ref())) })
    }

    /// `xmlGetNoNsProp`.
    pub fn get_no_ns_prop(self, name: &[u8]) -> Option<Vec<u8>> {
        let n = cstr(name);
        // SAFETY: as above.
        taken(unsafe { sys::xmlGetNoNsProp(self.raw(), n.as_ptr()) })
    }

    /// `xmlSetProp`.
    pub fn set_prop(self, name: &[u8], value: &[u8]) -> Option<Node> {
        let n = cstr(name);
        let v = cstr(value);
        // SAFETY: a live element and NUL-terminated strings.
        Node::from_raw(unsafe { sys::xmlSetProp(self.raw(), n.as_ptr(), v.as_ptr()) }.cast())
    }

    /// `xmlSetNsProp`.
    pub fn set_ns_prop(self, ns: Option<Ns>, name: &[u8], value: &[u8]) -> Option<Node> {
        let n = cstr(name);
        let v = cstr(value);
        let ns = ns.map_or(ptr::null_mut(), Ns::raw);
        // SAFETY: as above.
        Node::from_raw(unsafe { sys::xmlSetNsProp(self.raw(), ns, n.as_ptr(), v.as_ptr()) }.cast())
    }

    /// `xmlSetNsProp(node, ns, name, NULL)`: the attribute made or reset
    /// with no children, as php's modern `toggleAttribute` makes it.
    pub fn set_ns_prop_bare(self, ns: Option<Ns>, name: &[u8]) -> Option<Node> {
        let n = cstr(name);
        let ns = ns.map_or(ptr::null_mut(), Ns::raw);
        // SAFETY: a live element, a namespace or null, a NUL-terminated
        // name and a null value, which libxml takes as none.
        Node::from_raw(unsafe { sys::xmlSetNsProp(self.raw(), ns, n.as_ptr(), ptr::null()) }.cast())
    }

    /// `xmlNodeSetName`: the name replaced, interned in the document's
    /// dictionary where it has one, the old name freed unless the
    /// dictionary owns it; php's `Dom\Element::rename` does the same by
    /// hand.
    pub fn set_name(self, name: &[u8]) {
        let n = cstr(name);
        // SAFETY: a live node and a NUL-terminated name, copied by libxml.
        unsafe { sys::xmlNodeSetName(self.raw(), n.as_ptr()) };
    }

    /// `xmlSearchNs(node->doc, node, prefix)`.
    pub fn search_ns(self, prefix: Option<&[u8]>) -> Option<Ns> {
        let p = prefix.map(cstr);
        let doc = self.doc().map_or(ptr::null_mut(), |d| d.raw().cast());
        // SAFETY: a live node, its document and a prefix or null.
        Ns::from_raw(unsafe { sys::xmlSearchNs(doc, self.raw(), opt_ptr(p.as_ref())) })
    }

    /// `xmlSearchNsByHref(doc, node, href)`.
    pub fn search_ns_by_href(self, doc: Option<Node>, href: &[u8]) -> Option<Ns> {
        let h = cstr(href);
        let doc = doc.map_or(ptr::null_mut(), |d| d.raw().cast());
        // SAFETY: a live node, a document or none, a NUL-terminated href.
        Ns::from_raw(unsafe { sys::xmlSearchNsByHref(doc, self.raw(), h.as_ptr()) })
    }

    /// `xmlNewNs(node, href, prefix)`: a declaration on the element, or
    /// none where the prefix is taken or reserved.
    pub fn new_ns(self, href: &[u8], prefix: Option<&[u8]>) -> Option<Ns> {
        Ns::new(Some(self), Some(href), prefix)
    }

    /// `xmlSetNs`.
    pub fn set_ns_call(self, ns: Option<Ns>) {
        // SAFETY: a live node and a namespace or null.
        unsafe { sys::xmlSetNs(self.raw(), ns.map_or(ptr::null_mut(), Ns::raw)) };
    }

    /// `xmlRemoveID(node->doc, attr)`.
    pub fn remove_id(self) {
        let Some(doc) = self.doc() else {
            return;
        };
        // SAFETY: a live attribute and its document.
        unsafe { sys::xmlRemoveID(doc.raw().cast(), self.raw().cast()) };
    }

    /// `xmlDocSetRootElement(doc, root)`.
    pub fn set_root(self, root: Node) {
        // SAFETY: a live document and element.
        unsafe { sys::xmlDocSetRootElement(self.raw().cast(), root.raw()) };
    }

    /// `xmlGetID(doc, id)`'s attribute.
    pub fn get_id(self, id: &[u8]) -> Option<Node> {
        let i = cstr(id);
        // SAFETY: a live document and a NUL-terminated id.
        Node::from_raw(unsafe { sys::xmlGetID(self.raw().cast(), i.as_ptr()) }.cast())
    }

    /// `xmlGetDocEntity(node->doc, node->name)`.
    pub fn doc_entity(self) -> Option<Node> {
        let doc = self.doc().map_or(ptr::null_mut(), |d| d.raw().cast());
        let name = self.read(|n| n.name);
        // SAFETY: a live document or none and the node's own name.
        Node::from_raw(unsafe { sys::xmlGetDocEntity(doc, name) }.cast())
    }

    /// `xmlDOMWrapAdoptNode(NULL, node->doc, node, doc, NULL, 0)`.
    pub fn adopt_into(self, doc: Node) -> bool {
        let old = self.doc().map_or(ptr::null_mut(), |d| d.raw().cast());
        // SAFETY: a live node, its document and the destination document.
        unsafe {
            sys::xmlDOMWrapAdoptNode(
                ptr::null_mut(),
                old,
                self.raw(),
                doc.raw().cast(),
                ptr::null_mut(),
                0,
            ) == 0
        }
    }

    /// The node's name and content moved off `old`'s dictionary before it
    /// joins `new`, php's `libxml_fixup_name_and_content` for a library
    /// before 2.13, so the strings outlive the old document.
    pub fn undict(self, old: Option<Node>, new: Node) {
        let src = old.map_or(ptr::null_mut(), Node::dict);
        if src.is_null() {
            return;
        }
        let dst = new.dict();
        let owns = |s: *const u8| -> bool {
            // SAFETY: a live dictionary and a string or null.
            !s.is_null() && unsafe { sys::xmlDictOwns(src, s) } == 1
        };
        let name = self.read(|n| n.name);
        if owns(name) {
            let copy = if dst.is_null() {
                // SAFETY: a dictionary string, duplicated.
                unsafe { sys::xmlStrdup(name) }.cast_const()
            } else {
                // SAFETY: a live dictionary and a NUL-terminated string.
                unsafe { sys::xmlDictLookup(dst, name, -1) }
            };
            self.write(|n| n.name = copy);
        }
        if self.kind().has_node_tail() {
            let content = self.read(|n| n.content);
            if owns(content) {
                // SAFETY: a dictionary string, duplicated.
                let copy = unsafe { sys::xmlStrdup(content) };
                self.write(|n| n.content = copy);
            }
        }
    }

    /// The document's dictionary, for [`Node::undict`].
    fn dict(self) -> *mut c_void {
        self.doc_read(|d| d.dict).unwrap_or(ptr::null_mut())
    }

    /// `php_libxml_attr_value`: an attribute's value.
    pub fn attr_value(self) -> Vec<u8> {
        if self.kind() != Kind::Attribute {
            return Vec::new();
        }
        // SAFETY: a live attribute.
        unsafe { Tree::attr_value(self.raw().cast()) }
    }

    /// `php_libxml_invalidate_node_list_cache_from_doc` over the node's
    /// document.
    pub fn invalidate_document_lists(self) {
        if let Some(doc) = self.doc() {
            // SAFETY: a live document.
            unsafe { Tree::invalidate_from_doc(doc.raw().cast()) };
        }
    }

    /// `node->line`.
    pub fn line(self) -> u16 {
        self.read(|n| n.line)
    }

    /// Set `node->line`, as php's HTML5 bridge carries lexbor's line over;
    /// a text node past 65,534 keeps the whole line in `psvi`, where
    /// `xmlGetLineNo` reads it.
    pub fn set_line(self, line: usize) {
        match u16::try_from(line) {
            Ok(short) if short < u16::MAX => self.write(|n| n.line = short),
            _ => {
                let text = self.kind() == Kind::Text;
                self.write(|n| {
                    n.line = u16::MAX;
                    if text {
                        n.psvi = std::ptr::without_provenance_mut(line);
                    }
                });
            }
        }
    }

    /// `xmlAddID(NULL, doc, value, attr)`: the attribute registered as the
    /// document's ID `value`; `false` where the ID is taken already.
    pub fn add_id(self, doc: Node, value: &[u8]) -> bool {
        let v = cstr(value);
        // SAFETY: a live document and one of its attributes, and a
        // NUL-terminated value the table copies.
        !unsafe {
            sys::xmlAddID(
                ptr::null_mut(),
                doc.raw().cast(),
                v.as_ptr(),
                self.raw().cast(),
            )
        }
        .is_null()
    }
}

/// Which of a document's strings [`Node::set_doc_string`] replaces.
#[derive(Debug, Clone, Copy)]
pub enum DocString {
    Encoding,
    Version,
    Url,
}

/// Which of a DTD's tables [`Node::dtd_table`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtdTable {
    Entities,
    Notations,
}

/// `xmlHashTablePtr`, a DTD's entity or notation table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashTable(NonNull<c_void>);

impl HashTable {
    /// Whether the table holds nothing.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// `xmlHashSize`.
    pub fn len(self) -> usize {
        // SAFETY: a live table of a live DTD.
        usize::try_from(unsafe { sys::xmlHashSize(self.0.as_ptr()) }).unwrap_or(0)
    }

    /// The entity at `index` in `xmlHashScan`'s order, what
    /// `php_dom_libxml_hash_iter` walks to.
    pub fn entity_at(self, index: usize) -> Option<Node> {
        self.entries().get(index).copied().and_then(entity_node)
    }

    /// The entity named, `xmlHashLookup`.
    pub fn entity(self, name: &[u8]) -> Option<Node> {
        entity_node(self.lookup(name))
    }

    /// A fresh notation node of the entry at `index`, as `create_notation`
    /// makes one for each read: the caller's to own.
    pub fn notation_at(self, index: usize) -> Option<Node> {
        self.entries().get(index).copied().and_then(notation_node)
    }

    /// A fresh notation node of the entry named.
    pub fn notation(self, name: &[u8]) -> Option<Node> {
        notation_node(self.lookup(name))
    }

    /// Every entry, in `xmlHashScan`'s order.
    fn entries(self) -> Vec<*mut c_void> {
        unsafe extern "C" fn push(payload: *mut c_void, data: *mut c_void, _name: *const u8) {
            // SAFETY: `entries` hands its own `Vec`, live for the scan.
            let into = unsafe { &mut *data.cast::<Vec<*mut c_void>>() };
            into.push(payload);
        }
        let mut out: Vec<*mut c_void> = Vec::new();
        // SAFETY: a live table; `out` outlives the scan.
        unsafe { sys::xmlHashScan(self.0.as_ptr(), push, ptr::from_mut(&mut out).cast()) };
        out
    }

    /// `xmlHashLookup(table, name)`.
    fn lookup(self, name: &[u8]) -> *mut c_void {
        let n = cstr(name);
        // SAFETY: a live table and a NUL-terminated name.
        unsafe { sys::xmlHashLookup(self.0.as_ptr(), n.as_ptr()) }
    }
}

/// An entity table's entry as the node it is.
fn entity_node(entry: *mut c_void) -> Option<Node> {
    Node::from_raw(entry.cast())
}

/// `xmlNotation`, which the binding reads as three strings.
#[repr(C)]
struct Notation {
    name: *const u8,
    public_id: *const u8,
    system_id: *const u8,
}

/// `create_notation(notation->name, notation->PublicID, notation->SystemID)`:
/// an `xmlEntity` in disguise, typed `XML_NOTATION_NODE`, owning its
/// strings, which php hands out for a notation table's entry.
fn notation_node(entry: *mut c_void) -> Option<Node> {
    let n = entry.cast::<Notation>();
    if n.is_null() {
        return None;
    }
    // SAFETY: a notation table's entry is an `xmlNotation`, live with its
    // DTD.
    let notation = unsafe { &*n };
    let (name, public_id, system_id) = (notation.name, notation.public_id, notation.system_id);
    // SAFETY: a variable libxml2 sets at load and nothing here writes.
    let alloc = unsafe { sys::xmlMalloc };
    // SAFETY: a fresh allocation of the struct's size.
    let entity = unsafe { alloc(std::mem::size_of::<sys::xmlEntity>()) }.cast::<sys::xmlEntity>();
    if entity.is_null() {
        return None;
    }
    // SAFETY: the fresh allocation, zeroed as php's `memset` does.
    unsafe { ptr::write_bytes(entity, 0, 1) };
    let copy = |s: *const u8| {
        // SAFETY: a NUL-terminated string of the notation, or null.
        unsafe { sys::xmlStrdup(s) }
    };
    let copies = (copy(name), copy(public_id), copy(system_id));
    // SAFETY: the zeroed struct this owns.
    let e = unsafe { &mut *entity };
    e.type_ = sys::XML_NOTATION_NODE;
    e.name = copies.0;
    e.ExternalID = copies.1;
    e.SystemID = copies.2;
    Node::from_raw(entity.cast())
}

impl Ns {
    pub(crate) fn from_raw(p: *mut sys::xmlNs) -> Option<Ns> {
        NonNull::new(p).map(Ns)
    }

    pub(crate) fn raw(self) -> *mut sys::xmlNs {
        self.0.as_ptr()
    }

    fn read<T>(self, f: impl FnOnce(&sys::xmlNs) -> T) -> T {
        // SAFETY: a handle names a live declaration.
        f(unsafe { &*self.0.as_ptr() })
    }

    fn write(self, f: impl FnOnce(&mut sys::xmlNs)) {
        // SAFETY: as in `read`; no other reference is held across `f`.
        f(unsafe { &mut *self.0.as_ptr() });
    }

    pub fn href(self) -> Option<Vec<u8>> {
        let p = self.read(|n| n.href);
        (!p.is_null()).then(|| owned(p))
    }

    pub fn prefix(self) -> Option<Vec<u8>> {
        let p = self.read(|n| n.prefix);
        (!p.is_null()).then(|| owned(p))
    }

    pub fn next(self) -> Option<Ns> {
        Ns::from_raw(self.read(|n| n.next))
    }

    pub fn set_next(self, next: Option<Ns>) {
        self.write(|n| n.next = next.map_or(ptr::null_mut(), Ns::raw));
    }

    /// The href replaced, the old one freed, as `setAttributeNS` rewrites an
    /// `xmlns` declaration.
    pub fn set_href(self, href: &[u8]) {
        let c = cstr(href);
        // SAFETY: a NUL-terminated copy.
        let fresh = unsafe { sys::xmlStrdup(c.as_ptr()) };
        self.write(|n| {
            let old = std::mem::replace(&mut n.href, fresh);
            // SAFETY: the declaration owned its href.
            unsafe { Alloc::free(old.cast_mut().cast()) };
        });
    }

    /// `dom_eliminate_ns`'s first half: href and prefix freed and nulled.
    pub fn clear(self) {
        self.write(|n| {
            let href = std::mem::replace(&mut n.href, ptr::null());
            let prefix = std::mem::replace(&mut n.prefix, ptr::null());
            // SAFETY: the declaration owned both strings.
            unsafe { Alloc::free(href.cast_mut().cast()) };
            // SAFETY: as above.
            unsafe { Alloc::free(prefix.cast_mut().cast()) };
        });
    }

    /// An `XPath` namespace node's element, the one libxml keeps in its
    /// `next` (`xmlXPathNodeSetDupNs`).
    pub fn xpath_parent(self) -> Option<Node> {
        Node::from_raw(self.read(|n| n.next).cast())
    }

    /// `php_libxml_set_old_ns(doc, ns)`.
    pub fn move_to_old(self, doc: Option<Node>) {
        // SAFETY: a live declaration no list holds, and a live document or
        // none.
        unsafe { Tree::set_old_ns(doc.map_or(ptr::null_mut(), |d| d.raw().cast()), self.raw()) };
    }

    /// `xmlFreeNs`: a declaration no list holds any more, freed with its
    /// strings, as `dom_unlink_ns_decls` frees what it lent C14N.
    pub fn free(self) {
        // SAFETY: a live declaration nothing links to, freed once.
        unsafe { sys::xmlFreeNs(self.raw()) };
    }
}

impl Ns {
    /// `dom_create_owned_ns`: a declaration of `href` under `prefix` no
    /// element holds, made field by field as php makes it, so the `xml`
    /// prefix `xmlNewNs` refuses is made too. The caller hands it to a
    /// document's old list ([`Ns::move_to_old`]), which frees it with the
    /// document.
    pub fn owned(href: Option<&[u8]>, prefix: Option<&[u8]>) -> Option<Ns> {
        let ns = Ns::new(None, href, None)?;
        if let Some(p) = prefix {
            let c = cstr(p);
            // SAFETY: a NUL-terminated prefix, duplicated for the
            // declaration, whose prefix is null so nothing is lost.
            let copy = unsafe { sys::xmlStrdup(c.as_ptr()) };
            ns.write(|n| n.prefix = copy);
        }
        Some(ns)
    }

    /// The declaration's `_private`, php's magic token or free tag.
    pub(crate) fn set_private(self, private: *mut c_void) {
        self.write(|n| n.private = private);
    }

    /// `xmlNewNs(node, href, prefix)`, `node` none for a declaration no element
    /// holds yet.
    pub fn new(node: Option<Node>, href: Option<&[u8]>, prefix: Option<&[u8]>) -> Option<Ns> {
        let h = href.map(cstr);
        let p = prefix.map(cstr);
        // SAFETY: a live element or none, NUL-terminated strings or null.
        Ns::from_raw(unsafe {
            sys::xmlNewNs(
                node.map_or(ptr::null_mut(), Node::raw),
                opt_ptr(h.as_ref()),
                opt_ptr(p.as_ref()),
            )
        })
    }
}

// ---- constructors

fn doc_ptr(doc: Option<Node>) -> *mut sys::xmlDoc {
    doc.map_or(ptr::null_mut(), |d| d.raw().cast())
}

impl Node {
    /// `xmlNewDoc(version)`.
    pub fn new_doc(version: Option<&[u8]>) -> Option<Node> {
        let v = version.map(cstr);
        // SAFETY: a version or null.
        Node::from_raw(unsafe { sys::xmlNewDoc(opt_ptr(v.as_ref())) }.cast())
    }

    /// `xmlNewDocNode(doc, ns, name, content)`.
    pub fn new_element(
        doc: Option<Node>,
        ns: Option<Ns>,
        name: &[u8],
        content: Option<&[u8]>,
    ) -> Option<Node> {
        let n = cstr(name);
        let c = content.map(cstr);
        // SAFETY: a document or none, a namespace or none, NUL-terminated
        // strings or null.
        Node::from_raw(unsafe {
            sys::xmlNewDocNode(
                doc_ptr(doc),
                ns.map_or(ptr::null_mut(), Ns::raw),
                n.as_ptr(),
                opt_ptr(c.as_ref()),
            )
        })
    }

    /// `xmlNewDocText(doc, content)`.
    pub fn new_text(doc: Option<Node>, content: &[u8]) -> Option<Node> {
        let c = cstr(content);
        // SAFETY: as above.
        Node::from_raw(unsafe { sys::xmlNewDocText(doc_ptr(doc), c.as_ptr()) })
    }

    /// `xmlNewDocTextLen(doc, content, len)`, the bytes as they are.
    pub fn new_text_len(doc: Option<Node>, content: &[u8]) -> Option<Node> {
        let len = c_int::try_from(content.len()).unwrap_or(c_int::MAX);
        // SAFETY: a document or none and `len` readable bytes.
        Node::from_raw(unsafe { sys::xmlNewDocTextLen(doc_ptr(doc), content.as_ptr(), len) })
    }

    /// `xmlNewTextLen(content, len)` named `xmlStringTextNoenc`, so the
    /// save writes its bytes unescaped, as php's soap `to_xml_any` makes
    /// the text of an `<anyXML>` value.
    pub fn new_text_noenc(content: &[u8]) -> Option<Node> {
        let len = c_int::try_from(content.len()).unwrap_or(c_int::MAX);
        // SAFETY: `len` readable bytes.
        let node = Node::from_raw(unsafe { sys::xmlNewTextLen(content.as_ptr(), len) })?;
        let name = sys::xmlStringTextNoenc.as_ptr();
        node.write(|n| n.name = name);
        Some(node)
    }

    /// Whether a node is named `xmlStringTextNoenc`, by address as libxml
    /// compares it.
    pub fn is_text_noenc(self) -> bool {
        let name = sys::xmlStringTextNoenc.as_ptr();
        self.read(|n| ptr::eq(n.name, name))
    }

    /// `xmlNewDocComment(doc, content)`.
    pub fn new_comment(doc: Option<Node>, content: &[u8]) -> Option<Node> {
        let c = cstr(content);
        // SAFETY: as above.
        Node::from_raw(unsafe { sys::xmlNewDocComment(doc_ptr(doc), c.as_ptr()) })
    }

    /// `xmlNewCDataBlock(doc, content, len)`.
    pub fn new_cdata(doc: Option<Node>, content: &[u8]) -> Option<Node> {
        let len = c_int::try_from(content.len()).unwrap_or(c_int::MAX);
        // SAFETY: as above.
        Node::from_raw(unsafe { sys::xmlNewCDataBlock(doc_ptr(doc), content.as_ptr(), len) })
    }

    /// `xmlNewDocPI(doc, name, content)`.
    pub fn new_pi(doc: Option<Node>, name: &[u8], content: Option<&[u8]>) -> Option<Node> {
        let n = cstr(name);
        let c = content.map(cstr);
        // SAFETY: as above.
        Node::from_raw(unsafe { sys::xmlNewDocPI(doc_ptr(doc), n.as_ptr(), opt_ptr(c.as_ref())) })
    }

    /// `xmlNewReference(doc, name)`.
    pub fn new_reference(doc: Option<Node>, name: &[u8]) -> Option<Node> {
        let n = cstr(name);
        // SAFETY: as above.
        Node::from_raw(unsafe { sys::xmlNewReference(doc_ptr(doc), n.as_ptr()) })
    }

    /// `xmlNewDocFragment(doc)`.
    pub fn new_fragment(doc: Option<Node>) -> Option<Node> {
        // SAFETY: a document or none.
        Node::from_raw(unsafe { sys::xmlNewDocFragment(doc_ptr(doc)) })
    }

    /// `xmlNewDocProp(doc, name, value)`.
    pub fn new_attr(doc: Option<Node>, name: &[u8], value: Option<&[u8]>) -> Option<Node> {
        let n = cstr(name);
        let v = value.map(cstr);
        // SAFETY: as above.
        Node::from_raw(
            unsafe { sys::xmlNewDocProp(doc_ptr(doc), n.as_ptr(), opt_ptr(v.as_ref())) }.cast(),
        )
    }

    /// `xmlNewProp(NULL, name, value)`.
    pub fn new_prop(name: &[u8], value: Option<&[u8]>) -> Option<Node> {
        let n = cstr(name);
        let v = value.map(cstr);
        // SAFETY: NUL-terminated strings or null.
        Node::from_raw(
            unsafe { sys::xmlNewProp(ptr::null_mut(), n.as_ptr(), opt_ptr(v.as_ref())) }.cast(),
        )
    }

    /// `xmlNewPI(name, content)`.
    pub fn new_pi_detached(name: &[u8], content: Option<&[u8]>) -> Option<Node> {
        let n = cstr(name);
        let c = content.map(cstr);
        // SAFETY: as above.
        Node::from_raw(unsafe { sys::xmlNewPI(n.as_ptr(), opt_ptr(c.as_ref())) })
    }

    /// `xmlNewComment(content)`, null content for none.
    pub fn new_comment_detached(content: Option<&[u8]>) -> Option<Node> {
        let c = content.map(cstr);
        // SAFETY: as above.
        Node::from_raw(unsafe { sys::xmlNewComment(opt_ptr(c.as_ref())) })
    }

    /// `xmlNewText(content)`, null content for none.
    pub fn new_text_detached(content: Option<&[u8]>) -> Option<Node> {
        let c = content.map(cstr);
        // SAFETY: as above.
        Node::from_raw(unsafe { sys::xmlNewText(opt_ptr(c.as_ref())) })
    }

    /// `xmlCreateIntSubset(doc, name, external_id, system_id)`.
    pub fn create_int_subset(
        doc: Option<Node>,
        name: &[u8],
        external_id: Option<&[u8]>,
        system_id: Option<&[u8]>,
    ) -> Option<Node> {
        let n = cstr(name);
        let e = external_id.map(cstr);
        let s = system_id.map(cstr);
        // SAFETY: as above.
        Node::from_raw(
            unsafe {
                sys::xmlCreateIntSubset(
                    doc_ptr(doc),
                    n.as_ptr(),
                    opt_ptr(e.as_ref()),
                    opt_ptr(s.as_ref()),
                )
            }
            .cast(),
        )
    }

    /// `htmlNewDocNoDtD(NULL, NULL)`.
    pub fn new_html_doc() -> Option<Node> {
        // SAFETY: no argument.
        Node::from_raw(unsafe { sys::htmlNewDocNoDtD(ptr::null(), ptr::null()) }.cast())
    }
}

// ---- the tree calls php's simplexml makes

impl Node {
    /// `xmlNewChild(parent, ns, name, content)`: a child element appended,
    /// `content` read as markup whose entity references become nodes.
    pub fn new_child(self, ns: Option<Ns>, name: &[u8], content: Option<&[u8]>) -> Option<Node> {
        let n = cstr(name);
        let c = content.map(cstr);
        // SAFETY: a live parent, a namespace or none, NUL-terminated
        // strings or null.
        Node::from_raw(unsafe {
            sys::xmlNewChild(
                self.raw(),
                ns.map_or(ptr::null_mut(), Ns::raw),
                n.as_ptr(),
                opt_ptr(c.as_ref()),
            )
        })
    }

    /// `xmlNewTextChild(parent, ns, name, content)`: a child element
    /// appended, `content` its one text node as it is.
    pub fn new_text_child(
        self,
        ns: Option<Ns>,
        name: &[u8],
        content: Option<&[u8]>,
    ) -> Option<Node> {
        let n = cstr(name);
        let c = content.map(cstr);
        // SAFETY: as above.
        Node::from_raw(unsafe {
            sys::xmlNewTextChild(
                self.raw(),
                ns.map_or(ptr::null_mut(), Ns::raw),
                n.as_ptr(),
                opt_ptr(c.as_ref()),
            )
        })
    }

    /// `xmlNewNsProp(node, ns, name, value)`, which `xmlNewProp(node, name,
    /// value)` is with no namespace: an attribute appended whatever the
    /// element already has.
    pub fn new_ns_prop(self, ns: Option<Ns>, name: &[u8], value: Option<&[u8]>) -> Option<Node> {
        let n = cstr(name);
        let v = value.map(cstr);
        // SAFETY: a live element, a namespace or none, NUL-terminated
        // strings or null.
        Node::from_raw(
            unsafe {
                sys::xmlNewNsProp(
                    self.raw(),
                    ns.map_or(ptr::null_mut(), Ns::raw),
                    n.as_ptr(),
                    opt_ptr(v.as_ref()),
                )
            }
            .cast(),
        )
    }

    /// `xmlEncodeEntitiesReentrant(node->doc, text)`: the text with `&`,
    /// `<` and `>` (and what the document's encoding cannot carry) made
    /// references.
    pub fn encode_entities(self, text: &[u8]) -> Option<Vec<u8>> {
        let t = cstr(text);
        // SAFETY: the node's document or null, and a NUL-terminated string.
        taken(unsafe { sys::xmlEncodeEntitiesReentrant(doc_ptr(self.doc()), t.as_ptr()) })
    }

    /// `xmlCopyDoc(doc, recursive)`: a document of the caller's own.
    pub fn copy_doc(self, recursive: bool) -> Option<Node> {
        if !self.kind().is_document() {
            return None;
        }
        // SAFETY: a live document.
        Node::from_raw(unsafe { sys::xmlCopyDoc(self.raw().cast(), c_int::from(recursive)) }.cast())
    }
}

// ---- names

/// A name as libxml2 validates and splits it.
pub struct Name;

impl Name {
    /// `xmlValidateName(name, 0) == 0`.
    pub fn valid(name: &[u8]) -> bool {
        let n = cstr(name);
        // SAFETY: a NUL-terminated name.
        unsafe { sys::xmlValidateName(n.as_ptr(), 0) == 0 }
    }

    /// `xmlValidateQName(name, 0) == 0`.
    pub fn valid_qname(name: &[u8]) -> bool {
        let n = cstr(name);
        // SAFETY: as above.
        unsafe { sys::xmlValidateQName(n.as_ptr(), 0) == 0 }
    }

    /// `xmlValidateNCName(name, 0) == 0`.
    pub fn valid_ncname(name: &[u8]) -> bool {
        let n = cstr(name);
        // SAFETY: as above.
        unsafe { sys::xmlValidateNCName(n.as_ptr(), 0) == 0 }
    }

    /// `xmlSplitQName2(name, &prefix)`: the local name and the prefix, `None`
    /// where the name has no prefix.
    pub fn split2(name: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        let n = cstr(name);
        let mut prefix: *mut u8 = ptr::null_mut();
        // SAFETY: a NUL-terminated name and a local out-pointer.
        let local = unsafe { sys::xmlSplitQName2(n.as_ptr(), &raw mut prefix) };
        let local = taken(local)?;
        Some((local, taken(prefix).unwrap_or_default()))
    }

    /// `xmlSplitQName3(name, &len)`: where the local part starts, `None` for a
    /// name with no `:`.
    pub fn split3(name: &[u8]) -> Option<usize> {
        let n = cstr(name);
        let mut len: c_int = 0;
        // SAFETY: a NUL-terminated name and a local out-pointer.
        let local = unsafe { sys::xmlSplitQName3(n.as_ptr(), &raw mut len) };
        if local.is_null() {
            return None;
        }
        // `len` is the prefix's length; the local part is past its `:`.
        usize::try_from(len).ok().map(|len| len + 1)
    }
}

// ---- UTF-8, as libxml counts it

/// UTF-8 as libxml2 counts it, in characters.
pub struct Utf8;

impl Utf8 {
    /// `xmlUTF8Strlen`, `-1` for bytes that are no UTF-8.
    pub fn len(s: &[u8]) -> i64 {
        let c = cstr(s);
        // SAFETY: a NUL-terminated string.
        i64::from(unsafe { sys::xmlUTF8Strlen(c.as_ptr()) })
    }

    /// `xmlUTF8Strsub(s, start, len)`.
    pub fn sub(s: &[u8], start: i64, len: i64) -> Option<Vec<u8>> {
        let c = cstr(s);
        let start = c_int::try_from(start).ok()?;
        let len = c_int::try_from(len).ok()?;
        // SAFETY: a NUL-terminated string.
        taken(unsafe { sys::xmlUTF8Strsub(c.as_ptr(), start, len) })
    }

    /// `xmlUTF8Strndup(s, len)`.
    pub fn ndup(s: &[u8], len: i64) -> Option<Vec<u8>> {
        let c = cstr(s);
        let len = c_int::try_from(len).ok()?;
        // SAFETY: as above.
        taken(unsafe { sys::xmlUTF8Strndup(c.as_ptr(), len) })
    }
}

impl Library {
    /// `xmlFindCharEncodingHandler(name) != NULL`, the handler closed again.
    pub fn known_encoding(name: &[u8]) -> bool {
        let n = cstr(name);
        // SAFETY: a NUL-terminated name.
        let handler = unsafe { sys::xmlFindCharEncodingHandler(n.as_ptr().cast::<c_char>()) };
        if handler.is_null() {
            return false;
        }
        // SAFETY: the handler just found, closed once.
        unsafe { sys::xmlCharEncCloseFunc(handler) };
        true
    }

    /// `xmlBuildURI`, unused by the legacy classes beyond a base's check.
    pub fn canonic_path(path: &[u8]) -> Option<Vec<u8>> {
        let p = cstr(path);
        // SAFETY: a NUL-terminated path.
        taken(unsafe { sys::xmlCanonicPath(p.as_ptr()) })
    }

    /// `xmlPathToURI`: a file path as the URI a document's URL keeps.
    pub fn path_to_uri(path: &[u8]) -> Option<Vec<u8>> {
        let p = cstr(path);
        // SAFETY: a NUL-terminated path.
        taken(unsafe { sys::xmlPathToURI(p.as_ptr()) })
    }

    /// `xmlBuildURI(uri, base)`: `uri` resolved against `base`, which
    /// `php_schema.c` resolves an import's and an include's
    /// `schemaLocation` with; `None` where libxml could not.
    pub fn build_uri(uri: &[u8], base: Option<&[u8]>) -> Option<Vec<u8>> {
        let u = cstr(uri);
        let b = base.map(cstr);
        // SAFETY: NUL-terminated strings or null.
        taken(unsafe { sys::xmlBuildURI(u.as_ptr(), opt_ptr(b.as_ref())) })
    }

    /// `xmlCharEncOutFunc` over the handler `xmlFindCharEncodingHandler`
    /// finds for `name`: UTF-8 bytes in the encoding `name` names, as
    /// php's soap answers a string to a client or a server given an
    /// `encoding`; `None` where libxml knows no such encoding or could not
    /// convert.
    pub fn from_utf8(name: &[u8], bytes: &[u8]) -> Option<Vec<u8>> {
        Library::convert(name, bytes, true)
    }

    /// `xmlCharEncInFunc` over the handler for `name`: bytes in the
    /// encoding `name` names as UTF-8, as php's soap reads a string a
    /// program gave it.
    pub fn to_utf8(name: &[u8], bytes: &[u8]) -> Option<Vec<u8>> {
        Library::convert(name, bytes, false)
    }

    fn convert(name: &[u8], bytes: &[u8], out: bool) -> Option<Vec<u8>> {
        let len = c_int::try_from(bytes.len()).ok()?;
        let n = cstr(name);
        // SAFETY: a NUL-terminated name.
        let handler = unsafe { sys::xmlFindCharEncodingHandler(n.as_ptr().cast::<c_char>()) };
        if handler.is_null() {
            return None;
        }
        // SAFETY: no argument; a fresh buffer this function frees.
        let input = unsafe { sys::xmlBufferCreate() };
        // SAFETY: as above.
        let output = unsafe { sys::xmlBufferCreate() };
        let mut answer = None;
        if !input.is_null() && !output.is_null() {
            // SAFETY: a live buffer and `len` readable bytes, copied.
            let added = unsafe { sys::xmlBufferAdd(input, bytes.as_ptr(), len) };
            let n = if added != 0 {
                -1
            } else if out {
                // SAFETY: a live handler and two live buffers.
                unsafe { sys::xmlCharEncOutFunc(handler, output, input) }
            } else {
                // SAFETY: a live handler and two live buffers.
                unsafe { sys::xmlCharEncInFunc(handler, output, input) }
            };
            if n >= 0 {
                // SAFETY: a live buffer.
                let size = unsafe { sys::xmlBufferLength(output) };
                // SAFETY: a live buffer's content, `size` bytes long.
                let content = unsafe { sys::xmlBufferContent(output) };
                let size = usize::try_from(size).unwrap_or(0);
                answer = Some(if content.is_null() || size == 0 {
                    Vec::new()
                } else {
                    // SAFETY: `size` readable bytes the buffer owns, copied.
                    unsafe { std::slice::from_raw_parts(content, size) }.to_vec()
                });
            }
        }
        if !input.is_null() {
            // SAFETY: the buffer made above, freed once.
            unsafe { sys::xmlBufferFree(input) };
        }
        if !output.is_null() {
            // SAFETY: as above.
            unsafe { sys::xmlBufferFree(output) };
        }
        // SAFETY: the handler found above, closed once.
        unsafe { sys::xmlCharEncCloseFunc(handler) };
        answer
    }
}

/// A node and the reference to its own document that an object standing
/// for it counts: php's `intern->document` in
/// `php_dom_instantiate_object_helper`, checked. Only [`DocOf::under`] and
/// [`DocOf::exported`] build one, each by checking the reference against
/// the node's document, so no object stands for a node of one document
/// while counting another's, which lets the node's document go under the
/// object with its last real owner (#3541). `'d` is the borrow of what the
/// reference was read off, within one native call.
#[derive(Debug, Clone, Copy)]
pub struct DocOf<'d> {
    node: Node,
    counted: Counted,
    read_off: PhantomData<&'d NodeObject>,
}

/// What an object standing for a [`DocOf`]'s node counts.
#[derive(Debug, Clone, Copy)]
enum Counted {
    /// Nothing: the node is of no document.
    Nothing,
    /// The reference to the node's document a live object counts.
    Shared(SharedDoc),
    /// A first reference: the node is a document no object counts yet.
    First,
}

impl<'d> DocOf<'d> {
    /// `node` counted under the reference `by` counts where that is the
    /// node's document's, otherwise under the one the object standing
    /// for the node's document counts; a document node no object counts
    /// takes its first reference where `by` counts nothing. `None` where
    /// no reference to the node's document is in reach, so no object can
    /// stand for the node without counting another document.
    #[must_use]
    pub fn under(node: Node, by: Option<&'d NodeObject>) -> Option<DocOf<'d>> {
        let shared = by.and_then(|b| b.shared_doc().map(|s| (s, b.document())));
        DocOf::checked(node, shared, shared.is_none())
    }

    /// The node an export names, counted under the reference its object
    /// counts, checked as [`DocOf::under`] checks it.
    #[must_use]
    pub fn exported(export: &'d Exported) -> Option<DocOf<'d>> {
        // SAFETY: an export is made off a live object that counts the
        // document, within the native call that reads it.
        let shared = export.document().map(|s| (s, unsafe { s.doc() }));
        DocOf::checked(export.node(), shared, shared.is_none())
    }

    /// The check, with `shared` a reference beside the document it names.
    fn checked(
        node: Node,
        shared: Option<(SharedDoc, *mut sys::xmlDoc)>,
        fresh: bool,
    ) -> Option<DocOf<'d>> {
        let made = |counted| {
            Some(DocOf {
                node,
                counted,
                read_off: PhantomData,
            })
        };
        let Some(doc) = node.doc() else {
            return made(Counted::Nothing);
        };
        let raw = doc.raw().cast::<sys::xmlDoc>();
        if let Some((s, _)) = shared.filter(|&(_, d)| d == raw) {
            return made(Counted::Shared(s));
        }
        let own = doc.wrapper();
        if let Some(s) = own
            .as_deref()
            .filter(|o| o.document() == raw)
            .and_then(NodeObject::shared_doc)
        {
            return made(Counted::Shared(s));
        }
        if fresh && node == doc && own.is_none() {
            return made(Counted::First);
        }
        None
    }
}

impl NodeObject {
    /// `php_dom_instantiate_object_helper`'s libxml half: the node's own
    /// document counted, as [`DocOf`] checked it, and the node's proxy
    /// taken, by the half of an object that counts no document yet.
    pub fn attach(&mut self, req: &mut Request, of: DocOf<'_>) {
        self.count_document(req, of);
        // SAFETY: a live node, and this is the half of the object standing
        // for it.
        unsafe { self.increment_node_ptr(req, of.node.raw(), true) };
    }

    /// `php_libxml_increment_doc_ref` over the document of `of`'s node,
    /// standing for no node: a count held as a node list holds its
    /// document, given back by [`NodeObject::release`].
    pub fn count_document(&mut self, req: &mut Request, of: DocOf<'_>) {
        match of.counted {
            Counted::Nothing => {}
            Counted::Shared(s) => {
                // SAFETY: `DocOf` read `s` off a live object within this
                // call and checked that it names the node's document.
                unsafe { self.increment_doc_ref_shared(req, Some(s), ptr::null_mut()) };
            }
            Counted::First => {
                // SAFETY: the node is a live document no reference counts.
                unsafe { self.increment_doc_ref(req, of.node.raw().cast()) };
            }
        }
    }

    /// `intern->document = NULL; php_libxml_increment_doc_ref(intern, doc)`:
    /// a first reference to a document nothing counts yet, as a document
    /// object takes the tree it just parsed or made.
    pub fn adopt_document(&mut self, req: &mut Request, doc: Node) {
        // SAFETY: `doc` is a live document no reference counts yet.
        unsafe { self.increment_doc_ref(req, doc.raw().cast()) };
    }

    /// `intern->document = docobj->document;
    /// php_libxml_increment_doc_ref(intern, doc)`: one more owner of the
    /// document another object counts, as `DOMXPath` takes its
    /// document's, and a first reference to `doc` where there is none.
    pub fn share(&mut self, req: &mut Request, shared: Option<SharedDoc>, doc: Node) {
        // SAFETY: `shared` was read off a live object's reference in the
        // same native call, and `doc` is a live document.
        unsafe { self.increment_doc_ref_shared(req, shared, doc.raw().cast()) };
    }

    /// `php_libxml_increment_node_ptr(intern, node, intern)`.
    pub fn take_node(&mut self, req: &mut Request, node: Node) {
        // SAFETY: a live node the object will stand for.
        unsafe { self.increment_node_ptr(req, node.raw(), true) };
    }

    /// `php_libxml_increment_node_ptr(intern, node, NULL)`: the node counted
    /// and nothing named as the object that stands for it, as simplexml
    /// holds its nodes, so dom still answers a node of its own for one.
    pub fn hold_node(&mut self, req: &mut Request, node: Node) {
        // SAFETY: a live node, and no object named.
        unsafe { self.increment_node_ptr(req, node.raw(), false) };
    }
}

impl Node {
    /// `dom_set_document_ref_obj_single`: the object standing for the node,
    /// where one counts no document, counting `document`; `false` where one
    /// counts another already.
    pub fn share_document(self, req: &mut Request, document: SharedDoc) -> bool {
        let Some(mut wrapper) = self.wrapper() else {
            return true;
        };
        if wrapper.has_document() {
            return false;
        }
        // SAFETY: `document` is counted by the live object the caller read
        // it off, and this object counts none.
        unsafe { wrapper.increment_doc_ref_shared(req, Some(document), ptr::null_mut()) };
        true
    }

    /// `php_dom_transfer_document_ref_single_node`.
    pub fn transfer_document(self, req: &mut Request, document: SharedDoc) {
        if let Some(mut wrapper) = self.wrapper() {
            // SAFETY: as above.
            unsafe { wrapper.transfer_document(req, document) };
        }
    }
}

impl Node {
    /// `php_dom_create_fake_namespace_decl_node_ptr`: a node standing for
    /// the declaration of `href` and `prefix` on `parent`, typed `XML_NAMESPACE_DECL`,
    /// named by the prefix (or `xmlns`), its content the href, owning a
    /// copy of the declaration; the object that stands for it frees it.
    /// It is made in `doc`, the document of `parent` or, with no parent
    /// as for the clone of one whose element is gone, the original's (php
    /// 8.5.12's `fallback_doc`), and then stands under no element.
    pub fn fake_namespace_decl(
        parent: Option<Node>,
        doc: Option<Node>,
        href: Option<&[u8]>,
        prefix: Option<&[u8]>,
    ) -> Option<Node> {
        let copy = Ns::new(None, href, None)?;
        if let Some(p) = prefix {
            let c = cstr(p);
            // SAFETY: a NUL-terminated prefix, duplicated for the copy,
            // whose prefix is null so nothing is lost.
            let owned = unsafe { sys::xmlStrdup(c.as_ptr()) };
            copy.write(|n| n.prefix = owned);
        }
        let name = prefix.unwrap_or(b"xmlns");
        let node = Node::new_element(doc, None, name, href)?;
        node.write(|n| {
            n.type_ = sys::XML_NAMESPACE_DECL;
            n.parent = parent.map_or(ptr::null_mut(), Node::raw);
            n.ns = copy.raw();
        });
        Some(node)
    }
}
