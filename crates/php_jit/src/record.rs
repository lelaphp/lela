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
use php_heap::ZvalType;
use php_unit::{OpArray, Opcode, OperandKind, Opline, OplineIdx};

use crate::JitRequest;
use crate::policy::{self, Holder, Policy, State};
use crate::table::TraceId;
use crate::types::ExitId;

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
            Stop::RequestUnit => "call into a request-scoped op array",
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
    /// A call an `INIT_*` began, `ZEND_JIT_TRACE_INIT_CALL`: the op
    /// array its callee serves, none for a native's.
    InitCall { target: Option<FuncRef> },
    /// A PHP-to-PHP call followed, `ZEND_JIT_TRACE_ENTER`.
    Enter { func: FuncRef },
    /// Its return, `ZEND_JIT_TRACE_BACK`.
    Back,
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
pub(crate) fn recording(req: &mut Request) -> bool {
    req.state::<JitRequest>().recording
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
        req.state::<JitRequest>().recording = true;
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
        self.req.state::<JitRequest>().recording = false;
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
    let Opening { entered, behind } = opening;
    let mut req = Recording::set(req);
    let mut view = UnitView::new(&req);
    let held = tier::func(&req, frame).filter(|root| view.hold(&req, *root));
    let Some(root) = held else {
        return Ok((
            refused(&req, frame, start, Stop::RequestUnit),
            ended(frame, false),
        ));
    };
    let mut recorder = Recorder::new(policy, start, tier::epoch(&req), Claimed::of(claimed));
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
        events: recorder.events,
        stop,
        link_at: matches!(stop, Stop::Link(_))
            .then_some(recorder.link_at)
            .flatten(),
    };
    Ok((Recorded { record, view }, ended))
}

/// What a recording begins with beside its frame: for a side trace,
/// what its parent's exit knew of the frame's slots; for a loop, the
/// condition it begins at behind the frame ([`behind`]).
#[derive(Debug, Default)]
pub(crate) struct Opening {
    pub(crate) entered: Vec<Entered>,
    pub(crate) behind: Option<OplineIdx>,
}

