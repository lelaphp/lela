//! One function per opcode the engine serves (design §5.8). A handler
//! addresses its operands, calls a kernel in [`crate::ops`] or a native,
//! asks the tier where §4.3 seam 8 says to, and says what it did to the
//! frame chain; it holds no semantic of its own. `build.rs` dispatches to
//! these by name, so a handler here that is no opcode of `ops.toml` fails
//! the build, and every handler is generic over the tier, which is what
//! makes the safepoint a folded branch under `Nop`.
//!
//! The set is the one procedural PHP needs to reach a native: `ECHO`, the
//! call sequence `INIT_FCALL`, `SEND_VAL`, `SEND_VAR`, `DO_ICALL`, and
//! the one php's compiler emits for a name its function table has not,
//! `INIT_FCALL_BY_NAME`, `SEND_VAL_EX`, `SEND_VAR_EX`,
//! `SEND_VAR_NO_REF_EX`, `DO_FCALL_BY_NAME` (#252), `RETURN`, which
//! raises the return safepoint, `JMP` and `JMPNZ`, which
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
//! `ZEND_COUNT`; and the ones a `switch` and a `match` compile to
//! (#256): `ZEND_SWITCH_LONG`, `ZEND_SWITCH_STRING` and `ZEND_MATCH`
//! over their jump tables, `ZEND_CASE` and `ZEND_CASE_STRICT`, which
//! keep the subject alive for the next case, `ZEND_QM_ASSIGN`, which
//! moves an arm's value into the one temporary, `ZEND_MATCH_ERROR`,
//! which throws for the subject no arm took, and `ZEND_JMPZ`;
//! the frameless call `zend_compile_frameless_icall` emits
//! for a native with an entry of the call's arity (#254),
//! `ZEND_FRAMELESS_ICALL_0` to `ZEND_FRAMELESS_ICALL_3`, and the two
//! `zend_compile_ns_call` puts around it for an unqualified name in a
//! namespace, `ZEND_JMP_FRAMELESS` and `ZEND_INIT_NS_FCALL_BY_NAME`;
//! and `ZEND_CLONE`, the class's `clone_obj` through its handler table
//! (#281); and `ZEND_ASSIGN_REF`, which binds two CVs to one cell
//! (#286); and the three a `foreach` over an array compiles to (#305):
//! `ZEND_FE_RESET_R`, `ZEND_FE_FETCH_R` and `ZEND_FE_FREE`; and the
//! four an interpolated string compiles to (#355): `ZEND_FAST_CONCAT`
//! for two pieces, `ZEND_ROPE_INIT`, `ZEND_ROPE_ADD` and `ZEND_ROPE_END`
//! for more; and the ones a user function's call and body need (#309):
//! `ZEND_DO_UCALL`, which enters the callee's op array as the
//! `ZEND_USER_FUNCTION` arm of every `DO_*CALL` does, `ZEND_RECV` and
//! `ZEND_RECV_INIT`, the parameter prologue, `ZEND_FETCH_THIS` on a
//! frame with a receiver, and `ZEND_FETCH_CLASS_CONSTANT`.

// Every handler is generic over the tier and hands back the same edge, so
// the generated dispatch calls them all the same way; the ones that raise a
// safepoint or a fatal are the ones that name the tier or return an `Err`.
#![allow(clippy::extra_unused_type_parameters, clippy::unnecessary_wraps)]

use php_heap::{Fatal, FrameRef};
use php_unit::{CacheSlot, ConstIdx, OpArray, OperandKind, Opline};

use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{PhpArray, TypeTag, Value, ValueRef};
use crate::exec::{self, Cached, CallStage, CallState};
use crate::ops::{Offset, Switched};
use crate::tier::{SafepointIdx, Site, Stepped, Tier, continued};
use php_unit::Opcode;

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

/// `ZEND_DECLARE_CLASS`: `zend_bind_class` for the class the opline's
/// lowered name finds in the frame's unit, which is what php emits for
/// every class it did not early bind — one with an interface list, one
/// whose parent `CG(class_table)` did not yet hold, and one written
/// inside a block or a body (`zend_compile_class_decl`'s tail). php
/// finds its declaration by the runtime definition key beside `op1` and
/// its parent by `op2`; the unit carries one declaration per class, so
/// both are the declaration the name finds, and `op2` is not read.
/// Linking is [`Request::bind_class`], whose refusals are php's:
/// `Error` for a parent or an interface nothing declares, `E_ERROR` for
/// a parent that is an interface or an interface list that names
/// something else.
///
/// ```php
/// class C extends P {}
/// class P { const K = 'p'; }
/// var_dump(C::K, (new C) instanceof P);
/// ```
/// ```expect
/// string(1) "p"
/// bool(true)
/// ```
pub(crate) fn declare_class<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(serving) = req.vm().serving(frame) else {
        return Err(Fatal::Error(String::from(
            "InternalError: a DECLARE_CLASS on a frame serving no op array",
        )));
    };
    let Some(lowered) = exec::constant(code, ConstIdx(op.op1))
        .as_str_bytes()
        .map(<[u8]>::to_vec)
    else {
        return Err(Fatal::Error(String::from(
            "InternalError: a DECLARE_CLASS whose op1 is no class name",
        )));
    };
    match req.bind_class(&serving.unit, &lowered) {
        Ok(()) => Ok(advance(req, frame)),
        Err(thrown) => {
            req.leave_pending(&thrown);
            Ok(Stepped::Threw(frame))
        }
    }
}

/// `ZEND_DECLARE_FUNCTION` (`Z/Zend/zend_vm_def.h:8013`):
/// `do_bind_function` over the op array the frame's
/// `dynamic_func_defs` holds at `op2`, under the lowered name in
/// `op1`. php's compiler emits one for every function written below
/// the file's own statement list — inside a block, a loop, a `try` or
/// another body — so such a function is callable from where its
/// statement ran and from nowhere earlier, and one whose statement
/// never runs is never declared. A name the function table already
/// holds is `do_bind_function_error` at `E_ERROR`, php's fatal
/// ([`Request::bind_function`]).
///
/// ```php
/// for ($i = 0; $i < 1; $i = $i + 1) { function later() { return 7; } }
/// var_dump(later());
/// ```
/// ```expect
/// int(7)
/// ```
///
/// ```php
/// function f() { function g() { return 1; } }
/// f();
/// echo "bound\n";
/// g();
/// f();
/// ```
/// ```expect exit=255
/// bound
///
/// Fatal error: Cannot redeclare function g() (previously declared in %s:2) in %s on line 2
/// ```
///
/// ```php
/// later();
/// for ($i = 0; $i < 1; $i = $i + 1) { function later() { return 7; } }
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to undefined function later() in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn declare_function<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(serving) = req.vm().serving(frame) else {
        return Err(Fatal::Error(String::from(
            "InternalError: a DECLARE_FUNCTION on a frame serving no op array",
        )));
    };
    let Some(at) = code.dynamic_func_defs.get(op.op2 as usize).copied() else {
        return Err(Fatal::Error(String::from(
            "InternalError: a DECLARE_FUNCTION whose op2 names no dynamic function definition",
        )));
    };
    let Some(lowered) = exec::constant(code, ConstIdx(op.op1))
        .as_str_bytes()
        .map(<[u8]>::to_vec)
    else {
        return Err(Fatal::Error(String::from(
            "InternalError: a DECLARE_FUNCTION whose op1 is no function name",
        )));
    };
    match req.bind_function(&serving.unit, at, &lowered) {
        Ok(()) => Ok(advance(req, frame)),
        Err(thrown) => {
            req.leave_pending(&thrown);
            Ok(Stepped::Threw(frame))
        }
    }
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

/// `ZEND_INIT_FCALL` (`Z/Zend/zend_vm_def.h:4057`): the call frame for a
/// callee named at compile time, its lowered name in `op2` and the
/// number of arguments in `extended_value`. The callee is read off the
/// cache slot `result.num` names and, on the first pass, looked up and
/// written there (`CACHED_PTR`, `CACHE_PTR`), as the two `INIT_*_BY_NAME`
/// read and write theirs. A name nothing is declared under is php's
/// `Error`, over the same constant.
pub(crate) fn init_fcall<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let name = ConstIdx(op.op2);
    init_call(req, frame, code, op, &[name], name)
}

/// `ZEND_INIT_FCALL_BY_NAME` (`Z/Zend/zend_vm_def.h:3871`): the call
/// frame for a callee php's compiler found no function under, resolved
/// here instead. `op2` names the constant pair
/// `zend_add_func_name_literal` laid down, the name as written and then
/// its lowered form: the lookup is by the second
/// (`Z_STR_P(function_name+1)`) and the `Error` for a name nothing
/// declares says the first (`zend_undefined_function_helper`), which is
/// why `NoPe()` is named as written. The callee is read off the cache
/// slot `result.num` names, `CACHED_PTR(opline->result.num)`, and the
/// table is looked up only where the slot holds nothing yet, the callee
/// then written into it for every pass after; the test beside the
/// handlers counts one lookup over two passes. The frame it pushes is
/// the one `INIT_FCALL` pushes, so the `SEND_*_EX` and
/// `DO_FCALL_BY_NAME` after it are served the same way.
///
/// ```php
/// $a = 'x';
/// echo NoPe($a);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to undefined function NoPe() in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// The `Error` is raised at the `INIT`, before any argument is sent, so
/// an argument's own diagnostic is never raised: no warning for `$u`.
///
/// ```php
/// echo nope($u . 'y');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to undefined function nope() in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn init_fcall_by_name<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let written = ConstIdx(op.op2);
    let lowered = ConstIdx(op.op2.saturating_add(1));
    init_call(req, frame, code, op, &[lowered], written)
}

/// The body the three `INIT_*` handlers share: the callee off the cache
/// slot `result.num` names, or, where the slot is unprimed, looked up
/// under the constants at `lookups` in order and written into the slot
/// (`CACHED_PTR`, then `CACHE_PTR(opline->result.num, fbc)`); the
/// `Error` for a name nothing declares naming the constant at `named`;
/// and the call frame pushed with the number of arguments
/// `extended_value` says. A miss primes nothing, as php's does not: the
/// `Error` is thrown before `CACHE_PTR` is reached, so a name declared
/// after a failed pass is found on the next.
fn init_call(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    lookups: &[ConstIdx],
    named: ConstIdx,
) -> Edge {
    let named = exec::constant(code, named);
    let name = match crate::ops::to_string(req, &named) {
        Ok(name) => name,
        Err(thrown) => {
            req.leave_pending(&thrown);
            return Ok(Stepped::Threw(frame));
        }
    };
    let slot = CacheSlot(op.out);
    let function = if let Cached::Function(function) = req.vm().cached(frame, slot) {
        Some(function)
    } else {
        let found = lookups.iter().find_map(|&at| {
            let key = exec::constant(code, at);
            exec::find_function(req, key.as_str_bytes().unwrap_or(name.as_bytes()))
        });
        if let Some(function) = found {
            req.vm_mut().prime(frame, slot, Cached::Function(function));
        }
        found
    };
    let Some(function) = function else {
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
        name: Some(name),
        arguments: Vec::new(),
        count,
        this: None,
        called_scope: None,
        stage: CallStage::Building,
        frameless: false,
        coerced: Vec::new(),
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

/// `ZEND_SEND_REF` (`Z/Zend/zend_vm_def.h:5087`), the argument a call
/// site whose callee the table has places for a by-reference parameter:
/// the variable is made a reference where it holds none
/// (`ZVAL_MAKE_REF_EX(varptr, 2)`, the cell taking the variable's owner
/// and the argument's at once), and the cell is what the call frame
/// holds. The callee's CV is that cell, so a write through the parameter
/// is a write to the caller's variable, and a `null` is what an
/// unwritten variable is bound to (`BP_VAR_W` warns of nothing).
///
/// ```php
/// function f(&$x, &$u) { $x = 2; $u = 3; }
/// $a = 1;
/// f($a, $b);
/// var_dump($a, $b);
/// ```
/// ```expect
/// int(2)
/// int(3)
/// ```
pub(crate) fn send_ref<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(at) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_SEND_REF over an operand php compiles no write over",
        )));
    };
    let held = exec::value_of(at.read());
    let cell = if let Some(cell) = held.as_php_ref() {
        *cell
    } else {
        let cell = match req.reference(held.undef_as_null()) {
            Ok(cell) => cell,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        // The owner the variable had of what it held is the cell's.
        at.write(cell.as_value().to_zval());
        cell
    };
    let bound = cell.as_value();
    req.addref(&bound);
    place_argument(req, frame, op, bound)
}

/// `ARG_MUST_BE_SENT_BY_REF` (`Z/Zend/zend_compile.h:1137`) over the
/// callee the `INIT` resolved: whether the parameter numbered `number`
/// takes its argument by reference, the variadic's bit for every
/// argument past the declared ones.
fn must_be_sent_by_ref(req: &Request, number: u32) -> bool {
    let Some(function) = req.vm().building_call_function() else {
        return false;
    };
    let Some(decl) = req.function_decl(function) else {
        return false;
    };
    let at = usize::try_from(number.saturating_sub(1)).unwrap_or(usize::MAX);
    let variadic = decl.parameters.last().filter(|last| last.variadic);
    decl.parameters
        .get(at)
        .filter(|parameter| !parameter.variadic)
        .or(variadic)
        .is_some_and(|parameter| parameter.by_ref)
}

/// `zend_cannot_pass_by_reference` (`Z/Zend/zend_execute.c`): the
/// `Error` an argument that is no variable raises where the parameter
/// takes one by reference, which php delays from the compiler to this
/// opline.
fn cannot_pass_by_reference(req: &mut Request, number: u32) -> Thrown {
    let function = req.vm().building_call_function();
    let name = function.map_or_else(String::new, |function| req.qualified_name_of(function));
    let at = usize::try_from(number.saturating_sub(1)).unwrap_or(usize::MAX);
    let parameter = function
        .and_then(|function| req.function_decl(function))
        .and_then(|decl| decl.parameters.get(at).cloned())
        .map(|parameter| format!(" (${})", String::from_utf8_lossy(&parameter.name.0)))
        .unwrap_or_default();
    let message =
        format!("{name}(): Argument #{number}{parameter} could not be passed by reference");
    crate::api::raising::throw(req, "Error", &message)
}

/// `ZEND_SEND_VAL_EX`: `SEND_VAL` for an argument that is no variable,
/// which reads the parameter's by-reference bit off the callee the
/// `INIT` resolved (`ARG_MUST_BE_SENT_BY_REF`) and raises php's `Error`
/// where it is set, since a constant has no place for a reference to
/// point at. The compiler emits it for an argument it read as a value
/// where the callee it knows takes one by reference, which is why the
/// error is raised here and not where the argument is written, and for
/// every argument of a callee it knows nothing of.
///
/// ```php
/// function f(&$x) {}
/// f(1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: f(): Argument #1 ($x) could not be passed by reference in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// echo nope('x', 1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to undefined function nope() in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn send_val_ex<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if must_be_sent_by_ref(req, op.op2) {
        let thrown = cannot_pass_by_reference(req, op.op2);
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        return threw(req, frame, &thrown);
    }
    send(req, frame, code, op)
}

/// `ZEND_SEND_VAR_EX`: `SEND_VAR` for a variable sent to a callee the
/// compiler knew nothing of, which reads the parameter's by-reference
/// bit off the callee the `INIT` resolved
/// (`ARG_SHOULD_BE_SENT_BY_REF`) and dispatches to [`send_ref`] where it
/// is set. A method call is where a program reaches it: the compiler
/// knows a function's declaration at its call site and a method's only
/// through the receiver, which it does not follow, so `$o->m($a)` is
/// this opline and the by-reference binding is read here.
///
/// ```php
/// class C { function m(&$x) { $x = 2; } }
/// $o = new C;
/// $o->m($a);
/// var_dump($a);
/// ```
/// ```expect
/// int(2)
/// ```
///
/// ```php
/// $a = 'x';
/// echo nope($a);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to undefined function nope() in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn send_var_ex<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if must_be_sent_by_ref(req, op.op2) {
        return send_ref::<T>(req, frame, code, op);
    }
    send(req, frame, code, op)
}

/// `ZEND_SEND_VAR_NO_REF_EX`: what a call returned, sent to a callee the
/// compiler knew nothing of; `SEND_VAR` where the parameter is not by
/// reference (`ZEND_VM_C_GOTO(send_var)`), which is every parameter
/// here; reached as [`send_val_ex`] is.
///
/// ```php
/// echo nope(strlen('ab'), nope2('y'));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to undefined function nope() in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn send_var_no_ref_ex<T: Tier>(
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
    do_call(req, frame, code, op, "ZEND_DO_ICALL")
}

/// `ZEND_DO_FCALL_BY_NAME`: serve the callee an `INIT_FCALL_BY_NAME`
/// resolved. Every callee this engine has is a native, so this is the
/// handler's `ZEND_INTERNAL_FUNCTION` arm, which is `DO_ICALL`'s body:
/// the frame stays on the stack while the native runs, is popped whether
/// it returned or threw, and what it returned is the temporary's under
/// `RETURN_VALUE_USED` and given up otherwise. The `ZEND_USER_FUNCTION`
/// arm, which enters the callee's op array, comes with user functions.
/// Reached as [`send_val_ex`] is, and held by the test beside the
/// handlers; the witness pins the `INIT` raising before it.
///
/// ```php
/// nope();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to undefined function nope() in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn do_fcall_by_name<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    do_call(req, frame, code, op, "ZEND_DO_FCALL_BY_NAME")
}

/// `ZEND_DO_FCALL`: serve the callee an `INIT_STATIC_METHOD_CALL`, an
/// `INIT_METHOD_CALL` or a `NEW` resolved, which is a native method or
/// the pass function of a class with no constructor, so this is the
/// handler's `ZEND_INTERNAL_FUNCTION` arm, `DO_ICALL`'s body: the
/// frame stays on the stack while the entry runs, with its receiver
/// for `Request::this` to answer, is popped whether it returned or
/// threw, the receiver's owner given up with it
/// (`ZEND_CALL_RELEASE_THIS`), and what it returned is the temporary's
/// under `RETURN_VALUE_USED` and given up otherwise, which is every
/// constructor's `DO_FCALL`.
///
/// ```php
/// class M {}
/// $m = new M(1);
/// var_dump($m instanceof M);
/// ```
/// ```expect
/// bool(true)
/// ```
pub(crate) fn do_fcall<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    do_call(req, frame, code, op, "ZEND_DO_FCALL")
}

/// `ZEND_DO_UCALL` (`Z/Zend/zend_vm_def.h:4169`): the callee an
/// `INIT_FCALL` or an `INIT_STATIC_METHOD_CALL` resolved is a user
/// function the compiler knew, so this is the `ZEND_USER_FUNCTION` arm
/// alone: the call frame becomes the callee's frame ([`enter`]), and
/// the loop goes on in it (design §5.6).
///
/// ```php
/// function f($a, $b) { echo $a + $b, "|"; return $a * $b; }
/// class C { public $p = 2; static function s($x) { return $x . 's'; } function m() { return $this->p + 1; } }
/// echo f(2, 3), "|", C::s('a'), "|", (new C)->m(), "|", strlen(f(1, 1)), "\n";
/// f(1, 2);
/// var_dump(f(2, 2) === 4, f(1, 2, 3) === 2);
/// ```
/// ```expect
/// 5|6|as|3|2|1
/// 3|4|3|bool(true)
/// bool(true)
/// ```
///
/// What a call leaves behind is nothing: the frame's slots, its
/// arguments and its receiver are given up when it is popped, so the
/// heap stands where it stood.
///
/// ```php
/// function f($a) { $b = str_repeat($a, 100); return strlen($b); }
/// class C { function m($s) { return f($s) + 1; } }
/// $c = new C; $before = memory_get_usage();
/// $n = $c->m('x'); $n = f('yy');
/// var_dump($n, memory_get_usage() - $before);
/// ```
/// ```expect
/// int(200)
/// int(0)
/// ```
pub(crate) fn do_ucall<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    do_call(req, frame, code, op, "ZEND_DO_UCALL")
}

/// The `ZEND_USER_FUNCTION` arm of the `DO_*CALL` handlers
/// (`Z/Zend/zend_vm_def.h:4169`, `i_init_func_execute_data`, design
/// §5.6): the served call taken off the call stack and pushed as the
/// callee's frame, its arguments in the parameters' CVs and its
/// receiver carried over ([`Request::push_user_frame`]), the caller's
/// slot for the value recorded where `RETURN_VALUE_USED`; then the
/// `RECV*` prologue. php skips the whole prologue where the function has
/// no typed parameter and was passed at least as many arguments as it
/// declares (`Z/Zend/zend_execute.c:4426-4437`), and runs it from opline
/// zero otherwise; here it runs to the entry safepoint, which the
/// compiler numbered after the prologue, so a `RECV` that throws is
/// never inside a recording and the frame is reported [`Stepped::Threw`].
/// A prologue that raised nothing is [`Stepped::Pushed`], and the loop's
/// activation performs the interrupt check and raises `Site::Entry`.
fn enter(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    function: Function,
) -> Edge {
    let Some(call) = req.vm_mut().pop_call() else {
        return Err(Fatal::Error(String::from(
            "InternalError: a user call with no call frame under it",
        )));
    };
    let returns_to =
        (op.out_type != OperandKind::Unused).then(|| (frame, exec::temporary_slot(code, op.out)));
    let passed = call.arguments.len();
    let Some(callee) = req.push_user_frame(function, call, returns_to) else {
        return Err(Fatal::Error(String::from(
            "InternalError: a user function whose body the unit has not",
        )));
    };
    let Some(serving) = req.vm().serving(callee) else {
        return Err(Fatal::Error(String::from(
            "InternalError: the frame just pushed is serving no op array",
        )));
    };
    let Some(body) = serving.code() else {
        return Err(Fatal::Error(String::from(
            "InternalError: the frame just pushed names a body the unit has not",
        )));
    };
    let entry = body.safepoints.first().map_or(0, |at| at.0);
    // `i_init_func_execute_data` (`Z/Zend/zend_execute.c:4426-4437`)
    // skips the `RECV*` oplines of the arguments a call passed, and only
    // where the function declares no type: `ZEND_ACC_HAS_TYPE_HINTS`
    // keeps every one of them, since each is a type check. The design's
    // entry safepoint stands after the last of them either way (§5.6,
    // the accepted `trace:entry-counter-typed-recv` row).
    let (parameters, typed) = req.function_decl(function).map_or((0, false), |decl| {
        (
            u32::try_from(decl.parameters.iter().filter(|p| !p.variadic).count())
                .unwrap_or(u32::MAX),
            decl.parameters.iter().any(|p| p.ty.is_some()),
        )
    });
    let passed = u32::try_from(passed).unwrap_or(u32::MAX);
    if passed <= parameters && !typed {
        // `opline += num_args`: the `RECV*` of each argument the call
        // passed is skipped and the ones after it still run, so a
        // parameter the call left out still reaches its `RECV` and its
        // `ArgumentCountError`.
        req.vm_mut().jump(callee, passed.min(entry));
    }
    while req.vm().opline(callee) < entry {
        let Some(prologue) = exec::opline_at(body, req.vm().opline(callee)) else {
            break;
        };
        let stepped = match prologue.opcode {
            Opcode::Recv => recv::<crate::tier::Nop>(req, callee, body, prologue)?,
            Opcode::RecvInit => recv_init::<crate::tier::Nop>(req, callee, body, prologue)?,
            Opcode::RecvVariadic => recv_variadic::<crate::tier::Nop>(req, callee, body, prologue)?,
            other => {
                return Err(Fatal::Error(format!(
                    "InternalError: {} stands before the entry safepoint, and this engine serves no handler for it yet",
                    other.name()
                )));
            }
        };
        if let Stepped::Threw(at) = stepped {
            return Ok(Stepped::Threw(at));
        }
    }
    Ok(Stepped::Pushed(callee))
}

