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
//! only while it is that chain's innermost, so each is ended once at
//! most: by its return, by the unwind that leaves it, or by [`end_all`],
//! php's `zend_observer_fcall_end_all` (`Z/Zend/zend_observer.c:319-329`),
//! which ends every one still open from the innermost out with no value
//! at `php_request_shutdown`'s first step (`Z/main/main.c:1970-1972`).
//! A fatal ends nothing where it bails out: its frames stay on the
//! chain, kept as they stood ([`bailed_out`]), for that step to end.
//!
//! The slot is the request's, never the unit's: a handler added to or
//! removed from one function while a request is served
//! ([`Request::add_begin_handler`] and its kin, php's
//! `zend_observer_add_begin_handler`) writes the slot in that request's
//! runtime cache, or its table of natives' slots for a native, so the
//! unit every request shares stays as published
//! (principle 3), and the next request resolves the function afresh.
//! The begin and end walks read the slot for each handler they call, as
//! php's walk the slot in place, so a handler removed mid-walk moves
//! the next into its place ([`Removed`]).
//!
//! Beside the calls stand php's notices, three lists the bundle
//! declares as it declares observers, each told in declaration order
//! and checked by one test of its list where php reads its
//! `zend_observer_*_observed` word: [`error`], every diagnostic before
//! the user's handler and `zend_error_cb`; [`function_declared`] and
//! [`class_linked`], at each declaration a request makes, told for a
//! unit's load as php with and without opcache tells them
//! ([`function_declared_at_load`], [`class_linked_at_load`]).
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
//! A handler reads the frame it is handed through [`Observed::frame`],
//! and the user frame below it through [`Observed::caller`], and keeps
//! one as a [`FrameId`](crate::api::request::FrameId) (#1827).
//!
//! [`Extension`]: crate::api::abi::Extension
//! [`ExtensionBuilder::observer`]: crate::api::abi::ExtensionBuilder::observer

mod opcodes;

use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use php_heap::{Fatal, FrameRef};
use php_unit::Unit;

use crate::api::call::Function;
use crate::api::raising::{Level, Thrown};
use crate::api::request::{ChainFrame, FrameId, FrameView, OpenCall, Request, TimeLimitSuspended};
use crate::api::values::{ClassId, Owner, PhpObject, Value};
use crate::supplied::tell;

pub(crate) use opcodes::{
    Declared, ObservedOpcodes, OpcodeObserver, Telling, Untold, tell_before, told,
};
pub use opcodes::{OpcodeHandler, Opcodes, OpcodesDeclared};

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

/// A frame on the open chain as a fatal left it: what its end
/// handlers read of it once it no longer stands, php's `execute_data`
/// that `zend_bailout` leaves in place on the VM stack, which
/// `zend_observer_fcall_end_all` reads at `php_request_shutdown`
/// (`Z/main/main.c:1970-1972`). Read while the frame stood, as the
/// fatal left by `zend_bailout` ([`bailed_out`]); its identity tells it from a frame pushed
/// later at the same place, so a frame below a `zend_try` that caught
/// the fatal is still ended at its return where it is the innermost
/// open, as php's `execute_data == EG(current_observed_frame)` ends it.
#[derive(Debug)]
pub(crate) struct Left {
    pub(crate) frame: FrameId,
    function: Option<Function>,
    scope: Option<ClassId>,
    filename: Box<[u8]>,
    slot: Slot,
}

impl Left {
    /// `frame`, standing on the running stack, as a fatal leaves it.
    pub(crate) fn of(req: &Request, frame: FrameRef) -> Left {
        let view = req.frame_view(frame);
        Left {
            frame: view.id(),
            function: view.function(),
            scope: view.function_scope(),
            filename: view.filename().into(),
            slot: req.observer_slot(Observable::Code(frame)),
        }
    }
}

