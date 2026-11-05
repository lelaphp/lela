//! The walk that builds a [`TraceIr`] (`jit-design.md` §4.2): one pass
//! over the record's events in order, php's `zend_jit_trace_build_tssa`
//! with its type inference and guard placement folded into the one
//! loop, then the exits numbered and laid out from the state the trace
//! held where each leaves.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::{NonZeroU32, NonZeroUsize};

use php_engine::tier::{
    self, CalleeSlot, ClassId, FramelessAt, FuncRef, MapPtrRaw, PlainCall, PropertySlot, Sample,
    UnitView,
};
use php_heap::ZvalType;
use php_unit::decl::Parameter;
use php_unit::{Effects, OpArray, Opcode, OperandKind, Opline, OplineIdx};

use super::lattice::{self, Types};
use super::opcodes::{
    Carried, Declared, Hold, ISEMPTY, MAX_ARG_FLAG_NUM, MAY_BE_RESOURCE, Within, arith, assigned,
    carried, check_sites, checked_order, compiled_inline, compound_in_place, constant,
    constant_types, goes_one_way, guard_sites, held_as_is, inline_may_leave, is_call,
    keeps_temporaries, keyed, may_exit, needs_call_chain, packed_guard, param_types, params, reads,
    reads_by_name, recv_exits, referenced, slot_of, within, written, written_through,
};
use super::{
    Abort, ArgChecks, BodyEntry, Check, Constant, Def, Defaults, End, Entry, ExitId, ExitSpec,
    Form, Frame, FrameInfo, Frees, Fused, Grows, Guard, GuardAt, Home, Imm, Interrupt, Known,
    Landing, Layout, Leave, Line, LoopHeader, Object, Op, OpId, Operand, Owners, PairCallee,
    PairClass, Place, PushRoom, Read, Repeat, Returned, Slot, Slotted, Store, Stored, TraceIr,
    Unguarded, Unwind, UnwindId, Value, ValueId, Vars, on_declared_slot,
};
use crate::analysis::{Analysed, Inference};
use crate::record::{self, Began, Callee, Event, Landed, Start, Stop, TraceRecord};
use crate::table::TraceId;

mod infer;

use infer::{Inferred, Reads};
use php_ssa::{Range, Types as Php};
mod loads;
mod ranges;
mod stack;
use stack::pushes_inline;

/// php's `ZEND_JIT_TRACE_MAX_EXITS`: the most exits one trace has.
const MAX_EXITS: usize = 512;
/// What a value no reference counts is: what a slot may hold and be
/// overwritten or freed with no destructor running.
const UNCOUNTED: Types = Types::UNDEF
    .or(Types::NULL)
    .or(Types::BOOL)
    .or(Types::NUMBER);

/// The trace `rec` builds, read through `view` and `analysed`.
pub(super) fn build<'a>(
    rec: &'a TraceRecord,
    view: &'a UnitView,
    analysed: &'a Analysed,
    inference: &'a Inference<'a>,
) -> Result<TraceIr, Abort> {
    if !rec.stop.compiles() {
        return Err(Abort {
            stop: rec.stop,
            why: "the recording stopped on a failed attempt",
        });
    }
    let mut builder = Builder::new(rec, view, analysed, inference)?;
    builder.walk()?;
    builder.finish()
}

/// php's `COMPILER_ERROR`: the record does not fit the op arrays its view
/// holds.
fn mismatch(why: &'static str) -> Abort {
    Abort {
        stop: Stop::CompilerError,
        why,
    }
}

/// One frame's op array and whether a symbol table reaches its
/// variables.
#[derive(Clone, Copy)]
struct Meta<'a> {
    code: &'a OpArray,
    /// The parameters its declaration lists, where the view says.
    parameters: Option<&'a [Parameter]>,
    aliased: bool,
    /// For a frame the trace pushed, the opline of its caller's call.
    called_at: Option<OplineIdx>,
    /// For a frame the trace entered in or returned to, the opline its
    /// part of the trace begins at.
    begins: Option<OplineIdx>,
}

/// What the trace held before one op: every value of a live frame, how
/// many calls compiled code began are under way, and whether it holds one
/// of them apart from its frame's call chain ([`Call::delayed`]).
#[derive(Default, Clone)]
struct Snapshot {
    values: Vec<(Slot, ValueId)>,
    calls: usize,
    delayed: bool,
}

/// Whose state an exit's stores are read from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Point {
    /// The trace's entry, before its first op: everything is in memory.
    Start,
    /// Before the op at that place.
    Before(OpId),
    /// After the op at that place.
    After(OpId),
    /// Where the trace ends.
    End,
}

/// Where an exit sends the interpreter.
#[derive(Clone, Copy)]
enum At {
    Opline(OplineIdx),
    /// The frame stands where it goes on.
    Stands,
    /// Where a return past the trace's lowest frame left the interpreter,
    /// in a caller the guard found other than the one recorded: the trace
    /// knows nothing of that frame's slots and holds nothing of its own
    /// there, so the exit stores nothing and knows no type.
    Landed,
    /// After a fetch whose element's type a guard holds: the opline past
    /// it with what `left` says in the fetch's slot, the temporaries it
    /// read freed, and a call under way restored.
    Fetched {
        fetch: OplineIdx,
        frees: Frees,
        out: ValueId,
        left: Left,
    },
    /// After an `ADD` or a `SUB` of `1`, an increment or a decrement
    /// whose long overflowed, past `ZEND_LONG_MAX` where it went `up`:
    /// the opline past it with what `wrote` says in its slots
    /// ([`Within::Overflowed`]).
    Overflowed {
        op: OplineIdx,
        up: bool,
        wrote: Overflow,
    },
    /// After an increment or a decrement whose long a guard holds a
    /// double, which did not overflow: the opline past it with the longs
    /// it wrote in its slots ([`Within::Stayed`]).
    Stayed {
        op: OplineIdx,
        wrote: Overflow,
    },
}

impl At {
    /// Where an inline arithmetic `line` leaves whose `out` a guard
    /// holds within it, as `kind` says: at its opline, which the
    /// interpreter serves again, or past it with what it overflowed to,
    /// or the longs it wrote where it did not, in its slots.
    fn overflowing(kind: Within, line: &Line, out: ValueId) -> At {
        let op = line.opline;
        let wrote = match (line.opcode, line.op1_def) {
            (Opcode::PreInc | Opcode::PreDec, Some(var)) => Overflow::Pre { var, out: line.out },
            (Opcode::PostInc | Opcode::PostDec, Some(var)) => Overflow::Post { var, out: line.out },
            _ => Overflow::Sum { out },
        };
        match (kind, wrote) {
            (Within::Overflowed { up }, _) => At::Overflowed { op, up, wrote },
            (Within::Stayed, Overflow::Pre { .. } | Overflow::Post { .. }) => {
                At::Stayed { op, wrote }
            }
            _ => At::Opline(op),
        }
    }
}

/// What an op that overflowed left in its slots where it leaves past
/// itself ([`At::Overflowed`]), each a constant php's stack map holds,
/// or what a step that did not left there ([`At::Stayed`]), each the
/// long php's stack map holds in the step's register.
#[derive(Clone, Copy)]
enum Overflow {
    /// An `ADD` or a `SUB` of `1`: its `out` the double
    /// (`zend_jit_math_long_long`, `Z/ext/opcache/jit/zend_jit_ir.c:5086-5117`).
    Sum { out: ValueId },
    /// A `PRE_INC` or a `PRE_DEC`: its variable the double, and its
    /// `out`, where it is used, the double too (`zend_jit_inc_dec`,
    /// `:4775-4810`).
    Pre { var: ValueId, out: Option<ValueId> },
    /// A `POST_INC` or a `POST_DEC`: its variable the double, and its
    /// `out`, where it is used, the long the variable held,
    /// `ZEND_LONG_MAX` or `ZEND_LONG_MIN` (`:4775-4810`).
    Post { var: ValueId, out: Option<ValueId> },
}

impl Overflow {
    /// The values written and what each is written, its variable's or
    /// its `out`'s first.
    fn stores(self, up: bool) -> [Option<(ValueId, Stored)>; 2] {
        let double = Stored::Overflowed { up };
        match self {
            Overflow::Sum { out } => [Some((out, double)), None],
            Overflow::Pre { var, out } => [Some((var, double)), out.map(|out| (out, double))],
            Overflow::Post { var, out } => [
                Some((var, double)),
                out.map(|out| (out, Stored::Limit { up })),
            ],
        }
    }

    /// The values a step that did not overflow wrote and what each is
    /// written, its variable's or its `out`'s first: the long it went to,
    /// and in a `POST_*`'s `out` the long the variable held
    /// (`zend_jit_inc_dec`, `Z/ext/opcache/jit/zend_jit_ir.c:4824-4856`).
    /// An `ADD` or a `SUB` writes none, as php's leaves at its opline.
    fn stayed(self) -> [Option<(ValueId, Stored)>; 2] {
        let went = |value| Stored::Stayed { value, read: false };
        match self {
            Overflow::Sum { .. } => [None, None],
            Overflow::Pre { var, out } => [Some((var, went(var))), out.map(|out| (out, went(out)))],
            Overflow::Post { var, out } => [
                Some((var, went(var))),
                out.map(|out| {
                    (
                        out,
                        Stored::Stayed {
                            value: out,
                            read: true,
                        },
                    )
                }),
            ],
        }
    }
}

/// What a fetch that leaves past itself left in its slot: the element,
/// read in place or through the reference it is, where a guard of its
/// type failed; `null` where a `FETCH_DIM_IS` found none, as php's
/// not-found exit leaves it (`zend_jit_fetch_dim_read`,
/// `Z/ext/opcache/jit/zend_jit_ir.c:12576-12610`).
#[derive(Clone, Copy)]
enum Left {
    Copied(Read),
    Null,
}

/// An exit before it is numbered.
#[derive(Clone, Copy)]
struct Draft {
    point: Point,
    frame: Frame,
    at: At,
    grows: Grows,
    link: Option<TraceId>,
}

/// What an opline sees of its three operands once its guards held, which
/// it sees through a reference, and what php's inference knows of each,
/// which is what it writes is typed by.
#[derive(Clone, Copy)]
struct Seen {
    types: [Types; 3],
    /// How the opline reads each operand, where a guard held it: in
    /// place, through the reference it holds, or through the indirect
    /// `VAR` a fetch for writing left.
    read: [Read; 3],
    /// The seen types of a value no symbol table reaches, and the value's
    /// own of one a symbol table reaches, which a guard narrows for its
    /// opline alone, as php's `CHECK_OP_TRACE_TYPE` narrows `op1_info`
    /// and leaves the SSA variable's info as anything: so what an opline
    /// over such a variable writes is what it writes over anything, and
    /// the opline that reads it holds it, as php's does.
    inferred: [Types; 3],
}

/// What one opline wrote: its `out` slot, and the values it wrote there
/// and to its variable operands, its `OP_DATA`'s among them.
struct Written {
    slot: Option<Slot>,
    out: Option<ValueId>,
    op1: Option<ValueId>,
    op2: Option<ValueId>,
    data: Option<ValueId>,
}

/// A guard before its exit is numbered.
#[derive(Clone, Copy)]
struct Placed {
    at: GuardAt,
    check: Check,
    exit: Draft,
}

/// Where `is_checked_guard` holds a loop phi's long guard: within the
/// arithmetic its back edge made, or nowhere, where that arithmetic is an
/// increment or a decrement php's `zend_may_overflow` says cannot
/// overflow.
enum Checked {
    Within(Placed),
    Nowhere,
}

/// A call an `INIT_*` began: whether compiled code began it, which may
/// leave the interpreter's call chain behind it until the call is made,
/// and what each argument was sent as, where the send says.
struct Call {
    inline: bool,
    /// Whether compiled code holds it apart from its frame's call chain
    /// until it is made, php's `delayed_call_chain`: an inline `INIT_*`
    /// that [`needs_call_chain`] lets delay the link.
    delayed: bool,
    /// Per argument, what its send wrote, where it sent a value.
    args: Vec<Sent>,
    /// For a native's call, what its declaration says it returns.
    returns: Option<Types>,
    /// Its callee, as the recording wrote it down.
    callee: Callee,
    /// How compiled code makes it, where it pushed it in place
    /// ([`Op::InitCall`]'s `plain`).
    pushed: Option<Pushed>,
    /// How many arguments its `INIT_*` says are passed, where it pushed
    /// it in place ([`PlainCall::count`]); none past a named argument, an
    /// unpack or an array sent, which may move the count or leave a
    /// parameter between them undefined, php's `unknown_num_args`, so its
    /// `DO_*CALL` is the handler's.
    passed: Option<u32>,
    /// Whether a function's `INIT_*` by its name began it, so it serves
    /// the callee recorded.
    named: bool,
    /// How its last `CHECK_FUNC_ARG` found the argument it checked sent,
    /// php's `TRACE_FRAME_IS_LAST_SEND_BY_VAL` and `_BY_REF`, which the
    /// `FETCH_*_FUNC_ARG` after it reads.
    last_send: LastSend,
}

/// How compiled code makes a call it pushed in place: in place, or in
/// place only where each argument is of the type its parameter declares,
/// since the activation passes over `RECV`s that check them
/// ([`PlainCall::checks`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pushed {
    Activates,
    Checks,
}

/// How a call's last `CHECK_FUNC_ARG` found its argument sent, where the
/// trace knows the callee (`zend_jit_trace.c:2199-2215`): by value, which
/// the `FETCH_*_FUNC_ARG` after it reads as a read; or not known to be, as
/// at a call's start, past a named argument and by reference, which the
/// handlers serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastSend {
    Unknown,
    ByValue,
}

/// What compiled code serves an inline opline's work in place of the
/// kernel it would call, so that it takes none of the kernel's exits, as
/// php's code generator takes none there: the push of a plain call, a
/// send to a callee that takes the argument by value, the activation of
/// a plain call, and the leave of a frame that holds nothing to give up.
enum InPlace {
    Push(PlainCall),
    Send,
    Make(ArgChecks, Defaults),
    Leave,
}

/// The call an `INIT_*` pushes in place, and what holds the callee the
/// trace recorded before it.
struct PushAt {
    plain: PlainCall,
    held: HeldBy,
}

/// What holds a call pushed in place to the callee the trace recorded:
/// the class guard before it on a receiver of a published class, whose
/// table answers the one method in every request; or what the opline's
/// runtime-cache slot holds, which compiled code reads in the request it
/// runs in, the pair and the class it is held to, or a function.
enum HeldBy {
    Class(Object, ClassId),
    Pair(CalleeSlot, PairClass),
    /// A function's call by its name: the callee the slot holds, which
    /// the request's image of the call names.
    Slot(MapPtrRaw),
}

/// What a send wrote into a call's argument: the types of what it sent,
/// where it sent a value, and the register value it sent from, which an
/// untyped parameter of an inlined callee goes on being held in, as
/// php's send sets the callee's stack map to the register it sent
/// (`SET_STACK_REF`) where no `RECV` checks a type.
#[derive(Debug, Clone, Copy, Default)]
struct Sent {
    types: Option<Types>,
    reg: Option<ValueId>,
}

