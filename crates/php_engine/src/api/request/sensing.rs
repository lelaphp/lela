//! A debug build's sensing points (design §10): the engine's own sensors,
//! a `TMP` written over an owner it still held and a `TMP` read holding
//! what no `TMP` holds, and the questions the
//! loop and the request's end ask of it and of the heap's record
//! ([`php_heap::Heap::take_sensed`]), which end the request in the
//! `InternalError` naming what was found. What the request has made of
//! them is [`Sensing`]'s, whose field no file but this one reads. A
//! release build senses nothing and compiles none of this.

use php_heap::{Fatal, FrameRef};
use php_unit::{OpArray, Opcode, Opline};

use crate::exec;

use super::Request;

/// What a debug build's sensors have made of one request.
#[derive(Debug, Default)]
pub(super) enum Sensing {
    /// Nothing the engine's own sensor found waits; the heap's record
    /// is asked at each sensing point.
    #[default]
    Watching,
    /// The first write of a `TMP` found over a slot that still held an
    /// owner, until the next sensing point takes it.
    Dirty(DirtyTemporary),
    /// The first `TMP` an inline path that takes it untested found
    /// holding what no `TMP` holds, until the next sensing point takes
    /// it.
    Unmovable(UnmovableTemporary),
    /// A sensed defect has ended the request. What a sensor finds after
    /// it is read through memory that defect has already made
    /// untrustworthy, so php's one fatal stands alone and nothing more
    /// is raised.
    Ended,
}

/// A write a debug build stopped: the opline whose `TMP` it was, the
/// opcode serving it, the temporary's slot, as the dump numbers it,
/// and what the slot held.
#[derive(Debug, Clone, Copy)]
pub(super) struct DirtyTemporary {
    opline: u32,
    opcode: Opcode,
    number: u32,
    held: php_heap::ZvalType,
}

impl DirtyTemporary {
    fn into_fatal(self) -> Fatal {
        Fatal::error(format!(
            "InternalError: {} at opline {} writes its TMP #{} over an owner it still holds ({:?}): the handler that last read the slot moved it out and did not empty it",
            self.opcode.name(),
            self.opline,
            self.number,
            self.held,
        ))
    }
}

/// A `TMP` a debug build found holding a reference, an `IS_INDIRECT`,
/// nothing at all or another value no variable may hold, where the
/// opline reading it takes it as php's `_TMP` handlers do, untested
/// (`ZEND_ASSIGN_SPEC_CV_TMP`): the opcode, the temporary's slot, as the
/// dump numbers it, and what it held.
#[derive(Debug, Clone, Copy)]
pub(super) struct UnmovableTemporary {
    opcode: Opcode,
    number: u32,
    held: php_heap::ZvalType,
}

impl UnmovableTemporary {
    fn into_fatal(self) -> Fatal {
        Fatal::error(format!(
            "InternalError: {} reads its TMP #{} holding {:?}, which no TMP holds: the handler that wrote the slot left it so",
            self.opcode.name(),
            self.number,
            self.held,
        ))
    }
}

impl Request {
    /// A debug build's question where an inline path takes the `TMP`
    /// `number` of `op` as php's `_TMP` handlers take it, untested: a
    /// `held` no `TMP` holds is recorded, the first of them.
    pub(crate) fn sense_unmovable_temporary(
        &mut self,
        op: &Opline,
        number: u32,
        held: php_heap::ZvalType,
    ) {
        if matches!(self.sensing, Sensing::Watching) {
            self.sensing = Sensing::Unmovable(UnmovableTemporary {
                opcode: op.opcode,
                number,
                held,
            });
        }
    }

