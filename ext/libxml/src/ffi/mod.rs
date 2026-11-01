//! The binding to libxml2 (design §7.2, `unsafe.homes`' `ext/*/src/ffi/`):
//! the platform's library, found by `build.rs` through `pkg-config
//! libxml-2.0`, and the one home of this crate's `unsafe`. Beside the
//! declarations ([`sys`]) it carries what `php_libxml.h` declares
//! `PHPAPI`, for the extensions that include that header (design §4.10):
//!
//! - [`NodeObject`], `php_libxml_node_object`: the libxml half of a node
//!   object, the counted proxy a node's `_private` names
//!   (`php_libxml_node_ptr`) and the counted document it holds
//!   (`php_libxml_ref_obj`), with php's increments and decrements;
//! - [`node_free_list`] and [`node_free_resource`], how php frees a
//!   subtree a program let go of, keeping every node something still
//!   names;
//! - [`capture`], the structured error handler set around one call and
//!   the errors it raised copied out, which [`crate::report`] routes as
//!   php's handlers do;
//! - [`dump_doc`] and [`dump_node`], `php_libxml_document_handlers`'
//!   default serialisers, and [`attr_value`], `php_libxml_attr_value`.
//!
//! libxml2 keeps its error handler per thread, so setting it around a call
//! from the request that makes the call is all the isolation two requests
//! on two threads need (ADR-0005).
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

mod io;
mod loader;
mod reader;
mod sax;
pub(crate) mod sys;
mod tree;
mod writer;
mod xpath;

pub(crate) use io::Reader;
pub use io::{
    C14nOut, C14nSelect, C14nTarget, Dump, Fragment, Parsed, Parser, Save, Schema, Source,
    Validate, Validated,
};
pub(crate) use loader::{Context, Site, Uri};
pub use reader::{Expanded, RelaxNg, TextReader};
pub use sax::{At, AttributeNs, Position, PushParser, Sax};
pub use sys::{
    HTML_PARSE_NOBLANKS, HTML_PARSE_NODEFDTD, HTML_PARSE_NOERROR, HTML_PARSE_NOIMPLIED,
    HTML_PARSE_NONET, HTML_PARSE_NOWARNING, HTML_PARSE_RECOVER, XML_ATTRIBUTE_ID,
    XML_EXTERNAL_GENERAL_UNPARSED_ENTITY, XML_PARSE_BIG_LINES, XML_PARSE_COMPACT,
    XML_PARSE_DTDATTR, XML_PARSE_DTDLOAD, XML_PARSE_DTDVALID, XML_PARSE_HUGE, XML_PARSE_IGNORE_ENC,
    XML_PARSE_NOBLANKS, XML_PARSE_NOCDATA, XML_PARSE_NOENT, XML_PARSE_NOERROR, XML_PARSE_NONET,
    XML_PARSE_NOWARNING, XML_PARSE_NSCLEAN, XML_PARSE_PEDANTIC, XML_PARSE_RECOVER,
    XML_PARSE_XINCLUDE, XML_SAVE_AS_XML, XML_SAVE_FORMAT, XML_SAVE_NO_DECL, XML_SAVE_NO_EMPTY,
};
pub use tree::{DocOf, DocString, DtdTable, HashTable, Kind, Name, Node, Ns, Utf8};
pub use writer::Emitter;
pub use xpath::{Answer, Called, Function, Handler, Item, Object, Operand, Returned, Scope, XPath};

/// A copy of `bytes` up to its first NUL with the NUL after, which is
/// what a C function handed a `zend_string`'s `char *` reads.
fn cstr(bytes: &[u8]) -> Vec<u8> {
    let head = c_str(bytes);
    let mut out = Vec::with_capacity(head.len() + 1);
    out.extend_from_slice(head);
    out.push(0);
    out
}

use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr::{self, NonNull};

use php_ext::coerce::c_str;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{ClassId, Emalloc, PhpObject};

/// What libxml allocated: its strings read and copied, and its
/// allocations given back.
pub(crate) struct Alloc;

impl Alloc {
    /// The bytes of a C string libxml owns, empty for a null pointer.
    ///
    /// # Safety
    /// `p` is null or a NUL-terminated string that outlives the returned
    /// slice.
    pub(crate) unsafe fn bytes<'a>(p: *const u8) -> &'a [u8] {
        if p.is_null() {
            return &[];
        }
        // SAFETY: the caller's contract.
        unsafe { CStr::from_ptr(p.cast::<c_char>()) }.to_bytes()
    }

    /// The bytes of a string libxml allocated, copied and given back with
    /// `xmlFree`; `None` for its null.
    ///
    /// # Safety
    /// `p` is null or a NUL-terminated string from libxml's allocator that no
    /// one else frees.
    pub(crate) unsafe fn take(p: *mut u8) -> Option<Vec<u8>> {
        if p.is_null() {
            return None;
        }
        // SAFETY: the caller's contract.
        let copy = unsafe { Alloc::bytes(p) }.to_vec();
        // SAFETY: as above; freed once, here.
        unsafe { Alloc::free(p.cast::<c_void>()) };
        Some(copy)
    }

    /// `xmlFree`, the allocator's own release.
    ///
    /// # Safety
    /// `p` is null or an allocation of libxml's allocator nothing uses again.
    pub(crate) unsafe fn free(p: *mut c_void) {
        if p.is_null() {
            return;
        }
        // SAFETY: a variable libxml2 sets at load and nothing here writes.
        let release = unsafe { sys::xmlFree };
        // SAFETY: the caller's contract.
        unsafe { release(p) };
    }
}

/// The library itself.
pub struct Library;

impl Library {
    /// `xmlInitParser`, which php calls before every parse: idempotent, and
    /// safe to reach from any thread.
    pub fn init() {
        // SAFETY: no argument; libxml guards its one-time setup itself.
        unsafe { sys::xmlInitParser() };
    }

    /// `php_libxml_initialize`'s ZTS half (`Z/ext/libxml/libxml.c:941-947`):
    /// `xmlSchemaInitTypes` and `xmlRelaxNGInitTypes`, the schema type
    /// tables built once before any thread validates, since libxml2
    /// builds them unguarded on first use. php's threaded build makes the
    /// calls; this engine serves its requests on threads.
    pub(crate) fn init_types() {
        // SAFETY: no argument; called once for the process, before any
        // request validates.
        unsafe { sys::xmlSchemaInitTypes() };
        // SAFETY: as above; the status is 0 once the types stand, and php
        // does not read it.
        let _ = unsafe { sys::xmlRelaxNGInitTypes() };
    }

    /// `php_libxml_initialize`'s `xmlSetExternalEntityLoader`: php's
    /// loader, which every parse of the process reaches and which serves
    /// libxml's own default outside a call that lends a request.
    pub fn install_loader() {
        loader::EntityLoader::install();
    }

    /// Whether `source`, escaped as `xmlURIEscapeStr(source, ":")`,
    /// parses as a URI with a scheme: the test `_xmlreader_get_valid_file_path`
    /// and `_xmlwriter_get_valid_file_path` make before reading `source` as
    /// a local path.
    #[must_use]
    pub fn escaped_has_scheme(source: &[u8]) -> bool {
        Uri::escaped_has_scheme(source)
    }

    /// `f` under the parse defaults ext/xsl's `importStylesheet` sets
    /// around `xsltParseStylesheetDoc`: `PHP_LIBXML_SANITIZE_GLOBALS`,
    /// then `xmlSubstituteEntitiesDefault(1)` and
    /// `xmlLoadExtDtdDefaultValue = XML_DETECT_IDS | XML_COMPLETE_ATTRS`,
    /// and `PHP_LIBXML_RESTORE_GLOBALS` after. libxml2 keeps both per
    /// thread.
    pub fn stylesheet_defaults<R>(f: impl FnOnce() -> R) -> R {
        io::sanitized_globals(|| {
            // SAFETY: no pointer argument; the thread's own default, put
            // back with the rest as the sanitising ends.
            let _ = unsafe { sys::xmlSubstituteEntitiesDefault(1) };
            // SAFETY: the address of the thread's own default.
            let load = unsafe { sys::__xmlLoadExtDtdDefaultValue() };
            // SAFETY: the thread's own global, written by copy;
            // `XML_DETECT_IDS | XML_COMPLETE_ATTRS`.
            unsafe { *load = 2 | 4 };
            f()
        })
    }

    /// The calling thread's deprecated parse defaults set as a careless
    /// library would leave them, what `zend_test_override_libxml_global_state`
    /// sets through `<libxml/globals.h>` and `<libxml/parser.h>`: external
    /// DTDs loaded, validation, the pedantic parser, entities substituted,
    /// line numbers kept and blanks dropped. php's parsers each set their
    /// own options, so none of it reaches a parse they make
    /// (GHSA-3qrf-m4j2-pcrr). libxml2 keeps them per thread, and they
    /// stand for the thread's later requests, as php's stand for its
    /// process.
    pub fn override_global_state() {
        // SAFETY: the address of the thread's own default.
        let load = unsafe { sys::__xmlLoadExtDtdDefaultValue() };
        // SAFETY: the thread's own global, written by copy.
        unsafe { *load = 1 };
        // SAFETY: as above.
        let validity = unsafe { sys::__xmlDoValidityCheckingDefaultValue() };
        // SAFETY: as above.
        unsafe { *validity = 1 };
        // SAFETY: no pointer argument; the thread's own default, whose
        // old value php discards.
        let _ = unsafe { sys::xmlPedanticParserDefault(1) };
        // SAFETY: as above.
        let _ = unsafe { sys::xmlSubstituteEntitiesDefault(1) };
        // SAFETY: as above.
        let _ = unsafe { sys::xmlLineNumbersDefault(1) };
        // SAFETY: as above.
        let _ = unsafe { sys::xmlKeepBlanksDefault(0) };
    }

    /// `xmlParserVersion`, the library's own version string: `"20913"`.
    #[must_use]
    pub fn loaded_version() -> Vec<u8> {
        // SAFETY: a static string of the library, never written.
        unsafe { Alloc::bytes(sys::xmlParserVersion.cast::<u8>()) }.to_vec()
    }
}

// ---- errors: `php_libxml_structured_error_handler` and its siblings

unsafe extern "C" {
    fn php_libxml_generic_push(saved: *mut *mut c_void, saved_ctx: *mut *mut c_void);
    fn php_libxml_generic_pop(saved: *mut c_void, saved_ctx: *mut c_void);
}

/// `php_libxml_error_handler` past `generic.c`: what libxml2 wrote straight
/// through `xmlGenericError`, joined to the capture running on this thread
/// as the generic handler's errors are.
#[unsafe(no_mangle)]
extern "C" fn php_libxml_generic_text(text: *const c_char, len: usize) {
    if text.is_null() {
        return;
    }
    // SAFETY: `generic.c` hands `len` bytes of a live string.
    let bytes = unsafe { std::slice::from_raw_parts(text.cast::<u8>(), len) };
    Raised::generic(bytes);
}

/// How php would have printed one error with internal errors off, which
/// is decided by the handler libxml reached, not by the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `php_libxml_ctx_error` and `php_libxml_ctx_warning`, the handlers
    /// php puts on a parser: the message, then ` in <file>, line:
    /// <n>` or ` in Entity, line: <n>`.
    Parser,
    /// `php_libxml_error_handler`, the generic handler: the message alone.
    Generic,
    /// libxml's own `xmlParserError` or `xmlParserWarning`, what a
    /// parser has before php replaces them: `xmlReportError`'s text
    /// through the generic handler.
    Default,
    /// No handler: the one `LIBXML_NOERROR` or `LIBXML_NOWARNING` took off
    /// the parser, so the error reaches only the list.
    Silent,
}

/// `xmlErrorLevel`, decoded once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    None,
    Warning,
    Error,
    Fatal,
}

