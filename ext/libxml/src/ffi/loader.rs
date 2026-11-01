//! `php_libxml.c`'s loader half: libxml2's file I/O channelled through php's
//! streams (`php_libxml_streams_IO_*`, the reading and writing buffer
//! creators php installs), and the external entity loader that calls a
//! program's resolver (`php_libxml_external_entity_loader`). What the
//! callbacks decide is [`crate::load`]'s, over the request the running
//! [`Call`] lends; what is here is libxml's side of each callback.
//!
//! libxml2 2.9 keeps the two buffer creators per thread and the entity
//! loader per process. The creators are set around each call that lends a
//! request ([`Creators`]); the loader is set once, to [`entity_loader`],
//! which finds its request in the thread's [`Call`] and serves libxml's
//! own default where there is none, as php's `php_libxml_pre_ext_ent_loader`
//! does outside a request.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr;

use php_ext::raising::Thrown;
use php_ext::request::{OwnedStream as _, Request, Stream};
use php_ext::values::Owner;

use super::{Alloc, Call, Io, cstr, sys};
use crate::load::{Loader, Resolved};

/// The creators php installs, set on this thread for one call and put
/// back when dropped: `php_libxml_input_buffer_create_filename` and
/// `php_libxml_output_buffer_create_filename`.
pub(crate) struct Creators {
    input: Option<sys::xmlParserInputBufferCreateFilenameFunc>,
    output: Option<sys::xmlOutputBufferCreateFilenameFunc>,
}

impl Creators {
    pub(crate) fn set() -> Creators {
        Creators::replace(Some(input_buffer), Some(output_buffer))
    }

    /// Neither creator: libxml's own, as outside a request.
    fn cleared() -> Creators {
        Creators::replace(None, None)
    }

    fn replace(
        input: Option<sys::xmlParserInputBufferCreateFilenameFunc>,
        output: Option<sys::xmlOutputBufferCreateFilenameFunc>,
    ) -> Creators {
        // SAFETY: this thread's creator, replaced; the old one is kept.
        let input = unsafe { sys::xmlParserInputBufferCreateFilenameDefault(input) };
        // SAFETY: as above.
        let output = unsafe { sys::xmlOutputBufferCreateFilenameDefault(output) };
        Creators { input, output }
    }
}

impl Drop for Creators {
    fn drop(&mut self) {
        // SAFETY: the creators that were set before, put back.
        unsafe { sys::xmlParserInputBufferCreateFilenameDefault(self.input) };
        // SAFETY: as above.
        unsafe { sys::xmlOutputBufferCreateFilenameDefault(self.output) };
    }
}

/// The process's external entity loader, [`entity_loader`].
pub(crate) struct EntityLoader;

impl EntityLoader {
    /// `php_libxml_initialize`'s `xmlSetExternalEntityLoader`: the loader
    /// every parse of the process reaches, set once, as php sets it at
    /// `MINIT`. Setting it again writes the same function.
    pub(crate) fn install() {
        // SAFETY: no argument; the process's loader, read.
        let current = unsafe { sys::xmlGetExternalEntityLoader() };
        let ours: sys::xmlExternalEntityLoader = entity_loader;
        if current.map(|f| f as usize) != Some(ours as usize) {
            // SAFETY: a function that stays valid for the process's life.
            unsafe { sys::xmlSetExternalEntityLoader(Some(ours)) };
        }
    }
}

/// The request the running call lends, and the call's own `Io`.
pub(super) fn lent() -> Option<(*mut Call, *mut Io)> {
    let call = Call::current()?;
    // SAFETY: the running record, live while it runs.
    let io = unsafe { (*call).io };
    (!io.is_null()).then_some((call, io))
}

/// # Safety
/// `io` is the running call's `Io`.
pub(super) unsafe fn request<'a>(io: *mut Io) -> &'a mut Request {
    // SAFETY: the caller's contract.
    let req = unsafe { (*io).req };
    // SAFETY: the request is lent for the call and nothing else borrows
    // it while libxml runs.
    unsafe { &mut *req }
}

