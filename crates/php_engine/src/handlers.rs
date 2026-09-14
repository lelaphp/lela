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
//! `ZEND_IS_SMALLER`, which a loop's condition is, the rest of the
//! comparison level, `ZEND_IS_SMALLER_OR_EQUAL`, `ZEND_IS_EQUAL`,
//! `ZEND_IS_NOT_EQUAL`, `ZEND_IS_IDENTICAL`, `ZEND_IS_NOT_IDENTICAL` and
//! `ZEND_SPACESHIP`, `ZEND_PRE_INC` and
//! `ZEND_POST_INC`, which step a CV in place, `ZEND_FREE`, which
//! gives up a temporary nothing reads, and the four opcodes php's
//! compiler emits for a call in `zend_try_compile_special_func`'s table
//! (#219): `ZEND_STRLEN`, `ZEND_TYPE_CHECK`, `ZEND_DEFINED` and
//! `ZEND_COUNT`.

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

/// `ZEND_FREE`: a temporary nothing reads, given up where it stands
/// (`FREE_OP1`). `zend_do_free` emits one after an opline it cannot
/// mark `UNUSED`, so `$a + 1;` as a statement is an `ADD` and a `FREE`
/// of its temporary, and what an operator built is given back to the
/// heap on the opline after it, as php gives it back.
///
/// ```php
/// $b = memory_get_usage(); str_repeat("a", 3000) . "b"; var_dump(memory_get_usage() - $b);
/// ```
/// ```expect
/// int(0)
/// ```
pub(crate) fn free<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    Ok(advance(req, frame))
}

/// `ZEND_ECHO`: the operand as a string (`zval_get_string`), written to
/// the request's body, the string given up after the write and the
/// operand freed (`zend_string_release_ex`, `FREE_OP1`).
pub(crate) fn echo<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::operand(req, frame, code, op.op1_type, op.op1);
    let text = crate::ops::to_string(req, &value);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    match text {
        Ok(text) => {
            req.write(text.as_bytes());
            req.release(Value::string(text));
            Ok(advance(req, frame))
        }
        Err(thrown) => {
            req.leave_pending(&thrown);
            Ok(Stepped::Threw(frame))
        }
    }
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
    let name = match crate::ops::to_string(req, &named) {
        Ok(name) => name,
        Err(thrown) => {
            req.leave_pending(&thrown);
            return Ok(Stepped::Threw(frame));
        }
    };
    let Some(function) = req.function_named(name.as_bytes()) else {
        let thrown = crate::api::raising::throw(
            req,
            "Error",
            &format!(
                "Call to undefined function {}()",
                String::from_utf8_lossy(name.as_bytes())
            ),
        );
        req.release(Value::string(name));
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
/// threw, as php frees it on both paths, and its arguments are given up
/// with it (`zend_vm_stack_free_args`). What the native returned is the
/// engine's from here: the temporary owns it under `RETURN_VALUE_USED`,
/// and where the compiler marked the temporary `UNUSED`, nothing reads
/// the value and it is given up at the call, after the arguments, as
/// php's `i_zval_ptr_dtor(ret)` gives it up; that is when its
/// destructor runs (design §5.10).
///
/// ```php
/// $b = memory_get_usage(); str_repeat("a", 3000); var_dump(memory_get_usage() - $b);
/// ```
/// ```expect
/// int(0)
/// ```
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
    req.pop_call();
    match served {
        Ok(value) => {
            if op.out_type == OperandKind::Unused {
                req.release(value);
            } else {
                let slot = exec::temporary_slot(code, op.out);
                let previous = req.set_slot(frame, slot, value);
                req.release(previous);
            }
            Ok(advance(req, frame))
        }
        Err(thrown) => {
            req.leave_pending(&thrown);
            Ok(Stepped::Threw(frame))
        }
    }
}

