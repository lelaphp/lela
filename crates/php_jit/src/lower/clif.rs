//! One trace as one Cranelift function (`jit-design.md` §4.3): the walk
//! over a [`TraceIr`]'s ops in order, each guard where it is held, each
//! exit a cold block that stores what the trace kept in registers and
//! answers its number, and every kernel called through [`Emit::call`],
//! the one emitter, which tests the status word the kernel answers.
//!
//! **Slots.** A slot is read and written in its frame's room at
//! `ROOM_BYTES + 16 * var`, its word at offset zero and its type word,
//! php's `Z_TYPE_INFO`, at [`Zval::TYPE_AT`]. A value `types` keeps in a
//! register is a Cranelift variable, `i64` for a long, `f64` for a double
//! and `i8` for a boolean, written to its slot with its type word only
//! where an exit leaves, a kernel reads it as an operand, or an
//! assignment overwrote a counted value there; every other value is read
//! from its slot where an op reads it, by the type its guard holds.
//!
//! **What each inline form compiles to.** The arithmetic, the
//! comparisons, the jumps and the assignments of the scalar core are
//! compiled code over their operands, with php's overflow to a double,
//! its division and shift errors left to the interpreter through the
//! opline's exit, and a long arithmetic `types` holds long leaving where
//! it overflows. A packed array is read and written in place: a fetch
//! past `nNumUsed` or of a hole leaves, and a write leaves unless the
//! array is counted and held once and the element is there and counts
//! nothing. A send writes the call's slot. The frame chain is the
//! engine's: an inline `INIT_*` is [`Kernel::PushCall`], which refuses a
//! callee other than the one recorded, and an inline `DO_*CALL`,
//! `RETURN` and `SEND_VAL_EX`, whose by-reference check is the callee's
//! declaration, are [`Kernel::Step`]; compiled code follows the frame
//! chain by the [`Stood`] each writes.

use std::mem::offset_of;

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{F64, I8, I32, I64};
use cranelift_codegen::ir::{
    AbiParam, Block, Function, InstBuilder, MemFlagsData, SigRef, Signature, SourceLoc, StackSlot,
    StackSlotData, StackSlotKind, Type, UserFuncName, Value,
};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use php_engine::tier::FuncRef;
use php_heap::{Arr, ROOM_BYTES, Ref, Zval, ZvalType};
use php_unit::{MapPtrOffset, Opcode};

use super::{ABANDONED, Fuzzed, INTERRUPT_AT, Kernel, Stood, Symbols, located};
use crate::record::Stop;
use crate::types::{
    Abort, Check, Constant, Def, End, ExitId, Form, Frame, Fused, Guard, GuardAt, Home, Imm,
    Layout, Line, Op, OpId, Operand, Repeat, Slot, Stored, TraceIr, Types, ValueId,
};

/// A layout offset as the immediate a load or a store takes.
const fn imm(at: usize) -> i32 {
    assert!(at < 0x7fff_ffff, "a layout offset past an immediate");
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "held below i32::MAX above"
    )]
    let at = at as i32;
    at
}

/// The type word of a zval, php's `Z_TYPE_INFO`.
const TYPE_AT: i32 = imm(Zval::TYPE_AT);
/// `IS_TYPE_REFCOUNTED` in the type word: the flags byte follows the type
/// byte, as php's `Z_TYPE_FLAGS_SHIFT` lays it.
const COUNTED: i64 = (Zval::TYPE_REFCOUNTED as i64) << 8;
const _: () = assert!(Zval::FLAGS_AT == Zval::TYPE_AT + 1);
/// A slot, `sizeof(zval)`.
const SLOT: i64 = 16;
const _: () = assert!(size_of::<Zval>() == 16);
const STOOD_FRAME: i32 = imm(offset_of!(Stood, frame));
const STOOD_CALL: i32 = imm(offset_of!(Stood, call));
const STOOD_FUNC: i32 = imm(offset_of!(Stood, func));
const STOOD_OPLINE: i32 = imm(offset_of!(Stood, opline));

/// The type byte of `tag`, as an immediate.
fn tag(tag: ZvalType) -> i64 {
    i64::from(tag as u8)
}

/// Every load and store of compiled code is aligned and of memory the
/// trace's guards and kernels hold live.
fn trusted() -> MemFlagsData {
    MemFlagsData::trusted()
}

/// php's `COMPILER_ERROR`, for a trace whose shape the lowering does not
/// expect of `types`.
fn mismatch(why: &'static str) -> Abort {
    Abort {
        stop: Stop::CompilerError,
        why,
    }
}

/// The function `ir` lowers to on the target `config` describes, calling
/// the kernels at `symbols`, and, where `fuzzed` holds any bytes, failing
/// each guard once by them.
pub(super) fn lower(
    ir: &TraceIr,
    config: TargetFrontendConfig,
    symbols: &Symbols,
    fuzzed: &Fuzzed,
) -> Result<Function, Abort> {
    let call_conv = config.default_call_conv;
    let mut sig = Signature::new(call_conv);
    sig.params.extend([AbiParam::new(I64), AbiParam::new(I64)]);
    sig.returns.push(AbiParam::new(I32));
    let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
    let mut context = FunctionBuilderContext::new();
    let builder = FunctionBuilder::new(&mut func, &mut context);
    let mut emit = Emit::new(ir, builder, call_conv, symbols);
    let (base, len) = fuzzed.span();
    if len > 0 {
        let base = i64::try_from(base).map_err(|_| mismatch("an address past the host's"))?;
        emit.fuzz = Some(Fuzz { base, len, next: 0 });
    }
    emit.trace()?;
    emit.b.finalize(config);
    Ok(func)
}

/// How many guards of `ir` exit-fuzz can fail at once without a wrong
/// answer, the interrupt test among them: each whose exit is right
/// whatever the value it holds, which a jump's direction is not, since
/// its exit lands where the jump the guard did not expect went.
pub(super) fn fuzz_sites(ir: &TraceIr) -> usize {
    let guards = ir.guards.iter();
    guards
        .filter(|guard| !matches!(guard.check, Check::Direction { .. }))
        .count()
        + 1
}

/// Exit-fuzz's bytes as the lowering hands them out (`jit.mode=exit-fuzz`):
/// where they start, how many there are, and the next not handed out.
#[derive(Debug, Clone, Copy)]
struct Fuzz {
    base: i64,
    len: usize,
    next: usize,
}

/// The kinds a value of the scalar core is, in a register or in its slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Long,
    Double,
    /// False or true, which the value says.
    Bool,
    Null,
    False,
    True,
}

impl Kind {
    /// The one kind `types` are, where they are one of the scalar core's.
    fn of(types: Types) -> Option<Kind> {
        if !types.scalar() {
            return None;
        }
        match types.tag() {
            Some(tag) => Kind::of_tag(tag),
            None => Some(Kind::Bool),
        }
    }

    /// The kind of a value of the type byte `tag`.
    fn of_tag(tag: ZvalType) -> Option<Kind> {
        Some(match tag {
            ZvalType::Long => Kind::Long,
            ZvalType::Double => Kind::Double,
            ZvalType::Null => Kind::Null,
            ZvalType::False => Kind::False,
            ZvalType::True => Kind::True,
            _ => return None,
        })
    }

    /// The Cranelift type a register of the kind is.
    fn ty(self) -> Type {
        match self {
            Kind::Long => I64,
            Kind::Double => F64,
            Kind::Bool | Kind::Null | Kind::False | Kind::True => I8,
        }
    }
}

/// A scalar compiled code holds: a long, a double, a boolean as an `i8` of
/// zero or one, or null.
#[derive(Debug, Clone, Copy)]
enum Scalar {
    Long(Value),
    Double(Value),
    Bool(Value),
    Null,
}

/// What an arithmetic computed: a long, a double, or, where a long one
/// overflowed or a division was inexact, either, which `overflowed`
/// says, what php's `ZEND_SIGNED_MULTIPLY_LONG` and `div_function`
/// leave it.
#[derive(Debug, Clone, Copy)]
enum Num {
    Long(Value),
    Double(Value),
    Either {
        overflowed: Value,
        long: Value,
        double: Value,
    },
}

