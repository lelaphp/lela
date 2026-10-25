//! The binding to libtidy (design §7.2, revision 72; `unsafe.homes`'
//! `ext/*/src/ffi/`): the keg's tidy-html5 5.8, the library the oracle
//! links, which `build.rs` finds through `pkg-config`, and the one home
//! of this crate's `unsafe`. The declarations are `tidy.h`'s and
//! `tidybuffio.h`'s, written here, no bindgen.
//!
//! php hands libtidy `emalloc`, `erealloc` and `efree` through
//! `tidySetMallocCall` and its kin (`Z/ext/tidy/tidy.c:175-188`), so what
//! a document holds is on the request's heap and `memory_get_usage()`
//! sees it. Here each [`Doc`] is made by `tidyCreateWithAllocator` over a
//! [`Heap`] of its own, whose callbacks are the same three
//! ([`emalloc`], [`erealloc`], [`efree`]), and every buffer is
//! initialised over the same allocator; a call into libtidy is made
//! through [`Doc::lend`], which lends the request's heap for that call
//! alone (ADR-0014). Where the memory limit refuses a block, libtidy,
//! which checks no allocation, is handed one from the process's
//! allocator, and the call answers the limit's fatal, as php's `emalloc`
//! bails out.
//!
//! A [`Node`] names a node of a document's tree by the generation the
//! tree was parsed or repaired in; a node of an older tree, which
//! libtidy freed, reads as none.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_ulong, c_void};
use std::ptr::NonNull;

use php_ext::raising::Thrown;
use php_ext::request::{Request, efree, emalloc, erealloc};
use php_ext::values::Emalloc;

type Opaque = *const c_void;

/// `TidyAllocator`, what libtidy is handed: the first field of a [`Heap`].
#[repr(C)]
struct Allocator {
    vtbl: *const Vtbl,
}

/// `TidyAllocatorVtbl`.
#[repr(C)]
struct Vtbl {
    alloc: extern "C" fn(*mut Allocator, usize) -> *mut c_void,
    realloc: extern "C" fn(*mut Allocator, *mut c_void, usize) -> *mut c_void,
    free: extern "C" fn(*mut Allocator, *mut c_void),
    panic: extern "C" fn(*mut Allocator, *const c_char),
}

/// `TidyBuffer`.
#[repr(C)]
struct Buffer {
    allocator: *mut Allocator,
    bp: *mut u8,
    size: c_uint,
    allocated: c_uint,
    next: c_uint,
}