/// `ZEND_ASSIGN`, `zend_assign_to_variable`
/// (`Z/Zend/zend_execute.h:165`): the value in `op2` becomes the CV's, a
/// temporary moved into it and a CV or a constant copied with one more
/// owner; the temporary `result` names is one more owner again
/// (`ZVAL_COPY`) under `RETURN_VALUE_USED` and is not written where the
/// compiler marked it `UNUSED`, which is every assignment that is a
/// statement; and what the CV held is given up last, after the write,
/// which is where a destructor is served (`i_zval_ptr_dtor(garbage)`).
/// A CV holding a reference is the first thing php tests (`Z_ISREF_P`,
/// line 171): the write goes to the value the cell holds and the cell
/// stays the CV's, so every other holder of the cell reads the new
/// value; the witness of that is `&$x`'s, once it compiles, and the
/// test beside the handlers holds it meanwhile.
///
/// A CV given a new string gives its old one back to the heap, so what
/// `memory_get_usage()` reads falls by what the old string took, and
/// the string a CV alone held is held by the CV alone: the deltas are
/// read before anything is printed, because php's first write lays its
/// `php_output` layer down and holds thirty-two bytes for it.
///
/// ```php
/// $b = memory_get_usage();
/// $s = str_repeat("ab", 3000); $held = memory_get_usage() - $b;
/// $s = "x"; var_dump($held, memory_get_usage() - $b);
/// ```
/// ```expect
/// int(8192)
/// int(0)
/// ```
///
/// An array assigned from a CV is `ZVAL_COPY`, one more owner of the one
/// array and no element copied (design §5.3): the second variable
/// holds it through the first giving it up, and the heap gets the rows
/// back, 1176 bytes for the five of `token_get_all` here, only when the
/// last owner does.
///
/// ```php
/// $t = token_get_all('<?php echo 12;'); $b = memory_get_usage();
/// $u = $t; $t = 1; $shared = memory_get_usage() - $b;
/// $u = 1; var_dump($shared, memory_get_usage() - $b);
/// ```
/// ```expect
/// int(0)
/// int(-1176)
/// ```
pub(crate) fn assign<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::take_operand(req, frame, code, op.op2_type, op.op2);
    if op.out_type != OperandKind::Unused {
        req.addref(&value);
        let slot = exec::temporary_slot(code, op.out);
        let previous = req.set_slot(frame, slot, value.clone());
        req.release(previous);
    }
    if let Some(r) = req.slot(frame, op.op1).as_php_ref() {
        req.reference_set(r.heap(), value);
    } else {
        let garbage = req.set_slot(frame, op.op1, value);
        req.release(garbage);
    }
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

/// `ZEND_IS_SMALLER_OR_EQUAL`: [`crate::ops::is_smaller_or_equal`], the
/// same way, and the opcode `>=` is with its operands swapped.
pub(crate) fn is_smaller_or_equal<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::is_smaller_or_equal)
}

/// `ZEND_IS_EQUAL`: [`crate::ops::is_equal`], the same way. php's
/// handler branches on the answer where a `JMPZ` or a `JMPNZ` follows
/// (`ZEND_VM_SMART_BRANCH`); the compiler emits the two oplines and the
/// jump reads the temporary, which is the same observable path one
/// opline longer.
pub(crate) fn is_equal<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::is_equal)
}

/// `ZEND_IS_NOT_EQUAL`: [`crate::ops::is_not_equal`], the same way.
pub(crate) fn is_not_equal<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::is_not_equal)
}

/// `ZEND_IS_IDENTICAL`: [`crate::ops::is_identical`], the same way, both
/// operands dereferenced (`GET_OP*_ZVAL_PTR_DEREF`), which the kernel
/// does.
pub(crate) fn is_identical<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::is_identical)
}

/// `ZEND_IS_NOT_IDENTICAL`: [`crate::ops::is_not_identical`], the same
/// way.
pub(crate) fn is_not_identical<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::is_not_identical)
}

/// `ZEND_SPACESHIP`: [`crate::ops::spaceship`], the same way, the long
/// `compare_function` writes into the temporary.
pub(crate) fn spaceship<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::spaceship)
}

/// `ZEND_PRE_INC`: [`crate::ops::increment`] over the variable `op1`
/// names, the stepped value written back to it and, under
/// `RETURN_VALUE_USED`, to the temporary `result` names with one more
/// owner (`ZVAL_COPY`), so a string stepped and read is held by the
/// variable and by the temporary until each gives it up. A CV nothing
/// has written is php's `ZVAL_UNDEFINED_OP1`, the warning and null in
/// its place, which `++` then makes `1`.
///
/// ```php
/// $s = "a"; echo ++$s, $s; $t = "y"; echo $t++, $t;
/// ```
/// ```expect
/// Deprecated: Increment on non-numeric string is deprecated, use str_increment() instead in %s on line 2
/// bb
/// Deprecated: Increment on non-numeric string is deprecated, use str_increment() instead in %s on line 2
/// yz
/// ```
pub(crate) fn pre_inc<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    incdec(req, frame, code, op, false, crate::ops::increment)
}

/// `ZEND_POST_INC`: the same step, with the value the variable held
/// before it in the temporary.
pub(crate) fn post_inc<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    incdec(req, frame, code, op, true, crate::ops::increment)
}