struct Builder<'a> {
    rec: &'a TraceRecord,
    view: &'a UnitView,
    /// What php's analysis of each op array the trace passes through
    /// learned, which `zend_jit_trace_build_tssa` reads.
    analysed: &'a Analysed,
    /// php's inference over the trace's ops, each read in its op array.
    inference: &'a Inference<'a>,
    frames: Vec<FrameInfo>,
    meta: Vec<Meta<'a>>,
    /// Per frame, what each slot holds now.
    slots: Vec<Vec<Option<ValueId>>>,
    /// The frames live now, the running one last.
    stack: Vec<Frame>,
    values: Vec<Value>,
    /// Per value, its types where it was made, before a guard narrowed
    /// them: what the trace knows of its slot before the guard holds.
    born: Vec<Types>,
    ops: Vec<Op>,
    snapshots: Vec<Snapshot>,
    guards: Vec<Placed>,
    /// php's code generator's guards at an opline its inference guards
    /// nowhere (`CHECK_OP_TRACE_TYPE`), which [`Builder::hold_checks`]
    /// settles once the walk has seen what marks each value
    /// `MAY_BE_GUARD`.
    checks: Vec<Placed>,
    leaves: Vec<(OpId, Leaves, Draft)>,
    calls: Vec<Call>,
    /// The plain call the `INIT_*` just built pushes in place, for the
    /// call it begins.
    pushing: Option<PlainCall>,
    map_ptrs: Vec<MapPtrRaw>,
    phis: Vec<ValueId>,
    /// What the last return returned.
    returned: Types,
    /// The last call opline the trace stepped, which a frame it pushes
    /// returns to.
    call_at: Option<OplineIdx>,
    /// Where the last opline went on, where the record says.
    successor: Option<OplineIdx>,
    /// The place in `ops` a loop goes back to.
    header: OpId,
    /// The exit a trace that does not loop leaves through.
    end_draft: Option<Draft>,
    /// Whether the trace leaves its lowest frame in place by the `RETURN`
    /// it stops at, and goes on in the interpreter where the return left
    /// it ([`End::Return`]).
    returns_at_end: bool,
    /// What the trace repeats, where its stop makes it loop.
    repeat: Option<Repeat>,
    /// An inlined callee's untyped parameter, by the value it entered
    /// with, and the caller's register value it was sent from ([`Call::regs`]).
    held: HashMap<ValueId, ValueId>,
    /// A caller's register value, by the first call into a frame above it
    /// that stored it to its slot as it entered (`Op::Enter`'s `stores`).
    spilled: HashMap<ValueId, OpId>,
    /// In a trace that does not loop, the guards held at the first read
    /// of what the root frame entered with, which php's allocator may
    /// load at entry ([`Builder::load_at_entry`]).
    entry_reads: Vec<usize>,
    /// Per line, the operands' types its recording saw.
    sampled: HashMap<OpId, [Option<Sample>; 3]>,
    /// The class a guard held each object the trace holds to, `$this` of
    /// a frame or a value, as php's `TRACE_FRAME_SET_THIS_CLASS_CHECKED`
    /// and `MAY_BE_CLASS_GUARD` mark a class guarded once
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:14317-14335`): an object's class
    /// never changes, and a value read again is a value of its own.
    classed: HashMap<Object, ClassId>,
    /// The values of variables no symbol table reaches that the trace read
    /// through a reference a guard of it held, and what an opline that read
    /// one so wrote back to its variable: php's `guarded_reference` on the
    /// trace SSA's variable (`Z/Zend/Optimizer/zend_ssa.h:129`), set where
    /// its code generator fetches the reference and kept as
    /// [`Carried`] says, which spares an `ASSIGN` its guard
    /// ([`Hold::Held`]). A loop's phi and a value the trace entered with
    /// are no such value, as php's are not.
    held_references: HashSet<ValueId>,
    /// The range php's code generator reads of a value where it is not
    /// the value's own, read where a later opline is compiled and never
    /// by the inference: what a comparison the trace compiled as a branch
    /// left it holding on the way the trace went on, as the code generator
    /// writes it over the trace SSA's range
    /// (`zend_jit_trace_update_condition_ranges`), or the range, or none,
    /// of what the line defines as the code generator propagates it once
    /// it has compiled the line ([`Builder::refresh`]).
    compiled: HashMap<ValueId, Option<Range>>,
    /// The increments and decrements php's `zend_may_overflow` says
    /// cannot overflow, which hold no guard on what they write.
    bounded: HashSet<OpId>,
}

/// Where a property opline is served in place: the object it reads, the
/// class the recording saw it of, and the slot the engine placed its
/// name at on that class ([`UnitView::place`]).
#[derive(Debug, Clone, Copy)]
struct PropertyAt {
    object: Object,
    class: ClassId,
    slot: PropertySlot,
}

impl<'a> Builder<'a> {
    fn new(
        rec: &'a TraceRecord,
        view: &'a UnitView,
        analysed: &'a Analysed,
        inference: &'a Inference<'a>,
    ) -> Result<Builder<'a>, Abort> {
        let mut builder = Builder {
            rec,
            view,
            analysed,
            inference,
            frames: Vec::new(),
            meta: Vec::new(),
            slots: Vec::new(),
            stack: Vec::new(),
            values: Vec::new(),
            born: Vec::new(),
            ops: Vec::new(),
            snapshots: Vec::new(),
            guards: Vec::new(),
            checks: Vec::new(),
            leaves: Vec::new(),
            calls: Vec::new(),
            pushing: None,
            map_ptrs: Vec::new(),
            phis: Vec::new(),
            returned: Types::NULL,
            call_at: None,
            successor: None,
            header: OpId(0),
            end_draft: None,
            returns_at_end: false,
            repeat: match rec.stop {
                Stop::Loop => Some(Repeat::Loop),
                Stop::RecursiveCall => Some(Repeat::Call),
                Stop::RecursiveRet => Some(Repeat::Return),
                _ => None,
            },
            held: HashMap::new(),
            spilled: HashMap::new(),
            entry_reads: Vec::new(),
            sampled: HashMap::new(),
            classed: HashMap::new(),
            held_references: HashSet::new(),
            compiled: HashMap::new(),
            bounded: HashSet::new(),
        };
        let begins = rec.events.iter().find_map(|event| match event {
            Event::Op { opline, .. } => Some(*opline),
            _ => None,
        });
        let root = builder.frame(rec.root, Place::Root, None, begins)?;
        builder.stack.push(root);
        Ok(builder)
    }

    /// A new frame of `func`.
    fn frame(
        &mut self,
        func: FuncRef,
        place: Place,
        called_at: Option<OplineIdx>,
        begins: Option<OplineIdx>,
    ) -> Result<Frame, Abort> {
        let code = self
            .view
            .op_array(func)
            .ok_or_else(|| mismatch("the record names an op array its view does not hold"))?;
        let frame =
            Frame(u16::try_from(self.frames.len()).map_err(|_| mismatch("too many frames"))?);
        let aliased = self.view.is_main(func).unwrap_or(true) || reads_by_name(code);
        self.frames.push(FrameInfo {
            func,
            place,
            cvs: code.num_cvs,
            temps: code.num_tmps,
            literals: code.literals.as_ptr().addr(),
        });
        self.meta.push(Meta {
            code,
            parameters: self.view.parameters(func),
            aliased,
            called_at,
            begins,
        });
        let size = code.num_cvs.saturating_add(code.num_tmps) as usize;
        self.slots.push(vec![None; size]);
        Ok(frame)
    }

    fn meta(&self, frame: Frame) -> Meta<'a> {
        self.meta[usize::from(frame.0)]
    }

    fn running(&self) -> Result<Frame, Abort> {
        self.stack
            .last()
            .copied()
            .ok_or_else(|| mismatch("no frame runs"))
    }

    /// Whether a symbol table reaches `slot`.
    fn aliased(&self, slot: Slot) -> bool {
        let meta = self.meta(slot.frame);
        slot.var < meta.code.num_cvs
            && (meta.aliased
                || meta
                    .code
                    .vars
                    .get(slot.var as usize)
                    .is_some_and(|name| &*name.0 == b"http_response_header"))
    }

    // ---------------------------------------------------------------- the walk

    fn walk(&mut self) -> Result<(), Abort> {
        let rec = self.rec;
        let events = rec.events.as_slice();
        if rec.landed == Landed::Stood {
            self.ip_guard()?;
        }
        let unexecuted = match (rec.stop, events.last()) {
            (
                Stop::Return
                | Stop::Interpreter
                | Stop::Trampoline
                | Stop::PropHookCall
                | Stop::BadFunc,
                Some(Event::Op { .. }),
            ) => 1,
            _ => 0,
        };
        let body = &events[..events.len() - unexecuted];
        match self.repeat {
            Some(Repeat::Loop) => self.loop_phis(body)?,
            Some(Repeat::Call) => self.call_phis(),
            Some(Repeat::Return) => self.ret_phi(body.first()),
            None => {}
        }
        for (at, event) in body.iter().enumerate() {
            let ahead = events.get(at + 1..).unwrap_or_default();
            match event {
                Event::Op {
                    func,
                    opline,
                    op1,
                    op2,
                    data,
                    branch,
                } => self.line(*func, *opline, [*op1, *op2, *data], *branch, ahead)?,
                Event::InitCall { callee, began } => self.init_call(*callee, *began, ahead)?,
                Event::Enter { func } => self.enter(*func)?,
                Event::Back => self.back(ahead)?,
            }
        }
        self.end(events.get(body.len()))?;
        if let (Stop::Return, Some(last)) = (rec.stop, events.get(body.len()))
            && self.returns_in_place(last)?
        {
            self.return_at_end(last)?;
        }
        self.stack_checks();
        Ok(())
    }

    /// Whether the `RETURN` a trace stops at, `last`, leaves its frame in
    /// place: php's code compiles the `RETURN` a trace stops at, leaves the
    /// frame and goes on in the interpreter where the return left it
    /// (`zend_jit_leave_func`, then `zend_jit_trace_return`,
    /// `Z/ext/opcache/jit/zend_jit_trace.c:6532-6556`). Here where its
    /// frame leaves in place and its value, held as the opline's guard
    /// holds it ([`Builder::guard_use`]), is a scalar or one [`returned`]
    /// hands on; otherwise the interpreter serves it, through the trace's
    /// end. A variable the recording saw holding a reference to a scalar
    /// is held by no guard and copied through the reference
    /// ([`Returned::Shared`]) where the trace knows it set, as the opline's
    /// form reads it.
    fn returns_in_place(&self, last: &Event) -> Result<bool, Abort> {
        let Event::Op {
            func, opline, op1, ..
        } = last
        else {
            return Ok(false);
        };
        let frame = self.running()?;
        let code = self.meta(frame).code;
        let Some(op) = code.oplines.get(opline.0 as usize) else {
            return Err(mismatch("an opline past its op array"));
        };
        if op.opcode != Opcode::Return
            || self.frames[usize::from(frame.0)].func != *func
            || !self.leaves_in_place(frame)
        {
            return Ok(false);
        }
        let held = |kind| {
            slot_of(frame, kind, op.op1).map(|slot| {
                match self.slots[usize::from(frame.0)].get(slot.var as usize) {
                    Some(Some(value)) => self.val(*value).types,
                    _ => self.entry_types(slot),
                }
            })
        };
        let scalar = match (op.op1_type, op1) {
            (OperandKind::Unused, _) => true,
            (OperandKind::Const, _) => constant_types(constant(code, op.op1), code).scalar(),
            (OperandKind::Cv, Some(sample)) if Read::sampled(*sample) == Read::Through => {
                Types::of_tag(sample.kind).scalar()
                    && held(OperandKind::Cv).is_some_and(|types| !types.meets(Types::UNDEF))
            }
            (_, Some(sample)) if !sample.indirect => Types::of_tag(sample.kind).scalar(),
            (kind, _) => held(kind).is_some_and(Types::scalar),
        };
        let t1 = match op.op1_type {
            OperandKind::Const => constant_types(constant(code, op.op1), code),
            kind => held(kind).unwrap_or(Types::ANY),
        };
        Ok(scalar || returned(op, code, t1) != Returned::Scalar)
    }

    /// The `RETURN` the trace stops at, `last`, left in place, after the
    /// trace's end took what an exit there stores
    /// ([`Builder::returns_in_place`]).
    fn return_at_end(&mut self, last: &Event) -> Result<(), Abort> {
        let Event::Op {
            func,
            opline,
            op1,
            op2,
            data,
            branch,
        } = last
        else {
            return Err(mismatch("a trace's last return that is no opline"));
        };
        self.returns_at_end = true;
        self.line(*func, *opline, [*op1, *op2, *data], *branch, &[])?;
        match self.ops.last() {
            Some(Op::Line(line)) if matches!(line.imm, Imm::Leave { .. }) => Ok(()),
            _ => Err(mismatch("a trace's last return not left in place")),
        }
    }

    /// The phis of a loop: every slot of the root's frame that its body
    /// reads before it writes and also writes, php's
    /// `zend_jit_trace_add_phis` over `zend_dfg_add_use_def_op`.
    fn loop_phis(&mut self, body: &[Event]) -> Result<(), Abort> {
        let meta = self.meta(Frame::ROOT);
        let size = self.slots[0].len();
        let (mut used, mut defined) = (vec![false; size], vec![false; size]);
        let mut depth = 0u32;
        for event in body {
            match event {
                Event::Enter { .. } => depth += 1,
                Event::Back => depth = depth.saturating_sub(1),
                Event::Op { func, opline, .. } if depth == 0 && *func == self.rec.root => {
                    let Some(op) = meta.code.oplines.get(opline.0 as usize) else {
                        return Err(mismatch("an opline past its op array"));
                    };
                    let after = meta.code.oplines.get(opline.0 as usize + 1);
                    for var in reads(op, after) {
                        if let (Some(false), Some(use_)) = (
                            defined.get(var as usize).copied(),
                            used.get_mut(var as usize),
                        ) {
                            *use_ = true;
                        }
                    }
                    for var in written(op, after) {
                        if let Some(def) = defined.get_mut(var as usize) {
                            *def = true;
                        }
                    }
                }
                _ => {}
            }
        }
        // The trace's last opline, which `zend_jit_trace_is_false_loop`
        // reads where it is the root's.
        let last = body.iter().rev().find_map(|event| match event {
            Event::Op { func, opline, .. } => Some((*func == self.rec.root).then_some(opline.0)),
            _ => None,
        });
        let (func, start) = (self.rec.root, meta.begins);
        for var in 0..size {
            if used[var] && defined[var] {
                let slot = Slot {
                    frame: Frame::ROOT,
                    var: u32::try_from(var).unwrap_or(u32::MAX),
                };
                let header = start
                    .and_then(|start| {
                        self.analysed
                            .header(func, start.0, last.flatten(), slot.var)
                    })
                    .map(|info| (info.types, info.range));
                self.phi(slot, header);
            }
        }
        Ok(())
    }

    /// The phis of a recursive call: the root's parameters, php's
    /// `zend_jit_trace_add_call_phis`.
    fn call_phis(&mut self) {
        let meta = self.meta(Frame::ROOT);
        for param in params(meta.code, meta.parameters) {
            let slot = Slot {
                frame: Frame::ROOT,
                var: param.cv,
            };
            self.phi(slot, self.reaching_start(slot));
        }
    }

    /// The phi of a recursive return, php's `zend_jit_trace_add_ret_phis`
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:763-788`): the slot the call
    /// before the trace's first opline writes, the landing a return trace
    /// is rooted at (`jit-design.md` §2.2), where its value is used.
    fn ret_phi(&mut self, first: Option<&Event>) {
        let Some(Event::Op { func, opline, .. }) = first else {
            return;
        };
        let call = (opline.0.checked_sub(1))
            .and_then(|at| self.meta(Frame::ROOT).code.oplines.get(at as usize))
            .filter(|call| is_call(call.opcode) && self.frames[0].func == *func);
        if let Some(slot) = call.and_then(|call| slot_of(Frame::ROOT, call.out_type, call.out)) {
            self.phi(slot, self.reaching_start(slot));
        }
    }

    /// What php's analysis knows the root's `slot` holds where the trace
    /// begins, which a recursive call's or return's phi copies: php
    /// copies what the phi's first use reads (`zend_jit_trace_copy_ssa_var_info`
    /// with no opline), the definition that reaches the trace's start.
    fn reaching_start(&self, slot: Slot) -> Option<(php_ssa::Types, Option<Range>)> {
        let at = self.meta(Frame::ROOT).begins?;
        let info = self.analysed.reaching(self.rec.root, at.0, slot.var)?;
        Some((info.types, info.range))
    }

    /// A phi at the header for `slot`, entered with what it holds now,
    /// and of what php's analysis knows its loop's header phi holds where
    /// it knows (`header`), of what it entered with otherwise, as php
    /// propagates the initial value through its phis
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:1774-1790`).
    fn phi(&mut self, slot: Slot, header: Option<(php_ssa::Types, Option<Range>)>) {
        let entry = self.current(slot);
        let (types, range) = match header {
            Some((header, range)) if !self.aliased(slot) => {
                (Types::UNKNOWN.restricted(header), range)
            }
            _ => (self.val(entry).types, None),
        };
        let phi = self.value(slot, types, Def::Phi { entry, back: entry }, Home::Memory);
        self.val_mut(phi).range = range;
        self.set(slot, phi);
        self.phis.push(phi);
    }

    /// One recorded opline.
    #[allow(
        clippy::too_many_lines,
        reason = "the one place an opline's guards, form, writes and exits meet"
    )]
    fn line(
        &mut self,
        func: FuncRef,
        at: OplineIdx,
        samples: [Option<Sample>; 3],
        branch: Option<bool>,
        ahead: &[Event],
    ) -> Result<(), Abort> {
        let next = ahead.first();
        let frame = self.running()?;
        if self.frames[usize::from(frame.0)].func != func {
            return Err(mismatch("an opline of another op array than its frame's"));
        }
        let code = self.meta(frame).code;
        let op = self.as_compiled(
            *code
                .oplines
                .get(at.0 as usize)
                .ok_or_else(|| mismatch("an opline past its op array"))?,
        );
        let after = code.oplines.get(at.0 as usize + 1).copied();
        let fused = fused(&op, after.as_ref(), branch);
        let data_line = after.filter(|data| data.opcode == Opcode::OpData);
        let idx = self.begin();
        let op1 = self.read(frame, op.op1_type, op.op1)?;
        let op2 = self.read(frame, op.op2_type, op.op2)?;
        let data = match data_line {
            Some(data) => self.read(frame, data.op1_type, data.op1)?,
            None => Operand::Unused,
        };
        let seen = self.op_guards(
            idx,
            frame,
            at,
            &op,
            data_line.as_ref(),
            [op1, op2, data],
            samples,
        );
        let property = self.property_at(
            frame,
            (func, at),
            (&op, data_line.as_ref()),
            (op1, samples[0]),
            seen,
        );
        let form = self.form(frame, (&op, data_line.as_ref()), seen, (next, branch));
        let pushed = match op.opcode {
            Opcode::InitMethodCall => {
                self.method_at((frame, func, at), &op, (op1, samples[0]), seen, next)
            }
            Opcode::InitStaticMethodCall => self.static_at((func, at), &op, next),
            Opcode::InitFcallByName | Opcode::InitNsFcallByName => self.named_at(func, &op, next),
            _ => None,
        };
        let fetched = self.view.fetched_slot(func, &op);
        let form = match (op.opcode, property) {
            (opcode, Some(_)) if on_declared_slot(opcode) => Form::Inline,
            (opcode, None) if on_declared_slot(opcode) => Form::Step,
            (Opcode::InitMethodCall | Opcode::InitStaticMethodCall, _) if pushed.is_some() => {
                Form::Inline
            }
            _ if fetched.is_some() => Form::Inline,
            _ => form,
        };
        let pair = match pushed.as_ref().map(|pushed| &pushed.held) {
            Some(HeldBy::Class(object, class)) => {
                self.class_guard(idx, frame, at, (*object, *class));
                None
            }
            Some(HeldBy::Pair(slot, class)) => Some((*slot, *class)),
            Some(HeldBy::Slot(_)) | None => None,
        };
        let named = match pushed.as_ref().map(|pushed| &pushed.held) {
            Some(HeldBy::Slot(slot)) => Some(*slot),
            _ => None,
        };
        if let (Form::Inline, Some(property)) = (form, property)
            && property.slot.cached().is_none()
        {
            self.class_guard(idx, frame, at, (property.object, property.class));
        }
        let pair_callee = pushed.as_ref().and_then(|pushed| pushed.plain.callee());
        let in_place = match pushed {
            Some(pushed) => Some(InPlace::Push(pushed.plain)),
            None => (form == Form::Inline)
                .then(|| self.in_place(frame, &op, next))
                .flatten(),
        };
        self.sampled.insert(idx, samples);
        let cached = (property.and_then(|property| property.slot.cached()))
            .or_else(|| pair.map(|(slot, _)| slot.cached()))
            .or(named)
            .or(fetched);
        let map_ptr = match cached {
            Some(raw) if form == Form::Inline => Some(self.map_ptr_of(raw)),
            _ => None,
        };
        self.record_send(frame, &op, op1);
        if !keeps_temporaries(op.opcode) {
            // The opline frees every temporary it reads, as its handler
            // does: the slot holds nothing an exit stores after it.
            for operand in [op1, op2, data] {
                self.consume(operand);
            }
        }
        let [t1, ..] = seen.types;
        let reads = Reads {
            op: &op,
            data: data_line.as_ref(),
            operands: [op1, op2, data],
            types: seen.inferred,
            samples,
        };
        let inferred = self.inferred(frame, at, &reads, next);
        let written = self.write(
            idx,
            frame,
            (&op, data_line.as_ref()),
            seen,
            inferred,
            form,
            fused.is_some(),
        );
        self.carry_references(
            (&op, data_line.as_ref(), code),
            [op1, op2, data],
            (samples, seen),
            &written,
        );
        // The object a property opline writes through is the one it read,
        // of the class a guard held it of (`COPY_SSA_OBJ_TYPE`).
        if on_declared_slot(op.opcode)
            && let (Operand::Value(read), Some(def)) = (op1, written.op1)
            && let Some(class) = self.classed.get(&Object::Value(read)).copied()
            && !self.aliased(self.val(read).slot)
        {
            self.classed.insert(Object::Value(def), class);
        }
        if is_call(op.opcode) {
            self.call_at = Some(at);
            if !matches!(next, Some(Event::Enter { .. })) {
                // A native's call is made here; a user function's is
                // made where its frame is entered.
                self.calls.pop();
            }
        }
        self.ops.push(Op::Line(Line {
            frame,
            opline: at,
            opcode: op.opcode,
            op1,
            op2,
            data,
            out: written.out,
            op1_def: written.op1,
            op2_def: written.op2,
            fused,
            imm: match in_place {
                Some(InPlace::Send) => Imm::Arg(op.op2),
                Some(InPlace::Leave) => Imm::Leave {
                    owners: self.owners(frame),
                    value: returned(&op, code, t1),
                    receives: self.receives(frame),
                    closure: record::closure(self.view, self.frames[usize::from(frame.0)].func),
                },
                Some(InPlace::Make(checks, defaults)) => Imm::Make {
                    returns: (op.out_type != OperandKind::Unused).then_some(op.out),
                    checks,
                    defaults,
                },
                _ if named.is_some() => Imm::Named,
                _ if let Some((slot, class)) = pair => Imm::Pair {
                    function: slot
                        .function()
                        .map_or(PairCallee::Record, PairCallee::Number),
                    class,
                },
                _ if form == Form::Inline
                    && op.opcode == Opcode::FetchDimR
                    && t1 == Types::STRING =>
                {
                    Imm::StrOffset
                }
                _ if let Some(property) = property.filter(|_| form == Form::Inline) => {
                    Imm::Property {
                        slot: match property.slot.cached() {
                            Some(_) => Slotted::Cached,
                            None => Slotted::Declared(property.slot.slot()),
                        },
                        binary: (op.opcode == Opcode::AssignObjOp)
                            .then(|| lattice::binary_of(op.extended_value))
                            .flatten(),
                    }
                }
                _ => imm(&op),
            },
            form,
            map_ptr,
            exit: None,
            unwind: None,
        }));
        // php reads the range an increment's operand holds as it compiles
        // the increment, then propagates what it wrote.
        if matches!(
            op.opcode,
            Opcode::PreInc | Opcode::PreDec | Opcode::PostInc | Opcode::PostDec
        ) && !self.may_overflow(&op, op1, written.op1)
        {
            self.bounded.insert(idx);
        }
        if let (Some(jump), Some(taken)) = (fused, branch) {
            self.narrow_by_condition(&op, (op1, op2), jump == Fused::Jmpnz, taken);
        }
        self.refresh(idx, &reads);
        let fused = fused.is_some();
        let len = if fused || data_line.is_some() { 2 } else { 1 };
        let jump = if fused { after } else { Some(op) };
        let target = jump.and_then(|jump| jump.jump_target());
        self.successor = went(at, &op, target, len, branch);
        self.own_exit(
            idx,
            frame,
            &op,
            (form, in_place.as_ref()),
            (t1, inferred.may_throw),
        );
        if let Some(InPlace::Push(plain)) = in_place {
            self.pushing = Some(plain);
        }
        self.stands(idx, (frame, at), &op, after.as_ref(), ahead, t1);
        // A call pushed in place under its slot's pair goes to its
        // handler where the slot holds another, which may push another
        // callee: held to the one the trace recorded after it, as a
        // stepped `INIT_*`'s is.
        if pair.is_some()
            && let Some(func) = pair_callee
        {
            self.guards.push(Placed {
                at: GuardAt::Within(idx),
                check: Check::Callee { func },
                exit: draft(Point::After(idx), frame, At::Stands),
            });
        }
        if form == Form::Step
            && let Some(plain) = self.handed_at(&op, next)
            && let Some(func) = plain.callee()
        {
            self.guards.push(Placed {
                at: GuardAt::Within(idx),
                check: Check::Callee { func },
                exit: draft(Point::After(idx), frame, At::Stands),
            });
            self.pushing = Some(plain);
        }
        // An array's reset goes on whatever it holds, so php's code holds
        // no direction there.
        let one_way = op.opcode == Opcode::FeResetR && form == Form::Inline;
        if let Some(taken) = branch.filter(|_| !one_way) {
            let to = Branched {
                taken,
                fused,
                len,
                target,
                opcode: op.opcode,
                held: t1,
            };
            self.direction(idx, frame, at, op1, form, to);
        }
        if form == Form::Step && op.opcode.effects().contains(Effects::CALL) {
            self.clobber(idx);
        }
        if fused && let Some(slot) = written.slot {
            self.kill(slot);
        }
        Ok(())
    }

    /// What an opline writes, once its guards held and its form is
    /// known, as php's inference learned it ([`Builder::inferred`]): its
    /// `out` slot, unless a call it makes writes it on return or a fused
    /// jump consumes it; its variable `op1` and `op2`, and its
    /// `OP_DATA`'s; and what it returns, for the frame it returns to.
    #[allow(
        clippy::too_many_arguments,
        reason = "the one place the opline's parts meet"
    )]
    ///
    /// What it writes to `op2` or the `OP_DATA`'s variable is in its slot:
    /// a `foreach` over an array php knows holds booleans reads each one
    /// from the slot `FE_FETCH_R` copied it to, under force and where
    /// every guard fails once:
    ///
    /// ```php
    /// function s($n) {
    ///     $a = []; for ($i = 0; $i < $n; $i++) { $a[] = ($i % 3) != 0; }
    ///     $c = 0; foreach ($a as $p) { if ($p) { $c++; } }
    ///     return $c;
    /// }
    /// echo s(30), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 20
    /// ```
    ///
    /// ```php
    /// function s($n) {
    ///     $a = []; for ($i = 0; $i < $n; $i++) { $a[] = ($i % 3) != 0; }
    ///     $c = 0; foreach ($a as $p) { if ($p) { $c++; } }
    ///     return $c;
    /// }
    /// echo s(30), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 20
    /// ```
    fn write(
        &mut self,
        idx: OpId,
        frame: Frame,
        (op, data_line): (&Opline, Option<&Opline>),
        seen: Seen,
        inferred: Inferred,
        form: Form,
        fused: bool,
    ) -> Written {
        let [t1, ..] = seen.inferred;
        let slot = slot_of(frame, op.out_type, op.out);
        let pushes = form == Form::Inline && is_call(op.opcode);
        // A native's call returns what its declaration says, as php's
        // trace TSSA types a `DO_ICALL`'s result by the `func` its record
        // names (`Z/ext/opcache/jit/zend_jit_trace.c:2626-2650`), and a
        // frameless call's by the native its opline names, through the
        // call map (`zend_get_func_info`).
        // Only those two callees are fixed where the opline is compiled:
        // a call by name, a method's or a dynamic one may serve
        // another function than the one recorded, which php's trace holds
        // with a guard on the callee this tier does not take.
        let returns = if op.opcode.is_frameless_icall() {
            (self.view.frameless_returns(op.extended_value)).map(super::declared)
        } else if op.opcode == Opcode::DoIcall {
            self.calls.last().and_then(|call| call.returns)
        } else {
            None
        };
        let out = match (slot, inferred.out) {
            (Some(slot), Some(held)) if !pushes && !fused => {
                let types = returns.map_or(held.types, |returns| held.types.and(returns));
                // php's `zend_jit_fetch_static_prop` writes the slot
                // whatever the property's type, in the arm its handler
                // serves too, and keeps no register.
                let form = match op.opcode {
                    Opcode::FetchStaticPropR => Form::Step,
                    _ => form,
                };
                Some(self.define(slot, (types, held.range), idx, form))
            }
            _ => None,
        };
        let op1 = match (slot_of(frame, op.op1_type, op.op1), inferred.op1) {
            (Some(slot), Some(held)) => {
                // A write through a reference leaves the slot one.
                let types = if seen.read[0] == Read::Through {
                    held.types.or(Types::REF)
                } else {
                    held.types
                };
                Some(self.define(slot, (types, held.range), idx, form))
            }
            _ => None,
        };
        // What an op writes to its `op2` or its `OP_DATA`'s variable, as
        // `FE_FETCH_R`'s copy of the element, is written to the slot, so
        // the value is in memory, typed as php's inference types it.
        let op2 = match (slot_of(frame, op.op2_type, op.op2), inferred.op2) {
            (Some(slot), Some(held)) => {
                Some(self.define(slot, (held.types, held.range), idx, Form::Step))
            }
            _ => None,
        };
        let data = if let (Some(data), Some(held)) = (data_line, inferred.data)
            && let Some(slot) = slot_of(frame, data.op1_type, data.op1)
        {
            Some(self.define(slot, (held.types, held.range), idx, Form::Step))
        } else {
            None
        };
        if op.opcode == Opcode::Return {
            self.returned = if op.op1_type == OperandKind::Unused {
                Types::NULL
            } else {
                assigned(t1)
            };
        }
        // A `RETURN_BY_REF` propagates no type, as php's trace SSA sets
        // `return_value_info` at a `RETURN` alone (`Z/ext/opcache/jit/zend_jit_trace.c:2190-2210`),
        // so what the caller's call writes is the reference to anything
        // php's inference types it, and no exit's stack map knows its type.
        if op.opcode == Opcode::ReturnByRef {
            self.returned = Types::ANY.or(Types::REF);
        }
        Written {
            slot,
            out,
            op1,
            op2,
            data,
        }
    }

    /// php's IP guard after an opline whose handler may go elsewhere than
    /// the record says ([`may_exit`]), held by `zend_jit_trace_handler`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:17129-17236`) to the opline the
    /// trace serves next; for a table's jump, an exit per opline it may
    /// go to instead ([`arms`]), as php's `zend_jit_switch` takes them.
    fn stands(
        &mut self,
        idx: OpId,
        (frame, here): (Frame, OplineIdx),
        op: &Opline,
        after: Option<&Opline>,
        ahead: &[Event],
        t1: Types,
    ) {
        if !may_exit(op, after) {
            return;
        }
        let Some((func, opline)) = self.following(ahead) else {
            return;
        };
        let arms = (func == self.frames[usize::from(frame.0)].func)
            .then(|| arms(op, here, self.meta(frame).code, opline, t1))
            .flatten();
        match arms {
            Some(arms) => {
                for arm in arms {
                    self.guards.push(Placed {
                        at: GuardAt::Within(idx),
                        check: Check::Arm { opline: arm },
                        exit: draft(Point::After(idx), frame, At::Opline(arm)),
                    });
                }
            }
            None => self.stands_at(idx, frame, func, opline),
        }
    }

    /// `Check::Stands` within the op at `idx`, leaving where the step or
    /// the return left the interpreter, in `frame`.
    fn stands_at(&mut self, idx: OpId, frame: Frame, func: FuncRef, opline: OplineIdx) {
        self.guards.push(Placed {
            at: GuardAt::Within(idx),
            check: Check::Stands { func, opline },
            exit: draft(Point::After(idx), frame, At::Stands),
        });
    }

    /// The opline the trace serves next, where the record says it: the
    /// next one it recorded before a return, or, past the last, the
    /// header a loop, a recursive call or a recursive return goes back to,
    /// as php holds the trace's next opline at every end but `RETURN` and
    /// `INTERPRETER` (`zend_jit_ir.c:17152-17156`).
    fn following(&self, ahead: &[Event]) -> Option<(FuncRef, OplineIdx)> {
        let mut until_back = ahead
            .iter()
            .take_while(|event| !matches!(event, Event::Back));
        let next = until_back.find_map(|event| match event {
            Event::Op { func, opline, .. } => Some((*func, *opline)),
            _ => None,
        });
        // Nothing served before the end: what follows is a call begun or
        // entered, and the trace goes back to its header.
        let ends = next.is_none() && !ahead.iter().any(|event| matches!(event, Event::Back));
        match (next, ends, self.repeat) {
            (Some(next), ..) => Some(next),
            (None, true, Some(_)) => self.header_line(),
            _ => None,
        }
    }

    /// The opline the trace's header serves first: past the calls its
    /// root began before the recording reached it, whose fake inits a
    /// return trace opens with, as php steps over `INIT_CALL` records to
    /// the opline it holds the instruction pointer to
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:11210-11214`).
    fn header_line(&self) -> Option<(FuncRef, OplineIdx)> {
        let header = (self.ops.get(self.header.index()..)?.iter())
            .find(|op| !matches!(op, Op::InitCall { .. }));
        let Some(Op::Line(line)) = header else {
            return None;
        };
        Some((
            self.frames.get(usize::from(line.frame.0))?.func,
            line.opline,
        ))
    }

    /// The direction guard of a conditional jump, leaving where the jump
    /// goes the way it did not go, past what the jump wrote before it.
    fn direction(
        &mut self,
        idx: OpId,
        frame: Frame,
        at: OplineIdx,
        op1: Operand,
        form: Form,
        to: Branched,
    ) {
        let Branched {
            taken,
            fused,
            len,
            target,
            opcode,
            held,
        } = to;
        if !fused && matches!(op1, Operand::Constant(_)) {
            // A constant condition goes one way; nothing to hold.
            return;
        }
        if goes_one_way(opcode, held) {
            // Nor does one its guards hold as one kind no greater than
            // `true`: php's unconditional branch.
            return;
        }
        // Where the jump goes the way it did not go, as php's exit lands
        // on the other target (`zend_jit_trace_handler`'s
        // `exit_opline`): a compared value the fused jump read is dead
        // past it, so nothing is stored for it.
        let other = if taken {
            Some(OplineIdx(at.0 + len))
        } else {
            target
        };
        let to = other.map_or(At::Stands, At::Opline);
        // php writes a `JMPZ_EX`'s boolean before it guards the way it
        // went, whichever target reads it (`zend_jit_bool_jmpznz`,
        // `Z/ext/opcache/jit/zend_jit_ir.c:7778-7783`), and an inline
        // `FE_FETCH_R` guards before it writes.
        let written = form == Form::Step || matches!(opcode, Opcode::JmpzEx | Opcode::JmpnzEx);
        let exit = draft(
            if written {
                Point::After(idx)
            } else {
                Point::Before(idx)
            },
            frame,
            to,
        );
        self.guards.push(Placed {
            at: GuardAt::Within(idx),
            check: Check::Direction { taken },
            exit,
        });
    }

    /// Where an opline itself leaves: through an exit where an inline
    /// one cannot finish, before it does anything, which the interpreter
    /// runs again; through an unwind where a kernel it calls throws, a
    /// stepped one that may, an inline assignment over a counted value,
    /// whose destructor may, an inline fetch or test of an element of a
    /// temporary array, which frees it once its element is read, and an
    /// inline element
    /// write, which its handler serves where it cannot write in place.
    fn own_exit(
        &mut self,
        idx: OpId,
        frame: Frame,
        op: &Opline,
        (form, in_place): (Form, Option<&InPlace>),
        (old, may_throw): (Types, bool),
    ) {
        let Some(Op::Line(line)) = self.ops.get(idx.index()) else {
            return;
        };
        // What compiled code serves in place takes no exit of its own, but
        // a call by its name held to the function its slot holds; a kernel
        // it falls back to that throws unwinds.
        let (at, op1, op2) = (line.opline, line.op1, line.op2);
        let may_leave = inline_may_leave(line) && (in_place.is_none() || line.imm == Imm::Named);
        if form == Form::Inline && may_leave {
            let unfinished = draft(Point::Before(idx), frame, At::Opline(at));
            // An element a fetch does not find is the interpreter's to
            // warn of, through php's `ZEND_JIT_EXIT_TO_VM`
            // (`zend_jit_fetch_dimension_address_inner`).
            let grows = if matches!(
                op.opcode,
                Opcode::FetchDimR | Opcode::FetchObjR | Opcode::FetchObjW
            ) {
                Grows::Never
            } else {
                Grows::Side
            };
            self.leaves.push((
                idx,
                Leaves::Unfinished,
                Draft {
                    grows,
                    ..unfinished
                },
            ));
        }
        let throws = match form {
            Form::Inline => {
                (op.opcode == Opcode::Assign && !old.within(UNCOUNTED))
                    // A NAN's warning runs the error handler, which may
                    // throw, where the handler would leave it pending.
                    || (takes_truth(op.opcode) && old == Types::DOUBLE)
                    || (matches!(
                        op.opcode,
                        Opcode::FetchDimR | Opcode::FetchDimIs | Opcode::IssetIsemptyDimObj
                    ) && self.is_temp(op1))
                    // The temporary it reads is given up, and a destructor
                    // may throw.
                    || (op.opcode == Opcode::TypeCheck
                        && self.is_temp(op1)
                        && !old.within(UNCOUNTED))
                    // A temporary string or array it reads is given up.
                    || (matches!(
                        op.opcode,
                        Opcode::IsIdentical
                            | Opcode::IsNotIdentical
                            | Opcode::Strlen
                            | Opcode::Count
                    ) && [op1, op2].into_iter().any(|operand| {
                        self.is_temp(operand) && !self.types_of(frame, operand).within(UNCOUNTED)
                    }))
                    || matches!(op.opcode, Opcode::FeFree | Opcode::Free)
                    // The handler that makes the reference may refuse at
                    // the memory limit.
                    || op.opcode == Opcode::SendRef
                    // The join may refuse, the memory limit or the string's
                    // size, and leave it pending; a constant's name may
                    // answer nothing, or a deprecation run the error
                    // handler, and a `static` property's handler serves
                    // what no slot answers and may refuse it.
                    || matches!(
                        op.opcode,
                        Opcode::RopeEnd
                            | Opcode::Concat
                            | Opcode::FastConcat
                            | Opcode::FetchConstant
                            | Opcode::FetchStaticPropR
                    )
                    || (op.opcode == Opcode::AssignOp
                        && lattice::binary_of(op.extended_value) == Some(Opcode::Concat))
                    || matches!(
                        op.opcode,
                        Opcode::AssignDim
                            | Opcode::AssignDimOp
                            | Opcode::FetchDimW
                            | Opcode::FetchDimRw
                    )
                    // What a property held is given up, or its handler
                    // serves what the inline form does not, and either
                    // may throw.
                    || (on_declared_slot(op.opcode)
                        && !matches!(op.opcode, Opcode::FetchObjR | Opcode::FetchObjW))
                    // What the variable held is given up, and a destructor
                    // may throw.
                    || op.opcode == Opcode::FeFetchR
                    || matches!(
                        op.opcode,
                        Opcode::DoUcall | Opcode::DoFcall | Opcode::DoFcallByName
                    )
                    // The leave kernel a variable that owns something
                    // leaves through frees it, and a destructor may throw,
                    // as may the receiver's, which it gives up after them.
                    || (matches!(in_place, Some(InPlace::Leave))
                        && (!self.uncounted(frame) || self.receives(frame)))
                    // A call pushed under its slot's pair goes to its
                    // handler where the slot holds another, which looks
                    // the class and the method up and may throw; an
                    // offset's read warns, and may refuse the key.
                    || matches!(line.imm, Imm::Pair { .. } | Imm::StrOffset)
            }
            // php checks for a throwable after a handler where
            // `zend_may_throw_ex` says it may leave one
            // (`Z/ext/opcache/jit/zend_jit_trace.c:6503-6516`).
            Form::Step => may_throw,
        };
        if throws {
            // A NAN's warning leaves at its call, before the op writes its
            // boolean; every other throw an inline op has is past its write.
            let point = if form == Form::Inline && takes_truth(op.opcode) {
                Point::Before(idx)
            } else {
                Point::After(idx)
            };
            let unwind = draft(point, frame, At::Opline(at));
            self.leaves.push((idx, Leaves::Thrown, unwind));
        }
    }

    /// A call an `INIT_*` began, here or before the trace reached the
    /// frame, whose arguments sent before it the trace knows nothing of;
    /// `ahead`, the events after it, say whether compiled code links it
    /// into the frame's call chain at once.
    fn init_call(&mut self, callee: Callee, began: Began, ahead: &[Event]) -> Result<(), Abort> {
        let frame = self.running()?;
        let init = match self.ops.last() {
            Some(Op::Line(line)) if began == Began::Here && line.form == Form::Inline => {
                Some(line.opcode)
            }
            _ => None,
        };
        let inline = init.is_some();
        let code = self.meta(frame).code;
        let after = (ahead.iter())
            .filter(|event| !matches!(event, Event::InitCall { .. }))
            .map_while(|event| match event {
                Event::Op { opline, .. } => code.oplines.get(opline.0 as usize).map(|op| op.opcode),
                Event::InitCall { .. } | Event::Enter { .. } | Event::Back => None,
            });
        let delayed = init.is_some_and(|init| !needs_call_chain(init, after));
        let plain = self
            .pushing
            .take()
            .filter(|plain| began == Began::Here && callee.user() == plain.callee());
        // A function's call by its name serves the function recorded: no
        // function is declared twice, and an unqualified name in a
        // namespace, which another may answer, is held to the one recorded
        // by what its slot holds as php's trace holds it. A method's or a
        // dynamic call's may serve another, so how it takes its arguments
        // is not known.
        let named = plain.as_ref().is_some_and(|plain| plain.scope().is_some())
            || began == Began::Here
                && matches!(
                    self.ops.last(),
                    Some(Op::Line(line)) if pushes_inline(line.opcode)
                );
        self.begin();
        self.calls.push(Call {
            inline,
            delayed,
            args: Vec::new(),
            returns: match callee {
                Callee::Native(returns) => Some(returns),
                Callee::User(_) | Callee::Request => None,
            },
            callee,
            pushed: plain.as_ref().map(|plain| {
                if plain.checks() {
                    Pushed::Checks
                } else {
                    Pushed::Activates
                }
            }),
            passed: Some(plain.as_ref().map_or(0, PlainCall::count)),
            named,
            last_send: LastSend::Unknown,
        });
        self.ops.push(Op::InitCall {
            frame,
            callee,
            began,
            plain,
            room: PushRoom::Unchecked,
        });
        Ok(())
    }

    /// Whether the call under way takes its argument numbered `n` by
    /// value, where the trace knows its callee, a function's call by its
    /// name, php's `ARG_SHOULD_BE_SENT_BY_REF` over `frame->call->func`.
    /// A method's call may serve a method of another class than the one
    /// recorded, which takes the same argument by reference, so its sends
    /// are the handlers':
    ///
    /// ```php
    /// class A { function f($x) { return $x + 1; } }
    /// class B { function f(&$x) { $x++; return $x; } }
    /// function g($objs) { $v = 1; $s = 0; foreach ($objs as $o) { $s += $o->f($v); } return [$s, $v]; }
    /// echo json_encode(g([new A, new A, new A, new B, new B, new A])), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [15,3]
    /// ```
    fn sends_by_value(&self, n: u32) -> Option<bool> {
        let call = self.calls.last().filter(|call| call.named)?;
        let callee = call.callee.user()?;
        let params = self.view.parameters(callee)?;
        Some(sent_by_value(params, n))
    }

    /// The opline as the trace compiles it, where the callee the trace
    /// knows settles how an argument is sent: a `SEND_VAR_EX`, a
    /// `SEND_VAR_NO_REF_EX` or a `SEND_FUNC_ARG` to a parameter taken by
    /// value is a `SEND_VAR`, whose variable it leaves as it was
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:2373-2382`, `:5324-5378`); a
    /// variable's `SEND_VAR_EX` or `SEND_FUNC_ARG` to one taken by
    /// reference is a `SEND_REF`, as `zend_jit_send_var` hands it to
    /// `zend_jit_send_ref` (`Z/ext/opcache/jit/zend_jit_ir.c:9662-9669`);
    /// and a `FETCH_DIM_FUNC_ARG` after a `CHECK_FUNC_ARG`
    /// that found its argument sent by value is a `FETCH_DIM_R`, as php's
    /// code generator falls through to it (`:5822-5829`, the guards
    /// `:2001-2010`). A `CHECK_FUNC_ARG` records what it found on the call
    /// (`:2199-2215`). Every other opline is itself. A by-reference
    /// parameter, a by-value one, what a call returned by value and by
    /// reference, and a variable undefined at its send, each to a function
    /// called by its name:
    /// ```php
    /// if (true) { function inc(&$x) { $x++; } function both(&$a, $b) { $a += $b; } function val($x) { $x++; return $x; } function &ref() { static $v = 1; $v++; return $v; } function arr($a) { return count($a); } }
    /// function loop($n) { $s = 0; $u = 0; $l = [1, 2]; for ($i = 0; $i < $n; $i++) { inc($s); both($s, $i); $v = val($i); $u += val(val($v)) + val(ref()) + arr($l) + val(arr(array_merge($l, [$i]))); } return [$s, $u]; }
    /// echo json_encode(loop(30)), "\n";
    /// function undef($a) { return $a === null ? 1 : 2; }
    /// function warned($n) { $s = 0; for ($i = 0; $i < $n; $i++) { if ($i == 25) { $s += undef($u); } else { $v = $i; $s += undef($v); } unset($v); } return $s; }
    /// echo warned(30), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [465,1230]
    ///
    /// Warning: Undefined variable $u in %SCRATCH%/witness.php on line 6
    /// 59
    /// ```
    ///
    /// ```php
    /// if (true) { function inc(&$x) { $x++; } function both(&$a, $b) { $a += $b; } function val($x) { $x++; return $x; } function &ref() { static $v = 1; $v++; return $v; } function arr($a) { return count($a); } }
    /// function loop($n) { $s = 0; $u = 0; $l = [1, 2]; for ($i = 0; $i < $n; $i++) { inc($s); both($s, $i); $v = val($i); $u += val(val($v)) + val(ref()) + arr($l) + val(arr(array_merge($l, [$i]))); } return [$s, $u]; }
    /// echo json_encode(loop(30)), "\n";
    /// function undef($a) { return $a === null ? 1 : 2; }
    /// function warned($n) { $s = 0; for ($i = 0; $i < $n; $i++) { if ($i == 25) { $s += undef($u); } else { $v = $i; $s += undef($v); } unset($v); } return $s; }
    /// echo warned(30), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [465,1230]
    ///
    /// Warning: Undefined variable $u in %SCRATCH%/witness.php on line 6
    /// 59
    /// ```
    fn as_compiled(&mut self, mut op: Opline) -> Opline {
        let numbered = op.op2_type == OperandKind::Unused;
        match op.opcode {
            Opcode::SendVarEx | Opcode::SendVarNoRefEx | Opcode::SendFuncArg
                if numbered && (op.opcode == Opcode::SendFuncArg || op.op2 <= MAX_ARG_FLAG_NUM) =>
            {
                match self.sends_by_value(op.op2) {
                    Some(true) => op.opcode = Opcode::SendVar,
                    Some(false)
                        if op.opcode != Opcode::SendVarNoRefEx
                            && op.op1_type == OperandKind::Cv =>
                    {
                        op.opcode = Opcode::SendRef;
                    }
                    _ => {}
                }
            }
            Opcode::CheckFuncArg => {
                let by_value = numbered
                    && op.op2 <= MAX_ARG_FLAG_NUM
                    && self.sends_by_value(op.op2) == Some(true);
                let found = if by_value {
                    LastSend::ByValue
                } else {
                    LastSend::Unknown
                };
                if let Some(call) = self.calls.last_mut() {
                    call.last_send = found;
                }
            }
            Opcode::FetchDimFuncArg
                if self
                    .calls
                    .last()
                    .is_some_and(|call| call.last_send == LastSend::ByValue) =>
            {
                op.opcode = Opcode::FetchDimR;
            }
            _ => {}
        }
        op
    }

    /// What compiled code serves of the inline opline `op` of `frame` in
    /// place, `next` being the record after it: the push of an `INIT_*`
    /// whose callee the trace recorded where php's needs no guard, an
    /// `INIT_FCALL` of a user function of the engine's arena or a call of
    /// the frame's own op array, php's recursive call
    /// ([`Builder::named_at`] the rest of the calls by name)
    /// (`zend_jit_init_fcall`, `Z/ext/opcache/jit/zend_jit_ir.c:8790-8906`),
    /// which the view took as a plain call; a `SEND_VAL_EX` to a callee the
    /// trace recorded that takes the argument by value, decided as php's
    /// `zend_jit_send_val` decides it as it compiles (`:9466-9506`); the
    /// `DO_*CALL` that makes a call pushed in place; and a function's
    /// `RETURN` where no observer is declared.
    fn in_place(&self, frame: Frame, op: &Opline, next: Option<&Event>) -> Option<InPlace> {
        let func = self.frames[usize::from(frame.0)].func;
        match op.opcode {
            Opcode::InitFcall | Opcode::InitFcallByName | Opcode::InitNsFcallByName => {
                let Some(Event::InitCall {
                    callee,
                    began: Began::Here,
                }) = next
                else {
                    return None;
                };
                let plain = match callee {
                    // A function named in full, or the frame's own.
                    Callee::User(callee) if op.opcode == Opcode::InitFcall || *callee == func => {
                        (self.view.calls_of(*callee, op.extended_value))
                            .find(|plain| plain.scope().is_none())?
                    }
                    // A native named in full, which php's trace pushes as
                    // it pushes a user function's call
                    // (`zend_jit_push_call_frame`, `Z/ext/opcache/jit/zend_jit_ir.c:8466-8650`).
                    Callee::Native(_) if op.opcode == Opcode::InitFcall => {
                        let code = self.meta(frame).code;
                        let name = code.literals.get(op.op2 as usize)?.str_bytes()?;
                        self.view.native_call(name, op.extended_value)?
                    }
                    Callee::User(_) | Callee::Native(_) | Callee::Request => return None,
                };
                Some(InPlace::Push(plain.clone()))
            }
            Opcode::SendValEx if op.op2_type == OperandKind::Unused => {
                self.sends_by_value(op.op2)?.then_some(InPlace::Send)
            }
            Opcode::DoUcall | Opcode::DoFcall | Opcode::DoFcallByName => {
                if !matches!(next, Some(Event::Enter { .. })) {
                    return None;
                }
                let call = self.calls.last()?;
                let checks = match call.pushed? {
                    Pushed::Activates => ArgChecks::default(),
                    Pushed::Checks => self.arg_checks(call)?,
                };
                Some(InPlace::Make(checks, self.defaults(call)?))
            }
            Opcode::Return => self.leaves_in_place(frame).then_some(InPlace::Leave),
            _ => None,
        }
    }

    /// What compiled code checks of `call`'s arguments where the call's
    /// activation passes over checking `RECV`s ([`PlainCall::checks`]),
    /// as php's trace compiles `zend_jit_recv`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:10825-10873`): nothing of an
    /// argument the trace knows, from what its send wrote, of a type its
    /// parameter declares and holds as it is ([`held_as_is`]); the
    /// declared type's mask, the `RECV`'s own `op2.num` or a passed
    /// `RECV_INIT`'s, held as the call is made, of any other, a reference
    /// among them, whose value the check reads ([`ArgChecks`]), an
    /// argument it fails handed to its `RECV`'s handler alone
    /// (`Emit::verify_args`). None, the call kernel's, which serves the
    /// prologue, where a parameter is variadic or declares a type no mask
    /// holds, a class. A long sent to an `int` is made in place with no
    /// check; one sent to a `float`, and a string sent to an `int`, are
    /// checked, the handler coercing them; arrays by reference are
    /// checked in place, and a string by reference to an `int` is coerced
    /// by the handler:
    ///
    /// ```php
    /// function fib(int $n): int { if ($n < 2) { return $n; } return fib($n - 1) + fib($n - 2); }
    /// function half(float $x) { return $x / 2; }
    /// function halves($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += half($i); } return $s; }
    /// function twice(int $n) { return $n * 2; }
    /// function many($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += twice($i % 3 == 0 ? "4" : $i); } return $t; }
    /// function grow(array &$a, int &$n) { $a[] = $n; $n += 1; }
    /// function grows($k) { $a = []; $n = 0; for ($i = 0; $i < $k; $i++) { if ($i == 5) { $n = "5"; } grow($a, $n); } return [count($a), $n]; }
    /// var_dump(fib(15), halves(10), many(9));
    /// echo json_encode(grows(9)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// int(610)
    /// float(22.5)
    /// int(78)
    /// [9,9]
    /// ```
    ///
    /// ```php
    /// function grow(array &$a, int &$n) { $a[] = $n; $n += 1; }
    /// function grows($k) { $a = []; $n = 0; for ($i = 0; $i < $k; $i++) { if ($i == 5) { $n = "5"; } grow($a, $n); } return [count($a), $n]; }
    /// echo json_encode(grows(9)), "\n";
    /// function strict(array &$a) { return count($a); }
    /// function strictly($k) { $a = [1]; $t = 0; for ($i = 0; $i < $k; $i++) { if ($i == 6) { $a = 3; } $t += strict($a); } return $t; }
    /// try { strictly(9); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [9,9]
    /// strict(): Argument #1 ($a) must be of type array, int given, called in %s on line 6
    /// ```
    fn arg_checks(&self, call: &Call) -> Option<ArgChecks> {
        let callee = call.callee.user()?;
        let params = self.view.parameters(callee)?;
        let code = self.view.op_array(callee)?;
        let passed = call.passed? as usize;
        let mut checks = ArgChecks::default();
        for (at, param) in params.iter().enumerate() {
            if param.variadic {
                return None;
            }
            let Some(ty) = param.ty.as_ref().filter(|_| at < passed) else {
                continue;
            };
            let held = held_as_is(ty)?;
            let sent = call
                .args
                .get(at)
                .and_then(|sent| sent.types)
                .filter(|sent| {
                    sent.kinds() != Types::NONE && !sent.meets(Types::REF.or(Types::UNDEF))
                });
            if sent.is_some_and(|sent| !param.by_ref && sent.kinds().within(held)) {
                continue;
            }
            // The `RECV`'s mask, its `op2.num`, or a passed `RECV_INIT`'s,
            // which checks the declared type's as php's
            // `zend_verify_recv_arg_type` does.
            code.oplines.get(at).filter(|op| {
                matches!(op.opcode, Opcode::Recv | Opcode::RecvInit) && op.op1 as usize == at + 1
            })?;
            checks = checks.with(at, NonZeroU32::new(ty.mask())?)?;
        }
        Some(checks)
    }

    /// What compiled code copies into the parameters of `call` it is
    /// passed no argument for ([`Defaults`]), as php's trace compiles
    /// `zend_jit_recv_init` past the arguments its frame knows were
    /// passed, copying the default (`jit_ZVAL_COPY_CONST`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:10886-10950`), and checking no
    /// type of what it copied, as php's `RECV_INIT` checks none. None,
    /// the call kernel's, which serves the prologue, where such a
    /// parameter has no `RECV_INIT`, or its default names a constant
    /// expression, which php evaluates, or is counted. Defaults of each
    /// kind are copied, a `float` parameter's integer default the double
    /// the compiler made of it, a constant's and an expression's left to
    /// the kernel:
    ///
    /// ```php
    /// const STEP = 3;
    /// function f($a, $b = 2, $c = 1.5, $d = "s", $e = [1], $f = null) { return $a + $b + $c + strlen($d) + count($e) + ($f ?? 0); }
    /// function g($a, $b = STEP) { return $a + $b; }
    /// function h($a, int $b = 4, float $x = 1) { return var_export($x, true) === '1.0' ? $a * $b : 0; }
    /// function k($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += f($i) + f($i, 1) + g($i) + h($i); } return $s; }
    /// var_dump(k(10));
    /// ```
    /// ```expect regime=jit-force process
    /// float(445)
    /// ```
    fn defaults(&self, call: &Call) -> Option<Defaults> {
        let passed = call.passed? as usize;
        let Some(callee) = call.callee.user() else {
            return Some(Defaults::default());
        };
        let params = self.view.parameters(callee)?;
        let code = self.view.op_array(callee)?;
        let mut defaults = Defaults::default();
        for at in passed..params.len() {
            let recv = code
                .oplines
                .get(at)
                .filter(|op| op.opcode == Opcode::RecvInit && op.op1 as usize == at + 1)?;
            let literal = code
                .literals
                .get(recv.op2 as usize)
                .filter(|literal| !literal.is_refcounted() && literal.as_const_expr().is_none())?;
            let address = NonZeroUsize::new(std::ptr::from_ref(literal).addr())?;
            defaults = defaults.with(at, address)?;
        }
        Some(defaults)
    }

    /// What a send wrote into the call under way, where it sends a value,
    /// and what moves its count, so its `DO_*CALL` is its handler's:
    /// ```php
    /// function f($a = 1, $b = 2, $c = 3) { return $a * 100 + $b * 10 + $c; }
    /// if (true) { function g($a = 1, $b = 2, $c = 3) { return $a * 100 + $b * 10 + $c; } }
    /// function loop($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += f(b: $i) + f(c: 1, a: 2) + g(b: $i) + g(1, c: $i) + g(...[1, 2]) + f(...[3]); } return $s; }
    /// echo loop(30), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 38925
    /// ```
    ///
    /// ```php
    /// function f($a = 1, $b = 2, $c = 3) { return $a * 100 + $b * 10 + $c; }
    /// if (true) { function g($a = 1, $b = 2, $c = 3) { return $a * 100 + $b * 10 + $c; } }
    /// function loop($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += f(b: $i) + f(c: 1, a: 2) + g(b: $i) + g(1, c: $i) + g(...[1, 2]) + f(...[3]); } return $s; }
    /// echo loop(30), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 38925
    /// ```
    fn record_send(&mut self, frame: Frame, op: &Opline, op1: Operand) {
        // php takes the count as unknown past these
        // (`Z/ext/opcache/jit/zend_jit_ir.c:10050-10054`); a send by name
        // comes before the `CHECK_UNDEF_ARGS` the compiler leaves after it.
        if matches!(
            op.opcode,
            Opcode::SendUnpack | Opcode::SendArray | Opcode::CheckUndefArgs
        ) && let Some(call) = self.calls.last_mut()
        {
            call.passed = None;
        }
        let by_value = matches!(
            op.opcode,
            Opcode::SendVal | Opcode::SendVar | Opcode::SendValEx
        );
        let sends = by_value
            || matches!(
                op.opcode,
                Opcode::SendVarEx
                    | Opcode::SendVarNoRef
                    | Opcode::SendVarNoRefEx
                    | Opcode::SendFuncArg
                    | Opcode::SendRef
                    | Opcode::SendUser
            );
        if !sends || op.op2_type != OperandKind::Unused {
            return;
        }
        let types = by_value.then(|| assigned(self.types_of(frame, op1)));
        let reg = match op1 {
            Operand::Value(value) if by_value && self.val(value).home == Home::Register => {
                Some(value)
            }
            _ => None,
        };
        let Some(call) = self.calls.last_mut() else {
            return;
        };
        let at = op.op2.saturating_sub(1) as usize;
        if call.args.len() <= at {
            call.args.resize(at + 1, Sent::default());
        }
        if let Some(arg) = call.args.get_mut(at) {
            *arg = Sent { types, reg };
        }
    }

    /// The frame a call pushed runs, its parameters what was sent.
    fn enter(&mut self, func: FuncRef) -> Result<(), Abort> {
        let caller = self.running()?;
        let idx = self.begin();
        let call = self.calls.pop();
        let stores: Vec<ValueId> = (self.slots[usize::from(caller.0)].iter())
            .flatten()
            .copied()
            .filter(|value| self.val(*value).home == Home::Register)
            .collect();
        let frame = self.frame(func, Place::Callee(caller), self.call_at, None)?;
        self.stack.push(frame);
        let meta = self.meta(frame);
        for param in params(meta.code, meta.parameters) {
            let sent = (call.as_ref())
                .and_then(|call| call.args.get(param.number.saturating_sub(1) as usize))
                .copied()
                .unwrap_or_default();
            let types = param_types(param.declared, sent.types);
            let slot = Slot {
                frame,
                var: param.cv,
            };
            let value = self.value(slot, types, Def::Op(idx), Home::Memory);
            self.set(slot, value);
            if let (Declared::Untyped, Some(reg)) = (param.declared, sent.reg) {
                self.held.insert(value, reg);
            }
        }
        for value in &stores {
            // The first call that stores it: a caller makes calls one after
            // another and stores the same value at each.
            self.spilled.entry(*value).or_insert(idx);
        }
        self.ops.push(Op::Enter { frame, stores });
        Ok(())
    }

    /// A return: back into the frame that called, or, past the trace's
    /// lowest frame, into a caller whose call the guard holds: where the
    /// trace goes on there, the opline recorded next, or, past the last,
    /// the header its recursive return goes back to or the opline it
    /// links at, as php's `zend_jit_leave_func` holds the instruction
    /// pointer to the next record's opline
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:11188-11224`).
    ///
    /// That opline is the one past the calls the caller began before the
    /// return, its fake inits, as php's code steps over the `INIT_CALL`
    /// records after a `BACK` to the opline it holds the pointer to
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:11210-11214`). A return trace
    /// rooted inside an argument of an outer call opens with its fake
    /// init, and its recursive return goes back there only where the
    /// caller stands at that send; where it stands past the outer call, the
    /// trace escapes. Under the recipe's hot thresholds of one, `Ack`'s
    /// return trace at its inner call's landing returns into a caller past
    /// its outer call:
    ///
    /// ```php
    /// function Ack($m, $n) {
    ///     if ($m == 0) return $n + 1;
    ///     if ($n == 0) return Ack($m - 1, 1);
    ///     return Ack($m - 1, Ack($m, ($n - 1)));
    /// }
    /// echo Ack(3, 7), "\n";
    /// ```
    /// ```expect regime=jit-tracing process
    /// 1021
    /// ```
    ///
    /// How php compiles the return decides how the guard leaves
    /// ([`Builder::handled`]). A `RETURN` php compiles with
    /// `zend_jit_return` escapes, taking no exit, as `zend_jit_leave_func`
    /// jumps to `jit_stub_trace_escape`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:11199-11240`), whether this tier
    /// leaves its frame in place or steps it. A return php hands to its
    /// handler holds the opline with an exit within its line, where
    /// `zend_jit_trace_handler` takes an exit point past the handler
    /// (`:17200-17330`). That exit names no opline, so no other place takes
    /// it, and it stores nothing and knows no type ([`At::Landed`]): where
    /// it is taken, the caller the interpreter stands in is not the one
    /// the trace recorded. A
    /// return trace whose `RETURN_BY_REF` returns into the file's code,
    /// away from its recursion's header, takes that exit and grows a side
    /// trace there, #3688's program:
    ///
    /// ```php
    /// function h() { return 1; }
    /// function &p($n) { static $z = 0; if ($n == 0) { return $z; } $x = &p($n - 1); h(); return $x; }
    /// $r = &p(6); echo $r, "\n";
    /// for ($i = 0; $i < 5; $i++) { $r = &p(8); echo $r, "\n"; }
    /// echo "done\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 0
    /// 0
    /// 0
    /// 0
    /// 0
    /// 0
    /// done
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:4
    /// ---- TRACE 1 abort (deep recursion)
    ///
    /// ---- TRACE 1 start (enter) p() %s:3
    /// ---- TRACE 1 stop (recursive call)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 1 p() %s:3
    /// ---- TRACE 2 start (side trace 1/1) p() %s:3
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    ///
    /// ---- TRACE 3 start (return) p() %s:3
    /// ---- TRACE 3 stop (recursive return)
    /// ---- TRACE 3 compiled
    ///
    ///      TRACE 3 exit 2 $main() %s:4
    /// ---- TRACE 4 start (side trace 3/2) $main() %s:4
    /// ---- TRACE 4 stop (link to 1)
    /// ---- TRACE 4 compiled
    ///
    ///      TRACE 4 exit 0 $main() %s:5
    /// ---- TRACE 5 start (side trace 4/0) $main() %s:5
    /// ---- TRACE 5 stop (link to 1)
    /// ---- TRACE 5 compiled
    ///
    ///      TRACE 5 exit 3 $main() %s:6
    /// ---- TRACE 6 start (side trace 5/3) $main() %s:6
    /// ---- TRACE 6 stop (return)
    /// ---- TRACE 6 compiled
    /// ```
    ///
    /// The same where every guard fails once, the exit landing the
    /// interpreter in each caller a return past the root steps into:
    ///
    /// ```php
    /// function h() { return 1; }
    /// function &p($n) { static $z = 0; if ($n == 0) { return $z; } $x = &p($n - 1); h(); return $x; }
    /// $r = &p(6); echo $r, "\n";
    /// for ($i = 0; $i < 5; $i++) { $r = &p(8); echo $r, "\n"; }
    /// echo "done\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 0
    /// 0
    /// 0
    /// 0
    /// 0
    /// 0
    /// done
    /// ```
    fn back(&mut self, ahead: &[Event]) -> Result<(), Abort> {
        let idx = self.begin();
        let from = self.running()?;
        self.stack.pop();
        let next = (ahead.iter()).find(|event| !matches!(event, Event::InitCall { .. }));
        let next_op = match next {
            Some(Event::Op { func, opline, .. }) => Some((*func, *opline)),
            Some(_) => None,
            None if self.repeat == Some(Repeat::Return) => self.header_line(),
            None => self.rec.link_at,
        };
        let (caller, call_at, guard) = if let Some(&caller) = self.stack.last() {
            (caller, self.meta(from).called_at, None)
        } else {
            let func = next_op.map_or(self.rec.root, |(func, _)| func);
            let caller = self.frame(func, Place::Caller(from), None, next_op.map(|(_, at)| at))?;
            self.stack.push(caller);
            let call_at = next_op
                .and_then(|(_, at)| at.0.checked_sub(1))
                .map(OplineIdx);
            (caller, call_at, next_op)
        };
        // The return's line, just before: a step served it, or it left its
        // frame in place.
        let escapes = match guard {
            Some((func, opline)) => {
                let at = OpId(idx.0.wrapping_sub(1));
                let handled = match self.ops.get(at.index()) {
                    Some(Op::Line(line)) if line.frame == from => self.handled(line),
                    _ => return Err(mismatch("a return past the lowest frame with no line")),
                };
                if handled {
                    self.guards.push(Placed {
                        at: GuardAt::Within(at),
                        check: Check::Stands { func, opline },
                        exit: draft(Point::After(idx), caller, At::Landed),
                    });
                    None
                } else {
                    guard
                }
            }
            None => None,
        };
        let code = self.meta(caller).code;
        let out = call_at
            .and_then(|at| code.oplines.get(at.0 as usize))
            .filter(|call| is_call(call.opcode))
            .and_then(|call| slot_of(caller, call.out_type, call.out))
            .map(|slot| {
                let returned = self.returned;
                self.define(slot, (returned, None), idx, Form::Step)
            });
        self.returned = Types::NULL;
        self.ops.push(Op::Back {
            frame: caller,
            out,
            escapes,
        });
        Ok(())
    }

    /// Whether php's trace hands the return `line` to its handler,
    /// `zend_jit_trace_handler`, and not to `zend_jit_return` and
    /// `zend_jit_leave_func`: a `RETURN_BY_REF`, which it compiles no
    /// other way, and a `RETURN` of a file's own code or of an operand
    /// that may be undefined (`Z/ext/opcache/jit/zend_jit_trace.c:5580-5595`),
    /// whether this tier steps the return or leaves its frame in place.
    /// The `RETURN` of `eval`'d code php hands it too is never recorded
    /// here (revision 120).
    ///
    /// A return trace whose `RETURN` of a string php compiles with
    /// `zend_jit_return`, as this tier leaves it in place ([`returned`]),
    /// so where it returns into the file's loop, away from its recursion's
    /// header, it escapes and writes no exit line, and php's interpreter
    /// goes on to root a trace at the loop:
    ///
    /// ```php
    /// function h() { return 1; }
    /// function p($n) { if ($n == 0) { return ""; } $x = p($n - 1); h(); $x .= "a"; return $x; }
    /// for ($i = 0; $i < 5; $i++) { $r = p(8); }
    /// echo strlen($r), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 8
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:4
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) $main() %s:4
    /// ---- TRACE 1 abort (deep recursion)
    ///
    /// ---- TRACE 1 start (enter) p() %s:3
    /// ---- TRACE 1 stop (recursive call)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 1 p() %s:3
    /// ---- TRACE 2 start (side trace 1/1) p() %s:3
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    ///
    /// ---- TRACE 3 start (return) p() %s:3
    /// ---- TRACE 3 stop (recursive return)
    /// ---- TRACE 3 compiled
    ///
    /// ---- TRACE 4 start (loop) $main() %s:4
    /// ---- TRACE 4 stop (link to 1)
    /// ---- TRACE 4 compiled
    ///
    /// ---- TRACE 5 start (return) $main() %s:4
    /// ---- TRACE 5 stop (link to 4)
    /// ---- TRACE 5 compiled
    ///
    ///      TRACE 4 exit 1 $main() %s:5
    /// ---- TRACE 6 start (side trace 4/1) $main() %s:5
    /// ---- TRACE 6 stop (return)
    /// ---- TRACE 6 compiled
    /// ```
    fn handled(&self, line: &Line) -> bool {
        match line.opcode {
            Opcode::ReturnByRef => true,
            Opcode::Return => {
                let func = self.frames[usize::from(line.frame.0)].func;
                self.view.is_main(func) != Some(false)
                    || matches!(line.op1, Operand::Value(value)
                        if self.val(value).types.meets(Types::UNDEF))
            }
            _ => false,
        }
    }

    /// The clobber rule (`jit-design.md` §4.2): after a call-effect
    /// kernel, every value of a live frame that a symbol table reaches or
    /// that may be a reference is read again.
    ///
    /// A kernel with no `call` effect may call user code too: `INIT_ARRAY`'s
    /// and `ADD_ARRAY_ELEMENT`'s handler raise a null key's deprecation
    /// through the error handler and release a duplicate key's value
    /// (`Z/Zend/zend_vm_def.h:6279-6292`), and the release kernel compiled
    /// code calls for a temporary runs a destructor. None clobbers, as
    /// none has to: what that code can write is a variable a symbol table
    /// reaches, whose every write [`Builder::define`] types as anything
    /// and whose reads [`Builder::guard_use`] narrows for the reading
    /// opline alone; a reference, whose inner type is held at every read;
    /// or an array, which it separates before it writes it. So no fact the
    /// trace keeps is one such code can change, as php's code generator
    /// calls such an opline's handler and keeps every type and register
    /// across it (`Z/ext/opcache/jit/zend_jit_trace.c:6480-6493`). An
    /// error handler that writes a reference's inner value, a global, a
    /// file's own variable and a reference's array at either site:
    ///
    /// ```php
    /// function h() { global $g, $w; static $c = 0; if (++$c % 5 == 0) { $g = $c + 0.5; $GLOBALS['m'] = $c + 0.5; $GLOBALS['l'] = ["a" => 1, 2 => $c + 0.5]; $w(); } return true; }
    /// set_error_handler('h');
    /// function f(&$r, array &$v, $n, $k) {
    ///     global $w; static $s = 0; $w = function () use (&$s) { $s = 0.5; }; $t = 0;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $r = $i; $s = $i; $v = [1, 2, 3]; $x = $v[2]; $a = [$k => 1]; $t = $t + $r + $s + $v[2];
    ///         $r = $i; $s = $i; $v = [1, 2, 3]; $x = $v[2]; $a = [1, $k => 2]; $t = $t + $r + $s + $v[2];
    ///     }
    ///     return $t;
    /// }
    /// $g = 0; $l = []; $t = 0; $k = null;
    /// echo f($g, $l, 8, null), " ";
    /// for ($i = 0; $i < 8; $i++) { $m = $i; $a = [$k => 1]; $t = $t + $m; }
    /// echo $t, "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 189.5 45.5
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function h() { global $g, $w; static $c = 0; if (++$c % 5 == 0) { $g = $c + 0.5; $GLOBALS['m'] = $c + 0.5; $GLOBALS['l'] = ["a" => 1, 2 => $c + 0.5]; $w(); } return true; }
    /// set_error_handler('h');
    /// function f(&$r, array &$v, $n, $k) {
    ///     global $w; static $s = 0; $w = function () use (&$s) { $s = 0.5; }; $t = 0;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $r = $i; $s = $i; $v = [1, 2, 3]; $x = $v[2]; $a = [$k => 1]; $t = $t + $r + $s + $v[2];
    ///         $r = $i; $s = $i; $v = [1, 2, 3]; $x = $v[2]; $a = [1, $k => 2]; $t = $t + $r + $s + $v[2];
    ///     }
    ///     return $t;
    /// }
    /// $g = 0; $l = []; $t = 0; $k = null;
    /// echo f($g, $l, 8, null), " ";
    /// for ($i = 0; $i < 8; $i++) { $m = $i; $a = [$k => 1]; $t = $t + $m; }
    /// echo $t, "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 189.5 45.5
    /// ```
    ///
    /// A destructor that writes a reference's inner value, called at a
    /// duplicate key, at `FETCH_DIM_R`'s release of its temporary and at
    /// `TYPE_CHECK`'s:
    ///
    /// ```php
    /// class D { function __destruct() { global $g; static $c = 0; if (++$c % 7 == 0) { $g = $c + 0.5; } } }
    /// function f(&$r, $n) {
    ///     $t = 0; $k = 1;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $o = new D; $r = $i; $a = [$k => $o, $k => ($o = null)]; $t = $t + $r;
    ///         $o = new D; $r = $i; $v = [$o, $o = null][1]; $t = $t + $r;
    ///         $o = new D; $r = $i; $v = is_array([$o, $o = null]); $t = $t + $r;
    ///     }
    ///     return $t;
    /// }
    /// $g = 0;
    /// var_dump(f($g, 10));
    /// ```
    /// ```expect regime=jit-force process
    /// float(186)
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class D { function __destruct() { global $g; static $c = 0; if (++$c % 7 == 0) { $g = $c + 0.5; } } }
    /// function f(&$r, $n) {
    ///     $t = 0; $k = 1;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $o = new D; $r = $i; $a = [$k => $o, $k => ($o = null)]; $t = $t + $r;
    ///         $o = new D; $r = $i; $v = [$o, $o = null][1]; $t = $t + $r;
    ///         $o = new D; $r = $i; $v = is_array([$o, $o = null]); $t = $t + $r;
    ///     }
    ///     return $t;
    /// }
    /// $g = 0;
    /// var_dump(f($g, 10));
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// float(186)
    /// ```
    ///
    /// The leave of a frame whose variables own something, compiled in
    /// place, frees them inline and calls the `free_cv` kernel for a
    /// destructor, with no `call` effect, so the caller keeps every fact
    /// across it as php's code keeps them across `zend_jit_free_cv`. A
    /// destructor served at the leave of an inlined callee that writes the
    /// reference the caller holds and a global array, the caller's own
    /// array kept as it was:
    ///
    /// ```php
    /// class D { function __destruct() { global $g; static $c = 0; if (++$c % 7 == 0) { $g = $c + 0.5; $GLOBALS['l'][1] = $c + 0.5; } } }
    /// function h($i) { $d = new D; return $i; }
    /// function f(&$r, $n) {
    ///     global $l; $t = 0; $v = [1, 2, 3]; $v[] = 4;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $r = $i; $l = [1, 2, 3]; $t = $t + h($i) + $r + $l[1] + $v[1];
    ///     }
    ///     return $t;
    /// }
    /// $g = 0; $l = [];
    /// echo f($g, 30), " ", $g, "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 1060 29
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class D { function __destruct() { global $g; static $c = 0; if (++$c % 7 == 0) { $g = $c + 0.5; $GLOBALS['l'][1] = $c + 0.5; } } }
    /// function h($i) { $d = new D; return $i; }
    /// function f(&$r, $n) {
    ///     global $l; $t = 0; $v = [1, 2, 3]; $v[] = 4;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $r = $i; $l = [1, 2, 3]; $t = $t + h($i) + $r + $l[1] + $v[1];
    ///     }
    ///     return $t;
    /// }
    /// $g = 0; $l = [];
    /// echo f($g, 30), " ", $g, "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 1060 29
    /// ```
    ///
    /// A variable a guard held a reference keeps php's `guarded_reference`
    /// across a native's call, which defines no variable of its caller,
    /// so the `ASSIGN` to it after `str_repeat` takes no exit of its own,
    /// and the direction's is php's exit 5:
    ///
    /// ```php
    /// function f() { $x = 0; $r = &$x; $c = 0; for ($i = 0; $i < 100; $i++) { $x = $i; $c += strlen(str_repeat("a", 2)); $x = $c; if ($i > 80) { $c++; } } return $c + $x; }
    /// echo f(), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 437
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:3
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %s:2
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 5 f() %s:2
    /// ---- TRACE 2 start (side trace 1/5) f() %s:2
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    ///
    ///      TRACE 1 exit 2 f() %s:2
    /// ---- TRACE 3 start (side trace 1/2) f() %s:2
    /// ---- TRACE 3 stop (return)
    /// ---- TRACE 3 compiled
    /// ```
    fn clobber(&mut self, idx: OpId) {
        let live: Vec<Frame> = self.stack.clone();
        for frame in live {
            let held: Vec<(u32, ValueId)> = self.slots[usize::from(frame.0)]
                .iter()
                .enumerate()
                .filter_map(|(var, value)| Some((u32::try_from(var).ok()?, (*value)?)))
                .collect();
            for (var, value) in held {
                let slot = Slot { frame, var };
                if self.aliased(slot) || self.val(value).types.meets(Types::REF) {
                    let types = if var < self.meta(frame).code.num_cvs {
                        Types::UNKNOWN
                    } else {
                        Types::UNKNOWN.without(Types::UNDEF)
                    };
                    let reloaded = self.value(slot, types, Def::Reload(idx), Home::Memory);
                    // php's `guarded_reference` is the SSA variable's, and
                    // a call defines no variable of its caller: a variable
                    // a guard held a reference holds one still.
                    if self.held_references.contains(&value) {
                        self.held_references.insert(reloaded);
                    }
                    self.set(slot, reloaded);
                }
            }
        }
    }

    // ------------------------------------------------------------ values

    /// The place of the next op, its snapshot taken.
    fn begin(&mut self) -> OpId {
        let snapshot = self.snapshot();
        self.snapshots.push(snapshot);
        OpId(u32::try_from(self.ops.len()).unwrap_or(u32::MAX))
    }

    fn val(&self, value: ValueId) -> &Value {
        &self.values[value.0 as usize]
    }

    fn val_mut(&mut self, value: ValueId) -> &mut Value {
        &mut self.values[value.0 as usize]
    }

    fn snapshot(&self) -> Snapshot {
        let mut values = Vec::new();
        for &frame in &self.stack {
            for (var, value) in self.slots[usize::from(frame.0)].iter().enumerate() {
                if let (Some(value), Ok(var)) = (value, u32::try_from(var)) {
                    values.push((Slot { frame, var }, *value));
                }
            }
        }
        Snapshot {
            values,
            calls: self.calls.iter().filter(|call| call.inline).count(),
            delayed: self.calls.iter().any(|call| call.delayed),
        }
    }

    fn value(&mut self, slot: Slot, types: Types, def: Def, home: Home) -> ValueId {
        let id = ValueId(u32::try_from(self.values.len()).unwrap_or(u32::MAX));
        self.born.push(types);
        self.values.push(Value {
            slot,
            types,
            range: None,
            def,
            home,
            unguarded: None,
        });
        id
    }

    fn set(&mut self, slot: Slot, value: ValueId) {
        if let Some(at) = self.slots[usize::from(slot.frame.0)].get_mut(slot.var as usize) {
            *at = Some(value);
        }
    }

    /// A temporary read for the last time: its slot holds nothing after.
    fn consume(&mut self, operand: Operand) {
        let Operand::Value(value) = operand else {
            return;
        };
        let slot = self.val(value).slot;
        let current = self.slots[usize::from(slot.frame.0)].get(slot.var as usize);
        if slot.var >= self.meta(slot.frame).code.num_cvs && current == Some(&Some(value)) {
            self.kill(slot);
        }
    }

    fn kill(&mut self, slot: Slot) {
        if let Some(at) = self.slots[usize::from(slot.frame.0)].get_mut(slot.var as usize) {
            *at = None;
        }
    }

    /// A value op `idx` writes to `slot`: anything where a symbol table
    /// reaches the slot, as php types an aliased variable's every write;
    /// in a register where the op is inline and the value one scalar.
    fn define(
        &mut self,
        slot: Slot,
        (types, range): (Types, Option<Range>),
        idx: OpId,
        form: Form,
    ) -> ValueId {
        let aliased = self.aliased(slot);
        let (types, range) = if aliased {
            (Types::UNKNOWN, None)
        } else {
            (types, range)
        };
        let home = if form == Form::Inline && !aliased && types.scalar() {
            Home::Register
        } else {
            Home::Memory
        };
        let value = self.value(slot, types, Def::Op(idx), home);
        self.val_mut(value).range = range;
        self.set(slot, value);
        value
    }

    /// What `slot` holds now: what it entered with where nothing in the
    /// trace wrote it.
    fn current(&mut self, slot: Slot) -> ValueId {
        if let Some(Some(value)) = self.slots[usize::from(slot.frame.0)].get(slot.var as usize) {
            return *value;
        }
        let types = self.entry_types(slot);
        let value = self.value(slot, types, Def::Entry, Home::Memory);
        self.val_mut(value).range = self.entry_range(slot);
        self.set(slot, value);
        value
    }

    /// What `slot` holds where the frame's part of the trace begins,
    /// php's initial `var_info` of `zend_jit_trace_build_tssa`
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:1678-1772`): in a trace that
    /// enters a function, a parameter its declaration, which the entry has
    /// checked, and any other variable unset; in a frame the trace pushed,
    /// unset; elsewhere what php's analysis of the op array knows reaches
    /// the opline the frame's part begins at
    /// (`zend_jit_trace_copy_ssa_var_info`), and anything where it knows
    /// nothing. A parameter php's analysis knows is an `int` past a
    /// `VERIFY_RETURN_TYPE` that leaves it as it is, and one that coerces
    /// it, an `int` returned as a `string` the leave frees:
    ///
    /// ```php
    /// function s(int $n): string { if ($n < 0) { return "neg"; } return $n; }
    /// function f(int $n): int { if ($n < 2) { return $n; } $a = f($n - 1); return $a + 1; }
    /// function k($m) { $t = ""; $c = 0; for ($i = 0; $i < $m; $i++) { $t = s($i); $c += f(5); } return "$t $c"; }
    /// k(10); $u = memory_get_usage(); echo k(100), " ", memory_get_usage() - $u, "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 99 500 32
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function s(int $n): string { if ($n < 0) { return "neg"; } return $n; }
    /// function f(int $n): int { if ($n < 2) { return $n; } $a = f($n - 1); return $a + 1; }
    /// function k($m) { $t = ""; $c = 0; for ($i = 0; $i < $m; $i++) { $t = s($i); $c += f(5); } return "$t $c"; }
    /// k(10); $u = memory_get_usage(); echo k(100), " ", memory_get_usage() - $u, "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 99 500 32
    /// ```
    /// The range of the long `slot` holds where the frame's part of the
    /// trace begins, where [`Builder::entry_types`] copies what php's
    /// analysis knows reaches it, as `zend_jit_trace_copy_ssa_var_info`
    /// copies the range with the types.
    fn entry_range(&self, slot: Slot) -> Option<Range> {
        let meta = self.meta(slot.frame);
        let entered = (self.rec.entered.iter())
            .any(|entered| slot.frame == Frame::ROOT && entered.var == slot.var);
        let FrameInfo { func, place, .. } = self.frames[usize::from(slot.frame.0)];
        let cv = slot.var < meta.code.num_cvs;
        let copied = match place {
            _ if !cv => true,
            Place::Callee(_) => false,
            Place::Root if self.rec.start == Start::Enter => false,
            _ => !self.aliased(slot),
        };
        if entered || !copied {
            return None;
        }
        let at = meta.begins?;
        self.analysed.reaching(func, at.0, slot.var)?.range
    }

    fn entry_types(&self, slot: Slot) -> Types {
        let meta = self.meta(slot.frame);
        // A side trace enters with what its parent's exit knew of the
        // frame it starts in, as php's side reads its parent's stack map;
        // a slot a symbol table reaches is guarded at every read still.
        let known = (self.rec.entered.iter())
            .find(|entered| slot.frame == Frame::ROOT && entered.var == slot.var)
            .filter(|_| !self.aliased(slot));
        let FrameInfo { func, place, .. } = self.frames[usize::from(slot.frame.0)];
        let reaching = |unknown: Types| {
            (meta.begins)
                .and_then(|at| self.analysed.reaching(func, at.0, slot.var))
                .map_or(unknown, |info| unknown.restricted(info.types))
        };
        // Narrowed by what php's analysis knows reaches it, as php's side
        // copies the op array's info and meets it with its parent's type.
        if let Some(known) = known {
            return reaching(Types::of_tag(known.tag));
        }
        let cv = slot.var < meta.code.num_cvs;
        if !cv {
            return reaching(Types::UNKNOWN.without(Types::UNDEF));
        }
        if self.aliased(slot) {
            return Types::UNKNOWN;
        }
        let param = params(meta.code, meta.parameters)
            .into_iter()
            .find(|p| p.cv == slot.var);
        // A variable no opline makes a reference, and no declaration
        // passes by reference, holds none.
        let by_ref = param.is_some_and(|p| matches!(p.declared, Declared::Referenced(_)));
        let unknown = if by_ref || referenced(meta.code, slot.var) {
            Types::UNKNOWN
        } else {
            Types::UNKNOWN.without(Types::REF)
        };
        match place {
            Place::Callee(_) => Types::UNDEF,
            Place::Root if self.rec.start == Start::Enter => {
                param.map_or(Types::UNDEF, |p| param_types(p.declared, None))
            }
            _ => reaching(unknown),
        }
    }

    fn read(&mut self, frame: Frame, kind: OperandKind, field: u32) -> Result<Operand, Abort> {
        let code = self.meta(frame).code;
        Ok(match kind {
            OperandKind::Unused => Operand::Unused,
            OperandKind::Const => Operand::Constant(constant(code, field)),
            _ => {
                let slot = slot_of(frame, kind, field)
                    .ok_or_else(|| mismatch("an operand past its frame"))?;
                if slot.var as usize >= self.slots[usize::from(frame.0)].len() {
                    return Err(mismatch("an operand past its frame"));
                }
                Operand::Value(self.current(slot))
            }
        })
    }

    /// What an operand of an opline of `frame` may be.
    fn types_of(&self, frame: Frame, operand: Operand) -> Types {
        match operand {
            Operand::Unused => Types::NONE,
            Operand::Value(value) => self.val(value).types,
            Operand::Constant(constant) => constant_types(constant, self.meta(frame).code),
        }
    }

    /// Whether an operand is a temporary, which the op that reads it
    /// frees.
    fn is_temp(&self, operand: Operand) -> bool {
        match operand {
            Operand::Value(value) => {
                let slot = self.val(value).slot;
                slot.var >= self.meta(slot.frame).code.num_cvs
            }
            _ => false,
        }
    }

    // ------------------------------------------------------------ guards

    /// php's guards of one opline (`zend_jit_trace_build_tssa`,
    /// `Z/ext/opcache/jit/zend_jit_trace.c:1835-2245`): the operands its
    /// switch adds a guard for, then the packed guard of an array read or
    /// written by a long key. What the opline sees of each operand once
    /// its guards held, which for a value a symbol table reaches or a
    /// reference is the guarded type at this opline alone, as php's
    /// `CHECK_OP_TRACE_TYPE` narrows `op1_info` for the opline it guards.
    /// An operand no guard of its own narrowed is seen as its value
    /// stands once the others held, so `$i * $i` sees both operands as
    /// the one guard on its second narrowed them.
    ///
    /// What php's inference reads of an operand the recording saw
    /// through a reference is what its trace SSA holds of the variable, a
    /// reference to anything, whatever a guard held of it for this opline
    /// alone, as `zend_jit_fetch_reference` (`Z/ext/opcache/jit/zend_jit_ir.c:17021-17085`)
    /// and an `ASSIGN`'s `op1_info &= ~MAY_BE_REF` past
    /// `zend_jit_guard_reference` (`Z/ext/opcache/jit/zend_jit_trace.c:5155-5161`)
    /// narrow the code generator's `op1_info` and no SSA variable's. So what an
    /// `ASSIGN` writes through one is a reference to anything,
    /// `UPDATE_SSA_TYPE`'s widening, a variable assigned from it holds no
    /// type the trace knows, and the `ASSIGN` that reads that variable
    /// next holds it by an exit, php's exit 2, so the loop's direction is
    /// php's exit 3 (#3711):
    ///
    /// ```php
    /// function f() { $x = 0; $y = 0; $r = &$x; $c = 0; for ($i = 0; $i < 100; $i++) { $x = $i; $y = $x; $x = $y; if ($i > 50) { $c++; } } return $c + $x; }
    /// echo f(), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 148
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:3
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %s:2
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 3 f() %s:2
    /// ---- TRACE 2 start (side trace 1/3) f() %s:2
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    ///
    ///      TRACE 1 exit 0 f() %s:2
    /// ---- TRACE 3 start (side trace 1/0) f() %s:2
    /// ---- TRACE 3 stop (return)
    /// ---- TRACE 3 compiled
    /// ```
    #[allow(
        clippy::too_many_arguments,
        reason = "the one place the opline's parts meet"
    )]
    fn op_guards(
        &mut self,
        idx: OpId,
        frame: Frame,
        here: OplineIdx,
        op: &Opline,
        data_line: Option<&Opline>,
        operands: [Operand; 3],
        samples: [Option<Sample>; 3],
    ) -> Seen {
        let types = operands.map(|operand| self.types_of(frame, operand));
        let mut held: [Option<(Types, Read)>; 3] = [None; 3];
        let referenced = match operands[0] {
            Operand::Value(value) => self.held_references.contains(&value),
            _ => false,
        };
        let code = self.meta(frame).code;
        let sites = guard_sites(op, data_line, samples, (types, referenced), code);
        for (site, hold) in checked_order(op.opcode)
            .iter()
            .filter_map(|site| sites.iter().find(|(held, _)| held == site))
        {
            let at = *site as usize;
            // php follows an indirect `VAR` for the element writes alone.
            let sample = samples[at].filter(|s| !s.indirect || written_through(op.opcode));
            if let Some(guarded) = self.guard_use(idx, here, operands[at], (sample, *hold)) {
                held[at] = Some(guarded);
                self.hold_reference(operands[at], guarded.1);
            }
        }
        for site in check_sites(op, samples) {
            let at = site as usize;
            if sites.iter().all(|(held, _)| *held != site) {
                self.check_use(idx, here, operands[at], samples[at]);
            }
        }
        let mut seen = Seen {
            types: [Types::NONE; 3],
            read: [Read::InPlace; 3],
            inferred: [Types::NONE; 3],
        };
        for (at, operand) in operands.into_iter().enumerate() {
            let mut types = self.types_of(frame, operand);
            // A reference no guard held is read as php's trace SSA reads
            // it, `IS_UNKNOWN` (`Z/ext/opcache/jit/zend_jit_trace.c:1819-1833`):
            // a reference to anything, whatever the slot's types say. Its
            // inference reads it so where a guard held it too.
            let through = samples[at].is_some_and(|s| Read::sampled(s) == Read::Through);
            if through {
                types = types.or(Types::REF);
            }
            (seen.types[at], seen.read[at]) = held[at].unwrap_or((types, Read::InPlace));
            seen.inferred[at] = match operand {
                Operand::Value(value) if through || self.aliased(self.val(value).slot) => types,
                _ => seen.types[at],
            };
        }
        if let Some(layout) = packed_guard(op, samples, self.meta(frame).code)
            && let Operand::Value(value) = operands[0]
        {
            let types = self.val(value).types;
            let slot = self.val(value).slot;
            // A slot the recording saw holding a reference is held to no
            // layout in place, whatever its own types say: what it holds
            // is the reference, and an array's flags are read through it
            // or not at all.
            let in_place = samples[0].is_none_or(|s| Read::sampled(s) != Read::Through);
            if seen.read[0] == Read::Through {
                // An array read through a reference, held to its layout at
                // this read and narrowed for this opline alone, as its type
                // is (`zend_jit_fetch_reference`, `Z/ext/opcache/jit/zend_jit_ir.c:16952-17010`).
                if seen.types[0].either_layout() {
                    seen.types[0] = seen.types[0].laid_out(layout);
                    self.guards.push(Placed {
                        at: GuardAt::Before(idx),
                        check: Check::Layout {
                            value,
                            layout,
                            read: Read::Through,
                        },
                        exit: draft(Point::Before(idx), slot.frame, At::Opline(here)),
                    });
                }
            } else if in_place && types.either_layout() && !self.aliased(slot) {
                self.val_mut(value).types = types.laid_out(layout);
                seen.types[0] = types.laid_out(layout);
                seen.inferred[0] = types.laid_out(layout);
                let check = Check::Layout {
                    value,
                    layout,
                    read: Read::InPlace,
                };
                self.place(idx, here, value, check, None);
            } else if in_place
                && self.aliased(slot)
                && seen.types[0].either_layout()
                && matches!(
                    op.opcode,
                    Opcode::FetchDimR | Opcode::FetchDimIs | Opcode::FetchListR
                )
            {
                // An array a symbol table reaches, read by a long key, has
                // its layout held at the read by php's code generator, as
                // its type is, and narrowed for this opline alone.
                seen.types[0] = seen.types[0].laid_out(layout);
                self.guards.push(Placed {
                    at: GuardAt::Before(idx),
                    check: Check::Layout {
                        value,
                        layout,
                        read: Read::InPlace,
                    },
                    exit: draft(Point::Before(idx), slot.frame, At::Opline(here)),
                });
            }
        }
        seen
    }

    /// Marks `operand`'s value held a reference where the opline reads it
    /// through one a guard held, and its variable is one no symbol table
    /// reaches, php's `guarded_reference = 1` past `zend_jit_fetch_reference`
    /// and `zend_jit_guard_reference` (`Z/ext/opcache/jit/zend_jit_trace.c:4589-4607`,
    /// `:5155-5167`, `:5853-5860`, and the rest of their callers).
    fn hold_reference(&mut self, operand: Operand, read: Read) {
        if let Operand::Value(value) = operand
            && read == Read::Through
            && !self.is_temp(operand)
            && !self.aliased(self.val(value).slot)
        {
            self.held_references.insert(value);
        }
    }

    /// What an opline leaves marked of the values it wrote back to the
    /// variables it read ([`Builder::held_references`]): the value its
    /// variable `op1` read through a reference a guard held is written
    /// back marked, as php marks the `op1_def` of each fetch that defines
    /// one; and [`carried`] keeps or sets the mark of the others.
    fn carry_references(
        &mut self,
        (op, data_line, code): (&Opline, Option<&Opline>, &OpArray),
        operands: [Operand; 3],
        (samples, seen): ([Option<Sample>; 3], Seen),
        written: &Written,
    ) {
        let defs = [written.op1, written.op2, written.data];
        let mark = |builder: &mut Self, at: usize| {
            if let Some(def) = defs[at]
                && !builder.aliased(builder.val(def).slot)
            {
                builder.held_references.insert(def);
            }
        };
        if seen.read[0] == Read::Through && self.is_marked(operands[0]) {
            mark(self, 0);
        }
        match carried(op, data_line, samples, seen.types[0], code) {
            Some(Carried::Kept(site)) if self.is_marked(operands[site as usize]) => {
                mark(self, site as usize);
            }
            Some(Carried::Marked(site)) => mark(self, site as usize),
            Some(Carried::Kept(_)) | None => {}
        }
    }

    /// Whether `operand` is a value [`Builder::held_references`] marks.
    fn is_marked(&self, operand: Operand) -> bool {
        matches!(operand, Operand::Value(value) if self.held_references.contains(&value))
    }

    /// php's `ADD_OP_GUARD` on one operand, `zend_jit_trace_add_op_guard`:
    /// a type guard where the recorded type is narrower than the value's,
    /// narrowing the value unless a symbol table reaches it or a register
    /// holds it; a reference's inner type, where php's code generator
    /// fetches the reference ([`guard_sites`]); that a variable an
    /// `ASSIGN` writes is a reference still, where no guard before held it
    /// one ([`Hold::Reference`], [`Hold::Held`]); and the type of what
    /// an indirect `VAR` points at, at the element write that reads it,
    /// as php's `zend_jit_fetch_indirect_var` holds it (`Z/ext/opcache/jit/zend_jit_ir.c:17087-17141`),
    /// leaving at the opline. What the opline sees of the operand after
    /// it, where a guard is held, and how it reads it: in place, through
    /// a reference, which a write to the slot writes through, or through
    /// the `VAR`.
    ///
    /// A register holds what its op computed, a boolean either way where
    /// the recording saw `true` or `false`, so the guard is tested on the
    /// register before the opline, as php's `zend_jit_type_guard` tests
    /// the type of the value an op wrote (#3076): narrowed to the one
    /// seen, the register would hold a constant, the guard would test
    /// nothing, and the trace would go the recorded way past the
    /// iteration where the comparison turns.
    ///
    /// ```php
    /// function f() { $h = 0; for ($i = 0; $i < 1000000; $i++) { if ($i < 500000 && $i >= 0) { $h++; } } return $h; }
    /// echo f(), "\n";
    /// ```
    /// ```expect regime=jit-tracing process
    /// 500000
    /// ```
    ///
    /// The `VAR` a `function &f()` returns, recorded through the reference
    /// it is, is fetched where php's code generator fetches it, a
    /// `COUNT` of a packed array its handler's and a `FETCH_DIM_R` held to
    /// the reference and its array's layout read through it (#3706):
    ///
    /// ```php
    /// function &f() { static $a = [1, 2, 3]; return $a; } $s = 0; for ($i = 0; $i < 100; $i++) { $s += count(f()) + f()[1]; } echo $s, "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 500
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:2
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) $main() %s:2
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 2 $main() %s:2
    /// ---- TRACE 2 start (side trace 1/2) $main() %s:2
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    /// ```
    ///
    /// The same in a function, under a condition that turns, so the
    /// exits the inline `FETCH_DIM_R` takes, the reference's, the missing
    /// element's, the layout's and its element's, number the direction
    /// php's exit 9:
    ///
    /// ```php
    /// function &f(): array { static $a = [1, 2, 3]; return $a; }
    /// function g() { $s = 0; for ($i = 0; $i < 100; $i++) { $s += f()[1]; if ($i > 50) { $s++; } } return $s; }
    /// echo g(), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 249
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:4
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) g() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 9 g() %s:3
    /// ---- TRACE 2 start (side trace 1/9) g() %s:3
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    ///
    ///      TRACE 1 exit 2 g() %s:3
    /// ---- TRACE 3 start (side trace 1/2) g() %s:3
    /// ---- TRACE 3 stop (return)
    /// ---- TRACE 3 compiled
    /// ```
    ///
    /// And a `COUNT` of a hash the reference holds, held as php's
    /// `zend_jit_fetch_reference` holds it, by an exit before the
    /// direction's, php's exit 5:
    ///
    /// ```php
    /// function &f() { static $a = ['x' => 1, 'y' => 2]; return $a; }
    /// function g() { $s = 0; for ($i = 0; $i < 100; $i++) { $s += count(f()); if ($i > 50) { $s++; } } return $s; }
    /// echo g(), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 249
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:4
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) g() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 5 g() %s:3
    /// ---- TRACE 2 start (side trace 1/5) g() %s:3
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    ///
    ///      TRACE 1 exit 2 g() %s:3
    /// ---- TRACE 3 start (side trace 1/2) g() %s:3
    /// ---- TRACE 3 stop (return)
    /// ---- TRACE 3 compiled
    /// ```
    fn guard_use(
        &mut self,
        idx: OpId,
        here: OplineIdx,
        operand: Operand,
        (sample, hold): (Option<Sample>, Hold),
    ) -> Option<(Types, Read)> {
        let (Operand::Value(value), Some(sample)) = (operand, sample) else {
            return None;
        };
        let slot = self.val(value).slot;
        let tag = sample.kind;
        let before = Placed {
            at: GuardAt::Before(idx),
            check: Check::Type { value, tag },
            exit: draft(Point::Before(idx), slot.frame, At::Opline(here)),
        };
        let temporary = slot.var >= self.meta(slot.frame).code.num_cvs;
        // An indirect `VAR` is followed to what it points at, as php's
        // `zend_jit_fetch_indirect_var` follows it, and on to what that
        // refers to where it is a reference, as `zend_jit_fetch_reference`
        // follows it, and a reference a variable or a temporary holds to
        // what it refers to, the `VAR` a `function &f()` returns among them.
        let read = Read::sampled(sample);
        // php's `op1_info &= ~MAY_BE_REF` past `zend_jit_guard_reference`:
        // what the variable refers to, of what its inference knows.
        let referred = (self.val(value).types.without(Types::REF), Read::Through);
        let check = match (hold, read, temporary) {
            (Hold::Held, Read::Through, false) => return Some(referred),
            (Hold::Reference, Read::Through, false) => {
                self.guards.push(Placed {
                    check: Check::Reference { value },
                    ..before
                });
                return Some(referred);
            }
            (_, Read::InPlace, _) => None,
            (_, Read::Indirect(pointee), true) => Some(Check::Indirect {
                value,
                tag,
                pointee,
            }),
            (Hold::Fetched, Read::Through, _) => Some(Check::Deref { value, tag }),
            (_, Read::Indirect(_) | Read::Through, _) => return None,
        };
        if let Some(check) = check {
            self.guards.push(Placed { check, ..before });
            return Some((Types::of_tag(tag), read));
        }
        let current = *self.val(value);
        let types = current.types;
        if types.tag() == Some(tag) {
            return None;
        }
        if self.aliased(slot) || current.home == Home::Register {
            self.guards.push(before);
            return Some((types.narrowed(tag), Read::InPlace));
        }
        self.val_mut(value).types = types.narrowed(tag);
        self.place(idx, here, value, Check::Type { value, tag }, Some(tag));
        Some((types.narrowed(tag), Read::InPlace))
    }

    /// php's code generator's guard on an operand its inference does not
    /// guard at this opline (`CHECK_OP_TRACE_TYPE`, which holds an operand
    /// marked `MAY_BE_GUARD` anywhere in the trace): held before the opline
    /// where the recording saw a type, and settled once the walk is done
    /// ([`Builder::hold_checks`]). It narrows nothing the opline sees as
    /// the walk reads it: the oplines it is held at are stepped, or read
    /// the operand as the inference's guard held it.
    fn check_use(&mut self, idx: OpId, here: OplineIdx, operand: Operand, sample: Option<Sample>) {
        let (Operand::Value(value), Some(sample)) = (operand, sample) else {
            return;
        };
        let slot = self.val(value).slot;
        if sample.indirect || sample.reference {
            return;
        }
        self.checks.push(Placed {
            at: GuardAt::Before(idx),
            check: Check::Type {
                value,
                tag: sample.kind,
            },
            exit: draft(Point::Before(idx), slot.frame, At::Opline(here)),
        });
    }

    /// Where the guard on a value no symbol table reaches is held: once at
    /// entry for what the root's frame entered with, in a trace that loops
    /// by its own loop or its own recursive call, as php's code generator
    /// holds a loop-invariant type there only for those stops
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:4237-4272`), and before the op
    /// that reads it first in any other trace, a side's among them, as
    /// php's holds `MAY_BE_GUARD` and `MAY_BE_PACKED_GUARD` at the read,
    /// but at entry again in a trace that does not loop where php's
    /// register allocator loads the value as the trace enters
    /// ([`Builder::load_at_entry`], `:4283-4310`), decided once the walk
    /// has seen every read; a recursive return's loop holds only what the call wrote there, its
    /// phi ([`Builder::close_phis`]); by the inline op that
    /// computed it for an arithmetic's overflow and a fetched element's
    /// type, as php's code generator holds a guarded value it writes, an
    /// element twice, read in place and through the reference it is, as
    /// php's two reads of it each hold it, a `FETCH_DIM_IS`'s with the exit
    /// an element it does not find leaves by before them
    /// ([`Within::Fetched`]'s `missed`), and an arithmetic only where
    /// php's code generator serves it inline ([`Builder::over_aliased`]);
    /// before the op that reads it otherwise.
    fn place(
        &mut self,
        idx: OpId,
        here: OplineIdx,
        value: ValueId,
        check: Check,
        tag: Option<ZvalType>,
    ) {
        let held = *self.val(value);
        let computed = match (held.def, tag) {
            (Def::Op(def), Some(tag)) => match self.ops.get(def.index()) {
                Some(Op::Line(line)) if line.form == Form::Inline && line.imm != Imm::StrOffset => {
                    within(line.opcode, tag, line.op2)
                        .filter(|kind| {
                            matches!(kind, Within::Fetched { .. })
                                || !self.over_aliased(line.op1, line.op2)
                        })
                        .map(|kind| {
                            // A dimension's element is read through a reference
                            // only where php's analysis says the array may hold
                            // one, `zend_jit_guard_fetch_result_type`'s `deref`
                            // (`Z/ext/opcache/jit/zend_jit_ir.c:12678-12679`).
                            let deref = line.opcode == Opcode::FetchObjR
                                || (self.types_of(line.frame, line.op1).info()).has(Php::OF_REF);
                            (
                                def,
                                line.frame,
                                line.opline,
                                kind,
                                (line.op1, line.op2),
                                deref,
                                At::overflowing(kind, line, value),
                            )
                        })
                }
                _ => None,
            },
            _ => None,
        };
        // An increment or a decrement php's `zend_may_overflow` says cannot
        // overflow is compiled with no check: what it writes is a long the
        // guard holds nowhere ([`Builder::may_overflow`]).
        if let Some((def, _, _, Within::Overflowed { .. }, ..)) = computed
            && self.bounded(def)
        {
            if !self.aliased(held.slot) && self.val(value).types.scalar() {
                self.val_mut(value).home = Home::Register;
            }
            return;
        }
        let mut then = None;
        let (at, exit) = match (held.def, computed) {
            (Def::Entry, _)
                if held.slot.frame == Frame::ROOT
                    && matches!(self.repeat, Some(Repeat::Loop | Repeat::Call)) =>
            {
                (GuardAt::Entry, self.start())
            }
            (_, Some((def, frame, opline, kind, (op1, op2), deref, overflowing))) => {
                let fetched = |left| At::Fetched {
                    fetch: opline,
                    frees: Frees {
                        op1: self.is_temp(op1),
                        op2: self.is_temp(op2),
                    },
                    out: value,
                    left,
                };
                let to = match kind {
                    Within::Fetched { .. } => fetched(Left::Copied(Read::InPlace)),
                    Within::Rerun | Within::Overflowed { .. } | Within::Stayed => overflowing,
                };
                if let (Within::Fetched { missed }, Some(tag)) = (kind, tag) {
                    then = deref.then_some(Placed {
                        at: GuardAt::Within(def),
                        check: Check::Deref { value, tag },
                        exit: draft(
                            Point::Before(def),
                            frame,
                            fetched(Left::Copied(Read::Through)),
                        ),
                    });
                    if missed {
                        let missed = draft(Point::Before(def), frame, fetched(Left::Null));
                        self.leaves.push((def, Leaves::Missed, missed));
                    }
                }
                if !self.aliased(held.slot) && self.val(value).types.scalar() {
                    self.val_mut(value).home = Home::Register;
                }
                (GuardAt::Within(def), draft(Point::Before(def), frame, to))
            }
            _ => {
                if held.def == Def::Entry && held.slot.frame == Frame::ROOT && self.repeat.is_none()
                {
                    self.entry_reads.push(self.guards.len());
                }
                (
                    GuardAt::Before(idx),
                    draft(Point::Before(idx), held.slot.frame, At::Opline(here)),
                )
            }
        };
        self.guards.push(Placed { at, check, exit });
        self.guards.extend(then);
    }

    /// Whether an opline reads a value a symbol table reaches, which php's
    /// inference holds as possibly undefined, so its code generator serves
    /// the opline's arithmetic through the handler and the opline that
    /// reads what it wrote holds its type (`zend_jit_trace.c`, `ZEND_ADD`'s
    /// `MAY_BE_UNDEF` arm).
    fn over_aliased(&self, op1: Operand, op2: Operand) -> bool {
        [op1, op2].into_iter().any(|operand| {
            matches!(operand, Operand::Value(value) if self.aliased(self.val(value).slot))
        })
    }

    /// A side trace whose parent's exit left the frame where it stood
    /// holds, before anything else, that the frame stands at the opline
    /// it was recorded from, php's IP guard
    /// (`zend_jit_trace_opline_guard`, `Z/ext/opcache/jit/zend_jit_trace.c:4178-4180`):
    /// its exit names no opline and knows nothing of the frame, as php
    /// takes it before the side's stack holds a type, and grows a side
    /// of its own. A side that recorded no opline has nothing to hold the
    /// frame to, and is refused rather than entered wherever it stands.
    fn ip_guard(&mut self) -> Result<(), Abort> {
        let (func, opline) = (self.rec.events.iter())
            .find_map(|event| match event {
                Event::Op { func, opline, .. } => Some((*func, *opline)),
                _ => None,
            })
            .ok_or_else(|| mismatch("a side of an exit that names no opline records none"))?;
        self.guards.push(Placed {
            at: GuardAt::Entry,
            check: Check::Stands { func, opline },
            exit: Draft {
                at: At::Stands,
                ..self.start()
            },
        });
        Ok(())
    }

    /// An exit at the trace's entry: the root's first opline, nothing to
    /// store.
    fn start(&self) -> Draft {
        let first = self.rec.events.iter().find_map(|event| match event {
            Event::Op { opline, .. } => Some(*opline),
            _ => None,
        });
        Draft {
            point: Point::Start,
            frame: Frame::ROOT,
            at: first.map_or(At::Stands, At::Opline),
            grows: Grows::Side,
            link: None,
        }
    }

    // ------------------------------------------------------------ forms

    /// How an opline is compiled: inline where it is in the scalar core
    /// and its operands' types are the ones the inline code serves,
    /// through `step` otherwise.
    fn form(
        &self,
        frame: Frame,
        (op, data): (&Opline, Option<&Opline>),
        seen: Seen,
        (next, branch): (Option<&Event>, Option<bool>),
    ) -> Form {
        let [t1, t2, _] = seen.types;
        // An opline that writes its variable `op1` writes through what it
        // sees through a reference, which only its handler does.
        let cv1 = op.op1_type == OperandKind::Cv && seen.read[0] == Read::InPlace;
        let number = |t: Types| t == Types::LONG || t == Types::DOUBLE;
        let scalar = |t: Types| t.scalar();
        let code = self.meta(frame).code;
        let copied = |kind, field, t| copied_whole(kind, field, t, code);
        if let Some(form) = strung(op, seen) {
            return form;
        }
        let inline = match op.opcode {
            Opcode::Jmp => true,
            // A scalar's truth, as php's `zend_jit_bool_jmpznz` compiles
            // it, a double's with the warning of a NAN in a cold arm
            // (`Z/ext/opcache/jit/zend_jit_ir.c:7750-7768`), and a scalar
            // moved.
            Opcode::Jmpz | Opcode::Jmpnz | Opcode::JmpzEx | Opcode::JmpnzEx | Opcode::QmAssign => {
                scalar(t1)
            }
            Opcode::IsSmaller | Opcode::IsSmallerOrEqual | Opcode::IsEqual | Opcode::IsNotEqual => {
                number(t1) && number(t2)
            }
            Opcode::Add
            | Opcode::Sub
            | Opcode::Mul
            | Opcode::Div
            | Opcode::Mod
            | Opcode::Sl
            | Opcode::Sr
            | Opcode::BwAnd
            | Opcode::BwOr
            | Opcode::BwXor => arith(op.opcode).is_some_and(|served| served.serves(t1, t2)),
            Opcode::AssignOp => {
                cv1 && op.out_type == OperandKind::Unused
                    && lattice::binary_of(op.extended_value)
                        .and_then(arith)
                        .is_some_and(|served| served.serves(t1, t2))
            }
            // A value no scalar is moved or copied into the variable, as
            // php's `zend_jit_assign_to_variable` copies it
            // (`Z/ext/opcache/jit/zend_jit_ir.c:5793-5950`).
            Opcode::Assign => {
                cv1 && !t1.meets(Types::REF)
                    && (scalar(t2)
                        || (op.out_type == OperandKind::Unused && copied(op.op2_type, op.op2, t2)))
            }
            Opcode::PreInc | Opcode::PreDec | Opcode::PostInc | Opcode::PostDec => {
                cv1 && number(t1)
            }
            Opcode::FetchDimR
            | Opcode::FetchDimIs
            | Opcode::IssetIsemptyDimObj
            | Opcode::AssignDim
            | Opcode::AssignDimOp
            | Opcode::FetchDimW
            | Opcode::FetchDimRw => dim_inline((op, data), seen.types, seen.read[0], code),
            Opcode::FeResetR | Opcode::FeFetchR => iterated(op, t1, seen.read[0], branch),
            // php's `zend_jit_send_val` and `zend_jit_send_var`, a value
            // no scalar moved into the call's slot from a temporary, copied
            // with an owner more from a variable, or as it is from a
            // constant no reference counts (`:9466-9790`).
            Opcode::SendVal | Opcode::SendVar | Opcode::SendValEx => {
                op.op2_type == OperandKind::Unused
                    && (scalar(t1) || copied(op.op1_type, op.op1, t1))
            }
            Opcode::TypeCheck => type_checked(op, t1, seen.read[0]),
            // php's `zend_jit_count`, over an array in place or through
            // the reference a guard held (`:11671-11713`).
            Opcode::Count => t1.tag() == Some(ZvalType::Array),
            // php's `zend_jit_send_ref` of a variable (`:9792-9860`): the
            // reference it holds sent with an owner more, or made first.
            Opcode::SendRef => op.op1_type == OperandKind::Cv && op.op2_type == OperandKind::Unused,
            // php's `zend_jit_free` of a `FREE`: the temporary given up as
            // `jit_FREE_OP` gives it up (`:11543-11580`).
            Opcode::Free => matches!(op.op1_type, OperandKind::Tmp | OperandKind::Var),
            // A check of an argument the trace knows the callee takes by
            // value, php's `zend_jit_check_func_arg` (`:9925-9960`).
            Opcode::CheckFuncArg => {
                op.op2_type == OperandKind::Unused
                    && self
                        .calls
                        .last()
                        .is_some_and(|call| call.last_send == LastSend::ByValue)
            }
            Opcode::InitFcall | Opcode::InitFcallByName | Opcode::InitNsFcallByName => match next {
                Some(Event::InitCall {
                    callee: Callee::User(_),
                    began: Began::Here,
                }) => true,
                // A native's call is pushed in place or stepped: the
                // `push_call` kernel knows a user function's alone.
                Some(Event::InitCall {
                    callee: Callee::Native(_),
                    began: Began::Here,
                }) => self.in_place(frame, op, next).is_some(),
                _ => false,
            },
            Opcode::DoUcall | Opcode::DoFcall | Opcode::DoFcallByName => {
                matches!(next, Some(Event::Enter { .. }))
            }
            Opcode::Return => {
                let code = self.meta(frame).code;
                (matches!(next, Some(Event::Back)) || self.returns_at_end)
                    && (scalar(t1)
                        || op.op1_type == OperandKind::Unused
                        || returned(op, code, t1) != Returned::Scalar)
                    && (self.uncounted(frame) || self.leaves_in_place(frame))
            }
            Opcode::VerifyReturnType => {
                let declared = Types::of_mask(self.meta(frame).code.return_mask)
                    .without(Types::OBJECT)
                    .without(Types::REF);
                // Seen through a reference, the slot keeps the reference,
                // which the inline form would write over with its value.
                op.op1_type != OperandKind::Unused
                    && seen.read[0] == Read::InPlace
                    && t1.kinds() != Types::NONE
                    && t1.within(declared)
            }
            _ => false,
        };
        if inline { Form::Inline } else { Form::Step }
    }

    /// Where a property opline of `frame`, at `at` of `func`, is served in
    /// place, as php's code generator serves one whose
    /// `zend_get_known_property_info` finds the property on the immutable
    /// class the trace recorded (`Z/ext/opcache/jit/zend_jit_ir.c:14313`):
    /// over a constant name, on `$this` or a variable that holds an object
    /// as it is, of a class the recording took the name's slot on
    /// ([`UnitView::place`]). A compound write, an increment and a
    /// decrement of a property with a type are served by php's
    /// `zend_jit_assign_to_typed_prop` and its kin, and so by the
    /// handler here, as is a write of what its type does not hold as it
    /// stands, a `readonly` one and one of a set visibility among them;
    /// a write of a scalar its type holds is in place, as the handler's
    /// own inline path writes it; a write in place writes a scalar, or a value no scalar where
    /// nothing reads what it writes to its `out` slot, as an assignment
    /// does; a compound write in place adds, subtracts or multiplies a
    /// number; an increment and a decrement in place write no `out` slot.
    fn property_at(
        &self,
        frame: Frame,
        (func, at): (FuncRef, OplineIdx),
        (op, data): (&Opline, Option<&Opline>),
        (op1, sample1): (Operand, Option<Sample>),
        seen: Seen,
    ) -> Option<PropertyAt> {
        let code = self.meta(frame).code;
        if !on_declared_slot(op.opcode) || op.op2_type != OperandKind::Const {
            return None;
        }
        let class = sample1?.class?;
        let object = match (op.op1_type, op1) {
            (OperandKind::Unused, _) => Object::This(frame),
            (OperandKind::Cv, Operand::Value(value))
                if seen.types[0] == Types::OBJECT && seen.read[0] == Read::InPlace =>
            {
                Object::Value(value)
            }
            _ => return None,
        };
        let slot = self.view.place(func, at, class)?;
        let td = seen.types[2];
        let unused = op.out_type == OperandKind::Unused;
        let served = match op.opcode {
            Opcode::FetchObjR => op.out_type == OperandKind::Tmp,
            // The place itself, `IS_INDIRECT`, as `zend_jit_fetch_obj`
            // writes it for a `_W` fetch of a property with no type.
            Opcode::FetchObjW => op.out_type == OperandKind::Var,
            // A typed slot takes a scalar its type holds as it stands, as
            // the handler's inline path writes one (`data_value`).
            Opcode::AssignObj if slot.typed() => {
                td.scalar() && td.tag().is_some_and(|tag| slot.admits(tag))
            }
            Opcode::AssignObj => data.is_some_and(|data| {
                td.scalar() || (unused && copied_whole(data.op1_type, data.op1, td, code))
            }),
            Opcode::AssignObjOp => {
                unused
                    && matches!(
                        lattice::binary_of(op.extended_value),
                        Some(Opcode::Add | Opcode::Sub | Opcode::Mul)
                    )
                    && (td == Types::LONG || td == Types::DOUBLE)
            }
            _ => unused,
        };
        let typed_served = matches!(op.opcode, Opcode::FetchObjR | Opcode::AssignObj);
        (served && (typed_served || !slot.typed())).then_some(PropertyAt {
            object,
            class,
            slot,
        })
    }

    /// The call a function's `INIT_FCALL_BY_NAME` or
    /// `INIT_NS_FCALL_BY_NAME` pushes in place, as php's
    /// `zend_jit_init_fcall` pushes it (`Z/ext/opcache/jit/zend_jit_ir.c:8809-8937`):
    /// under the guard on the callee the opline's runtime-cache slot holds,
    /// the function the recording took the call of ([`HeldBy::Slot`]). The
    /// frame's own op array is php's recursive call, which [`Builder::in_place`]
    /// pushes with no guard. None for a native's and a request's own
    /// function's, which their `push_call` kernel or handler serves. A
    /// name in a namespace answers what the slot holds, the global function
    /// past the `eval` that declares its namespace's own, as php's does,
    /// and a name nothing declares yet is the handler's `Error`:
    /// ```php
    /// namespace N {
    ///     function loop($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += add($i, 1); if ($i == 20 && !function_exists('N\\add')) { eval('namespace N; function add($a, $b) { return -1; }'); } } return $s; }
    ///     function late($n) { $s = 0; for ($i = 0; $i < $n; $i++) { try { $s += later($i); } catch (\Error $e) { echo $e->getMessage(), "\n"; if ($i == 2) { eval('namespace N; function later($x) { return $x; }'); } } } return $s; }
    /// }
    /// namespace {
    ///     if (true) { function add($a, $b) { return $a + $b; } }
    ///     echo \N\loop(40), " ", \N\loop(40), " ", \N\late(40), "\n";
    /// }
    /// ```
    /// ```expect regime=jit-force process
    /// 820 820 Call to undefined function N\later()
    /// Call to undefined function N\later()
    /// Call to undefined function N\later()
    /// 777
    /// ```
    ///
    /// ```php
    /// namespace N {
    ///     function loop($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += add($i, 1); if ($i == 20 && !function_exists('N\\add')) { eval('namespace N; function add($a, $b) { return -1; }'); } } return $s; }
    ///     function late($n) { $s = 0; for ($i = 0; $i < $n; $i++) { try { $s += later($i); } catch (\Error $e) { echo $e->getMessage(), "\n"; if ($i == 2) { eval('namespace N; function later($x) { return $x; }'); } } } return $s; }
    /// }
    /// namespace {
    ///     if (true) { function add($a, $b) { return $a + $b; } }
    ///     echo \N\loop(40), " ", \N\loop(40), " ", \N\late(40), "\n";
    /// }
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 820 820 Call to undefined function N\later()
    /// Call to undefined function N\later()
    /// Call to undefined function N\later()
    /// 777
    /// ```
    fn named_at(&self, func: FuncRef, op: &Opline, next: Option<&Event>) -> Option<PushAt> {
        let Some(Event::InitCall {
            callee: Callee::User(callee),
            began: Began::Here,
        }) = next
        else {
            return None;
        };
        if *callee == func {
            return None;
        }
        let plain = (self.view.calls_of(*callee, op.extended_value))
            .find(|plain| plain.scope().is_none())?
            .clone();
        Some(PushAt {
            plain,
            held: HeldBy::Slot(self.view.named_slot(func, op)?),
        })
    }

    /// The call a method's `INIT_METHOD_CALL` pushes in place, as php's
    /// `zend_jit_init_method_call` pushes it (`Z/ext/opcache/jit/zend_jit_ir.c:8950-9213`):
    /// over a constant name, on `$this`, a variable or a temporary that holds an object
    /// as it is, whose class's call the recording took
    /// ([`UnitView::calls_of`]). On a published class, under the class
    /// guard on the object; on a request-scoped one (#3558), under the
    /// guard on the pair the opline's runtime-cache slot holds, its class
    /// the object's and its callee the method, as php compares
    /// `CACHED_PTR(opline->result.num)` with `obj->ce` for a class that is
    /// not immutable (`:9025-9056`), where the recording took the pair
    /// ([`UnitView::callee_at`]).
    fn method_at(
        &self,
        (frame, func, at): (Frame, FuncRef, OplineIdx),
        op: &Opline,
        (op1, sample1): (Operand, Option<Sample>),
        seen: Seen,
        next: Option<&Event>,
    ) -> Option<PushAt> {
        if op.op2_type != OperandKind::Const {
            return None;
        }
        let Some(Event::InitCall {
            callee,
            began: Began::Here,
        }) = next
        else {
            return None;
        };
        let class = sample1?.class?;
        // A variable's object or a temporary's, `(new D)->m()`, which the
        // call takes over as the handler does (`_TMPVAR_CONST`).
        let object = match (op.op1_type, op1) {
            (OperandKind::Unused, _) => Object::This(frame),
            (OperandKind::Cv | OperandKind::Tmp | OperandKind::Var, Operand::Value(value))
                if seen.types[0] == Types::OBJECT && seen.read[0] == Read::InPlace =>
            {
                Object::Value(value)
            }
            _ => return None,
        };
        let plain = match callee {
            Callee::User(callee) => (self.view.calls_of(*callee, op.extended_value))
                .find(|plain| plain.scope() == Some(class))?
                .clone(),
            Callee::Request => self.request_call(func, op, class)?,
            Callee::Native(_) => return None,
        };
        let held = if tier::class_published(class) {
            HeldBy::Class(object, class)
        } else {
            let pair = (self.view.callee_at(func, at))
                .filter(|pair| pair.class() == class && pair.instance())?;
            HeldBy::Pair(pair, PairClass::Receiver)
        };
        Some(PushAt { plain, held })
    }

    /// The call an `INIT_STATIC_METHOD_CALL` over a constant method name
    /// pushes in place, as php's `zend_jit_init_static_method_call`
    /// pushes it (`Z/ext/opcache/jit/zend_jit_ir.c:9215-9370`), over a
    /// constant class, or `self` or `parent`, as php's trace compiles it
    /// for those three alone (`Z/ext/opcache/jit/zend_jit_trace.c:6428-6442`):
    /// under the guard on the pair its runtime-cache slot holds, the
    /// callee the method the recording took the call of. A static callee
    /// over a constant class is held to the published class the opline
    /// names, php's constant `ce`, or to whichever class a request-scoped
    /// name resolved to, which php reads from the slot; over `self` or
    /// `parent` its called scope is the calling frame's ([`PairClass::Forwarded`]).
    /// A callee that takes `$this` takes the calling frame's, of the
    /// published class the recording saw it of ([`PairClass::This`]).
    fn static_at(
        &self,
        (func, at): (FuncRef, OplineIdx),
        op: &Opline,
        next: Option<&Event>,
    ) -> Option<PushAt> {
        // An `UNUSED` class operand whose pair the view took is `self` or
        // `parent` ([`UnitView::hold_callee`]).
        let constant = op.op1_type == OperandKind::Const;
        let scoped = op.op1_type == OperandKind::Unused;
        if !(constant || scoped) || op.op2_type != OperandKind::Const {
            return None;
        }
        let pair = self.view.callee_at(func, at)?;
        let callee = match next {
            Some(Event::InitCall {
                callee: Callee::User(callee),
                began: Began::Here,
            }) => callee,
            // A request-scoped class's own static method whose op array
            // is the request's, through the class's name.
            Some(Event::InitCall {
                callee: Callee::Request,
                began: Began::Here,
            }) if constant && !pair.instance() => {
                let plain = self.request_call(func, op, pair.class())?;
                return Some(PushAt {
                    plain,
                    held: HeldBy::Pair(pair, PairClass::Named),
                });
            }
            _ => return None,
        };
        let named = (constant && tier::class_published(pair.class())).then_some(pair.class());
        let mut plains = self.view.calls_of(*callee, op.extended_value);
        let (plain, class) = if pair.instance() {
            // The receiver's class is the call's called scope, the one
            // plain call to the callee whose scope is published.
            let plain = plains
                .find(|plain| plain.scope().is_some_and(tier::class_published))?
                .clone();
            let receiver = plain.scope()?;
            (plain, PairClass::This { named, receiver })
        } else if scoped {
            (
                plains.find(|plain| plain.scope().is_some())?.clone(),
                PairClass::Forwarded,
            )
        } else {
            let plain = plains
                .find(|plain| plain.scope() == Some(pair.class()))?
                .clone();
            (plain, named.map_or(PairClass::Named, PairClass::Published))
        };
        Some(PushAt {
            plain,
            held: HeldBy::Pair(pair, class),
        })
    }

    /// The plain call the recording took of a method of the request-scoped
    /// class `scope` whose op array is the request's own, an evaluated
    /// class's, which the record names by no number (#3570): found by the
    /// method's name, the constant the `INIT_*` at `op` of the op array
    /// `func` names, lowered, and its argument count.
    fn request_call(&self, func: FuncRef, op: &Opline, scope: ClassId) -> Option<PlainCall> {
        if tier::class_published(scope) {
            return None;
        }
        let code = self.view.op_array(func)?;
        let name = code
            .literals
            .get(op.op2 as usize)?
            .str_bytes()?
            .to_ascii_lowercase();
        (self.view.request_call(scope, &name, op.extended_value)).cloned()
    }

    /// The call a stepped `NEW`, `INIT_STATIC_METHOD_CALL` or
    /// `INIT_METHOD_CALL` pushes through its handler, where the recording
    /// took a plain call of a method to the callee it recorded: the
    /// handler looks the method up, and a guard after it holds the call it
    /// pushed to that callee ([`Check::Callee`]), as php's code holds
    /// `call->func` with `zend_jit_func_guard` past the lookup
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:9283-9300`), so the call's sends,
    /// its activation and its leave are compiled in place.
    fn handed_at(&self, op: &Opline, next: Option<&Event>) -> Option<PlainCall> {
        if !matches!(
            op.opcode,
            Opcode::New | Opcode::InitStaticMethodCall | Opcode::InitMethodCall
        ) {
            return None;
        }
        let Some(Event::InitCall {
            callee: Callee::User(callee),
            began: Began::Here,
        }) = next
        else {
            return None;
        };
        (self.view.calls_of(*callee, op.extended_value))
            .find(|plain| plain.scope().is_some())
            .cloned()
    }

    /// php's `zend_jit_class_guard` on the object a property opline at
    /// `here` reads, before it, where no guard held its class already.
    fn class_guard(
        &mut self,
        idx: OpId,
        frame: Frame,
        here: OplineIdx,
        (object, class): (Object, ClassId),
    ) {
        if self.classed.get(&object) == Some(&class) {
            return;
        }
        self.classed.insert(object, class);
        self.guards.push(Placed {
            at: GuardAt::Before(idx),
            check: Check::Class { object, class },
            exit: draft(Point::Before(idx), frame, At::Opline(here)),
        });
    }

    /// Whether a `RETURN` of `frame` leaves it in place: a function's
    /// frame, with no observer declared, whose variables the leave tests
    /// as it leaves where they may own something ([`Imm::Leave`]).
    fn leaves_in_place(&self, frame: Frame) -> bool {
        let func = self.frames[usize::from(frame.0)].func;
        self.view.is_main(func) == Some(false) && !self.view.observed()
    }

    /// Whether `frame` may have a receiver, which its leave gives up
    /// ([`UnitView::receives`]); a frame of an op array the view does not
    /// hold is taken to.
    fn receives(&self, frame: Frame) -> bool {
        let func = self.frames[usize::from(frame.0)].func;
        self.view.receives(func) != Some(false)
    }

    /// Whether every variable of `frame` holds what no reference counts,
    /// so a return frees nothing that runs a destructor.
    fn uncounted(&self, frame: Frame) -> bool {
        self.owners(frame) == Owners::Uncounted
    }

    /// The variables of `frame` a return gives up ([`Owners`]): each the
    /// trace does not hold uncounted where it reaches it, as php's trace
    /// frees each whose `STACK_TYPE` and inference may be counted
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:5600-5631`).
    fn owners(&self, frame: Frame) -> Owners {
        let cvs = self.meta(frame).code.num_cvs;
        let mut owners = Vars::default();
        for var in 0..cvs {
            let slot = Slot { frame, var };
            let types = match self.slots[usize::from(frame.0)].get(var as usize) {
                Some(Some(value)) => self.val(*value).types,
                _ => self.entry_types(slot),
            };
            if types.within(UNCOUNTED) {
                continue;
            }
            match owners.with(var) {
                Some(with) => owners = with,
                None => return Owners::Kernel,
            }
        }
        if owners.is_empty() {
            Owners::Uncounted
        } else {
            Owners::Each(owners)
        }
    }

    /// The slot of [`TraceIr::map_ptrs`] that holds `raw`, taken where
    /// none does.
    fn map_ptr_of(&mut self, raw: MapPtrRaw) -> usize {
        if let Some(at) = self.map_ptrs.iter().position(|held| *held == raw) {
            return at;
        }
        self.map_ptrs.push(raw);
        self.map_ptrs.len() - 1
    }

    // ------------------------------------------------------------ the end

    /// How the trace ends: back to its header for a loop, through an exit
    /// otherwise, at the opline the trace did not serve, where the record
    /// went on, or where the frame stands.
    fn end(&mut self, unexecuted: Option<&Event>) -> Result<(), Abort> {
        let frame = self.running()?;
        let at = match unexecuted {
            Some(Event::Op { func, opline, .. }) => {
                if self.frames[usize::from(frame.0)].func != *func {
                    return Err(mismatch(
                        "the trace ends in another op array than its frame's",
                    ));
                }
                At::Opline(*opline)
            }
            _ => match self.ops.last() {
                Some(Op::Line(_)) => self.successor.map_or(At::Stands, At::Opline),
                _ => At::Stands,
            },
        };
        let link = match self.rec.stop {
            Stop::Link(trace) => Some(trace),
            _ => None,
        };
        self.end_draft = Some(Draft {
            point: Point::End,
            frame,
            at,
            grows: Grows::Side,
            link,
        });
        let final_snapshot = self.snapshot();
        self.snapshots.push(final_snapshot);
        Ok(())
    }

    fn finish(mut self) -> Result<TraceIr, Abort> {
        let repeat = self.repeat;
        if repeat.is_some() {
            self.close_phis()?;
        }
        let closes = self.closes();
        self.load_at_entry();
        self.hold_in_fetch();
        self.hold_checks();
        let numbered = self.number(closes)?;
        self.mark_unguarded(&numbered.guards);
        let end = numbered
            .end
            .ok_or_else(|| mismatch("a trace with no end"))?;
        let body = self.body_entry(&numbered.guards);
        Ok(TraceIr {
            entry: Entry {
                start: self.rec.start,
                root: self.rec.root,
                stop: self.rec.stop,
            },
            frames: self.frames,
            values: self.values,
            ops: self.ops,
            guards: numbered.guards,
            exits: numbered.exits,
            unwinds: numbered.unwinds,
            map_ptrs: self.map_ptrs,
            end,
            body,
        })
    }

    /// Each value a guard held after an op narrowed, what it held before
    /// that guard ([`Unguarded`]), `guards` in the order they are held.
    fn mark_unguarded(&mut self, guards: &[Guard]) {
        for guard in guards {
            if let (GuardAt::Before(until), Some((value, _))) =
                (guard.at, Narrowed::of(guard.check))
            {
                let types = self.born[value.0 as usize];
                let held = self.val_mut(value);
                if held.types != types && held.unguarded.is_none() {
                    held.unguarded = Some(Unguarded { until, types });
                }
            }
        }
    }

    /// php's `CHECK_OP_TRACE_TYPE` over what the walk marked: a value a
    /// type guard holds anywhere in the trace is `MAY_BE_GUARD` at every
    /// opline that reads it, those before the guard's among them, since a
    /// guard narrows php's SSA variable and not a copy of it. So php holds
    /// the type where the code generator first reads the value, and an
    /// opline before that guard does not trust a type no guard yet read
    /// (`zend_jit_trace_add_op_guard` and `CHECK_OP_TRACE_TYPE`,
    /// `Z/ext/opcache/jit/zend_jit_trace.c:393-427`): a value no symbol
    /// table reaches has its one guard moved up to the first such opline,
    /// which clears its mark, so no guard before that opline or at it
    /// leaves one to take; one a symbol table reaches keeps its mark, and
    /// each such opline holds it again.
    ///
    /// A `TYPE_CHECK` that asks for another type than the recording saw
    /// over what an inline `FE_FETCH_R` wrote, the `ASSIGN` after it the
    /// inference's guard: fused to a `JMPZ`, a `JMPNZ`, unfused, of a mask
    /// of two types and of all but one, and an exact one beside them. The
    /// foreach that ran off its end comes back over an array one element
    /// longer whose last is another type, which reads wrong if the check
    /// trusts the element it was recorded on:
    ///
    /// ```php
    /// function jz($v, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $l = $v[$i % 2]; foreach ($l as $e) { if (is_int($e)) { $t += 1; } else { $t += 2; } $w = $e; } } return $t; }
    /// function jnz($v, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $l = $v[$i % 2]; foreach ($l as $e) { if (!is_int($e)) { $t += 2; } else { $t += 1; } $w = $e; } } return $t; }
    /// function out($v, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $l = $v[$i % 2]; foreach ($l as $e) { $b = is_int($e); $t += $b ? 1 : 2; $w = $e; } } return $t; }
    /// function both($v, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $l = $v[$i % 2]; foreach ($l as $e) { $t += is_bool($e) ? 1 : 2; $w = $e; } } return $t; }
    /// function not($v, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $l = $v[$i % 2]; foreach ($l as $e) { $t += $e !== null ? 1 : 2; $w = $e; } } return $t; }
    /// function same($v, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $l = $v[$i % 2]; foreach ($l as $e) { $t += is_float($e) ? 1 : 2; $w = $e; } } return $t; }
    /// $d = [[1.5, 2.5], [1.5, 2.5, 3]];
    /// echo jz($d, 6), " ", jnz($d, 6), " ", out($d, 6), " ", both($d, 6), " ", same($d, 6), " ";
    /// echo both([[1, 2], [1, 2, true]], 6), " ", not([[1, 2], [1, 2, null]], 6), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 27 27 27 30 18 27 18
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function jz($v, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $l = $v[$i % 2]; foreach ($l as $e) { if (is_int($e)) { $t += 1; } else { $t += 2; } $w = $e; } } return $t; }
    /// function jnz($v, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $l = $v[$i % 2]; foreach ($l as $e) { if (!is_int($e)) { $t += 2; } else { $t += 1; } $w = $e; } } return $t; }
    /// function out($v, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $l = $v[$i % 2]; foreach ($l as $e) { $b = is_int($e); $t += $b ? 1 : 2; $w = $e; } } return $t; }
    /// function both($v, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $l = $v[$i % 2]; foreach ($l as $e) { $t += is_bool($e) ? 1 : 2; $w = $e; } } return $t; }
    /// function not($v, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $l = $v[$i % 2]; foreach ($l as $e) { $t += $e !== null ? 1 : 2; $w = $e; } } return $t; }
    /// function same($v, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $l = $v[$i % 2]; foreach ($l as $e) { $t += is_float($e) ? 1 : 2; $w = $e; } } return $t; }
    /// $d = [[1.5, 2.5], [1.5, 2.5, 3]];
    /// echo jz($d, 6), " ", jnz($d, 6), " ", out($d, 6), " ", both($d, 6), " ", same($d, 6), " ";
    /// echo both([[1, 2], [1, 2, true]], 6), " ", not([[1, 2], [1, 2, null]], 6), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 27 27 27 30 18 27 18
    /// ```
    ///
    /// php's exits: the guard at the `TYPE_CHECK`, exit 4, knows nothing of
    /// the element, and its side grows from there:
    ///
    /// ```php
    /// function f($v, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $l = $v[$i % 2]; foreach ($l as $e) { $t += is_int($e) ? 1 : 2; $w = $e; } } return $t; }
    /// echo f([[1.5, 2.5], [1.5, 2.5, 3]], 300), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 1350
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:3
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %SCRATCH%/witness.php:2
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %SCRATCH%/witness.php:2
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 3 f() %SCRATCH%/witness.php:2
    /// ---- TRACE 2 start (side trace 1/3) f() %SCRATCH%/witness.php:2
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    ///
    ///      TRACE 1 exit 4 f() %SCRATCH%/witness.php:2
    /// ---- TRACE 3 start (side trace 1/4) f() %SCRATCH%/witness.php:2
    /// ---- TRACE 3 stop (link to 1)
    /// ---- TRACE 3 compiled
    ///
    ///      TRACE 2 exit 3 f() %SCRATCH%/witness.php:2
    /// ---- TRACE 4 start (side trace 2/3) f() %SCRATCH%/witness.php:2
    /// ---- TRACE 4 stop (return)
    /// ---- TRACE 4 compiled
    /// ```
    fn hold_checks(&mut self) {
        for check in std::mem::take(&mut self.checks) {
            let (GuardAt::Before(reader), Check::Type { value, .. }) = (check.at, check.check)
            else {
                continue;
            };
            let on_value = |placed: &Placed| matches!(placed.check, Check::Type { value: v, .. } if v == value);
            if self.aliased(self.val(value).slot) {
                if self.guards.iter().any(on_value) {
                    self.guards.push(check);
                }
                continue;
            }
            let first = (self.guards.iter().enumerate())
                .filter(|(_, placed)| on_value(placed))
                .min_by_key(|(_, placed)| match placed.at {
                    GuardAt::Before(op) | GuardAt::Within(op) => Some(op),
                    GuardAt::Entry => None,
                });
            if let Some((
                at,
                Placed {
                    at: GuardAt::Before(op),
                    ..
                },
            )) = first
                && *op > reader
            {
                self.guards.remove(at);
                self.guards.push(check);
            }
        }
    }

    /// Where a loop's trace is entered from its back edge past its
    /// header ([`BodyEntry`]): a trace rooted at the condition of a `for`
    /// or a `while`, its first op, whether it closes the loop there or
    /// leaves, a link to an inner loop's trace, say; whose condition's oplines,
    /// in the frame the trace entered in, are followed by the body's
    /// first, an opline before the header's, the jump's landing. The
    /// interpreter served the condition before its back edge landed
    /// there, so what the frame holds there is what the trace holds after
    /// it. None where a call is under way there, a guard held at entry is
    /// one the frame's type bytes and layouts do not hold (`guards`), or
    /// the trace is a side's.
    fn body_entry(&self, guards: &[Guard]) -> Option<BodyEntry> {
        let rooted_at_first = match self.repeat {
            None => true,
            Some(Repeat::Loop) => self.header.index() == 0,
            Some(Repeat::Call | Repeat::Return) => false,
        };
        if self.rec.start != Start::Loop || !rooted_at_first {
            return None;
        }
        let held = (guards.iter().filter(|guard| guard.at == GuardAt::Entry)).all(|guard| {
            matches!(
                guard.check,
                Check::Type { .. } | Check::Deref { .. } | Check::Layout { .. }
            )
        });
        let Some(Op::Line(header)) = self.ops.first() else {
            return None;
        };
        // The condition's oplines, past the header's and in its frame, up
        // to the first before it, the body's.
        let (at, body) = (self.ops.iter().enumerate().skip(1))
            .map_while(|(at, op)| match op {
                Op::Line(line) if line.frame == Frame::ROOT => Some((at, line)),
                _ => None,
            })
            .find(|(_, line)| line.opline.0 < header.opline.0)?;
        let snapshot = self.snapshots.get(at)?;
        let rooted = header.frame == Frame::ROOT
            && snapshot.calls == 0
            && (snapshot.values.iter()).all(|(slot, _)| slot.frame == Frame::ROOT);
        // Every value a register carries round the loop is one the entry
        // loads, so the body reads none the entry left undefined.
        let carried = (self.values.iter().enumerate())
            .filter(|(_, value)| {
                matches!(value.def, Def::Phi { .. }) && value.home == Home::Register
            })
            .all(|(phi, _)| (snapshot.values.iter()).any(|(_, held)| held.0 as usize == phi));
        let at = OpId(u32::try_from(at).ok()?);
        (held && rooted && carried).then(|| BodyEntry {
            at,
            opline: body.opline,
            holds: snapshot.values.clone(),
        })
    }

    /// The exits numbered in the order the trace meets them: an entered
    /// trace's `RECV` exits ([`Builder::reserve_recv_exits`]), the guards
    /// held at entry, then op by op the guards before it, those it holds
    /// itself and its own exit, and last a loop's interrupt test, which
    /// php takes where it closes the loop, or the exit a trace that does
    /// not loop ends through. Each guard and each opline that leaves
    /// names its exit, one an earlier place took where their specs match
    /// ([`Exits::take`]); a guard on a value no symbol table reaches makes
    /// its type known to the exits after it, as php's `SET_STACK_TYPE`
    /// after a guard does. The places an opline unwinds from are numbered
    /// apart, one per set of stores.
    fn number(&mut self, closes: Closes) -> Result<Numbered, Abort> {
        let (order, ended) = self.order(closes);
        let mut exits = Exits::default();
        let interrupt_reuses = self.reserve_recv_exits(&mut exits)?;
        let mut unwinds: Vec<Unwind> = Vec::new();
        let mut known: HashMap<ValueId, ZvalType> = HashMap::new();
        let mut numbered = Numbered {
            exits: Vec::new(),
            unwinds: Vec::new(),
            guards: Vec::with_capacity(self.guards.len()),
            end: ended,
        };
        for (_, source) in order {
            let draft = match source {
                Source::Guard(at) => self.guards[at].exit,
                Source::Leave(at) => {
                    let (_, _, draft) = self.leaves[at];
                    draft
                }
                Source::Interrupt(_) => self.interrupt(),
                Source::End => self.end_draft.unwrap_or_else(|| self.start()),
            };
            let mut spec = self.spec(&draft, &known, self.entry_bound(source));
            if let Source::Guard(at) = source {
                spec.layout = matches!(self.guards[at].check, Check::Layout { .. });
            }
            let func = (self.frames.get(usize::from(spec.frame.0)))
                .ok_or_else(|| mismatch("an exit in no frame of the trace"))?
                .func;
            let level = Level {
                func,
                held: self.held_at(&draft),
            };
            match source {
                Source::Guard(at) => {
                    let placed = self.guards[at];
                    numbered.guards.push(Guard {
                        at: placed.at,
                        check: placed.check,
                        exit: exits.take(spec, level.clone())?,
                    });
                    let held = stack_type(placed.check);
                    if let Some((value, tag)) = held
                        && !self.aliased(self.val(value).slot)
                    {
                        known.insert(value, tag);
                    }
                }
                Source::Leave(at) => {
                    let (op, leaves, _) = self.leaves[at];
                    let leave = match leaves {
                        Leaves::Unfinished | Leaves::Missed => {
                            Leave::Exit(exits.take(spec, level.clone())?)
                        }
                        Leaves::Thrown => {
                            let unwind = Unwind {
                                stores: spec.stores,
                            };
                            let id = unwinds.iter().position(|held| *held == unwind);
                            let id = id.unwrap_or_else(|| {
                                unwinds.push(unwind);
                                unwinds.len() - 1
                            });
                            Leave::Unwind(UnwindId(
                                u16::try_from(id).map_err(|_| mismatch("too many unwinds"))?,
                            ))
                        }
                    };
                    if let Some(Op::Line(line)) = self.ops.get_mut(op.index()) {
                        match leave {
                            Leave::Exit(id) => line.exit = Some(id),
                            Leave::Unwind(id) => line.unwind = Some(id),
                        }
                    }
                }
                Source::Interrupt(repeat) => {
                    let exit = match interrupt_reuses {
                        Some(first) if spec.stores.is_empty() && spec.landing == Landing::PLAIN => {
                            first
                        }
                        _ => exits.take(spec, level.clone())?,
                    };
                    numbered.end = Some(End::Loop(LoopHeader {
                        at: self.header,
                        repeat,
                        interrupt: Interrupt::Exit(exit),
                    }));
                }
                Source::End => {
                    let exit = exits.take(spec, level.clone())?;
                    numbered.end = Some(if self.returns_at_end {
                        End::Return(exit)
                    } else {
                        End::Leave(exit)
                    });
                }
            }
        }
        numbered.exits = exits.specs;
        numbered.unwinds = unwinds;
        Ok(numbered)
    }

    /// php's `RECV` exits, numbered ahead of every other exit of an
    /// entered trace (`jit-design.md` §4.2, #3269): one
    /// `ZEND_JIT_EXIT_TO_VM` exit per `RECV` [`recv_exits`] names in the
    /// root's prologue. The prologue ran before `Site::Entry`, so no code
    /// tests them, and each lands where the interpreter goes on past it,
    /// at the entry's opline, the only place this engine's dispatch
    /// serves the root frame from. The first is the one a loop's
    /// interrupt test leaves through where it is the `RECV` at opline
    /// zero, php's trace's first, as php's `zend_jit_trace_get_exit_point`
    /// reuses it for the test it takes at the trace's first opline
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:7223`, the reuse at `:168-185`)
    /// where the test stores no register and lands as plainly, so that the
    /// exit does what the test's own would. What the test knows of the
    /// frame's types is not held to the `RECV`'s, as php's stack map
    /// holds it, since this tier's `known` is not php's stack types there
    /// and neither exit grows a side trace that would enter with it; the
    /// pinned php reuses it on fib, `f(int $a, $b)` and
    /// `f(int $a, $b, $c, int $d, $e = 1)` alike.
    fn reserve_recv_exits(&self, exits: &mut Exits) -> Result<Option<ExitId>, Abort> {
        if self.rec.start != Start::Enter {
            return Ok(None);
        }
        let meta = self.meta(Frame::ROOT);
        let recvs = recv_exits(meta.code, meta.parameters);
        let Some(&first) = recvs.first() else {
            return Ok(None);
        };
        let entry = (meta.code.safepoints.first().copied())
            .ok_or_else(|| mismatch("an entered op array with no entry safepoint"))?;
        let reserved = ExitSpec {
            frame: Frame::ROOT,
            opline: Some(entry),
            landing: Landing::PLAIN,
            stores: Vec::new(),
            known: Vec::new(),
            grows: Grows::Never,
            link: None,
            layout: false,
        };
        let ids = (recvs.iter())
            .map(|_| exits.push(reserved.clone()))
            .collect::<Result<Vec<ExitId>, Abort>>()?;
        Ok(ids.first().copied().filter(|_| first == OplineIdx(0)))
    }

    /// What the slots hold at `point`; none at the trace's start, where
    /// everything is in memory.
    fn snapshot_at(&self, point: Point) -> Option<&Snapshot> {
        match point {
            Point::Start => None,
            Point::Before(at) => self.snapshots.get(at.index()),
            Point::After(at) => self.snapshots.get(at.index() + 1),
            Point::End => self.snapshots.last(),
        }
    }

    /// The untyped parameters of `draft`'s frame that hold, where it
    /// leaves, the value they entered with, each with the caller's
    /// register value it was sent from: what php's stack map holds as a
    /// register reference there, so the exit is not one its caller's
    /// level shares, where php's typed `RECV` reads the parameter from
    /// its slot and its stack map holds none.
    fn held_at(&self, draft: &Draft) -> Vec<(u32, ValueId)> {
        let empty = Snapshot::default();
        let snapshot = self.snapshot_at(draft.point).unwrap_or(&empty);
        (snapshot.values.iter())
            .filter(|(slot, _)| slot.frame == draft.frame)
            .filter_map(|(slot, value)| Some((slot.var, *self.held.get(value)?)))
            .collect()
    }

    /// Every place that takes an exit, in the order the trace meets them,
    /// and the end of a loop whose header numbers no exit.
    fn order(&self, closes: Closes) -> (Vec<(Order, Source)>, Option<End>) {
        let mut order: Vec<(Order, Source)> = Vec::new();
        for (at, guard) in self.guards.iter().enumerate() {
            let key = match (guard.at, guard.check) {
                (GuardAt::Entry, check) => Order::Entry(self.guarded_slot(check)),
                (GuardAt::Before(op), Check::Layout { .. }) => Order::At(op, Stage::Inside),
                (GuardAt::Before(op), Check::Room { .. }) => Order::At(op, Stage::Pushes),
                (GuardAt::Before(op), _) => Order::At(op, Stage::Before),
                (GuardAt::Within(op), Check::Direction { .. }) => Order::At(op, Stage::Branches),
                (GuardAt::Within(op), _) => Order::At(op, Stage::Within),
            };
            order.push((key, Source::Guard(at)));
        }
        for (at, (op, leaves, _)) in self.leaves.iter().enumerate() {
            let opens = match leaves {
                Leaves::Missed => true,
                Leaves::Unfinished => matches!(
                    self.ops.get(op.index()),
                    Some(Op::Line(line))
                        if matches!(line.opcode, Opcode::FetchDimR | Opcode::FetchObjR)
                ),
                Leaves::Thrown => false,
            };
            let stage = if opens { Stage::Opens } else { Stage::Leaves };
            order.push((Order::At(*op, stage), Source::Leave(at)));
        }
        let mut ended = None;
        match closes {
            Closes::ThroughExit(repeat) => order.push((Order::End, Source::Interrupt(repeat))),
            Closes::NoExit(header) => ended = Some(End::Loop(header)),
            Closes::Leave => order.push((Order::End, Source::End)),
        }
        // Stable: within one place, in the order they were placed.
        order.sort_by_key(|(key, _)| *key);
        (order, ended)
    }

    /// How far into the variables an exit of `source` knows their entry.
    /// An entry guard is placed as php's code generator walks the
    /// variables in order, setting each one's stack type before its
    /// packed guard and after its type guard
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:4228-4288`): its exit knows a
    /// concrete entry of the variables before it alone, and one on no
    /// variable, the stack's room tested before the walk, knows none.
    /// A side trace grown from such an exit enters knowing what it knew,
    /// and guards none of it again: the row's layout guard hands its
    /// side the types of `$n`, `$out`, `$i` and `$sum`, so the side's
    /// exits are php's two, the increment's overflow and the loop's end:
    ///
    /// ```php
    /// function a(int $i, int $j): float { $ij = $i + $j; return 1.0 / ($ij * ($ij + 1) / 2 + $i + 1); }
    /// function f(int $n, array $u): array {
    ///     $out = [];
    ///     for ($i = 0; $i < $n; $i++) { $sum = 0.0; for ($j = 0; $j < $n; $j++) { $sum += a($i, $j) * $u[$j]; } $out[$i] = $sum; }
    ///     return $out;
    /// }
    /// $u = array_fill(0, 60, 1.0);
    /// $u = f(60, f(60, $u));
    /// printf("%.6f\n", $u[0]);
    /// ```
    /// ```expect regime=jit-inline process capture
    /// 3.230547
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:8
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %SCRATCH%/witness.php:5
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %SCRATCH%/witness.php:5
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 4 f() %SCRATCH%/witness.php:5
    /// ---- TRACE 2 start (side trace 1/4) f() %SCRATCH%/witness.php:5
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    ///
    ///      TRACE 2 exit 0 f() %SCRATCH%/witness.php:5
    /// ---- TRACE 3 start (side trace 2/0) f() %SCRATCH%/witness.php:5
    /// ---- TRACE 3 stop (link to 1)
    /// ---- TRACE 3 compiled
    ///
    ///      TRACE 3 exit 1 f() %SCRATCH%/witness.php:6
    /// ---- TRACE 4 start (side trace 3/1) f() %SCRATCH%/witness.php:6
    /// ---- TRACE 4 stop (return)
    /// ---- TRACE 4 compiled
    ///
    /// ---- TRACE 5 start (enter) f() %SCRATCH%/witness.php:3
    /// ---- TRACE 5 abort (inner loop)
    /// ---- TRACE 5 start (loop) f() %SCRATCH%/witness.php:5
    /// ---- TRACE 5 abort (compiled loop)
    /// ```
    fn entry_bound(&self, source: Source) -> Option<u32> {
        let Source::Guard(at) = source else {
            return None;
        };
        let placed = self.guards[at];
        (placed.at == GuardAt::Entry).then(|| {
            (self.guarded_slot(placed.check)).map_or(0, |slot| match placed.check {
                Check::Layout { .. } => slot.var.saturating_add(1),
                _ => slot.var,
            })
        })
    }

    /// The slot a guard holds a value of; none for a guard on no value.
    fn guarded_slot(&self, check: Check) -> Option<Slot> {
        match check {
            Check::Type { value, .. }
            | Check::Deref { value, .. }
            | Check::Reference { value }
            | Check::Indirect { value, .. }
            | Check::Layout { value, .. }
            | Check::Class {
                object: Object::Value(value),
                ..
            } => Some(self.val(value).slot),
            Check::Class {
                object: Object::This(_),
                ..
            }
            | Check::Direction { .. }
            | Check::Stands { .. }
            | Check::Callee { .. }
            | Check::Arm { .. }
            | Check::Room { .. } => None,
        }
    }

    /// How the trace closes, as php's `zend_jit_trace` chooses a loop's
    /// `timeout_exit_addr` (`Z/ext/opcache/jit/zend_jit_trace.c:7214-7231`):
    /// a loop by recursive return takes no interrupt test
    /// ([`Interrupt::None`]); a loop's test takes php's interrupt stub and
    /// no exit in a root trace whose code keeps the instruction pointer it
    /// entered with, php's `ZEND_JIT_TRACE_USES_INITIAL_IP`, and holds
    /// nothing in registers where the test leaves, so no deoptimisation
    /// is needed (`zend_jit_trace_stack_needs_deoptimization`); any other
    /// loop's test takes an exit at its header.
    fn closes(&self) -> Closes {
        let Some(repeat) = self.repeat else {
            return Closes::Leave;
        };
        let closed = |interrupt| {
            Closes::NoExit(LoopHeader {
                at: self.header,
                repeat,
                interrupt,
            })
        };
        let stubbed = || {
            self.rec.start.grew().is_none()
                && self.uses_initial_ip()
                && (self.spec(&self.interrupt(), &HashMap::new(), None).stores).is_empty()
        };
        match repeat {
            Repeat::Return => closed(Interrupt::None),
            Repeat::Loop | Repeat::Call => match self.at_header_opline().filter(|_| stubbed()) {
                Some(at) => closed(Interrupt::Stub(at)),
                None => Closes::ThroughExit(repeat),
            },
        }
    }

    /// php's `zend_jit_trace_uses_initial_ip` over the trace's ops as
    /// php's code generator walks them (`Z/ext/opcache/jit/zend_jit_ir.c:991-1109`):
    /// the code enters holding the trace's first opline in its
    /// instruction pointer and tracks it; a call it begins starts reusing
    /// the pointer for the call (`zend_jit_start_reuse_ip`) and stops
    /// tracking; an opline served through its handler
    /// ([`Builder::by_handler`]) sets the pointer (`zend_jit_set_ip`),
    /// using the one it tracks where it tracks one, and tracks the opline
    /// after it from then on; and an opline compiled inline at the opline
    /// tracked uses the pointer where its code may call into php's runtime,
    /// standing the frame there first (`jit_SET_EX_OPLINE`, `:1100-1109`): php's
    /// `BOOL_NOT` of a variable recorded through a reference calls
    /// `zend_is_true` (`:7790-7792`), so a loop whose first opline it is
    /// takes php's interrupt stub.
    fn uses_initial_ip(&self) -> bool {
        let first = self.ops.iter().find_map(|op| match op {
            Op::Line(line) => Some(self.place_of(line)),
            _ => None,
        });
        let mut tracked = first;
        for (at, op) in self.ops.iter().enumerate() {
            match op {
                Op::Line(line) if self.by_handler(at, line) => {
                    if tracked.is_some() {
                        return true;
                    }
                    tracked = Some(self.after_handler(line));
                }
                Op::Line(line) => {
                    if tracked == Some(self.place_of(line)) && self.calls_runtime(at, line) {
                        return true;
                    }
                }
                Op::InitCall {
                    began: Began::Here, ..
                } => tracked = None,
                Op::InitCall {
                    began: Began::Before,
                    ..
                }
                | Op::Enter { .. }
                | Op::Back { .. } => {}
            }
        }
        false
    }

    /// The opline `line` is, as php's pointer names it: by its op array,
    /// whatever frame of the trace serves it.
    fn place_of(&self, line: &Line) -> (FuncRef, OplineIdx) {
        (self.frames[usize::from(line.frame.0)].func, line.opline)
    }

    /// The opline php's handler leaves the pointer at after `line`: the
    /// next, past an `OP_DATA` that goes with it (`zend_jit_handler`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:4139-4153`).
    fn after_handler(&self, line: &Line) -> (FuncRef, OplineIdx) {
        let (func, at) = self.place_of(line);
        let code = self.meta(line.frame).code;
        let data = code
            .oplines
            .get(at.0 as usize + 1)
            .is_some_and(|after| after.opcode == Opcode::OpData);
        (func, OplineIdx(at.0 + if data { 2 } else { 1 }))
    }

    /// Whether php's inline code for `line`, the op at `at`, may call into
    /// php's runtime, standing the frame at it first: where an operand was recorded
    /// through a reference, or may be anything but a scalar once php's
    /// `CHECK_OP_TRACE_TYPE` narrowed it to what was recorded.
    fn calls_runtime(&self, at: usize, line: &Line) -> bool {
        let samples = OpId(u32::try_from(at).unwrap_or(u32::MAX));
        let samples = self.sampled.get(&samples).copied().unwrap_or([None; 3]);
        [line.op1, line.op2]
            .into_iter()
            .zip(samples)
            .any(|(operand, sample)| match (operand, sample) {
                (Operand::Value(_), Some(sample)) if sample.reference => true,
                (Operand::Value(_), Some(sample)) if !sample.indirect => {
                    !Types::of_tag(sample.kind).scalar()
                }
                (Operand::Value(value), _) => !self.val(value).types.scalar(),
                (Operand::Unused | Operand::Constant(_), _) => false,
            })
    }

    /// Whether php's code generator serves `line`, the op at `at`, through
    /// its handler, `zend_jit_trace_handler`, which sets the instruction
    /// pointer, whatever this tier serves inline or steps: an opline its
    /// switch has no case for, or one whose case breaks to the handler
    /// over what its operands may be (`Z/ext/opcache/jit/zend_jit_trace.c:4436-6477`).
    /// A case it compiles inline sets no pointer and reads none: `BOOL_NOT`
    /// is `zend_jit_bool_jmpznz` (`:5644-5654`), which this tier steps.
    /// What an operand may be is what the recording saw of it where it saw
    /// it, as php's `CHECK_OP_TRACE_TYPE` narrows it, and what `types`
    /// holds of it otherwise; but the `ADD`, `SUB` and `MUL` case reads it
    /// before it narrows, so a variable a symbol table reaches, which its
    /// inference holds as possibly undefined, is served by the handler
    /// there (`:4538-4552`).
    fn by_handler(&self, at: usize, line: &Line) -> bool {
        let code = self.meta(line.frame).code;
        let Some(op) = code.oplines.get(line.opline.0 as usize) else {
            return true;
        };
        let samples = OpId(u32::try_from(at).unwrap_or(u32::MAX));
        let samples = self.sampled.get(&samples).copied().unwrap_or([None; 3]);
        let seen = |n: usize, operand: Operand| match (operand, samples[n]) {
            (Operand::Value(_), Some(sample)) if !sample.indirect => Types::of_tag(sample.kind),
            (operand, _) => self.types_of(line.frame, operand),
        };
        let (t1, t2) = (seen(0, line.op1), seen(1, line.op2));
        let cv1 = op.op1_type == OperandKind::Cv;
        let long = |t: Types| t.meets(Types::LONG);
        let number = |t: Types| t.meets(Types::LONG) || t.meets(Types::DOUBLE);
        let string = |t: Types| t.meets(Types::STRING);
        match op.opcode {
            Opcode::PreInc | Opcode::PreDec | Opcode::PostInc | Opcode::PostDec => {
                !cv1 || !long(t1)
            }
            Opcode::BwOr
            | Opcode::BwAnd
            | Opcode::BwXor
            | Opcode::Sl
            | Opcode::Sr
            | Opcode::Mod => !long(t1) || !long(t2),
            Opcode::Add | Opcode::Sub | Opcode::Mul => {
                let arrays = op.opcode == Opcode::Add
                    && t1.kinds() == Types::ARRAY.kinds()
                    && t2.kinds() == Types::ARRAY.kinds();
                self.over_aliased(line.op1, line.op2) || (!arrays && (!number(t1) || !number(t2)))
            }
            Opcode::Concat | Opcode::FastConcat => {
                t1.meets(Types::UNDEF) || t2.meets(Types::UNDEF) || !string(t1) || !string(t2)
            }
            Opcode::AssignOp => {
                let served = match lattice::binary_of(op.extended_value) {
                    Some(Opcode::Add | Opcode::Sub | Opcode::Mul) => number(t1) && number(t2),
                    Some(
                        Opcode::BwOr
                        | Opcode::BwAnd
                        | Opcode::BwXor
                        | Opcode::Sl
                        | Opcode::Sr
                        | Opcode::Mod,
                    ) => long(t1) && long(t2),
                    Some(Opcode::Concat) => string(t1) && string(t2),
                    _ => false,
                };
                !cv1 || op.out_type != OperandKind::Unused || !served
            }
            Opcode::Assign => !cv1,
            Opcode::Cast => t1
                .tag()
                .is_none_or(|tag| op.extended_value != u32::from(tag as u8)),
            Opcode::TypeCheck => op.extended_value == MAY_BE_RESOURCE,
            Opcode::IssetIsemptyCv => op.extended_value & ISEMPTY != 0,
            Opcode::InArray => {
                matches!(op.op1_type, OperandKind::Var | OperandKind::Tmp)
                    || t1.kinds() != Types::STRING
            }
            Opcode::RopeInit | Opcode::RopeAdd | Opcode::RopeEnd => t2.kinds() != Types::STRING,
            opcode => !compiled_inline(opcode),
        }
    }

    /// A loop's interrupt test: at its header, storing what the header's
    /// phis hold in registers.
    fn interrupt(&self) -> Draft {
        let frame = match self.ops.get(self.header.index()) {
            Some(Op::Line(line)) => line.frame,
            _ => Frame::ROOT,
        };
        Draft {
            grows: Grows::Never,
            ..draft(Point::Before(self.header), frame, self.at_header())
        }
    }

    /// The opline of a loop's header, where it is one.
    fn at_header_opline(&self) -> Option<OplineIdx> {
        match self.ops.get(self.header.index()) {
            Some(Op::Line(line)) => Some(line.opline),
            _ => None,
        }
    }

    /// The opline of a loop's header.
    fn at_header(&self) -> At {
        match self.ops.get(self.header.index()) {
            Some(Op::Line(line)) => At::Opline(line.opline),
            _ => At::Stands,
        }
    }

    /// Each phi's back edge, then php's guard propagation through phi
    /// sources (`zend_jit_trace.c:2680-2770`): a phi whose back edge
    /// already has the type its guard holds is guarded once, on what the
    /// loop entered with.
    fn close_phis(&mut self) -> Result<(), Abort> {
        let last = self.running()?;
        for at in 0..self.phis.len() {
            let phi = self.phis[at];
            let Value { slot, def, .. } = *self.val(phi);
            let Def::Phi { entry, .. } = def else {
                continue;
            };
            // A recursion's next turn is the frame its last call or
            // return reached.
            let back_slot = match self.repeat {
                Some(Repeat::Call | Repeat::Return) => Slot {
                    frame: last,
                    var: slot.var,
                },
                Some(Repeat::Loop) | None => slot,
            };
            let back = self.slots[usize::from(back_slot.frame.0)]
                .get(back_slot.var as usize)
                .copied()
                .flatten()
                .unwrap_or(phi);
            self.val_mut(phi).def = Def::Phi { entry, back };
        }
        for at in 0..self.guards.len() {
            let Placed {
                at: GuardAt::Before(reader),
                check,
                ..
            } = self.guards[at]
            else {
                continue;
            };
            let Some((value, narrowed)) = Narrowed::of(check) else {
                continue;
            };
            let phi = *self.val(value);
            let Def::Phi { entry, back } = phi.def else {
                continue;
            };
            let entered = *self.val(entry);
            if entered.def != Def::Entry
                || entered.slot.frame != Frame::ROOT
                || self.aliased(phi.slot)
                || self.renumbered(phi.slot, reader)
            {
                continue;
            }
            let brought = self.val(back).types;
            let carried = match narrowed {
                Narrowed::Tag(_) => {
                    brought.kinds().within(phi.types.kinds()) && !brought.meets(Types::REF)
                }
                Narrowed::Layout(_) => brought.within(phi.types),
            };
            // php's third arm: a back edge whose value an inline
            // arithmetic made, of another type than the phi's guard holds,
            // is held to it within that arithmetic, `is_checked_guard`.
            let checked = match narrowed {
                Narrowed::Tag(ZvalType::Long) if !carried => self.checked_guard(back),
                _ => None,
            };
            if !carried && checked.is_none() {
                continue;
            }
            if let Some(held) = checked {
                let types = self.val(back).types.narrowed(ZvalType::Long);
                let slot = self.val(back).slot;
                self.val_mut(back).types = types;
                if !self.aliased(slot) {
                    self.val_mut(back).home = Home::Register;
                }
                if let Checked::Within(placed) = held {
                    self.guards.push(placed);
                }
            }
            self.val_mut(entry).types = match narrowed {
                Narrowed::Tag(tag) => entered.types.narrowed(tag),
                Narrowed::Layout(layout) => entered.types.laid_out(layout),
            };
            // Held on what the loop entered with, the phi is of its type
            // from the header on, as php's phi loses `MAY_BE_GUARD`.
            self.born[value.0 as usize] = phi.types;
            self.guards[at] = Placed {
                at: GuardAt::Entry,
                check: match check {
                    Check::Type { tag, .. } => Check::Type { value: entry, tag },
                    Check::Layout { layout, read, .. } => Check::Layout {
                        value: entry,
                        layout,
                        read,
                    },
                    other => other,
                },
                exit: self.start(),
            };
        }
        self.home_phis();
        Ok(())
    }

    /// Each phi in a register where it is one scalar no symbol table
    /// reaches and its back edge brings a register's value; a phi a type
    /// guard holds within the loop's body stays in its slot, where the
    /// guard reads its tag, since a register holds none.
    fn home_phis(&mut self) {
        for at in 0..self.phis.len() {
            let phi = self.phis[at];
            let Value {
                slot, types, def, ..
            } = *self.val(phi);
            let Def::Phi { back, .. } = def else { continue };
            let held_within = self.guards.iter().any(|placed| {
                matches!(placed, Placed {
                    at: GuardAt::Before(_),
                    check: Check::Type { value, .. },
                    ..
                } if *value == phi)
            });
            if !self.aliased(slot)
                && types.scalar()
                && !held_within
                && self.val(back).home == Home::Register
            {
                self.val_mut(phi).home = Home::Register;
            }
        }
    }

    /// Whether php's trace SSA reads another number than the loop phi of
    /// `slot` at `reader`: a line between the header and `reader` writes
    /// the slot anew ([`Builder::writes_anew`]), as a line that reads the
    /// variable and may take a counted owner of it does under
    /// `ZEND_SSA_RC_INFERENCE` (`Z/Zend/Optimizer/zend_ssa.c:544-800`),
    /// though `types` makes no value of that write. A guard at `reader` is
    /// then php's on that number, which `zend_jit_trace_build_tssa` marks
    /// `MAY_BE_GUARD` in the phi's stead, so its phi propagation
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:2690-2760`) moves nothing to
    /// the loop's entry and the guard stays at `reader`, its exit numbered
    /// there (#3865).
    ///
    /// A closure that takes `$c` by value before the loop's `+=`:
    ///
    /// ```php
    /// function f() { $c = 0; $g = null; for ($i = 0; $i < 100; $i++) { $g = function () use ($c) { return $c; }; $c += 2; if ($i > 50) { $c++; } } return $c + $g(); }
    /// echo f(), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 495
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:3
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %s:2
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 2 f() %s:2
    /// ---- TRACE 2 start (side trace 1/2) f() %s:2
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    ///
    ///      TRACE 1 exit 0 f() %s:2
    /// ---- TRACE 3 start (side trace 1/0) f() %s:2
    /// ---- TRACE 3 stop (return)
    /// ---- TRACE 3 compiled
    /// ```
    ///
    /// An array that takes `$c` as its element before the `+=`:
    ///
    /// ```php
    /// function f() { $c = 0; $y = null; for ($i = 0; $i < 100; $i++) { $y = [$c]; $c += 2; if ($i > 50) { $c++; } } return $c + $y[0]; }
    /// echo f(), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 495
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:3
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %s:2
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 2 f() %s:2
    /// ---- TRACE 2 start (side trace 1/2) f() %s:2
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    ///
    ///      TRACE 1 exit 0 f() %s:2
    /// ---- TRACE 3 start (side trace 1/0) f() %s:2
    /// ---- TRACE 3 stop (return)
    /// ---- TRACE 3 compiled
    /// ```
    fn renumbered(&self, slot: Slot, reader: OpId) -> bool {
        let between = self.ops.get(self.header.index()..reader.index());
        (between.unwrap_or_default().iter())
            .any(|op| matches!(op, Op::Line(line) if self.writes_anew(line, slot)))
    }

    /// php's `is_checked_guard` (`Z/ext/opcache/jit/zend_jit_trace.c:1182-1260`)
    /// over a loop phi guarded long and `back`, what its back edge brings:
    /// a value no reference holds that an inline `ADD`, `SUB` or `MUL`, a
    /// `+=`, `-=` or `*=`, or an increment or a decrement made over
    /// numbers, which holds its overflow itself, so the guard the phi
    /// held at the header each turn is held within it, rerunning it where
    /// it overflows. The guard it holds, where it is one, and none within
    /// an increment or a decrement php's `zend_may_overflow` says cannot
    /// overflow, which its code generator compiles with no check, so the
    /// guard is held nowhere ([`Builder::may_overflow`]).
    ///
    /// The back edge brings the last value php's trace SSA numbers in the
    /// slot, and a line after `back`'s that reads it and may take a
    /// counted owner of it, an `INIT_ARRAY` or `ADD_ARRAY_ELEMENT` of it,
    /// a `SEND_VAR`, the `$c` of `$y = $c`, numbers it anew under
    /// `ZEND_SSA_RC_INFERENCE` ([`Builder::writes_anew`]). Its definition
    /// is that line, which `is_checked_guard` holds nothing of, so the
    /// phi keeps its guard at the header, #3712's program, whose array
    /// takes `$c` after the `+=`:
    ///
    /// ```php
    /// function f() { $x = [1, 2]; $r = &$x; $c = 0; for ($i = 0; $i < 100; $i++) { $c += $x[1]; $x = [$i, $c]; if ($i > 50) { $c++; } } return $c; }
    /// echo f(), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 1.2676506002282E+30
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:3
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %s:2
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 8 f() %s:2
    /// ---- TRACE 2 start (side trace 1/8) f() %s:2
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    ///
    ///      TRACE 2 exit 0 f() %s:2
    /// ---- TRACE 3 start (side trace 2/0) f() %s:2
    /// ---- TRACE 3 stop (link to 1)
    /// ---- TRACE 3 compiled
    ///
    ///      TRACE 1 exit 5 f() %s:2
    /// ---- TRACE 4 start (side trace 1/5) f() %s:2
    /// ---- TRACE 4 stop (link to 1)
    /// ---- TRACE 4 compiled
    ///
    ///      TRACE 1 exit 1 f() %s:2
    /// ---- TRACE 5 start (side trace 1/1) f() %s:2
    /// ---- TRACE 5 stop (return)
    /// ---- TRACE 5 compiled
    /// ```
    fn checked_guard(&self, back: ValueId) -> Option<Checked> {
        let made = *self.val(back);
        if made.types.meets(Types::REF) || !made.types.meets(Types::LONG) {
            return None;
        }
        let Def::Op(def) = made.def else { return None };
        let Some(Op::Line(line)) = self.ops.get(def.index()) else {
            return None;
        };
        let later = self.ops.get(def.index() + 1..).unwrap_or_default();
        if later
            .iter()
            .any(|op| matches!(op, Op::Line(after) if self.writes_anew(after, made.slot)))
        {
            return None;
        }
        // A constant that is a number, or a value that may be one.
        let number = |operand: Operand| match operand {
            Operand::Constant(Constant::Long(_) | Constant::Double(_)) => true,
            Operand::Value(value) => self.val(value).types.meets(Types::NUMBER),
            Operand::Constant(_) | Operand::Unused => false,
        };
        // A variable or a temporary no reference holds.
        let unreferenced = |operand: Operand| match operand {
            Operand::Value(value) => !self.val(value).types.meets(Types::REF),
            Operand::Constant(_) | Operand::Unused => true,
        };
        let step = matches!(
            line.opcode,
            Opcode::PreInc | Opcode::PreDec | Opcode::PostInc | Opcode::PostDec
        );
        let checked = if line.op1_def == Some(back) {
            match line.opcode {
                _ if step => {
                    let Operand::Value(read) = line.op1 else {
                        return None;
                    };
                    !self.val(read).types.meets(Types::STRING) && number(line.op1)
                }
                Opcode::AssignOp => {
                    matches!(
                        line.imm,
                        Imm::Binary(Opcode::Add | Opcode::Sub | Opcode::Mul)
                    ) && unreferenced(line.op2)
                        && number(line.op1)
                        && number(line.op2)
                }
                _ => false,
            }
        } else if line.out == Some(back) {
            match line.opcode {
                Opcode::Add | Opcode::Sub | Opcode::Mul => {
                    unreferenced(line.op1)
                        && unreferenced(line.op2)
                        && number(line.op1)
                        && number(line.op2)
                }
                _ if step => unreferenced(line.op1) && number(line.op1),
                _ => false,
            }
        } else {
            false
        };
        let kind = within(line.opcode, ZvalType::Long, line.op2)
            .filter(|kind| matches!(kind, Within::Rerun | Within::Overflowed { .. }));
        let inline = line.form == Form::Inline && !self.over_aliased(line.op1, line.op2);
        let kind = kind.filter(|_| checked && inline)?;
        if step && self.bounded(def) {
            return Some(Checked::Nowhere);
        }
        Some(Checked::Within(Placed {
            at: GuardAt::Within(def),
            check: Check::Type {
                value: back,
                tag: ZvalType::Long,
            },
            exit: draft(
                Point::Before(def),
                line.frame,
                At::overflowing(kind, line, back),
            ),
        }))
    }

    /// Whether a call into a frame above `value`'s stored it to its slot
    /// as it entered (`Op::Enter`) before `point`. A value lives in its
    /// one slot, so the slot holds it from there on.
    fn spilled_before(&self, value: ValueId, point: Point) -> bool {
        self.spilled.get(&value).is_some_and(|&stored| match point {
            Point::Start => false,
            Point::Before(at) => stored < at,
            Point::After(at) => stored <= at,
            Point::End => true,
        })
    }

    /// Where `draft` lands the interpreter, how, and the store its landing
    /// adds beside the trace's registers: past a fetch whose element's
    /// type a guard holds, the element it copied, as php's guard on a
    /// fetched element leaves for the opline after it with the copy in
    /// its stack map (`zend_jit_guard_fetch_result_type`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:8067-8085`), and with a call
    /// compiled code holds apart restored, as
    /// `zend_jit_trace_get_exit_point` adds `ZEND_JIT_EXIT_RESTORE_CALL`
    /// to any exit taken while the call chain is delayed
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:144-147`).
    fn lands(&self, draft: &Draft, delayed: bool) -> (Option<OplineIdx>, Landing, Vec<Store>) {
        match draft.at {
            At::Opline(at) => (Some(at), restored(delayed), Vec::new()),
            At::Stands | At::Landed => (None, restored(delayed), Vec::new()),
            At::Fetched {
                fetch,
                frees,
                out,
                left,
            } => {
                let copied = Store {
                    slot: self.val(out).slot,
                    from: match left {
                        Left::Copied(read) => Stored::Copied { value: out, read },
                        Left::Null => Stored::Null,
                    },
                };
                let landing = Landing {
                    frees,
                    restores_call: delayed,
                };
                (Some(OplineIdx(fetch.0 + 1)), landing, vec![copied])
            }
            At::Overflowed { op, up, wrote } => {
                let overflowed = (wrote.stores(up).into_iter().flatten())
                    .map(|(value, from)| Store {
                        slot: self.val(value).slot,
                        from,
                    })
                    .collect();
                (Some(OplineIdx(op.0 + 1)), restored(delayed), overflowed)
            }
            At::Stayed { op, wrote } => {
                let stayed = (wrote.stayed().into_iter().flatten())
                    .map(|(value, from)| Store {
                        slot: self.val(value).slot,
                        from,
                    })
                    .collect();
                (Some(OplineIdx(op.0 + 1)), restored(delayed), stayed)
            }
        }
    }

    /// One exit's spec from its draft and the state it leaves, the values
    /// in `known` held by a guard before it.
    fn spec(
        &self,
        draft: &Draft,
        known: &HashMap<ValueId, ZvalType>,
        bound: Option<u32>,
    ) -> ExitSpec {
        let empty = Snapshot::default();
        let snapshot = self.snapshot_at(draft.point).unwrap_or(&empty);
        let (opline, landing, extra) = self.lands(draft, snapshot.delayed);
        // A frame a return past the root landed in is one the trace
        // holds nothing of ([`At::Landed`]).
        if matches!(draft.at, At::Landed) {
            return ExitSpec {
                frame: draft.frame,
                opline,
                landing,
                stores: Vec::new(),
                known: Vec::new(),
                grows: draft.grows,
                link: draft.link,
                layout: false,
            };
        }
        // A caller's register value is in its slot from the call into the
        // frame above it on, which stored it there (`Op::Enter`'s
        // `stores`), so an exit from a frame above stores none of it, as
        // php's stack map, relative to the frame an exit lands in, holds
        // none of a caller's slots.
        let mut stores: Vec<Store> = snapshot
            .values
            .iter()
            .filter(|(_, value)| self.val(*value).home == Home::Register)
            .filter(|(slot, value)| {
                slot.frame == draft.frame || !self.spilled_before(*value, draft.point)
            })
            .map(|(slot, value)| Store {
                slot: *slot,
                from: Stored::Value(*value),
            })
            .collect();
        for extra in &extra {
            stores.retain(|store| store.slot != extra.slot);
            stores.push(*extra);
        }
        stores.sort_by_key(|store| store.slot);
        // What the trace knows of the frame's slots there: a value's type
        // where it was made one, or once a guard before it held it, and
        // what an entry guard held of a slot the trace has not read yet.
        let held = |value: ValueId| {
            known
                .get(&value)
                .copied()
                .or_else(|| self.born[value.0 as usize].tag())
        };
        // A slot the trace has not read yet is known as it entered where
        // that is one type, as php's code generator sets the stack type of
        // each variable whose entry info is concrete before the trace's
        // first op (`Z/ext/opcache/jit/zend_jit_trace.c:4228-4245`), and
        // a frame it pushes holds its variables past the arguments unset.
        let read: HashSet<Slot> = snapshot.values.iter().map(|(slot, _)| *slot).collect();
        let concrete = (0..self.values.len())
            .filter_map(|at| u32::try_from(at).ok().map(ValueId))
            .filter(|value| {
                let entry = self.val(*value);
                entry.def == Def::Entry
                    && bound.is_none_or(|bound| entry.slot.var < bound)
                    && self.born[value.0 as usize].tag().is_some()
                    && !self.aliased(entry.slot)
            });
        let entered = (known.keys().copied())
            .filter(|value| self.val(*value).def == Def::Entry)
            .chain(concrete)
            .map(|value| (self.val(value).slot, value))
            .filter(|(slot, _)| !read.contains(slot))
            .collect::<BTreeMap<Slot, ValueId>>();
        // Past an op, a slot it wrote is known as nothing, which of the
        // ways it went it wrote it on or not, as php sets `IS_UNKNOWN` for
        // what an exit past a handler or a `JMPZ_EX` leaves there
        // (`zend_jit_trace_handler`, `Z/ext/opcache/jit/zend_jit_ir.c:17267-17285`;
        // `Z/ext/opcache/jit/zend_jit_trace.c:5707-5712`).
        let wrote = |value: ValueId| matches!(self.val(value).def, Def::Op(def) if Point::After(def) == draft.point);
        let mut known: Vec<Known> = (snapshot.values.iter().copied())
            .filter(|(_, value)| !wrote(*value))
            .chain(entered)
            .filter(|(slot, _)| slot.frame == draft.frame)
            .filter_map(|(slot, value)| {
                Some(Known {
                    slot,
                    tag: held(value)?,
                })
            })
            .collect();
        // A constant an overflow left in its slot, and a long a step
        // that did not overflow left there, is known as what it is, as
        // php's `zend_jit_math_long_long` and `zend_jit_inc_dec` set the
        // slot's stack type before they take the exit.
        for store in &extra {
            if let Some(tag) = store.from.stack_type() {
                known.retain(|known| known.slot != store.slot);
                known.push(Known {
                    slot: store.slot,
                    tag,
                });
            }
        }
        known.sort_by_key(|known| known.slot);
        ExitSpec {
            frame: draft.frame,
            opline,
            landing,
            stores,
            known,
            grows: draft.grows,
            link: draft.link,
            layout: false,
        }
    }
}

/// Where the trace goes on after `op` at `at`, `len` oplines long, where
/// the record says: its jump's `target` where the jump was taken or
/// always jumps, the opline past it where it went on.
fn went(
    at: OplineIdx,
    op: &Opline,
    target: Option<OplineIdx>,
    len: u32,
    branch: Option<bool>,
) -> Option<OplineIdx> {
    match (branch, op.opcode == Opcode::Jmp, op.jump_target().is_some()) {
        (Some(true), ..) | (None, true, _) => target,
        // A table's jump the record does not say.
        (None, false, true) => None,
        _ => Some(OplineIdx(at.0 + len)),
    }
}

/// Which way a conditional jump went, the jump fused to a comparison or
/// the opline's own, and where it goes: `len` oplines on, or to `target`;
/// the opline's `opcode` and what its condition holds once its guards
/// held, `held`.
#[derive(Clone, Copy)]
struct Branched {
    taken: bool,
    fused: bool,
    len: u32,
    target: Option<OplineIdx>,
    opcode: Opcode,
    held: Types,
}

/// How a strict comparison, a string's length, a boolean cast, a
/// `foreach`'s free, a concatenation, a rope's piece or `.=` is compiled
/// over what its operands are seen as, none for any other opline. Inline:
/// two scalars compared, or a string beside a string or a scalar, as php's
/// `zend_jit_identical` compares them inline or calls `zend_is_identical`
/// (`Z/ext/opcache/jit/zend_jit_ir.c:7292-7532`); a string's length read
/// in place, php's `zend_jit_strlen` (`:11623-11660`); a scalar's truth,
/// php's `zend_jit_bool_jmpznz`;
/// an array, which holds no iterator to delete, given up as php's
/// `zend_jit_free` gives it up (`:11543-11580`); a rope's piece that is a
/// string, which php's `zend_jit_rope` stores inline and joins through
/// `zend_jit_rope_end` (`:11753-11799`); a concatenation of two strings,
/// through php's `zend_jit_fast_concat_helper` (`zend_jit_concat`,
/// `:6016-6118`); and `.=` of a string onto a variable's string, through
/// php's `zend_jit_fast_assign_concat_helper` (`zend_jit_assign_op`,
/// `:6120-6290`). Stepped otherwise.
fn strung(op: &Opline, seen: Seen) -> Option<Form> {
    let [t1, t2, _] = seen.types;
    let string = |t: Types, at: usize| t == Types::STRING && seen.read[at] == Read::InPlace;
    let inline = match op.opcode {
        Opcode::IsIdentical | Opcode::IsNotIdentical => {
            (t1.scalar() && t2.scalar())
                || (string(t1, 0) && (string(t2, 1) || t2.scalar()))
                || (t1.scalar() && string(t2, 1))
        }
        Opcode::Strlen => string(t1, 0),
        // php's `zend_jit_fetch_dim_read` over a string
        // (`Z/ext/opcache/jit/zend_jit_ir.c:12720-12755`): by a long or a
        // string, what a key of any other kind warns of being the
        // handler's.
        Opcode::FetchDimR if t1 == Types::STRING => {
            (t2 == Types::LONG || t2 == Types::STRING) && seen.read[1] == Read::InPlace
        }
        Opcode::Bool | Opcode::BoolNot => t1.scalar(),
        Opcode::FeFree => t1.tag() == Some(ZvalType::Array),
        Opcode::RopeInit | Opcode::RopeAdd | Opcode::RopeEnd => string(t2, 1),
        Opcode::Concat | Opcode::FastConcat => string(t1, 0) && string(t2, 1),
        Opcode::AssignOp if lattice::binary_of(op.extended_value) == Some(Opcode::Concat) => {
            op.op1_type == OperandKind::Cv
                && op.out_type == OperandKind::Unused
                && string(t1, 0)
                && string(t2, 1)
        }
        _ => return None,
    };
    Some(if inline { Form::Inline } else { Form::Step })
}

/// Whether `opcode` reads its operand's truth, php's
/// `zend_jit_bool_jmpznz`'s oplines.
fn takes_truth(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::Jmpz
            | Opcode::Jmpnz
            | Opcode::JmpzEx
            | Opcode::JmpnzEx
            | Opcode::Bool
            | Opcode::BoolNot
    )
}

/// The jump after `op` it is fused to, where it is a comparison whose
/// direction the record keeps.
fn fused(op: &Opline, after: Option<&Opline>, branch: Option<bool>) -> Option<Fused> {
    let jump = after.filter(|_| branch.is_some() && !record::conditional(op))?;
    Some(if jump.opcode == Opcode::Jmpz {
        Fused::Jmpz
    } else {
        Fused::Jmpnz
    })
}

/// Whether the parameter numbered `n` of a user function declaring
/// `params` takes its argument by value: php's `!ARG_MUST_BE_SENT_BY_REF`,
/// a number past the declared parameters asking the variadic's.
fn sent_by_value(params: &[Parameter], n: u32) -> bool {
    let at = usize::try_from(n.saturating_sub(1)).unwrap_or(usize::MAX);
    params
        .get(at)
        .or_else(|| params.last().filter(|param| param.variadic))
        .is_none_or(|param| !param.by_ref || param.prefer_ref)
}

/// Whether a `TYPE_CHECK` is compiled inline, php's `zend_jit_type_check`
/// (`Z/ext/opcache/jit/zend_jit_ir.c:8162-8330`), over a value the trace
/// reads as it stands: a temporary, which is never a reference, or a
/// variable that is neither a reference, read in place by `read1`, nor of a type that
/// may be one, nor unset, which would warn. `is_resource()`, which asks
/// whether the resource is closed, is the handler's, as php's code
/// generator leaves it to it (`Z/ext/opcache/jit/zend_jit_trace.c:5536-5540`).
fn type_checked(op: &Opline, t1: Types, read1: Read) -> bool {
    op.extended_value & MAY_BE_RESOURCE == 0
        && match op.op1_type {
            OperandKind::Tmp => true,
            OperandKind::Cv | OperandKind::Var => {
                read1 == Read::InPlace
                    && t1.kinds() != Types::NONE
                    && !t1.meets(Types::REF.or(Types::UNDEF))
            }
            OperandKind::Const | OperandKind::Unused => false,
        }
}

/// Whether an `ASSIGN_DIM` over a variable's array is compiled inline over
/// what it sees of the array, the key and the value, `[t1, t2, td]`, its
/// `OP_DATA` `data`: an array of either layout, which the write tests
/// where it may be both, by a key the lookups take ([`keyed`]), of a scalar, or of a value no scalar moved or copied
/// whole into the element where its `out` slot is unused and the
/// value is not the array itself, as php's `zend_jit_assign_dim` writes
/// through `zend_jit_assign_to_variable`
/// (`Z/ext/opcache/jit/zend_jit_ir.c:13352-13527`, `:5793-5950`). php
/// compiles the write inline over an array that may be packed, testing
/// its layout where it may be either, and adds no layout guard where the
/// array was recorded packed (`Z/ext/opcache/jit/zend_jit_trace.c:1871-1893`).
fn element_written(
    op: &Opline,
    data: Option<&Opline>,
    [t1, t2, td]: [Types; 3],
    code: &OpArray,
) -> bool {
    let whole = |data: &Opline| {
        op.out_type == OperandKind::Unused
            && !(data.op1_type == OperandKind::Cv && data.op1 == op.op1)
            && copied_whole(data.op1_type, data.op1, td, code)
    };
    t1.tag() == Some(ZvalType::Array) && keyed(op, t2) && (td.scalar() || data.is_some_and(whole))
}

/// Whether a dimension opline is compiled inline over what it sees of
/// the array, the key and the value, `[t1, t2, td]`, and how it reads
/// `op1`, `read1`: a read ([`fetched_inline`]), a read under `BP_VAR_IS`
/// and `isset()` alone, as php's `zend_jit_isset_isempty_dim` serves
/// (`Z/ext/opcache/jit/zend_jit_ir.c:13157-13350`), each over a
/// variable's array or a temporary's, which it frees after, as php
/// compiles them over a `TMP` or a `VAR` (`Z/ext/opcache/jit/zend_jit_trace.c:5847-5880`,
/// `:5954-6030`); and a write ([`element_written`]), a compound write
/// ([`compound_in_place`]) and a fetch for writing, php's
/// `zend_jit_fetch_dim`, over a variable's array, the one the reference
/// it holds refers to, or the one the `VAR` a fetch for writing left
/// indirect points at, which php follows through
/// `zend_jit_fetch_indirect_var` (`:5068-5083`, `:5893-5900`).
fn dim_inline(
    (op, data): (&Opline, Option<&Opline>),
    [t1, t2, td]: [Types; 3],
    read1: Read,
    code: &OpArray,
) -> bool {
    let array = t1.tag() == Some(ZvalType::Array);
    let written = op.op1_type == OperandKind::Cv || matches!(read1, Read::Indirect(_));
    match op.opcode {
        Opcode::FetchDimR | Opcode::FetchDimIs => fetched_inline(op, t1, t2),
        Opcode::IssetIsemptyDimObj => {
            op.extended_value & ISEMPTY == 0 && fetched_inline(op, t1, t2)
        }
        Opcode::AssignDim => written && element_written(op, data, [t1, t2, td], code),
        Opcode::AssignDimOp => written && compound_in_place(op, t1, t2, td),
        Opcode::FetchDimW | Opcode::FetchDimRw => {
            written && op.out_type == OperandKind::Var && array && keyed(op, t2)
        }
        _ => false,
    }
}

/// Whether a `FETCH_DIM_R` is compiled inline over what it sees of the
/// array and the key, `t1` and `t2`, as php's `zend_jit_fetch_dim_read`
/// compiles a read of an array no constant holds by a long or a string
/// key (`Z/ext/opcache/jit/zend_jit_ir.c:12513-12700`): a packed array's
/// slot by a long key in place, and every other element through the
/// table's lookup, `zend_hash_find_known_hash`, `zend_jit_symtable_find`
/// or `_zend_hash_index_find` (`:11917-12511`). A key no constant is is
/// a variable's or a long, which the read gives up nothing of; php's
/// code frees a temporary string, which this form leaves to the handler.
fn fetched_inline(op: &Opline, t1: Types, t2: Types) -> bool {
    op.op1_type != OperandKind::Const && t1.tag() == Some(ZvalType::Array) && keyed(op, t2)
}

/// Whether an inline form moves or copies an operand of `kind`, at
/// `field`, seen as `t`, whole: what a temporary or a variable holds, of a
/// kind the trace knows, no reference and set, or a constant of `code` no
/// reference counts, which needs no owner.
fn copied_whole(kind: OperandKind, field: u32, t: Types, code: &OpArray) -> bool {
    match kind {
        OperandKind::Tmp | OperandKind::Var | OperandKind::Cv => {
            t.kinds() != Types::NONE && !t.meets(Types::REF.or(Types::UNDEF))
        }
        OperandKind::Const => code
            .literals
            .get(field as usize)
            .is_some_and(|literal| !literal.is_refcounted()),
        OperandKind::Unused => false,
    }
}

/// Whether a `foreach`'s reset or fetch is compiled inline over `t1`,
/// what it sees of its array, read in place by `read1` or through a
/// reference, the record having gone `branch`: php's `zend_jit_fe_reset`
/// over an array no constant holds, which goes on whether the array holds
/// anything or not, as the handler does
/// (`Z/ext/opcache/jit/zend_jit_ir.c:13756-13775`); php's
/// `zend_jit_fe_fetch` over a packed array a `VAR` iterator holds into a
/// variable, where the loop goes on, the end its exit (`:13800-14120`).
/// Where the record ended the loop, php serves the fetch as it is, and so
/// does the step.
fn iterated(op: &Opline, t1: Types, read1: Read, branch: Option<bool>) -> bool {
    match op.opcode {
        Opcode::FeResetR => {
            op.op1_type != OperandKind::Const
                && t1.tag() == Some(ZvalType::Array)
                && read1 == Read::InPlace
        }
        Opcode::FeFetchR => {
            op.op1_type == OperandKind::Var
                && t1 == Types::ARRAY.laid_out(Layout::Packed)
                && op.op2_type == OperandKind::Cv
                && branch == Some(false)
        }
        _ => false,
    }
}

/// What a `RETURN` of `code` hands its caller where it leaves in place:
/// a constant no reference counts, copied as it is; a temporary that is
/// no reference, moved; a variable that may hold a reference, which no
/// guard held (php's trace SSA reads it `IS_UNKNOWN`), or an object or a
/// string, copied through it with an owner more, where it is never unset,
/// as php serves the `RETURN` of a value that may be unset through its
/// handler; a scalar otherwise, which the form holds it to
/// (`zend_jit_return`, `Z/ext/opcache/jit/zend_jit_ir.c:11265-11400`, and
/// its caller, `Z/ext/opcache/jit/zend_jit_trace.c:5580-5607`). php's trace
/// moves a string or an array out of its variable; a string's extra owner
/// is seen by nothing, and an array's, given up with the variable, may
/// take it as a possible root, which `gc_status()` reads, so a variable's
/// array stays the interpreter's.
fn returned(op: &Opline, code: &OpArray, t1: Types) -> Returned {
    match op.op1_type {
        OperandKind::Const if !t1.scalar() => code
            .literals
            .get(op.op1 as usize)
            .filter(|literal| !literal.is_refcounted())
            .map_or(Returned::Scalar, |literal| {
                Returned::Constant(std::ptr::from_ref(literal).addr())
            }),
        OperandKind::Tmp if !t1.scalar() => Returned::Moved,
        OperandKind::Var if !t1.scalar() && !t1.meets(Types::REF) => Returned::Moved,
        OperandKind::Cv
            if !t1.meets(Types::UNDEF)
                && (t1.meets(Types::REF) || !(t1.scalar() || t1.meets(Types::ARRAY))) =>
        {
            Returned::Shared
        }
        _ => Returned::Scalar,
    }
}

/// What `op` names beside its operands that its inline form reads.
fn imm(op: &Opline) -> Imm {
    match op.opcode {
        Opcode::SendVal | Opcode::SendVar | Opcode::SendRef
            if op.op2_type == OperandKind::Unused =>
        {
            Imm::Arg(op.op2)
        }
        Opcode::AssignOp | Opcode::AssignDimOp => {
            lattice::binary_of(op.extended_value).map_or(Imm::None, Imm::Binary)
        }
        Opcode::TypeCheck => Imm::Mask(op.extended_value),
        opcode if opcode.is_frameless_icall() => Imm::Frameless(FramelessAt(op.extended_value)),
        _ => Imm::None,
    }
}

/// Where a table's jump at `at`, `SWITCH_LONG`, `SWITCH_STRING` or
/// `MATCH` over a subject of `t1`, leaves a trace that goes on at `next`: an exit at
/// each opline it may go to but `next`, in the order php's `zend_jit_switch`
/// takes them (`Z/ext/opcache/jit/zend_jit_ir.c:16222-16337`): the
/// default's, then for a `switch` over a subject that may be of its kind
/// the `CASE` chain's past it, then each arm's in the table's order, one
/// exit per opline. None for any other opline, and for a constant
/// subject, whose one target is `next`.
fn arms(
    op: &Opline,
    at: OplineIdx,
    code: &OpArray,
    next: OplineIdx,
    t1: Types,
) -> Option<Vec<OplineIdx>> {
    let kind = match op.opcode {
        Opcode::SwitchLong => Types::LONG,
        Opcode::SwitchString => Types::STRING,
        Opcode::Match => Types::LONG.or(Types::STRING),
        _ => return None,
    };
    if op.op1_type == OperandKind::Const {
        return Some(Vec::new());
    }
    let table = code.literals.get(op.op2 as usize)?.as_arr()?;
    let mut arms = Vec::new();
    let mut arm = |at: OplineIdx| {
        if at != next && !arms.contains(&at) {
            arms.push(at);
        }
    };
    arm(OplineIdx(op.extended_value));
    if !lattice::deref(t1).meets(kind) {
        return Some(arms);
    }
    if op.opcode != Opcode::Match {
        arm(OplineIdx(at.0 + 1));
    }
    for (_, target) in &table {
        arm(OplineIdx(u32::try_from(target.as_long()?).ok()?));
    }
    Some(arms)
}

/// An exit with nothing to store beside the trace's registers and no
/// trace to link to.
fn draft(point: Point, frame: Frame, at: At) -> Draft {
    Draft {
        point,
        frame,
        at,
        grows: Grows::Side,
        link: None,
    }
}

/// The exits a trace has taken so far, by number, and where each spec
/// that names an opline was taken, as [`Reused`] keys it.
#[derive(Default)]
struct Exits {
    specs: Vec<ExitSpec>,
    taken: HashMap<Reused, ExitId>,
}

/// What two places that leave through one exit hold alike, php's reuse
/// key: the exit's opline, `flags` and stack map, with its frame named by
/// the op array it runs and not by its place in the trace. php's stack
/// map is held relative to the frame pointer, the frame the exit lands
/// in, so one guard at one opline of two inlined levels of a recursion,
/// with the same types known, is one exit; so it is here, since an exit
/// lands in whichever frame is current. The stores stay as they are: a
/// register value lives in one frame's slot, so two places that store
/// one leave from one frame; but what an overflow left is a
/// constant its site writes in its own frame, as php's stack map holds
/// it, so it is named by its slot in the frame alone.
#[derive(Debug, PartialEq, Eq, Hash)]
struct Reused {
    level: Level,
    spec: ExitSpec,
}

/// The frame an exit leaves from, as php's frame-relative stack map
/// names it: the op array it runs, and its untyped parameters php's
/// stack map holds in the caller's registers they were sent from, by
/// variable, which keep an inlined level's exits apart from its
/// caller's, as php's register references do (`Builder::held_at`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Level {
    func: FuncRef,
    held: Vec<(u32, ValueId)>,
}

impl Reused {
    /// The key of `spec`, which leaves from `level`.
    fn of(spec: &ExitSpec, level: Level) -> Reused {
        let relative = |slot: Slot| Slot {
            frame: Frame::ROOT,
            ..slot
        };
        Reused {
            level,
            spec: ExitSpec {
                frame: Frame::ROOT,
                stores: (spec.stores.iter())
                    .map(|store| match store.from.stack_type() {
                        Some(_) => Store {
                            slot: relative(store.slot),
                            ..*store
                        },
                        None => *store,
                    })
                    .collect(),
                known: (spec.known.iter())
                    .map(|known| Known {
                        slot: relative(known.slot),
                        ..*known
                    })
                    .collect(),
                ..spec.clone()
            },
        }
    }
}

impl Exits {
    /// The exit that leaves as `spec` says, from `level`:
    /// php's `zend_jit_trace_get_exit_point`, which reuses an exit whose
    /// opline, `flags` and stack map match one taken
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:168-185`), and refuses the
    /// trace with `TooManyExits` where a new one would pass
    /// `ZEND_JIT_TRACE_MAX_EXITS`. An exit that names no opline is never
    /// reused, as php reuses none whose `to_opline` is null.
    fn take(&mut self, spec: ExitSpec, level: Level) -> Result<ExitId, Abort> {
        let key = spec.opline.is_some().then(|| Reused::of(&spec, level));
        if let Some(id) = key.as_ref().and_then(|key| self.taken.get(key)) {
            return Ok(*id);
        }
        let id = self.push(spec)?;
        if let Some(key) = key {
            self.taken.insert(key, id);
        }
        Ok(id)
    }

    /// A new exit that leaves as `spec` says, which no later place takes
    /// by its spec: refused with `TooManyExits` past
    /// `ZEND_JIT_TRACE_MAX_EXITS`, as php refuses any exit point there.
    fn push(&mut self, spec: ExitSpec) -> Result<ExitId, Abort> {
        if self.specs.len() == MAX_EXITS {
            return Err(Abort {
                stop: Stop::TooManyExits,
                why: "more exits than php's ZEND_JIT_TRACE_MAX_EXITS",
            });
        }
        let id = ExitId(u16::try_from(self.specs.len()).map_err(|_| mismatch("too many exits"))?);
        self.specs.push(spec);
        Ok(id)
    }
}

