//! php's register allocator's loads at entry (`ZREG_LOAD`): which values
//! the root frame entered with php's code generator loads into a
//! register as the trace enters, and so holds to their recorded type
//! there, before any other exit of the trace's body, where a trace that
//! does not loop otherwise holds the type at the value's first read
//! (`Z/ext/opcache/jit/zend_jit_trace.c:4283-4310`). The decision is
//! `zend_jit_trace_allocate_registers`' over the trace's SSA
//! (`:2847-3380`): a live range opens at entry for every variable the
//! trace reads, of no alias and of a type a register holds, and closes
//! for good where an opline that reads it keeps no operand in a register
//! (`zend_jit_opline_supports_reg`, `Z/ext/opcache/jit/zend_jit_ir.c:17434-17611`),
//! where a call the trace follows is entered while it is still to be
//! read, or where it is read once alone. What survives is loaded. The
//! allocation itself, which registers and where they spill, is
//! Cranelift's and not ported.

use php_engine::tier::Sample;
use php_heap::ZvalType;
use php_unit::{Opcode, OperandKind, Opline};

use super::{Builder, Placed};
use crate::types::lattice::{self, Types};
use crate::types::{Check, Frame, GuardAt, Line, Op, Operand, Slot, ValueId};

/// `MAX_ARG_FLAG_NUM` (`Z/Zend/zend_compile.h:1165`): the arguments whose
/// way of being sent a function's flags carry.
const MAX_ARG_FLAG_NUM: u32 = 12;

/// php's `ZEND_ISEMPTY`.
const ISEMPTY: u32 = 1 << 0;