/// `ZEND_RECV` (`Z/Zend/zend_vm_def.h:5692`): the parameter numbered
/// `op1`, from one, whose argument the caller placed in the CV
/// `result` names; nothing to do where the call passed it, and
/// `zend_missing_arg_error` where it did not, php's
/// `ArgumentCountError` naming the function as `Class::method` or
/// alone, how many were passed, the caller's file and line, and how
/// many the function requires, `exactly` where every parameter is
/// required and `at least` otherwise. The throwable records the
/// callee's line, which is where the loop stands, and its trace has
/// the call as its first row.
///
/// ```php
/// function f($a, $b) {}
/// f(1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: Too few arguments to function f(), 1 passed in %s on line 3 and exactly 2 expected in %s:2
/// Stack trace:
/// #0 %s(3): f(1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// class C { function m($a, $b = 1) {} }
/// (new C)->m();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: Too few arguments to function C::m(), 0 passed in %s on line 3 and at least 1 expected in %s:2
/// Stack trace:
/// #0 %s(3): C->m()
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// A parameter with a declared type the argument does not satisfy and
/// does not coerce into is `zend_verify_arg_error`'s `TypeError`, whose
/// sentence names the call site and whose own site is the function's:
///
/// ```php
/// function f(int $a) { var_dump($a); }
/// f("nope");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: f(): Argument #1 ($a) must be of type int, string given, called in %s on line 3 and defined in %s:2
/// Stack trace:
/// #0 %s(3): f('nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// Outside `strict_types` a scalar argument is converted where php
/// converts it (`zend_verify_weak_scalar_type_hint`), and what the
/// conversion made is what the parameter holds and what a trace prints:
///
/// ```php
/// function f(int $a, string $b, float $c, bool $d) { var_dump($a, $b, $c, $d); }
/// f("5", 7, 2, 1);
/// f(true, 1.5, "2.5", "x");
/// ```
/// ```expect
/// int(5)
/// string(1) "7"
/// float(2)
/// bool(true)
/// int(1)
/// string(3) "1.5"
/// float(2.5)
/// bool(true)
/// ```
///
/// A class named as a type is `instanceof_function` over the argument's
/// class, and the type error names the class as the declaration wrote
/// it:
///
/// ```php
/// class C { public $p = 1; }
/// function f(C $c) { var_dump($c->p); }
/// f(new C);
/// f(1);
/// ```
/// ```expect exit=255
/// int(1)
///
/// Fatal error: Uncaught TypeError: f(): Argument #1 ($c) must be of type C, int given, called in %s on line 5 and defined in %s:3
/// Stack trace:
/// #0 %s(5): f(1)
/// #1 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn recv<T: Tier>(req: &mut Request, frame: FrameRef, _: &OpArray, op: &Opline) -> Edge {
    let passed = req.vm().at(frame).map_or(0, |state| state.num_args);
    if op.op1 > passed {
        let thrown = missing_argument(req, frame, passed);
        return threw(req, frame, &thrown);
    }
    // `opline->op2.num & (1u << Z_TYPE_P(param))`: the mask the compiler
    // put in `op2` is the whole check for an argument of a type the
    // declaration names outright, and `MAY_BE_ANY` where no type is
    // written, so a parameter with none never leaves this line.
    let argument = req.slot(frame, op.out).deref();
    if op.op2 & (1u32 << u32::from(argument.type_code())) != 0 {
        return Ok(advance(req, frame));
    }
    if let Err(thrown) = verify_arg_type(req, frame, op.op1, op.out) {
        return threw(req, frame, &thrown);
    }
    Ok(advance(req, frame))
}

/// `zend_verify_recv_arg_type` (`Z/Zend/zend_execute.c:1241`) over the
/// parameter numbered `number`, whose argument stands in the CV `slot`:
/// `zend_check_type`, which is the declared type's own mask, then an
/// object against a class the type names (`instanceof_function`), then
/// `zend_verify_scalar_type_hint` under the caller's `strict_types`
/// ([`crate::api::coerce::scalar_type_hint`]). A coercion replaces the
/// argument where it stands, as php converts the zval in the frame; what
/// does not convert is `zend_verify_arg_error`, the `TypeError` naming
/// the function, the parameter, the declared type, the value's kind and,
/// where the caller is user code, its file and line.
fn verify_arg_type(
    req: &mut Request,
    frame: FrameRef,
    number: u32,
    slot: u32,
) -> Result<(), Thrown> {
    let at = usize::try_from(number.saturating_sub(1)).unwrap_or(usize::MAX);
    let argument = req.slot(frame, slot).deref();
    if let Some(coerced) = verified_arg(req, frame, number, at, &argument)? {
        let previous = req.set_slot(frame, slot, coerced);
        req.release(previous);
    }
    Ok(())
}

/// The body of [`verify_arg_type`] over a value rather than a slot,
/// which is what a variadic's arguments are: the declared type is the
/// parameter numbered `at`, the variadic's own for every argument it
/// collects, while `number` is the argument's own position, which is
/// what php's error says. `Some` is the value the coercion outside
/// `strict_types` made of it, which the caller writes where the value
/// stood; `None` is a value the type contains as it is.
fn verified_arg(
    req: &mut Request,
    frame: FrameRef,
    number: u32,
    at: usize,
    argument: &Value,
) -> Result<Option<Value>, Thrown> {
    let function = req.vm().at(frame).and_then(|state| state.function);
    let Some(ty) = function
        .and_then(|function| req.function_decl(function))
        .and_then(|decl| decl.parameters.get(at))
        .and_then(|parameter| parameter.ty.clone())
    else {
        return Ok(None);
    };
    if ty.contains_code(argument.type_code()) {
        return Ok(None);
    }
    if let Some(object) = argument.as_object()
        && named_types(&ty).any(|name| {
            req.class_named(&name.0)
                .is_some_and(|of| req.instance_of(object.class(), of))
        })
    {
        return Ok(None);
    }
    let strict = req.caller_strict_types();
    if let Some(coerced) = crate::api::coerce::scalar_type_hint(req, argument, ty.mask(), strict)? {
        return Ok(Some(coerced));
    }
    Err(arg_type_error(req, frame, number, at, &ty, argument))
}

/// Every class name a declared type holds, in the order it was written:
/// the one name of a class type, and each member of a union or an
/// intersection that is one, which `zend_check_type_slow` walks with
/// `ZEND_TYPE_LIST_FOREACH`.
fn named_types(ty: &php_unit::decl::Type) -> Box<dyn Iterator<Item = &php_unit::decl::Name> + '_> {
    use php_unit::decl::Type;
    match ty {
        Type::Named(name) => Box::new(std::iter::once(name)),
        Type::Nullable(inner) => named_types(inner),
        Type::Union(members) | Type::Intersection(members) => {
            Box::new(members.iter().flat_map(named_types))
        }
        _ => Box::new(std::iter::empty()),
    }
}

/// `ZEND_RECV_VARIADIC` (`Z/Zend/zend_vm_def.h:5773`): the arguments
/// from the one numbered `op1` on, collected into a packed array in the
/// variadic parameter's CV. The arguments past the declared parameters
/// are the frame's extra ones ([`exec::FrameState::extra_args`]), where
/// php leaves them on its stack past the temporaries, and they stay
/// there: each is one more owner in the array (`Z_ADDREF_P`) and the
/// frame gives its own up when it is popped
/// (`ZEND_CALL_FREE_EXTRA_ARGS`), so `func_get_args()` and a backtrace
/// read what the call passed after this opline as before it. A declared
/// type on the variadic is checked over each of them
/// (`zend_verify_variadic_arg_type`), the coercion outside
/// `strict_types` replacing the argument where it stands as `RECV`'s
/// replaces one; the `TypeError`
/// names the argument's own number and no parameter name, since php has
/// a name for the declared ones alone. A call that passed nothing from
/// here on leaves the empty array.
///
/// ```php
/// function f(int ...$r) { var_dump($r); }
/// f(1, 2);
/// f();
/// f("5");
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// array(0) {
/// }
/// array(1) {
///   [0]=>
///   int(5)
/// }
/// ```
///
/// The declared parameters before it take theirs, and the collected
/// array is the CV's own: writing through it leaves the arguments the
/// frame still holds as they were.
///
/// ```php
/// function f($a, ...$r) { $r[] = 4; var_dump($a, $r); }
/// f(1, 2, 3);
/// ```
/// ```expect
/// int(1)
/// array(3) {
///   [0]=>
///   int(2)
///   [1]=>
///   int(3)
///   [2]=>
///   int(4)
/// }
/// ```
///
/// The type error is the variadic's, numbered by the argument and named
/// by nothing.
///
/// ```php
/// function f(int ...$r) { var_dump($r); }
/// f(1, "x");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: f(): Argument #2 must be of type int, string given, called in %s on line 3 and defined in %s:2
/// Stack trace:
/// #0 %s(3): f(1, 'x')
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn recv_variadic<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    _: &OpArray,
    op: &Opline,
) -> Edge {
    let first = op.op1;
    let at = usize::try_from(first.saturating_sub(1)).unwrap_or(usize::MAX);
    let count = req.vm().at(frame).map_or(0, |state| state.extra_args.len());
    let collected = match req.array_sized(u32::try_from(count).unwrap_or(u32::MAX), true) {
        Ok(collected) => collected,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let previous = req.set_slot(frame, op.out, collected.to_value());
    req.release(previous);
    for step in 0..count {
        let Some(argument) = req
            .vm()
            .at(frame)
            .and_then(|state| state.extra_args.get(step).copied())
        else {
            break;
        };
        let number = first.saturating_add(u32::try_from(step).unwrap_or(0));
        let argument = match verified_arg(req, frame, number, at, &argument) {
            Ok(None) => argument,
            Ok(Some(coerced)) => {
                if let Some(state) = req.vm_mut().at_mut(frame)
                    && let Some(place) = state.extra_args.get_mut(step)
                {
                    let previous = std::mem::replace(place, coerced);
                    req.release(previous);
                }
                coerced
            }
            Err(thrown) => return threw(req, frame, &thrown),
        };
        req.addref(&argument);
        if let Err(thrown) = req.array_append(collected.heap(), argument) {
            return threw(req, frame, &thrown);
        }
    }
    Ok(advance(req, frame))
}

/// `ZEND_VERIFY_RETURN_TYPE` (`Z/Zend/zend_vm_def.h:4431`), which the
/// compiler puts before every `RETURN` of a function with a declared
/// return type and once more, with an `UNUSED` operand, at the end of
/// its body: the value the `RETURN` after it will hand back, held to the
/// declared type as an argument is held to a parameter's
/// (`zend_check_type` and the coercion beside it, which php reads the
/// *callee's* file's `strict_types` for where this reads the request's
/// one setting, the file's being #411's), and `zend_verify_return_error`'s
/// `TypeError`,
/// `{fn}(): Return value must be of type {declared}, {given} returned`,
/// where it does not hold. The `UNUSED` operand is
/// `zend_verify_missing_return_type`, the same error over `none`, which
/// is what control falling off the end of a typed function is. A
/// coercion is written back where the value stood, so the `RETURN` after
/// this hands back what the type asked for.
///
/// ```php
/// function f(): int { $x = "5"; return $x; }
/// var_dump(f());
/// ```
/// ```expect
/// int(5)
/// ```
///
/// ```php
/// function f(): int { $x = "s"; return $x; }
/// f();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: f(): Return value must be of type int, string returned in %s:2
/// Stack trace:
/// #0 %s(3): f()
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// function f(): int { }
/// f();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: f(): Return value must be of type int, none returned in %s:2
/// Stack trace:
/// #0 %s(3): f()
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn verify_return_type<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(ty) = req
        .vm()
        .at(frame)
        .and_then(|state| state.function)
        .and_then(|function| req.function_decl(function))
        .and_then(|decl| decl.return_type.clone())
    else {
        return Ok(advance(req, frame));
    };
    if op.op1_type == OperandKind::Unused {
        let thrown = return_type_error(req, frame, &ty, None);
        return threw(req, frame, &thrown);
    }
    let returned = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    if ty.contains_code(returned.type_code()) {
        return Ok(advance(req, frame));
    }
    if let Some(object) = returned.as_object()
        && named_types(&ty).any(|name| {
            req.class_named(&name.0)
                .is_some_and(|of| req.instance_of(object.class(), of))
        })
    {
        return Ok(advance(req, frame));
    }
    let strict = req.caller_strict_types();
    let coerced = match crate::api::coerce::scalar_type_hint(req, &returned, ty.mask(), strict) {
        Ok(coerced) => coerced,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let Some(coerced) = coerced else {
        let thrown = return_type_error(req, frame, &ty, Some(&returned));
        return threw(req, frame, &thrown);
    };
    let slot = if op.op1_type == OperandKind::Cv {
        op.op1
    } else {
        exec::temporary_slot(code, op.op1)
    };
    let previous = req.set_slot(frame, slot, coerced);
    req.release(previous);
    Ok(advance(req, frame))
}

/// `zend_verify_return_error` (`Z/Zend/zend_execute.c:1421`): the
/// `TypeError` over a return, `{fn}(): Return value must be of type
/// {declared}, {given} returned`, with `none` for the end of a body that
/// returned nothing. Its site is the function's own, where the loop
/// stands, as an argument's is.
fn return_type_error(
    req: &mut Request,
    frame: FrameRef,
    ty: &php_unit::decl::Type,
    returned: Option<&Value>,
) -> Thrown {
    let function = req.vm().at(frame).and_then(|state| state.function);
    let name = function.map_or_else(String::new, |function| req.qualified_name_of(function));
    let declared = crate::api::coerce::type_name(ty);
    let given = match returned {
        Some(returned) => crate::ops::value_name(req, &returned.undef_as_null()),
        None => String::from("none"),
    };
    let message = format!("{name}(): Return value must be of type {declared}, {given} returned");
    crate::api::raising::throw(req, "TypeError", &message)
}

/// `ZEND_VERIFY_NEVER_TYPE` (`Z/Zend/zend_vm_def.h:4501`), the opline
/// that closes a `never`-returning function's body: control reaching it
/// is `zend_verify_never_error`'s `TypeError`, since a `never` function
/// may leave only by a throw or by ending the request.
///
/// ```php
/// function f(): never { echo "a\n"; }
/// f();
/// ```
/// ```expect exit=255
/// a
///
/// Fatal error: Uncaught TypeError: f(): never-returning function must not implicitly return in %s:2
/// Stack trace:
/// #0 %s(3): f()
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn verify_never_type<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    _: &OpArray,
    _: &Opline,
) -> Edge {
    let function = req.vm().at(frame).and_then(|state| state.function);
    let name = function.map_or_else(String::new, |function| req.qualified_name_of(function));
    let kind = if req.vm().at(frame).is_some_and(|state| state.this.is_some()) {
        "method"
    } else {
        "function"
    };
    let message = format!("{name}(): never-returning {kind} must not implicitly return");
    let thrown = crate::api::raising::throw(req, "TypeError", &message);
    threw(req, frame, &thrown)
}

/// `zend_verify_arg_error` (`Z/Zend/zend_execute.c:707`): the `TypeError`
/// a user function's argument of the wrong type raises. The sentence is
/// `zend_argument_type_error`'s, `{fn}(): Argument #N ($name) must be of
/// type {declared}, {given} given`, with `, called in {file} on line {n}`
/// where the frame below is user code, which php's uncaught rendering
/// then closes with ` and defined`
/// ([`Request::throwable_to_string`]). The throwable's own site is the
/// callee's, where the loop stands.
fn arg_type_error(
    req: &mut Request,
    frame: FrameRef,
    number: u32,
    at: usize,
    ty: &php_unit::decl::Type,
    argument: &Value,
) -> Thrown {
    let function = req.vm().at(frame).and_then(|state| state.function);
    let name = function.map_or_else(String::new, |function| req.qualified_name_of(function));
    // `get_function_arg_name`: php has a name for an argument up to the
    // declared count alone, which excludes the variadic, so the error
    // over an argument a variadic collected is `Argument #2` bare.
    let decl = function.and_then(|function| req.function_decl(function));
    let named = decl
        .as_ref()
        .is_some_and(|decl| at < decl.parameters.iter().filter(|p| !p.variadic).count());
    let parameter = decl
        .filter(|_| named)
        .and_then(|decl| decl.parameters.get(at).cloned())
        .map(|parameter| format!(" (${})", String::from_utf8_lossy(&parameter.name.0)))
        .unwrap_or_default();
    let declared = crate::api::coerce::type_name(ty);
    let given = crate::ops::value_name(req, &argument.undef_as_null());
    let caller = req.vm().frames().iter().rev().nth(1).map(|caller| {
        (
            exec::body_of(&caller.unit, caller.body)
                .map(|code| String::from_utf8_lossy(&code.filename.0).into_owned())
                .unwrap_or_default(),
            exec::line_of(caller),
        )
    });
    let message = match caller {
        Some((file, line)) => format!(
            "{name}(): Argument #{number}{parameter} must be of type {declared}, {given} given, called in {file} on line {line}"
        ),
        None => format!(
            "{name}(): Argument #{number}{parameter} must be of type {declared}, {given} given"
        ),
    };
    crate::api::raising::throw(req, "TypeError", &message)
}

/// `zend_missing_arg_error` (`Z/Zend/zend_execute.c`): the caller is
/// the frame below, and its opline is the call.
fn missing_argument(req: &mut Request, frame: FrameRef, passed: u32) -> Thrown {
    let function = req.vm().at(frame).and_then(|state| state.function);
    let name = function.map_or_else(String::new, |function| req.qualified_name_of(function));
    let parameters: Vec<bool> = function
        .and_then(|function| req.function_decl(function))
        .map(|decl| {
            decl.parameters
                .iter()
                .map(|parameter| parameter.default.is_none() && !parameter.variadic)
                .collect()
        })
        .unwrap_or_default();
    let required = parameters
        .iter()
        .rposition(|required| *required)
        .map_or(0, |at| at + 1);
    let all = parameters.len();
    let how = if required == all {
        "exactly"
    } else {
        "at least"
    };
    let caller = req.vm().frames().iter().rev().nth(1).map(|caller| {
        (
            exec::body_of(&caller.unit, caller.body)
                .map(|code| String::from_utf8_lossy(&code.filename.0).into_owned())
                .unwrap_or_default(),
            exec::line_of(caller),
        )
    });
    let message = match caller {
        Some((file, line)) => format!(
            "Too few arguments to function {name}(), {passed} passed in {file} on line {line} and {how} {required} expected"
        ),
        None => format!(
            "Too few arguments to function {name}(), {passed} passed and {how} {required} expected"
        ),
    };
    crate::api::raising::throw(req, "ArgumentCountError", &message)
}

/// `ZEND_RECV_INIT` (`Z/Zend/zend_vm_def.h:5723`): the parameter
/// numbered `op1` with a default in `op2`; where the call passed no
/// argument for it, the default is copied into the CV `result` names
/// (`ZVAL_COPY(param, default_value)`), and nothing is done otherwise.
///
/// ```php
/// function f($a, $b = 'b', $c = [1, 2]) { var_dump($a, $b, $c); }
/// f(1); f(1, 2); f(1, 2, 3);
/// ```
/// ```expect
/// int(1)
/// string(1) "b"
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// int(1)
/// int(2)
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// int(1)
/// int(2)
/// int(3)
/// ```
///
/// A default is never type-checked and an argument that was passed
/// always is, which is php's `recv_init_check_type` label:
///
/// ```php
/// function f(int $a = 1) { var_dump($a); }
/// f(); f("2");
/// f([]);
/// ```
/// ```expect exit=255
/// int(1)
/// int(2)
///
/// Fatal error: Uncaught TypeError: f(): Argument #1 ($a) must be of type int, array given, called in %s on line 4 and defined in %s:2
/// Stack trace:
/// #0 %s(4): f(Array)
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn recv_init<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let passed = req.vm().at(frame).map_or(0, |state| state.num_args);
    if op.op1 > passed {
        let default = exec::constant(code, ConstIdx(op.op2));
        req.addref(&default);
        let previous = req.set_slot(frame, op.out, default);
        req.release(previous);
        return Ok(advance(req, frame));
    }
    // php checks the type of an argument that was passed and never of
    // the default it copied, and only where the function has one to
    // check (`ZEND_ACC_HAS_TYPE_HINTS`, the `recv_init_check_type`
    // label).
    if let Err(thrown) = verify_arg_type(req, frame, op.op1, op.out) {
        return threw(req, frame, &thrown);
    }
    Ok(advance(req, frame))
}

/// `ZEND_NEW` (`Z/Zend/zend_vm_def.h:5941`) over a class named at
/// compile time: the class looked up by the lowered name of the
/// constant pair in `op1`, `zend_fetch_class_by_name` with
/// `ZEND_FETCH_CLASS_EXCEPTION`, so a name nothing declares is php's
/// `Error` over the name as written; `object_init_ex`, which refuses an
/// interface, a trait, an enum and an abstract class and otherwise lays
/// the object down with its declared properties' defaults, into the
/// `VAR` `result` names; then the constructor's call frame with the
/// object as its receiver, one more owner (`Z_ADDREF_P(result)`), and
/// `extended_value` arguments to take. A class with no constructor
/// and no arguments skips the `DO_FCALL` after this opline
/// (`ZEND_VM_NEXT_OPCODE_EX(1, 2)`), and one with arguments pushes the
/// pass function to take them.
///
/// ```php
/// class M { public $a = 1; }
/// $m = new M; $n = new M(2, 3);
/// var_dump($m instanceof M, $n instanceof M, $m === $n, $m === $m);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
///
/// ```php
/// $x = new Nope;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Class "Nope" not found in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new Throwable;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot instantiate interface Throwable in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// What the object takes is what php's takes: `zend_objects_new` is
/// the fifty-six-byte `zend_object` with one property slot in it and
/// sixteen bytes per declared property past the first, so a class of
/// two properties is seventy-two bytes in the eighty-byte bin, given
/// back when the variable holding the last owner gives it up.
///
/// ```php
/// class M { public $a = 1; public $b; }
/// $b = memory_get_usage(); $m = new M; $held = memory_get_usage() - $b;
/// $m = 1; var_dump($held, memory_get_usage() - $b);
/// ```
/// ```expect
/// int(80)
/// int(0)
/// ```
pub(crate) fn new<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let class = match class_operand(
        req,
        frame,
        code,
        op.op1_type,
        op.op1,
        Some(CacheSlot(op.op2)),
    ) {
        Ok(class) => class,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let object = match crate::api::call::new_object(req, class) {
        Ok(object) => object,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, object.as_value());
    req.release(previous);
    let count = usize::try_from(op.extended_value).unwrap_or(0);
    let constructor = req.method_named(class, b"__construct");
    if constructor.is_none() {
        let at = req.vm().opline(frame);
        let next_is_do_fcall = exec::opline_at(code, at.saturating_add(1))
            .is_some_and(|next| next.opcode == php_unit::Opcode::DoFcall);
        if count == 0 && next_is_do_fcall {
            req.vm_mut().jump(frame, at.saturating_add(2));
            return Ok(Stepped::Same(frame));
        }
    }
    let function = constructor.unwrap_or(Function::PASS);
    let name = match req.method_name_string(function) {
        Ok(name) => name,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let this = constructor.map(|_| {
        req.addref(&object.as_value());
        object
    });
    req.vm_mut().push_call(CallState {
        function,
        name: Some(name),
        arguments: Vec::new(),
        count,
        this,
        called_scope: constructor.map(|_| class),
        stage: CallStage::Building,
        frameless: false,
        coerced: Vec::new(),
    });
    Ok(advance(req, frame))
}

/// `ZEND_INIT_STATIC_METHOD_CALL` (`Z/Zend/zend_vm_def.h:3738`) over a
/// class and a method both named at compile time: the class by the
/// lowered name of the pair in `op1`, `Class "X" not found` where
/// nothing declares it; the method by the lowered name of the pair in
/// `op2` through `zend_std_get_static_method`, `Call to undefined
/// method X::m()` where the class has none, the class's declared name
/// and the method as written; `op2` unused is the constructor, `Cannot
/// call constructor` where the class has none. An instance method
/// needs a `$this` of the class in the calling frame, which no frame
/// here has, so it is `Non-static method X::m() cannot be called
/// statically`. The call frame carries the class as its called scope
/// and no receiver.
///
/// ```php
/// Nope::m();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Class "Nope" not found in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// Exception::NoPe(1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to undefined method Exception::NoPe() in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// class M {}
/// M::__construct();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot call constructor in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// A class that declares one is past that branch, and its constructor
/// is an instance method called on the class.
///
/// ```php
/// Exception::__construct();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Non-static method Exception::__construct() cannot be called statically in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// The method found is held to `zend_std_get_static_method`'s access
/// rule before anything else is asked of it: a private method from the
/// global scope is `zend_bad_method_call`, the name as written, ahead
/// of its being an instance method called on the class.
///
/// ```php
/// Exception::__CLONE();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to private method Exception::__CLONE() from global scope in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn init_static_method_call<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let class = match class_operand(req, frame, code, op.op1_type, op.op1, None) {
        Ok(class) => class,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let (function, name) = match op.op2_type {
        OperandKind::Unused => {
            let Some(constructor) = req.method_named(class, b"__construct") else {
                let thrown = crate::api::raising::throw(req, "Error", "Cannot call constructor");
                return threw(req, frame, &thrown);
            };
            (constructor, None)
        }
        OperandKind::Const => {
            let written = exec::constant(code, ConstIdx(op.op2));
            let lowered = exec::constant(code, ConstIdx(op.op2.saturating_add(1)));
            let Some(function) = lowered
                .as_str_bytes()
                .and_then(|lowered| req.method_named(class, lowered))
            else {
                let thrown = undefined_method(req, class, &written);
                return threw(req, frame, &thrown);
            };
            if let Err(thrown) =
                req.check_method_access(function, written.as_str_bytes().unwrap_or_default())
            {
                return threw(req, frame, &thrown);
            }
            (function, Some(written))
        }
        _ => {
            return Err(Fatal::Error(String::from(
                "InternalError: ZEND_INIT_STATIC_METHOD_CALL over a method named at run time",
            )));
        }
    };
    if req
        .method_decl(function)
        .is_some_and(|decl| decl.is_instance)
    {
        let thrown = non_static_method_call(req, function);
        return threw(req, frame, &thrown);
    }
    let name = match name {
        Some(written) => match crate::ops::to_string(req, &written) {
            Ok(name) => name,
            Err(thrown) => return threw(req, frame, &thrown),
        },
        None => match req.method_name_string(function) {
            Ok(name) => name,
            Err(thrown) => return threw(req, frame, &thrown),
        },
    };
    req.vm_mut().push_call(CallState {
        function,
        name: Some(name),
        arguments: Vec::new(),
        count: usize::try_from(op.extended_value).unwrap_or(0),
        this: None,
        called_scope: Some(class),
        stage: CallStage::Building,
        frameless: false,
        coerced: Vec::new(),
    });
    Ok(advance(req, frame))
}