/// One trace's function as it is built.
struct Emit<'a> {
    ir: &'a TraceIr,
    b: FunctionBuilder<'a>,
    call_conv: CallConv,
    symbols: &'a Symbols,
    /// The `ExitCtx`, the function's first parameter.
    cx: Option<Value>,
    /// The address of the request's interrupt byte, read once as a
    /// looping trace enters.
    interrupt: Option<Value>,
    /// Where a kernel writes where the interpreter stands.
    stood: StackSlot,
    /// Per frame of the trace, the room it runs in.
    rooms: Vec<Variable>,
    /// Per value the trace keeps in a register, its variable.
    regs: Vec<Option<Variable>>,
    /// The rooms of the calls under way, the innermost last.
    calls: Vec<Variable>,
    /// Per kernel, its signature once imported.
    sigs: [Option<SigRef>; Kernel::ALL.len()],
    /// Per exit, its block once a site branches to it.
    exits: Vec<Option<Block>>,
    /// The block a kernel's refusal with no exit to take answers from.
    abandon: Option<Block>,
    /// The frame the trace runs in now.
    running: Frame,
    /// Under `jit.mode=exit-fuzz`, the bytes each guard fails once by.
    fuzz: Option<Fuzz>,
}

impl<'a> Emit<'a> {
    fn new(
        ir: &'a TraceIr,
        mut b: FunctionBuilder<'a>,
        call_conv: CallConv,
        symbols: &'a Symbols,
    ) -> Emit<'a> {
        let rooms = ir.frames.iter().map(|_| b.declare_var(I64)).collect();
        let stood = b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            u32::try_from(size_of::<Stood>()).unwrap_or(u32::MAX),
            3,
        ));
        Emit {
            ir,
            b,
            call_conv,
            symbols,
            cx: None,
            interrupt: None,
            stood,
            rooms,
            regs: vec![None; ir.values.len()],
            calls: Vec::new(),
            sigs: [None; Kernel::ALL.len()],
            exits: vec![None; ir.exits.len()],
            abandon: None,
            running: Frame::ROOT,
            fuzz: None,
        }
    }

    // ------------------------------------------------------------ the trace

    /// The whole trace: its entry and the guards held there, the ops
    /// before a loop's header, the header with its interrupt test, the
    /// body and the back edge, or the exit a trace that does not loop
    /// leaves through; then every exit's block.
    fn trace(&mut self) -> Result<(), Abort> {
        let entry = self.b.create_block();
        self.b.append_block_params_for_function_params(entry);
        self.b.switch_to_block(entry);
        self.b.seal_block(entry);
        let [cx, room] = *self.b.block_params(entry) else {
            return Err(mismatch("a trace's function takes two parameters"));
        };
        self.cx = Some(cx);
        self.b.def_var(self.room_var(Frame::ROOT)?, room);
        let header = match self.ir.end {
            End::Loop(header) => Some(header),
            End::Leave(_) => None,
        };
        if header.is_some() {
            self.interrupt = Some(self.b.ins().load(I64, trusted(), cx, INTERRUPT_AT));
        }
        for guard in self.ir.guards.iter().filter(|g| g.at == GuardAt::Entry) {
            self.guard(*guard)?;
        }
        let split = header.map_or(self.ir.ops.len(), |header| header.at.index());
        for at in 0..split {
            self.op(at)?;
        }
        match (self.ir.end, self.interrupt) {
            (End::Loop(header), Some(interrupt)) => {
                let phis = self.register_phis();
                let entered = phis
                    .iter()
                    .map(|&(phi, entry, _)| Ok((phi, self.entered(phi, entry)?)))
                    .collect::<Result<Vec<_>, Abort>>()?;
                for (phi, value) in entered {
                    self.define(phi, value)?;
                }
                let head = self.b.create_block();
                self.b.ins().jump(head, &[]);
                self.b.switch_to_block(head);
                self.b.set_srcloc(located(header.at.index()));
                let flag = self.b.ins().load(I8, trusted(), interrupt, 0);
                let flag = self.fuzzed(flag)?;
                self.leave_if(flag, header.interrupt)?;
                for at in split..self.ir.ops.len() {
                    self.op(at)?;
                }
                let brought = phis
                    .iter()
                    .map(|&(phi, _, back)| Ok((phi, self.register(back)?)))
                    .collect::<Result<Vec<_>, Abort>>()?;
                for (phi, value) in brought {
                    self.define(phi, value)?;
                }
                if matches!(header.repeat, Repeat::Call | Repeat::Return) {
                    // A recursion's next turn runs one frame on, where
                    // its last call or return reached, in the frame its
                    // header serves.
                    let looped = match self.ir.ops.get(header.at.index()) {
                        Some(Op::Line(line)) => line.frame,
                        _ => Frame::ROOT,
                    };
                    let last = self.room(self.running)?;
                    self.b.def_var(self.room_var(looped)?, last);
                }
                self.b.ins().jump(head, &[]);
                self.b.seal_block(head);
            }
            (End::Leave(exit), _) => {
                let block = self.exit_block(exit)?;
                self.b.ins().jump(block, &[]);
            }
            (End::Loop(_), None) => return Err(mismatch("a loop with no interrupt flag")),
        }
        self.exit_blocks()
    }

    /// The phis the trace keeps in registers: each, what it enters with
    /// and what its back edge brings.
    fn register_phis(&self) -> Vec<(ValueId, ValueId, ValueId)> {
        (0u32..)
            .map(ValueId)
            .zip(&self.ir.values)
            .filter_map(|(id, value)| match value.def {
                Def::Phi { entry, back } if value.home == Home::Register => Some((id, entry, back)),
                _ => None,
            })
            .collect()
    }

    /// What the phi `phi` enters its loop with, its entry value read as
    /// the kind the phi is.
    fn entered(&mut self, phi: ValueId, entry: ValueId) -> Result<Scalar, Abort> {
        let held = self.value(entry)?;
        if held.home == Home::Register {
            return self.register(entry);
        }
        let kind = Kind::of(self.value(phi)?.types)
            .ok_or_else(|| mismatch("a register phi of no scalar kind"))?;
        self.load(held.slot, kind, false)
    }

    /// The op at `at`: the guards held before it, then the op.
    fn op(&mut self, at: usize) -> Result<(), Abort> {
        let id = OpId(u32::try_from(at).map_err(|_| mismatch("too many ops"))?);
        self.b.set_srcloc(located(at));
        let before: Vec<Guard> = self
            .ir
            .guards
            .iter()
            .filter(|g| g.at == GuardAt::Before(id))
            .copied()
            .collect();
        for guard in before {
            self.guard(guard)?;
        }
        let ir = self.ir;
        let Some(op) = ir.ops.get(at) else {
            return Err(mismatch("an op past the trace"));
        };
        match op {
            Op::Line(line) => {
                self.running = line.frame;
                match line.form {
                    Form::Step => self.stepped(id, line),
                    Form::Inline => self.inline(id, line),
                }
            }
            Op::InitCall { .. } => {
                let call = self.stood_word(I64, STOOD_CALL);
                let var = self.b.declare_var(I64);
                self.b.def_var(var, call);
                self.calls.push(var);
                Ok(())
            }
            Op::Enter { frame } => {
                let room = self.stood_word(I64, STOOD_FRAME);
                self.b.def_var(self.room_var(*frame)?, room);
                self.running = *frame;
                Ok(())
            }
            Op::Back { frame, .. } => {
                let room = self.stood_word(I64, STOOD_FRAME);
                self.b.def_var(self.room_var(*frame)?, room);
                self.running = *frame;
                for guard in self.within(id) {
                    let Check::Caller { func, opline } = guard.check else {
                        return Err(mismatch("a return holds a guard of another kind"));
                    };
                    let landed = self.stood_word(I32, STOOD_FUNC);
                    let at = self.stood_word(I32, STOOD_OPLINE);
                    let other_func = self.b.ins().icmp_imm_u(
                        IntCC::NotEqual,
                        landed,
                        i64::from(engine_offset(func)?),
                    );
                    let other_opline =
                        self.b
                            .ins()
                            .icmp_imm_u(IntCC::NotEqual, at, i64::from(opline.0));
                    let other = self.b.ins().bor(other_func, other_opline);
                    let other = self.fuzzed(other)?;
                    self.leave_if(other, guard.exit)?;
                }
                Ok(())
            }
        }
    }

    // ------------------------------------------------------------ guards

    /// The guards the op at `at` holds itself.
    fn within(&self, at: OpId) -> Vec<Guard> {
        self.ir
            .guards
            .iter()
            .filter(|g| g.at == GuardAt::Within(at))
            .copied()
            .collect()
    }

    /// A guard held at entry or before an op: what it holds read from the
    /// value's slot, leaving through its exit where it does not hold.
    fn guard(&mut self, guard: Guard) -> Result<(), Abort> {
        match guard.check {
            Check::Type { value, tag: t } => {
                let held = self.value(value)?;
                if held.home == Home::Register {
                    return if Kind::of(held.types) == Kind::of_tag(t) {
                        Ok(())
                    } else {
                        Err(mismatch("a register guarded as another type"))
                    };
                }
                let (room, at) = self.place(held.slot)?;
                let byte = self.b.ins().load(I8, trusted(), room, at + TYPE_AT);
                let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, byte, tag(t));
                let other = self.fuzzed(other)?;
                self.leave_if(other, guard.exit)?;
            }
            Check::Deref { value, tag: t } => {
                let held = self.value(value)?;
                let (room, at) = self.place(held.slot)?;
                let byte = self.b.ins().load(I8, trusted(), room, at + TYPE_AT);
                let other =
                    self.b
                        .ins()
                        .icmp_imm_s(IntCC::NotEqual, byte, tag(ZvalType::Reference));
                let other = self.fuzzed(other)?;
                self.leave_if(other, guard.exit)?;
                let reference = self.b.ins().load(I64, trusted(), room, at);
                let inner =
                    self.b
                        .ins()
                        .load(I8, trusted(), reference, imm(Ref::VALUE_AT) + TYPE_AT);
                let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, inner, tag(t));
                self.leave_if(other, guard.exit)?;
            }
            Check::Layout { value, layout } => {
                let held = self.value(value)?;
                let (room, at) = self.place(held.slot)?;
                let array = self.b.ins().load(I64, trusted(), room, at);
                let flags = self.b.ins().load(I32, trusted(), array, imm(Arr::FLAGS_AT));
                let packed = self.b.ins().band_imm_u(flags, i64::from(Arr::PACKED));
                let cc = match layout {
                    Layout::Packed => IntCC::Equal,
                    Layout::Hash => IntCC::NotEqual,
                };
                let other = self.b.ins().icmp_imm_s(cc, packed, 0);
                let other = self.fuzzed(other)?;
                self.leave_if(other, guard.exit)?;
            }
            Check::Direction { .. } | Check::Caller { .. } => {
                return Err(mismatch("an op's own guard held before it"));
            }
        }
        Ok(())
    }

    /// The jump `jumps`, an `i8` of zero or one, held to the way each of
    /// the op's direction guards says it went.
    fn direction(&mut self, at: OpId, jumps: Value) -> Result<(), Abort> {
        for guard in self.within(at) {
            let Check::Direction { taken } = guard.check else {
                return Err(mismatch("a jump holds a guard of another kind"));
            };
            let other = self
                .b
                .ins()
                .icmp_imm_s(IntCC::Equal, jumps, i64::from(!taken));
            self.leave_if(other, guard.exit)?;
        }
        Ok(())
    }

    /// What an op computed, held to the type guards it holds on the values
    /// in `defs`: a number that may be either kind leaves where it is not
    /// the one held.
    fn settle(&mut self, at: OpId, defs: &[ValueId], num: Num) -> Result<Num, Abort> {
        let mut num = num;
        for guard in self.within(at) {
            let Check::Type { value, tag: t } = guard.check else {
                return Err(mismatch("an arithmetic holds a guard of another kind"));
            };
            if !defs.contains(&value) {
                return Err(mismatch("an arithmetic guards a value it does not write"));
            }
            num = match (num, Kind::of_tag(t)) {
                (
                    Num::Either {
                        overflowed, long, ..
                    },
                    Some(Kind::Long),
                ) => {
                    let overflowed = self.fuzzed(overflowed)?;
                    self.leave_if(overflowed, guard.exit)?;
                    Num::Long(long)
                }
                (
                    Num::Either {
                        overflowed, double, ..
                    },
                    Some(Kind::Double),
                ) => {
                    let exact = self.b.ins().icmp_imm_s(IntCC::Equal, overflowed, 0);
                    let exact = self.fuzzed(exact)?;
                    self.leave_if(exact, guard.exit)?;
                    Num::Double(double)
                }
                (Num::Long(_), Some(Kind::Long)) | (Num::Double(_), Some(Kind::Double)) => num,
                _ => return Err(mismatch("an arithmetic guarded as a type it cannot make")),
            };
        }
        Ok(num)
    }

    /// `cond`, a guard's failure, or under exit-fuzz that or the guard's
    /// first pass: the guard's byte read and set, so it fails once
    /// whatever it holds (`jit-design.md` §4.6, §7).
    fn fuzzed(&mut self, cond: Value) -> Result<Value, Abort> {
        let Some(fuzz) = self.fuzz.as_mut() else {
            return Ok(cond);
        };
        let at = fuzz.next;
        if at >= fuzz.len {
            return Err(mismatch("more guards fuzzed than the trace counted"));
        }
        fuzz.next += 1;
        let address = fuzz
            .base
            .saturating_add(i64::try_from(at).unwrap_or(i64::MAX));
        let byte = self.b.ins().iconst(I64, address);
        let failed = self.b.ins().load(I8, trusted(), byte, 0);
        let one = self.b.ins().iconst(I8, 1);
        self.b.ins().store(trusted(), one, byte, 0);
        let first = self.b.ins().icmp_imm_s(IntCC::Equal, failed, 0);
        Ok(self.b.ins().bor(cond, first))
    }

    /// Leave through `exit` where `cond` is not zero, and go on otherwise.
    fn leave_if(&mut self, cond: Value, exit: ExitId) -> Result<(), Abort> {
        let to = self.exit_block(exit)?;
        let on = self.b.create_block();
        self.b.ins().brif(cond, to, &[], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// The block of `exit`, made the first time a site branches to it.
    fn exit_block(&mut self, exit: ExitId) -> Result<Block, Abort> {
        let at = usize::from(exit.0);
        let slot = self
            .exits
            .get_mut(at)
            .ok_or_else(|| mismatch("an exit past the trace's"))?;
        if let Some(block) = *slot {
            return Ok(block);
        }
        let block = self.b.create_block();
        self.b.set_cold_block(block);
        self.exits[at] = Some(block);
        Ok(block)
    }

    /// The block a kernel's refusal answers [`ABANDONED`] from.
    fn abandoned(&mut self) -> Block {
        if let Some(block) = self.abandon {
            return block;
        }
        let block = self.b.create_block();
        self.b.set_cold_block(block);
        self.abandon = Some(block);
        block
    }

    /// Every exit's block a site branched to: the scalars the trace keeps
    /// in registers stored to their slots with their type words, the
    /// booleans a mispredicted comparison leaves, and the exit's number
    /// answered.
    fn exit_blocks(&mut self) -> Result<(), Abort> {
        self.b.set_srcloc(SourceLoc::default());
        for at in 0..self.exits.len() {
            let Some(block) = self.exits[at] else {
                continue;
            };
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let ir = self.ir;
            let spec = ir
                .exits
                .get(at)
                .ok_or_else(|| mismatch("an exit past the trace's"))?;
            for store in &spec.stores {
                match store.from {
                    Stored::Value(value) => {
                        if self.value(value)?.home == Home::Register {
                            let held = self.register(value)?;
                            self.store(store.slot, held)?;
                        }
                    }
                    Stored::Bool(b) => {
                        let held = Scalar::Bool(self.b.ins().iconst(I8, i64::from(b)));
                        self.store(store.slot, held)?;
                    }
                    // Its site wrote the slot before it branched here.
                    Stored::Copied(_) => {}
                }
            }
            let number = u32::try_from(at).map_err(|_| mismatch("too many exits"))?;
            let number = self.b.ins().iconst(I32, i64::from(number));
            self.b.ins().return_(&[number]);
        }
        if let Some(block) = self.abandon {
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let answer = self.b.ins().iconst(I32, i64::from(ABANDONED));
            self.b.ins().return_(&[answer]);
        }
        Ok(())
    }

    // ------------------------------------------------------------ kernels

    /// The one kernel-call emitter (`jit-design.md` §4.3): `kernel`'s
    /// wrapper called at its address with the `ExitCtx` and `args`, and its
    /// status word tested, leaving through `exit` where it is not zero, or
    /// answering [`ABANDONED`] where the opline names no exit.
    fn call(&mut self, kernel: Kernel, args: &[Value], exit: Option<ExitId>) -> Result<(), Abort> {
        let sig = self.signature(kernel);
        let address = i64::try_from(self.symbols.address(kernel))
            .map_err(|_| mismatch("a kernel address past the host's"))?;
        let callee = self.b.ins().iconst(I64, address);
        let cx = self
            .cx
            .ok_or_else(|| mismatch("a kernel called before the entry"))?;
        let mut all = Vec::with_capacity(args.len() + 1);
        all.push(cx);
        all.extend_from_slice(args);
        let inst = self.b.ins().call_indirect(sig, callee, &all);
        let status = self
            .b
            .inst_results(inst)
            .first()
            .copied()
            .ok_or_else(|| mismatch("a kernel that answers nothing"))?;
        let to = match exit {
            Some(exit) => self.exit_block(exit)?,
            None => self.abandoned(),
        };
        let on = self.b.create_block();
        self.b.ins().brif(status, to, &[], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// `kernel`'s signature, imported once.
    fn signature(&mut self, kernel: Kernel) -> SigRef {
        if let Some(sig) = self.sigs[kernel.index()] {
            return sig;
        }
        let mut sig = Signature::new(self.call_conv);
        // The `ExitCtx`, the frame's room and the opline come first.
        let params: &[Type] = match kernel {
            Kernel::Step => &[I64, I64, I32, I64],
            Kernel::PushCall => &[I64, I64, I32, I32, I64],
            Kernel::Release => &[I64, I64, I32, I64, I32],
            Kernel::Copy => &[I64, I64, I32, I64, I32, I64],
        };
        sig.params
            .extend(params.iter().map(|ty| AbiParam::new(*ty)));
        sig.returns.push(AbiParam::new(I32));
        let sig = self.b.import_signature(sig);
        self.sigs[kernel.index()] = Some(sig);
        sig
    }

    /// The room and the opline index a kernel serving `line` is called
    /// with.
    fn served(&mut self, line: &Line) -> Result<[Value; 2], Abort> {
        let room = self.room(line.frame)?;
        let opline = self.b.ins().iconst(I32, i64::from(line.opline.0));
        Ok([room, opline])
    }

    /// A word of the [`Stood`] the last kernel wrote.
    fn stood_word(&mut self, ty: Type, at: i32) -> Value {
        self.b.ins().stack_load(I64, ty, self.stood, at)
    }

    /// `line` served whole by [`Kernel::Step`]: the operands it reads from
    /// registers stored first, so the interpreter reads them in their
    /// slots, and the way a jump it served went held to its guards.
    fn stepped(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        for operand in [line.op1, line.op2, line.data] {
            if let Operand::Value(value) = operand
                && self.value(value)?.home == Home::Register
            {
                let held = self.register(value)?;
                self.store(self.value(value)?.slot, held)?;
            }
        }
        let [room, opline] = self.served(line)?;
        let stood = self.b.ins().stack_addr(I64, self.stood, 0);
        self.call(Kernel::Step, &[room, opline, stood], line.exit)?;
        if matches!(
            line.opcode,
            Opcode::DoFcall | Opcode::DoIcall | Opcode::DoUcall | Opcode::DoFcallByName
        ) {
            self.calls.pop();
        }
        for guard in self.within(at) {
            let Check::Direction { .. } = guard.check else {
                return Err(mismatch("a stepped opline holds a guard of another kind"));
            };
            let went = self.stood_word(I32, STOOD_OPLINE);
            let other = if let Some(next) = self.successor(at, line.frame) {
                self.b
                    .ins()
                    .icmp_imm_u(IntCC::NotEqual, went, i64::from(next))
            } else {
                let landing = self
                    .ir
                    .exits
                    .get(usize::from(guard.exit.0))
                    .and_then(|exit| exit.opline)
                    .ok_or_else(|| mismatch("a stepped jump with nowhere to hold"))?;
                self.b
                    .ins()
                    .icmp_imm_u(IntCC::Equal, went, i64::from(landing.0))
            };
            self.leave_if(other, guard.exit)?;
        }
        Ok(())
    }

    /// The opline the trace serves next in `frame` after the op at `at`,
    /// where it serves one there: where a stepped jump the trace follows
    /// went.
    fn successor(&self, at: OpId, frame: Frame) -> Option<u32> {
        let next = self
            .ir
            .ops
            .iter()
            .skip(at.index() + 1)
            .find_map(|op| match op {
                Op::Line(line) => Some(line),
                _ => None,
            });
        let next = match (next, self.ir.end) {
            (Some(line), _) => Some(line),
            (None, End::Loop(header)) => match self.ir.ops.get(header.at.index()) {
                Some(Op::Line(line)) => Some(line),
                _ => None,
            },
            (None, End::Leave(_)) => None,
        };
        next.filter(|line| line.frame == frame)
            .map(|line| line.opline.0)
    }

    // ------------------------------------------------------------ inline forms

    /// One inline opline (`jit-design.md` §4.3), the scalar core.
    fn inline(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        match line.opcode {
            Opcode::Jmp => Ok(()),
            Opcode::Jmpz | Opcode::Jmpnz | Opcode::JmpzEx | Opcode::JmpnzEx => self.jump(at, line),
            Opcode::QmAssign | Opcode::VerifyReturnType => {
                let held = self.scalar(at, line.op1)?;
                for value in [line.out, line.op1_def].into_iter().flatten() {
                    self.define(value, held)?;
                }
                Ok(())
            }
            Opcode::IsSmaller
            | Opcode::IsSmallerOrEqual
            | Opcode::IsEqual
            | Opcode::IsNotEqual
            | Opcode::IsIdentical
            | Opcode::IsNotIdentical => self.compare(at, line),
            Opcode::Add
            | Opcode::Sub
            | Opcode::Mul
            | Opcode::Div
            | Opcode::Mod
            | Opcode::Sl
            | Opcode::Sr
            | Opcode::BwAnd
            | Opcode::BwOr
            | Opcode::BwXor => self.arithmetic(at, line, line.opcode),
            Opcode::AssignOp => {
                let Imm::Binary(binary) = line.imm else {
                    return Err(mismatch("an ASSIGN_OP with no operator"));
                };
                self.arithmetic(at, line, binary)
            }
            Opcode::Assign => self.assign(at, line),
            Opcode::PreInc | Opcode::PreDec | Opcode::PostInc | Opcode::PostDec => {
                self.increment(at, line)
            }
            Opcode::FetchDimR => self.fetch(at, line),
            Opcode::AssignDim => self.assign_dim(at, line),
            Opcode::SendVal | Opcode::SendVar => self.send(at, line),
            Opcode::SendValEx
            | Opcode::DoUcall
            | Opcode::DoFcall
            | Opcode::DoFcallByName
            | Opcode::Return => self.stepped(at, line),
            Opcode::InitFcall | Opcode::InitFcallByName | Opcode::InitNsFcallByName => {
                self.push_call(at, line)
            }
            _ => Err(mismatch("an inline opline the lowering does not serve")),
        }
    }

    /// A conditional jump on a scalar: its direction held, and the
    /// boolean a `JMPZ_EX` or `JMPNZ_EX` writes.
    fn jump(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let held = self.scalar(at, line.op1)?;
        let truth = self.truth(held);
        if let Some(out) = line.out {
            self.define(out, Scalar::Bool(truth))?;
        }
        let jumps = match line.opcode {
            Opcode::Jmpz | Opcode::JmpzEx => self.b.ins().icmp_imm_s(IntCC::Equal, truth, 0),
            _ => truth,
        };
        self.direction(at, jumps)
    }

    /// A comparison of two scalars: the boolean it writes, or, fused to
    /// the jump after it, the way that jump goes held.
    fn compare(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let a = self.scalar(at, line.op1)?;
        let b = self.scalar(at, line.op2)?;
        let result = self.comparison(line.opcode, a, b)?;
        let jumps = match line.fused {
            Some(Fused::Jmpz) => self.b.ins().icmp_imm_s(IntCC::Equal, result, 0),
            Some(Fused::Jmpnz) => result,
            None => {
                if let Some(out) = line.out {
                    self.define(out, Scalar::Bool(result))?;
                }
                result
            }
        };
        self.direction(at, jumps)
    }

    /// The arithmetic `op` over the opline's two operands, written to what
    /// it writes: its `out`, or the variable an `ASSIGN_OP` assigns.
    fn arithmetic(&mut self, at: OpId, line: &Line, op: Opcode) -> Result<(), Abort> {
        let a = self.scalar(at, line.op1)?;
        let b = self.scalar(at, line.op2)?;
        let num = self.binary(op, a, b, line.exit)?;
        let defs: Vec<ValueId> = [line.out, line.op1_def].into_iter().flatten().collect();
        let num = self.settle(at, &defs, num)?;
        for value in defs {
            self.define_num(value, num)?;
        }
        Ok(())
    }

    /// An increment or a decrement of a number in a variable: the variable
    /// written, and what the opline writes beside it, the new value or,
    /// for a `POST_*`, the old.
    fn increment(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let old = self.scalar(at, line.op1)?;
        let up = matches!(line.opcode, Opcode::PreInc | Opcode::PostInc);
        let new = match old {
            Scalar::Long(x) => {
                let (long, overflowed) = if up {
                    let one = self.b.ins().iconst(I64, 1);
                    self.b.ins().sadd_overflow(x, one)
                } else {
                    let one = self.b.ins().iconst(I64, 1);
                    self.b.ins().ssub_overflow(x, one)
                };
                let real = self.b.ins().fcvt_from_sint(F64, x);
                let one = self.b.ins().f64const(1.0);
                let double = if up {
                    self.b.ins().fadd(real, one)
                } else {
                    self.b.ins().fsub(real, one)
                };
                Num::Either {
                    overflowed,
                    long,
                    double,
                }
            }
            Scalar::Double(x) => {
                let one = self.b.ins().f64const(1.0);
                Num::Double(if up {
                    self.b.ins().fadd(x, one)
                } else {
                    self.b.ins().fsub(x, one)
                })
            }
            Scalar::Bool(_) | Scalar::Null => {
                return Err(mismatch("an inline increment of no number"));
            }
        };
        let pre = matches!(line.opcode, Opcode::PreInc | Opcode::PreDec);
        let mut defs: Vec<ValueId> = line.op1_def.into_iter().collect();
        if pre {
            defs.extend(line.out);
        }
        let new = self.settle(at, &defs, new)?;
        for value in defs {
            self.define_num(value, new)?;
        }
        if let (false, Some(out)) = (pre, line.out) {
            self.define(out, old)?;
        }
        Ok(())
    }

    /// An assignment of a scalar to a variable: the variable written, and
    /// where what it held was counted, released after, as
    /// `zend_assign_to_variable` frees it, what its destructor throws
    /// leaving through the opline's exit. A register's slot is written
    /// too where the old value was counted, so no slot names a value
    /// freed.
    fn assign(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let held = self.scalar(at, line.op2)?;
        let Operand::Value(target) = line.op1 else {
            return Err(mismatch("an assignment to no variable"));
        };
        let slot = self.value(target)?.slot;
        let old = match line.exit {
            Some(_) => {
                let (room, at) = self.place(slot)?;
                let word = self.b.ins().load(I64, trusted(), room, at);
                let info = self.b.ins().load(I32, trusted(), room, at + TYPE_AT);
                Some((word, info))
            }
            None => None,
        };
        if let Some(def) = line.op1_def {
            self.define(def, held)?;
            if old.is_some() && self.value(def)?.home == Home::Register {
                self.store(slot, held)?;
            }
        }
        if let Some(out) = line.out {
            self.define(out, held)?;
        }
        if let Some((word, info)) = old {
            let counted = self.b.ins().band_imm_u(info, COUNTED);
            let [room, opline] = self.served(line)?;
            self.when(counted, |emit| {
                emit.call(Kernel::Release, &[room, opline, word, info], line.exit)
            })?;
        }
        Ok(())
    }

    /// A read of a packed array's element by a long key: past `nNumUsed`
    /// or a hole leaves for the interpreter to serve it again; an element
    /// that is a reference is read through it; one of the type the op's
    /// guard holds is read, and one of another copied to the slot the
    /// fetch writes as php's `ZVAL_COPY_DEREF` copies it, for the exit to
    /// go on past the fetch. A temporary array the fetch read is freed
    /// after, as its handler frees it, what a destructor throws leaving
    /// through the exit `types` gives the free.
    fn fetch(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let exit = line
            .exit
            .ok_or_else(|| mismatch("a fetch that cannot leave"))?;
        let Operand::Value(array) = line.op1 else {
            return Err(mismatch("a fetch from no array"));
        };
        let array_slot = self.value(array)?.slot;
        let Scalar::Long(key) = self.scalar(at, line.op2)? else {
            return Err(mismatch("a fetch by no long key"));
        };
        let out = line
            .out
            .ok_or_else(|| mismatch("a fetch that writes nothing"))?;
        let (room, array_at) = self.place(array_slot)?;
        let arr = self.b.ins().load(I64, trusted(), room, array_at);
        let element = self.element(arr, key, exit)?;
        let hole = self.b.ins().load(I8, trusted(), element, TYPE_AT);
        let hole = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, hole, tag(ZvalType::Undef));
        self.leave_if(hole, exit)?;
        let element = self.deref(element);
        let info = self.b.ins().load(I32, trusted(), element, TYPE_AT);
        let kind = self.b.ins().band_imm_u(info, 0xff);
        let [room_arg, opline] = self.served(line)?;
        let out_slot = self.value(out)?.slot;
        let guards = self.within(at);
        match guards.as_slice() {
            [] => self.copy(line, element, info, out_slot)?,
            [guard] => {
                let Check::Type { value, tag: t } = guard.check else {
                    return Err(mismatch("a fetch holds a guard of another kind"));
                };
                if value != out {
                    return Err(mismatch("a fetch guards a value it does not write"));
                }
                let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, kind, tag(t));
                let other = self.fuzzed(other)?;
                let goes_on = self
                    .ir
                    .exits
                    .get(usize::from(guard.exit.0))
                    .is_some_and(|spec| {
                        spec.stores
                            .iter()
                            .any(|store| store.from == Stored::Copied(out))
                    });
                let to = self.exit_block(guard.exit)?;
                let failed = self.b.create_block();
                self.b.set_cold_block(failed);
                let on = self.b.create_block();
                self.b.ins().brif(other, failed, &[], on, &[]);
                self.b.switch_to_block(failed);
                self.b.seal_block(failed);
                if goes_on {
                    // The exit goes on past the fetch: the element is the
                    // slot's, copied as the fetch copies it.
                    self.copy(line, element, info, out_slot)?;
                }
                self.b.ins().jump(to, &[]);
                self.b.switch_to_block(on);
                self.b.seal_block(on);
                match Kind::of_tag(t) {
                    Some(kind) => {
                        let read = self.read(element, 0, kind);
                        self.define(out, read)?;
                    }
                    None => self.copy(line, element, info, out_slot)?,
                }
            }
            _ => return Err(mismatch("a fetch holds more than one guard")),
        }
        if array_slot.var >= self.frame_cvs(array_slot.frame)? {
            let info = self.b.ins().load(I32, trusted(), room, array_at + TYPE_AT);
            let counted = self.b.ins().band_imm_u(info, COUNTED);
            self.when(counted, |emit| {
                emit.call(Kernel::Release, &[room_arg, opline, arr, info], line.freed)
            })?;
        }
        Ok(())
    }

    /// A write of a scalar to a packed array's element by a long key, in
    /// place: it leaves for the interpreter unless the array is counted
    /// and held once, so no other holder sees the write and no separation
    /// is due, and the element is there and counts nothing, so nothing is
    /// freed.
    fn assign_dim(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let exit = line
            .exit
            .ok_or_else(|| mismatch("an element write that cannot leave"))?;
        let Operand::Value(array) = line.op1 else {
            return Err(mismatch("an element write to no array"));
        };
        let (room, array_at) = self.place(self.value(array)?.slot)?;
        let info = self.b.ins().load(I32, trusted(), room, array_at + TYPE_AT);
        let counted = self.b.ins().band_imm_u(info, COUNTED);
        let immutable = self.b.ins().icmp_imm_s(IntCC::Equal, counted, 0);
        self.leave_if(immutable, exit)?;
        let arr = self.b.ins().load(I64, trusted(), room, array_at);
        let count = self
            .b
            .ins()
            .load(I32, trusted(), arr, imm(Arr::REFCOUNT_AT));
        let shared = self.b.ins().icmp_imm_s(IntCC::NotEqual, count, 1);
        self.leave_if(shared, exit)?;
        let Scalar::Long(key) = self.scalar(at, line.op2)? else {
            return Err(mismatch("an element write by no long key"));
        };
        let element = self.element(arr, key, exit)?;
        let held = self.b.ins().load(I32, trusted(), element, TYPE_AT);
        let kind = self.b.ins().band_imm_u(held, 0xff);
        let hole = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, kind, tag(ZvalType::Undef));
        self.leave_if(hole, exit)?;
        let counts = self.b.ins().band_imm_u(held, COUNTED);
        self.leave_if(counts, exit)?;
        let value = self.scalar(at, line.data)?;
        self.write(element, 0, value);
        if let Some(out) = line.out {
            self.define(out, value)?;
        }
        Ok(())
    }

    /// The address of the slot `key` names in the packed array `arr`,
    /// leaving through `exit` where the key is past `nNumUsed`.
    fn element(&mut self, arr: Value, key: Value, exit: ExitId) -> Result<Value, Abort> {
        let used = self.b.ins().load(I32, trusted(), arr, imm(Arr::USED_AT));
        let used = self.b.ins().uextend(I64, used);
        let past = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, key, used);
        self.leave_if(past, exit)?;
        let data = self.b.ins().load(I64, trusted(), arr, imm(Arr::DATA_AT));
        let offset = self.b.ins().imul_imm_s(key, SLOT);
        Ok(self.b.ins().iadd(data, offset))
    }

    /// The zval at `zval`, or, where it is a reference, the one the
    /// reference holds: php's `ZVAL_DEREF`.
    fn deref(&mut self, zval: Value) -> Value {
        let byte = self.b.ins().load(I8, trusted(), zval, TYPE_AT);
        let is_ref = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, byte, tag(ZvalType::Reference));
        let through = self.b.create_block();
        let merged = self.b.create_block();
        let at = self.b.append_block_param(merged, I64);
        self.b
            .ins()
            .brif(is_ref, through, &[], merged, &[zval.into()]);
        self.b.switch_to_block(through);
        self.b.seal_block(through);
        let reference = self.b.ins().load(I64, trusted(), zval, 0);
        let inner = self
            .b
            .ins()
            .iadd_imm_s(reference, i64::from(imm(Ref::VALUE_AT)));
        self.b.ins().jump(merged, &[inner.into()]);
        self.b.switch_to_block(merged);
        self.b.seal_block(merged);
        at
    }

    /// The zval at `zval`, of type word `info`, copied to `slot` by
    /// [`Kernel::Copy`].
    fn copy(&mut self, line: &Line, zval: Value, info: Value, slot: Slot) -> Result<(), Abort> {
        let word = self.b.ins().load(I64, trusted(), zval, 0);
        let (room, at) = self.place(slot)?;
        let out = self.b.ins().iadd_imm_s(room, i64::from(at));
        let [room, opline] = self.served(line)?;
        self.call(Kernel::Copy, &[room, opline, word, info, out], None)
    }

    /// A send of a scalar: the call's slot for the argument written.
    fn send(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Imm::Arg(number) = line.imm else {
            return Err(mismatch("a send with no argument number"));
        };
        let call = *self
            .calls
            .last()
            .ok_or_else(|| mismatch("a send with no call under way"))?;
        let held = self.scalar(at, line.op1)?;
        let call = self.b.use_var(call);
        let at = slot_offset(
            number
                .checked_sub(1)
                .ok_or_else(|| mismatch("an argument numbered zero"))?,
        )?;
        self.write(call, at, held);
        Ok(())
    }

    /// An inline `INIT_*` through [`Kernel::PushCall`], held to the callee
    /// the trace recorded: a refusal leaves through the opline's exit
    /// before anything is pushed.
    fn push_call(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Some(Op::InitCall {
            target: Some(callee),
            ..
        }) = self.ir.ops.get(at.index() + 1)
        else {
            return Err(mismatch("an inline call with no callee recorded"));
        };
        let exit = line
            .exit
            .ok_or_else(|| mismatch("an inline call that cannot leave"))?;
        let callee = self.b.ins().iconst(I32, i64::from(engine_offset(*callee)?));
        let [room, opline] = self.served(line)?;
        let stood = self.b.ins().stack_addr(I64, self.stood, 0);
        self.call(Kernel::PushCall, &[room, opline, callee, stood], Some(exit))?;
        let call = self.stood_word(I64, STOOD_CALL);
        let refused = self.b.ins().icmp_imm_s(IntCC::Equal, call, 0);
        self.leave_if(refused, exit)?;
        Ok(())
    }

    /// `body` where `cond` is not zero, then on.
    fn when(
        &mut self,
        cond: Value,
        body: impl FnOnce(&mut Emit<'a>) -> Result<(), Abort>,
    ) -> Result<(), Abort> {
        let then = self.b.create_block();
        let on = self.b.create_block();
        self.b.ins().brif(cond, then, &[], on, &[]);
        self.b.switch_to_block(then);
        self.b.seal_block(then);
        body(self)?;
        self.b.ins().jump(on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    // ------------------------------------------------------------ arithmetic

    /// php's binary arithmetic `op` over two scalars of the scalar core,
    /// leaving through `exit` where php raises an error the interpreter
    /// throws: a division or a modulo by zero, a shift by a negative
    /// number.
    fn binary(
        &mut self,
        op: Opcode,
        a: Scalar,
        b: Scalar,
        exit: Option<ExitId>,
    ) -> Result<Num, Abort> {
        let leaves =
            |exit: Option<ExitId>| exit.ok_or_else(|| mismatch("an arithmetic that cannot leave"));
        match (op, a, b) {
            (Opcode::Add | Opcode::Sub | Opcode::Mul, Scalar::Long(x), Scalar::Long(y)) => {
                let (long, overflowed) = match op {
                    Opcode::Add => self.b.ins().sadd_overflow(x, y),
                    Opcode::Sub => self.b.ins().ssub_overflow(x, y),
                    _ => self.b.ins().smul_overflow(x, y),
                };
                let (rx, ry) = (self.real(a)?, self.real(b)?);
                let double = self.float(op, rx, ry);
                Ok(Num::Either {
                    overflowed,
                    long,
                    double,
                })
            }
            (Opcode::Add | Opcode::Sub | Opcode::Mul, _, _) => {
                let (rx, ry) = (self.real(a)?, self.real(b)?);
                Ok(Num::Double(self.float(op, rx, ry)))
            }
            (Opcode::Div, Scalar::Long(x), Scalar::Long(y)) => {
                let exit = leaves(exit)?;
                let zero = self.b.ins().icmp_imm_s(IntCC::Equal, y, 0);
                self.leave_if(zero, exit)?;
                let minus_one = self.b.ins().icmp_imm_s(IntCC::Equal, y, -1);
                let one = self.b.ins().iconst(I64, 1);
                let divisor = self.b.ins().select(minus_one, one, y);
                let quotient = self.b.ins().sdiv(x, divisor);
                let negated = self.b.ins().ineg(x);
                let long = self.b.ins().select(minus_one, negated, quotient);
                let remainder = self.b.ins().srem(x, divisor);
                let inexact = self.b.ins().icmp_imm_s(IntCC::NotEqual, remainder, 0);
                let lowest = self.b.ins().icmp_imm_s(IntCC::Equal, x, i64::MIN);
                let overflows = self.b.ins().band(minus_one, lowest);
                let overflowed = self.b.ins().bor(inexact, overflows);
                let (rx, ry) = (self.real(a)?, self.real(b)?);
                let double = self.b.ins().fdiv(rx, ry);
                Ok(Num::Either {
                    overflowed,
                    long,
                    double,
                })
            }
            (Opcode::Div, _, _) => {
                let exit = leaves(exit)?;
                let (rx, ry) = (self.real(a)?, self.real(b)?);
                let zero = self.b.ins().f64const(0.0);
                let by_zero = self.b.ins().fcmp(FloatCC::Equal, ry, zero);
                self.leave_if(by_zero, exit)?;
                Ok(Num::Double(self.b.ins().fdiv(rx, ry)))
            }
            (Opcode::Mod, Scalar::Long(x), Scalar::Long(y)) => {
                let exit = leaves(exit)?;
                let zero = self.b.ins().icmp_imm_s(IntCC::Equal, y, 0);
                self.leave_if(zero, exit)?;
                // php answers zero for `% -1` rather than overflow.
                let minus_one = self.b.ins().icmp_imm_s(IntCC::Equal, y, -1);
                let one = self.b.ins().iconst(I64, 1);
                let divisor = self.b.ins().select(minus_one, one, y);
                Ok(Num::Long(self.b.ins().srem(x, divisor)))
            }
            (Opcode::Sl | Opcode::Sr, Scalar::Long(x), Scalar::Long(y)) => {
                let exit = leaves(exit)?;
                let negative = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, y, 0);
                self.leave_if(negative, exit)?;
                let wide = self
                    .b
                    .ins()
                    .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, y, 64);
                let (shifted, past) = if op == Opcode::Sl {
                    (self.b.ins().ishl(x, y), self.b.ins().iconst(I64, 0))
                } else {
                    (self.b.ins().sshr(x, y), self.b.ins().sshr_imm_u(x, 63))
                };
                Ok(Num::Long(self.b.ins().select(wide, past, shifted)))
            }
            (Opcode::BwAnd, Scalar::Long(x), Scalar::Long(y)) => {
                Ok(Num::Long(self.b.ins().band(x, y)))
            }
            (Opcode::BwOr, Scalar::Long(x), Scalar::Long(y)) => {
                Ok(Num::Long(self.b.ins().bor(x, y)))
            }
            (Opcode::BwXor, Scalar::Long(x), Scalar::Long(y)) => {
                Ok(Num::Long(self.b.ins().bxor(x, y)))
            }
            _ => Err(mismatch(
                "an inline arithmetic over operands it does not serve",
            )),
        }
    }

    /// The double arithmetic of `op`.
    fn float(&mut self, op: Opcode, x: Value, y: Value) -> Value {
        match op {
            Opcode::Add => self.b.ins().fadd(x, y),
            Opcode::Sub => self.b.ins().fsub(x, y),
            _ => self.b.ins().fmul(x, y),
        }
    }

    /// A number as a double, a long converted as php's `(double)` does.
    fn real(&mut self, held: Scalar) -> Result<Value, Abort> {
        match held {
            Scalar::Long(x) => Ok(self.b.ins().fcvt_from_sint(F64, x)),
            Scalar::Double(x) => Ok(x),
            Scalar::Bool(_) | Scalar::Null => Err(mismatch("an inline arithmetic over no number")),
        }
    }

    /// php's comparison `op` of two scalars, an `i8` of zero or one: two
    /// longs as longs, a long beside a double as two doubles
    /// (`compare_longs_to_doubles`), so NaN is equal to nothing and below
    /// nothing; identity as php's `zend_is_identical` of scalars.
    fn comparison(&mut self, op: Opcode, a: Scalar, b: Scalar) -> Result<Value, Abort> {
        let numeric = |cc: IntCC, fc: FloatCC| (cc, fc);
        let (cc, fc) = match op {
            Opcode::IsSmaller => numeric(IntCC::SignedLessThan, FloatCC::LessThan),
            Opcode::IsSmallerOrEqual => {
                numeric(IntCC::SignedLessThanOrEqual, FloatCC::LessThanOrEqual)
            }
            Opcode::IsEqual => numeric(IntCC::Equal, FloatCC::Equal),
            Opcode::IsNotEqual => numeric(IntCC::NotEqual, FloatCC::NotEqual),
            Opcode::IsIdentical | Opcode::IsNotIdentical => {
                let same = self.identical(a, b);
                return Ok(if op == Opcode::IsIdentical {
                    same
                } else {
                    self.b.ins().icmp_imm_s(IntCC::Equal, same, 0)
                });
            }
            _ => return Err(mismatch("an inline comparison it does not serve")),
        };
        if let (Scalar::Long(x), Scalar::Long(y)) = (a, b) {
            return Ok(self.b.ins().icmp(cc, x, y));
        }
        let (rx, ry) = (self.real(a)?, self.real(b)?);
        Ok(self.b.ins().fcmp(fc, rx, ry))
    }

    /// Whether two scalars are identical: of one type and one value.
    fn identical(&mut self, a: Scalar, b: Scalar) -> Value {
        match (a, b) {
            (Scalar::Long(x), Scalar::Long(y)) | (Scalar::Bool(x), Scalar::Bool(y)) => {
                self.b.ins().icmp(IntCC::Equal, x, y)
            }
            (Scalar::Double(x), Scalar::Double(y)) => self.b.ins().fcmp(FloatCC::Equal, x, y),
            (Scalar::Null, Scalar::Null) => self.b.ins().iconst(I8, 1),
            _ => self.b.ins().iconst(I8, 0),
        }
    }

    /// php's `zend_is_true` of a scalar: a long or a double not zero, NaN
    /// true.
    fn truth(&mut self, held: Scalar) -> Value {
        match held {
            Scalar::Long(x) => self.b.ins().icmp_imm_s(IntCC::NotEqual, x, 0),
            Scalar::Double(x) => {
                let zero = self.b.ins().f64const(0.0);
                self.b.ins().fcmp(FloatCC::NotEqual, x, zero)
            }
            Scalar::Bool(b) => b,
            Scalar::Null => self.b.ins().iconst(I8, 0),
        }
    }

    // ------------------------------------------------------------ values

    fn value(&self, value: ValueId) -> Result<crate::types::Value, Abort> {
        self.ir
            .values
            .get(value.0 as usize)
            .copied()
            .ok_or_else(|| mismatch("a value past the trace's"))
    }

    /// The variables of `frame`, past which its slots are temporaries.
    fn frame_cvs(&self, frame: Frame) -> Result<u32, Abort> {
        self.ir
            .frames
            .get(usize::from(frame.0))
            .map(|info| info.cvs)
            .ok_or_else(|| mismatch("a frame past the trace's"))
    }

    /// The variable of `frame`'s room.
    fn room_var(&self, frame: Frame) -> Result<Variable, Abort> {
        self.rooms
            .get(usize::from(frame.0))
            .copied()
            .ok_or_else(|| mismatch("a frame past the trace's"))
    }

    /// `frame`'s room.
    fn room(&mut self, frame: Frame) -> Result<Value, Abort> {
        let var = self.room_var(frame)?;
        Ok(self.b.use_var(var))
    }

    /// Where `slot` is: its frame's room and its offset there.
    fn place(&mut self, slot: Slot) -> Result<(Value, i32), Abort> {
        Ok((self.room(slot.frame)?, slot_offset(slot.var)?))
    }

    /// The variable of a value the trace keeps in a register, and its kind.
    fn register_var(&mut self, value: ValueId) -> Result<(Variable, Kind), Abort> {
        let kind = Kind::of(self.value(value)?.types)
            .ok_or_else(|| mismatch("a register of no scalar kind"))?;
        let at = value.0 as usize;
        let var = if let Some(var) = self.regs.get(at).copied().flatten() {
            var
        } else {
            let var = self.b.declare_var(kind.ty());
            if let Some(slot) = self.regs.get_mut(at) {
                *slot = Some(var);
            }
            var
        };
        Ok((var, kind))
    }

    /// What a value the trace keeps in a register holds.
    fn register(&mut self, value: ValueId) -> Result<Scalar, Abort> {
        let (var, kind) = self.register_var(value)?;
        Ok(match kind {
            Kind::Long => Scalar::Long(self.b.use_var(var)),
            Kind::Double => Scalar::Double(self.b.use_var(var)),
            Kind::Bool => Scalar::Bool(self.b.use_var(var)),
            Kind::Null => Scalar::Null,
            Kind::False => Scalar::Bool(self.b.ins().iconst(I8, 0)),
            Kind::True => Scalar::Bool(self.b.ins().iconst(I8, 1)),
        })
    }

    /// An operand of the inline op at `at` as the scalar it reads: a
    /// constant, a register, or its slot read as the type the guard
    /// before the op holds, through a reference where the guard holds one.
    fn scalar(&mut self, at: OpId, operand: Operand) -> Result<Scalar, Abort> {
        let Operand::Value(value) = operand else {
            return self.scalar_unguarded(operand);
        };
        let held = self.value(value)?;
        if held.home == Home::Register {
            return self.register(value);
        }
        let guarded = self.ir.guards.iter().find_map(|g| match g.check {
            Check::Type { value: v, tag: t } if v == value && g.at == GuardAt::Before(at) => {
                Some((t, false))
            }
            Check::Deref { value: v, tag: t } if v == value && g.at == GuardAt::Before(at) => {
                Some((t, true))
            }
            _ => None,
        });
        let (kind, through) = match guarded {
            Some((t, through)) => (Kind::of_tag(t), through),
            None => (Kind::of(held.types), false),
        };
        let kind =
            kind.ok_or_else(|| mismatch("an inline opline reads a value of no scalar kind"))?;
        self.load(held.slot, kind, through)
    }

    /// An operand that is a constant or a value read as its own types say.
    fn scalar_unguarded(&mut self, operand: Operand) -> Result<Scalar, Abort> {
        match operand {
            Operand::Constant(constant) => Ok(match constant {
                Constant::Null => Scalar::Null,
                Constant::False => Scalar::Bool(self.b.ins().iconst(I8, 0)),
                Constant::True => Scalar::Bool(self.b.ins().iconst(I8, 1)),
                Constant::Long(n) => Scalar::Long(self.b.ins().iconst(I64, n)),
                Constant::Double(d) => Scalar::Double(self.b.ins().f64const(d)),
                Constant::Other(_) => {
                    return Err(mismatch("an inline opline reads a constant of no scalar"));
                }
            }),
            Operand::Value(value) => {
                let held = self.value(value)?;
                if held.home == Home::Register {
                    return self.register(value);
                }
                let kind = Kind::of(held.types)
                    .ok_or_else(|| mismatch("an inline opline reads a value of no scalar kind"))?;
                self.load(held.slot, kind, false)
            }
            Operand::Unused => Err(mismatch("an inline opline reads an unused operand")),
        }
    }

    /// `slot` read as `kind`, through the reference it holds where
    /// `through`.
    fn load(&mut self, slot: Slot, kind: Kind, through: bool) -> Result<Scalar, Abort> {
        let (room, at) = self.place(slot)?;
        if through {
            let reference = self.b.ins().load(I64, trusted(), room, at);
            return Ok(self.read(reference, imm(Ref::VALUE_AT), kind));
        }
        Ok(self.read(room, at, kind))
    }

    /// The zval at `base + at` read as `kind`.
    fn read(&mut self, base: Value, at: i32, kind: Kind) -> Scalar {
        match kind {
            Kind::Long => Scalar::Long(self.b.ins().load(I64, trusted(), base, at)),
            Kind::Double => Scalar::Double(self.b.ins().load(F64, trusted(), base, at)),
            Kind::Bool => {
                let byte = self.b.ins().load(I8, trusted(), base, at + TYPE_AT);
                Scalar::Bool(
                    self.b
                        .ins()
                        .icmp_imm_s(IntCC::Equal, byte, tag(ZvalType::True)),
                )
            }
            Kind::Null => Scalar::Null,
            Kind::False => Scalar::Bool(self.b.ins().iconst(I8, 0)),
            Kind::True => Scalar::Bool(self.b.ins().iconst(I8, 1)),
        }
    }

    /// `held` written to `slot` with its type word, as php's `ZVAL_LONG`,
    /// `ZVAL_DOUBLE`, `ZVAL_BOOL` and `ZVAL_NULL` write one.
    fn store(&mut self, slot: Slot, held: Scalar) -> Result<(), Abort> {
        let (room, at) = self.place(slot)?;
        self.write(room, at, held);
        Ok(())
    }

    /// `held` written to the zval at `base + at` with its type word.
    fn write(&mut self, base: Value, at: i32, held: Scalar) {
        let info = match held {
            Scalar::Long(x) => {
                self.b.ins().store(trusted(), x, base, at);
                self.b.ins().iconst(I32, tag(ZvalType::Long))
            }
            Scalar::Double(x) => {
                self.b.ins().store(trusted(), x, base, at);
                self.b.ins().iconst(I32, tag(ZvalType::Double))
            }
            Scalar::Bool(b) => {
                let b = self.b.ins().uextend(I32, b);
                self.b.ins().iadd_imm_s(b, tag(ZvalType::False))
            }
            Scalar::Null => self.b.ins().iconst(I32, tag(ZvalType::Null)),
        };
        self.b.ins().store(trusted(), info, base, at + TYPE_AT);
    }

    /// The value `value` an op writes, as `held`: its register, or its
    /// slot.
    fn define(&mut self, value: ValueId, held: Scalar) -> Result<(), Abort> {
        let target = self.value(value)?;
        if target.home == Home::Memory {
            return self.store(target.slot, held);
        }
        let (var, kind) = self.register_var(value)?;
        match (kind, held) {
            (Kind::Long, Scalar::Long(x))
            | (Kind::Double, Scalar::Double(x))
            | (Kind::Bool, Scalar::Bool(x)) => {
                self.b.def_var(var, x);
                Ok(())
            }
            (Kind::Null, Scalar::Null) | (Kind::False | Kind::True, Scalar::Bool(_)) => Ok(()),
            _ => Err(mismatch("a register written with another kind")),
        }
    }

    /// The value `value` an arithmetic writes, as `num`: one that may be
    /// either kind goes to its slot with the type word it turned out.
    fn define_num(&mut self, value: ValueId, num: Num) -> Result<(), Abort> {
        match num {
            Num::Long(x) => self.define(value, Scalar::Long(x)),
            Num::Double(x) => self.define(value, Scalar::Double(x)),
            Num::Either {
                overflowed,
                long,
                double,
            } => {
                let target = self.value(value)?;
                if target.home == Home::Register {
                    return Err(mismatch("a register written with a number of either kind"));
                }
                let (room, at) = self.place(target.slot)?;
                let bits = self.b.ins().bitcast(I64, MemFlagsData::new(), double);
                let word = self.b.ins().select(overflowed, bits, long);
                let as_double = self.b.ins().iconst(I32, tag(ZvalType::Double));
                let as_long = self.b.ins().iconst(I32, tag(ZvalType::Long));
                let info = self.b.ins().select(overflowed, as_double, as_long);
                self.b.ins().store(trusted(), word, room, at);
                self.b.ins().store(trusted(), info, room, at + TYPE_AT);
                Ok(())
            }
        }
    }
}

/// Where slot `var` is in its frame's room: past php's
/// `ZEND_CALL_FRAME_SLOT`, sixteen bytes a slot.
fn slot_offset(var: u32) -> Result<i32, Abort> {
    i64::from(var)
        .checked_mul(SLOT)
        .and_then(|at| at.checked_add(i64::from(imm(ROOM_BYTES))))
        .and_then(|at| i32::try_from(at).ok())
        .ok_or_else(|| mismatch("a slot past an immediate"))
}

/// The engine offset that numbers an op array a trace holds; a
/// request-scoped one is never held (`jit-design.md` §4.1).
fn engine_offset(func: FuncRef) -> Result<u32, Abort> {
    match func.offset() {
        MapPtrOffset::Engine(n) => Ok(n),
        MapPtrOffset::Request(_) => Err(mismatch("a request-scoped op array in a trace")),
    }
}
