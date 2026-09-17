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
//! (#281).

// Every handler is generic over the tier and hands back the same edge, so
// the generated dispatch calls them all the same way; the ones that raise a
// safepoint or a fatal are the ones that name the tier or return an `Err`.
#![allow(clippy::extra_unused_type_parameters, clippy::unnecessary_wraps)]

use php_heap::{Fatal, FrameRef};
use php_unit::{ConstIdx, OpArray, OperandKind, Opline};

use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{TypeTag, Value};
use crate::exec::{self, CallStage, CallState};
use crate::ops::Switched;
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
/// its lowered name in `op2` and the number of arguments in
/// `extended_value`. A name nothing is declared under is php's `Error`,
/// over the same constant.
pub(crate) fn init_fcall<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let name = ConstIdx(op.op2);
    init_call(req, frame, code, op, name, name)
}

/// `ZEND_INIT_FCALL_BY_NAME`: the call frame for a callee php's compiler
/// found no function under, resolved here instead. `op2` names the
/// constant pair `zend_add_func_name_literal` laid down, the name as
/// written and then its lowered form: the lookup is by the second
/// (`Z_STR_P(function_name+1)`) and the `Error` for a name nothing
/// declares says the first (`zend_undefined_function_helper`), which is
/// why `NoPe()` is named as written. The frame it pushes is the one
/// `INIT_FCALL` pushes, so the `SEND_*_EX` and `DO_FCALL_BY_NAME` after
/// it are served the same way.
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
    init_call(req, frame, code, op, lowered, written)
}

/// The body the two `INIT_*` handlers share: the callee looked up under
/// the constant at `lookup`, the `Error` for a name nothing declares
/// naming the constant at `named`, and the call frame pushed with the
/// number of arguments `extended_value` says.
fn init_call(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    lookup: ConstIdx,
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
    let lookup = exec::constant(code, lookup);
    let function = req.function_named(lookup.as_str_bytes().unwrap_or(name.as_bytes()));
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

/// `ZEND_SEND_VAL_EX`: `SEND_VAL` for a callee the compiler knew nothing
/// of, which reads the parameter's by-reference bit off the callee the
/// `INIT` resolved and raises `Cannot pass parameter %d by reference`
/// where it is set. By value here, since nothing declares a by-reference
/// parameter yet, so the body is `SEND_VAL`'s. The engine's compiler is
/// handed the bundle's whole function table, so a program reaches this
/// opline only through a table the program grows, which nothing does
/// yet; the test beside the handlers holds it meanwhile, and the
/// witness pins that the `INIT` before it has already raised.
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
    send(req, frame, code, op)
}

/// `ZEND_SEND_VAR_EX`: `SEND_VAR` for a callee the compiler knew nothing
/// of, which reads the parameter's by-reference bit off the callee the
/// `INIT` resolved and sends a reference where it is set. By value here,
/// since nothing declares a by-reference parameter yet, so the body is
/// `SEND_VAR`'s; reached as [`send_val_ex`] is.
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
    let class = match class_operand(req, code, op.op1_type, op.op1) {
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
    let class = match class_operand(req, code, op.op1_type, op.op1) {
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

/// `ZEND_FETCH_THIS` (`Z/Zend/zend_vm_def.h:9214`): `$this` outside a
/// method's body. `EX(This)` is an object only on a frame a method call
/// pushed, and no user frame is one until a method body is compiled,
/// so the frame here holds no receiver and this is php's `Error`
/// (`zend_this_not_in_object_context_helper`), the temporary it would
/// write left undefined (`UNDEF_RESULT`).
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
    _: &OpArray,
    _: &Opline,
) -> Edge {
    let thrown = crate::api::raising::throw(req, "Error", "Using $this when not in object context");
    threw(req, frame, &thrown)
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
    if op.op1_type == OperandKind::Unused {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_FETCH_OBJ_R over $this, which no frame here carries",
        )));
    }
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
            let lowered = exec::constant(code, ConstIdx(op.op2.saturating_add(1)));
            lowered
                .as_str_bytes()
                .and_then(|lowered| req.class_named(lowered))
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

