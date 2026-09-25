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
//! frame with a receiver, and `ZEND_FETCH_CLASS_CONSTANT`; and the three
//! a `@` compiles to (#354): `ZEND_BEGIN_SILENCE` and `ZEND_END_SILENCE`
//! around the expression, and `ZEND_FETCH_R` by a constant name, which
//! is how a variable under `@` is read inside the span.

// Every handler is generic over the tier and hands back the same edge, so
// the generated dispatch calls them all the same way; the ones that raise a
// safepoint or a fatal are the ones that name the tier or return an `Err`.
#![allow(clippy::extra_unused_type_parameters, clippy::unnecessary_wraps)]

use php_heap::{Fatal, FrameRef};
use php_unit::{CacheSlot, ConstIdx, OpArray, OperandKind, Opline};

use crate::api::abi::{PropertyRead, PropertyTest};
use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::request::{Opened, Request};
use crate::api::values::{
    ClosureInfo, PhpArray, PhpObject, PhpRef, PhpString, TypeTag, Value, ValueRef,
};
use crate::exec::{
    self, Cached, CallStage, CallState, E_FATAL_ERRORS, cleanup_live_vars,
    cleanup_unfinished_calls, free_carried_value, has_only_fatal_errors, restore_silenced,
};
use crate::ops::{Offset, Switched};
use crate::tier::{SafepointIdx, Site, Stepped, Tier, continued};
use php_unit::Opcode;

/// What one handler did to the frame chain, and the fatal that ends the
/// request instead.
pub(crate) type Edge = Result<Stepped, Fatal>;

/// `ZEND_BIND_REF` (`Z/Zend/zend_compile.h:1194`): the bit a
/// `BIND_LEXICAL`'s and a `BIND_STATIC`'s `extended_value` carries for
/// a binding by reference, the slot standing above the three of them.
const BIND_REF: u32 = 1;
/// `ZEND_BIND_IMPLICIT`: an arrow function's capture, which raises no
/// warning for a variable the scope has not.
const BIND_IMPLICIT: u32 = 2;

/// `IS_CONSTANT_UNQUALIFIED_IN_NAMESPACE` (`Z/Zend/zend_types.h:637`),
/// the one bit a `FETCH_CONSTANT`'s `op1` carries: the name was
/// written unqualified inside a namespace, so the global spelling
/// stands in the constant slot after the namespaced one.
const UNQUALIFIED_IN_NAMESPACE: u32 = 0x800;

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

/// `ZEND_DECLARE_CONST` (`Z/Zend/zend_vm_def.h:8269`), the opline
/// `zend_compile_const_decl` leaves for each declarator of a `const`
/// statement: the name in `op1`, the value in `op2`, and
/// `zend_register_constant` over them, which is the table `define()`
/// writes into and `defined()` reads (`Z/Zend/zend_constants.c:511`).
/// The name of a constant declared inside a namespace is stored with
/// its namespace lowered and its own part as written, which is the same
/// `strrchr` the native does, so `A\K` and `a\K` are the one name and
/// `A\k` is another.
///
/// A name that stands already is no fatal: php warns and leaves the
/// constant it had, `Constant %s already defined, this will be an error
/// in PHP 9`, the same sentence `define()` raises from the same place.
/// `__COMPILER_HALT_OFFSET__` and the three special constants take that
/// path too, though the compiler refuses the latter three before the
/// opline is reached.
///
/// ```php
/// const K = 1;
/// const L = 2, M = 3;
/// var_dump(defined('K'), defined('L'), defined('M'), defined('NOPE'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
///
/// ```php
/// const K = 1;
/// const K = 2;
/// var_dump(defined('K'));
/// ```
/// ```expect
/// Warning: Constant K already defined, this will be an error in PHP 9 in %s on line 3
/// bool(true)
/// ```
///
/// ```php
/// define('K', 1);
/// const K = 2;
/// ```
/// ```expect
/// Warning: Constant K already defined, this will be an error in PHP 9 in %s on line 3
/// ```
pub(crate) fn declare_const<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(written) = exec::constant(code, ConstIdx(op.op1))
        .as_str_bytes()
        .map(<[u8]>::to_vec)
    else {
        return Err(Fatal::Error(String::from(
            "InternalError: a DECLARE_CONST whose op1 is no constant name",
        )));
    };
    let value = exec::constant(code, ConstIdx(op.op2));
    // `zend_register_constant`'s key: the namespace lowered and the
    // name itself as written.
    let mut key = written;
    if let Some(slash) = key.iter().rposition(|b| *b == b'\\') {
        key[..slash].make_ascii_lowercase();
    }
    req.addref(&value);
    match req.define_constant(&key, value) {
        Ok(true) => Ok(advance(req, frame)),
        Ok(false) => {
            req.release(value);
            crate::api::raising::warning(
                req,
                &format!(
                    "Constant {} already defined, this will be an error in PHP 9",
                    String::from_utf8_lossy(&key)
                ),
            );
            Ok(advance(req, frame))
        }
        Err(thrown) => {
            req.release(value);
            req.leave_pending(&thrown);
            Ok(Stepped::Threw(frame))
        }
    }
}

/// `ZEND_DECLARE_LAMBDA_FUNCTION` (`Z/Zend/zend_vm_def.h:8343`):
/// `zend_create_closure` over the op array the frame's
/// `dynamic_func_defs` holds at `op2`, into the temporary `result`
/// names. The scope is the enclosing op array's, the class a method
/// whose body the closure stands in was declared in; the called scope
/// is the receiver's class where the frame has one and the frame's own
/// otherwise; and the receiver is bound unless the closure was written
/// `static` or the frame has none. The closure carries its own table
/// for the variables the `BIND_LEXICAL` oplines after it bind
/// ([`ClosureInfo::bound`]), one slot per name of the op array's
/// `bound_vars`, as `zend_create_closure_ex` gives every closure a
/// `zend_array_dup` of the op array's.
///
/// ```php
/// $f = function ($a) { return $a * 2; };
/// var_dump($f(4), $f->__invoke(5), $f instanceof Closure);
/// ```
/// ```expect
/// int(8)
/// int(10)
/// bool(true)
/// ```
///
/// A closure written in a method is bound to the receiver, and one
/// written `static` is not.
///
/// ```php
/// class C {
///     public $p = 7;
///     function m() { return function () { return $this->p; }; }
///     function s() { return static function () { return $this->p; }; }
/// }
/// $c = new C;
/// $bound = $c->m();
/// var_dump($bound());
/// $free = $c->s();
/// $free();
/// ```
/// ```expect exit=255
/// int(7)
///
/// Fatal error: Uncaught Error: Using $this when not in object context in %s:5
/// Stack trace:
/// #0 %s(11): C::{closure:C::s():5}()
/// #1 {main}
///   thrown in %s on line 5
/// ```
/// What a closure leaves behind is nothing: the object, the variables
/// it bound and the frame its call pushed are given up when the last
/// owner lets go, so the heap stands where it stood.
///
/// ```php
/// $before = memory_get_usage();
/// $i = 0;
/// while ($i < 3) {
///     $f = function ($a) use ($i) { return $a + $i; };
///     $f(1);
///     $i = $i + 1;
/// }
/// $f = null;
/// var_dump(memory_get_usage() - $before);
/// ```
/// ```expect
/// int(0)
/// ```
pub(crate) fn declare_lambda_function<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(serving) = req.vm().serving(frame) else {
        return Err(Fatal::Error(String::from(
            "InternalError: a DECLARE_LAMBDA_FUNCTION on a frame serving no op array",
        )));
    };
    let Some(at) = code.dynamic_func_defs.get(op.op2 as usize).copied() else {
        return Err(Fatal::Error(String::from(
            "InternalError: a DECLARE_LAMBDA_FUNCTION whose op2 names no dynamic function definition",
        )));
    };
    let Some((_, body)) = serving.unit.dynamic.get(at as usize) else {
        return Err(Fatal::Error(String::from(
            "InternalError: a DECLARE_LAMBDA_FUNCTION naming a definition the unit has not",
        )));
    };
    let (slots, no_receiver) = (body.bound_vars.len(), body.no_receiver);
    let Some(function) = req.closure_function(&serving.unit, at) else {
        return Err(Fatal::Error(String::from(
            "InternalError: a DECLARE_LAMBDA_FUNCTION naming a definition the unit has not",
        )));
    };
    let scope = req.frame_scope(frame);
    let this = req
        .vm()
        .this(frame)
        .filter(|_| !no_receiver && scope.is_some());
    let closure = ClosureInfo {
        function,
        this,
        scope,
        bound: vec![Value::undef(); slots],
        fake: false,
    };
    let Some(class) = req.class_named(b"closure") else {
        return Err(Fatal::Error(String::from(
            "InternalError: a DECLARE_LAMBDA_FUNCTION with no Closure class declared",
        )));
    };
    // `ZVAL_OBJ_COPY(&closure->this_ptr, ...)`: the receiver the
    // closure holds is one more owner, given back by
    // `zend_closure_free_storage`.
    if let Some(this) = this {
        req.addref(&this.as_value());
    }
    let made = match req.new_object_with(class, closure) {
        Ok(made) => made,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, made.as_value());
    req.release(previous);
    Ok(advance(req, frame))
}

/// `ZVAL_MAKE_REF_EX` over a CV (`Z/Zend/zend_vm_def.h:9112`, the
/// by-reference arm of `BIND_LEXICAL`, and `zend_assign_to_variable_ref`
/// under `ASSIGN_REF`): the cell the CV holds, or one made over what it
/// holds and written back into it, the owner the slot had moving into
/// the cell. The `Err` is the memory limit refusing the cell.
fn cv_reference(
    req: &mut Request,
    frame: FrameRef,
    at: u32,
) -> Result<crate::api::values::PhpRef, Thrown> {
    let held = req.slot(frame, at);
    if let Some(cell) = held.as_php_ref() {
        return Ok(*cell);
    }
    let cell = req.reference(held.undef_as_null())?;
    // The owner the slot had of the value is the cell's now.
    let _moved = req.set_slot(frame, at, cell.as_value());
    Ok(cell)
}

/// `ZEND_BIND_LEXICAL` (`Z/Zend/zend_vm_def.h:9101`): one `use`
/// variable of the closure the temporary in `op1` holds, from the CV in
/// `op2` into the slot `extended_value` names of the closure's own
/// table (`zend_closure_bind_var_ex`). By value the variable is
/// dereferenced and copied, one more owner; by reference,
/// `ZEND_BIND_REF`, the CV is made a reference where it is not one
/// already and the closure holds that cell, so a write on either side
/// is seen by the other. A variable the scope has not is the undefined
/// warning, except under `ZEND_BIND_IMPLICIT`, an arrow function's
/// capture, which php raises nothing for.
///
/// ```php
/// $b = 1;
/// $by_value = function () use ($b) { return $b; };
/// $by_ref = function () use (&$b) { return $b; };
/// $b = 2;
/// var_dump($by_value(), $by_ref());
/// ```
/// ```expect
/// int(1)
/// int(2)
/// ```
///
/// A closure's own write through a by-reference binding is the
/// enclosing scope's too, and a by-value one is the closure's alone.
///
/// ```php
/// $n = 1;
/// $bump = function () use (&$n) { $n = $n + 1; };
/// $copy = function () use ($n) { return $n; };
/// $bump(); $bump();
/// var_dump($n, $copy());
/// ```
/// ```expect
/// int(3)
/// int(1)
/// ```
///
/// An explicit `use` of a variable nothing has written warns where the
/// declaration stands, and binds the `null` the warning put in its
/// place; an arrow function's implicit capture of one warns nothing
/// there and binds the undefined value, so the warning is the body's
/// own read of it, at the body's line.
///
/// ```php
/// $f = function () use ($gone) { return $gone; };
/// $g = fn () => $missing;
/// var_dump($f(), $g());
/// ```
/// ```expect
/// Warning: Undefined variable $gone in %s on line 2
///
/// Warning: Undefined variable $missing in %s on line 3
/// NULL
/// NULL
/// ```
pub(crate) fn bind_lexical<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let closure = exec::operand(req, frame, code, op.op1_type, op.op1);
    let Some(object) = closure.as_object().copied() else {
        return Err(Fatal::Error(String::from(
            "InternalError: a BIND_LEXICAL over what is no Closure",
        )));
    };
    let slot = (op.extended_value >> 3) as usize;
    let by_ref = op.extended_value & BIND_REF != 0;
    let bound = if by_ref {
        let cell = match cv_reference(req, frame, op.op2) {
            Ok(cell) => cell,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        let bound = cell.as_value();
        req.addref(&bound);
        bound
    } else {
        // `ZVAL_UNDEFINED_OP2` unless `ZEND_BIND_IMPLICIT`, then
        // `ZVAL_DEREF` and `Z_TRY_ADDREF_P`.
        let held = if op.extended_value & BIND_IMPLICIT == 0 {
            exec::operand(req, frame, code, OperandKind::Cv, op.op2)
        } else {
            // `GET_OP2_ZVAL_PTR_UNDEF` and no `ZVAL_UNDEFINED_OP2`: an
            // implicit capture binds the undefined value as it stands,
            // so the warning is the body's own read of it.
            req.slot(frame, op.op2)
        };
        let held = held.deref();
        req.addref(&held);
        held
    };
    let Some(payload) = object.native_mut::<ClosureInfo>(req) else {
        req.release(bound);
        return Err(Fatal::Error(String::from(
            "InternalError: a BIND_LEXICAL over an object that is no Closure",
        )));
    };
    let Some(cell) = payload.bound.get_mut(slot) else {
        req.release(bound);
        return Err(Fatal::Error(String::from(
            "InternalError: a BIND_LEXICAL naming a slot the closure has not",
        )));
    };
    let previous = std::mem::replace(cell, bound);
    req.release(previous);
    Ok(advance(req, frame))
}

/// `ZEND_BIND_STATIC` (`Z/Zend/zend_vm_def.h:9137`): the prologue of a
/// closure's body, one opline per bound variable, which reads the slot
/// `extended_value` names of the closure's table into the CV in `op1`.
/// By value the slot's value is copied, one more owner; by reference
/// the CV becomes the cell the slot holds, which is the one the
/// enclosing scope's variable is. A `static` variable of a plain
/// function shares the opcode and no compiler here emits one, so a
/// frame that is no closure's is this engine's error.
///
/// ```php
/// $b = 1;
/// $f = function ($a) use ($b) { $b = $b + $a; return $b; };
/// var_dump($f(1), $f(1), $b);
/// ```
/// ```expect
/// int(2)
/// int(2)
/// int(1)
/// ```
///
/// Every declaration of one closure makes one with a table of its
/// own, so two closures of one `function` keyword bound by reference
/// count apart.
///
/// ```php
/// function counter() {
///     $n = 0;
///     return function () use (&$n) { $n = $n + 1; return $n; };
/// }
/// $a = counter();
/// $b = counter();
/// var_dump($a(), $a(), $b());
/// ```
/// ```expect
/// int(1)
/// int(2)
/// int(1)
/// ```
///
/// The cell the CV becomes is the one the enclosing variable is, so a
/// step written through either name is read by both, and the closure
/// that counts a shared number down reaches its bottom.
///
/// ```php
/// $i = 3;
/// $f = function () use (&$i) { echo $i--, "|"; };
/// $f(); $f(); $f();
/// echo "\n", $i, "\n";
/// ```
/// ```expect
/// 3|2|1|
/// 0
/// ```
///
/// A by-value `use` is the copy it was, rebound from the table at every
/// call, so the same closure written without the `&` counts nowhere.
///
/// ```php
/// $j = 3;
/// $g = function () use ($j) { echo $j--, "|"; };
/// $g(); $g(); $g();
/// echo "\n", $j, "\n";
/// ```
/// ```expect
/// 3|3|3|
/// 3
/// ```
pub(crate) fn bind_static<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    _: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(object) = req.vm().at(frame).and_then(|state| state.closure) else {
        return Err(Fatal::Error(String::from(
            "InternalError: a BIND_STATIC on a frame that is no closure's, and a static variable of a function is not served yet",
        )));
    };
    let slot = (op.extended_value >> 3) as usize;
    let by_ref = op.extended_value & BIND_REF != 0;
    let held = match object
        .closure(req)
        .and_then(|closure| closure.bound.get(slot))
    {
        Some(held) => *held,
        None => {
            return Err(Fatal::Error(String::from(
                "InternalError: a BIND_STATIC naming a slot the closure has not",
            )));
        }
    };
    let bound = if by_ref {
        let cell = if let Some(cell) = held.as_php_ref() {
            *cell
        } else {
            let cell = match req.reference(held.undef_as_null()) {
                Ok(cell) => cell,
                Err(thrown) => return threw(req, frame, &thrown),
            };
            // The owner the table's slot had is the cell's now.
            if let Some(payload) = object.native_mut::<ClosureInfo>(req)
                && let Some(at) = payload.bound.get_mut(slot)
            {
                *at = cell.as_value();
            }
            cell
        };
        let bound = cell.as_value();
        req.addref(&bound);
        bound
    } else {
        req.addref(&held);
        held
    };
    let previous = req.set_slot(frame, op.op1, bound);
    req.release(previous);
    Ok(advance(req, frame))
}

/// `ZEND_BIND_GLOBAL` (`Z/Zend/zend_vm_def.h:8679`), one variable of a
/// `global` statement: the CV in `op1` bound by reference to the entry
/// the name in `op2` has in `EG(symbol_table)`, the table the main op
/// array's variables and `$GLOBALS` are ([`Request::global_cell`]). A
/// name the table has not is laid down holding `null`, so the write
/// through the binding is a global the main op array reads after; a
/// cell the CV held before is given up after the bind (`GC_DELREF`).
///
/// ```php
/// function f() { global $a; var_dump($a); $a = 2; }
/// $a = 1;
/// f();
/// var_dump($a);
/// function g() { global $n, $m; var_dump($n); $n = 5; $m = 6; }
/// g();
/// var_dump($n, $m);
/// ```
/// ```expect
/// int(1)
/// int(2)
/// NULL
/// int(5)
/// int(6)
/// ```
///
/// The binding is by reference, so a second name bound in the same
/// function, or a `global` at the top level where the CV is the
/// entry's own, reads the one cell:
///
/// ```php
/// $x = 1;
/// global $x;
/// function h() { global $x; $y = &$x; $y++; unset($x); global $x; return $x; }
/// var_dump(h(), $x);
/// ```
/// ```expect
/// int(2)
/// int(2)
/// ```
pub(crate) fn bind_global<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let name = exec::constant(code, ConstIdx(op.op2));
    let Some(name) = name.as_str_bytes().map(<[u8]>::to_vec) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_BIND_GLOBAL over a name that is no string",
        )));
    };
    let cell = match req.global_cell(&name) {
        Ok(cell) => cell,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let previous = req.set_slot(frame, op.op1, cell.as_value());
    req.release(previous);
    Ok(advance(req, frame))
}

/// `ZEND_INIT_DYNAMIC_CALL` (`Z/Zend/zend_vm_def.h:3899`): the call
/// frame for a callee named as the program serves, `$f(…)`, each shape
/// through php's own initialiser. An object is
/// `zend_init_dynamic_call_object`: a `Closure` its payload's function
/// with its receiver, its scope and the closure itself on the call, so
/// the body's `BIND_STATIC` prologue finds what it bound, and any other
/// object its class's `__invoke`. A string is
/// `zend_init_dynamic_call_string`: `Class::method` a `static` call, and
/// otherwise the function of that name, a name nothing declares being
/// php's `Error` naming it as written. An array is
/// `zend_init_dynamic_call_array`, `[$object, 'method']` or `['Class',
/// 'method']`. What is none of the three is php's `Error` for a value
/// that is not callable.
///
/// ```php
/// class C { public $v = 'v'; function m($x) { return "m$x$this->v "; } static function s($x) { return "s$x "; } function __invoke($x) { return "i$x "; } }
/// $c = new C;
/// $a = [$c, 'm']; $b = ['C', 's']; $s = 'C::s'; $t = [$c, 's'];
/// echo $a(1), $b(2), $s(3), $c(4), $t(5), "\n";
/// ```
/// ```expect
/// m1v s2 s3 i4 s5
/// ```
///
/// Each refusal is php's `Error` in the initialiser's own words, which
/// are not `zend_is_callable_ex`'s.
///
/// ```php
/// class C { function m() {} private static function p() {} }
/// foreach ([[1], [1 => 1, 2 => 2], [1, 'm'], ['C', 1], ['Nope', 'm'], ['C', 'nope'], ['C', 'm'], 'C::m', 'C::p', [new C, 'nope'], new stdClass, 'Nope::x'] as $f) {
///     try { $f(); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// Error: Array callback must have exactly two elements
/// Error: Array callback has to contain indices 0 and 1
/// Error: First array member is not a valid class name or object
/// Error: Second array member is not a valid method
/// Error: Class "Nope" not found
/// Error: Call to undefined method C::nope()
/// Error: Non-static method C::m() cannot be called statically
/// Error: Non-static method C::m() cannot be called statically
/// Error: Call to private method C::p() from global scope
/// Error: Call to undefined method C::nope()
/// Error: Object of type stdClass is not callable
/// Error: Class "Nope" not found
/// ```
///
/// ```php
/// $f = function () { return 1; };
/// $name = 'strlen';
/// var_dump($f(), $name('abc'));
/// ```
/// ```expect
/// int(1)
/// int(3)
/// ```
///
/// A closure is a value, so one passed to a function is called through
/// the parameter that holds it, and a closure's own body calls the one
/// its parameter holds the same way.
///
/// ```php
/// function apply($f, $x) { return $f($x); }
/// $add = function ($n) { return $n + 1; };
/// var_dump(apply($add, 1), apply(fn ($n) => $n * 3, 2));
/// $twice = function ($f, $x) { return $f($f($x)); };
/// var_dump($twice($add, 1));
/// ```
/// ```expect
/// int(2)
/// int(6)
/// int(3)
/// ```
///
/// ```php
/// $nope = 'nope';
/// $nope();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to undefined function nope() in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// $n = 1;
/// $n();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Value of type int is not callable in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn init_dynamic_call<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let callee = exec::operand(req, frame, code, op.op2_type, op.op2).deref();
    let count = usize::try_from(op.extended_value).unwrap_or(0);
    let resolved = if let Some(object) = callee.as_object().copied() {
        dynamic_call_object(req, object)
    } else if let Some(name) = callee.as_str_bytes().map(<[u8]>::to_vec) {
        dynamic_call_string(req, &name)
    } else if let Some(array) = callee.as_array().copied() {
        dynamic_call_array(req, array)
    } else {
        Err(crate::api::raising::throw(
            req,
            "Error",
            &format!("Value of type {} is not callable", callee.type_name()),
        ))
    };
    let target = match resolved {
        Ok(target) => target,
        Err(thrown) => {
            exec::free_operand(req, frame, code, op.op2_type, op.op2);
            return threw(req, frame, &thrown);
        }
    };
    req.vm_mut().push_call(CallState {
        function: target.function,
        name: target.name,
        arguments: Vec::new(),
        count,
        this: target.this,
        called_scope: target.called_scope,
        closure: target.closure,
        extra_named: Vec::new(),
        stage: CallStage::Building,
        frameless: false,
        coerced: Vec::new(),
    });
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    Ok(advance(req, frame))
}

/// What a dynamic call resolved to: the callee, the name the call frame
/// carries for a method, the receiver it holds one owner of, the called
/// scope, and the closure the frame holds one owner of.
struct DynamicTarget {
    function: Function,
    name: Option<PhpString>,
    this: Option<PhpObject>,
    called_scope: Option<crate::api::values::ClassId>,
    closure: Option<PhpObject>,
}

/// `zend_init_dynamic_call_object` (`Z/Zend/zend_execute.c:5152`): the
/// object's `get_closure`, a `Closure`'s payload with its receiver, its
/// scope and the closure itself held by the call, or `__invoke` on the
/// object where its class declares one, and php's `Error` naming the
/// class otherwise.
fn dynamic_call_object(req: &mut Request, object: PhpObject) -> Result<DynamicTarget, Thrown> {
    if let Some(closure) = object.closure(req).cloned() {
        req.addref(&object.as_value());
        if let Some(this) = closure.this {
            req.addref(&this.as_value());
        }
        return Ok(DynamicTarget {
            function: closure.function,
            name: None,
            this: closure.this,
            called_scope: closure.scope,
            closure: Some(object),
        });
    }
    let class = object.class();
    let Some(function) = req.method_named(class, b"__invoke") else {
        let shown = req.class_name(class);
        return Err(crate::api::raising::throw(
            req,
            "Error",
            &format!("Object of type {shown} is not callable"),
        ));
    };
    let name = req.method_name_string(function)?;
    let this = if req
        .method_decl(function)
        .is_some_and(|decl| decl.is_instance)
    {
        req.addref(&object.as_value());
        Some(object)
    } else {
        None
    };
    Ok(DynamicTarget {
        function,
        name: Some(name),
        this,
        called_scope: Some(class),
        closure: None,
    })
}

/// `zend_init_dynamic_call_string` (`Z/Zend/zend_execute.c:5072`): a
/// `Class::method` string is the class by name, `Class "X" not found`
/// where nothing declares it, and the method as
/// `zend_std_get_static_method` finds it, with its access rule and
/// php's refusal of an instance method named on the class; any other
/// string is the function of that name with a leading `\` skipped, and
/// the `Error` naming it as written where there is none.
fn dynamic_call_string(req: &mut Request, name: &[u8]) -> Result<DynamicTarget, Thrown> {
    let split = name
        .windows(2)
        .rposition(|pair| pair == b"::")
        .filter(|at| *at > 0);
    if let Some(at) = split {
        let class = dynamic_call_class(req, &name[..at])?;
        return dynamic_call_static(req, class, &name[at + 2..]);
    }
    let bare = name.strip_prefix(b"\\").unwrap_or(name);
    let Some(function) = exec::find_function(req, &bare.to_ascii_lowercase()) else {
        return Err(crate::api::raising::throw(
            req,
            "Error",
            &format!(
                "Call to undefined function {}()",
                String::from_utf8_lossy(name)
            ),
        ));
    };
    Ok(DynamicTarget {
        function,
        name: None,
        this: None,
        called_scope: None,
        closure: None,
    })
}

/// `zend_init_dynamic_call_array` (`Z/Zend/zend_execute.c:5196`): a
/// pair of exactly the members 0 and 1, the first a class name or an
/// object and the second a method name, each shape php's `Error` in its
/// words where it is not; a class name's method found as a `static` call
/// finds it, an object's through its `get_method` with the access rule
/// `zend_std_get_method` holds it to, a `static` method on an object
/// called with no receiver.
fn dynamic_call_array(req: &mut Request, array: PhpArray) -> Result<DynamicTarget, Thrown> {
    let refuse = |req: &mut Request, text: &str| crate::api::raising::throw(req, "Error", text);
    if array.len() != 2 {
        return Err(refuse(req, "Array callback must have exactly two elements"));
    }
    let (Some(first), Some(second)) = (
        array.get(&crate::api::values::Key::Int(0)),
        array.get(&crate::api::values::Key::Int(1)),
    ) else {
        return Err(refuse(req, "Array callback has to contain indices 0 and 1"));
    };
    let (first, second) = (first.deref(), second.deref());
    if first.as_object().is_none() && first.as_str_bytes().is_none() {
        return Err(refuse(
            req,
            "First array member is not a valid class name or object",
        ));
    }
    let Some(method) = second.as_str_bytes().map(<[u8]>::to_vec) else {
        return Err(refuse(req, "Second array member is not a valid method"));
    };
    if let Some(class) = first.as_str_bytes().map(<[u8]>::to_vec) {
        let class = dynamic_call_class(req, &class)?;
        return dynamic_call_static(req, class, &method);
    }
    let Some(object) = first.as_object().copied() else {
        return Err(refuse(
            req,
            "First array member is not a valid class name or object",
        ));
    };
    let class = object.class();
    let table = req.object_handlers(object.heap());
    let Some(function) = table.get_method(req, object, &method.to_ascii_lowercase()) else {
        let written = Value::string(PhpString::new(req, &method)?);
        let thrown = undefined_method(req, class, &written);
        req.release(written);
        return Err(thrown);
    };
    req.check_method_access(function, &method)?;
    let name = PhpString::new(req, &method)?;
    let this = if req
        .method_decl(function)
        .is_some_and(|decl| decl.is_instance)
    {
        req.addref(&object.as_value());
        Some(object)
    } else {
        None
    };
    Ok(DynamicTarget {
        function,
        name: Some(name),
        this,
        called_scope: Some(class),
        closure: None,
    })
}

/// `zend_fetch_class_by_name` under `ZEND_FETCH_CLASS_EXCEPTION`: the
/// class of that name, a leading `\` skipped, and php's `Error` where
/// nothing declares it.
fn dynamic_call_class(
    req: &mut Request,
    name: &[u8],
) -> Result<crate::api::values::ClassId, Thrown> {
    let bare = name.strip_prefix(b"\\").unwrap_or(name);
    match req.class_named(bare) {
        Some(class) => Ok(class),
        None => Err(crate::api::raising::throw(
            req,
            "Error",
            &format!("Class \"{}\" not found", String::from_utf8_lossy(name)),
        )),
    }
}

/// `zend_std_get_static_method` over `class` and then the dynamic
/// call's refusal of an instance method: `Call to undefined method` and
/// `zend_bad_method_call` as `INIT_STATIC_METHOD_CALL` raises them, and
/// `zend_non_static_method_call`.
fn dynamic_call_static(
    req: &mut Request,
    class: crate::api::values::ClassId,
    method: &[u8],
) -> Result<DynamicTarget, Thrown> {
    let written = Value::string(PhpString::new(req, method)?);
    let Some(function) = req.method_named(class, &method.to_ascii_lowercase()) else {
        let thrown = undefined_method(req, class, &written);
        req.release(written);
        return Err(thrown);
    };
    req.release(written);
    req.check_method_access(function, method)?;
    req.static_method_found(function)?;
    if req
        .method_decl(function)
        .is_some_and(|decl| decl.is_instance)
    {
        return Err(non_static_method_call(req, function));
    }
    let name = PhpString::new(req, method)?;
    Ok(DynamicTarget {
        function,
        name: Some(name),
        this: None,
        called_scope: Some(class),
        closure: None,
    })
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
    let function = if let Cached::Function(function) = req.cached(frame, slot) {
        Some(function)
    } else {
        let found = lookups.iter().find_map(|&at| {
            let key = exec::constant(code, at);
            exec::find_function(req, key.as_str_bytes().unwrap_or(name.as_bytes()))
        });
        if let Some(function) = found {
            req.prime(frame, slot, Cached::Function(function));
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
        closure: None,
        extra_named: Vec::new(),
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
/// returned, sent by value: the compiler emits it for a callee the
/// table has whose parameter at this number takes its argument by
/// value, and [`send_ref`] where the parameter takes one by reference,
/// so a reference the operand holds is followed here and never passed
/// on (`ZVAL_COPY_DEREF`).
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

/// `ZEND_ARG_SEND_MODE` (`Z/Zend/zend_compile.h:1141`) over the callee
/// the `INIT` resolved: whether the parameter numbered `number` takes
/// its argument by reference, and whether only where it can
/// (`ZEND_SEND_PREFER_REF`), the variadic's for every argument past the
/// declared ones.
fn send_mode(req: &Request, number: u32) -> (bool, bool) {
    let Some(function) = req.vm().building_call_function() else {
        return (false, false);
    };
    let Some(decl) = req.function_decl(function) else {
        return (false, false);
    };
    let at = usize::try_from(number.saturating_sub(1)).unwrap_or(usize::MAX);
    let variadic = decl.parameters.last().filter(|last| last.variadic);
    decl.parameters
        .get(at)
        .filter(|parameter| !parameter.variadic)
        .or(variadic)
        .map_or((false, false), |parameter| {
            (parameter.by_ref, parameter.by_ref && parameter.prefer_ref)
        })
}

/// `ARG_SHOULD_BE_SENT_BY_REF`: by reference, or by reference where it
/// can be.
fn should_be_sent_by_ref(req: &Request, number: u32) -> bool {
    send_mode(req, number).0
}

/// `ARG_MUST_BE_SENT_BY_REF` (`Z/Zend/zend_compile.h:1137`): by
/// reference and nothing else.
fn must_be_sent_by_ref(req: &Request, number: u32) -> bool {
    let (by_ref, prefer) = send_mode(req, number);
    by_ref && !prefer
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
    if should_be_sent_by_ref(req, op.op2) {
        return send_ref::<T>(req, frame, code, op);
    }
    send(req, frame, code, op)
}

/// `ZEND_SEND_VAR_NO_REF_EX` (`Z/Zend/zend_vm_def.h:5019`): what a call
/// returned, sent to a callee the compiler knew nothing of, which reads
/// the parameter's by-reference bit off the callee the `INIT` resolved
/// (`ARG_SHOULD_BE_SENT_BY_REF`) and is `SEND_VAR` where it is unset
/// (`ZEND_VM_C_GOTO(send_var)`). Where it is set, what the call
/// returned is placed as it stands, with no reference followed: a call
/// that returned by reference hands its cell to the parameter, so a
/// write through the parameter lands where the callee named; anything
/// else has no place in the caller for such a write to land in, so it
/// is wrapped in a cell of its own (`ZVAL_NEW_REF(arg, arg)`) and php's
/// notice `Only variables should be passed by reference` is raised.
/// php's third arm, `ARG_MAY_BE_SENT_BY_REF`, is the parameter declared
/// prefer-by-reference, `extract`'s and `array_multisort`'s, which
/// takes what the call returned as it stands and without the notice.
///
/// ```php
/// class C { function m(&$x) { $x = 9; var_dump($x); } }
/// function g() { return 7; }
/// $o = new C;
/// $o->m(g());
/// ```
/// ```expect
/// Notice: Only variables should be passed by reference in %s on line 5
/// int(9)
/// ```
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
    let (by_ref, prefer) = send_mode(req, op.op2);
    if !by_ref {
        return send(req, frame, code, op);
    }
    // `GET_OP1_ZVAL_PTR(BP_VAR_R)` over a `VAR`, which follows no
    // reference: the operand's owner is the argument's from here.
    let returned = req.take_slot(frame, exec::temporary_slot(code, op.op1));
    if returned.as_php_ref().is_some() || prefer {
        return place_argument(req, frame, op, returned);
    }
    let cell = match req.reference(returned) {
        Ok(cell) => cell,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    crate::api::raising::notice(req, "Only variables should be passed by reference");
    place_argument(req, frame, op, cell.as_value())
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

/// `ZEND_INCLUDE_OR_EVAL` (`Z/Zend/zend_vm_def.h:6544`), the four file
/// constructs: the operand as a string (`zval_try_get_tmp_string`), the
/// file resolved, compiled and put in the included-files table
/// ([`Request::include_file`]), and its main op array served on a frame
/// of its own that shares the includer's variables
/// ([`Request::push_code_frame`]); what the file `return`s goes into the
/// `VAR` under `RETURN_VALUE_USED`, `int(1)` where it returns nothing,
/// since the compiler ends a file's op array in `RETURN int(1)`. A once
/// form over a path the table has answers `true` and pushes nothing;
/// a path nothing opens answers `false` under an `include`, after the
/// two warnings, and is the `Error` under a `require`. A file whose op
/// array is one `RETURN` of a constant, an empty file among them, is
/// answered without a frame (`Z/Zend/zend_vm_def.h:6569`). The file's
/// classes and functions are declared before its frame is pushed, as
/// opcache binds them when it loads the file, and a function the
/// request already has is the redeclaration fatal, so a `require` of
/// one file twice ends the request where php's does.
///
/// A witness's own file is the one file the recipe lays down, so the
/// witnesses here include `witness.php` itself, guarded by a variable
/// the includer and the included file share.
///
/// ```php
/// $n = ($n ?? 0) + 1;
/// if ($n > 1) { echo "included $n, a=$a\n"; $b = $n * 10; return $n; }
/// $a = 'x';
/// var_dump(include __FILE__, $b, include 'witness.php', $b);
/// var_dump(include_once __FILE__, require_once 'witness.php', include_once './witness.php');
/// ```
/// ```expect
/// included 2, a=x
/// included 3, a=x
/// int(2)
/// int(20)
/// int(3)
/// int(30)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
///
/// ```php
/// ini_set('include_path', '.');
/// var_dump(include 'nope.php');
/// require 'nope.php';
/// echo "unreached";
/// ```
/// ```expect exit=255
/// Warning: include(nope.php): Failed to open stream: No such file or directory in %s on line 3
///
/// Warning: include(): Failed opening 'nope.php' for inclusion (include_path='.') in %s on line 3
/// bool(false)
///
/// Warning: require(nope.php): Failed to open stream: No such file or directory in %s on line 4
///
/// Fatal error: Uncaught Error: Failed opening required 'nope.php' (include_path='.') in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
///
/// A throwable raised inside the included file unwinds through the
/// includer, whose trace names the construct as php's
/// `zend_fetch_debug_backtrace` names a code frame, with the included
/// file as its one argument where a frame stands above it and none
/// where the file itself threw.
///
/// ```php
/// if (!isset($go)) { function boom() { throw new Exception('boom'); } }
/// if ($go ?? false) { boom(); }
/// $go = 1;
/// try { include __FILE__; } catch (Exception $e) { echo $e->getTraceAsString(), "\n"; }
/// $go = 2;
/// try { include __FILE__; } catch (Exception $e) { echo $e->getTraceAsString(), "\n"; }
/// ```
/// ```expect
/// #0 %s(3): boom()
/// #1 %s(5): include('%s')
/// #2 {main}
/// #0 %s(3): boom()
/// #1 %s(7): include('%s')
/// #2 {main}
/// ```
pub(crate) fn include_or_eval<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let kind = match exec::IncludeKind::of(op.extended_value) {
        Some(exec::IncludeKind::Eval) => {
            return Err(Fatal::Error(String::from(
                "InternalError: an INCLUDE_OR_EVAL (eval), which the compiler does not emit yet",
            )));
        }
        Some(kind) => kind,
        None => {
            return Err(Fatal::Error(String::from(
                "InternalError: an INCLUDE_OR_EVAL whose extended_value names no construct",
            )));
        }
    };
    let operand = exec::operand(req, frame, code, op.op1_type, op.op1);
    let written = crate::ops::to_string(req, &operand);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    let written = match written {
        Ok(string) => {
            let bytes = string.as_bytes().to_vec();
            req.release(Value::string(string));
            bytes
        }
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let returns_to =
        (op.out_type != OperandKind::Unused).then(|| (frame, exec::temporary_slot(code, op.out)));
    let unit = match req.include_file(kind, &written) {
        Ok(Opened::Unit(unit)) => unit,
        Ok(Opened::Already) => return answered(req, frame, returns_to, Value::bool(true)),
        Ok(Opened::Failed) => return answered(req, frame, returns_to, Value::bool(false)),
        Err(thrown) => return threw(req, frame, &thrown),
    };
    crate::engine::note_compiled(req, &unit);
    if !req.declare_classes(&unit) || !req.declare_functions(&unit) {
        return Ok(Stepped::Threw(frame));
    }
    let main = exec::code_of(&unit);
    if let [only] = &main.oplines[..]
        && only.opcode == Opcode::Return
        && only.op1_type == OperandKind::Const
    {
        let value = exec::constant(main, ConstIdx(only.op1));
        req.addref(&value);
        return answered(req, frame, returns_to, value);
    }
    let callee = req.push_code_frame(unit, frame, returns_to)?;
    Ok(Stepped::Pushed(callee))
}

/// The value an `INCLUDE_OR_EVAL` answers without a frame, into the
/// `VAR` where the opline's value is read and given up where it is not.
fn answered(
    req: &mut Request,
    frame: FrameRef,
    returns_to: Option<(FrameRef, u32)>,
    value: Value,
) -> Edge {
    match returns_to {
        Some((at, slot)) => {
            let previous = req.set_slot(at, slot, value);
            req.release(previous);
        }
        None => req.release(value),
    }
    Ok(advance(req, frame))
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
    let callee = req.push_user_frame(function, call, returns_to, Some(frame))?;
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
    if let Some(at) = exec::parameter_prologue(req, callee, body, function, passed)? {
        return Ok(Stepped::Threw(at));
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
/// A call a native served has no user frame that pushed it, and php's
/// sentence leaves the call site out there
/// ([`missing_argument`]); the trace still carries the call, as
/// `[internal function]`.
///
/// ```php
/// class C { function m($a, $b) {} }
/// (new ReflectionMethod("C", "m"))->invoke(new C);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: Too few arguments to function C::m(), 0 passed and exactly 2 expected in %s:2
/// Stack trace:
/// #0 [internal function]: C->m()
/// #1 %s(3): ReflectionMethod->invoke(Object(C))
/// #2 {main}
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
/// Which of the two rules applies is the *caller's*
/// `declare(strict_types=1)` and never the callee's:
/// `ZEND_ARG_USES_STRICT_TYPES` (`Z/Zend/zend_compile.h:723`) reads
/// `ZEND_ACC_STRICT_TYPES` off `prev_execute_data->func`, the op array
/// the call was made from. The file above declares none, so every
/// argument there
/// converts; the same call in a file that declares it raises instead,
/// and nothing converts but an int where a float is asked for
/// (`zend_verify_scalar_type_hint`).
///
/// ```php
/// declare(strict_types=1);
/// function f(int $a) { var_dump($a); }
/// f("5");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: f(): Argument #1 ($a) must be of type int, string given, called in %s on line 4 and defined in %s:3
/// Stack trace:
/// #0 %s(4): f('5')
/// #1 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// declare(strict_types=1);
/// function f(float $a) { var_dump($a); }
/// f(5);
/// ```
/// ```expect
/// float(5)
/// ```
///
/// A frame no user opline pushed has no caller to read the bit off,
/// and php's there is the native's own frame, which carries none:
/// a callback a native calls converts its argument though the file the
/// callback was written in is the strict one. That is the whole of the
/// rule's caller half, measured.
///
/// ```php
/// declare(strict_types=1);
/// function f(int $a) { var_dump($a); }
/// register_shutdown_function('f', "5");
/// ```
/// ```expect
/// int(5)
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
    if crate::api::coerce::holds(req, argument, &ty, req.frame_scopes(frame)) {
        return Ok(None);
    }
    let strict = req.caller_strict_types(frame);
    if let Some(coerced) = crate::api::coerce::scalar_type_hint(req, argument, ty.mask(), strict)? {
        return Ok(Some(coerced));
    }
    Err(arg_type_error(req, frame, number, at, &ty, argument))
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
/// here on leaves the empty array. The extra named arguments a call
/// placed ([`exec::FrameState::extra_named`]) follow under their names,
/// each held to the declared type under the number after the last
/// positional one; the witness for that stands beside
/// `call_user_func_array` in `ext/standard`, the one caller that places
/// them.
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
    // `ZEND_CALL_HAS_EXTRA_NAMED_PARAMS`: the extra named arguments
    // after the positional ones, each under its name, held to the
    // declared type under the number php's `arg_num` stands at once
    // the loop above has walked past the last positional one.
    let number = first.saturating_add(u32::try_from(count).unwrap_or(u32::MAX));
    let named = req
        .vm()
        .at(frame)
        .map(|state| state.extra_named.clone())
        .unwrap_or_default();
    for (name, argument) in named {
        let argument = match verified_arg(req, frame, number, at, &argument) {
            Ok(None) => argument,
            Ok(Some(coerced)) => {
                if let Some(state) = req.vm_mut().at_mut(frame)
                    && let Some((_, place)) = state
                        .extra_named
                        .iter_mut()
                        .find(|(held, _)| held.as_bytes() == name.as_bytes())
                {
                    let previous = std::mem::replace(place, coerced);
                    req.release(previous);
                }
                coerced
            }
            Err(thrown) => return threw(req, frame, &thrown),
        };
        req.addref(&argument);
        let key = php_heap::Key::Str(name.heap());
        if let Err(thrown) = req.array_update(collected.heap(), key, argument) {
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
/// (`zend_check_type` and the coercion beside it, read under
/// `ZEND_RET_USES_STRICT_TYPES`, which is this frame's own function's
/// `ZEND_ACC_STRICT_TYPES` and so the *callee's* file's, where an
/// argument's is the caller's), and `zend_verify_return_error`'s
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
/// declare(strict_types=1);
/// function f(): int { $x = "5"; return $x; }
/// var_dump(f());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: f(): Return value must be of type int, string returned in %s:3
/// Stack trace:
/// #0 %s(4): f()
/// #1 {main}
///   thrown in %s on line 3
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
    if crate::api::coerce::holds(req, &returned, &ty, req.frame_scopes(frame)) {
        return Ok(advance(req, frame));
    }
    let strict = req.frame_strict_types(frame);
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
    let declared = crate::api::coerce::declared_type_name(ty);
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
/// where the frame that pushed the call is user code, which php's
/// uncaught rendering then closes with ` and defined`
/// ([`Request::throwable_to_string`]). The throwable's own site is the
/// callee's, where the loop stands.
///
/// The clause is picked by [`call_site`], `EX(prev_execute_data)` as
/// php reads it, and not by the frame standing under this one on the
/// request's stack: a call a native served has no user frame that
/// pushed it, so php names no call site there, where the frame below is
/// whatever user code happened to call the native.
///
/// ```php
/// class C { function m(int $a) { var_dump($a); } }
/// (new ReflectionMethod("C","m"))->invoke(new C, "nope");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: C::m(): Argument #1 ($a) must be of type int, string given in %s:2
/// Stack trace:
/// #0 [internal function]: C->m('nope')
/// #1 %s(3): ReflectionMethod->invoke(Object(C), 'nope')
/// #2 {main}
///   thrown in %s on line 2
/// ```
///
/// The same argument through the two paths, the clause on the one an
/// opline pushed and none on the one a native served:
///
/// ```php
/// function f(int $a) {}
/// try { f("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { (new ReflectionFunction("f"))->invoke("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// f(): Argument #1 ($a) must be of type int, string given, called in %s on line 3
/// f(): Argument #1 ($a) must be of type int, string given
/// ```
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
    let declared = crate::api::coerce::declared_type_name(ty);
    let given = crate::ops::value_name(req, &argument.undef_as_null());
    let caller = call_site(req, frame);
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

/// `EX(prev_execute_data)` as php's two argument diagnostics read it,
/// as a file and a line: the frame whose opline pushed the call to
/// `frame`, and nothing where a native pushed it or where `frame` is
/// the one `serve` pushed. Both `zend_missing_arg_error` and
/// `zend_verify_arg_error` drop the clause that names the call site
/// where this is nothing.
fn call_site(req: &Request, frame: FrameRef) -> Option<(String, u32)> {
    let caller = req.vm().at(frame)?.caller?;
    let caller = req.vm().at(caller)?;
    Some((
        exec::body_of(&caller.unit, caller.body)
            .map(|code| String::from_utf8_lossy(&code.filename.0).into_owned())
            .unwrap_or_default(),
        exec::line_of(caller),
    ))
}

/// `zend_missing_arg_error` (`Z/Zend/zend_execute.c`), which has two
/// wordings and picks between them by `EX(prev_execute_data)`: the
/// frame whose opline pushed the call names the call site, and where
/// there is none, or php's is no user frame
/// (`ZEND_USER_CODE(ptr->func->common.type)`), the sentence leaves the
/// call site out. A call a native served
/// ([`Request::serve_call`]) is the second: its frame has no caller
/// slot and no caller, and php's `prev_execute_data` there is the
/// native's own frame. Reading the frame under this one instead would
/// name whatever user frame happens to stand below the native, which
/// is a call site the program never wrote.
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
    let caller = call_site(req, frame);
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
///
/// A default naming a constant is the `IS_CONSTANT_AST` constant the
/// compiler left, evaluated here: a persistent constant, a class
/// constant, and arithmetic over one:
///
/// ```php
/// class A { const X = 5; }
/// function f($a = PHP_INT_MAX, $b = A::X, $c = PHP_INT_SIZE * 2) { var_dump($a, $b, $c); }
/// f();
/// f(1, 2, 3);
/// ```
/// ```expect
/// int(9223372036854775807)
/// int(5)
/// int(16)
/// int(1)
/// int(2)
/// int(3)
/// ```
///
/// A name nothing defines is php's `Error` where the default is
/// evaluated, which is the callee's own line:
///
/// ```php
/// function g($a = NOPE) { var_dump($a); }
/// g();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Undefined constant "NOPE" in %s:2
/// Stack trace:
/// #0 %s(3): g()
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
        let default = match default_value(req, code, ConstIdx(op.op2)) {
            Ok(default) => default,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        let evaluated = default.1;
        let previous = req.set_slot(frame, op.out, default.0);
        req.release(previous);
        // php goes on to `recv_init_check_type` for a default it
        // evaluated and returns for one it copied, so a constant
        // expression's value is held to the parameter's type and a
        // folded literal is not.
        if !evaluated {
            return Ok(advance(req, frame));
        }
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

/// `RT_CONSTANT(opline, opline->op2)` as `ZEND_RECV_INIT` reads it: the
/// value with one more owner, and, where it is the `IS_CONSTANT_AST` the
/// compiler left for a default naming a constant (`zend_compile_params`
/// under `ZEND_COMPILE_NO_PERSISTENT_CONSTANT_SUBSTITUTION`), the
/// constant expression it indexes, evaluated in the function's scope
/// (`zval_update_constant_with_ctx`, `EX(func)->op_array.scope`). The
/// second word says which of the two it was, since php holds only what
/// it evaluated to the parameter's type. php keeps an evaluated value
/// that is not refcounted in the opline's runtime cache; nothing a
/// program can see turns on that, and it is not kept here.
fn default_value(req: &mut Request, code: &OpArray, at: ConstIdx) -> Result<(Value, bool), Thrown> {
    let unevaluated = code
        .literals
        .get(at.0 as usize)
        .and_then(|constant| constant.as_const_expr())
        .and_then(|at| code.const_exprs.get(at as usize));
    if let Some(expr) = unevaluated {
        let scope = req.executed_scope();
        return Ok((crate::api::call::eval_const_expr(req, expr, scope)?, true));
    }
    let value = exec::constant(code, at);
    req.addref(&value);
    Ok((value, false))
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
///
/// The constructor is what the handler table's `get_constructor` answers: a user
/// class's is held to the scope that may call it, and `Generator`'s is
/// php's refusal of a class reserved for internal use.
///
/// ```php
/// class C { private function __construct() { echo "made\n"; } static function make() { return new C; } }
/// C::make();
/// try { new C; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { new Generator; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// made
/// Call to private C::__construct() from global scope
/// The "Generator" class is reserved for internal use and cannot be manually instantiated
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
    // `Z_OBJ_HT_P(result)->get_constructor`: the table's answer, which
    // is `zend_std_get_constructor`'s visibility check for a user class
    // and a refusal for a class php reserves, `Generator`'s.
    let constructor = match req.handlers_of(class).get_constructor(req, object) {
        Ok(constructor) => constructor,
        Err(thrown) => return threw(req, frame, &thrown),
    };
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
        closure: None,
        extra_named: Vec::new(),
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
/// takes the calling frame's `$this` where it is an instance of the
/// class and is `Non-static method X::m() cannot be called statically`
/// where there is none; one declared `static` called through `self` or `parent`
/// forwards the calling frame's called scope, which is what `static`
/// reads in it. `op1` unused is `self`, `parent` or `static`, and a
/// `VAR` the class a `FETCH_CLASS` left.
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
///
/// The class and the callee are resolved on the first pass alone, which
/// a call inside a loop is where php's cache earns its slots; what runs
/// is what ran before it.
///
/// ```php
/// class S { static function m($n) { echo "m$n\n"; } }
/// $i = 0;
/// while ($i < 3) { S::m($i); $i = $i + 1; }
/// ```
/// ```expect
/// m0
/// m1
/// m2
/// ```
///
/// A method named on a trait itself, and never one a class took from
/// it, is php's deprecation on every pass, since php caches no such
/// callee, and an abstract one is `zend_abstract_method_call`'s `Error`
/// ([`Request::static_method_found`]):
///
/// ```php
/// trait T { static function f() { return 1; } abstract static function g(); }
/// class C { use T; static function g() { return 2; } }
/// echo C::f(), C::g(), "\n";
/// $i = 0;
/// while ($i < 2) { echo T::f(), "\n"; $i = $i + 1; }
/// $name = 'T::f'; echo $name(), "\n";
/// try { T::g(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 12
///
/// Deprecated: Calling static trait method T::f is deprecated, it should only be called on a class using the trait in %s on line 6
/// 1
///
/// Deprecated: Calling static trait method T::f is deprecated, it should only be called on a class using the trait in %s on line 6
/// 1
///
/// Deprecated: Calling static trait method T::f is deprecated, it should only be called on a class using the trait in %s on line 7
/// 1
/// Cannot call abstract method T::g()
/// ```
pub(crate) fn init_static_method_call<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let slot = CacheSlot(op.out);
    // The slot holds the class php resolved off a constant pair; a class
    // reference of the scope, or one a `FETCH_CLASS` left, is resolved on
    // every pass, since `static` and a computed name differ between them.
    let primed = if op.op1_type == OperandKind::Const {
        req.cached(frame, slot)
    } else {
        Cached::Unprimed
    };
    let class = match primed {
        Cached::Class(class) | Cached::ClassAndCallee { class, .. } => class,
        _ => {
            let class = match class_operand(req, frame, code, op.op1_type, op.op1, None) {
                Ok(class) => class,
                Err(thrown) => return threw(req, frame, &thrown),
            };
            // `if (OP2_TYPE != IS_CONST) { CACHE_PTR(opline->result.num, ce); }`:
            // where the callee is named as the program serves, or not
            // named at all, the class alone is what the slot holds, and
            // where it is a constant the pair below writes both.
            if op.op1_type == OperandKind::Const && op.op2_type != OperandKind::Const {
                req.prime(frame, slot, Cached::Class(class));
            }
            class
        }
    };
    let cached_callee = match (op.op2_type, primed) {
        (
            OperandKind::Const,
            Cached::ClassAndCallee {
                class: held,
                function,
            },
        ) if held == class => Some(function),
        _ => None,
    };
    if let Some(function) = cached_callee {
        let written = exec::constant(code, ConstIdx(op.op2));
        return static_call_frame(req, frame, op, class, function, Some(written));
    }
    let mut uncached = false;
    let (function, name) = match op.op2_type {
        OperandKind::Unused => {
            let Some(constructor) = req.method_named(class, b"__construct") else {
                let thrown = crate::api::raising::throw(req, "Error", "Cannot call constructor");
                return threw(req, frame, &thrown);
            };
            // `Z_OBJ(EX(This))->ce != ce->constructor->common.scope` over a
            // private constructor.
            let private = req
                .method_decl(constructor)
                .is_some_and(|decl| decl.visibility == php_unit::decl::Visibility::Private);
            let foreign = req
                .vm()
                .this(frame)
                .is_some_and(|this| Some(this.class()) != req.method_class(constructor));
            if private && foreign {
                let name = req.class_name(class);
                let thrown = crate::api::raising::throw(
                    req,
                    "Error",
                    &format!("Cannot call private {name}::__construct()"),
                );
                return threw(req, frame, &thrown);
            }
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
            match req.static_method_found(function) {
                Ok(of_trait) => uncached = of_trait,
                Err(thrown) => return threw(req, frame, &thrown),
            }
            (function, Some(written))
        }
        _ => {
            return Err(Fatal::Error(String::from(
                "InternalError: ZEND_INIT_STATIC_METHOD_CALL over a method named at run time",
            )));
        }
    };
    // `CACHE_POLYMORPHIC_PTR(opline->result.num, ce, fbc)`, where the
    // callee was named by a constant: after the lookup and its access
    // rule, and before the rule that the callee is no instance method,
    // which php applies on the cached pass too. php holds the write off
    // a callee reached through a trampoline, one marked
    // `ZEND_ACC_NEVER_CACHE` and one a trait declared; this engine has
    // only the last, whose deprecation is raised on every pass.
    if op.op1_type == OperandKind::Const && op.op2_type == OperandKind::Const && !uncached {
        req.prime(frame, slot, Cached::ClassAndCallee { class, function });
    }
    static_call_frame(req, frame, op, class, function, name)
}

/// What `INIT_STATIC_METHOD_CALL` does once it has its class and its
/// callee, whether the slot answered or the tables did: php's
/// `!(fbc->common.fn_flags & ZEND_ACC_STATIC)` rule, which is applied
/// on every pass and not only the one that resolved, and the call frame
/// over the class as its called scope.
fn static_call_frame(
    req: &mut Request,
    frame: FrameRef,
    op: &Opline,
    class: crate::api::values::ClassId,
    function: Function,
    written: Option<Value>,
) -> Edge {
    // `if (!(fbc->common.fn_flags & ZEND_ACC_STATIC))`: an instance
    // method takes the calling frame's `$this` where it is an instance
    // of the class, and is php's `Error` otherwise; a static one called
    // through `self` or `parent` forwards the calling frame's called
    // scope, `Z_OBJCE(EX(This))` or `Z_CE(EX(This))`.
    let receiver = req.vm().this(frame);
    let (this, called) = if req
        .method_decl(function)
        .is_some_and(|decl| decl.is_instance)
    {
        let Some(this) = receiver.filter(|this| req.instance_of(this.class(), class)) else {
            let thrown = non_static_method_call(req, function);
            return threw(req, frame, &thrown);
        };
        req.addref(&this.as_value());
        (Some(this), this.class())
    } else if op.op1_type == OperandKind::Unused
        && matches!(
            op.op1 & FETCH_CLASS_MASK,
            FETCH_CLASS_SELF | FETCH_CLASS_PARENT
        )
    {
        let forwarded = receiver
            .map(|this| this.class())
            .or_else(|| req.vm().at(frame).and_then(|state| state.called_scope));
        (None, forwarded.unwrap_or(class))
    } else {
        (None, class)
    };
    let name = match written {
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
        this,
        called_scope: Some(called),
        closure: None,
        extra_named: Vec::new(),
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
    let found = lowered.as_str_bytes().and_then(|lowered| {
        table
            .get_method(req, object, lowered)
            .map(|function| (function, None))
            .or_else(|| {
                let inner = table.method_receiver(req, object)?;
                let inner_table = req.object_handlers(inner.heap());
                inner_table
                    .get_method(req, inner, lowered)
                    .map(|function| (function, Some(inner)))
            })
    });
    let Some((function, receiver)) = found else {
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
    // A receiver `method_receiver` handed over stands in for the
    // object, one owner moved from the one to the other.
    let object = match receiver {
        Some(inner) => {
            req.addref(&inner.as_value());
            req.release(object.as_value());
            inner
        }
        None => object,
    };
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
        called_scope: Some(object.class()),
        closure: None,
        extra_named: Vec::new(),
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
/// A dimension written over `$this` is this opline's `VAR`, php's
/// `zend_compile_simple_var` under a write, and the `ASSIGN_DIM` over
/// it is the receiver's `offsetSet`; outside a method the fetch is the
/// `Error` and the write never runs.
///
/// ```php
/// class A implements ArrayAccess {
///     public function offsetExists($o): bool { return true; }
///     public function offsetGet($o): mixed { return 1; }
///     public function offsetSet($o, $v): void { echo "set $o=$v\n"; }
///     public function offsetUnset($o): void {}
///     public function m() { $this[0] = 1; }
/// }
/// (new A)->m();
/// $this[0] = 1;
/// ```
/// ```expect exit=255
/// set 0=1
///
/// Fatal error: Uncaught Error: Using $this when not in object context in %s:10
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 10
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

/// `ZEND_FETCH_R` (`Z/Zend/zend_vm_def.h:1825`), a variable the program
/// names read, and a variable under `@`, which `zend_compile_silence`
/// fetches by name so the read happens inside the span:
/// [`fetch_by_name`] under `BP_VAR_R`. The name is looked up in the
/// frame's symbol table, which `zend_rebuild_symbol_table` lays down
/// over the CVs, so a name the op array has a CV for finds it, and a
/// name it has none for finds what `extract` or a fetch for writing put
/// there; a name nothing wrote is `Undefined variable $a` and `null`.
/// `$this` is `zend_fetch_this_var`: the receiver with one more owner,
/// and on a frame without one `Undefined variable $this` and `null`.
///
/// ```php
/// echo @$undefined;
/// $b = 2; $r = 1; $c = &$r;
/// var_dump(@$a, @$this, @$b, @$c);
/// class K { function m() { return @$this; } }
/// var_dump((new K)->m() instanceof K);
/// echo $b;
/// ```
/// ```expect
/// NULL
/// NULL
/// int(2)
/// int(1)
/// bool(true)
/// 2
/// ```
///
/// A name the program computes is read from the same table, and one
/// that is no string is converted as a string is:
///
/// ```php
/// $n = 'v'; $v = 3; ${'1'} = 'one';
/// var_dump($$n, ${'v' . ''}, ${1}, $$m);
/// function f() { $n = 'w'; $w = 4; return $$n; }
/// var_dump(f());
/// ```
/// ```expect
/// Warning: Undefined variable $m in %s on line 3
///
/// Warning: Undefined variable $ in %s on line 3
/// int(3)
/// int(3)
/// string(3) "one"
/// NULL
/// int(4)
/// ```
pub(crate) fn fetch_r<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    fetch_by_name(req, frame, code, op, ByName::R)
}

/// `ZEND_FETCH_IS` (`Z/Zend/zend_vm_def.h:1851`), a variable the program
/// names under `isset`, `empty` or `??`: [`fetch_by_name`] under
/// `BP_VAR_IS`, a name nothing wrote `null` in silence.
///
/// ```php
/// $n = 'nope';
/// var_dump($$n ?? 'd', ${'x'}['k'] ?? 'e');
/// ```
/// ```expect
/// string(1) "d"
/// string(1) "e"
/// ```
pub(crate) fn fetch_is<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    fetch_by_name(req, frame, code, op, ByName::Is)
}

/// `ZEND_FETCH_W` (`Z/Zend/zend_vm_def.h:1830`), a variable the program
/// names fetched for writing, `$$x = 1`, `$$x[] = 1`, and the two halves
/// of a `global` over a name no CV has: [`fetch_by_name`] under
/// `BP_VAR_W`, a name the table has not laid down holding `null` and
/// no warning, the `VAR` an `IS_INDIRECT` to it. Under
/// `ZEND_FETCH_GLOBAL_LOCK` the name stays for the fetch after it.
///
/// ```php
/// $x = 'a';
/// $$x = 1;
/// ${'b' . 'c'}['k'] = 2;
/// var_dump($a, $bc);
/// function f() { global ${sf('g')}; $g++; }
/// function sf($n) { echo "sf\n"; return $n; }
/// $g = 5;
/// f();
/// var_dump($g);
/// try { global ${new stdClass}; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// array(1) {
///   ["k"]=>
///   int(2)
/// }
/// sf
/// int(6)
/// Object of class stdClass could not be converted to string
/// ```
pub(crate) fn fetch_w<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    fetch_by_name(req, frame, code, op, ByName::W)
}

/// `ZEND_FETCH_RW` (`Z/Zend/zend_vm_def.h:1835`), `$$x++` and `$$x .= 'a'`:
/// [`fetch_by_name`] under `BP_VAR_RW`, a name the table has not warned
/// of and then laid down holding `null`, so the step lands in it.
///
/// ```php
/// $x = 'n';
/// $$x++;
/// $$x .= 'a';
/// var_dump($n);
/// ```
/// ```expect
/// Warning: Undefined variable $n in %s on line 3
/// string(2) "1a"
/// ```
pub(crate) fn fetch_rw<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    fetch_by_name(req, frame, code, op, ByName::Rw)
}

/// php's `type` argument to `zend_fetch_var_address_helper`, the four
/// the fetches by name serve.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ByName {
    R,
    W,
    Rw,
    Is,
}

/// `ZEND_FETCH_GLOBAL` and `ZEND_FETCH_GLOBAL_LOCK`
/// (`Z/Zend/zend_compile.h:1082`): the table is `EG(symbol_table)`, and
/// under `ZEND_FETCH_GLOBAL_LOCK` the name is left for the fetch after this one.
const FETCH_GLOBAL: u32 = 1 << 1;
const FETCH_GLOBAL_LOCK: u32 = 1 << 3;

/// `zend_fetch_var_address_helper` (`Z/Zend/zend_vm_def.h:1724`): the
/// name read as a string (`zval_try_get_tmp_string`, a CV nothing wrote
/// warning first, an object with no `__toString` php's `Error`), the
/// table `zend_get_target_symbol_table` names (`EG(symbol_table)` under
/// `ZEND_FETCH_GLOBAL`, the frame's own, rebuilt where it has none,
/// otherwise), and the bucket found: a missing name is laid down under
/// `BP_VAR_W`, warned of and laid down under `BP_VAR_RW`, warned of and
/// read as `null` under `BP_VAR_R`, and `null` in silence under
/// `BP_VAR_IS`; `this`, where no bucket names it, is
/// `zend_fetch_this_var`'s. A read takes the value with one more owner
/// and a reference followed (`ZVAL_COPY_DEREF`) into the temporary; a
/// write leaves the place as an `IS_INDIRECT` in the `VAR`.
fn fetch_by_name(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    mode: ByName,
) -> Edge {
    let lock = op.extended_value & FETCH_GLOBAL_LOCK != 0;
    let global = op.extended_value & (FETCH_GLOBAL | FETCH_GLOBAL_LOCK) != 0;
    let out = exec::temporary_slot(code, op.out);
    let name = match fetched_name(req, frame, code, op) {
        Ok(name) => name,
        Err(thrown) => {
            if !lock {
                exec::free_operand(req, frame, code, op.op1_type, op.op1);
            }
            let previous = req.set_slot(frame, out, Value::undef());
            req.release(previous);
            return threw(req, frame, &thrown);
        }
    };
    let table = if global {
        req.global_table()
    } else {
        req.rebuild_symbol_table(frame)
    };
    let shown = String::from_utf8_lossy(&name).into_owned();
    let undefined = |req: &mut Request| {
        let scope = if op.extended_value & FETCH_GLOBAL != 0 {
            "global "
        } else {
            ""
        };
        crate::api::raising::warning(req, &format!("Undefined {scope}variable ${shown}"));
    };
    let held = req.symbol_value(table, &name);
    let unset = held.is_none_or(|value| value.tag() == TypeTag::Undef);
    // `zend_fetch_this_var`: `$this` where no bucket names it.
    if unset && name == b"this" {
        if !lock {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
        }
        return fetch_this_by_name(req, frame, out, mode);
    }
    let value = match mode {
        ByName::R | ByName::Is => {
            if unset && mode == ByName::R {
                undefined(req);
            }
            let value = held
                .filter(|_| !unset)
                .map_or_else(Value::null, |held| held.deref());
            req.addref(&value);
            Some(value)
        }
        ByName::W | ByName::Rw => {
            if unset && mode == ByName::Rw {
                undefined(req);
            }
            if req.is_pending() {
                None
            } else {
                let place = match req.symbol_place(table, &name, true) {
                    Ok(place) => place,
                    Err(thrown) => return threw(req, frame, &thrown),
                };
                let Some(place) = place else {
                    return Err(Fatal::Error(format!(
                        "InternalError: a fetch by name laid ${shown} down and the table does not hold it"
                    )));
                };
                if exec::value_of(place.read()).tag() == TypeTag::Undef {
                    place.write(php_heap::Zval::null());
                }
                let previous = req.take_slot(frame, out);
                req.release(previous);
                if let Some(var) = req.slot_ptr(frame, out) {
                    var.write(php_heap::Zval::indirect(place));
                }
                if !lock {
                    exec::free_operand(req, frame, code, op.op1_type, op.op1);
                }
                return Ok(advance(req, frame));
            }
        }
    };
    if !lock {
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
    }
    let previous = req.set_slot(frame, out, value.unwrap_or_else(Value::null));
    req.release(previous);
    if req.is_pending() {
        return Ok(Stepped::Threw(frame));
    }
    Ok(advance(req, frame))
}

/// The name a fetch by name reads: the constant as it stands, a string
/// as it is, and anything else converted as `zval_try_get_tmp_string`
/// converts it, a CV nothing wrote warning first
/// (`ZVAL_UNDEFINED_OP1`).
fn fetched_name(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Result<Vec<u8>, Thrown> {
    if op.op1_type == OperandKind::Const {
        let name = exec::constant(code, ConstIdx(op.op1));
        return Ok(name.as_str_bytes().unwrap_or_default().to_vec());
    }
    let held = exec::operand_undef(req, frame, code, op.op1_type, op.op1).deref();
    if let Some(bytes) = held.as_str_bytes() {
        return Ok(bytes.to_vec());
    }
    let held = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    let text = crate::ops::to_string(req, &held)?;
    let bytes = text.as_bytes().to_vec();
    req.release(Value::string(text));
    Ok(bytes)
}

/// `zend_fetch_this_var` (`Z/Zend/zend_execute.c:4185`): `$this` read
/// by name is the receiver, with php's warning where the frame has none
/// under `BP_VAR_R`; under a write it is `Cannot re-assign $this`.
fn fetch_this_by_name(req: &mut Request, frame: FrameRef, out: u32, mode: ByName) -> Edge {
    let value = match (req.vm().this(frame), mode) {
        (_, ByName::W | ByName::Rw) => {
            let previous = req.set_slot(frame, out, Value::undef());
            req.release(previous);
            let thrown = crate::api::raising::throw(req, "Error", "Cannot re-assign $this");
            return threw(req, frame, &thrown);
        }
        (Some(this), _) => {
            let value = this.as_value();
            req.addref(&value);
            value
        }
        (None, ByName::R) => {
            crate::api::raising::warning(req, "Undefined variable $this");
            Value::null()
        }
        (None, ByName::Is) => Value::null(),
    };
    let previous = req.set_slot(frame, out, value);
    req.release(previous);
    Ok(advance(req, frame))
}

/// `ZEND_BEGIN_SILENCE` (`Z/Zend/zend_vm_def.h`, handler 57): the
/// `error_reporting` standing saved into the temporary, and the word
/// masked to `E_FATAL_ERRORS` unless it holds nothing else already,
/// `E_HAS_ONLY_FATAL_ERRORS`, so a fatal is never silenced. php marks
/// the ini entry modified beside this so `zend_ini_deactivate` puts the
/// word back at request end; the entry's value is left as it was, which
/// is why `ini_get` under the `@` reads the unmasked level, and the
/// request's ini table is dropped with the request here, so there is
/// nothing to mark.
///
/// ```php
/// function f() { var_dump(error_reporting(), ini_get("error_reporting")); return "aa"; }
/// echo @f(), "\n";
/// var_dump(error_reporting());
/// var_dump(@@error_reporting());
/// error_reporting(1);
/// echo @f(), "\n";
/// ```
/// ```expect
/// int(4437)
/// string(5) "32767"
/// aa
/// int(32767)
/// int(4437)
/// int(1)
/// string(1) "1"
/// aa
/// ```
///
/// The masked word is what the diagnostics are held to, so a warning
/// under the span is neither displayed nor logged:
///
/// ```php
/// echo @str_repeat("a", 2), "\n";
/// echo @$a; var_dump(error_reporting());
/// var_dump(@[1][5]);
/// ```
/// ```expect
/// aa
/// int(32767)
/// NULL
/// ```
pub(crate) fn begin_silence<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let standing = req.error_reporting();
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, Value::int(standing));
    req.release(previous);
    if !has_only_fatal_errors(standing) {
        req.set_error_reporting(standing & E_FATAL_ERRORS);
    }
    Ok(advance(req, frame))
}

/// `ZEND_END_SILENCE` (`Z/Zend/zend_vm_def.h`, handler 58): the word
/// restored from the temporary `BEGIN_SILENCE` saved it in, while the
/// word still holds only fatal levels and the saved one held more.
/// The first test is php's check that nothing under the span moved the
/// word: an `error_reporting()` or an `ini_set` inside the `@` set a
/// level the `@` keeps. The second is the `BEGIN_SILENCE` that masked
/// nothing, whose `END_SILENCE` restores nothing.
///
/// ```php
/// @error_reporting(30717);
/// var_dump(error_reporting());
/// var_dump(@ini_set("error_reporting", "8"), error_reporting());
/// echo @$x, "done\n";
/// ```
/// ```expect
/// int(30717)
/// string(5) "30717"
/// int(8)
/// done
/// ```
///
/// A throw under the span leaves through the unwinder, not this opline;
/// [`cleanup_live_vars`] makes the same restore for the
/// `ZEND_LIVE_SILENCE` range the compiler opened:
///
/// ```php
/// function t() { throw new Exception("x"); }
/// function u() { var_dump(error_reporting()); throw new Exception("y"); }
/// try { @$a = 1 / 0; } catch (DivisionByZeroError $e) { var_dump(error_reporting()); }
/// try { echo @t(); } catch (Exception $e) { var_dump(error_reporting()); }
/// try { @u(); } catch (Exception $e) { var_dump(error_reporting()); }
/// ```
/// ```expect
/// int(32767)
/// int(32767)
/// int(4437)
/// int(32767)
/// ```
pub(crate) fn end_silence<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let slot = exec::temporary_slot(code, op.op1);
    let saved = req.take_slot(frame, slot);
    restore_silenced(req, &saved);
    req.release(saved);
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
/// A constant a trait declares is the composing class's to hand out,
/// and read on the trait itself it is php's `Error`, through the opline
/// and through `constant()` alike:
///
/// ```php
/// trait T { const X = 1; }
/// class C { use T; }
/// echo C::X, "\n";
/// try { echo T::X; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { echo constant('T::X'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 1
/// Cannot access trait constant T::X directly
/// Cannot access trait constant T::X directly
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

/// `ZEND_FETCH_CONSTANT` (`Z/Zend/zend_vm_def.h:6075`), the opline
/// `zend_compile_const` leaves for a name it could not substitute: the
/// constant is looked up as the program serves and its value copied
/// into the temporary `result` names.
///
/// The key is the constant slot after `op2` and never `op2` itself
/// (`RT_CONSTANT(opline, opline->op2) + 1`, `zend_quick_get_constant`,
/// `Z/Zend/zend_execute.c:5411`): php writes a constant into its table
/// under the name with the namespace lowered
/// (`zend_register_constant`), and that is the spelling the compiler
/// put there. Where `op1` carries
/// `IS_CONSTANT_UNQUALIFIED_IN_NAMESPACE` the name stood unqualified
/// inside a namespace, and the slot after that one is the global
/// spelling, which php tries when the namespaced name has no constant.
/// The `Error` for a miss names the constant as the source wrote it
/// resolved, which is `op2` itself.
///
/// php primes the opline's cache slot with the constant it found; this
/// engine looks the name up on every pass, which nothing observes,
/// since a deprecated constant is the one php never caches and the
/// table it reads is the request's own.
///
/// ```php
/// define("W", 1);
/// var_dump(W, defined("W"));
/// ```
/// ```expect
/// int(1)
/// bool(true)
/// ```
///
/// A name no table has is php's `Error`:
///
/// ```php
/// var_dump(NOPE);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Undefined constant "NOPE" in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// An unqualified name inside a namespace is the namespace's own
/// constant where one is defined and the global one where none is, and
/// a name written with a leading `\` is the global one whatever the
/// namespace:
///
/// ```php
/// namespace n;
/// define("n\\K", 2);
/// define("G", 3);
/// var_dump(K, G, \PHP_EOL === PHP_EOL, \G);
/// ```
/// ```expect
/// int(2)
/// int(3)
/// bool(true)
/// int(3)
/// ```
///
/// A namespaced name the program never defined names itself in the
/// `Error`, and the global fallback is not tried for a name written
/// with its namespace:
///
/// ```php
/// namespace n;
/// define("Q", 1);
/// var_dump(n\Q);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Undefined constant "n\n\Q" in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
pub(crate) fn fetch_constant<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if op.op2_type != OperandKind::Const {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_FETCH_CONSTANT over a name no literal carries",
        )));
    }
    let key = named(code, op.op2.saturating_add(1));
    let mut found = req.constant_named(&key);
    if found.is_none() && op.op1 & UNQUALIFIED_IN_NAMESPACE != 0 {
        let global = named(code, op.op2.saturating_add(2));
        found = req.constant_named(&global);
    }
    let Some(value) = found else {
        let written = String::from_utf8_lossy(&named(code, op.op2)).into_owned();
        let thrown =
            crate::api::raising::throw(req, "Error", &format!("Undefined constant \"{written}\""));
        return threw(req, frame, &thrown);
    };
    req.addref(&value);
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, value);
    req.release(previous);
    // `if (ZEND_CONSTANT_FLAGS(c) & CONST_DEPRECATED)`
    // (`Z/Zend/zend_execute.c:5439`): the value is copied first and the
    // deprecation raised after, and the opline then advances under
    // `ZEND_VM_NEXT_OPCODE_CHECK_EXCEPTION`, so a handler that throws
    // leaves the temporary written and the throwable pending. The
    // constant a deprecation is worded from is the one the compiler
    // refused to substitute (`can_ct_eval_const`), so the scan of the
    // declaration table is a scan this opline alone makes.
    crate::api::raising::deprecated_constant(req, &key);
    Ok(advance(req, frame))
}

/// The bytes of the constant at `at`, which is a name where the opline
/// carries one.
fn named(code: &OpArray, at: u32) -> Vec<u8> {
    exec::constant(code, ConstIdx(at))
        .as_str_bytes()
        .unwrap_or_default()
        .to_vec()
}

/// `ZEND_FETCH_STATIC_PROP_R` (`Z/Zend/zend_vm_def.h`, handler 172)
/// over a property and a class both named at compile time, which is
/// what `zend_compile_static_prop` emits for `C::$p`:
/// `ZEND_FETCH_STATIC_PROP_IS` (`Z/Zend/zend_vm_def.h:1928`), which
/// dispatches to `zend_fetch_static_prop_helper` with `BP_VAR_IS`, a
/// link of a chain an `isset` or a `??` reads: [`fetch_static_prop_r`]'s
/// body under [`PropertyRead::Silent`], which answers `null` where the
/// read that raises would have thrown. A property no class of the chain
/// declares, one declared as an instance property, one the scope may
/// not reach and a typed slot nothing has written are each `null` with
/// nothing thrown, the four lines php guards with `type != BP_VAR_IS`.
///
/// The class is fetched `ZEND_FETCH_CLASS_EXCEPTION` whatever the type,
/// so a class nothing declares is still php's `Error` here.
///
/// ```php
/// class C { public static $s = 1; private static $q = 2; protected static $r = 3; public $i = 4; public static $n = null; public static int $t; }
/// var_dump(C::$s ?? 'd', C::$q ?? 'd', C::$r ?? 'd', C::$i ?? 'd', C::$nope ?? 'd', C::$n ?? 'd', C::$t ?? 'd');
/// ```
/// ```expect
/// int(1)
/// string(1) "d"
/// string(1) "d"
/// string(1) "d"
/// string(1) "d"
/// string(1) "d"
/// string(1) "d"
/// ```
///
/// The read that raises throws over the same class, which is what the
/// silent one is for.
///
/// ```php
/// class C { public static $s = 1; }
/// var_dump(C::$s);
/// var_dump(C::$nope);
/// ```
/// ```expect exit=255
/// int(1)
///
/// Fatal error: Uncaught Error: Access to undeclared static property C::$nope in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
///
/// ```php
/// var_dump(Missing::$x ?? 'd');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Class "Missing" not found in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn fetch_static_prop_is<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    fetch_static_prop(req, frame, code, op, PropertyRead::Silent)
}

/// `zend_fetch_static_property_address_ex`
/// (`Z/Zend/zend_execute.c:3715`) with the property's name in `op1` and
/// the class's constant pair in `op2`. The class is `Class "X" not
/// found` where nothing declares it; the slot is
/// `zend_std_get_static_property_with_info`'s
/// ([`Request::static_property`]), which lays it down from the
/// declaration's default on first use, holds the read to the executed
/// scope, and refuses a typed slot nothing has written. What is read is
/// a temporary with one more owner of the value, a reference followed.
///
/// php reserves three cache slots here and, where `op1` is a constant,
/// leaves the first empty and primes the second with the address of the
/// slot itself (`CACHE_POLYMORPHIC_PTR`). This engine's table names a
/// slot by its class and its name rather than by an address, so nothing
/// is cached and the lookup is made on every pass, which nothing
/// observes.
///
/// ```php
/// class A { public static $p = 1; protected static $q = 2; private static $r = 3; static function all() { return A::$p + A::$q + A::$r; } }
/// class B extends A {}
/// var_dump(A::$p, B::$p, A::all());
/// ```
/// ```expect
/// int(1)
/// int(1)
/// int(6)
/// ```
///
/// ```php
/// class A { private static $r = 3; }
/// echo A::$r;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot access private property A::$r in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// class A { public $p = 1; }
/// echo A::$p;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Access to undeclared static property A::$p in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// class A { public static int $n; }
/// var_dump(A::$n);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Typed static property A::$n must not be accessed before initialization in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn fetch_static_prop_r<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    fetch_static_prop(req, frame, code, op, PropertyRead::Warning)
}

/// The body `FETCH_STATIC_PROP_R` and `FETCH_STATIC_PROP_IS` share,
/// which is php's one body under two fetch types: the class off the
/// constant pair in `op2`, the property's name off `op1`, and
/// [`Request::static_property`] over the executed scope under the fetch
/// type given.
/// `ZEND_FETCH_STATIC_PROP_W` (`Z/Zend/zend_vm_def.h:1857`), the base
/// of a dimension written over a `static` property: the slot addressed
/// as a place (`zend_fetch_static_property_address` under `BP_VAR_W`,
/// [`Request::static_property_place`]) and an `IS_INDIRECT` naming it
/// written into the opline's `VAR`, which is what the `ASSIGN_DIM` over
/// it writes through. The compiler marks the opline
/// `ZEND_FETCH_DIM_WRITE` where a dimension stands over it, which is
/// what the dump prints as `(dim write)`.
///
/// ```php
/// class C { public static $s = [1]; public static $n; }
/// C::$s[0] = 2;
/// C::$s[] = 3;
/// var_dump(C::$s);
/// C::$n[1] = 4;
/// var_dump(C::$n);
/// try { C::$nope[0] = 5; } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(2)
///   [1]=>
///   int(3)
/// }
/// array(1) {
///   [1]=>
///   int(4)
/// }
/// Error: Access to undeclared static property C::$nope
/// ```
pub(crate) fn fetch_static_prop_w<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    static_prop_place(req, frame, code, op, false)
}

/// `ZEND_FETCH_STATIC_PROP_RW` (`Z/Zend/zend_vm_def.h:1906`), the base
/// of a compound assignment or a `++` over an element of a `static`
/// property: [`fetch_static_prop_w`]'s place, with the read's refusal
/// of a typed slot nothing has written
/// (`zend_std_get_static_property_with_info` under `BP_VAR_RW`).
///
/// ```php
/// class C { public static $s = [1]; public static array $t; }
/// C::$s[0] += 2;
/// C::$s[1]++;
/// var_dump(C::$s);
/// try { C::$t[0]++; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Warning: Undefined array key 1 in %s on line 4
/// array(2) {
///   [0]=>
///   int(3)
///   [1]=>
///   int(1)
/// }
/// Typed static property C::$t must not be accessed before initialization
/// ```
pub(crate) fn fetch_static_prop_rw<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    static_prop_place(req, frame, code, op, true)
}

/// The body `FETCH_STATIC_PROP_W` and `FETCH_STATIC_PROP_RW` share:
/// the slot as a place, an `IS_INDIRECT` naming it in the `VAR`.
fn static_prop_place(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    rw: bool,
) -> Edge {
    if op.op1_type != OperandKind::Const {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_FETCH_STATIC_PROP_W over a property named at run time",
        )));
    }
    let class = match class_operand(req, frame, code, op.op2_type, op.op2, None) {
        Ok(class) => class,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let name = exec::constant(code, ConstIdx(op.op1));
    let name = name.as_str_bytes().unwrap_or_default().to_vec();
    let scope = req.executed_scope();
    if rw {
        match req.static_property(class, &name, scope, PropertyRead::Warning) {
            Ok(held) => req.release(held),
            Err(thrown) => return threw(req, frame, &thrown),
        }
    }
    let place = match req.static_property_place(class, &name, scope) {
        Ok(Some(place)) => place,
        Ok(None) => {
            return Err(Fatal::Error(format!(
                "InternalError: ZEND_FETCH_STATIC_PROP_W placed ${} and the table holds no slot for it",
                String::from_utf8_lossy(&name)
            )));
        }
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let out = exec::temporary_slot(code, op.out);
    let previous = req.take_slot(frame, out);
    req.release(previous);
    let Some(var) = req.slot_ptr(frame, out) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_FETCH_STATIC_PROP_W writes a VAR the frame does not have",
        )));
    };
    var.write(php_heap::Zval::indirect(place));
    Ok(advance(req, frame))
}

fn fetch_static_prop(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    fetch: PropertyRead,
) -> Edge {
    if op.op1_type != OperandKind::Const {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_FETCH_STATIC_PROP_R over a property named at run time",
        )));
    }
    let class = match class_operand(req, frame, code, op.op2_type, op.op2, None) {
        Ok(class) => class,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let name = exec::constant(code, ConstIdx(op.op1));
    let name = name.as_str_bytes().unwrap_or_default().to_vec();
    let scope = req.executed_scope();
    let value = match req.static_property(class, &name, scope, fetch) {
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
    fetch_obj(req, frame, code, op, PropertyRead::Warning)
}

/// `ZEND_FETCH_OBJ_IS` (`Z/Zend/zend_vm_def.h:2276`), a link of a chain
/// an `isset` or a `??` reads: [`fetch_obj_r`]'s body under
/// `BP_VAR_IS` ([`PropertyRead::Silent`]), which reads `null` where the
/// read that warns would have warned or thrown. A container that is no
/// object is `null` with nothing raised, and so is a property the class
/// has not got, one the scope may not reach, and a typed slot nothing
/// has written.
///
/// ```php
/// class C { public $p = 1; private $q = 2; protected $r = 3; public $n = null; }
/// $o = new C; $i = 5; $u = null; $a = ['k' => 7];
/// var_dump($o->p ?? 'd', $o->q ?? 'd', $o->r ?? 'd', $o->n ?? 'd', $o->missing ?? 'd');
/// var_dump($i->p ?? 'd', $u->p ?? 'd', $a->p ?? 'd', $undef->p ?? 'd', $o->p->q ?? 'd');
/// var_dump($a['k'] ?? 'd', $a['missing'] ?? 'd', $a['k']['z'] ?? 'd');
/// ```
/// ```expect
/// int(1)
/// string(1) "d"
/// string(1) "d"
/// string(1) "d"
/// string(1) "d"
/// string(1) "d"
/// string(1) "d"
/// string(1) "d"
/// string(1) "d"
/// string(1) "d"
/// int(7)
/// string(1) "d"
/// string(1) "d"
/// ```
///
/// The read that warns raises four diagnostics over the same program,
/// which is what the silent chain is for.
///
/// ```php
/// class C { public $p = 1; }
/// $o = new C; $i = 5; $a = ['k' => 7];
/// var_dump($o->missing);
/// var_dump($i->p);
/// var_dump($a['missing']);
/// ```
/// ```expect
/// Warning: Undefined property: C::$missing in %s on line 4
/// NULL
///
/// Warning: Attempt to read property "p" on int in %s on line 5
/// NULL
///
/// Warning: Undefined array key "missing" in %s on line 6
/// NULL
/// ```
pub(crate) fn fetch_obj_is<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    fetch_obj(req, frame, code, op, PropertyRead::Silent)
}

/// The body `FETCH_OBJ_R` and `FETCH_OBJ_IS` share, which is php's one
/// handler under two fetch types: the property's name off the constant
/// table, the container read as the fetch type reads it, the
/// `read_property` slot over an object and, over anything else, the
/// warning `BP_VAR_R` raises and `BP_VAR_IS` does not; the container
/// freed after it (`FREE_OP1`) and `null` or the value written into the
/// temporary.
fn fetch_obj(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    fetch: PropertyRead,
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
    // `GET_OP1_OBJ_ZVAL_PTR(BP_VAR_R)` warns of a variable nothing
    // wrote; the `BP_VAR_IS` reader takes the same slot as the `UNDEF`
    // it is and says nothing.
    let container = match fetch {
        PropertyRead::Warning => exec::operand(req, frame, code, op.op1_type, op.op1),
        PropertyRead::Silent => exec::operand_undef(req, frame, code, op.op1_type, op.op1),
    }
    .deref();
    let read = if let Some(object) = container.as_object().copied() {
        let table = req.object_handlers(object.heap());
        table.read_property(req, object, &name, fetch)
    } else {
        if fetch == PropertyRead::Warning {
            crate::api::raising::warning(
                req,
                &format!(
                    "Attempt to read property \"{}\" on {}",
                    String::from_utf8_lossy(&name),
                    crate::ops::value_name(req, &container)
                ),
            );
        }
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
    fetch_obj_place(req, frame, code, op, PropertyRead::Silent)
}

/// `ZEND_FETCH_OBJ_RW` (`Z/Zend/zend_vm_def.h:2072`), the property
/// fetch under a compound assignment's last link: [`fetch_obj_w`]'s
/// body with the read's own diagnostics in it, which is what `_RW` adds
/// to `_W`. `zend_std_get_property_ptr_ptr` under `BP_VAR_RW` raises
/// `Undefined property` for a declared slot nothing has written and
/// leaves it holding `null`, the uninitialised-access `Error` for a
/// typed one, and the same warning after the creation's deprecation for
/// a name no class declared ([`Request::dynamic_property_ref`]).
///
/// ```php
/// class C { public $p; public int $t; }
/// $i = new C;
/// unset($i->p);
/// $o = new C;
/// $o->p = $i;
/// $o->p->p += 1;
/// var_dump($o->p->p);
/// try { $o->t->q += 1; } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// $u = new C;
/// try { $u->nope->q += 1; } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Warning: Undefined property: C::$p in %s on line 7
/// int(1)
/// Error: Typed property C::$t must not be accessed before initialization
///
/// Deprecated: Creation of dynamic property C::$nope is deprecated in %s on line 11
///
/// Warning: Undefined property: C::$nope in %s on line 11
/// Error: Attempt to assign property "q" on null
/// ```
pub(crate) fn fetch_obj_rw<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    fetch_obj_place(req, frame, code, op, PropertyRead::Warning)
}

/// The body `FETCH_OBJ_W` and `FETCH_OBJ_RW` share, php's one
/// `zend_fetch_property_address` under two `BP_VAR_*`: `read` is
/// `BP_VAR_RW`'s own diagnostics, which `BP_VAR_W` lays the place down
/// without.
fn fetch_obj_place(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    read: PropertyRead,
) -> Edge {
    let at = match property_place(req, frame, code, op, read)? {
        PropertyPlace::At(at, _, _) => at,
        PropertyPlace::Threw(thrown) => return threw(req, frame, &thrown),
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
    exec::free_container_var(req, frame, code, op);
    Ok(advance(req, frame))
}

/// What [`property_place`] found: the property's place, the object and
/// the name, or the throwable, the container given up before it.
enum PropertyPlace {
    At(php_heap::ZvalRef, PhpObject, Vec<u8>),
    Threw(Thrown),
}

/// `zend_fetch_property_address` (`Z/Zend/zend_execute.c`) under
/// `BP_VAR_W` or `BP_VAR_RW`, the place `FETCH_OBJ_W`, `FETCH_OBJ_RW`
/// and `ASSIGN_OBJ_REF` write through: the object `op1` holds and the
/// slot or the dynamic entry of the name in `op2`.
fn property_place(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    read: PropertyRead,
) -> Result<PropertyPlace, Fatal> {
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
        return Ok(PropertyPlace::Threw(thrown));
    };
    // php places the name here with `zend_std_get_property_ptr_ptr`,
    // whose silence is `ce->__get != NULL`; the overloaded arm a class
    // with a `__get` takes, php's `Indirect modification of overloaded
    // property` notice, is not served yet (#513), so the placement is
    // loud and a property the scope may not reach is the `Error`.
    let placed = req.declared_slot_for_write(object.class(), &name, "indirectly modify", false);
    let at = match placed {
        Ok(Some((slot, typed))) => {
            // `if (UNEXPECTED(Z_TYPE_P(retval) == IS_UNDEF))`: under
            // `BP_VAR_RW` a slot nothing has written raises the read's
            // own diagnostic and php then leaves it holding `null`.
            if read == PropertyRead::Warning && object.heap().slot(slot).is_undef() {
                let shown = String::from_utf8_lossy(&name).into_owned();
                let class = req.class_name(object.class());
                if typed {
                    let thrown = crate::api::raising::throw(
                        req,
                        "Error",
                        &format!(
                            "Typed property {class}::${shown} must not be accessed before initialization"
                        ),
                    );
                    return Ok(PropertyPlace::Threw(freeing_container(
                        req, frame, code, op, thrown,
                    )));
                }
                crate::api::raising::warning(
                    req,
                    &format!("Undefined property: {class}::${shown}"),
                );
                if let Err(thrown) = req.write_declared_slot(object.heap(), slot, Value::null()) {
                    return Ok(PropertyPlace::Threw(freeing_container(
                        req, frame, code, op, thrown,
                    )));
                }
            }
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
            // `zend_std_get_property_ptr_ptr`: under `BP_VAR_W` the
            // entry is laid down with the creation's diagnostics and no
            // read's warning, and under `BP_VAR_RW` with the warning
            // after them.
            match req.dynamic_property_ref(object, key, read) {
                Ok(Some(at)) => at,
                Ok(None) => {
                    return Err(Fatal::Error(format!(
                        "InternalError: ZEND_FETCH_OBJ_W laid the dynamic property ${} down and the table does not hold it",
                        String::from_utf8_lossy(&name)
                    )));
                }
                Err(thrown) => {
                    return Ok(PropertyPlace::Threw(freeing_container(
                        req, frame, code, op, thrown,
                    )));
                }
            }
        }
        Err(thrown) => {
            return Ok(PropertyPlace::Threw(freeing_container(
                req, frame, code, op, thrown,
            )));
        }
    };
    Ok(PropertyPlace::At(at, object, name))
}

/// A throw out of [`property_place`] once the object is found: the
/// container given up as `FREE_OP1_VAR_PTR` gives it up, with no place
/// written to extract, before the throw.
fn freeing_container(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    thrown: Thrown,
) -> Thrown {
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    thrown
}

/// `ZEND_FETCH_DIM_R` (`Z/Zend/zend_vm_def.h:1933`), `$a[k]` where a
/// value is wanted, and `zend_fetch_dimension_address_read` under
/// `BP_VAR_R` (`Z/Zend/zend_execute.c:3056`) below it. The container is
/// read as `GET_OP1_ZVAL_PTR_UNDEF(BP_VAR_R)` reads it, warning of
/// nothing there, and a reference it holds is followed once; then the
/// four arms php has, each of which reads the dimension for itself, so
/// an undefined variable in the container warns before one in the
/// dimension does.
///
/// An array is [`array_entry`]: the entry at the key the ladder
/// [`crate::ops::array_offset`] makes, copied with a reference it holds
/// followed (`ZVAL_COPY_DEREF`), and php's `Undefined array key`
/// warning and `null` where the table has none (`zend_undefined_offset`
/// and `zend_undefined_index`). A string is [`string_offset`], the byte
/// at the offset. An object is `zend_std_read_dimension`, `offsetGet`
/// on an `ArrayAccess` ([`Request::read_dimension`]) and php's `Error`
/// `Cannot use object of type C as array` on a class that is not one.
/// Anything else is php's warning `Trying to access array offset on
/// null` over what `zend_zval_value_name` names it, and `null`.
///
/// Both operands are freed after the value is the handler's own
/// (`FREE_OP2`, `FREE_OP1`), so a chain frees each link's temporary
/// once the link over it has read through it.
///
/// ```php
/// $a = [1, 'k' => 2];
/// var_dump($a[0], $a['k'], $a[9], $a['nope']);
/// ```
/// ```expect
/// Warning: Undefined array key 9 in %s on line 3
///
/// Warning: Undefined array key "nope" in %s on line 3
/// int(1)
/// int(2)
/// NULL
/// NULL
/// ```
///
/// A chain reads each link through the one under it, a reference the
/// container holds is followed, and a reference an entry holds is
/// followed as the value is copied.
///
/// ```php
/// $u = [[1, 2], [3, 4]];
/// $r = &$u;
/// $x = 5; $u[0][0] = &$x;
/// var_dump($u[1][0], $r[0][0], $u[$k]);
/// ```
/// ```expect
/// Warning: Undefined variable $k in %s on line 5
///
/// Deprecated: Using null as an array offset is deprecated, use an empty string instead in %s on line 5
///
/// Warning: Undefined array key "" in %s on line 5
/// int(3)
/// int(5)
/// NULL
/// ```
///
/// What is neither an array nor a string warns once for the container
/// and once for the dimension, in that order, and reads `null`.
///
/// ```php
/// var_dump($z[$k]);
/// $n = 1; $f = 1.5; $b = false;
/// var_dump($n[0], $f[0], $b[0], $nn[0]);
/// ```
/// ```expect
/// Warning: Undefined variable $z in %s on line 2
///
/// Warning: Undefined variable $k in %s on line 2
///
/// Warning: Trying to access array offset on null in %s on line 2
/// NULL
///
/// Warning: Trying to access array offset on int in %s on line 4
///
/// Warning: Trying to access array offset on float in %s on line 4
///
/// Warning: Trying to access array offset on false in %s on line 4
///
/// Warning: Undefined variable $nn in %s on line 4
///
/// Warning: Trying to access array offset on null in %s on line 4
/// NULL
/// NULL
/// NULL
/// NULL
/// ```
///
/// A dimension that is no key is php's `TypeError`.
///
/// ```php
/// $a = [1];
/// var_dump($a[[]]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Cannot access offset of type array on array in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// An object is `offsetGet` with the dimension as it stands, and a class
/// that is no `ArrayAccess` is php's `Error`.
///
/// ```php
/// class C implements ArrayAccess {
///     public function offsetExists($offset): bool { return true; }
///     public function offsetGet($offset): mixed { return $offset; }
///     public function offsetSet($offset, $value): void {}
///     public function offsetUnset($offset): void {}
/// }
/// $c = new C;
/// var_dump($c[3], $c["k"]);
/// $o = new stdClass;
/// var_dump($o[0]);
/// ```
/// ```expect exit=255
/// int(3)
/// string(1) "k"
///
/// Fatal error: Uncaught Error: Cannot use object of type stdClass as array in %s:11
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 11
/// ```
pub(crate) fn fetch_dim_r<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let container = exec::operand_undef(req, frame, code, op.op1_type, op.op1).deref();
    let read = read_dimension(req, frame, code, op, &container, DimRead::Warning)?;
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
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

/// `ZEND_FETCH_DIM_IS` (`Z/Zend/zend_vm_def.h`), a link of a chain an
/// `isset`, an `empty` or a `??` reads: [`fetch_dim_r`]'s body under
/// `BP_VAR_IS` ([`DimRead::Silent`]), which reads `null` where the read
/// that warns would have warned. A key the table has not got, an offset
/// past the end of a string, a string offset that is no number, and a
/// container that is neither an array, a string nor an object are all
/// `null` with nothing raised; an offset whose type is no key at all is
/// still php's `TypeError`, since `slow_index_convert` and
/// `zend_illegal_string_offset` raise it under either read.
///
/// ```php
/// $a = [[1], 'k' => "ab"];
/// var_dump(isset($a[9][8]), isset($a[0][0]), isset($a['k'][1]), isset($a['k'][9]));
/// var_dump(empty($a[9][8]), empty($a[0][0]), isset($a[0][9]));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
pub(crate) fn fetch_dim_is<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let container = exec::operand_undef(req, frame, code, op.op1_type, op.op1).deref();
    let read = read_dimension(req, frame, code, op, &container, DimRead::Silent)?;
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
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

/// `ZEND_FETCH_LIST_R` (`Z/Zend/zend_vm_def.h:2431`), one element of a
/// `list()` or a `[…]` on the left of an `=` or in a `foreach`:
/// `zend_fetch_dimension_address_LIST_r`, which is the read of
/// [`fetch_dim_r`] with `is_list` set. An array and an object are read
/// as `FETCH_DIM_R` reads them; a string is no container here, so it
/// falls with every other value to the last arm, where what is more than
/// `null` warns `Cannot use int as array` in place of the read's
/// `Trying to access array offset`, and the element is `null`. The
/// container is not freed: the compiler reads it once per element and
/// frees it after the last (`zend_do_free`'s `FETCH_LIST_*` arm).
///
/// ```php
/// [$a, [$b, $c]] = [1, [2, 3]];
/// ['k' => $d, 'm' => $e] = ['k' => 4];
/// [$f, $g] = "ab";
/// [$h] = 5;
/// [$i] = null;
/// list(, $j) = [1, 2];
/// var_dump($a, $b, $c, $d, $e, $f, $g, $h, $i, $j);
/// ```
/// ```expect
/// Warning: Undefined array key "m" in %s on line 3
///
/// Warning: Cannot use string as array in %s on line 4
///
/// Warning: Cannot use string as array in %s on line 4
///
/// Warning: Cannot use int as array in %s on line 5
/// int(1)
/// int(2)
/// int(3)
/// int(4)
/// NULL
/// NULL
/// NULL
/// NULL
/// NULL
/// int(2)
/// ```
pub(crate) fn fetch_list_r<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    list_read(req, frame, code, op)
}

/// The body of [`fetch_list_r`], and of [`fetch_list_w`] over what is
/// no reference: the element read into the `VAR`, `FREE_OP2` and never
/// `FREE_OP1`.
fn list_read(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let container = exec::operand_undef(req, frame, code, op.op1_type, op.op1).deref();
    let read = if container.as_array().is_some() || container.as_object().is_some() {
        read_dimension(req, frame, code, op, &container, DimRead::Warning)?
    } else {
        // `ZVAL_UNDEFINED_OP1` and `ZVAL_UNDEFINED_OP2`, then the list's
        // own warning over what is more than `null`.
        let _container = exec::operand(req, frame, code, op.op1_type, op.op1);
        let _dim = exec::operand(req, frame, code, op.op2_type, op.op2);
        if !matches!(container.as_ref(), ValueRef::Undef | ValueRef::Null) {
            let named = crate::ops::type_name(req, &container);
            crate::api::raising::warning(req, &format!("Cannot use {named} as array"));
        }
        Ok(Value::null())
    };
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    let value = match read {
        Ok(value) => value,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, value);
    req.release(previous);
    Ok(advance(req, frame))
}

/// `ZEND_FETCH_LIST_W` (`Z/Zend/zend_vm_def.h:2443`), an element of a
/// list taken by reference, `[&$a] = $x`: the container is the `VAR`
/// the compiler's `MAKE_REF` left, and the element is fetched for
/// writing as [`fetch_dim_w`] fetches it, so the `MAKE_REF` after it
/// binds the variable to the array's own cell. A `VAR` holding neither
/// a reference nor a place, what a call returned by value, is php's
/// notice and the read of [`fetch_list_r`]. The container stays: the
/// compiler frees it after the last element.
///
/// ```php
/// $x = [1, [2]];
/// [&$a, [&$b]] = $x;
/// $a = 'a'; $b = 'b';
/// var_dump($x);
/// function f() { return [1]; }
/// [&$d] = f();
/// var_dump($d);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   &string(1) "a"
///   [1]=>
///   array(1) {
///     [0]=>
///     &string(1) "b"
///   }
/// }
///
/// Notice: Attempting to set reference to non referenceable value in %s on line 7
/// int(1)
/// ```
pub(crate) fn fetch_list_w<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if op.op1_type == OperandKind::Var {
        let held = req
            .slot_ptr(frame, exec::temporary_slot(code, op.op1))
            .map(php_heap::ZvalRef::read);
        let placed = held
            .is_some_and(|z| z.as_indirect().is_some() || exec::value_of(z).as_php_ref().is_some());
        if !placed {
            crate::api::raising::notice(
                req,
                "Attempting to set reference to non referenceable value",
            );
            return list_read(req, frame, code, op);
        }
    }
    fetch_dim_place(req, frame, code, op, DimRead::Silent, Keep::Container)
}

/// php's `type` argument to `zend_fetch_dimension_address_read`, of
/// which the two reads use two values: `BP_VAR_R`, which warns of a key
/// the table has not got, an offset past the end of a string and a
/// container that is neither; and `BP_VAR_IS`, which warns of none of
/// the three and reads `null` instead, since what asks is an `isset` or
/// a `??` and the answer is the question.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DimRead {
    Warning,
    Silent,
}

/// The four arms of `zend_fetch_dimension_address_read`, the container
/// already dereferenced. Each reads the dimension itself, as php reads
/// it inside the arm it took: `ZVAL_UNDEFINED_OP2` sits in
/// `slow_index_convert` for an array, in `try_string_offset` for a
/// string, and after `ZVAL_UNDEFINED_OP1` in the arm that has neither.
/// That second warning is raised under both reads; the first and the
/// `Trying to access array offset` beside it are `BP_VAR_R`'s alone.
fn read_dimension(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    container: &Value,
    mode: DimRead,
) -> Result<Result<Value, Thrown>, Fatal> {
    if let Some(array) = container.as_array().copied() {
        let dim = exec::operand(req, frame, code, op.op2_type, op.op2);
        let key = match crate::ops::array_offset(req, &dim, op.op2_type == OperandKind::Const) {
            Ok(key) => key,
            Err(thrown) => return Ok(Err(thrown)),
        };
        return Ok(Ok(array_entry(req, array, key, mode)));
    }
    if let Some(text) = container.as_php_string().copied() {
        let dim = exec::operand(req, frame, code, op.op2_type, op.op2);
        return Ok(string_offset(req, text, &dim, mode));
    }
    if let Some(object) = container.as_object().copied() {
        let dim = object_dim(req, frame, code, op);
        return Ok(match mode {
            DimRead::Warning => {
                let table = req.object_handlers(object.heap());
                table.read_dimension(req, object, dim.as_ref())
            }
            DimRead::Silent => req.read_dimension_is(object, dim.as_ref()),
        });
    }
    if mode == DimRead::Silent {
        // `if (type != BP_VAR_IS && Z_TYPE_P(container) == IS_UNDEF)`:
        // the container's own read raises nothing here, and the
        // dimension's still does.
        let _dim = exec::operand(req, frame, code, op.op2_type, op.op2);
        return Ok(Ok(Value::null()));
    }
    let named = crate::ops::value_name(req, &container.undef_as_null());
    // `ZVAL_UNDEFINED_OP1` and `ZVAL_UNDEFINED_OP2`: the warning each
    // operand raises where it is an unwritten variable, in php's order.
    let _container = exec::operand(req, frame, code, op.op1_type, op.op1);
    let _dim = exec::operand(req, frame, code, op.op2_type, op.op2);
    crate::api::raising::warning(req, &format!("Trying to access array offset on {named}"));
    Ok(Ok(Value::null()))
}

/// `zend_fetch_dimension_address_inner` under `BP_VAR_R`
/// (`Z/Zend/zend_execute.c:2802`): the entry at `key`, copied with one
/// more owner and a reference it holds followed (`ZVAL_COPY_DEREF`);
/// php's `Undefined array key` warning and `null` where the table has
/// none, the key rendered as `zend_undefined_offset` renders a long and
/// `zend_undefined_index` a string.
fn array_entry(req: &mut Request, array: PhpArray, key: php_heap::Key, mode: DimRead) -> Value {
    if let Some(found) = array.heap().get(key) {
        let value = exec::value_of(found).deref();
        req.addref(&value);
        return value;
    }
    if mode == DimRead::Silent {
        return Value::null();
    }
    let message = match key {
        php_heap::Key::Long(n) => format!("Undefined array key {n}"),
        php_heap::Key::Str(s) => format!(
            "Undefined array key \"{}\"",
            String::from_utf8_lossy(s.bytes())
        ),
    };
    crate::api::raising::warning(req, &message);
    Value::null()
}

/// The dimension a handler over an object reads, php's `if (OP2_TYPE ==
/// IS_CONST && Z_EXTRA_P(dim) == ZEND_EXTRA_VALUE) dim++`
/// (`Z/Zend/zend_vm_def.h:6822`): the string as written where
/// `zend_handle_numeric_dim` folded a constant dimension to a long
/// ([`OpArray::dimension_written`]), so `$o['10']` hands `offsetGet` the
/// string `"10"` where `$a['10']` is the key `10` (bug #63217); the
/// operand as it stands everywhere else. `None` is the `UNUSED` operand
/// of `$o[]`, which php hands over as `null`.
///
/// Three handlers read the pair, `offsetGet`, `offsetSet` and
/// `offsetUnset`, and an array reads the long all three fold to.
///
/// ```php
/// class C implements ArrayAccess {
///     public function offsetExists($offset): bool { return true; }
///     public function offsetGet($offset): mixed { var_dump($offset); return 0; }
///     public function offsetSet($offset, $value): void { var_dump($offset); }
///     public function offsetUnset($offset): void { var_dump($offset); }
/// }
/// $c = new C;
/// $c["10"] = 1;
/// $c[10] = 1;
/// $c["k"] = 1;
/// var_dump($c["10"]);
/// unset($c["10"]);
/// unset($c[10]);
/// $a = ["10" => 5];
/// unset($a["10"]);
/// var_dump($a);
/// ```
/// ```expect
/// string(2) "10"
/// int(10)
/// string(1) "k"
/// string(2) "10"
/// int(0)
/// string(2) "10"
/// int(10)
/// array(0) {
/// }
/// ```
fn object_dim(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Option<Value> {
    if op.op2_type == OperandKind::Unused {
        return None;
    }
    if op.op2_type == OperandKind::Const {
        return Some(
            code.dimension_written(ConstIdx(op.op2))
                .map_or_else(Value::null, exec::value_of),
        );
    }
    Some(exec::operand(req, frame, code, op.op2_type, op.op2))
}

/// `zend_check_string_offset` (`Z/Zend/zend_execute.c`), which
/// `try_string_offset` runs over the same set: the offset php reads out
/// of a dimension over a string, for a read and for a write alike.
///
/// A long is itself, and a reference is followed once. A string is
/// `is_numeric_string_ex` with php's errors allowed: an integer is the
/// offset, with php's warning `Illegal string offset` where anything but
/// whitespace follows it, and everything else, a written float among
/// them, is `zend_illegal_string_offset`'s `TypeError`. `null`, a bool, a
/// float and an unwritten variable warn `String offset cast occurred` and
/// then convert, the float silently since the warning has been raised
/// ([`crate::ops::dval_to_lval_silent`]). An array, an object and a
/// resource are the `TypeError`.
fn string_dim_offset(req: &mut Request, dim: &Value, mode: DimRead) -> Result<Option<i64>, Thrown> {
    let dim = dim.deref();
    let illegal = |req: &mut Request, dim: &Value| {
        crate::api::raising::throw(
            req,
            "TypeError",
            &format!("Cannot access offset of type {} on string", dim.type_name()),
        )
    };
    match dim.as_ref() {
        ValueRef::Int(n) => Ok(Some(n)),
        ValueRef::Str(s) => match php_heap::numeric_prefix(s.as_bytes()) {
            Some((php_heap::Number::Long(n), trailing)) => {
                if trailing {
                    crate::api::raising::warning(
                        req,
                        &format!(
                            "Illegal string offset \"{}\"",
                            String::from_utf8_lossy(s.as_bytes())
                        ),
                    );
                }
                Ok(Some(n))
            }
            // `if (type == BP_VAR_IS) { ZVAL_NULL(result); return; }`,
            // the one arm that stands before `zend_illegal_string_offset`.
            _ if mode == DimRead::Silent => Ok(None),
            _ => Err(illegal(req, &dim)),
        },
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(_) | ValueRef::Float(_) => {
            if mode == DimRead::Warning {
                crate::api::raising::warning(req, "String offset cast occurred");
            }
            Ok(Some(match dim.as_ref() {
                ValueRef::Float(f) => crate::ops::dval_to_lval_silent(f),
                ValueRef::Bool(true) => 1,
                _ => 0,
            }))
        }
        ValueRef::Array(_)
        | ValueRef::Object(_)
        | ValueRef::Resource(_)
        | ValueRef::Reference(_) => Err(illegal(req, &dim)),
    }
}

/// `try_string_offset` and the `ZVAL_CHAR` after it
/// (`zend_fetch_dimension_address_read`, `Z/Zend/zend_execute.c:3073`):
/// the offset php reads out of the dimension, and the one byte at it as
/// a string of one byte.
///
/// A long is itself. A string is `is_numeric_string_ex` with php's
/// errors allowed: an integer is the offset, with php's warning `Illegal
/// string offset` where anything but whitespace follows it, and
/// everything else, a written float among them, is
/// `zend_illegal_string_offset`'s `TypeError`. `null`, a bool, a float
/// and an unwritten variable warn `String offset cast occurred` and then
/// convert, the float silently since the warning has been raised
/// ([`crate::ops::dval_to_lval_silent`]). An array, an object and a
/// resource are the `TypeError`.
///
/// A negative offset counts from the end. An offset the string is not
/// long enough for is php's warning `Uninitialized string offset` and
/// the empty string.
///
/// ```php
/// $s = "abc";
/// var_dump($s[1], $s[-1], $s["1"], $s[1.7], $s[5], $s["0x1"]);
/// ```
/// ```expect
/// Warning: String offset cast occurred in %s on line 3
///
/// Warning: Uninitialized string offset 5 in %s on line 3
///
/// Warning: Illegal string offset "0x1" in %s on line 3
/// string(1) "b"
/// string(1) "c"
/// string(1) "b"
/// string(1) "b"
/// string(0) ""
/// string(1) "a"
/// ```
///
/// A dimension that is no offset php reads is its `TypeError`.
///
/// ```php
/// $s = "abc";
/// var_dump($s["x"]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Cannot access offset of type string on string in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
fn string_offset(
    req: &mut Request,
    text: PhpString,
    dim: &Value,
    mode: DimRead,
) -> Result<Value, Thrown> {
    let Some(offset) = string_dim_offset(req, dim, mode)? else {
        return Ok(Value::null());
    };
    let len = i64::try_from(text.len()).unwrap_or(i64::MAX);
    let at = if offset < 0 {
        len.saturating_add(offset)
    } else {
        offset
    };
    if at < 0 || at >= len {
        // `if (type != BP_VAR_IS)`: the warning and the empty string are
        // the read's, and `null` with nothing raised is the other's.
        if mode == DimRead::Silent {
            return Ok(Value::null());
        }
        crate::api::raising::warning(req, &format!("Uninitialized string offset {offset}"));
        return Ok(Value::string(PhpString::new(req, b"")?));
    }
    let byte = text
        .as_bytes()
        .get(usize::try_from(at).unwrap_or(0))
        .copied()
        .unwrap_or(0);
    Ok(Value::string(PhpString::new(req, &[byte])?))
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

/// `ZEND_FETCH_DIM_REF`, `ZEND_FETCH_DIM_DIM` and `ZEND_FETCH_DIM_OBJ`
/// (`Z/Zend/zend_compile.h:1095`), the `extended_value` values the
/// compiler puts on a `FETCH_DIM_W`: what `zend_wrong_string_offset_error`
/// reads to pick its sentence for a string under a write
/// (`Z/Zend/zend_execute.c:1779`).
const FETCH_DIM_REF: u32 = 1;

/// `ZEND_FETCH_DIM_OBJ` (`Z/Zend/zend_compile.h:1097`), the value
/// `zend_delayed_compile_prop` leaves on the `FETCH_DIM_W` under a
/// `->`: `$s[0]->p = 1` over a string is `Cannot use string offset as
/// an object` where `$s[0][1] = 1` is the array's sentence.
const FETCH_DIM_OBJ: u32 = 3;

/// `ZEND_FETCH_DIM_INCDEC` (`Z/Zend/zend_compile.h:1098`), on the last
/// `FETCH_DIM_RW` of a `++` or a `--` over an element: `Cannot
/// increment/decrement string offsets`.
const FETCH_DIM_INCDEC: u32 = 4;

/// What the container of a dimension write turns out to be, the three
/// `zend_fetch_dimension_address` and `ZEND_ASSIGN_DIM` both have a body
/// for: the array the write lands in, the object whose `offsetSet` and
/// `offsetGet` stand in for one, and the string a byte of which the
/// write replaces, named by the place that holds it.
enum Container {
    Array(PhpArray),
    Object(PhpObject),
    Text(php_heap::ZvalRef),
}

/// The one walk to the array a dimension lands in, the head every site
/// that reaches a container shares: `zend_fetch_dimension_address`'s
/// `try_array` (`Z/Zend/zend_execute.c:2903`), `ZEND_ASSIGN_DIM`'s and
/// `ZEND_UNSET_DIM`'s. A reference is followed once, since no reference
/// holds a reference, and the place it was followed to is handed back,
/// so the write lands in the cell and not in the name of it; an array
/// there is `SEPARATE_ARRAY`, one owner of it from here and the copy
/// written back into the place where the array was shared. `None` is
/// what is no array, which each site's own ladder reads, because php has
/// one ladder for a write and another for an unset.
fn separated_array(
    req: &mut Request,
    place: php_heap::ZvalRef,
) -> Result<(php_heap::ZvalRef, Option<PhpArray>), Thrown> {
    let mut at = place;
    let mut held = exec::value_of(at.read());
    if let Some(cell) = held.as_php_ref() {
        at = cell.heap().val_ref();
        held = exec::value_of(at.read());
    }
    let Some(array) = held.as_array() else {
        return Ok((at, None));
    };
    let separated = req.array_separate(array.heap())?;
    if !separated.is_same(array.heap()) {
        at.write(php_heap::Zval::array(separated));
    }
    Ok((at, Some(PhpArray::of(separated))))
}

/// `zend_fetch_dimension_address`'s walk to the container a dimension
/// write lands in (`Z/Zend/zend_execute.c:2903`), which `ASSIGN_DIM`
/// walks too: the array is [`separated_array`]'s, and `null`, an
/// unwritten variable and `false` become a new array in place
/// (`array_init`), `false` with php's 8.1 deprecation before it
/// (`zend_false_to_array_deprecated`). A string and an object are handed
/// back for the site to serve, since what each does differs between a
/// fetch and an assignment. What is left is php's `Error` `Cannot use a
/// scalar value as an array` (`zend_use_scalar_as_array`) for a number,
/// `true` and a resource. `ZEND_UNSET_DIM` shares the walk and not this
/// ladder, since php gives it one of its own
/// (`Z/Zend/zend_vm_def.h:6812`): it makes no array out of what is none.
fn write_container(req: &mut Request, place: php_heap::ZvalRef) -> Result<Container, Thrown> {
    let (at, array) = separated_array(req, place)?;
    if let Some(array) = array {
        return Ok(Container::Array(array));
    }
    let held = exec::value_of(at.read());
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
                return Err(Thrown::of(None));
            }
        }
        let array = req.array()?;
        at.write(php_heap::Zval::array(array.heap()));
        return Ok(Container::Array(array));
    }
    if held.as_php_string().is_some() {
        return Ok(Container::Text(at));
    }
    if let Some(object) = held.as_object().copied() {
        return Ok(Container::Object(object));
    }
    Err(crate::api::raising::throw(
        req,
        "Error",
        "Cannot use a scalar value as an array",
    ))
}

/// `zend_assign_to_string_offset` (`Z/Zend/zend_execute.c`), `$s[k] = v`
/// over a string: the offset read as [`string_dim_offset`] reads it,
/// php's warning `Illegal string offset` and `null` for one further from
/// the end than the string is long, and a negative one counted from the
/// end. The value is the first byte of what it reads as a string, php's
/// `Error` `Cannot assign an empty string to a string offset` where it
/// has none and the warning `Only the first byte will be assigned to the
/// string offset` where it has more; what is written is that one byte,
/// and what the opline's temporary is handed is that byte as a string of
/// one byte (`ZVAL_CHAR`).
///
/// The string is php's `SEPARATE_STRING` first: one the request owns
/// alone and long enough is written where it stands
/// ([`php_heap::Str::put_byte`]) and allocates nothing, and every other
/// one is laid down anew, which is the copy php makes for a shared or an
/// interned string and the `zend_string_extend` it makes for an offset
/// past the end, the bytes between padded with spaces.
///
/// ```php
/// $s = "abc";
/// $s[1] = "X";
/// var_dump($s);
/// $s[5] = "Z";
/// var_dump($s);
/// $s[-1] = "Q";
/// var_dump($s);
/// $s[0] = "hello";
/// var_dump($s);
/// $s[-9] = "z";
/// var_dump($s);
/// ```
/// ```expect
/// string(3) "aXc"
/// string(6) "aXc  Z"
/// string(6) "aXc  Q"
///
/// Warning: Only the first byte will be assigned to the string offset in %s on line 9
/// string(6) "hXc  Q"
///
/// Warning: Illegal string offset -9 in %s on line 11
/// string(6) "hXc  Q"
/// ```
///
/// A value with no byte at all is php's `Error`.
///
/// ```php
/// $s = "abc";
/// $s[0] = "";
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot assign an empty string to a string offset in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// `$s[]` is no offset at all, php's `zend_use_new_element_for_string`.
///
/// ```php
/// $s = "abc";
/// $s[] = "y";
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: [] operator not supported for strings in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
fn assign_string_offset(
    req: &mut Request,
    at: php_heap::ZvalRef,
    dim: &Value,
    value: &Value,
) -> Result<Value, Thrown> {
    let held = exec::value_of(at.read());
    let Some(text) = held.as_php_string().copied() else {
        return Ok(Value::null());
    };
    // A write is `BP_VAR_W`, which `zend_check_string_offset` runs
    // through the read's ladder with php's errors on, so no arm of it
    // answers `null` here.
    let Some(offset) = string_dim_offset(req, dim, DimRead::Warning)? else {
        return Ok(Value::null());
    };
    let len = i64::try_from(text.len()).unwrap_or(i64::MAX);
    if offset < -len {
        crate::api::raising::warning(req, &format!("Illegal string offset {offset}"));
        return Ok(Value::null());
    }
    let offset = if offset < 0 {
        offset.saturating_add(len)
    } else {
        offset
    };
    let written = crate::ops::to_string(req, value)?;
    let count = written.len();
    let byte = written.as_bytes().first().copied().unwrap_or(0);
    req.release(Value::string(written));
    if count == 0 {
        return Err(crate::api::raising::throw(
            req,
            "Error",
            "Cannot assign an empty string to a string offset",
        ));
    }
    if count != 1 {
        crate::api::raising::warning(
            req,
            "Only the first byte will be assigned to the string offset",
        );
    }
    let index = usize::try_from(offset).unwrap_or(usize::MAX);
    let need = index.saturating_add(1).max(text.len());
    let s = text.heap();
    let alone = !s.is_interned() && !s.is_persistent() && s.refcount() == 1;
    if alone && need == text.len() {
        s.put_byte(index, byte);
    } else {
        let fresh = PhpString::filled(req, need, |bytes| {
            let old = text.as_bytes();
            let copied = old.len().min(bytes.len());
            if let (Some(head), Some(from)) = (bytes.get_mut(..copied), old.get(..copied)) {
                head.copy_from_slice(from);
            }
            for slot in bytes.iter_mut().skip(copied) {
                *slot = b' ';
            }
            if let Some(slot) = bytes.get_mut(index) {
                *slot = byte;
            }
        })?;
        at.write(php_heap::Zval::string(fresh.heap()));
        req.release(held);
    }
    Ok(Value::string(PhpString::new(req, &[byte])?))
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
/// A string under a write is no container at all: `[]` is php's `Error`
/// `[] operator not supported for strings`, and a dimension is
/// `zend_check_string_offset`'s diagnostics and then
/// `zend_wrong_string_offset_error`, whose sentence the opline's
/// `ZEND_FETCH_DIM_REF`, `ZEND_FETCH_DIM_DIM` or `ZEND_FETCH_DIM_OBJ`
/// picks — a reference, an array or an object over the offset. An object is
/// `offsetGet` under `BP_VAR_W`: what it returned is written into the
/// `VAR` as a value and not an `IS_INDIRECT`, with php's notice
/// `Indirect modification of overloaded element of C has no effect`
/// where that value is no object, so the write after it lands in the
/// temporary and reaches nothing.
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
///
/// ```php
/// $s = "abc";
/// $s[0][1] = "x";
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot use string offset as an array in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// $s = "abc";
/// $s[0]->p = "x";
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot use string offset as an object in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// $s = "abc";
/// $s[0] = &$x;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot create references to/from string offsets in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// class C implements ArrayAccess {
///     public function offsetExists($offset): bool { return true; }
///     public function offsetGet($offset): mixed { return 7; }
///     public function offsetSet($offset, $value): void { var_dump("set", $offset, $value); }
///     public function offsetUnset($offset): void {}
/// }
/// $c = new C;
/// $c[0][1] = 5;
/// ```
/// ```expect exit=255
/// Notice: Indirect modification of overloaded element of C has no effect in %s on line 9
///
/// Fatal error: Uncaught Error: Cannot use a scalar value as an array in %s:9
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 9
/// ```
pub(crate) fn fetch_dim_w<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    fetch_dim_place(req, frame, code, op, DimRead::Silent, Keep::Nothing)
}

/// `ZEND_FETCH_DIM_RW` (`Z/Zend/zend_vm_def.h:2024`), the link of a
/// compound assignment's chain under the last: [`fetch_dim_w`]'s body
/// with the read's own diagnostic in it, which is what `_RW` adds to
/// `_W`. `zend_fetch_dimension_address_inner` under `BP_VAR_RW` is
/// `zend_undefined_offset_write` and `zend_undefined_index_write`: the
/// `Undefined array key` warning, and then the entry laid down holding
/// `null` exactly as `_W` lays it down.
///
/// ```php
/// $a = [[1]];
/// $a[0][1] *= 2;
/// var_dump($a);
/// $b = [];
/// $b[0][0] += 1;
/// var_dump($b);
/// ```
/// ```expect
///
/// Warning: Undefined array key 1 in %s on line 3
/// array(1) {
///   [0]=>
///   array(2) {
///     [0]=>
///     int(1)
///     [1]=>
///     int(0)
///   }
/// }
///
/// Warning: Undefined array key 0 in %s on line 6
///
/// Warning: Undefined array key 0 in %s on line 6
/// array(1) {
///   [0]=>
///   array(1) {
///     [0]=>
///     int(1)
///   }
/// }
/// ```
pub(crate) fn fetch_dim_rw<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    fetch_dim_place(req, frame, code, op, DimRead::Warning, Keep::Nothing)
}

/// The array arm of [`fetch_dim_place`], `fetch_from_array`: the entry
/// the dimension names as the `IS_INDIRECT` the `VAR` takes, laid down
/// by [`write_entry`] under `BP_VAR_W` and by [`rw_entry`] under
/// `BP_VAR_RW`, and `null` where the array went while the warning ran,
/// php's `ZVAL_NULL(result)`.
fn array_place(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    array: PhpArray,
    read: DimRead,
) -> Result<Result<php_heap::Zval, Thrown>, Fatal> {
    let dim = (op.op2_type != OperandKind::Unused)
        .then(|| exec::operand(req, frame, code, op.op2_type, op.op2));
    let constant = op.op2_type == OperandKind::Const;
    let landing = match read {
        DimRead::Warning => rw_entry(req, array, dim.as_ref(), constant)?,
        DimRead::Silent => write_entry(req, array, dim.as_ref(), constant)?.map(Landing::from),
    };
    Ok(match landing {
        Ok(Landing::Entry(entry)) => Ok(php_heap::Zval::indirect(entry)),
        Ok(Landing::Gone) => Ok(php_heap::Zval::null()),
        Ok(Landing::Occupied) => Err(crate::api::raising::throw(
            req,
            "Error",
            "Cannot add element to the array as the next element is already occupied",
        )),
        Err(thrown) => Err(thrown),
    })
}

/// The body `FETCH_DIM_W` and `FETCH_DIM_RW` share, php's one
/// `zend_fetch_dimension_address` under two `BP_VAR_*`: `read` is
/// `BP_VAR_RW`'s own diagnostic for a key the table has not got, which
/// `BP_VAR_W` lays down in silence.
fn fetch_dim_place(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    read: DimRead,
    keep: Keep,
) -> Edge {
    let out = exec::temporary_slot(code, op.out);
    let Some(place) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_FETCH_DIM_W over a constant or a temporary, which php compiles no write over",
        )));
    };
    let written = match write_container(req, place) {
        Ok(Container::Array(array)) => array_place(req, frame, code, op, array, read)?,
        Ok(Container::Object(object)) => {
            let dim = object_dim(req, frame, code, op);
            let table = req.object_handlers(object.heap());
            if let Some(placed) = crate::weakrefs::dimension_place(req, table, object, dim.as_ref())
            {
                placed.map(php_heap::Zval::indirect)
            } else {
                match table.read_dimension(req, object, dim.as_ref()) {
                    Ok(value) => {
                        if value.as_object().is_none() {
                            let class = req.class_name(object.class());
                            crate::api::raising::notice(
                                req,
                                &format!(
                                    "Indirect modification of overloaded element of {class} has no effect"
                                ),
                            );
                        }
                        Ok(value.to_zval())
                    }
                    Err(thrown) => Err(thrown),
                }
            }
        }
        Ok(Container::Text(_)) if op.op2_type == OperandKind::Unused => {
            // `zend_use_new_element_for_string`.
            Err(crate::api::raising::throw(
                req,
                "Error",
                "[] operator not supported for strings",
            ))
        }
        Ok(Container::Text(_)) => {
            // `zend_check_string_offset`, whose diagnostics come first,
            // then `zend_wrong_string_offset_error`, whose sentence the
            // opline's `extended_value` picks.
            let dim = exec::operand(req, frame, code, op.op2_type, op.op2);
            Err(match string_dim_offset(req, &dim, DimRead::Warning) {
                Ok(_) if op.extended_value == FETCH_DIM_REF => crate::api::raising::throw(
                    req,
                    "Error",
                    "Cannot create references to/from string offsets",
                ),
                Ok(_) if op.extended_value == FETCH_DIM_OBJ => crate::api::raising::throw(
                    req,
                    "Error",
                    "Cannot use string offset as an object",
                ),
                Ok(_) if op.extended_value == FETCH_DIM_INCDEC => crate::api::raising::throw(
                    req,
                    "Error",
                    "Cannot increment/decrement string offsets",
                ),
                Ok(_) => {
                    crate::api::raising::throw(req, "Error", "Cannot use string offset as an array")
                }
                Err(thrown) => thrown,
            })
        }
        Err(thrown) => Err(thrown),
    };
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    let previous = req.take_slot(frame, out);
    req.release(previous);
    let written = match written {
        Ok(written) => written,
        Err(thrown) => {
            if keep == Keep::Nothing {
                exec::free_operand(req, frame, code, op.op1_type, op.op1);
            }
            return threw(req, frame, &thrown);
        }
    };
    let Some(var) = req.slot_ptr(frame, out) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_FETCH_DIM_W writes a VAR the frame does not have",
        )));
    };
    var.write(written);
    if keep == Keep::Nothing {
        exec::free_container_var(req, frame, code, op);
    }
    Ok(advance(req, frame))
}

/// Whether a dimension fetched for writing gives its `VAR` container up
/// after it, `FREE_OP1_VAR_PTR`: `FETCH_DIM_W` and `FETCH_DIM_RW` do,
/// and `FETCH_LIST_W` keeps it for the elements after it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keep {
    Nothing,
    Container,
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
        (Some(object), _) => match class_operand(req, frame, code, op.op2_type, op.op2, None) {
            Ok(class) => req.instance_of(object.class(), class),
            Err(thrown) => {
                exec::free_operand(req, frame, code, op.op1_type, op.op1);
                return threw(req, frame, &thrown);
            }
        },
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
/// and the callee together where it is (`CACHE_POLYMORPHIC_PTR`), so it
/// reads and writes [`Cached::ClassAndCallee`] off the slot itself.
fn cached_class(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    at: u32,
    slot: Option<CacheSlot>,
) -> Option<crate::api::values::ClassId> {
    if let Some(Cached::Class(class)) = slot.map(|slot| req.cached(frame, slot)) {
        return Some(class);
    }
    let lowered = exec::constant(code, ConstIdx(at.saturating_add(1)));
    let found = lowered
        .as_str_bytes()
        .and_then(|lowered| exec::find_class(req, lowered));
    if let (Some(slot), Some(class)) = (slot, found) {
        req.prime(frame, slot, Cached::Class(class));
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
    match kind {
        OperandKind::Const => {}
        // `zend_fetch_class(NULL, opline->opN.num)`: `self`, `parent` or
        // `static`, the fetch type in the operand's number.
        OperandKind::Unused => return scope_class(req, at),
        // `Z_CE_P(EX_VAR(opline->opN.var))`, what a `FETCH_CLASS` left.
        OperandKind::Var => return fetched_class(req, frame, code, at),
        OperandKind::Tmp | OperandKind::Cv => {
            return Err(crate::api::raising::fatal(
                req,
                "InternalError: a class operand that is neither a constant, a fetch type nor a FETCH_CLASS",
            ));
        }
    }
    if let Some(class) = cached_class(req, frame, code, at, slot) {
        return Ok(class);
    }
    let written = exec::constant(code, ConstIdx(at));
    if let Some(class) =
        crate::api::call::autoload(req, written.as_str_bytes().unwrap_or_default())?
    {
        if let Some(slot) = slot {
            req.prime(frame, slot, Cached::Class(class));
        }
        return Ok(class);
    }
    Err(crate::api::raising::throw(
        req,
        "Error",
        &format!(
            "Class \"{}\" not found",
            String::from_utf8_lossy(written.as_str_bytes().unwrap_or_default())
        ),
    ))
}

/// `ZEND_FETCH_CLASS_MASK` (`Z/Zend/zend_compile.h:1034`): the bits of a
/// class operand's number that are its fetch type, the flags above them.
const FETCH_CLASS_MASK: u32 = 0x0f;

/// `ZEND_FETCH_CLASS_SELF` and `ZEND_FETCH_CLASS_PARENT`
/// (`Z/Zend/zend_compile.h:1028`), the two fetch types a call on the class
/// forwards the called scope through.
const FETCH_CLASS_SELF: u32 = 1;
const FETCH_CLASS_PARENT: u32 = 2;

/// `zend_fetch_class(NULL, fetch_type)` (`Z/Zend/zend_execute_API.c:1712`)
/// under `ZEND_FETCH_CLASS_EXCEPTION`, which every compiled reference
/// carries: `self` the executed scope, `parent` its parent and `static`
/// the called scope, each php's `Error` where there is none
/// ([`crate::api::call::fetch_class`]).
fn scope_class(req: &mut Request, fetch: u32) -> Result<crate::api::values::ClassId, Thrown> {
    let name: &[u8] = match fetch & FETCH_CLASS_MASK {
        FETCH_CLASS_SELF => b"self",
        FETCH_CLASS_PARENT => b"parent",
        _ => b"static",
    };
    crate::api::call::fetch_class(req, name)
}

/// `Z_CE_P(EX_VAR(var))`: the class a `FETCH_CLASS` left in its `VAR`.
/// php keeps the class entry's pointer in the slot, which is no value a
/// program can read; the slot holds the class's identity as a long
/// here ([`hold_class`]), and nothing but the opline after the fetch
/// reads it.
fn fetched_class(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    at: u32,
) -> Result<crate::api::values::ClassId, Thrown> {
    let held = req.slot(frame, exec::temporary_slot(code, at));
    match held.as_int().and_then(|id| u32::try_from(id).ok()) {
        Some(id) => Ok(crate::api::values::ClassId(id)),
        None => Err(crate::api::raising::fatal(
            req,
            "InternalError: a class operand whose VAR no FETCH_CLASS wrote",
        )),
    }
}

/// `Z_CE_P(EX_VAR(opline->result.var)) = ce`: the class written into the
/// `VAR` [`fetched_class`] reads.
fn hold_class(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    out: u32,
    class: crate::api::values::ClassId,
) {
    let slot = exec::temporary_slot(code, out);
    let previous = req.set_slot(frame, slot, Value::int(i64::from(class.0)));
    req.release(previous);
}

/// `ZEND_FETCH_CLASS_NAME` (`Z/Zend/zend_vm_def.h:8895`), `C::class`
/// where the compiler could not fold it: over a value, the object's
/// class's name, what is no object php's `TypeError` `Cannot use
/// "::class" on int`; over a fetch type, the name of
/// `EX(func)->op_array.scope`, of its parent, or of the called scope
/// (`Z_OBJCE(EX(This))` or `Z_CE(EX(This))`), php's `Error` where the
/// op array has no scope or the class no parent.
///
/// ```php
/// class A { static function who() { return static::class; } }
/// class B extends A {}
/// $b = new B;
/// var_dump(B::who(), $b::class);
/// try { $n = 1; $n::class; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// echo static::class;
/// ```
/// ```expect exit=255
/// string(1) "B"
/// string(1) "B"
/// Cannot use "::class" on int
///
/// Fatal error: Uncaught Error: Cannot use "static" in the global scope in %s:7
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 7
/// ```
pub(crate) fn fetch_class_name<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let class = if op.op1_type == OperandKind::Unused {
        let fetch = op.op1 & FETCH_CLASS_MASK;
        let word = match fetch {
            FETCH_CLASS_SELF => "self",
            FETCH_CLASS_PARENT => "parent",
            _ => "static",
        };
        let Some(scope) = req.frame_scope(frame) else {
            let thrown = crate::api::raising::throw(
                req,
                "Error",
                &format!("Cannot use \"{word}\" in the global scope"),
            );
            return threw(req, frame, &thrown);
        };
        match fetch {
            FETCH_CLASS_SELF => scope,
            FETCH_CLASS_PARENT => {
                let Some(parent) = req.class_parent(scope) else {
                    let thrown = crate::api::raising::throw(
                        req,
                        "Error",
                        "Cannot use \"parent\" when current class scope has no parent",
                    );
                    return threw(req, frame, &thrown);
                };
                parent
            }
            _ => req
                .vm()
                .this(frame)
                .map(|this| this.class())
                .or_else(|| req.vm().at(frame).and_then(|state| state.called_scope))
                .unwrap_or(scope),
        }
    } else {
        let value = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
        let class = value.as_object().map(PhpObject::class);
        let Some(class) = class else {
            let shown = crate::ops::value_name(req, &value);
            let thrown = crate::api::raising::throw(
                req,
                "TypeError",
                &format!("Cannot use \"::class\" on {shown}"),
            );
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            return threw(req, frame, &thrown);
        };
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        class
    };
    let name = req
        .class_decl(class)
        .map(|decl| decl.name.0.to_vec())
        .unwrap_or_default();
    let name = match PhpString::new(req, &name) {
        Ok(name) => name,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, Value::string(name));
    req.release(previous);
    Ok(advance(req, frame))
}

/// `ZEND_FETCH_CLASS` (`Z/Zend/zend_vm_def.h:3540`), what
/// `zend_compile_class_ref` emits for a class the program computes,
/// `$o::X`, `new $name`, `$cls::m()`: an object's class, a string's
/// class by name through `zend_fetch_class` (the autoloaders asked,
/// `self`, `parent` and `static` read as their fetch type), and php's
/// `Error` for any other value, an undefined variable having warned.
/// The class is written into the `VAR` for the opline after it.
///
/// ```php
/// class A { const X = 'x'; public static $p = 'p'; static function m() { return 'm'; } }
/// $o = new A; $n = 'A'; $l = 'a';
/// var_dump($o::X, $n::$p, $l::m(), get_class(new $n));
/// try { $i = 1; $i::X; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $u = 'Nope'; $u::m(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "x"
/// string(1) "p"
/// string(1) "m"
/// string(1) "A"
/// Class name must be a valid object or a string
/// Class "Nope" not found
/// ```
pub(crate) fn fetch_class<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let class = if op.op2_type == OperandKind::Unused {
        scope_class(req, op.op1)
    } else {
        let value = exec::operand(req, frame, code, op.op2_type, op.op2).deref();
        let class = if let Some(object) = value.as_object() {
            Ok(object.class())
        } else if let Some(name) = value.as_str_bytes() {
            let name = name.to_vec();
            crate::api::call::fetch_class(req, &name)
        } else {
            Err(crate::api::raising::throw(
                req,
                "Error",
                "Class name must be a valid object or a string",
            ))
        };
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
        class
    };
    match class {
        Ok(class) => {
            hold_class(req, frame, code, op.out, class);
            Ok(advance(req, frame))
        }
        Err(thrown) => threw(req, frame, &thrown),
    }
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
    let answer = match req.cached(frame, slot) {
        Cached::Hit => Cached::Hit,
        Cached::Miss => Cached::Miss,
        Cached::Unprimed
        | Cached::Function(_)
        | Cached::Class(_)
        | Cached::ClassAndCallee { .. } => {
            let name = exec::constant(code, ConstIdx(op.op1));
            let found = name
                .as_str_bytes()
                .is_some_and(|name| exec::find_function(req, name).is_some());
            let answer = if found { Cached::Miss } else { Cached::Hit };
            req.prime(frame, slot, answer);
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
        closure: None,
        extra_named: Vec::new(),
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
    // `GET_OP1_ZVAL_PTR_PTR_UNDEF(BP_VAR_W)`: a CV, or the place a fetch
    // by name left in its `VAR` as an `IS_INDIRECT`.
    let Some(at) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN over a variable the frame does not have",
        )));
    };
    let strict = req.frame_strict_types(frame);
    // php copies into `result` what the write landed, which a typed
    // cell may have converted, so the copy is after the write and not
    // before it.
    let (landed, garbage) = match assign_to_place(req, at, value, strict) {
        Ok(pair) => pair,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    if op.out_type != OperandKind::Unused {
        req.addref(&landed);
        let slot = exec::temporary_slot(code, op.out);
        let previous = req.set_slot(frame, slot, landed);
        req.release(previous);
    }
    req.release(garbage);
    Ok(advance(req, frame))
}

/// `ZEND_ASSIGN_OP` (`Z/Zend/zend_vm_def.h:1257`), `$a op= v` over a
/// CV: one opline, the variable in `op1`, the value in `op2` and the
/// operator's own opcode in `extended_value`, which php reads back with
/// `get_binary_op`.
///
/// php reads the value first (`GET_OP2_ZVAL_PTR(BP_VAR_R)`) and the
/// variable after it (`GET_OP1_ZVAL_PTR_PTR(BP_VAR_RW)`), so `$v .= $w`
/// with both undefined warns for `$w` before `$v`. The `BP_VAR_RW` read
/// makes an undefined CV `null` where `BP_VAR_R` leaves it undefined
/// (`_get_zval_ptr_cv_BP_VAR_RW`, `Z/Zend/zend_execute.c:242`), which is
/// what a variable an operator threw over is left holding.
///
/// The operation is php's own binary kernel over what the variable holds
/// and the value (`zend_binary_op(var_ptr, var_ptr, value)`), so `$a /=
/// 0` raises exactly what `/` raises and the variable is left as it was;
/// the write lands in the cell where the CV holds a reference, as
/// `ASSIGN` over one does, and a cell that carries types holds the value
/// the operation came to before it lands
/// (`ZEND_REF_HAS_TYPE_SOURCES`, `zend_binary_assign_op_typed_ref`); the
/// temporary takes what landed under `RETURN_VALUE_USED`, which a typed
/// cell may have made something else.
///
/// ```php
/// $a = 6; $a |= 3; $a .= "x"; var_dump($a);
/// $a = 7; var_dump($a **= 2, $a <<= 1, $a %= 5, $a -= 1.5);
/// $b = &$a; $b *= 2; var_dump($a);
/// ```
/// ```expect
/// string(2) "7x"
/// int(49)
/// int(98)
/// int(3)
/// float(1.5)
/// float(3)
/// ```
///
/// A kernel that threw leaves the variable as it stood, and `%=` over
/// zero is the same `DivisionByZeroError` with php's own message for the
/// operator:
///
/// ```php
/// $a = 1;
/// try { $a /= 0; } catch (DivisionByZeroError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// var_dump($a);
/// $a %= 0;
/// ```
/// ```expect exit=255
/// DivisionByZeroError: Division by zero
/// int(1)
///
/// Fatal error: Uncaught DivisionByZeroError: Modulo by zero in %s:5
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 5
/// ```
///
/// The value is read before the variable, so the two warnings of `$v .=
/// $w` name `$w` first; the variable the `BP_VAR_RW` read made `null` is
/// what the operator works over.
///
/// ```php
/// $u += 1; var_dump($u);
/// $v .= $w;
/// var_dump($v);
/// ```
/// ```expect
/// Warning: Undefined variable $u in %s on line 2
/// int(1)
///
/// Warning: Undefined variable $w in %s on line 3
///
/// Warning: Undefined variable $v in %s on line 3
/// string(0) ""
/// ```
pub(crate) fn assign_op<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(kernel) = binary_op_of(op.extended_value) else {
        return Err(Fatal::Error(format!(
            "InternalError: ZEND_ASSIGN_OP over the operator {}, which is no binary operation",
            op.extended_value
        )));
    };
    let value = exec::operand(req, frame, code, op.op2_type, op.op2);
    // A CV, or under a `VAR` the place `FETCH_RW` by name left there,
    // which it has already made `null` where nothing was written.
    let held = if op.op1_type == OperandKind::Cv {
        let held = exec::operand(req, frame, code, op.op1_type, op.op1);
        if req.slot(frame, op.op1).tag() == TypeTag::Undef {
            let previous = req.set_slot(frame, op.op1, Value::null());
            req.release(previous);
        }
        held
    } else {
        write_place(req, frame, code, op.op1_type, op.op1)
            .map_or_else(Value::null, |place| exec::value_of(place.read()))
    };
    let computed = if grows_in_place(req, op.extended_value, &held) {
        crate::ops::concat_assign(req, &held, &value, |req| {
            let at = write_place(req, frame, code, op.op1_type, op.op1);
            vacate(req, at);
        })
    } else {
        kernel(req, &held, &value)
    };
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    let stepped = match computed {
        Ok(stepped) => stepped,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let Some(at) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_OP over a variable the frame does not have",
        )));
    };
    // php's `ZEND_REF_HAS_TYPE_SOURCES` arm,
    // `zend_binary_assign_op_typed_ref`: the operation is over what the
    // cell holds either way, and the value it comes to is held to each
    // type the cell carries before it lands, which
    // [`assign_to_place`] does under the frame's own regime (#496).
    // What landed is what `result` takes, so the copy is after the
    // write and not before it.
    let strict = req.frame_strict_types(frame);
    let (landed, garbage) = match assign_to_place(req, at, stepped, strict) {
        Ok(pair) => pair,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    write_out(req, frame, code, op, &landed);
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
///
/// One dimension bound to another of the same array is the `VAR` a
/// `MAKE_REF` left, whose owner `FREE_OP2` gives up here, so the cell
/// is the array's thirty-two bytes and the whole array goes when
/// nothing names it.
///
/// ```php
/// $a[0] = 1; $b = memory_get_usage(); $a[1] = &$a[0]; $held = memory_get_usage() - $b;
/// unset($a); var_dump($held, memory_get_usage() - $b);
/// ```
/// ```expect
/// int(32)
/// int(-216)
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
            exec::free_operand(req, frame, code, op.op2_type, op.op2);
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
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
    // `FREE_OP2()` before `FREE_OP1()`: a `VAR` holding the
    // `IS_INDIRECT` a fetch left owns nothing and this drops nothing,
    // and one holding the cell a `MAKE_REF` or this handler itself put
    // there gives up the owner it took, so the cell is held by the
    // place it binds and by the target alone.
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    Ok(advance(req, frame))
}

/// `ZEND_ASSIGN_OBJ_REF` (`Z/Zend/zend_vm_def.h:2878`), `$o->p = &$x`,
/// two oplines: this one names the object and the property, and the
/// `OP_DATA` after it carries the source, so the handler steps over
/// both. `zend_assign_to_property_reference`: the property's place
/// found as `FETCH_OBJ_W` finds it ([`property_place`]), the source made
/// a reference in place where it holds none, and the property bound to
/// that cell; a typed property holds the cell to its type first and
/// becomes one of the cell's type sources
/// (`zend_assign_to_typed_property_reference`). A call that returned by
/// value is php's notice and an assignment by value
/// (`zend_wrong_assign_to_variable_reference`). The `VAR` takes the
/// cell under `RETURN_VALUE_USED`.
///
/// ```php
/// class C { public $p; public int $n = 0; }
/// $o = new C;
/// $x = 1;
/// $o->p = &$x;
/// $x = 2;
/// $o->d = &$x;
/// $o->d = 3;
/// var_dump($o->p, $x);
/// $s = "5";
/// $o->n = &$s;
/// var_dump($s);
/// $t = "no";
/// try { $o->n = &$t; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// function f() { return 7; }
/// $o->p = &f();
/// var_dump($o->p, $x);
/// $z = $o->q = &$x;
/// var_dump($z);
/// ```
/// ```expect
/// Deprecated: Creation of dynamic property C::$d is deprecated in %s on line 7
/// int(3)
/// int(3)
/// int(5)
/// Cannot assign string to property C::$n of type int
///
/// Notice: Only variables should be assigned by reference in %s on line 16
/// int(7)
/// int(7)
///
/// Deprecated: Creation of dynamic property C::$q is deprecated in %s on line 18
/// int(7)
/// ```
pub(crate) fn assign_obj_ref<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let at = req.vm().opline(frame);
    let Some(data) =
        exec::opline_at(code, at.saturating_add(1)).filter(|next| next.opcode == Opcode::OpData)
    else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_OBJ_REF with no OP_DATA after it",
        )));
    };
    let (data_type, data_at) = (data.op1_type, data.op1);
    let (place, object, name) = match property_place(req, frame, code, op, PropertyRead::Silent)? {
        PropertyPlace::At(place, object, name) => (place, object, name),
        PropertyPlace::Threw(thrown) => {
            exec::free_operand(req, frame, code, data_type, data_at);
            return threw(req, frame, &thrown);
        }
    };
    let Some(source) = write_place(req, frame, code, data_type, data_at) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_OBJ_REF over a source php compiles no write over",
        )));
    };
    let held = exec::value_of(source.read());
    let strict = req.frame_strict_types(frame);
    if op.extended_value & RETURNS_FUNCTION != 0 && held.as_php_ref().is_none() {
        crate::api::raising::notice(req, "Only variables should be assigned by reference");
        if req.is_pending() {
            exec::free_operand(req, frame, code, data_type, data_at);
            return Ok(Stepped::Threw(frame));
        }
        let value = held.undef_as_null();
        req.addref(&value);
        let (landed, garbage) = match assign_to_place(req, place, value, strict) {
            Ok(pair) => pair,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        write_out(req, frame, code, op, &landed);
        req.release(garbage);
        exec::free_operand(req, frame, code, data_type, data_at);
        req.vm_mut().jump(frame, at.saturating_add(2));
        return Ok(Stepped::Same(frame));
    }
    let cell = if let Some(cell) = held.as_php_ref() {
        *cell
    } else {
        let cell = match req.reference(held.undef_as_null()) {
            Ok(cell) => cell,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        // The owner the source had of `held` is the cell's now.
        source.write(cell.as_value().to_zval());
        cell
    };
    let typed = req.declared_typed_property(object.class(), &name);
    if let Some(typed) = typed.as_ref() {
        match crate::ops::verify_prop_assignable_by_ref(
            req,
            cell.heap(),
            &typed.ty,
            typed.declaring,
            &name,
            strict,
        ) {
            Ok(None) => {}
            Ok(Some(coerced)) => req.reference_set(cell.heap(), coerced),
            Err(thrown) => {
                exec::free_operand(req, frame, code, data_type, data_at);
                return threw(req, frame, &thrown);
            }
        }
    }
    let garbage = exec::value_of(place.read());
    // `ZEND_REF_DEL_TYPE_SOURCE`: the cell the slot held is no longer
    // typed by this property.
    if let (Some(typed), Some(old)) = (typed.as_ref(), garbage.as_php_ref()) {
        req.ref_del_type_source(old.heap(), typed.declaring, &name);
    }
    let bound = cell.as_value();
    req.addref(&bound);
    place.write(bound.to_zval());
    if let Some(typed) = typed {
        req.ref_add_type_source(
            cell.heap(),
            crate::api::request::RefSource {
                declaring: typed.declaring,
                name: name.clone(),
                ty: typed.ty,
            },
        );
    }
    write_out(req, frame, code, op, &bound);
    req.release(garbage);
    exec::free_operand(req, frame, code, data_type, data_at);
    req.vm_mut().jump(frame, at.saturating_add(2));
    Ok(Stepped::Same(frame))
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
///
/// A slot the class declared with a type takes the write through
/// `zend_assign_to_typed_prop`: the value the type accepts as it
/// stands, the weak rule's coercion where the file is not `strict_types`
/// and the type is a scalar one, and php's `TypeError` where it is
/// neither, the slot keeping what it held.
///
/// ```php
/// class C { public int $n = 1; }
/// $c = new C;
/// $c->n = "7";
/// var_dump($c->n);
/// try { $c->n = "x"; } catch (TypeError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// var_dump($c->n);
/// ```
/// ```expect
/// int(7)
/// TypeError: Cannot assign string to property C::$n of type int
/// int(7)
/// ```
///
/// A `readonly` slot is refused before the value is looked at, once
/// something has written it; the class that declared it is the one the
/// `Error` names, and the write the constructor's promotion makes is
/// the initialisation the bit lets through.
///
/// ```php
/// class C { public function __construct(public readonly int $r) {} }
/// $c = new C(1);
/// try { $c->r = 2; } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// var_dump($c->r);
/// ```
/// ```expect
/// Error: Cannot modify readonly property C::$r
/// int(1)
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
    // `zend_get_property_offset(ce, name, ce->__set != NULL, ...)`:
    // a class with a `__set` places the name quietly, so a property the
    // scope may not reach reaches `write_property` and the accessor
    // there, which is where php raises the `Error` if the guard stands.
    let silent = req.serves_accessor(object.class(), b"__set");
    let placed = match req.declared_slot_for_write(object.class(), bytes, "modify", silent) {
        Ok(placed) => placed,
        Err(thrown) => {
            req.release(value);
            return Ok(Err(thrown));
        }
    };
    match placed {
        // `zend_assign_to_typed_prop`: the value held to the declared
        // type, coerced by the weak rule where the file was not declared
        // `strict_types`, and php's `TypeError` where it neither
        // satisfies nor coerces, the value given up as `FREE_OP_DATA`
        // gives it up. What is written to the slot, and what the
        // temporary takes under `RETURN_VALUE_USED`, is the coercion
        // where there was one, php's `tmp`.
        Some((slot, true)) => {
            let Some(typed) = req.declared_typed_property(object.class(), bytes) else {
                req.release(value);
                return Err(Fatal::Error(format!(
                    "InternalError: ZEND_ASSIGN_OBJ placed the typed property ${shown_name}, whose declared type the class does not carry"
                )));
            };
            // The `ZEND_ACC_READONLY` arm, which stands before the
            // value is looked at at all. php's fast path reaches
            // `zend_assign_to_typed_prop` only over a slot that holds a
            // value, and a slot nothing has written takes the write
            // through `zend_std_write_property`, whose own readonly arm
            // stands under the same `Z_TYPE_P(variable_ptr) != IS_UNDEF`
            // (`Z/Zend/zend_object_handlers.c:1089`), so what the bit
            // refuses is the second write and never the initialisation.
            if typed.readonly && !object.heap().slot(slot).is_undef() {
                req.release(value);
                let class = req.class_name(typed.declaring);
                return Ok(Err(crate::api::raising::throw(
                    req,
                    "Error",
                    &format!("Cannot modify readonly property {class}::${shown_name}"),
                )));
            }
            let strict = req.frame_strict_types(frame);
            let value = match crate::ops::verify_property_type(
                req,
                &value,
                &typed.ty,
                typed.declaring,
                bytes,
                strict,
            ) {
                Ok(None) => value,
                Ok(Some(coerced)) => {
                    req.release(value);
                    coerced
                }
                Err(thrown) => {
                    req.release(value);
                    return Ok(Err(thrown));
                }
            };
            write_out(req, frame, code, op, &value);
            req.assign_declared_slot(object.heap(), slot, value);
            Ok(Ok(()))
        }
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

/// `ZEND_ASSIGN_STATIC_PROP` (`Z/Zend/zend_vm_def.h:2620`),
/// `C::$p = v`, two oplines: this one names the property in `op1` and
/// the class's constant pair in `op2`, and the `OP_DATA` after it
/// carries the value (`GET_OP_DATA_ZVAL_PTR`), so the handler steps
/// over both (`ZEND_VM_NEXT_OPCODE_EX(1, 2)`).
///
/// php addresses the slot before it reads the value
/// (`zend_fetch_static_property_address` under `BP_VAR_W`, then
/// `GET_OP_DATA_ZVAL_PTR`), and on a slot it cannot address gives the
/// value operand up unread (`FREE_OP_DATA`), leaves the temporary
/// undefined (`UNDEF_RESULT`) and throws. So an undeclared property
/// written an undefined variable is php's `Error` alone: the warning
/// the read would raise is never reached, which is the order pinned
/// below. The slot is [`Request::static_slot`]'s, laid down from the
/// declaration's default on first use and held to the executed scope;
/// the write is `zend_update_static_property_ex`
/// ([`Request::set_static_property`]), which writes through a slot
/// holding a reference. The temporary `op` names is one more owner of
/// the value under `RETURN_VALUE_USED` and is not written where the
/// compiler marked it `UNUSED`.
///
/// php reserves three cache slots here as the fetch does, and this
/// engine's table names a slot by its class and its name rather than by
/// an address, so nothing is cached and the lookup is made on every
/// pass, which nothing observes. The address is taken under
/// [`PropertyRead::Warning`], php's `BP_VAR_W` as far as the slot is
/// concerned, so the silent miss [`Request::static_slot`] answers under
/// `BP_VAR_IS` cannot stand here. A typed slot is
/// `zend_assign_to_typed_prop` under this frame's
/// `EX_USES_STRICT_TYPES()`, served by the write itself
/// ([`Request::set_static_property`]): the value the declared type
/// accepts, the weak rule's coercion where the file is not strict, and
/// php's `TypeError` where it is neither.
///
/// ```php
/// class C { public static int $n = 1; }
/// C::$n = 5;
/// var_dump(C::$n);
/// C::$n = "7";
/// var_dump(C::$n);
/// ```
/// ```expect
/// int(5)
/// int(7)
/// ```
///
/// ```php
/// class C { public static int $n = 1; }
/// C::$n = "x";
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Cannot assign string to property C::$n of type int in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// Which of the two rules the write is held under is the one the file
/// this frame's op array was compiled from declared (#411): the weak
/// one converts `"5"` where the declared type is `int`, and a file that
/// declares `strict_types=1` raises over the same write.
///
/// ```php
/// class C { public static int $n = 1; }
/// C::$n = "5";
/// var_dump(C::$n);
/// ```
/// ```expect
/// int(5)
/// ```
///
/// ```php
/// declare(strict_types=1);
/// class C { public static int $n = 1; }
/// C::$n = "5";
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Cannot assign string to property C::$n of type int in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
///
/// A write to a typed declared property of an object is the same
/// verify from the same frame, `zend_assign_to_typed_prop` over
/// `ZEND_ASSIGN_OBJ`.
///
/// ```php
/// declare(strict_types=1);
/// class C { public int $n = 1; }
/// $c = new C;
/// $c->n = "5";
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Cannot assign string to property C::$n of type int in %s:5
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 5
/// ```
///
/// ```php
/// class T { public static $a = 1; }
/// var_dump(T::$a);
/// T::$a = 5;
/// var_dump(T::$a);
/// var_dump(T::$a = 7, T::$a);
/// ```
/// ```expect
/// int(1)
/// int(5)
/// int(7)
/// int(7)
/// ```
///
/// The write lands on the class that declared the property, so a
/// subclass and the parent name one slot.
///
/// ```php
/// class A { public static $p = 1; }
/// class B extends A {}
/// B::$p = 2;
/// var_dump(A::$p, B::$p);
/// ```
/// ```expect
/// int(2)
/// int(2)
/// ```
///
/// A property no class of the chain declares is php's `Error`, and the
/// value is given up unread: the undefined variable warns nothing.
///
/// ```php
/// class C { public static $y = 1; }
/// C::$x = $undef;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Access to undeclared static property C::$x in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// A property the scope may not reach is php's `Error` before the
/// undeclared text, as the read's is.
///
/// ```php
/// class C { private static $p = 1; }
/// C::$p = 2;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot access private property C::$p in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn assign_static_prop<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if op.op1_type != OperandKind::Const {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_STATIC_PROP over a property named at run time",
        )));
    }
    let at = req.vm().opline(frame);
    let Some(data) = exec::opline_at(code, at.saturating_add(1))
        .filter(|next| next.opcode == php_unit::Opcode::OpData)
    else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_STATIC_PROP with no OP_DATA after it",
        )));
    };
    let (data_type, data_at) = (data.op1_type, data.op1);
    let name = exec::constant(code, ConstIdx(op.op1));
    let name = name.as_str_bytes().unwrap_or_default().to_vec();
    let scope = req.executed_scope();
    let addressed = match class_operand(req, frame, code, op.op2_type, op.op2, None) {
        Ok(class) => req.static_slot(class, &name, scope, PropertyRead::Warning),
        Err(thrown) => Err(thrown),
    };
    let placed = match addressed {
        // A write is never `BP_VAR_IS`, so the silent miss cannot stand
        // here.
        Ok(placed) => placed,
        Err(thrown) => {
            exec::free_operand(req, frame, code, data_type, data_at);
            return threw(req, frame, &thrown);
        }
    };
    let Some((declaring, property)) = placed else {
        exec::free_operand(req, frame, code, data_type, data_at);
        req.vm_mut().jump(frame, at.saturating_add(2));
        return Ok(Stepped::Same(frame));
    };
    let value = exec::take_operand(req, frame, code, data_type, data_at);
    write_out(req, frame, code, op, &value);
    // `zend_assign_to_typed_prop` under this frame's
    // `EX_USES_STRICT_TYPES()`, which the write itself serves
    // ([`Request::set_static_property`]): a typed slot takes the value
    // the declared type accepts, coerced by the weak rule where the file
    // is not strict, and php's `TypeError` where it neither satisfies
    // nor coerces. The temporary already names the value php's
    // `RETURN_VALUE_USED` copies, which php writes before the verify
    // too.
    let strict = req.frame_strict_types(frame);
    if let Err(thrown) = req.set_static_slot(declaring, &property, &name, value, strict) {
        return threw(req, frame, &thrown);
    }
    req.vm_mut().jump(frame, at.saturating_add(2));
    Ok(Stepped::Same(frame))
}

/// The slot one of the five read-modify-write opcodes over a
/// `static` property stands on, addressed as php addresses it before it reads
/// anything else: `zend_fetch_static_property_address` under
/// `BP_VAR_RW`, which is [`Request::static_slot`] under
/// [`PropertyRead::Warning`], so a name no class declares and one the
/// scope may not reach each throw where php's does. `None` is the miss
/// that raised nothing, which a write can only meet where the class
/// itself could not be addressed.
fn static_place(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Result<Option<Placed>, Thrown> {
    let name = exec::constant(code, ConstIdx(op.op1));
    let name = name.as_str_bytes().unwrap_or_default().to_vec();
    let scope = req.executed_scope();
    let class = class_operand(req, frame, code, op.op2_type, op.op2, None)?;
    match req.static_slot(class, &name, scope, PropertyRead::Warning)? {
        Some((declaring, property)) => Ok(Some(Placed {
            class,
            declaring,
            name,
            scope,
            ty: property.ty.clone(),
        })),
        None => Ok(None),
    }
}

/// What [`static_place`] addressed: the class the opline named, the
/// class that declared the property, the property's name, the scope the
/// access was measured against, and the declared type the write is held
/// to, which is php's `prop_info` beside the slot.
struct Placed {
    class: crate::api::values::ClassId,
    declaring: crate::api::values::ClassId,
    name: Vec<u8>,
    scope: Option<crate::api::values::ClassId>,
    ty: Option<php_unit::decl::Type>,
}

/// One kernel of two values, which is what `get_binary_op` hands php's
/// compound assignment back.
type BinaryKernel = fn(&mut Request, &Value, &Value) -> Result<Value, Thrown>;

/// The binary operation `extended_value` names on a compound
/// assignment, `ASSIGN_OP` and `ASSIGN_STATIC_PROP_OP` alike, which php
/// reads with `get_binary_op` over the opcode
/// `zend_compile_compound_assign` wrote there.
fn binary_op_of(extended_value: u32) -> Option<BinaryKernel> {
    let opcode = u8::try_from(extended_value)
        .ok()
        .and_then(Opcode::from_u8)?;
    Some(match opcode {
        Opcode::Add => crate::ops::add,
        Opcode::Sub => crate::ops::sub,
        Opcode::Mul => crate::ops::mul,
        Opcode::Div => crate::ops::div,
        Opcode::Mod => crate::ops::r#mod,
        Opcode::Pow => crate::ops::pow,
        Opcode::Sl => crate::ops::shift_left,
        Opcode::Sr => crate::ops::shift_right,
        Opcode::Concat => crate::ops::concat,
        Opcode::BwOr => crate::ops::bitwise_or,
        Opcode::BwXor => crate::ops::bitwise_xor,
        Opcode::BwAnd => crate::ops::bitwise_and,
        _ => return None,
    })
}

/// Whether a compound assignment over `held` is `concat_function` with
/// its own left operand as the place written,
/// [`crate::ops::concat_assign`]: the operator is `.`, and the place is
/// not a cell carrying types over a value that is no string, which php
/// hands to `zend_binary_op` and the check of each type instead
/// (`zend_binary_assign_op_typed_ref`, `Z/Zend/zend_execute.c:1677-1682`).
fn grows_in_place(req: &Request, extended_value: u32, held: &Value) -> bool {
    if !is_concat(extended_value) {
        return false;
    }
    match held.as_php_ref() {
        Some(cell) => {
            req.ref_type_sources(cell.heap()).is_empty() || cell.get().as_php_string().is_some()
        }
        None => true,
    }
}

/// Whether `extended_value` names `.`, the operator of `.=`.
fn is_concat(extended_value: u32) -> bool {
    u8::try_from(extended_value)
        .ok()
        .and_then(Opcode::from_u8)
        .is_some_and(|opcode| opcode == Opcode::Concat)
}

/// `i_zval_ptr_dtor(result); ZVAL_NULL(result)` of `concat_function`
/// over a place, read again where it stands now: a reference there is
/// followed to its cell, `null` is written, and the owner the place held
/// is given up.
fn vacate(req: &mut Request, at: Option<php_heap::ZvalRef>) {
    let Some(mut at) = at else {
        return;
    };
    if let Some(cell) = exec::value_of(at.read()).as_php_ref() {
        at = cell.heap().val_ref();
    }
    let held = exec::value_of(at.read());
    at.write(Value::null().to_zval());
    req.release(held);
}

/// `ZEND_ASSIGN_STATIC_PROP_OP` (`Z/Zend/zend_vm_def.h:2714`),
/// `C::$p op= v`, two oplines: this one names the property in `op1`,
/// the class's constant pair in `op2` and the operator in
/// `extended_value`, and the `OP_DATA` after it carries the value and
/// the cache slot the fetch's own `extended_value` held, so the handler
/// steps over both.
///
/// php addresses the slot before it reads the value
/// (`zend_fetch_static_property_address` under `BP_VAR_RW`, then
/// `GET_OP_DATA_ZVAL_PTR_DEREF`) and on one it cannot address gives the
/// value operand up unread, as `ZEND_ASSIGN_STATIC_PROP` does. The
/// operation is php's own binary kernel over what the slot holds and
/// the value (`binary_op(prop, prop, value)`), so `C::$p .= 1` and
/// `C::$p / 0` raise exactly what `.` and `/` raise; the temporary
/// takes what the slot now holds under `RETURN_VALUE_USED`.
///
/// ```php
/// class C { public static $p = 1; }
/// C::$p += 2;
/// var_dump(C::$p);
/// var_dump(C::$p .= "x");
/// C::$p = 7;
/// var_dump(C::$p **= 2, C::$p <<= 1, C::$p %= 5, C::$p |= 8);
/// ```
/// ```expect
/// int(3)
/// string(2) "3x"
/// int(49)
/// int(98)
/// int(3)
/// int(11)
/// ```
///
/// A property nothing declares is php's `Error` and the value is never
/// read, so the warning an undefined variable beside it would raise
/// never stands:
///
/// ```php
/// class C { public static $y = 1; }
/// C::$x += $undef;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Access to undeclared static property C::$x in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
/// A slot with a declared type is `zend_binary_assign_op_typed_prop`
/// (`Z/Zend/zend_execute.c:1693`): what the kernel made is held to the
/// type, and the weak rule converts it where the frame is not strict,
/// which is where the deprecation a lossy conversion raises comes from.
/// The temporary takes the value that landed and not the one the kernel
/// made.
///
/// ```php
/// class C { public static int $n = 1; }
/// var_dump(C::$n += 2);
/// var_dump(C::$n .= "5");
/// var_dump(C::$n /= 2);
/// ```
/// ```expect
/// int(3)
/// int(35)
///
/// Deprecated: Implicit conversion from float 17.5 to int loses precision in %s on line 5
/// int(17)
/// ```
///
/// What neither satisfies the type nor converts is php's `TypeError`,
/// and the slot keeps what it held (`zval_ptr_dtor(&z_copy)`):
///
/// ```php
/// class C { public static int $n = 1; }
/// try { C::$n .= "x"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(C::$n);
/// ```
/// ```expect
/// Cannot assign string to property C::$n of type int
/// int(1)
/// ```
pub(crate) fn assign_static_prop_op<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if op.op1_type != OperandKind::Const {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_STATIC_PROP_OP over a property named at run time",
        )));
    }
    let Some(kernel) = binary_op_of(op.extended_value) else {
        return Err(Fatal::Error(format!(
            "InternalError: ZEND_ASSIGN_STATIC_PROP_OP over the operator {} , which is no binary operation",
            op.extended_value
        )));
    };
    let at = req.vm().opline(frame);
    let Some(data) =
        exec::opline_at(code, at.saturating_add(1)).filter(|next| next.opcode == Opcode::OpData)
    else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_STATIC_PROP_OP with no OP_DATA after it",
        )));
    };
    let (data_type, data_at) = (data.op1_type, data.op1);
    let placed = match static_place(req, frame, code, op) {
        Ok(placed) => placed,
        Err(thrown) => {
            exec::free_operand(req, frame, code, data_type, data_at);
            return threw(req, frame, &thrown);
        }
    };
    let Some(Placed {
        class,
        declaring,
        name,
        scope,
        ty,
    }) = placed
    else {
        exec::free_operand(req, frame, code, data_type, data_at);
        req.vm_mut().jump(frame, at.saturating_add(2));
        return Ok(Stepped::Same(frame));
    };
    let value = exec::take_operand(req, frame, code, data_type, data_at);
    let held = match req.static_property(class, &name, scope, PropertyRead::Warning) {
        Ok(held) => held,
        Err(thrown) => {
            req.release(value);
            return threw(req, frame, &thrown);
        }
    };
    // php's `.=` over a string grows it where the slot holds it
    // (`zend_binary_assign_op_typed_prop`'s first arm, and
    // `zend_binary_op` over an untyped slot); `vacate` gives up the
    // read's owner and the slot's, so the append holds the only one.
    let in_place = is_concat(op.extended_value)
        && (held.as_php_string().is_some()
            || ty.is_none()
                && req
                    .static_slot_cell(declaring, &name)
                    .is_none_or(|cell| req.ref_type_sources(cell.heap()).is_empty()));
    let computed = if in_place {
        crate::ops::concat_assign(req, &held, &value, |req| {
            req.release(held);
            req.store_static_property(declaring, &name, Value::null());
        })
    } else {
        kernel(req, &held, &value)
    };
    // An `Err` never vacated, so the read's owner is still ours then.
    if !in_place || computed.is_err() {
        req.release(held);
    }
    req.release(value);
    let stepped = match computed {
        Ok(stepped) => stepped,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let strict = req.frame_strict_types(frame);
    let stepped = match ty {
        None => stepped,
        Some(ty) => match verified_op(req, &stepped, &ty, declaring, &name, strict) {
            Ok(None) => stepped,
            Ok(Some(coerced)) => {
                req.release(stepped);
                coerced
            }
            Err(thrown) => {
                // php gives the operation's value up and the slot keeps
                // what it held (`zval_ptr_dtor(&z_copy)`).
                req.release(stepped);
                return threw(req, frame, &thrown);
            }
        },
    };
    write_out(req, frame, code, op, &stepped);
    // The value is already the declared type's, so the store is php's
    // `ZVAL_COPY_VALUE(zptr, &z_copy)` and verifies nothing a second
    // time: a weak conversion raises a deprecation and php raises it
    // once.
    req.store_static_property(declaring, &name, stepped);
    req.vm_mut().jump(frame, at.saturating_add(2));
    Ok(Stepped::Same(frame))
}

/// `zend_binary_assign_op_typed_prop` (`Z/Zend/zend_execute.c:1693`):
/// the value the operation made, held to the declared type under the
/// frame's regime before it lands in the slot. Where the slot holds a
/// cell another name is bound to, php verifies against the cell's own
/// sources instead (`zend_binary_assign_op_typed_ref`), which is the
/// same type by a different sentence.
fn verified_op(
    req: &mut Request,
    stepped: &Value,
    ty: &php_unit::decl::Type,
    declaring: crate::api::values::ClassId,
    name: &[u8],
    strict: bool,
) -> Result<Option<Value>, Thrown> {
    match req.static_slot_cell(declaring, name) {
        Some(cell) if !req.ref_type_sources(cell.heap()).is_empty() => {
            crate::ops::verify_ref_assignable(req, cell.heap(), stepped, strict)
        }
        _ => crate::ops::verify_property_type(req, stepped, ty, declaring, name, strict),
    }
}

/// The body the four steps over a `static` property share: the slot
/// addressed as [`assign_static_prop_op`] addresses it, the kernel over
/// what it holds, the stepped value written back, and the temporary
/// under `RETURN_VALUE_USED` taking the value before the step for a
/// `POST_*` and the one after it for a `PRE_*`.
///
/// A typed slot is `zend_incdec_typed_prop`
/// (`Z/Zend/zend_execute.c:2262`): the stepped value held to the
/// declared type and coerced by it, the slot keeping what it held where
/// the value neither satisfies nor coerces. An `int` slot php would
/// have to step into a float is the one refusal that writes anyway,
/// `zend_throw_incdec_prop_error`, and what it writes is php's own
/// `ZEND_LONG_MAX` or `ZEND_LONG_MIN`.
fn static_prop_step(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    post: bool,
    increment: bool,
) -> Edge {
    let kernel: fn(&mut Request, &Value) -> Result<Value, Thrown> = if increment {
        crate::ops::increment
    } else {
        crate::ops::decrement
    };
    if op.op1_type != OperandKind::Const {
        return Err(Fatal::Error(String::from(
            "InternalError: a step over a static property named at run time",
        )));
    }
    let placed = match static_place(req, frame, code, op) {
        Ok(placed) => placed,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let Some(Placed {
        class,
        declaring,
        name,
        scope,
        ty,
    }) = placed
    else {
        return Ok(advance(req, frame));
    };
    let held = match req.static_property(class, &name, scope, PropertyRead::Warning) {
        Ok(held) => held,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let stepped = match kernel(req, &held) {
        Ok(stepped) => stepped,
        Err(thrown) => {
            req.release(held);
            return threw(req, frame, &thrown);
        }
    };
    let strict = req.frame_strict_types(frame);
    let stepped = match ty {
        None => stepped,
        Some(ty) if crate::ops::steps_past_int(&held, &stepped, &ty) => {
            let (thrown, limit) =
                crate::ops::incdec_prop_error(req, &ty, declaring, &name, increment);
            req.release(held);
            req.release(stepped);
            // php writes the limit into the slot and raises beside it,
            // so the slot stands at its maximal value after the catch.
            req.store_static_property(declaring, &name, limit);
            return threw(req, frame, &thrown);
        }
        Some(ty) => {
            match crate::ops::verify_property_type(req, &stepped, &ty, declaring, &name, strict) {
                Ok(None) => stepped,
                Ok(Some(coerced)) => {
                    req.release(stepped);
                    coerced
                }
                Err(thrown) => {
                    // `ZVAL_COPY_VALUE(var_ptr, copy)`: the slot keeps
                    // what it held and the stepped value is given up.
                    req.release(stepped);
                    req.release(held);
                    return threw(req, frame, &thrown);
                }
            }
        }
    };
    write_out(req, frame, code, op, if post { &held } else { &stepped });
    req.release(held);
    req.store_static_property(declaring, &name, stepped);
    Ok(advance(req, frame))
}

/// `ZEND_PRE_INC_STATIC_PROP` (`Z/Zend/zend_vm_def.h:6297`), `++C::$p`:
/// [`crate::ops::increment`] over what the slot holds, written back,
/// and the value after the step in the temporary under
/// `RETURN_VALUE_USED`. `C::$p++` written as a statement is this
/// opcode too, since `zend_do_free` turns a `POST_*` nothing reads into
/// the `PRE_*` beside it.
///
/// ```php
/// class C { public static $p = 1; public static $s = "a"; }
/// var_dump(++C::$p);
/// C::$p++;
/// var_dump(C::$p, C::$p++, C::$p);
/// var_dump(++C::$s);
/// ```
/// ```expect
/// int(2)
/// int(3)
/// int(3)
/// int(4)
///
/// Deprecated: Increment on non-numeric string is deprecated, use str_increment() instead in %s on line 6
/// string(1) "b"
/// ```
/// An `int` slot at `PHP_INT_MAX` is the one step php refuses rather
/// than widen: `zend_throw_incdec_prop_error` raises the `TypeError`
/// and the slot takes `ZEND_LONG_MAX`, so it stands at its maximal
/// value after the catch. A slot whose type accepts a float is stepped
/// into one.
///
/// ```php
/// class C { public static int $n = PHP_INT_MAX; public static int|float $f = PHP_INT_MAX; }
/// try { ++C::$n; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(C::$n);
/// var_dump(++C::$f);
/// ```
/// ```expect
/// Cannot increment property C::$n of type int past its maximal value
/// int(9223372036854775807)
/// float(9.223372036854776E+18)
/// ```
///
/// A declared type the stepped value does not satisfy converts it where
/// the weak rule can, so `++` over a `?string` holding null is php's
/// `"1"` and not the int the kernel made:
///
/// ```php
/// class C { public static ?string $s = null; }
/// var_dump(++C::$s);
/// ```
/// ```expect
/// string(1) "1"
/// ```
pub(crate) fn pre_inc_static_prop<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    static_prop_step(req, frame, code, op, false, true)
}

/// `ZEND_PRE_DEC_STATIC_PROP`, `--C::$p`: the same over
/// [`crate::ops::decrement`], which leaves a null where `++` makes it
/// `1` and warns as it does so.
///
/// ```php
/// class C { public static $p = 1; public static $n; }
/// var_dump(--C::$p);
/// var_dump(--C::$n);
/// ```
/// ```expect
/// int(0)
///
/// Warning: Decrement on type null has no effect, this will change in the next major version of PHP in %s on line 4
/// NULL
/// ```
pub(crate) fn pre_dec_static_prop<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    static_prop_step(req, frame, code, op, false, false)
}

/// `ZEND_POST_INC_STATIC_PROP`, `C::$p++` where the value is read: the
/// same step, with the value the slot held before it in the temporary.
///
/// ```php
/// class C { public static $p = 1; }
/// var_dump(C::$p++, C::$p--, C::$p);
/// ```
/// ```expect
/// int(1)
/// int(2)
/// int(1)
/// ```
pub(crate) fn post_inc_static_prop<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    static_prop_step(req, frame, code, op, true, true)
}

/// `ZEND_POST_DEC_STATIC_PROP`, `C::$p--` where the value is read: the
/// same step over [`crate::ops::decrement`], the value before it in the
/// temporary.
///
/// ```php
/// class C { public static $p = "9"; }
/// var_dump(C::$p--, C::$p);
/// ```
/// ```expect
/// string(1) "9"
/// int(8)
/// ```
/// An `int` slot at `PHP_INT_MIN` is the same refusal the other way,
/// `past its minimal value`, and the temporary is never read because
/// the `TypeError` stands where the expression did.
///
/// ```php
/// class C { public static int $n = PHP_INT_MIN; }
/// try { var_dump(C::$n--); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(C::$n);
/// ```
/// ```expect
/// Cannot decrement property C::$n of type int past its minimal value
/// int(-9223372036854775808)
/// ```
pub(crate) fn post_dec_static_prop<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    static_prop_step(req, frame, code, op, true, false)
}

/// `ZEND_ASSIGN_STATIC_PROP_REF` (`Z/Zend/zend_vm_def.h:2648`),
/// `C::$p = &$x`, two oplines: this one names the property in `op1` and
/// the class's constant pair in `op2`, and the `OP_DATA` after it
/// carries the source, so the handler steps over both. The slot is
/// addressed before the source is read, as every write to one is, and
/// binding is `zend_assign_to_variable_reference`: the source's own
/// cell where it stands in one already, else a new cell over what it
/// held, written back into the source so both names hold it
/// ([`Request::bind_static_property`]). `ZEND_RETURNS_FUNCTION` on
/// `extended_value`, beside the cache slot it shares the field with, is
/// the source that is a call, and a call that returned by value is
/// php's notice over a cell of its own.
///
/// ```php
/// class C { public static $p = 1; }
/// $x = 5;
/// C::$p = &$x;
/// $x = 7;
/// var_dump(C::$p);
/// C::$p = 9;
/// var_dump($x);
/// ```
/// ```expect
/// int(7)
/// int(9)
/// ```
/// A slot with a declared type is `zend_assign_to_typed_property_reference`
/// (`Z/Zend/zend_execute.c:583`): the source is held to the type before
/// the bind and converted in the cell where the weak rule can, so both
/// names read the converted value; then the type goes onto the cell
/// (`ZEND_REF_ADD_TYPE_SOURCE`), so a later write through the other
/// name is held to it too and raises php's `TypeError` naming the
/// property that holds the cell.
///
/// ```php
/// class C { public static int $n = 1; }
/// $x = "5";
/// C::$n = &$x;
/// var_dump($x, C::$n);
/// $x = "7";
/// var_dump(C::$n);
/// try { $x = "q"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($x, C::$n);
/// ```
/// ```expect
/// int(5)
/// int(5)
/// int(7)
/// Cannot assign string to reference held by property C::$n of type int
/// int(7)
/// int(7)
/// ```
///
/// A source the type refuses outright is php's `TypeError` for the
/// property and no bind at all:
///
/// ```php
/// class C { public static int $n = 1; }
/// $x = "q";
/// C::$n = &$x;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Cannot assign string to property C::$n of type int in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
///
/// Two typed properties may hold one cell only where they agree on the
/// value, which is `zend_throw_ref_type_error_type` where the second
/// would have to convert what the first holds:
///
/// ```php
/// class C { public static int $a = 1; public static string $b = "x"; }
/// $x = 1;
/// C::$a = &$x;
/// try { C::$b = &$x; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($x, C::$a, C::$b);
/// ```
/// ```expect
/// Reference with value of type int held by property C::$a of type int is not compatible with property C::$b of type string
/// int(1)
/// int(1)
/// string(1) "x"
/// ```
pub(crate) fn assign_static_prop_ref<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if op.op1_type != OperandKind::Const {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_STATIC_PROP_REF over a property named at run time",
        )));
    }
    let at = req.vm().opline(frame);
    let Some(data) =
        exec::opline_at(code, at.saturating_add(1)).filter(|next| next.opcode == Opcode::OpData)
    else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_STATIC_PROP_REF with no OP_DATA after it",
        )));
    };
    let (data_type, data_at) = (data.op1_type, data.op1);
    let placed = match static_place(req, frame, code, op) {
        Ok(placed) => placed,
        Err(thrown) => {
            exec::free_operand(req, frame, code, data_type, data_at);
            return threw(req, frame, &thrown);
        }
    };
    let Some(Placed {
        class,
        declaring,
        name,
        scope,
        ty,
    }) = placed
    else {
        exec::free_operand(req, frame, code, data_type, data_at);
        req.vm_mut().jump(frame, at.saturating_add(2));
        return Ok(Stepped::Same(frame));
    };
    let Some(source) = write_place(req, frame, code, data_type, data_at) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_STATIC_PROP_REF over a source php compiles no write over",
        )));
    };
    let held = exec::value_of(source.read());
    let returns_function = op.extended_value & RETURNS_FUNCTION != 0 && held.as_php_ref().is_none();
    if returns_function {
        crate::api::raising::notice(req, "Only variables should be assigned by reference");
        if req.is_pending() {
            return Ok(Stepped::Threw(frame));
        }
    }
    let cell = if let Some(cell) = held.as_php_ref() {
        *cell
    } else {
        let held = held.undef_as_null();
        let cell = match req.reference(held) {
            Ok(cell) => cell,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        // The owner the source had of `held` is the cell's now.
        source.write(cell.as_value().to_zval());
        cell
    };
    // `zend_assign_to_typed_property_reference`
    // (`Z/Zend/zend_execute.c:583`): the cell is held to the declared
    // type before it is bound, and the type goes onto the cell, so a
    // write through any name bound to it is held to the type too. php
    // takes this arm only where the source is a place; a call that
    // returned by value is the notice and an assignment by value.
    let typed = ty.filter(|_| !returns_function);
    if let Some(ty) = typed.as_ref() {
        let strict = req.frame_strict_types(frame);
        match crate::ops::verify_prop_assignable_by_ref(
            req,
            cell.heap(),
            ty,
            declaring,
            &name,
            strict,
        ) {
            Ok(None) => {}
            // The conversion lands in the cell, so the name bound to it
            // reads the converted value too.
            Ok(Some(coerced)) => req.reference_set(cell.heap(), coerced),
            Err(thrown) => return threw(req, frame, &thrown),
        }
    }
    // `ZEND_REF_DEL_TYPE_SOURCE`: the cell the slot held is no longer
    // typed by this property.
    if let Some(old) = req.static_slot_cell(declaring, &name) {
        req.ref_del_type_source(old.heap(), declaring, &name);
    }
    let bound = cell.as_value();
    req.addref(&bound);
    write_out(req, frame, code, op, &bound);
    if let Err(thrown) = req.bind_static_property(class, &name, scope, bound) {
        return threw(req, frame, &thrown);
    }
    if let Some(ty) = typed {
        req.ref_add_type_source(
            cell.heap(),
            crate::api::request::RefSource {
                declaring,
                name: name.clone(),
                ty,
            },
        );
    }
    req.vm_mut().jump(frame, at.saturating_add(2));
    Ok(Stepped::Same(frame))
}

/// `zend_assign_to_variable` (`Z/Zend/zend_execute.h:171`) over a place:
/// a reference standing there is followed and the write lands in the
/// cell, as `ASSIGN` over a CV bound by `&` lands there; the value is
/// written first and what the place held is handed back for the caller
/// to release last, after the `result` is copied, which is where a
/// destructor is served (design §5.10).
///
/// A cell a typed property's slot holds is php's one other arm,
/// `ZEND_REF_HAS_TYPE_SOURCES` into `zend_assign_to_typed_ref`: the
/// value is held to each type the cell carries and may be converted by
/// them, so what landed is not always what was written. The pair is
/// that value, a view the caller copies into `result`, and what the
/// place held, which the caller releases last.
fn assign_to_place(
    req: &mut Request,
    at: php_heap::ZvalRef,
    value: Value,
    strict: bool,
) -> Result<(Value, Value), Thrown> {
    let mut at = at;
    let mut held = exec::value_of(at.read());
    if let Some(cell) = held.as_php_ref() {
        let cell = *cell;
        if !req.ref_type_sources(cell.heap()).is_empty() {
            req.assign_to_reference(cell.heap(), value, strict)?;
            return Ok((cell.get(), Value::default()));
        }
        at = cell.heap().val_ref();
        held = exec::value_of(at.read());
    }
    at.write(value.to_zval());
    Ok((value, held))
}

/// `zend_assign_to_object_dim` (`Z/Zend/zend_execute.c:1558`), the arm
/// `ZEND_ASSIGN_DIM` takes over an object: the class's `write_dimension`,
/// at `Std` `offsetSet` with the dimension and the value
/// ([`Request::write_dimension`]), `null` as the dimension for
/// `$o[] = v`, and php's `Error` `Cannot use object of type C as array`
/// for a class that is no `ArrayAccess`. The temporary `out` names one
/// more owner of the value under `RETURN_VALUE_USED`, which is the value
/// written and never what `offsetSet` returned. A class of an extension
/// is written through its own slot, as `MultipleIterator`'s
/// `spl_multiple_iterator_write_dimension` attaches an iterator though
/// the class is no `ArrayAccess`; its reads go through `read_dimension`
/// the same way.
///
/// ```php
/// $m = new MultipleIterator();
/// $m[new ArrayIterator([1, 2])] = 'a';
/// try { $m[new stdClass] = 'b'; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($m->countIterators());
/// ```
/// ```expect
/// Can only attach objects that implement the Iterator interface
/// int(1)
/// ```
fn assign_object_dim(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    data: Opline,
    object: PhpObject,
) -> Result<(), Thrown> {
    let dim = object_dim(req, frame, code, op);
    let value = exec::take_operand(req, frame, code, data.op1_type, data.op1);
    req.addref(&value);
    let table = req.object_handlers(object.heap());
    let served = table.write_dimension(req, object, dim.as_ref(), value);
    let answer = match served {
        Ok(()) => {
            write_out(req, frame, code, op, &value);
            Ok(())
        }
        Err(thrown) => Err(thrown),
    };
    req.release(value);
    answer
}

/// The arm `ZEND_ASSIGN_DIM` takes over a string: `$s[] = v` is php's
/// `zend_use_new_element_for_string`, read before the value is, and a
/// dimension is [`assign_string_offset`], whose byte is what the
/// temporary `out` names under `RETURN_VALUE_USED`.
fn assign_text_dim(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    data: Opline,
    place: php_heap::ZvalRef,
) -> Result<(), Thrown> {
    if op.op2_type == OperandKind::Unused {
        return Err(crate::api::raising::throw(
            req,
            "Error",
            "[] operator not supported for strings",
        ));
    }
    let dim = exec::operand(req, frame, code, op.op2_type, op.op2);
    let value = exec::take_operand(req, frame, code, data.op1_type, data.op1);
    let byte = assign_string_offset(req, place, &dim, &value);
    req.release(value);
    let byte = byte?;
    write_out(req, frame, code, op, &byte);
    req.release(byte);
    Ok(())
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
/// An object is `zend_assign_to_object_dim`: `offsetSet` with the
/// dimension and the value, `null` as the dimension for `$o[] = v`, and
/// php's `Error` `Cannot use object of type C as array` for a class that
/// is no `ArrayAccess`. A string is [`assign_string_offset`].
///
/// ```php
/// class C implements ArrayAccess {
///     public function offsetExists($offset): bool { return true; }
///     public function offsetGet($offset): mixed { var_dump("get", $offset); return 7; }
///     public function offsetSet($offset, $value): void { var_dump("set", $offset, $value); }
///     public function offsetUnset($offset): void {}
/// }
/// $c = new C;
/// $c[0] = 1;
/// $c[] = 2;
/// var_dump($c["k"]);
/// ```
/// ```expect
/// string(3) "set"
/// int(0)
/// int(1)
/// string(3) "set"
/// NULL
/// int(2)
/// string(3) "get"
/// string(1) "k"
/// int(7)
/// ```
///
/// ```php
/// $o = new stdClass;
/// $o[0] = 1;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot use object of type stdClass as array in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
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
    let written = match write_container(req, place) {
        Ok(Container::Array(array)) if op.op2_type == OperandKind::Unused => {
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
        Ok(Container::Array(array)) => {
            let dim = exec::operand(req, frame, code, op.op2_type, op.op2);
            let entry = write_entry(req, array, Some(&dim), op.op2_type == OperandKind::Const)?;
            match entry {
                Ok(Some(entry)) => {
                    let value = exec::take_operand(req, frame, code, data.op1_type, data.op1);
                    let strict = req.frame_strict_types(frame);
                    match assign_to_place(req, entry, value, strict) {
                        Ok((landed, garbage)) => {
                            write_out(req, frame, code, op, &landed);
                            req.release(garbage);
                            Ok(())
                        }
                        Err(thrown) => Err(thrown),
                    }
                }
                Ok(None) => Err(crate::api::raising::throw(
                    req,
                    "Error",
                    "Cannot add element to the array as the next element is already occupied",
                )),
                Err(thrown) => Err(thrown),
            }
        }
        Ok(Container::Object(object)) => assign_object_dim(req, frame, code, op, data, object),
        Ok(Container::Text(place)) => assign_text_dim(req, frame, code, op, data, place),
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

/// `ZEND_SEPARATE` (`Z/Zend/zend_vm_def.h:8370`), the opline
/// `zend_separate_if_call_and_write` puts after a call whose `VAR` a
/// write is about to land in: a cell nothing else holds is unwrapped in
/// place (`ZVAL_UNREF`), so the write reaches the value and not the
/// cell, and a cell more than one name holds is left standing, which is
/// how `g()[0] = 7` over a function returning by reference still writes
/// through to what `g` returned.
///
/// ```php
/// function f() { return [5, 6]; }
/// f()[0] = 9;
/// echo "written and gone\n";
/// ```
/// ```expect
/// written and gone
/// ```
///
/// The same opline under a property write (#336), which is where a
/// chain rooted at a call lands: the write reaches the object the call
/// answered with, whether the call answered by value or by reference.
///
/// ```php
/// class C { public $p = 0; }
/// function f($o) { return $o; }
/// function &r(&$x) { return $x; }
/// $c = new C;
/// f($c)->p = 7;
/// echo $c->p, "\n";
/// r($c)->p = 9;
/// echo $c->p, "\n";
/// ```
/// ```expect
/// 7
/// 9
/// ```
pub(crate) fn separate<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let slot = exec::temporary_slot(code, op.op1);
    let held = req.slot(frame, slot);
    let Some(cell) = held.as_php_ref().copied() else {
        return Ok(advance(req, frame));
    };
    if cell.heap().refcount() != 1 {
        return Ok(advance(req, frame));
    }
    // The value the cell held is the slot's from here, and the cell is
    // given up after the write, which is what frees it: one owner is
    // taken for the slot before the cell lets its own go.
    let inner = cell.get();
    req.addref(&inner);
    let previous = req.set_slot(frame, slot, inner);
    req.release(previous);
    Ok(advance(req, frame))
}

/// The two oplines a compound assignment to a place is, and the kernel
/// its operator names: what every arm of `ASSIGN_DIM_OP` and
/// `ASSIGN_OBJ_OP` is handed beside the request and the frame.
#[derive(Clone, Copy)]
struct Compound<'a> {
    op: &'a Opline,
    data: Opline,
    kernel: BinaryKernel,
}

/// Where a dimension write under `BP_VAR_RW` lands, what
/// `zend_fetch_dimension_address_inner` answers for it.
enum Landing {
    /// The entry at the key, in place.
    Entry(php_heap::ZvalRef),
    /// `$a[]` with `nNextFreeElement` at `ZEND_LONG_MAX` taken, which
    /// is `zend_cannot_add_element`'s `Error` at the site.
    Occupied,
    /// php's `NULL` with nothing thrown: the handler the `Undefined
    /// array key` warning reached destroyed the array or took an owner
    /// of it, so there is no entry to write into and the site's
    /// temporary takes `null` ([`undefined_key_write`]).
    Gone,
}

impl From<Option<php_heap::ZvalRef>> for Landing {
    fn from(entry: Option<php_heap::ZvalRef>) -> Landing {
        entry.map_or(Landing::Occupied, Landing::Entry)
    }
}

/// The entry a compound assignment to a dimension lands in, php's
/// `zend_fetch_dimension_address_inner_RW` (`Z/Zend/zend_execute.c`):
/// the entry at the key, laid down holding `null` where the table has
/// none after php's `Undefined array key` warning
/// ([`undefined_key_write`]), which is the one thing the `BP_VAR_W`
/// walk ([`write_entry`]) does not raise. The `UNUSED` dimension of
/// `$a[] op= v` is an entry appended with nothing in it
/// (`zend_hash_next_index_insert(ht, &EG(uninitialized_zval))`), which
/// raises no warning at all.
fn rw_entry(
    req: &mut Request,
    array: PhpArray,
    dim: Option<&Value>,
    constant: bool,
) -> Result<Result<Landing, Thrown>, Fatal> {
    let Some(dim) = dim else {
        return Ok(req.array_next_entry(array.heap()).map(Landing::from));
    };
    let key = match crate::ops::array_offset(req, dim, constant) {
        Ok(key) => key,
        Err(thrown) => return Ok(Err(thrown)),
    };
    if array.heap().get(key).is_none() {
        return Ok(undefined_key_write(req, array, key));
    }
    Ok(req.array_lookup(array.heap(), key).map(Landing::from))
}

/// `zend_undefined_offset_write` and `zend_undefined_index_write`
/// (`Z/Zend/zend_execute.c:2507-2550`): the `Undefined array key`
/// warning for a key `array` has not got, and the entry laid down
/// holding `null` after it. The warning may reach a handler that
/// overwrites the variable holding the array, so the array is held
/// across it, `GC_ADDREF(ht)`, and a string key too, which the handler
/// may free as well. Where the array has any owner but that one and
/// its own after the warning, it is given up, `zend_array_destroy` at
/// the last, and nothing is written: [`Landing::Gone`]. An immutable
/// array is never held, as php's `IS_ARRAY_IMMUTABLE` test says.
///
/// ```php
/// $v = null;
/// set_error_handler(function () use (&$v) { $v = 0; });
/// $v[0] .= "xyz";
/// var_dump($v);
/// $v = null;
/// $v["k"]["j"] .= "xyz";
/// var_dump($v);
/// set_error_handler(function ($no, $message) use (&$v, &$copy) {
///     echo $message, "\n";
///     $copy = $v;
/// });
/// $v = [];
/// $v[5] .= "x";
/// $v[1][2] .= "y";
/// var_dump($v, $copy);
/// ```
/// ```expect
/// int(0)
/// int(0)
/// Undefined array key 5
/// Undefined array key 1
/// Undefined array key 2
/// array(0) {
/// }
/// array(0) {
/// }
/// ```
fn undefined_key_write(
    req: &mut Request,
    array: PhpArray,
    key: php_heap::Key,
) -> Result<Landing, Thrown> {
    let held = (!array.heap().is_immutable()).then_some(Value::array(array));
    if let Some(held) = &held {
        req.addref(held);
    }
    let name = match key {
        php_heap::Key::Str(s) => Some(exec::value_of(php_heap::Zval::string(s))),
        php_heap::Key::Long(_) => None,
    };
    if let Some(name) = &name {
        req.addref(name);
    }
    let message = match key {
        php_heap::Key::Long(n) => format!("Undefined array key {n}"),
        php_heap::Key::Str(s) => format!(
            "Undefined array key \"{}\"",
            String::from_utf8_lossy(s.bytes())
        ),
    };
    crate::api::raising::warning(req, &message);
    let gone = held.is_some_and(|held| {
        let owners = held.refcount();
        req.release_nogc(held);
        owners != 2
    });
    let landing = if req.is_pending() {
        Err(Thrown::of(None))
    } else if gone {
        Ok(Landing::Gone)
    } else {
        req.array_lookup(array.heap(), key).map(Landing::from)
    };
    if let Some(name) = name {
        req.release(name);
    }
    landing
}

/// `ZEND_ASSIGN_DIM_OP` (`Z/Zend/zend_vm_def.h:1156`), `$a[k] op= v`
/// and `$a[] op= v`, two oplines: this one names the container in
/// `op1`, the dimension in `op2` and the operator in `extended_value`,
/// and the `OP_DATA` after it carries the value, so the handler steps
/// over both. A dimension fetch has no cache slot, so the `OP_DATA`
/// carries none where `ASSIGN_OBJ_OP`'s carries one.
///
/// The container is walked as every dimension write walks it
/// ([`write_container`]): a reference followed, an array separated,
/// `null`, an unwritten variable and `false` made into a new array with
/// php's deprecation for the `false`. The entry is [`rw_entry`]'s,
/// which is the `BP_VAR_W` walk with php's `Undefined array key`
/// warning before it, and the operation is php's own binary kernel over
/// what the entry holds and the value, so `$a[0] /= 0` raises exactly
/// what `/` raises and the entry keeps what it held. What landed is
/// what the temporary takes under `RETURN_VALUE_USED`, which a cell
/// carrying types may have made something else.
///
/// An object is `zend_binary_assign_op_obj_dim`
/// (`Z/Zend/zend_execute.c:1638`): `offsetGet` under `BP_VAR_R`, the
/// kernel over what it answered, and `offsetSet` with what it made, with
/// php's `Error` `Cannot use object of type C as array` for a class
/// that is no `ArrayAccess`. A string is
/// `zend_binary_assign_op_dim_slow`: `Cannot use assign-op operators
/// with string offsets` for a dimension and `[] operator not supported
/// for strings` for the append. A number, `true` and a resource are
/// `Cannot use a scalar value as an array`, which the walk itself
/// raises.
///
/// ```php
/// $a = [1];
/// $a[0] += 2;
/// $a[0] .= "x";
/// var_dump($a);
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   string(2) "3x"
/// }
/// ```
///
/// The append names no key, so it raises no warning and the entry it
/// laid down holds nothing for the operator to read:
///
/// ```php
/// $a = [1];
/// $a[0] += 2;
/// $a[0] .= "x";
/// $a[] .= "z";
/// var_dump($a);
/// $b = [];
/// $b["k"] += 5;
/// var_dump($b);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(2) "3x"
///   [1]=>
///   string(1) "z"
/// }
///
/// Warning: Undefined array key "k" in %s on line 8
/// array(1) {
///   ["k"]=>
///   int(5)
/// }
/// ```
///
/// A variable nothing wrote becomes the array, which is the warning for
/// the variable and then the one for the key; a `false` takes php's
/// deprecation before both.
///
/// ```php
/// $u[0] += 1;
/// var_dump($u);
/// $f = false;
/// $f[0] += 1;
/// var_dump($f);
/// ```
/// ```expect
/// Warning: Undefined variable $u in %s on line 2
///
/// Warning: Undefined array key 0 in %s on line 2
/// array(1) {
///   [0]=>
///   int(1)
/// }
///
/// Deprecated: Automatic conversion of false to array is deprecated in %s on line 5
///
/// Warning: Undefined array key 0 in %s on line 5
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// ```
///
/// A string offset, an append over a string and a scalar are php's
/// three `Error`s, and an `ArrayAccess` is `offsetGet` and then
/// `offsetSet`:
///
/// ```php
/// $s = "abc";
/// try { $s[0] += 1; } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// try { $s[] .= "q"; } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// $n = 5;
/// try { $n[0] += 1; } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// class A implements ArrayAccess {
///     public function offsetExists($o): bool { return true; }
///     public function offsetGet($o): mixed { echo "get $o\n"; return 1; }
///     public function offsetSet($o, $v): void { echo "set $o = "; var_dump($v); }
///     public function offsetUnset($o): void {}
/// }
/// $x = new A;
/// $x[2] += 10;
/// $o = new stdClass;
/// try { $o[0] += 1; } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Error: Cannot use assign-op operators with string offsets
/// Error: [] operator not supported for strings
/// Error: Cannot use a scalar value as an array
/// get 2
/// set 2 = int(11)
/// Error: Cannot use object of type stdClass as array
/// ```
pub(crate) fn assign_dim_op<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(kernel) = binary_op_of(op.extended_value) else {
        return Err(Fatal::Error(format!(
            "InternalError: ZEND_ASSIGN_DIM_OP over the operator {}, which is no binary operation",
            op.extended_value
        )));
    };
    let at = req.vm().opline(frame);
    let Some(data) =
        exec::opline_at(code, at.saturating_add(1)).filter(|next| next.opcode == Opcode::OpData)
    else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_DIM_OP with no OP_DATA after it",
        )));
    };
    let data = *data;
    let step = Compound { op, data, kernel };
    let Some(place) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_DIM_OP over a constant or a temporary, which php compiles no write over",
        )));
    };
    // `ZVAL_UNDEFINED_OP1()`: a CV nothing wrote is the warning php
    // raises in the arm that makes the array, before the array is made
    // and so before the key's own warning. `ZEND_ASSIGN_DIM` raises
    // none, since a write alone creates.
    if op.op1_type == OperandKind::Cv && req.slot(frame, op.op1).tag() == TypeTag::Undef {
        let undefined = exec::operand(req, frame, code, OperandKind::Cv, op.op1);
        req.release(undefined);
    }
    let written = match write_container(req, place) {
        Ok(Container::Array(array)) => {
            let dim = (op.op2_type != OperandKind::Unused)
                .then(|| exec::operand(req, frame, code, op.op2_type, op.op2));
            match rw_entry(req, array, dim.as_ref(), op.op2_type == OperandKind::Const)? {
                Ok(Landing::Entry(entry)) => stepped_entry(req, frame, code, step, entry),
                // `assign_dim_op_ret_null`: `FREE_OP_DATA` and a `null`
                // result.
                Ok(Landing::Gone) => {
                    exec::free_operand(req, frame, code, data.op1_type, data.op1);
                    write_out(req, frame, code, op, &Value::null());
                    Ok(())
                }
                Ok(Landing::Occupied) => Err(crate::api::raising::throw(
                    req,
                    "Error",
                    "Cannot add element to the array as the next element is already occupied",
                )),
                Err(thrown) => Err(thrown),
            }
        }
        Ok(Container::Object(object)) => binary_assign_op_obj_dim(req, frame, code, step, object),
        Ok(Container::Text(_)) => Err(crate::api::raising::throw(
            req,
            "Error",
            if op.op2_type == OperandKind::Unused {
                "[] operator not supported for strings"
            } else {
                "Cannot use assign-op operators with string offsets"
            },
        )),
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

/// The kernel over what one place holds and the `OP_DATA`'s value, and
/// the write back into that place: php's `zend_binary_op(var_ptr,
/// var_ptr, value)` with the reference followed and the cell's types
/// held to where it carries any ([`assign_to_place`], which is
/// `zend_binary_assign_op_typed_ref`). What landed is what the
/// temporary takes under `RETURN_VALUE_USED`, and the value operand is
/// given up either way, as `FREE_OP_DATA` gives it up.
fn stepped_entry(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    step: Compound<'_>,
    entry: php_heap::ZvalRef,
) -> Result<(), Thrown> {
    let Compound { op, data, kernel } = step;
    let value = exec::operand(req, frame, code, data.op1_type, data.op1);
    let raw = exec::value_of(entry.read());
    let held = raw.deref();
    let computed = if grows_in_place(req, op.extended_value, &raw) {
        crate::ops::concat_assign(req, &held, &value, |req| vacate(req, Some(entry)))
    } else {
        kernel(req, &held, &value)
    };
    exec::free_operand(req, frame, code, data.op1_type, data.op1);
    let stepped = computed?;
    let strict = req.caller_strict_types(frame);
    let (landed, garbage) = assign_to_place(req, entry, stepped, strict)?;
    write_out(req, frame, code, op, &landed);
    req.release(garbage);
    Ok(())
}

/// `zend_binary_assign_op_obj_dim` (`Z/Zend/zend_execute.c:1638`), the
/// arm `ZEND_ASSIGN_DIM_OP` takes over an object: `offsetGet` with the
/// dimension under `BP_VAR_R`, the kernel over what it answered and the
/// value, and `offsetSet` with what it made. php's `Error` `Cannot use
/// object of type C as array` is what a class that is no `ArrayAccess`
/// raises, and the temporary takes what the operation made and
/// never what `offsetSet` returned.
fn binary_assign_op_obj_dim(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    step: Compound<'_>,
    object: PhpObject,
) -> Result<(), Thrown> {
    let Compound { op, data, kernel } = step;
    let dim = object_dim(req, frame, code, op);
    let value = exec::operand(req, frame, code, data.op1_type, data.op1);
    let table = req.object_handlers(object.heap());
    let read = table.read_dimension(req, object, dim.as_ref());
    let held = match read {
        Ok(held) => held,
        Err(thrown) => {
            exec::free_operand(req, frame, code, data.op1_type, data.op1);
            return Err(thrown);
        }
    };
    let computed = kernel(req, &held, &value);
    req.release(held);
    exec::free_operand(req, frame, code, data.op1_type, data.op1);
    let stepped = computed?;
    let written = table.write_dimension(req, object, dim.as_ref(), stepped);
    if let Err(thrown) = written {
        req.release(stepped);
        return Err(thrown);
    }
    write_out(req, frame, code, op, &stepped);
    req.release(stepped);
    Ok(())
}

/// `ZEND_ASSIGN_OBJ_OP` (`Z/Zend/zend_vm_def.h:1008`), `$o->p op= v`,
/// two oplines: this one names the receiver in `op1`, the property in
/// `op2` and the operator in `extended_value`, and the `OP_DATA` after
/// it carries the value and the three cache slots the fetch's own
/// `extended_value` held, so the handler steps over both.
///
/// php addresses the property through `get_property_ptr_ptr` under
/// `BP_VAR_RW` and works over the slot in place, falling back to
/// `read_property` and `write_property` where a class hands out no
/// place (`zend_assign_op_overloaded_property`). Here a property the
/// class declared is the slot in place, held to its declared type where
/// it carries one (`zend_binary_assign_op_typed_prop`), and every other
/// name is the pair of handlers, which is php's overloaded arm and is
/// where `__get` and `__set` are served. A name no class declared and
/// no object holds yet is created by the write, so this engine raises
/// its `Undefined property` warning before the deprecation for the
/// dynamic property where php raises the deprecation first (#505).
///
/// ```php
/// class C { public $p = 1; public int $n = 2; private $q = 3; }
/// $o = new C;
/// $o->p |= 3;
/// $o->n += 1;
/// var_dump($o->p, $o->n);
/// try { $o->q .= "x"; } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// try { $o->n .= "zz"; } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// var_dump($o->n);
/// ```
/// ```expect
/// int(3)
/// int(3)
/// Error: Cannot access private property C::$q
/// TypeError: Cannot assign string to property C::$n of type int
/// int(3)
/// ```
///
/// A receiver that is no object is php's `Error`, the value never read:
///
/// ```php
/// $u = null;
/// try { $u->w += 1; } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// $i = 3;
/// try { $i->w += 1; } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Error: Attempt to assign property "w" on null
/// Error: Attempt to assign property "w" on int
/// ```
pub(crate) fn assign_obj_op<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(kernel) = binary_op_of(op.extended_value) else {
        return Err(Fatal::Error(format!(
            "InternalError: ZEND_ASSIGN_OBJ_OP over the operator {}, which is no binary operation",
            op.extended_value
        )));
    };
    let name = match op.op2_type {
        OperandKind::Const => exec::constant(code, ConstIdx(op.op2)),
        _ => {
            return Err(Fatal::Error(String::from(
                "InternalError: ZEND_ASSIGN_OBJ_OP over a property named at run time",
            )));
        }
    };
    let at = req.vm().opline(frame);
    let Some(data) =
        exec::opline_at(code, at.saturating_add(1)).filter(|next| next.opcode == Opcode::OpData)
    else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_ASSIGN_OBJ_OP with no OP_DATA after it",
        )));
    };
    let data = *data;
    let step = Compound { op, data, kernel };
    let container =
        exec::container(req, frame, code, op.op1_type, op.op1, "ZEND_ASSIGN_OBJ_OP")?.deref();
    let written = if let Some(object) = container.as_object().copied() {
        step_property(req, frame, code, step, object, &name)?
    } else {
        let shown_name = String::from_utf8_lossy(name.as_str_bytes().unwrap_or_default());
        let shown = crate::ops::value_name(req, &container.undef_as_null());
        let thrown = crate::api::raising::throw(
            req,
            "Error",
            &format!("Attempt to assign property \"{shown_name}\" on {shown}"),
        );
        exec::free_operand(req, frame, code, data.op1_type, data.op1);
        write_out(req, frame, code, op, &Value::null());
        Err(thrown)
    };
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    if let Err(thrown) = written {
        return threw(req, frame, &thrown);
    }
    req.vm_mut().jump(frame, at.saturating_add(2));
    Ok(Stepped::Same(frame))
}

/// `ASSIGN_OBJ_OP`'s body over an object: a property the class declared
/// is the slot in place, the kernel over what it holds and the value,
/// and the write back through [`Request::assign_declared_slot`], held
/// to the declared type where the slot carries one; every other name is
/// `read_property` and `write_property`, php's
/// `zend_assign_op_overloaded_property`.
fn step_property(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    step: Compound<'_>,
    object: PhpObject,
    name: &Value,
) -> Result<Result<(), Thrown>, Fatal> {
    let Compound { op, data, .. } = step;
    let bytes = name.as_str_bytes().unwrap_or_default().to_vec();
    // `zend_std_get_property_ptr_ptr`, whose silence is
    // `ce->__get != NULL`: a class with a `__get` hands out no place
    // for a property the scope may not reach and takes the overloaded
    // arm over it instead.
    let silent = req.serves_accessor(object.class(), b"__get");
    let placed = match req.declared_slot_for_write(object.class(), &bytes, "modify", silent) {
        Ok(placed) => placed,
        Err(thrown) => {
            exec::free_operand(req, frame, code, data.op1_type, data.op1);
            write_out(req, frame, code, op, &Value::null());
            return Ok(Err(thrown));
        }
    };
    let Some((slot, typed)) = placed else {
        let Some(key) = name.as_php_string().copied() else {
            return Err(Fatal::Error(String::from(
                "InternalError: ZEND_ASSIGN_OBJ_OP over a property whose name is no string",
            )));
        };
        // `zend_std_get_property_ptr_ptr`'s two answers for a name no
        // class declared: a class with a `__get` the guard is not
        // holding hands out no place and the overloaded pair takes the
        // site; every other class takes the place, which is the dynamic
        // entry laid down here.
        if req.overloaded_place(object, &bytes) {
            return Ok(overloaded_property_step(
                req, frame, code, step, object, key,
            ));
        }
        return Ok(dynamic_property_step(req, frame, code, step, object, key));
    };
    // `zend_std_get_property_ptr_ptr`'s valid-offset arm
    // (`Z/Zend/zend_object_handlers.c:1429`): a slot nothing has
    // written is the read's own diagnostic under `BP_VAR_RW`, raised
    // before the value is read, and then `ZVAL_NULL` over the slot. A
    // `__get` the guard is not holding serves an untyped one instead,
    // its class handing out no place at all; a typed slot takes the
    // uninitialised-access error whatever the class serves, which is
    // php's `prop_info && (Z_PROP_FLAG_P(retval) & IS_PROP_UNINIT)`.
    if object.heap().slot(slot).is_undef() {
        let shown = String::from_utf8_lossy(&bytes).into_owned();
        if typed {
            let class = req.class_name(object.class());
            let thrown = crate::api::raising::throw(
                req,
                "Error",
                &format!(
                    "Typed property {class}::${shown} must not be accessed before initialization"
                ),
            );
            exec::free_operand(req, frame, code, data.op1_type, data.op1);
            write_out(req, frame, code, op, &Value::null());
            return Ok(Err(thrown));
        }
        if req.overloaded_place(object, &bytes) {
            let Some(key) = name.as_php_string().copied() else {
                return Err(Fatal::Error(String::from(
                    "InternalError: ZEND_ASSIGN_OBJ_OP over a property whose name is no string",
                )));
            };
            return Ok(overloaded_property_step(
                req, frame, code, step, object, key,
            ));
        }
        let class = req.class_name(object.class());
        crate::api::raising::warning(req, &format!("Undefined property: {class}::${shown}"));
    }
    let value = exec::operand(req, frame, code, data.op1_type, data.op1);
    let computed = slot_step(req, step, object, (slot, typed), &value);
    exec::free_operand(req, frame, code, data.op1_type, data.op1);
    let stepped = match computed {
        Ok(stepped) => stepped,
        Err(thrown) => {
            write_out(req, frame, code, op, &Value::null());
            return Ok(Err(thrown));
        }
    };
    if !typed {
        write_out(req, frame, code, op, &stepped);
        req.assign_declared_slot(object.heap(), slot, stepped);
        return Ok(Ok(()));
    }
    // `zend_binary_assign_op_typed_prop`: what the kernel made is held
    // to the declared type under this frame's own regime, and what
    // landed is what the temporary takes.
    let Some(typed) = req.declared_typed_property(object.class(), &bytes) else {
        req.release(stepped);
        let shown = String::from_utf8_lossy(&bytes);
        return Err(Fatal::Error(format!(
            "InternalError: ZEND_ASSIGN_OBJ_OP placed the typed property ${shown}, whose declared type the class does not carry"
        )));
    };
    let strict = req.caller_strict_types(frame);
    let landed = match crate::ops::verify_property_type(
        req,
        &stepped,
        &typed.ty,
        typed.declaring,
        &bytes,
        strict,
    ) {
        Ok(None) => stepped,
        Ok(Some(coerced)) => {
            req.release(stepped);
            coerced
        }
        Err(thrown) => {
            req.release(stepped);
            write_out(req, frame, code, op, &Value::null());
            return Ok(Err(thrown));
        }
    };
    write_out(req, frame, code, op, &landed);
    req.assign_declared_slot(object.heap(), slot, landed);
    Ok(Ok(()))
}

/// `ZEND_ASSIGN_OBJ_OP` over the place `zend_std_get_property_ptr_ptr`
/// hands out for a name no class declared: the dynamic entry, taken
/// before the value is read ([`Request::dynamic_property_ref`]) and so
/// created with php's deprecation before the read's `Undefined
/// property` warning, which is the order the pair of handlers raises
/// them in the other way round.
///
/// php runs the kernel over that place, `zend_binary_op(zptr, zptr,
/// value)`. Here the place is taken and then read and written by the
/// same pair the overloaded arm uses: with the entry standing, the read
/// finds it and raises nothing, and the write lands in it and raises
/// nothing, so what the two do to the object is what the one kernel
/// over the cell does.
///
/// ```php
/// class C { public $p = 1; public $u; public int $t; }
/// $o = new C;
/// $o->fresh += 5;
/// var_dump($o->fresh);
/// unset($o->u);
/// $o->u .= "x";
/// var_dump($o->u);
/// try { $o->t += 1; } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// var_dump($o);
/// ```
/// ```expect
///
/// Deprecated: Creation of dynamic property C::$fresh is deprecated in %s on line 4
///
/// Warning: Undefined property: C::$fresh in %s on line 4
/// int(5)
///
/// Warning: Undefined property: C::$u in %s on line 7
/// string(1) "x"
/// Error: Typed property C::$t must not be accessed before initialization
/// object(C)#1 (3) {
///   ["p"]=>
///   int(1)
///   ["u"]=>
///   string(1) "x"
///   ["t"]=>
///   uninitialized(int)
///   ["fresh"]=>
///   int(5)
/// }
/// ```
///
/// A class that allows the dynamic property raises the read's warning
/// alone, the creation having nothing to say:
///
/// ```php
/// #[\AllowDynamicProperties]
/// class D { }
/// $d = new D;
/// $d->fresh += 5;
/// var_dump($d->fresh);
/// $s = new stdClass;
/// $s->n .= "a";
/// var_dump($s->n);
/// ```
/// ```expect
///
/// Warning: Undefined property: D::$fresh in %s on line 5
/// int(5)
///
/// Warning: Undefined property: stdClass::$n in %s on line 8
/// string(1) "a"
/// ```
fn dynamic_property_step(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    step: Compound<'_>,
    object: PhpObject,
    name: crate::api::values::PhpString,
) -> Result<(), Thrown> {
    req.addref(&Value::string(name));
    let place = req.dynamic_property_ref(object, name, PropertyRead::Warning);
    if let Err(thrown) = place {
        let Compound { op, data, .. } = step;
        exec::free_operand(req, frame, code, data.op1_type, data.op1);
        write_out(req, frame, code, op, &Value::null());
        return Err(thrown);
    }
    overloaded_property_step(req, frame, code, step, object, name)
}

/// The kernel of a compound assignment over what a declared slot holds
/// and `value`: `.=` grows the string in the slot where it stands
/// ([`crate::ops::concat_assign`]), over a typed slot only where it holds
/// a string, the first arm of `zend_binary_assign_op_typed_prop`; every
/// other operator and value is the step's kernel, whose value the caller
/// writes back.
fn slot_step(
    req: &mut Request,
    step: Compound<'_>,
    object: PhpObject,
    (slot, typed): (u32, bool),
    value: &Value,
) -> Result<Value, Thrown> {
    let raw = exec::value_of(object.heap().slot(slot));
    let held = raw.deref().undef_as_null();
    if grows_in_place(req, step.op.extended_value, &raw)
        && (!typed || held.as_php_string().is_some())
    {
        return crate::ops::concat_assign(req, &held, value, |req| {
            let at = object.heap().slot_ref(slot);
            vacate(req, at);
        });
    }
    (step.kernel)(req, &held, value)
}

/// `zend_assign_op_overloaded_property` (`Z/Zend/zend_execute.c:2429`),
/// the arm a class that hands out no place takes: `read_property` under
/// `BP_VAR_R`, the kernel over what it answered and the value, and
/// `write_property` with what it made, which is what the temporary takes
/// under `RETURN_VALUE_USED`. A class serving the name with `__get` and
/// `__set` is read once and written once, and nothing lands on the
/// object but what the setter puts there.
///
/// ```php
/// class O {
///     public function __get($k) { echo "get $k\n"; return 4; }
///     public function __set($k, $v) { echo "set $k=$v\n"; }
/// }
/// $o = new O;
/// $o->p |= 3;
/// var_dump($o->p);
/// var_dump($o);
/// ```
/// ```expect
/// get p
/// set p=7
/// get p
/// int(4)
/// object(O)#1 (0) {
/// }
/// ```
fn overloaded_property_step(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    step: Compound<'_>,
    object: PhpObject,
    name: crate::api::values::PhpString,
) -> Result<(), Thrown> {
    let Compound { op, data, kernel } = step;
    let value = exec::operand(req, frame, code, data.op1_type, data.op1);
    let table = req.object_handlers(object.heap());
    let held = match table.read_property(req, object, name.as_bytes(), PropertyRead::Warning) {
        Ok(held) => held,
        Err(thrown) => {
            exec::free_operand(req, frame, code, data.op1_type, data.op1);
            write_out(req, frame, code, op, &Value::null());
            return Err(thrown);
        }
    };
    let computed = kernel(req, &held, &value);
    req.release(held);
    exec::free_operand(req, frame, code, data.op1_type, data.op1);
    let stepped = match computed {
        Ok(stepped) => stepped,
        Err(thrown) => {
            write_out(req, frame, code, op, &Value::null());
            return Err(thrown);
        }
    };
    write_out(req, frame, code, op, &stepped);
    req.addref(&stepped);
    let table = req.object_handlers(object.heap());
    let written = table.write_property(req, object, name, stepped);
    if written.is_err() {
        req.release(stepped);
    }
    written
}

/// `ZEND_PRE_INC_OBJ` (`Z/Zend/zend_vm_def.h:1288`), `++$o->p`: the
/// receiver read in place as `GET_OP1_OBJ_ZVAL_PTR_PTR_UNDEF(BP_VAR_RW)`
/// reads it, a reference followed; what is no object is php's `Error`
/// `Attempt to increment/decrement property "p" on null`
/// (`zend_throw_non_object_error`), an undefined CV having warned. On
/// an object a declared slot the scope may reach is stepped in place
/// (`zend_pre_incdec_property_zval`), an `int` slot at its edge refused
/// as `zend_throw_incdec_prop_error` refuses it and a typed slot held to
/// its type after the step (`zend_incdec_typed_prop`), what it held kept
/// where the type refuses; a name the class did not declare is the
/// dynamic entry laid down with the read's warning, and one the class
/// serves through `__get` is `read_property` and `write_property`
/// (`zend_pre_incdec_overloaded_property`). The value after the step is
/// the temporary's under `RETURN_VALUE_USED`.
///
/// ```php
/// class C { public $p = 1; public $s = "a"; }
/// $o = new C;
/// ++$o->p;
/// var_dump(++$o->p, $o->p, ++$o->s);
/// $a = [new C];
/// ++$a[0]->p;
/// var_dump($a[0]->p);
/// ```
/// ```expect
///
/// Deprecated: Increment on non-numeric string is deprecated, use str_increment() instead in %s on line 5
/// int(3)
/// int(3)
/// string(1) "b"
/// int(2)
/// ```
///
/// ```php
/// $n = null;
/// try { ++$n->p; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $u->p--; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Attempt to increment/decrement property "p" on null
///
/// Warning: Undefined variable $u in %s on line 4
/// Attempt to increment/decrement property "p" on null
/// ```
pub(crate) fn pre_inc_obj<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    incdec_obj(req, frame, code, op, IncDec::PRE_INC)
}

/// `ZEND_PRE_DEC_OBJ`, `--$o->p`: the same over
/// [`crate::ops::decrement`], an `int` slot at `PHP_INT_MIN` refused
/// the other way.
///
/// ```php
/// class C { public int $n = PHP_INT_MIN; public int|float $f = PHP_INT_MIN; public $u; }
/// $o = new C;
/// try { --$o->n; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($o->n, --$o->f, --$o->u);
/// ```
/// ```expect
/// Cannot decrement property C::$n of type int past its minimal value
///
/// Warning: Decrement on type null has no effect, this will change in the next major version of PHP in %s on line 5
/// int(-9223372036854775808)
/// float(-9.223372036854776E+18)
/// NULL
/// ```
pub(crate) fn pre_dec_obj<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    incdec_obj(req, frame, code, op, IncDec::PRE_DEC)
}

/// `ZEND_POST_INC_OBJ`, `$o->p++` where the value is read: the same
/// step, with the value the property held before it in the temporary,
/// dereferenced (`zend_post_incdec_property_zval`). A typed slot the
/// step would take out of its type keeps what it held, and a `?int`
/// holding null is stepped to `1`.
///
/// ```php
/// class C { public ?int $n = null; public int $m = PHP_INT_MAX; public $p = 5; }
/// $o = new C;
/// var_dump($o->n++, $o->n, $o->p++, $o->p);
/// try { $o->m++; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($o->m);
/// ```
/// ```expect
/// NULL
/// int(1)
/// int(5)
/// int(6)
/// Cannot increment property C::$m of type int past its maximal value
/// int(9223372036854775807)
/// ```
///
/// A class serving the name through `__get` and `__set` is read once
/// and written once with the stepped value; a name nothing declared is
/// the dynamic entry, created with php's deprecation after the read's
/// warning.
///
/// ```php
/// class O {
///     public function __get($k) { echo "get $k\n"; return 4; }
///     public function __set($k, $v) { echo "set $k=$v\n"; }
/// }
/// $o = new O;
/// var_dump($o->p++);
/// class D { }
/// $d = new D;
/// $d->q++;
/// var_dump($d->q);
/// ```
/// ```expect
/// get p
/// set p=5
/// int(4)
///
/// Deprecated: Creation of dynamic property D::$q is deprecated in %s on line 10
///
/// Warning: Undefined property: D::$q in %s on line 10
/// int(1)
/// ```
pub(crate) fn post_inc_obj<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    incdec_obj(req, frame, code, op, IncDec::POST_INC)
}

/// `ZEND_POST_DEC_OBJ`, `$o->p--` where the value is read: the same
/// step over [`crate::ops::decrement`], the value before it in the
/// temporary.
///
/// ```php
/// class C { public $p = "9"; }
/// $o = new C;
/// var_dump($o->p--, $o->p);
/// ```
/// ```expect
/// string(1) "9"
/// int(8)
/// ```
pub(crate) fn post_dec_obj<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    incdec_obj(req, frame, code, op, IncDec::POST_DEC)
}

/// Which of the four steps a `*_INC_OBJ` or `*_DEC_OBJ` opline is:
/// whether the temporary takes the value before the step, and whether
/// the step is `increment_function`'s, php's `ZEND_IS_INCREMENT`.
#[derive(Clone, Copy)]
struct IncDec {
    post: bool,
    increment: bool,
}

impl IncDec {
    const PRE_INC: IncDec = IncDec {
        post: false,
        increment: true,
    };
    const PRE_DEC: IncDec = IncDec {
        post: false,
        increment: false,
    };
    const POST_INC: IncDec = IncDec {
        post: true,
        increment: true,
    };
    const POST_DEC: IncDec = IncDec {
        post: true,
        increment: false,
    };

    fn kernel(self) -> fn(&mut Request, &Value) -> Result<Value, Thrown> {
        if self.increment {
            crate::ops::increment
        } else {
            crate::ops::decrement
        }
    }
}

/// The body the four `*_INC_OBJ`/`*_DEC_OBJ` handlers share, php's one
/// handler per step over `zend_pre_incdec_property_zval`,
/// `zend_post_incdec_property_zval` and their overloaded twins: the
/// receiver, the name in `op2`, and the step over whichever place the
/// object hands out; `FREE_OP1` after it.
fn incdec_obj(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    step: IncDec,
) -> Edge {
    let name = match op.op2_type {
        OperandKind::Const => exec::constant(code, ConstIdx(op.op2)),
        _ => {
            return Err(Fatal::Error(String::from(
                "InternalError: an increment of a property named at run time",
            )));
        }
    };
    let container = match op.op1_type {
        OperandKind::Cv => exec::operand(req, frame, code, op.op1_type, op.op1),
        _ => exec::container(req, frame, code, op.op1_type, op.op1, "ZEND_PRE_INC_OBJ")?,
    }
    .deref();
    let stepped = if let Some(object) = container.as_object().copied() {
        incdec_property(req, frame, code, op, step, object, &name)?
    } else {
        let shown_name = String::from_utf8_lossy(name.as_str_bytes().unwrap_or_default());
        let shown = crate::ops::value_name(req, &container.undef_as_null());
        let thrown = crate::api::raising::throw(
            req,
            "Error",
            &format!("Attempt to increment/decrement property \"{shown_name}\" on {shown}"),
        );
        write_out(req, frame, code, op, &Value::null());
        Err(thrown)
    };
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    match stepped {
        Ok(()) => Ok(advance(req, frame)),
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// The step over an object's property, `get_property_ptr_ptr` under
/// `BP_VAR_RW` and what php does with the place it answers: a declared
/// slot stepped in place and held to its declared type, the dynamic
/// entry laid down, or the overloaded pair where the class hands out no
/// place.
fn incdec_property(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    step: IncDec,
    object: PhpObject,
    name: &Value,
) -> Result<Result<(), Thrown>, Fatal> {
    let bytes = name.as_str_bytes().unwrap_or_default().to_vec();
    let Some(key) = name.as_php_string().copied() else {
        return Err(Fatal::Error(String::from(
            "InternalError: an increment of a property whose name is no string",
        )));
    };
    let silent = req.serves_accessor(object.class(), b"__get");
    let placed = match req.declared_slot_for_write(object.class(), &bytes, "modify", silent) {
        Ok(placed) => placed,
        Err(thrown) => {
            write_out(req, frame, code, op, &Value::null());
            return Ok(Err(thrown));
        }
    };
    let Some((slot, typed)) = placed else {
        if req.overloaded_place(object, &bytes) {
            return Ok(overloaded_incdec(req, frame, code, op, step, object, key));
        }
        req.addref(&Value::string(key));
        if let Err(thrown) = req.dynamic_property_ref(object, key, PropertyRead::Warning) {
            write_out(req, frame, code, op, &Value::null());
            return Ok(Err(thrown));
        }
        return Ok(overloaded_incdec(req, frame, code, op, step, object, key));
    };
    if object.heap().slot(slot).is_undef() {
        let shown = String::from_utf8_lossy(&bytes).into_owned();
        if typed {
            let class = req.class_name(object.class());
            let thrown = crate::api::raising::throw(
                req,
                "Error",
                &format!(
                    "Typed property {class}::${shown} must not be accessed before initialization"
                ),
            );
            write_out(req, frame, code, op, &Value::null());
            return Ok(Err(thrown));
        }
        if req.overloaded_place(object, &bytes) {
            return Ok(overloaded_incdec(req, frame, code, op, step, object, key));
        }
        let class = req.class_name(object.class());
        crate::api::raising::warning(req, &format!("Undefined property: {class}::${shown}"));
    }
    let held = exec::value_of(object.heap().slot(slot))
        .deref()
        .undef_as_null();
    req.addref(&held);
    let stepped = match step.kernel()(req, &held) {
        Ok(stepped) => stepped,
        Err(thrown) => {
            req.release(held);
            write_out(req, frame, code, op, &Value::null());
            return Ok(Err(thrown));
        }
    };
    let landed = if typed {
        match typed_step(req, frame, object, slot, &bytes, step, (held, stepped))? {
            Ok(landed) => landed,
            Err(thrown) => {
                req.release(held);
                return Ok(Err(thrown));
            }
        }
    } else {
        stepped
    };
    write_out(
        req,
        frame,
        code,
        op,
        if step.post { &held } else { &landed },
    );
    req.release(held);
    req.assign_declared_slot(object.heap(), slot, landed);
    Ok(Ok(()))
}

/// `zend_incdec_typed_prop` (`Z/Zend/zend_execute.c:2262`) over a
/// declared slot's step: an `int` stepped into a float its type does not
/// accept is `zend_throw_incdec_prop_error`, the slot taking the limit;
/// otherwise the stepped value held to the type, converted where the
/// weak rule can, and refused with the slot keeping what it held where
/// it cannot. The stepped value is given up on a refusal; what stood
/// in the slot is the caller's to release.
fn typed_step(
    req: &mut Request,
    frame: FrameRef,
    object: PhpObject,
    slot: u32,
    bytes: &[u8],
    step: IncDec,
    (held, stepped): (Value, Value),
) -> Result<Result<Value, Thrown>, Fatal> {
    let Some(declared) = req.declared_typed_property(object.class(), bytes) else {
        req.release(stepped);
        let shown = String::from_utf8_lossy(bytes);
        return Err(Fatal::Error(format!(
            "InternalError: an increment placed the typed property ${shown}, whose declared type the class does not carry"
        )));
    };
    if crate::ops::steps_past_int(&held, &stepped, &declared.ty) {
        // `zend_throw_incdec_prop_error`: the slot takes the limit and
        // the `TypeError` stands beside it.
        let (thrown, limit) = crate::ops::incdec_prop_error(
            req,
            &declared.ty,
            declared.declaring,
            bytes,
            step.increment,
        );
        req.release(stepped);
        req.assign_declared_slot(object.heap(), slot, limit);
        return Ok(Err(thrown));
    }
    let strict = req.frame_strict_types(frame);
    Ok(
        match crate::ops::verify_property_type(
            req,
            &stepped,
            &declared.ty,
            declared.declaring,
            bytes,
            strict,
        ) {
            Ok(None) => Ok(stepped),
            Ok(Some(coerced)) => {
                req.release(stepped);
                Ok(coerced)
            }
            // `ZVAL_COPY_VALUE(var_ptr, copy)`: the slot keeps what it
            // held.
            Err(thrown) => {
                req.release(stepped);
                Err(thrown)
            }
        },
    )
}

/// `zend_pre_incdec_overloaded_property` and
/// `zend_post_incdec_overloaded_property` (`Z/Zend/zend_execute.c:2367`):
/// `read_property` under `BP_VAR_R`, the step over a dereferenced copy
/// of what it answered, the temporary written with the value before or
/// after it, and `write_property` with the stepped value.
fn overloaded_incdec(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    step: IncDec,
    object: PhpObject,
    name: crate::api::values::PhpString,
) -> Result<(), Thrown> {
    let table = req.object_handlers(object.heap());
    let read = table.read_property(req, object, name.as_bytes(), PropertyRead::Warning);
    let held = match read {
        Ok(held) => held,
        Err(thrown) => {
            write_out(req, frame, code, op, &Value::null());
            return Err(thrown);
        }
    };
    let copy = held.deref();
    req.addref(&copy);
    req.release(held);
    let stepped = match step.kernel()(req, &copy) {
        Ok(stepped) => stepped,
        Err(thrown) => {
            req.release(copy);
            write_out(req, frame, code, op, &Value::null());
            return Err(thrown);
        }
    };
    write_out(
        req,
        frame,
        code,
        op,
        if step.post { &copy } else { &stepped },
    );
    req.release(copy);
    // `write_property` takes an owner of the value and of the name,
    // the stepped value's own and one more of the name.
    req.addref(&Value::string(name));
    let table = req.object_handlers(object.heap());
    table.write_property(req, object, name, stepped)
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

/// `ZEND_RETURNS_VALUE` (`Z/Zend/zend_compile.h:1199`), on a
/// `RETURN_BY_REF` whose operand is a `VAR` that is no variable, what
/// `zend_compile_return` puts there for an expression that is neither
/// a call nor a variable.
const RETURNS_VALUE: u32 = 1 << 1;

/// `ZEND_ARRAY_ELEMENT_REF`, `ZEND_ARRAY_NOT_PACKED` and
/// `ZEND_ARRAY_SIZE_SHIFT` (`Z/Zend/zend_compile.h:1201`): the bits of
/// `INIT_ARRAY`'s and `ADD_ARRAY_ELEMENT`'s `extended_value`, the element
/// count above the two flags.
const ARRAY_ELEMENT_REF: u32 = 1 << 0;
const ARRAY_NOT_PACKED: u32 = 1 << 1;
const ARRAY_SIZE_SHIFT: u32 = 2;

/// `ZEND_UNSET_CV` (`Z/Zend/zend_vm_def.h:6627`), what `unset($a)`
/// compiles to: the CV left `IS_UNDEF` and what it held given up after
/// it (`ZVAL_UNDEF(var)` then `GC_DTOR(garbage)`, in that order, so a
/// destructor that reads the variable finds it already unset). The
/// variable is no less declared for it: a read of it warns as a read of
/// one nothing ever wrote warns, and `isset` says `false`.
///
/// ```php
/// $a = 1; unset($a); var_dump(isset($a)); echo $a;
/// ```
/// ```expect
/// bool(false)
///
/// Warning: Undefined variable $a in %s on line 2
/// ```
///
/// The value is given up where the opline stands, which a destructor
/// says: php's order is the CV emptied first and the object released
/// after.
///
/// ```php
/// class C { public $n; function __construct($n) { $this->n = $n; } function __destruct() { echo "gone ", $this->n, "\n"; } }
/// $x = new C(1); $y = $x; echo "before\n"; unset($x); echo "one name left\n"; unset($y); echo "after\n";
/// ```
/// ```expect
/// before
/// one name left
/// gone 1
/// after
/// ```
pub(crate) fn unset_cv<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    _: &OpArray,
    op: &Opline,
) -> Edge {
    let garbage = req.take_slot(frame, op.op1);
    req.release(garbage);
    Ok(advance(req, frame))
}

/// `ZEND_UNSET_DIM` (`Z/Zend/zend_vm_def.h:6735`), what `unset($a[$k])`
/// compiles to: the container read as a write fetch reads it
/// (`GET_OP1_OBJ_ZVAL_PTR_PTR_UNDEF(BP_VAR_UNSET)`), a reference
/// followed, and an array separated where more than one name holds it
/// (`SEPARATE_ARRAY`, which writes the copy back where the container
/// stands) before [`crate::ops::unset_dim`] deletes at the offset.
/// Everything else is the ladder after the `do`: an object through its
/// handler table's `unset_dimension`, `offsetUnset` on an `ArrayAccess`;
/// a string php's `Error` `Cannot unset string offsets`; a `true`, a
/// number or a resource php's `Cannot unset offset in a non-array
/// variable`; a `false` php's deprecation for the array it would have
/// been made into; and `null`, or a variable nothing wrote, nothing at
/// all, with the warning a read of that variable raises.
///
/// ```php
/// $a = [1, 2, 3]; $b = $a; unset($b[1]); var_dump($a, $b);
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   [2]=>
///   int(3)
/// }
/// array(2) {
///   [0]=>
///   int(1)
///   [2]=>
///   int(3)
/// }
/// ```
///
/// A name bound to the array reaches the same table, so the delete is
/// seen through both.
///
/// ```php
/// $a = [1, 2]; $r = &$a; unset($r[0]); var_dump($a);
/// ```
/// ```expect
/// array(1) {
///   [1]=>
///   int(2)
/// }
/// ```
///
/// What is no array is php's `Error`, and what is `null` is nothing.
///
/// ```php
/// $n = null; unset($n[0]); unset($u[0]); var_dump($n);
/// $s = "ab";
/// unset($s[0]);
/// ```
/// ```expect exit=255
/// Warning: Undefined variable $u in %s on line 2
/// NULL
///
/// Fatal error: Uncaught Error: Cannot unset string offsets in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
pub(crate) fn unset_dim<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let offset = exec::operand_undef(req, frame, code, op.op2_type, op.op2);
    let Some(place) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::Error(String::from(
            "InternalError: ZEND_UNSET_DIM over a constant or a temporary, which php compiles no write over",
        )));
    };
    // The reference followed once and the array separated are
    // [`separated_array`], the walk every dimension site shares; the
    // ladder after it is `UNSET_DIM`'s own, which makes no array out of
    // what is none.
    let found = match separated_array(req, place) {
        Ok(found) => found,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let unset = if let (_, Some(array)) = found {
        let constant = op.op2_type == OperandKind::Const;
        crate::ops::unset_dim(req, array, &offset, constant)
    } else {
        // The two reads php raises a warning for stand here, after the
        // array arm and before the rest (`ZVAL_UNDEFINED_OP1`,
        // `ZVAL_UNDEFINED_OP2`).
        let container = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
        // `if (OP2_TYPE == IS_CONST && Z_EXTRA_P(offset) ==
        // ZEND_EXTRA_VALUE) offset++`: an object is handed the dimension
        // as it was written, which is the only arm that reads it at all.
        let offset = if container.as_object().is_some() {
            object_dim(req, frame, code, op).unwrap_or_else(Value::null)
        } else {
            exec::operand(req, frame, code, op.op2_type, op.op2)
        };
        unset_container(req, &container, &offset)
    };
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    if let Err(thrown) = unset {
        return threw(req, frame, &thrown);
    }
    Ok(advance(req, frame))
}

/// `UNSET_DIM`'s ladder for a container that is no array
/// (`Z/Zend/zend_vm_def.h:6812`), which is its own and not the write's:
/// an object through `unset_dimension`, `offsetUnset` on an
/// `ArrayAccess`, handed the dimension as written; a string, a `true`, a
/// number or a resource php's `Error`; a `false` php's deprecation,
/// raised without the array a write would have made; and `null` or a
/// variable nothing wrote nothing at all.
fn unset_container(req: &mut Request, container: &Value, offset: &Value) -> Result<(), Thrown> {
    match container.as_ref() {
        ValueRef::Object(object) => {
            let table = req.object_handlers(object.heap());
            table.unset_dimension(req, *object, offset)
        }
        ValueRef::Str(_) => Err(crate::api::raising::throw(
            req,
            "Error",
            "Cannot unset string offsets",
        )),
        ValueRef::Bool(false) => {
            // `zend_false_to_array_deprecated`, the one this handler
            // raises without making the array.
            crate::api::raising::deprecated(
                req,
                "Automatic conversion of false to array is deprecated",
            );
            if req.is_pending() {
                return Err(Thrown::of(None));
            }
            Ok(())
        }
        ValueRef::Undef | ValueRef::Null => Ok(()),
        _ => Err(crate::api::raising::throw(
            req,
            "Error",
            "Cannot unset offset in a non-array variable",
        )),
    }
}

/// `ZEND_UNSET_OBJ` (`Z/Zend/zend_vm_def.h:6837`), what `unset($o->p)`
/// compiles to: the receiver read as a write fetch reads it, a reference
/// followed, and the object's handler table's `unset_property` over the
/// name `op2` names, `zend_std_unset_property` at `Std`
/// ([`Request::unset_property`]): a declared slot the scope may reach
/// left `IS_UNDEF` and a dynamic entry removed. A receiver that is no
/// object is nothing at all, no warning and no error, which is php's
/// `break` out of the `do`.
///
/// A declared slot emptied is not a property removed: it stands in the
/// class and reads as one nothing wrote, so `foreach` steps over it as
/// php steps over an `IS_UNDEF` bucket.
///
/// ```php
/// class C { public $p = 1; public $q = 2; }
/// $c = new C; unset($c->p); unset($c->nope); unset($u->p);
/// foreach ($c as $k => $v) echo "$k=$v ";
/// ```
/// ```expect
/// q=2
/// ```
///
/// An untyped slot emptied reads as a dynamic name that was never
/// written does, so `var_dump` prints neither it nor a placeholder for
/// it.
///
/// ```php
/// class C { public $p = 1; public $q = 2; }
/// $c = new C; unset($c->p); var_dump($c);
/// ```
/// ```expect
/// object(C)#1 (1) {
///   ["q"]=>
///   int(2)
/// }
/// ```
///
/// A dynamic property is removed from the table, and one a class did not
/// declare is gone from `var_dump` with it.
///
/// ```php
/// $o = new stdClass; $o->a = 1; $o->b = 2; unset($o->a); var_dump($o);
/// ```
/// ```expect
/// object(stdClass)#1 (1) {
///   ["b"]=>
///   int(2)
/// }
/// ```
pub(crate) fn unset_obj<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let name = match op.op2_type {
        OperandKind::Const => exec::constant(code, ConstIdx(op.op2)),
        _ => {
            return Err(Fatal::Error(String::from(
                "InternalError: ZEND_UNSET_OBJ over a property named at run time",
            )));
        }
    };
    let container =
        exec::container(req, frame, code, op.op1_type, op.op1, "ZEND_UNSET_OBJ")?.deref();
    let unset = match container.as_object().copied() {
        Some(object) => {
            let bytes = name.as_str_bytes().unwrap_or_default();
            let table = req.object_handlers(object.heap());
            table.unset_property(req, object, bytes)
        }
        None => Ok(()),
    };
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    if let Err(thrown) = unset {
        return threw(req, frame, &thrown);
    }
    Ok(advance(req, frame))
}

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
/// moves and whose slot is left `UNDEF` ([`take_temporary`]); the
/// position `Z_FE_POS` starts at zero; a `VAR` operand is freed
/// (`FREE_OP1_IF_VAR`) and the next opline is the `FE_FETCH_R`.
/// An operand that is neither array nor object is php's warning, the
/// `VAR` left `UNDEF`, the operand freed (`FREE_OP1`) and the jump
/// `op2` names, to the `FE_FREE` past the loop.
///
/// An object whose class has no `get_iterator` is copied the same way
/// and walks its property table: the table separated where a cast
/// shares it and registered with a hash iterator at its first property
/// ([`Request::object_walk_start`]), whose index is the `VAR`'s
/// `Z_FE_ITER`, so a body that deletes or adds a property leaves the
/// walk where php's leaves it; an object of no property at all jumps
/// past the loop with `Z_FE_ITER` at `-1`. An object whose class has one,
/// php's `Traversable`, is `zend_fe_reset_iterator` instead
/// ([`fe_reset_iterator`]): the `VAR` holds its iterator, and a
/// `foreach` over it is walked through its methods. What the `VAR`
/// holds is read back as one [`ForeachVar`].
///
/// The walk follows the table its body changes: a dynamic property
/// deleted ahead of it is not reached and one added is, in either kind
/// of loop, the declared slots walked first as php's table holds them.
///
/// ```php
/// #[AllowDynamicProperties]
/// class Q { public $a = 1; public $b = 2; }
/// $o = new Q; $o->x = 1; $o->y = 2; $o->z = 3;
/// foreach ($o as $k => &$v) {
///     echo $k;
///     if ($k === 'x') { unset($o->y); $o->w = 4; }
/// }
/// echo "\n";
/// foreach ($o as $k => $v) { echo $k; if ($k === 'a') { $o->n = 5; } }
/// ```
/// ```expect
/// abxzw
/// abxzwn
/// ```
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
/// A loop over a `TMP` in a function body leaves nothing behind for
/// the frame's teardown either: [`Request::pop_frame`] gives up every
/// owner the frame's slots still hold, temporaries with the CVs, so a
/// subject left standing in its `TMP` after the `FE_FREE` gave it up
/// was given up again on the return, and the next blocks of its size
/// were handed out of a broken free list. `Zend/tests/gc_051.phpt`'s
/// early return out of a loop over a cast is one shape, a loop walked to
/// its end over an `INIT_ARRAY` the other (#771).
///
/// ```php
/// function f(string $s): object { foreach ((array) $s as $v) { $o = new stdClass; return $o; } }
/// function g($s) { foreach ([$s] as $v) {} }
/// for ($i = 0; $i < 3; $i++) {
///     $a = new stdClass; $b = new stdClass; $a->ref = $b; $b->ref = $a;
///     $r = f('x'); g('x');
///     $x = []; $x[] = 1; $y = []; $y[] = 1; $z = []; $z[] = 1;
/// }
/// echo "ok\n";
/// ```
/// ```expect
/// ok
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
///
/// The `TMP` a dimension read leaves is the loop's from the reset, so
/// the `FETCH_DIM_R` of the next pass writes its slot over nothing: an
/// array a native built, whose second element is the subject, is read
/// thirty times over with its previous copy gone each time (#723).
///
/// ```php
/// for ($i = 0; $i < 30; $i++) {
///     $r = json_decode('{"w":[],"errors":[]}', true);
///     foreach ($r["errors"] as $e) {}
/// }
/// echo "ok\n";
/// ```
/// ```expect
/// ok
/// ```
///
/// The same holds of every `TMP` subject: an array an `INIT_ARRAY`
/// builds and an object a cast makes are each given up once, by the
/// `FE_FREE`, so the loop over them costs nothing when it is done, and
/// each pass walks its own object.
///
/// ```php
/// $b = memory_get_usage();
/// for ($i = 0; $i < 30; $i++) { foreach ([$i] as $e) {} }
/// var_dump(memory_get_usage() - $b);
/// for ($i = 0; $i < 30; $i++) {
///     foreach ([[$i]] as $e) {}
///     foreach ((object)['a' => $i] as $k => $v) echo $i < 3 ? "$k=$v " : '';
/// }
/// ```
/// ```expect
/// int(0)
/// a=0 a=1 a=2
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
        if op.op1_type == OperandKind::Tmp {
            take_temporary(req, frame, code, op.op1);
        } else {
            req.addref(&subject);
        }
        let previous = req.set_slot(frame, slot, subject);
        req.release(previous);
        if let Some(cell) = req.slot_ptr(frame, slot) {
            // `Z_FE_POS_P(EX_VAR(opline->result.var)) = pos`, which php
            // writes after the copy and not through it: the word is the
            // slot's, not the value's (`ZvalRef::write`, #491).
            cell.set_fe_pos(0);
        }
        if op.op1_type == OperandKind::Var {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
        }
        return Ok(advance(req, frame));
    }
    if let Some(object) = subject.as_object().copied() {
        if crate::interfaces::is_traversable(req, object.class()) {
            return fe_reset_iterator(req, frame, code, op, object, false);
        }
        let started = match req.object_walk_start(object) {
            Ok(started) => started.map(HashIter),
            Err(thrown) => return threw(req, frame, &thrown),
        };
        if op.op1_type == OperandKind::Tmp {
            take_temporary(req, frame, code, op.op1);
        } else {
            req.addref(&subject);
        }
        let previous = req.set_slot(frame, slot, subject);
        req.release(previous);
        set_fe_iter(req, frame, slot, started);
        if op.op1_type == OperandKind::Var {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
        }
        if started.is_none() {
            req.vm_mut().jump(frame, op.op2);
            return Ok(Stepped::Same(frame));
        }
        return Ok(advance(req, frame));
    }
    fe_reset_refused(req, frame, code, op, &subject)
}

/// `zend_fe_reset_iterator` (`Z/Zend/zend_execute.c:5368`), the arm both
/// resets take for an object whose class has a `get_iterator`, php's
/// `Traversable`: the iterator [`crate::interfaces::reset`] makes takes
/// the `VAR`, the operand is freed after it (`FREE_OP1`), and an
/// iterator whose first `valid()` answered `false` jumps past the loop.
/// A throw, the refusal of a loop by reference among them, leaves the
/// `VAR` `UNDEF`.
///
/// A generator is walked the same way, through its own table of
/// iterator functions: the reset runs its body to the first `yield`,
/// and a body that returns before one is an empty loop.
///
/// ```php
/// function g() { echo "[start]"; yield 'a' => 1; yield 'b' => 2; }
/// foreach (g() as $k => $v) echo "$k=$v ";
/// function e() { echo "[empty]"; return; yield; }
/// foreach (e() as $v) echo "no";
/// echo "\n";
/// ```
/// ```expect
/// [start]a=1 b=2 [empty]
/// ```
fn fe_reset_iterator(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    object: PhpObject,
    by_ref: bool,
) -> Edge {
    let reset = crate::interfaces::reset(req, object, by_ref);
    let held = match &reset {
        Ok((wrapper, _)) => wrapper.as_value(),
        Err(_) => Value::undef(),
    };
    let previous = req.set_slot(frame, exec::temporary_slot(code, op.out), held);
    req.release(previous);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    match reset {
        Ok((_, false)) => Ok(advance(req, frame)),
        Ok((_, true)) => {
            req.vm_mut().jump(frame, op.op2);
            Ok(Stepped::Same(frame))
        }
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// php's `ZVAL_COPY_VALUE` out of a `TMP` whose owner the reset's
/// `VAR` takes: php leaves the dead temporary's bits standing, since
/// nothing reads them again, but every write of a slot here gives up
/// what the slot held, so the next opline to write this one would give
/// the subject up a second time after the `FE_FREE` did. The slot is
/// emptied instead, as [`exec::take_operand`] empties a `TMP` it moves.
fn take_temporary(req: &mut Request, frame: FrameRef, code: &OpArray, field: u32) {
    // The value is the `VAR`'s now, so what the take hands back is not
    // released here.
    let _ = req.take_slot(frame, exec::temporary_slot(code, field));
}

/// The arm both resets share for what is neither array nor object
/// (`Z/Zend/zend_vm_def.h:6950`): php's warning naming the value as
/// `zend_zval_value_name` names it, the `VAR` left `UNDEF` with
/// `Z_FE_ITER` at `-1` so the `FE_FREE` deletes nothing, the operand
/// freed and the jump `op2` names.
fn fe_reset_refused(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    subject: &Value,
) -> Edge {
    crate::api::raising::warning(
        req,
        &format!(
            "foreach() argument must be of type array|object, {} given",
            crate::ops::value_name(req, subject)
        ),
    );
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, Value::undef());
    req.release(previous);
    set_fe_iter(req, frame, slot, None);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    req.vm_mut().jump(frame, op.op2);
    Ok(Stepped::Same(frame))
}

/// `Z_FE_ITER` (`Z/Zend/zend_types.h:424`): the index a walk over a
/// property table, or by reference over an array, is registered at in
/// the heap's table of hash iterators (`zend_hash_iterator_add`), so
/// the walk follows what the body adds, deletes and rehashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HashIter(u32);

/// `(uint32_t)-1` in the loop variable's `u2` word: no hash iterator,
/// the `Z_FE_ITER` of a reset that registered none. Only
/// [`set_fe_iter`] writes it and only [`foreach_var`] reads it.
const NO_WALK: u32 = u32::MAX;

/// What a `foreach`'s loop variable holds, the `VAR` a reset filled,
/// decoded once from the value and its `u2` word: every `FE_FETCH_*`,
/// `FE_FREE` and the unwind of a `ZEND_LIVE_LOOP` range match on it and
/// on nothing else. The kind is the held value's type, as php's handlers
/// read it: an array is a by-value walk and the word its `Z_FE_POS`; a
/// reference is a by-reference walk and the word its `Z_FE_ITER`; an
/// object is an iterator where it is one's wrapper
/// (`zend_iterator_unwrap`, `Z/Zend/zend_vm_def.h:7073`), which
/// `zend_fe_reset_iterator` put there for a class with a
/// `get_iterator`, and otherwise the object itself, walked by its
/// properties from its `Z_FE_ITER`. The word never tells one kind from
/// another, and an iterator's progress is its own
/// ([`crate::interfaces::ObjectIterator`]), not the word's.
///
/// The five kinds side by side: an object of no property and an empty
/// `ArrayObject` serve no body, the one past its properties and the
/// other past its iterator's first `valid()`; an `IteratorAggregate` and
/// an `Iterator` are walked through their methods, the iterator holding
/// the object number php's `zend_iterator_init` takes, so the object the
/// body makes is `#3`; a plain object by reference writes its
/// properties.
///
/// ```php
/// class It implements Iterator {
///     private $i = 0;
///     function current(): mixed { return $this->i * 10; }
///     function key(): mixed { return "k$this->i"; }
///     function next(): void { $this->i++; }
///     function rewind(): void { $this->i = 0; }
///     function valid(): bool { return $this->i < 2; }
/// }
/// class Agg implements IteratorAggregate {
///     function getIterator(): Iterator { return new ArrayIterator(['x' => 1, 'y' => 2]); }
/// }
/// foreach (new stdClass as $v) echo "never\n";
/// foreach (new ArrayObject([]) as $v) echo "never\n";
/// foreach (new Agg as $k => $v) echo "$k=$v;";
/// echo "\n";
/// $it = new It;
/// foreach ($it as $k => $v) { echo "$k=$v;"; var_dump(new stdClass); }
/// $o = new stdClass; $o->a = 1; $o->b = 2;
/// foreach ($o as $k => &$v) { $v = "$k$v"; }
/// unset($v);
/// var_dump($o);
/// ```
/// ```expect
/// x=1;y=2;
/// k0=0;object(stdClass)#3 (0) {
/// }
/// k1=10;object(stdClass)#3 (0) {
/// }
/// object(stdClass)#2 (2) {
///   ["a"]=>
///   string(2) "a1"
///   ["b"]=>
///   string(2) "b2"
/// }
/// ```
///
/// A `Traversable` by reference is `zend_user_it_get_iterator`'s
/// refusal, after an `IteratorAggregate` answered its `getIterator()`.
///
/// ```php
/// class It implements Iterator {
///     function current(): mixed { return 1; }
///     function key(): mixed { return 0; }
///     function next(): void {}
///     function rewind(): void { echo "rewind\n"; }
///     function valid(): bool { return true; }
/// }
/// class Agg implements IteratorAggregate {
///     function getIterator(): Iterator { echo "get\n"; return new It; }
/// }
/// try { foreach (new Agg as &$v) {} } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $it = new It;
/// foreach ($it as &$v) {}
/// ```
/// ```expect exit=255
/// get
/// An iterator cannot be used with foreach by reference
///
/// Fatal error: Uncaught Error: An iterator cannot be used with foreach by reference in %s:14
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 14
/// ```
#[derive(Clone, Copy)]
pub(crate) enum ForeachVar {
    /// `FE_RESET_R` over an array: the copy the loop reads, walked from
    /// `Z_FE_POS`.
    Array { array: php_heap::Arr, pos: u32 },
    /// An object walked by its property table: by value, or by
    /// reference over a temporary, which `FE_RESET_RW` holds as it
    /// stands.
    Properties { object: PhpObject, walk: HashIter },
    /// `FE_RESET_RW` over a variable: the reference its array or object
    /// was made into, walked over whatever the body leaves in it.
    Reference { cell: PhpRef, walk: HashIter },
    /// `zend_fe_reset_iterator`'s iterator, the wrapper object that owns
    /// the [`crate::interfaces::ObjectIterator`].
    Iterator(PhpObject),
    /// A reset that jumped past its loop and registered no walk: the
    /// refused operand's `UNDEF`, or an object of no property with
    /// `Z_FE_ITER` at `-1`. No fetch is reached over one.
    Skipped,
}

impl ForeachVar {
    /// `if (Z_TYPE_P(var) != IS_ARRAY && Z_FE_ITER_P(var) != (uint32_t)-1)
    /// zend_hash_iterator_del(Z_FE_ITER_P(var))` over the loop variable
    /// `held`: what `FE_FREE` and the unwind of a `ZEND_LIVE_LOOP` range
    /// do before the release, the unwind of a frame on the stack
    /// ([`exec::cleanup_live_vars`]) and of a generator's frame off it
    /// alike.
    pub(crate) fn iterator_del(req: &mut Request, held: php_heap::Zval) {
        match foreach_var(req, held) {
            ForeachVar::Properties { walk, .. } | ForeachVar::Reference { walk, .. } => {
                req.heap_mut().iterator_del(walk.0);
            }
            ForeachVar::Array { .. } | ForeachVar::Iterator(_) | ForeachVar::Skipped => {}
        }
    }
}

/// The one decoding of a loop variable ([`ForeachVar`]).
fn foreach_var(req: &Request, held: php_heap::Zval) -> ForeachVar {
    if let Some(array) = held.as_arr() {
        return ForeachVar::Array {
            array,
            pos: held.fe_pos(),
        };
    }
    let walk = (held.fe_pos() != NO_WALK).then_some(HashIter(held.fe_pos()));
    let value = Value::of_zval(held);
    if let Some(cell) = value.as_php_ref().copied() {
        return walk.map_or(ForeachVar::Skipped, |walk| ForeachVar::Reference {
            cell,
            walk,
        });
    }
    if let Some(object) = value.as_object().copied() {
        if crate::interfaces::is_iterator_wrapper(req, object) {
            return ForeachVar::Iterator(object);
        }
        return walk.map_or(ForeachVar::Skipped, |walk| ForeachVar::Properties {
            object,
            walk,
        });
    }
    ForeachVar::Skipped
}

/// `Z_FE_ITER_P(EX_VAR(opline->result.var)) = idx`, [`NO_WALK`] for
/// none: the loop variable's `u2` word, written after the value as php
/// writes it, since a value write leaves `u2` where it stands
/// (`ZvalRef::write`, #491).
fn set_fe_iter(req: &mut Request, frame: FrameRef, slot: u32, walk: Option<HashIter>) {
    if let Some(cell) = req.slot_ptr(frame, slot) {
        cell.set_fe_pos(walk.map_or(NO_WALK, |walk| walk.0));
    }
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
/// (`Z/Zend/zend_vm_def.h:7058`) instead: the next property the
/// executing scope may see from where the loop's hash iterator stands
/// ([`Request::object_walk`]), its unmangled name as the key, and the
/// same jump where nothing is left. The value is the property's with a
/// reference followed, one more owner, as an array element's is. A
/// `VAR` holding an iterator's wrapper is that function's iterator arm
/// ([`fe_fetch_iterator`]). Which of the three it is, is the one
/// decoding [`ForeachVar`] makes.
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
    let (key, value) = match foreach_var(req, at.read()) {
        ForeachVar::Array { array, pos } => {
            let Some((found, key, value)) = array.live_from(pos) else {
                req.vm_mut().jump(frame, op.extended_value);
                return Ok(Stepped::Same(frame));
            };
            // `Z_FE_POS_P(array) = pos + 1`: the word alone, since a
            // value write is `ZVAL_COPY_VALUE` and leaves `u2` standing
            // (#491).
            at.set_fe_pos(found.saturating_add(1));
            (key, exec::value_of(value))
        }
        ForeachVar::Properties { object, walk } => match req.object_walk(object, walk.0) {
            Ok(Some((key, walked))) => (key, Request::walked_value(object, walked)),
            Ok(None) => {
                req.vm_mut().jump(frame, op.extended_value);
                return Ok(Stepped::Same(frame));
            }
            Err(thrown) => return threw(req, frame, &thrown),
        },
        ForeachVar::Iterator(iterator) => return fe_fetch_iterator(req, frame, code, op, iterator),
        ForeachVar::Reference { .. } | ForeachVar::Skipped => {
            return Err(Fatal::Error(String::from(
                "InternalError: FE_FETCH_R over a VAR no by-value reset filled",
            )));
        }
    };
    if op.out_type != OperandKind::Unused {
        let key = match fe_key(req, key) {
            Ok(key) => key,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        let out = exec::temporary_slot(code, op.out);
        let previous = req.set_slot(frame, out, key);
        req.release(previous);
    }
    fe_assign(req, frame, code, op, value)
}

/// The iterator arm of `zend_fe_fetch_object_helper`
/// (`Z/Zend/zend_vm_def.h:7117`): the next element
/// [`crate::interfaces::fetch`] finds, its key to `result` where the
/// opline keeps one and its value to the variable as an array element's
/// goes, or the jump to `extended_value` at the end. A generator's
/// `move_forward` resumes it, so the value its `yield` receives is the
/// `null` a resumption without `send()` hands it.
///
/// ```php
/// function g() { $x = yield 1; echo "[$x]"; yield 2; }
/// foreach (g() as $v) echo $v;
/// echo "\n";
/// ```
/// ```expect
/// 1[]2
/// ```
fn fe_fetch_iterator(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    iterator: PhpObject,
) -> Edge {
    let keyed = op.out_type != OperandKind::Unused;
    let (current, key) = match crate::interfaces::fetch(req, iterator, keyed, false) {
        Ok(Some(found)) => found,
        Ok(None) => {
            req.vm_mut().jump(frame, op.extended_value);
            return Ok(Stepped::Same(frame));
        }
        Err(thrown) => return threw(req, frame, &thrown),
    };
    if let Some(key) = key {
        let out = exec::temporary_slot(code, op.out);
        let previous = req.set_slot(frame, out, key);
        req.release(previous);
    }
    let assigned = fe_assign(req, frame, code, op, current);
    req.release(current);
    assigned
}

/// The key a fetch writes to its `result` (`RETURN_VALUE_USED`): an
/// integer as it stands, and a string with one more owner
/// (`ZVAL_STR_COPY`).
fn fe_key(req: &mut Request, key: php_heap::Key) -> Result<Value, Thrown> {
    Ok(match key {
        php_heap::Key::Long(n) => Value::int(n),
        php_heap::Key::Str(s) => {
            let s = Value::string(PhpString::of(s));
            req.addref(&s);
            s
        }
    })
}

/// The value a by-value fetch hands its CV, `zend_assign_to_variable`
/// (`Z/Zend/zend_execute.c`): one more owner with a reference followed,
/// written through the cell the CV holds or over what it held, which is
/// released after the write. A `VAR`, what the compiler gives a fetch
/// whose target is a list, a property or an element, takes the value as
/// it stands with one more owner (`ZVAL_COPY_VALUE` and `GC_ADDREF`),
/// for the assignment after it to read.
///
/// ```php
/// $xs = [[1, 2], [3, 4]];
/// foreach ($xs as [$a, $b]) { echo $a, $b, "\n"; }
/// foreach ($xs as $k => [$a, $b]) { echo $k, $a, $b, "\n"; }
/// $o = new stdClass;
/// foreach ($xs as $o->p) {}
/// var_dump($o->p);
/// ```
/// ```expect
/// 12
/// 34
/// 012
/// 134
/// array(2) {
///   [0]=>
///   int(3)
///   [1]=>
///   int(4)
/// }
/// ```
fn fe_assign(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    value: Value,
) -> Edge {
    if op.op2_type == OperandKind::Var {
        req.addref(&value);
        let out = exec::temporary_slot(code, op.op2);
        let previous = req.set_slot(frame, out, value);
        req.release(previous);
        return Ok(advance(req, frame));
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

/// `ZEND_FE_RESET_RW` (`Z/Zend/zend_vm_def.h:6958`), the first opline
/// of a `foreach` by reference: the subject read in place
/// (`GET_OP1_ZVAL_PTR_PTR`), a CV or the place a `_W` fetch's `VAR`
/// names. An array there is made a reference where it is not one
/// (`ZVAL_NEW_REF(array_ref, array_ref)`), the `VAR` the opline's
/// `result` names holds one more owner of that cell, the array inside
/// it is separated (`SEPARATE_ARRAY`) so the loop writes the variable's
/// own copy, and a hash iterator is registered at its first position
/// (`zend_hash_iterator_add`), its index the `VAR`'s `Z_FE_ITER`. A
/// temporary is wrapped in a cell of its own and a constant array is
/// copied first. An object is the same cell, or the object itself from
/// a temporary, over the walk [`Request::object_walk_start`] registers.
/// What is neither is php's warning and the jump past the loop, as for
/// the by-value reset.
///
/// The loop writes through the variable, and an element the body
/// appends is walked, because the iterator's position is the table's
/// and not a count.
///
/// ```php
/// $a = [1, 2, 3];
/// foreach ($a as &$v) { $v *= 2; if ($v == 2) { $a[] = 4; } }
/// var_dump($a);
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   int(2)
///   [1]=>
///   int(4)
///   [2]=>
///   int(6)
///   [3]=>
///   &int(8)
/// }
/// ```
///
/// A copy the loop shares its array with keeps its own values: the
/// separation is the variable's, before the first write.
///
/// ```php
/// $a = ['x' => 1, 'y' => 2]; $b = $a;
/// foreach ($a as $k => &$v) { $v = "$k$v"; }
/// unset($v);
/// var_dump($a, $b);
/// ```
/// ```expect
/// array(2) {
///   ["x"]=>
///   string(2) "x1"
///   ["y"]=>
///   string(2) "y2"
/// }
/// array(2) {
///   ["x"]=>
///   int(1)
///   ["y"]=>
///   int(2)
/// }
/// ```
///
/// An element the body deletes ahead of the walk is not reached, and a
/// rehash the body's appends cause moves the walk with the element it
/// stood at.
///
/// ```php
/// $a = ['a' => 1, 'b' => 2, 'c' => 3, 'd' => 4, 'e' => 5, 'f' => 6, 'g' => 7, 'h' => 8];
/// foreach ($a as $k => &$v) {
///     echo $k;
///     if ($k === 'b') { unset($a['a'], $a['c']); $a['i'] = 9; }
/// }
/// unset($v);
/// echo "\n";
/// foreach ($a as $k => $v) echo $k;
/// ```
/// ```expect
/// abdefghi
/// bdefghi
/// ```
///
/// A dimension is fetched for writing, so the loop binds the elements
/// of the nested array in place, and a constant array is walked through
/// a copy of its own.
///
/// ```php
/// $m = ['r' => [1, 2]];
/// foreach ($m['r'] as &$v) { $v += 10; }
/// unset($v);
/// foreach ([7, 8] as $k => &$w) { $w = $k; }
/// var_dump($m, $w);
/// ```
/// ```expect
/// array(1) {
///   ["r"]=>
///   array(2) {
///     [0]=>
///     int(11)
///     [1]=>
///     int(12)
///   }
/// }
/// int(1)
/// ```
///
/// ```php
/// $n = 5;
/// foreach ($n as &$v) {}
/// echo "on\n";
/// ```
/// ```expect
/// Warning: foreach() argument must be of type array|object, int given in %s on line 3
/// on
/// ```
pub(crate) fn fe_reset_rw<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let slot = exec::temporary_slot(code, op.out);
    // `GET_OP1_ZVAL_PTR_PTR(BP_VAR_R)`: the place a CV or a `VAR` names,
    // a `VAR`'s `IS_INDIRECT` followed; a temporary or a constant is a
    // value and names no place.
    let place = match op.op1_type {
        OperandKind::Cv => {
            if req.slot(frame, op.op1).tag() == TypeTag::Undef {
                // `_get_zval_ptr_cv_BP_VAR_R`: the undefined-variable
                // warning, and `null`, which is the refusal below.
                let _ = exec::operand(req, frame, code, op.op1_type, op.op1);
            }
            req.slot_ptr(frame, op.op1)
        }
        OperandKind::Var => {
            let at = exec::temporary_slot(code, op.op1);
            req.slot_ptr(frame, at)
                .map(|cell| cell.read().as_indirect().unwrap_or(cell))
        }
        _ => None,
    };
    let Some(place) = place else {
        return fe_reset_rw_value(req, frame, code, op, slot);
    };
    let subject = exec::value_of(place.read()).deref();
    let is_array = subject.as_array().is_some();
    if !is_array && subject.as_object().is_none() {
        return fe_reset_refused(req, frame, code, op, &subject.undef_as_null());
    }
    if let Some(object) = subject.as_object().copied()
        && crate::interfaces::is_traversable(req, object.class())
    {
        return fe_reset_iterator(req, frame, code, op, object, true);
    }
    // `if (array_ptr == array_ref) ZVAL_NEW_REF(array_ref, array_ref)`:
    // the place's owner is the cell's from here.
    let held = exec::value_of(place.read());
    let cell = if let Some(cell) = held.as_php_ref() {
        *cell
    } else {
        let cell = match req.reference(held) {
            Ok(cell) => cell,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        place.write(cell.as_value().to_zval());
        cell
    };
    req.addref(&cell.as_value());
    let previous = req.set_slot(frame, slot, cell.as_value());
    req.release(previous);
    let started = if is_array {
        match req.reference_separate(cell.heap()) {
            Ok(Some(array)) => Some(req.heap_mut().iterator_add(array, 0)),
            Ok(None) => None,
            Err(thrown) => return threw(req, frame, &thrown),
        }
    } else {
        let Some(object) = subject.as_object().copied() else {
            return Ok(advance(req, frame));
        };
        match req.object_walk_start(object) {
            Ok(started) => started,
            Err(thrown) => return threw(req, frame, &thrown),
        }
    }
    .map(HashIter);
    set_fe_iter(req, frame, slot, started);
    if op.op1_type == OperandKind::Var {
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
    }
    if started.is_none() {
        req.vm_mut().jump(frame, op.op2);
        return Ok(Stepped::Same(frame));
    }
    Ok(advance(req, frame))
}

/// `FE_RESET_RW` over a temporary or a constant: an array is wrapped in
/// a cell of its own (`ZVAL_NEW_REF(array_ref, array_ptr)`), a constant
/// one copied into it first (`zend_array_dup`), and separated; an
/// object is the `VAR`'s as it stands, over its walk.
fn fe_reset_rw_value(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    slot: u32,
) -> Edge {
    let subject = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    if let Some(array) = subject.as_array().copied() {
        let owned = if op.op1_type == OperandKind::Const {
            match req.array_dup(array.heap()) {
                Ok(copy) => Value::array(PhpArray::of(copy)),
                Err(thrown) => return threw(req, frame, &thrown),
            }
        } else {
            // The temporary's owner is the cell's, and the slot is
            // emptied so nothing gives it up twice.
            let _ = req.take_slot(frame, exec::temporary_slot(code, op.op1));
            subject
        };
        let cell = match req.reference(owned) {
            Ok(cell) => cell,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        let previous = req.set_slot(frame, slot, cell.as_value());
        req.release(previous);
        let started = match req.reference_separate(cell.heap()) {
            Ok(Some(array)) => Some(HashIter(req.heap_mut().iterator_add(array, 0))),
            Ok(None) => None,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        set_fe_iter(req, frame, slot, started);
        return Ok(advance(req, frame));
    }
    if let Some(object) = subject.as_object().copied()
        && op.op1_type != OperandKind::Const
    {
        if crate::interfaces::is_traversable(req, object.class()) {
            return fe_reset_iterator(req, frame, code, op, object, true);
        }
        let _ = req.take_slot(frame, exec::temporary_slot(code, op.op1));
        let previous = req.set_slot(frame, slot, subject);
        req.release(previous);
        let started = match req.object_walk_start(object) {
            Ok(started) => started.map(HashIter),
            Err(thrown) => return threw(req, frame, &thrown),
        };
        set_fe_iter(req, frame, slot, started);
        if started.is_none() {
            req.vm_mut().jump(frame, op.op2);
            return Ok(Stepped::Same(frame));
        }
        return Ok(advance(req, frame));
    }
    fe_reset_refused(req, frame, code, op, &subject)
}

/// `ZEND_FE_FETCH_RW` (`Z/Zend/zend_vm_def.h:7256`): the subject the
/// reset's `VAR` holds, a reference followed. Over an array the
/// iterator's position (`zend_hash_iterator_pos_ex`), which separates
/// an array the body shared and moves the walk onto a table the body
/// replaced; the first live element from there, or the jump to the
/// opline `extended_value` names; the iterator one past it; the key to
/// `result`. Over an object the next property the scope may see
/// ([`Request::object_walk`]). The element is made a reference in place
/// where it is not one (`ZVAL_NEW_EMPTY_REF`), a declared property's
/// with its type or php's `readonly` refusal
/// ([`Request::walked_reference`]), and the CV `op2` names is bound to
/// that cell (`ZVAL_REF`), what it held given up. A subject the body
/// made neither array nor object is php's warning and the jump.
///
/// ```php
/// #[AllowDynamicProperties]
/// class P { public $a = 1; protected $b = 2; public int $c = 3; }
/// $o = new P; $o->d = 4;
/// foreach ($o as $k => &$v) { $v = $v * 10; }
/// unset($v);
/// var_dump($o);
/// ```
/// ```expect
/// object(P)#1 (4) {
///   ["a"]=>
///   int(10)
///   ["b":protected]=>
///   int(2)
///   ["c"]=>
///   int(30)
///   ["d"]=>
///   int(40)
/// }
/// ```
///
/// A typed property the loop binds keeps its type through the cell, so
/// a write through the loop's variable is held to it; a `readonly` one
/// cannot be bound at all.
///
/// ```php
/// class T { public int $n = 1; public function __construct(public readonly int $r = 2) {} }
/// $o = new T;
/// try { foreach ($o as &$v) { $v = "x"; } } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// unset($v);
/// $o = new T;
/// try { foreach ($o as $k => &$v) { echo "$k\n"; } } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot assign string to reference held by property T::$n of type int
/// n
/// Cannot acquire reference to readonly property T::$r
/// ```
///
/// ```php
/// $a = [1, 2];
/// foreach ($a as &$v) { $a = 5; }
/// echo "on\n";
/// ```
/// ```expect
/// Warning: foreach() argument must be of type array|object, int given in %s on line 3
/// on
/// ```
pub(crate) fn fe_fetch_rw<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let slot = exec::temporary_slot(code, op.op1);
    let Some(at) = req.slot_ptr(frame, slot) else {
        return Err(Fatal::Error(String::from(
            "InternalError: FE_FETCH_RW over a VAR the frame does not have",
        )));
    };
    let found = match foreach_var(req, at.read()) {
        ForeachVar::Reference { cell, walk } => {
            let subject = cell.as_value().deref();
            if let Some(array) = subject.as_array().copied() {
                fe_walk_array_rw(req, op, cell, array, walk)
            } else if let Some(object) = subject.as_object().copied() {
                fe_walk_object_rw(req, op, object, walk)
            } else {
                crate::api::raising::warning(
                    req,
                    &format!(
                        "foreach() argument must be of type array|object, {} given",
                        crate::ops::value_name(req, &subject.undef_as_null())
                    ),
                );
                if req.is_pending() {
                    return Ok(Stepped::Threw(frame));
                }
                Ok(Ok(None))
            }
        }
        ForeachVar::Properties { object, walk } => fe_walk_object_rw(req, op, object, walk),
        ForeachVar::Iterator(iterator) => fe_iterator_rw(req, iterator, op),
        ForeachVar::Array { .. } | ForeachVar::Skipped => {
            return Err(Fatal::Error(String::from(
                "InternalError: FE_FETCH_RW over a VAR no by-reference reset filled",
            )));
        }
    };
    let BoundRw { key, cell, owned } = match found? {
        Ok(Some(found)) => found,
        Ok(None) => {
            req.vm_mut().jump(frame, op.extended_value);
            return Ok(Stepped::Same(frame));
        }
        Err(thrown) => return threw(req, frame, &thrown),
    };
    if let Some(key) = key {
        let out = exec::temporary_slot(code, op.out);
        let previous = req.set_slot(frame, out, key);
        req.release(previous);
    }
    let bound = cell.as_value();
    if op.op2_type == OperandKind::Cv {
        let same = req
            .slot(frame, op.op2)
            .as_php_ref()
            .is_some_and(|held| *held == cell);
        if !same {
            req.addref(&bound);
            let garbage = req.set_slot(frame, op.op2, bound);
            req.release(garbage);
        }
    } else {
        req.addref(&bound);
        let out = exec::temporary_slot(code, op.op2);
        let garbage = req.set_slot(frame, out, bound);
        req.release(garbage);
    }
    if owned {
        req.release(bound);
    }
    Ok(advance(req, frame))
}

/// What one by-reference fetch found: `None` is the end of the walk, the
/// outer `Err` a fatal and the inner a throw.
type FoundRw = Result<Result<Option<BoundRw>, Thrown>, Fatal>;

/// One element a by-reference fetch binds its variable to.
struct BoundRw {
    /// The key, a value of its own for `result`; none where the opline
    /// keeps no key.
    key: Option<Value>,
    /// The cell the variable is bound to.
    cell: PhpRef,
    /// Whether the fetch holds an owner of the cell, given up once the
    /// variable holds its own: an iterator's element, which no table
    /// keeps; an array's or an object's is the element's.
    owned: bool,
}

/// `FE_FETCH_RW` over the array a reference holds: the iterator's
/// position (`zend_hash_iterator_pos_ex`), which separates an array the
/// body shared and moves the walk onto a table the body replaced; the
/// first live element from there; the iterator one past it; and the
/// element made a reference in place where it is not one
/// (`ZVAL_NEW_EMPTY_REF`).
fn fe_walk_array_rw(
    req: &mut Request,
    op: &Opline,
    cell: PhpRef,
    array: PhpArray,
    walk: HashIter,
) -> FoundRw {
    let (pos, walked) = req.heap_mut().iterator_pos_ex(walk.0, array.heap())?;
    if !walked.is_same(array.heap()) {
        // `ZVAL_ARR(array, zend_array_dup(ht))`: the reference holds
        // the copy from here, the shared original one owner fewer.
        cell.heap().val_ref().write(php_heap::Zval::array(walked));
    }
    let Some((found, key, value)) = walked.live_from(pos) else {
        return Ok(Ok(None));
    };
    req.heap_mut()
        .iterator_set_pos(walk.0, found.saturating_add(1));
    let value = exec::value_of(value);
    let element = if let Some(element) = value.as_php_ref() {
        *element
    } else {
        let element = match req.reference(value) {
            Ok(element) => element,
            Err(thrown) => return Ok(Err(thrown)),
        };
        if let Some(place) = walked.value_ref(key) {
            place.write(element.as_value().to_zval());
        }
        element
    };
    Ok(fe_key_kept(req, op, key).map(|key| {
        Some(BoundRw {
            key,
            cell: element,
            owned: false,
        })
    }))
}

/// `FE_FETCH_RW` over an object walked by its properties: the next one
/// the scope may see ([`Request::object_walk`]), bound through
/// [`Request::walked_reference`], which gives a declared property's cell
/// its type or refuses a `readonly` one.
fn fe_walk_object_rw(req: &mut Request, op: &Opline, object: PhpObject, walk: HashIter) -> FoundRw {
    let (key, walked) = match req.object_walk(object, walk.0) {
        Ok(Some(walked)) => walked,
        Ok(None) => return Ok(Ok(None)),
        Err(thrown) => return Ok(Err(thrown)),
    };
    let cell = match req.walked_reference(object, walked) {
        Ok(cell) => cell,
        Err(thrown) => return Ok(Err(thrown)),
    };
    Ok(fe_key_kept(req, op, key).map(|key| {
        Some(BoundRw {
            key,
            cell,
            owned: false,
        })
    }))
}

/// `FE_FETCH_RW`'s iterator arm (`Z/Zend/zend_vm_def.h:7358`): the
/// element [`crate::interfaces::fetch`] finds by reference, the zval
/// `get_current_data` points at made a reference where it is not one.
/// The one iterator a reset by reference makes is a generator's that
/// yields by reference, whose current value is made a reference in the
/// generator itself, so a write through the loop's variable is a write
/// to what the body yielded; `zend_user_it_get_iterator` refuses a loop
/// by reference, and a class's own `get_iterator` that takes one is
/// #735's. A generator that does not yield by reference is
/// `zend_generator_get_iterator`'s refusal.
///
/// ```php
/// function &g() { $a = 1; yield $a; echo "a=$a\n"; }
/// foreach (g() as &$v) $v = 5;
/// function h() { yield 1; }
/// try { foreach (h() as &$v) {} } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// a=5
/// You can only iterate a generator by-reference if it declared that it yields by-reference
/// ```
fn fe_iterator_rw(req: &mut Request, iterator: PhpObject, op: &Opline) -> FoundRw {
    let keyed = op.out_type != OperandKind::Unused;
    let (current, key) = match crate::interfaces::fetch(req, iterator, keyed, true) {
        Ok(Some(found)) => found,
        Ok(None) => return Ok(Ok(None)),
        Err(thrown) => return Ok(Err(thrown)),
    };
    let cell = if let Some(cell) = current.as_php_ref() {
        Ok(*cell)
    } else {
        req.reference(current)
    };
    match cell {
        Ok(cell) => Ok(Ok(Some(BoundRw {
            key,
            cell,
            owned: true,
        }))),
        Err(thrown) => {
            if let Some(key) = key {
                req.release(key);
            }
            Ok(Err(thrown))
        }
    }
}

/// The key a fetch writes to its `result` where the opline keeps one
/// (`RETURN_VALUE_USED`), [`fe_key`]'s value, and none otherwise.
fn fe_key_kept(
    req: &mut Request,
    op: &Opline,
    key: php_heap::Key,
) -> Result<Option<Value>, Thrown> {
    if op.out_type == OperandKind::Unused {
        return Ok(None);
    }
    fe_key(req, key).map(Some)
}

/// `ZEND_FE_FREE` (`Z/Zend/zend_vm_def.h:3260`): the `VAR` the reset
/// filled given up where the loop ends, the array's, the object's or
/// the reference's owner released (`zval_ptr_dtor_nogc`), and nothing
/// over the `UNDEF` a warned-on reset left. The fetch before it jumps
/// here, and a `break` out of the loop lands here too. A `VAR` that
/// holds anything but an array is a walk registered with a hash
/// iterator, an object's or a by-reference one, and the iterator its
/// `Z_FE_ITER` names is deleted first (`zend_hash_iterator_del`), `-1`
/// being none.
///
/// A `return` inside a by-reference loop runs the `FE_FREE` the compiler
/// put before it, and a throw out of one frees the variable through the
/// unwind's `ZEND_LIVE_LOOP` range; either way the iterator goes with
/// it, so a loop nested in a loop over one array walks it afresh.
///
/// ```php
/// function f() { foreach ([1, 2, 3] as &$v) { if ($v == 2) return $v; } }
/// var_dump(f());
/// function g() { $a = [1, 2]; foreach ($a as &$v) { throw new Exception("x"); } }
/// try { g(); } catch (Exception $e) { echo "caught\n"; }
/// $a = [1, 2, 3];
/// foreach ($a as &$v) { foreach ($a as &$w) { $w++; } }
/// var_dump($a);
/// ```
/// ```expect
/// int(2)
/// caught
/// array(3) {
///   [0]=>
///   int(4)
///   [1]=>
///   int(5)
///   [2]=>
///   &int(6)
/// }
/// ```
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
    fe_iterator_del(req, frame, exec::temporary_slot(code, op.op1));
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    Ok(advance(req, frame))
}

/// [`ForeachVar::iterator_del`] over the loop variable in `slot`.
fn fe_iterator_del(req: &mut Request, frame: FrameRef, slot: u32) {
    if let Some(held) = req.slot_ptr(frame, slot).map(php_heap::ZvalRef::read) {
        ForeachVar::iterator_del(req, held);
    }
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

/// `ZEND_CONCAT` (`Z/Zend/zend_vm_def.h:374`): [`crate::ops::concat`],
/// the same way, but for php's own arm for a left temporary
/// ([`concat_into`]), which grows the left string where it stands.
///
/// The string `str_repeat` returned is the temporary's alone and grows
/// into the `run` the joined string needs, the old slot given back as
/// it moves, so the peak moves by the joined string alone and not by
/// the two standing together; a string a variable holds is copied into
/// a new one beside it:
///
/// ```php
/// $a = str_repeat('a', 5000); memory_reset_peak_usage(); $m = memory_get_usage();
/// $b = str_repeat('b', 3000) . $a; $p = memory_get_peak_usage() - $m;
/// memory_reset_peak_usage(); $m = memory_get_usage();
/// $c = $b . '!'; $q = memory_get_peak_usage() - $m;
/// var_dump(strlen($b), $c === str_repeat('b', 3000) . $a . '!', $p, $q, $b[0], $b[7999]);
/// ```
/// ```expect
/// int(8000)
/// bool(true)
/// int(8192)
/// int(8192)
/// string(1) "b"
/// string(1) "a"
/// ```
pub(crate) fn concat<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    concat_into(req, frame, code, op)
}

/// The body `CONCAT` and `FAST_CONCAT` share: php's arm for a left
/// operand that is a temporary, when both sides are strings and neither
/// is empty (`Z/Zend/zend_vm_def.h:407-418`, `:3317-3328`), hands the
/// temporary's owner to [`Request::string_append`], which grows the
/// string where it stands when that owner is its only one and copies it
/// otherwise, as `zend_string_extend` and php's alloc arm beside it do;
/// every other pair is [`binary`] over [`crate::ops::concat`].
fn concat_into(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    if matches!(op.op1_type, OperandKind::Tmp | OperandKind::Var) {
        let left = exec::operand(req, frame, code, op.op1_type, op.op1);
        let right = exec::operand(req, frame, code, op.op2_type, op.op2);
        if let (Some(&a), Some(&b)) = (left.as_php_string(), right.as_php_string())
            && !a.is_empty()
            && !b.is_empty()
        {
            // The temporary's owner is the append's now, and nothing
            // releases it again.
            req.take_slot(frame, exec::temporary_slot(code, op.op1));
            let joined = req.string_append(a, b);
            exec::free_operand(req, frame, code, op.op2_type, op.op2);
            return match joined {
                Ok(joined) => {
                    let slot = exec::temporary_slot(code, op.out);
                    let previous = req.set_slot(frame, slot, Value::string(joined));
                    req.release(previous);
                    Ok(advance(req, frame))
                }
                Err(thrown) => {
                    req.leave_pending(&thrown);
                    Ok(Stepped::Threw(frame))
                }
            };
        }
    }
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
/// the operands freed after the kernel (`FREE_OP1`, `FREE_OP2`). A left
/// piece that is a temporary string grows where it stands, as
/// [`concat`]'s does ([`concat_into`]).
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
    concat_into(req, frame, code, op)
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
///
/// The step of a variable holding a reference is written inside the
/// cell, and the temporary takes the dereferenced value, so `++$m`
/// reads as the number and not as the cell.
///
/// ```php
/// $n = 1;
/// $m = &$n;
/// var_dump(++$m, $n, $m++, $n);
/// ```
/// ```expect
/// int(2)
/// int(2)
/// int(2)
/// int(3)
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
///
/// A variable holding a reference is stepped inside the cell, so the
/// step is read by every other name bound to it: a by-reference
/// parameter and a `&` binding alike.
///
/// ```php
/// function step(&$x) { $x--; }
/// $a = 1; step($a); var_dump($a);
/// $b = 1; $r = &$b; $r--; var_dump($b, $r);
/// ```
/// ```expect
/// int(0)
/// int(0)
/// int(0)
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

/// `ZEND_BW_NOT` (`Z/Zend/zend_vm_def.h:947`), what `~` compiles to:
/// [`crate::ops::bitwise_not`] over the one operand, into the
/// temporary, with the operand freed after it. php's compiler folds the
/// operator over every constant `zend_unary_op_produces_error` lets
/// through, so what reaches this handler is a variable, a call's value,
/// or a constant of the kinds the fold refuses: `null`, `false`, `true`,
/// an array and a double the long conversion would lose something of.
///
/// ```php
/// $x = 6 * extension_loaded('Core');
/// var_dump(~$x, ~~$x, -~$x);
/// ```
/// ```expect
/// int(-7)
/// int(6)
/// int(7)
/// ```
pub(crate) fn bw_not<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    unary(req, frame, code, op, crate::ops::bitwise_not)
}

/// `ZEND_BW_OR` (`Z/Zend/zend_vm_def.h:824`), what `|` compiles to:
/// [`crate::ops::bitwise_or`] over both operands, into the temporary,
/// with both freed after it.
pub(crate) fn bw_or<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::bitwise_or)
}

/// `ZEND_BW_XOR` (`Z/Zend/zend_vm_def.h:902`), what `^` compiles to:
/// [`crate::ops::bitwise_xor`] the same way.
pub(crate) fn bw_xor<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::bitwise_xor)
}

/// `ZEND_BW_AND` (`Z/Zend/zend_vm_def.h:863`), what `&` compiles to:
/// [`crate::ops::bitwise_and`] the same way.
pub(crate) fn bw_and<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, crate::ops::bitwise_and)
}

/// `ZEND_BOOL_NOT` (`Z/Zend/zend_vm_def.h:961`), what `!` compiles to:
/// [`crate::ops::is_true`] over the operand negated, `!i_zend_is_true`,
/// into the temporary; the operand freed after it. A temporary this
/// leaves, `zend_do_free` never frees, as `ZEND_BOOL`'s is never freed.
///
/// php's compiler folds the operator over a constant operand that is no
/// double (`zend_try_ct_eval_unary_op`), so a constant reaches the
/// kernel only as a float, or from a CV, a call or a temporary.
///
/// ```php
/// $a = 0; var_dump(!$a, !"0", !"", !1, ![], !null);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
///
/// A double is the one constant php leaves for the opcode, and an
/// object is the operand that is always true.
///
/// ```php
/// var_dump(!1.5, !0.0, !NAN, !!new stdClass);
/// ```
/// ```expect
/// Warning: unexpected NAN value was coerced to bool in %s on line 2
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
pub(crate) fn bool_not<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    unary(req, frame, code, op, |req, value| {
        Ok(Value::bool(!crate::ops::is_true(req, value)))
    })
}

/// `ZEND_BOOL_XOR` (`Z/Zend/zend_vm_def.h:920`), what `xor` compiles to:
/// `boolean_xor_function` over both operands, which is
/// [`crate::ops::is_true`] on each and the two answers differing. It is
/// the one spelling of the five with no jump in it, so both sides are
/// served whatever either answers, and `pass_two` swaps its operands
/// where `op1` is the lesser kind (`SPEC(COMMUTATIVE)`), which the
/// answer does not read.
///
/// ```php
/// function f($n) { echo "f$n"; return $n; }
/// var_dump(f(1) xor f(0));
/// $a = 1; $b = 0; $c = "";
/// var_dump($a xor $b, $a xor $a, $b xor $c);
/// ```
/// ```expect
/// f1f0bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
pub(crate) fn bool_xor<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary(req, frame, code, op, |req, left, right| {
        Ok(Value::bool(
            crate::ops::is_true(req, left) != crate::ops::is_true(req, right),
        ))
    })
}

/// `ZEND_ISEMPTY` (`Z/Zend/zend_compile.h:1005`), the one bit of an
/// `ISSET_ISEMPTY_*` opline's `extended_value`: the word compiled was
/// `empty` and not `isset`.
const ISEMPTY: u32 = 1 << 0;

/// `ZEND_ISSET_ISEMPTY_CV` (`Z/Zend/zend_vm_def.h:7459`), what
/// `isset($a)` and `empty($a)` compile to over a CV: the slot read where
/// it stands, and a slot nothing wrote read as the `UNDEF` it is, with
/// no warning and nothing made of it. Under `isset` the answer is
/// [`crate::ops::is_set`]; under `ZEND_ISEMPTY` it is
/// [`crate::ops::is_true`] negated, `!i_zend_is_true`, which is the arm
/// that can warn, over a NAN. php branches on the answer where a `JMPZ`
/// or a `JMPNZ` follows (`ZEND_VM_SMART_BRANCH`); the temporary is
/// written here and the jump reads it, as `TYPE_CHECK`'s is.
///
/// ```php
/// $a = 1; $n = null; $z = "0";
/// var_dump(isset($a), empty($a), isset($n), empty($n), isset($u), empty($u), empty($z));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
pub(crate) fn isset_isempty_cv<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = req.slot(frame, op.op1);
    let answer = if op.extended_value & ISEMPTY == 0 {
        crate::ops::is_set(&value)
    } else {
        !crate::ops::is_true(req, &value)
    };
    write_out(req, frame, code, op, &Value::bool(answer));
    Ok(advance(req, frame))
}

/// `ZEND_ISSET_ISEMPTY_DIM_OBJ` (`Z/Zend/zend_vm_def.h:7546`), what
/// `isset($a[0])` and `empty($a[0])` compile to over the last link of a
/// chain: the container read as `GET_OP1_OBJ_ZVAL_PTR_UNDEF(BP_VAR_IS)`
/// reads it, which warns of nothing, and the answer written into the
/// temporary as a bool.
///
/// An array is asked whether the key is there and what it holds, never
/// fetched from, so no `Undefined array key` warning is raised on any
/// path: under `isset` the answer is that the entry stands and is not
/// `null`, a reference followed, and under `ZEND_ISEMPTY` it is that the
/// entry is absent or [`crate::ops::is_true`] says no. A string is the
/// offset being inside it, and under `empty` the byte at it being `0`
/// (`zend_isset_dim_slow`, `zend_isempty_dim_slow`). An object is
/// `offsetExists`, with `offsetGet`'s truth after it under `empty`
/// ([`Request::has_dimension`] with `not_empty`). Everything else is
/// `false` for `isset` and `true` for `empty`, which is what the two
/// slow paths answer for a container that is none of the three.
///
/// ```php
/// $a = [1, 'k' => null];
/// var_dump(isset($a[0]), isset($a['k']), isset($a[9]), empty($a[0]), empty($a['k']));
/// $s = "abc";
/// var_dump(isset($s[1]), isset($s[9]), isset($s["x"]), isset($s[-1]));
/// $z = "0ab";
/// var_dump(empty($z[0]), empty($z[1]), empty($z[9]));
/// $n = null;
/// var_dump(isset($n[0]), empty($n[0]));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
///
/// An object is its `ArrayAccess` pair, and a class that is none is
/// php's `Error` under either word:
///
/// ```php
/// class C implements ArrayAccess {
///     public function offsetExists($offset): bool { echo "e"; return $offset === 0; }
///     public function offsetGet($offset): mixed { echo "g"; return 0; }
///     public function offsetSet($offset, $value): void {}
///     public function offsetUnset($offset): void {}
/// }
/// $c = new C;
/// var_dump(isset($c[0]), isset($c[1]), empty($c[0]));
/// $o = new stdClass;
/// var_dump(isset($o[0]));
/// ```
/// ```expect exit=255
/// eeegbool(true)
/// bool(false)
/// bool(true)
///
/// Fatal error: Uncaught Error: Cannot use object of type stdClass as array in %s:11
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 11
/// ```
pub(crate) fn isset_isempty_dim_obj<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let empty = op.extended_value & ISEMPTY != 0;
    let container = exec::operand_undef(req, frame, code, op.op1_type, op.op1).deref();
    let answer = if let Some(array) = container.as_array().copied() {
        let offset = exec::operand(req, frame, code, op.op2_type, op.op2);
        crate::ops::isset_dim(
            req,
            array,
            &offset,
            op.op2_type == OperandKind::Const,
            empty,
        )
    } else {
        // `if (OP2_TYPE == IS_CONST && Z_EXTRA_P(offset) ==
        // ZEND_EXTRA_VALUE) offset++`: an object is handed the dimension
        // as it was written, which is the arm that reads it at all.
        let offset = if container.as_object().is_some() {
            object_dim(req, frame, code, op).unwrap_or_else(Value::null)
        } else {
            exec::operand(req, frame, code, op.op2_type, op.op2)
        };
        isset_container(req, &container, &offset, empty)
    };
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    match answer {
        Ok(answer) => {
            write_out(req, frame, code, op, &Value::bool(answer));
            Ok(advance(req, frame))
        }
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// `ZEND_ISSET_ISEMPTY_PROP_OBJ` (`Z/Zend/zend_vm_def.h:7625`), what
/// `isset($o->p)` and `empty($o->p)` compile to: the receiver read as
/// `BP_VAR_IS` reads it, which says nothing of a variable nothing wrote,
/// the property named by `op2`, and the object's handler table's
/// `has_property` asked `ZEND_PROPERTY_ISSET` or, under `ZEND_ISEMPTY`,
/// `ZEND_PROPERTY_NOT_EMPTY`, whose answer `empty` reads inverted
/// (`result = (extended_value & ZEND_ISEMPTY) ^ has_property(...)`). A
/// receiver that is no object is `false` under `isset` and `true` under
/// `empty`, with nothing raised, which is php's `result =
/// (opline->extended_value & ZEND_ISEMPTY)`.
///
/// php branches on the answer where a `JMPZ` or a `JMPNZ` follows
/// (`ZEND_VM_SMART_BRANCH`); the temporary stands and the jump reads it,
/// as it does for every `ISSET_ISEMPTY_*` here.
///
/// ```php
/// class C { public $p = 1; public $n = null; private $q = 2; public int $t; }
/// $c = new C;
/// $c->dyn = 0;
/// var_dump(isset($c->p), isset($c->n), isset($c->q), isset($c->t), isset($c->nope), isset($c->dyn));
/// var_dump(empty($c->p), empty($c->n), empty($c->dyn), empty($c->nope));
/// $u = null;
/// var_dump(isset($u->p), empty($u->p), isset($nothing->p));
/// ```
/// ```expect
///
/// Deprecated: Creation of dynamic property C::$dyn is deprecated in %s on line 4
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
pub(crate) fn isset_isempty_prop_obj<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let empty = op.extended_value & ISEMPTY != 0;
    let name = match op.op2_type {
        OperandKind::Const => exec::constant(code, ConstIdx(op.op2)),
        _ => {
            return Err(Fatal::Error(String::from(
                "InternalError: ZEND_ISSET_ISEMPTY_PROP_OBJ over a property named at run time",
            )));
        }
    };
    let name = name.as_str_bytes().unwrap_or_default().to_vec();
    let container = exec::operand_undef(req, frame, code, op.op1_type, op.op1).deref();
    let answer = match container.as_object().copied() {
        Some(object) => {
            let test = if empty {
                PropertyTest::NotEmpty
            } else {
                PropertyTest::Isset
            };
            let table = req.object_handlers(object.heap());
            table
                .has_property(req, object, &name, test)
                .map(|held| held != empty)
        }
        None => Ok(empty),
    };
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    match answer {
        Ok(answer) => {
            write_out(req, frame, code, op, &Value::bool(answer));
            Ok(advance(req, frame))
        }
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// `zend_isset_dim_slow` and `zend_isempty_dim_slow`
/// (`Z/Zend/zend_execute.c:3294`), the ladder for a container that is no
/// array: an object through `has_dimension`, `offsetExists` and, under
/// `empty`, `offsetGet`'s truth after it, the answer negated for
/// `empty`; a string the offset standing inside it, and under `empty`
/// the byte at it being `0`, with an offset that is no simple scalar and
/// no whole numeric string answering as an absent one does; and
/// everything else `false` under `isset` and `true` under `empty`, with
/// nothing raised. An object is asked through its class's
/// `has_dimension`, so a class of an extension answers as php's own
/// `has_dimension` does, `ArrayObject` holding a `null` as not set.
///
/// ```php
/// $o = new ArrayObject(['n' => null, 'z' => 0]);
/// var_dump(isset($o['n']), isset($o['z']), empty($o['z']), isset($o['none']));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
fn isset_container(
    req: &mut Request,
    container: &Value,
    offset: &Value,
    empty: bool,
) -> Result<bool, Thrown> {
    match container.as_ref() {
        ValueRef::Object(object) => {
            let table = req.object_handlers(object.heap());
            let held = table.has_dimension(req, *object, offset, empty)?;
            Ok(held != empty)
        }
        ValueRef::Str(text) => {
            let Some(at) = string_isset_offset(req, offset) else {
                return Ok(empty);
            };
            let len = i64::try_from(text.len()).unwrap_or(i64::MAX);
            let at = if at < 0 { at.saturating_add(len) } else { at };
            if at < 0 || at >= len {
                return Ok(empty);
            }
            if !empty {
                return Ok(true);
            }
            let byte = text
                .as_bytes()
                .get(usize::try_from(at).unwrap_or(0))
                .copied()
                .unwrap_or(0);
            Ok(byte == b'0')
        }
        _ => Ok(empty),
    }
}

/// The offset `zend_isset_dim_slow` reads out of a dimension over a
/// string: a long as itself; a type below `IS_STRING`, which is a slot
/// nothing wrote, a `null`, a bool or a float, through
/// `zval_get_long_ex`; a string only where `is_numeric_string` reads a
/// whole long out of it, with nothing raised for one that does not; and
/// an array, an object or a resource no offset at all, which is `None`
/// here and the absent offset's answer there.
fn string_isset_offset(req: &mut Request, offset: &Value) -> Option<i64> {
    match offset.deref().as_ref() {
        ValueRef::Int(n) => Some(n),
        ValueRef::Undef | ValueRef::Null => Some(0),
        ValueRef::Bool(b) => Some(i64::from(b)),
        ValueRef::Float(f) => Some(crate::ops::double_to_long(req, f)),
        ValueRef::Str(s) => match php_heap::numeric_prefix(s.as_bytes()) {
            Some((php_heap::Number::Long(n), false)) => Some(n),
            _ => None,
        },
        // A reference is followed above, so the arm stands for the
        // three php's `default` refuses an offset for.
        ValueRef::Array(_)
        | ValueRef::Object(_)
        | ValueRef::Resource(_)
        | ValueRef::Reference(_) => None,
    }
}

/// `ZEND_DEFINED`: [`crate::ops::defined`] over the constant string
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

/// `ZEND_JMPZ_EX` (`Z/Zend/zend_vm_def.h:3180`), the jump
/// `zend_compile_short_circuiting` leaves for `&&` and for `and`: the
/// operand read as `GET_OP1_ZVAL_PTR_UNDEF(BP_VAR_R)` reads it, its
/// truth ([`crate::ops::is_true`]) written into the temporary `result`
/// names as a bool, and the jump in `op2` taken past the right side
/// where that truth is false, which is what leaves the right side
/// unserved. A CV nothing wrote is the undefined-variable warning and a
/// false, as `ZVAL_UNDEFINED_OP1` raises it. The jump is forward always,
/// since php's compiler emits this opcode only over a right side it has
/// yet to compile, and `result` carries a temporary where a back-edge
/// would carry its safepoint number, so there is no site here to raise,
/// as [`jmp_set`] has none.
///
/// ```php
/// function f($n) { echo "f$n"; return $n; }
/// var_dump(f(0) && f(1));
/// var_dump(f(2) && f(3));
/// var_dump($u && f(4));
/// ```
/// ```expect
/// f0bool(false)
/// f2f3bool(true)
///
/// Warning: Undefined variable $u in %s on line 5
/// bool(false)
/// ```
pub(crate) fn jmpz_ex<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::operand(req, frame, code, op.op1_type, op.op1);
    let held = crate::ops::is_true(req, &value);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    write_out(req, frame, code, op, &Value::bool(held));
    if held {
        return Ok(advance(req, frame));
    }
    req.vm_mut().jump(frame, op.op2);
    Ok(Stepped::Same(frame))
}

/// `ZEND_JMPNZ_EX` (`Z/Zend/zend_vm_def.h:3216`): [`jmpz_ex`] with the
/// test the other way round, what `||` and `or` compile to, the jump
/// taken past the right side where the operand is true. The bool goes
/// into the temporary whichever way the test went, which is what makes
/// the operator's value the left side's truth and not the left side.
///
/// ```php
/// function f($n) { echo "f$n"; return $n; }
/// var_dump(f(1) || f(2));
/// var_dump(f(0) || f(3));
/// ```
/// ```expect
/// f1bool(true)
/// f0f3bool(true)
/// ```
pub(crate) fn jmpnz_ex<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::operand(req, frame, code, op.op1_type, op.op1);
    let held = crate::ops::is_true(req, &value);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    write_out(req, frame, code, op, &Value::bool(held));
    if !held {
        return Ok(advance(req, frame));
    }
    req.vm_mut().jump(frame, op.op2);
    Ok(Stepped::Same(frame))
}

/// `ZEND_JMP_SET`: the left operand of `a ?: c`
/// (`zend_compile_shorthand_conditional`), read once. True as
/// [`crate::ops::is_true`] reads it, it is the operator's value: it goes
/// into the temporary `result` names, taken as [`qm_assign`] takes one,
/// and the jump in `op2` is taken past the right operand. False, the
/// operand is freed and the opline after it compiles the right operand.
/// The jump is forward always, since php's compiler emits this opcode
/// only over the right operand it has yet to compile, and the opline
/// carries a temporary in `result` where a back-edge would carry its
/// safepoint number, so there is no site here to raise.
///
/// ```php
/// function f() { echo "f"; return 0; }
/// echo f() ?: "b", 1 ?: "b", "" ?: "e", null ?: "n";
/// ```
/// ```expect
/// fb1en
/// ```
pub(crate) fn jmp_set<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::operand(req, frame, code, op.op1_type, op.op1);
    if !crate::ops::is_true(req, &value) {
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        return Ok(advance(req, frame));
    }
    let taken = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, taken);
    req.release(previous);
    req.vm_mut().jump(frame, op.op2);
    Ok(Stepped::Same(frame))
}

/// `ZEND_JMP_NULL` (`Z/Zend/zend_vm_def.h:7841`), the short circuit of a
/// `?->`: a receiver that is neither `null` nor undefined, a reference
/// followed, goes on to the member after it; otherwise the chain's
/// value is written into the `result` the whole chain's would be in, `null` for an
/// expression, `false` under `isset` and `true` under `empty` (the
/// opline's `ZEND_SHORT_CIRCUITING_CHAIN_*`), and the jump in `op2` is
/// taken past the rest of the chain, the arguments of a call in it
/// among what never runs. An undefined variable warns where the chain
/// is read as an expression and not under `isset`, `empty` or `??`
/// (`ZEND_JMP_NULL_BP_VAR_IS`). A `VAR` holding a reference to `null`
/// is freed.
///
/// The jump is forward always, past oplines the compiler has yet to
/// lay down when it emits this one, so there is no site here.
///
/// ```php
/// function f() { echo "f called\n"; return 1; }
/// $a = null;
/// var_dump($a?->b, $a?->m(f()));
/// class C { public $p = 'p'; public $o; function m($x) { return $this; } }
/// $c = new C;
/// var_dump($c?->p, $c?->m(1)?->p, "{$c?->p}!$c?->p", $c->o?->x, $c?->o?->x);
/// var_dump(isset($a?->b), empty($a?->b), isset($c?->p), empty($c?->p));
/// var_dump($u?->x);
/// var_dump($c?->m(2)->p);
/// ```
/// ```expect
/// NULL
/// NULL
/// string(1) "p"
/// string(1) "p"
/// string(3) "p!p"
/// NULL
/// NULL
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
///
/// Warning: Undefined variable $u in %s on line 9
/// NULL
/// string(1) "p"
/// ```
pub(crate) fn jmp_null<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let held = exec::operand_undef(req, frame, code, op.op1_type, op.op1);
    let value = held.deref();
    if !matches!(value.as_ref(), ValueRef::Undef | ValueRef::Null) {
        return Ok(advance(req, frame));
    }
    if held.as_php_ref().is_some() {
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
    }
    let result = match op.extended_value & SHORT_CIRCUITING_CHAIN_MASK {
        SHORT_CIRCUITING_CHAIN_EXPR => {
            if op.op1_type == OperandKind::Cv
                && matches!(held.as_ref(), ValueRef::Undef)
                && op.extended_value & JMP_NULL_BP_VAR_IS == 0
            {
                // `ZVAL_UNDEFINED_OP1`.
                let _ = exec::operand(req, frame, code, op.op1_type, op.op1);
                if req.is_pending() {
                    return Ok(Stepped::Threw(frame));
                }
            }
            Value::null()
        }
        SHORT_CIRCUITING_CHAIN_ISSET => Value::bool(false),
        _ => Value::bool(true),
    };
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, result);
    req.release(previous);
    req.vm_mut().jump(frame, op.op2);
    Ok(Stepped::Same(frame))
}

/// `ZEND_SHORT_CIRCUITING_CHAIN_MASK` and the three values under it
/// (`Z/Zend/zend_compile.h:438`), and `ZEND_JMP_NULL_BP_VAR_IS` beside
/// them, the bits the compiler leaves on a `JMP_NULL`.
const SHORT_CIRCUITING_CHAIN_MASK: u32 = 0x3;
const SHORT_CIRCUITING_CHAIN_EXPR: u32 = 0;
const SHORT_CIRCUITING_CHAIN_ISSET: u32 = 1;
const JMP_NULL_BP_VAR_IS: u32 = 4;

/// `ZEND_COALESCE`: the left side of `a ?? b`, read as
/// `GET_OP1_ZVAL_PTR(BP_VAR_IS)` reads it, which is the silent read: an
/// undefined CV is `null` and no warning stands. A reference is
/// followed. What php's `Z_TYPE_P(value) > IS_NULL` answers true for,
/// anything that is neither undefined nor `null`, is the operator's
/// value: it goes into the temporary `result` names, taken as
/// [`qm_assign`] takes one, and the jump in `op2` is taken past the
/// right side. Otherwise the operand is freed and the opline after it
/// compiles the right side into that same temporary.
///
/// The jump is forward always, since php's compiler emits this opcode
/// only over a right side it has yet to compile, and `result` carries a
/// temporary where a back-edge would carry its safepoint number, so
/// there is no site here to raise.
///
/// ```php
/// function f() { echo "f"; return "r"; }
/// echo $undefined ?? "u", "\n";
/// echo f() ?? "b", "\n";
/// $a = null; $b = 0;
/// echo $a ?? "n", $b ?? "z", "\n";
/// echo $undefined ?? $alsoUndefined ?? "both", "\n";
/// ```
/// ```expect
/// u
/// fr
/// n0
/// both
/// ```
pub(crate) fn coalesce<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::operand_undef(req, frame, code, op.op1_type, op.op1).deref();
    if matches!(value.tag(), TypeTag::Undef | TypeTag::Null) {
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        return Ok(advance(req, frame));
    }
    let taken = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    let slot = exec::temporary_slot(code, op.out);
    let previous = req.set_slot(frame, slot, taken);
    req.release(previous);
    req.vm_mut().jump(frame, op.op2);
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

/// `ZEND_IN_ARRAY`: [`crate::ops::in_array`] over the needle, the table
/// in `op2` and `$strict` in `extended_value`, into the
/// temporary; the needle freed after it. An undefined CV is
/// `ZVAL_UNDEFINED_OP1`, the warning [`exec::operand`] raises, and then
/// `null`, which is what php's handler looks up after it warns. php
/// branches on the answer where a `JMPZ` or a `JMPNZ` follows
/// (`ZEND_VM_SMART_BRANCH`); the jump reads the temporary here, as
/// after [`type_check`].
pub(crate) fn in_array<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let table = exec::constant(code, ConstIdx(op.op2));
    let Some(table) = table.as_array().copied() else {
        return Err(no_table("IN_ARRAY"));
    };
    let strict = op.extended_value != 0;
    unary(req, frame, code, op, |req, needle| {
        crate::ops::in_array(req, needle, table, strict).map(Value::bool)
    })
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
    req.leave_frame(frame);
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
///
/// Where the slot holds a reference, the four helpers
/// (`zend_pre_inc_helper` and its three neighbours,
/// `Z/Zend/zend_vm_def.h:1546-1683`) step `Z_REFVAL_P(var_ptr)` and not
/// the slot: `var_ptr = Z_REFVAL_P(var_ptr)` before
/// `increment_function`, so the step is written inside the cell and
/// every name bound to it reads it, and the temporary takes the
/// dereferenced value. Writing the stepped value over the slot instead
/// would drop the binding, which is what a closure's `use (&$x)` and a
/// by-reference parameter are.
fn incdec(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    post: bool,
    kernel: fn(&mut Request, &Value) -> Result<Value, Thrown>,
) -> Edge {
    // `GET_OP1_ZVAL_PTR_PTR_UNDEF(BP_VAR_RW)`: a CV as it stands, and a
    // `VAR` through the `IS_INDIRECT` a `FETCH_DIM_RW` left there, which
    // is the element the step is written into.
    let place = match op.op1_type {
        OperandKind::Var => write_place(req, frame, code, op.op1_type, op.op1),
        _ => None,
    };
    let named = match place {
        Some(place) => exec::value_of(place.read()).undef_as_null(),
        None => exec::operand(req, frame, code, op.op1_type, op.op1),
    };
    // `if (Z_TYPE_P(var_ptr) == IS_REFERENCE) { var_ptr = Z_REFVAL_P(var_ptr); }`.
    let cell = named.as_php_ref().copied();
    let held = named.deref();
    match kernel(req, &held) {
        Ok(stepped) => {
            if op.out_type != OperandKind::Unused {
                // `ZVAL_COPY(EX_VAR(opline->result.var), var_ptr)`,
                // before the step for a `POST_*` and after it for a
                // `PRE_*`; one more owner either way.
                let out = exec::temporary_slot(code, op.out);
                let value = if post { held } else { stepped };
                req.addref(&value);
                let previous = req.set_slot(frame, out, value);
                req.release(previous);
            }
            // The kernel's one owner of the stepped value is the
            // variable's from here, and what stood there is released,
            // which is what the in-place step does.
            if let Some(cell) = cell {
                cell.set(req, stepped);
            } else if let Some(place) = place {
                let previous = exec::value_of(place.read());
                place.write(stepped.into_zval());
                req.release(previous);
            } else {
                let previous = req.set_slot(frame, op.op1, stepped);
                req.release(previous);
            }
            // `FREE_OP1()`: a `VAR` holding an `IS_INDIRECT` owns
            // nothing, and one holding what `offsetGet` answered gives
            // it up.
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            Ok(advance(req, frame))
        }
        Err(thrown) => {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
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

/// The fatal for an opcode over a constant table, a jump table or
/// `IN_ARRAY`'s, whose `op2` is no array, which no unit the compiler
/// wrote has (design §10).
fn no_table(opcode: &str) -> Fatal {
    Fatal::Error(format!(
        "InternalError: {opcode} names a constant that is no table"
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
///
/// The unwind exit reaches the clause as any throwable does, php's
/// `ex` being set, and matches none of them: `UnwindExit` is a class
/// of its own under no `Throwable`, so every `catch` rethrows it and
/// the unwinder goes on outward.
///
/// ```php
/// try { echo "a\n"; exit(3); } catch (Throwable $t) { echo "caught\n"; }
/// echo "after\n";
/// ```
/// ```expect exit=3
/// a
/// ```
pub(crate) fn catch<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let pending = match req.pending() {
        Some(pending) => Some(pending),
        None if req.unwind_exit() => None,
        None => return jump_to(req, frame, op.op2),
    };
    let slot = Some(CacheSlot(op.extended_value & !LAST_CATCH));
    let matched = pending.is_some_and(|pending| {
        cached_class(req, frame, code, op.op1, slot)
            .is_some_and(|of| req.instance_of(pending.class(), of))
    });
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
/// The unwind exit `exit` throws is told apart twice, as php tells it
/// apart with `zend_is_unwind_exit`: no `finally` block is entered for
/// it, which php's own comment there says is for now
/// (`Z/Zend/zend_vm_def.h:8116`), and a throwable a block it is
/// unwinding through had delayed is given up rather than chained
/// (`Z/Zend/zend_vm_def.h:8143`), so an `exit` inside a `finally`
/// block ends the request with its status and the throwable that
/// entered the block is never rendered.
///
/// ```php
/// try { echo "a\n"; exit(5); } finally { echo "f\n"; }
/// echo "after\n";
/// ```
/// ```expect exit=5
/// a
/// ```
///
/// ```php
/// function f() { try { exit(6); } finally { echo "f\n"; } }
/// try { f(); } catch (Throwable $t) { echo "c\n"; } finally { echo "g\n"; }
/// echo "after\n";
/// ```
/// ```expect exit=6
/// ```
///
/// ```php
/// try { throw new Exception("e"); } finally { echo "f\n"; exit(2); }
/// ```
/// ```expect exit=2
/// f
/// ```
///
/// A `finally` block an ordinary throwable enters still runs, and the
/// `exit` inside a `catch` clause unwinds from there.
///
/// ```php
/// try { try { throw new Exception("e"); } finally { echo "f\n"; } } catch (Exception $e) { echo "c\n"; exit(4); } finally { echo "g\n"; }
/// echo "after\n";
/// ```
/// ```expect exit=4
/// f
/// c
/// ```
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
            if req.unwind_exit() {
                at = offset.checked_sub(1);
                continue;
            }
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
                    None if req.unwind_exit() => req.release(delayed.as_value()),
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
    req.leave_frame(frame);
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
/// `ZEND_RETURNS_VALUE` names; a call's `VAR` that holds no reference,
/// `ZEND_RETURNS_FUNCTION`, is the same notice and the same fresh cell,
/// and one that holds a reference, a call that returned by reference,
/// is that cell. The frame is left as [`r#return`] leaves it.
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
///
/// ```php
/// function g() { return 1; }
/// function &f() { return g(); }
/// function &h(&$x) { return $x; }
/// function &k(&$x) { return h($x); }
/// $b = &f();
/// $a = 1;
/// $c = &k($a);
/// $c = 2;
/// var_dump($b, $a);
/// ```
/// ```expect
/// Notice: Only variable references should be returned by reference in %s on line 3
/// int(1)
/// int(2)
/// ```
pub(crate) fn return_by_ref<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let by_value = match op.op1_type {
        OperandKind::Const | OperandKind::Tmp => true,
        OperandKind::Var => op.extended_value == RETURNS_VALUE,
        OperandKind::Cv | OperandKind::Unused => false,
    };
    let returned = if by_value {
        // `ZEND_RETURNS_VALUE`: a constant or a temporary is no
        // variable, so php raises its notice and wraps the value in a
        // cell of its own; a `VAR` already holding a cell is handed on
        // as it stands (`ZVAL_COPY_VALUE`).
        crate::api::raising::notice(
            req,
            "Only variable references should be returned by reference",
        );
        let value = if op.op1_type == OperandKind::Var {
            req.take_slot(frame, exec::temporary_slot(code, op.op1))
        } else {
            exec::take_operand(req, frame, code, op.op1_type, op.op1)
        };
        if value.as_php_ref().is_some() {
            value
        } else {
            match req.reference(value) {
                Ok(cell) => cell.as_value(),
                Err(thrown) => return threw(req, frame, &thrown),
            }
        }
    } else if let Some(at) = write_place(req, frame, code, op.op1_type, op.op1) {
        let held = exec::value_of(at.read());
        if op.op1_type == OperandKind::Var
            && op.extended_value == RETURNS_FUNCTION
            && held.as_php_ref().is_none()
        {
            // `ZEND_RETURNS_FUNCTION` over a call that returned by
            // value: the notice, and the value moved into a cell of its
            // own (`ZVAL_NEW_REF`).
            crate::api::raising::notice(
                req,
                "Only variable references should be returned by reference",
            );
            let value = req.take_slot(frame, exec::temporary_slot(code, op.op1));
            match req.reference(value) {
                Ok(cell) => cell.as_value(),
                Err(thrown) => return threw(req, frame, &thrown),
            }
        } else {
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
        }
    } else {
        return Err(Fatal::Error(String::from(
            "InternalError: RETURN_BY_REF over an operand that is no place",
        )));
    };
    returned_to_caller::<T>(req, frame, op, returned)
}

/// `ZEND_GENERATOR_CREATE` (`Z/Zend/zend_vm_def.h:4652`): the frame
/// moved into the `Generator` the call returns ([`crate::generator::create`]).
pub(crate) fn generator_create<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    _: &OpArray,
    _: &Opline,
) -> Edge {
    crate::generator::create(req, frame)
}

/// `ZEND_YIELD` (`Z/Zend/zend_vm_def.h:8397`): the previous value and
/// key given up, the value and the key the opline names handed out, an
/// integer key counted up where none is written, the `VAR` the opline
/// writes set to `null` for `send()` to fill, and the frame suspended at
/// the opline. A by-reference generator hands out its variable's own
/// cell, a constant or a temporary with php's notice, and a call that
/// returned by value with the same notice and a copy.
///
/// ```php
/// function &g(array &$a) { yield $a[0]; $x = 1; yield $x; }
/// $a = [1, 2];
/// foreach (g($a) as &$v) $v = 9;
/// var_dump($a);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(9)
///   [1]=>
///   int(2)
/// }
/// ```
///
/// ```php
/// function &g() { $v = 1; yield $v; echo "v=$v\n"; yield 2; }
/// foreach (g() as $k => $x) { var_dump($x); }
/// ```
/// ```expect
/// int(1)
/// v=1
///
/// Notice: Only variable references should be yielded by reference in %s on line 2
/// int(2)
/// ```
pub(crate) fn r#yield<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let generator = crate::generator::running(req, frame)?;
    if crate::generator::forced_close(req, generator) {
        // `zend_yield_in_closed_generator_helper`.
        let thrown = crate::api::raising::throw(
            req,
            "Error",
            "Cannot yield from finally in a force-closed generator",
        );
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        if op.out_type != OperandKind::Unused {
            let garbage = req.take_slot(frame, exec::temporary_slot(code, op.out));
            req.release(garbage);
        }
        return threw(req, frame, &thrown);
    }
    crate::generator::drop_current(req, generator);
    let value = if op.op1_type == OperandKind::Unused {
        Value::null()
    } else if crate::generator::yields_by_ref(req, frame) {
        match yielded_by_ref(req, frame, code, op) {
            Ok(value) => value,
            Err(thrown) => return threw(req, frame, &thrown),
        }
    } else {
        exec::take_operand(req, frame, code, op.op1_type, op.op1)
    };
    let key = (op.op2_type != OperandKind::Unused)
        .then(|| exec::take_operand(req, frame, code, op.op2_type, op.op2));
    let send_target = (op.out_type != OperandKind::Unused).then(|| {
        let slot = exec::temporary_slot(code, op.out);
        let previous = req.set_slot(frame, slot, Value::null());
        req.release(previous);
        slot
    });
    crate::generator::yielded(req, generator, value, key, send_target);
    crate::generator::suspend(req, generator)?;
    Ok(Stepped::Done)
}

/// The value a by-reference `YIELD` hands out: `GET_OP1_ZVAL_PTR_PTR(BP_VAR_W)`
/// made a reference (`ZVAL_MAKE_REF_EX(value_ptr, 2)`), the generator
/// one owner of the cell, then `FREE_OP1`; a constant or a temporary,
/// and a call's value that is no reference, the notice and a copy.
fn yielded_by_ref(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Result<Value, Thrown> {
    if matches!(op.op1_type, OperandKind::Const | OperandKind::Tmp) {
        crate::api::raising::notice(
            req,
            "Only variable references should be yielded by reference",
        );
        return Ok(exec::take_operand(req, frame, code, op.op1_type, op.op1));
    }
    let Some(place) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Ok(Value::null());
    };
    let held = exec::value_of(place.read());
    let value = if op.op1_type == OperandKind::Var
        && op.extended_value == RETURNS_FUNCTION
        && held.as_php_ref().is_none()
    {
        crate::api::raising::notice(
            req,
            "Only variable references should be yielded by reference",
        );
        let value = held.deref();
        req.addref(&value);
        value
    } else if let Some(cell) = held.as_php_ref() {
        req.addref(&held);
        cell.as_value()
    } else {
        let cell = req.reference(held.undef_as_null())?;
        place.write(cell.as_value().to_zval());
        req.addref(&cell.as_value());
        cell.as_value()
    };
    if op.op1_type == OperandKind::Var {
        let slot = exec::temporary_slot(code, op.op1);
        let raw = req.slot_ptr(frame, slot).map(php_heap::ZvalRef::read);
        if raw.is_some_and(|raw| raw.as_indirect().is_some()) {
            let _place = req.take_slot(frame, slot);
        } else {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
        }
    }
    Ok(value)
}

/// `ZEND_YIELD_FROM` (`Z/Zend/zend_vm_def.h:8518`): the operand becomes
/// what the generator delegates to ([`crate::generator::yield_from`])
/// and the frame is suspended at the opline, its temporary `null` until
/// a generator delegated to returns into it; a generator that has
/// already returned is its return value at once, and the frame goes on.
///
/// ```php
/// function inner() { yield 1; return 2; }
/// function outer($i) { $r = yield from $i; var_dump($r); }
/// foreach (outer(inner()) as $v) var_dump($v);
/// function bad() { yield from 1; }
/// try { bad()->current(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// int(2)
/// Can use "yield from" only with arrays and Traversables
/// ```
///
/// A generator delegated to twice is closed the second time, which php
/// checks before it reads what the generator returned:
///
/// ```php
/// function inner() { yield 1; return 2; }
/// function outer($i) { yield from $i; yield from $i; }
/// foreach (outer(inner()) as $v) var_dump($v);
/// ```
/// ```expect exit=255
/// int(1)
///
/// Fatal error: Uncaught Error: Generator passed to yield from was aborted without proper return and is unable to continue in %s:3
/// Stack trace:
/// #0 %s(4): outer(Object(Generator))
/// #1 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn yield_from<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let generator = crate::generator::running(req, frame)?;
    let value = exec::operand(req, frame, code, op.op1_type, op.op1);
    if crate::generator::forced_close(req, generator) {
        let thrown = crate::api::raising::throw(
            req,
            "Error",
            "Cannot use \"yield from\" in a force-closed generator",
        );
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        return threw(req, frame, &thrown);
    }
    let owned = match op.op1_type {
        OperandKind::Tmp | OperandKind::Var => {
            req.take_slot(frame, exec::temporary_slot(code, op.op1))
        }
        _ => {
            req.addref(&value);
            value
        }
    };
    let owned = match owned.as_php_ref() {
        Some(cell) => {
            let held = cell.get();
            req.addref(&held);
            req.release(owned);
            held
        }
        None => owned,
    };
    let out = (op.out_type != OperandKind::Unused).then(|| exec::temporary_slot(code, op.out));
    match crate::generator::yield_from(req, generator, owned, op.op1_type == OperandKind::Const) {
        Ok(crate::generator::From::Finished(retval)) => {
            match out {
                Some(slot) => {
                    let previous = req.set_slot(frame, slot, retval);
                    req.release(previous);
                }
                None => req.release(retval),
            }
            Ok(advance(req, frame))
        }
        Ok(crate::generator::From::Delegating) => {
            if let Some(slot) = out {
                let previous = req.set_slot(frame, slot, Value::null());
                req.release(previous);
            }
            crate::generator::suspend(req, generator)?;
            Ok(Stepped::Done)
        }
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// `ZEND_GENERATOR_RETURN` (`Z/Zend/zend_vm_def.h:4730`): the operand
/// kept as what the generator returned, which `getReturn()` reads, and
/// the frame closed; the resumption is over.
///
/// ```php
/// function g() { $a = [1]; yield; return $a; }
/// $g = g();
/// $g->next();
/// var_dump($g->valid(), $g->getReturn());
/// ```
/// ```expect
/// bool(false)
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// ```
pub(crate) fn generator_return<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let generator = crate::generator::running(req, frame)?;
    let value = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    crate::generator::returned(req, generator, value);
    Ok(Stepped::Done)
}

/// `ZEND_MAKE_REF` (`Z/Zend/zend_vm_def.h:9262`), what a by-reference
/// `return` emits over its variable where a `finally` stands between
/// it and the `RETURN_BY_REF`: the CV is made a reference where it
/// holds none, a CV nothing wrote holding a fresh cell over `null`
/// (`ZVAL_NEW_EMPTY_REF`), and the `VAR` the opline writes holds that
/// cell as one more owner, so what the block writes into the variable
/// is what the caller is bound to. A `VAR` operand is the place a
/// `FETCH_*_W` left as `IS_INDIRECT`, made a reference the same way, or
/// a call's value moved into the written `VAR` as it stands
/// (`ZVAL_COPY_VALUE`), which `RETURN_BY_REF` then wraps under
/// `ZEND_RETURNS_FUNCTION`.
///
/// ```php
/// function &f(&$a) { try { return $a; } finally { $a = 2; } }
/// $x = 1;
/// $y = &f($x);
/// $y = 3;
/// var_dump($x);
/// function &g() { try { return $u; } finally { echo "g\n"; } }
/// $z = &g();
/// var_dump($z);
/// ```
/// ```expect
/// int(3)
/// g
/// NULL
/// ```
///
/// A by-reference return of a call's value past a `finally` is the
/// call's own value, and the notice `RETURN_BY_REF` raises for what is
/// no variable is raised after the block ran.
///
/// ```php
/// function h() { return 1; }
/// function &k() { try { return h(); } finally { echo "k\n"; } }
/// var_dump(k());
/// ```
/// ```expect
/// k
///
/// Notice: Only variable references should be returned by reference in %s on line 3
/// int(1)
/// ```
pub(crate) fn make_ref<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let out = exec::temporary_slot(code, op.out);
    let held = if op.op1_type == OperandKind::Cv {
        let cell = match cv_reference(req, frame, op.op1) {
            Ok(cell) => cell,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        req.addref(&cell.as_value());
        cell.as_value()
    } else {
        let at = exec::temporary_slot(code, op.op1);
        let place = req
            .slot_ptr(frame, at)
            .and_then(|slot| slot.read().as_indirect());
        match place {
            Some(place) => {
                let value = exec::value_of(place.read());
                let cell = if let Some(cell) = value.as_php_ref() {
                    *cell
                } else {
                    let cell = match req.reference(value.undef_as_null()) {
                        Ok(cell) => cell,
                        Err(thrown) => return threw(req, frame, &thrown),
                    };
                    place.write(cell.as_value().to_zval());
                    cell
                };
                req.addref(&cell.as_value());
                cell.as_value()
            }
            None => req.take_slot(frame, at),
        }
    };
    let previous = req.set_slot(frame, out, held);
    req.release(previous);
    Ok(advance(req, frame))
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
        let frame = req.pushed(Arc::clone(&unit));
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
        let frame = req.pushed(Arc::clone(&unit));
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
        let frame = req.pushed(Arc::clone(&taken));
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
        let frame = req.pushed(Arc::clone(&not_taken));
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
        let outer = req.pushed(Arc::clone(&unit));
        let guard = req.stack_mut().push_entry(outer);
        let inner = req.pushed(Arc::clone(&unit));
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
        let frame = req.pushed(Arc::clone(&unit));
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
        let frame = req.pushed(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let init = code.oplines.first().copied().unwrap_or_default();
        assert_eq!(req.cached(frame, CacheSlot(init.out)), Cached::Unprimed);

        assert_eq!(
            init_fcall_by_name::<Nop>(&mut req, frame, code, &init),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().lookups, 1, "the first pass looks the table up");
        let strlen = req.function_named(b"strlen");
        assert!(strlen.is_some());
        assert_eq!(
            req.cached(frame, CacheSlot(init.out)),
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

    /// A unit declaring `f`, whose body is the `INIT_FCALL_BY_NAME` of
    /// [`by_name`] alone, and a main op array with a cache slot of its
    /// own; each op array with the map pointer publication assigns it.
    fn declaring_f(engine: &Engine, strings: &mut php_heap::Strings) -> Arc<Unit> {
        use php_unit::decl::{FunctionDecl, Name};
        let by_name = by_name(strings);
        let mut body = by_name.main.clone();
        body.oplines = body.oplines.iter().take(1).copied().collect();
        let decl = FunctionDecl {
            name: Name::from("f"),
            parameters: Vec::new(),
            return_type: None,
            returns_by_ref: false,
            is_generator: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name::default(),
            line: 0,
            end_line: 0,
            frameless: Vec::new(),
        };
        let mut unit = Unit {
            main: OpArray {
                cache_size: 8,
                ..OpArray::default()
            },
            functions: vec![(decl, body)],
            ..Unit::default()
        };
        crate::engine::assign_map_ptrs(&mut unit, &engine.shared.map_ptr_last);
        Arc::new(unit)
    }

    /// The runtime cache is the op array's for the request and not the
    /// frame's (`ZEND_MAP_PTR(op_array->run_time_cache)`, laid down by
    /// `init_func_run_time_cache` on the first activation): a function
    /// called twice finds on its second call the slot its first call
    /// primed, so the `INIT_FCALL_BY_NAME` in its body looks the table
    /// up once across the two calls, and the main op array's own slot
    /// stands at another entry and is not what it primed.
    #[test]
    fn a_function_called_twice_reads_on_its_second_call_what_its_first_primed() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = declaring_f(&engine, &mut strings);
        let main = req.pushed(Arc::clone(&unit));
        req.declare_functions(&unit);
        let f = req.function_named(b"f").unwrap_or(Function::PASS);
        let code = crate::exec::body_of(&unit, exec::Body::Function(0))
            .cloned()
            .unwrap_or_default();
        let init = code.oplines.first().copied().unwrap_or_default();
        let strlen = req.function_named(b"strlen");

        for call in 1..=2 {
            let state = CallState {
                function: f,
                name: None,
                arguments: Vec::new(),
                count: 0,
                this: None,
                called_scope: None,
                closure: None,
                extra_named: Vec::new(),
                stage: CallStage::Serving,
                frameless: false,
                coerced: Vec::new(),
            };
            let frame = req
                .push_user_frame(f, state, None, Some(main))
                .unwrap_or(main);
            assert_eq!(
                init_fcall_by_name::<Nop>(&mut req, frame, &code, &init),
                Ok(Stepped::Same(frame)),
                "call {call}"
            );
            assert_eq!(
                req.vm().lookups,
                1,
                "the first call looks the table up and the second reads the slot (call {call})"
            );
            assert_eq!(
                req.cached(frame, CacheSlot(init.out)),
                Cached::Function(strlen.unwrap_or(Function::PASS)),
                "the slot is primed with the callee (call {call})"
            );
            assert_eq!(popped_callee(&mut req), strlen, "call {call}");
            req.pop_frame();
        }
        assert_eq!(
            req.cached(main, CacheSlot(0)),
            Cached::Unprimed,
            "the main op array's cache is its own"
        );
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
        let frame = req.pushed(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let init = code.oplines.first().copied().unwrap_or_default();
        let strlen = req.function_named(b"strlen");

        assert_eq!(
            init_ns_fcall_by_name::<Nop>(&mut req, frame, code, &init),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().lookups, 2, "foo\\strlen misses, strlen hits");
        assert_eq!(
            req.cached(frame, CacheSlot(8)),
            Cached::Function(strlen.unwrap_or(Function::PASS))
        );
        assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

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
        let frame = req.pushed(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let init = code.oplines.first().copied().unwrap_or_default();
        for pass in 1..=2 {
            assert_eq!(
                init_fcall_by_name::<Nop>(&mut req, frame, code, &init),
                Ok(Stepped::Threw(frame))
            );
            assert_eq!(req.vm().lookups, pass);
            assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);
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
        let frame = req.pushed(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let clause = code.oplines.first().copied().unwrap_or_default();
        let error = req.class_named(b"error");
        assert!(error.is_some(), "the engine declares Error");
        assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

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
                matches!(req.cached(frame, CacheSlot(0)), Cached::Class(class) if Some(class) == error),
                "the clause's class is in the slot after pass {pass}"
            );
        }
    }

    /// A unit whose main op array is one `INIT_STATIC_METHOD_CALL` over
    /// the class's constant pair at the head of the table and, where
    /// `method` is given, the method's pair after it, with the two cache
    /// slots the compiler reserves in `result` at byte zero. `method`
    /// left out is php's `CONSTRUCTOR` operand, the `C::__construct()`
    /// form, whose `op2` is `UNUSED`.
    fn static_method_call(
        strings: &mut php_heap::Strings,
        class: (&[u8], &[u8]),
        method: Option<(&[u8], &[u8])>,
    ) -> Arc<Unit> {
        let (op2_type, op2) = match method {
            Some(_) => (OperandKind::Const, 2),
            None => (OperandKind::Unused, 0),
        };
        let mut literals = vec![interned(strings, class.0), interned(strings, class.1)];
        if let Some((written, lowered)) = method {
            literals.push(interned(strings, written));
            literals.push(interned(strings, lowered));
        }
        Arc::new(Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::InitStaticMethodCall,
                    op1_type: OperandKind::Const,
                    op1: 0,
                    op2_type,
                    op2,
                    out: 0,
                    ..Opline::default()
                }]
                .into_boxed_slice(),
                literals: literals.into_boxed_slice(),
                cache_size: 16,
                ..OpArray::default()
            },
            ..Unit::default()
        })
    }

    /// `ZEND_INIT_STATIC_METHOD_CALL` over two constants, twice, which
    /// is what a call on a class inside a loop is: the first pass finds its
    /// slot unprimed, looks the class up once and writes the class and
    /// the callee together, `CACHE_POLYMORPHIC_PTR(opline->result.num,
    /// ce, fbc)`; the second reads the pair back and looks nothing up
    /// and resolves no method.
    #[test]
    fn a_static_call_over_two_constants_resolves_its_class_and_callee_once() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = static_method_call(
            &mut strings,
            (b"Closure", b"closure"),
            Some((b"fromCallable", b"fromcallable")),
        );
        let frame = req.pushed(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let op = code.oplines.first().copied().unwrap_or_default();
        let closure = req.class_named(b"closure");
        assert!(closure.is_some(), "the engine declares Closure");
        let callee = closure.and_then(|class| req.method_named(class, b"fromcallable"));
        assert!(callee.is_some(), "Closure::fromCallable is a static method");
        assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

        for pass in 1..=2 {
            req.vm_mut().jump(frame, 0);
            assert_eq!(
                init_static_method_call::<Nop>(&mut req, frame, code, &op),
                Ok(Stepped::Same(frame)),
                "the call frame is pushed on pass {pass}"
            );
            assert_eq!(req.vm().lookups, 1, "one lookup over {pass} pass(es)");
            assert!(
                matches!(
                    req.cached(frame, CacheSlot(0)),
                    Cached::ClassAndCallee { class, function }
                        if Some(class) == closure && Some(function) == callee
                ),
                "the class and the callee are in the slot after pass {pass}"
            );
        }
    }

    /// php writes the class alone into the first slot only where `op2`
    /// is no constant, `if (OP2_TYPE != IS_CONST) CACHE_PTR(...)`: the
    /// `C::__construct()` form, whose `op2` is `UNUSED`, primes the
    /// class on its first pass and looks nothing up on the next, though
    /// the opline ends in php's `Error` for an instance method called
    /// on the class either way.
    #[test]
    fn a_static_call_naming_no_method_primes_the_class_alone() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = static_method_call(&mut strings, (b"Exception", b"exception"), None);
        let frame = req.pushed(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let op = code.oplines.first().copied().unwrap_or_default();
        let exception = req.class_named(b"exception");
        assert!(exception.is_some(), "the engine declares Exception");

        for pass in 1..=2 {
            req.vm_mut().jump(frame, 0);
            assert_eq!(
                init_static_method_call::<Nop>(&mut req, frame, code, &op),
                Ok(Stepped::Threw(frame)),
                "a constructor is an instance method on pass {pass}"
            );
            assert!(req.take_pending().is_some(), "it threw on pass {pass}");
            assert_eq!(req.vm().lookups, 1, "one lookup over {pass} pass(es)");
            assert!(
                matches!(req.cached(frame, CacheSlot(0)), Cached::Class(class)
                    if Some(class) == exception),
                "the class alone is in the slot after pass {pass}"
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
        let frame = req.pushed(Arc::clone(&unit));
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
            assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);
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
        let frame = req.pushed(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let named = req.class_named(b"stdclass");
        assert!(named.is_some(), "the engine declares stdClass");
        let class = named.unwrap_or(ClassId(u32::MAX));
        for slot in [0, 8, 16] {
            req.prime(frame, CacheSlot(slot), Cached::Class(class));
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
        let frame = req.pushed(Arc::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let jmp = code.oplines.first().copied().unwrap_or_default();
        assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

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
            assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Hit);
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
        let frame = req.pushed(Arc::clone(&unit));
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
            assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Miss);
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
            is_generator: false,
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
        let frame = req.pushed(Arc::clone(&unit));
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
        let frame = req.pushed(Arc::clone(&unit));
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
        let frame = req.pushed(Arc::clone(&unit));
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
        let frame = req.pushed(Arc::clone(&unit));
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
            let started = req.object_walk_start(object);
            assert!(
                matches!(started, Ok(Some(_))),
                "Exception declares properties, so the walk is registered"
            );
            if let Ok(Some(idx)) = started {
                assert!(
                    matches!(req.object_walk(object, idx), Ok(None)),
                    "the global scope sees none of Exception's properties"
                );
                req.heap_mut().iterator_del(idx);
            }
            req.release(object.as_value());
        }
    }

    /// A debug build stops at a write of a `TMP` over an owner the slot
    /// still holds, #723's shape planted: a string left in the `TMP` a
    /// `QM_ASSIGN` writes, as `FE_RESET_R` left its subject before #723,
    /// is the `InternalError` naming the opcode, the opline and the
    /// temporary, raised by the loop after the handler and before the
    /// next one, and not a second release several allocations later.
    #[cfg(debug_assertions)]
    #[test]
    fn a_write_over_a_temporary_that_still_holds_a_value_is_an_internal_error() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = Arc::new(Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::QmAssign,
                    op1_type: OperandKind::Const,
                    op1: 0,
                    out_type: OperandKind::Tmp,
                    out: 0,
                    ..Opline::default()
                }]
                .into_boxed_slice(),
                literals: vec![php_heap::Zval::long(7)].into_boxed_slice(),
                num_tmps: 1,
                ..OpArray::default()
            },
            ..Unit::default()
        });
        let frame = req.pushed(Arc::clone(&unit));
        let Ok(left) = crate::api::values::PhpString::new(&mut req, b"an owner left behind") else {
            return;
        };
        let planted = req.set_slot(frame, 0, Value::string(left));
        assert!(
            planted.to_zval().is_undef(),
            "the slot was empty before the plant"
        );

        assert_eq!(
            crate::tier::step(&mut req, frame),
            Err(Fatal::Error(String::from(
                "InternalError: ZEND_QM_ASSIGN at opline 0 writes its TMP #0 over an owner it still holds (String): the handler that last read the slot moved it out and did not empty it"
            )))
        );
        assert_eq!(
            req.slot(frame, 0).as_int(),
            Some(7),
            "the write itself stood"
        );
        req.pop_frame();
    }
}
