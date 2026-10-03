//! Seam 8, the tier (design §4.3, `jit-design.md` §2, §3; ADR-0006): the
//! loop asks the tier at three safepoints and nowhere else, through one
//! method bound per engine. `Nop` is the interpreter's own adapter; the
//! compiled tier is `php_jit::Jit`. The word each safepoint carries is
//! two atomics whose bits only the tier interprets.
//!
//! [`execute`] and [`step`] are the loop's two entries: one dispatch over
//! `php_unit::Opcode` (design §5.8), generated from `ops.toml` in this
//! crate's `build.rs` and generic over the tier, so `Nop` folds the
//! safepoint away. `execute` raises `Site::Entry` for the frame it is
//! given, after that frame's prologue; `RETURN` raises `Site::Return` for
//! every other frame and for no entry frame, which the stack's entry slot
//! says.
//!
//! The debugging host is a `Tier` too (`jit-design.md` §2.4, revision 8;
//! design §5.8, revision 51): its `safepoint` is where it stops the serving and [`step`]
//! is its loop, as the recorder's, with [`unwind`] beside it for the
//! frame a step left a throwable pending in, and the frame reads
//! between the steps. `php_sapi` re-exports this face, so a host that
//! binds a tier names none of `php_engine`.

use std::sync::atomic::AtomicU32;

use crate::api::abi::Extension;
use crate::{exec, interrupt};

pub use crate::api::request::Request;
pub use php_heap::{Fatal, FrameRef};
pub use php_unit::Opcode;

mod code;
mod frames;
mod values;
pub use code::{
    Class, Code, Named, Shape, class_named, frame_code, function_named, method_named, table_sizes,
    user_classes, user_functions,
};
pub use frames::{
    Argument, Aside, Called, Evaluated, Pending, Position, Serves, Under, below, call, caller,
    catches, entry, evaluate, evaluate_code, exit, exiting, function_exists, oplines, pending,
    position, restore, serves, set_aside,
};
pub(crate) use values::values_shown;
pub use values::{
    Identity, Key, Shown, TraceRow, Variable, backtrace, included_files, last_error, literals,
    set_argv, superglobals, user_constants, variable, variables,
};

/// The number of a safepoint within its op array; zero is the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SafepointIdx(pub u32);

impl SafepointIdx {
    /// The entry safepoint.
    pub const ENTRY: SafepointIdx = SafepointIdx(0);
}

/// Where the loop is asking: a loop header from its backward jump, the
/// activation of a frame after its prologue, or a return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    Loop(SafepointIdx),
    Entry,
    Return(SafepointIdx),
}

impl Site {
    /// The safepoint's number.
    #[must_use]
    pub fn idx(self) -> SafepointIdx {
        match self {
            Site::Loop(i) | Site::Return(i) => i,
            Site::Entry => SafepointIdx::ENTRY,
        }
    }
}

/// What the tier told the loop to do.
#[derive(Debug)]
pub enum Flow {
    /// Nothing happened; the loop continues at the frame's opline.
    Continue,
    /// The tier served code; the loop continues in this frame after the
    /// interrupt and pending-throwable tests.
    Resume(FrameRef),
    /// The tier ran the serving's entry frame to its end: the serving is
    /// over and its return value is set. `execute` takes it as
    /// [`Stepped::Done`], as php's compiled code returns `ZEND_VM_RETURN`
    /// to `execute_ex` for a `ZEND_CALL_TOP` frame (`zend_jit_ir.c:2104`;
    /// `jit-design.md` §2.2, revision 10).
    Returned,
    /// A fatal was raised inside tier code.
    Bail(Fatal),
}

/// The tier: one method at the safepoints, and the state it declares as
/// an extension would.
pub trait Tier: 'static {
    /// Asked at the three sites and nowhere else.
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow;

    /// The tier's engine state, request state and ini, under seam 3
    /// without being an extension; `None` declares nothing.
    #[must_use]
    fn extension() -> Option<Extension> {
        None
    }
}

/// The interpreter alone: one inlined constant, so the branch on `Flow`
/// folds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Nop;

impl Tier for Nop {
    #[inline(always)] // jit-design.md §2.2: one constant, so the branch on `Flow` folds
    #[allow(clippy::inline_always)]
    fn safepoint(_: &mut Request, _: FrameRef, _: Site) -> Flow {
        Flow::Continue
    }
}

