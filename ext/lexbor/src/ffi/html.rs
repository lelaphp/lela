//! lexbor's HTML5 parser (`lexbor/html`), as `html_document.c`,
//! `html5_parser.c` and `inner_outer_html_mixin.c` drive it: a
//! [`Document`] parsed chunk by chunk with the tokenizer's and the tree
//! builder's parse errors read as they are raised, the [`Observations`]
//! php's patch 0002 records, a [`Fragment`] parsed as the children of one
//! element, and the tree walked through [`Node`] handles that borrow the
//! document they name, so none outlives it.

use std::marker::PhantomData;
use std::ptr::NonNull;

use super::encoding::Status;
use super::{Lent, Owned, sys};

/// An HTML document of lexbor's, `lxb_html_document_t`, on the request's
/// heap: made, parsed and given back ([`Owned`]) under a lend, with
/// every node and parser it owns.
pub struct Document(NonNull<sys::lxb_dom_document_t>);

// SAFETY: the document and everything it points at are owned by this
// value alone and reached only through it.
unsafe impl Send for Document {}

/// `lxb_dom_document_cmode_t`: whether the document is in quirks, which the
/// tree builder sets from the doctype and CSS selectors read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quirks {
    NoQuirks,
    Quirks,
    LimitedQuirks,
}

/// What the tree builder saw that the tree does not keep (php's patch
/// 0002), and the quirks it settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // three observations, each one bit of the tree builder's
pub struct Observations {
    pub has_explicit_html_tag: bool,
    pub has_explicit_head_tag: bool,
    pub has_explicit_body_tag: bool,
    pub quirks: Quirks,
}

/// One tokenizer parse error: which, and its byte offset in the chunk the
/// caller handed [`Document::parse_chunk`], `pos - chunk` as php computes
/// it (wrapping where lexbor pointed into its own buffer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenizerError {
    pub id: TokenizerErrorId,
    pub offset: usize,
}

/// One tree-builder parse error, with the position php's patch 0001
/// records: line and column from zero, and the token's length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeError {
    pub id: TreeErrorId,
    pub line: usize,
    pub column: usize,
    pub length: usize,
}

impl Document {
    /// `lxb_html_document_create`.
    #[must_use]
    pub fn new(_: &Lent) -> Option<Document> {
        // SAFETY: no argument; the document is this value's from here.
        NonNull::new(unsafe { sys::lxb_html_document_create() }).map(Document)
    }

    fn raw(&self) -> *mut sys::lxb_dom_document_t {
        self.0.as_ptr()
    }

    /// `lxb_html_document_parse_chunk_begin`.
    pub fn parse_chunk_begin(&mut self, _: &Lent) -> Status {
        // SAFETY: a live document this owns.
        Status::from_raw(unsafe { sys::lxb_html_document_parse_chunk_begin(self.raw()) })
    }

    /// `lxb_html_document_parse_chunk`: `chunk` tokenized and built; the
    /// parser keeps no pointer into it.
    pub fn parse_chunk(&mut self, _: &Lent, chunk: &[u8]) -> Status {
        // SAFETY: a live document and the live bytes of `chunk`.
        Status::from_raw(unsafe {
            sys::lxb_html_document_parse_chunk(self.raw(), chunk.as_ptr(), chunk.len())
        })
    }

    /// `lxb_html_document_parse_chunk_end`.
    pub fn parse_chunk_end(&mut self, _: &Lent) -> Status {
        // SAFETY: a live document this owns.
        Status::from_raw(unsafe { sys::lxb_html_document_parse_chunk_end(self.raw()) })
    }

    fn parser(&self) -> *mut std::ffi::c_void {
        // SAFETY: a live document; `parser` is its field.
        unsafe { (*self.raw()).parser }
    }