/// The exits numbered, the unwinds, the guards naming their exits, and
/// how the trace ends, through the exits a loop's interrupt test or a
/// trace's end take.
struct Numbered {
    exits: Vec<ExitSpec>,
    unwinds: Vec<Unwind>,
    guards: Vec<Guard>,
    end: Option<End>,
}

/// How a trace ends, for its numbering ([`Builder::closes`]).
#[derive(Clone, Copy)]
enum Closes {
    /// A loop whose interrupt test leaves through an exit at its header,
    /// numbered after every exit of its body.
    ThroughExit(Repeat),
    /// A loop whose header numbers no exit, whole before the numbering:
    /// its test takes php's stub, or it takes no test.
    NoExit(LoopHeader),
    /// A trace that does not loop, through the exit it ends by.
    Leave,
}

/// Where an exit falls in the order the trace meets them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Order {
    /// A guard held as the trace enters, by the slot it holds, as php's
    /// code holds the entry's types slot by slot (`zend_jit_trace`'s
    /// loop over `op_array->last_var + op_array->T`).
    Entry(Option<Slot>),
    /// At an op, in the order of the op's stages.
    At(OpId, Stage),
    /// Where the trace ends: a loop's interrupt test, or the exit a trace
    /// that does not loop ends through.
    End,
}