unsafe extern "C" {
    fn tidyCreateWithAllocator(allocator: *mut Allocator) -> Opaque;
    fn tidyRelease(doc: Opaque);
    fn tidySetErrorBuffer(doc: Opaque, buf: *mut Buffer) -> c_int;
    fn tidyOptSetBool(doc: Opaque, id: c_int, value: c_int) -> c_int;
    fn tidyOptGetBool(doc: Opaque, id: c_int) -> c_int;
    fn tidyOptSetInt(doc: Opaque, id: c_int, value: c_ulong) -> c_int;
    fn tidyOptGetInt(doc: Opaque, id: c_int) -> c_ulong;
    fn tidyOptSetValue(doc: Opaque, id: c_int, value: *const c_char) -> c_int;
    fn tidyOptGetValue(doc: Opaque, id: c_int) -> *const c_char;
    fn tidyLoadConfig(doc: Opaque, path: *const c_char) -> c_int;
    fn tidySetCharEncoding(doc: Opaque, encoding: *const c_char) -> c_int;
    fn tidyParseBuffer(doc: Opaque, buf: *mut Buffer) -> c_int;
    fn tidyCleanAndRepair(doc: Opaque) -> c_int;
    fn tidyRunDiagnostics(doc: Opaque) -> c_int;
    fn tidySaveBuffer(doc: Opaque, buf: *mut Buffer) -> c_int;
    fn tidyLibraryVersion() -> *const c_char;
    fn tidyReleaseDate() -> *const c_char;
    fn tidyGetOptionByName(doc: Opaque, name: *const c_char) -> Opaque;
    fn tidyOptGetCategory(opt: Opaque) -> c_int;
    fn tidyOptGetType(opt: Opaque) -> c_int;
    fn tidyOptGetId(opt: Opaque) -> c_int;
    fn tidyOptGetName(opt: Opaque) -> *const c_char;
    fn tidyOptGetDoc(doc: Opaque, opt: Opaque) -> *const c_char;
    fn tidyGetOptionList(doc: Opaque) -> Opaque;
    fn tidyGetNextOption(doc: Opaque, iter: *mut Opaque) -> Opaque;
    fn tidyStatus(doc: Opaque) -> c_int;
    fn tidyDetectedHtmlVersion(doc: Opaque) -> c_int;
    fn tidyDetectedXhtml(doc: Opaque) -> c_int;
    fn tidyDetectedGenericXml(doc: Opaque) -> c_int;
    fn tidyErrorCount(doc: Opaque) -> c_uint;
    fn tidyWarningCount(doc: Opaque) -> c_uint;
    fn tidyAccessWarningCount(doc: Opaque) -> c_uint;
    fn tidyConfigErrorCount(doc: Opaque) -> c_uint;
    fn tidyGetRoot(doc: Opaque) -> Opaque;
    fn tidyGetHtml(doc: Opaque) -> Opaque;
    fn tidyGetHead(doc: Opaque) -> Opaque;
    fn tidyGetBody(doc: Opaque) -> Opaque;
    fn tidyGetParent(node: Opaque) -> Opaque;
    fn tidyGetChild(node: Opaque) -> Opaque;
    fn tidyGetNext(node: Opaque) -> Opaque;
    fn tidyGetPrev(node: Opaque) -> Opaque;
    fn tidyNodeGetText(doc: Opaque, node: Opaque, buf: *mut Buffer) -> c_int;
    fn tidyNodeGetName(node: Opaque) -> *const c_char;
    fn tidyNodeGetType(node: Opaque) -> c_int;
    fn tidyNodeLine(node: Opaque) -> c_uint;
    fn tidyNodeColumn(node: Opaque) -> c_uint;
    fn tidyNodeIsProp(doc: Opaque, node: Opaque) -> c_int;
    fn tidyNodeGetId(node: Opaque) -> c_int;
    fn tidyAttrFirst(node: Opaque) -> Opaque;
    fn tidyAttrNext(attr: Opaque) -> Opaque;
    fn tidyAttrName(attr: Opaque) -> *const c_char;
    fn tidyAttrValue(attr: Opaque) -> *const c_char;
    fn tidyBufInitWithAllocator(buf: *mut Buffer, allocator: *mut Allocator);
    fn tidyBufAttach(buf: *mut Buffer, bp: *mut u8, size: c_uint);
    fn tidyBufFree(buf: *mut Buffer);
}

/// `TidyForceOutput` and `TidyMark`, the two options php sets on every
/// document it makes.
const FORCE_OUTPUT: c_int = 30;
const MARK: c_int = 49;
/// `TidyInternalCategory`, the options `tidyOptGetCategory` keeps from a
/// program.
const INTERNAL_CATEGORY: c_int = 312;

/// The bytes of a C string libtidy answered, `None` for its NULL.
fn text(at: *const c_char) -> Option<Vec<u8>> {
    if at.is_null() {
        return None;
    }
    // SAFETY: libtidy answers a NUL-terminated string that lives as long
    // as the document or the library, which this copy precedes.
    let text = unsafe { CStr::from_ptr(at) };
    Some(text.to_bytes().to_vec())
}

/// `bytes` as C reads `ZSTR_VAL`, ending at its first NUL.
fn c_string(bytes: &[u8]) -> CString {
    CString::new(php_ext::coerce::c_str(bytes)).unwrap_or_default()
}

/// `tidyLibraryVersion()`: `5.8.0` on the oracle's machine.
pub(crate) fn version() -> String {
    // SAFETY: `tidyLibraryVersion` answers the library's static string.
    let at = unsafe { tidyLibraryVersion() };
    String::from_utf8_lossy(&text(at).unwrap_or_default()).into_owned()
}

/// `tidyReleaseDate()`: `2021/07/10` for 5.8.0.
pub(crate) fn release_date() -> String {
    // SAFETY: `tidyReleaseDate` answers the library's static string.
    let at = unsafe { tidyReleaseDate() };
    String::from_utf8_lossy(&text(at).unwrap_or_default()).into_owned()
}

