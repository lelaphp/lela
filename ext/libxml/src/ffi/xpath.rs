//! libxml2's `XPath`: one evaluator owned by a `DOMXPath`, evaluated with php's
//! error capture and with the request reachable from the functions php
//! registers on it (`php:function`, `php:functionString` and each
//! `registerPhpFunctionNS` name), as `dom_xpath_ext_function_php` reaches
//! the object through `ctxt->context->userData`.

use std::ffi::{c_int, c_void};
use std::ptr::{self, NonNull};

use super::{Alloc, Call, Io, Raised, cstr, loader, sys};
use php_ext::raising::Thrown;
use php_ext::request::{ForeignId, Request};

use super::tree::{Kind, Node, Ns};

/// One member of a node set: a node, or a namespace node libxml made for
/// the namespace axis, copied out, since the set frees its copies with
/// the object `XPath` answered: the declaration's `href` and `prefix`, and the element
/// libxml kept in its `next`.
#[derive(Debug, Clone)]
pub enum Item {
    Node(Node),
    Namespace {
        href: Option<Vec<u8>>,
        prefix: Option<Vec<u8>>,
        parent: Option<Node>,
    },
}

fn items(set: *mut sys::xmlNodeSet) -> Vec<Item> {
    if set.is_null() {
        return Vec::new();
    }
    // SAFETY: a live node set of a live result.
    let s = unsafe { &*set };
    let len = usize::try_from(s.nodeNr).unwrap_or(0);
    if s.nodeTab.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: `nodeNr` entries at `nodeTab`.
    let table = unsafe { std::slice::from_raw_parts(s.nodeTab, len) };
    table
        .iter()
        .filter_map(|p| {
            let node = Node::from_raw(*p)?;
            // An `xmlNs` keeps its type in the same word an `xmlNode` does,
            // and every namespace node XPath hands out is one.
            if node.kind() == Kind::NamespaceDecl {
                let ns = Ns::from_raw(p.cast())?;
                return Some(Item::Namespace {
                    href: ns.href(),
                    prefix: ns.prefix(),
                    parent: ns.xpath_parent(),
                });
            }
            Some(Item::Node(node))
        })
        .collect()
}

/// What an evaluation answered.
#[derive(Debug, Clone)]
pub enum Answer {
    NodeSet(Vec<Item>),
    Bool(bool),
    Number(f64),
    Str(Vec<u8>),
    Other,
}

/// One argument a registered function is called with, in call order.
#[derive(Debug, Clone)]
pub enum Operand {
    Str(Vec<u8>),
    Bool(bool),
    Number(f64),
    Nodes(Vec<Item>),
}

/// Which registered function libxml called.
#[derive(Debug, Clone)]
pub enum Called {
    /// `php:function` or `php:functionString`: the name is the first
    /// argument; `strings` says node sets came as strings.
    Php { strings: bool },
    /// A name `registerPhpFunctionNS` registered in a namespace.
    Custom { ns: Vec<u8>, name: Vec<u8> },
}

/// What a registered function hands back to the expression.
#[derive(Debug, Clone)]
pub enum Returned {
    /// A node object: `xmlXPathNewNodeSet(nodep)`, a set of its node, or
    /// the empty set where the object stands for no node.
    NodeSet(Option<Node>),
    Bool(bool),
    Str(Vec<u8>),
    /// Nothing: the call threw or failed, so php pushes an empty string as
    /// the sentinel that keeps the stack balanced.
    Sentinel,
}

/// The handler an evaluation serves calls through.
pub type Handler<'a> =
    dyn FnMut(&mut Request, Called, Vec<Operand>) -> Result<Returned, Thrown> + 'a;

/// What one evaluation lends the functions it calls.
struct Session<'s> {
    req: *mut Request,
    handler: &'s mut Handler<'s>,
    thrown: Option<Thrown>,
    /// The loads' record where the evaluation lends them the request
    /// too ([`XPath::lend`]), so a throw from a function is the call's
    /// first throw and silences what libxml raises after it; null
    /// otherwise.
    io: *mut Io,
}

