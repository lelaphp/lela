//! The walk that builds a [`TraceIr`] (`jit-design.md` §4.2): one pass
//! over the record's events in order, php's `zend_jit_trace_build_tssa`
//! with its type inference and guard placement folded into the one
//! loop, then the exits numbered and laid out from the state the trace
//! held where each leaves.

use std::collections::{HashMap, HashSet};

use php_engine::tier::{FuncRef, MapPtrRaw, PlainCall, Sample, UnitView};
use php_heap::ZvalType;
use php_unit::decl::Parameter;
use php_unit::{CacheSlot, Effects, OpArray, Opcode, OperandKind, Opline, OplineIdx};

use super::lattice::{self, Types};
use super::opcodes::{
    Declared, ISEMPTY, Within, arith, assigned, check_sites, compiled_inline, constant,
    constant_types, goes_one_way, guard_sites, inline_may_leave, is_call, keeps_temporaries,
    may_exit, op1_types, out_types, packed_guard, param_types, params, reads, reads_by_name,
    referenced, slot_of, within, writes, written, written_after_entry,
};
use super::{
    Abort, BodyEntry, Check, Constant, Def, End, Entry, ExitId, ExitSpec, Form, Frame, FrameInfo,
    Fused, Grows, Guard, GuardAt, Home, Imm, Interrupt, Known, Landing, Layout, Leave, Line,
    LoopHeader, Op, OpId, Operand, Place, Read, Repeat, Slot, Store, Stored, TraceIr, Unwind,
    UnwindId, Value, ValueId,
};
use crate::record::{self, Began, Callee, Event, Landed, Start, Stop, TraceRecord};
use crate::table::TraceId;

mod loads;
mod stack;

/// php's `ZEND_JIT_TRACE_MAX_EXITS`: the most exits one trace has.
const MAX_EXITS: usize = 512;
/// What a value no reference counts is: what a slot may hold and be
/// overwritten or freed with no destructor running.
const UNCOUNTED: Types = Types::UNDEF
    .or(Types::NULL)
    .or(Types::BOOL)
    .or(Types::NUMBER);

/// The trace `rec` builds, read through `view`.
pub(super) fn build(rec: &TraceRecord, view: &UnitView) -> Result<TraceIr, Abort> {
    if !rec.stop.compiles() {
        return Err(Abort {
            stop: rec.stop,
            why: "the recording stopped on a failed attempt",
        });
    }
    let mut builder = Builder::new(rec, view)?;
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
}

/// What the trace held before one op: every value of a live frame, and
/// how many calls compiled code began are under way.
#[derive(Default, Clone)]
struct Snapshot {
    values: Vec<(Slot, ValueId)>,
    calls: usize,
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
    /// After a fetch whose element's type a guard holds: the opline past
    /// it with the element, read as `read` says, stored and its
    /// temporaries freed, or, while a call is under way, the fetch again.
    Fetched {
        fetch: OplineIdx,
        op1: bool,
        op2: bool,
        out: ValueId,
        read: Read,
    },
}

/// An exit before it is numbered.
#[derive(Clone, Copy)]
struct Draft {
    point: Point,
    frame: Frame,
    at: At,
    extra: Option<Store>,
    grows: Grows,
    link: Option<TraceId>,
}

/// What an opline sees of its three operands once its guards held, which
/// it sees through a reference, and what php's inference knows of each,
/// which is what it writes is typed by.
#[derive(Clone, Copy)]
struct Seen {
    types: [Types; 3],
    referenced: [bool; 3],
    /// The seen types of a value no symbol table reaches, and the value's
    /// own of one a symbol table reaches, which a guard narrows for its
    /// opline alone, as php's `CHECK_OP_TRACE_TYPE` narrows `op1_info`
    /// and leaves the SSA variable's info as anything: so what an opline
    /// over such a variable writes is what it writes over anything, and
    /// the opline that reads it holds it, as php's does.
    inferred: [Types; 3],
}

/// What one opline wrote: its `out` slot, and the values it wrote there
/// and to its variable operands.
struct Written {
    slot: Option<Slot>,
    out: Option<ValueId>,
    op1: Option<ValueId>,
    op2: Option<ValueId>,
}

/// A guard before its exit is numbered.
#[derive(Clone, Copy)]
struct Placed {
    at: GuardAt,
    check: Check,
    exit: Draft,
}

/// A call an `INIT_*` began: whether compiled code began it, which may
/// leave the interpreter's call chain behind it until the call is made,
/// and what each argument was sent as, where the send says.
struct Call {
    inline: bool,
    /// Per argument, what its send wrote, where it sent a value.
    args: Vec<Sent>,
    /// For a native's call, what its declaration says it returns.
    returns: Option<Types>,
    /// Its callee, as the recording wrote it down.
    callee: Callee,
    /// Whether compiled code pushed it in place ([`Op::InitCall`]'s
    /// `plain`), and so makes it in place.
    plain: bool,
}

/// What compiled code serves an inline opline's work in place of the
/// kernel it would call, so that it takes none of the kernel's exits, as
/// php's code generator takes none there: the push of a plain call, a
/// send to a callee that takes the argument by value, the activation of
/// a plain call, and the leave of a frame that holds nothing to give up.
enum InPlace {
    Push(PlainCall),
    Send,
    Make,
    Leave,
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
    /// php's code generator's guards on a value a symbol table reaches at
    /// an opline its inference guards nowhere (`CHECK_OP_TRACE_TYPE`), held
    /// once a guard of its inference holds the same value anywhere in the
    /// trace, which is what marks it `MAY_BE_GUARD`.
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
    /// In a trace that does not loop, the guards held at the first read
    /// of what the root frame entered with, which php's allocator may
    /// load at entry ([`Builder::load_at_entry`]).
    entry_reads: Vec<usize>,
    /// Per line, the operands' types its recording saw.
    sampled: HashMap<OpId, [Option<Sample>; 3]>,
}