impl Level {
    fn of(raw: c_int) -> Level {
        match raw {
            sys::XML_ERR_WARNING => Level::Warning,
            sys::XML_ERR_ERROR => Level::Error,
            sys::XML_ERR_FATAL => Level::Fatal,
            _ => Level::None,
        }
    }

    /// The number `LibXMLError::$level` carries.
    #[must_use]
    pub fn number(self) -> i64 {
        match self {
            Level::None => 0,
            Level::Warning => 1,
            Level::Error => 2,
            Level::Fatal => 3,
        }
    }
}

/// One `xmlError` libxml raised, copied out of the library as
/// `xmlCopyError` copies it for php's list.
#[derive(Debug, Clone)]
pub struct Raised {
    pub domain: i32,
    pub code: i32,
    pub level: Level,
    pub message: Vec<u8>,
    pub file: Option<Vec<u8>>,
    pub line: i32,
    /// `int2`, the column.
    pub column: i32,
    /// The name of the element `node` names, which `xmlReportError`
    /// prints.
    pub element: Option<Vec<u8>>,
    /// `xmlReportError`'s text, what libxml's own handler writes through
    /// `xmlGenericError`: one piece per line, the site, the element, the
    /// domain, the level and the message, then the input's line and a
    /// caret under the column where a parser raised it.
    pub printed: Vec<u8>,
    pub route: Route,
}

impl Raised {
    /// An error php raises itself, `php_list_set_error_structure` with no
    /// `xmlError`: `XML_ERR_INTERNAL_ERROR` at `XML_ERR_ERROR`.
    #[must_use]
    pub fn internal(message: &[u8]) -> Raised {
        Raised {
            domain: 0,
            code: sys::XML_ERR_INTERNAL_ERROR,
            level: Level::Error,
            message: message.to_vec(),
            file: None,
            line: 0,
            column: 0,
            element: None,
            printed: Vec::new(),
            route: Route::Generic,
        }
    }
}

unsafe extern "C" fn collect(user: *mut c_void, error: *mut sys::xmlError) {
    if user.is_null() || error.is_null() {
        return;
    }
    // SAFETY: `Call::run` set `user` to its own record, live for the call
    // it wraps, and libxml calls back on that call's thread.
    let call = unsafe { &mut *user.cast::<Call>() };
    // An error raised after a program's code threw from inside the call
    // reaches the list alone: php's handler raises nothing while
    // `EG(exception)` is set.
    let after_throw = call.thrown();
    let routing = call.routing;
    let into = &mut call.raised;
    // SAFETY: libxml hands a live error for the duration of the callback.
    let e = unsafe { &*error };
    // SAFETY: the error's strings are NUL-terminated and live as long as it.
    let message = unsafe { Alloc::bytes(e.message.cast::<u8>()) }.to_vec();
    // SAFETY: as above.
    let file = (!e.file.is_null()).then(|| unsafe { Alloc::bytes(e.file.cast::<u8>()) }.to_vec());
    let node = e.node.cast::<sys::xmlNode>();
    let element = if node.is_null() {
        None
    } else {
        // SAFETY: an error's node is the live node it concerns.
        let n = unsafe { &*node };
        // SAFETY: an element's name is a NUL-terminated string it owns.
        (n.type_ == sys::XML_ELEMENT_NODE).then(|| unsafe { Alloc::bytes(n.name) }.to_vec())
    };
    // `__xmlRaiseError` reads `ctx` as a parser context for these domains
    // alone; for every other one it is whatever context raised it.
    let parser = if matches!(
        e.domain,
        sys::XML_FROM_PARSER
            | sys::XML_FROM_HTML
            | sys::XML_FROM_DTD
            | sys::XML_FROM_NAMESPACE
            | sys::XML_FROM_IO
            | sys::XML_FROM_VALID
    ) {
        e.ctxt.cast::<sys::xmlParserCtxt>()
    } else {
        ptr::null_mut()
    };
    let level = Level::of(e.level);
    // SAFETY: `parser` is null or the live context raising the error.
    let printed = unsafe {
        printed_text(
            e,
            parser,
            &message,
            file.as_deref(),
            element.as_deref(),
            level,
        )
    };
    into.push(Raised {
        domain: e.domain,
        code: e.code,
        level,
        message,
        file,
        line: e.line,
        column: e.int2,
        element,
        printed,
        route: if after_throw {
            Route::Silent
        } else if parser.is_null() {
            Route::Generic
        } else if let Some(Routing::Parser { sax }) = routing {
            // SAFETY: the live parser raising the error.
            unsafe { Routing::parser_route(sax, parser, level) }
        } else {
            Route::Parser
        },
    });
}

/// `xmlReportError` in 2.9.13, into bytes: the site from the parser's
/// input (or the error's file and line), the element, the domain and the
/// level, the message with its newline, then the input's line around the
/// position.
///
/// # Safety
/// `parser` is null or a live parser's `xmlParserCtxt`.
unsafe fn printed_text(
    e: &sys::xmlError,
    parser: *mut sys::xmlParserCtxt,
    message: &[u8],
    file: Option<&[u8]>,
    element: Option<&[u8]>,
    level: Level,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut input: *mut sys::xmlParserInput = ptr::null_mut();
    let mut current: *mut sys::xmlParserInput = ptr::null_mut();
    if parser.is_null() {
        if let Some(f) = file {
            out.extend_from_slice(f);
            out.extend_from_slice(format!(":{}: ", e.line).as_bytes());
        } else if e.line != 0
            && matches!(
                e.domain,
                sys::XML_FROM_PARSER
                    | sys::XML_FROM_SCHEMASV
                    | sys::XML_FROM_SCHEMASP
                    | sys::XML_FROM_DTD
                    | sys::XML_FROM_RELAXNGP
                    | sys::XML_FROM_RELAXNGV
            )
        {
            out.extend_from_slice(format!("Entity: line {}: ", e.line).as_bytes());
        }
    } else {
        // SAFETY: the caller's contract.
        let p = unsafe { &*parser };
        input = p.input;
        // SAFETY: a live context's input is null or live.
        if !input.is_null() && unsafe { (*input).filename.is_null() } && p.inputNr > 1 {
            current = input;
            let at = usize::try_from(p.inputNr - 2).unwrap_or(0);
            // SAFETY: `inputNr` entries at `inputTab`, `at` below it.
            let slot = unsafe { p.inputTab.add(at) };
            // SAFETY: a live entry of the table.
            input = unsafe { *slot };
        }
        if !input.is_null() {
            // SAFETY: a live input.
            let i = unsafe { &*input };
            if !i.filename.is_null() {
                // SAFETY: a NUL-terminated name the input owns.
                out.extend_from_slice(unsafe { Alloc::bytes(i.filename.cast::<u8>()) });
                out.extend_from_slice(format!(":{}: ", i.line).as_bytes());
            } else if e.line != 0 && e.domain == sys::XML_FROM_PARSER {
                out.extend_from_slice(format!("Entity: line {}: ", i.line).as_bytes());
            }
        }
    }
    if let Some(name) = element {
        out.extend_from_slice(b"element ");
        out.extend_from_slice(name);
        out.extend_from_slice(b": ");
    }
    out.extend_from_slice(domain_name(e.domain));
    out.extend_from_slice(match level {
        Level::None => b": ".as_slice(),
        Level::Warning => b"warning : ",
        Level::Error | Level::Fatal => b"error : ",
    });
    out.extend_from_slice(message);
    if !message.ends_with(b"\n") {
        out.push(b'\n');
    }
    if !parser.is_null() {
        // SAFETY: null or a live input of the live context.
        unsafe { file_context(input, &mut out) };
        if !current.is_null() {
            // SAFETY: as above.
            let c = unsafe { &*current };
            if !c.filename.is_null() {
                // SAFETY: a NUL-terminated name the input owns.
                out.extend_from_slice(unsafe { Alloc::bytes(c.filename.cast::<u8>()) });
                out.extend_from_slice(format!(":{}: \n", c.line).as_bytes());
            } else if e.line != 0 && e.domain == sys::XML_FROM_PARSER {
                out.extend_from_slice(format!("Entity: line {}: \n", c.line).as_bytes());
            }
            // SAFETY: as above.
            unsafe { file_context(current, &mut out) };
        }
    }
    out
}

/// `xmlReportError`'s domain word.
fn domain_name(domain: c_int) -> &'static [u8] {
    match domain {
        sys::XML_FROM_PARSER | sys::XML_FROM_XPOINTER => b"parser ",
        sys::XML_FROM_NAMESPACE => b"namespace ",
        sys::XML_FROM_DTD | sys::XML_FROM_VALID => b"validity ",
        sys::XML_FROM_HTML => b"HTML parser ",
        sys::XML_FROM_MEMORY => b"memory ",
        sys::XML_FROM_OUTPUT => b"output ",
        sys::XML_FROM_IO => b"I/O ",
        sys::XML_FROM_XINCLUDE => b"XInclude ",
        sys::XML_FROM_XPATH => b"XPath ",
        sys::XML_FROM_REGEXP => b"regexp ",
        sys::XML_FROM_MODULE => b"module ",
        sys::XML_FROM_SCHEMASV => b"Schemas validity ",
        sys::XML_FROM_SCHEMASP => b"Schemas parser ",
        sys::XML_FROM_RELAXNGP => b"Relax-NG parser ",
        sys::XML_FROM_RELAXNGV => b"Relax-NG validity ",
        sys::XML_FROM_CATALOG => b"Catalog ",
        sys::XML_FROM_C14N => b"C14N ",
        sys::XML_FROM_XSLT => b"XSLT ",
        sys::XML_FROM_I18N => b"encoding ",
        _ => b"",
    }
}

/// `xmlParserPrintFileContextInternal` in 2.9.13: the input's line around
/// the position, 80 bytes at most, and a line with `^` under the column.
///
/// # Safety
/// `input` is null or a live parser input.
unsafe fn file_context(input: *mut sys::xmlParserInput, out: &mut Vec<u8>) {
    const ROOM: usize = 80;
    if input.is_null() {
        return;
    }
    // SAFETY: the caller's contract.
    let i = unsafe { &*input };
    if i.cur.is_null() || i.base.is_null() {
        return;
    }
    let (base, at) = (i.base, i.cur);
    // SAFETY: `base..cur` is one live buffer.
    let read = |p: *const u8| unsafe { *p };
    let mut cur = at;
    while cur > base && matches!(read(cur), b'\n' | b'\r') {
        // SAFETY: still inside the buffer, above `base`.
        cur = unsafe { cur.sub(1) };
    }
    let mut n = 0;
    while n < ROOM && cur > base && !matches!(read(cur), b'\n' | b'\r') {
        n += 1;
        // SAFETY: as above.
        cur = unsafe { cur.sub(1) };
    }
    if matches!(read(cur), b'\n' | b'\r') {
        // SAFETY: the byte after an end of line is inside the buffer.
        cur = unsafe { cur.add(1) };
    }
    // SAFETY: both inside the one buffer, `cur` at or before `at`.
    let col = usize::try_from(unsafe { at.offset_from(cur) }).unwrap_or(0);
    let mut content = Vec::with_capacity(ROOM + 1);
    while content.len() < ROOM && !matches!(read(cur), 0 | b'\n' | b'\r') {
        content.push(read(cur));
        // SAFETY: the buffer ends at a NUL this loop stops on.
        cur = unsafe { cur.add(1) };
    }
    out.extend_from_slice(&content);
    out.push(b'\n');
    let mut caret = Vec::with_capacity(ROOM + 1);
    for (k, byte) in content.iter().enumerate() {
        if k >= col || k >= ROOM - 1 {
            break;
        }
        caret.push(if *byte == b'\t' { b'\t' } else { b' ' });
    }
    caret.push(b'^');
    out.extend_from_slice(&caret);
    out.push(b'\n');
}