/// An `xmlXPathFunction` over an untyped `xmlXPathParserContextPtr`, as a library over
/// libxml2 registers one in an evaluator of its own: libxslt's
/// `xsltRegisterExtModuleFunction` and `xsltRegisterExtFunction` take it.
pub type Function = unsafe extern "C" fn(ctxt: *mut c_void, nargs: c_int);

/// `xmlXPathContextPtr`, owned.
pub struct XPath(NonNull<sys::xmlXPathContext>);

// SAFETY: the context and the document it reads are reached only from the
// request that made them, one thread at a time (ADR-0005).
unsafe impl Send for XPath {}

impl Drop for XPath {
    fn drop(&mut self) {
        // SAFETY: the context this owns, freed once.
        unsafe { sys::xmlXPathFreeContext(self.0.as_ptr()) };
    }
}

const PHP_NS: &[u8] = b"http://php.net/xpath\0";

impl XPath {
    /// `xmlXPathNewContext(doc)` with `php:functionString` and
    /// `php:function` registered, as `dom_xpath_construct` makes it.
    pub fn new(doc: Node) -> Option<XPath> {
        // SAFETY: a live document.
        let ctx = NonNull::new(unsafe { sys::xmlXPathNewContext(doc.raw().cast()) })?;
        let context = XPath(ctx);
        // SAFETY: a live context and NUL-terminated names.
        unsafe {
            sys::xmlXPathRegisterFuncNS(
                ctx.as_ptr(),
                c"functionString".as_ptr().cast(),
                PHP_NS.as_ptr(),
                Some(php_string),
            );
        }
        // SAFETY: as above.
        unsafe {
            sys::xmlXPathRegisterFuncNS(
                ctx.as_ptr(),
                c"function".as_ptr().cast(),
                PHP_NS.as_ptr(),
                Some(php_object),
            );
        }
        Some(context)
    }

    /// `xmlXPathNewContext(doc)` alone, as simplexml makes its evaluator:
    /// no function of php's registered.
    pub fn bare(doc: Node) -> Option<XPath> {
        // SAFETY: a live document.
        NonNull::new(unsafe { sys::xmlXPathNewContext(doc.raw().cast()) }).map(XPath)
    }

    /// `php:functionString`, `php:function` and a name
    /// `registerPhpFunctionNS` registered, as a library over libxml2
    /// registers them on its own evaluator, ext/xsl's libxslt under
    /// `http://php.net/xsl` (`xsl_ext_function_string_php`,
    /// `xsl_ext_function_object_php` and `xsl_ext_function_trampoline`),
    /// each served through the handler [`XPath::lend`] lent.
    pub const PHP_STRING: Function = foreign_string;
    pub const PHP_OBJECT: Function = foreign_object;
    pub const CUSTOM: Function = foreign_custom;

    /// `f`, a call into a library over libxml2 that evaluates `XPath` in
    /// `ctx`, an evaluator of its own, with the request lent to the functions
    /// its expressions call, served through `handler`, and to libxml's
    /// loads and saves, as [`Raised::capture_io`] lends it: the answer,
    /// what libxml raised, and the first throwable a program's code raised
    /// from inside the call, after which libxml's errors reach the list
    /// alone.
    ///
    /// # Safety
    /// `ctx` is a live `xmlXPathContextPtr` that nothing but `f` uses
    /// meanwhile and whose `userData` its owner leaves alone.
    pub unsafe fn lend<R>(
        ctx: *mut c_void,
        req: &mut Request,
        handler: &mut Handler<'_>,
        f: impl FnOnce() -> R,
    ) -> (R, Vec<Raised>, Option<Thrown>) {
        let ctx = ctx.cast::<sys::xmlXPathContext>();
        let mut io = Io {
            req: ptr::from_mut(req),
            thrown: None,
        };
        let mut session = Session {
            req: ptr::from_mut(req),
            handler,
            thrown: None,
            io: &raw mut io,
        };
        // SAFETY: the caller's contract; the session outlives `f`, and the
        // field is put back before it goes.
        let outer =
            unsafe { std::mem::replace(&mut (*ctx).userData, ptr::from_mut(&mut session).cast()) };
        let (answer, raised) = {
            let _creators = loader::Creators::set();
            Call::run(None, &raw mut io, f)
        };
        // SAFETY: as above.
        unsafe { (*ctx).userData = outer };
        let thrown = session.thrown.take();
        (answer, raised, io.thrown.or(thrown))
    }