/// One document's `TidyAllocator`: the vtable libtidy calls through and
/// the reason libtidy's panic left to raise.
#[repr(C)]
struct Heap {
    base: Allocator,
    panicked: Option<Vec<u8>>,
}

static VTBL: Vtbl = Vtbl {
    alloc,
    realloc,
    free,
    panic,
};

/// `php_tidy_malloc`: `emalloc(len)` on the lent request's heap, the
/// bytes left as the heap finds them.
extern "C" fn alloc(_heap: *mut Allocator, len: usize) -> *mut c_void {
    emalloc(len)
}

/// `php_tidy_free`: `efree(ptr)`.
extern "C" fn free(_heap: *mut Allocator, block: *mut c_void) {
    // SAFETY: libtidy frees a block once, and only one this allocator
    // answered, under the lend of the call it is in.
    unsafe { efree(block) }
}

/// `php_tidy_realloc`: `erealloc(ptr, len)`, the bytes kept to the
/// shorter length and left past it as they are found.
extern "C" fn realloc(_heap: *mut Allocator, block: *mut c_void, len: usize) -> *mut c_void {
    // SAFETY: as in `free`, a block this allocator answered.
    unsafe { erealloc(block, len) }
}

/// `php_tidy_panic`, kept for the call's caller to raise as php's
/// `E_ERROR`.
extern "C" fn panic(heap: *mut Allocator, message: *const c_char) {
    // SAFETY: libtidy hands back the allocator a document was made with,
    // a `Heap` its `Doc` keeps boxed, and calls it from one thread.
    let heap = unsafe { &mut *heap.cast::<Heap>() };
    heap.panicked = Some(text(message).unwrap_or_default());
}

/// What a call into libtidy came to beside its answer: the memory
/// limit's fatal, or libtidy's panic with its reason.
enum Failed {
    Limit(Thrown),
    Panic(Vec<u8>),
}

/// A `TidyOption`, a row of libtidy's static option table.
#[derive(Clone, Copy)]
pub(crate) struct Opt(NonNull<c_void>);

/// `TidyOptionType`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OptType {
    String,
    Integer,
    Boolean,
}

impl Opt {
    /// `tidyOptGetName`.
    pub(crate) fn name(self) -> Vec<u8> {
        // SAFETY: the option is a row of the static table.
        text(unsafe { tidyOptGetName(self.0.as_ptr()) }).unwrap_or_default()
    }

    /// `tidyOptGetType`.
    pub(crate) fn kind(self) -> OptType {
        // SAFETY: as in `name`.
        match unsafe { tidyOptGetType(self.0.as_ptr()) } {
            0 => OptType::String,
            1 => OptType::Integer,
            _ => OptType::Boolean,
        }
    }

    /// `tidyOptGetCategory(opt) == TidyInternalCategory`.
    pub(crate) fn is_internal(self) -> bool {
        // SAFETY: as in `name`.
        unsafe { tidyOptGetCategory(self.0.as_ptr()) == INTERNAL_CATEGORY }
    }

    fn id(self) -> c_int {
        // SAFETY: as in `name`.
        unsafe { tidyOptGetId(self.0.as_ptr()) }
    }
}

/// A node of a document's tree, of the generation it was read in.
#[derive(Clone, Copy)]
pub(crate) struct Node {
    at: NonNull<c_void>,
    generation: u32,
}

// SAFETY: a node is an address the request's own thread reads, through
// the `Doc` that owns its tree and only while the tree is the one it was
// read from.
unsafe impl Send for Node {}

/// `TidyNodeType`'s values php names.
pub(crate) const NODE_ROOT: i64 = 0;
pub(crate) const NODE_DOCTYPE: i64 = 1;
pub(crate) const NODE_COMMENT: i64 = 2;
pub(crate) const NODE_TEXT: i64 = 4;
pub(crate) const NODE_START: i64 = 5;
pub(crate) const NODE_END: i64 = 6;
pub(crate) const NODE_STARTEND: i64 = 7;
pub(crate) const NODE_ASP: i64 = 10;
pub(crate) const NODE_JSTE: i64 = 11;
pub(crate) const NODE_PHP: i64 = 12;

/// Which of a document's four nodes `tidyGetRoot` and its kin answer.
#[derive(Clone, Copy)]
pub(crate) enum Base {
    Root,
    Html,
    Head,
    Body,
}

