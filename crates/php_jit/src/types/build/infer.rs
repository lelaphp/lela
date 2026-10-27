//! What each opline of the trace writes, as `zend_jit_trace_build_tssa`
//! learns it (`Z/ext/opcache/jit/zend_jit_trace.c:2286-2470`): the range
//! of each variable it defines, `zend_jit_trace_copy_ssa_var_range`'s
//! `zend_inference_propagate_range` met with what php's analysis of the
//! op array knows; then the types, php's `zend_update_type_info`; then
//! both narrowed by that analysis, `zend_jit_trace_restrict_ssa_var_info`.
//! The trace's SSA they read is, for each opline, the opline
//! and its `OP_DATA` with what the values it reads hold, and the opline
//! after it where that one reads what it defines, which is all
//! `zend_update_type_info` reads of the SSA; the inference is `php_ssa`'s,
//! the one function that learns it for an op array's own SSA.

use php_engine::tier::{ClassId, FuncRef, Sample};
use php_heap::ZvalType;
use php_ssa::{Alias, Position, Range, Ssa, SsaOp, SsaVar, SsaVarInfo, Var, VarInfo};
use php_unit::{OpArray, Opcode, OperandKind, Opline, OplineIdx};

use super::Builder;
use crate::analysis::Defined;
use crate::record::Event;
use crate::types::lattice::Types;
use crate::types::opcodes::{slot_of, writes};
use crate::types::{Frame, Operand};

/// What an opline writes to each variable it defines, where it defines
/// it: its `out`, its `op1` and `op2`, and its `OP_DATA`'s `op1`.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Inferred {
    pub(super) out: Option<Held>,
    pub(super) op1: Option<Held>,
    pub(super) op2: Option<Held>,
    pub(super) data: Option<Held>,
}

/// What a variable holds: its types, and the range of the long it holds
/// where php's inference knows one.
#[derive(Debug, Clone, Copy)]
pub(super) struct Held {
    pub(super) types: Types,
    pub(super) range: Option<Range>,
}

/// The operands one opline reads, as the builder holds them, with the
/// types its guards held each to.
pub(super) struct Reads<'r> {
    pub(super) op: &'r Opline,
    pub(super) data: Option<&'r Opline>,
    pub(super) operands: [Operand; 3],
    pub(super) types: [Types; 3],
    pub(super) samples: [Option<Sample>; 3],
}

/// One opline's trace SSA as it is built: the variables, what each
/// holds, and the ops.
struct Tssa {
    vars: Vec<SsaVarInfo>,
    info: Vec<VarInfo<ClassId>>,
    ops: Vec<SsaOp>,
}

impl Tssa {
    fn var(
        &mut self,
        var: u32,
        alias: Alias,
        definition: Option<u32>,
        info: VarInfo<ClassId>,
    ) -> SsaVar {
        let id = SsaVar(u32::try_from(self.vars.len()).unwrap_or(u32::MAX));
        self.vars.push(SsaVarInfo {
            var: Var(var),
            scc: None,
            definition,
            definition_phi: None,
            use_chain: None,
            phi_use_chain: None,
            sym_use_chain: None,
            no_val: false,
            scc_entry: false,
            alias,
        });
        self.info.push(info);
        id
    }
}

/// The place past every op of one opline's trace SSA: a use there is one
/// no op reads next, as a use later in the trace is.
const LATER: u32 = u32::MAX;

