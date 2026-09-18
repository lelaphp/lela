//! Emission, after `zend_compile.c` (design §6.1): what the statements
//! read from a source file become as oplines, with php's operand kinds,
//! php's constant table and php's safepoint numbering (design §5.7).
//!
//! One op array is built at a time. Constants are added as
//! `zend_add_literal` adds them, the same value twice keeping one slot;
//! CVs are numbered as `lookup_cv` numbers them, one per name in the
//! order the names are met; temporaries are numbered as
//! `get_temporary_variable` numbers them, and a temporary's slot on the
//! frame is its number past the last CV.
//!
//! An operator over values known here is folded as php folds it
//! (`zend_try_ct_eval_binary_op`), so `1/3` and `'a' . 'b'` are each one
//! constant and no opline. What it cannot fold becomes the opcode php
//! emits, into a temporary as `zend_emit_op_tmp` leaves it: `ZEND_ADD`,
//! `ZEND_SUB`, `ZEND_MUL` and `ZEND_DIV`, `ZEND_MOD`, `ZEND_POW`,
//! `ZEND_SL`, `ZEND_SR`, `ZEND_CONCAT`, the comparisons `ZEND_IS_SMALLER`
//! and `ZEND_IS_SMALLER_OR_EQUAL`, which `>` and `>=` are with the
//! operands swapped (`zend_compile_greater`), and the equalities
//! `ZEND_IS_EQUAL`, `ZEND_IS_NOT_EQUAL`, `ZEND_IS_IDENTICAL`,
//! `ZEND_IS_NOT_IDENTICAL` and `ZEND_SPACESHIP`, whose kernels the
//! engine serves. A constant operand of a `.` is converted to a string
//! where it stands whether or not the other side folds, as
//! `zend_compile_binary_op` converts it, so `2 . $a` is a `CONCAT` over
//! the constant `"2"`; a comparison over two constants folds always,
//! through the same `zend_compare` and `zend_is_identical` the opcodes
//! read; and a `===` or `!==` against a constant null, false or true is
//! the `ZEND_TYPE_CHECK` php emits in its place. After the final return,
//! `pass_two`'s one change to an operand order: a `COMMUTATIVE` opcode
//! whose first operand is the lesser kind has its operands swapped, so
//! `2 * $a` and `2 == $a` read the CV first, and then `pass_two`'s
//! live-range table, [`crate::live`]. A call of a name in
//! [`crate::special`]'s table is served as php's compiler serves it,
//! a constant or an opcode of its own (`ZEND_STRLEN`, `ZEND_TYPE_CHECK`,
//! `ZEND_DEFINED`, `ZEND_COUNT`), where the engine's function table
//! holds the name as a native ([`Options::functions`]); any other call
//! of a name the table holds is `INIT_FCALL`, `SEND_*` and `DO_ICALL`,
//! and a call of a name it has not is `zend_compile_dynamic_call`'s
//! `INIT_FCALL_BY_NAME` over the name's constant pair, `SEND_*_EX` and
//! `DO_FCALL_BY_NAME`, for the engine to resolve as the program serves.
//! Between the two, a call whose count one of the native's frameless
//! entries serves is `zend_compile_frameless_icall`'s
//! `FRAMELESS_ICALL_<n>` over the arguments, a third in an `OP_DATA`,
//! with the engine's table row in `extended_value`; and an unqualified
//! name in a namespace is `zend_compile_ns_call`'s call by name,
//! `INIT_NS_FCALL_BY_NAME` over three constants, in a `JMP_FRAMELESS`
//! branch beside the frameless call where the global function has an
//! entry (#254).
//!
//! A variable read is a `CV` operand and no opline (`zend_try_compile_cv`),
//! and an assignment to one is `ASSIGN` with the CV in `op1` and the value
//! in `op2` (`zend_compile_assign`), the CV looked up before the value is
//! compiled, which is what puts `$a` before `$b` in `$a = f($b)`. `++$a`
//! and `$a++` are `PRE_INC` and `POST_INC` over the CV
//! (`zend_compile_pre_incdec`, `zend_compile_post_incdec`), and a
//! `POST_INC` whose value nothing reads becomes the `PRE_INC` php's
//! `zend_do_free` turns it into, so `$i++` as a loop's step is php's
//! `PRE_INC CV0($i)`. `@expr` is `zend_compile_silence`'s:
//! `BEGIN_SILENCE` into a temporary, the expression, and `END_SILENCE`
//! over that temporary; a variable under it is fetched by name, php's
//! `zend_compile_simple_var_no_cv`, a `FETCH_R (local)` over the name's
//! constant into a temporary, so the read of an unset variable is inside
//! the silenced span and no CV is made for it. What an opline leaves is written in its
//! `out_type` as `zend_emit_op` and `zend_emit_op_tmp` write
//! `result_type`, `VAR` for what a call returns and `TMP` for what an
//! operator or an assignment leaves; a value nothing reads is
//! `zend_do_free`'s: the mark `UNUSED` on the opline that left it where
//! php sets one, and a `FREE` otherwise.
//!
//! A loop is emitted as `zend_compile_for` and `zend_compile_while` emit
//! one: a `JMP` forward over the body to the condition, and the condition
//! as a `JMPNZ` back to the body's first opline, which is the back-edge
//! and carries its safepoint number (design §5.7). php folds nothing
//! here, so `for (;;)` is a `JMPNZ` over the constant `true` and not a
//! `JMP`, which is the shape the compile-identity lane holds this to.
//! A `foreach` is `zend_compile_foreach`'s: `FE_RESET_R` over the
//! subject into a `VAR`, `FE_FETCH_R` over that `VAR` with the value's
//! CV in `op2`, the key into a temporary the `ASSIGN` after it reads,
//! the body, the `JMP` back to the `FE_FETCH_R`, which is the back-edge,
//! and the `FE_FREE` where the loop ends, which is where the reset's
//! `op2` and the fetch's `extended_value` point; `FE_RESET_RW` and
//! `FE_FETCH_RW` where the value is taken by reference. The `FE_FREE`
//! is the loop's variable, which a `break` or `continue` out past the
//! loop and a `return` inside it emit marked `ZEND_FREE_ON_RETURN`
//! (`zend_handle_loops_and_finally_ex`), and the live-range walk reads
//! that mark ([`crate::live`]). A `return` is `zend_compile_return`'s:
//! the value, or the constant `null`, after every open loop's variable
//! is freed, and `RETURN` carrying its safepoint number. An array
//! expression whose every element is constant is `zend_try_ct_eval_array`'s
//! one constant array with php's keys; one with a variable in it is
//! `INIT_ARRAY` over the first element and `ADD_ARRAY_ELEMENT` over each
//! after it, as `zend_compile_array` emits them.

use std::cmp::Ordering;

use php_heap::{
    Fatal, Intern, Key, Number, Scalar, Zval, ZvalType, double_to_string, numeric_prefix,
};
use php_unit::decl::{
    ClassConstDecl, ClassDecl, ClassFlags, ClassKind, ConstDecl, ConstExpr, EnumBacking,
    FunctionDecl, MethodDecl, Name, Parameter, PropertyDecl, Type, Visibility,
};
use php_unit::{
    Diagnostic, FramelessEntry, Level, MethodBody, OpArray, Opcode, OperandKind, Opline, OplineIdx,
    TryCatch, Unit, frameless_table,
};

use crate::read::{
    Arm, BinaryOp, Case, CastTo, Catch, Class, Element, Expr, Function, Magic, Method, Piece, Read,
    Stmt, Target,
};
use crate::{CompileError, Options, StackLimit};

/// `ZEND_FETCH_LOCAL` (`Z/Zend/zend_compile.h:1083`), the
/// `extended_value` of a `FETCH_*` by name that is no auto-global, which
/// the dump prints as `(local)`.
const FETCH_LOCAL: u32 = 1 << 2;

/// `IS_VOID` (`Z/Zend/zend_types.h:637`), the type code `void` carries
/// in a declared type's mask.
const VOID_CODE: u8 = 14;

/// `IS_NEVER` (`Z/Zend/zend_types.h:640`).
const NEVER_CODE: u8 = 17;

/// `ZEND_RETURNS_FUNCTION` (`Z/Zend/zend_compile.h:1198`), on a
/// `RETURN_BY_REF` returning what a call returned.
const RETURNS_FUNCTION: u32 = 1 << 0;

/// `ZEND_RETURNS_VALUE` (`Z/Zend/zend_compile.h:1199`), on a
/// `RETURN_BY_REF` returning what is no variable.
const RETURNS_VALUE: u32 = 1 << 1;

/// `sizeof(zval)`, sixteen bytes on php's every target, which
/// `zend_compile_rope_finalize` divides a rope's string pointers into
/// to reserve its temporaries.
const ZVAL_SIZE: usize = 16;

/// One op array under construction, and the home its constant strings
/// are interned into (design §5.2): the engine's table when the unit is
/// bound for the unit cache, the request's own otherwise, and the
/// compiler never learns which (`Z/Zend/zend_compile.c`'s
/// `zend_add_literal_string`, which calls `zend_new_interned_string`
/// through whichever handler is installed).
pub(crate) struct Emit<'a> {
    strings: &'a mut dyn Intern,
    /// php's `precision` while the file compiles, [`Options::precision`].
    precision: i32,
    /// The engine's persistent constants, [`Options::persistent`].
    persistent: &'a [ConstDecl],
    /// The engine's natives, [`Options::functions`].
    functions: &'a [FunctionDecl],
    /// The engine's classes, [`Options::classes`].
    known_classes: &'a [ClassDecl],
    /// `zend_flf_functions` over them, what a `FRAMELESS_ICALL_*`'s
    /// `extended_value` indexes (`find_frameless_function_offset`).
    frameless: Vec<FramelessEntry>,
    /// `FC(current_namespace)`: the namespace a `namespace` statement
    /// put what follows it in, and none before one.
    namespace: Option<Vec<u8>>,
    /// `CG(active_class_entry)`: the class whose members are being
    /// compiled, by name, and none outside a class body.
    active_class: Option<Vec<u8>>,
    /// `CG(active_op_array)->function_name`: the function or method whose
    /// body is being compiled, by name, and none in the main op array,
    /// which `__FUNCTION__` and `__METHOD__` fold to.
    active_function: Option<Vec<u8>>,
    /// `this_guaranteed_exists`: whether the op array being compiled is
    /// an instance method's, whose `$this` is the frame's own and needs
    /// no `FETCH_THIS` as a receiver.
    this_in_scope: bool,
    /// `CG(active_op_array)->arg_info[-1]` under `ZEND_ACC_HAS_RETURN_TYPE`:
    /// the return type written after the parameters, which every
    /// `return` in the body is checked against, and none where the
    /// function declares one.
    return_type: Option<Type>,
    /// `ZEND_ACC_RETURN_REFERENCE`: whether the op array being compiled
    /// was written `function &f()`, which makes every `return` in it a
    /// `RETURN_BY_REF`.
    returns_by_ref: bool,
    /// How many statement lists stand open: one inside the file's own,
    /// which is where `zend_compile_top_stmt` binds a function at compile
    /// time; more inside a block, a loop or a body.
    depth: u32,
    /// `CG(context).active_property_info_name`: the property whose
    /// default is being compiled, which `__PROPERTY__` folds to.
    active_property: Option<Vec<u8>>,
    /// `CG(context).in_jmp_frameless_branch`: whether a
    /// `JMP_FRAMELESS` branch is being compiled, in which no call takes
    /// a branch of its own.
    in_jmp_frameless_branch: bool,
    /// How many statements have been compiled, which is what a first
    /// `namespace` statement must find at zero.
    statements_seen: u32,
    /// The thread's stack limit, [`Options::stack`], held to where the
    /// emitter recurses.
    stack: StackLimit,
    oplines: Vec<Opline>,
    constants: Vec<Zval>,
    vars: Vec<Name>,
    safepoints: Vec<OplineIdx>,
    temporaries: u32,
    cache_slots: u32,
    line: u32,
    /// What `zend_error` raised so far, [`Unit::diagnostics`].
    diagnostics: Vec<Diagnostic>,
    /// The classes the file declares, [`Unit::classes`], in the order
    /// `zend_compile_class_decl` bound them; the last is the one whose
    /// body is being compiled while `active_class` names it.
    classes: Vec<ClassDecl>,
    /// The functions the file declares with their bodies,
    /// [`Unit::functions`]: `CG(function_table)` past the natives, each
    /// entry landing once its body has compiled, which is when php's
    /// `ZEND_ACC_DONE_PASS_TWO` lets a call site know it.
    functions_declared: Vec<(FunctionDecl, OpArray)>,
    /// The functions the file declares that no name binds while it
    /// compiles, [`Unit::dynamic`]: php's `dynamic_func_defs` over every
    /// op array of the file, in the order `zend_add_dynamic_func_def`
    /// made them. A slot is taken before the body compiles, as php takes
    /// its offset there, and filled when it has.
    dynamic: Vec<Option<(FunctionDecl, OpArray)>>,
    /// `op_array->dynamic_func_defs` of the op array being compiled:
    /// which entries of [`Emit::dynamic`] it declares, in the order a
    /// `DECLARE_FUNCTION`'s `op2` indexes them.
    dynamic_func_defs: Vec<u32>,
    /// The method bodies of `classes`, [`Unit::methods`].
    methods: Vec<MethodBody>,
    /// The file's name, [`Options::filename`], what a declaration
    /// records as its site.
    filename: Name,
    /// `CG(context).brk_cont_array`: every loop and `switch` of the op
    /// array, in the order begun, and `current_brk_cont`, the innermost
    /// one open.
    loops: Vec<Loop>,
    current_loop: Option<usize>,
    /// The `ZEND_BRK` and `ZEND_CONT` oplines, which `pass_two` turns
    /// into the `JMP` each is once every loop's end is known: the opline,
    /// the loop it was written in, its depth and whether it breaks.
    breaks: Vec<Pending>,
    /// `op_array->try_catch_array`: every `try` of the op array in the
    /// order `zend_add_try_element` added them, which is the order their
    /// `try_op`s stand in.
    try_catch: Vec<TryCatch>,
    /// `CG(loop_var_stack)`, the finally half: what a `return` out of
    /// here must emit before the `RETURN`, innermost last
    /// (`zend_handle_loops_and_finally_ex`). The loop half is [`Emit::loops`],
    /// pushed and popped beside it, so an entry records how many loops
    /// stood open under it and the walk interleaves the two again.
    unwind: Vec<Unwind>,
    /// `CG(context).fast_call_var`: the temporary the innermost
    /// `finally` keeps its return address and its delayed throwable in,
    /// and none outside one.
    fast_call_var: Option<u32>,
    /// `CG(context).try_catch_offset`: which element of [`Emit::try_catch`]
    /// the oplines being emitted stand in, `-1` outside every `try`.
    try_catch_offset: Option<u32>,
}

/// One entry of `CG(loop_var_stack)` a `finally` puts there
/// (`zend_compile_try`): the `FAST_CALL` that stands while the `try` and
/// its `catch`es compile, and the `DISCARD_EXCEPTION` that stands while
/// the `finally` block itself does. `loops` is how many loops of this op
/// array stood open when it was pushed, which is where it falls among
/// them on the way out.
#[derive(Clone, Copy)]
enum Unwind {
    FastCall {
        var: u32,
        try_catch_offset: u32,
        loops: u32,
    },
    DiscardException {
        var: u32,
        loops: u32,
    },
}

impl Unwind {
    /// How many loops stood open under this entry.
    fn loops(self) -> u32 {
        match self {
            Unwind::FastCall { loops, .. } | Unwind::DiscardException { loops, .. } => loops,
        }
    }
}

/// `zend_brk_cont_element`: one loop or `switch`, the loop it stands in,
/// where `break` and `continue` out of it land, and the temporary a
/// `switch` holds its subject in, which a `break` out past it frees
/// (`zend_loop_var`, the `ZEND_FREE` of `zend_handle_loops_and_finally_ex`).
struct Loop {
    parent: Option<usize>,
    is_switch: bool,
    brk: u32,
    cont: u32,
    var: Option<LoopVar>,
    /// The safepoint number of the loop's header where `continue` lands
    /// on it by a backward jump, a `foreach`'s `FE_FETCH_*`; a `for`, a
    /// `while` and a `switch` are continued by a forward jump, which is
    /// no site (design §5.7).
    header: Option<u32>,
}

/// `zend_loop_var` where a loop has one: the opcode that frees it,
/// `ZEND_FREE` for a `switch` subject and `ZEND_FE_FREE` for a
/// `foreach`'s iterator, and the operand it frees.
#[derive(Clone, Copy)]
struct LoopVar {
    opcode: Opcode,
    kind: OperandKind,
    var: u32,
}

impl LoopVar {
    /// `zend_begin_loop`'s test: a loop has a variable to free only where
    /// its `loop_var` is a `VAR` or a `TMP`; a constant or a CV is nobody's.
    fn of(opcode: Opcode, var: Where) -> Option<LoopVar> {
        let (kind, var) = match var {
            Where::Temporary(at) => (OperandKind::Tmp, at),
            Where::Returned(at) => (OperandKind::Var, at),
            Where::Constant(_) | Where::Cv(_) => return None,
        };
        Some(LoopVar { opcode, kind, var })
    }
}

/// One `break` or `continue` before `pass_two` resolves it.
struct Pending {
    at: u32,
    r#loop: usize,
    depth: u32,
    is_break: bool,
}

/// The jump table of one `SWITCH_LONG`, `SWITCH_STRING` or `MATCH`
/// while its targets are being learnt: the opline, the constant slot
/// reserved for the array, and the entries so far.
struct Table {
    at: u32,
    slot: u32,
    entries: Vec<(Key, Zval)>,
}

/// `ZEND_FREE_ON_RETURN`, the bit `zend_compile.h` sets on the `FREE` a
/// `break` or `continue` emits for the subject of each `switch` it
/// leaves, which the live-range walk reads (`crate::live`).
const FREE_ON_RETURN: u32 = 1 << 0;

/// `ZEND_ARRAY_NOT_PACKED` and `ZEND_ARRAY_SIZE_SHIFT`
/// (`Z/Zend/zend_compile.h:1202`): the bit `zend_compile_array` sets on
/// an `INIT_ARRAY` once a constant string key is seen, and where above
/// the two flags it writes the element count.
const ARRAY_NOT_PACKED: u32 = 1 << 1;
const ARRAY_SIZE_SHIFT: u32 = 2;
/// `ZEND_FREE_SWITCH`, the bit `zend_compile.h` sets on the `FREE` of a
/// `switch` or `match` subject at its end.
const FREE_SWITCH: u32 = 1 << 1;

/// `ZEND_LAST_CATCH` (`Z/Zend/zend_compile.h`): the bit
/// `zend_compile_try` sets on the last `CATCH` of a `try`, beside the
/// cache slot, which is a byte offset and so never has it.
const LAST_CATCH: u32 = 1 << 0;

/// Where an expression left its value, php's `znode`: the value itself
/// for a constant, which reaches the table only when the opline that
/// names it is emitted (`zend_add_literal`); a temporary holding what a
/// call returned (php's `IS_VAR`); a temporary an operator wrote
/// (php's `IS_TMP_VAR`); or a CV, the variable itself (php's `IS_CV`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Where {
    Constant(Zval),
    Returned(u32),
    Temporary(u32),
    Cv(u32),
}

/// What a magic constant folds to: `__LINE__`'s number, or the string
/// every other one is.
enum Folded {
    Line(i64),
    Text(Vec<u8>),
}

/// `zend_dirname` (`Z/Zend/zend_compile.c:2176`) on a path that is no
/// Windows one: trailing slashes stripped, then the last component,
/// then the slashes before it; `/` for a path of slashes alone, `.` for
/// one with no slash in it.
fn dirname(path: &[u8]) -> Vec<u8> {
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return if path.is_empty() {
            Vec::new()
        } else {
            b"/".to_vec()
        };
    }
    while end > 0 && path[end - 1] != b'/' {
        end -= 1;
    }
    if end == 0 {
        return b".".to_vec();
    }
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return b"/".to_vec();
    }
    path[..end].to_vec()
}

/// The unit of one source file: its main op array, the statements and
/// then php's implicit final `RETURN 1` (`zend_emit_final_return`, with
/// `extended_value` at -1), and the diagnostics raised on the way.
pub(crate) fn unit(
    read: &Read,
    options: &Options,
    lines: u32,
    strings: &mut dyn Intern,
) -> Result<Unit, CompileError> {
    let mut emit = Emit {
        strings,
        precision: options.precision,
        persistent: &options.persistent,
        functions: &options.functions,
        known_classes: &options.classes,
        frameless: frameless_table(&options.functions),
        namespace: None,
        active_class: None,
        active_function: None,
        this_in_scope: false,
        return_type: None,
        returns_by_ref: false,
        depth: 0,
        active_property: None,
        in_jmp_frameless_branch: false,
        statements_seen: 0,
        stack: options.stack,
        oplines: Vec::new(),
        constants: Vec::new(),
        vars: Vec::new(),
        safepoints: vec![OplineIdx(0)],
        temporaries: 0,
        cache_slots: 0,
        line: 1,
        diagnostics: Vec::new(),
        classes: Vec::new(),
        functions_declared: Vec::new(),
        dynamic: Vec::new(),
        dynamic_func_defs: Vec::new(),
        methods: Vec::new(),
        filename: options.filename.clone(),
        loops: Vec::new(),
        current_loop: None,
        breaks: Vec::new(),
        try_catch: Vec::new(),
        unwind: Vec::new(),
        fast_call_var: None,
        try_catch_offset: None,
    };
    // A fatal leaves the file with what `zend_error` had raised before
    // it already rendered, so those travel with it.
    if let Err(mut refused) = emit.statements(read) {
        refused.raised = emit.diagnostics;
        return Err(refused);
    }
    let main = emit.finish(true, 1, lines.max(1))?;
    let line = emit.line;
    let mut dynamic = Vec::with_capacity(emit.dynamic.len());
    for def in emit.dynamic {
        let Some(def) = def else {
            return Err(CompileError {
                level: Level::Error,
                message: String::from(
                    "InternalError: a dynamic function definition whose body never landed",
                ),
                line,
                raised: emit.diagnostics,
            });
        };
        dynamic.push(def);
    }
    Ok(Unit {
        main,
        functions: emit.functions_declared,
        dynamic,
        methods: emit.methods,
        diagnostics: emit.diagnostics,
        classes: emit.classes,
        ..Unit::default()
    })
}

/// The state of one op array under construction, what `zend_oparray_context_begin`
/// and `CG(active_op_array)` switch: taken off the emitter while a
/// function's body compiles and put back after it, so the body's
/// oplines, constants, variables, safepoints, temporaries, cache slots
/// and loops are its own.
struct Saved {
    oplines: Vec<Opline>,
    constants: Vec<Zval>,
    vars: Vec<Name>,
    safepoints: Vec<OplineIdx>,
    temporaries: u32,
    cache_slots: u32,
    loops: Vec<Loop>,
    current_loop: Option<usize>,
    breaks: Vec<Pending>,
    try_catch: Vec<TryCatch>,
    unwind: Vec<Unwind>,
    fast_call_var: Option<u32>,
    try_catch_offset: Option<u32>,
    active_function: Option<Vec<u8>>,
    this_in_scope: bool,
    return_type: Option<Type>,
    returns_by_ref: bool,
    dynamic_func_defs: Vec<u32>,
}

/// `pass_two`'s `ZEND_FAST_CALL` arm (`Z/Zend/zend_opcode.c:1150`):
/// every `FAST_CALL` carries the number of its `try_catch_array`
/// element in `op1` until here, and the first opline of that element's
/// `finally` block from here, which is where the handler jumps.
fn resolve_fast_calls(oplines: &mut [Opline], try_catch: &[TryCatch]) {
    for op in oplines {
        if op.opcode != Opcode::FastCall {
            continue;
        }
        if let Some(element) = try_catch.get(op.op1 as usize) {
            op.op1 = element.finally_op.0;
        }
    }
}

/// Design §5.7's assertion over one op array, before it is a unit:
/// every backward jump other than `FAST_RET`, a jump whose target is at
/// or before itself, is an opcode with a `safepoint` column, and the
/// entry of `safepoints` its `out` names is its target. The failure is
/// this compiler's and never php's, so it is an `InternalError` fatal
/// with the opline in its message (design §10), not a panic.
fn headers_numbered(oplines: &[Opline], safepoints: &[OplineIdx]) -> Result<(), CompileError> {
    for (at, op) in oplines.iter().enumerate() {
        if op.opcode == Opcode::FastRet {
            continue;
        }
        let Some(target) = op.jump_target() else {
            continue;
        };
        if target.0 as usize > at {
            continue;
        }
        let refused = |what: &str| CompileError {
            level: Level::Error,
            message: format!(
                "InternalError: {} at opline {at} jumps back to {:04} {what}",
                op.opcode.name().trim_start_matches("ZEND_"),
                target.0
            ),
            line: op.lineno,
            raised: Vec::new(),
        };
        if !op.opcode.is_safepoint_site() {
            return Err(refused("with no safepoint column for its number"));
        }
        if safepoints.get(op.out as usize) != Some(&target) {
            return Err(refused(&format!(
                "carrying safepoint {}, which is not that header",
                op.out
            )));
        }
    }
    Ok(())
}

