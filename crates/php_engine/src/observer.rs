//! Seam 9, the observer (design §4.3 row 9 and "What a caller of seam 9
//! must know", revision 79): php's observer interface for user calls,
//! `Z/Zend/zend_observer.c`.
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
//!   `handlers::unwind_from`.
//!
//! That is the plain test #1808 measured, which #1954 ruled stands: the test
//! lives in these two functions alone, so php's observer-specialised call
//! and return handlers, the escape design §4.3 names, would change no
//! item of the seam. Past the word, out of line and cold, a frame's
//! function is resolved on its first call into a slot in the runtime
//! cache of the op array it serves ([`Slot`]), never indexed by the
//! function's identity number, which is sparse: every observer asked in
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
//! What is not here yet, each built on the same slot and chain:
//! natives (#1810), a fiber's own chain and its notices (#1811), the
//! other notices and adding and removing handlers while a request is
//! served (#1812), the stepping function's events (#1813), and a frame
//! reference a handler may keep (#1827).
//!
//! [`Extension`]: crate::api::abi::Extension
//! [`ExtensionBuilder::observer`]: crate::api::abi::ExtensionBuilder::observer

use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use php_heap::{Fatal, FrameRef};

use crate::api::call::Function;
use crate::api::request::Request;
use crate::api::values::{ClassId, PhpObject, Value};

/// `zend_observer_fcall_init` (`Z/Zend/zend_observer.h:70`): asked once of each function, on its first
/// call in the request, and of each included, evaluated or main file's
/// frame on its activation, what handlers observe it; the default,
/// neither, declines it, as php's answers `{NULL, NULL}`.
pub type ObserverInit = fn(&mut Observed<'_>) -> ObserverHandlers;

/// `zend_observer_fcall_begin_handler`: a frame begun, before its
/// parameters are received.
pub type BeginHandler = fn(&mut Observed<'_>);

/// `zend_observer_fcall_end_handler`: a frame ended, with what it
/// returned, or `None` where an unwind or a fatal left it.
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
/// the one frame observed, which no reference outlives. A frame reference that
/// can be kept is #1827's.
pub struct Observed<'a> {
    req: &'a mut Request,
    frame: FrameRef,
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

impl Observed<'_> {
    /// `EX(func)` where the frame serves a function or a method; `None`
    /// for an included, evaluated or main file's op array, which php
    /// names by its `filename` alone.
    #[must_use]
    pub fn function(&self) -> Option<Function> {
        self.req.frame_function(self.frame)
    }

    /// `EX(func)->common.scope`: the class a method is declared in, the
    /// one a closure is bound to, and the includer's for a file's frame.
    #[must_use]
    pub fn function_scope(&self) -> Option<ClassId> {
        self.req.frame_scope(self.frame)
    }

    /// `EX(func)->op_array.filename`: the file the op array was compiled
    /// from, `… : eval()'d code` for an evaluated string's.
    #[must_use]
    pub fn filename(&self) -> &[u8] {
        self.req.frame_filename(self.frame).unwrap_or_default()
    }
}

/// One op array's observer slot in a request's runtime cache, php's
/// `ZEND_OBSERVER_DATA`: unresolved until the first call, then the
/// handlers every observer answered, or declined where none answered
/// any (`ZEND_OBSERVER_NONE_OBSERVED`). Held where the op array's
/// runtime cache is, so it lives as long as php's: a function's for the
/// request, a file's for its activation.
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
        begin(req, frame);
    }
}

/// The return home: `frame` about to be left, by its `RETURN` with what
/// it returned, or by an unwind with nothing. As [`call`] while no
/// observer is declared.
#[inline(always)] // as `call`
#[allow(clippy::inline_always)]
pub(crate) fn returned(req: &mut Request, frame: FrameRef, value: Option<&Value>) {
    if req.shared.observers != 0 {
        end(req, frame, value);
    }
}

