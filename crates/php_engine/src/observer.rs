//! Seam 9, the observer (design §4.3 row 9 and "What a caller of seam 9
//! must know", revision 79): php's observer interface for user calls and
//! natives, `Z/Zend/zend_observer.c`.
//!
//! An extension declares an observer on its [`Extension`] value
//! ([`ExtensionBuilder::observer`]), as php's `MINIT` calls
//! `zend_observer_fcall_register` (`Z/Zend/zend_observer.c:44`); the bundle's observers are listed in
//! the order the extensions start ([`crate::supplied`]). The engine fixes
//! one word at its build, `Shared::observers`, how many were declared, and
//! the two homes test it and nothing else while it is zero:
//!
//! - [`call`], where a frame is pushed and before its `RECV` prologue
//!   runs, as `ZEND_OBSERVER_FCALL_BEGIN` stands in `ZEND_DO_UCALL`
//!   (`Z/Zend/zend_vm_def.h:4189`), in `zend_call_function`
//!   (`Z/Zend/zend_execute_API.c:1009`), in `ZEND_INCLUDE_OR_EVAL`
//!   (`Z/Zend/zend_vm_def.h:6603`) and in `zend_execute`
//!   (`Z/Zend/zend_vm_execute.skl:79`);
//! - [`returned`], where a frame is left by its `RETURN` with what it
//!   returned (`ZEND_OBSERVER_FCALL_END`, `Z/Zend/zend_vm_def.h:4581`),
//!   and by an unwind with nothing, in the uncaught arm of
//!   `zend_dispatch_try_catch_finally_helper` (`:8157-8161`), which is
//!   `handlers::unwind_from`;
//! - [`native_call`] and [`native_returned`], around a native's entry in
//!   `Request::serve_native`, which every call of a native reaches: the
//!   call opcodes, as `ZEND_DO_ICALL` begins the call before
//!   `fbc->internal_function.handler` and ends it after
//!   (`Z/Zend/zend_vm_def.h:4125`, `:4140`), a native a native calls, as
//!   `zend_call_function` does (`Z/Zend/zend_execute_API.c:1020`,
//!   `:1039`), and a frameless call. php's frameless handler tests the
//!   callee's slot and, unless it reads none-observed, falls back to a
//!   full call, `zend_frameless_observed_call`
//!   (`Z/Zend/zend_vm_def.h:9775`, `Z/Zend/zend_execute.c:1577-1613`),
//!   as design §4.3 says a caller of seam 9 must know; here the
//!   frameless handlers lay a frameless call down as a full call already,
//!   so it is observed as one with no test of its own.
//!
//! Both homes stand where no tier's type parameter reaches, so a
//! stepping host's [`crate::tier::step`] and [`crate::tier::unwind`]
//! raise them as the loop does (#1813).
//!
//! That is the plain test #1808 measured, which #1954 ruled stands: the test
//! lives in these functions alone, so php's observer-specialised call
//! and return handlers, the escape design §4.3 names, would change no
//! item of the seam. Past the word, out of line and cold, a frame's
//! function is resolved on its first call into a slot in the runtime
//! cache of the op array it serves ([`Slot`]), never indexed by the
//! function's identity number, which is sparse; a native's slot stands in
//! the request's table of natives' slots at the native's place among the
//! natives, php's internal runtime cache
//! (`zend_observer_fcall_internal_function_extension`,
//! `zend_reset_internal_run_time_cache` at each request's start,
//! `Z/Zend/zend.c:1329`). Every observer is asked in
//! declaration order, each answering a begin handler, an end handler,
//! both or neither ([`ObserverHandlers`]), php's
//! `zend_observer_fcall_install` (`Z/Zend/zend_observer.c:116-156`). There is no limit on how many. Begin
//! handlers are called in declaration order and end handlers in reverse. A
//! frame with an end handler is kept on the request's chain of open
//! frames (`Request::open_observed`, php's `EG(current_observed_frame)`), and is ended
//! only while it is that chain's innermost, so each is ended once: by its
//! return, by the unwind that leaves it, or, at a fatal, by
//! [`end_all`], which ends every one still open from the innermost out
//! with no value, after the fatal is rendered, as php's
//! `zend_observer_fcall_end_all` (`Z/Zend/zend_observer.c:319-329`) does at
//! `php_request_shutdown` (`Z/main/main.c:1970-1972`).
//!
//! A generator's frame is not begun at its call, as php's
//! `zend_observer_fcall_begin` skips `ZEND_ACC_GENERATOR`: it is begun
//! at each resumption, [`generator_resumed`]
//! (`zend_observer_generator_resume`, `Z/Zend/zend_generators.c:847`),
//! and ended at each suspension with the value it yielded,
//! [`generator_yielded`] (`:851`), and at its `return` with what it
//! returned (`ZEND_GENERATOR_RETURN`, `Z/Zend/zend_vm_def.h:4766`), so
//! each resumption is a begin and an end of its own.
//!
//! A fiber runs on a stack of its own, so the chain of open frames is
//! one per stack: it rides with the execution state a switch swaps
//! ([`crate::fiber`]), as php saves `EG(current_observed_frame)` into
//! the `zend_fiber_context` it leaves and restores the one it enters
//! (`zend_observer_fiber_switch_notify`, `Z/Zend/zend_observer.c:409-425`),
//! and a frame of one stack is never ended in place of another's. The
//! three fiber notices, creation, switch and destruction
//! ([`fiber_initialized`], [`fiber_switched`], [`fiber_destroyed`]), are
//! called in declaration order whether or not any call observer is
//! declared, as php's lists are walked; a switch away from a fiber that
//! is dead ends what its chain still holds first.
//!
//! What is not here yet, each built on the same slot and chain:
//! the other notices and adding and removing handlers while a request
//! is served (#1812). A handler reads the frame it is handed through
//! [`Observed::frame`], and the user frame below it through
//! [`Observed::caller`], and keeps one as a
//! [`FrameId`](crate::api::request::FrameId) (#1827).
//!
//! [`Extension`]: crate::api::abi::Extension
//! [`ExtensionBuilder::observer`]: crate::api::abi::ExtensionBuilder::observer