    /// `ctx->doc`.
    pub fn doc(&self) -> Option<Node> {
        // SAFETY: the live context this owns.
        Node::from_raw(unsafe { (*self.0.as_ptr()).doc }.cast())
    }

    /// `xmlXPathRegisterNs`: whether it took.
    pub fn register_ns(&self, prefix: &[u8], uri: &[u8]) -> bool {
        let p = cstr(prefix);
        let u = cstr(uri);
        // SAFETY: a live context and NUL-terminated strings.
        unsafe { sys::xmlXPathRegisterNs(self.0.as_ptr(), p.as_ptr(), u.as_ptr()) == 0 }
    }

    /// `xmlXPathRegisterFuncNS(ctx, name, ns, dom_xpath_ext_function_trampoline)`.
    pub fn register_custom(&self, ns: &[u8], name: &[u8]) {
        let n = cstr(name);
        let u = cstr(ns);
        // SAFETY: a live context and NUL-terminated strings, copied by
        // libxml's hash.
        unsafe {
            sys::xmlXPathRegisterFuncNS(self.0.as_ptr(), n.as_ptr(), u.as_ptr(), Some(custom))
        };
    }

    /// `xmlXPathEvalExpression(expr, ctx)` with `ctx->node` at `node` and,
    /// where `namespaces` asks, the node's in-scope declarations
    /// (`xmlGetNsList`), the functions served through `handler`; the
    /// answer, what libxml raised, and what a function threw.
    pub fn eval(
        &self,
        req: &mut Request,
        expr: &[u8],
        node: Option<Node>,
        namespaces: bool,
        handler: &mut Handler<'_>,
    ) -> (Option<Answer>, Vec<Raised>, Option<Thrown>) {
        let scope = if namespaces {
            Scope::InScope
        } else {
            Scope::None
        };
        self.eval_scoped(req, expr, node, scope, handler)
    }

    /// [`XPath::eval`] with the declarations registered for the
    /// evaluation given as a [`Scope`], as the modern classes hand php's
    /// `php_dom_get_in_scope_ns` list over.
    pub fn eval_scoped(
        &self,
        req: &mut Request,
        expr: &[u8],
        node: Option<Node>,
        scope: Scope<'_>,
        handler: &mut Handler<'_>,
    ) -> (Option<Answer>, Vec<Raised>, Option<Thrown>) {
        let (object, raised, thrown) = self.eval_with(req, expr, node, scope, handler);
        let answer = object.map(|o| o.answer());
        (answer, raised, thrown)
    }

    /// [`XPath::eval`], the answer kept as libxml made it.
    pub fn eval_raw(
        &self,
        req: &mut Request,
        expr: &[u8],
        node: Option<Node>,
        namespaces: bool,
        handler: &mut Handler<'_>,
    ) -> (Option<Object>, Vec<Raised>, Option<Thrown>) {
        let scope = if namespaces {
            Scope::InScope
        } else {
            Scope::None
        };
        self.eval_with(req, expr, node, scope, handler)
    }

    fn eval_with(
        &self,
        req: &mut Request,
        expr: &[u8],
        node: Option<Node>,
        scope: Scope<'_>,
        handler: &mut Handler<'_>,
    ) -> (Option<Object>, Vec<Raised>, Option<Thrown>) {
        evaluate(self.0, req, expr, node, scope, handler)
    }

    /// [`XPath::eval_scoped`] on the evaluator `id` names, which the
    /// request keeps ([`Memory::adopt`]) for an object PHP code reaches
    /// during the evaluation, held for its span ([`Memory::holding`],
    /// ADR-0013): a function the expression calls may evaluate again,
    /// register a namespace or a function on it, or replace it, as
    /// `DOMXPath::__construct` does, and the evaluation runs to its end
    /// on the one it started with. `None` where its owner gave it up.
    pub fn eval_held(
        req: &mut Request,
        id: ForeignId<XPath>,
        expr: &[u8],
        node: Option<Node>,
        scope: Scope<'_>,
        handler: &mut Handler<'_>,
    ) -> Option<(Option<Answer>, Vec<Raised>, Option<Thrown>)> {
        req.memory_mut()
            .holding(id, |req, held| {
                let ctx = held.get(req)?.0;
                let (object, raised, thrown) = evaluate(ctx, req, expr, node, scope, handler);
                Some((object.map(|o| o.answer()), raised, thrown))
            })
            .flatten()
    }
}