impl Emit<'_> {
    fn push(&mut self, opline: Opline) -> u32 {
        let at = self.next();
        self.oplines.push(Opline {
            lineno: self.line,
            ..opline
        });
        at
    }

    /// The slot holding `value`, the same value twice keeping one slot.
    fn constant(&mut self, value: Zval) -> u32 {
        if let Some(at) = self.constants.iter().position(|held| *held == value) {
            return u32::try_from(at).unwrap_or(0);
        }
        let at = u32::try_from(self.constants.len()).unwrap_or(u32::MAX);
        self.constants.push(value);
        at
    }

    /// `zend_add_literal` as it stands: a slot of its own for `value`,
    /// whether or not the table holds the value already, which a name's
    /// constant pair needs where the name is its own lowered form.
    fn literal(&mut self, value: Zval) -> u32 {
        let at = u32::try_from(self.constants.len()).unwrap_or(u32::MAX);
        self.constants.push(value);
        at
    }

    fn temporary(&mut self) -> u32 {
        let at = self.temporaries;
        self.temporaries = self.temporaries.saturating_add(1);
        at
    }

    /// `lookup_cv`: the CV holding the variable of this name, which is
    /// the next one when no opline has named it yet. php compares the
    /// name's bytes, so `$a` and `$A` are two CVs.
    fn lookup_cv(&mut self, name: &[u8]) -> u32 {
        if let Some(at) = self.vars.iter().position(|held| &*held.0 == name) {
            return u32::try_from(at).unwrap_or(u32::MAX);
        }
        let at = u32::try_from(self.vars.len()).unwrap_or(u32::MAX);
        self.vars.push(Name(name.into()));
        at
    }

    /// `zend_alloc_cache_slot`: the next slot of the op array's cache, as
    /// an offset in bytes, one pointer per slot (`sizeof(void*)`), which
    /// is the number an opline carries and the dump prints for a
    /// `JMP_FRAMELESS`.
    pub(crate) fn cache_slot(&mut self) -> u32 {
        self.cache_slots(1)
    }

    /// `zend_alloc_cache_slots(count)`: the first of `count` slots in a
    /// row, as an offset in bytes, `op_array->cache_size` grown by
    /// `count * sizeof(void*)`.
    fn cache_slots(&mut self, count: u32) -> u32 {
        const SLOT: u32 = 8;
        let at = self.cache_slots;
        self.cache_slots = self.cache_slots.saturating_add(count.saturating_mul(SLOT));
        at
    }

    /// The number of a new safepoint at `opline`; the entry is zero and
    /// is already there.
    fn safepoint(&mut self, at: u32) -> u32 {
        let number = u32::try_from(self.safepoints.len()).unwrap_or(0);
        self.safepoints.push(OplineIdx(at));
        number
    }

    /// The compile-time fatal for what stands on the statement's line:
    /// this compiler's sentence for what it cannot read yet, which the
    /// engine renders as the parse error #163 framed it as.
    fn fatal(&self, message: impl Into<String>) -> CompileError {
        CompileError {
            level: Level::Parse,
            message: message.into(),
            line: self.line,
            raised: Vec::new(),
        }
    }

    /// `zend_check_stack_limit`, at the top of `zend_compile_expr` and
    /// `zend_eval_const_expr`: php's `E_COMPILE_ERROR` once the frame
    /// stands past the thread's limit.
    fn check_stack(&self) -> Result<(), CompileError> {
        self.stack.check(self.line)
    }

    /// [`Self::constant_expr`] asked for a fold: the value where the
    /// expression is a constant one, nothing where it is not, and the
    /// fatal through, since php's compiler leaves the file at
    /// `E_COMPILE_ERROR` and folds nothing after it.
    pub(crate) fn constant_of(&mut self, expr: &Expr) -> Result<Option<Zval>, CompileError> {
        match self.constant_expr(expr) {
            Ok(value) => Ok(Some(value)),
            Err(refused) if refused.level == Level::CompileError => Err(refused),
            Err(_) => Ok(None),
        }
    }

    /// `zend_error` under `CG(in_compilation)` for what is not a fatal:
    /// the diagnostic is recorded on the unit at the line
    /// `zend_get_compiled_lineno` answers (`get_filename_lineno`,
    /// `Z/Zend/zend.c:1640`), for the engine to raise before the op
    /// array runs, which is where php writes it.
    fn raise(&mut self, level: Level, message: &str) {
        self.diagnostics.push(Diagnostic {
            level,
            line: self.line,
            message: message.into(),
        });
    }

    /// The constant string for these bytes, interned into whichever home
    /// this compilation was given: php's `zend_new_interned_string` over
    /// the string a `CONST` operand holds. A home with no memory left
    /// refuses, and the refusal is the compile-time fatal php raises
    /// where it runs out (`Z/Zend/zend_alloc.c:1059`).
    pub(crate) fn string(&mut self, bytes: &[u8]) -> Result<Zval, CompileError> {
        match self.strings.intern(bytes) {
            Ok(s) => Ok(Zval::string(s)),
            Err(Fatal::Error(message)) => Err(self.fatal(message)),
            Err(Fatal::OutOfMemory(_)) => Err(self.fatal("Out of memory")),
        }
    }

    fn statement(&mut self, statement: &Stmt) -> Result<(), CompileError> {
        if let Stmt::Namespace(name) = statement {
            return self.namespace(name);
        }
        self.statements_seen = self.statements_seen.saturating_add(1);
        match statement {
            Stmt::Namespace(_) => {}
            Stmt::Inline(text) => {
                let text = self.string(text)?;
                let at = self.constant(text);
                self.push(Opline {
                    opcode: Opcode::Echo,
                    op1_type: OperandKind::Const,
                    op1: at,
                    ..Opline::default()
                });
            }
            Stmt::Echo(parts) => {
                for part in parts {
                    let value = self.expr(part)?;
                    let (op1_type, op1) = self.operand(value);
                    self.push(Opline {
                        opcode: Opcode::Echo,
                        op1_type,
                        op1,
                        ..Opline::default()
                    });
                }
            }
            Stmt::Effect(expr) => {
                let value = self.expr(expr)?;
                self.do_free(value);
            }
            Stmt::Block(body) => self.statements(body)?,
            Stmt::For {
                init,
                cond,
                step,
                body,
            } => self.r#for(init, cond, step, body)?,
            Stmt::While { cond, body } => self.r#while(cond, body)?,
            Stmt::Foreach {
                expr,
                value,
                key,
                body,
            } => self.foreach(expr, value, key.as_ref(), body)?,
            Stmt::Return(value) => self.r#return(value.as_ref())?,
            Stmt::Switch { expr, cases } => self.switch(expr, cases)?,
            Stmt::Break(depth) => self.break_continue(depth.as_ref(), true)?,
            Stmt::Continue(depth) => self.break_continue(depth.as_ref(), false)?,
            Stmt::Class(class) => self.class(class)?,
            Stmt::Function(function) => self.function_decl(function)?,
            Stmt::Try {
                body,
                catches,
                finally,
            } => self.r#try(body, catches, finally.as_ref())?,
        }
        Ok(())
    }

    /// `zend_compile_try` (`Z/Zend/zend_compile.c:6735`): one element of
    /// `try_catch_array` opened at the body's first opline; the body;
    /// a `JMP` over the `catch` clauses where any stands; one `CATCH` per
    /// class named, each clause's last `CATCH` jumping to the next
    /// clause's first, and the last clause's carrying `ZEND_LAST_CATCH`
    /// and jumping nowhere, which is where the throwable is rethrown;
    /// then, where a `finally` is written, the `FAST_CALL` into the
    /// block, the `JMP` over it, the block between `finally_op` and
    /// `finally_end`, and the `FAST_RET` that ends it.
    ///
    /// `try` with neither a `catch` nor a `finally` is php's compile
    /// error in php's words.
    ///
    /// ```php
    /// try { throw new Exception("x"); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// try { echo "a\n"; } finally { echo "b\n"; }
    /// try { throw new TypeError("t"); } catch (ValueError $e) { echo "no\n"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; } finally { echo "f\n"; }
    /// ```
    /// ```expect
    /// x
    /// a
    /// b
    /// t
    /// f
    /// ```
    fn r#try(
        &mut self,
        body: &Read,
        catches: &[Catch],
        finally: Option<&(Read, u32)>,
    ) -> Result<(), CompileError> {
        if catches.is_empty() && finally.is_none() {
            return Err(self.compile_error("Cannot use try without catch or finally"));
        }
        let offset = u32::try_from(self.try_catch.len()).unwrap_or(u32::MAX);
        self.try_catch.push(TryCatch {
            try_op: OplineIdx(self.next()),
            ..TryCatch::default()
        });
        let saved_fast_call = self.fast_call_var;
        let saved_offset = self.try_catch_offset;
        if finally.is_some() {
            let var = self.temporary();
            self.fast_call_var = Some(var);
            self.unwind.push(Unwind::FastCall {
                var,
                try_catch_offset: offset,
                loops: self.loops_open(),
            });
        }
        self.try_catch_offset = Some(offset);
        self.statements(body)?;
        let mut over_catches = Vec::new();
        if !catches.is_empty() {
            over_catches.push(self.jump_forward());
        }
        for (i, clause) in catches.iter().enumerate() {
            let last_catch = i + 1 == catches.len();
            self.line = clause.line;
            if i == 0 {
                let at = OplineIdx(self.next());
                if let Some(element) = self.try_catch.get_mut(offset as usize) {
                    element.catch_op = at;
                }
            }
            let opnum_catch = self.catch_clause(clause, last_catch)?;
            self.statements(&clause.body)?;
            if !last_catch {
                over_catches.push(self.jump_forward());
                let next = self.next();
                if let Some(op) = self.oplines.get_mut(opnum_catch as usize) {
                    op.op2 = next;
                }
            }
        }
        for jump in over_catches {
            self.update_jump_target_to_next(jump);
        }
        if let Some((block, line)) = finally {
            self.finally_block(block, *line, offset, saved_offset)?;
            self.fast_call_var = saved_fast_call;
        }
        self.try_catch_offset = saved_offset;
        Ok(())
    }

    /// One `catch (A|B $e)` clause's `CATCH` oplines, `zend_compile_try`'s
    /// inner loop: one per class named, each with the class's constant
    /// pair in `op1` and a cache slot in `extended_value`, the throwable
    /// bound into the clause's CV where one is written and released where
    /// none is (`RETURN_VALUE_USED`). A `CATCH` that is not the last
    /// class of its clause jumps over the rest to the clause's body.
    /// Returns the clause's last `CATCH`, whose `op2` is the next
    /// clause's first and is left unset on the last clause of the `try`.
    fn catch_clause(&mut self, clause: &Catch, last_catch: bool) -> Result<u32, CompileError> {
        if let Some(name) = &clause.var
            && name == b"this"
        {
            return Err(self.compile_error("Cannot re-assign $this"));
        }
        let mut multicatch = Vec::new();
        let mut opnum_catch = self.next();
        let count = clause.types.len();
        for (j, class) in clause.types.iter().enumerate() {
            let last_class = j + 1 == count;
            let class_at = self.class_name_literal(class)?;
            let slot = self.cache_slot();
            let (out_type, out) = match &clause.var {
                Some(name) => (OperandKind::Cv, self.lookup_cv(name)),
                None => (OperandKind::Unused, u32::MAX),
            };
            let extended = if last_catch && last_class {
                slot | LAST_CATCH
            } else {
                slot
            };
            opnum_catch = self.push(Opline {
                opcode: Opcode::Catch,
                op1_type: OperandKind::Const,
                op1: class_at,
                out_type,
                out,
                extended_value: extended,
                ..Opline::default()
            });
            if !last_class {
                multicatch.push(self.jump_forward());
                let next = self.next();
                if let Some(op) = self.oplines.get_mut(opnum_catch as usize) {
                    op.op2 = next;
                }
            }
        }
        for jump in multicatch {
            self.update_jump_target_to_next(jump);
        }
        Ok(opnum_catch)
    }

    /// `zend_compile_try`'s `finally` half: the `FAST_CALL` into the
    /// block, whose `result` is the fast-call temporary and whose `op1`
    /// `pass_two` turns from the element's number into the block's first
    /// opline; the `JMP` over the block; the block between `finally_op`
    /// and `finally_end`; and the `FAST_RET` that returns to whoever
    /// called it, carrying the enclosing `try`'s number in `op2`. The
    /// `DISCARD_EXCEPTION` marker stands on the unwind stack while the
    /// block compiles, in the `FAST_CALL`'s place.
    fn finally_block(
        &mut self,
        block: &Read,
        line: u32,
        offset: u32,
        enclosing: Option<u32>,
    ) -> Result<(), CompileError> {
        let Some(var) = self.fast_call_var else {
            return Err(self.fatal("InternalError: a `finally` with no fast-call temporary"));
        };
        self.unwind.pop();
        self.unwind.push(Unwind::DiscardException {
            var,
            loops: self.loops_open(),
        });
        self.line = line;
        self.push(Opline {
            opcode: Opcode::FastCall,
            op1: offset,
            out_type: OperandKind::Tmp,
            out: var,
            ..Opline::default()
        });
        let opnum_fast_call = self.next().saturating_sub(1);
        let over = self.jump_forward();
        self.statements(block)?;
        let finally_op = OplineIdx(opnum_fast_call.saturating_add(2));
        let finally_end = OplineIdx(self.next());
        if let Some(element) = self.try_catch.get_mut(offset as usize) {
            element.finally_op = finally_op;
            element.finally_end = finally_end;
        }
        self.push(Opline {
            opcode: Opcode::FastRet,
            op1_type: OperandKind::Tmp,
            op1: var,
            op2: enclosing.unwrap_or(u32::MAX),
            ..Opline::default()
        });
        self.update_jump_target_to_next(over);
        self.unwind.pop();
        Ok(())
    }

    /// `zend_emit_jump(0)`: a `JMP` whose target is learnt later.
    fn jump_forward(&mut self) -> u32 {
        self.push(Opline {
            opcode: Opcode::Jmp,
            ..Opline::default()
        })
    }

    /// How many loops of this op array stand open, which is where an
    /// unwind entry pushed now falls among them.
    fn loops_open(&self) -> u32 {
        let mut count = 0;
        let mut at = self.current_loop;
        while let Some(index) = at {
            count += 1;
            at = self.loops.get(index).and_then(|open| open.parent);
        }
        count
    }

    /// `zend_compile_func_decl` for a function at the top level of the
    /// file, `toplevel` in `zend_compile_top_stmt`: its body compiles to
    /// an op array of its own and the declaration goes into the unit's
    /// functions, since php binds such a function at compile time
    /// (`zend_begin_func_decl`'s `zend_hash_add_ptr(CG(function_table), …)`)
    /// and emits no opline for it. The name is `zend_prefix_with_ns`
    /// over the written one. A name the table holds is
    /// `do_bind_function_error`, php's `E_COMPILE_ERROR` in its two
    /// wordings, the earlier declaration's file and first line named
    /// when it was a user function. A function declared inside a block,
    /// a loop or another body is [`Self::nested_function_decl`].
    fn function_decl(&mut self, function: &Function) -> Result<(), CompileError> {
        let name = self.prefix_with_ns(&function.name);
        if self.depth != 1 {
            return self.nested_function_decl(function, &name);
        }
        let lowered = name.to_ascii_lowercase();
        if let Some((earlier, body)) = self
            .functions_declared
            .iter()
            .find(|(decl, _)| decl.name.0.to_ascii_lowercase() == lowered)
        {
            return Err(self.compile_error(format!(
                "Cannot redeclare function {} (previously declared in {}:{})",
                String::from_utf8_lossy(&name) + "()",
                String::from_utf8_lossy(&earlier.file.0),
                body.line_start
            )));
        }
        if self.native_index(&lowered).is_some() {
            return Err(self.compile_error(format!(
                "Cannot redeclare function {}()",
                String::from_utf8_lossy(&name)
            )));
        }
        let declared = self.body(function, &name, None, false)?;
        self.functions_declared.push(declared);
        Ok(())
    }

    /// `zend_compile_func_decl` with `toplevel` false, which is every
    /// function php meets below the file's own statement list: inside an
    /// `if`, a loop, a `try` or another function's body.
    /// `zend_begin_func_decl` binds no name while the file compiles.
    /// Instead `zend_add_dynamic_func_def` takes the next offset of the
    /// enclosing op array's `dynamic_func_defs` and a `DECLARE_FUNCTION`
    /// opline stands where the declaration was written, the lowered name
    /// in `op1` and that offset in `op2`, so the binding happens as the
    /// program serves and only where the statement runs, which is the
    /// engine's `DECLARE_FUNCTION` handler. The offset is taken before
    /// the body compiles, as php takes it there, so a function inside
    /// this one lands after it in the unit's table.
    ///
    /// php names no redeclaration here: two declarations of one name
    /// compile, and the second `DECLARE_FUNCTION` that runs is
    /// `do_bind_function_error` at `E_ERROR`.
    fn nested_function_decl(
        &mut self,
        function: &Function,
        name: &[u8],
    ) -> Result<(), CompileError> {
        let at = u32::try_from(self.dynamic.len()).unwrap_or(u32::MAX);
        self.dynamic.push(None);
        let offset = u32::try_from(self.dynamic_func_defs.len()).unwrap_or(u32::MAX);
        self.dynamic_func_defs.push(at);
        let lowered = self.string(&name.to_ascii_lowercase())?;
        let op1 = self.literal(lowered);
        self.push(Opline {
            opcode: Opcode::DeclareFunction,
            op1_type: OperandKind::Const,
            op1,
            op2: offset,
            ..Opline::default()
        });
        let declared = self.body(function, name, None, false)?;
        if let Some(slot) = self.dynamic.get_mut(at as usize) {
            *slot = Some(declared);
        }
        Ok(())
    }

    /// `zend_declare_typed_property` for each promoted parameter of a
    /// constructor (`zend_compile_params`): the property carries the
    /// parameter's visibility, its `readonly` bit and its declared type,
    /// and no explicit default, which for a typed one is uninitialized
    /// and for an untyped one the implicit `null`. A name the class
    /// already declares is php's `Cannot redeclare C::$p`, and a
    /// `readonly` promotion with no type is php's own refusal.
    fn promoted_properties(
        &mut self,
        class: usize,
        class_name: &[u8],
        function: &Function,
    ) -> Result<(), CompileError> {
        for parameter in &function.parameters {
            let Some(promoted) = parameter.promote else {
                continue;
            };
            if self
                .classes
                .get(class)
                .is_some_and(|decl| decl.properties.iter().any(|p| *p.name.0 == *parameter.name))
            {
                return Err(self.compile_error(format!(
                    "Cannot redeclare {}::${}",
                    String::from_utf8_lossy(class_name),
                    String::from_utf8_lossy(&parameter.name)
                )));
            }
            if promoted.is_readonly && parameter.ty.is_none() {
                return Err(self.compile_error(format!(
                    "Readonly property {}::${} must have type",
                    String::from_utf8_lossy(class_name),
                    String::from_utf8_lossy(&parameter.name)
                )));
            }
            let default = parameter.ty.is_none().then_some(ConstExpr::Null);
            let Some(decl) = self.classes.get_mut(class) else {
                return Err(
                    self.fatal("InternalError: a promoted property of a class the unit has not")
                );
            };
            decl.properties.push(PropertyDecl {
                name: Name(parameter.name.clone().into_boxed_slice()),
                visibility: promoted.visibility,
                is_instance: true,
                is_readonly: promoted.is_readonly,
                ty: parameter.ty.clone(),
                default,
                attributes: Vec::new(),
                doc_comment: None,
            });
        }
        Ok(())
    }

    /// `zend_compile_func_decl` for a method of the class being
    /// compiled, at `class` in the unit's classes: the body compiles to
    /// an op array of its own, and the declaration goes into the class's
    /// methods, where `zend_begin_method_decl` adds it to
    /// `ce->function_table`. A name the class already has is php's
    /// `Cannot redeclare C::m()`, `E_COMPILE_ERROR`, spelled as the
    /// second declaration spells it. An instance method's body has
    /// `$this` as its frame's own (`this_guaranteed_exists`).
    fn method_decl(&mut self, class: usize, method: &Method) -> Result<(), CompileError> {
        let class_name = self
            .classes
            .get(class)
            .map(|decl| decl.name.0.to_vec())
            .unwrap_or_default();
        let name = method.function.name.clone();
        if self.classes.get(class).is_some_and(|decl| {
            decl.methods
                .iter()
                .any(|m| m.function.name.0.eq_ignore_ascii_case(&name))
        }) {
            return Err(self.compile_error(format!(
                "Cannot redeclare {}::{}()",
                String::from_utf8_lossy(&class_name),
                String::from_utf8_lossy(&name)
            )));
        }
        let (function, body) =
            self.body(&method.function, &name, Some(&class_name), method.is_static)?;
        self.promoted_properties(class, &class_name, &method.function)?;
        let Some(decl) = self.classes.get_mut(class) else {
            return Err(self.fatal("InternalError: a method of a class the unit has not"));
        };
        let at = u32::try_from(decl.methods.len()).unwrap_or(u32::MAX);
        decl.methods.push(MethodDecl {
            function,
            visibility: method.visibility,
            is_instance: !method.is_static,
            is_abstract: false,
            is_final: method.is_final,
            tentative_return: false,
        });
        self.methods.push(MethodBody {
            class: u32::try_from(class).unwrap_or(u32::MAX),
            method: at,
            body,
        });
        Ok(())
    }

    /// The body of one function or method as its own op array, what
    /// `zend_compile_func_decl` does between `zend_oparray_context_begin`
    /// and `zend_oparray_context_end`: the emitter's op array state set
    /// aside, `zend_compile_params` as the `RECV*` prologue, the entry
    /// safepoint numbered after it, the statements, php's implicit
    /// `RETURN null` at the closing brace's line
    /// (`zend_emit_final_return(0)` under `CG(zend_lineno) =
    /// decl->end_lineno`), and `pass_two`. `scope` is the class a method
    /// belongs to, none for a function; `$this` is in scope in an
    /// instance method's body and nowhere else. The declaration keeps
    /// each parameter's default as the constant expression it was
    /// written as, and the op array is `zend_compile_params`' prologue:
    /// `RECV` numbered from one, or `RECV_INIT` with the default folded
    /// into a constant where one is written, each into the parameter's
    /// CV, which `lookup_cv` must find as the next one (`Redefinition of
    /// parameter $x` where it does not). A default that folds to no
    /// constant, which php keeps as an `IS_CONSTANT_AST`, is not read yet.
    fn body(
        &mut self,
        function: &Function,
        name: &[u8],
        scope: Option<&[u8]>,
        is_static: bool,
    ) -> Result<(FunctionDecl, OpArray), CompileError> {
        let saved = self.begin_body(name, scope.is_some() && !is_static);
        let outcome = self.parameters_and_body(function, name, scope);
        self.end_body(saved);
        outcome
    }

    fn parameters_and_body(
        &mut self,
        function: &Function,
        name: &[u8],
        scope: Option<&[u8]>,
    ) -> Result<(FunctionDecl, OpArray), CompileError> {
        let line = function.line;
        self.line = line;
        self.returns_by_ref = function.by_ref;
        self.return_type.clone_from(&function.return_type);
        let is_ctor = scope.is_some() && name.eq_ignore_ascii_case(b"__construct");
        let parameters = self.recv_prologue(function, is_ctor)?;
        // The entry safepoint is the opline after the prologue (design
        // §5.7), which `begin_body` numbered zero at opline zero; the
        // assignments a promoted parameter makes stand after it, as
        // `zend_compile_params` emits them after every `RECV*`.
        let entry = self.next();
        if let Some(first) = self.safepoints.first_mut() {
            *first = OplineIdx(entry);
        }
        self.promotions(function)?;
        self.statements(&function.body)?;
        self.line = function.end_line;
        let body = self.finish(false, line, function.end_line)?;
        Ok((
            FunctionDecl {
                name: Name(name.into()),
                parameters,
                return_type: function.return_type.clone(),
                returns_by_ref: function.by_ref,
                attributes: Vec::new(),
                doc_comment: function.doc_comment.clone().map(Vec::into_boxed_slice),
                file: self.filename.clone(),
                line,
                end_line: function.end_line,
                frameless: Vec::new(),
            },
            body,
        ))
    }

    /// `zend_compile_params`' first pass, the `RECV*` prologue: one
    /// opline per declared parameter into its CV, which `lookup_cv` must
    /// find as the next one (`Redefinition of parameter $x` where it does
    /// not). `RECV_VARIADIC` for a `...`, `RECV_INIT` with the default
    /// folded into a constant, and `RECV` otherwise, carrying the pure
    /// mask of the declared type in `op2` so the handler's first test is
    /// one `&`, `MAY_BE_ANY` where no type is written. A default that
    /// folds to no constant, which php keeps as an `IS_CONSTANT_AST`, is
    /// not read yet.
    ///
    /// Two rewrites php makes on the way: a default of `null` under a
    /// written type is its PHP 5 spelling of `?T`
    /// (`zend_compile_typename_ex`'s `force_nullable`), and an optional
    /// parameter written before a required one becomes a required one
    /// with no default whatever it was written as, which is why the last
    /// required parameter is found before any opline is emitted.
    fn recv_prologue(
        &mut self,
        function: &Function,
        is_ctor: bool,
    ) -> Result<Vec<Parameter>, CompileError> {
        let last_required = function
            .parameters
            .iter()
            .rposition(|parameter| parameter.default.is_none() && !parameter.variadic);
        let mut parameters = Vec::with_capacity(function.parameters.len());
        let mut variadic_seen = false;
        for (at, parameter) in function.parameters.iter().enumerate() {
            let number = u32::try_from(at).unwrap_or(u32::MAX);
            let cv = self.lookup_cv(&parameter.name);
            if cv != number {
                return Err(self.compile_error(format!(
                    "Redefinition of parameter ${}",
                    String::from_utf8_lossy(&parameter.name)
                )));
            }
            self.parameter_refused(parameter, variadic_seen, is_ctor)?;
            variadic_seen = parameter.variadic;
            // `zend_compile_params` folds a default under
            // `ZEND_COMPILE_NO_PERSISTENT_CONSTANT_SUBSTITUTION`, so one
            // naming a constant stays an `IS_CONSTANT_AST` literal for
            // the engine and for Reflection, which this table has no
            // zval for yet.
            let folded = match &parameter.default {
                Some(expr) => {
                    if names_a_constant(expr) {
                        return Err(self.fatal(
                            "a parameter default naming a constant this compiler cannot read yet",
                        ));
                    }
                    let Some(folded) = self.constant_of(expr)? else {
                        return Err(self.fatal("a parameter default this compiler cannot read yet"));
                    };
                    Some(folded)
                }
                None => None,
            };
            let implicitly_nullable = parameter.promote.is_none()
                && folded
                    .as_ref()
                    .is_some_and(|zval| zval.kind() == ZvalType::Null)
                && parameter
                    .ty
                    .as_ref()
                    .is_some_and(|ty| !ty.contains_code(ZvalType::Null as u8));
            let ty = match (&parameter.ty, implicitly_nullable) {
                (Some(ty), true) => Some(Type::Nullable(Box::new(ty.clone()))),
                (ty, _) => ty.clone(),
            };
            let keeps_default = !parameter.variadic
                && (implicitly_nullable || last_required.is_none_or(|last| at > last));
            let default = if let (Some(folded), true) = (folded, keeps_default) {
                let slot = self.constant(folded);
                self.push(Opline {
                    opcode: Opcode::RecvInit,
                    op1: number.saturating_add(1),
                    op2_type: OperandKind::Const,
                    op2: slot,
                    out_type: OperandKind::Cv,
                    out: cv,
                    ..Opline::default()
                });
                parameter
                    .default
                    .as_ref()
                    .map(|expr| self.const_expr(expr))
                    .transpose()?
            } else {
                self.push(Opline {
                    opcode: if parameter.variadic {
                        Opcode::RecvVariadic
                    } else {
                        Opcode::Recv
                    },
                    op1: number.saturating_add(1),
                    op2: if parameter.variadic {
                        0
                    } else {
                        ty.as_ref().map_or(Type::MAY_BE_ANY, Type::mask)
                    },
                    out_type: OperandKind::Cv,
                    out: cv,
                    ..Opline::default()
                });
                None
            };
            parameters.push(Parameter {
                name: Name(parameter.name.clone().into_boxed_slice()),
                ty,
                by_ref: parameter.by_ref,
                variadic: parameter.variadic,
                default,
                attributes: Vec::new(),
            });
        }
        Ok(parameters)
    }

    /// What `zend_compile_params` refuses about one parameter, in php's
    /// words: a second variadic, a variadic with a default, and a
    /// promotion outside a constructor or on a variadic.
    fn parameter_refused(
        &self,
        parameter: &crate::read::Parameter,
        variadic_seen: bool,
        is_ctor: bool,
    ) -> Result<(), CompileError> {
        if variadic_seen {
            return Err(self.compile_error("Only the last parameter can be variadic"));
        }
        if parameter.variadic && parameter.default.is_some() {
            return Err(self.compile_error("Variadic parameter cannot have a default value"));
        }
        if parameter.promote.is_some() {
            if !is_ctor {
                return Err(
                    self.compile_error("Cannot declare promoted property outside a constructor")
                );
            }
            if parameter.variadic {
                return Err(self.compile_error("Cannot declare variadic promoted property"));
            }
        }
        Ok(())
    }

    /// `zend_compile_params`' second pass: `$this->p = $p` for every
    /// promoted parameter, `ASSIGN_OBJ` over the name with three cache
    /// slots and the CV as its `OP_DATA`, or `ASSIGN_OBJ_REF` where the
    /// parameter is by reference.
    fn promotions(&mut self, function: &Function) -> Result<(), CompileError> {
        for parameter in &function.parameters {
            if parameter.promote.is_none() {
                continue;
            }
            let name = self.string(&parameter.name)?;
            let slot = self.literal(name);
            let slots = self.cache_slots(3);
            self.push(Opline {
                opcode: if parameter.by_ref {
                    Opcode::AssignObjRef
                } else {
                    Opcode::AssignObj
                },
                op2_type: OperandKind::Const,
                op2: slot,
                extended_value: slots,
                ..Opline::default()
            });
            let cv = self.lookup_cv(&parameter.name);
            self.push(Opline {
                opcode: Opcode::OpData,
                op1_type: OperandKind::Cv,
                op1: cv,
                ..Opline::default()
            });
        }
        Ok(())
    }

    /// `zend_oparray_context_begin` with a fresh op array as
    /// `CG(active_op_array)`: the state of the one being compiled set
    /// aside, and the emitter over an empty one whose entry safepoint
    /// stands at opline zero until the prologue is known.
    fn begin_body(&mut self, name: &[u8], this_in_scope: bool) -> Saved {
        Saved {
            return_type: self.return_type.take(),
            returns_by_ref: std::mem::take(&mut self.returns_by_ref),
            oplines: std::mem::take(&mut self.oplines),
            constants: std::mem::take(&mut self.constants),
            vars: std::mem::take(&mut self.vars),
            safepoints: std::mem::replace(&mut self.safepoints, vec![OplineIdx(0)]),
            temporaries: std::mem::take(&mut self.temporaries),
            cache_slots: std::mem::take(&mut self.cache_slots),
            loops: std::mem::take(&mut self.loops),
            current_loop: self.current_loop.take(),
            breaks: std::mem::take(&mut self.breaks),
            try_catch: std::mem::take(&mut self.try_catch),
            unwind: std::mem::take(&mut self.unwind),
            fast_call_var: self.fast_call_var.take(),
            try_catch_offset: self.try_catch_offset.take(),
            active_function: self.active_function.replace(name.to_vec()),
            this_in_scope: std::mem::replace(&mut self.this_in_scope, this_in_scope),
            dynamic_func_defs: std::mem::take(&mut self.dynamic_func_defs),
        }
    }

    /// `zend_oparray_context_end`: the enclosing op array's state back
    /// on the emitter.
    fn end_body(&mut self, saved: Saved) {
        self.oplines = saved.oplines;
        self.constants = saved.constants;
        self.vars = saved.vars;
        self.safepoints = saved.safepoints;
        self.temporaries = saved.temporaries;
        self.cache_slots = saved.cache_slots;
        self.loops = saved.loops;
        self.current_loop = saved.current_loop;
        self.breaks = saved.breaks;
        self.try_catch = saved.try_catch;
        self.unwind = saved.unwind;
        self.fast_call_var = saved.fast_call_var;
        self.try_catch_offset = saved.try_catch_offset;
        self.active_function = saved.active_function;
        self.this_in_scope = saved.this_in_scope;
        self.return_type = saved.return_type;
        self.returns_by_ref = saved.returns_by_ref;
        self.dynamic_func_defs = saved.dynamic_func_defs;
    }

    /// The op array the emitter holds, finished as `zend_compile` and
    /// `zend_compile_func_decl` finish one: php's implicit final return
    /// (`zend_emit_final_return`, `1` for the main op array and `null`
    /// for a function), `pass_two` over the `BRK` and `CONT` oplines,
    /// design §5.7's assertion over the headers, the commutative swap
    /// and the live ranges. The emitter's op array state is spent by
    /// this and read by nothing after it.
    fn finish(
        &mut self,
        return_one: bool,
        line_start: u32,
        line_end: u32,
    ) -> Result<OpArray, CompileError> {
        self.final_return(return_one)?;
        self.resolve_breaks();
        resolve_fast_calls(&mut self.oplines, &self.try_catch);
        headers_numbered(&self.oplines, &self.safepoints)?;
        swap_commutative(&mut self.oplines);
        let mut oplines = std::mem::take(&mut self.oplines);
        let live_ranges = crate::live::live_ranges(&mut oplines, self.temporaries);
        let vars = std::mem::take(&mut self.vars);
        let num_cvs = u32::try_from(vars.len()).unwrap_or(u32::MAX);
        Ok(OpArray {
            oplines: oplines.into_boxed_slice(),
            literals: std::mem::take(&mut self.constants).into_boxed_slice(),
            vars: vars.into_boxed_slice(),
            num_cvs,
            num_tmps: self.temporaries,
            try_catch: std::mem::take(&mut self.try_catch).into_boxed_slice(),
            live_ranges,
            safepoints: std::mem::take(&mut self.safepoints).into_boxed_slice(),
            filename: self.filename.clone(),
            line_start,
            line_end,
            cache_size: self.cache_slots,
            dynamic_func_defs: std::mem::take(&mut self.dynamic_func_defs).into_boxed_slice(),
        })
    }

    /// `zend_compile_class_decl` for a top-level class: the declaration
    /// goes into the unit's classes, and whether an opline is emitted
    /// beside it is php's early-binding question. php binds a class at
    /// compile time (`zend_hash_add_ptr(CG(class_table), …)` under
    /// `toplevel`) when it implements no interface and either has no
    /// parent or names one `CG(class_table)` already holds, which
    /// `zend_try_early_bind` then links; the oracle's dump of
    /// `class M {} echo 1;` is the `ECHO` and the `RETURN` alone, and so
    /// is `class P {} class C extends P {} echo 1;`. Everything else
    /// carries a `ZEND_DECLARE_CLASS` opline where it was written, which
    /// binds the class as the program serves
    /// ([`Self::declare_class`]): `class C extends P {} class P {}`
    /// dumps `DECLARE_CLASS string("c") string("p")` and works, because
    /// `P` went into the table while the file compiled. The class's name is
    /// `zend_prefix_with_ns` over the written one: `A\M` for `class M`
    /// under `namespace A;`, the name written where no namespace stands,
    /// so a fetch [`Self::class_name`] resolves in the same namespace
    /// names it, `__CLASS__` folds to it and php's fatals name it. The
    /// name a second declaration of the same file repeats fails that
    /// bind in php, which then emits a `DECLARE_CLASS` for the runtime
    /// fatal; that opline is not emitted here, so the repeat is refused
    /// as not yet read. A class declared inside a block or a body is
    /// refused as [`Self::function_decl`] refuses a function there: php
    /// emits its `DECLARE_CLASS` in whatever op array it stands in, and
    /// the engine reads only the main one's. The members are
    /// `zend_compile_prop_decl` and `zend_compile_class_const_decl`: a
    /// name declared twice is php's fatal in php's words, at the
    /// declarator's line, and a default or a value is kept as the
    /// constant expression it was written as (design §4.7).
    ///
    /// A parent the table already holds is the early bind, which links
    /// the class before anything runs:
    ///
    /// ```php
    /// class P { const K = 'p'; public $x = 1; }
    /// class C extends P {}
    /// $c = new C;
    /// var_dump(C::K, $c instanceof P);
    /// ```
    /// ```expect
    /// string(1) "p"
    /// bool(true)
    /// ```
    ///
    /// An interface list is never early bound, so the class is declared
    /// by the `DECLARE_CLASS` where it was written:
    ///
    /// ```php
    /// interface I { const J = 3; }
    /// class C implements I {}
    /// var_dump(C::J, (new C) instanceof I);
    /// ```
    /// ```expect
    /// int(3)
    /// bool(true)
    /// ```
    fn class(&mut self, class: &Class) -> Result<(), CompileError> {
        if self.depth != 1 {
            return Err(self
                .fatal("a class declared inside a block or a body this compiler cannot read yet"));
        }
        let name = self.prefix_with_ns(&class.name);
        let wanted = name.to_ascii_lowercase();
        if self
            .classes
            .iter()
            .any(|declared| declared.name.0.to_ascii_lowercase() == wanted)
        {
            return Err(self.fatal("a class declared twice this compiler cannot read yet"));
        }
        let line = self.line;
        let parent = match &class.parent {
            Some(written) => Some(self.inherited_name(written)?),
            None => None,
        };
        let mut interfaces = Vec::with_capacity(class.interfaces.len());
        for written in &class.interfaces {
            interfaces.push(self.inherited_name(written)?);
        }
        // `zend_lookup_class_ex(ce->parent_name, NULL,
        // ZEND_FETCH_CLASS_NO_AUTOLOAD)` stands before the class itself
        // goes into the table, so a class naming itself as its parent
        // finds nothing and binds as the program serves.
        let early = interfaces.is_empty()
            && match &parent {
                None => true,
                Some(parent) => self.known_class(parent).is_some(),
            };
        let mut decl = ClassDecl {
            name: Name(name.clone().into_boxed_slice()),
            kind: class.kind,
            flags: ClassFlags::default(),
            parent: parent
                .as_ref()
                .map(|parent| Name(parent.clone().into_boxed_slice())),
            interfaces: interfaces
                .iter()
                .map(|i| Name(i.clone().into_boxed_slice()))
                .collect(),
            traits: Vec::new(),
            backing: EnumBacking::Pure,
            constants: Vec::new(),
            properties: Vec::new(),
            methods: Vec::new(),
            attributes: Vec::new(),
            doc_comment: class.doc_comment.clone().map(Vec::into_boxed_slice),
            file: self.filename.clone(),
            line,
            end_line: class.end_line,
        };
        self.active_class = Some(name.clone());
        self.class_constants(&mut decl, class, &name)?;
        self.class_properties(&mut decl, class, &name)?;
        let at = self.classes.len();
        self.classes.push(decl);
        for method in &class.methods {
            self.line = method.function.line;
            self.method_decl(at, method)?;
        }
        self.active_class = None;
        self.line = line;
        if early {
            // `zend_try_early_bind`, which links the class here: what
            // `zend_do_inheritance` refuses is php's `E_ERROR` while the
            // file compiles, so nothing of the file runs.
            if let Some(parent) = &parent {
                self.inherit_at_compile_time(&name, parent)?;
            }
            return Ok(());
        }
        self.declare_class(&name, parent.as_deref())
    }

    /// `zend_compile_class_const_decl`: one declarator at a time, at its
    /// own line, its value kept as the constant expression it was
    /// written as (design §4.7), and a name the group declares twice
    /// php's fatal in php's words.
    fn class_constants(
        &mut self,
        decl: &mut ClassDecl,
        class: &Class,
        name: &[u8],
    ) -> Result<(), CompileError> {
        for constant in &class.constants {
            self.line = constant.line;
            if decl.constants.iter().any(|c| *c.name.0 == *constant.name) {
                return Err(self.compile_error(format!(
                    "Cannot redefine class constant {}::{}",
                    String::from_utf8_lossy(name),
                    String::from_utf8_lossy(&constant.name)
                )));
            }
            let value = self.const_expr(&constant.value)?;
            decl.constants.push(ClassConstDecl {
                name: Name(constant.name.clone().into_boxed_slice()),
                visibility: constant.visibility,
                is_final: constant.is_final,
                ty: None,
                value,
                attributes: Vec::new(),
                doc_comment: constant.doc_comment.clone().map(Vec::into_boxed_slice),
            });
        }
        Ok(())
    }

    /// `zend_compile_prop_decl`, the same one declarator at a time,
    /// with `CG(context).active_property_info_name` set over the
    /// default so `__PROPERTY__` folds to the declarator's name.
    fn class_properties(
        &mut self,
        decl: &mut ClassDecl,
        class: &Class,
        name: &[u8],
    ) -> Result<(), CompileError> {
        for property in &class.properties {
            self.line = property.line;
            if decl.properties.iter().any(|p| *p.name.0 == *property.name) {
                return Err(self.compile_error(format!(
                    "Cannot redeclare {}::${}",
                    String::from_utf8_lossy(name),
                    String::from_utf8_lossy(&property.name)
                )));
            }
            self.active_property = Some(property.name.clone());
            let default = match &property.default {
                Some(expr) => Some(self.const_expr(expr)?),
                None => None,
            };
            self.active_property = None;
            decl.properties.push(PropertyDecl {
                name: Name(property.name.clone().into_boxed_slice()),
                visibility: property.visibility,
                is_instance: !property.is_static,
                is_readonly: false,
                ty: None,
                default,
                attributes: Vec::new(),
                doc_comment: property.doc_comment.clone().map(Vec::into_boxed_slice),
            });
        }
        Ok(())
    }

    /// `zend_resolve_class_name` over a name written after `extends` or
    /// `implements`: the name a `FETCH_CLASS` of the same file resolves
    /// to ([`Self::class_name`]). `self`, `parent` and `static` are what
    /// php's parser refuses there, and are refused here as not yet read.
    fn inherited_name(&mut self, written: &[u8]) -> Result<Vec<u8>, CompileError> {
        let name = self.class_name(written);
        if name.eq_ignore_ascii_case(b"self")
            || name.eq_ignore_ascii_case(b"parent")
            || name.eq_ignore_ascii_case(b"static")
        {
            return Err(self.fatal("a class reference of the scope this compiler cannot read yet"));
        }
        Ok(name)
    }

    /// `zend_lookup_class_ex` over `CG(class_table)`: the declaration of
    /// the class of that name, one the engine declared
    /// ([`Options::classes`]) or one this file has bound before this
    /// point.
    fn known_class(&self, name: &[u8]) -> Option<&ClassDecl> {
        let wanted = name.to_ascii_lowercase();
        self.known_classes
            .iter()
            .chain(self.classes.iter())
            .find(|decl| decl.name.0.to_ascii_lowercase() == wanted)
    }

    /// What `zend_do_inheritance` refuses at the early bind, which is
    /// `E_ERROR` while the file compiles and so runs nothing of it: a
    /// parent that is an interface. The rest of inheritance is the
    /// engine's, since a class this cannot link here it declares with a
    /// `DECLARE_CLASS` instead. The refusal stands while the file
    /// compiles, so nothing of it runs:
    ///
    /// ```php
    /// echo "before\n";
    /// interface I {}
    /// class C extends I {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C cannot extend interface I in %s on line 4
    /// ```
    fn inherit_at_compile_time(&mut self, name: &[u8], parent: &[u8]) -> Result<(), CompileError> {
        let Some(found) = self.known_class(parent) else {
            return Ok(());
        };
        if found.kind != ClassKind::Interface {
            return Ok(());
        }
        let parent = String::from_utf8_lossy(&found.name.0).into_owned();
        Err(CompileError {
            level: Level::Error,
            message: format!(
                "Class {} cannot extend interface {parent}",
                String::from_utf8_lossy(name)
            ),
            line: self.line,
            raised: Vec::new(),
        })
    }

    /// `ZEND_DECLARE_CLASS`, what php emits for a class it did not early
    /// bind: the class's lowered name in `op1` and, where it has a
    /// parent, the parent's lowered name in `op2`, which is what
    /// `zend_bind_class` looks both up by. php adds the runtime
    /// definition key to the constant table after `op1` and keys
    /// `CG(class_table)` by it; the unit carries one declaration per
    /// class under its own name, so the key is not modelled.
    fn declare_class(&mut self, name: &[u8], parent: Option<&[u8]>) -> Result<(), CompileError> {
        let lowered = self.string(&name.to_ascii_lowercase())?;
        let op1 = self.literal(lowered);
        let (op2_type, op2) = match parent {
            Some(parent) => {
                let lowered = self.string(&parent.to_ascii_lowercase())?;
                (OperandKind::Const, self.literal(lowered))
            }
            None => (OperandKind::Unused, 0),
        };
        self.push(Opline {
            opcode: Opcode::DeclareClass,
            op1_type: OperandKind::Const,
            op1,
            op2_type,
            op2,
            ..Opline::default()
        });
        Ok(())
    }

    /// A constant expression as a declaration keeps it (design §4.7):
    /// `zend_const_expr_to_zval` folds what `zend_eval_const_expr` can,
    /// so `1 + 2` is `3`, `'a' . 'b'` is `'ab'` and a number or a string as written is itself, and what names a
    /// constant stays the tree it was written as, for the engine to
    /// evaluate on first use. `zend_is_allowed_in_const_expr` refuses a
    /// variable, a call, an assignment and an increment in php's words;
    /// a cast or a `match`, which php allows or refuses on its own
    /// terms, is not read yet.
    ///
    /// A concatenation of two strings in a property's default is the
    /// one string it folds to, kept as a declaration keeps a string:
    ///
    /// ```php
    /// class M { public $p = 'a' . 'b' . 1; }
    /// $o = new M;
    /// echo $o->p;
    /// ```
    /// ```expect exit=0
    /// ab1
    /// ```
    fn const_expr(&mut self, expr: &Expr) -> Result<ConstExpr, CompileError> {
        self.check_stack()?;
        match expr {
            Expr::Null => Ok(ConstExpr::Null),
            Expr::Bool(b) => Ok(ConstExpr::Bool(*b)),
            Expr::Int(n) => Ok(ConstExpr::Long(*n)),
            Expr::Float(f) => Ok(ConstExpr::Double(*f)),
            Expr::Str(bytes) => Ok(ConstExpr::Str(bytes.clone().into_boxed_slice())),
            Expr::Constant(name) => Ok(ConstExpr::Constant(Name(name.clone().into_boxed_slice()))),
            // `zend_compile_const_expr_class_const`: the class named as
            // written, resolved in the namespace, and the constant's
            // name, kept as the tree for the engine to fetch on first
            // use; `self`, `parent` and `static` are refused as
            // `class_name_literal` refuses them.
            Expr::ClassConst { class, name } => {
                let class = self.class_name(class);
                if class.eq_ignore_ascii_case(b"self")
                    || class.eq_ignore_ascii_case(b"parent")
                    || class.eq_ignore_ascii_case(b"static")
                {
                    return Err(
                        self.fatal("a class reference of the scope this compiler cannot read yet")
                    );
                }
                Ok(ConstExpr::ClassConstant {
                    class: Name(class.into_boxed_slice()),
                    name: Name(name.clone().into_boxed_slice()),
                })
            }
            // `zend_eval_const_expr` folds a magic constant as
            // `zend_compile_expr` does; the one it leaves,
            // `__CLASS__` in a trait, is not read yet (a trait is not).
            Expr::Magic { which, line } => Ok(match self.magic_const(*which, *line) {
                Folded::Line(n) => ConstExpr::Long(n),
                Folded::Text(text) => ConstExpr::Str(text.into_boxed_slice()),
            }),
            // `ZEND_AST_ARRAY` in a constant expression stays the tree
            // it was written as, each element's key and value.
            Expr::Array(elements) => {
                let mut items = Vec::with_capacity(elements.len());
                for element in elements {
                    let key = match &element.key {
                        Some(key) => Some(self.const_expr(key)?),
                        None => None,
                    };
                    items.push((key, self.const_expr(&element.value)?));
                }
                Ok(ConstExpr::Array(items))
            }
            Expr::Unary { .. } | Expr::Binary { .. } => {
                if let Some(folded) = self.constant_of(expr)? {
                    match folded.kind() {
                        ZvalType::Null => return Ok(ConstExpr::Null),
                        ZvalType::False => return Ok(ConstExpr::Bool(false)),
                        ZvalType::True => return Ok(ConstExpr::Bool(true)),
                        ZvalType::Long => {
                            if let Some(n) = folded.as_long() {
                                return Ok(ConstExpr::Long(n));
                            }
                        }
                        ZvalType::Double => {
                            if let Some(f) = folded.as_double() {
                                return Ok(ConstExpr::Double(f));
                            }
                        }
                        ZvalType::String => {
                            if let Some(s) = folded.as_str() {
                                return Ok(ConstExpr::Str(s.bytes().into()));
                            }
                        }
                        _ => {}
                    }
                    return Err(self.fatal("a constant expression this compiler cannot read yet"));
                }
                match expr {
                    Expr::Unary { minus, operand } => Ok(ConstExpr::Unary {
                        op: Name(if *minus { b"-".as_slice() } else { b"+" }.into()),
                        operand: Box::new(self.const_expr(operand)?),
                    }),
                    Expr::Binary { op, left, right } => Ok(ConstExpr::Binary {
                        op: Name(op.spelling().as_bytes().into()),
                        lhs: Box::new(self.const_expr(left)?),
                        rhs: Box::new(self.const_expr(right)?),
                    }),
                    _ => Err(self.fatal("a constant expression this compiler cannot read yet")),
                }
            }
            Expr::Var(_)
            | Expr::This
            | Expr::Prop { .. }
            | Expr::Assign { .. }
            | Expr::AssignProp { .. }
            | Expr::AssignRef { .. }
            | Expr::Call { .. }
            | Expr::PreInc(_)
            | Expr::PostInc(_)
            | Expr::PreDec(_)
            | Expr::PostDec(_)
            | Expr::StaticCall { .. }
            | Expr::MethodCall { .. }
            | Expr::StaticProp { .. }
            | Expr::Silence(_)
            | Expr::Throw(_)
            | Expr::Encaps(_) => {
                Err(self.compile_error("Constant expression contains invalid operations"))
            }
            Expr::Cast { .. } | Expr::Match { .. } | Expr::New { .. } | Expr::Instanceof { .. } => {
                Err(self.fatal("a constant expression this compiler cannot read yet"))
            }
        }
    }

    /// `zend_compile_namespace` for the unbraced form: the name is the
    /// namespace of what follows, and a first one after any statement
    /// but an empty one is php's compile error (`zend_is_first_statement`);
    /// a later one moves the namespace, as php's does. No opline.
    fn namespace(&mut self, name: &[u8]) -> Result<(), CompileError> {
        if self.namespace.is_none() && self.statements_seen > 0 {
            return Err(self.compile_error(
                "Namespace declaration statement has to be the very first statement or after any declare call in the script",
            ));
        }
        self.namespace = Some(name.to_vec());
        Ok(())
    }

    /// `zend_compile_stmt_list`: each statement at its own line.
    fn statements(&mut self, body: &Read) -> Result<(), CompileError> {
        self.depth = self.depth.saturating_add(1);
        for (statement, line) in body {
            self.line = *line;
            self.statement(statement)?;
        }
        self.depth = self.depth.saturating_sub(1);
        Ok(())
    }

    /// `zend_compile_for`: the initialisers, a `JMP` forward to the
    /// condition, the body, the step expressions, then the condition
    /// as a `JMPNZ` back to the body's first opline. The oplines of the
    /// header carry the statement's line, as the body's carry theirs.
    fn r#for(
        &mut self,
        init: &[Expr],
        cond: &[Expr],
        step: &[Expr],
        body: &Read,
    ) -> Result<(), CompileError> {
        let line = self.line;
        let init = self.expr_list(init)?;
        self.do_free(init);
        let jump = self.push(Opline {
            opcode: Opcode::Jmp,
            ..Opline::default()
        });
        self.begin_loop(None, None, false);
        let start = self.next();
        self.statements(body)?;
        let continue_at = self.next();
        self.line = line;
        let step = self.expr_list(step)?;
        self.do_free(step);
        self.update_jump_target_to_next(jump);
        let cond = self.expr_list(cond)?;
        self.emit_cond_jump(cond, start);
        self.end_loop(continue_at);
        Ok(())
    }

    /// `zend_compile_while`: a `JMP` forward to the condition, the body,
    /// then the condition as a `JMPNZ` back to the body's first opline.
    fn r#while(&mut self, cond: &Expr, body: &Read) -> Result<(), CompileError> {
        let line = self.line;
        let jump = self.push(Opline {
            opcode: Opcode::Jmp,
            ..Opline::default()
        });
        self.begin_loop(None, None, false);
        let start = self.next();
        self.statements(body)?;
        let continue_at = self.next();
        self.line = line;
        self.update_jump_target_to_next(jump);
        let cond = self.expr(cond)?;
        self.emit_cond_jump(cond, start);
        self.end_loop(continue_at);
        Ok(())
    }

    /// `zend_compile_foreach` (`Z/Zend/zend_compile.c:6094`): the key
    /// refused where php refuses it, a reference or a list; the subject
    /// compiled, under `BP_VAR_W` where the value is by reference and
    /// the subject a variable, which for a CV is the CV itself, with
    /// `SEPARATE` after a call's `VAR` (`zend_separate_if_call_and_write`);
    /// `FE_RESET_R` or `FE_RESET_RW` over it into a `VAR`; the loop begun
    /// with `FE_FREE` over that `VAR` as its variable; `FE_FETCH_R` or
    /// `FE_FETCH_RW` with the value's CV in `op2`, `$this` refused as
    /// php refuses it; the key, where one is written, as the temporary
    /// the fetch writes, assigned to its CV (`zend_make_tmp_result`,
    /// `zend_emit_assign_znode`); the body; then at the statement's line
    /// the `JMP` back to the fetch, the back-edge, the reset's `op2` and
    /// the fetch's `extended_value` aimed past the loop, the loop ended
    /// and `FE_FREE` over the `VAR`. The header's safepoint is numbered
    /// when the loop begins, so the `JMP` and every `continue` that lands
    /// on the fetch carry one number (design §5.7).
    ///
    /// A value or a key that is not a variable, a property or a
    /// dimension under `BP_VAR_W` and a subject `zend_compile_var` would
    /// fetch for writing are not read yet.
    fn foreach(
        &mut self,
        expr: &Expr,
        value: &Target,
        key: Option<&Target>,
        body: &Read,
    ) -> Result<(), CompileError> {
        let line = self.line;
        if key.is_some_and(|key| key.by_ref) {
            return Err(self.compile_error("Key element cannot be a reference"));
        }
        let by_ref = value.by_ref;
        let subject = self.foreach_subject(expr, by_ref)?;
        let (op1_type, op1) = self.operand(subject);
        let reset = self.temporary();
        let opnum_reset = self.push(Opline {
            opcode: if by_ref {
                Opcode::FeResetRw
            } else {
                Opcode::FeResetR
            },
            op1_type,
            op1,
            out_type: OperandKind::Var,
            out: reset,
            ..Opline::default()
        });
        let opnum_fetch = self.next();
        let header = self.safepoint(opnum_fetch);
        self.begin_loop(
            Some(LoopVar {
                opcode: Opcode::FeFree,
                kind: OperandKind::Var,
                var: reset,
            }),
            Some(header),
            false,
        );
        let value_cv = self.foreach_target(&value.expr)?;
        self.push(Opline {
            opcode: if by_ref {
                Opcode::FeFetchRw
            } else {
                Opcode::FeFetchR
            },
            op1_type: OperandKind::Var,
            op1: reset,
            op2_type: OperandKind::Cv,
            op2: value_cv,
            ..Opline::default()
        });
        if let Some(key) = key {
            self.foreach_key(opnum_fetch, &key.expr)?;
        }
        self.statements(body)?;
        self.line = line;
        self.push(Opline {
            opcode: Opcode::Jmp,
            op1: opnum_fetch,
            out: header,
            ..Opline::default()
        });
        let after = self.next();
        if let Some(opline) = self.oplines.get_mut(opnum_reset as usize) {
            opline.op2 = after;
        }
        if let Some(opline) = self.oplines.get_mut(opnum_fetch as usize) {
            opline.extended_value = after;
        }
        self.end_loop(opnum_fetch);
        self.push(Opline {
            opcode: Opcode::FeFree,
            op1_type: OperandKind::Var,
            op1: reset,
            ..Opline::default()
        });
        Ok(())
    }

    /// The subject of a `foreach`: compiled as an expression, and where
    /// the value is taken by reference, as `zend_compile_var` under
    /// `BP_VAR_W` compiles a variable, which for a CV is the CV itself;
    /// a call's `VAR` is separated after it
    /// (`zend_separate_if_call_and_write`), and a call whose value is a
    /// temporary, one of the special table's, is php's compile error. A
    /// property fetched for writing is not read yet.
    fn foreach_subject(&mut self, expr: &Expr, by_ref: bool) -> Result<Where, CompileError> {
        if by_ref && matches!(expr, Expr::Prop { .. }) {
            return Err(self
                .fatal("a `foreach` by reference over a property this compiler cannot read yet"));
        }
        let subject = self.expr(expr)?;
        if by_ref && is_call(expr) {
            let Where::Returned(at) = subject else {
                return Err(
                    self.compile_error("Cannot use result of built-in function in write context")
                );
            };
            self.push(Opline {
                opcode: Opcode::Separate,
                op1_type: OperandKind::Var,
                op1: at,
                out_type: OperandKind::Var,
                out: at,
                ..Opline::default()
            });
        }
        Ok(subject)
    }

    /// The key of a `foreach`: the fetch at `opnum_fetch` given a
    /// temporary to write the key into (`zend_make_tmp_result`), then
    /// `zend_emit_assign_znode`'s `ASSIGN` of it into the key's CV, into
    /// a temporary nothing reads, which `zend_do_free` marks `UNUSED`.
    fn foreach_key(&mut self, opnum_fetch: u32, key: &Expr) -> Result<(), CompileError> {
        let key_cv = self.foreach_target(key)?;
        let key_tmp = self.temporary();
        if let Some(fetch) = self.oplines.get_mut(opnum_fetch as usize) {
            fetch.out_type = OperandKind::Tmp;
            fetch.out = key_tmp;
        }
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::Assign,
            op1_type: OperandKind::Cv,
            op1: key_cv,
            op2_type: OperandKind::Tmp,
            op2: key_tmp,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        self.do_free(Where::Temporary(out));
        Ok(())
    }

    /// The CV a `foreach` value or key is assigned into: a variable
    /// through `zend_try_compile_cv`, `$this` php's compile error, and
    /// anything else, which php assigns through a `VAR`, not read yet.
    fn foreach_target(&mut self, target: &Expr) -> Result<u32, CompileError> {
        match target {
            Expr::Var(name) => Ok(self.lookup_cv(name)),
            Expr::This => Err(self.compile_error("Cannot re-assign $this")),
            _ => Err(self.fatal("a `foreach` whose target this compiler cannot read yet")),
        }
    }

    /// `zend_compile_return` (`Z/Zend/zend_compile.c:5694`): the value,
    /// or the constant `null` where none is written; the value copied
    /// into a temporary where a `finally` stands between here and the
    /// `RETURN` ([`Self::copied_past_finally`]); the return type check
    /// where the op array declares one ([`Self::return_type_check`]);
    /// every open loop's variable freed and every open `finally` called,
    /// innermost first ([`Self::unwind_to_return`]); then `RETURN` over
    /// the value, or `RETURN_BY_REF` where the function was written
    /// `function &f`, carrying its safepoint number as
    /// [`Self::final_return`]'s carries its own. `extended_value` is
    /// `ZEND_RETURNS_FUNCTION` where a by-reference return returns a
    /// call's value and `ZEND_RETURNS_VALUE` where it returns what is no
    /// variable; zero otherwise, which is every return of the main op
    /// array, that returns nothing by reference.
    ///
    /// ```php
    /// function f() { try { return "t"; } finally { echo "f\n"; } }
    /// function g() { try { throw new Exception("x"); } catch (Exception $e) { return $e->getMessage(); } finally { echo "g\n"; } }
    /// echo f(), "\n", g(), "\n";
    /// ```
    /// ```expect
    /// f
    /// t
    /// g
    /// x
    /// ```
    fn r#return(&mut self, value: Option<&Expr>) -> Result<(), CompileError> {
        let by_ref = self.returns_by_ref;
        if by_ref
            && value.is_some_and(|expr| {
                matches!(
                    expr,
                    Expr::Prop { .. } | Expr::StaticProp { .. } | Expr::This
                )
            })
        {
            return Err(self.fatal(
                "a return by reference of what is no variable this compiler cannot read yet",
            ));
        }
        let extended_value = match value {
            Some(_) if !by_ref => 0,
            Some(Expr::Call { .. } | Expr::MethodCall { .. } | Expr::StaticCall { .. }) => {
                RETURNS_FUNCTION
            }
            Some(Expr::Var(_)) | None => 0,
            Some(_) => RETURNS_VALUE,
        };
        let compiled = match value {
            Some(expr) => self.expr(expr)?,
            None => Where::Constant(Zval::null()),
        };
        let compiled = self.copied_past_finally(compiled, by_ref)?;
        let compiled = self.return_type_check(compiled, value.is_some(), false)?;
        self.unwind_to_return(compiled);
        let (op1_type, op1) = self.operand(compiled);
        let at = self.next();
        let number = self.safepoint(at);
        self.push(Opline {
            opcode: if by_ref {
                Opcode::ReturnByRef
            } else {
                Opcode::Return
            },
            op1_type,
            op1,
            out: number,
            extended_value,
            ..Opline::default()
        });
        Ok(())
    }

    /// `zend_emit_return_type_check` (`Z/Zend/zend_compile.c:2623`): the
    /// `VERIFY_RETURN_TYPE` before a `RETURN`, where the op array
    /// declares a return type the value is not already known to satisfy.
    /// `void` needs no check and refuses a value in php's words; `never`
    /// refuses a `return` written at all, and its implicit case is
    /// [`Self::final_return`]'s `VERIFY_NEVER_TYPE`; `mixed` and a
    /// constant of a type the declaration accepts need no check either.
    /// What is checked reads the value in `op1`, and a constant is moved
    /// into a temporary the opline writes, which the `RETURN` then
    /// returns.
    fn return_type_check(
        &mut self,
        value: Where,
        written: bool,
        implicit: bool,
    ) -> Result<Where, CompileError> {
        let Some(ty) = self.return_type.clone() else {
            return Ok(value);
        };
        let what = if self.active_class.is_some() {
            "method"
        } else {
            "function"
        };
        if ty.contains_code(VOID_CODE) {
            if written {
                return Err(self.compile_error(match value {
                    Where::Constant(zval) if zval.kind() == ZvalType::Null => format!(
                        "A void {what} must not return a value (did you mean \"return;\" instead of \"return null;\"?)"
                    ),
                    _ => format!("A void {what} must not return a value"),
                }));
            }
            return Ok(value);
        }
        if ty.contains_code(NEVER_CODE) {
            return Err(self.compile_error(format!("A never-returning {what} must not return")));
        }
        if !written && !implicit {
            return Err(self.compile_error(
                if ty.contains_code(ZvalType::Null as u8) {
                    format!(
                        "A {what} with return type must return a value (did you mean \"return null;\" instead of \"return;\"?)"
                    )
                } else {
                    format!("A {what} with return type must return a value")
                },
            ));
        }
        if written && ty.mask() & Type::MAY_BE_ANY == Type::MAY_BE_ANY {
            return Ok(value);
        }
        if let (true, Where::Constant(zval)) = (written, value)
            && ty.contains_code(zval.kind() as u8)
        {
            return Ok(value);
        }
        let (op1_type, op1) = if written {
            self.operand(value)
        } else {
            (OperandKind::Unused, 0)
        };
        let constant = op1_type == OperandKind::Const;
        let out = if constant { self.temporary() } else { 0 };
        self.push(Opline {
            opcode: Opcode::VerifyReturnType,
            op1_type,
            op1,
            out_type: if constant {
                OperandKind::Tmp
            } else {
                OperandKind::Unused
            },
            out,
            ..Opline::default()
        });
        Ok(if constant {
            Where::Temporary(out)
        } else {
            value
        })
    }

    /// `zend_begin_loop`: one more element of `brk_cont_array`, the
    /// innermost from here, holding the variable the loop frees when it
    /// has one, and the header's safepoint number where a `continue`
    /// jumps back to it.
    fn begin_loop(&mut self, var: Option<LoopVar>, header: Option<u32>, is_switch: bool) {
        self.loops.push(Loop {
            parent: self.current_loop,
            is_switch,
            brk: 0,
            cont: 0,
            var,
            header,
        });
        self.current_loop = Some(self.loops.len() - 1);
    }

    /// `zend_end_loop`: `continue` lands at `cont`, `break` at the
    /// opline about to be emitted, and the loop outside is the innermost
    /// again.
    fn end_loop(&mut self, cont: u32) {
        let brk = self.next();
        if let Some(current) = self.current_loop
            && let Some(open) = self.loops.get_mut(current)
        {
            open.brk = brk;
            open.cont = cont;
            self.current_loop = open.parent;
        }
    }

    /// `zend_compile_return`'s copy into a temporary: a returned CV is
    /// moved into a `QM_ASSIGN` where the op array has a `finally` block
    /// and one stands between here and the `RETURN` (`zend_has_finally`),
    /// so code in the `finally` cannot write the value out from under
    /// the return. A constant or a temporary is already nobody else's.
    fn copied_past_finally(&mut self, value: Where, by_ref: bool) -> Result<Where, CompileError> {
        // `zend_has_finally`: a `FAST_CALL` stands on the stack, which
        // is a `finally` between here and the `RETURN`; inside a
        // `finally` block its own marker is a `DISCARD_EXCEPTION` and
        // the copy is not made.
        if !self
            .unwind
            .iter()
            .any(|marker| matches!(marker, Unwind::FastCall { .. }))
        {
            return Ok(value);
        }
        if by_ref {
            // php makes a `MAKE_REF` of a `VAR` here and checks the
            // return type again past the block, neither of which this
            // compiler emits yet.
            return Err(self.fatal(
                "a return by reference out of a `try` with a `finally` this compiler cannot read yet",
            ));
        }
        let Where::Cv(cv) = value else {
            return Ok(value);
        };
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::QmAssign,
            op1_type: OperandKind::Cv,
            op1: cv,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_handle_loops_and_finally` on the way to a `RETURN`: every
    /// entry of `CG(loop_var_stack)` innermost first, with no depth to
    /// stop at, so every open loop's variable is freed and every open
    /// `finally` is called. The two stacks are one in php; here the
    /// loops hang off [`Emit::current_loop`] and the `finally` markers
    /// off [`Emit::unwind`], each marker recording how many loops stood
    /// under it, which puts them back in one order.
    ///
    /// A `FAST_CALL` emitted here carries the value being returned in
    /// `op2`, which is what a `DISCARD_EXCEPTION` or an unwind through
    /// the block gives up when the return never happens.
    fn unwind_to_return(&mut self, value: Where) {
        let (value_type, value_at) = match value {
            Where::Temporary(at) => (OperandKind::Tmp, at),
            Where::Returned(at) => (OperandKind::Var, at),
            Where::Constant(_) | Where::Cv(_) => (OperandKind::Unused, 0),
        };
        let mut loops = self.current_loop;
        let mut open = self.loops_open();
        let mut markers = self.unwind.len();
        loop {
            let under = markers
                .checked_sub(1)
                .and_then(|last| self.unwind.get(last))
                .map(|marker| marker.loops());
            match under {
                // The marker was pushed with this many loops under it,
                // so it stands above every loop still open once that
                // many are left, and is emitted first.
                Some(under) if under >= open => {
                    markers -= 1;
                    match self.unwind.get(markers).copied() {
                        Some(Unwind::FastCall {
                            var,
                            try_catch_offset,
                            ..
                        }) => self.push(Opline {
                            opcode: Opcode::FastCall,
                            op1: try_catch_offset,
                            op2_type: value_type,
                            op2: value_at,
                            out_type: OperandKind::Tmp,
                            out: var,
                            ..Opline::default()
                        }),
                        Some(Unwind::DiscardException { var, .. }) => self.push(Opline {
                            opcode: Opcode::DiscardException,
                            op1_type: OperandKind::Tmp,
                            op1: var,
                            ..Opline::default()
                        }),
                        None => break,
                    };
                }
                _ => {
                    let Some(index) = loops else {
                        break;
                    };
                    let Some((var, parent)) =
                        self.loops.get(index).map(|one| (one.var, one.parent))
                    else {
                        break;
                    };
                    if let Some(var) = var {
                        self.free_on_return(var);
                    }
                    loops = parent;
                    open = open.saturating_sub(1);
                }
            }
        }
    }

    /// One opline of `zend_handle_loops_and_finally_ex`: the loop's
    /// variable freed by the loop's own opcode, flagged
    /// `ZEND_FREE_ON_RETURN`, which the live-range walk splits the
    /// variable's range around ([`crate::live`]).
    fn free_on_return(&mut self, var: LoopVar) {
        self.push(Opline {
            opcode: var.opcode,
            op1_type: var.kind,
            op1: var.var,
            extended_value: FREE_ON_RETURN,
            ..Opline::default()
        });
    }

    /// `zend_compile_break_continue`: the depth, `1` unwritten and
    /// otherwise the positive integer php's compiler holds it to; the
    /// loop or `switch` it stands in, without which php refuses; the
    /// `FREE` of each `switch` subject the jump leaves behind
    /// (`zend_handle_loops_and_finally_ex`), which is also where a depth
    /// past the loops that stand is refused; the compile-time warning
    /// for a `continue` that targets a `switch`; then the `ZEND_BRK` or
    /// `ZEND_CONT`, here the `JMP` `pass_two` makes of it once its
    /// target is known ([`Self::resolve_breaks`]).
    fn break_continue(&mut self, depth: Option<&Expr>, is_break: bool) -> Result<(), CompileError> {
        let word = if is_break { "break" } else { "continue" };
        let depth = match depth {
            None => 1,
            Some(expr) => {
                let Some(value) = self.constant_of(expr)? else {
                    return Err(self.compile_error(format!(
                        "'{word}' operator with non-integer operand is no longer supported"
                    )));
                };
                match value.as_long() {
                    Some(depth) if depth >= 1 => u32::try_from(depth).unwrap_or(u32::MAX),
                    _ => {
                        return Err(self.compile_error(format!(
                            "'{word}' operator accepts only positive integers"
                        )));
                    }
                }
            }
        };
        let Some(current) = self.current_loop else {
            return Err(
                self.compile_error(format!("'{word}' not in the 'loop' or 'switch' context"))
            );
        };
        // `zend_handle_loops_and_finally_ex` emits the `FAST_CALL` of
        // every `finally` the jump leaves, and `zend_check_finally_breakout`
        // refuses the jump that leaves one it cannot; neither is read
        // yet, so a jump that would cross a marker is refused rather
        // than emitted without its `finally`.
        if self
            .unwind
            .last()
            .is_some_and(|marker| marker.loops() >= self.loops_open().saturating_sub(depth))
        {
            return Err(self.fatal(format!(
                "a `{word}` out of a `try` with a `finally` this compiler cannot read yet"
            )));
        }
        // `zend_handle_loops_and_finally_ex`: the loops inside the target
        // are left, each `switch` subject among them freed on the way.
        let mut left = 0;
        let mut at = Some(current);
        while left + 1 < depth {
            let Some(index) = at else {
                break;
            };
            let Some((var, parent)) = self.loops.get(index).map(|open| (open.var, open.parent))
            else {
                break;
            };
            if let Some(var) = var {
                self.free_on_return(var);
            }
            at = parent;
            left += 1;
        }
        if at.is_none() || left + 1 < depth {
            let plural = if depth == 1 { "" } else { "s" };
            return Err(self.compile_error(format!("Cannot '{word}' {depth} level{plural}")));
        }
        if !is_break
            && let Some(target) = at.and_then(|index| self.loops.get(index))
            && target.is_switch
        {
            let hint = if target.parent.is_none() {
                String::new()
            } else {
                format!(". Did you mean to use \"continue {}\"?", depth + 1)
            };
            let message = if depth == 1 {
                format!("\"continue\" targeting switch is equivalent to \"break\"{hint}")
            } else {
                format!(
                    "\"continue {depth}\" targeting switch is equivalent to \"break {depth}\"{hint}"
                )
            };
            self.raise(Level::CompileWarning, &message);
        }
        let opline = self.push(Opline {
            opcode: Opcode::Jmp,
            ..Opline::default()
        });
        self.breaks.push(Pending {
            at: opline,
            r#loop: current,
            depth,
            is_break,
        });
        Ok(())
    }

    /// `pass_two`'s `ZEND_BRK` and `ZEND_CONT` arm, `zend_get_brk_cont_target`:
    /// the loop `depth` levels out from the one the jump was written in,
    /// and its `brk` or `cont` as the `JMP`'s target.
    fn resolve_breaks(&mut self) {
        for jump in std::mem::take(&mut self.breaks) {
            let mut index = jump.r#loop;
            let mut levels = jump.depth;
            while levels > 1 {
                match self.loops.get(index).and_then(|open| open.parent) {
                    Some(parent) => index = parent,
                    None => break,
                }
                levels -= 1;
            }
            let Some(target) = self.loops.get(index) else {
                continue;
            };
            let header = if jump.is_break { None } else { target.header };
            let target = if jump.is_break {
                target.brk
            } else {
                target.cont
            };
            if let Some(opline) = self.oplines.get_mut(jump.at as usize) {
                opline.op1 = target;
                // A `continue` that jumps back to a `foreach`'s header is
                // a back-edge and carries the header's number, the one
                // the loop's own `JMP` carries (design §5.7).
                if let Some(header) = header {
                    opline.out = header;
                }
            }
        }
    }

    /// `zend_compile_switch`: the subject compiled, then the loop
    /// begun over it; the cases' type read
    /// (`determine_switch_jumptable_type`) and, where
    /// `should_use_jumptable` asks for one, five `long` cases or two
    /// `string` ones, a `SWITCH_LONG` or `SWITCH_STRING` over the table
    /// as a constant array, its slot in the table taken here and the
    /// array laid down once every target is known; the chain over the
    /// cases and the `JMP` to the default; each case's statements with
    /// the jumps aimed at them and the table's targets recorded; the
    /// loop ended, and the subject freed when it is a temporary.
    fn switch(&mut self, expr: &Expr, cases: &[Case]) -> Result<(), CompileError> {
        let subject = self.expr(expr)?;
        self.begin_loop(LoopVar::of(Opcode::Free, subject), None, true);
        let case_node = self.temporary();
        let mut table = match self.switch_jumptable_type(cases)? {
            Some(kind) if should_use_jumptable(cases, kind) => Some(self.emit_table(
                if kind == ZvalType::Long {
                    Opcode::SwitchLong
                } else {
                    Opcode::SwitchString
                },
                subject,
            )),
            _ => None,
        };
        let jumps = self.switch_chain(subject, case_node, cases)?;
        let default_jump = self.push(Opline {
            opcode: Opcode::Jmp,
            ..Opline::default()
        });
        let mut has_default = false;
        for (case, jump) in cases.iter().zip(jumps) {
            if let (Some((cond, _)), Some(jump)) = (&case.cond, jump) {
                self.update_jump_target_to_next(jump);
                self.table_add(&mut table, cond)?;
            } else {
                has_default = true;
                self.update_jump_target_to_next(default_jump);
                self.table_default_to_next(table.as_ref());
            }
            self.statements(&case.body)?;
        }
        if !has_default {
            self.update_jump_target_to_next(default_jump);
            self.table_default_to_next(table.as_ref());
        }
        self.lay_table(table)?;
        let end = self.next();
        self.end_loop(end);
        self.free_subject(subject);
        Ok(())
    }

    /// The chain of `zend_compile_switch`: per case, one `IS_EQUAL`
    /// (`CASE` over a temporary subject) into the one shared temporary
    /// and a `JMPNZ` forward, or a `JMPZ` or `JMPNZ` over the case alone
    /// when the subject is a constant `false` or `true`; the default's
    /// one default clause refused as php refuses it. The jump of each
    /// case, none for the default.
    fn switch_chain(
        &mut self,
        subject: Where,
        case_node: u32,
        cases: &[Case],
    ) -> Result<Vec<Option<u32>>, CompileError> {
        let mut jumps = Vec::with_capacity(cases.len());
        let mut has_default = false;
        for case in cases {
            let Some((cond, cond_line)) = &case.cond else {
                if has_default {
                    self.line = case.line;
                    return Err(
                        self.compile_error("Switch statements may only contain one default clause")
                    );
                }
                has_default = true;
                jumps.push(None);
                continue;
            };
            self.line = *cond_line;
            let cond = self.expr(cond)?;
            let jump = match subject {
                Where::Constant(value) if value.kind() == ZvalType::False => {
                    self.cond_jump_forward(Opcode::Jmpz, cond)
                }
                Where::Constant(value) if value.kind() == ZvalType::True => {
                    self.cond_jump_forward(Opcode::Jmpnz, cond)
                }
                _ => {
                    let opcode = match subject {
                        Where::Temporary(_) | Where::Returned(_) => Opcode::Case,
                        _ => Opcode::IsEqual,
                    };
                    self.case(opcode, subject, cond, case_node);
                    self.cond_jump_forward(Opcode::Jmpnz, Where::Temporary(case_node))
                }
            };
            jumps.push(Some(jump));
        }
        Ok(jumps)
    }

    /// One comparison of the chain, `zend_emit_op` with `SET_NODE` of
    /// the shared temporary as what it leaves: `IS_EQUAL`, `CASE`,
    /// `IS_IDENTICAL` or `CASE_STRICT` over the subject and the
    /// condition.
    fn case(&mut self, opcode: Opcode, subject: Where, cond: Where, case_node: u32) {
        let (op1_type, op1) = self.operand(subject);
        let (op2_type, op2) = self.operand(cond);
        self.push(Opline {
            opcode,
            op1_type,
            op1,
            op2_type,
            op2,
            out_type: OperandKind::Tmp,
            out: case_node,
            ..Opline::default()
        });
    }

    /// `determine_switch_jumptable_type`: `long` or `string` when every
    /// case is a constant of that one type and no string case is
    /// numeric, which a hash lookup could not compare as `==` compares
    /// it; nothing otherwise.
    fn switch_jumptable_type(&mut self, cases: &[Case]) -> Result<Option<ZvalType>, CompileError> {
        let mut common = None;
        for case in cases {
            let Some((cond, _)) = &case.cond else {
                continue;
            };
            let Some(value) = self.constant_of(cond)? else {
                return Ok(None);
            };
            let kind = value.kind();
            if kind != ZvalType::Long && kind != ZvalType::String {
                return Ok(None);
            }
            match common {
                None => common = Some(kind),
                Some(held) if held != kind => return Ok(None),
                Some(_) => {}
            }
            if let Some(s) = value.as_str()
                && numeric_prefix(s.bytes()).is_some_and(|(_, trailing)| !trailing)
            {
                return Ok(None);
            }
        }
        Ok(common)
    }

    /// The opline over a jump table, `SWITCH_LONG`, `SWITCH_STRING` or
    /// `MATCH`: the constant slot the array will take, reserved as
    /// `zend_emit_op` adds the constant when the opline is emitted, and
    /// the opline over the subject and it.
    fn emit_table(&mut self, opcode: Opcode, subject: Where) -> Table {
        let slot = self.literal(Zval::null());
        let (op1_type, op1) = self.operand(subject);
        let at = self.push(Opline {
            opcode,
            op1_type,
            op1,
            op2_type: OperandKind::Const,
            op2: slot,
            ..Opline::default()
        });
        Table {
            at,
            slot,
            entries: Vec::new(),
        }
    }

    /// `zend_hash_index_add` or `zend_hash_add` of the opline about to be
    /// emitted under the condition's value: the first case of a value
    /// keeps its target. Nothing where no table is being built.
    fn table_add(&mut self, table: &mut Option<Table>, cond: &Expr) -> Result<(), CompileError> {
        let Some(table) = table else {
            return Ok(());
        };
        let target = self.next();
        if let Some(key) = self.jump_key(cond)?
            && !table.entries.iter().any(|(held, _)| *held == key)
        {
            table.entries.push((key, Zval::long(i64::from(target))));
        }
        Ok(())
    }

    /// The table opline's `extended_value` aimed at the opline about to
    /// be emitted: where the default lands.
    fn table_default_to_next(&mut self, table: Option<&Table>) {
        let Some(table) = table else {
            return;
        };
        let target = self.next();
        if let Some(opline) = self.oplines.get_mut(table.at as usize) {
            opline.extended_value = target;
        }
    }

    /// The constant array the slot was reserved for, laid down in the
    /// home the unit's strings are in ([`Intern::constant_array`]).
    fn lay_table(&mut self, table: Option<Table>) -> Result<(), CompileError> {
        let Some(table) = table else {
            return Ok(());
        };
        let array = match self.strings.constant_array(&table.entries) {
            Ok(array) => array,
            Err(Fatal::Error(message)) => return Err(self.fatal(message)),
            Err(Fatal::OutOfMemory(_)) => return Err(self.fatal("Out of memory")),
        };
        if let Some(held) = self.constants.get_mut(table.slot as usize) {
            *held = Zval::array(array);
        }
        Ok(())
    }

    /// The key a case or an arm condition adds to a jump table: its
    /// integer, or its string as interned; nothing for a constant of
    /// another type, which the type walk ruled out already.
    fn jump_key(&mut self, cond: &Expr) -> Result<Option<Key>, CompileError> {
        let Some(value) = self.constant_of(cond)? else {
            return Ok(None);
        };
        Ok(match value.kind() {
            ZvalType::Long => value.as_long().map(Key::Long),
            ZvalType::String => value.as_str().map(Key::Str),
            _ => None,
        })
    }

    /// The `FREE` after a `switch` or a `match` whose subject is a
    /// temporary, flagged `ZEND_FREE_SWITCH`; a constant or a CV is
    /// nobody's to free.
    fn free_subject(&mut self, subject: Where) {
        let (kind, at) = match subject {
            Where::Temporary(at) => (OperandKind::Tmp, at),
            Where::Returned(at) => (OperandKind::Var, at),
            Where::Constant(_) | Where::Cv(_) => return,
        };
        self.push(Opline {
            opcode: Opcode::Free,
            op1_type: kind,
            op1: at,
            extended_value: FREE_SWITCH,
            ..Opline::default()
        });
    }

    /// `zend_compile_match`: the subject and one temporary for the
    /// conditions' answers, allocated whether or not the chain uses it;
    /// `MATCH` over the table where every condition is a constant `long`
    /// or `string` and there are two or more (`can_match_use_jumptable`),
    /// else the chain and the `JMP` to the default; a `MATCH_ERROR`
    /// first where no arm is the default, so the value's temporary is
    /// defined before it is freed; then the arms; the constant `null`
    /// for a `match` with no arm; and the subject freed when it is a
    /// temporary.
    fn r#match(&mut self, expr: &Expr, arms: &[Arm], line: u32) -> Result<Where, CompileError> {
        self.line = line;
        let subject = self.expr(expr)?;
        let case_node = self.temporary();
        let mut has_default = false;
        for arm in arms {
            if arm.conds.is_none() {
                if has_default {
                    self.line = arm.line;
                    return Err(
                        self.compile_error("Match expressions may only contain one default arm")
                    );
                }
                has_default = true;
            }
        }
        let mut table = None;
        let mut jumps = Vec::new();
        let mut default_jump = None;
        if self.match_uses_jumptable(arms)? {
            table = Some(self.emit_table(Opcode::Match, subject));
        } else {
            jumps = self.match_chain(subject, case_node, arms)?;
            default_jump = Some(self.push(Opline {
                opcode: Opcode::Jmp,
                ..Opline::default()
            }));
        }
        if !has_default {
            if let Some(jump) = default_jump {
                self.update_jump_target_to_next(jump);
            }
            self.table_default_to_next(table.as_ref());
            let (op1_type, op1) = self.operand(subject);
            self.push(Opline {
                opcode: Opcode::MatchError,
                op1_type,
                op1,
                ..Opline::default()
            });
        }
        let result = self.match_arms(arms, &mut table, &jumps, default_jump)?;
        self.lay_table(table)?;
        self.free_subject(subject);
        Ok(match result {
            Some(out) => Where::Temporary(out),
            None => Where::Constant(Zval::null()),
        })
    }

    /// `can_match_use_jumptable` and `count_match_conds`: every
    /// condition a constant `long` or `string`, and two or more of them.
    fn match_uses_jumptable(&mut self, arms: &[Arm]) -> Result<bool, CompileError> {
        let mut count = 0;
        for arm in arms {
            let Some(conds) = &arm.conds else {
                continue;
            };
            for (cond, _) in conds {
                count += 1;
                let Some(value) = self.constant_of(cond)? else {
                    return Ok(false);
                };
                if !matches!(value.kind(), ZvalType::Long | ZvalType::String) {
                    return Ok(false);
                }
            }
        }
        Ok(count >= 2)
    }

    /// The chain of `zend_compile_match`: per condition, at its line, one
    /// `IS_IDENTICAL` (`CASE_STRICT` over a temporary subject) into the
    /// shared temporary and a `JMPNZ` forward. The jump of each condition.
    fn match_chain(
        &mut self,
        subject: Where,
        case_node: u32,
        arms: &[Arm],
    ) -> Result<Vec<u32>, CompileError> {
        let mut jumps = Vec::new();
        for (cond, cond_line) in arms.iter().filter_map(|arm| arm.conds.as_ref()).flatten() {
            self.line = *cond_line;
            let cond = self.expr(cond)?;
            let opcode = match subject {
                Where::Temporary(_) | Where::Returned(_) => Opcode::CaseStrict,
                _ => Opcode::IsIdentical,
            };
            self.case(opcode, subject, cond, case_node);
            jumps.push(self.cond_jump_forward(Opcode::Jmpnz, Where::Temporary(case_node)));
        }
        Ok(jumps)
    }

    /// The arms of `zend_compile_match`: each condition's jump aimed at
    /// its arm and the table's target recorded, the default's `JMP` and
    /// the table's default at the default arm; the body as a `QM_ASSIGN`
    /// into the one temporary, `zend_emit_op_tmp` allocating it at the
    /// first arm, and a `JMP` past the arms, every one aimed at the
    /// opline after the last. That temporary, none for no arm.
    fn match_arms(
        &mut self,
        arms: &[Arm],
        table: &mut Option<Table>,
        jumps: &[u32],
        default_jump: Option<u32>,
    ) -> Result<Option<u32>, CompileError> {
        let mut result = None;
        let mut ends = Vec::with_capacity(arms.len());
        let mut jumps = jumps.iter().copied();
        for arm in arms {
            if let Some(conds) = &arm.conds {
                for (cond, _) in conds {
                    if let Some(jump) = jumps.next() {
                        self.update_jump_target_to_next(jump);
                    }
                    self.table_add(table, cond)?;
                }
            } else {
                if let Some(jump) = default_jump {
                    self.update_jump_target_to_next(jump);
                }
                self.table_default_to_next(table.as_ref());
            }
            self.line = arm.line;
            let body = self.expr(&arm.body)?;
            let (op1_type, op1) = self.operand(body);
            let out = *result.get_or_insert_with(|| self.temporary());
            self.push(Opline {
                opcode: Opcode::QmAssign,
                op1_type,
                op1,
                out_type: OperandKind::Tmp,
                out,
                ..Opline::default()
            });
            ends.push(self.push(Opline {
                opcode: Opcode::Jmp,
                ..Opline::default()
            }));
        }
        for end in ends {
            self.update_jump_target_to_next(end);
        }
        Ok(result)
    }

    /// `zend_error_noreturn(E_COMPILE_ERROR, …)` at the line the
    /// compiler stands on: php's own sentence for what its compiler
    /// refuses after the parse, which ends the request as its
    /// `E_COMPILE_ERROR` does.
    fn compile_error(&self, message: impl Into<String>) -> CompileError {
        CompileError {
            level: Level::CompileError,
            message: message.into(),
            line: self.line,
            raised: Vec::new(),
        }
    }

    /// `zend_compile_for_expr_list`: every expression compiled in turn,
    /// each one's value freed before the next is compiled, and where the
    /// last one left its value; the constant `true` for a list with
    /// nothing in it, which is what `for (;;)` tests.
    fn expr_list(&mut self, exprs: &[Expr]) -> Result<Where, CompileError> {
        let mut last = Where::Constant(Zval::bool(true));
        for expr in exprs {
            self.do_free(last);
            last = self.expr(expr)?;
        }
        Ok(last)
    }

    /// `zend_do_free` over a value nothing reads (`Z/Zend/zend_compile.c:755`).
    /// A temporary: when the last opline is the one that left it, a
    /// `BOOL` or `BOOL_NOT` needs no freeing; a `POST_INC` or `POST_DEC`
    /// becomes the `PRE_*` php converts it to, "convert $i++ to ++$i",
    /// with its `result` marked `UNUSED`, which is the opline php's dump
    /// prints for `$i++` as a statement or a loop's step; an `ASSIGN*`
    /// or a `PRE_*` has its `result` marked `UNUSED`; any other
    /// temporary is freed by a `FREE`. What a call returned, a `VAR`:
    /// when the last opline is the call that left it, its `result` is
    /// marked `UNUSED`, and a `FETCH_THIS` becomes a `NOP`; a frameless
    /// call is freed by a `FREE` always. Otherwise the oplines are walked
    /// back to the one that left it, and only a `NEW`'s value, or one a
    /// `FETCH_LIST_*` read, is freed. A constant and a CV are nobody's to
    /// free. `SET_UNUSED` is the kind alone here: the number php
    /// overwrites with -1 is read by nothing once the kind is `UNUSED`.
    ///
    /// php steps back over `END_SILENCE` and `OP_DATA` to reach the
    /// opline that left the value, and for a `VAR` over `EXT_FCALL_END`
    /// too, which is never emitted here; so `@f();` marks the call's
    /// `VAR` unused through the `END_SILENCE` after it, and `@$a = 1;`
    /// the `ASSIGN`'s temporary.
    fn do_free(&mut self, value: Where) {
        match value {
            Where::Temporary(at) => {
                if let Some(last) = self.last_that_left_a_value()
                    && last.out_type == OperandKind::Tmp
                    && last.out == at
                {
                    match last.opcode {
                        Opcode::Bool | Opcode::BoolNot => return,
                        Opcode::PostInc | Opcode::PostDec => {
                            last.opcode = if last.opcode == Opcode::PostInc {
                                Opcode::PreInc
                            } else {
                                Opcode::PreDec
                            };
                            last.out_type = OperandKind::Unused;
                            return;
                        }
                        Opcode::PostIncStaticProp
                        | Opcode::PostDecStaticProp
                        | Opcode::PostIncObj
                        | Opcode::PostDecObj => {
                            last.opcode = match last.opcode {
                                Opcode::PostIncStaticProp => Opcode::PreIncStaticProp,
                                Opcode::PostDecStaticProp => Opcode::PreDecStaticProp,
                                Opcode::PostIncObj => Opcode::PreIncObj,
                                _ => Opcode::PreDecObj,
                            };
                            last.out_type = OperandKind::Unused;
                            return;
                        }
                        Opcode::Assign
                        | Opcode::AssignDim
                        | Opcode::AssignObj
                        | Opcode::AssignStaticProp
                        | Opcode::AssignOp
                        | Opcode::AssignDimOp
                        | Opcode::AssignObjOp
                        | Opcode::AssignStaticPropOp
                        | Opcode::PreIncStaticProp
                        | Opcode::PreDecStaticProp
                        | Opcode::PreIncObj
                        | Opcode::PreDecObj
                        | Opcode::PreInc
                        | Opcode::PreDec => {
                            last.out_type = OperandKind::Unused;
                            return;
                        }
                        _ => {}
                    }
                }
                self.free(OperandKind::Tmp, at);
            }
            Where::Returned(at) => {
                let Some(last) = self.last_that_left_a_value() else {
                    return;
                };
                if last.out_type == OperandKind::Var && last.out == at {
                    if last.opcode == Opcode::FetchThis {
                        last.opcode = Opcode::Nop;
                    }
                    if last.opcode.is_frameless_icall() {
                        self.free(OperandKind::Var, at);
                    } else {
                        last.out_type = OperandKind::Unused;
                    }
                    return;
                }
                for opline in self.oplines.iter().rev() {
                    if matches!(
                        opline.opcode,
                        Opcode::FetchListR | Opcode::FetchListW | Opcode::ExtStmt
                    ) && opline.op1_type == OperandKind::Var
                        && opline.op1 == at
                    {
                        self.free(OperandKind::Var, at);
                        return;
                    }
                    if opline.out_type == OperandKind::Var && opline.out == at {
                        if opline.opcode == Opcode::New {
                            self.free(OperandKind::Var, at);
                        }
                        return;
                    }
                }
            }
            Where::Constant(_) | Where::Cv(_) => {}
        }
    }

    /// The last opline that is no `OP_DATA` and no `END_SILENCE`, which
    /// is the one that left the value `zend_do_free` is given: php walks
    /// back over both from the last opline before it reads what one left.
    fn last_that_left_a_value(&mut self) -> Option<&mut Opline> {
        self.oplines
            .iter_mut()
            .rev()
            .find(|opline| !matches!(opline.opcode, Opcode::OpData | Opcode::EndSilence))
    }

    /// `zend_emit_op(NULL, ZEND_FREE, op1, NULL)`: the temporary or the
    /// `VAR` freed where it stands.
    fn free(&mut self, kind: OperandKind, at: u32) {
        self.push(Opline {
            opcode: Opcode::Free,
            op1_type: kind,
            op1: at,
            ..Opline::default()
        });
    }

    /// `zend_emit_cond_jump(ZEND_JMPNZ, ...)`: the condition tested and
    /// the jump to `target` taken when it holds, the target in `op2`. A
    /// target at or before the opline is a back-edge, and the opline
    /// carries the number of its target, the loop's header, numbered
    /// here since a `for` and a `while` have this one backward jump
    /// (design §5.7: headers are numbered, not back-edges).
    fn emit_cond_jump(&mut self, cond: Where, target: u32) {
        let (op1_type, op1) = self.operand(cond);
        let at = self.next();
        let out = if target <= at {
            self.safepoint(target)
        } else {
            0
        };
        self.push(Opline {
            opcode: Opcode::Jmpnz,
            op1_type,
            op1,
            op2: target,
            out,
            ..Opline::default()
        });
    }

    /// `zend_emit_cond_jump(opcode, cond, 0)`: the condition tested and
    /// the jump forward to a target not yet known, which
    /// [`Self::update_jump_target_to_next`] fills in; the opline's number
    /// for it.
    fn cond_jump_forward(&mut self, opcode: Opcode, cond: Where) -> u32 {
        let (op1_type, op1) = self.operand(cond);
        self.push(Opline {
            opcode,
            op1_type,
            op1,
            ..Opline::default()
        })
    }

    /// `zend_update_jump_target_to_next`: the jump at `jump` aimed at
    /// the opline about to be emitted, in the field its opcode keeps a
    /// target in, `op1` for a `JMP` and `op2` for the conditional jumps.
    fn update_jump_target_to_next(&mut self, jump: u32) {
        let target = self.next();
        if let Some(opline) = self.oplines.get_mut(jump as usize) {
            if opline.opcode == Opcode::Jmp {
                opline.op1 = target;
            } else {
                opline.op2 = target;
            }
        }
    }

    /// `get_next_op_number`: the number the next opline will have.
    fn next(&self) -> u32 {
        u32::try_from(self.oplines.len()).unwrap_or(u32::MAX)
    }

    /// The operand fields one [`Where`] fills. A constant reaches the
    /// table here and not before, as `zend_add_literal` adds it when the
    /// opline that names it is emitted, which is what puts the table's
    /// slots in php's own order.
    fn operand(&mut self, value: Where) -> (OperandKind, u32) {
        match value {
            Where::Constant(value) => (OperandKind::Const, self.constant(value)),
            Where::Returned(at) => (OperandKind::Var, at),
            Where::Temporary(at) => (OperandKind::Tmp, at),
            Where::Cv(at) => (OperandKind::Cv, at),
        }
    }

    /// `zend_compile_expr`: compile `expr` and say where its value is.
    pub(crate) fn expr(&mut self, expr: &Expr) -> Result<Where, CompileError> {
        self.check_stack()?;
        match expr {
            Expr::Call { name, arguments } => self.call(name, arguments),
            Expr::Unary { minus, operand } => self.unary(*minus, operand),
            Expr::Silence(operand) => self.silence(operand),
            Expr::Encaps(pieces) => self.encaps(pieces),
            Expr::Cast { to, operand } => self.cast(*to, operand),
            Expr::Binary { op, left, right } => self.binary(*op, left, right),
            // `zend_compile_simple_var` through `zend_try_compile_cv`: the
            // variable is its CV, and no opline reads it into anything.
            Expr::Var(name) => Ok(Where::Cv(self.lookup_cv(name))),
            // `zend_compile_simple_var` for `$this` under `BP_VAR_R`:
            // `FETCH_THIS` into a temporary.
            Expr::This => Ok(self.fetch_this(OperandKind::Tmp)),
            Expr::Prop { object, name } => self.prop(object, name),
            Expr::Assign { var, value } => self.assign(var, value),
            Expr::AssignProp {
                object,
                name,
                value,
            } => self.assign_prop(object, name, value),
            Expr::AssignRef { var, source } => Ok(self.assign_ref(var, source)),
            Expr::PreInc(var) => Ok(self.incdec(var, Opcode::PreInc)),
            Expr::PostInc(var) => Ok(self.incdec(var, Opcode::PostInc)),
            Expr::PreDec(var) => Ok(self.incdec(var, Opcode::PreDec)),
            Expr::PostDec(var) => Ok(self.incdec(var, Opcode::PostDec)),
            Expr::Match { expr, arms, line } => self.r#match(expr, arms, *line),
            Expr::New { class, arguments } => self.new_expression(class, arguments),
            Expr::StaticCall {
                class,
                method,
                arguments,
            } => self.static_call(class, method, arguments),
            Expr::MethodCall {
                object,
                method,
                arguments,
            } => self.method_call(object, method, arguments),
            Expr::Instanceof { expr, class } => self.instance_of(expr, class),
            Expr::ClassConst { class, name } => self.class_const(class, name),
            Expr::StaticProp { class, name } => self.static_prop(class, name),
            Expr::Array(elements) => self.array(elements),
            Expr::Throw(operand) => self.throw(operand),
            known => Ok(Where::Constant(self.constant_expr(known)?)),
        }
    }

    /// `zend_compile_throw` (`Z/Zend/zend_compile.c:5795`): the operand,
    /// then `ZEND_THROW` over it with nothing in `result`. The value the
    /// expression leaves is the constant `true`, php's `ZVAL_TRUE` for
    /// an expression `throw`; nothing downstream reads it, since the
    /// opline leaves a throwable pending whatever stands after it, and
    /// `zend_do_free` over it is what leaves `throw new E;` its one
    /// opline.
    fn throw(&mut self, operand: &Expr) -> Result<Where, CompileError> {
        let value = self.expr(operand)?;
        let (op1_type, op1) = self.operand(value);
        self.push(Opline {
            opcode: Opcode::Throw,
            op1_type,
            op1,
            ..Opline::default()
        });
        Ok(Where::Constant(Zval::bool(true)))
    }

    /// `zend_compile_pre_incdec` and `zend_compile_post_incdec`, the
    /// `ZEND_AST_VAR` arm: the variable's CV (`zend_compile_var` under
    /// `BP_VAR_RW`, which delays nothing for a plain CV), then the opcode
    /// over it into a temporary (`zend_emit_op_tmp`), `op2` unused.
    fn incdec(&mut self, var: &[u8], opcode: Opcode) -> Where {
        let cv = self.lookup_cv(var);
        let out = self.temporary();
        self.push(Opline {
            opcode,
            op1_type: OperandKind::Cv,
            op1: cv,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        Where::Temporary(out)
    }

    /// `zend_compile_assign`, the `ZEND_AST_VAR` arm: the variable's CV
    /// looked up first (`zend_delayed_compile_var`, which delays nothing
    /// for a plain CV), then the value, then `ASSIGN` into a temporary
    /// (`zend_emit_op_tmp`). The temporary is numbered whether or not
    /// anything reads it; [`Self::do_free`] marks it `UNUSED` when
    /// nothing does, which is what the engine's `ASSIGN` reads as
    /// `RETURN_VALUE_USED`.
    fn assign(&mut self, var: &[u8], value: &Expr) -> Result<Where, CompileError> {
        let cv = self.lookup_cv(var);
        let value = self.expr(value)?;
        let (op2_type, op2) = self.operand(value);
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::Assign,
            op1_type: OperandKind::Cv,
            op1: cv,
            op2_type,
            op2,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_assign_ref` (`Z/Zend/zend_compile.c:3549`) for
    /// `$b = &$a` over two variables: the target's CV looked up first
    /// (`zend_delayed_compile_var` under `BP_VAR_W`, which delays nothing
    /// for a plain CV), the source's after it (`zend_compile_var` under
    /// `BP_VAR_W`), then `ASSIGN_REF` over the two into a `VAR`
    /// (`zend_emit_op`), `extended_value` zero since the source is no
    /// call (`ZEND_RETURNS_FUNCTION`). No `MAKE_REF` stands between
    /// them: php emits one only where the source is neither a CV nor
    /// bound for a plain variable. [`Self::do_free`] marks the `VAR`
    /// `UNUSED` where nothing reads it, as it marks a call's.
    fn assign_ref(&mut self, var: &[u8], source: &[u8]) -> Where {
        let target = self.lookup_cv(var);
        let source = self.lookup_cv(source);
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::AssignRef,
            op1_type: OperandKind::Cv,
            op1: target,
            op2_type: OperandKind::Cv,
            op2: source,
            out_type: OperandKind::Var,
            out,
            ..Opline::default()
        });
        Where::Returned(out)
    }

    /// `zend_compile_assign`, the `ZEND_AST_PROP` arm: the property
    /// fetch compiled delayed (`zend_delayed_compile_prop` under
    /// `BP_VAR_W`), the value after it, the delayed oplines laid down at
    /// `zend_delayed_compile_end` with the last of them turned into
    /// `ASSIGN_OBJ` writing a temporary, and `OP_DATA` carrying the value
    /// (`zend_emit_op_data`). What is delayed is the chain of property
    /// fetches alone: the receiver under the chain is compiled where it
    /// stands, before the value, as `zend_delayed_compile_var` compiles
    /// a CV or `$this` (`FETCH_THIS` into a `VAR`); each fetch but the
    /// last is `FETCH_OBJ_W` into a `VAR` (`zend_adjust_for_fetch_type`
    /// under `BP_VAR_W`), its name one constant in `op2` and three cache
    /// slots in `extended_value`, and its `VAR` is numbered, its name
    /// added and its slots taken when the fetch is delayed, before the
    /// value's own, which is the order php's temporaries come in. A
    /// call under the chain is php's `SEPARATE` over its `VAR`
    /// (`zend_separate_if_call_and_write`), and anything else under it
    /// is php's compile error for a temporary expression in a write;
    /// neither is read yet.
    fn assign_prop(
        &mut self,
        object: &Expr,
        name: &[u8],
        value: &Expr,
    ) -> Result<Where, CompileError> {
        let mut names = vec![name];
        let mut base = object;
        while let Expr::Prop { object, name } = base {
            names.push(name);
            base = object;
        }
        if !matches!(base, Expr::Var(_) | Expr::This) {
            return Err(self.fatal(
                "a property assignment over what is no variable this compiler cannot read yet",
            ));
        }
        let (mut op1_type, mut op1) = self.receiver(base)?;
        let mut links = Vec::with_capacity(names.len());
        for name in names.iter().rev() {
            let name = self.string(name)?;
            let name_at = self.literal(name);
            let out = self.temporary();
            let slots = self.cache_slots(3);
            links.push((name_at, out, slots));
        }
        let value = self.expr(value)?;
        let (data_type, data) = self.operand(value);
        let Some((last, fetches)) = links.split_last() else {
            return Err(self.fatal("a property assignment with no property"));
        };
        for &(name_at, out, slots) in fetches {
            self.push(Opline {
                opcode: Opcode::FetchObjW,
                op1_type,
                op1,
                op2_type: OperandKind::Const,
                op2: name_at,
                out_type: OperandKind::Var,
                out,
                extended_value: slots,
                ..Opline::default()
            });
            op1_type = OperandKind::Var;
            op1 = out;
        }
        let &(name_at, out, slots) = last;
        self.push(Opline {
            opcode: Opcode::AssignObj,
            op1_type,
            op1,
            op2_type: OperandKind::Const,
            op2: name_at,
            out_type: OperandKind::Tmp,
            out,
            extended_value: slots,
            ..Opline::default()
        });
        self.push(Opline {
            opcode: Opcode::OpData,
            op1_type: data_type,
            op1: data,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_binary_op`: the value where
    /// `zend_try_ct_eval_binary_op` folds the operator over two constants,
    /// and the opcode into a temporary where it does not
    /// (`zend_emit_op_tmp`).
    fn binary(&mut self, op: BinaryOp, left: &Expr, right: &Expr) -> Result<Where, CompileError> {
        if op == BinaryOp::Concat {
            return self.concat(left, right);
        }
        if compares(op) {
            return self.comparison(op, left, right);
        }
        if let (Some(a), Some(b)) = (self.constant_of(left)?, self.constant_of(right)?)
            && let Some(folded) = fold(op, a, b)
        {
            return Ok(Where::Constant(folded));
        }
        let opcode = Self::opcode_of(op);
        let left = self.expr(left)?;
        let right = self.expr(right)?;
        Ok(self.emit_binary(opcode, left, right))
    }

    /// `zend_compile_binary_op`'s `ZEND_CONCAT` arm: both sides
    /// compiled, the fold over two constants, and otherwise each constant
    /// side converted to a string where it stands and `CONCAT` into a
    /// temporary. php converts before it emits, so what the opline reads
    /// is the string and never the number it was written as.
    fn concat(&mut self, left: &Expr, right: &Expr) -> Result<Where, CompileError> {
        let left = self.expr(left)?;
        let right = self.expr(right)?;
        if let (Where::Constant(a), Where::Constant(b)) = (left, right)
            && let Some(folded) = self.concat_fold(a, b)?
        {
            return Ok(Where::Constant(folded));
        }
        let left = self.string_operand(left)?;
        let right = self.string_operand(right)?;
        Ok(self.emit_binary(Opcode::Concat, left, right))
    }

    /// `zend_compile_binary_op`'s comparison arms and `zend_compile_greater`:
    /// both sides compiled, the fold over two constants, which
    /// `zend_binary_op_produces_error` never refuses for a comparison; a
    /// `===` or `!==` against a constant null, false or true as the
    /// `TYPE_CHECK` php emits in its place, `$a === null` being
    /// `is_null($a)`; and otherwise the opcode into a temporary, `>` and
    /// `>=` as `IS_SMALLER` and `IS_SMALLER_OR_EQUAL` with the operands
    /// swapped, which is the one shape php has for them.
    fn comparison(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
    ) -> Result<Where, CompileError> {
        let left = self.expr(left)?;
        let right = self.expr(right)?;
        if let (Where::Constant(a), Where::Constant(b)) = (left, right) {
            return Ok(Where::Constant(self.compare_fold(op, a, b)));
        }
        if let Some((operand, mask)) = type_check(op, left, right) {
            return Ok(self.emit_unary(Opcode::TypeCheck, operand, mask));
        }
        Ok(match op {
            BinaryOp::Greater => self.emit_binary(Opcode::IsSmaller, right, left),
            BinaryOp::GreaterOrEqual => self.emit_binary(Opcode::IsSmallerOrEqual, right, left),
            _ => self.emit_binary(Self::opcode_of(op), left, right),
        })
    }

    /// `zend_try_ct_eval_binary_op` over `is_smaller_function` and its
    /// kin, and `zend_ct_eval_greater`, which calls the same two with
    /// the constants swapped: the bool each comparison reads off
    /// [`php_heap::compare`]'s ordering, the long `compare_function`
    /// writes for `<=>`, and [`php_heap::identical`] for `===` and `!==`,
    /// at the compile-time `precision` a double is rendered at against a
    /// string that is not numeric.
    fn compare_fold(&self, op: BinaryOp, left: Zval, right: Zval) -> Zval {
        let (a, b) = (left.as_str(), right.as_str());
        let (left, right) = (scalar(left, a.as_ref()), scalar(right, b.as_ref()));
        let compare = |l, r| php_heap::compare(l, r, self.precision);
        match op {
            BinaryOp::IsIdentical => Zval::bool(php_heap::identical(left, right)),
            BinaryOp::IsNotIdentical => Zval::bool(!php_heap::identical(left, right)),
            BinaryOp::Greater => Zval::bool(compare(right, left) == Ordering::Less),
            BinaryOp::GreaterOrEqual => Zval::bool(compare(right, left) != Ordering::Greater),
            BinaryOp::IsSmaller => Zval::bool(compare(left, right) == Ordering::Less),
            BinaryOp::IsSmallerOrEqual => Zval::bool(compare(left, right) != Ordering::Greater),
            BinaryOp::IsEqual => Zval::bool(compare(left, right) == Ordering::Equal),
            BinaryOp::IsNotEqual => Zval::bool(compare(left, right) != Ordering::Equal),
            BinaryOp::Spaceship => Zval::long(match compare(left, right) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            }),
            // An arithmetic operator is [`fold`]'s and a `.` is
            // [`Emit::concat_fold`]'s; neither reaches here.
            BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Mod
            | BinaryOp::Pow
            | BinaryOp::Sl
            | BinaryOp::Sr
            | BinaryOp::Concat => Zval::null(),
        }
    }

    /// `zend_try_ct_eval_binary_op` over `concat_function`: the one
    /// string the two constants join into, or nothing where either is
    /// left to the opcode.
    pub(crate) fn concat_fold(
        &mut self,
        left: Zval,
        right: Zval,
    ) -> Result<Option<Zval>, CompileError> {
        let (Some(a), Some(b)) = (self.stringified(left)?, self.stringified(right)?) else {
            return Ok(None);
        };
        let (Some(a), Some(b)) = (a.as_str(), b.as_str()) else {
            return Ok(None);
        };
        let joined = [a.bytes(), b.bytes()].concat();
        Ok(Some(self.string(&joined)?))
    }

    /// A constant operand of a `.` as the string `zend_compile_binary_op`
    /// converts it to; any other operand as it is.
    fn string_operand(&mut self, value: Where) -> Result<Where, CompileError> {
        match value {
            Where::Constant(v) => Ok(Where::Constant(self.stringified(v)?.unwrap_or(v))),
            other => Ok(other),
        }
    }

    /// `_convert_to_string` over a constant: null and false the empty
    /// string, true `"1"`, a long its digits, a double the text
    /// `zend_double_to_str` renders at the compile-time precision, a
    /// string itself. A NAN is the text `NAN` and the warning
    /// `zend_double_to_str` raises after rendering it
    /// (`Z/Zend/zend_operators.c:3600`), raised while the file compiles,
    /// so it stands before anything the program writes and the op
    /// array carries the string, as php's does.
    ///
    /// ```php
    /// echo 'a'; echo NAN . 'z';
    /// ```
    /// ```expect
    /// Warning: unexpected NAN value was coerced to string in %s on line 2
    /// aNANz
    /// ```
    ///
    /// The warning is the compilation's and not the statement's: it is
    /// raised before `ini_set()` runs, and once per operand converted.
    ///
    /// ```php
    /// ini_set('error_reporting', '0');
    /// $a = 1; echo NAN . $a . NAN;
    /// ```
    /// ```expect
    /// Warning: unexpected NAN value was coerced to string in %s on line 3
    ///
    /// Warning: unexpected NAN value was coerced to string in %s on line 3
    /// NAN1NAN
    /// ```
    pub(crate) fn stringified(&mut self, value: Zval) -> Result<Option<Zval>, CompileError> {
        if let Some(n) = value.as_long() {
            return Ok(Some(self.string(n.to_string().as_bytes())?));
        }
        if let Some(d) = value.as_double() {
            let text = self.string(double_to_string(d, self.precision).as_bytes())?;
            if d.is_nan() {
                self.raise(Level::Warning, "unexpected NAN value was coerced to string");
            }
            return Ok(Some(text));
        }
        match value.kind() {
            ZvalType::String => Ok(Some(value)),
            ZvalType::True => Ok(Some(self.string(b"1")?)),
            ZvalType::Null | ZvalType::False => Ok(Some(self.string(b"")?)),
            _ => Ok(None),
        }
    }

    /// `zend_compile_unary_pm`: php folds unary `-` and `+` as the
    /// multiplication by -1 and by 1 it would otherwise emit, and emits
    /// that multiplication where the operand is not a constant.
    fn unary(&mut self, minus: bool, operand: &Expr) -> Result<Where, CompileError> {
        let sign = Zval::long(if minus { -1 } else { 1 });
        if let Some(value) = self.constant_of(operand)?
            && let Some(folded) = fold(BinaryOp::Mul, value, sign)
        {
            return Ok(Where::Constant(folded));
        }
        let operand = self.expr(operand)?;
        Ok(self.emit_binary(Opcode::Mul, operand, Where::Constant(sign)))
    }

    /// `zend_compile_silence` (`Z/Zend/zend_compile.c:10896`):
    /// `BEGIN_SILENCE` into a temporary (`zend_emit_op_tmp`), the
    /// expression, and `END_SILENCE` over that temporary with nothing
    /// left. A variable as the expression, `$a` or `$this`, is
    /// `zend_compile_simple_var_no_cv` under `BP_VAR_R`: `FETCH_R` over
    /// the name's constant string, `ZEND_FETCH_LOCAL` in
    /// `extended_value`, into a temporary
    /// (`zend_adjust_for_fetch_type`), "otherwise the CV access will
    /// happen outside the silenced section"; `$_GET` and its kin, which
    /// php fetches `ZEND_FETCH_GLOBAL`, are refused by the reader. The
    /// value is where the expression left it, and its `zend_do_free`
    /// steps back over the `END_SILENCE` ([`Self::last_that_left_a_value`]).
    fn silence(&mut self, operand: &Expr) -> Result<Where, CompileError> {
        let silence = self.temporary();
        self.push(Opline {
            opcode: Opcode::BeginSilence,
            out_type: OperandKind::Tmp,
            out: silence,
            ..Opline::default()
        });
        let value = match operand {
            Expr::Var(name) => self.fetch_by_name(name)?,
            Expr::This => self.fetch_by_name(b"this")?,
            _ => self.expr(operand)?,
        };
        self.push(Opline {
            opcode: Opcode::EndSilence,
            op1_type: OperandKind::Tmp,
            op1: silence,
            ..Opline::default()
        });
        Ok(value)
    }

    /// `zend_compile_simple_var_no_cv` under `BP_VAR_R` for a name that
    /// is no auto-global: `FETCH_R (local)` over the name's constant
    /// into a temporary.
    fn fetch_by_name(&mut self, name: &[u8]) -> Result<Where, CompileError> {
        let name = self.string(name)?;
        let at = self.constant(name);
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::FetchR,
            op1_type: OperandKind::Const,
            op1: at,
            out_type: OperandKind::Tmp,
            out,
            extended_value: FETCH_LOCAL,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_encaps_list` (`Z/Zend/zend_compile.c:11221`): each
    /// piece compiled in order; a constant piece is converted to a string,
    /// dropped when empty, joined onto the constant before it
    /// (`concat_function`), or held with a `NOP` reserved for the
    /// `ROPE_ADD` it becomes once the next variable piece, or the end,
    /// arrives; a variable piece is `zend_compile_rope_add`, `ROPE_INIT`
    /// for the first element and `ROPE_ADD` after, the element's number
    /// in `extended_value` and the rope's temporary unassigned until
    /// [`Self::rope_finalize`]. `${a}` is `ZEND_ENCAPS_VAR_DOLLAR_CURLY`,
    /// deprecated before its piece compiles. A list with no variable
    /// piece is the joined constant, which the reader never hands here
    /// since such a string is [`Expr::Str`].
    fn encaps(&mut self, pieces: &[Piece]) -> Result<Where, CompileError> {
        let mut last_const: Option<Zval> = None;
        let mut reserved: Option<usize> = None;
        let mut init_at: Option<usize> = None;
        let mut count = 0u32;
        for piece in pieces {
            if piece.dollar_curly {
                self.raise(
                    Level::Deprecated,
                    "Using ${var} in strings is deprecated, use {$var} instead",
                );
            }
            let value = self.expr(&piece.expr)?;
            if let Where::Constant(value) = value {
                let value = self.stringified(value)?.unwrap_or(value);
                if value.as_str().is_some_and(|s| s.bytes().is_empty()) {
                    continue;
                }
                last_const = Some(if let Some(held) = last_const.take() {
                    self.concat_fold(held, value)?.unwrap_or(value)
                } else {
                    reserved = Some(self.oplines.len());
                    self.push(Opline::default());
                    value
                });
                continue;
            }
            if count == 0 {
                init_at = Some(match (&last_const, reserved) {
                    (Some(_), Some(at)) => at,
                    _ => self.oplines.len(),
                });
            }
            if let (Some(held), Some(at)) = (last_const.take(), reserved) {
                self.rope_add_at(at, count, Where::Constant(held));
                count += 1;
            }
            let at = self.oplines.len();
            self.push(Opline::default());
            self.rope_add_at(at, count, value);
            count += 1;
        }
        if count == 0 {
            if let Some(at) = reserved {
                self.oplines.truncate(at);
            }
            return Ok(Where::Constant(match last_const {
                Some(held) => held,
                None => self.string(b"")?,
            }));
        }
        if let (Some(held), Some(at)) = (last_const.take(), reserved) {
            self.rope_add_at(at, count, Where::Constant(held));
            count += 1;
        }
        let Some(init_at) = init_at else {
            return Err(self.fatal("a rope with elements and no first"));
        };
        Ok(self.rope_finalize(count, init_at))
    }

    /// The rope's temporary before `zend_compile_rope_finalize` assigns
    /// it, php's `result->u.op.var = -1`.
    const ROPE_UNASSIGNED: u32 = u32::MAX;

    /// `zend_compile_rope_add_ex` over the opline at `at`: `ROPE_INIT`
    /// for element 0 and `ROPE_ADD` over the rope's temporary after it,
    /// the element in `op2`, the rope's temporary as what it leaves, and
    /// the element's number in `extended_value`.
    fn rope_add_at(&mut self, at: usize, num: u32, element: Where) {
        let (op2_type, op2) = self.operand(element);
        let Some(opline) = self.oplines.get_mut(at) else {
            return;
        };
        if num == 0 {
            opline.opcode = Opcode::RopeInit;
        } else {
            opline.opcode = Opcode::RopeAdd;
            opline.op1_type = OperandKind::Tmp;
            opline.op1 = Self::ROPE_UNASSIGNED;
        }
        opline.op2_type = op2_type;
        opline.op2 = op2;
        opline.out_type = OperandKind::Tmp;
        opline.out = Self::ROPE_UNASSIGNED;
        opline.extended_value = num;
    }

    /// `zend_compile_rope_finalize` (`Z/Zend/zend_compile.c:11186`) over
    /// the `count` elements from the opline at `init_at` to the last:
    /// one element is a `CAST (string)` of it into a temporary, the one
    /// opline rewritten; two are `FAST_CONCAT` of the first over the
    /// second's opline into a temporary, the first's opline made a `NOP`
    /// (`MAKE_NOP`); more are the count in `ROPE_INIT`'s
    /// `extended_value`, the last opline `ROPE_END` into a temporary
    /// with the rope's temporary as its `op1`, `zend_make_tmp_result`
    /// before `get_temporary_variable`, then one slot fewer than the
    /// `zval`s the `count` string pointers fill, and every opline of the
    /// rope given that temporary.
    fn rope_finalize(&mut self, count: u32, init_at: usize) -> Where {
        let last_at = self.oplines.len().saturating_sub(1);
        if count == 1 {
            let out = self.temporary();
            if let Some(opline) = self.oplines.get_mut(last_at) {
                opline.opcode = Opcode::Cast;
                opline.extended_value = CastTo::Str.type_code();
                opline.op1_type = opline.op2_type;
                opline.op1 = opline.op2;
                opline.op2_type = OperandKind::Unused;
                opline.op2 = 0;
                opline.out = out;
            }
            return Where::Temporary(out);
        }
        if count == 2 {
            let out = self.temporary();
            let (first_type, first) = self
                .oplines
                .get(init_at)
                .map_or((OperandKind::Unused, 0), |init| (init.op2_type, init.op2));
            if let Some(opline) = self.oplines.get_mut(last_at) {
                opline.opcode = Opcode::FastConcat;
                opline.extended_value = 0;
                opline.op1_type = first_type;
                opline.op1 = first;
                opline.out = out;
            }
            if let Some(init) = self.oplines.get_mut(init_at) {
                let lineno = init.lineno;
                *init = Opline {
                    lineno,
                    ..Opline::default()
                };
            }
            return Where::Temporary(out);
        }
        let out = self.temporary();
        let rope = self.temporary();
        let mut slots = (count as usize * size_of::<usize>()).div_ceil(ZVAL_SIZE);
        while slots > 1 {
            self.temporary();
            slots -= 1;
        }
        if let Some(init) = self.oplines.get_mut(init_at) {
            init.extended_value = count;
        }
        if let Some(opline) = self.oplines.get_mut(last_at) {
            opline.opcode = Opcode::RopeEnd;
            opline.out = out;
            opline.op1 = rope;
        }
        for opline in self.oplines.iter_mut().take(last_at).skip(init_at) {
            if opline.out == Self::ROPE_UNASSIGNED {
                match opline.opcode {
                    Opcode::RopeAdd => {
                        opline.op1 = rope;
                        opline.out = rope;
                    }
                    Opcode::RopeInit => opline.out = rope,
                    _ => {}
                }
            }
        }
        Where::Temporary(out)
    }

    /// `zend_compile_cast`: the operand compiled first, then `ZEND_BOOL`
    /// over it for `(bool)` and `ZEND_CAST` with the type in
    /// `extended_value` for the rest, into a temporary; nothing is folded,
    /// as php folds a cast only inside a constant expression
    /// (`zend_eval_const_expr`), so `(float)1` stands as an opline.
    fn cast(&mut self, to: CastTo, operand: &Expr) -> Result<Where, CompileError> {
        let operand = self.expr(operand)?;
        Ok(match to {
            CastTo::Bool => self.emit_unary(Opcode::Bool, operand, 0),
            _ => self.emit_unary(Opcode::Cast, operand, to.type_code()),
        })
    }

    /// `zend_emit_op_tmp` over one operand: the opline with `op1`,
    /// `extended_value` as the caller sets it after the emission, and a
    /// temporary for what it leaves, which is where the value is.
    pub(crate) fn emit_unary(
        &mut self,
        opcode: Opcode,
        operand: Where,
        extended_value: u32,
    ) -> Where {
        let (op1_type, op1) = self.operand(operand);
        let out = self.temporary();
        self.push(Opline {
            opcode,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            extended_value,
            ..Opline::default()
        });
        Where::Temporary(out)
    }

    /// `zend_emit_op_tmp`: the opline with both operands and a temporary
    /// for what it leaves, which is where the value is.
    fn emit_binary(&mut self, opcode: Opcode, left: Where, right: Where) -> Where {
        let (op1_type, op1) = self.operand(left);
        let (op2_type, op2) = self.operand(right);
        let out = self.temporary();
        self.push(Opline {
            opcode,
            op1_type,
            op1,
            op2_type,
            op2,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        Where::Temporary(out)
    }

    /// The opcode `zend_compile_binary_op` gives each operator.
    fn opcode_of(op: BinaryOp) -> Opcode {
        match op {
            BinaryOp::Add => Opcode::Add,
            BinaryOp::Sub => Opcode::Sub,
            BinaryOp::Mul => Opcode::Mul,
            BinaryOp::Div => Opcode::Div,
            BinaryOp::Mod => Opcode::Mod,
            BinaryOp::Pow => Opcode::Pow,
            BinaryOp::Sl => Opcode::Sl,
            BinaryOp::Sr => Opcode::Sr,
            BinaryOp::Concat => Opcode::Concat,
            BinaryOp::IsSmaller | BinaryOp::Greater => Opcode::IsSmaller,
            BinaryOp::IsSmallerOrEqual | BinaryOp::GreaterOrEqual => Opcode::IsSmallerOrEqual,
            BinaryOp::IsEqual => Opcode::IsEqual,
            BinaryOp::IsNotEqual => Opcode::IsNotEqual,
            BinaryOp::IsIdentical => Opcode::IsIdentical,
            BinaryOp::IsNotIdentical => Opcode::IsNotIdentical,
            BinaryOp::Spaceship => Opcode::Spaceship,
        }
    }

    /// php's constant expression: the value `zend_compile_expr` leaves as
    /// an `IS_CONST` operand, folding a number as written, a persistent constant
    /// (`zend_try_ct_eval_const`), arithmetic over two of them
    /// (`zend_try_ct_eval_binary_op`) and a call of a name in
    /// [`crate::special`]'s table over constants
    /// (`zend_try_compile_special_func`).
    pub(crate) fn constant_expr(&mut self, expr: &Expr) -> Result<Zval, CompileError> {
        self.check_stack()?;
        match expr {
            Expr::Null => Ok(Zval::null()),
            Expr::Bool(b) => Ok(Zval::bool(*b)),
            Expr::Int(n) => Ok(Zval::long(*n)),
            Expr::Float(f) => Ok(Zval::double(*f)),
            Expr::Str(bytes) => self.string(bytes),
            Expr::Constant(name) => self
                .const_name(name)?
                .ok_or_else(|| self.fatal("a constant this compiler cannot read yet")),
            Expr::Magic { which, line } => match self.magic_const(*which, *line) {
                Folded::Line(n) => Ok(Zval::long(n)),
                Folded::Text(text) => self.string(&text),
            },
            // `zend_try_ct_eval_unary_pm`: php folds unary `-` and `+` as
            // the multiplication by -1 and by 1 it would otherwise emit.
            Expr::Unary { minus, operand } => {
                let value = self.constant_expr(operand)?;
                fold(
                    BinaryOp::Mul,
                    value,
                    Zval::long(if *minus { -1 } else { 1 }),
                )
                .ok_or_else(|| self.fatal("arithmetic this compiler cannot fold yet"))
            }
            Expr::Binary {
                op: BinaryOp::Concat,
                left,
                right,
            } => {
                let left = self.constant_expr(left)?;
                let right = self.constant_expr(right)?;
                self.concat_fold(left, right)?
                    .ok_or_else(|| self.fatal("a concatenation this compiler cannot fold yet"))
            }
            Expr::Binary { op, left, right } if compares(*op) => {
                let left = self.constant_expr(left)?;
                let right = self.constant_expr(right)?;
                Ok(self.compare_fold(*op, left, right))
            }
            Expr::Binary { op, left, right } => {
                let left = self.constant_expr(left)?;
                let right = self.constant_expr(right)?;
                fold(*op, left, right)
                    .ok_or_else(|| self.fatal("arithmetic this compiler cannot fold yet"))
            }
            Expr::Call { name, arguments } => {
                let (resolved, runtime) = self.function_name(name);
                let folded = if runtime {
                    None
                } else {
                    self.special_constant(&resolved.to_ascii_lowercase(), arguments)?
                };
                folded.ok_or_else(|| self.fatal("a call this compiler cannot fold"))
            }
            Expr::Array(elements) => self
                .constant_array(elements)?
                .ok_or_else(|| self.fatal("an array expression that is no constant")),
            Expr::Var(_)
            | Expr::This
            | Expr::Prop { .. }
            | Expr::Assign { .. }
            | Expr::AssignProp { .. }
            | Expr::AssignRef { .. }
            | Expr::PreInc(_)
            | Expr::PostInc(_)
            | Expr::PreDec(_)
            | Expr::PostDec(_)
            | Expr::StaticProp { .. } => Err(self.fatal("a variable is no constant expression")),
            Expr::Match { .. } => Err(self.fatal("a match is no constant expression")),
            Expr::Silence(_) => Err(self.fatal("a silenced expression is no constant expression")),
            Expr::Encaps(_) => Err(self.fatal("an interpolated string is no constant expression")),
            Expr::New { .. } | Expr::StaticCall { .. } | Expr::MethodCall { .. } => {
                Err(self.fatal("a call is no constant expression"))
            }
            Expr::Instanceof { .. } => Err(self.fatal("an instanceof is no constant expression")),
            // `zend_try_ct_eval_class_const` folds nothing under
            // `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION`, which opcache
            // sets, so a class constant is fetched as the program serves.
            Expr::ClassConst { .. } => Err(self.fatal("a class constant is fetched at run time")),
            // `zend_compile_expr` folds no cast; only a constant
            // expression's walk does (`zend_eval_const_expr`), and none is
            // read yet.
            Expr::Cast { .. } => Err(self.fatal("a cast is no constant expression here")),
            // `zend_compile_throw` emits an opline whatever stands
            // around it, so a `throw` folds to nothing.
            Expr::Throw(_) => Err(self.fatal("a `throw` is no constant expression")),
        }
    }

    /// `zend_try_ct_eval_array` (`Z/Zend/zend_compile.c:10085`) over an
    /// array expression: nothing unless every key and value folds
    /// (`zend_eval_const_expr` over each, and the walk stops folding
    /// only at the compile error php's does), else one constant array
    /// in the home the unit's strings are in ([`Intern::constant_array`]),
    /// the keys as php writes them, an integer as itself, a string
    /// through `zend_symtable_update`, which makes an integer of a
    /// decimal one, a double truncated where it is a whole number,
    /// `false` and `true` as 0 and 1, and the element with no key at
    /// `nNextFreeElement`, one past the largest integer key so far and 0
    /// before any. A double that is no whole number, a `null` key,
    /// which warns when served, or a next index past `ZEND_LONG_MAX`
    /// leaves the array to the request, php's `fail`, and is nothing
    /// here too; an array or an object as a key is php's `Illegal
    /// offset type`, raised only once every element is known constant,
    /// as php raises it in its second pass.
    fn constant_array(&mut self, elements: &[Element]) -> Result<Option<Zval>, CompileError> {
        let mut folded: Vec<(Option<Zval>, Zval)> = Vec::with_capacity(elements.len());
        for element in elements {
            let key = match &element.key {
                Some(key) => match self.constant_of(key)? {
                    Some(key) => Some(key),
                    None => return Ok(None),
                },
                None => None,
            };
            let Some(value) = self.constant_of(&element.value)? else {
                return Ok(None);
            };
            folded.push((key, value));
        }
        let mut entries: Vec<(Key, Zval)> = Vec::with_capacity(elements.len());
        let mut next_free = i64::MIN;
        for (key, value) in folded {
            let key = match key {
                None => {
                    let index = if next_free == i64::MIN { 0 } else { next_free };
                    if index == i64::MAX && entries.iter().any(|(k, _)| *k == Key::Long(index)) {
                        return Ok(None);
                    }
                    Key::Long(index)
                }
                Some(key) => match key.kind() {
                    ZvalType::Long => Key::Long(key.as_long().unwrap_or(0)),
                    ZvalType::String => match key.as_str() {
                        Some(s) => numeric_key(s.bytes()).map_or(Key::Str(s), Key::Long),
                        None => Key::Long(0),
                    },
                    ZvalType::Double => {
                        let d = key.as_double().unwrap_or(0.0);
                        match long_compatible(Number::Double(d)) {
                            Some(n) => Key::Long(n),
                            None => return Ok(None),
                        }
                    }
                    ZvalType::False => Key::Long(0),
                    ZvalType::True => Key::Long(1),
                    ZvalType::Null => return Ok(None),
                    _ => return Err(self.compile_error("Illegal offset type")),
                },
            };
            if let Key::Long(n) = key
                && n >= next_free
            {
                next_free = n.saturating_add(1);
            }
            match entries.iter_mut().find(|(k, _)| *k == key) {
                Some(held) => held.1 = value,
                None => entries.push((key, value)),
            }
        }
        match self.strings.constant_array(&entries) {
            Ok(array) => Ok(Some(Zval::array(array))),
            Err(Fatal::Error(message)) => Err(self.fatal(message)),
            Err(Fatal::OutOfMemory(_)) => Err(self.fatal("Out of memory")),
        }
    }

    /// `zend_compile_array` (`Z/Zend/zend_compile.c:10935`): the constant
    /// [`Self::constant_array`] folds, else the request builds the array
    /// one element at a time. For each element the key is compiled
    /// first and, a constant string that is a canonical decimal, made
    /// the long php's `zend_handle_numeric_op` makes of it, then the
    /// value; the first element is `INIT_ARRAY` over the two into a
    /// temporary (`zend_emit_op_tmp`, so the temporary is numbered
    /// after the first element's own), the count of elements in
    /// `extended_value` above `ZEND_ARRAY_SIZE_SHIFT`, and every element
    /// after it `ADD_ARRAY_ELEMENT` over the two with the same temporary
    /// as its `result`. `ZEND_ARRAY_NOT_PACKED` is set on the
    /// `INIT_ARRAY` once a constant string key is seen, and
    /// `ZEND_ARRAY_ELEMENT_REF` never, since a `&` element is not read
    /// yet; neither is `...`, which would be `ADD_ARRAY_UNPACK`.
    fn array(&mut self, elements: &[Element]) -> Result<Where, CompileError> {
        if let Some(value) = self.constant_array(elements)? {
            return Ok(Where::Constant(value));
        }
        let count = u32::try_from(elements.len()).unwrap_or(u32::MAX);
        let mut init_at = 0usize;
        let mut out = 0u32;
        let mut packed = true;
        for (i, element) in elements.iter().enumerate() {
            let key = match &element.key {
                Some(key) => {
                    let key = self.expr(key)?;
                    Some(Self::numeric_op(key))
                }
                None => None,
            };
            let value = self.expr(&element.value)?;
            let (op1_type, op1) = self.operand(value);
            let (op2_type, op2) = match key {
                Some(key) => self.operand(key),
                None => (OperandKind::Unused, 0),
            };
            if i == 0 {
                out = self.temporary();
                init_at = self.oplines.len();
                self.push(Opline {
                    opcode: Opcode::InitArray,
                    op1_type,
                    op1,
                    op2_type,
                    op2,
                    out_type: OperandKind::Tmp,
                    out,
                    extended_value: count << ARRAY_SIZE_SHIFT,
                    ..Opline::default()
                });
            } else {
                self.push(Opline {
                    opcode: Opcode::AddArrayElement,
                    op1_type,
                    op1,
                    op2_type,
                    op2,
                    out_type: OperandKind::Tmp,
                    out,
                    ..Opline::default()
                });
            }
            if matches!(key, Some(Where::Constant(k)) if k.kind() == ZvalType::String) {
                packed = false;
            }
        }
        if !packed && let Some(init) = self.oplines.get_mut(init_at) {
            init.extended_value |= ARRAY_NOT_PACKED;
        }
        Ok(Where::Temporary(out))
    }

    /// `zend_handle_numeric_op` (`Z/Zend/zend_compile.c:2775`): a
    /// constant string key that `ZEND_HANDLE_NUMERIC` reads as an integer
    /// is that integer, so `['3' => 4]` writes at 3; every other operand
    /// stands.
    fn numeric_op(key: Where) -> Where {
        if let Where::Constant(value) = key
            && let Some(s) = value.as_str()
            && let Some(n) = numeric_key(s.bytes())
        {
            return Where::Constant(Zval::long(n));
        }
        key
    }

    /// `zend_compile_call`: `CG(function_table)` read first
    /// ([`Self::native`]), and a name the table has not compiled as
    /// `zend_compile_dynamic_call` compiles it, [`Self::dynamic_call`],
    /// for the engine to resolve as the program serves. A name the table
    /// has is what `zend_try_compile_special_func` makes of it where the
    /// name is in [`crate::special`]'s table, a constant or an opcode of
    /// its own, and `INIT_FCALL` over the lowered name otherwise. Every
    /// callable this engine has is a native, so the call op after an
    /// `INIT_FCALL` is `DO_ICALL`; `DO_UCALL` comes with user functions.
    ///
    /// Every call is given a `VAR` for what it returns (`zend_emit_op`
    /// with a `result`, `zend_make_var_result`), numbered whether or not
    /// anything reads it; [`Self::do_free`] marks it `UNUSED` when
    /// nothing does, which is what the engine's `DO_ICALL` reads as
    /// `RETURN_VALUE_USED`.
    fn call(&mut self, name: &[u8], arguments: &[Expr]) -> Result<Where, CompileError> {
        let (resolved, runtime) = self.function_name(name);
        if runtime {
            return self.ns_call(&resolved, arguments);
        }
        let lowered = resolved.to_ascii_lowercase();
        let count = u32::try_from(arguments.len()).unwrap_or(0);
        let (stack, call_op) = match self.native_index(&lowered) {
            Some(index) => {
                if let Some(served) = self.special(&lowered, arguments)? {
                    return Ok(served);
                }
                if let Some(served) = self.frameless_icall(index, arguments, None)? {
                    return Ok(served);
                }
                (used_stack(count, None), Opcode::DoIcall)
            }
            None => match self.declared_function(&lowered) {
                Some(body) => (used_stack(count, Some(body)), Opcode::DoUcall),
                None => return self.dynamic_call(&resolved, arguments),
            },
        };
        let lowered = self.string(&lowered)?;
        let lowered = self.constant(lowered);
        let slot = self.cache_slot();
        self.push(Opline {
            opcode: Opcode::InitFcall,
            op1: stack,
            op2_type: OperandKind::Const,
            op2: lowered,
            out: slot,
            extended_value: count,
            ..Opline::default()
        });
        self.args(arguments, true)?;
        Ok(self.call_op(call_op))
    }

    /// `zend_hash_find_ptr(CG(function_table), lcname)` over the
    /// functions this file has declared so far, with `fbc_is_finalized`
    /// folded in: a body that has compiled, whose `used_stack` a call
    /// site can write; none for a native, which [`Self::native_index`]
    /// answers, and none for the function whose own body is being
    /// compiled, which is in php's table without `ZEND_ACC_DONE_PASS_TWO`
    /// and so compiles a call to itself by name.
    fn declared_function(&self, lowered: &[u8]) -> Option<&OpArray> {
        self.functions_declared
            .iter()
            .find(|(decl, _)| decl.name.0.to_ascii_lowercase() == lowered)
            .map(|(_, body)| body)
    }

    /// `zend_compile_dynamic_call` for a name written as a constant
    /// string that `CG(function_table)` has not: `INIT_FCALL_BY_NAME`
    /// over the name's constant pair, the arguments sent for a callee
    /// nothing is known of, and `DO_FCALL_BY_NAME`
    /// (`zend_compile_call_common` with `fbc` null, whose `used_stack`
    /// is written for an `INIT_FCALL` alone, so `op1` stays unused).
    /// The engine resolves the pair's lowered name at the `INIT` and
    /// raises php's `Error` over the written one where nothing declares
    /// it.
    fn dynamic_call(&mut self, name: &[u8], arguments: &[Expr]) -> Result<Where, CompileError> {
        let pair = self.func_name_literal(name)?;
        let count = u32::try_from(arguments.len()).unwrap_or(0);
        let slot = self.cache_slot();
        self.push(Opline {
            opcode: Opcode::InitFcallByName,
            op2_type: OperandKind::Const,
            op2: pair,
            out: slot,
            extended_value: count,
            ..Opline::default()
        });
        self.args(arguments, false)?;
        Ok(self.call_op(Opcode::DoFcallByName))
    }

    /// `zend_compile_new` for a class named as written: `NEW` with the
    /// class's constant pair in `op1`, a cache slot in `op2` and the
    /// number of arguments in `extended_value`, written into the `VAR`
    /// the object lands in; then the arguments sent for a callee
    /// nothing is known of, since the constructor is looked up as the
    /// program serves (`zend_compile_call_common` with `fbc` null), and
    /// the `DO_FCALL` that serves it, whose own `VAR` `zend_do_free`
    /// marks unused. `self`, `parent` and `static` are class references
    /// of another kind (`zend_get_class_fetch_type`), not read yet.
    fn new_expression(&mut self, class: &[u8], arguments: &[Expr]) -> Result<Where, CompileError> {
        let class = self.class_name_literal(class)?;
        let out = self.temporary();
        let slot = self.cache_slot();
        let count = u32::try_from(arguments.len()).unwrap_or(0);
        self.push(Opline {
            opcode: Opcode::New,
            op1_type: OperandKind::Const,
            op1: class,
            op2: slot,
            out_type: OperandKind::Var,
            out,
            extended_value: count,
            ..Opline::default()
        });
        self.args(arguments, false)?;
        let served = self.call_op(Opcode::DoFcall);
        self.do_free(served);
        Ok(Where::Returned(out))
    }

    /// `zend_compile_static_call` for a class and a method both named as
    /// written: `INIT_STATIC_METHOD_CALL` with the class's constant pair
    /// in `op1`, the method's in `op2`, two cache slots in `result` and
    /// the number of arguments in `extended_value`; `__construct` is
    /// php's `CONSTRUCTOR`, `op2` unused and one cache slot. The callee
    /// is known where the linked table holds the class and a public
    /// method of the name (`zend_get_compatible_func_or_null`), which is
    /// what decides how the arguments are sent; the call op is
    /// `DO_FCALL` either way (`zend_get_call_op`).
    fn static_call(
        &mut self,
        class: &[u8],
        method: &[u8],
        arguments: &[Expr],
    ) -> Result<Where, CompileError> {
        let known = self.known_method(class, method);
        let call_op = match known {
            Some(Known::User) => Opcode::DoUcall,
            Some(Known::Native) | None => Opcode::DoFcall,
        };
        let known = known.is_some();
        let class_at = self.class_name_literal(class)?;
        let count = u32::try_from(arguments.len()).unwrap_or(0);
        let opline = if method.eq_ignore_ascii_case(b"__construct") {
            Opline {
                opcode: Opcode::InitStaticMethodCall,
                op1_type: OperandKind::Const,
                op1: class_at,
                out: self.cache_slot(),
                extended_value: count,
                ..Opline::default()
            }
        } else {
            let method_at = self.func_name_literal(method)?;
            Opline {
                opcode: Opcode::InitStaticMethodCall,
                op1_type: OperandKind::Const,
                op1: class_at,
                op2_type: OperandKind::Const,
                op2: method_at,
                out: self.cache_slots(2),
                extended_value: count,
                ..Opline::default()
            }
        };
        self.push(opline);
        self.args(arguments, known)?;
        Ok(self.call_op(call_op))
    }

    /// `zend_compile_method_call`: the receiver compiled,
    /// `INIT_METHOD_CALL` over it with the method's constant pair in
    /// `op2`, two cache slots in `result` and the number of arguments in
    /// `extended_value`, the arguments sent for a callee nothing is
    /// known of, and `DO_FCALL`.
    fn method_call(
        &mut self,
        object: &Expr,
        method: &[u8],
        arguments: &[Expr],
    ) -> Result<Where, CompileError> {
        let (op1_type, op1) = self.receiver(object)?;
        let method_at = self.func_name_literal(method)?;
        let count = u32::try_from(arguments.len()).unwrap_or(0);
        let slot = self.cache_slots(2);
        self.push(Opline {
            opcode: Opcode::InitMethodCall,
            op1_type,
            op1,
            op2_type: OperandKind::Const,
            op2: method_at,
            out: slot,
            extended_value: count,
            ..Opline::default()
        });
        self.args(arguments, false)?;
        Ok(self.call_op(Opcode::DoFcall))
    }

    /// The receiver of a method call or a property fetch: `$this` is
    /// `FETCH_THIS` into a `VAR` (`zend_delayed_compile_prop` and
    /// `zend_compile_method_call`, where `this_guaranteed_exists` is
    /// false outside a method's body, which is everywhere this compiler
    /// reads); anything else is the expression itself.
    fn receiver(&mut self, object: &Expr) -> Result<(OperandKind, u32), CompileError> {
        match object {
            Expr::This if self.this_in_scope => Ok((OperandKind::Unused, 0)),
            Expr::This => {
                let this = self.fetch_this(OperandKind::Var);
                Ok(self.operand(this))
            }
            other => {
                let value = self.expr(other)?;
                Ok(self.operand(value))
            }
        }
    }

    /// `zend_compile_class_const` under `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION`,
    /// which opcache sets, so nothing folds: `FETCH_CLASS_CONSTANT` with
    /// the class's constant pair in `op1` (`zend_set_class_name_op1`),
    /// the constant's name in `op2` and two cache slots in
    /// `extended_value`, into a temporary.
    fn class_const(&mut self, class: &[u8], name: &[u8]) -> Result<Where, CompileError> {
        let class_at = self.class_name_literal(class)?;
        let name = self.string(name)?;
        let name_at = self.literal(name);
        let out = self.temporary();
        let slots = self.cache_slots(2);
        self.push(Opline {
            opcode: Opcode::FetchClassConstant,
            op1_type: OperandKind::Const,
            op1: class_at,
            op2_type: OperandKind::Const,
            op2: name_at,
            out_type: OperandKind::Tmp,
            out,
            extended_value: slots,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_static_prop` under `BP_VAR_R`, which delays nothing
    /// this compiler reads: `FETCH_STATIC_PROP_R` with the property's
    /// name as one constant in `op1` and three cache slots in
    /// `extended_value` (`zend_alloc_cache_slots(3)`), then the class's
    /// constant pair in `op2` (`zend_add_class_name_literal`), which
    /// reserves no slot of its own where `op1` is a constant. The fetch
    /// is written into a temporary (`zend_adjust_for_fetch_type`). php
    /// reads the property's name before the class's, so the literals
    /// stand in that order; `self`, `parent` and `static` are refused
    /// as [`Self::class_name_literal`] refuses them.
    fn static_prop(&mut self, class: &[u8], name: &[u8]) -> Result<Where, CompileError> {
        let name = self.string(name)?;
        let name_at = self.literal(name);
        let out = self.temporary();
        let slots = self.cache_slots(3);
        let class_at = self.class_name_literal(class)?;
        self.push(Opline {
            opcode: Opcode::FetchStaticPropR,
            op1_type: OperandKind::Const,
            op1: name_at,
            op2_type: OperandKind::Const,
            op2: class_at,
            out_type: OperandKind::Tmp,
            out,
            extended_value: slots,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_emit_op(result, ZEND_FETCH_THIS, NULL, NULL)`: `$this` into
    /// a temporary, a `TMP` where it is read and a `VAR` where it is a
    /// receiver or is sent to a callee nothing is known of.
    fn fetch_this(&mut self, out_type: OperandKind) -> Where {
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::FetchThis,
            out_type,
            out,
            ..Opline::default()
        });
        match out_type {
            OperandKind::Var => Where::Returned(out),
            _ => Where::Temporary(out),
        }
    }

    /// `zend_delayed_compile_prop` under `BP_VAR_R`, which delays nothing
    /// this compiler reads: the receiver, then `FETCH_OBJ_R` over it with
    /// the property's name as one constant in `op2`, three cache slots in
    /// `extended_value` (`zend_alloc_cache_slots(3)`), written into a
    /// temporary (`zend_adjust_for_fetch_type`).
    fn prop(&mut self, object: &Expr, name: &[u8]) -> Result<Where, CompileError> {
        let (op1_type, op1) = self.receiver(object)?;
        let name = self.string(name)?;
        let name_at = self.literal(name);
        let out = self.temporary();
        let slots = self.cache_slots(3);
        self.push(Opline {
            opcode: Opcode::FetchObjR,
            op1_type,
            op1,
            op2_type: OperandKind::Const,
            op2: name_at,
            out_type: OperandKind::Tmp,
            out,
            extended_value: slots,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_instanceof`: a constant on the left is `false`
    /// where it stands, php's compile-time answer for what is no object;
    /// otherwise `INSTANCEOF` over the operand with the class's constant
    /// pair in `op2` and a cache slot in `extended_value`, into a
    /// temporary.
    fn instance_of(&mut self, expr: &Expr, class: &[u8]) -> Result<Where, CompileError> {
        let value = self.expr(expr)?;
        if let Where::Constant(_) = value {
            return Ok(Where::Constant(Zval::bool(false)));
        }
        let class_at = self.class_name_literal(class)?;
        let (op1_type, op1) = self.operand(value);
        let out = self.temporary();
        let slot = self.cache_slot();
        self.push(Opline {
            opcode: Opcode::Instanceof,
            op1_type,
            op1,
            op2_type: OperandKind::Const,
            op2: class_at,
            out_type: OperandKind::Tmp,
            out,
            extended_value: slot,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_add_class_name_literal` over `zend_resolve_class_name`: the
    /// name as written with its leading separator dropped, and its
    /// lowered form in the slot after it, which the engine looks the
    /// class up by (`Z_STR_P(RT_CONSTANT(opline, opline->op1) + 1)`).
    /// `self`, `parent` and `static` name no class of the table
    /// (`zend_get_class_fetch_type`) and are refused as not yet read.
    fn class_name_literal(&mut self, name: &[u8]) -> Result<u32, CompileError> {
        let written = self.class_name(name);
        if written.eq_ignore_ascii_case(b"self")
            || written.eq_ignore_ascii_case(b"parent")
            || written.eq_ignore_ascii_case(b"static")
        {
            return Err(self.fatal("a class reference of the scope this compiler cannot read yet"));
        }
        let lowered = written.to_ascii_lowercase();
        let written = self.string(&written)?;
        let lowered = self.string(&lowered)?;
        let at = self.literal(written);
        self.literal(lowered);
        Ok(at)
    }

    /// `zend_get_compatible_func_or_null` over `CG(class_table)`: whether
    /// the class named, one the engine declared ([`Options::classes`])
    /// or one this file has bound before the call, has a public method
    /// of the name; a private or protected one is compatible only from
    /// inside the class, which nothing here is. The name is resolved as
    /// the fetch resolves it ([`Self::class_name`]), which is the name a
    /// declaration of the file took under the same namespace.
    fn known_method(&self, class: &[u8], method: &[u8]) -> Option<Known> {
        let class = self.class_name(class).to_ascii_lowercase();
        let method = method.to_ascii_lowercase();
        let own = self
            .active_class
            .as_ref()
            .is_some_and(|active| active.to_ascii_lowercase() == class);
        if let Some(decl) = self
            .known_classes
            .iter()
            .find(|decl| decl.name.0.to_ascii_lowercase() == class)
        {
            return decl
                .methods
                .iter()
                .any(|m| {
                    m.function.name.0.to_ascii_lowercase() == method
                        && m.visibility == Visibility::Public
                })
                .then_some(Known::Native);
        }
        let decl = self
            .classes
            .iter()
            .find(|decl| decl.name.0.to_ascii_lowercase() == class)?;
        decl.methods
            .iter()
            .any(|m| {
                m.function.name.0.to_ascii_lowercase() == method
                    && (m.visibility == Visibility::Public || own)
            })
            .then_some(Known::User)
    }

    /// `zend_add_func_name_literal`: the name as written and its lowered
    /// form, two slots in a row whatever the table already holds, since
    /// the engine reads the second as the one after the first
    /// (`Z_STR_P(function_name+1)`).
    fn func_name_literal(&mut self, name: &[u8]) -> Result<u32, CompileError> {
        let written = name;
        let lowered = written.to_ascii_lowercase();
        let written = self.string(written)?;
        let lowered = self.string(&lowered)?;
        let at = self.literal(written);
        self.literal(lowered);
        Ok(at)
    }

    /// `zend_compile_function_name` over `zend_resolve_function_name`:
    /// the name a call is resolved to, and whether the resolution waits
    /// for the program to be served. A name written with a leading `\` is
    /// what follows it, fully qualified; `namespace\f` is the current
    /// namespace before `f`, fully qualified; a name with a `\` in it
    /// is the current namespace before it, fully qualified, since no
    /// `use` is read here; and an unqualified name is itself where no
    /// namespace stands, and the namespace before it where one does,
    /// resolved as the program serves, which is the `true`.
    fn function_name(&self, name: &[u8]) -> (Vec<u8>, bool) {
        if let Some(fully_qualified) = name.strip_prefix(b"\\") {
            return (fully_qualified.to_vec(), false);
        }
        let Some(namespace) = &self.namespace else {
            return (name.to_vec(), false);
        };
        let prefixed = |rest: &[u8]| {
            let mut full = namespace.clone();
            full.push(b'\\');
            full.extend_from_slice(rest);
            full
        };
        if name.len() > 10 && name[..10].eq_ignore_ascii_case(b"namespace\\") {
            return (prefixed(&name[10..]), false);
        }
        (prefixed(name), !name.contains(&b'\\'))
    }

    /// `zend_resolve_class_name` for a name no `use` is read for: a name
    /// written with a leading `\` is what follows it, and in a namespace
    /// `namespace\C` and any other name take the namespace before them,
    /// as [`Self::function_name`] resolves a function's, with nothing
    /// waiting for the program, since a class name resolves at compile
    /// time.
    fn class_name(&self, name: &[u8]) -> Vec<u8> {
        self.function_name(name).0
    }

    /// `zend_prefix_with_ns`: the name a declaration takes, the current
    /// namespace, a `\` and the name as written where a namespace
    /// stands, and the name as written where none does.
    fn prefix_with_ns(&self, name: &[u8]) -> Vec<u8> {
        let Some(namespace) = &self.namespace else {
            return name.to_vec();
        };
        let mut full = namespace.clone();
        full.push(b'\\');
        full.extend_from_slice(name);
        full
    }

    /// `zend_compile_const` over `zend_resolve_const_name` and
    /// `zend_try_ct_eval_const`: the value of the constant `name` names
    /// where php substitutes it, and nothing where php emits
    /// `FETCH_CONSTANT`. A leading `\` names the global constant; an
    /// unqualified name in a namespace names the namespace's own, which
    /// nothing declares, so php fetches it as the program serves and
    /// falls back to the global one then; outside a namespace it is
    /// the persistent table's.
    /// `zend_try_ct_eval_magic_const` (`Z/Zend/zend_compile.c:9830`),
    /// which `zend_compile_magic_const` sends every magic constant
    /// through and takes the `IS_CONST` operand it answers: `__LINE__`
    /// is the token's line, `__FILE__` `CG(compiled_filename)`,
    /// `__DIR__` its `zend_dirname`, which is the working directory
    /// where the name has no directory in it, `__NAMESPACE__`
    /// `FC(current_namespace)` and the empty string before one,
    /// `__CLASS__` and `__TRAIT__` the active class's name where one is
    /// and the empty string where none is, `__PROPERTY__` the property
    /// whose default is being compiled, `__FUNCTION__` and `__METHOD__`
    /// the active op array's name, which is the empty string in the main
    /// op array and directly inside a class body, the two this compiler
    /// reads. A function, a method, a closure and a trait are not read
    /// yet, so the arms `__CLASS__` in a trait takes, the one answer
    /// `zend_try_ct_eval_magic_const` refuses and `ZEND_FETCH_CLASS_NAME`
    /// serves as the program runs, and `Class::method` are not reached.
    ///
    /// `__FILE__` is the request's name and `__LINE__` the token's:
    ///
    /// ```php
    /// echo __FILE__, ' ', __LINE__;
    /// ```
    /// ```expect exit=0
    /// %s 2
    /// ```
    ///
    /// The line is the token's own and not the statement's, `ast->lineno`
    /// of the `ZEND_AST_MAGIC_CONST` node:
    ///
    /// ```php
    /// echo
    /// __LINE__, ' ', __line__,
    /// ' ', __LINE__;
    /// ```
    /// ```expect exit=0
    /// 3 3 4
    /// ```
    ///
    /// `__DIR__` under the recipe: `witness.php` has no directory in
    /// its name, so `zend_dirname` answers `.` and the constant is the
    /// working directory, which `%s` stands for since the oracle's is
    /// its scratch directory and the linked engine's the harness's:
    ///
    /// ```php
    /// echo __DIR__, "\n";
    /// var_dump(str_contains(__DIR__, '/'));
    /// ```
    /// ```expect exit=0
    /// %s
    /// bool(true)
    /// ```
    ///
    /// `__NAMESPACE__` is the namespace in force, and the empty string
    /// before any:
    ///
    /// ```php
    /// echo '[', __NAMESPACE__, ']';
    /// ```
    /// ```expect exit=0
    /// []
    /// ```
    ///
    /// ```php
    /// namespace A\B;
    /// echo __NAMESPACE__;
    /// ```
    /// ```expect exit=0
    /// A\B
    /// ```
    ///
    /// The scope's five are the empty string in the main op array, where
    /// there is no class and no function:
    ///
    /// ```php
    /// echo '[', __CLASS__, ']';
    /// ```
    /// ```expect exit=0
    /// []
    /// ```
    ///
    /// ```php
    /// echo '[', __FUNCTION__, ']';
    /// ```
    /// ```expect exit=0
    /// []
    /// ```
    ///
    /// ```php
    /// echo '[', __METHOD__, ']';
    /// ```
    /// ```expect exit=0
    /// []
    /// ```
    ///
    /// ```php
    /// echo '[', __TRAIT__, ']';
    /// ```
    /// ```expect exit=0
    /// []
    /// ```
    ///
    /// ```php
    /// echo '[', __PROPERTY__, ']';
    /// ```
    /// ```expect exit=0
    /// []
    /// ```
    ///
    /// Inside a class body, `__CLASS__` is the class's name, `__PROPERTY__`
    /// the property whose default is being compiled and `__METHOD__` the
    /// empty string, since php reads a member's constant expression as
    /// standing directly inside the class and not in a function:
    ///
    /// ```php
    /// class M { public $p = __CLASS__ . '::' . __PROPERTY__ . '|' . __METHOD__ . '|' . __TRAIT__; }
    /// $o = new M;
    /// echo $o->p;
    /// ```
    /// ```expect exit=0
    /// M::p||
    /// ```
    fn magic_const(&self, which: Magic, line: u32) -> Folded {
        let text = match which {
            Magic::Line => return Folded::Line(i64::from(line)),
            Magic::File => self.filename.0.to_vec(),
            Magic::Dir => {
                let dirname = dirname(&self.filename.0);
                if dirname == b"." {
                    std::env::current_dir()
                        .map(|cwd| cwd.to_string_lossy().into_owned().into_bytes())
                        .unwrap_or(dirname)
                } else {
                    dirname
                }
            }
            Magic::Namespace => self.namespace.clone().unwrap_or_default(),
            Magic::Class => self.active_class.clone().unwrap_or_default(),
            Magic::Property => self.active_property.clone().unwrap_or_default(),
            // `zend_compile_func_decl` names the op array; the main one
            // has no `function_name`, and a member's constant expression
            // stands directly inside the class.
            Magic::Function => self.active_function.clone().unwrap_or_default(),
            // `__METHOD__` is `Class::method` inside a method and the
            // function's name inside a function (`zend_compile_const`).
            Magic::Method => match (&self.active_class, &self.active_function) {
                (Some(class), Some(function)) => {
                    let mut text = class.clone();
                    text.extend_from_slice(b"::");
                    text.extend_from_slice(function);
                    text
                }
                (None, Some(function)) => function.clone(),
                (_, None) => Vec::new(),
            },
            // A trait is not read yet, so `__TRAIT__` names none.
            Magic::Trait => Vec::new(),
        };
        Folded::Text(text)
    }

    fn const_name(&mut self, name: &[u8]) -> Result<Option<Zval>, CompileError> {
        if self.namespace.is_some() && !name.starts_with(b"\\") {
            return Ok(None);
        }
        self.persistent(name)
    }

    /// `find_frameless_function_info` (`Z/Zend/zend_compile.c:4649`): the
    /// arity of the native's first frameless entry that serves a call
    /// of `count` arguments, none where none does. An entry serves the
    /// call when its arity is at least the count, the native's required
    /// count is at most the count, and a variadic native's entry is of
    /// the count exactly; a call of more than three arguments has none.
    fn frameless_arity(&self, index: usize, count: usize) -> Option<u32> {
        let decl = self.functions.get(index)?;
        if count > 3 {
            return None;
        }
        let required = decl
            .parameters
            .iter()
            .take_while(|p| p.default.is_none() && !p.variadic)
            .count();
        let variadic = decl.parameters.iter().any(|p| p.variadic);
        let count32 = u32::try_from(count).ok()?;
        decl.frameless
            .iter()
            .copied()
            .find(|&arity| arity >= count32 && required <= count && (!variadic || arity == count32))
    }

    /// `find_frameless_function_offset`: the row of the engine's
    /// frameless table for the native at `index` and `arity`, the
    /// `extended_value` the opline carries.
    fn frameless_offset(&self, index: usize, arity: u32) -> Option<u32> {
        let function = u32::try_from(index).ok()?;
        self.frameless
            .iter()
            .position(|entry| entry.function == function && entry.arity == arity)
            .and_then(|at| u32::try_from(at).ok())
    }

    /// `zend_compile_frameless_icall` and `zend_compile_frameless_icall_ex`
    /// (`Z/Zend/zend_compile.c:4690-4732`): the call of the native at
    /// `index` as `FRAMELESS_ICALL_<n>` where an entry of its serves the
    /// argument count, and nothing where none does, which is the
    /// ordinary call. The arguments are compiled in order and each
    /// parameter the call left out is filled from its declared default
    /// (`zend_get_default_from_internal_arg_info`), a default this
    /// compiler cannot read as a value leaving the call as it is; the
    /// first two are the opline's operands and a third is an `OP_DATA`
    /// after it. The value is a temporary of the opline's own
    /// (`zend_emit_op_tmp`), or `into` where `zend_compile_ns_call`
    /// hands over the `VAR` its call by name returns into, so both
    /// branches leave the value in one place.
    fn frameless_icall(
        &mut self,
        index: usize,
        arguments: &[Expr],
        into: Option<u32>,
    ) -> Result<Option<Where>, CompileError> {
        let Some(arity) = self.frameless_arity(index, arguments.len()) else {
            return Ok(None);
        };
        let Some(offset) = self.frameless_offset(index, arity) else {
            return Ok(None);
        };
        let mut values = Vec::new();
        for at in 0..arity as usize {
            if let Some(argument) = arguments.get(at) {
                values.push(self.expr(argument)?);
                continue;
            }
            let default = self
                .functions
                .get(index)
                .and_then(|decl| decl.parameters.get(at))
                .and_then(|parameter| parameter.default.as_ref())
                .and_then(|default| self.default_value(default).transpose())
                .transpose()?;
            let Some(default) = default else {
                return Ok(None);
            };
            values.push(Where::Constant(default));
        }
        let opcode = match arity {
            0 => Opcode::FramelessIcall0,
            1 => Opcode::FramelessIcall1,
            2 => Opcode::FramelessIcall2,
            _ => Opcode::FramelessIcall3,
        };
        let (out_type, out) = match into {
            Some(var) => (OperandKind::Var, var),
            None => (OperandKind::Tmp, self.temporary()),
        };
        let mut operands = values.iter().map(|value| self.operand(*value));
        let (op1_type, op1) = operands.next().unwrap_or((OperandKind::Unused, 0));
        let (op2_type, op2) = operands.next().unwrap_or((OperandKind::Unused, 0));
        let data = operands.next();
        self.push(Opline {
            opcode,
            op1_type,
            op1,
            op2_type,
            op2,
            out_type,
            out,
            extended_value: offset,
            ..Opline::default()
        });
        if let Some((op1_type, op1)) = data {
            // `zend_emit_op_data`
            self.push(Opline {
                opcode: Opcode::OpData,
                op1_type,
                op1,
                ..Opline::default()
            });
        }
        Ok(Some(match into {
            Some(var) => Where::Returned(var),
            None => Where::Temporary(out),
        }))
    }

    /// `zend_get_default_from_internal_arg_info` for a default written
    /// as a value: the constant, or nothing for one written as an
    /// expression, which no frameless entry of php's reaches.
    fn default_value(&mut self, default: &ConstExpr) -> Result<Option<Zval>, CompileError> {
        Ok(match default {
            ConstExpr::Null => Some(Zval::null()),
            ConstExpr::Bool(b) => Some(Zval::bool(*b)),
            ConstExpr::Long(n) => Some(Zval::long(*n)),
            ConstExpr::Double(d) => Some(Zval::double(*d)),
            ConstExpr::Str(bytes) => Some(self.string(bytes)?),
            _ => None,
        })
    }

    /// `zend_compile_ns_call` (`Z/Zend/zend_compile.c:4734`): the call of
    /// an unqualified name in a namespace, resolved as the program
    /// serves. `INIT_NS_FCALL_BY_NAME` over the name's three constants,
    /// the arguments sent for a callee nothing is known of, and
    /// `DO_FCALL_BY_NAME`. Where the global function of that name has a
    /// frameless entry for the count, and no such branch is being
    /// compiled already, the call by name is put in a branch:
    /// `JMP_FRAMELESS` over the lowered name before it, which the engine
    /// takes when the namespace declares no function of the name, a `JMP`
    /// past the frameless call after it, and the `FRAMELESS_ICALL_<n>`
    /// itself, returning into the same `VAR`, compiled with its arguments
    /// a second time. The `JMP_FRAMELESS` takes its cache slot after
    /// the `INIT_NS_FCALL_BY_NAME` took its own, which is the number
    /// the dump prints.
    fn ns_call(&mut self, name: &[u8], arguments: &[Expr]) -> Result<Where, CompileError> {
        let name_constants = self.ns_func_name_literal(name)?;
        let lowered = name.to_ascii_lowercase();
        let unqualified = lowered
            .rsplit(|b| *b == b'\\')
            .next()
            .unwrap_or_default()
            .to_vec();
        let mut branch = None;
        if !self.in_jmp_frameless_branch
            && let Some(index) = self.native_index(&unqualified)
            && self.frameless_arity(index, arguments.len()).is_some()
        {
            self.in_jmp_frameless_branch = true;
            let over = self.string(&lowered)?;
            let op1 = self.literal(over);
            let at = self.push(Opline {
                opcode: Opcode::JmpFrameless,
                op1_type: OperandKind::Const,
                op1,
                ..Opline::default()
            });
            branch = Some((at, index));
        }
        let count = u32::try_from(arguments.len()).unwrap_or(0);
        let slot = self.cache_slot();
        self.push(Opline {
            opcode: Opcode::InitNsFcallByName,
            op2_type: OperandKind::Const,
            op2: name_constants,
            out: slot,
            extended_value: count,
            ..Opline::default()
        });
        self.args(arguments, false)?;
        let returned = self.call_op(Opcode::DoFcallByName);
        if let Some((jmp_fl, index)) = branch
            && let Where::Returned(var) = returned
        {
            let jmp_end = self.push(Opline {
                opcode: Opcode::Jmp,
                ..Opline::default()
            });
            let target = self.next();
            self.frameless_icall(index, arguments, Some(var))?;
            let slot = self.cache_slot();
            if let Some(opline) = self.oplines.get_mut(jmp_fl as usize) {
                opline.op2 = target;
                opline.extended_value = slot;
            }
            let after = self.next();
            if let Some(opline) = self.oplines.get_mut(jmp_end as usize) {
                opline.op1 = after;
            }
            self.in_jmp_frameless_branch = false;
        }
        Ok(returned)
    }

    /// `zend_add_ns_func_name_literal`: the name with its namespace as
    /// written, its lowered form, and the lowered name alone, three
    /// slots in a row, which the engine reads as the two after the first
    /// (`Z_STR_P(function_name + 1)`, `+ 2`).
    fn ns_func_name_literal(&mut self, name: &[u8]) -> Result<u32, CompileError> {
        let lowered = name.to_ascii_lowercase();
        let unqualified = lowered
            .rsplit(|b| *b == b'\\')
            .next()
            .unwrap_or_default()
            .to_vec();
        let written = self.string(name)?;
        let lowered = self.string(&lowered)?;
        let unqualified = self.string(&unqualified)?;
        let at = self.literal(written);
        self.literal(lowered);
        self.literal(unqualified);
        Ok(at)
    }

    /// `zend_compile_args` for arguments none of which is by reference,
    /// unpacked or named: each compiled and sent at its number. A callee
    /// the table has (`fbc`) takes a VAR or a CV by `SEND_VAR` and
    /// anything else by `SEND_VAL`; one it has not takes a CV by
    /// `SEND_VAR_EX`, what a call returned by `SEND_VAR_NO_REF_EX`, and a
    /// constant or a temporary by `SEND_VAL_EX`, each of which reads the
    /// parameter's by-reference bit off the callee as the program serves.
    /// `$this` sent to a callee nothing is known of is `FETCH_THIS` into
    /// a `VAR` and `SEND_VAR_EX`; a property fetch sent to one is php's
    /// `CHECK_FUNC_ARG` and `FETCH_OBJ_FUNC_ARG`, not read yet.
    fn args(&mut self, arguments: &[Expr], known: bool) -> Result<(), CompileError> {
        for (at, argument) in arguments.iter().enumerate() {
            let number = u32::try_from(at).unwrap_or(0) + 1;
            let value = match argument {
                Expr::This if !known => {
                    let this = self.fetch_this(OperandKind::Var);
                    let (op1_type, op1) = self.operand(this);
                    self.push(Opline {
                        opcode: Opcode::SendVarEx,
                        op1_type,
                        op1,
                        op2: number,
                        out: number - 1,
                        ..Opline::default()
                    });
                    continue;
                }
                Expr::Prop { .. } if !known => {
                    return Err(self.fatal(
                        "a property fetch sent to a callee the table has not this compiler cannot read yet",
                    ));
                }
                other => self.expr(other)?,
            };
            let (op1_type, op1) = self.operand(value);
            let opcode = match (known, op1_type) {
                (true, OperandKind::Var | OperandKind::Cv) => Opcode::SendVar,
                (true, _) => Opcode::SendVal,
                (false, OperandKind::Cv) => Opcode::SendVarEx,
                (false, OperandKind::Var) => Opcode::SendVarNoRefEx,
                (false, _) => Opcode::SendValEx,
            };
            self.push(Opline {
                opcode,
                op1_type,
                op1,
                op2: number,
                out: number - 1,
                ..Opline::default()
            });
        }
        Ok(())
    }

    /// The call op `zend_get_call_op` picked, with the `VAR` for what
    /// the call returns.
    fn call_op(&mut self, opcode: Opcode) -> Where {
        let out = self.temporary();
        self.push(Opline {
            opcode,
            out_type: OperandKind::Var,
            out,
            ..Opline::default()
        });
        Where::Returned(out)
    }

    /// `zend_emit_final_return` (`Z/Zend/zend_compile.c:2688`): a
    /// return type of `never` is `VERIFY_NEVER_TYPE` and nothing after
    /// it; every other declared one is the implicit return type check
    /// ([`Self::return_type_check`] with no value). Then the main op
    /// array returns the constant `1` and a function `null`,
    /// `extended_value` at -1 on both, by reference where the function
    /// was written `function &f`, and the return carries its safepoint
    /// number.
    fn final_return(&mut self, return_one: bool) -> Result<(), CompileError> {
        if let Some(ty) = self.return_type.clone() {
            if ty.contains_code(NEVER_CODE) {
                self.push(Opline {
                    opcode: Opcode::VerifyNeverType,
                    ..Opline::default()
                });
                return Ok(());
            }
            self.return_type_check(Where::Constant(Zval::null()), false, true)?;
        }
        let value = self.constant(if return_one {
            Zval::long(1)
        } else {
            Zval::null()
        });
        let at = self.next();
        let number = self.safepoint(at);
        self.push(Opline {
            opcode: if self.returns_by_ref {
                Opcode::ReturnByRef
            } else {
                Opcode::Return
            },
            op1_type: OperandKind::Const,
            op1: value,
            out: number,
            extended_value: u32::MAX,
            ..Opline::default()
        });
        Ok(())
    }
}

/// Whether a constant expression names a constant, which
/// `zend_compile_params` leaves unsubstituted in a parameter's default:
/// a constant by name or a class constant, anywhere in the tree. `true`,
/// `false` and `null` are the reader's own values and name none.
fn names_a_constant(expr: &Expr) -> bool {
    match expr {
        Expr::Constant(_) | Expr::ClassConst { .. } => true,
        Expr::Unary { operand, .. } => names_a_constant(operand),
        Expr::Binary { left, right, .. } => names_a_constant(left) || names_a_constant(right),
        Expr::Array(elements) => elements.iter().any(|element| {
            element.key.as_ref().is_some_and(names_a_constant) || names_a_constant(&element.value)
        }),
        _ => false,
    }
}

/// What a call site knows of its callee once the table has it
/// (`zend_get_call_op`): a native, served by `DO_ICALL` or `DO_FCALL`,
/// or a user function of this file, served by `DO_UCALL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Known {
    Native,
    User,
}

/// `zend_is_call`: a call by name, a call on a class or a method call,
/// whose `VAR` is separated where it is written to.
fn is_call(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Call { .. } | Expr::StaticCall { .. } | Expr::MethodCall { .. }
    )
}

/// `ZEND_HANDLE_NUMERIC_STR` (`Z/Zend/zend_hash.h:402`,
/// `_zend_handle_numeric_str_ex` at `zend_hash.c:3300`): the integer a
/// string key stands for, a decimal with no leading zero, no sign but
/// `-`, not `-0`, within `zend_long`; nothing for any other string.
fn numeric_key(key: &[u8]) -> Option<i64> {
    let (negative, digits) = match key {
        [b'-', rest @ ..] => (true, rest),
        _ => (false, key),
    };
    let &first = digits.first()?;
    if !first.is_ascii_digit() || (first == b'0' && key.len() > 1) || digits.len() > 19 {
        return None;
    }
    let mut n: i64 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        let digit = i64::from(b - b'0');
        n = n.checked_mul(10)?;
        n = if negative {
            n.checked_sub(digit)?
        } else {
            n.checked_add(digit)?
        };
    }
    Some(n)
}

/// `zend_vm_calc_used_stack` (`Z/Zend/zend_execute.h:362`), the number
/// `zend_compile_call_common` writes into an `INIT_FCALL`'s `op1`
/// (`Z/Zend/zend_compile.c:3983`): the frame header's slots
/// (`ZEND_CALL_FRAME_SLOT`, five zvals on a 64-bit build), one slot per
/// argument, and the callee's temporaries, of which a native has none
/// while nothing observes it (`Z/Zend/zend_API.c:2988-2995`); for a
/// user callee, its CVs too, less the parameters the arguments fill, in
/// bytes. php's own VM sizes its stack by this number; here it is what
/// the dump prints, `INIT_FCALL 1 96 string("set_time_limit")` and
/// `INIT_FCALL 1 128 string("f")`, and the engine's stack is sized by
/// the engine.
fn used_stack(arguments: u32, callee: Option<&OpArray>) -> u32 {
    const CALL_FRAME_SLOT: u32 = 5;
    let zval = u32::try_from(std::mem::size_of::<Zval>()).unwrap_or(16);
    let mut slots = CALL_FRAME_SLOT.saturating_add(arguments);
    if let Some(body) = callee {
        // `func->common.T`, then the callee's CVs less the parameters the
        // arguments already fill (`op_array.last_var - MIN(num_args, …)`).
        let parameters = body
            .oplines
            .iter()
            .take_while(|op| matches!(op.opcode, Opcode::Recv | Opcode::RecvInit))
            .count();
        let parameters = u32::try_from(parameters).unwrap_or(u32::MAX);
        slots = slots
            .saturating_add(body.num_tmps)
            .saturating_add(body.num_cvs)
            .saturating_sub(parameters.min(arguments));
    }
    slots.saturating_mul(zval)
}

impl Emit<'_> {
    /// `zend_hash_find_ptr(CG(function_table), lcname)` with php's
    /// `fbc->type == ZEND_INTERNAL_FUNCTION` test folded in: whether the
    /// lowered name is one of the natives the engine declared
    /// ([`Options::functions`]), which is what
    /// `zend_try_compile_special_func` requires before it serves a call
    /// itself, so a name the bundle has not is the call.
    pub(crate) fn native(&self, lowered: &[u8]) -> bool {
        self.native_index(lowered).is_some()
    }

    /// [`Self::native`] with the native's place in the table, php's
    /// `fbc`: what the frameless table is indexed by.
    fn native_index(&self, lowered: &[u8]) -> Option<usize> {
        self.functions
            .iter()
            .position(|decl| decl.name.0.eq_ignore_ascii_case(lowered))
    }

    /// `zend_try_ct_eval_const`: php substitutes a persistent constant's
    /// value where it stands, so a `T_*` of the tokenizer reaches no
    /// opline. The table is the engine's, what its extensions declared
    /// ([`Options::persistent`]); `INF` and `NAN`, which php reads from
    /// the table `ext/standard` registers
    /// (`REGISTER_DOUBLE_CONSTANT("INF", ZEND_INFINITY, CONST_PERSISTENT)`,
    /// `Z/ext/standard/basic_functions_arginfo.h:3580`), stand here until
    /// that extension declares them. A constant's name is case-sensitive,
    /// and a leading `\` names the same global constant as none. A
    /// declared value that is no scalar is not folded, as php folds none
    /// whose value is not yet evaluated.
    pub(crate) fn persistent(&mut self, name: &[u8]) -> Result<Option<Zval>, CompileError> {
        let mut at = 0;
        while name.get(at) == Some(&b'\\') {
            at += 1;
        }
        let name = name.get(at..).unwrap_or(&[]);
        match name {
            b"INF" => return Ok(Some(Zval::double(f64::INFINITY))),
            b"NAN" => return Ok(Some(Zval::double(f64::NAN))),
            _ => {}
        }
        let Some(declared) = self.persistent.iter().find(|c| c.name.0.as_ref() == name) else {
            return Ok(None);
        };
        Ok(match &declared.value {
            ConstExpr::Null => Some(Zval::null()),
            ConstExpr::Bool(b) => Some(Zval::bool(*b)),
            ConstExpr::Long(n) => Some(Zval::long(*n)),
            ConstExpr::Double(d) => Some(Zval::double(*d)),
            ConstExpr::Str(bytes) => Some(self.string(bytes)?),
            _ => None,
        })
    }
}

/// `zendi_try_convert_scalar_to_number` for a constant, under the part
/// of `zend_binary_op_produces_error` that reads the operand alone: the
/// number a long, a double, null and a bool are worth, and the number a
/// string opens with when nothing follows it. A string that opens with
/// no number, or carries anything past the one it opens with, is what
/// php's `is_numeric_string` without `allow_errors` refuses, and the
/// opcode is left standing to throw or warn where it stands.
fn number(value: Zval) -> Option<Number> {
    if let Some(n) = value.as_long() {
        return Some(Number::Long(n));
    }
    if let Some(d) = value.as_double() {
        return Some(Number::Double(d));
    }
    if let Some(s) = value.as_str() {
        return match numeric_prefix(s.bytes()) {
            Some((number, false)) => Some(number),
            _ => None,
        };
    }
    match value.kind() {
        ZvalType::Null | ZvalType::False => Some(Number::Long(0)),
        ZvalType::True => Some(Number::Long(1)),
        _ => None,
    }
}

/// `zend_is_op_long_compatible` over one converted operand: the long a
/// double is worth when `zend_is_long_compatible` says the conversion
/// loses nothing, which is what a modulo casts through; nothing for a
/// double that is not whole, not finite or past the long's range.
#[expect(
    clippy::float_cmp,
    reason = "php's `(double) l == d`, the exact equality being copied"
)]
fn long_compatible(number: Number) -> Option<i64> {
    match number {
        Number::Long(n) => Some(n),
        Number::Double(d) => {
            // `ZEND_DOUBLE_FITS_LONG`, which NAN and the infinities fail
            // as they fail php's `zend_finite`, then php's `(zend_long) d`
            // read back as a double.
            let fits = (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&d);
            #[expect(
                clippy::cast_possible_truncation,
                reason = "php's `(zend_long) d` over a double that fits, whose truncation the equality below holds to nothing"
            )]
            let l = d as i64;
            (fits && Number::Long(l).double() == d).then_some(l)
        }
    }
}

