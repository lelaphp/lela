//! php's stack check (`zend_jit_stack_check`): the room a trace holds the
//! VM stack's top page to before it pushes frames, as php's trace
//! accounts for it in two passes over the record. `zend_jit_trace_build_tssa`
//! sums what a loop's calls push, its `used_stack`
//! (`Z/ext/opcache/jit/zend_jit_trace.c:1787-1791`, `:2251-2254`,
//! `:2586-2640`), which the trace's entry holds once
//! (`:4189-4196`); `zend_jit_trace` then follows what that entry check
//! and the calls pushed since cover, its `checked_stack` and
//! `peek_checked_stack` (`:6950-6966`, `:7015-7168`), and
//! `zend_jit_push_call_frame` holds each `INIT_*` that pushes past them
//! (`Z/ext/opcache/jit/zend_jit_ir.c:8466-8550`). Each check's exit goes
//! back to the interpreter, `ZEND_JIT_EXIT_TO_VM`, which pushes the frame
//! on a page of its own.

use std::collections::HashMap;

use php_engine::tier::FuncRef;
use php_unit::{OpArray, Opcode, Opline};

use super::{At, Builder, Draft, Placed, Point, draft};
use crate::record::{Began, Stop};
use crate::types::{Check, Frame, Grows, GuardAt, Op, OpId, Place};

/// `ZEND_CALL_FRAME_SLOT`: the slots of a frame's header,
/// `sizeof(zend_execute_data)` in zvals.
const FRAME_SLOT: u32 = 5;

/// A zval, `sizeof(zval)`.
const SLOT: u32 = 16;

/// The `INIT_*` oplines whose push php's code generator makes inline,
/// through `zend_jit_push_call_frame`, and so holds against the room:
/// a function's, by name or not. A method's, a static method's and a
/// closure's are php's too, under conditions of their own, and are held
/// once this tier compiles them as php does (#2963).
fn pushes_inline(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::InitFcall | Opcode::InitFcallByName | Opcode::InitNsFcallByName
    )
}

/// The `INIT_*` oplines whose frame `zend_jit_trace_build_tssa` adds to a
/// loop's `used_stack`.
fn counted(opcode: Opcode) -> bool {
    pushes_inline(opcode)
        || matches!(
            opcode,
            Opcode::InitMethodCall | Opcode::InitDynamicCall | Opcode::InitStaticMethodCall
        )
}

/// The `INIT_*` oplines after which `zend_jit_trace` adds the frame to
/// what its checks cover; any other resets what they cover.
fn covered(opcode: Opcode) -> bool {
    pushes_inline(opcode) || matches!(opcode, Opcode::InitMethodCall | Opcode::InitDynamicCall)
}

/// php's `zend_vm_calc_used_stack`: the bytes a call of `num_args`
/// arguments to `callee` takes on the stack, its header, its arguments
/// and its temporaries, and a user function's variables past the
/// arguments it declares; a native's, none for its op array.
fn used(num_args: u32, callee: Option<&OpArray>) -> i64 {
    let slots = match callee {
        Some(code) => {
            let declared = (code.oplines.iter())
                .take_while(|op| matches!(op.opcode, Opcode::Recv | Opcode::RecvInit))
                .count();
            let declared = u32::try_from(declared).unwrap_or(u32::MAX);
            FRAME_SLOT
                + num_args
                + code.num_tmps
                + code.num_cvs.saturating_sub(declared.min(num_args))
        }
        None => FRAME_SLOT + num_args,
    };
    i64::from(slots) * i64::from(SLOT)
}

