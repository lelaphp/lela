//! One function per opcode the engine serves (design §5.8). A handler
//! addresses its operands, calls a kernel in [`crate::ops`] or a native,
//! asks the tier where §4.3 seam 8 says to, and says what it did to the
//! frame chain; it holds no semantic of its own. `build.rs` dispatches to
//! these by name, so a handler here that is no opcode of `ops.toml` fails
//! the build, and every handler is generic over the tier, which is what
//! makes the safepoint a folded branch under `Nop`.
//!
//! The set is the one procedural PHP needs to reach a native: `ECHO`, the
//! call sequence `INIT_FCALL`, `SEND_VAL`, `SEND_VAR`, `DO_ICALL`,
//! `RETURN`, which raises the return safepoint, `JMP` and `JMPNZ`, which
//! raise the loop safepoint when they jump backward, `ASSIGN`, which
//! writes a CV, the arithmetic `ZEND_ADD`, `ZEND_SUB`, `ZEND_MUL`,
//! `ZEND_DIV`, `ZEND_MOD`, `ZEND_POW`, `ZEND_SL` and `ZEND_SR` and the
//! `ZEND_CONCAT` that php emits for what its compiler does not fold,
//! `ZEND_IS_SMALLER`, which a loop's condition is, and `ZEND_PRE_INC` and
//! `ZEND_POST_INC`, which step a CV in place.

// Every handler is generic over the tier and hands back the same edge, so
// the generated dispatch calls them all the same way; the ones that raise a
// safepoint or a fatal are the ones that name the tier or return an `Err`.
#![allow(clippy::extra_unused_type_parameters, clippy::unnecessary_wraps)]

use php_heap::{Fatal, FrameRef};
use php_unit::{ConstIdx, OpArray, OperandKind, Opline};

use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::Value;
use crate::exec::{self, CallStage, CallState};
use crate::tier::{SafepointIdx, Site, Stepped, Tier, continued};

/// What one handler did to the frame chain, and the fatal that ends the
/// request instead.
pub(crate) type Edge = Result<Stepped, Fatal>;

/// `ZEND_NOP`: nothing, and on to the opline after it.
pub(crate) fn nop<T: Tier>(req: &mut Request, frame: FrameRef, _: &OpArray, _: &Opline) -> Edge {
    Ok(advance(req, frame))
}

/// `ZEND_ECHO`: the operand as a string (`zval_get_string`), written to
/// the request's body.
pub(crate) fn echo<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::operand(req, frame, code, op.op1_type, op.op1);
    let text = crate::ops::to_string(req, &value);
    req.write(text.as_bytes());
    Ok(advance(req, frame))
}

/// `ZEND_INIT_FCALL`: the call frame for a callee named at compile time,
/// its name in `op2` and the number of arguments in `extended_value`. A
/// name nothing is declared under is php's `Error`.
pub(crate) fn init_fcall<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let named = exec::constant(code, ConstIdx(op.op2));
    let name = crate::ops::to_string(req, &named);
    let Some(function) = req.function_named(name.as_bytes()) else {
        let thrown = crate::api::raising::throw(
            req,
            "Error",
            &format!(
                "Call to undefined function {}()",
                String::from_utf8_lossy(name.as_bytes())
            ),
        );
        req.leave_pending(&thrown);
        return Ok(Stepped::Threw(frame));
    };
    let count = usize::try_from(op.extended_value).unwrap_or(0);
    req.vm_mut().push_call(CallState {
        function,
        name,
        arguments: Vec::new(),
        count,
        stage: CallStage::Building,
    });
    Ok(advance(req, frame))
}

/// `ZEND_SEND_VAL`: an argument that is a constant or a temporary, placed
/// in the call frame being built.
pub(crate) fn send_val<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    send(req, frame, code, op)
}

/// `ZEND_SEND_VAR`: an argument that is a variable or what a call
/// returned. By value here, since nothing declares a by-reference
/// parameter yet.
pub(crate) fn send_var<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    send(req, frame, code, op)
}