impl Builder<'_> {
    /// What the opline `at` of `frame` writes, as php's inference over
    /// the trace learns it, `next` the event that follows it.
    pub(super) fn inferred(
        &self,
        frame: Frame,
        at: OplineIdx,
        reads: &Reads<'_>,
        next: Option<&Event>,
    ) -> Inferred {
        let func = self.frames[usize::from(frame.0)].func;
        let Reads { op, data, .. } = *reads;
        let written = writes(op, data);
        let [t1, t2, _] = reads.types;
        let (mut tssa, positions) = self.trace_ssa(frame, at, reads, next);
        self.copy_ranges(func, at, &mut tssa, &positions, data.is_some());
        let exits = matches!(op.opcode, Opcode::FeFetchR | Opcode::FeFetchRw)
            && op.jump_target().is_some_and(|target| {
                matches!(next, Some(Event::Op { func: f, opline, .. }) if *f == func && *opline == target)
            });
        if exits {
            // php leaves a `FE_FETCH`'s variables as they were where the
            // trace leaves the loop there; they are read nowhere after.
            if let (Some(def), Some(use_)) = (tssa.ops[0].op2_def, tssa.ops[0].op2_use) {
                tssa.info[def.index()] = tssa.info[use_.index()];
            }
        } else if !self.inference.update(func, &mut tssa, &positions, 0) {
            return unknown(op, data, written);
        }
        // A packed array whose element is written in place stays packed
        // (`zend_jit_trace.c:2357-2372`).
        if op.opcode == Opcode::AssignDimOp
            && let Some(def) = tssa.ops[0].op1_def
            && t1.tag() == Some(ZvalType::Array)
            && reads.samples[0].is_some_and(|s| s.packed)
            && reads.samples[2].is_some_and(|s| s.kind != ZvalType::Undef)
            && long_key(op, self.meta(frame).code, t2)
        {
            let info = &mut tssa.info[def.index()];
            info.types = info
                .types
                .without(php_ssa::Types::NUMERIC_HASH | php_ssa::Types::STRING_HASH);
        }
        let held = |var: Option<SsaVar>, which: Defined, i: u32, restrict: bool| {
            let info = tssa.info.get(var?.index())?;
            let mut held = Held {
                types: Types::of_info(info.types),
                range: info.range,
            };
            if restrict && let Some(known) = self.analysed.defined(func, at.0 + i, which) {
                // `zend_jit_trace_restrict_ssa_var_info`.
                held.types = held.types.restricted(known.types);
                if let Some(range) = known.range {
                    held.range = Some(held.range.map_or(range, |r| met(r, range)));
                }
            }
            Some(held)
        };
        let main = tssa.ops[0];
        Inferred {
            out: held(main.out_def, Defined::Out, 0, true),
            // php's `SEND_VAR_EX` of a value sent by value keeps what it
            // read, unrestricted.
            op1: held(
                main.op1_def,
                Defined::Op1,
                0,
                op.opcode != Opcode::SendVarEx,
            ),
            op2: held(main.op2_def, Defined::Op2, 0, !exits),
            data: tssa
                .ops
                .get(1)
                .filter(|_| data.is_some())
                .and_then(|d| held(d.op1_def, Defined::Op1, 1, true)),
        }
    }

    /// The trace's SSA php's inference reads at the opline `at` of
    /// `frame`: the opline, its `OP_DATA`, and the opline `next` where it
    /// reads what this one defines; and where each stands, php's
    /// `ssa_opcodes`.
    fn trace_ssa(
        &self,
        frame: Frame,
        at: OplineIdx,
        reads: &Reads<'_>,
        next: Option<&Event>,
    ) -> (Ssa<ClassId>, Vec<Position>) {
        let func = self.frames[usize::from(frame.0)].func;
        let Reads { op, data, .. } = *reads;
        let written = writes(op, data);
        let array = u32::from(frame.0);
        let mut tssa = Tssa {
            vars: Vec::new(),
            info: Vec::new(),
            ops: Vec::new(),
        };
        let alias = |var: u32| {
            let slot = crate::types::Slot { frame, var };
            if self.aliased(slot) {
                Alias::Symtable
            } else {
                Alias::None
            }
        };
        let used =
            |tssa: &mut Tssa, kind: OperandKind, field: u32, operand: Operand, types: Types| {
                if !matches!(kind, OperandKind::Cv | OperandKind::Tmp | OperandKind::Var) {
                    return None;
                }
                let range = match operand {
                    Operand::Value(value) => self.val(value).range,
                    _ => None,
                };
                let info = VarInfo {
                    types: types.info(),
                    class: None,
                    range,
                };
                Some(tssa.var(field, alias(field), None, info))
            };
        let [o1, o2, od] = reads.operands;
        let [t1, t2, td] = reads.types;
        let mut main = SsaOp {
            op1_use: used(&mut tssa, op.op1_type, op.op1, o1, t1),
            op2_use: used(&mut tssa, op.op2_type, op.op2, o2, t2),
            ..SsaOp::default()
        };
        // php's SSA reads an opline's `out` where it is a CV, or the
        // array an `ADD_ARRAY_*` adds to.
        let reads_out = (op.out_type == OperandKind::Cv && op.opcode != Opcode::Recv)
            || matches!(op.opcode, Opcode::AddArrayElement | Opcode::AddArrayUnpack);
        if reads_out {
            let (operand, types) = self.held_in(frame, op.out);
            main.out_use = used(&mut tssa, op.out_type, op.out, operand, types);
        }
        let defined = |tssa: &mut Tssa, kind: OperandKind, field: u32, at: u32| {
            slot_of(frame, kind, field)
                .map(|_| tssa.var(field, alias(field), Some(at), VarInfo::default()))
        };
        let pushes = pushes_out(op);
        if !pushes {
            main.out_def = defined(&mut tssa, op.out_type, op.out, 0);
        }
        if written.op1 {
            main.op1_def = defined(&mut tssa, op.op1_type, op.op1, 0);
        }
        if written.op2 {
            main.op2_def = defined(&mut tssa, op.op2_type, op.op2, 0);
        }
        tssa.ops.push(main);
        let mut positions = vec![Position {
            array,
            opline: at.0,
        }];
        if let Some(data) = data {
            let mut data_op = SsaOp {
                op1_use: used(&mut tssa, data.op1_type, data.op1, od, td),
                ..SsaOp::default()
            };
            if written.data {
                data_op.op1_def = defined(&mut tssa, data.op1_type, data.op1, 1);
            }
            tssa.ops.push(data_op);
            positions.push(Position {
                array,
                opline: at.0 + 1,
            });
        }
        self.link_next(&mut tssa, &mut positions, func, next, array);
        let tssa = Ssa {
            phis: Vec::new(),
            block_phis: Vec::new(),
            ops: tssa.ops,
            vars: tssa.vars,
            sccs: 0,
            info: tssa.info,
        };
        (tssa, positions)
    }

    /// Each variable the opline at `at` of `func` defines given its
    /// range before php's update reads it:
    /// `zend_jit_trace_copy_ssa_var_range`, the propagated range met with
    /// what php's analysis of the op array knows, or the propagation
    /// alone where that analysis has none.
    fn copy_ranges(
        &self,
        func: FuncRef,
        at: OplineIdx,
        tssa: &mut Ssa<ClassId>,
        positions: &[Position],
        has_data: bool,
    ) {
        let defs = |tssa: &Ssa<ClassId>| {
            let main = tssa.ops[0];
            let data = tssa.ops.get(1).filter(|_| has_data).map(|d| d.op1_def);
            [
                (main.out_def, main.out_use, Defined::Out, 0),
                (main.op1_def, main.op1_use, Defined::Op1, 0),
                (main.op2_def, main.op2_use, Defined::Op2, 0),
                (
                    data.flatten(),
                    tssa.ops.get(1).and_then(|d| d.op1_use),
                    Defined::Op1,
                    1,
                ),
            ]
        };
        // `zend_jit_trace_copy_ssa_var_range`, or the propagation alone
        // where php's analysis has no op array to read.
        for (def, use_, which, i) in defs(tssa) {
            let Some(def) = def else { continue };
            let place = at.0 + i;
            let static_info = self.analysed.defined(func, place, which).copied();
            let propagates = match &static_info {
                Some(info) => !info.types.has(php_ssa::Types::REF),
                None => use_
                    .and_then(|u| tssa.info.get(u.index()))
                    .is_none_or(|u| !u.types.has(php_ssa::Types::REF)),
            };
            let mut range = if propagates {
                self.inference
                    .propagate_range(func, tssa, positions, i as usize, def)
            } else {
                None
            };
            if let Some(known) = static_info.and_then(|info| info.range) {
                range = Some(range.map_or(known, |r| met(r, known)));
            }
            if let Some(info) = tssa.info.get_mut(def.index()) {
                info.range = range;
            }
        }
    }

    /// Where `next` is the opline after this one's in the same op array,
    /// the op that reads what this one defines, so that php's inference
    /// sees what reads it next; any other use is a later one.
    fn link_next(
        &self,
        tssa: &mut Tssa,
        positions: &mut Vec<Position>,
        func: FuncRef,
        next: Option<&Event>,
        array: u32,
    ) {
        let defs: Vec<SsaVar> = (tssa.ops.iter())
            .flat_map(|op| [op.out_def, op.op1_def, op.op2_def])
            .flatten()
            .collect();
        for def in &defs {
            tssa.vars[def.index()].use_chain = Some(LATER);
        }
        let Some(Event::Op {
            func: f, opline, ..
        }) = next
        else {
            return;
        };
        let Some(code) = self.view.op_array(*f) else {
            return;
        };
        let Some(next_op) = code.oplines.get(opline.0 as usize) else {
            return;
        };
        if *f != func {
            return;
        }
        let index = u32::try_from(tssa.ops.len()).unwrap_or(LATER);
        let reads = |kind: OperandKind, field: u32| {
            matches!(kind, OperandKind::Cv | OperandKind::Tmp | OperandKind::Var)
                .then(|| {
                    defs.iter()
                        .rev()
                        .copied()
                        .find(|d| tssa.vars[d.index()].var.0 == field)
                })
                .flatten()
        };
        let read = SsaOp {
            op1_use: reads(next_op.op1_type, next_op.op1),
            op2_use: reads(next_op.op2_type, next_op.op2),
            ..SsaOp::default()
        };
        for def in [read.op1_use, read.op2_use].into_iter().flatten() {
            tssa.vars[def.index()].use_chain = Some(index);
        }
        tssa.ops.push(read);
        positions.push(Position {
            array,
            opline: opline.0,
        });
    }

    /// What `frame`'s variable `var` holds before the opline writes it:
    /// the value the trace wrote there, or what it held where the frame's
    /// part began.
    fn held_in(&self, frame: Frame, var: u32) -> (Operand, Types) {
        let slot = crate::types::Slot { frame, var };
        match self.slots[usize::from(frame.0)].get(var as usize) {
            Some(Some(value)) => (Operand::Value(*value), self.val(*value).types),
            _ => (Operand::Unused, self.entry_types(slot)),
        }
    }
}

