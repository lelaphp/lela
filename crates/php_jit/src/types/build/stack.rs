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

use php_unit::{OpArray, Opcode, Opline};

use super::{At, Builder, Draft, Placed, Point, draft};
use crate::record::{Began, Callee, Stop};
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
fn counted_by_tssa(opcode: Opcode) -> bool {
    pushes_inline(opcode)
        || matches!(
            opcode,
            Opcode::InitMethodCall | Opcode::InitDynamicCall | Opcode::InitStaticMethodCall
        )
}

/// The `INIT_*` oplines after which `zend_jit_trace` adds the frame to
/// what its checks cover; any other resets what they cover.
fn covering(opcode: Opcode) -> bool {
    pushes_inline(opcode) || matches!(opcode, Opcode::InitMethodCall | Opcode::InitDynamicCall)
}

/// What a loop's calls push, php's `used_stack` and `max_used_stack`:
/// what stands pushed now, and the most that did.
#[derive(Debug, Clone, Copy)]
struct Counted {
    used: i64,
    max: i64,
}

/// What the stack checks so far cover, php's `checked_stack` and
/// `peek_checked_stack`: the bytes the calls under way since took, and
/// the most a check has held room for.
#[derive(Debug, Clone, Copy, Default)]
struct Covered {
    checked: i64,
    peek: i64,
}

impl Covered {
    /// Whether a push of `bytes` needs a check of its own:
    /// `zend_jit_push_call_frame`'s `used_stack <= checked_stack` failed.
    fn misses(self, bytes: i64) -> bool {
        bytes > self.peek - self.checked
    }

    /// A push of `bytes` covered.
    fn push(self, bytes: i64) -> Covered {
        let checked = self.checked + bytes;
        Covered {
            checked,
            peek: self.peek.max(checked),
        }
    }
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
    /// The `INIT_*` line at `at`, its opline and the callee the op after
    /// it records, where it records one.
    fn init_at(&self, at: usize) -> Option<(Opline, Option<Callee>)> {
        let Some(Op::Line(line)) = self.ops.get(at) else {
            return None;
        };
        let op = *self
            .meta(line.frame)
            .code
            .oplines
            .get(line.opline.0 as usize)?;
        let callee = match self.ops.get(at + 1) {
            Some(Op::InitCall {
                callee,
                began: Began::Here,
                ..
            }) => Some(*callee),
            _ => None,
        };
        Some((op, callee))
    }

    /// The bytes the call the `INIT_*` `op` begins to `callee` takes, php's
    /// `zend_vm_calc_used_stack` over the `func` the trace recorded; none
    /// for a user function whose op array the view does not hold.
    fn pushed(&self, op: &Opline, callee: Callee) -> Option<i64> {
        let code = match callee {
            Callee::User(f) => Some(self.view.op_array(f)?),
            Callee::Native(_) => None,
        };
        Some(used(op.extended_value, code))
    }

    /// The frame each op of the trace runs in as it begins: where a return
    /// leaves from, for the op it is.
    fn running_before(&self) -> Vec<Frame> {
        let mut running = Frame::ROOT;
        (self.ops.iter())
            .map(|op| {
                let before = running;
                running = match op {
                    Op::Line(line) => line.frame,
                    Op::Enter { frame, .. } | Op::Back { frame, .. } => *frame,
                    Op::InitCall { .. } => running,
                };
                before
            })
            .collect()
    }

    /// Whether `frame` is one a call the trace followed pushed, php's
    /// trace frame with a `prev`.
    fn entered(&self, frame: Frame) -> bool {
        (self.frames.get(usize::from(frame.0)))
            .is_some_and(|info| matches!(info.place, Place::Callee(_)))
    }