/// `ZEND_DO_ICALL`: serve the native the call frame names, and write
/// what it returned into the temporary `result` names. The frame stays
/// on the stack with its arguments while the native runs, as php's does
/// (`EG(current_execute_data) = call` before `fbc->internal_function.handler`,
/// `zend_vm_stack_free_call_frame` after), so the native reads them off
/// it and a backtrace taken inside the native prints them; nothing copies
/// them onto the request. It is popped whether the native returned or
/// threw, as php frees it on both paths.
pub(crate) fn do_icall<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(function) = req.vm_mut().activate_call() else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_DO_ICALL with no call frame under it",
        )));
    };
    let served = req.serve_native(function);
    req.vm_mut().pop_call();
    match served {
        Ok(value) => {
            let slot = exec::temporary_slot(code, op.out);
            req.vm_mut().set_slot(frame, slot, value);
            Ok(advance(req, frame))
        }
        Err(thrown) => {
            req.leave_pending(&thrown);
            Ok(Stepped::Threw(frame))
        }
    }
}

/// `ZEND_ASSIGN`: the value in `op2` written to the CV `op1` names, and
/// to the temporary `result` names. php writes the temporary only under
/// `RETURN_VALUE_USED` and the compiler marks the mark in `result_type`,
/// which an [`Opline`] has no field for (design §5.7), so the temporary
/// is written whether an opline reads it or not; the compiler numbered
/// it either way, as php's does. `zend_assign_to_variable`'s other work,
/// the reference the CV may hold and the release of what it held, comes
/// with the memory model (design §11, Lane A); a `Value` here is owned
/// and dropped.
pub(crate) fn assign<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::operand(req, frame, code, op.op2_type, op.op2);
    req.vm_mut().set_slot(frame, op.op1, value.clone());
    let slot = exec::temporary_slot(code, op.out);
    req.vm_mut().set_slot(frame, slot, value);
    Ok(advance(req, frame))
}

/// `ZEND_ADD`: [`crate::ops::add`] over the two operands, into the
/// temporary the opline's `result` field names.
pub(crate) fn add<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::add)
}

/// `ZEND_SUB`: [`crate::ops::sub`], the same way.
pub(crate) fn sub<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::sub)
}

/// `ZEND_MUL`: [`crate::ops::mul`], the same way.
pub(crate) fn mul<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::mul)
}

/// `ZEND_DIV`: [`crate::ops::div`], the same way; a divisor of zero
/// leaves php's `DivisionByZeroError` pending where the opline stands.
pub(crate) fn div<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::div)
}

/// `ZEND_MOD`: [`crate::ops::mod`], the same way; a divisor of zero
/// leaves php's `DivisionByZeroError` pending where the opline stands.
pub(crate) fn r#mod<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::r#mod)
}

/// `ZEND_POW`: [`crate::ops::pow`], the same way.
pub(crate) fn pow<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::pow)
}

/// `ZEND_SL`: [`crate::ops::shift_left`], the same way; a negative count
/// leaves php's `ArithmeticError` pending where the opline stands.
pub(crate) fn sl<T: Tier>(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    binary(req, frame, code, op, crate::ops::shift_left)
}

/// `ZEND_SR`: [`crate::ops::shift_right`], the same way.
pub(crate) fn sr<T: Tier>(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    binary(req, frame, code, op, crate::ops::shift_right)
}

/// `ZEND_CONCAT`: [`crate::ops::concat`], the same way.
pub(crate) fn concat<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::concat)
}

/// `ZEND_IS_SMALLER`: [`crate::ops::is_smaller`] over the two operands,
/// into the temporary the opline's `result` field names. php's handler
/// takes the long pair and the double pairs before it calls
/// `zend_compare`, and those are the kernel's first arms.
pub(crate) fn is_smaller<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::is_smaller)
}

/// `ZEND_PRE_INC`: [`crate::ops::increment`] over the variable `op1`
/// names, the stepped value written back to it and to the temporary
/// `result` names, which php writes only under `RETURN_VALUE_USED`
/// (design §5.7, the field #227 brings). A CV nothing has written is
/// php's `ZVAL_UNDEFINED_OP1`, the warning and null in its place, which
/// `++` then makes `1`.
pub(crate) fn pre_inc<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    incdec(req, frame, code, op, false)
}

/// `ZEND_POST_INC`: the same step, with the value the variable held
/// before it in the temporary.
pub(crate) fn post_inc<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    incdec(req, frame, code, op, true)
}