/// One safepoint's word (`jit-design.md` §3.2): two atomics, zero at
/// publication, all ones when sealed.
#[derive(Debug, Default)]
pub struct Word {
    halves: [AtomicU32; 2],
}

impl Word {
    /// The one accessor; the meaning of the bits is the tier's.
    #[must_use]
    pub fn halves(&self) -> &[AtomicU32; 2] {
        &self.halves
    }

    /// A word no tier acts on: every bit set. Allocated for every
    /// safepoint of a request-scoped op array.
    #[must_use]
    pub fn sealed() -> Word {
        Word {
            halves: [AtomicU32::new(u32::MAX), AtomicU32::new(u32::MAX)],
        }
    }
}

/// What one `step` did to the frame chain (`jit-design.md` §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stepped {
    Same(FrameRef),
    Pushed(FrameRef),
    Popped(FrameRef),
    Moved(FrameRef),
    Threw(FrameRef),
    Done,
}

/// The generated dispatch (design §4.3 seam 7): one `match` arm per
/// opcode `ops.toml` names, over the handlers beside it.
mod dispatch {
    use php_heap::{Fatal, FrameRef};
    use php_unit::{OpArray, Opcode, Opline};

    use crate::api::request::Request;
    use crate::tier::{Stepped, Tier};

    include!(concat!(env!("OUT_DIR"), "/dispatch.rs"));
}

/// What a handler does with the flow a safepoint answered. `Continue` is
/// the whole of it while `Nop` is the only adapter; a handler cannot
/// dispatch from another frame, so a tier that served code at a
/// handler's site lands with `php_jit` (ADR-0006). The loop's own sites
/// take a `Resume` through [`answered`].
pub(crate) fn continued(flow: Flow) -> Result<(), Fatal> {
    match flow {
        Flow::Continue => Ok(()),
        Flow::Bail(fatal) => Err(fatal),
        Flow::Resume(_) | Flow::Returned => Err(Fatal::Error(String::from(
            "InternalError: a tier served code and this loop has no resumption for it yet",
        ))),
    }
}

/// What the loop does with the flow a safepoint answered
/// (`jit-design.md` §2.2), as an edge it already knows: `Continue` goes
/// on in the same frame, and a `Resume` is the frame the tier left the
/// loop in, after the interrupt check the loop performs at a back-edge
/// and the pending test it performs after a `throw`-effect op. The two
/// are in that order, and an interrupt the tier found raised is raised
/// still, because consuming it is the loop's.
fn answered(req: &mut Request, flow: Flow, frame: FrameRef) -> Result<Stepped, Fatal> {
    let at = match flow {
        Flow::Continue => return Ok(Stepped::Same(frame)),
        Flow::Bail(fatal) => return Err(fatal),
        Flow::Returned => return Ok(Stepped::Done),
        Flow::Resume(at) => at,
    };
    if interrupt::check(req)? || req.is_pending() {
        return Ok(Stepped::Threw(at));
    }
    Ok(Stepped::Same(at))
}

/// The activation of a frame, its prologue having raised nothing
/// (design §5.6, §4.3 seam 8): the interrupt check, then `Site::Entry`,
/// then the frame the loop goes on in.
fn activated<T: Tier>(req: &mut Request, frame: FrameRef) -> Result<Stepped, Fatal> {
    if interrupt::check(req)? {
        return Ok(Stepped::Threw(frame));
    }
    let flow = T::safepoint(req, frame, Site::Entry);
    answered(req, flow, frame)
}

/// The dispatch loop over `frame` until the entry frame returns, asking
/// `T` at the safepoints. `Ok(())` exactly when the entry frame has
/// returned or left a throwable it did not catch pending; `Err` carries
/// the fatal. The throwable is rendered by the caller once the frame is
/// popped, as php's `zend_execute_scripts` calls `zend_exception_error`
/// after `zend_execute` has returned, so what the rendering serves
/// stands on no frame of the main op array's: a throwable its
/// `__toString` raises names no active file.
///
/// ```php
/// class E extends Exception { function __construct() { $this->message = new stdClass; } }
/// throw new E;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Object of class stdClass could not be converted to string in [no active file]:0
/// Stack trace:
/// #0 [internal function]: Exception->__toString()
/// #1 {main}
///   thrown in [no active file] on line 0
/// ```
pub fn execute<T: Tier>(req: &mut Request, frame: FrameRef) -> Result<(), Fatal> {
    serving::<T>(req, frame)
}