/// What one handler is handed: a call that stands, the frame or the
/// native's call it is, or a frame a fatal left ([`Left`]).
#[derive(Debug, Clone, Copy)]
enum Handed<'a> {
    Standing(Observable),
    Left(&'a Left),
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

/// Whether an observer is registered for the engine that starts, asked
/// once its extension's start is over, as php's `MINIT` calls
/// `zend_observer_fcall_register` under a switch of its ini
/// ([`crate::api::abi::ExtensionBuilder::observer_when`]).
pub type ObserverRegistered = fn(&crate::api::abi::Start) -> bool;

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
/// serves, [`Observed::caller`] the user frame below, [`Observed::walk`]
/// every frame of php's chain from it down. A frame kept past
/// the handler is its [`FrameId`](crate::api::request::FrameId),
/// checked each time it is read again.
pub struct Observed<'a> {
    req: &'a mut Request,
    at: Handed<'a>,
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

impl<'a> Observed<'a> {
    /// A call that stands, handed to a handler.
    pub(crate) fn standing(req: &'a mut Request, at: Observable) -> Observed<'a> {
        Observed {
            req,
            at: Handed::Standing(at),
        }
    }
}

impl Observed<'_> {
    /// The frame observed, `execute_data`, read for as long as the
    /// handler does not write through the request: what it serves, where
    /// it stands, the frames below it, and its identity to keep. `None`
    /// for a native's call, which serves no op array and stands in no
    /// frame of the loop's: its function is [`Observed::function`], and
    /// the frame it was called from [`Observed::caller`]; and `None` for
    /// a frame a fatal left, ended at the request's shutdown once it no
    /// longer stands, whose function and file are still read here.
    #[must_use]
    pub fn frame(&self) -> Option<FrameView<'_>> {
        match self.at {
            Handed::Standing(Observable::Code(frame)) => Some(self.req.frame_view(frame)),
            Handed::Standing(Observable::Native(_)) | Handed::Left(_) => None,
        }
    }

    /// `EX(prev_execute_data)` as far as user code goes: the user frame
    /// below the one observed ([`FrameView::caller`]), and for a native's
    /// call the user frame it stands on, the innermost on the running
    /// stack, past any native between them; none below a frame a fatal
    /// left.
    #[must_use]
    pub fn caller(&self) -> Option<FrameView<'_>> {
        match self.at {
            Handed::Standing(Observable::Code(frame)) => self.req.frame_view(frame).caller(),
            Handed::Standing(Observable::Native(_)) => self.req.innermost_frame_view(),
            Handed::Left(_) => None,
        }
    }

    /// php's `prev_execute_data` walk from the frame observed: the frame
    /// or the native's call itself, then every frame below it, natives'
    /// calls and the frames generators and fibers link in among them
    /// ([`FrameView::walk`]); nothing from a frame a fatal left.
    #[must_use]
    pub fn walk(&self) -> std::vec::IntoIter<ChainFrame<'_>> {
        match self.at {
            Handed::Standing(Observable::Code(frame)) => self.req.frame_view(frame).walk(),
            Handed::Standing(Observable::Native(call)) => self.req.native_walk(call.function),
            Handed::Left(_) => Vec::new().into_iter(),
        }
    }

    /// `EX(func)` where the frame serves a function, a method or a
    /// native; `None` for an included, evaluated or main file's op
    /// array, which php names by its `filename` alone.
    #[must_use]
    pub fn function(&self) -> Option<Function> {
        match self.at {
            Handed::Standing(Observable::Code(frame)) => self.req.frame_view(frame).function(),
            Handed::Standing(Observable::Native(call)) => Some(call.function),
            Handed::Left(left) => left.function,
        }
    }

    /// `EX(func)->common.scope`: the class a method, native or not, is
    /// declared in, the one a closure is bound to, and the includer's
    /// for a file's frame.
    #[must_use]
    pub fn function_scope(&self) -> Option<ClassId> {
        match self.at {
            Handed::Standing(Observable::Code(frame)) => {
                self.req.frame_view(frame).function_scope()
            }
            Handed::Standing(Observable::Native(call)) => self.req.method_class(call.function),
            Handed::Left(left) => left.scope,
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
            Handed::Standing(Observable::Code(frame)) => self.req.frame_view(frame).filename(),
            Handed::Standing(Observable::Native(_)) => &[],
            Handed::Left(left) => &left.filename,
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
/// reverse, as `zend_observer_fcall_install` lays them out, and as
/// handlers added and removed while the request is served leave them
/// ([`Request::add_begin_handler`] and its kin). php's slot holds one
/// begin and one end handler per observer declared, so neither list
/// grows past that count.
#[derive(Debug, Clone)]
pub(crate) struct Installed {
    begin: Vec<BeginHandler>,
    end: Vec<EndHandler>,
}

impl Slot {
    /// The begin handler at `at` of the slot as it stands now, read
    /// afresh for each one called, as `zend_observer_fcall_begin_prechecked`
    /// walks the slot itself: a handler that removes itself moves the
    /// next into its place, which the walk then passes over, and php
    /// hands the remover that next handler to call itself
    /// ([`Removed::next`]).
    pub(crate) fn begin_at(&self, at: usize) -> Option<BeginHandler> {
        match self {
            Slot::Installed(handlers) => handlers.begin.get(at).copied(),
            Slot::Unresolved | Slot::Declined => None,
        }
    }

    /// The end handler at `at`, read as [`Slot::begin_at`] reads.
    pub(crate) fn end_at(&self, at: usize) -> Option<EndHandler> {
        match self {
            Slot::Installed(handlers) => handlers.end.get(at).copied(),
            Slot::Unresolved | Slot::Declined => None,
        }
    }

    /// Whether an end handler observes the function now, which is what
    /// puts a frame begun on the open chain.
    fn ends(&self) -> bool {
        matches!(self, Slot::Installed(handlers) if !handlers.end.is_empty())
    }

    /// The handlers to add one to: none while the slot is unresolved,
    /// and an empty pair where every observer declined, which the
    /// addition installs, as `zend_observer_add_begin_handler` writes
    /// over `ZEND_OBSERVER_NONE_OBSERVED`.
    fn handlers_for_adding(&mut self) -> Option<&mut Installed> {
        if matches!(self, Slot::Declined) {
            *self = Slot::Installed(Arc::new(Installed {
                begin: Vec::new(),
                end: Vec::new(),
            }));
        }
        match self {
            Slot::Installed(handlers) => Some(Arc::make_mut(handlers)),
            Slot::Unresolved | Slot::Declined => None,
        }
    }

    /// `zend_observer_add_begin_handler` (`Z/Zend/zend_observer.c:184-199`):
    /// `begin` after the begin handlers the slot holds, where fewer than
    /// `room` stand; php's slot has room for one per observer declared
    /// and holds the caller to that (`ZEND_UNREACHABLE`), where this
    /// answers false.
    pub(crate) fn add_begin(&mut self, begin: BeginHandler, room: usize) -> bool {
        self.add(room, |held| &mut held.begin, |list| list.push(begin))
    }

    /// `zend_observer_add_end_handler` (`Z/Zend/zend_observer.c:216-230`):
    /// `end` before the end handlers the slot holds, so they stay the
    /// reverse of the begin handlers, where fewer than `room` stand.
    pub(crate) fn add_end(&mut self, end: EndHandler, room: usize) -> bool {
        self.add(room, |held| &mut held.end, |list| list.insert(0, end))
    }

    /// The addition both make: `put` over the list `side` names where
    /// fewer than `room` stand in it.
    fn add<H>(
        &mut self,
        room: usize,
        side: fn(&mut Installed) -> &mut Vec<H>,
        put: impl FnOnce(&mut Vec<H>),
    ) -> bool {
        match self.handlers_for_adding().map(side) {
            Some(list) if list.len() < room => {
                put(list);
                true
            }
            _ => {
                self.settle();
                false
            }
        }
    }

    /// `zend_observer_remove_begin_handler` (`Z/Zend/zend_observer.c:201-214`):
    /// `begin` taken out where the slot holds it, the handlers after it
    /// moved up, and the one now in its place handed back.
    pub(crate) fn remove_begin(&mut self, begin: BeginHandler) -> Option<Removed<BeginHandler>> {
        self.remove(
            |held| &mut held.begin,
            |held| std::ptr::fn_addr_eq(held, begin),
        )
    }

    /// `zend_observer_remove_end_handler` (`Z/Zend/zend_observer.c:232-244`):
    /// as [`Slot::remove_begin`], over the end handlers.
    pub(crate) fn remove_end(&mut self, end: EndHandler) -> Option<Removed<EndHandler>> {
        self.remove(|held| &mut held.end, |held| std::ptr::fn_addr_eq(held, end))
    }

    /// The removal both make, over the list `side` names.
    fn remove<H: Copy>(
        &mut self,
        side: fn(&mut Installed) -> &mut Vec<H>,
        is: impl Fn(H) -> bool,
    ) -> Option<Removed<H>> {
        let Slot::Installed(handlers) = self else {
            return None;
        };
        let list = side(Arc::make_mut(handlers));
        let at = list.iter().position(|&held| is(held))?;
        list.remove(at);
        let next = list.get(at).copied();
        self.settle();
        Some(Removed { next })
    }

    /// php's invariant after an addition or a removal: a slot with no
    /// begin and no end handler is `ZEND_OBSERVER_NONE_OBSERVED`, so the
    /// call home passes over it as it passes over a declined function.
    fn settle(&mut self) {
        if matches!(self, Slot::Installed(handlers) if handlers.begin.is_empty() && handlers.end.is_empty())
        {
            *self = Slot::Declined;
        }
    }
}

/// What removing a handler from a function hands back
/// ([`Request::remove_begin_handler`], [`Request::remove_end_handler`]),
/// php's `next` out-parameter: the handler that moved into the removed
/// one's place, if any. A handler that removes itself while the slot is
/// being walked moves that one into its own place, which the walk then
/// passes over, so php's contract is that the remover calls it itself,
/// with the arguments it was handed.
#[derive(Debug, Clone, Copy)]
pub struct Removed<H> {
    pub next: Option<H>,
}

/// Whether any observer is declared, which every home tests first: what
/// an inline path that serves no home asks before it stands in for one.
#[inline(always)] // as `call`
#[allow(clippy::inline_always)]
pub(crate) fn observing(req: &Request) -> bool {
    req.shared.observers != 0
}

/// Whether a frameless call of `function` is served as a full call, a
/// frame pushed for the observers to see: php's `ZEND_OBSERVER_ENABLED
/// && !zend_observer_handler_is_unobserved(...)` before
/// `zend_frameless_observed_call` (`Z/Zend/zend_vm_def.h:9775`), which
/// is every call while the function's slot is unresolved and none once
/// the observers declined it. One load and one compare while no
/// observer is declared.
#[inline(always)] // as `call`
#[allow(clippy::inline_always)]
pub(crate) fn frameless_observed(req: &Request, function: Function) -> bool {
    observing(req) && !req.native_unobserved(function)
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
    let value = Owner::take(req, value);
    end(req, Observable::Code(frame), Some(&*value));
    value.give_up(req);
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
    let opened = slot.ends();
    if opened {
        req.open_observed(at);
    }
    let mut next = 0;
    while let Some(begin) = req.observer_begin_at(at, next) {
        begin(&mut Observed::standing(req, at));
        next += 1;
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
    let mut begin = Vec::new();
    let mut end = Vec::new();
    tell!(req, observers, |req, (init, _)| {
        let handlers = init(&mut Observed::standing(req, at));
        begin.extend(handlers.begin);
        end.extend(handlers.end);
    });
    end.reverse();
    let slot = if begin.is_empty() && end.is_empty() {
        Slot::Declined
    } else {
        Slot::Installed(Arc::new(Installed { begin, end }))
    };
    req.set_observer_slot(at, slot.clone());
    slot
}

/// `zend_observer_fcall_end` (`Z/Zend/zend_observer.h:124-128`): where `frame` is the innermost open one,
/// it is taken off the chain and its end handlers called in reverse
/// declaration order; any other frame was not begun with an end
/// handler, or was ended already, or stands below a frame a caught
/// fatal left, which stays the innermost until the request's shutdown
/// ([`end_all`]). The frame leaves the chain before its handlers are
/// called, so a fatal inside one does not end it twice. Nothing is
/// ended while a fatal leaves by `zend_bailout`, whose jump passes over
/// every `ZEND_OBSERVER_FCALL_END` between the fatal and the `zend_try`
/// that catches it.
#[cold]
#[inline(never)]
fn end(req: &mut Request, at: Observable, value: Option<&Value>) {
    if !req.bailing_out() && req.close_observed(at) {
        call_end_handlers(req, Handed::Standing(at), value);
    }
}

/// [`end`] over a native's call begun with an end handler: ended where
/// it is still the innermost open and no fatal is leaving it. The throwable the native left, its
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
/// handlers of `at`'s slot as it stands when the call is left, which
/// handlers added or removed since its begin have moved, called in the
/// order it holds them and read afresh for each, as [`Slot::begin_at`];
/// a frame a fatal left is ended by the slot it held then.
fn call_end_handlers(req: &mut Request, at: Handed<'_>, value: Option<&Value>) {
    let mut next = 0;
    loop {
        let end = match at {
            Handed::Standing(at) => req.observer_end_at(at, next),
            Handed::Left(left) => left.slot.end_at(next),
        };
        let Some(end) = end else {
            break;
        };
        end(&mut Observed { req, at }, value);
        next += 1;
    }
}

/// A serving's loop left, by its entry frame or by a fatal the loop
/// raised and nothing has rendered yet: a resumed generator's caller and
/// `php -r`'s pop the frames above the serving's entry before they
/// render it, which would leave [`bailed_out`] frames that no longer
/// stand to keep. With an observer declared, the fatal is rendered here,
/// where they still do, and the request leaves by `zend_bailout`, so the
/// caller's own rendering finds it rendered; with none, nothing changes.
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

/// A fatal leaving by `zend_bailout`, rendered, its frames still
/// standing: nothing is ended, as php's jump ends nothing and leaves
/// `EG(current_observed_frame)` where it stood, and every frame the open
/// chain holds is kept as it stands now ([`Left`]), since the frames
/// the fatal leaves are popped on its way out where php's stay on the
/// VM stack. The `zend_try` that catches the fatal decides the rest:
/// at the request's top they are ended at its shutdown ([`end_all`]); a
/// native's, as soap's client stands in one, leaves them the innermost
/// open, so the frames below it are not ended at their return and are
/// ended at the shutdown too, unless the fatal left no frame above the
/// native, which then ends at its return as the frames below it do;
/// a shutdown function's or a destructor's, after the shutdown's first
/// step, leaves them open for good; a fiber's ends them at the switch
/// out of it.
pub(crate) fn bailed_out(req: &mut Request) {
    if req.shared.observers != 0 {
        req.keep_observed();
    }
}

/// `zend_observer_fcall_end_all` (`Z/Zend/zend_observer.c:319-329`):
/// every call still open ended with no value, the innermost first, the
/// chain emptied before the first handler runs. php calls it at the
/// first step of `php_request_shutdown` (`Z/main/main.c:1970-1972`),
/// before the shutdown functions, for the frames a fatal left, and as a
/// fiber that died of one is switched out of
/// (`Z/Zend/zend_observer.c:415`, [`fiber_switched`]). A frame the fatal
/// left that no longer stands is ended as it was kept ([`Left`]).
pub(crate) fn end_all(req: &mut Request) {
    if req.shared.observers == 0 {
        return;
    }
    for opened in req.take_observed() {
        let at = match &opened {
            OpenCall::Call(at) => Handed::Standing(*at),
            OpenCall::Left(left) => Handed::Left(left),
        };
        call_end_handlers(req, at, None);
    }
}

/// `zend_observer_error_cb` (`Z/Zend/zend_observer.h:152`): told of a
/// diagnostic ([`ErrorNotice`]), as php's `zend_error_impl` tells every
/// one registered before it hands the diagnostic to the user's handler
/// or to `zend_error_cb`.
pub(crate) type ErrorObserver = fn(&mut Request, &ErrorNotice<'_>);

/// What an error observer is told of one diagnostic, the four values
/// `zend_observer_error_cb` is handed: the level, the file, the line and
/// the message, and whether php's `type` carried `E_DONT_BAIL`.
#[derive(Debug, Clone, Copy)]
pub struct ErrorNotice<'a> {
    level: Level,
    file: &'a [u8],
    line: u32,
    message: &'a [u8],
    told: Told,
}

/// Which of php's two error notices a diagnostic is told as
/// (`zend_observer_error_notify`'s callers): one `zend_error` raised,
/// or an uncaught throwable `zend_exception_error` renders, told with
/// `E_DONT_BAIL` ([`ErrorNotice::uncaught`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Told {
    Raised,
    Uncaught,
}

impl<'a> ErrorNotice<'a> {
    /// The level the diagnostic was raised at, `type & E_ALL`.
    #[must_use]
    pub fn level(&self) -> Level {
        self.level
    }

    /// The file the diagnostic names, `Unknown` where nothing executes.
    #[must_use]
    pub fn file(&self) -> &'a [u8] {
        self.file
    }

    /// The line the diagnostic names.
    #[must_use]
    pub fn line(&self) -> u32 {
        self.line
    }

    /// The message, its bytes as raised.
    #[must_use]
    pub fn message(&self) -> &'a [u8] {
        self.message
    }

    /// Whether this is an uncaught throwable rendered as the request
    /// ends, `zend_exception_error`'s `E_ERROR`, or a `ParseError`'s or a
    /// `CompileError`'s own level, which php tells with `E_DONT_BAIL` set
    /// in `type` (`Z/Zend/zend_exceptions.c:926`, `:951-953`), so a tool
    /// tells it from a fatal `zend_error` raised; and a throwable that
    /// stood pending as a fatal was raised, which php reports at
    /// `E_WARNING` with `E_DONT_BAIL` before the fatal
    /// (`Z/Zend/zend.c:1514`).
    #[must_use]
    pub fn uncaught(&self) -> bool {
        self.told == Told::Uncaught
    }
}

