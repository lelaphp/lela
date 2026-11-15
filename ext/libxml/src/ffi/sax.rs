//! libxml2's SAX2 push parser as php's xml extension lays it down
//! (`compat.c`, design §4.10): the `xmlParserCtxt` over the handler
//! table `php_xml_compat_handlers`, each callback handed to a [`Sax`] with
//! its arguments read out of libxml's, and [`At`], what `compat.c` reads
//! of the parser from inside a callback. What the callbacks decide is the
//! expat compatibility php writes over them, the implementor's.

use std::ffi::{c_char, c_int, c_void};
use std::ptr::{self, NonNull};

use php_ext::raising::Thrown;
use php_ext::request::Request;

use super::{Alloc, Io, Node, Raised, Routing, loader, sys};

/// A SAX2 name, the local part with its prefix and namespace URI, and for
/// an attribute its value.
#[derive(Debug, Clone, Copy, Default)]
pub struct AttributeNs<'a> {
    pub local: &'a [u8],
    pub prefix: Option<&'a [u8]>,
    pub uri: Option<&'a [u8]>,
    pub value: &'a [u8],
}

/// The callbacks of `php_xml_compat_handlers`, each served with the
/// request and the parser as it stands. A throwable is kept as the
/// parse's first and parsing goes on, as libxml goes on under php with
/// `EG(exception)` set.
pub trait Sax {
    /// `startElement`, the SAX1 tag a parser without namespaces reports.
    fn start_element(
        &mut self,
        req: &mut Request,
        at: &At,
        name: &[u8],
        attributes: &[(&[u8], &[u8])],
    ) -> Result<(), Thrown>;
    /// `startElementNs`: the tag, its namespace declarations as prefix and
    /// URI, and its attributes.
    fn start_element_ns(
        &mut self,
        req: &mut Request,
        at: &At,
        tag: AttributeNs<'_>,
        namespaces: &[(Option<&[u8]>, &[u8])],
        attributes: &[AttributeNs<'_>],
    ) -> Result<(), Thrown>;
    /// `endElement`.
    fn end_element(&mut self, req: &mut Request, at: &At, name: &[u8]) -> Result<(), Thrown>;
    /// `endElementNs`.
    fn end_element_ns(
        &mut self,
        req: &mut Request,
        at: &At,
        tag: AttributeNs<'_>,
    ) -> Result<(), Thrown>;
    /// `characters` and `cdataBlock`.
    fn characters(&mut self, req: &mut Request, at: &At, text: &[u8]) -> Result<(), Thrown>;
    /// `processingInstruction`.
    fn processing_instruction(
        &mut self,
        req: &mut Request,
        at: &At,
        target: &[u8],
        data: Option<&[u8]>,
    ) -> Result<(), Thrown>;
    /// `comment`.
    fn comment(&mut self, req: &mut Request, at: &At, text: &[u8]) -> Result<(), Thrown>;
    /// `unparsedEntityDecl`: name, public id, system id, notation.
    fn unparsed_entity_decl(
        &mut self,
        req: &mut Request,
        at: &At,
        decl: [Option<&[u8]>; 4],
    ) -> Result<(), Thrown>;
    /// `notationDecl`: name, public id, system id.
    fn notation_decl(
        &mut self,
        req: &mut Request,
        at: &At,
        decl: [Option<&[u8]>; 3],
    ) -> Result<(), Thrown>;
    /// `getEntity` outside the DTD: the entity `name` resolves to, the
    /// predefined one or the document's, which libxml is answered.
    fn entity(
        &mut self,
        req: &mut Request,
        at: &At,
        name: &[u8],
        found: Option<Node>,
    ) -> Result<(), Thrown>;
}

/// The parser a callback was made from: what `compat.c` reads of
/// `parser->parser`, and whether a handler threw.
pub struct At {
    ctxt: *mut sys::xmlParserCtxt,
    io: *mut Io,
}

impl At {
    /// The live `xmlParserCtxt`.
    fn ctxt(&self) -> &sys::xmlParserCtxt {
        // SAFETY: the context whose callback made this, live while it runs.
        unsafe { &*self.ctxt }
    }

    /// Whether a handler threw during this parse, php's `EG(exception)`.
    #[must_use]
    pub fn pending(&self) -> bool {
        // SAFETY: the running call's `Io`, live while the callback runs.
        unsafe { (*self.io).thrown.is_some() }
    }