/// `ZEND_PRE_DEC`: [`crate::ops::decrement`] over the variable `op1`
/// names, written back and, under `RETURN_VALUE_USED`, into the
/// temporary, as `ZEND_PRE_INC` does with [`crate::ops::increment`]. A
/// CV nothing has written is the warning and null, which `--` leaves
/// null behind its own warning where `++` makes it `1`.
///
/// ```php
/// $u--; var_dump($u); echo --$v, "|", $w--, "|", $w, "|", $x = 5, $x--, --$x, $x;
/// ```
/// ```expect
///
/// Warning: Undefined variable $u in %s on line 2
///
/// Warning: Decrement on type null has no effect, this will change in the next major version of PHP in %s on line 2
/// NULL
///
/// Warning: Undefined variable $v in %s on line 2
///
/// Warning: Decrement on type null has no effect, this will change in the next major version of PHP in %s on line 2
/// |
/// Warning: Undefined variable $w in %s on line 2
///
/// Warning: Decrement on type null has no effect, this will change in the next major version of PHP in %s on line 2
/// ||5533
/// ```
pub(crate) fn pre_dec<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    incdec(req, frame, code, op, false, crate::ops::decrement)
}

/// `ZEND_POST_DEC`: the same step, with the value the variable held
/// before it in the temporary.
///
/// ```php
/// $s = "9"; echo $s--, $s; $t = ""; echo $t--, "|", $t;
/// ```
/// ```expect
/// 98
/// Deprecated: Decrement on empty string is deprecated as non-numeric in %s on line 2
/// |-1
/// ```
pub(crate) fn post_dec<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    incdec(req, frame, code, op, true, crate::ops::decrement)
}

/// `ZEND_STRLEN`: [`crate::ops::strlen`] over the operand, into the
/// temporary the opline's `result` field names; the operand freed after
/// the kernel (`FREE_OP1`), so a temporary string is given up here.
pub(crate) fn strlen<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    unary(req, frame, code, op, crate::ops::strlen)
}

/// `ZEND_TYPE_CHECK`: [`crate::ops::type_check`] over the operand with
/// the mask in `extended_value`, into the temporary; the operand freed
/// after it. php's handler branches on the answer where a `JMPZ` or a
/// `JMPNZ` follows (`ZEND_VM_SMART_BRANCH`); the compiler emits the two
/// oplines and the jump reads the temporary, which is the same
/// observable path one opline longer.
pub(crate) fn type_check<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let mask = op.extended_value;
    unary(req, frame, code, op, |_, value| {
        Ok(Value::bool(crate::ops::type_check(value, mask)))
    })
}

/// `ZEND_DEFINED`: [`crate::ops::defined`] over the constant string
/// `op1` names, into the temporary. The cache slot in `extended_value`
/// is not read: php keeps the answer in the per-request opline cache
/// (design §5.4), and the answer is the same whether or not it was
/// kept, so the cache lands with the fetches whose cost it changes.
pub(crate) fn defined<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    unary(req, frame, code, op, |req, value| {
        let name = value.as_str_bytes().unwrap_or_default();
        Ok(Value::bool(crate::ops::defined(req, name)))
    })
}

/// `ZEND_COUNT`: [`crate::ops::count`] over the operand, `sizeof`'s
/// bit in `extended_value`, into the temporary; the operand freed after
/// it.
pub(crate) fn count<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let sizeof = op.extended_value != 0;
    unary(req, frame, code, op, |req, value| {
        crate::ops::count(req, value, sizeof).map(Value::int)
    })
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
    let taken = crate::ops::is_true(req, &value);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    if !taken {
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

/// `ZEND_RETURN`: the operand taken as the caller's (`ZVAL_COPY` of a
/// CV or a constant, a temporary moved), then the return safepoint, then
/// the frame popped (design §4.3 seam 8, §5.8). The safepoint carries
/// the number the compiler wrote in `result`, and is raised for every
/// frame but the entry frame of this serving, which the stack's entry
/// slot says.
pub(crate) fn r#return<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let returned = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    if req.stack_mut().entry() == frame {
        req.set_returned(returned);
        return Ok(Stepped::Done);
    }
    continued(T::safepoint(req, frame, Site::Return(SafepointIdx(op.out))))?;
    let returns_to = req.vm().returns_to(frame);
    req.pop_frame();
    let caller = req.stack_mut().current();
    match returns_to {
        Some((at, slot)) => {
            let previous = req.set_slot(at, slot, returned);
            req.release(previous);
        }
        None => req.release(returned),
    }
    // The caller stands at the call opline that pushed this frame, and
    // goes on from the one after it.
    req.vm_mut().advance(caller);
    Ok(Stepped::Popped(caller))
}

