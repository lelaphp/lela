//! The recorder (`jit-design.md` §4.1): [`run`], one loop over
//! `tier::step` that writes down what the interpreter did from a hot
//! safepoint until php's recorder would have stopped, as a
//! [`TraceRecord`] beside the [`UnitView`] it was read through. Where a
//! recording starts is a [`Start`] and why it stops a [`Stop`], which
//! `policy` settles a claim on and the dump names in php's words.
//!
//! The loop is php's `zend_jit_trace_execute`
//! (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:725-1440`) over this
//! engine's edges: before each opline it samples the three operands and
//! writes one [`Event::Op`]; it stops before stepping where php's does
//! (`CATCH`, `FAST_CALL` and `FAST_RET` are not supported, an
//! `INCLUDE_OR_EVAL` exits to the interpreter, a heap frame is not
//! followed); it steps, and classifies what `step` did ([`Stepped`]) as
//! php classifies `execute_data` changing under it; and at the opline
//! the step left it at it reads the word of the loop header there,
//! closing the loop, linking to a compiled trace, or stopping at an inner
//! loop as php does at a handler whose `trace_flags` say so. The limits
//! are php's: `opcache.jit_max_trace_length` records, ten calls deep,
//! four returns, and `opcache.jit_max_loop_unrolls`,
//! `opcache.jit_max_recursive_calls` and `_returns` as the `Policy` holds
//! them. Where php's bad root cache remembers why a root failed, the
//! word does ([`policy::bad_stop`]).
//!
//! While [`run`] runs, the request records ([`JitRequest`], php's
//! `JIT_G(tracing)`; `jit-design.md` §4.9, revision 30), so a nested
//! `execute` a native, a destructor or a fiber serves does not record:
//! its safepoints bump and do not claim (`dispatch`). A guard clears it
//! on every way out, a fatal, a panic and the unwind that destroys a
//! fiber suspended inside the recording included.

use std::ops::{Deref, DerefMut};

use php_engine::tier::{
    self, Building, Epoch, Fatal, FrameRef, FuncRef, Request, SafepointIdx, Sample, Stepped,
    UnitView,
};
use php_heap::{ZvalType, constant_truth};
use php_unit::{OpArray, Opcode, OperandKind, Opline, OplineIdx};

use crate::JitRequest;
use crate::policy::{self, Holder, Policy, State};
use crate::table::TraceId;
use crate::types::{self, ExitId, Types};
use php_engine::api::abi::EngineState;

#[cfg(test)]
mod tests;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the tests of record and of types read recorded traces back as fixtures"
    )
)]
mod text;

/// Where a recording starts: php's `ZEND_JIT_TRACE_START_LOOP`, `_ENTER`,
/// `_RETURN` and `_SIDE` (`Z/ext/opcache/jit/zend_jit_internal.h:317-320`),
/// a side trace naming the trace and exit it grows from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Start {
    Loop,
    Enter,
    Return,
    Side { parent: TraceId, exit: ExitId },
}

impl Start {
    /// `zend_jit_trace_star_desc`, the word a root's `TRACE start` line
    /// names it by; a side trace's line names its parent instead.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Start::Loop => "loop",
            Start::Enter => "enter",
            Start::Return => "return",
            Start::Side { .. } => "side",
        }
    }
}

impl Start {
    /// For a side trace, the trace and exit it grows from; none for a
    /// root.
    pub(crate) fn grew(self) -> Option<(TraceId, ExitId)> {
        match self {
            Start::Side { parent, exit } => Some((parent, exit)),
            Start::Loop | Start::Enter | Start::Return => None,
        }
    }

    /// The start as a record's text form and a trace's dump write it:
    /// its word, a side trace's as `side <parent>/<exit>`.
    pub(crate) fn written(self) -> String {
        match self {
            Start::Side { parent, exit } => format!("side {}/{}", parent.get(), exit.0),
            other => other.describe().to_owned(),
        }
    }
}

/// An op array as a record's text form and a trace's dump name it: its
/// map-pointer offset, `e` for the engine's and `r` for a request's.
pub(crate) fn func_name(f: FuncRef) -> String {
    match f.offset() {
        php_unit::MapPtrOffset::Engine(n) => format!("e{n}"),
        php_unit::MapPtrOffset::Request(n) => format!("r{n}"),
    }
}

/// Why a recording or a compile stopped: php's `ZEND_JIT_TRACE_STOP`
/// list (`Z/ext/opcache/jit/zend_jit_internal.h:261-293`) with the two of
/// this engine's own, `MovedFrame` and `RequestUnit` (`jit-design.md`
/// §4.1). The first nine end a trace that compiles; the rest are
/// failures, and from `TooManyTraces` on php blacklists at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    Loop,
    RecursiveCall,
    RecursiveRet,
    Return,
    Link(TraceId),
    Interpreter,
    Trampoline,
    PropHookCall,
    BadFunc,
    NotSupported,
    Exception,
    TooLong,
    TooDeep,
    TooDeepRet,
    DeepRecursion,
    LoopUnroll,
    LoopExit,
    RecursionExit,
    BlackList,
    InnerLoop,
    CompiledLoop,
    CompilerError,
    MovedFrame,
    RequestUnit,
    TooManyTraces,
    TooManyChildren,
    TooManyExits,
}

impl Stop {
    /// `zend_jit_trace_stop_description`, the words a `TRACE stop` or
    /// `abort` line gives the stop in; this engine's two in the same
    /// voice. A link is named by its trace, which the line writes itself.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Stop::Loop => "loop",
            Stop::RecursiveCall => "recursive call",
            Stop::RecursiveRet => "recursive return",
            Stop::Return => "return",
            Stop::Link(_) => "link to another trace",
            Stop::Interpreter => "exit to VM interpreter",
            Stop::Trampoline => "trampoline call",
            Stop::PropHookCall => "property hook call",
            Stop::BadFunc => "bad function call",
            Stop::NotSupported => "not supported instructions",
            Stop::Exception => "exception",
            Stop::TooLong => "trace too long",
            Stop::TooDeep => "trace too deep",
            Stop::TooDeepRet => "trace too deep return",
            Stop::DeepRecursion => "deep recursion",
            Stop::LoopUnroll => "loop unroll limit reached",
            Stop::LoopExit => "exit from loop",
            Stop::RecursionExit => "return from recursive function",
            Stop::BlackList => "trace blacklisted",
            Stop::InnerLoop => "inner loop",
            Stop::CompiledLoop => "compiled loop",
            Stop::CompilerError => "JIT compilation error",
            Stop::MovedFrame => "frame moved to the heap",
            Stop::RequestUnit => "root in a request-scoped op array",
            Stop::TooManyTraces => "too many traces",
            Stop::TooManyChildren => "too many side traces",
            Stop::TooManyExits => "too many side exits",
        }
    }

    /// The stop as a record's text form and a trace's dump write it: its
    /// words, a link's as `link <trace>`.
    pub(crate) fn written(self) -> String {
        match self {
            Stop::Link(to) => format!("link {}", to.get()),
            other => other.describe().to_owned(),
        }
    }

    /// `ZEND_JIT_TRACE_STOP_OK`: a recording that stopped here is a trace
    /// to compile; any other is a failed attempt.
    pub(crate) fn compiles(self) -> bool {
        matches!(
            self,
            Stop::Loop
                | Stop::RecursiveCall
                | Stop::RecursiveRet
                | Stop::Return
                | Stop::Link(_)
                | Stop::Interpreter
                | Stop::Trampoline
                | Stop::PropHookCall
                | Stop::BadFunc
        )
    }

    /// php's `p->stop >= ZEND_JIT_TRACE_STOP_LINK` of a trace that
    /// compiles: one that ends where another trace or the interpreter
    /// goes on, whose landings it arms ([`landings`]).
    fn hands_on(self) -> bool {
        matches!(
            self,
            Stop::Link(_)
                | Stop::Interpreter
                | Stop::Trampoline
                | Stop::PropHookCall
                | Stop::BadFunc
        )
    }

    /// `ZEND_JIT_TRACE_STOP_MAY_RECOVER`: a failure the word may try
    /// again after, where the three past it blacklist at once.
    pub(crate) fn recovers(self) -> bool {
        !matches!(
            self,
            Stop::TooManyTraces | Stop::TooManyChildren | Stop::TooManyExits
        )
    }

    /// Every stop but a link, whose trace a code cannot carry, in the
    /// order [`Stop::code`] numbers them.
    const CODED: [Stop; 26] = [
        Stop::Loop,
        Stop::RecursiveCall,
        Stop::RecursiveRet,
        Stop::Return,
        Stop::Interpreter,
        Stop::Trampoline,
        Stop::PropHookCall,
        Stop::BadFunc,
        Stop::NotSupported,
        Stop::Exception,
        Stop::TooLong,
        Stop::TooDeep,
        Stop::TooDeepRet,
        Stop::DeepRecursion,
        Stop::LoopUnroll,
        Stop::LoopExit,
        Stop::RecursionExit,
        Stop::BlackList,
        Stop::InnerLoop,
        Stop::CompiledLoop,
        Stop::CompilerError,
        Stop::MovedFrame,
        Stop::RequestUnit,
        Stop::TooManyTraces,
        Stop::TooManyChildren,
        Stop::TooManyExits,
    ];

    /// The stop as the one byte a cold word keeps after a failed attempt
    /// (`policy::bad_stop`): one past its place among the coded stops, so
    /// zero is none; a link, which never fails an attempt, is zero too.
    pub(crate) fn code(self) -> u8 {
        Stop::CODED
            .iter()
            .position(|coded| *coded == self)
            .and_then(|at| u8::try_from(at + 1).ok())
            .unwrap_or(0)
    }

    /// The stop [`Stop::code`] wrote as `code`; none for zero.
    pub(crate) fn of_code(code: u8) -> Option<Stop> {
        Stop::CODED.get(usize::from(code).checked_sub(1)?).copied()
    }
}

/// One thing the recorder wrote down, php's `zend_jit_trace_rec`
/// (`Z/ext/opcache/jit/zend_jit_internal.h:365-373`) as this engine's
/// edges give it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    /// An opline about to be served, `ZEND_JIT_TRACE_VM`: where it stands and
    /// its three operands sampled, `data` being the `OP_DATA` after an
    /// assignment's; `branch` for a conditional jump, whether it was
    /// taken: the opline's own, or the jump a fused compare-and-branch
    /// serves with it, one handler over two oplines (design §5.8). php
    /// reads where its trace went from the next record and from its
    /// `END`'s opline; this record has no `END`, so a trace that ends on
    /// a jump says which way it went here.
    Op {
        func: FuncRef,
        opline: OplineIdx,
        op1: Option<Sample>,
        op2: Option<Sample>,
        data: Option<Sample>,
        branch: Option<bool>,
    },
    /// A call an `INIT_*` began, `ZEND_JIT_TRACE_INIT_CALL`: its callee,
    /// php's record's `func`, and whether the recording saw it begin.
    InitCall { callee: Callee, began: Began },
    /// A PHP-to-PHP call followed, `ZEND_JIT_TRACE_ENTER`.
    Enter { func: FuncRef },
    /// Its return, `ZEND_JIT_TRACE_BACK`.
    Back,
}