/// What happens at an op, in the order php's code generator takes its
/// exits: the guards before it on its operands' types; the exit the way
/// the trace did not go leaves through, which php takes before it emits
/// the op's code, its smart branch's or `FE_FETCH_R`'s
/// (`Z/ext/opcache/jit/zend_jit_trace.c`'s `zend_jit_trace_get_exit_point`
/// before `zend_jit_cmp`, `zend_jit_isset_isempty_dim` or
/// `zend_jit_fe_fetch`, whose packed guard follows it,
/// `Z/ext/opcache/jit/zend_jit_ir.c:13839-13843`); the exit an
/// inline fetch opens its code with, php's `ZEND_JIT_EXIT_TO_VM` that
/// `zend_jit_fetch_dim_read` takes first (`Z/ext/opcache/jit/zend_jit_ir.c:12540-12547`),
/// or `FETCH_DIM_IS`'s `not_found_exit_addr` (`:12576-12610`);
/// the layout guard php takes inside the op's code, after it
/// (`zend_jit_fetch_dimension_address_inner`, `:11966-11980`); the
/// guards the op holds on what it computes; then any other exit of its
/// own, an inline `INIT_*`'s where the callee is not the one recorded,
/// php's guard on the function its cache slot holds; and last the stack
/// check of the frame it pushes (`zend_jit_init_fcall` then
/// `zend_jit_push_call_frame`, `:8790-8906`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Before,
    Branches,
    Opens,
    Inside,
    Within,
    Leaves,
    Pushes,
}