/// The body the binary operators' handlers share: both operands addressed,
/// the kernel called, both operands freed (`FREE_OP1`, `FREE_OP2`), and
/// what the kernel returned written into the temporary the opline's
/// `result` field names. A kernel that threw leaves the throwable
/// pending, which is the `Threw` edge the loop reads.
fn binary(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    kernel: fn(&mut Request, &Value, &Value) -> Result<Value, Thrown>,
) -> Edge {
    let left = exec::operand(req, frame, code, op.op1_type, op.op1);
    let right = exec::operand(req, frame, code, op.op2_type, op.op2);
    let computed = kernel(req, &left, &right);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    match computed {
        Ok(value) => {
            let slot = exec::temporary_slot(code, op.out);
            let previous = req.set_slot(frame, slot, value);
            req.release(previous);
            Ok(advance(req, frame))
        }
        Err(thrown) => {
            req.leave_pending(&thrown);
            Ok(Stepped::Threw(frame))
        }
    }
}

/// The body the one-operand kernels' handlers share: the operand
/// addressed, the kernel called, the operand freed (`FREE_OP1`), and
/// what the kernel returned written into the temporary the opline's
/// `result` field names. A kernel that threw leaves the throwable
/// pending, which is the `Threw` edge the loop reads.
fn unary(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    kernel: impl FnOnce(&mut Request, &Value) -> Result<Value, Thrown>,
) -> Edge {
    let operand = exec::operand(req, frame, code, op.op1_type, op.op1);
    let computed = kernel(req, &operand);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    match computed {
        Ok(value) => {
            let slot = exec::temporary_slot(code, op.out);
            let previous = req.set_slot(frame, slot, value);
            req.release(previous);
            Ok(advance(req, frame))
        }
        Err(thrown) => {
            req.leave_pending(&thrown);
            Ok(Stepped::Threw(frame))
        }
    }
}