/// `zend_observer_function_declared_cb` (`Z/Zend/zend_observer.h:131`):
/// told of a function declared, and the lowered name it is declared
/// under.
pub(crate) type FunctionDeclared = fn(&mut Request, Function, &[u8]);

/// `zend_observer_class_linked_cb` (`Z/Zend/zend_observer.h:141`): told
/// of a class linked into the request's table, and the lowered name it
/// stands under, an alias's for `class_alias()`.
pub(crate) type ClassLinked = fn(&mut Request, ClassId, &[u8]);

/// `zend_observer_error_notify` (`Z/Zend/zend_observer.h:156-160`): each
/// error observer the bundle declared told of the diagnostic, in
/// declaration order. php calls it in `zend_error_impl` once the
/// diagnostic is past the compile's record and any pending throwable it
/// reports, beside and before `zend_error_cb` (`Z/Zend/zend.c:1528`),
/// and in `zend_exception_error` before an uncaught throwable reaches
/// `zend_error_cb` (`Z/Zend/zend_exceptions.c:926`, `:953`). It is not
/// `zend_error_cb`'s chain ([`crate::api::abi::chain::Error`]): an error
/// observer is a list told of every diagnostic, whether the user's
/// handler takes it or `error_reporting` hides it, and it changes
/// nothing of how the diagnostic is served; a link of `zend_error_cb`
/// serves the diagnostics the user's handler leaves, and may serve one
/// in place of php's own `php_error_cb`. Nothing more than one check of
/// the list while none is declared; `told` says which notice it is.
#[inline]
pub(crate) fn error(
    req: &mut Request,
    level: Level,
    file: &[u8],
    line: u32,
    message: &[u8],
    told: Told,
) {
    if req.shared.supplied.error_observers.is_empty() {
        return;
    }
    notify_error(
        req,
        ErrorNotice {
            level,
            file,
            line,
            message,
            told,
        },
    );
}