/// A node's attributes, each name with its value where it has one.
pub(crate) type Attributes = Vec<(Vec<u8>, Option<Vec<u8>>)>;

/// What a node's properties are made of, read in one pass.
pub(crate) struct NodeFacts {
    pub(crate) text: Option<Vec<u8>>,
    pub(crate) name: Option<Vec<u8>>,
    pub(crate) kind: i64,
    pub(crate) line: i64,
    pub(crate) column: i64,
    pub(crate) proprietary: bool,
    pub(crate) id: i64,
    pub(crate) attributes: Option<Attributes>,
    pub(crate) children: Vec<Node>,
}

/// One `TidyDoc`, its allocator and its error buffer, php's
/// `PHPTidyDoc` less the count, which the extension keeps. What libtidy
/// holds is on the request's heap, as php's `emalloc` callbacks put it.
///
/// ```php
/// $w = new tidy; $w2 = tidy_parse_string('<p>w</p>');
/// $m = memory_get_usage(); $t = new tidy; echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $t->parseString('<p>x</p>'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $b = $t->body(); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); unset($b, $t); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $s = tidy_repair_string('<p>y'); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 16664
/// 10752
/// 1328
/// -28744
/// 112
/// ```
pub(crate) struct Doc {
    raw: Opaque,
    heap: NonNull<Heap>,
    /// The error buffer's `TidyBuffer`, `emalloc(sizeof(TidyBuffer))`.
    errbuf: Option<Emalloc>,
    generation: u32,
}

// SAFETY: a document is the request's, read and written on its thread
// alone, and its allocator reaches the request only while a call lends it.
unsafe impl Send for Doc {}

impl Doc {
    /// `tidyCreate()`, an error buffer set, and `TidyForceOutput` on and
    /// `TidyMark` off, as php makes every document; `None` where
    /// `tidySetErrorBuffer` refused, php's `Could not set Tidy error
    /// buffer`.
    pub(crate) fn new(req: &mut Request) -> Result<Option<Doc>, Thrown> {
        let heap = Box::new(Heap {
            base: Allocator {
                vtbl: &raw const VTBL,
            },
            panicked: None,
        });
        let mut doc = Doc {
            raw: std::ptr::null(),
            heap: NonNull::from(Box::leak(heap)),
            errbuf: None,
            generation: 0,
        };
        let heap = doc.heap.as_ptr().cast::<Allocator>();
        let mut errbuf = Emalloc::new(req, std::mem::size_of::<Buffer>())?;
        // The heap's blocks are aligned for any `emalloc` caller's struct.
        #[allow(clippy::cast_ptr_alignment)]
        let buf = errbuf.as_mut_ptr().cast::<Buffer>();
        let set = doc.lend(req, |_| {
            // SAFETY: the allocator outlives the document it makes.
            let raw = unsafe { tidyCreateWithAllocator(heap) };
            // SAFETY: `buf` is the error buffer's own block, as long as a
            // `TidyBuffer` and aligned for one.
            unsafe { tidyBufInitWithAllocator(buf, heap) };
            // SAFETY: the document was just made; the buffer lives as long.
            let set = unsafe { tidySetErrorBuffer(raw, buf) };
            (raw, set)
        });
        let (raw, set) = match set {
            Ok(made) => made,
            Err(failed) => return Err(Doc::fail(req, failed)),
        };
        doc.raw = raw;
        doc.errbuf = Some(errbuf);
        if set != 0 {
            doc.release(req);
            return Ok(None);
        }
        doc.set_bool(req, FORCE_OUTPUT, true)?;
        doc.set_bool(req, MARK, false)?;
        Ok(Some(doc))
    }

    /// `call` with `req`'s heap lent for its length (ADR-0014), and what
    /// it left to raise.
    fn lend<R>(&mut self, req: &mut Request, call: impl FnOnce(Opaque) -> R) -> Result<R, Failed> {
        let raw = self.raw;
        let answer = req.memory_mut().lend_heap(|| call(raw));
        // SAFETY: the box is this document's, alive until it drops, and
        // libtidy, which wrote the panic, has returned.
        let panicked = unsafe { (*self.heap.as_ptr()).panicked.take() };
        match (answer, panicked) {
            (Err(thrown), _) => Err(Failed::Limit(thrown)),
            (Ok(_), Some(reason)) => Err(Failed::Panic(reason)),
            (Ok(answer), None) => Ok(answer),
        }
    }