/// Why an opline leaves of its own.
#[derive(Clone, Copy)]
enum Leaves {
    /// It cannot finish: through an exit.
    Unfinished,
    /// A fetch found no element and wrote `null`: through an exit past
    /// it ([`Within::Fetched`]'s `missed`).
    Missed,
    /// A kernel it called threw, bailed out or panicked: through an
    /// unwind.
    Thrown,
}

/// Where an exit's draft is kept until it is numbered.
#[derive(Clone, Copy)]
enum Source {
    Guard(usize),
    Leave(usize),
    Interrupt(Repeat),
    End,
}

#[derive(Clone, Copy)]
enum Narrowed {
    Tag(ZvalType),
    Layout(Layout),
}

impl Narrowed {
    /// What a guard before an op narrows a loop's phi to, where it may
    /// move to the loop's entry: a type, or a variable's layout; a
    /// reference's array is held at each read, never once.
    fn of(check: Check) -> Option<(ValueId, Narrowed)> {
        match check {
            Check::Type { value, tag } => Some((value, Narrowed::Tag(tag))),
            Check::Layout {
                value,
                layout,
                read: Read::InPlace,
            } => Some((value, Narrowed::Layout(layout))),
            _ => None,
        }
    }
}

/// The type a guard's exit and every exit after it know its slot holds,
/// php's `SET_STACK_TYPE` past a guard: a type's, an array's held in place;
/// none for a reference or an indirect `VAR`, whose type no stack map
/// keeps, as php's `zend_jit_fetch_reference`, `zend_jit_guard_reference`
/// and `zend_jit_fetch_indirect_var` set none, nor for a guard on no slot.
fn stack_type(check: Check) -> Option<(ValueId, ZvalType)> {
    match check {
        Check::Type { value, tag } => Some((value, tag)),
        Check::Layout {
            value,
            read: Read::InPlace,
            ..
        } => Some((value, ZvalType::Array)),
        Check::Layout {
            read: Read::Through | Read::Indirect(_),
            ..
        }
        | Check::Deref { .. }
        | Check::Reference { .. }
        | Check::Indirect { .. }
        | Check::Class { .. }
        | Check::Direction { .. }
        | Check::Stands { .. }
        | Check::Callee { .. }
        | Check::Arm { .. }
        | Check::Room { .. } => None,
    }
}

fn restored(delayed: bool) -> Landing {
    if delayed {
        Landing::RESTORE_CALL
    } else {
        Landing::PLAIN
    }
}