#[cold]
#[inline(never)]
fn notify_error(req: &mut Request, notice: ErrorNotice<'_>) {
    tell!(req, error_observers, |req, observer| observer(req, &notice));
}

/// `zend_observer_function_declared_notify`
/// (`Z/Zend/zend_observer.h:136-140`): each observer of declared
/// functions told of `function`, declared under `lowered`, in
/// declaration order. php tells them where `do_bind_function` binds a
/// `DECLARE_FUNCTION`'s function (`Z/Zend/zend_compile.c:1300`) and
/// where a file's top-level function is declared as the file loads
/// ([`function_declared_at_load`]).
pub(crate) fn function_declared(req: &mut Request, function: Function, lowered: &[u8]) {
    if !functions_declared_observed(req) {
        return;
    }
    tell!(req, functions_declared, |req, observer| observer(
        req, function, lowered
    ));
}

/// `zend_observer_class_linked_notify` (`Z/Zend/zend_observer.h:146-150`):
/// each observer of linked classes told of `class`, standing under
/// `lowered`, in declaration order. php tells them where
/// `zend_bind_class_in_slot` binds a `DECLARE_CLASS`'s class
/// (`Z/Zend/zend_compile.c:1326`, `:1332`), where a user class is given
/// an alias (`Z/Zend/zend_API.c:3635`), and where a file's classes are
/// linked as it loads ([`class_linked_at_load`]); an anonymous class's
/// `DECLARE_ANON_CLASS` tells none.
pub(crate) fn class_linked(req: &mut Request, class: ClassId, lowered: &[u8]) {
    if !classes_linked_observed(req) {
        return;
    }
    tell!(req, classes_linked, |req, observer| observer(
        req, class, lowered
    ));
}