/// The first throwable of the call kept, as `EG(exception)` keeps the
/// first; a later one is dropped as php's `zend_throw_exception_internal`
/// chains it where it cannot be seen from here.
///
/// # Safety
/// `io` is the running call's `Io`.
pub(super) unsafe fn keep(io: *mut Io, thrown: Thrown) {
    // SAFETY: the caller's contract.
    let slot = unsafe { &mut (*io).thrown };
    if slot.is_none() {
        *slot = Some(thrown);
    }
}

/// Every error the running call and the calls it nests in have collected,
/// routed now, outermost first, so that what php code prints from a
/// callback comes after them, as php's handlers had printed them already.
///
/// # Safety
/// `call` is the running record and `io` its `Io`.
pub(super) unsafe fn flush(call: *mut Call, io: *mut Io) {
    let mut chain = Vec::new();
    let mut at = call;
    while !at.is_null() {
        chain.push(at);
        // SAFETY: a record of the chain, live while the innermost runs.
        at = unsafe { (*at).parent };
    }
    // SAFETY: the caller's contract.
    let req = unsafe { request(io) };
    for record in chain.into_iter().rev() {
        // SAFETY: as above; the errors are taken out, so none is routed
        // twice.
        let r = unsafe { &mut *record };
        let mut taken = std::mem::take(&mut r.raised);
        if let Some(routing) = r.routing {
            routing.apply(&mut taken);
        }
        crate::Errors::route(req, taken);
    }
}

/// `f` served as php code is served from inside a libxml call: libxml's
/// per-thread handler, its data and the creators cleared, so what the code
/// parses is a call of its own, and put back after.
pub(super) fn suspended<R>(f: impl FnOnce() -> R) -> R {
    // SAFETY: the address of this thread's handler.
    let handler_at = unsafe { sys::__xmlStructuredError() };
    // SAFETY: the thread's own global, read by copy.
    let handler = unsafe { *handler_at };
    // SAFETY: the address of this thread's handler context.
    let context_at = unsafe { sys::__xmlStructuredErrorContext() };
    // SAFETY: as above.
    let context = unsafe { *context_at };
    // SAFETY: no handler, no context.
    unsafe { sys::xmlSetStructuredErrorFunc(ptr::null_mut(), None) };
    let answer = {
        let _cleared = Creators::cleared();
        f()
    };
    // SAFETY: the handler and context that were set before.
    unsafe { sys::xmlSetStructuredErrorFunc(context, handler) };
    answer
}

/// A C string libxml handed a callback, `None` for its null.
///
/// # Safety
/// `p` is null or a NUL-terminated string live for the callback.
unsafe fn bytes<'a>(p: *const c_char) -> Option<&'a [u8]> {
    // SAFETY: the caller's contract.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_bytes())
}

/// Where a parser stands, `parser->input`, which `php_libxml_ctx_error`
/// prints and records: the input's file, line and column.
#[derive(Debug, Clone)]
pub(crate) struct Site {
    pub(crate) file: Option<Vec<u8>>,
    pub(crate) line: i32,
    pub(crate) column: i32,
}

/// The parser's input, where it has one.
///
/// # Safety
/// `ctxt` is null or a live parser.
unsafe fn site(ctxt: *mut sys::xmlParserCtxt) -> Option<Site> {
    if ctxt.is_null() {
        return None;
    }
    // SAFETY: the caller's contract.
    let input = unsafe { (*ctxt).input };
    if input.is_null() {
        return None;
    }
    // SAFETY: a live parser's input.
    let i = unsafe { &*input };
    Some(Site {
        // SAFETY: the input's name, or null.
        file: unsafe { bytes(i.filename) }.map(<[u8]>::to_vec),
        line: i.line,
        column: i.col,
    })
}

/// The four members of the parser a resolver is handed as `$context`,
/// `directory`, `intSubName`, `extSubURI` and `extSubSystem`, each `None`
/// for its null.
#[derive(Debug, Clone, Default)]
pub(crate) struct Context {
    pub(crate) directory: Option<Vec<u8>>,
    pub(crate) int_sub_name: Option<Vec<u8>>,
    pub(crate) ext_sub_uri: Option<Vec<u8>>,
    pub(crate) ext_sub_system: Option<Vec<u8>>,
}

