//! What the recorder reads of a frame between its steps (`jit-design.md`
//! §4.1), which no extension reads and so the native face does not hand
//! out: the type of the value an operand names, as php's
//! `zend_jit_trace_execute` reads `Z_TYPE_P(EX_VAR(var))` before each
//! handler it runs (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:848-940`),
//! whether a frame is a generator's, which the recorder does not
//! follow, and the call an `INIT_*` began, php's `EX(call)`, whose
//! callee the recorder writes down. Reads alone: nothing here writes a
//! frame.

use php_heap::{FrameRef, Slot, Zval, ZvalType};
use php_unit::OperandKind;

use super::{FuncRef, Request};
use crate::api::values::ClassId;

/// One operand's value as the recorder samples it, php's
/// `op1_type`/`op2_type`/`op3_type` of a `ZEND_JIT_TRACE_VM` record with
/// the class a `ZEND_JIT_TRACE_OP1_TYPE` record carries: its type once an
/// `IS_INDIRECT` and then an `IS_REFERENCE` were followed, each marked
/// where it was (`IS_TRACE_INDIRECT`, `IS_TRACE_REFERENCE`), an array's
/// packed layout (`IS_TRACE_PACKED`), and an object's class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub kind: ZvalType,
    pub indirect: bool,
    pub reference: bool,
    pub packed: bool,
    pub class: Option<ClassId>,
}

/// The value the operand of kind `kind` and field `field` names in
/// `frame`, sampled: none for an operand that names no slot of the frame
/// (`IS_CONST`, `IS_UNUSED`), for a frame that does not stand and for
/// one that serves no op array.
///
/// php samples the zval in place, `EX_VAR(opline->op1.var)`; a `CV`'s
/// field is its slot, and so is a temporary's, which the compiler folded
/// past the CVs.
#[must_use]
pub fn sample(req: &Request, frame: FrameRef, kind: OperandKind, field: u32) -> Option<Sample> {
    let at = match kind {
        OperandKind::Cv | OperandKind::Tmp | OperandKind::Var => field,
        OperandKind::Const | OperandKind::Unused => return None,
    };
    req.served_op_array(frame)?;
    let held = req.vm().stack.frame(frame)?;
    if usize::try_from(at).ok()? >= held.len() {
        return None;
    }
    Some(sampled(held.slot(Slot(at))))
}

/// `z` sampled as php's recorder samples a slot: through an
/// `IS_INDIRECT`, then through an `IS_REFERENCE`, each marked.
fn sampled(z: Zval) -> Sample {
    let (z, indirect) = z.as_indirect().map_or((z, false), |at| (at.read(), true));
    let (z, reference) = z
        .as_reference()
        .map_or((z, false), |cell| (cell.val(), true));
    Sample {
        kind: z.kind(),
        indirect,
        reference,
        packed: z.as_arr().is_some_and(|arr| arr.is_packed()),
        class: z.as_obj().map(|obj| ClassId(obj.class())),
    }
}

/// Whether `frame` serves a generator, its frame moved off the stack by
/// `GENERATOR_CREATE` and put back for a resumption: a frame the recorder
/// stops at (`jit-design.md` §4.1, `MovedFrame`).
#[must_use]
pub fn on_heap(req: &Request, frame: FrameRef) -> bool {
    req.frame_at(frame)
        .is_some_and(|at| at.generator().is_some())
}

/// The number a class is known by within its request (design §5.14,
/// *Identities*), which a recorded trace writes in its text form
/// (`jit-design.md` §4.1, `TraceRecord::dump`).
#[must_use]
pub fn class_number(class: ClassId) -> u32 {
    class.0
}

/// The class [`class_number`] gave `number`, read back, as a recorded
/// trace's text form names it; whether any request has such a class is
/// the reader's to ask.
#[must_use]
pub fn numbered_class(number: u32) -> ClassId {
    ClassId(number)
}

/// The call an `INIT_*` opline began and no `DO_*CALL` has made a frame
/// of yet, php's `EX(call)`: the record it stands on, the innermost on
/// the stack, and the op array its callee serves where the callee is a
/// user function, none for a native's call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Building {
    pub call: FrameRef,
    pub target: Option<FuncRef>,
}

/// The call being built at the top of the stack, if one is: what the
/// recorder compares before and after a step, as php's recorder compares
/// `EX(call)` with the call it saw last
/// (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:1233-1288`).
#[must_use]
pub fn building(req: &Request) -> Option<Building> {
    let (call, frame) = req.vm().stack.top()?;
    let header = frame.header();
    if !header.info.building() {
        return None;
    }
    let target = header
        .function
        .and_then(|function| req.user_function(function))
        .and_then(|user| crate::exec::body_of(&user.unit, user.body))
        .map(|code| FuncRef::at(code.runtime_cache));
    Some(Building { call, target })
}
