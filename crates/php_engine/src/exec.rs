//! The engine's half of a frame (design §5.6): what `php_heap`'s VM
//! stack does not hold, because it belongs with the engine's `Func` and
//! not with the memory model — the op array being served, the opline the
//! loop is at, the frame's slots, and where the caller wants the value
//! the frame returns. Beside them, the calls `INIT_FCALL` builds and
//! `DO_ICALL` serves, which are php's frames pushed on the VM stack
//! before their arguments are known.
//!
//! The slots are [`Value`]s here, the same stand-in `api::values` is,
//! and become the frame's zvals with the memory model (design §11, Lane
//! A); the frame's shape on the VM stack is already its zval count.

use std::sync::Arc;

use php_heap::{FrameRef, Zval};
use php_unit::{ConstIdx, OpArray, OperandKind, Opline};

use crate::api::call::Function;
use crate::api::values::{PhpString, Value};

/// One frame's engine state.
pub(crate) struct FrameState {
    /// The op array being served.
    pub(crate) code: Arc<OpArray>,
    /// Where in it the loop stands.
    pub(crate) opline: u32,
    /// The CVs, then the temporaries past the last CV.
    pub(crate) slots: Vec<Value>,
}

/// One call between its `INIT_FCALL` and the `DO_*CALL` that serves it:
/// what php pushes on the VM stack as a call frame.
pub(crate) struct CallState {
    /// The callee, resolved at the `INIT_FCALL`.
    pub(crate) function: Function,
    /// The callee's name, for what a raise says.
    pub(crate) name: PhpString,
    /// The arguments the `SEND_*` oplines have placed.
    pub(crate) arguments: Vec<Value>,
}

/// Every frame's engine state, and the calls being built.
#[derive(Default)]
pub(crate) struct Vm {
    frames: Vec<FrameState>,
    calls: Vec<CallState>,
}

impl Vm {
    /// Push one frame's state; its place is the `FrameRef` the VM stack
    /// gave the frame, and the two are pushed and popped together.
    pub(crate) fn push(&mut self, code: Arc<OpArray>) {
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        self.frames.push(FrameState {
            code,
            opline: 0,
            slots: vec![Value::null(); slots],
        });
    }

    pub(crate) fn pop(&mut self) {
        self.frames.pop();
    }

    pub(crate) fn at(&self, frame: FrameRef) -> Option<&FrameState> {
        self.frames.get(frame.0 as usize)
    }

    pub(crate) fn at_mut(&mut self, frame: FrameRef) -> Option<&mut FrameState> {
        self.frames.get_mut(frame.0 as usize)
    }

    /// The op array being served in `frame`, shared so the loop can read
    /// its oplines while the request is borrowed.
    pub(crate) fn code(&self, frame: FrameRef) -> Option<Arc<OpArray>> {
        self.at(frame).map(|state| Arc::clone(&state.code))
    }

    /// Where the loop stands in `frame`.
    pub(crate) fn opline(&self, frame: FrameRef) -> u32 {
        self.at(frame).map_or(0, |state| state.opline)
    }

    /// `ZEND_VM_NEXT_OPCODE`: on to the opline after this one.
    pub(crate) fn advance(&mut self, frame: FrameRef) {
        if let Some(state) = self.at_mut(frame) {
            state.opline = state.opline.saturating_add(1);
        }
    }

    /// The value in a slot; a slot past the end reads as `null`, as an
    /// uninitialised zval reads.
    pub(crate) fn slot(&self, frame: FrameRef, at: u32) -> Value {
        self.at(frame)
            .and_then(|state| state.slots.get(at as usize))
            .cloned()
            .unwrap_or_else(Value::null)
    }

    pub(crate) fn set_slot(&mut self, frame: FrameRef, at: u32, value: Value) {
        if let Some(slot) = self
            .at_mut(frame)
            .and_then(|state| state.slots.get_mut(at as usize))
        {
            *slot = value;
        }
    }

    pub(crate) fn push_call(&mut self, call: CallState) {
        self.calls.push(call);
    }

    /// The call the `SEND_*` oplines are placing arguments into.
    pub(crate) fn building_call(&mut self) -> Option<&mut CallState> {
        self.calls.last_mut()
    }

    /// The call a `DO_*CALL` serves, taken off the stack.
    pub(crate) fn take_call(&mut self) -> Option<CallState> {
        self.calls.pop()
    }
}

/// The slot a `TMP`/`VAR` operand names: temporaries stand past the last
/// CV on the frame, as `EX_NUM_TO_VAR` places them past the last CV.
pub(crate) fn temporary_slot(code: &OpArray, number: u32) -> u32 {
    code.num_cvs.saturating_add(number)
}

/// One operand's value (seam 6, design §4.3): a constant from the op
/// array's table, a CV or a temporary from the frame. An `UNUSED` operand
/// has no value and reads as `null`, which is what a handler that reads
/// one would have been doing wrong.
pub(crate) fn operand(
    vm: &Vm,
    frame: FrameRef,
    code: &OpArray,
    kind: OperandKind,
    field: u32,
) -> Value {
    match kind {
        OperandKind::Const => constant(code, ConstIdx(field)),
        OperandKind::Cv => vm.slot(frame, field),
        OperandKind::Tmp | OperandKind::Var => vm.slot(frame, temporary_slot(code, field)),
        OperandKind::Unused => Value::null(),
    }
}

/// The op array's constant at `at`, as a value.
pub(crate) fn constant(code: &OpArray, at: ConstIdx) -> Value {
    code.literals
        .get(at.0 as usize)
        .copied()
        .map_or_else(Value::null, value_of)
}

/// A zval as the value vocabulary holds it. Only the forms a compiled
/// program's constant table holds are here; the rest are the memory
/// model's and arrive with it (design §11, Lane A).
pub(crate) fn value_of(zval: Zval) -> Value {
    if let Some(n) = zval.as_long() {
        return Value::int(n);
    }
    if let Some(f) = zval.as_double() {
        return Value::float(f);
    }
    if let Some(b) = zval.as_bool() {
        return Value::bool(b);
    }
    match zval.as_str() {
        Some(s) => Value::string(PhpString::from_bytes(s.bytes())),
        None => Value::null(),
    }
}

/// The opline at `at` of `code`, if there is one.
pub(crate) fn opline_at(code: &OpArray, at: u32) -> Option<&Opline> {
    code.oplines.get(at as usize)
}