/// The callee of a call a recording wrote down, php's `func` of a
/// `ZEND_JIT_TRACE_INIT_CALL` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Callee {
    /// A user function, by the op array it serves.
    User(FuncRef),
    /// A native, with the types its declaration says it returns, which
    /// php's `zend_jit_trace_build_tssa` types what its `DO_ICALL` writes by
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:2626-2650`); anything where it
    /// declares none.
    Native(Types),
    /// A user function whose op array is the request's own, an `eval`'s
    /// or an uncached include's, which a record never names: its number
    /// is this request's alone, and another request numbers another op
    /// array the same, so nothing a trace compiles may read it (design
    /// §3.4, #3570). php's record names `func` here, which its traces may
    /// embed since they belong to one process.
    Request,
}

impl Callee {
    /// The callee of the call `call` is building, as `req` serves it.
    fn of(req: &Request, call: tier::Building) -> Callee {
        match call.target {
            Some(f) if matches!(f.offset(), php_unit::MapPtrOffset::Request(_)) => Callee::Request,
            Some(f) => Callee::User(f),
            None => Callee::Native(
                tier::native_returns(req, call.call).map_or(Types::ANY, |ty| types::declared(&ty)),
            ),
        }
    }

    /// The op array a user function's call serves; none for a native's.
    pub(crate) fn user(self) -> Option<FuncRef> {
        match self {
            Callee::User(f) => Some(f),
            Callee::Native(_) | Callee::Request => None,
        }
    }
}

/// Where a call under way in a recorded frame began: at an `INIT_*` the
/// recording served, or before the recording reached the frame, php's
/// `ZEND_JIT_TRACE_FAKE_INIT_CALL`, written down as the recording starts
/// in the frame or follows a return into it
/// (`zend_jit_trace_record_fake_init_call`,
/// `Z/ext/opcache/jit/zend_jit_vm_helpers.c:631-682`), so the trace knows
/// the call its sends write and its `DO_*CALL` makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Began {
    /// At an `INIT_*` the recording served.
    Here,
    /// Before the recording reached the frame.
    Before,
}

impl Began {
    /// The word a recorded trace's text form and `lower::dump` write the
    /// call with, php's `>init` and `>fake_init`.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Began::Here => "init",
            Began::Before => "fake-init",
        }
    }
}

/// One recording: where it started, the op array whose word claimed it,
/// what happened, and why it stopped. [`TraceRecord::dump`] and
/// [`TraceRecord::parse`] are inverses, so a recorded trace is a fixture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TraceRecord {
    pub(crate) start: Start,
    pub(crate) root: FuncRef,
    /// For a side trace, what its parent's exit knew of the slots of the
    /// frame it starts in, php's stack map that `zend_jit_trace_build_tssa`
    /// reads a side's entry types from; empty for a root.
    pub(crate) entered: Vec<Entered>,
    /// For a side trace, where its parent's exit left the interpreter;
    /// [`Landed::Named`] for a root.
    pub(crate) landed: Landed,
    pub(crate) events: Vec<Event>,
    pub(crate) stop: Stop,
    /// Where a trace that links hands on, the opline the trace it links
    /// to roots at, php's `ZEND_JIT_TRACE_END` record's opline: what a
    /// return past the trace's lowest frame with no opline recorded after
    /// it is held to land on (`zend_jit_leave_func`'s IP guard,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:11188-11224`). None for any
    /// other stop.
    pub(crate) link_at: Option<(FuncRef, OplineIdx)>,
}

/// Where a side trace's parent exit left the interpreter, php's exit
/// point's `opline`: at the opline the exit names, or where the frame
/// stood, the exit naming none. A side of an exit that names none holds
/// the opline it was recorded from at its first op, php's IP guard
/// (`zend_jit_trace_opline_guard`, `Z/ext/opcache/jit/zend_jit_trace.c:4178-4180`),
/// since the exit leaves the frame wherever it stood.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Landed {
    #[default]
    Named,
    Stood,
}

/// One slot of the frame a side trace starts in whose type its parent's
/// exit knew, php's `STACK_TYPE`: the slot, a variable or a temporary
/// numbered past the variables, and its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entered {
    pub(crate) var: u32,
    pub(crate) tag: ZvalType,
}

/// A recording and the view it was read through, which holds the
/// request's epoch and every op array the recording entered, so the
/// trace can be built on a thread with no request (`jit-design.md` §4.1
/// *The owned view*).
#[derive(Debug)]
pub(crate) struct Recorded {
    pub(crate) record: TraceRecord,
    pub(crate) view: UnitView,
}

/// Where the program stands once the recorder stopped, which is where
/// the interpreter goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ended {
    /// The frame the interpreter goes on in.
    pub(crate) at: FrameRef,
    /// For a stop at an inner loop, that loop's header, where php
    /// records again at once (`ZEND_JIT_TRACE_STOP_REPEAT`).
    pub(crate) inner: Option<(FuncRef, SafepointIdx)>,
    /// Whether anything was stepped: a recording that stopped at its
    /// first opline left the program where the safepoint found it.
    pub(crate) stepped: bool,
    /// Where the trace was rooted once it stopped, where that is not the
    /// claimed word's safepoint: a loop php unrolled and made the trace
    /// of (`zend_jit_trace_subtrace`), rooted at its header.
    pub(crate) rerooted: Option<(FuncRef, SafepointIdx)>,
    /// The compiled trace whose root the recording stopped at, a loop's
    /// header or a call's entry, which php's interpreter, going on there,
    /// enters at once, the trace being that opline's handler.
    pub(crate) stood: Option<TraceId>,
    /// Whether the recording stopped in a frame a call it followed
    /// pushed, at the opline its activation stood it at, where php's
    /// interpreter goes on through the entry's handler.
    pub(crate) activated: bool,
}

/// php's `ZEND_JIT_TRACE_MAX_CALL_DEPTH`: how many calls deep a trace
/// follows.
const MAX_CALL_DEPTH: u32 = 10;
/// php's `ZEND_JIT_TRACE_MAX_RET_DEPTH`: how many returns past its root a
/// return trace follows.
const MAX_RET_DEPTH: u32 = 4;
/// php's `ZEND_JIT_TRACE_START_REC_SIZE`: the records a trace's head
/// takes before its first opline.
const HEAD: u32 = 2;

/// Whether a recording is under way on the request, on whichever
/// execution: php's `JIT_G(tracing)`, which `zend_jit_trace_hot_root`
/// tests before it records (`Z/ext/opcache/jit/zend_jit_trace.c:8140`).
/// A visit reads it through `dispatch::quiet`, beside the trace the
/// request submitted, in one look-up of the request's state; the tests
/// read it here.
///
/// A fiber suspended inside its own recording: `$main`'s recording stops
/// at the `eval`, whose code is no root, so the fiber's `body()` roots;
/// while it stands suspended the request records, and the resumer's hot
/// `work()` roots nothing; destroying the fiber unwinds its recording,
/// and the next `work()` roots (#2999's ruling).
///
/// ```php
/// function body() { Fiber::suspend(1); echo "never\n"; }
/// function work() { return 2; }
/// eval('$f = new Fiber("body"); $f->start(); echo work(), "\n"; $f = null; echo work(), "\n"; exit;');
/// ```
/// ```expect regime=jit-inline process
/// 2
/// 2
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:4
/// ---- TRACE 1 stop (exit to VM interpreter)
/// ---- TRACE 1 compiled
///
/// ---- TRACE 2 start (enter) body() %s:2
/// ---- TRACE 2 abort (exception)
///
/// ---- TRACE 2 start (enter) work() %s:3
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
/// ```
#[cfg(test)]
pub(crate) fn recording(req: &Request) -> bool {
    tier::state::<JitRequest>(req).is_some_and(|state| state.recording.get())
}

/// The request recording for the length of a recording, as php sets
/// `JIT_G(tracing)` around its recorder (`zend_jit_trace.c:8170-8173`),
/// and clear again on drop, so a fatal out of a step, a panic, or the
/// unwind that destroys a fiber suspended inside the recording leaves the
/// request free to record. Holds the request and lends it on.
struct Recording<'r> {
    req: &'r mut Request,
}

impl<'r> Recording<'r> {
    fn set(req: &'r mut Request) -> Recording<'r> {
        if let Some(state) = tier::state::<JitRequest>(req) {
            state.recording.set(true);
        }
        Recording { req }
    }
}

impl Deref for Recording<'_> {
    type Target = Request;

    fn deref(&self) -> &Request {
        self.req
    }
}

impl DerefMut for Recording<'_> {
    fn deref_mut(&mut self) -> &mut Request {
        self.req
    }
}

impl Drop for Recording<'_> {
    fn drop(&mut self) {
        if let Some(state) = tier::state::<JitRequest>(self.req) {
            state.recording.set(false);
        }
    }
}

/// How a recording ended with no record: a fatal a step returned, or
/// one the request raised and is leaving through, php's `zend_bailout`
/// out of the recorder (`zend_jit_trace_execute` never returns), which
/// the loop goes on unwinding from the frame named.
#[derive(Debug)]
pub(crate) enum Bailed {
    Fatal(Fatal),
    Unwinding(FrameRef),
}

/// Record from `frame`, which stands where `start` roots, on the word
/// `claimed` holds: a loop header for `Loop`, the first opline past the
/// prologue for `Enter`, the caller's landing past the call a return
/// landed from for `Return` (`jit-design.md` §2.2, #3036), an exit's
/// landing for a side. `Ok` with the record and where the program
/// stands; `Err` where the request bailed out under it, the recording
/// thrown away.
pub(crate) fn run(
    req: &mut Request,
    frame: FrameRef,
    start: Start,
    claimed: &Holder,
    policy: &Policy,
    opening: Opening,
) -> Result<(Recorded, Ended), Bailed> {
    let Opening {
        entered,
        landed,
        behind,
        ret_depth,
    } = opening;
    let mut req = Recording::set(req);
    let mut view = UnitView::new(&req);
    let held = tier::func(&req, frame).filter(|root| view.hold(&req, *root));
    let Some(root) = held else {
        return Ok((
            refused(&req, frame, start, Stop::RequestUnit),
            ended(frame, false),
        ));
    };
    let mut recorder = Recorder::new(
        policy,
        start,
        tier::epoch(&req),
        Claimed::of(claimed),
        ret_depth,
    );
    let (stop, ended) = recorder.record(&mut req, &mut view, frame, root, behind)?;
    let record = TraceRecord {
        start: recorder.start,
        root: recorder.rerooted.map_or(root, |(func, _)| func),
        // A side trace php rooted at a loop it unrolled is a root.
        entered: if recorder.start.grew().is_some() {
            entered
        } else {
            Vec::new()
        },
        landed: if recorder.start.grew().is_some() {
            landed
        } else {
            Landed::Named
        },
        events: recorder.events,
        stop,
        link_at: matches!(stop, Stop::Link(_))
            .then_some(recorder.link_at)
            .flatten(),
    };
    Ok((Recorded { record, view }, ended))
}

/// What a recording begins with beside its frame: for a side trace,
/// what its parent's exit knew of the frame's slots and where it left
/// the interpreter; for a loop, the condition it begins at behind the
/// frame ([`behind`]).
#[derive(Debug, Default)]
pub(crate) struct Opening {
    pub(crate) entered: Vec<Entered>,
    pub(crate) landed: Landed,
    pub(crate) behind: Option<OplineIdx>,
    /// For a side trace, how many returns past its first frame it follows
    /// ([`ret_depth`]).
    pub(crate) ret_depth: u32,
}

/// How many returns past `frame` a side trace of the loop `root` names
/// follows, php's `ret_depth` (`Z/ext/opcache/jit/zend_jit_trace.c:8566-8585`):
/// where the root is a loop's in another op array, the frames from
/// `frame` down to the nearest that runs it, so a side that starts in a
/// callee the loop's trace inlined goes back to the loop and links to
/// it, where the callee's return would end it; none where no such frame
/// stands within php's four returns.
///
/// php compares `zend_op_array` addresses. `$main` runs in a copy of the
/// cached op array and a closure in its object's, while a trace names
/// the original of either, so neither is ever found: a loop in a file's
/// own code or in a closure gives its sides no depth. Its side stops at
/// the return, and the landing's own trace goes on from there.
///
/// A polymorphic callee of a function's loop, whose side links back to
/// the loop, and of a closure's, whose side stops at the return:
///
/// ```php
/// function poly($x) { if ($x % 2 == 1) { return 1.5; } return 2; }
/// function run($n) { $s = 0; while ($n-- > 0) { $s += (int) poly($n); } return $s; }
/// $c = function ($n) { $s = 0; while ($n-- > 0) { $s += (int) poly($n); } return $s; };
/// echo run(7), " ", $c(7), "\n";
/// ```
/// ```expect regime=jit-inline process capture
/// 11 11
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:4
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) run() %SCRATCH%/witness.php:3
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 5 poly() %SCRATCH%/witness.php:2
/// ---- TRACE 2 start (side trace 1/5) poly() %SCRATCH%/witness.php:2
/// ---- TRACE 2 stop (link to 1)
/// ---- TRACE 2 compiled
///
///      TRACE 1 exit 4 run() %SCRATCH%/witness.php:3
/// ---- TRACE 3 start (side trace 1/4) run() %SCRATCH%/witness.php:3
/// ---- TRACE 3 stop (return)
/// ---- TRACE 3 compiled
///
/// ---- TRACE 4 start (enter) {closure:%SCRATCH%/witness.php:4}() %SCRATCH%/witness.php:4
/// ---- TRACE 4 abort (inner loop)
/// ---- TRACE 4 start (loop) {closure:%SCRATCH%/witness.php:4}() %SCRATCH%/witness.php:4
/// ---- TRACE 4 stop (loop)
/// ---- TRACE 4 compiled
///
///      TRACE 4 exit 5 poly() %SCRATCH%/witness.php:2
/// ---- TRACE 5 start (side trace 4/5) poly() %SCRATCH%/witness.php:2
/// ---- TRACE 5 stop (return)
/// ---- TRACE 5 compiled
///
///      TRACE 4 exit 4 {closure:%SCRATCH%/witness.php:4}() %SCRATCH%/witness.php:4
/// ---- TRACE 6 start (side trace 4/4) {closure:%SCRATCH%/witness.php:4}() %SCRATCH%/witness.php:4
/// ---- TRACE 6 stop (return)
/// ---- TRACE 6 compiled
/// ```
pub(crate) fn ret_depth(
    req: &Request,
    frame: FrameRef,
    (root, at): (FuncRef, OplineIdx),
    folds: Folds,
) -> u32 {
    if tier::func(req, frame) == Some(root) {
        return 0;
    }
    let mut view = UnitView::new(req);
    let copied = !view.hold(req, root) || view.is_main(root) != Some(false) || closure(&view, root);
    if copied || (view.op_array(root)).is_none_or(|code| header_at(code, at, folds).is_none()) {
        return 0;
    }
    let mut below = frame;
    for depth in 1..=MAX_RET_DEPTH {
        let Some(next) = tier::below(req, below) else {
            return 0;
        };
        if tier::func(req, next) == Some(root) {
            return depth;
        }
        below = next;
    }
    0
}