/// `zend_observer_fcall_begin` (`Z/Zend/zend_observer.c:288-294`): the
/// frame begun ([`begin_prechecked`]) unless it is a generator's, which
/// is begun when it is resumed ([`generator_resumed`]) and not at its
/// call.
#[cold]
#[inline(never)]
fn begin(req: &mut Request, frame: FrameRef) {
    let generator = req
        .frame_function(frame)
        .and_then(|function| req.function_decl(function))
        .is_some_and(|decl| decl.is_generator);
    if generator {
        return;
    }
    begin_prechecked(req, frame);
}

/// The resumption home: a generator's `frame` pushed again by
/// `zend_generator_resume` and about to be served, which is begun as a
/// call is, `zend_observer_generator_resume` (`Z/Zend/zend_observer.c:283-286`,
/// called at `Z/Zend/zend_generators.c:847`). As [`call`] while no
/// observer is declared.
#[inline]
pub(crate) fn generator_resumed(req: &mut Request, frame: FrameRef) {
    if req.shared.observers != 0 {
        begin_prechecked(req, frame);
    }
}

/// The suspension home: a generator's `frame` about to leave the stack
/// at a `YIELD`, or at a `YIELD_FROM` that delegates, ended with the
/// value the generator holds then, php's
/// `zend_observer_fcall_end(generator->execute_data, &generator->value)`
/// once `zend_execute_ex` has returned (`Z/Zend/zend_generators.c:849-852`):
/// what it yielded, or, at a `yield from`, what it yielded last. As
/// [`call`] while no observer is declared.
#[inline]
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
    end(req, frame, Some(&value));
    req.release(value);
}

/// `zend_observer_fcall_begin_prechecked` (`Z/Zend/zend_observer.c:257-281`):
/// the frame's slot resolved on its first call, the frame put on the
/// open chain where an end handler observes it, and the begin handlers
/// called in declaration order.
fn begin_prechecked(req: &mut Request, frame: FrameRef) {
    let slot = match req.observer_slot(frame) {
        Slot::Unresolved => resolve(req, frame),
        resolved => resolved,
    };
    let Slot::Installed(handlers) = slot else {
        return;
    };
    if !handlers.end.is_empty() {
        req.open_observed(frame);
    }
    for begin in &handlers.begin {
        begin(&mut Observed { req, frame });
    }
}

/// `zend_observer_fcall_install` (`Z/Zend/zend_observer.c:116-156`): every observer asked of the frame in
/// declaration order, and what they answered written into its slot.
fn resolve(req: &mut Request, frame: FrameRef) -> Slot {
    let shared = Arc::clone(&req.shared);
    let mut begin = Vec::new();
    let mut end = Vec::new();
    for init in shared.supplied.observers.iter() {
        let handlers = init(&mut Observed { req, frame });
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
    req.set_observer_slot(frame, slot.clone());
    slot
}

/// `zend_observer_fcall_end` (`Z/Zend/zend_observer.h:124-128`): where `frame` is the innermost open one,
/// it is taken off the chain and its end handlers called in reverse
/// declaration order; any other frame was not begun with an end
/// handler, or was ended already. The frame leaves the chain before its
/// handlers are called, so a fatal inside one does not end it twice.
#[cold]
#[inline(never)]
fn end(req: &mut Request, frame: FrameRef, value: Option<&Value>) {
    if req.close_observed(frame) {
        call_end_handlers(req, frame, value);
    }
}

/// `call_end_observers` (`Z/Zend/zend_observer.c:296-311`): the end
/// handlers of `frame`'s slot, called in the order it holds them.
fn call_end_handlers(req: &mut Request, frame: FrameRef, value: Option<&Value>) {
    let Slot::Installed(handlers) = req.observer_slot(frame) else {
        return;
    };
    for end in &handlers.end {
        end(&mut Observed { req, frame }, value);
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
    for frame in req.take_observed() {
        call_end_handlers(req, frame, None);
    }
}