/// `ZEND_INIT_METHOD_CALL` (`Z/Zend/zend_vm_def.h:3583`) over a
/// receiver and a method named at compile time: the receiver read as
/// `GET_OP1_OBJ_ZVAL_PTR_UNDEF` reads it, a reference followed, and
/// what is no object php's `Error` naming the method as written and
/// the value as `zend_zval_value_name` names it, an undefined variable
/// having warned first; the method by the lowered name of the pair in
/// `op2` through the class's `get_method`, `Call to undefined method
/// X::m()` where the class has none. The call frame takes the receiver
/// as one more owner (`GC_ADDREF(obj)` for a CV, the temporary's own
/// owner moved for a `TMP` or a `VAR`) and the receiver's class as its
/// called scope; a method declared `static` is called on the class,
/// with no receiver.
///
/// ```php
/// $o = 1; $o->m();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to a member function m() on int in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// $u->m(1);
/// ```
/// ```expect exit=255
/// Warning: Undefined variable $u in %s on line 2
///
/// Fatal error: Uncaught Error: Call to a member function m() on null in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// class M {}
/// (new M)->NoPe();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to undefined method M::NoPe() in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// `zend_std_get_method`'s access rule ([`Request::check_method_access`]):
/// a method that is not public is called from its own class's scope,
/// and from anywhere else, the global scope here, it is
/// `zend_bad_method_call`, naming the visibility, the declaring class
/// and the method as written. The receiver is freed before the throw.
///
/// ```php
/// (new Error)->__clone();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to private method Error::__clone() from global scope in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn init_method_call<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let written = match op.op2_type {
        OperandKind::Const => exec::constant(code, ConstIdx(op.op2)),
        _ => {
            return Err(Fatal::Error(String::from(
                "InternalError: ZEND_INIT_METHOD_CALL over a method named at run time",
            )));
        }
    };
    let lowered = exec::constant(code, ConstIdx(op.op2.saturating_add(1)));
    let receiver = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    let Some(object) = receiver.as_object().copied() else {
        let thrown = crate::api::raising::throw(
            req,
            "Error",
            &format!(
                "Call to a member function {}() on {}",
                String::from_utf8_lossy(written.as_str_bytes().unwrap_or_default()),
                crate::ops::value_name(req, &receiver)
            ),
        );
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        return threw(req, frame, &thrown);
    };
    let class = object.class();
    let table = req.object_handlers(object.heap());
    let Some(function) = lowered
        .as_str_bytes()
        .and_then(|lowered| table.get_method(req, object, lowered))
    else {
        let thrown = undefined_method(req, class, &written);
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        return threw(req, frame, &thrown);
    };
    if let Err(thrown) =
        req.check_method_access(function, written.as_str_bytes().unwrap_or_default())
    {
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        return threw(req, frame, &thrown);
    }
    let name = match crate::ops::to_string(req, &written) {
        Ok(name) => name,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    // The frame's owner of the receiver: the temporary's own, moved,
    // or one more of the CV's or the constant's; a reference the slot
    // held is followed and given up.
    let taken = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    if taken.as_object().is_none() {
        req.addref(&object.as_value());
        req.release(taken);
    }
    let this = if req
        .method_decl(function)
        .is_some_and(|decl| decl.is_instance)
    {
        Some(object)
    } else {
        req.release(object.as_value());
        None
    };
    req.vm_mut().push_call(CallState {
        function,
        name: Some(name),
        arguments: Vec::new(),
        count: usize::try_from(op.extended_value).unwrap_or(0),
        this,
        called_scope: Some(class),
        stage: CallStage::Building,
        frameless: false,
        coerced: Vec::new(),
    });
    Ok(advance(req, frame))
}

/// `ZEND_FETCH_THIS` (`Z/Zend/zend_vm_def.h:9214`): `$this` read as a
/// value, or as a receiver where the compiler could not know a frame
/// has one. On a frame a method call pushed, `EX(This)` is the receiver
/// and the temporary takes one more owner of it (`ZVAL_OBJ_COPY`); on
/// the main op array, in a function or in a method declared `static` the frame
/// holds no receiver and this is php's `Error`
/// (`zend_this_not_in_object_context_helper`), the temporary it would
/// write left undefined (`UNDEF_RESULT`).
///
/// ```php
/// class C { public $p = 1; function m() { var_dump($this === $this, $this instanceof C); return $this; } static function s() { return $this; } }
/// $c = new C; var_dump($c->m() === $c);
/// C::s();
/// ```
/// ```expect exit=255
/// bool(true)
/// bool(true)
/// bool(true)
///
/// Fatal error: Uncaught Error: Using $this when not in object context in %s:2
/// Stack trace:
/// #0 %s(4): C::s()
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// echo $this;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Using $this when not in object context in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// $this->m(1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Using $this when not in object context in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn fetch_this<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(this) = req.vm().this(frame) else {
        let thrown =
            crate::api::raising::throw(req, "Error", "Using $this when not in object context");
        return threw(req, frame, &thrown);
    };
    let value = this.as_value();
    req.addref(&value);
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, value);
    req.release(previous);
    Ok(advance(req, frame))
}

/// `ZEND_FETCH_CLASS_CONSTANT` (`Z/Zend/zend_vm_def.h`, handler 181)
/// over a class and a constant both named at compile time, which is
/// what `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION` leaves of `C::A`: the
/// class by the lowered name of the pair in `op1`, `Class "X" not
/// found` where nothing declares it; the constant by name in the
/// class's constants, `Undefined constant C::A` where it has none; a
/// constant the scope may not reach is `Cannot access private constant
/// C::A` (`zend_verify_const_access`); and the value, the constant
/// expression the declaration kept evaluated on this use
/// (`zval_update_constant_ex`), into the temporary `result` names.
/// The two cache slots php primes with the class and the constant are
/// not read here; the lookup is made on every pass, which nothing
/// observes.
///
/// ```php
/// class A { const X = 1 + 2; const S = 'a' . 'b'; protected const P = 3; private const Q = 4; static function q() { return A::Q + A::P; } }
/// echo A::X, A::S, A::q(), "\n";
/// echo A::P;
/// ```
/// ```expect exit=255
/// 3ab7
///
/// Fatal error: Uncaught Error: Cannot access protected constant A::P in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
///
/// ```php
/// class A {}
/// echo A::NOPE;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Undefined constant A::NOPE in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn fetch_class_constant<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let class = match class_operand(
        req,
        frame,
        code,
        op.op1_type,
        op.op1,
        Some(CacheSlot(op.extended_value)),
    ) {
        Ok(class) => class,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    if op.op2_type != OperandKind::Const {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_FETCH_CLASS_CONSTANT over a constant named at run time",
        )));
    }
    let name = exec::constant(code, ConstIdx(op.op2));
    let name = name.as_str_bytes().unwrap_or_default().to_vec();
    let value = match req.class_constant(class, &name) {
        Ok(value) => value,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, value);
    req.release(previous);
    Ok(advance(req, frame))
}

/// `ZEND_FETCH_OBJ_R` (`Z/Zend/zend_vm_def.h:2069`) over a property
/// named at compile time: the container read as
/// `GET_OP1_OBJ_ZVAL_PTR_UNDEF` reads it, a reference followed, an
/// undefined variable having warned; what is no object warns `Attempt
/// to read property "p" on int` (`zend_wrong_property_read`, the value
/// as `zend_zval_value_name` names it) and writes `null`. An
/// object's property is read through the class's `read_property`
/// ([`Request::read_property`]): the declared slot where the scope may
/// see it, php's `Error` where it may not, the dynamic table, the
/// `Error` for a typed property nothing has initialised, and the
/// warning and `null` for a name the object has not. What is written
/// is a temporary with one more owner of the value; the container is freed
/// (`FREE_OP1`) before the pending throwable is read.
///
/// ```php
/// class C { public $p = 1; protected $q = 2; private $r = 3; public $u; }
/// $c = new C; $n = 1;
/// var_dump($c->p, $c->u, $c->nope, $n->p, $z->p, $c->p->q);
/// ```
/// ```expect
/// Warning: Undefined property: C::$nope in %s on line 4
///
/// Warning: Attempt to read property "p" on int in %s on line 4
///
/// Warning: Undefined variable $z in %s on line 4
///
/// Warning: Attempt to read property "p" on null in %s on line 4
///
/// Warning: Attempt to read property "q" on int in %s on line 4
/// int(1)
/// NULL
/// NULL
/// NULL
/// NULL
/// NULL
/// ```
///
/// ```php
/// class C { public $p = 1; protected $q = 2; }
/// $c = new C; echo $c->q;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot access protected property C::$q in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// class C { private $r = 3; }
/// echo (new C)->r;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot access private property C::$r in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// A typed property nothing has written is php's `Error`, `Typed
/// property C::$t must not be accessed before initialization`; the
/// witness of that waits on the compiler reading a typed property
/// (`public int $t;`), which it does not yet, and every typed property
/// a native declares is written by its constructor.
///
/// A native class's declared properties are read the same way: the
/// four of `PhpToken`, typed and written by its constructor.
///
/// ```php
/// $k = new PhpToken(328, 'echo', 1, 6); $l = new PhpToken(1, 'x');
/// var_dump($k->id, $k->text, $k->line, $k->pos, $l->line, $l->pos);
/// ```
/// ```expect
/// int(328)
/// string(4) "echo"
/// int(1)
/// int(6)
/// int(-1)
/// int(-1)
/// ```
pub(crate) fn fetch_obj_r<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let name = match op.op2_type {
        OperandKind::Const => exec::constant(code, ConstIdx(op.op2)),
        _ => {
            return Err(Fatal::Error(String::from(
                "InternalError: ZEND_FETCH_OBJ_R over a property named at run time",
            )));
        }
    };
    let name = name.as_str_bytes().unwrap_or_default().to_vec();
    let container = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    let read = if let Some(object) = container.as_object().copied() {
        let table = req.object_handlers(object.heap());
        table.read_property(req, object, &name)
    } else {
        crate::api::raising::warning(
            req,
            &format!(
                "Attempt to read property \"{}\" on {}",
                String::from_utf8_lossy(&name),
                crate::ops::value_name(req, &container)
            ),
        );
        Ok(Value::null())
    };
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    let value = match read {
        Ok(value) => value,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, value);
    req.release(previous);
    Ok(advance(req, frame))
}

/// `ZEND_FETCH_OBJ_W` (`Z/Zend/zend_vm_def.h:2238`),
/// `zend_fetch_property_address` under `BP_VAR_W`: the link of a
/// property chain before the one written, `$o->q` of `$o->q->p = 1`,
/// whose `VAR` is left holding an `IS_INDIRECT` to the property's slot
/// (`ZVAL_INDIRECT(result, ptr)`) for the `ASSIGN_OBJ` after it to write
/// through (design §5.6). The container is read in place and warns of
/// nothing ([`exec::container`]), a reference it holds followed; what is
/// no object is php's `Error` `Attempt to modify property "q" on null`
/// (`zend_throw_non_object_error`, this opcode's arm), the operand freed
/// first (`FREE_OP1_VAR_PTR`). On an object the name is placed by
/// `zend_std_get_property_ptr_ptr`: a declared slot the scope may reach
/// is the slot in place, written or not, since under `BP_VAR_W` an
/// `UNDEF` slot is handed back as it stands for the write after it to
/// find; one the scope may not reach is `zend_bad_property_access`'s
/// `Error`. A name the class did not declare is a bucket of the
/// object's dynamic table ([`Request::dynamic_property_ref`]): the one
/// it already holds, named in place through
/// [`php_heap::Arr::value_ref`], or one made on the spot holding
/// `EG(uninitialized_zval)`, which php refuses for a class with
/// `ZEND_ACC_NO_DYNAMIC_PROPERTIES` and deprecates for a class without
/// `ZEND_ACC_ALLOW_DYNAMIC_PROPERTIES`.
///
/// ```php
/// class C { public $p = 1; public $q; }
/// $o = new C; $o->q = new C;
/// $o->q->p = 5; $r = $o->p = 7;
/// var_dump($o->q->p, $o->p, $r);
/// ```
/// ```expect
/// int(5)
/// int(7)
/// int(7)
/// ```
///
/// ```php
/// $u->q->p = 1;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Attempt to modify property "q" on null in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// class C { private $q; }
/// $o = new C;
/// $o->q->p = 1;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot access private property C::$q in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
///
/// A dynamic property is named in place like a declared slot, so the
/// `ASSIGN_OBJ` after this writes through the bucket and the write
/// lands on the object the property holds. Each of the two names the
/// class did not declare is deprecated once, where it is created.
///
/// ```php
/// class C {}
/// $o = new C; $o->dyn = new C;
/// $o->dyn->p = 1; var_dump($o);
/// ```
/// ```expect
/// Deprecated: Creation of dynamic property C::$dyn is deprecated in %s on line 3
///
/// Deprecated: Creation of dynamic property C::$p is deprecated in %s on line 4
/// object(C)#1 (1) {
///   ["dyn"]=>
///   object(C)#2 (1) {
///     ["p"]=>
///     int(1)
///   }
/// }
/// ```
pub(crate) fn fetch_obj_w<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let name = match op.op2_type {
        OperandKind::Const => exec::constant(code, ConstIdx(op.op2)),
        _ => {
            return Err(Fatal::Error(String::from(
                "InternalError: ZEND_FETCH_OBJ_W over a property named at run time",
            )));
        }
    };
    let name_value = name;
    let name = name.as_str_bytes().unwrap_or_default().to_vec();
    let container =
        exec::container(req, frame, code, op.op1_type, op.op1, "ZEND_FETCH_OBJ_W")?.deref();
    let Some(object) = container.as_object().copied() else {
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        let shown = crate::ops::value_name(req, &container.undef_as_null());
        let thrown = crate::api::raising::throw(
            req,
            "Error",
            &format!(
                "Attempt to modify property \"{}\" on {shown}",
                String::from_utf8_lossy(&name)
            ),
        );
        return threw(req, frame, &thrown);
    };
    let placed = req.declared_slot_for_write(object.class(), &name);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    let at = match placed {
        Ok(Some((slot, _))) => {
            let Some(at) = object.heap().slot_ref(slot) else {
                return Err(Fatal::Error(format!(
                    "InternalError: ZEND_FETCH_OBJ_W placed ${} in slot {slot}, which the object does not have",
                    String::from_utf8_lossy(&name)
                )));
            };
            at
        }
        Ok(None) => {
            let Some(key) = name_value.as_php_string().copied() else {
                return Err(Fatal::Error(String::from(
                    "InternalError: ZEND_FETCH_OBJ_W over a property whose name is no string",
                )));
            };
            req.addref(&name_value);
            match req.dynamic_property_ref(object, key) {
                Ok(Some(at)) => at,
                Ok(None) => {
                    return Err(Fatal::Error(format!(
                        "InternalError: ZEND_FETCH_OBJ_W laid the dynamic property ${} down and the table does not hold it",
                        String::from_utf8_lossy(&name)
                    )));
                }
                Err(thrown) => return threw(req, frame, &thrown),
            }
        }
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let out = exec::temporary_slot(code, op.out);
    let previous = req.take_slot(frame, out);
    req.release(previous);
    let Some(var) = req.slot_ptr(frame, out) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_FETCH_OBJ_W writes a VAR the frame does not have",
        )));
    };
    var.write(php_heap::Zval::indirect(at));
    Ok(advance(req, frame))
}

/// The place a dimension write lands in, php's
/// `GET_OP1_ZVAL_PTR_PTR_UNDEF(BP_VAR_W)` (`_get_zval_ptr_ptr_var`,
/// `Z/Zend/zend_execute.c:167`): a CV's own slot, or a `VAR` read
/// through the `IS_INDIRECT` a `FETCH_DIM_W` or a `FETCH_OBJ_W` left
/// there and as it stands otherwise. A `CONST` or a `TMP` is no
/// container php compiles a write over, and is `None`.
fn write_place(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    kind: OperandKind,
    field: u32,
) -> Option<php_heap::ZvalRef> {
    match kind {
        OperandKind::Cv => req.slot_ptr(frame, field),
        OperandKind::Var => {
            let at = req.slot_ptr(frame, exec::temporary_slot(code, field))?;
            Some(at.read().as_indirect().unwrap_or(at))
        }
        OperandKind::Const | OperandKind::Tmp | OperandKind::Unused => None,
    }
}

/// `zend_fetch_dimension_address`'s walk to the array a dimension write
/// lands in (`Z/Zend/zend_execute.c:2903`), which `ASSIGN_DIM` walks
/// too: an array is `SEPARATE_ARRAY`, one owner of it from here and the
/// copy written back into the place where the array was shared; a
/// reference is followed once, since no reference holds a reference;
/// and `null`, an unwritten variable and `false` become a new array in
/// place (`array_init`), `false` with php's 8.1 deprecation before it
/// (`zend_false_to_array_deprecated`). What is left is php's `Error`
/// `Cannot use a scalar value as an array` (`zend_use_scalar_as_array`)
/// for a number, `true` and a resource; a string offset and an object's
/// `offsetSet` are the internal-error fatals naming the gap, since
/// `zend_assign_to_string_offset` and `zend_assign_to_object_dim` are
/// not served yet.
fn write_array(
    req: &mut Request,
    place: php_heap::ZvalRef,
    opcode: &str,
) -> Result<Result<PhpArray, Thrown>, Fatal> {
    let mut at = place;
    let mut held = exec::value_of(at.read());
    if let Some(cell) = held.as_php_ref() {
        at = cell.heap().val_ref();
        held = exec::value_of(at.read());
    }
    if let Some(array) = held.as_array() {
        let separated = match req.array_separate(array.heap()) {
            Ok(separated) => separated,
            Err(thrown) => return Ok(Err(thrown)),
        };
        if !separated.is_same(array.heap()) {
            at.write(php_heap::Zval::array(separated));
        }
        return Ok(Ok(PhpArray::of(separated)));
    }
    if matches!(
        held.as_ref(),
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false)
    ) {
        if held.as_bool() == Some(false) {
            crate::api::raising::deprecated(
                req,
                "Automatic conversion of false to array is deprecated",
            );
            if req.is_pending() {
                return Ok(Err(Thrown::of(None)));
            }
        }
        let array = match req.array() {
            Ok(array) => array,
            Err(thrown) => return Ok(Err(thrown)),
        };
        at.write(php_heap::Zval::array(array.heap()));
        return Ok(Ok(array));
    }
    if held.as_php_string().is_some() {
        return Err(Fatal::Error(format!(
            "InternalError: {opcode} over a string offset, which zend_assign_to_string_offset is not serving yet"
        )));
    }
    if held.as_object().is_some() {
        return Err(Fatal::Error(format!(
            "InternalError: {opcode} over an object, whose offsetSet is not served yet"
        )));
    }
    Ok(Err(crate::api::raising::throw(
        req,
        "Error",
        "Cannot use a scalar value as an array",
    )))
}

/// The entry a dimension write lands in, php's
/// `zend_fetch_dimension_address_inner` under `BP_VAR_W`
/// (`Z/Zend/zend_execute.c:2802`): the entry at the key in place, laid
/// down holding `null` where the table has none (`zend_hash_lookup`), or
/// one appended for the `UNUSED` dimension of `$a[]`
/// (`zend_hash_next_index_insert`). The key is the ladder
/// [`crate::ops::array_offset`] holds for every array write, a `CONST`
/// string taken as it is since the compiler applied
/// `zend_handle_numeric_dim` to it. `None` is the next index taken at
/// `ZEND_LONG_MAX`, `zend_cannot_add_element`'s `Error` at the site.
fn write_entry(
    req: &mut Request,
    array: PhpArray,
    dim: Option<&Value>,
    constant: bool,
) -> Result<Result<Option<php_heap::ZvalRef>, Thrown>, Fatal> {
    let Some(dim) = dim else {
        return Ok(req.array_next_entry(array.heap()));
    };
    let key = match crate::ops::array_offset(req, dim, constant) {
        Ok(key) => key,
        Err(thrown) => return Ok(Err(thrown)),
    };
    Ok(req.array_lookup(array.heap(), key))
}

/// `ZEND_FETCH_DIM_W` (`Z/Zend/zend_vm_def.h:1968`), the link under a
/// dimension write the compiler delays: the container walked to the
/// array it names ([`write_array`]), the entry at the dimension laid
/// down ([`write_entry`]), and an `IS_INDIRECT` naming it into the
/// opline's `VAR`, which is what the `ASSIGN_DIM`, the `ASSIGN_REF` or
/// the next `FETCH_DIM_W` over it writes through. The dimension is read
/// as `BP_VAR_R` reads it, so an undefined variable warns where php's
/// `slow_index_convert_w` warns; then `FREE_OP2` and, for a `VAR`
/// container, `FREE_OP1`, which frees nothing while the `IS_INDIRECT`
/// stands there. A throw leaves the `VAR` `UNDEF`, php's
/// `ZVAL_UNDEF(result)`.
///
/// A chain of dimensions builds the arrays under it as it goes, each
/// link's entry `null` until the link over it writes through it.
///
/// ```php
/// $a[1][2] = 3;
/// $a[1][] = 4;
/// var_dump($a);
/// ```
/// ```expect
/// array(1) {
///   [1]=>
///   array(2) {
///     [2]=>
///     int(3)
///     [3]=>
///     int(4)
///   }
/// }
/// ```
pub(crate) fn fetch_dim_w<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let out = exec::temporary_slot(code, op.out);
    let Some(place) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_FETCH_DIM_W over a constant or a temporary, which php compiles no write over",
        )));
    };
    let entry = match write_array(req, place, "ZEND_FETCH_DIM_W")? {
        Ok(array) => {
            let dim = (op.op2_type != OperandKind::Unused)
                .then(|| exec::operand(req, frame, code, op.op2_type, op.op2));
            let written = write_entry(req, array, dim.as_ref(), op.op2_type == OperandKind::Const)?;
            match written {
                Ok(Some(entry)) => Ok(entry),
                Ok(None) => Err(crate::api::raising::throw(
                    req,
                    "Error",
                    "Cannot add element to the array as the next element is already occupied",
                )),
                Err(thrown) => Err(thrown),
            }
        }
        Err(thrown) => Err(thrown),
    };
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    let previous = req.take_slot(frame, out);
    req.release(previous);
    let entry = match entry {
        Ok(entry) => entry,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let Some(var) = req.slot_ptr(frame, out) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_FETCH_DIM_W writes a VAR the frame does not have",
        )));
    };
    var.write(php_heap::Zval::indirect(entry));
    Ok(advance(req, frame))
}

