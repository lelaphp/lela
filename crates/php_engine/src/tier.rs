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
//! design §5.8, revision 51): its `safepoint` is where it stops the
//! serving and [`step`] is its loop, as the recorder's, with [`unwind`]
//! beside it for the frame a step left a throwable pending in. This face
//! is the mechanism and the writes: the frames a step hands out, the
//! stack they stand on, and the code a tier has the engine serve on its
//! own account. What a frame, an op array or a linked table holds, a
//! tier reads through the native face, as an extension does ([`frame`]
//! hands a frame a step named over to it; #1911). What no extension
//! reads, the recorder reads here between its steps (`jit-design.md`
//! §4.1): the type an operand's slot holds, whether a frame is a
//! generator's, and the call an `INIT_*` began. `php_sapi` re-exports
//! this face, so a host that binds a tier names none of `php_engine`.

use std::sync::atomic::AtomicU32;

/// What a tier declares its state with, as an extension does (design
/// §4.4; `jit-design.md` §4, §5.3).
pub use crate::api::abi::{Extension, ExtensionBuilder, Refusal, Start, StartupIni};
/// What a tier's request start answers where it fails, as an
/// extension's `rinit` does.
pub use crate::api::raising::Thrown;
use crate::observer::{Declared, ObservedOpcodes, Telling, Untold};
use crate::{exec, interrupt};
use php_unit::OpArray;

pub use crate::api::request::Request;
pub use php_compile::Hooked;
pub use php_heap::{Fatal, FrameRef};

mod frames;
mod held;
pub(crate) mod published;
mod sampled;
#[cfg(dispatch = "tailcall")]
mod tail;
mod view;
pub use frames::{
    Argument, Aside, Called, Compiled, Evaluated, Under, begin, below, call, catches, compile,
    entry, evaluate, evaluate_code, exit, frame, restore, set_argv, set_aside, set_stdin,
};
use held::{Entered, Kept, Units};
pub use sampled::{Building, Sample, building, class_number, numbered_class, on_heap, sample};
pub use view::{Epoch, FuncRef, MapPtrRaw, UnitView, epoch, epoch_state, func, word};

pub use crate::api::values::ClassId;

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

    /// Which of php's execution hooks the tier replaces, as phpdbg
    /// replaces `zend_execute_ex`: every unit the engine compiles is
    /// compiled under it, so its calls are the ones `zend_get_call_op`
    /// picks for the same host (design §4.3, revision 58).
    const HOOKED: Hooked = Hooked::NONE;

    /// Whether the tier holds the frame a serving's `execute` enters
    /// before it executes it, as phpdbg holds the program it compiled at
    /// its prompt and executes it at `run`: `execute` then
    /// raises no observer begin for that frame (seam 9), and the tier
    /// raises it with [`begin`] where it executes the frame, so a frame
    /// it never executes is never begun and never ended.
    const HOLDS: bool = false;

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
    pub const fn sealed() -> Word {
        Word {
            halves: [AtomicU32::new(u32::MAX), AtomicU32::new(u32::MAX)],
        }
    }
}

/// The trace number of an execution, php's `EG(jit_trace_num)`
/// (`jit-design.md` §4.9, revisions 23 and NEXT; design §5.11,
/// revision 52): one word the engine keeps per execution and only the
/// tier reads, the number of the trace executing on that execution. It
/// excludes nothing: the tier's recording exclusion is the request's, as
/// php's `JIT_G(tracing)` is. As with [`Word`], zero is no trace and the
/// bits are the tier's; where the engine saves, sets and restores it is
/// [`trace_num`]'s to say.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TraceNum(u32);

impl TraceNum {
    /// No trace: what a request and a fiber's body start at and what a
    /// generator's body runs at ([`trace_num`]).
    pub const NONE: TraceNum = TraceNum(0);

    /// The tier's number as the engine keeps it.
    #[must_use]
    pub const fn from_raw(n: u32) -> TraceNum {
        TraceNum(n)
    }