impl Raised {
    /// `f` served with php's structured handler set, and every error libxml
    /// raised meanwhile copied out beside `f`'s answer; the handler that was
    /// set before is set again after, so a callback that reaches libxml
    /// inside `f` nests. Inside a [`Raised::capture_io`] the loads `f`
    /// makes read through that call's request.
    pub fn capture<R>(f: impl FnOnce() -> R) -> (R, Vec<Raised>) {
        Call::run(None, ptr::null_mut(), f)
    }

    /// [`Raised::capture`] for a call whose loads read through php's
    /// streams: the request lent to libxml's reading and writing callbacks
    /// and to the external entity loader for the length of `f`, as
    /// `php_libxml_input_buffer_create_filename`,
    /// `php_libxml_output_buffer_create_filename` and
    /// `php_libxml_external_entity_loader` reach php from inside a
    /// parse; and what a program's loader threw, handed back.
    pub(crate) fn capture_io<R>(
        req: &mut Request,
        routing: Option<Routing>,
        f: impl FnOnce() -> R,
    ) -> (R, Vec<Raised>, Option<Thrown>) {
        let mut io = Io {
            req: ptr::from_mut(req),
            thrown: None,
        };
        let (answer, raised) = {
            let _creators = loader::Creators::set();
            Call::run(routing, &raw mut io, f)
        };
        (answer, raised, io.thrown)
    }

    /// [`Raised::capture_io`] for a library over libxml2 whose own parses
    /// read through php's streams, ext/xsl's libxslt compiling a
    /// stylesheet that imports another.
    pub fn capture_request<R>(
        req: &mut Request,
        f: impl FnOnce() -> R,
    ) -> (R, Vec<Raised>, Option<Thrown>) {
        Raised::capture_io(req, None, f)
    }

    /// `php_libxml_ctx_error(NULL, "%s", message)` from the error handler
    /// of a library over libxml2, which libxml's own capture does not
    /// see: libxslt's `xsltGenericError`, which `xsl_libxslt_error_handler`
    /// hands php's routing. It joins the errors of the capture running on
    /// this thread, routed as the generic handler's, and is dropped where
    /// none runs.
    pub fn generic(message: &[u8]) {
        let Some(call) = Call::current() else {
            return;
        };
        // SAFETY: the record of the capture running on this thread, live
        // while it runs, and nothing else borrows it during this callback.
        let call = unsafe { &mut *call };
        let mut error = Raised::internal(message);
        if call.thrown() {
            error.route = Route::Silent;
        }
        call.raised.push(error);
    }

    /// [`Raised::capture`] with the routes its errors take decided by
    /// `routing`.
    pub(crate) fn capture_with<R>(routing: Routing, f: impl FnOnce() -> R) -> (R, Vec<Raised>) {
        Call::run(Some(routing), ptr::null_mut(), f)
    }

    /// One phase of a [`Raised::capture_io`] call: `f` captured on its
    /// own, the routes its errors took decided by `routing` (the handlers
    /// php had put on the parser by then), and the errors handed to the
    /// enclosing call in the order they came.
    pub(crate) fn phase<R>(routing: Routing, f: impl FnOnce() -> R) -> R {
        let (answer, raised) = Call::run(Some(routing), ptr::null_mut(), f);
        match Call::current() {
            // SAFETY: the enclosing call's record, live while it runs.
            Some(parent) => unsafe { (*parent).raised.extend(raised) },
            None => drop(raised),
        }
        answer
    }

    /// `xmlGetLastError`, copied: what `libxml_get_last_error()` answers when
    /// php keeps no list of its own.
    #[must_use]
    pub fn last() -> Option<Raised> {
        // SAFETY: the thread's last error, or null.
        let last = unsafe { sys::xmlGetLastError() };
        if last.is_null() {
            return None;
        }
        // The stored record outlives the parser and the node it names, so
        // it is read through a copy that names neither: its strings are
        // the record's own and live while it stands.
        // SAFETY: `last` is the thread's live, aligned record.
        let mut copy = unsafe { ptr::read(last) };
        copy.ctxt = ptr::null_mut();
        copy.node = ptr::null_mut();
        let mut call = Call {
            raised: Vec::new(),
            routing: None,
            io: ptr::null_mut(),
            parent: ptr::null_mut(),
        };
        // SAFETY: `collect` reads the copy, whose strings are the live
        // record's, into `call`.
        unsafe { collect((&raw mut call).cast(), &raw mut copy) };
        call.raised.pop().filter(|e| e.code != 0)
    }

    /// `xmlResetLastError`.
    pub fn reset_last() {
        // SAFETY: no argument; the thread's own record.
        unsafe { sys::xmlResetLastError() };
    }
}

/// The handlers php had put on the libxml `ctxt` an error came from,
/// which decide the route it takes.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Routing {
    /// A parser's: `sax` is the handler table of the parser php configured
    /// (`dom_document_parser`'s `ctxt->sax->error = php_libxml_ctx_error`),
    /// null before it has; see [`Routing::parser_route`].
    Parser { sax: *mut sys::xmlSAXHandler },
    /// A validator's: `xmlSchemaSetParserErrors` and its kin set to php's
    /// generic handler, so its own errors are their bare message, and the
    /// errors of a parser libxml opens inside it to read a schema, which
    /// php never configured, are libxml's own text through the same
    /// handler.
    Validator,
}

impl Routing {
    /// The route of an error `parser` raised at `level`, from the handler
    /// libxml would have called on it: none where `LIBXML_NOERROR` or
    /// `LIBXML_NOWARNING` took it off, php's where the parser reads the
    /// table php configured (a parser libxml opens for an external entity
    /// reads it once the entity is loaded), and libxml's own otherwise.
    ///
    /// # Safety
    /// `parser` is the live parser that raised the error.
    unsafe fn parser_route(
        configured: *mut sys::xmlSAXHandler,
        parser: *mut sys::xmlParserCtxt,
        level: Level,
    ) -> Route {
        // SAFETY: the caller's contract.
        let sax = unsafe { (*parser).sax };
        if sax.is_null() {
            return Route::Default;
        }
        // SAFETY: a live parser's handler table.
        let table = unsafe { &*sax };
        let handler = if level == Level::Warning {
            table.warning
        } else {
            table.error
        };
        if handler.is_null() {
            Route::Silent
        } else if !configured.is_null() && sax == configured {
            Route::Parser
        } else {
            Route::Default
        }
    }

    /// The routes of `raised` decided at the end of a validator's call. A
    /// route a throw already decided stays.
    pub(crate) fn apply(self, raised: &mut [Raised]) {
        if let Routing::Validator = self {
            for error in raised {
                error.route = match error.route {
                    Route::Silent => Route::Silent,
                    Route::Parser => Route::Default,
                    Route::Generic | Route::Default => Route::Generic,
                };
            }
        }
    }
}

/// The request a [`Raised::capture_io`] call lends libxml's callbacks, and
/// the first throwable a program's code raised from inside the call.
pub(crate) struct Io {
    req: *mut Request,
    thrown: Option<Thrown>,
}

/// One capture's record, what the thread's structured handler data points
/// at while the capture runs: the errors collected, the rule that routes
/// them, the request the loads read through (inherited from the capture it
/// nests in), and that capture.
///
/// libxml2 keeps the structured handler and its data per thread, and
/// this crate is the only one that sets them, to [`collect`] and a record
/// of this type for the length of one call from one request. So the
/// thread's handler data is the one place a callback libxml makes without
/// data of its own (the two buffer creators, the entity loader)
/// finds the request that made the call: nothing is kept past the call,
/// and nothing is shared between threads (ADR-0005).
pub(crate) struct Call {
    raised: Vec<Raised>,
    routing: Option<Routing>,
    io: *mut Io,
    parent: *mut Call,
}

impl Call {
    /// The record of the capture running on this thread, if one is.
    fn current() -> Option<*mut Call> {
        // SAFETY: the address of this thread's handler.
        let handler_at = unsafe { sys::__xmlStructuredError() };
        // SAFETY: the thread's own global, read by copy.
        let handler = unsafe { *handler_at };
        let ours: sys::xmlStructuredErrorFunc = collect;
        if handler.map(|h| h as usize) != Some(ours as usize) {
            return None;
        }
        // SAFETY: the address of this thread's handler context.
        let context_at = unsafe { sys::__xmlStructuredErrorContext() };
        // SAFETY: the thread's own global, a live record while `collect`
        // is the handler.
        let context = unsafe { *context_at };
        (!context.is_null()).then(|| context.cast::<Call>())
    }

    /// `f` served with a record of its own as the thread's handler data, its
    /// errors routed by `routing` at the end, and the thread's handler and
    /// data put back after.
    fn run<R>(routing: Option<Routing>, io: *mut Io, f: impl FnOnce() -> R) -> (R, Vec<Raised>) {
        // SAFETY: the address of this thread's handler.
        let handler = unsafe { sys::__xmlStructuredError() };
        // SAFETY: the address of this thread's handler context.
        let context = unsafe { sys::__xmlStructuredErrorContext() };
        // SAFETY: both addresses are the thread's own globals, read by copy.
        let saved_handler = unsafe { *handler };
        // SAFETY: as above.
        let saved_context = unsafe { *context };
        let parent = Call::current().unwrap_or(ptr::null_mut());
        let io = if io.is_null() && !parent.is_null() {
            // SAFETY: the enclosing record, live while it runs.
            unsafe { (*parent).io }
        } else {
            io
        };
        let mut call = Call {
            raised: Vec::new(),
            routing,
            io,
            parent,
        };
        // SAFETY: `call` outlives the call, and the handler is taken off
        // again before it is dropped.
        unsafe { sys::xmlSetStructuredErrorFunc((&raw mut call).cast(), Some(collect)) };
        let (mut generic, mut generic_ctx) = (ptr::null_mut(), ptr::null_mut());
        // SAFETY: two slots of this frame; php's generic handler set on this
        // thread, taken off again below.
        unsafe { php_libxml_generic_push(&raw mut generic, &raw mut generic_ctx) };
        let answered = f();
        // SAFETY: the handler and context the push replaced.
        unsafe { php_libxml_generic_pop(generic, generic_ctx) };
        // SAFETY: the handler and context that were set before.
        unsafe { sys::xmlSetStructuredErrorFunc(saved_context, saved_handler) };
        if let Some(routing) = call.routing {
            routing.apply(&mut call.raised);
        }
        (answered, call.raised)
    }

    /// Whether a program's code threw from inside the call.
    fn thrown(&self) -> bool {
        // SAFETY: the call's `Io`, live while the call runs, or null.
        !self.io.is_null() && unsafe { (*self.io).thrown.is_some() }
    }
}

// ---- php_libxml_ref_obj, php_libxml_node_ptr, php_libxml_node_object

/// `php_libxml_class_type`: which of dom's two class families the nodes
/// of one document are handed out as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClassType {
    #[default]
    Unset,
    Legacy,
    Modern,
}

/// `libxml_doc_props`' seven switches, `DOMDocument`'s parse and save
/// options: what `dom_get_doc_props_read_only` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // php's seven switches, each one property of the document
pub struct Switches {
    pub format: bool,
    pub validate_on_parse: bool,
    pub resolve_externals: bool,
    pub preserve_whitespace: bool,
    pub substitute_entities: bool,
    pub strict_error: bool,
    pub recover: bool,
}

/// `default_doc_props`.
impl Default for Switches {
    fn default() -> Switches {
        Switches {
            format: false,
            validate_on_parse: false,
            resolve_externals: false,
            preserve_whitespace: true,
            substitute_entities: false,
            strict_error: true,
            recover: false,
        }
    }
}

/// `sizeof(libxml_doc_props)` on the pinned 64-bit build: the class
/// map's pointer and the seven switches (`Z/ext/libxml/php_libxml.h`).
const DOC_PROPS: usize = 16;