/// `ZEND_CLONE` (`Z/Zend/zend_vm_def.h:6010`), what the compiler emits
/// for `clone $o` and a one-argument `clone($o)`: the operand read as
/// `GET_OP1_OBJ_ZVAL_PTR_UNDEF` reads it, a reference followed, an
/// undefined variable having warned; what is no object is php's
/// `TypeError` naming `clone()`'s first parameter and the value as
/// `zend_zval_value_name` names it. An object is
/// [`Request::clone_object`], the body the `clone()` function shares:
/// a null `clone_obj` in the table of the class is the `Error` for an
/// uncloneable object, read before `__clone` is looked at; a `__clone`
/// the scope may not reach is `zend_bad_method_call`'s; then the
/// class's `clone_obj` (design §5.4). The copy is written to the
/// temporary as its one owner, and the operand is freed (`FREE_OP1`)
/// before the pending throwable is read.
///
/// ```php
/// class C { public $p = 1; }
/// $c = new C; $d = clone $c;
/// var_dump($d->p, $c === $d, $d instanceof C, clone($c) instanceof C);
/// $f = Closure::fromCallable('strlen'); var_dump((clone $f)->__invoke('abc'));
/// ```
/// ```expect
/// int(1)
/// bool(false)
/// bool(true)
/// bool(true)
/// int(3)
/// ```
///
/// ```php
/// clone 1;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: clone(): Argument #1 ($object) must be of type object, int given in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// clone $u;
/// ```
/// ```expect exit=255
/// Warning: Undefined variable $u in %s on line 2
///
/// Fatal error: Uncaught TypeError: clone(): Argument #1 ($object) must be of type object, null given in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// A throwable's table carries `default_exception_handlers`, whose
/// `clone_obj` is null, and that is read before the private `__clone`
/// `Exception` declares.
///
/// ```php
/// clone new Exception;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Trying to clone an uncloneable object of class Exception in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn clone<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if op.op1_type == OperandKind::Unused {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_CLONE over $this, which no frame here carries",
        )));
    }
    let operand = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    let cloned = match operand.as_object().copied() {
        Some(object) => req.clone_object(object, None),
        None => Err(crate::api::raising::throw(
            req,
            "TypeError",
            &format!(
                "clone(): Argument #1 ($object) must be of type object, {} given",
                crate::ops::value_name(req, &operand)
            ),
        )),
    };
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    let object = match cloned {
        Ok(object) => object,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, object.as_value());
    req.release(previous);
    Ok(advance(req, frame))
}

/// `ZEND_INSTANCEOF` (`Z/Zend/zend_vm_def.h:8041`) over a class named
/// at compile time: `true` where the operand is an object of the class
/// the lowered name of the pair in `op2` names, or of a class that
/// extends or implements it (`instanceof_function`), the class looked
/// up with `ZEND_FETCH_CLASS_NO_AUTOLOAD` and silently, so a name
/// nothing declares is `false`; `false` for anything that is no
/// object, an undefined variable having warned; a reference is
/// followed. The operand is freed (`FREE_OP1`).
///
/// ```php
/// class M {} $m = new M; $e = new Exception; $a = 1;
/// var_dump($m instanceof M, $m instanceof m, $m instanceof Exception, $m instanceof Nope, $a instanceof M, $u instanceof M);
/// var_dump($e instanceof Exception, $e instanceof Throwable, $e instanceof Stringable, $e instanceof Error, (new TypeError) instanceof Error);
/// ```
/// ```expect
/// Warning: Undefined variable $u in %s on line 3
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
pub(crate) fn instanceof<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    let answer = match (value.as_object(), op.op2_type) {
        (Some(object), OperandKind::Const) => {
            let slot = Some(CacheSlot(op.extended_value));
            cached_class(req, frame, code, op.op2, slot)
                .is_some_and(|class| req.instance_of(object.class(), class))
        }
        (Some(_), _) => {
            return Err(Fatal::Error(String::from(
                "InternalError: ZEND_INSTANCEOF over a class named at run time",
            )));
        }
        (None, _) => false,
    };
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, Value::bool(answer));
    req.release(previous);
    Ok(advance(req, frame))
}

/// `CACHED_PTR(slot)`, else `zend_lookup_class_ex` under
/// `ZEND_FETCH_CLASS_NO_AUTOLOAD` over the lowered half of the constant
/// pair at `at` and `CACHE_PTR(slot, ce)` over what it found: the
/// silent resolution `ZEND_CATCH` and `ZEND_INSTANCEOF` make, and the
/// lookup half of the throwing one, [`class_operand`]. A name nothing
/// declares primes nothing — php's `CATCH` writes the null it found and
/// reads it back as unprimed, and the other three throw before
/// `CACHE_PTR` — so a class declared after a pass is found by the next.
///
/// No slot is [`init_static_method_call`]'s: php primes that one with
/// the class alone only where `op2` is no constant, and with the class
/// and the callee together where it is (`CACHE_POLYMORPHIC_PTR`), a
/// pair this cache has no variant for (#433).
fn cached_class(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    at: u32,
    slot: Option<CacheSlot>,
) -> Option<crate::api::values::ClassId> {
    if let Some(Cached::Class(class)) = slot.map(|slot| req.vm().cached(frame, slot)) {
        return Some(class);
    }
    let lowered = exec::constant(code, ConstIdx(at.saturating_add(1)));
    let found = lowered
        .as_str_bytes()
        .and_then(|lowered| exec::find_class(req, lowered));
    if let (Some(slot), Some(class)) = (slot, found) {
        req.vm_mut().prime(frame, slot, Cached::Class(class));
    }
    found
}

/// `zend_fetch_class_by_name` with `ZEND_FETCH_CLASS_EXCEPTION` over a
/// class's constant pair: the class the opline's cache slot holds or
/// [`cached_class`] looks up under the lowered name in the slot after
/// `at`, and php's `Error` naming the one as written where nothing
/// declares it. A class named as the program serves is not served yet.
fn class_operand(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    kind: OperandKind,
    at: u32,
    slot: Option<CacheSlot>,
) -> Result<crate::api::values::ClassId, Thrown> {
    if kind != OperandKind::Const {
        return Err(crate::api::raising::fatal(
            req,
            "InternalError: a class named at run time is not served",
        ));
    }
    if let Some(class) = cached_class(req, frame, code, at, slot) {
        return Ok(class);
    }
    let written = exec::constant(code, ConstIdx(at));
    Err(crate::api::raising::throw(
        req,
        "Error",
        &format!(
            "Class \"{}\" not found",
            String::from_utf8_lossy(written.as_str_bytes().unwrap_or_default())
        ),
    ))
}

/// `zend_undefined_method`: php's `Error` naming the class as declared
/// and the method as written.
fn undefined_method(
    req: &mut Request,
    class: crate::api::values::ClassId,
    written: &Value,
) -> Thrown {
    let class = req.class_name(class);
    crate::api::raising::throw(
        req,
        "Error",
        &format!(
            "Call to undefined method {class}::{}()",
            String::from_utf8_lossy(written.as_str_bytes().unwrap_or_default())
        ),
    )
}

/// `zend_non_static_method_call`: php's `Error` naming the method's
/// class and name as declared.
fn non_static_method_call(req: &mut Request, function: Function) -> Thrown {
    let name = req.qualified_name_of(function);
    crate::api::raising::throw(
        req,
        "Error",
        &format!("Non-static method {name}() cannot be called statically"),
    )
}

/// The `Threw` edge: the throwable left pending where the loop reads it.
fn threw(req: &mut Request, frame: FrameRef, thrown: &Thrown) -> Edge {
    req.leave_pending(thrown);
    Ok(Stepped::Threw(frame))
}

/// The body the `DO_*CALL` handlers share, `opcode` naming the one that
/// found no call frame under it: the `ZEND_USER_FUNCTION` arm enters
/// the callee's op array ([`enter`]), the `ZEND_INTERNAL_FUNCTION` arm
/// serves the native on the call frame.
fn do_call(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline, opcode: &str) -> Edge {
    let Some(function) = req.vm_mut().activate_call() else {
        return Err(Fatal::Error(format!(
            "InternalError: {opcode} with no call frame under it"
        )));
    };
    if req.user_function(function).is_some() {
        return enter(req, frame, code, op, function);
    }
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

/// `ZEND_INIT_NS_FCALL_BY_NAME` (`Z/Zend/zend_vm_def.h:4024`): the call
/// frame for an unqualified name written in a namespace, which php's
/// compiler resolves as the program serves (`zend_compile_ns_call`).
/// `op2` names the three constants `zend_add_ns_func_name_literal` laid
/// down: the name as written with the namespace before it, its lowered
/// form, and the lowered name alone; the callee is looked up under the
/// second and then the third (the handler's two
/// `zend_hash_find_known_hash`), and the `Error` for a name nothing
/// declares says the first. Both lookups are made only where the cache
/// slot `result.num` names holds nothing yet, and the callee found is
/// written into it, `CACHE_PTR(opline->result.num, fbc)`, for every
/// pass after; the test beside the handlers counts the lookups over two
/// passes. The frame it pushes is the one `INIT_FCALL` pushes, so the
/// `SEND_*_EX` and `DO_FCALL_BY_NAME` after it are served the same way.
///
/// ```php
/// namespace Foo;
/// echo \strlen("ab"), "|", strlen("abc"), "|", trim(" x "), "\n";
/// nope(1);
/// ```
/// ```expect exit=255
/// 2|3|x
///
/// Fatal error: Uncaught Error: Call to undefined function Foo\nope() in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
pub(crate) fn init_ns_fcall_by_name<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let written = ConstIdx(op.op2);
    let lowered = ConstIdx(op.op2.saturating_add(1));
    let unqualified = ConstIdx(op.op2.saturating_add(2));
    init_call(req, frame, code, op, &[lowered, unqualified], written)
}

/// `ZEND_JMP_FRAMELESS` (`Z/Zend/zend_vm_def.h:9743`): the jump
/// `zend_compile_ns_call` puts before the call by name of an unqualified
/// name in a namespace whose global function has a frameless entry.
/// `op1` is the lowered name with the namespace before it,
/// `foo\substr`; where the function table has that name the call by
/// name is what follows and the jump is not taken, `ZEND_JMP_FL_MISS`,
/// and where it has not, the name is the global function's and the jump
/// lands on the `FRAMELESS_ICALL_*` over it, `ZEND_JMP_FL_HIT`. The
/// answer is read off the cache slot `extended_value` names,
/// `CACHED_PTR(opline->extended_value)`: `ZEND_JMP_FL_UNPRIMED`, the
/// zero a fresh cache holds, on the first pass, when the table is
/// looked up once and `MISS` or `HIT` written into the slot
/// (`result = (func == NULL) + 1; CACHE_PTR(...)`), and that answer on
/// every pass after with no lookup, which the test beside the handlers
/// counts. So a namespaced twin declared after the first pass is not
/// seen by this opline, as php's is not.
///
/// ```php
/// namespace Foo;
/// $s = "hello";
/// var_dump(substr($s, 1, 3), substr($s, -2), str_contains($s, "ell"), trim(" " . $s . " "));
/// ```
/// ```expect
/// string(3) "ell"
/// string(2) "lo"
/// bool(true)
/// string(5) "hello"
/// ```
pub(crate) fn jmp_frameless<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let slot = CacheSlot(op.extended_value);
    let answer = match req.vm().cached(frame, slot) {
        Cached::Hit => Cached::Hit,
        Cached::Miss => Cached::Miss,
        Cached::Unprimed | Cached::Function(_) | Cached::Class(_) => {
            let name = exec::constant(code, ConstIdx(op.op1));
            let found = name
                .as_str_bytes()
                .is_some_and(|name| exec::find_function(req, name).is_some());
            let answer = if found { Cached::Miss } else { Cached::Hit };
            req.vm_mut().prime(frame, slot, answer);
            answer
        }
    };
    if answer == Cached::Hit {
        req.vm_mut().jump(frame, op.op2);
        return Ok(Stepped::Same(frame));
    }
    Ok(advance(req, frame))
}

/// `ZEND_FRAMELESS_ICALL_0` (`Z/Zend/zend_vm_def.h:9766`): the native at
/// `extended_value` of the engine's frameless table served over no
/// argument, its value written into the temporary `result` names. No
/// native of php 8.5.9 declares an entry of this arity
/// (`ZEND_FRAMELESS_FUNCTION(name, 0)` appears in no source at the pin),
/// so php's compiler emits this opcode for no program and no witness
/// reaches it under the oracle; the test beside the handlers holds it
/// over an op array built by hand, and what it shares with the three
/// below is [`frameless_call`], whose witnesses are theirs.
///
/// ```php
/// var_dump(str_contains("abc", "b"));
/// ```
/// ```expect
/// bool(true)
/// ```
pub(crate) fn frameless_icall_0<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    frameless_call(req, frame, code, op, &[])
}

/// `ZEND_FRAMELESS_ICALL_1` (`Z/Zend/zend_vm_def.h:9786`): the native
/// over `op1`, read `GET_OP1_ZVAL_PTR_DEREF` and freed `FREE_OP1` after
/// the native returned, as [`frameless_call`] reads and frees it.
///
/// ```php
/// $s = " a b ";
/// var_dump(trim($s), trim("\t\n x\0"), trim(12), trim(""));
/// ```
/// ```expect
/// string(3) "a b"
/// string(1) "x"
/// string(2) "12"
/// string(0) ""
/// ```
///
/// A raise inside the native reads the native's declaration off the
/// frame this opcode lays down, so its words are the framed call's, and
/// the trace prints the frame php synthesises for a frameless call
/// (`zend_fetch_debug_backtrace`, `Z/Zend/zend_builtin_functions.c:1984`),
/// with the operand as passed. An undefined CV is `null` with the
/// warning, and `null` to a string parameter is the 8.1 deprecation.
///
/// ```php
/// var_dump(trim($u));
/// trim(token_get_all("<?php"));
/// ```
/// ```expect exit=255
/// Warning: Undefined variable $u in %s on line 2
///
/// Deprecated: trim(): Passing null to parameter #1 ($string) of type string is deprecated in %s on line 2
/// string(0) ""
///
/// Fatal error: Uncaught TypeError: trim(): Argument #1 ($string) must be of type string, array given in %s:3
/// Stack trace:
/// #0 %s(3): trim(Array)
/// #1 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn frameless_icall_1<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    frameless_call(req, frame, code, op, &[(op.op1_type, op.op1)])
}

/// `ZEND_FRAMELESS_ICALL_2` (`Z/Zend/zend_vm_def.h:9812`): the native
/// over `op1` and `op2`, the two freed in that order after it returned.
///
/// ```php
/// $h = "haystack";
/// var_dump(str_contains($h, "st"), str_contains($h, ""), str_contains($h, "x"), str_contains(1234, 23), trim("xaxx", "x"), substr($h, 4), substr($h, -3), substr($h, 9), substr("ab", 1) . "c");
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// string(1) "a"
/// string(4) "tack"
/// string(3) "ack"
/// string(0) ""
/// string(2) "bc"
/// ```
///
/// What a coercion made is the body's own and not the operand's: a
/// trace taken after the first argument was coerced prints the number
/// it was passed as, where the framed call prints the string
/// (`Z_FLF_PARAM_STR` against `Z_PARAM_STR`), and the temporary an
/// argument came in is given up after the native returned, so the
/// heap reads the same before and after.
///
/// ```php
/// $b = memory_get_usage(); $a = "z"; str_contains($a . "y", $a . "x"); var_dump(memory_get_usage() - $b);
/// str_contains(12, token_get_all(""));
/// ```
/// ```expect exit=255
/// int(0)
///
/// Fatal error: Uncaught TypeError: str_contains(): Argument #2 ($needle) must be of type string, array given in %s:3
/// Stack trace:
/// #0 %s(3): str_contains(12, Array)
/// #1 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn frameless_icall_2<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    frameless_call(
        req,
        frame,
        code,
        op,
        &[(op.op1_type, op.op1), (op.op2_type, op.op2)],
    )
}

/// `ZEND_FRAMELESS_ICALL_3` (`Z/Zend/zend_vm_def.h:9846`): the native
/// over `op1`, `op2` and the `OP_DATA` after the opline
/// (`GET_OP_DATA_ZVAL_PTR_DEREF`), the three freed in that order after
/// it returned, and the loop stepped past the `OP_DATA`,
/// `ZEND_VM_NEXT_OPCODE_EX(1, 2)`.
///
/// ```php
/// $s = "hello world";
/// var_dump(substr($s, 0, 5), substr($s, 6, 100), substr($s, -5, 2), substr($s, 2, -8), substr($s, 3, -9), substr($s, 3, null), substr($s, 11, 1), substr($s, 5, 0));
/// ```
/// ```expect
/// string(5) "hello"
/// string(5) "world"
/// string(2) "wo"
/// string(1) "l"
/// string(0) ""
/// string(8) "lo world"
/// string(0) ""
/// string(0) ""
/// ```
///
/// ```php
/// substr("abc", 1, "x");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: substr(): Argument #3 ($length) must be of type ?int, string given in %s:2
/// Stack trace:
/// #0 %s(2): substr('abc', 1, 'x')
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn frameless_icall_3<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let at = req.vm().opline(frame);
    let Some(data) = exec::opline_at(code, at.saturating_add(1)) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_FRAMELESS_ICALL_3 with no OP_DATA after it",
        )));
    };
    frameless_call(
        req,
        frame,
        code,
        op,
        &[
            (op.op1_type, op.op1),
            (op.op2_type, op.op2),
            (data.op1_type, data.op1),
        ],
    )
}

/// The body the four `FRAMELESS_ICALL_*` handlers share: the native the
/// engine's frameless table holds at `extended_value` (`ZEND_FLF_FUNC`,
/// `ZEND_FLF_HANDLER`), served over the operands in `arguments`. php
/// hands its frameless body the operands and a `return_value` and no
/// frame; here the body is the native's one entry, which reads its
/// arguments off the call it is being served from, so the operands are
/// laid down as a call already serving, each read as the handler reads
/// it (`GET_OP*_ZVAL_PTR_DEREF`) with one more owner where php holds a
/// pointer, and the call is popped after the native returned, which
/// frees each operand where php's `FREE_OP*` frees it. What the native
/// returned is the temporary's, `ZVAL_NULL(result)` and then the
/// body's write of it; a native that threw leaves the throwable
/// pending as `ZEND_VM_NEXT_OPCODE_CHECK_EXCEPTION` finds it. The loop
/// steps past the opline and past the `OP_DATA` a third argument came
/// in.
fn frameless_call(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    arguments: &[(OperandKind, u32)],
) -> Edge {
    let Some(entry) = req.frameless_entry(op.extended_value) else {
        return Err(Fatal::Error(format!(
            "InternalError: {} indexes frameless entry {} and the engine's table has {} entries",
            op.opcode.name(),
            op.extended_value,
            req.frameless_count()
        )));
    };
    let values: Vec<Value> = arguments
        .iter()
        .map(|&(kind, field)| exec::take_operand(req, frame, code, kind, field))
        .collect();
    req.vm_mut().push_call(CallState {
        function: Function(entry.function),
        name: None,
        count: values.len(),
        arguments: values,
        this: None,
        called_scope: None,
        stage: CallStage::Serving,
        frameless: true,
        coerced: Vec::new(),
    });
    let served = req.serve_native(Function(entry.function));
    req.pop_call();
    let at = req.vm().opline(frame);
    let width = if arguments.len() == 3 { 2 } else { 1 };
    match served {
        Ok(value) => {
            let slot = exec::temporary_slot(code, op.out);
            let previous = req.set_slot(frame, slot, value);
            req.release(previous);
            req.vm_mut().jump(frame, at.saturating_add(width));
            Ok(Stepped::Same(frame))
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
/// value, through whichever name the write came.
///
/// ```php
/// $b = &$a; $a = 1; var_dump($b); $b = "x"; var_dump($a);
/// ```
/// ```expect
/// int(1)
/// string(1) "x"
/// ```
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
///
/// The owner the CV gives up is the last one an object has, so the
/// assignment is where its destructor is served (design §5.10): the
/// value written stands in the CV first and `i_zval_ptr_dtor(garbage)`
/// after it, so the second object is made and in place before the
/// first's `__destruct` is served, and the statement after the
/// assignment is served after that. What the CV still holds at the end is served by the
/// shutdown walk ([`Request::call_destructors`]).
///
/// ```php
/// class A {
///     public $n;
///     function __construct($n) { echo "c", $n, "\n"; $this->n = $n; }
///     function __destruct() { echo "d", $this->n, "\n"; }
/// }
/// $a = new A(1);
/// $a = new A(2);
/// echo "after\n";
/// ```
/// ```expect
/// c1
/// c2
/// d1
/// after
/// d2
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
        let previous = req.set_slot(frame, slot, value);
        req.release(previous);
    }
    let Some(at) = req.slot_ptr(frame, op.op1) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN over a CV the frame does not have",
        )));
    };
    let garbage = assign_to_place(at, value);
    req.release(garbage);
    Ok(advance(req, frame))
}