/// `ZEND_JMP`: the unconditional jump `pass_two` leaves, its target
/// opline in `op1`. A jump to an earlier opline is a loop's back-edge
/// and is a safepoint site (design §5.7): the frame is positioned at the
/// target, as a tier is asked with the jump target as the frame's opline
/// (`jit-design.md` §2.2), then the interrupt check, then the site with
/// the number the compiler wrote in `result`. A forward jump is neither
/// and costs one comparison.
pub(crate) fn jmp<T: Tier>(req: &mut Request, frame: FrameRef, _: &OpArray, op: &Opline) -> Edge {
    let back = op.op1 <= req.vm().opline(frame);
    req.vm_mut().jump(frame, op.op1);
    if back {
        crate::interrupt::check(req)?;
        continued(T::safepoint(req, frame, Site::Loop(SafepointIdx(op.out))))?;
    }
    Ok(Stepped::Same(frame))
}

/// `ZEND_JMPNZ`: the jump `zend_emit_cond_jump` leaves at the end of a
/// `for`, a `while` and a `do`, its target opline in `op2`, taken when
/// the operand is true as [`crate::ops::is_true`] reads it. Taken to an
/// earlier opline it is the loop's back-edge and does what [`jmp`] does
/// there: the frame at the target, the interrupt check, the site with
/// the number the compiler wrote in `result`. Not taken, it is
/// `ZEND_VM_NEXT_OPCODE`.
pub(crate) fn jmpnz<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::operand(req, frame, code, op.op1_type, op.op1);
    if !crate::ops::is_true(req, &value) {
        return Ok(advance(req, frame));
    }
    let back = op.op2 <= req.vm().opline(frame);
    req.vm_mut().jump(frame, op.op2);
    if back {
        crate::interrupt::check(req)?;
        continued(T::safepoint(req, frame, Site::Loop(SafepointIdx(op.out))))?;
    }
    Ok(Stepped::Same(frame))
}

/// `ZEND_RETURN`: the operand read, then the return safepoint, then the
/// frame popped (design §4.3 seam 8, §5.8). The safepoint carries the
/// number the compiler wrote in `result`, and is raised for every frame
/// but the entry frame of this serving, which the stack's entry slot
/// says.
pub(crate) fn r#return<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let returned = exec::operand(req, frame, code, op.op1_type, op.op1);
    if req.stack_mut().entry() == frame {
        req.set_returned(returned);
        return Ok(Stepped::Done);
    }
    continued(T::safepoint(req, frame, Site::Return(SafepointIdx(op.out))))?;
    let returns_to = req.vm().returns_to(frame);
    req.pop_frame();
    let caller = req.stack_mut().current();
    if let Some((at, slot)) = returns_to {
        req.vm_mut().set_slot(at, slot, returned);
    }
    // The caller stands at the call opline that pushed this frame, and
    // goes on from the one after it.
    req.vm_mut().advance(caller);
    Ok(Stepped::Popped(caller))
}

/// The body the binary operators' handlers share: both operands addressed,
/// the kernel called, and what it returned written into the temporary
/// the opline's `result` field names. A kernel that threw leaves the
/// throwable pending, which is the `Threw` edge the loop reads.
fn binary(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    kernel: fn(&mut Request, &Value, &Value) -> Result<Value, Thrown>,
) -> Edge {
    let left = exec::operand(req, frame, code, op.op1_type, op.op1);
    let right = exec::operand(req, frame, code, op.op2_type, op.op2);
    match kernel(req, &left, &right) {
        Ok(value) => {
            let slot = exec::temporary_slot(code, op.out);
            req.vm_mut().set_slot(frame, slot, value);
            Ok(advance(req, frame))
        }
        Err(thrown) => {
            req.leave_pending(&thrown);
            Ok(Stepped::Threw(frame))
        }
    }
}

