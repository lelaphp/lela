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

use super::{EpochClassId, EpochFunction};
use super::{FuncRef, Request};
use crate::api::values::ClassId;

/// One operand's value as the recorder samples it, php's
/// `op1_type`/`op2_type`/`op3_type` of a `ZEND_JIT_TRACE_VM` record with
/// the class a `ZEND_JIT_TRACE_OP1_TYPE` record carries: its type once an
/// `IS_INDIRECT` and then an `IS_REFERENCE` were followed, each marked
/// where it was (`IS_TRACE_INDIRECT`, `IS_TRACE_REFERENCE`), an array's
/// packed layout (`IS_TRACE_PACKED`), and an object's class, named as a
/// `C`: by the request's number as [`sample`] reads it, and as a
/// recording names it in its record ([`RecordClass`](super::RecordClass),
/// made by [`UnitView::record_class`](super::UnitView::record_class)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, php_macros::EngineState)]
pub struct Sample<C = ClassId> {
    pub kind: ZvalType,
    pub indirect: bool,
    pub reference: bool,
    pub packed: bool,
    pub class: Option<C>,
}

impl<C> Sample<C> {
    /// The sample with its class named as `name` names it.
    pub fn named<D>(self, name: impl FnOnce(C) -> D) -> Sample<D> {
        Sample {
            kind: self.kind,
            indirect: self.indirect,
            reference: self.reference,
            packed: self.packed,
            class: self.class.map(name),
        }
    }
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

/// The class of `frame`'s `$this`, `Z_OBJCE(EX(This))`, which the
/// recorder writes down for a property opline or a method's call on
/// `$this` as it writes down an operand's: none for a frame with no
/// `$this`.
#[must_use]
pub fn this_class(req: &Request, frame: FrameRef) -> Option<ClassId> {
    let this = req.vm().at(frame)?.this()?;
    Some(ClassId(this.heap().class()))
}

/// Whether `frame` serves a generator, its frame moved off the stack by
/// `GENERATOR_CREATE` and put back for a resumption: a frame the recorder
/// stops at (`jit-design.md` §4.1, `MovedFrame`).
#[must_use]
pub fn on_heap(req: &Request, frame: FrameRef) -> bool {
    req.frame_at(frame)
        .is_some_and(|at| at.generator().is_some())
}

/// The number every request of the epoch knows a shared class by
/// (design §5.14, *Identities*, revision 183), which compiled code
/// compares an object's class against: only a class the view minted an
/// [`EpochClassId`] for has one, so no code compiled for the epoch names
/// a request's own class by its number.
#[must_use]
pub fn class_number(class: EpochClassId) -> u32 {
    ClassId::from(class).0
}

/// The number every request of the epoch calls a function it shares by,
/// as compiled code compares a pair's callee against it: only a function
/// the view minted an [`EpochFunction`] for has one.
#[must_use]
pub fn function_number(function: EpochFunction) -> u32 {
    crate::api::call::Function::from(function).0
}

/// The number a class is known by within one request, a request's own
/// class's among them, as that request's runtime cache holds it: what a
/// test reads beside a slot it touched. No record holds one
/// ([`RecordClass`](super::RecordClass)).
#[must_use]
pub fn recorded_number(class: ClassId) -> u32 {
    class.0
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
    building_at(req, building_call(req)?)
}

/// The return type the native a call being built at `call` serves
/// declares, php's `arg_info[-1]` that `zend_jit_trace_build_tssa` types
/// what a `DO_ICALL` writes by: none for a user function's call, and for a
/// native that declares none or returns by reference.
#[must_use]
pub fn native_returns(req: &Request, call: FrameRef) -> Option<php_unit::decl::Type> {
    let function = req.vm().stack.frame(call)?.header().function()?;
    if req.user_function(function).is_some() {
        return None;
    }
    let decl = req.symbols().function_decl(function)?;
    if decl.returns_by_ref {
        return None;
    }
    decl.return_type.clone()
}

/// The calls `frame` began and has not made yet, the outermost first:
/// php's `EX(call)` and the `prev_execute_data` chain under it, which a
/// recording that starts in `frame`, or follows a return into it, writes
/// down as its fake inits, one per call
/// (`zend_jit_trace_record_fake_init_call`,
/// `Z/ext/opcache/jit/zend_jit_vm_helpers.c:631-682`). Each is a record
/// on the stack above `frame`, the innermost on top.
#[must_use]
pub fn under_way(req: &Request, frame: FrameRef) -> Vec<Building> {
    let mut calls = Vec::new();
    let Some((top, _)) = req.vm().stack.top() else {
        return calls;
    };
    for at in (frame.0.saturating_add(1)..=top.0).rev() {
        let call = FrameRef(at);
        let built = req
            .vm()
            .stack
            .frame(call)
            .is_some_and(|record| record.header().info.building());
        match building_at(req, call).filter(|_| built) {
            Some(building) => calls.push(building),
            None => break,
        }
    }
    calls.reverse();
    calls
}

/// The call being built at `call`, and its callee's op array where it is
/// a user function.
fn building_at(req: &Request, call: FrameRef) -> Option<Building> {
    let header = req.vm().stack.frame(call)?.header();
    let target = header
        .function()
        .and_then(|function| req.user_function(function))
        .and_then(|user| crate::exec::body_of(user.unit(), user.body()))
        .map(|code| FuncRef::at(code.runtime_cache));
    Some(Building { call, target })
}

/// The record of the call being built at the top of the stack, if one
/// is: [`building`]'s `call` alone, what compiled code follows as
/// `EX(call)` after a kernel, read without its callee's lookup. Inlined
/// into the tier's kernels across the crate, as every step reads it.
#[must_use]
#[inline]
pub fn building_call(req: &Request) -> Option<FrameRef> {
    let (call, frame) = req.vm().stack.top()?;
    frame.header().info.building().then_some(call)
}