    /// The tier's number back.
    #[must_use]
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// The trace number of the execution the request runs on: the tier's,
/// as php's tracing tier reads `EG(jit_trace_num)`. A fiber switch
/// saves and restores it and a fiber's body starts at [`TraceNum::NONE`]
/// (`Z/Zend/zend_fibers.c:105-155`, `:599`); a generator's body runs at
/// `NONE` and its resumer reads its own again after it
/// (`Z/Zend/zend_generators.c:793-865`); a debugger serving a frame
/// below a fiber leaves it the running fiber's, as phpdbg moves
/// `EG(current_execute_data)` alone (`Z/sapi/phpdbg/phpdbg_frame.c:116`).
#[must_use]
pub fn trace_num(req: &Request) -> TraceNum {
    req.trace_num()
}

/// The trace number of the execution the request runs on written, as
/// the tier sets it around a trace it executes ([`trace_num`]).
pub fn set_trace_num(req: &mut Request, n: TraceNum) {
    req.set_trace_num(n);
}

/// The sink the host lent the request, where it is the host's `T`: the
/// value a host hands each request it serves and takes back at its end
/// ([`crate::engine::Engine::serve`]), so what the host keeps across its
/// requests, as php's phpdbg keeps `PHPDBG_G` across the cycles of its
/// main loop, its tier and its natives reach from the request and from
/// nowhere else (design §5.14 *Threads*, `jit-design.md` §8). None where
/// the request writes to another sink, as one the host lints with.
#[must_use]
pub fn sink<T: crate::engine::OutputSink>(req: &mut Request) -> Option<&mut T> {
    req.sink_as()
}

/// `source` served on `engine` as the test face serves a program named
/// `name` in `dir` ([`crate::test::Engine::serve_capture_in`]), and the
/// request state of type `T` as the request left it once it ended: what
/// a tier under test kept in the state it declared through
/// [`Tier::extension`], or an extension under test in the state it
/// declared, read back by the test that served the program, since
/// neither keeps anything anywhere else (`jit-design.md` §8, design
/// §5.14 *Threads*). None where the request built no such state.
#[cfg(any(test, feature = "test"))]
#[must_use]
pub fn serve_kept<T: Send + 'static>(
    engine: &crate::engine::Engine,
    dir: &std::path::Path,
    name: &[u8],
    source: &str,
) -> (crate::test::Served, Option<T>) {
    engine.serve_capture_keeping(dir, name, source)
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

/// Where one handler sends the loop (design §5.8): on in the frame it
/// was handed, at an opline the loop holds in its own hand, or along
/// any other edge of the frame chain. The first two are php's
/// `ZEND_VM_NEXT_OPCODE` and `ZEND_VM_SET_OPCODE` with `opline` in a
/// register: no frame's state records where the loop stands while it
/// goes on in one frame, and the position is written to the frame
/// where something outside the loop can read it, as php's
/// `SAVE_OPLINE()` writes `EX(opline)`: before a handler the dispatch
/// does not leave its own position to, at a safepoint, at a call, a
/// throw or a return, and wherever a step hands the frame to a tier.
/// Every other edge is a [`Stepped`] with every frame's position
/// written where the handler left it.
///
/// One word, so that a handler's answer, `Result<Next, Fatal>`, is two
/// registers and never a slot in memory the loop reads back: the way in
/// the high half and the opline or the frame in the low, which
/// [`Next::way`] reads back as a [`Way`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Next(u64);

/// What a [`Next`] says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Way {
    /// `ZEND_VM_NEXT_OPCODE`: the opline after this one.
    Advance,
    /// `ZEND_VM_SET_OPCODE`: this opline of the same frame.
    Jump(u32),
    /// Any other edge, the positions on the chain written.
    Left(Stepped),
}

impl Next {
    /// The high half of each way, written by the constructors below and
    /// read by [`Next::way`] and nowhere else.
    const ADVANCE_WAY: u64 = 0;
    const JUMP_WAY: u64 = 1;
    const SAME_WAY: u64 = 2;
    const PUSHED_WAY: u64 = 3;
    const POPPED_WAY: u64 = 4;
    const MOVED_WAY: u64 = 5;
    const THREW_WAY: u64 = 6;
    const DONE_WAY: u64 = 7;

    /// `ZEND_VM_NEXT_OPCODE`.
    pub(crate) const ADVANCE: Next = Next(Next::ADVANCE_WAY << 32);