    /// A failed call made a `Thrown`: the limit's, or php's `E_ERROR` for
    /// libtidy's panic.
    fn fail(req: &mut Request, failed: Failed) -> Thrown {
        match failed {
            Failed::Limit(thrown) => thrown,
            Failed::Panic(reason) => {
                let text = format!(
                    "Could not allocate memory for tidy! (Reason: {})",
                    String::from_utf8_lossy(&reason)
                );
                php_ext::raising::fatal(req, &text)
            }
        }
    }

    /// [`Doc::lend`] with its failure made a `Thrown`.
    fn call<R>(&mut self, req: &mut Request, call: impl FnOnce(Opaque) -> R) -> Result<R, Thrown> {
        match self.lend(req, call) {
            Ok(answer) => Ok(answer),
            Err(failed) => Err(Doc::fail(req, failed)),
        }
    }

    /// `tidyBufFree(errbuf)`, `efree(errbuf)` and `tidyRelease(doc)`, in
    /// php's order: everything libtidy held given back.
    pub(crate) fn release(mut self, req: &mut Request) {
        let mut errbuf = self.errbuf.take();
        // The heap's blocks are aligned for any `emalloc` caller's struct.
        #[allow(clippy::cast_ptr_alignment)]
        let buf = errbuf
            .as_mut()
            .map(|errbuf| errbuf.as_mut_ptr().cast::<Buffer>());
        self.lend(req, |raw| {
            if let Some(buf) = buf {
                // SAFETY: the buffer was initialised over this allocator.
                unsafe { tidyBufFree(buf) };
            }
            if !raw.is_null() {
                // SAFETY: the document is released once, here.
                unsafe { tidyRelease(raw) };
            }
        })
        .ok();
        if let Some(errbuf) = errbuf {
            errbuf.free(req);
        }
    }

    fn set_bool(&mut self, req: &mut Request, id: c_int, on: bool) -> Result<bool, Thrown> {
        // SAFETY: a live document and one of its option ids.
        self.call(
            req,
            |raw| unsafe { tidyOptSetBool(raw, id, c_int::from(on)) } != 0,
        )
    }

    /// `tidyLoadConfig`: below zero where the file did not open, above
    /// where it held errors.
    pub(crate) fn load_config(&mut self, req: &mut Request, path: &[u8]) -> Result<i32, Thrown> {
        let path = c_string(path);
        // SAFETY: a live document and a C string.
        self.call(req, |raw| unsafe { tidyLoadConfig(raw, path.as_ptr()) })
    }

    /// `tidySetCharEncoding`: below zero for a name libtidy refuses.
    pub(crate) fn set_encoding(&mut self, req: &mut Request, name: &[u8]) -> Result<i32, Thrown> {
        let name = c_string(name);
        // SAFETY: a live document and a C string.
        self.call(req, |raw| unsafe {
            tidySetCharEncoding(raw, name.as_ptr())
        })
    }

    /// `tidyParseBuffer` over `input`, attached and not copied; the tree
    /// before it is no longer read.
    pub(crate) fn parse(&mut self, req: &mut Request, input: &[u8]) -> Result<i32, Thrown> {
        self.generation = self.generation.wrapping_add(1);
        let heap = self.heap.as_ptr().cast::<Allocator>();
        let len = c_uint::try_from(input.len()).unwrap_or(c_uint::MAX);
        let mut buf = empty_buffer();
        self.call(req, |raw| {
            // SAFETY: a buffer of this frame over the document's allocator.
            unsafe { tidyBufInitWithAllocator(&raw mut buf, heap) };
            // SAFETY: libtidy reads the attached bytes and never frees
            // them, since nothing frees this buffer.
            unsafe { tidyBufAttach(&raw mut buf, input.as_ptr().cast_mut(), len) };
            // SAFETY: a live document and the buffer above.
            unsafe { tidyParseBuffer(raw, &raw mut buf) }
        })
    }

    /// `tidyCleanAndRepair`, after which the tree is another.
    pub(crate) fn clean_repair(&mut self, req: &mut Request) -> Result<i32, Thrown> {
        self.generation = self.generation.wrapping_add(1);
        // SAFETY: a live document.
        self.call(req, |raw| unsafe { tidyCleanAndRepair(raw) })
    }