/// `ZEND_ASSIGN_REF` (`Z/Zend/zend_vm_def.h:2839`), `$b = &$a` over two
/// CVs: `zend_assign_to_variable_reference`
/// (`Z/Zend/zend_execute.c:565`). The source is read in place under
/// `BP_VAR_W`, so a CV nothing wrote is made `null` and no warning is
/// raised (`_get_zval_ptr_cv_BP_VAR_W`); one holding no cell is given
/// one over the value it held, `ZVAL_NEW_REF` (`ZVAL_MAKE_REF_EX`,
/// `Z/Zend/zend_types.h:1241`), thirty-two bytes out of the request's
/// heap that the source CV holds from here. The cell takes one more
/// owner (`GC_ADDREF`) and the target CV holds it; the temporary
/// `result` names is one more owner again under `RETURN_VALUE_USED`
/// (`ZVAL_COPY`), and it is the cell, which the `ASSIGN` or the `ECHO`
/// that reads the `VAR` follows; what the target held is given up
/// last (`GC_DTOR(garbage)`). Two names of one cell, `$a = &$a`, are
/// left as they stand, which php reads off the two places being one
/// (`variable_ptr == value_ptr`) and not off the two operands. A `VAR`
/// target is what a `FETCH_DIM_W` left for `$a[] = &$x`, written
/// through the `IS_INDIRECT` in it; one holding no `IS_INDIRECT` is an
/// object's dimension, php's `Error` `Cannot assign by reference to an
/// array dimension of an object`. A `VAR` source is `MAKE_REF` and
/// `ZEND_RETURNS_FUNCTION`, which land with a call as the source.
///
/// A dimension bound by `&` is one cell the array's entry and the CV
/// both hold, so a write through either is read through both, and an
/// array that holds a reference to itself is php's `*RECURSION*`.
///
/// ```php
/// $a[] = &$x; $x = 1; var_dump($a); $a[0] = 2; var_dump($x);
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   &int(1)
/// }
/// int(2)
/// ```
///
/// One cell, two names: a write through either is read through both,
/// and a plain assignment from one of them copies the value, not the
/// cell.
///
/// ```php
/// $b = &$a; var_dump($a, $b); $a = 1; $c = $b; $b = 2; var_dump($a, $b, $c);
/// ```
/// ```expect
/// NULL
/// NULL
/// int(2)
/// int(2)
/// int(1)
/// ```
///
/// The cell is php's thirty-two bytes and one CV bound to itself takes
/// one; the deltas are read before anything is printed, as `ASSIGN`'s
/// are.
///
/// ```php
/// $x = 1; $b = memory_get_usage(); $y = &$x; $held = memory_get_usage() - $b;
/// $x = &$x; $again = memory_get_usage() - $b; var_dump($held, $again, $x);
/// ```
/// ```expect
/// int(32)
/// int(32)
/// int(1)
/// ```
///
/// The `VAR` holds the cell, and an `ASSIGN` reading it follows the
/// cell to the value: `$c` is a copy and not a third name.
///
/// ```php
/// $a = 1; $c = $b = &$a; $b = 2; var_dump($c);
/// ```
/// ```expect
/// int(1)
/// ```
pub(crate) fn assign_ref<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(source) = write_place(req, frame, code, op.op2_type, op.op2) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_REF over a source php compiles no write over",
        )));
    };
    let target = match op.op1_type {
        OperandKind::Cv => req.slot_ptr(frame, op.op1),
        OperandKind::Var => {
            let at = req.slot_ptr(frame, exec::temporary_slot(code, op.op1));
            let Some(entry) = at.and_then(|at| at.read().as_indirect()) else {
                let thrown = crate::api::raising::throw(
                    req,
                    "Error",
                    "Cannot assign by reference to an array dimension of an object",
                );
                exec::free_operand(req, frame, code, op.op1_type, op.op1);
                return threw(req, frame, &thrown);
            };
            Some(entry)
        }
        OperandKind::Const | OperandKind::Tmp | OperandKind::Unused => None,
    };
    let Some(target) = target else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_REF over a target php compiles no write over",
        )));
    };
    let held = exec::value_of(source.read());
    // `ZEND_RETURNS_FUNCTION`: the source is a call, and a call that
    // returned by value has no cell to bind, which is php's notice and a
    // cell of its own over the value
    // (`zend_wrong_assign_to_variable_reference`).
    if op.extended_value & RETURNS_FUNCTION != 0 && held.as_php_ref().is_none() {
        crate::api::raising::notice(req, "Only variables should be assigned by reference");
        if req.is_pending() {
            return Ok(Stepped::Threw(frame));
        }
    }
    let cell = match held.as_php_ref() {
        Some(cell) if target == source => {
            write_out(req, frame, code, op, &cell.as_value());
            return Ok(advance(req, frame));
        }
        Some(cell) => *cell,
        None => {
            let held = held.undef_as_null();
            let cell = match req.reference(held) {
                Ok(cell) => cell,
                Err(thrown) => return threw(req, frame, &thrown),
            };
            // The owner the slot had of `held` is the cell's now.
            source.write(cell.as_value().to_zval());
            cell
        }
    };
    let bound = cell.as_value();
    req.addref(&bound);
    let garbage = exec::value_of(target.read());
    target.write(bound.to_zval());
    write_out(req, frame, code, op, &bound);
    req.release(garbage);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    Ok(advance(req, frame))
}

/// `ZEND_ASSIGN_OBJ` (`Z/Zend/zend_vm_def.h:2466`), `$o->p = v`, two
/// oplines: this one names the container and the property and the
/// `OP_DATA` after it carries the value (`GET_OP_DATA_ZVAL_PTR`), so the
/// handler steps over both (`ZEND_VM_NEXT_OPCODE_EX(1, 2)`). The
/// container is read in place ([`exec::container`]): a CV as it stands,
/// or a `VAR` through the `IS_INDIRECT` a `FETCH_OBJ_W` left there
/// (`_get_zval_ptr_ptr_var`), which is where [`php_heap::Frame::slot_ptr`]
/// is read through; a reference it holds is followed. The value is the
/// handler's own as `ASSIGN` takes it, a temporary moved and a CV or a
/// constant copied, a CV's reference followed, an undefined CV warned
/// of. What is no object is php's `Error` `Attempt to assign property
/// "p" on null` (`zend_throw_non_object_error`), `null` in the
/// temporary `out` names where it is read and the value given up
/// (`FREE_OP_DATA`). On an object a declared slot the scope may reach
/// is written in place (`zend_assign_to_variable`,
/// [`Request::assign_declared_slot`]), what it held released after the
/// write; one the scope may not reach is `zend_bad_property_access`'s
/// `Error`; a name the class did not declare goes to the object's
/// handler table's `write_property`, which lays it in the dynamic
/// table. The temporary `out` names is one more owner of the value
/// under `RETURN_VALUE_USED` (`ZVAL_COPY`) and is not written where
/// the compiler marked it `UNUSED`. Then `FREE_OP1_VAR_PTR`: a
/// `VAR` holding an `IS_INDIRECT` owns nothing and is left `UNDEF`, one
/// holding the object `NEW` or `FETCH_THIS` left gives it up. A typed
/// slot's coercion (`zend_assign_to_typed_prop`) is not served yet, and
/// only a native declares one, so a write to it is the internal-error
/// fatal naming the gap. Creating a dynamic property is php's two arms
/// at `write_std_property`'s `else`
/// ([`Request::dynamic_property_creation`]): the `Error`
/// `Cannot create dynamic property C::$p` for a class with
/// `ZEND_ACC_NO_DYNAMIC_PROPERTIES`, and the 8.2 deprecation for a
/// class without `ZEND_ACC_ALLOW_DYNAMIC_PROPERTIES`; a name the
/// dynamic table already holds is written with neither.
///
/// A property given a new string gives its old one back to the heap,
/// as a CV does under `ASSIGN`, and the deltas are read before anything
/// is printed for the same reason.
///
/// ```php
/// class C { public $p; }
/// $o = new C; $b = memory_get_usage();
/// $o->p = str_repeat("ab", 3000); $held = memory_get_usage() - $b;
/// $o->p = "x"; var_dump($held, memory_get_usage() - $b, $o->p);
/// ```
/// ```expect
/// int(8192)
/// int(0)
/// string(1) "x"
/// ```
///
/// The value is read before the container is tested, so an undefined
/// CV warns before the `Error` over the container is raised.
///
/// ```php
/// $u->p = $v;
/// ```
/// ```expect exit=255
/// Warning: Undefined variable $v in %s on line 2
///
/// Fatal error: Uncaught Error: Attempt to assign property "p" on null in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// A declared slot nothing has written is handed back as it stands by
/// the `FETCH_OBJ_W` before this, so the write through it finds `null`.
///
/// ```php
/// class C { public $q; }
/// $o = new C;
/// $o->q->p = 1;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Attempt to assign property "p" on null in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
///
/// ```php
/// class C { protected $p = 1; }
/// $o = new C;
/// $o->p = 2;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot access protected property C::$p in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
///
/// The deprecation is the creation's, once: the second write to the
/// same name raises nothing, and `stdClass`, which php registers with
/// `ZEND_ACC_ALLOW_DYNAMIC_PROPERTIES`, raises nothing at all.
///
/// ```php
/// class C { public $d = 1; }
/// $o = new C; $o->p = "x"; $o->p = "y";
/// $s = new stdClass; $s->q = 1;
/// var_dump($o, $s);
/// ```
/// ```expect
/// Deprecated: Creation of dynamic property C::$p is deprecated in %s on line 3
/// object(C)#1 (2) {
///   ["d"]=>
///   int(1)
///   ["p"]=>
///   string(1) "y"
/// }
/// object(stdClass)#2 (1) {
///   ["q"]=>
///   int(1)
/// }
/// ```
///
/// A class php registers with `ZEND_ACC_NO_DYNAMIC_PROPERTIES` refuses
/// the creation outright, which `Closure` is the one of.
///
/// ```php
/// $f = Closure::fromCallable("strlen");
/// $f->p = 1;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot create dynamic property Closure::$p in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn assign_obj<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let name = match op.op2_type {
        OperandKind::Const => exec::constant(code, ConstIdx(op.op2)),
        _ => {
            return Err(Fatal::Error(String::from(
                "InternalError: ZEND_ASSIGN_OBJ over a property named at run time",
            )));
        }
    };
    let at = req.vm().opline(frame);
    let Some(data) = exec::opline_at(code, at.saturating_add(1))
        .filter(|next| next.opcode == php_unit::Opcode::OpData)
    else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_OBJ with no OP_DATA after it",
        )));
    };
    let container =
        exec::container(req, frame, code, op.op1_type, op.op1, "ZEND_ASSIGN_OBJ")?.deref();
    let value = exec::take_operand(req, frame, code, data.op1_type, data.op1);
    let written = if let Some(object) = container.as_object().copied() {
        assign_property(req, frame, code, op, object, &name, value)?
    } else {
        let shown_name = String::from_utf8_lossy(name.as_str_bytes().unwrap_or_default());
        let shown = crate::ops::value_name(req, &container.undef_as_null());
        let thrown = crate::api::raising::throw(
            req,
            "Error",
            &format!("Attempt to assign property \"{shown_name}\" on {shown}"),
        );
        write_out(req, frame, code, op, &Value::null());
        req.release(value);
        Err(thrown)
    };
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    if let Err(thrown) = written {
        return threw(req, frame, &thrown);
    }
    req.vm_mut().jump(frame, at.saturating_add(2));
    Ok(Stepped::Same(frame))
}

/// `ASSIGN_OBJ`'s body over an object: the name placed
/// ([`Request::declared_slot_for_write`]), the temporary `out` names
/// written under `RETURN_VALUE_USED`, and the value into the declared
/// slot in place or through the object's handler table's
/// `write_property` for a name the class did not declare. `value` is
/// given up on the `Error` path, as `FREE_OP_DATA` gives it up.
fn assign_property(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    object: crate::api::values::PhpObject,
    name: &Value,
    value: Value,
) -> Result<Result<(), Thrown>, Fatal> {
    let bytes = name.as_str_bytes().unwrap_or_default();
    let shown_name = String::from_utf8_lossy(bytes).into_owned();
    let placed = match req.declared_slot_for_write(object.class(), bytes) {
        Ok(placed) => placed,
        Err(thrown) => {
            req.release(value);
            return Ok(Err(thrown));
        }
    };
    match placed {
        Some((_, true)) => Err(Fatal::Error(format!(
            "InternalError: ZEND_ASSIGN_OBJ over the typed property ${shown_name}, whose coercion is not served yet"
        ))),
        Some((slot, false)) => {
            write_out(req, frame, code, op, &value);
            req.assign_declared_slot(object.heap(), slot, value);
            Ok(Ok(()))
        }
        None => {
            let Some(key) = name.as_php_string().copied() else {
                return Err(Fatal::Error(String::from(
                    "InternalError: ZEND_ASSIGN_OBJ over a property whose name is no string",
                )));
            };
            write_out(req, frame, code, op, &value);
            req.addref(name);
            let table = req.object_handlers(object.heap());
            Ok(table.write_property(req, object, key, value))
        }
    }
}

/// `zend_assign_to_variable` (`Z/Zend/zend_execute.h:171`) over a place:
/// a reference standing there is followed and the write lands in the
/// cell, as `ASSIGN` over a CV bound by `&` lands there; the value is
/// written first and what the place held is handed back for the caller
/// to release last, after the `result` is copied, which is where a
/// destructor is served (design §5.10).
fn assign_to_place(at: php_heap::ZvalRef, value: Value) -> Value {
    let mut at = at;
    let mut held = exec::value_of(at.read());
    if let Some(cell) = held.as_php_ref() {
        at = cell.heap().val_ref();
        held = exec::value_of(at.read());
    }
    at.write(value.to_zval());
    held
}

/// `ZEND_ASSIGN_DIM` (`Z/Zend/zend_vm_def.h:2657`), `$a[k] = v` and
/// `$a[] = v`, two oplines: this one names the container and the
/// dimension and the `OP_DATA` after it carries the value, so the
/// handler steps over both (`ZEND_VM_NEXT_OPCODE_EX(1, 2)`). The
/// container is walked to the array it names ([`write_array`]), which is
/// where a `null` or an unwritten variable becomes one; then the two
/// arms php has. The append, an `UNUSED` dimension, reads the value
/// first and `zend_hash_next_index_insert`s it, php's `Error` `Cannot
/// add element to the array as the next element is already occupied`
/// where the next index is taken. A dimension is read first instead, so
/// an undefined variable in the dimension warns before one in the value
/// does, the entry is laid down ([`write_entry`]) and the value written
/// through it as `zend_assign_to_variable` writes one
/// ([`assign_to_place`]): a reference in the entry is followed, so a
/// write through `$a[0]` bound by `&` reaches what it is bound to. The
/// temporary `out` names is one more owner of the value under
/// `RETURN_VALUE_USED` and `null` where the write raised
/// (`assign_dim_error`); what the entry held is released last.
///
/// ```php
/// $a = [1];
/// $a[0] = 2;
/// $a[] = 3;
/// $a['k'] = 4;
/// $a['10'] = 5;
/// var_dump($a, $b[] = 6, $b);
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   int(2)
///   [1]=>
///   int(3)
///   ["k"]=>
///   int(4)
///   [10]=>
///   int(5)
/// }
/// int(6)
/// array(1) {
///   [0]=>
///   int(6)
/// }
/// ```
///
/// A dimension is read before the value, so php's two diagnostics over
/// an unwritten variable come in that order, and `null` as an offset is
/// the empty string.
///
/// ```php
/// $a[$k] = $v;
/// var_dump($a);
/// ```
/// ```expect
/// Warning: Undefined variable $k in %s on line 2
///
/// Deprecated: Using null as an array offset is deprecated, use an empty string instead in %s on line 2
///
/// Warning: Undefined variable $v in %s on line 2
/// array(1) {
///   [""]=>
///   NULL
/// }
/// ```
///
/// A write through an entry bound by `&` reaches what it is bound to,
/// and `false` becomes an array with php's deprecation where `null`
/// becomes one with none.
///
/// ```php
/// $a[0] = &$b;
/// $a[0] = 7;
/// $c = false;
/// $c[] = 1;
/// var_dump($b, $c);
/// ```
/// ```expect
/// Deprecated: Automatic conversion of false to array is deprecated in %s on line 5
/// int(7)
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// ```
///
/// A scalar is php's `Error`, and the array the write did not reach
/// stands as it was.
///
/// ```php
/// $a = 1;
/// $a[0] = 2;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot use a scalar value as an array in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn assign_dim<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let at = req.vm().opline(frame);
    let Some(data) = exec::opline_at(code, at.saturating_add(1))
        .filter(|next| next.opcode == php_unit::Opcode::OpData)
    else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_DIM with no OP_DATA after it",
        )));
    };
    let data = *data;
    let Some(place) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_DIM over a constant or a temporary, which php compiles no write over",
        )));
    };
    let written = match write_array(req, place, "ZEND_ASSIGN_DIM")? {
        Ok(array) if op.op2_type == OperandKind::Unused => {
            let value = exec::take_operand(req, frame, code, data.op1_type, data.op1);
            match req.array_append(array.heap(), value) {
                Ok(true) => {
                    write_out(req, frame, code, op, &value);
                    Ok(())
                }
                Ok(false) => Err(crate::api::raising::throw(
                    req,
                    "Error",
                    "Cannot add element to the array as the next element is already occupied",
                )),
                Err(thrown) => Err(thrown),
            }
        }
        Ok(array) => {
            let dim = exec::operand(req, frame, code, op.op2_type, op.op2);
            let entry = write_entry(req, array, Some(&dim), op.op2_type == OperandKind::Const)?;
            match entry {
                Ok(Some(entry)) => {
                    let value = exec::take_operand(req, frame, code, data.op1_type, data.op1);
                    let garbage = assign_to_place(entry, value);
                    write_out(req, frame, code, op, &value);
                    req.release(garbage);
                    Ok(())
                }
                Ok(None) => Err(crate::api::raising::throw(
                    req,
                    "Error",
                    "Cannot add element to the array as the next element is already occupied",
                )),
                Err(thrown) => Err(thrown),
            }
        }
        Err(thrown) => Err(thrown),
    };
    if op.op2_type != OperandKind::Unused {
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
    }
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    if let Err(thrown) = written {
        exec::free_operand(req, frame, code, data.op1_type, data.op1);
        write_out(req, frame, code, op, &Value::null());
        return threw(req, frame, &thrown);
    }
    req.vm_mut().jump(frame, at.saturating_add(2));
    Ok(Stepped::Same(frame))
}

/// `if (RETURN_VALUE_USED(opline)) ZVAL_COPY(EX_VAR(opline->result.var), value)`:
/// the temporary `out` names one more owner of `value`, and nothing
/// where the compiler marked the temporary `UNUSED`.
fn write_out(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline, value: &Value) {
    if op.out_type == OperandKind::Unused {
        return;
    }
    req.addref(value);
    let previous = req.set_slot(frame, exec::temporary_slot(code, op.out), *value);
    req.release(previous);
}

/// `ZEND_RETURNS_FUNCTION` (`Z/Zend/zend_compile.h:1198`), on an
/// `ASSIGN_REF` whose source is a call: what the handler reads to raise
/// php's notice where the call returned by value.
const RETURNS_FUNCTION: u32 = 1 << 0;

/// `ZEND_ARRAY_ELEMENT_REF`, `ZEND_ARRAY_NOT_PACKED` and
/// `ZEND_ARRAY_SIZE_SHIFT` (`Z/Zend/zend_compile.h:1201`): the bits of
/// `INIT_ARRAY`'s and `ADD_ARRAY_ELEMENT`'s `extended_value`, the element
/// count above the two flags.
const ARRAY_ELEMENT_REF: u32 = 1 << 0;
const ARRAY_NOT_PACKED: u32 = 1 << 1;
const ARRAY_SIZE_SHIFT: u32 = 2;

/// `ZEND_INIT_ARRAY` (`Z/Zend/zend_vm_def.h:6473`), the first opline of
/// an array expression `zend_try_ct_eval_array` left to the request:
/// `zend_new_array` sized for the count `extended_value` carries above
/// `ZEND_ARRAY_SIZE_SHIFT`, laid down as a hash at once where the
/// compiler set `ZEND_ARRAY_NOT_PACKED` (`zend_hash_real_init_mixed`),
/// into the temporary the opline's `result` names; then, `op1` being
/// the first element, `ZEND_VM_DISPATCH_TO_HANDLER(ZEND_ADD_ARRAY_ELEMENT)`
/// over the same opline, which is [`add_element`] here. An `UNUSED`
/// `op1` is the empty array and the next opline, which the compiler
/// folds to a constant and never emits.
///
/// ```php
/// var_dump(['x', 'y'], [1, $a = 2, 'k' => 3]);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(1) "x"
///   [1]=>
///   string(1) "y"
/// }
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   ["k"]=>
///   int(3)
/// }
/// ```
///
/// The table is php's: an array with no string key is packed, eight
/// slots of sixteen bytes and the two-word hash after the fifty-six-byte
/// header; one the compiler marked `ZEND_ARRAY_NOT_PACKED` is eight
/// buckets of thirty-two and a sixteen-word hash, so
/// `memory_get_usage()` reads the second as php does. The deltas are
/// read before anything is printed, as `ASSIGN`'s are.
///
/// ```php
/// $x = 1; $b = memory_get_usage(); $p = [$x, 2]; $packed = memory_get_usage() - $b;
/// $m = [$x, 'k' => 2]; $mixed = memory_get_usage() - $b - $packed;
/// var_dump($packed, $mixed);
/// ```
/// ```expect
/// int(216)
/// int(376)
/// ```
pub(crate) fn init_array<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let size = if op.op1_type == OperandKind::Unused {
        0
    } else {
        op.extended_value >> ARRAY_SIZE_SHIFT
    };
    let packed = op.extended_value & ARRAY_NOT_PACKED == 0;
    let array = match req.array_sized(size, packed) {
        Ok(array) => array,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, Value::array(array));
    req.release(previous);
    if op.op1_type == OperandKind::Unused {
        return Ok(advance(req, frame));
    }
    add_element(req, frame, code, op, array)
}

/// `ZEND_ADD_ARRAY_ELEMENT` (`Z/Zend/zend_vm_def.h:6224`): one more
/// element into the array the `result` temporary holds, which
/// `INIT_ARRAY` left there with one owner, through [`add_element`].
///
/// A key is written where it stands, so a repeated one keeps its first
/// place and takes the last value, and an element with no key lands
/// past the largest integer key so far, as `nNextFreeElement` moves.
///
/// ```php
/// $a = 1; $k = 'k';
/// var_dump([5 => $a, 'x', $k => 2, 5 => 'again', 'y']);
/// ```
/// ```expect
/// array(4) {
///   [5]=>
///   string(5) "again"
///   [6]=>
///   string(1) "x"
///   ["k"]=>
///   int(2)
///   [7]=>
///   string(1) "y"
/// }
/// ```
pub(crate) fn add_array_element<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let held = req.slot(frame, exec::temporary_slot(code, op.out));
    let Some(array) = held.as_array().copied() else {
        return Err(Fatal::Error(String::from(
            "InternalError: ADD_ARRAY_ELEMENT over a temporary holding no array",
        )));
    };
    add_element(req, frame, code, op, array)
}