    /// `ZEND_VM_SET_OPCODE(to)`.
    pub(crate) const fn jump(to: u32) -> Next {
        Next(Next::JUMP_WAY << 32 | to as u64)
    }

    /// Where the handler sends the loop.
    #[inline(always)] // read once per opline by the loop: a shift and a compare
    #[allow(clippy::inline_always)]
    pub(crate) const fn way(self) -> Way {
        // The low half is the opline or the frame, as the high half says.
        #[allow(clippy::cast_possible_truncation)]
        let low = self.0 as u32;
        match self.0 >> 32 {
            Next::ADVANCE_WAY => Way::Advance,
            Next::JUMP_WAY => Way::Jump(low),
            Next::SAME_WAY => Way::Left(Stepped::Same(FrameRef(low))),
            Next::PUSHED_WAY => Way::Left(Stepped::Pushed(FrameRef(low))),
            Next::POPPED_WAY => Way::Left(Stepped::Popped(FrameRef(low))),
            Next::MOVED_WAY => Way::Left(Stepped::Moved(FrameRef(low))),
            Next::THREW_WAY => Way::Left(Stepped::Threw(FrameRef(low))),
            // `DONE_WAY`, the one high half left: the fields are private,
            // so no other is ever written.
            _ => Way::Left(Stepped::Done),
        }
    }
}

impl From<Stepped> for Next {
    fn from(stepped: Stepped) -> Next {
        let (way, frame) = match stepped {
            Stepped::Same(at) => (Next::SAME_WAY, at.0),
            Stepped::Pushed(at) => (Next::PUSHED_WAY, at.0),
            Stepped::Popped(at) => (Next::POPPED_WAY, at.0),
            Stepped::Moved(at) => (Next::MOVED_WAY, at.0),
            Stepped::Threw(at) => (Next::THREW_WAY, at.0),
            Stepped::Done => (Next::DONE_WAY, 0),
        };
        Next(way << 32 | u64::from(frame))
    }
}

/// What a handler's [`Next`] is as an edge of the frame chain, for an
/// entry that hands the frame on rather than going on in it: a step,
/// the unwinder, a prologue. The position the loop would have held is
/// written to `frame`, the opline at `at` being the one the handler
/// served.
pub(crate) fn settled(req: &mut Request, frame: FrameRef, at: u32, next: Next) -> Stepped {
    let to = match next.way() {
        Way::Left(stepped) => return stepped,
        Way::Advance => at.saturating_add(1),
        Way::Jump(to) => to,
    };
    req.vm_mut().stand(frame, to);
    Stepped::Same(frame)
}

/// The generated dispatch (design §4.3 seam 7): one `match` arm per
/// `php_unit::Handler` an opline is resolved to, over the handlers beside
/// it.
mod dispatch {
    use php_heap::{Fatal, FrameRef};
    use php_unit::{Handler, OpArray, Opline};

    use crate::api::request::Request;
    use crate::tier::{Next, Tier};

    include!(concat!(env!("OUT_DIR"), "/dispatch.rs"));
}

/// What a handler does with the flow a safepoint answered. `Continue` is
/// the whole of it while `Nop` is the only adapter; a handler cannot
/// dispatch from another frame, so a tier that served code at a
/// handler's site lands with `php_jit` (ADR-0006). The loop's own sites
/// take a `Resume` through [`answered`]. Inlined into every handler
/// that raises a site, so that under `Nop` the whole of it folds away
/// with the site (design §5.7).
#[inline(always)]
#[allow(clippy::inline_always)]
pub(crate) fn continued(flow: Flow) -> Result<(), Fatal> {
    match flow {
        Flow::Continue => Ok(()),
        Flow::Bail(fatal) => Err(fatal),
        Flow::Resume(_) | Flow::Returned => Err(Fatal::error(
            "InternalError: a tier served code and this loop has no resumption for it yet",
        )),
    }
}