    /// `start_element_emit_default`'s text: the tag the input stands in,
    /// from its `<` to the input's position, a `/` there closed as `>`.
    #[must_use]
    pub fn start_tag(&self) -> Vec<u8> {
        let input = self.ctxt().input;
        if input.is_null() {
            return Vec::new();
        }
        // SAFETY: the live parser's input.
        let i = unsafe { &*input };
        if i.cur.is_null() || i.base.is_null() {
            return Vec::new();
        }
        let back = i.cur.addr().saturating_sub(i.base.addr());
        // SAFETY: `base..=cur` lies in the input's buffer.
        let window = unsafe { std::slice::from_raw_parts(i.base, back + 1) };
        let start = window.iter().rposition(|b| *b == b'<').unwrap_or(0);
        let mut tag = window[start..].to_vec();
        if let Some(last) = tag.last_mut()
            && *last == b'/'
        {
            *last = b'>';
        }
        tag
    }

    /// Where the parser stands.
    #[must_use]
    pub fn position(&self) -> Position {
        // SAFETY: the context whose callback made this, live while it runs.
        unsafe { position_of(self.ctxt) }
    }

    /// `parser->parser->instate == XML_PARSER_CONTENT`.
    #[must_use]
    pub fn in_content(&self) -> bool {
        self.ctxt().instate == sys::XML_PARSER_CONTENT
    }

    /// `xmlStopParser(ctxt)` with `errNo` set to `code`, as php stops it
    /// where an external entity reference handler refused.
    pub fn stop(&self, code: i32) {
        // SAFETY: the live context.
        unsafe { sys::xmlStopParser(self.ctxt) };
        // SAFETY: as above, the one field written.
        unsafe { (*self.ctxt).errNo = code };
    }
}

/// `XML_Parser`'s libxml half: the push parser's `xmlParserCtxt` over php's table.
pub struct PushParser(NonNull<sys::xmlParserCtxt>);

// SAFETY: the context is reached only from the request that made it, one
// thread at a time (ADR-0005).
unsafe impl Send for PushParser {}

impl Drop for PushParser {
    fn drop(&mut self) {
        let ctxt = self.0.as_ptr();
        // SAFETY: the context this owns.
        let doc = unsafe { (*ctxt).myDoc };
        if !doc.is_null() {
            // SAFETY: the context's document, freed once.
            unsafe { sys::xmlFreeDoc(doc) };
            // SAFETY: the field cleared, as `XML_ParserFree` clears it.
            unsafe { (*ctxt).myDoc = ptr::null_mut() };
        }
        // SAFETY: the context, freed once.
        unsafe { sys::xmlFreeParserCtxt(ctxt) };
    }
}

impl PushParser {
    /// `XML_ParserCreate_MM`: `xmlCreatePushParserCtxt` over php's table,
    /// sanitised, with `XML_PARSE_OLDSAX | XML_PARSE_NOENT`, reporting SAX2
    /// tags where `namespaces` asks and SAX1 ones otherwise.
    #[must_use]
    pub fn new(namespaces: bool) -> Option<PushParser> {
        let mut table = sys::xmlSAXHandler {
            callbacks: [ptr::null_mut(); 21],
            warning: ptr::null_mut(),
            error: ptr::null_mut(),
            fatal_error: ptr::null_mut(),
            getParameterEntity: ptr::null_mut(),
            cdataBlock: characters as *mut c_void,
            externalSubset: ptr::null_mut(),
            initialized: sys::XML_SAX2_MAGIC,
            private: ptr::null_mut(),
            startElementNs: start_element_ns as *mut c_void,
            endElementNs: end_element_ns as *mut c_void,
            serror: ptr::null_mut(),
        };
        table.callbacks[5] = get_entity as *mut c_void;
        table.callbacks[7] = notation_decl as *mut c_void;
        table.callbacks[10] = unparsed_entity_decl as *mut c_void;
        table.callbacks[14] = start_element as *mut c_void;
        table.callbacks[15] = end_element as *mut c_void;
        table.callbacks[17] = characters as *mut c_void;
        table.callbacks[19] = processing_instruction as *mut c_void;
        table.callbacks[20] = comment as *mut c_void;
        // SAFETY: a table libxml copies; no data, chunk or file name.
        let raw = unsafe {
            sys::xmlCreatePushParserCtxt(
                (&raw mut table).cast(),
                ptr::null_mut(),
                ptr::null(),
                0,
                ptr::null(),
            )
        };
        let ctxt = NonNull::new(raw)?;
        // SAFETY: the fresh context.
        super::io::sanitize(unsafe { &mut *ctxt.as_ptr() });
        let options = sys::XML_PARSE_OLDSAX | sys::XML_PARSE_NOENT;
        // SAFETY: as above.
        unsafe { sys::xmlCtxtUseOptions(ctxt.as_ptr(), options) };
        // SAFETY: as above.
        let c = unsafe { &mut *ctxt.as_ptr() };
        c.wellFormed = 0;
        if !namespaces && !c.sax.is_null() {
            // SAFETY: the context's own copy of the table.
            unsafe { (*c.sax).initialized = 1 };
        }
        Some(PushParser(ctxt))
    }