/// `zend_binary_op_produces_error` past the operands' own reading, over
/// the numbers they are worth: a division by zero, which `zval_get_double`
/// reads `"0"`, `false` and `null` as too; a modulo over an operand that
/// is not long-compatible or by zero; `0 ** (<0)`; and a shift over an
/// operand that is not long-compatible or by a negative number, each of
/// which throws where the opcode stands.
fn produces_error(op: BinaryOp, a: Number, b: Number) -> bool {
    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => false,
        // php's guard is `zval_get_double(op2) == 0.0`, which -0.0
        // meets as well.
        BinaryOp::Div => b.double() == 0.0,
        BinaryOp::Mod => !matches!(
            (long_compatible(a), long_compatible(b)),
            (Some(_), Some(divisor)) if divisor != 0
        ),
        BinaryOp::Pow => a.double() == 0.0 && b.double() < 0.0,
        BinaryOp::Sl | BinaryOp::Sr => !matches!(
            (long_compatible(a), long_compatible(b)),
            (Some(_), Some(by)) if by >= 0
        ),
        // A `.` is [`Emit::concat_fold`]'s and a comparison
        // [`Emit::compare_fold`]'s; neither reaches here.
        BinaryOp::Concat => true,
        _ => compares(op),
    }
}

/// `zend_try_ct_eval_binary_op`: the value php folds an arithmetic
/// operator over two constants to, or `None` where php leaves it to the
/// opcode. php converts each operand as the opcode's kernel converts it,
/// through the same `add_function` and its kin, and folds unless
/// `zend_binary_op_produces_error` says the opcode would throw or warn.
fn fold(op: BinaryOp, left: Zval, right: Zval) -> Option<Zval> {
    let (a, b) = (number(left)?, number(right)?);
    if produces_error(op, a, b) {
        return None;
    }
    match (op, a, b) {
        (_, Number::Long(a), Number::Long(b)) => Some(longs(op, a, b)),
        // `mod_function` and the two shifts read both sides as the longs
        // `zval_get_long` makes of them, which the guard held to be exact.
        (BinaryOp::Mod | BinaryOp::Sl | BinaryOp::Sr, a, b) => {
            Some(longs(op, long_compatible(a)?, long_compatible(b)?))
        }
        (_, a, b) => doubles(op, a.double(), b.double()),
    }
}