    /// `tidyRunDiagnostics`.
    pub(crate) fn diagnose(&mut self, req: &mut Request) -> Result<i32, Thrown> {
        // SAFETY: a live document.
        self.call(req, |raw| unsafe { tidyRunDiagnostics(raw) })
    }

    /// `tidySaveBuffer` into a buffer of the document's allocator: its
    /// bytes, the NUL libtidy ends with among them.
    pub(crate) fn save(&mut self, req: &mut Request) -> Result<Vec<u8>, Thrown> {
        let heap = self.heap.as_ptr().cast::<Allocator>();
        let mut buf = empty_buffer();
        self.call(req, |raw| {
            // SAFETY: a buffer of this frame over the document's allocator.
            unsafe { tidyBufInitWithAllocator(&raw mut buf, heap) };
            // SAFETY: a live document and the buffer above.
            unsafe { tidySaveBuffer(raw, &raw mut buf) };
            let bytes = buffered(&buf);
            // SAFETY: the buffer's bytes were allocated over its allocator.
            unsafe { tidyBufFree(&raw mut buf) };
            bytes
        })
    }

    /// The error buffer's bytes while it has any, `errbuf->bp` and
    /// `errbuf->size`.
    pub(crate) fn errors(&self) -> Option<Vec<u8>> {
        let errbuf = self.errbuf.as_ref()?;
        // SAFETY: the block holds the `TidyBuffer` libtidy writes, read
        // between calls.
        #[allow(clippy::cast_ptr_alignment)]
        let buf = unsafe { &*errbuf.bytes().as_ptr().cast::<Buffer>() };
        if buf.bp.is_null() {
            return None;
        }
        Some(buffered(buf))
    }

    /// `tidyGetOptionByName`.
    pub(crate) fn option(&self, name: &[u8]) -> Option<Opt> {
        let name = c_string(name);
        // SAFETY: a live document and a C string.
        let at = unsafe { tidyGetOptionByName(self.raw, name.as_ptr()) };
        NonNull::new(at.cast_mut()).map(Opt)
    }

    /// Every option, `tidyGetOptionList` and `tidyGetNextOption`.
    pub(crate) fn options(&self) -> Vec<Opt> {
        let mut out = Vec::new();
        // SAFETY: a live document.
        let mut iter = unsafe { tidyGetOptionList(self.raw) };
        while !iter.is_null() {
            // SAFETY: the iterator `tidyGetOptionList` answered.
            let opt = unsafe { tidyGetNextOption(self.raw, &raw mut iter) };
            if let Some(opt) = NonNull::new(opt.cast_mut()) {
                out.push(Opt(opt));
            }
        }
        out
    }

    /// `tidyOptGetDoc`.
    pub(crate) fn option_doc(&self, opt: Opt) -> Option<Vec<u8>> {
        // SAFETY: a live document and a row of the option table.
        text(unsafe { tidyOptGetDoc(self.raw, opt.0.as_ptr()) })
    }

    /// `tidyOptGetValue`, `None` for its NULL.
    pub(crate) fn get_value(&self, opt: Opt) -> Option<Vec<u8>> {
        // SAFETY: a live document and an option id.
        text(unsafe { tidyOptGetValue(self.raw, opt.id()) })
    }

    /// `tidyOptGetInt`.
    pub(crate) fn get_int(&self, opt: Opt) -> u64 {
        // SAFETY: a live document and an option id.
        unsafe { tidyOptGetInt(self.raw, opt.id()) }
    }

    /// `tidyOptGetBool`.
    pub(crate) fn get_bool(&self, opt: Opt) -> bool {
        // SAFETY: a live document and an option id.
        unsafe { tidyOptGetBool(self.raw, opt.id()) != 0 }
    }

    /// `tidyOptSetValue`.
    pub(crate) fn set_value(
        &mut self,
        req: &mut Request,
        opt: Opt,
        value: &[u8],
    ) -> Result<bool, Thrown> {
        let (id, value) = (opt.id(), c_string(value));
        // SAFETY: a live document, an option id and a C string.
        self.call(
            req,
            |raw| unsafe { tidyOptSetValue(raw, id, value.as_ptr()) } != 0,
        )
    }