/// The body `INIT_ARRAY` and `ADD_ARRAY_ELEMENT` share, the operand
/// addressing over [`crate::ops::add_array_element`]: `op1` taken as
/// the array's own (`Z_TRY_ADDREF` over a `CONST` or a `CV`, moved out
/// of a `TMP` or a `VAR`, a reference followed, a CV nothing wrote the
/// warning and `null`); `op2` read as it stands
/// (`GET_OP2_ZVAL_PTR_UNDEF`), a CV nothing wrote warned on here and
/// handed on as the `UNDEF` it is, a `CONST` marked constant so the
/// kernel keeps a numeric string; then `FREE_OP2`. The
/// `ZEND_ARRAY_ELEMENT_REF` arm, `[&$x]`, is not compiled yet and is an
/// internal error rather than a value written by the wrong rule.
fn add_element(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    array: crate::api::values::PhpArray,
) -> Edge {
    if op.extended_value & ARRAY_ELEMENT_REF != 0 {
        return Err(Fatal::Error(String::from(
            "InternalError: ADD_ARRAY_ELEMENT by reference, which this compiler does not emit yet",
        )));
    }
    let value = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    let offset = if op.op2_type == OperandKind::Unused {
        Offset::Next
    } else {
        let key = exec::operand_undef(req, frame, code, op.op2_type, op.op2);
        if key.tag() == TypeTag::Undef {
            exec::operand(req, frame, code, op.op2_type, op.op2);
        }
        Offset::At {
            key,
            constant: op.op2_type == OperandKind::Const,
        }
    };
    let added = crate::ops::add_array_element(req, array, offset, value);
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    match added {
        Ok(()) => Ok(advance(req, frame)),
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// `ZEND_FE_RESET_R` (`Z/Zend/zend_vm_def.h:6882`), the first opline of
/// a `foreach` by value: the subject read under `BP_VAR_R` with a
/// reference followed (`GET_OP1_ZVAL_PTR_DEREF`), so a CV nothing wrote
/// is the undefined-variable warning and `null`. An array is copied
/// into the `VAR` the opline's `result` names (`ZVAL_COPY_VALUE`) with
/// one more owner taken unless the operand is a `TMP`, whose owner
/// moves; the position `Z_FE_POS` starts at zero; a `VAR` operand is
/// freed (`FREE_OP1_IF_VAR`) and the next opline is the `FE_FETCH_R`.
/// An operand that is neither array nor object is php's warning, the
/// `VAR` left `UNDEF`, the operand freed (`FREE_OP1`) and the jump
/// `op2` names, to the `FE_FREE` past the loop.
///
/// An object whose class has no `get_iterator` is copied the same way
/// and walks its property table: the table separated where a cast
/// shares it ([`Request::separate_object_properties`]), and the
/// position of the walk, php's `Z_FE_ITER` off `zend_hash_iterator_add`
/// and this engine's `Z_FE_POS`, at the first property. php's iterator
/// list (`EG(ht_iterators)`), which follows a table its loop rehashes,
/// is not in the heap yet (#434); the position is the array loop's and
/// the `FE_FREE` has no iterator to delete. No class declares a
/// `get_iterator`, `Traversable` not being declared yet, so the other
/// arm, `zend_fe_reset_iterator`, is unreachable (#367).
///
/// The properties an object walks are the ones the executing scope may
/// see: a private or a protected one read from outside the class is
/// stepped over, as `zend_check_property_access` steps over its mangled
/// key, and the key is the name unmangled.
///
/// ```php
/// class P { public $a = 1; private $b = 2; public $c = 3; }
/// foreach (new P as $k => $v) echo "$k=$v ";
/// ```
/// ```expect
/// a=1 c=3
/// ```
///
/// An object of no properties runs the body no time; a property
/// declared with no default is the `null` php's
/// `default_properties_table` put there and is walked like any other. A
/// slot that holds nothing at all is stepped over as php steps over an
/// `IS_UNDEF` bucket; the two that make one, `unset` over a property
/// (#435) and a typed property with no default (#407), are not compiled
/// yet, so nothing in a program reaches that arm and #435's witness is
/// what will hold it.
///
/// ```php
/// foreach (new stdClass as $v) echo 'no';
/// class U { public $kept = 2; public $null; }
/// foreach (new U as $k => $v) echo $k, ' ';
/// ```
/// ```expect
/// kept null
/// ```
///
/// A packed array, a hash with string keys and an empty one: the loop
/// over the second runs its body no time, and the third's key and
/// value are one interpolated string (#355).
///
/// ```php
/// foreach (['x', 'y'] as $p) echo $p; foreach ([] as $v) echo 'no'; foreach (['k' => 1] as $k => $v) echo "$k=$v";
/// ```
/// ```expect
/// xyk=1
/// ```
///
/// The warning names the value as `zend_zval_value_name` names it,
/// and the request runs on past the loop.
///
/// ```php
/// foreach (1 as $v) {} echo "on";
/// ```
/// ```expect
/// Warning: foreach() argument must be of type array|object, int given in %s on line 2
/// on
/// ```
///
/// The loop reads the array it copied, so an assignment to the subject
/// inside the body changes nothing the loop sees, and the copy takes no
/// element of its own: one more owner, released by the `FE_FREE`. The
/// delta is read before anything is printed, as `ASSIGN`'s are.
///
/// ```php
/// $a = [1, 2, 3]; $b = memory_get_usage(); $n = 0;
/// foreach ($a as $v) { $a = []; $n = $n * 10 + $v; }
/// $d = memory_get_usage() - $b; var_dump($n, $a, $d);
/// ```
/// ```expect
/// int(123)
/// array(0) {
/// }
/// int(0)
/// ```
pub(crate) fn fe_reset_r<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let subject = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    let slot = exec::temporary_slot(code, op.out);
    if subject.as_array().is_some() {
        if op.op1_type != OperandKind::Tmp {
            req.addref(&subject);
        }
        let previous = req.set_slot(frame, slot, subject);
        req.release(previous);
        if op.op1_type == OperandKind::Var {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
        }
        return Ok(advance(req, frame));
    }
    if let Some(object) = subject.as_object() {
        if let Err(thrown) = req.separate_object_properties(*object) {
            return threw(req, frame, &thrown);
        }
        if op.op1_type != OperandKind::Tmp {
            req.addref(&subject);
        }
        let previous = req.set_slot(frame, slot, subject);
        req.release(previous);
        if let Some(cell) = req.slot_ptr(frame, slot) {
            let mut held = cell.read();
            held.set_fe_pos(0);
            cell.write(held);
        }
        if op.op1_type == OperandKind::Var {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
        }
        return Ok(advance(req, frame));
    }
    crate::api::raising::warning(
        req,
        &format!(
            "foreach() argument must be of type array|object, {} given",
            crate::ops::value_name(req, &subject)
        ),
    );
    let previous = req.set_slot(frame, slot, Value::undef());
    req.release(previous);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    req.vm_mut().jump(frame, op.op2);
    Ok(Stepped::Same(frame))
}

/// `ZEND_FE_FETCH_R` (`Z/Zend/zend_vm_def.h:7176`): the array in the
/// `VAR` `op1` names, read in place so its `Z_FE_POS` is the position
/// the last fetch left; the first live bucket from there
/// ([`php_heap::Arr::live_from`], the `while` over `IS_UNDEF`), or the
/// jump to the opline `extended_value` names when none is left. The
/// position moves one past the bucket, the key is written to the
/// `result` temporary when the opline has one (`RETURN_VALUE_USED`): the
/// position of a packed array's element, the bucket's `h` or its string
/// with one more owner (`ZVAL_STR_COPY`) for a hash. The value goes to
/// the CV `op2` names through `zend_assign_to_variable`, which is what
/// [`assign`] does: one more owner of the value with a reference
/// followed, written through the cell the CV holds or over what it held,
/// which is released after the write.
///
/// The key temporary is what the `ASSIGN` after the fetch moves into the
/// key variable: the position for a packed array, the bucket's key for
/// a hash, the integer one `nNextFreeElement` gave included.
///
/// ```php
/// foreach ([10, 20, 30] as $k => $v) echo $k, ':', $v, ';';
/// foreach (['b' => 1, 5 => 2, 'c' => 3, 4] as $k => $v) echo $k, ':', $v, ';';
/// ```
/// ```expect
/// 0:10;1:20;2:30;b:1;5:2;c:3;6:4;
/// ```
///
/// A `VAR` holding an object is `zend_fe_fetch_object_helper`
/// (`Z/Zend/zend_vm_def.h:7315`) instead: the property at the position
/// or the first one past it the executing scope may see
/// ([`Request::object_property_from`]), its unmangled name as the key,
/// and the same jump where nothing is left. The value is the property's
/// with a reference followed, one more owner, as an array element's is.
///
/// ```php
/// class C { public $n = 1; protected $p = 2; public $s = 'x'; private $q = 3; public function all() { foreach ($this as $k => $v) echo "$k=$v "; } }
/// $c = new C; foreach ($c as $k => $v) echo "$k=$v ";
/// $c->all();
/// ```
/// ```expect
/// n=1 s=x n=1 p=2 s=x q=3
/// ```
pub(crate) fn fe_fetch_r<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let slot = exec::temporary_slot(code, op.op1);
    let Some(at) = req.slot_ptr(frame, slot) else {
        return Err(Fatal::Error(String::from(
            "InternalError: FE_FETCH_R over a VAR the frame does not have",
        )));
    };
    let mut held = at.read();
    let found = if let Some(object) = Value::of_zval(held).as_object() {
        match req.object_property_from(*object, held.fe_pos()) {
            Ok(found) => found,
            Err(thrown) => return threw(req, frame, &thrown),
        }
    } else if let Some(array) = held.as_arr() {
        array
            .live_from(held.fe_pos())
            .map(|(pos, key, value)| (pos, key, exec::value_of(value)))
    } else {
        return Err(Fatal::Error(String::from(
            "InternalError: FE_FETCH_R over a VAR holding neither array nor object",
        )));
    };
    let Some((pos, key, value)) = found else {
        req.vm_mut().jump(frame, op.extended_value);
        return Ok(Stepped::Same(frame));
    };
    held.set_fe_pos(pos.saturating_add(1));
    at.write(held);
    if op.out_type != OperandKind::Unused {
        let key = match key {
            php_heap::Key::Long(n) => Value::int(n),
            php_heap::Key::Str(s) => {
                let s = Value::string(crate::api::values::PhpString::of(s));
                req.addref(&s);
                s
            }
        };
        let out = exec::temporary_slot(code, op.out);
        let previous = req.set_slot(frame, out, key);
        req.release(previous);
    }
    let value = value.deref();
    req.addref(&value);
    if let Some(r) = req.slot(frame, op.op2).as_php_ref() {
        req.reference_set(r.heap(), value);
    } else {
        let garbage = req.set_slot(frame, op.op2, value);
        req.release(garbage);
    }
    Ok(advance(req, frame))
}

/// `ZEND_FE_FREE` (`Z/Zend/zend_vm_def.h:3260`): the `VAR` the
/// `FE_RESET_R` filled given up where the loop ends, the array's or the
/// object's owner released (`zval_ptr_dtor_nogc`), and nothing over the
/// `UNDEF` a warned-on reset left. The `FE_FETCH_R` before it jumps
/// here, and a `break` out of the loop lands here too. php deletes the
/// hash iterator its object loop took as well
/// (`zend_hash_iterator_del`); this engine's object loop keeps the
/// position in the `VAR` and takes no iterator, so there is none to
/// delete (#434).
///
/// The array the `INIT_ARRAY` built is given back here and the string
/// it held stays with `$s` alone, 3072 bytes in the bin php puts it in,
/// given back when `$s` lets go; the deltas are read before anything is
/// printed, as `ASSIGN`'s are.
///
/// ```php
/// $b = memory_get_usage();
/// foreach ([str_repeat('a', 3000)] as $s) { $n = strlen($s); break; }
/// $held = memory_get_usage() - $b; $s = 1; $after = memory_get_usage() - $b; var_dump($n, $held, $after);
/// ```
/// ```expect
/// int(3000)
/// int(3072)
/// int(0)
/// ```
pub(crate) fn fe_free<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
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

/// `ZEND_FAST_CONCAT` (`Z/Zend/zend_vm_def.h:3285`): what an
/// interpolated string of two pieces compiles to
/// (`zend_compile_rope_finalize`), [`crate::ops::concat`] over the two
/// operands into the temporary, as `CONCAT` is. php's handler takes the
/// two-strings arm first and hands the rest to `zval_get_string_func`,
/// which is the kernel's first arm and its rest: each non-string piece
/// converted where php converts it, the `Array to string conversion`
/// warning and an object's `__toString` raised in operand order, and
/// the operands freed after the kernel (`FREE_OP1`, `FREE_OP2`).
///
/// ```php
/// $a = 1; echo "x$a", "$a";
/// $f = 1 / 3; $t = true; $n = null; $b = false; echo "|$f", "|$t", "|$n", "|$b|";
/// ```
/// ```expect
/// x11|0.33333333333333|1|||
/// ```
///
/// An array piece is the warning and the word `Array`; an object with a
/// `__toString` is what it returns, `Exception::__toString` here since
/// a user method is not compiled yet.
///
/// ```php
/// $a = [1]; $o = new Exception('m');
/// echo "a=$a", "|o=$o";
/// ```
/// ```expect
/// Warning: Array to string conversion in %s on line 3
/// a=Array|o=Exception: m in %s:2
/// Stack trace:
/// #0 {main}
/// ```
pub(crate) fn fast_concat<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::concat)
}

/// `ZEND_ROPE_INIT` (`Z/Zend/zend_vm_def.h:3410`): the first piece of
/// an interpolated string of three or more, converted as
/// `zval_get_string_func` converts it ([`crate::ops::to_string`]), into
/// element zero of the rope the `result` temporary holds, with
/// `extended_value` the number of pieces the rope will carry. php lays
/// the rope's `zend_string *` pointers over the stretch of slots the
/// compiler reserved, two to a zval; a zval here holds one value, so
/// the rope is a packed array of the pieces in the stretch's first
/// slot, sized for all of them at once, and the other reserved slots
/// stay `UNDEF`. What a program sees is the same: the pieces converted in
/// order, each warning and `__toString` raised where php raises it, one
/// string of their joined length at the `ROPE_END`. A piece whose
/// conversion threw leaves the throwable pending
/// (`ZEND_VM_NEXT_OPCODE_CHECK_EXCEPTION`).
///
/// ```php
/// $a = 1; $b = 2; echo "$a-$b-$a";
/// ```
/// ```expect
/// 1-2-1
/// ```
///
/// A CV nothing wrote is the undefined-variable warning and the empty
/// piece, `ZVAL_UNDEFINED_OP2` before the conversion.
///
/// ```php
/// $b = 2; echo "$u-$b-$u|";
/// ```
/// ```expect
/// Warning: Undefined variable $u in %s on line 2
///
/// Warning: Undefined variable $u in %s on line 2
/// -2-|
/// ```
pub(crate) fn rope_init<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let rope = match req.array_sized(op.extended_value, true) {
        Ok(rope) => rope,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, Value::array(rope));
    req.release(previous);
    rope_piece(req, frame, code, op, rope)
}

/// `ZEND_ROPE_ADD` (`Z/Zend/zend_vm_def.h:3445`): one more piece,
/// converted as [`crate::ops::to_string`] converts it, into element
/// `extended_value` of the rope `op1` names, which is the `result` too
/// (`op1 and result are the same`). The piece is appended, and
/// `extended_value` is its index by construction: the compiler numbers
/// the pieces from zero in emission order.
///
/// A float piece is printed at `precision`, a `true` piece is `1`, and
/// `null` and `false` pieces are empty.
///
/// ```php
/// $f = 1.5; $t = true; $n = null; $b = false;
/// echo "f=$f t=$t n=$n b=$b|";
/// ```
/// ```expect
/// f=1.5 t=1 n= b=|
/// ```
///
/// An object piece with no `__toString` is php's `Error`, thrown from
/// the conversion with the rope's pieces so far still held.
///
/// ```php
/// class B {}
/// $o = new B; $a = 1;
/// echo "$a-$o-$a";
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Object of class B could not be converted to string in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
pub(crate) fn rope_add<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let rope = rope_of(req, frame, code, op)?;
    rope_piece(req, frame, code, op, rope)
}

/// `ZEND_ROPE_END` (`Z/Zend/zend_vm_def.h:3480`): the last piece into
/// its element, then the pieces' lengths summed, one string of that
/// length filled with them in order, the rope given up and the string
/// into the temporary the opline's `result` names. A last piece whose
/// conversion threw releases every piece (`zend_string_release_ex` over
/// the rope) and leaves the temporary `UNDEF`, the throwable pending.
///
/// ```php
/// $a = 1; $b = 2; $c = 3; echo "$a$b$c|", "$a+$b+$c=", $a + $b + $c, "|";
/// ```
/// ```expect
/// 123|1+2+3=6|
/// ```
///
/// An array piece anywhere in the rope is the warning at the piece and
/// `Array` in its place; an object piece with a `__toString` is what it
/// returned, `Exception::__toString` here since a user method is not
/// compiled yet.
///
/// ```php
/// $a = [1]; $o = new Exception('m'); $n = 7;
/// echo "$n:$a:$o:$n\n";
/// ```
/// ```expect
/// Warning: Array to string conversion in %s on line 3
/// 7:Array:Exception: m in %s:2
/// Stack trace:
/// #0 {main}:7
/// ```
pub(crate) fn rope_end<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let mut rope = rope_of(req, frame, code, op)?;
    let rope_slot = exec::temporary_slot(code, op.op1);
    let piece = exec::operand(req, frame, code, op.op2_type, op.op2);
    let converted = crate::ops::to_string(req, &piece);
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    let appended = match converted {
        Ok(text) => rope.append(req, Value::string(text)),
        Err(thrown) => Err(thrown),
    };
    if let Err(thrown) = appended {
        let held = req.take_slot(frame, rope_slot);
        req.release(held);
        return threw(req, frame, &thrown);
    }
    let len: usize = rope
        .iter()
        .map(|(_, v)| {
            v.as_php_string()
                .map_or(0, crate::api::values::PhpString::len)
        })
        .sum();
    let joined = crate::api::values::PhpString::filled(req, len, |bytes| {
        let mut at: usize = 0;
        for (_, v) in &rope {
            let Some(s) = v.as_php_string() else {
                continue;
            };
            let end = at.saturating_add(s.len()).min(bytes.len());
            if let Some(target) = bytes.get_mut(at..end) {
                target.copy_from_slice(&s.as_bytes()[..target.len()]);
            }
            at = end;
        }
    });
    let held = req.take_slot(frame, rope_slot);
    req.release(held);
    match joined {
        Ok(joined) => {
            let out = exec::temporary_slot(code, op.out);
            let previous = req.set_slot(frame, out, Value::string(joined));
            req.release(previous);
            Ok(advance(req, frame))
        }
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// The rope the opline's `op1` temporary holds, the packed array
/// `ROPE_INIT` put there; a slot holding anything else is an invariant
/// the compiler's rope emission keeps, so it is an internal error.
fn rope_of(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Result<crate::api::values::PhpArray, Fatal> {
    let slot = exec::temporary_slot(code, op.op1);
    req.slot(frame, slot).as_array().copied().ok_or_else(|| {
        Fatal::Error(format!(
            "InternalError: {} over a temporary holding no rope",
            op.opcode.name()
        ))
    })
}

/// One piece of a rope: the `op2` operand converted as
/// `zval_get_string_func` converts it, the operand freed (`FREE_OP2`),
/// and the string appended to the rope; a conversion that threw leaves
/// the throwable pending, which is the `Threw` edge the loop reads.
fn rope_piece(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    mut rope: crate::api::values::PhpArray,
) -> Edge {
    let piece = exec::operand(req, frame, code, op.op2_type, op.op2);
    let converted = crate::ops::to_string(req, &piece);
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    let appended = match converted {
        Ok(text) => rope.append(req, Value::string(text)),
        Err(thrown) => Err(thrown),
    };
    match appended {
        Ok(()) => Ok(advance(req, frame)),
        Err(thrown) => threw(req, frame, &thrown),
    }
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

/// `ZEND_CAST`: [`crate::ops::cast`] over the operand to the type
/// `extended_value` names, into the temporary; the operand freed after
/// it. A type constant that is none of the handler's five arms is the
/// internal-error fatal naming it, as an opcode with no handler is.
pub(crate) fn cast<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(to) = crate::ops::CastTo::of(op.extended_value) else {
        return Err(Fatal::Error(format!(
            "InternalError: CAST to type {} has no conversion in this engine yet",
            op.extended_value
        )));
    };
    unary(req, frame, code, op, |req, value| {
        crate::ops::cast(req, value, to)
    })
}

/// `ZEND_BOOL`: [`crate::ops::is_true`] over the operand, into the
/// temporary; the operand freed after it. What `(bool)` compiles to
/// (`zend_compile_cast`), and a temporary `zend_do_free` never frees.
///
/// ```php
/// $x = "0"; echo (bool)$x, "|", (bool)"", "|", (bool)"a", "|", (bool)0.0, "|", (bool)"0.0", "|", (bool)null;
/// ```
/// ```expect
/// ||1||1|
/// ```
pub(crate) fn bool<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    unary(req, frame, code, op, |req, value| {
        Ok(Value::bool(crate::ops::is_true(req, value)))
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

/// `ZEND_JMPZ`: `ZEND_JMPNZ` with the test the other way round, the jump
/// in `op2` taken when the operand is false; what `zend_compile_switch`
/// emits over a case of a `switch (false)`. A backward one is served as
/// [`jmpnz`] serves it, though php's compiler emits none.
///
/// ```php
/// $a = 0;
/// switch (false) { case $a: echo "zero"; break; case 2: echo "two"; }
/// switch (true) { case $a: echo "zero"; break; default: echo "none"; }
/// ```
/// ```expect
/// zeronone
/// ```
pub(crate) fn jmpz<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::operand(req, frame, code, op.op1_type, op.op1);
    let taken = !crate::ops::is_true(req, &value);
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

/// `ZEND_SWITCH_LONG`: [`crate::ops::switch_long`] over the subject,
/// read as `GET_OP1_ZVAL_PTR_UNDEF` reads it, and the jump table in
/// `op2`; the frame at the target or at the default in
/// `extended_value`, or on to the `CASE` chain after it. The subject is
/// nobody's to free here; the chain and the switch's `FREE` read it.
pub(crate) fn switch_long<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let subject = exec::operand_undef(req, frame, code, op.op1_type, op.op1);
    let table = exec::constant(code, ConstIdx(op.op2));
    let Some(table) = table.as_array() else {
        return Err(no_table("SWITCH_LONG"));
    };
    Ok(switched(
        req,
        frame,
        crate::ops::switch_long(&subject, *table),
        op,
    ))
}

/// `ZEND_SWITCH_STRING`: [`crate::ops::switch_string`], the same way.
pub(crate) fn switch_string<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let subject = exec::operand_undef(req, frame, code, op.op1_type, op.op1);
    let table = exec::constant(code, ConstIdx(op.op2));
    let Some(table) = table.as_array() else {
        return Err(no_table("SWITCH_STRING"));
    };
    Ok(switched(
        req,
        frame,
        crate::ops::switch_string(&subject, *table),
        op,
    ))
}

/// `ZEND_MATCH`: [`crate::ops::match_target`] over the subject and the
/// jump table in `op2`; an undefined CV is `ZVAL_UNDEFINED_OP1`, the
/// warning [`exec::operand`] raises, and then the default.
pub(crate) fn r#match<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let mut subject = exec::operand_undef(req, frame, code, op.op1_type, op.op1);
    if subject.tag() == TypeTag::Undef {
        subject = exec::operand(req, frame, code, op.op1_type, op.op1);
    }
    let table = exec::constant(code, ConstIdx(op.op2));
    let Some(table) = table.as_array() else {
        return Err(no_table("MATCH"));
    };
    Ok(switched(
        req,
        frame,
        crate::ops::match_target(&subject, *table),
        op,
    ))
}

/// `ZEND_MATCH_ERROR`: [`crate::ops::match_unhandled_error`] over the
/// subject as it stands, `GET_OP1_ZVAL_PTR_UNDEF`, left pending; the
/// subject is not freed here, as php's handler frees nothing before
/// `HANDLE_EXCEPTION`.
pub(crate) fn match_error<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let subject = exec::operand_undef(req, frame, code, op.op1_type, op.op1);
    let thrown = crate::ops::match_unhandled_error(req, &subject);
    req.leave_pending(&thrown);
    Ok(Stepped::Threw(frame))
}

/// `ZEND_CASE`: [`crate::ops::is_equal`] over a temporary subject and
/// the case, `op2` freed and `op1` kept for the next case
/// (`FREE_OP2` alone), which is why `zend_compile_switch` emits it in
/// place of `IS_EQUAL` for a subject that is a temporary. The `FREE` at
/// the switch's end gives the subject up.
///
/// ```php
/// $a = "ab";
/// switch (strlen($a)) { case 1: echo "one"; break; case 2: echo "two"; break; case "2": echo "again"; break; }
/// switch (strlen($a)) { case "2": echo "again"; }
/// echo match (strlen($a)) { 1 => "one", 2 => "two" };
/// echo match (strlen($a)) { "2" => "string", 2 => "int" };
/// ```
/// ```expect
/// twoagaintwoint
/// ```
pub(crate) fn case<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    compared(req, frame, code, op, crate::ops::is_equal)
}

/// `ZEND_CASE_STRICT`: [`crate::ops::is_identical`], the same way, for
/// the arm of a `match` over a temporary subject.
pub(crate) fn case_strict<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    compared(req, frame, code, op, crate::ops::is_identical)
}

/// `ZEND_QM_ASSIGN`: the operand into the temporary `result` names, a
/// temporary moved and a CV or a constant copied with one more owner,
/// the reference a CV holds followed (`ZVAL_COPY_DEREF`), an undefined
/// CV warned about and `null`; what every arm of a `match` leaves its
/// value by.
pub(crate) fn qm_assign<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, value);
    req.release(previous);
    Ok(advance(req, frame))
}

/// `ZEND_RETURN`: the operand taken as the caller's (`ZVAL_COPY` of a
/// CV or a constant, a temporary moved), then the return safepoint, then
/// the frame popped (design §4.3 seam 8, §5.8), its CVs, temporaries
/// and receiver given up with it (`zend_leave_helper`), and the value
/// written into the caller's slot where the call wanted it and given up
/// where it did not (`RETURN_VALUE_USED`). The caller goes on after its
/// call opline. The safepoint carries the number the compiler wrote in
/// `result`, and is raised for every frame but the entry frame of this
/// serving, which the stack's entry slot says.
///
/// ```php
/// function f() { return; }
/// function g() { $a = 1; }
/// function h($x) { return $x; }
/// var_dump(f(), g(), h(1.5), h('s'), h(null), h([1]));
/// ```
/// ```expect
/// NULL
/// NULL
/// float(1.5)
/// string(1) "s"
/// NULL
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// ```
pub(crate) fn r#return<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let returned = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    returned_to_caller::<T>(req, frame, op, returned)
}