/// The parser's four members.
///
/// # Safety
/// `ctxt` is null or a live parser.
unsafe fn context(ctxt: *mut sys::xmlParserCtxt) -> Context {
    if ctxt.is_null() {
        return Context::default();
    }
    // SAFETY: the caller's contract.
    let c = unsafe { &*ctxt };
    let own = |p: *const u8| {
        // SAFETY: a NUL-terminated string the parser owns, or null.
        unsafe { bytes(p.cast()) }.map(<[u8]>::to_vec)
    };
    Context {
        directory: own(c.directory.cast()),
        int_sub_name: own(c.intSubName),
        ext_sub_uri: own(c.extSubURI),
        ext_sub_system: own(c.extSubSystem),
    }
}

/// libxml2's own loader, `xmlDefaultExternalEntityLoader`, which the
/// library does not export: a network URL opened as a file where the
/// parser allows the network, anything else through
/// `xmlNoNetExternalEntityLoader`, which serves the default loader for
/// every URL it lets through and refuses the network under
/// `XML_PARSE_NONET`, as the default does.
///
/// # Safety
/// The three are what libxml handed the entity loader.
unsafe fn default_loader(
    url: *const c_char,
    id: *const c_char,
    ctxt: *mut sys::xmlParserCtxt,
) -> *mut sys::xmlParserInput {
    // SAFETY: the caller's contract.
    let nonet = !ctxt.is_null() && unsafe { (*ctxt).options } & sys::XML_PARSE_NONET != 0;
    // SAFETY: as above.
    let network = unsafe { bytes(url) }.is_some_and(|u| {
        let lower = u.get(..7).map(<[u8]>::to_ascii_lowercase);
        lower.as_deref() == Some(b"http://")
            || lower.as_deref().is_some_and(|l| l.starts_with(b"ftp://"))
    });
    if network && !nonet {
        // SAFETY: as above.
        return unsafe { sys::xmlNewInputFromFile(ctxt, url) };
    }
    // SAFETY: as above.
    unsafe { sys::xmlNoNetExternalEntityLoader(url, id, ctxt) }
}

/// `php_libxml_pre_ext_ent_loader` and `php_libxml_external_entity_loader`:
/// outside a call that lends a request, libxml's default; with no
/// resolver set, the default; otherwise the resolver called with the
/// public and system identifiers and the parser's four members, and what
/// it answered made the entity: a stream read through as it stands, a
/// path opened through the default's creator, and nothing, which is
/// php's error.
unsafe extern "C" fn entity_loader(
    url: *const c_char,
    id: *const c_char,
    ctxt: *mut sys::xmlParserCtxt,
) -> *mut sys::xmlParserInput {
    let Some((call, io)) = lent() else {
        // SAFETY: what libxml handed this loader.
        return unsafe { default_loader(url, id, ctxt) };
    };
    // SAFETY: the running call and its `Io`.
    let req = unsafe { request(io) };
    if !Loader::has_resolver(req) {
        // SAFETY: what libxml handed this loader.
        return unsafe { default_loader(url, id, ctxt) };
    }
    // SAFETY: as above.
    unsafe { flush(call, io) };
    // SAFETY: a string libxml handed this loader, or null.
    let id_bytes = unsafe { bytes(id) };
    // SAFETY: as above.
    let url_bytes = unsafe { bytes(url) };
    // SAFETY: a live parser or null.
    let members = unsafe { context(ctxt) };
    let resolved = suspended(|| Loader::resolve(req, id_bytes, url_bytes, &members));
    // SAFETY: a live parser or null.
    let at = unsafe { site(ctxt) };
    let mut input = ptr::null_mut();
    let mut resource: Option<Vec<u8>> = None;
    match resolved {
        Resolved::Failed { name, thrown } => {
            // SAFETY: the running call's `Io`.
            unsafe { keep(io, thrown) };
            let text = format!("Call to user entity loader callback '{name}' has failed");
            crate::Errors::ctx(req, at.as_ref(), text.as_bytes(), true);
        }
        Resolved::Thrown(thrown) => {
            // SAFETY: as above.
            unsafe { keep(io, thrown) };
        }
        Resolved::Stream(stream) => {
            // SAFETY: a live parser or null, and a stream this input owns.
            input = unsafe { stream_input(ctxt, stream) };
            if input.is_null() {
                crate::Errors::ctx(
                    req,
                    at.as_ref(),
                    b"Could not allocate parser input buffer",
                    false,
                );
            }
        }
        Resolved::Path(path) => resource = Some(path),
        Resolved::Nothing => {}
    }
    if input.is_null() {
        if let Some(path) = resource {
            let p = cstr(&path);
            // SAFETY: a live parser or null and a NUL-terminated path.
            input = unsafe { sys::xmlNewInputFromFile(ctxt, p.as_ptr().cast()) };
        } else {
            // SAFETY: the running call's `Io`.
            let suppressed = unsafe { (*io).thrown.is_some() };
            let text = match id_bytes {
                None => {
                    b"Failed to load external entity because the resolver function returned null\n"
                        .to_vec()
                }
                Some(id) => {
                    let mut t = b"Failed to load external entity \"".to_vec();
                    t.extend_from_slice(id);
                    t.extend_from_slice(b"\"\n");
                    t
                }
            };
            crate::Errors::ctx(req, at.as_ref(), &text, suppressed);
        }
    }
    input
}