    /// The tokenizer's errors from index `from` on, their offsets taken
    /// against `chunk`, the slice last handed to [`Document::parse_chunk`]
    /// (`lexbor_libxml2_bridge_report_errors`). Empty before any parse.
    #[must_use]
    pub fn tokenizer_errors(&self, from: usize, chunk: &[u8]) -> Vec<TokenizerError> {
        let parser = self.parser();
        if parser.is_null() {
            return Vec::new();
        }
        // SAFETY: a live parser of this document.
        let tkz = unsafe { sys::lxb_html_parser_tokenizer_noi(parser) }
            .cast::<sys::lxb_html_tokenizer_head_t>();
        if tkz.is_null() {
            return Vec::new();
        }
        // SAFETY: a live tokenizer, read up to its parse errors.
        let errors = unsafe { (*tkz).parse_errors };
        let base = chunk.as_ptr() as usize;
        entries::<sys::lxb_html_tokenizer_error_t>(errors, from)
            .map(|e| TokenizerError {
                id: TokenizerErrorId::from_raw(e.id),
                offset: (e.pos as usize).wrapping_sub(base),
            })
            .collect()
    }

    /// The tree builder's errors from index `from` on.
    #[must_use]
    pub fn tree_errors(&self, from: usize) -> Vec<TreeError> {
        let Some(tree) = self.tree() else {
            return Vec::new();
        };
        entries::<sys::lxb_html_tree_error_t>(tree.parse_errors, from)
            .map(|e| TreeError {
                id: TreeErrorId::from_raw(e.id),
                line: e.line,
                column: e.column,
                length: e.length,
            })
            .collect()
    }

    fn tree(&self) -> Option<&sys::lxb_html_tree_head_t> {
        let parser = self.parser();
        if parser.is_null() {
            return None;
        }
        // SAFETY: a live parser of this document.
        let tree =
            unsafe { sys::lxb_html_parser_tree_noi(parser) }.cast::<sys::lxb_html_tree_head_t>();
        // SAFETY: a live tree while the parser lives, read up to the
        // fields mirrored.
        unsafe { tree.as_ref() }
    }

    /// `lexbor_libxml2_bridge_copy_observations`.
    #[must_use]
    pub fn observations(&self) -> Observations {
        let (html, head, body) = self.tree().map_or((false, false, false), |t| {
            (
                t.has_explicit_html_tag,
                t.has_explicit_head_tag,
                t.has_explicit_body_tag,
            )
        });
        Observations {
            has_explicit_html_tag: html,
            has_explicit_head_tag: head,
            has_explicit_body_tag: body,
            quirks: self.quirks(),
        }
    }

    /// `document->dom_document.compat_mode`.
    #[must_use]
    pub fn quirks(&self) -> Quirks {
        // SAFETY: a live document.
        match unsafe { (*self.raw()).compat } {
            0x01 => Quirks::Quirks,
            0x02 => Quirks::LimitedQuirks,
            _ => Quirks::NoQuirks,
        }
    }

    /// Set `document->dom_document.compat_mode`, as the fragment parse
    /// carries the quirks of the document it parses for over.
    pub fn set_quirks(&mut self, quirks: Quirks) {
        let raw = match quirks {
            Quirks::NoQuirks => 0x00,
            Quirks::Quirks => 0x01,
            Quirks::LimitedQuirks => 0x02,
        };
        // SAFETY: a live document this owns.
        unsafe { (*self.raw()).compat = raw };
    }

    /// The document node, whose children the bridge converts from the last
    /// back (`lxb_dom_interface_node(document)`).
    #[must_use]
    pub fn node(&self) -> Node<'_> {
        Node {
            ptr: self.0.cast(),
            doc: PhantomData,
        }
    }
}

impl Owned for Document {
    /// `lxb_html_document_destroy`.
    fn give_back(self, _: &Lent) {
        // SAFETY: the document this owns, destroyed once with its parser:
        // the value is taken.
        unsafe { sys::lxb_html_document_destroy(self.raw()) };
    }
}

/// The HTML fragment parsing algorithm, `dom_html_fragment_parsing_algorithm`:
/// a document of its own with an element standing for the one the
/// fragment is parsed under, parsed chunk by chunk and ended by
/// [`FragmentParser::end`], or given back unended ([`Owned`]).
pub struct FragmentParser {
    document: Document,
}