/// What the loop does with the flow a safepoint answered
/// (`jit-design.md` §2.2), as an edge it already knows: `Continue` goes
/// on in the same frame, and a `Resume` is the frame the tier left the
/// loop in, after the interrupt check the loop performs at a back-edge
/// and the pending test it performs after a `throw`-effect op. The two
/// are in that order, and an interrupt the tier found raised is raised
/// still, because consuming it is the loop's.
#[inline]
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
#[inline]
fn activated<T: Tier>(req: &mut Request, frame: FrameRef) -> Result<Stepped, Fatal> {
    if interrupt::check(req)? {
        return Ok(Stepped::Threw(frame));
    }
    let flow = T::safepoint(req, frame, Site::Entry);
    answered(req, flow, frame)
}

/// One entry of the loop, as the engine binds it.
pub(crate) type Loop = fn(&mut Request, FrameRef) -> Result<(), Fatal>;

/// How an engine binds its tier's loop once its declared opcodes are
/// fixed: [`Loops::bound`] for the tier.
pub(crate) type BindLoops = fn(&ObservedOpcodes) -> Loops;

/// The loop's three entries, [`execute`], [`reenter`] and [`resume`],
/// bound to one tier and to one choice of whether declared opcodes are
/// told: the engine binds them as it starts, once the declared set is
/// fixed, as php resolves an opline's handler when it compiles it
/// (design §5.8, §7.1).
#[derive(Clone, Copy)]
pub(crate) struct Loops {
    /// The loop over a serving's entry frame, `execute_ex`.
    pub(crate) entry: Loop,
    /// The same loop for a serving inside a serving,
    /// `zend_call_function` over a user function.
    pub(crate) reenter: Loop,
    /// The same loop over a generator's frame the resumption put back.
    pub(crate) resume: Loop,
}

impl Loops {
    /// `T`'s loop for an engine whose declared set is `opcodes`: the
    /// instantiation that tells the set's observers where an extension
    /// declared an opcode, and the one with no test where none did.
    pub(crate) fn bound<T: Tier>(opcodes: &ObservedOpcodes) -> Loops {
        if opcodes.declares_any() {
            Loops::of::<T, Declared>()
        } else {
            Loops::of::<T, Untold>()
        }
    }

    fn of<T: Tier, D: Telling>() -> Loops {
        Loops {
            entry: execute::<T, D>,
            reenter: reenter::<T, D>,
            resume: resume::<T, D>,
        }
    }
}

/// `zend_execute` over a frame its caller pushed for a main op array:
/// the frame's observer begin, `ZEND_OBSERVER_FCALL_BEGIN`, unless `T`
/// holds it ([`Tier::HOLDS`]), then the dispatch loop over `frame`
/// until the entry frame returns, asking `T` at the safepoints. `Ok(())` exactly when the entry frame has
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
pub(crate) fn execute<T: Tier, D: Telling>(
    req: &mut Request,
    frame: FrameRef,
) -> Result<(), Fatal> {
    if !T::HOLDS {
        crate::observer::call(req, frame);
    }
    serving::<T, D>(req, frame)
}

