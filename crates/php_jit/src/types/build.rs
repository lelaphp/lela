//! The walk that builds a [`TraceIr`] (`jit-design.md` §4.2): one pass
//! over the record's events in order, php's `zend_jit_trace_build_tssa`
//! with its type inference and guard placement folded into the one
//! loop, then the exits numbered and laid out from the state the trace
//! held where each leaves.

use std::collections::HashMap;

use php_engine::tier::{FuncRef, MapPtrRaw, Sample, UnitView};
use php_heap::ZvalType;
use php_unit::decl::Parameter;
use php_unit::{CacheSlot, Effects, OpArray, Opcode, OperandKind, Opline, OplineIdx};

use super::lattice::{self, Types};
use super::opcodes::{
    Declared, Within, arith, assigned, constant, constant_types, guard_sites, inline_may_leave,
    is_call, keeps_temporaries, op1_types, out_types, packed_guard, param_types, params, reads,
    reads_by_name, referenced, slot_of, within, writes, written, written_after_entry,
};
use super::{
    Abort, Check, Def, End, Entry, ExitId, ExitSpec, Form, Frame, FrameInfo, Fused, Guard, GuardAt,
    Home, Imm, Landing, Layout, Line, LoopHeader, Op, OpId, Operand, Place, Repeat, Slot, Store,
    Stored, TraceIr, Value, ValueId,
};
use crate::record::{self, Event, Start, Stop, TraceRecord};
use crate::table::TraceId;

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
    /// it with the element stored and its temporaries freed, or, while a
    /// call is under way, the fetch again.
    Fetched {
        fetch: OplineIdx,
        op1: bool,
        op2: bool,
        out: ValueId,
    },
}

/// An exit before it is numbered.
#[derive(Clone, Copy)]
struct Draft {
    point: Point,
    frame: Frame,
    at: At,
    extra: Option<Store>,
    link: Option<TraceId>,
}

