//! The binding to libxslt and libexslt (design §7.2, revision 72,
//! `unsafe.homes`' `ext/*/src/ffi/`): the platform's libraries, found by
//! `build.rs` through `pkg-config`, the declarations written to 1.1.35 and
//! 0.8.20, and `shim.c` for the variadic error handler and the transformation's
//! fields. The documents libxslt reads and writes are libxml2's, reached
//! through ext/libxml's surface ([`Node::as_ptr`], [`XPath::lend`]), so the
//! errors libxml raises meanwhile are captured as every other call's are.
//!
//! libxslt keeps its process-wide functions, its generic error handler and its
//! limits process-wide; they are set once, when the extension is built,
//! and read by every thread after (ADR-0005).
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{c_char, c_int, c_void};
use std::marker::PhantomData;
use std::ptr::{self, NonNull};

use php_ext::raising::Thrown;
use php_ext::request::{Foreign, ForeignId, Held, Request};
use php_ext_libxml::ffi::{Function, Handler, NewDoc, Node, Raised, XPath};

type SecurityCheck = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char) -> c_int;

#[link(name = "xslt")]
unsafe extern "C" {
    static xsltMaxDepth: c_int;
    static xsltMaxVars: c_int;
    static xsltLibxsltVersion: c_int;
    static xsltLibxmlVersion: c_int;
    fn xsltParseStylesheetDoc(doc: *mut c_void) -> *mut c_void;
    fn xsltFreeStylesheet(sheet: *mut c_void);
    fn xsltNewTransformContext(sheet: *mut c_void, doc: *mut c_void) -> *mut c_void;
    fn xsltFreeTransformContext(ctxt: *mut c_void);
    fn xsltQuoteOneUserParam(ctxt: *mut c_void, name: *const u8, value: *const u8) -> c_int;
    fn xsltNewSecurityPrefs() -> *mut c_void;
    fn xsltFreeSecurityPrefs(sec: *mut c_void);
    fn xsltSetSecurityPrefs(sec: *mut c_void, option: c_int, func: SecurityCheck) -> c_int;
    fn xsltSetCtxtSecurityPrefs(sec: *mut c_void, ctxt: *mut c_void) -> c_int;
    fn xsltSecurityForbid(sec: *mut c_void, ctxt: *mut c_void, value: *const c_char) -> c_int;
    fn xsltRegisterExtFunction(
        ctxt: *mut c_void,
        name: *const u8,
        uri: *const u8,
        f: Function,
    ) -> c_int;
    fn xsltRegisterExtModuleFunction(name: *const u8, uri: *const u8, f: Function) -> c_int;
    fn xsltSaveResultToFilename(
        uri: *const c_char,
        result: *mut c_void,
        sheet: *mut c_void,
        compression: c_int,
    ) -> c_int;
    fn xsltSaveResultToString(
        out: *mut *mut u8,
        len: *mut c_int,
        result: *mut c_void,
        sheet: *mut c_void,
    ) -> c_int;
}

#[link(name = "exslt")]
unsafe extern "C" {
    fn exsltRegisterAll();
}

unsafe extern "C" {
    fn php_xsl_set_error_handler();
    fn php_xsl_set_limits(ctxt: *mut c_void, xinclude: c_int, depth: c_int, vars: c_int);
    fn php_xsl_xpath_context(ctxt: *mut c_void) -> *mut c_void;
    fn php_xsl_sheet_doc(sheet: *mut c_void) -> *mut c_void;
    fn php_xsl_apply(
        sheet: *mut c_void,
        doc: *mut c_void,
        ctxt: *mut c_void,
        profile: *const c_char,
    ) -> *mut c_void;
    fn php_xsl_free(p: *mut c_void);
}

/// `http://php.net/xsl`, NUL-terminated.
const PHP_NS: &[u8] = b"http://php.net/xsl\0";

/// A copy of `bytes` up to its first NUL with the NUL after.
fn cstr(bytes: &[u8]) -> Vec<u8> {
    let mut out = php_ext::coerce::c_str(bytes).to_vec();
    out.push(0);
    out
}