/// The stream a resolver answered, as the parser's input:
/// `xmlParserInputBufferCreateIO` over it with no encoding and
/// `xmlNewIOInputStream`, the stream closed with the buffer.
///
/// # Safety
/// `ctxt` is null or a live parser.
unsafe fn stream_input(
    ctxt: *mut sys::xmlParserCtxt,
    stream: Owner<Stream>,
) -> *mut sys::xmlParserInput {
    let context = Box::into_raw(Box::new(stream)).cast::<c_void>();
    // SAFETY: the callbacks read the box this hands over; the buffer owns
    // it from here and closes it once.
    let buffer = unsafe {
        sys::xmlParserInputBufferCreateIO(
            stream_read,
            Some(stream_close),
            context,
            sys::XML_CHAR_ENCODING_NONE,
        )
    };
    if buffer.is_null() {
        // SAFETY: the box no buffer took, closed here.
        unsafe { stream_close(context) };
        return ptr::null_mut();
    }
    // SAFETY: a live parser or null and a buffer the input takes over.
    let input = unsafe { sys::xmlNewIOInputStream(ctxt, buffer, sys::XML_CHAR_ENCODING_NONE) };
    if input.is_null() {
        // SAFETY: the buffer no input took, freed once; it closes the stream.
        unsafe { sys::xmlFreeParserInputBuffer(buffer) };
    }
    input
}

/// `php_libxml_input_buffer_create_filename`: a path libxml would open,
/// opened through php's wrappers where the loader is enabled, as a buffer
/// that reads through the stream. Outside a call that lends a request,
/// libxml's own creator.
unsafe extern "C" fn input_buffer(
    uri: *const c_char,
    enc: c_int,
) -> *mut sys::xmlParserInputBuffer {
    let Some((call, io)) = lent() else {
        // SAFETY: what libxml handed this creator.
        return unsafe { sys::__xmlParserInputBufferCreateFilename(uri, enc) };
    };
    // SAFETY: a string libxml handed this creator, or null.
    let Some(path) = (unsafe { bytes(uri) }) else {
        return ptr::null_mut();
    };
    // SAFETY: the running call and its `Io`.
    unsafe { flush(call, io) };
    // SAFETY: as above.
    let req = unsafe { request(io) };
    let stream = match Loader::open_input(req, path) {
        Ok(Some(stream)) => stream,
        Ok(None) => return ptr::null_mut(),
        Err(thrown) => {
            // SAFETY: as above.
            unsafe { keep(io, thrown) };
            return ptr::null_mut();
        }
    };
    // An external transport's encoding: the charset of the http headers
    // the stream carries, where libxml was handed none.
    let mut enc = enc;
    if enc == sys::XML_CHAR_ENCODING_NONE
        && let Some(charset) = crate::mime_sniff::sniff_charset_from_stream(req, *stream)
    {
        let name = cstr(&charset);
        // SAFETY: a NUL-terminated name that outlives the call.
        let parsed = unsafe { sys::xmlParseCharEncoding(name.as_ptr().cast()) };
        enc = parsed.max(sys::XML_CHAR_ENCODING_NONE);
    }
    let context = Box::into_raw(Box::new(stream)).cast::<c_void>();
    // SAFETY: the callbacks read the box this hands over; the buffer owns
    // it from here.
    let buffer =
        unsafe { sys::xmlParserInputBufferCreateIO(stream_read, Some(stream_close), context, enc) };
    if buffer.is_null() {
        // SAFETY: the box no buffer took, closed here.
        unsafe { stream_close(context) };
    }
    buffer
}