/// `zend_observer_function_declared_observed`: whether an observer of
/// declared functions is declared, which a site asks before it does any
/// work for the notice.
pub(crate) fn functions_declared_observed(req: &Request) -> bool {
    !req.shared.supplied.functions_declared.is_empty()
}

/// `zend_observer_class_linked_observed`, as
/// [`functions_declared_observed`].
pub(crate) fn classes_linked_observed(req: &Request) -> bool {
    !req.shared.supplied.classes_linked.is_empty()
}

/// The function at `at` of `unit`'s top-level functions told as its
/// load declares it ([`Request::declare_unit`]), under its lowered name.
/// The engine's units are shared and kept like opcache's, so a unit
/// served again from the engine's cache is told again, as each
/// request's compile or load tells php's observers again.
///
/// [`Request::declare_unit`]: crate::api::request::Request::declare_unit
pub(crate) fn function_declared_at_load(req: &mut Request, unit: &Unit, at: usize) {
    if !functions_declared_observed(req) {
        return;
    }
    let Some((decl, _)) = unit.functions.get(at) else {
        return;
    };
    let name = decl.name.0.to_ascii_lowercase();
    if let Some(declared) = req.function_named((&decl.name).into()) {
        function_declared(req, declared, &name);
    }
}

/// The class at `at` of `unit` told as its load links it
/// ([`Request::declare_unit`]), under its lowered name, as
/// [`function_declared_at_load`].
///
/// [`Request::declare_unit`]: crate::api::request::Request::declare_unit
pub(crate) fn class_linked_at_load(req: &mut Request, unit: &Unit, at: usize) {
    if !classes_linked_observed(req) {
        return;
    }
    let Some(decl) = unit.classes.get(at) else {
        return;
    };
    let name = decl.name.0.to_ascii_lowercase();
    if let Some(class) = req.class_named((&decl.name).into()) {
        class_linked(req, class, &name);
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
/// destructor fiber among them, the object an extension's own coroutine
/// stands in ([`crate::fiber::Coroutine`]), or the request's own
/// stack, `EG(main_fiber_context)`; and its status at the notice. What an
/// extension keeps per fiber, which php keeps in the `reserved` slots,
/// it keys by the object [`ObservedFiber::fiber`] names, its
/// `PhpObject::index`, from the creation notice to the destruction
/// notice, and by none for the request's own.
#[derive(Debug, Clone, Copy)]
pub struct ObservedFiber {
    fiber: Option<PhpObject>,
    status: FiberStatus,
    /// What php's observer reads of a `Fiber` through
    /// `zend_fiber_from_context`, which it reads only where
    /// `context->kind` is `zend_ce_fiber`: none for an extension's
    /// coroutine and the request's own stack.
    as_fiber: Option<AsFiber>,
}

/// A `Fiber`'s `ZEND_FIBER_FLAG_THREW`, `ZEND_FIBER_FLAG_DESTROYED` and
/// `caller` at a notice.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AsFiber {
    pub(crate) threw: bool,
    pub(crate) destroyed: bool,
    pub(crate) caller: Option<Caller>,
}

/// php's `fiber->caller`: the stack a running coroutine was resumed
/// from, which its suspend goes back to; set by the resume, kept while
/// a suspend of a coroutine below it passes through it, and cleared by
/// its own suspend and when its body ends.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Caller {
    /// `EG(main_fiber_context)`, the request's own stack.
    Request,
    /// A coroutine, a `Fiber`'s or an extension's.
    Coroutine(crate::fiber::Context),
}

