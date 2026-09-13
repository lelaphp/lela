//! The engine's half of a frame (design §5.6): what `php_heap`'s VM
//! stack does not hold, because it belongs with the engine's `Func` and
//! not with the memory model — the op array being served, the opline the
//! loop is at, the frame's slots, and where the caller wants the value
//! the frame returns. Beside them, the calls `INIT_FCALL` builds and
//! `DO_ICALL` serves, which are php's frames pushed on the VM stack
//! before their arguments are known.
//!
//! The slots are [`Value`]s: a scalar or a string in one is the heap's
//! zval, sixteen bytes naming a `php_heap` string with no owner taken
//! (design §5.1), and the engine takes and gives up owners here where
//! php's handlers do, with `ZVAL_COPY`, `ZVAL_COPY_VALUE` and `FREE_OP`
//! as [`take_operand`], [`operand`] and [`free_operand`]. An array, an
//! object, a resource and a reference in a slot are still the owned
//! stand-ins of `api::values`, and move to the heap's frame with the
//! rest of Lane A (design §11, days 3–7).

use std::sync::Arc;

use php_heap::{FrameRef, Zval};
use php_unit::{ConstIdx, OpArray, OperandKind, Opline, Unit};

use crate::api::call::Function;
use crate::api::raising::warning;
use crate::api::request::Request;
use crate::api::values::{PhpString, TypeTag, Value};

/// One frame's engine state.
pub(crate) struct FrameState {
    /// The unit being served, shared with every other request serving it
    /// and never copied (design §5.14). Which of its op arrays the frame
    /// is in is the main one, until the call opcodes that push a frame
    /// pick another (`DO_UCALL`, design §11 days 3–7).
    pub(crate) unit: Arc<Unit>,
    /// Where in it the loop stands.
    pub(crate) opline: u32,
    /// The CVs, then the temporaries past the last CV.
    pub(crate) slots: Vec<Value>,
    /// The caller's slot for what this frame returns, when the call that
    /// pushed it wants the value.
    pub(crate) returns_to: Option<(FrameRef, u32)>,
}

/// One call from its `INIT_FCALL` until the `DO_*CALL` that serves it
/// returns: what php pushes on the VM stack as a call frame, and keeps
/// there while the callee runs.
pub(crate) struct CallState {
    /// The callee, resolved at the `INIT_FCALL`.
    pub(crate) function: Function,
    /// The callee's name, for what a raise says.
    pub(crate) name: PhpString,
    /// The arguments the `SEND_*` oplines have placed, read off this
    /// frame by the callee and by a backtrace taken while it runs, as
    /// `ZEND_CALL_ARG` reads them off php's; nothing copies them.
    pub(crate) arguments: Vec<Value>,
    /// How many the `INIT_FCALL` said there are, which bounds them.
    pub(crate) count: usize,
    /// Where the call stands.
    pub(crate) stage: CallStage,
}