use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use php_heap::{Fatal, FrameRef};

use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::request::{FrameView, Request, TimeLimitSuspended};
use crate::api::values::{ClassId, PhpObject, Value};

/// What an observer observes, php's `zend_execute_data` of a call: a
/// frame the loop serves, a function's or an included, evaluated or main
/// file's, or a native's call. The open chain holds these, and a
/// handler's [`Observed`] reads the one it is handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Observable {
    /// A frame the loop serves, whose slot stands in its op array's
    /// runtime cache.
    Code(FrameRef),
    /// A native's call, served while it stands on the call stack.
    Native(NativeCall),
}

/// One native's call begun with an end handler: the native, and the
/// number the request gave the call as it began it, which tells it
/// apart from every other call of the same native still open, as
/// php's `execute_data == EG(current_observed_frame)` tells frames
/// apart by their address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NativeCall {
    pub(crate) function: Function,
    number: u64,
}

/// What a native's end handlers are handed where a throwable stands,
/// which php decides by the path that called it: the call opcodes hand
/// `NULL` while `EG(exception)` holds one, whether the native threw it or
/// a handler it called did, `EG(exception) ? NULL : ret`
/// (`Z/Zend/zend_vm_def.h:4140`), while `zend_call_function`
/// (`Z/Zend/zend_execute_API.c:1039`) and an observed frameless call
/// (`Z/Zend/zend_execute.c:1597`) hand the return value whatever stands,
/// which holds the `null` it was laid down with where the native threw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnThrow {
    /// No value, as a call opcode hands.
    Nothing,
    /// The return value, `null` where the native threw.
    ReturnValue,
}