impl Caller {
    /// The caller a resume from `resumer` sets, none being the request's
    /// own stack.
    pub(crate) fn of(resumer: Option<crate::fiber::Context>) -> Caller {
        resumer.map_or(Caller::Request, Caller::Coroutine)
    }

    /// Whether this is the stack `fiber` names, none being the
    /// request's own.
    fn is(self, fiber: Option<PhpObject>) -> bool {
        match (self, fiber) {
            (Caller::Request, None) => true,
            (Caller::Coroutine(caller), Some(fiber)) => caller.object().index() == fiber.index(),
            (Caller::Request, Some(_)) | (Caller::Coroutine(_), None) => false,
        }
    }
}

impl ObservedFiber {
    pub(crate) fn of(
        fiber: Option<PhpObject>,
        status: FiberStatus,
        as_fiber: Option<AsFiber>,
    ) -> ObservedFiber {
        ObservedFiber {
            fiber,
            status,
            as_fiber,
        }
    }

    /// The object the coroutine stands in, a `Fiber`'s as
    /// `zend_fiber_from_context` reads it or an extension's; `None` for
    /// the request's own stack.
    #[must_use]
    pub fn fiber(&self) -> Option<PhpObject> {
        self.fiber
    }

    /// `context->kind == zend_ce_fiber`: whether the coroutine is a
    /// `Fiber`'s, whose flags and `caller` php's observer reads, and not
    /// an extension's or the request's own stack.
    #[must_use]
    pub fn is_fiber(&self) -> bool {
        self.as_fiber.is_some()
    }