/// `PHP_MINIT_FUNCTION(xsl)`'s library half: `exsltRegisterAll`, php's two
/// process-wide functions under `http://php.net/xsl`, and php's error handler.
pub(crate) fn init() {
    // SAFETY: no argument; libexslt registers into libxslt's tables.
    unsafe { exsltRegisterAll() };
    // SAFETY: NUL-terminated names, copied into libxslt's table.
    unsafe {
        xsltRegisterExtModuleFunction(
            c"functionString".as_ptr().cast(),
            PHP_NS.as_ptr(),
            XPath::PHP_STRING,
        )
    };
    // SAFETY: as above.
    unsafe {
        xsltRegisterExtModuleFunction(
            c"function".as_ptr().cast(),
            PHP_NS.as_ptr(),
            XPath::PHP_OBJECT,
        )
    };
    // SAFETY: no argument; the handler is the shim's.
    unsafe { php_xsl_set_error_handler() };
}

/// `xsltMaxDepth` and `xsltMaxVars`, the limits a new processor starts at.
pub(crate) fn limits() -> (i64, i64) {
    // SAFETY: two globals libxslt sets at load and nothing here writes.
    let depth = unsafe { xsltMaxDepth };
    // SAFETY: as above.
    let vars = unsafe { xsltMaxVars };
    (i64::from(depth), i64::from(vars))
}

/// `xsltLibxsltVersion` and `xsltLibxmlVersion`, as `PHP_MINFO` prints them.
pub(crate) fn versions() -> (i32, i32) {
    // SAFETY: two constants of the library.
    let xslt = unsafe { xsltLibxsltVersion };
    // SAFETY: as above.
    let xml = unsafe { xsltLibxmlVersion };
    (xslt, xml)
}

/// `xsl_libxslt_error_handler` past the shim: the message, with libxslt's
/// names for the two limits put as the properties php names them, joined
/// to the capture that runs as libxml's generic handler's errors are.
#[unsafe(no_mangle)]
extern "C" fn php_xsl_error_text(text: *const c_char, len: usize, whole: c_int) {
    if text.is_null() {
        return;
    }
    // SAFETY: the shim hands `len` bytes of a live string.
    let bytes = unsafe { std::slice::from_raw_parts(text.cast::<u8>(), len) };
    let mut message = bytes.to_vec();
    if whole != 0 {
        for (search, replace) in [
            (&b"xsltMaxDepth (--maxdepth)"[..], &b"$maxTemplateDepth"[..]),
            (b"maxTemplateVars (--maxvars)", b"$maxTemplateVars"),
        ] {
            if let Some(at) = bytes.windows(search.len()).position(|w| w == search) {
                message = [&bytes[..at], replace, &bytes[at + search.len()..]].concat();
                break;
            }
        }
    }
    Raised::generic(&message);
}

/// `xsltStylesheetPtr`, owned: freed with the document it compiled.
pub(crate) struct Stylesheet(NonNull<c_void>);

// SAFETY: a stylesheet is reached only from the request that compiled it,
// one thread at a time (ADR-0005).
unsafe impl Send for Stylesheet {}

impl Drop for Stylesheet {
    fn drop(&mut self) {
        // SAFETY: the stylesheet this owns, freed once with its document.
        unsafe { xsltFreeStylesheet(self.0.as_ptr()) };
    }
}

impl Stylesheet {
    /// `xsltParseStylesheetDoc(doc)` with the request lent to the loads its
    /// imports make: the stylesheet, which owns `doc` from here, or `None`,
    /// leaving `doc` the caller's; what libxml and libxslt raised; what a
    /// loader threw.
    pub(crate) fn parse(
        req: &mut Request,
        doc: Node,
    ) -> (Option<Stylesheet>, Vec<Raised>, Option<Thrown>) {
        let (sheet, raised, thrown) = Raised::capture_request(req, || {
            php_ext_libxml::ffi::Library::stylesheet_defaults(|| {
                // SAFETY: a live document no one else frees while it is parsed.
                unsafe { xsltParseStylesheetDoc(doc.as_ptr()) }
            })
        });
        (NonNull::new(sheet).map(Stylesheet), raised, thrown)
    }

    /// `sheet->doc`.
    pub(crate) fn doc(&self) -> Option<Node> {
        // SAFETY: the live stylesheet this owns.
        let doc = unsafe { php_xsl_sheet_doc(self.0.as_ptr()) };
        // SAFETY: the stylesheet's own document, live while it is.
        unsafe { Node::from_ptr(doc) }
    }
}

/// One compiled stylesheet the request keeps for an `XSLTProcessor`
/// ([`Memory::adopt`], ADR-0013): what its payload stores, given up by
/// [`Sheet::close`]. A transformation holds it for its span
/// ([`SheetId::held`]), so a callback's `importStylesheet()` that
/// replaces it leaves its free to the transformation's end, where php's
/// `xsltFreeStylesheet` frees the one libxslt is transforming with
/// (#2319).
pub(crate) struct Sheet(Foreign<Stylesheet>);