/// `zend_call_function` over a user function: the same loop on a frame
/// pushed inside a serving that is already standing, for a native or a
/// destructor calling back into PHP ([`Request::serve_user`]). What its
/// entry frame did not catch stays pending and `Ok(())` says the
/// serving is over, not that the frame returned; the caller reads the
/// pending slot, as php's caller reads `EG(exception)`.
pub(crate) fn reenter<T: Tier, D: Telling>(
    req: &mut Request,
    frame: FrameRef,
) -> Result<(), Fatal> {
    serving::<T, D>(req, frame)
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
pub(crate) fn resume<T: Tier, D: Telling>(req: &mut Request, frame: FrameRef) -> Result<(), Fatal> {
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
    let outcome = serve_frame::<T, D>(req, first);
    crate::observer::loop_left(req, &outcome);
    req.stack_mut().pop_entry(guard);
    outcome
}

/// One serving: the entry slot pushed, the loop, the entry slot popped
/// on the fatal path as on the normal one.
fn serving<T: Tier, D: Telling>(req: &mut Request, frame: FrameRef) -> Result<(), Fatal> {
    let guard = req.stack_mut().push_entry(frame);
    let outcome = serve::<T, D>(req, frame);
    crate::observer::loop_left(req, &outcome);
    req.stack_mut().pop_entry(guard);
    outcome
}

/// The loop itself, with the entry slot standing. `execute` activates
/// its own entry frame whatever the caller is, where php installs its
/// own site (design §4.3 seam 8).
fn serve<T: Tier, D: Telling>(req: &mut Request, entry: FrameRef) -> Result<(), Fatal> {
    if interrupt::check(req)? {
        return serve_frame::<T, D>(req, Stepped::Threw(entry));
    }
    // A throwable raised before any opline of the entry frame ran, where
    // php clears `EG(opline_before_exception)` so that
    // `ZEND_HANDLE_EXCEPTION` looks for no `try` element and frees
    // nothing the frame's opline would have written: the serving is
    // over with the throwable pending, as the unwinder leaves an entry
    // frame it caught nothing in.
    if refused_at_entry(req) {
        // php's `execute_ex` hands the refusal to `ZEND_HANDLE_EXCEPTION`,
        // whose uncaught arm ends what the frame's push began.
        req.vm_mut().stand_at_exception_op(entry, true);
        crate::observer::returned(req, entry, None);
        return Ok(());
    }
    let flow = T::safepoint(req, entry, Site::Entry);
    let first = answered(req, flow, entry)?;
    serve_frame::<T, D>(req, first)
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

/// The loop itself (design §5.8): from the edge its first step took,
/// the oplines of one frame dispatched one after another for as long as
/// each answers on in the frame, and every other edge taken here too, a
/// callee pushed activated and entered, a caller returned to gone on in
/// past its call, a throwable unwound, until the serving is over. The
/// opline the loop stands at is held in hand across the handlers, as
/// php's `execute_ex` keeps `opline` in a register, and read off a frame
/// only where an edge enters it; it is written back only where
/// something outside the loop can read it ([`Next`]). The op array is
/// held in hand the same way ([`held`]): taken up where an edge enters a
/// frame afresh, through a share of its unit the loop keeps across the
/// frames, and, on a call to a function the loop called before and on a
/// return to a frame it entered, the one it kept for that function or
/// frame, with no lookup, without leaving the inner loop. A callee
/// pushed has what is left of its `RECV*` prologue served here, where its
/// op array is in hand and the loop's choice of telling is known (design
/// §5.6), and is then activated.
/// The observers of declared opcodes are told as `D` says: [`Untold`]
/// has nothing to tell and no test, so the loop of an engine where none
/// is declared pays nothing for them (design §5.8, §7.1), in a callee's
/// prologue too.
///
/// The units the loop holds a share of are held until the serving ends
/// or every place is taken ([`held::Units`]); then the loop lets them go
/// and takes the edge up again holding none.
fn serve_frame<T: Tier, D: Telling>(req: &mut Request, first: Stepped) -> Result<(), Fatal> {
    let mut stepped = first;
    loop {
        let units = Units::default();
        match serve_held::<T, D>(req, stepped, &units)? {
            Some(again) => stepped = again,
            None => return Ok(()),
        }
    }
}

/// [`serve_frame`] while `units` has a place for every unit its frames
/// enter: `None` where the serving is over, and the edge it was taking
/// up where every place is taken.
#[inline(always)] // the loop's body, called once a serving
#[allow(clippy::inline_always)]
fn serve_held<'u, T: Tier, D: Telling>(
    req: &mut Request,
    first: Stepped,
    units: &'u Units,
) -> Result<Option<Stepped>, Fatal> {
    let declared = D::of(req);
    let mut kept = Kept::<'u>::default();
    let mut stepped = first;
    loop {
        // An edge the loop takes up the frame of afresh, through the
        // function table or the frame's annex.
        let (mut frame, pushed) = match stepped {
            Stepped::Same(frame) => {
                kept.forget();
                (frame, false)
            }
            Stepped::Popped(frame) | Stepped::Moved(frame) => (frame, false),
            Stepped::Pushed(callee) => (callee, true),
            Stepped::Threw(frame) => {
                // A tier's `Resume` with something raised answers here,
                // and it may have entered frames the loop did not.
                kept.forget();
                let unwound = unwind_frame::<T>(req, frame)?;
                if unwound == Stepped::Threw(frame) {
                    // The entry frame caught nothing: the throwable stays
                    // pending for the caller, the native that re-entered
                    // the loop or `serve` once the main frame is popped.
                    return Ok(None);
                }
                stepped = unwound;
                continue;
            }
            Stepped::Done => return Ok(None),
        };
        let mut body = match units.enter(req, &mut kept, frame) {
            Some(Entered::Held(code)) => code,
            Some(Entered::Full) => return Ok(Some(stepped)),
            None => return Err(no_frame(frame)),
        };
        if pushed {
            let start = req.vm().opline(frame);
            let entry = exec::entry_of(body);
            match entered::<T, D>(req, frame, body, (start, entry), declared)? {
                Stepped::Same(at) if at == frame => {}
                other => {
                    stepped = other;
                    continue;
                }
            }
        }
        let mut oplines = &*body.oplines;
        let mut at = req.vm().opline(frame);
        stepped = loop {
            #[cfg(not(dispatch = "tailcall"))]
            let way = {
                let Some(op) = oplines.get(at as usize) else {
                    return Err(past_the_end(frame, at));
                };
                declared.before(req, frame, at, op.opcode);
                let next = dispatch::dispatch::<T>(req, frame, body, op, at)?;
                #[cfg(debug_assertions)]
                req.sensed_after(body, op)?;
                next.way()
            };
            #[cfg(dispatch = "tailcall")]
            let way = tail::chain::<T, D>(req, frame, body, oplines, at, &declared)?.way();
            match way {
                // `at` is the index of an opline that stands, so it is
                // under the length of an op array, which no 32-byte opline
                // count brings to `u32::MAX`: the add does not wrap.
                Way::Advance => at = at.wrapping_add(1),
                Way::Jump(to) => at = to,
                // A return to a frame the loop entered: its op array as
                // the loop left it for the call.
                Way::Left(stepped @ (Stepped::Popped(to) | Stepped::Moved(to))) => {
                    let Some(code) = kept.returned(to) else {
                        break stepped;
                    };
                    frame = to;
                    body = code;
                    oplines = &*body.oplines;
                    at = req.vm().opline(frame);
                }
                // A call to a function the loop called before: its op
                // array as the loop entered it then.
                Way::Left(stepped @ Stepped::Pushed(callee)) => {
                    let Some((function, start)) = req
                        .vm()
                        .at(callee)
                        .and_then(|header| Some((header.function?, header.opline)))
                    else {
                        break stepped;
                    };
                    let Some(callee_body) = kept.called(function) else {
                        break stepped;
                    };
                    kept.keep(callee, callee_body);
                    let entry = exec::entry_of(callee_body);
                    match entered::<T, D>(req, callee, callee_body, (start, entry), declared)? {
                        Stepped::Same(at) if at == callee => {}
                        other => break other,
                    }
                    frame = callee;
                    body = callee_body;
                    oplines = &*body.oplines;
                    // A prologue served moved the callee on; with none, it
                    // stands where its activation put it.
                    at = if start < entry {
                        req.vm().opline(frame)
                    } else {
                        start
                    };
                }
                Way::Left(stepped) => break stepped,
            }
        };
    }
}

/// A callee just pushed taken up by the loop, standing at `start`: what
/// is left of its `RECV*` prologue served over `code`, the op array the
/// loop holds for it (design §5.6), up to `entry`, its entry safepoint
/// ([`exec::entry_of`]), and then its activation, the interrupt check
/// and `Site::Entry`. `Threw` of the callee where a `RECV` raised.
#[inline(always)] // once per call edge, in the loop
#[allow(clippy::inline_always)]
fn entered<T: Tier, D: Telling>(
    req: &mut Request,
    callee: FrameRef,
    code: &OpArray,
    (start, entry): (u32, u32),
    told: D,
) -> Result<Stepped, Fatal> {
    if start < entry {
        // The parameters php's `RECV` passes on its first test, read off
        // the frame in one run where nothing observes the opcode; the
        // handlers serve the rest from the first that does not pass.
        let at = if told.tells(php_unit::Opcode::Recv) {
            start
        } else {
            exec::recv_passed(req, callee, code, start)
        };
        if at != start {
            req.vm_mut().stand(callee, at);
        }
        if at < entry
            && let Some(threw) = exec::prologue(req, callee, code, told)?
        {
            return Ok(Stepped::Threw(threw));
        }
    }
    activated::<T>(req, callee)
}

/// The internal error for a frame the loop was sent into that serves no
/// op array, which no unit the compiler wrote reaches.
#[cold]
fn no_frame(frame: FrameRef) -> Fatal {
    Fatal::error(format!(
        "InternalError: frame {} is serving no op array",
        frame.0
    ))
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
    let serving = exec::standing(req, frame)?;
    let code = exec::serving_code(&serving, frame)?;
    let at = req.vm().opline(frame);
    let op = exec::opline_at(code, at).ok_or_else(|| past_the_end(frame, at))?;
    let next = crate::handlers::handle_exception::<T>(req, frame, code, op)?;
    Ok(settled(req, frame, at, next))
}

/// Exactly one handler, the one at the frame's opline, with `Nop` at the
/// safepoints; what it did to the frame chain is reported, and a
/// throwable it raised is left pending. `Nop` keeps the stepping host's
/// own `safepoint` from being asked inside its stepping; the observer's
/// begin and end (seam 9) are raised as the loop raises them, at the
/// push a call steps into and the return it steps out of, since their
/// homes take no tier, and the observers of a declared opcode are told
/// of the opline before its handler runs, as the loop tells them.
///
/// A frame the handler pushed is activated as the loop activates it,
/// with `Nop` at its `Site::Entry`: the interrupt is checked there, as
/// php's `ZEND_VM_ENTER_EX` checks it inside `ZEND_DO_UCALL`, so a
/// stepped call meets an interrupt where the loop's does, after its
/// push has begun it, and answers `Threw` of the callee where the
/// interrupt left a throwable pending.
pub fn step(req: &mut Request, frame: FrameRef) -> Result<Stepped, Fatal> {
    let serving = exec::standing(req, frame)?;
    let code = exec::serving_code(&serving, frame)?;
    let at = req.vm().opline(frame);
    let op = exec::opline_at(code, at).ok_or_else(|| past_the_end(frame, at))?;
    crate::observer::tell_before(req, frame, op.opcode);
    let next = dispatch::dispatch::<Nop>(req, frame, code, op, at)?;
    #[cfg(debug_assertions)]
    req.sensed_after(code, op)?;
    match settled(req, frame, at, next) {
        Stepped::Pushed(callee) => {
            let serving = exec::standing(req, callee)?;
            let told = Declared::of(req);
            let code = exec::serving_code(&serving, callee)?;
            let start = req.vm().opline(callee);
            let entry = exec::entry_of(code);
            match entered::<Nop, Declared>(req, callee, code, (start, entry), told)? {
                Stepped::Threw(at) => Ok(Stepped::Threw(at)),
                _ => Ok(Stepped::Pushed(callee)),
            }
        }
        stepped => Ok(stepped),
    }
}

/// `ZEND_HANDLE_EXCEPTION` over one frame, what the loop runs for a
/// [`Stepped::Threw`]: the frame lands at a `catch` clause or a
/// `finally` block and it answers `Same`, or it is left and it
/// answers `Threw` of its caller with the throwable still pending; with
/// nothing pending, it is left as a return of nothing and it answers
/// `Popped` of its caller, which goes on past the call, or `Done` where
/// it is the serving's entry frame, its return value `UNDEF`, which a
/// tier hands back with `Flow::Returned`.
///
/// A frame where nothing takes the throwable is ended for its
/// observers (seam 9) at the unwinder's uncaught arm, as the loop ends
/// it, once, the entry frame too.
///
/// On the entry frame of the serving, which the unwinder does not
/// leave, `Threw` of that frame says nothing took it; a tier hands that
/// frame back with `Flow::Resume` and the loop ends the serving, so one
/// asks [`catches`] first and unwinds only a frame that takes it.
pub fn unwind(req: &mut Request, frame: FrameRef) -> Result<Stepped, Fatal> {
    unwind_frame::<Nop>(req, frame)
}

/// The internal error for a frame whose position is past the end of
/// its op array, which no unit the compiler wrote reaches.
#[cold]
fn past_the_end(frame: FrameRef, at: u32) -> Fatal {
    Fatal::error(format!(
        "InternalError: frame {} stands at opline {at}, past the end of its op array",
        frame.0
    ))
}