/// The half of `RETURN` that leaves the frame, which `RETURN_BY_REF`
/// shares: the return safepoint, the frame popped, and `returned` into
/// the `VAR` the call opline named, or released where the caller reads
/// nothing.
fn returned_to_caller<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    op: &Opline,
    returned: Value,
) -> Edge {
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
                    stepped
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

/// The body `CASE` and `CASE_STRICT` share: [`binary`] with `op1` kept,
/// `FREE_OP2` alone, since the subject is read again by the next case.
fn compared(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    kernel: fn(&mut Request, &Value, &Value) -> Result<Value, Thrown>,
) -> Edge {
    let left = exec::operand(req, frame, code, op.op1_type, op.op1);
    let right = exec::operand(req, frame, code, op.op2_type, op.op2);
    let computed = kernel(req, &left, &right);
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

/// Where a jump-table opcode sends the frame: `ZEND_VM_SET_RELATIVE_OPCODE`
/// to the target or to `extended_value`, or `ZEND_VM_NEXT_OPCODE` on to
/// the chain. Every target is forward, so none is a safepoint site.
fn switched(req: &mut Request, frame: FrameRef, switched: Switched, op: &Opline) -> Stepped {
    match switched {
        Switched::Target(target) => {
            req.vm_mut().jump(frame, target);
            Stepped::Same(frame)
        }
        Switched::Default => {
            req.vm_mut().jump(frame, op.extended_value);
            Stepped::Same(frame)
        }
        Switched::Chain => advance(req, frame),
    }
}

/// The fatal for a jump-table opcode whose `op2` is no array, which no
/// unit the compiler wrote has (design §10).
fn no_table(opcode: &str) -> Fatal {
    Fatal::Error(format!(
        "InternalError: {opcode} names a constant that is no jump table"
    ))
}

/// `ZEND_VM_NEXT_OPCODE`: the same frame, at the opline after this one,
/// and the `Threw` edge where the opline left a throwable pending,
/// which is `ZEND_VM_NEXT_OPCODE_CHECK_EXCEPTION`. php has the two
/// macros and spends the test only in the handlers that can raise; this
/// is one rule for both, because nothing is pending after a handler php
/// gives the unchecked macro to, and the release a handler makes last
/// is where a destructor is served (design §5.10), which is PHP code
/// between this opline and the next.
fn advance(req: &mut Request, frame: FrameRef) -> Stepped {
    req.vm_mut().advance(frame);
    if req.is_pending() {
        return Stepped::Threw(frame);
    }
    Stepped::Same(frame)
}

/// The body the two `SEND_*` handlers share: the operand placed in the
/// call frame being built, at the argument number `op2` names, the frame
/// owning it from here: a CV or a constant with one more owner
/// (`ZVAL_COPY`, `SEND_VAR`'s and `SEND_VAL`'s copy), a temporary moved
/// (`ZVAL_COPY_VALUE`).
fn send(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let value = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    place_argument(req, frame, op, value)
}

/// The placing half of [`send`]: `value` into the call frame being
/// built, at the argument number `op2` names, the frame owning it from
/// here.
fn place_argument(req: &mut Request, frame: FrameRef, op: &Opline, value: Value) -> Edge {
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

/// `ZEND_LAST_CATCH` (`Z/Zend/zend_compile.h`): the bit beside the cache
/// slot on the `extended_value` of the last `CATCH` of a `try`, the one
/// that rethrows rather than jumping to a clause after it.
const LAST_CATCH: u32 = 1 << 0;

/// `ZEND_FREE_ON_RETURN`: the bit on the `FREE` or `FE_FREE` a `return`,
/// a `break` or a `continue` emits for a loop variable it leaves behind,
/// which moves where a throwable those oplines raise is held to have
/// been raised.
const FREE_ON_RETURN: u32 = 1 << 0;

/// `ZEND_THROW` (`Z/Zend/zend_vm_def.h:4777`): the operand made pending,
/// `zend_throw_exception_object`, with one more owner for the pending
/// slot before the operand is freed (`Z_TRY_ADDREF_P`, `FREE_OP1`). A
/// reference is followed; a value that is no object is php's `Error`
/// saying only objects are thrown, and an object of a class that does
/// not implement `Throwable` php's `Error` saying so.
///
/// php saves and restores `EG(exception)` around the raise
/// (`zend_exception_save`), which chains a throwable already pending as
/// the new one's `previous`; nothing is ever pending at a `THROW` in
/// this loop, which reports [`Stepped::Threw`] at the opline that
/// raised, so the save and the restore are both nothing here.
///
/// ```php
/// try { throw new Exception("boom", 7); } catch (Exception $e) { var_dump($e->getMessage(), $e->getCode()); }
/// try { throw new stdClass; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $n = 1; throw $n; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "boom"
/// int(7)
/// Cannot throw objects that do not implement Throwable
/// Can only throw objects
/// ```
pub(crate) fn throw<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    let thrown = thrown_value(req, &value);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    threw(req, frame, &thrown)
}

/// `zend_throw_exception_object` (`Z/Zend/zend_exceptions.c`): the object
/// pending where it implements `Throwable`, and php's `Error` in php's
/// words where the value is no object or its class implements nothing.
fn thrown_value(req: &mut Request, value: &Value) -> Thrown {
    let Some(object) = value.as_object() else {
        return crate::api::raising::throw(req, "Error", "Can only throw objects");
    };
    let throwable = req.class_named(b"throwable");
    if !throwable.is_some_and(|of| req.instance_of(object.class(), of)) {
        return crate::api::raising::throw(
            req,
            "Error",
            "Cannot throw objects that do not implement Throwable",
        );
    }
    crate::api::raising::throw_value(req, value)
}

/// `ZEND_CATCH` (`Z/Zend/zend_vm_def.h:4812`): nothing pending is the
/// fall-through of a `try` that did not throw, and jumps past the
/// clause; otherwise the class named by the lowered half of the
/// constant pair in `op1`, looked up silently and without autoloading,
/// against the throwable's own (`instanceof_function`). A clause that
/// does not match jumps to the next `CATCH`, and the last of the `try`,
/// which carries `ZEND_LAST_CATCH` and names no next, leaves the
/// throwable pending for the unwinder to take further out
/// (`zend_rethrow_exception`). A clause that matches takes the
/// throwable off the pending slot and binds it to the CV `result`
/// names, a reference the CV holds written through, and gives it up
/// where the clause names no variable.
///
/// The class is resolved once: the slot `extended_value` carries beside
/// `ZEND_LAST_CATCH` holds it after the first pass, and every pass after
/// reads it back, `CACHE_PTR(opline->extended_value & ~ZEND_LAST_CATCH,
/// catch_ce)` over [`cached_class`]. A clause naming a class nothing
/// declares primes nothing, so it looks the table up on every pass, as
/// php's does over the null it cached.
///
/// ```php
/// function f() { try { throw new TypeError("t"); } catch (ValueError $e) { echo "no\n"; } }
/// try { f(); } catch (TypeError $e) { echo "outer ", $e->getMessage(), "\n"; }
/// try { throw new TypeError("m"); } catch (ValueError|TypeError $e) { echo get_class($e), " ", $e->getMessage(), "\n"; }
/// try { throw new Exception("u"); } catch (Exception) { echo "bare\n"; }
/// ```
/// ```expect
/// outer t
/// TypeError m
/// bare
/// ```
///
/// The slot is the frame's, so every iteration of a loop reads the one
/// the first pass primed, and the clause answers the same each time.
///
/// ```php
/// for ($i = 0; $i < 3; $i++) {
///   try { throw new TypeError("t$i"); } catch (ValueError $e) { echo "no\n"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// t0
/// t1
/// t2
/// ```
pub(crate) fn catch<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(pending) = req.pending() else {
        return jump_to(req, frame, op.op2);
    };
    let slot = Some(CacheSlot(op.extended_value & !LAST_CATCH));
    let matched = cached_class(req, frame, code, op.op1, slot)
        .is_some_and(|of| req.instance_of(pending.class(), of));
    if !matched {
        if op.extended_value & LAST_CATCH != 0 {
            return Ok(Stepped::Threw(frame));
        }
        return jump_to(req, frame, op.op2);
    }
    let Some(caught) = req.take_pending() else {
        return Ok(advance(req, frame));
    };
    if op.out_type == OperandKind::Cv {
        let value = caught.as_value();
        if let Some(cell) = req.slot(frame, op.out).as_php_ref() {
            req.reference_set(cell.heap(), value);
        } else {
            let garbage = req.set_slot(frame, op.out, value);
            req.release(garbage);
        }
    } else {
        req.release(caught.as_value());
    }
    Ok(advance(req, frame))
}

/// `ZEND_FAST_CALL` (`Z/Zend/zend_vm_def.h:8647`): the cell of the
/// temporary `result` names records the opline to return past and no
/// delayed throwable, and the frame jumps into the `finally` block
/// `op1` names, which `pass_two` wrote there over the number of the
/// `try` element.
///
/// ```php
/// function f() { try { return "t"; } finally { echo "f\n"; } }
/// echo f(), "\n";
/// try { echo "a\n"; } finally { echo "b\n"; }
/// ```
/// ```expect
/// f
/// t
/// a
/// b
/// ```
pub(crate) fn fast_call<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    _: &OpArray,
    op: &Opline,
) -> Edge {
    let at = req.vm().opline(frame);
    let left = req.vm_mut().fast_call(frame, op.out).and_then(|cell| {
        cell.from = Some(at);
        cell.delayed.take()
    });
    // php's temporary is dead at every `FAST_CALL` and holds no object;
    // a cell this engine kept from an earlier pass is given up rather
    // than written over.
    if let Some(object) = left {
        req.release(object.as_value());
    }
    jump_to(req, frame, op.op1)
}

/// `ZEND_FAST_RET` (`Z/Zend/zend_vm_def.h:8658`): where the cell records
/// a `FAST_CALL`, the frame goes on at the opline after it; where it
/// records none the block was entered by an unwind, so the throwable it
/// delayed is pending again and the unwind goes on from here, at the
/// `try` element `op2` names, php's `orig_try_catch_offset` and
/// `(uint32_t)-1` outside every `try`.
///
/// ```php
/// function f() { try { throw new Exception("x"); } finally { echo "f\n"; } }
/// try { f(); } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// f
/// caught x
/// ```
pub(crate) fn fast_ret<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let cell = req.vm_mut().fast_call(frame, op.op1).map(|cell| {
        let from = cell.from.take();
        let delayed = cell.delayed.take();
        (from, delayed)
    });
    let (from, delayed) = cell.unwrap_or((None, None));
    if let Some(from) = from {
        return jump_to(req, frame, from.saturating_add(1));
    }
    if let Some(object) = delayed {
        req.leave_pending(&Thrown::of(Some(object)));
    }
    let at = req.vm().opline(frame);
    let enclosing = (op.op2 != u32::MAX).then_some(op.op2);
    unwind_from::<T>(req, frame, code, enclosing, at)
}

/// `ZEND_DISCARD_EXCEPTION` (`Z/Zend/zend_vm_def.h:8620`): the opline a
/// `return` out of a `finally` block emits for the block it leaves. The
/// value a `FAST_CALL` was carrying past the block is given up, since
/// the return it belonged to never happened, and so is the throwable
/// the cell delayed.
///
/// ```php
/// function f() { try { throw new Exception("x"); } finally { return "r"; } }
/// echo f(), "\n";
/// ```
/// ```expect
/// r
/// ```
pub(crate) fn discard_exception<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let cell = req.vm_mut().fast_call(frame, op.op1).map(|cell| {
        let from = cell.from;
        (from, cell.delayed.take())
    });
    let (from, delayed) = cell.unwrap_or((None, None));
    if let Some(from) = from {
        free_carried_value(req, frame, code, from);
    }
    if let Some(object) = delayed {
        req.release(object.as_value());
    }
    Ok(advance(req, frame))
}

/// `ZEND_HANDLE_EXCEPTION` (`Z/Zend/zend_vm_def.h:8176`), the unwinder:
/// what the loop runs at the opline a handler left a throwable pending
/// at, php's `EG(opline_before_exception)`. The innermost `try` element
/// the opline stands in is found, the calls the frame was still
/// building are given up (`cleanup_unfinished_calls`), the temporary
/// the throwing opline was to write is given up, and the elements are
/// walked outwards ([`unwind_from`]).
///
/// php reaches this by replacing the frame's opline with its
/// `exception_op`; the loop here calls it on the [`Stepped::Threw`]
/// edge with the throwing opline still standing, which is the same
/// opline php reads back out of `EG(opline_before_exception)`.
///
/// ```php
/// function f() { try { throw new Exception("x"); } catch (TypeError $e) { echo "no\n"; } }
/// try { f(); } catch (Exception $e) { echo "outer ", $e->getMessage(), "\n"; } finally { echo "f\n"; }
/// ```
/// ```expect
/// outer x
/// f
/// ```
pub(crate) fn handle_exception<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let mut op_num = req.vm().opline(frame);
    // A throwable a loop variable's destruction raised on the way out of
    // a `foreach` is held to have been raised at the last of the frees,
    // which is where the `try` element covering it is looked for.
    if matches!(op.opcode, Opcode::Free | Opcode::FeFree) && op.extended_value & FREE_ON_RETURN != 0
    {
        let moved = op_num.saturating_add(op.op2);
        cleanup_live_vars(req, frame, code, op_num, moved);
        op_num = moved;
    }
    let mut current = None;
    for (at, element) in code.try_catch.iter().enumerate() {
        if element.try_op.0 > op_num {
            break;
        }
        if op_num < element.catch_op.0 || op_num < element.finally_end.0 {
            current = u32::try_from(at).ok();
        }
    }
    cleanup_unfinished_calls(req, frame);
    free_throwing_result(req, frame, code, op);
    unwind_from::<T>(req, frame, code, current, op_num)
}

/// `zend_dispatch_try_catch_finally_helper` (`Z/Zend/zend_vm_def.h:8100`):
/// the `try` elements from `from` outwards, each answered by where
/// `op_num` stands in it. Before its `catch_op` with a throwable
/// pending, the frame lands at the clause. Before its `finally_op`, the
/// throwable is delayed in the element's fast-call cell and the frame
/// lands at the block, which will end in a `FAST_RET` that takes the
/// unwind up again. Inside the block already, the value a `return` was
/// carrying is given up and the throwable the block delayed is chained
/// under the one being unwound (`zend_exception_set_previous`) or
/// becomes it.
///
/// Nothing left to answer is php's uncaught path: the frame's live
/// temporaries are given up and the frame is left, its caller reading
/// the throwable at the call opline as php's `zend_leave_helper` leaves
/// it. The entry frame of a serving is not left; the loop renders the
/// uncaught fatal there, or leaves the throwable pending for the native
/// that entered it.
fn unwind_from<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    from: Option<u32>,
    op_num: u32,
) -> Edge {
    let mut at = from;
    while let Some(offset) = at {
        let Some(element) = code.try_catch.get(offset as usize).copied() else {
            break;
        };
        if op_num < element.catch_op.0 && req.is_pending() {
            cleanup_live_vars(req, frame, code, op_num, element.catch_op.0);
            return jump_to(req, frame, element.catch_op.0);
        }
        if op_num < element.finally_op.0 {
            let Some(var) = fast_call_var(code, element) else {
                return Err(Fatal::Error(String::from(
                    "InternalError: a try element whose finally_end names no FAST_RET",
                )));
            };
            cleanup_live_vars(req, frame, code, op_num, element.finally_op.0);
            let delayed = req.take_pending();
            if let Some(cell) = req.vm_mut().fast_call(frame, var) {
                cell.delayed = delayed;
                cell.from = None;
            }
            return jump_to(req, frame, element.finally_op.0);
        }
        if op_num < element.finally_end.0 {
            let Some(var) = fast_call_var(code, element) else {
                return Err(Fatal::Error(String::from(
                    "InternalError: a try element whose finally_end names no FAST_RET",
                )));
            };
            let carried = req
                .vm_mut()
                .fast_call(frame, var)
                .and_then(|cell| cell.from);
            if let Some(from) = carried {
                free_carried_value(req, frame, code, from);
            }
            let delayed = req
                .vm_mut()
                .fast_call(frame, var)
                .and_then(|cell| cell.delayed.take());
            if let Some(delayed) = delayed {
                match req.pending() {
                    Some(pending) => req.set_previous(pending, delayed),
                    None => req.leave_pending(&Thrown::of(Some(delayed))),
                }
            }
        }
        at = offset.checked_sub(1);
    }
    cleanup_live_vars(req, frame, code, op_num, 0);
    if req.stack_mut().entry() == frame {
        return Ok(Stepped::Threw(frame));
    }
    let returns_to = req.vm().returns_to(frame);
    req.pop_frame();
    let caller = req.stack_mut().current();
    // `zend_leave_helper`: the callee wrote no return value, so the
    // caller's temporary for the call is left undefined, and the caller
    // stands at the call opline, not past it, for the unwind to go on
    // from there.
    if let Some((at, slot)) = returns_to {
        let garbage = req.take_slot(at, slot);
        req.release(garbage);
    }
    Ok(Stepped::Threw(caller))
}

/// `EX(func)->op_array.opcodes[try_catch->finally_end].op1.var`: the
/// fast-call temporary of one `try` element, which its `FAST_RET` names.
fn fast_call_var(code: &OpArray, element: php_unit::TryCatch) -> Option<u32> {
    exec::opline_at(code, element.finally_end.0).map(|op| op.op1)
}

/// The value a `FAST_CALL` emitted by a `return` was carrying past the
/// `finally` block in its `op2`, given up where the return never
/// happened.
fn free_carried_value(req: &mut Request, frame: FrameRef, code: &OpArray, from: u32) {
    let Some(op) = exec::opline_at(code, from) else {
        return;
    };
    if !matches!(op.op2_type, OperandKind::Tmp | OperandKind::Var) {
        return;
    }
    let slot = exec::temporary_slot(code, op.op2);
    let garbage = req.take_slot(frame, slot);
    req.release(garbage);
}

/// `ZEND_HANDLE_EXCEPTION`'s free of the throwing opline's `result`: a
/// temporary the opline was to write and did not. php leaves the four
/// opcodes that build a structure to their live ranges, and the two
/// whose `result` is a class rather than a value to nobody; a smart
/// branch may never have written its temporary, which reads `UNDEF`
/// here and is nobody's either.
fn free_throwing_result(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) {
    if !matches!(op.out_type, OperandKind::Tmp | OperandKind::Var) {
        return;
    }
    if matches!(
        op.opcode,
        Opcode::AddArrayElement
            | Opcode::AddArrayUnpack
            | Opcode::RopeInit
            | Opcode::RopeAdd
            | Opcode::FetchClass
            | Opcode::DeclareAnonClass
    ) {
        return;
    }
    let slot = exec::temporary_slot(code, op.out);
    let garbage = req.take_slot(frame, slot);
    req.release(garbage);
}

/// `cleanup_live_vars` (`Z/Zend/zend_execute.c`): every live range the
/// unwind leaves, given up. A range is left when it is open at `op_num`
/// and the opline being landed on is outside it; `catch_op` of zero is
/// php's "landing nowhere", the walk out of the frame, where every open
/// range is left.
///
/// php frees each kind by what it holds; every kind here is one value in
/// one temporary, freed the same way, and the one that is not,
/// `ZEND_LIVE_SILENCE`, holds the saved `error_reporting`, which is an
/// integer nobody owns and whose restore arrives with `BEGIN_SILENCE`.
fn cleanup_live_vars(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op_num: u32,
    catch_op: u32,
) {
    for range in &code.live_ranges {
        if range.start.0 > op_num {
            break;
        }
        if op_num >= range.end.0 {
            continue;
        }
        if catch_op != 0 && catch_op >= range.start.0 && catch_op < range.end.0 {
            continue;
        }
        let slot = exec::temporary_slot(code, range.var);
        let garbage = req.take_slot(frame, slot);
        req.release(garbage);
    }
}

/// `cleanup_unfinished_calls` (`Z/Zend/zend_execute.c`): the calls the
/// frame was still building or serving when the throwable was raised,
/// given up innermost first. A frame records how many calls stood below
/// it, which is where its own begin.
fn cleanup_unfinished_calls(req: &mut Request, frame: FrameRef) {
    let below = req.vm().at(frame).map_or(0, |state| state.calls_below);
    while u32::try_from(req.vm().calls().len()).unwrap_or(0) > below {
        req.pop_call();
    }
}

/// `ZEND_VM_JMP_EX(target, 0)`: the frame at `at`, with neither the
/// interrupt test nor the pending test the loop's own jumps make.
fn jump_to(req: &mut Request, frame: FrameRef, at: u32) -> Edge {
    req.vm_mut().jump(frame, at);
    Ok(Stepped::Same(frame))
}