/// The long arms of `add_function`, `sub_function`, `mul_function`,
/// `div_function`, `mod_function`, `pow_function`, `shift_left_function`
/// and `shift_right_function`: the value php's `fast_long_*` macros
/// leave, a double where the operation overflowed and, for a division,
/// where it did not come out whole. The divisor is not zero and no shift
/// is by a negative number; [`produces_error`] held both.
#[expect(
    clippy::cast_precision_loss,
    reason = "php's `(double) lval`, whose rounding is the behaviour being copied"
)]
fn longs(op: BinaryOp, a: i64, b: i64) -> Zval {
    let overflowed = |n: Option<i64>, d: f64| n.map_or_else(|| Zval::double(d), Zval::long);
    match op {
        BinaryOp::Add => overflowed(a.checked_add(b), a as f64 + b as f64),
        BinaryOp::Sub => overflowed(a.checked_sub(b), a as f64 - b as f64),
        BinaryOp::Mul => overflowed(a.checked_mul(b), a as f64 * b as f64),
        // php's own case, "prevent overflow error/crash".
        BinaryOp::Div if a == i64::MIN && b == -1 => Zval::double(a as f64 / -1.0),
        BinaryOp::Div if a % b == 0 => Zval::long(a / b),
        BinaryOp::Div => Zval::double(a as f64 / b as f64),
        // php's own case, `% -1` being zero where the division overflows.
        BinaryOp::Mod if b == -1 => Zval::long(0),
        BinaryOp::Mod => Zval::long(a % b),
        BinaryOp::Pow => power(a, b),
        BinaryOp::Sl => Zval::long(php_heap::shift_left(a, b.unsigned_abs())),
        BinaryOp::Sr => Zval::long(php_heap::shift_right(a, b.unsigned_abs())),
        // A `.` is [`Emit::concat_fold`]'s and a comparison
        // [`Emit::compare_fold`]'s; [`produces_error`] held both.
        _ => Zval::null(),
    }
}