/// `zend_observer_fcall_init` (`Z/Zend/zend_observer.h:70`): asked once
/// of each function, user or native, on its first call in the request,
/// and of each included, evaluated or main file's
/// frame on its activation, what handlers observe it; the default,
/// neither, declines it, as php's answers `{NULL, NULL}`.
pub type ObserverInit = fn(&mut Observed<'_>) -> ObserverHandlers;

/// `zend_observer_fcall_begin_handler`: a frame begun, before its
/// parameters are received.
pub type BeginHandler = fn(&mut Observed<'_>);

/// `zend_observer_fcall_end_handler`: a frame ended, with what it
/// returned, or `None` where an unwind or a fatal left it or a native a
/// call opcode served left a throwable standing ([`OnThrow`]).
pub type EndHandler = fn(&mut Observed<'_>, Option<&Value>);

/// What an [`ObserverInit`] answers of one function,
/// `zend_observer_fcall_handlers` (`Z/Zend/zend_observer.h:64-67`): either handler, both, or neither.
#[derive(Debug, Clone, Copy, Default)]
pub struct ObserverHandlers {
    pub begin: Option<BeginHandler>,
    pub end: Option<EndHandler>,
}

/// The frame an observer is handed, borrowed for the handler's span
/// alone (design §4.3 row 9): the request through `Deref`, and reads of
/// the one frame observed, a function's, a file's or a native's call,
/// which no borrow outlives: [`Observed::frame`] reads a frame the loop
/// serves, [`Observed::caller`] the user frame below. A frame kept past
/// the handler is its [`FrameId`](crate::api::request::FrameId),
/// checked each time it is read again.
pub struct Observed<'a> {
    req: &'a mut Request,
    at: Observable,
}

impl Deref for Observed<'_> {
    type Target = Request;
    fn deref(&self) -> &Request {
        self.req
    }
}

impl DerefMut for Observed<'_> {
    fn deref_mut(&mut self) -> &mut Request {
        self.req
    }
}

impl AsMut<Request> for Observed<'_> {
    /// The request whose time limit a [`TimeLimitSuspended`] over the
    /// frame suspends.
    fn as_mut(&mut self) -> &mut Request {
        self.req
    }
}

impl Observed<'_> {
    /// The frame observed, `execute_data`, read for as long as the
    /// handler does not write through the request: what it serves, where
    /// it stands, the frames below it, and its identity to keep. `None`
    /// for a native's call, which serves no op array and stands in no
    /// frame of the loop's: its function is [`Observed::function`], and
    /// the frame it was called from [`Observed::caller`].
    #[must_use]
    pub fn frame(&self) -> Option<FrameView<'_>> {
        match self.at {
            Observable::Code(frame) => Some(self.req.frame_view(frame)),
            Observable::Native(_) => None,
        }
    }

    /// `EX(prev_execute_data)` as far as user code goes: the user frame
    /// below the one observed ([`FrameView::caller`]), and for a native's
    /// call the user frame it stands on, the innermost on the running
    /// stack, past any native between them.
    #[must_use]
    pub fn caller(&self) -> Option<FrameView<'_>> {
        match self.at {
            Observable::Code(frame) => self.req.frame_view(frame).caller(),
            Observable::Native(_) => self.req.innermost_frame_view(),
        }
    }

    /// `EX(func)` where the frame serves a function, a method or a
    /// native; `None` for an included, evaluated or main file's op
    /// array, which php names by its `filename` alone.
    #[must_use]
    pub fn function(&self) -> Option<Function> {
        match self.at {
            Observable::Code(frame) => self.req.frame_view(frame).function(),
            Observable::Native(call) => Some(call.function),
        }
    }

    /// `EX(func)->common.scope`: the class a method, native or not, is
    /// declared in, the one a closure is bound to, and the includer's
    /// for a file's frame.
    #[must_use]
    pub fn function_scope(&self) -> Option<ClassId> {
        match self.at {
            Observable::Code(frame) => self.req.frame_view(frame).function_scope(),
            Observable::Native(call) => self.req.method_class(call.function),
        }
    }

    /// [`Request::suspend_time_limit`] over the observed frame: the
    /// guard derefs to this, so the frame's reads stay the handler's while
    /// it blocks, a debugger's answers to its client among them.
    pub fn suspend_time_limit(&mut self) -> TimeLimitSuspended<'_, Self> {
        TimeLimitSuspended::over(self)
    }

    /// `EX(func)->op_array.filename`: the file the op array was compiled
    /// from, `… : eval()'d code` for an evaluated string's; empty for a
    /// native, which no file holds.
    #[must_use]
    pub fn filename(&self) -> &[u8] {
        match self.at {
            Observable::Code(frame) => self.req.frame_view(frame).filename(),
            Observable::Native(_) => &[],
        }
    }
}