/// `xmlXPathEvalExpression(expr, ctx)` as [`XPath::eval_scoped`] makes it,
/// on an evaluator open for the call: `node`, the declarations and the
/// session set for the evaluation and the fields put back after, the
/// session to the one an evaluation in progress on the same evaluator had
/// set, so an evaluation a function makes nests in it.
fn evaluate(
    ctx: NonNull<sys::xmlXPathContext>,
    req: &mut Request,
    expr: &[u8],
    node: Option<Node>,
    scope: Scope<'_>,
    handler: &mut Handler<'_>,
) -> (Option<Object>, Vec<Raised>, Option<Thrown>) {
    let ctx = ctx.as_ptr();
    let e = cstr(expr);
    let mut session = Session {
        req: ptr::from_mut(req),
        handler,
        thrown: None,
        io: ptr::null_mut(),
    };
    // SAFETY: a context open for the call; the fields php sets for one
    // evaluation.
    let c = unsafe { &mut *ctx };
    // What an evaluation in progress on the same evaluator set, put back
    // after, so a function's evaluation leaves the outer one the node it
    // evaluates at and its declarations (php 8.5.12).
    let (old_node, old_namespaces, old_ns_nr) = (c.node, c.namespaces, c.nsNr);
    c.node = node.map_or(ptr::null_mut(), Node::raw);
    let list = if let (Scope::InScope, Some(n)) = (&scope, node) {
        let doc = n.doc().map_or(ptr::null_mut(), |d| d.raw().cast());
        // SAFETY: a live node and its document.
        unsafe { sys::xmlGetNsList(doc, n.raw()) }
    } else {
        ptr::null_mut()
    };
    let mut given: Vec<*mut sys::xmlNs> = match scope {
        Scope::List(list) if node.is_some() => list.iter().map(|ns| ns.raw()).collect(),
        _ => Vec::new(),
    };
    if !given.is_empty() {
        c.namespaces = given.as_mut_ptr();
        c.nsNr = c_int::try_from(given.len()).unwrap_or(0);
    }
    if !list.is_null() {
        let mut count = 0;
        loop {
            // SAFETY: within the NULL-terminated array libxml allocated,
            // up to and including its terminator.
            let at = unsafe { list.add(count) };
            // SAFETY: as above.
            if unsafe { *at }.is_null() {
                break;
            }
            count += 1;
        }
        c.namespaces = list;
        c.nsNr = c_int::try_from(count).unwrap_or(0);
    }
    let outer = std::mem::replace(&mut c.userData, ptr::from_mut(&mut session).cast());
    let (object, raised) = Raised::capture(|| {
        // SAFETY: a live context and a NUL-terminated expression; the
        // session outlives the call.
        unsafe { sys::xmlXPathEvalExpression(e.as_ptr(), ctx) }
    });
    // SAFETY: as above, the fields reset.
    let c = unsafe { &mut *ctx };
    c.userData = outer;
    c.node = old_node;
    c.namespaces = old_namespaces;
    c.nsNr = old_ns_nr;
    if !list.is_null() {
        // SAFETY: the array `xmlGetNsList` allocated, freed once.
        unsafe { Alloc::free(list.cast()) };
    }
    drop(given);
    (
        NonNull::new(object).map(Object),
        raised,
        session.thrown.take(),
    )
}

/// The declarations one evaluation registers: none, the ones in scope at
/// the node it starts from as libxml lists them, or a list the caller
/// made.
#[derive(Clone, Copy)]
pub enum Scope<'a> {
    None,
    InScope,
    List(&'a [Ns]),
}

/// An `xmlXPathObjectPtr`, owned.
pub struct Object(NonNull<sys::xmlXPathObject>);

impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: the object this owns, freed once.
        unsafe { sys::xmlXPathFreeObject(self.0.as_ptr()) };
    }
}