/// Whether the op array `func` names is a closure's body, php's
/// `ZEND_ACC_CLOSURE`, which its compiler names `{closure:...}`; false for
/// one `view` does not hold.
pub(crate) fn closure(view: &UnitView, func: FuncRef) -> bool {
    (view.declaration(func))
        .and_then(|declared| declared.function)
        .is_some_and(|function| function.name.0.starts_with(b"{closure:"))
}

/// The landings a compiled trace arms (`jit-design.md` §2.2, #3036),
/// where php's code generator calls `zend_jit_trace_setup_ret_counter`
/// once the trace's code is generated
/// (`Z/ext/opcache/jit/zend_jit_trace.c:7322-7368`): for a recursive
/// call, the opline past each call into the root's op array; for a
/// trace that ends where another trace or the interpreter goes on, the
/// opline past its last where that is a call, an `include` or an `eval`
/// or a `yield`, and past the call each frame below the one it ends in
/// made; `DO_ICALL`, a native's call, is not among php's. Each as its op
/// array and opline, but one whose `trace_flags` php has set already, a
/// loop's header or an opline the recorder does not serve, which php's
/// arming leaves as it is; none for any other stop.
pub(crate) fn landings(
    record: &TraceRecord,
    view: &UnitView,
    folds: Folds,
) -> Vec<(FuncRef, OplineIdx)> {
    let past = |(func, at): (FuncRef, OplineIdx)| (func, OplineIdx(at.0.saturating_add(1)));
    let mut last = None;
    let mut calls = Vec::new();
    let mut armed = Vec::new();
    for event in &record.events {
        match event {
            Event::Op { func, opline, .. } => last = Some((*func, *opline)),
            Event::Enter { func } => {
                if let Some(call) = last {
                    if record.stop == Stop::RecursiveCall && *func == record.root {
                        armed.push(past(call));
                    }
                    calls.push(call);
                }
            }
            Event::Back => {
                calls.pop();
            }
            Event::InitCall { .. } => {}
        }
    }
    if record.stop.hands_on() {
        let called = last.filter(|(func, at)| {
            (view.op_array(*func))
                .and_then(|code| code.oplines.get(at.0 as usize))
                .is_some_and(|op| lands_past(op.opcode))
        });
        armed.extend(called.map(past));
        armed.extend(calls.into_iter().rev().map(past));
    }
    armed.retain(|(func, at)| {
        (view.op_array(*func)).is_some_and(|code| {
            header_at(code, *at, folds).is_none()
                && (code.oplines.get(at.0 as usize)).is_some_and(|op| !unsupported(op.opcode))
        })
    });
    armed
}

/// Whether a frame goes on past `opcode` from something the opline left
/// it for: php's list where its code generator arms the opline past
/// one (`Z/ext/opcache/jit/zend_jit_trace.c:7322-7368`).
pub(crate) fn lands_past(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::DoUcall
            | Opcode::DoFcall
            | Opcode::DoFcallByName
            | Opcode::Yield
            | Opcode::YieldFrom
            | Opcode::IncludeOrEval
    )
}

/// The root of loop `header` where a return can land there, the opline
/// past a call; none for a loop no return lands at the root of, whose
/// landing word stays unlaid, so an op array no trace returns into
/// allocates none (`jit-design.md` §2.2, §3.2).
pub(crate) fn root_landing(
    code: &OpArray,
    header: SafepointIdx,
    folds: Folds,
) -> Option<OplineIdx> {
    let root = loop_root(code, header, folds)?;
    let before = code
        .oplines
        .get(usize::try_from(root.0.checked_sub(1)?).ok()?)?;
    lands_past(before.opcode).then_some(root)
}

/// The condition a loop's recording begins at behind `frame`, which the
/// loop's back edge left on the body: php's trace roots at the
/// condition of a `for` or a `while` ([`loop_root`]), which the back edge
/// served the moment before, so where the condition is one
/// compare-and-branch over variables and constants, which reads what it
/// read then and writes nothing a later opline reads, it is written down
/// as served, its jump taken back to the body, and the recording goes
/// on at the body, with nothing stepped (`jit-design.md` §4.1, revision
/// 54, as built until a person rules on #3178). None for any other
/// loop, which [`headed`] steps on to its header.
///
/// A loop the back edge first meets after one turn records that turn as
/// php's does, from its condition, where stepping on to the condition
/// first recorded the turn after it, here the loop's exit:
///
/// ```php
/// $g = strlen(...); $s = 0;
/// for ($i = 0; $i < 1; $i++) { $s += $i; }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-inline process
/// 0
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:2
/// ---- TRACE 1 stop (exit to VM interpreter)
/// ---- TRACE 1 compiled
///
/// ---- TRACE 2 start (loop) $main() %s:3
/// ---- TRACE 2 stop (loop)
/// ---- TRACE 2 compiled
///
///      TRACE 2 exit 1 $main() %s:4
/// ---- TRACE 3 start (side trace 2/1) $main() %s:4
/// ---- TRACE 3 stop (return)
/// ---- TRACE 3 compiled
///
/// ```
pub(crate) fn behind(
    req: &Request,
    frame: FrameRef,
    header: SafepointIdx,
    folds: Folds,
) -> Option<OplineIdx> {
    let mut view = UnitView::new(req);
    let code = tier::func(req, frame)
        .filter(|func| view.hold(req, *func))
        .and_then(|func| view.op_array(func))?;
    let body = position(req, frame);
    let root = loop_root(code, header, folds).filter(|root| *root != body)?;
    let compare = code.oplines.get(root.0 as usize)?;
    let jump = code.oplines.get(root.0 as usize + 1)?;
    let read = |kind: OperandKind| matches!(kind, OperandKind::Cv | OperandKind::Const);
    let condition = matches!(
        compare.opcode,
        Opcode::IsSmaller
            | Opcode::IsSmallerOrEqual
            | Opcode::IsEqual
            | Opcode::IsNotEqual
            | Opcode::IsIdentical
            | Opcode::IsNotIdentical
    ) && compare.fuses(jump)
        && jump.jump_target() == Some(body)
        && read(compare.op1_type)
        && read(compare.op2_type);
    condition.then_some(root)
}

/// Where the interpreter stands once a loop's back edge, which left
/// `frame` where it lands, has been stepped on to the opline php's trace
/// of the loop roots at ([`loop_root`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Headed {
    /// It stood there already, and nothing was stepped.
    Stands(FrameRef),
    /// It was stepped there: the rest of the body and, for a `for`, its
    /// step, served by the interpreter's handlers.
    Stepped(FrameRef),
    /// It was stepped and went another way before it got there, a
    /// `break`, a return or a throw: the interpreter goes on at the
    /// frame named.
    Away(FrameRef),
    /// It was not stepped: its op array is not one a trace reads. The
    /// interpreter goes on where it stands.
    Unstepped,
}

/// `frame`, which a back edge of loop `header` left where it lands,
/// stepped on to where php's trace of the loop is rooted and entered:
/// php's heat and its trace stand at the loop's header ([`loop_root`]),
/// the condition of a `for` or a `while` that php's optimizer keeps,
/// which the interpreter reaches past the body
/// the back edge lands on, so a recording starts there and a trace is
/// entered there, as php's handler at that opline is the trace. A call
/// the body makes is stepped through; a fatal is the step's.
pub(crate) fn headed(
    req: &mut Request,
    frame: FrameRef,
    header: SafepointIdx,
    folds: Folds,
) -> Result<Headed, Fatal> {
    let from = position(req, frame);
    let mut view = UnitView::new(req);
    let Some(code) = served_code(req, &mut view, frame) else {
        return Ok(Headed::Unstepped);
    };
    let Some(root) = loop_root(code, header, folds) else {
        return Ok(Headed::Unstepped);
    };
    if root == from {
        return Ok(Headed::Stands(frame));
    }
    let (reached, at) = stepped_on(req, frame, code, |now| {
        if now == root {
            Some(Reached::There)
        } else if now.0 < from.0 || now.0 > root.0 {
            Some(Reached::Away)
        } else {
            None
        }
    })?;
    Ok(match reached {
        Reached::There => Headed::Stepped(at),
        Reached::Away => Headed::Away(at),
    })
}

/// Where a frame a return landed in stands once stepped on toward the
/// header of the loop its landing is in ([`arrived`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arrived {
    /// At the opline php's trace of the loop roots at.
    Header(FrameRef),
    /// At a call the body makes first, which php's interpreter makes,
    /// its entry counted, before the header is reached: not stepped.
    Call(FrameRef),
    /// Somewhere else first, a `break`, a return or a throw, or past
    /// the loop: the interpreter goes on at the frame named.
    Away(FrameRef),
    /// Nothing was stepped: its op array is not one a trace reads.
    Unstepped,
}

/// `frame`, which a return left at its landing in the body of loop
/// `header`, stepped on as php's interpreter runs the body's rest, to
/// the opline php's trace of the loop roots at ([`loop_root`]), short of
/// any call the body makes on the way, whose entry php's interpreter
/// counts. A fatal is the step's.
pub(crate) fn arrived(
    req: &mut Request,
    frame: FrameRef,
    header: SafepointIdx,
    folds: Folds,
) -> Result<Arrived, Fatal> {
    let from = position(req, frame);
    let mut view = UnitView::new(req);
    let Some(code) = served_code(req, &mut view, frame) else {
        return Ok(Arrived::Unstepped);
    };
    let Some(root) = loop_root(code, header, folds) else {
        return Ok(Arrived::Unstepped);
    };
    let calls =
        |now: OplineIdx| (code.oplines.get(now.0 as usize)).is_some_and(|op| is_do_call(op.opcode));
    let (reached, at) = stepped_on(req, frame, code, |now| {
        if now == root || calls(now) {
            Some(Reached::There)
        } else if now.0 < from.0 || now.0 > root.0 {
            Some(Reached::Away)
        } else {
            None
        }
    })?;
    Ok(match reached {
        Reached::There if position(req, at) == root => Arrived::Header(at),
        Reached::There => Arrived::Call(at),
        Reached::Away => Arrived::Away(at),
    })
}

/// The op array `frame` runs, as `view` holds it; none where it runs no
/// op array a trace reads.
fn served_code<'v>(req: &Request, view: &'v mut UnitView, frame: FrameRef) -> Option<&'v OpArray> {
    let func = tier::func(req, frame).filter(|func| view.hold(req, *func))?;
    view.op_array(func)
}

/// `frame`, which stands at the condition loop `header` roots at
/// ([`loop_root`]), stepped through that condition and the jump it ends
/// in, as php's interpreter serves them at the loop's header once the
/// header's handler has counted the visit: the frame where they went,
/// the body, past the loop, or wherever the condition threw or left to.
/// None where nothing is stepped: a loop rooted where its back edge
/// lands, which has no condition before its body, or a frame that does
/// not stand at its condition. The back edge's site, which would count
/// the same visit again, is not asked: a step serves a back edge with
/// `Nop` at its safepoint (`tier::step`).
pub(crate) fn served(
    req: &mut Request,
    frame: FrameRef,
    header: SafepointIdx,
    folds: Folds,
) -> Result<Option<FrameRef>, Fatal> {
    let mut view = UnitView::new(req);
    let Some(code) = served_code(req, &mut view, frame) else {
        return Ok(None);
    };
    let Some(site) = code.safepoints.get(header.0 as usize).copied() else {
        return Ok(None);
    };
    let root = rooted(code, site, folds);
    let end = (code.oplines.iter().enumerate().skip(root.0 as usize))
        .find(|(_, op)| op.jump_target() == Some(site))
        .and_then(|(at, _)| u32::try_from(at).ok());
    let Some(end) = end.filter(|_| root != site && position(req, frame) == root) else {
        return Ok(None);
    };
    let (_, at) = stepped_on(req, frame, code, |now| {
        (now.0 < root.0 || now.0 > end).then_some(Reached::There)
    })?;
    Ok(Some(at))
}

