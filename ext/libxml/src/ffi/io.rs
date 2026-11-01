//! Parsing, serialising, canonicalising and validating through libxml2,
//! each call wrapped in php's error capture ([`Raised::capture`]) so what
//! libxml raised comes back beside the answer for [`php_ext_libxml::report`]
//! to route.

use std::ffi::{c_char, c_int, c_void};
use std::ptr;

use php_ext::raising::Thrown;
use php_ext::request::Request;

use super::{Alloc, Library, Raised, Routing, Sink, Tree, cstr, sys};

use super::tree::{NewDoc, Node};

/// Where a parser reads from.
#[derive(Debug, Clone, Copy)]
pub enum Source<'a> {
    /// The bytes themselves, `xmlCreateMemoryParserCtxt`.
    Memory(&'a [u8]),
    /// A path libxml opens, `xmlCreateFileParserCtxt`.
    File(&'a [u8]),
}

/// What `dom_document_parser` answers.
#[derive(Debug)]
pub enum Parsed {
    /// No parser could be made: `NULL`.
    Nothing,
    /// The document was not well formed and recovery was off:
    /// `DOM_DOCUMENT_MALFORMED`, the tree already freed.
    Malformed,
    /// The document, the caller's to own; none where the parser made
    /// none.
    Document(Option<NewDoc>),
}

/// `php_libxml_sanitize_parse_ctxt_options`.
pub(super) fn sanitize(ctxt: &mut sys::xmlParserCtxt) {
    ctxt.loadsubset = 0;
    ctxt.validate = 0;
    ctxt.pedantic = 0;
    ctxt.replaceEntities = 0;
    ctxt.linenumbers = 0;
    ctxt.keepBlanks = 1;
    ctxt.options = 0;
}

/// `PHP_LIBXML_SANITIZE_GLOBALS` and `PHP_LIBXML_RESTORE_GLOBALS`
/// (`Z/ext/libxml/php_libxml.h:237-257`) around `f`: the calling thread's
/// deprecated parse defaults, which libxml2 copies into each parser it
/// makes, set to php's for the call (no external subset, no validation,
/// nothing pedantic, entities kept, no line numbers, blanks kept) and put
/// back after, so a library that left them otherwise, or
/// `zend_test_override_libxml_global_state`, reaches no parse of php's
/// that makes its parser inside libxml (GHSA-3qrf-m4j2-pcrr). A parser
/// php makes itself is [`sanitize`]d instead.
pub(super) fn sanitized_globals<R>(f: impl FnOnce() -> R) -> R {
    // SAFETY: the address of the thread's own default.
    let load = unsafe { sys::__xmlLoadExtDtdDefaultValue() };
    // SAFETY: as above.
    let validity = unsafe { sys::__xmlDoValidityCheckingDefaultValue() };
    // SAFETY: the thread's own global, read by copy.
    let old_load = unsafe { *load };
    // SAFETY: as above, written.
    unsafe { *load = 0 };
    // SAFETY: as above, read by copy.
    let old_validity = unsafe { *validity };
    // SAFETY: as above, written.
    unsafe { *validity = 0 };
    // SAFETY: no pointer argument; each the thread's own default, the old
    // value answered.
    let pedantic = unsafe { sys::xmlPedanticParserDefault(0) };
    // SAFETY: as above.
    let substitute = unsafe { sys::xmlSubstituteEntitiesDefault(0) };
    // SAFETY: as above.
    let line_numbers = unsafe { sys::xmlLineNumbersDefault(0) };
    // SAFETY: as above.
    let blanks = unsafe { sys::xmlKeepBlanksDefault(1) };
    let answer = f();
    // SAFETY: the thread's own globals, put back as they were.
    unsafe { *load = old_load };
    // SAFETY: as above.
    unsafe { *validity = old_validity };
    // SAFETY: as above, through the setters.
    let _ = unsafe { sys::xmlPedanticParserDefault(pedantic) };
    // SAFETY: as above.
    let _ = unsafe { sys::xmlSubstituteEntitiesDefault(substitute) };
    // SAFETY: as above.
    let _ = unsafe { sys::xmlLineNumbersDefault(line_numbers) };
    // SAFETY: as above.
    let _ = unsafe { sys::xmlKeepBlanksDefault(blanks) };
    answer
}

/// libxml2's parsers, and the `XInclude` pass over a parsed document.
pub struct Parser;

impl Parser {
    /// `dom_document_parser`: the XML parser over `source` with `options`,
    /// the directory a document read from memory takes as its base,
    /// recovery on or off, and the encoding the caller put in place of the
    /// document's own (`xmlSwitchToEncoding`), which the caller has already
    /// found known. What the parser loads, the file itself included, reads
    /// through `req`'s streams ([`Raised::capture_io`]).
    pub fn xml(
        req: &mut Request,
        source: Source<'_>,
        options: c_int,
        directory: Option<&[u8]>,
        recover: bool,
        encoding: Option<&[u8]>,
    ) -> (Parsed, Vec<Raised>, Option<Thrown>) {
        Library::init();
        Raised::capture_io(req, None, || {
            let created = Routing::Parser {
                sax: ptr::null_mut(),
            };
            let ctxt = Raised::phase(created, || match source {
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
            if ctxt.is_null() {
                return Parsed::Nothing;
            }
            if let Some(name) = encoding {
                let n = cstr(name);
                // SAFETY: a NUL-terminated name.
                let handler = unsafe { sys::xmlFindCharEncodingHandler(n.as_ptr().cast()) };
                if !handler.is_null() {
                    // SAFETY: a live context and a handler it takes over.
                    unsafe { sys::xmlSwitchToEncoding(ctxt, handler) };
                }
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
            // `ctxt->sax->error = php_libxml_ctx_error`: from here on the
            // parser's errors reach php's handlers.
            // SAFETY: the live context.
            let sax = unsafe { (*ctxt).sax };
            let parsing = Routing::Parser { sax };
            let well_formed = Raised::phase(parsing, || {
                // SAFETY: a live context and options.
                unsafe { sys::xmlCtxtUseOptions(ctxt, options) };
                // SAFETY: a live context.
                unsafe { sys::xmlParseDocument(ctxt) };
                // SAFETY: the context, read after the parse.
                unsafe { (*ctxt).wellFormed != 0 }
            });
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
                // SAFETY: the tree the parser made, the caller's alone.
                Parsed::Document(unsafe { NewDoc::from_ptr(doc.cast()) })
            } else {
                // SAFETY: the malformed tree, freed once; the context forgets it.
                unsafe { sys::xmlFreeDoc(c.myDoc) };
                c.myDoc = ptr::null_mut();
                Parsed::Malformed
            };
            // SAFETY: the context, freed once.
            unsafe { sys::xmlFreeParserCtxt(ctxt) };
            answer
        })
    }

    /// `xmlReadMemory(data, len, NULL, NULL, options)` or `xmlReadFile(path,
    /// NULL, options)`, simplexml's parse: a parser php never configures,
    /// so what it raises is libxml's own text through the generic
    /// handler, and a tree only where it was well formed or `options`
    /// recover. What it loads reads through `req`'s streams.
    pub fn read(
        req: &mut Request,
        source: Source<'_>,
        options: c_int,
    ) -> (Option<NewDoc>, Vec<Raised>, Option<Thrown>) {
        Library::init();
        let unconfigured = Routing::Parser {
            sax: ptr::null_mut(),
        };
        Raised::capture_io(req, Some(unconfigured), || {
            let doc = sanitized_globals(|| match source {
                Source::Memory(bytes) => {
                    let len = c_int::try_from(bytes.len()).unwrap_or(c_int::MAX);
                    // SAFETY: `len` readable bytes, no base URL and no encoding.
                    unsafe {
                        sys::xmlReadMemory(
                            bytes.as_ptr().cast(),
                            len,
                            ptr::null(),
                            ptr::null(),
                            options,
                        )
                    }
                }
                Source::File(path) => {
                    let p = cstr(path);
                    // SAFETY: a NUL-terminated path and no encoding.
                    unsafe { sys::xmlReadFile(p.as_ptr().cast(), ptr::null(), options) }
                }
            });
            // SAFETY: the tree `xmlReadMemory` or `xmlReadFile` made, the
            // caller's alone.
            unsafe { NewDoc::from_ptr(doc.cast()) }
        })
    }

    /// `soap_xmlParse_ex` over `xmlCreateMemoryParserCtxt` or
    /// `xmlCreateFileParserCtxt`, php's soap parse: the parser sanitised,
    /// blanks dropped (`keepBlanks = 0`, `ignorableWhitespace` and
    /// `comment` set to nothing), its warnings and errors silenced,
    /// `XML_PARSE_HUGE`, and the document parsed with the entity loader
    /// disabled; the tree where it was well formed, its `URL` the
    /// parser's directory where it had none, and nothing otherwise. The
    /// file is opened through `req`'s streams before the loader is
    /// disabled, as php creates the parser first.
    pub fn soap(
        req: &mut Request,
        source: Source<'_>,
    ) -> (Option<Node>, Vec<Raised>, Option<Thrown>) {
        Library::init();
        let created = Routing::Parser {
            sax: ptr::null_mut(),
        };
        let (ctxt, mut raised, thrown) = Raised::capture_io(req, Some(created), || match source {
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
        if ctxt.is_null() || thrown.is_some() {
            if !ctxt.is_null() {
                let ((), more, _) = Raised::capture_io(req, None, || {
                    // SAFETY: the context made above, freed once, its input
                    // closed through `req`'s streams.
                    unsafe { sys::xmlFreeParserCtxt(ctxt) };
                });
                raised.extend(more);
            }
            return (None, raised, thrown);
        }
        // SAFETY: the context just made, this function's alone until freed.
        let c = unsafe { &mut *ctxt };
        sanitize(c);
        c.keepBlanks = 0;
        c.options |= sys::XML_PARSE_HUGE;
        // SAFETY: the context's own handler table.
        let sax = unsafe { &mut *c.sax };
        sax.callbacks[IGNORABLE_WHITESPACE] = soap_ignorable_whitespace as *mut c_void;
        sax.callbacks[COMMENT] = soap_comment as *mut c_void;
        sax.warning = ptr::null_mut();
        sax.error = ptr::null_mut();
        let old = std::mem::replace(
            &mut req.state::<crate::State>().entity_loader_disabled,
            true,
        );
        let parsing = Routing::Parser { sax: c.sax };
        // The parser is freed inside the capture, so the input it opened
        // through `req`'s streams is closed through them.
        let (answer, more, thrown) = Raised::capture_io(req, Some(parsing), || {
            // SAFETY: a live context.
            unsafe { sys::xmlParseDocument(ctxt) };
            // SAFETY: the context this owns, read after the parse.
            let c = unsafe { &mut *ctxt };
            let answer = if c.wellFormed != 0 {
                let doc = c.myDoc;
                if !doc.is_null() && !c.directory.is_null() {
                    // SAFETY: the parsed document, whose URL is set once.
                    let d = unsafe { &mut *doc };
                    if d.URL.is_null() {
                        // SAFETY: the context's NUL-terminated directory.
                        d.URL = unsafe { sys::xmlStrdup(c.directory.cast::<u8>()) };
                    }
                }
                Node::from_raw(doc.cast())
            } else {
                // SAFETY: the malformed tree, freed once; the context forgets it.
                unsafe { sys::xmlFreeDoc(c.myDoc) };
                c.myDoc = ptr::null_mut();
                None
            };
            // SAFETY: the context, freed once.
            unsafe { sys::xmlFreeParserCtxt(ctxt) };
            answer
        });
        req.state::<crate::State>().entity_loader_disabled = old;
        raised.extend(more);
        (answer, raised, thrown)
    }

    /// `dom_load_html`'s parse: the HTML parser over `source`, `options` put
    /// on the parser where any are given, and whatever tree it built; what
    /// it loads reads through `req`'s streams.
    pub fn html(
        req: &mut Request,
        source: Source<'_>,
        options: c_int,
    ) -> (Option<NewDoc>, Vec<Raised>, Option<Thrown>) {
        Library::init();
        Raised::capture_io(req, None, || {
            let created = Routing::Parser {
                sax: ptr::null_mut(),
            };
            let ctxt = Raised::phase(created, || match source {
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
            if ctxt.is_null() {
                return None;
            }
            // SAFETY: the context just made.
            sanitize(unsafe { &mut *ctxt });
            // `ctxt->sax->error = php_libxml_ctx_error`: from here on the
            // parser's errors reach php's handlers.
            // SAFETY: the live context.
            let sax = unsafe { (*ctxt).sax };
            let parsing = Routing::Parser { sax };
            let doc = Raised::phase(parsing, || {
                if options != 0 {
                    // SAFETY: a live context.
                    unsafe { sys::htmlCtxtUseOptions(ctxt, options) };
                }
                // SAFETY: a live context.
                unsafe { sys::htmlParseDocument(ctxt) };
                // SAFETY: the context, read after the parse.
                unsafe { (*ctxt).myDoc }
            });
            // SAFETY: the context, freed once; the tree is the caller's.
            unsafe { sys::htmlFreeParserCtxt(ctxt) };
            // SAFETY: the tree the parser made, the caller's alone.
            unsafe { NewDoc::from_ptr(doc.cast()) }
        })
    }

    /// `xmlParseBalancedChunkMemory(doc, NULL, NULL, 0, data, &list)`: the
    /// nodes, `Err` where the chunk was not balanced.
    pub fn chunk(doc: Option<Node>, data: &[u8]) -> (Result<Option<Node>, ()>, Vec<Raised>) {
        let d = cstr(data);
        let mut list: *mut sys::xmlNode = ptr::null_mut();
        let (status, raised) = Raised::capture(|| {
            sanitized_globals(|| {
                // SAFETY: a live document or none, a NUL-terminated chunk
                // and a local out-pointer.
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
            })
        });
        let answer = if status == 0 {
            Ok(Node::from_raw(list))
        } else {
            Err(())
        };
        (answer, raised)
    }

    /// `xmlXIncludeProcessFlags(doc, flags | XML_PARSE_NOXINCNODE)`, what it
    /// includes read through `req`'s streams; the parsers libxml opens for
    /// an include are its own, never configured by php, so their errors
    /// are printed in libxml's own words.
    pub fn xinclude(
        req: &mut Request,
        doc: Node,
        flags: c_int,
    ) -> (i32, Vec<Raised>, Option<Thrown>) {
        const NOXINCNODE: c_int = 1 << 15;
        let unconfigured = Routing::Parser {
            sax: ptr::null_mut(),
        };
        Raised::capture_io(req, Some(unconfigured), || {
            sanitized_globals(|| {
                // SAFETY: a live document.
                unsafe { sys::xmlXIncludeProcessFlags(doc.raw().cast(), flags | NOXINCNODE) }
            })
        })
    }
}

/// `xmlSAXHandler.ignorableWhitespace` and `.comment`, by their place
/// among the handler table's first callbacks.
const IGNORABLE_WHITESPACE: usize = 18;
const COMMENT: usize = 20;

/// `soap_ignorableWhitespace`: nothing.
extern "C" fn soap_ignorable_whitespace(_ctx: *mut c_void, _ch: *const u8, _len: c_int) {}

/// `soap_Comment`: nothing.
extern "C" fn soap_comment(_ctx: *mut c_void, _value: *const u8) {}

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
    /// written, `-1` on failure; the file opened through `req`'s wrappers.
    pub fn file(
        req: &mut Request,
        path: &[u8],
        doc: Node,
        encoding: Option<&[u8]>,
        format: bool,
        no_empty: bool,
    ) -> (i64, Vec<Raised>, Option<Thrown>) {
        let p = cstr(path);
        let e = encoding.map(cstr);
        Raised::capture_io(req, None, || {
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

    /// `php_libxml_dump_node_to_file`: `node` dumped with `xmlNodeDumpOutput`
    /// into `xmlOutputBufferCreateFilename(file)`; what closing the buffer
    /// answers, `-1` where the file could not be opened. The file opens
    /// through `req`'s wrappers.
    pub fn node_file(
        req: &mut Request,
        path: &[u8],
        doc: Node,
        node: Node,
        format: bool,
    ) -> (i64, Vec<Raised>, Option<Thrown>) {
        let p = cstr(path);
        Raised::capture_io(req, None, || {
            // SAFETY: a NUL-terminated path, no encoder and no compression.
            let out = unsafe {
                sys::xmlOutputBufferCreateFilename(p.as_ptr().cast(), ptr::null_mut(), 0)
            };
            if out.is_null() {
                return -1;
            }
            // SAFETY: the buffer just made, a live document and a node of it.
            unsafe {
                sys::xmlNodeDumpOutput(
                    out,
                    doc.raw().cast(),
                    node.raw(),
                    0,
                    c_int::from(format),
                    ptr::null(),
                );
            }
            // SAFETY: the buffer, closed once.
            i64::from(unsafe { sys::xmlOutputBufferClose(out) })
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

    /// `htmlSaveFileFormat(file, doc, encoding, format)`, the file opened
    /// through `req`'s wrappers.
    pub fn html_file(
        req: &mut Request,
        path: &[u8],
        doc: Node,
        encoding: Option<&[u8]>,
        format: bool,
    ) -> (i64, Vec<Raised>, Option<Thrown>) {
        let p = cstr(path);
        let e = encoding.map(cstr);
        Raised::capture_io(req, None, || {
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

    /// `xmlC14NExecute` or `xmlC14NDocSaveTo` into memory or a file, a file
    /// opened through `req`'s wrappers.
    pub fn c14n(
        req: &mut Request,
        doc: Node,
        select: &C14nSelect<'_>,
        exclusive: bool,
        prefixes: Option<&[Vec<u8>]>,
        with_comments: bool,
        target: &C14nTarget<'_>,
    ) -> (C14nOut, Vec<Raised>, Option<Thrown>) {
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
        Raised::capture_io(req, None, || {
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
    /// `xmlValidateDocument` with a fresh validator, the external subset
    /// it loads read through `req`'s streams.
    pub fn dtd(req: &mut Request, doc: Node) -> (bool, Vec<Raised>, Option<Thrown>) {
        Raised::capture_io(req, Some(Routing::Validator), || {
            sanitized_globals(|| {
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
        })
    }

    /// `dom_document_schema_validate` and `dom_document_relaxNG_validate`'s
    /// libxml half over a schema read from `source`, what it loads read
    /// through `req`'s streams.
    pub fn schema(
        req: &mut Request,
        doc: Node,
        schema: Schema,
        source: Source<'_>,
        flags: c_int,
    ) -> (Validated, Vec<Raised>, Option<Thrown>) {
        Raised::capture_io(req, Some(Routing::Validator), || {
            sanitized_globals(|| {
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

/// What `dom_xml_fragment_parsing_algorithm` had libxml parse.
pub enum Fragment {
    /// `xmlCreatePushParserCtxt` made no parser.
    NoParser,
    /// The markup was not well formed, or not namespace well formed: the
    /// tree is already freed.
    Malformed,
    /// The document the parser built, freed as it drops, its dictionary
    /// unset since that one belongs to the document the markup was parsed
    /// for; none where the parser built none.
    Document(Option<NewDoc>),
}

/// `XML_MAX_DICTIONARY_LIMIT`.
const MAX_DICTIONARY_LIMIT: usize = 10_000_000;

impl Parser {
    /// `dom_xml_fragment_parsing_algorithm`'s steps 1 to 5: a push parser
    /// whose dictionary is `doc`'s (made the first time), php's sanitised
    /// options with `XML_PARSE_IGNORE_ENC | XML_PARSE_NOERROR |
    /// XML_PARSE_NOWARNING`, UTF-8 switched in, fed `chunks` in order with
    /// the last one terminating the parse.
    pub fn fragment(doc: Node, chunks: &[&[u8]]) -> (Fragment, Vec<Raised>) {
        const OPTIONS: c_int =
            sys::XML_PARSE_IGNORE_ENC | sys::XML_PARSE_NOERROR | sys::XML_PARSE_NOWARNING;
        Library::init();
        // Every error of the parser php configures here reaches no handler,
        // `XML_PARSE_NOERROR | XML_PARSE_NOWARNING` having taken them off.
        let unconfigured = Routing::Parser {
            sax: ptr::null_mut(),
        };
        let (answer, raised) = Raised::capture_with(unconfigured, || {
            // SAFETY: no SAX handler, no user data, no initial chunk.
            let ctxt = unsafe {
                sys::xmlCreatePushParserCtxt(
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                    0,
                    ptr::null(),
                )
            };
            if ctxt.is_null() {
                return Fragment::NoParser;
            }
            // SAFETY: the context just made, this function's alone.
            let c = unsafe { &mut *ctxt };
            // SAFETY: the context's own dictionary, replaced below.
            unsafe { sys::xmlDictFree(c.dict) };
            // SAFETY: a live document; its dictionary is read and set once.
            let d = unsafe { &mut *doc.raw().cast::<sys::xmlDoc>() };
            if d.dict.is_null() {
                // SAFETY: no argument.
                d.dict = unsafe { sys::xmlDictCreate() };
                // SAFETY: the dictionary just made.
                unsafe { sys::xmlDictSetLimit(d.dict, MAX_DICTIONARY_LIMIT) };
            }
            c.dict = d.dict;
            sanitize(c);
            // SAFETY: a live context and options.
            unsafe { sys::xmlCtxtUseOptions(ctxt, OPTIONS) };
            // SAFETY: a NUL-terminated name.
            let utf8 = unsafe { sys::xmlFindCharEncodingHandler(c"UTF-8".as_ptr()) };
            // SAFETY: a live context and the handler it takes over.
            unsafe { sys::xmlSwitchToEncoding(ctxt, utf8) };
            let last = chunks.len().saturating_sub(1);
            for (i, chunk) in chunks.iter().enumerate() {
                let len = c_int::try_from(chunk.len()).unwrap_or(c_int::MAX);
                // SAFETY: a live context and `len` readable bytes.
                unsafe {
                    sys::xmlParseChunk(ctxt, chunk.as_ptr().cast(), len, c_int::from(i == last))
                };
            }
            // SAFETY: the context, read after the parse.
            let c = unsafe { &mut *ctxt };
            if c.wellFormed == 0 || c.nsWellFormed == 0 {
                c.dict = ptr::null_mut();
                // SAFETY: the tree the parser built, freed once.
                unsafe { sys::xmlFreeDoc(c.myDoc) };
                c.myDoc = ptr::null_mut();
                // SAFETY: the context, freed once, the dictionary not with it.
                unsafe { sys::xmlFreeParserCtxt(ctxt) };
                return Fragment::Malformed;
            }
            let built = c.myDoc;
            // SAFETY: the context, freed once; its dictionary reference is
            // the one the built document took.
            unsafe { sys::xmlFreeParserCtxt(ctxt) };
            if !built.is_null() {
                // SAFETY: the built document, whose dictionary is the context
                // document's and is not freed with it.
                unsafe { (*built).dict = ptr::null_mut() };
            }
            // SAFETY: the built document or null, no reference counting it
            // and the context that made it freed above.
            Fragment::Document(unsafe { NewDoc::from_ptr(built.cast()) })
        });
        (answer, raised)
    }
}

/// The two writers of `php_new_dom_dump_node_to_str_ex`, both appending to
/// one string: `xmlSaveToIO`'s `xmlSaveCtxt`, for what php hands libxml's own
/// serialiser (a document's declaration, a DTD, an entity reference), and
/// `xmlOutputBufferCreateIO`'s buffer, for what dom's XML serialiser
/// writes. Each converts into the document's encoding with an encoder of
/// its own, as php's do, so what either one starts its bytes with (a
/// UTF-16 byte order mark) is written where php writes it.
pub struct Dump {
    ctxt: *mut sys::xmlSaveCtxt,
    out: *mut sys::xmlOutputBuffer,
}

impl Dump {
    /// The pair laid down for `encoding` (none: no conversion) with the
    /// save `options`, `body` handed it, then the buffer flushed and
    /// closed and the `xmlSaveCtxt` closed: the bytes where every step
    /// succeeded, and what libxml raised.
    pub fn serialise(
        encoding: Option<&[u8]>,
        options: c_int,
        body: impl FnOnce(&mut Dump) -> bool,
    ) -> (Option<Vec<u8>>, Vec<Raised>) {
        Library::init();
        let mut bytes: Vec<u8> = Vec::new();
        let into: *mut c_void = ptr::from_mut(&mut bytes).cast();
        let name = encoding.map(cstr);
        let enc = name
            .as_ref()
            .map_or(ptr::null(), |n| n.as_ptr().cast::<c_char>());
        let (ok, raised) = Raised::capture(|| {
            // SAFETY: `bytes` outlives the context, closed below; an encoding
            // name or null.
            let ctxt = unsafe { sys::xmlSaveToIO(super::write_into, None, into, enc, options) };
            if ctxt.is_null() {
                return false;
            }
            let handler = if enc.is_null() {
                ptr::null_mut()
            } else {
                // SAFETY: a NUL-terminated name.
                unsafe { sys::xmlFindCharEncodingHandler(enc) }
            };
            // SAFETY: `bytes` outlives the buffer, closed below; the buffer
            // takes the handler over.
            let out =
                unsafe { sys::xmlOutputBufferCreateIO(super::write_into, None, into, handler) };
            let mut ok = if out.is_null() {
                if !handler.is_null() {
                    // SAFETY: the handler no buffer took.
                    unsafe { sys::xmlCharEncCloseFunc(handler) };
                }
                false
            } else {
                let mut dump = Dump { ctxt, out };
                let mut ok = body(&mut dump);
                // SAFETY: the live buffer.
                ok &= unsafe { sys::xmlOutputBufferFlush(out) } >= 0;
                // SAFETY: the buffer, closed once.
                ok &= unsafe { sys::xmlOutputBufferClose(out) } >= 0;
                ok
            };
            // SAFETY: the context, closed once.
            ok &= unsafe { sys::xmlSaveClose(ctxt) } >= 0;
            ok
        });
        (ok.then_some(bytes), raised)
    }

    /// `xmlOutputBufferWrite` of `bytes` into dom's buffer.
    pub fn write(&mut self, bytes: &[u8]) -> bool {
        let Ok(len) = c_int::try_from(bytes.len()) else {
            return false;
        };
        // SAFETY: a live buffer and `len` readable bytes.
        unsafe { sys::xmlOutputBufferWrite(self.out, len, bytes.as_ptr().cast()) >= 0 }
    }

    /// `dom_xml_serializing_a_document_node`'s first half: dom's buffer
    /// flushed, then libxml's `xmlSaveDoc` of the document with its
    /// children hidden (its declaration and nothing under it), flushed.
    pub fn document_head(&mut self, doc: Node) -> bool {
        let children = doc.children();
        doc.set_children(None);
        // SAFETY: a live buffer.
        let mut ok = unsafe { sys::xmlOutputBufferFlush(self.out) } >= 0;
        // SAFETY: a live context and document.
        ok = ok && unsafe { sys::xmlSaveDoc(self.ctxt, doc.raw().cast()) } >= 0;
        // SAFETY: a live context.
        ok = ok && unsafe { sys::xmlSaveFlush(self.ctxt) } >= 0;
        doc.set_children(children);
        ok
    }

    /// The default arm of `dom_xml_serialization_algorithm`: dom's buffer
    /// flushed, then libxml's `xmlSaveTree` of the node, flushed.
    pub fn tree(&mut self, node: Node) -> bool {
        // SAFETY: a live buffer.
        let mut ok = unsafe { sys::xmlOutputBufferFlush(self.out) } >= 0;
        // SAFETY: a live context and node.
        ok = ok && unsafe { sys::xmlSaveTree(self.ctxt, node.raw()) } >= 0;
        // SAFETY: a live context.
        ok && unsafe { sys::xmlSaveFlush(self.ctxt) } >= 0
    }
}

/// The first element an `xmlTextReader` meets, as
/// `php_libxml_svg_image_handle` reads it: its local name, and its `width`
/// and `height` attributes where it has them.
#[derive(Debug, Clone)]
pub(crate) struct FirstElement {
    pub(crate) name: Option<Vec<u8>>,
    pub(crate) width: Option<Vec<u8>>,
    pub(crate) height: Option<Vec<u8>>,
}

/// libxml2's pull reader, `xmlreader.h`.
pub(crate) struct Reader;

impl Reader {
    /// `xmlReaderForMemory` over `bytes` with `XML_PARSE_NOWARNING |
    /// XML_PARSE_NOERROR | XML_PARSE_NONET`, read until its first element;
    /// `None` where no reader could be made or the reader met no element.
    /// What libxml raised meanwhile reaches no handler, as the options say.
    pub(crate) fn first_element(bytes: &[u8]) -> Option<FirstElement> {
        const OPTIONS: c_int =
            sys::XML_PARSE_NOWARNING | sys::XML_PARSE_NOERROR | sys::XML_PARSE_NONET;
        Library::init();
        let len = c_int::try_from(bytes.len()).ok()?;
        let (element, _silenced) = Raised::capture(|| {
            // `PHP_LIBXML_SANITIZE_GLOBALS(reader_for_stream)` around the
            // reader's making, whose parser copies the defaults.
            let reader = sanitized_globals(|| {
                // SAFETY: `len` readable bytes the reader reads for its
                // life, which ends below; no URL, no encoding.
                unsafe {
                    sys::xmlReaderForMemory(
                        bytes.as_ptr().cast(),
                        len,
                        ptr::null(),
                        ptr::null(),
                        OPTIONS,
                    )
                }
            });
            if reader.is_null() {
                return None;
            }
            let mut found = None;
            // SAFETY: a live reader.
            while unsafe { sys::xmlTextReaderRead(reader) } == 1 {
                // SAFETY: as above.
                if unsafe { sys::xmlTextReaderNodeType(reader) } != sys::XML_READER_TYPE_ELEMENT {
                    continue;
                }
                // SAFETY: a live reader; the name is the reader's own, or null.
                let name = unsafe { sys::xmlTextReaderConstLocalName(reader) };
                let attribute = |attr: &[u8]| {
                    // SAFETY: a live reader and a NUL-terminated name.
                    let value = unsafe { sys::xmlTextReaderGetAttribute(reader, attr.as_ptr()) };
                    // SAFETY: a string of libxml's allocator nothing else frees.
                    unsafe { Alloc::take(value) }
                };
                found = Some(FirstElement {
                    // SAFETY: the name, live until the reader moves on.
                    name: (!name.is_null()).then(|| unsafe { Alloc::bytes(name) }.to_vec()),
                    width: attribute(b"width\0"),
                    height: attribute(b"height\0"),
                });
                break;
            }
            // SAFETY: the reader, freed once.
            unsafe { sys::xmlFreeTextReader(reader) };
            found
        });
        element
    }
}
