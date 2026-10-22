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

use std::any::Any;
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
/// The compiled tier's compile thread (`jit-design.md` §4.8), an engine's
/// thread as ADR-0010's Threads has them: started through the engine's
/// one home and joined before every fork.
pub use crate::fork::{Parked, Worker};
/// Whether the bundle observes an opcode, which the compiled tier's start
/// asks before it runs (`jit-design.md` §4.3, *Observed opcodes*).
pub use crate::observer::observes;
pub use php_compile::Hooked;
pub use php_heap::{Fatal, FrameRef};

mod crossing;
mod frames;
mod held;
pub(crate) mod published;
mod sampled;
#[cfg(dispatch = "tailcall")]
mod tail;
mod view;
pub use crossing::{at_room, current, free_operands, primed, release, room, stack_room, stand};
pub use frames::{
    Argument, Aside, Called, Compiled, Evaluated, Under, begin, below, call, catches, compile,
    entry, evaluate, evaluate_code, exit, frame, restore, set_argv, set_aside, set_stdin,
};
use held::{Entered, Kept, Scope, Units};
pub use sampled::{
    Building, Sample, building, building_call, class_number, native_returns, numbered_class,
    on_heap, sample, under_way,
};
pub use view::{
    Epoch, FuncRef, Held, MapPtrRaw, UnitView, epoch, epoch_state, frame_cache, func, landing,
    running_func,
};

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
    /// The tier's state on a request (`jit-design.md` §2.2, #3144): its
    /// engine state resolved once, as the request is laid, and what its
    /// visits keep on the request between them, as php's tracing tier
    /// keeps `JIT_G`. Read at one place, [`state`], with no lookup in the
    /// slot table the extensions' states stand in; `()` for a tier that
    /// keeps none.
    type State: Send + 'static;

    /// Asked at the three sites and nowhere else, with `held`, the op
    /// array the loop holds for `frame` (`jit-design.md` §2.2, #3144), so
    /// the tier reads the site's word from what the loop has in hand.
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, held: Held<'_>) -> Flow;

    /// The tier's [`State`](Tier::State) for a request of the engine
    /// `started`, laid with the request where the engine binds the tier's
    /// loop ([`Tier::asked`]): none where it binds `Nop`'s, and by
    /// default.
    #[must_use]
    fn lay(started: &Started<'_>) -> Option<Self::State> {
        let _ = started;
        None
    }

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

    /// Whether the engine `started` asks the tier at its safepoints at
    /// all, asked once as the engine binds its loop, after every engine
    /// state is built: where it answers `false` the engine binds
    /// [`Nop`]'s loop, so a safepoint costs the loop nothing, as php's VM
    /// keeps its own handlers where its compiled tier is not on.
    /// A tier that [`HOLDS`](Tier::HOLDS) or hooks the compile
    /// ([`HOOKED`](Tier::HOOKED)) is bound whatever it answers, since
    /// `Nop`'s loop neither holds a frame nor runs a hooked call as it
    /// does.
    #[must_use]
    fn asked(started: &Started<'_>) -> bool {
        let _ = started;
        true
    }
}

/// The started engine as its tier reads it at [`Tier::asked`]: the
/// engine states its extensions and the tier built (ADR-0005).
#[derive(Clone, Copy)]
pub struct Started<'a> {
    shared: &'a crate::engine::Shared,
}

impl<'a> Started<'a> {
    /// The engine state of type `E`, as [`Request::engine`] reads it:
    /// `None` for a type no extension built.
    #[must_use]
    pub fn engine<E: Send + Sync + 'static>(&self) -> Option<&'a E> {
        (self.shared.state_slots).engine_state(&self.shared.engine_states)
    }
}

/// The interpreter alone: one inlined constant, so the branch on `Flow`
/// folds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Nop;

impl Tier for Nop {
    type State = ();

    #[inline(always)] // jit-design.md §2.2: one constant, so the branch on `Flow` folds
    #[allow(clippy::inline_always)]
    fn safepoint(_: &mut Request, _: FrameRef, _: Site, _: Held<'_>) -> Flow {
        Flow::Continue
    }
}