/// One op array's or one native's observer slot, php's
/// `ZEND_OBSERVER_DATA`: unresolved until the first call, then the
/// handlers every observer answered, or declined where none answered
/// any (`ZEND_OBSERVER_NONE_OBSERVED`). Held where php's runtime cache
/// is, so it lives as long as php's: a function's for the request, a
/// file's for its activation, a native's for the request.
#[derive(Debug, Clone, Default)]
pub(crate) enum Slot {
    #[default]
    Unresolved,
    Declined,
    Installed(Arc<Installed>),
}

/// The handlers a slot resolved to: begin in declaration order, end in
/// reverse, as `zend_observer_fcall_install` lays them out.
#[derive(Debug)]
pub(crate) struct Installed {
    begin: Box<[BeginHandler]>,
    end: Box<[EndHandler]>,
}

/// The call home: `frame` pushed, its prologue not yet served. One load and
/// one compare while no observer is declared.
#[inline(always)] // design §4.3 seam 9: the home pays the load and the branch, never a call
#[allow(clippy::inline_always)]
pub(crate) fn call(req: &mut Request, frame: FrameRef) {
    if req.shared.observers != 0 {
        begin(req, Observable::Code(frame));
    }
}

/// The return home: `frame` about to be left, by its `RETURN` with what
/// it returned, or by an unwind with nothing. As [`call`] while no
/// observer is declared.
#[inline(always)] // as `call`
#[allow(clippy::inline_always)]
pub(crate) fn returned(req: &mut Request, frame: FrameRef, value: Option<&Value>) {
    if req.shared.observers != 0 {
        end(req, Observable::Code(frame), value);
    }
}

/// The call home of a native: `function`'s call on top of the call stack
/// and about to be served, `ZEND_OBSERVER_FCALL_BEGIN(call)` before
/// `fbc->internal_function.handler` (`Z/Zend/zend_vm_def.h:4125`). The
/// call begun with an end handler is what [`native_returned`] is handed;
/// none where no observer is declared, after one load and one compare,
/// or where nothing observes the call's end.
#[inline(always)] // as `call`
#[allow(clippy::inline_always)]
pub(crate) fn native_call(req: &mut Request, function: Function) -> Option<NativeCall> {
    if req.shared.observers != 0 {
        begin_native(req, function)
    } else {
        None
    }
}

/// The return home of a native: the call [`native_call`] began with an
/// end handler ended, `served` being what the entry answered and
/// `deferred` the throwable a stream raised while it ran, handed what
/// the native returned, or, where a throwable stands, what `on_throw`
/// says the path that called it hands (`ZEND_OBSERVER_FCALL_END`,
/// `Z/Zend/zend_vm_def.h:4140`). One branch on what [`native_call`]
/// answered, and no load.
#[inline(always)] // as `call`
#[allow(clippy::inline_always)]
pub(crate) fn native_returned(
    req: &mut Request,
    begun: Option<NativeCall>,
    served: &Result<Value, Thrown>,
    deferred: Option<&Thrown>,
    on_throw: OnThrow,
) {
    if let Some(begun) = begun {
        end_native(req, begun, served, deferred, on_throw);
    }
}

/// `zend_observer_fcall_begin` (`Z/Zend/zend_observer.c:288-294`): the
/// frame begun ([`begin_prechecked`]) unless it is a generator's, which
/// is begun when it is resumed ([`generator_resumed`]) and not at its
/// call.
#[cold]
#[inline(never)]
fn begin(req: &mut Request, at: Observable) {
    if let Observable::Code(frame) = at {
        let generator = req
            .frame_function(frame)
            .and_then(|function| req.function_decl(function))
            .is_some_and(|decl| decl.is_generator);
        if generator {
            return;
        }
    }
    let _ = begin_prechecked(req, at);
}

/// The resumption home: a generator's `frame` pushed again by
/// `zend_generator_resume` and about to be served, which is begun as a
/// call is, `zend_observer_generator_resume` (`Z/Zend/zend_observer.c:283-286`,
/// called at `Z/Zend/zend_generators.c:847`). As [`call`] while no
/// observer is declared.
#[inline(always)] // as `call`
#[allow(clippy::inline_always)]
pub(crate) fn generator_resumed(req: &mut Request, frame: FrameRef) {
    if req.shared.observers != 0 {
        begin_resumed(req, frame);
    }
}