    /// `tidyOptSetInt`, `zend_long` as C converts it to `ulong`.
    #[allow(clippy::cast_sign_loss)]
    pub(crate) fn set_int(
        &mut self,
        req: &mut Request,
        opt: Opt,
        value: i64,
    ) -> Result<bool, Thrown> {
        let id = opt.id();
        // SAFETY: a live document and an option id.
        self.call(
            req,
            |raw| unsafe { tidyOptSetInt(raw, id, value as c_ulong) } != 0,
        )
    }

    /// `tidyOptSetBool`, the `zend_long` as C converts it to `Bool`.
    pub(crate) fn set_flag(
        &mut self,
        req: &mut Request,
        opt: Opt,
        value: i64,
    ) -> Result<bool, Thrown> {
        let id = opt.id();
        #[allow(clippy::cast_possible_truncation)]
        let value = value as c_int;
        // SAFETY: a live document and an option id.
        self.call(req, |raw| unsafe { tidyOptSetBool(raw, id, value) } != 0)
    }

    /// `tidyStatus`, `tidyDetectedHtmlVersion` and the four counts, by
    /// what php's natives ask.
    pub(crate) fn status(&self, which: Status) -> i64 {
        let signed: unsafe extern "C" fn(Opaque) -> c_int = match which {
            Status::Overall => tidyStatus,
            Status::HtmlVersion => tidyDetectedHtmlVersion,
            Status::Xhtml => tidyDetectedXhtml,
            Status::Xml => tidyDetectedGenericXml,
            Status::Errors | Status::Warnings | Status::Access | Status::Config => {
                let count: unsafe extern "C" fn(Opaque) -> c_uint = match which {
                    Status::Errors => tidyErrorCount,
                    Status::Warnings => tidyWarningCount,
                    Status::Access => tidyAccessWarningCount,
                    _ => tidyConfigErrorCount,
                };
                // SAFETY: a live document, read alone.
                return i64::from(unsafe { count(self.raw) });
            }
        };
        // SAFETY: a live document, read alone.
        i64::from(unsafe { signed(self.raw) })
    }

    fn node(&self, at: Opaque) -> Option<Node> {
        NonNull::new(at.cast_mut()).map(|at| Node {
            at,
            generation: self.generation,
        })
    }

    /// A node of this tree, `None` for one of a tree libtidy freed.
    fn live(&self, node: Node) -> Option<Opaque> {
        (node.generation == self.generation).then_some(node.at.as_ptr().cast_const())
    }

    /// `tidyGetRoot`, `tidyGetHtml`, `tidyGetHead` or `tidyGetBody`.
    pub(crate) fn base(&self, which: Base) -> Option<Node> {
        let get: unsafe extern "C" fn(Opaque) -> Opaque = match which {
            Base::Root => tidyGetRoot,
            Base::Html => tidyGetHtml,
            Base::Head => tidyGetHead,
            Base::Body => tidyGetBody,
        };
        // SAFETY: a live document.
        let at = unsafe { get(self.raw) };
        self.node(at)
    }

    /// `tidyGetParent`, `tidyGetChild`, `tidyGetNext` or `tidyGetPrev`.
    pub(crate) fn step(&self, node: Node, to: Step) -> Option<Node> {
        let at = self.live(node)?;
        let get: unsafe extern "C" fn(Opaque) -> Opaque = match to {
            Step::Parent => tidyGetParent,
            Step::Child => tidyGetChild,
            Step::Next => tidyGetNext,
            Step::Prev => tidyGetPrev,
        };
        // SAFETY: a node of the document's current tree.
        let next = unsafe { get(at) };
        self.node(next)
    }

    /// `tidyNodeGetType`, `TidyNode_Root` for a freed node.
    pub(crate) fn kind(&self, node: Node) -> i64 {
        self.live(node).map_or(NODE_ROOT, |at| {
            // SAFETY: a node of the document's current tree.
            i64::from(unsafe { tidyNodeGetType(at) })
        })
    }