/// Two ranges met, as `zend_jit_trace_copy_ssa_var_range` meets them.
fn met(a: Range, b: Range) -> Range {
    Range {
        min: a.min.max(b.min),
        max: a.max.min(b.max),
        underflow: a.underflow && b.underflow,
        overflow: a.overflow && b.overflow,
    }
}

/// Whether the opline's key is a long: a constant one, or a variable
/// held as one.
fn long_key(op: &Opline, code: &OpArray, t2: Types) -> bool {
    if op.op2_type == OperandKind::Const {
        code.literals
            .get(op.op2 as usize)
            .is_some_and(|key| key.as_long().is_some())
    } else {
        t2.tag() == Some(ZvalType::Long)
    }
}

/// Whether the opline's `out` is no variable it defines here.
fn pushes_out(op: &Opline) -> bool {
    op.out_type == OperandKind::Unused
}

/// What an opline php's inference cannot read writes: anything.
fn unknown(op: &Opline, data: Option<&Opline>, written: crate::types::opcodes::Writes) -> Inferred {
    let any = |types: Types| Some(Held { types, range: None });
    Inferred {
        out: (op.out_type != OperandKind::Unused && !pushes_out(op))
            .then(|| any(Types::UNKNOWN.without(Types::UNDEF)))
            .flatten(),
        op1: written.op1.then(|| any(Types::UNKNOWN)).flatten(),
        op2: written.op2.then(|| any(Types::UNKNOWN)).flatten(),
        data: (written.data && data.is_some())
            .then(|| any(Types::UNKNOWN))
            .flatten(),
    }
}