    /// A debug build's question at a write over a slot that held an
    /// owner of a `held`: whether the slot is the `TMP` of the opline
    /// `frame` stands at, which the handler that read it last should
    /// have left empty. The first such write is recorded.
    pub(super) fn sense_dirty_temporary(
        &mut self,
        frame: FrameRef,
        at: u32,
        held: php_heap::ZvalType,
    ) {
        if !matches!(self.sensing, Sensing::Watching) {
            return;
        }
        let opline = self.vm.opline(frame);
        let Some(serving) = self.serving(frame) else {
            return;
        };
        let Some(op) = serving
            .code()
            .and_then(|code| exec::opline_at(code, opline).copied())
        else {
            return;
        };
        let its_own = match op.out_type {
            php_unit::OperandKind::Tmp => op.out == at,
            php_unit::OperandKind::Var
            | php_unit::OperandKind::Cv
            | php_unit::OperandKind::Const
            | php_unit::OperandKind::Unused => false,
        };
        if its_own {
            self.sensing = Sensing::Dirty(DirtyTemporary {
                opline,
                opcode: op.opcode,
                number: op.out,
                held,
            });
        }
    }

    /// What a debug build's sensors found since the loop last asked
    /// (design §10): a `TMP` written over an owner it still held,
    /// or what the heap's sensors found ([`php_heap::Heap::take_sensed`]),
    /// a refcount operation on a block the heap had freed among them, as
    /// the `InternalError` fatal that ends the request; the first of
    /// them, and taken, so it is raised once and the request is ended by
    /// it ([`Sensing::Ended`]). Nothing while a fatal is leaving: what
    /// was found waits for the request's end, where the fatal's
    /// `zend_bailout` has been caught and it can be rendered. The loop
    /// asks after every handler ([`Request::sensed_after`]), and
    /// [`Request::end_in_sensed`] asks where the request ends.
    fn take_sensed(&mut self) -> Option<Fatal> {
        if self.bailout {
            return None;
        }
        let sensed = match std::mem::replace(&mut self.sensing, Sensing::Ended) {
            Sensing::Ended => return None,
            Sensing::Dirty(dirty) => Some(dirty.into_fatal()),
            Sensing::Unmovable(unmovable) => Some(unmovable.into_fatal()),
            Sensing::Watching => self.heap.take_sensed(),
        };
        if sensed.is_none() {
            self.sensing = Sensing::Watching;
        }
        sensed
    }

    /// The loop's sensing point after the handler of `op`, in `code`,
    /// ran: what the sensors found ends the request in the
    /// `InternalError` rendered at that handler's line, and not at the
    /// opline the handler moved the frame to, which is the statement
    /// after the one that tripped the sensor. The fatal is handed
    /// back for the loop to leave by; it is rendered and the request is
    /// leaving by it, so the caller's `end_in` renders nothing more, as
    /// for any fatal raised while one leaves.
    pub(crate) fn sensed_after(&mut self, code: &OpArray, op: &Opline) -> Result<(), Fatal> {
        let Some(sensed) = self.take_sensed() else {
            return Ok(());
        };
        match sensed.reason() {
            php_heap::FatalReason::Error(message) => {
                self.end_in_fatal_at(message, &code.filename.0, op.lineno);
            }
            php_heap::FatalReason::OutOfMemory(_)
            | php_heap::FatalReason::HardLimit { .. }
            | php_heap::FatalReason::Panic(_)
            | php_heap::FatalReason::Refused => {
                self.end_in(&sensed);
            }
        }
        Err(sensed)
    }

    /// A debug build's sensing point outside the loop, where a request
    /// ends: [`Engine::shutdown_request`] after the request's last
    /// release and [`Engine::start_failed`] where its startup failed.
    /// What the sensors found that no handler step was left to ask for,
    /// in a request-end function or as the request's tables were freed,
    /// ends the request here in the `InternalError` the loop would have
    /// ended it in, rendered with nothing executing, so no refusal goes
    /// unraised. It is a step of its own, as each step of
    /// `php_request_shutdown` is its own `zend_try`, so a fatal raised
    /// before it and still leaving does not keep it from being rendered.
    ///
    /// [`Engine::shutdown_request`]: crate::engine::Engine::shutdown_request
    /// [`Engine::start_failed`]: crate::engine::Engine::start_failed
    pub(crate) fn end_in_sensed(&mut self) {
        self.catch_bailout();
        if let Some(sensed) = self.take_sensed() {
            self.end_in(&sensed);
            self.catch_bailout();
        }
    }
}