/// `pow_function_base`'s long arm: [`php_heap::power`] for an exponent
/// that is not negative, and php's `pow()` over two doubles for one that
/// is. No zero base meets a negative exponent here; [`produces_error`]
/// held that.
#[expect(
    clippy::cast_precision_loss,
    reason = "php's `(double) lval`, whose rounding is the behaviour being copied"
)]
fn power(a: i64, b: i64) -> Zval {
    match u64::try_from(b) {
        Ok(exponent) => match php_heap::power(a, exponent) {
            Number::Long(n) => Zval::long(n),
            Number::Double(d) => Zval::double(d),
        },
        Err(_) => Zval::double((a as f64).powf(b as f64)),
    }
}

/// The double arms of the same operators, where php converts both
/// operands and works in doubles. The divisor is not zero and no zero is
/// raised to a negative power; [`produces_error`] held both.
fn doubles(op: BinaryOp, a: f64, b: f64) -> Option<Zval> {
    match op {
        BinaryOp::Add => Some(Zval::double(a + b)),
        BinaryOp::Sub => Some(Zval::double(a - b)),
        BinaryOp::Mul => Some(Zval::double(a * b)),
        BinaryOp::Div => Some(Zval::double(a / b)),
        BinaryOp::Pow => Some(Zval::double(a.powf(b))),
        // A modulo and a shift are [`longs`]'s: two long-compatible
        // doubles go there, and the guard refused every other pair; a
        // `.` and a comparison never reach [`fold`].
        _ => None,
    }
}