    /// php's `used_stack`: what a loop's calls push at most, the frame of
    /// each `INIT_*` its frame makes added and taken off again as the
    /// frame returns; none where it cannot say, for a trace that does
    /// not loop by its own loop among them.
    fn used_stack(&self) -> Option<i64> {
        let mut counted = (self.rec.stop == Stop::Loop).then_some(Counted { used: 0, max: 0 });
        let mut by_frame: HashMap<Frame, i64> = HashMap::new();
        let running = self.running_before();
        for (at, op) in self.ops.iter().enumerate() {
            let Some(now) = counted else {
                break;
            };
            counted = match op {
                Op::Line(line)
                    if matches!(
                        line.opcode,
                        Opcode::SendArray
                            | Opcode::SendUnpack
                            | Opcode::CheckUndefArgs
                            | Opcode::IncludeOrEval
                    ) =>
                {
                    None
                }
                Op::InitCall {
                    frame,
                    began: Began::Here,
                    ..
                } => {
                    let init = at.checked_sub(1).and_then(|before| self.init_at(before));
                    let pushed = init.and_then(|(op, callee)| {
                        counted_by_tssa(op.opcode)
                            .then(|| self.pushed(&op, callee?))
                            .flatten()
                    });
                    pushed.map(|bytes| {
                        by_frame.insert(*frame, bytes);
                        let used = now.used + bytes;
                        Counted {
                            used,
                            max: now.max.max(used),
                        }
                    })
                }
                Op::Enter { frame, .. } => {
                    by_frame.insert(*frame, 0);
                    Some(now)
                }
                Op::Back { .. } => {
                    let left = running.get(at).copied().unwrap_or(Frame::ROOT);
                    self.entered(left).then(|| Counted {
                        used: if now.used > 0 {
                            now.used - by_frame.get(&left).copied().unwrap_or(0)
                        } else {
                            now.used
                        },
                        ..now
                    })
                }
                Op::Line(_) | Op::InitCall { .. } => Some(now),
            };
        }
        counted.map(|counted| counted.max).filter(|max| *max > 0)
    }

    /// php's stack checks, placed: one at entry for what a loop's calls
    /// push, and one before each `INIT_*` that pushes past what the checks
    /// before it cover, each leaving for the interpreter at the opline it
    /// holds.
    pub(super) fn stack_checks(&mut self) {
        let mut placed = Vec::new();
        let mut covered = Covered::default();
        if let Some(used) = self.used_stack() {
            covered.peek = used;
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
        let mut calls: Vec<Covered> = Vec::new();
        let mut entered: HashMap<Frame, Covered> = HashMap::new();
        let running = self.running_before();
        for (at, op) in self.ops.iter().enumerate() {
            match op {
                Op::Line(line) => {
                    let made = matches!(
                        line.opcode,
                        Opcode::DoIcall | Opcode::DoUcall | Opcode::DoFcall | Opcode::DoFcallByName
                    ) && !matches!(self.ops.get(at + 1), Some(Op::Enter { .. }));
                    if made {
                        covered = calls.pop().unwrap_or(covered);
                    }
                    if !pushes_inline(line.opcode) {
                        continue;
                    }
                    let Some((op, callee)) = self.init_at(at) else {
                        continue;
                    };
                    let pushed = callee.and_then(|callee| self.pushed(&op, callee));
                    if pushed.is_none_or(|bytes| covered.misses(bytes)) {
                        let bytes = pushed.unwrap_or_else(|| used(op.extended_value, None));
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
                    callee,
                    began: Began::Here,
                    ..
                } => {
                    calls.push(covered);
                    let init = at.checked_sub(1).and_then(|before| self.init_at(before));
                    covered = match init {
                        Some((op, _)) if covering(op.opcode) => covered.push(
                            self.pushed(&op, *callee)
                                .unwrap_or_else(|| used(op.extended_value, None)),
                        ),
                        _ => Covered::default(),
                    };
                }
                Op::InitCall {
                    began: Began::Before,
                    ..
                } => {
                    calls.push(covered);
                    covered = Covered::default();
                }
                Op::Enter { frame, .. } => {
                    entered.insert(*frame, calls.pop().unwrap_or_default());
                }
                Op::Back { .. } => {
                    let left = running.get(at).copied().unwrap_or(Frame::ROOT);
                    covered = entered.get(&left).copied().unwrap_or_default();
                }
            }
        }
        self.guards.extend(placed);
    }
}