#[cold]
#[inline(never)]
fn begin_resumed(req: &mut Request, frame: FrameRef) {
    let _ = begin_prechecked(req, Observable::Code(frame));
}

/// The suspension home: a generator's `frame` about to leave the stack
/// at a `YIELD`, or at a `YIELD_FROM` that delegates, ended with the
/// value the generator holds then, php's
/// `zend_observer_fcall_end(generator->execute_data, &generator->value)`
/// once `zend_execute_ex` has returned (`Z/Zend/zend_generators.c:849-852`):
/// what it yielded, or, at a `yield from`, what it yielded last. As
/// [`call`] while no observer is declared.
#[inline(always)] // as `call`
#[allow(clippy::inline_always)]
pub(crate) fn generator_yielded(req: &mut Request, frame: FrameRef, generator: PhpObject) {
    if req.shared.observers != 0 {
        end_yielded(req, frame, generator);
    }
}

#[cold]
#[inline(never)]
fn end_yielded(req: &mut Request, frame: FrameRef, generator: PhpObject) {
    let value = crate::generator::value(req, generator);
    req.addref(&value);
    end(req, Observable::Code(frame), Some(&value));
    req.release(value);
}

/// `zend_observer_fcall_begin_prechecked` (`Z/Zend/zend_observer.c:257-281`)
/// past the generator's test: the slot resolved where it is not yet, the
/// call put on the open chain where an end handler observes it, which is
/// the answer, and the begin handlers called in declaration order.
fn begin_prechecked(req: &mut Request, at: Observable) -> bool {
    let slot = match req.observer_slot(at) {
        Slot::Unresolved => resolve(req, at),
        resolved => resolved,
    };
    let Slot::Installed(handlers) = slot else {
        return false;
    };
    let opened = !handlers.end.is_empty();
    if opened {
        req.open_observed(at);
    }
    for begin in &handlers.begin {
        begin(&mut Observed { req, at });
    }
    opened
}

/// [`begin`] over a native's call: none for a native with no slot, a
/// call's trampoline, which php leaves unobserved
/// (`ZEND_ACC_CALL_VIA_TRAMPOLINE` in
/// `zend_observer_fcall_has_no_observers`,
/// `Z/Zend/zend_observer.h:96-112`); otherwise the call numbered and
/// begun, and answered where it was opened.
#[cold]
#[inline(never)]
fn begin_native(req: &mut Request, function: Function) -> Option<NativeCall> {
    if !req.has_native_slot(function) {
        return None;
    }
    let call = NativeCall {
        function,
        number: req.number_native_call(),
    };
    begin_prechecked(req, Observable::Native(call)).then_some(call)
}

/// `zend_observer_fcall_install` (`Z/Zend/zend_observer.c:116-156`): every observer asked of the frame in
/// declaration order, and what they answered written into its slot.
fn resolve(req: &mut Request, at: Observable) -> Slot {
    let shared = Arc::clone(&req.shared);
    let mut begin = Vec::new();
    let mut end = Vec::new();
    for init in shared.supplied.observers.iter() {
        let handlers = init(&mut Observed { req, at });
        begin.extend(handlers.begin);
        end.extend(handlers.end);
    }
    end.reverse();
    let slot = if begin.is_empty() && end.is_empty() {
        Slot::Declined
    } else {
        Slot::Installed(Arc::new(Installed {
            begin: begin.into_boxed_slice(),
            end: end.into_boxed_slice(),
        }))
    };
    req.set_observer_slot(at, slot.clone());
    slot
}

/// `zend_observer_fcall_end` (`Z/Zend/zend_observer.h:124-128`): where `frame` is the innermost open one,
/// it is taken off the chain and its end handlers called in reverse
/// declaration order; any other frame was not begun with an end
/// handler, or was ended already. The frame leaves the chain before its
/// handlers are called, so a fatal inside one does not end it twice.
#[cold]
#[inline(never)]
fn end(req: &mut Request, at: Observable, value: Option<&Value>) {
    if req.close_observed(at) {
        call_end_handlers(req, at, value);
    }
}