impl Owned for FragmentParser {
    fn give_back(self, lent: &Lent) {
        self.document.give_back(lent);
    }
}

/// A parsed fragment: its document, and the node whose children are the
/// fragment's, given back with the document ([`Owned`]).
pub struct Fragment {
    /// Held so the nodes `root` reaches live as long as this.
    document: Document,
    root: NonNull<sys::lxb_dom_node_t>,
}

impl Owned for Fragment {
    fn give_back(self, lent: &Lent) {
        self.document.give_back(lent);
    }
}

impl FragmentParser {
    /// A document in `quirks` with an element of the tag
    /// `name` in the namespace `ns_uri` (`lxb_tag_data_by_name`,
    /// `lxb_ns_data_by_link`, `LXB_TAG__UNDEF` and `LXB_NS__UNDEF` for
    /// what lexbor does not know), and
    /// `lxb_html_document_parse_fragment_chunk_begin`.
    #[must_use]
    pub fn begin(
        lent: &Lent,
        name: &[u8],
        ns_uri: &[u8],
        quirks: Quirks,
    ) -> Option<FragmentParser> {
        let mut document = Document::new(lent)?;
        document.set_quirks(quirks);
        // SAFETY: a live document; the element is the document's.
        let Some(context) =
            NonNull::new(unsafe { sys::lxb_dom_element_interface_create(document.raw()) })
        else {
            document.give_back(lent);
            return None;
        };
        // SAFETY: a live document; `tags` is its field.
        let tags = unsafe { (*document.raw()).tags };
        // SAFETY: the document's tag table and the live bytes of `name`.
        let tag = unsafe { sys::lxb_tag_id_by_name_noi(tags, name.as_ptr(), name.len()) };
        // SAFETY: a live document; `ns` is its field.
        let ns_table = unsafe { (*document.raw()).ns };
        // SAFETY: the document's namespace table and the live bytes of
        // `ns_uri`.
        let ns_data = unsafe { sys::lxb_ns_data_by_link(ns_table, ns_uri.as_ptr(), ns_uri.len()) };
        // SAFETY: null or an entry of the table.
        let ns = unsafe { ns_data.as_ref() }.map_or(sys::LXB_NS__UNDEF, |d| d.ns_id);
        // SAFETY: the element just created, written before any parse.
        unsafe { (*context.as_ptr()).node.local_name = tag };
        // SAFETY: as above.
        unsafe { (*context.as_ptr()).node.ns = ns };
        // SAFETY: a live document and its element.
        let status = unsafe {
            sys::lxb_html_document_parse_fragment_chunk_begin(document.raw(), context.as_ptr())
        };
        if status == sys::LXB_STATUS_OK {
            Some(FragmentParser { document })
        } else {
            document.give_back(lent);
            None
        }
    }

    /// `lxb_html_document_parse_fragment_chunk`.
    pub fn chunk(&mut self, _: &Lent, chunk: &[u8]) -> Status {
        // SAFETY: a live document and the live bytes of `chunk`.
        Status::from_raw(unsafe {
            sys::lxb_html_document_parse_fragment_chunk(
                self.document.raw(),
                chunk.as_ptr(),
                chunk.len(),
            )
        })
    }

    /// `lxb_html_document_parse_fragment_chunk_end`: the fragment, `None`
    /// where lexbor gave no root, its document then given back.
    #[must_use]
    pub fn end(self, lent: &Lent) -> Option<Fragment> {
        // SAFETY: a live document mid-parse.
        let root = unsafe { sys::lxb_html_document_parse_fragment_chunk_end(self.document.raw()) };
        let Some(root) = NonNull::new(root) else {
            self.document.give_back(lent);
            return None;
        };
        Some(Fragment {
            document: self.document,
            root,
        })
    }
}

impl Fragment {
    /// The node whose children are the fragment's; the bridge converts
    /// from its last child back.
    #[must_use]
    pub fn root(&self) -> Node<'_> {
        Node {
            ptr: self.root,
            doc: PhantomData,
        }
    }
}

