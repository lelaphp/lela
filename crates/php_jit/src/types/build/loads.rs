//! php's register allocator's loads at entry (`ZREG_LOAD`): which values
//! the root frame entered with php's code generator loads into a
//! register as the trace enters, and so holds to their recorded type
//! there, before any other exit of the trace's body, where a trace that
//! does not loop otherwise holds the type at the value's first read
//! (`Z/ext/opcache/jit/zend_jit_trace.c:4283-4310`). The decision is
//! `zend_jit_trace_allocate_registers`' over the trace's SSA
//! (`:2847-3374`): a live range opens at entry for every variable the
//! trace reads, of no alias and of a type a register holds, and closes
//! for good where an opline that reads it keeps no operand in a register
//! (`zend_jit_opline_supports_reg`, `Z/ext/opcache/jit/zend_jit_ir.c:17434-17611`),
//! where a call the trace follows is entered while it is still to be
//! read, or where it is read once alone. What survives is loaded. The
//! allocation itself, which registers and where they spill, is
//! Cranelift's and not ported. A side's parent may hold a value in a
//! register, which php's side reuses and loads nothing for (`:2896-2905`,
//! `:4291`); here an exit stores every register it holds, and its side
//! knows the type of what the exit stored, so no guard is held on it to
//! begin with. The same allocator decides where a property fetch php
//! compiles inline holds the type of what it fetched
//! ([`Builder::hold_in_fetch`]).

use php_heap::ZvalType;
use php_unit::{Opcode, OperandKind, Opline};