    /// `context->status` at the notice.
    #[must_use]
    pub fn status(&self) -> FiberStatus {
        self.status
    }

    /// `ZEND_FIBER_FLAG_THREW`: the fiber's callable left a throwable,
    /// or the unwind exit, which is how a `Dead` fiber that threw is told
    /// from one that returned. Set on a `Fiber` alone, whose flags are
    /// the ones php keeps on `zend_fiber`.
    #[must_use]
    pub fn threw(&self) -> bool {
        self.as_fiber.is_some_and(|fiber| fiber.threw)
    }

    /// `ZEND_FIBER_FLAG_DESTROYED`: the fiber is being torn down, resumed
    /// with the graceful exit because nothing holds it any more, which
    /// is how a switch into a fiber to destroy it is told from one that
    /// resumes it. Set on a `Fiber` alone.
    #[must_use]
    pub fn destroyed(&self) -> bool {
        self.as_fiber.is_some_and(|fiber| fiber.destroyed)
    }

    /// `fiber->caller == from`: a `Fiber` this switch enters because
    /// `from` resumed it, and not one a suspend that passed through it
    /// left waiting on another stack. `false` for any other coroutine.
    #[must_use]
    pub fn resumed_by(&self, from: &ObservedFiber) -> bool {
        self.as_fiber
            .and_then(|fiber| fiber.caller)
            .is_some_and(|caller| caller.is(from.fiber))
    }

    /// `fiber->caller != NULL`: a `Fiber` that runs, or waits on a
    /// coroutine it resumed or one a suspend passed through it to, and
    /// so is not suspended by this switch. `false` for any other
    /// coroutine.
    #[must_use]
    pub fn has_caller(&self) -> bool {
        self.as_fiber.is_some_and(|fiber| fiber.caller.is_some())
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
    tell!(req, fiber_init, |req, handler| handler(req, fiber));
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
    tell!(req, fiber_switch, |req, handler| handler(req, from, to));
}

/// `zend_observer_fiber_destroy_notify` (`Z/Zend/zend_observer.c:427-436`):
/// a dead `fiber` given back, once the switch out of it has
/// restored its resumer's state (`zend_fiber_destroy_context`,
/// `Z/Zend/zend_fibers.c:465-475`, called at `:549-551`).
pub(crate) fn fiber_destroyed(req: &mut Request, fiber: ObservedFiber) {
    if req.shared.supplied.fiber_destroy.is_empty() {
        return;
    }
    tell!(req, fiber_destroy, |req, handler| handler(req, fiber));
}