/// The rows of a class map: each base class with the class a program
/// registered for it, in the table's order.
pub type ClassRows = Vec<(ClassId, ClassId)>;

/// `libxml_doc_props`, `emalloc`'d by `dom_get_doc_props` the first time
/// a switch or the class map is written and freed with the document by
/// `php_libxml_decrement_doc_ref_directly`: the switches, and the classes
/// `registerNodeClass` put in place of dom's own, a table on the
/// request's heap `ALLOC_HASHTABLE`d at the first registration.
///
/// ```php
/// $doc = Dom\XMLDocument::createFromString('<r/>');
/// $c = $doc->cloneNode(true); unset($c);
/// $m = memory_get_usage();
/// $c = $doc->cloneNode(true);
/// echo memory_get_usage() - $m, "\n";
/// $d = new DOMDocument();
/// $m = memory_get_usage();
/// $d->formatOutput = true;
/// echo memory_get_usage() - $m, "\n";
/// class E extends DOMElement {}
/// $d->registerNodeClass('DOMElement', 'E');
/// echo memory_get_usage() - $m, "\n";
/// $d->registerNodeClass('DOMElement', null);
/// echo memory_get_usage() - $m, "\n";
/// $d->loadXML('<r/>');
/// echo memory_get_usage() - $m, "\n";
/// $c = $d->cloneNode();
/// echo memory_get_usage() - $m, "\n";
/// unset($c, $d);
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 280
/// 16
/// 392
/// 392
/// 392
/// 320
/// -416
/// ```
#[derive(Debug, Default)]
pub struct DocProps {
    pub switches: Switches,
    /// `classmap`: base class to the class a program registered for it,
    /// `zend_hash_update_ptr` over the base's name.
    classmap: Option<crate::table::Table<(ClassId, ClassId)>>,
    /// The struct's `emalloc` on the request's heap.
    charge: Option<Emalloc>,
}

impl DocProps {
    /// `dom_get_doc_classmap`: the class registered in place of `base`,
    /// `base` where none is.
    #[must_use]
    pub fn mapped(&self, base: ClassId) -> ClassId {
        self.classmap
            .as_ref()
            .and_then(|map| map.rows().find(|(b, _)| *b == base))
            .map_or(base, |(_, class)| *class)
    }

    /// The class map's rows, `None` where none was made.
    #[must_use]
    pub fn class_rows(&self) -> Option<ClassRows> {
        self.classmap
            .as_ref()
            .map(|map| map.rows().copied().collect())
    }

    /// `dom_set_doc_classmap`: the table made at the first registration,
    /// then `class` written over `base`'s row in place, or the row
    /// deleted where `class` is `None`; the `Err` is the memory limit.
    pub fn set_class(
        &mut self,
        req: &mut Request,
        base: ClassId,
        class: Option<ClassId>,
    ) -> Result<(), Thrown> {
        let map = match (&mut self.classmap, class) {
            (Some(map), _) => map,
            (None, None) => return Ok(()),
            (None, Some(_)) => self.classmap.insert(crate::table::Table::header(req)?),
        };
        let at = map.rows().position(|(b, _)| *b == base);
        match (at, class) {
            (Some(at), Some(class)) => {
                if let Some(row) = map.rows_mut().nth(at) {
                    row.1 = class;
                }
            }
            (Some(at), None) => {
                map.remove(at);
            }
            (None, Some(class)) => map.add(req, (base, class))?,
            (None, None) => {}
        }
        Ok(())
    }

    /// `dom_copy_document_ref`'s copy of a class map: a table
    /// `ALLOC_HASHTABLE`d and each row added in order, as `zend_hash_copy`
    /// adds them; the `Err` is the memory limit.
    pub fn copy_classmap(&mut self, req: &mut Request, rows: ClassRows) -> Result<(), Thrown> {
        let map = crate::table::Table::header(req)?;
        if let Some(mut old) = self.classmap.replace(map) {
            old.free(req);
        }
        if let Some(map) = self.classmap.as_mut() {
            for row in rows {
                map.add(req, row)?;
            }
        }
        Ok(())
    }

    /// `zend_hash_destroy` and `FREE_HASHTABLE` of the class map, then
    /// `efree(document->doc_props)`.
    pub(crate) fn free(mut self, req: &mut Request) {
        if let Some(mut map) = self.classmap.take() {
            map.free(req);
        }
        if let Some(charge) = self.charge.take() {
            charge.free(req);
        }
    }
}

/// `php_libxml_quirks_mode`: the compatibility the HTML5 parser left a
/// document in, which CSS selector matching reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quirks {
    #[default]
    NoQuirks,
    Quirks,
    LimitedQuirks,
}

/// `php_libxml_private_data_header`: what the extension that made a
/// document keeps beside libxml's tree for the document's lifetime, dom's
/// namespace mapper and template contents for its modern classes. It is
/// dropped when the document's last reference goes, before `xmlFreeDoc`,
/// as php runs the header's `dtor` first.
pub trait PrivateData: std::any::Any {
    /// The header's `ns_hook`: an element whose namespace carries the tag
    /// ([`NodeObject::add_ns_tag`]) is being freed by
    /// `php_libxml_node_free`.
    fn element_freed(&mut self, element: Node);

    /// The header's `dtor` with the request at hand: what the data holds
    /// of the request's heap given back, as php's `efree`s it, before the
    /// data is dropped. Data dropped with no request at hand leaves that
    /// to the request's end.
    fn destroy(&mut self, req: &mut Request) {
        let _ = req;
    }
}

/// `php_libxml_ref_obj`, with the `emalloc(sizeof(php_libxml_ref_obj))`
/// php makes for it held on the request's heap ([`REF_OBJ`]).
struct DocRef {
    doc: *mut sys::xmlDoc,
    refcount: u32,
    props: Option<DocProps>,
    /// `cache_tag.modification_nr`.
    modification: usize,
    class_type: ClassType,
    quirks: Quirks,
    private: Option<Box<dyn PrivateData>>,
    /// `handlers`: `None` is `php_libxml_default_document_handlers`.
    handlers: Option<&'static dyn crate::DocumentHandlers>,
    /// The struct's allocation on the request's heap, given back where
    /// php's `efree(document)` gives it back.
    charge: Option<Emalloc>,
}

/// `sizeof(php_libxml_ref_obj)` on the pinned 64-bit build: `ptr`,
/// `doc_props`, `cache_tag`, `private_data`, `handlers`, `refcount` and
/// the two eight-bit fields (`Z/ext/libxml/php_libxml.h:110-119`).
const REF_OBJ: usize = 48;

/// `sizeof(php_libxml_node_ptr)`: `node`, `refcount` and `_private`
/// (`Z/ext/libxml/php_libxml.h:121-125`).
const NODE_PTR: usize = 24;

/// `emalloc(size)` for one of the structs php's libxml glue allocates
/// on the request's heap; none where the memory limit refused it, whose
/// fatal the request records, as php's `emalloc` bails out.
fn charge(req: &mut Request, size: usize) -> Option<Emalloc> {
    Emalloc::new(req, size).ok()
}

/// `efree` of such a struct's allocation, where a request is at hand:
/// one freed where none is, a payload dropped past its `free_obj`, is
/// the request's end's, as the heap's release takes it.
fn give_back(req: Option<&mut Request>, charge: Option<Emalloc>) {
    if let (Some(req), Some(charge)) = (req, charge) {
        charge.free(req);
    }
}

/// `php_libxml_node_ptr`: what a node's `_private` names while an object
/// stands for it, with php's `emalloc(sizeof(php_libxml_node_ptr))` held
/// on the request's heap ([`NODE_PTR`]).
struct NodePtr {
    node: *mut sys::xmlNode,
    refcount: u32,
    /// php's `_private`: the object's libxml half.
    wrapper: *mut Wrapper,
    /// The struct's allocation on the request's heap.
    charge: Option<Emalloc>,
}

/// The fields of `php_libxml_node_object`, laid down apart from the
/// engine's heap so a free walk that meets the node can reach them
/// without the request, as php's pointer into the object does.
struct Wrapper {
    node: *mut NodePtr,
    document: *mut DocRef,
    object: Option<PhpObject>,
}

/// `php_libxml_node_object`: the libxml half of a node object's payload,
/// one allocation it owns.
pub struct NodeObject(NonNull<Wrapper>);

// SAFETY: a node object, its proxy, its document and every libxml node
// they name are reached only from the request that made them, served by
// one thread at a time and moved between threads only whole (ADR-0005).
unsafe impl Send for NodeObject {}

impl Default for NodeObject {
    fn default() -> NodeObject {
        let wrapper = Box::new(Wrapper {
            node: ptr::null_mut(),
            document: ptr::null_mut(),
            object: None,
        });
        NodeObject(NonNull::from(Box::leak(wrapper)))
    }
}