/// `lxb_dom_node_type_t`, the kinds the HTML parser makes and the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Element,
    Attribute,
    Text,
    CdataSection,
    ProcessingInstruction,
    Comment,
    Document,
    DocumentType,
    DocumentFragment,
    /// The historical kinds and lexbor's own.
    Other,
}

/// The namespaces lexbor names (`lxb_ns_id_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    Html,
    MathMl,
    Svg,
    XLink,
    Xml,
    Xmlns,
    /// `LXB_NS__UNDEF`, `LXB_NS__ANY` and ids past lexbor's own.
    Other,
}

impl Namespace {
    fn from_raw(ns: usize) -> Namespace {
        match ns {
            sys::LXB_NS_HTML => Namespace::Html,
            sys::LXB_NS_MATH => Namespace::MathMl,
            sys::LXB_NS_SVG => Namespace::Svg,
            sys::LXB_NS_XLINK => Namespace::XLink,
            sys::LXB_NS_XML => Namespace::Xml,
            sys::LXB_NS_XMLNS => Namespace::Xmlns,
            _ => Namespace::Other,
        }
    }
}

/// A node of a lexbor document, borrowed for as long as the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node<'doc> {
    ptr: NonNull<sys::lxb_dom_node_t>,
    doc: PhantomData<&'doc Document>,
}