/// `zend_call_function` over a user function: the same loop on a frame
/// pushed inside a serving that is already standing, for a native or a
/// destructor calling back into PHP ([`Request::serve_user`]). What its
/// entry frame did not catch stays pending and `Ok(())` says the
/// serving is over, not that the frame returned; the caller reads the
/// pending slot, as php's caller reads `EG(exception)`.
pub(crate) fn reenter<T: Tier>(req: &mut Request, frame: FrameRef) -> Result<(), Fatal> {
    serving::<T>(req, frame)
}

/// `zend_generator_resume`'s `zend_execute_ex(generator->execute_data)`
/// (design §5.11): the same loop once more on a generator's frame, which
/// the resumption has put back on the stack where the resumer stands.
/// The frame was activated before `GENERATOR_CREATE` moved it and is not
/// activated again, so no `Site::Entry` is raised; the loop dispatches
/// the opline the frame stands at, or unwinds from it where a throwable
/// is pending, which is how `Generator::throw` raises at the `yield`.
/// The serving is over when the frame yields, returns, or leaves a
/// throwable it did not catch pending for the resumer to read.
pub(crate) fn resume<T: Tier>(req: &mut Request, frame: FrameRef) -> Result<(), Fatal> {
    let guard = req.stack_mut().push_entry(frame);
    if refused_at_entry(req) {
        req.stack_mut().pop_entry(guard);
        return Ok(());
    }
    let first = if req.is_pending() {
        Stepped::Threw(frame)
    } else {
        Stepped::Same(frame)
    };
    let outcome = serve_from::<T>(req, first);
    req.stack_mut().pop_entry(guard);
    outcome
}

/// One serving: the entry slot pushed, the loop, the entry slot popped
/// on the fatal path as on the normal one.
fn serving<T: Tier>(req: &mut Request, frame: FrameRef) -> Result<(), Fatal> {
    let guard = req.stack_mut().push_entry(frame);
    let outcome = serve::<T>(req, frame);
    req.stack_mut().pop_entry(guard);
    outcome
}

/// The loop itself, with the entry slot standing. `execute` activates
/// its own entry frame whatever the caller is, where php installs its
/// own site (design §4.3 seam 8).
fn serve<T: Tier>(req: &mut Request, entry: FrameRef) -> Result<(), Fatal> {
    if interrupt::check(req)? {
        return serve_from::<T>(req, Stepped::Threw(entry));
    }
    // A throwable raised before any opline of the entry frame ran, where
    // php clears `EG(opline_before_exception)` so that
    // `ZEND_HANDLE_EXCEPTION` looks for no `try` element and frees
    // nothing the frame's opline would have written: the serving is
    // over with the throwable pending, as the unwinder leaves an entry
    // frame it caught nothing in.
    if refused_at_entry(req) {
        return Ok(());
    }
    let flow = T::safepoint(req, entry, Site::Entry);
    let first = answered(req, flow, entry)?;
    serve_from::<T>(req, first)
}

/// `execute_ex`'s own check (`Z/Zend/zend_vm_execute.h:116244`), after
/// the interrupt check and before the first opline: where the machine
/// stack has come down to `zend.max_allowed_stack_size`, php's
/// `zend_call_stack_size_error` is left pending, and whether it was.
/// php enters `execute_ex` again for every call a native makes back
/// into PHP and for every generator it resumes, never for a user
/// function a user function calls, so this is where a recursion through
/// a callback is refused before it ends the process.
///
/// Under the small-stack regime a recursion through `array_map` reaches
/// the ini's 256K in both engines; the number is the ini's less the
/// platform's least reserve, so it is masked.
///
/// ```php
/// function f($n) { return $n ? array_map('f', [$n - 1])[0] : 0; }
/// try { var_dump(f(50000)); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect regime=small-stack
/// Maximum call stack size of %d bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?
/// ```
fn refused_at_entry(req: &mut Request) -> bool {
    match crate::api::raising::call_stack_limit(req) {
        Ok(()) => false,
        Err(thrown) => {
            req.leave_pending(&thrown);
            true
        }
    }
}