/// Where [`stepped_on`] stopped stepping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reached {
    /// Where it was stepping to.
    There,
    /// Somewhere else first.
    Away,
}

/// `frame` stepped one handler at a time, the calls it makes stepped
/// through, until `reached` says, of the opline it stands at in its own
/// op array `code`, where it is; `Away` where it leaves its serving's
/// entry first, or a step returned past it, moved it or threw. The frame
/// it stopped in beside.
fn stepped_on(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    reached: impl Fn(OplineIdx) -> Option<Reached>,
) -> Result<(Reached, FrameRef), Fatal> {
    let (mut at, mut depth) = (frame, 0u32);
    loop {
        if depth == 0 {
            let now = position(req, at);
            if let Some(reached) = reached(now) {
                return Ok((reached, at));
            }
            let leaving = (code.oplines.get(now.0 as usize))
                .is_some_and(|op| leaves(op.opcode) && at == tier::entry(req));
            if leaving {
                return Ok((Reached::Away, at));
            }
        }
        match tier::step(req, at)? {
            Stepped::Same(next) => at = next,
            Stepped::Pushed(callee) => {
                depth += 1;
                at = callee;
            }
            Stepped::Popped(caller) if depth > 0 => {
                depth -= 1;
                at = caller;
            }
            Stepped::Popped(next) | Stepped::Moved(next) | Stepped::Threw(next) => {
                return Ok((Reached::Away, next));
            }
            Stepped::Done => return Err(stepped_past_entry()),
        }
    }
}

/// A recording refused before its first opline: no event, and `stop`.
pub(crate) fn refused(req: &Request, frame: FrameRef, start: Start, stop: Stop) -> Recorded {
    let record = TraceRecord {
        start,
        root: tier::func(req, frame).unwrap_or(REQUEST_SCOPED),
        entered: Vec::new(),
        landed: Landed::Named,
        events: Vec::new(),
        stop,
        link_at: None,
    };
    Recorded {
        record,
        view: UnitView::new(req),
    }
}

/// The name of an op array no epoch holds, for a recording refused at a
/// frame that names none of its own.
const REQUEST_SCOPED: FuncRef = FuncRef::at(php_unit::MapPtrOffset::Request(0));

/// What the loop keeps between steps, php's locals of
/// `zend_jit_trace_execute`.
struct Recorder<'p> {
    policy: &'p Policy,
    start: Start,
    epoch: Epoch,
    /// The claimed word, whose heat started the recording: php's bad root
    /// cache key.
    claimed: Claimed,
    /// php's `orig_opline`: the op array and opline the trace is rooted
    /// at, which closes a loop and names recursion.
    root: Option<(FuncRef, OplineIdx)>,
    events: Vec<Event>,
    /// php's `idx`: the records the trace holds, its head included.
    length: u32,
    level: u32,
    ret_level: u32,
    /// php's `ret_depth` ([`ret_depth`]).
    ret_depth: u32,
    /// php's `unrolled_calls`: the op array entered at each depth, the
    /// returns past the root first.
    unrolled: Vec<FuncRef>,
    backtrack_recursion: Option<usize>,
    backtrack_ret_recursion: Option<(usize, u32)>,
    backtrack_link_to_enter: Option<(usize, TraceId)>,
    /// php's `last_loop`, `last_loop_opline` and `last_loop_level`.
    last_loop: Option<(usize, FuncRef, OplineIdx, SafepointIdx, u32)>,
    loop_unrolls: u8,
    /// The calls under way whose `DO_*CALL` stops at the interpreter,
    /// php's `ZEND_CALL_MEGAMORPHIC`.
    megamorphic: Vec<FrameRef>,
    rerooted: Option<(FuncRef, SafepointIdx)>,
    stood: Option<TraceId>,
    /// Where the recording linked, [`TraceRecord::link_at`].
    link_at: Option<(FuncRef, OplineIdx)>,
    /// Whether it stopped at a frame's activation, [`Ended::activated`].
    activated: bool,
}

/// The word a recording was claimed on: a root's, by its op array and
/// safepoint, a return root's, by its op array and landing, or an
/// exit's, by its trace and exit.
#[derive(Debug, Clone, Copy)]
enum Claimed {
    Root(FuncRef, SafepointIdx),
    Landing(FuncRef, OplineIdx),
    Exit(TraceId, ExitId),
}

impl Claimed {
    /// Where the word `holder` holds stands.
    fn of(holder: &Holder) -> Claimed {
        match holder {
            Holder::Root { func, at, .. } => Claimed::Root(*func, *at),
            Holder::Landing { func, at, .. } => Claimed::Landing(*func, *at),
            Holder::Exit { trace, exit, .. } => Claimed::Exit(*trace, *exit),
        }
    }
}

/// How one step of the loop ended: on to the next opline, stopped, or
/// stopped at an inner loop's header, which php records again at once.
enum Went {
    On,
    Stopped(Stop),
    Inner(SafepointIdx),
}