impl<'doc> Node<'doc> {
    fn node(self) -> &'doc sys::lxb_dom_node_t {
        // SAFETY: a node of a document that outlives 'doc and is not
        // parsed into while it is borrowed.
        unsafe { &*self.ptr.as_ptr() }
    }

    fn wrap(ptr: *mut sys::lxb_dom_node_t) -> Option<Node<'doc>> {
        NonNull::new(ptr).map(|ptr| Node {
            ptr,
            doc: PhantomData,
        })
    }

    /// The node's kind.
    #[must_use]
    pub fn kind(self) -> NodeKind {
        match self.node().type_ {
            sys::LXB_DOM_NODE_TYPE_ELEMENT => NodeKind::Element,
            sys::LXB_DOM_NODE_TYPE_ATTRIBUTE => NodeKind::Attribute,
            sys::LXB_DOM_NODE_TYPE_TEXT => NodeKind::Text,
            sys::LXB_DOM_NODE_TYPE_CDATA_SECTION => NodeKind::CdataSection,
            sys::LXB_DOM_NODE_TYPE_PROCESSING_INSTRUCTION => NodeKind::ProcessingInstruction,
            sys::LXB_DOM_NODE_TYPE_COMMENT => NodeKind::Comment,
            sys::LXB_DOM_NODE_TYPE_DOCUMENT => NodeKind::Document,
            sys::LXB_DOM_NODE_TYPE_DOCUMENT_TYPE => NodeKind::DocumentType,
            sys::LXB_DOM_NODE_TYPE_DOCUMENT_FRAGMENT => NodeKind::DocumentFragment,
            _ => NodeKind::Other,
        }
    }

    #[must_use]
    pub fn first_child(self) -> Option<Node<'doc>> {
        Node::wrap(self.node().first_child)
    }

    #[must_use]
    pub fn last_child(self) -> Option<Node<'doc>> {
        Node::wrap(self.node().last_child)
    }

    #[must_use]
    pub fn next(self) -> Option<Node<'doc>> {
        Node::wrap(self.node().next)
    }

    #[must_use]
    pub fn prev(self) -> Option<Node<'doc>> {
        Node::wrap(self.node().prev)
    }

    #[must_use]
    pub fn parent(self) -> Option<Node<'doc>> {
        Node::wrap(self.node().parent)
    }

    /// The line the tokenizer saw the node on, from zero (php's patch
    /// 0001), which php's `getLineNo()` shows as it is.
    #[must_use]
    pub fn line(self) -> usize {
        self.node().line
    }

    /// The node's namespace, `node->ns`.
    #[must_use]
    pub fn namespace(self) -> Namespace {
        Namespace::from_raw(self.node().ns)
    }

    /// Whether the node has a prefix, `node->prefix != 0`.
    #[must_use]
    pub fn has_prefix(self) -> bool {
        self.node().prefix != 0
    }

    fn element(self) -> Option<*mut sys::lxb_dom_element_t> {
        (self.kind() == NodeKind::Element).then(|| self.ptr.as_ptr().cast())
    }

    /// An element's qualified name, `lxb_dom_element_qualified_name`; empty
    /// for any other node.
    #[must_use]
    pub fn qualified_name(self) -> &'doc [u8] {
        let Some(element) = self.element() else {
            return b"";
        };
        let mut len = 0usize;
        // SAFETY: a live element; the name is the document's.
        let name = unsafe { sys::lxb_dom_element_qualified_name(element, &raw mut len) };
        // SAFETY: `len` bytes at `name`, the document's.
        unsafe { bytes(name, len) }
    }

    /// Whether this is the HTML `template` element,
    /// `lxb_html_tree_node_is(node, LXB_TAG_TEMPLATE)`.
    #[must_use]
    pub fn is_template(self) -> bool {
        let n = self.node();
        n.local_name == sys::LXB_TAG_TEMPLATE && n.ns == sys::LXB_NS_HTML
    }

    /// A template element's content fragment,
    /// `lxb_html_interface_template(node)->content`.
    #[must_use]
    pub fn template_content(self) -> Option<Node<'doc>> {
        if !self.is_template() || self.kind() != NodeKind::Element {
            return None;
        }
        let template = self.ptr.as_ptr().cast::<sys::lxb_html_template_element_t>();
        // SAFETY: an HTML template element is a `lxb_html_template_element_t`.
        Node::wrap(unsafe { (*template).content })
    }

    /// An element's attributes in order.
    #[must_use]
    pub fn attributes(self) -> Attributes<'doc> {
        let first = self.element().map_or(std::ptr::null_mut(), |e| {
            // SAFETY: a live element.
            unsafe { (*e).first_attr }
        });
        Attributes {
            next: NonNull::new(first),
            doc: PhantomData,
        }
    }

    /// A text or comment node's data, `char_data.data`; empty for any
    /// other node.
    #[must_use]
    pub fn data(self) -> &'doc [u8] {
        if !matches!(
            self.kind(),
            NodeKind::Text | NodeKind::Comment | NodeKind::CdataSection
        ) {
            return b"";
        }
        let cd = self.ptr.as_ptr().cast::<sys::lxb_dom_character_data_t>();
        // SAFETY: a character data node is a `lxb_dom_character_data_t`.
        let data = unsafe { &(*cd).data };
        // SAFETY: `length` bytes at `data`, the document's.
        unsafe { bytes(data.data, data.length) }
    }

    fn doctype_field(
        self,
        read: unsafe extern "C" fn(*mut sys::lxb_dom_node_t, *mut usize) -> *const u8,
    ) -> &'doc [u8] {
        if self.kind() != NodeKind::DocumentType {
            return b"";
        }
        let mut len = 0usize;
        // SAFETY: a live doctype node.
        let p = unsafe { read(self.ptr.as_ptr(), &raw mut len) };
        // SAFETY: `len` bytes at `p`, the document's.
        unsafe { bytes(p, len) }
    }

    /// A doctype's name, `lxb_dom_document_type_name`.
    #[must_use]
    pub fn doctype_name(self) -> &'doc [u8] {
        self.doctype_field(sys::lxb_dom_document_type_name_noi)
    }

    /// A doctype's public id, empty where it has none.
    #[must_use]
    pub fn doctype_public_id(self) -> &'doc [u8] {
        self.doctype_field(sys::lxb_dom_document_type_public_id_noi)
    }

    /// A doctype's system id, empty where it has none.
    #[must_use]
    pub fn doctype_system_id(self) -> &'doc [u8] {
        self.doctype_field(sys::lxb_dom_document_type_system_id_noi)
    }
}

/// An element's attributes in order, `first_attr` along `next`.
pub struct Attributes<'doc> {
    next: Option<NonNull<sys::lxb_dom_attr_t>>,
    doc: PhantomData<&'doc Document>,
}

