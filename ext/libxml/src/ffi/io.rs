//! Parsing, serialising, canonicalising and validating through libxml2,
//! each call wrapped in php's error capture ([`Raised::capture`]) so what
//! libxml raised comes back beside the answer for [`php_ext_libxml::report`]
//! to route.

use std::ffi::{c_char, c_int, c_void};
use std::ptr;

use super::{Alloc, Level, Library, Raised, Route, Sink, Tree, cstr, sys};

use super::tree::Node;

/// Where a parser reads from.
#[derive(Debug, Clone, Copy)]
pub enum Source<'a> {
    /// The bytes themselves, `xmlCreateMemoryParserCtxt`.
    Memory(&'a [u8]),
    /// A path libxml opens, `xmlCreateFileParserCtxt`.
    File(&'a [u8]),
}

/// What `dom_document_parser` answers.
#[derive(Debug, Clone, Copy)]
pub enum Parsed {
    /// No parser could be made: `NULL`.
    Nothing,
    /// The document was not well formed and recovery was off:
    /// `DOM_DOCUMENT_MALFORMED`, the tree already freed.
    Malformed,
    /// The document, the caller's to own; none where the parser made
    /// none.
    Document(Option<Node>),
}

/// `php_libxml_sanitize_parse_ctxt_options`.
fn sanitize(ctxt: &mut sys::xmlParserCtxt) {
    ctxt.loadsubset = 0;
    ctxt.validate = 0;
    ctxt.pedantic = 0;
    ctxt.replaceEntities = 0;
    ctxt.linenumbers = 0;
    ctxt.keepBlanks = 1;
    ctxt.options = 0;
}

/// The routes an error took, given the handlers php had put on the
/// parser when it came: libxml's own defaults before, php's after,
/// none for a level `LIBXML_NOERROR` or `LIBXML_NOWARNING` took the
/// handler of.
fn route(raised: &mut [Raised], configured: bool, options: c_int) {
    for error in raised {
        if error.route == Route::Generic {
            continue;
        }
        if !configured {
            error.route = Route::Default;
            continue;
        }
        let silenced = match error.level {
            Level::Warning => options & sys::XML_PARSE_NOWARNING != 0,
            Level::Error | Level::Fatal => options & sys::XML_PARSE_NOERROR != 0,
            Level::None => false,
        };
        if silenced {
            error.route = Route::Silent;
        }
    }
}

/// libxml2's parsers, and the `XInclude` pass over a parsed document.
pub struct Parser;

impl Parser {
    /// `dom_document_parser`: the XML parser over `source` with `options`,
    /// the directory a document read from memory takes as its base, and
    /// recovery on or off.
    pub fn xml(
        source: Source<'_>,
        options: c_int,
        directory: Option<&[u8]>,
        recover: bool,
    ) -> (Parsed, Vec<Raised>) {
        Library::init();
        let (ctxt, mut created) = Raised::capture(|| match source {
            Source::Memory(bytes) => {
                let len = c_int::try_from(bytes.len()).unwrap_or(c_int::MAX);
                // SAFETY: `len` readable bytes, copied by the context.
                unsafe { sys::xmlCreateMemoryParserCtxt(bytes.as_ptr().cast(), len) }
            }
            Source::File(path) => {
                let p = cstr(path);
                // SAFETY: a NUL-terminated path.
                unsafe { sys::xmlCreateFileParserCtxt(p.as_ptr().cast()) }
            }
        });
        route(&mut created, false, options);
        if ctxt.is_null() {
            return (Parsed::Nothing, created);
        }
        // SAFETY: the context just made, this function's alone until freed.
        let c = unsafe { &mut *ctxt };
        if let (Source::Memory(_), Some(dir)) = (source, directory) {
            let d = cstr(dir);
            // SAFETY: the context's own string, replaced.
            unsafe { Alloc::free(c.directory.cast()) };
            // SAFETY: a NUL-terminated path, canonicalised into a new string.
            c.directory = unsafe { sys::xmlCanonicPath(d.as_ptr()) }.cast::<c_char>();
        }
        sanitize(c);
        let (well_formed, mut parsed) = Raised::capture(|| {
            // SAFETY: a live context and options.
            unsafe { sys::xmlCtxtUseOptions(ctxt, options) };
            // SAFETY: a live context.
            unsafe { sys::xmlParseDocument(ctxt) };
            // SAFETY: the context, read after the parse.
            unsafe { (*ctxt).wellFormed != 0 }
        });
        route(&mut parsed, true, options);
        created.append(&mut parsed);
        // SAFETY: the context this owns.
        let c = unsafe { &mut *ctxt };
        let answer = if well_formed || recover {
            let doc = c.myDoc;
            if !doc.is_null() && !c.directory.is_null() {
                // SAFETY: the parsed document, whose URL is set once.
                let d = unsafe { &mut *doc };
                if d.URL.is_null() {
                    // SAFETY: the context's NUL-terminated directory.
                    d.URL = unsafe { sys::xmlStrdup(c.directory.cast::<u8>()) };
                }
            }
            Parsed::Document(Node::from_raw(doc.cast()))
        } else {
            // SAFETY: the malformed tree, freed once; the context forgets it.
            unsafe { sys::xmlFreeDoc(c.myDoc) };
            c.myDoc = ptr::null_mut();
            Parsed::Malformed
        };
        // SAFETY: the context, freed once.
        unsafe { sys::xmlFreeParserCtxt(ctxt) };
        (answer, created)
    }

    /// `dom_load_html`'s parse: the HTML parser over `source`, `options` put
    /// on the parser where any are given, and whatever tree it built.
    pub fn html(source: Source<'_>, options: c_int) -> (Option<Node>, Vec<Raised>) {
        Library::init();
        let (ctxt, mut created) = Raised::capture(|| match source {
            Source::Memory(bytes) => {
                let len = c_int::try_from(bytes.len()).unwrap_or(c_int::MAX);
                // SAFETY: `len` readable bytes.
                unsafe { sys::htmlCreateMemoryParserCtxt(bytes.as_ptr().cast(), len) }
            }
            Source::File(path) => {
                let p = cstr(path);
                // SAFETY: a NUL-terminated path and no encoding.
                unsafe { sys::htmlCreateFileParserCtxt(p.as_ptr().cast(), ptr::null()) }
            }
        });
        route(&mut created, false, options);
        if ctxt.is_null() {
            return (None, created);
        }
        // SAFETY: the context just made.
        sanitize(unsafe { &mut *ctxt });
        let (doc, mut parsed) = Raised::capture(|| {
            if options != 0 {
                // SAFETY: a live context.
                unsafe { sys::htmlCtxtUseOptions(ctxt, options) };
            }
            // SAFETY: a live context.
            unsafe { sys::htmlParseDocument(ctxt) };
            // SAFETY: the context, read after the parse.
            unsafe { (*ctxt).myDoc }
        });
        route(&mut parsed, true, options);
        created.append(&mut parsed);
        // SAFETY: the context, freed once; the tree is the caller's.
        unsafe { sys::htmlFreeParserCtxt(ctxt) };
        (Node::from_raw(doc.cast()), created)
    }

    /// `xmlParseBalancedChunkMemory(doc, NULL, NULL, 0, data, &list)`: the
    /// nodes, `Err` where the chunk was not balanced.
    pub fn chunk(doc: Option<Node>, data: &[u8]) -> (Result<Option<Node>, ()>, Vec<Raised>) {
        let d = cstr(data);
        let mut list: *mut sys::xmlNode = ptr::null_mut();
        let (status, raised) = Raised::capture(|| {
            // SAFETY: a live document or none, a NUL-terminated chunk and a
            // local out-pointer.
            unsafe {
                sys::xmlParseBalancedChunkMemory(
                    doc.map_or(ptr::null_mut(), |n| n.raw().cast()),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    0,
                    d.as_ptr(),
                    &raw mut list,
                )
            }
        });
        let answer = if status == 0 {
            Ok(Node::from_raw(list))
        } else {
            Err(())
        };
        (answer, raised)
    }

    /// `xmlXIncludeProcessFlags(doc, flags | XML_PARSE_NOXINCNODE)`.
    pub fn xinclude(doc: Node, flags: c_int) -> (i32, Vec<Raised>) {
        const NOXINCNODE: c_int = 1 << 15;
        Raised::capture(|| {
            // SAFETY: a live document.
            unsafe { sys::xmlXIncludeProcessFlags(doc.raw().cast(), flags | NOXINCNODE) }
        })
    }
}

/// `xmlSaveNoEmptyTags` set for one serialisation and restored after.
fn with_no_empty_tags<R>(on: bool, f: impl FnOnce() -> R) -> R {
    // SAFETY: no argument; the address of the thread's switch.
    let slot = unsafe { sys::__xmlSaveNoEmptyTags() };
    // SAFETY: the thread's own global, read and written on its thread.
    let old = unsafe { std::mem::replace(&mut *slot, c_int::from(on)) };
    let answer = f();
    // SAFETY: as above.
    unsafe { *slot = old };
    answer
}

/// libxml2's serialisers, each call wrapped in php's error capture.
pub struct Save;

impl Save {
    /// `dump_doc_to_str` with `xmlSaveNoEmptyTags` as `options` says.
    pub fn doc(
        doc: Node,
        options: c_int,
        no_empty: bool,
        encoding: Option<&[u8]>,
    ) -> (Option<Vec<u8>>, Vec<Raised>) {
        Raised::capture(|| {
            with_no_empty_tags(no_empty, || {
                // SAFETY: a live document.
                unsafe { Tree::dump_doc(doc.raw().cast(), options, encoding) }
            })
        })
    }

    /// `dump_node_to_str` with `xmlSaveNoEmptyTags` as `options` says.
    pub fn node(
        doc: Node,
        node: Node,
        format: bool,
        no_empty: bool,
        encoding: Option<&[u8]>,
    ) -> (Option<Vec<u8>>, Vec<Raised>) {
        Raised::capture(|| {
            with_no_empty_tags(no_empty, || {
                // SAFETY: a live document and a node of it.
                unsafe { Tree::dump_node(doc.raw().cast(), node.raw(), format, encoding) }
            })
        })
    }

    /// A node dumped with no document and no format, as
    /// `internalSubset` dumps each declaration of the subset.
    pub fn bare(node: Node) -> Option<Vec<u8>> {
        // SAFETY: a live node.
        unsafe { Tree::dump_node(ptr::null_mut(), node.raw(), false, None) }
    }

    /// `xmlSaveFormatFileEnc(file, doc, encoding, format)`: the bytes
    /// written, `-1` on failure.
    pub fn file(
        path: &[u8],
        doc: Node,
        encoding: Option<&[u8]>,
        format: bool,
        no_empty: bool,
    ) -> (i64, Vec<Raised>) {
        let p = cstr(path);
        let e = encoding.map(cstr);
        Raised::capture(|| {
            with_no_empty_tags(no_empty, || {
                // SAFETY: a NUL-terminated path, a live document, an encoding or
                // null.
                i64::from(unsafe {
                    sys::xmlSaveFormatFileEnc(
                        p.as_ptr().cast(),
                        doc.raw().cast(),
                        e.as_ref().map_or(ptr::null(), |e| e.as_ptr().cast()),
                        c_int::from(format),
                    )
                })
            })
        })
    }

    /// `htmlDocDumpMemoryFormat`.
    pub fn html_doc(doc: Node, format: bool) -> (Option<Vec<u8>>, Vec<Raised>) {
        Raised::capture(|| {
            let mut mem: *mut u8 = ptr::null_mut();
            let mut size: c_int = 0;
            // SAFETY: a live document and local out-pointers.
            unsafe {
                sys::htmlDocDumpMemoryFormat(
                    doc.raw().cast(),
                    &raw mut mem,
                    &raw mut size,
                    c_int::from(format),
                );
            }
            let len = usize::try_from(size).unwrap_or(0);
            let out = if mem.is_null() || len == 0 {
                None
            } else {
                // SAFETY: libxml answered `size` bytes at `mem`.
                Some(unsafe { std::slice::from_raw_parts(mem, len) }.to_vec())
            };
            // SAFETY: the buffer libxml allocated, freed once.
            unsafe { Alloc::free(mem.cast()) };
            out
        })
    }

    /// `htmlNodeDumpFormatOutput` of each node into one buffer.
    pub fn html_nodes(doc: Node, nodes: &[Node], format: bool) -> (Option<Vec<u8>>, Vec<Raised>) {
        Raised::capture(|| {
            let sink = Sink::new()?;
            for node in nodes {
                // SAFETY: a live buffer, document and node.
                unsafe {
                    sys::htmlNodeDumpFormatOutput(
                        sink.raw(),
                        doc.raw().cast(),
                        node.raw(),
                        ptr::null(),
                        c_int::from(format),
                    );
                }
            }
            sink.finish()
        })
    }

    /// `htmlSaveFileFormat(file, doc, encoding, format)`.
    pub fn html_file(
        path: &[u8],
        doc: Node,
        encoding: Option<&[u8]>,
        format: bool,
    ) -> (i64, Vec<Raised>) {
        let p = cstr(path);
        let e = encoding.map(cstr);
        Raised::capture(|| {
            // SAFETY: a NUL-terminated path, a live document, an encoding or
            // null.
            i64::from(unsafe {
                sys::htmlSaveFileFormat(
                    p.as_ptr().cast(),
                    doc.raw().cast(),
                    e.as_ref().map_or(ptr::null(), |e| e.as_ptr().cast()),
                    c_int::from(format),
                )
            })
        })
    }

    /// `htmlGetMetaEncoding`.
    pub fn meta_encoding(doc: Node) -> Option<Vec<u8>> {
        // SAFETY: a live HTML document.
        let p = unsafe { sys::htmlGetMetaEncoding(doc.raw().cast()) };
        // SAFETY: a string of the document, or null.
        (!p.is_null()).then(|| unsafe { Alloc::bytes(p) }.to_vec())
    }

    /// `xmlC14NExecute` or `xmlC14NDocSaveTo` into memory or a file.
    pub fn c14n(
        doc: Node,
        select: &C14nSelect<'_>,
        exclusive: bool,
        prefixes: Option<&[Vec<u8>]>,
        with_comments: bool,
        target: &C14nTarget<'_>,
    ) -> (C14nOut, Vec<Raised>) {
        let owned: Vec<Vec<u8>> = prefixes
            .unwrap_or_default()
            .iter()
            .map(|p| cstr(p))
            .collect();
        let mut table: Vec<*mut u8> = owned.iter().map(|p| p.as_ptr().cast_mut()).collect();
        table.push(ptr::null_mut());
        let prefix_table = if prefixes.is_some() {
            table.as_mut_ptr()
        } else {
            ptr::null_mut()
        };
        Raised::capture(|| {
            let run = |buf: *mut sys::xmlOutputBuffer| -> c_int {
                match select {
                    C14nSelect::Subtree(root) => {
                        // SAFETY: a live document and buffer; the callback reads
                        // the root it is handed and the ancestor chains libxml
                        // walks.
                        unsafe {
                            sys::xmlC14NExecute(
                                doc.raw().cast(),
                                Some(subtree_visible),
                                root.raw().cast(),
                                c_int::from(exclusive),
                                prefix_table,
                                c_int::from(with_comments),
                                buf,
                            )
                        }
                    }
                    C14nSelect::Document | C14nSelect::NodeSet(_) => {
                        let set = match select {
                            C14nSelect::NodeSet(object) => {
                                object.node_set().unwrap_or(ptr::null_mut())
                            }
                            _ => ptr::null_mut(),
                        };
                        // SAFETY: a live document, a node set of it or null,
                        // and a live buffer.
                        unsafe {
                            sys::xmlC14NDocSaveTo(
                                doc.raw().cast(),
                                set,
                                c_int::from(exclusive),
                                prefix_table,
                                c_int::from(with_comments),
                                buf,
                            )
                        }
                    }
                }
            };
            match target {
                C14nTarget::Memory => {
                    let Some(sink) = Sink::new() else {
                        return C14nOut::Failed;
                    };
                    if run(sink.raw()) < 0 {
                        return C14nOut::Failed;
                    }
                    sink.finish().map_or(C14nOut::Failed, C14nOut::Bytes)
                }
                C14nTarget::File(path) => {
                    let p = cstr(path);
                    // SAFETY: a NUL-terminated path, no encoder.
                    let buf = unsafe {
                        sys::xmlOutputBufferCreateFilename(p.as_ptr().cast(), ptr::null_mut(), 0)
                    };
                    if buf.is_null() {
                        return C14nOut::Failed;
                    }
                    let status = run(buf);
                    // SAFETY: the buffer, closed once; it answers the bytes it
                    // wrote.
                    let written = unsafe { sys::xmlOutputBufferClose(buf) };
                    if status < 0 {
                        C14nOut::Failed
                    } else {
                        C14nOut::Written(i64::from(written))
                    }
                }
            }
        })
    }
}

/// Where the canonical bytes go.
pub enum C14nTarget<'a> {
    Memory,
    File(&'a [u8]),
}

/// The nodes C14N includes.
pub enum C14nSelect<'a> {
    /// Every node of the document.
    Document,
    /// The node and what hangs from it, `dom_canonicalize_node_parent_lookup_cb`.
    Subtree(Node),
    /// The node set an `XPath` query answered.
    NodeSet(&'a super::Object),
}

unsafe extern "C" fn subtree_visible(
    user: *mut c_void,
    node: *mut sys::xmlNode,
    parent: *mut sys::xmlNode,
) -> c_int {
    let root = user.cast::<sys::xmlNode>();
    if node == root {
        return 1;
    }
    let mut at = parent;
    while !at.is_null() {
        if at == root {
            return 1;
        }
        // SAFETY: a live ancestor chain of the document being written.
        at = unsafe { (*at).parent };
    }
    0
}

/// What C14N answered: the bytes for memory, the count for a file.
pub enum C14nOut {
    Bytes(Vec<u8>),
    Written(i64),
    Failed,
}

/// libxml2's validators over a document.
pub struct Validate;

impl Validate {
    /// `xmlValidateDocument` with a fresh validator.
    pub fn dtd(doc: Node) -> (bool, Vec<Raised>) {
        Raised::capture(|| {
            // SAFETY: no argument.
            let cvp = unsafe { sys::xmlNewValidCtxt() };
            if cvp.is_null() {
                return false;
            }
            // SAFETY: a live context and document.
            let valid = unsafe { sys::xmlValidateDocument(cvp, doc.raw().cast()) } != 0;
            // SAFETY: the context, freed once.
            unsafe { sys::xmlFreeValidCtxt(cvp) };
            valid
        })
    }

    /// `dom_document_schema_validate` and `dom_document_relaxNG_validate`'s
    /// libxml half over a schema read from `source`.
    pub fn schema(
        doc: Node,
        schema: Schema,
        source: Source<'_>,
        flags: c_int,
    ) -> (Validated, Vec<Raised>) {
        Raised::capture(|| {
            let parser = match (schema, source) {
                (Schema::Xsd, Source::File(path)) => {
                    let p = cstr(path);
                    // SAFETY: a NUL-terminated path.
                    unsafe { sys::xmlSchemaNewParserCtxt(p.as_ptr().cast()) }
                }
                (Schema::Xsd, Source::Memory(bytes)) => {
                    let len = c_int::try_from(bytes.len()).unwrap_or(c_int::MAX);
                    // SAFETY: `len` readable bytes.
                    unsafe { sys::xmlSchemaNewMemParserCtxt(bytes.as_ptr().cast(), len) }
                }
                (Schema::RelaxNg, Source::File(path)) => {
                    let p = cstr(path);
                    // SAFETY: a NUL-terminated path.
                    unsafe { sys::xmlRelaxNGNewParserCtxt(p.as_ptr().cast()) }
                }
                (Schema::RelaxNg, Source::Memory(bytes)) => {
                    let len = c_int::try_from(bytes.len()).unwrap_or(c_int::MAX);
                    // SAFETY: `len` readable bytes.
                    unsafe { sys::xmlRelaxNGNewMemParserCtxt(bytes.as_ptr().cast(), len) }
                }
            };
            if parser.is_null() {
                return Validated::InvalidSchema;
            }
            let parsed = match schema {
                Schema::Xsd => {
                    // SAFETY: a live parser context, freed once after.
                    let s = unsafe { sys::xmlSchemaParse(parser) };
                    // SAFETY: as above.
                    unsafe { sys::xmlSchemaFreeParserCtxt(parser) };
                    s
                }
                Schema::RelaxNg => {
                    // SAFETY: as above.
                    let s = unsafe { sys::xmlRelaxNGParse(parser) };
                    // SAFETY: as above.
                    unsafe { sys::xmlRelaxNGFreeParserCtxt(parser) };
                    s
                }
            };
            if parsed.is_null() {
                return Validated::InvalidSchema;
            }
            let valid = match schema {
                Schema::Xsd => {
                    // SAFETY: a live schema.
                    let v = unsafe { sys::xmlSchemaNewValidCtxt(parsed) };
                    if v.is_null() {
                        // SAFETY: the schema, freed once.
                        unsafe { sys::xmlSchemaFree(parsed) };
                        return Validated::Unusable;
                    }
                    // SAFETY: a live validation context.
                    unsafe { sys::xmlSchemaSetValidOptions(v, flags & 1) };
                    // SAFETY: a live context and document.
                    let r = unsafe { sys::xmlSchemaValidateDoc(v, doc.raw().cast()) };
                    // SAFETY: both freed once.
                    unsafe { sys::xmlSchemaFree(parsed) };
                    // SAFETY: as above.
                    unsafe { sys::xmlSchemaFreeValidCtxt(v) };
                    r == 0
                }
                Schema::RelaxNg => {
                    // SAFETY: a live schema.
                    let v = unsafe { sys::xmlRelaxNGNewValidCtxt(parsed) };
                    if v.is_null() {
                        // SAFETY: the schema, freed once.
                        unsafe { sys::xmlRelaxNGFree(parsed) };
                        return Validated::Unusable;
                    }
                    // SAFETY: a live context and document.
                    let r = unsafe { sys::xmlRelaxNGValidateDoc(v, doc.raw().cast()) };
                    // SAFETY: both freed once.
                    unsafe { sys::xmlRelaxNGFree(parsed) };
                    // SAFETY: as above.
                    unsafe { sys::xmlRelaxNGFreeValidCtxt(v) };
                    r == 0
                }
            };
            Validated::Done(valid)
        })
    }
}

/// What a schema validation answered.
pub enum Validated {
    /// The schema would not parse: php's "Invalid Schema".
    InvalidSchema,
    /// No validator: php's `Error`.
    Unusable,
    /// Whether the document is valid.
    Done(bool),
}

/// Which schema language.
#[derive(Clone, Copy)]
pub enum Schema {
    Xsd,
    RelaxNg,
}