impl<'p> Recorder<'p> {
    fn new(
        policy: &'p Policy,
        start: Start,
        epoch: Epoch,
        claimed: Claimed,
        ret_depth: u32,
    ) -> Recorder<'p> {
        Recorder {
            policy,
            start,
            epoch,
            claimed,
            root: None,
            events: Vec::new(),
            length: HEAD,
            level: 0,
            ret_level: 0,
            ret_depth,
            unrolled: Vec::new(),
            backtrack_recursion: None,
            backtrack_ret_recursion: None,
            backtrack_link_to_enter: None,
            last_loop: None,
            loop_unrolls: 0,
            megamorphic: Vec::new(),
            rerooted: None,
            stood: None,
            link_at: None,
            activated: false,
        }
    }

    /// `records` more of php's records, and whether the trace is now too
    /// long (`TRACE_RECORD`'s `idx >= JIT_G(max_trace_length) - 2`).
    fn count(&mut self, records: u32) -> bool {
        self.length = self.length.saturating_add(records);
        self.length >= self.policy.max_trace_length().saturating_sub(2)
    }

    /// php's `zend_jit_trace_bad_stop_event` for the root: the stop the
    /// claimed word last failed on, where it failed `times` times.
    fn root_bad_stop(&self, times: u8) -> Option<Stop> {
        let word = match self.claimed {
            Claimed::Root(func, at) => self.epoch.word(func, at),
            Claimed::Landing(func, at) => self.epoch.landing(func, at),
            Claimed::Exit(trace, exit) => crate::table::exit_word(&self.epoch, trace, exit),
        };
        policy::bad_stop(word, times)
    }

    /// The opline at `at` of `func`, about to be served in `frame`,
    /// written down as one `Op` with its operands sampled, and the native
    /// a frameless call names taken into `view`
    /// ([`UnitView::hold_frameless`]); its `op1`'s sample and how many
    /// class records php's trace adds beside it ([`classes`]).
    fn write_down(
        &mut self,
        req: &Request,
        view: &mut UnitView,
        frame: FrameRef,
        (func, at): (FuncRef, OplineIdx),
        (op, after): (&Opline, Option<&Opline>),
    ) -> (Option<Sample>, u32) {
        let (op1, op2, data) = sampled(req, frame, op, after);
        self.events.push(Event::Op {
            func,
            opline: at,
            op1,
            op2,
            data,
            branch: None,
        });
        if op.opcode.is_frameless_icall() {
            view.hold_frameless(req, op.extended_value);
        }
        (op1, classes(op, op1, op2))
    }

    /// The recording loop from `frame`, rooted in `root`'s op array.
    fn record(
        &mut self,
        req: &mut Recording<'_>,
        view: &mut UnitView,
        mut frame: FrameRef,
        root: FuncRef,
        behind: Option<OplineIdx>,
    ) -> Result<(Stop, Ended), Bailed> {
        let mut func = root;
        let mut stepped = false;
        if tier::on_heap(req, frame) {
            return Ok((Stop::MovedFrame, ended(frame, false)));
        }
        if self.begun(req, view, frame, root, behind)? {
            return Ok((Stop::TooLong, ended(frame, false)));
        }
        loop {
            let at = position(req, frame);
            // Only the root can stand here unheld: every op array the
            // recording goes on into was held as it pushed or landed.
            let Some(code) = view.op_array(func) else {
                return Ok((Stop::RequestUnit, ended(frame, stepped)));
            };
            let Some(op) = code.oplines.get(at.0 as usize).copied() else {
                return Err(Bailed::Fatal(off_the_end(at)));
            };
            let after = code.oplines.get(at.0 as usize + 1).copied();
            // php checks a trace's first opline before it records it, and
            // records every later one before it stops at it; behind a
            // condition, the condition is the first.
            if !stepped && behind.is_none() && unsupported(op.opcode) {
                return Ok((Stop::NotSupported, ended(frame, false)));
            }
            let (op1, classes) =
                self.write_down(req, view, frame, (func, at), (&op, after.as_ref()));
            if self.count(1 + classes) {
                return Ok((Stop::TooLong, ended(frame, stepped)));
            }
            if unsupported(op.opcode) {
                return Ok((Stop::NotSupported, ended(frame, stepped)));
            }
            if matches!(op.opcode, Opcode::IncludeOrEval | Opcode::CallableConvert) {
                return Ok((Stop::Interpreter, ended(frame, stepped)));
            }
            let call = tier::building(req);
            if is_do_call(op.opcode) {
                if call.is_some_and(|call| self.megamorphic.contains(&call.call)) {
                    return Ok((Stop::Interpreter, ended(frame, stepped)));
                }
                // php's `ZEND_JIT_TRACE_DO_ICALL` record for a native's call.
                if call.is_some_and(|call| call.target.is_none()) && self.count(1) {
                    return Ok((Stop::TooLong, ended(frame, stepped)));
                }
            }
            // The entry-frame rule (`jit-design.md` §4.1): the caller of
            // the serving's entry frame is native, so its return ends the
            // trace before it is stepped, and `Done` is never stepped into.
            if leaves(op.opcode) && frame == tier::entry(req) {
                return Ok((Stop::Return, ended(frame, stepped)));
            }
            let edge = tier::step(req, frame).map_err(Bailed::Fatal)?;
            stepped = true;
            let went = match edge {
                Stepped::Same(next) => {
                    placed(req, view, (frame, next), (&op, at), op1);
                    frame = next;
                    self.same(req, view, frame, (&op, at, after.as_ref()), call)
                }
                Stepped::Pushed(callee) => {
                    frame = callee;
                    self.activating(req, view, callee, &mut func)
                }
                Stepped::Popped(caller) => {
                    frame = caller;
                    self.popped(req, view, caller, &mut func)
                }
                Stepped::Moved(caller) => {
                    frame = caller;
                    Went::Stopped(Stop::MovedFrame)
                }
                Stepped::Threw(at) if req.diagnostics().bailing_out() => {
                    return Err(Bailed::Unwinding(at));
                }
                Stepped::Threw(at) => {
                    frame = at;
                    Went::Stopped(Stop::Exception)
                }
                Stepped::Done => return Err(Bailed::Fatal(stepped_past_entry())),
            };
            let went = match went {
                Went::On => self.landed(req, view, frame, func),
                other => other,
            };
            match went {
                Went::On => {}
                Went::Stopped(stop) => {
                    let stop = self.backtracked(stop);
                    return Ok((stop, self.ended(frame)));
                }
                Went::Inner(header) => {
                    let stop = self.backtracked(Stop::InnerLoop);
                    let mut ended = self.ended(frame);
                    if stop == Stop::InnerLoop {
                        ended.inner = Some((func, header));
                    }
                    return Ok((stop, ended));
                }
            }
        }
    }

    /// The trace's root, `frame`'s opline in `root`, or the condition
    /// `behind` it, the fused compare a loop's back edge served before it
    /// left `frame` on the body ([`behind`]), which is written down as
    /// served: its operands as the frame holds them, its jump taken to
    /// the body. Whether that made the trace too long.
    fn begun(
        &mut self,
        req: &Request,
        view: &mut UnitView,
        frame: FrameRef,
        root: FuncRef,
        behind: Option<OplineIdx>,
    ) -> Result<bool, Bailed> {
        self.root = Some((root, behind.unwrap_or_else(|| position(req, frame))));
        self.under_way(req, frame);
        let Some(condition) = behind else {
            return Ok(false);
        };
        let op = (view.op_array(root))
            .and_then(|code| code.oplines.get(condition.0 as usize).copied())
            .ok_or_else(|| Bailed::Fatal(off_the_end(condition)))?;
        let (_, classes) = self.write_down(req, view, frame, (root, condition), (&op, None));
        if let Some(Event::Op { branch, .. }) = self.events.last_mut() {
            *branch = Some(true);
        }
        Ok(self.count(1 + classes))
    }

    /// The calls `frame` began before the recording reached it, written
    /// down as php's fake inits, the outermost first, as php's recorder
    /// writes them where its recording starts and where it follows a
    /// return past its root (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:803-812`,
    /// `:1189-1196`). php stops at none of them for the trace's length,
    /// and neither does this: the next record stops it. A return trace
    /// rooted inside an argument of an outer call so knows the call its
    /// send writes and its `DO_ICALL` makes, and compiles where php's
    /// does (the number of the exit the side trace grows from is
    /// #3203's):
    ///
    /// ```php
    /// function c($x) { $s = 0; for ($i = 0; $i < 3; $i++) { $s += $x; } return $s; }
    /// function g($v) { printf("%d\n", c($v)); return 1; }
    /// g(5); g(6); g(7); g(8);
    /// ```
    /// ```expect regime=jit-inline process
    /// 15
    /// 18
    /// 21
    /// 24
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:4
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) c() %s:2
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit %d c() %s:2
    /// ---- TRACE 2 start (side trace 1/%d) c() %s:2
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    ///
    /// ---- TRACE 3 start (enter) g() %s:3
    /// ---- TRACE 3 stop (link to 1)
    /// ---- TRACE 3 compiled
    ///
    /// ---- TRACE 4 start (return) g() %s:3
    /// ---- TRACE 4 stop (return)
    /// ---- TRACE 4 compiled
    ///
    /// ```
    fn under_way(&mut self, req: &Request, frame: FrameRef) {
        for call in tier::under_way(req, frame) {
            self.events.push(Event::InitCall {
                callee: Callee::of(req, call),
                began: Began::Before,
            });
            if self.count(1) {
                break;
            }
        }
    }

    /// Where a recording that stepped stopped.
    fn ended(&self, at: FrameRef) -> Ended {
        Ended {
            rerooted: self.rerooted,
            stood: self.stood,
            activated: self.activated,
            ..ended(at, true)
        }
    }

    /// A step that went on in a frame: the direction of a conditional
    /// jump, or of the fused compare-and-branch the opline began,
    /// written on its `Op`, and a call an `INIT_*` began written down,
    /// a plain one taken into `view` for compiled code to write in place
    /// ([`UnitView::hold_call`]), with the pair a method's call wrote into
    /// its slot ([`UnitView::hold_callee`]).
    fn same(
        &mut self,
        req: &Request,
        view: &mut UnitView,
        frame: FrameRef,
        (op, at, after): (&Opline, OplineIdx, Option<&Opline>),
        before: Option<Building>,
    ) -> Went {
        let now = position(req, frame);
        let jump = after
            .filter(|jump| op.fuses(jump) && now.0 != at.0.wrapping_add(1))
            .or_else(|| conditional(op).then_some(op));
        if let Some(jump) = jump
            && let Some(Event::Op { branch, .. }) = self.events.last_mut()
        {
            *branch = Some(jump.jump_target() == Some(now));
        }
        if is_do_call(op.opcode) {
            if let Some(call) = before {
                self.megamorphic.retain(|frame| *frame != call.call);
            }
            return Went::On;
        }
        let Some(call) = tier::building(req) else {
            return Went::On;
        };
        if before.map(|b| b.call) == Some(call.call) {
            return Went::On;
        }
        // `zend_jit_may_be_polymorphic_call` under
        // `opcache.jit_max_polymorphic_calls=0`.
        if self.policy.max_polymorphic_calls() == 0 && call.target.is_some() && polymorphic(op) {
            self.megamorphic.push(call.call);
        }
        let callee = Callee::of(req, call);
        view.hold_call(req, call.call);
        // The pair a method's call over constant names wrote into its
        // slot, which compiled code holds the request's slot to before it
        // pushes the call in place.
        if matches!(
            op.opcode,
            Opcode::InitMethodCall | Opcode::InitStaticMethodCall
        ) {
            view.hold_callee(req, frame, at);
        }
        self.events.push(Event::InitCall {
            callee,
            began: Began::Here,
        });
        if self.count(1) {
            return Went::Stopped(Stop::TooLong);
        }
        Went::On
    }

    /// [`Recorder::pushed`], the recording's stop there kept as one at
    /// the callee's activation ([`Ended::activated`]).
    fn activating(
        &mut self,
        req: &Request,
        view: &mut UnitView,
        callee: FrameRef,
        func: &mut FuncRef,
    ) -> Went {
        let went = self.pushed(req, view, callee, func);
        self.activated = matches!(went, Went::Stopped(_));
        went
    }

    /// A call followed into `callee`, php's "Enter into function": the
    /// op array held by the view, then php's depth and recursion limits,
    /// then the callee's entry word. A request-scoped op array cannot be
    /// held, and the recording stops at it with `Interpreter`, as php's
    /// `zend_jit_trace_execute` stops at a frame whose op array keeps no
    /// `jit_extension` (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:1097-1109`):
    /// the trace leaves at the `DO_*CALL`, whose handler activates the
    /// callee, and arms the landing past it ([`landings`]; #3570). The
    /// loop's trace compiles and stops at the call, and the return trace
    /// rooted at the landing past it links back to the loop's:
    ///
    /// ```php
    /// eval('function h($a) { return $a + 1; } $declared = true;');
    /// function f($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += h($i); } return $t; }
    /// echo f(100), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 5050
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:2
    /// ---- TRACE 1 stop (exit to VM interpreter)
    /// ---- TRACE 1 compiled
    ///
    /// ---- TRACE 2 start (return) $main() %SCRATCH%/witness.php:4
    /// ---- TRACE 2 abort (inner loop)
    /// ---- TRACE 2 start (loop) f() %SCRATCH%/witness.php:3
    /// ---- TRACE 2 stop (exit to VM interpreter)
    /// ---- TRACE 2 compiled
    ///
    /// ---- TRACE 3 start (return) f() %SCRATCH%/witness.php:3
    /// ---- TRACE 3 stop (link to 2)
    /// ---- TRACE 3 compiled
    ///
    ///      TRACE 2 exit 2 f() %SCRATCH%/witness.php:3
    /// ---- TRACE 4 start (side trace 2/2) f() %SCRATCH%/witness.php:3
    /// ---- TRACE 4 stop (return)
    /// ---- TRACE 4 compiled
    /// ```
    ///
    /// A call into a property hook's body stops with `PropHookCall`, as
    /// php's recorder stops at a callee whose op array has `prop_info`
    /// (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:1129-1132`; #3577): the
    /// child's hook's trace stops at its call to its parent's, and the
    /// return trace past the call starts in the child's:
    ///
    /// ```php
    /// class P { public int $x = 2 { get => $this->x + 1; } }
    /// class C extends P {
    ///     public int $x = 2 {
    ///         get => parent::$x::get() * 2;
    ///     }
    /// }
    /// function f($c, $n) { $s = 0; while ($n-- > 0) { $s += $c->x; } return $s; }
    /// echo f(new C, 5), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 30
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:9
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %SCRATCH%/witness.php:8
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    /// ---- TRACE 2 start (enter) C::$x::get() %SCRATCH%/witness.php:5
    /// ---- TRACE 2 stop (property hook call)
    /// ---- TRACE 2 compiled
    ///
    /// ---- TRACE 3 start (enter) P::$x::get() %SCRATCH%/witness.php:2
    /// ---- TRACE 3 stop (return)
    /// ---- TRACE 3 compiled
    ///
    /// ---- TRACE 4 start (return) C::$x::get() %SCRATCH%/witness.php:5
    /// ---- TRACE 4 stop (return)
    /// ---- TRACE 4 compiled
    ///
    ///      TRACE 1 exit 4 f() %SCRATCH%/witness.php:8
    /// ---- TRACE 5 start (side trace 1/4) f() %SCRATCH%/witness.php:8
    /// ---- TRACE 5 stop (return)
    /// ---- TRACE 5 compiled
    /// ```
    ///
    /// Calls into an `eval`'d function, one taking its argument by
    /// reference, and an evaluated class's method, from a loop whose trace
    /// stops at each, under force and where every guard fails once:
    ///
    /// ```php
    /// eval('function h($a) { return $a + 1; } function r(&$a, $b = 10) { $a = $a * 2; return $a + $b; }');
    /// eval('class Evald { public $b = 3; function get($v) { return $this->b * $v; } }');
    /// function f($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $a = $i; $t += h($a) + r($a) + $a; } return $t; }
    /// function g($o, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o->get($i); } return $t; }
    /// echo f(30), " ", g(new Evald, 30), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 2505 1305
    /// ```
    ///
    /// ```php
    /// eval('function h($a) { return $a + 1; } function r(&$a, $b = 10) { $a = $a * 2; return $a + $b; }');
    /// eval('class Evald { public $b = 3; function get($v) { return $this->b * $v; } }');
    /// function f($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $a = $i; $t += h($a) + r($a) + $a; } return $t; }
    /// function g($o, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o->get($i); } return $t; }
    /// echo f(30), " ", g(new Evald, 30), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 2505 1305
    /// ```
    fn pushed(
        &mut self,
        req: &Request,
        view: &mut UnitView,
        callee: FrameRef,
        func: &mut FuncRef,
    ) -> Went {
        let Some(entered) = tier::func(req, callee).filter(|f| view.hold(req, *f)) else {
            return Went::Stopped(Stop::Interpreter);
        };
        if self.level > MAX_CALL_DEPTH {
            return Went::Stopped(Stop::TooDeep);
        }
        if view.is_hook(entered) == Some(true) {
            return Went::Stopped(Stop::PropHookCall);
        }
        self.events.push(Event::Enter { func: entered });
        if self.count(1) {
            return Went::Stopped(Stop::TooLong);
        }
        let count = self.recursive_calls(entered);
        let most = u32::from(self.policy.max_recursive_calls());
        if self.root == Some((entered, position(req, callee))) {
            if count + 1 >= most {
                return Went::Stopped(Stop::RecursiveCall);
            }
            self.backtrack_recursion = Some(self.events.len());
        } else if count >= most {
            return Went::Stopped(Stop::DeepRecursion);
        }
        self.unroll_at(self.ret_level + self.level, entered);
        self.level += 1;
        *func = entered;
        // The callee's entry word, php's `trace_flags` of the opline the
        // call landed on.
        match policy::read(self.epoch.word(entered, SafepointIdx::ENTRY)) {
            State::Compiled(id) if self.start == Start::Return => {
                if self.backtrack_link_to_enter.is_none() {
                    self.backtrack_link_to_enter = Some((self.events.len(), id));
                }
                Went::On
            }
            State::Compiled(id) => {
                self.stood = Some(id);
                self.link_at = Some((entered, position(req, callee)));
                Went::Stopped(Stop::Link(id))
            }
            State::Blacklisted => Went::Stopped(Stop::BlackList),
            State::Cold | State::Recording => Went::On,
        }
    }

    /// A return to `caller`, php's "Return from function": at depth zero
    /// the end of a loop or entry trace, or a return php follows past a
    /// return trace's root; deeper, one level back. The landing it
    /// returned to is read where every opline is ([`Recorder::landed`]).
    ///
    /// A trace in a closure stops at its `RETURN` here, unexecuted, and
    /// compiles it: the return into the file's loop is made in the trace,
    /// with its closure given up, and whatever guard fails on the way out:
    ///
    /// ```php
    /// function f($x) { if ($x % 3 == 0) { return 1.5; } if ($x % 3 == 1) { return 2; } return true; }
    /// $s = 0; $h = function ($i) { return f($i); }; $k = fn ($i) => "a" . f($i);
    /// for ($i = 0; $i < 300; $i++) { $s += (int) $h($i); $s += strlen($k($i)); }
    /// echo $s, "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 1200
    /// ```
    ///
    /// ```php
    /// function f($x) { if ($x % 3 == 0) { return 1.5; } if ($x % 3 == 1) { return 2; } return true; }
    /// $s = 0; $h = function ($i) { return f($i); }; $k = fn ($i) => "a" . f($i);
    /// for ($i = 0; $i < 300; $i++) { $s += (int) $h($i); $s += strlen($k($i)); }
    /// echo $s, "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 1200
    /// ```
    fn popped(
        &mut self,
        req: &Request,
        view: &mut UnitView,
        caller: FrameRef,
        func: &mut FuncRef,
    ) -> Went {
        if self.level == 0 {
            let limit = self.policy.blacklist_root().saturating_sub(1);
            match self.start {
                Start::Return
                    if self.policy.max_recursive_returns() > 0
                        && self.has_recursive_ret(req, caller) =>
                {
                    if let Some(stop) = self.followed_return(req, view, caller, func) {
                        return Went::Stopped(stop);
                    }
                }
                Start::Loop if self.root_bad_stop(limit) != Some(Stop::LoopExit) => {
                    return Went::Stopped(Stop::LoopExit);
                }
                Start::Enter
                    if recursion_exits(req, caller)
                        && self.root_bad_stop(limit) != Some(Stop::RecursionExit) =>
                {
                    return Went::Stopped(Stop::RecursionExit);
                }
                Start::Side { .. } if self.ret_level < self.ret_depth => {
                    let Some(landed) = tier::func(req, caller).filter(|f| view.hold(req, *f))
                    else {
                        return Went::Stopped(Stop::Return);
                    };
                    self.events.push(Event::Back);
                    if self.count(1) {
                        return Went::Stopped(Stop::TooLong);
                    }
                    self.ret_level += 1;
                    self.last_loop = None;
                    *func = landed;
                    self.under_way(req, caller);
                }
                _ => return Went::Stopped(Stop::Return),
            }
        } else {
            self.level -= 1;
            if self.last_loop.is_some_and(|(.., level)| self.level < level) {
                self.last_loop = None;
            }
            self.events.push(Event::Back);
            if self.count(1) {
                return Went::Stopped(Stop::TooLong);
            }
            // A caller the recording entered serves an op array the view
            // holds; where it serves none, php's branch for a frame whose
            // op array keeps no `jit_extension`, its caller not the frame
            // it left, stops with `Return`
            // (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:1097-1109`).
            let Some(landed) = tier::func(req, caller) else {
                return Went::Stopped(Stop::Return);
            };
            *func = landed;
        }
        Went::On
    }

    /// A return trace's return past its root that php follows, its
    /// recursion counted; the stop where it may not. A return into a
    /// request-scoped op array stops with `Return`, on php's branch for a
    /// frame whose op array keeps no `jit_extension`, its caller not the
    /// frame it left (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:1097-1109`;
    /// #3570).
    fn followed_return(
        &mut self,
        req: &Request,
        view: &mut UnitView,
        caller: FrameRef,
        func: &mut FuncRef,
    ) -> Option<Stop> {
        if self.ret_level > MAX_RET_DEPTH {
            return Some(Stop::TooDeepRet);
        }
        let Some(landed) = tier::func(req, caller).filter(|f| view.hold(req, *f)) else {
            return Some(Stop::Return);
        };
        self.events.push(Event::Back);
        if self.count(1) {
            return Some(Stop::TooLong);
        }
        let count = self.recursive_returns(landed);
        let most = u32::from(self.policy.max_recursive_returns());
        if self.root == Some((landed, position(req, caller))) {
            if count + 1 >= most {
                return Some(Stop::RecursiveRet);
            }
            self.backtrack_ret_recursion = Some((self.events.len(), self.ret_level));
        } else if count >= most {
            return Some(Stop::DeepRecursion);
        }
        self.unroll_at(self.ret_level, landed);
        self.ret_level += 1;
        self.last_loop = None;
        *func = landed;
        self.under_way(req, caller);
        None
    }

    /// At the opline a step left the recording at: the word of the loop
    /// header there, where there is one, read as php reads a handler's
    /// `trace_flags` (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:1296-1375`).
    fn landed(&mut self, req: &Request, view: &UnitView, frame: FrameRef, func: FuncRef) -> Went {
        let at = position(req, frame);
        let Some(header) =
            (view.op_array(func)).and_then(|code| header_at(code, at, self.policy.folds()))
        else {
            return self.return_landing(func, at);
        };
        let depth = self.level + self.ret_level;
        let limit = self.policy.blacklist_root().saturating_sub(1);
        match policy::read(self.epoch.word(func, header)) {
            State::Compiled(id) => {
                self.stood = Some(id);
                self.link_at = Some((func, at));
                let compiled_loop = self.start == Start::Loop
                    && depth == 0
                    && self.root_bad_stop(limit) != Some(Stop::CompiledLoop);
                Went::Stopped(if compiled_loop {
                    Stop::CompiledLoop
                } else {
                    Stop::Link(id)
                })
            }
            State::Blacklisted => Went::Stopped(Stop::BlackList),
            State::Cold | State::Recording => self.loop_header(func, at, header),
        }
    }

    /// An armed return landing the recording reached, read as php reads
    /// its `trace_flags` at every opline it records
    /// (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:1294-1325`): a return
    /// trace compiled there is linked to, and one blacklisted stops the
    /// recording.
    fn return_landing(&mut self, func: FuncRef, at: OplineIdx) -> Went {
        let landing = self.epoch.landing(func, at);
        if !policy::armed(landing) {
            return Went::On;
        }
        match policy::read(landing) {
            State::Compiled(id) => {
                self.stood = Some(id);
                self.link_at = Some((func, at));
                Went::Stopped(Stop::Link(id))
            }
            State::Blacklisted => Went::Stopped(Stop::BlackList),
            State::Cold | State::Recording => Went::On,
        }
    }

    /// A loop header that is no compiled trace's: the root's closes the
    /// loop; another is an inner loop php records on its own, unless
    /// attempts at it failed, when it is unrolled up to php's limit and
    /// a second visit at one depth makes the unrolled loop the trace.
    fn loop_header(&mut self, func: FuncRef, at: OplineIdx, header: SafepointIdx) -> Went {
        let side = self.start.grew().is_some();
        let depth = self.level + self.ret_level;
        if !side && self.root == Some((func, at)) && depth == 0 {
            return Went::Stopped(Stop::Loop);
        }
        let half = self.policy.blacklist_root() / 2;
        if !side || depth != 0 {
            let bad = policy::bad_stop(self.epoch.word(func, header), half);
            if bad != Some(Stop::InnerLoop)
                && bad != Some(Stop::LoopExit)
                && (side || self.root_bad_stop(half) != Some(Stop::InnerLoop))
            {
                return Went::Inner(header);
            }
        }
        match self.last_loop {
            Some((from, f, opline, number, level))
                if f == func && opline == at && level == self.level =>
            {
                // php's `zend_jit_trace_subtrace`: the unrolled loop is the
                // trace, rooted at its header.
                self.events.drain(..from);
                self.start = Start::Loop;
                self.root = Some((func, at));
                self.rerooted = Some((func, number));
                self.ret_level = 0;
                Went::Stopped(Stop::Loop)
            }
            _ if self.loop_unrolls < self.policy.max_loop_unrolls() => {
                self.last_loop = Some((self.events.len(), func, at, header, self.level));
                self.loop_unrolls += 1;
                Went::On
            }
            _ => Went::Stopped(Stop::LoopUnroll),
        }
    }

    /// php's fall back past a failed recording ([`Recorder::fallen_back`]),
    /// where a stop it falls back to links at no place the recording
    /// stood at.
    fn backtracked(&mut self, stop: Stop) -> Stop {
        let fallen = self.fallen_back(stop);
        if fallen != stop {
            self.link_at = None;
        }
        fallen
    }

    /// php's fall back past a failed recording: to the recursive call or
    /// return it saw, or to the compiled entry it would have linked to.
    fn fallen_back(&mut self, stop: Stop) -> Stop {
        if stop.compiles() {
            return stop;
        }
        if let Some(at) = self.backtrack_recursion {
            self.events.truncate(at);
            return Stop::RecursiveCall;
        }
        if let Some((at, ret_level)) = self.backtrack_ret_recursion {
            self.events.truncate(at);
            self.ret_level = ret_level;
            return Stop::RecursiveRet;
        }
        if let Some((at, id)) = self.backtrack_link_to_enter {
            let half = self.policy.blacklist_root() / 2;
            if stop == Stop::DeepRecursion && self.root_bad_stop(half) == Some(Stop::DeepRecursion)
            {
                self.events.truncate(at);
                return Stop::Link(id);
            }
        }
        stop
    }

    /// php's `zend_jit_trace_recursive_call_count`.
    fn recursive_calls(&self, func: FuncRef) -> u32 {
        let (from, to) = (
            self.ret_level as usize,
            (self.ret_level + self.level) as usize,
        );
        count_of(self.unrolled.get(from..to), func)
    }

    /// php's `zend_jit_trace_recursive_ret_count`.
    fn recursive_returns(&self, func: FuncRef) -> u32 {
        count_of(self.unrolled.get(..self.ret_level as usize), func)
    }

    /// `unrolled_calls[at] = func`.
    fn unroll_at(&mut self, at: u32, func: FuncRef) {
        let at = at as usize;
        if self.unrolled.len() <= at {
            self.unrolled.resize(at + 1, func);
        }
        if let Some(slot) = self.unrolled.get_mut(at) {
            *slot = func;
        }
    }

    /// php's `zend_jit_trace_has_recursive_ret`: whether, from `caller`
    /// down, a frame of the root's op array waits on a call that returns
    /// to the root's opline.
    fn has_recursive_ret(&self, req: &Request, caller: FrameRef) -> bool {
        let Some((root, at)) = self.root else {
            return false;
        };
        let mut level = self.ret_level;
        let mut frame = Some(caller);
        while let Some(at_frame) = frame
            && level < MAX_RET_DEPTH
        {
            if tier::func(req, at_frame) == Some(root)
                && position(req, at_frame).0.wrapping_add(1) == at.0
            {
                return true;
            }
            frame = tier::below(req, at_frame);
            level += 1;
        }
        false
    }
}