impl<'doc> Iterator for Attributes<'doc> {
    type Item = Attr<'doc>;

    fn next(&mut self) -> Option<Attr<'doc>> {
        let at = self.next?;
        // SAFETY: an attribute of a live element.
        self.next = NonNull::new(unsafe { (*at.as_ptr()).next });
        Some(Attr {
            ptr: at,
            doc: PhantomData,
        })
    }
}

/// One attribute of an element, borrowed for as long as the document.
#[derive(Debug, Clone, Copy)]
pub struct Attr<'doc> {
    ptr: NonNull<sys::lxb_dom_attr_t>,
    doc: PhantomData<&'doc Document>,
}

impl<'doc> Attr<'doc> {
    fn node(self) -> &'doc sys::lxb_dom_node_t {
        // SAFETY: an attribute of a live document; its head is a node.
        unsafe { &(*self.ptr.as_ptr()).node }
    }

    /// `lxb_dom_attr_qualified_name`: `prefix:name` as written.
    #[must_use]
    pub fn qualified_name(self) -> &'doc [u8] {
        let mut len = 0usize;
        // SAFETY: a live attribute.
        let name = unsafe { sys::lxb_dom_attr_qualified_name(self.ptr.as_ptr(), &raw mut len) };
        // SAFETY: `len` bytes at `name`, the document's.
        unsafe { bytes(name, len) }
    }

    /// Whether the attribute has a prefix, `attr->node.prefix != 0`.
    #[must_use]
    pub fn has_prefix(self) -> bool {
        self.node().prefix != 0
    }

    /// The attribute's namespace, `attr->node.ns`.
    #[must_use]
    pub fn namespace(self) -> Namespace {
        Namespace::from_raw(self.node().ns)
    }

    /// `lxb_dom_attr_value`: empty where it has none.
    #[must_use]
    pub fn value(self) -> &'doc [u8] {
        let mut len = 0usize;
        // SAFETY: a live attribute.
        let value = unsafe { sys::lxb_dom_attr_value_noi(self.ptr.as_ptr(), &raw mut len) };
        // SAFETY: `len` bytes at `value`, the document's.
        unsafe { bytes(value, len) }
    }
}

/// The entries of a `lexbor_array_obj_t` of `T` from `from` on.
pub(super) fn entries<'a, T: 'a>(
    array: *const sys::lexbor_array_obj_t,
    from: usize,
) -> impl Iterator<Item = &'a T> {
    // SAFETY: null or a live array of its owner.
    let array = unsafe { array.as_ref() };
    let (list, length, size) = array
        .map_or((std::ptr::null_mut::<std::ffi::c_void>(), 0, 0), |a| {
            (a.list, a.length, a.struct_size)
        });
    (from..length).filter_map(move |i| {
        if list.is_null() || size < size_of::<T>() {
            return None;
        }
        // SAFETY: `i < length` entries of `size` bytes each, each a `T`.
        let entry = unsafe { list.byte_add(i * size) };
        // SAFETY: as above.
        Some(unsafe { &*entry.cast::<T>() })
    })
}

/// `len` bytes at `p`, empty for a null pointer.
///
/// # Safety
/// `p` is null or points at `len` bytes that live for `'a`.
unsafe fn bytes<'a>(p: *const u8, len: usize) -> &'a [u8] {
    if p.is_null() {
        return b"";
    }
    // SAFETY: the caller's contract.
    unsafe { std::slice::from_raw_parts(p, len) }
}

macro_rules! error_ids {
    ($(#[$meta:meta])* $name:ident { $($variant:ident),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name {
            $($variant,)*
            /// An id past the ones php names.
            Other(u32),
        }

        impl $name {
            fn from_raw(raw: u32) -> $name {
                const ALL: &[$name] = &[$($name::$variant),*];
                usize::try_from(raw)
                    .ok()
                    .and_then(|i| ALL.get(i).copied())
                    .unwrap_or($name::Other(raw))
            }
        }
    };
}