/// Where a request keeps its tier's [`Tier::State`]
/// (`jit-design.md` §2.2, #3144): laid once with the request, by the
/// loop the engine bound ([`Loops::bound`]), and read by [`state`].
pub(crate) struct TierState(Option<Box<dyn Any + Send>>);

impl TierState {
    /// The tier state of a request of the engine `shared`, as the loop it
    /// bound lays it.
    pub(crate) fn laid(shared: &crate::engine::Shared) -> TierState {
        (shared.loops.lay)(Started { shared })
    }

    /// `T`'s state for a request of the engine `started`.
    fn of<T: Tier>(started: Started<'_>) -> TierState {
        TierState(T::lay(&started).map(|state| Box::new(state) as Box<dyn Any + Send>))
    }
}

/// The tier's state the request was laid with, where it is an `S`
/// ([`Tier::State`]): one check of its type, and no lookup. None where
/// the engine binds a loop that lays none, `Nop`'s.
#[must_use]
pub fn state<S: Any>(req: &Request) -> Option<&S> {
    req.tier.0.as_deref()?.downcast_ref()
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
/// (`jit-design.md` §4.9, revisions 23 and 33; design §5.11,
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

/// What a handler's site, a backward `JMP*` or a `RETURN*`, does with a
/// flow the tier answered other than `Continue`, which the handler goes
/// on past itself, the hot path: what the loop's own site does
/// ([`answered`]), as an edge. The tier stepped past the site's opline
/// before it answered (`jit-design.md` §2.2), the jump taken or the frame
/// popped and its caller landed, so the handler does neither: a `Resume`
/// goes on in the frame it names, after the interrupt check and the
/// pending test, and `Returned` is the serving's end.
#[cold]
#[inline(never)]
pub(crate) fn resumed(req: &mut Request, flow: Flow, frame: FrameRef) -> Result<Next, Fatal> {
    answered(req, flow, frame).map(Next::from)
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

/// The activation of a frame serving `code`, its prologue having
/// raised nothing (design §5.6, §4.3 seam 8): the interrupt check, then
/// `Site::Entry`, then the frame the loop goes on in.
#[inline]
fn activated<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
) -> Result<Stepped, Fatal> {
    if interrupt::check(req)? {
        return Ok(Stepped::Threw(frame));
    }
    let flow = T::safepoint(req, frame, Site::Entry, Held::of(code));
    answered(req, flow, frame)
}

/// One entry of the loop, as the engine binds it.
pub(crate) type Loop = fn(&mut Request, FrameRef) -> Result<(), Fatal>;

/// How an engine binds its tier's loop once its engine states are built
/// and its declared opcodes fixed: [`Loops::bound`] for the tier.
pub(crate) type BindLoops = fn(&crate::engine::Shared) -> Loops;

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
    /// The tier's state a request of the engine is laid with, the bound
    /// tier's ([`Tier::lay`]).
    pub(crate) lay: fn(Started<'_>) -> TierState,
}

impl Loops {
    /// `T`'s loop for the engine `shared` started, or `Nop`'s where `T`
    /// is not asked ([`Tier::asked`]) and neither holds a frame nor
    /// hooks the compile, told as its declared set says
    /// ([`Loops::told`]).
    pub(crate) fn bound<T: Tier>(shared: &crate::engine::Shared) -> Loops {
        if T::HOLDS || T::HOOKED != Hooked::NONE || T::asked(&Started { shared }) {
            Loops::told::<T>(&shared.opcodes)
        } else {
            Loops::told::<Nop>(&shared.opcodes)
        }
    }

    /// The loop of an engine not yet started, which serves nothing
    /// until its start binds the tier's ([`Loops::bound`]).
    pub(crate) fn unstarted() -> Loops {
        Loops::of::<Nop, Untold>()
    }

    /// `T`'s loop for an engine whose declared set is `opcodes`: the
    /// instantiation that tells the set's observers where an extension
    /// declared an opcode, and the one with no test where none did.
    fn told<T: Tier>(opcodes: &ObservedOpcodes) -> Loops {
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
            lay: TierState::of::<T>,
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
///
/// Under detect, a native's own recursion inside the serving is held to
/// php's budget below this frame, and the limit stands where it stood
/// once the serving is over (`php_compile::StackLimit::reentered`): six
/// thousand levels of recursion through `array_map` take several times
/// php's budget on this engine's stack, and the natives that walk a nest
/// at the bottom of it walk it whole, as php's do there (#3142).
///
/// ```php
/// function bottom() {
///     $a = []; for ($i = 0; $i < 200; $i++) { $a = [$a]; }
///     $o = new stdClass; $o->x = [1]; $p = new stdClass; $p->x = [1];
///     return [$o == $p, json_encode($a, 0, 1000) !== false, strlen(serialize($a))];
/// }
/// function m($n) { return $n == 0 ? bottom() : array_map(fn($x) => m($x), [$n - 1])[0]; }
/// var_dump(m(6000));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   bool(true)
///   [1]=>
///   bool(true)
///   [2]=>
///   int(2006)
/// }
/// ```
pub(crate) fn reenter<T: Tier, D: Telling>(
    req: &mut Request,
    frame: FrameRef,
) -> Result<(), Fatal> {
    let reentered = req.stack_reentered();
    let outcome = serving::<T, D>(req, frame);
    req.stack_left(reentered);
    outcome
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
    let reentered = req.stack_reentered();
    if refused_at_entry(req) {
        req.stack_left(reentered);
        req.stack_mut().pop_entry(guard);
        return Ok(());
    }
    let first = if req.is_pending() {
        Stepped::Threw(frame)
    } else {
        Stepped::Same(frame)
    };
    let outcome = serve_frame::<T, D>(req, first);
    req.stack_left(reentered);
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
    // The op array the entry frame serves, held for its site through a
    // share of its unit, once a serving: the loop holds none of it yet.
    let flow = {
        let serving = exec::standing(req, entry)?;
        let code = exec::serving_code(&serving, entry)?;
        T::safepoint(req, entry, Site::Entry, Held::of(code))
    };
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
/// a callback is refused before it ends the process. Under detect the
/// limit here is the whole of the thread, which the host sized so that
/// this recursion, whose level costs a multiple of php's, reaches php's
/// depth (design §5.6, #3043); a native's own recursion is held to
/// php's budget (`php_compile::StackLimit::entry_overflowed`), and the
/// number named is php's either way.
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
///
/// An unbounded recursion through a callback, on the CLI's own stack, is
/// refused the same way at whatever depth that stack allows (#3022):
///
/// ```php
/// function f($n) { return array_map('f', [$n + 1]); }
/// try { f(0); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Error: Maximum call stack size of %d bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?
/// ```
fn refused_at_entry(req: &mut Request) -> bool {
    match crate::api::raising::call_stack_entry(req) {
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
/// or a frame's unit has no place ([`held::Units`]); then the loop gives
/// them back to the request and takes the edge up again holding none,
/// in the places it laid down before. A file's frame whose unit the
/// request compiled for itself, an `eval`'s or an uncached include's, is
/// served in a scope of its own ([`serve_took`]), which gives that unit
/// back as the edge leaves the frame, so the loop around it never lets
/// its own units go for it.
fn serve_frame<T: Tier, D: Telling>(req: &mut Request, first: Stepped) -> Result<(), Fatal> {
    let mut stepped = first;
    let mut units = Units::default();
    loop {
        let served = serve_held::<T, D>(req, stepped, &units, Scope::Whole)?;
        units.give_back(req);
        match served {
            Some(again) => stepped = again,
            None => return Ok(()),
        }
    }
}

/// [`serve_frame`] over the frames of `scope` while `units` has a place
/// for every unit its frames enter: `None` where the serving is over,
/// and the edge it was taking up where every place is taken or where the
/// edge left the scope. One body of the loop, called once a serving and
/// once a scope.
#[inline(always)] // the loop's body, called once a serving and once a scope
#[allow(clippy::inline_always)]
// One function, so the optimiser reads every edge of the loop together:
// the loop out of line, one copy for both callers, measured the call
// edges 3 to 6% higher (#3051).
#[allow(clippy::too_many_lines)]
fn serve_held<'u, T: Tier, D: Telling>(
    req: &mut Request,
    first: Stepped,
    units: &'u Units,
    scope: Scope<'u>,
) -> Result<Option<Stepped>, Fatal> {
    let declared = D::of(req);
    scope.measured(req);
    let mut kept = Kept::<'u>::default();
    let mut stepped = first;
    // Every way out of this loop is taken once a serving, a scope or a
    // restart and is marked cold, so the estimate that places the
    // handlers' calls in line reads the loop as the hot code it is.
    loop {
        // An edge the loop takes up the frame of afresh, through what it
        // kept, the function table or the frame's annex.
        let (mut frame, pushed) = match stepped {
            Stepped::Same(frame)
            | Stepped::Popped(frame)
            | Stepped::Moved(frame)
            | Stepped::Threw(frame)
                if scope.left(frame) =>
            {
                std::hint::cold_path();
                return Ok(Some(stepped));
            }
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
                    std::hint::cold_path();
                    return Ok(None);
                }
                stepped = unwound;
                continue;
            }
            Stepped::Done => {
                std::hint::cold_path();
                return Ok(None);
            }
        };
        // A return to a frame the loop entered, which a scope that served
        // its callee hands back: its op array as the loop left it.
        let kept_body = if pushed { None } else { kept.returned(frame) };
        let mut body = match kept_body.or_else(|| scope.own(&mut kept, frame)) {
            Some(code) => code,
            None => match units.enter(req, &mut kept, frame) {
                Some(Entered::Held(code)) => code,
                Some(Entered::Took(unit, moved)) => {
                    let Some(left) =
                        serve_took::<T, D>(req, (stepped, frame), units, (unit, moved))?
                    else {
                        std::hint::cold_path();
                        return Ok(None);
                    };
                    // A scope that let every unit go with its frames still
                    // standing (`Entered::Full`) hands back an edge into
                    // them, frames this loop did not enter.
                    if edge_frame(left).is_some_and(|at| at.0 >= frame.0) {
                        std::hint::cold_path();
                        kept.forget();
                    }
                    stepped = left;
                    continue;
                }
                Some(Entered::Full) => {
                    std::hint::cold_path();
                    return Ok(Some(stepped));
                }
                None => return Err(no_frame(frame)),
            },
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
                    // Where the callee stands: a prologue served moved it
                    // on, and so did a tier whose `Site::Entry` answered a
                    // `Resume` of it, a trace entered there or a recording
                    // (#3077); the push's `start` is neither's.
                    at = req.vm().opline(frame);
                }
                Way::Left(stepped) => break stepped,
            }
        };
    }
}

/// `frame`, which the edge `first` takes up, a file's whose unit the request compiled
/// for itself, `unit`, and the frames it calls, served in a scope of
/// their own (design §5.8): the scope holds the share of `unit` the loop
/// reads its op array through and keeps the op arrays it enters apart
/// from those the loop around it kept, which it never lets go for it.
/// The scope is over at the first edge to a frame below its own, which
/// it hands back, or where the serving is; the share is given back to
/// the request there ([`Request::give_back`]), so the unit's room is
/// free for the next compile once the frame's own share has gone with
/// it, as php's `zend_leave_helper` frees an `eval`'s op array at its
/// return, and the loop goes on in the frame returned to with the op
/// array it kept for it. An `eval` in a loop compiles each in the room
/// of the one before it and costs the loop around it one scope:
///
/// ```php
/// function f($n) { return $n ? eval('return f($n - 1) + 1;') : 0; }
/// $s = 0;
/// for ($i = 0; $i < 3; $i++) {
///     $s += eval('return ' . $i . ';') + f(3);
///     try { eval('throw new Exception("e" . $i);'); } catch (Exception $e) { echo $e->getMessage(), " "; }
/// }
/// echo $s, "\n";
/// ```
/// ```expect
/// e0 e1 e2 12
/// ```
///
/// A scope takes its own frames of the thread's stack, which php's
/// `eval` does not, so php's limit moves below each by what it takes
/// ([`php_compile::StackLimit::nested`]), and where the thread has no
/// room for another the frame is served in the scope it stands in, as
/// php serves every `eval`. On a fiber's stack the limit is the
/// fiber's floor and cannot move, so there every such frame is served
/// without a scope, its unit given back as the loop takes its edge up
/// afresh (#3192). A recursion through `eval` is php's depth
/// whatever the limit, and past what the thread holds of scopes:
///
/// ```php
/// function f($n) { return $n ? eval('return f($n - 1);') + 1 : 0; }
/// echo f(5000), "\n";
/// ```
/// ```expect regime=small-stack
/// 5000
/// ```
///
/// ```php
/// function f($n) { return $n ? eval('return f($n - 1);') + 1 : 0; }
/// echo f(40000), "\n";
/// ```
/// ```expect
/// 40000
/// ```
///
/// Inside a fiber, a native's own recursion below a thousand nested
/// `eval`s has the whole of the fiber's budget, as php's has: a chain
/// php's `serialize()` takes is taken, and one php refuses is refused.
///
/// ```php
/// function chain($d) { $o = $first = new stdClass; for ($i = 0; $i < $d; $i++) { $o->next = new stdClass; $o = $o->next; } return $first; }
/// function unchain($o) { while ($next = $o->next ?? null) { $o->next = $next->next ?? null; } }
/// function serialized($d) { $c = chain($d); try { serialize($c); return "served"; } catch (Error $e) { return preg_replace('/\d+ bytes/', 'N bytes', $e->getMessage()); } finally { unchain($c); } }
/// function f($n, $d) { return $n ? eval('return f($n - 1, $d);') : serialized($d); }
/// $fiber = new Fiber(function () { echo f(1000, 1336), "\n", f(1000, 100000), "\n"; });
/// $fiber->start();
/// ```
/// ```expect
/// served
/// Maximum call stack size of N bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?
/// ```
#[inline(always)] // once a scope, in the loop's body
#[allow(clippy::inline_always)]
fn serve_took<T: Tier, D: Telling>(
    req: &mut Request,
    (first, frame): (Stepped, FrameRef),
    units: &Units,
    (unit, moved): (php_unit::PublishedUnit, php_compile::Reentered),
) -> Result<Option<Stepped>, Fatal> {
    let scope = Scope::took(frame, &unit, held::here());
    let served = serve_scope::<T, D>(req, first, units, scope);
    req.stack_left(moved);
    req.give_back(unit);
    served
}

/// The frame an edge takes up, none for the serving's end.
fn edge_frame(stepped: Stepped) -> Option<FrameRef> {
    match stepped {
        Stepped::Same(frame)
        | Stepped::Pushed(frame)
        | Stepped::Popped(frame)
        | Stepped::Moved(frame)
        | Stepped::Threw(frame) => Some(frame),
        Stepped::Done => None,
    }
}

/// [`serve_held`] over a scope a frame of a unit the request compiled
/// for itself is served in, out of line.
#[inline(never)]
fn serve_scope<T: Tier, D: Telling>(
    req: &mut Request,
    first: Stepped,
    units: &Units,
    scope: Scope<'_>,
) -> Result<Option<Stepped>, Fatal> {
    serve_held::<T, D>(req, first, units, scope)
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
    activated::<T>(req, callee, code)
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
    match stepped_over(req, frame, code)? {
        Stepped::Pushed(callee) => {
            let serving = exec::standing(req, callee)?;
            let code = exec::serving_code(&serving, callee)?;
            activated_over(req, callee, code)
        }
        stepped => Ok(stepped),
    }
}

/// [`step`] over `held`, the op array a holder holds for `frame`, as
/// compiled code steps an opline of the op array its trace recorded: no
/// share of a unit is taken and the frame's function is not looked up
/// (`jit-design.md` §5.2, #3147). A frame the handler pushed is activated
/// over the op array `epoch`, the epoch the holder holds `held` in,
/// holds for it, and over a share of its unit where the epoch holds none,
/// a request's.
pub fn step_held(
    req: &mut Request,
    frame: FrameRef,
    held: Held<'_>,
    epoch: &Epoch,
) -> Result<Stepped, Fatal> {
    match stepped_over(req, frame, held.code())? {
        Stepped::Pushed(callee) => {
            if let Some(code) = func(req, callee).and_then(|f| epoch.held(f)) {
                return activated_over(req, callee, code.code());
            }
            let serving = exec::standing(req, callee)?;
            let code = exec::serving_code(&serving, callee)?;
            activated_over(req, callee, code)
        }
        stepped => Ok(stepped),
    }
}

/// The `DO_UCALL`, `DO_FCALL` or `DO_FCALL_BY_NAME` at `frame`'s opline
/// of `held`, made as compiled code makes a call to the callee its trace
/// recorded, `callee`, which the `INIT_*` before it pushed: the handler's
/// inline path, php's `i_init_func_execute_data` writing the header's
/// words ([`Request::enter_call`]), and the callee's activation over
/// `callee`, with no dispatch (`jit-design.md` §5.2, #3147). `Pushed`
/// of the callee, or `Threw` of it where its prologue or the interrupt
/// left a throwable pending. Where an observer of the opcode is
/// declared, the opline is another, or the inline path refuses the call,
/// the opline is stepped over `held` as [`step_held`] steps it.
pub fn call_held(
    req: &mut Request,
    frame: FrameRef,
    (held, callee): (Held<'_>, Held<'_>),
    epoch: &Epoch,
) -> Result<Stepped, Fatal> {
    let at = req.vm().opline(frame);
    let op = exec::opline_at(held.code(), at).ok_or_else(|| past_the_end(frame, at))?;
    let flagged = match op.opcode {
        php_unit::Opcode::DoUcall => false,
        php_unit::Opcode::DoFcall | php_unit::Opcode::DoFcallByName => true,
        _ => return step_held(req, frame, held, epoch),
    };
    if Declared::of(req).tells(op.opcode) {
        return step_held(req, frame, held, epoch);
    }
    let returns_to = (op.out_type != php_unit::OperandKind::Unused).then_some(op.out);
    let Some(pushed) = req.enter_call(returns_to, frame, flagged) else {
        return step_held(req, frame, held, epoch);
    };
    // The callee the trace recorded, as its `INIT_*` guarded it; any
    // other is served from its unit.
    if running_func(req, pushed) == Some(callee.func()) {
        return activated_over(req, pushed, callee.code());
    }
    let serving = exec::standing(req, pushed)?;
    let code = exec::serving_code(&serving, pushed)?;
    activated_over(req, pushed, code)
}

/// The one handler at `frame`'s opline over `code`, with `Nop` at the
/// safepoints, the observers told as the loop tells them ([`step`]).
#[inline]
fn stepped_over(req: &mut Request, frame: FrameRef, code: &OpArray) -> Result<Stepped, Fatal> {
    let at = req.vm().opline(frame);
    let op = exec::opline_at(code, at).ok_or_else(|| past_the_end(frame, at))?;
    crate::observer::tell_before(req, frame, op.opcode);
    let next = dispatch::dispatch::<Nop>(req, frame, code, op, at)?;
    #[cfg(debug_assertions)]
    req.sensed_after(code, op)?;
    Ok(settled(req, frame, at, next))
}

/// A callee a step pushed, serving `code`, activated as the loop
/// activates it, with `Nop` at its `Site::Entry` ([`step`]): `Threw` of
/// it where its prologue or the interrupt left a throwable pending,
/// `Pushed` of it otherwise.
#[inline]
fn activated_over(req: &mut Request, callee: FrameRef, code: &OpArray) -> Result<Stepped, Fatal> {
    let told = Declared::of(req);
    let start = req.vm().opline(callee);
    let entry = exec::entry_of(code);
    match entered::<Nop, Declared>(req, callee, code, (start, entry), told)? {
        Stepped::Threw(at) => Ok(Stepped::Threw(at)),
        _ => Ok(Stepped::Pushed(callee)),
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

/// The interrupt check the loop makes at a back edge (design §5.8), for
/// a tier that handles the interrupt where php's compiled code does, in
/// its interrupt stub (`jit_stub_interrupt_handler`), and goes on in its
/// own code: the interrupt cleared, then php's timeout fatal, or each
/// extension's interrupt hook served. `Ok(true)` where a hook left a
/// throwable pending, which the tier hands back to the loop to unwind.
pub fn interrupted(req: &mut Request) -> Result<bool, Fatal> {
    interrupt::check(req)
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