/// `ZEND_RETURN_BY_REF` (`Z/Zend/zend_vm_def.h:4586`), what a function
/// written `function &f` returns by: the variable is made a reference
/// where it holds none (`ZVAL_MAKE_REF_EX`, as `SEND_REF` makes one),
/// and the caller's `VAR` holds that cell, so the `ASSIGN_REF` after the
/// call binds a name to what the callee named. A constant or a temporary
/// is php's notice `Only variable references should be returned by
/// reference` and a fresh cell over the value, which is the arm
/// `ZEND_RETURNS_VALUE` names; the frame is left as [`r#return`] leaves
/// it.
///
/// ```php
/// function &f(&$x) { return $x; }
/// $a = 1;
/// $b = &f($a);
/// $b = 2;
/// var_dump($a);
/// ```
/// ```expect
/// int(2)
/// ```
///
/// ```php
/// function &f() { return 1; }
/// $b = &f();
/// var_dump($b);
/// ```
/// ```expect
/// Notice: Only variable references should be returned by reference in %s on line 2
/// int(1)
/// ```
pub(crate) fn return_by_ref<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let returned = if let Some(at) = write_place(req, frame, code, op.op1_type, op.op1) {
        let held = exec::value_of(at.read());
        let cell = if let Some(cell) = held.as_php_ref() {
            req.addref(&held);
            *cell
        } else {
            let cell = match req.reference(held.undef_as_null()) {
                Ok(cell) => cell,
                Err(thrown) => return threw(req, frame, &thrown),
            };
            at.write(cell.as_value().to_zval());
            req.addref(&cell.as_value());
            cell
        };
        cell.as_value()
    } else {
        // `ZEND_RETURNS_VALUE`: a constant or a temporary is no
        // variable, so php raises its notice and wraps the value in a
        // cell of its own.
        crate::api::raising::notice(
            req,
            "Only variable references should be returned by reference",
        );
        let value = exec::take_operand(req, frame, code, op.op1_type, op.op1);
        match req.reference(value) {
            Ok(cell) => cell.as_value(),
            Err(thrown) => return threw(req, frame, &thrown),
        }
    };
    returned_to_caller::<T>(req, frame, op, returned)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use php_heap::Intern as _;
    use php_unit::{Opcode, OperandKind, Unit};

    use super::*;
    use crate::api::values::{ClassId, PhpObject};
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

    /// A unit whose main op array is `INIT_FCALL_BY_NAME` over the pair
    /// `StrLen`, `strlen`, a `SEND_VAL_EX` of the constant `"abc"` and a
    /// `DO_FCALL_BY_NAME` into `V0`: the shape php's compiler emits for
    /// `StrLen("abc")` where its function table has not the name at
    /// compile time and the program declares it before the call runs.
    fn by_name(strings: &mut php_heap::Strings) -> Arc<Unit> {
        let text = |strings: &mut php_heap::Strings, bytes: &[u8]| {
            strings
                .intern(bytes)
                .map_or(php_heap::Zval::UNDEF, php_heap::Zval::string)
        };
        let written = text(strings, b"StrLen");
        let lowered = text(strings, b"strlen");
        let argument = text(strings, b"abc");
        Arc::new(Unit {
            main: OpArray {
                oplines: vec![
                    Opline {
                        opcode: Opcode::InitFcallByName,
                        op2_type: OperandKind::Const,
                        op2: 0,
                        out: 0,
                        extended_value: 1,
                        ..Opline::default()
                    },
                    Opline {
                        opcode: Opcode::SendValEx,
                        op1_type: OperandKind::Const,
                        op1: 2,
                        op2: 1,
                        out: 0,
                        ..Opline::default()
                    },
                    Opline {
                        opcode: Opcode::DoFcallByName,
                        out_type: OperandKind::Var,
                        out: 0,
                        ..Opline::default()
                    },
                ]
                .into_boxed_slice(),
                literals: vec![written, lowered, argument].into_boxed_slice(),
                num_tmps: 1,
                cache_size: 8,
                ..OpArray::default()
            },
            ..Unit::default()
        })
    }

    /// Interned `bytes` as a constant, `UNDEF` where the table is full.
    fn interned(strings: &mut php_heap::Strings, bytes: &[u8]) -> php_heap::Zval {
        strings
            .intern(bytes)
            .map_or(php_heap::Zval::UNDEF, php_heap::Zval::string)
    }

    /// `ZEND_INIT_FCALL_BY_NAME` resolves the pair's lowered name into
    /// the frame `INIT_FCALL` builds, `SEND_VAL_EX` places the argument
    /// on it as `SEND_VAL` does and `DO_FCALL_BY_NAME` serves the native
    /// as `DO_ICALL` does, into the `VAR` the opline names. The
    /// compiler is handed the bundle's whole table, so no program
    /// reaches these three with a name the engine resolves until a
    /// program can grow the table; this unit is that program.
    #[test]
    fn a_call_by_name_resolves_the_lowered_constant_and_serves_the_native() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = by_name(&mut strings);
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let init = code.oplines.first().copied().unwrap_or_default();
        let send = code.oplines.get(1).copied().unwrap_or_default();
        let call = code.oplines.get(2).copied().unwrap_or_default();

        assert_eq!(
            init_fcall_by_name::<Nop>(&mut req, frame, code, &init),
            Ok(Stepped::Same(frame))
        );
        assert!(!req.is_pending(), "the lowered constant is the table's key");
        assert_eq!(
            send_val_ex::<Nop>(&mut req, frame, code, &send),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(
            do_fcall_by_name::<Nop>(&mut req, frame, code, &call),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.slot(frame, 0).as_int(), Some(3));
        assert_eq!(req.vm().opline(frame), 3);
    }

    /// `ZEND_INIT_FCALL_BY_NAME` over two passes of one opline: the
    /// first finds its cache slot unprimed, looks the table up once and
    /// writes the callee into the slot, `CACHE_PTR(opline->result.num,
    /// fbc)`; the second reads the callee off the slot and looks
    /// nothing up.
    #[test]
    fn a_call_by_name_looks_the_table_up_once_and_reads_its_slot_after() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = by_name(&mut strings);
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let init = code.oplines.first().copied().unwrap_or_default();
        assert_eq!(
            req.vm().cached(frame, CacheSlot(init.out)),
            Cached::Unprimed
        );

        assert_eq!(
            init_fcall_by_name::<Nop>(&mut req, frame, code, &init),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().lookups, 1, "the first pass looks the table up");
        let strlen = req.function_named(b"strlen");
        assert!(strlen.is_some());
        assert_eq!(
            req.vm().cached(frame, CacheSlot(init.out)),
            Cached::Function(strlen.unwrap_or(Function::PASS)),
            "and primes the slot with the callee"
        );

        assert_eq!(
            init_fcall_by_name::<Nop>(&mut req, frame, code, &init),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().lookups, 1, "the second pass reads the slot");
        assert_eq!(
            popped_callee(&mut req),
            strlen,
            "and pushes the same callee"
        );
        assert_eq!(popped_callee(&mut req), strlen);
    }

    /// The call last pushed off the stack, its callee, the name it held
    /// given up.
    fn popped_callee(req: &mut Request) -> Option<Function> {
        let call = req.vm_mut().pop_call()?;
        if let Some(name) = call.name {
            req.release(Value::string(name));
        }
        Some(call.function)
    }

    /// `ZEND_INIT_NS_FCALL_BY_NAME` over `Foo\StrLen`, `foo\strlen`,
    /// `strlen`: the first pass makes the handler's two lookups, the
    /// namespaced name that misses and the global one that hits, and
    /// primes the slot; the second pass makes neither.
    #[test]
    fn a_namespaced_call_by_name_looks_the_table_up_twice_once() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = Arc::new(Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::InitNsFcallByName,
                    op2_type: OperandKind::Const,
                    op2: 0,
                    out: 8,
                    extended_value: 0,
                    ..Opline::default()
                }]
                .into_boxed_slice(),
                literals: vec![
                    interned(&mut strings, b"Foo\\StrLen"),
                    interned(&mut strings, b"foo\\strlen"),
                    interned(&mut strings, b"strlen"),
                ]
                .into_boxed_slice(),
                cache_size: 16,
                ..OpArray::default()
            },
            ..Unit::default()
        });
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let init = code.oplines.first().copied().unwrap_or_default();
        let strlen = req.function_named(b"strlen");

        assert_eq!(
            init_ns_fcall_by_name::<Nop>(&mut req, frame, code, &init),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().lookups, 2, "foo\\strlen misses, strlen hits");
        assert_eq!(
            req.vm().cached(frame, CacheSlot(8)),
            Cached::Function(strlen.unwrap_or(Function::PASS))
        );
        assert_eq!(req.vm().cached(frame, CacheSlot(0)), Cached::Unprimed);

        assert_eq!(
            init_ns_fcall_by_name::<Nop>(&mut req, frame, code, &init),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().lookups, 2, "the second pass looks nothing up");
        assert_eq!(popped_callee(&mut req), strlen);
        assert_eq!(popped_callee(&mut req), strlen);
    }

    /// A miss primes nothing: php's `zend_undefined_function_helper` is
    /// dispatched to before `CACHE_PTR`, so a second pass over a name
    /// nothing declares looks the table up again and throws again.
    #[test]
    fn a_call_by_name_nothing_declares_primes_no_slot() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = Arc::new(Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::InitFcallByName,
                    op2_type: OperandKind::Const,
                    op2: 0,
                    out: 0,
                    ..Opline::default()
                }]
                .into_boxed_slice(),
                literals: vec![
                    interned(&mut strings, b"NoPe"),
                    interned(&mut strings, b"nope"),
                ]
                .into_boxed_slice(),
                cache_size: 8,
                ..OpArray::default()
            },
            ..Unit::default()
        });
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let init = code.oplines.first().copied().unwrap_or_default();
        for pass in 1..=2 {
            assert_eq!(
                init_fcall_by_name::<Nop>(&mut req, frame, code, &init),
                Ok(Stepped::Threw(frame))
            );
            assert_eq!(req.vm().lookups, pass);
            assert_eq!(req.vm().cached(frame, CacheSlot(0)), Cached::Unprimed);
            assert!(matches!(req.uncaught(), Err(Fatal::Error(_))));
        }
    }

    /// A unit whose main op array is one `CATCH` of the class the pair
    /// `written`/`lowered` names, the last clause of its `try`
    /// (`ZEND_LAST_CATCH`), naming no variable, its cache slot at byte
    /// zero and its jump to the `NOP` after it.
    fn catching(strings: &mut php_heap::Strings, written: &[u8], lowered: &[u8]) -> Arc<Unit> {
        Arc::new(Unit {
            main: OpArray {
                oplines: vec![
                    Opline {
                        opcode: Opcode::Catch,
                        op1_type: OperandKind::Const,
                        op1: 0,
                        op2: 1,
                        out_type: OperandKind::Unused,
                        extended_value: LAST_CATCH,
                        ..Opline::default()
                    },
                    Opline::default(),
                ]
                .into_boxed_slice(),
                literals: vec![interned(strings, written), interned(strings, lowered)]
                    .into_boxed_slice(),
                cache_size: 8,
                ..OpArray::default()
            },
            ..Unit::default()
        })
    }

    /// `ZEND_CATCH` over two passes of one opline, which is what a
    /// `catch` inside a loop is: the first finds its cache slot
    /// unprimed, looks the class up once and writes the clause's
    /// class into the slot the `extended_value` carries beside
    /// `ZEND_LAST_CATCH`, `CACHE_PTR(opline->extended_value &
    /// ~ZEND_LAST_CATCH, catch_ce)`; the second reads it back and looks
    /// nothing up.
    #[test]
    fn a_catch_looks_the_class_up_once_and_reads_its_slot_after() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = catching(&mut strings, b"Error", b"error");
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let clause = code.oplines.first().copied().unwrap_or_default();
        let error = req.class_named(b"error");
        assert!(error.is_some(), "the engine declares Error");
        assert_eq!(req.vm().cached(frame, CacheSlot(0)), Cached::Unprimed);

        for pass in 1..=2 {
            let thrown = crate::api::raising::throw(&mut req, "Error", "e");
            req.leave_pending(&thrown);
            req.vm_mut().jump(frame, 0);
            assert_eq!(
                catch::<Nop>(&mut req, frame, code, &clause),
                Ok(Stepped::Same(frame))
            );
            assert!(
                req.pending().is_none(),
                "the clause caught it on pass {pass}"
            );
            assert_eq!(req.vm().lookups, 1, "one lookup over {pass} pass(es)");
            assert!(
                matches!(req.vm().cached(frame, CacheSlot(0)), Cached::Class(class) if Some(class) == error),
                "the clause's class is in the slot after pass {pass}"
            );
        }
    }

    /// A clause naming a class nothing declares primes nothing, as
    /// php's caches the null `zend_lookup_class_ex` gave it and reads it
    /// back as unprimed: the table is looked up on every pass, and the
    /// throwable the clause does not match is left pending by the
    /// `ZEND_LAST_CATCH` for the unwinder.
    #[test]
    fn a_catch_of_no_class_primes_no_slot() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = catching(&mut strings, b"NoPe", b"nope");
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let clause = code.oplines.first().copied().unwrap_or_default();
        let thrown = crate::api::raising::throw(&mut req, "Error", "e");
        req.leave_pending(&thrown);

        for pass in 1..=2 {
            req.vm_mut().jump(frame, 0);
            assert_eq!(
                catch::<Nop>(&mut req, frame, code, &clause),
                Ok(Stepped::Threw(frame))
            );
            assert_eq!(req.vm().lookups, pass, "looked up again on pass {pass}");
            assert_eq!(req.vm().cached(frame, CacheSlot(0)), Cached::Unprimed);
        }
        assert!(matches!(req.uncaught(), Err(Fatal::Error(_))));
    }

    /// The other three handlers that resolve a class off a constant
    /// pair read the slot their opline names, `NEW`'s in `op2` and
    /// `INSTANCEOF`'s and `FETCH_CLASS_CONSTANT`'s in `extended_value`:
    /// a slot primed with `stdClass` answers all three over a pair that
    /// names `NoPe`, which nothing declares, so a handler that looked
    /// the table up would throw `Class "NoPe" not found` instead. The
    /// class constant is one `stdClass` has not, so what it throws
    /// names the class the slot held.
    #[test]
    fn new_instanceof_and_a_class_constant_read_the_class_off_their_slot() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = Arc::new(Unit {
            main: OpArray {
                oplines: vec![
                    Opline {
                        opcode: Opcode::New,
                        op1_type: OperandKind::Const,
                        op1: 0,
                        op2: 0,
                        out_type: OperandKind::Var,
                        out: 0,
                        ..Opline::default()
                    },
                    Opline {
                        opcode: Opcode::DoFcall,
                        ..Opline::default()
                    },
                    Opline {
                        opcode: Opcode::Instanceof,
                        op1_type: OperandKind::Var,
                        op1: 0,
                        op2_type: OperandKind::Const,
                        op2: 0,
                        out_type: OperandKind::Tmp,
                        out: 1,
                        extended_value: 8,
                        ..Opline::default()
                    },
                    Opline {
                        opcode: Opcode::FetchClassConstant,
                        op1_type: OperandKind::Const,
                        op1: 0,
                        op2_type: OperandKind::Const,
                        op2: 2,
                        out_type: OperandKind::Tmp,
                        out: 2,
                        extended_value: 16,
                        ..Opline::default()
                    },
                ]
                .into_boxed_slice(),
                literals: vec![
                    interned(&mut strings, b"NoPe"),
                    interned(&mut strings, b"nope"),
                    interned(&mut strings, b"NOPE"),
                ]
                .into_boxed_slice(),
                num_tmps: 3,
                cache_size: 24,
                ..OpArray::default()
            },
            ..Unit::default()
        });
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let named = req.class_named(b"stdclass");
        assert!(named.is_some(), "the engine declares stdClass");
        let class = named.unwrap_or(ClassId(u32::MAX));
        for slot in [0, 8, 16] {
            req.vm_mut()
                .prime(frame, CacheSlot(slot), Cached::Class(class));
        }

        let new_op = code.oplines.first().copied().unwrap_or_default();
        assert_eq!(
            new::<Nop>(&mut req, frame, code, &new_op),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(
            req.slot(frame, 0).as_object().map(PhpObject::class),
            Some(class),
            "NEW took its class off op2's slot"
        );

        let instanceof_op = code.oplines.get(2).copied().unwrap_or_default();
        assert_eq!(
            instanceof::<Nop>(&mut req, frame, code, &instanceof_op),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(
            req.slot(frame, 1).as_bool(),
            Some(true),
            "INSTANCEOF took its class off extended_value's slot"
        );

        let constant_op = code.oplines.get(3).copied().unwrap_or_default();
        assert_eq!(
            fetch_class_constant::<Nop>(&mut req, frame, code, &constant_op),
            Ok(Stepped::Threw(frame))
        );
        assert!(
            matches!(req.uncaught(), Err(Fatal::Error(message)) if message.contains("Undefined constant stdClass::NOPE")),
            "FETCH_CLASS_CONSTANT took its class off extended_value's slot"
        );
        assert_eq!(req.vm().lookups, 0, "no handler looked the table up");
    }

    /// A unit whose main op array is `JMP_FRAMELESS` over `name`, its
    /// jump to opline 2, its cache slot at byte 0, then two `NOP`s.
    fn jmp_frameless_over(strings: &mut php_heap::Strings, name: &[u8]) -> Arc<Unit> {
        Arc::new(Unit {
            main: OpArray {
                oplines: vec![
                    Opline {
                        opcode: Opcode::JmpFrameless,
                        op1_type: OperandKind::Const,
                        op1: 0,
                        op2: 2,
                        extended_value: 0,
                        ..Opline::default()
                    },
                    Opline::default(),
                    Opline::default(),
                ]
                .into_boxed_slice(),
                literals: vec![interned(strings, name)].into_boxed_slice(),
                cache_size: 8,
                ..OpArray::default()
            },
            ..Unit::default()
        })
    }

    /// `ZEND_JMP_FRAMELESS` over two passes of one opline: the first
    /// finds `ZEND_JMP_FL_UNPRIMED`, looks the namespaced name up once
    /// and primes `ZEND_JMP_FL_HIT` where the table has it not, so the
    /// jump is taken; the second reads `HIT` off the slot and jumps with
    /// no lookup.
    #[test]
    fn a_frameless_jump_primes_a_hit_once_and_jumps_on_it_after() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = jmp_frameless_over(&mut strings, b"foo\\substr");
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let jmp = code.oplines.first().copied().unwrap_or_default();
        assert_eq!(req.vm().cached(frame, CacheSlot(0)), Cached::Unprimed);

        for pass in 1..=2 {
            req.vm_mut().jump(frame, 0);
            assert_eq!(
                jmp_frameless::<Nop>(&mut req, frame, code, &jmp),
                Ok(Stepped::Same(frame))
            );
            assert_eq!(
                req.vm().opline(frame),
                2,
                "the jump is taken on pass {pass}"
            );
            assert_eq!(req.vm().lookups, 1, "one lookup over {pass} pass(es)");
            assert_eq!(req.vm().cached(frame, CacheSlot(0)), Cached::Hit);
        }
    }

    /// The other answer: a name the table has primes `ZEND_JMP_FL_MISS`
    /// on the first pass, and the jump is not taken on either.
    #[test]
    fn a_frameless_jump_primes_a_miss_once_and_falls_through_on_it_after() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = jmp_frameless_over(&mut strings, b"strlen");
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let jmp = code.oplines.first().copied().unwrap_or_default();

        for pass in 1..=2 {
            req.vm_mut().jump(frame, 0);
            assert_eq!(
                jmp_frameless::<Nop>(&mut req, frame, code, &jmp),
                Ok(Stepped::Same(frame))
            );
            assert_eq!(
                req.vm().opline(frame),
                1,
                "on to the call by name on pass {pass}"
            );
            assert_eq!(req.vm().lookups, 1);
            assert_eq!(req.vm().cached(frame, CacheSlot(0)), Cached::Miss);
        }
    }

    /// An extension of one native, `seven()`, with a frameless entry of
    /// arity zero, which no native of php's declares: what
    /// `FRAMELESS_ICALL_0` needs to be reached at all.
    fn with_a_nullary_frameless_native() -> Engine {
        use crate::api::abi::ExtensionBuilder;
        use php_unit::decl::{FunctionDecl, Name};
        fn seven(_: &mut Request) -> Result<Value, Thrown> {
            Ok(Value::int(7))
        }
        let decl = FunctionDecl {
            name: Name::from("seven"),
            parameters: Vec::new(),
            return_type: None,
            returns_by_ref: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name::default(),
            line: 0,
            end_line: 0,
            frameless: vec![0],
        };
        let extension = ExtensionBuilder::default()
            .functions(vec![(decl, seven as crate::api::abi::Entry)])
            .build("nullary");
        Engine::builder().extensions(&[extension]).build()
    }

    /// `ZEND_FRAMELESS_ICALL_0` serves the native at the table's row
    /// over no argument, into the temporary, and steps one opline; the
    /// row is the native's place in `zend_flf_functions`, which the
    /// engine built from the bundle in registration order. No program
    /// reaches this opcode under the oracle, since no native of php
    /// 8.5.9 declares a nullary entry; this unit is that program.
    #[test]
    fn a_nullary_frameless_call_serves_the_native_into_its_temporary() {
        let engine = with_a_nullary_frameless_native();
        let row = engine
            .shared
            .frameless
            .iter()
            .position(|entry| entry.arity == 0)
            .and_then(|at| u32::try_from(at).ok())
            .unwrap_or(u32::MAX);
        assert_eq!(
            engine.frameless().get(row as usize).map(|n| n.0.as_ref()),
            Some(&b"seven"[..])
        );
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = Arc::new(Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::FramelessIcall0,
                    out_type: OperandKind::Tmp,
                    out: 0,
                    extended_value: row,
                    ..Opline::default()
                }]
                .into_boxed_slice(),
                num_tmps: 1,
                ..OpArray::default()
            },
            ..Unit::default()
        });
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let call = code.oplines.first().copied().unwrap_or_default();
        assert_eq!(
            frameless_icall_0::<Nop>(&mut req, frame, code, &call),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.slot(frame, 0).as_int(), Some(7));
        assert_eq!(req.vm().opline(frame), 1);
        assert!(
            req.vm().serving_call().is_none(),
            "the call is popped with the native"
        );
    }

    /// A row past the engine's table is the internal-error fatal that
    /// names it, never a read past the end.
    #[test]
    fn a_row_the_table_has_not_is_an_internal_error() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = Arc::new(Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::FramelessIcall0,
                    out_type: OperandKind::Tmp,
                    out: 0,
                    extended_value: 1_000_000,
                    ..Opline::default()
                }]
                .into_boxed_slice(),
                num_tmps: 1,
                ..OpArray::default()
            },
            ..Unit::default()
        });
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let call = code.oplines.first().copied().unwrap_or_default();
        assert!(matches!(
            frameless_icall_0::<Nop>(&mut req, frame, code, &call),
            Err(Fatal::Error(message)) if message.starts_with("InternalError: ZEND_FRAMELESS_ICALL_0 indexes frameless entry 1000000")
        ));
    }

    /// `zend_undefined_function_helper` names the constant as written
    /// (`RT_CONSTANT(opline, opline->op2)`) where the lookup read the
    /// lowered one after it, so the `Error` for `NoPe()` says `NoPe`;
    /// the served witness beside [`init_fcall_by_name`] pins the whole
    /// line against the oracle.
    #[test]
    fn a_call_by_name_of_no_function_raises_over_the_name_as_written() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = by_name(&mut strings);
        let mut oplines = unit.main.oplines.to_vec();
        let written = strings
            .intern(b"NoPe")
            .map_or(php_heap::Zval::UNDEF, php_heap::Zval::string);
        let lowered = strings
            .intern(b"nope")
            .map_or(php_heap::Zval::UNDEF, php_heap::Zval::string);
        if let Some(init) = oplines.first_mut() {
            init.op2 = 3;
        }
        let unit = Arc::new(Unit {
            main: OpArray {
                oplines: oplines.into_boxed_slice(),
                literals: unit
                    .main
                    .literals
                    .iter()
                    .copied()
                    .chain([written, lowered])
                    .collect(),
                ..unit.main.clone()
            },
            ..Unit::default()
        });
        let frame = req.push_frame(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let init = code.oplines.first().copied().unwrap_or_default();

        assert_eq!(
            init_fcall_by_name::<Nop>(&mut req, frame, code, &init),
            Ok(Stepped::Threw(frame))
        );
        assert!(req.is_pending());
        let message = match req.uncaught() {
            Err(Fatal::Error(message)) => message,
            other => format!("{other:?}"),
        };
        assert!(
            message.starts_with("Uncaught Error: Call to undefined function NoPe() in "),
            "{message}"
        );
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

    /// #367, the first acceptance: a `foreach` by value over a plain
    /// object walks the properties the scope may see, the private one
    /// stepped over from outside the class, the key the name as
    /// written. The witness beside [`fe_reset_r`] pins the same line
    /// against the oracle; this holds the engine to it.
    #[test]
    fn a_foreach_over_an_object_walks_the_properties_the_scope_may_see() {
        let served = Engine::builder().build().serve_capture(
            "<?php class P { public $a = 1; private $b = 2; public $c = 3; } foreach (new P as $k => $v) echo \"$k=$v \";",
            crate::test::Capture::default(),
        );
        assert_eq!(served.stdout, b"a=1 c=3 ");
        assert_eq!(served.fatal, None);
    }

    /// The same loop from inside the class sees every property, as
    /// `zend_check_property_access` lets the declaring scope through.
    #[test]
    fn a_foreach_over_this_walks_the_private_properties_too() {
        let served = Engine::builder().build().serve_capture(
            "<?php class C { public $n = 1; protected $p = 2; public $s = 'x'; private $q = 3; public function all() { foreach ($this as $k => $v) echo \"$k=$v \"; } } $c = new C; foreach ($c as $k => $v) echo \"$k=$v \"; $c->all();",
            crate::test::Capture::default(),
        );
        assert_eq!(served.stdout, b"n=1 s=x n=1 p=2 s=x q=3 ");
        assert_eq!(served.fatal, None);
    }

    /// #367, the second acceptance: an object of no properties runs the
    /// body no time, and a property declared with no default is the
    /// `null` `default_properties_table` put there, walked like any
    /// other.
    #[test]
    fn a_foreach_over_an_object_of_no_properties_runs_its_body_no_time() {
        let served = Engine::builder().build().serve_capture(
            "<?php foreach (new stdClass as $v) echo 'no'; class U { public $kept = 2; public $null; } foreach (new U as $k => $v) echo $k, ' ';",
            crate::test::Capture::default(),
        );
        assert_eq!(served.stdout, b"kept null ");
        assert_eq!(served.fatal, None);
    }

    /// The walk itself: every property `Exception` declares is
    /// protected or private, so the global scope may see none of them
    /// and `zend_fe_fetch_object_helper` steps over the whole table,
    /// which is the jump past the loop.
    #[test]
    fn the_walk_over_an_object_of_properties_the_scope_may_not_see_finds_nothing() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let named = req.class_named(b"exception");
        assert!(named.is_some(), "the engine declares Exception");
        let class = named.unwrap_or(ClassId(u32::MAX));
        let made = crate::api::call::new_object(&mut req, class);
        assert!(made.is_ok(), "Exception is instantiable");
        if let Ok(object) = made {
            assert!(
                matches!(req.object_property_from(object, 0), Ok(None)),
                "the global scope sees none of Exception's properties"
            );
            req.release(object.as_value());
        }
    }
}