/// [`end`] over a native's call begun with an end handler: ended where
/// it is still the innermost open, which a fatal inside it leaves it
/// not, [`end_all`] having ended it. The throwable the native left, its
/// own or a stream's, stands in the pending slot while the end handlers
/// are called, as it stands in `EG(exception)` at php's
/// `ZEND_OBSERVER_FCALL_END`, so a handler reads it there and a call it
/// makes returns at once ([`Request::while_pending`]).
#[cold]
#[inline(never)]
fn end_native(
    req: &mut Request,
    call: NativeCall,
    served: &Result<Value, Thrown>,
    deferred: Option<&Thrown>,
    on_throw: OnThrow,
) {
    let at = Observable::Native(call);
    let returned = served.as_ref().ok();
    let standing = served.as_ref().err().or(deferred);
    let null = Value::null();
    let value = match on_throw {
        OnThrow::Nothing => returned.filter(|_| standing.is_none() && !req.is_pending()),
        OnThrow::ReturnValue => Some(returned.unwrap_or(&null)),
    };
    let Some(thrown) = standing else {
        end(req, at, value);
        return;
    };
    if req
        .while_pending(thrown, |req| end(req, at, value))
        .is_none()
    {
        end(req, at, value);
    }
}

/// `call_end_observers` (`Z/Zend/zend_observer.c:296-311`): the end
/// handlers of `at`'s slot, called in the order it holds them.
fn call_end_handlers(req: &mut Request, at: Observable, value: Option<&Value>) {
    let Slot::Installed(handlers) = req.observer_slot(at) else {
        return;
    };
    for end in &handlers.end {
        end(&mut Observed { req, at }, value);
    }
}

/// A serving's loop left, by its entry frame or by a fatal the loop
/// raised and nothing has rendered yet: a resumed generator's caller and
/// `php -r`'s pop the frames above the serving's entry before they
/// render it, which would leave [`end_all`] frames that no longer stand.
/// With an observer declared, the fatal is rendered here, where they
/// still do, and the request leaves by `zend_bailout`, so the caller's
/// own rendering finds it rendered; with none, nothing changes.
#[inline]
pub(crate) fn loop_left(req: &mut Request, outcome: &Result<(), Fatal>) {
    if let Err(fatal) = outcome
        && req.shared.observers != 0
    {
        render_where_frames_stand(req, fatal);
    }
}

#[cold]
#[inline(never)]
fn render_where_frames_stand(req: &mut Request, fatal: &Fatal) {
    req.end_in(fatal);
}

/// `zend_observer_fcall_end_all` (`Z/Zend/zend_observer.c:319-329`): at a fatal, every frame still open
/// ended with no value, the innermost first, the chain emptied before
/// the first handler runs. The frames still stand: the request ends
/// them here, where the fatal is raised and after it is
/// rendered, rather than at `php_request_shutdown` as php does, because
/// the serving that catches the fatal pops them on its way out.
pub(crate) fn end_all(req: &mut Request) {
    if req.shared.observers == 0 {
        return;
    }
    for at in req.take_observed() {
        call_end_handlers(req, at, None);
    }
}

/// `zend_fiber_status` (`Z/Zend/zend_fibers.h:33-38`) as a fiber
/// observer reads it on an [`ObservedFiber`]: php's own, where the
/// fiber a switch leaves is `Running`, or `Dead` once it has returned,
/// thrown or ended in a fatal, and the fiber it enters is `Init` on its
/// first switch and `Suspended` after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiberStatus {
    Init,
    Running,
    Suspended,
    Dead,
}

/// A fiber as a fiber observer is handed it, php's `zend_fiber_context`
/// (`Z/Zend/zend_fibers.h:76-100`): a `Fiber` object, the collector's
/// destructor fiber among them, or the request's own stack,
/// `EG(main_fiber_context)`; and its status at the notice. What an
/// extension keeps per fiber, which php keeps in the `reserved` slots,
/// it keys by the object [`ObservedFiber::fiber`] names, its
/// `PhpObject::index`, from the creation notice to the destruction
/// notice, and by none for the request's own.
#[derive(Debug, Clone, Copy)]
pub struct ObservedFiber {
    fiber: Option<PhpObject>,
    status: FiberStatus,
    threw: bool,
    destroyed: bool,
}