/// Whether the operator is one of the comparison and equality levels,
/// which `zend_compile_binary_op` folds through `zend_compare` or
/// `zend_is_identical` and never refuses, and `zend_compile_greater`
/// folds the same way with the sides swapped.
fn compares(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::IsSmaller
            | BinaryOp::IsSmallerOrEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterOrEqual
            | BinaryOp::IsEqual
            | BinaryOp::IsNotEqual
            | BinaryOp::IsIdentical
            | BinaryOp::IsNotIdentical
            | BinaryOp::Spaceship
    )
}

/// `zend_compile_binary_op`'s `ZEND_IS_IDENTICAL` and
/// `ZEND_IS_NOT_IDENTICAL` arm: a `===` or a `!==` whose one constant
/// side is null, false or true is `TYPE_CHECK` over the other side, with
/// `1 << IS_*` of the constant in `extended_value` for `===` and
/// `MAY_BE_ANY` less that bit for `!==`. The operand to check and the
/// mask, or nothing where the shape is not this one.
fn type_check(op: BinaryOp, left: Where, right: Where) -> Option<(Where, u32)> {
    /// `MAY_BE_ANY`: the nine bits `IS_NULL` through `IS_RESOURCE`.
    const MAY_BE_ANY: u32 = 0x3FE;
    if !matches!(op, BinaryOp::IsIdentical | BinaryOp::IsNotIdentical) {
        return None;
    }
    let ((Where::Constant(constant), other) | (other, Where::Constant(constant))) = (left, right)
    else {
        return None;
    };
    let bit = match constant.kind() {
        kind @ (ZvalType::Null | ZvalType::False | ZvalType::True) => 1 << (kind as u32),
        _ => return None,
    };
    Some((
        other,
        if op == BinaryOp::IsIdentical {
            bit
        } else {
            MAY_BE_ANY - bit
        },
    ))
}

