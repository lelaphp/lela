//! The ranges php's code generator reads as it compiles a trace, after
//! `zend_jit_trace_build_tssa` has inferred them: a comparison it
//! compiles as a branch narrows what its operands hold past it
//! (`zend_jit_trace_update_condition_ranges`,
//! `Z/ext/opcache/jit/zend_jit_trace.c:3705-3827`), each long an opline
//! defines is propagated again over what its operands hold once it is
//! compiled (`:6558-6830`), and an increment or a decrement checks its
//! overflow only where those ranges leave room for one
//! (`zend_may_overflow`, `Z/ext/opcache/jit/zend_jit.c:910-950`). The code
//! generator writes them over the trace SSA's ranges after its dump, so
//! they are kept beside each value's own range and never read by the
//! inference.

use php_ssa::Range;
use php_unit::{Opcode, Opline};

use super::Builder;
use super::infer::{Reads, met};
use crate::analysis::Defined;
use crate::types::lattice::Types;
use crate::types::{Constant, Line, Op, OpId, Operand, ValueId};

impl Builder<'_> {
    /// The range the code generator reads of `operand` where it compiles
    /// the opline that reads it: a long constant's own, or the value's,
    /// as a comparison compiled before narrowed it; none where nothing
    /// bounds it (`_ssa_op1_has_range` and its kin,
    /// `Z/Zend/Optimizer/zend_inference.h:39-120`).
    pub(super) fn compiled_range(&self, operand: Operand) -> Option<Range> {
        match operand {
            Operand::Constant(Constant::Long(n)) => Some(Range {
                min: n,
                max: n,
                underflow: false,
                overflow: false,
            }),
            Operand::Value(value) => match self.compiled.get(&value) {
                Some(range) => *range,
                None => self.val(value).range,
            },
            Operand::Constant(_) | Operand::Unused => None,
        }
    }

    /// php's `zend_jit_trace_update_condition_ranges` over a comparison
    /// `op` the trace compiles fused to its jump, reading `operands`,
    /// where the trace goes on the way `taken` says: what each operand
    /// holds on the way the trace goes on, past the exit the other way
    /// leaves through. The test of the first operand's type is php's,
    /// made twice there and of the second operand never.
    ///
    /// The loop's own comparison bounds the phi of the variable it steps below the
    /// bound it is compared with, so its increment cannot overflow: the
    /// pinned php compiles no overflow check on `$i++` here
    /// (`TRACE 1 exit info` under `opcache.jit_debug` bits 19 and 20
    /// lists `exit_8: 0014` and `exit_9: 0016/VM` and nothing past the
    /// `PRE_INC`), though its trace SSA dumps the phi as `RANGE[1..++]`:
    ///
    /// ```php
    /// function f($s, $n, $k) { $t = 0; for ($i = 1; $i < $n; $i++) { $t += strncmp($i + $k, $s . $i, $i + $k); } return $t; }
    /// echo f('a', 20, 2), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// -875
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
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    /// ```
    pub(super) fn narrow_by_condition(
        &mut self,
        op: &Opline,
        operands: (Operand, Operand),
        jumps_if_true: bool,
        taken: bool,
    ) {
        let (op1, op2) = operands;
        let mut exit_if_true = taken != jumps_if_true;
        // php turns the exit's sense round for `!==` before it narrows
        // (`zend_jit_trace.c:5509-5511`).
        if op.opcode == Opcode::IsNotIdentical {
            exit_if_true = !exit_if_true;
        }
        let long = |operand: Operand| match operand {
            Operand::Value(value) => {
                self.val(value).types.kinds().without(Types::UNDEF) == Types::LONG
            }
            Operand::Constant(Constant::Long(_)) => true,
            Operand::Constant(_) | Operand::Unused => false,
        };
        if !long(op1) {
            return;
        }
        let bounds = |operand| {
            self.compiled_range(operand)
                .map_or((i64::MIN, i64::MAX), |range| (range.min, range.max))
        };
        let ((min1, max1), (min2, max2)) = (bounds(op1), bounds(op2));
        let (one, two) = match (op.opcode, exit_if_true) {
            (
                Opcode::IsEqual
                | Opcode::Case
                | Opcode::IsIdentical
                | Opcode::CaseStrict
                | Opcode::IsNotIdentical,
                false,
            )
            | (Opcode::IsNotEqual, true) => (
                (min1.max(min2), max1.min(max2)),
                (min2.max(min1), max2.min(max1)),
            ),
            // `op1 <= op2`.
            (Opcode::IsSmallerOrEqual, false) => ((min1, max1.min(max2)), (min2.max(min1), max2)),
            // `op1 > op2`.
            (Opcode::IsSmallerOrEqual, true) => (
                (
                    if min2 == i64::MAX {
                        min1
                    } else {
                        min1.max(min2 + 1)
                    },
                    max1,
                ),
                (
                    min2,
                    if max2 == i64::MIN {
                        max1
                    } else {
                        max2.min(max1.wrapping_sub(1))
                    },
                ),
            ),
            // `op1 < op2`.
            (Opcode::IsSmaller, false) => (
                (
                    min1,
                    if max2 == i64::MIN {
                        max1
                    } else {
                        max1.min(max2 - 1)
                    },
                ),
                (
                    if min1 == i64::MAX {
                        min2
                    } else {
                        min2.max(min1 + 1)
                    },
                    max2,
                ),
            ),
            // `op1 >= op2`.
            (Opcode::IsSmaller, true) => ((min1.max(min2), max1), (min2, max2.min(max1))),
            _ => return,
        };
        for (operand, (min, max)) in [(op1, one), (op2, two)] {
            if let Operand::Value(value) = operand {
                let range = Range {
                    min,
                    max,
                    underflow: false,
                    overflow: false,
                };
                self.compiled.insert(value, Some(range));
            }
        }
    }

    /// The range of each variable the line at `at` defines in php's trace
    /// SSA, as its code generator holds it once it has compiled the line
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:6558-6830`): one that holds a
    /// long is propagated again over the ranges the line's operands hold
    /// there, and may pass neither bound; where the propagation knows
    /// nothing of it, it keeps the range the trace SSA gave it, and a
    /// variable php's trace SSA numbers anew where this tier keeps the
    /// value it read ([`Builder::writes_anew`]) is read after the line as
    /// that new variable ([`Builder::renumbered_range`]). So the element
    /// `$a[$i & 1023] = $i` writes is a long below `ZEND_LONG_MAX` where
    /// the loop's `$i < $n` held `$i` there, and the `$i++` that reads it
    /// next cannot overflow: the pinned php's trace of `array_write`'s `k`
    /// takes no exit past its `PRE_INC`. `method_call`'s `k` sends `$i`
    /// before its `$i++`, and php propagates nothing over a
    /// `SEND_VAR_EX`, so its `PRE_INC` reads `#16`, what the `SEND_VAR_EX`
    /// read, and keeps its overflow exit, `exit_8: 0012/0020/10 ...
    /// CV3($i):float`.
    pub(super) fn refresh(&mut self, at: OpId, reads: &Reads<'_>) {
        let Some(Op::Line(line)) = self.ops.get(at.index()) else {
            return;
        };
        let line = line.clone();
        let [anew1, anew2, anew_data] = self.defines(&line);
        let kept = |defined: bool, operand: Operand| match operand {
            Operand::Value(value) if defined => (Some(value), true),
            _ => (None, false),
        };
        let parts = [
            (line.out, false),
            match line.op1_def {
                Some(def) => (Some(def), false),
                None => kept(anew1, line.op1),
            },
            match line.op2_def {
                Some(def) => (Some(def), false),
                None => kept(anew2, line.op2),
            },
            kept(anew_data, line.data),
        ];
        let propagated = self.compiled_propagation(
            line.frame,
            line.opline,
            reads,
            parts.map(|(v, _)| v.is_some()),
        );
        let named = [None, Some(Part::Op1), Some(Part::Op2), Some(Part::Data)];
        for (((value, kept), range), part) in parts.into_iter().zip(propagated).zip(named) {
            let Some(value) = value else { continue };
            let types = self.val(value).types;
            let long =
                types.kinds().without(Types::UNDEF) == Types::LONG && !types.meets(Types::REF);
            let range = match (long, range, part) {
                (true, Some(range), _) => Some(Range {
                    underflow: false,
                    overflow: false,
                    ..range
                }),
                (_, _, Some(part)) if kept => self.renumbered_range(&line, part, value),
                _ => continue,
            };
            self.compiled.insert(value, range);
        }
    }

    /// The range php's trace SSA gives the variable `line` numbers anew at
    /// `part`, which held `value`: a `SEND_VAR_EX` to a parameter taken
    /// by value keeps what it read (`Z/ext/opcache/jit/zend_jit_trace.c:2388-2396`);
    /// any other is `zend_jit_trace_copy_ssa_var_range`'s, the range
    /// `zend_inference_propagate_range` copies from the operand met with
    /// what php's analysis of the op array knows of the variable
    /// (`:1009-1058`, `Z/Zend/Optimizer/zend_inference.c:1252-1500`), which
    /// `zend_jit_trace_restrict_ssa_var_info` meets again.
    fn renumbered_range(&self, line: &Line, part: Part, value: ValueId) -> Option<Range> {
        let read = self.val(value).range;
        let op = self.opline_of(line)?;
        if op.opcode == Opcode::SendVarEx && line.opcode == Opcode::SendVar {
            return read;
        }
        let copied = match (part, op.opcode) {
            (
                Part::Op1,
                Opcode::SendVar
                | Opcode::Cast
                | Opcode::QmAssign
                | Opcode::JmpSet
                | Opcode::Coalesce
                | Opcode::CopyTmp
                | Opcode::UnsetDim
                | Opcode::UnsetObj,
            )
            | (Part::Op2, Opcode::Assign)
            | (
                Part::Data,
                Opcode::AssignDim
                | Opcode::AssignObj
                | Opcode::AssignStaticProp
                | Opcode::AssignDimOp
                | Opcode::AssignObjOp
                | Opcode::AssignStaticPropOp,
            ) => read,
            _ => None,
        };
        let (at, which) = match part {
            Part::Op1 => (line.opline.0, Defined::Op1),
            Part::Op2 => (line.opline.0, Defined::Op2),
            Part::Data => (line.opline.0 + 1, Defined::Op1),
        };
        let func = self.frames[usize::from(line.frame.0)].func;
        let Some(known) = self.analysed.defined(func, at, which) else {
            return copied;
        };
        let propagated = copied.filter(|_| {
            !known.types.has(php_ssa::Types::REF) && !self.analysed.aliased(func, at, which)
        });
        match (propagated, known.range) {
            (Some(range), Some(known)) => Some(met(range, known)),
            (range, known) => range.or(known),
        }
    }

    /// php's `zend_may_overflow` of an increment or a decrement `op`
    /// reading `op1` and defining `def` (`Z/ext/opcache/jit/zend_jit.c:910-950`):
    /// where what it defines may pass the bound it steps toward, whether
    /// what it reads may stand at that bound, `ZEND_LONG_MAX` for an
    /// increment and `ZEND_LONG_MIN` for a decrement, as the code
    /// generator reads it there. The range it reads is `range.max` or
    /// `range.min` whatever its `overflow` or `underflow` says
    /// (`_ssa_op1_max_range`, `Z/Zend/Optimizer/zend_inference.h:71-85`).
    pub(super) fn may_overflow(&self, op: &Opline, op1: Operand, def: Option<ValueId>) -> bool {
        let up = matches!(op.opcode, Opcode::PreInc | Opcode::PostInc);
        let passes = def
            .and_then(|def| self.val(def).range)
            .is_none_or(|range| if up { range.overflow } else { range.underflow });
        if !passes {
            return false;
        }
        self.compiled_range(op1).is_none_or(|range| {
            if up {
                range.max == i64::MAX
            } else {
                range.min == i64::MIN
            }
        })
    }

    /// Whether the increment or decrement at `at` is one php's
    /// `zend_may_overflow` said cannot overflow, which its code generator
    /// compiles with no check and no exit (`zend_jit_inc_dec`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:4762-4777`).
    pub(super) fn bounded(&self, at: OpId) -> bool {
        self.bounded.contains(&at)
    }
}

/// Which of a line's operands php's trace SSA numbers anew.
#[derive(Clone, Copy)]
enum Part {
    Op1,
    Op2,
    Data,
}