impl Builder<'_> {
    /// The `INIT_*` line at `at`, its opline and the call it begins, where
    /// the op after it records one.
    fn init_at(&self, at: usize) -> Option<(Opline, Option<FuncRef>)> {
        let Some(Op::Line(line)) = self.ops.get(at) else {
            return None;
        };
        let op = *self
            .meta(line.frame)
            .code
            .oplines
            .get(line.opline.0 as usize)?;
        let target = match self.ops.get(at + 1) {
            Some(Op::InitCall {
                target,
                began: Began::Here,
                ..
            }) => Some(*target),
            _ => None,
        };
        Some((op, target.flatten()))
    }

    /// The bytes the call the `INIT_*` `op` begins takes, and whether the
    /// trace knows its callee: php's `zend_vm_calc_used_stack` over the
    /// `func` the trace recorded, a native's where it recorded none.
    fn pushed(&self, op: &Opline, recorded: bool, target: Option<FuncRef>) -> Option<i64> {
        if !recorded {
            return None;
        }
        let callee = match target {
            Some(f) => Some(self.view.op_array(f)?),
            None => None,
        };
        Some(used(op.extended_value, callee))
    }

    /// php's `used_stack`: what a loop's calls push at most, the frame of
    /// each `INIT_*` its frame makes added and taken off again as the
    /// frame returns; none where it cannot say, for a trace that does
    /// not loop by its own loop among them.
    fn used_stack(&self) -> Option<i64> {
        let mut used: i64 = if self.rec.stop == Stop::Loop { 0 } else { -1 };
        let mut max = used;
        let mut by_frame: HashMap<Frame, i64> = HashMap::new();
        let mut running = Frame::ROOT;
        for (at, op) in self.ops.iter().enumerate() {
            match op {
                Op::Line(line) => {
                    running = line.frame;
                    if matches!(
                        line.opcode,
                        Opcode::SendArray
                            | Opcode::SendUnpack
                            | Opcode::CheckUndefArgs
                            | Opcode::IncludeOrEval
                    ) {
                        (used, max) = (-1, -1);
                    }
                }
                Op::InitCall {
                    frame,
                    began: Began::Here,
                    ..
                } if used >= 0 => {
                    let init = at.checked_sub(1).and_then(|before| self.init_at(before));
                    let pushed = init.and_then(|(op, target)| {
                        counted(op.opcode)
                            .then(|| self.pushed(&op, true, target))
                            .flatten()
                    });
                    if let Some(bytes) = pushed {
                        by_frame.insert(*frame, bytes);
                        used += bytes;
                        max = max.max(used);
                    } else {
                        (used, max) = (-1, -1);
                    }
                }
                Op::InitCall { .. } => {}
                Op::Enter { frame } => {
                    by_frame.insert(*frame, 0);
                    running = *frame;
                }
                Op::Back { frame, .. } => {
                    let left = running;
                    running = *frame;
                    let entered = (self.frames.get(usize::from(left.0)))
                        .is_some_and(|info| matches!(info.place, Place::Callee(_)));
                    if !entered {
                        (used, max) = (-1, -1);
                    } else if used > 0 {
                        used -= by_frame.get(&left).copied().unwrap_or(0);
                    }
                }
            }
        }
        (max > 0).then_some(max)
    }

    /// php's stack checks, placed: one at entry for what a loop's calls
    /// push, and one before each `INIT_*` that pushes past what the checks
    /// before it cover, each leaving for the interpreter at the opline it
    /// holds.
    pub(super) fn stack_checks(&mut self) {
        let mut placed = Vec::new();
        let (mut checked, mut peek) = (0, 0);
        if let Some(used) = self.used_stack() {
            peek = used;
            placed.push(Placed {
                at: GuardAt::Entry,
                check: Check::Room {
                    bytes: u32::try_from(used).unwrap_or(u32::MAX),
                },
                exit: Draft {
                    grows: Grows::Never,
                    ..self.start()
                },
            });
        }
        let mut calls: Vec<(i64, i64)> = Vec::new();
        let mut entered: HashMap<Frame, (i64, i64)> = HashMap::new();
        let mut running = Frame::ROOT;
        for at in 0..self.ops.len() {
            match &self.ops[at] {
                Op::Line(line) => {
                    running = line.frame;
                    let made = matches!(
                        line.opcode,
                        Opcode::DoIcall | Opcode::DoUcall | Opcode::DoFcall | Opcode::DoFcallByName
                    ) && !matches!(self.ops.get(at + 1), Some(Op::Enter { .. }));
                    if made {
                        (checked, peek) = calls.pop().unwrap_or((checked, peek));
                    }
                    if !pushes_inline(line.opcode) {
                        continue;
                    }
                    let Some((op, target)) = self.init_at(at) else {
                        continue;
                    };
                    let recorded = matches!(
                        self.ops.get(at + 1),
                        Some(Op::InitCall {
                            began: Began::Here,
                            ..
                        })
                    );
                    let pushed = self.pushed(&op, recorded, target);
                    let bytes = pushed.unwrap_or_else(|| used(op.extended_value, None));
                    if pushed.is_none_or(|bytes| bytes > peek - checked) {
                        let idx = OpId(u32::try_from(at).unwrap_or(u32::MAX));
                        placed.push(Placed {
                            at: GuardAt::Before(idx),
                            check: Check::Room {
                                bytes: u32::try_from(bytes).unwrap_or(u32::MAX),
                            },
                            exit: Draft {
                                grows: Grows::Never,
                                ..draft(Point::Before(idx), line.frame, At::Opline(line.opline))
                            },
                        });
                    }
                }
                Op::InitCall {
                    began: Began::Here, ..
                } => {
                    calls.push((checked, peek));
                    let init = at.checked_sub(1).and_then(|before| self.init_at(before));
                    match init {
                        Some((op, target)) if covered(op.opcode) => {
                            checked += self
                                .pushed(&op, true, target)
                                .unwrap_or_else(|| used(op.extended_value, None));
                            peek = peek.max(checked);
                        }
                        _ => (checked, peek) = (0, 0),
                    }
                }
                Op::InitCall {
                    began: Began::Before,
                    ..
                } => {
                    calls.push((checked, peek));
                    (checked, peek) = (0, 0);
                }
                Op::Enter { frame } => {
                    entered.insert(*frame, calls.pop().unwrap_or((0, 0)));
                    running = *frame;
                }
                Op::Back { frame, .. } => {
                    let left = running;
                    running = *frame;
                    (checked, peek) = entered.get(&left).copied().unwrap_or((0, 0));
                }
            }
        }
        self.guards.extend(placed);
    }
}