    /// `xml_parse_helper` and `XML_Parse`: the parse's options set as
    /// `huge` asks, `xmlParseChunk` of `data` with each callback served
    /// through `sax`; whether the chunk parsed with nothing above a
    /// warning, what libxml raised, and the first throwable.
    pub fn parse(
        &mut self,
        req: &mut Request,
        data: &[u8],
        last: bool,
        huge: bool,
        sax: &mut dyn Sax,
    ) -> (bool, Vec<Raised>, Option<Thrown>) {
        let ctxt = self.0.as_ptr();
        // SAFETY: the live context.
        let c = unsafe { &mut *ctxt };
        if huge {
            c.options |= sys::XML_PARSE_HUGE;
        } else {
            c.options &= !sys::XML_PARSE_HUGE;
        }
        let limit = if huge {
            0
        } else {
            sys::XML_MAX_DICTIONARY_LIMIT
        };
        // SAFETY: the context's dictionary.
        unsafe { sys::xmlDictSetLimit(c.dict, limit) };
        let len = c_int::try_from(data.len()).unwrap_or(c_int::MAX);
        let mut session = Session { sax, ctxt };
        let routing = Routing::Parser {
            sax: ptr::null_mut(),
        };
        Raised::capture_io(req, Some(routing), || {
            // SAFETY: the context, its data the session for this call.
            unsafe { (*ctxt).userData = (&raw mut session).cast() };
            let chunk = data.as_ptr().cast::<c_char>();
            // SAFETY: the live context and `len` readable bytes.
            let error = unsafe { sys::xmlParseChunk(ctxt, chunk, len, c_int::from(last)) };
            // SAFETY: the data set above, cleared.
            unsafe { (*ctxt).userData = ptr::null_mut() };
            if error != 0 {
                return false;
            }
            // SAFETY: the live context.
            let last_error = unsafe { sys::xmlCtxtGetLastError(ctxt.cast()) };
            // SAFETY: null or the context's own error.
            last_error.is_null() || unsafe { (*last_error).level } <= sys::XML_ERR_WARNING
        })
    }

    /// Where the parser stands.
    #[must_use]
    pub fn position(&self) -> Position {
        // SAFETY: the live context this owns.
        unsafe { position_of(self.0.as_ptr()) }
    }
}

/// Where a parser stands: what `XML_GetErrorCode`,
/// `XML_GetCurrentLineNumber`, `XML_GetCurrentColumnNumber` and
/// `XML_GetCurrentByteIndex` read, `errNo`, the input's line and column,
/// and what it consumed with how far into its buffer it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Position {
    pub error_code: i32,
    pub line: i32,
    pub column: i32,
    pub byte_index: i64,
}

/// # Safety
/// `ctxt` is a live parser.
unsafe fn position_of(ctxt: *mut sys::xmlParserCtxt) -> Position {
    // SAFETY: the caller's contract.
    let c = unsafe { &*ctxt };
    let mut at = Position {
        error_code: c.errNo,
        ..Position::default()
    };
    if !c.input.is_null() {
        // SAFETY: the context's live input.
        let i = unsafe { &*c.input };
        let into = i.cur.addr().wrapping_sub(i.base.addr());
        let consumed = i64::try_from(i.consumed).unwrap_or(i64::MAX);
        at.line = i.line;
        at.column = i.col;
        at.byte_index = consumed.saturating_add(i64::try_from(into).unwrap_or(0));
    }
    at
}

/// What one parse lends the callbacks: the implementor, and the parser
/// the callbacks are made from, which libxml hands them only as this.
struct Session<'s> {
    sax: &'s mut dyn Sax,
    ctxt: *mut sys::xmlParserCtxt,
}

/// The session of the callback `user` came with.
///
/// # Safety
/// `user` is the parser's `userData`: the session `parse` set, live for the
/// call, or null outside one.
unsafe fn session<'a>(user: *mut c_void) -> Option<&'a mut Session<'a>> {
    // SAFETY: the caller's contract.
    unsafe { user.cast::<Session<'a>>().as_mut() }
}