/// `php_libxml_output_buffer_create_filename`: a path libxml would write,
/// opened for writing through php's wrappers, as a buffer that writes
/// through the stream; the encoder closed where none is made, as
/// `__xmlOutputBufferCreateFilename` closes it.
unsafe extern "C" fn output_buffer(
    uri: *const c_char,
    encoder: *mut sys::xmlCharEncodingHandler,
    _compression: c_int,
) -> *mut sys::xmlOutputBuffer {
    let failed = || {
        // SAFETY: the encoder libxml handed over, or null.
        unsafe { sys::xmlCharEncCloseFunc(encoder) };
        ptr::null_mut()
    };
    let Some((call, io)) = lent() else {
        return failed();
    };
    // SAFETY: a string libxml handed this creator, or null.
    let Some(path) = (unsafe { bytes(uri) }) else {
        return failed();
    };
    // SAFETY: the running call and its `Io`.
    unsafe { flush(call, io) };
    // SAFETY: as above.
    let req = unsafe { request(io) };
    let stream = match Loader::open_output(req, path) {
        Ok(Some(stream)) => stream,
        Ok(None) => return failed(),
        Err(thrown) => {
            // SAFETY: as above.
            unsafe { keep(io, thrown) };
            return failed();
        }
    };
    let context = Box::into_raw(Box::new(stream)).cast::<c_void>();
    // SAFETY: the callbacks read the box this hands over; the buffer owns
    // it and the encoder from here.
    let buffer =
        unsafe { sys::xmlOutputBufferCreateIO(stream_write, Some(stream_close), context, encoder) };
    if buffer.is_null() {
        // SAFETY: the box no buffer took, closed here.
        unsafe { stream_close(context) };
        return failed();
    }
    buffer
}

/// `php_libxml_streams_IO_read`: `php_stream_read` into libxml's buffer.
pub(super) unsafe extern "C" fn stream_read(
    context: *mut c_void,
    buffer: *mut c_char,
    len: c_int,
) -> c_int {
    let Some((_, io)) = lent() else {
        return -1;
    };
    let Ok(want) = usize::try_from(len) else {
        return -1;
    };
    // SAFETY: the box a creator handed libxml, live until closed.
    let stream: Stream = unsafe { **context.cast::<Owner<Stream>>() };
    // SAFETY: the running call's `Io`.
    let req = unsafe { request(io) };
    match Loader::read(req, stream, want) {
        Ok(Some(bytes)) => {
            let n = bytes.len().min(want);
            // SAFETY: libxml's buffer has room for `len` bytes.
            let into = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), want) };
            into[..n].copy_from_slice(&bytes[..n]);
            c_int::try_from(n).unwrap_or(-1)
        }
        Ok(None) => -1,
        Err(thrown) => {
            // SAFETY: as above.
            unsafe { keep(io, thrown) };
            -1
        }
    }
}

/// `php_libxml_streams_IO_write`: `php_stream_write` of libxml's bytes.
pub(super) unsafe extern "C" fn stream_write(
    context: *mut c_void,
    buffer: *const c_char,
    len: c_int,
) -> c_int {
    // `_php_stream_write` of nothing writes nothing and answers `0`,
    // lent a request or not.
    if len == 0 {
        return 0;
    }
    let Some((_, io)) = lent() else {
        return -1;
    };
    let Ok(n) = usize::try_from(len) else {
        return -1;
    };
    // SAFETY: the box a creator handed libxml, live until closed.
    let stream: Stream = unsafe { **context.cast::<Owner<Stream>>() };
    // SAFETY: libxml hands `len` bytes at `buffer`.
    let bytes = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n) };
    // SAFETY: the running call's `Io`.
    let req = unsafe { request(io) };
    stream
        .write(req, bytes)
        .and_then(|w| c_int::try_from(w).ok())
        .unwrap_or(-1)
}

/// A stream a reader or an emitter borrows (`XMLReader::fromStream`,
/// `XMLWriter::toStream`) boxed for libxml's callbacks, one more owner of
/// its resource taken, as `Z_ADDREF_P(stream_zv)` takes it, so the stream
/// outlives the program's own name for it.
pub(super) fn lend(req: &mut Request, stream: Stream) -> *mut c_void {
    Box::into_raw(Box::new(Owner::take(req, stream))).cast::<c_void>()
}