/// The body `PRE_INC` and `POST_INC` share: the variable's slot, a CV
/// or a `VAR`, read as `GET_OP1_ZVAL_PTR_PTR_UNDEF(BP_VAR_RW)` reads it;
/// the kernel over what it held; the slot and the temporary written. A
/// kernel that threw leaves the throwable pending and the variable as it
/// was, which is the `Threw` edge the loop reads.
fn incdec(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline, post: bool) -> Edge {
    let slot = match op.op1_type {
        OperandKind::Cv => op.op1,
        _ => exec::temporary_slot(code, op.op1),
    };
    let held = exec::operand(req, frame, code, op.op1_type, op.op1);
    match crate::ops::increment(req, &held) {
        Ok(stepped) => {
            let out = exec::temporary_slot(code, op.out);
            req.vm_mut()
                .set_slot(frame, out, if post { held } else { stepped.clone() });
            req.vm_mut().set_slot(frame, slot, stepped);
            Ok(advance(req, frame))
        }
        Err(thrown) => {
            req.leave_pending(&thrown);
            Ok(Stepped::Threw(frame))
        }
    }
}

/// `ZEND_VM_NEXT_OPCODE`: the same frame, at the opline after this one.
fn advance(req: &mut Request, frame: FrameRef) -> Stepped {
    req.vm_mut().advance(frame);
    Stepped::Same(frame)
}