/// The three operands of `op` sampled as php samples them before the
/// handler (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:848-932`): `op1` and
/// `op2` where they name a slot and the opcode reads a value there, and
/// the `OP_DATA`'s `op1` after an assignment, whose class php does not
/// record.
pub(crate) fn sampled(
    req: &Request,
    frame: FrameRef,
    op: &Opline,
    after: Option<&Opline>,
) -> (Option<Sample>, Option<Sample>, Option<Sample>) {
    let op1_read = !matches!(
        op.opcode,
        Opcode::RopeAdd
            | Opcode::RopeEnd
            | Opcode::New
            | Opcode::FetchClassConstant
            | Opcode::InitStaticMethodCall
    );
    let op1 = op1_read
        .then(|| tier::sample(req, frame, op.op1_type, op.op1))
        .flatten()
        .or_else(|| on_this(op).then(|| this(req, frame)).flatten());
    let static_prop = matches!(
        op.opcode,
        Opcode::Instanceof
            | Opcode::UnsetStaticProp
            | Opcode::IssetIsemptyStaticProp
            | Opcode::AssignStaticProp
            | Opcode::AssignStaticPropRef
            | Opcode::AssignStaticPropOp
            | Opcode::PreIncStaticProp
            | Opcode::PostIncStaticProp
            | Opcode::PreDecStaticProp
            | Opcode::PostDecStaticProp
            | Opcode::FetchStaticPropR
            | Opcode::FetchStaticPropW
            | Opcode::FetchStaticPropRw
            | Opcode::FetchStaticPropIs
            | Opcode::FetchStaticPropFuncArg
            | Opcode::FetchStaticPropUnset
    );
    let fe_fetch = matches!(op.opcode, Opcode::FeFetchR | Opcode::FeFetchRw);
    let op2_read = !static_prop && (op.op2_type == OperandKind::Cv || !fe_fetch);
    let op2 = op2_read
        .then(|| tier::sample(req, frame, op.op2_type, op.op2))
        .flatten();
    let assigns = matches!(
        op.opcode,
        Opcode::AssignDim
            | Opcode::AssignObj
            | Opcode::AssignStaticProp
            | Opcode::AssignDimOp
            | Opcode::AssignObjOp
            | Opcode::AssignStaticPropOp
            | Opcode::AssignObjRef
            | Opcode::AssignStaticPropRef
    );
    let data = after
        .filter(|_| assigns)
        .and_then(|data| tier::sample(req, frame, data.op1_type, data.op1))
        .map(|sample| Sample {
            class: None,
            ..sample
        });
    (op1, op2, data)
}

/// The place a property opline at `at`, stepped in `frame` and gone on
/// in `next`, found on the object of the class its `op1` sample names,
/// taken where it stays in its frame ([`UnitView::hold_place`]).
fn placed(
    req: &Request,
    view: &mut UnitView,
    (frame, next): (FrameRef, FrameRef),
    (op, at): (&Opline, OplineIdx),
    op1: Option<Sample>,
) {
    if next == frame
        && let Some(class) = op1.and_then(|sample| sample.class)
        && types::on_declared_slot(op.opcode)
    {
        view.hold_place(req, frame, at, class);
    }
}