/// The landings a compiled trace arms (`jit-design.md` §2.2, #3036),
/// where php's code generator calls `zend_jit_trace_setup_ret_counter`
/// once the trace's code is generated
/// (`Z/ext/opcache/jit/zend_jit_trace.c:7322-7368`): for a recursive
/// call, the opline past each call into the root's op array; for a
/// trace that ends where another trace or the interpreter goes on, the
/// opline past its last where that is a call, an `include` or an `eval`
/// or a `yield`, and past the call each frame below the one it ends in
/// made. Each as its op array and opline; none for any other stop.
pub(crate) fn landings(record: &TraceRecord, view: &UnitView) -> Vec<(FuncRef, OplineIdx)> {
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
                .is_some_and(|op| {
                    matches!(
                        op.opcode,
                        Opcode::DoUcall
                            | Opcode::DoFcall
                            | Opcode::DoFcallByName
                            | Opcode::Yield
                            | Opcode::YieldFrom
                            | Opcode::IncludeOrEval
                    )
                })
        });
        armed.extend(called.map(past));
        armed.extend(calls.into_iter().rev().map(past));
    }
    armed
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
pub(crate) fn behind(req: &Request, frame: FrameRef, header: SafepointIdx) -> Option<OplineIdx> {
    let mut view = UnitView::new(req);
    let code = tier::func(req, frame)
        .filter(|func| view.hold(req, *func))
        .and_then(|func| view.op_array(func))?;
    let body = position(req, frame);
    let root = loop_root(code, header).filter(|root| *root != body)?;
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
/// php's heat and its trace stand at the loop's header, the condition
/// of a `for` or a `while`, which the interpreter reaches past the body
/// the back edge lands on, so a recording starts there and a trace is
/// entered there, as php's handler at that opline is the trace. A call
/// the body makes is stepped through; a fatal is the step's.
pub(crate) fn headed(
    req: &mut Request,
    frame: FrameRef,
    header: SafepointIdx,
) -> Result<Headed, Fatal> {
    let from = position(req, frame);
    let mut view = UnitView::new(req);
    let Some(code) = tier::func(req, frame)
        .filter(|func| view.hold(req, *func))
        .and_then(|func| view.op_array(func))
    else {
        return Ok(Headed::Unstepped);
    };
    let Some(root) = loop_root(code, header) else {
        return Ok(Headed::Unstepped);
    };
    if root == from {
        return Ok(Headed::Stands(frame));
    }
    let (mut at, mut depth) = (frame, 0u32);
    loop {
        if depth == 0 {
            let now = position(req, at);
            if now == root {
                return Ok(Headed::Stepped(at));
            }
            let leaving = (code.oplines.get(now.0 as usize))
                .is_some_and(|op| leaves(op.opcode) && at == tier::entry(req));
            if now.0 < from.0 || now.0 > root.0 || leaving {
                return Ok(Headed::Away(at));
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
                return Ok(Headed::Away(next));
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
    fn new(policy: &'p Policy, start: Start, epoch: Epoch, claimed: Claimed) -> Recorder<'p> {
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
            let (op1, op2, data) = sampled(req, frame, &op, after.as_ref());
            let classes = u32::from(op1.is_some_and(|s| s.class.is_some()))
                + u32::from(op2.is_some_and(|s| s.class.is_some()));
            self.events.push(Event::Op {
                func,
                opline: at,
                op1,
                op2,
                data,
                branch: None,
            });
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
                    frame = next;
                    self.same(req, frame, &op, at, after.as_ref(), call)
                }
                Stepped::Pushed(callee) => {
                    frame = callee;
                    self.pushed(req, view, callee, &mut func)
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
        view: &UnitView,
        frame: FrameRef,
        root: FuncRef,
        behind: Option<OplineIdx>,
    ) -> Result<bool, Bailed> {
        self.root = Some((root, behind.unwrap_or_else(|| position(req, frame))));
        let Some(condition) = behind else {
            return Ok(false);
        };
        let op = (view.op_array(root))
            .and_then(|code| code.oplines.get(condition.0 as usize).copied())
            .ok_or_else(|| Bailed::Fatal(off_the_end(condition)))?;
        let (op1, op2, _) = sampled(req, frame, &op, None);
        let classes = u32::from(op1.is_some_and(|s| s.class.is_some()))
            + u32::from(op2.is_some_and(|s| s.class.is_some()));
        self.events.push(Event::Op {
            func: root,
            opline: condition,
            op1,
            op2,
            data: None,
            branch: Some(true),
        });
        Ok(self.count(1 + classes))
    }

    /// Where a recording that stepped stopped.
    fn ended(&self, at: FrameRef) -> Ended {
        Ended {
            rerooted: self.rerooted,
            stood: self.stood,
            ..ended(at, true)
        }
    }

    /// A step that went on in a frame: the direction of a conditional
    /// jump, or of the fused compare-and-branch the opline began,
    /// written on its `Op`, and a call an `INIT_*` began written down.
    fn same(
        &mut self,
        req: &Request,
        frame: FrameRef,
        op: &Opline,
        at: OplineIdx,
        after: Option<&Opline>,
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
        self.events.push(Event::InitCall {
            target: call.target,
        });
        if self.count(1) {
            return Went::Stopped(Stop::TooLong);
        }
        Went::On
    }

    /// A call followed into `callee`, php's "Enter into function": the
    /// op array held by the view or the recording stopped at it, then
    /// php's depth and recursion limits, then the callee's entry word.
    fn pushed(
        &mut self,
        req: &Request,
        view: &mut UnitView,
        callee: FrameRef,
        func: &mut FuncRef,
    ) -> Went {
        let Some(entered) = tier::func(req, callee).filter(|f| view.hold(req, *f)) else {
            return Went::Stopped(Stop::RequestUnit);
        };
        if self.level > MAX_CALL_DEPTH {
            return Went::Stopped(Stop::TooDeep);
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
    /// return trace's root; deeper, one level back. Then the landing it
    /// returned to, as php reads the `trace_flags` of the opline a return
    /// lands on (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:1294-1325`): a
    /// return trace compiled there is linked to, and one blacklisted there
    /// stops the recording.
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
            let Some(landed) = tier::func(req, caller) else {
                return Went::Stopped(Stop::RequestUnit);
            };
            *func = landed;
        }
        let landing = self.epoch.landing(*func, position(req, caller));
        if !policy::armed(landing) {
            return Went::On;
        }
        match policy::read(landing) {
            State::Compiled(id) => {
                self.stood = Some(id);
                self.link_at = Some((*func, position(req, caller)));
                Went::Stopped(Stop::Link(id))
            }
            State::Blacklisted => Went::Stopped(Stop::BlackList),
            State::Cold | State::Recording => Went::On,
        }
    }

    /// A return trace's return past its root that php follows, its
    /// recursion counted; the stop where it may not.
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
            return Some(Stop::RequestUnit);
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
        None
    }

    /// At the opline a step left the recording at: the word of the loop
    /// header there, where there is one, read as php reads a handler's
    /// `trace_flags` (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:1296-1375`).
    fn landed(&mut self, req: &Request, view: &UnitView, frame: FrameRef, func: FuncRef) -> Went {
        let at = position(req, frame);
        let Some(header) = view.op_array(func).and_then(|code| header_at(code, at)) else {
            return Went::On;
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
fn sampled(
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
        .flatten();
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
    }
}

/// The opline `frame` stands at.
fn position(req: &Request, frame: FrameRef) -> OplineIdx {
    tier::frame(req, frame).map_or(OplineIdx(0), |view| view.opline())
}

/// The loop header standing at `at`, as php's trace roots it: the
/// safepoint past the entry, and no return's own number, whose loop
/// [`loop_root`] finds at `at`.
pub(crate) fn header_at(code: &OpArray, at: OplineIdx) -> Option<SafepointIdx> {
    code.safepoints
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(number, site)| !is_return_site(code, **site, *number))
        .find(|(_, site)| {
            (**site == at || before_body(code, **site) == Some(at)) && rooted(code, **site) == at
        })
        .and_then(|(number, _)| u32::try_from(number).ok())
        .map(SafepointIdx)
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
pub(crate) fn loop_root(code: &OpArray, header: SafepointIdx) -> Option<OplineIdx> {
    code.safepoints
        .get(header.0 as usize)
        .map(|site| rooted(code, *site))
}

/// [`loop_root`] of the loop whose back edge lands on `site`.
fn rooted(code: &OpArray, site: OplineIdx) -> OplineIdx {
    before_body(code, site)
        .filter(|condition| {
            // The condition's own jump back to the body, at or past the
            // `JMP`'s target: a `JMP` past a loop entered where it lands,
            // an `else`'s or a `break`'s, is followed by none.
            (code.oplines.iter().skip(condition.0 as usize))
                .any(|op| op.jump_target() == Some(site))
        })
        .unwrap_or(site)
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
fn leaves(opcode: Opcode) -> bool {
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