impl Builder<'_> {
    /// In a trace that does not loop, each guard [`Builder::place`] held
    /// at the first read of what the root frame entered with, held at
    /// entry instead where php's allocator loads the value there.
    pub(super) fn load_at_entry(&mut self) {
        for at in std::mem::take(&mut self.entry_reads) {
            let Some(placed) = self.guards.get(at).copied() else {
                continue;
            };
            let Check::Type { value, tag } = placed.check else {
                continue;
            };
            if self.loads(value, tag) {
                self.guards[at] = Placed {
                    at: GuardAt::Entry,
                    exit: self.start(),
                    ..placed
                };
            }
        }
    }

    /// Whether php's allocator loads `value`, which the root frame
    /// entered with and a guard holds to `tag`, at the trace's entry: its
    /// live range opens there and survives to the end of the walk
    /// (`zend_jit_trace_allocate_registers`).
    fn loads(&self, value: ValueId, tag: ZvalType) -> bool {
        let slot = self.val(value).slot;
        // `zend_jit_var_supports_reg`: a long or a double, which the guard
        // makes it; and `NO_ALIAS`.
        if !matches!(tag, ZvalType::Long | ZvalType::Double) || self.aliased(slot) {
            return false;
        }
        // The ops that use the value as php's trace SSA numbers it, up to
        // the one that writes its slot anew, after which a use is another
        // variable's.
        let reads = |line: &Line| [line.op1, line.op2, line.data].contains(&Operand::Value(value));
        let mut uses: Vec<usize> = Vec::new();
        let mut ends = None;
        for (at, op) in self.ops.iter().enumerate() {
            let Op::Line(line) = op else {
                continue;
            };
            if reads(line) {
                uses.push(at);
            }
            if self.writes_anew(line, slot) {
                ends = Some(at);
                break;
            }
        }
        let line_at = |at: usize| match self.ops.get(at) {
            Some(Op::Line(line)) => Some(line),
            _ => None,
        };
        // An interval opens only where the first use reads the value.
        match uses.first().and_then(|&at| line_at(at)) {
            Some(first) if !no_val_use(first, value) => {}
            _ => return false,
        }
        let mut current = Frame::ROOT;
        let mut last_use = false;
        for (at, op) in self.ops.iter().enumerate() {
            match op {
                // Entering a call frees every register of the frame it
                // leaves whose value is still to be read.
                Op::Enter { frame } => {
                    let live = ends.is_none_or(|ends| at < ends);
                    if current == Frame::ROOT && live && !last_use {
                        return false;
                    }
                    current = *frame;
                }
                Op::Back { frame, .. } => current = *frame,
                Op::InitCall { .. } => {}
                Op::Line(line) => {
                    current = line.frame;
                    let Some(k) = uses.iter().position(|&used| used == at) else {
                        continue;
                    };
                    if !self.supports_reg(at, line) {
                        return false;
                    }
                    if !no_val_use(line, value) {
                        let next = uses.get(k + 1).and_then(|&next| line_at(next));
                        last_use = self.last_use(line, value, next);
                    }
                }
            }
        }
        // A range read once, and that its last, is no register's.
        !(last_use && uses.len() == 1)
    }

    /// Which of a line's operands, `[op1, op2, OP_DATA's op1]`, php's
    /// trace SSA writes anew, as `_zend_ssa_rename_op` numbers them under
    /// `ZEND_SSA_RC_INFERENCE`, which `zend_jit_trace_build_tssa` builds
    /// with (`Z/ext/opcache/jit/zend_jit_trace.c:1331`,
    /// `Z/Zend/Optimizer/zend_ssa.c:544-800`): what the opline writes, and
    /// a variable it reads and may take a counted owner of, a sent or
    /// copied variable and an assigned one among them.
    fn defines(&self, line: &Line) -> [bool; 3] {
        let Some(op) = self.opline_of(line) else {
            return [false; 3];
        };
        let cv = |kind: OperandKind| kind == OperandKind::Cv;
        let frameless = matches!(
            line.opcode,
            Opcode::FramelessIcall1 | Opcode::FramelessIcall2 | Opcode::FramelessIcall3
        );
        let op1 = line.op1_def.is_some()
            || line.opcode == Opcode::CopyTmp
            || (cv(op.op1_type)
                && (frameless
                    || matches!(
                        line.opcode,
                        Opcode::SendVar
                            | Opcode::Cast
                            | Opcode::QmAssign
                            | Opcode::JmpSet
                            | Opcode::Coalesce
                            | Opcode::FeResetR
                            | Opcode::InitArray
                            | Opcode::AddArrayElement
                            | Opcode::Yield
                    )));
        let op2 = line.op2_def.is_some()
            || (cv(op.op2_type) && (frameless || line.opcode == Opcode::Assign));
        let data = matches!(
            line.opcode,
            Opcode::AssignDim
                | Opcode::AssignObj
                | Opcode::AssignStaticProp
                | Opcode::AssignObjRef
                | Opcode::AssignStaticPropRef
                | Opcode::FramelessIcall3
        ) && self
            .opline_of_data(line)
            .is_some_and(|data| cv(data.op1_type));
        [op1, op2, data]
    }

    /// Whether `line` writes `slot` anew in php's trace SSA: an operand
    /// in it that it [`Builder::defines`], or its `out` slot.
    fn writes_anew(&self, line: &Line, slot: Slot) -> bool {
        let in_slot = |operand: Operand| matches!(operand, Operand::Value(held) if self.val(held).slot == slot);
        let [op1, op2, data] = self.defines(line);
        (op1 && in_slot(line.op1))
            || (op2 && in_slot(line.op2))
            || (data && in_slot(line.data))
            || line.out.is_some_and(|out| self.val(out).slot == slot)
    }

    /// Whether the use of `value` at `line` is its last in a register,
    /// `ZREG_LAST_USE` (`zend_jit_trace_use_var` and the operands' kinds
    /// in `zend_jit_trace_allocate_registers`): the opline writes the
    /// variable anew, or no use follows, or the next writes over it
    /// without reading it, where php asks its function-level SSA
    /// (`zend_ssa_is_last_use`), which this tier does not keep (#3149);
    /// and a temporary is read once, but where a `foreach`, a `switch`
    /// or a rope keeps it beyond the opline.
    fn last_use(&self, line: &Line, value: ValueId, next: Option<&Line>) -> bool {
        let at = |operand: Operand| operand == Operand::Value(value);
        let [op1, op2, data] = self.defines(line);
        let redefined = (at(line.op1) && op1) || (at(line.op2) && op2) || (at(line.data) && data);
        if redefined || next.is_none_or(|next| no_val_use(next, value)) {
            return true;
        }
        let Some(op) = self.opline_of(line) else {
            return false;
        };
        let kept = matches!(
            line.opcode,
            Opcode::Case
                | Opcode::CaseStrict
                | Opcode::SwitchLong
                | Opcode::Match
                | Opcode::FetchListR
                | Opcode::CopyTmp
                | Opcode::SwitchString
                | Opcode::FeFetchR
                | Opcode::FeFetchRw
                | Opcode::FetchListW
                | Opcode::VerifyReturnType
                | Opcode::BindLexical
                | Opcode::RopeAdd
        );
        (at(line.op1) && op.op1_type != OperandKind::Cv && !kept)
            || (at(line.op2) && op.op2_type != OperandKind::Cv)
            || (at(line.data)
                && (self.opline_of_data(line)).is_some_and(|data| data.op1_type != OperandKind::Cv))
    }

    /// The opline a line serves.
    fn opline_of(&self, line: &Line) -> Option<Opline> {
        let code = self.meta(line.frame).code;
        code.oplines.get(line.opline.0 as usize).copied()
    }

    /// php's `zend_jit_opline_supports_reg` over the types the trace
    /// holds of the opline's operands and the ones its recording saw:
    /// whether its code reads an operand from a register.
    fn supports_reg(&self, at: usize, line: &Line) -> bool {
        let Some(op) = self.opline_of(line) else {
            return false;
        };
        let samples = self.sampled.get(at).copied().flatten().unwrap_or([None; 3]);
        let frame = line.frame;
        let (t1, t2) = (
            self.types_of(frame, line.op1),
            self.types_of(frame, line.op2),
        );
        let cv1 = op.op1_type == OperandKind::Cv;
        let cv_or_const = matches!(op.op1_type, OperandKind::Cv | OperandKind::Const);
        let only = |kind: Types| t1.kinds() == kind && !t1.meets(Types::REF);
        match line.opcode {
            Opcode::SendVar | Opcode::SendVal | Opcode::SendValEx => {
                op.op2_type != OperandKind::Const
                    && (line.opcode != Opcode::SendValEx || op.op2 <= MAX_ARG_FLAG_NUM)
            }
            Opcode::QmAssign
            | Opcode::IsSmaller
            | Opcode::IsSmallerOrEqual
            | Opcode::IsEqual
            | Opcode::IsNotEqual
            | Opcode::IsIdentical
            | Opcode::IsNotIdentical
            | Opcode::Case
            | Opcode::Jmpz
            | Opcode::Jmpnz
            | Opcode::Bool
            | Opcode::BoolNot
            | Opcode::JmpzEx
            | Opcode::JmpnzEx
            | Opcode::FetchConstant => true,
            // A file's own code returns to no function.
            Opcode::Return => !self
                .view
                .is_main(self.frames[usize::from(frame.0)].func)
                .unwrap_or(true),
            Opcode::Assign => cv1,
            Opcode::AssignOp => {
                cv1 && op.out_type == OperandKind::Unused
                    && lattice::binary_of(op.extended_value)
                        .is_some_and(|binary| supported_binary_op(binary, t1, t2))
            }
            Opcode::Add
            | Opcode::Sub
            | Opcode::Mul
            | Opcode::BwOr
            | Opcode::BwAnd
            | Opcode::BwXor
            | Opcode::Sl
            | Opcode::Sr
            | Opcode::Mod => arithmetic_in_reg(line.opcode, t1, t2, samples),
            Opcode::PreInc | Opcode::PreDec | Opcode::PostInc | Opcode::PostDec => {
                cv1 && t1.meets(Types::LONG) && !t1.meets(Types::REF)
            }
            Opcode::Strlen => cv_or_const && only(Types::STRING),
            Opcode::Count => cv_or_const && only(Types::ARRAY),
            Opcode::IssetIsemptyDimObj
            | Opcode::FetchDimR
            | Opcode::FetchDimIs
            | Opcode::FetchListR => {
                (line.opcode != Opcode::IssetIsemptyDimObj || op.extended_value & ISEMPTY == 0)
                    && by_key(t1, t2, samples[0])
            }
            Opcode::AssignDimOp
            | Opcode::AssignDim
            | Opcode::FetchDimW
            | Opcode::FetchDimRw
            | Opcode::FetchListW => self.dim_write_in_reg(line, &op, samples),
            Opcode::AssignObjOp | Opcode::FetchObjR | Opcode::AssignObj => {
                self.property_in_reg(line, &op)
            }
            _ => false,
        }
    }

    /// `zend_jit_opline_supports_reg`'s arm for a write to an element:
    /// a variable's array, or a `VAR` the recording saw indirect, by a
    /// long or a string key, `$a[x] = $a` aside; a compound one only
    /// over an operator php's code keeps in registers.
    fn dim_write_in_reg(&self, line: &Line, op: &Opline, samples: [Option<Sample>; 3]) -> bool {
        let (t1, t2) = (
            self.types_of(line.frame, line.op1),
            self.types_of(line.frame, line.op2),
        );
        let unused = op.out_type == OperandKind::Unused;
        if line.opcode == Opcode::AssignDimOp {
            let td = self.types_of(line.frame, line.data);
            let served = lattice::binary_of(op.extended_value)
                .is_some_and(|binary| supported_binary_op(binary, Types::ANY, td));
            if !unused || !served {
                return false;
            }
        }
        let self_assigned = matches!(line.opcode, Opcode::AssignDim | Opcode::AssignDimOp)
            && (self.opline_of_data(line))
                .is_some_and(|data| data.op1_type == OperandKind::Cv && data.op1 == op.op1);
        let var = op.op1_type == OperandKind::Var
            && (samples[0].is_none_or(|sample| !sample.indirect) || !unused);
        if (op.op1_type == OperandKind::Cv && self_assigned) || var {
            return false;
        }
        by_key(t1, t2, samples[0])
    }

    /// `zend_jit_opline_supports_reg`'s arm for a property by a constant
    /// name: of `$this` or of what may be an object; a compound write
    /// only over an operator php's code keeps in registers.
    fn property_in_reg(&self, line: &Line, op: &Opline) -> bool {
        if line.opcode == Opcode::AssignObjOp {
            let td = self.types_of(line.frame, line.data);
            let served = lattice::binary_of(op.extended_value)
                .is_some_and(|binary| supported_binary_op(binary, Types::ANY, td));
            if op.out_type != OperandKind::Unused || !served {
                return false;
            }
        }
        let named = op.op2_type == OperandKind::Const
            && (self.meta(line.frame).code.literals.get(op.op2 as usize))
                .and_then(|name| name.str_bytes())
                .is_some_and(|name| name.first().is_some_and(|byte| *byte != 0));
        named
            && (op.op1_type == OperandKind::Unused
                || self.types_of(line.frame, line.op1).meets(Types::OBJECT))
    }

    /// The `OP_DATA` after a line's opline.
    fn opline_of_data(&self, line: &Line) -> Option<Opline> {
        let code = self.meta(line.frame).code;
        (code.oplines.get(line.opline.0 as usize + 1).copied())
            .filter(|data| data.opcode == Opcode::OpData)
    }
}