    /// `tidyNodeGetText` into a buffer of the document's allocator, `None`
    /// where it answers `no`.
    pub(crate) fn node_text(
        &mut self,
        req: &mut Request,
        node: Node,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let Some(at) = self.live(node) else {
            return Ok(None);
        };
        let heap = self.heap.as_ptr().cast::<Allocator>();
        let mut buf = empty_buffer();
        self.call(req, |raw| {
            // SAFETY: a buffer of this frame over the document's allocator.
            unsafe { tidyBufInitWithAllocator(&raw mut buf, heap) };
            // SAFETY: a live document, its node and the buffer above.
            let got = unsafe { tidyNodeGetText(raw, at, &raw mut buf) } != 0;
            let bytes = got.then(|| buffered(&buf));
            // SAFETY: the buffer's bytes were allocated over its allocator.
            unsafe { tidyBufFree(&raw mut buf) };
            bytes
        })
    }

    /// What `tidy_add_node_default_properties` reads of a node, in its
    /// order.
    pub(crate) fn facts(&mut self, req: &mut Request, node: Node) -> Result<NodeFacts, Thrown> {
        let read = self.node_text(req, node)?;
        let mut facts = NodeFacts {
            text: read,
            name: None,
            kind: NODE_ROOT,
            line: 0,
            column: 0,
            proprietary: false,
            id: 0,
            attributes: None,
            children: Vec::new(),
        };
        let Some(at) = self.live(node) else {
            return Ok(facts);
        };
        // SAFETY: a node of the document's current tree.
        facts.name = text(unsafe { tidyNodeGetName(at) });
        facts.kind = self.kind(node);
        // SAFETY: as above.
        facts.line = i64::from(unsafe { tidyNodeLine(at) });
        // SAFETY: as above.
        facts.column = i64::from(unsafe { tidyNodeColumn(at) });
        // SAFETY: a live document and its node.
        facts.proprietary = unsafe { tidyNodeIsProp(self.raw, at) } != 0;
        // SAFETY: a node of the document's current tree.
        facts.id = i64::from(unsafe { tidyNodeGetId(at) });
        // SAFETY: as above.
        let mut attr = unsafe { tidyAttrFirst(at) };
        if !attr.is_null() {
            let mut rows = Vec::new();
            while !attr.is_null() {
                // SAFETY: an attribute of the node, as libtidy chains them.
                if let Some(name) = text(unsafe { tidyAttrName(attr) }) {
                    // SAFETY: as above.
                    rows.push((name, text(unsafe { tidyAttrValue(attr) })));
                }
                // SAFETY: as above.
                attr = unsafe { tidyAttrNext(attr) };
            }
            facts.attributes = Some(rows);
        }
        let mut child = self.step(node, Step::Child);
        while let Some(next) = child {
            facts.children.push(next);
            child = self.step(next, Step::Next);
        }
        Ok(facts)
    }

    /// Whether the node is markup, `TidyNode_Start`, `_End` or
    /// `_StartEnd`.
    pub(crate) fn is_html(&self, node: Node) -> bool {
        matches!(self.kind(node), NODE_START | NODE_END | NODE_STARTEND)
    }
}

impl Drop for Doc {
    /// A document never released is the request's end's: the heap's
    /// blocks go with the heap, libtidy is not called again, and the
    /// allocator itself is given back.
    fn drop(&mut self) {
        // SAFETY: the box was leaked by `Doc::new` and is taken back once.
        drop(unsafe { Box::from_raw(self.heap.as_ptr()) });
    }
}

/// What [`Doc::status`] reads.
#[derive(Clone, Copy)]
pub(crate) enum Status {
    Overall,
    HtmlVersion,
    Xhtml,
    Xml,
    Errors,
    Warnings,
    Access,
    Config,
}

/// What [`Doc::step`] walks to.
#[derive(Clone, Copy)]
pub(crate) enum Step {
    Parent,
    Child,
    Next,
    Prev,
}

fn empty_buffer() -> Buffer {
    Buffer {
        allocator: std::ptr::null_mut(),
        bp: std::ptr::null_mut(),
        size: 0,
        allocated: 0,
        next: 0,
    }
}

/// A buffer's bytes, `bp[..size]`.
fn buffered(buf: &Buffer) -> Vec<u8> {
    if buf.bp.is_null() || buf.size == 0 {
        return Vec::new();
    }
    // SAFETY: libtidy keeps `size` bytes at `bp`.
    unsafe { std::slice::from_raw_parts(buf.bp, buf.size as usize) }.to_vec()
}