/// The name of a [`Sheet`], copied freely.
#[derive(Clone, Copy)]
pub(crate) struct SheetId(ForeignId<Stylesheet>);

/// A stylesheet held for the span `'h` of one [`SheetId::held`], which
/// it cannot outlive, so the pointer it carries is live while it lives.
#[derive(Clone, Copy)]
pub(crate) struct Compiled<'h> {
    raw: NonNull<c_void>,
    span: PhantomData<Held<'h, Stylesheet>>,
}

impl Sheet {
    /// `stylesheet` kept by the request.
    pub(crate) fn adopt(req: &mut Request, stylesheet: Stylesheet) -> Sheet {
        Sheet(req.memory_mut().adopt(stylesheet))
    }

    pub(crate) fn id(&self) -> SheetId {
        SheetId(self.0.id())
    }

    /// Given up: freed now, or as the last transformation that holds it
    /// ends.
    pub(crate) fn close(self, req: &mut Request) {
        drop(req.memory_mut().disown(self.0));
    }
}

impl SheetId {
    /// `body` served with the stylesheet held ([`Memory::holding`]);
    /// `None` where its owner gave it up before.
    pub(crate) fn held<R>(
        self,
        req: &mut Request,
        body: impl for<'h> FnOnce(&mut Request, Compiled<'h>) -> R,
    ) -> Option<R> {
        req.memory_mut()
            .holding(self.0, |req, held| {
                let raw = held.get(req)?.0;
                let sheet = Compiled {
                    raw,
                    span: PhantomData,
                };
                Some(body(req, sheet))
            })
            .flatten()
    }
}

impl Compiled<'_> {
    /// `xsltSaveResultToString`: the bytes, `None` where it failed, and
    /// empty where it wrote none.
    pub(crate) fn save_to_string(&self, result: Node) -> Option<Vec<u8>> {
        let mut out: *mut u8 = ptr::null_mut();
        let mut len: c_int = 0;
        // SAFETY: a live result document and the stylesheet held; the two out
        // pointers are this frame's.
        let ret = unsafe {
            xsltSaveResultToString(
                &raw mut out,
                &raw mut len,
                result.as_ptr(),
                self.raw.as_ptr(),
            )
        };
        let bytes = if out.is_null() {
            Vec::new()
        } else {
            let n = usize::try_from(len).unwrap_or(0);
            // SAFETY: `len` bytes libxml allocated at `out`.
            let copy = unsafe { std::slice::from_raw_parts(out, n) }.to_vec();
            // SAFETY: libxml's allocation, freed once.
            unsafe { php_xsl_free(out.cast()) };
            copy
        };
        (ret >= 0).then_some(bytes)
    }

    /// `xsltSaveResultToFilename(uri, result, sheet, 0)`, written through
    /// php's streams: what it answered, and what libxml raised and a
    /// stream threw.
    pub(crate) fn save_to_file(
        &self,
        req: &mut Request,
        uri: &[u8],
        result: Node,
    ) -> (i64, Vec<Raised>, Option<Thrown>) {
        let u = cstr(uri);
        let (ret, raised, thrown) = Raised::capture_request(req, || {
            // SAFETY: a NUL-terminated URI, a live result and the stylesheet held.
            unsafe {
                xsltSaveResultToFilename(u.as_ptr().cast(), result.as_ptr(), self.raw.as_ptr(), 0)
            }
        });
        (i64::from(ret), raised, thrown)
    }
}

/// `xsltTransformContextPtr` over one stylesheet and one document, owned,
/// with the security preferences set on it.
pub(crate) struct Transform<'s> {
    ctxt: NonNull<c_void>,
    sheet: Compiled<'s>,
    doc: Node,
    prefs: Option<NonNull<c_void>>,
}

impl Drop for Transform<'_> {
    fn drop(&mut self) {
        // SAFETY: the context this owns, freed once.
        unsafe { xsltFreeTransformContext(self.ctxt.as_ptr()) };
        if let Some(prefs) = self.prefs {
            // SAFETY: the preferences this made, freed once after the
            // context that read them.
            unsafe { xsltFreeSecurityPrefs(prefs.as_ptr()) };
        }
    }
}

/// `XSLT_SECPREF_*`, in php's bit order.
const SECURITY: [(i64, c_int); 5] = [(2, 1), (4, 2), (8, 3), (16, 4), (32, 5)];