/// The body the two `SEND_*` handlers share: the operand placed in the
/// call frame being built, at the argument number `op2` names.
fn send(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let value = exec::operand(req, frame, code, op.op1_type, op.op1);
    let at = usize::try_from(op.op2.saturating_sub(1)).unwrap_or(usize::MAX);
    let Some(call) = req.vm_mut().building_call() else {
        return Err(Fatal::Error(String::from(
            "InternalError: a SEND_* opline with no call frame under it",
        )));
    };
    // The `INIT_FCALL` said how many arguments the call has, so an
    // argument number past that is a compiler bug and not an allocation.
    if at >= call.count {
        return Err(Fatal::Error(format!(
            "InternalError: a SEND_* opline places argument {} of a call of {}",
            at.saturating_add(1),
            call.count
        )));
    }
    if call.arguments.len() <= at {
        call.arguments.resize_with(at + 1, Value::null);
    }
    if let Some(slot) = call.arguments.get_mut(at) {
        *slot = value;
    }
    Ok(advance(req, frame))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use php_unit::{Opcode, OperandKind, Unit};

    use super::*;
    use crate::engine::{Engine, RequestDescription};
    use crate::probe::{Asked, Probe};

    /// A unit whose main op array is one `JMP` to `target`, its
    /// safepoint number `number`.
    fn jumping(target: u32, number: u32) -> Arc<Unit> {
        Arc::new(Unit {
            main: OpArray {
                oplines: vec![
                    Opline::default(),
                    Opline {
                        opcode: Opcode::Jmp,
                        op1: target,
                        out: number,
                        ..Opline::default()
                    },
                ]
                .into_boxed_slice(),
                ..OpArray::default()
            },
            ..Unit::default()
        })
    }

    /// Design §5.7, §5.8: a `JMP` to an earlier opline is a back-edge and
    /// raises the loop safepoint with the number the compiler wrote; one
    /// to a later opline raises nothing. Both leave the frame at the
    /// target, which is the opline a tier is asked with.
    #[test]
    fn a_backward_jump_raises_the_loop_safepoint_and_a_forward_one_does_not() {
        let engine = Engine::builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = jumping(0, 1);
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let back = code.oplines.get(1).copied().unwrap_or_default();
        req.vm_mut().jump(frame, 1);

        assert_eq!(
            jmp::<Probe>(&mut req, frame, code, &back),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().opline(frame), 0);
        assert_eq!(
            req.state::<Asked>().sites,
            vec![(Site::Loop(SafepointIdx(1)), frame)]
        );

        let forward = Opline {
            opcode: Opcode::Jmp,
            op1: 1,
            ..Opline::default()
        };
        assert_eq!(
            jmp::<Probe>(&mut req, frame, code, &forward),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().opline(frame), 1);
        assert_eq!(req.state::<Asked>().sites.len(), 1);
    }

    /// Design §5.8: the interrupt check at the back-edge, before the
    /// site, so a request whose time is up stops within one iteration
    /// and the tier is not asked.
    #[test]
    fn the_interrupt_is_tested_at_a_back_edge() {
        let engine = Engine::builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = jumping(0, 1);
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let back = code.oplines.get(1).copied().unwrap_or_default();
        req.vm_mut().jump(frame, 1);
        req.interrupt().elapsed(1);

        assert_eq!(
            jmp::<Probe>(&mut req, frame, code, &back),
            Err(Fatal::Error(String::from(
                "Maximum execution time of 1 second exceeded"
            )))
        );
        assert_eq!(req.state::<Asked>().sites, vec![]);
    }

    /// A unit whose main op array is `NOP` then a `JMPNZ` over the
    /// constant at slot zero, aimed at the `NOP`, with safepoint number
    /// one: the shape of `for (;;)` after a body of one opline.
    fn conditional(literal: php_heap::Zval) -> Arc<Unit> {
        Arc::new(Unit {
            main: OpArray {
                oplines: vec![
                    Opline::default(),
                    Opline {
                        opcode: Opcode::Jmpnz,
                        op1_type: OperandKind::Const,
                        op1: 0,
                        op2: 0,
                        out: 1,
                        ..Opline::default()
                    },
                ]
                .into_boxed_slice(),
                literals: vec![literal].into_boxed_slice(),
                ..OpArray::default()
            },
            ..Unit::default()
        })
    }

    /// Design §5.7, §5.8: a `JMPNZ` taken to an earlier opline is the
    /// back-edge and raises the loop safepoint as `JMP` does; one not
    /// taken goes on to the next opline and raises nothing.
    #[test]
    fn a_conditional_jump_is_the_back_edge_only_when_it_is_taken() {
        let engine = Engine::builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let taken = conditional(php_heap::Zval::bool(true));
        let frame = req.push_frame(Arc::clone(&taken));
        let code = crate::exec::code_of(&taken);
        let op = code.oplines.get(1).copied().unwrap_or_default();
        req.vm_mut().jump(frame, 1);

        assert_eq!(
            jmpnz::<Probe>(&mut req, frame, code, &op),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().opline(frame), 0);
        assert_eq!(
            req.state::<Asked>().sites,
            vec![(Site::Loop(SafepointIdx(1)), frame)]
        );

        let not_taken = conditional(php_heap::Zval::long(0));
        let frame = req.push_frame(Arc::clone(&not_taken));
        let code = crate::exec::code_of(&not_taken);
        req.vm_mut().jump(frame, 1);
        assert_eq!(
            jmpnz::<Probe>(&mut req, frame, code, &op),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().opline(frame), 2, "on to the opline after it");
        assert_eq!(
            req.state::<Asked>().sites.len(),
            1,
            "nothing more was asked"
        );
    }

    /// A unit whose main op array is one `RETURN null`, its safepoint
    /// number `number`, as the compiler writes it into `result`.
    fn returning(number: u32) -> Arc<Unit> {
        Arc::new(Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::Return,
                    op1_type: OperandKind::Const,
                    op1: 0,
                    out: number,
                    ..Opline::default()
                }]
                .into_boxed_slice(),
                literals: vec![php_heap::Zval::null()].into_boxed_slice(),
                ..OpArray::default()
            },
            ..Unit::default()
        })
    }

    /// Design §4.3 seam 8: `RETURN` raises the return safepoint, with the
    /// number the compiler wrote, for a frame that is not the entry frame
    /// of this serving, and pops it to its caller.
    #[test]
    fn a_return_in_no_entry_frame_raises_the_return_safepoint_and_pops() {
        let engine = Engine::builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = returning(1);
        let outer = req.push_frame(Arc::clone(&unit));
        let guard = req.stack_mut().push_entry(outer);
        let inner = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let op = code.oplines.first().copied().unwrap_or_default();

        assert_eq!(
            r#return::<Probe>(&mut req, inner, code, &op),
            Ok(Stepped::Popped(outer))
        );
        assert_eq!(
            req.state::<Asked>().sites,
            vec![(Site::Return(SafepointIdx(1)), inner)]
        );

        // The entry frame's own return raises nothing and ends the loop.
        assert_eq!(
            r#return::<Probe>(&mut req, outer, code, &op),
            Ok(Stepped::Done)
        );
        assert_eq!(req.state::<Asked>().sites.len(), 1);
        req.stack_mut().pop_entry(guard);
    }
}