impl Drop for NodeObject {
    /// What a `free_obj` left: the object's node and document let go of
    /// with no request at hand, so what they free of the request's heap
    /// is the request's end's to release.
    fn drop(&mut self) {
        self.release_in(None);
        // SAFETY: the allocation `default` leaked; after `release` no
        // proxy names it, so this is its one owner.
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

impl NodeObject {
    fn w(&self) -> *mut Wrapper {
        self.0.as_ptr()
    }

    /// The libxml half borrowed off the payload that owns it, for a call
    /// that takes the request beside it, which the payload's own borrow
    /// would hold: the same half, freeing nothing when it goes. The borrow
    /// is used and dropped within the native call that took it, before
    /// anything can free the object, as [`Node::wrapper`]'s is.
    #[must_use]
    pub fn borrow(&self) -> Borrowed {
        Borrowed(std::mem::ManuallyDrop::new(NodeObject(self.0)))
    }

    /// The object this is the half of, which the node's proxy hands back
    /// as `php_dom_object_get_data` does.
    pub fn set_object(&mut self, object: PhpObject) {
        // SAFETY: the wrapper this owns; no reference into it is held.
        unsafe { (*self.w()).object = Some(object) };
    }

    /// The node, `dom_object_get_node`: null where there is none.
    #[must_use]
    pub(crate) fn node(&self) -> *mut sys::xmlNode {
        // SAFETY: the wrapper this owns.
        let proxy = unsafe { (*self.w()).node };
        if proxy.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: a proxy the wrapper names is live until its count falls
        // to zero, which clears the wrapper's pointer first.
        unsafe { (*proxy).node }
    }

    /// The node the object stands for, `dom_object_get_node`.
    #[must_use]
    pub fn bound(&self) -> Option<Node> {
        Node::from_raw(self.node())
    }

    /// The document the object counts, as a node.
    #[must_use]
    pub fn document_node(&self) -> Option<Node> {
        Node::from_raw(self.document().cast())
    }

    /// Whether the object stands for anything, `intern->ptr != NULL`.
    #[must_use]
    pub fn is_bound(&self) -> bool {
        // SAFETY: the wrapper this owns.
        !unsafe { (*self.w()).node }.is_null()
    }

    /// `php_libxml_increment_node_ptr(object, node, private_data)`, where
    /// `claim` is dom's `private_data`, the object named as the one that
    /// stands for the node where none is yet, and its absence simplexml's
    /// `NULL`, the node counted and nothing named ("only dom uses
    /// `_private`").
    ///
    /// # Safety
    /// `node` is a live node, and where `claim` this object is the one
    /// that stands for it or will.
    pub(crate) unsafe fn increment_node_ptr(
        &mut self,
        req: &mut Request,
        node: *mut sys::xmlNode,
        claim: bool,
    ) -> u32 {
        if node.is_null() {
            return 0;
        }
        let w = self.w();
        // SAFETY: the wrapper this owns.
        let current = unsafe { (*w).node };
        if !current.is_null() {
            // SAFETY: a proxy the wrapper names is live.
            if unsafe { (*current).node } == node {
                // SAFETY: as above.
                return unsafe { (*current).refcount };
            }
            self.decrement_node_ptr(req);
        }
        // SAFETY: the caller's contract: a live node.
        let private = unsafe { (*node).private }.cast::<NodePtr>();
        let named = if claim { w } else { ptr::null_mut() };
        if private.is_null() {
            let proxy = Box::into_raw(Box::new(NodePtr {
                node,
                refcount: 1,
                wrapper: named,
                charge: charge(req, NODE_PTR),
            }));
            // SAFETY: the wrapper this owns.
            unsafe { (*w).node = proxy };
            // SAFETY: a live node; `_private` is the application's field.
            unsafe { (*node).private = proxy.cast() };
            return 1;
        }
        // SAFETY: a proxy the node names is live while the node names it.
        let proxy = unsafe { &mut *private };
        proxy.refcount += 1;
        if proxy.wrapper.is_null() {
            proxy.wrapper = named;
        }
        // SAFETY: the wrapper this owns.
        unsafe { (*w).node = private };
        proxy.refcount
    }

    /// `php_libxml_decrement_node_ptr`: the proxy counted once less, and
    /// this object naming none after (`object->node = NULL`).
    pub fn decrement_node_ptr(&mut self, req: &mut Request) -> u32 {
        self.decrement_node_ptr_in(Some(req))
    }

    fn decrement_node_ptr_in(&mut self, req: Option<&mut Request>) -> u32 {
        let w = self.w();
        // SAFETY: the wrapper this owns.
        let proxy = unsafe { (*w).node };
        if proxy.is_null() {
            return 0;
        }
        // SAFETY: a proxy the wrapper names is live.
        let left = unsafe { decrement_node_ptr_ref(proxy, req) };
        // SAFETY: the wrapper this owns.
        unsafe { (*w).node = ptr::null_mut() };
        left
    }

    /// `php_libxml_increment_doc_ref`: one more owner of the document
    /// this already holds, or a first one of `doc`.
    ///
    /// # Safety
    /// `doc` is null or a live document no other `php_libxml_ref_obj`
    /// owns where this holds none.
    pub(crate) unsafe fn increment_doc_ref(
        &mut self,
        req: &mut Request,
        doc: *mut sys::xmlDoc,
    ) -> u32 {
        let w = self.w();
        // SAFETY: the wrapper this owns.
        let current = unsafe { (*w).document };
        if !current.is_null() {
            // SAFETY: a document the wrapper counts is live.
            let d = unsafe { &mut *current };
            d.refcount += 1;
            return d.refcount;
        }
        if doc.is_null() {
            return 0;
        }
        let d = Box::into_raw(Box::new(DocRef {
            doc,
            refcount: 1,
            props: None,
            modification: 1,
            class_type: ClassType::Unset,
            quirks: Quirks::NoQuirks,
            private: None,
            handlers: None,
            charge: charge(req, REF_OBJ),
        }));
        // SAFETY: the wrapper this owns.
        unsafe { (*w).document = d };
        1
    }

    /// The document this counts, as a value another object can share:
    /// php's `intern->document` read, no count taken.
    #[must_use]
    pub(crate) fn shared_doc(&self) -> Option<SharedDoc> {
        // SAFETY: the wrapper this owns.
        NonNull::new(unsafe { (*self.w()).document }).map(SharedDoc)
    }

    /// `intern->document = shared; php_libxml_increment_doc_ref(intern,
    /// doc)`: one more owner of `shared` where one is given, of the
    /// document this already counts otherwise, and a first reference to
    /// `doc` where this counts none; what `php_dom_instantiate_object_helper`
    /// does so a node object shares its parent's document.
    ///
    /// # Safety
    /// `shared` is a document some live object counts, this counts no
    /// other one where `shared` is given, and `doc` is null or live.
    pub(crate) unsafe fn increment_doc_ref_shared(
        &mut self,
        req: &mut Request,
        shared: Option<SharedDoc>,
        doc: *mut sys::xmlDoc,
    ) -> u32 {
        if let Some(handle) = shared {
            // SAFETY: the wrapper this owns.
            unsafe { (*self.w()).document = handle.0.as_ptr() };
        }
        // SAFETY: the caller's contract.
        unsafe { self.increment_doc_ref(req, doc) }
    }

    /// `php_dom_transfer_document_ref_single_node`: `to` counted once more
    /// first, since this may be its last owner and the two the same, and
    /// then the document this counted let go of.
    ///
    /// # Safety
    /// `to` is a document some live object counts.
    pub(crate) unsafe fn transfer_document(&mut self, req: &mut Request, to: SharedDoc) {
        // SAFETY: the caller's contract.
        unsafe { (*to.0.as_ptr()).refcount += 1 };
        self.decrement_doc_ref(req);
        // SAFETY: the wrapper this owns.
        unsafe { (*self.w()).document = to.0.as_ptr() };
    }

    /// `php_libxml_decrement_doc_ref`.
    pub fn decrement_doc_ref(&mut self, req: &mut Request) -> u32 {
        self.decrement_doc_ref_in(Some(req))
    }

    fn decrement_doc_ref_in(&mut self, req: Option<&mut Request>) -> u32 {
        let w = self.w();
        // SAFETY: the wrapper this owns.
        let d = unsafe { (*w).document };
        if d.is_null() {
            return 0;
        }
        // SAFETY: as above.
        unsafe { (*w).document = ptr::null_mut() };
        // SAFETY: a document the wrapper counted is live.
        unsafe { decrement_doc_ref_directly(d, req) }
    }

    /// `php_libxml_node_decrement_resource`: the node let go of, freed
    /// with its subtree where nothing else holds it and it hangs from no
    /// parent, and the document let go of after.
    pub fn decrement_resource(&mut self, req: &mut Request) {
        self.decrement_resource_in(Some(req));
    }

    fn decrement_resource_in(&mut self, mut req: Option<&mut Request>) {
        let w = self.w();
        // SAFETY: the wrapper this owns.
        let proxy = unsafe { (*w).node };
        if !proxy.is_null() {
            // SAFETY: a proxy the wrapper names is live.
            let node = unsafe { (*proxy).node };
            let left = self.decrement_node_ptr_in(req.as_deref_mut());
            if left == 0 {
                // SAFETY: the node is live or null, and nothing names it.
                unsafe { Tree::free_resource(node, req.as_deref_mut()) };
            } else {
                // SAFETY: the proxy outlives this decrement since others
                // still count it.
                let proxy = unsafe { &mut *proxy };
                if proxy.wrapper == w {
                    proxy.wrapper = ptr::null_mut();
                }
            }
        }
        self.decrement_doc_ref_in(req);
    }

    /// `dom_objects_free_storage`'s libxml half: a document object lets
    /// go of its node and its document, any other node object of its
    /// resource. Idempotent, as the `ptr = NULL` after it makes php's.
    pub fn release(&mut self, req: &mut Request) {
        self.release_in(Some(req));
    }

    fn release_in(&mut self, mut req: Option<&mut Request>) {
        let node = self.node();
        if !node.is_null() {
            // SAFETY: a live node the proxy names.
            let kind = unsafe { (*node).type_ };
            if kind == sys::XML_DOCUMENT_NODE || kind == sys::XML_HTML_DOCUMENT_NODE {
                self.decrement_node_ptr_in(req.as_deref_mut());
                self.decrement_doc_ref_in(req.as_deref_mut());
            } else {
                self.decrement_resource_in(req.as_deref_mut());
            }
        }
        let w = self.w();
        // SAFETY: the wrapper this owns.
        unsafe {
            (*w).node = ptr::null_mut();
        }
        // A wrapper with no node may still count a document: a
        // `DOMNodeList` holds its document this way.
        self.decrement_doc_ref_in(req);
    }

    /// The document this counts, `intern->document->ptr`, null where it
    /// counts none.
    #[must_use]
    pub(crate) fn document(&self) -> *mut sys::xmlDoc {
        match self.doc_ref() {
            Some(d) => d.doc,
            None => ptr::null_mut(),
        }
    }

    fn doc_ref(&self) -> Option<&DocRef> {
        // SAFETY: the wrapper this owns.
        let d = unsafe { (*self.w()).document };
        // SAFETY: a counted document is live while this counts it.
        unsafe { d.as_ref() }
    }

    fn doc_ref_mut(&mut self) -> Option<&mut DocRef> {
        // SAFETY: the wrapper this owns.
        let d = unsafe { (*self.w()).document };
        // SAFETY: as above; `&mut self` is the one borrow of it here.
        unsafe { d.as_mut() }
    }

    /// Whether this counts a document at all, `intern->document != NULL`.
    #[must_use]
    pub fn has_document(&self) -> bool {
        self.doc_ref().is_some()
    }

    /// `dom_get_doc_props_read_only`: the switches, php's defaults where
    /// the document has none yet or there is no document.
    #[must_use]
    pub fn props(&self) -> Switches {
        self.doc_props().map(|p| p.switches).unwrap_or_default()
    }

    /// The document's `libxml_doc_props`, none where it has none yet or
    /// there is no document.
    #[must_use]
    pub fn doc_props(&self) -> Option<&DocProps> {
        self.doc_ref()?.props.as_ref()
    }

    /// `dom_get_doc_classmap`: the class registered in place of `base`
    /// for the document, `base` where none is.
    #[must_use]
    pub fn mapped(&self, base: ClassId) -> ClassId {
        self.doc_props().map_or(base, |p| p.mapped(base))
    }

    /// `dom_get_doc_props`: the props to write, `emalloc`'d from the
    /// defaults the first time; `None` where there is no document, whose
    /// write php makes on a copy nothing keeps. The `Err` is the memory
    /// limit.
    pub fn props_mut(&mut self, req: &mut Request) -> Result<Option<&mut DocProps>, Thrown> {
        let Some(d) = self.doc_ref_mut() else {
            return Ok(None);
        };
        if d.props.is_none() {
            let charge = Emalloc::new(req, DOC_PROPS)?;
            d.props = Some(DocProps {
                charge: Some(charge),
                ..DocProps::default()
            });
        }
        Ok(d.props.as_mut())
    }

    /// The props taken off the document, `document->doc_props = NULL`
    /// after reading them, as a reload keeps them across the new tree.
    pub fn take_props(&mut self) -> Option<DocProps> {
        self.doc_ref_mut().and_then(|d| d.props.take())
    }

    /// `document->doc_props = props`, any the document had freed; props
    /// with no document to take them freed with them.
    pub fn put_props(&mut self, req: &mut Request, props: Option<DocProps>) {
        let props = match self.doc_ref_mut() {
            Some(d) => std::mem::replace(&mut d.props, props),
            None => props,
        };
        if let Some(props) = props {
            props.free(req);
        }
    }

    /// `cache_tag.modification_nr` of the document.
    #[must_use]
    pub fn modification(&self) -> usize {
        self.doc_ref().map_or(0, |d| d.modification)
    }

    /// Set the document's modification number, as a reload carries the
    /// old number over.
    pub fn set_modification(&mut self, n: usize) {
        if let Some(d) = self.doc_ref_mut() {
            d.modification = n;
        }
    }

    /// `php_libxml_invalidate_node_list_cache`.
    pub fn invalidate(&mut self) {
        if let Some(d) = self.doc_ref_mut() {
            d.modification += 1;
        }
    }

    /// `document->class_type`.
    #[must_use]
    pub fn class_type(&self) -> ClassType {
        self.doc_ref().map_or(ClassType::Unset, |d| d.class_type)
    }

    /// Set `document->class_type`.
    pub fn set_class_type(&mut self, class_type: ClassType) {
        if let Some(d) = self.doc_ref_mut() {
            d.class_type = class_type;
        }
    }

    /// `document->quirks_mode`.
    #[must_use]
    pub fn quirks(&self) -> Quirks {
        self.doc_ref().map_or(Quirks::NoQuirks, |d| d.quirks)
    }

    /// Set `document->quirks_mode`.
    pub fn set_quirks(&mut self, quirks: Quirks) {
        if let Some(d) = self.doc_ref_mut() {
            d.quirks = quirks;
        }
    }

    /// `document->private_data`, read as the type the extension that set
    /// it put there; `None` where there is none or it is another type.
    #[must_use]
    pub fn private_data<T: PrivateData>(&self) -> Option<&T> {
        let data: &dyn std::any::Any = self.doc_ref()?.private.as_deref()?;
        data.downcast_ref::<T>()
    }

    /// `document->private_data`, to write.
    pub fn private_data_mut<T: PrivateData>(&mut self) -> Option<&mut T> {
        let data: &mut dyn std::any::Any = self.doc_ref_mut()?.private.as_deref_mut()?;
        data.downcast_mut::<T>()
    }

    /// `document->private_data = data`: what was there given up with the
    /// request at hand, the header's `dtor` served before the drop, as
    /// `php_libxml_decrement_doc_ref` serves it. On an object with no
    /// document, which php's makers never lay private data on (they set
    /// it on `intern->document`, which `php_libxml_increment_doc_ref`
    /// laid down first), `data` itself is given up the same way, so no
    /// private data is ever dropped with no request at hand.
    pub fn set_private_data(&mut self, req: &mut Request, data: Box<dyn PrivateData>) {
        let left = match self.doc_ref_mut() {
            Some(d) => d.private.replace(data),
            None => Some(data),
        };
        if let Some(mut left) = left {
            left.destroy(req);
        }
    }

    /// `document->handlers`: how the document serialises, libxml's own
    /// dumps where the extension that made it set none.
    #[must_use]
    pub fn document_handlers(&self) -> &'static dyn crate::DocumentHandlers {
        self.doc_ref()
            .and_then(|d| d.handlers)
            .unwrap_or(&crate::DefaultHandlers)
    }

    /// Set `document->handlers`, as `dom_set_xml_class` gives a modern
    /// document dom's own serialiser.
    pub fn set_document_handlers(&mut self, handlers: &'static dyn crate::DocumentHandlers) {
        if let Some(d) = self.doc_ref_mut() {
            d.handlers = Some(handlers);
        }
    }

    /// `dom_add_element_ns_hook`: the element given a namespace of its
    /// own, a copy of the one it has kept on the document's old list,
    /// tagged so that `php_libxml_node_free` calls this document's
    /// [`PrivateData::element_freed`] when it frees the element.
    pub fn add_ns_tag(&mut self, element: Node) {
        let w = self.w();
        // SAFETY: the wrapper this owns.
        let d = unsafe { (*w).document };
        if d.is_null() {
            return;
        }
        let Some(ns) = element.ns() else {
            return;
        };
        let Some(copy) = Ns::owned(ns.href().as_deref(), ns.prefix().as_deref()) else {
            return;
        };
        let tag = d.cast::<c_void>().wrapping_byte_add(NS_TAG_HOOK);
        copy.set_private(tag);
        element.set_ns(Some(copy));
        copy.move_to_old(element.doc());
    }

    /// The count of the document, `document->refcount`.
    #[must_use]
    pub fn doc_refcount(&self) -> u32 {
        self.doc_ref().map_or(0, |d| d.refcount)
    }

    /// `intern->document->ptr = NULL`: the tree the document reference
    /// owns handed to whatever takes it next, so the reference's last
    /// count frees its switches and its namespace mappings and leaves the
    /// tree alone. This is how ext/xsl's `importStylesheet` gives libxslt
    /// the document of the clone it compiled. Every object that counts
    /// the reference sees it detached.
    pub fn detach_document(&mut self) {
        if let Some(d) = self.doc_ref_mut() {
            d.doc = ptr::null_mut();
        }
    }
}

/// A document reference handed from one object to another, php's
/// `php_libxml_ref_obj *` copied: it counts nothing itself. It stays in
/// this crate: another reaches a document's reference only as the
/// [`DocOf`] checked against its node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SharedDoc(NonNull<DocRef>);

impl SharedDoc {
    /// The document the reference names, `ref->ptr`.
    ///
    /// # Safety
    /// Some live object counts the document.
    pub(crate) unsafe fn doc(self) -> *mut sys::xmlDoc {
        // SAFETY: the caller's contract.
        unsafe { (*self.0.as_ptr()).doc }
    }
}

/// What an export answers for an object, `php_libxml_import_node`'s node
/// and the `php_libxml_node_object` php reads beside it through the
/// object's handler offset: the node the object stands for and the
/// document it counts, read off the live object within one native call.
#[derive(Debug, Clone, Copy)]
pub struct Exported {
    node: Node,
    document: Option<NonNull<DocRef>>,
}

impl Exported {
    /// The export of `node` for the object whose libxml half is `obj`.
    #[must_use]
    pub fn of(node: Node, obj: &NodeObject) -> Exported {
        Exported {
            node,
            document: obj.shared_doc().map(|d| d.0),
        }
    }