/// Whether `op` reads `$this` as its object, its `op1` unused: a property
/// opline or a method's call on `$this`, whose class the recorder writes
/// down as `op1`'s, as php's code generator reads `$this`'s class off
/// the frame it compiles for.
fn on_this(op: &Opline) -> bool {
    op.op1_type == OperandKind::Unused
        && (types::on_declared_slot(op.opcode)
            || matches!(
                op.opcode,
                Opcode::InitMethodCall | Opcode::IssetIsemptyPropObj
            ))
}

/// `$this` of `frame` sampled as an object of its class.
fn this(req: &Request, frame: FrameRef) -> Option<Sample> {
    tier::this_class(req, frame).map(|class| Sample {
        kind: ZvalType::Object,
        indirect: false,
        reference: false,
        packed: false,
        class: Some(class),
    })
}

/// php's `ZEND_JIT_TRACE_OP1_TYPE` and `_OP2_TYPE` records an opline
/// takes, one per class its slotted operands hold: `$this`'s class, which
/// this recorder writes down for a property opline and a method's call,
/// php records only in a closure, so it is counted for no operand php
/// samples no slot of.
pub(crate) fn classes(op: &Opline, op1: Option<Sample>, op2: Option<Sample>) -> u32 {
    let slotted = |kind: OperandKind| !matches!(kind, OperandKind::Unused | OperandKind::Const);
    u32::from(slotted(op.op1_type) && op1.is_some_and(|s| s.class.is_some()))
        + u32::from(op2.is_some_and(|s| s.class.is_some()))
}

/// Whether the frame a root's return landed in serves the same op array
/// as the frame below it: php's `RECURSION_EXIT`.
fn recursion_exits(req: &Request, caller: FrameRef) -> bool {
    let here = tier::func(req, caller);
    here.is_some() && tier::below(req, caller).is_some_and(|below| tier::func(req, below) == here)
}

/// How many of `seen` are `func`.
fn count_of(seen: Option<&[FuncRef]>, func: FuncRef) -> u32 {
    seen.map_or(0, |seen| {
        u32::try_from(seen.iter().filter(|f| **f == func).count()).unwrap_or(u32::MAX)
    })
}

/// Where a recording stopped, nothing rerooted.
fn ended(at: FrameRef, stepped: bool) -> Ended {
    Ended {
        at,
        inner: None,
        stepped,
        rerooted: None,
        stood: None,
        activated: false,
    }
}

/// The opline `frame` stands at.
pub(crate) fn position(req: &Request, frame: FrameRef) -> OplineIdx {
    tier::frame(req, frame).map_or(OplineIdx(0), |view| view.opline())
}

/// The loop header standing at `at`, as php's trace roots it, where
/// php's CFG finds a loop at all ([`found`]): [`loop_at`]'s.
pub(crate) fn header_at(code: &OpArray, at: OplineIdx, folds: Folds) -> Option<SafepointIdx> {
    loop_at(code, at, folds).filter(|header| found(code, *header, folds))
}

/// The loop whose root stands at `at`, counted or not: the safepoint
/// past the entry, and no return's own number, whose loop [`loop_root`]
/// finds at `at`. Its word says whether php counts it, which a claim
/// asks ([`counts`]), so a return that lands here reads it with no walk
/// of the op array (`dispatch::goes_on`).
pub(crate) fn loop_at(code: &OpArray, at: OplineIdx, folds: Folds) -> Option<SafepointIdx> {
    code.safepoints
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(number, site)| !is_return_site(code, **site, *number))
        .find(|(_, site)| {
            (**site == at || before_body(code, **site) == Some(at))
                && rooted(code, **site, folds) == at
        })
        .and_then(|(number, _)| u32::try_from(number).ok())
        .map(SafepointIdx)
}

/// Whether php counts the heat of loop `header`, whose back edge left
/// `frame` where it lands: `zend_jit_setup_hot_trace_counters` swaps the
/// handler of the header of each loop php's CFG finds
/// (`Z/ext/opcache/jit/zend_jit_trace.c:8954-8980`), and finds none where
/// the header is not [`reachable`] from the entry but through a `catch`, so
/// php never records such a loop, and its word is blacklisted at its
/// first claim, with no line (`dispatch::hot`). An op array no trace
/// reads is counted: its word reads sealed already.
///
/// The `try` throws in `$main` itself, so the code past its `catch` is
/// reached through the `catch` alone, and the entry's recording aborts
/// at the throw and roots nothing more, where a throw in a callee
/// (`dispatch::safepoint`) leaves the loop a root:
///
/// ```php
/// try { throw new Exception("x"); } catch (Exception $e) {}
/// $s = 0;
/// for ($i = 0; $i < 3; $i++) { $s += $i; }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-inline process
/// 3
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:2
/// ---- TRACE 1 abort (exception)
///
/// ```
pub(crate) fn counts(req: &Request, frame: FrameRef, header: SafepointIdx, folds: Folds) -> bool {
    tier::served(req, frame).is_none_or(|held| found(held.code(), header, folds))
}

/// Whether php's CFG finds loop `header`: its root is [`reachable`] from
/// the entry, and so is an edge back into the root from inside the loop,
/// as `zend_cfg_identify_loops` finds a loop by a back edge from a block
/// the entry reaches, an unreached block drawing no edge, and a header
/// with one predecessor heading none (`Z/Zend/Optimizer/zend_cfg.c:607-617`,
/// `:778-880`). A loop rooted where its back edge lands, a `foreach` or a
/// `do`, has its back edge in a jump at or past that opline. A `for` or a
/// `while`, rooted at its condition ([`rooted`]), has it in the body
/// between the opline the back edge lands on and the condition: the
/// body's last opline falling into the condition, or a `continue`'s jump
/// to it. The condition's own jump into the body is no back edge, since
/// the condition dominates the body.
///
/// A `foreach` whose body always returns has none: the `JMP` its
/// compiler emits past the body is dead, and php's trace records through
/// the `FE_FETCH_R` as through any other opline.
///
/// ```php
/// function first($a) { foreach ($a as $v) { return $v; } return 0; }
/// $s = 0;
/// for ($i = 0; $i < 3; $i++) { $s += first([$i, 9]); }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-inline process capture
/// 3
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:3
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) $main() %SCRATCH%/witness.php:4
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 2 $main() %SCRATCH%/witness.php:5
/// ---- TRACE 2 start (side trace 1/2) $main() %SCRATCH%/witness.php:5
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
/// ```
///
/// Nor has a `while` whose body always returns: nothing reaches its
/// condition but the `JMP` that enters the loop, so the condition heads
/// no loop, and the `JMPNZ` from it into the body is no back edge:
///
/// ```php
/// function first($a, $n) { $k = $n - 2; while ($k < $n) { return $a[$k]; } return 0; }
/// $s = 0;
/// for ($i = 0; $i < 3; $i++) { $s += first([$i, 9], 2); }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-inline process capture
/// 3
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:3
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) $main() %SCRATCH%/witness.php:4
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 2 $main() %SCRATCH%/witness.php:5
/// ---- TRACE 2 start (side trace 1/2) $main() %SCRATCH%/witness.php:5
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
/// ```
///
/// and a `for` whose body always returns, its step never reached, has
/// none either:
///
/// ```php
/// function first($a, $n) { for ($k = $n - 2; $k < $n; $k++) { return $a[$k]; } return 0; }
/// $s = 0;
/// for ($i = 0; $i < 3; $i++) { $s += first([$i, 9], 2); }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-inline process capture
/// 3
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:3
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) $main() %SCRATCH%/witness.php:4
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 2 $main() %SCRATCH%/witness.php:5
/// ---- TRACE 2 start (side trace 1/2) $main() %SCRATCH%/witness.php:5
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
/// ```
pub(crate) fn found(code: &OpArray, header: SafepointIdx, folds: Folds) -> bool {
    let (Some(root), Some(&site)) = (
        loop_root(code, header, folds),
        code.safepoints.get(header.0 as usize),
    ) else {
        return false;
    };
    let seen = reachable(code);
    let reaches = |at: OplineIdx| seen.get(at.0 as usize).copied().unwrap_or(false);
    let jumps_to = |at: OplineIdx, to: OplineIdx| tier::jump_targets(code, at).any(|t| t == to);
    if !reaches(root) {
        return false;
    }
    if root == site {
        let end = u32::try_from(code.oplines.len()).unwrap_or(u32::MAX);
        return (site.0..end)
            .map(OplineIdx)
            .any(|at| reaches(at) && jumps_to(at, site));
    }
    // The body, from where the back edge lands up to the condition.
    (site.0..root.0).map(OplineIdx).any(|at| {
        let falls = at.0 + 1 == root.0
            && code
                .oplines
                .get(at.0 as usize)
                .is_some_and(|op| !ends_block(op.opcode));
        reaches(at) && (falls || jumps_to(at, root))
    })
}

/// Whether no edge leaves the opline past one of `opcode`'s: a return, a
/// throw, a `MATCH_ERROR`, a `VERIFY_NEVER_TYPE`, a `FAST_RET`, a `JMP`
/// and a `MATCH` (`zend_build_cfg`, `Z/Zend/Optimizer/zend_cfg.c:496-587`).
fn ends_block(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::Return
            | Opcode::ReturnByRef
            | Opcode::GeneratorReturn
            | Opcode::Throw
            | Opcode::MatchError
            | Opcode::VerifyNeverType
            | Opcode::FastRet
            | Opcode::Jmp
            | Opcode::Match
    )
}

/// Per opline, whether it is reached from the op array's entry along
/// the edges php's CFG draws (`zend_build_cfg`,
/// `Z/Zend/Optimizer/zend_cfg.c:496-587`): each of its
/// [`tier::jump_targets`], and the opline after each opline that has
/// one, all
/// but a return, a throw, a `MATCH_ERROR`, a `VERIFY_NEVER_TYPE`, a
/// `FAST_RET`, a `JMP` and a `MATCH`. No edge enters a `catch` from the
/// `try` it guards, so code reached through a `catch` alone has no
/// immediate dominator (`zend_cfg_compute_dominators_tree`, `:694-724`),
/// and `zend_cfg_identify_loops` looks for loops among the entry's
/// dominator tree alone (`:832-844`). A throw inside an expression ends a
/// block in php only where one starts past it; here it ends one always,
/// which nothing observes, as the opline past a throw never runs.
///
/// `php_ssa`'s CFG and dominator tree replace this walk once ADR-0015's
/// crate lands, and this tier reads its loop headers from there.
fn reachable(code: &OpArray) -> Vec<bool> {
    let mut seen = vec![false; code.oplines.len()];
    let mut work = vec![OplineIdx(0)];
    while let Some(at) = work.pop() {
        let index = at.0 as usize;
        let (Some(op), Some(seen)) = (code.oplines.get(index), seen.get_mut(index)) else {
            continue;
        };
        if std::mem::replace(seen, true) {
            continue;
        }
        work.extend(tier::jump_targets(code, at));
        if !ends_block(op.opcode) {
            work.extend(at.0.checked_add(1).map(OplineIdx));
        }
    }
    seen
}