/// The close of a stream [`lend`] boxed: the owner it took given up,
/// `zend_list_delete`, and nothing closed. Outside a call that lends a
/// request the box is left, and its owner with it, for the request's end.
pub(super) unsafe extern "C" fn stream_release(context: *mut c_void) -> c_int {
    let Some((_, io)) = lent() else {
        return 0;
    };
    // SAFETY: the box `lend` handed libxml, released once, here.
    let stream = *unsafe { Box::from_raw(context.cast::<Owner<Stream>>()) };
    // SAFETY: the running call's `Io`.
    let req = unsafe { request(io) };
    stream.give_up(req);
    0
}

/// `php_libxml_streams_IO_close`: `php_stream_close`, and the box given
/// back. Outside a call that lends a request the box is left, and the
/// stream's owner with it, for the request's end to close.
unsafe extern "C" fn stream_close(context: *mut c_void) -> c_int {
    let Some((_, io)) = lent() else {
        return 0;
    };
    // SAFETY: the box a creator handed libxml, closed once, here.
    let stream = *unsafe { Box::from_raw(context.cast::<Owner<Stream>>()) };
    // SAFETY: the running call's `Io`.
    let req = unsafe { request(io) };
    stream.free(req);
    0
}

/// libxml2's URI functions, as php's loader reads a path with them.
pub(crate) struct Uri;

/// A URI `xmlParseURI` took: its scheme, `None` where it has none.
pub(crate) struct Parsed {
    pub(crate) scheme: Option<Vec<u8>>,
}

impl Uri {
    /// `xmlParseURI` as `php_libxml_streams_IO_open_wrapper` reads it:
    /// `None` where the text is no URI.
    pub(crate) fn parse(uri: &[u8]) -> Option<Parsed> {
        let text = cstr(uri);
        // SAFETY: a NUL-terminated string.
        let parsed = unsafe { sys::xmlParseURI(text.as_ptr().cast()) };
        if parsed.is_null() {
            return None;
        }
        // SAFETY: a live URI.
        let raw = unsafe { (*parsed).scheme };
        // SAFETY: the URI's scheme, a string it owns, or null.
        let scheme = unsafe { bytes(raw) }.map(<[u8]>::to_vec);
        // SAFETY: the URI, freed once.
        unsafe { sys::xmlFreeURI(parsed) };
        Some(Parsed { scheme })
    }

    /// `xmlURIEscapeStr(source, ":")` read by `xmlParseURIReference` into a
    /// fresh `xmlCreateURI`: whether it names a scheme.
    pub(crate) fn escaped_has_scheme(source: &[u8]) -> bool {
        let text = cstr(source);
        // SAFETY: no argument; a fresh URI, or null.
        let uri = unsafe { sys::xmlCreateURI() };
        if uri.is_null() {
            return false;
        }
        // SAFETY: a NUL-terminated string and list, escaped into a new one.
        let escaped = unsafe { sys::xmlURIEscapeStr(text.as_ptr(), c":".as_ptr().cast()) };
        if !escaped.is_null() {
            // SAFETY: a live URI and the NUL-terminated escaped string.
            unsafe { sys::xmlParseURIReference(uri, escaped.cast()) };
            // SAFETY: the escaped string, freed once.
            unsafe { Alloc::free(escaped.cast()) };
        }
        // SAFETY: a live URI.
        let scheme = !unsafe { (*uri).scheme }.is_null();
        // SAFETY: the URI, freed once.
        unsafe { sys::xmlFreeURI(uri) };
        scheme
    }

    /// `xmlURIUnescapeString(uri, 0, NULL)`: every `%XX` decoded.
    pub(crate) fn unescape(uri: &[u8]) -> Option<Vec<u8>> {
        let text = cstr(uri);
        // SAFETY: a NUL-terminated string, decoded into a new one.
        let out = unsafe { sys::xmlURIUnescapeString(text.as_ptr().cast(), 0, ptr::null_mut()) };
        // SAFETY: a string of libxml's allocator nothing else frees.
        unsafe { Alloc::take(out.cast()) }
    }
}