    /// The node, `php_libxml_import_node`'s answer.
    #[must_use]
    pub fn node(&self) -> Node {
        self.node
    }

    /// The document reference the object counts, which [`DocOf::exported`]
    /// checks against the node; `None` is php's `nodeobj->document == NULL`.
    #[must_use]
    pub(crate) fn document(&self) -> Option<SharedDoc> {
        self.document.map(SharedDoc)
    }

    /// Whether the object counts a document at all, php's
    /// `nodeobj->document != NULL`.
    #[must_use]
    pub fn has_document(&self) -> bool {
        self.document.is_some()
    }

    /// `nodeobj->document->class_type`.
    #[must_use]
    pub fn class_type(&self) -> ClassType {
        // SAFETY: an export is made off a live object that counts the
        // document, within the native call that reads it.
        self.document
            .map_or(ClassType::Unset, |d| unsafe { (*d.as_ptr()).class_type })
    }

    /// Set `nodeobj->document->class_type`, as `dom_import_simplexml`
    /// locks the classes the document's nodes are handed out as.
    pub fn set_class_type(&self, class_type: ClassType) {
        if let Some(d) = self.document {
            // SAFETY: as in `class_type`.
            unsafe { (*d.as_ptr()).class_type = class_type };
        }
    }

    /// `dom_get_doc_classmap(nodeobj->document, base)`.
    #[must_use]
    pub fn mapped(&self, base: ClassId) -> ClassId {
        // SAFETY: as in `class_type`.
        self.document
            .and_then(|d| unsafe { (*d.as_ptr()).props.as_ref() }.map(|p| p.mapped(base)))
            .unwrap_or(base)
    }
}

/// The libxml half of the object that stands for a node, borrowed where a
/// walk of the tree meets it: php's `node->_private->_private`. It
/// dereferences to the [`NodeObject`] its object owns and frees nothing
/// when it goes.
pub struct Borrowed(std::mem::ManuallyDrop<NodeObject>);

impl std::ops::Deref for Borrowed {
    type Target = NodeObject;
    fn deref(&self) -> &NodeObject {
        &self.0
    }
}

impl std::ops::DerefMut for Borrowed {
    fn deref_mut(&mut self) -> &mut NodeObject {
        &mut self.0
    }
}

/// `php_libxml.h`'s tree functions over libxml2 nodes: the object a node
/// answers as, the frees that keep what objects name, and the default
/// serialisers.
pub(crate) struct Tree;

impl Tree {
    /// The object's libxml half for `node`, `None` where no object stands for
    /// it.
    ///
    /// # Safety
    /// `node` is null or live, and nothing else borrows that object's half
    /// while the borrow lives.
    #[must_use]
    pub(crate) unsafe fn borrowed(node: *mut sys::xmlNode) -> Option<Borrowed> {
        // SAFETY: the caller's contract.
        let wrapper = unsafe { doc_wrapper(node) };
        NonNull::new(wrapper).map(|w| Borrowed(std::mem::ManuallyDrop::new(NodeObject(w))))
    }

    /// The object a node's proxy names, `php_dom_object_get_data`: the one
    /// php hands back again for the same node.
    ///
    /// # Safety
    /// `node` is null or live.
    #[must_use]
    pub(crate) unsafe fn object_of(node: *mut sys::xmlNode) -> Option<PhpObject> {
        if node.is_null() {
            return None;
        }
        // SAFETY: the caller's contract.
        let proxy = unsafe { (*node).private }.cast::<NodePtr>();
        if proxy.is_null() {
            return None;
        }
        // SAFETY: a proxy a node names is live.
        let wrapper = unsafe { (*proxy).wrapper };
        if wrapper.is_null() {
            return None;
        }
        // SAFETY: a wrapper a proxy names is owned by a live object.
        unsafe { (*wrapper).object }
    }

    /// `php_libxml_invalidate_node_list_cache_from_doc`.
    ///
    /// # Safety
    /// `doc` is null or live.
    pub(crate) unsafe fn invalidate_from_doc(doc: *mut sys::xmlDoc) {
        // SAFETY: the caller's contract; a document is a node.
        let w = unsafe { doc_wrapper(doc.cast()) };
        if w.is_null() {
            return;
        }
        // SAFETY: a wrapper a proxy names is owned by a live object.
        let d = unsafe { (*w).document };
        if !d.is_null() {
            // SAFETY: a counted document is live.
            unsafe { (*d).modification += 1 };
        }
    }

    /// `php_libxml_set_old_ns`.
    ///
    /// # Safety
    /// `doc` is null or live, and `ns` a live declaration no list holds.
    pub(crate) unsafe fn set_old_ns(doc: *mut sys::xmlDoc, ns: *mut sys::xmlNs) {
        // SAFETY: the caller's contract; a one-long list.
        unsafe { set_old_ns_list(doc, ns, ns) };
    }

    /// `php_libxml_node_free_list`: a sibling list freed, each node some
    /// object still names unlinked and kept instead, its namespaces
    /// reconciled so they stay declared inside it.
    ///
    /// # Safety
    /// `node` is null or the first of a live list nothing else frees.
    pub(crate) unsafe fn free_list(node: *mut sys::xmlNode, mut req: Option<&mut Request>) {
        let mut cur = node;
        while !cur.is_null() {
            // SAFETY: a live node of the list.
            let n = unsafe { &mut *cur };
            if !n.private.is_null() {
                let next = n.next;
                // SAFETY: a live node.
                unsafe { sys::xmlUnlinkNode(cur) };
                if n.type_ == sys::XML_ELEMENT_NODE {
                    // SAFETY: a proxy a node names is live.
                    let wrapper = unsafe { (*n.private.cast::<NodePtr>()).wrapper };
                    if !wrapper.is_null() {
                        // SAFETY: a wrapper a proxy names is owned by a live object.
                        let d = unsafe { (*wrapper).document };
                        // SAFETY: the wrapper's document is live while counted.
                        let legacy = d.is_null() || unsafe { (*d).class_type } != ClassType::Modern;
                        if legacy {
                            // SAFETY: a live, unlinked element and its
                            // document or none.
                            unsafe { reconcile_detached(cur) };
                        }
                    }
                }
                cur = next;
                continue;
            }
            match n.type_ {
                sys::XML_ENTITY_REF_NODE | sys::XML_NOTATION_NODE => {}
                // SAFETY: an entity declaration node is an `xmlEntity`.
                sys::XML_ENTITY_DECL => unsafe { unlink_entity_decl(cur.cast()) },
                sys::XML_ATTRIBUTE_NODE => {
                    // SAFETY: an attribute node is an `xmlAttr`.
                    let atype = unsafe { (*cur.cast::<sys::xmlAttr>()).atype };
                    if !n.doc.is_null() && atype == sys::XML_ATTRIBUTE_ID {
                        // SAFETY: a live document and attribute.
                        unsafe { sys::xmlRemoveID(n.doc, cur.cast()) };
                    }
                    // SAFETY: the attribute's value list.
                    unsafe { Tree::free_list(n.children, req.as_deref_mut()) };
                }
                sys::XML_ATTRIBUTE_DECL
                | sys::XML_DTD_NODE
                | sys::XML_DOCUMENT_TYPE_NODE
                | sys::XML_NAMESPACE_DECL
                | sys::XML_TEXT_NODE => {
                    // SAFETY: the node's children.
                    unsafe { Tree::free_list(n.children, req.as_deref_mut()) };
                }
                _ => {
                    // SAFETY: the node's children and attributes.
                    unsafe { Tree::free_list(n.children, req.as_deref_mut()) };
                    // SAFETY: as above.
                    unsafe { Tree::free_list(n.properties.cast(), req.as_deref_mut()) };
                }
            }
            let next = n.next;
            // SAFETY: a live node, unlinked, unregistered and freed once.
            unsafe { sys::xmlUnlinkNode(cur) };
            // SAFETY: as above.
            unsafe { unregister_node(cur, req.as_deref_mut()) };
            // SAFETY: as above.
            unsafe { node_free(cur) };
            cur = next;
        }
    }