impl<'s> Transform<'s> {
    /// `xsltNewTransformContext(sheet, doc)`.
    pub(crate) fn new(sheet: Compiled<'s>, doc: Node) -> Option<Transform<'s>> {
        // SAFETY: a stylesheet held for the transformation, and a live
        // document.
        let ctxt =
            NonNull::new(unsafe { xsltNewTransformContext(sheet.raw.as_ptr(), doc.as_ptr()) })?;
        Some(Transform {
            ctxt,
            sheet,
            doc,
            prefs: None,
        })
    }

    /// `xsltQuoteOneUserParam`: whether it took.
    pub(crate) fn quote_param(&self, name: &[u8], value: &[u8]) -> bool {
        let (n, v) = (cstr(name), cstr(value));
        // SAFETY: the live context and NUL-terminated strings, copied.
        unsafe { xsltQuoteOneUserParam(self.ctxt.as_ptr(), n.as_ptr(), v.as_ptr()) >= 0 }
    }

    /// `ctxt->xinclude`, `maxTemplateDepth` and `maxTemplateVars`.
    pub(crate) fn set_limits(&self, xinclude: bool, depth: i64, vars: i64) {
        let depth = c_int::try_from(depth).unwrap_or(c_int::MAX);
        let vars = c_int::try_from(vars).unwrap_or(c_int::MAX);
        // SAFETY: the live context.
        unsafe { php_xsl_set_limits(self.ctxt.as_ptr(), c_int::from(xinclude), depth, vars) };
    }

    /// The preferences of `XSLTProcessor::setSecurityPrefs`, each set bit
    /// forbidden: whether every step took.
    pub(crate) fn secure(&mut self, prefs: i64) -> bool {
        if prefs == 0 {
            return true;
        }
        // SAFETY: no argument.
        let Some(sec) = NonNull::new(unsafe { xsltNewSecurityPrefs() }) else {
            return false;
        };
        self.prefs = Some(sec);
        let mut ok = true;
        for (bit, option) in SECURITY {
            if prefs & bit != 0 {
                // SAFETY: live preferences and libxslt's own check.
                ok &=
                    unsafe { xsltSetSecurityPrefs(sec.as_ptr(), option, xsltSecurityForbid) } == 0;
            }
        }
        // SAFETY: live preferences and context.
        ok &= unsafe { xsltSetCtxtSecurityPrefs(sec.as_ptr(), self.ctxt.as_ptr()) } == 0;
        ok
    }

    /// `xsltRegisterExtFunction(ctxt, name, ns, xsl_ext_function_trampoline)`.
    pub(crate) fn register(&self, ns: &[u8], name: &[u8]) {
        let (n, u) = (cstr(name), cstr(ns));
        // SAFETY: the live context and NUL-terminated strings, copied.
        unsafe {
            xsltRegisterExtFunction(self.ctxt.as_ptr(), n.as_ptr(), u.as_ptr(), XPath::CUSTOM)
        };
    }

    /// `xsltApplyStylesheetUser` with the profile written to `profile`,
    /// the request lent to the functions the stylesheet calls through
    /// `handler` and to its loads: the document made, which is the caller's, what
    /// libxml and libxslt raised, and what a program's code threw.
    pub(crate) fn apply(
        &self,
        req: &mut Request,
        profile: Option<&[u8]>,
        handler: &mut Handler<'_>,
    ) -> (Option<NewDoc>, Vec<Raised>, Option<Thrown>) {
        let path = profile.map(cstr);
        let path_ptr = path
            .as_ref()
            .map_or(ptr::null(), |p| p.as_ptr().cast::<c_char>());
        // SAFETY: the live context.
        let xpath = unsafe { php_xsl_xpath_context(self.ctxt.as_ptr()) };
        let run = || {
            // SAFETY: the live stylesheet, document and context, and a
            // NUL-terminated path or null.
            unsafe {
                php_xsl_apply(
                    self.sheet.raw.as_ptr(),
                    self.doc.as_ptr(),
                    self.ctxt.as_ptr(),
                    path_ptr,
                )
            }
        };
        // SAFETY: the context's own `XPath` context, which libxslt leaves
        // the `userData` of to its caller.
        let (result, raised, thrown) = unsafe { XPath::lend(xpath, req, handler, run) };
        // SAFETY: the result libxslt made, which nothing counts or frees
        // until the caller does.
        (unsafe { NewDoc::from_ptr(result) }, raised, thrown)
    }
}