/// The session's implementor served with the request the call lends,
/// errors collected so far routed first and the handler served as php code
/// is served from inside a libxml call; a throwable kept as the call's
/// first.
fn serve(user: *mut c_void, f: impl FnOnce(&mut dyn Sax, &mut Request, &At) -> Result<(), Thrown>) {
    let Some((call, io)) = loader::lent() else {
        return;
    };
    // SAFETY: `parse` set the context's data to its session.
    let Some(session) = (unsafe { session(user) }) else {
        return;
    };
    // SAFETY: the running call and its `Io`.
    unsafe { loader::flush(call, io) };
    // SAFETY: as above.
    let req = unsafe { loader::request(io) };
    let at = At {
        ctxt: session.ctxt,
        io,
    };
    let sax = &mut *session.sax;
    if let Err(thrown) = loader::suspended(|| f(sax, req, &at)) {
        // SAFETY: the running call's `Io`.
        unsafe { loader::keep(io, thrown) };
    }
}

/// A C string libxml handed a callback, `None` for its null.
///
/// # Safety
/// `p` is null or a NUL-terminated string live for the callback.
unsafe fn text<'a>(p: *const u8) -> Option<&'a [u8]> {
    // SAFETY: the caller's contract.
    (!p.is_null()).then(|| unsafe { Alloc::bytes(p) })
}

/// The bytes from `start` to `end`.
///
/// # Safety
/// Both are null or bound one live span of the callback.
unsafe fn span<'a>(start: *const u8, end: *const u8) -> &'a [u8] {
    if start.is_null() || end.addr() <= start.addr() {
        return &[];
    }
    // SAFETY: the caller's contract.
    unsafe { std::slice::from_raw_parts(start, end.addr() - start.addr()) }
}

/// A SAX2 name: the local part, prefix and URI.
///
/// # Safety
/// Each is null or a NUL-terminated string live for the callback.
unsafe fn tag<'a>(local: *const u8, prefix: *const u8, uri: *const u8) -> AttributeNs<'a> {
    AttributeNs {
        // SAFETY: the caller's contract.
        local: unsafe { text(local) }.unwrap_or_default(),
        // SAFETY: as above.
        prefix: unsafe { text(prefix) },
        // SAFETY: as above.
        uri: unsafe { text(uri) },
        value: &[],
    }
}