/// php's `MAY_BE_RESOURCE`, `1 << IS_RESOURCE`, the mask a `TYPE_CHECK`
/// for `is_resource()` names.
const MAY_BE_RESOURCE: u32 = 1 << ZvalType::Resource as u32;

impl<'a> Builder<'a> {
    fn new(rec: &'a TraceRecord, view: &'a UnitView) -> Result<Builder<'a>, Abort> {
        let mut builder = Builder {
            rec,
            view,
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
            entry_reads: Vec::new(),
            sampled: HashMap::new(),
        };
        let root = builder.frame(rec.root, Place::Root, None)?;
        builder.stack.push(root);
        Ok(builder)
    }

    /// A new frame of `func`.
    fn frame(
        &mut self,
        func: FuncRef,
        place: Place,
        called_at: Option<OplineIdx>,
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
        });
        self.meta.push(Meta {
            code,
            parameters: self.view.parameters(func),
            aliased,
            called_at,
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
            let next = ahead.first();
            match event {
                Event::Op {
                    func,
                    opline,
                    op1,
                    op2,
                    data,
                    branch,
                } => self.line(*func, *opline, [*op1, *op2, *data], *branch, ahead)?,
                Event::InitCall { callee, began } => self.init_call(*callee, *began)?,
                Event::Enter { func } => self.enter(*func)?,
                Event::Back => self.back(next)?,
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
    /// `Z/ext/opcache/jit/zend_jit_trace.c:6532-6556`). Here only where its
    /// frame leaves in place and its value, held as the opline's guard
    /// holds it ([`Builder::guard_use`]), is a scalar; otherwise the
    /// interpreter serves it, through the trace's end.
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
        let scalar = match (op.op1_type, op1) {
            (OperandKind::Unused, _) => true,
            (OperandKind::Const, _) => constant_types(constant(code, op.op1), code).scalar(),
            (_, Some(sample)) if !sample.indirect => Types::of_tag(sample.kind).scalar(),
            (kind, _) => slot_of(frame, kind, op.op1).is_some_and(|slot| {
                let held = self.slots[usize::from(frame.0)].get(slot.var as usize);
                match held {
                    Some(Some(value)) => self.val(*value).types,
                    _ => self.entry_types(slot),
                }
                .scalar()
            }),
        };
        Ok(scalar)
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
        for var in 0..size {
            if used[var] && defined[var] {
                let slot = Slot {
                    frame: Frame::ROOT,
                    var: u32::try_from(var).unwrap_or(u32::MAX),
                };
                self.phi(slot);
            }
        }
        Ok(())
    }

    /// The phis of a recursive call: the root's parameters, php's
    /// `zend_jit_trace_add_call_phis`.
    fn call_phis(&mut self) {
        let meta = self.meta(Frame::ROOT);
        for param in params(meta.code, meta.parameters) {
            self.phi(Slot {
                frame: Frame::ROOT,
                var: param.cv,
            });
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
            self.phi(slot);
        }
    }

    /// A phi at the header for `slot`, entered with what it holds now.
    fn phi(&mut self, slot: Slot) {
        let entry = self.current(slot);
        let types = self.val(entry).types;
        let phi = self.value(slot, types, Def::Phi { entry, back: entry }, Home::Memory);
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
        let op = *code
            .oplines
            .get(at.0 as usize)
            .ok_or_else(|| mismatch("an opline past its op array"))?;
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
        let form = self.form(frame, &op, seen, next);
        let in_place = (form == Form::Inline)
            .then(|| self.in_place(frame, &op, next))
            .flatten();
        self.sampled.insert(idx, samples);
        let map_ptr = self.map_ptr(func, &op, form);
        self.record_send(frame, &op, op1);
        if !keeps_temporaries(op.opcode) {
            // The opline frees every temporary it reads, as its handler
            // does: the slot holds nothing an exit stores after it.
            for operand in [op1, op2, data] {
                self.consume(operand);
            }
        }
        let [t1, ..] = seen.types;
        let written = self.write(
            idx,
            frame,
            &op,
            data_line.as_ref(),
            seen,
            samples[0],
            form,
            fused.is_some(),
        );
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
                    owners: !self.uncounted(frame),
                },
                Some(InPlace::Make) => Imm::Make {
                    returns: (op.out_type != OperandKind::Unused).then_some(op.out),
                },
                _ => imm(&op),
            },
            form,
            map_ptr,
            exit: None,
            unwind: None,
        }));
        let fused = fused.is_some();
        let len = if fused || data_line.is_some() { 2 } else { 1 };
        let jump = if fused { after } else { Some(op) };
        let target = jump.and_then(|jump| jump.jump_target());
        self.successor = went(at, &op, target, len, branch);
        self.own_exit(idx, frame, &op, (form, in_place.as_ref()), t1);
        if let Some(InPlace::Push(plain)) = in_place {
            self.pushing = Some(plain);
        }
        self.stands(idx, (frame, at), &op, after.as_ref(), ahead, t1);
        if let Some(taken) = branch {
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
    /// known: its `out` slot, unless a call it makes writes it on return
    /// or a fused jump consumes it; its variable `op1` and `op2`, and its
    /// `OP_DATA`'s; and what it returns, for the frame it returns to.
    #[allow(
        clippy::too_many_arguments,
        reason = "the one place the opline's parts meet"
    )]
    fn write(
        &mut self,
        idx: OpId,
        frame: Frame,
        op: &Opline,
        data_line: Option<&Opline>,
        seen: Seen,
        sample1: Option<Sample>,
        form: Form,
        fused: bool,
    ) -> Written {
        let code = self.meta(frame).code;
        let [t1, t2, td] = seen.inferred;
        let writes = writes(op, data_line);
        let slot = slot_of(frame, op.out_type, op.out);
        let pushes = form == Form::Inline && is_call(op.opcode);
        // A native's call returns what its declaration says, as php's
        // trace TSSA types a `DO_ICALL`'s result by the `func` its record
        // names (`Z/ext/opcache/jit/zend_jit_trace.c:2626-2650`).
        // Only a `DO_ICALL`'s callee is fixed where the opline is
        // compiled: a call by name, a method's or a dynamic one may serve
        // another function than the one recorded, which php's trace holds
        // with a guard on the callee this tier does not take.
        let returns = (op.opcode == Opcode::DoIcall)
            .then(|| self.calls.last().and_then(|call| call.returns))
            .flatten();
        let out = match slot {
            Some(slot) if !pushes && !fused => {
                let types = out_types(op, t1, t2, td, code);
                let types = returns.map_or(types, |returns| types.and(returns));
                Some(self.define(slot, types, idx, form))
            }
            _ => None,
        };
        let op1 = match slot_of(frame, op.op1_type, op.op1) {
            Some(slot) if writes.op1 => {
                let types = op1_types(op, t1, t2, sample1, code);
                // A write through a reference leaves the slot one.
                let types = if seen.referenced[0] {
                    types.or(Types::REF)
                } else {
                    types
                };
                Some(self.define(slot, types, idx, form))
            }
            _ => None,
        };
        let op2 = match slot_of(frame, op.op2_type, op.op2) {
            Some(slot) if writes.op2 => Some(self.define(slot, Types::UNKNOWN, idx, form)),
            _ => None,
        };
        if let (true, Some(data)) = (writes.data, data_line)
            && let Some(slot) = slot_of(frame, data.op1_type, data.op1)
        {
            self.define(slot, Types::UNKNOWN, idx, form);
        }
        if op.opcode == Opcode::Return {
            self.returned = if op.op1_type == OperandKind::Unused {
                Types::NULL
            } else {
                assigned(t1)
            };
        }
        Written {
            slot,
            out,
            op1,
            op2,
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
            (None, true, Some(_)) => match self.ops.get(self.header.index()) {
                Some(Op::Line(line)) => {
                    Some((self.frames[usize::from(line.frame.0)].func, line.opline))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// The direction guard of a conditional jump, leaving where the jump
    /// goes the way it did not go.
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
        let exit = match form {
            Form::Inline => draft(Point::Before(idx), frame, to),
            Form::Step => draft(Point::After(idx), frame, to),
        };
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
    /// whose destructor may, an inline fetch from a temporary array,
    /// which frees it once its element is read, and an inline element
    /// write, which its handler serves where it cannot write in place.
    fn own_exit(
        &mut self,
        idx: OpId,
        frame: Frame,
        op: &Opline,
        (form, in_place): (Form, Option<&InPlace>),
        old: Types,
    ) {
        let Some(Op::Line(line)) = self.ops.get(idx.index()) else {
            return;
        };
        // What compiled code serves in place takes no exit of its own; a
        // kernel it falls back to that throws unwinds.
        let (at, op1) = (line.opline, line.op1);
        let may_leave = inline_may_leave(line) && in_place.is_none();
        if form == Form::Inline && may_leave {
            let unfinished = draft(Point::Before(idx), frame, At::Opline(at));
            // An element a fetch does not find is the interpreter's to
            // warn of, through php's `ZEND_JIT_EXIT_TO_VM`
            // (`zend_jit_fetch_dimension_address_inner`).
            let grows = if op.opcode == Opcode::FetchDimR {
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
                    || (op.opcode == Opcode::FetchDimR && self.is_temp(op1))
                    || op.opcode == Opcode::AssignDim
                    || matches!(
                        op.opcode,
                        Opcode::DoUcall | Opcode::DoFcall | Opcode::DoFcallByName
                    )
                    // The leave kernel a variable that owns something
                    // leaves through frees it, and a destructor may throw.
                    || (matches!(in_place, Some(InPlace::Leave)) && !self.uncounted(frame))
            }
            Form::Step => op.opcode.effects().contains(Effects::THROW),
        };
        if throws {
            let unwind = draft(Point::After(idx), frame, At::Opline(at));
            self.leaves.push((idx, Leaves::Thrown, unwind));
        }
    }

    /// A call an `INIT_*` began, here or before the trace reached the
    /// frame, whose arguments sent before it the trace knows nothing of.
    fn init_call(&mut self, callee: Callee, began: Began) -> Result<(), Abort> {
        let frame = self.running()?;
        let inline = began == Began::Here
            && matches!(
                self.ops.last(),
                Some(Op::Line(line)) if line.form == Form::Inline
            );
        let plain = self
            .pushing
            .take()
            .filter(|plain| began == Began::Here && callee.user() == Some(plain.callee()));
        self.begin();
        self.calls.push(Call {
            inline,
            args: Vec::new(),
            returns: match callee {
                Callee::Native(returns) => Some(returns),
                Callee::User(_) => None,
            },
            callee,
            plain: plain.is_some(),
        });
        self.ops.push(Op::InitCall {
            frame,
            callee,
            began,
            plain,
        });
        Ok(())
    }

    /// What compiled code serves of the inline opline `op` of `frame` in
    /// place, `next` being the record after it: the push of an `INIT_*`
    /// whose callee the trace recorded where php's needs no guard, an
    /// `INIT_FCALL` of a user function of the engine's arena or a call of
    /// the frame's own op array, php's recursive call
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
                    callee: Callee::User(callee),
                    began: Began::Here,
                }) = next
                else {
                    return None;
                };
                let known = op.opcode == Opcode::InitFcall || *callee == func;
                let plain = self.view.call(*callee, op.extended_value)?;
                known.then(|| InPlace::Push(plain.clone()))
            }
            Opcode::SendValEx if op.op2_type == OperandKind::Unused => {
                let callee = self.calls.last()?.callee.user()?;
                let params = self.view.parameters(callee)?;
                sent_by_value(params, op.op2).then_some(InPlace::Send)
            }
            Opcode::DoUcall | Opcode::DoFcall | Opcode::DoFcallByName => {
                let made = matches!(next, Some(Event::Enter { .. }))
                    && self.calls.last().is_some_and(|call| call.plain);
                made.then_some(InPlace::Make)
            }
            Opcode::Return => self.leaves_in_place(frame).then_some(InPlace::Leave),
            _ => None,
        }
    }

    /// What a send wrote into the call under way, where it sends a value.
    fn record_send(&mut self, frame: Frame, op: &Opline, op1: Operand) {
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
        let frame = self.frame(func, Place::Callee(caller), self.call_at)?;
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
        self.ops.push(Op::Enter { frame });
        Ok(())
    }

    /// A return: back into the frame that called, or, past the trace's
    /// lowest frame, into a caller whose call the guard holds: where the
    /// trace goes on there, the opline recorded next, or, past the last,
    /// the header its recursive return goes back to or the opline it
    /// links at, as php's `zend_jit_leave_func` holds the instruction
    /// pointer to the next record's opline
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:11188-11224`).
    fn back(&mut self, next: Option<&Event>) -> Result<(), Abort> {
        let idx = self.begin();
        let from = self.running()?;
        self.stack.pop();
        let next_op = match next {
            Some(Event::Op { func, opline, .. }) => Some((*func, *opline)),
            Some(_) => None,
            None => match (self.repeat, self.ops.get(self.header.index())) {
                (Some(Repeat::Return), Some(Op::Line(line))) => {
                    Some((self.frames[usize::from(line.frame.0)].func, line.opline))
                }
                _ => self.rec.link_at,
            },
        };
        let (caller, call_at, guard) = if let Some(&caller) = self.stack.last() {
            (caller, self.meta(from).called_at, None)
        } else {
            let func = next_op.map_or(self.rec.root, |(func, _)| func);
            let caller = self.frame(func, Place::Caller(from), None)?;
            self.stack.push(caller);
            let call_at = next_op
                .and_then(|(_, at)| at.0.checked_sub(1))
                .map(OplineIdx);
            (caller, call_at, next_op)
        };
        let code = self.meta(caller).code;
        let out = call_at
            .and_then(|at| code.oplines.get(at.0 as usize))
            .filter(|call| is_call(call.opcode))
            .and_then(|call| slot_of(caller, call.out_type, call.out))
            .map(|slot| {
                let returned = self.returned;
                self.define(slot, returned, idx, Form::Step)
            });
        self.returned = Types::NULL;
        self.ops.push(Op::Back {
            frame: caller,
            out,
            escapes: guard,
        });
        Ok(())
    }

    /// The clobber rule (`jit-design.md` §4.2): after a call-effect
    /// kernel, every value of a live frame that a symbol table reaches or
    /// that may be a reference is read again.
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
        }
    }

    fn value(&mut self, slot: Slot, types: Types, def: Def, home: Home) -> ValueId {
        let id = ValueId(u32::try_from(self.values.len()).unwrap_or(u32::MAX));
        self.born.push(types);
        self.values.push(Value {
            slot,
            types,
            def,
            home,
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
    fn define(&mut self, slot: Slot, types: Types, idx: OpId, form: Form) -> ValueId {
        let aliased = self.aliased(slot);
        let types = if aliased { Types::UNKNOWN } else { types };
        let home = if form == Form::Inline && !aliased && types.scalar() {
            Home::Register
        } else {
            Home::Memory
        };
        let value = self.value(slot, types, Def::Op(idx), home);
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
        self.set(slot, value);
        value
    }

    /// What the op array alone implies `slot` holds where the frame's
    /// part of the trace begins, php's initial `var_info` of
    /// `zend_jit_trace_build_tssa`: a parameter its declaration, which an
    /// entry has checked, and which no later write can have changed where
    /// nothing else writes it; a variable of a frame the trace pushed is
    /// unset; anything else is anything.
    fn entry_types(&self, slot: Slot) -> Types {
        let meta = self.meta(slot.frame);
        // A side trace enters with what its parent's exit knew of the
        // frame it starts in, as php's side reads its parent's stack map;
        // a slot a symbol table reaches is guarded at every read still.
        let known = (self.rec.entered.iter())
            .find(|entered| slot.frame == Frame::ROOT && entered.var == slot.var)
            .filter(|_| !self.aliased(slot));
        if let Some(known) = known {
            return Types::of_tag(known.tag);
        }
        let cv = slot.var < meta.code.num_cvs;
        if !cv {
            return Types::UNKNOWN.without(Types::UNDEF);
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
        match self.frames[usize::from(slot.frame.0)].place {
            Place::Callee(_) => Types::UNDEF,
            Place::Root if self.rec.start == Start::Enter => {
                param.map_or(Types::UNDEF, |p| param_types(p.declared, None))
            }
            _ => match param {
                Some(p) if !written_after_entry(meta.code, slot.var) => {
                    param_types(p.declared, None)
                }
                _ => unknown,
            },
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
        let t1 = self.types_of(frame, operands[0]);
        let mut held: [Option<(Types, bool)>; 3] = [None; 3];
        let sites = guard_sites(op, data_line, samples, t1, self.meta(frame).code);
        for site in &sites {
            let at = *site as usize;
            if let Some(guarded) = self.guard_use(idx, here, operands[at], samples[at]) {
                held[at] = Some(guarded);
            }
        }
        for site in check_sites(op, samples) {
            let at = site as usize;
            if !sites.contains(&site) {
                self.check_use(idx, here, operands[at], samples[at]);
            }
        }
        let mut seen = Seen {
            types: [Types::NONE; 3],
            referenced: [false; 3],
            inferred: [Types::NONE; 3],
        };
        for (at, operand) in operands.into_iter().enumerate() {
            let types = self.types_of(frame, operand);
            (seen.types[at], seen.referenced[at]) = held[at].unwrap_or((types, false));
            seen.inferred[at] = match operand {
                Operand::Value(value) if self.aliased(self.val(value).slot) => types,
                _ => seen.types[at],
            };
        }
        if let Some(layout) = packed_guard(op, samples, self.meta(frame).code)
            && let Operand::Value(value) = operands[0]
        {
            let types = self.val(value).types;
            let slot = self.val(value).slot;
            if types.either_layout() && !self.aliased(slot) {
                self.val_mut(value).types = types.laid_out(layout);
                seen.types[0] = types.laid_out(layout);
                seen.inferred[0] = types.laid_out(layout);
                self.place(idx, here, value, Check::Layout { value, layout }, None);
            } else if self.aliased(slot)
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
                    check: Check::Layout { value, layout },
                    exit: draft(Point::Before(idx), slot.frame, At::Opline(here)),
                });
            }
        }
        seen
    }

    /// php's `ADD_OP_GUARD` on one operand, `zend_jit_trace_add_op_guard`:
    /// a type guard where the recorded type is narrower than the value's,
    /// narrowing the value unless a symbol table reaches it or a register
    /// holds it; a reference's inner type at every read, which php's code
    /// generator holds where its TSSA adds nothing. What the opline sees
    /// of the operand after it, where a guard is held, and whether it sees
    /// it through a reference, which a write to the slot writes through.
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
    fn guard_use(
        &mut self,
        idx: OpId,
        here: OplineIdx,
        operand: Operand,
        sample: Option<Sample>,
    ) -> Option<(Types, bool)> {
        let (Operand::Value(value), Some(sample)) = (operand, sample) else {
            return None;
        };
        if sample.indirect {
            return None;
        }
        let slot = self.val(value).slot;
        let tag = sample.kind;
        let before = Placed {
            at: GuardAt::Before(idx),
            check: Check::Type { value, tag },
            exit: draft(Point::Before(idx), slot.frame, At::Opline(here)),
        };
        if sample.reference {
            if slot.var >= self.meta(slot.frame).code.num_cvs {
                return None;
            }
            self.guards.push(Placed {
                check: Check::Deref { value, tag },
                ..before
            });
            return Some((Types::of_tag(tag), true));
        }
        let held = *self.val(value);
        let types = held.types;
        if types.tag() == Some(tag) {
            return None;
        }
        if self.aliased(slot) || held.home == Home::Register {
            self.guards.push(before);
            return Some((types.narrowed(tag), false));
        }
        self.val_mut(value).types = types.narrowed(tag);
        self.place(idx, here, value, Check::Type { value, tag }, Some(tag));
        Some((types.narrowed(tag), false))
    }

    /// php's code generator's guard on an operand its inference does not
    /// guard at this opline (`CHECK_OP_TRACE_TYPE` over an operand marked
    /// `MAY_BE_GUARD` elsewhere): only on a value a symbol table reaches,
    /// whose mark no guard clears, held before the opline where the
    /// recording saw a type, and kept once a guard of php's inference
    /// holds the same value ([`Builder::checks`]). It narrows nothing the
    /// opline sees: the oplines it is held at are stepped, or read the
    /// operand as the inference's guard held it.
    fn check_use(&mut self, idx: OpId, here: OplineIdx, operand: Operand, sample: Option<Sample>) {
        let (Operand::Value(value), Some(sample)) = (operand, sample) else {
            return;
        };
        let slot = self.val(value).slot;
        if sample.indirect || sample.reference || !self.aliased(slot) {
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
    /// php's two reads of it each hold it, and an arithmetic only where
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
                Some(Op::Line(line)) if line.form == Form::Inline => within(line.opcode, tag)
                    .filter(|kind| {
                        *kind == Within::Fetched || !self.over_aliased(line.op1, line.op2)
                    })
                    .map(|kind| (def, line.frame, line.opline, kind, line.op1, line.op2)),
                _ => None,
            },
            _ => None,
        };
        let mut then = None;
        let (at, exit) = match (held.def, computed) {
            (Def::Entry, _)
                if held.slot.frame == Frame::ROOT
                    && matches!(self.repeat, Some(Repeat::Loop | Repeat::Call)) =>
            {
                (GuardAt::Entry, self.start())
            }
            (_, Some((def, frame, opline, kind, op1, op2))) => {
                let fetched = |read| At::Fetched {
                    fetch: opline,
                    op1: self.is_temp(op1),
                    op2: self.is_temp(op2),
                    out: value,
                    read,
                };
                let to = match kind {
                    Within::Rerun => At::Opline(opline),
                    Within::Fetched => fetched(Read::InPlace),
                };
                if let (Within::Fetched, Some(tag)) = (kind, tag) {
                    then = Some(Placed {
                        at: GuardAt::Within(def),
                        check: Check::Deref { value, tag },
                        exit: draft(Point::Before(def), frame, fetched(Read::Through)),
                    });
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
            extra: None,
            grows: Grows::Side,
            link: None,
        }
    }

    // ------------------------------------------------------------ forms

    /// How an opline is compiled: inline where it is in the scalar core
    /// and its operands' types are the ones the inline code serves,
    /// through `step` otherwise.
    fn form(&self, frame: Frame, op: &Opline, seen: Seen, next: Option<&Event>) -> Form {
        let [t1, t2, td] = seen.types;
        // An opline that writes its variable `op1` writes through what it
        // sees through a reference, which only its handler does.
        let cv1 = op.op1_type == OperandKind::Cv && !seen.referenced[0];
        let number = |t: Types| t == Types::LONG || t == Types::DOUBLE;
        let long = |t: Types| t == Types::LONG;
        let scalar = |t: Types| t.scalar();
        let packed = Types::ARRAY.laid_out(Layout::Packed);
        let inline = match op.opcode {
            Opcode::Jmp => true,
            // A double's truth is the handler's, which warns of a NAN as
            // php 8.5 coerces it (`ZEND_JMPZ`, `Z/Zend/zend_vm_def.h`).
            Opcode::Jmpz | Opcode::Jmpnz | Opcode::JmpzEx | Opcode::JmpnzEx => {
                scalar(t1) && t1 != Types::DOUBLE
            }
            Opcode::QmAssign => scalar(t1),
            Opcode::IsSmaller | Opcode::IsSmallerOrEqual | Opcode::IsEqual | Opcode::IsNotEqual => {
                number(t1) && number(t2)
            }
            Opcode::IsIdentical | Opcode::IsNotIdentical => scalar(t1) && scalar(t2),
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
            Opcode::Assign => cv1 && scalar(t2) && !t1.meets(Types::REF),
            Opcode::PreInc | Opcode::PreDec | Opcode::PostInc | Opcode::PostDec => {
                cv1 && number(t1)
            }
            Opcode::FetchDimR => op.op1_type != OperandKind::Const && t1 == packed && long(t2),
            // php compiles the write inline over an array that may be
            // packed, testing its layout where it may be either, and adds
            // no layout guard where the array was recorded packed
            // (`Z/ext/opcache/jit/zend_jit_trace.c:1871-1893`).
            Opcode::AssignDim => {
                cv1 && t1.tag() == Some(ZvalType::Array)
                    && t1.meets(Types::PACKED)
                    && long(t2)
                    && scalar(td)
            }
            Opcode::SendVal | Opcode::SendVar | Opcode::SendValEx => {
                op.op2_type == OperandKind::Unused && scalar(t1)
            }
            Opcode::InitFcall | Opcode::InitFcallByName | Opcode::InitNsFcallByName => {
                matches!(
                    next,
                    Some(Event::InitCall {
                        callee: Callee::User(_),
                        began: Began::Here,
                    })
                )
            }
            Opcode::DoUcall | Opcode::DoFcall | Opcode::DoFcallByName => {
                matches!(next, Some(Event::Enter { .. }))
            }
            Opcode::Return => {
                (matches!(next, Some(Event::Back)) || self.returns_at_end)
                    && (scalar(t1) || op.op1_type == OperandKind::Unused)
                    && (self.uncounted(frame) || self.leaves_in_place(frame))
            }
            Opcode::VerifyReturnType => {
                let declared = Types::of_mask(self.meta(frame).code.return_mask)
                    .without(Types::OBJECT)
                    .without(Types::REF);
                // Seen through a reference, the slot keeps the reference,
                // which the inline form would write over with its value.
                op.op1_type != OperandKind::Unused
                    && !seen.referenced[0]
                    && t1.kinds() != Types::NONE
                    && t1.within(declared)
            }
            _ => false,
        };
        if inline { Form::Inline } else { Form::Step }
    }

    /// Whether a `RETURN` of `frame` leaves it in place: a function's
    /// frame, with no observer declared, whose variables the leave tests
    /// as it leaves where they may own something ([`Imm::Leave`]).
    fn leaves_in_place(&self, frame: Frame) -> bool {
        let func = self.frames[usize::from(frame.0)].func;
        self.view.is_main(func) == Some(false) && !self.view.observed()
    }

    /// Whether every variable of `frame` holds what no reference counts,
    /// so a return frees nothing that runs a destructor.
    fn uncounted(&self, frame: Frame) -> bool {
        let cvs = self.meta(frame).code.num_cvs;
        (0..cvs).all(|var| {
            let slot = Slot { frame, var };
            let types = match self.slots[usize::from(frame.0)].get(var as usize) {
                Some(Some(value)) => self.val(*value).types,
                _ => self.entry_types(slot),
            };
            types.within(UNCOUNTED)
        })
    }

    /// The runtime cache slot an inline call's `INIT_*` reads its callee
    /// from, `result.num`.
    fn map_ptr(&mut self, func: FuncRef, op: &Opline, form: Form) -> Option<usize> {
        if form != Form::Inline
            || !matches!(
                op.opcode,
                Opcode::InitFcall | Opcode::InitFcallByName | Opcode::InitNsFcallByName
            )
        {
            return None;
        }
        let raw = self.view.cache_slot(func, CacheSlot(op.out))?;
        if let Some(at) = self.map_ptrs.iter().position(|held| *held == raw) {
            return Some(at);
        }
        self.map_ptrs.push(raw);
        Some(self.map_ptrs.len() - 1)
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
            extra: None,
            grows: Grows::Side,
            link,
        });
        let final_snapshot = self.snapshot();
        self.snapshots.push(final_snapshot);
        Ok(())
    }

    fn finish(mut self) -> Result<TraceIr, Abort> {
        let marked: HashSet<ValueId> = (self.guards.iter())
            .filter_map(|placed| match placed.check {
                Check::Type { value, .. } => Some(value),
                _ => None,
            })
            .collect();
        let checks = std::mem::take(&mut self.checks);
        self.guards.extend(checks.into_iter().filter(
            |placed| matches!(placed.check, Check::Type { value, .. } if marked.contains(&value)),
        ));
        let repeat = self.repeat;
        if repeat.is_some() {
            self.close_phis()?;
        }
        let closes = match repeat {
            None => Closes::Leave,
            Some(repeat) => Closes::Loop {
                repeat,
                stub: self
                    .stubbed(repeat)
                    .then(|| self.at_header_opline())
                    .flatten(),
            },
        };
        self.load_at_entry();
        let numbered = self.number(closes)?;
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

    /// The exits numbered in the order the trace meets them: the guards
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
        let (order, stubbed) = self.order(closes);
        let mut exits = Exits::default();
        let mut unwinds: Vec<Unwind> = Vec::new();
        let mut known: HashMap<ValueId, ZvalType> = HashMap::new();
        let mut numbered = Numbered {
            exits: Vec::new(),
            unwinds: Vec::new(),
            guards: Vec::with_capacity(self.guards.len()),
            end: stubbed,
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
            let mut spec = self.spec(&draft, &known);
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
                    let held = match placed.check {
                        Check::Type { value, tag } => Some((value, tag)),
                        Check::Layout { value, .. } => Some((value, ZvalType::Array)),
                        Check::Deref { .. }
                        | Check::Direction { .. }
                        | Check::Stands { .. }
                        | Check::Arm { .. }
                        | Check::Room { .. } => None,
                    };
                    if let Some((value, tag)) = held
                        && !self.aliased(self.val(value).slot)
                    {
                        known.insert(value, tag);
                    }
                }
                Source::Leave(at) => {
                    let (op, leaves, _) = self.leaves[at];
                    let leave = match leaves {
                        Leaves::Unfinished => Leave::Exit(exits.take(spec, level.clone())?),
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
                    numbered.end = Some(End::Loop(LoopHeader {
                        at: self.header,
                        repeat,
                        interrupt: Interrupt::Exit(exits.take(spec, level.clone())?),
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
    /// and the end of a loop whose interrupt test takes php's stub, which
    /// takes none.
    fn order(&self, closes: Closes) -> (Vec<(Order, Source)>, Option<End>) {
        let mut order: Vec<(Order, Source)> = Vec::new();
        for (at, guard) in self.guards.iter().enumerate() {
            let key = match (guard.at, guard.check) {
                (GuardAt::Entry, check) => Order::Entry(self.guarded_slot(check)),
                (GuardAt::Before(op), Check::Layout { .. }) => Order::At(op, Stage::Inside),
                (GuardAt::Before(op), Check::Room { .. }) => Order::At(op, Stage::Pushes),
                (GuardAt::Before(op), _) => Order::At(op, Stage::Before),
                (GuardAt::Within(op), _) => Order::At(op, Stage::Within),
            };
            order.push((key, Source::Guard(at)));
        }
        for (at, (op, leaves, _)) in self.leaves.iter().enumerate() {
            let opens = matches!(leaves, Leaves::Unfinished)
                && matches!(
                    self.ops.get(op.index()),
                    Some(Op::Line(line)) if line.opcode == Opcode::FetchDimR
                );
            let stage = if opens { Stage::Opens } else { Stage::Leaves };
            order.push((Order::At(*op, stage), Source::Leave(at)));
        }
        let mut stubbed = None;
        match closes {
            Closes::Loop { repeat, stub: None } => {
                order.push((Order::End, Source::Interrupt(repeat)));
            }
            Closes::Loop {
                repeat,
                stub: Some(at),
            } => {
                stubbed = Some(End::Loop(LoopHeader {
                    at: self.header,
                    repeat,
                    interrupt: Interrupt::Stub(at),
                }));
            }
            Closes::Leave => order.push((Order::End, Source::End)),
        }
        // Stable: within one place, in the order they were placed.
        order.sort_by_key(|(key, _)| *key);
        (order, stubbed)
    }

    /// The slot a guard holds a value of; none for a guard on no value.
    fn guarded_slot(&self, check: Check) -> Option<Slot> {
        match check {
            Check::Type { value, .. }
            | Check::Deref { value, .. }
            | Check::Layout { value, .. } => Some(self.val(value).slot),
            Check::Direction { .. }
            | Check::Stands { .. }
            | Check::Arm { .. }
            | Check::Room { .. } => None,
        }
    }

    /// Whether a loop's interrupt test takes php's interrupt stub and no
    /// exit (`Z/ext/opcache/jit/zend_jit_trace.c:7210-7228`): in a root
    /// trace that does not loop by returning, whose code keeps the
    /// instruction pointer it entered with, php's
    /// `ZEND_JIT_TRACE_USES_INITIAL_IP`, and holds nothing in registers
    /// where the test leaves, so no deoptimisation is needed
    /// (`zend_jit_trace_stack_needs_deoptimization`).
    fn stubbed(&self, repeat: Repeat) -> bool {
        let root = self.rec.start.grew().is_none();
        root && repeat != Repeat::Return
            && self.uses_initial_ip()
            && self
                .spec(&self.interrupt(), &HashMap::new())
                .stores
                .is_empty()
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
                at: GuardAt::Before(_),
                check,
                ..
            } = self.guards[at]
            else {
                continue;
            };
            let (value, narrowed) = match check {
                Check::Type { value, tag } => (value, Narrowed::Tag(tag)),
                Check::Layout { value, layout } => (value, Narrowed::Layout(layout)),
                _ => continue,
            };
            let phi = *self.val(value);
            let Def::Phi { entry, back } = phi.def else {
                continue;
            };
            let entered = *self.val(entry);
            if entered.def != Def::Entry
                || entered.slot.frame != Frame::ROOT
                || self.aliased(phi.slot)
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
                self.guards.push(held);
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
                    Check::Layout { layout, .. } => Check::Layout {
                        value: entry,
                        layout,
                    },
                    other => other,
                },
                exit: self.start(),
            };
        }
        for at in 0..self.phis.len() {
            let phi = self.phis[at];
            let Value {
                slot, types, def, ..
            } = *self.val(phi);
            let Def::Phi { back, .. } = def else { continue };
            if !self.aliased(slot) && types.scalar() && self.val(back).home == Home::Register {
                self.val_mut(phi).home = Home::Register;
            }
        }
        Ok(())
    }

    /// php's `is_checked_guard` (`Z/ext/opcache/jit/zend_jit_trace.c:1182-1260`)
    /// over a loop phi guarded long and `back`, what its back edge brings:
    /// a value no reference holds that an inline `ADD`, `SUB` or `MUL`, a
    /// `+=`, `-=` or `*=`, or an increment or a decrement made over
    /// numbers, which holds its overflow itself, so the guard the phi
    /// held at the header each turn is held within it, rerunning it where
    /// it overflows. The guard it holds, where it is one.
    fn checked_guard(&self, back: ValueId) -> Option<Placed> {
        let made = *self.val(back);
        if made.types.meets(Types::REF) || !made.types.meets(Types::LONG) {
            return None;
        }
        let Def::Op(def) = made.def else { return None };
        let Some(Op::Line(line)) = self.ops.get(def.index()) else {
            return None;
        };
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
        let inline = line.form == Form::Inline
            && within(line.opcode, ZvalType::Long) == Some(Within::Rerun)
            && !self.over_aliased(line.op1, line.op2);
        (checked && inline).then(|| Placed {
            at: GuardAt::Within(def),
            check: Check::Type {
                value: back,
                tag: ZvalType::Long,
            },
            exit: draft(Point::Before(def), line.frame, At::Opline(line.opline)),
        })
    }

    /// One exit's spec from its draft and the state it leaves, the values
    /// in `known` held by a guard before it.
    fn spec(&self, draft: &Draft, known: &HashMap<ValueId, ZvalType>) -> ExitSpec {
        let empty = Snapshot::default();
        let snapshot = self.snapshot_at(draft.point).unwrap_or(&empty);
        let calls = snapshot.calls > 0;
        let mut extra = draft.extra;
        let (opline, landing) = match draft.at {
            At::Opline(at) => (Some(at), restored(calls)),
            At::Stands => (None, restored(calls)),
            At::Fetched {
                fetch,
                op1,
                op2,
                out,
                read,
            } => {
                if calls {
                    (Some(fetch), Landing::RestoreCall)
                } else {
                    extra = Some(Store {
                        slot: self.val(out).slot,
                        from: Stored::Copied { value: out, read },
                    });
                    let landing = if op1 || op2 {
                        Landing::FreeTemps { op1, op2 }
                    } else {
                        Landing::Plain
                    };
                    (Some(OplineIdx(fetch.0 + 1)), landing)
                }
            }
        };
        let mut stores: Vec<Store> = snapshot
            .values
            .iter()
            .filter(|(_, value)| self.val(*value).home == Home::Register)
            .map(|(slot, value)| Store {
                slot: *slot,
                from: Stored::Value(*value),
            })
            .collect();
        if let Some(extra) = extra {
            stores.retain(|store| store.slot != extra.slot);
            stores.push(extra);
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
        let read: HashSet<Slot> = snapshot.values.iter().map(|(slot, _)| *slot).collect();
        let entered = (known.keys())
            .filter(|value| self.val(**value).def == Def::Entry)
            .map(|value| (self.val(*value).slot, *value))
            .filter(|(slot, _)| !read.contains(slot));
        // Past an op, a slot it wrote is known as it was before it, as
        // php's code generator sets the type of what an op writes only
        // once the op's code, and the exits it takes, are done
        // (`zend_jit_trace`'s `SET_STACK_TYPE` after each op).
        let before = match draft.point {
            Point::After(at) => self.snapshots.get(at.index()),
            _ => None,
        };
        let as_before = |(slot, value): (Slot, ValueId)| match (before, self.val(value).def) {
            (Some(before), Def::Op(def)) if Point::After(def) == draft.point => (before.values)
                .iter()
                .find(|(held, _)| *held == slot)
                .copied(),
            _ => Some((slot, value)),
        };
        let mut known: Vec<Known> = (snapshot.values.iter().copied())
            .filter_map(as_before)
            .chain(entered)
            .filter(|(slot, _)| slot.frame == draft.frame)
            .filter_map(|(slot, value)| {
                Some(Known {
                    slot,
                    tag: held(value)?,
                })
            })
            .collect();
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

/// What `op` names beside its operands that its inline form reads.
fn imm(op: &Opline) -> Imm {
    match op.opcode {
        Opcode::SendVal | Opcode::SendVar if op.op2_type == OperandKind::Unused => Imm::Arg(op.op2),
        Opcode::AssignOp => lattice::binary_of(op.extended_value).map_or(Imm::None, Imm::Binary),
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
        extra: None,
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
/// one leave from one frame.
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
        if self.specs.len() == MAX_EXITS {
            return Err(Abort {
                stop: Stop::TooManyExits,
                why: "more exits than php's ZEND_JIT_TRACE_MAX_EXITS",
            });
        }
        let id = ExitId(u16::try_from(self.specs.len()).map_err(|_| mismatch("too many exits"))?);
        if let Some(key) = key {
            self.taken.insert(key, id);
        }
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

/// How a trace ends, for its numbering: a loop, its interrupt test
/// taking an exit or, where `stub` names the header's opline, php's
/// stub; or a trace that does not loop, through the exit it ends by.
#[derive(Clone, Copy)]
enum Closes {
    Loop {
        repeat: Repeat,
        stub: Option<OplineIdx>,
    },
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
/// exits: the guards before it on its operands' types; the exit an
/// inline fetch opens its code with, php's `ZEND_JIT_EXIT_TO_VM` that
/// `zend_jit_fetch_dim_read` takes first (`Z/ext/opcache/jit/zend_jit_ir.c:12540-12547`);
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

fn restored(calls: bool) -> Landing {
    if calls {
        Landing::RestoreCall
    } else {
        Landing::Plain
    }
}