    /// `php_libxml_node_free_resource`: a node no object names any more,
    /// freed with its subtree where it hangs from no parent.
    ///
    /// # Safety
    /// `node` is null or live.
    pub(crate) unsafe fn free_resource(node: *mut sys::xmlNode, mut req: Option<&mut Request>) {
        if node.is_null() {
            return;
        }
        // SAFETY: the caller's contract.
        let n = unsafe { &mut *node };
        match n.type_ {
            sys::XML_DOCUMENT_NODE | sys::XML_HTML_DOCUMENT_NODE => {}
            sys::XML_ENTITY_REF_NODE => {
                // SAFETY: a live node.
                unsafe { unregister_node(node, req.as_deref_mut()) };
                if n.parent.is_null() {
                    // SAFETY: an unlinked reference, whose children are the
                    // declaration's and not its own.
                    unsafe { node_free(node) };
                }
            }
            _ => {
                if n.parent.is_null() || n.type_ == sys::XML_NAMESPACE_DECL {
                    // SAFETY: the subtree the node owns.
                    unsafe { Tree::free_list(n.children, req.as_deref_mut()) };
                    if n.type_ == sys::XML_ELEMENT_NODE {
                        // SAFETY: as above.
                        unsafe { Tree::free_list(n.properties.cast(), req.as_deref_mut()) };
                    }
                    // SAFETY: a live node.
                    unsafe { unregister_node(node, req.as_deref_mut()) };
                    // SAFETY: as above, freed once.
                    unsafe { node_free(node) };
                } else {
                    // SAFETY: a live node.
                    unsafe { unregister_node(node, req) };
                }
            }
        }
    }

    /// `php_libxml_attr_value`: an attribute's value, its one text child's
    /// content read straight or the list's content joined.
    ///
    /// # Safety
    /// `attr` is live.
    #[must_use]
    pub(crate) unsafe fn attr_value(attr: *mut sys::xmlAttr) -> Vec<u8> {
        // SAFETY: the caller's contract.
        let a = unsafe { &*attr };
        if a.children.is_null() {
            return Vec::new();
        }
        // SAFETY: a live child list.
        let first = unsafe { &*a.children };
        if first.type_ == sys::XML_TEXT_NODE && first.next.is_null() {
            // SAFETY: the text's content, or null.
            return unsafe { Alloc::bytes(first.content) }.to_vec();
        }
        // SAFETY: a live attribute read as a node.
        let joined = unsafe { sys::xmlNodeGetContent(attr.cast()) };
        // SAFETY: a string of libxml's allocator nothing else frees.
        unsafe { Alloc::take(joined) }.unwrap_or_default()
    }

    /// `php_libxml_default_dump_doc_to_str`: `xmlSaveToIO` into a buffer,
    /// `xmlSaveDoc`, `xmlSaveClose`; `None` where either failed.
    ///
    /// # Safety
    /// `doc` is live.
    #[must_use]
    pub(crate) unsafe fn dump_doc(
        doc: *mut sys::xmlDoc,
        options: i32,
        encoding: Option<&[u8]>,
    ) -> Option<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        let encoding = encoding.map(c_string);
        let enc = encoding
            .as_ref()
            .map_or(ptr::null(), |e| e.as_ptr().cast::<c_char>());
        // SAFETY: `out` outlives the context, which is closed below.
        let ctxt = unsafe {
            sys::xmlSaveToIO(
                write_into,
                None,
                ptr::from_mut(&mut out).cast(),
                enc,
                options,
            )
        };
        if ctxt.is_null() {
            return None;
        }
        // SAFETY: a live context and document.
        let status = unsafe { sys::xmlSaveDoc(ctxt, doc) };
        // SAFETY: the context, closed once.
        let closed = unsafe { sys::xmlSaveClose(ctxt) };
        (status >= 0 && closed >= 0).then_some(out)
    }

    /// `php_libxml_default_dump_node_to_str`: `xmlNodeDumpOutput` into a
    /// buffer; `None` where the flush failed.
    ///
    /// # Safety
    /// `doc` is null or live and `node` live.
    #[must_use]
    pub(crate) unsafe fn dump_node(
        doc: *mut sys::xmlDoc,
        node: *mut sys::xmlNode,
        format: bool,
        encoding: Option<&[u8]>,
    ) -> Option<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        // SAFETY: `out` outlives the buffer, which is closed below.
        let buf = unsafe {
            sys::xmlOutputBufferCreateIO(
                write_into,
                None,
                ptr::from_mut(&mut out).cast(),
                ptr::null_mut(),
            )
        };
        if buf.is_null() {
            return None;
        }
        let encoding = encoding.map(c_string);
        let enc = encoding
            .as_ref()
            .map_or(ptr::null(), |e| e.as_ptr().cast::<c_char>());
        // SAFETY: a live buffer, document and node.
        unsafe { sys::xmlNodeDumpOutput(buf, doc, node, 0, c_int::from(format), enc) };
        // SAFETY: the live buffer.
        let flushed = unsafe { sys::xmlOutputBufferFlush(buf) };
        // SAFETY: the buffer, closed once.
        unsafe { sys::xmlOutputBufferClose(buf) };
        (flushed >= 0).then_some(out)
    }
}

/// `php_libxml_decrement_node_ptr_ref`.
///
/// # Safety
/// `proxy` is a live proxy.
unsafe fn decrement_node_ptr_ref(proxy: *mut NodePtr, req: Option<&mut Request>) -> u32 {
    // SAFETY: the caller's contract.
    let p = unsafe { &mut *proxy };
    p.refcount -= 1;
    let left = p.refcount;
    if left == 0 {
        if !p.node.is_null() {
            // SAFETY: a node a live proxy names is live.
            unsafe { (*p.node).private = ptr::null_mut() };
        }
        if !p.wrapper.is_null() {
            // SAFETY: a wrapper a proxy names is owned by a live object.
            unsafe { (*p.wrapper).node = ptr::null_mut() };
        }
        give_back(req, p.charge.take());
        // SAFETY: the allocation `increment_node_ptr` leaked; its last
        // count, and nothing names it any more.
        drop(unsafe { Box::from_raw(proxy) });
    }
    left
}

/// `php_libxml_decrement_doc_ref_directly`.
///
/// # Safety
/// `d` is a live document reference.
unsafe fn decrement_doc_ref_directly(d: *mut DocRef, req: Option<&mut Request>) -> u32 {
    // SAFETY: the caller's contract.
    let r = unsafe { &mut *d };
    r.refcount -= 1;
    let left = r.refcount;
    if left == 0 {
        let mut req = req;
        if let Some(mut private) = r.private.take() {
            if let Some(req) = req.as_deref_mut() {
                private.destroy(req);
            }
            drop(private);
        }
        if !r.doc.is_null() {
            // SAFETY: the document this reference owned, freed once.
            unsafe { sys::xmlFreeDoc(r.doc) };
        }
        if let (Some(props), Some(req)) = (r.props.take(), req.as_deref_mut()) {
            props.free(req);
        }
        give_back(req, r.charge.take());
        // SAFETY: the allocation `increment_doc_ref` leaked, its last count.
        drop(unsafe { Box::from_raw(d) });
    }
    left
}

/// The document object's switches reached from any node of its tree,
/// `node->doc->_private`'s wrapper: what php reads as
/// `object_private->document` for a node no object stands for.
///
/// # Safety
/// `node` is null or live.
unsafe fn doc_wrapper(node: *mut sys::xmlNode) -> *mut Wrapper {
    if node.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: the caller's contract.
    let proxy = unsafe { (*node).private }.cast::<NodePtr>();
    if proxy.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: a proxy a node names is live.
    unsafe { (*proxy).wrapper }
}

/// `php_libxml_unregister_node`.
///
/// # Safety
/// `node` is live.
unsafe fn unregister_node(node: *mut sys::xmlNode, mut req: Option<&mut Request>) {
    // SAFETY: the caller's contract.
    let proxy = unsafe { (*node).private }.cast::<NodePtr>();
    if proxy.is_null() {
        return;
    }
    // SAFETY: a proxy a node names is live.
    let wrapper = unsafe { (*proxy).wrapper };
    if wrapper.is_null() {
        // SAFETY: a live node.
        if unsafe { (*node).type_ } != sys::XML_DOCUMENT_NODE {
            // SAFETY: as above.
            unsafe { (*node).private = ptr::null_mut() };
        }
        // SAFETY: a live proxy.
        unsafe { (*proxy).node = ptr::null_mut() };
        return;
    }
    // SAFETY: a wrapper a proxy names is owned by a live object; a
    // `NodeObject` over the same allocation for the two decrements, which
    // is forgotten after so the owner's is the one that frees it.
    let mut borrowed =
        std::mem::ManuallyDrop::new(NodeObject(unsafe { NonNull::new_unchecked(wrapper) }));
    borrowed.decrement_node_ptr_in(req.as_deref_mut());
    borrowed.decrement_doc_ref_in(req);
}

/// `LIBXML_NS_TAG_HOOK`: the low bit of a namespace's `_private` that
/// says the rest points at a document whose private data is told of the
/// free.
const NS_TAG_HOOK: usize = 1;

/// `php_libxml_node_free`'s call of the private data's `ns_hook` for an
/// element whose namespace carries the tag.
///
/// # Safety
/// `node` is a live element.
unsafe fn call_ns_hook(node: *mut sys::xmlNode) {
    // SAFETY: the caller's contract.
    let ns = unsafe { (*node).ns };
    if ns.is_null() {
        return;
    }
    // SAFETY: a live element's namespace.
    let private = unsafe { (*ns).private };
    if private.addr() & NS_TAG_HOOK == 0 {
        return;
    }
    let d = private.wrapping_byte_sub(NS_TAG_HOOK).cast::<DocRef>();
    // SAFETY: the tag names the reference of the element's document, which
    // lives while any object of its tree does, and one does while this
    // frees a node an object let go of.
    let r = unsafe { &mut *d };
    if let (Some(data), Some(element)) = (r.private.as_deref_mut(), Node::from_raw(node)) {
        data.element_freed(element);
    }
}