error_ids! {
    /// `lxb_html_tokenizer_error_id_t`, in lexbor's order.
    TokenizerErrorId {
        AbruptClosingOfEmptyComment,
        AbruptDoctypePublicIdentifier,
        AbruptDoctypeSystemIdentifier,
        AbsenceOfDigitsInNumericCharacterReference,
        CdataInHtmlContent,
        CharacterReferenceOutsideUnicodeRange,
        ControlCharacterInInputStream,
        ControlCharacterReference,
        EndTagWithAttributes,
        DuplicateAttribute,
        EndTagWithTrailingSolidus,
        EofBeforeTagName,
        EofInCdata,
        EofInComment,
        EofInDoctype,
        EofInScriptHtmlCommentLikeText,
        EofInTag,
        IncorrectlyClosedComment,
        IncorrectlyOpenedComment,
        InvalidCharacterSequenceAfterDoctypeName,
        InvalidFirstCharacterOfTagName,
        MissingAttributeValue,
        MissingDoctypeName,
        MissingDoctypePublicIdentifier,
        MissingDoctypeSystemIdentifier,
        MissingEndTagName,
        MissingQuoteBeforeDoctypePublicIdentifier,
        MissingQuoteBeforeDoctypeSystemIdentifier,
        MissingSemicolonAfterCharacterReference,
        MissingWhitespaceAfterDoctypePublicKeyword,
        MissingWhitespaceAfterDoctypeSystemKeyword,
        MissingWhitespaceBeforeDoctypeName,
        MissingWhitespaceBetweenAttributes,
        MissingWhitespaceBetweenDoctypePublicAndSystemIdentifiers,
        NestedComment,
        NoncharacterCharacterReference,
        NoncharacterInInputStream,
        NonVoidHtmlElementStartTagWithTrailingSolidus,
        NullCharacterReference,
        SurrogateCharacterReference,
        SurrogateInInputStream,
        UnexpectedCharacterAfterDoctypeSystemIdentifier,
        UnexpectedCharacterInAttributeName,
        UnexpectedCharacterInUnquotedAttributeValue,
        UnexpectedEqualsSignBeforeAttributeName,
        UnexpectedNullCharacter,
        UnexpectedQuestionMarkInsteadOfTagName,
        UnexpectedSolidusInTag,
        UnknownNamedCharacterReference,
    }
}

error_ids! {
    /// `lxb_html_tree_error_id_t`, in lexbor's order, up to the last one
    /// php names.
    TreeErrorId {
        UnexpectedToken,
        UnexpectedClosedToken,
        NullCharacter,
        UnexpectedCharacterToken,
        UnexpectedTokenInInitialMode,
        BadDoctypeTokenInInitialMode,
        DoctypeTokenInBeforeHtmlMode,
        UnexpectedClosedTokenInBeforeHtmlMode,
        DoctypeTokenInBeforeHeadMode,
        UnexpectedClosedTokenInBeforeHeadMode,
        DoctypeTokenInHeadMode,
        NonVoidHtmlElementStartTagWithTrailingSolidus,
        HeadTokenInHeadMode,
        UnexpectedClosedTokenInHeadMode,
        TemplateClosedTokenWithoutOpeningInHeadMode,
        TemplateElementIsNotCurrentInHeadMode,
        DoctypeTokenInHeadNoscriptMode,
        DoctypeTokenAfterHeadMode,
        HeadTokenAfterHeadMode,
        DoctypeTokenInBodyMode,
        BadEndingOpenElementsIsWrong,
        OpenElementsIsWrong,
        UnexpectedElementInOpenElementsStack,
        MissingElementInOpenElementsStack,
        NoBodyElementInScope,
        MissingElementInScope,
        UnexpectedElementInScope,
        UnexpectedElementInActiveFormattingStack,
        UnexpectedEndOfFile,
        CharactersInTableText,
        DoctypeTokenInTableMode,
        DoctypeTokenInSelectMode,
        DoctypeTokenAfterBodyMode,
        DoctypeTokenInFramesetMode,
        DoctypeTokenAfterFramesetMode,
        DoctypeTokenForeignContentMode,
    }
}