/// The loop from the edge its first step took.
fn serve_from<T: Tier>(req: &mut Request, first: Stepped) -> Result<(), Fatal> {
    let mut frame;
    let mut stepped = first;
    loop {
        match stepped {
            Stepped::Same(at) | Stepped::Moved(at) | Stepped::Popped(at) => frame = at,
            Stepped::Pushed(at) => {
                frame = at;
                stepped = activated::<T>(req, frame)?;
                continue;
            }
            Stepped::Threw(at) => {
                let unwound = unwind_frame::<T>(req, at)?;
                if unwound == Stepped::Threw(at) {
                    // The entry frame caught nothing: the throwable stays
                    // pending for the caller, the native that re-entered
                    // the loop or `serve` once the main frame is popped.
                    return Ok(());
                }
                stepped = unwound;
                continue;
            }
            Stepped::Done => return Ok(()),
        }
        let serving = standing(req, frame)?;
        let code = code_of(&serving, frame)?;
        let op = *at_opline(req, frame, code)?;
        stepped = dispatch::dispatch::<T>(req, frame, code, &op)?;
        #[cfg(debug_assertions)]
        if let Some(sensed) = req.take_sensed() {
            return Err(sensed);
        }
    }
}

/// The unwinder over one frame (design §5.9): php replaces the frame's
/// opline with its `exception_op`, whose handler is
/// `ZEND_HANDLE_EXCEPTION`, and reads the opline that raised back out
/// of `EG(opline_before_exception)`; that opline is still where the
/// frame stands here, so the handler is called on it directly. It
/// answers the same frame where it landed at a `catch` or a `finally`,
/// the caller where it left the frame, and this frame again where the
/// serving's entry frame caught nothing, which is where the serving
/// ends.
fn unwind_frame<T: Tier>(req: &mut Request, frame: FrameRef) -> Result<Stepped, Fatal> {
    let serving = standing(req, frame)?;
    let code = code_of(&serving, frame)?;
    let op = *at_opline(req, frame, code)?;
    crate::handlers::handle_exception::<T>(req, frame, code, &op)
}

/// Exactly one handler, the one at the frame's opline, with `Nop` at the
/// safepoints; what it did to the frame chain is reported, and a
/// throwable it raised is left pending.
pub fn step(req: &mut Request, frame: FrameRef) -> Result<Stepped, Fatal> {
    let serving = standing(req, frame)?;
    let code = code_of(&serving, frame)?;
    let op = *at_opline(req, frame, code)?;
    let stepped = dispatch::dispatch::<Nop>(req, frame, code, &op)?;
    #[cfg(debug_assertions)]
    if let Some(sensed) = req.take_sensed() {
        return Err(sensed);
    }
    Ok(stepped)
}

/// `ZEND_HANDLE_EXCEPTION` over one frame, what the loop runs for a
/// [`Stepped::Threw`]: the frame lands at a `catch` clause or a
/// `finally` block and it answers `Same`, or it is left and it
/// answers `Threw` of its caller with the throwable still pending; with
/// nothing pending, it is left as a return of nothing and it answers
/// `Popped` of its caller, which goes on past the call, or `Done` where
/// it is the serving's entry frame, its return value `UNDEF`, which a
/// tier hands back with `Flow::Returned`.
/// On the entry frame of the serving, which the unwinder does not
/// leave, `Threw` of that frame says nothing took it; a tier hands that
/// frame back with `Flow::Resume` and the loop ends the serving, so one
/// asks [`catches`] first and unwinds only a frame that takes it.
pub fn unwind(req: &mut Request, frame: FrameRef) -> Result<Stepped, Fatal> {
    unwind_frame::<Nop>(req, frame)
}

/// What `frame` is serving, held by its own share of the unit so the
/// oplines can be read while the request is borrowed.
fn standing(req: &Request, frame: FrameRef) -> Result<exec::Serving, Fatal> {
    req.vm().serving(frame).ok_or_else(|| {
        Fatal::Error(format!(
            "InternalError: frame {} is serving no op array",
            frame.0
        ))
    })
}

/// The op array `frame` is serving, out of its unit.
fn code_of(serving: &exec::Serving, frame: FrameRef) -> Result<&php_unit::OpArray, Fatal> {
    serving.code().ok_or_else(|| {
        Fatal::Error(format!(
            "InternalError: frame {} names a body its unit has not",
            frame.0
        ))
    })
}

/// The opline `frame` stands at.
fn at_opline<'a>(
    req: &Request,
    frame: FrameRef,
    code: &'a php_unit::OpArray,
) -> Result<&'a php_unit::Opline, Fatal> {
    let at = req.vm().opline(frame);
    exec::opline_at(code, at).ok_or_else(|| {
        Fatal::Error(format!(
            "InternalError: frame {} stands at opline {at}, past the end of its op array",
            frame.0
        ))
    })
}