/// `php_libxml_node_free`.
///
/// # Safety
/// `node` is live and unlinked, and nothing frees it after this.
unsafe fn node_free(node: *mut sys::xmlNode) {
    // SAFETY: the caller's contract.
    let n = unsafe { &mut *node };
    if n.type_ == sys::XML_ELEMENT_NODE {
        // SAFETY: a live element.
        unsafe { call_ns_hook(node) };
    }
    if !n.private.is_null() {
        // SAFETY: a proxy a node names is live.
        unsafe { (*n.private.cast::<NodePtr>()).node = ptr::null_mut() };
    }
    match n.type_ {
        // SAFETY: an attribute node is an `xmlAttr`, freed once.
        sys::XML_ATTRIBUTE_NODE => unsafe { sys::xmlFreeProp(node.cast()) },
        sys::XML_ENTITY_DECL => {
            let entity = node.cast::<sys::xmlEntity>();
            // SAFETY: an entity declaration node is an `xmlEntity`.
            if unsafe { (*entity).etype } != sys::XML_INTERNAL_PREDEFINED_ENTITY {
                // SAFETY: as above.
                unsafe { unlink_entity_decl(entity) };
                // SAFETY: as above; php's field-by-field release for a
                // library before 2.12, which has no `xmlFreeEntity`.
                unsafe { free_entity(entity) };
            }
        }
        sys::XML_NOTATION_NODE => {
            let entity = node.cast::<sys::xmlEntity>();
            // SAFETY: `create_notation`'s entity in disguise, whose
            // strings are its own.
            unsafe {
                Alloc::free(n.name.cast_mut().cast());
            }
            // SAFETY: a notation node is an `xmlEntity`.
            let e = unsafe { &*entity };
            let (external, system) = (e.ExternalID, e.SystemID);
            // SAFETY: as above.
            unsafe { Alloc::free(external.cast_mut().cast()) };
            // SAFETY: as above.
            unsafe { Alloc::free(system.cast_mut().cast()) };
            // SAFETY: as above, the struct itself last.
            unsafe { Alloc::free(node.cast()) };
        }
        sys::XML_ELEMENT_DECL | sys::XML_ATTRIBUTE_DECL => {}
        sys::XML_NAMESPACE_DECL => {
            if !n.ns.is_null() {
                // SAFETY: the fake declaration node owns its namespace.
                unsafe { sys::xmlFreeNs(n.ns) };
                n.ns = ptr::null_mut();
            }
            n.type_ = sys::XML_ELEMENT_NODE;
            // SAFETY: the fake node is an element node in all but type.
            unsafe { sys::xmlFreeNode(node) };
        }
        sys::XML_DTD_NODE => {
            let dtd = node.cast::<sys::xmlDtd>();
            // SAFETY: a DTD node is an `xmlDtd`.
            let d = unsafe { &*dtd };
            if d.private.is_null() {
                for table in [d.entities, d.pentities] {
                    // SAFETY: a hash table the DTD owns, scanned with
                    // itself as the scanner's data.
                    unsafe { sys::xmlHashScan(table, unlink_entity, table) };
                }
            }
            // SAFETY: as above, freed once.
            unsafe { sys::xmlFreeDtd(dtd) };
        }
        sys::XML_ELEMENT_NODE => {
            if !n.nsDef.is_null() && !n.doc.is_null() {
                let first = n.nsDef;
                let mut last = first;
                // SAFETY: the declarations are a live list.
                while !unsafe { (*last).next }.is_null() {
                    // SAFETY: as above.
                    last = unsafe { (*last).next };
                }
                // SAFETY: the element's document is live.
                unsafe { set_old_ns_list(n.doc, first, last) };
                n.nsDef = ptr::null_mut();
            }
            // SAFETY: a live, unlinked element, freed once.
            unsafe { sys::xmlFreeNode(node) };
        }
        // SAFETY: any other node, freed once.
        _ => unsafe { sys::xmlFreeNode(node) },
    }
}

/// php's release of an entity declaration for a library before 2.12.
///
/// # Safety
/// `entity` is live, unlinked, and freed nowhere else.
unsafe fn free_entity(entity: *mut sys::xmlEntity) {
    // SAFETY: the caller's contract.
    let e = unsafe { &mut *entity };
    if !e.children.is_null() && e.owner != 0 {
        // SAFETY: a live child list.
        let parent = unsafe { (*e.children).parent };
        if parent == entity.cast() {
            // SAFETY: the list the entity owns.
            unsafe { sys::xmlFreeNodeList(e.children) };
        }
    }
    let dict = if e.doc.is_null() {
        ptr::null_mut()
    } else {
        // SAFETY: a live document.
        unsafe { (*e.doc).dict }
    };
    let owned = |s: *const u8| -> bool {
        // SAFETY: a live dictionary or none.
        dict.is_null() || unsafe { sys::xmlDictOwns(dict, s) } == 0
    };
    for s in [
        e.name,
        e.ExternalID,
        e.SystemID,
        e.URI,
        e.content.cast_const(),
        e.orig.cast_const(),
    ] {
        if owned(s) {
            // SAFETY: a string the entity owns and no dictionary does.
            unsafe { Alloc::free(s.cast_mut().cast()) };
        }
    }
    // SAFETY: the struct last.
    unsafe { Alloc::free(entity.cast()) };
}

/// `php_libxml_unlink_entity_decl`.
///
/// # Safety
/// `entity` is live.
unsafe fn unlink_entity_decl(entity: *mut sys::xmlEntity) {
    // SAFETY: the caller's contract.
    let e = unsafe { &*entity };
    if e.parent.is_null() {
        return;
    }
    // SAFETY: an entity's parent is its live DTD.
    let dtd = unsafe { &*e.parent };
    for table in [dtd.entities, dtd.pentities] {
        // SAFETY: a table the DTD owns, and the entity's own name.
        if unsafe { sys::xmlHashLookup(table, e.name) } == entity.cast() {
            // SAFETY: as above; the entry is removed, not freed.
            unsafe { sys::xmlHashRemoveEntry(table, e.name, ptr::null_mut()) };
        }
    }
}

/// `php_libxml_unlink_entity`, a hash scanner.
unsafe extern "C" fn unlink_entity(data: *mut c_void, table: *mut c_void, name: *const u8) {
    let entity = data.cast::<sys::xmlEntity>();
    // SAFETY: the scanner hands each live entry of `table`.
    if !unsafe { (*entity).private }.is_null() {
        // SAFETY: as above; the entry is removed, not freed.
        unsafe { sys::xmlHashRemoveEntry(table, name, ptr::null_mut()) };
    }
}

/// `php_libxml_set_old_ns_list`.
///
/// # Safety
/// `doc` is null or live; `first` to `last` is a live list whose `last`
/// ends it.
unsafe fn set_old_ns_list(doc: *mut sys::xmlDoc, first: *mut sys::xmlNs, last: *mut sys::xmlNs) {
    if doc.is_null() {
        return;
    }
    // SAFETY: the caller's contract.
    let d = unsafe { &mut *doc };
    if d.oldNs.is_null() {
        // SAFETY: a variable libxml2 sets at load and nothing here writes.
        let alloc = unsafe { sys::xmlMalloc };
        // SAFETY: an allocation of libxml's own allocator, the size of the
        // struct, which `xmlFreeDoc` frees with the rest of the list.
        let ns = unsafe { alloc(std::mem::size_of::<sys::xmlNs>()) }.cast::<sys::xmlNs>();
        if ns.is_null() {
            return;
        }
        // SAFETY: a NUL-terminated constant.
        let href = unsafe { sys::xmlStrdup(sys::XML_XML_NAMESPACE.as_ptr()) };
        // SAFETY: as above.
        let prefix = unsafe { sys::xmlStrdup(c"xml".as_ptr().cast()) };
        // SAFETY: `ns` is the fresh allocation, written whole; `memset`
        // then the fields php sets.
        unsafe {
            ns.write(sys::xmlNs {
                next: ptr::null_mut(),
                type_: sys::XML_LOCAL_NAMESPACE,
                href,
                prefix,
                private: ptr::null_mut(),
                context: ptr::null_mut(),
            });
        }
        d.oldNs = ns;
    } else {
        // SAFETY: the document's live list head.
        let after = unsafe { (*d.oldNs).next };
        // SAFETY: the caller's live list.
        unsafe { (*last).next = after };
    }
    // SAFETY: a live head.
    unsafe { (*d.oldNs).next = first };
}

/// `xmlReconciliateNs` for a node about to outlive its tree; a detached
/// node reconciles against a stand-in document, as php does for a
/// library before 2.13, and keeps what it declared.
///
/// # Safety
/// `node` is live and unlinked.
unsafe fn reconcile_detached(node: *mut sys::xmlNode) {
    // SAFETY: the caller's contract.
    let n = unsafe { &mut *node };
    if !n.doc.is_null() {
        // SAFETY: a live element and document.
        unsafe { sys::xmlReconciliateNs(n.doc, node) };
        return;
    }
    // SAFETY: a zeroed document is what php's `memset` lays down; every
    // field is a pointer, an integer or null.
    let mut dummy: sys::xmlDoc = unsafe { std::mem::zeroed() };
    dummy.type_ = sys::XML_DOCUMENT_NODE;
    n.doc = &raw mut dummy;
    // SAFETY: a live element and a document that outlives the call.
    unsafe { sys::xmlReconciliateNs(n.doc, node) };
    n.doc = ptr::null_mut();
    if !dummy.oldNs.is_null() {
        let old = n.nsDef;
        n.nsDef = dummy.oldNs;
        // SAFETY: the one declaration the stand-in grew.
        unsafe { (*dummy.oldNs).next = old };
    }
}

// ---- php_libxml_document_handlers' defaults

unsafe extern "C" fn write_into(context: *mut c_void, buffer: *const c_char, len: c_int) -> c_int {
    let Ok(n) = usize::try_from(len) else {
        return -1;
    };
    // SAFETY: the context is the `Vec` the dump below handed libxml.
    let into = unsafe { &mut *context.cast::<Vec<u8>>() };
    // SAFETY: libxml hands `len` bytes at `buffer`.
    into.extend_from_slice(unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n) });
    len
}

fn c_string(bytes: &[u8]) -> Vec<u8> {
    let mut out = php_ext::coerce::c_str(bytes).to_vec();
    out.push(0);
    out
}

/// A buffer libxml writes into through [`write_into`]: the buffer
/// dom's HTML and C14N serialisers hand libxml, closed with its bytes
/// taken.
pub(crate) struct Sink {
    buf: *mut sys::xmlOutputBuffer,
    /// The bytes, at an address that stays put while the sink moves.
    out: NonNull<Vec<u8>>,
}

impl Sink {
    /// `xmlOutputBufferCreateIO` into memory, with no encoder.
    #[must_use]
    pub(crate) fn new() -> Option<Sink> {
        let out = NonNull::from(Box::leak(Box::new(Vec::<u8>::new())));
        // SAFETY: the leaked `Vec` outlives the buffer, which `finish` or
        // `drop` closes before the `Vec` is freed.
        let buf = unsafe {
            sys::xmlOutputBufferCreateIO(write_into, None, out.as_ptr().cast(), ptr::null_mut())
        };
        let sink = Sink { buf, out };
        (!buf.is_null()).then_some(sink)
    }

    /// The buffer, for the one libxml call that fills it.
    #[must_use]
    pub(crate) fn raw(&self) -> *mut sys::xmlOutputBuffer {
        self.buf
    }

    /// Flushed and closed, and what was written; `None` where the flush
    /// failed.
    #[must_use]
    pub(crate) fn finish(mut self) -> Option<Vec<u8>> {
        // SAFETY: the live buffer.
        let flushed = unsafe { sys::xmlOutputBufferFlush(self.buf) };
        // SAFETY: the buffer, closed once; `drop` then sees null.
        unsafe { sys::xmlOutputBufferClose(self.buf) };
        self.buf = ptr::null_mut();
        // SAFETY: the `Vec` this owns, no longer written by libxml.
        let taken = std::mem::take(unsafe { &mut *self.out.as_ptr() });
        (flushed >= 0).then_some(taken)
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if !self.buf.is_null() {
            // SAFETY: a buffer `finish` did not close.
            unsafe { sys::xmlOutputBufferClose(self.buf) };
        }
        // SAFETY: the allocation `new` leaked, freed once, after the
        // buffer that wrote into it is closed.
        drop(unsafe { Box::from_raw(self.out.as_ptr()) });
    }
}
