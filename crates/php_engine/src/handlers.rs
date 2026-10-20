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

use php_unit::decl::Bare;
use std::cmp::Ordering;
use std::ops::ControlFlow;

use php_heap::{Fatal, FrameRef, Zval, ZvalType};
use php_unit::{CacheSlot, ConstIdx, OpArray, OperandKind, Opline};

use crate::api::abi::{DimensionRead, PropertyPlace, PropertyRead, PropertyTest};
use crate::api::call::Function;
use crate::api::raising::{Thrown, non_static_method_call, none_pending, undefined_method};
use crate::api::request::ropes::RopeAt;
use crate::api::request::{CodeFrom, Opened, Raised, Request};
use crate::api::values::walk::{self, Hold};
use crate::api::values::{
    ClosureInfo, Owner, PhpArray, PhpObject, PhpRef, PhpString, TypeTag, Value, ValueRef,
};
use crate::exec::{
    self, Cached, E_FATAL_ERRORS, LAST_CATCH, cached_class, cleanup_live_vars,
    cleanup_unfinished_calls, free_carried_value, has_only_fatal_errors, restore_silenced,
};
use crate::observer::OnThrow;
use crate::operand::{ByKind, Operand};
use crate::ops::inline::{self, Arith, Binary, Bitwise, Shift};
use crate::ops::{DimRead, Offset, Switched, array_entry, string_dim_offset, string_offset};
use crate::tier::{Flow, Next, SafepointIdx, Site, Stepped, Tier};
use branch::Branch;
use php_unit::Opcode;

/// What one handler did to the frame chain, and the fatal that ends the
/// request instead.
pub(crate) type Edge = Result<Next, Fatal>;

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

/// `ZEND_EXT_STMT` (`Z/Zend/zend_vm_def.h:7923`), which a compile with
/// extended info leaves before each statement (`-e`, #1780): each Zend
/// extension's `statement_handler` told of the statement the frame
/// stands at, and on to the opline after, or what a handler threw
/// raised here ([`crate::statement`]). The bundle declares none, so the
/// loop goes on. `op1`, the operand of a pipe the opline stands before,
/// is read by nothing and freed by nothing, as php's handler leaves it.
///
/// ```php
/// function f($a) { return $a + 1; }
/// $x = f(1);
/// if ($x > 5) { echo "big"; } elseif ($x > 1) { echo "mid"; } else { echo "small"; }
/// for ($i = 0; $i < 2; $i++) { echo $i; }
/// echo "\n", "1234" |> strlen(...), eval('return 5;'), "\n";
/// $o = new ArrayObject([1, 2]);
/// var_dump(count($o), [1, 2] |> array_sum(...));
/// ```
/// ```expect regime=extended-info
/// mid01
/// 45
/// int(2)
/// int(3)
/// ```
pub(crate) fn ext_stmt<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    _: &OpArray,
    _: &Opline,
) -> Edge {
    if let Err(thrown) = crate::statement::told(req, frame) {
        return threw(req, frame, &thrown);
    }
    Ok(advance(req, frame))
}

/// `ZEND_EXT_FCALL_BEGIN` (`Z/Zend/zend_vm_def.h:7935`), before a call
/// op compiled with extended info: php tells each loaded Zend
/// extension's `fcall_begin_handler`, and no Zend extension here has one:
/// its row carries a statement handler alone (#2038; [`ext_stmt`]'s
/// witness serves it).
pub(crate) fn ext_fcall_begin<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    _: &OpArray,
    _: &Opline,
) -> Edge {
    Ok(advance(req, frame))
}

/// `ZEND_EXT_FCALL_END` (`Z/Zend/zend_vm_def.h:7947`), after a call op
/// compiled with extended info: php tells each loaded Zend extension's
/// `fcall_end_handler`, and no Zend extension here has one: its row
/// carries a statement handler alone (#2038; [`ext_stmt`]'s witness
/// serves it).
pub(crate) fn ext_fcall_end<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    _: &OpArray,
    _: &Opline,
) -> Edge {
    Ok(advance(req, frame))
}

/// `ZEND_EXT_NOP` (`Z/Zend/zend_vm_def.h:8086`): nothing, and on to the
/// opline after it. php 8.5's compiler emits none, so only a unit some
/// other tool wrote would carry one.
pub(crate) fn ext_nop<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    _: &OpArray,
    _: &Opline,
) -> Edge {
    Ok(advance(req, frame))
}

/// `ZEND_TICKS` (`Z/Zend/zend_vm_def.h:8024`), what `declare(ticks=N)`
/// leaves after each statement in its scope, `N` in `extended_value`:
/// every `N`th one passed serves the functions `register_tick_function()`
/// kept ([`Request::tick`]), with fiber switching blocked while they are
/// served.
///
/// ```php
/// declare(ticks=1);
/// function t($a) { echo "tick $a\n"; }
/// register_tick_function('t', 'x');
/// echo "a\n";
/// unregister_tick_function('t');
/// echo "b\n";
/// ```
/// ```expect
/// tick x
/// a
/// tick x
/// b
/// ```
///
/// ```php
/// declare(ticks=2);
/// register_tick_function(function () { echo "tick\n"; });
/// echo 1; echo 2; echo 3; echo 4, "\n";
/// ```
/// ```expect
/// tick
/// 1tick
/// 2tick
/// 3tick
/// 4tick
///
/// tick
/// ```
///
/// A fiber neither starts, suspends nor resumes from inside a function
/// `ZEND_TICKS` serves:
///
/// ```php
/// declare(ticks=1);
/// register_tick_function(function () {
///     if (Fiber::getCurrent()) {
///         try { Fiber::suspend(); } catch (FiberError $e) { echo $e->getMessage(), "\n"; }
///     }
/// });
/// $f = new Fiber(function () { echo "in\n"; });
/// $f->start();
/// echo "out\n";
/// ```
/// ```expect
/// in
/// Cannot switch fibers in current execution context
/// out
/// ```
pub(crate) fn ticks<T: Tier>(req: &mut Request, frame: FrameRef, _: &OpArray, op: &Opline) -> Edge {
    if let Err(thrown) = req.tick(op.extended_value) {
        return threw(req, frame, &thrown);
    }
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

/// `ZEND_DECLARE_CLASS` (`Z/Zend/zend_vm_def.h:7959`): `do_bind_class`
/// for the class the frame's unit files under the runtime definition
/// key in the constant after `op1`, which is what php emits for every
/// class it did not early bind and does not delay — one written inside
/// a block or a body, and a top-level one with an interface list or a
/// trait (`zend_compile_class_decl`'s tail; the rest are
/// [`declare_class_delayed`]'s). `op2`, the parent's lowered name, is
/// the declaration's own parent and is not read. Binding is
/// [`Request::bind_class`], whose refusals are php's: `Error` for a
/// parent or an interface nothing declares, `E_ERROR` for a parent that
/// is an interface or an interface list that names something else, and
/// the redeclaration.
///
/// ```php
/// interface I { const J = 3; }
/// var_dump(class_exists('C', false));
/// class C implements I {}
/// var_dump(C::J);
/// ```
/// ```expect
/// bool(false)
/// int(3)
/// ```
///
/// The polyfill's shape, a class declared only where none is:
///
/// ```php
/// foreach ([1, 2] as $i) {
///     if (!class_exists('Poly', false)) {
///         class Poly implements Countable { function count(): int { return 3; } }
///         echo "declared\n";
///     }
/// }
/// var_dump(count(new Poly));
/// ```
/// ```expect
/// declared
/// int(3)
/// ```
pub(crate) fn declare_class<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let serving = exec::standing(req, frame)?;
    let key = runtime_key(code, op)?;
    match req.bind_class_in_slot(&serving.unit, &key) {
        Ok(()) => Ok(advance(req, frame)),
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// The runtime definition key a class-declaring opline carries in the
/// constant after its `op1`, php's `rtd_key = lcname + 1`.
fn runtime_key(code: &OpArray, op: &Opline) -> Result<Vec<u8>, Fatal> {
    constant_bytes(code, op, op.op1.saturating_add(1))
}

/// The string a declaring opline carries in its constant `at`, the name
/// it declares or its runtime definition key.
fn constant_bytes(code: &OpArray, op: &Opline, at: u32) -> Result<Vec<u8>, Fatal> {
    exec::constant(code, ConstIdx(at))
        .as_str_bytes()
        .map(<[u8]>::to_vec)
        .ok_or_else(|| {
            Fatal::error(format!(
                "InternalError: a {} with no string in its constant {at}",
                op.opcode.name().trim_start_matches("ZEND_")
            ))
        })
}

/// `ZEND_DECLARE_CLASS_DELAYED` (`Z/Zend/zend_vm_def.h:7968`), what
/// opcache's compiler emits for a top-level class with no interface
/// and no trait that it could not bind early: nothing where the unit's
/// load bound the class, and the declaration `ZEND_DECLARE_CLASS` makes
/// where it did not ([`Request::bind_delayed`]).
///
/// ```php
/// var_dump(class_exists('C', false));
/// class C extends P {}
/// class P { const K = 'p'; }
/// var_dump(C::K, (new C) instanceof P);
/// ```
/// ```expect
/// bool(false)
/// string(1) "p"
/// bool(true)
/// ```
pub(crate) fn declare_class_delayed<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let serving = exec::standing(req, frame)?;
    let key = runtime_key(code, op)?;
    match req.bind_delayed(&serving.unit, &key) {
        Ok(()) => Ok(advance(req, frame)),
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// `ZEND_DECLARE_ANON_CLASS` (`Z/Zend/zend_vm_def.h:7988`): the class
/// the opline's cache slot holds, or the anonymous class the unit files
/// under the lowered name in `op1` ([`Request::declare_anonymous`]),
/// linked the first time any opline of its declaration runs and the same
/// class every time after, written into the `VAR` the `NEW` after it
/// reads. `op2`, the parent's lowered name, is the declaration's own
/// parent and is not read.
///
/// ```php
/// class P { function __construct(public $v) { echo "P($v)\n"; } }
/// function make($v) { return new class($v) extends P {}; }
/// $a = make(1); $b = make(2);
/// var_dump(get_class($a) === get_class($b), $b->v, $a instanceof P);
/// ```
/// ```expect
/// P(1)
/// P(2)
/// bool(true)
/// int(2)
/// bool(true)
/// ```
///
/// What the link refuses is refused where the declaration runs, after
/// what ran before it, and an anonymous class is told to implement what
/// it left abstract, never to be declared abstract:
///
/// ```php
/// echo "before\n";
/// $o = new class extends Nope {};
/// ```
/// ```expect exit=255
/// before
///
/// Fatal error: Uncaught Error: Class "Nope" not found in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// interface I { function f(); }
/// echo "before\n";
/// $o = new class implements I {};
/// ```
/// ```expect exit=255
/// before
///
/// Fatal error: Class I@anonymous must implement 1 abstract method (I::f) in %s on line 4
/// ```
pub(crate) fn declare_anon_class<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let slot = CacheSlot(op.extended_value);
    let class = if let Cached::Class(class) = req.cached(frame, slot) {
        class
    } else {
        let serving = exec::standing(req, frame)?;
        let key = constant_bytes(code, op, op.op1)?;
        match req.declare_anonymous(&serving.unit, &key) {
            Ok(class) => {
                req.prime(frame, slot, Cached::Class(class));
                class
            }
            Err(thrown) => {
                return threw(req, frame, &thrown);
            }
        }
    };
    hold_class(req, frame, code, op.out, class);
    Ok(advance(req, frame))
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
    let serving = exec::standing(req, frame)?;
    let Some(at) = code.dynamic_func_defs.get(op.op2 as usize).copied() else {
        return Err(Fatal::error(
            "InternalError: a DECLARE_FUNCTION whose op2 names no dynamic function definition",
        ));
    };
    let lowered = constant_bytes(code, op, op.op1)?;
    match req.bind_function(&serving.unit, at, &lowered) {
        Ok(()) => Ok(advance(req, frame)),
        Err(thrown) => threw(req, frame, &thrown),
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
    let written = constant_bytes(code, op, op.op1)?;
    // A value the compiler could not fold is its constant expression,
    // evaluated here as `zval_update_constant` evaluates it; the value is
    // the constant's, one owner.
    let value = match exec::default_value(req, code, ConstIdx(op.op2)) {
        Ok((value, _)) => value,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    // The value's one owner is the sink's, which warns and gives it up
    // where the name stands; the sink keys it as `zend_register_constant`
    // does.
    let value = crate::api::values::Owner::adopt(value);
    req.define_constant(&written, value);
    Ok(advance(req, frame))
}

/// `ZEND_DECLARE_ATTRIBUTED_CONST` (`Z/Zend/zend_vm_def.h:8301`), the
/// opline `zend_compile_const_decl` leaves for a `const` written with
/// attributes: [`declare_const`]'s registration, then
/// `zend_constant_add_attributes` over the attributes of the
/// declaration the `OP_DATA` after it names, which raises
/// `CONST_DEPRECATED` from a `#[\Deprecated]` among them; both oplines
/// stepped over (`ZEND_VM_NEXT_OPCODE_EX(1, 2)`). A name that stands
/// already warns as `DECLARE_CONST` does and keeps what it had, its
/// attributes too.
///
/// ```php
/// #[\MyAttribute]
/// const MY_CONST = "Has attributes (1)";
/// #[\MyOtherAttribute]
/// const MY_CONST = "Has attributes (2)";
/// echo MY_CONST, "\n";
/// echo (new ReflectionConstant('MY_CONST'))->getAttributes()[0]->getName(), "\n";
/// ```
/// ```expect
/// Warning: Constant MY_CONST already defined, this will be an error in PHP 9 in %s on line 5
/// Has attributes (1)
/// MyAttribute
/// ```
///
/// ```php
/// #[Deprecated]
/// const OLD = 1;
/// var_dump((new ReflectionConstant('OLD'))->isDeprecated());
/// echo OLD, "\n";
/// ```
/// ```expect
/// bool(true)
///
/// Deprecated: Constant OLD is deprecated in %s on line 5
/// 1
/// ```
///
/// A value the compiler could not fold is evaluated as the constant is
/// declared:
///
/// ```php
/// define('A', 1);
/// #[MyAttribute]
/// const B = A + 1;
/// var_dump(B, (new ReflectionConstant('B'))->getValue());
/// ```
/// ```expect
/// int(2)
/// int(2)
/// ```
pub(crate) fn declare_attributed_const<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let at = req.vm().opline(frame);
    let data = exec::opline_at(code, at.saturating_add(1))
        .filter(|next| next.opcode == Opcode::OpData && next.op1_type == OperandKind::Const);
    let (Some(written), Some(data)) = (
        exec::constant(code, ConstIdx(op.op1))
            .as_str_bytes()
            .map(<[u8]>::to_vec),
        data,
    ) else {
        return Err(Fatal::error(
            "InternalError: a DECLARE_ATTRIBUTED_CONST with no name or no OP_DATA after it",
        ));
    };
    let serving = exec::standing(req, frame)?;
    let Some(decl) = code
        .literals
        .get(data.op1 as usize)
        .and_then(|literal| literal.as_ptr())
        .and_then(|index| serving.unit.constants.get(index as usize))
    else {
        return Err(Fatal::error(
            "InternalError: a DECLARE_ATTRIBUTED_CONST whose OP_DATA names no declaration",
        ));
    };
    let attributes = decl.attributes.clone();
    // A value the compiler could not fold is its constant expression,
    // evaluated here as `zval_update_constant` evaluates it.
    let value = match exec::default_value(req, code, ConstIdx(op.op2)) {
        Ok((value, _)) => value,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let value = crate::api::values::Owner::adopt(value);
    req.define_attributed_constant(&written, value, attributes);
    Ok(advance_over_data(req, frame, at))
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
/// ([`ClosureInfo::bound`]), one slot per entry of the op array's
/// `bound_vars` at its starting value, as `zend_create_closure_ex`
/// gives every closure a `zend_array_dup` of the op array's.
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
    let serving = exec::standing(req, frame)?;
    let Some(at) = code.dynamic_func_defs.get(op.op2 as usize).copied() else {
        return Err(Fatal::error(
            "InternalError: a DECLARE_LAMBDA_FUNCTION whose op2 names no dynamic function definition",
        ));
    };
    let Some((_, body)) = serving.unit.dynamic.get(at as usize) else {
        return Err(Fatal::error(
            "InternalError: a DECLARE_LAMBDA_FUNCTION naming a definition the unit has not",
        ));
    };
    let no_receiver = body.no_receiver;
    let bound = match exec::starting_statics(req, body) {
        Ok(bound) => bound,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let Some(function) = req.closure_function(&serving.unit, at) else {
        return Err(Fatal::error(
            "InternalError: a DECLARE_LAMBDA_FUNCTION naming a definition the unit has not",
        ));
    };
    let scope = req.frame_scope(frame);
    let receiver = req.vm().this(frame);
    let this = receiver.filter(|_| !no_receiver && scope.is_some());
    // `ZEND_DECLARE_LAMBDA_FUNCTION`: the class of `EX(This)` where it
    // is an object, else `Z_CE(EX(This))`, the frame's called scope.
    let called_scope = receiver
        .map(|object| object.class())
        .or_else(|| req.vm().at(frame).and_then(exec::Header::called_scope));
    let closure = ClosureInfo {
        function,
        this,
        scope,
        called_scope,
        bound,
        fake: false,
    };
    let Some(class) = req.class_named(Bare::resolved(b"closure")) else {
        return Err(Fatal::error(
            "InternalError: a DECLARE_LAMBDA_FUNCTION with no Closure class declared",
        ));
    };
    // `ZVAL_OBJ_COPY(&closure->this_ptr, ...)`: the receiver the
    // closure holds is one more owner, given back by
    // `zend_closure_free_storage`.
    if let Some(this) = this {
        req.addref(&this.as_value());
    }
    // `zend_create_closure_ex` over the op array: its cache, laid down
    // for the scope here where no closure laid it down yet.
    let cache = match crate::classes::closure_cache(
        req,
        function,
        scope,
        crate::classes::ClosureCache::Function,
    ) {
        Ok(cache) => cache,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let payload = crate::classes::ClosureObject {
        info: closure,
        cache,
    };
    let made = match req.new_object_with(class, payload) {
        Ok(made) => made,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    set_out(req, frame, code, op, made.as_value());
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
        return Err(Fatal::error(
            "InternalError: a BIND_LEXICAL over what is no Closure",
        ));
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
    let place = object
        .closure(req)
        .and_then(|closure| closure.bound)
        .zip(u32::try_from(slot).ok())
        .and_then(|(table, slot)| table.at_position(slot));
    let Some((previous, place)) = place else {
        req.release(bound);
        return Err(Fatal::error(
            "InternalError: a BIND_LEXICAL naming a slot the closure has not",
        ));
    };
    place.heap().write(bound.to_zval());
    req.release(previous);
    Ok(advance(req, frame))
}

/// `ZEND_BIND_STATIC` (`Z/Zend/zend_vm_def.h:9133`): the slot
/// `extended_value` names of the frame's `static` table read into the
/// CV in `op1`. The table is the closure's own for a closure's frame and
/// otherwise the request's copy for the function or the main op array
/// ([`Request::statics_of`]), laid down from the unit's starting values
/// at the first touch, so the unit is never written. By value, a
/// closure's `use` variable, the slot's value is copied, one more owner;
/// by reference the CV becomes the cell the slot holds, made over the
/// slot's value where it holds none yet, or over `op2`'s where a
/// `static`'s initializer ran, whose value is freed after.
///
/// A `static` keeps its value from one call to the next, in a function,
/// a method and a closure alike:
///
/// ```php
/// function f() { static $n = 0, $seen = []; $n++; $seen[] = $n; return count($seen); }
/// class C { function m() { static $m = 10; return ++$m; } }
/// $g = function () { static $k = 0; return ++$k; };
/// $c = new C;
/// echo f(), f(), f(), ' ', $c->m(), ' ', $c->m(), ' ', $g(), $g(), "\n";
/// ```
/// ```expect
/// 123 11 12 12
/// ```
///
/// An initializer that folds to no constant runs once, at the first
/// call, and the value it computed is the one every later call binds:
///
/// ```php
/// function h($x) { static $o = new stdClass, $v = $x * 2; $o->n = ($o->n ?? 0) + 1; return [$o->n, $v]; }
/// echo json_encode([h(1), h(5), h(9)]), "\n";
/// ```
/// ```expect
/// [[1,2],[2,2],[3,2]]
/// ```
///
/// A method a trait gave two classes is a function of each, with a
/// table of its own, where one a class inherits is its parent's, table
/// and all:
///
/// ```php
/// trait T { function n() { static $n = 0; return ++$n; } }
/// class A { use T; }
/// class B { use T; }
/// class P { function m() { static $m = 0; return ++$m; } }
/// class Q extends P {}
/// $a = new A; $b = new B;
/// echo $a->n(), $a->n(), $b->n(), ' ', (new P)->m(), (new Q)->m(), "\n";
/// ```
/// ```expect
/// 121 12
/// ```
///
/// Each closure object has its table, so two closures of one declaration
/// count apart:
///
/// ```php
/// function mk() { return function () { static $k = 0; return ++$k; }; }
/// $x = mk(); $y = mk();
/// echo $x(), $x(), $y(), "\n";
/// ```
/// ```expect
/// 121
/// ```
///
/// The closure's `use` variables are the first entries of the same
/// table, read by value in the prologue:
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
///
/// A string initializer is the unit's interned string, as opcache
/// persists `op_array->static_variables` (design §5.2):
///
/// ```php
/// function sv() { static $x = 'static var'; debug_zval_dump($x); } sv();
/// ```
/// ```expect regime=immutable-literals
/// string(10) "static var" interned
/// ```
pub(crate) fn bind_static<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let slot = (op.extended_value >> 3) as usize;
    let by_ref = op.extended_value & BIND_REF != 0;
    let at = req.statics_of(frame, code);
    let held = match req.static_at(at, code, slot) {
        Ok(Some(held)) => held,
        Ok(None) => {
            return Err(Fatal::error(
                "InternalError: a BIND_STATIC naming a slot the static table has not",
            ));
        }
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let bound = if by_ref {
        let cell = if let Some(cell) = held.as_php_ref() {
            *cell
        } else {
            // `ZVAL_COPY(&ref->val, GET_OP2_ZVAL_PTR_DEREF(BP_VAR_R))`
            // where an initializer ran, the slot's own value otherwise.
            let value = if op.op2_type == OperandKind::Unused {
                held.undef_as_null()
            } else {
                let value = exec::operand(req, frame, code, op.op2_type, op.op2).deref();
                req.addref(&value);
                value
            };
            let cell = match req.reference(value) {
                Ok(cell) => cell,
                Err(thrown) => return threw(req, frame, &thrown),
            };
            // The owner the table's slot had is the cell's now.
            let _moved = req.set_static(at, slot, cell.as_value());
            cell
        };
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
        let bound = cell.as_value();
        req.addref(&bound);
        bound
    } else {
        req.addref(&held);
        held
    };
    req.replace_slot(frame, op.op1, bound);
    Ok(advance(req, frame))
}

/// `ZEND_BIND_INIT_STATIC_OR_JMP` (`Z/Zend/zend_vm_def.h:9186`): the
/// guard before a `static`'s initializer that folds to no constant. The
/// slot `extended_value` names still holds the `null` the table starts
/// it at until the `BIND_STATIC` after the initializer binds it, so a
/// `null` runs the initializer; a reference is what an earlier call
/// computed, which the CV in `op1` is bound to, and the initializer is
/// jumped over to `op2`.
///
/// ```php
/// function next_id() { echo "init\n"; return 7; }
/// function f() { static $id = next_id(); return $id++; }
/// echo f(), f(), f(), "\n";
/// ```
/// ```expect
/// init
/// 789
/// ```
///
/// An initializer that throws binds nothing, so the next call runs it
/// again:
///
/// ```php
/// function pick($fail) { if ($fail) { throw new Exception("no"); } return 5; }
/// function g($fail) { static $v = pick($fail); return $v; }
/// try { g(true); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// echo g(false), "\n";
/// ```
/// ```expect
/// no
/// 5
/// ```
pub(crate) fn bind_init_static_or_jmp<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let slot = (op.extended_value >> 3) as usize;
    let at = req.statics_of(frame, code);
    let held = match req.static_at(at, code, slot) {
        Ok(held) => held,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let Some(cell) = held.and_then(|held| held.as_php_ref().copied()) else {
        return Ok(advance(req, frame));
    };
    let bound = cell.as_value();
    req.addref(&bound);
    req.replace_slot(frame, op.op1, bound);
    jump_to(op.op2)
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
    let Some(name) = name.as_php_string().copied() else {
        return Err(Fatal::error(
            "InternalError: ZEND_BIND_GLOBAL over a name that is no string",
        ));
    };
    let cell = match req.global_cell(name) {
        Ok(cell) => cell,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    req.replace_slot(frame, op.op1, cell.as_value());
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
/// An object's method is found through its class's `get_method`, which
/// may hand the call to another object, as a dual iterator hands an
/// unknown method to its inner iterator; the call and
/// `zend_is_callable_ex` both reach it there.
///
/// ```php
/// class Inner extends ArrayIterator { function hello() { return get_class($this) . " says hello"; } }
/// $outer = new IteratorIterator(new Inner([1]));
/// $f = [$outer, 'hello'];
/// try { echo $f(), "\n"; } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// try { echo call_user_func($f), "\n"; } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Inner says hello
/// Inner says hello
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
    at: u32,
) -> Edge {
    if let Some(edge) = inline_init_closure(req, frame, code, op, at) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    let callee = exec::operand(req, frame, code, op.op2_type, op.op2).deref();
    let resolved = if let Some(object) = callee.as_object().copied() {
        dynamic_call_object(req, object)
    } else if let Some(name) = callee.as_str_bytes() {
        dynamic_call_string(req, name, req.vm().this(frame))
    } else if let Some(array) = callee.as_array().copied() {
        dynamic_call_array(req, array, req.vm().this(frame))
    } else {
        Err(crate::api::raising::throw_error(
            req,
            "Error",
            &format!("Value of type {} is not callable", callee.type_name()),
        ))
    };
    push_dynamic(req, frame, code, op, resolved)
}

/// The inline path of `INIT_DYNAMIC_CALL` (`Z/Zend/zend_vm_def.h:3938`)
/// over a CV that holds a `Closure`, `$f(...)`:
/// `zend_init_dynamic_call_object`'s first arm, the payload read once
/// ([`closure_target`]) and the call pushed with the closure and its
/// receiver held, as php reads `get_closure` and `fbc` directly. Nothing
/// here raises but the memory limit on a page the push takes, reported
/// at this opline; `None`, with nothing touched, for every other
/// callee, which the handler resolves. A closure bound to a receiver
/// reads it and its called scope, one bound into another scope reads
/// that scope's members, and each call gives back the owners it took:
///
/// ```php
/// class P { public $n = 'p'; function get() { return function ($s) { return $this->n . $s . static::class; }; } }
/// class Q extends P { public $n = 'q'; }
/// $f = (new Q)->get();
/// $g = Closure::bind(function () { return $this->n; }, new P, P::class);
/// $r = $f("-") . " " . $g();
/// $m = memory_get_usage();
/// for ($i = 0; $i < 3; $i++) { $r = $f('-') . ' ' . $g(); }
/// echo $r, ' ', memory_get_usage() - $m, "\n";
/// unset($f);
/// var_dump($g());
/// ```
/// ```expect
/// q-Q p 32
/// string(1) "p"
/// ```
#[inline(always)] // as `inline_init_fcall`
#[allow(clippy::inline_always)]
fn inline_init_closure(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Option<Edge> {
    if op.op2_type != OperandKind::Cv {
        return None;
    }
    let held = ByKind(op.op2_type).read(req.slots_mut(frame), code, op.op2);
    let target = closure_target(req, PhpObject::of(held.as_obj()?))?;
    Some(
        match req.push_call(dynamic_header(&target, op.extended_value)) {
            Ok(_) => Ok(advanced()),
            Err(fatal) => {
                req.vm_mut().stand(frame, at);
                Err(fatal)
            }
        },
    )
}

/// What a dynamic call resolved to: the callee, the name the call frame
/// carries for a method, the receiver it holds one owner of, the called
/// scope, and the closure the frame holds one owner of, with whether it
/// reads a runtime cache of its own ([`crate::classes::ClosureCache`]).
struct DynamicTarget {
    function: Function,
    name: Option<PhpString>,
    this: Option<PhpObject>,
    called_scope: Option<crate::api::values::ClassId>,
    closure: Option<(PhpObject, bool)>,
}

/// `zend_init_dynamic_call_object` (`Z/Zend/zend_execute.c:5152`): the
/// object's `get_closure`, a `Closure`'s payload with its receiver, its
/// scope and the closure itself held by the call, or `__invoke` on the
/// object where its class declares one, and php's `Error` naming the
/// class otherwise.
fn dynamic_call_object(req: &mut Request, object: PhpObject) -> Result<DynamicTarget, Thrown> {
    if let Some(target) = closure_target(req, object) {
        return Ok(target);
    }
    let class = object.class();
    if let Some(made) = req.handler_closure(object) {
        if let Some(this) = made.this {
            req.addref(&this.as_value());
        }
        return Ok(DynamicTarget {
            function: made.function,
            name: None,
            this: made.this,
            called_scope: made.called_scope,
            closure: None,
        });
    }
    let Some(function) = req.method_named(class, b"__invoke") else {
        let shown = req.class_name(class);
        return Err(crate::api::raising::throw_error(
            req,
            "Error",
            &format!("Object of type {shown} is not callable"),
        ));
    };
    let name = req.method_name_string(function)?;
    let this = if req.takes_this(function) {
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

/// `zend_closure_get_closure` with `GC_ADDREF` of the closure and its
/// receiver: what a call through `object`, a `Closure`, takes, the
/// payload read once for the function, the receiver, the called scope
/// and whether it reads a runtime cache of its own, as php's
/// `get_closure` hands `fbc` and the object over in one call. None for
/// an object that is no closure, with nothing touched.
fn closure_target(req: &Request, object: PhpObject) -> Option<DynamicTarget> {
    let closure = object.native::<crate::classes::ClosureObject>(req)?;
    let (function, this, called_scope) = (
        closure.info.function,
        closure.info.this,
        closure.info.called_scope,
    );
    let own_cache = matches!(closure.cache, crate::classes::ClosureCache::Own(_));
    req.addref(&object.as_value());
    if let Some(this) = this {
        req.addref(&this.as_value());
    }
    Some(DynamicTarget {
        function,
        name: None,
        this,
        called_scope,
        closure: Some((object, own_cache)),
    })
}

/// `zend_init_dynamic_call_string` (`Z/Zend/zend_execute.c:5072`): a
/// `Class::method` string is the class by name, `Class "X" not found`
/// where nothing declares it, and the method as
/// `zend_std_get_static_method` finds it, with its access rule and
/// php's refusal of an instance method named on the class; any other
/// string is the function of that name with a leading `\` skipped, and
/// the `Error` naming it as written where there is none. The operand
/// holds the string while the function is found, which reaches no PHP
/// code; a class's autoloaders are PHP code that may free it, so the
/// class's name is copied.
fn dynamic_call_string(
    req: &mut Request,
    name: &[u8],
    this: Option<PhpObject>,
) -> Result<DynamicTarget, Thrown> {
    if let Some(at) = crate::api::request::compound_split(name) {
        let name = name.to_vec();
        let class = dynamic_call_class(req, &name[..at])?;
        // `zend_string_init` of the part past `::`, the name the call
        // carries, as php's `zend_init_dynamic_call_string` makes it.
        let method = req.string(name.get(at + 2..).unwrap_or_default())?;
        return dynamic_call_static(req, class, method, this);
    }
    let Some(function) = exec::find_function(req, Bare::of(name)) else {
        return Err(crate::api::raising::undefined_function(req, name));
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
/// finds it, an object's as [`method_on`] finds it, through its
/// `get_method` and the receiver that may hand the call to, with the
/// access rule `zend_std_get_method` holds it to, the receiver then the
/// called scope; a `static` method on an object called with no receiver.
///
/// The method name the call carries is the member's own string with one
/// more owner, and the class name is held across the autoloader it may
/// serve, so a call by a pair leaves the heap where it stood, as a call
/// by `Class::method` does with the name it cuts:
///
/// ```php
/// class C { function m() { return 1; } static function s() { return 2; } }
/// spl_autoload_register(function ($c) { eval("class $c { static function t() { return 3; } }"); });
/// $o = new C; $n = str_repeat("m", 1); $c = str_repeat("C", 1); $d = str_repeat("D", 1);
/// $r = [$o, $n]() + [$c, "s"]() + "C::s"() + [$d, "t"]();
/// $b = memory_get_usage();
/// for ($i = 0; $i < 3; $i++) { $r += [$o, $n]() + [$c, "s"]() + "C::s"() + [$d, "t"](); }
/// var_dump($r, memory_get_usage() - $b);
/// ```
/// ```expect
/// int(32)
/// int(0)
/// ```
fn dynamic_call_array(
    req: &mut Request,
    array: PhpArray,
    this: Option<PhpObject>,
) -> Result<DynamicTarget, Thrown> {
    let refuse =
        |req: &mut Request, text: &str| crate::api::raising::throw_error(req, "Error", text);
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
    let Some(method) = second.as_php_string().copied() else {
        return Err(refuse(req, "Second array member is not a valid method"));
    };
    // The members' strings are the call's from here, one owner each,
    // as php's `zend_init_dynamic_call_array` holds them: the class name
    // across the autoloader it may serve, and the method name as the
    // name the call carries, never a copy of either.
    req.addref(&second);
    if let Some(class) = first.as_php_string().copied() {
        req.addref(&first);
        let class = dynamic_call_class(req, class.as_bytes());
        req.release(first);
        return match class {
            Ok(class) => dynamic_call_static(req, class, method, this),
            Err(thrown) => {
                req.release(second);
                Err(thrown)
            }
        };
    }
    let Some(object) = first.as_object().copied() else {
        req.release(second);
        return Err(refuse(
            req,
            "First array member is not a valid class name or object",
        ));
    };
    let found = method_on(
        req,
        object,
        &exec::Lowered::of(method.as_bytes()),
        &second,
        Undefined::OnHanded,
    );
    let (function, receiver) = match found {
        Ok(found) => found,
        Err(thrown) => {
            req.release(second);
            return Err(thrown);
        }
    };
    let object = receiver.unwrap_or(object);
    let this = if req.takes_this(function) {
        req.addref(&object.as_value());
        Some(object)
    } else {
        None
    };
    Ok(DynamicTarget {
        function,
        name: Some(method),
        this,
        called_scope: Some(object.class()),
        closure: None,
    })
}

/// `zend_fetch_class_by_name` under `ZEND_FETCH_CLASS_EXCEPTION`: the
/// class of that name, a leading `\` skipped, the autoloaders asked,
/// what they throw handed on, and php's `Error` where nothing declares
/// it.
///
/// ```php
/// spl_autoload_register(function ($name) { throw new Exception("loading $name"); });
/// try { $a = ["Nope", "f"]; $a(); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// loading Nope
/// ```
fn dynamic_call_class(
    req: &mut Request,
    name: &[u8],
) -> Result<crate::api::values::ClassId, Thrown> {
    match crate::api::call::autoload(req, name)? {
        Some(class) => Ok(class),
        None => Err(crate::api::raising::class_not_found(req, name)),
    }
}

/// `zend_std_get_static_method` over `class` and then the dynamic
/// call's refusal of an instance method: `Call to undefined method` and
/// `zend_bad_method_call` as `INIT_STATIC_METHOD_CALL` raises them, and
/// `zend_non_static_method_call`.
fn dynamic_call_static(
    req: &mut Request,
    class: crate::api::values::ClassId,
    method: PhpString,
    this: Option<PhpObject>,
) -> Result<DynamicTarget, Thrown> {
    let written = Value::string(method);
    let found = static_method(
        req,
        class,
        &exec::Lowered::of(method.as_bytes()),
        &written,
        this,
    );
    let refused = match found {
        Ok(callee) if callee.takes_this => Err(non_static_method_call(req, callee.function)),
        Ok(callee) => Ok(callee.function),
        Err(thrown) => Err(thrown),
    };
    let function = match refused {
        Ok(function) => function,
        Err(thrown) => {
            req.release(written);
            return Err(thrown);
        }
    };
    Ok(DynamicTarget {
        function,
        name: Some(method),
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
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// `ZEND_INIT_FCALL` (`Z/Zend/zend_vm_def.h:4057`): the call frame for a
/// callee named at compile time, its lowered name in `op2` and the
/// number of arguments in `extended_value`. The callee is read off the
/// cache slot `result.num` names and, on the first pass, looked up and
/// written there (`CACHED_PTR`, `CACHE_PTR`), as the two `INIT_*_BY_NAME`
/// read and write theirs. A name nothing is declared under is php's
/// `Error`, over the same constant.
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn init_fcall<T: Tier, A: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_init_fcall(req, frame, op, at) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    let name = ConstIdx(op.op2);
    init_call(req, frame, code, op, &[name], name)
}

/// The inline path of `ZEND_INIT_FCALL` (`ZEND_INIT_FCALL_SPEC_CONST`,
/// `Z/Zend/zend_vm_def.h:4057`): the callee read off the cache slot
/// `result.num` names, `CACHED_PTR`, and where it is a user function,
/// the call pushed with the room its frame shape and the arguments past
/// its parameters take (`_zend_vm_stack_push_call_frame_ex` over the
/// `used_stack` php's compiler wrote into `op1`). Nothing here raises
/// but the memory limit on a page the push takes, which is reported at
/// this opline. `None`, with nothing touched, for a slot not primed yet
/// and a callee that is a native, a trampoline or anything else
/// [`Request::call_room`] sizes. `INIT_FCALL_BY_NAME` and
/// `INIT_NS_FCALL_BY_NAME` read their callee off the same slot once a
/// first pass primed it, and take it too.
#[inline(always)] // as `inline_send`
#[allow(clippy::inline_always)]
fn inline_init_fcall(req: &mut Request, frame: FrameRef, op: &Opline, at: u32) -> Option<Edge> {
    let Cached::Function(function) = req.cached(frame, CacheSlot(op.out)) else {
        return None;
    };
    let user = req.user_function(function)?;
    let room = user.shape.room(op.extended_value);
    let mut header = exec::Header::call(function, op.extended_value);
    header.callee = user.callee_at();
    header.info.join(user.shape.sent);
    Some(match req.push_room(room as usize, header) {
        Ok(_) => Ok(advanced()),
        Err(fatal) => {
            req.vm_mut().stand(frame, at);
            Err(fatal)
        }
    })
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn init_fcall_by_name<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_init_fcall(req, frame, op, at) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
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
    let slot = CacheSlot(op.out);
    let function = if let Cached::Function(function) = req.cached(frame, slot) {
        Some(function)
    } else {
        let named = exec::constant(code, named);
        let found = lookups.iter().find_map(|&at| {
            let key = exec::constant(code, at);
            let key = key
                .as_str_bytes()
                .or(named.as_str_bytes())
                .unwrap_or_default();
            exec::find_function(req, Bare::resolved(key))
        });
        if let Some(function) = found {
            req.prime(frame, slot, Cached::Function(function));
        }
        found
    };
    let Some(function) = function else {
        // The name as the constant spells it, `zval_get_string`.
        let named = exec::constant(code, named);
        let name = match crate::ops::to_string(req, &named) {
            Ok(name) => name,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        let thrown = crate::api::raising::undefined_function(req, name.as_bytes());
        req.release(Value::string(name));
        return threw(req, frame, &thrown);
    };
    pushed(req, frame, exec::Header::call(function, op.extended_value))
}

/// `zend_vm_stack_push_call_frame` for an `INIT_*`: the call `header`
/// lays down pushed with its callee's room, and the opline advanced.
fn pushed(req: &mut Request, frame: FrameRef, header: exec::Header) -> Edge {
    req.push_call(header)?;
    Ok(advance(req, frame))
}

/// `ZEND_SEND_VAL`: an argument that is a constant or a temporary, placed
/// in the call frame being built, at its number or, where `op2` is the
/// name the compiler could not place, where `zend_handle_named_arg`
/// places it ([`reserve_arg`]). A name the compiler placed is no name
/// at all: an argument written `name:` in the order the callee declares
/// its parameters is sent at its number, and one after a skipped
/// parameter is sent by name, the skipped one filled by the
/// `CHECK_UNDEF_ARGS` after the last argument.
///
/// ```php
/// var_dump(str_pad('abc', length: 6, pad_type: STR_PAD_LEFT));
/// var_dump(str_pad(string: 'abc', length: 5, pad_string: '-'));
/// ```
/// ```expect
/// string(6) "   abc"
/// string(5) "abc--"
/// ```
///
/// A name the callee declares no parameter for is php's `Error`, and so
/// is a name whose parameter an argument already filled; the value is
/// given up and nothing is placed.
///
/// ```php
/// try { str_pad('abc', 6, nope: 1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { str_pad('abc', 6, string: 'x'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Unknown named parameter $nope
/// Named parameter $string overwrites previous argument
/// ```
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn send_val<T: Tier, A: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_send(req, frame, code, op, A::default(), Sending::Value) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    sent(req, frame, code, op)
}

/// `ZEND_SEND_VAR`: an argument that is a variable or what a call
/// returned, sent by value: the compiler emits it for a callee the
/// table has whose parameter at this number takes its argument by
/// value, and [`send_ref`] where the parameter takes one by reference,
/// so a reference the operand holds is followed here and never passed
/// on (`ZVAL_COPY_DEREF`). By name as [`send_val`] sends one.
///
/// ```php
/// $n = 6; $s = 'abc';
/// var_dump(str_pad($s, pad_type: STR_PAD_BOTH, length: $n));
/// ```
/// ```expect
/// string(6) " abc  "
/// ```
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn send_var<T: Tier, A: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_send(req, frame, code, op, A::default(), Sending::Value) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    sent(req, frame, code, op)
}

/// `SEND_VAL` and `SEND_VAR` past their inline path: the slot
/// [`reserve_arg`] gives the argument, by number or by name, and the
/// operand placed there ([`send`]). A call of its own, so that the
/// inline path before it keeps to the registers it needs.
#[inline(never)]
fn sent(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let reserved = match reserve_arg(req, frame, code, op) {
        Ok(reserved) => reserved,
        Err(edge) => return edge,
    };
    send(req, frame, code, op, reserved.slot)
}

/// The inline path of `ZEND_SEND_VAL` and `ZEND_SEND_VAR` by number
/// (`ZEND_SEND_VAL_SPEC_CONST_CONST`, `_TMP_CONST`,
/// `ZEND_SEND_VAR_SPEC_CV_CONST`, `Z/Zend/zend_vm_def.h:4886-4983`):
/// `ZEND_CALL_VAR(EX(call), opline->result.var)` is the callee's slot
/// for the argument's number, and the operand goes into it as php's
/// handler puts it, a constant that counts no owner copied
/// (`ZVAL_COPY_VALUE`), a temporary moved, its slot left `UNDEF` for
/// the frame's release to pass over, and a CV copied with one more
/// owner (`ZVAL_COPY`). `None`, with nothing touched, for an argument
/// by name, a number past what the `INIT` said, a slot an argument
/// stands in already, a refcounted constant, a reference, whose value
/// is what is sent (`ZVAL_COPY_DEREF`), and a CV nothing wrote, whose
/// warning is [`send`]'s. `SEND_VAL_EX` and `SEND_VAR_EX` take it too
/// where the callee's parameter at the number takes its argument by
/// value, as php's `_EX` handlers go to the same arm once
/// `ARG_SHOULD_BE_SENT_BY_REF` is clear, which `ex` asks of the callee
/// ([`exec::CallInfo::sends_by_value`], then [`Request::arg_send_mode`]).
///
/// ```php
/// function f($a, $b, $c) { return "$a|$b|$c"; }
/// $s = str_repeat("s", 2);
/// echo f($s, str_repeat("t", 2), 'c'), "\n";
/// $r = &$s; $s .= "!";
/// echo f($r, $s, [1][0]), "\n";
/// echo f($undef, 1.5, null), "\n";
/// debug_zval_dump($s);
/// ```
/// ```expect
/// ss|tt|c
/// ss!|ss!|1
///
/// Warning: Undefined variable $undef in %s on line 7
/// |1.5|
/// string(3) "ss!" refcount(2)
/// ```
#[inline(always)] // the handler's fast half, as php's is in the handler
#[allow(clippy::inline_always)]
fn inline_send(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    a: impl Operand,
    sending: Sending,
) -> Option<Edge> {
    if op.op2_type == OperandKind::Const {
        return None;
    }
    let to = (op.op2 as usize).checked_sub(1)?;
    let (call, callee) = req.vm().stack.top()?;
    let header = callee.header();
    if !header.info.building()
        || to >= header.num_args as usize
        || !callee.slots().get(to)?.is_undef()
        || match sending {
            Sending::Value => false,
            Sending::Declared => {
                !header.info.sends_by_value(op.op2) && req.arg_send_mode(header.function?, op.op2).0
            }
            Sending::Checked => header.info.send_by_ref(),
        }
    {
        return None;
    }
    let slots = req.slots_mut(frame);
    let z = a.read(slots, code, op.op1);
    match a.kind() {
        OperandKind::Const if crate::operand::owns_nothing(z) => {}
        OperandKind::Tmp | OperandKind::Var if crate::operand::movable(z) => {
            *slots.get_mut(a.slot(code, op.op1)?)? = Zval::UNDEF;
        }
        OperandKind::Cv if crate::operand::movable(z) => z.addref(),
        _ => return None,
    }
    let placed = req
        .vm_mut()
        .stack
        .frame_mut(call)
        .and_then(|callee| callee.slots_mut().get_mut(to));
    match placed {
        Some(placed) => *placed = z,
        // The callee's slot was read above and the frame is the same
        // one, so this arm is unreachable; what was taken is given back.
        None => req.release(Value::of_zval(z)),
    }
    Some(Ok(advanced()))
}

/// Who says an argument [`inline_send`] places goes by value: the
/// compiler, which emitted `SEND_VAL` or `SEND_VAR`; the callee's
/// parameter, which a `SEND_*_EX` reads
/// (`ARG_SHOULD_BE_SENT_BY_REF`); or the `CHECK_FUNC_ARG` before a
/// `SEND_FUNC_ARG`, which read it for the call
/// (`ZEND_CALL_SEND_ARG_BY_REF`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sending {
    Value,
    Declared,
    Checked,
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
///
/// A dimension, a property and a class's property are fetched for
/// writing first, and the place the fetch left is what is bound; by
/// name, the parameter the name gives is the one bound.
///
/// ```php
/// class C { public $p = [3, 1, 2]; public static $s = [9, 8]; }
/// $a = ['k' => [2, 1]]; $o = new C;
/// sort($a['k']); sort($o->p); sort(C::$s);
/// function f(&$x = null, &$y = null) { $y = 'y'; }
/// f(y: $b);
/// var_dump($a['k'][0], $o->p[0], C::$s[0], $b);
/// ```
/// ```expect
/// int(1)
/// int(1)
/// int(8)
/// string(1) "y"
/// ```
pub(crate) fn send_ref<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let reserved = match reserve_arg(req, frame, code, op) {
        Ok(reserved) => reserved,
        Err(edge) => return edge,
    };
    send_by_ref(req, frame, code, op, reserved.slot)
}

/// The body of [`send_ref`] once the slot is reserved, which
/// `SEND_VAR_EX` and `SEND_FUNC_ARG` dispatch to where the parameter
/// takes its argument by reference.
fn send_by_ref(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    slot: ArgSlot,
) -> Edge {
    let Some(at) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::error(
            "InternalError: ZEND_SEND_REF over an operand php compiles no write over",
        ));
    };
    let bound = bound_at(req, at);
    // `FREE_OP1_VAR_PTR()`: a `VAR` holding the `IS_INDIRECT` a fetch
    // left owns nothing, and one holding the cell a write fetch fell
    // back to `read_property` for (a `&get` hook's) gives its owner up,
    // the argument holding its own.
    if op.op1_type == OperandKind::Var {
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
    }
    match bound {
        Ok(bound) => place_argument(req, frame, slot, bound),
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// `ZVAL_MAKE_REF_EX(place, 2)`: the cell a place holds, or a new one
/// over what it held, `null` for what nothing wrote, written back into
/// the place, which gives the cell the owner the place had of the
/// value; the cell comes back with one more owner, the caller's.
fn bound_at(req: &mut Request, at: php_heap::ZvalRef) -> Result<Value, Thrown> {
    let bound = req.make_ref(at)?.as_value();
    req.addref(&bound);
    Ok(bound)
}

/// `ZEND_ARG_SEND_MODE` (`Z/Zend/zend_compile.h:1141`) over the callee
/// the `INIT` resolved: whether the parameter numbered `number` takes
/// its argument by reference, and whether only where it can
/// (`ZEND_SEND_PREFER_REF`), the variadic's for every argument past the
/// declared ones. The bit the `INIT` wrote answers the first twelve
/// arguments of a user callee with no lookup, as
/// `QUICK_ARG_SHOULD_BE_SENT_BY_REF` reads `quick_arg_flags`
/// ([`exec::CallInfo::sends_by_value`]); the declaration answers the
/// rest. Below, the twelfth argument is the last the bits answer, the
/// thirteenth goes by value and the fourteenth by reference as the
/// declaration says, a variadic's take every argument past the declared
/// ones, a name takes its parameter's, and a constant where a reference
/// is taken is php's `Error`:
///
/// ```php
/// if (PHP_VERSION_ID > 0) {
///     function r1(&$a) { $a = 'r1'; }
///     function r12($a1, $a2, $a3, $a4, $a5, $a6, $a7, $a8, $a9, $a10, $a11, &$a12, $a13, &$a14) { $a12 = 'r12'; $a14 = 'r14'; }
///     function rv($a, &...$v) { foreach ($v as &$x) { $x = 'v'; } }
///     function rn($a, &$b = null) { $b = 'n'; }
/// }
/// r1($x);
/// r12(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, $y, $w, $z);
/// rv(0, $p, $q);
/// rn(1, b: $n);
/// var_dump($x, $y, $w, $z, $p, $q, $n);
/// try { r1(1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { r12(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: Undefined variable $w in %s on line 9
///
/// Warning: Undefined variable $w in %s on line 12
/// string(2) "r1"
/// string(3) "r12"
/// NULL
/// string(3) "r14"
/// string(1) "v"
/// string(1) "v"
/// string(1) "n"
/// r1(): Argument #1 ($a) could not be passed by reference
/// r12(): Argument #12 ($a12) could not be passed by reference
/// ```
fn send_mode(req: &Request, number: u32) -> (bool, bool) {
    match req.vm().building() {
        Some(call) if !call.info.sends_by_value(number) => {
            call.function.map_or((false, false), |function| {
                req.arg_send_mode(function, number)
            })
        }
        _ => (false, false),
    }
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
/// opline. The parameter is named where it is one of the declared ones
/// (`get_function_arg_name`), and a variadic's is not.
fn cannot_pass_by_reference(req: &mut Request, number: u32) -> Thrown {
    let function = req.building_call_function();
    let name = function.map_or_else(String::new, |function| req.qualified_name_of(function));
    let parameter = crate::api::raising::arg_name_suffix(req, function, number);
    let message =
        format!("{name}(): Argument #{number}{parameter} could not be passed by reference");
    crate::api::raising::throw_error(req, "Error", &message)
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
///
/// By name, the bit is the named parameter's, and a name the callee
/// declares no parameter for is its variadic's or php's `Error`; the
/// extra named arguments a variadic collects follow the positional
/// ones under their names, in the order they were written.
///
/// ```php
/// class C {
///     function m($a, $b = 'b', ...$rest) { var_dump($a, $b, $rest); }
///     function r(&$x) {}
/// }
/// $o = new C;
/// $o->m(b: 2, a: 1);
/// $o->m(1, 2, 3, z: 'z', y: 'y');
/// try { $o->m(1, a: 2); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $o->m(1, z: 1, z: 2); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $o->r(y: 1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $o->r(x: 1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// int(2)
/// array(0) {
/// }
/// int(1)
/// int(2)
/// array(3) {
///   [0]=>
///   int(3)
///   ["z"]=>
///   string(1) "z"
///   ["y"]=>
///   string(1) "y"
/// }
/// Named parameter $a overwrites previous argument
/// Named parameter $z overwrites previous argument
/// Unknown named parameter $y
/// C::r(): Argument #1 ($x) could not be passed by reference
/// ```
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn send_val_ex<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_send(req, frame, code, op, ByKind(op.op1_type), Sending::Declared) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    let reserved = match reserve_arg(req, frame, code, op) {
        Ok(reserved) => reserved,
        Err(edge) => return edge,
    };
    if must_be_sent_by_ref(req, reserved.number) {
        let thrown = cannot_pass_by_reference(req, reserved.number);
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        return threw(req, frame, &thrown);
    }
    send(req, frame, code, op, reserved.slot)
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
///
/// By name, the named parameter's bit decides, and a variadic taking
/// its arguments by reference binds an extra named one too.
///
/// ```php
/// class C {
///     function m($a = 0, &$b = null) { $b = 'bound'; }
///     function v(&...$refs) { foreach ($refs as &$r) { $r = 'v'; } }
/// }
/// $o = new C;
/// $o->m(b: $x);
/// $o->v(k: $y);
/// var_dump($x, $y);
/// ```
/// ```expect
/// string(5) "bound"
/// string(1) "v"
/// ```
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn send_var_ex<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_send(req, frame, code, op, ByKind(op.op1_type), Sending::Declared) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    let reserved = match reserve_arg(req, frame, code, op) {
        Ok(reserved) => reserved,
        Err(edge) => return edge,
    };
    if should_be_sent_by_ref(req, reserved.number) {
        return send_by_ref(req, frame, code, op, reserved.slot);
    }
    send(req, frame, code, op, reserved.slot)
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
    let reserved = match reserve_arg(req, frame, code, op) {
        Ok(reserved) => reserved,
        Err(edge) => return edge,
    };
    let (by_ref, prefer) = send_mode(req, reserved.number);
    if !by_ref {
        return send(req, frame, code, op, reserved.slot);
    }
    send_returned(req, frame, code, op, reserved.slot, prefer)
}

/// `ZEND_SEND_VAR_NO_REF` (`Z/Zend/zend_vm_def.h:4988`): what a call
/// returned, sent where the compiler knew the parameter takes its
/// argument by reference and nothing else: [`send_var_no_ref_ex`]'s
/// by-reference arm, with the bit read at compile time. A call that
/// returned by value has no place for the write to land in, and php's
/// notice says so.
///
/// ```php
/// function g() { return [3, 1, 2]; }
/// var_dump(sort(g()));
/// ```
/// ```expect
/// Notice: Only variables should be passed by reference in %s on line 3
/// bool(true)
/// ```
pub(crate) fn send_var_no_ref<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let reserved = match reserve_arg(req, frame, code, op) {
        Ok(reserved) => reserved,
        Err(edge) => return edge,
    };
    send_returned(req, frame, code, op, reserved.slot, false)
}

/// What a call returned placed for a parameter taking it by reference:
/// as it stands where it is a reference or the parameter takes one only
/// where it can (`ARG_MAY_BE_SENT_BY_REF`), and otherwise wrapped in a
/// cell of its own (`ZVAL_NEW_REF(arg, arg)`) with php's notice.
fn send_returned(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    slot: ArgSlot,
    prefer: bool,
) -> Edge {
    // `GET_OP1_ZVAL_PTR(BP_VAR_R)` over a `VAR`, which follows no
    // reference: the operand's owner is the argument's from here.
    let returned = req.take_slot(frame, exec::temporary_slot(code, op.op1));
    if returned.as_php_ref().is_some() || prefer {
        return place_argument(req, frame, slot, returned);
    }
    let cell = match req.reference(returned) {
        Ok(cell) => cell,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    crate::api::raising::notice(req, "Only variables should be passed by reference");
    place_argument(req, frame, slot, cell.as_value())
}

/// `ZEND_CHECK_FUNC_ARG` (`Z/Zend/zend_vm_def.h:5183`), before a
/// dimension, a property or a class's property sent to a callee the
/// compiler knew nothing of: the parameter's by-reference bit read off
/// the callee the `INIT` resolved, by its number or by its name, and
/// kept on the call being built (`ZEND_CALL_SEND_ARG_BY_REF`) for the
/// `_FUNC_ARG` fetch and the `SEND_FUNC_ARG` after it to serve the
/// argument as a write or as a read by. A name the callee declares no
/// parameter for is sent by value, and the send raises php's `Error`.
///
/// ```php
/// class C {
///     public $p = [3, 1, 2]; public $q = 'q';
///     function byref(&$a) { sort($a); }
///     function byval($a) { return $a; }
/// }
/// $o = new C;
/// $o->byref($o->p);
/// var_dump($o->p, $o->byval($o->q), $o->byval(a: $o->q));
/// try { $o->byval(nope: $o->q); } catch (Error $e) { echo $e->getMessage(), "\n"; }
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
/// string(1) "q"
/// string(1) "q"
/// Unknown named parameter $nope
/// ```
pub(crate) fn check_func_arg<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    // By number, where the `INIT` wrote the parameter's by-value bit, the
    // call being built read once and written once.
    if let ArgAt::Number(number) = ArgAt::of(op)
        && let Some(call) = req.vm_mut().building_mut()
        && call.info.sends_by_value(number)
    {
        call.info.set_send_by_ref(false);
        return Ok(advanced());
    }
    req.vm_mut().stand(frame, at);
    let number = match ArgAt::of(op) {
        ArgAt::Number(number) => Some(number),
        ArgAt::Name(at) => {
            let name = exec::constant(code, at);
            let function = req.building_call_function();
            match (name.as_str_bytes(), function) {
                (Some(name), Some(function)) => {
                    crate::api::call::arg_offset_by_name(req, function, name).number()
                }
                _ => None,
            }
        }
    };
    let by_ref = number.is_some_and(|number| should_be_sent_by_ref(req, number));
    let Some(call) = req
        .building_call()
        .and_then(|call| req.vm_mut().at_mut(call))
    else {
        return Err(Fatal::error(
            "InternalError: ZEND_CHECK_FUNC_ARG with no call frame under it",
        ));
    };
    call.info.set_send_by_ref(by_ref);
    Ok(advance(req, frame))
}

/// `ZEND_SEND_FUNC_ARG` (`Z/Zend/zend_vm_def.h:5215`): what a
/// `_FUNC_ARG` fetch left, sent as the `CHECK_FUNC_ARG` before it read
/// the parameter: the place the write fetch laid down, bound as
/// [`send_ref`] binds a variable, or the value the read fetched, sent
/// as `SEND_VAR` sends one, a reference it holds followed.
///
/// ```php
/// class C { function m(&$a, $b) { $a[] = $b; } }
/// class D { public static $s = []; public $list = ['k' => []]; }
/// $o = new C; $d = new D;
/// $o->m(D::$s, 1);
/// $o->m($d->list['k'], 2);
/// $o->m(b: 3, a: $d->list['k']);
/// var_dump(D::$s, $d->list);
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// array(1) {
///   ["k"]=>
///   array(2) {
///     [0]=>
///     int(2)
///     [1]=>
///     int(3)
///   }
/// }
/// ```
///
/// By value it is `SEND_VAR`'s inline path over what the read left,
/// for a container that is a CV, a constant or what a call returned and
/// a key that is a CV, a constant or a temporary, a string's offset
/// among them; a key the array has not got and an offset past the string
/// warn as the read does, by reference the element is laid down, and
/// `[]` read is php's `Error`:
///
/// ```php
/// if (PHP_VERSION_ID > 0) { function v($x) { return $x; } function r(&$x) { $x = 'w'; } }
/// $a = [1, 2]; $s = 'str'; $j = 1;
/// echo v($a[$j]), v($a[1]), v($a[$j + 0]), v([5, 6][$j]), v([5, 6][1]), v((fn () => [7, 8])()[$j]), v($s[1]), v($s[$j + 1]), v(($a + [])[$j]), "\n";
/// var_dump(v($a[7]), v($s[5]));
/// r($b['k']);
/// r($a[]);
/// var_dump($b, $a);
/// try { v($a[]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 222668tr2
///
/// Warning: Undefined array key 7 in %s on line 5
///
/// Warning: Uninitialized string offset 5 in %s on line 5
/// NULL
/// string(0) ""
/// array(1) {
///   ["k"]=>
///   string(1) "w"
/// }
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   [2]=>
///   string(1) "w"
/// }
/// Cannot use [] for reading
/// ```
pub(crate) fn send_func_arg<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    // By value, `SEND_VAR`'s body (`ZEND_VM_C_GOTO(send_var)`).
    if let Some(edge) = inline_send(req, frame, code, op, ByKind(op.op1_type), Sending::Checked) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    let reserved = match reserve_arg(req, frame, code, op) {
        Ok(reserved) => reserved,
        Err(edge) => return edge,
    };
    if sends_arg_by_ref(req) {
        return send_by_ref(req, frame, code, op, reserved.slot);
    }
    send(req, frame, code, op, reserved.slot)
}

/// `ZEND_CALL_INFO(EX(call)) & ZEND_CALL_SEND_ARG_BY_REF`: what the last
/// `CHECK_FUNC_ARG` read for the call being built.
fn sends_arg_by_ref(req: &Request) -> bool {
    req.vm()
        .building()
        .is_some_and(|call| call.info.send_by_ref())
}

/// `ZEND_CHECK_UNDEF_ARGS` (`Z/Zend/zend_vm_def.h:5656`), after the last
/// argument of a call where a named one may have skipped a parameter:
/// nothing where every slot up to the last one placed is filled, and
/// otherwise `zend_handle_undef_args` over the call
/// ([`crate::api::call::handle_undef_args`]), which fills each skipped
/// parameter with its default or raises php's `ArgumentCountError` for
/// one that has none.
///
/// ```php
/// function f($a = 'a', $b = 'b', $c = 'c') { echo "$a $b $c\n"; }
/// f(c: 'C');
/// f(b: 'B', c: 'C');
/// class K { const D = 'd'; static function m($x = self::D, $y = 0) { echo "$x $y\n"; } }
/// K::m(y: 1);
/// try { str_pad('a', pad_type: 1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// a b C
/// a B C
/// d 1
/// str_pad(): Argument #2 ($length) not passed
/// ```
pub(crate) fn check_undef_args<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    _: &OpArray,
    _: &Opline,
) -> Edge {
    let Some(call) = req.building_call() else {
        return Ok(advance(req, frame));
    };
    let undef = req.frame_at(call).is_some_and(|call| {
        call.arguments()
            .iter()
            .any(|argument| matches!(argument.as_ref(), ValueRef::Undef))
    });
    if undef && let Err(thrown) = crate::api::call::handle_undef_args(req, call) {
        return threw(req, frame, &thrown);
    }
    Ok(advance(req, frame))
}

/// `ZEND_INIT_USER_CALL` (`Z/Zend/zend_vm_def.h:3952`), what php's
/// compiler makes of `call_user_func(...)` and `call_user_func_array(...)`
/// spelled by a name it resolves (`zend_compile_init_user_func`): the
/// callback in `op2` resolved by `zend_is_callable_ex` as a native's
/// `callable` parameter resolves one ([`Request::callable_checked`]),
/// and the call frame pushed for it straight from this frame, marked
/// `ZEND_CALL_DYNAMIC`, with `extended_value` arguments to come. No
/// frame of the native's is pushed, so a trace through the callee names
/// the line of the call and no `[internal function]`, and a callback
/// that is none is the `TypeError` in the native's words, `op1`'s name,
/// raised on this frame.
///
/// ```php
/// function f() { throw new Exception('in f'); }
/// call_user_func('f');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Exception: in f in %s:2
/// Stack trace:
/// #0 %s(3): f()
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// call_user_func('nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: call_user_func(): Argument #1 ($callback) must be a valid callback, function "nope" not found or invalid function name in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// call_user_func_array('str_pad', ['abc', 'nope' => 6]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Unknown named parameter $nope in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// Every shape a callback takes, and a callback the program names:
///
/// ```php
/// class C { public $v = 'v'; function m($x) { return "m$x$this->v "; } static function s($x) { return "s$x "; } function __invoke($x) { return "i$x "; } }
/// $c = new C; $cb = 'strtoupper';
/// echo call_user_func([$c, 'm'], 1), call_user_func(['C', 's'], 2), call_user_func('C::s', 3), call_user_func($c, 4), call_user_func(fn ($x) => "c$x ", 5), call_user_func($cb, "u "), call_user_func_array([$c, 'm'], [6]), "|\n";
/// try { call_user_func([$c, 'nope']); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { call_user_func_array(1, []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// m1v s2 s3 i4 c5 U m6v |
/// call_user_func(): Argument #1 ($callback) must be a valid callback, class C does not have a method "nope"
/// call_user_func_array(): Argument #1 ($callback) must be a valid callback, no array or string given
/// ```
pub(crate) fn init_user_call<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let callback = exec::operand(req, frame, code, op.op2_type, op.op2);
    let resolved = req.callable_checked(&callback);
    let target = match resolved {
        // A deprecation's handler may have thrown with the callable found.
        Ok(_) if req.is_pending() => {
            exec::free_operand(req, frame, code, op.op2_type, op.op2);
            return Ok(Stepped::Threw(frame).into());
        }
        Ok(callable) => user_call_target(req, callable.target()),
        // `zend_type_error`, chained onto what a deprecation's handler
        // or an autoloader left pending, `(null)` where it left the
        // resolution no sentence.
        Err(refusal) => {
            let native = exec::constant(code, ConstIdx(op.op1));
            let native = String::from_utf8_lossy(native.as_str_bytes().unwrap_or_default());
            let message =
                format!("{native}(): Argument #1 ($callback) must be a valid callback, {refusal}");
            Err(refusal.across(req, |req| {
                crate::api::raising::throw_over_pending(req, "TypeError", &message)
            }))
        }
    };
    push_dynamic(req, frame, code, op, target)
}

/// The call an `INIT_DYNAMIC_CALL` or `INIT_USER_CALL` resolved its
/// callee to, `ZEND_CALL_DYNAMIC`, laid down with the callee operand
/// freed; the callee operand freed and the throw raised where it
/// resolved to none.
fn push_dynamic(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    resolved: Result<DynamicTarget, Thrown>,
) -> Edge {
    let mut target = match resolved {
        Ok(target) => target,
        Err(thrown) => {
            exec::free_operand(req, frame, code, op.op2_type, op.op2);
            return threw(req, frame, &thrown);
        }
    };
    if let Some(name) = target.name.take() {
        req.release(Value::string(name));
    }
    req.push_call(dynamic_header(&target, op.extended_value))?;
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    Ok(advance(req, frame))
}

/// The header of the call a dynamic call resolved to, `ZEND_CALL_DYNAMIC`
/// with `count` arguments: its receiver and closure held, its called
/// scope, and whether the closure reads a cache of its own.
fn dynamic_header(target: &DynamicTarget, count: u32) -> exec::Header {
    let mut header = exec::Header::call(target.function, count);
    header.set_receiver(target.this.map(exec::Receiver::Owned));
    header.set_called_scope(target.called_scope);
    header.closure = target.closure.map(|(closure, _)| closure);
    header.info.set_dynamic(true);
    header
        .info
        .set_own_cache(target.closure.is_some_and(|(_, own)| own));
    header
}

/// The call frame a resolved callback takes, `zend_fcall_info_cache` as
/// `ZEND_INIT_USER_CALL` reads it: a function alone; a method with its
/// scope, and its receiver as one more owner (`GC_ADDREF` under
/// `ZEND_CALL_RELEASE_THIS`); a closure as `INIT_DYNAMIC_CALL` takes
/// one, the closure and its bound receiver held by the call.
fn user_call_target(
    req: &mut Request,
    target: crate::api::call::Target,
) -> Result<DynamicTarget, Thrown> {
    match target {
        crate::api::call::Target::Function(function) => Ok(DynamicTarget {
            function,
            name: None,
            this: None,
            called_scope: None,
            closure: None,
        }),
        crate::api::call::Target::Method {
            function,
            scope,
            this,
        } => {
            let name = req.method_name_string(function)?;
            if let Some(this) = this {
                req.addref(&this.as_value());
            }
            Ok(DynamicTarget {
                function,
                name: Some(name),
                this,
                called_scope: Some(scope),
                closure: None,
            })
        }
        crate::api::call::Target::Closure(object) => dynamic_call_object(req, object),
    }
}

/// `ZEND_SEND_USER` (`Z/Zend/zend_vm_def.h:5635`), each argument after
/// the callback of `call_user_func(...)`: the value read, a reference
/// followed, and copied into the slot at its number; where the parameter
/// takes its argument by reference and nothing else, php's warning
/// (`zend_param_must_be_ref`) and the value in a reference of its own,
/// so what the callee writes through it is lost.
///
/// ```php
/// function r(&$x) { $x = 'written'; return 'r'; }
/// $a = 'kept';
/// function v(&...$all) { foreach ($all as &$one) { $one = 'written'; } return count($all); }
/// var_dump(call_user_func('r', $a), $a, call_user_func('v', $a, 2), $a);
/// ```
/// ```expect
/// Warning: r(): Argument #1 ($x) must be passed by reference, value given in %s on line 5
///
/// Warning: v(): Argument #1 must be passed by reference, value given in %s on line 5
///
/// Warning: v(): Argument #2 must be passed by reference, value given in %s on line 5
/// string(1) "r"
/// string(4) "kept"
/// int(2)
/// string(4) "kept"
/// ```
pub(crate) fn send_user<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let reserved = match reserve_arg(req, frame, code, op) {
        Ok(reserved) => reserved,
        Err(edge) => return edge,
    };
    let value = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    let value = if must_be_sent_by_ref(req, reserved.number) {
        if let Some(function) = req.building_call_function() {
            req.param_must_be_ref(function, reserved.number);
        }
        match req.reference(value) {
            Ok(cell) => cell.as_value(),
            Err(thrown) => return threw(req, frame, &thrown),
        }
    } else {
        value
    };
    place_argument(req, frame, reserved.slot, value)
}

/// `ZEND_SEND_ARRAY` (`Z/Zend/zend_vm_def.h:5484`), the array of
/// `call_user_func_array(...)`: what is no array php's `TypeError`; each
/// integer-keyed element the next positional argument whatever its key,
/// a string-keyed one placed by its name
/// ([`crate::api::call::handle_named_arg`], the one placement every
/// named argument takes), and a positional element after a named one
/// php's `Error`. A parameter that takes its argument by reference and
/// nothing else is handed an element that is no reference with php's
/// warning, in a reference of its own; one that takes it by value is
/// handed a reference's value. With `op2` the array is the
/// `array_slice($array, <offset>, $length)` the compiler read: the
/// offset in `extended_value`, the length in `op2`, `null` for the rest,
/// and no key read.
///
/// ```php
/// function f($a, &$b, ...$rest) { $b = 'b'; var_dump(func_get_args()); }
/// $x = ['one', 'two', 'k' => 'three'];
/// call_user_func_array('f', $x);
/// call_user_func_array('f', ['one', 'b' => 'two']);
/// var_dump(call_user_func_array('max', array_slice([3, 9, 4, 7], 1, 2)), call_user_func_array('max', array_slice([3, 9, 4, 7], 2, null)), call_user_func_array('max', array_slice([3, 9, 4, 7, 1], 0, -3)), call_user_func_array('max', array_slice([3, 9, 4, 7], 1, '2')));
/// try { call_user_func_array('f', [1, 2, 'a' => 3]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { call_user_func_array('f', ['a' => 1, 2]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { call_user_func_array('f', 'x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { call_user_func_array('max', array_slice([1], 0, [])); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: f(): Argument #2 ($b) must be passed by reference, value given in %s on line 4
/// array(2) {
///   [0]=>
///   string(3) "one"
///   [1]=>
///   string(1) "b"
/// }
///
/// Warning: f(): Argument #2 ($b) must be passed by reference, value given in %s on line 5
/// array(2) {
///   [0]=>
///   string(3) "one"
///   [1]=>
///   string(1) "b"
/// }
/// int(9)
/// int(7)
/// int(9)
/// int(9)
///
/// Warning: f(): Argument #2 ($b) must be passed by reference, value given in %s on line 7
/// Named parameter $a overwrites previous argument
/// Cannot use positional argument after named argument
/// call_user_func_array(): Argument #2 ($args) must be of type array, string given
/// array_slice(): Argument #3 ($length) must be of type ?int, array given
/// ```
///
/// A reference among the elements reaches a `__call` as the reference
/// it is ("don't separate references for __call"), and any other
/// callee's by-value parameter as its value:
///
/// ```php
/// function inc(&$x) { $x++; }
/// class P { function __call($n, $a) { call_user_func_array('inc', $a); } }
/// function v($x) { $x = 5; }
/// $arg = 1;
/// call_user_func_array([new P, 'bar'], [&$arg]);
/// call_user_func_array('v', [&$arg]);
/// var_dump($arg);
/// ```
/// ```expect
/// int(2)
/// ```
pub(crate) fn send_array<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let given = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    let Some(array) = given.as_array().copied() else {
        let shown = crate::ops::value_name(req, &given.undef_as_null());
        let thrown = crate::api::raising::throw_error(
            req,
            "TypeError",
            &format!(
                "call_user_func_array(): Argument #2 ($args) must be of type array, {shown} given"
            ),
        );
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        return threw(req, frame, &thrown);
    };
    let placed = if op.op2_type == OperandKind::Unused {
        send_array_elements(req, array)
    } else {
        send_array_slice(req, frame, code, op, array)
    };
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    match placed {
        Ok(()) => Ok(advance(req, frame)),
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// `SEND_ARRAY`'s walk of the whole array, keys read.
fn send_array_elements(req: &mut Request, array: PhpArray) -> Result<(), Thrown> {
    if req.building_call().is_none() {
        return Err(no_call_under(req));
    }
    let mut have_named = false;
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(ht, name, arg)`
    // (`Z/Zend/zend_vm_def.h:5583`), no owner of the table taken: the
    // operand holds it while a by-reference warning reaches the program.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let value = *row.value();
        let (slot, number) = match row.key().clone() {
            crate::api::values::Key::Str(name) => {
                have_named = true;
                match reserve_named(req, name) {
                    Ok(Ok(reserved)) => (reserved.slot, reserved.number),
                    Ok(Err(thrown)) => return Err(thrown),
                    Err(fatal) => return Err(ended(req, fatal)),
                }
            }
            crate::api::values::Key::Int(_) => {
                if have_named {
                    return Err(crate::api::raising::throw_error(
                        req,
                        "Error",
                        "Cannot use positional argument after named argument",
                    ));
                }
                match reserve_next(req) {
                    Ok(reserved) => (reserved.slot, reserved.number),
                    Err(fatal) => return Err(ended(req, fatal)),
                }
            }
        };
        send_array_element(req, slot, number, value)?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// `SEND_ARRAY` over `array_slice($array, <offset>, $length)`: the
/// length read as `array_slice`'s `?int $length` is, a negative one
/// counted from the end, and the elements from the offset on sent in
/// order, no key read.
fn send_array_slice(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    array: PhpArray,
) -> Result<(), Thrown> {
    let given = exec::operand(req, frame, code, op.op2_type, op.op2).deref();
    let skip = usize::try_from(op.extended_value).unwrap_or(usize::MAX);
    let count = array.len();
    let rest = i64::try_from(count.saturating_sub(skip)).unwrap_or(i64::MAX);
    let length = match given.as_ref() {
        ValueRef::Int(n) => Some(n),
        ValueRef::Null | ValueRef::Undef => Some(rest),
        ValueRef::Bool(b) if !req.frame_strict_types(frame) => Some(i64::from(b)),
        _ if !req.frame_strict_types(frame) => crate::ops::arg_long_weak(req, &given),
        _ => None,
    };
    let Some(mut length) = length else {
        let shown = crate::ops::value_name(req, &given);
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
        return Err(crate::api::raising::throw_error(
            req,
            "TypeError",
            &format!("array_slice(): Argument #3 ($length) must be of type ?int, {shown} given"),
        ));
    };
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    if length < 0 {
        length += rest;
    }
    if skip >= count || length <= 0 {
        return Ok(());
    }
    let length = usize::try_from(length.min(rest)).unwrap_or(0);
    // `ZEND_HASH_FOREACH_VAL(ht, arg)` (`Z/Zend/zend_vm_def.h:5541`), the
    // first `skip` rows stepped over, no owner of the table taken: the
    // operand holds it while a by-reference warning reaches the program.
    let (mut skip, mut sent) = (skip, 0);
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        if skip > 0 {
            skip -= 1;
            return Ok(ControlFlow::Continue(()));
        }
        if sent >= length {
            return Ok(ControlFlow::Break(()));
        }
        sent += 1;
        let reserved = match reserve_next(req) {
            Ok(reserved) => reserved,
            Err(fatal) => return Err(ended(req, fatal)),
        };
        send_array_element(req, reserved.slot, reserved.number, *row.value())?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// One element of `SEND_ARRAY` into its slot, by the parameter's way of
/// taking it (`ARG_SHOULD_BE_SENT_BY_REF`, `ARG_MAY_BE_SENT_BY_REF`):
/// the element with one more owner, a reference's value where the
/// parameter takes a value, and a value that is no reference wrapped in
/// one of its own, with php's warning, where the parameter takes a
/// reference and nothing else. `number` is the parameter's, php's
/// `arg_num`.
fn send_array_element(
    req: &mut Request,
    slot: ArgSlot,
    number: u32,
    element: Value,
) -> Result<(), Thrown> {
    let is_ref = element.as_php_ref().is_some();
    let value = if should_be_sent_by_ref(req, number) {
        if !is_ref && must_be_sent_by_ref(req, number) {
            if let Some(function) = req.building_call_function() {
                req.param_must_be_ref(function, number);
            }
            req.addref(&element);
            req.reference(element)?.as_value()
        } else {
            req.addref(&element);
            element
        }
    } else {
        // "don't separate references for __call": a trampoline takes
        // the reference as it is, every other callee its value.
        let value = if req
            .building_call_function()
            .is_some_and(|function| req.via_trampoline(function))
        {
            element
        } else {
            element.deref()
        };
        req.addref(&value);
        value
    };
    if let Err(fatal) = put_argument(req, slot, value) {
        return Err(ended(req, fatal));
    }
    Ok(())
}

/// The internal error a `SEND_ARRAY` with no call frame under it is.
/// The fatal a walk over a call's arguments met, ending the request.
fn ended(req: &mut Request, fatal: Fatal) -> Thrown {
    match req.fatal_of::<()>(Err(fatal)) {
        Err(thrown) => thrown,
        Ok(()) => Thrown::of(None),
    }
}

fn no_call_under(req: &mut Request) -> Thrown {
    crate::api::raising::fatal(
        req,
        "InternalError: a SEND_ARRAY opline with no call frame under it",
    )
}

/// `ZEND_ASSERT_CHECK` (`Z/Zend/zend_vm_def.h:8880`), in front of every
/// `assert()` php compiles while `zend.assertions` is at or above zero:
/// while it stands at or below zero as the program serves, the jump in `op2` is
/// taken over the whole call, so nothing of its arguments is evaluated,
/// and `true` is what the call leaves where its value is read;
/// otherwise the call is served.
///
/// ```php
/// function f() { echo "evaluated\n"; return false; }
/// ini_set('zend.assertions', 0);
/// var_dump(assert(f()));
/// assert(f(), 'x');
/// ini_set('zend.assertions', 1);
/// try { var_dump(assert(f())); } catch (AssertionError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// evaluated
/// assert(f())
/// ```
pub(crate) fn assert_check<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if req.assertions() > 0 {
        return Ok(advance(req, frame));
    }
    write_out(req, frame, code, op, &Value::bool(true));
    jump_to(op.op2)
}

/// Where a `SEND_*` or a `CHECK_FUNC_ARG` places its argument, php's
/// `op2` decoded once: the argument's number, or, where `op2` is an
/// `IS_CONST`, the name the compiler left because it could not place
/// the argument itself.
#[derive(Debug, Clone, Copy)]
enum ArgAt {
    Number(u32),
    Name(ConstIdx),
}

impl ArgAt {
    fn of(op: &Opline) -> ArgAt {
        match op.op2_type {
            OperandKind::Const => ArgAt::Name(ConstIdx(op.op2)),
            _ => ArgAt::Number(op.op2),
        }
    }
}

/// A slot of the call being built a send writes into, the call's own
/// record named once where the send reserved it: a positional
/// argument's, or the one `zend_handle_named_arg` reserved for a named
/// one.
#[derive(Debug, Clone, Copy)]
struct ArgSlot {
    call: FrameRef,
    to: ArgTo,
}

/// Where in its call an [`ArgSlot`] stands.
#[derive(Debug, Clone, Copy)]
enum ArgTo {
    Positional(usize),
    Named(crate::api::call::NamedAt),
}

/// A send's slot and the argument's number, php's `arg` and `arg_num`,
/// the number the by-reference bit is read at.
#[derive(Debug, Clone, Copy)]
struct Reserved {
    slot: ArgSlot,
    number: u32,
}

/// The slot a send writes into: `ZEND_CALL_VAR` over the slot
/// `EX_NUM_TO_VAR` numbered for a number, bounded by the count the `INIT`
/// said, and `zend_handle_named_arg` for a name
/// ([`crate::api::call::handle_named_arg`], the one placement
/// `zend_call_function`'s named arguments take too), whose extra named
/// entry holds one owner of the name. What it refuses is php's `Error`,
/// the operand given up (`FREE_OP1`), and the `Err` is the edge the
/// handler returns.
fn reserve_arg(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Result<Reserved, Edge> {
    let no_call = || -> Result<Reserved, Edge> {
        Err(Err(Fatal::error(
            "InternalError: a SEND_* opline with no call frame under it",
        )))
    };
    match ArgAt::of(op) {
        ArgAt::Number(number) => {
            let at = usize::try_from(number.saturating_sub(1)).unwrap_or(usize::MAX);
            let Some((call, passed)) = req
                .vm()
                .building_at()
                .and_then(|call| Some((call, req.vm().at(call)?.num_args as usize)))
            else {
                return no_call();
            };
            // The `INIT_*` said how many arguments the call has in
            // order, so a number past that is a compiler bug and not an
            // allocation.
            if at >= passed {
                return Err(Err(Fatal::error(format!(
                    "InternalError: a SEND_* opline places argument {number} of a call of {passed}"
                ))));
            }
            Ok(Reserved {
                slot: ArgSlot {
                    call,
                    to: ArgTo::Positional(at),
                },
                number,
            })
        }
        ArgAt::Name(at) => {
            let Some(name) = exec::constant(code, at).as_php_string().copied() else {
                return Err(Err(Fatal::error(
                    "InternalError: a SEND_* opline names its argument by what is no string",
                )));
            };
            match reserve_named(req, name) {
                Ok(Ok(reserved)) => Ok(reserved),
                Ok(Err(thrown)) => {
                    exec::free_operand(req, frame, code, op.op1_type, op.op1);
                    Err(threw(req, frame, &thrown))
                }
                Err(fatal) => Err(Err(fatal)),
            }
        }
    }
}

/// The slot `zend_handle_named_arg` gives a named argument of the call
/// being built ([`crate::api::call::handle_named_arg`], the one
/// placement), whose extra named entry holds one owner of the name of
/// its own; what it refuses is php's `Error`, the inner `Err`. A `SEND_*`
/// by name and a string key `SEND_UNPACK` spreads both reach it.
fn reserve_named(req: &mut Request, name: PhpString) -> Result<Result<Reserved, Thrown>, Fatal> {
    let no_call = || Fatal::error("InternalError: a SEND_* opline with no call frame under it");
    let Some(function) = req.building_call_function() else {
        return Err(no_call());
    };
    let offset = crate::api::call::arg_offset_by_name(req, function, name.as_bytes());
    let Some(call) = req.building_call() else {
        return Err(no_call());
    };
    let passed = req
        .vm()
        .at(call)
        .map_or(0, |header| header.num_args as usize);
    let placed = {
        let frame = req.vm().stack.frame(call);
        let set = |at: usize| {
            frame.is_some_and(|frame| {
                frame
                    .slots()
                    .get(at)
                    .is_some_and(|z| z.kind() != php_heap::ZvalType::Undef)
            })
        };
        let named = req
            .annex_of(call)
            .map_or(&[][..], |annex| &annex.extra_named[..]);
        crate::api::call::named_arg_place(offset, passed, set, named, name)
    };
    let (place, number) = match placed {
        Ok(placed) => placed,
        Err(refused) => return Ok(Err(refused.throw(req, name))),
    };
    let at = match place {
        crate::api::call::NamedTo::Argument(at) => {
            // `zend_vm_stack_extend_call_frame` past what the call was
            // given, `ZEND_CALL_NUM_ARGS` raised to take it, every slot
            // between left `UNDEF` for `CHECK_UNDEF_ARGS`.
            if at >= passed {
                let len = req
                    .vm()
                    .stack
                    .frame(call)
                    .map_or(0, crate::exec::HeapFrame::len);
                if at >= len {
                    req.grow_call(call, at + 1 - len)?;
                }
                if let Some(header) = req.vm_mut().at_mut(call) {
                    header.num_args = u32::try_from(at + 1).unwrap_or(u32::MAX);
                }
            }
            crate::api::call::NamedAt::Argument(at)
        }
        crate::api::call::NamedTo::Extra => {
            // `zend_new_array(0)` and the add that grows it, one owner of
            // the name the table's.
            req.addref(&Value::string(name));
            let mut named = req
                .annex_mut(call)
                .map(|annex| std::mem::take(&mut annex.extra_named))
                .unwrap_or_default();
            named.rows_mut().push((name, Value::undef()));
            let at = named.len() - 1;
            let laid = named.lay(req);
            if let Some(annex) = req.annex_mut(call) {
                annex.extra_named = named;
            }
            if let Err(thrown) = laid {
                return Ok(Err(thrown));
            }
            crate::api::call::NamedAt::Extra(at)
        }
    };
    Ok(Ok(Reserved {
        slot: ArgSlot {
            call,
            to: ArgTo::Named(at),
        },
        number,
    }))
}

/// The next positional slot of the call being built, past every
/// argument placed so far (`ZEND_CALL_NUM_ARGS(EX(call)) + 1`), which a
/// `SEND_UNPACK` places an integer-keyed element in; the call's count
/// grows to take it, as `zend_vm_stack_extend_call_frame` grows the
/// frame.
fn reserve_next(req: &mut Request) -> Result<Reserved, Fatal> {
    let Some(call) = req.building_call() else {
        return Err(Fatal::error(
            "InternalError: SEND_UNPACK with no call frame under it",
        ));
    };
    let at = req
        .vm()
        .at(call)
        .map_or(0, |header| header.num_args as usize);
    let len = req
        .vm()
        .stack
        .frame(call)
        .map_or(0, crate::exec::HeapFrame::len);
    if at >= len {
        req.grow_call(call, at + 1 - len)?;
    }
    if let Some(header) = req.vm_mut().at_mut(call) {
        header.num_args = u32::try_from(at + 1).unwrap_or(u32::MAX);
    }
    Ok(Reserved {
        slot: ArgSlot {
            call,
            to: ArgTo::Positional(at),
        },
        number: u32::try_from(at + 1).unwrap_or(u32::MAX),
    })
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn do_fcall_by_name<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if let Some(edge) = inline_enter(req, frame, code, op, true) {
        return edge;
    }
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn do_fcall<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if let Some(edge) = inline_enter(req, frame, code, op, true) {
        return edge;
    }
    do_call(req, frame, code, op, "ZEND_DO_FCALL")
}

/// `ZEND_INCLUDE_OR_EVAL` (`Z/Zend/zend_vm_def.h:6544`), the four file
/// constructs and `eval`: the operand as a string (`zval_try_get_tmp_string`), the
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
///
/// `eval` is the fifth arm, one opline with one path through here: the
/// string compiled as the request's own unit ([`Request::eval_string`])
/// and served on the same kind of frame an included file is, attached to
/// the caller's variables, so what the string writes the caller reads.
/// What it `return`s is the value; a string that returns nothing
/// answers `null`, since `compile_string` ends its op array in `RETURN
/// null`, and an empty string answers `false`, `compile_string`'s
/// `NULL`, with no frame.
///
/// ```php
/// function f($x) { $y = eval('return $x * 2;'); eval('$z = $y + 1;'); return [$y, $z]; }
/// var_dump(f(3), eval(''), eval('$a = 1;'), $a, eval("?>raw\n<?php return __LINE__;"));
/// ```
/// ```expect
/// raw
/// array(2) {
///   [0]=>
///   int(6)
///   [1]=>
///   int(7)
/// }
/// bool(false)
/// NULL
/// int(1)
/// int(2)
/// ```
///
/// A throwable raised inside the string unwinds through the caller, and
/// the frame the string ran on traces as `eval()` with no argument
/// (`zend_fetch_debug_backtrace`'s `build_filename_arg = 0`), at the
/// caller's line, while what it called traces at the string's own name.
///
/// ```php
/// function boom() { throw new Exception('boom'); }
/// try { eval("\n\nboom();"); } catch (Exception $e) { echo $e->getTraceAsString(), "\n"; }
/// ```
/// ```expect
/// #0 %s(3) : eval()'d code(3): boom()
/// #1 %s(3): eval()
/// #2 {main}
/// ```
pub(crate) fn include_or_eval<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(kind) = exec::IncludeKind::of(op.extended_value) else {
        return Err(Fatal::error(
            "InternalError: an INCLUDE_OR_EVAL whose extended_value names no construct",
        ));
    };
    let operand = exec::operand(req, frame, code, op.op1_type, op.op1);
    let returns_to =
        (op.out_type != OperandKind::Unused).then(|| (frame, exec::temporary_slot(code, op.out)));
    let edge = match crate::ops::to_string(req, &operand) {
        // `tmp_inc_filename`, held across the compile and given up after
        // it, as `zend_include_or_eval` holds it.
        Ok(string) => {
            let site = (&*code.filename.0, op.lineno);
            let edge = included(req, frame, kind, string.as_bytes(), site, returns_to);
            req.release(Value::string(string));
            edge
        }
        Err(thrown) => threw(req, frame, &thrown),
    };
    // `FREE_OP1()`: the operand stands while the file is opened and its
    // frame pushed, and goes before the file runs.
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    edge
}

/// `zend_include_or_eval` over the path or the code `written`, and the
/// handler's arms after it: the value answered without a frame, or the
/// code frame pushed with the op array php holds for it. A unit whose
/// code is its one `RETURN` of a constant answers that constant with no
/// frame only while the tier the engine binds hooks no user op array
/// (`Hooked::user`), php's `zend_execute_ex == execute_ex`
/// (`Z/Zend/zend_vm_def.h:6569-6572`): where it hooks them, the
/// debugger's loop steps that opline as any other, so a breakpoint on an
/// included file of declarations alone stops there. The binding is the
/// engine's and not the handler's `T`, which a tier stepping one handler
/// at a time instantiates as `Nop`.
///
/// ```php
/// file_put_contents(__DIR__ . '/decls.php', "<?php\nfunction bar() {\n  return 3;\n}\n");
/// include __DIR__ . '/decls.php';
/// echo bar(), "\n";
/// unlink(__DIR__ . '/decls.php');
/// ```
/// ```request phpdbg
/// b ZEND_RETURN
/// r
/// c
/// c
/// c
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at ZEND_RETURN]
/// prompt> [Breakpoint #0 in ZEND_RETURN at %SCRATCH%/decls.php:5, hits: 1]
/// >00005:%s
/// prompt> [Breakpoint #0 in ZEND_RETURN at %SCRATCH%/decls.php:3, hits: 2]
/// >00003:   return 3;
///  00004: }
///  00005:%s
/// prompt> 3
/// [Breakpoint #0 in ZEND_RETURN at %SCRATCH%/witness.php:5, hits: 3]
/// >00005: unlink(__DIR__ . '/decls.php');
/// prompt> [Script ended normally]
/// prompt>
/// ```
fn included(
    req: &mut Request,
    frame: FrameRef,
    kind: exec::IncludeKind,
    written: &[u8],
    site: (&[u8], u32),
    returns_to: Option<(FrameRef, u32)>,
) -> Edge {
    let opened = match kind {
        exec::IncludeKind::Eval => req.eval_string(written, site),
        exec::IncludeKind::Include
        | exec::IncludeKind::IncludeOnce
        | exec::IncludeKind::Require
        | exec::IncludeKind::RequireOnce => req.include_file(kind, written),
    };
    let unit = match opened {
        Ok(Opened::Unit(unit)) => unit,
        Ok(Opened::Already) => return answered(req, frame, returns_to, Value::bool(true)),
        Ok(Opened::Failed) => return answered(req, frame, returns_to, Value::bool(false)),
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let source = match kind {
        exec::IncludeKind::Eval => php_compile::SourceKind::EVAL,
        _ => php_compile::SourceKind::File,
    };
    if !crate::engine::note_compiled(req, &unit, source) {
        return Ok(Stepped::Threw(frame).into());
    }
    // The op array `zend_accel_load_script` copies out of the cache, or
    // `zend_compile` makes, the request's while the file runs.
    let copy = match crate::api::values::Emalloc::new(req, exec::OP_ARRAY_SIZE) {
        Ok(copy) => copy,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let main = exec::code_of(&unit);
    if !req.shared.declared().hooked.user
        && let [only] = &main.oplines[..]
        && only.opcode == Opcode::Return
        && only.op1_type == OperandKind::Const
    {
        let value = exec::constant(main, ConstIdx(only.op1));
        req.addref(&value);
        copy.free(req);
        // `destroy_op_array`: its names go with it.
        req.release_unit_strings(Some(&unit));
        return answered(req, frame, returns_to, value);
    }
    let callee = req.push_code_frame(unit, CodeFrom::Nested(frame, returns_to))?;
    crate::observer::call(req, callee);
    if let Err(thrown) = req.hold_op_array(callee, copy) {
        return threw(req, callee, &thrown);
    }
    Ok(Stepped::Pushed(callee).into())
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
            req.replace_slot(at, slot, value);
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn do_ucall<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if let Some(edge) = inline_enter(req, frame, code, op, false) {
        return edge;
    }
    do_call(req, frame, code, op, "ZEND_DO_UCALL")
}

/// The inline path of `DO_UCALL`, and of `DO_FCALL` and
/// `DO_FCALL_BY_NAME` where `flagged`: a call to a user function that
/// `i_init_func_execute_data` makes a frame of by writing words alone
/// ([`Request::enter_call`]), entered with no call out of the loop. The
/// answer is [`Stepped::Pushed`], as [`enter`]'s; `None`, with nothing
/// touched, for every other call, which [`do_call`] serves.
#[inline(always)] // as `inline_init_fcall`
#[allow(clippy::inline_always)]
fn inline_enter(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    flagged: bool,
) -> Option<Edge> {
    let returns_to =
        (op.out_type != OperandKind::Unused).then(|| exec::temporary_slot(code, op.out));
    let callee = req.enter_call(returns_to, frame, flagged)?;
    Some(Ok(Stepped::Pushed(callee).into()))
}

/// The `ZEND_USER_FUNCTION` arm of the `DO_*CALL` handlers
/// (`Z/Zend/zend_vm_def.h:4169`, `i_init_func_execute_data`, design
/// §5.6): the served call taken off the call stack and pushed as the
/// callee's frame, its arguments in the parameters' CVs and its
/// receiver carried over ([`Request::push_user_frame`]), the caller's
/// slot for the value recorded where `RETURN_VALUE_USED`, the frame
/// standing where php's push leaves it: past the whole `RECV*` prologue
/// where the function has no typed parameter and was passed at least as
/// many arguments as it declares (`Z/Zend/zend_execute.c:4426-4437`),
/// and at opline zero otherwise. The answer is [`Stepped::Pushed`], and
/// the loop that takes it, which holds the callee's op array as it
/// enters it, runs what is left of the prologue to the entry safepoint
/// ([`exec::prologue`]), so a `RECV` that throws is never inside a
/// recording and the frame is reported [`Stepped::Threw`], and then the
/// activation's interrupt check and `Site::Entry`.
fn enter(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    call: FrameRef,
    shape: crate::api::request::FrameShape,
) -> Edge {
    let returns_to =
        (op.out_type != OperandKind::Unused).then(|| exec::temporary_slot(code, op.out));
    let activated = req.activate_user(call, shape, returns_to, exec::Caller::Opline(frame))?;
    Ok(Stepped::Pushed(activated.frame).into())
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
/// argument where it stands, as php converts the zval in the frame, and
/// a by-reference parameter's in the cell it is bound to; what does not
/// convert is `zend_verify_arg_error`, the `TypeError` naming the
/// function, the parameter, the declared type, the value's kind and,
/// where the caller is user code, its file and line.
///
/// ```php
/// function to_int(int &$x) {}
/// function to_string(string &$x) {}
/// class C { public float $f = 1.5; }
/// $x = 1.0; to_int($x); var_dump($x);
/// to_string($x); var_dump($x);
/// $c = new C; $r = &$c->f;
/// try { to_int($r); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// string(1) "1"
/// to_int(): Argument #1 ($x) must be of type int, float given, called in %s on line 8
/// ```
fn verify_arg_type(
    req: &mut Request,
    frame: FrameRef,
    number: u32,
    slot: u32,
) -> Result<(), Thrown> {
    let at = usize::try_from(number.saturating_sub(1)).unwrap_or(usize::MAX);
    let argument = req.slot(frame, slot);
    if let Some(coerced) = verified_arg(req, frame, number, at, &argument)? {
        req.replace_slot(frame, slot, coerced);
    }
    Ok(())
}

/// The body of [`verify_arg_type`] over a value rather than a slot,
/// which is what a variadic's arguments are: the declared type is the
/// parameter numbered `at`, the variadic's own for every argument it
/// collects, while `number` is the argument's own position, which is
/// what php's error says. `Some` is the value the coercion outside
/// `strict_types` made of it, which the caller writes where the value
/// stood; `None` is a value the type contains as it is, or a reference,
/// whose coercion is written into the cell here, as `zend_check_type`
/// converts the value the reference holds. A cell some typed property
/// is a source of converts nothing ("We cannot have conversions for
/// typed refs"), so what the type does not hold is the error.
fn verified_arg(
    req: &mut Request,
    frame: FrameRef,
    number: u32,
    at: usize,
    raw: &Value,
) -> Result<Option<Value>, Thrown> {
    let argument = &raw.deref();
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
    let cell = raw.as_php_ref().copied();
    let typed_cell = cell.is_some_and(|cell| !req.ref_type_sources(cell.heap()).is_empty());
    if !typed_cell
        && let Some(coerced) =
            crate::api::coerce::scalar_type_hint(req, argument, ty.mask(), strict)?
    {
        let Some(cell) = cell else {
            return Ok(Some(coerced));
        };
        req.reference_set(cell.heap(), coerced);
        return Ok(None);
    }
    Err(arg_type_error(req, frame, number, &ty, argument))
}

/// `ZEND_RECV_VARIADIC` (`Z/Zend/zend_vm_def.h:5773`): the arguments
/// from the one numbered `op1` on, collected into a packed array in the
/// variadic parameter's CV. The arguments past the declared parameters
/// are the frame's extra ones ([`exec::Annex::extra_args`]), where
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
/// placed ([`exec::Annex::extra_named`]) follow under their names,
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
    // The extra arguments stand past the CVs and the temporaries.
    let (base, count) = req.frame_at(frame).map_or((0, 0), |state| {
        let base = state
            .code()
            .map_or(0, |code| code.num_cvs.saturating_add(code.num_tmps));
        (base, state.extra_args().len())
    });
    let named = req
        .annex_of(frame)
        .map(|extra| extra.extra_named.to_vec())
        .unwrap_or_default();
    // An untyped variadic with no positional extras is php's
    // `ZVAL_ARR(params, EX(extra_named_params))`, the frame's table
    // itself, which the array collected here stands for.
    let untyped = req
        .vm()
        .at(frame)
        .and_then(|state| state.function)
        .and_then(|function| req.function_decl(function))
        .and_then(|decl| decl.parameters.get(at))
        .is_none_or(|parameter| parameter.ty.is_none());
    if count == 0 && untyped {
        let table = req
            .annex_mut(frame)
            .map(|extra| extra.extra_named.take_table());
        if let Some(table) = table {
            table.free(req);
        }
    }
    // `ZVAL_EMPTY_ARRAY(params)` where nothing is collected; an extra
    // named argument is php's `SEPARATE_ARRAY` of it, a table of its own.
    let collected = if count == 0 && named.is_empty() {
        Ok(PhpArray::empty())
    } else {
        req.array_sized(u32::try_from(count).unwrap_or(u32::MAX), true)
    };
    let collected = match collected {
        Ok(collected) => collected,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    req.replace_slot(frame, op.out, collected.to_value());
    for step in 0..count {
        let place = base.saturating_add(u32::try_from(step).unwrap_or(u32::MAX));
        let argument = req.slot(frame, place);
        let number = first.saturating_add(u32::try_from(step).unwrap_or(0));
        let argument = match verified_arg(req, frame, number, at, &argument) {
            Ok(None) => argument,
            Ok(Some(coerced)) => {
                let previous = req.swap_slot(frame, place, coerced);
                req.release(previous);
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
    for (name, argument) in named {
        let argument = match verified_arg(req, frame, number, at, &argument) {
            Ok(None) => argument,
            Ok(Some(coerced)) => {
                if let Some(extra) = req.annex_mut(frame)
                    && let Some((_, place)) = extra
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
///
/// A property returned by reference is the place its fetch found, read
/// through, and a value the declared type coerces is written back into
/// it:
///
/// ```php
/// class C { public $a = [1]; public $f = 1;
///     function &get(): array { return $this->a; }
///     function &num(): float { return $this->f; } }
/// $c = new C; $r = &$c->get(); $r[] = 2; var_dump($c->a);
/// $n = &$c->num(); var_dump($n, $c->f);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// float(1)
/// float(1)
/// ```
///
/// A by-reference parameter returned by reference is coerced where the
/// reference holds it, so the caller's variable reads the coerced value;
/// returned by value it is copied out of the reference first:
///
/// ```php
/// function &r(int &$a): string { return $a; }
/// function v(int &$a): string { return $a; }
/// $x = 1; v($x); var_dump($x); r($x); var_dump($x);
/// ```
/// ```expect
/// int(1)
/// string(1) "1"
/// ```
///
/// A `CONST` operand is copied into the temporary the opline writes
/// before it is checked, so the `RETURN` after reads the coerced value,
/// or the constant itself where it passes, wherever the constant stands
/// in the op array's table, a jump table's arms included (#2276):
///
/// ```php
/// function f($m): int { switch ($m) { case "a": return 1.5; case "b": return 2; } }
/// var_dump(f("a"));
/// function g($m): string { switch ($m) { case "a": return 15; case "b": return 2; } }
/// var_dump(g("a"), g("b"));
/// function h(): callable { return "strlen"; }
/// var_dump(h());
/// ```
/// ```expect
///
/// Deprecated: Implicit conversion from float 1.5 to int loses precision in %s on line 2
/// int(1)
/// string(2) "15"
/// string(1) "2"
/// string(6) "strlen"
/// ```
///
/// The first test is php's whole check for a value of a type the
/// declaration names outright, `ZEND_TYPE_CONTAINS_CODE(ret_info->type,
/// Z_TYPE_P(retval_ref))`: the op array's [`OpArray::return_mask`]
/// against the type of the value a `CV`, a `TMP` or a `VAR` holds where
/// it stands, which advances with the declaration unread, as `RECV`'s
/// mask does. php tests a reference's own type there, which always
/// misses and holds on the slow path; this tests the value it holds,
/// which cannot be told apart, since a hit writes nothing. Bit 0 is
/// never in a mask, so an undefined `CV` misses and warns below. A
/// `CONST`, the end of the body, the place a by-reference return
/// fetched and every value the mask misses take the declaration's whole
/// check, as `zend_check_type_slow` does:
///
/// ```php
/// function i(array $a): int { return $a[0]; }
/// function f(array $a): float { $v = $a[1]; return $v; }
/// function n(?int $x): ?int { return $x; }
/// function u(array $a): int|string { return $a[2]; }
/// var_dump(i([7, 1.5, "s"]), f([7, 1.5, "s"]), n(null), u([7, 1.5, "s"]));
/// ```
/// ```expect
/// int(7)
/// float(1.5)
/// NULL
/// string(1) "s"
/// ```
///
/// A class, and `static`, which names the called class, hold an object
/// on the slow path, and the error names the class `static` resolved to:
///
/// ```php
/// class A { function me(): static { return $this; } function other(): static { return new A; } }
/// class B extends A {}
/// function c(object $o): A { return $o; }
/// var_dump(get_class(c(new B)), get_class((new B)->me()));
/// foreach ([fn () => c(new stdClass), fn () => (new B)->other()] as $f) {
///     try { $f(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// string(1) "B"
/// string(1) "B"
/// c(): Return value must be of type A, stdClass returned
/// A::other(): Return value must be of type B, A returned
/// ```
///
/// A by-reference return that holds hands back the place it fetched, or
/// the reference a `CV` holds, untouched:
///
/// ```php
/// function &d(array &$a): int { return $a[0]; }
/// function &g(int &$v): int { return $v; }
/// $a = [5]; $x = &d($a); $x = 6;
/// $i = 1; $y = &g($i); $y = 2;
/// var_dump($a[0], $i);
/// ```
/// ```expect
/// int(6)
/// int(2)
/// ```
///
/// An undefined `CV` warns here and holds as `null` where the type
/// allows it, and the `RETURN` after warns again; where it does not,
/// the `TypeError` is over `null`:
///
/// ```php
/// function n(): ?int { return $u; }
/// var_dump(n());
/// function i(): int { return $u; }
/// try { i(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Warning: Undefined variable $u in %s on line 2
///
/// Warning: Undefined variable $u in %s on line 2
/// NULL
///
/// Warning: Undefined variable $u in %s on line 4
/// i(): Return value must be of type int, null returned
/// ```
///
/// An `int` returned as `float` misses the mask and is widened, under
/// `strict_types` as outside it:
///
/// ```php
/// function f(int $i): float { return $i; }
/// var_dump(f(1));
/// ```
/// ```expect
/// float(1)
/// ```
///
/// ```php
/// declare(strict_types=1);
/// function f(int $i): float { return $i; }
/// var_dump(f(1));
/// ```
/// ```expect
/// float(1)
/// ```
///
/// A temporary the mask misses is coerced where it stands outside
/// `strict_types`, and is the `TypeError` under it:
///
/// ```php
/// function s(int $i): string { return $i + 1; }
/// var_dump(s(1));
/// ```
/// ```expect
/// string(1) "2"
/// ```
///
/// ```php
/// declare(strict_types=1);
/// function s(int $i): string { return $i + 1; }
/// try { s(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// s(): Return value must be of type string, int returned
/// ```
pub(crate) fn verify_return_type<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if op.op1_type != OperandKind::Const {
        let at = ByKind(op.op1_type).read(req.slots_mut(frame), code, op.op1);
        let returned = crate::operand::deref(at);
        // `MAY_BE_ANY` keeps the codes a value can have: an `IS_INDIRECT`
        // would test bit 12, which is `callable`'s.
        let code_bit = 1u32 << (returned.kind() as u32);
        if code.return_mask & php_unit::decl::Type::MAY_BE_ANY & code_bit != 0 {
            return Ok(advanced());
        }
    }
    return_mask_missed(req, frame, code, op)
}

/// [`verify_return_type`] past its mask: `zend_check_type_slow` over the
/// declared type, the coercion beside it and the `TypeError`, out of
/// line so the hit saves no register for it.
#[cold]
#[inline(never)]
fn return_mask_missed(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let Some((_, by_ref)) = declared_return(req, frame) else {
        return Ok(advance(req, frame));
    };
    if op.op1_type == OperandKind::Unused {
        let thrown = return_type_error(req, frame, None)?;
        return threw(req, frame, &thrown);
    }
    // A `VAR` a by-reference return fetched is the place it found, read
    // through (`Z_INDIRECT_P(retval_ptr)`), and a coerced value lands in
    // that place.
    let indirect = match op.op1_type {
        OperandKind::Var => req
            .slot_ptr(frame, exec::temporary_slot(code, op.op1))
            .and_then(|at| at.read().as_indirect()),
        _ => None,
    };
    let returned = match indirect {
        Some(place) => exec::value_of(place.read()).deref(),
        None => exec::operand(req, frame, code, op.op1_type, op.op1).deref(),
    };
    // A `CONST` operand is checked in the temporary the `RETURN` reads
    // (`ZVAL_COPY(EX_VAR(opline->result.var), retval_ptr)`), never in
    // place in the table.
    let constant = op.op1_type == OperandKind::Const;
    if constant {
        write_out(req, frame, code, op, &returned);
    }
    // The declared type is read where the request holds it and let go
    // before the one test that takes the request whole.
    let scopes = req.frame_scopes(frame);
    let held = crate::api::coerce::holding(req, &returned, declared_again(req, frame)?, scopes);
    if held.resolve(req, &returned, scopes) {
        return Ok(advance(req, frame));
    }
    let strict = req.frame_strict_types(frame);
    let mask = code.return_mask;
    let coerced = match crate::api::coerce::scalar_type_hint(req, &returned, mask, strict) {
        Ok(coerced) => coerced,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let Some(coerced) = coerced else {
        let thrown = return_type_error(req, frame, Some(&returned))?;
        return threw(req, frame, &thrown);
    };
    if let Some(place) = indirect {
        let held = exec::value_of(place.read());
        let place = held
            .as_php_ref()
            .map_or(place, |cell| cell.heap().val_ref());
        let previous = exec::value_of(place.read());
        place.write(coerced.to_zval());
        req.release(previous);
        return Ok(advance(req, frame));
    }
    let slot = match op.op1_type {
        OperandKind::Cv => op.op1,
        _ if constant => exec::temporary_slot(code, op.out),
        _ => exec::temporary_slot(code, op.op1),
    };
    // `ZEND_ACC_RETURN_REFERENCE` over a reference: the check is handed
    // the reference and coerces the value it holds, which every name
    // bound to it reads after; a by-value return unwraps it first.
    if by_ref
        && let Some(cell) = req
            .slot_ptr(frame, slot)
            .and_then(|at| exec::value_of(at.read()).as_php_ref().copied())
    {
        let place = cell.heap().val_ref();
        let previous = exec::value_of(place.read());
        place.write(coerced.to_zval());
        req.release(previous);
        return Ok(advance(req, frame));
    }
    req.replace_slot(frame, slot, coerced);
    Ok(advance(req, frame))
}

/// `EX(func)->common.arg_info - 1` of the frame's function where it
/// declares a return type: the type, borrowed where the request holds
/// it, and whether the function returns by reference.
fn declared_return(req: &Request, frame: FrameRef) -> Option<(&php_unit::decl::Type, bool)> {
    let function = req.vm().at(frame)?.function?;
    let decl = req.function_decl(function)?;
    Some((decl.return_type.as_ref()?, decl.returns_by_ref))
}

/// [`declared_return`]'s type read again after the slow path's first
/// read found it, once the request has been let go of: none there is
/// this engine's error, never a type that holds or a nameless one.
fn declared_again(req: &Request, frame: FrameRef) -> Result<&php_unit::decl::Type, Fatal> {
    declared_return(req, frame).map(|(ty, _)| ty).ok_or_else(|| {
        Fatal::error("InternalError: a VERIFY_RETURN_TYPE whose function no longer declares its return type")
    })
}

/// `zend_verify_return_error` (`Z/Zend/zend_execute.c:1421`): the
/// `TypeError` over a return, `{fn}(): Return value must be of type
/// {declared}, {given} returned`, with `none` for the end of a body that
/// returned nothing. Its site is the function's own, where the loop
/// stands, as an argument's is.
fn return_type_error(
    req: &mut Request,
    frame: FrameRef,
    returned: Option<&Value>,
) -> Result<Thrown, Fatal> {
    let name = req.frame_function_name(frame);
    let declared = resolved_type_name(req, frame, declared_again(req, frame)?);
    let given = match returned {
        Some(returned) => crate::ops::value_name(req, &returned.undef_as_null()),
        None => String::from("none"),
    };
    let message = format!("{name}(): Return value must be of type {declared}, {given} returned");
    Ok(crate::api::raising::throw_error(req, "TypeError", &message))
}

/// `zend_verify_type_error_common`'s `need_msg`: the type resolved
/// against the frame's scope and called scope
/// ([`crate::api::coerce::resolved_type_name`]), up to the NUL `%s`
/// stops at.
fn resolved_type_name(req: &Request, frame: FrameRef, ty: &php_unit::decl::Type) -> String {
    let mut named = crate::api::coerce::resolved_type_name(req, req.frame_scopes(frame), ty);
    if let Some(end) = named.find('\0') {
        named.truncate(end);
    }
    named
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
    // `zf->common.scope ? "method" : "function"`: a static method is
    // a method, and a closure written in a class is one.
    let name = req.frame_function_name(frame);
    let kind = if req.frame_scope(frame).is_some() {
        "method"
    } else {
        "function"
    };
    let message = format!("{name}(): never-returning {kind} must not implicitly return");
    let thrown = crate::api::raising::throw_error(req, "TypeError", &message);
    threw(req, frame, &thrown)
}

/// `ZEND_CALLABLE_CONVERT` (`Z/Zend/zend_vm_def.h:9725`), the `(...)`
/// of a first-class callable: the call the `INIT_*` before it began is
/// made a closure by [`crate::classes::closure_from_frame`] and never
/// served, and the call frame is let go, its receiver with it. So what
/// the `INIT_*` resolved is what the closure holds: a method's receiver
/// and its scope, the class a method was called on by `C::m(...)` as its
/// called scope, a private method reached from its own class, and a
/// closure called through `$f(...)` or `->__invoke(...)` itself.
///
/// ```php
/// class P {
///     private function hidden($x) { return "hidden$x"; }
///     function expose() { return $this->hidden(...); }
///     static function who() { return static::class; }
/// }
/// class C extends P { public $v = 1; function m() { return $this->v; } }
/// $c = new C;
/// $m = $c->m(...);
/// $who = C::who(...);
/// $f = function () { return 'f'; };
/// var_dump($m(), $c->expose()(1), $who(), $f(...) === $f, $f->__invoke(...) === $f);
/// var_dump($m, $who);
/// ```
/// ```expect
/// int(1)
/// string(7) "hidden1"
/// string(1) "C"
/// bool(true)
/// bool(true)
/// object(Closure)#2 (2) {
///   ["function"]=>
///   string(4) "C::m"
///   ["this"]=>
///   object(C)#1 (1) {
///     ["v"]=>
///     int(1)
///   }
/// }
/// object(Closure)#3 (1) {
///   ["function"]=>
///   string(6) "P::who"
/// }
/// ```
///
/// A name only `__call` or `__callStatic` answers is the trampoline the
/// `INIT_*` resolved, and the closure is `zend_closure_call_magic` over
/// it, which calls the magic method with the name, on the receiver or
/// on the class the call named:
///
/// ```php
/// class Foo {
///     public function __call($method, $args) { return "$method(" . implode(',', $args) . ")"; }
///     public static function __callStatic($method, $args) { return static::class . "::$method"; }
/// }
/// class Bar extends Foo {}
/// $foo = new Foo;
/// $i = $foo->anything(...);
/// $s = Foo::anythingStatic(...);
/// $b = Bar::anythingStatic(...);
/// var_dump($i(1, 2), $s(), $b());
/// ```
/// ```expect
/// string(13) "anything(1,2)"
/// string(19) "Foo::anythingStatic"
/// string(19) "Bar::anythingStatic"
/// ```
///
/// What the `INIT_*` refuses stands, since the `CALLABLE_CONVERT` is
/// never reached:
///
/// ```php
/// class T { private static function p() {} function i() {} }
/// foreach ([fn () => does_not_exist(...), fn () => T::p(...), fn () => T::i(...), fn () => (new stdClass)->nope(...)] as $make) {
///     try { $make(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// Call to undefined function does_not_exist()
/// Call to private method T::p() from global scope
/// Non-static method T::i() cannot be called statically
/// Call to undefined method stdClass::nope()
/// ```
pub(crate) fn callable_convert<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(call) = req.building_call().and_then(|call| req.frame_at(call)) else {
        return Err(Fatal::error(
            "InternalError: ZEND_CALLABLE_CONVERT with no call frame under it",
        ));
    };
    let callee = match call.closure() {
        Some(closure) => crate::classes::CallFrame::Closure(closure),
        None => crate::classes::CallFrame::Callee {
            function: call.callee(),
            this: call.this(),
            called_scope: call.called_scope(),
        },
    };
    let made = crate::classes::closure_from_frame(req, callee);
    // `OBJ_RELEASE(Z_OBJ(call->This))` where `ZEND_CALL_RELEASE_THIS`,
    // and `zend_vm_stack_free_call_frame`.
    req.pop_call();
    let made = match made {
        Ok(made) => made,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    set_out(req, frame, code, op, made.as_value());
    Ok(advance(req, frame))
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
///
/// A function served from outside the loop while the loop stood in
/// user code with no native above it, an error handler a handler's
/// warning reached or an autoloader a class fetch reached, names that
/// frame as its call site and verifies under its regime, since php's
/// `prev_execute_data` is the frame `EG(current_execute_data)` was; one
/// a native served names none ([`exec::Caller`]).
///
/// ```php
/// declare(strict_types=1);
/// set_error_handler(function (string $no, string $m) { return true; });
/// try { echo $u; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// set_error_handler(null);
/// try { array_map(function (int $x) {}, ["a"]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// spl_autoload_register(function (int $c) {});
/// try { new Nope; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// {closure:%s:3}(): Argument #1 ($no) must be of type string, int given, called in %s on line 4
/// {closure:%s:6}(): Argument #1 ($x) must be of type int, string given
/// {closure:%s:7}(): Argument #1 ($c) must be of type int, string given, called in %s on line 8
/// ```
///
/// It is `zend_argument_type_error`'s, so the type's string where it
/// names more than one, a method's member string and the two printed
/// buffers stand across the throw, and it peaks where php's does:
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
/// function u(int $x) {}
/// class K { function m(int $x) {} static function s(int|string $x) {} }
/// $u = new ReflectionFunction('u'); $m = new ReflectionMethod('K', 'm'); $s = new ReflectionMethod('K', 's'); $k = new K;
/// pk(function () use ($u) { try { $u->invoke("x"); } catch (TypeError $e) {} });
/// pk(function () use ($m, $k) { try { $m->invoke($k, "x"); } catch (TypeError $e) {} });
/// pk(function () use ($s) { try { $s->invoke(null, []); } catch (TypeError $e) {} });
/// ```
/// ```expect
/// 3496 32
/// 3544 0
/// 3584 0
/// ```
fn arg_type_error(
    req: &mut Request,
    frame: FrameRef,
    number: u32,
    ty: &php_unit::decl::Type,
    argument: &Value,
) -> Thrown {
    let function = req.vm().at(frame).and_then(|state| state.function);
    // `get_active_function_or_method_name`, which `%s` prints up to the
    // NUL an anonymous class's name holds: its class alone.
    let name = function.map_or_else(String::new, |function| match req.method_class(function) {
        Some(class) if req.class_name_bytes(class).contains(&0) => req.class_name(class),
        _ => req.frame_function_name(frame),
    });
    // The error over an argument a variadic collected is `Argument #2`
    // bare: php names one up to the declared count alone.
    let parameter = crate::api::raising::arg_name_suffix(req, function, number);
    let declared = resolved_type_name(req, frame, ty);
    let given = crate::ops::value_name(req, &argument.undef_as_null());
    let tail = match call_site(req, frame) {
        Some((file, line)) => {
            format!("must be of type {declared}, {given} given, called in {file} on line {line}")
        }
        None => format!("must be of type {declared}, {given} given"),
    };
    let message = format!("{name}(): Argument #{number}{parameter} {tail}");
    // `zend_type_to_string_resolved`'s `need_msg`, a string made for
    // a type of more than one name, and `zend_argument_type_error`'s
    // member string for a method, stand across the throw.
    let need = declared
        .contains(['|', '&', '?'])
        .then_some(declared.as_str());
    let member = req.frame_function_name(frame);
    let member = member.contains("::").then_some(member.as_str());
    crate::api::raising::under_name(req, need, |req| {
        crate::api::raising::throw_argument_error(req, member, "TypeError", &tail, &message)
    })
}

/// `EX(prev_execute_data)` as php's two argument diagnostics read it,
/// as a file and a line: the frame whose opline pushed the call to
/// `frame`, and nothing where a native pushed it or where `frame` is
/// the one `serve` pushed. Both `zend_missing_arg_error` and
/// `zend_verify_arg_error` drop the clause that names the call site
/// where this is nothing. A frame a native pushed from an attribute's
/// site ([`exec::Site`]) names the site.
fn call_site(req: &Request, frame: FrameRef) -> Option<(String, u32)> {
    let Some(caller) = req.vm().at(frame)?.caller().frame() else {
        // A call a native served from an attribute's site, which is
        // user code (`zend_get_attribute_object`'s dummy frame).
        return req
            .site_below(frame)
            .map(|site| (String::from_utf8_lossy(&site.file).into_owned(), site.line));
    };
    let caller = req.frame_at(caller)?;
    Some((
        caller
            .file()
            .map(|file| String::from_utf8_lossy(file).into_owned())
            .unwrap_or_default(),
        caller.line(),
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
    let name = req.frame_function_name(frame);
    let parameters: Vec<bool> = function
        .and_then(|function| req.function_decl(function))
        .map(|decl| {
            decl.parameters
                .iter()
                .map(php_unit::decl::Parameter::is_required)
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
    crate::api::raising::throw_error(req, "ArgumentCountError", &message)
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
        let default = match exec::default_value(req, code, ConstIdx(op.op2)) {
            Ok(default) => default,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        let evaluated = default.1;
        req.replace_slot(frame, op.out, default.0);
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
    let object = match crate::api::call::new_object_made(req, class) {
        Ok(object) => object,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    set_out(req, frame, code, op, object.as_value());
    let count = usize::try_from(op.extended_value).unwrap_or(0);
    // `Z_OBJ_HT_P(result)->get_constructor`: the table's answer, which
    // is `zend_std_get_constructor`'s visibility check for a user class
    // and a refusal for a class php reserves, `Generator`'s.
    let constructor = match req.made(class).handlers.get_constructor(req, object) {
        Ok(constructor) => constructor,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    if constructor.is_none() {
        let at = req.vm().opline(frame);
        let next_is_do_fcall = exec::opline_at(code, at.saturating_add(1))
            .is_some_and(|next| next.opcode == php_unit::Opcode::DoFcall);
        if count == 0 && next_is_do_fcall {
            return Ok(advance_over_data(req, frame, at));
        }
    }
    let function = constructor.unwrap_or(Function::PASS);
    let this = constructor.map(|_| {
        req.addref(&object.as_value());
        object
    });
    let mut header = exec::Header::call(function, op.extended_value);
    header.set_receiver(this.map(exec::Receiver::Owned));
    header.set_called_scope(constructor.map(|_| class));
    pushed(req, frame, header)
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
/// The pair is read back only in the scope that wrote it: a trait's
/// method runs in the scope of each class that uses it, and a closure
/// in the scope it is bound to, over one op array and so one slot,
/// where php gives each scope a cache of its own. The private method
/// one scope may call is the access rule's `Error` from the next.
///
/// ```php
/// trait T { function call() { return A::p(); } }
/// class A { use T; private static function p() { return "A::p"; } }
/// class B { use T; }
/// foreach ([new A, new B, new A] as $by) {
///     try { echo $by->call(), "\n"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// $f = function () { return A::p(); };
/// foreach ([Closure::bind($f, null, A::class), $f] as $g) {
///     try { echo $g(), "\n"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// A::p
/// Call to private method A::p() from scope B
/// A::p
/// A::p
/// Call to private method A::p() from global scope
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
                Err(thrown) => {
                    exec::free_operand(req, frame, code, op.op2_type, op.op2);
                    return threw(req, frame, &thrown);
                }
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
    // The pair answers where it holds this pass's class and was written
    // from this frame's scope ([`Request::cached_callee`]).
    let cached_callee = match (op.op2_type, primed) {
        (OperandKind::Const, Cached::ClassAndCallee { .. }) => {
            req.cached_callee(frame, slot, class)
        }
        _ => None,
    };
    if let Some((function, instance)) = cached_callee {
        return static_call_frame(req, frame, op, class, (function, instance));
    }
    let mut uncached = false;
    // Whether the callee takes `$this`, where the lookup read it.
    let mut looked_up = None;
    let function = match op.op2_type {
        OperandKind::Unused => {
            let Some(constructor) = req.constructor(class) else {
                let thrown =
                    crate::api::raising::throw_error(req, "Error", "Cannot call constructor");
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
                let thrown = crate::api::raising::throw_error(
                    req,
                    "Error",
                    &format!("Cannot call private {name}::__construct()"),
                );
                return threw(req, frame, &thrown);
            }
            constructor
        }
        OperandKind::Const => {
            let written = exec::constant(code, ConstIdx(op.op2));
            let lowered = exec::constant(code, ConstIdx(op.op2.saturating_add(1)));
            let this = req.vm().this(frame);
            match static_method(
                req,
                class,
                lowered.as_str_bytes().unwrap_or_default(),
                &written,
                this,
            ) {
                Ok(callee) => {
                    uncached = callee.uncached;
                    looked_up = Some(callee.takes_this);
                    callee.function
                }
                Err(thrown) => return threw(req, frame, &thrown),
            }
        }
        _ => return static_call_named(req, frame, code, op, class),
    };
    // `CACHE_POLYMORPHIC_PTR(opline->result.num, ce, fbc)`, where the
    // callee was named by a constant: after the lookup and its access
    // rule, and before the rule that the callee is no instance method,
    // which php applies on the cached pass too. php holds the write off
    // a callee reached through a trampoline, one marked
    // `ZEND_ACC_NEVER_CACHE` and one a trait declared, whose deprecation
    // is raised on every pass.
    let instance = looked_up.unwrap_or_else(|| req.takes_this(function));
    if op.op1_type == OperandKind::Const && op.op2_type == OperandKind::Const && !uncached {
        req.prime_callee(frame, slot, class, (function, instance));
    }
    static_call_frame(req, frame, op, class, (function, instance))
}

/// `zend_std_get_static_method` (`Z/Zend/zend_object_handlers.c:2040`)
/// over a name the program wrote: the method `class` has under the
/// lowered name where the scope may call it, the trampoline
/// `get_static_method_fallback` answers where the class has none or the
/// scope may not call it ([`Request::static_trampoline`], over `this`,
/// the calling frame's receiver), and php's `Error` where neither
/// serves, `Call to undefined method` or the access rule's; then the
/// end [`Request::static_method_found`] holds a method to. The `bool`
/// is whether php keeps the callee out of the cache slot: one
/// [`Request::never_cached`] names, and a method named on a trait.
fn static_method(
    req: &mut Request,
    class: crate::api::values::ClassId,
    lowered: &[u8],
    written: &Value,
    this: Option<PhpObject>,
) -> Result<StaticCallee, Thrown> {
    let name = written.as_str_bytes().unwrap_or_default();
    if let Some(function) = req
        .handlers_of(class)
        .get_static_method(req, class, lowered, name)?
    {
        return Ok(StaticCallee {
            function,
            uncached: true,
            takes_this: req.takes_this(function),
        });
    }
    let function = match req.method_named(class, lowered) {
        Some(function) if req.method_accessible(function) => function,
        Some(function) => {
            if let Some(trampoline) = req.static_trampoline(class, name, this) {
                trampoline
            } else {
                req.check_method_access(function, name)?;
                function
            }
        }
        None => match req.static_trampoline(class, name, this) {
            Some(trampoline) => trampoline,
            None => {
                return Err(undefined_method(
                    req,
                    class,
                    written.as_str_bytes().unwrap_or_default(),
                ));
            }
        },
    };
    let never_cached = req.never_cached(function);
    let found = req.static_method_found(function)?;
    Ok(StaticCallee {
        function,
        uncached: never_cached || found.of_trait,
        takes_this: found.takes_this,
    })
}

/// The callee [`static_method`] found, whether php keeps it out of the
/// cache slot, and whether it takes `$this`, each read once.
#[derive(Debug, Clone, Copy)]
struct StaticCallee {
    function: Function,
    uncached: bool,
    takes_this: bool,
}

/// `INIT_STATIC_METHOD_CALL` over a method the program names, once the
/// class is resolved: the name held to be a string
/// ([`exec::MethodName`]), then the method found as
/// `zend_std_get_static_method` finds it with no key, with its access
/// rule and a trait's deprecation, and nothing cached, since the next
/// pass may name another.
fn static_call_named(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    class: crate::api::values::ClassId,
) -> Edge {
    let method = match exec::MethodName::of(req, frame, code, op) {
        Ok(method) => method,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let this = req.vm().this(frame);
    let found = static_method(req, class, &method.lowered(), &method.written, this);
    let edge = match found {
        Ok(callee) => {
            static_call_frame(req, frame, op, class, (callee.function, callee.takes_this))
        }
        Err(thrown) => threw(req, frame, &thrown),
    };
    method.release(req);
    edge
}

/// What `INIT_STATIC_METHOD_CALL` does once it has its class and its
/// callee, whether the slot answered or the tables did: php's
/// `!(fbc->common.fn_flags & ZEND_ACC_STATIC)` rule, which is applied
/// on every pass and not only the one that resolved, and the call frame
/// over the class as its called scope. `callee` is the function with
/// whether it takes `$this`, which the cache slot holds beside it and
/// a pass that looked it up read once ([`Request::takes_this`]).
fn static_call_frame(
    req: &mut Request,
    frame: FrameRef,
    op: &Opline,
    class: crate::api::values::ClassId,
    (function, instance): (Function, bool),
) -> Edge {
    // `if (!(fbc->common.fn_flags & ZEND_ACC_STATIC))`: an instance
    // method takes the calling frame's `$this` where it is an instance
    // of the class, and is php's `Error` otherwise; a static one called
    // through `self` or `parent` forwards the calling frame's called
    // scope, `Z_OBJCE(EX(This))` or `Z_CE(EX(This))`.
    let receiver = req.vm().this(frame);
    let (this, called) = if instance {
        let Some(this) = receiver.filter(|this| req.instance_of(this.class(), class)) else {
            let thrown = non_static_method_call(req, function);
            return threw(req, frame, &thrown);
        };
        // `ZEND_CALL_HAS_THIS` over the calling frame's own `$this`,
        // which that frame keeps: no `ZEND_CALL_RELEASE_THIS`.
        (Some(exec::Receiver::lent(this)), this.class())
    } else if op.op1_type == OperandKind::Unused
        && matches!(
            op.op1 & FETCH_CLASS_MASK,
            FETCH_CLASS_SELF | FETCH_CLASS_PARENT
        )
    {
        let forwarded = receiver
            .map(|this| this.class())
            .or_else(|| req.vm().at(frame).and_then(exec::Header::called_scope));
        (None, forwarded.unwrap_or(class))
    } else {
        (None, class)
    };
    let mut header = exec::Header::call(function, op.extended_value);
    header.set_receiver(this);
    header.set_called_scope(Some(called));
    pushed(req, frame, header)
}

/// `ZEND_INIT_PARENT_PROPERTY_HOOK_CALL` (`Z/Zend/zend_vm_def.h:9886`),
/// `parent::$name::get()` and `parent::$name::set($v)` inside a hook:
/// the parent of the running hook's class, the property it holds under
/// the name in `op1`, and the hook of the kind in `op2` called on the
/// hook's own `$this`, or, where the parent's property has none, the
/// trampoline php makes, which reads or writes the property as the
/// parent's hook would have found it ([`Request::parent_hook_trampoline`]).
///
/// ```php
/// class P { public $x = 2; }
/// class C extends P { public $x = 2 { get => parent::$x::get() * 3; set { parent::$x::set($value + 1); } } }
/// $c = new C;
/// var_dump($c->x);
/// $c->x = 4;
/// var_dump($c->x);
/// class Q { public $p { get => parent::$p::get(); } }
/// try { (new Q)->p; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// class R extends P { public $y { get => parent::$y::get(); } }
/// try { (new R)->y; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(6)
/// int(15)
/// Cannot use "parent" when current class scope has no parent
/// Undefined property P::$y
/// ```
pub(crate) fn init_parent_property_hook_call<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let scope = req
        .vm()
        .at(frame)
        .and_then(|state| state.function)
        .and_then(|function| req.method_class(function));
    let Some(parent) = scope.and_then(|scope| req.class_parent(scope)) else {
        let thrown = crate::api::raising::throw_error(
            req,
            "Error",
            "Cannot use \"parent\" when current class scope has no parent",
        );
        return threw(req, frame, &thrown);
    };
    let name = exec::constant(code, ConstIdx(op.op1));
    let name = name.as_str_bytes().unwrap_or_default().to_vec();
    let shown = String::from_utf8_lossy(&name).into_owned();
    let parent_name = req.class_name(parent);
    let Some((declaring, property)) = req.property_info_of(parent, &name) else {
        let thrown = crate::api::raising::throw_error(
            req,
            "Error",
            &format!("Undefined property {parent_name}::${shown}"),
        );
        return threw(req, frame, &thrown);
    };
    if property.visibility == php_unit::decl::Visibility::Private {
        let thrown = crate::api::raising::throw_error(
            req,
            "Error",
            &format!("Cannot access private property {parent_name}::${shown}"),
        );
        return threw(req, frame, &thrown);
    }
    let kind = if op.op2 == 0 {
        php_unit::decl::HookKind::Get
    } else {
        php_unit::decl::HookKind::Set
    };
    let hook = req.row_hooked(parent, &name).and_then(|hooked| match kind {
        php_unit::decl::HookKind::Get => hooked.get,
        php_unit::decl::HookKind::Set => hooked.set,
    });
    // The trampoline's scope is the property's class, which its call
    // carries as its called scope; a hook is called on `$this` as any
    // method is.
    let (function, trampoline) = match hook {
        Some(function) => (function, false),
        None => (req.parent_hook_trampoline(kind), true),
    };
    // `INIT_PARENT_PROPERTY_HOOK_CALL`: `ZEND_CALL_HAS_THIS` over the
    // frame's own `$this`, which the frame keeps, and no
    // `ZEND_CALL_RELEASE_THIS`.
    let this = req.vm().this(frame);
    let receiver = this.map(exec::Receiver::lent);
    // The trampoline's name, `$name::get`, which php makes with the
    // trampoline (`zend_get_property_hook_trampoline`); a hook of its
    // own needs none.
    let written = if trampoline {
        let written = format!("${shown}::{}", kind.name());
        match req.string(written.as_bytes()) {
            Ok(written) => Some(written),
            Err(thrown) => return threw(req, frame, &thrown),
        }
    } else {
        None
    };
    let mut header = exec::Header::call(function, op.extended_value);
    header.set_receiver(receiver);
    header.set_called_scope(Some(match this {
        Some(this) if !trampoline => this.class(),
        _ => declaring,
    }));
    let call = req.push_call(header)?;
    // The trampoline reads the property off the name it was built
    // under, php's `internal_function.reserved[0]`; a hook's own body
    // has no use for it.
    if let Some(written) = written {
        match req.annex_mut(call) {
            Some(annex) => annex.name = Some(written),
            None => req.release(Value::string(written)),
        }
    }
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
///
/// A static method called through an object gives the frame's owner of
/// the receiver up before the call, so a temporary receiver is
/// destructed first, and the called scope is its class, read while it
/// stood; a destructor that throws is the throw, and the call is not
/// made.
///
/// ```php
/// class A {
///     static function name() { return 'A'; }
///     static function baz() {
///         $fn = function () { return static::name(); };
///         echo static::name(), ' ', $fn(), "\n";
///     }
///     function __destruct() { echo "destructed\n"; }
/// }
/// class B extends A { static function name() { return 'B'; } }
/// (new B)->baz();
/// class T { static function s() { echo "called\n"; } function __destruct() { throw new Exception('in dtor'); } }
/// try { (new T)->s(); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// destructed
/// B B
/// in dtor
/// ```
///
/// A method the program names is looked up on the first pass on each
/// class, and read back from the opline's cache slot on every pass
/// after on the same class from the same scope ([`method_of_call`]).
/// The scope is part of what the slot answers for: a trait's method
/// runs in the scope of each class that uses it and a closure in the
/// one it is bound to, over one op array, and the callee one scope
/// reaches, a private method or the private one standing in for a
/// child's public one, is the access rule's `Error` or the child's
/// method from another.
///
/// ```php
/// trait T { function call($o) { return $o->m(); } }
/// class P { use T; private function m() { return "P::m"; } }
/// class C extends P { public function m() { return "C::m"; } }
/// class D { use T; }
/// class A { private function m() { return "A::m"; } }
/// foreach ([[new P, new C], [new D, new C], [new P, new C], [new D, new A]] as [$by, $on]) {
///     try { echo $by->call($on), "\n"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// $f = function ($o) { return $o->m(); };
/// foreach ([Closure::bind($f, null, A::class), $f] as $g) {
///     try { echo $g(new A), "\n"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// P::m
/// C::m
/// P::m
/// Call to private method A::m() from scope D
/// A::m
/// Call to private method A::m() from global scope
/// ```
///
/// What a lookup answers for the object and not for its class is never
/// held: `__call`'s trampoline, a closure's `__invoke`, the method an
/// iterator hands on to the one it wraps, and a method a `PDO`
/// connection's connector lays on it.
///
/// ```php
/// class M { function __call($n, $a) { return "__call $n"; } }
/// class N extends M { function m() { return "N::m"; } }
/// foreach ([new M, new N, new M] as $o) { echo $o->m(), "\n"; }
/// foreach ([function () { return 1; }, fn() => 2] as $f) { echo $f->__invoke(), "\n"; }
/// class One extends ArrayIterator { function hello() { return "One"; } }
/// class Two extends ArrayIterator { function hello() { return "Two"; } }
/// foreach ([new One([]), new Two([])] as $in) { echo (new IteratorIterator($in))->hello(), "\n"; }
/// $a = new PDO('sqlite::memory:');
/// $b = (new ReflectionClass('PDO'))->newInstanceWithoutConstructor();
/// foreach ([$a, $b] as $p) {
///     try { var_dump(@$p->sqliteCreateFunction('f', 'strlen')); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// __call m
/// N::m
/// __call m
/// 1
/// 2
/// One
/// Two
/// bool(true)
/// Call to undefined method PDO::sqliteCreateFunction()
/// ```
///
/// A primed slot answers without the class's `get_method`, so a class
/// whose handler refuses an object in a state refuses it only until a
/// pass on another object of the class has primed the slot, in php as
/// here: an iterator whose constructor never ran is refused on the
/// first pass and called on the third.
///
/// ```php
/// class Lazy extends RecursiveIteratorIterator {
///     function __construct($init) { if ($init) parent::__construct(new RecursiveArrayIterator([])); }
///     function bar() { return "bar"; }
/// }
/// foreach ([new Lazy(false), new Lazy(true), new Lazy(false)] as $it) {
///     try { echo $it->bar(), "\n"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// The Lazy instance wasn't initialized properly
/// bar
/// bar
/// ```
pub(crate) fn init_method_call<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if let Some(edge) = inline_init_method_call(req, frame, code, op) {
        return edge;
    }
    method_call(req, frame, code, op)
}

/// The inline path of `ZEND_INIT_METHOD_CALL` over a constant name
/// (`ZEND_INIT_METHOD_CALL_SPEC_CV_CONST`, `_TMPVAR_CONST` and
/// `_UNUSED_CONST`, `Z/Zend/zend_vm_def.h:3778`): the receiver an
/// object a CV or a temporary holds, or the frame's `$this`; the callee
/// off the cache slot `result.num` names where the slot holds the
/// receiver's class and was written from this frame's scope
/// (`CACHED_PTR(opline->result.num) == called_scope`), and takes
/// `$this`; and the call pushed over the receiver with the class as its
/// called scope, the CV's object with one more owner (`GC_ADDREF`), the
/// temporary's moved, and `$this` lent. Nothing here raises but the
/// memory limit on a page the push takes. `None`, with nothing touched,
/// for a name the program computes, a slot not primed for the class
/// and scope, a static callee, and a receiver that is no object or is
/// one through a reference, which [`method_call`] serves. Out of the
/// loop's own body, a call of its own, since inlined it costs every
/// other opline of the loop registers.
#[inline(never)]
fn inline_init_method_call(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Option<Edge> {
    if op.op2_type != OperandKind::Const {
        return None;
    }
    let at = match op.op1_type {
        OperandKind::Unused => None,
        OperandKind::Cv | OperandKind::Tmp | OperandKind::Var => {
            Some(ByKind(op.op1_type).slot(code, op.op1)?)
        }
        OperandKind::Const => return None,
    };
    let object = match at {
        None => req.vm().this(frame)?,
        Some(at) => PhpObject::of(req.slots_mut(frame).get(at)?.as_obj()?),
    };
    let class = object.class();
    let (function, instance) = req.cached_callee(frame, CacheSlot(op.out), class)?;
    if !instance {
        return None;
    }
    let (room, callee, sent) = req.call_room(function, op.extended_value);
    let mut header = exec::Header::call(function, op.extended_value);
    header.callee = callee;
    header.info.join(sent);
    header.set_called_scope(Some(class));
    header.set_receiver(Some(match at {
        None => exec::Receiver::lent(object),
        Some(_) => exec::Receiver::Owned(object),
    }));
    if let Err(fatal) = req.push_room(room, header) {
        return Some(Err(fatal));
    }
    match (op.op1_type, at) {
        // `GC_ADDREF(obj)`: the receiver is an object, so its own
        // header's count and nothing that asks its type.
        (OperandKind::Cv, _) => object.heap().addref(),
        (OperandKind::Tmp | OperandKind::Var, Some(at)) => {
            if let Some(slot) = req.slots_mut(frame).get_mut(at) {
                *slot = Zval::UNDEF;
            }
        }
        _ => {}
    }
    Some(Ok(advanced()))
}

/// `INIT_METHOD_CALL` past its inline path: the name, the receiver and
/// the callee looked up or read off the slot, and the call pushed. A
/// call of its own, so that the inline path before it keeps to the
/// registers it needs.
#[inline(never)]
fn method_call(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    // A method the program names is held to be a string before the
    // receiver is read ([`exec::MethodName`]).
    let method = match exec::MethodName::of(req, frame, code, op) {
        Ok(method) => method,
        Err(thrown) => {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            return threw(req, frame, &thrown);
        }
    };
    let receiver = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    let found = match receiver.as_object().copied() {
        Some(object) => {
            method_of_call(req, frame, op, object, &method).map(|found| (object, found))
        }
        None => Err(crate::api::raising::throw_error(
            req,
            "Error",
            &format!(
                "Call to a member function {}() on {}",
                String::from_utf8_lossy(method.written.as_str_bytes().unwrap_or_default()),
                crate::ops::value_name(req, &receiver)
            ),
        )),
    };
    let (object, (function, receiver, instance)) = match found {
        Ok(found) => found,
        Err(thrown) => {
            method.release(req);
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            return threw(req, frame, &thrown);
        }
    };
    method.release(req);
    // `$this->m()`, `INIT_METHOD_CALL` over `UNUSED`: the calling frame
    // keeps its `$this`, and the call takes no owner of it (no
    // `ZEND_CALL_RELEASE_THIS`), the object lent, unless the class's
    // `method_receiver` handed another over.
    let lends = op.op1_type == OperandKind::Unused && receiver.is_none();
    // The frame's owner of the receiver otherwise: the temporary's own,
    // moved, or one more of the CV's or the constant's; a reference the
    // slot held is followed and given up.
    if !lends {
        take_receiver(req, frame, code, op, object);
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
    // `called_scope = obj->ce`, read while the frame's owner stands: a
    // static method gives that owner up here, and the receiver it was
    // may go with it, its destructor served before the call.
    let called = object.class();
    let this = if instance {
        Some(if lends {
            exec::Receiver::lent(object)
        } else {
            exec::Receiver::Owned(object)
        })
    } else if lends {
        None
    } else {
        req.release(object.as_value());
        if req.is_pending() {
            return threw(req, frame, &Thrown::of(None));
        }
        None
    };
    let mut header = exec::Header::call(function, op.extended_value);
    header.set_receiver(this);
    header.set_called_scope(Some(called));
    pushed(req, frame, header)
}

/// The call's owner of `object`, the receiver `INIT_METHOD_CALL` read
/// off `op1`: a `CV` that holds the object takes one more on the
/// object's own header (`GC_ADDREF`), and a `TMP` or a `VAR` that holds
/// it is moved, its slot left `UNDEF`, as php's handler moves it. A
/// slot that holds it through a reference is taken as
/// [`exec::take_operand`] takes it, the reference followed and given
/// up and one more owner of the object taken.
///
/// ```php
/// class C { public $n = 0; function m() { return ++$this->n; } function __destruct() { echo "d\n"; } }
/// $m = "m"; $o = new C; $r = &$o;
/// var_dump($o->$m(), $r->$m(), (new C)->$m());
/// echo "end\n";
/// ```
/// ```expect
/// d
/// int(1)
/// int(2)
/// int(1)
/// end
/// d
/// ```
fn take_receiver(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    object: PhpObject,
) {
    let at = match op.op1_type {
        OperandKind::Cv => Some(op.op1 as usize),
        OperandKind::Tmp | OperandKind::Var => Some(exec::temporary_slot(code, op.op1) as usize),
        OperandKind::Const | OperandKind::Unused => None,
    };
    if let Some(slot) = at.and_then(|at| req.slots_mut(frame).get_mut(at))
        && slot
            .as_obj()
            .is_some_and(|held| PhpObject::of(held) == object)
    {
        if op.op1_type == OperandKind::Cv {
            object.heap().addref();
        } else {
            *slot = Zval::UNDEF;
        }
        return;
    }
    let taken = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    if taken.as_object().is_none() {
        req.addref(&object.as_value());
        req.release(taken);
    }
}

/// The callee of an `INIT_METHOD_CALL` on `object`, and the receiver
/// its class handed the call to where one did: php's
/// `CACHED_PTR(opline->result.num) == called_scope` first, where `op2`
/// is a constant, and [`method_on`] where the slot is unprimed or holds
/// another class or was written from another scope
/// ([`Cached::ClassAndCallee`]). The answer is written back, the
/// receiver's class, the frame's scope and the callee together
/// (`CACHE_POLYMORPHIC_PTR`, `Z/Zend/zend_vm_def.h:3694`), only where
/// `op2` is a constant, the callee is no trampoline and no
/// `ZEND_ACC_NEVER_CACHE` method ([`Request::never_cached`]), and the
/// object itself answered, `obj == orig_obj`. A primed slot answers
/// without asking the class's `get_method`, so a handler that refuses
/// an object in a state, as an iterator whose constructor never ran, is
/// not asked again for another object of the class, as php's is not.
fn method_of_call(
    req: &mut Request,
    frame: FrameRef,
    op: &Opline,
    object: PhpObject,
    method: &exec::MethodName,
) -> Result<(Function, Option<PhpObject>, bool), Thrown> {
    let slot = CacheSlot(op.out);
    let class = object.class();
    let named = op.op2_type == OperandKind::Const;
    if named && let Some((function, instance)) = req.cached_callee(frame, slot, class) {
        return Ok((function, None, instance));
    }
    let (function, receiver) = method_on(
        req,
        object,
        &method.lowered(),
        &method.written,
        Undefined::OnCalled,
    )?;
    let instance = req.takes_this(function);
    if named && receiver.is_none() && !req.never_cached(function) {
        req.prime_callee(frame, slot, class, (function, instance));
    }
    Ok((function, receiver, instance))
}

/// Which object `Call to undefined method X::m()` names where no class
/// answers a method: the one called on, `orig_obj` in
/// `INIT_METHOD_CALL`, or the last one its class handed the call to,
/// the object `get_method(&object, …)` left in
/// `zend_init_dynamic_call_array`.
#[derive(Clone, Copy)]
enum Undefined {
    OnCalled,
    OnHanded,
}

/// `obj->handlers->get_method(&obj, name, key)` and what
/// `INIT_METHOD_CALL` and `INIT_DYNAMIC_CALL` hold the answer to: the
/// method by its lowered name on the object, the trampoline of its
/// class's `__call` where the class has no such method
/// (`zend_std_get_method`), or the method on the receiver its class
/// hands the call to instead ([`Request::method_found`]), so a chain of
/// outer iterators hands an unknown method on to the innermost that has
/// it; `Call to undefined method X::m()`, naming the object `undefined`
/// says, where none answers. A method the object's own class answers
/// that the scope may not call is `__call`'s too where the class has
/// one, and `zend_std_get_method`'s access rule's `Error` where it has
/// none; one a receiver answers is called whatever its visibility, as
/// `spl_dual_it_get_method` finds it in the inner class's function
/// table with no rule. A class's handler that refuses the call raises
/// here.
///
/// ```php
/// class Deep extends ArrayIterator { function deep() { return get_class($this); } }
/// $it = new CachingIterator(new LimitIterator(new IteratorIterator(new Deep([1, 2])), 0, 1));
/// echo $it->deep(), "\n";
/// ```
/// ```expect
/// Deep
/// ```
///
/// ```php
/// class Plain extends ArrayIterator { private function priv() { return "priv ran"; } }
/// $b = new IteratorIterator(new IteratorIterator(new Plain([])));
/// foreach ([fn () => $b->priv(), fn () => [$b, 'priv'](), fn () => $b->nope(), fn () => [$b, 'nope']()] as $f) {
///     try { echo $f(), "\n"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// priv ran
/// priv ran
/// Call to undefined method IteratorIterator::nope()
/// Call to undefined method Plain::nope()
/// ```
fn method_on(
    req: &mut Request,
    object: PhpObject,
    lowered: &[u8],
    written: &Value,
    undefined: Undefined,
) -> Result<(Function, Option<PhpObject>), Thrown> {
    // The lookup a primed slot spares, counted where the tests read it
    // ([`exec::Vm::lookups`]).
    #[cfg(test)]
    {
        req.vm_mut().lookups = req.vm_mut().lookups.saturating_add(1);
    }
    let bytes = written.as_str_bytes().unwrap_or_default();
    let (found, receiver) = req.method_found(object, lowered, bytes)?;
    let Some(function) = found else {
        let named = match undefined {
            Undefined::OnCalled => object,
            Undefined::OnHanded => receiver.unwrap_or(object),
        };
        return Err(undefined_method(
            req,
            named.class(),
            written.as_str_bytes().unwrap_or_default(),
        ));
    };
    if receiver.is_some() || req.method_accessible(function) {
        return Ok((function, receiver));
    }
    // The scope may not call it: `__call` takes the call where the class
    // has one, and otherwise the access rule's `Error`.
    let function = req
        .call_trampoline(object.class(), bytes, false)
        .unwrap_or(function);
    req.check_method_access(function, bytes)?;
    Ok((function, None))
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
        let thrown = crate::api::raising::throw_error(
            req,
            "Error",
            "Using $this when not in object context",
        );
        return threw(req, frame, &thrown);
    };
    let value = this.as_value();
    req.addref(&value);
    set_out(req, frame, code, op, value);
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

/// `ZEND_FETCH_UNSET` (`Z/Zend/zend_vm_def.h:1848`), the base of
/// `unset($_SERVER['k'])` and `unset($$x['k'])`: [`fetch_by_name`] under
/// `BP_VAR_UNSET`, a name the table holds handed out as the place the
/// `UNSET_DIM` after it deletes from, and a name it has not, or whose
/// CV holds nothing, `null` in silence, nothing laid down.
///
/// ```php
/// $_SERVER['k'] = 1;
/// $_SERVER['l'] = 2;
/// unset($_SERVER['k']);
/// $n = 'a';
/// $a = ['x' => 1, 'y' => 2];
/// $m = 'none';
/// unset($$n['x'], $$n['nope'], $$m['z']);
/// var_dump(isset($_SERVER['k']), $_SERVER['l'], $a, isset($none));
/// ```
/// ```expect
/// bool(false)
/// int(2)
/// array(1) {
///   ["y"]=>
///   int(2)
/// }
/// bool(false)
/// ```
pub(crate) fn fetch_unset<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    fetch_by_name(req, frame, code, op, ByName::Unset)
}

/// `ZEND_FETCH_FUNC_ARG` (`Z/Zend/zend_vm_def.h:1840`), a variable by
/// name under the chain sent to a callee the compiler knew nothing of:
/// [`fetch_by_name`] under `BP_VAR_W` where the `CHECK_FUNC_ARG` before
/// it found the parameter takes its argument by reference, and under
/// `BP_VAR_R` otherwise.
///
/// ```php
/// $x = 'a';
/// $a = ['k' => [2, 1]];
/// fill($$x['new']);
/// var_dump($a['new'], byval(${'a'}['k'][0]), byval($$x['none']));
/// function fill(&$v) { $v = 'set'; }
/// function byval($v) { return $v; }
/// ```
/// ```expect
/// Warning: Undefined array key "none" in %s on line 5
/// string(3) "set"
/// int(2)
/// NULL
/// ```
pub(crate) fn fetch_func_arg<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let mode = if sends_arg_by_ref(req) {
        ByName::W
    } else {
        ByName::R
    };
    fetch_by_name(req, frame, code, op, mode)
}

/// `ZEND_FETCH_GLOBALS` (`Z/Zend/zend_vm_def.h:9229`), `$GLOBALS`: a
/// copy of `EG(symbol_table)` into the temporary
/// ([`Request::symbol_table_array`]), so what the program writes into it
/// is its own and never a variable, the table's names in the table's
/// order, each auto-global first, a numeric name an integer key, and a
/// reference two variables share kept as one.
///
/// ```php
/// $a = 1; $GLOBALS['b'] = 2;
/// var_dump($GLOBALS['a'], $b, isset($GLOBALS['c']), array_key_exists('a', $GLOBALS));
/// unset($GLOBALS['a']);
/// var_dump(isset($a));
/// var_dump(array_slice(array_keys($GLOBALS), 0, 7));
/// $GLOBALS['7'] = 'seven';
/// $r = 1; $ref = &$r;
/// $g = $GLOBALS;
/// var_dump(array_key_last($g), $g['r'], $g['ref']);
/// $g['r'] = 2;
/// var_dump($r);
/// ```
/// ```expect
/// int(1)
/// int(2)
/// bool(false)
/// bool(true)
/// bool(false)
/// array(7) {
///   [0]=>
///   string(4) "argv"
///   [1]=>
///   string(4) "argc"
///   [2]=>
///   string(4) "_GET"
///   [3]=>
///   string(5) "_POST"
///   [4]=>
///   string(7) "_COOKIE"
///   [5]=>
///   string(6) "_FILES"
///   [6]=>
///   string(7) "_SERVER"
/// }
/// int(7)
/// int(1)
/// int(1)
/// int(2)
/// ```
pub(crate) fn fetch_globals<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let array = match req.symbol_table_array() {
        Ok(array) => array,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    set_out(req, frame, code, op, Value::array(array));
    Ok(advance(req, frame))
}

/// `ZEND_ISSET_ISEMPTY_VAR` (`Z/Zend/zend_vm_def.h:7481`), `isset($$x)`,
/// `isset($_SERVER)` and `empty(${'a'})`: the name read as
/// `zval_get_tmp_string` reads it ([`isset_name`]), the table
/// `zend_get_target_symbol_table` names, and the bucket asked, never
/// fetched: under `isset` that it stands and holds no `null`, a
/// reference followed, and under `ZEND_ISEMPTY` that it is absent or
/// `i_zend_is_true` says no. A CV attached to the name that holds
/// nothing answers as an absent name.
///
/// ```php
/// $a = 1; $z = 0; $nn = null;
/// $names = ['a', 'z', 'nn', 'u'];
/// foreach ($names as $name) { var_dump(isset($$name), empty($$name)); }
/// var_dump(isset($_SERVER), empty($_FILES), isset($_GET), empty($_POST));
/// function f() { $x = 1; $k = 'x'; $u = 'y'; var_dump(isset($$k), isset($$u), empty($$u)); }
/// f();
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
pub(crate) fn isset_isempty_var<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let name = match isset_name(req, frame, code, op) {
        Ok(name) => name,
        Err(thrown) => {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            return threw(req, frame, &thrown);
        }
    };
    let table = if op.extended_value & FETCH_GLOBAL != 0 {
        req.global_table()
    } else {
        req.rebuild_symbol_table(frame)
    };
    let held = req.symbol_value(table, &name);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    let answer = match held {
        None => op.extended_value & ISEMPTY != 0,
        Some(value) if op.extended_value & ISEMPTY == 0 => crate::ops::is_set(&value),
        Some(value) => match crate::ops::is_true(req, &value) {
            Ok(truth) => !truth,
            Err(thrown) => return threw(req, frame, &thrown),
        },
    };
    smart_branch::<T, branch::Peeked>(req, frame, code, op, Ok(Value::bool(answer)))
}

/// The name an `ISSET_ISEMPTY_VAR` asks after,
/// `GET_OP1_ZVAL_PTR(BP_VAR_IS)` and `zval_get_tmp_string`: the constant
/// as it stands, a CV nothing wrote the empty string with no warning,
/// and anything else converted as `zval_get_string` converts it, an
/// object with no `__toString` php's `Error`.
fn isset_name(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Result<Vec<u8>, Thrown> {
    let held = exec::operand_undef(req, frame, code, op.op1_type, op.op1).deref();
    if held.tag() == TypeTag::Undef {
        return Ok(Vec::new());
    }
    if let Some(bytes) = held.as_str_bytes() {
        return Ok(bytes.to_vec());
    }
    let text = crate::ops::to_string(req, &held)?;
    let bytes = text.as_bytes().to_vec();
    req.release(Value::string(text));
    Ok(bytes)
}

/// `ZEND_UNSET_VAR` (`Z/Zend/zend_vm_def.h:6645`), `unset($$x)` and
/// `unset($_SERVER)`: the name read as [`fetch_by_name`] reads it, and
/// `zend_hash_del_ind` in the table `zend_get_target_symbol_table` names
/// ([`Request::symbol_unset`]): a bucket that holds its value is
/// deleted, and one attached to a CV leaves the CV holding nothing, the
/// name staying where it was.
///
/// ```php
/// $a = 1; $b = 2; $n = 'a';
/// unset($$n);
/// var_dump(isset($a), $b);
/// function f() { $x = 1; $k = 'x'; unset($$k); var_dump(isset($x)); $x = 2; var_dump($x); }
/// f();
/// $_SERVER['q'] = 1;
/// unset($_SERVER);
/// var_dump(isset($_SERVER));
/// class D { function __destruct() { echo "gone\n"; } }
/// $d = new D; $m = 'd'; unset($$m); echo "after\n";
/// ```
/// ```expect
/// bool(false)
/// int(2)
/// bool(false)
/// int(2)
/// bool(false)
/// gone
/// after
/// ```
pub(crate) fn unset_var<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let name = match fetched_name(req, frame, code, op) {
        Ok(name) => name,
        Err(thrown) => {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            return threw(req, frame, &thrown);
        }
    };
    let table = if op.extended_value & FETCH_GLOBAL != 0 {
        req.global_table()
    } else {
        req.rebuild_symbol_table(frame)
    };
    req.symbol_unset(table, name.as_bytes());
    name.give_up(req);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    if req.is_pending() {
        return Ok(Stepped::Threw(frame).into());
    }
    Ok(advance(req, frame))
}

/// php's `type` argument to `zend_fetch_var_address_helper`, the five
/// the fetches by name serve.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ByName {
    R,
    W,
    Rw,
    Is,
    Unset,
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
    let out = exec::temporary_slot(code, op.out);
    let name = match fetched_name(req, frame, code, op) {
        Ok(name) => name,
        Err(thrown) => {
            if !lock {
                exec::free_operand(req, frame, code, op.op1_type, op.op1);
            }
            req.replace_slot(frame, out, Value::undef());
            return threw(req, frame, &thrown);
        }
    };
    // The name is held across the fetch, as php's `tmp_name` is, so
    // the table that lays it down takes an owner of the string the
    // program wrote it with.
    let edge = fetch_named(req, frame, code, op, mode, *name);
    name.give_up(req);
    edge
}

/// [`fetch_by_name`] once its name is read.
fn fetch_named(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    mode: ByName,
    name: PhpString,
) -> Edge {
    let lock = op.extended_value & FETCH_GLOBAL_LOCK != 0;
    let global = op.extended_value & (FETCH_GLOBAL | FETCH_GLOBAL_LOCK) != 0;
    let out = exec::temporary_slot(code, op.out);
    let table = if global {
        req.global_table()
    } else {
        req.rebuild_symbol_table(frame)
    };
    let shown = String::from_utf8_lossy(name.as_bytes()).into_owned();
    let undefined = |req: &mut Request| {
        let scope = if op.extended_value & FETCH_GLOBAL != 0 {
            "global "
        } else {
            ""
        };
        crate::api::raising::warning(req, &format!("Undefined {scope}variable ${shown}"));
    };
    let held = req.symbol_value(table, name.as_bytes());
    let unset = held.is_none_or(|value| value.tag() == TypeTag::Undef);
    // `zend_fetch_this_var`: `$this` where no bucket names it.
    if unset && name.as_bytes() == b"this" {
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
        // `BP_VAR_UNSET` over a name the table has not, or whose CV
        // holds nothing: `&EG(uninitialized_zval)`, nothing laid down.
        ByName::Unset if unset => Some(Value::null()),
        ByName::W | ByName::Rw | ByName::Unset => {
            if unset && mode == ByName::Rw {
                undefined(req);
            }
            if req.is_pending() {
                None
            } else {
                let place = match req.symbol_place(table, name, mode != ByName::Unset) {
                    Ok(place) => place,
                    Err(thrown) => return threw(req, frame, &thrown),
                };
                let Some(place) = place else {
                    return Err(Fatal::error(format!(
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
    req.replace_slot(frame, out, value.unwrap_or_else(Value::null));
    if req.is_pending() {
        return Ok(Stepped::Threw(frame).into());
    }
    Ok(advance(req, frame))
}

/// The name a fetch by name reads: the constant as it stands, a string
/// as it is, and anything else converted as `zval_try_get_tmp_string`
/// converts it, a CV nothing wrote warning first
/// (`ZVAL_UNDEFINED_OP1`), with one owner for the caller: the string
/// itself where the operand is one, the conversion's otherwise.
fn fetched_name(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Result<Owner<PhpString>, Thrown> {
    let held = if op.op1_type == OperandKind::Const {
        exec::constant(code, ConstIdx(op.op1))
    } else {
        exec::operand_undef(req, frame, code, op.op1_type, op.op1).deref()
    };
    if let Some(name) = held.as_php_string() {
        return Ok(Owner::take(req, *name));
    }
    if op.op1_type == OperandKind::Const {
        return PhpString::new(req, b"");
    }
    let held = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    let text = crate::ops::to_string(req, &held)?;
    Ok(Owner::adopt(text))
}

/// `zend_fetch_static_property_address_ex`'s first half
/// (`Z/Zend/zend_execute.c:3715`), which every opline over a `static`
/// property shares: the class `op2` names, php's `Error` where nothing
/// declares it, and then the property's name `op1` holds, the constant
/// the compiler wrote or, for `A::$$n` and `A::${expr}`, the value
/// converted as [`fetched_name`] converts it after the class; `op1`
/// given up either way (`FREE_OP(op1_type, opline->op1.var)`).
///
/// ```php
/// class A { public static $b = 0; public static $c = [0, 1]; }
/// $b = 'b'; $c = 'c';
/// A::$$b = 5; A::${'c'}[] = 2; A::$$b++;
/// var_dump(A::$$b, A::${'c'}, isset(A::${'b'}), A::${$c}[1]);
/// try { var_dump(A::${42}); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { var_dump(Missing::$$b); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { unset(A::$$b); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(6)
/// array(3) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(1)
///   [2]=>
///   int(2)
/// }
/// bool(true)
/// int(1)
/// Access to undeclared static property A::$42
/// Class "Missing" not found
/// Attempt to unset static property A::$b
/// ```
fn static_member(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Result<(crate::api::values::ClassId, Vec<u8>), Thrown> {
    let class = match class_operand(req, frame, code, op.op2_type, op.op2, None) {
        Ok(class) => class,
        Err(thrown) => {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            return Err(thrown);
        }
    };
    let name = fetched_name(req, frame, code, op).map(|name| {
        let bytes = name.as_bytes().to_vec();
        name.give_up(req);
        bytes
    });
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    Ok((class, name?))
}

/// `zend_fetch_this_var` (`Z/Zend/zend_execute.c:4185`): `$this` read
/// by name is the receiver, with php's warning where the frame has none
/// under `BP_VAR_R`; under a write it is `Cannot re-assign $this`, and
/// under `BP_VAR_UNSET` `Cannot unset $this`.
fn fetch_this_by_name(req: &mut Request, frame: FrameRef, out: u32, mode: ByName) -> Edge {
    let value = match (req.vm().this(frame), mode) {
        (_, ByName::W | ByName::Rw | ByName::Unset) => {
            req.replace_slot(frame, out, Value::undef());
            let message = if mode == ByName::Unset {
                "Cannot unset $this"
            } else {
                "Cannot re-assign $this"
            };
            let thrown = crate::api::raising::throw_error(req, "Error", message);
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
    req.replace_slot(frame, out, value);
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
/// string(5) "30719"
/// aa
/// int(30719)
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
/// int(30719)
/// NULL
/// ```
pub(crate) fn begin_silence<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let standing = req.diagnostics().error_reporting();
    set_out(req, frame, code, op, Value::int(standing));
    if !has_only_fatal_errors(standing) {
        req.set_error_reporting(standing & E_FATAL_ERRORS);
        req.silence_modifies_error_reporting();
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
/// int(30719)
/// int(30719)
/// int(4437)
/// int(30719)
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
/// A name the program computes, `C::{expr}` (8.3), is read as any
/// operand is: `class` names the class, and what is no string is php's
/// `TypeError`:
///
/// ```php
/// class Foo { const BAR = 'bar'; }
/// $n = 'BAR'; $f = 'Foo'; $i = 42;
/// var_dump(Foo::{$n}, $f::{$n}, Foo::{strtolower('CLASS')}, Foo::{'B' . 'AR'});
/// try { var_dump(Foo::{$i}); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { var_dump(Foo::{[]}); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { var_dump(Foo::{$undefined}); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "bar"
/// string(3) "bar"
/// string(3) "Foo"
/// string(3) "bar"
/// Cannot use value of type int as class constant name
/// Cannot use value of type array as class constant name
///
/// Warning: Undefined variable $undefined in %s on line 7
/// Cannot use value of type null as class constant name
/// ```
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
        Err(thrown) => {
            exec::free_operand(req, frame, code, op.op2_type, op.op2);
            return threw(req, frame, &thrown);
        }
    };
    // `GET_OP2_ZVAL_PTR_DEREF(BP_VAR_R)`: a name the program computed,
    // `C::{expr}`, read as any operand is, a variable nothing wrote
    // warning first; what is no string is `TypeError`
    // (`zend_invalid_class_constant_type_error`), and `class` in any
    // case is the class's name, the compiler having caught only the
    // spelling it saw.
    let named = exec::operand(req, frame, code, op.op2_type, op.op2).deref();
    let Some(name) = named.as_str_bytes().map(<[u8]>::to_vec) else {
        let message = format!(
            "Cannot use value of type {} as class constant name",
            named.type_name()
        );
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
        let thrown = crate::api::raising::throw_error(req, "TypeError", &message);
        return threw(req, frame, &thrown);
    };
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    let value = if op.op2_type != OperandKind::Const && name.eq_ignore_ascii_case(b"class") {
        let written = req.class_name_bytes(class);
        req.declared_string(&written)
            .map(|name| name.into_value().hand_on())
    } else {
        req.class_constant(class, &name)
    };
    computed_into(req, frame, code, op, value)
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn fetch_constant<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    // `CACHED_PTR(opline->extended_value)`: the constant a pass before
    // found, read without a lookup.
    if let exec::Cached::Constant(at) = req.cached(frame, CacheSlot(op.extended_value))
        && let Some(value) = req.constant_at(at)
    {
        req.addref(&value);
        set_out(req, frame, code, op, value);
        return Ok(advance(req, frame));
    }
    fetch_constant_found(req, frame, code, op)
}

/// [`fetch_constant`] where its slot holds nothing: the lookup, by the
/// name and then, for an unqualified name in a namespace, the global one,
/// and the slot written for a constant that is not deprecated.
#[inline(never)]
fn fetch_constant_found(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    if op.op2_type != OperandKind::Const {
        return Err(Fatal::error(
            "InternalError: ZEND_FETCH_CONSTANT over a name no literal carries",
        ));
    }
    let slot = CacheSlot(op.extended_value);
    let mut key = named(code, op.op2.saturating_add(1));
    let mut found = req.constant_found(&key);
    if found.is_none() && op.op1 & UNQUALIFIED_IN_NAMESPACE != 0 {
        key = named(code, op.op2.saturating_add(2));
        found = req.constant_found(&key);
    }
    let Some((at, value, deprecated)) = found else {
        let written = String::from_utf8_lossy(&named(code, op.op2)).into_owned();
        let thrown = crate::api::raising::throw_error(
            req,
            "Error",
            &format!("Undefined constant \"{written}\""),
        );
        return threw(req, frame, &thrown);
    };
    req.addref(&value);
    set_out(req, frame, code, op, value);
    if !deprecated {
        req.prime(frame, slot, exec::Cached::Constant(at));
        return Ok(advance(req, frame));
    }
    // `if (ZEND_CONSTANT_FLAGS(c) & CONST_DEPRECATED)`
    // (`Z/Zend/zend_execute.c:5439`): the value is copied first and the
    // deprecation raised after, and the opline then advances under
    // `ZEND_VM_NEXT_OPCODE_CHECK_EXCEPTION`, so a handler that throws
    // leaves the temporary written and the throwable pending. The
    // constant a deprecation is worded from is the one found, under the
    // name the table holds it by.
    if let Err(thrown) = crate::api::raising::deprecated_constant(req, &key) {
        req.leave_pending(&thrown);
    }
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
/// [`Request::bind_static_place`]) and an `IS_INDIRECT` naming it
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
    static_prop_place(req, frame, code, op, StaticFetch::Write)
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
    static_prop_place(req, frame, code, op, StaticFetch::ReadWrite)
}

/// Which of `zend_fetch_static_prop_helper`'s write modes a fetch is:
/// `BP_VAR_W`, `BP_VAR_RW` or `BP_VAR_UNSET`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StaticFetch {
    Write,
    ReadWrite,
    Unset,
}

/// The body `FETCH_STATIC_PROP_W`, `FETCH_STATIC_PROP_RW` and
/// `FETCH_STATIC_PROP_UNSET` share: the slot as a place, an
/// `IS_INDIRECT` naming it in the `VAR`. A property whose set
/// visibility the scope lacks is no place: an object it holds is copied
/// into the `VAR`, since writing into the object modifies no property,
/// an unset over an unwritten one is nothing, and anything else is
/// php's `Cannot indirectly modify`
/// (`Z/Zend/zend_vm_def.h:1873`).
fn static_prop_place(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    mode: StaticFetch,
) -> Edge {
    let rw = mode == StaticFetch::ReadWrite;
    let (class, name) = match static_member(req, frame, code, op) {
        Ok(member) => member,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let scope = req.executed_scope();
    if rw {
        match req.static_property(class, &name, scope, PropertyRead::Warning) {
            Ok(held) => req.release(held),
            Err(thrown) => return threw(req, frame, &thrown),
        }
    }
    // The slot found and filled first, raising what access raises; then
    // `zend_handle_fetch_obj_flags`'s refusals over the slot as it stands,
    // before the binding lays `null` in a slot nothing has written; then
    // the place.
    let info = match req.static_slot(class, &name, scope, PropertyRead::Warning) {
        Ok(Some(info)) => info,
        Ok(None) => {
            return Err(Fatal::error(format!(
                "InternalError: ZEND_FETCH_STATIC_PROP_W placed ${} and the table holds no slot for it",
                String::from_utf8_lossy(&name)
            )));
        }
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let flag = if rw {
        None
    } else {
        FetchFlag::of(op.extended_value)
    };
    let typed = flag.and_then(|_| req.declared_typed_static(class, &name));
    if let (Some(flag), Some(typed)) = (flag, &typed) {
        let held = req.held_static(info);
        if let Err(thrown) = refuse_fetch(req, &held, typed, &name, flag) {
            return threw(req, frame, &thrown);
        }
    }
    if req.lacks_set_access(info) {
        let held = req.held_static(info).deref();
        let unwritten = matches!(held.as_ref(), ValueRef::Undef);
        if held.as_object().is_some() || (mode == StaticFetch::Unset && unwritten) {
            let copied = held.undef_as_null();
            req.addref(&copied);
            set_out(req, frame, code, op, copied);
            return Ok(advance(req, frame));
        }
        if let Some(thrown) = req.set_refused(info, "indirectly modify") {
            return threw(req, frame, &thrown);
        }
    }
    let place = match req.bind_static_place(info) {
        Ok(place) => place,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    if flag == Some(FetchFlag::Ref)
        && let Some(typed) = &typed
    {
        static_type_source(req, info, typed, &name);
    }
    let out = exec::temporary_slot(code, op.out);
    let previous = req.take_slot(frame, out);
    req.release(previous);
    // Under `ZEND_FETCH_REF` php's slot becomes the reference the `&`
    // binds to. The slot here is the cell its place was bound to, so the
    // `VAR` is that cell, with an owner, and the bind shares it rather
    // than making a second cell inside it.
    let bound = (flag == Some(FetchFlag::Ref))
        .then(|| req.static_slot_cell(info))
        .flatten();
    if let Some(cell) = bound {
        req.addref(&cell.as_value());
        req.replace_slot(frame, out, cell.as_value());
        return Ok(advance(req, frame));
    }
    let Some(var) = req.slot_ptr(frame, out) else {
        return Err(Fatal::error(
            "InternalError: ZEND_FETCH_STATIC_PROP_W writes a VAR the frame does not have",
        ));
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
    let (class, name) = match static_member(req, frame, code, op) {
        Ok(member) => member,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let scope = req.executed_scope();
    let read = req.static_property(class, &name, scope, fetch);
    computed_into(req, frame, code, op, read)
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
///
/// A `&__get` answers with its reference, and the temporary holds the
/// value it names, so a `foreach` over it frees what it took and not
/// the array the reference keeps:
///
/// ```php
/// class A { private $k = []; function &__get($n) { return $this->k[$n]; } }
/// $a = new A; $b = &$a->arr; $b = [1]; $b[] = 2;
/// foreach ($a->arr as $v) { echo $v, "\n"; }
/// var_dump($b);
/// ```
/// ```expect
/// 1
/// 2
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// ```
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn fetch_obj_r<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_fetch_obj_r(req, frame, code, op, (A::default(), B::default())) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    fetched_obj(req, frame, code, op)
}

/// `FETCH_OBJ_R` past its inline path: [`fetch_obj`] under `BP_VAR_R`.
/// A call of its own, so that the inline path before it keeps to the
/// registers it needs.
#[inline(never)]
fn fetched_obj(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    fetch_obj(req, frame, code, op, PropertyRead::Warning)
}

/// The inline path of `ZEND_FETCH_OBJ_R` over a constant name
/// (`ZEND_FETCH_OBJ_R_SPEC_CV_CONST`, `_UNUSED_CONST`,
/// `Z/Zend/zend_vm_def.h:2102-2160`): the receiver a CV holds, or
/// `$this`, whose class is the one the cache slot was primed with, and
/// the place the slot names, a declared slot or a dynamic property's
/// bucket ([`cached_place`]), read in place, `ZVAL_COPY_DEREF` into the
/// temporary. `None`, with nothing touched, for a name that is no
/// constant, a container that is a temporary, which the fetch would
/// free, or no object, a lazy object, a place the cache does not name
/// for this class and frame ([`Request::cached_property`]), one nothing
/// holds, whose read is `__get`'s or the warning's, and a temporary
/// holding an owner.
#[inline(always)] // the handler's fast half, as php's is in the handler
#[allow(clippy::inline_always)]
fn inline_fetch_obj_r(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    (a, b): (impl Operand, impl Operand),
) -> Option<Edge> {
    let object = inline_receiver(req, frame, code, op, a, b)?;
    let (place, _) = cached_place(
        req,
        (frame, code, op),
        property_cache_slot(op),
        object,
        false,
    )?;
    let z = crate::operand::deref(place.read());
    if z.is_undef() {
        return None;
    }
    let out = req
        .slots_mut(frame)
        .get_mut(exec::temporary_slot(code, op.out) as usize)?;
    if out.is_refcounted() {
        return None;
    }
    z.addref();
    *out = z;
    Some(Ok(advanced()))
}

/// The place cache slot `slot` of a property opcode over a constant name
/// names on `object` ([`Request::cached_property`]), and whether its
/// property is typed: the declared slot in place, or the bucket of the
/// object's dynamic table the name is in, found by the name's known hash
/// (`zend_hash_find_known_hash(zobj->properties, name)`,
/// `Z/Zend/zend_vm_def.h:2154`), for a `write` only in a table nothing
/// else shares, whose separation is the kernel's. None where the cache
/// names no place for this class and frame, and where the table holds
/// no such name, which is `__get`'s, the warning's or a write that lays
/// it down.
#[inline(always)] // as `inline_fetch_obj_r`
#[allow(clippy::inline_always)]
fn cached_place(
    req: &Request,
    (frame, code, op): (FrameRef, &OpArray, &Opline),
    slot: CacheSlot,
    object: php_heap::Obj,
    write: bool,
) -> Option<(php_heap::ZvalRef, bool)> {
    let class = crate::api::values::ClassId(object.class());
    match req.cached_property(frame, slot, class)? {
        exec::PropertyOffset::Declared { slot, typed } => Some((object.slot_ref(slot)?, typed)),
        exec::PropertyOffset::Dynamic => {
            let table = object
                .properties()
                .filter(|table| !write || (table.refcount() == 1 && !table.is_immutable()))?;
            let name = code.literals.get(op.op2 as usize)?.as_str()?;
            Some((table.value_ref(php_heap::Key::Str(name))?, false))
        }
    }
}

/// [`Request::prime_property`] for a property opcode whose name is a
/// constant, at cache slot `slot`: `zend_get_property_offset` given the
/// opline's `cache_slot`, which a computed name's site passes as `NULL`.
fn prime_constant(
    req: &mut Request,
    frame: FrameRef,
    op: &Opline,
    slot: CacheSlot,
    object: PhpObject,
    name: &Value,
) {
    if op.op2_type == OperandKind::Const
        && let Some(bytes) = name.as_str_bytes()
    {
        req.prime_property(frame, slot, object, bytes);
    }
}

/// `CACHE_ADDR(opline->extended_value & ~ZEND_FETCH_OBJ_FLAGS)`: the
/// first of the three cache slots a property opcode over a constant
/// name carries in `extended_value`, below the fetch flags a `_W` and a
/// `_FUNC_ARG` fetch carry beside it.
fn property_cache_slot(op: &Opline) -> CacheSlot {
    CacheSlot(op.extended_value & !(FETCH_REF | FETCH_DIM_WRITE))
}

/// The object a property opcode's inline path works on: `$this` for an
/// `UNUSED` container, the object a CV holds, a reference followed,
/// for a CV, and only where the name is a constant and the object is
/// not lazy. None for every other container, which the kernel reads.
#[inline(always)] // as `inline_fetch_obj_r`
#[allow(clippy::inline_always)]
fn inline_receiver(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    a: impl Operand,
    b: impl Operand,
) -> Option<php_heap::Obj> {
    if b.kind() != OperandKind::Const {
        return None;
    }
    let object = match a.kind() {
        OperandKind::Unused => req.vm().this(frame)?.heap(),
        OperandKind::Cv => {
            crate::operand::deref(a.read(req.slots_mut(frame), code, op.op1)).as_obj()?
        }
        _ => return None,
    };
    (object.laziness() == php_heap::Laziness::Eager).then_some(object)
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
/// handler under two fetch types: the container read as the fetch type
/// reads it, then the property's name ([`exec::Member`]) and the
/// `read_property` slot over an object and, over anything else, the
/// warning `BP_VAR_R` raises and `BP_VAR_IS` does not; the name and the
/// container freed after it (`FREE_OP2`, `FREE_OP1`) and `null` or the
/// value written into the temporary.
fn fetch_obj(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    fetch: PropertyRead,
) -> Edge {
    let mut name = exec::Member::default();
    // `GET_OP1_OBJ_ZVAL_PTR(BP_VAR_R)` warns of a variable nothing
    // wrote; the `BP_VAR_IS` reader takes the same slot as the `UNDEF`
    // it is and says nothing.
    let container = match fetch {
        PropertyRead::Warning | PropertyRead::Write | PropertyRead::Unset => {
            exec::operand(req, frame, code, op.op1_type, op.op1)
        }
        PropertyRead::Silent => exec::operand_undef(req, frame, code, op.op1_type, op.op1),
    }
    .deref();
    let read = if let Some(object) = container.as_object().copied() {
        name.name(req, frame, code, op).and_then(|name| {
            prime_constant(req, frame, op, property_cache_slot(op), object, &name);
            let name = name.as_str_bytes().unwrap_or_default();
            let table = req.object_handlers(object.heap());
            table
                .read_property(req, object, name, fetch)
                .map(Owner::hand_on)
        })
    } else if fetch == PropertyRead::Warning {
        name.name(req, frame, code, op).map(|name| {
            let name = name.as_str_bytes().unwrap_or_default();
            crate::api::raising::warning(
                req,
                &format!(
                    "Attempt to read property \"{}\" on {}",
                    String::from_utf8_lossy(name),
                    crate::ops::value_name(req, &container)
                ),
            );
            Value::null()
        })
    } else {
        exec::Member::touch(req, frame, code, op);
        Ok(Value::null())
    };
    name.release(req, frame, code, op);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    // `ZVAL_COPY_DEREF`, or `zend_unwrap_reference` where the slot
    // answered into the temporary: a `&__get` hands back its reference,
    // and the temporary holds the value, never the cell.
    let value = match read {
        Ok(value) => req.unwrap_reference(value),
        Err(thrown) => return threw(req, frame, &thrown),
    };
    set_out(req, frame, code, op, value);
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
///
/// Where the class's `get_property_ptr_ptr` answers `NULL`, the fetch
/// reads through `read_property` instead of placing the name, as
/// `zend_fetch_property_address` falls back: `PDORow` serves every
/// name, and one that is no column reads `null`, so no dynamic property
/// is created on the row and the write over it is php's `Error`;
/// `DOMDocument::$documentElement` reads the element, and the write
/// lands on it.
///
/// ```php
/// $db = new PDO('sqlite::memory:');
/// $row = $db->query('SELECT 1 AS a')->fetch(PDO::FETCH_LAZY);
/// try { $row->documentElement->firstChild = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $d = new DOMDocument;
/// $d->loadXML('<r>old</r>');
/// $d->documentElement->nodeValue = 'new';
/// echo $d->saveXML();
/// ```
/// ```expect
/// Attempt to assign property "firstChild" on null
/// <?xml version="1.0"?>
/// <r>new</r>
/// ```
///
/// What a `&__get` answers from a local is a reference nothing else
/// holds, which the `VAR` takes as the value it names (`ZVAL_UNREF`),
/// so the write through it lands on the object:
///
/// ```php
/// class Foo { private $bar; function __construct() { $this->bar = new Bar; }
///     function &__get($k) { $bar = $this->bar; return $bar; } }
/// class Bar { public $on = []; }
/// $foo = new Foo;
/// $foo->bar->on[] = 1;
/// var_dump(count($foo->bar->on));
/// ```
/// ```expect
/// int(1)
/// ```
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn fetch_obj_w<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_fetch_obj_w(req, frame, code, op, (A::default(), B::default())) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    fetched_obj_place(req, frame, code, op)
}

/// `FETCH_OBJ_W` past its inline path: [`fetch_obj_place`] under
/// `BP_VAR_W`. A call of its own, as [`fetched_obj`] is.
#[inline(never)]
fn fetched_obj_place(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    fetch_obj_place(req, frame, code, op, PropertyRead::Silent)
}

/// The inline path of `ZEND_FETCH_OBJ_W` over a constant name
/// (`zend_fetch_property_address`'s cached arm,
/// `Z/Zend/zend_execute.c:3540-3560`): the place the cache slot names
/// on a receiver of the class it was primed with ([`cached_place`]),
/// where it holds a value, left in the `VAR` as an `IS_INDIRECT` for the
/// write after it. `None` as [`inline_fetch_obj_r`]'s, and for a typed slot, whose
/// fetch flags are [`fetch_flags`]', and a `VAR` holding an owner.
#[inline(always)] // as `inline_fetch_obj_r`
#[allow(clippy::inline_always)]
fn inline_fetch_obj_w(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    (a, b): (impl Operand, impl Operand),
) -> Option<Edge> {
    let object = inline_receiver(req, frame, code, op, a, b)?;
    let (place, typed) = cached_place(
        req,
        (frame, code, op),
        property_cache_slot(op),
        object,
        true,
    )?;
    if typed || place.read().is_undef() {
        return None;
    }
    let out = req
        .slots_mut(frame)
        .get_mut(exec::temporary_slot(code, op.out) as usize)
        .filter(|out| !out.is_refcounted())?;
    *out = Zval::indirect(place);
    Some(Ok(advanced()))
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

/// `ZEND_FETCH_OBJ_FUNC_ARG` (`Z/Zend/zend_vm_def.h:2397`), a property
/// sent to a callee the compiler knew nothing of: [`fetch_obj_w`] where
/// the `CHECK_FUNC_ARG` before it found the parameter takes its
/// argument by reference, so the callee writes into the property, and
/// [`fetch_obj_r`] otherwise, with the read's warnings. A property
/// under it is fetched the same way, and a temporary has no place to
/// write into.
///
/// ```php
/// class N { public $list = [2, 1]; }
/// class C {
///     public $p = [3, 1, 2]; public $n;
///     function __construct() { $this->n = new N; }
///     function byref(&$a) { $a[] = 0; }
///     function byval($a) { return is_array($a) ? count($a) : $a; }
///     function own() { $this->byref($this->p); return $this->byval($this->p); }
/// }
/// $o = new C;
/// $o->byref($o->p);
/// $o->byref($o->n->list);
/// var_dump($o->p, $o->n->list, $o->byval($o->p), $o->own());
/// var_dump($o->byval($o->missing));
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   int(3)
///   [1]=>
///   int(1)
///   [2]=>
///   int(2)
///   [3]=>
///   int(0)
/// }
/// array(3) {
///   [0]=>
///   int(2)
///   [1]=>
///   int(1)
///   [2]=>
///   int(0)
/// }
/// int(4)
/// int(5)
///
/// Warning: Undefined property: C::$missing in %s on line 14
/// NULL
/// ```
pub(crate) fn fetch_obj_func_arg<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if !sends_arg_by_ref(req) {
        return fetched_obj(req, frame, code, op);
    }
    if matches!(op.op1_type, OperandKind::Const | OperandKind::Tmp) {
        return use_tmp_in_write_context(req, frame, code, op);
    }
    fetched_obj_place(req, frame, code, op)
}

/// `ZEND_FETCH_DIM_FUNC_ARG` (`Z/Zend/zend_vm_def.h:2035`), an element
/// sent to a callee the compiler knew nothing of: [`fetch_dim_w`] where
/// the parameter takes its argument by reference, which lays the
/// element down where it was not and appends for `[]`, and
/// [`fetch_dim_r`] otherwise, where `[]` is php's `Error`.
///
/// ```php
/// class C { function byref(&$a) { $a = 'set'; } function byval($a) { return $a; } }
/// $o = new C; $a = ['k' => 'v'];
/// $o->byref($a['new']);
/// $o->byref($a[]);
/// var_dump($a, $o->byval($a['k']), $o->byval($a['none']));
/// try { $o->byval($a[]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: Undefined array key "none" in %s on line 6
/// array(3) {
///   ["k"]=>
///   string(1) "v"
///   ["new"]=>
///   string(3) "set"
///   [0]=>
///   string(3) "set"
/// }
/// string(1) "v"
/// NULL
/// Cannot use [] for reading
/// ```
///
/// By value it is the read's own inline path over an element the array
/// has, the warning of one it has not got the kernel's; by reference
/// the element is laid down:
///
/// ```php
/// function k(array $a, array &$b, $j, $s) { return [g($a[$j]), g($a[$s]), g($b[$j]), r($b[$j]), r($b[5]), g($a[9])]; }
/// function g($v) { return $v; }
/// function r(&$v) { $v .= "!"; return $v; }
/// $a = [1.5, "k" => "v"]; $b = [2.5];
/// echo json_encode([k($a, $b, 0, "k"), $b]), "\n";
/// ```
/// ```expect
///
/// Warning: Undefined array key 9 in %s on line 2
/// [[1.5,"v",2.5,"2.5!","!",null],{"0":"2.5!","5":"!"}]
/// ```
#[cfg_attr(not(debug_assertions), inline(always))] // design §5.8, as `fetch_dim_r`
#[cfg_attr(debug_assertions, inline(never))] // a debug `serve_frame` holds every inlined arm's locals
#[allow(clippy::inline_always)]
pub(crate) fn fetch_dim_func_arg<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if sends_arg_by_ref(req) {
        req.vm_mut().stand(frame, at);
        if matches!(A::default().kind(), OperandKind::Const | OperandKind::Tmp) {
            return use_tmp_in_write_context(req, frame, code, op);
        }
        return fetched_dim_place(req, frame, code, op);
    }
    if B::default().kind() == OperandKind::Unused {
        // `zend_use_undef_in_read_context_helper`.
        req.vm_mut().stand(frame, at);
        let thrown = crate::api::raising::throw_error(req, "Error", "Cannot use [] for reading");
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        return threw(req, frame, &thrown);
    }
    // `ZEND_VM_DISPATCH_TO_HANDLER(ZEND_FETCH_DIM_R)`: the read's body.
    fetch_dim_r::<T, A, B>(req, frame, code, op, at)
}

/// `ZEND_FETCH_STATIC_PROP_FUNC_ARG` (`Z/Zend/zend_vm_def.h:1912`), a
/// class's property sent to a callee the compiler knew nothing of:
/// [`fetch_static_prop_w`] where the parameter takes its argument by
/// reference and [`fetch_static_prop_r`] otherwise.
///
/// ```php
/// class S { public static $list = [2, 1]; public static $n = 5; }
/// class C { function byref(&$a) { sort($a); } function byval($a) { return $a + 1; } }
/// $o = new C;
/// $o->byref(S::$list);
/// var_dump(S::$list, $o->byval(S::$n));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// int(6)
/// ```
pub(crate) fn fetch_static_prop_func_arg<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if sends_arg_by_ref(req) {
        return fetch_static_prop_w::<T>(req, frame, code, op);
    }
    fetch_static_prop_r::<T>(req, frame, code, op)
}

/// `zend_use_tmp_in_write_context_helper`: a `_FUNC_ARG` fetch over a
/// temporary where the parameter takes its argument by reference, php's
/// `Error`, both operands given up.
fn use_tmp_in_write_context(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let thrown = crate::api::raising::throw_error(
        req,
        "Error",
        "Cannot use temporary expression in write context",
    );
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    threw(req, frame, &thrown)
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
    let mut name = exec::Member::default();
    let found = property_place(req, frame, code, op, read, &mut name);
    name.release(req, frame, code, op);
    let at = match found? {
        PropertyFound::At(at, object, name) => {
            // `BP_VAR_RW` carries no flags; `BP_VAR_W` masks them off
            // the cache slot.
            let flag = if read == PropertyRead::Silent {
                FetchFlag::of(op.extended_value)
            } else {
                None
            };
            if let Some(typed) = req.declared_typed_property(object.class(), &name)
                && let Err(thrown) = fetch_flags(req, at, &typed, &name, flag)
            {
                let thrown = freeing_container(req, frame, code, op, thrown);
                return threw(req, frame, &thrown);
            }
            at
        }
        PropertyFound::Served(object, name) => {
            return fetch_obj_read(req, frame, code, op, object, &name);
        }
        PropertyFound::Threw(thrown) => return threw(req, frame, &thrown),
    };
    let out = exec::temporary_slot(code, op.out);
    let previous = req.take_slot(frame, out);
    req.release(previous);
    let Some(var) = req.slot_ptr(frame, out) else {
        return Err(Fatal::error(
            "InternalError: ZEND_FETCH_OBJ_W writes a VAR the frame does not have",
        ));
    };
    var.write(php_heap::Zval::indirect(at));
    exec::free_container_var(req, frame, code, op);
    Ok(advance(req, frame))
}

/// `zend_handle_fetch_obj_flags` (`Z/Zend/zend_execute.c:3470`), the
/// bits a `FETCH_OBJ_W` or `FETCH_STATIC_PROP_W` over a typed property
/// carries under `ZEND_FETCH_OBJ_FLAGS`. `ZEND_FETCH_DIM_WRITE`, a
/// dimension written over the place, refuses a place that would become
/// an array the type does not take. `ZEND_FETCH_REF`, a place bound by
/// `&`, turns the slot into a cell typed by the property, so every write
/// through a name bound to it is held to the type
/// ([`Request::assign_to_reference`]); a slot nothing has written is
/// `null` in the cell where the type takes `null`, and php's `Error`
/// where it does not.
///
/// ```php
/// class C { public int $i = 1; public ?int $n; public int $u; public ?string $s = null; }
/// $o = new C;
/// $r = &$o->i;
/// try { $r = "x"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $r = "5";
/// $q = &$o->n;
/// try { $u = &$o->u; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $o->s[] = 1; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($o);
/// ```
/// ```expect
/// Cannot assign string to reference held by property C::$i of type int
/// Cannot access uninitialized non-nullable property C::$u by reference
/// Cannot auto-initialize an array inside property C::$s of type ?string
/// object(C)#1 (3) {
///   ["i"]=>
///   &int(5)
///   ["n"]=>
///   &NULL
///   ["u"]=>
///   uninitialized(int)
///   ["s"]=>
///   NULL
/// }
/// ```
fn fetch_flags(
    req: &mut Request,
    at: php_heap::ZvalRef,
    typed: &crate::api::request::TypedProperty,
    name: &[u8],
    flag: Option<FetchFlag>,
) -> Result<(), Thrown> {
    match flag {
        None => Ok(()),
        Some(FetchFlag::Ref) => req.fetch_typed_slot_ref(at, typed, name),
        Some(flag) => refuse_fetch(req, &exec::value_of(at.read()), typed, name, flag),
    }
}

/// `zend_handle_fetch_obj_flags` under `ZEND_FETCH_REF` over a `static`
/// property's slot, once [`refuse_fetch`] has passed it: the slot is a
/// cell already, bound when its place was taken
/// ([`Request::bind_static_place`]), and it gains the property as a
/// type source where it does not carry it yet, so every write through a
/// name bound to it is held to the type.
///
/// ```php
/// class T { public static int $x = 0; public static ?string $s; public static int $u; }
/// $x = &T::$x;
/// try { $x = "foo"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $x = "5";
/// try { T::$s["k"][] = 1; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $u = &T::$u; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $s = &T::$s;
/// var_dump(T::$x, $s);
/// ```
/// ```expect
/// Cannot assign string to reference held by property T::$x of type int
/// Cannot auto-initialize an array inside property T::$s of type ?string
/// Cannot access uninitialized non-nullable property T::$u by reference
/// int(5)
/// NULL
/// ```
fn static_type_source(
    req: &mut Request,
    info: crate::api::request::PropertyInfo,
    typed: &crate::api::request::TypedProperty,
    name: &[u8],
) {
    if let Some(cell) = req.static_slot_cell(info)
        && !req
            .ref_type_sources(cell.heap())
            .iter()
            .any(|source| source.declaring == typed.declaring && source.name == name)
    {
        req.ref_add_type_source(cell.heap(), type_source(typed, name));
    }
}

/// `zend_verify_ref_array_assignable` (`Z/Zend/zend_execute.c:3458`): a
/// dimension written through a reference that holds nothing an array
/// could be written into yet is refused where a property the reference is
/// typed by takes no array, the first such property named in php's
/// `TypeError` (`zend_throw_auto_init_in_ref_error`).
///
/// ```php
/// class C { public ?string $s = null; public ?array $a = null; }
/// $c = new C; $s = &$c->s; $a = &$c->a;
/// try { $s[] = 1; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $a["k"] = 1;
/// var_dump($c);
/// ```
/// ```expect
/// Cannot auto-initialize an array inside a reference held by property C::$s of type ?string
/// object(C)#1 (2) {
///   ["s"]=>
///   &NULL
///   ["a"]=>
///   &array(1) {
///     ["k"]=>
///     int(1)
///   }
/// }
/// ```
fn verify_ref_array_assignable(req: &mut Request, cell: PhpRef) -> Result<(), Thrown> {
    let refused = req
        .ref_type_sources(cell.heap())
        .iter()
        .find(|source| source.ty.mask() & MAY_BE_ARRAY == 0)
        .map(|source| {
            (
                source.declaring,
                php_unit::type_to_string(&source.ty),
                String::from_utf8_lossy(&source.name).into_owned(),
            )
        });
    let Some((declaring, ty, shown)) = refused else {
        return Ok(());
    };
    let class = req.class_name(declaring);
    Err(crate::api::raising::throw_error(
        req,
        "TypeError",
        &format!(
            "Cannot auto-initialize an array inside a reference held by property {class}::${shown} of type {ty}"
        ),
    ))
}

/// The refusals `zend_handle_fetch_obj_flags` makes before it touches the
/// place: a dimension written over a place php would auto-initialize to an
/// array the type does not take, and a place bound by `&` that nothing has
/// written where the type does not take `null`.
fn refuse_fetch(
    req: &mut Request,
    held: &Value,
    typed: &crate::api::request::TypedProperty,
    name: &[u8],
    flag: FetchFlag,
) -> Result<(), Thrown> {
    let shown = String::from_utf8_lossy(name).into_owned();
    match flag {
        FetchFlag::DimWrite => {
            let promotes = matches!(
                held.deref().as_ref(),
                ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false)
            );
            if promotes && typed.ty.mask() & MAY_BE_ARRAY == 0 {
                let class = req.class_name(typed.declaring);
                let ty = php_unit::type_to_string(&typed.ty);
                return Err(crate::api::raising::throw_error(
                    req,
                    "TypeError",
                    &format!(
                        "Cannot auto-initialize an array inside property {class}::${shown} of type {ty}"
                    ),
                ));
            }
        }
        FetchFlag::Ref => req.refuse_unwritten_ref(held, typed, name)?,
    }
    Ok(())
}

/// The property as a cell's type source, `ZEND_REF_ADD_TYPE_SOURCE`'s
/// `prop_info`.
fn type_source(
    typed: &crate::api::request::TypedProperty,
    name: &[u8],
) -> crate::api::request::RefSource {
    crate::api::request::RefSource {
        declaring: typed.declaring,
        name: name.to_vec(),
        ty: typed.ty.clone(),
    }
}

/// The bit under `ZEND_FETCH_OBJ_FLAGS` (`Z/Zend/zend_compile.h:1089-1096`)
/// a property fetch for a write carries in its `extended_value`, decoded
/// once: `ZEND_FETCH_REF`, a place bound by `&`, or `ZEND_FETCH_DIM_WRITE`,
/// a dimension written over it. php sets at most one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchFlag {
    Ref,
    DimWrite,
}

impl FetchFlag {
    /// The bit an opline's `extended_value` carries, if any.
    fn of(extended_value: u32) -> Option<FetchFlag> {
        match extended_value & (FETCH_REF | FETCH_DIM_WRITE) {
            FETCH_REF => Some(FetchFlag::Ref),
            FETCH_DIM_WRITE => Some(FetchFlag::DimWrite),
            _ => None,
        }
    }
}

/// `ZEND_FETCH_REF` and `ZEND_FETCH_DIM_WRITE`
/// (`Z/Zend/zend_compile.h:1089-1096`).
const FETCH_REF: u32 = 1;
const FETCH_DIM_WRITE: u32 = 2;
/// `MAY_BE_ARRAY` (`Z/Zend/zend_type_info.h`).
const MAY_BE_ARRAY: u32 = 1 << 7;

/// What [`property_place`] found: the property's place, the object and
/// the name; the object and the name where the class serves the name
/// itself ([`PropertyPlace::Handlers`], php's `NULL` from
/// `get_property_ptr_ptr`), which the caller reads through
/// `read_property`; or the throwable, the container given up before it.
enum PropertyFound {
    At(php_heap::ZvalRef, PhpObject, Vec<u8>),
    Served(PhpObject, Vec<u8>),
    Threw(Thrown),
}

/// `zend_fetch_property_address`'s arm over what is no object
/// (`Z/Zend/zend_execute.c:3521`): `ZVAL_UNDEFINED_OP1` under every type
/// but `BP_VAR_W`, php's `BP_VAR_RW` being [`PropertyRead::Warning`]
/// here, then `zend_throw_non_object_error`, chained over what a handler
/// of the warning threw, the operand freed.
fn non_object_place(
    req: &mut Request,
    (frame, code, op): (FrameRef, &OpArray, &Opline),
    read: PropertyRead,
    container: &Value,
    name: &[u8],
) -> Thrown {
    if op.op1_type == OperandKind::Cv
        && matches!(read, PropertyRead::Warning | PropertyRead::Unset)
        && matches!(container.as_ref(), ValueRef::Undef)
    {
        let _ = exec::operand(req, frame, code, op.op1_type, op.op1);
    }
    let ran_on = req.pending();
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    let thrown = crate::api::raising::non_object_error(req, "modify", name, container);
    req.raised_over(thrown, ran_on)
}

/// `zend_fetch_property_address` (`Z/Zend/zend_execute.c`) under
/// `BP_VAR_W` or `BP_VAR_RW`, the place `FETCH_OBJ_W`, `FETCH_OBJ_RW`
/// and `ASSIGN_OBJ_REF` write through: the object `op1` holds and the
/// slot or the dynamic entry of the name in `op2`, which the caller
/// gives up after ([`exec::Member::release`]).
fn property_place(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    read: PropertyRead,
    member: &mut exec::Member,
) -> Result<PropertyFound, Fatal> {
    let container =
        exec::container(req, frame, code, op.op1_type, op.op1, "ZEND_FETCH_OBJ_W")?.deref();
    let name_value = match member.name(req, frame, code, op) {
        Ok(name) => name,
        Err(thrown) => {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            return Ok(PropertyFound::Threw(thrown));
        }
    };
    let name = name_value.as_str_bytes().unwrap_or_default().to_vec();
    let Some(object) = container.as_object().copied() else {
        let thrown = non_object_place(req, (frame, code, op), read, &container, &name);
        return Ok(PropertyFound::Threw(thrown));
    };
    prime_constant(req, frame, op, property_cache_slot(op), object, &name_value);
    let (object, lent) = match req.lazy_place(object, &name) {
        Ok(placed) => placed,
        Err(thrown) => {
            return Ok(PropertyFound::Threw(freeing_container(
                req, frame, code, op, thrown,
            )));
        }
    };
    // The guard a proxy's `__get` lent the instance is held while the
    // place is resolved there, and given back before what the fetch
    // does with it.
    let found = property_place_on(req, (frame, code, op), read, object, &name, &name_value);
    req.give_back_guard(lent);
    found
}

/// [`property_place`] on the object the place is resolved on:
/// `zend_std_get_property_ptr_ptr`'s arms and what the fetch falls back
/// to where it answers none.
fn property_place_on(
    req: &mut Request,
    (frame, code, op): (FrameRef, &OpArray, &Opline),
    read: PropertyRead,
    object: PhpObject,
    name: &[u8],
    name_value: &Value,
) -> Result<PropertyFound, Fatal> {
    let name = name.to_vec();
    let place = match place_of(req, object, &name) {
        Ok(place) => place,
        // The class refused the place with a throwable, php's
        // `&EG(error_zval)`.
        Err(thrown) => {
            return Ok(PropertyFound::Threw(freeing_container(
                req, frame, code, op, thrown,
            )));
        }
    };
    let placed = match place {
        // `if (NULL == ptr) ptr = zobj->handlers->read_property(...)`:
        // a name the class serves is read, not placed.
        PropertyPlace::Handlers | PropertyPlace::Apart => {
            return Ok(PropertyFound::Served(object, name));
        }
        // php places the name here with `zend_std_get_property_ptr_ptr`,
        // whose silence is `ce->__get != NULL`, and a property the scope
        // may not reach is the `Error`; the arms below are the names it
        // answers no place for.
        // A readonly or set-restricted property holding an object is
        // read, as php's `read_property` returns it under `BP_VAR_W`: the
        // write goes through the object and the property is untouched.
        // And `zend_std_get_property_ptr_ptr`'s `NULL` for a name the
        // class's `__get` answers: the fetch falls back to
        // `read_property` (#513).
        PropertyPlace::Std
            if req.write_fetch_reads_object(object, &name, op.opcode == Opcode::FetchObjUnset)
                || req.overloaded_fetch(object, &name) =>
        {
            return Ok(PropertyFound::Served(object, name));
        }
        // `BP_VAR_RW` reads the slot before anything is placed, so an
        // unwritten one is the read's diagnostic ahead of the set
        // visibility's refusal.
        PropertyPlace::Std
            if read == PropertyRead::Warning
                && let Err(thrown) = req.read_before_write(object, &name) =>
        {
            return Ok(PropertyFound::Threw(freeing_container(
                req, frame, code, op, thrown,
            )));
        }
        PropertyPlace::Std => {
            req.declared_slot_for_write(object, &name, "indirectly modify", false)
        }
    };
    let at = match placed {
        Ok(Some((slot, _))) => {
            let Some(at) = object.heap().slot_ref(slot) else {
                return Err(Fatal::error(format!(
                    "InternalError: ZEND_FETCH_OBJ_W placed ${} in slot {slot}, which the object does not have",
                    String::from_utf8_lossy(&name)
                )));
            };
            at
        }
        Ok(None) => {
            let Some(key) = name_value.as_php_string().copied() else {
                return Err(Fatal::error(
                    "InternalError: ZEND_FETCH_OBJ_W over a property whose name is no string",
                ));
            };
            req.addref(name_value);
            // `zend_std_get_property_ptr_ptr`: under `BP_VAR_W` the
            // entry is laid down with the creation's diagnostics and no
            // read's warning, and under `BP_VAR_RW` with the warning
            // after them.
            match req.dynamic_property_ref(object, key, read) {
                Ok(Some(at)) => at,
                Ok(None) => {
                    return Err(Fatal::error(format!(
                        "InternalError: ZEND_FETCH_OBJ_W laid the dynamic property ${} down and the table does not hold it",
                        String::from_utf8_lossy(&name)
                    )));
                }
                Err(thrown) => {
                    return Ok(PropertyFound::Threw(freeing_container(
                        req, frame, code, op, thrown,
                    )));
                }
            }
        }
        Err(thrown) => {
            return Ok(PropertyFound::Threw(freeing_container(
                req, frame, code, op, thrown,
            )));
        }
    };
    Ok(PropertyFound::At(at, object, name))
}

/// `zend_fetch_property_address`'s fallback where the class's
/// `get_property_ptr_ptr` answers `NULL` ([`PropertyPlace::Handlers`]):
/// the class's `read_property` under the fetch's own type, which
/// `zend_std_read_property` reads as `BP_VAR_R` for `_W` and `_RW`
/// alike, and what it answers written into the `VAR` as a value of the
/// fetch's own rather than a place. The write over the fetch then lands
/// on that value, and never lays a property down on the object.
fn fetch_obj_read(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    object: PhpObject,
    name: &[u8],
) -> Edge {
    let table = req.object_handlers(object.heap());
    // `BP_VAR_UNSET` reads what is not there as `null`, quietly; every
    // other write fetch reads as a write.
    let mode = if op.opcode == Opcode::FetchObjUnset {
        PropertyRead::Unset
    } else {
        PropertyRead::Write
    };
    let read = table
        .read_property(req, object, name, mode)
        .map(Owner::hand_on);
    let value = match read {
        // `if (Z_ISREF_P(ptr) && Z_REFCOUNT_P(ptr) == 1) ZVAL_UNREF(ptr)`:
        // a reference nothing else holds, a `&__get`'s local, is the
        // value it names.
        Ok(value) if value.as_php_ref().is_some() && value.refcount() == 1 => {
            req.unwrap_reference(value)
        }
        Ok(value) => value,
        Err(thrown) => {
            let thrown = freeing_container(req, frame, code, op, thrown);
            return threw(req, frame, &thrown);
        }
    };
    set_out(req, frame, code, op, value);
    exec::free_container_var(req, frame, code, op);
    Ok(advance(req, frame))
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn fetch_dim_r<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_fetch_dim_r(req, frame, code, op, (A::default(), B::default())) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    fetched_dim(req, frame, code, op)
}

/// The inline path of `ZEND_FETCH_DIM_R` (`ZEND_FETCH_DIM_R_SPEC_*`,
/// `Z/Zend/zend_vm_def.h:1970-1995`, `fetch_dim_r_array`): an array's
/// element at a long or string key ([`inline::dim_key`],
/// [`inline::element`]) copied into the temporary with one more owner.
/// `None`, with nothing touched, for a container that is no array, a
/// key the array has not got, a dimension php converts, an operand the
/// fetch would have to give up, and a temporary holding an owner.
#[inline(always)] // the handler's fast half, as php's is in the handler
#[allow(clippy::inline_always)]
fn inline_fetch_dim_r(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    (a, b): (impl Operand, impl Operand),
) -> Option<Edge> {
    let slots = req.slots_mut(frame);
    let (array, (key, free_dim)) = inline_dim(slots, code, op, a, b)?;
    let z = inline::element(array, key)?;
    let out = exec::temporary_slot(code, op.out) as usize;
    if slots.get(out)?.is_refcounted() {
        return None;
    }
    *slots.get_mut(out)? = z;
    z.addref();
    if free_dim {
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
    }
    Some(Ok(advanced()))
}

/// The array and the key a dimension read's inline path works on: the
/// array a CV holds, a reference followed, or one a constant or a
/// temporary holds that counts no owner, so the opcode has nothing to
/// give up (`FREE_OP1`); and the key [`inline_key`] reads, with whether
/// the dimension is given up after. None for every other pair.
#[inline(always)] // as `inline_fetch_dim_r`
#[allow(clippy::inline_always)]
fn inline_dim(
    slots: &[Zval],
    code: &OpArray,
    op: &Opline,
    a: impl Operand,
    b: impl Operand,
) -> Option<(php_heap::Arr, (php_heap::Key, bool))> {
    let container = a.read(slots, code, op.op1);
    let container = match a.kind() {
        OperandKind::Cv => crate::operand::deref(container),
        OperandKind::Unused => return None,
        _ if container.is_refcounted() => return None,
        _ => container,
    };
    Some((container.as_arr()?, inline_key(slots, code, op, b)?))
}

/// The key [`inline::dim_key`] reads out of a dimension, and whether
/// the opcode gives the dimension up after it (`FREE_OP2`): a
/// temporary string, which serves no code as it goes. None for an
/// append and for a temporary holding any other owner.
#[inline(always)] // as `inline_fetch_dim_r`
#[allow(clippy::inline_always)]
fn inline_key(
    slots: &[Zval],
    code: &OpArray,
    op: &Opline,
    b: impl Operand,
) -> Option<(php_heap::Key, bool)> {
    let dim = b.read(slots, code, op.op2);
    let free = matches!(b.kind(), OperandKind::Tmp | OperandKind::Var) && dim.is_refcounted();
    if b.kind() == OperandKind::Unused || (free && dim.kind() != php_heap::ZvalType::String) {
        return None;
    }
    Some((inline::dim_key(dim, b.kind() == OperandKind::Const)?, free))
}

/// `FETCH_DIM_R` past its inline path: the container read as
/// `zend_fetch_dimension_address_read_R` reads it and the read made by
/// [`read_dimension`]. A call of its own, as [`fetched_obj`] is.
#[inline(never)]
fn fetched_dim(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let container = exec::operand_undef(req, frame, code, op.op1_type, op.op1).deref();
    let read = read_dimension(req, frame, code, op, &container, DimRead::Warning)?;
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    computed_into(req, frame, code, op, read)
}

/// `ZEND_FETCH_DIM_IS` (`Z/Zend/zend_vm_def.h`), a link of a chain an
/// `isset`, an `empty` or a `??` reads: [`fetch_dim_r`]'s body under
/// `BP_VAR_IS` ([`DimRead::Silent`]), which reads `null` where the read
/// that warns would have warned. A key the table has not got, an offset
/// past the end of a string, a string offset that is no number, and a
/// container that is neither an array, a string nor an object are all
/// `null` with nothing raised; an offset whose type is no key at all is
/// still php's `TypeError`, since `slow_index_convert` and
/// `zend_illegal_string_offset` raise it under either read. An object
/// that is no `ArrayAccess` is read through its class's own
/// `read_dimension`, as dom's maps and lists are.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r a="1"><x/></r>');
/// var_dump($d->documentElement->attributes[0]->nodeName ?? '/', $d->documentElement->attributes[3] ?? '/', $d->documentElement->childNodes[0]->nodeName ?? '/');
/// try { var_dump((new stdClass)[0] ?? '/'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "/"
/// string(1) "x"
/// Cannot use object of type stdClass as array
/// ```
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
    computed_into(req, frame, code, op, read)
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
    computed_into(req, frame, code, op, read)
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
        // `read_dimension(obj, dim, type, rv)` through the class's own
        // handler: under `BP_VAR_IS` an `ArrayAccess` is
        // `zend_std_read_dimension`'s `offsetExists` then `offsetGet`, a
        // class with no handler is the same `Cannot use object` refusal,
        // and a class php reads in C (dom's maps and lists) is the
        // handler that reads the element whatever the type.
        return Ok(object_held(req, object, |req| {
            let dim = object_dim(req, frame, code, op);
            let table = req.object_handlers(object.heap());
            let fetch = match mode {
                DimRead::Warning => DimensionRead::Warning,
                DimRead::Silent => DimensionRead::Silent,
            };
            table
                .read_dimension(req, object, dim.as_ref(), fetch)
                .map(Owner::hand_on)
        }));
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

/// An object a dimension opline works on, held across `step`: php's
/// `GC_ADDREF(obj)` in `zend_fetch_dimension_address_read`,
/// `zend_fetch_dimension_address`, `ZEND_ASSIGN_DIM` and
/// `zend_binary_assign_op_obj_dim`, taken before the dimension is read,
/// since its `Undefined variable` warning may reach a handler that
/// overwrites the variable the object is in, and given back after the
/// class's handlers answered, `GC_DELREF(obj) == 0` and
/// `zend_objects_store_del` at the last.
///
/// ```php
/// class A implements ArrayAccess {
///     public function &offsetGet($n): mixed { echo "get\n"; return $this->v; }
///     public function offsetSet($n, $v): void { echo "set\n"; }
///     public function offsetUnset($n): void {}
///     public function offsetExists($n): bool { return false; }
///     public function __destruct() { echo "destructed\n"; }
///     public $v = [];
/// }
/// set_error_handler(function ($no, $message) {
///     echo $message, "\n";
///     $GLOBALS['a'] = null;
/// });
/// $a = new A;
/// $a[$c];
/// $a = new A;
/// $a[$c] = 1;
/// $a = new A;
/// $a[$c] .= 'x';
/// $a = new A;
/// $a[$c][] = 1;
/// echo "done\n";
/// ```
/// ```expect
/// Undefined variable $c
/// get
/// destructed
/// Undefined variable $c
/// set
/// destructed
/// Undefined variable $c
/// get
/// Array to string conversion
/// set
/// destructed
/// Undefined variable $c
/// get
/// destructed
/// done
/// ```
fn object_held<T>(req: &mut Request, object: PhpObject, step: impl FnOnce(&mut Request) -> T) -> T {
    let held = Owner::take(req, object.as_value());
    let stepped = step(req);
    held.let_go(req);
    stepped
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
/// write replaces, named by the place that holds it. `Gone` is the
/// array made for `false` that the handler its deprecation reached gave
/// up, `GC_DELREF(ht) == 0`: the write lands nowhere, raises nothing
/// more and leaves `null` in the site's temporary.
enum Container {
    Array(PhpArray),
    Object(PhpObject),
    Text(php_heap::ZvalRef),
    Gone,
}

/// Which of php's ladders a dimension write walks its container by,
/// since the three bodies that make an array out of `null`, an
/// unwritten variable and `false` differ in what they raise and in what
/// they read again after it: `zend_fetch_dimension_address` for
/// `FETCH_DIM_W` and, with [`DimRead::Warning`], `FETCH_DIM_RW`
/// (`Z/Zend/zend_execute.c:2904`); `ZEND_ASSIGN_DIM`
/// (`Z/Zend/zend_vm_def.h:2601`); `ZEND_ASSIGN_DIM_OP`
/// (`Z/Zend/zend_vm_def.h:1156`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Walk {
    Fetch(DimRead),
    Assign,
    AssignOp,
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
/// write lands in (`Z/Zend/zend_execute.c:2904`), and the heads of
/// `ZEND_ASSIGN_DIM` and `ZEND_ASSIGN_DIM_OP`, each by its own ladder
/// (`walk`): the array is [`separated_array`]'s, and a string and an
/// object are handed back for the site to serve, since what each does
/// differs between a fetch and an assignment. A number, `true` and a
/// resource are php's `Error` `Cannot use a scalar value as an array`
/// (`zend_use_scalar_as_array`). `ZEND_UNSET_DIM` shares the walk and not
/// this ladder, since php gives it one of its own
/// (`Z/Zend/zend_vm_def.h:6812`): it makes no array out of what is none.
///
/// `null`, an unwritten variable and `false` become a new array
/// ([`made_array`]), and here the three ladders part. Under a reference
/// the fetch holds the cell's declared types to an array and makes one
/// with no deprecation at all, the assignment holds them and deprecates
/// as it does outside one, and the compound assignment does neither
/// check. Outside one, `FETCH_DIM_RW` and `ASSIGN_DIM_OP` warn of an
/// unwritten variable first (`ZVAL_UNDEFINED_OP1`), with nothing yet laid
/// into it.
///
/// ```php
/// class C { public ?int $p = null; }
/// $o = new C;
/// $r = &$o->p;
/// try { $r[0][1] = 1; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $r[0] += 1;
/// var_dump($o->p);
/// $f = false;
/// $g = &$f;
/// $g[0][1] = 1;
/// $h = false;
/// $h[0][1] = 1;
/// $u[0][1] .= "x";
/// var_dump($f, $h, $u);
/// ```
/// ```expect
/// Cannot auto-initialize an array inside a reference held by property C::$p of type ?int
///
/// Warning: Undefined array key 0 in %s on line 6
/// array(1) {
///   [0]=>
///   int(1)
/// }
///
/// Deprecated: Automatic conversion of false to array is deprecated in %s on line 12
///
/// Warning: Undefined variable $u in %s on line 13
///
/// Warning: Undefined array key 0 in %s on line 13
///
/// Warning: Undefined array key 1 in %s on line 13
/// array(1) {
///   [0]=>
///   array(1) {
///     [1]=>
///     int(1)
///   }
/// }
/// array(1) {
///   [0]=>
///   array(1) {
///     [1]=>
///     int(1)
///   }
/// }
/// array(1) {
///   [0]=>
///   array(1) {
///     [1]=>
///     string(1) "x"
///   }
/// }
/// ```
fn write_container(
    req: &mut Request,
    (frame, code, op): (FrameRef, &OpArray, &Opline),
    place: php_heap::ZvalRef,
    walk: Walk,
) -> Result<Container, Thrown> {
    let cell = exec::value_of(place.read()).as_php_ref().copied();
    let (at, array) = separated_array(req, place)?;
    if let Some(array) = array {
        return Ok(Container::Array(array));
    }
    let held = exec::value_of(at.read());
    match held.as_ref() {
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false) => {}
        ValueRef::Str(_) => return Ok(Container::Text(at)),
        ValueRef::Object(object) => return Ok(Container::Object(*object)),
        _ => {
            return Err(crate::api::raising::throw_error(
                req,
                "Error",
                "Cannot use a scalar value as an array",
            ));
        }
    }
    let typed = cell.filter(|cell| cell.heap().sources() != 0);
    match (walk, cell) {
        (Walk::Fetch(_), Some(_)) => {
            if let Some(cell) = typed {
                verify_ref_array_assignable(req, cell)?;
            }
            let array = req.array()?;
            at.write(php_heap::Zval::array(array.heap()));
            return Ok(Container::Array(array));
        }
        (Walk::Assign, _) => {
            if let Some(cell) = typed {
                verify_ref_array_assignable(req, cell)?;
            }
        }
        (Walk::Fetch(DimRead::Warning) | Walk::AssignOp, None)
            if matches!(held.as_ref(), ValueRef::Undef) && op.op1_type == OperandKind::Cv =>
        {
            let undefined = exec::operand(req, frame, code, OperandKind::Cv, op.op1);
            req.release(undefined);
        }
        _ => {}
    }
    let Some(array) = made_array(req, at, held.as_bool() == Some(false))? else {
        return Ok(Container::Gone);
    };
    Ok(match walk {
        // `goto fetch_from_array`: `Z_ARRVAL_P(container)` read again,
        // and written into as it stands, shared or not.
        Walk::Fetch(_) => exec::value_of(at.read())
            .as_array()
            .map_or(Container::Gone, |array| Container::Array(*array)),
        // `goto try_assign_dim_array`: `SEPARATE_ARRAY(object_ptr)`, so
        // a handler that copied the array keeps the empty one.
        Walk::Assign => match separated_array(req, at)? {
            (_, Some(array)) => Container::Array(array),
            (_, None) => Container::Gone,
        },
        // `goto assign_dim_op_new_array`: the table php made, whatever
        // the place holds now.
        Walk::AssignOp => Container::Array(array),
    })
}

/// The array `ZVAL_ARR(container, zend_new_array(...))` lays into a
/// place that held `null`, nothing or `false`, written over what stands
/// there and before any handler is reached, so a handler php's
/// `zend_false_to_array_deprecated` reaches after it reads the empty
/// array in the variable. The array is held across the deprecation,
/// `GC_ADDREF(ht)`, and `None` is the handler having given up every other
/// owner of it: `GC_DELREF(ht) == 0` and `zend_array_destroy`. A throwable
/// the handler raised is left pending, since php tests the count alone.
///
/// ```php
/// set_error_handler(function () { $GLOBALS['x'] = $GLOBALS['y']; });
/// function x(&$s) { $s[0] = 1; }
/// $y = false;
/// x($y);
/// var_dump($x, $y);
/// set_error_handler(function () { $GLOBALS['z'] = 1; });
/// $z = false;
/// var_dump($z[0] = 2, $z);
/// ```
/// ```expect
/// array(0) {
/// }
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// NULL
/// int(1)
/// ```
fn made_array(
    req: &mut Request,
    at: php_heap::ZvalRef,
    was_false: bool,
) -> Result<Option<PhpArray>, Thrown> {
    let array = req.array()?;
    at.write(php_heap::Zval::array(array.heap()));
    if !was_false {
        return Ok(Some(array));
    }
    let held = Owner::take(req, Value::array(array));
    crate::api::raising::deprecated(req, "Automatic conversion of false to array is deprecated");
    Ok((!held.let_go(req).freed()).then_some(array))
}

/// `zend_assign_to_string_offset` (`Z/Zend/zend_execute.c:2038`),
/// `$s[k] = v` over a string: the offset read as [`string_dim_offset`]
/// reads it, php's warning `Illegal string offset` and `null` for one
/// further from the end than the string is long, and a negative one
/// counted from the end. The value is the first byte of what it reads as
/// a string, php's `Error` `Cannot assign an empty string to a string
/// offset` where it has none and the warning `Only the first byte will
/// be assigned to the string offset` where it has more; what is written
/// is that one byte, and [`TextWrite::Byte`] is what the opline's
/// temporary is handed, that byte as a string of one byte (`ZVAL_CHAR`).
///
/// The string is php's `SEPARATE_STRING` first ([`separate_string`]),
/// a copy laid into the place for a shared or an interned one, so the string written is the
/// place's alone before anything can raise. Then the dimension that is
/// no long, the value that is no string and the value longer than a
/// byte each raise with the string held ([`Owner::take`]), php's
/// `GC_ADDREF(s)`, since a handler the diagnostic reaches may overwrite
/// the variable the string hangs from; where the hold was the string's
/// last owner the write lands nowhere and the temporary is `null`
/// ([`TextWrite::Gone`], `GC_DELREF(s) == 0` and `zend_string_efree`).
/// A string still the place's alone and long enough is written where it
/// stands ([`php_heap::Str::put_byte`]) and allocates nothing, and any
/// other is laid down anew, the `zend_string_extend` php makes for an
/// offset past the end, the bytes between padded with spaces.
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
/// A handler that overwrites the array the string sits in frees the
/// string under the write, at the dimension's diagnostics, at the
/// value's and at the warning over a value longer than a byte; the
/// dimension raises before the value does.
///
/// ```php
/// set_error_handler(function ($no, $message) {
///     echo $message, "\n";
///     $GLOBALS['a'] = '';
/// });
/// $a = ['a'];
/// var_dump($a[0][$d] = 'b');
/// var_dump($a);
/// $a = ['abc'];
/// var_dump($a[0][0] = 7.5);
/// var_dump($a);
/// $a = ['abc'];
/// var_dump($a[0][0] = $v);
/// var_dump($a);
/// $a = ['abc'];
/// var_dump($a[0][0] = 'long');
/// var_dump($a);
/// set_error_handler(function ($no, $message) { echo $message, "\n"; });
/// $s = 'abc';
/// var_dump($s[$d] = 5, $s);
/// ```
/// ```expect
/// Undefined variable $d
/// String offset cast occurred
/// NULL
/// string(0) ""
/// Only the first byte will be assigned to the string offset
/// NULL
/// string(0) ""
/// Undefined variable $v
/// NULL
/// string(0) ""
/// Only the first byte will be assigned to the string offset
/// NULL
/// string(0) ""
/// Undefined variable $d
/// String offset cast occurred
/// string(1) "5"
/// string(3) "5bc"
/// ```
///
/// The place is held standing too ([`Keeper`]): the array a
/// `FETCH_DIM_W` walked to, or the reference the place is the value of,
/// has one more owner across the diagnostics, and the write lands only
/// where that table is still someone else's and the place still holds
/// the string. A handler that copies the string out and then gives the
/// array up leaves the write nowhere, where php writes through the
/// entry of the table it freed; a handler that shares the array sees
/// the byte written, as php's does.
///
/// ```php
/// set_error_handler(function () { $GLOBALS['b'] = $GLOBALS['a'][0]; $GLOBALS['a'] = null; });
/// $a = [str_repeat('abc', 1)];
/// $a[0][9] = 'long';
/// var_dump($a, strlen($b) > 0);
/// set_error_handler(function () { $GLOBALS['c'] = $GLOBALS['a']; });
/// $a = [str_repeat('abc', 1)];
/// var_dump($a[0][1] = 'long', $a === $c, $c[0]);
/// set_error_handler(function () { $GLOBALS['a'] = null; });
/// $a = [[str_repeat('abc', 1)]];
/// var_dump($a[0][0][1] = 'long', $a);
/// ```
/// ```expect
/// NULL
/// bool(true)
/// string(1) "l"
/// bool(true)
/// string(3) "alc"
/// NULL
/// NULL
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
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    data: Opline,
    at: php_heap::ZvalRef,
    keeper: &Keeper,
) -> Result<TextWrite, Thrown> {
    let Some(text) = separate_string(req, at)? else {
        return Ok(TextWrite::Gone);
    };
    let string = Value::string(text);
    // A write is `BP_VAR_W`, which `zend_check_string_offset` runs
    // through the read's ladder with php's errors on, so no arm of it
    // answers `None` here.
    let dim = exec::operand_undef(req, frame, code, op.op2_type, op.op2);
    let offset = if let ValueRef::Int(n) = dim.as_ref() {
        n
    } else {
        let offset = string_held(req, keeper, at, string, |req| {
            // `ZVAL_UNDEFINED_OP2()` and the cast warning after it.
            let dim = exec::operand(req, frame, code, op.op2_type, op.op2);
            string_dim_offset(req, &dim, DimRead::Warning)
        })?;
        match offset {
            Some(Some(offset)) => offset,
            Some(None) | None => return Ok(TextWrite::Gone),
        }
    };
    let len = i64::try_from(text.len()).unwrap_or(i64::MAX);
    if offset < -len {
        crate::api::raising::warning(req, &format!("Illegal string offset {offset}"));
        return Ok(TextWrite::Gone);
    }
    let offset = if offset < 0 {
        offset.saturating_add(len)
    } else {
        offset
    };
    let Some((count, byte)) = offset_byte(req, frame, code, data, keeper, at, string)? else {
        return Ok(TextWrite::Gone);
    };
    if count == 0 {
        return Err(crate::api::raising::throw_error(
            req,
            "Error",
            "Cannot assign an empty string to a string offset",
        ));
    }
    if count != 1
        && string_held(req, keeper, at, string, |req| {
            crate::api::raising::warning(
                req,
                "Only the first byte will be assigned to the string offset",
            );
            Ok(())
        })?
        .is_none()
    {
        return Ok(TextWrite::Gone);
    }
    let index = usize::try_from(offset).unwrap_or(usize::MAX);
    let s = text.heap();
    if s.refcount() == 1 && index < text.len() {
        s.put_byte(index, byte);
    } else {
        let need = index.saturating_add(1).max(text.len());
        let fresh = req.string_with(need, |bytes| {
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
        req.release(string);
    }
    Ok(TextWrite::Byte(byte))
}

/// php's `SEPARATE_STRING` over the string at `at`: the string itself
/// where the place is its one owner, and otherwise a copy of it laid
/// into the place, the one the place had given up (`GC_DELREF`).
fn separate_string(req: &mut Request, at: php_heap::ZvalRef) -> Result<Option<PhpString>, Thrown> {
    let held = exec::value_of(at.read());
    let Some(text) = held.as_php_string().copied() else {
        return Ok(None);
    };
    let s = text.heap();
    if !s.is_interned() && !s.is_persistent() && s.refcount() == 1 {
        return Ok(Some(text));
    }
    let fresh = req.string_with(text.len(), |bytes| {
        for (slot, byte) in bytes.iter_mut().zip(text.as_bytes()) {
            *slot = *byte;
        }
    })?;
    at.write(php_heap::Zval::string(fresh.heap()));
    // Another owner stands, or none was ever counted.
    req.release(held);
    Ok(Some(fresh))
}

/// The value of a write over a string offset read for the byte it
/// writes: its length and its first byte. A string is read as it
/// stands; anything else is read as a string with `string` held, php's
/// `zval_undefined_cv()` for an unwritten variable and
/// `zval_try_get_string_func()` after it, and `None` where the hold was
/// the string's last owner.
fn offset_byte(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    data: Opline,
    keeper: &Keeper,
    at: php_heap::ZvalRef,
    string: Value,
) -> Result<Option<(usize, u8)>, Thrown> {
    let first = |bytes: &[u8]| (bytes.len(), bytes.first().copied().unwrap_or(0));
    if let Some(value) = exec::operand_undef(req, frame, code, data.op1_type, data.op1)
        .deref()
        .as_php_string()
    {
        return Ok(Some(first(value.as_bytes())));
    }
    string_held(req, keeper, at, string, |req| {
        let value = exec::take_operand(req, frame, code, data.op1_type, data.op1);
        let written = crate::ops::to_string(req, &value);
        req.release(value);
        let written = written?;
        let read = first(written.as_bytes());
        req.release(Value::string(written));
        Ok(read)
    })
}

/// What a write over a string offset handed the opline's temporary: the
/// byte written, or `null` where the write landed nowhere.
enum TextWrite {
    Byte(u8),
    Gone,
}

/// `step` with the string a write is landing in held across it, php's
/// `GC_ADDREF(s)` before a diagnostic or a conversion and `GC_DELREF(s)
/// == 0` after it: what `step` answered where the string stands in its
/// place, `None` where the hold was its last owner and it is freed or
/// where its place no longer stands or holds it ([`Keeper::stands`]),
/// and the throw where one is pending (`EG(exception)`), which the
/// handler after the write raises whether the string stands or not.
fn string_held<T>(
    req: &mut Request,
    keeper: &Keeper,
    at: php_heap::ZvalRef,
    string: Value,
    step: impl FnOnce(&mut Request) -> Result<T, Thrown>,
) -> Result<Option<T>, Thrown> {
    let held = Owner::take(req, string);
    let stepped = step(req);
    let left = held.let_go(req);
    let stepped = stepped?;
    none_pending(req)?;
    Ok((!left.freed() && keeper.stands(at, string)).then_some(stepped))
}

/// What keeps the place a write over a string offset lands in standing
/// across the diagnostics that may reach a handler, which may give up
/// every owner of the array the place is an entry of.
enum Keeper {
    /// A `CV`'s slot, or a `VAR` holding the string itself: the frame's,
    /// which stands while the handler runs.
    Frame,
    /// An owner of the array or object the place is an entry of, as the
    /// `FETCH_DIM_W` before walked to it ([`Request::hold_walk`]), or of
    /// the reference the place is the value of.
    Held(Owner<Value>),
    /// An entry of a table no owner was taken of: a property's slot, a
    /// static property's, a symbol table's. The string's own hold is all
    /// the write goes by, as php's is.
    Unheld,
}

impl Keeper {
    /// Whether the place still stands and still holds `string` once a
    /// diagnostic has reached a handler: its table owned by someone
    /// besides the hold, and the string in it the one the write started
    /// from.
    fn stands(&self, at: php_heap::ZvalRef, string: Value) -> bool {
        let holds = || {
            let held = exec::value_of(at.read());
            match (held.as_php_string(), string.as_php_string()) {
                (Some(held), Some(string)) => held.heap().is_same(string.heap()),
                _ => false,
            }
        };
        match self {
            Keeper::Frame => holds(),
            Keeper::Held(hold) => hold.refcount() > 1 && holds(),
            Keeper::Unheld => true,
        }
    }

    /// The owner given back, `GC_DELREF`: where it was the table's last,
    /// the table goes and the write with it.
    fn give_back(self, req: &mut Request) {
        if let Keeper::Held(hold) = self {
            let _ = hold.let_go(req);
        }
    }
}

/// What keeps the string place `text_at` of the `ASSIGN_DIM` `op` at
/// opline `at` standing, taken before PHP code can be reached: `place`
/// is what `op1` names and `text_at` where [`write_container`] found
/// the string, the value of a reference where `place` holds one. A
/// `VAR`'s `IS_INDIRECT` is an entry of the table the `FETCH_DIM_W`
/// just before walked to, which php's delayed compile puts next to the
/// write.
fn keeper(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    (at, op): (u32, &Opline),
    place: php_heap::ZvalRef,
    text_at: php_heap::ZvalRef,
) -> Keeper {
    let cell = exec::value_of(place.read());
    if text_at != place && cell.as_php_ref().is_some() {
        return Keeper::Held(Owner::take(req, cell));
    }
    match op.op1_type {
        OperandKind::Var => {
            let indirect = req
                .slot_ptr(frame, exec::temporary_slot(code, op.op1))
                .and_then(|var| var.read().as_indirect());
            if indirect.is_none() {
                return Keeper::Frame;
            }
            let walked = at
                .checked_sub(1)
                .and_then(|before| exec::opline_at(code, before))
                .is_some_and(|before| {
                    before.opcode == Opcode::FetchDimW
                        && before.out_type == OperandKind::Var
                        && before.out == op.op1
                });
            match walked.then(|| req.hold_walk(place)).flatten() {
                Some(hold) => Keeper::Held(hold),
                None => Keeper::Unheld,
            }
        }
        OperandKind::Cv => Keeper::Frame,
        OperandKind::Const | OperandKind::Tmp | OperandKind::Unused => Keeper::Unheld,
    }
}

/// Where a dimension write over an array lands, before the table is
/// asked: the next index for the `UNUSED` dimension of `$a[]`, the key
/// the dimension makes, or nowhere, where a handler the key's
/// diagnostics reached gave the array up or took an owner of it.
#[derive(Clone, Copy)]
enum WriteAt {
    Next,
    At(php_heap::Key),
    Gone,
}

/// The dimension of a write over `array` read and made a key, php's
/// `slow_index_convert_w` (`Z/Zend/zend_execute.c:2710`) behind the
/// long and string arms that raise nothing. The key is the ladder
/// [`crate::ops::array_offset`] holds for every array write, a `CONST`
/// string taken as it is since the compiler applied
/// `zend_handle_numeric_dim` to it. Every other dimension may raise
/// before it is a key, `Undefined variable` for a variable nothing
/// wrote and the `null` offset's deprecation after it, and a handler
/// the diagnostic reaches may overwrite the variable the array hangs
/// from, so the array is held across them ([`Owner::take`]) and the
/// write lands nowhere ([`WriteAt::Gone`]) where anything but its one
/// owner is left after (`GC_DELREF(ht) != 1`), the array destroyed with
/// the hold where that was the last.
///
/// ```php
/// set_error_handler(function ($no, $message) {
///     echo $message, "\n";
///     $GLOBALS['a'] = null;
/// });
/// $a[$c] = 'x';
/// var_dump($a);
/// $a[null] = 1;
/// var_dump($a);
/// $a[$c][$c] = 'x';
/// var_dump($a);
/// $a = [];
/// $b = &$a;
/// set_error_handler(function () use (&$copy) { $copy = $GLOBALS['a']; });
/// $a[null] = 2;
/// var_dump($a, $copy);
/// ```
/// ```expect
/// Undefined variable $c
/// NULL
/// Using null as an array offset is deprecated, use an empty string instead
/// NULL
/// Undefined variable $c
/// Undefined variable $c
/// Using null as an array offset is deprecated, use an empty string instead
/// NULL
/// array(0) {
/// }
/// array(0) {
/// }
/// ```
fn write_at(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    array: PhpArray,
) -> Result<WriteAt, Thrown> {
    if op.op2_type == OperandKind::Unused {
        return Ok(WriteAt::Next);
    }
    let constant = op.op2_type == OperandKind::Const;
    let dim = exec::operand_undef(req, frame, code, op.op2_type, op.op2);
    if matches!(dim.as_ref(), ValueRef::Int(_) | ValueRef::Str(_)) {
        return crate::ops::array_offset(req, &dim, constant).map(WriteAt::At);
    }
    // `ZVAL_UNDEFINED_OP2()`, and the `null` arm it falls through to
    // only where the array is still the write's alone.
    let dim = if dim.tag() == TypeTag::Undef {
        let Some(dim) = held_alone(req, array, |req| {
            Ok(exec::operand(req, frame, code, op.op2_type, op.op2))
        })?
        else {
            return Ok(WriteAt::Gone);
        };
        dim
    } else {
        dim
    };
    let key = held_alone(req, array, |req| {
        crate::ops::array_offset(req, &dim, constant)
    })?;
    Ok(key.map_or(WriteAt::Gone, WriteAt::At))
}

/// `step` with `array` held across it, php's `GC_ADDREF(ht)` before a
/// diagnostic and `GC_DELREF(ht) != 1` after it: what `step` answered
/// where the array is still the write's alone, `None` where a handler
/// gave it up or took an owner of it, and the throw where one is
/// pending, `if (EG(exception)) return IS_NULL`.
fn held_alone<T>(
    req: &mut Request,
    array: PhpArray,
    step: impl FnOnce(&mut Request) -> Result<T, Thrown>,
) -> Result<Option<T>, Thrown> {
    let held = Owner::take(req, Value::array(array));
    let stepped = step(req);
    let left = held.let_go(req);
    let stepped = stepped?;
    none_pending(req)?;
    Ok((!left.not_alone()).then_some(stepped))
}

/// The entry a dimension write lands in, php's
/// `zend_fetch_dimension_address_inner` under `BP_VAR_W`
/// (`Z/Zend/zend_execute.c:2802`): the entry at the key in place, laid
/// down holding `null` where the table has none (`zend_hash_lookup`), or
/// one appended for the `UNUSED` dimension of `$a[]`
/// (`zend_hash_next_index_insert`). [`Landing::Occupied`] is the next
/// index taken at `ZEND_LONG_MAX`, `zend_cannot_add_element`'s `Error`
/// at the site.
fn write_entry(req: &mut Request, array: PhpArray, at: WriteAt) -> Result<Landing, Thrown> {
    match at {
        WriteAt::Next => req.array_next_entry(array.heap()).map(Landing::from),
        WriteAt::At(key) => req.array_lookup(array.heap(), key).map(Landing::Entry),
        WriteAt::Gone => Ok(Landing::Gone),
    }
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
/// temporary and reaches nothing. An `&offsetGet` hands back the
/// reference, the place itself, and warns of nothing:
///
/// ```php
/// class A implements ArrayAccess {
///     public $data = ['k' => []];
///     public function &offsetGet($k): mixed { return $this->data[$k]; }
///     public function offsetExists($k): bool { return true; }
///     public function offsetSet($k, $v): void {}
///     public function offsetUnset($k): void {}
/// }
/// $a = new A;
/// $a['k']['x'] = 1;
/// var_dump($a->data);
/// ```
/// ```expect
/// array(1) {
///   ["k"]=>
///   array(1) {
///     ["x"]=>
///     int(1)
///   }
/// }
/// ```
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn fetch_dim_w<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    let kinds = (A::default(), B::default());
    if let Some(edge) = inline_fetch_dim_w(req, frame, (code, op, at), DimRead::Silent, kinds) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    fetched_dim_place(req, frame, code, op)
}

/// `FETCH_DIM_W` past its inline path: [`fetch_dim_place`] under
/// `BP_VAR_W`. A call of its own, as [`fetched_obj`] is.
#[inline(never)]
fn fetched_dim_place(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    fetch_dim_place(req, frame, code, op, DimRead::Silent, Keep::Nothing)
}

/// The inline path of `ZEND_FETCH_DIM_W` and `ZEND_FETCH_DIM_RW` over a
/// CV (`zend_fetch_dimension_address_W` and `_RW` into
/// `zend_fetch_dimension_address_inner`): an array the CV alone owns,
/// or the one a reference it holds names, so `SEPARATE_ARRAY` copies
/// nothing, whose element at the key holds a value, named in place by
/// the `VAR` as an `IS_INDIRECT` for the write after it; and the same
/// over a `VAR` a `FETCH_*_W` before it left naming such an array,
/// `$a[1][0] = ...`'s inner link. `read` is the fetch's: under
/// [`DimRead::Silent`], `_W`, a key the array has not got is laid down
/// here, and under [`DimRead::Warning`], `_RW`, it is the kernel's,
/// which warns `Undefined array key` before it lays the entry down; that
/// is all the two differ in. `None`, with nothing touched, for every
/// other container, an element that is a reference or a symbol table's
/// `IS_INDIRECT`, and a `VAR` holding an owner.
///
/// ```php
/// function w(array &$a, array &$n) { $a[0][1] = 2; $a[] = 3; $a[1]--; $a[5]++; $n[0][0] += 1; return $a; }
/// $a = [[1], 7]; $n = [[1]]; $b = $a; $r = &$a[1];
/// var_dump(w($a, $n), $b[1], $r, $n);
/// $c = [1, 2]; $c[1]--; $c[0]++; --$c[0]; $c[1] *= 3;
/// var_dump($c);
/// ```
/// ```expect
///
/// Warning: Undefined array key 5 in %s on line 2
/// array(4) {
///   [0]=>
///   array(2) {
///     [0]=>
///     int(1)
///     [1]=>
///     int(2)
///   }
///   [1]=>
///   &int(6)
///   [2]=>
///   int(3)
///   [5]=>
///   int(1)
/// }
/// int(7)
/// int(6)
/// array(1) {
///   [0]=>
///   array(1) {
///     [0]=>
///     int(2)
///   }
/// }
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(3)
/// }
/// ```
#[inline(always)] // as `inline_fetch_dim_r`
#[allow(clippy::inline_always)]
fn inline_fetch_dim_w(
    req: &mut Request,
    frame: FrameRef,
    (code, op, at): (&OpArray, &Opline, u32),
    read: DimRead,
    (a, b): (impl Operand, impl Operand),
) -> Option<Edge> {
    let out = exec::temporary_slot(code, op.out) as usize;
    if req.slots_mut(frame).get(out)?.is_refcounted() {
        return None;
    }
    let placed = match owned_entry(req, frame, (code, op, at), read, (a, b))? {
        Ok(placed) => placed,
        Err(edge) => return Some(edge),
    };
    free_dim_operands(req, frame, code, op, (a, placed.free_dim));
    *req.slots_mut(frame).get_mut(out)? = Zval::indirect(placed.entry);
    // The table the entry stands in, for the write after this to hold
    // across what it serves, as every `FETCH_DIM_W` notes it.
    let table = Value::array(PhpArray::of(placed.table));
    req.note_walk(Some((placed.entry, table)));
    Some(Ok(advanced()))
}

/// The array a dimension write's inline path writes into, as
/// `GET_OP1_ZVAL_PTR_PTR_UNDEF(BP_VAR_W)` and the handler's
/// `Z_ISREF_P` arm find it: the array a CV holds, or the place a
/// `FETCH_*_W` left in a `VAR` as an `IS_INDIRECT` holds, and in either
/// the one a reference there names (`Z_REFVAL_P`, `goto
/// try_assign_dim_array`), where that array is refcounted with one
/// owner, so `SEPARATE_ARRAY` copies nothing. None for every other
/// container.
#[inline(always)] // as `inline_fetch_dim_r`
#[allow(clippy::inline_always)]
fn owned_array(
    slots: &[Zval],
    code: &OpArray,
    op: &Opline,
    a: impl Operand,
) -> Option<php_heap::Arr> {
    let held = a.read(slots, code, op.op1);
    let held = match a.kind() {
        OperandKind::Cv => held,
        OperandKind::Var => held.as_indirect()?.read(),
        _ => return None,
    };
    let held = crate::operand::deref(held);
    let array = held.as_arr()?;
    (held.is_refcounted() && array.refcount() == 1).then_some(array)
}

/// The element a dimension write's inline path writes in place, as
/// `zend_fetch_dimension_address_inner` finds or lays it down: in the
/// array [`owned_array`] finds, the entry at the key [`inline_key`]
/// reads, or the next one for an append. Under [`DimRead::Silent`],
/// `BP_VAR_W`, a key the table has not got is laid down holding `null`
/// (`zend_hash_lookup`, `zend_hash_next_index_insert`), which serves no
/// code; under [`DimRead::Warning`], `BP_VAR_RW`, it and an append are
/// the kernel's, which warns first. None, with nothing touched, for
/// every other container, an entry that is a reference or a symbol
/// table's `IS_INDIRECT`, and an append past the last index, whose
/// `Error` is the kernel's; the memory limit a laying-down meets is
/// raised at the opline at `at`, the `Err`.
#[inline(always)] // as `inline_fetch_dim_r`
#[allow(clippy::inline_always)]
fn owned_entry(
    req: &mut Request,
    frame: FrameRef,
    (code, op, at): (&OpArray, &Opline, u32),
    read: DimRead,
    (a, b): (impl Operand, impl Operand),
) -> Option<Result<OwnedEntry, Edge>> {
    let slots = req.slots_mut(frame);
    let array = owned_array(slots, code, op, a)?;
    let key = match b.kind() {
        OperandKind::Unused => None,
        _ => Some(inline_key(slots, code, op, b)?),
    };
    // `ZEND_HASH_INDEX_LOOKUP`'s shape: the hit found inline, and only a
    // miss handed to the lookup that lays the entry down.
    let found = match key {
        Some((key, _)) => array.value_ref(key),
        None => None,
    };
    let entry = match (found, read) {
        (Some(entry), _) => entry,
        (None, DimRead::Warning) => return None,
        (None, DimRead::Silent) => match laid_down(req, (frame, at), array, key)? {
            Ok(entry) => entry,
            Err(edge) => return Some(Err(edge)),
        },
    };
    let plain = !matches!(
        entry.read().kind(),
        php_heap::ZvalType::Undef | php_heap::ZvalType::Reference | php_heap::ZvalType::Indirect
    );
    plain.then_some(Ok(OwnedEntry {
        entry,
        table: array,
        free_dim: key.is_some_and(|(_, free)| free),
    }))
}

/// The miss of [`owned_entry`] under `BP_VAR_W`: the entry laid down
/// holding `null` at the key (`zend_hash_lookup`) or appended for `$a[]`
/// (`zend_hash_next_index_insert`); None for an append past the last
/// index, whose `Error` is the kernel's, and the memory limit raised at
/// the opline at `at`, the `Err`.
#[inline(never)]
fn laid_down(
    req: &mut Request,
    (frame, at): (FrameRef, u32),
    array: php_heap::Arr,
    key: Option<(php_heap::Key, bool)>,
) -> Option<Result<php_heap::ZvalRef, Edge>> {
    let laid = match key {
        Some((key, _)) => req.array_lookup(array, key).map(Some),
        None => req.array_next_entry(array),
    };
    match laid {
        Ok(entry) => entry.map(Ok),
        Err(thrown) => {
            req.vm_mut().stand(frame, at);
            Some(Err(threw(req, frame, &thrown)))
        }
    }
}

/// An entry [`owned_entry`] found or laid down: the place, the table it
/// stands in, and whether the dimension is a temporary
/// string the opcode gives up after the write (`FREE_OP2`).
#[derive(Clone, Copy)]
struct OwnedEntry {
    entry: php_heap::ZvalRef,
    table: php_heap::Arr,
    free_dim: bool,
}

/// `FREE_OP1_VAR_PTR()` and `FREE_OP2()` after a dimension write's
/// inline path: the `VAR` the container was named through, an
/// `IS_INDIRECT` that owns nothing, emptied as [`exec::free_operand`]
/// empties it, and a temporary string the dimension was, given up,
/// which serves no code.
#[inline(always)] // as `inline_fetch_dim_r`
#[allow(clippy::inline_always)]
fn free_dim_operands(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    (a, free_dim): (impl Operand, bool),
) {
    if a.kind() == OperandKind::Var
        && let Some(var) = a
            .slot(code, op.op1)
            .and_then(|at| req.slots_mut(frame).get_mut(at))
    {
        *var = Zval::UNDEF;
    }
    if free_dim {
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
    }
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
#[cfg_attr(not(debug_assertions), inline(always))] // design §5.8, as `fetch_dim_r`
#[cfg_attr(debug_assertions, inline(never))] // a debug `serve_frame` holds every inlined arm's locals
#[allow(clippy::inline_always)]
pub(crate) fn fetch_dim_rw<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    let kinds = (A::default(), B::default());
    if let Some(edge) = inline_fetch_dim_w(req, frame, (code, op, at), DimRead::Warning, kinds) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
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
    let landing = write_at(req, frame, code, op, array).and_then(|at| match read {
        DimRead::Warning => rw_entry(req, array, at),
        DimRead::Silent => write_entry(req, array, at),
    });
    Ok(match landing {
        Ok(Landing::Entry(entry)) => Ok(php_heap::Zval::indirect(entry)),
        Ok(Landing::Gone) => Ok(php_heap::Zval::null()),
        Ok(Landing::Occupied) => Err(crate::api::raising::cannot_add_element(req)),
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
    let Some(place) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::error(
            "InternalError: ZEND_FETCH_DIM_W over a constant or a temporary, which php compiles no write over",
        ));
    };
    let container = write_container(req, (frame, code, op), place, Walk::Fetch(read));
    let ran_on = req.pending();
    let (written, table) = match container {
        Ok(Container::Array(array)) => (
            array_place(req, frame, code, op, array, read)?,
            Some(Value::array(array)),
        ),
        Ok(Container::Object(object)) => {
            // php's `type`: `BP_VAR_RW` warns of a key it lays down,
            // `BP_VAR_W` lays it down in silence.
            let fetch = match read {
                DimRead::Warning => DimensionRead::ReadWrite,
                DimRead::Silent => DimensionRead::Write,
            };
            let written = object_held(req, object, |req| {
                object_place(req, frame, code, op, object, fetch)
            });
            (written, Some(Value::object(object)))
        }
        Ok(Container::Text(_)) => (Err(text_dim_place(req, frame, code, op, false)), None),
        // `goto return_null`: `ZVAL_NULL(result)`.
        Ok(Container::Gone) => (Ok(php_heap::Zval::null()), None),
        Err(thrown) => (Err(thrown), None),
    };
    let written = written.map_err(|thrown| req.raised_over(thrown, ran_on));
    lay_dim_place(req, frame, code, op, written, keep, table)
}

/// `zend_fetch_dimension_address`'s string arm under a write or, with
/// `unset`, an unset, which is always php's `Error`: `[] operator not
/// supported for strings` for the append
/// (`zend_use_new_element_for_string`), and otherwise
/// `zend_check_string_offset`'s diagnostics and then
/// `zend_wrong_string_offset_error`, whose sentence the opline's
/// `extended_value` picks.
fn text_dim_place(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    unset: bool,
) -> Thrown {
    if op.op2_type == OperandKind::Unused {
        return crate::api::raising::throw_error(
            req,
            "Error",
            "[] operator not supported for strings",
        );
    }
    let dim = exec::operand(req, frame, code, op.op2_type, op.op2);
    let checked = if unset {
        unset_string_offset(req, &dim)
    } else {
        string_dim_offset(req, &dim, DimRead::Warning).map(|_| ())
    };
    if let Err(thrown) = checked {
        return thrown;
    }
    // `zend_wrong_string_offset_error` raises nothing over a throwable a
    // handler of the offset's warning left pending, and names
    // `FETCH_LIST_W` by its opcode alone.
    if req.is_pending() {
        return Thrown::of(None);
    }
    if op.opcode == Opcode::FetchListW {
        return crate::api::raising::throw_error(
            req,
            "Error",
            "Cannot create references to/from string offsets",
        );
    }
    let message = match op.extended_value {
        FETCH_DIM_REF => "Cannot create references to/from string offsets",
        FETCH_DIM_OBJ => "Cannot use string offset as an object",
        FETCH_DIM_INCDEC => "Cannot increment/decrement string offsets",
        _ => "Cannot use string offset as an array",
    };
    crate::api::raising::throw_error(req, "Error", message)
}

/// `zend_check_string_offset` under `BP_VAR_UNSET`: what
/// [`string_dim_offset`] reads with its one difference, that an offset
/// php cannot read as an integer is `zend_illegal_container_offset`'s
/// unset sentence, the `Error` `Cannot unset string offsets`, and a
/// string with anything after its integer warns of nothing.
fn unset_string_offset(req: &mut Request, dim: &Value) -> Result<(), Thrown> {
    let dim = dim.deref();
    let legal = match dim.as_ref() {
        ValueRef::Int(_) => true,
        ValueRef::Str(s) => matches!(
            php_heap::numeric_prefix(s.as_bytes()),
            Some((php_heap::Number::Long(_), _))
        ),
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(_) | ValueRef::Float(_) => {
            crate::api::raising::warning(req, "String offset cast occurred");
            true
        }
        _ => false,
    };
    if legal {
        return Ok(());
    }
    Err(crate::api::raising::throw_error(
        req,
        "Error",
        "Cannot unset string offsets",
    ))
}

/// What a `FETCH_DIM_W`, `_RW` or `_UNSET` found laid into its `VAR`:
/// the dimension freed, the place written, and the container `VAR`
/// given up after it unless `keep` says the next opline reads it
/// (`FREE_OP1_VAR_PTR`); a throwable leaves the `VAR` empty. The entry
/// the `VAR` is left pointing at is noted with `table`, the array or
/// object it is an entry of, for the `ASSIGN_DIM` after it
/// ([`Request::note_walk`]); a `VAR` left holding anything else notes
/// nothing.
fn lay_dim_place(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    written: Result<php_heap::Zval, Thrown>,
    keep: Keep,
    table: Option<Value>,
) -> Edge {
    req.note_walk(None);
    let out = exec::temporary_slot(code, op.out);
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    let previous = req.take_slot(frame, out);
    req.release(previous);
    let written = match written {
        Ok(written) => written,
        Err(thrown) => {
            let thrown = req.free_pending(thrown, |req| {
                if keep == Keep::Nothing {
                    exec::free_operand(req, frame, code, op.op1_type, op.op1);
                }
            });
            return threw(req, frame, &thrown);
        }
    };
    let Some(var) = req.slot_ptr(frame, out) else {
        return Err(Fatal::error(
            "InternalError: ZEND_FETCH_DIM_W writes a VAR the frame does not have",
        ));
    };
    var.write(written);
    if keep == Keep::Nothing {
        exec::free_container_var(req, frame, code, op);
    }
    req.note_walk(table.and_then(|table| Some((var.read().as_indirect()?, table))));
    Ok(advance(req, frame))
}

/// `zend_fetch_dimension_address`'s object arm under `BP_VAR_W`,
/// `BP_VAR_RW` or `BP_VAR_UNSET` (`fetch`, which the class's
/// `read_dimension` is handed): the place it hands back for the
/// dimension, or the value written into nothing with php's notice where
/// it is no place and no object.
fn object_place(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    object: PhpObject,
    fetch: DimensionRead,
) -> Result<php_heap::Zval, Thrown> {
    let dim = object_dim(req, frame, code, op);
    let table = req.object_handlers(object.heap());
    match table.dimension_place(req, object, dim.as_ref(), fetch) {
        Ok(Some(place)) => {
            // `else if (Z_REFCOUNT_P(retval) == 1) ZVAL_UNREF(retval)`:
            // a reference the storage alone holds is the value again.
            let at = place.heap();
            let held = Value::of_zval(at.read());
            if held.as_php_ref().is_some() && held.refcount() == 1 {
                let value = req.unwrap_reference(held);
                at.write(value.to_zval());
            }
            Ok(php_heap::Zval::indirect(at))
        }
        Err(thrown) => Err(thrown),
        Ok(None) => match table
            .read_dimension(req, object, dim.as_ref(), fetch)
            .map(Owner::hand_on)
        {
            // `zend_fetch_dimension_address`'s object arm: a
            // reference is the place itself and warns of
            // nothing, and one the `VAR` is the last owner of is
            // the value it holds (`ZVAL_UNREF`).
            Ok(value) if value.as_php_ref().is_some() => {
                let value = if value.refcount() == 1 {
                    exec::owned_deref(req, value)
                } else {
                    value
                };
                Ok(value.to_zval())
            }
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
        },
    }
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
        return Err(Fatal::error(
            "InternalError: ZEND_CLONE over $this, which no frame here carries",
        ));
    }
    let operand = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    let cloned = match operand.as_object().copied() {
        Some(object) => req.clone_object(object, None).map(Owner::hand_on),
        None => Err(crate::api::raising::throw_error(
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
    set_out(req, frame, code, op, object.as_value());
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
        // `ce && instanceof_function(...)`: a silent `FETCH_CLASS` found
        // nothing.
        (Some(_), OperandKind::Var)
            if req.slot(frame, exec::temporary_slot(code, op.op2)).tag() == TypeTag::Null =>
        {
            false
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
    smart_branch::<T, branch::Peeked>(req, frame, code, op, Ok(Value::bool(answer)))
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
        crate::api::call::autoload_keyed(req, written.as_str_bytes().unwrap_or_default())?
    {
        if let Some(slot) = slot {
            req.prime(frame, slot, Cached::Class(class));
        }
        return Ok(class);
    }
    Err(crate::api::raising::class_not_found(
        req,
        written.as_str_bytes().unwrap_or_default(),
    ))
}

/// `ZEND_FETCH_CLASS_MASK` (`Z/Zend/zend_compile.h:1034`): the bits of a
/// class operand's number that are its fetch type, the flags above them.
const FETCH_CLASS_MASK: u32 = 0x0f;

/// `ZEND_FETCH_CLASS_NO_AUTOLOAD` and `ZEND_FETCH_CLASS_SILENT`
/// (`Z/Zend/zend_compile.h:1035-1036`).
const FETCH_CLASS_NO_AUTOLOAD: u32 = 0x80;
const FETCH_CLASS_SILENT: u32 = 0x100;

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
    req.replace_slot(frame, slot, Value::int(i64::from(class.0)));
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
            let thrown = crate::api::raising::throw_error(
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
                    let thrown = crate::api::raising::throw_error(
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
                .or_else(|| req.vm().at(frame).and_then(exec::Header::called_scope))
                .unwrap_or(scope),
        }
    } else {
        let value = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
        let class = value.as_object().map(PhpObject::class);
        let Some(class) = class else {
            let shown = crate::ops::value_name(req, &value);
            let thrown = crate::api::raising::throw_error(
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
    let name = match req.declared_string(&name) {
        Ok(name) => name,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    set_out(req, frame, code, op, name.into_value().hand_on());
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
///
/// Under `ZEND_FETCH_CLASS_SILENT`, which `instanceof` compiles with
/// beside `ZEND_FETCH_CLASS_NO_AUTOLOAD`, a name nothing declares is no
/// class and no error, and the operation after it reads the null:
///
/// ```php
/// namespace Foo; class Bar {}
/// $n = 'Bar'; $f = 'Foo\Bar';
/// spl_autoload_register(function ($c) { echo "autoload $c\n"; });
/// var_dump(new \stdClass instanceof $n, new Bar instanceof $n, new Bar instanceof $f);
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
pub(crate) fn fetch_class<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let class = if op.op2_type == OperandKind::Unused {
        scope_class(req, op.op1).map(Some)
    } else {
        let value = exec::operand(req, frame, code, op.op2_type, op.op2).deref();
        let class = if let Some(object) = value.as_object() {
            Ok(Some(object.class()))
        } else if let Some(name) = value.as_str_bytes() {
            let name = name.to_vec();
            match (
                op.op1 & FETCH_CLASS_SILENT,
                op.op1 & FETCH_CLASS_NO_AUTOLOAD,
            ) {
                (0, _) => crate::api::call::fetch_class(req, &name).map(Some),
                (_, 0) => crate::api::call::autoload(req, &name),
                _ => crate::api::call::lookup_class(req, &name),
            }
        } else {
            Err(crate::api::raising::throw_error(
                req,
                "Error",
                "Class name must be a valid object or a string",
            ))
        };
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
        class
    };
    match class {
        Ok(Some(class)) => {
            hold_class(req, frame, code, op.out, class);
            Ok(advance(req, frame))
        }
        // `Z_CE_P(EX_VAR(opline->result.var)) = NULL`.
        Ok(None) => {
            set_out(req, frame, code, op, Value::null());
            Ok(advance(req, frame))
        }
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// The opline a handler that is two oplines stands at, and the
/// `OP_DATA` after it that carries its value (`OP_DATA` in php's
/// `zend_vm_def.h`, read with `opline + 1`).
fn data_after(
    req: &Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Result<(u32, Opline), Fatal> {
    let at = req.vm().opline(frame);
    exec::opline_at(code, at.saturating_add(1))
        .filter(|next| next.opcode == Opcode::OpData)
        .map(|data| (at, *data))
        .ok_or_else(|| {
            Fatal::error(format!(
                "InternalError: {} with no OP_DATA after it",
                op.opcode.name()
            ))
        })
}

/// The `Threw` edge: the throwable left pending where the loop reads it.
fn threw(req: &mut Request, frame: FrameRef, thrown: &Thrown) -> Edge {
    req.leave_pending(thrown);
    Ok(Stepped::Threw(frame).into())
}

/// The body the `DO_*CALL` handlers share, `opcode` naming the one that
/// found no call frame under it: the `ZEND_USER_FUNCTION` arm enters
/// the callee's op array ([`enter`]), the `ZEND_INTERNAL_FUNCTION` arm
/// serves the native on the call frame.
fn do_call(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline, opcode: &str) -> Edge {
    if matches!(op.opcode, Opcode::DoFcall | Opcode::DoFcallByName)
        && let Err(thrown) = flagged_call(req, op)
    {
        req.pop_call();
        return threw(req, frame, &thrown);
    }
    let Some((call, function)) = req.activate_call(op.out_type != OperandKind::Unused) else {
        return Err(Fatal::error(format!(
            "InternalError: {opcode} with no call frame under it"
        )));
    };
    // A user function is never a trampoline, so the call it names is
    // entered before the trampolines are asked about it.
    if let Some(user) = req.user_function(function) {
        let shape = user.shape;
        return enter(req, frame, code, op, call, shape);
    }
    let function = match crate::trampoline::hand_to_magic(req, function) {
        Ok(function) => function,
        Err(thrown) => {
            req.pop_call();
            return threw(req, frame, &thrown);
        }
    };
    if let Some(user) = req.user_function(function) {
        let shape = user.shape;
        return enter(req, frame, code, op, call, shape);
    }
    let served = req.serve_native(function, OnThrow::Nothing);
    // `ZEND_VM_FCALL_INTERRUPT_CHECK(call)`: the check php makes after a
    // native returns, with the call still on the stack, which is where
    // a signal the native delivered to its own process is dispatched
    // under `pcntl_async_signals(true)`. A native that threw leaves the
    // interrupt raised for the next site, since php calls no PHP handler
    // while `EG(exception)` stands.
    let interrupted = match &served {
        Ok(_) => crate::interrupt::check(req),
        Err(_) => Ok(false),
    };
    req.pop_call();
    match served {
        Ok(value) => {
            let threw = match interrupted {
                Ok(threw) => threw,
                Err(fatal) => {
                    req.release(value);
                    return Err(fatal);
                }
            };
            let into = (op.out_type != OperandKind::Unused)
                .then(|| (frame, exec::temporary_slot(code, op.out)));
            if handed_back(req, into, value) || threw {
                return Ok(Stepped::Threw(frame).into());
            }
            Ok(advance(req, frame))
        }
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// `DO_FCALL` and `DO_FCALL_BY_NAME` over a callee whose `fn_flags`
/// carry `ZEND_ACC_DEPRECATED`, or `ZEND_ACC_NODISCARD` where the
/// opline's `result` operand is `UNUSED` (`Z/Zend/zend_vm_def.h:4316-4335`): the
/// deprecation and then the warning, raised with the call built and not
/// yet entered, which is why `zend_get_call_op` keeps such a callee off
/// `DO_UCALL` and `DO_ICALL`, the two that never test. What a handler
/// threw is the `Err`, and the call is never made.
fn flagged_call(req: &mut Request, op: &Opline) -> Result<(), Thrown> {
    let Some(function) = req.building_call_function() else {
        return Ok(());
    };
    let flags = req.function_flags(function);
    if flags.deprecated {
        crate::attributes::deprecated_function(req, function)?;
    }
    if flags.no_discard && op.out_type == OperandKind::Unused {
        crate::attributes::no_discard_function(req, function)?;
    }
    Ok(())
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn init_ns_fcall_by_name<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_init_fcall(req, frame, op, at) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
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
        | Cached::ClassAndCallee { .. }
        | Cached::Property { .. }
        | Cached::Constant(_) => {
            let name = exec::constant(code, ConstIdx(op.op1));
            let found = name
                .as_str_bytes()
                .is_some_and(|name| exec::find_function(req, Bare::resolved(name)).is_some());
            let answer = if found { Cached::Miss } else { Cached::Hit };
            req.prime(frame, slot, answer);
            answer
        }
    };
    if answer == Cached::Hit {
        return jump_to(op.op2);
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
    let (_, data) = data_after(req, frame, code, op)?;
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
///
/// php lays that call down only where an observer watches the native,
/// `zend_frameless_observed_call` (`Z/Zend/zend_vm_def.h:9775`); any
/// other is served over no frame, so here its record stands off the
/// VM stack's pages ([`Request::push_frameless`]) and takes no room
/// php's does not. A recursion that leaves its last frame one slot
/// short of a page's end opens the next page at the same depth whether
/// its innermost frame then calls a frameless native or not, and
/// `memory_get_peak_usage()` reads that page where php's reads it:
///
/// ```php
/// function f($n, $a, $call) {
///     return $n ? f($n - 1, $a, $call) : ($call ? in_array(2, $a) . substr('abc', $n, 1) . strlen('a' . $n) : 0);
/// }
/// foreach ([false, true] as $call) {
///     for ($d = 1; ; $d++) {
///         memory_reset_peak_usage();
///         $base = memory_get_usage();
///         f($d, [1, 2], $call);
///         if (memory_get_peak_usage() - $base >= 262144) { echo $d, "\n"; break; }
///     }
/// }
/// ```
/// ```expect
/// 909
/// 909
/// ```
///
/// The operands are given up as the call ends, `FREE_OP*`, so a
/// temporary the native read is destroyed before the statement that
/// reads what the call returned goes on, and a string an argument's
/// coercion made is the call's and gone with it:
///
/// ```php
/// class S { function __toString(): string { return "abc"; } function __destruct() { echo "d\n"; } }
/// var_dump(str_contains(new S, "b"));
/// $m = memory_get_usage(); $r = strtr(new S, "a", "x"); var_dump($r, memory_get_usage() - $m);
/// echo "after\n";
/// ```
/// ```expect
/// d
/// bool(true)
/// d
/// string(3) "xbc"
/// int(32)
/// after
/// ```
fn frameless_call(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    arguments: &[(OperandKind, u32)],
) -> Edge {
    let Some((function, entry)) = req.frameless_entry(op.extended_value) else {
        return Err(Fatal::error(format!(
            "InternalError: {} indexes frameless entry {} and the engine's table has {} entries",
            op.opcode.name(),
            op.extended_value,
            req.frameless_count()
        )));
    };
    let mut header =
        exec::Header::call(function, u32::try_from(arguments.len()).unwrap_or(u32::MAX));
    header.info.set_serving();
    header.info.set_frameless(true);
    let call = req.push_frameless(header)?;
    for (at, &(kind, field)) in arguments.iter().enumerate() {
        let operand = exec::frameless_operand(req, frame, code, kind, field);
        let placed = req
            .vm_mut()
            .stack
            .frame_mut(call)
            .and_then(|record| record.slots_mut().get_mut(at));
        match placed {
            Some(slot) => *slot = operand,
            None => req.release_zval_nogc(operand),
        }
    }
    // The call ends with the result whether the native threw or not, as
    // php's frameless handler and its full call for an observed one do
    // (`Z/Zend/zend_execute.c:1597`).
    let served = req.serve_entry(function, entry, OnThrow::ReturnValue);
    req.pop_frameless();
    let at = req.vm().opline(frame);
    let width = if arguments.len() == 3 { 2 } else { 1 };
    match served {
        Ok(value) => {
            set_out(req, frame, code, op, value);
            jump_to(at.saturating_add(width))
        }
        Err(thrown) => threw(req, frame, &thrown),
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn assign<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_assign(req, frame, code, op, (A::default(), B::default())) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    assigned(req, frame, code, op)
}

/// `ZEND_ASSIGN` past its inline path: the value taken as
/// `zend_assign_to_variable` takes it, into the CV or the place a fetch
/// by name left, what stood there given up last. A call of its own, so
/// that the inline path before it keeps to the registers it needs.
#[inline(never)]
fn assigned(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let value = exec::take_operand(req, frame, code, op.op2_type, op.op2);
    // `GET_OP1_ZVAL_PTR_PTR_UNDEF(BP_VAR_W)`: a CV, or the place a fetch
    // by name left in its `VAR` as an `IS_INDIRECT`.
    let Some(at) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::error(
            "InternalError: ZEND_ASSIGN over a variable the frame does not have",
        ));
    };
    // php copies into `result` what the write landed, which a typed
    // cell may have converted, so the copy is after the write and not
    // before it.
    let (landed, garbage) = match assign_to_place(req, frame, at, value) {
        Ok(pair) => pair,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    write_out(req, frame, code, op, &landed);
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn assign_op<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let kinds = (ByKind(op.op1_type), ByKind(op.op2_type));
    if let Some(edge) = inline_assign_op(req, frame, code, op, kinds) {
        return edge;
    }
    assigned_op(req, frame, code, op)
}

/// The inline path of `ZEND_ASSIGN_OP` over a CV
/// (`ZEND_ASSIGN_OP_SPEC_CV_*`, `Z/Zend/zend_vm_def.h:1257`): a CV
/// holding a number and a value that is a number as it stands, under an
/// operator [`inline::compound`] names, computed by [`inline::binary`]
/// as the binary opcode's own inline pair computes it, written into the
/// CV in place and, under `RETURN_VALUE_USED`, into the temporary.
/// A CV holding a number is no reference, so no type a property carries
/// is read and neither is the frame's `strict_types` regime, which is
/// php's order. `None`, with nothing touched, for a `VAR`, a CV holding
/// anything else, a reference among it, a value that is no number,
/// every pair the rule refuses, and a temporary holding an owner.
#[inline(always)] // as `inline_arith`
#[allow(clippy::inline_always)]
fn inline_assign_op(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    (a, b): (impl Operand, impl Operand),
) -> Option<Edge> {
    // The value first: one that is no number, a numeric string among
    // them, is the refusal met most, and it reads nothing else.
    let slots = req.slots_mut(frame);
    let value = b.read(slots, code, op.op2);
    if !matches!(value.kind(), ZvalType::Long | ZvalType::Double) {
        return None;
    }
    let rule = inline::compound(op.extended_value)?;
    if a.kind() != OperandKind::Cv {
        return None;
    }
    let at = a.slot(code, op.op1)?;
    let held = *slots.get(at)?;
    let z = inline::binary(rule, held, value)?;
    if op.out_type != OperandKind::Unused {
        write_owning_nothing(slots, code, op, z)?;
    }
    *slots.get_mut(at)? = z;
    Some(Ok(advanced()))
}

/// `ASSIGN_OP` past its inline path: both operands read as values, the
/// kernel over them, and what it came to written to the place
/// ([`assign_to_place`]). A call of its own, as [`assigned_dim_op`] is.
#[inline(never)]
fn assigned_op(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let Some(kernel) = binary_op_of(op.extended_value) else {
        return Err(Fatal::error(format!(
            "InternalError: ZEND_ASSIGN_OP over the operator {}, which is no binary operation",
            op.extended_value
        )));
    };
    let value = exec::operand(req, frame, code, op.op2_type, op.op2);
    // A CV, or under a `VAR` the place `FETCH_RW` by name left there,
    // which it has already made `null` where nothing was written.
    let held = if op.op1_type == OperandKind::Cv {
        let held = exec::operand(req, frame, code, op.op1_type, op.op1);
        // What the read warned of is `null`, so only then is the slot
        // asked whether nothing was written there.
        if held.tag() == TypeTag::Null && req.slot(frame, op.op1).tag() == TypeTag::Undef {
            req.replace_slot(frame, op.op1, Value::null());
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
        return Err(Fatal::error(
            "InternalError: ZEND_ASSIGN_OP over a variable the frame does not have",
        ));
    };
    // php's `ZEND_REF_HAS_TYPE_SOURCES` arm,
    // `zend_binary_assign_op_typed_ref`: the operation is over what the
    // cell holds either way, and the value it comes to is held to each
    // type the cell carries before it lands, which
    // [`assign_to_place`] does under the frame's own regime (#496).
    // What landed is what `result` takes, so the copy is after the
    // write and not before it.
    let (landed, garbage) = match assign_to_place(req, frame, at, stepped) {
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
///
/// A call that returned by value is php's notice and a plain assignment,
/// through a reference the target already is:
///
/// ```php
/// function g() { return 5; }
/// function s() { static $v; if (!isset($v)) { $v = &g(); } return $v; }
/// var_dump(s(), s());
/// ```
/// ```expect
/// Notice: Only variables should be assigned by reference in %s on line 3
/// int(5)
/// int(5)
/// ```
pub(crate) fn assign_ref<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(source) = write_place(req, frame, code, op.op2_type, op.op2) else {
        return Err(Fatal::error(
            "InternalError: ZEND_ASSIGN_REF over a source php compiles no write over",
        ));
    };
    let target = match op.op1_type {
        OperandKind::Cv => req.slot_ptr(frame, op.op1),
        OperandKind::Var => {
            let at = req.slot_ptr(frame, exec::temporary_slot(code, op.op1));
            let Some(entry) = at.and_then(|at| at.read().as_indirect()) else {
                let thrown = crate::api::raising::throw_error(
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
        return Err(Fatal::error(
            "InternalError: ZEND_ASSIGN_REF over a target php compiles no write over",
        ));
    };
    let held = exec::value_of(source.read());
    // `ZEND_RETURNS_FUNCTION`: the source is a call, and a call that
    // returned by value has no cell to bind, which is php's notice and
    // then the plain assignment (`zend_wrong_assign_to_variable_reference`
    // over `zend_assign_to_variable`), so a target that is a reference,
    // a `static` among them, keeps it and takes the value through it.
    if op.extended_value & RETURNS_FUNCTION != 0 && held.as_php_ref().is_none() {
        crate::api::raising::notice(req, "Only variables should be assigned by reference");
        if req.is_pending() {
            return Ok(Stepped::Threw(frame).into());
        }
        let value = held.undef_as_null();
        req.addref(&value);
        let (landed, garbage) = match assign_to_place(req, frame, target, value) {
            Ok(pair) => pair,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        write_out(req, frame, code, op, &landed);
        req.release(garbage);
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        return Ok(advance(req, frame));
    }
    let cell = match held.as_php_ref() {
        Some(cell) if target == source => {
            write_out(req, frame, code, op, &cell.as_value());
            exec::free_operand(req, frame, code, op.op2_type, op.op2);
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            return Ok(advance(req, frame));
        }
        // The owner the slot had of `held` is the cell's now.
        _ => match req.make_ref(source) {
            Ok(cell) => cell,
            Err(thrown) => return threw(req, frame, &thrown),
        },
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

/// `zend_assign_to_property_reference` over a name the class serves
/// itself ([`PropertyFound::Served`]): what `read_property` answered is
/// given up and the throw is `Cannot assign by reference to overloaded
/// object`, or the throw the read raised (`Z_ISERROR_P`). The read is
/// `zend_fetch_property_address`'s `BP_VAR_W`, so a value `__get`
/// answered is php's notice that the modification has no effect first.
fn overloaded_reference(req: &mut Request, object: PhpObject, name: &[u8]) -> Thrown {
    if let Some(thrown) = req.hooked_reference_refusal(object, name) {
        return thrown;
    }
    let table = req.object_handlers(object.heap());
    match table.read_property(req, object, name, PropertyRead::Write) {
        Ok(value) => {
            value.give_up(req);
            crate::api::raising::throw_error(
                req,
                "Error",
                "Cannot assign by reference to overloaded object",
            )
        }
        Err(thrown) => thrown,
    }
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
/// cell under `RETURN_VALUE_USED`. A name the class serves itself is
/// read through its `read_property`, as `zend_fetch_property_address`
/// reads it, and is php's `Error`: nothing binds to it.
///
/// ```php
/// $d = new DOMDocument;
/// $x = 'a';
/// try { $d->documentURI = &$x; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($x);
/// ```
/// ```expect
/// Cannot assign by reference to overloaded object
/// string(1) "a"
/// ```
///
/// A name the class's `__get` answers is read under `BP_VAR_W`, php's
/// notice ahead of the `Error`:
///
/// ```php
/// class A { public $q = 3; function __get($n) { return $this->q; } }
/// $a = new A; $b = 1;
/// try { $a->w = &$b; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Notice: Indirect modification of overloaded property A::$w has no effect in %s on line 4
/// Cannot assign by reference to overloaded object
/// ```
///
/// ```php
/// class C { public $p; }
/// $o = new C;
/// $o->p = &$x;
/// $x = 1;
/// var_dump($o->p);
/// ```
/// ```expect
/// int(1)
/// ```
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
    let (at, data) = data_after(req, frame, code, op)?;
    let (data_type, data_at) = (data.op1_type, data.op1);
    let mut member = exec::Member::default();
    let found = property_place(req, frame, code, op, PropertyRead::Silent, &mut member);
    member.release(req, frame, code, op);
    let (place, object, name) = match found? {
        PropertyFound::At(place, object, name) => (place, object, name),
        PropertyFound::Served(object, name) => {
            let thrown = overloaded_reference(req, object, &name);
            let thrown = freeing_container(req, frame, code, op, thrown);
            exec::free_operand(req, frame, code, data_type, data_at);
            return threw(req, frame, &thrown);
        }
        PropertyFound::Threw(thrown) => {
            exec::free_operand(req, frame, code, data_type, data_at);
            return threw(req, frame, &thrown);
        }
    };
    let Some(source) = write_place(req, frame, code, data_type, data_at) else {
        return Err(Fatal::error(
            "InternalError: ZEND_ASSIGN_OBJ_REF over a source php compiles no write over",
        ));
    };
    let held = exec::value_of(source.read());
    let strict = req.frame_strict_types(frame);
    if op.extended_value & RETURNS_FUNCTION != 0 && held.as_php_ref().is_none() {
        crate::api::raising::notice(req, "Only variables should be assigned by reference");
        if req.is_pending() {
            exec::free_operand(req, frame, code, data_type, data_at);
            return Ok(Stepped::Threw(frame).into());
        }
        let value = held.undef_as_null();
        req.addref(&value);
        let (landed, garbage) = match assign_to_place(req, frame, place, value) {
            Ok(pair) => pair,
            Err(thrown) => return threw(req, frame, &thrown),
        };
        write_out(req, frame, code, op, &landed);
        req.release(garbage);
        exec::free_operand(req, frame, code, data_type, data_at);
        return Ok(advance_over_data(req, frame, at));
    }
    // The owner the source had of `held` is the cell's now.
    let cell = match req.make_ref(source) {
        Ok(cell) => cell,
        Err(thrown) => return threw(req, frame, &thrown),
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
    Ok(advance_over_data(req, frame, at))
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
///
/// A property the class serves is its `write_property`'s, declared or
/// not, as `ZEND_ASSIGN_OBJ` calls `zobj->handlers->write_property`
/// wherever its cache slot has placed nothing: `DOMNode::$nodeValue`
/// and `DOMDocument::$preserveWhiteSpace` are declared, and the write
/// reaches the document.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><e/></r>');
/// $e = $d->documentElement->firstChild;
/// $e->nodeValue = 'a<b';
/// var_dump($d->preserveWhiteSpace = false);
/// echo $d->saveXML($e), "\n";
/// ```
/// ```expect
/// bool(false)
/// <e>a&lt;b</e>
/// ```
///
/// A CV container is read after the value, as php reads `object` after
/// `GET_OP_DATA_ZVAL_PTR`, so a handler of an undefined value's warning
/// that replaced the variable is what the write meets.
///
/// ```php
/// class C { public $p; }
/// set_error_handler(function ($code, $msg) use (&$o) { echo "$msg\n"; $o = 0; });
/// $o = new C;
/// try { $o->p = $y; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($o);
/// ```
/// ```expect
/// Undefined variable $y
/// Attempt to assign property "p" on int
/// int(0)
/// ```
///
/// A VAR container's object is held across an undefined value's read
/// and given back after the write, so the write lands in a live object
/// whatever a handler of the warning gave up, and its destructor is
/// served at the release. The fence pins the engine's side of the row
/// `trace:assign-obj-entry-container-under-a-handler` and is held to the
/// engine alone.
///
/// ```php
/// class C { public $p = 1; function __destruct() { echo "dtor ", $this->p ?? "NULL", "\n"; } }
/// set_error_handler(function ($code, $msg) use (&$a) { echo "Error: $msg\n"; $a = 0; });
/// $a = [new C];
/// $a[0]->p = $y;
/// var_dump($a);
/// echo "DONE\n";
/// ```
/// ```expect
/// Error: Undefined variable $y
/// dtor NULL
/// int(0)
/// DONE
/// ```
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn assign_obj<
    T: Tier,
    A: Operand + Default,
    B: Operand + Default,
    D: Operand + Default,
>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_assign_obj(
        req,
        frame,
        code,
        op,
        at,
        (A::default(), B::default(), D::default()),
    ) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    assigned_obj(req, frame, code, op)
}

/// The inline path of `ZEND_ASSIGN_OBJ` over a constant name
/// (`ZEND_ASSIGN_OBJ_SPEC_CV_CONST_OP_DATA_*`, `_UNUSED_CONST_*`,
/// `Z/Zend/zend_vm_def.h:2489-2537`): the place the cache slot names on
/// a receiver of the class it was primed with ([`cached_place`]), a
/// declared slot or a dynamic property's bucket, holding a value that
/// owns nothing, takes the `OP_DATA` value as
/// `zend_assign_to_variable` takes it, a constant copied, a temporary
/// moved and a CV copied with one more owner, and the temporary takes it
/// too under `RETURN_VALUE_USED`; nothing is given up, so nothing runs.
/// `None`, with nothing touched, as [`inline_fetch_obj_r`]'s, and for a
/// typed slot, which `zend_assign_to_typed_prop` holds to its type, its
/// readonly and its set visibility among it, a slot holding an owner or
/// a reference, a value that is a reference or nothing written, and a
/// temporary holding an owner.
///
/// ```php
/// class C { public $p = 1; public $q; private $r = 0;
///     function set($v) { $this->r = $v; return $this->r; } }
/// $c = new C; $s = str_repeat("s", 2);
/// for ($i = 0; $i < 3; $i++) { $x = $c->p = $i; $c->q = $s; echo $c->set($i * 2), $x, "|"; }
/// $c->p = new ArrayObject([1]); $c->p = 2; $c->q = null;
/// var_dump($c, $s);
/// ```
/// ```expect
/// 00|21|42|object(C)#1 (3) {
///   ["p"]=>
///   int(2)
///   ["q"]=>
///   NULL
///   ["r":"C":private]=>
///   int(4)
/// }
/// string(2) "ss"
/// ```
#[inline(always)] // as `inline_fetch_obj_r`
#[allow(clippy::inline_always)]
fn inline_assign_obj(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
    (a, b, data): (impl Operand, impl Operand, impl Operand),
) -> Option<Edge> {
    let object = inline_receiver(req, frame, code, op, a, b)?;
    let (place, typed) = cached_place(
        req,
        (frame, code, op),
        property_cache_slot(op),
        object,
        true,
    )?;
    let held = place.read();
    if typed || held.is_undef() || held.is_refcounted() {
        return None;
    }
    let z = data_value(req, frame, code, op, at, data)?;
    place.write(z);
    Some(Ok(Next::jump(at.saturating_add(2))))
}

/// The `OP_DATA` value the inline path of `ASSIGN_OBJ` takes, by
/// [`taken_value`]'s rule: the `OP_DATA` opline after `at`,
/// whose `op1` is of the kind `data` is, as the assignment's handler was
/// resolved over it where its unit was published (`php_unit::Handler`),
/// php's `_OP_DATA_CV` and the rest, so neither its opcode nor its kind
/// is tested here.
#[inline(always)] // as `inline_fetch_dim_r`
#[allow(clippy::inline_always)]
fn data_value(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
    data: impl Operand,
) -> Option<Zval> {
    let field = exec::opline_at(code, at.saturating_add(1))?.op1;
    taken_value(req.slots_mut(frame), code, op, data, field)
}

/// What [`taken_value`] takes, found with nothing touched: the zval,
/// the temporary it is moved out of, and the temporary that takes it
/// too under `RETURN_VALUE_USED`. Asking first is what an inline path
/// that lays an element down does before it lays it, so an element it
/// laid down is never one the kernel lays down a second time.
#[derive(Clone, Copy)]
struct Taking {
    z: Zval,
    moved: Option<usize>,
    out: Option<usize>,
}

/// The [`Taking`] of the value `field` names, by [`taken_value`]'s rule.
#[inline(always)] // as `inline_fetch_dim_r`
#[allow(clippy::inline_always)]
fn taking(
    slots: &[Zval],
    code: &OpArray,
    op: &Opline,
    value: impl Operand,
    field: u32,
) -> Option<Taking> {
    let z = value.read(slots, code, field);
    if !crate::operand::movable(z)
        || (value.kind() == OperandKind::Const && !crate::operand::owns_nothing(z))
    {
        return None;
    }
    let out = if op.out_type == OperandKind::Unused {
        None
    } else {
        let out = exec::temporary_slot(code, op.out) as usize;
        if slots.get(out)?.is_refcounted() {
            return None;
        }
        Some(out)
    };
    let moved = match value.kind() {
        OperandKind::Tmp | OperandKind::Var => Some(value.slot(code, field)?),
        _ => None,
    };
    Some(Taking { z, moved, out })
}

impl Taking {
    /// The value taken: moved out of its temporary or one more owner of
    /// it taken, and one more for the temporary that takes it too.
    #[inline(always)] // as `inline_fetch_dim_r`
    #[allow(clippy::inline_always)]
    fn take(self, slots: &mut [Zval]) -> Option<Zval> {
        let Taking { z, moved, out } = self;
        match moved {
            Some(at) => *slots.get_mut(at)? = Zval::UNDEF,
            None => z.addref(),
        }
        if let Some(out) = out {
            z.addref();
            *slots.get_mut(out)? = z;
        }
        Some(z)
    }
}

/// The value an assignment's inline path takes, one rule for `ASSIGN`'s
/// `op2` and the `OP_DATA` of `ASSIGN_OBJ` and `ASSIGN_DIM`, as
/// `zend_assign_to_variable` takes it: a constant that counts no owner
/// copied, a temporary moved out of its slot and a CV copied with one
/// more owner, and, under `RETURN_VALUE_USED`, one more owner for the
/// temporary, which takes it too. None, with nothing touched, for a
/// value nothing wrote, whose warning is the kernel's, a reference,
/// whose value is what is assigned, a refcounted constant, and a
/// temporary holding an owner.
///
/// ```php
/// $a = str_repeat('a', 3); $o = new stdClass; $d = [];
/// $x = $a; $o->p = $a; $d[0] = $a; $y = $a . 'b'; $o->q = $a . 'c'; $d[1] = $a . 'd';
/// debug_zval_dump($a, $y, $o->q, $d[1]);
/// ```
/// ```expect
/// string(3) "aaa" refcount(5)
/// string(4) "aaab" refcount(2)
/// string(4) "aaac" refcount(2)
/// string(4) "aaad" refcount(2)
/// ```
#[inline(always)] // as `inline_fetch_dim_r`
#[allow(clippy::inline_always)]
fn taken_value(
    slots: &mut [Zval],
    code: &OpArray,
    op: &Opline,
    value: impl Operand,
    field: u32,
) -> Option<Zval> {
    taking(slots, code, op, value, field)?.take(slots)
}

/// `ASSIGN_OBJ` past its inline path: the value, the receiver and the
/// name read as php reads them, and the write [`assign_property`] makes.
/// A call of its own, as [`fetched_obj`] is.
#[inline(never)]
fn assigned_obj(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let (at, data) = data_after(req, frame, code, op)?;
    // A CV is read through its slot after the value, as php reads
    // `object` after `GET_OP_DATA_ZVAL_PTR`: an undefined value's
    // handler may have replaced it.
    let fetch = |req: &mut Request| {
        exec::container(req, frame, code, op.op1_type, op.op1, "ZEND_ASSIGN_OBJ")
    };
    let early = (op.op1_type != OperandKind::Cv)
        .then(|| fetch(req))
        .transpose()?;
    // An undefined value's handler can free the table a VAR's INDIRECT
    // slot stands in, and the object with it: the object is held across
    // the read and given back after the write, as php guards a container
    // its handlers can free (`trace:assign-obj-entry-container-under-a-handler`).
    let guard = early
        .filter(|_| {
            op.op1_type == OperandKind::Var
                && data.op1_type == OperandKind::Cv
                && req.slot(frame, data.op1).tag() == TypeTag::Undef
        })
        .map(|container| Owner::take(req, container.deref()));
    let value = exec::take_operand(req, frame, code, data.op1_type, data.op1);
    let container = match &guard {
        Some(held) => **held,
        None => early.map_or_else(|| fetch(req), Ok)?.deref(),
    };
    let mut member = exec::Member::default();
    let written = match (
        member.name(req, frame, code, op),
        container.as_object().copied(),
    ) {
        (Ok(name), Some(object)) => {
            prime_constant(req, frame, op, property_cache_slot(op), object, &name);
            match assign_property(req, frame, code, op, object, &name, value) {
                Ok(written) => written,
                Err(fatal) => {
                    if let Some(held) = guard {
                        held.abandon(&fatal);
                    }
                    return Err(fatal);
                }
            }
        }
        (Ok(name), None) => {
            let name = name.as_str_bytes().unwrap_or_default();
            let thrown = crate::api::raising::non_object_error(req, "assign", name, &container);
            write_out(req, frame, code, op, &Value::null());
            req.release(value);
            Err(thrown)
        }
        (Err(thrown), _) => {
            req.release(value);
            Err(thrown)
        }
    };
    member.release(req, frame, code, op);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    if let Some(held) = guard {
        held.give_up(req);
    }
    if let Err(thrown) = written {
        return threw(req, frame, &thrown);
    }
    Ok(advance_over_data(req, frame, at))
}

/// `ASSIGN_OBJ`'s body over an object: the name placed
/// ([`Request::declared_slot_for_write`]), the temporary `out` names
/// written under `RETURN_VALUE_USED`, and the value into the declared
/// slot in place or through the object's handler table's
/// `write_property` for a name the class did not declare or serves
/// itself ([`PropertyPlace::Handlers`]). `value` is given up on the
/// `Error` path, as `FREE_OP_DATA` gives it up.
///
/// A table whose own `write_property` guards a declared name takes the
/// assignment whatever the slot holds
/// ([`ObjectHandlers::write_overloaded`](crate::api::abi::ObjectHandlers::write_overloaded)):
///
/// ```php
/// $db = new PDO('sqlite::memory:');
/// $s = $db->query('SELECT 1');
/// try { $s->queryString = 'x'; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($s->queryString);
/// ```
/// ```expect
/// Property queryString is read only
/// string(8) "SELECT 1"
/// ```
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
    // A lazy object takes the write through `write_property`, which is
    // where php's slot nothing has written and name nothing holds reach
    // `forward_write_to_lazy_object`.
    // A table whose own `write_property` answers for a declared name
    // takes every assignment, as php's `ASSIGN_OBJ` reaches a slot in
    // place only through `zend_std_write_property`'s cache.
    let lazy = object.heap().laziness() != php_heap::Laziness::Eager;
    let place = if lazy || object.handlers(req).write_overloaded() {
        PropertyPlace::Handlers
    } else {
        match place_of(req, object, bytes) {
            Ok(place) => place,
            Err(thrown) => {
                req.release(value);
                return Ok(Err(thrown));
            }
        }
    };
    let placed = match place {
        // `zobj->handlers->write_property`: the class serves the name,
        // declared or not, so the write is its handler's, as a name no
        // class declared is; php's `ASSIGN_OBJ` asks no place, so one the
        // class keeps apart is its `write_property`'s too.
        PropertyPlace::Handlers | PropertyPlace::Apart => None,
        // `zend_get_property_offset(ce, name, ce->__set != NULL, ...)`:
        // a class with a `__set` places the name quietly, so a property
        // the scope may not reach reaches `write_property` and the
        // accessor there, which is where php raises the `Error` if the
        // guard stands.
        PropertyPlace::Std => {
            let silent = req.serves_accessor(object.class(), b"__set");
            match req.declared_slot_for_write(object, bytes, "modify", silent) {
                Ok(placed) => placed,
                Err(thrown) => {
                    req.release(value);
                    return Ok(Err(thrown));
                }
            }
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
                return Err(Fatal::error(format!(
                    "InternalError: ZEND_ASSIGN_OBJ placed the typed property ${}, whose declared type the class does not carry",
                    String::from_utf8_lossy(bytes)
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
            let reinitable = object.heap().slot_bit(slot, php_heap::PropBit::Reinitable);
            if typed.readonly && !reinitable && !object.heap().slot(slot).is_undef() {
                req.release(value);
                return Ok(Err(req.readonly_modification(
                    typed.declaring,
                    bytes,
                    "modify",
                )));
            }
            let strict = req.frame_strict_types(frame);
            let value = match req.hold_to_property_type_of(object, &typed, bytes, value, strict) {
                Ok(value) => value,
                Err(thrown) => return Ok(Err(thrown)),
            };
            match req.assign_declared_slot(object.heap(), slot, value, strict) {
                Ok(pair) => landed_out(req, frame, code, op, pair),
                Err(thrown) => return Ok(Err(thrown)),
            }
            // `Z_PROP_FLAG_P(property_val) &= ~IS_PROP_REINITABLE`: a
            // readonly property `__clone` wrote is locked again.
            object
                .heap()
                .set_slot_bit(slot, php_heap::PropBit::Reinitable, false);
            Ok(Ok(()))
        }
        Some((slot, false)) => {
            let strict = req.frame_strict_types(frame);
            Ok(req
                .assign_declared_slot(object.heap(), slot, value, strict)
                .map(|pair| landed_out(req, frame, code, op, pair)))
        }
        None => {
            let Some(key) = name.as_php_string().copied() else {
                return Err(Fatal::error(
                    "InternalError: ZEND_ASSIGN_OBJ over a property whose name is no string",
                ));
            };
            write_out(req, frame, code, op, &value);
            let key = Owner::take(req, key);
            let value = Owner::adopt(value);
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
    let (at, data) = data_after(req, frame, code, op)?;
    let (data_type, data_at) = (data.op1_type, data.op1);
    let scope = req.executed_scope();
    let (addressed, name) = match static_member(req, frame, code, op) {
        Ok((class, name)) => (
            req.static_slot(class, &name, scope, PropertyRead::Warning),
            name,
        ),
        Err(thrown) => (Err(thrown), Vec::new()),
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
    let Some(info) = placed else {
        exec::free_operand(req, frame, code, data_type, data_at);
        return Ok(advance_over_data(req, frame, at));
    };
    let value = exec::take_operand(req, frame, code, data_type, data_at);
    // `zend_assign_to_typed_prop`'s set-visibility arm, after the value
    // is read.
    if let Some(thrown) = req.set_refused(info, "modify") {
        req.release(value);
        return threw(req, frame, &thrown);
    }
    // `zend_assign_to_typed_prop` under this frame's
    // `EX_USES_STRICT_TYPES()`, which the write itself serves
    // ([`Request::set_static_property`]): a typed slot takes the value
    // the declared type accepts, coerced by the weak rule where the file
    // is not strict, and php's `TypeError` where it neither satisfies
    // nor coerces. `RETURN_VALUE_USED` copies what landed, after it.
    let strict = req.frame_strict_types(frame);
    match req.set_static_slot(info, &name, value, strict) {
        Ok(pair) => landed_out(req, frame, code, op, pair),
        Err(thrown) => return threw(req, frame, &thrown),
    }
    Ok(advance_over_data(req, frame, at))
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
    let (class, name) = static_member(req, frame, code, op)?;
    let scope = req.executed_scope();
    match req.static_slot(class, &name, scope, PropertyRead::Warning)? {
        Some(info) => {
            if let Some(thrown) = req.set_refused(info, "indirectly modify") {
                return Err(thrown);
            }
            Ok(Some(Placed {
                class,
                info,
                name,
                scope,
                ty: req.property_decl(info).and_then(|p| p.ty.clone()),
            }))
        }
        None => Ok(None),
    }
}

/// What [`static_place`] addressed: the class the opline named, the
/// property's row, whose class declared it and names its slot, the
/// property's name, the scope the access was measured against, and the
/// declared type the write is held to, which is php's `prop_info` beside
/// the slot.
struct Placed {
    class: crate::api::values::ClassId,
    info: crate::api::request::PropertyInfo,
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
///
/// A slot bound to a cell a typed property is a source of is
/// `zend_binary_assign_op_typed_ref`, held to the cell's types whatever
/// the slot's own declaration:
///
/// ```php
/// class T { public static int $i = 123; public static $p; }
/// T::$p = &T::$i;
/// try { T::$p .= "x"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// T::$p .= "4";
/// var_dump(T::$i);
/// ```
/// ```expect
/// Cannot assign string to reference held by property T::$i of type int
/// int(1234)
/// ```
pub(crate) fn assign_static_prop_op<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(kernel) = binary_op_of(op.extended_value) else {
        return Err(Fatal::error(format!(
            "InternalError: ZEND_ASSIGN_STATIC_PROP_OP over the operator {} , which is no binary operation",
            op.extended_value
        )));
    };
    let (at, data) = data_after(req, frame, code, op)?;
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
        info,
        name,
        scope,
        ty,
    }) = placed
    else {
        exec::free_operand(req, frame, code, data_type, data_at);
        return Ok(advance_over_data(req, frame, at));
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
                    .static_slot_cell(info)
                    .is_none_or(|cell| req.ref_type_sources(cell.heap()).is_empty()));
    let computed = if in_place {
        crate::ops::concat_assign(req, &held, &value, |req| {
            req.release(held);
            req.store_static_property(info, Value::null());
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
    // `ZEND_REF_HAS_TYPE_SOURCES` ahead of the property's own type:
    // `zend_binary_assign_op_typed_ref` over the cell the slot holds.
    if let Some(cell) = req
        .static_slot_cell(info)
        .filter(|cell| !req.ref_type_sources(cell.heap()).is_empty())
    {
        if let Err(thrown) = req.assign_to_reference(cell.heap(), stepped, strict) {
            return threw(req, frame, &thrown);
        }
        write_out(req, frame, code, op, &cell.get());
        return Ok(advance_over_data(req, frame, at));
    }
    let stepped = match ty {
        None => stepped,
        Some(ty) => {
            let verdict = verified_op(req, &stepped, &ty, info, &name, strict);
            // php gives the operation's value up and the slot keeps what
            // it held (`zval_ptr_dtor(&z_copy)`).
            match crate::ops::landed(req, stepped, verdict) {
                Ok(landed) => landed,
                Err(thrown) => return threw(req, frame, &thrown),
            }
        }
    };
    write_out(req, frame, code, op, &stepped);
    // The value is already the declared type's, so the store is php's
    // `ZVAL_COPY_VALUE(zptr, &z_copy)` and verifies nothing a second
    // time: a weak conversion raises a deprecation and php raises it
    // once.
    req.store_static_property(info, stepped);
    Ok(advance_over_data(req, frame, at))
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
    info: crate::api::request::PropertyInfo,
    name: &[u8],
    strict: bool,
) -> Result<Option<Value>, Thrown> {
    match req.static_slot_cell(info) {
        Some(cell) if !req.ref_type_sources(cell.heap()).is_empty() => {
            crate::ops::verify_ref_assignable(req, cell.heap(), stepped, strict)
        }
        _ => crate::ops::verify_property_type(req, stepped, ty, info.declaring, name, strict),
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
    let placed = match static_place(req, frame, code, op) {
        Ok(placed) => placed,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let Some(Placed {
        class,
        info,
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
            let (thrown, limit) = crate::ops::incdec_prop_error(
                req,
                &ty,
                info.declaring,
                &name,
                (increment, crate::ops::Holder::Property),
            );
            req.release(held);
            req.release(stepped);
            // php writes the limit into the slot and raises beside it,
            // so the slot stands at its maximal value after the catch.
            req.store_static_property(info, limit);
            return threw(req, frame, &thrown);
        }
        Some(ty) => {
            let verdict =
                crate::ops::verify_property_type(req, &stepped, &ty, info.declaring, &name, strict);
            match crate::ops::landed(req, stepped, verdict) {
                Ok(landed) => landed,
                Err(thrown) => {
                    // `ZVAL_COPY_VALUE(var_ptr, copy)`: the slot keeps
                    // what it held and the stepped value is given up.
                    req.release(held);
                    return threw(req, frame, &thrown);
                }
            }
        }
    };
    write_out(req, frame, code, op, if post { &held } else { &stepped });
    req.release(held);
    req.store_static_property(info, stepped);
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
    let (at, data) = data_after(req, frame, code, op)?;
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
        info,
        name,
        scope,
        ty,
    }) = placed
    else {
        exec::free_operand(req, frame, code, data_type, data_at);
        return Ok(advance_over_data(req, frame, at));
    };
    let Some(source) = write_place(req, frame, code, data_type, data_at) else {
        return Err(Fatal::error(
            "InternalError: ZEND_ASSIGN_STATIC_PROP_REF over a source php compiles no write over",
        ));
    };
    let held = exec::value_of(source.read());
    let returns_function = op.extended_value & RETURNS_FUNCTION != 0 && held.as_php_ref().is_none();
    if returns_function {
        crate::api::raising::notice(req, "Only variables should be assigned by reference");
        if req.is_pending() {
            return Ok(Stepped::Threw(frame).into());
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
            info.declaring,
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
    if let Some(old) = req.static_slot_cell(info) {
        req.ref_del_type_source(old.heap(), info.declaring, &name);
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
                declaring: info.declaring,
                name: name.clone(),
                ty,
            },
        );
    }
    Ok(advance_over_data(req, frame, at))
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
/// place held, which the caller releases last there too
/// (`zend_assign_to_typed_ref_ex`'s `garbage_ptr`), so a destructor it
/// serves that writes the cell does so after `result` took the value.
///
/// ```php
/// class Test {
///     static ?Test $test;
///     function __destruct() { $GLOBALS['a'] = null; }
/// }
/// $a = new Test;
/// Test::$test = &$a;
/// var_dump($a = new Test);
/// var_dump($a);
/// ```
/// ```expect
/// object(Test)#2 (0) {
/// }
/// NULL
/// ```
fn assign_to_place(
    req: &mut Request,
    frame: FrameRef,
    at: php_heap::ZvalRef,
    value: Value,
) -> Result<(Value, Value), Thrown> {
    let mut at = at;
    let mut held = exec::value_of(at.read());
    let mut value = value;
    if let Some(cell) = held.as_php_ref() {
        let cell = *cell;
        if !req.ref_type_sources(cell.heap()).is_empty() {
            // The frame's regime, read only where a type is held to it.
            let strict = req.frame_strict_types(frame);
            let verdict = crate::ops::verify_ref_assignable(req, cell.heap(), &value, strict);
            value = crate::ops::landed(req, value, verdict)?;
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
    let table = req.object_handlers(object.heap());
    let given = Owner::take(req, value);
    let served = table.write_dimension(req, object, dim.as_ref(), given);
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
/// dimension is [`assign_string_offset`], whose byte, or `null` where
/// the write landed nowhere, is what the temporary `out` names under
/// `RETURN_VALUE_USED`.
fn assign_text_dim(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    data: Opline,
    place: php_heap::ZvalRef,
    keeper: &Keeper,
) -> Result<(), Thrown> {
    if op.op2_type == OperandKind::Unused {
        return Err(crate::api::raising::throw_error(
            req,
            "Error",
            "[] operator not supported for strings",
        ));
    }
    let written = assign_string_offset(req, frame, code, op, data, place, keeper);
    // `FREE_OP_DATA`, where the write landed before the value was read.
    exec::free_operand(req, frame, code, data.op1_type, data.op1);
    let byte = match written? {
        TextWrite::Byte(byte) => Value::string(req.string(&[byte])?),
        TextWrite::Gone => Value::null(),
    };
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
///
/// An append of an undefined CV warns under one more owner of the
/// table ([`append_value`]), and where the warning's handler freed it
/// nothing is written and the temporary takes `null`.
///
/// ```php
/// set_error_handler(function ($code, $msg) use (&$a) { echo "$msg\n"; $a = 0; });
/// $a[] = $y;
/// var_dump($a);
/// $b = [[1]];
/// set_error_handler(function ($code, $msg) use (&$b) { echo "$msg\n"; $b = 0; });
/// var_dump($b[0][] = $y);
/// ```
/// ```expect
/// Undefined variable $y
/// int(0)
/// Undefined variable $y
/// NULL
/// ```
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn assign_dim<
    T: Tier,
    A: Operand + Default,
    B: Operand + Default,
    D: Operand + Default,
>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_assign_dim(
        req,
        frame,
        code,
        op,
        at,
        (A::default(), B::default(), D::default()),
    ) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    assigned_dim(req, frame, code, op)
}

/// The inline path of `ZEND_ASSIGN_DIM` over a CV
/// (`ZEND_ASSIGN_DIM_SPEC_CV_*`, `Z/Zend/zend_vm_def.h:2621-2660`): an
/// array the CV alone owns, or the one a reference it holds names
/// ([`owned_array`]), whose element at the key holds a value and no
/// reference, takes the `OP_DATA` value ([`taken_value`]), as
/// `zend_assign_to_variable` writes it, and the temporary takes it
/// under `RETURN_VALUE_USED`. What the element held is given up after
/// both, `GC_DTOR_NO_REF(garbage)`: a string serves no destructor and is
/// offered to no collector, so it is given up inline, and any other
/// owner, an array or an object, is released with the opline standing,
/// where a destructor it serves runs and a throw it leaves is the edge's.
/// `None`, with nothing touched, as [`inline_fetch_dim_w`]'s, for an
/// element that is a reference, and a value or a temporary
/// [`taken_value`] refuses, which is asked before an element is laid
/// down, so the kernel never lays down a second one for a key the
/// table did not have or for an append.
///
/// ```php
/// $a = [1]; $a[] = 7; $r = 2; $x = &$r; $a[] = $x; $a[9] = $x; $a["k"] = $u;
/// function f() { static $n = 0, $seen = []; $n++; $seen[] = $n; return count($seen); }
/// echo json_encode($a), f(), f(), f(), "\n";
/// ```
/// ```expect
///
/// Warning: Undefined variable $u in %s on line 2
/// {"0":1,"1":7,"2":2,"9":2,"k":null}123
/// ```
///
/// ```php
/// $a = [1, 2, 3]; $h = ["x" => 1, "y" => [1]]; $b = $a; $s = str_repeat("s", 2);
/// for ($i = 0; $i < 3; $i++) { $a[$i] = $i * 2; $r = $h["x"] = $s; }
/// $a[1] = 9; $h["y"] = 0; $a[5] = 1; $h["x"] = 2;
/// var_dump($a, $b[1], $h, $r);
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(9)
///   [2]=>
///   int(4)
///   [5]=>
///   int(1)
/// }
/// int(2)
/// array(2) {
///   ["x"]=>
///   int(2)
///   ["y"]=>
///   int(0)
/// }
/// string(2) "ss"
/// ```
///
/// What the element held is given up after the write, so a destructor
/// it serves reads the array with the value in it, before the next
/// statement runs, and a throw from it is raised at the assignment:
///
/// ```php
/// class D { function __construct(public $n) {} function __destruct() { global $a; echo "dtor {$this->n} sees ", $a[0] instanceof D ? "D" : $a[0], "\n"; } }
/// class T { function __destruct() { throw new Exception("from dtor"); } }
/// $a = [new D(1), str_repeat("s", 3), [1, [2]]]; $s = str_repeat("t", 2);
/// $a[0] = 5; echo "next\n";
/// $a[1] = $s; $a[1] = 7; debug_zval_dump($s);
/// $c = [new T];
/// try { $c[0] = 1; echo "never\n"; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
/// var_dump($c, $a);
/// ```
/// ```expect
/// dtor 1 sees 5
/// next
/// string(2) "tt" refcount(2)
/// caught from dtor
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// array(3) {
///   [0]=>
///   int(5)
///   [1]=>
///   int(7)
///   [2]=>
///   array(2) {
///     [0]=>
///     int(1)
///     [1]=>
///     array(1) {
///       [0]=>
///       int(2)
///     }
///   }
/// }
/// ```
///
/// An array given up is released as `zval_ptr_dtor` releases it, a
/// possible root of the collector where an owner of it stands:
///
/// ```php
/// $x = [new stdClass]; $a = [$x, [new stdClass], 2];
/// $before = gc_status()["roots"];
/// $a[0] = 0;
/// $kept = gc_status()["roots"] - $before;
/// $a[1] = 0;
/// var_dump($kept, gc_status()["roots"] - $before, $a);
/// function f(array &$v) { $v[1] = [3]; $v[1] = 4; }
/// $b = [1, 2]; f($b); var_dump($b);
/// ```
/// ```expect
/// int(1)
/// int(1)
/// array(3) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(0)
///   [2]=>
///   int(2)
/// }
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(4)
/// }
/// ```
#[inline(always)] // as `inline_fetch_dim_r`
#[allow(clippy::inline_always)]
fn inline_assign_dim(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
    (a, b, data): (impl Operand, impl Operand, impl Operand),
) -> Option<Edge> {
    let field = exec::opline_at(code, at.saturating_add(1))?.op1;
    let taking = taking(req.slots_mut(frame), code, op, data, field)?;
    let placed = match owned_entry(req, frame, (code, op, at), DimRead::Silent, (a, b))? {
        Ok(placed) => placed,
        Err(edge) => return Some(edge),
    };
    let garbage = placed.entry.read();
    let z = taking.take(req.slots_mut(frame))?;
    placed.entry.write(z);
    free_dim_operands(req, frame, code, op, (a, placed.free_dim));
    if !garbage.is_refcounted() {
        return Some(Ok(Next::jump(at.saturating_add(2))));
    }
    if let Some(text) = garbage.as_str() {
        req.heap_mut().release_string(text);
        return Some(Ok(Next::jump(at.saturating_add(2))));
    }
    req.vm_mut().stand(frame, at);
    req.release(exec::value_of(garbage));
    Some(Ok(advance_over_data(req, frame, at)))
}

/// `ASSIGN_DIM` past its inline path: the container placed for the
/// write ([`write_container`]) and the write each kind of container
/// takes. A call of its own, as [`fetched_obj`] is.
#[inline(never)]
fn assigned_dim(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let (at, data) = data_after(req, frame, code, op)?;
    let Some(place) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::error(
            "InternalError: ZEND_ASSIGN_DIM over a constant or a temporary, which php compiles no write over",
        ));
    };
    let container = write_container(req, (frame, code, op), place, Walk::Assign);
    let ran_on = req.pending();
    let written = match container {
        Ok(Container::Array(array)) if op.op2_type == OperandKind::Unused => {
            match append_value(req, (frame, code), data, array)
                .map(|v| (v, req.array_append(array.heap(), v)))
            {
                // `assign_dim_error`: the handler freed the table.
                None => {
                    write_out(req, frame, code, op, &Value::null());
                    Ok(())
                }
                Some((value, Ok(true))) => {
                    write_out(req, frame, code, op, &value);
                    Ok(())
                }
                Some((_, Ok(false))) => Err(crate::api::raising::cannot_add_element(req)),
                Some((_, Err(thrown))) => Err(thrown),
            }
        }
        Ok(Container::Array(array)) => {
            let landing =
                write_at(req, frame, code, op, array).and_then(|at| write_entry(req, array, at));
            match landing {
                Ok(Landing::Entry(entry)) => {
                    let value = exec::take_operand(req, frame, code, data.op1_type, data.op1);
                    match assign_to_place(req, frame, entry, value) {
                        Ok((landed, garbage)) => {
                            write_out(req, frame, code, op, &landed);
                            req.release(garbage);
                            Ok(())
                        }
                        Err(thrown) => Err(thrown),
                    }
                }
                // `assign_dim_error`: `FREE_OP_DATA` and a `null` result.
                Ok(Landing::Gone) => {
                    exec::free_operand(req, frame, code, data.op1_type, data.op1);
                    write_out(req, frame, code, op, &Value::null());
                    Ok(())
                }
                Ok(Landing::Occupied) => Err(crate::api::raising::cannot_add_element(req)),
                Err(thrown) => Err(thrown),
            }
        }
        Ok(Container::Object(object)) => object_held(req, object, |req| {
            assign_object_dim(req, frame, code, op, data, object)
        }),
        Ok(Container::Text(text_at)) => {
            let keeper = keeper(req, frame, code, (at, op), place, text_at);
            let written = assign_text_dim(req, frame, code, op, data, text_at, &keeper);
            keeper.give_back(req);
            written
        }
        // `assign_dim_error`: `FREE_OP_DATA` and a `null` result.
        Ok(Container::Gone) => {
            exec::free_operand(req, frame, code, data.op1_type, data.op1);
            write_out(req, frame, code, op, &Value::null());
            Ok(())
        }
        Err(thrown) => Err(thrown),
    };
    if op.op2_type != OperandKind::Unused {
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
    }
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    if let Err(thrown) = written {
        let thrown = req.raised_over(thrown, ran_on);
        let thrown = req.free_pending(thrown, |req| {
            exec::free_operand(req, frame, code, data.op1_type, data.op1);
            write_out(req, frame, code, op, &Value::null());
        });
        return threw(req, frame, &thrown);
    }
    Ok(advance_over_data(req, frame, at))
}

/// The value `$a[] = $v` appends, read as `ZEND_ASSIGN_DIM` reads it
/// (`Z/Zend/zend_vm_def.h:2674`): an undefined CV's warning is raised
/// under one more owner of the table, and where the handler it reached
/// gave up the last other one the table is freed and nothing is
/// appended, `None`, which is `assign_dim_error`.
fn append_value(
    req: &mut Request,
    (frame, code): (FrameRef, &OpArray),
    data: Opline,
    array: PhpArray,
) -> Option<Value> {
    if data.op1_type != OperandKind::Cv || req.slot(frame, data.op1).tag() != TypeTag::Undef {
        return Some(exec::take_operand(
            req,
            frame,
            code,
            data.op1_type,
            data.op1,
        ));
    }
    let held = Owner::take(req, Value::array(array));
    let value = exec::take_operand(req, frame, code, data.op1_type, data.op1);
    (!held.let_go(req).freed()).then_some(value)
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
    req.replace_slot(frame, slot, inner);
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
    /// The `OP_DATA` is an undefined CV already read, its warning raised
    /// before the container was looked at, as `ZEND_ASSIGN_OBJ_OP` reads
    /// the value before it tests the object.
    read: bool,
}

impl Compound<'_> {
    /// `GET_OP_DATA_ZVAL_PTR(BP_VAR_R)`, or the `null` an undefined CV
    /// read early answered (`zval_undefined_cv`).
    fn value(self, req: &mut Request, frame: FrameRef, code: &OpArray) -> Value {
        if self.read {
            return Value::null();
        }
        exec::operand(req, frame, code, self.data.op1_type, self.data.op1)
    }
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
/// raises no warning at all. A key the table holds is found once, and
/// the entry found is the one answered.
fn rw_entry(req: &mut Request, array: PhpArray, at: WriteAt) -> Result<Landing, Thrown> {
    match at {
        WriteAt::At(key) => match array.heap().value_ref(key) {
            Some(entry) => Ok(Landing::Entry(entry)),
            None => undefined_key_write(req, array, key),
        },
        at => write_entry(req, array, at),
    }
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
    let held = Owner::take(req, Value::array(array));
    let name = match key {
        php_heap::Key::Str(s) => Some(Owner::take(req, exec::value_of(php_heap::Zval::string(s)))),
        php_heap::Key::Long(_) => None,
    };
    let message = match key {
        php_heap::Key::Long(n) => format!("Undefined array key {n}"),
        php_heap::Key::Str(s) => format!(
            "Undefined array key \"{}\"",
            String::from_utf8_lossy(s.bytes())
        ),
    };
    crate::api::raising::warning(req, &message);
    let left = held.let_go(req);
    let landing = if req.is_pending() {
        Err(Thrown::of(None))
    } else if left.not_alone() {
        Ok(Landing::Gone)
    } else {
        req.array_lookup(array.heap(), key).map(Landing::Entry)
    };
    if let Some(name) = name {
        name.let_go(req);
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
///
/// Over a string the offset is checked as a write checks it first,
/// `zend_check_string_offset` under `BP_VAR_RW`, its warning before the
/// `Error`, and a handler of that warning that threw is all there is.
///
/// ```php
/// $s = "abc";
/// try { $s[true] .= "x"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// set_error_handler(function ($no, $m) { throw new Exception($m); });
/// try { $s[null] += 1; } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Warning: String offset cast occurred in %s on line 3
/// Cannot use assign-op operators with string offsets
/// Exception: String offset cast occurred
/// ```
#[cfg_attr(not(debug_assertions), inline(always))] // design §5.8, as `fetch_dim_r`
#[cfg_attr(debug_assertions, inline(never))] // a debug `serve_frame` holds every inlined arm's locals
#[allow(clippy::inline_always)]
pub(crate) fn assign_dim_op<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    let kinds = (A::default(), B::default());
    if let Some(edge) = inline_assign_dim_op(req, frame, (code, op, at), kinds) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    assigned_dim_op(req, frame, code, op)
}

/// The inline path of `ZEND_ASSIGN_DIM_OP` over a CV
/// (`ZEND_ASSIGN_DIM_OP_SPEC_CV_*`, `Z/Zend/zend_vm_def.h:1156-1200`,
/// `assign_dim_op_array`): in the array the CV alone owns, or the one a
/// reference it holds names ([`owned_array`]), the element at the key
/// [`inline_key`] reads, where it holds a number, and an `OP_DATA` value
/// that is a number as it stands, under an operator
/// [`inline::compound`] names, computed by [`inline::binary`] as the
/// binary opcode's own inline pair computes it, and written in place and,
/// under `RETURN_VALUE_USED`, into the temporary. Nothing is converted,
/// warned of or given up, so nothing runs. `None`, with nothing touched,
/// for everything else, which is the kernel's: a key the array has not
/// got and its `Undefined array key` warning, an append, an element that
/// is a reference or no number, a value that is no number, another
/// operator, `.=` among them, and a shared array, which the kernel
/// separates.
///
/// ```php
/// function f(array &$v) { $v[0] += 0.5; $v[1] -= 1; $v["k"] *= 3; return $v; }
/// $a = [1, PHP_INT_MAX, 1.5, "k" => 2]; $b = $a; $i = 1; $one = 1;
/// $a[0] += 2; $a[$i] += $one; $a[2] *= 2; $a["k"] -= 0.5;
/// var_dump($a, $b[1]);
/// $r = ($a[0] += 2); $t = ($a[0] -= $r);
/// var_dump($r, $t);
/// $c = [1, 2]; var_dump(f($c), $c);
/// $d = [1]; $d[] = 2; $e = $d; $d[0] += 1; var_dump($d[0], $e[0]);
/// $m = [1]; $m[1] += 1; $m[0] .= "x"; $m[] += 2; var_dump($m);
/// $x = 5; $n = [1]; $n[0] = &$x; $n[0] += 1; var_dump($x);
/// $s = ["3"]; $s[0] += 1; $u = [1]; $u[0] += "2"; $w = [2]; $w[0] /= 2; $p = [2]; $p[0] **= 3; var_dump($s, $u, $w, $p);
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   int(3)
///   [1]=>
///   float(9.223372036854776E+18)
///   [2]=>
///   float(3)
///   ["k"]=>
///   float(1.5)
/// }
/// int(9223372036854775807)
/// int(5)
/// int(0)
///
/// Warning: Undefined array key "k" in %s on line 2
/// array(3) {
///   [0]=>
///   float(1.5)
///   [1]=>
///   int(1)
///   ["k"]=>
///   int(0)
/// }
/// array(3) {
///   [0]=>
///   float(1.5)
///   [1]=>
///   int(1)
///   ["k"]=>
///   int(0)
/// }
/// int(2)
/// int(1)
///
/// Warning: Undefined array key 1 in %s on line 10
/// array(3) {
///   [0]=>
///   string(2) "1x"
///   [1]=>
///   int(1)
///   [2]=>
///   int(2)
/// }
/// int(6)
/// array(1) {
///   [0]=>
///   int(4)
/// }
/// array(1) {
///   [0]=>
///   int(3)
/// }
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// array(1) {
///   [0]=>
///   int(8)
/// }
/// ```
#[inline(always)] // as `inline_fetch_dim_r`
#[allow(clippy::inline_always)]
fn inline_assign_dim_op(
    req: &mut Request,
    frame: FrameRef,
    (code, op, at): (&OpArray, &Opline, u32),
    (a, b): (impl Operand, impl Operand),
) -> Option<Edge> {
    let rule = inline::compound(op.extended_value)?;
    if a.kind() != OperandKind::Cv {
        return None;
    }
    let data = *exec::opline_at(code, at.saturating_add(1))
        .filter(|data| data.opcode == Opcode::OpData)?;
    let slots = req.slots_mut(frame);
    let array = owned_array(slots, code, op, a)?;
    let (key, free_dim) = inline_key(slots, code, op, b)?;
    let entry = array.value_ref(key)?;
    // `get_op_data_zval_ptr_r`: php specialises no `OP_DATA` kind here.
    let value = ByKind(data.op1_type).read(slots, code, data.op1);
    let z = inline::binary(rule, entry.read(), value)?;
    if op.out_type != OperandKind::Unused {
        write_owning_nothing(slots, code, op, z)?;
    }
    entry.write(z);
    if free_dim {
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
    }
    Some(Ok(Next::jump(at.saturating_add(2))))
}

/// `ASSIGN_DIM_OP` past its inline path: the container walked, the
/// entry found or laid down after its warning, and the kernel over it.
/// A call of its own, as [`assigned_dim`] is.
#[inline(never)]
fn assigned_dim_op(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let Some(kernel) = binary_op_of(op.extended_value) else {
        return Err(Fatal::error(format!(
            "InternalError: ZEND_ASSIGN_DIM_OP over the operator {}, which is no binary operation",
            op.extended_value
        )));
    };
    let (at, data) = data_after(req, frame, code, op)?;
    let step = Compound {
        op,
        data,
        kernel,
        read: false,
    };
    let Some(place) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::error(
            "InternalError: ZEND_ASSIGN_DIM_OP over a constant or a temporary, which php compiles no write over",
        ));
    };
    let container = write_container(req, (frame, code, op), place, Walk::AssignOp);
    let ran_on = req.pending();
    let written = match container {
        Ok(Container::Array(array)) => {
            let landing =
                write_at(req, frame, code, op, array).and_then(|at| rw_entry(req, array, at));
            match landing {
                Ok(Landing::Entry(entry)) => stepped_entry(req, frame, code, step, entry),
                // `assign_dim_op_ret_null`: `FREE_OP_DATA` and a `null`
                // result.
                Ok(Landing::Gone) => {
                    exec::free_operand(req, frame, code, data.op1_type, data.op1);
                    write_out(req, frame, code, op, &Value::null());
                    Ok(())
                }
                Ok(Landing::Occupied) => Err(crate::api::raising::cannot_add_element(req)),
                Err(thrown) => Err(thrown),
            }
        }
        Ok(Container::Object(object)) => object_held(req, object, |req| {
            binary_assign_op_obj_dim(req, frame, code, step, object)
        }),
        Ok(Container::Text(_)) if op.op2_type == OperandKind::Unused => Err(
            crate::api::raising::throw_error(req, "Error", "[] operator not supported for strings"),
        ),
        // `zend_binary_assign_op_dim_slow`: `zend_check_string_offset`
        // under `BP_VAR_RW`, its warnings and its `Error`, and then
        // `zend_wrong_string_offset_error`, nothing over a throwable a
        // handler of those warnings left.
        Ok(Container::Text(_)) => {
            let dim = exec::operand(req, frame, code, op.op2_type, op.op2);
            match string_dim_offset(req, &dim, DimRead::Warning) {
                Err(thrown) => Err(thrown),
                Ok(_) if req.is_pending() => Err(Thrown::of(None)),
                Ok(_) => Err(crate::api::raising::throw_error(
                    req,
                    "Error",
                    "Cannot use assign-op operators with string offsets",
                )),
            }
        }
        // `assign_dim_op_ret_null`.
        Ok(Container::Gone) => {
            exec::free_operand(req, frame, code, data.op1_type, data.op1);
            write_out(req, frame, code, op, &Value::null());
            Ok(())
        }
        Err(thrown) => Err(thrown),
    };
    if op.op2_type != OperandKind::Unused {
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
    }
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    if let Err(thrown) = written {
        let thrown = req.raised_over(thrown, ran_on);
        let thrown = req.free_pending(thrown, |req| {
            exec::free_operand(req, frame, code, data.op1_type, data.op1);
            write_out(req, frame, code, op, &Value::null());
        });
        return threw(req, frame, &thrown);
    }
    Ok(advance_over_data(req, frame, at))
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
    let Compound {
        op, data, kernel, ..
    } = step;
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
    // `EX_USES_STRICT_TYPES()`: the regime of the frame the opline
    // stands in.
    let (landed, garbage) = assign_to_place(req, frame, entry, stepped)?;
    write_out(req, frame, code, op, &landed);
    req.release(garbage);
    Ok(())
}

/// `zend_binary_assign_op_obj_dim` (`Z/Zend/zend_execute.c:1638`), the
/// arm `ZEND_ASSIGN_DIM_OP` takes over an object: `offsetGet` with the
/// dimension under `BP_VAR_R`, the kernel over what it answered and the
/// value, and `offsetSet` with what it made, and the value given up
/// after it. Where `zend_std_read_dimension` raised, `offsetGet` having
/// or the class being no `ArrayAccess`, `zend_use_object_as_array`
/// raises php's `Error` `Cannot use object of type C as array` over it,
/// chained ([`Request::std_dimensions`]). The temporary takes what the operation made and never what
/// `offsetSet` returned.
///
/// ```php
/// class A implements ArrayAccess {
///     function offsetGet($x): mixed { echo "get\n"; return 1; }
///     function offsetSet($x, $y): void { echo "set $y\n"; }
///     function offsetExists($x): bool { return true; }
///     function offsetUnset($x): void {}
/// }
/// class D { function __toString(): string { return "d"; } function __destruct() { echo "dtor\n"; } }
/// $a = new A;
/// $a[0] .= new D;
/// $o = new stdClass;
/// try { $o[0] .= 1; } catch (Error $e) { echo $e->getMessage(), " / ", $e->getPrevious()->getMessage(), "\n"; }
/// ```
/// ```expect
/// get
/// set 1d
/// dtor
/// Cannot use object of type stdClass as array / Cannot use object of type stdClass as array
/// ```
///
/// A class an extension serves answers for itself, as
/// `spl_array_read_dimension_ex` answers `&EG(uninitialized_zval)` and
/// no `NULL`, so nothing is chained over its own `TypeError`:
///
/// ```php
/// $ao = new ArrayObject;
/// try { $ao[[]] += 1; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot access offset of type array on ArrayObject
/// ```
fn binary_assign_op_obj_dim(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    step: Compound<'_>,
    object: PhpObject,
) -> Result<(), Thrown> {
    let Compound {
        op, data, kernel, ..
    } = step;
    let dim = object_dim(req, frame, code, op);
    let value = exec::operand(req, frame, code, data.op1_type, data.op1);
    let table = req.object_handlers(object.heap());
    let read = table.read_dimension(req, object, dim.as_ref(), DimensionRead::Warning);
    let held = match read.map(Owner::hand_on) {
        Ok(held) => held,
        // `zend_use_object_as_array`: php's `read_dimension` answered
        // `NULL`, so the `Error` is raised over what it raised, chained.
        // An extension's table answers for itself.
        Err(thrown) if thrown.exception().is_some() && req.std_dimensions(object.class()) => {
            req.leave_pending(&thrown);
            let ran_on = req.pending();
            let class = req.class_name(object.class());
            let error = crate::api::raising::throw_error(
                req,
                "Error",
                &format!("Cannot use object of type {class} as array"),
            );
            return Err(req.raised_over(error, ran_on));
        }
        Err(thrown) => return Err(thrown),
    };
    let computed = kernel(req, &held, &value);
    req.release(held);
    // `if (zend_binary_op(...) == SUCCESS) write_dimension`, and
    // `FREE_OP_DATA` after it: a destructor the value's release serves
    // is served after `offsetSet`, and over what the operator raised.
    let stepped = computed?;
    // `write_dimension` takes an owner of its own, where php's copies
    // the value, so the one the temporary and the release below read
    // stays.
    let given = Owner::take(req, stepped);
    let written = table.write_dimension(req, object, dim.as_ref(), given);
    if let Err(thrown) = written {
        req.release(stepped);
        return Err(thrown);
    }
    write_out(req, frame, code, op, &stepped);
    req.release(stepped);
    exec::free_operand(req, frame, code, data.op1_type, data.op1);
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
///
/// A property the class serves, where its `get_property_ptr_ptr`
/// answers `NULL`, is `zend_assign_op_overloaded_property`: read
/// through `read_property`, the kernel over it, and the sum written
/// back through `write_property`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r>a</r>');
/// $r = $d->documentElement;
/// var_dump($r->nodeValue .= 'b');
/// echo $d->saveXML($r), "\n";
/// ```
/// ```expect
/// string(2) "ab"
/// <r>ab</r>
/// ```
///
/// A typed property and a typed reference hold what the operation made
/// under the regime of the file the opline stands in
/// (`EX_USES_STRICT_TYPES()`), and a strict file's keep what they held:
///
/// ```php
/// declare(strict_types=1);
/// class A { public string $foo = "100"; public int $n = 1; }
/// $o = new A;
/// try { $o->foo += 5; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $a = ["k" => &$o->n];
/// try { $a["k"] .= "5"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($o->foo, $o->n);
/// ```
/// ```expect
/// Cannot assign int to property A::$foo of type string
/// Cannot assign string to reference held by property A::$n of type int
/// string(3) "100"
/// int(1)
/// ```
///
/// A slot bound to a cell a typed property is a source of is
/// `zend_binary_assign_op_typed_ref`:
///
/// ```php
/// $a = new class { public int $foo = 1; public $_; };
/// $a->_ = &$a->foo;
/// try { $a->_ .= "e50"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($a->_ .= "1", $a->foo);
/// ```
/// ```expect
/// Cannot assign string to reference held by property class@anonymous::$foo of type int
/// int(11)
/// int(11)
/// ```
///
/// A variable nothing wrote is warned of before the `Error`, after the
/// value is read, as `FETCH_OBJ_RW` warns before its own:
///
/// ```php
/// try { $u1->a += $v; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $u4->a->a += 5; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Warning: Undefined variable $v in %s on line 2
///
/// Warning: Undefined variable $u1 in %s on line 2
/// Attempt to assign property "a" on null
///
/// Warning: Undefined variable $u4 in %s on line 3
/// Attempt to modify property "a" on null
/// ```
///
/// An undefined value is read, and warned of, before the object is
/// looked at (`Z/Zend/zend_vm_def.h:1026`), so a handler that replaced
/// the variable is what the operation meets, and php's `Undefined
/// property` comes after the value's warning.
///
/// ```php
/// class C { public $p = 1; }
/// set_error_handler(function ($code, $msg) use (&$o) { echo "$msg\n"; $o = 0; });
/// $o = new C;
/// try { $o->p += $y; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// restore_error_handler();
/// $o = new stdClass;
/// $o->q .= $y;
/// var_dump($o);
/// ```
/// ```expect
/// Undefined variable $y
/// Attempt to assign property "p" on int
///
/// Warning: Undefined variable $y in %s on line 8
///
/// Warning: Undefined property: stdClass::$q in %s on line 8
/// object(stdClass)#1 (1) {
///   ["q"]=>
///   string(0) ""
/// }
/// ```
///
/// A name read from a CV is read, and warned of, before the value:
///
/// ```php
/// $o = new stdClass;
/// $o->$n += $y;
/// ```
/// ```expect
///
/// Warning: Undefined variable $n in %s on line 3
///
/// Warning: Undefined variable $y in %s on line 3
///
/// Warning: Undefined property: stdClass::$ in %s on line 3
/// ```
pub(crate) fn assign_obj_op<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    if let Some(edge) = inline_assign_obj_op(req, frame, code, op) {
        return edge;
    }
    let Some(kernel) = binary_op_of(op.extended_value) else {
        return Err(Fatal::error(format!(
            "InternalError: ZEND_ASSIGN_OBJ_OP over the operator {}, which is no binary operation",
            op.extended_value
        )));
    };
    let (at, data) = data_after(req, frame, code, op)?;
    // The name, and then an undefined CV's warning, before the container
    // is read, whose handler may have replaced it
    // (`Z/Zend/zend_vm_def.h:1023-1026`).
    let mut member = exec::Member::default();
    let name = member.name(req, frame, code, op);
    let read =
        data.op1_type == OperandKind::Cv && req.slot(frame, data.op1).tag() == TypeTag::Undef;
    if read {
        exec::operand(req, frame, code, data.op1_type, data.op1);
    }
    let step = Compound {
        op,
        data,
        kernel,
        read,
    };
    let container =
        exec::container(req, frame, code, op.op1_type, op.op1, "ZEND_ASSIGN_OBJ_OP")?.deref();
    let written = match (name, container.as_object().copied()) {
        (Ok(name), Some(object)) => {
            prime_constant(
                req,
                frame,
                op,
                CacheSlot(data.extended_value),
                object,
                &name,
            );
            step_property(req, frame, code, step, object, &name)?
        }
        (Ok(name), None) => {
            // `GET_OP_DATA_ZVAL_PTR(BP_VAR_R)` and `ZVAL_UNDEFINED_OP1`,
            // in php's order, before `zend_throw_non_object_error`.
            let _value = step.value(req, frame, code);
            if op.op1_type == OperandKind::Cv && matches!(container.as_ref(), ValueRef::Undef) {
                let _ = exec::operand(req, frame, code, op.op1_type, op.op1);
            }
            let ran_on = req.pending();
            let name = name.as_str_bytes().unwrap_or_default();
            let thrown = crate::api::raising::non_object_error(req, "assign", name, &container);
            let thrown = req.raised_over(thrown, ran_on);
            let thrown = req.free_pending(thrown, |req| {
                exec::free_operand(req, frame, code, data.op1_type, data.op1);
                write_out(req, frame, code, op, &Value::null());
            });
            Err(thrown)
        }
        (Err(thrown), _) => {
            exec::free_operand(req, frame, code, data.op1_type, data.op1);
            write_out(req, frame, code, op, &Value::null());
            Err(thrown)
        }
    };
    member.release(req, frame, code, op);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    if let Err(thrown) = written {
        return threw(req, frame, &thrown);
    }
    Ok(advance_over_data(req, frame, at))
}

/// The inline path of `ZEND_ASSIGN_OBJ_OP` over a constant name
/// (`zend_binary_assign_op_obj_dim`'s cached place,
/// `Z/Zend/zend_vm_def.h:1046-1068`): the place the cache slot names
/// ([`cached_place`]) holding a number, and an `OP_DATA` value that is
/// one, under the ten operators [`inline::compound`] names, computed by
/// [`inline::binary`] and written in place and, under
/// `RETURN_VALUE_USED`, into the temporary. `None`, with nothing
/// touched, as [`inline_fetch_obj_r`]'s, for a typed slot, and for every
/// pair the rule leaves to the kernel.
///
/// ```php
/// class C { public $p = 1; }
/// function f($o, $d) { for ($i = 0; $i < 2; $i++) { $o->p += 2; $o->p -= $d; $o->p *= 3; $x = $o->p /= 2; $o->p %= 7; $o->p <<= 1; $o->p |= 8; } return $x; }
/// $c = new C; $s = new stdClass; $s->p = 1.5;
/// var_dump(f($c, 1), $c->p, f($s, 0.5), $s->p);
/// $s->p = PHP_INT_MAX; $s->p += 1; var_dump($s->p);
/// ```
/// ```expect
///
/// Deprecated: Implicit conversion from float 22.5 to int loses precision in %s on line 3
///
/// Deprecated: Implicit conversion from float 4.5 to int loses precision in %s on line 3
///
/// Deprecated: Implicit conversion from float 14.25 to int loses precision in %s on line 3
/// float(22.5)
/// int(10)
/// float(14.25)
/// int(8)
/// float(9.223372036854776E+18)
/// ```
fn inline_assign_obj_op(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Option<Edge> {
    let rule = inline::compound(op.extended_value)?;
    let (a, b) = (ByKind(op.op1_type), ByKind(op.op2_type));
    let object = inline_receiver(req, frame, code, op, a, b)?;
    let (at, data) = data_after(req, frame, code, op).ok()?;
    // `CACHE_ADDR((opline+1)->extended_value)`: the operator is in the
    // opline's own field, so the slot is in the `OP_DATA`'s.
    let slot = CacheSlot(data.extended_value);
    let (place, typed) = cached_place(req, (frame, code, op), slot, object, true)?;
    if typed {
        return None;
    }
    let slots = req.slots_mut(frame);
    // `get_op_data_zval_ptr_r`: php specialises no `OP_DATA` kind here.
    let value = ByKind(data.op1_type).read(slots, code, data.op1);
    let z = inline::binary(rule, place.read(), value)?;
    if op.out_type != OperandKind::Unused {
        write_owning_nothing(slots, code, op, z)?;
    }
    place.write(z);
    Some(Ok(Next::jump(at.saturating_add(2))))
}

/// The operation of a compound assignment over a declared property
/// that hands out no place, readonly or set-restricted
/// ([`Request::place_withheld`]): what `read_property` answers and the
/// `OP_DATA` value through the kernel, the value operand given up
/// either way, as `zend_assign_op_overloaded_property` does before its
/// `write_property`.
fn withheld_step(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    step: Compound<'_>,
    object: PhpObject,
    name: &[u8],
) -> Result<Value, Thrown> {
    let Compound {
        op, data, kernel, ..
    } = step;
    let value = step.value(req, frame, code);
    let table = req.object_handlers(object.heap());
    let computed = match table.read_property(req, object, name, PropertyRead::Warning) {
        Ok(held) => {
            let computed = kernel(req, &held, &value);
            held.give_up(req);
            computed
        }
        Err(thrown) => Err(thrown),
    };
    exec::free_operand(req, frame, code, data.op1_type, data.op1);
    if computed.is_err() {
        write_out(req, frame, code, op, &Value::null());
    }
    computed
}

/// `ASSIGN_OBJ_OP`'s body over an object: a property the class declared
/// is the slot in place, the kernel over what it holds and the value,
/// and the write back through [`Request::assign_declared_slot`], held
/// to the declared type where the slot carries one; every other name is
/// `read_property` and `write_property`, php's
/// `zend_assign_op_overloaded_property`.
///
/// Inside a lazy proxy's `__get` for `x`, `$obj->x += 1` resolves the
/// place on the instance while the proxy's guard is lent to it, so the
/// instance lays `x` down and its own `__get` is not served:
///
/// ```php
/// #[AllowDynamicProperties]
/// class C { public $calls = 0;
///     public function __get($n) { echo "__get($n)\n"; if (++$this->calls > 3) return 0; $this->$n += 1; return 7; } }
/// $r = new ReflectionClass(C::class);
/// $p = $r->newLazyProxy(function () { return new C; });
/// var_dump($p->x, $p);
/// ```
/// ```expect
/// __get(x)
///
/// Warning: Undefined property: C::$x in %s on line 4
/// int(7)
/// lazy proxy object(C)#3 (1) {
///   ["instance"]=>
///   object(C)#4 (2) {
///     ["calls"]=>
///     int(1)
///     ["x"]=>
///     int(1)
///   }
/// }
/// ```
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
    let Some(key) = name.as_php_string().copied() else {
        return Err(Fatal::error(
            "InternalError: ZEND_ASSIGN_OBJ_OP over a property whose name is no string",
        ));
    };
    // The guard a proxy's `__get` lent the instance is held while the
    // place is resolved there, and given back before the operation over
    // it, as around php's `get_property_ptr_ptr` on the instance.
    let placed = req.lazy_place(object, &bytes).and_then(|(object, lent)| {
        let ptr = property_ptr(req, object, &bytes, key).and_then(|ptr| match ptr {
            PropertyPtr::Slot(slot, typed) if object.heap().slot(slot).is_undef() => {
                unwritten_step_slot(req, object, &bytes, slot, typed)
            }
            ptr => Ok(ptr),
        });
        req.give_back_guard(lent);
        ptr.map(|ptr| (object, ptr))
    });
    let (object, ptr) = match placed {
        Ok(placed) => placed,
        Err(thrown) => {
            exec::free_operand(req, frame, code, data.op1_type, data.op1);
            write_out(req, frame, code, op, &Value::null());
            return Ok(Err(thrown));
        }
    };
    match ptr {
        PropertyPtr::Served(back) => Ok(served_property_step(
            req, frame, code, step, object, key, back,
        )),
        // A readonly or set-restricted property gets no place, so the
        // operation is made over what `read_property` answers, and what
        // it throws comes before what the write refuses
        // (`zend_assign_op_overloaded_property`).
        PropertyPtr::Withheld => {
            let precomputed = match withheld_step(req, frame, code, step, object, &bytes) {
                Ok(stepped) => stepped,
                Err(thrown) => return Ok(Err(thrown)),
            };
            let silent = req.serves_accessor(object.class(), b"__get");
            match req.declared_slot_for_write(object, &bytes, "modify", silent) {
                Ok(Some((slot, typed))) => step_slot(
                    req,
                    frame,
                    code,
                    step,
                    (object, slot, typed),
                    &bytes,
                    Some(precomputed),
                ),
                Ok(None) => {
                    req.release(precomputed);
                    Err(Fatal::error(
                        "InternalError: ZEND_ASSIGN_OBJ_OP withheld the place of a property no class declared",
                    ))
                }
                Err(thrown) => {
                    req.release(precomputed);
                    write_out(req, frame, code, op, &Value::null());
                    Ok(Err(thrown))
                }
            }
        }
        PropertyPtr::Slot(slot, typed) => {
            step_slot(req, frame, code, step, (object, slot, typed), &bytes, None)
        }
    }
}

/// `zend_std_get_property_ptr_ptr`'s valid-offset arm over a slot
/// nothing has written, for a compound assignment
/// (`Z/Zend/zend_object_handlers.c:1429`): the read's own diagnostic
/// under `BP_VAR_RW` before the value is read, and then `ZVAL_NULL`
/// over the slot. A `__get` the guard is not holding serves an untyped
/// one instead, its class handing out no place at all; a typed slot
/// takes the uninitialised-access error whatever the class serves,
/// which is php's `prop_info && (Z_PROP_FLAG_P(retval) & IS_PROP_UNINIT)`.
fn unwritten_step_slot(
    req: &mut Request,
    object: PhpObject,
    bytes: &[u8],
    slot: u32,
    typed: bool,
) -> Result<PropertyPtr, Thrown> {
    if typed {
        return Err(req.uninitialized_property(object.class(), bytes));
    }
    let shown = String::from_utf8_lossy(bytes).into_owned();
    if req.overloaded_place(object, bytes) {
        return Ok(PropertyPtr::Served(WriteBack::Property));
    }
    let class = req.class_name(object.class());
    crate::api::raising::warning(req, &format!("Undefined property: {class}::${shown}"));
    Ok(PropertyPtr::Slot(slot, typed))
}

/// What `zend_std_get_property_ptr_ptr` answers an in-place operation
/// over a property under `BP_VAR_RW`, a compound assignment's or an
/// increment's: resolved while the guard a proxy's `__get` lent the
/// instance stands ([`Request::lazy_place`]), and operated over once it
/// is given back.
enum PropertyPtr {
    /// php's `NULL`: the pair of handlers takes the site, the stepped
    /// value written back as the [`WriteBack`] says. A name the class
    /// serves or keeps apart, one no class declared whose `__get` answers
    /// it, and a dynamic entry laid down here, which the pair then finds.
    Served(WriteBack),
    /// A readonly or set-restricted property, which hands out no place:
    /// `zend_assign_op_overloaded_property`, the write refused after it.
    Withheld,
    /// The declared slot, and whether its property is typed.
    Slot(u32, bool),
}

/// The place `zend_std_get_property_ptr_ptr` answers for `name` on
/// `object` under `BP_VAR_RW`, the class's `get_property_ptr_ptr` first:
/// the class's own verdict, a withheld declared property, the declared
/// slot, or, for a name no class declared, the overloaded pair where the
/// class's `__get` answers it and the dynamic entry laid down otherwise,
/// with the creation's diagnostics and the read's warning. An unwritten
/// slot is the caller's to diagnose, the two sites raising differently.
///
/// For a compound assignment over a name no class declared, the place is
/// the dynamic entry, taken before the value is read ([`Request::dynamic_property_ref`]) and so
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
fn property_ptr(
    req: &mut Request,
    object: PhpObject,
    bytes: &[u8],
    key: crate::api::values::PhpString,
) -> Result<PropertyPtr, Thrown> {
    let placed = match place_of(req, object, bytes)? {
        // php's `NULL` from the class's own slot: the class serves the
        // name, so the pair of handlers takes the site, or php's pointer
        // into what the class keeps apart, which the operation changes
        // with nothing written back.
        served @ (PropertyPlace::Handlers | PropertyPlace::Apart) => {
            return Ok(PropertyPtr::Served(WriteBack::of(served)));
        }
        // `zend_std_get_property_ptr_ptr`, whose silence is
        // `ce->__get != NULL`: a class with a `__get` hands out no
        // place for a property the scope may not reach and takes the
        // overloaded arm over it instead.
        PropertyPlace::Std => {
            let silent = req.serves_accessor(object.class(), b"__get");
            // `BP_VAR_RW` reads an unwritten slot before the placement.
            req.read_before_write(object, bytes)?;
            if req.place_withheld(object, bytes) {
                return Ok(PropertyPtr::Withheld);
            }
            req.declared_slot_for_write(object, bytes, "modify", silent)?
        }
    };
    if let Some((slot, typed)) = placed {
        return Ok(PropertyPtr::Slot(slot, typed));
    }
    // `zend_std_get_property_ptr_ptr`'s two answers for a name no class
    // declared: a class with a `__get` the guard is not holding hands
    // out no place and the overloaded pair takes the site; every other
    // class takes the place, which is the dynamic entry laid down here.
    if !req.overloaded_place(object, bytes) {
        req.addref(&Value::string(key));
        req.dynamic_property_ref(object, key, PropertyRead::Warning)?;
    }
    Ok(PropertyPtr::Served(WriteBack::Property))
}

/// `ASSIGN_OBJ_OP` over the declared slot it placed: the kernel over
/// what the slot holds and the `OP_DATA` value, unless the operation was
/// made already over what `read_property` answered, and what it made
/// written back, as it is into an untyped slot and into a typed one
/// held to the declared type under the frame's own regime
/// (`zend_binary_assign_op_typed_prop`), what landed being what the
/// temporary takes.
fn step_slot(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    step: Compound<'_>,
    (object, slot, typed): (PhpObject, u32, bool),
    bytes: &[u8],
    precomputed: Option<Value>,
) -> Result<Result<(), Thrown>, Fatal> {
    let Compound { op, data, .. } = step;
    let computed = precomputed.map_or_else(
        || {
            let value = step.value(req, frame, code);
            let computed = slot_step(req, step, object, (slot, typed), &value);
            exec::free_operand(req, frame, code, data.op1_type, data.op1);
            computed
        },
        Ok,
    );
    let stepped = match computed {
        Ok(stepped) => stepped,
        Err(thrown) => {
            write_out(req, frame, code, op, &Value::null());
            return Ok(Err(thrown));
        }
    };
    land_step(req, frame, code, op, object, (slot, typed), bytes, stepped)
}

/// What a compound step made, written back into the declared slot it
/// read. `ZEND_REF_HAS_TYPE_SOURCES` stands ahead of the property's own
/// type: a slot holding a cell some typed property is a source of is
/// `zend_binary_assign_op_typed_ref`, which the slot's write serves
/// ([`Request::assign_declared_slot`]); only a slot holding none is
/// held to its own declaration first, `zend_binary_assign_op_typed_prop`,
/// under this frame's own regime. What landed is what the temporary
/// takes.
#[allow(clippy::too_many_arguments)] // the handler's frame, the slot and the value, each one fact
fn land_step(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    object: PhpObject,
    (slot, typed): (u32, bool),
    bytes: &[u8],
    stepped: Value,
) -> Result<Result<(), Thrown>, Fatal> {
    let bound = object
        .heap()
        .slot(slot)
        .as_reference()
        .is_some_and(|cell| cell.sources() != 0);
    let strict = req.caller_strict_types(frame);
    if !typed || bound {
        return Ok(req
            .assign_declared_slot(object.heap(), slot, stepped, strict)
            .map(|pair| landed_out(req, frame, code, op, pair)));
    }
    let Some(typed) = req.declared_typed_property(object.class(), bytes) else {
        req.release(stepped);
        let shown = String::from_utf8_lossy(bytes);
        return Err(Fatal::error(format!(
            "InternalError: ZEND_ASSIGN_OBJ_OP placed the typed property ${shown}, whose declared type the class does not carry"
        )));
    };
    let strict = req.frame_strict_types(frame);
    let landed = match req.hold_to_property_type(&typed, bytes, stepped, strict) {
        Ok(landed) => landed,
        Err(thrown) => {
            write_out(req, frame, code, op, &Value::null());
            return Ok(Err(thrown));
        }
    };
    Ok(req
        .assign_declared_slot(object.heap(), slot, landed, strict)
        .map(|pair| landed_out(req, frame, code, op, pair)))
}

/// What a write answered written into the temporary `out` names, then
/// what the place held given up, php's `GC_DTOR(garbage)` after the
/// `ZVAL_COPY` into `result`.
fn landed_out(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    (landed, garbage): (Value, Value),
) {
    write_out(req, frame, code, op, &landed);
    req.release(garbage);
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
///
/// The stepped value is written back as `back` says.
fn served_property_step(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    step: Compound<'_>,
    object: PhpObject,
    name: crate::api::values::PhpString,
    back: WriteBack,
) -> Result<(), Thrown> {
    let Compound {
        op, data, kernel, ..
    } = step;
    let value = step.value(req, frame, code);
    let table = req.object_handlers(object.heap());
    let held = match table.read_property(req, object, name.as_bytes(), back.read()) {
        Ok(held) => held,
        Err(thrown) => {
            exec::free_operand(req, frame, code, data.op1_type, data.op1);
            write_out(req, frame, code, op, &Value::null());
            return Err(thrown);
        }
    };
    let computed = kernel(req, &held, &value);
    held.give_up(req);
    exec::free_operand(req, frame, code, data.op1_type, data.op1);
    let stepped = match computed {
        Ok(stepped) => stepped,
        Err(thrown) => {
            write_out(req, frame, code, op, &Value::null());
            return Err(thrown);
        }
    };
    write_out(req, frame, code, op, &stepped);
    if back == WriteBack::Apart {
        req.release(stepped);
        return Ok(());
    }
    // `write_property` takes an owner of the value and of the name, on
    // every arm: the stepped value's own, which php's `zval_ptr_dtor(&res)`
    // gives up after the write, and one more of the name, which the
    // operand keeps its own of.
    let name = Owner::take(req, name);
    let stepped = Owner::adopt(stepped);
    let table = req.object_handlers(object.heap());
    table.write_property(req, object, name, stepped)
}

/// Where a compound write or a step over a property the class serves
/// goes after the operation: back through `write_property`
/// (`zend_assign_op_overloaded_property`), or nowhere, the class having
/// answered a place of its own ([`PropertyPlace::Apart`], php's pointer
/// into `sxe->tmp`) that the operation changes in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteBack {
    Property,
    Apart,
}

impl WriteBack {
    /// Where the class's verdict sends the stepped value.
    fn of(place: PropertyPlace) -> WriteBack {
        match place {
            PropertyPlace::Apart => WriteBack::Apart,
            PropertyPlace::Handlers | PropertyPlace::Std => WriteBack::Property,
        }
    }

    /// The read the operation makes: `BP_VAR_R` through the pair of
    /// handlers, and the write fetch that lays the place down for one
    /// the class keeps apart.
    fn read(self) -> PropertyRead {
        match self {
            WriteBack::Property => PropertyRead::Warning,
            WriteBack::Apart => PropertyRead::Write,
        }
    }
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
///
/// A property the class serves is the overloaded pair, both ways
/// round: `DateInterval` answers `d` from the interval itself, so the
/// step lands there and no dynamic property is laid down.
///
/// ```php
/// $i = new DateInterval('P1D');
/// var_dump($i->d++, ++$i->d, $i->d--, --$i->d);
/// var_dump($i->format('%d'));
/// ```
/// ```expect
/// int(1)
/// int(3)
/// int(3)
/// int(1)
/// string(1) "1"
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
    if let Some(edge) = inline_incdec_obj(req, frame, code, op, step) {
        return edge;
    }
    let container = match op.op1_type {
        OperandKind::Cv => exec::operand(req, frame, code, op.op1_type, op.op1),
        _ => exec::container(req, frame, code, op.op1_type, op.op1, "ZEND_PRE_INC_OBJ")?,
    }
    .deref();
    let mut member = exec::Member::default();
    let stepped = match (
        member.name(req, frame, code, op),
        container.as_object().copied(),
    ) {
        (Ok(name), Some(object)) => {
            prime_constant(req, frame, op, property_cache_slot(op), object, &name);
            incdec_property(req, frame, code, op, step, object, &name)?
        }
        (Ok(name), None) => {
            let name = name.as_str_bytes().unwrap_or_default();
            let thrown =
                crate::api::raising::non_object_error(req, "increment/decrement", name, &container);
            write_out(req, frame, code, op, &Value::null());
            Err(thrown)
        }
        (Err(thrown), _) => {
            write_out(req, frame, code, op, &Value::null());
            Err(thrown)
        }
    };
    member.release(req, frame, code, op);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    match stepped {
        Ok(()) => Ok(advance(req, frame)),
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// The inline path of the four `*_INC_OBJ`/`*_DEC_OBJ` over a constant
/// name (`zend_pre_incdec_property_zval` and its `POST_` twin over the
/// cached place, `Z/Zend/zend_execute.c:2283`): the place the cache slot
/// names ([`cached_place`]) holding a number, stepped by
/// [`inline::step`] in place, the value before or after it in the
/// temporary. `None`, with nothing touched, as [`inline_fetch_obj_r`]'s,
/// for a typed slot, and for anything that is no number.
///
/// ```php
/// class C { public $p = 1; }
/// function f($o) { for ($i = 0; $i < 2; $i++) { $o->p++; ++$o->p; $o->p--; $a = $o->p++; $b = --$o->p; } return [$a, $b]; }
/// $c = new C; $s = new stdClass; $s->p = 0.5; $m = new stdClass; $m->p = PHP_INT_MAX - 2;
/// var_dump(f($c), $c->p, f($s), $s->p, f($m), $m->p);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(3)
///   [1]=>
///   int(3)
/// }
/// int(3)
/// array(2) {
///   [0]=>
///   float(2.5)
///   [1]=>
///   float(2.5)
/// }
/// float(2.5)
/// array(2) {
///   [0]=>
///   float(9.223372036854776E+18)
///   [1]=>
///   float(9.223372036854776E+18)
/// }
/// float(9.223372036854776E+18)
/// ```
fn inline_incdec_obj(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    step: IncDec,
) -> Option<Edge> {
    let (a, b) = (ByKind(op.op1_type), ByKind(op.op2_type));
    let object = inline_receiver(req, frame, code, op, a, b)?;
    let (place, typed) = cached_place(
        req,
        (frame, code, op),
        property_cache_slot(op),
        object,
        true,
    )?;
    if typed {
        return None;
    }
    let held = place.read();
    let arith = if step.increment {
        Arith::Add
    } else {
        Arith::Sub
    };
    let stepped = inline::step(arith, held)?;
    if op.out_type != OperandKind::Unused {
        let out = if step.post { held } else { stepped };
        write_owning_nothing(req.slots_mut(frame), code, op, out)?;
    }
    place.write(stepped);
    Some(Ok(advanced()))
}

/// The step over an object's property, `get_property_ptr_ptr` under
/// `BP_VAR_RW` and what php does with the place it answers: a declared
/// slot stepped in place and held to its declared type, the dynamic
/// entry laid down, or the overloaded pair where the class hands out no
/// place. A readonly property holding an object gets none either, so
/// the step throws and the write it goes on to throws again, chained:
///
/// ```php
/// class T { public readonly object $p; function __construct() { $this->p = new stdClass; } }
/// $t = new T;
/// try { $t->p++; } catch (Error $e) { echo $e->getMessage(), ' / ', $e->getPrevious()->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot modify readonly property T::$p / Cannot increment stdClass
/// ```
///
/// Inside a lazy proxy's `__get` for `x`, `$obj->x++` resolves the place
/// on the instance while the proxy's guard is lent to it, so the
/// instance lays `x` down and its own `__get` is not served:
///
/// ```php
/// #[AllowDynamicProperties]
/// class D { public $calls = 0;
///     public function __get($n) { echo "__get($n)\n"; if (++$this->calls > 3) return 0; $this->$n++; return 7; } }
/// $r = new ReflectionClass(D::class);
/// $p = $r->newLazyProxy(function () { return new D; });
/// var_dump($p->x, $p);
/// ```
/// ```expect
/// __get(x)
///
/// Warning: Undefined property: D::$x in %s on line 4
/// int(7)
/// lazy proxy object(D)#3 (1) {
///   ["instance"]=>
///   object(D)#4 (2) {
///     ["calls"]=>
///     int(1)
///     ["x"]=>
///     int(1)
///   }
/// }
/// ```
fn incdec_property(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    step: IncDec,
    object: PhpObject,
    name: &Value,
) -> Result<Result<(), Thrown>, Fatal> {
    let bytes = name.as_str_bytes().unwrap_or_default();
    let Some(key) = name.as_php_string().copied() else {
        return Err(Fatal::error(
            "InternalError: an increment of a property whose name is no string",
        ));
    };
    // The guard a proxy's `__get` lent the instance is held while the
    // place is resolved there, and given back before the step over it.
    let placed = req.lazy_place(object, bytes).and_then(|(object, lent)| {
        let ptr = property_ptr(req, object, bytes, key).map(|ptr| match ptr {
            // [`Request::read_before_write`] raised what an unwritten
            // slot raises before the placement; one it let by is a
            // `__get`'s to answer.
            PropertyPtr::Slot(slot, _)
                if object.heap().slot(slot).is_undef() && req.overloaded_place(object, bytes) =>
            {
                PropertyPtr::Served(WriteBack::Property)
            }
            ptr => ptr,
        });
        req.give_back_guard(lent);
        ptr.map(|ptr| (object, ptr))
    });
    let (object, (slot, typed)) = match placed {
        Ok((object, PropertyPtr::Slot(slot, typed))) => (object, (slot, typed)),
        Ok((object, PropertyPtr::Served(back))) => {
            return Ok(served_incdec(req, frame, code, op, step, object, key, back));
        }
        // A readonly or set-restricted property gets no place, so the
        // step is the overloaded one, and what the write refuses is
        // chained onto what the step threw.
        Ok((object, PropertyPtr::Withheld)) => {
            return Ok(overloaded_incdec(req, frame, code, op, step, object, key));
        }
        Err(thrown) => {
            write_out(req, frame, code, op, &Value::null());
            return Ok(Err(thrown));
        }
    };
    let named = exec::value_of(object.heap().slot(slot));
    let held = named.deref().undef_as_null();
    req.addref(&held);
    let stepped = match step.kernel()(req, &held) {
        Ok(stepped) => stepped,
        Err(thrown) => {
            req.release(held);
            write_out(req, frame, code, op, &Value::null());
            return Ok(Err(thrown));
        }
    };
    // `zend_pre_incdec_property_zval`: a slot bound to a reference other
    // properties type is `zend_incdec_typed_ref`, not the property's own
    // step.
    if let Some(cell) = named
        .as_php_ref()
        .copied()
        .filter(|cell| cell.heap().sources() != 0)
    {
        return Ok(step_property_ref(
            req, frame, code, op, step, cell, held, stepped,
        ));
    }
    let landed = if typed {
        match typed_step(req, frame, object, slot, bytes, step, (held, stepped))? {
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
    let strict = req.frame_strict_types(frame);
    Ok(req
        .assign_declared_slot(object.heap(), slot, landed, strict)
        .map(|(_, garbage)| req.release(garbage)))
}

/// The property step over a slot bound to a reference other properties
/// type: [`step_through_typed_ref`] over what the cell held, the
/// temporary written with the value before or after it, and the cell
/// written unless the properties refused the step. `held` and `stepped`
/// are given up or landed here.
#[allow(clippy::too_many_arguments)]
fn step_property_ref(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    step: IncDec,
    cell: PhpRef,
    held: Value,
    stepped: Value,
) -> Result<(), Thrown> {
    let strict = req.frame_strict_types(frame);
    let (stored, raised) =
        match step_through_typed_ref(req, cell, &held, stepped, step.increment, strict) {
            RefStep::Landed(stored) => (stored, None),
            RefStep::Clamped(stored, thrown) => (stored, Some(thrown)),
            RefStep::Refused(thrown) => {
                req.release(held);
                write_out(req, frame, code, op, &Value::null());
                return Err(thrown);
            }
        };
    write_out(
        req,
        frame,
        code,
        op,
        if step.post { &held } else { &stored },
    );
    req.release(held);
    req.reference_set(cell.heap(), stored);
    raised.map_or(Ok(()), Err)
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
        return Err(Fatal::error(format!(
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
            (step.increment, crate::ops::Holder::Property),
        );
        req.release(stepped);
        // The slot holds no cell another property types, which the
        // caller took to `zend_incdec_typed_ref` first, so the store is
        // the property's own.
        let strict = req.frame_strict_types(frame);
        let stored = req.assign_declared_slot(object.heap(), slot, limit, strict);
        return Ok(stored
            .map(|(_, garbage)| req.release(garbage))
            .and(Err(thrown)));
    }
    // A refusal is `ZVAL_COPY_VALUE(var_ptr, copy)`: the slot keeps
    // what it held.
    let strict = req.frame_strict_types(frame);
    Ok(req.hold_to_property_type(&declared, bytes, stepped, strict))
}

/// `zend_pre_incdec_overloaded_property` and
/// `zend_post_incdec_overloaded_property` (`Z/Zend/zend_execute.c:2367`):
/// `read_property` under `BP_VAR_R`, the step over a dereferenced copy
/// of what it answered, the temporary written with the value before or
/// after it, and `write_property` with the stepped value, or with the
/// copy unchanged where the step was refused.
///
/// ```php
/// $o = new class { public $bar; function __construct() { $this->bar = new stdClass; } function &__get($x) { return $this->bar; } };
/// try { $o->foo++; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(isset($o->foo));
/// ```
/// ```expect
/// Deprecated: Creation of dynamic property class@anonymous::$foo is deprecated in %s on line 3
/// Cannot increment stdClass
/// bool(true)
/// ```
fn overloaded_incdec(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    step: IncDec,
    object: PhpObject,
    name: crate::api::values::PhpString,
) -> Result<(), Thrown> {
    served_incdec(
        req,
        frame,
        code,
        op,
        step,
        object,
        name,
        WriteBack::Property,
    )
}

/// [`overloaded_incdec`] with the stepped value written back as `back` says.
#[allow(clippy::too_many_arguments)] // the site's operands, and where the result goes
fn served_incdec(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    step: IncDec,
    object: PhpObject,
    name: crate::api::values::PhpString,
    back: WriteBack,
) -> Result<(), Thrown> {
    let table = req.object_handlers(object.heap());
    let read = table.read_property(req, object, name.as_bytes(), back.read());
    let held = match read {
        Ok(held) => held,
        Err(thrown) => {
            write_out(req, frame, code, op, &Value::null());
            return Err(thrown);
        }
    };
    let copy = held.deref();
    req.addref(&copy);
    held.give_up(req);
    // A step `increment_function` refuses leaves the copy as it was,
    // and php writes it back all the same, the refusal standing.
    let (stepped, refused) = match step.kernel()(req, &copy) {
        Ok(stepped) => (stepped, None),
        Err(thrown) => {
            req.addref(&copy);
            (copy, Some(thrown))
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
    if back == WriteBack::Apart {
        req.release(stepped);
        return Ok(());
    }
    // `write_property` takes an owner of the value and of the name,
    // the stepped value's own and one more of the name.
    let name = Owner::take(req, name);
    let stepped = Owner::adopt(stepped);
    let table = req.object_handlers(object.heap());
    let wrote = table.write_property(req, object, name, stepped);
    match (refused, wrote) {
        (None, wrote) => wrote,
        (Some(refusal), Ok(())) => Err(refusal),
        // What the write raised is thrown over the refusal, which
        // becomes its previous as `zend_throw_exception_internal` makes it.
        (Some(refusal), Err(raised)) => match (refusal.exception(), raised.exception()) {
            (Some(&first), Some(&second)) => {
                req.set_previous(second, first);
                Err(raised)
            }
            _ => Err(raised),
        },
    }
}

/// `zobj->handlers->get_property_ptr_ptr` asked for its verdict alone
/// ([`PropertyPlace`]): whether a write to `name` on the object is the
/// standard place, which the property kernels lay down themselves, or
/// goes through the class's `read_property` and `write_property`.
fn place_of(req: &mut Request, object: PhpObject, name: &[u8]) -> Result<PropertyPlace, Thrown> {
    let table = req.object_handlers(object.heap());
    table.get_property_ptr_ptr(req, object, name)
}

/// `ZVAL_COPY_VALUE(EX_VAR(opline->result.var), value)`: the temporary
/// `out` names takes `value`'s owner, and what it held is released.
fn set_out(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline, value: Value) {
    req.replace_slot(frame, exec::temporary_slot(code, op.out), value);
}

/// `if (RETURN_VALUE_USED(opline)) ZVAL_COPY(EX_VAR(opline->result.var), value)`:
/// the temporary `out` names one more owner of `value`, and nothing
/// where the compiler marked the temporary `UNUSED`.
fn write_out(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline, value: &Value) {
    if op.out_type == OperandKind::Unused {
        return;
    }
    req.addref(value);
    set_out(req, frame, code, op, *value);
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
    // The zval taken and given up as it stands, no `Value` made of it, so
    // the release is `i_zval_ptr_dtor`'s decrement with no kind asked.
    let garbage = req
        .slots_mut(frame)
        .get_mut(op.op1 as usize)
        .map_or(Zval::UNDEF, |slot| std::mem::replace(slot, Zval::UNDEF));
    req.release_zval(garbage);
    Ok(advance(req, frame))
}

/// `ZEND_FETCH_DIM_UNSET` (`Z/Zend/zend_vm_def.h`, handler 96), a link
/// under what an `unset` deletes, `$a[0]` of `unset($a[0][1])`:
/// `zend_fetch_dimension_address` under `BP_VAR_UNSET`, which finds a
/// place and makes none. An array is separated and the entry at the key
/// is the `IS_INDIRECT` the `VAR` takes, a key the array has not `null`
/// in silence; an object is `offsetGet` as under a write
/// ([`object_place`] under `DimensionRead::Unset`); a string is the write's `Error`, or `Cannot
/// unset string offsets` where the offset is none php reads as an
/// integer ([`unset_string_offset`]); `null`, a variable nothing wrote
/// (warned of) and `false` (deprecated) are `null`, no array made of
/// them; anything else is php's `Error` `Cannot unset offset in a
/// non-array variable`.
///
/// ```php
/// $a = [[1, 2], 'k' => [3]];
/// unset($a[0][0], $a['k'][0], $a['none'][0], $u[0][0]);
/// var_dump($a);
/// $s = "ab";
/// try { unset($s[0][0]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { unset($s['x']['y']); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $i = 1;
/// try { unset($i[0][0]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Warning: Undefined variable $u in %s on line 3
/// array(2) {
///   [0]=>
///   array(1) {
///     [1]=>
///     int(2)
///   }
///   ["k"]=>
///   array(0) {
///   }
/// }
/// Cannot use string offset as an array
/// Cannot unset string offsets
/// Cannot unset offset in a non-array variable
/// ```
pub(crate) fn fetch_dim_unset<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let Some(place) = write_place(req, frame, code, op.op1_type, op.op1) else {
        return Err(Fatal::error(
            "InternalError: ZEND_FETCH_DIM_UNSET over a constant or a temporary, which php compiles no unset over",
        ));
    };
    let written = match separated_array(req, place) {
        Ok((_, Some(array))) => unset_entry(req, frame, code, op, array),
        Ok((at, None)) => {
            let held = exec::value_of(at.read());
            match held.as_ref() {
                ValueRef::Object(object) => {
                    object_place(req, frame, code, op, *object, DimensionRead::Unset)
                }
                ValueRef::Str(_) => Err(text_dim_place(req, frame, code, op, true)),
                ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false) => {
                    unset_under_nothing(req, frame, code, op, &held)
                }
                _ => Err(crate::api::raising::throw_error(
                    req,
                    "Error",
                    "Cannot unset offset in a non-array variable",
                )),
            }
        }
        Err(thrown) => Err(thrown),
    };
    lay_dim_place(req, frame, code, op, written, Keep::Nothing, None)
}

/// `zend_fetch_dimension_address_inner` under `BP_VAR_UNSET`: the key
/// the dimension makes, and the entry at it as the `IS_INDIRECT` the
/// `VAR` takes, or `null` where the array has none
/// (`&EG(uninitialized_zval)`); nothing is laid down.
fn unset_entry(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    array: PhpArray,
) -> Result<php_heap::Zval, Thrown> {
    let dim = exec::operand(req, frame, code, op.op2_type, op.op2);
    let key = crate::ops::array_offset(req, &dim, op.op2_type == OperandKind::Const)?;
    Ok(array
        .heap()
        .value_ref(key)
        .map_or_else(php_heap::Zval::null, php_heap::Zval::indirect))
}

/// `zend_fetch_dimension_address`'s `return_null` under `BP_VAR_UNSET`,
/// a container of `null`, `false` or nothing: a CV nothing wrote warned
/// of (`ZVAL_UNDEFINED_OP1`), `false` deprecated as the array it is not
/// made into, and a CV dimension nothing wrote warned of after
/// (`ZVAL_UNDEFINED_OP2`).
fn unset_under_nothing(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    held: &Value,
) -> Result<php_heap::Zval, Thrown> {
    if held.tag() == TypeTag::Undef && op.op1_type == OperandKind::Cv {
        let _ = exec::operand(req, frame, code, op.op1_type, op.op1);
    }
    if held.as_bool() == Some(false) {
        crate::api::raising::deprecated(
            req,
            "Automatic conversion of false to array is deprecated",
        );
        none_pending(req)?;
    }
    if op.op2_type == OperandKind::Cv {
        let _ = exec::operand(req, frame, code, op.op2_type, op.op2);
    }
    Ok(php_heap::Zval::null())
}

/// `ZEND_FETCH_OBJ_UNSET` (`Z/Zend/zend_vm_def.h`, handler 97), a
/// property under what an `unset` deletes, `$o->p` of
/// `unset($o->p->q)`: `zend_fetch_property_address` under
/// `BP_VAR_UNSET`, [`fetch_obj_w`]'s place over an object, and over what
/// is none `null` in silence (`if (type == BP_VAR_UNSET) { ZVAL_NULL(result);
/// return; }`), a variable nothing wrote warned of first.
///
/// ```php
/// $o = new stdClass; $o->p = new stdClass; $o->p->q = 1;
/// unset($o->p->q, $n->p->q);
/// var_dump($o);
/// ```
/// ```expect
///
/// Warning: Undefined variable $n in %s on line 3
/// object(stdClass)#1 (1) {
///   ["p"]=>
///   object(stdClass)#2 (0) {
///   }
/// }
/// ```
pub(crate) fn fetch_obj_unset<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let container = exec::container(
        req,
        frame,
        code,
        op.op1_type,
        op.op1,
        "ZEND_FETCH_OBJ_UNSET",
    )?;
    if container.deref().as_object().is_some() {
        return fetch_obj_place(req, frame, code, op, PropertyRead::Silent);
    }
    if container.tag() == TypeTag::Undef && op.op1_type == OperandKind::Cv {
        let _ = exec::operand(req, frame, code, op.op1_type, op.op1);
    }
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    set_out(req, frame, code, op, Value::null());
    Ok(advance(req, frame))
}

/// `ZEND_FETCH_STATIC_PROP_UNSET` (`Z/Zend/zend_vm_def.h`, handler 179's
/// neighbour), a class's property under what an `unset` deletes:
/// `zend_fetch_static_prop_helper` under `BP_VAR_UNSET`, the place
/// [`fetch_static_prop_w`] finds.
///
/// ```php
/// class C { public static $s = ['a' => 1, 'b' => 2]; }
/// unset(C::$s['a']);
/// var_dump(C::$s);
/// ```
/// ```expect
/// array(1) {
///   ["b"]=>
///   int(2)
/// }
/// ```
pub(crate) fn fetch_static_prop_unset<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    static_prop_place(req, frame, code, op, StaticFetch::Unset)
}

/// `ZEND_UNSET_STATIC_PROP` (`Z/Zend/zend_vm_def.h`, handler 179),
/// `unset(C::$s)`: the class fetched as a `static` property's is, and
/// then `zend_std_unset_static_property`, which is always php's `Error`
/// `Attempt to unset static property C::$s`.
///
/// ```php
/// class C { public static $s = 1; }
/// try { unset(C::$s); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump(C::$s);
/// ```
/// ```expect
/// Attempt to unset static property C::$s
/// int(1)
/// ```
pub(crate) fn unset_static_prop<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let (class, name) = match static_member(req, frame, code, op) {
        Ok(member) => member,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let class = req.class_name(class);
    let thrown = crate::api::raising::throw_error(
        req,
        "Error",
        &format!(
            "Attempt to unset static property {class}::${}",
            String::from_utf8_lossy(&name)
        ),
    );
    threw(req, frame, &thrown)
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
        return Err(Fatal::error(
            "InternalError: ZEND_UNSET_DIM over a constant or a temporary, which php compiles no write over",
        ));
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
        // `ZVAL_UNDEFINED_OP2`); a `VAR` is the place a
        // `FETCH_DIM_UNSET` or a `FETCH_OBJ_UNSET` left, read through.
        let container = match op.op1_type {
            OperandKind::Var => exec::value_of(found.0.read()).deref(),
            _ => exec::operand(req, frame, code, op.op1_type, op.op1).deref(),
        };
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
        ValueRef::Str(_) => Err(crate::api::raising::throw_error(
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
            none_pending(req)?;
            Ok(())
        }
        ValueRef::Undef | ValueRef::Null => Ok(()),
        _ => Err(crate::api::raising::throw_error(
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
    let container =
        exec::container(req, frame, code, op.op1_type, op.op1, "ZEND_UNSET_OBJ")?.deref();
    let mut member = exec::Member::default();
    let unset = if let Some(object) = container.as_object().copied() {
        member.name(req, frame, code, op).and_then(|name| {
            let table = req.object_handlers(object.heap());
            table.unset_property(req, object, name.as_str_bytes().unwrap_or_default())
        })
    } else {
        exec::Member::touch(req, frame, code, op);
        Ok(())
    };
    member.release(req, frame, code, op);
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
    set_out(req, frame, code, op, Value::array(array));
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
        return Err(Fatal::error(
            "InternalError: ADD_ARRAY_ELEMENT over a temporary holding no array",
        ));
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
/// kernel keeps a numeric string; then `FREE_OP2`. Under
/// `ZEND_ARRAY_ELEMENT_REF`, `[&$x]`, `op1` is the place a CV or a write
/// fetch names (`GET_OP1_ZVAL_PTR_PTR(BP_VAR_W)`, which warns of
/// nothing), made a reference where it holds none, and the entry is
/// that cell; a `VAR` is freed after it, so `[&$this]` holds a cell
/// nothing else does, which `array_slice()` copies as its value:
///
/// ```php
/// function r(&$r) {}
/// class K { function __construct() { $a = [&$this]; var_dump(ReflectionReference::fromArrayElement($a, 0)); $b = array_slice($a, 0, 1); call_user_func_array('r', $b); } }
/// new K;
/// ```
/// ```expect
/// NULL
///
/// Warning: r(): Argument #1 ($r) must be passed by reference, value given in %s on line 3
/// ```
fn add_element(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    array: crate::api::values::PhpArray,
) -> Edge {
    let value = if op.extended_value & ARRAY_ELEMENT_REF != 0 {
        let Some(at) = write_place(req, frame, code, op.op1_type, op.op1) else {
            return Err(Fatal::error(
                "InternalError: ADD_ARRAY_ELEMENT by reference over an operand php compiles no write over",
            ));
        };
        let bound = bound_at(req, at);
        // `FREE_OP1_VAR_PTR()`: a `VAR` holding a value of its own, what
        // `FETCH_THIS` left for `[&$this]`, gives its owner up, so the
        // cell the entry holds is the entry's alone.
        if op.op1_type == OperandKind::Var {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
        }
        match bound {
            Ok(bound) => bound,
            Err(thrown) => return threw(req, frame, &thrown),
        }
    } else {
        exec::take_operand(req, frame, code, op.op1_type, op.op1)
    };
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

/// `ZEND_ADD_ARRAY_UNPACK` (`Z/Zend/zend_vm_def.h:6334`), `...$x`
/// inside an array: the operand read (`GET_OP1_ZVAL_PTR(BP_VAR_R)`, a CV
/// nothing wrote the warning and `null`) and spread into the array the
/// `result` temporary holds ([`unpack_into`]), then `FREE_OP1`.
///
/// ```php
/// function g() { yield 'a' => 1; yield 3 => 2; yield '7' => 3; }
/// $x = [5 => 'x', 'k' => 'y'];
/// var_dump([0, ...$x, ...g(), ...new ArrayIterator(['k' => 'z'])]);
/// $a = [1]; $r = &$a[0]; $b = [...$a]; $b[0] = 2; var_dump($a[0]);
/// ```
/// ```expect
/// array(6) {
///   [0]=>
///   int(0)
///   [1]=>
///   string(1) "x"
///   ["k"]=>
///   string(1) "z"
///   ["a"]=>
///   int(1)
///   [2]=>
///   int(2)
///   [3]=>
///   int(3)
/// }
/// int(2)
/// ```
///
/// What is neither an array nor a `Traversable` is php's `Error`, an
/// object that is no `Traversable` its `TypeError`, and a key an
/// iterator hands over that is neither an int nor a string its `Error`:
///
/// ```php
/// $n = null;
/// try { var_dump([...$n]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { var_dump([...new stdClass]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// function k() { yield 1.5 => 1; }
/// try { var_dump([...k()]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Error: Only arrays and Traversables can be unpacked, null given
/// TypeError: Only arrays and Traversables can be unpacked, stdClass given
/// Error: Keys must be of type int|string during array unpacking
/// ```
pub(crate) fn add_array_unpack<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let held = req.slot(frame, exec::temporary_slot(code, op.out));
    let Some(array) = held.as_array().copied() else {
        return Err(Fatal::error(
            "InternalError: ADD_ARRAY_UNPACK over a temporary holding no array",
        ));
    };
    let spread = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
    let added = unpack_into(req, array, &spread);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    match added {
        Ok(()) => Ok(advance(req, frame)),
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// The body of [`add_array_unpack`] over `array`, which has one owner:
/// an array's elements in order, a string key written as it stands
/// (`zend_hash_update`) and an integer one dropped for the next index
/// (`zend_hash_next_index_insert`, php's `Error` when it is taken), a
/// reference only this array held followed and any other kept as the
/// reference it is; a `Traversable`'s through its iterator, a string
/// key that is a decimal and an int alike taking the next index and
/// every value's reference followed.
fn unpack_into(req: &mut Request, array: PhpArray, spread: &Value) -> Result<(), Thrown> {
    let a = array.heap();
    if let Some(source) = spread.as_array().copied() {
        // `ZEND_HASH_FOREACH_STR_KEY_VAL(ht, key, val)`
        // (`Z/Zend/zend_vm_def.h:6369`), no owner of the table taken: the
        // operand holds it while an update releases what it overwrites. A
        // reference's count is read as php reads it.
        walk::foreach(req, source, Hold::None, |req, row| -> walk::Step {
            let value = match row.value().as_php_ref() {
                Some(cell) if row.value().refcount() == 1 => cell.get(),
                _ => *row.value(),
            };
            req.addref(&value);
            match row.key() {
                key @ crate::api::values::Key::Str(_) => req.array_update(a, key.heap(), value)?,
                crate::api::values::Key::Int(_) => append_unpacked(req, a, value)?,
            }
            Ok(ControlFlow::Continue(()))
        })?;
        return Ok(());
    }
    let Some(object) = spread.as_object().copied() else {
        let given = crate::ops::value_name(req, spread);
        return Err(crate::api::raising::throw_error(
            req,
            "Error",
            &format!("Only arrays and Traversables can be unpacked, {given} given"),
        ));
    };
    let wrapper = unpack_iterator(req, object)?;
    let walked = (|| -> Result<(), Thrown> {
        crate::interfaces::rewind(req, wrapper)?;
        while let Some((value, key)) = crate::interfaces::delegated(req, wrapper)? {
            let value = exec::owned_deref(req, value);
            let name = match unpacked_key(req, key, "array") {
                Ok(name) => name,
                Err(thrown) => {
                    req.release(value);
                    return Err(thrown);
                }
            };
            match name {
                Some(name) => {
                    let updated = req.array_update(a, php_heap::Key::Str(name.heap()), value);
                    req.release(Value::string(name));
                    updated?;
                }
                None => append_unpacked(req, a, value)?,
            }
        }
        Ok(())
    })();
    req.release(wrapper.as_value());
    walked
}

/// `zend_hash_next_index_insert` of an unpacked element, and
/// `zend_cannot_add_element` where the next index is taken.
fn append_unpacked(req: &mut Request, a: php_heap::Arr, value: Value) -> Result<(), Thrown> {
    if req.array_append(a, value)? {
        return Ok(());
    }
    Err(crate::api::raising::cannot_add_element(req))
}

/// `ce->get_iterator` over an object a `...` spreads: its iterator, one
/// owner the caller's, or php's `TypeError` for an object whose class is
/// no `Traversable`.
fn unpack_iterator(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    if !crate::interfaces::is_traversable(req, object.class()) {
        let given = crate::ops::value_name(req, &object.as_value());
        return Err(crate::api::raising::throw_error(
            req,
            "TypeError",
            &format!("Only arrays and Traversables can be unpacked, {given} given"),
        ));
    }
    crate::interfaces::get_iterator(req, object, false)
}

/// The key an iterator handed a `...` over, one owner the caller holds,
/// read as both unpack handlers read it: an int, or a string that is a
/// decimal (`ZEND_HANDLE_NUMERIC`), is none, the next position; any other
/// string is the name, its owner the caller's; anything else is php's
/// `Error` naming what the unpacking was `during`.
fn unpacked_key(req: &mut Request, key: Value, during: &str) -> Result<Option<PhpString>, Thrown> {
    let key = exec::owned_deref(req, key);
    if let Some(name) = key.as_php_string().copied() {
        if matches!(
            crate::api::values::Key::lookup(name),
            crate::api::values::Key::Int(_)
        ) {
            req.release(key);
            return Ok(None);
        }
        return Ok(Some(name));
    }
    let is_int = key.as_int().is_some();
    req.release(key);
    if is_int {
        return Ok(None);
    }
    Err(crate::api::raising::throw_error(
        req,
        "Error",
        &format!("Keys must be of type int|string during {during} unpacking"),
    ))
}

/// `ZEND_SEND_UNPACK` (`Z/Zend/zend_vm_def.h:5266`), `f(...$args)`: the
/// operand's elements placed in the call being built after the
/// arguments placed before it, a reference followed. An integer key is
/// the next positional argument and a string key a named one, placed by
/// `zend_handle_named_arg` ([`reserve_named`]), so a positional one
/// after a name is php's `Error`; a `Traversable` is walked through its
/// iterator, a key that is a decimal string read as an int.
///
/// ```php
/// function f($a, $b = 'B', ...$rest) { echo $a, ' ', $b, ' ', implode(',', array_keys($rest)), '|', implode(',', $rest), "\n"; }
/// f(...[1, 2, 3]);
/// f(1, ...['b' => 2, 'x' => 3]);
/// function g() { yield 1; yield 'b' => 2; }
/// f(...new ArrayIterator(['a' => 'A', 'z' => 'Z']));
/// f(...g());
/// var_dump(max(...[3, 9, 4]), sprintf(...['%s-%s', 'x', 'y']));
/// function h(...$v) { var_dump($v); }
/// h(...[1, 2], ...['k' => 3], z: 4);
/// ```
/// ```expect
/// 1 2 0|3
/// 1 2 x|3
/// A B z|Z
/// 1 2 |
/// int(9)
/// string(3) "x-y"
/// array(4) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   ["k"]=>
///   int(3)
///   ["z"]=>
///   int(4)
/// }
/// ```
///
/// ```php
/// function f($a, $b = 'B', ...$rest) {}
/// function named() { yield 'a' => 1; yield 2; }
/// function nulled() { yield null => 1; }
/// try { f(...['a' => 1, 2]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { f(...named()); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { f(...1); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { f(...nulled()); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { f(...[1], a: 2); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot use positional argument after named argument during unpacking
/// Cannot use positional argument after named argument during unpacking
/// TypeError: Only arrays and Traversables can be unpacked, int given
/// Keys must be of type int|string during argument unpacking
/// Named parameter $a overwrites previous argument
/// ```
///
/// A parameter taken by reference is bound to the array's element,
/// made a reference in place, the array separated first where the
/// variable shares it; a `Traversable`'s element has no place to bind,
/// and is passed by value with php's warning.
///
/// ```php
/// function r(&$x, &$y) { $x .= '!'; $y .= '?'; }
/// $a = ['p', 'q']; $c = $a;
/// r(...$a);
/// var_dump($a, $c);
/// class C { function m(&$x) { $x = 1; } }
/// (new C)->m(...new ArrayIterator(['s']));
/// r(...['x' => 'u', 'y' => 'v']);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(2) "p!"
///   [1]=>
///   string(2) "q?"
/// }
/// array(2) {
///   [0]=>
///   string(1) "p"
///   [1]=>
///   string(1) "q"
/// }
///
/// Warning: Cannot pass by-reference argument 1 of C::m() by unpacking a Traversable, passing by-value instead in %s on line 7
/// ```
pub(crate) fn send_unpack<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let held = exec::operand_undef(req, frame, code, op.op1_type, op.op1);
    let spread = held.deref();
    let sent = if let Some(array) = spread.as_array().copied() {
        send_unpacked_array(req, frame, code, op, array)
    } else if let Some(object) = spread.as_object().copied() {
        match unpack_iterator(req, object) {
            Ok(wrapper) => {
                let sent = send_unpacked_iterator(req, wrapper);
                req.release(wrapper.as_value());
                sent
            }
            Err(thrown) => Ok(Err(thrown)),
        }
    } else {
        if spread.tag() == TypeTag::Undef {
            exec::operand(req, frame, code, op.op1_type, op.op1);
        }
        let given = crate::ops::value_name(req, &spread.undef_as_null());
        Ok(Err(crate::api::raising::throw_error(
            req,
            "TypeError",
            &format!("Only arrays and Traversables can be unpacked, {given} given"),
        )))
    };
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    match sent? {
        Ok(()) => Ok(advance(req, frame)),
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// [`send_unpack`]'s array arm. Where the operand is a variable and an
/// element goes to a parameter taken by reference, the array is
/// separated in the variable first (`SEPARATE_ARRAY`), and each such
/// element made a reference in place (`ZVAL_MAKE_REF_EX(arg, 2)`); an
/// array that is no variable's gives such a parameter a new reference
/// over a copy of the element.
fn send_unpacked_array(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    array: PhpArray,
) -> Result<Result<(), Thrown>, Fatal> {
    let variable = matches!(op.op1_type, OperandKind::Cv | OperandKind::Var);
    let mut array = array;
    if variable && unpack_binds(req, array) {
        let Some(place) = write_place(req, frame, code, op.op1_type, op.op1) else {
            return Err(Fatal::error(
                "InternalError: SEND_UNPACK over a variable with no place",
            ));
        };
        match separated_array(req, place) {
            Ok((_, Some(separated))) => array = separated,
            Ok((_, None)) => {}
            Err(thrown) => return Ok(Err(thrown)),
        }
    }
    let mut named = false;
    // A fatal ends the walk as its break and is handed on after it.
    let fatal = |fatal: Fatal| -> walk::Step<Fatal> { Ok(ControlFlow::Break(fatal)) };
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(ht, name, arg)`
    // (`Z/Zend/zend_vm_def.h:5309`), no owner of the table taken: the
    // operand holds it, separated first where an element is bound.
    let walked = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step<Fatal> {
        let key = row.key().clone();
        let element = *row.value();
        let reserved = match key {
            crate::api::values::Key::Str(name) => {
                named = true;
                match reserve_named(req, name) {
                    Ok(Ok(reserved)) => reserved,
                    Ok(Err(thrown)) => return Err(thrown),
                    Err(ended) => return fatal(ended),
                }
            }
            crate::api::values::Key::Int(_) if named => {
                return Err(positional_after_named(req));
            }
            crate::api::values::Key::Int(_) => match reserve_next(req) {
                Ok(reserved) => reserved,
                Err(ended) => return fatal(ended),
            },
        };
        let value = if !should_be_sent_by_ref(req, reserved.number) {
            let value = element.deref();
            req.addref(&value);
            value
        } else if element.as_php_ref().is_some() {
            req.addref(&element);
            element
        } else if variable {
            let entry = req.array_lookup(array.heap(), key.heap())?;
            bound_at(req, entry)?
        } else {
            req.addref(&element);
            req.reference(element)?.as_value()
        };
        match put_argument(req, reserved.slot, value) {
            Ok(()) => Ok(ControlFlow::Continue(())),
            Err(ended) => fatal(ended),
        }
    });
    match walked {
        Ok(None) => {}
        Ok(Some(ended)) => return Err(ended),
        Err(thrown) => return Ok(Err(thrown)),
    }
    Ok(Ok(()))
}

/// Whether any element of `array` goes to a parameter of the call being
/// built that takes it by reference, which is when php separates the
/// array before it binds one.
fn unpack_binds(req: &Request, array: PhpArray) -> bool {
    let Some(function) = req.building_call_function() else {
        return false;
    };
    let mut next = req
        .building_call()
        .and_then(|call| req.vm().at(call))
        .map_or(0, |call| call.num_args as usize);
    crate::api::values::walk::rows(req, array).any(|(key, _)| {
        let number = match key {
            crate::api::values::Key::Str(name) => {
                crate::api::call::arg_offset_by_name(req, function, name.as_bytes()).number()
            }
            crate::api::values::Key::Int(_) => {
                next += 1;
                u32::try_from(next).ok()
            }
        };
        number.is_some_and(|number| should_be_sent_by_ref(req, number))
    })
}

/// [`send_unpack`]'s `Traversable` arm over the iterator `wrapper`: each
/// element placed by its key as the array arm places it, its reference
/// followed, and a parameter that must take a reference given a new one
/// with php's warning, since an iterator's element is no place to bind.
fn send_unpacked_iterator(
    req: &mut Request,
    wrapper: PhpObject,
) -> Result<Result<(), Thrown>, Fatal> {
    if let Err(thrown) = crate::interfaces::rewind(req, wrapper) {
        return Ok(Err(thrown));
    }
    let mut named = false;
    loop {
        let (value, key) = match crate::interfaces::delegated(req, wrapper) {
            Ok(Some(pair)) => pair,
            Ok(None) => return Ok(Ok(())),
            Err(thrown) => return Ok(Err(thrown)),
        };
        let value = exec::owned_deref(req, value);
        let name = match unpacked_key(req, key, "argument") {
            Ok(name) => name,
            Err(thrown) => {
                req.release(value);
                return Ok(Err(thrown));
            }
        };
        let reserved = match name {
            Some(name) => {
                named = true;
                let reserved = reserve_named(req, name);
                req.release(Value::string(name));
                match reserved {
                    Ok(Ok(reserved)) => Ok(reserved),
                    Ok(Err(thrown)) => Err(thrown),
                    Err(fatal) => {
                        req.release(value);
                        return Err(fatal);
                    }
                }
            }
            None if named => Err(positional_after_named(req)),
            None => match reserve_next(req) {
                Ok(reserved) => Ok(reserved),
                Err(fatal) => {
                    req.release(value);
                    return Err(fatal);
                }
            },
        };
        let reserved = match reserved {
            Ok(reserved) => reserved,
            Err(thrown) => {
                req.release(value);
                return Ok(Err(thrown));
            }
        };
        let value = if must_be_sent_by_ref(req, reserved.number) {
            let function = req.building_call_function();
            let callee =
                function.map_or_else(String::new, |function| req.qualified_name_of(function));
            crate::api::raising::warning(
                req,
                &format!(
                    "Cannot pass by-reference argument {} of {callee}() by unpacking a Traversable, passing by-value instead",
                    reserved.number
                ),
            );
            match req.reference(value) {
                Ok(cell) => cell.as_value(),
                Err(thrown) => return Ok(Err(thrown)),
            }
        } else {
            value
        };
        put_argument(req, reserved.slot, value)?;
    }
}

/// php's `Error` for an integer key after a string one in what
/// `SEND_UNPACK` spreads.
fn positional_after_named(req: &mut Request) -> Thrown {
    crate::api::raising::throw_error(
        req,
        "Error",
        "Cannot use positional argument after named argument during unpacking",
    )
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
        req.replace_slot(frame, slot, subject);
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
        req.replace_slot(frame, slot, subject);
        return fe_reset_walked(req, frame, code, op, started);
    }
    fe_reset_refused(req, frame, code, op, &subject)
}

/// The ending both resets share for a walk the heap's table of hash
/// iterators follows, over a property table or by reference: the
/// `VAR`'s `Z_FE_ITER`, a `VAR` operand freed (`FREE_OP1_IF_VAR`), and
/// the jump `op2` names where the walk has nothing to start at.
fn fe_reset_walked(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    started: Option<HashIter>,
) -> Edge {
    set_fe_iter(req, frame, exec::temporary_slot(code, op.out), started);
    if op.op1_type == OperandKind::Var {
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
    }
    if started.is_none() {
        return jump_to(op.op2);
    }
    Ok(advance(req, frame))
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
///
/// What the reset threw stands pending while the operand and the
/// iterator are given up, so a destructor that throws there chains it.
///
/// ```php
/// class D extends RecursiveArrayIterator { function __destruct() { throw new Exception("dtor"); } }
/// class R extends RecursiveIteratorIterator { function beginIteration(): void { throw new Exception("rewind"); } }
/// try { foreach (new R(new D([1])) as $v) {} } catch (Exception $e) { echo $e->getMessage(), " ", $e->getPrevious()?->getMessage(), "\n"; }
/// ```
/// ```expect
/// dtor rewind
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
    set_out(req, frame, code, op, held);
    let free = |req: &mut Request| exec::free_operand(req, frame, code, op.op1_type, op.op1);
    match reset {
        Ok((_, empty)) => {
            free(req);
            if empty {
                jump_to(op.op2)
            } else {
                Ok(advance(req, frame))
            }
        }
        // `FREE_OP1()` with `EG(exception)` set, so a destructor it
        // serves chains what the reset threw.
        Err(thrown) => {
            let thrown = req.free_pending(thrown, free);
            threw(req, frame, &thrown)
        }
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
    req.replace_slot(frame, slot, Value::undef());
    set_fe_iter(req, frame, slot, None);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    jump_to(op.op2)
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
                req.hooked_walk_end(walk.0);
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
        // `Z_OBJPROP_P(array)` at each fetch: a proxy that forwards is
        // walked over its instance's table.
        return walk.map_or(ForeachVar::Skipped, |walk| ForeachVar::Properties {
            object: req.lazy_reader(object),
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn fe_fetch_r<T: Tier, A: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_fe_fetch_r(req, frame, code, op, A::default()) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    fetched_element(req, frame, code, op)
}

/// The inline path of `ZEND_FE_FETCH_R` over an array into a CV
/// (`ZEND_FE_FETCH_R_SPEC_VAR_HANDLER`, `Z/Zend/zend_vm_def.h:7090-7140`):
/// the next live position from `Z_FE_POS`, the jump to
/// `extended_value` past the end, and otherwise the position moved past
/// it, the key into the temporary under `RETURN_VALUE_USED` and the
/// value, a reference followed, into the CV with one more owner. `None`,
/// with nothing touched, for an object or an iterator, a target that is
/// no CV, an element that is a symbol table's `IS_INDIRECT`, a CV that
/// holds an owner other than a string, a reference among them, whose
/// write is `zend_assign_to_variable`'s, and a temporary holding an
/// owner. A string the CV held is given up after the write, which
/// serves no code.
///
/// ```php
/// $a = [1, "s" => str_repeat("b", 2), 3 => [4]]; unset($a[0]); $a[] = 5;
/// foreach ($a as $k => $v) { echo $k, "=", is_array($v) ? "A" : $v, " "; }
/// $r = 7; $x = [&$r, 8]; foreach ($x as $v) echo $v; foreach ([] as $v) echo "never";
/// echo "\n";
/// ```
/// ```expect
/// s=bb 3=A 4=5 78
/// ```
#[inline(always)] // the handler's fast half, as php's is in the handler
#[allow(clippy::inline_always)]
fn inline_fe_fetch_r(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    a: impl Operand,
) -> Option<Edge> {
    if op.op2_type != OperandKind::Cv {
        return None;
    }
    let var = a.slot(code, op.op1)?;
    let target = op.op2 as usize;
    let out = exec::temporary_slot(code, op.out) as usize;
    let slots = req.slots_mut(frame);
    let held = *slots.get(var)?;
    let array = held.as_arr()?;
    let Some((found, key, value)) = array.live_from(held.fe_pos()) else {
        return Some(jump_to(op.extended_value));
    };
    let value = crate::operand::deref(value);
    let keyed = op.out_type != OperandKind::Unused;
    let old = *slots.get(target)?;
    if value.is_undef()
        || value.kind() == php_heap::ZvalType::Indirect
        || (old.is_refcounted() && old.kind() != php_heap::ZvalType::String)
        || (keyed && slots.get(out)?.is_refcounted())
    {
        return None;
    }
    let key = match key {
        php_heap::Key::Long(n) => Zval::long(n),
        php_heap::Key::Str(name) => Zval::string(name),
    };
    slots.get_mut(var)?.set_fe_pos(found.saturating_add(1));
    if keyed {
        *slots.get_mut(out)? = key;
    }
    *slots.get_mut(target)? = value;
    if keyed {
        key.addref();
    }
    value.addref();
    // What the CV held, a string at most, which serves no code as it goes.
    if old.is_refcounted() {
        req.release_nogc(Value::of_zval(old));
    }
    Some(Ok(advanced()))
}

/// `FE_FETCH_R` past its inline path: the next element of an array, an
/// object's properties or an iterator, by [`foreach_var`]'s decoding.
/// A call of its own, as [`fetched_obj`] is.
#[inline(never)]
fn fetched_element(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let slot = exec::temporary_slot(code, op.op1);
    let Some(at) = req.slot_ptr(frame, slot) else {
        return Err(Fatal::error(
            "InternalError: FE_FETCH_R over a VAR the frame does not have",
        ));
    };
    // A hooked property's value is the walk's own owner, given up once
    // the loop's variable holds its own, and so is an object's key.
    let mut owned = None;
    let mut owned_key = None;
    let (key, value) = match foreach_var(req, at.read()) {
        ForeachVar::Array { array, pos } => {
            let Some((found, key, value)) = array.live_from(pos) else {
                return jump_to(op.extended_value);
            };
            // `Z_FE_POS_P(array) = pos + 1`: the word alone, since a
            // value write is `ZVAL_COPY_VALUE` and leaves `u2` standing
            // (#491).
            at.set_fe_pos(found.saturating_add(1));
            (key, exec::value_of(value))
        }
        ForeachVar::Properties { object, walk } => match req.object_walk(object, walk.0, false) {
            Ok(Some((key, walked))) => {
                let key = match req.walked_key(key) {
                    Ok(key) => key,
                    Err(thrown) => return threw(req, frame, &thrown),
                };
                let value = Request::walked_value(object, walked);
                if walked.owned() {
                    owned = Some(value);
                }
                let view = key.heap();
                owned_key = Some(key);
                (view, value)
            }
            Ok(None) => {
                return jump_to(op.extended_value);
            }
            Err(thrown) => return threw(req, frame, &thrown),
        },
        ForeachVar::Iterator(iterator) => return fe_fetch_iterator(req, frame, code, op, iterator),
        ForeachVar::Reference { .. } | ForeachVar::Skipped => {
            return Err(Fatal::error(
                "InternalError: FE_FETCH_R over a VAR no by-value reset filled",
            ));
        }
    };
    if let Some(key) = fe_key_kept(req, op, key) {
        set_out(req, frame, code, op, key);
    }
    if let Some(key) = owned_key {
        key.give_up(req);
    }
    let assigned = fe_assign(req, frame, code, op, value);
    if let Some(owned) = owned {
        req.release(owned);
    }
    assigned
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
            return jump_to(op.extended_value);
        }
        Err(thrown) => return threw(req, frame, &thrown),
    };
    if let Some(key) = key {
        set_out(req, frame, code, op, key);
    }
    let assigned = fe_assign(req, frame, code, op, current);
    req.release(current);
    assigned
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
        req.replace_slot(frame, out, value);
        return Ok(advance(req, frame));
    }
    let value = value.deref();
    req.addref(&value);
    if let Some(r) = req.slot(frame, op.op2).as_php_ref() {
        req.reference_set(r.heap(), value);
    } else {
        req.replace_slot(frame, op.op2, value);
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
    let cell = match req.make_ref(place) {
        Ok(cell) => cell,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    req.addref(&cell.as_value());
    req.replace_slot(frame, slot, cell.as_value());
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
    fe_reset_walked(req, frame, code, op, started)
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
        req.replace_slot(frame, slot, cell.as_value());
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
        req.replace_slot(frame, slot, subject);
        let started = match req.object_walk_start(object) {
            Ok(started) => started.map(HashIter),
            Err(thrown) => return threw(req, frame, &thrown),
        };
        return fe_reset_walked(req, frame, code, op, started);
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
        return Err(Fatal::error(
            "InternalError: FE_FETCH_RW over a VAR the frame does not have",
        ));
    };
    let found = match foreach_var(req, at.read()) {
        ForeachVar::Reference { cell, walk } => {
            let subject = cell.as_value().deref();
            if let Some(array) = subject.as_array().copied() {
                fe_walk_array_rw(req, op, cell, array, walk)
            } else if let Some(object) = subject.as_object().copied() {
                // `Z_OBJPROP_P(array)` over what the variable holds now,
                // which initialises an object made lazy since the reset.
                match req.lazy_init(object) {
                    Ok(object) => fe_walk_object_rw(req, op, object, walk),
                    Err(thrown) => Ok(Err(thrown)),
                }
            } else {
                crate::api::raising::warning(
                    req,
                    &format!(
                        "foreach() argument must be of type array|object, {} given",
                        crate::ops::value_name(req, &subject.undef_as_null())
                    ),
                );
                if req.is_pending() {
                    return Ok(Stepped::Threw(frame).into());
                }
                Ok(Ok(None))
            }
        }
        ForeachVar::Properties { object, walk } => fe_walk_object_rw(req, op, object, walk),
        ForeachVar::Iterator(iterator) => fe_iterator_rw(req, iterator, op),
        ForeachVar::Array { .. } | ForeachVar::Skipped => {
            return Err(Fatal::error(
                "InternalError: FE_FETCH_RW over a VAR no by-reference reset filled",
            ));
        }
    };
    let BoundRw { key, cell, owned } = match found? {
        Ok(Some(found)) => found,
        Ok(None) => {
            return jump_to(op.extended_value);
        }
        Err(thrown) => return threw(req, frame, &thrown),
    };
    if let Some(key) = key {
        let out = exec::temporary_slot(code, op.out);
        req.replace_slot(frame, out, key);
    }
    let bound = cell.as_value();
    if op.op2_type == OperandKind::Cv {
        let same = req
            .slot(frame, op.op2)
            .as_php_ref()
            .is_some_and(|held| *held == cell);
        if !same {
            req.addref(&bound);
            req.replace_slot(frame, op.op2, bound);
        }
    } else {
        req.addref(&bound);
        let out = exec::temporary_slot(code, op.op2);
        req.replace_slot(frame, out, bound);
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

/// `FE_FETCH_RW` over the array a reference holds: one step of the
/// loop's hash iterator ([`walk::array_step`]), which separates an
/// array the body shared and moves the walk onto a table the body
/// replaced, finds the first live element from there and leaves the
/// iterator one past it; and the element made a reference in place
/// where it is not one (`ZVAL_NEW_EMPTY_REF`).
fn fe_walk_array_rw(
    req: &mut Request,
    op: &Opline,
    cell: PhpRef,
    array: PhpArray,
    walk: HashIter,
) -> FoundRw {
    let stepped = walk::array_step(req, cell, array, walk.0, walk::Advance::Past)?;
    let Some((walked, _, key, value)) = stepped else {
        return Ok(Ok(None));
    };
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
    let key = fe_key_kept(req, op, key);
    Ok(Ok(Some(BoundRw {
        key,
        cell: element,
        owned: false,
    })))
}

/// `FE_FETCH_RW` over an object walked by its properties: the next one
/// the scope may see ([`Request::object_walk`]), bound through
/// [`Request::walked_reference`], which gives a declared property's cell
/// its type or refuses a `readonly` one. Where the body put another
/// object in the loop's variable, the walk goes on in it from its
/// table's pointer, as `zend_hash_iterator_pos` moves the iterator, and
/// from an array's pointer as the array arm does:
///
/// ```php
/// class P { public $x = 1; public $y = 2; public $z = 3; }
/// $c = [1, 2]; foreach ($c as $k => &$v) { echo " $k=$v"; if ($k === 0) { $p = new P; @next($p); $c = $p; } } echo "\n";
/// $o = new P; $c = $o; foreach ($c as $k => &$v) { echo " $k=$v"; if ($k === 'x') { $q = new P; @next($q); @next($q); $c = $q; } } echo "\n";
/// ```
/// ```expect
///  0=1 y=2 z=3
///  x=1 z=3
/// ```
fn fe_walk_object_rw(req: &mut Request, op: &Opline, object: PhpObject, walk: HashIter) -> FoundRw {
    let (key, walked) = match req.object_walk(object, walk.0, true) {
        Ok(Some(walked)) => walked,
        Ok(None) => return Ok(Ok(None)),
        Err(thrown) => return Ok(Err(thrown)),
    };
    // A `&get` hook's answer is the walk's own owner of the cell, as an
    // iterator's element is.
    let owned = walked.owned();
    let cell = match req.walked_reference(object, walked) {
        Ok(cell) => cell,
        Err(thrown) => {
            key.give_up(req);
            return Ok(Err(thrown));
        }
    };
    let key = match req.walked_key(key) {
        Ok(key) => key,
        Err(thrown) => return Ok(Err(thrown)),
    };
    let kept = fe_key_kept(req, op, key.heap());
    key.give_up(req);
    Ok(Ok(Some(BoundRw {
        key: kept,
        cell,
        owned,
    })))
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
/// (`RETURN_VALUE_USED`), [`exec::key_value`]'s value, and none otherwise.
fn fe_key_kept(req: &mut Request, op: &Opline, key: php_heap::Key) -> Option<Value> {
    (op.out_type != OperandKind::Unused).then(|| exec::key_value(req, key))
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn add<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_arith(
        req,
        frame,
        code,
        op,
        Arith::Add,
        (A::default(), B::default()),
    ) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    binary(req, frame, code, op, crate::ops::add)
}

/// `ZEND_SUB`: [`crate::ops::sub`], the same way.
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn sub<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_arith(
        req,
        frame,
        code,
        op,
        Arith::Sub,
        (A::default(), B::default()),
    ) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    binary(req, frame, code, op, crate::ops::sub)
}

/// `ZEND_MUL`: [`crate::ops::mul`], the same way.
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn mul<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_arith(
        req,
        frame,
        code,
        op,
        Arith::Mul,
        (A::default(), B::default()),
    ) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
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
    binary_by_kind(req, frame, code, op, (Binary::Div, crate::ops::div))
}

/// `ZEND_MOD`: [`crate::ops::mod`], the same way; a divisor of zero
/// leaves php's `DivisionByZeroError` pending where the opline stands.
pub(crate) fn r#mod<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    binary_by_kind(req, frame, code, op, (Binary::Mod, crate::ops::r#mod))
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
    let kernel = (
        Binary::Shift(Shift::Left),
        crate::ops::shift_left as BinaryKernel,
    );
    binary_by_kind(req, frame, code, op, kernel)
}

/// `ZEND_SR`: [`crate::ops::shift_right`], the same way.
pub(crate) fn sr<T: Tier>(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let kernel = (
        Binary::Shift(Shift::Right),
        crate::ops::shift_right as BinaryKernel,
    );
    binary_by_kind(req, frame, code, op, kernel)
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn concat<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_concat(req, frame, code, op, at, (A::default(), B::default())) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    concat_into(req, frame, code, op)
}

/// The inline path of `ZEND_CONCAT` and `ZEND_FAST_CONCAT` over two
/// strings (`Z/Zend/zend_vm_def.h:382-431`, `:3293-3341`), the arm
/// [`concat_into`] takes for them read off the zvals as they stand: a
/// left temporary's string grown where it stands
/// ([`Request::string_append`]), any other left side joined into a new
/// string ([`crate::ops::join`]), the right side given up where it is a
/// temporary, and the string into the temporary. `None`, with nothing
/// touched, for anything but two strings, an empty side that is no
/// constant, which hands the other on, and a temporary holding an
/// owner. What the join refuses, the memory limit or php's
/// `String size overflow`, is raised at this opline, as the kernel's.
///
/// ```php
/// $a = "ab"; $b = "cd"; $e = "";
/// for ($i = 0; $i < 2; $i++) { echo $a . $b, "|", "x$a", "|", $a . "", "|", $a . $e, "|", strtoupper($a) . $b, "\n"; }
/// ```
/// ```expect
/// abcd|xab|ab|ab|ABcd
/// abcd|xab|ab|ab|ABcd
/// ```
#[inline(always)] // the handler's fast half, as php's is in the handler
#[allow(clippy::inline_always)]
fn inline_concat(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
    (a, b): (impl Operand, impl Operand),
) -> Option<Edge> {
    let slots = req.slots_mut(frame);
    let (left, right) = (a.read(slots, code, op.op1), b.read(slots, code, op.op2));
    let (x, y) = (left.as_str()?, right.as_str()?);
    let out = exec::temporary_slot(code, op.out) as usize;
    if (a.kind() != OperandKind::Const && x.is_empty())
        || (b.kind() != OperandKind::Const && y.is_empty())
        || slots.get(out)?.is_refcounted()
    {
        return None;
    }
    let (x, y) = (PhpString::of(x), PhpString::of(y));
    let joined = if matches!(a.kind(), OperandKind::Tmp | OperandKind::Var) {
        // The temporary's owner is the append's now.
        *slots.get_mut(a.slot(code, op.op1)?)? = Zval::UNDEF;
        req.string_append(x, y)
    } else {
        crate::ops::join(req, x, y)
    };
    if matches!(b.kind(), OperandKind::Tmp | OperandKind::Var) {
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
    }
    Some(match joined {
        Ok(joined) => {
            if let Some(into) = req.slots_mut(frame).get_mut(out) {
                *into = Zval::string(joined.heap());
            }
            Ok(advanced())
        }
        Err(thrown) => {
            req.vm_mut().stand(frame, at);
            threw(req, frame, &thrown)
        }
    })
}

/// The body `CONCAT` and `FAST_CONCAT` share, php's arms for two
/// strings (`Z/Zend/zend_vm_def.h:382-431`, `:3293-3341`). An empty side
/// hands the other on only where it is no `CONST`: the compiler's `''`
/// is joined as any string is, so `$x . ''` is a new string and not
/// `$x`'s. A left temporary's owner goes to [`Request::string_append`],
/// which grows the string where it stands when that owner is its only
/// one and copies it otherwise, as `zend_string_extend` and php's alloc
/// arm beside it do; any other left side is joined into a new string
/// ([`crate::ops::join`]). Every other pair, the empty side's included,
/// is [`crate::ops::concat`] over the two operands as [`binary`] reads
/// them, whose empty side hands the other on as php's does.
///
/// A string an array holds, joined with a constant `''` on either side,
/// is a string of its own, and so is a variable's; a variable's `''` is
/// the empty side that hands the other on, and a temporary that was the
/// string's one owner grows into the joined string where it stands:
///
/// ```php
/// $a = [str_repeat('a', 1)]; $x = str_repeat('xy', 2); $e = '';
/// debug_zval_dump($a[0] . '', '' . $a[0], $x . '', '' . $x, $x . $e, $e . $x, str_repeat('ab', 2) . '');
/// ```
/// ```expect
/// string(1) "a" refcount(1)
/// string(1) "a" refcount(1)
/// string(4) "xyxy" refcount(1)
/// string(4) "xyxy" refcount(1)
/// string(4) "xyxy" refcount(3)
/// string(4) "xyxy" refcount(3)
/// string(4) "abab" refcount(1)
/// ```
///
/// Each operand is read once, so an undefined variable warns once on
/// either side:
///
/// ```php
/// echo $u . '|', '|' . $v, "\n";
/// ```
/// ```expect
/// Warning: Undefined variable $u in %s on line 2
/// |
/// Warning: Undefined variable $v in %s on line 2
/// |
/// ```
#[inline(never)]
fn concat_into(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    // Each operand is read once: a `CV` that is undefined warns where
    // it is read.
    let left = exec::operand(req, frame, code, op.op1_type, op.op1);
    let right = exec::operand(req, frame, code, op.op2_type, op.op2);
    let computed = match (left.as_php_string(), right.as_php_string()) {
        (Some(&a), Some(&b))
            if (op.op1_type == OperandKind::Const || !a.is_empty())
                && (op.op2_type == OperandKind::Const || !b.is_empty()) =>
        {
            if matches!(op.op1_type, OperandKind::Tmp | OperandKind::Var) {
                // The temporary's owner is the append's now, and nothing
                // releases it again.
                req.take_slot(frame, exec::temporary_slot(code, op.op1));
                req.string_append(a, b)
            } else {
                // A `CV` or a `CONST` is no operand to free.
                crate::ops::join(req, a, b)
            }
            .map(Value::string)
        }
        _ => {
            let computed = crate::ops::concat(req, &left, &right);
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            computed
        }
    };
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    computed_into(req, frame, code, op, computed)
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn fast_concat<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_concat(req, frame, code, op, at, (A::default(), B::default())) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    concat_into(req, frame, code, op)
}

/// `ZEND_ROPE_INIT` (`Z/Zend/zend_vm_def.h:3410`): the first piece of
/// an interpolated string of three or more, converted as
/// `zval_get_string_func` converts it ([`crate::ops::to_string`]), as
/// the first piece of a rope the `result` temporary names, with
/// `extended_value` the number of pieces the rope will carry. php lays
/// the rope's `zend_string *` pointers over the stretch of slots the
/// compiler reserved, two to a zval; a zval here holds one value, so
/// the temporary holds the rope's position among the request's
/// ([`crate::api::request::ropes`]) and the other reserved slots stay
/// `UNDEF`. What a program sees is the same: the pieces converted in
/// order, each warning and `__toString` raised where php raises it, one
/// string of their joined length at the `ROPE_END`. A piece whose
/// conversion threw leaves the throwable pending
/// (`ZEND_VM_NEXT_OPCODE_CHECK_EXCEPTION`). A string piece is taken
/// inline at each operand kind, as php's specialised handlers take it,
/// and any other piece is converted after the opline is stood at.
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn rope_init<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    let rope = req.rope_open();
    set_out(req, frame, code, op, rope.value());
    if inline_rope_piece(req, frame, code, op, rope, B::default()) {
        return Ok(advanced());
    }
    req.vm_mut().stand(frame, at);
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn rope_add<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    let rope = rope_of(req, frame, code, op)?;
    if inline_rope_piece(req, frame, code, op, rope, B::default()) {
        return Ok(advanced());
    }
    req.vm_mut().stand(frame, at);
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
pub(crate) fn rope_end<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    let rope = rope_of(req, frame, code, op)?;
    let rope_slot = exec::temporary_slot(code, op.op1);
    req.vm_mut().stand(frame, at);
    let converted = if inline_rope_piece(req, frame, code, op, rope, B::default()) {
        Ok(())
    } else {
        let piece = exec::operand(req, frame, code, op.op2_type, op.op2);
        let converted = crate::ops::to_string(req, &piece);
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
        converted.map(|text| {
            req.rope_push(rope, text);
        })
    };
    let joined = match converted {
        Ok(()) => req.rope_end(rope),
        Err(thrown) => {
            req.rope_close(rope);
            Err(thrown)
        }
    };
    req.take_slot(frame, rope_slot);
    match joined {
        Ok(joined) => {
            set_out(req, frame, code, op, Value::string(joined));
            Ok(advance(req, frame))
        }
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// The rope the opline's `op1` temporary names, the position
/// `ROPE_INIT` put there; a slot holding anything else is an invariant
/// the compiler's rope emission keeps, so it is an internal error.
fn rope_of(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Result<RopeAt, Fatal> {
    let slot = exec::temporary_slot(code, op.op1);
    req.slot(frame, slot)
        .as_int()
        .and_then(RopeAt::of)
        .ok_or_else(|| {
            Fatal::error(format!(
                "InternalError: {} over a temporary holding no rope",
                op.opcode.name()
            ))
        })
}

/// A string piece taken as php's `ROPE_*` handlers take one, with no
/// conversion: a temporary's owner moved onto the rope and its slot
/// emptied, a constant's or a CV's string with one more owner
/// (`Z_ADDREF_P` where it is refcounted, `zend_string_copy`); `false`
/// for any other piece, which [`rope_piece`] converts.
#[inline(always)] // design §5.8: the string arm is the handler's body
#[allow(clippy::inline_always)]
fn inline_rope_piece(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    rope: RopeAt,
    b: impl Operand,
) -> bool {
    let slots = req.slots_mut(frame);
    let piece = b.read(slots, code, op.op2);
    let Some(text) = piece.as_str() else {
        return false;
    };
    if matches!(b.kind(), OperandKind::Tmp | OperandKind::Var) {
        if let Some(held) = b.slot(code, op.op2).and_then(|at| slots.get_mut(at)) {
            *held = Zval::UNDEF;
        }
    } else if piece.is_refcounted() {
        req.addref(&Value::of_zval(piece));
    }
    req.rope_push(rope, PhpString::of(text));
    true
}

/// One piece of a rope: the `op2` operand converted as
/// `zval_get_string_func` converts it, the operand freed (`FREE_OP2`),
/// and the string pushed onto the rope; a conversion that threw leaves
/// the throwable pending, which is the `Threw` edge the loop reads, and
/// the pieces so far to the live range's cleanup.
fn rope_piece(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    rope: RopeAt,
) -> Edge {
    let piece = exec::operand(req, frame, code, op.op2_type, op.op2);
    let converted = crate::ops::to_string(req, &piece);
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    match converted {
        Ok(text) => {
            req.rope_push(rope, text);
            Ok(advance(req, frame))
        }
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// `ZEND_IS_SMALLER`: [`crate::ops::is_smaller`] over the two operands,
/// into the temporary the opline's `result` field names. php's handler
/// takes the long pair and the double pairs before it calls
/// `zend_compare`, and those are the kernel's first arms.
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn is_smaller<T: Tier, A: Operand + Default, B: Operand + Default, S: Branch>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_compare::<T, S>(
        req,
        frame,
        code,
        op,
        at,
        Ordering::is_lt,
        (A::default(), B::default()),
    ) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    compared_branch::<T, S>(req, frame, code, op, crate::ops::is_smaller)
}

/// `ZEND_IS_SMALLER_OR_EQUAL`: [`crate::ops::is_smaller_or_equal`], the
/// same way, and the opcode `>=` is with its operands swapped.
pub(crate) fn is_smaller_or_equal<T: Tier, S: Branch>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let kinds = (ByKind(op.op1_type), ByKind(op.op2_type));
    if let Some(edge) = inline_compare::<T, S>(
        req,
        frame,
        code,
        op,
        req.vm().opline(frame),
        Ordering::is_le,
        kinds,
    ) {
        return edge;
    }
    compared_branch::<T, S>(req, frame, code, op, crate::ops::is_smaller_or_equal)
}

/// `ZEND_IS_EQUAL`: [`crate::ops::is_equal`], the same way. Each
/// comparison branches over a `JMPZ` or a `JMPNZ` after it
/// ([`smart_branch`]).
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn is_equal<T: Tier, A: Operand + Default, B: Operand + Default, S: Branch>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_compare::<T, S>(
        req,
        frame,
        code,
        op,
        at,
        Ordering::is_eq,
        (A::default(), B::default()),
    ) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    compared_branch::<T, S>(req, frame, code, op, crate::ops::is_equal)
}

/// `ZEND_IS_NOT_EQUAL`: [`crate::ops::is_not_equal`], the same way.
pub(crate) fn is_not_equal<T: Tier, S: Branch>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let kinds = (ByKind(op.op1_type), ByKind(op.op2_type));
    if let Some(edge) = inline_compare::<T, S>(
        req,
        frame,
        code,
        op,
        req.vm().opline(frame),
        Ordering::is_ne,
        kinds,
    ) {
        return edge;
    }
    compared_branch::<T, S>(req, frame, code, op, crate::ops::is_not_equal)
}

/// `ZEND_IS_IDENTICAL`: [`crate::ops::is_identical`], the same way, both
/// operands dereferenced (`GET_OP*_ZVAL_PTR_DEREF`), which the kernel
/// does.
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn is_identical<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) =
        inline_identical::<T, false>(req, frame, code, op, at, (A::default(), B::default()))
    {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    compared_branch::<T, branch::Peeked>(req, frame, code, op, crate::ops::is_identical)
}

/// The inline path of `ZEND_IS_IDENTICAL` (`ZEND_IS_IDENTICAL_SPEC_*`,
/// `Z/Zend/zend_vm_def.h:477-497`) and, the answer the other way round
/// (`NOT`), of `ZEND_IS_NOT_IDENTICAL`: both operands, a reference followed
/// (`GET_OP*_ZVAL_PTR_DEREF`), compared as [`inline::identical`]
/// compares them, the answer branched on where a `JMPZ` or a `JMPNZ`
/// is fused to it ([`branch::Peeked`]) and written into the temporary
/// otherwise. `None`, with nothing touched, for a pair the kernel
/// compares, a temporary operand that holds an owner, which the opcode
/// would give up (`FREE_OP1`, `FREE_OP2`), and a temporary for the answer
/// holding an owner.
///
/// ```php
/// $a = 1; $b = "x"; $c = [1]; $o = new stdClass;
/// for ($i = 0; $i < 3; $i++) { if ($i === 1) echo "one"; var_dump($a === $i, $b === "x", $c === [1], $o === $o); }
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// onebool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
#[inline(always)] // as `inline_arith`
#[allow(clippy::inline_always)]
fn inline_identical<T: Tier, const NOT: bool>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
    (a, b): (impl Operand, impl Operand),
) -> Option<Edge> {
    let slots = req.slots_mut(frame);
    let side = |kind: OperandKind, z: Zval| {
        if z.is_refcounted() && matches!(kind, OperandKind::Tmp | OperandKind::Var) {
            return None;
        }
        Some(crate::operand::deref(z))
    };
    let x = side(a.kind(), a.read(slots, code, op.op1))?;
    let y = side(b.kind(), b.read(slots, code, op.op2))?;
    let answer = inline::identical(x, y)? != NOT;
    if let Some(jump) = branch::Peeked::fused(code, op, at) {
        return Some(branch_fused::<T>(
            req,
            frame,
            at.saturating_add(1),
            jump,
            answer,
        ));
    }
    write_owning_nothing(slots, code, op, Zval::bool(answer))?;
    Some(Ok(advanced()))
}

/// `ZEND_IS_NOT_IDENTICAL`: [`crate::ops::is_not_identical`], the same
/// way, its inline path [`inline_identical`]'s with the answer negated,
/// read through [`ByKind`]. A variable nothing wrote is the kernel's,
/// and its warning with it.
///
/// ```php
/// $a = 1; $f = 1.0; $s = "x"; $o = new stdClass; $r = &$a;
/// for ($i = 0; $i < 3; $i++) { if ($i !== 1) echo $i; }
/// var_dump($a !== $f, $s !== "x", $r !== 1, $o !== $o, $o !== new stdClass, [1] !== [1]);
/// var_dump($u !== null);
/// if ($v !== 1) echo "v\n";
/// ```
/// ```expect
/// 02bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
///
/// Warning: Undefined variable $u in %s on line 5
/// bool(false)
///
/// Warning: Undefined variable $v in %s on line 6
/// v
/// ```
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn is_not_identical<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let at = req.vm().opline(frame);
    let kinds = (ByKind(op.op1_type), ByKind(op.op2_type));
    if let Some(edge) = inline_identical::<T, true>(req, frame, code, op, at, kinds) {
        return edge;
    }
    compared_branch::<T, branch::Peeked>(req, frame, code, op, crate::ops::is_not_identical)
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn pre_inc<T: Tier, A: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_incdec(req, frame, code, op, (Arith::Add, false), A::default()) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
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
    let step = (Arith::Add, true);
    if let Some(edge) = inline_incdec(req, frame, code, op, step, ByKind(op.op1_type)) {
        return edge;
    }
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
#[cfg_attr(not(debug_assertions), inline(always))] // design §5.8, as `pre_inc`
#[cfg_attr(debug_assertions, inline(never))] // a debug `serve_frame` holds every inlined arm's locals
#[allow(clippy::inline_always)]
pub(crate) fn pre_dec<T: Tier, A: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_incdec(req, frame, code, op, (Arith::Sub, false), A::default()) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
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
    let step = (Arith::Sub, true);
    if let Some(edge) = inline_incdec(req, frame, code, op, step, ByKind(op.op1_type)) {
        return edge;
    }
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
    if let Some(edge) = inline_strlen(req, frame, code, op, ByKind(op.op1_type)) {
        return edge;
    }
    unary(req, frame, code, op, crate::ops::strlen)
}

/// The inline path of `ZEND_STRLEN` (`Z/Zend/zend_vm_def.h:8781`): the
/// length [`inline::string_length`] reads of a string as it stands,
/// into the temporary. `None`, with nothing touched, for a temporary
/// holding the string's owner, which the opcode gives up (`FREE_OP1`),
/// for everything the rule leaves to the kernel, and for a temporary
/// for the answer holding an owner.
#[inline(always)] // as `inline_arith`
#[allow(clippy::inline_always)]
fn inline_strlen(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    a: impl Operand,
) -> Option<Edge> {
    let slots = req.slots_mut(frame);
    let z = a.read(slots, code, op.op1);
    if matches!(a.kind(), OperandKind::Tmp | OperandKind::Var) && z.is_refcounted() {
        return None;
    }
    let length = inline::string_length(z)?;
    write_owning_nothing(slots, code, op, Zval::long(length))?;
    Some(Ok(advanced()))
}

/// `ZEND_TYPE_CHECK`: [`crate::ops::type_check`] over the operand with
/// the mask in `extended_value`, into the temporary; the operand freed
/// after it, or branched on ([`smart_branch`]).
pub(crate) fn type_check<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let at = req.vm().opline(frame);
    if let Some(edge) = inline_type_check::<T>(req, frame, code, op, at, ByKind(op.op1_type)) {
        return edge;
    }
    let mask = op.extended_value;
    let computed = unary_kernel(req, frame, code, op, |_, value| {
        Ok(Value::bool(crate::ops::type_check(value, mask)))
    });
    smart_branch::<T, branch::Peeked>(req, frame, code, op, computed)
}

/// The inline path of `ZEND_TYPE_CHECK` (`Z/Zend/zend_vm_def.h:8821`):
/// [`inline::type_check`]'s answer over the operand as it stands,
/// branched on where a `JMPZ` or a `JMPNZ` is fused to it
/// ([`branch::Peeked`]) and written into the temporary otherwise. A
/// temporary holding an owner, which `FREE_OP1` gives up, is given up
/// here where another owner stands, as the `binary_trees` kernel's `$node[0] === null`
/// over a node built as the program goes gives up its element's copy; its last
/// owner, whose release frees it, is the kernel's. `None`, with nothing
/// touched, for everything the rule leaves to the kernel and a
/// temporary for the answer holding an owner of something else.
#[inline(always)] // as `inline_arith`
#[allow(clippy::inline_always)]
fn inline_type_check<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
    a: impl Operand,
) -> Option<Edge> {
    let slots = req.slots_mut(frame);
    let z = a.read(slots, code, op.op1);
    let answer = inline::type_check(z, op.extended_value)?;
    let fused = branch::Peeked::fused(code, op, at);
    let owned = matches!(a.kind(), OperandKind::Tmp | OperandKind::Var) && z.is_refcounted();
    if owned {
        let operand = a.slot(code, op.op1)?;
        let out = exec::temporary_slot(code, op.out) as usize;
        if fused.is_none() && out != operand && slots.get(out)?.is_refcounted() {
            return None;
        }
        if !php_heap::Heap::release_nogc_kept(z) {
            return None;
        }
        *slots.get_mut(operand)? = Zval::UNDEF;
    }
    if let Some(jump) = fused {
        return Some(branch_fused::<T>(
            req,
            frame,
            at.saturating_add(1),
            jump,
            answer,
        ));
    }
    write_owning_nothing(slots, code, op, Zval::bool(answer))?;
    Some(Ok(advanced()))
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
        return Err(Fatal::error(format!(
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
    if let Some(edge) = inline_truth::<false>(req, frame, code, op, ByKind(op.op1_type)) {
        return edge;
    }
    unary(req, frame, code, op, |req, value| {
        crate::ops::is_true(req, value).map(Value::bool)
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
    let kernel = (
        Binary::Bitwise(Bitwise::Or),
        crate::ops::bitwise_or as BinaryKernel,
    );
    binary_by_kind(req, frame, code, op, kernel)
}

/// `ZEND_BW_XOR` (`Z/Zend/zend_vm_def.h:902`), what `^` compiles to:
/// [`crate::ops::bitwise_xor`] the same way.
pub(crate) fn bw_xor<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let kernel = (
        Binary::Bitwise(Bitwise::Xor),
        crate::ops::bitwise_xor as BinaryKernel,
    );
    binary_by_kind(req, frame, code, op, kernel)
}

/// `ZEND_BW_AND` (`Z/Zend/zend_vm_def.h:863`), what `&` compiles to:
/// [`crate::ops::bitwise_and`] the same way.
pub(crate) fn bw_and<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let kernel = (
        Binary::Bitwise(Bitwise::And),
        crate::ops::bitwise_and as BinaryKernel,
    );
    binary_by_kind(req, frame, code, op, kernel)
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
    if let Some(edge) = inline_truth::<true>(req, frame, code, op, ByKind(op.op1_type)) {
        return edge;
    }
    unary(req, frame, code, op, |req, value| {
        Ok(Value::bool(!crate::ops::is_true(req, value)?))
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
            crate::ops::is_true(req, left)? != crate::ops::is_true(req, right)?,
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
/// that can warn, over a NAN. The answer is branched on as every
/// `ISSET_ISEMPTY_*`'s is ([`smart_branch`]).
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
        Ok(crate::ops::is_set(&value))
    } else {
        crate::ops::is_true(req, &value).map(|truth| !truth)
    };
    smart_branch::<T, branch::Peeked>(req, frame, code, op, answer.map(Value::bool))
}

/// `ZEND_ISSET_ISEMPTY_THIS` (`Z/Zend/zend_vm_def.h:9239`),
/// `isset($this)` and `empty($this)`: whether the frame has a receiver,
/// the other way round under `ZEND_ISEMPTY`.
///
/// ```php
/// class C { function m() { return [isset($this), empty($this)]; } static function s() { return isset($this); } }
/// var_dump((new C)->m(), C::s(), isset($this), empty($this));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   bool(true)
///   [1]=>
///   bool(false)
/// }
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
pub(crate) fn isset_isempty_this<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let has = req.vm().this(frame).is_some();
    let answer = has != (op.extended_value & ISEMPTY != 0);
    write_out(req, frame, code, op, &Value::bool(answer));
    Ok(advance(req, frame))
}

/// `ZEND_ISSET_ISEMPTY_STATIC_PROP` (`Z/Zend/zend_vm_def.h:7526`),
/// `isset(C::$s)` and `empty(C::$s)`: the class fetched as a `static`
/// property's is, a class nothing declares php's `Error` even here, and
/// the property read under `BP_VAR_IS`, which says nothing of one the
/// class has not or the scope may not reach
/// (`zend_std_get_static_property_with_info`); the answer is then
/// [`isset_isempty_cv`]'s over what was read.
///
/// ```php
/// class P { private static $p = 1; }
/// class C extends P { public static $s = 0; public static ?int $t; public static $r;
///     static function inside() { return isset(parent::$p); } }
/// $x = null; C::$r = &$x;
/// var_dump(isset(C::$s), empty(C::$s), isset(C::$t), isset(C::$r), isset(C::$none), C::inside());
/// try { isset(Nope::$s); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// Class "Nope" not found
/// ```
pub(crate) fn isset_isempty_static_prop<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let (class, name) = match static_member(req, frame, code, op) {
        Ok(member) => member,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let scope = req.executed_scope();
    let held = match req.static_property(class, &name, scope, PropertyRead::Silent) {
        Ok(held) => held,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    let answer = if op.extended_value & ISEMPTY == 0 {
        Ok(crate::ops::is_set(&held))
    } else {
        crate::ops::is_true(req, &held).map(|truth| !truth)
    };
    req.release(held);
    smart_branch::<T, branch::Peeked>(req, frame, code, op, answer.map(Value::bool))
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
///
/// `$this` as the container is `FETCH_THIS` into a temporary first, as
/// php compiles it, so the opcode reads only the kinds its spec lists,
/// in a method and outside one alike:
///
/// ```php
/// class T implements ArrayAccess {
///     public function offsetExists($offset): bool { echo "e"; return $offset === 0; }
///     public function offsetGet($offset): mixed { echo "g"; return [1 => 0]; }
///     public function offsetSet($offset, $value): void {}
///     public function offsetUnset($offset): void {}
///     public function t() { return [isset($this[0]), empty($this[0]), isset($this[0][1]), isset($this[1])]; }
/// }
/// var_dump((new T)->t());
/// try { var_dump(isset($this[0])); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// eegegearray(4) {
///   [0]=>
///   bool(true)
///   [1]=>
///   bool(false)
///   [2]=>
///   bool(true)
///   [3]=>
///   bool(false)
/// }
/// Using $this when not in object context
/// ```
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn isset_isempty_dim_obj<T: Tier, A: Operand + Default, B: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    let operands = (A::default(), B::default());
    if let Some(edge) = inline_isset_dim::<T>(req, frame, code, op, at, operands) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    tested_dim::<T>(req, frame, code, op)
}

/// The inline path of `ZEND_ISSET_ISEMPTY_DIM_OBJ` over an array
/// (`Z/Zend/zend_vm_def.h:7539-7570`, `isset_again`): the element at
/// a long or string key ([`inline::dim_key`], [`inline::element`])
/// read as `isset` reads it, set where it is anything but `null`, and
/// as `empty` reads it, its truth as [`inline::truth`] reads a scalar;
/// a key the array has not got is unset and empty. The answer is
/// branched on where a `JMPZ` or a `JMPNZ` is fused to it
/// ([`branch::Peeked`]) and written into the temporary otherwise. `None`,
/// with nothing touched, as [`inline_fetch_dim_r`]'s, for an `empty` of
/// an element whose truth is the kernel's, and a temporary holding an
/// owner.
///
/// ```php
/// $a = [0, null, "", "0", [], 1.5, "k" => "v"]; $k = "k"; $n = 1;
/// foreach ([0, 1, 2, 3, 4, 5, 6, "k"] as $i) echo isset($a[$i]) ? "s" : "u", empty($a[$i]) ? "e" : "f", " ";
/// var_dump(isset($a[$k]), isset($a[$n]), !empty($a[$k]));
/// if (isset($a[0])) echo "branch\n";
/// ```
/// ```expect
/// se ue se se se sf ue sf bool(true)
/// bool(false)
/// bool(true)
/// branch
/// ```
#[inline(always)] // as `inline_fetch_dim_r`
#[allow(clippy::inline_always)]
fn inline_isset_dim<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
    (a, b): (impl Operand, impl Operand),
) -> Option<Edge> {
    let slots = req.slots_mut(frame);
    let (array, (key, free_dim)) = inline_dim(slots, code, op, a, b)?;
    let found = inline::element(array, key);
    let answer = if op.extended_value & ISEMPTY == 0 {
        found.is_some_and(|z| z.kind() != php_heap::ZvalType::Null)
    } else {
        match found {
            None => true,
            Some(z) => !inline::truth(z)?,
        }
    };
    let out = exec::temporary_slot(code, op.out) as usize;
    let fused = branch::Peeked::fused(code, op, at);
    if fused.is_none() && slots.get(out)?.is_refcounted() {
        return None;
    }
    if free_dim {
        exec::free_operand(req, frame, code, op.op2_type, op.op2);
    }
    if let Some(jump) = fused {
        return Some(branch_fused::<T>(
            req,
            frame,
            at.saturating_add(1),
            jump,
            answer,
        ));
    }
    write_owning_nothing(req.slots_mut(frame), code, op, Zval::bool(answer))?;
    Some(Ok(advanced()))
}

/// `ISSET_ISEMPTY_DIM_OBJ` past its inline path: the container read as
/// `BP_VAR_IS` reads it and the test each kind of container takes. A
/// call of its own, as [`fetched_obj`] is.
#[inline(never)]
fn tested_dim<T: Tier>(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
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
    smart_branch::<T, branch::Peeked>(req, frame, code, op, answer.map(Value::bool))
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
/// The answer is branched on as every `ISSET_ISEMPTY_*`'s is
/// ([`smart_branch`]).
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
    let container = exec::operand_undef(req, frame, code, op.op1_type, op.op1).deref();
    let mut member = exec::Member::default();
    let answer = if let Some(object) = container.as_object().copied() {
        member.name(req, frame, code, op).and_then(|name| {
            let name = name.as_str_bytes().unwrap_or_default();
            let test = if empty {
                PropertyTest::NotEmpty
            } else {
                PropertyTest::Isset
            };
            let table = req.object_handlers(object.heap());
            table
                .has_property(req, object, name, test)
                .map(|held| held != empty)
        })
    } else {
        exec::Member::touch(req, frame, code, op);
        Ok(empty)
    };
    member.release(req, frame, code, op);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    smart_branch::<T, branch::Peeked>(req, frame, code, op, answer.map(Value::bool))
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
        ValueRef::Float(f) => Some(crate::ops::double_to_long_strict(req, f)),
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
    let computed = unary_kernel(req, frame, code, op, |req, value| {
        let name = value.as_str_bytes().unwrap_or_default();
        Ok(Value::bool(crate::ops::defined(req, name)))
    });
    smart_branch::<T, branch::Peeked>(req, frame, code, op, computed)
}

/// `ZEND_FUNC_GET_ARGS` (`Z/Zend/zend_vm_def.h:9639`), what php's
/// compiler makes of `func_get_args()` inside a function
/// (`zend_compile_func_get_args`): the arguments the frame was passed,
/// as its slots hold them now, each reference followed and a slot
/// nothing holds `null`, into the temporary; with a constant `op1`, the
/// ones after the first that many, the form php's compiler makes of
/// `array_slice(func_get_args(), n)` (`zend_compile_func_array_slice`).
///
/// ```php
/// function f($a, &$b, $c = 3, ...$rest) { $a = 9; $b = 'b'; return func_get_args(); }
/// $x = 1;
/// var_dump(f(1, $x), f(1, $x, 2, 4, 5));
/// $g = function () { return func_get_args(); };
/// var_dump($g());
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(9)
///   [1]=>
///   string(1) "b"
/// }
/// array(5) {
///   [0]=>
///   int(9)
///   [1]=>
///   string(1) "b"
///   [2]=>
///   int(2)
///   [3]=>
///   int(4)
///   [4]=>
///   int(5)
/// }
/// array(0) {
/// }
/// ```
///
/// With nothing to copy the temporary is the process's empty array,
/// `zend_empty_array`, which costs nothing:
///
/// ```php
/// function f() { return func_get_args(); }
/// function g($a) { return array_slice(func_get_args(), 1); }
/// $m = memory_get_usage(); $a = f(); $b = g(1);
/// var_dump(memory_get_usage() - $m);
/// debug_zval_dump($a, $b);
/// ```
/// ```expect
/// int(0)
/// array(0) interned {
/// }
/// array(0) interned {
/// }
/// ```
pub(crate) fn func_get_args<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let skip = if op.op1_type == OperandKind::Const {
        exec::constant(code, ConstIdx(op.op1))
            .as_int()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0)
    } else {
        0
    };
    let arguments = req.frame_arguments(frame);
    match crate::natives::copied_arguments(req, arguments, skip) {
        Ok(array) => {
            set_out(req, frame, code, op, Value::array(array.hand_on()));
            Ok(advance(req, frame))
        }
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// `ZEND_FUNC_NUM_ARGS` (`Z/Zend/zend_vm_def.h:9628`), what php's
/// compiler makes of `func_num_args()` inside a function
/// (`zend_compile_func_num_args`): `EX_NUM_ARGS()`, every argument the
/// frame was passed, the extra ones among them, into the temporary.
///
/// ```php
/// function f($a, $b = 2) { return func_num_args(); }
/// $g = function (...$r) { return func_num_args(); };
/// var_dump(f(1), f(1, 2, 3), $g(), $g(1, 2));
/// ```
/// ```expect
/// int(1)
/// int(3)
/// int(0)
/// int(2)
/// ```
pub(crate) fn func_num_args<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let passed = req.vm().at(frame).map_or(0, |state| state.num_args);
    set_out(req, frame, code, op, Value::int(i64::from(passed)));
    Ok(advance(req, frame))
}

/// `ZEND_GET_CLASS` (`Z/Zend/zend_vm_def.h:9557`), what php's compiler
/// makes of `get_class()` and `get_class($o)` (`zend_compile_func_get_class`):
/// with no operand, the name of the class the frame's op array was
/// written in, `EX(func)->common.scope`, behind the 8.3 deprecation,
/// and php's `Error` where there is none; with one, the name of the
/// object's class, a reference followed, and php's `TypeError` naming
/// what it is for anything else, a variable nothing wrote warned of
/// first. The name is the class's as declared.
///
/// ```php
/// class A { function f() { return [get_class(), get_class($this)]; } static function s() { return get_class(); } }
/// class B extends A {}
/// $b = new B; $r = &$b;
/// var_dump((new B)->f(), B::s(), get_class($r), get_class(new ArrayObject));
/// try { get_class(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { get_class(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { get_class($u); } catch (TypeError $e) { echo $e->getMessage(), " at ", count($e->getTrace()), "\n"; }
/// ```
/// ```expect
///
/// Deprecated: Calling get_class() without arguments is deprecated in %s on line 2
///
/// Deprecated: Calling get_class() without arguments is deprecated in %s on line 2
/// array(2) {
///   [0]=>
///   string(1) "A"
///   [1]=>
///   string(1) "B"
/// }
/// string(1) "A"
/// string(1) "B"
/// string(11) "ArrayObject"
/// get_class() without arguments must be called from within a class
/// get_class(): Argument #1 ($object) must be of type object, int given
///
/// Warning: Undefined variable $u in %s on line 8
/// get_class(): Argument #1 ($object) must be of type object, null given at 0
/// ```
pub(crate) fn get_class<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let class = if op.op1_type == OperandKind::Unused {
        let Some(scope) = req.frame_scope(frame) else {
            let thrown = crate::api::raising::throw_error(
                req,
                "Error",
                "get_class() without arguments must be called from within a class",
            );
            return threw(req, frame, &thrown);
        };
        crate::api::raising::deprecated(req, "Calling get_class() without arguments is deprecated");
        scope
    } else {
        let value = exec::operand(req, frame, code, op.op1_type, op.op1).deref();
        let class = value.as_object().map(PhpObject::class);
        let Some(class) = class else {
            let shown = crate::ops::value_name(req, &value);
            let thrown = crate::api::raising::throw_error(
                req,
                "TypeError",
                &format!(
                    "get_class(): Argument #1 ($object) must be of type object, {shown} given"
                ),
            );
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            return threw(req, frame, &thrown);
        };
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
        class
    };
    class_name_into(req, frame, code, op, class)
}

/// `ZEND_GET_CALLED_CLASS` (`Z/Zend/zend_vm_def.h:9603`), what php's
/// compiler makes of `get_called_class()`
/// (`zend_compile_func_get_called_class`): the name of the class
/// `static` names in the frame, the receiver's where there is one and
/// the called scope where there is not, and php's `Error` outside a
/// class.
///
/// ```php
/// class A { static function f() { return get_called_class(); } function g() { return get_called_class(); } }
/// class B extends A {}
/// var_dump(A::f(), B::f(), (new B)->g());
/// function h() { return get_called_class(); }
/// try { h(); } catch (Error $e) { echo $e->getMessage(), " at ", count($e->getTrace()), "\n"; }
/// ```
/// ```expect
/// string(1) "A"
/// string(1) "B"
/// string(1) "B"
/// get_called_class() must be called from within a class at 1
/// ```
pub(crate) fn get_called_class<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let called = req
        .vm()
        .this(frame)
        .map(|this| this.class())
        .or_else(|| req.vm().at(frame).and_then(exec::Header::called_scope));
    let Some(class) = called else {
        let thrown = crate::api::raising::throw_error(
            req,
            "Error",
            "get_called_class() must be called from within a class",
        );
        return threw(req, frame, &thrown);
    };
    class_name_into(req, frame, code, op, class)
}

/// The declared name of `class` as a string of the request's, into the
/// temporary the opline's `result` names, and on to the next opline:
/// the tail `ZEND_GET_CLASS` and `ZEND_GET_CALLED_CLASS` share,
/// `ZVAL_STR_COPY(EX_VAR(opline->result.var), ce->name)`.
fn class_name_into(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    class: crate::api::values::ClassId,
) -> Edge {
    let name = req.class_name_bytes(class);
    let name = match req.declared_string(&name) {
        Ok(name) => name,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    set_out(req, frame, code, op, name.into_value().hand_on());
    Ok(advance(req, frame))
}

/// `ZEND_GET_TYPE` (`Z/Zend/zend_vm_def.h:9623`), what php's compiler
/// makes of `gettype($v)` (`zend_compile_func_gettype`):
/// [`crate::ops::legacy_type`] over the operand, a variable nothing
/// wrote warned of and `NULL`, into the temporary; the operand freed
/// after it.
///
/// ```php
/// $f = fopen('php://memory', 'r'); $c = fopen('php://memory', 'r'); fclose($c);
/// $x = 1; $r = &$x;
/// echo gettype(null), " ", gettype(false), " ", gettype($r), " ", gettype(1.5), " ", gettype("s"), " ", gettype([]), " ", gettype(new stdClass), " ", gettype($f), " ", gettype($c), "\n";
/// echo gettype($u), "\n";
/// ```
/// ```expect
/// NULL boolean integer double string array object resource resource (closed)
///
/// Warning: Undefined variable $u in %s on line 5
/// NULL
/// ```
pub(crate) fn get_type<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    unary(req, frame, code, op, |req, value| {
        req.string(crate::ops::legacy_type(value))
            .map(Value::string)
    })
}

/// `ZEND_ARRAY_KEY_EXISTS` (`Z/Zend/zend_vm_def.h:9747`), what php's
/// compiler makes of `array_key_exists($k, $a)`
/// (`zend_compile_func_array_key_exists`): over an array, a reference
/// followed, [`crate::ops::array_key_exists`], with the key's
/// `Undefined variable` raised where it asks; over anything else
/// (`zend_array_key_exists_error`) the key and then the array warned of
/// where nothing wrote them, and php's `TypeError` naming what the array
/// is where neither warning threw. The answer into the temporary, the
/// array freed and then the key, or branched on ([`smart_branch`]).
///
/// ```php
/// $a = ['k' => null]; $r = &$a;
/// var_dump(array_key_exists('k', $r), array_key_exists('j', $a), array_key_exists($u, ['' => 1]));
/// if (array_key_exists('k', $a)) echo "branch\n";
/// try { array_key_exists('k', 'x'); } catch (TypeError $e) { echo $e->getMessage(), " at ", count($e->getTrace()), "\n"; }
/// try { array_key_exists($u, $v); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { array_key_exists('k', new ArrayObject(['k' => 1])); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Warning: Undefined variable $u in %s on line 3
/// bool(true)
/// bool(false)
/// bool(true)
/// branch
/// array_key_exists(): Argument #2 ($array) must be of type array, string given at 0
///
/// Warning: Undefined variable $u in %s on line 6
///
/// Warning: Undefined variable $v in %s on line 6
/// array_key_exists(): Argument #2 ($array) must be of type array, null given
/// array_key_exists(): Argument #2 ($array) must be of type array, ArrayObject given
/// ```
pub(crate) fn array_key_exists<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let key = exec::operand_undef(req, frame, code, op.op1_type, op.op1);
    let subject = exec::operand_undef(req, frame, code, op.op2_type, op.op2).deref();
    let answered = if let Some(array) = subject.as_array().copied() {
        crate::ops::array_key_exists(req, array, &key, |req| {
            exec::operand(req, frame, code, op.op1_type, op.op1);
        })
    } else {
        if key.tag() == TypeTag::Undef {
            exec::operand(req, frame, code, op.op1_type, op.op1);
        }
        if subject.tag() == TypeTag::Undef {
            exec::operand(req, frame, code, op.op2_type, op.op2);
        }
        if !req.is_pending() {
            let shown = crate::ops::value_name(req, &subject);
            let thrown = crate::api::raising::throw_error(
                req,
                "TypeError",
                &format!(
                    "array_key_exists(): Argument #2 ($array) must be of type array, {shown} given"
                ),
            );
            req.leave_pending(&thrown);
        }
        Ok(false)
    };
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    smart_branch::<T, branch::Peeked>(req, frame, code, op, answered.map(Value::bool))
}

/// `ZEND_COUNT`: [`crate::ops::count`] over the operand, `sizeof`'s
/// bit in `extended_value`, into the temporary; the operand freed after
/// it.
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn count<T: Tier, A: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_count(req, frame, code, op, A::default()) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    counted(req, frame, code, op)
}

/// The inline path of `ZEND_COUNT` over an array
/// (`Z/Zend/zend_vm_def.h:9178-9182`): `zend_hash_num_elements` into
/// the temporary. `None`, with nothing touched, for an operand that is
/// no array, one a temporary holds that counts owners, which the count
/// would give up, a symbol table holding emptied `IS_INDIRECT` slots,
/// which `zend_array_count` walks, and a temporary holding an owner.
///
/// ```php
/// $a = [1, 2, [3, 4]]; $r = &$a; $e = [];
/// var_dump(count($a), count($r), count($e), count($a[2]), sizeof($a), count([1, 2]));
/// function f() { $x = 1; $y = 2; return count(get_defined_vars()); }
/// var_dump(f());
/// ```
/// ```expect
/// int(3)
/// int(3)
/// int(0)
/// int(2)
/// int(3)
/// int(2)
/// int(2)
/// ```
#[inline(always)] // as `inline_fetch_dim_r`
#[allow(clippy::inline_always)]
fn inline_count(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    a: impl Operand,
) -> Option<Edge> {
    let slots = req.slots_mut(frame);
    let z = a.read(slots, code, op.op1);
    let z = match a.kind() {
        OperandKind::Cv => crate::operand::deref(z),
        _ if z.is_refcounted() => return None,
        _ => z,
    };
    let array = z.as_arr().filter(|array| !array.has_empty_ind())?;
    let n = i64::try_from(array.len()).ok()?;
    write_owning_nothing(slots, code, op, Zval::long(n))?;
    Some(Ok(advanced()))
}

/// `COUNT` past its inline path: [`crate::ops::count`] over the
/// operand. A call of its own, as [`fetched_obj`] is.
#[inline(never)]
fn counted(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
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
    let at = req.vm().opline(frame);
    jumped::<T>(req, frame, at, op.op1, op.out)
}

/// `ZEND_VM_JMP` from the opline at `at` to `target`, the frame left at
/// the target; a target at or before `at` is the back-edge, where the
/// interrupt is tested and then the loop safepoint raised with
/// `number`, the one the compiler wrote into the jump's `result`.
#[inline(always)] // the back-edge of every loop, in the loop
#[allow(clippy::inline_always)]
fn jumped<T: Tier>(req: &mut Request, frame: FrameRef, at: u32, target: u32, number: u32) -> Edge {
    if target <= at {
        req.vm_mut().stand(frame, target);
        if crate::interrupt::check(req)? {
            return Ok(Stepped::Threw(frame).into());
        }
        match T::safepoint(req, frame, Site::Loop(SafepointIdx(number))) {
            Flow::Continue => {}
            // The tier ran the jump and on past it (`jit-design.md` §2.2).
            flow => return crate::tier::resumed(req, flow, frame),
        }
    }
    Ok(Next::jump(target))
}

/// `ZEND_JMPNZ`: the jump `zend_emit_cond_jump` leaves at the end of a
/// `for`, a `while` and a `do`, its target opline in `op2`, taken when
/// the operand is true as [`crate::ops::is_true`] reads it. Taken to an
/// earlier opline it is the loop's back-edge and does what [`jmp`] does
/// there: the frame at the target, the interrupt check, the site with
/// the number the compiler wrote in `result`. Not taken, it is
/// `ZEND_VM_NEXT_OPCODE`.
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn jmpnz<T: Tier, A: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_branch::<T, true, false>(req, frame, code, op, at, A::default()) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    branched::<T, true, false>(req, frame, code, op)
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn jmpz<T: Tier, A: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_branch::<T, false, false>(req, frame, code, op, at, A::default()) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    branched::<T, false, false>(req, frame, code, op)
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
    if let Some(edge) = inline_branch::<T, false, true>(
        req,
        frame,
        code,
        op,
        req.vm().opline(frame),
        ByKind(op.op1_type),
    ) {
        return edge;
    }
    branched::<T, false, true>(req, frame, code, op)
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
    if let Some(edge) = inline_branch::<T, true, true>(
        req,
        frame,
        code,
        op,
        req.vm().opline(frame),
        ByKind(op.op1_type),
    ) {
        return edge;
    }
    branched::<T, true, true>(req, frame, code, op)
}

/// The body the four conditional jumps share: the operand read as
/// [`crate::ops::is_true`] reads it and freed, the bool written into the
/// temporary for an `_EX` (`EX`) whichever way the test went, and the
/// jump to `op2` taken where the truth is `ON`, after `ZEND_VM_JMP`'s
/// read of `EG(exception)`, since a conversion the test served may have
/// thrown. The plain pair's jump is [`jumped`], a back-edge's loop
/// safepoint among it; an `_EX`'s is [`jump_to`], which no loop closes.
/// A call of its own, so that an inline path before it keeps to the
/// registers it needs.
#[inline(never)]
fn branched<T: Tier, const ON: bool, const EX: bool>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = exec::operand(req, frame, code, op.op1_type, op.op1);
    let truth = crate::ops::is_true(req, &value);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    let truth = match truth {
        Ok(truth) => truth,
        Err(thrown) => return threw(req, frame, &thrown),
    };
    if EX {
        write_out(req, frame, code, op, &Value::bool(truth));
    }
    if truth != ON {
        return Ok(advance(req, frame));
    }
    if req.is_pending() {
        return Ok(Stepped::Threw(frame).into());
    }
    if EX {
        return jump_to(op.op2);
    }
    let at = req.vm().opline(frame);
    jumped::<T>(req, frame, at, op.op2, op.out)
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
    match crate::ops::is_true(req, &value) {
        Ok(true) => {}
        Ok(false) => {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            return Ok(advance(req, frame));
        }
        Err(thrown) => {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            return threw(req, frame, &thrown);
        }
    }
    let taken = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    set_out(req, frame, code, op, taken);
    jump_to(op.op2)
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
                    return Ok(Stepped::Threw(frame).into());
                }
            }
            Value::null()
        }
        SHORT_CIRCUITING_CHAIN_ISSET => Value::bool(false),
        _ => Value::bool(true),
    };
    set_out(req, frame, code, op, result);
    jump_to(op.op2)
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
    set_out(req, frame, code, op, taken);
    jump_to(op.op2)
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
    let table = jump_table(code, op, "SWITCH_LONG")?;
    let target = crate::ops::switch_long(&subject, table);
    Ok(switched(req, frame, target, op))
}

/// `ZEND_SWITCH_STRING`: [`crate::ops::switch_string`], the same way.
pub(crate) fn switch_string<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let subject = exec::operand_undef(req, frame, code, op.op1_type, op.op1);
    let table = jump_table(code, op, "SWITCH_STRING")?;
    let target = crate::ops::switch_string(&subject, table);
    Ok(switched(req, frame, target, op))
}

/// `ZEND_MATCH`: [`crate::ops::match_target`] over the subject and the
/// jump table in `op2`; an undefined CV is `ZVAL_UNDEFINED_OP1`, the
/// warning [`exec::operand`] raises, and then the default, unless a
/// handler of the warning threw, which php's handler raises there
/// (`HANDLE_EXCEPTION`) before any arm runs.
///
/// ```php
/// set_error_handler(function ($no, $text) { throw new Exception($text); });
/// try { echo match ($u) { default => "never\n", 1, 2, 3, 4, 5 => "no\n" }; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { echo match ($v) { default => "never\n", "1", "2", "3", "4", "5" => "no\n" }; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// restore_error_handler();
/// echo match ($w) { default => "default\n", 1 => "one\n" };
/// ```
/// ```expect
/// Undefined variable $u
/// Undefined variable $v
///
/// Warning: Undefined variable $w in %s on line 6
/// default
/// ```
pub(crate) fn r#match<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let mut subject = exec::operand_undef(req, frame, code, op.op1_type, op.op1);
    if subject.tag() == TypeTag::Undef {
        subject = exec::operand(req, frame, code, op.op1_type, op.op1);
        if req.is_pending() {
            return Ok(Stepped::Threw(frame).into());
        }
    }
    let table = jump_table(code, op, "MATCH")?;
    let target = crate::ops::match_target(&subject, table);
    Ok(switched(req, frame, target, op))
}

/// `ZEND_IN_ARRAY`: [`crate::ops::in_array`] over the needle, the table
/// in `op2` and `$strict` in `extended_value`, into the
/// temporary; the needle freed after it. An undefined CV is
/// `ZVAL_UNDEFINED_OP1`, the warning [`exec::operand`] raises, and then
/// `null`, which is what php's handler looks up after it warns; the
/// answer branched on as [`type_check`]'s is.
pub(crate) fn in_array<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let table = jump_table(code, op, "IN_ARRAY")?;
    let strict = op.extended_value != 0;
    let computed = unary_kernel(req, frame, code, op, |req, needle| {
        crate::ops::in_array(req, needle, table, strict).map(Value::bool)
    });
    smart_branch::<T, branch::Peeked>(req, frame, code, op, computed)
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
    threw(req, frame, &thrown)
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
    let computed = binary_kernel(req, frame, code, op, crate::ops::is_equal);
    smart_branch::<T, branch::Peeked>(req, frame, code, op, computed)
}

/// `ZEND_CASE_STRICT`: [`crate::ops::is_identical`], the same way, for
/// the arm of a `match` over a temporary subject.
pub(crate) fn case_strict<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let computed = binary_kernel(req, frame, code, op, crate::ops::is_identical);
    smart_branch::<T, branch::Peeked>(req, frame, code, op, computed)
}

/// `ZEND_QM_ASSIGN`: the operand into the temporary `result` names, a
/// temporary moved and a CV or a constant copied with one more owner,
/// the reference a CV holds followed (`ZVAL_COPY_DEREF`), an undefined
/// CV warned about and `null`; what every arm of a `match` leaves its
/// value by.
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn qm_assign<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    // The inline path: a value that owns nothing, copied into a
    // temporary that owns nothing either (`ZVAL_COPY_VALUE`), which is
    // what php's `QM_ASSIGN_LONG` and `QM_ASSIGN_DOUBLE` do for the pairs
    // its optimizer proved, tested here where the opline runs.
    let slots = req.slots_mut(frame);
    let z = ByKind(op.op1_type).read(slots, code, op.op1);
    if crate::operand::owns_nothing(z) && write_owning_nothing(slots, code, op, z).is_some() {
        return Ok(advanced());
    }
    qm_assigned(req, frame, code, op)
}

/// `QM_ASSIGN` past its inline path: the operand taken, with one more
/// owner or moved, into the temporary. A call of its own, so that the
/// inline path before it keeps to the registers it needs.
#[inline(never)]
fn qm_assigned(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    let value = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    set_out(req, frame, code, op, value);
    Ok(advance(req, frame))
}

/// `ZEND_COPY_TMP`: what the `TMP` or `VAR` in `op1` holds copied into
/// `result` with one more owner (`ZVAL_COPY`), `op1` left standing, a
/// reference copied as the reference it is. What a `??=` holds of an
/// expression its read computed for its write to read again
/// (`zend_compile_memoized_expr`), so a call on the way runs once:
///
/// ```php
/// function k() { echo "k"; return 'k'; }
/// $a = [];
/// var_dump($a[k()] ??= 1, $a[k()] ??= 2);
/// ```
/// ```expect
/// kkint(1)
/// int(1)
/// ```
pub(crate) fn copy_tmp<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let value = req.slot(frame, exec::temporary_slot(code, op.op1));
    req.addref(&value);
    set_out(req, frame, code, op, value);
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
#[inline(always)] // design §5.8: the loop dispatches into its body, as php's hybrid VM does
#[allow(clippy::inline_always)]
pub(crate) fn r#return<T: Tier, A: Operand + Default>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
) -> Edge {
    if let Some(edge) = inline_return::<T>(req, frame, code, op, A::default()) {
        return edge;
    }
    req.vm_mut().stand(frame, at);
    returned::<T>(req, frame, code, op)
}

/// `RETURN` past its inline path: the operand taken as the handler's
/// own ([`exec::take_operand`]) and the frame left with it
/// ([`returned_to_caller`]). A call of its own, so that the inline path
/// before it keeps to the registers it needs.
#[inline(never)]
fn returned<T: Tier>(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline) -> Edge {
    if let Some(edge) = return_site::<T>(req, frame, op) {
        return edge;
    }
    let returned = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    returned_to_caller(req, frame, returned)
}

/// The return safepoint of a `RETURN*` (design §5.8, `jit-design.md`
/// §2.2), raised for every frame but the entry frame of this serving
/// with the number the compiler wrote in `result`, before the handler
/// takes its operand: `None` where the tier answered `Continue`, and
/// otherwise the edge it resumed at, since a tier that recorded ran the
/// return and the caller's landing already and nothing of the return is
/// left to do here.
#[inline(always)] // once per return, in the RETURN arms
#[allow(clippy::inline_always)]
fn return_site<T: Tier>(req: &mut Request, frame: FrameRef, op: &Opline) -> Option<Edge> {
    if req.stack_mut().entry() == frame {
        return None;
    }
    match T::safepoint(req, frame, Site::Return(SafepointIdx(op.out))) {
        Flow::Continue => None,
        flow => Some(crate::tier::resumed(req, flow, frame)),
    }
}

/// The inline path of `ZEND_RETURN` (`ZEND_RETURN_SPEC_CONST`, `_TMP`,
/// `_CV`, `Z/Zend/zend_vm_def.h:4598`, into `zend_leave_helper`'s first
/// arm): the value copied into the caller's temporary, a temporary's
/// moved, and a frame whose leave destroys nothing popped, the caller
/// going on past its call. php's arm frees the CVs, which here gives up
/// strings at most; so no destructor runs and nothing can be pending
/// after it. The return safepoint is raised as
/// [`returned_to_caller`] raises it, once every test has passed, so the
/// kernel is never asked after it. `None`, with nothing touched, while
/// an observer is declared, on the serving's entry frame, for a value
/// that is a reference, or a CV's or a constant's that counts owners,
/// one nothing reads that would have to be given up, a caller's
/// temporary that holds an owner, and a frame [`Request::leaves_plainly`]
/// refuses.
///
/// A temporary string is moved into the caller's temporary, a string
/// parameter and a local are given up with the frame, a string nothing
/// reads and an object a local holds leave by the kernel, the object's
/// destructor served before the caller goes on:
///
/// ```php
/// class D { function __destruct() { echo "d\n"; } }
/// function t() { $s = str_repeat("ab", 2); return $s . "!"; }
/// function p($x) { $y = $x . "-"; return 1; }
/// function u() { return str_repeat("x", 2); }
/// function o() { $d = new D; return 2.5; }
/// $m = memory_get_usage();
/// var_dump(t(), p(str_repeat("q", 2)), o());
/// u();
/// var_dump(memory_get_usage() - $m);
/// ```
/// ```expect
/// d
/// string(5) "abab!"
/// int(1)
/// float(2.5)
/// int(32)
/// ```
#[inline(always)] // as `inline_send`
#[allow(clippy::inline_always)]
fn inline_return<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    a: impl Operand,
) -> Option<Edge> {
    if crate::observer::observing(req) || req.stack_mut().entry() == frame {
        return None;
    }
    let z = a.read(req.slots_mut(frame), code, op.op1);
    let moved = match a.kind() {
        OperandKind::Tmp | OperandKind::Var if crate::operand::movable(z) => a.slot(code, op.op1),
        OperandKind::Const | OperandKind::Cv | OperandKind::Tmp | OperandKind::Var
            if crate::operand::owns_nothing(z) =>
        {
            None
        }
        _ => return None,
    };
    return_plainly::<T>(req, frame, (code.num_cvs, moved), z, SafepointIdx(op.out))
}

/// [`inline_return`] once its operand is read: the frame left by
/// [`Request::leave_plainly`] where [`Request::leaves_plainly`] finds it
/// can be, after the return safepoint.
#[inline(always)] // as `inline_return`
#[allow(clippy::inline_always)]
fn return_plainly<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    (cvs, moved): (u32, Option<usize>),
    z: Zval,
    site: SafepointIdx,
) -> Option<Edge> {
    let leave = req.leaves_plainly(frame, cvs, moved, z)?;
    match T::safepoint(req, frame, Site::Return(site)) {
        Flow::Continue => {}
        // The tier ran the return and the caller's landing
        // (`jit-design.md` §2.2); the frame it left is not this one.
        flow => return Some(crate::tier::resumed(req, flow, frame)),
    }
    Some(Ok(req.leave_plainly(leave, z).into()))
}

/// The half of `RETURN` that leaves the frame, which `RETURN_BY_REF`
/// shares, once its return safepoint ([`return_site`]) is behind it:
/// the frame popped, and `returned` into the `VAR` the call opline
/// named, or released where the caller reads nothing.
fn returned_to_caller(req: &mut Request, frame: FrameRef, returned: Value) -> Edge {
    crate::observer::returned(req, frame, Some(&returned));
    if req.stack_mut().entry() == frame {
        req.set_returned(returned);
        return Ok(Stepped::Done.into());
    }
    let (caller, returns_to) = req.returning(frame);
    req.leave_frame(frame);
    if handed_back(req, returns_to, returned) {
        return Ok(Stepped::Threw(caller).into());
    }
    // The caller stands at the call opline that pushed this frame, and
    // goes on from the one after it.
    req.vm_mut().advance(caller);
    Ok(Stepped::Popped(caller).into())
}

/// What a call returned, handed to the frame that made the call: into
/// the frame's temporary `into` names where the call opline reads it, and given
/// up where it does not (`RETURN_VALUE_USED`). Then whether a throwable
/// is pending, which the popped callee's receiver or the value given up
/// may have left by running a destructor that threw, and which php
/// raises at the call opline (`zend_leave_helper`'s
/// `zend_rethrow_exception`, `ZEND_VM_NEXT_OPCODE_CHECK_EXCEPTION` after
/// an internal call), so the `try` around the call catches it. The
/// caller still stands at its call opline; it goes on past it only
/// where this answers `false`.
///
/// ```php
/// class D {
///     public function __destruct() { throw new Exception('dtor'); }
///     public function m() { return $this; }
///     public function __call($m, $a) { return $this; }
/// }
/// function make() { return new D(); }
/// try { make()->m(); } catch (Exception $e) { echo "method: ", $e->getMessage(), "\n"; }
/// try { make()->unknown(); } catch (Exception $e) { echo "__call: ", $e->getMessage(), "\n"; }
/// try { array_pop(...[[make()]]); } catch (Exception $e) { echo "native: ", $e->getMessage(), "\n"; }
/// echo "after\n";
/// ```
/// ```expect
/// method: dtor
/// __call: dtor
/// native: dtor
/// after
/// ```
fn handed_back(req: &mut Request, into: Option<(FrameRef, u32)>, value: Value) -> bool {
    match into {
        Some((at, slot)) => {
            req.replace_slot(at, slot, value);
        }
        None => req.release(value),
    }
    req.is_pending()
}

/// The body the binary operators' handlers share: both operands addressed,
/// the kernel called, both operands freed (`FREE_OP1`, `FREE_OP2`), and
/// what the kernel returned written into the temporary the opline's
/// `result` field names. A kernel that threw leaves the throwable
/// pending, which is the `Threw` edge the loop reads. A call of its
/// own, so that an inline path before it keeps to the registers it needs.
#[inline(never)]
fn binary(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    kernel: fn(&mut Request, &Value, &Value) -> Result<Value, Thrown>,
) -> Edge {
    let computed = binary_kernel(req, frame, code, op, kernel);
    computed_into(req, frame, code, op, computed)
}

/// A binary operator's handler the spec does not specialise: its
/// operands read through [`ByKind`] and taken by `op`'s inline rule
/// where it takes them ([`inline_arith`]), and [`binary`] over `kernel`
/// where it does not.
#[inline(always)] // the inline path is the handler's, in the loop
#[allow(clippy::inline_always)]
fn binary_by_kind(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    (rule, kernel): (Binary, BinaryKernel),
) -> Edge {
    let kinds = (ByKind(op.op1_type), ByKind(op.op2_type));
    if let Some(edge) = inline_arith(req, frame, code, op, rule, kinds) {
        return edge;
    }
    binary(req, frame, code, op, kernel)
}

/// [`binary`] up to the write: what the kernel computed, both operands
/// freed, `FREE_OP1` then `FREE_OP2`; `CASE` and `CASE_STRICT` keep
/// `op1`, `FREE_OP2` alone, since the subject is read again by the next
/// case.
fn binary_kernel(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    kernel: fn(&mut Request, &Value, &Value) -> Result<Value, Thrown>,
) -> Result<Value, Thrown> {
    let left = exec::operand(req, frame, code, op.op1_type, op.op1);
    let right = exec::operand(req, frame, code, op.op2_type, op.op2);
    let computed = kernel(req, &left, &right);
    if !matches!(op.opcode, Opcode::Case | Opcode::CaseStrict) {
        exec::free_operand(req, frame, code, op.op1_type, op.op1);
    }
    exec::free_operand(req, frame, code, op.op2_type, op.op2);
    computed
}

/// What a kernel computed, written into the temporary the opline's
/// `result` field names and the opline advanced past; a kernel that
/// threw leaves the throwable pending, which is the `Threw` edge the
/// loop reads.
fn computed_into(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    computed: Result<Value, Thrown>,
) -> Edge {
    match computed {
        Ok(value) => {
            set_out(req, frame, code, op, value);
            Ok(advance(req, frame))
        }
        Err(thrown) => threw(req, frame, &thrown),
    }
}

/// `ZEND_VM_SMART_BRANCH` (`Z/Zend/zend_execute.c:5803`), how each
/// handler `zend_is_smart_branch` names ends: where the opline after it
/// is the `JMPZ` or `JMPNZ` `zend_emit_cond_jump` left over its
/// temporary, which is when php's compiler marks the `result`, the
/// handler is one over both oplines (design §5.8). It writes nothing,
/// goes on past the jump where the jump is not taken, and takes the
/// jump as [`jumped`] does from the jump's opline, a backward one
/// raising the loop safepoint with the jump's number; a throwable
/// pending stops it at this opline. Anywhere else the answer is
/// written into the temporary, as [`computed_into`] writes it.
///
/// ```php
/// function f($n) { $i = 0; while ($i < $n) { $i++; } return isset($n) && $n == $i ? "met $i" : "no"; }
/// echo f(3), "\n";
/// class A {} $a = new A; $k = ['x' => 1];
/// if ($a instanceof A) echo "A "; if (!is_int($a)) echo "obj "; if (array_key_exists('x', $k)) echo "x ";
/// for ($j = 0; $j <= 2; $j++) { if ($j === 1) continue; echo $j; } echo "\n";
/// set_error_handler(function ($no, $text) { throw new Exception($text); });
/// try { if ($u == 1) echo "no"; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// met 3
/// A obj x 02
/// Undefined variable $u
/// ```
fn smart_branch<T: Tier, S: Branch>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    computed: Result<Value, Thrown>,
) -> Edge {
    let at = req.vm().opline(frame);
    let (Some(jump), Ok(answer)) = (S::fused(code, op, at), &computed) else {
        return computed_into(req, frame, code, op, computed);
    };
    if req.is_pending() {
        return Ok(Stepped::Threw(frame).into());
    }
    branch_fused::<T>(
        req,
        frame,
        at.saturating_add(1),
        jump,
        answer.as_bool() == Some(true),
    )
}

/// A comparison's handler past its inline path: [`binary_kernel`] and
/// [`smart_branch`] over what it computed. A call of its own, so that
/// the inline path before it keeps to the registers it needs.
#[inline(never)]
fn compared_branch<T: Tier, S: Branch>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    kernel: fn(&mut Request, &Value, &Value) -> Result<Value, Thrown>,
) -> Edge {
    let computed = binary_kernel(req, frame, code, op, kernel);
    smart_branch::<T, S>(req, frame, code, op, computed)
}

/// Where a comparison's answer goes (`ZEND_VM_SMART_BRANCH`, design
/// §5.8): into its temporary, or into the `JMPZ` or `JMPNZ` after it
/// that branches on the temporary, which makes the comparison's handler
/// one over both oplines. A comparison whose handler line says
/// `SMART_BRANCH` is resolved to one of three handlers where its unit is
/// published (`php_unit::Handler`), as php's `_JMPZ` and `_JMPNZ`
/// handlers are, so the handler knows which and reads the jump for its
/// target alone; every other comparison peeks at the opline after it, as
/// php's `ZEND_VM_SMART_BRANCH` reads what `pass_two` marked.
pub(crate) mod branch {
    use php_unit::{OpArray, Opcode, Opline};

    use crate::exec;

    /// The jump a comparison is fused to, and the truth that takes it:
    /// `false` for a `JMPZ`, `true` for a `JMPNZ`.
    #[derive(Clone, Copy)]
    pub(crate) struct Fused<'c> {
        pub(crate) jump: &'c Opline,
        pub(crate) on: bool,
    }

    /// Which of a comparison's handlers serves it.
    pub(crate) trait Branch {
        /// The jump fused to the comparison `op` at `at`, if any.
        fn fused<'c>(code: &'c OpArray, op: &Opline, at: u32) -> Option<Fused<'c>>;
    }

    /// The handler with no jump fused: the answer is written.
    pub(crate) struct Unfused;
    /// The handler fused to the `JMPZ` after it, php's `_JMPZ`.
    pub(crate) struct OnJmpz;
    /// The handler fused to the `JMPNZ` after it, php's `_JMPNZ`.
    pub(crate) struct OnJmpnz;
    /// A comparison whose handler line says no `SMART_BRANCH`, which
    /// tests the opline after it on every execution
    /// ([`Opline::fuses`]).
    pub(crate) struct Peeked;

    impl Branch for Unfused {
        #[inline(always)] // a constant, so the fused path folds away
        #[allow(clippy::inline_always)]
        fn fused<'c>(_: &'c OpArray, _: &Opline, _: u32) -> Option<Fused<'c>> {
            None
        }
    }

    impl Branch for OnJmpz {
        #[inline(always)] // the jump's one read, in the loop
        #[allow(clippy::inline_always)]
        fn fused<'c>(code: &'c OpArray, _: &Opline, at: u32) -> Option<Fused<'c>> {
            exec::opline_at(code, at.saturating_add(1)).map(|jump| Fused { jump, on: false })
        }
    }

    impl Branch for OnJmpnz {
        #[inline(always)] // the jump's one read, in the loop
        #[allow(clippy::inline_always)]
        fn fused<'c>(code: &'c OpArray, _: &Opline, at: u32) -> Option<Fused<'c>> {
            exec::opline_at(code, at.saturating_add(1)).map(|jump| Fused { jump, on: true })
        }
    }

    impl Branch for Peeked {
        #[inline(always)] // the comparison's inline path, which is in the loop
        #[allow(clippy::inline_always)]
        fn fused<'c>(code: &'c OpArray, op: &Opline, at: u32) -> Option<Fused<'c>> {
            exec::opline_at(code, at.saturating_add(1))
                .filter(|jump| op.fuses(jump))
                .map(|jump| Fused {
                    jump,
                    on: jump.opcode == Opcode::Jmpnz,
                })
        }
    }
}

/// The fused jump at `at` over the comparison's `answer`: on past the
/// jump where it is not taken, and taken as [`jumped`] takes it from the
/// jump's own opline, a backward one raising the loop safepoint with
/// the jump's number.
#[inline(always)] // as `branch::Branch::fused`
#[allow(clippy::inline_always)]
fn branch_fused<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    at: u32,
    fused: branch::Fused<'_>,
    answer: bool,
) -> Edge {
    if answer != fused.on {
        return jump_to(at.saturating_add(1));
    }
    jumped::<T>(req, frame, at, fused.jump.op2, fused.jump.out)
}

/// The inline path of the binary operators (design §5.8,
/// `Z/Zend/zend_vm_def.h:50-940`), `ZEND_ADD`, `ZEND_SUB` and `ZEND_MUL`
/// monomorphised and `ZEND_DIV`, `ZEND_MOD`, the shifts and the bitwise
/// three through [`ByKind`]: both operands read where `a`
/// and `b` address them and, where [`inline::binary`] takes them as they
/// stand, the number written into the temporary and the opline advanced
/// past, with no kernel, no owner taken and none given up. A temporary
/// operand that held a number owns nothing and is left where it stands,
/// as php leaves it. `None`, with nothing touched, where the operands
/// are not two numbers or the temporary holds an owner, and the kernel
/// serves the opline.
#[inline(always)] // the handler's fast half, as php's is in the handler
#[allow(clippy::inline_always)]
fn inline_arith(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    rule: impl inline::Operator,
    (a, b): (impl Operand, impl Operand),
) -> Option<Edge> {
    let slots = req.slots_mut(frame);
    let computed = rule.apply(a.read(slots, code, op.op1), b.read(slots, code, op.op2))?;
    write_owning_nothing(slots, code, op, computed)?;
    Some(Ok(advanced()))
}

/// The inline path of the comparisons that branch (`ZEND_IS_SMALLER`,
/// `ZEND_IS_SMALLER_OR_EQUAL`, `ZEND_IS_EQUAL`, `ZEND_IS_NOT_EQUAL`):
/// two numbers as they stand ordered by [`inline::order`], and the
/// answer `holds` reads off the order branched on where a `JMPZ` or a
/// `JMPNZ` is fused to the comparison ([`branch::Branch`]), or written into
/// the temporary as a bool where none is. `None`, with nothing touched,
/// where the operands are not two numbers, or the temporary holds an
/// owner.
#[inline(always)] // as `inline_arith`
#[allow(clippy::inline_always)]
fn inline_compare<T: Tier, S: Branch>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
    holds: impl FnOnce(Ordering) -> bool,
    (a, b): (impl Operand, impl Operand),
) -> Option<Edge> {
    let slots = req.slots_mut(frame);
    let answer = holds(inline::order(
        a.read(slots, code, op.op1),
        b.read(slots, code, op.op2),
    )?);
    if let Some(jump) = S::fused(code, op, at) {
        return Some(branch_fused::<T>(
            req,
            frame,
            at.saturating_add(1),
            jump,
            answer,
        ));
    }
    write_owning_nothing(req.slots_mut(frame), code, op, Zval::bool(answer))?;
    Some(Ok(advanced()))
}

/// The inline path of the four conditional jumps
/// (`Z/Zend/zend_vm_def.h:3003-3053`), `ON` the truth that takes the
/// jump and `EX` whether the truth is written into the temporary as a
/// bool, as [`branched`] serves them: the operand's truth as
/// [`inline::truth`] reads it, nothing to free, since a temporary
/// holding a scalar owns nothing, and the jump in `op2` taken where the
/// truth is `ON`, the plain pair's as [`jumped`] takes it, a back-edge's
/// interrupt test and loop safepoint among it, and an `_EX`'s as
/// [`jump_to`] does. `None` where the truth is the kernel's to read, or
/// an `_EX`'s temporary holds an owner.
#[inline(always)] // as `inline_arith`
#[allow(clippy::inline_always)]
fn inline_branch<T: Tier, const ON: bool, const EX: bool>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    at: u32,
    a: impl Operand,
) -> Option<Edge> {
    let slots = req.slots_mut(frame);
    let truth = inline::truth(a.read(slots, code, op.op1))?;
    if EX {
        write_owning_nothing(slots, code, op, Zval::bool(truth))?;
    }
    if truth != ON {
        return Some(Ok(advanced()));
    }
    if EX {
        return Some(jump_to(op.op2));
    }
    Some(jumped::<T>(req, frame, at, op.op2, op.out))
}

/// The inline path of `ZEND_BOOL` (`NOT` false) and `ZEND_BOOL_NOT`
/// (`NOT` true): the operand's truth as [`inline::truth`] reads it, the
/// other way round for `NOT`, written into the temporary as a bool.
/// `None` where the truth is the kernel's to read, or the temporary
/// holds an owner.
#[inline(always)] // as `inline_arith`
#[allow(clippy::inline_always)]
fn inline_truth<const NOT: bool>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    a: impl Operand,
) -> Option<Edge> {
    let slots = req.slots_mut(frame);
    let truth = inline::truth(a.read(slots, code, op.op1))?;
    write_owning_nothing(slots, code, op, Zval::bool(truth != NOT))?;
    Some(Ok(advanced()))
}

/// The inline path of `ZEND_PRE_INC`, `ZEND_PRE_DEC` and their `POST_`
/// pair (`post`) over a variable holding a number
/// (`Z/Zend/zend_vm_def.h:1490-1683`): a CV, or the element a `VAR`
/// names through the `IS_INDIRECT` a `FETCH_DIM_RW` left there, as
/// `GET_OP1_ZVAL_PTR_PTR_UNDEF(BP_VAR_RW)` reads it. `+ 1` or `- 1` as
/// [`inline::step`] computes it, into the variable and, under
/// `RETURN_VALUE_USED`, the temporary, which takes what the variable
/// held before the step for a `POST_*`; the `VAR` is given up as
/// `FREE_OP1` gives up an `IS_INDIRECT`, which owns nothing. `None`, with
/// nothing touched, for a variable holding anything but a number,
/// which a reference is, a `VAR` holding no `IS_INDIRECT`, and a
/// temporary holding an owner.
///
/// ```php
/// $a = ["l" => 1, "d" => 1.5, "s" => "5", "n" => null, "m" => PHP_INT_MIN]; $k = "l";
/// var_dump(--$a[$k], $a["d"]--, ++$a["s"], --$a["n"], $a["n"]++, --$a["m"], $a["d"]);
/// $b = [1]; $c = $b; $b[0]++; $r = &$b[0]; --$b[0]; var_dump($b, $c, $r);
/// ```
/// ```expect
///
/// Warning: Decrement on type null has no effect, this will change in the next major version of PHP in %s on line 3
/// int(0)
/// float(1.5)
/// int(6)
/// NULL
/// NULL
/// float(-9.223372036854776E+18)
/// float(0.5)
/// array(1) {
///   [0]=>
///   &int(1)
/// }
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// int(1)
/// ```
#[inline(always)] // as `inline_arith`
#[allow(clippy::inline_always)]
fn inline_incdec(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    (arith, post): (Arith, bool),
    a: impl Operand,
) -> Option<Edge> {
    let at = a.slot(code, op.op1)?;
    let slots = req.slots_mut(frame);
    match a.kind() {
        OperandKind::Cv => {
            let held = *slots.get(at)?;
            let stepped = inline::step(arith, held)?;
            if op.out_type != OperandKind::Unused {
                write_owning_nothing(slots, code, op, if post { held } else { stepped })?;
            }
            *slots.get_mut(at)? = stepped;
        }
        OperandKind::Var => {
            let entry = slots.get(at)?.as_indirect()?;
            let held = entry.read();
            let stepped = inline::step(arith, held)?;
            if op.out_type != OperandKind::Unused {
                write_owning_nothing(slots, code, op, if post { held } else { stepped })?;
            }
            entry.write(stepped);
            *slots.get_mut(at)? = Zval::UNDEF;
        }
        OperandKind::Tmp | OperandKind::Const | OperandKind::Unused => return None,
    }
    Some(Ok(advanced()))
}

/// The inline path of `ZEND_ASSIGN` to a CV (`ZEND_ASSIGN_SPEC_CV_*`,
/// `zend_assign_to_variable`): the value taken by [`taken_value`]'s rule
/// and written over a variable that holds nothing that counts owners or
/// a string, which is given up after the write, as
/// `zend_assign_to_variable`'s `GC_DELREF` of the garbage gives it up; a
/// string serves no destructor and is offered to no collector. `None`
/// for a `VAR`, a variable holding a reference or any other owner, and
/// a value the rule leaves to the kernel.
///
/// ```php
/// $s = str_repeat('x', 3); $t = str_repeat('y', 3);
/// $s = $t . 'z'; $s = $t; $u = $s = $t . 'w';
/// debug_zval_dump($s, $t, $u);
/// ```
/// ```expect
/// string(4) "yyyw" refcount(4)
/// string(3) "yyy" refcount(2)
/// string(4) "yyyw" refcount(4)
/// ```
#[inline(always)] // as `inline_arith`
#[allow(clippy::inline_always)]
fn inline_assign(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    (a, b): (impl Operand, impl Operand),
) -> Option<Edge> {
    if a.kind() != OperandKind::Cv {
        return None;
    }
    let at = a.slot(code, op.op1)?;
    let slots = req.slots_mut(frame);
    let held = *slots.get(at)?;
    let garbage = held.is_refcounted().then(|| held.as_str()).flatten();
    if held.is_refcounted() && garbage.is_none() {
        return None;
    }
    let value = taken_value(slots, code, op, b, op.op2)?;
    *slots.get_mut(at)? = value;
    if let Some(garbage) = garbage {
        req.heap_mut().release_string(garbage);
    }
    Some(Ok(advanced()))
}

/// `ZVAL_COPY_VALUE(EX_VAR(opline->result.var), z)` for a zval that owns
/// nothing, into a temporary that owns nothing either; `None`, nothing
/// written, where the temporary holds an owner, since a write over it
/// would give the owner up twice (#723) and [`set_out`] is where that is
/// sensed.
#[inline(always)] // as `inline_arith`
#[allow(clippy::inline_always)]
fn write_owning_nothing(slots: &mut [Zval], code: &OpArray, op: &Opline, z: Zval) -> Option<()> {
    let out = slots.get_mut(exec::temporary_slot(code, op.out) as usize)?;
    if out.is_refcounted() {
        return None;
    }
    *out = z;
    Some(())
}

/// `ZEND_VM_NEXT_OPCODE` after an inline path, which serves no user
/// code and so leaves nothing pending for [`advance`] to test.
fn advanced() -> Next {
    Next::ADVANCE
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
    let computed = unary_kernel(req, frame, code, op, kernel);
    computed_into(req, frame, code, op, computed)
}

/// [`unary`] up to the write: what the kernel computed, the operand
/// freed.
fn unary_kernel(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    kernel: impl FnOnce(&mut Request, &Value) -> Result<Value, Thrown>,
) -> Result<Value, Thrown> {
    let operand = exec::operand(req, frame, code, op.op1_type, op.op1);
    let computed = kernel(req, &operand);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    computed
}

/// `zend_incdec_typed_ref` (`Z/Zend/zend_execute.c:2230`): the step over
/// a variable bound to a reference that typed properties hold. An `int`
/// the step carried past its range into a `float` is php's `TypeError`
/// where a property the reference is typed by takes no `float`, and the
/// cell holds the range's end; anything else the step makes is held to
/// the properties as an assignment through the reference is, the value
/// it held standing where they refuse it.
///
/// ```php
/// class T { public int $i = PHP_INT_MAX; public int|float $f = PHP_INT_MAX; public int|bool $b = 1; }
/// $t = new T; $i = &$t->i; $f = &$t->f; $b = &$t->b;
/// try { $i++; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $f++; $b--;
/// var_dump($t->i, $t->f, $t->b);
/// ```
/// ```expect
/// Cannot increment a reference held by property T::$i of type int past its maximal value
/// int(9223372036854775807)
/// float(9.223372036854776E+18)
/// int(0)
/// ```
#[allow(clippy::too_many_arguments)]
fn incdec_typed_ref(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
    post: bool,
    kernel: fn(&mut Request, &Value) -> Result<Value, Thrown>,
    cell: PhpRef,
    held: Value,
) -> Edge {
    let stepped = match kernel(req, &held) {
        Ok(stepped) => stepped,
        Err(thrown) => {
            return threw(req, frame, &thrown);
        }
    };
    let increment = matches!(op.opcode, Opcode::PreInc | Opcode::PostInc);
    let strict = req.frame_strict_types(frame);
    let (stored, raised) =
        match step_through_typed_ref(req, cell, &held, stepped, increment, strict) {
            RefStep::Landed(stored) => (stored, None),
            RefStep::Clamped(stored, thrown) => (stored, Some(thrown)),
            RefStep::Refused(thrown) => {
                write_out(req, frame, code, op, &Value::null());
                exec::free_operand(req, frame, code, op.op1_type, op.op1);
                return threw(req, frame, &thrown);
            }
        };
    write_out(req, frame, code, op, if post { &held } else { &stored });
    req.reference_set(cell.heap(), stored);
    exec::free_operand(req, frame, code, op.op1_type, op.op1);
    if let Some(thrown) = raised {
        return threw(req, frame, &thrown);
    }
    Ok(advance(req, frame))
}

/// What a step through a reference typed properties hold comes to
/// ([`step_through_typed_ref`]).
enum RefStep {
    /// The value to store in the cell: the step, or the conversion the
    /// properties agree on.
    Landed(Value),
    /// The range's end to store, beside php's `TypeError` for an `int`
    /// stepped into a `float` a property takes no `float` for.
    Clamped(Value, Thrown),
    /// The properties refuse the step; the cell keeps what it held.
    Refused(Thrown),
}

/// The heart of `zend_incdec_typed_ref`, over the value `held` and what
/// the step made of it, `stepped`, whose owner it takes: an `int` carried
/// past its range into a `float` a property the reference is typed by
/// takes no `float` for is [`RefStep::Clamped`] at the range's end, and
/// anything else is held to the properties as an assignment through the
/// reference is (`zend_verify_ref_assignable_zval`).
fn step_through_typed_ref(
    req: &mut Request,
    cell: PhpRef,
    held: &Value,
    stepped: Value,
    increment: bool,
    strict: bool,
) -> RefStep {
    let refusing = req
        .ref_type_sources(cell.heap())
        .iter()
        .find(|source| crate::ops::steps_past_int(held, &stepped, &source.ty))
        .map(|source| (source.declaring, source.ty.clone(), source.name.clone()));
    if let Some((declaring, ty, name)) = refusing {
        req.release(stepped);
        let step = (increment, crate::ops::Holder::Reference);
        let (thrown, limit) = crate::ops::incdec_prop_error(req, &ty, declaring, &name, step);
        return RefStep::Clamped(limit, thrown);
    }
    let verdict = crate::ops::verify_ref_assignable(req, cell.heap(), &stepped, strict);
    match crate::ops::landed(req, stepped, verdict) {
        Ok(landed) => RefStep::Landed(landed),
        Err(thrown) => RefStep::Refused(thrown),
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
///
/// A call of its own, so that an inline path before it keeps to the
/// registers it needs.
#[inline(never)]
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
    // `ZEND_REF_HAS_TYPE_SOURCES(ref)`: `zend_incdec_typed_ref`.
    if let Some(cell) = cell.filter(|cell| cell.heap().sources() != 0) {
        return incdec_typed_ref(req, frame, code, op, post, kernel, cell, held);
    }
    match kernel(req, &held) {
        Ok(stepped) => {
            // `ZVAL_COPY(EX_VAR(opline->result.var), var_ptr)`, before
            // the step for a `POST_*` and after it for a `PRE_*`; one
            // more owner either way.
            write_out(req, frame, code, op, if post { &held } else { &stepped });
            // The kernel's one owner of the stepped value is the
            // variable's from here, and what stood there is released,
            // which is what the in-place step does.
            if let Some(cell) = cell {
                req.reference_set(cell.heap(), stepped);
            } else if let Some(place) = place {
                let previous = exec::value_of(place.read());
                place.write(stepped.into_zval());
                req.release(previous);
            } else {
                req.replace_slot(frame, op.op1, stepped);
            }
            // `FREE_OP1()`: a `VAR` holding an `IS_INDIRECT` owns
            // nothing, and one holding what `offsetGet` answered gives
            // it up.
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            Ok(advance(req, frame))
        }
        Err(thrown) => {
            exec::free_operand(req, frame, code, op.op1_type, op.op1);
            threw(req, frame, &thrown)
        }
    }
}

/// Where a jump-table opcode sends the frame: `ZEND_VM_SET_RELATIVE_OPCODE`
/// to the target or to `extended_value`, or `ZEND_VM_NEXT_OPCODE` on to
/// the chain. Every target is forward, so none is a safepoint site.
fn switched(req: &Request, frame: FrameRef, switched: Switched, op: &Opline) -> Next {
    match switched {
        Switched::Target(target) => Next::jump(target),
        Switched::Default => Next::jump(op.extended_value),
        Switched::Chain => advance(req, frame),
    }
}

/// The table a `SWITCH_LONG`, `SWITCH_STRING`, `MATCH` or `IN_ARRAY`
/// carries as the constant `op2` names, or the fatal for an `op2` that
/// is no array, which no unit the compiler wrote has (design §10).
fn jump_table(code: &OpArray, op: &Opline, opcode: &str) -> Result<PhpArray, Fatal> {
    let table = exec::constant(code, ConstIdx(op.op2)).as_array().copied();
    table.ok_or_else(|| {
        Fatal::error(format!(
            "InternalError: {opcode} names a constant that is no table"
        ))
    })
}

/// `ZEND_VM_NEXT_OPCODE`: the same frame, at the opline after this one,
/// and the `Threw` edge where the opline left a throwable pending,
/// which is `ZEND_VM_NEXT_OPCODE_CHECK_EXCEPTION`. php has the two
/// macros and spends the test only in the handlers that can raise; this
/// is one rule for both, because nothing is pending after a handler php
/// gives the unchecked macro to, and the release a handler makes last
/// is where a destructor is served (design §5.10), which is PHP code
/// between this opline and the next. The frame stays at the opline that
/// raised, php's `EG(opline_before_exception)`, for the unwinder to
/// read which `try` element and live ranges it stands in.
fn advance(req: &Request, frame: FrameRef) -> Next {
    if req.is_pending() {
        return Stepped::Threw(frame).into();
    }
    Next::ADVANCE
}

/// `ZEND_VM_NEXT_OPCODE_EX(1, 2)`, [`advance`] for a handler that is two
/// oplines, the `OP_DATA` after it carrying the value: the opline after
/// the pair, and the `Threw` edge at this one where a throwable is
/// pending, which a handler php carries on past a diagnostic leaves.
/// Every such handler ends here, so what a destructor or a `__set` it
/// served threw is raised at the pair and never by the opline after it,
/// which an inline path serves with no pending test of its own:
///
/// ```php
/// class D { function __destruct() { throw new Exception("d"); } }
/// class S { public static $s = [0]; }
/// $o = (object)["p" => [0]];
/// try { var_dump($o->p += [new D]); } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
/// try { var_dump(S::$s += [new D]); } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
/// try { var_dump($o->q = new D, 1); } catch (Exception $e) { echo "never\n"; }
/// try { $o->q = 2; echo "past\n"; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// caught d
/// caught d
/// object(D)#3 (0) {
/// }
/// int(1)
/// caught d
/// ```
fn advance_over_data(req: &Request, frame: FrameRef, at: u32) -> Next {
    if req.is_pending() {
        return Stepped::Threw(frame).into();
    }
    Next::jump(at.saturating_add(2))
}

/// The body the by-value `SEND_*` handlers share: the operand placed in
/// the slot [`reserve_arg`] gave the send, the frame owning it from
/// here: a CV or a constant with one more owner (`ZVAL_COPY`,
/// `SEND_VAR`'s and `SEND_VAL`'s copy), a temporary moved
/// (`ZVAL_COPY_VALUE`).
fn send(req: &mut Request, frame: FrameRef, code: &OpArray, op: &Opline, slot: ArgSlot) -> Edge {
    let value = exec::take_operand(req, frame, code, op.op1_type, op.op1);
    place_argument(req, frame, slot, value)
}

/// The placing half of [`send`]: `value` into the call frame being
/// built, in `slot`, the frame owning it from here.
fn place_argument(req: &mut Request, frame: FrameRef, slot: ArgSlot, value: Value) -> Edge {
    put_argument(req, slot, value)?;
    Ok(advance(req, frame))
}

/// `value` written into `slot` of the call being built, the call owning
/// it from here.
fn put_argument(req: &mut Request, slot: ArgSlot, value: Value) -> Result<(), Fatal> {
    let call = slot.call;
    let previous = match slot.to {
        ArgTo::Positional(at) | ArgTo::Named(crate::api::call::NamedAt::Argument(at)) => req
            .vm_mut()
            .stack
            .frame_mut(call)
            .and_then(|frame| frame.slots_mut().get_mut(at))
            .map(|placed| Value::of_zval(std::mem::replace(placed, value.to_zval()))),
        ArgTo::Named(crate::api::call::NamedAt::Extra(at)) => req
            .annex_mut(call)
            .and_then(|annex| annex.extra_named.get_mut(at))
            .map(|(_, placed)| std::mem::replace(placed, value)),
    };
    let Some(previous) = previous else {
        req.release(value);
        return Err(Fatal::error(
            "InternalError: a SEND_* opline places an argument in a slot the call has not",
        ));
    };
    req.release(previous);
    Ok(())
}

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
        return crate::api::raising::throw_error(req, "Error", "Can only throw objects");
    };
    let throwable = req.class_named(Bare::resolved(b"throwable"));
    if !throwable.is_some_and(|of| req.instance_of(object.class(), of)) {
        return crate::api::raising::throw_error(
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
///
/// The variable the clause names is written as an assignment writes
/// it, strictly: bound to a typed property's cell, a throwable the type
/// refuses is php's `TypeError`, raised from the clause.
///
/// ```php
/// class T { public int $i = 42; }
/// $t = new T; $ref = &$t->i;
/// try { try { throw new Exception("ex"); } catch (Exception $ref) { echo "no\n"; } }
/// catch (TypeError $e) { echo $e->getMessage(), " on line ", $e->getLine(), "\n"; }
/// var_dump($t->i);
/// ```
/// ```expect
/// Cannot assign Exception to reference held by property T::$i of type int on line 4
/// int(42)
/// ```
pub(crate) fn catch<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    let pending = match req.pending() {
        Some(pending) => Some(pending),
        None if req.is_pending() => None,
        None => return jump_to(op.op2),
    };
    let slot = Some(CacheSlot(op.extended_value & !LAST_CATCH));
    let matched = pending.is_some_and(|pending| {
        cached_class(req, frame, code, op.op1, slot)
            .is_some_and(|of| req.instance_of(pending.class(), of))
    });
    if !matched {
        if op.extended_value & LAST_CATCH != 0 {
            return Ok(Stepped::Threw(frame).into());
        }
        return jump_to(op.op2);
    }
    let Some(caught) = req.take_pending_view() else {
        return Ok(advance(req, frame));
    };
    if op.out_type == OperandKind::Cv {
        let value = caught.as_value();
        if let Some(cell) = req.slot(frame, op.out).as_php_ref() {
            // `zend_assign_to_variable(ex, &tmp, IS_TMP_VAR, /* strict */
            // 1)`: a cell a typed property is a source of takes the
            // throwable only where every type holds it, with no
            // coercion, and refuses it with php's `TypeError` otherwise.
            if let Err(thrown) = req.assign_to_reference(cell.heap(), value, true) {
                return threw(req, frame, &thrown);
            }
        } else {
            req.replace_slot(frame, op.out, value);
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
    let left = req.fast_call(frame, op.out).and_then(|cell| {
        cell.from = Some(at);
        cell.delayed.take()
    });
    // php's temporary is dead at every `FAST_CALL` and holds no object;
    // a cell this engine kept from an earlier pass is given up rather
    // than written over.
    if let Some(left) = left {
        req.release_raised(left);
    }
    jump_to(op.op1)
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
    let cell = req.fast_call(frame, op.op1).map(|cell| {
        let from = cell.from.take();
        let delayed = cell.delayed.take();
        (from, delayed)
    });
    let (from, delayed) = cell.unwrap_or((None, None));
    if let Some(from) = from {
        return jump_to(from.saturating_add(1));
    }
    if let Some(delayed) = delayed {
        req.raise_under(delayed);
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
    let cell = req.fast_call(frame, op.op1).map(|cell| {
        let from = cell.from;
        (from, cell.delayed.take())
    });
    let (from, delayed) = cell.unwrap_or((None, None));
    if let Some(from) = from {
        free_carried_value(req, frame, code, from);
    }
    if let Some(delayed) = delayed {
        req.release_raised(delayed);
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
///
/// A throwable a loop variable's destructor raises from the `FE_FREE`
/// of a `return`, after the `finally` block the return called, is
/// raised past the loop's `try`, whose `catch` never takes it:
///
/// ```php
/// class A { public $x = 1; function __destruct() { throw new Exception("d"); } }
/// function f() {
///     foreach (new A as $a) {
///         try { return $a; } catch (Exception $e) { echo "inner\n"; } finally { echo "finally\n"; }
///     }
/// }
/// try { f(); } catch (Exception $e) { echo "outer ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// finally
/// outer d
/// ```
pub(crate) fn handle_exception<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    op: &Opline,
) -> Edge {
    req.vm_mut().stand_at_exception_op(frame, true);
    let mut op_num = req.vm().opline(frame);
    // A throwable a loop variable's destruction raised on the way out of
    // a `foreach` is held to have been raised at the last of the frees,
    // which is where the `try` element covering it is looked for.
    if let Some(to_end) = exec::loop_end_distance(op) {
        let moved = op_num.saturating_add(to_end);
        cleanup_live_vars(req, frame, code, op_num, moved);
        op_num = moved;
    }
    let current = exec::innermost_try(code, op_num);
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
            req.vm_mut().stand_at_exception_op(frame, false);
            return jump_to(element.catch_op.0);
        }
        if op_num < element.finally_op.0 {
            if req.unwinding() {
                at = offset.checked_sub(1);
                continue;
            }
            let Some(var) = fast_call_var(code, element) else {
                return Err(Fatal::error(
                    "InternalError: a try element whose finally_end names no FAST_RET",
                ));
            };
            cleanup_live_vars(req, frame, code, op_num, element.finally_op.0);
            let delayed = req.take_raised();
            if let Some(cell) = req.fast_call(frame, var) {
                cell.delayed = delayed;
                cell.from = None;
            }
            req.vm_mut().stand_at_exception_op(frame, false);
            return jump_to(element.finally_op.0);
        }
        if op_num < element.finally_end.0 {
            let Some(var) = fast_call_var(code, element) else {
                return Err(Fatal::error(
                    "InternalError: a try element whose finally_end names no FAST_RET",
                ));
            };
            let carried = req.fast_call(frame, var).and_then(|cell| cell.from);
            if let Some(from) = carried {
                free_carried_value(req, frame, code, from);
            }
            let delayed = req
                .fast_call(frame, var)
                .and_then(|cell| cell.delayed.take());
            if let Some(delayed) = delayed {
                match (req.diagnostics().raised(), delayed) {
                    (Some(Raised::Throwable(pending)), Raised::Throwable(delayed)) => {
                        req.set_previous(pending, delayed);
                    }
                    (None, delayed) if !req.diagnostics().bailing_out() => {
                        req.set_raised(Some(delayed));
                    }
                    // Either exit, or a fatal, gives up what the block
                    // delayed; a throwable gives up a delayed exit.
                    (_, delayed) => req.release_raised(delayed),
                }
            }
        }
        at = offset.checked_sub(1);
    }
    // The uncaught arm (`Z/Zend/zend_vm_def.h:8157-8161`): the frame's
    // observers ended with no value before its live temporaries go, the
    // frame standing at `ZEND_HANDLE_EXCEPTION` where the unwinder
    // reached it from a throw, and at the `FAST_RET` that took the
    // unwind up again, where php reads a stale `EX(opline)`: the last
    // one a handler saved, since `FAST_RET` saves none.
    crate::observer::returned(req, frame, None);
    cleanup_live_vars(req, frame, code, op_num, 0);
    if req.stack_mut().entry() == frame {
        if !req.is_pending() {
            // The return of nothing below, from the serving's entry
            // frame, which the loop's caller pops: the serving is over.
            req.set_returned(Value::undef());
            return Ok(Stepped::Done.into());
        }
        return Ok(Stepped::Threw(frame).into());
    }
    let (caller, returns_to) = req.returning(frame);
    req.leave_frame(frame);
    // `zend_leave_helper`: the callee wrote no return value, so the
    // caller's temporary for the call is left undefined, and the caller
    // stands at the call opline, not past it, for the unwind to go on
    // from there.
    if let Some((at, slot)) = returns_to {
        let garbage = req.take_slot(at, slot);
        req.release(garbage);
    }
    if !req.is_pending() {
        // Nothing to unwind: a tier went on in a frame at its exception
        // op after what was thrown there was given up, as phpdbg's `ev`
        // renders it. The helper's return-value slot is `UNDEF` and,
        // nothing pending, `zend_leave_helper` leaves the frame as a
        // return of nothing; the caller goes on past its call.
        req.vm_mut().advance(caller);
        return Ok(Stepped::Popped(caller).into());
    }
    Ok(Stepped::Threw(caller).into())
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
fn jump_to(at: u32) -> Edge {
    Ok(Next::jump(at))
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
    if let Some(edge) = return_site::<T>(req, frame, op) {
        return edge;
    }
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
            match bound_at(req, at) {
                Ok(bound) => bound,
                Err(thrown) => return threw(req, frame, &thrown),
            }
        }
    } else {
        return Err(Fatal::error(
            "InternalError: RETURN_BY_REF over an operand that is no place",
        ));
    };
    returned_to_caller(req, frame, returned)
}

/// `ZEND_GENERATOR_CREATE` (`Z/Zend/zend_vm_def.h:4652`): the frame
/// moved into the `Generator` the call returns ([`crate::generator::create`]).
pub(crate) fn generator_create<T: Tier>(
    req: &mut Request,
    frame: FrameRef,
    _: &OpArray,
    _: &Opline,
) -> Edge {
    crate::generator::create(req, frame).map(Next::from)
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
        let thrown = crate::api::raising::throw_error(
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
        req.replace_slot(frame, slot, Value::null());
        slot
    });
    crate::generator::yielded(req, generator, value, key, send_target);
    crate::observer::generator_yielded(req, frame, generator);
    crate::generator::suspend(req, generator)?;
    Ok(Stepped::Done.into())
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
    } else {
        bound_at(req, place)?
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
        let thrown = crate::api::raising::throw_error(
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
    let owned = exec::owned_deref(req, owned);
    let out = (op.out_type != OperandKind::Unused).then(|| exec::temporary_slot(code, op.out));
    match crate::generator::yield_from(req, generator, owned, op.op1_type == OperandKind::Const) {
        Ok(crate::generator::From::Finished(retval)) => {
            match out {
                Some(slot) => {
                    req.replace_slot(frame, slot, retval);
                }
                None => req.release(retval),
            }
            Ok(advance(req, frame))
        }
        Ok(crate::generator::From::Delegating) => {
            if let Some(slot) = out {
                req.replace_slot(frame, slot, Value::null());
            }
            crate::observer::generator_yielded(req, frame, generator);
            crate::generator::suspend(req, generator)?;
            Ok(Stepped::Done.into())
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
    // `ZEND_OBSERVER_FCALL_END(generator->execute_data, &generator->retval)`
    // before the generator is closed (`Z/Zend/zend_vm_def.h:4766`).
    crate::observer::returned(req, frame, Some(&value));
    crate::generator::returned(req, generator, value);
    Ok(Stepped::Done.into())
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
            Some(place) => match bound_at(req, place) {
                Ok(bound) => bound,
                Err(thrown) => return threw(req, frame, &thrown),
            },
            None => req.take_slot(frame, at),
        }
    };
    req.replace_slot(frame, out, held);
    Ok(advance(req, frame))
}

#[cfg(test)]
mod tests {

    /// The edge a handler called outside the loop served, as
    /// [`crate::tier::step`] hands it out: the position the loop would
    /// have held written back to the frame, which the handler was
    /// entered at as the dispatch enters it.
    macro_rules! served {
        ($req:ident, $frame:expr, $at:ident => $call:expr) => {{
            let $at = $req.vm().opline($frame);
            let edge = $call;
            edge.map(|next| crate::tier::settled(&mut $req, $frame, $at, next))
        }};
    }
    use std::sync::Arc;

    use php_heap::Intern as _;
    use php_unit::{Opcode, OperandKind, PublishedUnit, Unit};

    use super::*;
    use crate::api::request::Leaving;
    use crate::api::values::{ClassId, PhpObject};
    use crate::engine::{Engine, RequestDescription};
    use crate::probe::{Asked, Probe};
    use crate::tier::Nop;

    /// A unit whose main op array is one `JMP` to `target`, its
    /// safepoint number `number`.
    fn jumping(target: u32, number: u32) -> PublishedUnit {
        Unit {
            main: OpArray {
                oplines: vec![
                    Opline::default(),
                    Opline {
                        opcode: Opcode::Jmp,
                        op1: target,
                        out: number,
                        ..Opline::default()
                    },
                ],
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None)
    }

    /// Design §5.7, §5.8: a `JMP` to an earlier opline is a back-edge and
    /// raises the loop safepoint with the number the compiler wrote; one
    /// to a later opline raises nothing. Both leave the frame at the
    /// target, which is the opline a tier is asked with.
    #[test]
    fn a_backward_jump_raises_the_loop_safepoint_and_a_forward_one_does_not() {
        let engine = Engine::cli_builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = jumping(0, 1);
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let back = code.oplines.get(1).copied().unwrap_or_default();
        req.vm_mut().stand(frame, 1);

        assert_eq!(
            served!(req, frame, at => jmp::<Probe>(&mut req, frame, code, &back)),
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
            served!(req, frame, at => jmp::<Probe>(&mut req, frame, code, &forward)),
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
        let engine = Engine::cli_builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = jumping(0, 1);
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let back = code.oplines.get(1).copied().unwrap_or_default();
        req.vm_mut().stand(frame, 1);
        req.limits().interrupt().elapsed(1);

        assert_eq!(
            served!(req, frame, at => jmp::<Probe>(&mut req, frame, code, &back)),
            Err(Fatal::error("Maximum execution time of 1 second exceeded"))
        );
        assert_eq!(req.state::<Asked>().sites, vec![]);
    }

    /// A unit whose main op array is `NOP` then a `JMPNZ` over the
    /// constant at slot zero, aimed at the `NOP`, with safepoint number
    /// one: the shape of `for (;;)` after a body of one opline.
    fn conditional(literal: php_heap::Zval) -> PublishedUnit {
        Unit {
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
                ],
                literals: vec![literal],
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None)
    }

    /// Design §5.7, §5.8: a `JMPNZ` taken to an earlier opline is the
    /// back-edge and raises the loop safepoint as `JMP` does; one not
    /// taken goes on to the next opline and raises nothing.
    #[test]
    fn a_conditional_jump_is_the_back_edge_only_when_it_is_taken() {
        let engine = Engine::cli_builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let taken = conditional(php_heap::Zval::bool(true));
        let frame = req.pushed(PublishedUnit::clone(&taken));
        let code = crate::exec::code_of(&taken);
        let op = code.oplines.get(1).copied().unwrap_or_default();
        req.vm_mut().stand(frame, 1);

        assert_eq!(
            served!(req, frame, at => jmpnz::<Probe, crate::operand::Const>(&mut req, frame, code, &op, at)),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().opline(frame), 0);
        assert_eq!(
            req.state::<Asked>().sites,
            vec![(Site::Loop(SafepointIdx(1)), frame)]
        );

        let not_taken = conditional(php_heap::Zval::long(0));
        let frame = req.pushed(PublishedUnit::clone(&not_taken));
        let code = crate::exec::code_of(&not_taken);
        req.vm_mut().stand(frame, 1);
        assert_eq!(
            served!(req, frame, at => jmpnz::<Probe, crate::operand::Const>(&mut req, frame, code, &op, at)),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().opline(frame), 2, "on to the opline after it");
        assert_eq!(
            req.state::<Asked>().sites.len(),
            1,
            "nothing more was asked"
        );
    }

    /// A unit whose main op array is `NOP`, `IS_SMALLER` over the
    /// constants `left` and one into temporary zero, and a `JMPNZ` over
    /// that temporary aimed at the `NOP` with safepoint number seven:
    /// `do {} while ($i < 1)` over a constant.
    fn compared_loop(left: i64) -> PublishedUnit {
        Unit {
            main: OpArray {
                oplines: vec![
                    Opline::default(),
                    Opline {
                        opcode: Opcode::IsSmaller,
                        op1_type: OperandKind::Const,
                        op2_type: OperandKind::Const,
                        op2: 1,
                        out_type: OperandKind::Tmp,
                        out: 0,
                        ..Opline::default()
                    },
                    Opline {
                        opcode: Opcode::Jmpnz,
                        op1_type: OperandKind::Tmp,
                        op1: 0,
                        op2: 0,
                        out: 7,
                        ..Opline::default()
                    },
                    Opline::default(),
                ],
                literals: vec![php_heap::Zval::long(left), php_heap::Zval::long(1)],
                num_tmps: 1,
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None)
    }

    /// Design §5.8: a comparison with a `JMPNZ` over its temporary after
    /// it is one handler over both oplines. Taken backward, the fused
    /// handler raises one loop safepoint with the branch opline's
    /// number and leaves the frame at the target; not taken, one `step`
    /// lands past the jump, raising nothing.
    #[test]
    fn a_comparison_branches_over_the_jump_after_it_as_one_step() {
        let engine = Engine::builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let taken = compared_loop(0);
        let frame = req.pushed(PublishedUnit::clone(&taken));
        let code = crate::exec::code_of(&taken);
        let op = code.oplines.get(1).copied().unwrap_or_default();
        req.vm_mut().stand(frame, 1);

        assert_eq!(
            served!(req, frame, at => is_smaller::<Probe, crate::operand::Const, crate::operand::Const, branch::OnJmpnz>(
                &mut req, frame, code, &op, at
            )),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().opline(frame), 0);
        assert_eq!(
            req.state::<Asked>().sites,
            vec![(Site::Loop(SafepointIdx(7)), frame)]
        );

        let not_taken = compared_loop(1);
        let frame = req.pushed(PublishedUnit::clone(&not_taken));
        req.vm_mut().stand(frame, 1);
        assert_eq!(crate::tier::step(&mut req, frame), Ok(Stepped::Same(frame)));
        assert_eq!(req.vm().opline(frame), 3, "past the jump in one step");
        assert_eq!(req.state::<Asked>().sites.len(), 1);
    }

    /// A unit whose main op array is one `RETURN null`, its safepoint
    /// number `number`, as the compiler writes it into `result`.
    fn returning(number: u32) -> PublishedUnit {
        Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::Return,
                    op1_type: OperandKind::Const,
                    op1: 0,
                    out: number,
                    ..Opline::default()
                }],
                literals: vec![php_heap::Zval::null()],
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None)
    }

    /// Design §4.3 seam 8: `RETURN` raises the return safepoint, with the
    /// number the compiler wrote, for a frame that is not the entry frame
    /// of this serving, and pops it to its caller.
    #[test]
    fn a_return_in_no_entry_frame_raises_the_return_safepoint_and_pops() {
        let engine = Engine::cli_builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = returning(1);
        let outer = req.pushed(PublishedUnit::clone(&unit));
        let guard = req.stack_mut().push_entry(outer);
        let inner = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let op = code.oplines.first().copied().unwrap_or_default();

        assert_eq!(
            served!(req, inner, at => r#return::<Probe, crate::operand::Const>(&mut req, inner, code, &op, at)),
            Ok(Stepped::Popped(outer))
        );
        assert_eq!(
            req.state::<Asked>().sites,
            vec![(Site::Return(SafepointIdx(1)), inner)]
        );

        // The entry frame's own return raises nothing and ends the loop.
        assert_eq!(
            served!(req, outer, at => r#return::<Probe, crate::operand::Const>(&mut req, outer, code, &op, at)),
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
    fn by_name(strings: &mut php_heap::Strings) -> PublishedUnit {
        let text = |strings: &mut php_heap::Strings, bytes: &[u8]| {
            strings
                .intern(bytes)
                .map_or(php_heap::Zval::UNDEF, php_heap::Zval::string)
        };
        let written = text(strings, b"StrLen");
        let lowered = text(strings, b"strlen");
        let argument = text(strings, b"abc");
        Unit {
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
                ],
                literals: vec![written, lowered, argument],
                num_tmps: 1,
                cache_size: 8,
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None)
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
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = by_name(&mut strings);
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let init = code.oplines.first().copied().unwrap_or_default();
        let send = code.oplines.get(1).copied().unwrap_or_default();
        let call = code.oplines.get(2).copied().unwrap_or_default();

        assert_eq!(
            served!(req, frame, at => init_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
            Ok(Stepped::Same(frame))
        );
        assert!(!req.is_pending(), "the lowered constant is the table's key");
        assert_eq!(
            served!(req, frame, at => send_val_ex::<Nop>(&mut req, frame, code, &send, at)),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(
            served!(req, frame, at => do_fcall_by_name::<Nop>(&mut req, frame, code, &call)),
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
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = by_name(&mut strings);
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let init = code.oplines.first().copied().unwrap_or_default();
        assert_eq!(req.cached(frame, CacheSlot(init.out)), Cached::Unprimed);

        assert_eq!(
            served!(req, frame, at => init_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().lookups, 1, "the first pass looks the table up");
        let strlen = req.function_named(Bare::resolved(b"strlen"));
        assert!(strlen.is_some());
        assert_eq!(
            req.cached(frame, CacheSlot(init.out)),
            Cached::Function(strlen.unwrap_or(Function::PASS)),
            "and primes the slot with the callee"
        );

        assert_eq!(
            served!(req, frame, at => init_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
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
    /// own, not yet shared.
    fn declaring_f(strings: &mut php_heap::Strings) -> Unit {
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
        Unit {
            main: OpArray {
                cache_size: 8,
                ..OpArray::default()
            },
            functions: vec![(decl, body)],
            ..Unit::default()
        }
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
        let engine = Engine::cli_builder().build();
        let req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let mut unit = declaring_f(&mut strings);
        crate::engine::assign_map_ptrs(&mut unit, &engine.shared.map_ptr_last);
        called_twice(req, &unit.published(None));
    }

    /// The same over a unit compiled for the request alone, whose op
    /// arrays take caches of the request's own and none of the engine's
    /// map-pointer offsets, php's heap caches (`ZEND_ACC_HEAP_RT_CACHE`):
    /// the function's second call still reads what its first primed, and
    /// the engine's count has not moved (#528).
    #[test]
    fn a_request_only_unit_s_function_called_twice_reads_what_its_first_call_primed() {
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let count = || {
            engine
                .shared
                .map_ptr_last
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        let before = count();
        let unit = req.take_unit(declaring_f(&mut strings));
        called_twice(req, &unit);
        assert_eq!(count(), before, "the engine's count is where it was");
    }

    /// `f` of `unit` called twice from its main op array: one lookup of
    /// the table across both calls, each call's slot primed with the
    /// callee, and the main op array's own slot not what they primed.
    fn called_twice(mut req: Request, unit: &PublishedUnit) {
        let main = req.pushed(PublishedUnit::clone(unit));
        req.declare_functions(unit);
        let f = req
            .function_named(Bare::resolved(b"f"))
            .unwrap_or(Function::PASS);
        let code = crate::exec::body_of(unit, exec::Body::Function(0))
            .cloned()
            .unwrap_or_default();
        let init = code.oplines.first().copied().unwrap_or_default();
        let strlen = req.function_named(Bare::resolved(b"strlen"));

        for call in 1..=2 {
            let mut header = exec::Header::call(f, 0);
            header.info.set_serving();
            let frame = req
                .push_call(header)
                .and_then(|call| {
                    let shape = req.call_shape(call).unwrap_or_default();
                    req.activate_user(call, shape, None, crate::exec::Caller::Opline(main))
                })
                .map_or(main, |activated| activated.frame);
            assert_eq!(
                served!(req, frame, at => init_fcall_by_name::<Nop>(&mut req, frame, &code, &init, at)),
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
        let call = req.building_call()?;
        let callee = req.vm().at(call)?.function;
        req.pop_call();
        callee
    }

    /// `ZEND_INIT_NS_FCALL_BY_NAME` over `Foo\StrLen`, `foo\strlen`,
    /// `strlen`: the first pass makes the handler's two lookups, the
    /// namespaced name that misses and the global one that hits, and
    /// primes the slot; the second pass makes neither.
    #[test]
    fn a_namespaced_call_by_name_looks_the_table_up_twice_once() {
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::InitNsFcallByName,
                    op2_type: OperandKind::Const,
                    op2: 0,
                    out: 8,
                    extended_value: 0,
                    ..Opline::default()
                }],
                literals: vec![
                    interned(&mut strings, b"Foo\\StrLen"),
                    interned(&mut strings, b"foo\\strlen"),
                    interned(&mut strings, b"strlen"),
                ],
                cache_size: 16,
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None);
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let init = code.oplines.first().copied().unwrap_or_default();
        let strlen = req.function_named(Bare::resolved(b"strlen"));

        assert_eq!(
            served!(req, frame, at => init_ns_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.vm().lookups, 2, "foo\\strlen misses, strlen hits");
        assert_eq!(
            req.cached(frame, CacheSlot(8)),
            Cached::Function(strlen.unwrap_or(Function::PASS))
        );
        assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

        assert_eq!(
            served!(req, frame, at => init_ns_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
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
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::InitFcallByName,
                    op2_type: OperandKind::Const,
                    op2: 0,
                    out: 0,
                    ..Opline::default()
                }],
                literals: vec![
                    interned(&mut strings, b"NoPe"),
                    interned(&mut strings, b"nope"),
                ],
                cache_size: 8,
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None);
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let init = code.oplines.first().copied().unwrap_or_default();
        for pass in 1..=2 {
            assert_eq!(
                served!(req, frame, at => init_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
                Ok(Stepped::Threw(frame))
            );
            assert_eq!(req.vm().lookups, pass);
            assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);
            assert_eq!(req.uncaught(), Err(Leaving));
            // Each pass is a serving of its own, whose end catches the
            // bailout the rendering began.
            req.catch_bailout();
        }
    }

    /// A unit whose main op array is one `CATCH` of the class the pair
    /// `written`/`lowered` names, the last clause of its `try`
    /// (`ZEND_LAST_CATCH`), naming no variable, its cache slot at byte
    /// zero and its jump to the `NOP` after it.
    fn catching(strings: &mut php_heap::Strings, written: &[u8], lowered: &[u8]) -> PublishedUnit {
        Unit {
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
                ],
                literals: vec![interned(strings, written), interned(strings, lowered)],
                cache_size: 8,
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None)
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
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = catching(&mut strings, b"Error", b"error");
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let clause = code.oplines.first().copied().unwrap_or_default();
        let error = req.class_named(Bare::resolved(b"error"));
        assert!(error.is_some(), "the engine declares Error");
        assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

        for pass in 1..=2 {
            let thrown = crate::api::raising::throw_error(&mut req, "Error", "e");
            req.leave_pending(&thrown);
            req.vm_mut().stand(frame, 0);
            assert_eq!(
                served!(req, frame, at => catch::<Nop>(&mut req, frame, code, &clause)),
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
    ) -> PublishedUnit {
        let (op2_type, op2) = match method {
            Some(_) => (OperandKind::Const, 2),
            None => (OperandKind::Unused, 0),
        };
        let mut literals = vec![interned(strings, class.0), interned(strings, class.1)];
        if let Some((written, lowered)) = method {
            literals.push(interned(strings, written));
            literals.push(interned(strings, lowered));
        }
        Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::InitStaticMethodCall,
                    op1_type: OperandKind::Const,
                    op1: 0,
                    op2_type,
                    op2,
                    out: 0,
                    ..Opline::default()
                }],
                literals,
                cache_size: 16,
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None)
    }

    /// `ZEND_INIT_STATIC_METHOD_CALL` over two constants, twice, which
    /// is what a call on a class inside a loop is: the first pass finds its
    /// slot unprimed, looks the class up once and writes the class and
    /// the callee together, `CACHE_POLYMORPHIC_PTR(opline->result.num,
    /// ce, fbc)`; the second reads the pair back and looks nothing up
    /// and resolves no method.
    #[test]
    fn a_static_call_over_two_constants_resolves_its_class_and_callee_once() {
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = static_method_call(
            &mut strings,
            (b"Closure", b"closure"),
            Some((b"fromCallable", b"fromcallable")),
        );
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let op = code.oplines.first().copied().unwrap_or_default();
        let closure = req.class_named(Bare::resolved(b"closure"));
        assert!(closure.is_some(), "the engine declares Closure");
        let callee = closure.and_then(|class| req.method_named(class, b"fromcallable"));
        assert!(callee.is_some(), "Closure::fromCallable is a static method");
        assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

        for pass in 1..=2 {
            req.vm_mut().stand(frame, 0);
            assert_eq!(
                served!(req, frame, at => init_static_method_call::<Nop>(&mut req, frame, code, &op)),
                Ok(Stepped::Same(frame)),
                "the call frame is pushed on pass {pass}"
            );
            assert_eq!(req.vm().lookups, 1, "one lookup over {pass} pass(es)");
            assert!(
                matches!(
                    req.cached(frame, CacheSlot(0)),
                    Cached::ClassAndCallee { class, scope: None, function, .. }
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
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = static_method_call(&mut strings, (b"Exception", b"exception"), None);
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let op = code.oplines.first().copied().unwrap_or_default();
        let exception = req.class_named(Bare::resolved(b"exception"));
        assert!(exception.is_some(), "the engine declares Exception");

        for pass in 1..=2 {
            req.vm_mut().stand(frame, 0);
            assert_eq!(
                served!(req, frame, at => init_static_method_call::<Nop>(&mut req, frame, code, &op)),
                Ok(Stepped::Threw(frame)),
                "a constructor is an instance method on pass {pass}"
            );
            assert!(req.take_pending_view().is_some(), "it threw on pass {pass}");
            assert_eq!(req.vm().lookups, 1, "one lookup over {pass} pass(es)");
            assert!(
                matches!(req.cached(frame, CacheSlot(0)), Cached::Class(class)
                    if Some(class) == exception),
                "the class alone is in the slot after pass {pass}"
            );
        }
    }

    /// A unit whose main op array is one `INIT_METHOD_CALL` over the CV
    /// `$o` and the pair `GetMessage`, `getmessage`, with the two cache
    /// slots the compiler reserves in `result` at byte zero: `$o->GetMessage()`.
    fn method_call(strings: &mut php_heap::Strings) -> PublishedUnit {
        Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::InitMethodCall,
                    op1_type: OperandKind::Cv,
                    op1: 0,
                    op2_type: OperandKind::Const,
                    op2: 0,
                    out: 0,
                    ..Opline::default()
                }],
                literals: vec![
                    interned(strings, b"GetMessage"),
                    interned(strings, b"getmessage"),
                ],
                vars: vec![php_unit::decl::Name::from("o")],
                num_cvs: 1,
                // `zend_alloc_cache_slots(2)`: the class and the callee.
                cache_size: 16,
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None)
    }

    /// `ZEND_INIT_METHOD_CALL` over a constant name, twice on an
    /// `Exception` and once on an `Error`, which is what `$o->m()` in a
    /// loop over two classes is: the first pass finds the slot
    /// unprimed, looks the method up and writes the receiver's class
    /// and the callee together, `CACHE_POLYMORPHIC_PTR(opline->result.num,
    /// called_scope, fbc)`; the second reads the pair back and looks
    /// nothing up; the third, on another class, looks the method up once
    /// more and writes that class's pair over it.
    #[test]
    fn a_method_call_by_a_constant_name_looks_its_callee_up_once_per_class() {
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = method_call(&mut strings);
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let op = code.oplines.first().copied().unwrap_or_default();
        let exception = req.class_named(Bare::resolved(b"exception"));
        let error = req.class_named(Bare::resolved(b"error"));
        assert!(
            exception.is_some() && error.is_some(),
            "the engine declares both"
        );
        let callee = |req: &Request, class: Option<ClassId>| {
            class.and_then(|class| req.method_named(class, b"getmessage"))
        };
        let (on_exception, on_error) = (callee(&req, exception), callee(&req, error));
        assert!(
            on_exception.is_some() && on_exception != on_error,
            "each declares its own"
        );
        assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

        for (pass, class, function, lookups) in [
            (1, exception, on_exception, 1),
            (2, exception, on_exception, 1),
            (3, error, on_error, 2),
        ] {
            let made = class.map(|class| req.new_object(class));
            assert!(
                matches!(made, Some(Ok(_))),
                "an object is made on pass {pass}"
            );
            let Some(Ok(object)) = made else { continue };
            req.replace_slot(frame, 0, object.as_value());
            req.vm_mut().stand(frame, 0);
            assert_eq!(
                served!(req, frame, at => init_method_call::<Nop>(&mut req, frame, code, &op)),
                Ok(Stepped::Same(frame)),
                "the call frame is pushed on pass {pass}"
            );
            req.pop_call();
            assert_eq!(req.vm().lookups, lookups, "lookups over {pass} pass(es)");
            assert!(
                matches!(
                    req.cached(frame, CacheSlot(0)),
                    Cached::ClassAndCallee { class: held, scope: None, function: callee, .. }
                        if Some(held) == class && Some(callee) == function
                ),
                "the receiver's class and its callee are in the slot after pass {pass}"
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
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = catching(&mut strings, b"NoPe", b"nope");
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let clause = code.oplines.first().copied().unwrap_or_default();
        let thrown = crate::api::raising::throw_error(&mut req, "Error", "e");
        req.leave_pending(&thrown);

        for pass in 1..=2 {
            req.vm_mut().stand(frame, 0);
            assert_eq!(
                served!(req, frame, at => catch::<Nop>(&mut req, frame, code, &clause)),
                Ok(Stepped::Threw(frame))
            );
            assert_eq!(req.vm().lookups, pass, "looked up again on pass {pass}");
            assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);
        }
        assert_eq!(req.uncaught(), Err(Leaving));
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
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = Unit {
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
                ],
                literals: vec![
                    interned(&mut strings, b"NoPe"),
                    interned(&mut strings, b"nope"),
                    interned(&mut strings, b"NOPE"),
                ],
                num_tmps: 3,
                cache_size: 24,
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None);
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let named = req.class_named(Bare::resolved(b"stdclass"));
        assert!(named.is_some(), "the engine declares stdClass");
        let class = named.unwrap_or(ClassId(u32::MAX));
        for slot in [0, 8, 16] {
            req.prime(frame, CacheSlot(slot), Cached::Class(class));
        }

        let new_op = code.oplines.first().copied().unwrap_or_default();
        assert_eq!(
            served!(req, frame, at => new::<Nop>(&mut req, frame, code, &new_op)),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(
            req.slot(frame, 0).as_object().map(PhpObject::class),
            Some(class),
            "NEW took its class off op2's slot"
        );

        let instanceof_op = code.oplines.get(2).copied().unwrap_or_default();
        assert_eq!(
            served!(req, frame, at => instanceof::<Nop>(&mut req, frame, code, &instanceof_op)),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(
            req.slot(frame, 1).as_bool(),
            Some(true),
            "INSTANCEOF took its class off extended_value's slot"
        );

        let constant_op = code.oplines.get(3).copied().unwrap_or_default();
        assert_eq!(
            served!(req, frame, at => fetch_class_constant::<Nop>(&mut req, frame, code, &constant_op)),
            Ok(Stepped::Threw(frame))
        );
        assert_eq!(
            pending_message(&req),
            "Undefined constant stdClass::NOPE",
            "FETCH_CLASS_CONSTANT took its class off extended_value's slot"
        );
        assert_eq!(req.vm().lookups, 0, "no handler looked the table up");
    }

    /// A unit whose main op array is `JMP_FRAMELESS` over `name`, its
    /// jump to opline 2, its cache slot at byte 0, then two `NOP`s.
    fn jmp_frameless_over(strings: &mut php_heap::Strings, name: &[u8]) -> PublishedUnit {
        Unit {
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
                ],
                literals: vec![interned(strings, name)],
                cache_size: 8,
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None)
    }

    /// `ZEND_JMP_FRAMELESS` over two passes of one opline: the first
    /// finds `ZEND_JMP_FL_UNPRIMED`, looks the namespaced name up once
    /// and primes `ZEND_JMP_FL_HIT` where the table has it not, so the
    /// jump is taken; the second reads `HIT` off the slot and jumps with
    /// no lookup.
    #[test]
    fn a_frameless_jump_primes_a_hit_once_and_jumps_on_it_after() {
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = jmp_frameless_over(&mut strings, b"foo\\substr");
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let jmp = code.oplines.first().copied().unwrap_or_default();
        assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

        for pass in 1..=2 {
            req.vm_mut().stand(frame, 0);
            assert_eq!(
                served!(req, frame, at => jmp_frameless::<Nop>(&mut req, frame, code, &jmp)),
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
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = jmp_frameless_over(&mut strings, b"strlen");
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let jmp = code.oplines.first().copied().unwrap_or_default();

        for pass in 1..=2 {
            req.vm_mut().stand(frame, 0);
            assert_eq!(
                served!(req, frame, at => jmp_frameless::<Nop>(&mut req, frame, code, &jmp)),
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
        fn seven(_: &mut Request) -> Result<Owner<Value>, Thrown> {
            Ok(Owner::int(7))
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
        Engine::cli_builder().extensions(&[extension]).build()
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
        let unit = Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::FramelessIcall0,
                    out_type: OperandKind::Tmp,
                    out: 0,
                    extended_value: row,
                    ..Opline::default()
                }],
                num_tmps: 1,
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None);
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let call = code.oplines.first().copied().unwrap_or_default();
        assert_eq!(
            served!(req, frame, at => frameless_icall_0::<Nop>(&mut req, frame, code, &call)),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(req.slot(frame, 0).as_int(), Some(7));
        assert_eq!(req.vm().opline(frame), 1);
        assert!(
            req.serving_call().is_none(),
            "the call is popped with the native"
        );
    }

    /// A row past the engine's table is the internal-error fatal that
    /// names it, never a read past the end.
    #[test]
    fn a_row_the_table_has_not_is_an_internal_error() {
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::FramelessIcall0,
                    out_type: OperandKind::Tmp,
                    out: 0,
                    extended_value: 1_000_000,
                    ..Opline::default()
                }],
                num_tmps: 1,
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None);
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let call = code.oplines.first().copied().unwrap_or_default();
        assert!(matches!(
            served!(req, frame, at => frameless_icall_0::<Nop>(&mut req, frame, code, &call)),
            Err(fatal) if matches!(fatal.reason(), php_heap::FatalReason::Error(message) if message.starts_with("InternalError: ZEND_FRAMELESS_ICALL_0 indexes frameless entry 1000000"))
        ));
    }

    /// `zend_undefined_function_helper` names the constant as written
    /// (`RT_CONSTANT(opline, opline->op2)`) where the lookup read the
    /// lowered one after it, so the `Error` for `NoPe()` says `NoPe`;
    /// the served witness beside [`init_fcall_by_name`] pins the whole
    /// line against the oracle.
    #[test]
    fn a_call_by_name_of_no_function_raises_over_the_name_as_written() {
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let mut strings = php_heap::Strings::new();
        let unit = by_name(&mut strings);
        let mut oplines = unit.main.oplines.clone();
        let written = strings
            .intern(b"NoPe")
            .map_or(php_heap::Zval::UNDEF, php_heap::Zval::string);
        let lowered = strings
            .intern(b"nope")
            .map_or(php_heap::Zval::UNDEF, php_heap::Zval::string);
        if let Some(init) = oplines.first_mut() {
            init.op2 = 3;
        }
        let unit = Unit {
            main: OpArray {
                oplines,
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
        }
        .published(None);
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let init = code.oplines.first().copied().unwrap_or_default();

        assert_eq!(
            served!(req, frame, at => init_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
            Ok(Stepped::Threw(frame))
        );
        assert_eq!(pending_message(&req), "Call to undefined function NoPe()");
    }

    /// The message of the throwable pending, empty where none is.
    fn pending_message(req: &Request) -> String {
        req.pending()
            .and_then(|throwable| throwable.prop(req, b"message"))
            .and_then(|message| message.as_str_bytes().map(<[u8]>::to_vec))
            .map(|message| String::from_utf8_lossy(&message).into_owned())
            .unwrap_or_default()
    }

    /// A unit whose main op array has one CV, one temporary and one
    /// `ASSIGN` of the constant `1` into the CV, its value unused:
    /// `$a = 1;`.
    fn assigning() -> PublishedUnit {
        Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::Assign,
                    op1_type: OperandKind::Cv,
                    op1: 0,
                    op2_type: OperandKind::Const,
                    op2: 0,
                    out_type: OperandKind::Unused,
                    ..Opline::default()
                }],
                literals: vec![php_heap::Zval::long(1)],
                num_cvs: 1,
                num_tmps: 1,
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None)
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
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = assigning();
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let code = crate::exec::code_of(&unit);
        let mut op = code.oplines.first().copied().unwrap_or_default();
        op.out_type = OperandKind::Tmp;
        op.out = 0;
        let Ok(text) = req.string(b"in the cell") else {
            return;
        };
        let Ok(cell) = req.reference(Value::string(text)) else {
            return;
        };
        let start = req.memory().usage(false);
        // Two CVs would share the cell; this frame has one, and the test
        // is the second holder.
        req.addref(&cell.as_value());
        req.replace_slot(frame, 0, cell.as_value());

        assert_eq!(
            served!(req, frame, at => assign::<Nop, crate::operand::Cv, crate::operand::Const>(&mut req, frame, code, &op, at)),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(
            req.slot(frame, 0).as_php_ref(),
            Some(&cell),
            "the CV still holds the cell"
        );
        assert_eq!(cell.get().as_int(), Some(1), "and the cell holds the value");
        assert_eq!(
            req.memory().usage(false),
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
        assert_eq!(req.memory().usage(false), start - 40 - 32);
    }

    /// #367, the first acceptance: a `foreach` by value over a plain
    /// object walks the properties the scope may see, the private one
    /// stepped over from outside the class, the key the name as
    /// written. The witness beside [`fe_reset_r`] pins the same line
    /// against the oracle; this holds the engine to it.
    #[test]
    fn a_foreach_over_an_object_walks_the_properties_the_scope_may_see() {
        let served = Engine::cli_builder().build().serve_capture(
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
        let served = Engine::cli_builder().build().serve_capture(
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
        let served = Engine::cli_builder().build().serve_capture(
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
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let named = req.class_named(Bare::resolved(b"exception"));
        assert!(named.is_some(), "the engine declares Exception");
        let class = named.unwrap_or(ClassId(u32::MAX));
        let made = crate::api::call::new_object_made(&mut req, class);
        assert!(made.is_ok(), "Exception is instantiable");
        if let Ok(object) = made {
            let started = req.object_walk_start(object);
            assert!(
                matches!(started, Ok(Some(_))),
                "Exception declares properties, so the walk is registered"
            );
            if let Ok(Some(idx)) = started {
                assert!(
                    matches!(req.object_walk(object, idx, false), Ok(None)),
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
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let unit = Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::QmAssign,
                    op1_type: OperandKind::Const,
                    op1: 0,
                    out_type: OperandKind::Tmp,
                    out: 0,
                    lineno: 5,
                    ..Opline::default()
                }],
                literals: vec![php_heap::Zval::long(7)],
                num_tmps: 1,
                filename: php_unit::decl::Name::from("planted.php"),
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None);
        let frame = req.pushed(PublishedUnit::clone(&unit));
        let Ok(left) = req.string(b"an owner left behind") else {
            return;
        };
        let planted = req.set_slot(frame, 0, Value::string(left));
        assert!(
            planted.to_zval().is_undef(),
            "the slot was empty before the plant"
        );

        assert_eq!(
            crate::tier::step(&mut req, frame),
            Err(Fatal::error(
                "InternalError: ZEND_QM_ASSIGN at opline 0 writes its TMP #0 over an owner it still holds (String): the handler that last read the slot moved it out and did not empty it"
            ))
        );
        assert_eq!(
            req.slot(frame, 0).as_int(),
            Some(7),
            "the write itself stood"
        );
        // Rendered at the line of the handler that wrote the temporary
        // (#2046).
        assert!(
            matches!(req.ending(), Some(crate::api::request::Ending::Fatal(logged)) if logged.contains(" in planted.php on line 5")),
            "{:?}",
            req.ending()
        );
        req.pop_frame();
    }
}