impl Object {
    fn answer(&self) -> Answer {
        // SAFETY: the live object this owns.
        let o = unsafe { &*self.0.as_ptr() };
        match o.type_ {
            sys::XPATH_NODESET => Answer::NodeSet(items(o.nodesetval)),
            sys::XPATH_BOOLEAN => Answer::Bool(o.boolval != 0),
            sys::XPATH_NUMBER => Answer::Number(o.floatval),
            sys::XPATH_STRING => {
                // SAFETY: the object's string, or null.
                Answer::Str(unsafe { Alloc::bytes(o.stringval) }.to_vec())
            }
            _ => Answer::Other,
        }
    }

    /// Whether the answer is a node set, which C14N reads.
    #[must_use]
    pub fn is_node_set(&self) -> bool {
        self.node_set().is_some()
    }

    /// The node set, for C14N; null for another type.
    pub(crate) fn node_set(&self) -> Option<*mut sys::xmlNodeSet> {
        // SAFETY: the live object this owns.
        let o = unsafe { &*self.0.as_ptr() };
        (o.type_ == sys::XPATH_NODESET).then_some(o.nodesetval)
    }
}

impl XPath {
    /// `xmlXPathNewContext` for one C14N query at `node`, with `namespaces`
    /// registered, the query evaluated and its answer kept.
    pub fn c14n_query(
        doc: Node,
        node: Node,
        query: &[u8],
        namespaces: &[(Vec<u8>, Vec<u8>)],
    ) -> (Option<(XPath, Object)>, Vec<Raised>) {
        // SAFETY: a live document.
        let Some(ctx) = NonNull::new(unsafe { sys::xmlXPathNewContext(doc.raw().cast()) }) else {
            return (None, Vec::new());
        };
        let context = XPath(ctx);
        for (prefix, uri) in namespaces {
            context.register_ns(prefix, uri);
        }
        let q = cstr(query);
        // SAFETY: the live context.
        unsafe { (*ctx.as_ptr()).node = node.raw() };
        let (object, raised) = Raised::capture(|| {
            // SAFETY: a live context and a NUL-terminated query.
            unsafe { sys::xmlXPathEvalExpression(q.as_ptr(), ctx.as_ptr()) }
        });
        // SAFETY: as above.
        unsafe { (*ctx.as_ptr()).node = ptr::null_mut() };
        (NonNull::new(object).map(|o| (context, Object(o))), raised)
    }
}

/// Pop `n` arguments off the stack, the last first, as
/// `php_dom_xpath_callback_fetch_args` does, handing them back in call
/// order.
fn pop_args(ctxt: *mut sys::xmlXPathParserContext, n: usize, strings: bool) -> Vec<Operand> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        // SAFETY: a live parser context with at least `n` values stacked.
        let obj = unsafe { sys::xmlXPathValuePop(ctxt) };
        if obj.is_null() {
            out.push(Operand::Str(Vec::new()));
            continue;
        }
        // SAFETY: the value just popped, this function's until freed.
        let o = unsafe { &*obj };
        let arg = match o.type_ {
            sys::XPATH_STRING => {
                // SAFETY: the object's string, or null.
                Operand::Str(unsafe { Alloc::bytes(o.stringval) }.to_vec())
            }
            sys::XPATH_BOOLEAN => Operand::Bool(o.boolval != 0),
            sys::XPATH_NUMBER => Operand::Number(o.floatval),
            sys::XPATH_NODESET if !strings => Operand::Nodes(items(o.nodesetval)),
            _ => {
                // SAFETY: a live object cast to its string value.
                let cast = unsafe { sys::xmlXPathCastToString(obj) };
                // SAFETY: an allocation handed over, freed once.
                Operand::Str(unsafe { Alloc::take(cast) }.unwrap_or_default())
            }
        };
        // SAFETY: the popped value, freed once.
        unsafe { sys::xmlXPathFreeObject(obj) };
        out.push(arg);
    }
    out.reverse();
    out
}