use super::{At, Builder, Left, Placed, Point, draft};
use crate::analysis::Defined;
use crate::record::Sample;
use crate::types::lattice::{self, Types};
use crate::types::opcodes::{ISEMPTY, MAX_ARG_FLAG_NUM};
use crate::types::{
    Check, Def, Form, Frame, Frees, GuardAt, Line, Op, OpId, Operand, Read, Slot, ValueId,
};

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

    /// Each guard [`Builder::place`] held before the op that reads what a
    /// stepped `FETCH_OBJ_R` wrote, held within the fetch instead where
    /// php's allocator keeps what the fetch writes in a register: a long
    /// or a double, of a fetch and read by an op whose code keeps it in
    /// one (`zend_jit_trace_allocate_registers`,
    /// `Z/ext/opcache/jit/zend_jit_trace.c:2958-3058`). php compiles a
    /// fetch by a constant name inline, of a property it finds no
    /// declaration of too, and where it writes a register,
    /// `zend_jit_fetch_obj` holds the type as it reads the property, in
    /// place and through the reference it may be, by two exits past the
    /// fetch (`Z/ext/opcache/jit/zend_jit_ir.c:14607-14687`, and
    /// `:8024-8090`, `zend_jit_guard_fetch_result_type`), where a fetch
    /// that writes memory is held by the op that reads it, by one. Here
    /// the fetch is the handler's, which writes no reference, and the
    /// guards read the slot it wrote; the exits land past it as php's
    /// do, with nothing to free, which the handler freed.
    ///
    /// A loop adding a dynamic property of a local `stdClass` to a sum
    /// takes php's two exits at the fetch, so the `$i > 50` side trace
    /// grows from php's exit 5 (#3884):
    ///
    /// ```php
    /// function f() { $o = new stdClass; $o->q = 1; $x = 0; $n = 0; for ($i = 0; $i < 100; $i++) { $n += $o->q; if ($i > 50) { $n++; } } return $n + $x; }
    /// echo f(), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 149
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
    ///      TRACE 1 exit 1 f() %s:2
    /// ---- TRACE 3 start (side trace 1/1) f() %s:2
    /// ---- TRACE 3 stop (return)
    /// ---- TRACE 3 compiled
    /// ```
    pub(super) fn hold_in_fetch(&mut self) {
        for at in 0..self.guards.len() {
            let Some(placed) = self.guards.get(at).copied() else {
                continue;
            };
            let (GuardAt::Before(reader), Check::Type { value, tag }) = (placed.at, placed.check)
            else {
                continue;
            };
            let Def::Op(fetch) = self.val(value).def else {
                continue;
            };
            let (Some(fetched), Some(read)) = (self.line_at(fetch), self.line_at(reader)) else {
                continue;
            };
            let held = matches!(tag, ZvalType::Long | ZvalType::Double)
                && fetched.opcode == Opcode::FetchObjR
                && fetched.form == Form::Step
                && fetched.out == Some(value)
                && self.supports_reg(fetch, fetched)
                && self.supports_reg(reader, read);
            if !held {
                continue;
            }
            let (frame, opline) = (fetched.frame, fetched.opline);
            let past = |left| {
                let at = At::Fetched {
                    fetch: opline,
                    frees: Frees::NONE,
                    out: value,
                    left,
                };
                draft(Point::Before(fetch), frame, at)
            };
            let (in_place, through) = (
                past(Left::Copied(Read::InPlace)),
                past(Left::Copied(Read::Through)),
            );
            self.guards[at] = Placed {
                at: GuardAt::Within(fetch),
                check: placed.check,
                exit: in_place,
            };
            self.guards.push(Placed {
                at: GuardAt::Within(fetch),
                check: Check::Deref { value, tag },
                exit: through,
            });
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
        // The uses of the value as php's trace SSA numbers them, up to
        // the op that writes its slot anew, after which a use is another
        // variable's; an opline and its `OP_DATA` are two ops there.
        let mut uses: Vec<Use> = Vec::new();
        let mut ends = None;
        for (at, op) in self.ops_by_id() {
            let Op::Line(line) = op else {
                continue;
            };
            let reads = |operand: Operand| operand == Operand::Value(value);
            if reads(line.op1) || reads(line.op2) {
                uses.push(Use {
                    at,
                    part: Part::Opline,
                });
            }
            if reads(line.data) {
                uses.push(Use {
                    at,
                    part: Part::Data,
                });
            }
            if self.writes_anew(line, slot) {
                ends = Some(at);
                break;
            }
        }
        // An interval opens only where the first use reads the value.
        match uses.first() {
            Some(first) if !self.no_val_at(*first, value) => {}
            _ => return false,
        }
        let mut current = Frame::ROOT;
        let mut last_use = false;
        for (at, op) in self.ops_by_id() {
            match op {
                // Entering a call frees every register of the frame it
                // leaves whose value is still to be read.
                Op::Enter { frame, .. } => {
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
                    if !uses.iter().any(|used| used.at == at) {
                        continue;
                    }
                    if !self.supports_reg(at, line) {
                        return false;
                    }
                    for (k, used) in uses.iter().enumerate().filter(|(_, used)| used.at == at) {
                        if !self.no_val_at(*used, value) {
                            let next = uses.get(k + 1).copied();
                            last_use = self.last_use(line, used.part, value, next);
                        }
                    }
                }
            }
        }
        // A range read once, and that its last, is no register's.
        !(last_use && uses.len() == 1)
    }

    /// Whether `used` names the value without reading it.
    fn no_val_at(&self, used: Use, value: ValueId) -> bool {
        used.part == Part::Opline
            && self
                .line_at(used.at)
                .is_some_and(|line| no_val_use(line, value))
    }

    /// The trace's ops, each by its place.
    fn ops_by_id(&self) -> impl Iterator<Item = (OpId, &Op)> {
        (0..).map(OpId).zip(&self.ops)
    }

    /// The line at `at`, where the op there is one.
    fn line_at(&self, at: OpId) -> Option<&Line> {
        match self.ops.get(at.index()) {
            Some(Op::Line(line)) => Some(line),
            _ => None,
        }
    }

    /// Which of a line's operands, `[op1, op2, OP_DATA's op1]`, php's
    /// trace SSA writes anew, as `_zend_ssa_rename_op` numbers them under
    /// `ZEND_SSA_RC_INFERENCE`, which `zend_jit_trace_build_tssa` builds
    /// with (`Z/ext/opcache/jit/zend_jit_trace.c:1331`,
    /// `Z/Zend/Optimizer/zend_ssa.c:544-800`): what the opline writes, and
    /// a variable it reads and may take a counted owner of, a sent or
    /// copied variable and an assigned one among them, and the variable a
    /// closure's `use` binds, by value as by reference (`:754-760`).
    ///
    /// A loop whose `+=` a closure captures by value after it, #3721's
    /// program: the back edge brings the `BIND_LEXICAL`'s `$c`, which
    /// `is_checked_guard` holds nothing of, so the phi keeps its guard at
    /// the header, and the `$i > 50` side trace grows from php's exit:
    ///
    /// ```php
    /// function f() { $c = 0; $g = null; for ($i = 0; $i < 100; $i++) { $c += 2; $g = function () use ($c) { return $c; }; if ($i > 50) { $c++; } } return $c + $g(); }
    /// echo f(), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 497
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
    pub(super) fn defines(&self, line: &Line) -> [bool; 3] {
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
            || (cv(op.op2_type)
                && (frameless || matches!(line.opcode, Opcode::Assign | Opcode::BindLexical)));
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
    pub(super) fn writes_anew(&self, line: &Line, slot: Slot) -> bool {
        let in_slot = |operand: Operand| matches!(operand, Operand::Value(held) if self.val(held).slot == slot);
        let [op1, op2, data] = self.defines(line);
        (op1 && in_slot(line.op1))
            || (op2 && in_slot(line.op2))
            || (data && in_slot(line.data))
            || line.out.is_some_and(|out| self.val(out).slot == slot)
    }

    /// Whether the use of `value` at `part` of `line` is its last in a
    /// register, `ZREG_LAST_USE`, as `zend_jit_trace_use_var` marks it
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:2828-2856`) on an interval
    /// loaded at entry: the op writes the variable anew, or no use
    /// follows; where the next use writes over it without reading it,
    /// php's analysis of the op array says (`zend_ssa_is_last_use`,
    /// [`crate::analysis::Analysed::is_last_use`]), and no mark where it
    /// has no SSA.
    fn last_use(&self, line: &Line, part: Part, value: ValueId, next: Option<Use>) -> bool {
        let at = |operand: Operand| operand == Operand::Value(value);
        let [op1, op2, data] = self.defines(line);
        let redefined = match part {
            Part::Opline => (at(line.op1) && op1) || (at(line.op2) && op2),
            Part::Data => data,
        };
        match next {
            _ if redefined => return true,
            None => return true,
            Some(next) if !self.no_val_at(next, value) => return false,
            Some(_) => {}
        }
        let (opline, which) = match part {
            Part::Opline if at(line.op1) => (line.opline.0, Defined::Op1),
            Part::Opline => (line.opline.0, Defined::Op2),
            Part::Data => (line.opline.0 + 1, Defined::Op1),
        };
        let func = self.frames[usize::from(line.frame.0)].func;
        let code = self.meta(line.frame).code;
        (self.analysed.is_last_use(func, code, opline, which)).unwrap_or(false)
    }

    /// The opline a line serves.
    pub(super) fn opline_of(&self, line: &Line) -> Option<Opline> {
        let code = self.meta(line.frame).code;
        code.oplines.get(line.opline.0 as usize).copied()
    }

    /// php's `zend_jit_opline_supports_reg` over the types the trace
    /// holds of the opline's operands and the ones its recording saw:
    /// whether its code reads an operand from a register.
    fn supports_reg(&self, at: OpId, line: &Line) -> bool {
        let Some(op) = self.opline_of(line) else {
            return false;
        };
        let samples = self.sampled.get(&at).copied().unwrap_or([None; 3]);
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
            // An assignment to a variable a symbol table reaches, which
            // other code may write, keeps nothing in a register
            // (`Z/ext/opcache/jit/zend_jit_trace.c:2950-2957`).
            Opcode::Assign => {
                cv1 && !line
                    .op1_def
                    .is_some_and(|def| self.aliased(self.val(def).slot))
            }
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
        if line.opcode == Opcode::AssignDimOp && !self.compound_in_reg(line, op) {
            return false;
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

    /// A compound assignment to an element or a property: its `out` slot
    /// unused and its operator one php's code keeps in registers over
    /// anything and the `OP_DATA`'s value.
    fn compound_in_reg(&self, line: &Line, op: &Opline) -> bool {
        let td = self.types_of(line.frame, line.data);
        op.out_type == OperandKind::Unused
            && lattice::binary_of(op.extended_value)
                .is_some_and(|binary| supported_binary_op(binary, Types::ANY, td))
    }

    /// `zend_jit_opline_supports_reg`'s arm for a property by a constant
    /// name: of `$this` or of what may be an object; a compound write
    /// only over an operator php's code keeps in registers.
    fn property_in_reg(&self, line: &Line, op: &Opline) -> bool {
        if line.opcode == Opcode::AssignObjOp && !self.compound_in_reg(line, op) {
            return false;
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

/// One use of a value in php's trace SSA: the op at its place, or the
/// `OP_DATA` after it, which the SSA numbers as an op of its own.
#[derive(Debug, Clone, Copy)]
struct Use {
    at: OpId,
    part: Part,
}

/// Which of a line's two ops in php's trace SSA a use is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Opline,
    Data,
}