unsafe extern "C" fn start_element(
    user: *mut c_void,
    name: *const u8,
    attributes: *const *const u8,
) {
    // SAFETY: libxml's NUL-terminated name.
    let name = unsafe { text(name) }.unwrap_or_default();
    let mut pairs = Vec::new();
    let mut at = attributes;
    // SAFETY: a NULL-terminated array of name and value pairs, or null.
    while !at.is_null() && !unsafe { *at }.is_null() {
        // SAFETY: the pair's name.
        let n = unsafe { *at };
        // SAFETY: the slot after the name, the pair's value.
        let second = unsafe { at.add(1) };
        // SAFETY: as above.
        let v = unsafe { *second };
        // SAFETY: NUL-terminated strings of the array.
        let n = unsafe { text(n) }.unwrap_or_default();
        // SAFETY: as above.
        pairs.push((n, unsafe { text(v) }.unwrap_or_default()));
        // SAFETY: the next pair, or the terminator.
        at = unsafe { at.add(2) };
    }
    serve(user, |sax, req, at| {
        sax.start_element(req, at, name, &pairs)
    });
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn start_element_ns(
    user: *mut c_void,
    local: *const u8,
    prefix: *const u8,
    uri: *const u8,
    nb_namespaces: c_int,
    namespaces: *const *const u8,
    nb_attributes: c_int,
    _nb_defaulted: c_int,
    attributes: *const *const u8,
) {
    // SAFETY: libxml's NUL-terminated strings or null.
    let name = unsafe { tag(local, prefix, uri) };
    let n = usize::try_from(nb_namespaces).unwrap_or(0);
    // SAFETY: `nb_namespaces` pairs of prefix and URI.
    let flat = unsafe { rows(namespaces, 2 * n) };
    let ns: Vec<_> = flat
        .chunks_exact(2)
        .map(|p| {
            // SAFETY: a NUL-terminated prefix of the array, or null.
            let prefix = unsafe { text(p[0]) };
            // SAFETY: a NUL-terminated URI of the array.
            (prefix, unsafe { text(p[1]) }.unwrap_or_default())
        })
        .collect();
    let n = usize::try_from(nb_attributes).unwrap_or(0);
    // SAFETY: `nb_attributes` rows of five: local name, prefix, URI, and
    // the value's start and end.
    let flat = unsafe { rows(attributes, 5 * n) };
    let attrs: Vec<_> = flat
        .chunks_exact(5)
        .map(|row| AttributeNs {
            // SAFETY: as above.
            value: unsafe { span(row[3], row[4]) },
            // SAFETY: as above.
            ..unsafe { tag(row[0], row[1], row[2]) }
        })
        .collect();
    serve(user, |sax, req, at| {
        sax.start_element_ns(req, at, name, &ns, &attrs)
    });
}

/// `n` pointers at `p`.
///
/// # Safety
/// `p` is null with `n` zero, or `n` pointers live for the callback.
unsafe fn rows<'a>(p: *const *const u8, n: usize) -> &'a [*const u8] {
    if p.is_null() || n == 0 {
        return &[];
    }
    // SAFETY: the caller's contract.
    unsafe { std::slice::from_raw_parts(p, n) }
}

unsafe extern "C" fn end_element(user: *mut c_void, name: *const u8) {
    // SAFETY: libxml's NUL-terminated name.
    let name = unsafe { text(name) }.unwrap_or_default();
    serve(user, |sax, req, at| sax.end_element(req, at, name));
}

unsafe extern "C" fn end_element_ns(
    user: *mut c_void,
    local: *const u8,
    prefix: *const u8,
    uri: *const u8,
) {
    // SAFETY: libxml's NUL-terminated strings or null.
    let name = unsafe { tag(local, prefix, uri) };
    serve(user, |sax, req, at| sax.end_element_ns(req, at, name));
}

unsafe extern "C" fn characters(user: *mut c_void, ch: *const u8, len: c_int) {
    let n = usize::try_from(len).unwrap_or(0);
    // SAFETY: `len` bytes libxml hands the callback.
    let text = unsafe { span(ch, ch.wrapping_add(n)) };
    serve(user, |sax, req, at| sax.characters(req, at, text));
}

unsafe extern "C" fn processing_instruction(user: *mut c_void, target: *const u8, data: *const u8) {
    // SAFETY: libxml's NUL-terminated strings or null.
    let target = unsafe { text(target) }.unwrap_or_default();
    // SAFETY: as above.
    let data = unsafe { text(data) };
    serve(user, |sax, req, at| {
        sax.processing_instruction(req, at, target, data)
    });
}

unsafe extern "C" fn comment(user: *mut c_void, value: *const u8) {
    // SAFETY: libxml's NUL-terminated comment.
    let value = unsafe { text(value) }.unwrap_or_default();
    serve(user, |sax, req, at| sax.comment(req, at, value));
}

unsafe extern "C" fn unparsed_entity_decl(
    user: *mut c_void,
    name: *const u8,
    public_id: *const u8,
    system_id: *const u8,
    notation: *const u8,
) {
    // SAFETY: libxml's NUL-terminated strings or null, each.
    let decl = [name, public_id, system_id, notation].map(|p| unsafe { text(p) });
    serve(user, |sax, req, at| sax.unparsed_entity_decl(req, at, decl));
}

unsafe extern "C" fn notation_decl(
    user: *mut c_void,
    name: *const u8,
    public_id: *const u8,
    system_id: *const u8,
) {
    // SAFETY: libxml's NUL-terminated strings or null, each.
    let decl = [name, public_id, system_id].map(|p| unsafe { text(p) });
    serve(user, |sax, req, at| sax.notation_decl(req, at, decl));
}

/// `get_entity`'s libxml half: outside the DTD, the predefined entity or
/// the document's, handed to the implementor and answered to libxml.
unsafe extern "C" fn get_entity(user: *mut c_void, name: *const u8) -> *mut sys::xmlEntity {
    // SAFETY: `parse` set the context's data to its session.
    let Some(ctxt) = (unsafe { session(user) }).map(|s| s.ctxt) else {
        return ptr::null_mut();
    };
    // SAFETY: the context parsing, live for the callback.
    let c = unsafe { &*ctxt };
    if c.inSubset != 0 {
        return ptr::null_mut();
    }
    // SAFETY: libxml's NUL-terminated name.
    let Some(bytes) = (unsafe { text(name) }) else {
        return ptr::null_mut();
    };
    // SAFETY: a NUL-terminated name.
    let mut found = unsafe { sys::xmlGetPredefinedEntity(name) };
    if found.is_null() {
        // SAFETY: the context's document, or null, and the name.
        found = unsafe { sys::xmlGetDocEntity(c.myDoc, name) };
    }
    let entity = Node::from_raw(found.cast());
    serve(user, |sax, req, at| sax.entity(req, at, bytes, entity));
    found
}