/// php's `zend_jit_supported_binary_op`
/// (`Z/ext/opcache/jit/zend_jit.c:1390-1416`): whether its code keeps a
/// compound assignment's operands in registers.
fn supported_binary_op(op: Opcode, t1: Types, t2: Types) -> bool {
    if t1.meets(Types::UNDEF) || t2.meets(Types::UNDEF) {
        return false;
    }
    match op {
        Opcode::Add | Opcode::Sub | Opcode::Mul => {
            t1.meets(Types::NUMBER) && t2.meets(Types::NUMBER)
        }
        Opcode::BwOr | Opcode::BwAnd | Opcode::BwXor | Opcode::Sl | Opcode::Sr | Opcode::Mod => {
            t1.meets(Types::LONG) && t2.meets(Types::LONG)
        }
        Opcode::Concat => t1.meets(Types::STRING) && t2.meets(Types::STRING),
        _ => false,
    }
}

/// php's `zend_ssa_is_no_val_use`: an opline that names `value` and does
/// not read it, an assignment over it or a `foreach` writing it.
fn no_val_use(line: &Line, value: ValueId) -> bool {
    let at = |operand: Operand| operand == Operand::Value(value);
    match line.opcode {
        Opcode::Assign | Opcode::UnsetCv | Opcode::BindGlobal | Opcode::BindStatic => {
            at(line.op1) && !at(line.op2)
        }
        Opcode::FeFetchR | Opcode::FeFetchRw => at(line.op2) && !at(line.op1),
        _ => false,
    }
}