impl ObservedFiber {
    pub(crate) fn of(fiber: Option<PhpObject>, status: FiberStatus) -> ObservedFiber {
        ObservedFiber {
            fiber,
            status,
            threw: false,
            destroyed: false,
        }
    }

    /// With whether the `Fiber` threw and whether it is being destroyed.
    pub(crate) fn flagged(self, threw: bool, destroyed: bool) -> ObservedFiber {
        ObservedFiber {
            threw,
            destroyed,
            ..self
        }
    }

    /// The `Fiber` object, `zend_fiber_from_context`; `None` for the
    /// request's own stack, whose `kind` is not `zend_ce_fiber`.
    #[must_use]
    pub fn fiber(&self) -> Option<PhpObject> {
        self.fiber
    }

    /// `context->status` at the notice.
    #[must_use]
    pub fn status(&self) -> FiberStatus {
        self.status
    }

    /// `ZEND_FIBER_FLAG_THREW`: the fiber's callable left a throwable,
    /// or the unwind exit, which is how a `Dead` fiber that threw is told
    /// from one that returned. Never set on the request's own stack.
    #[must_use]
    pub fn threw(&self) -> bool {
        self.threw
    }

    /// `ZEND_FIBER_FLAG_DESTROYED`: the fiber is being torn down, resumed
    /// with the graceful exit because nothing holds it any more, which
    /// is how a switch into a fiber to destroy it is told from one that
    /// resumes it. Never set on the request's own stack.
    #[must_use]
    pub fn destroyed(&self) -> bool {
        self.destroyed
    }
}

/// `zend_observer_fiber_init_handler` and
/// `zend_observer_fiber_destroy_handler`: a fiber created, or given
/// back.
pub type FiberHandler = fn(&mut Request, ObservedFiber);

/// `zend_observer_fiber_switch_handler`: the fiber a switch leaves and
/// the one it enters, called while the one it leaves is still the one
/// served.
pub type FiberSwitchHandler = fn(&mut Request, ObservedFiber, ObservedFiber);

/// `zend_observer_fiber_init_notify` (`Z/Zend/zend_observer.c:396-407`):
/// `fiber`'s stack laid down by its start, before the first switch
/// into it (`zend_fiber_init_context`, `Z/Zend/zend_fibers.c:460`).
pub(crate) fn fiber_initialized(req: &mut Request, fiber: ObservedFiber) {
    if req.shared.supplied.fiber_init.is_empty() {
        return;
    }
    let shared = Arc::clone(&req.shared);
    for handler in shared.supplied.fiber_init.iter() {
        handler(req, fiber);
    }
}

/// `zend_observer_fiber_switch_notify` (`Z/Zend/zend_observer.c:409-425`)
/// up to the chain's swap, which the execution state's swap makes
/// ([`Request::switch_execution`]): where `from` is dead, what its
/// chain still holds ended first, which is nothing for a fiber that
/// returned or threw and every frame left open for one that ended in a
/// fatal; then each switch handler, while `from`'s frames are the ones
/// standing.
pub(crate) fn fiber_switched(req: &mut Request, from: ObservedFiber, to: ObservedFiber) {
    if from.status == FiberStatus::Dead {
        end_all(req);
    }
    if req.shared.supplied.fiber_switch.is_empty() {
        return;
    }
    let shared = Arc::clone(&req.shared);
    for handler in shared.supplied.fiber_switch.iter() {
        handler(req, from, to);
    }
}

/// `zend_observer_fiber_destroy_notify` (`Z/Zend/zend_observer.c:427-436`):
/// a dead `fiber` given back, once the switch out of it has
/// restored its resumer's state (`zend_fiber_destroy_context`,
/// `Z/Zend/zend_fibers.c:465-475`, called at `:549-551`).
pub(crate) fn fiber_destroyed(req: &mut Request, fiber: ObservedFiber) {
    if req.shared.supplied.fiber_destroy.is_empty() {
        return;
    }
    let shared = Arc::clone(&req.shared);
    for handler in shared.supplied.fiber_destroy.iter() {
        handler(req, fiber);
    }
}