/// The body `PRE_INC`, `POST_INC`, `PRE_DEC` and `POST_DEC` share: the
/// variable's slot, a CV or a `VAR`, read as
/// `GET_OP1_ZVAL_PTR_PTR_UNDEF(BP_VAR_RW)` reads it; the kernel, the
/// step, over what it held; the slot written, and the temporary under
/// `RETURN_VALUE_USED`, which a `POST_*` always is, since
/// `zend_do_free` turns one nothing reads into the `PRE_*`. A kernel
/// that threw leaves the throwable pending and the variable as it was,
/// which is the `Threw` edge the loop reads.
fn incdec(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    post: bool,
    kernel: fn(&mut Request, &Value) -> Result<Value, Thrown>,
) -> Edge {
    let slot = match op.op1_type {
        OperandKind::Cv => op.op1,
        _ => exec::temporary_slot(code, op.op1),
    };
    let held = exec::operand(req, frame, code, op.op1_type, op.op1);
    match kernel(req, &held) {
        Ok(stepped) => {
            if op.out_type != OperandKind::Unused {
                let out = exec::temporary_slot(code, op.out);
                let value = if post {
                    held
                } else {
                    req.addref(&stepped);
                    stepped.clone()
                };
                req.set_slot(frame, out, value);
            }
            req.set_slot(frame, slot, stepped);
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
/// call frame being built, at the argument number `op2` names, the frame
/// owning it from here: a CV or a constant with one more owner
/// (`ZVAL_COPY`, `SEND_VAR`'s and `SEND_VAL`'s copy), a temporary moved
/// (`ZVAL_COPY_VALUE`).
fn send(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let value = exec::take_operand(req, frame, code, op.op1_type, op.op1);
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
    use crate::tier::Nop;

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

    /// A class declaring `__destruct`, as an extension declares one:
    /// what [`Request`]'s side of seam 1 looks for on the class of the
    /// object it is handed.
    fn destructible(name: &[u8]) -> php_unit::decl::ClassDecl {
        use php_unit::decl::{
            ClassDecl, ClassFlags, ClassKind, EnumBacking, FunctionDecl, MethodDecl, Name,
            Visibility,
        };
        ClassDecl {
            name: Name(name.into()),
            kind: ClassKind::Class,
            flags: ClassFlags::default(),
            parent: None,
            interfaces: Vec::new(),
            traits: Vec::new(),
            backing: EnumBacking::Pure,
            constants: Vec::new(),
            properties: Vec::new(),
            methods: vec![MethodDecl {
                function: FunctionDecl {
                    name: Name(b"__destruct".as_slice().into()),
                    parameters: Vec::new(),
                    return_type: None,
                    returns_by_ref: false,
                    attributes: Vec::new(),
                    doc_comment: None,
                    file: Name(Box::new([])),
                    line: 0,
                },
                visibility: Visibility::Public,
                is_instance: true,
                is_abstract: false,
                is_final: false,
                tentative_return: false,
            }],
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
        }
    }

    /// A unit whose main op array has one CV, one temporary and one
    /// `ASSIGN` of the constant `1` into the CV, its value unused:
    /// `$a = 1;`.
    fn assigning() -> Arc<Unit> {
        Arc::new(Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::Assign,
                    op1_type: OperandKind::Cv,
                    op1: 0,
                    op2_type: OperandKind::Const,
                    op2: 0,
                    out_type: OperandKind::Unused,
                    ..Opline::default()
                }]
                .into_boxed_slice(),
                literals: vec![php_heap::Zval::long(1)].into_boxed_slice(),
                num_cvs: 1,
                num_tmps: 1,
                ..OpArray::default()
            },
            ..Unit::default()
        })
    }

    /// Design §5.10 and seam 1: `ASSIGN` over a CV holding an object's
    /// last owner writes the CV and then gives the object up, and the
    /// release reaches the class's `__destruct` through the heap's hooks
    /// with the request whole, since serving a method takes `&mut
    /// Request`. No method can be served until #259 and #260, so what
    /// the destructor's call leaves today is php's `Error` for an
    /// undefined method, pending on the request: that the raise landed
    /// is the proof the seam was crossed, and the witness that reads
    /// `__destruct`'s own bytes replaces this test when a native class
    /// can declare one. The object is freed after the destructor, its
    /// bucket free again, as `zend_objects_store_del` frees it.
    #[test]
    fn an_assign_over_the_last_owner_serves_the_destructor_after_the_write() {
        let destructible = crate::api::abi::ExtensionBuilder::default()
            .classes(vec![destructible(b"Destructible")])
            .build("destructible");
        let engine = Engine::builder().extensions(&[destructible]).build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = assigning();
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let op = code.oplines.first().copied().unwrap_or_default();
        let Ok(object) = req.new_object_of("Destructible") else {
            return;
        };
        let id = php_heap::ObjectId(object.index());
        let previous = req.set_slot(frame, 0, object.as_value());
        req.release(previous);
        assert!(!req.is_pending());

        assert_eq!(
            assign::<Nop>(&mut req, frame, code, &op),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.slot(frame, 0).as_int(), Some(1), "written first");
        assert!(
            req.is_pending(),
            "the release reached the destructor, whose call raised on the request"
        );
        assert!(
            req.heap().stored(id).is_none(),
            "and the object was freed after the destructor"
        );
        assert_eq!(
            req.heap().objects(),
            1,
            "the store holds the pending Error and nothing else"
        );
    }

    /// `zend_assign_to_variable`'s first test (`Z/Zend/zend_execute.h:171`):
    /// `ASSIGN` over a CV holding a reference writes the value the cell
    /// holds, releases what the cell held after the write, and leaves the
    /// cell the CV's, so a second CV bound to the same cell reads the new
    /// value, which is what `$b = &$a; $a = 1;` shows once `&$x`
    /// compiles. The temporary `result` names, under `RETURN_VALUE_USED`,
    /// is the value and never the cell.
    #[test]
    fn an_assign_into_a_cv_holding_a_reference_writes_the_referent() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = assigning();
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let mut op = code.oplines.first().copied().unwrap_or_default();
        op.out_type = OperandKind::Tmp;
        op.out = 0;
        let Ok(text) = crate::api::values::PhpString::new(&mut req, b"in the cell") else {
            return;
        };
        let Ok(cell) = crate::api::values::PhpRef::new(&mut req, Value::string(text)) else {
            return;
        };
        let start = req.memory_usage(false);
        // Two CVs would share the cell; this frame has one, and the test
        // is the second holder.
        req.addref(&cell.as_value());
        let previous = req.set_slot(frame, 0, cell.as_value());
        req.release(previous);

        assert_eq!(
            assign::<Nop>(&mut req, frame, code, &op),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(
            req.slot(frame, 0).as_php_ref(),
            Some(&cell),
            "the CV still holds the cell"
        );
        assert_eq!(cell.get().as_int(), Some(1), "and the cell holds the value");
        assert_eq!(
            req.memory_usage(false),
            start - 40,
            "what the cell held went back to the heap"
        );
        assert_eq!(
            req.slot(frame, 1).as_int(),
            Some(1),
            "the result is the value, not the cell"
        );
        req.pop_frame();
        assert_eq!(
            cell.get().as_int(),
            Some(1),
            "the second holder keeps the cell"
        );
        req.release(cell.as_value());
        assert_eq!(req.memory_usage(false), start - 40 - 32);
    }
}