/// `zend_jit_opline_supports_reg`'s arm for an arithmetic: both operands
/// of the kinds its code keeps in registers, narrowed to what the
/// recording saw (`info &= 1 << trace->opN_type`), and neither undefined
/// for an addition, a subtraction or a product.
fn arithmetic_in_reg(opcode: Opcode, t1: Types, t2: Types, samples: [Option<Sample>; 3]) -> bool {
    let seen = |types: Types, sample: Option<Sample>| {
        sample.map_or(types, |sample| types.and(Types::of_tag(sample.kind)))
    };
    let (s1, s2) = (seen(t1, samples[0]), seen(t2, samples[1]));
    match opcode {
        Opcode::Add | Opcode::Sub | Opcode::Mul => {
            ![t1, t2, s1, s2].iter().any(|t| t.meets(Types::UNDEF))
                && s1.meets(Types::NUMBER)
                && s2.meets(Types::NUMBER)
        }
        _ => s1.meets(Types::LONG) && s2.meets(Types::LONG),
    }
}

/// An array read or written by a long or a string key, the array's kinds
/// narrowed to an array where the recording saw one
/// (`info &= ~((MAY_BE_ANY|MAY_BE_UNDEF) - MAY_BE_ARRAY)`).
fn by_key(t1: Types, t2: Types, sample: Option<Sample>) -> bool {
    let t1 = match sample {
        Some(sample) if sample.kind == ZvalType::Array => {
            t1.without(Types::ANY.or(Types::UNDEF).kinds().without(Types::ARRAY))
        }
        _ => t1,
    };
    t1.kinds() == Types::ARRAY && (t2.kinds() == Types::LONG || t2.kinds() == Types::STRING)
}