fn push(ctxt: *mut sys::xmlXPathParserContext, value: Returned) {
    let obj = match value {
        Returned::NodeSet(node) => {
            let raw = node.map_or(std::ptr::null_mut(), Node::raw);
            // SAFETY: a live node of the document being queried, or null,
            // for which libxml makes the empty set.
            unsafe { sys::xmlXPathNewNodeSet(raw) }
        }
        // SAFETY: no pointer argument.
        Returned::Bool(b) => unsafe { sys::xmlXPathNewBoolean(c_int::from(b)) },
        Returned::Str(s) => {
            let c = cstr(&s);
            // SAFETY: a NUL-terminated string, copied.
            unsafe { sys::xmlXPathNewString(c.as_ptr()) }
        }
        // SAFETY: as above.
        Returned::Sentinel => unsafe { sys::xmlXPathNewString(c"".as_ptr().cast()) },
    };
    // SAFETY: a live parser context and a fresh value it takes.
    unsafe { sys::xmlXPathValuePush(ctxt, obj) };
}

/// The session of the evaluation in progress, from the evaluator's
/// `userData`.
fn session<'a>(ctxt: *mut sys::xmlXPathParserContext) -> Option<&'a mut Session<'a>> {
    if ctxt.is_null() {
        return None;
    }
    // SAFETY: a live parser context whose context is the one evaluating.
    let context = unsafe { (*ctxt).context };
    if context.is_null() {
        return None;
    }
    // SAFETY: as above.
    let data = unsafe { (*context).userData };
    // SAFETY: `eval_raw` set it to its own session, live for the call, and
    // nothing else reaches it meanwhile.
    unsafe { data.cast::<Session<'a>>().as_mut() }
}

fn serve(ctxt: *mut sys::xmlXPathParserContext, nargs: c_int, called: Called, strings: bool) {
    let n = usize::try_from(nargs).unwrap_or(0);
    let Some(session) = session(ctxt) else {
        drop(pop_args(ctxt, n, strings));
        return;
    };
    let args = pop_args(ctxt, n, strings);
    // SAFETY: set by `eval_raw` from the `&mut Request` it was given,
    // which that call does not touch until libxml returns, so this is the
    // one live borrow of it.
    let Some(req) = (unsafe { session.req.as_mut() }) else {
        push(ctxt, Returned::Sentinel);
        return;
    };
    match (session.handler)(req, called, args) {
        Ok(returned) => push(ctxt, returned),
        Err(thrown) => {
            // SAFETY: null, or the record `XPath::lend` keeps live for the
            // call that reached here.
            let slot = match unsafe { session.io.as_mut() } {
                Some(io) => &mut io.thrown,
                None => &mut session.thrown,
            };
            if slot.is_none() {
                *slot = Some(thrown);
            }
            push(ctxt, Returned::Sentinel);
        }
    }
}

unsafe extern "C" fn php_string(ctxt: *mut sys::xmlXPathParserContext, nargs: c_int) {
    serve(ctxt, nargs, Called::Php { strings: true }, true);
}

unsafe extern "C" fn php_object(ctxt: *mut sys::xmlXPathParserContext, nargs: c_int) {
    serve(ctxt, nargs, Called::Php { strings: false }, false);
}

unsafe extern "C" fn custom(ctxt: *mut sys::xmlXPathParserContext, nargs: c_int) {
    if ctxt.is_null() {
        return;
    }
    // SAFETY: a live parser context.
    let context = unsafe { (*ctxt).context };
    if context.is_null() {
        return;
    }
    // SAFETY: the live context evaluating, which names the function being
    // called.
    let c = unsafe { &*context };
    // SAFETY: NUL-terminated names of the context, or null.
    let name = unsafe { Alloc::bytes(c.function) }.to_vec();
    // SAFETY: as above.
    let ns = unsafe { Alloc::bytes(c.functionURI) }.to_vec();
    serve(ctxt, nargs, Called::Custom { ns, name }, false);
}

unsafe extern "C" fn foreign_string(ctxt: *mut c_void, nargs: c_int) {
    serve(ctxt.cast(), nargs, Called::Php { strings: true }, true);
}

unsafe extern "C" fn foreign_object(ctxt: *mut c_void, nargs: c_int) {
    serve(ctxt.cast(), nargs, Called::Php { strings: false }, false);
}

unsafe extern "C" fn foreign_custom(ctxt: *mut c_void, nargs: c_int) {
    // SAFETY: libxml's call of a function registered as `custom` is.
    unsafe { custom(ctxt.cast(), nargs) };
}