/// What an opline sees of its three operands once its guards held, and
/// which it sees through a reference.
#[derive(Clone, Copy)]
struct Seen {
    types: [Types; 3],
    referenced: [bool; 3],
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
    args: Vec<Option<Types>>,
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
    ops: Vec<Op>,
    snapshots: Vec<Snapshot>,
    guards: Vec<Placed>,
    leaves: Vec<(OpId, Leaves, Draft)>,
    calls: Vec<Call>,
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
    /// What the trace repeats, where its stop makes it loop.
    repeat: Option<Repeat>,
}

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
            ops: Vec::new(),
            snapshots: Vec::new(),
            guards: Vec::new(),
            leaves: Vec::new(),
            calls: Vec::new(),
            map_ptrs: Vec::new(),
            phis: Vec::new(),
            returned: Types::NULL,
            call_at: None,
            successor: None,
            header: OpId(0),
            end_draft: None,
            repeat: match rec.stop {
                Stop::Loop => Some(Repeat::Loop),
                Stop::RecursiveCall => Some(Repeat::Call),
                Stop::RecursiveRet => Some(Repeat::Return),
                _ => None,
            },
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
            Some(Repeat::Return) | None => {}
        }
        for (at, event) in body.iter().enumerate() {
            let next = events.get(at + 1);
            match event {
                Event::Op {
                    func,
                    opline,
                    op1,
                    op2,
                    data,
                    branch,
                } => {
                    let root_return = at == 0 && rec.start == Start::Return;
                    self.line(
                        *func,
                        *opline,
                        [*op1, *op2, *data],
                        *branch,
                        next,
                        root_return,
                    )?;
                    if root_return {
                        self.back(next)?;
                        if self.repeat == Some(Repeat::Return) {
                            self.ret_phi();
                        }
                    }
                }
                Event::InitCall { target } => self.init_call(*target)?,
                Event::Enter { func } => self.enter(*func)?,
                Event::Back => self.back(next)?,
            }
        }
        self.end(events.get(body.len()))
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

    /// The phi of a recursive return: the slot the call the root returned
    /// to writes, php's `zend_jit_trace_add_ret_phis`.
    fn ret_phi(&mut self) {
        self.header = OpId(u32::try_from(self.ops.len()).unwrap_or(u32::MAX));
        if let Some(Op::Back { out: Some(out), .. }) = self.ops.last() {
            let slot = self.val(*out).slot;
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
    fn line(
        &mut self,
        func: FuncRef,
        at: OplineIdx,
        samples: [Option<Sample>; 3],
        branch: Option<bool>,
        next: Option<&Event>,
        root_return: bool,
    ) -> Result<(), Abort> {
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
        let form = self.form(frame, &op, seen, next, root_return);
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
            imm: imm(&op),
            form,
            map_ptr,
            exit: None,
            freed: None,
        }));
        let fused = fused.is_some();
        let len = if fused || data_line.is_some() { 2 } else { 1 };
        let jump = if fused { after } else { Some(op) };
        let target = jump.and_then(|jump| jump.jump_target());
        self.successor = went(at, &op, target, len, branch);
        self.own_exit(idx, frame, &op, form, t1);
        if let Some(taken) = branch {
            let jump = if fused { after } else { None };
            self.direction(
                idx,
                frame,
                at,
                op1,
                jump,
                taken,
                form,
                len,
                target,
                written.slot,
            );
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
        let [t1, t2, td] = seen.types;
        let writes = writes(op, data_line);
        let slot = slot_of(frame, op.out_type, op.out);
        let pushes = form == Form::Inline && is_call(op.opcode);
        let out = match slot {
            Some(slot) if !pushes && !fused => {
                Some(self.define(slot, out_types(op, t1, t2, td, code), idx, form))
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

    /// The direction guard of a conditional jump.
    #[allow(
        clippy::too_many_arguments,
        reason = "the one place the opline's parts meet"
    )]
    fn direction(
        &mut self,
        idx: OpId,
        frame: Frame,
        at: OplineIdx,
        op1: Operand,
        jump: Option<Opline>,
        taken: bool,
        form: Form,
        len: u32,
        target: Option<OplineIdx>,
        out_slot: Option<Slot>,
    ) {
        if jump.is_none() && matches!(op1, Operand::Constant(_)) {
            // A constant condition goes one way; nothing to hold.
            return;
        }
        let exit = match (form, jump) {
            (Form::Inline, Some(jump)) => {
                let jumps_on_false = jump.opcode == Opcode::Jmpz;
                // The value that sends the jump the other way.
                let other = if jumps_on_false { taken } else { !taken };
                let extra = out_slot.map(|slot| Store {
                    slot,
                    from: Stored::Bool(other),
                });
                Draft {
                    point: Point::Before(idx),
                    frame,
                    at: At::Opline(OplineIdx(at.0 + 1)),
                    extra,
                    link: None,
                }
            }
            (Form::Inline, None) => draft(Point::Before(idx), frame, At::Opline(at)),
            (Form::Step, _) => {
                let other = if taken {
                    Some(OplineIdx(at.0 + len))
                } else {
                    target
                };
                let to = other.map_or(At::Stands, At::Opline);
                draft(Point::After(idx), frame, to)
            }
        };
        self.guards.push(Placed {
            at: GuardAt::Within(idx),
            check: Check::Direction { taken },
            exit,
        });
    }

    /// The exit an opline itself takes where it cannot finish: an inline
    /// one before it does anything, which the interpreter runs again, a
    /// stepped one that threw, at its opline with what it threw pending;
    /// and an inline fetch's from a temporary array, where freeing the
    /// array threw once the fetch was done.
    fn own_exit(&mut self, idx: OpId, frame: Frame, op: &Opline, form: Form, old: Types) {
        let Some(Op::Line(line)) = self.ops.get(idx.index()) else {
            return;
        };
        let (at, op1) = (line.opline, line.op1);
        let unfinished = match form {
            Form::Inline if inline_may_leave(op.opcode) => {
                draft(Point::Before(idx), frame, At::Opline(at))
            }
            // An assignment over a value a reference counts frees it, and
            // its destructor may throw once the assignment is done.
            Form::Inline if op.opcode == Opcode::Assign && !old.within(UNCOUNTED) => {
                draft(Point::After(idx), frame, At::Opline(at))
            }
            Form::Step if op.opcode.effects().contains(Effects::THROW) => {
                draft(Point::After(idx), frame, At::Opline(at))
            }
            _ => return,
        };
        self.leaves.push((idx, Leaves::Unfinished, unfinished));
        if form == Form::Inline && op.opcode == Opcode::FetchDimR && self.is_temp(op1) {
            // The temporary array the fetch read is freed once its element
            // is read, and freeing it may run a destructor that throws.
            let freed = draft(Point::After(idx), frame, At::Opline(at));
            self.leaves.push((idx, Leaves::Freed, freed));
        }
    }

    /// A call an `INIT_*` began.
    fn init_call(&mut self, target: Option<FuncRef>) -> Result<(), Abort> {
        let frame = self.running()?;
        let inline = matches!(
            self.ops.last(),
            Some(Op::Line(line)) if line.form == Form::Inline
        );
        self.begin();
        self.calls.push(Call {
            inline,
            args: Vec::new(),
        });
        self.ops.push(Op::InitCall { frame, target });
        Ok(())
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
        let Some(call) = self.calls.last_mut() else {
            return;
        };
        let at = op.op2.saturating_sub(1) as usize;
        if call.args.len() <= at {
            call.args.resize(at + 1, None);
        }
        if let Some(arg) = call.args.get_mut(at) {
            *arg = types;
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
            let sent = call
                .as_ref()
                .and_then(|call| call.args.get(param.number.saturating_sub(1) as usize))
                .copied()
                .flatten();
            let types = param_types(param.declared, sent);
            let slot = Slot {
                frame,
                var: param.cv,
            };
            let value = self.value(slot, types, Def::Op(idx), Home::Memory);
            self.set(slot, value);
        }
        self.ops.push(Op::Enter { frame });
        Ok(())
    }

    /// A return: back into the frame that called, or, past the trace's
    /// lowest frame, into a caller whose call the guard holds.
    fn back(&mut self, next: Option<&Event>) -> Result<(), Abort> {
        let idx = self.begin();
        let from = self.running()?;
        self.stack.pop();
        let next_op = match next {
            Some(Event::Op { func, opline, .. }) => Some((*func, *opline)),
            _ => None,
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
            let guard = next_op.map(|(func, opline)| Check::Caller { func, opline });
            (caller, call_at, guard)
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
        self.ops.push(Op::Back { frame: caller, out });
        if let Some(check) = guard {
            let exit = draft(Point::After(idx), caller, At::Stands);
            self.guards.push(Placed {
                at: GuardAt::Within(idx),
                check,
                exit,
            });
        }
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
        for site in guard_sites(op, data_line, samples, t1, self.meta(frame).code) {
            let at = site as usize;
            if let Some(guarded) = self.guard_use(idx, here, operands[at], samples[at]) {
                held[at] = Some(guarded);
            }
        }
        let mut seen = Seen {
            types: [Types::NONE; 3],
            referenced: [false; 3],
        };
        for (at, operand) in operands.into_iter().enumerate() {
            (seen.types[at], seen.referenced[at]) =
                held[at].unwrap_or_else(|| (self.types_of(frame, operand), false));
        }
        if let Some(layout) = packed_guard(op, samples, self.meta(frame).code)
            && let Operand::Value(value) = operands[0]
        {
            let types = self.val(value).types;
            if types.either_layout() && !self.aliased(self.val(value).slot) {
                self.val_mut(value).types = types.laid_out(layout);
                seen.types[0] = types.laid_out(layout);
                self.place(idx, here, value, Check::Layout { value, layout }, None);
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

    /// Where the guard on a value no symbol table reaches is held: once at
    /// entry for what the root's frame entered with; by the inline op that
    /// computed it for an arithmetic's overflow and a fetched element's
    /// type, as php's code generator holds a guarded value it writes;
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
                    .map(|kind| (def, line.frame, line.opline, kind, line.op1, line.op2)),
                _ => None,
            },
            _ => None,
        };
        let (at, exit) = match (held.def, computed) {
            (Def::Entry, _) if held.slot.frame == Frame::ROOT => (GuardAt::Entry, self.start()),
            (_, Some((def, frame, opline, kind, op1, op2))) => {
                let to = match kind {
                    Within::Rerun => At::Opline(opline),
                    Within::Fetched => At::Fetched {
                        fetch: opline,
                        op1: self.is_temp(op1),
                        op2: self.is_temp(op2),
                        out: value,
                    },
                };
                if !self.aliased(held.slot) && self.val(value).types.scalar() {
                    self.val_mut(value).home = Home::Register;
                }
                (GuardAt::Within(def), draft(Point::Before(def), frame, to))
            }
            _ => (
                GuardAt::Before(idx),
                draft(Point::Before(idx), held.slot.frame, At::Opline(here)),
            ),
        };
        self.guards.push(Placed { at, check, exit });
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
        op: &Opline,
        seen: Seen,
        next: Option<&Event>,
        root_return: bool,
    ) -> Form {
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
            Opcode::Jmpz | Opcode::Jmpnz | Opcode::JmpzEx | Opcode::JmpnzEx | Opcode::QmAssign => {
                scalar(t1)
            }
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
            Opcode::AssignDim => cv1 && t1 == packed && long(t2) && scalar(td),
            Opcode::SendVal | Opcode::SendVar | Opcode::SendValEx => {
                op.op2_type == OperandKind::Unused && scalar(t1)
            }
            Opcode::InitFcall | Opcode::InitFcallByName | Opcode::InitNsFcallByName => {
                matches!(next, Some(Event::InitCall { target: Some(_) }))
            }
            Opcode::DoUcall | Opcode::DoFcall | Opcode::DoFcallByName => {
                matches!(next, Some(Event::Enter { .. }))
            }
            Opcode::Return => {
                (root_return || matches!(next, Some(Event::Back)))
                    && (scalar(t1) || op.op1_type == OperandKind::Unused)
                    && self.uncounted(frame)
            }
            Opcode::VerifyReturnType => {
                let declared = Types::of_mask(self.meta(frame).code.return_mask)
                    .without(Types::OBJECT)
                    .without(Types::REF);
                op.op1_type != OperandKind::Unused
                    && t1.kinds() != Types::NONE
                    && t1.within(declared)
            }
            _ => false,
        };
        if inline { Form::Inline } else { Form::Step }
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
        let numbered = self.number(repeat.is_some())?;
        let end = match (repeat, numbered.interrupt, numbered.end) {
            (Some(repeat), Some(interrupt), _) => End::Loop(LoopHeader {
                at: self.header,
                repeat,
                interrupt,
            }),
            (_, _, Some(exit)) => End::Leave(exit),
            _ => return Err(mismatch("a trace with no end")),
        };
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
            map_ptrs: self.map_ptrs,
            end,
        })
    }

    /// The exits numbered in the order the trace meets them: the guards
    /// held at entry, a loop's interrupt test at its header, then op by
    /// op the guards before it, those it holds itself and its own exit,
    /// and last the exit a trace that does not loop ends through. Each
    /// guard and each opline that leaves names its exit, one an earlier
    /// place took where their specs match ([`Exits::take`]).
    fn number(&mut self, loops: bool) -> Result<Numbered, Abort> {
        let mut order: Vec<(Order, Source)> = Vec::new();
        for (at, guard) in self.guards.iter().enumerate() {
            let key = match guard.at {
                GuardAt::Entry => Order::Entry,
                GuardAt::Before(op) => Order::At(op, Stage::Before),
                GuardAt::Within(op) => Order::At(op, Stage::Within),
            };
            order.push((key, Source::Guard(at)));
        }
        for (at, (op, ..)) in self.leaves.iter().enumerate() {
            order.push((Order::At(*op, Stage::Leaves), Source::Leave(at)));
        }
        order.push(if loops {
            (Order::Header, Source::Interrupt)
        } else {
            (Order::End, Source::End)
        });
        // Stable: within one place, in the order they were placed.
        order.sort_by_key(|(key, _)| *key);
        let mut exits = Exits::default();
        let mut numbered = Numbered {
            exits: Vec::new(),
            guards: Vec::with_capacity(self.guards.len()),
            interrupt: None,
            end: None,
        };
        for (_, source) in order {
            let draft = match source {
                Source::Guard(at) => self.guards[at].exit,
                Source::Leave(at) => {
                    let (_, _, draft) = self.leaves[at];
                    draft
                }
                Source::Interrupt => self.interrupt(),
                Source::End => self.end_draft.unwrap_or_else(|| self.start()),
            };
            let id = exits.take(self.spec(&draft))?;
            match source {
                Source::Guard(at) => {
                    let placed = self.guards[at];
                    numbered.guards.push(Guard {
                        at: placed.at,
                        check: placed.check,
                        exit: id,
                    });
                }
                Source::Leave(at) => {
                    let (op, leaves, _) = self.leaves[at];
                    if let Some(Op::Line(line)) = self.ops.get_mut(op.index()) {
                        match leaves {
                            Leaves::Unfinished => line.exit = Some(id),
                            Leaves::Freed => line.freed = Some(id),
                        }
                    }
                }
                Source::Interrupt => numbered.interrupt = Some(id),
                Source::End => numbered.end = Some(id),
            }
        }
        numbered.exits = exits.specs;
        Ok(numbered)
    }

    /// A loop's interrupt test: at its header, storing what the header's
    /// phis hold in registers.
    fn interrupt(&self) -> Draft {
        let frame = match self.ops.get(self.header.index()) {
            Some(Op::Line(line)) => line.frame,
            _ => Frame::ROOT,
        };
        draft(Point::Before(self.header), frame, self.at_header())
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
            if !carried {
                continue;
            }
            self.val_mut(entry).types = match narrowed {
                Narrowed::Tag(tag) => entered.types.narrowed(tag),
                Narrowed::Layout(layout) => entered.types.laid_out(layout),
            };
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

    /// One exit's spec from its draft and the state it leaves.
    fn spec(&self, draft: &Draft) -> ExitSpec {
        let empty = Snapshot::default();
        let snapshot = match draft.point {
            Point::Start => &empty,
            Point::Before(at) => self.snapshots.get(at.index()).unwrap_or(&empty),
            Point::After(at) => self.snapshots.get(at.index() + 1).unwrap_or(&empty),
            Point::End => self.snapshots.last().unwrap_or(&empty),
        };
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
            } => {
                if calls {
                    (Some(fetch), Landing::RestoreCall)
                } else {
                    extra = Some(Store {
                        slot: self.val(out).slot,
                        from: Stored::Copied(out),
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
        ExitSpec {
            frame: draft.frame,
            opline,
            landing,
            stores,
            link: draft.link,
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

/// What `op` names beside its operands that its inline form reads.
fn imm(op: &Opline) -> Imm {
    match op.opcode {
        Opcode::SendVal | Opcode::SendVar | Opcode::SendValEx
            if op.op2_type == OperandKind::Unused =>
        {
            Imm::Arg(op.op2)
        }
        Opcode::AssignOp => lattice::binary_of(op.extended_value).map_or(Imm::None, Imm::Binary),
        _ => Imm::None,
    }
}

/// An exit with nothing to store beside the trace's registers and no
/// trace to link to.
fn draft(point: Point, frame: Frame, at: At) -> Draft {
    Draft {
        point,
        frame,
        at,
        extra: None,
        link: None,
    }
}

/// The exits a trace has taken so far, by number, and where each spec
/// that names an opline was taken.
#[derive(Default)]
struct Exits {
    specs: Vec<ExitSpec>,
    taken: HashMap<ExitSpec, ExitId>,
}

impl Exits {
    /// The exit that leaves as `spec` says: php's
    /// `zend_jit_trace_get_exit_point`, which reuses an exit whose opline,
    /// `flags` and stack map match one taken
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:168-185`), and refuses the
    /// trace with `TooManyExits` where a new one would pass
    /// `ZEND_JIT_TRACE_MAX_EXITS`. An exit that names no opline
    /// is never reused, as php reuses none whose `to_opline` is null.
    fn take(&mut self, spec: ExitSpec) -> Result<ExitId, Abort> {
        if spec.opline.is_some()
            && let Some(id) = self.taken.get(&spec)
        {
            return Ok(*id);
        }
        if self.specs.len() == MAX_EXITS {
            return Err(Abort {
                stop: Stop::TooManyExits,
                why: "more exits than php's ZEND_JIT_TRACE_MAX_EXITS",
            });
        }
        let id = ExitId(u16::try_from(self.specs.len()).map_err(|_| mismatch("too many exits"))?);
        if spec.opline.is_some() {
            self.taken.insert(spec.clone(), id);
        }
        self.specs.push(spec);
        Ok(id)
    }
}

/// The exits numbered, the guards naming theirs, and the exits a loop's
/// interrupt test and a trace's end take.
struct Numbered {
    exits: Vec<ExitSpec>,
    guards: Vec<Guard>,
    interrupt: Option<ExitId>,
    end: Option<ExitId>,
}

/// Where an exit falls in the order the trace meets them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Order {
    /// A guard held as the trace enters.
    Entry,
    /// A loop's interrupt test, at its header.
    Header,
    /// At an op, in the order of the op's stages.
    At(OpId, Stage),
    /// The exit a trace that does not loop ends through.
    End,
}

/// What happens at an op, in order: the guards before it, the guards it
/// holds itself, then its own exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Before,
    Within,
    Leaves,
}

/// Why an opline leaves through an exit of its own.
#[derive(Clone, Copy)]
enum Leaves {
    /// It cannot finish, or what it ran threw.
    Unfinished,
    /// It finished, and freeing a temporary it read threw.
    Freed,
}

/// Where an exit's draft is kept until it is numbered.
#[derive(Clone, Copy)]
enum Source {
    Guard(usize),
    Leave(usize),
    Interrupt,
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