/// Where a call stands: its `SEND_*` oplines placing arguments after the
/// `INIT_FCALL`, or its `DO_*CALL` serving it. A backtrace sees the
/// second and not the first, as `zend_fetch_debug_backtrace` walks
/// `EG(current_execute_data)`, a chain a frame `INIT_FCALL` has pushed
/// and nothing has activated is not on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallStage {
    Building,
    Serving,
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
    pub(crate) fn push(&mut self, unit: Arc<Unit>) {
        let code = code_of(&unit);
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        // `i_init_func_execute_data`: every CV is `IS_UNDEF` until an
        // opline writes it, which is what a read of one warns of.
        self.frames.push(FrameState {
            unit,
            opline: 0,
            slots: vec![Value::undef(); slots],
            returns_to: None,
        });
    }

    /// The frame's state off the stack, its slots handed back for the
    /// request to release: php's `i_free_compiled_variables` over the
    /// CVs, and the temporaries with them, since a temporary the compiler
    /// numbered for an opline's value and nothing read is still an owner
    /// here.
    pub(crate) fn pop(&mut self) -> Vec<Value> {
        self.frames
            .pop()
            .map(|state| state.slots)
            .unwrap_or_default()
    }

    pub(crate) fn at(&self, frame: FrameRef) -> Option<&FrameState> {
        self.frames.get(frame.0 as usize)
    }

    pub(crate) fn at_mut(&mut self, frame: FrameRef) -> Option<&mut FrameState> {
        self.frames.get_mut(frame.0 as usize)
    }

    /// The unit being served in `frame`, shared rather than copied, so
    /// the loop can read its oplines while the request is borrowed.
    pub(crate) fn serving(&self, frame: FrameRef) -> Option<Arc<Unit>> {
        self.at(frame).map(|state| Arc::clone(&state.unit))
    }

    /// Where `frame` is to write what it returns, and nothing when the
    /// call that pushed it wants no value.
    pub(crate) fn returns_to(&self, frame: FrameRef) -> Option<(FrameRef, u32)> {
        self.at(frame).and_then(|state| state.returns_to)
    }

    /// Where the loop stands in `frame`.
    pub(crate) fn opline(&self, frame: FrameRef) -> u32 {
        self.at(frame).map_or(0, |state| state.opline)
    }

    /// `zend_get_executed_lineno`: the line of the opline the innermost
    /// frame stands at, which is the line a diagnostic names, and zero
    /// while no frame is being served.
    pub(crate) fn executing_line(&self) -> u32 {
        self.frames
            .last()
            .and_then(|state| opline_at(code_of(&state.unit), state.opline))
            .map_or(0, |op| op.lineno)
    }

    /// `ZEND_VM_SET_OPCODE`: the frame stands at `at`, which is where a
    /// jump leaves it.
    pub(crate) fn jump(&mut self, frame: FrameRef, at: u32) {
        if let Some(state) = self.at_mut(frame) {
            state.opline = at;
        }
    }

    /// `ZEND_VM_NEXT_OPCODE`: on to the opline after this one.
    pub(crate) fn advance(&mut self, frame: FrameRef) {
        if let Some(state) = self.at_mut(frame) {
            state.opline = state.opline.saturating_add(1);
        }
    }

    /// The value in a slot, a view of it: `ZVAL_COPY_VALUE`, no owner
    /// taken. A slot past the end reads as `null`, as an uninitialised
    /// zval reads.
    pub(crate) fn slot(&self, frame: FrameRef, at: u32) -> Value {
        self.at(frame)
            .and_then(|state| state.slots.get(at as usize))
            .cloned()
            .unwrap_or_else(Value::null)
    }

    /// The value out of a slot, the slot left `UNDEF`: what the slot
    /// owned is the caller's now, which is how a temporary is consumed.
    pub(crate) fn take_slot(&mut self, frame: FrameRef, at: u32) -> Value {
        self.at_mut(frame)
            .and_then(|state| state.slots.get_mut(at as usize))
            .map_or_else(Value::null, |slot| std::mem::replace(slot, Value::undef()))
    }

    /// `value` into a slot, the slot owning it from here, and what the
    /// slot held handed back for the caller to release; a slot past the
    /// end takes nothing and hands `value` back.
    pub(crate) fn set_slot(&mut self, frame: FrameRef, at: u32, value: Value) -> Value {
        match self
            .at_mut(frame)
            .and_then(|state| state.slots.get_mut(at as usize))
        {
            Some(slot) => std::mem::replace(slot, value),
            None => value,
        }
    }

    pub(crate) fn push_call(&mut self, call: CallState) {
        self.calls.push(call);
    }

    /// The call the `SEND_*` oplines are placing arguments into.
    pub(crate) fn building_call(&mut self) -> Option<&mut CallState> {
        self.calls.last_mut()
    }

    /// The call being served, for a coercion that writes an argument
    /// back into the frame as php's `Z_PARAM_*` do.
    pub(crate) fn serving_call_mut(&mut self) -> Option<&mut CallState> {
        self.calls
            .last_mut()
            .filter(|call| call.stage == CallStage::Serving)
    }

    /// `DO_*CALL` making the call it serves the current frame,
    /// `EG(current_execute_data) = call`: the callee, and the frame left
    /// on the stack for the callee to read its arguments off. Nothing
    /// under a `DO_*CALL` is none.
    pub(crate) fn activate_call(&mut self) -> Option<Function> {
        let call = self.calls.last_mut()?;
        call.stage = CallStage::Serving;
        Some(call.function)
    }

    /// `zend_vm_stack_free_call_frame`: the served call off the stack,
    /// its arguments and its name handed back for the request to release
    /// (`zend_vm_stack_free_args`).
    pub(crate) fn pop_call(&mut self) -> Option<CallState> {
        self.calls.pop()
    }

    /// The call being served, php's `execute_data` inside a native: the
    /// one the innermost `DO_*CALL` activated, and none while the loop is
    /// between calls or a call is still being built.
    pub(crate) fn serving_call(&self) -> Option<&CallState> {
        self.calls
            .last()
            .filter(|call| call.stage == CallStage::Serving)
    }

    /// The calls being served, innermost first: what a backtrace walks
    /// below the op array being served, `EG(current_execute_data)` and
    /// its `prev_execute_data` chain with the frames being built left
    /// out.
    pub(crate) fn serving_calls(&self) -> impl Iterator<Item = &CallState> {
        self.calls
            .iter()
            .rev()
            .filter(|call| call.stage == CallStage::Serving)
    }
}