/// `zend_fetch_class_by_name` with `ZEND_FETCH_CLASS_EXCEPTION` over a
/// class's constant pair: the class under the lowered name in the slot
/// after `at`, and php's `Error` naming the one as written where
/// nothing declares it. A class named as the program serves is not served yet.
fn class_operand(
    req: &mut Request,
    code: &OpArray,
    kind: OperandKind,
    at: u32,
) -> Result<crate::api::values::ClassId, Thrown> {
    if kind != OperandKind::Const {
        return Err(crate::api::raising::fatal(
            req,
            "InternalError: a class named at run time is not served",
        ));
    }
    let lowered = exec::constant(code, ConstIdx(at.saturating_add(1)));
    if let Some(class) = lowered
        .as_str_bytes()
        .and_then(|lowered| req.class_named(lowered))
    {
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

/// The body the two `DO_*CALL` handlers share, `opcode` naming the one
/// that found no call frame under it.
fn do_call(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline, opcode: &str) -> Edge {
    let Some(function) = req.vm_mut().activate_call() else {
        return Err(Fatal::Error(format!(
            "InternalError: {opcode} with no call frame under it"
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

/// `ZEND_INIT_NS_FCALL_BY_NAME`: the call frame for an unqualified name
/// written in a namespace, which php's compiler resolves as the program
/// serves (`zend_compile_ns_call`). `op2` names the three constants
/// `zend_add_ns_func_name_literal` laid down: the name as written with
/// the namespace before it, its lowered form, and the lowered name
/// alone; the callee is looked up under the second and then the third
/// (`Z/Zend/zend_vm_def.h`, the handler's two `zend_hash_find_known_hash`),
/// and the `Error` for a name nothing declares says the first. The
/// frame it pushes is the one `INIT_FCALL` pushes, so the `SEND_*_EX`
/// and `DO_FCALL_BY_NAME` after it are served the same way.
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
    let lookup = if exec::constant(code, lowered)
        .as_str_bytes()
        .is_some_and(|name| req.function_named(name).is_some())
    {
        lowered
    } else {
        unqualified
    };
    init_call(req, frame, code, op, lookup, written)
}

/// `ZEND_JMP_FRAMELESS` (`Z/Zend/zend_vm_def.h:9743`): the jump
/// `zend_compile_ns_call` puts before the call by name of an unqualified
/// name in a namespace whose global function has a frameless entry.
/// `op1` is the lowered name with the namespace before it,
/// `foo\substr`; where the function table has that name the call by
/// name is what follows and the jump is not taken, `ZEND_JMP_FL_MISS`,
/// and where it has not, the name is the global function's and the jump
/// lands on the `FRAMELESS_ICALL_*` over it, `ZEND_JMP_FL_HIT`. php
/// primes the answer into the opline's cache slot the first time and
/// reads it after; the table is looked up each time here, which reads
/// the same while nothing declares a function as the program serves,
/// the way `INIT_FCALL_BY_NAME` resolves at every `INIT` here.
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
    let name = exec::constant(code, ConstIdx(op.op1));
    let hit = name
        .as_str_bytes()
        .is_some_and(|name| req.function_named(name).is_none());
    if hit {
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

/// `ZEND_CAST`: [`crate::ops::cast`] over the operand to the type
/// `extended_value` names, into the temporary; the operand freed after
/// it. A type this engine converts to nothing yet, `(array)` and
/// `(object)`, is the internal-error fatal naming the opcode, as an
/// opcode with no handler is.
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

    use php_heap::Intern as _;
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
                ..OpArray::default()
            },
            ..Unit::default()
        })
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
                    frameless: Vec::new(),
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
            .classes(vec![(destructible(b"Destructible"), Vec::new())])
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