/// `pass_two`'s one change to an operand order: `zend_vm_set_opcode_handler`
/// swaps the operands of an opcode whose handler is specialised
/// `COMMUTATIVE` when `op1_type < op2_type`, so a constant is never the
/// first operand of a `MUL` or an `IS_EQUAL` and `2 * $a` is php's
/// `MUL CV0($a) int(2)`. The order is observable: the `TypeError` of
/// `2 * []` names `array * int`.
/// `should_use_jumptable` (`Z/Zend/zend_compile.c:6279`): five cases,
/// the default among them, for a `long` table and two for a `string`
/// one.
fn should_use_jumptable(cases: &[Case], kind: ZvalType) -> bool {
    if kind == ZvalType::Long {
        cases.len() >= 5
    } else {
        cases.len() >= 2
    }
}

fn swap_commutative(oplines: &mut [Opline]) {
    for op in oplines {
        if op.opcode.is_commutative() && (op.op1_type as u8) < (op.op2_type as u8) {
            std::mem::swap(&mut op.op1_type, &mut op.op2_type);
            std::mem::swap(&mut op.op1, &mut op.op2);
        }
    }
}

/// A constant as [`php_heap::compare`] switches on it: the five kinds a
/// constant table holds, the string's bytes borrowed from the [`php_heap::Str`]
/// the caller took.
fn scalar(value: Zval, string: Option<&php_heap::Str>) -> Scalar<'_> {
    if let Some(n) = value.as_long() {
        return Scalar::Long(n);
    }
    if let Some(d) = value.as_double() {
        return Scalar::Double(d);
    }
    if let Some(s) = string {
        return Scalar::Str(s.bytes());
    }
    match value.kind() {
        ZvalType::True => Scalar::Bool(true),
        ZvalType::False => Scalar::Bool(false),
        _ => Scalar::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_unit::decl::MethodDecl;

    /// `zend_get_compatible_func_or_null` over a class this file bound
    /// under `namespace A;` (#322): the class stands in the table as
    /// `A\M`, and `M::f` written in the same namespace resolves to that
    /// name, so its public `f` is a known callee, where the same fetch
    /// written outside the namespace or with a leading `\` names the
    /// global `M` the table has not. The methods a file declares are
    /// #309's to read, so the declaration here carries one directly.
    #[test]
    fn a_known_method_is_found_on_a_class_the_file_declared_in_a_namespace() {
        let mut strings = php_heap::Strings::new();
        let options = Options::default();
        let mut emit = Emit {
            strings: &mut strings,
            precision: options.precision,
            persistent: &options.persistent,
            functions: &options.functions,
            known_classes: &options.classes,
            frameless: Vec::new(),
            namespace: Some(b"A".to_vec()),
            active_class: None,
            active_function: None,
            this_in_scope: false,
            return_type: None,
            returns_by_ref: false,
            // `zend_compile_top_stmt`'s `toplevel`, which is where a
            // class binds at compile time.
            depth: 1,
            active_property: None,
            in_jmp_frameless_branch: false,
            statements_seen: 0,
            stack: options.stack,
            oplines: Vec::new(),
            constants: Vec::new(),
            vars: Vec::new(),
            safepoints: Vec::new(),
            temporaries: 0,
            cache_slots: 0,
            line: 1,
            diagnostics: Vec::new(),
            classes: Vec::new(),
            functions_declared: Vec::new(),
            dynamic: Vec::new(),
            dynamic_func_defs: Vec::new(),
            methods: Vec::new(),
            filename: options.filename.clone(),
            loops: Vec::new(),
            current_loop: None,
            breaks: Vec::new(),
            try_catch: Vec::new(),
            unwind: Vec::new(),
            fast_call_var: None,
            try_catch_offset: None,
        };
        let read = Class {
            name: b"M".to_vec(),
            kind: ClassKind::Class,
            parent: None,
            interfaces: Vec::new(),
            doc_comment: None,
            properties: Vec::new(),
            constants: Vec::new(),
            methods: Vec::new(),
            end_line: 1,
        };
        assert!(emit.class(&read).is_ok());
        assert_eq!(emit.classes[0].name.0.as_ref(), b"A\\M".as_slice());
        emit.classes[0].methods.push(MethodDecl {
            function: FunctionDecl {
                name: Name(b"f".as_slice().into()),
                parameters: Vec::new(),
                return_type: None,
                returns_by_ref: false,
                attributes: Vec::new(),
                doc_comment: None,
                file: Name(Box::new([])),
                line: 0,
                end_line: 0,
                frameless: Vec::new(),
            },
            visibility: Visibility::Public,
            is_instance: false,
            is_abstract: false,
            is_final: false,
            tentative_return: false,
        });
        let user = Some(Known::User);
        assert_eq!(
            emit.known_method(b"M", b"f"),
            user,
            "M under namespace A is A\\M"
        );
        assert_eq!(
            emit.known_method(b"m", b"F"),
            user,
            "matched whatever the case"
        );
        assert_eq!(emit.known_method(b"namespace\\M", b"f"), user);
        assert_eq!(emit.known_method(b"\\A\\M", b"f"), user);
        assert_eq!(
            emit.known_method(b"\\M", b"f"),
            None,
            "the global M is not declared"
        );
        emit.namespace = None;
        assert_eq!(
            emit.known_method(b"M", b"f"),
            None,
            "outside the namespace M is the global one"
        );
        assert_eq!(emit.known_method(b"A\\M", b"f"), user);
    }

    /// Design §5.7's assertion, on an op array laid down by hand (#353):
    /// a backward `JMP` whose `out` names no entry of `safepoints` is
    /// refused, as is one whose entry is another opline; a `FAST_RET`
    /// to an earlier index and a forward jump are not read; a backward
    /// jump whose entry is its target passes.
    #[test]
    fn a_backward_jump_whose_number_names_no_header_is_refused() {
        let jump = |opcode: Opcode, op1: u32, out: u32| Opline {
            opcode,
            op1,
            out,
            ..Opline::default()
        };
        let entry = [OplineIdx(0), OplineIdx(1)];
        let refused = |oplines: &[Opline]| {
            headers_numbered(oplines, &entry)
                .err()
                .map(|refused| (refused.level, refused.message))
        };
        assert_eq!(
            refused(&[jump(Opcode::Nop, 0, 0), jump(Opcode::Jmp, 0, 2)]),
            Some((
                Level::Error,
                "InternalError: JMP at opline 1 jumps back to 0000 carrying safepoint 2, which is not that header"
                    .to_string()
            )),
            "a JMP back to 0000 numbered 2, past the table"
        );
        assert_eq!(
            refused(&[jump(Opcode::Nop, 0, 0), jump(Opcode::Jmp, 0, 1)]),
            Some((
                Level::Error,
                "InternalError: JMP at opline 1 jumps back to 0000 carrying safepoint 1, which is not that header"
                    .to_string()
            )),
            "a JMP back to 0000 numbered 1, whose entry is 0001"
        );
        assert_eq!(
            refused(&[jump(Opcode::Nop, 0, 0), jump(Opcode::Jmp, 0, 0)]),
            None,
            "a JMP back to 0000 numbered 0, whose entry is 0000"
        );
        assert_eq!(
            refused(&[jump(Opcode::Jmp, 1, 0), jump(Opcode::FastRet, 0, 0)]),
            None,
            "a forward JMP and a FAST_RET are no back-edges"
        );
        assert_eq!(
            refused(&[jump(Opcode::Nop, 0, 0), jump(Opcode::JmpNull, 0, 0)]),
            Some((
                Level::Error,
                "InternalError: JMP_NULL at opline 1 jumps back to 0000 with no safepoint column for its number"
                    .to_string()
            )),
            "JMP_NULL has no safepoint column"
        );
    }
}