/// The op array of `unit` a frame serves.
pub(crate) fn code_of(unit: &Unit) -> &OpArray {
    &unit.main
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
///
/// A CV nothing has written is php's `BP_VAR_R` read of an undefined
/// variable, `zval_undefined_cv` (`Z/Zend/zend_execute.c:242`): the
/// warning naming the variable, and `null` in its place. php makes the
/// CV no less undefined for having been read, so a second read warns
/// again.
///
/// ```php
/// echo $a; $a = 1; echo $a, $a = 2, $a;
/// echo $b, $b;
/// ```
/// ```expect
/// Warning: Undefined variable $a in %s on line 2
/// 122
/// Warning: Undefined variable $b in %s on line 3
///
/// Warning: Undefined variable $b in %s on line 3
/// ```
pub(crate) fn operand(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    kind: OperandKind,
    field: u32,
) -> Value {
    match kind {
        OperandKind::Const => constant(code, ConstIdx(field)),
        OperandKind::Cv => {
            let value = req.vm().slot(frame, field);
            if value.tag() != TypeTag::Undef {
                return value;
            }
            let name = code
                .vars
                .get(field as usize)
                .map(|name| String::from_utf8_lossy(&name.0).into_owned())
                .unwrap_or_default();
            warning(req, &format!("Undefined variable ${name}"));
            Value::null()
        }
        OperandKind::Tmp | OperandKind::Var => req.vm().slot(frame, temporary_slot(code, field)),
        OperandKind::Unused => Value::null(),
    }
}

/// The operand's value as the handler's own: a `CV` or a `CONST` with
/// one more owner taken (`ZVAL_COPY`, `Z_TRY_ADDREF`), a `TMP` or a `VAR`
/// moved out of its slot, which is left `UNDEF` (`ZVAL_COPY_VALUE` and
/// no `FREE_OP`). This is what `ASSIGN`, `SEND_*` and `RETURN` read,
/// each of them a handler that keeps the value past the opline.
pub(crate) fn take_operand(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    kind: OperandKind,
    field: u32,
) -> Value {
    match kind {
        OperandKind::Tmp | OperandKind::Var => {
            req.vm_mut().take_slot(frame, temporary_slot(code, field))
        }
        OperandKind::Const | OperandKind::Cv | OperandKind::Unused => {
            let value = operand(req, frame, code, kind, field);
            req.addref(&value);
            value
        }
    }
}

/// `FREE_OP`: a `TMP` or a `VAR` operand read by [`operand`] and done
/// with, its slot's owner given up; a `CV` and a `CONST` are nobody's to
/// free here and nothing happens.
pub(crate) fn free_operand(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    kind: OperandKind,
    field: u32,
) {
    if matches!(kind, OperandKind::Tmp | OperandKind::Var) {
        let value = req.vm_mut().take_slot(frame, temporary_slot(code, field));
        req.release(value);
    }
}

/// The op array's constant at `at`, as a value.
pub(crate) fn constant(code: &OpArray, at: ConstIdx) -> Value {
    code.literals
        .get(at.0 as usize)
        .copied()
        .map_or_else(Value::null, value_of)
}

/// A zval as the value vocabulary holds it, no owner taken: the string
/// is named and not copied, as php's `ZVAL_COPY_VALUE` names it. Only
/// the forms a compiled program's constant table holds are here; the
/// rest are the memory model's and arrive with it (design §11, Lane A).
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
        Some(s) => Value::string(PhpString::of(s)),
        None => Value::null(),
    }
}

/// The opline at `at` of `code`, if there is one.
pub(crate) fn opline_at(code: &OpArray, at: u32) -> Option<&Opline> {
    code.oplines.get(at as usize)
}