/// Whether php's optimizer folds a loop's constant condition before its
/// tier reads the op array ([`folds_to_jmp`]): pass 1 makes a `JMPZ` or a
/// `JMPNZ` on a constant that takes it a `JMP`, and pass 3's jump
/// optimization or pass 5's CFG threads the `JMP` entering the loop onto
/// the body, and pass 5 folds the constant on its own
/// (`Z/Zend/Optimizer/zend_optimizer.c:1072-1107`), as
/// `opcache.optimization_level` asks for each. With neither, php's CFG
/// heads a `while (true)` at its condition, the `while`'s line, as it
/// heads a `for`'s (#3578):
///
/// ```php
/// function f() {
///     $i = 0;
///     while (true) {
///         if ($i === 10) {
///             return $i;
///         }
///         $i++;
///     }
/// }
/// echo f(), "\n";
/// ```
/// ```expect regime=jit-ssa process
/// 10
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:11
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) f() %SCRATCH%/witness.php:4
/// ---- TRACE 1 stop (loop)
///
/// f:
///      ; (lines=8, args=0, vars=1, tmps=3, ssa_vars=5)
///      ; (JIT)
///      ; %SCRATCH%/witness.php:2-10
///      ; return  [null, long, double] RANGE[0..++]
///      ; #0.CV0($i) NOVAL [undef]
/// BB0:
///      ; start lines=[0-1]
///      ; to=(BB4)
///      ; level=0
///      ; children=(BB4)
/// 0000 ASSIGN #0.CV0($i) NOVAL [undef] -> #1.CV0($i) [long] RANGE[0..0] int(0)
/// 0001 JMP BB4
///
/// BB1:
///      ; target lines=[2-3]
///      ; from=(BB4)
///      ; to=(BB3, BB2)
///      ; idom=BB4
///      ; level=2
///      ; loop_header=4
///      ; children=(BB2, BB3)
/// 0002 #3.T2 [bool] = IS_IDENTICAL #2.CV0($i) [long, double] RANGE[0..++] int(10)
/// 0003 JMPZ #3.T2 [bool] BB3
///
/// BB2:
///      ; follow exit lines=[4-4]
///      ; from=(BB1)
///      ; idom=BB1
///      ; level=3
/// 0004 RETURN #2.CV0($i) [long, double] RANGE[0..++]
///
/// BB3:
///      ; target lines=[5-5]
///      ; from=(BB1)
///      ; to=(BB4)
///      ; idom=BB1
///      ; level=3
///      ; loop_header=4
/// 0005 PRE_INC #2.CV0($i) [long, double] RANGE[0..++] -> #4.CV0($i) [long, double] RANGE[1..++]
///
/// BB4:
///      ; follow target loop_header lines=[6-6]
///      ; from=(BB0, BB3)
///      ; to=(BB1, BB5)
///      ; idom=BB0
///      ; level=1
///      ; children=(BB1, BB5)
///      #2.CV0($i) [long, double] RANGE[0..++] = Phi(#1.CV0($i) [long] RANGE[0..0], #4.CV0($i) [long, double] RANGE[1..++])
/// 0006 JMPNZ bool(true) BB1
///
/// BB5:
///      ; follow exit lines=[7-7]
///      ; from=(BB4)
///      ; idom=BB4
///      ; level=2
/// 0007 RETURN null
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 1 f() %SCRATCH%/witness.php:6
/// ---- TRACE 2 start (side trace 1/1) f() %SCRATCH%/witness.php:6
/// ---- TRACE 2 stop (return)
///
/// f:
///      ; (lines=8, args=0, vars=1, tmps=3, ssa_vars=5)
///      ; (JIT)
///      ; %SCRATCH%/witness.php:2-10
///      ; return  [null, long, double] RANGE[0..++]
///      ; #0.CV0($i) NOVAL [undef]
/// BB0:
///      ; start lines=[0-1]
///      ; to=(BB4)
///      ; level=0
///      ; children=(BB4)
/// 0000 ASSIGN #0.CV0($i) NOVAL [undef] -> #1.CV0($i) [long] RANGE[0..0] int(0)
/// 0001 JMP BB4
///
/// BB1:
///      ; target lines=[2-3]
///      ; from=(BB4)
///      ; to=(BB3, BB2)
///      ; idom=BB4
///      ; level=2
///      ; loop_header=4
///      ; children=(BB2, BB3)
/// 0002 #3.T2 [bool] = IS_IDENTICAL #2.CV0($i) [long, double] RANGE[0..++] int(10)
/// 0003 JMPZ #3.T2 [bool] BB3
///
/// BB2:
///      ; follow exit lines=[4-4]
///      ; from=(BB1)
///      ; idom=BB1
///      ; level=3
/// 0004 RETURN #2.CV0($i) [long, double] RANGE[0..++]
///
/// BB3:
///      ; target lines=[5-5]
///      ; from=(BB1)
///      ; to=(BB4)
///      ; idom=BB1
///      ; level=3
///      ; loop_header=4
/// 0005 PRE_INC #2.CV0($i) [long, double] RANGE[0..++] -> #4.CV0($i) [long, double] RANGE[1..++]
///
/// BB4:
///      ; follow target loop_header lines=[6-6]
///      ; from=(BB0, BB3)
///      ; to=(BB1, BB5)
///      ; idom=BB0
///      ; level=1
///      ; children=(BB1, BB5)
///      #2.CV0($i) [long, double] RANGE[0..++] = Phi(#1.CV0($i) [long] RANGE[0..0], #4.CV0($i) [long, double] RANGE[1..++])
/// 0006 JMPNZ bool(true) BB1
///
/// BB5:
///      ; follow exit lines=[7-7]
///      ; from=(BB4)
///      ; idom=BB4
///      ; level=2
/// 0007 RETURN null
/// ---- TRACE 2 compiled
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, EngineState)]
pub(crate) struct Folds(pub(crate) bool);

impl Folds {
    /// What `level`, php's `opcache.optimization_level`, folds.
    pub(crate) fn of(level: i64) -> Folds {
        const PASS_1: i64 = 1;
        const PASS_3: i64 = 1 << 2;
        const PASS_5: i64 = 1 << 4;
        Folds(level & PASS_5 != 0 || (level & PASS_1 != 0 && level & PASS_3 != 0))
    }
}

/// The opline php's trace of loop `header` roots at, where the safepoint
/// numbers an opline the back edge lands on: the block php's CFG finds
/// as the loop's header (`zend_cfg_identify_loops`), the one
/// `zend_jit_setup_hot_trace_counters` marks `ZEND_JIT_TRACE_START_LOOP`
/// (`Z/ext/opcache/jit/zend_jit_trace.c:8963-8980`). A `for` or a
/// `while` is compiled with its condition after its body and a `JMP` to
/// the condition before it (`zend_compile_for`, `zend_compile_while`), so
/// the loop is entered at its condition, which dominates the body and is
/// its header, while the back edge's `JMPNZ` lands on the body; every
/// other loop is entered where its back edge lands.
///
/// php's CFG is built over the op array its optimizer left, which this
/// engine runs none of (design §6.1, #1220), so where
/// `opcache.optimization_level` runs the passes that fold it ([`Folds`]),
/// a condition pass 1 folds as it is written, a jump on a constant
/// operand, is read as folded: a `while (true)` or a `for (;;)`, whose
/// condition is a jump on a constant php makes a `JMP` back to the body
/// ([`folds_to_jmp`]), is headed at the body, where the back edge lands,
/// and its condition is no header. The opline past a call that ends the
/// body is then a landing a compiled trace arms ([`landings`]), as php's
/// `zend_jit_trace_setup_ret_counter` arms the folded `JMP`, so a return
/// trace is rooted there and links to the loop's (#3317). At level 0, the
/// regime below, nothing folds: the loop is headed at its condition, and
/// no return trace is rooted past the call (#3578):
///
/// ```php
/// set_time_limit(1);
/// function f($n) { return $n ? f($n - 1) + 1 : 0; }
/// while (true) { f(50); }
/// ```
/// ```expect exit=255 regime=jit-inline process wall=1s
/// Fatal error: Maximum execution time of 1 second exceeded in %s on line 3
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:2
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) $main() %s:4
/// ---- TRACE 1 abort (deep recursion)
///
/// ---- TRACE 1 start (enter) f() %s:3
/// ---- TRACE 1 stop (recursive call)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 1 f() %s:3
/// ---- TRACE 2 start (side trace 1/1) f() %s:3
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
/// ---- TRACE 3 start (return) f() %s:3
/// ---- TRACE 3 stop (recursive return)
/// ---- TRACE 3 compiled
///
/// ---- TRACE 4 start (loop) $main() %s:4
/// ---- TRACE 4 stop (link to 1)
/// ---- TRACE 4 compiled
/// ```
pub(crate) fn loop_root(code: &OpArray, header: SafepointIdx, folds: Folds) -> Option<OplineIdx> {
    code.safepoints
        .get(header.0 as usize)
        .map(|site| rooted(code, *site, folds))
}

/// [`loop_root`] of the loop whose back edge lands on `site`.
fn rooted(code: &OpArray, site: OplineIdx, folds: Folds) -> OplineIdx {
    before_body(code, site)
        .filter(|condition| {
            // A condition php keeps, with its own jump back to the body
            // at or past the `JMP`'s target: a `JMP` past a loop entered
            // where it lands, an `else`'s or a `break`'s, is followed by
            // none.
            !(folds.0 && folds_to_jmp(code, *condition, site))
                && (code.oplines.iter().skip(condition.0 as usize))
                    .any(|op| op.jump_target() == Some(site))
        })
        .unwrap_or(site)
}

/// Whether the opline at `at` is a jump to `to` that php's optimizer
/// makes a `JMP` there: a `JMPZ` or a `JMPNZ` on a constant whose truth
/// takes it, which pass 1 rewrites as a `JMP` (and one that does not, as
/// a `NOP`; `Z/Zend/Optimizer/pass1.c:315-333`). Standing where a `for`'s
/// or a `while`'s `JMP` enters the loop, it is the whole of the
/// condition's block, which the block pass then threads that `JMP`
/// through onto the body, and drops as a jump to the next block
/// (`Z/Zend/Optimizer/block_pass.c:1272-1295`), so php's CFG enters the
/// loop at the body.
fn folds_to_jmp(code: &OpArray, at: OplineIdx, to: OplineIdx) -> bool {
    code.oplines.get(at.0 as usize).is_some_and(|op| {
        let jumps_when = match op.opcode {
            Opcode::Jmpnz => true,
            Opcode::Jmpz => false,
            _ => return false,
        };
        op.op1_type == OperandKind::Const
            && op.jump_target() == Some(to)
            && (code.literals.get(op.op1 as usize)).and_then(|value| constant_truth(*value))
                == Some(jumps_when)
    })
}

/// Where the `JMP` right before `site` goes, where it goes past it: a
/// `for`'s or a `while`'s jump to its condition.
fn before_body(code: &OpArray, site: OplineIdx) -> Option<OplineIdx> {
    let jump = code.oplines.get(site.0.checked_sub(1)? as usize)?;
    (jump.opcode == Opcode::Jmp)
        .then(|| jump.jump_target())
        .flatten()
        .filter(|to| to.0 > site.0)
}

/// Whether safepoint `number`, standing at `site`, is a return's own.
fn is_return_site(code: &OpArray, site: OplineIdx, number: usize) -> bool {
    code.oplines
        .get(site.0 as usize)
        .is_some_and(|op| returns(op.opcode) && op.out as usize == number)
}

/// Whether `op` jumps or falls through as its operand says: an opline
/// with a jump target that is no unconditional `JMP`, no `CATCH` or
/// `FAST_CALL`, and no `SWITCH_*` or `MATCH`, whose target is only the
/// default of a table.
pub(crate) fn conditional(op: &Opline) -> bool {
    op.jump_target().is_some()
        && !matches!(
            op.opcode,
            Opcode::Jmp
                | Opcode::Catch
                | Opcode::FastCall
                | Opcode::SwitchLong
                | Opcode::SwitchString
                | Opcode::Match
        )
}

/// php's `zend_jit_trace_supported`.
fn unsupported(opcode: Opcode) -> bool {
    matches!(opcode, Opcode::Catch | Opcode::FastCall | Opcode::FastRet)
}

/// The oplines whose site is a return's.
pub(crate) fn returns(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::Return | Opcode::ReturnByRef | Opcode::GeneratorReturn
    )
}

/// The oplines that leave the frame they stand in: a return, a yield, and
/// `GENERATOR_CREATE`, which moves its frame to the heap and returns the
/// generator to the caller.
pub(crate) fn leaves(opcode: Opcode) -> bool {
    returns(opcode)
        || matches!(
            opcode,
            Opcode::Yield | Opcode::YieldFrom | Opcode::GeneratorCreate
        )
}

/// The oplines that make a frame of a call.
fn is_do_call(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::DoFcall | Opcode::DoIcall | Opcode::DoUcall | Opcode::DoFcallByName
    )
}

/// php's `zend_jit_may_be_polymorphic_call` over the opline that began a
/// call.
fn polymorphic(op: &Opline) -> bool {
    match op.opcode {
        Opcode::InitMethodCall | Opcode::InitDynamicCall => true,
        Opcode::InitStaticMethodCall => {
            op.op1_type != OperandKind::Const || op.op2_type != OperandKind::Const
        }
        Opcode::InitUserCall => op.op2_type != OperandKind::Const,
        Opcode::New => op.op1_type != OperandKind::Const,
        _ => false,
    }
}

/// The internal error for a frame that stands past its op array, which
/// no unit the compiler wrote reaches.
#[cold]
fn off_the_end(at: OplineIdx) -> Fatal {
    Fatal::error(format!(
        "InternalError: the recorder stands at opline {}, past the end of its op array",
        at.0
    ))
}

/// The internal error for a step that popped the serving's entry frame,
/// which the entry-frame rule keeps the recorder from taking
/// (`jit-design.md` §4.1).
#[cold]
fn stepped_past_entry() -> Fatal {
    Fatal::error("InternalError: the recorder stepped past its serving's entry frame")
}
