//! Emission, after `zend_compile.c` (design §6.1): what the statements
//! read from a source file become as oplines, with php's operand kinds,
//! php's constant table and php's safepoint numbering (design §5.7).
//!
//! One op array is built at a time. Constants are added as
//! `zend_add_literal` adds them, each in a slot of its own;
//! CVs are numbered as `lookup_cv` numbers them, one per name in the
//! order the names are met; temporaries are numbered as
//! `get_temporary_variable` numbers them, and a temporary's operand is
//! folded to its slot on the frame, its number past the last CV, as the
//! op array is finished (`live::fold_slots`).
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

use std::borrow::Cow;
use std::cell::Cell;
use std::cmp::Ordering;
use std::num::NonZeroI64;
use std::rc::Rc;

use php_syntax::tree::{self, Block, Node, NodeId, Stmt, StmtList, TokenId};
use php_syntax::{Arena, ArenaVec, Holder, Parsed, SyntaxKind, TokenValue};

use php_heap::{
    Arr, Exponent, Fatal, Intern, Key, Made, Number, Scalar, Sizing, Str, Zval, ZvalType,
    constant_truth, double_to_string, numeric_prefix,
};
use php_link::Indexed;
use php_unit::decl::{
    AliasModifier, Attribute, AttributeArgument, ClassConstDecl, ClassDecl, ClassFlags, ClassKind,
    ConstDecl, ConstExpr, ConstantName, DynamicProperties, EnumBacking, FunctionDecl, HookKind,
    MethodDecl, Name, NameKind, Parameter, ParameterDefault, PropertyDecl, TraitAdaptation,
    TraitMethodRef, Type, UnitValue, Visibility,
};
use php_unit::{
    AutoGlobal, BoundVar, ConstIdx, Declares, Diagnostic, FramelessEntry, Grown, HookBody, Level,
    MethodBody, Nested, OpArray, Opcode, OperandKind, Opline, OplineIdx, Safepoints, TopLevel,
    TryCatch, Unit,
};

use php_syntax::magic_const_name;

use crate::{CompileError, MAXPATHLEN, Options, Refused, SourceKind, StackLimit, class_sizing};
use args::{Args, Callee, Plan};
use attributes::InternalAttribute;
use by_name::Symbols;
use eval::Unfolded;
use imports::{Imports, Namespacing, Symbol};
use member::{Member, MethodName, PropName};
use target::Target;
use walk::{Class, Closure, Function, Method};

mod args;
mod assert;
mod attributes;
mod by_name;
mod chain;
mod eval;
mod export;
mod extended;
mod hooks;
mod imports;
mod list;
mod magic;
mod member;
mod memo;
mod pipe;
mod target;
mod types;
pub use self::types::is_reserved_class_name;
mod user_call;
pub(crate) mod walk;

/// One arm of an `if` as the tree holds it, php's `ZEND_AST_IF_ELEM`:
/// the condition, none for the `else` arm, which is the last, as
/// `zend_compile_if` asserts; the body as one statement or, in the
/// alternative syntax, the list after the colon; and the arm's own
/// line, `elem_ast->lineno`, which `zend_ast_create` takes from the
/// arm's first child, so for an arm with a condition it is where the
/// condition stands and for the `else` arm where its statements start.
/// `zend_compile_if` sets `CG(zend_lineno)` to it before an arm past
/// the first compiles its condition and before the `JMP` that leaves an
/// arm another follows.
#[derive(Clone, Copy)]
struct IfElem {
    cond: Option<tree::Expr>,
    body: Option<Stmt>,
    stmt_list: Option<NodeId<StmtList>>,
    line: u32,
}

/// One piece of an interpolated string as the emitter reads it, php's
/// `encaps_var` or `T_ENCAPSED_AND_WHITESPACE`: what to compile, the
/// line php's node for it carries, and how it was written, php's `attr`
/// on the `encaps_var`, which `zend_compile_encaps_list` reads for its
/// two deprecations.
#[derive(Clone, Copy)]
struct Piece<'a> {
    fetch: Fetched<'a>,
    line: u32,
    written: Written,
}

/// What one piece of an interpolated string stands for: the text
/// written between the variables, a variable, the one `[…]`, `->p` or
/// `?->p` the in-string grammar takes after it (the `bool` is the `?`),
/// an expression under `{…}`, or
/// the variable an
/// expression under `${…}` names.
#[derive(Clone, Copy)]
enum Fetched<'a> {
    Text(&'a [u8]),
    Var(&'a [u8]),
    Dim(&'a [u8], Offset<'a>),
    Prop(&'a [u8], &'a [u8], bool),
    Node(tree::Expr),
    VarVar(tree::Expr),
}

/// The `[…]` after a variable inside a string, php's
/// `encaps_var_offset`: the value a bare word or a number stands for,
/// or the variable that names it.
#[derive(Clone, Copy)]
enum Offset<'a> {
    Value(Zval),
    Var(&'a [u8]),
}

/// What stands after a `::`: a class constant, a class's own property
/// or the class's name, php's `ZEND_AST_CLASS_CONST`,
/// `ZEND_AST_STATIC_PROP` and `ZEND_AST_CLASS_NAME`, which the grammar
/// tells apart by the `$` and the keyword.
enum Scoped<'a> {
    Constant {
        class: ClassSpec,
        name: ScopedName<'a>,
    },
    Property {
        class: ClassSpec,
        name: ScopedName<'a>,
    },
    /// `C::class`, php's `ZEND_AST_CLASS_NAME`.
    ClassName { class: ClassSpec },
}

/// The operands of an opline over a class's property, as
/// [`Emit::static_operands`] lays them down: the name in `op1`, the
/// class in `op2`, and the cache slots `extended_value` carries.
struct StaticOperands {
    name: (OperandKind, u32),
    class: (OperandKind, u32),
    slots: u32,
}

/// A member's name after `::` as the tree holds it: written out, the
/// identifier of `C::X` or the `$p` of `C::$p` without its `$`, or the
/// expression whose value names it, `C::{expr}` (8.3), `A::$$n` and
/// `A::${expr}`, which `zend_compile_class_const` and
/// `zend_compile_static_prop` compile with `zend_compile_expr` like any
/// other operand. One `$` of `A::$$n` is the property's own, so the
/// name is the value of `$n`, php's `ZEND_AST_VAR` under
/// `ZEND_AST_STATIC_PROP`.
#[derive(Clone, Copy)]
enum ScopedName<'a> {
    Written(&'a [u8]),
    Computed(tree::Expr),
}

/// One element of an array expression as `zend_compile_array` reads
/// it, decoded once from the tree: php's `ZEND_AST_ARRAY_ELEM`, whose
/// `attr` is the by-reference bit, and `ZEND_AST_UNPACK`, which has no
/// key.
#[derive(Clone, Copy)]
enum Element {
    /// `value` or `key => value`.
    Value {
        key: Option<tree::Expr>,
        value: tree::Expr,
    },
    /// `&value` or `key => &value`, the variable fetched for writing
    /// and bound (`ZEND_ARRAY_ELEMENT_REF`).
    Ref {
        key: Option<tree::Expr>,
        value: tree::Expr,
    },
    /// `...value`, `ADD_ARRAY_UNPACK`.
    Unpack(tree::Expr),
}

/// The class a `::`, a `new` or an `instanceof` stands on, as the tree
/// holds it: a name written out, php's `ZEND_AST_ZVAL`, or any other
/// expression, whose value names the class as the program serves.
#[derive(Clone, Copy)]
enum ClassSpec {
    Name(TokenId),
    Expr(tree::Expr),
}

impl ClassSpec {
    /// The class a `::`, a `new` or an `instanceof` stands on: a name
    /// written out, php's `ZEND_AST_ZVAL` that `zend_compile_class_ref`
    /// reads without compiling, or an expression it compiles. A name in
    /// parentheses is an expression, `'(' expr ')'` in
    /// `class_name_reference` and `fully_dereferenceable`, so `new (FOO)`
    /// names the class the constant `FOO` holds.
    ///
    /// ```php
    /// const C = 'B'; class B {} class C {} var_dump(new (C));
    /// var_dump(new C instanceof (C), new B instanceof (C));
    /// ```
    /// ```expect
    /// object(B)#1 (0) {
    /// }
    /// bool(false)
    /// bool(true)
    /// ```
    fn of(expr: tree::Expr) -> ClassSpec {
        match expr {
            tree::Expr::NameRef(name) => ClassSpec::Name(name),
            other => ClassSpec::Expr(other),
        }
    }
}

/// A class operand as php's tree holds it where a compile reads it,
/// after any `zend_eval_const_expr` has evaluated it, decoded once from a
/// [`ClassSpec`] ([`Emit::decoded_class`]): a name written out,
/// `ZEND_AST_ZVAL` with the `attr` of its spelling; a value,
/// `ZEND_AST_ZVAL` with `ZEND_NAME_FQ`, which the parser made of a string
/// or a number written where the class goes or the evaluator folded the
/// operand to; or an expression, compiled for the class it names.
#[derive(Clone, Copy)]
enum ClassOperand {
    Name(TokenId),
    Value(Zval),
    Expr(tree::Expr),
}

/// What `zend_compile_class_ref` (`Z/Zend/zend_compile.c:2819`) leaves
/// in its `znode`: a class named at compile time, its name resolved
/// (`IS_CONST`); `self`, `parent` or `static`, the fetch type and its
/// flags in the operand's number (`IS_UNUSED`); or the `VAR` a
/// `FETCH_CLASS` wrote the class into.
enum ClassRef<'a> {
    Named(&'a [u8]),
    Scope(u32),
    Fetched(u32),
}

/// `sizeof(void*)`: what one runtime cache slot takes of `cache_size`.
const SLOT: u32 = 8;

/// `cache_size` once the sites have taken `slots` of it, as `pass_two`
/// finishes it (`Z/Zend/zend_opcode.c:1140-1147`): a `RECV_INIT` whose
/// default php could not fold, an `IS_CONSTANT_AST`, keeps the value it
/// evaluates to in the runtime cache, a zval past every slot, so a
/// function with such a default lays a cache sixteen bytes larger for
/// each:
///
/// ```php
/// class A {}
/// $fs = [function ($a = 1) { new stdClass; }, function ($a = X) { new stdClass; }, function ($a = X, $b = Y) { new stdClass; }];
/// foreach ($fs as $f) { $m = memory_get_usage(); $b = Closure::bind($f, null, A::class); echo memory_get_usage() - $m, " "; unset($b); }
/// ```
/// ```expect
/// 400 408 424
/// ```
fn defaults_cached(oplines: &[Opline], literals: &[Zval], slots: u32) -> u32 {
    let unfolded = oplines
        .iter()
        .filter(|op| op.opcode == Opcode::RecvInit)
        .filter(|op| {
            literals
                .get(op.op2 as usize)
                .is_some_and(|literal| literal.as_const_expr().is_some())
        })
        .count();
    let zval = u32::try_from(std::mem::size_of::<Zval>()).unwrap_or(16);
    let unfolded = u32::try_from(unfolded).unwrap_or(u32::MAX);
    slots.saturating_add(unfolded.saturating_mul(zval))
}

/// `ZEND_FETCH_CLASS_SELF`, `_PARENT` and `_STATIC`
/// (`Z/Zend/zend_compile.h:1028`), the fetch types of a class reference
/// of the scope, and `ZEND_FETCH_CLASS_DEFAULT`, a class by name.
const FETCH_CLASS_DEFAULT: u32 = 0;
const FETCH_CLASS_SELF: u32 = 1;
const FETCH_CLASS_PARENT: u32 = 2;
const FETCH_CLASS_STATIC: u32 = 3;

/// `ZEND_FETCH_CLASS_NO_AUTOLOAD`, `ZEND_FETCH_CLASS_SILENT` and
/// `ZEND_FETCH_CLASS_EXCEPTION` (`Z/Zend/zend_compile.h:1035`), the flags
/// a class reference's fetch carries beside its type.
const FETCH_CLASS_NO_AUTOLOAD: u32 = 0x80;
const FETCH_CLASS_SILENT: u32 = 0x0100;
const FETCH_CLASS_EXCEPTION: u32 = 0x0200;

/// `zend_get_class_fetch_type` (`Z/Zend/zend_compile.c:1732`): which of
/// the three a name written out is, whatever its case, and
/// `ZEND_FETCH_CLASS_DEFAULT` for every other name.
fn class_fetch_type(name: &[u8]) -> u32 {
    if name.eq_ignore_ascii_case(b"self") {
        FETCH_CLASS_SELF
    } else if name.eq_ignore_ascii_case(b"parent") {
        FETCH_CLASS_PARENT
    } else if name.eq_ignore_ascii_case(b"static") {
        FETCH_CLASS_STATIC
    } else {
        FETCH_CLASS_DEFAULT
    }
}

/// What `zend_compile_simple_var` makes of a `$name`: the CV
/// `zend_try_compile_cv` gives it, `$this`, the one name
/// `is_this_fetch` picks out, fetched by `FETCH_THIS` and never a CV, or
/// an auto-global, which `zend_try_compile_cv` refuses and
/// `zend_compile_simple_var_no_cv` fetches by its name from
/// `EG(symbol_table)` ([`by_name`]), or `$GLOBALS`, `is_globals_fetch`,
/// which is `FETCH_GLOBALS` and no variable.
#[derive(PartialEq, Eq)]
enum SimpleVar<'a> {
    Cv(&'a [u8]),
    This,
    Auto(AutoGlobal),
    Globals,
}

/// `ZEND_FETCH_LOCAL` (`Z/Zend/zend_compile.h:1083`), the
/// `extended_value` of a `FETCH_*` by name that is no auto-global, which
/// the dump prints as `(local)`.
const FETCH_LOCAL: u32 = 1 << 2;

/// `ZEND_FETCH_GLOBAL` (`Z/Zend/zend_compile.h:1082`), the same slot for
/// a name `zend_is_auto_global` answers for, printed `(global)`.
const FETCH_GLOBAL: u32 = 1 << 1;

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

/// `ZEND_BIND_REF` (`Z/Zend/zend_compile.h:1194`), on a `BIND_STATIC`
/// that binds its CV to the table's entry by reference, which every
/// `static` does.
const BIND_REF: u32 = 1;

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
/// The callee `zend_get_call_op` is handed with its `init_op`
/// ([`Emit::get_call_op`]): a native after `INIT_FCALL`, a user function
/// or method the compiler knows, or none after `INIT_FCALL_BY_NAME` or
/// `INIT_NS_FCALL_BY_NAME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fbc {
    Native,
    User,
    ByName,
}

#[allow(clippy::struct_excessive_bools)] // php's compiler globals, each one fact of the compile
pub(crate) struct Emit<'a> {
    /// The parse this compiles: the tree the walk reads (design §4.9),
    /// and the token stream and the source a name, a value and a doc
    /// comment are read from.
    parsed: &'a Parsed<'a>,
    /// The compile's arena, the parse's own, which the compiler's
    /// temporaries are bumped into and given up with it (#2810): a name
    /// as resolved or lowered on its way to the op array's constants,
    /// which copy it, and the lists of a call's arguments, an array's
    /// elements and an `if`'s arms.
    arena: &'a Arena,
    strings: &'a mut dyn Intern,
    /// php's `precision` while the file compiles, [`Options::precision`].
    precision: i32,
    /// What an `assert()` compiles to, [`Options::assertions`].
    assertions: assert::Assertions,
    /// The engine's persistent constants, [`Options::persistent`].
    persistent: crate::Persistent<'a>,
    /// The engine's natives, [`Options::functions`].
    functions: Indexed<'a, FunctionDecl, php_link::Folded>,
    /// The engine's classes, [`Options::classes`].
    known_classes: Indexed<'a, ClassDecl, php_link::Folded>,
    /// The hooks the host replaced, [`Options::hooked`].
    hooked: crate::Hooked,
    /// Whether the extended oplines are emitted,
    /// [`Options::extended_info`].
    extended_info: bool,
    /// The request's, [`Options::request_classes`].
    request_classes: crate::RequestClasses<'a>,
    /// The request's, [`Options::request_constants`].
    request_constants: Option<crate::RequestConstants<'a>>,
    /// `zend_flf_functions` over them, [`Options::frameless`].
    frameless: &'a [FramelessEntry],
    /// `FC(current_namespace)`: the namespace a `namespace` statement
    /// put what follows it in, and none before one.
    namespace: Option<Vec<u8>>,
    /// How the file's `namespace` statements are written so far, and
    /// whether the braces of one are open.
    namespacing: Namespacing,
    /// `FC(imports)`, `FC(imports_function)`, `FC(imports_const)` and
    /// `FC(seen_symbols)`: what the `use` statements of the current
    /// namespace imported and what the file declared in it.
    imports: Imports,
    /// `CG(active_class_entry)`: the class whose members are being
    /// compiled, and none outside a class body.
    active_class: Option<ActiveClass>,
    /// The string the last fold a constant expression stands as was
    /// read as ([`Self::folded`]), which a class constant's declaration
    /// and a default keep ([`Self::held_value`]).
    folded_string: Option<Zval>,
    /// The `.`s php's parser folded before the compiler saw them
    /// ([`Self::parser_folds`]), each a `ZEND_AST_ZVAL` to the `.` over
    /// it.
    parser_dots: std::collections::HashSet<NodeId<tree::BinaryExpr>>,
    /// `CG(active_class_entry)->parent_name`: the name the class being
    /// compiled extends, which `parent` needs one of.
    active_parent: Option<Vec<u8>>,
    /// `CG(active_op_array)->function_name`: the function or method whose
    /// body is being compiled, and none in the main op array, which
    /// `__FUNCTION__` and `__METHOD__` fold to.
    active_function: Option<ActiveFunction>,
    /// The strings this compile made as it went that a declaration owns
    /// as its name or its key, each beside where that stands, handed to
    /// the home once the unit is whole ([`crate::Parts::intern`]).
    own_names: Vec<(crate::Named, Str)>,
    /// The file's name, `CG(compiled_filename)`, made the first time
    /// `__FILE__` asked, which the unit owns as `op_array->filename`.
    file_own: Option<Str>,
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
    /// How many statement lists stand open: one inside the file's own
    /// or a bracketed namespace's, whose statements are both php's top
    /// statements and where `zend_compile_top_stmt` binds a function at
    /// compile time; more inside a block, a loop or a body.
    depth: u32,
    /// `CG(context).active_property_info_name`: the property whose
    /// default or hook is being compiled, which `__PROPERTY__` folds to
    /// and `parent::$x::get()` is held to.
    active_property: Option<Vec<u8>>,
    /// `CG(context).active_property_hook_kind`: the hook whose body is
    /// being compiled, none outside one.
    active_hook: Option<HookKind>,
    /// `CG(context).in_jmp_frameless_branch`: whether a
    /// `JMP_FRAMELESS` branch is being compiled, in which no call takes
    /// a branch of its own.
    in_jmp_frameless_branch: bool,
    /// How many statements have been compiled, which is what a first
    /// `namespace` statement and a `strict_types` declaration must both
    /// find at zero (`zend_is_first_statement`). A `declare` is not one
    /// of them, since php's walk steps over every `ZEND_AST_DECLARE`
    /// before the node it is asked about, and a `namespace` statement
    /// is, since php's walk does not.
    statements_seen: u32,
    /// Whether every statement of the file's own list compiled so far is
    /// a `declare`, which `zend_is_first_statement` without `allow_nop`
    /// holds an `encoding` directive to: an empty statement (php's `NULL`
    /// child) ends it as any other does, and what a `declare`'s block
    /// holds is not in the list.
    pub(super) top_list: TopList,
    /// `ZEND_ACC_STRICT_TYPES` on the file's main op array, which
    /// `declare(strict_types=1)` sets and every op array compiled after
    /// it takes a copy of (`Z/Zend/zend_compile.c:8465`): the emitter's
    /// bodies nest inside the main one, so the bit is the file's and
    /// [`Emit::begin_body`] does not save it. A caller's, which is what
    /// `eval` and an `include` compile under, is
    /// [`Options::strict_types`].
    typing: Typing,
    /// `FC(declarables).ticks`: the count `declare(ticks=N)` set, none
    /// where no declaration stands or it declared zero. A file-level
    /// fact, which a function's body compiled inside the scope takes and
    /// [`Emit::begin_body`] does not save; a `declare` with a statement
    /// after its `)` puts back what stood before it once the statement
    /// is compiled.
    ticks: Option<NonZeroI64>,
    /// The thread's stack limit, [`Options::stack`], held to where the
    /// emitter recurses.
    stack: StackLimit,
    oplines: Vec<Opline>,
    /// php's `CG(short_circuiting_opnums)`: the `JMP_NULL` of each `?->`
    /// whose chain has not ended yet ([`chain`]).
    short_circuits: Vec<u32>,
    /// The node the next [`Self::expr`] compiles as a link of a chain the
    /// caller ends, php's `ZEND_SHORT_CIRCUITING_INNER`.
    chain_inner: Option<tree::Expr>,
    /// php's `CG(memoize_mode)` and `CG(memoized_exprs)`, which a `??=`
    /// opens over its place ([`memo`]).
    memo: memo::Memo,
    /// How the constant expression [`Self::const_expr`] builds is
    /// evaluated, php's `allow_dynamic` ([`Evaluation`]).
    const_evaluation: Evaluation,
    /// Whether [`Self::persistent`] substitutes a persistent constant,
    /// which it does but over a parameter's default ([`Substitution`]).
    substitution: Substitution,
    constants: Vec<Zval>,
    /// [`OpArray::const_exprs`]: the constant expressions a constant of
    /// kind `IS_CONSTANT_AST` indexes, in the order they were added.
    const_exprs: Vec<ConstExpr>,
    vars: Vec<Name>,
    /// `CG(context).has_assigned_to_http_response_header`.
    http_response_header: ResponseHeader,
    safepoints: Safepoints,
    temporaries: u32,
    cache_slots: u32,
    line: u32,
    /// What `zend_error` raised so far, [`Unit::diagnostics`].
    diagnostics: Vec<Diagnostic>,
    /// The classes the file declares, [`Unit::classes`], in the order
    /// `zend_compile_class_decl` bound them; the last is the one whose
    /// body is being compiled while `active_class` names it.
    classes: Vec<ClassDecl>,
    /// Which of [`Emit::classes`] were bound early, in the order bound:
    /// the file's classes `CG(class_table)` holds while it compiles.
    bound: Vec<usize>,
    /// [`Unit::top_level`]: each top-level function and class, in the
    /// order compiled.
    top_level: Vec<TopLevel>,
    /// [`Unit::completed`]: each class of [`Emit::classes`] as its
    /// declaration completed.
    completed: Vec<u32>,
    /// The rest of [`Emit::classes`], each under the runtime definition
    /// key its `DECLARE_CLASS` carries, [`Unit::runtime_keys`], in the
    /// order the keys were made.
    runtime_keys: Vec<(Name, u32)>,
    /// Each constant a `const` statement declared with attributes,
    /// [`Unit::constants`], which the `OP_DATA` after its
    /// `DECLARE_ATTRIBUTED_CONST` names by index.
    attributed_constants: Vec<ConstDecl>,
    /// [`Options::keys_before`]: the keys made before this source.
    keys_before: u32,
    /// `zend_op_array_extension_handles * sizeof(void*)`, what
    /// `init_op_array` starts every op array's `cache_size` at
    /// ([`Options::op_array_extensions`]).
    first_slot: u32,
    /// The functions the file declares with their bodies,
    /// [`Unit::functions`]: `CG(function_table)` past the natives, each
    /// entry landing once its body has compiled, which is when php's
    /// `ZEND_ACC_DONE_PASS_TWO` lets a call site know it.
    functions_declared: Vec<(FunctionDecl, OpArray)>,
    /// What `CG(function_table)` and `CG(class_table)` took of the file
    /// so far: each top-level function and each class bound early, as it
    /// landed, beside how many diagnostics had been raised by then, which
    /// a refusal after it carries ([`crate::Declared`]).
    landed: Vec<(TopLevel, usize)>,
    /// The functions the file declares that no name binds while it
    /// compiles, [`Unit::dynamic`]: php's `dynamic_func_defs` over every
    /// op array of the file, in the order `zend_add_dynamic_func_def`
    /// made them. A slot is taken before the body compiles, as php takes
    /// its offset there, and filled when it has.
    dynamic: Vec<Option<(FunctionDecl, OpArray)>>,
    /// [`Unit::cell`]: the one cell every closure of the file's constant
    /// expressions holds a share of ([`ConstExpr::Closure`]).
    cell: php_unit::UnitCell,
    /// How many `name(...)` of the file's constant expressions were
    /// compiled, each one's number in [`ConstExpr::FunctionCallable`].
    function_callables: u32,
    /// `op_array->dynamic_func_defs` of the op array being compiled:
    /// which entries of [`Emit::dynamic`] it declares, in the order a
    /// `DECLARE_FUNCTION`'s `op2` indexes them.
    dynamic_func_defs: Vec<u32>,
    /// [`OpArray::nested`] of the op array being compiled: each function,
    /// closure and class begun while it is the active one, with how far
    /// it had grown by then.
    nested: Vec<Nested>,
    /// The numeric-dimension pairs of the op array being compiled,
    /// [`OpArray::numeric_dims`]: the slot of every constant dimension
    /// `zend_handle_numeric_dim` folded, in the order they were laid
    /// down.
    numeric_dims: Vec<ConstIdx>,
    /// The method bodies of `classes`, [`Unit::methods`].
    methods: Vec<MethodBody>,
    /// The hook bodies of the properties of `classes`, [`Unit::hooks`].
    hooks: Vec<HookBody>,
    /// The file's name, [`Options::filename`], what a declaration
    /// records as its site.
    filename: &'a [u8],
    /// The file's name of a unit the caller gave up ([`crate::Room`]),
    /// written over for the first op array finished where it is as long.
    room_filename: Option<Name>,
    /// The request's working directory, [`Options::cwd`], what `__DIR__`
    /// is where the file's name has no directory.
    cwd: &'a [u8],
    /// `CG(context).brk_cont_array`: every loop and `switch` of the op
    /// array, in the order begun, and `current_brk_cont`, the innermost
    /// one open.
    loops: Vec<Loop>,
    current_loop: Option<usize>,
    /// The `ZEND_BRK`, `ZEND_CONT` and `ZEND_GOTO` oplines, which
    /// `pass_two` turns into the `JMP` each is once every loop's end and
    /// every label is known, in the order they stand.
    jumps: Vec<Pending>,
    /// `CG(context).labels`: every label of the op array being compiled,
    /// by name, with the opline it stands before and the loop it stands
    /// in (`zend_label`).
    labels: Vec<(Vec<u8>, Label)>,
    /// `op_array->try_catch_array`: every `try` of the op array in the
    /// order `zend_add_try_element` added them, which is the order their
    /// `try_op`s stand in.
    try_catch: Vec<TryCatch>,
    /// `CG(loop_var_stack)`, the finally half: what a `return`, a
    /// `break` or a `goto` out of here must emit before its jump,
    /// innermost last (`zend_handle_loops_and_finally_ex`). The loop
    /// half is [`Emit::loops`],
    /// pushed and popped beside it, so an entry records how many loops
    /// stood open under it and the walk interleaves the two again.
    unwind: Vec<Unwind>,
    /// How many entries `CG(loop_var_stack)` holds, across every op array
    /// being compiled: a loop's (`zend_begin_loop`), a `finally`'s and
    /// the separator `zend_compile_func_decl` pushes for each function
    /// body, which no op array sets aside.
    loop_vars: u32,
    /// The most [`Emit::loop_vars`] has held ([`Unit::loop_vars`]).
    loop_var_peak: u32,
    /// `CG(delayed_oplines_stack)`, across every op array being
    /// compiled: how many fetches `zend_delayed_emit_op` holds back now,
    /// each a [`HeldBack`] standing while its fetch waits to be laid
    /// down, and the most it has held ([`Unit::delayed_fetches`]).
    delayed: Rc<DelayedStack>,
    /// `CG(context).fast_call_var`: the temporary the innermost
    /// `finally` keeps its return address and its delayed throwable in,
    /// and none outside one.
    fast_call_var: Option<u32>,
    /// `CG(context).try_catch_offset`: which element of [`Emit::try_catch`]
    /// the oplines being emitted stand in, `-1` outside every `try`.
    try_catch_offset: Option<u32>,
    /// `op_array->static_variables`: the variables the op array being
    /// compiled binds through `BIND_STATIC`, in the order
    /// `zend_compile_static_var_common` added them, which is what a
    /// `BIND_LEXICAL`'s and a `BIND_STATIC`'s `extended_value` names.
    static_vars: Vec<BoundVar>,
    /// Whose fold is running, `zend_compile_expr`'s or
    /// `zend_eval_const_expr`'s ([`Fold`]).
    fold: Fold,
    /// The nodes `zend_eval_const_expr` replaced in php's tree, which
    /// every compile after it reads ([`eval`]).
    rewrites: eval::Rewrites,
    /// The strings a fold made that the node it folded alone holds,
    /// given back as the node over it consumes them ([`eval`]).
    claims: eval::Claims,
    /// `op_array->fn_flags` of the op array being compiled, the bits
    /// this compiler reads back.
    fn_flags: FnFlags,
    /// Where every generator of the file starts, ascending
    /// ([`walk::generators`]): php's parser marks `ZEND_ACC_GENERATOR`
    /// on a declaration before its body compiles.
    generators: Vec<u32>,
    /// Each auto-global the file names, in the order it was first met
    /// ([`Unit::arms`]).
    arms: Vec<AutoGlobal>,
    /// The offset of the `__halt_compiler();` the file's top statements
    /// end with, which `__COMPILER_HALT_OFFSET__` compiles to anywhere
    /// in the file (`zend_compile_const`), found before the walk as php
    /// finds it in `CG(ast)`; and the one the walk met, which the unit
    /// carries.
    halt: Option<i64>,
    halted: Option<i64>,
}

/// Which of php's two folds is running: `zend_compile_expr`'s, which
/// folds a call of a function [`crate::special`] knows over constants,
/// or `zend_eval_const_expr`'s, which folds no call. A `static`'s
/// initializer is the one place the second runs over an expression the
/// first would otherwise fold ([`Emit::static_var`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fold {
    Compile,
    EvalConstExpr,
}

/// `op_array->fn_flags` (`Z/Zend/zend_compile.h:283-296`), the two bits
/// the compiler sets and reads: `ZEND_ACC_CLOSURE`, which
/// `zend_begin_func_decl` reads to name a closure written inside
/// another one after its enclosing closure and not after a
/// `{closure:…}()` of its own, and `ZEND_ACC_STATIC`, which a closure
/// written `static function` or `static fn` carries and
/// `ZEND_DECLARE_LAMBDA_FUNCTION` reads to leave `$this` unbound.
#[allow(clippy::struct_excessive_bools)] // php's ZEND_ACC_* bits, one field each, and whether the op array has a scope
#[derive(Clone, Copy, Default)]
struct FnFlags {
    closure: bool,
    is_static: bool,
    /// `ZEND_ACC_GENERATOR`, which a `yield` in the body sets and every
    /// `return` reads.
    generator: bool,
    /// `ZEND_ACC_ABSTRACT`, which a method of an interface carries too:
    /// a constructor that is may promote no parameter.
    is_abstract: bool,
    /// Beside the bits, `op_array->scope` as the compiler leaves it: set
    /// on a method's op array by `zend_begin_method_decl` and on no
    /// other, a closure's included, which `__METHOD__` reads.
    method: bool,
}

/// What a body written as one expression after `=>` makes of it: the
/// `return` `zend_compile_func_decl` makes of an arrow function's and
/// `zend_compile_property_hooks` of a `get` hook's, or the assignment
/// to the backing value `zend_compile_property_hooks` makes of a `set`
/// hook's, `$this->name = expr;`.
#[derive(Clone, Copy)]
enum Short {
    Return(tree::Expr),
    Assign(tree::Expr),
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

/// One `ZEND_BRK`, `ZEND_CONT` or `ZEND_GOTO` before `pass_two`
/// resolves it: the opline, and what the resolution reads off it.
enum Pending {
    /// `ZEND_BRK` or `ZEND_CONT`: the loop it was written in (`op1`),
    /// its depth (`op2`) and whether it breaks.
    Break {
        at: u32,
        r#loop: usize,
        depth: u32,
        is_break: bool,
    },
    /// `ZEND_GOTO`: the label's constant (`op2`), how many oplines
    /// [`Emit::handle_loops_and_finally`] emitted before it (`op1`),
    /// and the loop it was written in (`extended_value`).
    Goto {
        at: u32,
        label: u32,
        emitted: u32,
        r#loop: Option<usize>,
    },
}

/// `zend_label`: where a label stands, the opline after it, and the
/// loop it was written in, which a `goto` to it must be written in too.
#[derive(Clone, Copy)]
struct Label {
    opline: u32,
    r#loop: Option<usize>,
}

/// The jump table of one `SWITCH_LONG`, `SWITCH_STRING` or `MATCH`
/// while its targets are being learnt: the opline, the constant slot
/// reserved for the array, the entries so far, and the size
/// `zend_hash_init` gives it, a `switch`'s cases or a `match`'s
/// conditions as written.
struct Table {
    at: u32,
    slot: u32,
    entries: Vec<(Key, Zval)>,
    size: usize,
}

/// What a jump table's key is taken from, the zval of the case as php's
/// tree holds it (`zend_hash_add(jumptable, Z_STR_P(cond_zv))`): the
/// scanner's string where nothing interned it, which the comparison's
/// constant is too where the request's home made it. A string the
/// engine's table holds is the comparison's alone, and the key a string
/// of its own, not interned ([`php_heap::Intern::interning`]).
#[derive(Clone, Copy)]
enum CaseValue {
    /// A `switch` case's constant, which its comparison keeps: the table
    /// takes one more owner of it.
    Compared(Zval),
    /// A `match` condition folded for the table alone, whose one owner
    /// the table takes.
    Folded(Zval),
}

/// `ZEND_FREE_ON_RETURN`, the bit `zend_compile.h` sets on the `FREE` a
/// `break` or `continue` emits for the subject of each `switch` it
/// leaves, which the live-range walk reads (`crate::live`).
const FREE_ON_RETURN: u32 = 1 << 0;

/// `IS_CONSTANT_UNQUALIFIED_IN_NAMESPACE`, the one bit a
/// `FETCH_CONSTANT`'s `op1` carries: the name was written unqualified
/// inside a namespace, so the global spelling stands in the constant
/// slot after the namespaced one.
const UNQUALIFIED_IN_NAMESPACE: u32 = 0x800;

/// `ZEND_ARRAY_NOT_PACKED` and `ZEND_ARRAY_SIZE_SHIFT`
/// (`Z/Zend/zend_compile.h:1202`): the bit `zend_compile_array` sets on
/// an `INIT_ARRAY` once a constant string key is seen, and where above
/// the two flags it writes the element count.
const ARRAY_NOT_PACKED: u32 = 1 << 1;
const ARRAY_SIZE_SHIFT: u32 = 2;
/// `ZEND_ARRAY_ELEMENT_REF` (`Z/Zend/zend_compile.h:1201`), the bit
/// `zend_compile_array` sets on the `INIT_ARRAY` or `ADD_ARRAY_ELEMENT`
/// of a `&` element.
const ARRAY_ELEMENT_REF: u32 = 1 << 0;
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

/// A node a compile is handed where the compiler may stand a value of
/// its own in, php's `zend_ast` under `ZEND_AST_ZNODE`: an expression of
/// the tree, compiled where it stands, or a value the compiler already
/// holds (`zend_ast_create_znode`), which compiles to itself and which no
/// reader of a node's shape takes for a call, a variable or a constant
/// written. The values held are a write's (`zend_emit_assign_znode`), a
/// pipe's argument and callee (`zend_compile_pipe`), and the description
/// `zend_compile_assert` hands `assert()`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Source {
    Expr(tree::Expr),
    Node(Where),
}

impl Source {
    /// The tree's node, none for a value held.
    pub(crate) fn tree(self) -> Option<tree::Expr> {
        match self {
            Source::Expr(expr) => Some(expr),
            Source::Node(_) => None,
        }
    }
}

/// `ZEND_FETCH_DIM_REF` (`Z/Zend/zend_compile.h:1095`), on the last
/// `FETCH_DIM_W` of a by-reference write: what php's string-offset error
/// reads to say `Cannot create references to/from string offsets`.
const FETCH_DIM_REF: u32 = 1;

/// `ZEND_FETCH_DIM_DIM` (`Z/Zend/zend_compile.h:1096`), on a
/// `FETCH_DIM_W` that has a dimension over it: `Cannot use string offset
/// as an array` the same way.
const FETCH_DIM_DIM: u32 = 2;

/// `ZEND_FETCH_DIM_OBJ` (`Z/Zend/zend_compile.h:1097`), on the last
/// `FETCH_DIM_W` of the chain a `->` stands on, where a dimension over
/// it would carry `ZEND_FETCH_DIM_DIM`: `Cannot use string offset as an
/// object` in place of that one's array
/// (`Z/Zend/zend_execute.c:1786`).
const FETCH_DIM_OBJ: u32 = 3;

/// `ZEND_FETCH_DIM_INCDEC` (`Z/Zend/zend_compile.h:1098`), on the last
/// `FETCH_DIM_RW` of a `++` or a `--` over an element: `Cannot
/// increment/decrement string offsets` in place of the array's
/// sentence.
const FETCH_DIM_INCDEC: u32 = 4;

/// One link of a dimension chain as `zend_delayed_compile_dim` leaves
/// it: the dimension's operand, `UNUSED` for the `[]` of an append, and
/// the `VAR` or temporary the opline writes.
struct DimLink {
    kind: OperandKind,
    at: u32,
    out: u32,
    held: HeldBack,
}

/// `CG(delayed_oplines_stack)` (`Z/Zend/zend_compile.c:425`), a
/// `zend_stack` of the `zend_op`s `zend_delayed_emit_op` holds back
/// while the rest of a chain of dimensions and properties compiles
/// (`:2458`) and `zend_delayed_compile_end` lays down: how many it holds
/// now and the most it has held. The stack is the compile's, so a chain
/// in a closure written inside a dimension stands on what the outer
/// chain holds.
#[derive(Default)]
struct DelayedStack {
    held: Cell<u32>,
    most: Cell<u32>,
}

/// One fetch `zend_delayed_emit_op` pushed onto [`DelayedStack`], which
/// stands on the stack for as long as this lives: beside the held-back
/// fetch until the fetch is laid down, as `zend_delayed_compile_end`
/// pops it. One of no stack holds nothing, which is what a fetch php's
/// compile does not delay carries.
#[derive(Default)]
struct HeldBack(Option<Rc<DelayedStack>>);

impl Drop for HeldBack {
    fn drop(&mut self) {
        if let Some(stack) = &self.0 {
            stack.held.set(stack.held.get().saturating_sub(1));
        }
    }
}

/// `zend_delayed_compile_dim`'s `type`, php's `BP_VAR_*` a chain of
/// dimensions and properties is compiled under. A read is `FETCH_DIM_R`
/// into a temporary (`zend_adjust_for_fetch_type` under `BP_VAR_R`),
/// where `[]` is php's compile error `Cannot use [] for reading`; a
/// write is `FETCH_DIM_W` into a `VAR` carrying `ZEND_FETCH_DIM_DIM`,
/// where `[]` is the append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fetch {
    Read,
    Write,
    /// `BP_VAR_RW`, the fetch a compound assignment's links take: the
    /// `_W` opcode plus one, which lays the place down as `_W` does and
    /// raises the read's own diagnostics on the way
    /// (`zend_adjust_for_fetch_type`'s second arm). php's
    /// `zend_delayed_compile_dim` gives a property base under it no
    /// `ZEND_FETCH_DIM_WRITE`, which it adds under `BP_VAR_W` alone.
    Rw,
    /// `BP_VAR_IS`, the read that raises nothing where what it reads is
    /// not there: `zend_adjust_for_fetch_type`'s third arm, which is
    /// `FETCH_DIM_R` plus three and a temporary for what it leaves.
    Is,
    /// `BP_VAR_FUNC_ARG`, the fetch of a variable sent to a callee the
    /// compiler knows nothing of: `zend_adjust_for_fetch_type`'s fourth
    /// arm, the `_R` opcode plus four into a `VAR`, which the engine
    /// serves as the write or as the read by what the `CHECK_FUNC_ARG`
    /// before it found. Its links are laid down as a write's are, and
    /// what stands under them is compiled as a read's is, with no
    /// `SEPARATE` over a call (`zend_separate_if_call_and_write`).
    FuncArg,
    /// `BP_VAR_UNSET`, the links under what an `unset` deletes:
    /// `zend_adjust_for_fetch_type`'s fifth arm, the `_R` opcode plus
    /// five into a `VAR`, which finds a place and makes none, and `[]`
    /// php's `Cannot use [] for unsetting`. A call under it is
    /// `SEPARATE`d as under a write.
    Unset,
}

/// The variable the http wrapper leaves in its caller's scope, and
/// what php 8.5 says of a read of it (`Z/Zend/zend_compile.c:2893`).
const HTTP_RESPONSE_HEADER: &[u8] = b"http_response_header";
const HTTP_RESPONSE_HEADER_DEPRECATED: &str = "The predefined locally scoped $http_response_header variable is deprecated, call http_get_last_response_headers() instead";

/// What `$http_response_header` is to the op array being compiled,
/// `CG(context).has_assigned_to_http_response_header`: the variable the
/// http wrapper leaves, whose reads php 8.5 deprecates, until the op
/// array writes it or takes it as a parameter and it is its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ResponseHeader {
    #[default]
    Predefined,
    Own,
}

/// `ZEND_FETCH_DIM_WRITE` (`Z/Zend/zend_compile.h:1096`), the bit
/// `zend_delayed_compile_dim` puts on the delayed `FETCH_OBJ_W` or
/// `FETCH_STATIC_PROP_W` under it: what the handler reads to know the
/// place it hands out is written through rather than written to.
const FETCH_DIM_WRITE: u32 = 2;

/// `ZEND_FETCH_REF` (`Z/Zend/zend_compile.h:1089`), the bit a
/// `FETCH_OBJ_W` and a `FETCH_STATIC_PROP_W` carry where `by_ref` stood
/// on the `zend_compile_var` that made them, which is a `&` and nothing
/// else: what `zend_handle_fetch_obj_flags` reads to bind the
/// property's own cell where a plain write would hand a place out. It
/// shares `extended_value` with the cache slots, which is why php masks
/// with `ZEND_FETCH_OBJ_FLAGS`, and the dump prints it `(ref)`.
const FETCH_REF: u32 = 1;

/// One held-back `FETCH_OBJ_RW` or `FETCH_OBJ_W` of a property chain,
/// php's delayed `zend_delayed_compile_prop`: the name's operand with
/// its cache slots and the `VAR` the fetch writes into, all taken where
/// the fetch was written and the opline laid down after the value.
struct PropLink {
    name: PropName,
    out: u32,
    /// Its place on the stack while it waits to be laid down.
    _held: HeldBack,
}

/// What a chain of dimensions or of property fetches stands on, php's
/// `zend_delayed_compile_var` over the node under the innermost `[` or
/// `->`: an operand settled where the base was written, which is a
/// variable's CV, `$this`, the `VAR` a call was separated into and what
/// every expression php compiles in place leaves; or one of the fetches
/// php delays, a dimension's, a property's and a `static` property's,
/// whose oplines are laid down after the chain over them has compiled,
/// so `$o->p[f()]` calls `f` before it fetches `p`.
enum Base {
    Settled((OperandKind, u32)),
    /// A chain of dimensions, `zend_delayed_compile_dim` over a node
    /// under a `->`: `$a[0]->p = 1` fetches `$a[0]` for writing, where
    /// an object compiled in place would read it into a temporary.
    Dim {
        container: Box<Base>,
        links: Vec<DimLink>,
    },
    /// A property's fetch, and what a `?->` under it laid down early,
    /// which php's stack keeps until the chain ends (`zend_delayed_compile_prop`
    /// turns each into a `NOP` where it stands).
    Prop {
        object: Box<Base>,
        name: PropName,
        out: u32,
        held: HeldBack,
        flushed: Vec<HeldBack>,
    },
    /// A `static` property's fetch, held back where php's compile delays
    /// it ([`Emit::hold_base`]) and holding nothing where it does not.
    StaticProp {
        name: (OperandKind, u32),
        class: (OperandKind, u32),
        out: u32,
        slots: u32,
        held: HeldBack,
    },
    /// A variable fetched by name, `zend_compile_simple_var_no_cv`
    /// with `delayed`: the name's operand, the table and the
    /// temporary, the opline laid down by [`by_name`], held back where
    /// php's compile delays it ([`Emit::hold_base`]).
    ByName {
        name: (OperandKind, u32),
        table: Symbols,
        out: u32,
        held: HeldBack,
    },
}

impl Base {
    /// Every fetch held back in this base, taken out where a `?->` lays
    /// them down early, for the stack to keep until the chain ends.
    fn take_held(&mut self, into: &mut Vec<HeldBack>) {
        match self {
            Base::Settled(_) => {}
            Base::Dim { container, links } => {
                container.take_held(into);
                into.extend(links.iter_mut().map(|link| std::mem::take(&mut link.held)));
            }
            Base::Prop {
                object,
                held,
                flushed,
                ..
            } => {
                object.take_held(into);
                into.push(std::mem::take(held));
                into.append(flushed);
            }
            Base::StaticProp { held, .. } | Base::ByName { held, .. } => {
                into.push(std::mem::take(held));
            }
        }
    }
}

/// What stands over the last held-back opline of a base, the compile
/// function that patches it once it has it back:
/// `zend_delayed_compile_dim`, which marks a dimension under it
/// `ZEND_FETCH_DIM_DIM` and a property under it `ZEND_FETCH_DIM_WRITE`;
/// or `zend_delayed_compile_prop`, which marks a dimension under it
/// `ZEND_FETCH_DIM_OBJ` and leaves a property alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Over {
    Dim,
    Obj,
    /// Nothing but the `&` itself, php's `by_ref` on
    /// `zend_compile_var`: a dimension's last link carries
    /// `ZEND_FETCH_DIM_REF` and a property's and a `static` property's
    /// fetch carries `ZEND_FETCH_REF`. What stands under such a last
    /// link is a dimension all the same, so the chain below it is
    /// marked as any other chain's is.
    Ref,
}

/// One link of a chain compiled under `BP_VAR_IS`, the opline
/// `zend_delayed_emit_op` holds back while the rest of the chain
/// compiles: the fetch's opcode, the operands settled where the link
/// was made, the temporary it writes, and its cache slots. `op1` is
/// `None` where the link reads the one under it.
struct IsLink {
    opcode: Opcode,
    op1: Option<(OperandKind, u32)>,
    op2_type: OperandKind,
    op2: u32,
    out: u32,
    extended_value: u32,
    /// Its place on the stack, and what a `?->` under it laid down
    /// early, which the stack keeps until the chain ends.
    held: HeldBack,
    flushed: Vec<HeldBack>,
}

/// Where a link of a `BP_VAR_IS` chain stands: the outermost, which
/// `isset` and `??` compile with `zend_compile_var`, or under another,
/// as the base `zend_delayed_compile_var` compiles. php's compile holds
/// a fetch by name or of a `static` property back only under another.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Outermost,
    Under,
}

/// What stands under one link of a `BP_VAR_IS` chain.
enum Under {
    /// The link before it, whose temporary the fetch reads.
    Link,
    /// The value the chain is read off.
    Value(Where),
}

/// A constant `zend_try_ct_eval_const` found and `can_ct_eval_const`
/// lets be its value, before it is copied where its name was written
/// ([`Emit::ct_found`]).
#[derive(Clone, Copy)]
enum Found<'a> {
    /// A persistent constant the engine declared, its value a scalar
    /// written out ([`declared_scalar`]).
    Declared(Scalar<'a>),
    /// A constant whose table holds its value, a view of it: one the
    /// request defined, or `PHP_SAPI` ([`crate::Persistent::host`]).
    Defined(Zval),
}

impl Found<'_> {
    /// What `zend_is_true` reads off the value, read without copying it.
    fn truth(self) -> Truth {
        match self {
            Found::Declared(Scalar::Double(d)) if d.is_nan() => Truth::Nan,
            Found::Declared(scalar) => Truth::Is(scalar.is_true()),
            Found::Defined(value) => Truth::of(value),
        }
    }
}

/// What `zend_is_true` reads off a constant the compile asks: a NAN,
/// which is true and over which `i_zend_is_true` warns
/// (`Z/Zend/zend_operators.h:425`), or the truth of anything else
/// ([`Emit::is_true`]).
#[derive(Clone, Copy)]
pub(crate) enum Truth {
    Nan,
    Is(bool),
}

impl Truth {
    pub(crate) fn of(value: Zval) -> Truth {
        if value.as_double().is_some_and(f64::is_nan) {
            Truth::Nan
        } else {
            Truth::Is(truth(value))
        }
    }
}

/// What an operator's fold answered: a string the fold made that the
/// node over it reads once, held as the operand's own claim
/// ([`eval::Claims`]), or a value some other place keeps, a
/// written operand's, a constant's, or one no claim covers.
enum Operated {
    Made(Made),
    Kept(Zval),
}

/// Which of php's two folds of a `.` over two constants
/// ([`Emit::concat_made`]): both are `concat_function`, each into a
/// zval of its own choosing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConcatFold {
    /// `zend_ast_create_concat_op` as the parser reduces the `.`, into
    /// its left side.
    Parser,
    /// `zend_try_ct_eval_binary_op` as the compiler folds it, into a
    /// fresh zval.
    Compiler,
}

impl Operated {
    /// The value, kept where it stands: a claim dropped, its string the
    /// unit's as any other the compile made.
    fn kept(self) -> Zval {
        match self {
            Operated::Made(made) => made.value(),
            Operated::Kept(value) => value,
        }
    }

    /// The value, a view: a claim stays the holder's.
    fn value(&self) -> Zval {
        match self {
            Operated::Made(made) => made.value(),
            Operated::Kept(value) => *value,
        }
    }
}

/// What a magic constant folds to: `__LINE__`'s number, or the string
/// every other one is.
enum Folded {
    Line(i64),
    Text(Vec<u8>),
    /// `__DIR__`, laid as a string of the second's length and cut to the
    /// first: the file name's, or `MAXPATHLEN` where the directory is
    /// the working one, which `zend_string_extend` grows it to before
    /// `VCWD_GETCWD` writes it ([`php_heap::Intern::intern_in`]).
    Dir(Vec<u8>, usize),
    /// The name of the function the constant stands in, whose own
    /// string php copies (`ZVAL_STR_COPY`, [`Emit::function_name_made`]).
    FunctionName,
    /// `CG(compiled_filename)`, whose string php copies
    /// ([`Emit::file_name_made`]).
    FileName,
    /// The name of the class the constant stands in, `ce->name`
    /// ([`Emit::class_name_made`]).
    ClassName,
    /// `__CLASS__` in a trait, the one magic constant
    /// `zend_try_ct_eval_magic_const` leaves to the program: the class
    /// that uses the trait, fetched as `FETCH_CLASS_NAME` of `self`.
    UsingClass,
}

/// `CG(active_class_entry)` as the compiler reads it: the class's name,
/// and its kind, of which a trait's is the one whose `self` names the
/// class that will use it and so is known to no fold
/// (`zend_is_scope_known`).
struct ActiveClass {
    name: Vec<u8>,
    kind: ClassKind,
    /// Where the declaration stands in [`Emit::classes`], which is not
    /// the last one once a method has declared an anonymous class.
    at: usize,
    /// `ZEND_ACC_ANON_CLASS`, whose `self` and `parent` a type keeps as
    /// written.
    anonymous: bool,
    /// `ce->name` once the body begins, the string the class's name is,
    /// which `self::class` and `__CLASS__` copy (`zend_string_copy`).
    own: Option<Str>,
    /// The method whose body is compiling, which `zend_begin_method_decl`
    /// put into the class's `function_table` before the body: none
    /// between methods.
    declaring: Option<Declaring>,
    /// `ce->num_hooked_prop_variance_checks`: the `set` hooks whose
    /// parameter's type could not be held to the property's while the
    /// file compiles.
    hooked_variance_checks: u32,
}

/// `CG(active_op_array)` as a function's body compiles: its
/// `function_name` by its bytes, where it lands in the unit, and the
/// string made for it once `__FUNCTION__`, `__METHOD__` or the key it is
/// bound under asked, which each of them copies (`zend_string_copy`).
struct ActiveFunction {
    name: Vec<u8>,
    at: crate::FunctionAt,
    own: Option<Str>,
}

impl ActiveFunction {
    /// The function named `name` that lands at `at`, no string made for
    /// its name yet.
    fn at(name: &[u8], at: crate::FunctionAt) -> ActiveFunction {
        ActiveFunction {
            name: name.to_vec(),
            at,
            own: None,
        }
    }
}

impl ActiveClass {
    /// `CG(active_class_entry) = ce` as the class's body begins: no
    /// method declaring yet and no hook counted.
    fn entered(
        name: &[u8],
        kind: ClassKind,
        at: usize,
        anonymous: bool,
        own: Option<Str>,
    ) -> ActiveClass {
        ActiveClass {
            name: name.to_vec(),
            kind,
            at,
            anonymous,
            declaring: None,
            hooked_variance_checks: 0,
            own,
        }
    }
}

/// What `zend_compile_class_decl` leaves for its caller once the body
/// has compiled: where the declaration stands in [`Emit::classes`], its
/// parent resolved, and `ce->num_hooked_prop_variance_checks`.
struct Entered {
    at: usize,
    parent: Option<Vec<u8>>,
    hooked_variance_checks: u32,
    /// The string the class's name is ([`ActiveClass::own`]).
    own: Option<Str>,
}

/// The method `zend_begin_method_decl` has put into
/// `CG(active_class_entry)->function_table` and whose body is compiling,
/// so a call in that body to the method itself is a call to a method the
/// table has: `DestructableObject::__destruct()` written inside
/// `DestructableObject::__destruct`.
struct Declaring {
    name: Vec<u8>,
    visibility: Visibility,
    is_final: bool,
    /// What a call site reads of it, none until `zend_compile_attributes`
    /// and `zend_compile_params` have read its attributes and its
    /// parameters, which no call in the body precedes.
    signature: Option<Signature>,
}

/// A user method's parameters, and whether its `fn_flags` carry
/// `ZEND_ACC_DEPRECATED` or `ZEND_ACC_NODISCARD`: what a call site whose
/// callee the table has reads of it.
struct Signature {
    parameters: Vec<Parameter>,
    flagged: bool,
}

/// One method `CG(active_class_entry)->function_table` holds while the
/// class compiles: a method compiled before the call, or the one whose
/// body the call stands in.
struct TableMethod {
    visibility: Visibility,
    is_final: bool,
    flagged: bool,
    callee: Callee,
}

/// The set visibility `zend_declare_typed_property` gives a public
/// readonly property that writes none, `ZEND_ACC_PROTECTED_SET`: a
/// readonly property is initialised from its class's scope or a related
/// one, and that is the rule a set visibility already states.
fn implicit_set_visibility(is_readonly: bool, visibility: Visibility) -> Option<Visibility> {
    (is_readonly && visibility == Visibility::Public).then_some(Visibility::Protected)
}

/// The `ce_flags` a class declaration starts with: its modifiers as
/// written, an enum final (`ZEND_ACC_FINAL` beside `ZEND_ACC_ENUM` from
/// the parser) and taking no dynamic property
/// (`zend_enum_register_props`), and a readonly class taking none
/// either, `ZEND_ACC_NO_DYNAMIC_PROPERTIES` from the parser, which
/// `#[\AllowDynamicProperties]` may not lift; an anonymous class is
/// not serializable (`zend_compile_class_decl`).
fn class_flags(
    class: &Class,
    dynamic_properties: DynamicProperties,
    is_anonymous: bool,
) -> ClassFlags {
    let is_enum = class.kind == ClassKind::Enum;
    ClassFlags {
        is_abstract: class.modifiers.is_abstract,
        is_final: is_enum || class.modifiers.is_final,
        is_readonly: class.modifiers.is_readonly,
        is_anonymous,
        is_top_level: false,
        not_serializable: is_anonymous,
        dynamic_properties: if is_enum || class.modifiers.is_readonly {
            DynamicProperties::Forbidden
        } else {
            dynamic_properties
        },
        use_guards: false,
        hooked_variance: false,
        stringable_added: false,
    }
}

/// `zend_is_valid_default_value` over a default or a constant's value
/// folded to a value: whether the type takes it, an int where only a
/// float is taken made the float in place, and the value's type where
/// the type does not take it. A value still naming a constant is held
/// where it is evaluated, so it is taken here.
fn valid_default(ty: &Type, default: &mut ConstExpr) -> Result<(), ZvalType> {
    let kind = match default {
        ConstExpr::Null => ZvalType::Null,
        ConstExpr::Bool(false) => ZvalType::False,
        ConstExpr::Bool(true) => ZvalType::True,
        ConstExpr::Long { .. } => ZvalType::Long,
        ConstExpr::Double(_) => ZvalType::Double,
        ConstExpr::Str(_) => ZvalType::String,
        ConstExpr::UnitValue(held) => held.value().kind(),
        _ => return Ok(()),
    };
    if ty.contains_code(kind as u8) {
        return Ok(());
    }
    if let ConstExpr::Long { value: n, .. } = default
        && ty.contains_code(ZvalType::Double as u8)
    {
        *default = ConstExpr::Double(double_of(Zval::long(*n)));
        return Ok(());
    }
    Err(kind)
}

/// `zend_zval_type_name` of a folded value's type.
fn zval_type_name(kind: ZvalType) -> &'static str {
    match kind {
        ZvalType::Null => "null",
        ZvalType::False | ZvalType::True => "bool",
        ZvalType::Long => "int",
        ZvalType::Double => "float",
        ZvalType::String => "string",
        _ => "array",
    }
}

/// The constant expression that stands for a value `zend_eval_const_expr`
/// folded, or `None` where the value is of a kind the unit format has no
/// constant expression for. A scalar becomes its own variant. An array is
/// not taken apart again: the array the fold built is kept, as a
/// [`UnitValue`], so a declaration reads that one array, and a write
/// through a copy of it separates first. That is so for code `eval`
/// compiles as for a file's, where a parameter defaults to an array
/// (#3121):
///
/// ```php
/// eval('function settings($o = ["depth" => [2, 3], "strict" => false]) { $o[] = count($o); return $o; }');
/// echo json_encode(settings()), " ", json_encode(settings()), "\n";
/// eval('function total(array $parts = [4, 5, 6]) { return array_sum($parts); }');
/// echo total(), "\n";
/// eval('function i($a = [1], $b = ["a" => 1], $c = [[]]) { return [$a, $b, $c]; }');
/// echo json_encode(i()), "\n";
/// eval('$f = fn ($v = [1]) => $v; echo json_encode($f()), "\n";');
/// ```
/// ```expect regime=jit-off
/// {"depth":[2,3],"strict":false,"0":2} {"depth":[2,3],"strict":false,"0":2}
/// 15
/// [[1],{"a":1},[[]]]
/// [1]
/// ```
fn folded_const_expr(folded: Zval) -> Option<ConstExpr> {
    if folded.as_arr().is_some() {
        return UnitValue::new(folded).map(ConstExpr::UnitValue);
    }
    Some(match Scalar::of(&folded)? {
        Scalar::Null => ConstExpr::Null,
        Scalar::Bool(b) => ConstExpr::Bool(b),
        Scalar::Long(n) => ConstExpr::long(n),
        Scalar::Double(d) => ConstExpr::Double(d),
        Scalar::Str(bytes) => ConstExpr::Str(bytes.into()),
    })
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

impl<'a> Emit<'a> {
    /// The walk's state before the first statement of `parsed`.
    #[allow(clippy::too_many_lines)] // the emitter laid down a field a line
    fn new(
        arena: &'a Arena,
        parsed: &'a Parsed<'a>,
        options: &'a Options<'a>,
        strings: &'a mut dyn Intern,
        halt: Option<i64>,
        room: crate::Room,
    ) -> Emit<'a> {
        let first_slot = options.op_array_extensions.saturating_mul(SLOT);
        let crate::Room {
            oplines,
            literals,
            filename,
        } = room;
        Emit {
            parsed,
            arena,
            strings,
            precision: options.precision,
            assertions: assert::Assertions::of(options.assertions),
            persistent: options.persistent,
            functions: options.functions,
            known_classes: options.classes,
            hooked: options.hooked,
            extended_info: options.extended_info,
            request_classes: options.request_classes,
            request_constants: options.request_constants,
            frameless: options.frameless,
            namespace: None,
            namespacing: Namespacing::Global,
            imports: Imports::default(),
            active_class: None,
            folded_string: None,
            parser_dots: std::collections::HashSet::new(),
            active_parent: None,
            active_function: None,
            own_names: Vec::new(),
            file_own: None,
            this_in_scope: false,
            return_type: None,
            returns_by_ref: false,
            depth: 0,
            active_property: None,
            active_hook: None,
            in_jmp_frameless_branch: false,
            statements_seen: 0,
            top_list: TopList::Declares,
            typing: Typing::of(options.strict_types),
            ticks: None,
            stack: options.stack,
            oplines,
            short_circuits: Vec::new(),
            chain_inner: None,
            memo: memo::Memo::default(),
            const_evaluation: Evaluation::Once,
            substitution: Substitution::Persistent,
            constants: literals,
            const_exprs: Vec::new(),
            vars: Vec::new(),
            http_response_header: ResponseHeader::Predefined,
            safepoints: entry_safepoints(),
            temporaries: 0,
            cache_slots: first_slot,
            line: 1,
            diagnostics: Vec::new(),
            classes: Vec::new(),
            bound: Vec::new(),
            top_level: Vec::new(),
            completed: Vec::new(),
            runtime_keys: Vec::new(),
            attributed_constants: Vec::new(),
            keys_before: options.keys_before,
            first_slot,
            functions_declared: Vec::new(),
            landed: Vec::new(),
            dynamic: Vec::new(),
            cell: php_unit::UnitCell::default(),
            function_callables: 0,
            dynamic_func_defs: Vec::new(),
            nested: Vec::new(),
            static_vars: Vec::new(),
            fold: Fold::Compile,
            rewrites: eval::Rewrites::default(),
            claims: eval::Claims::default(),
            fn_flags: FnFlags::default(),
            numeric_dims: Vec::new(),
            methods: Vec::new(),
            hooks: Vec::new(),
            filename: options.filename,
            room_filename: filename,
            cwd: options.cwd,
            loops: Vec::new(),
            current_loop: None,
            jumps: Vec::new(),
            labels: Vec::new(),
            try_catch: Vec::new(),
            unwind: Vec::new(),
            loop_vars: 0,
            loop_var_peak: 0,
            delayed: Rc::default(),
            fast_call_var: None,
            try_catch_offset: None,
            generators: walk::generators(&parsed.tree),
            arms: Vec::new(),
            halt,
            halted: None,
        }
    }
}

/// `zend_ast_export("", ast, "")` over a parsed file
/// (`Z/Zend/zend_ast.c:2954`): php's text of each top statement at no
/// indent, through the exporter `assert()`'s description is written by.
pub(crate) fn export(
    arena: &Arena,
    parsed: &Parsed<'_>,
    options: &Options<'_>,
    strings: &mut dyn Intern,
) -> Result<Vec<u8>, CompileError> {
    let mut emit = Emit::new(
        arena,
        parsed,
        options,
        strings,
        None,
        crate::Room::default(),
    );
    let mut out = Vec::new();
    let file = parsed
        .tree
        .root()
        .and_then(php_syntax::tree::Element::id::<php_syntax::tree::SourceFile>);
    if let Some(file) = file {
        emit.export_stmts(&mut out, parsed.tree.list(parsed.tree[file].stmts), 0)?;
    }
    Ok(out)
}

/// The unit of one source file: its main op array, the statements and
/// then php's implicit final `RETURN 1` (`zend_emit_final_return`, with
/// `extended_value` at -1), and the diagnostics raised on the way, after
/// `raised`, what was raised before the compile began. Its names are
/// interned before it is made ([`crate::intern_names`]), so the unit is
/// made once, where the caller takes it, and not moved on the way out.
#[allow(clippy::too_many_lines)] // every way a compile ends, each a few lines
pub(crate) fn unit(
    arena: &Arena,
    parsed: &Parsed<'_>,
    options: &Options<'_>,
    strings: &mut dyn Intern,
    halt: Option<i64>,
    room: crate::Room,
    mut raised: Vec<Diagnostic>,
) -> Result<Unit, CompileError> {
    // The scanner stops at `__halt_compiler();` (the byte `halt_end`
    // names), so a halted file's last line is that statement's, and the
    // data after it counts no line.
    // Read off the parse's line table: the lines up to that byte, or
    // the whole source's where there is none.
    let lines = match walk::halt_offset(parsed) {
        Some(end) => parsed
            .lines
            .line_number(u32::try_from(end).unwrap_or(u32::MAX)),
        None => parsed.lines.line_count(),
    };
    let mut emit = Emit::new(arena, parsed, options, strings, halt, room);
    // A fatal leaves the file with what `zend_error` had raised before
    // it already rendered, so those travel with it.
    if let Err(mut refused) = emit.source_file() {
        refused.raised = emit.diagnostics;
        // What php's compile had added to `CG(function_table)` and
        // `CG(class_table)` before it bailed out, and the keys it had
        // counted in `CG(rtd_key_counter)`, which nothing takes back:
        // every body the tables reach, and the closures they declare, a
        // slot the refused statement took and never filled ending them.
        // A refusal php throws is the parser's, made before `zend_compile`
        // declared or counted anything, as is a parse error.
        let bailed = refused.bailed();
        if bailed {
            refused.keys_made = u32::try_from(emit.runtime_keys.len()).unwrap_or(u32::MAX);
        }
        if bailed && !emit.landed.is_empty() {
            let (top_level, raised_before) = emit.landed.into_iter().unzip();
            let dynamic = emit.dynamic.into_iter().map_while(|def| def).collect();
            refused.declared = Some(Box::new(crate::Declared {
                unit: Unit {
                    functions: emit.functions_declared,
                    classes: emit.classes,
                    runtime_keys: emit.runtime_keys,
                    top_level,
                    methods: emit.methods,
                    hooks: emit.hooks,
                    completed: emit.completed,
                    dynamic,
                    cell: emit.cell,
                    ..Unit::default()
                },
                raised_before,
            }));
        }
        return Err(crate::refused_after(
            refused,
            raised,
            options,
            &emit.own_names,
            emit.strings,
        ));
    }
    // `CG(zend_lineno) = last_lineno` before `zend_emit_final_return`
    // (`Z/Zend/zend_language_scanner.l:622`): the implicit return stands
    // on the file's last line, where the scanner stopped, and not on
    // the last statement's.
    emit.line = lines.max(1);
    if let Err(refused) = emit.close(options.kind == SourceKind::File) {
        return Err(crate::refused_after(
            refused,
            raised,
            options,
            &emit.own_names,
            emit.strings,
        ));
    }
    let line = emit.line;
    let mut dynamic = Vec::with_capacity(emit.dynamic.len());
    for def in std::mem::take(&mut emit.dynamic) {
        let Some(def) = def else {
            let refused = Refused {
                level: Level::Error,
                message: String::from(
                    "InternalError: a dynamic function definition whose body never landed",
                )
                .into(),
                line,
                raised: std::mem::take(&mut emit.diagnostics),
                raise: crate::Raise::Fatal,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            }
            .into();
            return Err(crate::refused_after(
                refused,
                raised,
                options,
                &emit.own_names,
                emit.strings,
            ));
        };
        dynamic.push(def);
    }
    // The scanner's diagnostics stand before the compiler's.
    let diagnostics = if raised.is_empty() {
        std::mem::take(&mut emit.diagnostics)
    } else {
        raised.append(&mut emit.diagnostics);
        raised
    };
    let filename = emit.file_name();
    let parts = crate::Parts {
        file: &filename,
        main_vars: &emit.vars,
        functions: &emit.functions_declared,
        dynamic: &dynamic,
        methods: &emit.methods,
        hooks: &emit.hooks,
        classes: &emit.classes,
    };
    // `zend_mm`'s limit, raised as `E_ERROR` where the compile asked for
    // the memory.
    if let Err(limit) = parts.intern(
        crate::Making::of(options.kind),
        &emit.own_names,
        emit.strings,
    ) {
        let (raise, message) = crate::fatal_refusal(limit);
        return Err(Refused {
            level: Level::Error,
            message: message.into(),
            line: lines.max(1),
            raised: diagnostics,
            raise,
            stage: crate::Stage::Compiling,
            declared: None,
            keys_made: u32::try_from(emit.runtime_keys.len()).unwrap_or(u32::MAX),
        }
        .into());
    }
    // Made where the caller takes it: the main op array is laid in the
    // unit as the unit is.
    let main = emit.op_array(filename, 1, lines.max(1));
    let delayed_fetches = emit.delayed.most.get();
    Ok(Unit {
        main,
        functions: emit.functions_declared,
        dynamic,
        methods: emit.methods,
        hooks: emit.hooks,
        completed: emit.completed,
        diagnostics,
        classes: emit.classes,
        arms: emit.arms,
        runtime_keys: emit.runtime_keys,
        top_level: emit.top_level,
        constants: emit.attributed_constants,
        halt_offset: emit.halted,
        loop_vars: emit.loop_var_peak,
        delayed_fetches,
        seen_symbols: emit.imports.seen_peak(),
        cell: emit.cell,
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
    const_exprs: Vec<ConstExpr>,
    vars: Vec<Name>,
    http_response_header: ResponseHeader,
    safepoints: Safepoints,
    temporaries: u32,
    cache_slots: u32,
    loops: Vec<Loop>,
    current_loop: Option<usize>,
    jumps: Vec<Pending>,
    labels: Vec<(Vec<u8>, Label)>,
    try_catch: Vec<TryCatch>,
    unwind: Vec<Unwind>,
    fast_call_var: Option<u32>,
    try_catch_offset: Option<u32>,
    active_function: Option<ActiveFunction>,
    this_in_scope: bool,
    return_type: Option<Type>,
    returns_by_ref: bool,
    dynamic_func_defs: Vec<u32>,
    nested: Vec<Nested>,
    static_vars: Vec<BoundVar>,
    fn_flags: FnFlags,
    numeric_dims: Vec<ConstIdx>,
    active_property: Option<Vec<u8>>,
    active_hook: Option<HookKind>,
}

/// An op array's safepoints as it begins: the entry, number zero, inline
/// with room for its returns ([`Safepoints`]).
fn entry_safepoints() -> Safepoints {
    let mut safepoints = Safepoints::new();
    safepoints.push(OplineIdx(0));
    safepoints
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

/// `pass_two`'s `ZEND_ASSERT_CHECK` arm (`Z/Zend/zend_opcode.c:1190`):
/// what the check leaves is unused where what the call returns is, the
/// call being the opline before the check's jump target, or the one
/// before that where the target follows an `EXT_FCALL_END`, which
/// [`Emit::do_free`] marked `UNUSED` where nothing reads what it returns.
fn unused_assert_checks(oplines: &mut [Opline]) {
    for at in 0..oplines.len() {
        let Some(check) = oplines
            .get(at)
            .filter(|op| op.opcode == Opcode::AssertCheck)
        else {
            continue;
        };
        let call = check.op2.checked_sub(1).and_then(|call| {
            let found = oplines.get(call as usize)?;
            if found.opcode == Opcode::ExtFcallEnd {
                oplines.get(call.checked_sub(1)? as usize)
            } else {
                Some(found)
            }
        });
        let unused = call.is_some_and(|call| call.out_type == OperandKind::Unused);
        if unused && let Some(check) = oplines.get_mut(at) {
            check.out_type = OperandKind::Unused;
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
        let refused = |what: &str| {
            Refused {
                level: Level::Error,
                message: format!(
                    "InternalError: {} at opline {at} jumps back to {:04} {what}",
                    op.opcode.name().trim_start_matches("ZEND_"),
                    target.0
                )
                .into(),
                line: op.lineno,
                raised: Vec::new(),
                raise: crate::Raise::Fatal,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            }
            .into()
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

impl<'a> Emit<'a> {
    fn push(&mut self, opline: Opline) -> u32 {
        let at = self.next();
        self.oplines.push(Opline {
            lineno: self.line,
            ..opline
        });
        at
    }

    /// `zend_add_literal` (`Z/Zend/zend_compile.c:588`): a slot of its
    /// own for `value`, appended whether or not the table holds the value
    /// already. php never shares a slot at compile time (opcache's
    /// `compact_literals` is an optimizer pass, after the dump), and
    /// phpdbg's `info literal` prints the table as it stands. A slot
    /// reserved and laid later, a jump table's ([`Self::lay_table`]) or a
    /// `goto`'s label ([`Self::resolve_goto`]), and a name's constant
    /// pair, read by its adjacency, each rely on no other operand ever
    /// being handed its slot.
    fn literal(&mut self, value: Zval) -> u32 {
        let at = u32::try_from(self.constants.len()).unwrap_or(u32::MAX);
        self.constants.push(value);
        at
    }

    /// `zend_ast_create_zval` behind an `IS_CONSTANT_AST` zval: the
    /// expression into the op array's own table, and the zval that
    /// indexes it, which a slot of the constant table then holds like
    /// any other value.
    fn unevaluated(&mut self, expr: ConstExpr) -> Zval {
        let at = u32::try_from(self.const_exprs.len()).unwrap_or(u32::MAX);
        self.const_exprs.push(expr);
        Zval::const_expr(at)
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

    /// `zend_try_compile_cv`'s arm over `$http_response_header`, which
    /// php 8.5 deprecates for the variable the http wrapper leaves
    /// behind: until the op array has written it, a read under
    /// `BP_VAR_R` is the deprecation, raised again at each one, and a
    /// write under `BP_VAR_W` makes the name the op array's own.
    ///
    /// ```php
    /// function f() { echo @$http_response_header; }
    /// function g() { $http_response_header = 1; function h() { var_dump($http_response_header); } return $http_response_header; }
    /// function k($http_response_header) { return $http_response_header; }
    /// echo "done\n";
    /// ```
    /// ```expect
    /// Deprecated: The predefined locally scoped $http_response_header variable is deprecated, call http_get_last_response_headers() instead in %s on line 2
    ///
    /// Deprecated: The predefined locally scoped $http_response_header variable is deprecated, call http_get_last_response_headers() instead in %s on line 3
    /// done
    /// ```
    fn cv_fetched(&mut self, name: &[u8], fetch: Fetch) {
        if self.http_response_header == ResponseHeader::Own || name != HTTP_RESPONSE_HEADER {
            return;
        }
        match fetch {
            Fetch::Read => self.raise(Level::Deprecated, HTTP_RESPONSE_HEADER_DEPRECATED),
            Fetch::Write => self.http_response_header = ResponseHeader::Own,
            Fetch::Rw | Fetch::Is | Fetch::FuncArg | Fetch::Unset => {}
        }
    }

    /// [`Self::cv_fetched`] over the CV numbered `cv`.
    fn cv_numbered(&mut self, cv: u32, fetch: Fetch) {
        // The one name `cv_fetched` reads anything of, so the CV's own is
        // not copied out to be compared.
        let header = self
            .vars
            .get(cv as usize)
            .is_some_and(|name| &*name.0 == HTTP_RESPONSE_HEADER);
        if header {
            self.cv_fetched(HTTP_RESPONSE_HEADER, fetch);
        }
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

    /// What the heap refused the compile, at the line the compiler
    /// stands on: `zend_mm`'s limit, which php raises as `E_ERROR` where
    /// the compile asked for the memory. So it bails out of the compile
    /// as a compile error does, past what the compile had declared, no
    /// `try` around an `eval` catches it, and no fold passes over it
    /// ([`Self::constant_of`] holds all three).
    fn fatal_of(&self, fatal: Fatal) -> CompileError {
        let (raise, message) = crate::fatal_refusal(fatal);
        let mut refused = self.fatal(message);
        refused.level = Level::Error;
        refused.raise = raise;
        refused
    }

    /// The compile-time fatal for what stands on the statement's line,
    /// in this compiler's own sentence at `Level::Parse`, the parse error
    /// #163 framed it as, which no fold passes over. A construct this
    /// compiler cannot read yet is [`Self::unread`], which a fold leaves
    /// unfolded.
    fn fatal(&self, message: impl Into<Cow<'static, str>>) -> CompileError {
        Refused {
            level: Level::Parse,
            message: message.into(),
            line: self.line,
            raised: Vec::new(),
            raise: crate::Raise::Fatal,
            stage: crate::Stage::Compiling,
            declared: None,
            keys_made: 0,
        }
        .into()
    }

    /// `zend_check_stack_limit`, at the top of `zend_compile_expr` and
    /// `zend_eval_const_expr`: php's `E_COMPILE_ERROR` once the frame
    /// stands past the thread's limit. The refusal is a cold call of its
    /// own, so the sentence it formats is off the stack of every level
    /// of a nested expression (#3159).
    fn check_stack(&self) -> Result<(), CompileError> {
        if self.stack.overflowed() {
            return self.stack_refused();
        }
        Ok(())
    }

    /// [`Self::check_stack`]'s refusal.
    #[cold]
    #[inline(never)]
    fn stack_refused(&self) -> Result<(), CompileError> {
        self.stack.check(self.line)
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

    /// `zend_is_true` as the compile calls it over a constant: a NAN is
    /// true, and the warning `i_zend_is_true` raises over it is the
    /// compilation's ([`Self::raise`]).
    pub(crate) fn is_true(&mut self, truth: Truth) -> bool {
        match truth {
            Truth::Nan => {
                self.raise(Level::Warning, "unexpected NAN value was coerced to bool");
                true
            }
            Truth::Is(held) => held,
        }
    }

    /// The constant string for these bytes, interned into whichever home
    /// this compilation was given: php's `zend_new_interned_string` over
    /// the string a `CONST` operand holds. A home with no memory left
    /// refuses, and the refusal is the compile-time fatal php raises
    /// where it runs out (`Z/Zend/zend_alloc.c:1059`).
    pub(crate) fn string(&mut self, bytes: &[u8]) -> Result<Zval, CompileError> {
        match self.strings.intern(bytes) {
            Ok(s) => Ok(Zval::string(s)),
            Err(fatal) => Err(self.fatal_of(fatal)),
        }
    }

    /// [`Self::string`] interned whatever the fold asks
    /// ([`Intern::interning`]): a string php's compile copies out of one
    /// it holds interned, a persistent constant's value and
    /// `CG(compiled_filename)`, which `zend_set_compiled_filename` interns.
    fn interned_string(&mut self, bytes: &[u8]) -> Result<Zval, CompileError> {
        self.interning(true, |this| this.string(bytes))
    }

    /// [`Self::string`] of a declaration's own name or lowered key, one
    /// string this compile makes for it, which php copies where the
    /// compile asks for it again and which the declaration owns once the
    /// unit is whole ([`Self::own_names`]), laid at `sizing`'s size: a
    /// name php prints with `zend_strpprintf` at `zend_string_finit`'s
    /// ([`crate::Named::sizing`]), every other at `zend_string_init`'s.
    fn own_string(&mut self, bytes: &[u8], sizing: Sizing) -> Result<Zval, CompileError> {
        match self.strings.intern_made(bytes, false, sizing) {
            Ok(made) => Ok(made.value()),
            Err(fatal) => Err(self.fatal_of(fatal)),
        }
    }

    /// `zend_string_tolower(ce->name)`, the key `DECLARE_CLASS` and
    /// `DECLARE_ANON_CLASS` bind a class under: the class's own name
    /// (`own`, [`ActiveClass::own`]) held once more where lowering
    /// changes nothing, as `zend_string_tolower` hands back the string it
    /// was given, else a key of its own.
    fn class_key(
        &mut self,
        at: usize,
        own: Option<Str>,
        name: &[u8],
    ) -> Result<Zval, CompileError> {
        let lowered = self.arena.lowercase(name);
        if lowered == name
            && let Some(own) = own.map(Zval::string)
            && self.strings.hold(own)
        {
            return Ok(own);
        }
        let key = self.own_string(lowered, Sizing::Alloc)?;
        if let Some(key) = key.as_str() {
            self.own_names.push((crate::Named::ClassKey(at), key));
        }
        Ok(key)
    }

    /// `CG(active_class_entry)->name` as the claim of the one who asked:
    /// one more owner of the string the class's body began with
    /// ([`ActiveClass::own`]), so every `self::class`, `__CLASS__` and
    /// `__TRAIT__` of the class is that one string (`zend_string_copy`);
    /// the name made as asked before the body, in an attribute of the
    /// class, and the empty string outside any class. A function or a
    /// method of the class's name is a string of its own, as php's is
    /// (the file under the recipe's `TMPDIR`):
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3281-own-name.inc';
    /// file_put_contents($f, <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// echo m('class foo { function foo2() {} static function g() { return [__CLASS__, self::class]; } } function foo() { return __FUNCTION__; }'), "\n";
    /// echo m('class fx { function fx() { return __FUNCTION__ . __CLASS__; } }'), "\n";
    /// echo m('trait Tq { function t() { return __TRAIT__; } }'), "\n";
    /// echo json_encode([foo::g(), foo(), (new fx)->fx()]), "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 1000
    /// 608
    /// 608
    /// [["foo","foo"],"foo","fxfx"]
    /// ```
    fn class_name_made(&mut self) -> Result<Made, CompileError> {
        let (own, name) = match &self.active_class {
            Some(class) => (class.own, class.name.clone()),
            None => (None, Vec::new()),
        };
        if let Some(made) = own.and_then(|own| self.strings.hold_made(Zval::string(own))) {
            return Ok(made);
        }
        self.string_made(&name, false, Sizing::Alloc)
    }

    /// `CG(active_op_array)->function_name` as the claim of the one who
    /// asked: one more owner of the string made for the function's name
    /// the first time `__FUNCTION__`, `__METHOD__` or the key it is bound
    /// under asked ([`ActiveFunction::own`]), which the function owns, so
    /// each copy is that one string (`zend_string_copy`):
    ///
    /// ```php
    /// $put = function ($name, $src) { $f = sys_get_temp_dir() . "/$name"; file_put_contents("$f.tmp", $src); touch("$f.tmp", time() - 10); rename("$f.tmp", $f); return $f; };
    /// $f = $put('w3311-function-name.inc', <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// echo m('function fqwertyu() { return [__FUNCTION__, __FUNCTION__, __METHOD__]; }'), "\n";
    /// echo m('if (1) { function gqwertyu() { return __FUNCTION__ . __FUNCTION__; } }'), "\n";
    /// echo m('class Kqwertyu { function hqwertyu() { return [__FUNCTION__, __FUNCTION__]; } }'), "\n";
    /// $junk = array_fill(0, 1000, str_repeat("x", 100));
    /// echo json_encode(eval('return [fqwertyu(), gqwertyu(), (new Kqwertyu)->hqwertyu()];')), "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 480
    /// 312
    /// 880
    /// [["fqwertyu","fqwertyu","fqwertyu"],"gqwertyugqwertyu",["hqwertyu","hqwertyu"]]
    /// ```
    fn function_name_made(&mut self) -> Result<Made, CompileError> {
        let Some(function) = &self.active_function else {
            return self.string_made(b"", false, Sizing::Alloc);
        };
        if let Some(made) = (function.own).and_then(|own| self.strings.hold_made(Zval::string(own)))
        {
            return Ok(made);
        }
        let name = function.name.clone();
        let own = self.own_string(&name, function.at.sizing(&name))?;
        let made = self.strings.hold_made(own);
        if let Some(function) = &mut self.active_function {
            function.own = own.as_str();
        }
        made.map_or_else(|| self.string_made(&name, false, Sizing::Alloc), Ok)
    }

    /// `CG(compiled_filename)` as the claim of the one who asked: one
    /// more owner of the file's name, `zend_set_compiled_filename`'s
    /// `zend_new_interned_string`, made the first time `__FILE__` asked
    /// and owned by the unit, so each copy is that one string
    /// (`ZVAL_STR_COPY`), an `eval`'s and an uncached include's alike (the
    /// program under the recipe's `TMPDIR`, read after the heap is
    /// churned):
    ///
    /// ```php
    /// $put = function ($name, $src) { $f = sys_get_temp_dir() . "/$name"; file_put_contents("$f.tmp", $src); touch("$f.tmp", time() - 10); rename("$f.tmp", $f); return $f; };
    /// $f = $put('w3316-file-name.inc', <<<'PHP'
    /// <?php
    /// ini_set('opcache.file_update_protection', '2');
    /// function inc($name, $src) { $f = sys_get_temp_dir() . "/w3316-$name.inc"; file_put_contents($f, $src); $m = memory_get_usage(); include $f; $d = memory_get_usage() - $m; unlink($f); return $d; }
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// inc('w0', '<?php class W {} $q0 = 1;'); m('$q1 = 1;');
    /// echo inc('fn', '<?php function fa2() { return __FILE__; }'), "\n";
    /// echo inc('main', '<?php $GLOBALS["ff"] = __FILE__;'), "\n";
    /// echo inc('class', '<?php class Ca5 { const F = __FILE__; function m() { return __FILE__; } }'), "\n";
    /// echo m('function fa3() { return __FILE__ . __FILE__; }'), "\n";
    /// $junk = array_fill(0, 1000, str_repeat("x", 100));
    /// echo strlen(fa2()), " ", strlen($ff), " ", strlen(Ca5::F), " ", strlen((new Ca5)->m()), " ", strlen(fa3()) > 0 ? "y" : "n", "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 224
    /// 608
    /// 848
    /// 448
    /// 54 56 57 57 y
    /// ```
    fn file_name_made(&mut self) -> Result<Made, CompileError> {
        if let Some(made) =
            (self.file_own).and_then(|own| self.strings.hold_made(Zval::string(own)))
        {
            return Ok(made);
        }
        let filename = self.filename;
        let own = self.interned_string(filename)?;
        let made = self.strings.hold_made(own);
        if let Some(own) = own.as_str() {
            self.file_own = Some(own);
            self.own_names.push((crate::Named::File, own));
        }
        made.map_or_else(|| self.string_made(filename, false, Sizing::Alloc), Ok)
    }

    /// [`Self::string`] as a claim the one who asked holds alone
    /// ([`Intern::intern_made`]), of bytes known to be valid UTF-8 where
    /// `utf8` says so ([`Intern::intern_marked`]): a number's text, a fold
    /// of two marked strings; laid at `sizing`'s size, the parser's fold
    /// at `zend_string_finit`'s ([`Self::concat_made`]) and every other
    /// at `zend_string_init`'s.
    fn string_made(
        &mut self,
        bytes: &[u8],
        utf8: bool,
        sizing: Sizing,
    ) -> Result<Made, CompileError> {
        self.strings
            .intern_made(bytes, utf8, sizing)
            .map_err(|fatal| self.fatal_of(fatal))
    }

    /// [`Self::constant_expr`] asked for a fold: the value where the
    /// expression is a constant one, nothing where it is not or where
    /// this compiler cannot read it yet, which the compile then meets as
    /// it stands, and every other refusal through, since php's compiler
    /// leaves the file at the fatal and folds nothing after it.
    ///
    /// So the string a class constant's fold makes past the memory limit
    /// is php 8.5.12's delayed OOM, recorded at the line of the `eval` the
    /// fold ran under and raised as the request's `E_ERROR` at the next
    /// safepoint, and not a node left to the class to evaluate. The string
    /// is served, so the compile ends: the function the `eval` declared
    /// before it stands and so does the class, its constant whole. A unit
    /// of declarations alone answers with no frame, so the next safepoint
    /// is the main unit's return, past the `echo`. The constant is folded
    /// from a smaller one, so the fold, and not the source or its scan, is
    /// what outgrows the limit:
    ///
    /// ```php
    /// ini_set('display_errors', '0');
    /// register_shutdown_function(function () {
    ///     $e = error_get_last();
    ///     echo $e['type'], ' ', strtok($e['message'], '('), 'on line ', $e['line'], "\n";
    ///     var_dump(function_exists('f'), class_exists('C', false), strlen(C::X));
    /// });
    /// $a = str_repeat('a', 1000000);
    /// $x = implode(' . ', array_fill(0, 16, 'self::A'));
    /// ini_set('memory_limit', '20M');
    /// eval("function f() {}\nclass C {\n    const A = '$a';\n    const X = $x;\n}");
    /// echo "reached\n";
    /// ```
    /// ```expect exit=255
    /// reached
    /// 1 Allowed memory size of 20971520 bytes exhausted on line 11
    /// bool(true)
    /// bool(true)
    /// int(16000000)
    /// ```
    ///
    /// What the limit holds is what php's fold holds at once, the string
    /// it makes beside the operands it reads, each operand freed once the
    /// node over it stands ([`eval::Claims`]): never the sum of the
    /// strings the chain made on the way. So sixteen copies of a megabyte
    /// fold under `memory_limit=40M`, and not under 32M, with opcache and
    /// without it. Under 16M the fold would pass php 8.5.12's hard limit,
    /// twice the soft one, which ends the process with no shutdown, so the
    /// lowest limit asked is 20M:
    ///
    /// ```php
    /// $child = <<<'PHP'
    /// <?php
    /// ini_set('display_errors', '0');
    /// register_shutdown_function(function () {
    ///     $e = error_get_last();
    ///     echo $e ? strtok($e['message'], '(') . 'on line ' . $e['line'] : strlen(C::X) . ' bytes', "\n";
    /// });
    /// $a = str_repeat('a', 1000000);
    /// $x = implode(' . ', array_fill(0, 16, 'self::A'));
    /// ini_set('memory_limit', $argv[1]);
    /// eval("class C {\n    const A = '$a';\n    const X = $x;\n}");
    /// PHP;
    /// $f = sys_get_temp_dir() . '/fold-' . getmypid() . '.php';
    /// file_put_contents($f, $child);
    /// foreach (['20M', '24M', '32M', '40M', '128M'] as $limit) {
    ///     foreach (['-n', '-n -d opcache.enable_cli=1 -d opcache.jit=off'] as $regime) {
    ///         echo $limit, ' ', shell_exec(escapeshellarg(PHP_BINARY) . " $regime " . escapeshellarg($f) . " $limit");
    ///     }
    /// }
    /// unlink($f);
    /// ```
    /// ```expect process
    /// 20M Allowed memory size of 20971520 bytes exhausted on line 10
    /// 20M Allowed memory size of 20971520 bytes exhausted on line 10
    /// 24M Allowed memory size of 25165824 bytes exhausted on line 10
    /// 24M Allowed memory size of 25165824 bytes exhausted on line 10
    /// 32M Allowed memory size of 33554432 bytes exhausted on line 10
    /// 32M Allowed memory size of 33554432 bytes exhausted on line 10
    /// 40M 16000000 bytes
    /// 40M 16000000 bytes
    /// 128M 16000000 bytes
    /// 128M 16000000 bytes
    /// ```
    pub(crate) fn constant_of(&mut self, expr: tree::Expr) -> Result<Option<Zval>, CompileError> {
        match self.constant_expr(expr) {
            Ok(value) => Ok(Some(value)),
            Err(Unfolded::NoConstant(_) | Unfolded::Unread(_)) => Ok(None),
            Err(Unfolded::Refused(refused)) => Err(refused),
        }
    }

    /// `zend_eval_const_expr` over one node, what an array's elements, a
    /// declaration's constant expression and a `static`'s initializer are
    /// folded by: [`Self::constant_of`] under [`Fold::EvalConstExpr`],
    /// which folds a conditional, a `??` and a dimension over constants
    /// and no call.
    ///
    /// ```php
    /// $b = 5;
    /// var_dump([1 ? 2 : 3, $b], [0 ?: "d"], [null ?? 'n', ['k' => 1]['k']]);
    /// class C { const X = 1 ? 2 : 3; }
    /// var_dump(C::X);
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   int(2)
    ///   [1]=>
    ///   int(5)
    /// }
    /// array(1) {
    ///   [0]=>
    ///   string(1) "d"
    /// }
    /// array(2) {
    ///   [0]=>
    ///   string(1) "n"
    ///   [1]=>
    ///   int(1)
    /// }
    /// int(2)
    /// ```
    pub(crate) fn eval_const(&mut self, expr: tree::Expr) -> Result<Option<Zval>, CompileError> {
        let outer = std::mem::replace(&mut self.fold, Fold::EvalConstExpr);
        let folded = self.constant_of(expr);
        self.fold = outer;
        folded
    }

    /// `zend_compile_stmt` (`Z/Zend/zend_compile.c:11686`) over one
    /// statement node of the tree (design §4.9): the empty statement
    /// is nothing, as php's `statement: ';'` reduces to no node; a
    /// `namespace` and a `declare` are read before the statement is
    /// counted, since `zend_is_first_statement` steps over them; and
    /// what the tree parses and this compiler cannot emit yet is refused
    /// at the node's line in the one-line `cannot read yet` shape the
    /// phpt lane counts as unread.
    #[allow(clippy::too_many_lines)]
    pub(super) fn statement(&mut self, stmt: Stmt) -> Result<(), CompileError> {
        let tree = &self.parsed.tree;
        match stmt {
            Stmt::EmptyStmt(_) => return Ok(()),
            Stmt::NamespaceDef(id) => return self.namespace(id),
            Stmt::DeclareStmt(id) => return self.declare(id),
            _ => {}
        }
        self.statements_seen = self.statements_seen.saturating_add(1);
        match stmt {
            Stmt::EmptyStmt(_) | Stmt::NamespaceDef(_) | Stmt::DeclareStmt(_) => {}
            Stmt::ConstStmt(id) => self.const_decl(id)?,
            // `zend_compile_stmt`'s `ZEND_AST_ECHO` over the constant
            // string php's parser makes of the text outside the tags
            // (`T_INLINE_HTML`), which php echoes as it stands.
            Stmt::InlineHtmlStmt(id) => {
                let text = tree[id].body.map(|body| self.text(body).to_vec());
                let text = self.string(&text.unwrap_or_default())?;
                let at = self.literal(text);
                self.push(Opline {
                    opcode: Opcode::Echo,
                    op1_type: OperandKind::Const,
                    op1: at,
                    ..Opline::default()
                });
            }
            // php's parser makes `echo a, b;`, `unset($a, $b);`,
            // `global $a, $b;` and `static $a, $b;` a `ZEND_AST_STMT_LIST`
            // of one statement per element, each compiled by
            // `zend_compile_stmt` and so each ticked where a
            // `declare(ticks)` scope stands ([`Self::tick`]).
            Stmt::EchoStmt(id) => {
                for part in tree.list(tree[id].exprs) {
                    self.ext_stmt();
                    let value = self.expr(*part)?;
                    let (op1_type, op1) = self.operand(value);
                    self.push(Opline {
                        opcode: Opcode::Echo,
                        op1_type,
                        op1,
                        ..Opline::default()
                    });
                    self.tick();
                }
            }
            Stmt::ExprStmt(id) => self.expr_stmt(tree[id].expr)?,
            Stmt::Block(id) => self.block(id)?,
            Stmt::ForStmt(id) => self.r#for(id)?,
            Stmt::WhileStmt(id) => {
                let node = &tree[id];
                let cond = self.required(node.cond, "a `while` with no condition")?;
                self.r#while(cond, node.body, node.stmt_list)?;
            }
            Stmt::DoWhileStmt(id) => {
                let node = &tree[id];
                let cond = self.required(node.cond, "a `do` with no condition")?;
                self.do_while(node.body, cond)?;
            }
            Stmt::ForeachStmt(id) => self.foreach(id)?,
            Stmt::ReturnStmt(id) => self.r#return(tree[id].value)?,
            Stmt::SwitchStmt(id) => self.switch(id)?,
            Stmt::BreakStmt(id) => self.break_continue(tree[id].level, true)?,
            Stmt::ContinueStmt(id) => self.break_continue(tree[id].level, false)?,
            Stmt::GotoStmt(id) => {
                let Some(label) = tree[id].label else {
                    return Err(self.unread("a `goto` naming no label"));
                };
                let name = self.text(label).to_vec();
                self.goto(&name)?;
            }
            Stmt::LabelStmt(id) => {
                let name = self.text(tree[id].name).to_vec();
                self.label(&name)?;
            }
            Stmt::ClassDecl(_)
            | Stmt::InterfaceDecl(_)
            | Stmt::TraitDecl(_)
            | Stmt::EnumDecl(_) => {
                let class = self.walk_class_decl(stmt)?;
                self.class(&class)?;
            }
            Stmt::FunctionDecl(id) => {
                let function = self.walk_function_decl(id)?;
                self.function_decl(&function)?;
            }
            Stmt::TryStmt(id) => self.r#try(id)?,
            Stmt::UnsetStmt(id) => {
                for target in tree.list(tree[id].targets) {
                    self.ext_stmt();
                    self.unset(*target)?;
                    self.tick();
                }
            }
            Stmt::IfStmt(id) => self.r#if(id)?,
            Stmt::GlobalStmt(id) => {
                for var in tree.list(tree[id].vars).iter().copied() {
                    self.ext_stmt();
                    self.global_var(var)?;
                    self.tick();
                }
            }
            Stmt::StaticVarStmt(id) => self.static_var(id)?,
            Stmt::UseStmt(id) => self.use_stmt(id)?,
            // `inner_statement`'s `T_HALT_COMPILER` arm: the parser's
            // `CompileError`, a top statement being walked by
            // `top_statements`.
            Stmt::HaltCompilerStmt(_) => {
                return Err(walk::thrown(
                    self.line,
                    "__HALT_COMPILER() can only be used from the outermost scope",
                ));
            }
        }
        Ok(())
    }

    /// The refusal for a construct this compiler cannot emit yet, at
    /// the line the emitter stands on: [`crate::Raise::Unread`], whose
    /// sentence the phpt lane reads as unread ([`walk::unread`]).
    fn unread(&self, what: &str) -> CompileError {
        walk::unread(self.line, what)
    }

    /// A child the grammar requires, refused as `what` where recovery
    /// left none.
    fn required(&self, expr: Option<tree::Expr>, what: &str) -> Result<tree::Expr, CompileError> {
        expr.ok_or_else(|| self.unread(what))
    }

    /// The node under any parentheses around it: php's grammar drops
    /// `'(' expr ')'` and hands the compiler the inner node, so what a
    /// compile function looks at to sort an operand by its kind is the
    /// inner node, and the parenthesised conditional's mark is the one
    /// thing the parentheses leave, on the node ([`Self::expr`]). A node
    /// `zend_eval_const_expr` replaced with one of its sides is that side
    /// ([`Self::replaced`]).
    pub(crate) fn peel(&self, expr: tree::Expr) -> tree::Expr {
        self.replaced(peel(self.parsed, expr))
    }

    /// The line php's node for `expr` carries, `zend_ast.lineno`: the
    /// node's own, under any parentheses around it.
    fn line_of(&self, expr: tree::Expr) -> u32 {
        self.peel(expr).line(self.parsed)
    }

    /// The line a token stands on, what php's node for it carries.
    pub(super) fn token_line(&self, id: TokenId) -> Option<u32> {
        self.parsed
            .tokens
            .get(id.0 as usize)
            .map(|token| self.parsed.lines.line_number(token.offset))
    }

    /// The items of one of the tree's lists, read for as long as the
    /// parse lives, so a walk over them may compile into the emitter.
    pub(crate) fn list<T: tree::Listed>(&self, list: tree::List<T>) -> &'a [T] {
        self.parsed.tree.list(list)
    }

    /// A token's text as the scanner handed it over, the token filter's
    /// where one stands ([`php_syntax::token_text`]).
    fn text(&self, id: TokenId) -> &'a [u8] {
        php_syntax::token_text(self.parsed, id)
    }

    /// A `<variable>` token's name, without its `$`.
    fn variable_name(&self, id: TokenId) -> &'a [u8] {
        self.text(id).strip_prefix(b"$").unwrap_or_default()
    }

    /// The doc comment a declaration took, by the token index the
    /// builder recorded.
    fn doc_comment(&self, id: Option<TokenId>) -> Option<Vec<u8>> {
        id.map(|id| self.text(id).to_vec())
    }

    /// The tree's kind of a token.
    pub(crate) fn kind(&self, token: TokenId) -> SyntaxKind {
        self.parsed
            .tokens
            .get(token.0 as usize)
            .map_or(SyntaxKind::ERROR, |token| token.kind)
    }

    /// `$name` as `zend_compile_simple_var` sorts it: `$this`, which
    /// `is_this_fetch` picks out and no CV holds; an auto-global, which
    /// `zend_is_auto_global` arms ([`Self::arm`]) and php fetches by its
    /// name; `$GLOBALS`, which php reads by `FETCH_GLOBALS`; and the CV
    /// `zend_try_compile_cv` makes of every other name.
    fn simple_var_of(
        &mut self,
        id: NodeId<tree::SimpleVarExpr>,
        line: u32,
    ) -> Result<SimpleVar<'a>, CompileError> {
        let Some(var) = self.parsed.tree[id].var else {
            return Err(walk::unread(line, "a variable"));
        };
        let name = self.variable_name(var);
        if name == b"this" {
            return Ok(SimpleVar::This);
        }
        if let Some(global) = AutoGlobal::of(name) {
            self.arm(global);
            return Ok(SimpleVar::Auto(global));
        }
        if name == b"GLOBALS" {
            return Ok(SimpleVar::Globals);
        }
        Ok(SimpleVar::Cv(name))
    }

    /// `zend_is_auto_global` over a name the compiler met: the unit arms
    /// the auto-global ([`Unit::arms`]), once, in the order it was met.
    fn arm(&mut self, global: AutoGlobal) {
        if !self.arms.contains(&global) {
            self.arms.push(global);
        }
    }

    /// Whether `expr` is `$this`, php's `is_this_fetch`.
    fn is_this(&self, expr: tree::Expr) -> bool {
        match self.peel(expr) {
            tree::Expr::SimpleVarExpr(id) => self.parsed.tree[id]
                .var
                .is_some_and(|var| self.variable_name(var) == b"this"),
            _ => false,
        }
    }

    /// The receiver of a property fetch or a method call, php's
    /// `zend_compile_prop`'s first step: `$this` inside a method is the
    /// `IS_UNUSED` operand, `$this` elsewhere a `FETCH_THIS` into a
    /// `VAR`, and anything else the value `zend_compile_expr` leaves.
    fn receiver_of(&mut self, object: tree::Expr) -> Result<(OperandKind, u32), CompileError> {
        if self.is_this(object) {
            return Ok(self.this_receiver());
        }
        let value = self.chain_link(object)?;
        Ok(self.operand(value))
    }

    /// `$this` as a receiver, the `is_this_fetch` arm of
    /// `zend_delayed_compile_prop` and `zend_compile_method_call`: the
    /// `IS_UNUSED` operand where `this_guaranteed_exists`, and a
    /// `FETCH_THIS` into a `VAR` where it may not.
    fn this_receiver(&mut self) -> (OperandKind, u32) {
        if self.this_in_scope {
            return (OperandKind::Unused, 0);
        }
        let this = self.fetch_this(OperandKind::Var);
        self.operand(this)
    }

    /// `zend_compile_assign`, `zend_compile_assign_ref` and
    /// `zend_compile_compound_assign` by the target's shape and the
    /// operator: a variable, an element, a property or a class's
    /// property.
    fn assignment(
        &mut self,
        id: NodeId<tree::AssignExpr>,
        line: u32,
    ) -> Result<Where, CompileError> {
        let node = &self.parsed.tree[id];
        let (target, op, byref) = (self.peel(node.lhs), node.op, node.byref.is_some());
        let op = op.map(|op| self.kind(op));
        let Some(value) = node.rhs else {
            return Err(self.unread("an assignment"));
        };
        if byref {
            if op != Some(SyntaxKind::EQ) {
                return Err(self.unread("a by-reference compound assignment"));
            }
            return self.assign_ref(target, Source::Expr(value));
        }
        if op == Some(SyntaxKind::QUESTION_QUESTION_EQ) {
            return self.assign_coalesce(target, value);
        }
        if op != Some(SyntaxKind::EQ) {
            // `zend_ensure_writable_variable`, which every compound
            // operator runs first.
            self.ensure_writable(target)?;
        }
        if op == Some(SyntaxKind::EQ) {
            return self.assign_to(target, Source::Expr(value));
        }
        let Some(op) = op.and_then(compound_op) else {
            return Err(self.unread("an assignment operator"));
        };
        match target {
            // `$GLOBALS['k'] .= v` is a `ZEND_AST_VAR` to php, and so is
            // an auto-global.
            _ if self.is_by_name(target) => {
                self.assign_op_by_name(target, u32::from(Self::opcode_of(op) as u8), value)
            }
            tree::Expr::SimpleVarExpr(var) => match self.simple_var_of(var, line)? {
                SimpleVar::Cv(name) => self.assign_op(name, op, value),
                SimpleVar::This | SimpleVar::Auto(_) | SimpleVar::Globals => {
                    Err(self.unread("a compound assignment to what is no variable"))
                }
            },
            tree::Expr::SubscriptExpr(_) => self.assign_dim_op(target, op, value),
            tree::Expr::MemberAccessExpr(access) => {
                let name = self.member_of(access)?;
                let object = self.parsed.tree[access].obj;
                self.assign_obj_op(object, name, op, value)
            }
            tree::Expr::ScopedAccessExpr(scoped) => match self.scoped_of(scoped, line)? {
                Scoped::Property { class, name } => {
                    self.assign_static_prop_op(class, &name, op, value)
                }
                Scoped::Constant { .. } | Scoped::ClassName { .. } => {
                    Err(self.unread("a compound assignment to what is no variable"))
                }
            },
            _ => Err(self.unread("a compound assignment to what is no variable")),
        }
    }

    /// `++$a`, `$a--` and their kin over a variable, an element, a
    /// property or a class's property (`ZEND_AST_STATIC_PROP`), php's
    /// four step nodes; anything else is not read yet. What a call
    /// returned is php's compile-time fatal
    /// (`zend_ensure_writable_variable`), and `$this` is `FETCH_THIS`
    /// and the step over it, the `TypeError` for an object:
    ///
    /// ```php
    /// class Foo {
    ///     function f() {
    ///         try { ++$this; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    ///         try { $this--; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    ///     }
    /// }
    /// (new Foo)->f();
    /// ```
    /// ```expect
    /// Cannot increment Foo
    /// Cannot decrement Foo
    /// ```
    ///
    /// ```php
    /// function test() { return 42; }
    /// ++test();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Can't use function return value in write context in %s on line 3
    /// ```
    fn step(&mut self, target: tree::Expr, up: bool, pre: bool) -> Result<Where, CompileError> {
        let line = self.line_of(target);
        self.ensure_writable(target)?;
        let opcode = match (pre, up) {
            (true, true) => Opcode::PreInc,
            (true, false) => Opcode::PreDec,
            (false, true) => Opcode::PostInc,
            (false, false) => Opcode::PostDec,
        };
        match self.peel(target) {
            // `zend_ensure_writable_variable` (`Z/Zend/zend_compile.c:3388`):
            // what a call returned is no place to step.
            tree::Expr::CallExpr(id) => Err(self.call_written(id)),
            tree::Expr::SimpleVarExpr(var) => match self.simple_var_of(var, line)? {
                SimpleVar::Cv(name) => Ok(self.incdec(name, opcode)),
                // `zend_compile_var` over `$this` under `BP_VAR_RW`:
                // `FETCH_THIS` into a `VAR` and the step over it, which
                // is the `TypeError` for an object as the program serves.
                SimpleVar::This => {
                    let this = self.fetch_this(OperandKind::Var);
                    let (op1_type, op1) = self.operand(this);
                    let out = self.temporary();
                    self.push(Opline {
                        opcode,
                        op1_type,
                        op1,
                        out_type: OperandKind::Tmp,
                        out,
                        ..Opline::default()
                    });
                    Ok(Where::Temporary(out))
                }
                // An auto-global: `FETCH_RW (global)` and the step.
                SimpleVar::Auto(_) => self.step_by_name(target, opcode),
                SimpleVar::Globals => Err(self.fatal("`$GLOBALS` past its writable check")),
            },
            // `$GLOBALS['k']++`: `zend_compile_var` under `BP_VAR_RW` is
            // `FETCH_RW (global)` over the name, and the step.
            _ if self.is_by_name(target) => self.step_by_name(target, opcode),
            tree::Expr::ScopedAccessExpr(scoped) => match self.scoped_of(scoped, line)? {
                Scoped::Property { class, name } => {
                    let step = match (pre, up) {
                        (true, true) => Step::PreInc,
                        (true, false) => Step::PreDec,
                        (false, true) => Step::PostInc,
                        (false, false) => Step::PostDec,
                    };
                    self.static_prop_step(class, &name, step)
                }
                Scoped::Constant { .. } | Scoped::ClassName { .. } => Err(self.stepless(up)),
            },
            // `$$x++`: `FETCH_RW` by name and the step over it.
            tree::Expr::VarVarExpr(_) | tree::Expr::DynamicVarExpr(_) => {
                self.step_by_name(target, opcode)
            }
            tree::Expr::SubscriptExpr(_) => self.dim_step(target, up, pre),
            tree::Expr::MemberAccessExpr(access) => {
                let name = self.member_of(access)?;
                let object = self.parsed.tree[access].obj;
                self.prop_step(object, name, up, pre)
            }
            _ => Err(self.stepless(up)),
        }
    }

    /// `zend_compile_pre_incdec` and `zend_compile_post_incdec` over a
    /// `ZEND_AST_DIM` (`Z/Zend/zend_compile.c:10405`, `:10432`):
    /// `zend_compile_var` under `BP_VAR_RW`, every link of the chain a
    /// `FETCH_DIM_RW` into a `VAR`, the last marked
    /// `ZEND_FETCH_DIM_INCDEC`, which is the sentence a string offset
    /// under it is refused in; then `PRE_INC`, `PRE_DEC`, `POST_INC` or
    /// `POST_DEC` over that `VAR` into a temporary. `$a[]++` is php's
    /// append, the new element stepped from `null`.
    ///
    /// ```php
    /// $a = [1, 'k' => 5];
    /// $a[0]++;
    /// var_dump(++$a['k'], $a[0]--, $a);
    /// $b = [];
    /// $b[]++;
    /// $b['n']['m']++;
    /// var_dump($b);
    /// $s = "ab";
    /// try { $s[0]++; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(6)
    /// int(2)
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   ["k"]=>
    ///   int(6)
    /// }
    ///
    /// Warning: Undefined array key "n" in %s on line 7
    ///
    /// Warning: Undefined array key "m" in %s on line 7
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   ["n"]=>
    ///   array(1) {
    ///     ["m"]=>
    ///     int(1)
    ///   }
    /// }
    /// Cannot increment/decrement string offsets
    /// ```
    fn dim_step(&mut self, target: tree::Expr, up: bool, pre: bool) -> Result<Where, CompileError> {
        let (container, links) = self.dim_chain(target, Fetch::Rw)?;
        let Some((last, fetches)) = links.split_last() else {
            return Err(self.fatal("a dimension with no dimension"));
        };
        let (op1_type, op1) = self.dim_fetches(container, fetches, Fetch::Rw, Over::Dim);
        self.push(Opline {
            opcode: Opcode::FetchDimRw,
            op1_type,
            op1,
            op2_type: last.kind,
            op2: last.at,
            out_type: OperandKind::Var,
            out: last.out,
            extended_value: FETCH_DIM_INCDEC,
            ..Opline::default()
        });
        let out = self.temporary();
        self.push(Opline {
            opcode: match (pre, up) {
                (true, true) => Opcode::PreInc,
                (true, false) => Opcode::PreDec,
                (false, true) => Opcode::PostInc,
                (false, false) => Opcode::PostDec,
            },
            op1_type: OperandKind::Var,
            op1: last.out,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_pre_incdec` and `zend_compile_post_incdec` over a
    /// `ZEND_AST_PROP`: `zend_compile_prop` under `BP_VAR_RW`, whose last
    /// opline has its opcode set to `PRE_INC_OBJ`, `PRE_DEC_OBJ`,
    /// `POST_INC_OBJ` or `POST_DEC_OBJ` and its `result` made a
    /// temporary, the receiver in `op1`, the name in `op2` and three
    /// cache slots in `extended_value`; a property under it is
    /// `FETCH_OBJ_RW` and a dimension `FETCH_DIM_RW`, held back as
    /// [`Self::prop_chain`] holds them.
    fn prop_step(
        &mut self,
        object: tree::Expr,
        name: Member,
        up: bool,
        pre: bool,
    ) -> Result<Where, CompileError> {
        let (receiver, links) = self.prop_chain(object, Fetch::Rw)?;
        let name = self.prop_name(name)?;
        let out = self.temporary();
        // The step is the chain's last held-back `FETCH_OBJ_RW`.
        let held = self.held_back();
        let (op1_type, op1) = self.prop_fetches(receiver, &links, Fetch::Rw);
        drop(held);
        self.push(Opline {
            opcode: match (pre, up) {
                (true, true) => Opcode::PreIncObj,
                (true, false) => Opcode::PreDecObj,
                (false, true) => Opcode::PostIncObj,
                (false, false) => Opcode::PostDecObj,
            },
            op1_type,
            op1,
            op2_type: name.kind,
            op2: name.at,
            out_type: OperandKind::Tmp,
            out,
            extended_value: name.slots,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// The refusal for a `++` or a `--` over what is no variable.
    fn stepless(&self, up: bool) -> CompileError {
        self.unread(if up {
            "a `++` over what is no variable"
        } else {
            "a `--` over what is no variable"
        })
    }

    /// A call by the callee's shape, php's four call nodes: a named
    /// function (`ZEND_AST_CALL` over a `ZEND_AST_ZVAL`), a class's
    /// method (`ZEND_AST_STATIC_CALL`), an object's method
    /// (`ZEND_AST_METHOD_CALL`), or any other value, a callable the
    /// program holds (`ZEND_AST_CALL` over an expression). The callee is
    /// read as written, since php's `callable_expr: '(' expr ')'` makes
    /// a parenthesised one a value: `($o->p)()` calls what the property
    /// holds where `$o->p()` calls the method. A string written out, or
    /// folded by php's parser, is a name, fully qualified as
    /// `zend_resolve_function_name` reads a `ZEND_AST_ZVAL`.
    ///
    /// ```php
    /// class C {
    ///     public $p;
    ///     function __construct() { $this->p = fn ($x) => "prop $x"; }
    ///     function p($x) { return "method $x"; }
    ///     static function s($x) { return "static $x"; }
    /// }
    /// $o = new C;
    /// echo $o->p(1), ' | ', ($o->p)(2), "\n";
    /// echo 'strtoupper'('a'), ('str' . 'rev')('bc'), 'C::s'(3), "\n";
    /// echo (function ($x) { return "iife $x"; })(4), "\n";
    /// $f = [$o, 'p'];
    /// echo $f(5), ' | ', [new C, 'p'](6), ' | ', [C::class, 's'](7), "\n";
    /// $g = 'C::s';
    /// echo $g(8), (fn () => $g)()(9), "\n";
    /// ```
    /// ```expect
    /// method 1 | prop 2
    /// Acbstatic 3
    /// iife 4
    /// method 5 | method 6 | static 7
    /// static 8static 9
    /// ```
    fn call_of(
        &mut self,
        id: NodeId<tree::CallExpr>,
        line: u32,
        fetch: Fetch,
    ) -> Result<Where, CompileError> {
        let node = &self.parsed.tree[id];
        let (callee, args) = (node.callee, node.args);
        match callee {
            tree::Expr::NameRef(name) => {
                let name = self.text(name);
                let arguments = self.arguments(args, line)?;
                self.call(name, &arguments, fetch)
            }
            tree::Expr::ScopedAccessExpr(scoped) => {
                if let Some(called) = self.parent_hook_call(scoped, args, line)? {
                    return Ok(called);
                }
                let class = ClassSpec::of(self.parsed.tree[scoped].scope);
                let method = self.scoped_method_of(scoped)?;
                let arguments = self.arguments(args, line)?;
                self.static_call(class, method, &arguments)
            }
            tree::Expr::MemberAccessExpr(access) => {
                let method = self.member_of(access)?;
                let object = self.parsed.tree[access].obj;
                let nullsafe = self.is_nullsafe(access);
                let arguments = self.arguments(args, line)?;
                self.method_call(object, method, &arguments, nullsafe)
            }
            _ => {
                let arguments = self.arguments(args, line)?;
                // `zend_compile_function_name` over a `ZEND_AST_ZVAL`,
                // whose `attr` is `ZEND_NAME_FQ`: the string is the name,
                // a leading `\` dropped.
                if let Some(written) = self.written(callee)?
                    && let Some(name) = written.as_str()
                {
                    let name = name.bytes();
                    let mut qualified = Vec::with_capacity(name.len() + 1);
                    if name.first() != Some(&b'\\') {
                        qualified.push(b'\\');
                    }
                    qualified.extend_from_slice(name);
                    return self.call(&qualified, &arguments, fetch);
                }
                self.call_var(Source::Expr(callee), &arguments)
            }
        }
    }

    /// `zend_try_ct_eval_const`'s three special names: `true`, `false`
    /// and `null`, whatever their case and with or without the leading
    /// `\` `zend_resolve_const_name` strips, which php's compiler
    /// substitutes where they are written and no program may
    /// redeclare; every other name is a constant of its own.
    fn constant_name(&self, token: TokenId) -> Option<Zval> {
        let text = self.text(token);
        let name = text.strip_prefix(b"\\").unwrap_or(text);
        match name.to_ascii_lowercase().as_slice() {
            b"true" => Some(Zval::bool(true)),
            b"false" => Some(Zval::bool(false)),
            b"null" => Some(Zval::null()),
            _ => None,
        }
    }

    /// `zend_try_ct_eval_const` over a name written as a constant,
    /// php's `ZEND_AST_CONST`: one of [`Self::constant_name`]'s three,
    /// then the constant [`Self::const_name`] substitutes, a string the
    /// request's constant holds as a claim, and nothing where php leaves
    /// the name to `FETCH_CONSTANT`.
    fn named_constant(&mut self, token: TokenId) -> Result<Option<Operated>, CompileError> {
        if let Some(value) = self.constant_name(token) {
            return Ok(Some(Operated::Kept(value)));
        }
        let name = self.text(token).to_vec();
        self.const_name(&name)
    }

    /// The [`ClassOperand`] a class operand is as the tree stands now:
    /// a value where the parser made one ([`Self::written`]) or
    /// `zend_eval_const_expr` folded the expression to one ([`eval`]).
    fn decoded_class(&mut self, class: ClassSpec) -> Result<ClassOperand, CompileError> {
        let ClassSpec::Expr(expr) = class else {
            return Ok(match class {
                ClassSpec::Name(token) => ClassOperand::Name(token),
                ClassSpec::Expr(expr) => ClassOperand::Expr(expr),
            });
        };
        if let Some(value) = self.rewritten_value(self.peel(expr)) {
            return Ok(ClassOperand::Value(value));
        }
        Ok(match self.written(expr)? {
            Some(value) => ClassOperand::Value(value),
            None => ClassOperand::Expr(expr),
        })
    }

    /// `zend_compile_class_const`'s and `zend_eval_const_expr`'s first
    /// step over a class operand: `zend_eval_const_expr` over an
    /// expression, which leaves its rewrite for the class's compile, so
    /// what it folds to is the operand's value.
    fn evaluated_class(&mut self, class: ClassSpec) -> Result<ClassOperand, CompileError> {
        match class {
            ClassSpec::Name(token) => Ok(ClassOperand::Name(token)),
            ClassSpec::Expr(expr) => Ok(match self.eval_const(expr)? {
                Some(value) => ClassOperand::Value(value),
                None => ClassOperand::Expr(expr),
            }),
        }
    }

    /// `zend_resolve_class_name_ast` over a class operand php's tree
    /// holds as `ZEND_AST_ZVAL`, the name `zend_try_ct_eval_class_const`
    /// looks a constant of the class being compiled up by: a name written
    /// out resolved in the namespace, a value as `ZEND_NAME_FQ`
    /// ([`Self::fq_class_name`]); nothing for an expression.
    ///
    /// ```php
    /// class C { const X = 'x'; function f() { return ('C')::X . ('c' . '')::X; } }
    /// var_dump((new C)->f(), "C"[0]::X);
    /// ```
    /// ```expect
    /// string(2) "xx"
    /// string(1) "x"
    /// ```
    fn constant_class(&mut self, operand: ClassOperand) -> Result<Option<&'a [u8]>, CompileError> {
        match operand {
            ClassOperand::Name(token) => Ok(Some(self.class_name(self.text(token)))),
            ClassOperand::Value(value) => Ok(Some(self.arena.copy(&self.fq_class_name(value)?))),
            ClassOperand::Expr(_) => Ok(None),
        }
    }

    /// The name a class operand that is a value holds: a string, and
    /// php's `Illegal class name` for any other value.
    fn value_class_name(&self, value: Zval) -> Result<Vec<u8>, CompileError> {
        value
            .as_str()
            .map(|name| name.bytes().to_vec())
            .ok_or_else(|| self.compile_error("Illegal class name"))
    }

    /// `zend_resolve_class_name_ast` over a class operand that is a value
    /// ([`ClassOperand::Value`]), resolved as `ZEND_NAME_FQ`: its name
    /// ([`Self::value_class_name`]) with one leading `\` dropped, and
    /// `self`, `parent` or `static` php's `'\self' is an invalid class
    /// name`.
    ///
    /// ```php
    /// echo ("self")::X;
    /// ```
    /// ```expect exit=255
    /// Fatal error: '\self' is an invalid class name in %s on line 2
    /// ```
    fn fq_class_name(&self, value: Zval) -> Result<Vec<u8>, CompileError> {
        let name = self.value_class_name(value)?;
        self.fq_name(&name)
    }

    /// `zend_resolve_class_name` under `ZEND_NAME_FQ` over a name whose
    /// fetch type is the default one or that begins with `\`: the name
    /// with that `\` dropped, and php's compile error where what is left
    /// is `self`, `parent` or `static`.
    fn fq_name(&self, name: &[u8]) -> Result<Vec<u8>, CompileError> {
        let bare = name.strip_prefix(b"\\").unwrap_or(name);
        if class_fetch_type(bare) != FETCH_CLASS_DEFAULT {
            return Err(self.compile_error(format!(
                "'\\{}' is an invalid class name",
                String::from_utf8_lossy(bare)
            )));
        }
        Ok(bare.to_vec())
    }

    /// The type a cast is to, php's `attr` on `ZEND_AST_CAST`: the
    /// scanner's `T_INT_CAST`, `T_DOUBLE_CAST`, `T_STRING_CAST`,
    /// `T_ARRAY_CAST`, `T_OBJECT_CAST` and `T_BOOL_CAST`, each with the
    /// canonical spelling and the one 8.5 deprecates. `(unset)` is the
    /// compile error `zend_compile_cast` raises for it.
    fn cast_to(&self, op: Option<TokenId>, line: u32) -> Result<CastTo, CompileError> {
        match op.map(|op| php_syntax::cast_name(self.text(op))) {
            Some("T_INT_CAST") => Ok(CastTo::Long),
            Some("T_DOUBLE_CAST") => Ok(CastTo::Double),
            Some("T_STRING_CAST") => Ok(CastTo::Str),
            Some("T_ARRAY_CAST") => Ok(CastTo::Array),
            Some("T_OBJECT_CAST") => Ok(CastTo::Object),
            Some("T_BOOL_CAST") => Ok(CastTo::Bool),
            Some("T_UNSET_CAST") => Err(walk::refused(
                line,
                "The (unset) cast is no longer supported",
            )),
            _ => Err(walk::unread(line, "a cast")),
        }
    }

    /// Every element of an array expression as [`Element`] reads it, in
    /// order. An element with nothing in it, `[1, , 2]`, is php's
    /// compile error, raised at the line of the last element before it
    /// that had one (`zend_try_ct_eval_array`, `Z/Zend/zend_compile.c:10101`).
    ///
    /// ```php
    /// $a = [1,
    ///   , 2];
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use empty array elements in arrays in %s on line 2
    /// ```
    fn elements_of(
        &self,
        ids: &[NodeId<tree::ArrayElement>],
    ) -> Result<&'a [Element], CompileError> {
        let tree = &self.parsed.tree;
        let mut elements = self.arena.vec(ids.len());
        let mut last = self.line;
        for id in ids {
            let element = &tree[*id];
            let Some(value) = element.value else {
                let mut refused = self.compile_error("Cannot use empty array elements in arrays");
                refused.line = last;
                return Err(refused);
            };
            last = element.line();
            let key = element.key.map(|key| tree[key].key);
            elements.push(match (element.spread, key, element.byref) {
                (None, key, None) => Element::Value { key, value },
                (None, key, Some(_)) => Element::Ref { key, value },
                (Some(_), None, None) => Element::Unpack(value),
                // php's grammar has no `...` beside a key or a `&`
                // (`array_pair`, `Z/Zend/zend_language_parser.y`).
                (Some(_), ..) => {
                    return Err(walk::unread(last, "a `...` beside a key or a `&`"));
                }
            });
        }
        Ok(elements.into_slice())
    }

    /// `C::NAME` and `C::$name`: a class constant or a class's
    /// property (`ZEND_AST_STATIC_PROP`), over the class a name written
    /// out or an expression names ([`ClassSpec::of`]), or its name;
    /// a member the program names is refused as not read yet.
    fn scoped_of(
        &self,
        id: NodeId<tree::ScopedAccessExpr>,
        line: u32,
    ) -> Result<Scoped<'a>, CompileError> {
        let node = &self.parsed.tree[id];
        let class = ClassSpec::of(node.scope);
        let tree = &self.parsed.tree;
        let computed = |inner: Option<tree::Expr>, what| {
            inner
                .map(ScopedName::Computed)
                .ok_or_else(|| walk::unread(line, what))
        };
        match node.member {
            Some(tree::ScopedSelector::MemberName(member)) => {
                let Some(name) = tree[member].name else {
                    return Err(walk::unread(line, "a class constant"));
                };
                Ok(Scoped::Constant {
                    class,
                    name: ScopedName::Written(self.text(name)),
                })
            }
            Some(tree::ScopedSelector::BracedMember(braced)) => Ok(Scoped::Constant {
                class,
                name: computed(tree[braced].inner, "a class constant")?,
            }),
            Some(tree::ScopedSelector::SimpleVarExpr(var)) => {
                let Some(var) = tree[var].var else {
                    return Err(walk::unread(line, "a static property"));
                };
                Ok(Scoped::Property {
                    class,
                    name: ScopedName::Written(self.variable_name(var)),
                })
            }
            Some(tree::ScopedSelector::VarVarExpr(var)) => Ok(Scoped::Property {
                class,
                name: computed(tree[var].inner, "a static property")?,
            }),
            Some(tree::ScopedSelector::DynamicVarExpr(var)) => Ok(Scoped::Property {
                class,
                name: computed(tree[var].inner, "a static property")?,
            }),
            Some(tree::ScopedSelector::ClassKw(_)) => Ok(Scoped::ClassName { class }),
            None => Err(walk::unread(line, "a class member")),
        }
    }

    /// The three children of a `BinaryExpr`: the left side, the
    /// operator's kind and the right side, which recovery may leave
    /// absent.
    pub(crate) fn binary_of(
        &self,
        id: NodeId<tree::BinaryExpr>,
    ) -> (tree::Expr, Option<SyntaxKind>, Option<tree::Expr>) {
        let node = &self.parsed.tree[id];
        (node.lhs, node.op.map(|op| self.kind(op)), node.rhs)
    }

    /// Whether `expr` is `C::$p`, `C::$$p` or `C::${expr}`, php's
    /// `ZEND_AST_STATIC_PROP`: a scoped access whose member is a
    /// variable.
    fn is_static_prop(&self, expr: tree::Expr) -> bool {
        match self.peel(expr) {
            tree::Expr::ScopedAccessExpr(id) => self.names_property(id),
            _ => false,
        }
    }

    /// Whether the member after a `::` is a variable, `$p`, `$$p` or
    /// `${expr}`, which makes the access a class's property.
    pub(super) fn names_property(&self, id: NodeId<tree::ScopedAccessExpr>) -> bool {
        matches!(
            self.parsed.tree[id].member,
            Some(
                tree::ScopedSelector::SimpleVarExpr(_)
                    | tree::ScopedSelector::VarVarExpr(_)
                    | tree::ScopedSelector::DynamicVarExpr(_)
            )
        )
    }

    /// `zend_is_call` (`Z/Zend/zend_compile.c:2728`): a call by name, a
    /// call through a value, a call on a class or a method call, whose
    /// `VAR` is separated where it is written to, and a pipe, which is
    /// one of them built over its left side ([`Self::pipe`]). Each of the
    /// first four is one `ZEND_AST_CALL`, `ZEND_AST_STATIC_CALL` or
    /// `ZEND_AST_METHOD_CALL` in php and one `CallExpr` here, the callee
    /// telling them apart.
    fn is_call(&self, expr: tree::Expr) -> bool {
        match self.peel(expr) {
            tree::Expr::CallExpr(_) => true,
            tree::Expr::BinaryExpr(id) => {
                self.parsed.tree[id].op.map(|op| self.kind(op)) == Some(SyntaxKind::PIPE_GT)
            }
            _ => false,
        }
    }

    /// The `ZEND_AST_ZVAL` an expression is, or nothing where php's node
    /// is another kind: a number or a `'…'` string is its token's
    /// value, [`TokenValue::of`]; a `"…"`, a heredoc or a nowdoc with
    /// nothing interpolated is one string as php's parser folds it.
    fn literal_of(&self, expr: tree::Expr) -> Option<Literal> {
        written(self.parsed, expr)
    }
    /// Whether a node is php's `ZEND_AST_ZVAL`, one value its parser
    /// made before the compiler saw it: what
    /// `zend_compile_func_defined` and `zend_compile_func_chr` read an
    /// argument as.
    pub(crate) fn is_written_value(&self, expr: tree::Expr) -> bool {
        self.literal_of(expr).is_some()
    }

    /// `zend_compile_stmt_list` over a block's statements, entered as
    /// `zend_compile_stmt` enters one: `CG(zend_lineno) = ast->lineno`
    /// first (`Z/Zend/zend_compile.c:11692`), the `{`'s line, which each
    /// statement then sets to its own. A block that holds no statement
    /// sets it and emits nothing, so what is emitted after the block —
    /// the `JMP` back of a `for` with no condition, a `FAST_RET` — stands
    /// on the `{`'s line, which is [`Self::for`]'s witnesses.
    fn block(&mut self, id: NodeId<Block>) -> Result<(), CompileError> {
        let node = &self.parsed.tree[id];
        self.line = node.line();
        self.statements(self.list(node.stmts))
    }

    /// The body of a loop, an arm or a `declare`: a block's statements
    /// themselves, one statement alone, or the alternative syntax's
    /// list, which php's grammar hands `zend_compile_stmt` as one
    /// `statement` either way, a `ZEND_AST_STMT_LIST` for the first and
    /// the last.
    fn compound(
        &mut self,
        body: Option<Stmt>,
        stmt_list: Option<NodeId<StmtList>>,
    ) -> Result<(), CompileError> {
        match (body, stmt_list) {
            (Some(Stmt::Block(block)), _) => self.block(block),
            (Some(stmt), _) => self.statements(&[stmt]),
            (None, Some(list)) => {
                let node = &self.parsed.tree[list];
                self.line = node.line();
                self.statements(self.list(node.stmts))
            }
            (None, None) => Ok(()),
        }
    }

    /// `zend_compile_unset` (`Z/Zend/zend_compile.c:5555`), one operand:
    /// `zend_ensure_writable_variable`, `$this` php's compile error, and
    /// then the place decoded once ([`Self::place`]) and compiled under
    /// `BP_VAR_UNSET` with its last fetch's opcode overwritten, which
    /// leaves no value. A plain variable is `UNSET_CV` over the CV
    /// (`zend_try_compile_cv`), and one the program names `UNSET_VAR` over
    /// the name; an element is the chain under it held back
    /// ([`Self::delayed_var`] under [`Fetch::Unset`], so `$a[0][1]` is
    /// `FETCH_DIM_UNSET` under `UNSET_DIM`), the dimension, and the chain
    /// laid down, the append php's compile error; a property is the object
    /// the same way (`$a->b->c` is `FETCH_OBJ_UNSET` under `UNSET_OBJ`),
    /// the name as one constant in `op2` and three cache slots in
    /// `extended_value` (`zend_alloc_cache_slots(3)`); and a class's
    /// property is `UNSET_STATIC_PROP`, which the engine refuses in
    /// php's words.
    ///
    /// The constant offset goes through [`Self::dim_operand`], the one
    /// `zend_handle_numeric_dim` every dimension is held to, so the
    /// string as written stands in the slot after the long and
    /// `UNSET_DIM` hands `offsetUnset` on an `ArrayAccess` what php hands
    /// it (bug #63217).
    ///
    /// ```php
    /// $a = ['x' => ['y' => 1, 'z' => 2]];
    /// unset($a['x']['y'], $a['q']['r']);
    /// $o = new stdClass; $o->p = new stdClass; $o->p->q = 1; $o->p->r = [1, 2];
    /// unset($o->p->q, $o->p->r[0], $n->p, $n[0][1]);
    /// function f() { global $a; return $a; }
    /// $n = 'a';
    /// unset($$n['x']['z']);
    /// var_dump($a, $o);
    /// $v = 1; unset($$n, ${'v'});
    /// var_dump(isset($a), isset($v));
    /// class C { public static $s = [1, 2]; }
    /// unset(C::$s[0]);
    /// var_dump(C::$s);
    /// try { unset(C::$s); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    ///
    /// Warning: Undefined variable $n in %s on line 5
    /// array(1) {
    ///   ["x"]=>
    ///   array(0) {
    ///   }
    /// }
    /// object(stdClass)#1 (1) {
    ///   ["p"]=>
    ///   object(stdClass)#2 (1) {
    ///     ["r"]=>
    ///     array(1) {
    ///       [1]=>
    ///       int(2)
    ///     }
    ///   }
    /// }
    /// bool(false)
    /// bool(false)
    /// array(1) {
    ///   [1]=>
    ///   int(2)
    /// }
    /// Attempt to unset static property C::$s
    /// ```
    ///
    /// A variable fetched by name, an auto-global and `$GLOBALS['k']`
    /// among them, is `UNSET_VAR`, and the `FETCH_UNSET` under an element
    /// or a property of one; the object's own `unset_property` answers
    /// under the last, a hooked property's refusal among what it says:
    ///
    /// ```php
    /// class C { public $p = 1; public $q = 1; public int $h { get => 2; } }
    /// $_GET = new C;
    /// $n = 'o';
    /// $o = new C;
    /// $k = 'p';
    /// unset($_GET->p, $$n->$k, $GLOBALS['o']->q, $_POST->p);
    /// var_dump(isset($_GET->p), isset($o->p), isset($o->q), $_GET->h);
    /// try { unset($_GET->h); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $_SERVER['k'] = 1;
    /// $_SERVER['l'] = 2;
    /// $g = 1;
    /// unset($_SERVER['k'], $GLOBALS['g']);
    /// var_dump(isset($_SERVER['k']), $_SERVER['l'], isset($g));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// int(2)
    /// Cannot unset hooked property C::$h
    /// bool(false)
    /// int(2)
    /// bool(false)
    /// ```
    ///
    /// ```php
    /// unset($GLOBALS[]);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use [] for unsetting in %s on line 2
    /// ```
    fn unset(&mut self, target: tree::Expr) -> Result<(), CompileError> {
        let line = self.line_of(target);
        self.ensure_writable(target)?;
        if self.is_this(target) {
            return Err(walk::refused(line, "Cannot unset $this"));
        }
        // `zend_compile_unset`'s own arm over `$GLOBALS[]`.
        if self.is_globals_append(target) {
            return Err(walk::refused(line, "Cannot use [] for unsetting"));
        }
        match self.place(target)? {
            Target::Cv(name) => self.unset_cv(name),
            Target::Dim(dim) => {
                let tree::Expr::SubscriptExpr(id) = self.peel(dim) else {
                    return Err(self.fatal("an `unset` of an element that is no dimension"));
                };
                let (base, index) = (self.parsed.tree[id].base, self.parsed.tree[id].index);
                let mut container = self.delayed_var(base, Fetch::Unset)?;
                self.hold_base(&mut container);
                let Some(index) = index else {
                    return Err(walk::refused(line, "Cannot use [] for unsetting"));
                };
                let dim = self.expr(index)?;
                let (op2_type, op2) = self.dim_operand(dim);
                // The `UNSET_DIM` is the held-back `FETCH_DIM_UNSET`.
                let held = self.held_back();
                let (op1_type, op1) = self.base_fetch(container, Fetch::Unset, Over::Dim);
                drop(held);
                self.push(Opline {
                    opcode: Opcode::UnsetDim,
                    op1_type,
                    op1,
                    op2_type,
                    op2,
                    ..Opline::default()
                });
            }
            Target::Prop { object, name } => {
                let object = self.delayed_object(object, Fetch::Unset)?;
                let name = self.prop_name(name)?;
                // The `UNSET_OBJ` is the held-back `FETCH_OBJ_UNSET`.
                let held = self.held_back();
                let (op1_type, op1) = self.base_fetch(object, Fetch::Unset, Over::Obj);
                drop(held);
                self.push(Opline {
                    opcode: Opcode::UnsetObj,
                    op1_type,
                    op1,
                    op2_type: name.kind,
                    op2: name.at,
                    extended_value: name.slots,
                    ..Opline::default()
                });
            }
            Target::StaticProp { class, name } => {
                let StaticOperands {
                    name: (op1_type, op1),
                    class: (op2_type, op2),
                    slots,
                } = self.static_operands(class, &name)?;
                self.push(Opline {
                    opcode: Opcode::UnsetStaticProp,
                    op1_type,
                    op1,
                    op2_type,
                    op2,
                    extended_value: slots,
                    ..Opline::default()
                });
            }
            // `zend_compile_simple_var_no_cv` under `BP_VAR_UNSET`, its
            // `FETCH_UNSET` made `UNSET_VAR`: the name in `op1` and the
            // table it is deleted from in `extended_value`.
            Target::ByName(expr) => match self.named(expr)? {
                Some(by_name::Named::Cv(name)) => self.unset_cv(&name),
                Some(by_name::Named::This) => {
                    return Err(walk::refused(line, "Cannot unset $this"));
                }
                Some(by_name::Named::Globals) => {
                    return Err(self.compile_error(
                        "$GLOBALS can only be modified using the $GLOBALS[$name] = $value syntax",
                    ));
                }
                Some(by_name::Named::Fetch { name, table }) => {
                    let (op1_type, op1) = self.operand(name);
                    self.push(Opline {
                        opcode: Opcode::UnsetVar,
                        op1_type,
                        op1,
                        extended_value: table.fetch_type(),
                        ..Opline::default()
                    });
                }
                None => return Err(self.fatal("an `unset` by a name that is no variable")),
            },
            Target::List(_) => {
                return Err(walk::unread(line, "an `unset` over what is no variable"));
            }
        }
        Ok(())
    }

    /// `ZEND_UNSET_CV` over the CV `name` is.
    fn unset_cv(&mut self, name: &[u8]) {
        let cv = self.lookup_cv(name);
        self.push(Opline {
            opcode: Opcode::UnsetCv,
            op1_type: OperandKind::Cv,
            op1: cv,
            ..Opline::default()
        });
    }

    /// `zend_compile_global_var` (`Z/Zend/zend_compile.c:5441`) over one
    /// variable of a `global` statement: `$this` is php's compile error;
    /// a name `zend_try_compile_cv` gives a CV is `BIND_GLOBAL` with the
    /// CV in `op1`, the name as a constant string in `op2` and a cache
    /// slot in `extended_value`, the binding the engine makes to
    /// `EG(symbol_table)` by reference. The arm php keeps for a name it
    /// gives no CV, an auto-global or one the program computes, is
    /// `FETCH_W (global+lock)` and an `ASSIGN_REF` over a fetch by the
    /// same name ([`Self::global_by_name`]).
    ///
    /// ```php
    /// function f() { global $this; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use $this as global variable in %s on line 2
    /// ```
    fn global_var(&mut self, var: tree::Expr) -> Result<(), CompileError> {
        let line = self.line_of(var);
        self.line = line;
        let Some(named) = self.named(var)? else {
            return Err(self.unread("a `global` of what is no variable"));
        };
        let name = match named {
            by_name::Named::This => {
                return Err(walk::refused(line, "Cannot use $this as global variable"));
            }
            by_name::Named::Fetch { name, table } => {
                self.global_by_name(name, table);
                return Ok(());
            }
            // `zend_try_compile_cv` refuses `GLOBALS` as the auto-global
            // php registers it as, so the name is fetched as one.
            by_name::Named::Globals => {
                let name = self.string(b"GLOBALS")?;
                self.global_by_name(Where::Constant(name), Symbols::Global);
                return Ok(());
            }
            by_name::Named::Cv(name) => name,
        };
        self.cv_fetched(&name, Fetch::Read);
        let cv = self.lookup_cv(&name);
        let name = self.string(&name)?;
        let at = self.literal(name);
        let slot = self.cache_slot();
        self.push(Opline {
            opcode: Opcode::BindGlobal,
            op1_type: OperandKind::Cv,
            op1: cv,
            op2_type: OperandKind::Const,
            op2: at,
            extended_value: slot,
            ..Opline::default()
        });
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
    ///
    /// A label written just before the `try` stands outside it: php
    /// lays a `NOP` down between them, so a `goto` to it from inside
    /// leaves the `try` and serves the `finally` on the way
    /// (`Zend/tests/try/finally_goto_005.phpt`):
    ///
    /// ```php
    /// label: try {
    ///     goto label;
    /// } finally {
    ///     print "success";
    ///     return;
    /// }
    /// ```
    /// ```expect
    /// success
    /// ```
    fn r#try(&mut self, id: NodeId<tree::TryStmt>) -> Result<(), CompileError> {
        let node = &self.parsed.tree[id];
        let catches = self.list(node.catch_clauses);
        let finally = match node.finally_clause {
            Some(clause) => {
                let clause = &self.parsed.tree[clause];
                let Some(block) = clause.body else {
                    return Err(walk::unread(clause.line(), "a `finally` with no block"));
                };
                Some(block)
            }
            None => None,
        };
        if catches.is_empty() && finally.is_none() {
            return Err(self.compile_error("Cannot use try without catch or finally"));
        }
        // "label: try { } must not be equal to try { label: }": the
        // label written last standing at the next opline takes a `NOP`
        // of its own, so a `goto` to it leaves the `try` and runs the
        // `finally` (`Z/Zend/zend_compile.c`, `zend_compile_try`).
        if self
            .labels
            .last()
            .is_some_and(|(_, label)| label.opline == self.next())
        {
            self.push(Opline {
                opcode: Opcode::Nop,
                ..Opline::default()
            });
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
            self.loop_var_pushed();
        }
        self.try_catch_offset = Some(offset);
        self.block(node.body)?;
        let mut over_catches = Vec::new();
        if !catches.is_empty() {
            over_catches.push(self.jump_forward());
        }
        for (i, clause) in catches.iter().enumerate() {
            let last_catch = i + 1 == catches.len();
            let clause = &self.parsed.tree[*clause];
            self.line = clause.line();
            if i == 0 {
                let at = OplineIdx(self.next());
                if let Some(element) = self.try_catch.get_mut(offset as usize) {
                    element.catch_op = at;
                }
            }
            let opnum_catch = self.catch_clause(clause, last_catch)?;
            let Some(block) = clause.body else {
                return Err(walk::unread(clause.line(), "a `catch` with no block"));
            };
            self.block(block)?;
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
        if let Some(block) = finally {
            // `CG(zend_lineno) = finally_ast->lineno`, the block's own
            // line, before the `FAST_CALL` into it.
            let line = self.parsed.tree[block].line();
            self.finally_block(block, line, offset, saved_offset)?;
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
    ///
    /// `self`, `parent` and `static` name no class a `catch` can hold to
    /// (`zend_is_const_default_class_ref`):
    ///
    /// ```php
    /// class T { function m() { try { f(); } catch (static $e) {} } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Bad class name in the catch statement in %s on line 2
    /// ```
    fn catch_clause(
        &mut self,
        clause: &tree::CatchClause,
        last_catch: bool,
    ) -> Result<u32, CompileError> {
        let line = clause.line();
        let Some(types) = clause.types else {
            return Err(walk::unread(line, "a `catch` naming no class"));
        };
        let types: Vec<Vec<u8>> = self
            .list(self.parsed.tree[types].types)
            .iter()
            .map(|id| self.text(*id).to_vec())
            .collect();
        if types.is_empty() {
            return Err(walk::unread(line, "a `catch` naming no class"));
        }
        let var = clause.var.map(|id| self.variable_name(id));
        self.line = line;
        if types
            .iter()
            .any(|class| class_fetch_type(class) != FETCH_CLASS_DEFAULT)
        {
            return Err(self.compile_error("Bad class name in the catch statement"));
        }
        if var == Some(b"this".as_slice()) {
            return Err(self.compile_error("Cannot re-assign $this"));
        }
        let mut multicatch = Vec::new();
        let mut opnum_catch = self.next();
        let count = types.len();
        for (j, class) in types.iter().enumerate() {
            let last_class = j + 1 == count;
            let class_at = self.class_name_literal(class)?;
            let slot = self.cache_slot();
            let (out_type, out) = match &var {
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
        block: NodeId<Block>,
        line: u32,
        offset: u32,
        enclosing: Option<u32>,
    ) -> Result<(), CompileError> {
        let Some(var) = self.fast_call_var else {
            return Err(self.internal("a `finally` with no fast-call temporary"));
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
        self.block(block)?;
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
        self.loop_vars = self.loop_vars.saturating_sub(1);
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
    ///
    /// php adds the function once its body has compiled, so what the body
    /// raised comes before the redeclaration:
    ///
    /// ```php
    /// function f() {}
    /// function f(int $a = null) {}
    /// ```
    /// ```expect exit=255
    /// Deprecated: f(): Implicitly marking parameter $a as nullable is deprecated, the explicit nullable type must be used instead in %s on line 3
    ///
    /// Fatal error: Cannot redeclare function f() (previously declared in %s:2) in %s on line 3
    /// ```
    fn function_decl(&mut self, function: &Function) -> Result<(), CompileError> {
        let name = self.declare_symbol(Symbol::Function, &function.name)?;
        // `zend_begin_func_decl`, after the import check and before either
        // binding: `assert` is the compiler's own in every namespace
        // (`zend_compile_assert`).
        if function.name.eq_ignore_ascii_case(b"assert") {
            return Err(self.compile_error(
                "Defining a custom assert() function is not allowed, as the function has special semantics",
            ));
        }
        if self.depth != 1 {
            return self.nested_function_decl(function, &name);
        }
        let line = self.line;
        let at = crate::FunctionAt::Top(self.functions_declared.len());
        let active = ActiveFunction::at(&name, at);
        let declared = self.body(function, active, None, false, FnFlags::default())?;
        // "Only register the function after a successful compile": what
        // the body raised is raised before a name the table holds is
        // refused, at the declaration's first line.
        self.line = line;
        let lowered = self.arena.lowercase(&name);
        if let Some((earlier, body)) = self
            .functions_declared
            .iter()
            .find(|(decl, _)| decl.name.0.eq_ignore_ascii_case(lowered))
        {
            return Err(self.compile_error(format!(
                "Cannot redeclare function {} (previously declared in {}:{})",
                String::from_utf8_lossy(&name) + "()",
                String::from_utf8_lossy(&earlier.file.0),
                body.line_start
            )));
        }
        if self.native_index(lowered).is_some() {
            return Err(self.compile_error(format!(
                "Cannot redeclare function {}()",
                String::from_utf8_lossy(&name)
            )));
        }
        let at = u32::try_from(self.functions_declared.len()).unwrap_or(u32::MAX);
        self.top_level.push(TopLevel::Function(at));
        self.functions_declared.push(declared);
        self.landed
            .push((TopLevel::Function(at), self.diagnostics.len()));
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
        // `zend_string_tolower(op_array->function_name)`, which is that
        // string where lowering changes nothing.
        let lowercase = self.arena.lowercase(name);
        let lowered = self.own_string(lowercase, Sizing::Alloc)?;
        let mut active = ActiveFunction::at(name, crate::FunctionAt::Dynamic(at as usize));
        if let Some(key) = lowered.as_str() {
            if lowercase == name {
                active.own = Some(key);
            } else {
                self.own_names
                    .push((crate::Named::DefinitionKey(at as usize), key));
            }
        }
        let op1 = self.literal(lowered);
        self.push(Opline {
            opcode: Opcode::DeclareFunction,
            op1_type: OperandKind::Const,
            op1,
            op2: offset,
            ..Opline::default()
        });
        // "Do not leak the class scope into free standing functions,
        // even if they are dynamically defined inside a class method"
        // (`zend_compile_func_decl`): `CG(active_class_entry) = NULL`
        // for the body, so `__CLASS__` is empty there and a class it
        // declares is no nested one.
        let class = self.active_class.take();
        let parent = self.active_parent.take();
        let declared = self.body(function, active, None, false, FnFlags::default());
        self.active_class = class;
        self.active_parent = parent;
        let declared = declared?;
        if let Some(slot) = self.dynamic.get_mut(at as usize) {
            *slot = Some(declared);
        }
        Ok(())
    }

    /// `zend_compile_func_decl` over a closure as written, php's
    /// `ZEND_AST_CLOSURE` and `ZEND_AST_ARROW_FUNC`: the op array takes
    /// the next `zend_add_dynamic_func_def` offset of the enclosing one,
    /// as a nested function does ([`Self::nested_function_decl`]), and
    /// `zend_begin_func_decl`'s closure arm emits
    /// `ZEND_DECLARE_LAMBDA_FUNCTION` into a temporary instead of
    /// `ZEND_DECLARE_FUNCTION`, that temporary being where the `Closure`
    /// the engine makes of the op array lands. The name is
    /// [`Self::closure_name`], the binds are
    /// `zend_compile_closure_binding`'s `BIND_LEXICAL` per `use`
    /// variable — `find_implicit_binds` and
    /// `compile_implicit_lexical_binds` for an arrow function, whose
    /// captures are written nowhere — each at the line its name stands
    /// on, and the body reads them back with the `BIND_STATIC` prologue
    /// [`Self::closure_uses`] emits.
    fn closure(&mut self, closure: &Closure) -> Result<Where, CompileError> {
        let name = self.closure_name(closure.function.line);
        self.imports.see_closure(&name);
        let at = u32::try_from(self.dynamic.len()).unwrap_or(u32::MAX);
        self.dynamic.push(None);
        let offset = u32::try_from(self.dynamic_func_defs.len()).unwrap_or(u32::MAX);
        self.dynamic_func_defs.push(at);
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::DeclareLambdaFunction,
            out_type: OperandKind::Tmp,
            out,
            op2: offset,
            ..Opline::default()
        });
        let (binds, implicit_uses) = self.closure_binds(closure)?;
        let written = self.line;
        for bind in &binds {
            // `CG(zend_lineno) = zend_ast_get_lineno(var_name_ast)`.
            self.line = bind.line;
            let op2 = self.lookup_cv(&bind.name);
            self.push(Opline {
                opcode: Opcode::BindLexical,
                op1_type: OperandKind::Tmp,
                op1: out,
                op2_type: OperandKind::Cv,
                op2,
                extended_value: bind.slot_and_mode(false),
                ..Opline::default()
            });
        }
        self.line = written;
        let this_in_scope = self.this_in_scope && !closure.is_static;
        let flags = FnFlags {
            closure: true,
            is_static: closure.is_static,
            ..FnFlags::default()
        };
        let active = ActiveFunction::at(&name, crate::FunctionAt::Dynamic(at as usize));
        let saved = self.begin_body(active, this_in_scope, flags);
        let outcome = self.parameters_and_body(
            &closure.function,
            &name,
            None,
            &binds,
            closure.arrow.map(Short::Return),
        );
        let outcome = outcome.map(|(decl, mut code)| {
            code.implicit_uses = implicit_uses;
            (decl, code)
        });
        self.end_body(saved);
        if let Some(slot) = self.dynamic.get_mut(at as usize) {
            *slot = Some(outcome?);
        }
        Ok(Where::Temporary(out))
    }

    /// `zend_begin_func_decl`'s closure arm
    /// (`Z/Zend/zend_compile.c:8349-8385`): the name php builds for a
    /// closure, `{closure:%S%S%S%s:%u}` over the class, the separator,
    /// the function and the parentheses. In the main op array, which has
    /// no function name, the function is the file; inside a function or
    /// a method it is that name with `()` after it and the class and
    /// `::` before it; and inside another closure it is that closure's
    /// whole name with nothing added, so the chain does not grow a pair
    /// of parentheses per level. The number is the declaration's first
    /// line.
    fn closure_name(&self, line: u32) -> Vec<u8> {
        let mut name = b"{closure:".to_vec();
        match self.active_function.as_ref().map(|function| &function.name) {
            Some(enclosing) if self.fn_flags.closure => name.extend_from_slice(enclosing),
            Some(enclosing) => {
                if let Some(class) = &self.active_class {
                    name.extend_from_slice(&class.name);
                    name.extend_from_slice(b"::");
                }
                name.extend_from_slice(enclosing);
                name.extend_from_slice(b"()");
            }
            None => name.extend_from_slice(self.filename),
        }
        name.extend_from_slice(format!(":{line}}}").as_bytes());
        name
    }

    /// The variables a closure binds, in the order they land in
    /// `op_array->static_variables`: the `use` list as written for a
    /// closure (`zend_compile_closure_binding`), and for an arrow
    /// function the variables its body reads that its parameters do not
    /// declare (`find_implicit_binds`), beside the names that function's
    /// `info.uses` held ([`OpArray::implicit_uses`]). php's three compile
    /// errors are raised here, where `zend_hash_add` and the two name
    /// tests raise them.
    fn closure_binds(&mut self, closure: &Closure) -> Result<(Vec<Bind>, u32), CompileError> {
        if let Some(body) = closure.arrow {
            return Ok(self.implicit_binds(&closure.function, body));
        }
        let mut binds: Vec<Bind> = Vec::new();
        for used in &closure.uses {
            if used.name == b"this" {
                return Err(self.compile_error("Cannot use $this as lexical variable"));
            }
            if is_auto_global(&used.name) {
                return Err(self.compile_error("Cannot use auto-global as lexical variable"));
            }
            if binds.iter().any(|bind| bind.name == used.name) {
                return Err(self.compile_error(format!(
                    "Cannot use variable ${} twice",
                    String::from_utf8_lossy(&used.name)
                )));
            }
            binds.push(Bind {
                slot: u32::try_from(binds.len()).unwrap_or(u32::MAX),
                name: used.name.clone(),
                by_ref: used.by_ref,
                implicit: false,
                line: used.line,
            });
        }
        Ok((binds, 0))
    }

    /// `find_implicit_binds` (`Z/Zend/zend_compile.c:8089-8135`): the
    /// variables an arrow function's body reads, in the order it reads
    /// them, less its own parameters, `$this` and the names
    /// `zend_is_auto_global` answers for, which need no import. A closure written inside it contributes its
    /// `use` list and not its body; an arrow function inside it
    /// contributes its body. Every one is bound by value and implicitly,
    /// so a variable the enclosing scope has not is no warning. The
    /// count is of the names before the parameters are dropped, what
    /// `info.uses` held (`zend_hash_del`, `Z/Zend/zend_compile.c:8158`).
    fn implicit_binds(&self, function: &Function, body: tree::Expr) -> (Vec<Bind>, u32) {
        let mut names: Vec<Vec<u8>> = Vec::new();
        self.implicit_uses(&mut names, body);
        let held = u32::try_from(names.len()).unwrap_or(u32::MAX);
        names.retain(|name| {
            !function
                .parameters
                .iter()
                .any(|parameter| parameter.name == *name)
        });
        let binds = names
            .into_iter()
            .enumerate()
            .map(|(slot, name)| Bind {
                name,
                slot: u32::try_from(slot).unwrap_or(u32::MAX),
                by_ref: false,
                implicit: true,
                line: function.line,
            })
            .collect();
        (binds, held)
    }

    /// `zend_compile_closure_uses` and `zend_compile_implicit_closure_uses`
    /// (`Z/Zend/zend_compile.c:8007-8037`): the `BIND_STATIC` prologue
    /// of a closure's own op array, one opline per bound variable, which
    /// reads the value the `BIND_LEXICAL` beside the declaration put in
    /// the closure's table into the body's CV. php refuses a lexical
    /// variable a parameter already declares, `zend_compile_closure_uses`'
    /// walk over `op_array->vars`.
    fn closure_uses(&mut self, binds: &[Bind]) -> Result<(), CompileError> {
        for bind in binds {
            if !bind.implicit && self.vars.iter().any(|held| *held.0 == *bind.name) {
                return Err(self.compile_error(format!(
                    "Cannot use lexical variable ${} as a parameter name",
                    String::from_utf8_lossy(&bind.name)
                )));
            }
            self.static_vars.push(BoundVar {
                name: Name(bind.name.as_slice().into()),
                value: Zval::null(),
                lexical: true,
            });
            let op1 = self.lookup_cv(&bind.name);
            self.push(Opline {
                opcode: Opcode::BindStatic,
                op1_type: OperandKind::Cv,
                op1,
                extended_value: bind.slot_and_mode(true),
                ..Opline::default()
            });
        }
        Ok(())
    }

    /// `zend_compile_static_var` (`Z/Zend/zend_compile.c:5501`) over each
    /// variable of one `static` statement: `$this` and a name the op
    /// array's table already has are php's compile errors, then the
    /// initializer is folded as `zend_eval_const_expr` folds it. A value
    /// it folds to, or `null` where none is written, is what the table
    /// starts with, and one `BIND_STATIC` binds the CV to the table's
    /// entry by reference. An initializer it cannot fold leaves `null`
    /// in the table and runs as the evaluator left it ([`eval`]), a `??`
    /// over `null` its right side: `BIND_INIT_STATIC_OR_JMP`
    /// steps over it once the entry holds a value, and the
    /// `BIND_STATIC` after it binds the value it computed the first
    /// time. The slot and the `ZEND_BIND_REF` bit share `extended_value`
    /// as [`Bind::slot_and_mode`] lays them out.
    ///
    /// ```php
    /// function g($x) { static $v = $x * 2; return $v; }
    /// var_dump(g(1), g(5));
    /// for ($i = 0; $i < 3; $i++) { static $t = 10; $t++; }
    /// echo $t, "\n";
    /// ```
    /// ```expect
    /// int(2)
    /// int(2)
    /// 13
    /// ```
    ///
    /// ```php
    /// function f() { static $this; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use $this as static variable in %s on line 2
    /// ```
    ///
    /// A closure's `use` variables are entries of the same table, so a
    /// `static` of one of their names is the duplicate:
    ///
    /// ```php
    /// $f = function () use ($a) { static $a; };
    /// ```
    /// ```expect exit=255
    /// Fatal error: Duplicate declaration of static variable $a in %s on line 2
    /// ```
    fn static_var(&mut self, id: NodeId<tree::StaticVarStmt>) -> Result<(), CompileError> {
        for declarator in self
            .list(self.parsed.tree[id].static_var_declarators)
            .iter()
            .copied()
        {
            let declarator = &self.parsed.tree[declarator];
            let (line, init) = (declarator.line, declarator.init);
            let name = self.variable_name(declarator.var);
            self.line = line;
            self.ext_stmt();
            if name == b"this" {
                return Err(self.compile_error("Cannot use $this as static variable"));
            }
            if self.static_vars.iter().any(|held| *held.name.0 == *name) {
                return Err(self.compile_error(format!(
                    "Duplicate declaration of static variable ${}",
                    String::from_utf8_lossy(name)
                )));
            }
            let slot = u32::try_from(self.static_vars.len()).unwrap_or(u32::MAX) << 3;
            let folded = match init {
                None => Some(Zval::null()),
                Some(init) => self.interning(false, |this| this.eval_const(init))?,
            };
            self.static_vars.push(BoundVar {
                name: Name(name.into()),
                value: folded.unwrap_or_else(Zval::null),
                lexical: false,
            });
            let op1 = self.lookup_cv(name);
            let (Some(init), None) = (init, folded) else {
                self.push(Opline {
                    opcode: Opcode::BindStatic,
                    op1_type: OperandKind::Cv,
                    op1,
                    extended_value: slot | BIND_REF,
                    ..Opline::default()
                });
                self.tick();
                continue;
            };
            let jump = self.push(Opline {
                opcode: Opcode::BindInitStaticOrJmp,
                op1_type: OperandKind::Cv,
                op1,
                extended_value: slot,
                ..Opline::default()
            });
            let value = self.expr(init)?;
            let (op2_type, op2) = self.operand(value);
            self.push(Opline {
                opcode: Opcode::BindStatic,
                op1_type: OperandKind::Cv,
                op1,
                op2_type,
                op2,
                extended_value: slot | BIND_REF,
                ..Opline::default()
            });
            self.update_jump_target_to_next(jump);
            self.tick();
        }
        Ok(())
    }

    /// The property a constructor's promoted parameter declares, where
    /// `zend_compile_params` declares it, after the parameter's type:
    /// a name the class already has, a callable type, and an untyped
    /// `readonly` are php's fatals in php's words, and the property has
    /// no default of its own, uninitialized where it is typed and
    /// `null` where it is not.
    ///
    /// ```php
    /// class C { function __construct(public callable $f) {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Property C::$f cannot have type callable in %s on line 2
    /// ```
    ///
    /// A readonly class makes every one readonly:
    ///
    /// ```php
    /// readonly class P { function __construct(public int $x) {} }
    /// $p = new P(1);
    /// try { $p->x = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot modify readonly property P::$x
    /// ```
    ///
    /// ```php
    /// readonly class Q { function __construct(public $y) {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Readonly property Q::$y must have type in %s on line 2
    /// ```
    ///
    /// `final` promotes alone, and a set visibility is held to what a
    /// declared property's is:
    ///
    /// ```php
    /// class P { function __construct(final int $a, public private(set) int $b) {} }
    /// $p = new P(1, 2);
    /// var_dump($p->a, $p->b);
    /// try { $p->b = 3; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump((new ReflectionProperty('P', 'a'))->getModifiers(), (new ReflectionProperty('P', 'b'))->getModifiers());
    /// ```
    /// ```expect
    /// int(1)
    /// int(2)
    /// Cannot modify private(set) property P::$b from global scope
    /// int(33)
    /// int(4129)
    /// ```
    ///
    /// ```php
    /// class P { function __construct(public private(set) $b) {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Property with asymmetric visibility P::$b must have type in %s on line 2
    /// ```
    fn promoted_property(
        &mut self,
        parameter: &walk::Parameter,
        promoted: walk::Promoted,
        ty: Option<&Type>,
        earlier: &[Attribute],
    ) -> Result<(), CompileError> {
        let class_name = self.active_name().unwrap_or_default();
        let member = format!(
            "{}::${}",
            php_unit::decl::shown(&class_name),
            String::from_utf8_lossy(&parameter.name)
        );
        if self
            .active_decl()
            .is_some_and(|decl| decl.properties.iter().any(|p| *p.name.0 == *parameter.name))
        {
            return Err(self.compile_error(format!("Cannot redeclare {member}")));
        }
        if let Some(ty) = ty
            && ty.mask() & types::MAY_BE_CALLABLE != 0
        {
            return Err(self.compile_error(format!(
                "Property {member} cannot have type {}",
                php_unit::type_to_string(ty)
            )));
        }
        // A readonly class's promoted properties are readonly.
        let is_readonly = promoted.is_readonly
            || self
                .active_decl()
                .is_some_and(|decl| decl.flags.is_readonly);
        // A hooked one has no default until its link knows whether it
        // is virtual, and is no untyped readonly one to refuse here,
        // since its hooks refuse every readonly one.
        let hooked = parameter.hooks.is_some();
        if is_readonly && ty.is_none() && !hooked {
            return Err(self.compile_error(format!("Readonly property {member} must have type")));
        }
        let set_visibility = self.set_visibility_of(
            promoted.visibility,
            promoted.set_visibility,
            is_readonly,
            ty.is_some(),
            &member,
        )?;
        let default = (ty.is_none() && !hooked).then_some(ConstExpr::Null);
        let active = self.active_class.as_ref().map(|class| class.at);
        let (at, kind, declared_properties) =
            match active.and_then(|at| self.classes.get(at).map(|decl| (at, decl))) {
                Some((at, decl)) => (at, decl.kind, decl.properties.len()),
                None => {
                    return Err(self.internal("a promoted property of a class the unit has not"));
                }
            };
        let (hooks, is_virtual) = match &parameter.hooks {
            Some(hooks) => {
                let hooked = hooks::Hooked {
                    class: u32::try_from(at).unwrap_or(u32::MAX),
                    property: u32::try_from(declared_properties).unwrap_or(u32::MAX),
                    class_name: &class_name,
                    kind,
                    name: &parameter.name,
                    ty: parameter.ty,
                    declared: ty,
                    visibility: promoted.visibility,
                    is_static: false,
                    is_readonly,
                    is_abstract: false,
                };
                let (hooks, is_virtual) = self.property_hooks(&hooked, hooks)?;
                (Some(Box::new(hooks)), is_virtual)
            }
            None => (None, false),
        };
        let attributes = self.member_attributes(
            &parameter.attributes,
            attributes::TARGET_PROPERTY,
            attributes::TARGET_PARAMETER,
            parameter.line,
            earlier,
        )?;
        let declared = PropertyDecl {
            name: Name(parameter.name.clone().into_boxed_slice()),
            visibility: promoted.visibility,
            set_visibility,
            is_instance: true,
            is_readonly,
            is_final: promoted.is_final || set_visibility == Some(Visibility::Private),
            is_promoted: true,
            is_virtual,
            ty: ty.cloned(),
            default,
            attributes,
            doc_comment: parameter.doc_comment.clone().map(Vec::into_boxed_slice),
            hooks,
        };
        if let Some(decl) = self.classes.get(at) {
            self.hooked_property_verified(decl, &declared)?;
        }
        let Some(decl) = self.classes.get_mut(at) else {
            return Err(self.internal("a promoted property of a class the unit has not"));
        };
        decl.properties.push(declared);
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
    ///
    /// A class that declares `__toString` implements `Stringable`
    /// (`add_stringable_interface`), which keeps it from the early bind,
    /// and the method returns `string` where it declares nothing:
    ///
    /// ```php
    /// class A { function __toString() { return 1; } }
    /// var_dump(new A instanceof Stringable);
    /// echo new A, "\n";
    /// ```
    /// ```expect exit=0
    /// bool(true)
    /// 1
    /// ```
    fn method_decl(&mut self, class: usize, method: &Method) -> Result<(), CompileError> {
        let Some((class_name, kind, flags)) = self
            .classes
            .get(class)
            .map(|decl| (decl.name.0.to_vec(), decl.kind, decl.flags))
        else {
            return Err(self.internal("a method of a class the unit has not"));
        };
        let name = method.function.name.clone();
        let is_abstract = self.begin_method_decl(&class_name, kind, flags, method)?;
        if self.classes.get(class).is_some_and(|decl| {
            decl.methods
                .iter()
                .any(|m| m.function.name.0.eq_ignore_ascii_case(&name))
        }) {
            return Err(self.compile_error(format!(
                "Cannot redeclare {}::{}()",
                php_unit::decl::shown(&class_name),
                String::from_utf8_lossy(&name)
            )));
        }
        self.set_declaring(Some(Declaring {
            name: name.clone(),
            visibility: method.visibility,
            is_final: method.is_final,
            signature: None,
        }));
        let at = crate::FunctionAt::Method {
            class,
            method: self.classes.get(class).map_or(0, |decl| decl.methods.len()),
        };
        let compiled = self.body(
            &method.function,
            ActiveFunction::at(&name, at),
            Some(&class_name),
            method.is_static,
            FnFlags {
                is_abstract,
                ..FnFlags::default()
            },
        );
        self.set_declaring(None);
        let (function, body) = compiled?;
        let declared = MethodDecl {
            function,
            visibility: method.visibility,
            is_instance: !method.is_static,
            is_abstract,
            is_final: method.is_final,
            tentative_return: false,
        };
        self.line = method.function.line;
        self.magic_method(&class_name, &declared)?;
        let Some(decl) = self.classes.get_mut(class) else {
            return Err(self.internal("a method of a class the unit has not"));
        };
        // `add_stringable_interface`: a class that declares
        // `__toString` implements `Stringable`, written or not.
        if name.eq_ignore_ascii_case(b"__tostring")
            && decl.kind != ClassKind::Trait
            && !decl
                .interfaces
                .iter()
                .any(|interface| interface.0.eq_ignore_ascii_case(b"Stringable"))
        {
            decl.interfaces.push(Name(b"Stringable".as_slice().into()));
            decl.flags.stringable_added = true;
        }
        let at = u32::try_from(decl.methods.len()).unwrap_or(u32::MAX);
        decl.methods.push(declared);
        self.methods.push(MethodBody {
            class: u32::try_from(class).unwrap_or(u32::MAX),
            method: at,
            body,
        });
        Ok(())
    }

    /// `zend_compile_func_decl`'s arm over `ZEND_ACC_NODISCARD`, which
    /// the attribute's validator set: a function whose return type takes
    /// `void` or `never` returns no value to use.
    ///
    /// ```php
    /// class A { #[NoDiscard] function f(): void {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: A void method does not return a value, but #[\NoDiscard] requires a return value in %s on line 2
    /// ```
    ///
    /// ```php
    /// #[NoDiscard] function f(): never { exit; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: A never returning function does not return a value, but #[\NoDiscard] requires a return value in %s on line 2
    /// ```
    ///
    /// `validate_nodiscard` refuses it on a property's `get` or `set`
    /// first ([`php_unit::decl::hook_attribute_refusal`]), and sets no
    /// `ZEND_ACC_NODISCARD` there, so nothing after it is checked; under
    /// `#[\DelayedTargetValidation]` the refusal waits for
    /// `ReflectionAttribute::newInstance` and the hook compiles.
    ///
    /// ```php
    /// class A { public $p { #[\NoDiscard] get => 1; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: #[\NoDiscard] is not supported for property hooks in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { public string $p { #[DelayedTargetValidation] #[NoDiscard] set => strtoupper($value); } }
    /// $a = new A;
    /// $a->p = "x";
    /// var_dump($a->p);
    /// ```
    /// ```expect
    /// string(1) "X"
    /// ```
    fn no_discard_refused(&self, attributes: &[Attribute]) -> Result<(), CompileError> {
        let named = |which: InternalAttribute| {
            attributes
                .iter()
                .any(|a| InternalAttribute::named(&a.name.0) == Some(which))
        };
        let marked = named(InternalAttribute::NoDiscard);
        if marked && self.active_hook.is_some() {
            let refused = attributes
                .iter()
                .find_map(|a| php_unit::decl::hook_attribute_refusal(&a.name.0));
            return match refused {
                Some(refused) if !named(InternalAttribute::DelayedTargetValidation) => {
                    Err(self.compile_error(refused))
                }
                _ => Ok(()),
            };
        }
        let Some(ty) = self.return_type.as_ref().filter(|_| marked) else {
            return Ok(());
        };
        let what = if self.active_class.is_some() {
            "method"
        } else {
            "function"
        };
        let mask = ty.mask();
        if mask & types::MAY_BE_VOID != 0 {
            return Err(self.compile_error(format!(
                "A void {what} does not return a value, but #[\\NoDiscard] requires a return value"
            )));
        }
        if mask & types::MAY_BE_NEVER != 0 {
            return Err(self.compile_error(format!(
                "A never returning {what} does not return a value, but #[\\NoDiscard] requires a return value"
            )));
        }
        Ok(())
    }

    /// `zend_begin_method_decl`'s refusals of a method against its class,
    /// in php's order, before the name goes into `ce->function_table`, and
    /// whether the method is `ZEND_ACC_ABSTRACT`, which every method of
    /// an interface is. An abstract method stands in an abstract class,
    /// an interface or a trait, takes no body and is not private outside
    /// a trait; every other method has a body.
    ///
    /// ```php
    /// abstract class A { abstract function f(); }
    /// class B extends A { function f() { echo "B::f\n"; } }
    /// (new B)->f();
    /// interface I { function g(); }
    /// class C implements I { function g() { echo "C::g\n"; } }
    /// (new C)->g();
    /// ```
    /// ```expect
    /// B::f
    /// C::g
    /// ```
    ///
    /// ```php
    /// class A { abstract function f(); }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class A declares abstract method f() and must therefore be declared abstract in %s on line 2
    /// ```
    ///
    /// ```php
    /// $o = new class { abstract function f(); };
    /// ```
    /// ```expect exit=255
    /// Fatal error: Anonymous class method f() must not be abstract in %s on line 2
    /// ```
    ///
    /// ```php
    /// enum E { abstract function f(); }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Enum method E::f() must not be abstract in %s on line 2
    /// ```
    ///
    /// ```php
    /// interface I { protected function f(); }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Access type for interface method I::f() must be public in %s on line 2
    /// ```
    ///
    /// ```php
    /// interface I { final function f(); }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Interface method I::f() must not be final in %s on line 2
    /// ```
    ///
    /// ```php
    /// abstract class A { abstract private function f(); }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Abstract function A::f() cannot be declared private in %s on line 2
    /// ```
    ///
    /// ```php
    /// interface I { function f() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Interface function I::f() cannot contain body in %s on line 2
    /// ```
    ///
    /// ```php
    /// abstract class A { abstract function f() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Abstract function A::f() cannot contain body in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { function f(); }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Non-abstract method A::f() must contain body in %s on line 2
    /// ```
    ///
    /// A private final method is php's warning, raised as the class
    /// compiles, and a constructor is not:
    ///
    /// ```php
    /// class A { private final function f() {} private final function __construct() {} }
    /// echo "done\n";
    /// ```
    /// ```expect
    /// Warning: Private methods cannot be final as they are never overridden by other classes in %s on line 2
    /// done
    /// ```
    fn begin_method_decl(
        &mut self,
        class_name: &[u8],
        kind: ClassKind,
        flags: ClassFlags,
        method: &Method,
    ) -> Result<bool, CompileError> {
        let shown = format!(
            "{}::{}()",
            php_unit::decl::shown(class_name),
            String::from_utf8_lossy(&method.function.name)
        );
        if method.visibility == Visibility::Private
            && method.is_final
            && !method.function.name.eq_ignore_ascii_case(b"__construct")
        {
            self.raise(
                Level::CompileWarning,
                "Private methods cannot be final as they are never overridden by other classes",
            );
        }
        if method.is_abstract && !flags.is_abstract && kind != ClassKind::Trait {
            let name = String::from_utf8_lossy(&method.function.name);
            let class = php_unit::decl::shown(class_name);
            return Err(self.compile_error(match kind {
                // "Don't say that the class should be declared abstract if
                // it is anonymous or an enum and can't be abstract".
                _ if flags.is_anonymous => {
                    format!("Anonymous class method {name}() must not be abstract")
                }
                ClassKind::Enum | ClassKind::Interface => format!(
                    "{} method {class}::{name}() must not be abstract",
                    if kind == ClassKind::Enum {
                        "Enum"
                    } else {
                        "Interface"
                    }
                ),
                ClassKind::Class | ClassKind::Trait => format!(
                    "Class {class} declares abstract method {name}() and must therefore be declared abstract"
                ),
            }));
        }
        let in_interface = kind == ClassKind::Interface;
        if in_interface {
            if method.visibility != Visibility::Public {
                return Err(self.compile_error(format!(
                    "Access type for interface method {shown} must be public"
                )));
            }
            if method.is_final {
                return Err(
                    self.compile_error(format!("Interface method {shown} must not be final"))
                );
            }
        }
        let is_abstract = method.is_abstract || in_interface;
        let what = if in_interface {
            "Interface"
        } else {
            "Abstract"
        };
        if is_abstract {
            if method.visibility == Visibility::Private && kind != ClassKind::Trait {
                return Err(self.compile_error(format!(
                    "{what} function {shown} cannot be declared private"
                )));
            }
            if method.has_body {
                return Err(
                    self.compile_error(format!("{what} function {shown} cannot contain body"))
                );
            }
        } else if !method.has_body {
            return Err(
                self.compile_error(format!("Non-abstract method {shown} must contain body"))
            );
        }
        Ok(is_abstract)
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
        active: ActiveFunction,
        scope: Option<&[u8]>,
        is_static: bool,
        fn_flags: FnFlags,
    ) -> Result<(FunctionDecl, OpArray), CompileError> {
        let flags = FnFlags {
            method: scope.is_some(),
            ..fn_flags
        };
        let name = active.name.clone();
        let saved = self.begin_body(active, scope.is_some() && !is_static, flags);
        let outcome = self.parameters_and_body(function, &name, scope, &[], None);
        self.end_body(saved);
        outcome
    }

    /// The prologue, the binds, then the body: the statements as
    /// written, or for an arrow function and a hook's short form the
    /// one statement php makes of the expression after `=>`
    /// ([`Short`]), at that expression's line.
    fn parameters_and_body(
        &mut self,
        function: &Function,
        name: &[u8],
        scope: Option<&[u8]>,
        binds: &[Bind],
        arrow: Option<Short>,
    ) -> Result<(FunctionDecl, OpArray), CompileError> {
        let line = function.line;
        self.line = line;
        self.returns_by_ref = function.by_ref;
        let target = if scope.is_some() {
            attributes::TARGET_METHOD
        } else {
            attributes::TARGET_FUNCTION
        };
        let attributes = self.member_attributes(&function.attributes, target, 0, line, &[])?;
        // `zend_compile_params`' `fallback_return_type`: a method named
        // `__toString` with no return type written returns `string` all
        // the same, which is what `Stringable::__toString` holds it to.
        self.return_type = match function.return_type {
            Some(written) => Some(self.typename(written, false)?.0),
            // The `void` `zend_compile_property_hooks` writes as a `set`
            // hook's return type.
            None if function.void_return => Some(Type::Void),
            None if scope.is_some() && name.eq_ignore_ascii_case(b"__tostring") => {
                Some(Type::String)
            }
            None => None,
        };
        let is_ctor = scope.is_some() && name.eq_ignore_ascii_case(b"__construct");
        // `get_function_or_method_name`, what a deprecation of the
        // prologue names the function by: a method by its class, and a
        // closure by its name alone, since its op array has no scope
        // while it compiles.
        let shown = match scope {
            Some(owner) => format!(
                "{}::{}",
                String::from_utf8_lossy(owner),
                String::from_utf8_lossy(name)
            ),
            None => String::from_utf8_lossy(name).into_owned(),
        };
        // `zend_compile_params` over the return type, before any
        // parameter: `&` over `void` is deprecated.
        if function.by_ref && matches!(self.return_type, Some(Type::Void)) {
            self.raise(
                Level::Deprecated,
                &format!("{shown}(): Returning by reference from a void function is deprecated"),
            );
        }
        let parameters = self.recv_prologue(function, is_ctor, &shown)?;
        self.no_discard_refused(&attributes)?;
        self.declared_signature(&parameters, &attributes);
        // The entry safepoint is the opline after the prologue (design
        // §5.7), which `begin_body` numbered zero at opline zero; the
        // assignments a promoted parameter makes stand after it, as
        // `zend_compile_params` emits them after every `RECV*`.
        let entry = self.next();
        if let Some(first) = self.safepoints.first_mut() {
            *first = OplineIdx(entry);
        }
        self.promotions(function)?;
        if function.generator {
            // `zend_compile_func_decl` after `zend_compile_params`
            // (`Z/Zend/zend_compile.c:8553`): the frame is moved into
            // the `Generator` the call returns before anything else of
            // the body runs.
            self.fn_flags.generator = true;
            self.mark_function_as_generator()?;
            self.push(Opline {
                opcode: Opcode::GeneratorCreate,
                ..Opline::default()
            });
        }
        self.closure_uses(binds)?;
        self.statements(&function.body)?;
        if let Some(short) = arrow {
            let value = match short {
                Short::Return(value) | Short::Assign(value) => value,
            };
            self.depth = self.depth.saturating_add(1);
            self.line = value.line(self.parsed);
            self.ext_stmt();
            self.statements_seen = self.statements_seen.saturating_add(1);
            match short {
                // `zend_compile_func_decl` wraps an arrow function's
                // body in `ZEND_AST_RETURN` only where the return type
                // holds no `never`; under `never` the body is an
                // expression statement, and `VERIFY_NEVER_TYPE` follows.
                Short::Return(value) if self.returns_never() => self.expr_stmt(value)?,
                Short::Return(value) => {
                    self.r#return(Some(value))?;
                }
                Short::Assign(value) => self.assign_backing_value(value)?,
            }
            // The one statement php's walk makes of the body, compiled
            // by `zend_compile_stmt` as any other.
            self.tick();
            self.depth = self.depth.saturating_sub(1);
        }
        // `zend_compile_func_decl_ex`'s `zend_do_extended_stmt(NULL)`
        // before the implicit return, on the closing brace's line.
        self.line = function.end_line;
        self.ext_stmt();
        let body = self.finish(false, line, function.end_line)?;
        Ok((
            FunctionDecl {
                name: Name(name.into()),
                parameters,
                return_type: self.return_type.clone(),
                returns_by_ref: function.by_ref,
                is_generator: function.generator,
                attributes,
                doc_comment: function.doc_comment.clone().map(Vec::into_boxed_slice),
                file: Name(self.filename.into()),
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
    ///
    /// What the prologue refuses, and the two deprecations it raises and
    /// compiles past, each at the parameter's line and naming the function
    /// as `get_function_or_method_name` does:
    ///
    /// ```php
    /// function f($_GET) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot re-assign auto-global variable _GET in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f($this) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use $this as parameter in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f($a = 1, $b) {}
    /// ```
    /// ```expect exit=0
    /// Deprecated: f(): Optional parameter $a declared before required parameter $b is implicitly treated as a required parameter in %s on line 2
    /// ```
    ///
    /// ```php
    /// class C { function m(int $x = null) { var_dump($x); } }
    /// (new C)->m();
    /// ```
    /// ```expect exit=0
    /// Deprecated: C::m(): Implicitly marking parameter $x as nullable is deprecated, the explicit nullable type must be used instead in %s on line 2
    /// NULL
    /// ```
    ///
    /// The `Type $x = null` spelling before a required parameter is made
    /// required too, with its type nullable and no deprecation of its
    /// own: a call that leaves it out names it as not passed.
    ///
    /// ```php
    /// function f(Foo $c = null, $d) { var_dump($c, $d); }
    /// try { f(d: 1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
    /// f(null, 2);
    /// echo (new ReflectionFunction('f'))->getParameters()[0], "\n";
    /// ```
    /// ```expect
    /// Deprecated: f(): Implicitly marking parameter $c as nullable is deprecated, the explicit nullable type must be used instead in %s on line 2
    /// f(): Argument #1 ($c) not passed
    /// NULL
    /// int(2)
    /// Parameter #0 [ <required> ?Foo $c ]
    /// ```
    ///
    /// A function that returns by reference and is declared `void` is
    /// deprecated before its parameters are read:
    ///
    /// ```php
    /// function &f(): void {}
    /// $r = &f();
    /// var_dump($r);
    /// ```
    /// ```expect
    /// Deprecated: f(): Returning by reference from a void function is deprecated in %s on line 2
    ///
    /// Notice: Only variable references should be returned by reference in %s on line 2
    /// NULL
    /// ```
    ///
    /// A closure is named without a class, even written in a method:
    ///
    /// ```php
    /// class C { function m() { return function ($a = 1, $b) {}; } }
    /// ```
    /// ```expect
    /// Deprecated: {closure:C::m():2}(): Optional parameter $a declared before required parameter $b is implicitly treated as a required parameter in %s on line 2
    /// ```
    fn recv_prologue(
        &mut self,
        function: &Function,
        is_ctor: bool,
        shown: &str,
    ) -> Result<Vec<Parameter>, CompileError> {
        let last_required = function
            .parameters
            .iter()
            .rposition(|parameter| parameter.default.is_none() && !parameter.variadic);
        let mut parameters = Vec::with_capacity(function.parameters.len());
        let mut variadic_seen = false;
        for (at, parameter) in function.parameters.iter().enumerate() {
            let number = u32::try_from(at).unwrap_or(u32::MAX);
            self.line = parameter.line;
            let named = String::from_utf8_lossy(&parameter.name).into_owned();
            let cv = self.parameter_cv(parameter, number)?;
            self.parameter_refused(parameter, variadic_seen, is_ctor)?;
            variadic_seen = parameter.variadic;
            let mut folded =
                self.without_persistent_substitution(|this| this.parameter_default(parameter))?;
            let promoted_target = if parameter.promote.is_some() {
                attributes::TARGET_PROPERTY
            } else {
                0
            };
            let attrs = self.member_attributes(
                &parameter.attributes,
                attributes::TARGET_PARAMETER,
                promoted_target,
                parameter.line,
                &[],
            )?;
            let (ty, implicitly_nullable) = self.parameter_type(parameter, &mut folded, shown)?;
            // `zend_compile_params` makes every optional parameter before
            // the last required one `ZEND_RECV` with no default, the
            // `Type $x = null` spelling too, whose nullable type stands
            // and whose deprecation alone is skipped.
            let keeps_default = !parameter.variadic && last_required.is_none_or(|last| at > last);
            if let (Some(last), Some(_), false) = (last_required, &folded, implicitly_nullable)
                && at < last
                && let Some(required) = function.parameters.get(last)
            {
                self.raise(
                    Level::Deprecated,
                    &format!(
                        "{shown}(): Optional parameter ${named} declared before required parameter ${} is implicitly treated as a required parameter",
                        String::from_utf8_lossy(&required.name)
                    ),
                );
            }
            let default = if let (Some(folded), true) = (folded, keeps_default) {
                let slot = self.literal(folded);
                self.push(Opline {
                    opcode: Opcode::RecvInit,
                    op1: number.saturating_add(1),
                    op2_type: OperandKind::Const,
                    op2: slot,
                    out_type: OperandKind::Cv,
                    out: cv,
                    ..Opline::default()
                });
                self.declared_default(parameter, folded)?
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
            if let Some(promoted) = parameter.promote {
                self.promoted_property(parameter, promoted, ty.as_ref(), &attrs)?;
            }
            parameters.push(Parameter {
                name: Name(parameter.name.clone().into_boxed_slice()),
                ty,
                by_ref: parameter.by_ref,
                prefer_ref: false,
                promoted: parameter.promote.is_some(),
                variadic: parameter.variadic,
                default: default.map_or(ParameterDefault::None, ParameterDefault::Expr),
                attributes: attrs,
            });
        }
        Ok(parameters)
    }

    /// The CV a parameter takes, `lookup_cv` over its name, which must
    /// be the next one, after the names php refuses a parameter.
    fn parameter_cv(
        &mut self,
        parameter: &walk::Parameter,
        number: u32,
    ) -> Result<u32, CompileError> {
        let named = String::from_utf8_lossy(&parameter.name).into_owned();
        if is_auto_global(&parameter.name) {
            return Err(
                self.compile_error(format!("Cannot re-assign auto-global variable {named}"))
            );
        }
        let cv = self.lookup_cv(&parameter.name);
        if cv != number {
            return Err(self.compile_error(format!("Redefinition of parameter ${named}")));
        }
        if parameter.name == b"this" {
            return Err(self.compile_error("Cannot use $this as parameter"));
        }
        if parameter.name == HTTP_RESPONSE_HEADER {
            self.http_response_header = ResponseHeader::Own;
        }
        Ok(cv)
    }

    /// A parameter's compiled type, `zend_compile_typename_ex` with a
    /// `null` default under a written type as its PHP 5 spelling of
    /// `?T`, which is php's deprecation, and whether that made it
    /// nullable.
    fn parameter_type(
        &mut self,
        parameter: &walk::Parameter,
        folded: &mut Option<Zval>,
        shown: &str,
    ) -> Result<(Option<Type>, bool), CompileError> {
        let Some(written) = parameter.ty else {
            return Ok((None, false));
        };
        let force_nullable = parameter.promote.is_none()
            && folded
                .as_ref()
                .is_some_and(|zval| zval.kind() == ZvalType::Null);
        let (ty, forced) = self.typename(written, force_nullable)?;
        self.parameter_type_refused(&ty, parameter, folded.as_mut(), force_nullable)?;
        if !forced {
            return Ok((Some(ty), false));
        }
        self.raise(
            Level::Deprecated,
            &format!(
                "{shown}(): Implicitly marking parameter ${} as nullable is deprecated, the explicit nullable type must be used instead",
                String::from_utf8_lossy(&parameter.name)
            ),
        );
        Ok((Some(Type::Nullable(Box::new(ty))), true))
    }

    /// The default a parameter's declaration keeps beside its
    /// `RECV_INIT` constant `folded`, which is the default php keeps:
    /// an array it folded to, or a string the unit counts, is the
    /// declaration's too ([`UnitValue`]), and anything else is the
    /// expression as written, compiled as `zend_compile_params` compiles
    /// it.
    ///
    /// The value is the constant's, which holds it; the declaration is a
    /// view of it. So an `eval`'d default folded to an array reads php's
    /// value as written and folded, in a call, in Reflection and in
    /// `debug_zval_dump`, the constant, `$v` and the argument in the
    /// body, the constant and the copy from `getDefaultValue`:
    ///
    /// ```php
    /// eval('function g($v = null ?? [1], $w = [1] + [5 => 2], $x = [[1]][0], $y = 1 ? [2] : 3, $z = (array) "x") { debug_zval_dump($v); return [$v, $w, $x, $y, $z]; }');
    /// echo json_encode(g()), "\n";
    /// foreach ((new ReflectionFunction('g'))->getParameters() as $p) {
    ///     echo $p, "\n";
    ///     debug_zval_dump($p->getDefaultValue());
    /// }
    /// ```
    /// ```expect regime=jit-off
    /// array(1) packed refcount(3){
    ///   [0]=>
    ///   int(1)
    /// }
    /// [[1],{"0":1,"5":2},[1],[2],["x"]]
    /// Parameter #0 [ <optional> $v = [1] ]
    /// array(1) packed refcount(2){
    ///   [0]=>
    ///   int(1)
    /// }
    /// Parameter #1 [ <optional> $w = [0 => 1, 5 => 2] ]
    /// array(2) packed refcount(2){
    ///   [0]=>
    ///   int(1)
    ///   [5]=>
    ///   int(2)
    /// }
    /// Parameter #2 [ <optional> $x = [1] ]
    /// array(1) packed refcount(2){
    ///   [0]=>
    ///   int(1)
    /// }
    /// Parameter #3 [ <optional> $y = [2] ]
    /// array(1) packed refcount(2){
    ///   [0]=>
    ///   int(2)
    /// }
    /// Parameter #4 [ <optional> $z = ['x'] ]
    /// array(1) packed refcount(2){
    ///   [0]=>
    ///   string(1) "x" interned
    /// }
    /// ```
    fn declared_default(
        &mut self,
        parameter: &walk::Parameter,
        folded: Zval,
    ) -> Result<Option<ConstExpr>, CompileError> {
        if let Some(held) = UnitValue::new(folded) {
            return Ok(Some(ConstExpr::UnitValue(held)));
        }
        let written = self.without_persistent_substitution(|this| {
            parameter
                .default
                .map(|expr| this.dynamic_const_expr(expr))
                .transpose()
        })?;
        // `zend_is_valid_default_value` converted an int under a type
        // with `float` and no `int` where it stood.
        Ok(match (written, folded.kind()) {
            (Some(ConstExpr::Long { value: n, .. }), ZvalType::Double) => {
                Some(ConstExpr::Double(double_of(Zval::long(n))))
            }
            (written, _) => written,
        })
    }

    /// `f` with what the home lays interned or not as `on` says
    /// ([`Intern::interning`]), off as php's scanner and fold leave a
    /// string no interning asks for: a constant array's elements and
    /// keys, `in_array`'s haystack and a `static`'s initial value cost
    /// their strings where opcache holds the bytes, as the program below
    /// does, where a string an opline names costs none (the program under
    /// the recipe's `TMPDIR`, written into the past so opcache keeps it,
    /// read after the heap is churned):
    ///
    /// ```php
    /// $put = function ($name, $src) { $f = sys_get_temp_dir() . "/$name"; file_put_contents("$f.tmp", $src); touch("$f.tmp", time() - 10); rename("$f.tmp", $f); return $f; };
    /// $f = $put('w3323-uninterned.inc', <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// $a = $b = $c = null; m('$q1 = 1;'); m('function w1() {}'); m('$GLOBALS["z"] = 1;');
    /// $held = ["abcdefghijk1", "abcdefghijk2", "abcdefghijk3", "abcdefghijk4", "abcdefghijk5"];
    /// echo m('$GLOBALS["a"] = ["abcdefghijk1" => "abcdefghijk2", 7 => ["abcdefghijk5"]];'), " ";
    /// echo m('function sv1() { static $s = "abcdefghijk3"; return $s; }'), " ";
    /// echo m('function ia1($x) { return in_array($x, ["abcdefghijk4", "zz"], true); }'), " ";
    /// echo m('$GLOBALS["b"] = "abcdefghijk1"; $GLOBALS["c"] = "abcdefghijk1" . "";'), "\n";
    /// $junk = array_fill(0, 1000, str_repeat("x", 100));
    /// echo json_encode([$a, sv1(), ia1("abcdefghijk4"), $b, $c]), "\n";
    /// PHP);
    /// include $f;
    /// ```
    /// ```expect regime=jit-off
    /// 712 704 816 0
    /// [{"abcdefghijk1":"abcdefghijk2","7":["abcdefghijk5"]},"abcdefghijk3",true,"abcdefghijk1","abcdefghijk1"]
    /// ```
    fn interning<T>(&mut self, on: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = self.strings.interning(on);
        let done = f(self);
        self.strings.interning(outer);
        done
    }

    /// `f` under php's `ZEND_COMPILE_NO_PERSISTENT_CONSTANT_SUBSTITUTION`
    /// ([`Substitution::NoPersistent`]), which `zend_compile_params` sets
    /// over a parameter's default and restores after it.
    fn without_persistent_substitution<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = std::mem::replace(&mut self.substitution, Substitution::NoPersistent);
        let done = f(self);
        self.substitution = outer;
        done
    }

    /// A parameter's default as `zend_compile_params` folds it,
    /// `zend_const_expr_to_zval` with `allow_dynamic` under
    /// `ZEND_COMPILE_NO_PERSISTENT_CONSTANT_SUBSTITUTION`: no constant
    /// is substituted, a class's own included, so a default naming one
    /// stays the `IS_CONSTANT_AST` zval over the expression, which the
    /// engine evaluates on the pass that copies it and Reflection reads
    /// back as the name; a class's name is no constant, and
    /// `zend_try_compile_const_expr_resolve_class_name` resolves
    /// `Name::class`, and `self::class` and `parent::class` where the
    /// scope is known, to the string.
    ///
    /// ```php
    /// class P {}
    /// class C extends P {
    ///     const X = 1;
    ///     function f($a = self::class, $b = parent::class . '!', $c = Baz::class, $d = self::X, $e = [self::class], $f = PHP_INT_SIZE) {}
    /// }
    /// foreach ((new ReflectionMethod('C', 'f'))->getParameters() as $p) {
    ///     echo $p->isDefaultValueConstant() ? 'constant ' : 'value ', var_export($p->getDefaultValue(), true), "\n";
    /// }
    /// ```
    /// ```expect
    /// value 'C'
    /// value 'P!'
    /// value 'Baz'
    /// constant 1
    /// value array (
    ///   0 => 'C',
    /// )
    /// constant 8
    /// ```
    fn parameter_default(
        &mut self,
        parameter: &walk::Parameter,
    ) -> Result<Option<Zval>, CompileError> {
        parameter
            .default
            .map(|expr| self.const_expr_to_zval(expr))
            .transpose()
    }

    /// `zend_const_expr_to_zval` with `allow_dynamic`: the fold where
    /// `zend_eval_const_expr` folds the expression, and otherwise the
    /// tree ([`Self::dynamic_const_expr`]) as the `IS_CONSTANT_AST` the
    /// opline that reads it evaluates.
    fn const_expr_to_zval(&mut self, expr: tree::Expr) -> Result<Zval, CompileError> {
        if let Some(folded) = self.eval_const(expr)? {
            return Ok(folded);
        }
        let unevaluated = self.dynamic_const_expr(expr)?;
        Ok(self.unevaluated(unevaluated))
    }

    /// What `zend_compile_params` refuses about a parameter's compiled
    /// type: `void` and `never`, which no argument can be, and a
    /// default the type does not take, named by
    /// `zend_get_type_by_const`. A default that names a constant is
    /// checked when it is evaluated, and `null` under a written type is
    /// the nullable spelling. An int default under a type that takes
    /// `float` and not `int` is converted where it stands, as
    /// `zend_is_valid_default_value` converts it.
    ///
    /// ```php
    /// function f(void $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: void cannot be used as a parameter type in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(never $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: never cannot be used as a parameter type in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(
    ///     int $a,
    ///     float $arg = false
    /// ) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use bool as default value for parameter $arg of type float in %s on line 4
    /// ```
    ///
    /// ```php
    /// function f(iterable $x = 1) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use int as default value for parameter $x of type Traversable|array in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(float $a = 1, int|float $b = 2, ?array $c = [], int $d = PHP_INT_MAX, ?int $e = null) {
    ///     var_dump($a, $b, $c, $d, $e);
    /// }
    /// f();
    /// ```
    /// ```expect exit=0
    /// float(1)
    /// int(2)
    /// array(0) {
    /// }
    /// int(9223372036854775807)
    /// NULL
    /// ```
    fn parameter_type_refused(
        &self,
        ty: &Type,
        parameter: &walk::Parameter,
        folded: Option<&mut Zval>,
        force_nullable: bool,
    ) -> Result<(), CompileError> {
        let mask = ty.mask();
        if mask & types::MAY_BE_VOID != 0 {
            return Err(self.compile_error("void cannot be used as a parameter type"));
        }
        if mask & types::MAY_BE_NEVER != 0 {
            return Err(self.compile_error("never cannot be used as a parameter type"));
        }
        let Some(value) = folded else {
            return Ok(());
        };
        let kind = value.kind();
        if force_nullable || kind == ZvalType::ConstExpr {
            return Ok(());
        }
        if ty.contains_code(kind as u8) {
            return Ok(());
        }
        if ty.contains_code(ZvalType::Double as u8) && kind == ZvalType::Long {
            *value = Zval::double(double_of(*value));
            return Ok(());
        }
        let given = match kind {
            ZvalType::False | ZvalType::True => "bool",
            ZvalType::Long => "int",
            ZvalType::Double => "float",
            ZvalType::String => "string",
            ZvalType::Array => "array",
            ZvalType::Object => "object",
            _ => "null",
        };
        Err(self.compile_error(format!(
            "Cannot use {given} as default value for parameter ${} of type {}",
            String::from_utf8_lossy(&parameter.name),
            php_unit::type_to_string(ty)
        )))
    }

    /// What `zend_compile_params` refuses about one parameter, in php's
    /// words: a second variadic, a variadic with a default, and a
    /// promotion outside a constructor, in an abstract one or on a
    /// variadic.
    ///
    /// ```php
    /// abstract class A { abstract function __construct(public int $x); }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot declare promoted property in an abstract constructor in %s on line 2
    /// ```
    fn parameter_refused(
        &self,
        parameter: &walk::Parameter,
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
            if self.fn_flags.is_abstract {
                return Err(self
                    .compile_error("Cannot declare promoted property in an abstract constructor"));
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
            self.line = parameter.line;
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
    /// stands at opline zero until the prologue is known. The function
    /// is recorded in the enclosing op array's [`OpArray::nested`] as it
    /// stands now, past the `DECLARE_FUNCTION` or
    /// `DECLARE_LAMBDA_FUNCTION` and the binds `zend_begin_func_decl` and
    /// `zend_compile_closure_binding` emitted into it; a method or a hook
    /// after its class, in the order the class's members were written.
    fn begin_body(
        &mut self,
        function: ActiveFunction,
        this_in_scope: bool,
        fn_flags: FnFlags,
    ) -> Saved {
        self.loop_var_pushed();
        let index = |at: usize| u32::try_from(at).unwrap_or(u32::MAX);
        self.began(match function.at {
            crate::FunctionAt::Top(at) => Declares::Function(index(at)),
            crate::FunctionAt::Dynamic(at) => Declares::Dynamic(index(at)),
            crate::FunctionAt::Method { class, method } => Declares::Method {
                class: index(class),
                method: index(method),
            },
            crate::FunctionAt::Hook {
                class,
                property,
                kind,
            } => Declares::Hook {
                class: index(class),
                property: index(property),
                kind,
            },
        });
        Saved {
            return_type: self.return_type.take(),
            returns_by_ref: std::mem::take(&mut self.returns_by_ref),
            oplines: std::mem::take(&mut self.oplines),
            constants: std::mem::take(&mut self.constants),
            const_exprs: std::mem::take(&mut self.const_exprs),
            vars: std::mem::take(&mut self.vars),
            http_response_header: std::mem::take(&mut self.http_response_header),
            safepoints: std::mem::replace(&mut self.safepoints, entry_safepoints()),
            temporaries: std::mem::take(&mut self.temporaries),
            cache_slots: std::mem::replace(&mut self.cache_slots, self.first_slot),
            loops: std::mem::take(&mut self.loops),
            current_loop: self.current_loop.take(),
            jumps: std::mem::take(&mut self.jumps),
            labels: std::mem::take(&mut self.labels),
            try_catch: std::mem::take(&mut self.try_catch),
            unwind: std::mem::take(&mut self.unwind),
            fast_call_var: self.fast_call_var.take(),
            try_catch_offset: self.try_catch_offset.take(),
            active_function: self.active_function.replace(function),
            this_in_scope: std::mem::replace(&mut self.this_in_scope, this_in_scope),
            dynamic_func_defs: std::mem::take(&mut self.dynamic_func_defs),
            nested: std::mem::take(&mut self.nested),
            static_vars: std::mem::take(&mut self.static_vars),
            fn_flags: std::mem::replace(&mut self.fn_flags, fn_flags),
            numeric_dims: std::mem::take(&mut self.numeric_dims),
            // `zend_compile_func_decl_ex` sets the context's property
            // and hook to what it was handed, none but for a hook.
            active_property: self.active_property.take(),
            active_hook: self.active_hook.take(),
        }
    }

    /// `declares` begun at this place of the active op array, recorded
    /// with how far the op array has grown ([`Nested`]).
    fn began(&mut self, declares: Declares) {
        let count = |len: usize| u32::try_from(len).unwrap_or(u32::MAX);
        let grown = Grown {
            oplines: count(self.oplines.len()),
            constants: count(self.constants.len()),
            vars: count(self.vars.len()),
            dynamic_func_defs: count(self.dynamic_func_defs.len()),
            loops: count(self.loops.len()),
        };
        self.nested.push(Nested { declares, grown });
    }

    /// `zend_oparray_context_end`: the enclosing op array's state back
    /// on the emitter.
    fn end_body(&mut self, saved: Saved) {
        self.loop_vars = self.loop_vars.saturating_sub(1);
        self.oplines = saved.oplines;
        self.constants = saved.constants;
        self.const_exprs = saved.const_exprs;
        self.vars = saved.vars;
        self.http_response_header = saved.http_response_header;
        self.safepoints = saved.safepoints;
        self.temporaries = saved.temporaries;
        self.cache_slots = saved.cache_slots;
        self.loops = saved.loops;
        self.current_loop = saved.current_loop;
        self.jumps = saved.jumps;
        self.labels = saved.labels;
        self.try_catch = saved.try_catch;
        self.unwind = saved.unwind;
        self.fast_call_var = saved.fast_call_var;
        self.try_catch_offset = saved.try_catch_offset;
        // The string made for the body's name is the declaration's.
        if let Some(ActiveFunction {
            at, own: Some(own), ..
        }) = std::mem::replace(&mut self.active_function, saved.active_function)
        {
            self.own_names.push((crate::Named::Function(at), own));
        }
        self.this_in_scope = saved.this_in_scope;
        self.return_type = saved.return_type;
        self.returns_by_ref = saved.returns_by_ref;
        self.dynamic_func_defs = saved.dynamic_func_defs;
        self.nested = saved.nested;
        self.static_vars = saved.static_vars;
        self.fn_flags = saved.fn_flags;
        self.numeric_dims = saved.numeric_dims;
        self.active_property = saved.active_property;
        self.active_hook = saved.active_hook;
    }

    /// The file's name an op array carries, `op_array->filename`: the
    /// room's ([`crate::Room`]) written over where it is as long, which
    /// an `eval` of one line in a loop always is, else made.
    fn file_name(&mut self) -> Name {
        match self.room_filename.take() {
            Some(mut name) if name.0.len() == self.filename.len() => {
                name.0.copy_from_slice(self.filename);
                name
            }
            _ => Name(self.filename.into()),
        }
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
        self.close(return_one)?;
        let filename = self.file_name();
        Ok(self.op_array(filename, line_start, line_end))
    }

    /// [`Self::finish`]'s passes over the op array the emitter holds,
    /// before its tables are taken.
    fn close(&mut self, return_one: bool) -> Result<(), CompileError> {
        self.final_return(return_one)?;
        if self.extended_info {
            extended::update_extended_stmts(&mut self.oplines);
        }
        self.resolve_jumps()?;
        if self.fn_flags.generator {
            // `pass_two`'s `ZEND_RETURN` arm (`Z/Zend/zend_opcode.c:1213`).
            for op in &mut self.oplines {
                if matches!(op.opcode, Opcode::Return | Opcode::ReturnByRef) {
                    op.opcode = Opcode::GeneratorReturn;
                }
            }
        }
        resolve_fast_calls(&mut self.oplines, &self.try_catch);
        unused_assert_checks(&mut self.oplines);
        headers_numbered(&self.oplines, &self.safepoints)?;
        swap_commutative(&mut self.oplines);
        Ok(())
    }

    /// The op array the emitter holds, its tables taken, under `filename`:
    /// inlined, so the main op array's fields are written where the unit
    /// lays them ([`unit`]) rather than copied there.
    #[allow(clippy::inline_always)] // measured: one copy of the op array fewer per compile
    #[inline(always)]
    fn op_array(&mut self, filename: Name, line_start: u32, line_end: u32) -> OpArray {
        let mut oplines = std::mem::take(&mut self.oplines);
        let mut live_ranges = crate::live::live_ranges(&mut oplines, self.temporaries);
        let vars = std::mem::take(&mut self.vars);
        let num_cvs = u32::try_from(vars.len()).unwrap_or(u32::MAX);
        crate::live::fold_slots(&mut oplines, &mut live_ranges, num_cvs);
        let literals = std::mem::take(&mut self.constants);
        let cache_size = defaults_cached(&oplines, &literals, self.cache_slots);
        OpArray {
            oplines,
            literals,
            const_exprs: std::mem::take(&mut self.const_exprs),
            vars,
            num_cvs,
            num_tmps: self.temporaries,
            try_catch: std::mem::take(&mut self.try_catch),
            live_ranges,
            safepoints: std::mem::take(&mut self.safepoints),
            filename,
            line_start,
            line_end,
            cache_size,
            cache_room: cache_size,
            runtime_cache: php_unit::MapPtrOffset::default(),
            dynamic_func_defs: std::mem::take(&mut self.dynamic_func_defs),
            nested: std::mem::take(&mut self.nested),
            bound_vars: std::mem::take(&mut self.static_vars),
            implicit_uses: 0,
            loops: u32::try_from(self.loops.len()).unwrap_or(u32::MAX),
            no_receiver: self.fn_flags.is_static,
            strict_types: self.typing == Typing::Strict,
            return_mask: self.return_type.as_ref().map_or(0, Type::mask),
            numeric_dims: std::mem::take(&mut self.numeric_dims),
        }
    }

    /// `zend_compile_class_decl`: the declaration goes into the unit's
    /// classes, and whether an opline is emitted beside it is php's
    /// early-binding question. php binds a top-level class at compile
    /// time (`zend_hash_add_ptr(CG(class_table), …)` under `toplevel`)
    /// when it implements no interface, uses no trait, its name is not
    /// yet in the table and either it has no parent or names one
    /// `CG(class_table)` already holds, which `zend_try_early_bind` then
    /// links; the oracle's dump of `class M {} echo 1;` is the `ECHO`
    /// and the `RETURN` alone, and so is `class P {} class C extends P {}
    /// echo 1;`. Everything else carries a `ZEND_DECLARE_CLASS` opline,
    /// or the `ZEND_DECLARE_CLASS_DELAYED` opcache emits for a top-level
    /// one, where it was written, in whatever op array it stands in,
    /// which binds the class as the program serves ([`Self::declare_class`]):
    /// `class C extends P {} class P {}` works because `P` went into the
    /// table while the file compiled, a class written inside an `if`, a
    /// loop or a function body is declared only where that statement
    /// runs, and a second declaration of a name the file already bound
    /// is php's redeclaration fatal when its opline runs. The class's name is
    /// `zend_prefix_with_ns` over the written one: `A\M` for `class M`
    /// under `namespace A;`, the name written where no namespace stands,
    /// so a fetch [`Self::class_name`] resolves in the same namespace
    /// names it, `__CLASS__` folds to it and php's fatals name it. A
    /// class written while another's body compiles, in a method or a
    /// closure of it, is php's first refusal. The members are
    /// `zend_compile_prop_decl` and `zend_compile_class_const_decl`: a
    /// name declared twice is php's fatal in php's words, at the
    /// declarator's line, and a default or a value is kept as the
    /// constant expression it was written as (design §4.7).
    ///
    /// ```php
    /// class C {
    ///     function f() {
    ///         class D {}
    ///     }
    /// }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class declarations may not be nested in %s on line 4
    /// ```
    ///
    /// A function written in a method takes no class with it, so a class
    /// it declares is not nested:
    ///
    /// ```php
    /// class C {
    ///     function m() {
    ///         function g() { class D { const K = 1; } }
    ///         g();
    ///         return D::K;
    ///     }
    /// }
    /// var_dump((new C)->m());
    /// ```
    /// ```expect
    /// int(1)
    /// ```
    ///
    /// A class written in a block exists once its statement is executed,
    /// and not before; which of two declarations of one name is bound is
    /// the arm executed:
    ///
    /// ```php
    /// var_dump(class_exists('C', false));
    /// if (PHP_INT_SIZE > 4) {
    ///     class C { const K = 'wide'; }
    /// } else {
    ///     class C { const K = 'narrow'; }
    /// }
    /// var_dump(C::K);
    /// function f() {
    ///     class G extends C {}
    ///     return G::K;
    /// }
    /// var_dump(class_exists('G', false), f(), get_parent_class(new G));
    /// ```
    /// ```expect
    /// bool(false)
    /// string(4) "wide"
    /// bool(false)
    /// string(4) "wide"
    /// string(1) "C"
    /// ```
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
        if self.active_class.is_some() {
            return Err(self.compile_error("Class declarations may not be nested"));
        }
        // `toplevel`: a statement of the file's own list or a bracketed
        // namespace's, which `zend_compile_top_stmt` hands over.
        let toplevel = self.depth == 1;
        let name = self.declare_symbol(Symbol::Class, &class.name)?;
        let line = self.line;
        let Entered {
            at,
            parent,
            hooked_variance_checks,
            own,
        } = self.class_entry(class, &name, false)?;
        if let Some(decl) = self.classes.get_mut(at) {
            decl.flags.is_top_level = toplevel;
        }
        if toplevel {
            let at = u32::try_from(at).unwrap_or(u32::MAX);
            self.top_level.push(TopLevel::Class(at));
        }
        // `zend_compile_class_decl` decides once the body has compiled,
        // so an interface `add_stringable_interface` gave the class
        // counts, and an enum, which `zend_enum_add_interfaces` gave
        // `UnitEnum`, is never bound early; nor is a class that uses a
        // trait, whose methods only the link copies in. A name the table
        // already holds, the file's, the engine's or the request's, fails
        // `zend_hash_add_ptr` before any link is tried
        // (`register_early_bound_ce`), and the opline raises the
        // redeclaration when it runs. Nor is one whose `set` hook's
        // parameter type the file could not hold to its property's,
        // which only the link holds it to.
        let unlinked_free = hooked_variance_checks == 0
            && self
                .classes
                .get(at)
                .is_some_and(|decl| decl.interfaces.is_empty() && decl.traits.is_empty());
        let early = toplevel
            && unlinked_free
            && self.bound_class(&name).is_none()
            && !self.request_classes.holds(self.arena.lowercase(&name))
            && match &parent {
                None => true,
                Some(parent) => self.bound_class(parent).is_some(),
            };
        // A class with no parent is bound early too, and linked as
        // `zend_compile_class_decl` links it, which is where its
        // `#[\Override]` marks are held to what it overrides.
        if early && self.try_early_bind(at)? {
            self.bound.push(at);
            let class = TopLevel::Class(u32::try_from(at).unwrap_or(u32::MAX));
            self.landed.push((class, self.diagnostics.len()));
            return Ok(());
        }
        let delayed = toplevel && unlinked_free;
        self.declare_class(at, &name, own, parent.as_deref(), line, delayed)
    }

    /// `zend_compile_class_decl` from its `ce` to its body, which a
    /// named class and an anonymous one share: the parent and the
    /// interfaces resolved, the traits, the attributes, then the
    /// declaration pushed onto [`Emit::classes`] under `name` and its
    /// constants, properties and methods compiled into it in the order
    /// written, as `zend_compile_stmt` compiles the member list, so each
    /// member sees what stands above it in the class's tables and
    /// nothing below. Where it stands and its parent, resolved, are what
    /// the caller binds it by. An anonymous class is filed under its lowered name
    /// ([`Unit::runtime_keys`]) as it is pushed, before a method can
    /// declare another, as `zend_generate_anon_class_name` took the
    /// file's count before the body compiled.
    ///
    /// The methods' bodies go into the unit after those of every class a
    /// method declared, which is `CG(class_table)`'s order, each class
    /// added as its declaration completes, and so the order php's dump
    /// walks.
    ///
    /// The class's own attributes compile with the class as the active
    /// one, so `self` in them is the class, an anonymous one's too:
    ///
    /// A promoted property is declared as its constructor compiles, so
    /// it stands before a property written below the constructor; and a
    /// class a member declares takes the file's count in the order the
    /// members are written, a hook's among them:
    ///
    /// ```php
    /// class A { function __construct(public $a = 0) {} public $b = 1; }
    /// var_dump(new A);
    /// $o = new class { function m() { return new class {}; } public $h { get => new class {}; } };
    /// foreach ([$o->h, $o->m()] as $x) {
    ///     echo substr(get_class($x), strrpos(get_class($x), '$')), "\n";
    /// }
    /// ```
    /// ```expect
    /// object(A)#1 (2) {
    ///   ["a"]=>
    ///   int(0)
    ///   ["b"]=>
    ///   int(1)
    /// }
    /// $2
    /// $1
    /// ```
    ///
    /// ```php
    /// class C3 { private const FOO = 'foo'; static function foo() { return new #[A1(self::class, self::FOO)] class() { private const FOO = 'bar'; }; } }
    /// $r = new ReflectionObject(C3::foo());
    /// $a = $r->getAttributes()[0]->getArguments();
    /// var_dump($a[0] == $r->getName(), $a[1]);
    /// ```
    /// ```expect
    /// bool(true)
    /// string(3) "bar"
    /// ```
    fn class_entry(
        &mut self,
        class: &Class,
        name: &[u8],
        anonymous: bool,
    ) -> Result<Entered, CompileError> {
        let name = name.to_vec();
        let line = self.line;
        let parent = match &class.parent {
            Some(written) => Some(self.inherited_name(written, "class name")?),
            None => None,
        };
        let mut interfaces = Vec::with_capacity(class.interfaces.len());
        for written in &class.interfaces {
            interfaces.push(self.inherited_name(written, "interface name")?);
        }
        let (traits, adaptations) = self.trait_uses(class, &name)?;
        self.line = line;
        let backing = self.enum_backing(class, &name)?;
        if class.kind == ClassKind::Enum {
            // `zend_enum_add_interfaces`: every enum is a `UnitEnum` and
            // a backed one a `BackedEnum`, after the ones written, so no
            // enum is ever bound early.
            interfaces.push(b"UnitEnum".to_vec());
            if backing != EnumBacking::Pure {
                interfaces.push(b"BackedEnum".to_vec());
            }
        }
        // `zend_lookup_class_ex(ce->parent_name, NULL,
        // ZEND_FETCH_CLASS_NO_AUTOLOAD)` stands before the class itself
        // goes into the table, so a class naming itself as its parent
        // finds nothing and binds as the program serves.
        // `CG(active_class_entry) = ce` before `zend_compile_attributes`,
        // so `self` in an attribute of the class is the class, an
        // anonymous one's inside a method among them.
        let at = self.classes.len();
        // `zend_compile_class_decl` at its place in the active op array.
        self.began(Declares::Class(u32::try_from(at).unwrap_or(u32::MAX)));
        let enclosing = (self.active_class)
            .replace(ActiveClass::entered(&name, class.kind, at, anonymous, None));
        let enclosing_parent = std::mem::replace(&mut self.active_parent, parent.clone());
        let attributes = self.class_attributes(class, &name, class.line);
        self.active_class = enclosing;
        self.active_parent = enclosing_parent;
        let (attributes, dynamic_properties) = attributes?;
        let is_enum = class.kind == ClassKind::Enum;
        let decl = ClassDecl {
            name: Name(name.clone().into_boxed_slice()),
            kind: class.kind,
            flags: class_flags(class, dynamic_properties, anonymous),
            parent: parent
                .as_ref()
                .map(|parent| Name(parent.clone().into_boxed_slice())),
            interfaces: interfaces
                .iter()
                .map(|i| Name(i.clone().into_boxed_slice()))
                .collect(),
            implementors: Vec::new(),
            aliases: Vec::new(),
            traits,
            adaptations,
            backing,
            constants: Vec::new(),
            properties: enum_properties(class.kind, backing),
            methods: Vec::new(),
            attributes,
            doc_comment: class.doc_comment.clone().map(Vec::into_boxed_slice),
            file: Name(self.filename.into()),
            line,
            end_line: class.end_line,
            object_size: 0,
            object_beside: Box::default(),
            attribute_validator: None,
        };
        let own = self.own_string(&name, class_sizing(anonymous))?.as_str();
        (self.own_names).extend(own.map(|own| (crate::Named::Class(at), own)));
        self.active_class = Some(ActiveClass::entered(&name, class.kind, at, anonymous, own));
        self.active_parent.clone_from(&parent);
        // The class takes its place `at` in the unit before its body, so
        // a class a member declares, `new class {}` in a method or a
        // hook, takes the next place.
        self.classes.push(decl);
        if anonymous {
            self.runtime_keys.push((
                Name(name.to_ascii_lowercase().into_boxed_slice()),
                u32::try_from(at).unwrap_or(u32::MAX),
            ));
        }
        let mark = self.methods.len();
        for member in &class.body {
            match member {
                walk::ClassStmt::Constant(constant) => self.class_constant(at, constant, &name)?,
                walk::ClassStmt::Property(property) => self.class_property(at, property, &name)?,
                walk::ClassStmt::Method(method) => {
                    self.line = method.function.line;
                    self.method_decl(at, method)?;
                }
            }
        }
        self.own_bodies_last(mark, at);
        let (hooked_variance_checks, own) = (self.active_class.take())
            .map_or((0, None), |active| {
                (active.hooked_variance_checks, active.own)
            });
        if let Some(decl) = self.classes.get_mut(at) {
            decl.flags.hooked_variance = hooked_variance_checks > 0;
        }
        self.active_parent = None;
        self.member_statements(&class.statement_lines);
        self.line = line;
        if is_enum {
            self.enum_funcs_registered(at)?;
        }
        // `zend_hash_add_ptr(CG(class_table), ...)` after the body
        // compiled: the class enters the table after every class one of
        // its members declared.
        self.completed.push(u32::try_from(at).unwrap_or(u32::MAX));
        Ok(Entered {
            at,
            parent,
            hooked_variance_checks,
            own,
        })
    }

    /// The method bodies compiled since `mark` with those of the class
    /// at `at` last, after those of every class one of them declared,
    /// which `CG(class_table)` added as its declaration completed.
    fn own_bodies_last(&mut self, mark: usize, at: usize) {
        let own = u32::try_from(at).unwrap_or(u32::MAX);
        let (mut bodies, nested): (Vec<_>, Vec<_>) = self
            .methods
            .drain(mark..)
            .partition(|body| body.class == own);
        self.methods.extend(nested);
        self.methods.append(&mut bodies);
    }

    /// `zend_compile_new`'s anonymous arm, `zend_compile_class_decl`
    /// under `ZEND_ACC_ANON_CLASS` (`Z/Zend/zend_compile.c:9247`): the
    /// class takes the name `zend_generate_anon_class_name` makes, its
    /// parent's name or its first interface's or `class`, then
    /// `@anonymous`, a NUL, the file, `:`, the line of its `class` and
    /// `$` with the file's count of runtime definition keys so far in
    /// hex; it compiles as any class does, nested in another's method
    /// or not, and is never bound while the file compiles. Where it was
    /// written stands `ZEND_DECLARE_ANON_CLASS`, its lowered name in
    /// `op1`, the parent's in `op2` where it has one and a cache slot in
    /// `extended_value`, writing the class into the `VAR` returned for
    /// the `NEW` after it.
    ///
    /// ```php
    /// class P { function __construct(public $a) {} }
    /// interface I {}
    /// $o = new class(1) extends P implements I { public $b = 2; };
    /// var_dump($o, $o instanceof P, $o instanceof I);
    /// var_dump(strstr(get_class($o), "\0", true), get_class(new class implements I {}) !== get_class($o));
    /// ```
    /// ```expect
    /// object(P@anonymous)#1 (2) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["b"]=>
    ///   int(2)
    /// }
    /// bool(true)
    /// bool(true)
    /// string(11) "P@anonymous"
    /// bool(true)
    /// ```
    ///
    /// One declaration is one class, however many times it runs, and
    /// its name says where it was written:
    ///
    /// ```php
    /// function make() { return new class { public $n = 0; }; }
    /// $a = make(); $b = make();
    /// var_dump(get_class($a) === get_class($b), $a == $b);
    /// var_dump(str_replace(__FILE__, 'F', strstr(get_class($a), "\0")) === "\0F:2\$0");
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// ```
    ///
    /// A class's method may declare one, which takes the file's count
    /// before the class around it does:
    ///
    /// ```php
    /// $o = new class {
    ///     function inner() { return new class {}; }
    /// };
    /// $p = new class {};
    /// foreach ([$o, $o->inner(), $p] as $x) {
    ///     echo substr(get_class($x), strrpos(get_class($x), '$')), "\n";
    /// }
    /// ```
    /// ```expect
    /// $0
    /// $1
    /// $2
    /// ```
    ///
    /// A property hook may declare one, with properties and hooks of its
    /// own, and stays the hook it was around it:
    ///
    /// ```php
    /// class Base { public $y { get => 1; } }
    /// class Test extends Base { public $y { get => [new class { public $i = 2; public $j { get => __PROPERTY__; } }, parent::$y::get()]; } }
    /// $y = (new Test)->y;
    /// var_dump($y[1], $y[0]->i, $y[0]->j);
    /// ```
    /// ```expect
    /// int(1)
    /// int(2)
    /// string(1) "j"
    /// ```
    fn anonymous_class(&mut self, id: NodeId<tree::AnonClassDecl>) -> Result<u32, CompileError> {
        let class = self.walk_anon_class(id)?;
        self.line = class.line;
        let prefix = match (&class.parent, class.interfaces.first()) {
            (Some(parent), _) => self.inherited_name(parent, "class name")?,
            (None, Some(interface)) => self.inherited_name(interface, "interface name")?,
            (None, None) => b"class".to_vec(),
        };
        let mut name = prefix;
        name.extend_from_slice(b"@anonymous\0");
        name.extend_from_slice(self.filename);
        name.extend_from_slice(format!(":{}${:x}", class.line, self.keys_made()).as_bytes());
        // `CG(active_class_entry)` is the anonymous class while its body
        // compiles and the enclosing one's again after, and so is the
        // property hook a `new class` in a hook's body stands in, which
        // the class's own properties would otherwise leave behind.
        let enclosing = (
            self.active_class.take(),
            self.active_parent.take(),
            self.active_property.take(),
            self.active_hook.take(),
        );
        let entered = self.class_entry(&class, &name, true);
        (
            self.active_class,
            self.active_parent,
            self.active_property,
            self.active_hook,
        ) = enclosing;
        let Entered {
            at, parent, own, ..
        } = entered?;
        // "Reset lineno for final opcodes and errors".
        self.line = class.line;
        let (op2_type, op2) = match parent {
            Some(parent) => {
                let lowered = self.string(self.arena.lowercase(&parent))?;
                (OperandKind::Const, self.literal(lowered))
            }
            None => (OperandKind::Unused, 0),
        };
        let lowered = self.class_key(at, own, &name)?;
        let op1 = self.literal(lowered);
        let extended_value = self.cache_slot();
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::DeclareAnonClass,
            op1_type: OperandKind::Const,
            op1,
            op2_type,
            op2,
            out_type: OperandKind::Var,
            out,
            extended_value,
            ..Opline::default()
        });
        Ok(out)
    }

    /// The name of `CG(active_class_entry)`, and none outside a class
    /// body.
    fn active_name(&self) -> Option<Vec<u8>> {
        self.active_class.as_ref().map(|class| class.name.clone())
    }

    /// `zend_begin_method_decl`'s `zend_hash_add_ptr` into the active
    /// class's `function_table`, or the method's body done.
    fn set_declaring(&mut self, declaring: Option<Declaring>) {
        if let Some(active) = self.active_class.as_mut() {
            active.declaring = declaring;
        }
    }

    /// What `zend_compile_params` and `zend_compile_attributes` left on
    /// the op array of the method whose body is compiling, read by a call
    /// to it from its own body. The method's prologue is the first to
    /// record it: a closure's, one written in a parameter's default
    /// among them, records nothing, and a function declared in the body
    /// or a member of a class declared there compiles under another
    /// active class, or none.
    fn declared_signature(&mut self, parameters: &[Parameter], attributes: &[Attribute]) {
        if self.fn_flags.closure {
            return;
        }
        let Some(declaring) = self
            .active_class
            .as_mut()
            .and_then(|active| active.declaring.as_mut())
        else {
            return;
        };
        if declaring.signature.is_none() {
            declaring.signature = Some(Signature {
                parameters: parameters.to_vec(),
                flagged: attributes::deprecated_or_no_discard(attributes),
            });
        }
    }

    /// `zend_hash_find_ptr(&ce->function_table, lcname)` over the class
    /// declared at `at` while it compiles: the methods compiled so far,
    /// and the one whose body is compiling where `at` is the active
    /// class, which `zend_begin_method_decl` added before its body.
    fn table_method(&self, at: usize, name: &[u8]) -> Option<TableMethod> {
        let decl = self.classes.get(at)?;
        if let Some(method) = decl
            .methods
            .iter()
            .find(|m| m.function.name.0.eq_ignore_ascii_case(name))
        {
            return Some(TableMethod {
                visibility: method.visibility,
                is_final: method.is_final,
                flagged: attributes::deprecated_or_no_discard(&method.function.attributes),
                callee: Callee::of(&method.function.parameters),
            });
        }
        let active = self
            .active_class
            .as_ref()
            .filter(|active| active.at == at)?;
        let declaring = active
            .declaring
            .as_ref()
            .filter(|declaring| declaring.name.eq_ignore_ascii_case(name))?;
        let signature = declaring.signature.as_ref()?;
        Some(TableMethod {
            visibility: declaring.visibility,
            is_final: declaring.is_final,
            flagged: signature.flagged,
            callee: Callee::of(&signature.parameters),
        })
    }

    /// `CG(rtd_key_counter)`: the keys made before this source and by it
    /// so far, which the next key or anonymous class name takes.
    fn keys_made(&self) -> u32 {
        let made = u32::try_from(self.runtime_keys.len()).unwrap_or(u32::MAX);
        self.keys_before.saturating_add(made)
    }

    /// The declaration of `CG(active_class_entry)` once its members have
    /// gone onto [`Emit::classes`], and none before or outside a body.
    fn active_decl(&self) -> Option<&ClassDecl> {
        self.classes.get(self.active_class.as_ref()?.at)
    }

    /// `ZEND_ACC_TRAIT` on `CG(active_class_entry)`: the body being
    /// compiled is a trait's.
    fn in_trait(&self) -> bool {
        self.active_class
            .as_ref()
            .is_some_and(|class| class.kind == ClassKind::Trait)
    }

    /// The constant of the name `CG(active_class_entry)->constants_table`
    /// holds while a member compiles, the ones declared above it, with
    /// the value it was written as, where `zend_try_ct_eval_class_const`
    /// may substitute it: none of a trait's, since
    /// `zend_verify_ct_const_access` refuses a constant whose class is a
    /// trait, which a composing class takes as its own at the link; no
    /// enum case, whose value is the enum's instance for it
    /// (`ZEND_AST_CONST_ENUM_INIT`) and never the scalar it is backed by;
    /// and no `ZEND_ACC_DEPRECATED` one, fetched as the program serves,
    /// where its deprecation is raised.
    fn foldable_constant(&self, name: &[u8]) -> Option<&ConstExpr> {
        if self.in_trait() {
            return None;
        }
        (self.active_decl()?.constants.iter())
            .find(|c| *c.name.0 == *name)
            .filter(|c| !c.is_case && !attributes::is_deprecated(&c.attributes))
            .map(|c| &c.value)
    }

    /// `zend_compile_use_trait` (`Z/Zend/zend_compile.c:9137`) over each
    /// `use` of the body: every name resolved as a trait name, which an
    /// interface may not take, and each adaptation with its names
    /// resolved, an alias held to the modifiers
    /// `zend_check_trait_alias_modifiers` allows. The names are
    /// `ce->trait_names`; the adaptations go to the link as written.
    ///
    /// ```php
    /// interface I { use T; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use traits inside of interfaces. T is used in I in %s on line 2
    /// ```
    ///
    /// ```php
    /// trait T { function f() {} }
    /// class C { use T { f as static g; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use "static" as method modifier in trait alias in %s on line 3
    /// ```
    ///
    /// ```php
    /// class C { use static; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use "static" as trait name, as it is reserved in %s on line 2
    /// ```
    ///
    /// ```php
    /// trait T { function f() {} }
    /// class C { use T { static::f insteadof T; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use "static" as trait name, as it is reserved in %s on line 3
    /// ```
    fn trait_uses(
        &mut self,
        class: &Class,
        name: &[u8],
    ) -> Result<(Vec<Name>, Vec<TraitAdaptation>), CompileError> {
        let mut traits = Vec::new();
        let mut adaptations = Vec::new();
        let as_name = |bytes: Vec<u8>| Name(bytes.into_boxed_slice());
        for used in &class.traits {
            self.line = used.line;
            for written in &used.names {
                if class.kind == ClassKind::Interface {
                    return Err(self.compile_error(format!(
                        "Cannot use traits inside of interfaces. {} is used in {}",
                        String::from_utf8_lossy(written),
                        String::from_utf8_lossy(name)
                    )));
                }
                traits.push(as_name(self.inherited_name(written, "trait name")?));
            }
            for adaptation in &used.adaptations {
                adaptations.push(match adaptation {
                    walk::Adaptation::Insteadof {
                        trait_name,
                        method,
                        excluded,
                        line,
                    } => {
                        self.line = *line;
                        let trait_name = as_name(self.inherited_name(trait_name, "trait name")?);
                        let mut names = Vec::with_capacity(excluded.len());
                        for written in excluded {
                            names.push(as_name(self.inherited_name(written, "trait name")?));
                        }
                        TraitAdaptation::Insteadof {
                            trait_name,
                            method: as_name(method.clone()),
                            excluded: names,
                        }
                    }
                    walk::Adaptation::As {
                        trait_name,
                        method,
                        modifier,
                        alias,
                        line,
                    } => {
                        self.line = *line;
                        let modifier = match modifier {
                            None => None,
                            Some(walk::AsModifier::Visibility(visibility)) => {
                                Some(AliasModifier::Visibility(*visibility))
                            }
                            Some(walk::AsModifier::Final) => Some(AliasModifier::Final),
                            Some(walk::AsModifier::Readonly) => {
                                return Err(self.compile_error(
                                    "Cannot use the readonly modifier on a method",
                                ));
                            }
                            Some(walk::AsModifier::Static) => {
                                return Err(self.compile_error(
                                    "Cannot use \"static\" as method modifier in trait alias",
                                ));
                            }
                            Some(walk::AsModifier::Abstract) => {
                                return Err(self.compile_error(
                                    "Cannot use \"abstract\" as method modifier in trait alias",
                                ));
                            }
                        };
                        let trait_name = match trait_name {
                            Some(written) => {
                                Some(as_name(self.inherited_name(written, "trait name")?))
                            }
                            None => None,
                        };
                        TraitAdaptation::As {
                            method: TraitMethodRef {
                                trait_name,
                                method: as_name(method.clone()),
                            },
                            modifier,
                            alias: alias.clone().map(as_name),
                        }
                    }
                });
            }
        }
        Ok((traits, adaptations))
    }

    /// `zend_compile_enum_backing_type` (`Z/Zend/zend_compile.c:9221`):
    /// `int` or `string` after an enum's `:`, and php's compile error for
    /// any other type; a pure enum, and every other kind, is `Pure`.
    ///
    /// ```php
    /// enum E: float {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Enum backing type must be int or string, float given in %s on line 2
    /// ```
    fn enum_backing(&mut self, class: &Class, name: &[u8]) -> Result<EnumBacking, CompileError> {
        let _ = name;
        let Some(written) = class.backing else {
            return Ok(EnumBacking::Pure);
        };
        match self.typename(written, false)?.0 {
            Type::Int => Ok(EnumBacking::Int),
            Type::String => Ok(EnumBacking::String),
            other => Err(self.compile_error(format!(
                "Enum backing type must be int or string, {} given",
                php_unit::type_to_string(&other)
            ))),
        }
    }

    /// A class constant's value, or an enum case's
    /// (`zend_compile_enum_case`, `Z/Zend/zend_compile.c:9460`): a case
    /// of a backed enum must be written with its value and one of a pure
    /// enum without, each refusal php's compile error at the case's
    /// line; a pure case's value is `null`, which no reader takes for a
    /// backing value.
    ///
    /// ```php
    /// enum Suit: string { case Hearts = 'H'; case Spades; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Case Spades of backed enum Suit must have a value in %s on line 2
    /// ```
    ///
    /// ```php
    /// enum Suit { case Hearts = 1; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Case Hearts of non-backed enum Suit must not have a value in %s on line 2
    /// ```
    fn case_or_constant(
        &mut self,
        backing: EnumBacking,
        constant: &walk::Constant,
        name: &[u8],
    ) -> Result<ConstExpr, CompileError> {
        if !constant.is_case {
            let Some(value) = constant.value else {
                return Err(self.internal("a class constant with no value"));
            };
            return self.const_expr(value);
        }
        let backed = backing != EnumBacking::Pure;
        match (backed, constant.value) {
            (true, None) => Err(self.compile_error(format!(
                "Case {} of backed enum {} must have a value",
                String::from_utf8_lossy(&constant.name),
                String::from_utf8_lossy(name)
            ))),
            (false, Some(_)) => Err(self.compile_error(format!(
                "Case {} of non-backed enum {} must not have a value",
                String::from_utf8_lossy(&constant.name),
                String::from_utf8_lossy(name)
            ))),
            (true, Some(value)) => self.const_expr(value),
            (false, None) => Ok(ConstExpr::Null),
        }
    }

    /// The methods `zend_enum_register_funcs` gives the enum at `at` of
    /// the unit's classes: `cases`, and for a backed one `from` and
    /// `tryFrom`, after its own methods. php registers them as the enum
    /// links, when its `DECLARE_CLASS` runs, so a method of its own
    /// under one of those names compiles, and the link refuses the two
    /// as php's `Cannot redeclare` ([`php_link::link`]). The magic
    /// methods php keeps off an enum are the linker's to refuse too, as
    /// `zend_verify_enum` refuses them when the class links.
    fn enum_funcs_registered(&mut self, at: usize) -> Result<(), CompileError> {
        let Some(decl) = self.classes.get_mut(at) else {
            return Err(self.internal("an enum the unit has not"));
        };
        decl.methods.extend(php_unit::enum_methods(decl.backing));
        Ok(())
    }

    /// `zend_compile_class_const_decl`: one declarator at a time, at its
    /// own line, its value kept as the constant expression it was
    /// written as (design §4.7), and a name the group declares twice
    /// php's fatal in php's words.
    ///
    /// ```php
    /// class A { final private const X = 1; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Private constant A::X cannot be final as it is not visible to other classes in %s on line 2
    /// ```
    ///
    /// ```php
    /// interface A { protected const FOO = 10; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Access type for interface constant A::FOO must be public in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { const CLASS = 1; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: A class constant must not be called 'class'; it is reserved for class name fetching in %s on line 2
    /// ```
    fn class_constant(
        &mut self,
        at: usize,
        constant: &walk::Constant,
        name: &[u8],
    ) -> Result<(), CompileError> {
        let Some((kind, backing)) = self.classes.get(at).map(|decl| (decl.kind, decl.backing))
        else {
            return Err(self.internal("a constant of a class the unit has not"));
        };
        self.line = constant.line;
        let member = format!(
            "{}::{}",
            php_unit::decl::shown(name),
            String::from_utf8_lossy(&constant.name)
        );
        let ty = match constant.ty {
            Some(written) => Some(self.class_constant_type(written, &member)?),
            None => None,
        };
        if constant.visibility == Visibility::Private && constant.is_final {
            return Err(self.compile_error(format!(
                "Private constant {member} cannot be final as it is not visible to other classes"
            )));
        }
        self.folded_string = None;
        let mut value = self.case_or_constant(backing, constant, name)?;
        if !constant.is_case {
            value = self.held_value(value);
        }
        if let Some(ty) = &ty
            && let Err(kind) = valid_default(ty, &mut value)
        {
            return Err(self.compile_error(format!(
                "Cannot use {} as value for class constant {member} of type {}",
                zval_type_name(kind),
                php_unit::type_to_string(ty)
            )));
        }
        // `zend_declare_typed_class_constant`'s first test.
        if kind == ClassKind::Interface && constant.visibility != Visibility::Public {
            return Err(self.compile_error(format!(
                "Access type for interface constant {member} must be public"
            )));
        }
        if constant.name.eq_ignore_ascii_case(b"class") {
            return Err(self.compile_error(
                "A class constant must not be called 'class'; it is reserved for class name fetching",
            ));
        }
        if (self.classes.get(at))
            .is_some_and(|decl| decl.constants.iter().any(|c| *c.name.0 == *constant.name))
        {
            return Err(self.compile_error(format!(
                "Cannot redefine class constant {}::{}",
                php_unit::decl::shown(name),
                String::from_utf8_lossy(&constant.name)
            )));
        }
        let attributes = self.case_attributes(constant)?;
        let Some(decl) = self.classes.get_mut(at) else {
            return Err(self.internal("a constant of a class the unit has not"));
        };
        decl.constants.push(ClassConstDecl {
            name: Name(constant.name.clone().into_boxed_slice()),
            visibility: constant.visibility,
            is_final: constant.is_final,
            is_case: constant.is_case,
            ty,
            value,
            attributes,
            doc_comment: constant.doc_comment.clone().map(Vec::into_boxed_slice),
        });
        Ok(())
    }

    /// The type of a typed class constant, `zend_compile_typename` in
    /// `zend_compile_class_const_decl`: any type but one that takes
    /// `callable`, `void` or `never` short of `mixed`, and its value held
    /// to it by `zend_is_valid_default_value`.
    ///
    /// ```php
    /// class A { const callable X = 1; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class constant A::X cannot have type callable in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { const void X = 1; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class constant A::X cannot have type void in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { const int X = "a"; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use string as value for class constant A::X of type int in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { const bool X = 1; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use int as value for class constant A::X of type bool in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { const self X = null; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use null as value for class constant A::X of type A in %s on line 2
    /// ```
    ///
    /// What php takes, it takes, an int where a float is taken made the
    /// float:
    ///
    /// ```php
    /// enum E { case A; }
    /// class A { const float F = 1; const ?int N = null; const mixed M = []; const E C = E::A; const int|string U = 'u'; }
    /// var_dump(A::F, A::N, A::M, A::C, A::U);
    /// ```
    /// ```expect
    /// float(1)
    /// NULL
    /// array(0) {
    /// }
    /// enum(E::A)
    /// string(1) "u"
    /// ```
    fn class_constant_type(
        &mut self,
        written: tree::TypeExpr,
        member: &str,
    ) -> Result<Type, CompileError> {
        let ty = self.typename(written, false)?.0;
        let mask = ty.mask();
        if mask != Type::MAY_BE_ANY
            && mask & (types::MAY_BE_CALLABLE | types::MAY_BE_VOID | types::MAY_BE_NEVER) != 0
        {
            return Err(self.compile_error(format!(
                "Class constant {member} cannot have type {}",
                php_unit::type_to_string(&ty)
            )));
        }
        Ok(ty)
    }

    /// `zend_compile_attributes` for a class declaration
    /// (`Z/Zend/zend_compile.c`): each attribute's name resolved as a
    /// `FETCH_CLASS` of the same file resolves it
    /// ([`Self::class_name`]), and each argument evaluated no further
    /// than `zend_const_expr_to_zval` leaves it, the constant
    /// expression the declaration keeps for `ReflectionAttribute` to
    /// read (design §4.7). An attribute is a marker php never
    /// instantiates on its own, so a name no class answers is no error
    /// here, as it is none in php.
    ///
    /// Then php's second loop over the ones the engine declares
    /// ([`attributes::InternalAttribute`]), at the class's own line: a
    /// target its `#[Attribute]` does not allow and one written twice
    /// are php's `E_ERROR`, unless `#[\DelayedTargetValidation]`
    /// defers the target, and its validator's refusal is
    /// `E_COMPILE_ERROR`. `#[\AllowDynamicProperties]` sets
    /// `ZEND_ACC_ALLOW_DYNAMIC_PROPERTIES`, kept beside the attribute
    /// as php keeps it for `ReflectionClass`.
    ///
    /// ```php
    /// #[ReturnTypeWillChange]
    /// class C {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Attribute "ReturnTypeWillChange" cannot target class (allowed targets: method) in %s on line 3
    /// ```
    ///
    /// ```php
    /// #[Attribute]
    /// #[Attribute]
    /// class A {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Attribute "Attribute" must not be repeated in %s on line 4
    /// ```
    ///
    /// ```php
    /// #[Attribute]
    /// interface Demo {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot apply #[\Attribute] to interface Demo in %s on line 3
    /// ```
    ///
    /// ```php
    /// #[Deprecated]
    /// class Demo {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot apply #[\Deprecated] to class Demo in %s on line 3
    /// ```
    ///
    /// ```php
    /// #[AllowDynamicProperties]
    /// interface Demo {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot apply #[\AllowDynamicProperties] to interface Demo in %s on line 3
    /// ```
    ///
    /// A validator's refusal is deferred as the target is:
    ///
    /// ```php
    /// #[DelayedTargetValidation, Attribute]
    /// trait T {}
    /// echo "compiled\n";
    /// ```
    /// ```expect
    /// compiled
    /// ```
    ///
    /// What php takes, it takes: an internal attribute on a target it
    /// allows, one deferred, and a user's attribute of any name:
    ///
    /// ```php
    /// #[Attribute(Attribute::TARGET_CLASS)]
    /// #[AllowDynamicProperties]
    /// #[DelayedTargetValidation, ReturnTypeWillChange]
    /// #[Mine, Mine]
    /// class C {}
    /// $c = new C;
    /// $c->p = 1;
    /// echo $c->p, "\n";
    /// ```
    /// ```expect exit=0
    /// 1
    /// ```
    fn class_attributes(
        &mut self,
        class: &Class,
        class_name: &[u8],
        declared_at: u32,
    ) -> Result<(Vec<Attribute>, DynamicProperties), CompileError> {
        let mut out = Vec::new();
        // `CG(zend_lineno)` is the declaration's while its attributes
        // compile, so what an argument raises is raised there.
        self.line = declared_at;
        for attribute in &class.attributes {
            let name = self.class_name(&attribute.name).to_vec();
            out.push(self.attribute(attribute, name, declared_at, None)?);
        }
        self.line = declared_at;
        let delayed = attributes::delays_validation(&out);
        let mut dynamic_properties = DynamicProperties::default();
        for at in 0..out.len() {
            let held =
                self.attribute_held(&out, at, attributes::TARGET_CLASS, delayed, self.line)?;
            let Some(known) = held.and_then(|config| config.core) else {
                continue;
            };
            // A validator's refusal under `#[\DelayedTargetValidation]`
            // is kept for `ReflectionAttribute::newInstance` and not
            // raised here.
            if let Some(refused) = known.refused_on_class(
                class.kind,
                class.modifiers,
                &php_unit::decl::shown(class_name),
            ) {
                if delayed {
                    continue;
                }
                return Err(self.compile_error(refused));
            }
            if known == InternalAttribute::AllowDynamicProperties {
                dynamic_properties = DynamicProperties::Allowed;
            }
        }
        Ok((out, dynamic_properties))
    }

    /// One attribute as the declaration keeps it: the name resolved,
    /// each argument the constant expression `zend_const_expr_to_zval`
    /// leaves, and whether its name is a string of its own
    /// ([`Self::own_name`]). `again` is what a promoted parameter's
    /// compile made of the same tree, which php compiles once more for
    /// the property over the tree the first compile evaluated in place:
    /// each argument a copy of the first's value, one more owner of it
    /// (`ZVAL_COPY`), and its names the same strings.
    ///
    /// Its list is held to `zend_compile_attributes`' rules at the line
    /// of the declaration it stands on: no unpacking, a name once, and
    /// no positional argument after a named one.
    ///
    /// ```php
    /// #[A(a: 1, a: 2)]
    /// class C {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Duplicate named parameter $a in %s on line 3
    /// ```
    ///
    /// ```php
    /// class C {
    ///     #[A(a: 1, 2)]
    ///     function f() {}
    /// }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use positional argument after named argument in %s on line 4
    /// ```
    ///
    /// ```php
    /// #[A(...[1])]
    /// function f() {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use unpacking in attribute argument list in %s on line 3
    /// ```
    ///
    /// ```php
    /// #[Attribute(...)]
    /// class Foo {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot create Closure as attribute argument in %s on line 3
    /// ```
    ///
    /// ```php
    /// #[Attribute] class A { function __construct(public $x = 0, public $y = 0) {} }
    /// #[A(1, y: 2)] class C {}
    /// $a = (new ReflectionClass('C'))->getAttributes()[0];
    /// var_dump($a->getArguments());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   ["y"]=>
    ///   int(2)
    /// }
    /// ```
    fn attribute(
        &mut self,
        attribute: &walk::Attr,
        name: Vec<u8>,
        declared_at: u32,
        again: Option<&Attribute>,
    ) -> Result<Attribute, CompileError> {
        let own_name = self.own_name(&attribute.name, again.is_some());
        if let Some(earlier) = again {
            for argument in &earlier.arguments {
                crate::values::unit_values(&argument.value, &mut |z| {
                    self.strings.hold(z);
                });
            }
            return Ok(Attribute {
                own_name,
                ..earlier.clone()
            });
        }
        if attribute
            .arguments
            .contains(&walk::AttrArgument::CallableConvert)
        {
            return Err(walk::refused(
                declared_at,
                "Cannot create Closure as attribute argument",
            ));
        }
        let mut arguments: Vec<AttributeArgument> = Vec::new();
        for argument in &attribute.arguments {
            let (named, expr) = match argument {
                walk::AttrArgument::CallableConvert => continue,
                walk::AttrArgument::Unpack => {
                    return Err(walk::refused(
                        declared_at,
                        "Cannot use unpacking in attribute argument list",
                    ));
                }
                walk::AttrArgument::Named(named, expr) => {
                    if arguments
                        .iter()
                        .any(|earlier| earlier.name.as_ref().is_some_and(|n| *n.0 == **named))
                    {
                        return Err(walk::refused(
                            declared_at,
                            format!(
                                "Duplicate named parameter ${}",
                                String::from_utf8_lossy(named)
                            ),
                        ));
                    }
                    (Some(Name(named.clone().into_boxed_slice())), *expr)
                }
                walk::AttrArgument::Positional(expr) => {
                    if arguments.iter().any(|earlier| earlier.name.is_some()) {
                        return Err(walk::refused(
                            declared_at,
                            "Cannot use positional argument after named argument",
                        ));
                    }
                    (None, *expr)
                }
            };
            self.folded_string = None;
            let value = self.interning(false, |this| this.dynamic_const_expr(expr))?;
            arguments.push(AttributeArgument {
                name: named,
                value: self.held_value(value),
            });
        }
        Ok(Attribute {
            name: Name(name.into_boxed_slice()),
            arguments,
            line: attribute.line,
            strict: self.typing == Typing::Strict,
            own_name,
        })
    }

    /// `zend_compile_attributes` over an enum case's list, the target
    /// `ZEND_ATTRIBUTE_TARGET_CLASS_CONST`, which `ReflectionClassConstant::getAttributes`
    /// reads.
    ///
    /// ```php
    /// #[Attribute] class Tag { function __construct(public string $v) {} }
    /// enum Suit { #[Tag('red')] case Hearts; }
    /// $a = (new ReflectionClassConstant(Suit::class, 'Hearts'))->getAttributes()[0];
    /// var_dump($a->getName(), $a->getArguments());
    /// ```
    /// ```expect
    /// string(3) "Tag"
    /// array(1) {
    ///   [0]=>
    ///   string(3) "red"
    /// }
    /// ```
    fn case_attributes(
        &mut self,
        constant: &walk::Constant,
    ) -> Result<Vec<Attribute>, CompileError> {
        self.member_attributes(
            &constant.attributes,
            attributes::TARGET_CLASS_CONST,
            0,
            constant.line,
            &[],
        )
    }

    /// `zend_compile_attributes` for any target but a class, whose
    /// validators this adds nothing to: each attribute's name resolved
    /// and its arguments compiled, an internal attribute that does not
    /// take `target` but takes `promoted` left to the promoted
    /// property's list, then php's second loop at the declaration's
    /// line, `declared_at`: an internal attribute on a target it does
    /// not allow and one written twice are php's `E_ERROR`, unless
    /// `#[\DelayedTargetValidation]` defers the target. A promoted
    /// parameter's property compiles the list again (`earlier`, what the
    /// parameter's compile made of it), each attribute there one more
    /// copy of the parameter's ([`Self::attribute`]).
    ///
    /// ```php
    /// class A {
    ///     #[Attribute]
    ///     public
    ///     function f() {}
    /// }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Attribute "Attribute" cannot target method (allowed targets: class) in %s on line 5
    /// ```
    ///
    /// ```php
    /// class A { #[Attribute] public $x; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Attribute "Attribute" cannot target property (allowed targets: class) in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { #[\Override] const X = 1; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Attribute "Override" cannot target class constant (allowed targets: method, property) in %s on line 2
    /// ```
    ///
    /// ```php
    /// #[\Override]
    /// function f() {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Attribute "Override" cannot target function (allowed targets: method, property) in %s on line 3
    /// ```
    ///
    /// ```php
    /// function f(
    ///     #[Attribute]
    ///     $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Attribute "Attribute" cannot target parameter (allowed targets: class) in %s on line 4
    /// ```
    ///
    /// ```php
    /// function f(#[SensitiveParameter] #[SensitiveParameter] $x) {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Attribute "SensitiveParameter" must not be repeated in %s on line 2
    /// ```
    ///
    /// What php takes, it takes, a user's attribute of any name on any
    /// target and an internal one deferred:
    ///
    /// ```php
    /// #[Attribute] class Tag { function __construct(public string $v = '') {} }
    /// #[Tag('f')] function f(#[Tag('p')] $p) {}
    /// class C {
    ///     #[Tag('k')] const K = 1;
    ///     #[Tag('x')] public $x;
    ///     #[Tag('m'), DelayedTargetValidation, Attribute] function m() {}
    ///     function __construct(#[Tag('q'), SensitiveParameter] public $q = 0) {}
    /// }
    /// $name = fn ($r) => $r->getAttributes()[0]->getArguments()[0];
    /// echo $name(new ReflectionFunction('f')), $name(new ReflectionParameter('f', 'p')),
    ///     $name(new ReflectionClassConstant('C', 'K')), $name(new ReflectionProperty('C', 'x')),
    ///     $name(new ReflectionMethod('C', 'm')), $name(new ReflectionProperty('C', 'q')),
    ///     count((new ReflectionProperty('C', 'q'))->getAttributes()), "\n";
    /// ```
    /// ```expect
    /// fpkxmq1
    /// ```
    fn member_attributes(
        &mut self,
        written: &[walk::Attr],
        target: u32,
        promoted: u32,
        declared_at: u32,
        earlier: &[Attribute],
    ) -> Result<Vec<Attribute>, CompileError> {
        let mut out = Vec::with_capacity(written.len());
        let mut earlier = earlier.iter();
        self.line = declared_at;
        for attribute in written {
            let name = self.class_name(&attribute.name).to_vec();
            let targets = self
                .attribute_config(&name)
                .map(attributes::Config::targets);
            let left = |on: u32, to: u32| targets.is_some_and(|t| t & on == 0 && t & to != 0);
            // What the parameter's compile took of the list, which left
            // to the property what targets the property alone.
            let again = (!left(promoted, target)).then(|| earlier.next()).flatten();
            if left(target, promoted) {
                continue;
            }
            out.push(self.attribute(attribute, name, declared_at, again)?);
        }
        self.line = declared_at;
        let delayed = attributes::delays_validation(&out);
        for at in 0..out.len() {
            self.attribute_held(&out, at, target, delayed, declared_at)?;
        }
        Ok(out)
    }

    /// `zend_internal_attribute_get` over a resolved name: one of Core's
    /// attributes, or a class of the bundle declared with `#[Attribute]`.
    fn attribute_config(&self, name: &[u8]) -> Option<attributes::Config> {
        attributes::Config::named(name, |name| {
            self.known_classes.find(name).map(|(_, decl)| decl)
        })
    }

    /// `zend_compile_attributes`' second loop over the attribute of
    /// `out` at `at`: an internal one held to the target it stands on,
    /// unless `#[\DelayedTargetValidation]` defers that, and to standing
    /// once, unless its class may repeat, and then, where it stands on
    /// its target, to the validator its extension declared, whose
    /// sentence is the compile error unless `#[\DelayedTargetValidation]`
    /// keeps it for `ReflectionAttribute::newInstance`. What it is where
    /// it stands on its target, for Core's validators.
    fn attribute_held(
        &self,
        out: &[Attribute],
        at: usize,
        target: u32,
        delayed: bool,
        line: u32,
    ) -> Result<Option<attributes::Config>, CompileError> {
        let Some(attribute) = out.get(at) else {
            return Ok(None);
        };
        let Some(config) = self.attribute_config(&attribute.name.0) else {
            return Ok(None);
        };
        let shown = String::from_utf8_lossy(&attribute.name.0).into_owned();
        let targets = config.targets();
        let on_target = targets & target != 0;
        let refused = |message: String| {
            Refused {
                level: Level::Error,
                message: message.into(),
                line,
                raised: Vec::new(),
                raise: crate::Raise::Fatal,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            }
            .into()
        };
        if !on_target && !delayed {
            return Err(refused(format!(
                "Attribute \"{shown}\" cannot target {} (allowed targets: {})",
                attributes::target_names(target),
                attributes::target_names(targets)
            )));
        }
        if !config.repeatable()
            && out
                .iter()
                .enumerate()
                .any(|(other, o)| other != at && o.name.0.eq_ignore_ascii_case(&attribute.name.0))
        {
            return Err(refused(format!(
                "Attribute \"{shown}\" must not be repeated"
            )));
        }
        if on_target
            && !delayed
            && let Some(validator) = config.validator
            && let Ok(target) = php_unit::decl::AttributeTarget::try_from(target)
            && let Some(sentence) = (validator.0)(
                attribute,
                target,
                self.active_class
                    .as_ref()
                    .map(|class| class.name.as_slice()),
            )
        {
            let mut refused = refused(sentence);
            refused.level = Level::CompileError;
            return Err(refused);
        }
        Ok(on_target.then_some(config))
    }

    /// `zend_compile_prop_decl`, the same one declarator at a time,
    /// with `CG(context).active_property_info_name` set over the
    /// default so `__PROPERTY__` folds to the declarator's name, and
    /// [`Self::set_visibility_of`] over the group's asymmetric
    /// visibility.
    ///
    /// Every property of a readonly class is readonly, held to what a
    /// readonly property is held to:
    ///
    /// ```php
    /// readonly class P { public int $x; function __construct() { $this->x = 1; } }
    /// $p = new P;
    /// try { $p->x = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $p->y = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot modify readonly property P::$x
    /// Cannot create dynamic property P::$y
    /// ```
    ///
    /// ```php
    /// readonly class P { public $x; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Readonly property P::$x must have type in %s on line 2
    /// ```
    ///
    /// ```php
    /// readonly class P { public int $x = 1; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Readonly property P::$x cannot have default value in %s on line 2
    /// ```
    ///
    /// ```php
    /// readonly class P { public static int $x; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Static property P::$x cannot be readonly in %s on line 2
    /// ```
    fn class_property(
        &mut self,
        at: usize,
        property: &walk::Property,
        name: &[u8],
    ) -> Result<(), CompileError> {
        let Some((kind, is_readonly_class, declared_before)) = (self.classes.get(at))
            .map(|decl| (decl.kind, decl.flags.is_readonly, decl.properties.len()))
        else {
            return Err(self.internal("a property of a class the unit has not"));
        };
        self.line = property.line;
        if kind == ClassKind::Enum {
            return Err(self.compile_error(format!(
                "Enum {} cannot include properties",
                String::from_utf8_lossy(name)
            )));
        }
        self.property_modifiers_refused(kind, property)?;
        let member = format!(
            "{}::${}",
            php_unit::decl::shown(name),
            String::from_utf8_lossy(&property.name)
        );
        self.active_property = Some(property.name.clone());
        let ty = match property.ty {
            Some(written) => Some(self.property_type(written, &member)?),
            None => None,
        };
        if (self.classes.get(at))
            .is_some_and(|decl| decl.properties.iter().any(|p| *p.name.0 == *property.name))
        {
            return Err(self.compile_error(format!("Cannot redeclare {member}")));
        }
        let mut default = match property.default {
            Some(expr) => Some(self.held_const_expr(expr)?),
            None => None,
        };
        if let (Some(ty), Some(default)) = (&ty, default.as_mut()) {
            self.property_default_refused(ty, default, &member)?;
        }
        self.active_property = None;
        // A readonly class's properties are readonly.
        let is_readonly = property.is_readonly || is_readonly_class;
        if is_readonly {
            self.readonly_refused(&member, ty.is_some(), default.is_some(), property.is_static)?;
        }
        let set_visibility = self.set_visibility_of(
            property.visibility,
            property.set_visibility,
            is_readonly,
            ty.is_some(),
            &member,
        )?;
        let (hooks, is_virtual) = match &property.hooks {
            Some(hooks) => {
                let hooked = hooks::Hooked {
                    class: u32::try_from(at).unwrap_or(u32::MAX),
                    property: u32::try_from(declared_before).unwrap_or(u32::MAX),
                    class_name: name,
                    kind,
                    name: &property.name,
                    ty: property.ty,
                    declared: ty.as_ref(),
                    visibility: property.visibility,
                    is_static: property.is_static,
                    is_readonly,
                    is_abstract: property.is_abstract || kind == ClassKind::Interface,
                };
                let (hooks, is_virtual) = self.property_hooks(&hooked, hooks)?;
                (Some(Box::new(hooks)), is_virtual)
            }
            None => (None, false),
        };
        // `zend_compile_prop_decl` compiles the attributes under
        // `active_property_info_name`, where `__PROPERTY__` reads it.
        self.active_property = Some(property.name.clone());
        let attributes = self.member_attributes(
            &property.attributes,
            attributes::TARGET_PROPERTY,
            0,
            property.line,
            &[],
        );
        self.active_property = None;
        let attributes = attributes?;
        let declared = PropertyDecl {
            name: Name(property.name.clone().into_boxed_slice()),
            visibility: property.visibility,
            set_visibility,
            is_instance: !property.is_static,
            is_readonly,
            // `zend_declare_typed_property`: a `private(set)`
            // property is implicitly final.
            is_final: property.is_final || set_visibility == Some(Visibility::Private),
            is_promoted: false,
            is_virtual,
            ty,
            default,
            attributes,
            hooks,
            doc_comment: property.doc_comment.clone().map(Vec::into_boxed_slice),
        };
        if let Some(decl) = self.classes.get(at) {
            self.hooked_property_verified(decl, &declared)?;
        }
        let Some(decl) = self.classes.get_mut(at) else {
            return Err(self.internal("a property of a class the unit has not"));
        };
        decl.properties.push(declared);
        Ok(())
    }

    /// A declared property's type, `zend_compile_typename` in
    /// `zend_compile_prop_decl`, which refuses one that takes `void`,
    /// `never` or `callable`.
    fn property_type(
        &mut self,
        written: tree::TypeExpr,
        member: &str,
    ) -> Result<Type, CompileError> {
        let ty = self.typename(written, false)?.0;
        let mask = ty.mask();
        if mask & (types::MAY_BE_VOID | types::MAY_BE_NEVER | types::MAY_BE_CALLABLE) != 0 {
            return Err(self.compile_error(format!(
                "Property {member} cannot have type {}",
                php_unit::type_to_string(&ty)
            )));
        }
        Ok(ty)
    }

    /// What `zend_compile_prop_decl` refuses of a readonly property: one
    /// with no type, one with a default, and one declared `static`.
    fn readonly_refused(
        &self,
        member: &str,
        typed: bool,
        defaulted: bool,
        is_static: bool,
    ) -> Result<(), CompileError> {
        if !typed {
            return Err(self.compile_error(format!("Readonly property {member} must have type")));
        }
        if defaulted {
            return Err(self.compile_error(format!(
                "Readonly property {member} cannot have default value"
            )));
        }
        if is_static {
            return Err(self.compile_error(format!("Static property {member} cannot be readonly")));
        }
        Ok(())
    }

    /// `zend_verify_hooked_property` where `zend_compile_property_hooks`
    /// makes it, over a hooked property of a class that names no
    /// parent; a class that does is held to it by its link, once what
    /// it inherits is known ([`php_link::verify_hooked_property`]).
    ///
    /// ```php
    /// class Test { public $prop = 0 { get {} set {} } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot specify default value for virtual hooked property Test::$prop in %s on line 2
    /// ```
    fn hooked_property_verified(
        &self,
        decl: &ClassDecl,
        property: &PropertyDecl,
    ) -> Result<(), CompileError> {
        if decl.parent.is_some() {
            return Ok(());
        }
        match php_link::verify_hooked_property(&decl.name.0, property) {
            Some(refusal) => Err(self.compile_error(refusal.to_string())),
            None => Ok(()),
        }
    }

    /// What `zend_compile_prop_decl` refuses of a property's modifiers
    /// against each other and its class, before its type, in php's
    /// order: `final` beside `private`, what an interface's property may
    /// not be, and `abstract` or an interface's property without the
    /// accessor list that alone may make one.
    ///
    /// ```php
    /// class A { final private $x; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Property cannot be both final and private in %s on line 2
    /// ```
    ///
    /// ```php
    /// interface I { final public $x; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Property in interface cannot be final in %s on line 2
    /// ```
    ///
    /// ```php
    /// interface I { protected $x; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Property in interface cannot be protected or private in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { abstract public $x; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Only hooked properties may be declared abstract in %s on line 2
    /// ```
    ///
    /// ```php
    /// interface I { public $x; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Interfaces may only include hooked properties in %s on line 2
    /// ```
    ///
    /// What php takes, it takes:
    ///
    /// ```php
    /// class A { final public int $x = 1; public readonly int $y; function __construct() { $this->y = 2; } }
    /// $a = new A;
    /// var_dump($a->x, $a->y);
    /// try { $a->y = 3; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump((new ReflectionProperty('A', 'x'))->getModifiers(), (new ReflectionProperty('A', 'y'))->getModifiers());
    /// ```
    /// ```expect
    /// int(1)
    /// int(2)
    /// Cannot modify readonly property A::$y
    /// int(33)
    /// int(2177)
    /// ```
    ///
    /// ```php
    /// class A { final public $x; }
    /// class B extends A { public $x; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot override final property A::$x in %s on line 3
    /// ```
    fn property_modifiers_refused(
        &self,
        kind: ClassKind,
        property: &walk::Property,
    ) -> Result<(), CompileError> {
        if property.is_final && property.visibility == Visibility::Private {
            return Err(self.compile_error("Property cannot be both final and private"));
        }
        let in_interface = kind == ClassKind::Interface;
        if in_interface {
            if property.is_final {
                return Err(self.compile_error("Property in interface cannot be final"));
            }
            if property.visibility != Visibility::Public {
                return Err(
                    self.compile_error("Property in interface cannot be protected or private")
                );
            }
            if property.is_abstract {
                return Err(self.compile_error(
                    "Property in interface cannot be explicitly abstract. All interface members are implicitly abstract",
                ));
            }
            if property.hooks.is_none() {
                return Err(self.compile_error("Interfaces may only include hooked properties"));
            }
        }
        if property.is_abstract && property.hooks.is_none() {
            return Err(self.compile_error("Only hooked properties may be declared abstract"));
        }
        Ok(())
    }

    /// What `zend_compile_prop_decl` refuses about a typed property's
    /// default, `zend_is_valid_default_value` over the value it folded
    /// to: `null` under a type without it names the nullable type that
    /// would take it, and any other value php's
    /// `zend_zval_value_name`. A default still naming a constant is
    /// checked when it is evaluated, and an int under a type that takes
    /// `float` and not `int` becomes the float.
    ///
    /// ```php
    /// class C {
    ///     public int $a = 1;
    ///     public string|int $b = null;
    /// }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Default value for property of type string|int may not be null. Use the nullable type string|int|null to allow null default value in %s on line 4
    /// ```
    ///
    /// ```php
    /// class C { public array $bar = 1; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use int as default value for property C::$bar of type array in %s on line 2
    /// ```
    ///
    /// ```php
    /// class C { public float $f = 1; public ?int $n = null; public iterable $i = []; public int $k = PHP_INT_SIZE; }
    /// var_dump(new C);
    /// ```
    /// ```expect exit=0
    /// object(C)#1 (4) {
    ///   ["f"]=>
    ///   float(1)
    ///   ["n"]=>
    ///   NULL
    ///   ["i"]=>
    ///   array(0) {
    ///   }
    ///   ["k"]=>
    ///   int(8)
    /// }
    /// ```
    fn property_default_refused(
        &self,
        ty: &Type,
        default: &mut ConstExpr,
        member: &str,
    ) -> Result<(), CompileError> {
        let Err(kind) = valid_default(ty, default) else {
            return Ok(());
        };
        let text = php_unit::type_to_string(ty);
        if kind == ZvalType::Null && !matches!(ty, Type::Intersection(_)) {
            let nullable = Type::Union(vec![ty.clone(), Type::Null]);
            return Err(self.compile_error(format!(
                "Default value for property of type {text} may not be null. Use the nullable type {} to allow null default value",
                php_unit::type_to_string(&nullable)
            )));
        }
        let given = match kind {
            ZvalType::Null => "null",
            ZvalType::False => "false",
            ZvalType::True => "true",
            ZvalType::Long => "int",
            ZvalType::Double => "float",
            ZvalType::String => "string",
            _ => "array",
        };
        Err(self.compile_error(format!(
            "Cannot use {given} as default value for property {member} of type {text}"
        )))
    }

    /// `zend_declare_typed_property`'s arm over `ZEND_ACC_PPP_SET_MASK`
    /// (`Z/Zend/zend_API.c:4517-4541`): a set visibility needs a type,
    /// may not be weaker than the visibility a read is held to, and is
    /// dropped where the two are the same, which is why a declaration
    /// model never carries `public(set)`. The strength order is php's
    /// own, the `ZEND_ACC_*_SET` bits ascending from `PUBLIC_SET`
    /// (`Z/Zend/zend_compile.h:274-276`).
    ///
    /// ```php
    /// class Foo { public private(set) int $bar = 1; public protected(set) int $baz = 2; }
    /// $f = new Foo;
    /// var_dump($f->bar, $f->baz);
    /// ```
    /// ```expect
    /// int(1)
    /// int(2)
    /// ```
    ///
    /// An untyped one is refused where the property is named:
    ///
    /// ```php
    /// class Foo { public private(set) $bar; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Property with asymmetric visibility Foo::$bar must have type in %s on line 2
    /// ```
    ///
    /// So is a set visibility weaker than the one a read is held to:
    ///
    /// ```php
    /// class Foo { protected public(set) int $bar; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Visibility of property Foo::$bar must not be weaker than set visibility in %s on line 2
    /// ```
    ///
    /// A set visibility the read visibility equals is dropped, so
    /// `private private(set)` is the plain private property and is not
    /// final; `private(set)` raises `ZEND_ACC_FINAL` where it stands.
    /// php refuses neither of them on a `static` property, which the
    /// 8.4 proposal did: `static_props.phpt` of
    /// `Z/Zend/tests/asymmetric_visibility` declares four.
    ///
    /// ```php
    /// class Foo { private private(set) int $a = 1; public private(set) static int $b = 2; }
    /// var_dump((new ReflectionProperty('Foo', 'a'))->getModifiers());
    /// var_dump((new ReflectionProperty('Foo', 'b'))->getModifiers());
    /// ```
    /// ```expect
    /// int(4)
    /// int(4145)
    /// ```
    ///
    /// A public readonly property that writes no set visibility is
    /// `protected(set)`, and one that writes `public(set)` is public to
    /// write, the equal set visibility dropped only after the implicit
    /// one was passed over:
    ///
    /// ```php
    /// class P { public readonly int $a; public public(set) readonly int $b; }
    /// $p = new P;
    /// try { $p->a = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $p->b = 2;
    /// var_dump($p->b);
    /// ```
    /// ```expect
    /// Cannot modify protected(set) readonly property P::$a from global scope
    /// int(2)
    /// ```
    fn set_visibility_of(
        &self,
        visibility: Visibility,
        set: Option<Visibility>,
        is_readonly: bool,
        typed: bool,
        member: &str,
    ) -> Result<Option<Visibility>, CompileError> {
        let Some(set) = set else {
            return Ok(implicit_set_visibility(is_readonly, visibility));
        };
        if !typed {
            return Err(self.compile_error(format!(
                "Property with asymmetric visibility {member} must have type"
            )));
        }
        let strength = |visibility| match visibility {
            Visibility::Public => 0_u8,
            Visibility::Protected => 1,
            Visibility::Private => 2,
        };
        if strength(visibility) > strength(set) {
            return Err(self.compile_error(format!(
                "Visibility of property {member} must not be weaker than set visibility"
            )));
        }
        Ok((visibility != set).then_some(set))
    }

    /// `zend_resolve_const_class_name_reference` over a name written
    /// after `extends` (`what` is `class name`) or `implements`
    /// (`interface name`): the name a `FETCH_CLASS` of the same file
    /// resolves to ([`Self::class_name`]), and `self`, `parent` and
    /// `static` php's compile-time fatal, the name as written.
    ///
    /// ```php
    /// class A extends parent {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use "parent" as class name, as it is reserved in %s on line 2
    /// ```
    fn inherited_name(&mut self, written: &[u8], what: &str) -> Result<Vec<u8>, CompileError> {
        if class_fetch_type(written) != FETCH_CLASS_DEFAULT {
            return Err(self.compile_error(format!(
                "Cannot use \"{}\" as {what}, as it is reserved",
                String::from_utf8_lossy(written)
            )));
        }
        Ok(self.class_name(written).to_vec())
    }

    /// `zend_try_early_bind`: the class at `at` linked against what
    /// `CG(class_table)` holds while the file compiles, the engine's
    /// classes and the file's own bound so far, by the checks
    /// `zend_inheritance.c` makes ([`php_link`]). A refusal is
    /// php's `E_COMPILE_ERROR` while the file compiles, so nothing of it
    /// runs; a check that needs a class not bound yet leaves the class to
    /// its `DECLARE_CLASS` (false), linked where it was written; and the
    /// deprecations a tentative return raises are the unit's.
    ///
    /// ```php
    /// echo "before\n";
    /// interface I {}
    /// class C extends I {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C cannot extend interface I in %s on line 4
    /// ```
    ///
    /// ```php
    /// class C extends Closure {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C cannot extend final class Closure in %s on line 2
    /// ```
    ///
    /// ```php
    /// class G extends Generator {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class G cannot extend final class Generator in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { final function f() {} }
    /// class B extends A { function f() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot override final method A::f() in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { function f() {} }
    /// class B extends A { static function f() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot make non static method A::f() static in class B in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { static function f() {} }
    /// class B extends A { function f() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot make static method A::f() non static in class B in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { function f() {} }
    /// class B extends A { protected function f() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Access level to B::f() must be public (as in class A) in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { protected function f() {} }
    /// class B extends A { private function f() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Access level to B::f() must be protected (as in class A) or weaker in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { function f($a) {} }
    /// class B extends A { function f(int $a) {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Declaration of B::f(int $a) must be compatible with A::f($a) in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { public private(set) int $p; }
    /// class B extends A { public private(set) int $p; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot override final property A::$p in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { public static $p; }
    /// class B extends A { public $p; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot redeclare static A::$p as non static B::$p in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { function __construct(public readonly int $p) {} }
    /// class B extends A { public int $p; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot redeclare readonly property A::$p as non-readonly B::$p in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { public int $p; }
    /// class B extends A { public private(set) int $p; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Set access level of B::$p must be omitted (as in class A) in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { public protected(set) int $p; }
    /// class B extends A { public private(set) int $p; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Set access level of B::$p must be protected(set) (as in class A) or weaker in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { public int $p; }
    /// class B extends A { protected int $p; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Access level to B::$p must be public (as in class A) in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { public int $p; }
    /// class B extends A { public string $p; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type of B::$p must be int (as in class A) in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { public $p; }
    /// class B extends A { public int $p; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type of B::$p must be omitted to match the parent definition in class A in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { final const X = 1; }
    /// class B extends A { const X = 2; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: B::X cannot override final constant A::X in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { const X = 1; }
    /// class B extends A { protected const X = 2; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Access level to B::X must be public (as in class A) in %s on line 3
    /// ```
    ///
    /// What php takes, it takes: a wider parameter, an added optional
    /// one, a narrower return, a wider visibility, a property of the
    /// same type written its own way, and a parent's private method
    /// taken over on any terms, each bound early:
    ///
    /// ```php
    /// class A {
    ///     public int|string $p = 1;
    ///     protected function f(int $a): object { return $this; }
    ///     private function g(int $a) {}
    /// }
    /// class B extends A {
    ///     public string|int $p = 2;
    ///     public function f(int|string $a, $b = 0): B { return $this; }
    ///     public static function g() {}
    /// }
    /// $b = new B;
    /// var_dump($b->f(1) === $b, $b->p);
    /// ```
    /// ```expect exit=0
    /// bool(true)
    /// int(2)
    /// ```
    fn try_early_bind(&mut self, at: usize) -> Result<bool, CompileError> {
        let Some(child) = self.classes.get(at) else {
            return Ok(false);
        };
        let lookup = |name: &[u8]| {
            self.bound_class(name).map(|decl| php_link::Held {
                decl,
                bodies: None,
                linked: true,
                record: None,
            })
        };
        // A class that uses traits is never bound early, so nothing
        // here composes one, and no value is evaluated; the class is in
        // no table yet, so a type its parent writes does not find it.
        let linking = php_link::early_bind(child, &lookup, &|()| None);
        // `zend_can_early_bind`'s `INHERITANCE_UNRESOLVED`, silent: the
        // class is left to its opline.
        if !linking.obligations.is_empty() {
            return Ok(false);
        }
        for raised in linking.raised {
            self.diagnostics.push(Diagnostic {
                level: raised.level(),
                line: raised.site().line,
                message: raised.to_string().as_str().into(),
            });
        }
        match linking.outcome {
            php_link::Linked::Linked => Ok(true),
            php_link::Linked::Refused(refused) => Err(Refused {
                level: refused.refusal.level(),
                message: refused.refusal.to_string().into(),
                line: refused.site.line,
                raised: Vec::new(),
                raise: crate::Raise::Fatal,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            }
            .into()),
        }
    }

    /// A class `CG(class_table)` holds while the file compiles: one the
    /// engine declared, one of the file's own that was bound early,
    /// which a class declared by a `DECLARE_CLASS` is not yet, and, for a
    /// compile php runs without `ZEND_COMPILE_IGNORE_OTHER_FILES`, one
    /// the request declared before it ([`crate::RequestClasses::declared`]).
    ///
    /// So an `eval`'d class whose parent the request declared is bound
    /// early, as `zend_try_early_bind` binds it: it stands before the
    /// `eval`'s code runs, and a name after it in the same compile reads
    /// it, its constant substituted where the fetch and its cache slots
    /// would stand (the file under the recipe's `TMPDIR`, so each
    /// `eval`'s name has one length):
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3275-early-bound.inc';
    /// file_put_contents($f, <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// m('class X9 { const K = 1; }');
    /// echo m('class X10 extends X9 { const K = 1; } function f10() { return X10::K; }'), "\n";
    /// eval('echo class_exists("X11", false) ? "bound" : "unbound", "\n"; class X11 extends X9 {}');
    /// echo f10(), "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 640
    /// bound
    /// 1
    /// ```
    ///
    /// A file the cache keeps is compiled under
    /// `ZEND_COMPILE_IGNORE_OTHER_FILES`, so the same class there, its
    /// parent declared by the file prepended to it, is left to its
    /// `DECLARE_CLASS_DELAYED`, as opcache's dump of it reads in php:
    ///
    /// ```php
    /// $put = function ($name, $src) { $f = sys_get_temp_dir() . "/$name"; file_put_contents("$f.tmp", $src); touch("$f.tmp", time() - 10); rename("$f.tmp", $f); return $f; };
    /// $p = $put('w3275-parent.inc', '<?php class X9 { const K = 1; }');
    /// $c = $put('w3275-child.inc', '<?php class X10 extends X9 { const K = 2; } echo X10::K, "\n";');
    /// $run = shell_exec(escapeshellarg(PHP_BINARY) . ' -n -d opcache.enable_cli=1 -d opcache.jit=off -d opcache.opt_debug_level=0x10000 -d auto_prepend_file=' . escapeshellarg($p) . ' ' . escapeshellarg($c) . ' 2>&1');
    /// preg_match_all('/^(\d{4} DECLARE.*|\d)$/m', $run, $lines);
    /// echo implode("\n", $lines[0]), "\n";
    /// unlink($p);
    /// unlink($c);
    /// ```
    /// ```expect process regime=jit-off
    /// 0000 DECLARE_CLASS_DELAYED string("x10") string("x9")
    /// 2
    /// ```
    pub(super) fn bound_class(&self, name: &[u8]) -> Option<&ClassDecl> {
        if let Some((_, decl)) = self.known_classes.find(name) {
            return Some(decl);
        }
        (self.bound.iter().filter_map(|at| self.classes.get(*at)))
            .find(|decl| decl.name.0.eq_ignore_ascii_case(name))
            .or_else(|| self.request_classes.declared(self.arena.lowercase(name)))
    }

    /// `ZEND_DECLARE_CLASS`, what php emits for a class it did not early
    /// bind, the class at `at` of [`Emit::classes`]: the parent's
    /// lowered name in `op2` where it has one, the class's lowered name
    /// in `op1`, and in the constant after it the runtime definition key
    /// `zend_build_runtime_definition_key` makes (`Z/Zend/zend_compile.c:175`),
    /// `\0`, the lowered name, the file, `:`, the declaration's first
    /// line, `$` and the file's count of keys so far in hex, which is
    /// what the class is filed under ([`Unit::runtime_keys`]) and what
    /// `do_bind_class` finds it by, so that no two declarations of one
    /// name are one class. The key is `zend_strpprintf`'s, laid at
    /// `zend_string_finit`'s size ([`Self::own_string`]). The constants
    /// are added in php's order, the parent's first.
    ///
    /// A top-level class with no interface and no trait is one php's
    /// opcache delays (`ZEND_COMPILE_DELAYED_BINDING`, set by
    /// `persistent_compile_file`, the regime design §6.1 dumps under):
    /// its opline is `ZEND_DECLARE_CLASS_DELAYED`, `op2` the empty string
    /// where no parent is written, with a cache slot in
    /// `extended_value`, and the unit's load tries the early bind
    /// again against the classes declared by then
    /// ([`Unit::runtime_keys`]).
    fn declare_class(
        &mut self,
        at: usize,
        name: &[u8],
        own: Option<Str>,
        parent: Option<&[u8]>,
        line: u32,
        delayed: bool,
    ) -> Result<(), CompileError> {
        let (mut op2_type, mut op2) = match parent {
            Some(parent) => {
                let lowered = self.string(self.arena.lowercase(parent))?;
                (OperandKind::Const, self.literal(lowered))
            }
            None => (OperandKind::Unused, 0),
        };
        let lowered = self.arena.lowercase(name);
        let mut key = Vec::with_capacity(lowered.len() + self.filename.len() + 16);
        key.push(0);
        key.extend_from_slice(lowered);
        key.extend_from_slice(self.filename);
        key.extend_from_slice(format!(":{line}${:x}", self.keys_made()).as_bytes());
        let lowered = self.class_key(at, own, name)?;
        let op1 = self.literal(lowered);
        let filed = self.own_string(&key, Sizing::Finit)?;
        self.literal(filed);
        self.runtime_keys.push((
            Name(key.into_boxed_slice()),
            u32::try_from(at).unwrap_or(u32::MAX),
        ));
        if !delayed {
            self.push(Opline {
                opcode: Opcode::DeclareClass,
                op1_type: OperandKind::Const,
                op1,
                op2_type,
                op2,
                ..Opline::default()
            });
            return Ok(());
        }
        if parent.is_none() {
            let empty = self.string(b"")?;
            (op2_type, op2) = (OperandKind::Const, self.literal(empty));
        }
        let slot = self.cache_slot();
        self.push(Opline {
            opcode: Opcode::DeclareClassDelayed,
            op1_type: OperandKind::Const,
            op1,
            op2_type,
            op2,
            extended_value: slot,
            ..Opline::default()
        });
        Ok(())
    }

    /// A constant expression as a declaration keeps it (design §4.7):
    /// `zend_compile_const_expr` over a node: the constant expression a
    /// declaration keeps, php's `zend_const_expr_to_zval`, which folds
    /// what `zend_eval_const_expr` can, so `1 + 2` is `3`, `'a' . 'b'`
    /// is `'ab'` and a number or a string as written is itself, and what
    /// names a constant stays the tree it was written as, for the engine
    /// to evaluate on first use. `zend_is_allowed_in_const_expr` refuses
    /// a variable, a call, an assignment and an increment in php's
    /// words; a cast or a `match`, which php allows or refuses on its
    /// own terms, is not read yet.
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
    ///
    /// A constant of the class being compiled folds where it stands
    /// (`zend_try_ct_eval_class_const`), so `self::A + 1` is `2` in the
    /// declaration; `parent::` stays the tree, resolved in the declaring
    /// scope when the engine evaluates it:
    ///
    /// ```php
    /// class P { const A = 1; const B = self::A + 1; public $d = self::B; }
    /// class C extends P { const D = parent::B; const E = [self::D, P::A]; }
    /// var_dump(C::D, (new C)->d, C::E);
    /// ```
    /// ```expect
    /// int(2)
    /// int(2)
    /// array(2) {
    ///   [0]=>
    ///   int(2)
    ///   [1]=>
    ///   int(1)
    /// }
    /// ```
    ///
    /// `__CLASS__` in a trait is kept for the scope that evaluates it,
    /// which is the class that took the member, through a trait that
    /// uses the trait too:
    ///
    /// ```php
    /// trait T { public static $s = __CLASS__; public $p = [__CLASS__, __TRAIT__]; const K = __CLASS__; }
    /// trait U { use T; }
    /// class C { use U; }
    /// var_dump(C::$s, (new C)->p, C::K);
    /// ```
    /// ```expect
    /// string(1) "C"
    /// array(2) {
    ///   [0]=>
    ///   string(1) "C"
    ///   [1]=>
    ///   string(1) "T"
    /// }
    /// string(1) "C"
    /// ```
    ///
    /// `self::class` and `parent::class` where the scope is not known, a
    /// closure's or a trait's, stay `ZEND_AST_CLASS_NAME` for the scope
    /// that evaluates them:
    ///
    /// ```php
    /// $f = function ($x = self::class, $y = PARENT::class) {};
    /// foreach ((new ReflectionFunction($f))->getParameters() as $p) { echo $p, "\n"; }
    /// try { $f(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// trait T { public $p = self::class; }
    /// class C { use T; }
    /// var_dump((new C)->p);
    /// ```
    /// ```expect
    /// Parameter #0 [ <optional> $x = self::class ]
    /// Parameter #1 [ <optional> $y = parent::class ]
    /// Cannot use "self" when no class scope is active
    /// string(1) "C"
    /// ```
    ///
    /// A `??` over `null` and a conditional over a constant are the side
    /// the evaluator picked, as the default Reflection prints shows:
    ///
    /// ```php
    /// function f($x = null ?? Y, $y = true ? Z : 1, $z = [1 ? W : 2, null ?? V]) {}
    /// foreach ((new ReflectionFunction('f'))->getParameters() as $p) echo $p, "\n";
    /// ```
    /// ```expect
    /// Parameter #0 [ <optional> $x = Y ]
    /// Parameter #1 [ <optional> $y = Z ]
    /// Parameter #2 [ <optional> $z = [W, V] ]
    /// ```
    ///
    /// A property fetch's object is evaluated before the fetch is looked
    /// at, as `zend_eval_const_expr` walks it, so what the object
    /// refuses is refused first, and an attribute's argument is compiled
    /// at the line of the declaration it stands on (#855):
    ///
    /// ```php
    /// #[Attribute([,]->e)]
    /// class Foo {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use empty array elements in arrays in %s on line 3
    /// ```
    #[allow(clippy::too_many_lines)]
    fn const_expr(&mut self, expr: tree::Expr) -> Result<ConstExpr, CompileError> {
        // `zend_compile_const_expr` walks the tree `zend_eval_const_expr`
        // left ([`eval`]): a node it folded is its value, and a `??` or a
        // conditional it replaced is the side it picked.
        match self.rewrites.get(expr) {
            Some(eval::Rewrite::Value(value)) => return self.folded(value),
            Some(eval::Rewrite::Node(node)) => return self.const_expr(node),
            None => {}
        }
        let line = expr.line(self.parsed);
        self.check_stack()?;
        let tree = &self.parsed.tree;
        match expr {
            tree::Expr::ParenExpr(id) => match tree[id].inner {
                Some(inner) => self.const_expr(inner),
                None => Err(self.unread("an expression inside parentheses")),
            },
            // `zend_compile_const_expr_const`: the name resolved through
            // the constant imports and the namespace, an unqualified one
            // no import names, in a namespace, marked for the global
            // fallback the engine's fetch makes. It moves
            // `CG(zend_lineno)` to the name's line, which the opline
            // after the walk is emitted at.
            // `zend_eval_const_expr`'s `ZEND_AST_CONST` arm comes first:
            // the three special names and a constant
            // `zend_try_ct_eval_const` substitutes
            // ([`Self::named_constant`]), which a parameter's default
            // does not ([`Substitution::NoPersistent`]); the node is
            // replaced by the value, which keeps its owner.
            tree::Expr::NameRef(token) => {
                if let Some(value) = self.named_constant(token)? {
                    return self.folded(value.kept());
                }
                self.line = line;
                let written = self.text(token).to_vec();
                Ok(ConstExpr::Constant {
                    name: self.constant_resolved(&written),
                    line,
                })
            }
            // `zend_compile_const_expr_magic_const`: `__CLASS__` in a
            // trait is the one magic constant the fold leaves, kept as
            // `ZEND_AST_CONSTANT_CLASS` for the scope that evaluates it.
            tree::Expr::MagicConstExpr(id) => {
                let (which, line) = self.magic_of(id)?;
                if matches!(self.magic_const(which, line), Folded::UsingClass) {
                    return Ok(ConstExpr::ScopeClass);
                }
                let folded = self.magic_node(id)?;
                self.folded(folded)
            }
            // `zend_compile_const_expr_class_const`: the class named as
            // written, resolved in the namespace, and the constant's
            // name, kept as the tree for the engine to fetch on first
            // use; `self` and `parent` stay as written, for the engine
            // to resolve in the scope that evaluates the expression;
            // `static` and a class the program computes are php's
            // compile-time fatals.
            tree::Expr::ScopedAccessExpr(id) => match self.scoped_of(id, line)? {
                Scoped::Constant { class, name } => {
                    // `zend_eval_const_expr` folded the class to a
                    // constant, which names it where it is a string.
                    let operand = self.evaluated_class(class)?;
                    let written = match operand {
                        ClassOperand::Name(token) => self.text(token).to_vec(),
                        ClassOperand::Value(value) => match value.as_str() {
                            Some(text) => text.bytes().to_vec(),
                            None => {
                                return Err(self.compile_error(
                                    "Class name must be a valid object or a string",
                                ));
                            }
                        },
                        ClassOperand::Expr(_) => {
                            return Err(self.compile_error(
                                "Dynamic class names are not allowed in compile-time class constant references",
                            ));
                        }
                    };
                    if class_fetch_type(&written) == FETCH_CLASS_STATIC {
                        return Err(self.compile_error(
                            "\"static::\" is not allowed in compile-time constants",
                        ));
                    }
                    // `self` and `parent` keep the mark they were written
                    // with; any other name the one resolution leaves, a
                    // folded one resolved as `ZEND_NAME_FQ`.
                    let (class, class_kind) = match (class_fetch_type(&written), operand) {
                        (FETCH_CLASS_DEFAULT, ClassOperand::Value(_)) => {
                            (self.fq_name(&written)?, NameKind::FullyQualified)
                        }
                        (FETCH_CLASS_DEFAULT, _) => {
                            let (class, kind) = self.resolve_class_marked(&written);
                            (class.to_vec(), kind)
                        }
                        _ => (written, NameKind::NotFullyQualified),
                    };
                    // `self::{'X'}` folds to the name; one that does
                    // not fold to a string is kept as its own tree,
                    // evaluated before the class.
                    let Some(name) = self.folded_name(&name)? else {
                        let ScopedName::Computed(expr) = name else {
                            return Err(self.fatal("a written name that folded to nothing"));
                        };
                        return Ok(ConstExpr::DynamicClassConstant {
                            class: Name(class.into_boxed_slice()),
                            class_kind,
                            name: Box::new(self.const_expr(expr)?),
                            line,
                        });
                    };
                    // `zend_eval_const_expr`'s `ZEND_AST_CLASS_CONST`
                    // arm: a constant `zend_try_ct_eval_class_const`
                    // substitutes is its value
                    // ([`Self::ct_class_constant`]), which keeps its
                    // owner.
                    if let Some(value) = self.ct_class_constant(&class, name)? {
                        return self.folded(value.kept());
                    }
                    Ok(ConstExpr::ClassConstant {
                        class: Name(class.into_boxed_slice()),
                        class_kind,
                        name: Name(name.into()),
                        line,
                    })
                }
                Scoped::Property { .. } => {
                    Err(self.compile_error("Constant expression contains invalid operations"))
                }
                // `zend_compile_const_expr_class_name`, after
                // `zend_eval_const_expr` folded what
                // `zend_try_compile_const_expr_resolve_class_name` answers
                // for: a class the program computes and `static` are
                // php's compile-time fatals, and `self` or `parent`
                // where the scope is not known, in a trait, a closure or
                // code outside any function, is kept for the scope that evaluates
                // it (`zend_compile_const_expr_class_name`).
                Scoped::ClassName { class } => {
                    let operand = self.decoded_class(class)?;
                    match self.resolved_class_name(operand)? {
                        Some(Operated::Kept(folded)) => return self.folded(folded),
                        // `self::class` is the class's own name string
                        // (`ZVAL_STR_COPY`), no string of its own: the
                        // claim is one more owner of the name, kept for
                        // what holds the value.
                        Some(Operated::Made(made)) => return self.folded(made.value()),
                        None => {}
                    }
                    let ClassSpec::Name(token) = class else {
                        return Err(self.compile_error(
                            "(expression)::class cannot be used in constant expressions",
                        ));
                    };
                    // `self` or `parent` where the scope is not known
                    // keeps `ZEND_AST_CLASS_NAME` with the fetch type in
                    // place of the name, for the scope that evaluates it.
                    let class: &[u8] = match class_fetch_type(self.text(token)) {
                        FETCH_CLASS_STATIC => {
                            return Err(self.compile_error(
                                "static::class cannot be used for compile-time class name resolution",
                            ));
                        }
                        FETCH_CLASS_SELF => b"self",
                        FETCH_CLASS_PARENT => b"parent",
                        _ => {
                            return Err(self.internal("a class name that did not fold"));
                        }
                    };
                    Ok(ConstExpr::ClassName {
                        class: Name(class.into()),
                        line,
                    })
                }
            },
            // `ZEND_AST_ARRAY` in a constant expression is the unit's
            // constant array where every key and value folded
            // ([`Self::folded_array`]), and otherwise stays the tree it
            // was written as, each element's key and value, and a `...`
            // its `ZEND_AST_UNPACK` with no key; a `&` is what
            // `zend_is_allowed_in_const_expr` refuses.
            tree::Expr::ArrayExpr(id) => {
                let elements = self.elements_of(tree.list(tree[id].array_elements))?;
                // `zend_ast_create_list`: the list's line is its first
                // element's, whose `ZEND_AST_ARRAY_ELEM` is its value's.
                let line = match elements.first() {
                    Some(Element::Value { value, .. } | Element::Unpack(value)) => {
                        self.line_of(*value)
                    }
                    _ => line,
                };
                let mut items = Vec::with_capacity(elements.len());
                for &element in elements {
                    items.push(match element {
                        Element::Value { key, value } => {
                            let key = match key {
                                Some(key) => Some(self.const_expr(key)?),
                                None => None,
                            };
                            (key, self.const_expr(value)?)
                        }
                        Element::Unpack(value) => {
                            (None, ConstExpr::Unpack(Box::new(self.const_expr(value)?)))
                        }
                        Element::Ref { .. } => {
                            return Err(self
                                .compile_error("Constant expression contains invalid operations"));
                        }
                    });
                }
                if let Some(array) = self.interning(false, |this| this.folded_array(&items))? {
                    return Ok(ConstExpr::UnitValue(array));
                }
                Ok(ConstExpr::Array {
                    elements: items,
                    line,
                })
            }
            // The operators `zend_eval_const_expr` folds where it can
            // and leaves as the tree they were written as where it
            // cannot: the fold first, since php's own walk evaluates the
            // node before the constant expression is kept, and the
            // node's own shape after it, each child a constant
            // expression of its own.
            tree::Expr::UnaryExpr(id) => {
                if let Some(folded) = self.eval_const(expr)? {
                    return self.folded(folded);
                }
                let node = &tree[id];
                let operand = self.required(node.operand, "a unary operator")?;
                let op: &[u8] = match node.op.map(|op| self.kind(op)) {
                    Some(SyntaxKind::MINUS) => b"-",
                    Some(SyntaxKind::PLUS) => b"+",
                    Some(SyntaxKind::BANG) => b"!",
                    Some(SyntaxKind::TILDE) => b"~",
                    _ => return Err(self.unread("a unary operator")),
                };
                // `zend_ast_create` gives the node its operand's line.
                Ok(ConstExpr::Unary {
                    op: Name(op.into()),
                    operand: Box::new(self.const_expr(operand)?),
                    line: self.line_of(operand),
                })
            }
            tree::Expr::BinaryExpr(id) => {
                if let Some(folded) = self.eval_const(expr)? {
                    return self.folded(folded);
                }
                let node = &tree[id];
                let left = node.lhs;
                let right = self.required(node.rhs, "a binary operator")?;
                // `ZEND_AST_AND`, `ZEND_AST_OR` and `ZEND_AST_COALESCE` kept
                // under the spelling `zend_ast_export` writes them with;
                // the pipe is none of the nodes php's walk allows.
                let op: &[u8] = match node.op.map(|op| self.kind(op)) {
                    Some(SyntaxKind::AMP_AMP | SyntaxKind::AND_KW) => b"&&",
                    Some(SyntaxKind::PIPE_PIPE | SyntaxKind::OR_KW) => b"||",
                    Some(SyntaxKind::QUESTION_QUESTION) => b"??",
                    Some(SyntaxKind::PIPE_GT) => {
                        return Err(
                            self.compile_error("Constant expression contains invalid operations")
                        );
                    }
                    Some(kind) => match binary_op(kind) {
                        Some(op) => op.spelling().as_bytes(),
                        None => return Err(self.unread("a binary operator")),
                    },
                    None => return Err(self.unread("a binary operator")),
                };
                Ok(ConstExpr::Binary {
                    op: Name(op.into()),
                    lhs: Box::new(self.const_expr(left)?),
                    rhs: Box::new(self.const_expr(right)?),
                    line,
                })
            }
            // `ZEND_AST_CONDITIONAL`, folded where
            // `zend_eval_const_expr` folds it and kept otherwise.
            tree::Expr::TernaryExpr(id) => {
                if let Some(folded) = self.eval_const(expr)? {
                    return self.folded(folded);
                }
                let node = &tree[id];
                let (cond, then, otherwise) = (node.cond, node.then, node.else_branch);
                let otherwise = self.required(otherwise, "a conditional")?;
                let cond = Box::new(self.const_expr(cond)?);
                let then = match then {
                    Some(then) => Some(Box::new(self.const_expr(then)?)),
                    None => None,
                };
                Ok(ConstExpr::Conditional {
                    cond,
                    then,
                    otherwise: Box::new(self.const_expr(otherwise)?),
                })
            }
            // `ZEND_AST_DIM`: `[]` with nothing in it is php's compile
            // error, and a dimension `zend_eval_const_expr` does not fold
            // is kept for the engine, which reads it as `FETCH_DIM_R`
            // reads.
            tree::Expr::SubscriptExpr(id) => {
                let node = &tree[id];
                let (base, index) = (node.base, node.index);
                let Some(index) = index else {
                    return Err(self.compile_error("Cannot use [] for reading"));
                };
                if let Some(folded) = self.eval_const(expr)? {
                    return self.folded(folded);
                }
                Ok(ConstExpr::Dim {
                    container: Box::new(self.const_expr(base)?),
                    dim: Box::new(self.const_expr(index)?),
                    line,
                })
            }
            // `ZEND_AST_PROP` and `ZEND_AST_NULLSAFE_PROP`: the object and
            // the name, which may be an expression written in braces; a
            // name read out of a variable is none of the nodes php's walk
            // allows.
            tree::Expr::MemberAccessExpr(id) => {
                let node = &tree[id];
                let (object, member) = (node.obj, node.member);
                let nullsafe = self.is_nullsafe(id);
                let object = Box::new(self.const_expr(object)?);
                let name = match member {
                    Some(tree::MemberSelector::MemberName(member)) => {
                        let Some(name) = self.parsed.tree[member].name else {
                            return Err(self.unread("a property fetch"));
                        };
                        ConstExpr::Str(self.text(name).into())
                    }
                    Some(tree::MemberSelector::BracedMember(braced)) => {
                        let inner = self.required(self.parsed.tree[braced].inner, "a property")?;
                        self.const_expr(inner)?
                    }
                    _ => {
                        return Err(
                            self.compile_error("Constant expression contains invalid operations")
                        );
                    }
                };
                Ok(ConstExpr::Prop {
                    object,
                    name: Box::new(name),
                    nullsafe,
                    line,
                })
            }
            // `ZEND_AST_CAST`, folded where `zend_eval_const_expr` folds
            // it; `(object)` is php's compile error where the expression
            // is evaluated once.
            tree::Expr::CastExpr(id) => {
                let node = &tree[id];
                let (op, operand) = (node.op, node.operand);
                let to = self.cast_to(op, line)?;
                if to == CastTo::Object && self.const_evaluation == Evaluation::Once {
                    return Err(
                        self.compile_error("Object casts are not supported in this context")
                    );
                }
                if let Some(folded) = self.eval_const(expr)? {
                    return self.folded(folded);
                }
                let operand = self.required(operand, "a cast")?;
                Ok(ConstExpr::Cast {
                    to: to.ast_code(),
                    operand: Box::new(self.const_expr(operand)?),
                    line: self.line_of(operand),
                })
            }
            tree::Expr::NewExpr(id) => self.const_new(id),
            tree::Expr::ClosureExpr(_) => self.const_closure(expr, line),
            tree::Expr::CallExpr(id) => self.const_callable(id, line),
            // `zend_is_allowed_in_const_expr` refuses these in php's
            // words: a variable, a call, an assignment, an increment,
            // `@`, `throw`, `include`, `isset` and an interpolated
            // string are none of the nodes php's walk allows.
            tree::Expr::SimpleVarExpr(_)
            | tree::Expr::VarVarExpr(_)
            | tree::Expr::DynamicVarExpr(_)
            | tree::Expr::AssignExpr(_)
            | tree::Expr::ArrowFnExpr(_)
            | tree::Expr::CloneExpr(_)
            | tree::Expr::ExitExpr(_)
            | tree::Expr::PrefixIncDecExpr(_)
            | tree::Expr::PostfixIncDecExpr(_)
            | tree::Expr::ErrorSuppressExpr(_)
            | tree::Expr::ThrowExpr(_)
            | tree::Expr::IncludeExpr(_)
            | tree::Expr::IssetExpr(_)
            | tree::Expr::EmptyExpr(_)
            | tree::Expr::EvalExpr(_)
            | tree::Expr::ShellExecExpr(_)
            | tree::Expr::ListExpr(_)
            | tree::Expr::PrintExpr(_)
            | tree::Expr::YieldExpr(_)
            | tree::Expr::YieldFromExpr(_)
            | tree::Expr::InterpolatedStringExpr(_)
            | tree::Expr::HeredocExpr(_) => self.const_refused(expr),
            // What this compiler cannot keep as the tree it was written
            // as yet: a closure (8.5), whose body would be a function of
            // the unit the evaluation has no way to name, and a node php's
            // walk refuses that folds to nothing.
            _ => match self.eval_const(expr)? {
                Some(folded) => self.folded(folded),
                None => Err(self.unread("a constant expression")),
            },
        }
    }

    /// `zend_try_ct_eval_array` over the elements [`Self::const_expr`]
    /// kept for an array: where every key and value is a value, the one
    /// constant array laid down where the unit's strings are
    /// ([`Self::lay_array`]), its keys written as php writes them, which
    /// php's table of defaults holds as a value; nothing where an
    /// element is still to evaluate or a key leaves the array to the
    /// request.
    fn folded_array(
        &mut self,
        items: &[(Option<ConstExpr>, ConstExpr)],
    ) -> Result<Option<UnitValue>, CompileError> {
        let mut folded = Vec::with_capacity(items.len());
        let mut claims = Vec::new();
        let mut value_of = |this: &mut Self, expr: &ConstExpr| {
            Ok::<_, CompileError>(this.operated_of(expr)?.map(|operated| match operated {
                Operated::Made(made) => {
                    let value = made.value();
                    claims.push(made);
                    value
                }
                Operated::Kept(value) => value,
            }))
        };
        for (key, value) in items {
            if let ConstExpr::Unpack(inner) = value {
                let Some(inner) = value_of(self, inner)? else {
                    return Ok(None);
                };
                folded.push(FoldedElement::Unpack(inner));
                continue;
            }
            let key = match key {
                Some(key) => match value_of(self, key)? {
                    Some(key) => Some(key),
                    None => return Ok(None),
                },
                None => None,
            };
            let Some(value) = value_of(self, value)? else {
                return Ok(None);
            };
            folded.push(FoldedElement::Pair(key, value));
        }
        let Some(entries) = self.keyed_entries(folded)? else {
            return Ok(None);
        };
        let array = self.lay_array(&entries, claims, items.len())?;
        Ok(UnitValue::new(array.value()))
    }

    /// The value a folded constant expression stands for, none for one
    /// still to evaluate, as its reader holds it: a string made afresh
    /// for the read is the reader's alone ([`Operated::Made`]), which a
    /// fold gives back once the node over it stands, as php's operand
    /// that `ZVAL_COPY_OR_DUP` duplicated is freed there. A constant of
    /// the class being compiled is read as the string its fold made
    /// ([`Self::held_value`]), which `ZVAL_COPY_OR_DUP` shares, so
    /// sixteen reads of a megabyte allocate nothing and the fold's peak
    /// is its left operand and the string it joins. `A` is folded from a
    /// short source, so the copy of the source php's scanner makes
    /// (`zend_prepare_string_for_scanning`), which the engine does not,
    /// is too small to hide a megabyte read afresh:
    ///
    /// ```php
    /// $k = "    const B0 = 'abcdefgh';\n";
    /// for ($i = 1; $i <= 16; $i++) { $k .= "    const B$i = self::B" . ($i - 1) . " . self::B" . ($i - 1) . ";\n"; }
    /// $x = implode(' . ', array_fill(0, 16, 'self::A'));
    /// $code = "class C {\n{$k}    const A = self::B16 . self::B16;\n    const X = $x;\n}";
    /// $m = memory_get_usage();
    /// memory_reset_peak_usage();
    /// eval($code);
    /// $peak = (memory_get_peak_usage() - $m) / 1e6;
    /// echo abs($peak - 34.7) < 0.5 ? 'within 0.5 MB of 34.7 MB' : $peak, ' ', strlen(C::A), ' ', strlen(C::X), "\n";
    /// ```
    /// ```expect regime=jit-off
    /// within 0.5 MB of 34.7 MB 1048576 16777216
    /// ```
    fn operated_of(&mut self, expr: &ConstExpr) -> Result<Option<Operated>, CompileError> {
        Ok(Some(match expr {
            ConstExpr::Null => Operated::Kept(Zval::null()),
            ConstExpr::Bool(b) => Operated::Kept(Zval::bool(*b)),
            ConstExpr::Long { value, .. } => Operated::Kept(Zval::long(*value)),
            ConstExpr::Double(d) => Operated::Kept(Zval::double(*d)),
            ConstExpr::Str(bytes) => {
                Operated::Made(self.string_made(bytes, false, Sizing::Alloc)?)
            }
            ConstExpr::UnitValue(held) => Operated::Kept(held.value()),
            _ => return Ok(None),
        }))
    }

    /// A value `zend_eval_const_expr` folded, as the constant expression
    /// a declaration keeps.
    fn folded(&mut self, folded: Zval) -> Result<ConstExpr, CompileError> {
        self.folded_string = folded.as_str().map(|_| folded);
        folded_const_expr(folded).ok_or_else(|| self.unread("a constant expression"))
    }

    /// `value`, the constant expression a class's constant or a
    /// property's default compiled to, as the declaration holds it: the
    /// string its fold made where that is one the unit counts
    /// ([`UnitValue`]), which php's `constants_table` or default table
    /// keeps as the zval the compile put there, so the class's row and
    /// each read find it by identity and never by its bytes; else as it
    /// stands.
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3263-class-values.inc';
    /// file_put_contents($f, <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}');
    /// foreach ([
    ///     'class S1 { const K = "abcdefghij5"; }',
    ///     '$GLOBALS["a"] = S1::K;',
    ///     'class S2 { const K = "abcdefghij" . 5; }',
    ///     '$GLOBALS["a"] = S2::K;',
    ///     'interface S3 { const K = "abcdefghij" . "5"; }',
    ///     '$GLOBALS["a"] = S3::K;',
    /// ] as $c) echo m($c), "  ", $c, "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 536  class S1 { const K = "abcdefghij5"; }
    /// 888  $GLOBALS["a"] = S1::K;
    /// 544  class S2 { const K = "abcdefghij" . 5; }
    /// 0  $GLOBALS["a"] = S2::K;
    /// 544  interface S3 { const K = "abcdefghij" . "5"; }
    /// 0  $GLOBALS["a"] = S3::K;
    /// ```
    ///
    /// A class name a constant resolves to is such a value: `B10::class` and
    /// `Bxyz::class` a string of their own, as php's scanner made one for the
    /// name written, even where it names the class itself, whose own name is
    /// another string; `self::class` the class's name itself
    /// (`ZVAL_STR_COPY`):
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3268-class-names.inc';
    /// file_put_contents($f, <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// foreach ([
    ///     'class B9 { const K = "abcdefghij"; }',
    ///     'class B10 { const K = B10::class; }',
    ///     'class B11 { const K = Bxyz::class; }',
    ///     'class B12 { const K = self::class; }',
    ///     '$GLOBALS["a"] = B10::K;',
    ///     '$GLOBALS["b"] = [B9::K, B11::K, B12::K];',
    /// ] as $c) echo m($c), "  ", $c, "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 536  class B9 { const K = "abcdefghij"; }
    /// 528  class B10 { const K = B10::class; }
    /// 528  class B11 { const K = Bxyz::class; }
    /// 496  class B12 { const K = self::class; }
    /// 512  $GLOBALS["a"] = B10::K;
    /// 216  $GLOBALS["b"] = [B9::K, B11::K, B12::K];
    /// ```
    fn held_value(&mut self, value: ConstExpr) -> ConstExpr {
        let folded = self.folded_string.take();
        match (&value, folded.and_then(UnitValue::new)) {
            (ConstExpr::Str(bytes), Some(held))
                if held.value().as_str().is_some_and(|s| s.bytes() == &**bytes) =>
            {
                ConstExpr::UnitValue(held)
            }
            _ => value,
        }
    }

    /// [`Self::const_expr`] of a property's default, as the declaration
    /// holds it ([`Self::held_value`]).
    fn held_const_expr(&mut self, expr: tree::Expr) -> Result<ConstExpr, CompileError> {
        self.folded_string = None;
        let value = self.const_expr(expr)?;
        Ok(self.held_value(value))
    }

    /// [`Self::const_expr`] for an expression evaluated each time it is
    /// read, php's `allow_dynamic`: a parameter's default, an attribute's
    /// argument and a `const` statement's value ([`Evaluation`]).
    fn dynamic_const_expr(&mut self, expr: tree::Expr) -> Result<ConstExpr, CompileError> {
        let outer = std::mem::replace(&mut self.const_evaluation, Evaluation::EachTime);
        let built = self.const_expr(expr);
        self.const_evaluation = outer;
        built
    }

    /// `ZEND_AST_NEW` in a constant expression
    /// (`zend_compile_const_expr_new`, `Z/Zend/zend_compile.c:11470`):
    /// php's compile error where the expression is evaluated once; then
    /// the class a name written out, never an anonymous class, a class
    /// the program computes or `static`, resolved in the namespace, with
    /// `self` and `parent` kept for the scope that evaluates it; and the
    /// arguments as `zend_compile_const_expr_args` holds them, `(...)`,
    /// an unpacked argument and a positional one after a named one each
    /// php's compile error.
    ///
    /// ```php
    /// class P { function __construct(public $x = 'p') {} }
    /// class C extends P {
    ///     function __construct(public $x = 0, public $y = 0) {}
    ///     static function make($a = new self(1, y: 2), $b = new parent, $c = new stdClass) { return [$a, $b, $c]; }
    /// }
    /// var_dump(C::make());
    /// ```
    /// ```expect
    /// array(3) {
    ///   [0]=>
    ///   object(C)#1 (2) {
    ///     ["x"]=>
    ///     int(1)
    ///     ["y"]=>
    ///     int(2)
    ///   }
    ///   [1]=>
    ///   object(P)#2 (1) {
    ///     ["x"]=>
    ///     string(1) "p"
    ///   }
    ///   [2]=>
    ///   object(stdClass)#3 (0) {
    ///   }
    /// }
    /// ```
    fn const_new(&mut self, id: NodeId<tree::NewExpr>) -> Result<ConstExpr, CompileError> {
        if self.const_evaluation == Evaluation::Once {
            return Err(self.compile_error("New expressions are not supported in this context"));
        }
        let line = self.line;
        let node = &self.parsed.tree[id];
        let (anon, ctor, args) = (node.anon, node.ctor, node.args);
        if anon.is_some() {
            return Err(self.compile_error("Cannot use anonymous class in constant expression"));
        }
        let Some(ctor) = ctor else {
            return Err(self.unread("a `new`"));
        };
        let ClassSpec::Name(token) = ClassSpec::of(ctor) else {
            return Err(self.compile_error("Cannot use dynamic class name in constant expression"));
        };
        let class_line = self.line_of(ctor);
        let written = self.text(token).to_vec();
        if class_fetch_type(&written) == FETCH_CLASS_STATIC {
            return Err(self.compile_error("\"static\" is not allowed in compile-time constants"));
        }
        let class = self.class_name(&written);
        let mut arguments = Vec::new();
        if let Some(args) = args {
            let args::Args::Written { list, .. } = self.arguments(args, line)? else {
                return Err(self.compile_error("Cannot create Closure for new expression"));
            };
            let mut named = false;
            for &argument in list {
                arguments.push(match argument {
                    args::Arg::Positional(_) if named => {
                        return Err(self
                            .compile_error("Cannot use positional argument after named argument"));
                    }
                    args::Arg::Positional(Source::Expr(value)) => (None, self.const_expr(value)?),
                    args::Arg::Named(name, Source::Expr(value)) => {
                        named = true;
                        (Some(Name(name.into())), self.const_expr(value)?)
                    }
                    args::Arg::Unpack(_) => {
                        return Err(self.compile_error(
                            "Argument unpacking in constant expressions is not supported",
                        ));
                    }
                    args::Arg::Positional(Source::Node(_))
                    | args::Arg::Named(_, Source::Node(_)) => {
                        return Err(self.fatal("a constant `new` over a value the compiler holds"));
                    }
                });
            }
        }
        Ok(ConstExpr::New {
            class: Name(class.into()),
            arguments,
            line: class_line,
        })
    }

    /// `zend_compile_const_expr_closure` (`Z/Zend/zend_compile.c:11481`,
    /// 8.5): a closure in a constant expression is `static` and binds
    /// no variable, each php's compile-time fatal where it is not, and
    /// is compiled as `zend_compile_func_decl` compiles one at
    /// `FUNC_DECL_LEVEL_CONSTEXPR`: a function of the unit named as
    /// every closure is, which no op array declares and no opline
    /// makes, kept in the tree for the engine to make a closure of in
    /// the scope that evaluates it.
    ///
    /// ```php
    /// const G = static function (int $x): int { return $x + 1; };
    /// var_dump((G)(1), G instanceof Closure);
    /// #[Attribute] class A { public function __construct(public Closure $c) {} }
    /// #[A(static function () { return 'attr'; })] class U {}
    /// var_dump(((new ReflectionClass('U'))->getAttributes()[0]->newInstance()->c)());
    /// ```
    /// ```expect
    /// int(2)
    /// bool(true)
    /// string(4) "attr"
    /// ```
    ///
    /// ```php
    /// const F = function () {};
    /// ```
    /// ```expect exit=255
    /// Fatal error: Closures in constant expressions must be static in %s on line 2
    /// ```
    ///
    /// ```php
    /// const F = static function () use ($x) {};
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use(...) variables in constant expression in %s on line 2
    /// ```
    fn const_closure(&mut self, expr: tree::Expr, line: u32) -> Result<ConstExpr, CompileError> {
        let closure = self.walk_closure(expr, line)?;
        if !closure.is_static {
            return Err(self.compile_error("Closures in constant expressions must be static"));
        }
        if !closure.uses.is_empty() {
            return Err(self.compile_error("Cannot use(...) variables in constant expression"));
        }
        let name = self.closure_name(closure.function.line);
        self.imports.see_closure(&name);
        let at = u32::try_from(self.dynamic.len()).unwrap_or(u32::MAX);
        self.dynamic.push(None);
        let flags = FnFlags {
            closure: true,
            is_static: true,
            ..FnFlags::default()
        };
        let active = ActiveFunction::at(&name, crate::FunctionAt::Dynamic(at as usize));
        let saved = self.begin_body(active, false, flags);
        let outcome = self.parameters_and_body(&closure.function, &name, None, &[], None);
        self.end_body(saved);
        if let Some(slot) = self.dynamic.get_mut(at as usize) {
            *slot = Some(outcome?);
        }
        Ok(ConstExpr::Closure {
            at,
            name: Name(name.into_boxed_slice()),
            unit: self.cell.share(),
        })
    }

    /// `zend_compile_const_expr_fcc` (`Z/Zend/zend_compile.c:11501`,
    /// 8.5): a call in a constant expression is a first-class callable,
    /// `f(...)` or `C::m(...)`, and any other call php's `Constant
    /// expression contains invalid operations`. The function is named
    /// as written, resolved as a call resolves it and marked fully
    /// qualified where no global fallback applies; a name the program
    /// computes, and a value that is no string, are php's fatals. The
    /// class is a name, `self` or `parent`, never `static` nor one the
    /// program computes, and the method a name as written.
    ///
    /// ```php
    /// function f($cb = strlen(...), $up = strtoupper(...)) { return [$cb('abc'), $up('x')]; }
    /// var_dump(f());
    /// class C { static function m() { return 'm'; } const M = C::m(...); const N = C::{'m'}(...); }
    /// var_dump((C::M)(), (C::N)());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   int(3)
    ///   [1]=>
    ///   string(1) "X"
    /// }
    /// string(1) "m"
    /// string(1) "m"
    /// ```
    ///
    /// ```php
    /// const F = strlen('x');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Constant expression contains invalid operations in %s on line 2
    /// ```
    ///
    /// ```php
    /// const F = (0)(...);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Illegal function name in %s on line 2
    /// ```
    fn const_callable(
        &mut self,
        id: NodeId<tree::CallExpr>,
        line: u32,
    ) -> Result<ConstExpr, CompileError> {
        let (callee, args) = (self.parsed.tree[id].callee, self.parsed.tree[id].args);
        if !matches!(self.arguments(args, line)?, Args::CallableConvert) {
            return Err(self.compile_error("Constant expression contains invalid operations"));
        }
        // php's parser drops the parentheses around a callee, so what
        // they hold is the node itself: `(C)(...)` a constant's fetch,
        // which is no name, and `('f')(...)` a value it wrote.
        let peeled = self.peel(callee);
        let named = matches!(callee, tree::Expr::NameRef(_));
        match peeled {
            // `ZEND_AST_METHOD_CALL` and `ZEND_AST_NULLSAFE_METHOD_CALL`
            // are none of the nodes `zend_is_allowed_in_const_expr` allows.
            tree::Expr::MemberAccessExpr(_) => {
                Err(self.compile_error("Constant expression contains invalid operations"))
            }
            tree::Expr::NameRef(name) if named => {
                let written = self.text(name).to_vec();
                let (resolved, fully_qualified) =
                    self.resolve_non_class(&written, Symbol::Function);
                let name_kind = if fully_qualified {
                    NameKind::FullyQualified
                } else {
                    NameKind::NotFullyQualified
                };
                Ok(self.function_callable(resolved.to_vec(), name_kind, line))
            }
            // `name_ast->kind == ZEND_AST_ZVAL`: a value the parser
            // wrote, which names the function where it is a string and
            // is php's fatal where it is not; the string is no name
            // written and carries `ZEND_NAME_FQ`.
            written if self.is_written_value(written) => {
                let value = self.constant_of(written)?;
                let Some(text) = value.and_then(php_heap::Zval::as_str) else {
                    return Err(self.compile_error("Illegal function name"));
                };
                Ok(self.function_callable(text.bytes().to_vec(), NameKind::FullyQualified, line))
            }
            tree::Expr::ScopedAccessExpr(scoped) => self.const_method_callable(scoped, line),
            _ => Err(self.compile_error("Cannot use dynamic function name in constant expression")),
        }
    }

    /// One `name(...)` node, numbered among the unit's.
    fn function_callable(&mut self, name: Vec<u8>, name_kind: NameKind, line: u32) -> ConstExpr {
        let at = self.function_callables;
        self.function_callables = self.function_callables.saturating_add(1);
        ConstExpr::FunctionCallable {
            name: Name(name.into_boxed_slice()),
            name_kind,
            line,
            at,
            unit: self.cell.share(),
        }
    }

    /// [`Self::const_callable`]'s `ZEND_AST_STATIC_CALL` arm, over
    /// `zend_compile_const_expr_class_reference`.
    fn const_method_callable(
        &mut self,
        scoped: NodeId<tree::ScopedAccessExpr>,
        line: u32,
    ) -> Result<ConstExpr, CompileError> {
        let node = &self.parsed.tree[scoped];
        let (scope, member) = (node.scope, node.member);
        let class = match self.peel(scope) {
            tree::Expr::NameRef(token) => self.text(token).to_vec(),
            tree::Expr::NewExpr(_) => {
                return Err(self.compile_error("Cannot use anonymous class in constant expression"));
            }
            _ => {
                return Err(
                    self.compile_error("Cannot use dynamic class name in constant expression")
                );
            }
        };
        let class = match class_fetch_type(&class) {
            FETCH_CLASS_STATIC => {
                return Err(
                    self.compile_error("\"static\" is not allowed in compile-time constants")
                );
            }
            FETCH_CLASS_DEFAULT => self.resolve_class(&class).to_vec(),
            _ => class,
        };
        let method = match member {
            Some(tree::ScopedSelector::MemberName(member)) => match self.parsed.tree[member].name {
                Some(name) => self.text(name).to_vec(),
                None => return Err(self.unread("a method name")),
            },
            Some(tree::ScopedSelector::BracedMember(braced)) => {
                let written = self.parsed.tree[braced]
                    .inner
                    .filter(|inner| self.is_written_value(*inner));
                let Some(written) = written else {
                    return Err(
                        self.compile_error("Cannot use dynamic method name in constant expression")
                    );
                };
                let value = self.constant_of(written)?;
                let Some(text) = value.and_then(php_heap::Zval::as_str) else {
                    return Err(self.compile_error("Illegal method name"));
                };
                text.bytes().to_vec()
            }
            _ => {
                return Err(
                    self.compile_error("Cannot use dynamic method name in constant expression")
                );
            }
        };
        Ok(ConstExpr::MethodCallable {
            class: Name(class.into_boxed_slice()),
            method: Name(method.into_boxed_slice()),
            line,
        })
    }

    /// `zend_is_allowed_in_const_expr`'s refusal, php's
    /// `Constant expression contains invalid operations`, for a node
    /// that is no constant. A node this compiler cannot read yet keeps
    /// its own refusal, so the phpt lane counts the file as unread (an
    /// argument list with a `...` in it is php's first-class callable,
    /// which its walk allows and this does not read), and every other
    /// refusal the fold met is the answer as it stands.
    fn const_refused(&mut self, expr: tree::Expr) -> Result<ConstExpr, CompileError> {
        match self.constant_expr(expr) {
            Ok(folded) => self.folded(folded),
            Err(Unfolded::Unread(refused) | Unfolded::Refused(refused)) => Err(refused),
            Err(Unfolded::NoConstant(_)) => {
                Err(self.compile_error("Constant expression contains invalid operations"))
            }
        }
    }

    /// `zend_compile_declare` (`Z/Zend/zend_compile.c:6982`): each
    /// directive in written order, its value held to `ZEND_AST_ZVAL`
    /// before its name is looked at, then the statement written after
    /// the `)` where one is.
    ///
    /// `strict_types` is the one directive this compiler serves.
    /// `zend_is_first_statement` with `allow_nop` holds it to the
    /// file's own statement list with nothing but other `declare`s
    /// before it, which is [`Emit::statements_seen`] at zero at
    /// [`Emit::depth`] one; a statement after the `)` is the braced
    /// form php refuses; and its value must be the integer 0 or 1.
    /// A 1 sets [`Emit::typing`] to [`Typing::Strict`], which every op
    /// array of the file then carries.
    ///
    /// `ticks` is the count [`Emit::tick`] emits `ZEND_TICKS` with after
    /// every statement in its scope, the value read as `zval_get_long`
    /// reads it and zero turning the ticks off, and the scope the rest of
    /// the file or the statement after the `)`. `encoding` is the
    /// parser's to read ([`crate::multibyte`]); here it is held to
    /// `zend_is_first_statement` without `allow_nop`, so an empty
    /// statement before it refuses it too ([`Emit::top_list`]). Every
    /// other name is php's `E_COMPILE_WARNING`, and the file compiles on.
    fn declare(&mut self, id: NodeId<tree::DeclareStmt>) -> Result<(), CompileError> {
        let node = &self.parsed.tree[id];
        if node.stmt_list.is_some() {
            return Err(self.unread("a `declare` in the alternative syntax"));
        }
        let body = node.body.filter(|body| !matches!(body, Stmt::EmptyStmt(_)));
        let before = self.ticks;
        for directive in self.list(node.declare_directives) {
            let directive = &self.parsed.tree[*directive];
            self.line = directive.line();
            let Some(written_value) = directive.value else {
                return Err(self.unread("a `declare` directive"));
            };
            let written = self.text(directive.name);
            let name = String::from_utf8_lossy(written).into_owned();
            // `(*value_ast_ptr)->kind != ZEND_AST_ZVAL`: the value is one
            // token the scanner made a zval of, so `-1`, `true` and
            // `1 + 0` are each the fatal and not a fold.
            let Some(value) = self.literal_of(written_value) else {
                return Err(self.compile_error(format!("declare({name}) value must be a literal")));
            };
            if written.eq_ignore_ascii_case(b"strict_types") {
                if self.depth != 1 || self.statements_seen > 0 {
                    return Err(self.compile_error(
                        "strict_types declaration must be the very first statement in the script",
                    ));
                }
                if body.is_some() {
                    return Err(
                        self.compile_error("strict_types declaration must not use block mode")
                    );
                }
                match value {
                    Literal::Int(0) => {}
                    Literal::Int(1) => self.typing = Typing::Strict,
                    _ => {
                        return Err(self.compile_error(
                            "strict_types declaration must have 0 or 1 as its value",
                        ));
                    }
                }
            } else if written.eq_ignore_ascii_case(b"ticks") {
                // `zend_const_expr_to_zval` and `zval_get_long`.
                let count = match value {
                    Literal::Int(n) => n,
                    Literal::Other => self.constant_of(written_value)?.map_or(0, long_of),
                };
                self.ticks = NonZeroI64::new(count);
            } else if written.eq_ignore_ascii_case(b"encoding") {
                // `zend_is_first_statement` without `allow_nop`: an empty
                // statement before it is one too.
                if self.depth != 1 || self.top_list != TopList::Declares {
                    return Err(self.compile_error(
                        "Encoding declaration pragma must be the very first statement in the script",
                    ));
                }
            } else {
                self.raise(
                    Level::CompileWarning,
                    &format!("Unsupported declare '{name}'"),
                );
            }
        }
        // The statement written after the `)`, which php's grammar
        // leaves `NULL` for the `;` spelling and `zend_compile_declare`
        // compiles as the braced form where it is anything else: one
        // statement at its own line, as a body's is.
        if let Some(body) = body {
            self.statements(&[body])?;
            self.ticks = before;
        }
        Ok(())
    }

    /// `zend_compile_const_decl` (`Z/Zend/zend_compile.c:9667`): one
    /// `DECLARE_CONST` per declarator, in written order, its `op1` the
    /// name `zend_prefix_with_ns` made of the written one and its `op2`
    /// the value `zend_const_expr_to_zval` folded. `true`, `false` and
    /// `null`, whatever their case, are `zend_get_special_const`'s
    /// `Cannot redeclare constant '%s'`, raised before the name is
    /// prefixed, so the sentence names the written one. Whether the name
    /// stands already is the request's and not the compiler's, so
    /// nothing else is refused here.
    ///
    /// php keeps a value it could not fold as an `IS_CONSTANT_AST` for
    /// the opline to evaluate (`allow_dynamic`); this compiler folds
    /// what it can fold and refuses the rest as not read yet.
    ///
    /// Attributes before the statement are its one constant's, compiled
    /// after the declarators at the first one's line with the target
    /// `ZEND_ATTRIBUTE_TARGET_CONST`: a statement of more constants is
    /// php's compile error, and the last `DECLARE_CONST` becomes
    /// `DECLARE_ATTRIBUTED_CONST` with an `OP_DATA` after it naming the
    /// constant's declaration, [`Unit::constants`], where php's names
    /// the attribute table.
    ///
    /// ```php
    /// #[\Foo]
    /// const First = 1,
    ///     Second = 2;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot apply attributes to multiple constants at once in %s on line 3
    /// ```
    ///
    /// ```php
    /// #[Attribute]
    /// const EXAMPLE = 'Foo';
    /// ```
    /// ```expect exit=255
    /// Fatal error: Attribute "Attribute" cannot target constant (allowed targets: class) in %s on line 3
    /// ```
    ///
    /// ```php
    /// namespace Foo;
    /// #[Attr(1, x: 2)]
    /// const MyConst = 'v';
    /// $a = (new \ReflectionConstant('Foo\\MyConst'))->getAttributes()[0];
    /// var_dump(MyConst, $a->getName(), $a->getArguments());
    /// ```
    /// ```expect
    /// string(1) "v"
    /// string(8) "Foo\Attr"
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   ["x"]=>
    ///   int(2)
    /// }
    /// ```
    fn const_decl(&mut self, id: NodeId<tree::ConstStmt>) -> Result<(), CompileError> {
        let node = self.parsed.tree[id];
        let mut declared = None;
        for declarator in self.list(node.const_declarators) {
            let (name, value, line) = self.const_declarator(*declarator)?;
            self.line = line;
            // The fold, or the tree `DECLARE_CONST` evaluates as it runs.
            let (value, unevaluated) = if let Some(folded) = self.eval_const(value)? {
                (folded, None)
            } else {
                let tree = self.dynamic_const_expr(value)?;
                (self.unevaluated(tree.clone()), Some(tree))
            };
            if [&b"true"[..], b"false", b"null"]
                .iter()
                .any(|special| name.eq_ignore_ascii_case(special))
            {
                return Err(self.compile_error(format!(
                    "Cannot redeclare constant '{}'",
                    String::from_utf8_lossy(&name)
                )));
            }
            let full = self.declare_symbol(Symbol::Const, &name)?;
            declared = Some((full.clone(), value, unevaluated));
            let name = self.string(&full)?;
            let op1 = self.literal(name);
            let op2 = self.literal(value);
            self.push(Opline {
                opcode: Opcode::DeclareConst,
                op1_type: OperandKind::Const,
                op1,
                op2_type: OperandKind::Const,
                op2,
                ..Opline::default()
            });
        }
        if node.attrs.is_empty() {
            return Ok(());
        }
        let Some((name, value, unevaluated)) = declared else {
            return Err(self.internal("an attributed `const` that declared nothing"));
        };
        let first = self
            .list(node.const_declarators)
            .first()
            .map_or(self.line, |declarator| self.parsed.tree[*declarator].line());
        self.line = first;
        if node.const_declarators.len() > 1 {
            return Err(self.compile_error("Cannot apply attributes to multiple constants at once"));
        }
        let written = self.walk_attributes(self.list(node.attrs))?;
        let attributes =
            self.member_attributes(&written, attributes::TARGET_CONST, 0, first, &[])?;
        // The value `ZEND_DECLARE_ATTRIBUTED_CONST` evaluates, which a
        // value the compiler could not fold is the tree of.
        let Some(value) = unevaluated.or_else(|| folded_const_expr(value)) else {
            return Err(self.internal("an attributed `const` whose value folded to no scalar"));
        };
        let at = u32::try_from(self.attributed_constants.len()).unwrap_or(u32::MAX);
        self.attributed_constants.push(ConstDecl {
            name: Name(name.into_boxed_slice()),
            value,
            attributes,
        });
        if let Some(last) = self.oplines.last_mut() {
            last.opcode = Opcode::DeclareAttributedConst;
        }
        let op1 = self.literal(Zval::ptr(at));
        self.push(Opline {
            opcode: Opcode::OpData,
            op1_type: OperandKind::Const,
            op1,
            ..Opline::default()
        });
        Ok(())
    }

    /// One `name = value` declarator of a `const` statement or a class
    /// constant, php's `ZEND_AST_CONST_ELEM`: the name as written,
    /// unqualified, the expression after the `=`, and the line the name
    /// stands on, which is where php's fatal over it is raised.
    fn const_declarator(
        &self,
        id: NodeId<tree::ConstDeclarator>,
    ) -> Result<(Vec<u8>, tree::Expr, u32), CompileError> {
        let declarator = &self.parsed.tree[id];
        let line = declarator.line();
        let Some(name) = self.parsed.tree[declarator.name].name else {
            return Err(walk::unread(line, "a `const` whose name is no identifier"));
        };
        let Some(value) = declarator.value else {
            return Err(walk::unread(line, "a `const` declarator with no value"));
        };
        Ok((self.text(name).to_vec(), value, line))
    }

    /// `zend_compile_stmt_list`: each statement at its own line.
    fn statements(&mut self, stmts: &[Stmt]) -> Result<(), CompileError> {
        self.depth = self.depth.saturating_add(1);
        for stmt in stmts {
            if matches!(stmt, Stmt::EmptyStmt(_)) {
                continue;
            }
            self.line = stmt.line(self.parsed);
            self.stmt(*stmt)?;
        }
        self.depth = self.depth.saturating_sub(1);
        Ok(())
    }

    /// `zend_compile_stmt` (`Z/Zend/zend_compile.c:11685`): an
    /// `EXT_STMT` where the engine compiles with extended info
    /// ([`extended`]), the statement, then a `ZEND_TICKS` where a
    /// `declare(ticks)` scope stands, each where the statement is none of
    /// `zend_is_unticked_stmt`'s, a block or a label here (the class
    /// members it also names are no statements of this walk, and
    /// [`Self::member_statements`] stands for them). `echo`, `unset`,
    /// `global` and `static` are php's `ZEND_AST_STMT_LIST` of one
    /// statement per element, so each element is preceded and ticked
    /// where [`Self::statement`] compiles it, and the list is neither.
    pub(super) fn stmt(&mut self, stmt: Stmt) -> Result<(), CompileError> {
        if !matches!(
            stmt,
            Stmt::Block(_)
                | Stmt::LabelStmt(_)
                | Stmt::EchoStmt(_)
                | Stmt::UnsetStmt(_)
                | Stmt::GlobalStmt(_)
                | Stmt::StaticVarStmt(_)
        ) {
            self.ext_stmt();
        }
        self.statement(stmt)?;
        if !matches!(stmt, Stmt::Block(_) | Stmt::LabelStmt(_)) {
            self.tick();
        }
        Ok(())
    }

    /// `zend_compile_stmt` over the members of a class body it does not
    /// leave unticked, a property group and an enum case, one per line
    /// in `lines`: each one's `EXT_STMT` and `ZEND_TICKS` land in the op
    /// array the class is declared in, before the declaration's opline,
    /// since what the member itself compiles to stands elsewhere
    /// (`zend_compile_class_decl` compiles the body before it emits the
    /// declaration). A `ZEND_TICKS` right after another is not emitted, so
    /// where no `EXT_STMT` stands between them a member's is the one
    /// before it.
    pub(super) fn member_statements(&mut self, lines: &[u32]) {
        let line = self.line;
        for member in lines {
            self.line = *member;
            self.ext_stmt();
            self.tick();
        }
        self.line = line;
    }

    /// `zend_emit_tick`: `ZEND_TICKS` with the count in
    /// `extended_value`, where a `declare(ticks)` scope stands and the
    /// op array's last opline is not already one, which is what keeps
    /// `declare(ticks=1) { … }` from ticking twice at its end.
    ///
    /// ```php
    /// declare(ticks=1);
    /// register_tick_function(function () { echo "."; });
    /// echo "a";
    /// if (true) { echo "b"; echo "c"; }
    /// function f() { echo "f"; return 1; }
    /// f();
    /// $g = fn() => print "g";
    /// $g();
    /// echo "\n";
    /// ```
    /// ```expect
    /// .a.b.c.f...g.
    /// .
    /// ```
    ///
    /// ```php
    /// register_tick_function(function () { echo "."; });
    /// echo "a";
    /// declare(ticks=1) { echo "b"; echo "c"; }
    /// echo "d";
    /// declare(ticks=2);
    /// echo "e"; echo "f"; echo "g"; echo "\n";
    /// ```
    /// ```expect
    /// ab.c.de.fg.
    /// ```
    pub(super) fn tick(&mut self) {
        let Some(ticks) = self.ticks else {
            return;
        };
        if self
            .oplines
            .last()
            .is_some_and(|last| last.opcode == Opcode::Ticks)
        {
            return;
        }
        // `opline->extended_value = FC(declarables).ticks`, a
        // `uint32_t` over the `zend_long`.
        let every = u32::try_from(ticks.get() & i64::from(u32::MAX)).unwrap_or(u32::MAX);
        self.push(Opline {
            opcode: Opcode::Ticks,
            extended_value: every,
            ..Opline::default()
        });
    }

    /// `zend_compile_for`: the initialisers, a `JMP` forward to the
    /// condition, the body, the step expressions, then the condition
    /// as a `JMPNZ` back to the body's first opline.
    ///
    /// **The jump back stands at the line the body left.**
    /// `zend_compile_for` (`Z/Zend/zend_compile.c:6060-6091`) never
    /// writes `CG(zend_lineno)` itself: the header's oplines carry the
    /// statement's line because `zend_compile_expr` sets it from each
    /// expression it compiles, and where a list is empty nothing is
    /// compiled and nothing is set. So `for (;;)`, whose condition list
    /// is empty, emits its `JMP` back at whatever `zend_compile_stmt`
    /// left after the body, which is the body's own line where the body
    /// emitted no opline at all. The execution timer names that line:
    ///
    /// ```php
    /// set_time_limit(1); for (;;)
    /// { /*
    ///
    /// */
    /// }
    /// ```
    /// ```expect exit=255 wall=2s
    /// Fatal error: Maximum execution time of 1 second exceeded in %s on line 3
    /// ```
    ///
    /// A `finally` clause leaves its own block's line the same way, the
    /// `FAST_RET` over an empty body emitted there:
    ///
    /// ```php
    /// set_time_limit(1); for (;;) try {} finally
    /// { /*
    ///
    /// */
    /// }
    /// ```
    /// ```expect exit=255 wall=2s
    /// Fatal error: Maximum execution time of 1 second exceeded in %s on line 3
    /// ```
    ///
    /// The third shape #601 measured, `for (;;) if ($x) ; else\n{ … }`,
    /// is not a witness: its loop holds two jumps, the `JMPZ` of the
    /// condition on line 2 and the jump back on line 3, and php tests
    /// the interrupt after either of them, so the line it names is
    /// whichever the interrupt fell on — line 3 on ten runs of the
    /// pinned oracle and line 2 on the other two of twelve — where this
    /// engine tests at the back-edge alone and names line 2 on all
    /// twelve. Design §5.8 and ledger 28 leave that line unheld, and
    /// what the tree gives the `else` arm is held in `php_syntax`'s
    /// `tests/line.rs` instead.
    fn r#for(&mut self, id: NodeId<tree::ForStmt>) -> Result<(), CompileError> {
        let node = &self.parsed.tree[id];
        let tree = &self.parsed.tree;
        let init = node
            .init
            .map_or(&[][..], |init| tree.list(tree[init].exprs));
        let cond = node
            .cond
            .map_or(&[][..], |cond| tree.list(tree[cond].exprs));
        let step = node
            .step
            .map_or(&[][..], |step| tree.list(tree[step].exprs));
        let init = self.expr_list(init)?;
        self.do_free(init);
        let jump = self.push(Opline {
            opcode: Opcode::Jmp,
            ..Opline::default()
        });
        self.begin_loop(None, None, false);
        let start = self.next();
        self.compound(node.body, node.stmt_list)?;
        let continue_at = self.next();
        let step = self.expr_list(step)?;
        self.do_free(step);
        self.update_jump_target_to_next(jump);
        let cond = self.expr_list(cond)?;
        self.ext_stmt();
        self.emit_cond_jump(cond, start);
        self.end_loop(continue_at);
        Ok(())
    }

    /// `zend_compile_if` (`Z/Zend/zend_compile.c:6186`): one pass over
    /// the arms, each condition compiled and left to a `JMPZ` forward,
    /// the arm's statements after it, and a `JMP` to the end of the
    /// whole statement where another arm follows; the `JMPZ` is then
    /// aimed at the opline about to be emitted, which is the next arm's
    /// first. The `else` arm has no condition, so it emits its
    /// statements and nothing else, and every `JMP` collected is aimed
    /// past it at the end.
    ///
    /// `CG(zend_lineno)` is the arm's own line before a condition past
    /// the first, and before the `JMP`, which php sets so a line of the
    /// arm's body is not reported as covered by the jump that leaves it.
    /// Nothing is folded: `zend_compile_if` compiles the condition of
    /// `if (1)` and `if (0)` like any other, and the `JMPZ` over a
    /// constant stands in the dump before the optimizer runs.
    fn r#if(&mut self, id: NodeId<tree::IfStmt>) -> Result<(), CompileError> {
        let node = &self.parsed.tree[id];
        let tree = &self.parsed.tree;
        // The arms in written order, one `ZEND_AST_IF_ELEM` each, in
        // whichever syntax the tree parsed.
        let mut arms = self
            .arena
            .vec(1 + node.elseif_clauses.len() + node.alt_elseif_clauses.len() + 1);
        let cond = self.required(node.cond, "an `if` with no condition")?;
        arms.push(IfElem {
            cond: Some(cond),
            body: node.then,
            stmt_list: node.stmt_list,
            line: node.line(),
        });
        if node.stmt_list.is_some() {
            for clause in self.list(node.alt_elseif_clauses) {
                let clause = &tree[*clause];
                let cond = self.required(clause.cond, "an `elseif` with no condition")?;
                arms.push(IfElem {
                    cond: Some(cond),
                    body: None,
                    stmt_list: clause.stmt_list,
                    line: clause.line(),
                });
            }
            if let Some(clause) = node.alt_else_clause {
                let clause = &tree[clause];
                arms.push(IfElem {
                    cond: None,
                    body: None,
                    stmt_list: clause.stmt_list,
                    line: clause.line(),
                });
            }
        } else {
            for clause in self.list(node.elseif_clauses) {
                let clause = &tree[*clause];
                let cond = self.required(clause.cond, "an `elseif` with no condition")?;
                arms.push(IfElem {
                    cond: Some(cond),
                    body: clause.body,
                    stmt_list: None,
                    line: clause.line(),
                });
            }
            if let Some(clause) = node.else_clause {
                let clause = &tree[clause];
                arms.push(IfElem {
                    cond: None,
                    body: clause.body,
                    stmt_list: None,
                    line: clause.line(),
                });
            }
        }
        let last = arms.len().saturating_sub(1);
        let mut jumps = Vec::new();
        for (index, arm) in arms.iter().enumerate() {
            let Some(cond) = arm.cond else {
                // php asserts the arm with no condition is the last.
                self.compound(arm.body, arm.stmt_list)?;
                continue;
            };
            if index > 0 {
                self.line = arm.line;
                self.ext_stmt();
            }
            let cond = self.expr(cond)?;
            let opnum_jmpz = self.cond_jump_forward(Opcode::Jmpz, cond);
            self.compound(arm.body, arm.stmt_list)?;
            if index != last {
                self.line = arm.line;
                jumps.push(self.jump_forward());
            }
            self.update_jump_target_to_next(opnum_jmpz);
        }
        for jump in jumps {
            self.update_jump_target_to_next(jump);
        }
        Ok(())
    }

    /// `zend_compile_while`: a `JMP` forward to the condition, the body,
    /// then the condition as a `JMPNZ` back to the body's first opline.
    fn r#while(
        &mut self,
        cond: tree::Expr,
        body: Option<Stmt>,
        stmt_list: Option<NodeId<StmtList>>,
    ) -> Result<(), CompileError> {
        let line = self.line;
        let jump = self.push(Opline {
            opcode: Opcode::Jmp,
            ..Opline::default()
        });
        self.begin_loop(None, None, false);
        let start = self.next();
        self.compound(body, stmt_list)?;
        let continue_at = self.next();
        self.line = line;
        self.update_jump_target_to_next(jump);
        let cond = self.expr(cond)?;
        self.emit_cond_jump(cond, start);
        self.end_loop(continue_at);
        Ok(())
    }

    /// `zend_compile_do_while` (`Z/Zend/zend_compile.c:6010`):
    /// `zend_compile_while` without the `JMP` that carries the first
    /// pass over the condition, so the body is the loop's first opline
    /// and runs once before the condition is asked. The condition and
    /// the `JMPNZ` back to the body's first opline are emitted at the
    /// condition's own line, which is where php's node carries it; the
    /// back-edge is numbered as every loop header's is (design §5.7).
    ///
    /// `break` and `continue` are the same loop stack's as a `while`'s,
    /// so a `continue` lands on the condition and not on the body.
    ///
    /// ```php
    /// $n = 0; do { $n = $n + 1; } while (false); var_dump($n);
    /// $m = 0; do { $m = $m + 1; if ($m > 2) { break; } continue; } while ($m < 9); var_dump($m);
    /// ```
    /// ```expect
    /// int(1)
    /// int(3)
    /// ```
    /// The condition's own line and not the `do`'s, which the
    /// condition's node carries: what php reports for a throw out of
    /// the condition of a `do` whose `while` is written four lines
    /// below it.
    ///
    /// ```php
    /// $n = 0;
    /// do {
    ///     $n = $n + 1;
    /// } while (
    ///
    ///     $n % 0
    /// );
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught DivisionByZeroError: Modulo by zero in %s:7
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 7
    /// ```
    fn do_while(&mut self, body: Option<Stmt>, cond: tree::Expr) -> Result<(), CompileError> {
        self.begin_loop(None, None, false);
        let start = self.next();
        self.compound(body, None)?;
        let continue_at = self.next();
        self.line = self.line_of(cond);
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
    fn foreach(&mut self, id: NodeId<tree::ForeachStmt>) -> Result<(), CompileError> {
        let node = &self.parsed.tree[id];
        let tree = &self.parsed.tree;
        let line = self.line;
        let expr = self.required(node.iterable, "a `foreach` with no condition")?;
        let key = node.key.map(|key| &tree[key]);
        let Some(target) = node.target.map(|target| &tree[target]) else {
            return Err(self.unread("a `foreach` whose target"));
        };
        let Some(value) = target.target else {
            return Err(self.unread("a `foreach` whose target"));
        };
        if key.is_some_and(|key| key.byref.is_some()) {
            return Err(self.compile_error("Key element cannot be a reference"));
        }
        if key.is_some_and(|key| self.list_of(key.key).is_some()) {
            return Err(self.compile_error("Cannot use list as key element"));
        }
        // `zend_propagate_list_refs`: a list with an element taken by
        // reference takes the loop by reference.
        let list = self.list_of(value);
        let by_ref =
            target.byref.is_some() || list.as_ref().is_some_and(|list| self.list_has_refs(list));
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
        self.foreach_value(reset, by_ref, value, list.as_ref())?;
        if let Some(key) = key {
            self.foreach_key(opnum_fetch, key.key)?;
        }
        self.compound(node.body, node.stmt_list)?;
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

    /// The value half of `zend_compile_foreach`: `$this` is php's
    /// compile error; the fetch over the reset `VAR` writes a plain
    /// variable's CV, and anything else into a `VAR` of its own that the
    /// target is then assigned from, a list by `zend_compile_list_assign`
    /// and any other place by the `ASSIGN` or `ASSIGN_REF`
    /// `zend_emit_assign_znode` builds.
    fn foreach_value(
        &mut self,
        reset: u32,
        by_ref: bool,
        value: tree::Expr,
        list: Option<&target::List>,
    ) -> Result<(), CompileError> {
        if self.is_this(value) {
            return Err(self.compile_error("Cannot re-assign $this"));
        }
        let fetch = self.push(Opline {
            opcode: if by_ref {
                Opcode::FeFetchRw
            } else {
                Opcode::FeFetchR
            },
            op1_type: OperandKind::Var,
            op1: reset,
            ..Opline::default()
        });
        // The value's CV where it is a plain variable; otherwise a `VAR`
        // the fetch writes and the target is assigned from, as
        // `zend_emit_assign_znode` assigns it.
        let (op2_type, op2) = match (self.peel(value), list) {
            (tree::Expr::SimpleVarExpr(var), None) => match self.simple_var_of(var, self.line)? {
                SimpleVar::Cv(name) => {
                    self.cv_fetched(name, Fetch::Read);
                    (OperandKind::Cv, self.lookup_cv(name))
                }
                SimpleVar::This => return Err(self.compile_error("Cannot re-assign $this")),
                // `zend_try_compile_cv` refuses an auto-global, so the
                // fetch writes a `VAR` the variable is assigned from; the
                // assignment is where `$GLOBALS` meets its compile error.
                SimpleVar::Auto(_) | SimpleVar::Globals => (OperandKind::Var, self.temporary()),
            },
            _ => (OperandKind::Var, self.temporary()),
        };
        if let Some(opline) = self.oplines.get_mut(fetch as usize) {
            opline.op2_type = op2_type;
            opline.op2 = op2;
        }
        if op2_type == OperandKind::Var {
            let fetched = Where::Returned(op2);
            if let Some(list) = list {
                self.list_assign(list, fetched, false)?;
            } else {
                self.line = self.line_of(value);
                let assigned = if by_ref {
                    self.assign_ref(value, Source::Node(fetched))?
                } else {
                    self.assign_to(value, Source::Node(fetched))?
                };
                self.do_free(assigned);
            }
        }
        Ok(())
    }

    /// The subject of a `foreach`: compiled as an expression, and where
    /// the value is taken by reference, as `zend_compile_var` under
    /// `BP_VAR_W` compiles a variable, which for a CV is the CV itself
    /// and for a dimension, a property, a `static` property or a variable
    /// fetched by name the `_W` fetch of the whole chain into a `VAR`
    /// ([`Self::var_for_reference`]), so `FE_RESET_RW` binds the elements
    /// where they stand; a call's
    /// `VAR` is separated after it (`zend_separate_if_call_and_write`),
    /// and a call whose value is a temporary, one of the special
    /// table's, is php's compile error.
    ///
    /// ```php
    /// class C { public $p = [1, 2]; public static $s = ['a' => 1]; }
    /// $o = new C;
    /// foreach ($o->p as &$v) { $v *= 3; }
    /// unset($v);
    /// foreach (C::$s as &$w) { $w = 'b'; }
    /// unset($w);
    /// var_dump($o->p, C::$s);
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   int(3)
    ///   [1]=>
    ///   int(6)
    /// }
    /// array(1) {
    ///   ["a"]=>
    ///   string(1) "b"
    /// }
    /// ```
    ///
    /// `$this` is `FETCH_THIS` into a `VAR` there, and the loop writes
    /// the object's properties through it:
    ///
    /// ```php
    /// class P {
    ///     public $a = 1;
    ///     protected $b = 2;
    ///     function double() { foreach ($this as $k => &$v) { $v *= 2; } unset($v); foreach ($this as $k => $v) { echo "$k=$v "; } echo "\n"; }
    /// }
    /// (new P)->double();
    /// ```
    /// ```expect
    /// a=2 b=4
    /// ```
    fn foreach_subject(&mut self, expr: tree::Expr, by_ref: bool) -> Result<Where, CompileError> {
        let node = self.peel(expr);
        // `zend_can_write_to_variable`: a chain a `?->` stands in is
        // compiled as the value it reads.
        if by_ref
            && (matches!(
                node,
                tree::Expr::SubscriptExpr(_) | tree::Expr::MemberAccessExpr(_)
            ) || self.is_static_prop(node)
                || self.is_by_name(node))
            && !self.is_short_circuited(node)
        {
            let (kind, at) = self.var_for_reference(node)?;
            return Ok(match kind {
                OperandKind::Cv => Where::Cv(at),
                _ => Where::Returned(at),
            });
        }
        // `$GLOBALS` under `BP_VAR_W`: `FETCH_GLOBALS` into a `VAR`, and
        // `$this` `FETCH_THIS` into one, `zend_compile_simple_var`'s
        // `is_this_fetch` arm outside `BP_VAR_R` and `BP_VAR_IS`.
        if by_ref && self.is_globals(node) {
            return Ok(self.fetch_globals(OperandKind::Var));
        }
        if by_ref && self.is_this(node) {
            return Ok(self.fetch_this(OperandKind::Var));
        }
        let subject = self.expr(expr)?;
        if by_ref && self.is_call(expr) {
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
    fn foreach_key(&mut self, opnum_fetch: u32, key: tree::Expr) -> Result<(), CompileError> {
        let key_tmp = self.temporary();
        if let Some(fetch) = self.oplines.get_mut(opnum_fetch as usize) {
            fetch.out_type = OperandKind::Tmp;
            fetch.out = key_tmp;
        }
        // `zend_compile_expr` over the `ASSIGN` php builds: the key's line.
        self.line = self.line_of(key);
        let assigned = self.assign_to(key, Source::Node(Where::Temporary(key_tmp)))?;
        self.do_free(assigned);
        Ok(())
    }

    /// `zend_compile_return` (`Z/Zend/zend_compile.c:5694`): the value,
    /// a dimension fetched for writing where it is returned by
    /// reference (`zend_compile_var` under `BP_VAR_W`), or the constant
    /// `null` where none is written; the value copied
    /// into a temporary where a `finally` stands between here and the
    /// `RETURN` ([`Self::copied_past_finally`]); the return type check
    /// where the op array declares one ([`Self::return_type_check`]);
    /// every open loop's variable freed and every open `finally` called,
    /// innermost first ([`Self::handle_loops_and_finally`]), and the
    /// type check repeated after them for a by-reference return, whose
    /// cell the block may have written; then `RETURN` over
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
    ///
    /// A dimension returned by reference is fetched for writing
    /// ([`Self::var_for_reference`]), so the block writing beside it
    /// leaves the cell the caller is bound to where it was (php's bug
    /// #72216).
    ///
    /// ```php
    /// function &test() { $a = ["ok"]; try { return $a[0]; } finally { $a[""] = 42; } }
    /// var_dump(test());
    /// ```
    /// ```expect
    /// string(2) "ok"
    /// ```
    ///
    /// A property, a class's property, `$this`, a property under a
    /// property and a variable the program names are the variable
    /// fetched by reference too ([`Self::var_by_ref`]); what is no
    /// variable is `ZEND_RETURNS_VALUE` and a call `ZEND_RETURNS_FUNCTION`,
    /// each php's notice where no reference comes back:
    ///
    /// ```php
    /// class C {
    ///     public $p = [1]; public static $s = 2; public $o;
    ///     function &prop() { return $this->p; }
    ///     function &elem() { return $this->p[0]; }
    ///     function &stat() { return self::$s; }
    ///     function &me() { return $this; }
    ///     function &deep() { return $this->o->q; }
    ///     function &byname() { $n = 'x'; $x = 5; return $$n; }
    ///     function &val() { return 1 + 1; }
    ///     function &call() { return $this->val(); }
    /// }
    /// $c = new C; $c->o = new stdClass;
    /// $r = &$c->prop(); $r[] = 2; var_dump($c->p);
    /// $e = &$c->elem(); $e = 9; var_dump($c->p);
    /// $s = &$c->stat(); $s = 3; var_dump(C::$s);
    /// $d = &$c->deep(); $d = 'q'; var_dump($c->o);
    /// var_dump($c->me() === $c, $c->byname(), $c->val(), $c->call());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    /// array(2) {
    ///   [0]=>
    ///   &int(9)
    ///   [1]=>
    ///   int(2)
    /// }
    /// int(3)
    /// object(stdClass)#2 (1) {
    ///   ["q"]=>
    ///   &string(1) "q"
    /// }
    ///
    /// Notice: Only variable references should be returned by reference in %s on line 10
    ///
    /// Notice: Only variable references should be returned by reference in %s on line 10
    /// bool(true)
    /// int(5)
    /// int(2)
    /// int(2)
    /// ```
    fn r#return(&mut self, value: Option<tree::Expr>) -> Result<(), CompileError> {
        // For a generator the `&` is its yields', not its return's.
        let generator = self.fn_flags.generator;
        let by_ref = self.returns_by_ref && !generator;
        let written = value.map(|value| self.peel(value));
        // `zend_assert_not_short_circuited` over a variable returned by
        // reference.
        // `zend_is_variable`, the `ZEND_AST_VAR` of a name the program
        // computes among them.
        let variable = written.is_some_and(|expr| {
            self.is_variable(expr)
                || matches!(
                    expr,
                    tree::Expr::VarVarExpr(_) | tree::Expr::DynamicVarExpr(_)
                )
        });
        if by_ref && variable && written.is_some_and(|expr| self.is_short_circuited(expr)) {
            return Err(self.compile_error("Cannot take reference of a nullsafe chain"));
        }
        // `ZEND_RETURNS_FUNCTION` over a call and `ZEND_RETURNS_VALUE`
        // over what is no variable, the two the handler raises php's
        // notice for where no reference comes back.
        let extended_value = match written {
            Some(_) if !by_ref => 0,
            Some(expr) if self.is_call(expr) => RETURNS_FUNCTION,
            Some(_) if !variable => RETURNS_VALUE,
            _ => 0,
        };
        // `zend_compile_var(BP_VAR_W, 1)` over a variable returned by
        // reference ([`Self::var_by_ref`]): the cell the caller is bound
        // to is the variable's own, an element's or a property's slot.
        let compiled = match written {
            Some(expr) if by_ref && variable => match self.var_by_ref(expr)? {
                (OperandKind::Cv, at) => Where::Cv(at),
                (_, at) => Where::Returned(at),
            },
            Some(expr) => self.expr(expr)?,
            None => Where::Constant(Zval::null()),
        };
        let compiled = self.copied_past_finally(compiled, by_ref);
        // A generator's return type is held to `Generator` at compile
        // time instead ([`Self::mark_function_as_generator`]).
        let compiled = if generator {
            compiled
        } else {
            self.return_type_check(compiled, value.is_some(), false)?
        };
        let before_finally = self.next();
        self.handle_loops_and_finally(Some(compiled));
        // The `finally` may have written into the cell a by-reference
        // return hands back, so the return type is checked again where
        // the walk emitted anything.
        let compiled = if by_ref && before_finally != self.next() {
            self.return_type_check(compiled, value.is_some(), false)?
        } else {
            compiled
        };
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
        self.loop_var_pushed();
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

    /// `zend_stack_push(&CG(loop_var_stack), ..)`: one entry more, and
    /// the peak the unit reports raised to it.
    fn loop_var_pushed(&mut self) {
        self.loop_vars = self.loop_vars.saturating_add(1);
        self.loop_var_peak = self.loop_var_peak.max(self.loop_vars);
    }

    /// `zend_end_loop`: `continue` lands at `cont`, `break` at the
    /// opline about to be emitted, and the loop outside is the innermost
    /// again.
    fn end_loop(&mut self, cont: u32) {
        self.loop_vars = self.loop_vars.saturating_sub(1);
        let brk = self.next();
        if let Some(current) = self.current_loop
            && let Some(open) = self.loops.get_mut(current)
        {
            open.brk = brk;
            open.cont = cont;
            self.current_loop = open.parent;
        }
    }

    /// `zend_compile_return`'s copy past the block: a returned CV is
    /// moved into a `QM_ASSIGN` where a `finally` stands between here
    /// and the `RETURN` (`zend_has_finally`), so code in the `finally`
    /// cannot write the value out from under the return. A by-reference
    /// return makes a `MAKE_REF` of the CV, or of the `VAR` a call left,
    /// and returns the cell, so the `finally` writes into what the
    /// caller will be bound to. A constant or a temporary is already
    /// nobody else's.
    ///
    /// ```php
    /// function &f(array &$a): array { try { return $a; } finally { $a[] = 2; } }
    /// $x = [1];
    /// $y = &f($x);
    /// $y[] = 3;
    /// var_dump($x);
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
    /// ```
    fn copied_past_finally(&mut self, value: Where, by_ref: bool) -> Where {
        // `zend_has_finally`: a `FAST_CALL` stands on the stack, which
        // is a `finally` between here and the `RETURN`; inside a
        // `finally` block its own marker is a `DISCARD_EXCEPTION` and
        // the copy is not made.
        if !self
            .unwind
            .iter()
            .any(|marker| matches!(marker, Unwind::FastCall { .. }))
        {
            return value;
        }
        let (op1_type, op1) = match (value, by_ref) {
            (Where::Cv(cv), _) => (OperandKind::Cv, cv),
            (Where::Returned(var), true) => (OperandKind::Var, var),
            _ => return value,
        };
        let out = self.temporary();
        if by_ref {
            self.push(Opline {
                opcode: Opcode::MakeRef,
                op1_type,
                op1,
                out_type: OperandKind::Var,
                out,
                ..Opline::default()
            });
            return Where::Returned(out);
        }
        self.push(Opline {
            opcode: Opcode::QmAssign,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        Where::Temporary(out)
    }

    /// `zend_handle_loops_and_finally`: the walk with no depth an entry
    /// can stop at, what a `return` emits before the `RETURN`, so every
    /// open loop's variable is freed and every open `finally` is called.
    fn handle_loops_and_finally(&mut self, value: Option<Where>) {
        let count = self
            .loops_open()
            .saturating_add(u32::try_from(self.unwind.len()).unwrap_or(u32::MAX));
        self.handle_loops_and_finally_ex(count.saturating_add(1), value);
    }

    /// `zend_handle_loops_and_finally_ex` (`Z/Zend/zend_compile.c:5607`):
    /// `CG(loop_var_stack)` walked innermost first, to `depth` loops out.
    /// A `finally` marker on the way is its `FAST_CALL`, or the
    /// `DISCARD_EXCEPTION` of a block being left from inside it; a loop
    /// is one level of the depth, its variable freed on the way past
    /// where it has one, and the walk stops at the loop the depth names,
    /// whose own end frees it. Whether the depth was met is the answer,
    /// which `break n` past the loops that stand reads as its refusal.
    /// The two stacks are one in php; here the loops hang off
    /// [`Emit::current_loop`] and the `finally` markers off
    /// [`Emit::unwind`], each marker recording how many loops stood
    /// under it, which puts them back in one order.
    ///
    /// A `FAST_CALL` emitted here carries the value being returned in
    /// `op2` where a `return` gives one, which is what a
    /// `DISCARD_EXCEPTION` or an unwind through the block gives up when
    /// the return never happens; a `break` or a `continue` gives none.
    fn handle_loops_and_finally_ex(&mut self, mut depth: u32, value: Option<Where>) -> bool {
        let (value_type, value_at) = match value {
            Some(Where::Temporary(at)) => (OperandKind::Tmp, at),
            Some(Where::Returned(at)) => (OperandKind::Var, at),
            Some(Where::Constant(_) | Where::Cv(_)) | None => (OperandKind::Unused, 0),
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
                    if depth <= 1 {
                        return true;
                    }
                    let Some((var, parent)) =
                        self.loops.get(index).map(|one| (one.var, one.parent))
                    else {
                        break;
                    };
                    if let Some(var) = var {
                        self.free_on_return(var);
                    }
                    depth -= 1;
                    loops = parent;
                    open = open.saturating_sub(1);
                }
            }
        }
        depth == 0
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

    /// `zend_compile_break_continue` (`Z/Zend/zend_compile.c:5812`): the
    /// depth, `1` unwritten and otherwise the positive integer php's
    /// compiler holds it to; the loop or `switch` it stands in, without
    /// which php refuses; the `FAST_CALL` of every `finally` and the
    /// `FREE` of every loop variable the jump leaves behind
    /// ([`Self::handle_loops_and_finally_ex`]), which is also where a
    /// depth past the loops that stand is refused; the compile-time
    /// warning for a `continue` that targets a `switch`; then the
    /// `ZEND_BRK` or `ZEND_CONT`, here the `JMP` `pass_two` makes of it
    /// once its target is known ([`Self::resolve_jumps`]).
    ///
    /// ```php
    /// foreach ([1, 2] as $v) { try { break; } finally { echo "f"; } }
    /// foreach ([1, 2] as $v) { try { continue; } finally { echo "c"; } }
    /// foreach ([1, 2] as $v) { foreach ([3] as $w) { try { try { break 2; } finally { echo "i"; } } finally { echo "o"; } } }
    /// foreach ([1, 2] as $v) { try { switch ($v) { case 1: try { continue 2; } finally { echo "s"; } } } finally { echo "t"; } echo $v; }
    /// ```
    /// ```expect
    /// fcciostt2
    /// ```
    fn break_continue(
        &mut self,
        depth: Option<tree::Expr>,
        is_break: bool,
    ) -> Result<(), CompileError> {
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
        // `zend_handle_loops_and_finally_ex`: the `FAST_CALL` of every
        // `finally` the jump leaves and the `FREE` of every loop's
        // variable inside the target, innermost first, and the refusal
        // where the depth is more loops than stand.
        if !self.handle_loops_and_finally_ex(depth, None) {
            let plural = if depth == 1 { "" } else { "s" };
            return Err(self.compile_error(format!("Cannot '{word}' {depth} level{plural}")));
        }
        let mut at = Some(current);
        for _ in 1..depth {
            at = at
                .and_then(|index| self.loops.get(index))
                .and_then(|open| open.parent);
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
            // `E_WARNING`, which a user error handler is handed, where
            // the compiler's other warnings are `E_COMPILE_WARNING`.
            self.raise(Level::Warning, &message);
        }
        let opline = self.push(Opline {
            opcode: Opcode::Jmp,
            ..Opline::default()
        });
        self.jumps.push(Pending::Break {
            at: opline,
            r#loop: current,
            depth,
            is_break,
        });
        Ok(())
    }

    /// `zend_compile_label` (`Z/Zend/zend_compile.c:5966`): the label
    /// into `CG(context).labels` with the next opline's number and the
    /// loop it stands in; a name the op array already has is php's
    /// compile error.
    fn label(&mut self, name: &[u8]) -> Result<(), CompileError> {
        if self.labels.iter().any(|(held, _)| held == name) {
            return Err(self.compile_error(format!(
                "Label '{}' already defined",
                String::from_utf8_lossy(name)
            )));
        }
        let label = Label {
            opline: self.next(),
            r#loop: self.current_loop,
        };
        self.labels.push((name.to_vec(), label));
        Ok(())
    }

    /// `zend_compile_goto` (`Z/Zend/zend_compile.c:5950`): the label's
    /// name as a constant of its own (`zend_add_literal`), every open
    /// loop's variable freed and every open `finally` called as a
    /// `return` frees and calls them, since where the label stands is
    /// not known until `pass_two`, then the `ZEND_GOTO`, here the `JMP`
    /// [`Self::resolve_goto`] makes of it, carrying how many of those
    /// oplines stand before it and the loop it was written in.
    ///
    /// ```php
    /// $i = 0;
    /// again:
    /// $i++;
    /// if ($i < 3) { goto again; }
    /// foreach ([1, 2] as $v) { try { if ($v == 2) { goto out; } echo $v; } finally { echo "f"; } }
    /// out:
    /// echo $i, "\n";
    /// ```
    /// ```expect
    /// 1ff3
    /// ```
    fn goto(&mut self, name: &[u8]) -> Result<(), CompileError> {
        let label = self.string(name)?;
        let label = self.literal(label);
        let start = self.next();
        self.handle_loops_and_finally(None);
        let at = self.push(Opline {
            opcode: Opcode::Jmp,
            ..Opline::default()
        });
        self.jumps.push(Pending::Goto {
            at,
            label,
            emitted: at.saturating_sub(start),
            r#loop: self.current_loop,
        });
        Ok(())
    }

    /// `pass_two` over the jumps it resolves, in opline order: the
    /// `ZEND_BRK` and `ZEND_CONT` arm, `zend_get_brk_cont_target`, the
    /// loop `depth` levels out from the one the jump was written in and
    /// its `brk` or `cont` as the `JMP`'s target; and the `ZEND_GOTO`
    /// arm, [`Self::resolve_goto`]; each once
    /// [`Self::check_finally_breakout`] has allowed the jump.
    fn resolve_jumps(&mut self) -> Result<(), CompileError> {
        for jump in std::mem::take(&mut self.jumps) {
            match jump {
                Pending::Break {
                    at,
                    r#loop,
                    depth,
                    is_break,
                } => {
                    let mut index = r#loop;
                    let mut levels = depth;
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
                    let header = if is_break { None } else { target.header };
                    let target = if is_break { target.brk } else { target.cont };
                    self.check_finally_breakout(at, target)?;
                    if let Some(opline) = self.oplines.get_mut(at as usize) {
                        opline.op1 = target;
                        // A `continue` that jumps back to a `foreach`'s
                        // header is a back-edge and carries the header's
                        // number, the one the loop's own `JMP` carries
                        // (design §5.7).
                        if let Some(header) = header {
                            opline.out = header;
                        }
                    }
                }
                Pending::Goto {
                    at,
                    label,
                    emitted,
                    r#loop,
                } => self.resolve_goto(at, label, emitted, r#loop)?,
            }
        }
        Ok(())
    }

    /// `zend_resolve_goto_label` (`Z/Zend/zend_compile.c:5888`): the
    /// label the constant names, or php's compile error for one the op
    /// array has not; the loops between the `goto`'s and the label's,
    /// walked outwards, which is php's compile error where the label
    /// stands in a loop the `goto` does not; then the oplines the
    /// `goto` emitted for what it turns out not to leave, made `NOP`
    /// from the `JMP` backwards: one `FREE` for each loop it stays in,
    /// and one `FAST_CALL` for each `finally` whose `try` holds both
    /// ends. The `JMP` lands on the label; where that is a backward
    /// jump it is a back-edge and carries a safepoint number of its own
    /// (design §5.7). The label's slot is nulled once found, as php
    /// nulls it (`Z/Zend/zend_compile.c:5907`), which phpdbg's
    /// `info literal` shows by leaving it out.
    ///
    /// ```php
    /// goto missing;
    /// ```
    /// ```expect exit=255
    /// Fatal error: 'goto' to undefined label 'missing' in %s on line 2
    /// ```
    ///
    /// ```php
    /// goto inside;
    /// while (true) { inside: break; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: 'goto' into loop or switch statement is disallowed in %s on line 2
    /// ```
    ///
    /// ```php
    /// try { echo 1; } finally { goto out; }
    /// out: echo 2;
    /// ```
    /// ```expect exit=255
    /// Fatal error: jump out of a finally block is disallowed in %s on line 2
    /// ```
    ///
    /// ```php
    /// goto inside;
    /// try { echo 1; } finally { inside: echo 2; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: jump into a finally block is disallowed in %s on line 2
    /// ```
    fn resolve_goto(
        &mut self,
        at: u32,
        label: u32,
        emitted: u32,
        r#loop: Option<usize>,
    ) -> Result<(), CompileError> {
        let line = self
            .oplines
            .get(at as usize)
            .map_or(self.line, |op| op.lineno);
        let refused = |message: String| {
            Refused {
                level: Level::CompileError,
                message: message.into(),
                line,
                raised: Vec::new(),
                raise: crate::Raise::Fatal,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            }
            .into()
        };
        let name = self
            .constants
            .get(label as usize)
            .and_then(|held| held.as_str())
            .map(|s| s.bytes().to_vec())
            .unwrap_or_default();
        let Some(dest) = self
            .labels
            .iter()
            .find(|(held, _)| *held == name)
            .map(|(_, label)| *label)
        else {
            return Err(refused(format!(
                "'goto' to undefined label '{}'",
                String::from_utf8_lossy(&name)
            )));
        };
        if let Some(held) = self.constants.get_mut(label as usize) {
            *held = Zval::null();
        }
        let mut remove = emitted;
        let mut current = r#loop;
        while current != dest.r#loop {
            let Some(index) = current else {
                return Err(refused(String::from(
                    "'goto' into loop or switch statement is disallowed",
                )));
            };
            let Some(open) = self.loops.get(index) else {
                break;
            };
            if open.var.is_some() {
                remove = remove.saturating_sub(1);
            }
            current = open.parent;
        }
        for element in &self.try_catch {
            if element.try_op.0 > at {
                break;
            }
            if element.finally_op.0 != 0
                && at < element.finally_op.0.saturating_sub(1)
                && (dest.opline > element.finally_end.0 || dest.opline < element.try_op.0)
            {
                remove = remove.saturating_sub(1);
            }
        }
        let number = (dest.opline <= at).then(|| self.safepoint(dest.opline));
        if let Some(opline) = self.oplines.get_mut(at as usize) {
            opline.op1 = dest.opline;
            opline.out = number.unwrap_or(0);
        }
        for back in 1..=remove {
            if let Some(opline) = self.oplines.get_mut((at - back) as usize) {
                *opline = Opline {
                    lineno: opline.lineno,
                    ..Opline::default()
                };
            }
        }
        self.check_finally_breakout(at, dest.opline)
    }

    /// `zend_check_finally_breakout` (`Z/Zend/zend_opcode.c:687`), what
    /// `pass_two` asks of every `BRK`, `CONT` and `GOTO` of an op array
    /// with a `finally` block: a jump from outside a `finally` block to
    /// inside one, or from inside one to outside it, is php's compile
    /// error at the jump's line, in php's words. The block a `FAST_RET`
    /// ends is `finally_op` to `finally_end` inclusive.
    ///
    /// ```php
    /// foreach ([1] as $v) { try { echo 1; } finally { break; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: jump out of a finally block is disallowed in %s on line 2
    /// ```
    ///
    /// A loop that opens and closes inside the block is left by nothing.
    ///
    /// ```php
    /// foreach ([1] as $v) { try { echo 1; } finally { while (true) { break; } echo 2; } }
    /// ```
    /// ```expect
    /// 12
    /// ```
    fn check_finally_breakout(&self, op_num: u32, dst_num: u32) -> Result<(), CompileError> {
        for element in &self.try_catch {
            let (finally_op, finally_end) = (element.finally_op.0, element.finally_end.0);
            if finally_op == 0 {
                continue;
            }
            let from_inside = (finally_op..=finally_end).contains(&op_num);
            let to_inside = (finally_op..=finally_end).contains(&dst_num);
            let refused = if !from_inside && to_inside {
                "jump into a finally block is disallowed"
            } else if from_inside && !to_inside {
                "jump out of a finally block is disallowed"
            } else {
                continue;
            };
            let line = self
                .oplines
                .get(op_num as usize)
                .map_or(self.line, |op| op.lineno);
            return Err(Refused {
                level: Level::CompileError,
                message: String::from(refused).into(),
                line,
                raised: Vec::new(),
                raise: crate::Raise::Fatal,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            }
            .into());
        }
        Ok(())
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
    fn switch(&mut self, id: NodeId<tree::SwitchStmt>) -> Result<(), CompileError> {
        let node = &self.parsed.tree[id];
        let expr = self.required(node.subject, "a `switch` with no condition")?;
        let cases = self.list(node.switch_arms);
        let subject = self.expr(expr)?;
        self.begin_loop(LoopVar::of(Opcode::Free, subject), None, true);
        let case_node = self.temporary();
        let mut table = match self.switch_jumptable_type(cases)? {
            Some(kind) if should_use_jumptable(cases.len(), kind) => Some(self.emit_table(
                if kind == ZvalType::Long {
                    Opcode::SwitchLong
                } else {
                    Opcode::SwitchString
                },
                subject,
                cases.len(),
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
            let case = &self.parsed.tree[*case];
            if let (Some(_), Some((jump, cond))) = (case.value, jump) {
                self.update_jump_target_to_next(jump);
                if let Where::Constant(value) = cond {
                    self.table_add(&mut table, CaseValue::Compared(value))?;
                }
            } else {
                has_default = true;
                self.update_jump_target_to_next(default_jump);
                self.table_default_to_next(table.as_ref());
            }
            self.compound(None, case.stmt_list)?;
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
    /// Each case's condition at its own line, which its node carries
    /// and the `case` keyword's line is not: a condition written below
    /// its keyword is reported where it stands.
    ///
    /// ```php
    /// $z = 0;
    /// switch (9) {
    ///     case
    ///         1 % $z:
    ///         echo "a";
    /// }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught DivisionByZeroError: Modulo by zero in %s:5
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 5
    /// ```
    ///
    /// An arm written `case 1;` or `default;` is php's
    /// `ZEND_ALT_CASE_SYNTAX`, deprecated at 8.5 before the arm
    /// compiles and at the arm's own line, which the `separator` token
    /// the tree keeps names and the arm's `case` keyword does not:
    ///
    /// ```php
    /// switch (1) {
    ///     case 1;
    ///         echo "a\n";
    ///     default ;
    ///         echo "b\n";
    /// }
    /// ```
    /// ```expect
    /// Deprecated: Case statements followed by a semicolon (;) are deprecated, use a colon (:) instead in %s on line 3
    ///
    /// Deprecated: Case statements followed by a semicolon (;) are deprecated, use a colon (:) instead in %s on line 5
    /// a
    /// b
    /// ```
    ///
    /// The deprecation stands before the clause is read, so a second
    /// `default` written with a `;` is deprecated twice and refused
    /// after:
    ///
    /// ```php
    /// switch (1) { default; default; }
    /// ```
    /// ```expect exit=255
    /// Deprecated: Case statements followed by a semicolon (;) are deprecated, use a colon (:) instead in %s on line 2
    ///
    /// Deprecated: Case statements followed by a semicolon (;) are deprecated, use a colon (:) instead in %s on line 2
    ///
    /// Fatal error: Switch statements may only contain one default clause in %s on line 2
    /// ```
    fn switch_chain(
        &mut self,
        subject: Where,
        case_node: u32,
        cases: &[NodeId<tree::SwitchArm>],
    ) -> Result<Vec<Option<(u32, Where)>>, CompileError> {
        let mut jumps = Vec::with_capacity(cases.len());
        let mut has_default = false;
        for case in cases {
            let case = &self.parsed.tree[*case];
            // `ZEND_ALT_CASE_SYNTAX` (`Z/Zend/zend_compile.c:6339`): the
            // `;` spelling is deprecated under `CG(zend_lineno) =
            // case_ast->lineno`, before the default clause is counted
            // and before the condition compiles, so a second `default`
            // is deprecated and then refused.
            if case
                .separator
                .is_some_and(|token| self.kind(token) == SyntaxKind::SEMICOLON)
            {
                self.line = case.line();
                self.raise(
                    Level::Deprecated,
                    "Case statements followed by a semicolon (;) are deprecated, use a colon (:) instead",
                );
            }
            let Some(cond) = case.value else {
                if has_default {
                    self.line = case.line();
                    return Err(
                        self.compile_error("Switch statements may only contain one default clause")
                    );
                }
                has_default = true;
                jumps.push(None);
                continue;
            };
            self.line = self.line_of(cond);
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
            jumps.push(Some((jump, cond)));
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
    /// it; nothing otherwise. Each case is read through
    /// `zend_eval_const_expr`, which leaves its rewrite for the case's
    /// compile ([`eval`]), up to the first that does not fold: a
    /// conditional over a constant is a case of the value it picks, and
    /// a call, which the evaluator folds none of, is no constant here
    /// though its compile may fold it.
    ///
    /// ```php
    /// foreach ([5, 3] as $x) {
    ///     switch ($x) { case 1: case 2: case 3: case 4: case true ? 5 : 6: echo "a$x,"; break; default: echo "d$x,"; }
    ///     switch ($x) { case 1: case 2: case 4: case 6: case strlen('abc'): echo "b$x,"; }
    /// }
    /// echo "\n";
    /// ```
    /// ```expect
    /// a5,a3,b3,
    /// ```
    fn switch_jumptable_type(
        &mut self,
        cases: &[NodeId<tree::SwitchArm>],
    ) -> Result<Option<ZvalType>, CompileError> {
        let mut common = None;
        for case in cases {
            let Some(cond) = self.parsed.tree[*case].value else {
                continue;
            };
            let Some(value) = self.eval_const(cond)? else {
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
    /// the opline over the subject and it. A constant subject takes its
    /// slot first, `zend_emit_op` setting `op1` before `op2`
    /// (`Z/Zend/zend_compile.c:2294`, `:2298`).
    ///
    /// ```php
    /// function c($t) { return match ($t) { 'x' => 1, 'y' => 2, default => null }; }
    /// var_dump(c('x'), c('z'));
    /// ```
    /// ```expect
    /// int(1)
    /// NULL
    /// ```
    /// ```php
    /// function s($t) { switch ($t) { case 'a': return 1; case 'b': return 2; default: return null; } }
    /// var_dump(s('b'), s('z'));
    /// ```
    /// ```expect
    /// int(2)
    /// NULL
    /// ```
    fn emit_table(&mut self, opcode: Opcode, subject: Where, size: usize) -> Table {
        let (op1_type, op1) = self.operand(subject);
        let slot = self.literal(Zval::null());
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
            size,
        }
    }

    /// `zend_hash_index_add` or `zend_hash_add` of the opline about to be
    /// emitted under the condition's value: the first case of a value
    /// keeps its target, under the key [`CaseValue`] says. Nothing where
    /// no table is being built.
    ///
    /// So a `switch` costs one string a case, which its comparison and
    /// the table share, and a `match` one a condition, the table's alone,
    /// whether or not opcache holds the bytes, as the program below does;
    /// and a table is laid at the cases or conditions written, a default
    /// and a repeated condition among them, an uncached include's and an
    /// `eval`'s alike (the program under the recipe's `TMPDIR`, the
    /// includes written during the request):
    ///
    /// ```php
    /// $put = function ($name, $src) { $f = sys_get_temp_dir() . "/$name"; file_put_contents("$f.tmp", $src); touch("$f.tmp", time() - 10); rename("$f.tmp", $f); return $f; };
    /// $f = $put('w3323-tables.inc', <<<'PHP'
    /// <?php
    /// ini_set('opcache.file_update_protection', '2');
    /// function inc($name, $src) { $f = sys_get_temp_dir() . "/w3323-$name.inc"; file_put_contents($f, $src); $m = memory_get_usage(); include $f; $d = memory_get_usage() - $m; unlink($f); return $d; }
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// inc('w0', '<?php class W {} $q0 = 1;'); m('$q1 = 1;'); m('function w1() {}');
    /// echo inc('match', '<?php function mz1($x) { return match($x) { "aa" => 1, "bb" => 2, default => 3 }; }'), " ";
    /// echo m('function me1($x) { return match($x) { "aa" => 1, "bb" => 2, default => 3 }; }'), "\n";
    /// echo inc('switch', '<?php function sz1($x) { switch ($x) { case "aa": return 1; case "bb": return 2; case "cc": return 4; } return 3; }'), " ";
    /// echo m('function se1($x) { switch ($x) { case "aa": return 1; case "bb": return 2; case "cc": return 4; } return 3; }'), "\n";
    /// echo inc('long', '<?php function mz2($x) { return match($x) { 1 => 1, 2 => 2, default => 3 }; }'), "\n";
    /// echo m('function s8($x) { switch ($x) { case "a1": case "a2": case "a3": case "a4": case "a5": case "a6": case "a7": case "a8": return 1; default: return 3; } }'), " ";
    /// echo m('function m8($x) { return match($x) { "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "c1" => 1, default => 2 }; }'), "\n";
    /// $junk = array_fill(0, 1000, str_repeat("x", 100));
    /// echo mz1("aa"), me1("bb"), sz1("cc"), se1("bb"), mz2(2), s8("a8"), m8("c1"), m8("zz"), "\n";
    /// PHP);
    /// include $f;
    /// ```
    /// ```expect regime=jit-off
    /// 1096 1112
    /// 1304 1320
    /// 872
    /// 2056 1496
    /// 12422112
    /// ```
    fn table_add(
        &mut self,
        table: &mut Option<Table>,
        case: CaseValue,
    ) -> Result<(), CompileError> {
        let Some(table) = table else {
            return Ok(());
        };
        let target = self.next();
        let (CaseValue::Compared(value) | CaseValue::Folded(value)) = case;
        let Some(key) = jump_key(value) else {
            return Ok(());
        };
        if table.entries.iter().any(|(held, _)| *held == key) {
            return Ok(());
        }
        let key = match (key, case) {
            (Key::Str(s), _) if !value.is_refcounted() => {
                match self.interning(false, |this| this.strings.intern(s.bytes())) {
                    Ok(own) => Key::Str(own),
                    Err(fatal) => return Err(self.fatal_of(fatal)),
                }
            }
            (key, CaseValue::Compared(_)) => {
                self.strings.hold(value);
                key
            }
            (key, CaseValue::Folded(_)) => key,
        };
        table.entries.push((key, Zval::long(i64::from(target))));
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
        let array = match self.strings.constant_array(&table.entries, table.size) {
            Ok(array) => array,
            Err(fatal) => return Err(self.fatal_of(fatal)),
        };
        if let Some(held) = self.constants.get_mut(table.slot as usize) {
            *held = Zval::array(array);
        }
        Ok(())
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
    fn r#match(
        &mut self,
        expr: tree::Expr,
        arms: &[NodeId<tree::MatchArm>],
    ) -> Result<Where, CompileError> {
        self.line = self.line_of(expr);
        let subject = self.expr(expr)?;
        let case_node = self.temporary();
        let mut has_default = false;
        for arm in arms {
            let arm = &self.parsed.tree[*arm];
            if arm.conds.is_none() {
                if has_default {
                    self.line = arm.line();
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
        // `CG(zend_lineno)` as the subject left it, the line a condition
        // `can_match_use_jumptable` folded is created at.
        let subject_line = self.line;
        if self.match_uses_jumptable(arms)? {
            let conds = self.match_conds(arms).len();
            table = Some(self.emit_table(Opcode::Match, subject, conds));
        } else {
            jumps = self.match_chain(subject, case_node, arms, subject_line)?;
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
    /// condition a constant `long` or `string` as `zend_eval_const_expr`
    /// leaves it, and two or more of them. Each condition the evaluator
    /// reaches and folds is replaced with its value ([`eval`]), a node
    /// that takes the line the compile stands at.
    fn match_uses_jumptable(
        &mut self,
        arms: &[NodeId<tree::MatchArm>],
    ) -> Result<bool, CompileError> {
        let mut count = 0;
        for arm in arms {
            let Some(conds) = self.parsed.tree[*arm].conds else {
                continue;
            };
            for cond in self.list(self.parsed.tree[conds].exprs) {
                count += 1;
                let Some(value) = self.eval_const(*cond)? else {
                    return Ok(false);
                };
                if !matches!(value.kind(), ZvalType::Long | ZvalType::String) {
                    return Ok(false);
                }
            }
        }
        Ok(count >= 2)
    }

    /// Every condition of every arm, in written order: the conditions
    /// php's `ZEND_AST_MATCH_ARM_LIST` walks, the default arm carrying
    /// none.
    fn match_conds(&self, arms: &[NodeId<tree::MatchArm>]) -> Vec<tree::Expr> {
        let tree = &self.parsed.tree;
        arms.iter()
            .filter_map(|arm| tree[*arm].conds)
            .flat_map(|conds| tree.list(tree[conds].exprs).iter().copied())
            .collect()
    }

    /// The chain of `zend_compile_match`: per condition, at its line, one
    /// `IS_IDENTICAL` (`CASE_STRICT` over a temporary subject) into the
    /// shared temporary and a `JMPNZ` forward. The jump of each condition.
    /// Each condition at its own line, which its node carries: a
    /// condition written a line below the arm's first is reported there
    /// and not at the arm.
    ///
    /// ```php
    /// $z = 0;
    /// echo match (9) {
    ///     1 % $z
    ///         => "a",
    ///     default => "b",
    /// };
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught DivisionByZeroError: Modulo by zero in %s:4
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 4
    /// ```
    ///
    /// A condition `can_match_use_jumptable` folded, a constant written
    /// as more than a number or a string, stands at the line the subject
    /// left:
    ///
    /// ```php
    /// var_dump(match ($u) {
    ///     null => 1,
    ///     default => 2,
    /// });
    /// ```
    /// ```expect
    /// Warning: Undefined variable $u in %s on line 2
    /// int(1)
    /// ```
    fn match_chain(
        &mut self,
        subject: Where,
        case_node: u32,
        arms: &[NodeId<tree::MatchArm>],
        subject_line: u32,
    ) -> Result<Vec<u32>, CompileError> {
        let mut jumps = Vec::new();
        for cond in self.match_conds(arms) {
            // A condition the evaluator replaced with its value
            // ([`eval`]); a value the parser made keeps its own line.
            let was_folded = self.rewritten_value(self.peel(cond)).is_some();
            self.line = self.line_of(cond);
            let cond = self.expr(cond)?;
            if was_folded {
                self.line = subject_line;
            }
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
    /// Each body at its own line, which its node carries: an arm whose
    /// `=>` and body are written below its condition reports the body's
    /// line and not the condition's.
    ///
    /// ```php
    /// $z = 0;
    /// echo match (1) {
    ///     0 => "zero",
    ///     1
    ///         =>
    ///             1 % $z,
    /// };
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught DivisionByZeroError: Modulo by zero in %s:7
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 7
    /// ```
    fn match_arms(
        &mut self,
        arms: &[NodeId<tree::MatchArm>],
        table: &mut Option<Table>,
        jumps: &[u32],
        default_jump: Option<u32>,
    ) -> Result<Option<u32>, CompileError> {
        let mut result = None;
        let mut ends = Vec::with_capacity(arms.len());
        let mut jumps = jumps.iter().copied();
        for arm in arms {
            let node = &self.parsed.tree[*arm];
            let (conds, body, line) = (node.conds, node.body, node.line());
            if let Some(conds) = conds {
                for cond in self.list(self.parsed.tree[conds].exprs) {
                    if let Some(jump) = jumps.next() {
                        self.update_jump_target_to_next(jump);
                    }
                    if table.is_some()
                        && let Some(value) =
                            self.interning(false, |this| this.constant_of(*cond))?
                    {
                        self.table_add(table, CaseValue::Folded(value))?;
                    }
                }
            } else {
                if let Some(jump) = default_jump {
                    self.update_jump_target_to_next(jump);
                }
                self.table_default_to_next(table.as_ref());
            }
            let Some(body) = body else {
                return Err(walk::unread(line, "a `match` arm"));
            };
            self.line = self.line_of(body);
            let body = self.expr(body)?;
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
    #[cold]
    #[inline(never)]
    fn compile_error(&self, message: impl Into<Cow<'static, str>>) -> CompileError {
        Refused {
            level: Level::CompileError,
            message: message.into(),
            line: self.line,
            raised: Vec::new(),
            raise: crate::Raise::Fatal,
            stage: crate::Stage::Compiling,
            declared: None,
            keys_made: 0,
        }
        .into()
    }

    /// `zend_compile_for_expr_list`: every expression compiled in turn,
    /// each one's value freed before the next is compiled, and where the
    /// last one left its value; the constant `true` for a list with
    /// nothing in it, which is what `for (;;)` tests.
    fn expr_list(&mut self, exprs: &[tree::Expr]) -> Result<Where, CompileError> {
        let mut last = Where::Constant(Zval::bool(true));
        for expr in exprs {
            self.do_free(last);
            last = match self.void_cast_operand(*expr) {
                Some(operand) => {
                    self.void_cast(operand)?;
                    Where::Constant(Zval::null())
                }
                None => self.expr(*expr)?,
            };
        }
        Ok(last)
    }

    /// `zend_compile_stmt`'s arm for an expression: a `(void)` cast
    /// ([`Self::void_cast`]), or the expression compiled and its value
    /// freed (`zend_do_free`). An arrow function returning `never` is
    /// one, its body never wrapped in a `return`:
    ///
    /// ```php
    /// $x = fn(): never => throw new Exception('Here');
    /// try { $x(); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// $y = fn(): never => 42;
    /// try { $y(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Here
    /// {closure:%s:4}(): never-returning function must not implicitly return
    /// ```
    fn expr_stmt(&mut self, expr: tree::Expr) -> Result<(), CompileError> {
        if let Some(operand) = self.void_cast_operand(expr) {
            return self.void_cast(operand);
        }
        let value = self.expr(expr)?;
        self.do_free(value);
        Ok(())
    }

    /// The operand of a `(void)` cast, php's `ZEND_AST_CAST_VOID`, which
    /// the grammar allows at a statement's start and in a `for`
    /// header's lists alone; none for any other expression.
    fn void_cast_operand(&self, expr: tree::Expr) -> Option<tree::Expr> {
        let tree::Expr::CastExpr(id) = expr else {
            return None;
        };
        let node = &self.parsed.tree[id];
        let op = node.op?;
        (php_syntax::cast_name(self.text(op)) == "T_VOID_CAST").then_some(node.operand?)
    }

    /// `zend_compile_void_cast` (`Z/Zend/zend_compile.c`): the operand
    /// compiled as any expression, so what a call returns is used and no
    /// `#[\NoDiscard]` warning is raised for it, then freed by a `FREE`
    /// marked `ZEND_FREE_VOID_CAST` where it is a temporary, and as
    /// `zend_do_free` frees a constant.
    ///
    /// ```php
    /// #[NoDiscard] function f(): int { return 1; }
    /// (void) f();
    /// (void) 1;
    /// for ($i = 0; $i < 1; (void) $i++) {}
    /// echo $i, "\n";
    /// ```
    /// ```expect
    /// 1
    /// ```
    fn void_cast(&mut self, operand: tree::Expr) -> Result<(), CompileError> {
        let (op1_type, op1) = match self.expr(operand)? {
            Where::Temporary(at) => (OperandKind::Tmp, at),
            Where::Returned(at) => (OperandKind::Var, at),
            constant @ Where::Constant(_) => {
                self.do_free(constant);
                return Ok(());
            }
            Where::Cv(_) => return Ok(()),
        };
        self.push(Opline {
            opcode: Opcode::Free,
            op1_type,
            op1,
            extended_value: FREE_VOID_CAST,
            ..Opline::default()
        });
        Ok(())
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
    /// too, which a compile with extended info emits after a call; so `@f();` marks the call's
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
                let Some(last) = self.last_that_left_a_var() else {
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
        self.last_stepping_past(&[])
    }

    /// [`Self::last_that_left_a_value`] for a `VAR`, which steps back
    /// over the `EXT_FCALL_END` after a call as well.
    fn last_that_left_a_var(&mut self) -> Option<&mut Opline> {
        self.last_stepping_past(&[Opcode::ExtFcallEnd])
    }

    /// The last opline that is no `OP_DATA`, no `END_SILENCE` and none
    /// of `also`.
    fn last_stepping_past(&mut self, also: &[Opcode]) -> Option<&mut Opline> {
        self.oplines.iter_mut().rev().find(|opline| {
            !matches!(opline.opcode, Opcode::OpData | Opcode::EndSilence)
                && !also.contains(&opline.opcode)
        })
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
            Where::Constant(value) => (OperandKind::Const, self.literal(value)),
            Where::Returned(at) => (OperandKind::Var, at),
            Where::Temporary(at) => (OperandKind::Tmp, at),
            Where::Cv(at) => (OperandKind::Cv, at),
        }
    }

    /// `zend_compile_expr`: compile the expression this node is and say
    /// where its value is (design §4.9).
    ///
    /// `zend_compile_expr_inner` opens with
    /// `CG(zend_lineno) = zend_ast_get_lineno(ast)`
    /// (`Z/Zend/zend_compile.c:11807-11808`), so every opline this
    /// emits carries the line of the expression it came from and not
    /// the line its statement opened on; and `ZEND_CHECK_STACK_LIMIT`
    /// stands there too, which is one native frame per node of the tree
    /// here as it is one per node of `zend_ast` there. The line is left
    /// where the last expression compiled put it, as php leaves it: the
    /// `ADD` of `1\n+\n$a` is emitted after the right side, at the right
    /// side's line.
    ///
    /// A `(` and a `)` around an expression are no node of php's: its
    /// grammar hands the compiler what stands inside them, and the one
    /// thing they leave is what [`Self::conditional`] reads, a
    /// conditional that is a conditional's child with no parentheses
    /// between them. A node this compiler cannot emit yet is refused
    /// where it stands, in the one-line shape the phpt lane counts as
    /// unread.
    ///
    /// A `while` whose condition stands below the keyword raises from
    /// the condition's line, not the keyword's:
    ///
    /// ```php
    /// $a = 1;
    /// while
    /// (
    ///
    ///     1 / 0
    /// )
    /// {
    ///     $a = 0;
    /// }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught DivisionByZeroError: Division by zero in %s:6
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 6
    /// ```
    ///
    /// So does an argument written on its own line of a call spread
    /// over several, while the call's own oplines stay on the line the
    /// callee was named at (`zend_compile_call_common`'s `lineno`,
    /// `Z/Zend/zend_compile.c:3999`), which is the line the trace
    /// names:
    ///
    /// ```php
    /// function f($x) { throw new Exception("x"); }
    /// echo f(
    ///     $undefined
    /// );
    /// ```
    /// ```expect exit=255
    ///
    /// Warning: Undefined variable $undefined in %s on line 4
    ///
    /// Fatal error: Uncaught Exception: x in %s:2
    /// Stack trace:
    /// #0 %s(3): f(NULL)
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// `expr instanceof C` is `expr T_INSTANCEOF class_name_reference`
    /// with no associativity declared, so a second `instanceof` reads
    /// the first as its left side, php's `($o instanceof E) instanceof
    /// F`, which the grammar groups to the left as this compiles it;
    /// the oracle at 8.5.9 compiles it to two `INSTANCEOF`s, the second
    /// over the first's temporary, and a bool on the left is no object,
    /// so the chain is `false`.
    ///
    /// ```php
    /// $o = 1;
    /// var_dump($o instanceof E instanceof F);
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    ///
    /// Where the node is one a chain of `?->` can end in and no link over
    /// it marked it inner ([`Self::chain_link`]), every `JMP_NULL` the
    /// node laid down is aimed past it here ([`chain`]), php's
    /// `zend_compile_expr` and its `zend_short_circuiting_commit`.
    pub(crate) fn expr(&mut self, expr: tree::Expr) -> Result<Where, CompileError> {
        self.expr_under(expr, Fetch::Read)
    }

    /// A [`Source`] compiled: the expression where it stands, or the
    /// value already held.
    pub(crate) fn source(&mut self, source: Source) -> Result<Where, CompileError> {
        match source {
            Source::Expr(expr) => self.expr(expr),
            Source::Node(value) => Ok(value),
        }
    }

    /// [`Self::expr`] where the node stands under a fetch of its own,
    /// php's `zend_compile_var(result, ast, type, …)`: a call among what
    /// a write, an `unset`, a `??` or a by-name argument fetches is
    /// compiled under that `type`, which keeps it out of the specials
    /// php compiles only for a read, `chr`, `ord` and the frameless
    /// calls ([`Self::call`]); every other node is compiled as a read's.
    fn expr_under(&mut self, expr: tree::Expr, fetch: Fetch) -> Result<Where, CompileError> {
        let node = self.peel(expr);
        // `ZEND_AST_ZVAL` where `zend_eval_const_expr` folded the node.
        if let Some(value) = self.rewritten_value(node) {
            return Ok(Where::Constant(value));
        }
        if self.memo.mode != memo::MemoMode::None {
            return self.memoized(node);
        }
        let inner = self.chain_inner.take() == Some(node);
        match self.chain_end(node) {
            Some(end) if !inner => self.chain_ended(node, fetch, end),
            _ => self.expr_node(node, fetch),
        }
    }

    /// [`Self::expr_under`] over a node a chain of `?->` ends at, every
    /// `JMP_NULL` its compile laid down aimed past it. A call of its own,
    /// so the value it holds for the aim is off the frame every other
    /// node's compile answers straight into, a level of a nested
    /// expression among them (#3208).
    #[inline(never)]
    fn chain_ended(
        &mut self,
        node: tree::Expr,
        fetch: Fetch,
        end: chain::ChainEnd,
    ) -> Result<Where, CompileError> {
        let checkpoint = self.short_circuits.len();
        let value = self.expr_node(node, fetch)?;
        self.short_circuit_commit(checkpoint, value, end);
        Ok(value)
    }

    /// [`Self::expr`]'s body over one node, php's `zend_compile_expr_inner`.
    ///
    /// Every compile of a nested expression passes through this dispatch
    /// once per level, as php's passes through `zend_compile_expr_inner`,
    /// so it holds the dispatch and nothing else: each arm is one call
    /// whose answer is this function's, and what an arm reads of its
    /// node is read in that call's frame. An arm that read its node here
    /// would add its locals to every level of every nesting, which an
    /// unoptimised build lays out side by side rather than overlapping.
    ///
    /// Inlined into [`Self::expr_under`] and [`Self::chain_ended`], so
    /// the dispatch answers straight into the slot `expr_under`'s caller
    /// gave it and a left-nested chain stands in one 160-byte frame per
    /// operator, php's two frames' size. Left to the optimiser, which
    /// inlines it nowhere once it has two callers, it is a frame of its
    /// own with its arms inlined into it, 2,048 bytes a level (#3208).
    #[allow(clippy::inline_always)] // measured on #3208: the dispatch stands in expr_under's frame
    #[inline(always)]
    fn expr_node(&mut self, expr: tree::Expr, fetch: Fetch) -> Result<Where, CompileError> {
        let line = expr.line(self.parsed);
        self.line = line;
        self.check_stack()?;
        let tree = &self.parsed.tree;
        match expr {
            tree::Expr::ParenExpr(id) => self.paren_expr(id),
            tree::Expr::MagicConstExpr(id) => self.magic(id),
            tree::Expr::LiteralExpr(_) | tree::Expr::NowdocExpr(_) => self.literal_expr(expr),
            tree::Expr::NameRef(token) => self.name_expr(token),
            tree::Expr::InterpolatedStringExpr(id) => self.double_quoted_expr(id, line),
            tree::Expr::HeredocExpr(id) => self.heredoc_expr(id, line),
            tree::Expr::ShellExecExpr(id) => self.shell_exec(id, line),
            tree::Expr::SimpleVarExpr(id) => self.simple_var_expr(id, line, fetch),
            tree::Expr::VarVarExpr(id) => {
                self.operand_of(tree[id].inner, "a variable", Self::var_var)
            }
            tree::Expr::DynamicVarExpr(id) => {
                self.operand_of(tree[id].inner, "a variable", Self::var_var)
            }
            tree::Expr::ArrayExpr(id) => self.array_expr(id),
            tree::Expr::ListExpr(_) => {
                Err(self.compile_error("Cannot use list() as standalone expression"))
            }
            tree::Expr::UnaryExpr(id) => self.unary_expr(id),
            tree::Expr::PrefixIncDecExpr(id) => self.prefix_expr(id),
            tree::Expr::PostfixIncDecExpr(id) => self.postfix_expr(id),
            tree::Expr::CastExpr(id) => self.cast_expr(id, line),
            tree::Expr::ErrorSuppressExpr(id) => {
                self.operand_of(tree[id].operand, "an `@`", Self::silence)
            }
            tree::Expr::CloneExpr(id) => self.clone_expr(id, line),
            tree::Expr::ExitExpr(id) => self.exit_expr(id, line),
            tree::Expr::PrintExpr(id) => {
                self.operand_of(tree[id].operand, "a `print`", Self::print)
            }
            tree::Expr::ThrowExpr(id) => {
                self.operand_of(tree[id].operand, "a `throw`", Self::throw)
            }
            tree::Expr::BinaryExpr(id) => self.binary_expr(id),
            tree::Expr::InstanceofExpr(id) => self.instanceof_expr(id),
            tree::Expr::AssignExpr(id) => self.assignment(id, line),
            tree::Expr::TernaryExpr(id) => self.ternary_expr(id),
            tree::Expr::MatchExpr(id) => self.match_expr(id),
            tree::Expr::NewExpr(id) => self.new_expr(id, line),
            tree::Expr::YieldExpr(id) => self.yield_expr(id),
            tree::Expr::YieldFromExpr(id) => self.yield_from(id),
            tree::Expr::IncludeExpr(id) => self.include_expr(id),
            tree::Expr::IssetExpr(id) => self.isset_expr(id),
            tree::Expr::EmptyExpr(id) => {
                self.operand_of(tree[id].target, "an `empty`", |emit, target| {
                    emit.isset_or_empty(target, true)
                })
            }
            tree::Expr::EvalExpr(id) => {
                self.operand_of(tree[id].code, "an `eval`", |emit, code| {
                    emit.include_or_eval(IncludeKind::Eval, code)
                })
            }
            tree::Expr::CallExpr(id) => self.call_of(id, line, fetch),
            tree::Expr::MemberAccessExpr(_) => self.prop(expr),
            tree::Expr::ScopedAccessExpr(id) => self.scoped_expr(id, line),
            tree::Expr::SubscriptExpr(_) => self.dim_read(expr),
            tree::Expr::ClosureExpr(_) | tree::Expr::ArrowFnExpr(_) => self.closure_of(expr, line),
        }
    }

    /// A node's one operand, which the grammar requires and recovery
    /// may have left out, refused as `what` where it did, and compiled
    /// by `compile` where it stands.
    fn operand_of(
        &mut self,
        operand: Option<tree::Expr>,
        what: &str,
        compile: fn(&mut Self, tree::Expr) -> Result<Where, CompileError>,
    ) -> Result<Where, CompileError> {
        let operand = self.required(operand, what)?;
        compile(self, operand)
    }

    /// `'(' expr ')'`, which php's grammar drops: the inner node.
    fn paren_expr(&mut self, id: NodeId<tree::ParenExpr>) -> Result<Where, CompileError> {
        match self.parsed.tree[id].inner {
            Some(inner) => self.expr(inner),
            None => Err(self.unread("an expression inside parentheses")),
        }
    }

    /// `ZEND_AST_ZVAL`: a number, a string or a nowdoc as written.
    fn literal_expr(&mut self, expr: tree::Expr) -> Result<Where, CompileError> {
        self.constant_value(expr).map(Where::Constant)
    }

    /// `zend_compile_const`: `true`, `false` and `null` are the values
    /// `zend_try_ct_eval_const` substitutes whatever their case, and
    /// every other name is a constant fetched by the name as written.
    fn name_expr(&mut self, token: TokenId) -> Result<Where, CompileError> {
        if let Some(value) = self.constant_name(token) {
            return Ok(Where::Constant(value));
        }
        self.const_fetch(token)
    }

    /// `ZEND_AST_ENCAPS_LIST` written between double quotes.
    fn double_quoted_expr(
        &mut self,
        id: NodeId<tree::InterpolatedStringExpr>,
        line: u32,
    ) -> Result<Where, CompileError> {
        let parts = self.list(self.parsed.tree[id].parts);
        let holder = Holder::DoubleQuoted(&self.parsed.tree[id]);
        self.encaps(tree::Expr::InterpolatedStringExpr(id), parts, holder, line)
    }

    /// `ZEND_AST_ENCAPS_LIST` written as a heredoc.
    fn heredoc_expr(
        &mut self,
        id: NodeId<tree::HeredocExpr>,
        line: u32,
    ) -> Result<Where, CompileError> {
        let parts = self.list(self.parsed.tree[id].parts);
        let holder = Holder::Heredoc(&self.parsed.tree[id]);
        self.encaps(tree::Expr::HeredocExpr(id), parts, holder, line)
    }

    /// `zend_compile_simple_var` through `zend_try_compile_cv`: the
    /// variable is its CV, fetched under `fetch` ([`Self::cv_fetched`]),
    /// and no opline reads it into anything; `$this` under `BP_VAR_R` is
    /// `FETCH_THIS` into a temporary, and an auto-global `FETCH_R
    /// (global)` over its name.
    fn simple_var_expr(
        &mut self,
        id: NodeId<tree::SimpleVarExpr>,
        line: u32,
        fetch: Fetch,
    ) -> Result<Where, CompileError> {
        match self.simple_var_of(id, line)? {
            SimpleVar::Cv(name) => {
                self.cv_fetched(name, fetch);
                Ok(Where::Cv(self.lookup_cv(name)))
            }
            SimpleVar::This => Ok(self.fetch_this(OperandKind::Tmp)),
            SimpleVar::Auto(global) => self.fetch_by_name(global.name(), Symbols::Global),
            SimpleVar::Globals => Ok(self.fetch_globals(OperandKind::Tmp)),
        }
    }

    /// `ZEND_AST_ARRAY` read as a value.
    fn array_expr(&mut self, id: NodeId<tree::ArrayExpr>) -> Result<Where, CompileError> {
        let elements = self.list(self.parsed.tree[id].array_elements);
        if let Some(made) = self.constant_array(elements)? {
            let value = self.claimed(tree::Expr::ArrayExpr(id), Operated::Made(made));
            return Ok(Where::Constant(value));
        }
        self.array(elements)
    }

    /// `ZEND_AST_UNARY_MINUS`, `ZEND_AST_UNARY_PLUS` and
    /// `ZEND_AST_UNARY_OP`, each to its compile.
    fn unary_expr(&mut self, id: NodeId<tree::UnaryExpr>) -> Result<Where, CompileError> {
        let node = &self.parsed.tree[id];
        let operand = self.required(node.operand, "a unary operator")?;
        match node.op.map(|op| self.kind(op)) {
            Some(SyntaxKind::MINUS) => self.unary(true, operand),
            Some(SyntaxKind::PLUS) => self.unary(false, operand),
            Some(SyntaxKind::BANG) => self.bool_not(operand),
            Some(SyntaxKind::TILDE) => self.bw_not(id, operand),
            _ => Err(self.unread("a unary operator")),
        }
    }

    /// `ZEND_AST_PRE_INC` and `ZEND_AST_PRE_DEC`.
    fn prefix_expr(&mut self, id: NodeId<tree::PrefixIncDecExpr>) -> Result<Where, CompileError> {
        let node = &self.parsed.tree[id];
        let up = node.op.map(|op| self.kind(op)) == Some(SyntaxKind::PLUS_PLUS);
        let target = self.required(node.operand, if up { "a `++`" } else { "a `--`" })?;
        self.step(target, up, true)
    }

    /// `ZEND_AST_POST_INC` and `ZEND_AST_POST_DEC`.
    fn postfix_expr(&mut self, id: NodeId<tree::PostfixIncDecExpr>) -> Result<Where, CompileError> {
        let node = &self.parsed.tree[id];
        let up = node.op.map(|op| self.kind(op)) == Some(SyntaxKind::PLUS_PLUS);
        self.step(node.operand, up, false)
    }

    /// `ZEND_AST_CAST`.
    fn cast_expr(&mut self, id: NodeId<tree::CastExpr>, line: u32) -> Result<Where, CompileError> {
        let node = &self.parsed.tree[id];
        let to = self.cast_to(node.op, line)?;
        let operand = self.required(node.operand, "a cast")?;
        self.cast(to, operand)
    }

    /// `clone`, the call php's parser makes of it, `\clone` in the
    /// function table: `clone $o` one argument, and `clone(...)` its
    /// argument list as written, 8.5's `clone($o, ['p' => 1])` among
    /// them, which `zend_compile_func_clone` leaves to the function.
    fn clone_expr(
        &mut self,
        id: NodeId<tree::CloneExpr>,
        line: u32,
    ) -> Result<Where, CompileError> {
        let node = &self.parsed.tree[id];
        let arguments = match (node.operand, node.args) {
            (Some(operand), _) => Args::of(self.arena, &[operand], self.line_of(operand)),
            (None, Some(args)) => self.arguments(args, line)?,
            (None, None) => return Err(self.unread("a `clone`")),
        };
        self.call(b"\\clone", &arguments, Fetch::Read)
    }

    /// `exit`, the call php's parser makes of it, `\exit` in the
    /// function table.
    fn exit_expr(&mut self, id: NodeId<tree::ExitExpr>, line: u32) -> Result<Where, CompileError> {
        let arguments = match self.parsed.tree[id].args {
            Some(args) => self.arguments(args, line)?,
            None => Args::of(self.arena, &[], line),
        };
        self.call(b"\\exit", &arguments, Fetch::Read)
    }

    /// `ZEND_AST_AND`, `ZEND_AST_OR`, `ZEND_AST_COALESCE`, the pipe and
    /// `ZEND_AST_BINARY_OP`, each to its compile.
    ///
    /// Inlined into [`Self::expr_node`], with [`Self::binary`], so a
    /// left-nested chain stands in one frame per operator, where php's
    /// stands in `zend_compile_expr_inner` and `zend_compile_binary_op`,
    /// rather than two each saving every register it uses (#3159).
    #[allow(clippy::inline_always)] // measured on #3159: one frame a level of `1+1+…`, not two
    #[inline(always)]
    fn binary_expr(&mut self, id: NodeId<tree::BinaryExpr>) -> Result<Where, CompileError> {
        let node = &self.parsed.tree[id];
        let left = node.lhs;
        let right = self.required(node.rhs, "a binary operator")?;
        match node.op.map(|op| self.kind(op)) {
            Some(SyntaxKind::AMP_AMP | SyntaxKind::AND_KW) => {
                self.short_circuiting(false, left, right)
            }
            Some(SyntaxKind::PIPE_PIPE | SyntaxKind::OR_KW) => {
                self.short_circuiting(true, left, right)
            }
            Some(SyntaxKind::QUESTION_QUESTION) => self.coalesce(left, right),
            Some(SyntaxKind::PIPE_GT) => self.pipe(left, right),
            Some(kind) => match binary_op(kind) {
                Some(op) => self.binary(id, op, left, right),
                None => Err(self.unread("a binary operator")),
            },
            None => Err(self.unread("a binary operator")),
        }
    }

    /// `ZEND_AST_INSTANCEOF`.
    fn instanceof_expr(&mut self, id: NodeId<tree::InstanceofExpr>) -> Result<Where, CompileError> {
        let node = &self.parsed.tree[id];
        let Some(class) = node.rhs.map(ClassSpec::of) else {
            return Err(self.unread("an `instanceof`"));
        };
        self.instance_of(node.lhs, class)
    }

    /// `ZEND_AST_CONDITIONAL`.
    fn ternary_expr(&mut self, id: NodeId<tree::TernaryExpr>) -> Result<Where, CompileError> {
        let node = &self.parsed.tree[id];
        self.conditional(node.cond, node.then, node.else_branch)
    }

    /// `ZEND_AST_MATCH`.
    fn match_expr(&mut self, id: NodeId<tree::MatchExpr>) -> Result<Where, CompileError> {
        let node = &self.parsed.tree[id];
        let subject = self.required(node.subject, "a `match`")?;
        let arms = self.list(node.match_arms);
        self.r#match(subject, arms)
    }

    /// `ZEND_AST_NEW`, over a class named or an anonymous one.
    fn new_expr(&mut self, id: NodeId<tree::NewExpr>, line: u32) -> Result<Where, CompileError> {
        let node = &self.parsed.tree[id];
        if let Some(anon) = node.anon {
            let args = self.parsed.tree[anon].args;
            let class = ClassRef::Fetched(self.anonymous_class(anon)?);
            let arguments = match args {
                Some(args) => self.arguments(args, line)?,
                None => Args::of(self.arena, &[], line),
            };
            return self.new_of(&class, &arguments, line);
        }
        let Some(class) = node.ctor.map(ClassSpec::of) else {
            return Err(self.unread("a `new`"));
        };
        let arguments = match node.args {
            Some(args) => self.arguments(args, line)?,
            None => Args::of(self.arena, &[], line),
        };
        self.new_expression(class, &arguments)
    }

    /// `ZEND_AST_INCLUDE_OR_EVAL` written as one of the four includes.
    fn include_expr(&mut self, id: NodeId<tree::IncludeExpr>) -> Result<Where, CompileError> {
        let node = &self.parsed.tree[id];
        let kind = match node.op.map(|op| self.kind(op)) {
            Some(SyntaxKind::INCLUDE_KW) => IncludeKind::Include,
            Some(SyntaxKind::INCLUDE_ONCE_KW) => IncludeKind::IncludeOnce,
            Some(SyntaxKind::REQUIRE_KW) => IncludeKind::Require,
            Some(SyntaxKind::REQUIRE_ONCE_KW) => IncludeKind::RequireOnce,
            _ => return Err(self.unread("an `include`")),
        };
        let path = self.required(node.operand, "an `include`")?;
        self.include_or_eval(kind, path)
    }

    /// `ZEND_AST_ISSET` over each target, `&&` between them.
    fn isset_expr(&mut self, id: NodeId<tree::IssetExpr>) -> Result<Where, CompileError> {
        let targets = self.list(self.parsed.tree[id].targets);
        let Some((&first, rest)) = targets.split_first() else {
            return Err(self.unread("an `isset`"));
        };
        let mut held = self.isset_or_empty(first, false)?;
        for &target in rest {
            held =
                self.short_circuit_from(false, held, |emit| emit.isset_or_empty(target, false))?;
        }
        Ok(held)
    }

    /// `ZEND_AST_CLASS_CONST`, `ZEND_AST_STATIC_PROP` and
    /// `ZEND_AST_CLASS_NAME`, which the tree writes as one `::`.
    fn scoped_expr(
        &mut self,
        id: NodeId<tree::ScopedAccessExpr>,
        line: u32,
    ) -> Result<Where, CompileError> {
        match self.scoped_of(id, line)? {
            Scoped::Constant { class, name } => self.class_const(id, class, &name),
            Scoped::Property { class, name } => self.static_prop(class, &name),
            Scoped::ClassName { class } => self.class_name_of(class),
        }
    }

    /// `ZEND_AST_CLOSURE` and `ZEND_AST_ARROW_FUNC`.
    fn closure_of(&mut self, expr: tree::Expr, line: u32) -> Result<Where, CompileError> {
        let closure = self.walk_closure(expr, line)?;
        self.closure(&closure)
    }

    /// `__LINE__` and its kin, php's `ZEND_AST_MAGIC_CONST` with the
    /// token in `attr` and the token's own line in `lineno`, which is
    /// what `__LINE__` folds to and not the statement's
    /// (`zend_try_ct_eval_magic_const`).
    fn magic_node(&mut self, id: NodeId<tree::MagicConstExpr>) -> Result<Zval, CompileError> {
        let (which, line) = self.magic_of(id)?;
        match self.magic_const(which, line) {
            Folded::Line(n) => Ok(Zval::long(n)),
            Folded::Text(text) => self.string(&text),
            Folded::Dir(dir, room) => (self.strings.intern_in(&dir, room))
                .map(Zval::string)
                .map_err(|fatal| self.fatal_of(fatal)),
            Folded::FunctionName => self.function_name_made().map(|made| made.value()),
            Folded::FileName => self.file_name_made().map(|made| made.value()),
            Folded::ClassName => self.class_name_made().map(|made| made.value()),
            Folded::UsingClass => {
                Err(self.unread("a `__CLASS__` in a trait's constant expression"))
            }
        }
    }

    /// `zend_compile_magic_const` (`Z/Zend/zend_compile.c:11301`): the
    /// constant a magic constant folds to, and for `__CLASS__` in a
    /// trait `FETCH_CLASS_NAME` with `ZEND_FETCH_CLASS_SELF` in an unused
    /// `op1`, which the program answers with the using class.
    ///
    /// ```php
    /// trait T { function f() { var_dump(__CLASS__, __TRAIT__, __METHOD__, __FUNCTION__); } }
    /// class C { use T; }
    /// class D extends C {}
    /// (new D)->f();
    /// var_dump(__TRAIT__);
    /// ```
    /// ```expect exit=0
    /// string(1) "C"
    /// string(1) "T"
    /// string(4) "T::f"
    /// string(1) "f"
    /// string(0) ""
    /// ```
    fn magic(&mut self, id: NodeId<tree::MagicConstExpr>) -> Result<Where, CompileError> {
        let (which, line) = self.magic_of(id)?;
        if !matches!(self.magic_const(which, line), Folded::UsingClass) {
            return Ok(Where::Constant(self.magic_node(id)?));
        }
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::FetchClassName,
            op1_type: OperandKind::Unused,
            op1: FETCH_CLASS_SELF,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// Which magic constant a `MagicConstExpr` is, by its token's name
    /// in `php_syntax`, and the node's line.
    fn magic_of(&self, id: NodeId<tree::MagicConstExpr>) -> Result<(Magic, u32), CompileError> {
        let node = &self.parsed.tree[id];
        let which = node
            .token
            .and_then(|token| self.parsed.tokens.get(token.0 as usize))
            .and_then(|token| {
                self.parsed
                    .source
                    .get(token.offset as usize..(token.offset + token.len) as usize)
            })
            .and_then(Magic::of);
        match which {
            Some(which) => Ok((which, node.line)),
            None => Err(self.unread("a magic constant")),
        }
    }

    /// `zend_compile_isset_or_empty` (`Z/Zend/zend_compile.c:10816`),
    /// whose arms are the variable nodes: a `ZEND_AST_VAR` that
    /// `zend_try_compile_cv` makes a CV is `ISSET_ISEMPTY_CV` over the
    /// CV into a temporary, `$this` is `ISSET_ISEMPTY_THIS`, and a name
    /// the program computes `ISSET_ISEMPTY_VAR` over the name; a
    /// dimension, a property and a class's property are the chain
    /// compiled under `BP_VAR_IS` with the last link's opcode
    /// overwritten, `ISSET_ISEMPTY_DIM_OBJ`, `ISSET_ISEMPTY_PROP_OBJ` or
    /// `ISSET_ISEMPTY_STATIC_PROP`, so the link that would have fetched
    /// asks instead. `ZEND_ISEMPTY` goes into `extended_value` for
    /// `empty` and nothing for `isset`. What is no variable is `!expr`
    /// under `empty`, and php's compile error under `isset`; `$GLOBALS`
    /// itself is set.
    ///
    /// Where a `JMPZ` or a `JMPNZ` follows over the temporary, the
    /// handler branches over it (`ZEND_VM_SMART_BRANCH`); the compiler
    /// emits the two oplines all the same, as `TYPE_CHECK`'s are.
    ///
    /// Every link under the last is `FETCH_DIM_IS`, the read that raises
    /// nothing, so a chain says no where a read would have warned:
    ///
    /// ```php
    /// $a = [1, 'k' => null];
    /// var_dump(isset($a[0]), isset($a['k']), isset($a[9]), empty($a[0]));
    /// var_dump(isset($a[0][1]), isset($a[9][8][7]), empty($a['k'][0]));
    /// $s = "abc";
    /// var_dump(isset($s[1]), isset($s[9]), isset($s["x"]), empty($s[0]));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// ```
    ///
    /// A class's property, `$this`, a name the program computes and a
    /// property under a property, each asked and none fetched, and
    /// `empty` over an expression:
    ///
    /// ```php
    /// class C { public static $s = 1; private static $p = 2; public static $n; public $o;
    ///     function m() { return [isset($this), empty($this), isset($this->o->q), isset(self::$p)]; } }
    /// $c = new C; $c->o = new stdClass; $c->o->q = [0];
    /// var_dump(isset(C::$s), isset(C::$n), isset(C::$p), isset(C::$none), empty(C::$s), $c->m());
    /// $n = 'c'; $m = 'none';
    /// var_dump(isset($$n), isset($$m), empty($$n), isset($$n->o->q[0]), empty($c->o->q[0]), isset($this));
    /// var_dump(empty([]), empty([0]), empty(f()));
    /// function f() { return ''; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// array(4) {
    ///   [0]=>
    ///   bool(true)
    ///   [1]=>
    ///   bool(false)
    ///   [2]=>
    ///   bool(true)
    ///   [3]=>
    ///   bool(true)
    /// }
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
    /// ```php
    /// isset(1 + 1);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use isset() on the result of an expression (you can use "null !== expression" instead) in %s on line 2
    /// ```
    ///
    /// A property of a variable fetched by name stands on that variable's
    /// `FETCH_IS`, so a name nothing holds is no warning, and a hooked
    /// property's `get` answers `isset` and `empty` as the object has it:
    ///
    /// ```php
    /// class C { public ?int $q = null; public int $h { get => 3; } }
    /// $_GET = new C;
    /// $n = 'o';
    /// $o = new C;
    /// $m = 'none';
    /// var_dump(isset($_GET->h), empty($_GET->q), isset($$n?->h), isset($$m->p), empty($GLOBALS['o']->h));
    /// unset($_COOKIE);
    /// var_dump(isset($_COOKIE->p), isset($_COOKIE?->p));
    /// $a = ['o' => $o];
    /// $o->q = 1;
    /// var_dump(isset($a['o']->q), isset($a['o']->none), empty($a['x']->q));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// ```
    ///
    /// A variable fetched by name is asked in its table and never
    /// fetched, so nothing is warned of:
    ///
    /// ```php
    /// $a = 1; $z = 0;
    /// $names = ['a', 'z', 'none'];
    /// foreach ($names as $name) { var_dump(isset($$name), empty(${$name})); }
    /// var_dump(isset($_SERVER), empty($_FILES), isset($_ENV['nothing']), isset(${'a'}), empty($GLOBALS['z']));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// ```
    fn isset_or_empty(&mut self, var: tree::Expr, empty: bool) -> Result<Where, CompileError> {
        let extended_value = u32::from(empty);
        let var = self.peel(var);
        let by_name = self.is_by_name(var);
        // `!zend_is_variable(var_ast)`: `empty(expr)` is `!expr`, and
        // `isset(expr)` php's compile error.
        if !self.is_variable(var) && !by_name {
            if empty {
                return self.bool_not(var);
            }
            return Err(self.compile_error(
                "Cannot use isset() on the result of an expression (you can use \"null !== expression\" instead)",
            ));
        }
        // `is_globals_fetch`: `$GLOBALS` itself is always set.
        if self.is_globals(var) {
            return Ok(Where::Constant(Zval::bool(!empty)));
        }
        // `is_global_var_fetch`'s own arm over `$GLOBALS[]`.
        if self.is_globals_append(var) {
            return Err(self.compile_error("Cannot use [] for reading"));
        }
        // A `ZEND_AST_VAR`, `$GLOBALS['k']` among them: the CV, `$this`,
        // or `zend_compile_simple_var_no_cv` under `BP_VAR_IS` made
        // `ISSET_ISEMPTY_VAR`, the name in `op1` and the table's fetch
        // type beside `ZEND_ISEMPTY` in `extended_value`, the bucket
        // asked and never fetched.
        if by_name || matches!(var, tree::Expr::SimpleVarExpr(_)) {
            let Some(named) = self.named(var)? else {
                return Err(self.fatal("an `isset` over a name that is no variable"));
            };
            let (opcode, (op1_type, op1), fetch_type) = match named {
                by_name::Named::Cv(name) => (
                    Opcode::IssetIsemptyCv,
                    (OperandKind::Cv, self.lookup_cv(&name)),
                    0,
                ),
                by_name::Named::This => (Opcode::IssetIsemptyThis, (OperandKind::Unused, 0), 0),
                by_name::Named::Globals => return Ok(Where::Constant(Zval::bool(!empty))),
                by_name::Named::Fetch { name, table } => (
                    Opcode::IssetIsemptyVar,
                    self.operand(name),
                    table.fetch_type(),
                ),
            };
            let out = self.temporary();
            self.push(Opline {
                opcode,
                op1_type,
                op1,
                out_type: OperandKind::Tmp,
                out,
                extended_value: fetch_type | extended_value,
                ..Opline::default()
            });
            return Ok(Where::Temporary(out));
        }
        // `zend_compile_dim`, `zend_compile_prop` or
        // `zend_compile_static_prop` under `BP_VAR_IS`, the `??`'s own
        // chain ([`Self::silent_link`]), with the last fetch's opcode
        // overwritten: the link that would have read asks instead. The
        // cache slots of a property stand in `extended_value` where the
        // fetch left them, `ZEND_ISEMPTY` in the bit below them.
        let mut delayed = Vec::new();
        self.silent_link(var, &mut delayed, Place::Outermost)?;
        let Some(last) = delayed.last_mut() else {
            return Err(self.fatal("an `isset` over a chain with no link"));
        };
        last.opcode = match last.opcode {
            Opcode::FetchDimIs => Opcode::IssetIsemptyDimObj,
            Opcode::FetchObjIs => Opcode::IssetIsemptyPropObj,
            Opcode::FetchStaticPropIs => Opcode::IssetIsemptyStaticProp,
            _ => return Err(self.fatal("an `isset` over a link that is no fetch")),
        };
        last.extended_value |= extended_value;
        let Some(out) = self.lay_silent(&mut delayed, None) else {
            return Err(self.fatal("an `isset` over a chain with no link"));
        };
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_print` (`Z/Zend/zend_compile.c:10700`): the operand,
    /// then `ZEND_ECHO` over it with `extended_value` 1, php's mark of
    /// an `ECHO` a `print` emitted; the expression's value is the
    /// constant `1`, so a `print` reads as `int(1)` where it is used and
    /// a condition over it folds nothing away, the `JMPZ` standing over
    /// the constant.
    ///
    /// ```php
    /// $r = print "a";
    /// var_dump($r);
    /// if (print "b") echo "c";
    /// $n = 0;
    /// echo $n ? 1 : print "d", "\n";
    /// print "e" . print "f";
    /// ```
    /// ```expect
    /// aint(1)
    /// bcd1
    /// fe1
    /// ```
    fn print(&mut self, operand: tree::Expr) -> Result<Where, CompileError> {
        let value = self.expr(operand)?;
        let (op1_type, op1) = self.operand(value);
        self.push(Opline {
            opcode: Opcode::Echo,
            op1_type,
            op1,
            extended_value: 1,
            ..Opline::default()
        });
        Ok(Where::Constant(Zval::long(1)))
    }

    /// `zend_compile_throw` (`Z/Zend/zend_compile.c:5795`): the operand,
    /// then `ZEND_THROW` over it with nothing in `result`. The value the
    /// expression leaves is the constant `true`, php's `ZVAL_TRUE` for
    /// an expression `throw`; nothing downstream reads it, since the
    /// opline leaves a throwable pending whatever stands after it, and
    /// `zend_do_free` over it is what leaves `throw new E;` its one
    /// opline.
    fn throw(&mut self, operand: tree::Expr) -> Result<Where, CompileError> {
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

    /// `zend_mark_function_as_generator` (`Z/Zend/zend_compile.c:1530`):
    /// a `yield` needs a function to be in, and a generator's declared
    /// return type must admit a `Generator`, which a type does where it
    /// takes any object or names `Traversable`, `Iterator` or
    /// `Generator`. Both refusals are `E_COMPILE_ERROR`s at the line the
    /// compiler stands on, the `yield`'s or, from
    /// [`Self::parameters_and_body`], the declaration's.
    ///
    /// ```php
    /// function f(): int { yield 1; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Generator return type must be a supertype of Generator, int given in %s on line 2
    /// ```
    ///
    /// ```php
    /// function f(): iterable { yield 1; }
    /// function g(): Traversable|false { yield 2; }
    /// function h(): ?object { yield 3; }
    /// foreach ([f(), g(), h()] as $gen) foreach ($gen as $v) echo $v;
    /// ```
    /// ```expect
    /// 123
    /// ```
    /// Whether the op array being compiled is a function's, a method's
    /// or a closure's, `CG(active_op_array)->function_name`, and not a
    /// file's or an `eval`'s main one.
    pub(crate) fn in_function(&self) -> bool {
        self.active_function.is_some()
    }

    fn mark_function_as_generator(&mut self) -> Result<(), CompileError> {
        if self.active_function.is_none() {
            return Err(
                self.compile_error("The \"yield\" expression can only be used inside a function")
            );
        }
        let Some(ty) = self.return_type.clone() else {
            return Ok(());
        };
        if generator_compatible(&ty) {
            return Ok(());
        }
        Err(self.compile_error(format!(
            "Generator return type must be a supertype of Generator, {} given",
            php_unit::type_to_string(&ty)
        )))
    }

    /// `zend_compile_yield` (`Z/Zend/zend_compile.c:10716`): the key
    /// where one is written, then the value, then `YIELD` over the two
    /// into a `VAR`, which is where `send()` puts what it sends. Under a
    /// generator written `function &gen()` a variable is compiled for
    /// writing, so what the generator hands out is the variable's own
    /// cell, and a call carries `ZEND_RETURNS_FUNCTION` for the notice
    /// over one that returned by value.
    ///
    /// ```php
    /// function g() { $x = yield 1; echo "got $x\n"; yield 'k' => 2; $y = yield; var_dump($y); return 3; }
    /// $g = g();
    /// var_dump($g->current());
    /// var_dump($g->send('a'));
    /// var_dump($g->key());
    /// $g->next();
    /// var_dump($g->key(), $g->current());
    /// $g->send('b');
    /// var_dump($g->valid(), $g->getReturn());
    /// ```
    /// ```expect
    /// int(1)
    /// got a
    /// int(2)
    /// string(1) "k"
    /// int(1)
    /// NULL
    /// string(1) "b"
    /// bool(false)
    /// int(3)
    /// ```
    ///
    /// `yield` outside a function is php's compile error:
    ///
    /// ```php
    /// yield 1;
    /// ```
    /// ```expect exit=255
    /// Fatal error: The "yield" expression can only be used inside a function in %s on line 2
    /// ```
    fn yield_expr(&mut self, id: NodeId<tree::YieldExpr>) -> Result<Where, CompileError> {
        self.mark_function_as_generator()?;
        if !self.fn_flags.generator {
            return Err(self.internal("a `yield` in a body the scan found none in"));
        }
        let node = self.parsed.tree[id];
        let key = match node.key {
            Some(key) => {
                let key = self.parsed.tree[key].key;
                let value = self.expr(key)?;
                Some(self.operand(value))
            }
            None => None,
        };
        let by_ref = self.returns_by_ref;
        let extended_value = match node.value {
            Some(value) if by_ref && self.is_call(value) => RETURNS_FUNCTION,
            _ => 0,
        };
        let value = match node.value {
            Some(value) if by_ref && self.is_variable(value) => {
                let (kind, at, _) = self.ref_source(value, true)?;
                Some((kind, at))
            }
            Some(value) => {
                let value = self.expr(value)?;
                Some(self.operand(value))
            }
            None => None,
        };
        let (op1_type, op1) = value.unwrap_or((OperandKind::Unused, 0));
        let (op2_type, op2) = key.unwrap_or((OperandKind::Unused, 0));
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::Yield,
            op1_type,
            op1,
            op2_type,
            op2,
            out_type: OperandKind::Var,
            out,
            extended_value,
            ..Opline::default()
        });
        Ok(Where::Returned(out))
    }

    /// `zend_compile_yield_from` (`Z/Zend/zend_compile.c:10751`): the
    /// operand, then `YIELD_FROM` over it into a temporary, which holds
    /// what a generator delegated to returned. A generator written
    /// `function &gen()` may not delegate.
    ///
    /// ```php
    /// function inner() { yield 1; yield 2; return 'r'; }
    /// function outer() { $r = yield from inner(); yield 3; yield from ['a' => 4, 5]; return $r; }
    /// $g = outer();
    /// foreach ($g as $k => $v) echo "$k=$v ";
    /// echo $g->getReturn(), "\n";
    /// ```
    /// ```expect
    /// 0=1 1=2 0=3 a=4 0=5 r
    /// ```
    ///
    /// ```php
    /// function &g() { yield from [1]; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use "yield from" inside a by-reference generator in %s on line 2
    /// ```
    fn yield_from(&mut self, id: NodeId<tree::YieldFromExpr>) -> Result<Where, CompileError> {
        self.mark_function_as_generator()?;
        if !self.fn_flags.generator {
            return Err(self.internal("a `yield` in a body the scan found none in"));
        }
        if self.returns_by_ref {
            return Err(
                self.compile_error("Cannot use \"yield from\" inside a by-reference generator")
            );
        }
        let source = self.required(self.parsed.tree[id].source, "a `yield from`")?;
        let value = self.expr(source)?;
        let (op1_type, op1) = self.operand(value);
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::YieldFrom,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_is_variable` (`Z/Zend/zend_compile.c:2718`): a variable, a
    /// dimension, a property or a `static` property, the nodes a
    /// by-reference `yield` compiles for writing. A variable named by
    /// an expression is left to `zend_compile_expr` here, which reads it.
    fn is_variable(&self, expr: tree::Expr) -> bool {
        match self.peel(expr) {
            tree::Expr::SimpleVarExpr(_)
            | tree::Expr::SubscriptExpr(_)
            | tree::Expr::MemberAccessExpr(_) => true,
            other => self.is_static_prop(other),
        }
    }

    /// `zend_compile_include_or_eval` (`Z/Zend/zend_compile.c:10800`):
    /// the operand, then one `INCLUDE_OR_EVAL` over it into a `VAR`, the
    /// construct's bit ([`IncludeKind::attr`]) in `extended_value`;
    /// `zend_do_free` marks the `VAR` unused where nothing reads it, as
    /// it marks a call's. The engine's handler resolves the path, or
    /// compiles the string for an `eval`, serves the code on a frame of
    /// its own and leaves what it returned in the `VAR`.
    ///
    /// ```php
    /// $n = ($n ?? 0) + 1;
    /// if ($n > 1) { echo "run $n\n"; return $n * 10; }
    /// var_dump(include __FILE__, require 'witness.php');
    /// var_dump(include_once __FILE__, require_once './witness.php');
    /// ```
    /// ```expect
    /// run 2
    /// run 3
    /// int(20)
    /// int(30)
    /// bool(true)
    /// bool(true)
    /// ```
    ///
    /// `eval` is the same opline under `ZEND_EVAL`, its operand the
    /// string, whatever expression computes it:
    ///
    /// ```php
    /// $a = 1;
    /// var_dump(eval('return $a + 1;'), eval('$b = ' . '2;'), $b);
    /// ```
    /// ```expect
    /// int(2)
    /// NULL
    /// int(2)
    /// ```
    fn include_or_eval(
        &mut self,
        kind: IncludeKind,
        path: tree::Expr,
    ) -> Result<Where, CompileError> {
        self.ext_fcall(Opcode::ExtFcallBegin);
        let value = self.expr(path)?;
        let (op1_type, op1) = self.operand(value);
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::IncludeOrEval,
            op1_type,
            op1,
            out_type: OperandKind::Var,
            out,
            extended_value: kind.attr(),
            ..Opline::default()
        });
        self.ext_fcall(Opcode::ExtFcallEnd);
        Ok(Where::Returned(out))
    }

    /// `zend_compile_conditional` (`Z/Zend/zend_compile.c:10505`): the
    /// condition, a `JMPZ` to the false arm, the true arm into the one
    /// temporary its `QM_ASSIGN` allocates, a `JMP` past the false arm,
    /// and the false arm's `QM_ASSIGN` into that same temporary, which
    /// is where the operator leaves its value. A condition that is
    /// itself a conditional and stood between no parentheses is php's
    /// compile error, in the one of three sentences that names which of
    /// the two was written with a middle arm; `(a ?: b) ?: c` is the
    /// grouping php lets stand, since it answers what `a ?: (b ?: c)`
    /// answers.
    ///
    /// ```php
    /// echo 1 ? "a" : "b";
    /// echo (1 ? "a" : "b");
    /// echo 0 ? "a" : "b";
    /// echo (1 ? "a" : "b") ? "c" : "d";
    /// echo 1 ? "a" : (0 ? "c" : "d");
    /// ```
    /// ```expect
    /// aabca
    /// ```
    ///
    /// An arrow function captures what every arm reads, since
    /// `find_implicit_binds_recursively` walks a conditional's children
    /// like any other node's:
    ///
    /// ```php
    /// $a = 1; $b = 2; $c = 3;
    /// $f = fn () => $a ? $b : $c;
    /// $g = fn () => $a ?: $c;
    /// var_dump($f(), $g());
    /// ```
    /// ```expect
    /// int(2)
    /// int(1)
    /// ```
    ///
    /// The refusal stands while the file compiles, so nothing of it runs:
    ///
    /// ```php
    /// echo "before\n";
    /// echo 1 ? "a" : "b" ? "c" : "d";
    /// ```
    /// ```expect exit=255
    /// Fatal error: Unparenthesized `a ? b : c ? d : e` is not supported. Use either `(a ? b : c) ? d : e` or `a ? b : (c ? d : e)` in %s on line 3
    /// ```
    fn conditional(
        &mut self,
        cond: tree::Expr,
        then: Option<tree::Expr>,
        otherwise: Option<tree::Expr>,
    ) -> Result<Where, CompileError> {
        // `ZEND_PARENTHESIZED_CONDITIONAL` is the bit php's grammar
        // puts on a conditional a `(` and a `)` stand around, and
        // `zend_compile_conditional` reads it on its own left child to
        // tell an unparenthesised nested conditional, php's compile
        // error, from the grouping that is written out. Here the
        // parentheses are the `ParenExpr` between the two nodes, so a
        // child that is a `TernaryExpr` itself is php's unparenthesised
        // one and a child under parentheses is not.
        if let tree::Expr::TernaryExpr(id) = cond {
            let nested = self.parsed.tree[id].then;
            match (nested.is_some(), then.is_some()) {
                (true, true) => return Err(self.compile_error(
                    "Unparenthesized `a ? b : c ? d : e` is not supported. Use either `(a ? b : c) ? d : e` or `a ? b : (c ? d : e)`",
                )),
                (true, false) => return Err(self.compile_error(
                    "Unparenthesized `a ? b : c ?: d` is not supported. Use either `(a ? b : c) ?: d` or `a ? b : (c ?: d)`",
                )),
                (false, true) => return Err(self.compile_error(
                    "Unparenthesized `a ?: b ? c : d` is not supported. Use either `(a ?: b) ? c : d` or `a ?: (b ? c : d)`",
                )),
                (false, false) => {}
            }
        }
        let Some(otherwise) = otherwise else {
            return Err(self.unread("a `?`"));
        };
        let Some(then) = then else {
            return self.shorthand_conditional(cond, otherwise);
        };
        let tested = self.expr(cond)?;
        let opnum_jmpz = self.cond_jump_forward(Opcode::Jmpz, tested);
        let taken = self.expr(then)?;
        let (op1_type, op1) = self.operand(taken);
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::QmAssign,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        let opnum_jmp = self.jump_forward();
        self.update_jump_target_to_next(opnum_jmpz);
        let untaken = self.expr(otherwise)?;
        let (op1_type, op1) = self.operand(untaken);
        self.push(Opline {
            opcode: Opcode::QmAssign,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        self.update_jump_target_to_next(opnum_jmp);
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_coalesce` (`Z/Zend/zend_compile.c:10565`),
    /// `a ?? b`: the left side compiled under `BP_VAR_IS`
    /// ([`Self::silent_read`]), then `COALESCE` over it into a
    /// temporary with the jump target learnt after the right side, then
    /// the right side's `QM_ASSIGN` into that same temporary. php
    /// allocates no temporary for the `QM_ASSIGN`, which
    /// `zend_emit_op_tmp` does only where it is handed a `znode` to fill,
    /// and writes the coalesce's own into the opline instead
    /// (`SET_NODE(opline->result, ...)`).
    ///
    /// `zend_eval_const_expr` folds neither side, so `1 ?? 2` is the
    /// two oplines any other pair is.
    ///
    /// A call of its own, so what it takes is off the stack of a
    /// left-nested chain of the other operators (#3159).
    #[inline(never)]
    fn coalesce(&mut self, left: tree::Expr, right: tree::Expr) -> Result<Where, CompileError> {
        let held = self.silent_read(left)?;
        let (op1_type, op1) = self.operand(held);
        let out = self.temporary();
        let opnum = self.push(Opline {
            opcode: Opcode::Coalesce,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        let otherwise = self.expr(right)?;
        let (op1_type, op1) = self.operand(otherwise);
        self.push(Opline {
            opcode: Opcode::QmAssign,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        self.update_jump_target_to_next(opnum);
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_short_circuiting` (`Z/Zend/zend_compile.c:10350`),
    /// `a && b` and `a || b` and their word spellings: the left side
    /// compiled, then `JMPZ_EX` for `&&` or `JMPNZ_EX` for `||` into the
    /// operator's temporary with the jump target learnt after the right
    /// side, then the right side's `BOOL` into that same temporary. The
    /// temporary is the left side's own where the left side left one
    /// (`SET_NODE(opline_jmpz->result, &left_node)`), so a chain of the
    /// same operator writes one temporary however long it is, and a new
    /// one (`zend_make_tmp_result`) where the left side is a CV, a
    /// constant or a call's `VAR`.
    ///
    /// A constant left side is folded where php folds it: the operator
    /// answers the left side's own truth and the right side is never
    /// compiled where that truth settles it (`0 && f()`), and where it
    /// does not, the right side alone is the answer, as the constant it
    /// folds to or as one `BOOL` over it.
    ///
    /// Each constant side is destroyed once the operator stands
    /// (`zval_ptr_dtor` of its `u.constant`), so the string it was
    /// written as is given back ([`Self::side_folded`]): the left one's
    /// claim is taken where its compile left it on top, before the right
    /// side compiles, and given back last, as php frees it.
    ///
    /// A call of its own, so what it takes is off the stack of a
    /// left-nested chain of the other operators (#3159).
    #[inline(never)]
    fn short_circuiting(
        &mut self,
        or: bool,
        left: tree::Expr,
        right: tree::Expr,
    ) -> Result<Where, CompileError> {
        let side = left;
        let left = self.expr(side)?;
        let held = match left {
            Where::Constant(_) => self.claims.take_top(self.peel(side)),
            _ => None,
        };
        let answer = self.short_circuit_from(or, left, |emit| emit.expr(right));
        // A constant answer over a right side that compiled is the
        // right side's fold; one the left side settled compiled none,
        // so no claim of the right side's stands to give back.
        if let Ok(Where::Constant(_)) = answer {
            self.side_folded(right);
        }
        if let Some(made) = held {
            self.strings.give_back(made);
        }
        answer
    }

    /// [`Self::short_circuiting`] once its left side stands compiled,
    /// the right side compiled by `right` where the left side does not
    /// settle it: what `isset($a, $b)` shares with `&&`, since php's
    /// parser makes it the `ZEND_AST_AND` of one `isset` per variable
    /// (`isset_variables`). The first unset variable settles it and the
    /// ones after are never asked:
    ///
    /// ```php
    /// $a = 1; $b = null; $c = [0];
    /// class O { function __isset($n) { echo "asked $n\n"; return true; } }
    /// $o = new O;
    /// var_dump(isset($a, $c[0]), isset($a, $b, $o->p), isset($o->p, $a, $o->q));
    /// var_dump(isset($this, $a), isset($a, $c, $a));
    /// ```
    /// ```expect
    /// asked p
    /// asked q
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// ```
    ///
    /// Each constant side is read through `zend_is_true`
    /// ([`Self::is_true`]) as often as php reads it, so a `NAN` warns
    /// while the file compiles: once on the left of an `&&`, which it
    /// does not settle, twice on the left of an `||`, which it settles,
    /// and once on a right side the left one leaves the answer to:
    ///
    /// ```php
    /// echo "start\n";
    /// $x = 1;
    /// var_dump(NAN && $x);
    /// var_dump(NAN || $x);
    /// var_dump(0 || NAN, 1 || NAN, 0 && NAN);
    /// ```
    /// ```expect
    /// Warning: unexpected NAN value was coerced to bool in %SCRATCH%/witness.php on line 4
    ///
    /// Warning: unexpected NAN value was coerced to bool in %SCRATCH%/witness.php on line 5
    ///
    /// Warning: unexpected NAN value was coerced to bool in %SCRATCH%/witness.php on line 5
    ///
    /// Warning: unexpected NAN value was coerced to bool in %SCRATCH%/witness.php on line 6
    /// start
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// ```
    fn short_circuit_from(
        &mut self,
        or: bool,
        left: Where,
        right: impl FnOnce(&mut Self) -> Result<Where, CompileError>,
    ) -> Result<Where, CompileError> {
        if let Where::Constant(value) = left {
            // php asks `zend_is_true` of a settling left side twice, once
            // to test it and once for `ZVAL_BOOL`.
            let held = Truth::of(value);
            if self.is_true(held) == or {
                return Ok(Where::Constant(Zval::bool(self.is_true(held))));
            }
            let right = right(self)?;
            if let Where::Constant(value) = right {
                return Ok(Where::Constant(Zval::bool(self.is_true(Truth::of(value)))));
            }
            return Ok(self.emit_unary(Opcode::Bool, right, 0));
        }
        let (op1_type, op1) = self.operand(left);
        let out = match left {
            Where::Temporary(at) => at,
            _ => self.temporary(),
        };
        let opnum = self.push(Opline {
            opcode: if or { Opcode::JmpnzEx } else { Opcode::JmpzEx },
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        let otherwise = right(self)?;
        let (op1_type, op1) = self.operand(otherwise);
        self.push(Opline {
            opcode: Opcode::Bool,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        self.update_jump_target_to_next(opnum);
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_var` under `BP_VAR_IS`
    /// (`Z/Zend/zend_compile.c:12009`), the read that raises nothing
    /// where what it reads is not there. A `ZEND_AST_VAR` is the CV
    /// itself, whatever the type, fetched under `BP_VAR_IS`, which
    /// raises no deprecation over `$http_response_header`; a call, the
    /// chain's own or one under it, is compiled under `BP_VAR_IS` too,
    /// which keeps it from the frameless calls php compiles only for a
    /// read; and everything else is `zend_compile_expr`, which is what a
    /// number as written or an operator is. The three nodes with a
    /// fetch of their own are `FETCH_DIM_IS`, `FETCH_OBJ_IS` and
    /// `FETCH_STATIC_PROP_IS`, one per link of the chain.
    ///
    /// ```php
    /// var_dump($http_response_header ?? 1);
    /// var_dump(isset($http_response_header[0]));
    /// var_dump($http_response_header[0] ?? 2);
    /// var_dump(empty($http_response_header));
    /// var_dump(str_contains("x", "y")["a"] ?? 3);
    /// echo "--\n";
    /// var_dump(@$http_response_header);
    /// ```
    /// ```expect
    /// Deprecated: The predefined locally scoped $http_response_header variable is deprecated, call http_get_last_response_headers() instead in %s on line 8
    /// int(1)
    /// bool(false)
    /// int(2)
    /// bool(true)
    /// int(3)
    /// --
    /// NULL
    /// ```
    ///
    /// php holds each fetch back while the rest of the chain compiles
    /// (`zend_delayed_compile_var`) and flushes them in written order
    /// after it, so `$a[f()][g()] ?? 1` calls both natives and then
    /// fetches twice. The links are collected here and pushed at the
    /// end for the same reason; each link's operands are settled where
    /// the link is made, since that is where `SET_NODE` puts a
    /// constant into the table.
    fn silent_read(&mut self, expr: tree::Expr) -> Result<Where, CompileError> {
        // A variable fetched by name, one named at run time or an
        // auto-global, is `FETCH_IS` by its name.
        if self.is_by_name(expr) {
            let mut delayed = Vec::new();
            return match self.silent_under(expr, &mut delayed, Place::Outermost)? {
                Under::Value(value) => Ok(value),
                Under::Link => match self.lay_silent(&mut delayed, None) {
                    Some(out) => Ok(Where::Temporary(out)),
                    None => Err(self.fatal("a `??` over a name with no fetch")),
                },
            };
        }
        if !self.is_fetch(expr) {
            return self.expr_under(expr, Fetch::Is);
        }
        let checkpoint = self.short_circuits.len();
        let mut delayed = Vec::new();
        self.silent_link(expr, &mut delayed, Place::Outermost)?;
        let Some(out) = self.lay_silent(&mut delayed, None) else {
            return Err(self.fatal("a `??` over a chain with no link"));
        };
        let value = Where::Temporary(out);
        self.short_circuit_commit(checkpoint, value, chain::ChainEnd::Expr);
        Ok(value)
    }

    /// The links held back so far laid down in written order, each
    /// reading the one before it, `last` the temporary a link laid down
    /// earlier left; the temporary the last of them writes.
    fn lay_silent(&mut self, delayed: &mut Vec<IsLink>, mut last: Option<u32>) -> Option<u32> {
        for link in delayed.drain(..) {
            let (op1_type, op1) = link
                .op1
                .or_else(|| last.map(|out| (OperandKind::Tmp, out)))
                .unwrap_or((OperandKind::Unused, 0));
            self.push(Opline {
                opcode: link.opcode,
                op1_type,
                op1,
                op2_type: link.op2_type,
                op2: link.op2,
                out_type: OperandKind::Tmp,
                out: link.out,
                extended_value: link.extended_value,
                ..Opline::default()
            });
            last = Some(link.out);
        }
        last
    }

    /// One link of a `BP_VAR_IS` chain and every link under it, each
    /// pushed onto `delayed` in written order: `zend_delayed_compile_dim`,
    /// `zend_delayed_compile_prop` and `zend_delayed_compile_static_prop`,
    /// which each compile what stands under them, then the dimension or
    /// the property's name, and only then settle the fetch's operands.
    fn silent_link(
        &mut self,
        expr: tree::Expr,
        delayed: &mut Vec<IsLink>,
        place: Place,
    ) -> Result<(), CompileError> {
        let line = self.line_of(expr);
        match self.peel(expr) {
            tree::Expr::SubscriptExpr(id) => {
                let node = &self.parsed.tree[id];
                let (container, index) = (node.base, node.index);
                let under = self.silent_under(container, delayed, Place::Under)?;
                let Some(index) = index else {
                    return Err(self.compile_error("Cannot use [] for reading"));
                };
                let value = self.expr(index)?;
                let op1 = self.settled(&under);
                let (op2_type, op2) = self.dim_operand(value);
                let out = self.temporary();
                delayed.push(IsLink {
                    opcode: Opcode::FetchDimIs,
                    op1,
                    op2_type,
                    op2,
                    out,
                    extended_value: 0,
                    held: self.held_back(),
                    flushed: Vec::new(),
                });
                Ok(())
            }
            tree::Expr::MemberAccessExpr(id) => {
                let name = self.member_of(id)?;
                let object = self.parsed.tree[id].obj;
                // `zend_delayed_compile_prop`'s `is_this_fetch` arm:
                // `$this` inside a method is the `IS_UNUSED` receiver, and
                // outside one `FETCH_THIS` into a `VAR`, neither of which a
                // `?->` stands over. Under a dimension `$this` is no
                // receiver but `zend_compile_simple_var`'s temporary
                // ([`Self::silent_under`]).
                let mut flushed = Vec::new();
                let op1 = if self.is_this(object) {
                    Some(self.receiver_of(object)?)
                } else {
                    let mut under = self.silent_under(object, delayed, Place::Under)?;
                    // A `?->`: the links under it laid down now and the
                    // `JMP_NULL` over what they left, as
                    // `zend_delayed_compile_prop` flushes them, each a `NOP`
                    // the stack holds until the chain ends.
                    if self.is_nullsafe(id) {
                        let pair = match under {
                            Under::Link => {
                                for link in delayed.iter_mut() {
                                    flushed.push(std::mem::take(&mut link.held));
                                    flushed.append(&mut link.flushed);
                                }
                                let out = self.lay_silent(delayed, None).unwrap_or_default();
                                under = Under::Value(Where::Temporary(out));
                                (OperandKind::Tmp, out)
                            }
                            Under::Value(value) => self.operand(value),
                        };
                        self.jmp_null(pair, true);
                    }
                    self.settled(&under)
                };
                let name = self.prop_name(name)?;
                let out = self.temporary();
                delayed.push(IsLink {
                    opcode: Opcode::FetchObjIs,
                    op1,
                    op2_type: name.kind,
                    op2: name.at,
                    out,
                    extended_value: name.slots,
                    held: self.held_back(),
                    flushed,
                });
                Ok(())
            }
            // `zend_compile_static_prop` names the property in `op1` and
            // the class in `op2`, so the link stands under nothing and
            // is always the first of its chain.
            tree::Expr::ScopedAccessExpr(id) => {
                let Scoped::Property { class, name } = self.scoped_of(id, line)? else {
                    return Err(self.fatal("a `??` over a class constant, which is no variable"));
                };
                let StaticOperands {
                    name,
                    class: (op2_type, op2),
                    slots: extended_value,
                } = self.static_operands(class, &name)?;
                let out = self.temporary();
                // `zend_compile_static_prop` holds it back only under
                // another link, where `zend_delayed_compile_var` compiles it.
                let held = match place {
                    Place::Under => self.held_back(),
                    Place::Outermost => HeldBack::default(),
                };
                delayed.push(IsLink {
                    opcode: Opcode::FetchStaticPropIs,
                    op1: Some(name),
                    op2_type,
                    op2,
                    out,
                    extended_value,
                    held,
                    flushed: Vec::new(),
                });
                Ok(())
            }
            other => Err(self.fatal(format!("a `??` over {other:?}, which is no variable node"))),
        }
    }

    /// Whether a node is one of the three php delays under
    /// `BP_VAR_IS`, `zend_delayed_compile_var`'s `ZEND_AST_DIM`,
    /// `ZEND_AST_PROP` and `ZEND_AST_STATIC_PROP`; a class constant
    /// after a `::` is none of them.
    fn is_fetch(&self, expr: tree::Expr) -> bool {
        match self.peel(expr) {
            tree::Expr::SubscriptExpr(_) | tree::Expr::MemberAccessExpr(_) => true,
            tree::Expr::ScopedAccessExpr(id) => self.names_property(id),
            _ => false,
        }
    }

    /// What stands under one link: the link before it where the node is
    /// one of the three php delays, and otherwise the value
    /// `zend_compile_expr` leaves, which is the read that warns as php
    /// reads it there. `$this` under a dimension is that value,
    /// `zend_compile_simple_var`'s `FETCH_THIS` into a temporary, as
    /// `ISSET_ISEMPTY_DIM_OBJ` and `FETCH_DIM_IS` list no `UNUSED`
    /// container; the receiver of a property is the link's own
    /// ([`Self::silent_link`]).
    fn silent_under(
        &mut self,
        expr: tree::Expr,
        delayed: &mut Vec<IsLink>,
        place: Place,
    ) -> Result<Under, CompileError> {
        if self.is_by_name(expr) {
            return Ok(match self.delayed_named(expr, Fetch::Is)? {
                Some(Base::ByName {
                    name, table, out, ..
                }) => {
                    // Held back only under another link, as
                    // `zend_delayed_compile_var` compiles it with `delayed`.
                    let held = match place {
                        Place::Under => self.held_back(),
                        Place::Outermost => HeldBack::default(),
                    };
                    delayed.push(IsLink {
                        opcode: Opcode::FetchIs,
                        op1: Some(name),
                        op2_type: OperandKind::Unused,
                        op2: 0,
                        out,
                        extended_value: table.fetch_type(),
                        held,
                        flushed: Vec::new(),
                    });
                    Under::Link
                }
                Some(Base::Settled((OperandKind::Cv, at))) => Under::Value(Where::Cv(at)),
                Some(Base::Settled((_, at))) => Under::Value(Where::Temporary(at)),
                _ => return Err(self.fatal("a `??` over a name that is no variable")),
            });
        }
        if self.is_fetch(expr) {
            self.silent_link(expr, delayed, place)?;
            return Ok(Under::Link);
        }
        Ok(Under::Value(self.chain_link_under(expr, Fetch::Is)?))
    }

    /// The `op1` a link carries: nothing where it reads the link under
    /// it, and the operand pair otherwise, settled here because this is
    /// where `SET_NODE` would add a constant to the table.
    fn settled(&mut self, under: &Under) -> Option<(OperandKind, u32)> {
        match *under {
            Under::Link => None,
            Under::Value(value) => Some(self.operand(value)),
        }
    }

    /// `zend_compile_shorthand_conditional`
    /// (`Z/Zend/zend_compile.c:10480`), `a ?: c`: the left operand
    /// compiled once, a `JMP_SET` that leaves it in the temporary and
    /// jumps past the rest when it is true, and the right operand's
    /// `QM_ASSIGN` into the same temporary otherwise. The left operand
    /// is served once, which is what the form is for.
    ///
    /// ```php
    /// function f() { echo "f"; return 0; }
    /// echo f() ?: "b";
    /// echo 1 ?: "b";
    /// ```
    /// ```expect
    /// fb1
    /// ```
    fn shorthand_conditional(
        &mut self,
        cond: tree::Expr,
        otherwise: tree::Expr,
    ) -> Result<Where, CompileError> {
        let tested = self.expr(cond)?;
        let (op1_type, op1) = self.operand(tested);
        let out = self.temporary();
        let opnum_jmp_set = self.push(Opline {
            opcode: Opcode::JmpSet,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        let untaken = self.expr(otherwise)?;
        let (op1_type, op1) = self.operand(untaken);
        self.push(Opline {
            opcode: Opcode::QmAssign,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        self.update_jump_target_to_next(opnum_jmp_set);
        Ok(Where::Temporary(out))
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

    /// `zend_compile_compound_assign`, the `ZEND_AST_VAR` arm
    /// (`Z/Zend/zend_compile.c:3634`): the variable's CV looked up first
    /// (`zend_delayed_compile_var` under `BP_VAR_RW`, which delays
    /// nothing for a plain CV), then the value, then one `ASSIGN_OP`
    /// over the two into a temporary (`zend_emit_op_tmp`) with the
    /// binary operator's own opcode in `extended_value`. The temporary
    /// is numbered whether or not anything reads it, as [`Self::assign`]
    /// numbers its own.
    ///
    /// php sets `CG(zend_lineno)` back to the target's line for an
    /// `ASSIGN` and does not for this node, so the opline carries the
    /// line the value ended on: `$a\n+=\n1;` is line 3 and its warning
    /// for an undefined `$a` names line 3.
    fn assign_op(
        &mut self,
        var: &[u8],
        op: BinaryOp,
        value: tree::Expr,
    ) -> Result<Where, CompileError> {
        let cv = self.lookup_cv(var);
        let value = self.expr(value)?;
        let (op2_type, op2) = self.operand(value);
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::AssignOp,
            op1_type: OperandKind::Cv,
            op1: cv,
            op2_type,
            op2,
            out_type: OperandKind::Tmp,
            out,
            extended_value: u32::from(Self::opcode_of(op) as u8),
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_assign`, the `ZEND_AST_VAR` arm: the variable's CV
    /// looked up first (`zend_delayed_compile_var`, which delays nothing
    /// for a plain CV), then the value, then `ASSIGN` into a temporary
    /// (`zend_emit_op_tmp`). The temporary is numbered whether or not
    /// anything reads it; [`Self::do_free`] marks it `UNUSED` when
    /// nothing does, which is what the engine's `ASSIGN` reads as
    /// `RETURN_VALUE_USED`.
    fn assign(&mut self, var: &[u8], value: Source) -> Result<Where, CompileError> {
        // `CG(zend_lineno) = zend_ast_get_lineno(var_ast)` before the
        // `ASSIGN` (`Z/Zend/zend_compile.c:3474`): the target's line,
        // which is this node's, and not the line its value ended on.
        let line = self.line;
        let cv = self.lookup_cv(var);
        self.cv_fetched(var, Fetch::Write);
        let value = self.source(value)?;
        let (op2_type, op2) = self.operand(value);
        let out = self.temporary();
        self.line = line;
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

    /// `zend_compile_var(…, BP_VAR_W, 1)` over one of the fetches php
    /// delays — a dimension, a property and a `static` property — what a
    /// by-reference assignment's source and a by-reference return's
    /// operand are: the chain [`Self::delayed_var`] held back, laid
    /// straight back down ([`Self::base_fetch`]) with nothing over its
    /// last opline but the `&` itself, so that one carries
    /// `ZEND_FETCH_DIM_REF` or `ZEND_FETCH_REF` where a plain write
    /// carries the dimension's or the property's own bit. The `VAR` it
    /// left is the operand.
    fn var_for_reference(
        &mut self,
        target: tree::Expr,
    ) -> Result<(OperandKind, u32), CompileError> {
        let held = self.delayed_var(target, Fetch::Write)?;
        Ok(self.base_fetch(held, Fetch::Write, Over::Ref))
    }

    /// `zend_compile_var(…, BP_VAR_W, 1)`, a variable fetched by
    /// reference, what the right of a `= &` and a `return` by reference
    /// compile: a plain variable is its CV; a dimension, a property, a
    /// `static` property and a variable the program names are the `_W`
    /// fetch of the whole chain with the `&`'s own bit on its last opline
    /// ([`Self::var_for_reference`]); `$this` is `FETCH_THIS` into a
    /// `VAR` (`zend_compile_simple_var`'s `is_this_fetch` arm, whose
    /// opline writes a `VAR` and not a temporary under `BP_VAR_W`), and
    /// `$GLOBALS` its `FETCH_GLOBALS` into a `VAR` the same way; a call
    /// is the `VAR` it returned into; and everything else is
    /// `zend_compile_var_inner`'s default arm, php's
    /// `Cannot use temporary expression in write context`.
    fn var_by_ref(&mut self, node: tree::Expr) -> Result<(OperandKind, u32), CompileError> {
        let line = self.line;
        Ok(match self.peel(node) {
            node if self.is_by_name(node) => self.var_for_reference(node)?,
            tree::Expr::SimpleVarExpr(id) => match self.simple_var_of(id, line)? {
                SimpleVar::Cv(name) => {
                    self.cv_fetched(name, Fetch::Write);
                    (OperandKind::Cv, self.lookup_cv(name))
                }
                SimpleVar::This => {
                    let this = self.fetch_this(OperandKind::Var);
                    self.operand(this)
                }
                SimpleVar::Globals => {
                    let globals = self.fetch_globals(OperandKind::Var);
                    self.operand(globals)
                }
                SimpleVar::Auto(_) => return Err(self.fatal("an auto-global not fetched by name")),
            },
            node @ (tree::Expr::SubscriptExpr(_)
            | tree::Expr::MemberAccessExpr(_)
            | tree::Expr::VarVarExpr(_)
            | tree::Expr::DynamicVarExpr(_)) => self.var_for_reference(node)?,
            node @ tree::Expr::ScopedAccessExpr(id)
                if matches!(self.scoped_of(id, line)?, Scoped::Property { .. }) =>
            {
                self.var_for_reference(node)?
            }
            node @ tree::Expr::CallExpr(_) => {
                let returned = self.expr_under(node, Fetch::Write)?;
                self.operand(returned)
            }
            _ => {
                return Err(self.compile_error("Cannot use temporary expression in write context"));
            }
        })
    }

    /// `zend_compile_var(&source_node, source_ast, BP_VAR_W, 1)` and the
    /// `MAKE_REF` after it, what stands on the right of a `= &`
    /// (`zend_compile_assign_ref`, `Z/Zend/zend_compile.c:3568`): the
    /// variable fetched by reference ([`Self::var_by_ref`]), after
    /// `zend_assert_not_short_circuited`.
    ///
    /// `MAKE_REF` stands over what that left where the target is not a
    /// plain `$name` and the source is no CV, php's fix for bug #71539:
    /// both sides may modify one structure and the place the left side
    /// named can dangle while the right side runs, so the right side is
    /// made a reference before the left side is fetched at all. That is
    /// why `$a[1] = &$a[0]` fetches the source, makes the reference and
    /// only then fetches the target.
    ///
    /// `Cannot use result of built-in function in write context` is
    /// php's arm after it, over a call it answered in a temporary. php
    /// reads the node `MAKE_REF` has already overwritten, so only a
    /// plain `$name` on the left reaches that error: `$a = &count($x)`
    /// is refused where `$a[0] = &count($x)` compiles to a `MAKE_REF`
    /// over a temporary, which is an opcode php has no handler for and
    /// runs into `Invalid opcode 140/2/0`. The copy is php's, quirk and
    /// all, because the dump is held to it byte for byte.
    fn ref_source(
        &mut self,
        source: tree::Expr,
        plain_target: bool,
    ) -> Result<(OperandKind, u32, u32), CompileError> {
        self.check_stack()?;
        let node = self.peel(source);
        // `zend_assert_not_short_circuited`.
        if self.is_short_circuited(node) {
            return Err(self.compile_error("Cannot take reference of a nullsafe chain"));
        }
        let (kind, at) = self.var_by_ref(node)?;
        // Bug #71539: `zend_emit_op(&source_node, ZEND_MAKE_REF,
        // &source_node, NULL)`, and `source_node` is the `VAR` it wrote
        // from here on, which is the node the arm below reads.
        let (kind, at) = if plain_target || kind == OperandKind::Cv {
            (kind, at)
        } else {
            let out = self.temporary();
            self.push(Opline {
                opcode: Opcode::MakeRef,
                op1_type: kind,
                op1: at,
                out_type: OperandKind::Var,
                out,
                ..Opline::default()
            });
            (OperandKind::Var, out)
        };
        let call = self.is_call(source);
        if kind != OperandKind::Var && call {
            return Err(
                self.compile_error("Cannot use result of built-in function in write context")
            );
        }
        // `ZEND_RETURNS_FUNCTION` where the source is a call
        // (`zend_is_call`), the bit the handler reads to raise php's
        // notice over a call that returned by value.
        Ok((kind, at, if call { RETURNS_FUNCTION } else { 0 }))
    }

    /// [`Self::ref_source`] over a [`Source`]: a value the compiler
    /// already holds, php's `ZEND_AST_ZNODE`, is its own operand, takes no
    /// `MAKE_REF` (`source_ast->kind != ZEND_AST_ZNODE`) and is no call.
    fn ref_source_of(
        &mut self,
        source: Source,
        plain_target: bool,
    ) -> Result<(OperandKind, u32, u32), CompileError> {
        match source {
            Source::Expr(source) => self.ref_source(source, plain_target),
            Source::Node(value) => {
                let (kind, at) = self.operand(value);
                Ok((kind, at, 0))
            }
        }
    }

    /// `zend_compile_assign_ref` (`Z/Zend/zend_compile.c:3549`),
    /// `$b = &$a`: the target compiled first and held back
    /// (`zend_delayed_compile_var` under `BP_VAR_W` with `by_ref`, which
    /// holds back a dimension's fetch and delays nothing for a plain
    /// CV), the source compiled after it where it stands
    /// ([`Self::ref_source`], php's `zend_compile_var` under the same
    /// pair), then the target's own fetch laid down
    /// (`zend_delayed_compile_end`) and `ASSIGN_REF` over the two into a
    /// `VAR` (`zend_emit_op`), `extended_value` the source's
    /// `ZEND_RETURNS_FUNCTION` or nothing. That order is why
    /// `$a[1] = &$a[0]` fetches `$a[0]` first, makes the reference, and
    /// only then fetches `$a[1]`: the right side may move what the left
    /// side named. [`Self::do_free`] marks the `VAR` `UNUSED` where
    /// nothing reads it, as it marks a call's.
    ///
    /// A dimension as the target, `$a[] = &$x`, is the whole chain
    /// fetched for writing, the last link carried by `ZEND_FETCH_DIM_REF`
    /// where a plain write carries nothing, and `ASSIGN_REF` over the
    /// `VAR` it left: php turns the last delayed opline into an
    /// `ASSIGN_*_REF` only for a property and a class's own property,
    /// and leaves a dimension's fetch standing. A property is
    /// `ASSIGN_OBJ_REF` over its object, the source in an `OP_DATA`.
    ///
    /// One dimension bound to another of the same array: the source is
    /// fetched for writing, so what both names hold is one cell and a
    /// write through either is read through both.
    ///
    /// ```php
    /// $a[0] = 1; $a[1] = &$a[0]; $a[1] = 2; var_dump($a);
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   &int(2)
    ///   [1]=>
    ///   &int(2)
    /// }
    /// ```
    ///
    /// A property and a `static` property on the right are the same
    /// fetch under `ZEND_FETCH_REF`, which binds the property's own
    /// cell rather than a copy of what it holds.
    ///
    /// ```php
    /// class C { public $p = 1; public static $s = 3; }
    /// $o = new C;
    /// $x = &$o->p; $x = 2;
    /// $y = &C::$s; $y = 4;
    /// var_dump($o->p, C::$s);
    /// ```
    /// ```expect
    /// int(2)
    /// int(4)
    /// ```
    fn assign_ref(&mut self, target: tree::Expr, source: Source) -> Result<Where, CompileError> {
        let line = self.line;
        let target = self.peel(target);
        let decoded = self.target(target)?;
        if let Source::Expr(source) = source {
            if self.is_short_circuited(source) {
                return Err(self.compile_error("Cannot take reference of a nullsafe chain"));
            }
            if self.is_globals(source) {
                return Err(self.compile_error("Cannot acquire reference to $GLOBALS"));
            }
        }
        // `zend_delayed_compile_var(&target_node, target_ast, BP_VAR_W, 1)`:
        // a plain CV settles where it stands and a dimension's fetches
        // wait for `base_fetch` below, after the source has compiled.
        let held = match decoded {
            Target::StaticProp { class, name } => {
                return self.assign_static_prop_ref(class, &name, source);
            }
            Target::Cv(name) => Base::Settled((OperandKind::Cv, self.lookup_cv(name))),
            Target::Dim(_) | Target::ByName(_) | Target::Prop { .. } => {
                let mut held = self.delayed_var(target, Fetch::Write)?;
                self.hold_base(&mut held);
                held
            }
            Target::List(_) => {
                return Err(walk::unread(
                    line,
                    "a by-reference assignment to what is no variable",
                ));
            }
        };
        // php's `target_ast->kind != ZEND_AST_VAR ||
        // target_ast->child[0]->kind != ZEND_AST_ZVAL`, the one shape no
        // `MAKE_REF` stands under.
        let plain = matches!(held, Base::Settled((OperandKind::Cv, _)));
        let (op2_type, op2, flags) = self.ref_source_of(source, plain)?;
        // `opline->opcode == ZEND_FETCH_OBJ_W`: the held-back property
        // fetch becomes `ASSIGN_OBJ_REF` over the object's own fetch,
        // `ZEND_FETCH_REF` off and `ZEND_RETURNS_FUNCTION` on, the source
        // in the `OP_DATA` after it, and the fetch's `VAR` what it leaves.
        if let Base::Prop {
            object, name, out, ..
        } = held
        {
            let (op1_type, op1) = self.base_fetch(*object, Fetch::Write, Over::Obj);
            self.push(Opline {
                opcode: Opcode::AssignObjRef,
                op1_type,
                op1,
                op2_type: name.kind,
                op2: name.at,
                out_type: OperandKind::Var,
                out,
                extended_value: name.slots | flags,
                ..Opline::default()
            });
            self.push(Opline {
                opcode: Opcode::OpData,
                op1_type: op2_type,
                op1: op2,
                ..Opline::default()
            });
            return Ok(Where::Returned(out));
        }
        let (op1_type, op1) = self.base_fetch(held, Fetch::Write, Over::Ref);
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::AssignRef,
            op1_type,
            op1,
            op2_type,
            op2,
            out_type: OperandKind::Var,
            out,
            extended_value: flags,
            ..Opline::default()
        });
        Ok(Where::Returned(out))
    }

    /// `zend_compile_assign`, the `ZEND_AST_DIM` arm: the chain of
    /// dimensions compiled delayed (`zend_delayed_compile_dim` under
    /// `BP_VAR_W`), the value after it, the delayed oplines laid down at
    /// `zend_delayed_compile_end` with the last of them turned into
    /// `ASSIGN_DIM` writing a temporary, and `OP_DATA` carrying the value
    /// (`zend_emit_op_data`), which is why the handler steps over two
    /// oplines. Each link but the last is `FETCH_DIM_W` into a `VAR`
    /// carrying `ZEND_FETCH_DIM_DIM`, the bit that names the dimension
    /// under a dimension in php's string-offset error; the `[]` of an
    /// append is an `UNUSED` `op2`, which the dump prints as `NEXT`.
    fn assign_dim(&mut self, target: tree::Expr, value: Source) -> Result<Where, CompileError> {
        let (container, links) = self.dim_chain(target, Fetch::Write)?;
        // `zend_delayed_emit_op` stamped the fetch's line where the
        // fetch stood, and `zend_delayed_compile_end` carries it into
        // the op array unchanged, so the value compiled after it does
        // not move the `ASSIGN_DIM` (`Z/Zend/zend_compile.c:3487`).
        let line = self.line;
        let value = match value {
            Source::Expr(value) => self.assigned_value(target, value)?,
            Source::Node(value) => value,
        };
        let (data_type, data) = self.operand(value);
        let Some((last, fetches)) = links.split_last() else {
            return Err(self.fatal("a dimension assignment with no dimension"));
        };
        let (op1_type, op1) = self.dim_fetches(container, fetches, Fetch::Write, Over::Dim);
        self.line = line;
        self.push(Opline {
            opcode: Opcode::AssignDim,
            op1_type,
            op1,
            op2_type: last.kind,
            op2: last.at,
            out_type: OperandKind::Tmp,
            out: last.out,
            ..Opline::default()
        });
        self.push(Opline {
            opcode: Opcode::OpData,
            op1_type: data_type,
            op1: data,
            ..Opline::default()
        });
        Ok(Where::Temporary(last.out))
    }

    /// `zend_delayed_compile_dim` over a chain of dimensions under
    /// `fetch`: the container, compiled before anything else, and one
    /// link per dimension from the innermost out. Each link's dimension
    /// is compiled where it stands, before the next link and before the
    /// value, and its `VAR` is numbered after it, which is the order
    /// `zend_delayed_emit_op` numbers php's. `[]` with nothing in it is
    /// the append under a write and php's `Cannot use [] for reading`
    /// under a read.
    ///
    /// The container is [`Self::delayed_var`]'s, php's
    /// `zend_delayed_compile_var` over the node under the innermost `[`.
    ///
    /// ```php
    /// var_dump("ab"[0], [1, 2][1], "ab"[9] ?? 'd');
    /// ```
    /// ```expect
    /// string(1) "a"
    /// int(2)
    /// string(1) "d"
    /// ```
    fn dim_chain(
        &mut self,
        target: tree::Expr,
        fetch: Fetch,
    ) -> Result<(Base, Vec<DimLink>), CompileError> {
        let mut dimensions = Vec::new();
        let mut base = self.peel(target);
        // `$GLOBALS['k']` is no dimension but the fetch by name the chain
        // stands on (`zend_delayed_compile_dim`'s first arm).
        while let tree::Expr::SubscriptExpr(id) = base
            && self.global_var_fetch(base).is_none()
        {
            let node = &self.parsed.tree[id];
            dimensions.push(node.index);
            base = self.peel(node.base);
        }
        let mut container = self.delayed_var(base, fetch)?;
        self.hold_base(&mut container);
        let mut links = Vec::with_capacity(dimensions.len());
        for index in dimensions.clone().iter().rev() {
            let (kind, at) = match index {
                Some(index) => {
                    let value = self.expr(*index)?;
                    self.dim_operand(value)
                }
                // `zend_delayed_compile_dim`: the append names no
                // element to read, under `BP_VAR_R` and `BP_VAR_IS` alike.
                None if matches!(fetch, Fetch::Read | Fetch::Is) => {
                    return Err(self.compile_error("Cannot use [] for reading"));
                }
                None if fetch == Fetch::Unset => {
                    return Err(self.compile_error("Cannot use [] for unsetting"));
                }
                None => (OperandKind::Unused, 0),
            };
            let out = self.temporary();
            links.push(DimLink {
                kind,
                at,
                out,
                held: self.held_back(),
            });
        }
        Ok((container, links))
    }

    /// `zend_delayed_emit_op`'s push of one fetch onto
    /// `CG(delayed_oplines_stack)` (`Z/Zend/zend_compile.c:2458`), which
    /// stands there while what this answers lives, and the most the stack
    /// has held taken in. An assignment to a variable a variable names
    /// holds its `FETCH_W` back, which lays the stack's sixteen, 512
    /// bytes, where the plain one keeps a name `$$n` makes none of; and a
    /// chain whose dimension is a chain of its own holds the outer's
    /// fetches beneath the inner's, so ten over ten grow it past sixteen:
    ///
    /// ```php
    /// $m = memory_get_usage(); eval('$n = "zz"; $o = 1;'); $plain = memory_get_usage() - $m;
    /// $m = memory_get_usage(); eval('$n = "zz"; $$n = 1;'); echo memory_get_usage() - $m - $plain, "\n";
    /// $a = 1; $b = 1;
    /// $nested = '$c = $a' . str_repeat('[0]', 10) . '[$b' . str_repeat('[0]', 10) . '];';
    /// $flat = str_pad('$c = $a[0]', strlen($nested) - 1) . ';';
    /// @eval($flat); @eval($flat);
    /// $m = memory_get_usage(); @eval($nested); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 480
    /// 512
    /// ```
    fn held_back(&self) -> HeldBack {
        let stack = &self.delayed;
        let held = stack.held.get().saturating_add(1);
        stack.held.set(held);
        stack.most.set(stack.most.get().max(held));
        HeldBack(Some(Rc::clone(stack)))
    }

    /// A chain's base held back where it is a fetch by name or of a
    /// `static` property: `zend_delayed_compile_var` compiles each with
    /// `delayed`, where `zend_compile_var` at the outermost compiles it in
    /// place and holds nothing.
    fn hold_base(&self, base: &mut Base) {
        if let Base::StaticProp { held, .. } | Base::ByName { held, .. } = base {
            *held = self.held_back();
        }
    }

    /// `zend_delayed_compile_var` (`Z/Zend/zend_compile.c:12019`), the
    /// node a chain of dimensions or of property fetches stands on,
    /// compiled under `fetch`: a plain variable is its CV
    /// (`zend_compile_simple_var` with `delayed`, which lays no opline
    /// down), a dimension and a property are the fetches php holds back,
    /// so their oplines stand after the chain over them has compiled
    /// (`$o->p[f()]` calls `f` before it fetches `p`), and everything
    /// else is `zend_compile_var` where it stands.
    ///
    /// Under a write that last group is `zend_compile_var`'s default
    /// arm, `Cannot use temporary expression in write context`, but for
    /// `$this`, which is `FETCH_THIS` into a `VAR`, and a call, which
    /// takes `SEPARATE` over the `VAR` it returned into
    /// (`zend_separate_if_call_and_write`), so the write lands in a
    /// value of the request's own and not in one the callee still holds;
    /// a call php answered in a temporary has no `VAR` to separate and
    /// is its own compile error.
    ///
    /// php checks the stack here as it does at the top of
    /// `zend_compile_expr`, since a chain of `[` and `->` recurses one
    /// native frame per link.
    #[allow(clippy::too_many_lines)]
    fn delayed_var(&mut self, base: tree::Expr, fetch: Fetch) -> Result<Base, CompileError> {
        self.check_stack()?;
        let line = self.line;
        let base = self.peel(base);
        Ok(match (base, fetch) {
            (tree::Expr::SimpleVarExpr(id), _)
                if matches!(self.simple_var_of(id, line)?, SimpleVar::Cv(_)) =>
            {
                let SimpleVar::Cv(name) = self.simple_var_of(id, line)? else {
                    return Err(self.fatal("a variable that is no CV"));
                };
                self.cv_fetched(name, fetch);
                Base::Settled((OperandKind::Cv, self.lookup_cv(name)))
            }
            // `zend_compile_simple_var` with `delayed` over a name the
            // program computes or an auto-global's: the fetch by name
            // held back.
            _ if self.is_by_name(base) => {
                let Some(held) = self.delayed_named(base, fetch)? else {
                    return Err(self.fatal("a variable by name that is no variable"));
                };
                held
            }
            // `zend_delayed_compile_dim`: the chain's own container and
            // its links, each dimension compiled where it stands and
            // every opline held back for [`Self::base_fetch`].
            (tree::Expr::SubscriptExpr(_), _) => {
                let (container, links) = self.dim_chain(base, fetch)?;
                Base::Dim {
                    container: Box::new(container),
                    links,
                }
            }
            // `zend_delayed_compile_prop`: the object under the `->` is
            // itself `zend_delayed_compile_var`'s, and the fetch is held
            // back, its temporary, its name and its three cache slots
            // taken here and its opline laid down by
            // [`Self::base_fetch`].
            (tree::Expr::MemberAccessExpr(id), _) => {
                let name = self.member_of(id)?;
                let object = self.parsed.tree[id].obj;
                let mut object_held = self.delayed_object(object, fetch)?;
                // `zend_delayed_compile_prop` under a `?->`: what the
                // object's chain held back is laid down now, and the
                // `JMP_NULL` over it stands before the member's own fetch.
                // php turns each into a `NOP` on the stack, which holds
                // it until the chain ends.
                let mut flushed = Vec::new();
                if self.is_nullsafe(id) && !self.is_this(object) {
                    object_held.take_held(&mut flushed);
                    let settled = self.base_fetch(object_held, fetch, Over::Obj);
                    self.jmp_null(settled, fetch == Fetch::Is);
                    object_held = Base::Settled(settled);
                }
                let name = self.prop_name(name)?;
                let out = self.temporary();
                Base::Prop {
                    object: Box::new(object_held),
                    name,
                    out,
                    held: self.held_back(),
                    flushed,
                }
            }
            // `zend_compile_static_prop` with `delayed`, the other
            // fetch php holds back: the name, the class's constant
            // pair and three slots taken here.
            (tree::Expr::ScopedAccessExpr(id), _)
                if matches!(self.scoped_of(id, line)?, Scoped::Property { .. }) =>
            {
                let Scoped::Property { class, name } = self.scoped_of(id, line)? else {
                    return Err(self.fatal("a class constant that is no property"));
                };
                let StaticOperands { name, class, slots } = self.static_operands(class, &name)?;
                let out = self.temporary();
                Base::StaticProp {
                    name,
                    class,
                    out,
                    slots,
                    held: HeldBack::default(),
                }
            }
            // `zend_compile_simple_var`'s `is_this_fetch` arm under
            // `BP_VAR_FUNC_ARG`, which leaves the `VAR` a write's does.
            (_, Fetch::FuncArg) if self.is_this(base) => {
                let this = self.fetch_this(OperandKind::Var);
                Base::Settled(self.operand(this))
            }
            (_, Fetch::Read | Fetch::Is | Fetch::FuncArg) => {
                let value = self.chain_link_under(base, fetch)?;
                Base::Settled(self.operand(value))
            }
            // `zend_separate_if_call_and_write`: a call under a write
            // is `SEPARATE` over the `VAR` it returned into; a call php
            // answered in a temporary has no `VAR` to separate. A pipe
            // is `zend_compile_var_inner`'s default arm below.
            (_, Fetch::Write | Fetch::Rw | Fetch::Unset)
                if matches!(base, tree::Expr::CallExpr(_)) =>
            {
                let returned = self.expr_under(base, fetch)?;
                let Where::Returned(at) = returned else {
                    return Err(self
                        .compile_error("Cannot use result of built-in function in write context"));
                };
                self.push(Opline {
                    opcode: Opcode::Separate,
                    op1_type: OperandKind::Var,
                    op1: at,
                    out_type: OperandKind::Var,
                    out: at,
                    ..Opline::default()
                });
                Base::Settled((OperandKind::Var, at))
            }
            // `zend_compile_simple_var`'s `is_this_fetch` arm under a
            // write: `FETCH_THIS` into a `VAR`, which fails at run time
            // where no object is in scope. The `UNUSED` receiver a
            // property fetch takes is `zend_delayed_compile_prop`'s own
            // arm and stands above this one
            // ([`Self::delayed_object`]).
            (_, Fetch::Write | Fetch::Rw | Fetch::Unset) if self.is_this(base) => {
                let this = self.fetch_this(OperandKind::Var);
                Base::Settled(self.operand(this))
            }
            // The `is_globals_fetch` arm under a write, `$GLOBALS->p = 1`:
            // `FETCH_GLOBALS` into a `VAR`, which the fetch over it finds
            // no object in as the program serves.
            (_, Fetch::Write | Fetch::Rw) if self.is_globals(base) => {
                let globals = self.fetch_globals(OperandKind::Var);
                Base::Settled(self.operand(globals))
            }
            // `zend_compile_var`'s default arm under a write.
            (_, Fetch::Write | Fetch::Rw | Fetch::Unset) => {
                return Err(self.compile_error("Cannot use temporary expression in write context"));
            }
        })
    }

    /// `zend_delayed_compile_prop`'s first step, the object under a
    /// `->`: `$this` inside a method is `is_this_fetch`'s `IS_UNUSED`
    /// receiver, which no opline fetches, and everything else is
    /// [`Self::delayed_var`]'s, so a dimension under a property write
    /// is `FETCH_DIM_W` and not the read's temporary.
    fn delayed_object(&mut self, object: tree::Expr, fetch: Fetch) -> Result<Base, CompileError> {
        if self.is_this(object) {
            return Ok(Base::Settled(self.receiver(object)?));
        }
        let mut base = self.delayed_var(object, fetch)?;
        self.hold_base(&mut base);
        Ok(base)
    }

    /// The fetch oplines of a chain, each the container of the next, and
    /// the operand the opline after them reads: `FETCH_DIM_W` into a
    /// `VAR` carrying `ZEND_FETCH_DIM_DIM` under a write, `FETCH_DIM_R`
    /// into a temporary under a read. What stands over the last of them
    /// is `over`, the compile function that patches it.
    fn dim_fetches(
        &mut self,
        container: Base,
        fetches: &[DimLink],
        fetch: Fetch,
        over: Over,
    ) -> (OperandKind, u32) {
        let (opcode, out_type) = match fetch {
            Fetch::Read => (Opcode::FetchDimR, OperandKind::Tmp),
            Fetch::Write => (Opcode::FetchDimW, OperandKind::Var),
            Fetch::Rw => (Opcode::FetchDimRw, OperandKind::Var),
            Fetch::Is => (Opcode::FetchDimIs, OperandKind::Tmp),
            Fetch::FuncArg => (Opcode::FetchDimFuncArg, OperandKind::Var),
            Fetch::Unset => (Opcode::FetchDimUnset, OperandKind::Var),
        };
        let under = if fetches.is_empty() { over } else { Over::Dim };
        let (mut op1_type, mut op1) = self.base_fetch(container, fetch, under);
        for (at, link) in fetches.iter().enumerate() {
            // Every link but the last has a dimension over it, and the
            // last one has what `over` names; a read patches nothing,
            // since `zend_adjust_for_fetch_type` left `FETCH_DIM_R` and
            // `FETCH_DIM_IS` with no `extended_value` at all.
            let last = at + 1 == fetches.len();
            let extended_value = match (fetch, last.then_some(over)) {
                (Fetch::Read | Fetch::Is, _) => 0,
                (_, Some(Over::Obj)) => FETCH_DIM_OBJ,
                // `zend_delayed_compile_dim`'s `if (by_ref)`, which
                // overwrites what the recursion left: the outermost
                // link of a chain a `&` takes is the reference's own.
                (_, Some(Over::Ref)) => FETCH_DIM_REF,
                (_, _) => FETCH_DIM_DIM,
            };
            self.push(Opline {
                opcode,
                op1_type,
                op1,
                op2_type: link.kind,
                op2: link.at,
                out_type,
                out: link.out,
                extended_value,
                ..Opline::default()
            });
            op1_type = out_type;
            op1 = link.out;
        }
        (op1_type, op1)
    }

    /// The base's own oplines, laid down at `zend_delayed_compile_end`
    /// before the chain's own: nothing where the base settled where it
    /// stood, and the fetches php delayed where it did, innermost first.
    /// Under a write a property's fetch is the `_W` opcode into a `VAR`
    /// carrying `ZEND_FETCH_DIM_WRITE`, the bit
    /// `zend_delayed_compile_dim` adds to the cache slots where a
    /// dimension stands over it; under a read it is the `_R` opcode into
    /// a temporary (`zend_adjust_for_fetch_type`).
    fn base_fetch(&mut self, container: Base, fetch: Fetch, over: Over) -> (OperandKind, u32) {
        let write = matches!(
            fetch,
            Fetch::Write | Fetch::Rw | Fetch::FuncArg | Fetch::Unset
        );
        let out_type = if write {
            OperandKind::Var
        } else {
            OperandKind::Tmp
        };
        // `if (type == BP_VAR_W && (opline->opcode == ZEND_FETCH_STATIC_PROP_W
        // || opline->opcode == ZEND_FETCH_OBJ_W)) opline->extended_value |=
        // ZEND_FETCH_DIM_WRITE`: the bit is `zend_delayed_compile_dim`'s
        // and `BP_VAR_W`'s alone, so a property under a `->` or under a
        // `BP_VAR_RW` chain carries the slots and nothing else.
        // `ZEND_FETCH_REF` is the other half of the pair, put on by
        // `zend_delayed_compile_var`'s `by_ref` and by
        // `zend_compile_static_prop`'s, where nothing but the `&`
        // stands over the fetch.
        let extended_value = |slots: u32| match over {
            Over::Dim if matches!(fetch, Fetch::Write) => slots | FETCH_DIM_WRITE,
            Over::Ref => slots | FETCH_REF,
            Over::Dim | Over::Obj => slots,
        };
        match container {
            Base::Settled(pair) => pair,
            Base::ByName {
                name, table, out, ..
            } => self.lay_fetch_by_name(name, table, out, fetch),
            // `zend_delayed_compile_dim`'s own oplines, every link of
            // the chain laid down here: the one the chain over this
            // base reads is the last of them, and it is the one `over`
            // patches.
            Base::Dim { container, links } => self.dim_fetches(*container, &links, fetch, over),
            Base::Prop {
                object, name, out, ..
            } => {
                let (op1_type, op1) = self.base_fetch(*object, fetch, Over::Obj);
                self.push(Opline {
                    opcode: match fetch {
                        Fetch::Read => Opcode::FetchObjR,
                        Fetch::Write => Opcode::FetchObjW,
                        Fetch::Rw => Opcode::FetchObjRw,
                        Fetch::Is => Opcode::FetchObjIs,
                        Fetch::FuncArg => Opcode::FetchObjFuncArg,
                        Fetch::Unset => Opcode::FetchObjUnset,
                    },
                    op1_type,
                    op1,
                    op2_type: name.kind,
                    op2: name.at,
                    out_type,
                    out,
                    extended_value: extended_value(name.slots),
                    ..Opline::default()
                });
                (out_type, out)
            }
            Base::StaticProp {
                name: (name_type, name_at),
                class: (class_type, class_at),
                out,
                slots,
                ..
            } => {
                self.push(Opline {
                    opcode: match fetch {
                        Fetch::Read => Opcode::FetchStaticPropR,
                        Fetch::Write => Opcode::FetchStaticPropW,
                        Fetch::Rw => Opcode::FetchStaticPropRw,
                        Fetch::Is => Opcode::FetchStaticPropIs,
                        Fetch::FuncArg => Opcode::FetchStaticPropFuncArg,
                        Fetch::Unset => Opcode::FetchStaticPropUnset,
                    },
                    op1_type: name_type,
                    op1: name_at,
                    op2_type: class_type,
                    op2: class_at,
                    out_type,
                    out,
                    extended_value: extended_value(slots),
                    ..Opline::default()
                });
                (out_type, out)
            }
        }
    }

    /// `zend_compile_dim` under `BP_VAR_R`, `$a[k]` where a value is
    /// wanted: the whole chain is `FETCH_DIM_R`, one per link, each
    /// reading the temporary the link under it wrote
    /// (`zend_adjust_for_fetch_type` marks the fetch's out operand a
    /// `TMP` where a write leaves a `VAR`), and the value is the last
    /// one's temporary.
    fn dim_read(&mut self, target: tree::Expr) -> Result<Where, CompileError> {
        let (container, links) = self.dim_chain(target, Fetch::Read)?;
        match self.dim_fetches(container, &links, Fetch::Read, Over::Dim) {
            (OperandKind::Tmp, at) => Ok(Where::Temporary(at)),
            _ => Err(self.fatal("a dimension read with no dimension")),
        }
    }

    /// `zend_handle_numeric_dim` (`Z/Zend/zend_compile.c:2788`): a
    /// constant dimension whose string is a canonical decimal integer is
    /// that integer, so `$a["10"]` and `$a[10]` are one key and the
    /// handler needs no numeric test over a constant. php keeps the
    /// string in a second slot right after it, marked
    /// `Z_EXTRA = ZEND_EXTRA_VALUE`, which a dimension over an
    /// `ArrayAccess` object reads instead of the long (bug #63217); here
    /// the pair is the two slots in a row and the first one's index in
    /// [`OpArray::numeric_dims`]. Both are `zend_add_literal` as it
    /// stands, never a slot the table holds already, since the pair is
    /// read by its adjacency.
    fn dim_operand(&mut self, value: Where) -> (OperandKind, u32) {
        let Where::Constant(constant) = value else {
            return self.operand(value);
        };
        let folded = constant
            .as_str()
            .and_then(|bytes| php_heap::canonical_int(bytes.bytes()));
        let Some(n) = folded else {
            return self.operand(value);
        };
        let at = self.literal(Zval::long(n));
        let _written = self.literal(constant);
        self.numeric_dims.push(ConstIdx(at));
        (OperandKind::Const, at)
    }

    /// `zend_compile_expr_with_potential_assign_to_self`: `$a[0] = $a`
    /// evaluates the right `$a` first, through a `QM_ASSIGN` into a
    /// temporary, since the write to the left one may move what the
    /// right one named. php compares the written names of the chain's
    /// base and of the value (`zend_is_assign_to_self`), the chain walked
    /// through every node `zend_is_variable` takes, so `$a[0][1] = $a`,
    /// `$a->x[] = $a` and `$a[0]->p[1] = $a` are each one too.
    ///
    /// ```php
    /// $a = new stdClass;
    /// $a->x[] = $a;
    /// var_dump($a->x[0] === $a);
    /// $b = [new stdClass];
    /// $b[0]->p[1] = $b;
    /// var_dump(count($b[0]->p[1]), $b[0]->p[1][0] === $b[0]);
    /// ```
    /// ```expect
    /// bool(true)
    /// int(1)
    /// bool(true)
    /// ```
    fn assigned_value(
        &mut self,
        target: tree::Expr,
        value: tree::Expr,
    ) -> Result<Where, CompileError> {
        let line = self.line;
        let tree::Expr::SimpleVarExpr(id) = self.peel(value) else {
            return self.expr(value);
        };
        let SimpleVar::Cv(name) = self.simple_var_of(id, line)? else {
            return self.expr(value);
        };
        let tree::Expr::SimpleVarExpr(id) = self.chain_base(target) else {
            return self.expr(value);
        };
        if self.simple_var_of(id, line)? != SimpleVar::Cv(name) {
            return self.expr(value);
        }
        self.cv_fetched(name, Fetch::Read);
        let cv = self.lookup_cv(name);
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

    /// `zend_is_assign_to_self`'s walk down a target to its first
    /// child while `zend_is_variable` (`Z/Zend/zend_compile.c:2718`)
    /// holds and the node is no `ZEND_AST_VAR`: a dimension's base, a
    /// property's object and a `static` property's class. A call stops
    /// it, so `$a->m()->x[0] = $a` assigns the CV itself. Where it stops
    /// is what the value's name is held to.
    ///
    /// ```php
    /// class C { public static $s = []; public $x = []; function m() { return $this; } }
    /// $a = new C;
    /// $a->m()->x[0] = $a;
    /// $a::$s[0] = $a;
    /// var_dump($a->x[0] === $a, C::$s[0] === $a);
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// ```
    fn chain_base(&self, target: tree::Expr) -> tree::Expr {
        let mut node = self.peel(target);
        loop {
            let child = match node {
                tree::Expr::SubscriptExpr(id) => self.parsed.tree[id].base,
                tree::Expr::MemberAccessExpr(id) => self.parsed.tree[id].obj,
                tree::Expr::ScopedAccessExpr(id) if self.names_property(id) => {
                    self.parsed.tree[id].scope
                }
                _ => return node,
            };
            node = self.peel(child);
        }
    }

    /// `zend_compile_assign`, the `ZEND_AST_PROP` arm: the property
    /// fetch compiled delayed (`zend_delayed_compile_prop` under
    /// `BP_VAR_W`), the value after it, the delayed oplines laid down at
    /// `zend_delayed_compile_end` with the last of them turned into
    /// `ASSIGN_OBJ` writing a temporary, and `OP_DATA` carrying the value
    /// (`zend_emit_op_data`). What is delayed is the chain of property
    /// fetches and the base under it: the base is
    /// [`Self::delayed_object`]'s, so a CV settles where it stands, a
    /// `$this` inside a method is the `IS_UNUSED` receiver, a call takes
    /// `SEPARATE` over its own `VAR` (`zend_separate_if_call_and_write`)
    /// before the delayed fetches, and a dimension is a `FETCH_DIM_W`
    /// held back with them, carrying `ZEND_FETCH_DIM_OBJ` as the last
    /// link under a `->`. Each fetch of the chain but the last is
    /// `FETCH_OBJ_W` into a `VAR` (`zend_adjust_for_fetch_type` under
    /// `BP_VAR_W`), its name one constant in `op2` and three cache
    /// slots in `extended_value`, and its `VAR` is numbered, its name
    /// added and its slots taken when the fetch is delayed, before the
    /// value's own, which is the order php's temporaries come in.
    ///
    /// ```php
    /// $a = [new stdClass];
    /// $a[0]->p = 1;
    /// $a[0]->o = new stdClass;
    /// $a[0]->o->q = 2;
    /// var_dump($a);
    /// ```
    /// ```expect
    /// array(1) {
    ///   [0]=>
    ///   object(stdClass)#1 (2) {
    ///     ["p"]=>
    ///     int(1)
    ///     ["o"]=>
    ///     object(stdClass)#2 (1) {
    ///       ["q"]=>
    ///       int(2)
    ///     }
    ///   }
    /// }
    /// ```
    fn assign_prop(
        &mut self,
        object: tree::Expr,
        name: Member,
        value: Source,
    ) -> Result<Where, CompileError> {
        let (base, links) = self.prop_chain(object, Fetch::Write)?;
        let name = self.prop_name(name)?;
        let out = self.temporary();
        // The `ASSIGN_OBJ` is the chain's last held-back `FETCH_OBJ_W`,
        // laid down after the value.
        let held = self.held_back();
        // The delayed fetch's line, as [`Self::assign_dim`] keeps it.
        let line = self.line;
        let value = self.source(value)?;
        let (data_type, data) = self.operand(value);
        self.line = line;
        let (op1_type, op1) = self.prop_fetches(base, &links, Fetch::Write);
        drop(held);
        self.push(Opline {
            opcode: Opcode::AssignObj,
            op1_type,
            op1,
            op2_type: name.kind,
            op2: name.at,
            out_type: OperandKind::Tmp,
            out,
            extended_value: name.slots,
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

    /// `zend_compile_assign`, the `ZEND_AST_STATIC_PROP` arm
    /// (`Z/Zend/zend_compile.c:3477`): the fetch compiled delayed
    /// (`zend_delayed_compile_var` under `BP_VAR_W`, which is
    /// [`Self::static_prop`]'s `zend_compile_static_prop` with
    /// `delayed`), the value after it, and the one delayed opline laid
    /// down at `zend_delayed_compile_end` with its opcode set to
    /// `ASSIGN_STATIC_PROP` and its `result` to a temporary, then
    /// `OP_DATA` carrying the value (`zend_emit_op_data`), which is why
    /// the handler steps over two oplines. The property's name is one
    /// constant in `op1`, the class's constant pair is `op2` and three
    /// cache slots stand in `extended_value`, each taken where the
    /// fetch is delayed and so before the value's own literals and
    /// temporaries; [`Self::do_free`] marks the temporary `UNUSED`
    /// where nothing reads it, which is why php's dump of
    /// `T::$a = 5; var_dump(T::$a);` numbers the read's temporary `T1`.
    fn assign_static_prop(
        &mut self,
        class: ClassSpec,
        name: &ScopedName,
        value: Source,
    ) -> Result<Where, CompileError> {
        let StaticOperands {
            name: (name_type, name_at),
            class: (class_type, class_at),
            slots,
        } = self.static_operands(class, name)?;
        let out = self.temporary();
        // `zend_delayed_compile_var`'s one held-back fetch, laid down
        // after the value.
        let held = self.held_back();
        // The delayed fetch's line, as [`Self::assign_dim`] keeps it.
        let line = self.line;
        let value = self.source(value)?;
        let (data_type, data) = self.operand(value);
        self.line = line;
        self.push(Opline {
            opcode: Opcode::AssignStaticProp,
            op1_type: name_type,
            op1: name_at,
            op2_type: class_type,
            op2: class_at,
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
        drop(held);
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_compound_assign`, the `ZEND_AST_DIM` arm
    /// (`Z/Zend/zend_compile.c:3659`): the chain compiled delayed under
    /// `BP_VAR_RW` (`zend_delayed_compile_dim`), the value after it with
    /// `zend_compile_expr_with_potential_assign_to_self`, which the
    /// plain variable arm does not use, and the one delayed opline laid
    /// down with its opcode set to `ASSIGN_DIM_OP`, the operator in
    /// `extended_value` and its `result` a temporary. The value follows
    /// on an `OP_DATA` and carries no cache slot, since a dimension
    /// fetch has none to move.
    ///
    /// A chain of more than one dimension is php's `FETCH_DIM_RW` for
    /// every link but the last, and a property under one is
    /// `FETCH_OBJ_RW`; both are [`Fetch::Rw`], the chain compiled under
    /// `BP_VAR_RW` from end to end.
    fn assign_dim_op(
        &mut self,
        target: tree::Expr,
        op: BinaryOp,
        value: tree::Expr,
    ) -> Result<Where, CompileError> {
        let (container, links) = self.dim_chain(target, Fetch::Rw)?;
        let Some((last, fetches)) = links.split_last() else {
            return Err(self.fatal("a dimension with no dimension"));
        };
        // `zend_delayed_emit_op` stamped the fetch's line where the
        // fetch stood, as [`Self::assign_dim`] keeps it.
        let line = self.line;
        let value = self.assigned_value(target, value)?;
        let (data_type, data) = self.operand(value);
        let (op1_type, op1) = self.dim_fetches(container, fetches, Fetch::Rw, Over::Dim);
        self.line = line;
        self.push(Opline {
            opcode: Opcode::AssignDimOp,
            op1_type,
            op1,
            op2_type: last.kind,
            op2: last.at,
            out_type: OperandKind::Tmp,
            out: last.out,
            extended_value: u32::from(Self::opcode_of(op) as u8),
            ..Opline::default()
        });
        self.push(Opline {
            opcode: Opcode::OpData,
            op1_type: data_type,
            op1: data,
            ..Opline::default()
        });
        Ok(Where::Temporary(last.out))
    }

    /// `zend_compile_compound_assign`, the `ZEND_AST_PROP` arm
    /// (`Z/Zend/zend_compile.c:3672`): the fetch compiled delayed under
    /// `BP_VAR_RW` (`zend_delayed_compile_prop`), the value after it,
    /// and the one delayed opline laid down with its opcode set to
    /// `ASSIGN_OBJ_OP` and its `result` a temporary. The operator takes
    /// the place the cache slot held in `extended_value` and the slot
    /// moves onto the `OP_DATA`, as the `static` property arm moves it
    /// ([`Self::assign_static_prop_op`]).
    ///
    /// A property under a property is php's `FETCH_OBJ_RW` for every
    /// fetch but the last, which [`Self::prop_chain`] holds back with
    /// the rest of the chain.
    ///
    /// ```php
    /// $a = ['k' => [1 => new stdClass]];
    /// $a['k'][1]->p = 'a';
    /// $a['k'][1]->p .= 'x';
    /// var_dump($a['k'][1]->p);
    /// ```
    /// ```expect
    /// string(2) "ax"
    /// ```
    fn assign_obj_op(
        &mut self,
        object: tree::Expr,
        name: Member,
        op: BinaryOp,
        value: tree::Expr,
    ) -> Result<Where, CompileError> {
        let (receiver, links) = self.prop_chain(object, Fetch::Rw)?;
        let name = self.prop_name(name)?;
        let out = self.temporary();
        // The `ASSIGN_OBJ_OP` is the chain's last held-back `FETCH_OBJ_RW`.
        let held = self.held_back();
        // The delayed fetch's line, as [`Self::assign_prop`] keeps it.
        let line = self.line;
        let value = self.expr(value)?;
        let (data_type, data) = self.operand(value);
        let (op1_type, op1) = self.prop_fetches(receiver, &links, Fetch::Rw);
        drop(held);
        self.line = line;
        self.push(Opline {
            opcode: Opcode::AssignObjOp,
            op1_type,
            op1,
            op2_type: name.kind,
            op2: name.at,
            out_type: OperandKind::Tmp,
            out,
            extended_value: u32::from(Self::opcode_of(op) as u8),
            ..Opline::default()
        });
        self.push(Opline {
            opcode: Opcode::OpData,
            op1_type: data_type,
            op1: data,
            extended_value: name.slots,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_delayed_compile_prop` under `BP_VAR_RW` or `BP_VAR_W` over
    /// the receiver of a property fetch: the innermost receiver,
    /// compiled where it stands, and one held-back link per `->` under
    /// the last, each taking its name, its temporary and its cache
    /// slots here and its opline laid down by [`Self::prop_fetches`]
    /// after the value, so `$o->p->q += f()` calls `f` before it fetches
    /// `p`, as php's delayed emission does. A name the program computes
    /// is compiled here too, innermost first, before the value. The
    /// receiver is [`Self::delayed_object`]'s, so a dimension under it
    /// is `FETCH_DIM_RW` or `FETCH_DIM_W` held back with the rest
    /// (`$a[0]->p += 1`).
    fn prop_chain(
        &mut self,
        object: tree::Expr,
        fetch: Fetch,
    ) -> Result<(Base, Vec<PropLink>), CompileError> {
        let mut names = Vec::new();
        let mut base = self.peel(object);
        while let tree::Expr::MemberAccessExpr(id) = base {
            names.push(self.member_of(id)?);
            base = self.peel(self.parsed.tree[id].obj);
        }
        let receiver = self.delayed_object(base, fetch)?;
        let mut links = Vec::with_capacity(names.len());
        for &name in names.iter().rev() {
            let name = self.prop_name(name)?;
            let out = self.temporary();
            links.push(PropLink {
                name,
                out,
                _held: self.held_back(),
            });
        }
        Ok((receiver, links))
    }

    /// The held-back `FETCH_OBJ_RW` or `FETCH_OBJ_W` oplines of a
    /// property chain, laid down at `zend_delayed_compile_end` in the
    /// order they were taken, each over the `VAR` the one under it left.
    fn prop_fetches(
        &mut self,
        receiver: Base,
        links: &[PropLink],
        fetch: Fetch,
    ) -> (OperandKind, u32) {
        let (mut op1_type, mut op1) = self.base_fetch(receiver, fetch, Over::Obj);
        for link in links {
            self.push(Opline {
                opcode: if fetch == Fetch::Write {
                    Opcode::FetchObjW
                } else {
                    Opcode::FetchObjRw
                },
                op1_type,
                op1,
                op2_type: link.name.kind,
                op2: link.name.at,
                out_type: OperandKind::Var,
                out: link.out,
                extended_value: link.name.slots,
                ..Opline::default()
            });
            op1_type = OperandKind::Var;
            op1 = link.out;
        }
        (op1_type, op1)
    }

    /// `zend_compile_compound_assign`, the `ZEND_AST_STATIC_PROP` arm
    /// (`Z/Zend/zend_compile.c:3643`): the fetch compiled delayed under
    /// `BP_VAR_RW`, the value after it, and the one delayed opline laid
    /// down with its opcode set to `ASSIGN_STATIC_PROP_OP` and its
    /// `result` a temporary. The operator takes the place the cache slot
    /// held in `extended_value`, and the slot moves onto the `OP_DATA`
    /// beside it (`opline = zend_emit_op_data(&expr_node);
    /// opline->extended_value = cache_slot;`), which is where the
    /// handler reads it from.
    fn assign_static_prop_op(
        &mut self,
        class: ClassSpec,
        name: &ScopedName,
        op: BinaryOp,
        value: tree::Expr,
    ) -> Result<Where, CompileError> {
        let StaticOperands {
            name: (name_type, name_at),
            class: (class_type, class_at),
            slots,
        } = self.static_operands(class, name)?;
        let out = self.temporary();
        // `zend_delayed_compile_var`'s one held-back fetch, laid down
        // after the value.
        let held = self.held_back();
        // The delayed fetch's line, as [`Self::assign_static_prop`]
        // keeps it.
        let line = self.line;
        let value = self.expr(value)?;
        let (data_type, data) = self.operand(value);
        self.line = line;
        self.push(Opline {
            opcode: Opcode::AssignStaticPropOp,
            op1_type: name_type,
            op1: name_at,
            op2_type: class_type,
            op2: class_at,
            out_type: OperandKind::Tmp,
            out,
            extended_value: u32::from(Self::opcode_of(op) as u8),
            ..Opline::default()
        });
        self.push(Opline {
            opcode: Opcode::OpData,
            op1_type: data_type,
            op1: data,
            extended_value: slots,
            ..Opline::default()
        });
        drop(held);
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_pre_incdec` and `zend_compile_post_incdec` over a
    /// `ZEND_AST_STATIC_PROP` (`Z/Zend/zend_compile.c:10416`, `:10444`):
    /// the fetch's own opline under `BP_VAR_RW`, its opcode set to one
    /// of the four steps and its `result` a temporary, and nothing
    /// beside it. The property's name is `op1`, the class's constant
    /// pair `op2` and three cache slots stand in `extended_value`, as
    /// the fetch left them. [`Self::do_free`] turns a `POST_*` nothing
    /// reads into the `PRE_*` php's `zend_do_free` turns it into, which
    /// is why `C::$p++` as a statement is `PRE_INC_STATIC_PROP`.
    fn static_prop_step(
        &mut self,
        class: ClassSpec,
        name: &ScopedName,
        step: Step,
    ) -> Result<Where, CompileError> {
        let StaticOperands {
            name: (name_type, name_at),
            class: (class_type, class_at),
            slots,
        } = self.static_operands(class, name)?;
        let out = self.temporary();
        self.push(Opline {
            opcode: match step {
                Step::PreInc => Opcode::PreIncStaticProp,
                Step::PreDec => Opcode::PreDecStaticProp,
                Step::PostInc => Opcode::PostIncStaticProp,
                Step::PostDec => Opcode::PostDecStaticProp,
            },
            op1_type: name_type,
            op1: name_at,
            op2_type: class_type,
            op2: class_at,
            out_type: OperandKind::Tmp,
            out,
            extended_value: slots,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_assign_ref`, the arm over the fetch it left
    /// standing as `FETCH_STATIC_PROP_W` (`Z/Zend/zend_compile.c:3597`):
    /// the opcode becomes `ASSIGN_STATIC_PROP_REF`, `ZEND_FETCH_REF`
    /// comes off `extended_value` and `ZEND_RETURNS_FUNCTION` goes on
    /// where the source is a call, and the source rides in the `OP_DATA`
    /// beside it, so the handler steps over two oplines. What the
    /// opline leaves is the fetch's `VAR`, which [`Self::do_free`]
    /// marks `UNUSED` where nothing reads it.
    ///
    /// The name, the `VAR` and the cache slots are taken before the
    /// source compiles and the opline is pushed after it, which is
    /// where `zend_delayed_compile_end` lays the held-back
    /// `FETCH_STATIC_PROP_W` down. A `static` property is no plain
    /// `$name`, so [`Self::ref_source`] puts `MAKE_REF` over whatever
    /// the source leaves but a CV.
    fn assign_static_prop_ref(
        &mut self,
        class: ClassSpec,
        name: &ScopedName,
        source: Source,
    ) -> Result<Where, CompileError> {
        let StaticOperands {
            name: (name_type, name_at),
            class: (class_type, class_at),
            slots,
        } = self.static_operands(class, name)?;
        let out = self.temporary();
        // `zend_delayed_compile_var`'s one held-back fetch, laid down
        // after the value.
        let held = self.held_back();
        let (data_type, data, flags) = self.ref_source_of(source, false)?;
        self.push(Opline {
            opcode: Opcode::AssignStaticPropRef,
            op1_type: name_type,
            op1: name_at,
            op2_type: class_type,
            op2: class_at,
            out_type: OperandKind::Var,
            out,
            extended_value: slots | flags,
            ..Opline::default()
        });
        self.push(Opline {
            opcode: Opcode::OpData,
            op1_type: data_type,
            op1: data,
            ..Opline::default()
        });
        drop(held);
        Ok(Where::Returned(out))
    }

    /// `zend_compile_binary_op`: both sides compiled, left then right,
    /// and the operator over what they compiled to
    /// ([`Self::binary_operands`]). Each side is walked once, by its
    /// compile, so a chain of `n` operators is `n` compiles deep and no
    /// deeper, as php's is, and this frame, which a left-nested chain
    /// stands in once per operator, holds the two compiles and nothing
    /// else.
    #[allow(clippy::inline_always)] // measured: as `binary_expr`'s
    #[inline(always)]
    fn binary(
        &mut self,
        id: NodeId<tree::BinaryExpr>,
        op: BinaryOp,
        left: tree::Expr,
        right: tree::Expr,
    ) -> Result<Where, CompileError> {
        let left = self.expr(left)?;
        let right = self.expr(right)?;
        self.binary_operands(id, op, left, right)
    }

    /// The rest of `zend_compile_binary_op` over its two compiled sides:
    /// the value where `zend_try_ct_eval_binary_op` folds two constants,
    /// the one fold [`Self::constant_operands`] makes for
    /// `zend_eval_const_expr` too ([`Self::operands_folded`]), and
    /// otherwise the `ZEND_CONCAT`, comparison and bitwise arms each to
    /// its own and the opcode into a temporary (`zend_emit_op_tmp`). A
    /// call of its own, so what an arm takes is off the stack of a
    /// left-nested chain, which stands in [`Self::binary`] once per
    /// operator (#3159).
    #[inline(never)]
    fn binary_operands(
        &mut self,
        id: NodeId<tree::BinaryExpr>,
        op: BinaryOp,
        left: Where,
        right: Where,
    ) -> Result<Where, CompileError> {
        if let (Where::Constant(a), Where::Constant(b)) = (left, right) {
            let fold = match (op, self.binary_of(id)) {
                (BinaryOp::Concat, (lhs, _, Some(rhs))) => self.parser_folds(id, lhs, rhs),
                _ => ConcatFold::Compiler,
            };
            match self.constant_operands(op, a, b, fold) {
                Ok(folded) => return Ok(Where::Constant(self.operands_folded(id, folded))),
                Err(Unfolded::NoConstant(_)) => {}
                Err(Unfolded::Unread(refused) | Unfolded::Refused(refused)) => {
                    return Err(refused);
                }
            }
        }
        if op == BinaryOp::Concat {
            return self.concat(left, right);
        }
        if compares(op) {
            return Ok(self.comparison(op, left, right));
        }
        Ok(self.emit_binary(Self::opcode_of(op), left, right, 0))
    }

    /// `zend_try_ct_eval_binary_op` over the arithmetic operators: two
    /// constant arrays under `+` are the one pair of arrays
    /// `zend_binary_op_produces_error` lets through, which `add_function`
    /// joins ([`Self::array_union`]); an array beside anything else, or
    /// under any other operator, is left to the opcode, which throws;
    /// every other pair is [`fold`]'s.
    fn arithmetic_fold(
        &mut self,
        op: BinaryOp,
        left: Zval,
        right: Zval,
    ) -> Result<Option<Operated>, CompileError> {
        match (op, left.as_arr(), right.as_arr()) {
            (BinaryOp::Add, Some(left), Some(right)) => self
                .array_union(left, right)
                .map(|made| Some(Operated::Made(made))),
            (_, None, None) => Ok(fold(op, left, right).map(Operated::Kept)),
            _ => Ok(None),
        }
    }

    /// `add_function` over two arrays: the left one's entries in its
    /// order, then each of the right one's whose key the left one has
    /// not (`zend_hash_merge` without overwrite), laid down as one
    /// constant array.
    ///
    /// ```php
    /// var_dump([0, 1] + [2, 3, 4], ['a' => 1, 5 => 'x'] + ['a' => 2, 'b' => 3], [] + []);
    /// $p = [0, 1] + [2];
    /// $p[] = 'n';
    /// var_dump($p);
    /// ```
    /// ```expect
    /// array(3) {
    ///   [0]=>
    ///   int(0)
    ///   [1]=>
    ///   int(1)
    ///   [2]=>
    ///   int(4)
    /// }
    /// array(3) {
    ///   ["a"]=>
    ///   int(1)
    ///   [5]=>
    ///   string(1) "x"
    ///   ["b"]=>
    ///   int(3)
    /// }
    /// array(0) {
    /// }
    /// array(3) {
    ///   [0]=>
    ///   int(0)
    ///   [1]=>
    ///   int(1)
    ///   [2]=>
    ///   string(1) "n"
    /// }
    /// ```
    fn array_union(&mut self, left: Arr, right: Arr) -> Result<Made, CompileError> {
        let mut entries: Vec<(Key, Zval)> = left.iter().collect();
        entries.extend(right.iter().filter(|(key, _)| left.get(*key).is_none()));
        self.lay_array(&entries, Vec::new(), entries.len())
    }

    /// `zend_compile_binary_op` once `zend_try_ct_eval_binary_op` has
    /// folded its two constant sides: each side's constant destroyed
    /// (`zval_ptr_dtor` of `left_node.u.constant` and
    /// `right_node.u.constant`), so a string the scanner or a fold below
    /// made for the side alone is given back where its claim stands on
    /// top of [`eval::Claims`], the right side's above the left side's;
    /// and the value the node's own claim ([`Self::claimed`]), which the
    /// operator over it gives back in turn, or keeps where an opline
    /// reads the value as its `CONST` operand. The claim is taken off the
    /// top and no mark is held, so the frame of [`Self::binary`], which a
    /// left-nested chain stands in once per operator, holds only the
    /// node's id for it.
    fn operands_folded(&mut self, id: NodeId<tree::BinaryExpr>, folded: Operated) -> Zval {
        let node = &self.parsed.tree[id];
        let sides = [node.rhs, Some(node.lhs)];
        for side in sides.into_iter().flatten() {
            self.side_folded(side);
        }
        self.claimed(tree::Expr::BinaryExpr(id), folded)
    }

    /// One side of a compile's fold destroyed once the fold stands
    /// (`zval_ptr_dtor` of the side's `u.constant`): its own claim given
    /// back where its compile left it on top of [`eval::Claims`], and
    /// nothing where the side's constant is kept elsewhere. A unary
    /// operator's fold and a short circuit over a constant left side
    /// give theirs back as a binary operator's does, so only the string
    /// a fold leaves standing joins opcache's buffer:
    ///
    /// ```php
    /// $n = fn () => opcache_get_status()['interned_strings_usage']['number_of_strings'];
    /// foreach (['-"78"', '!"ab"', '~"st"', '~"cd" . "e"', '"vw" && "yz"', '"ij" || "kl"', '"" || "mn"', '"op" && $q'] as $i => $e) {
    ///     $f = sys_get_temp_dir() . "/side-$i-" . getmypid() . '.php';
    ///     file_put_contents($f, "<?php \$z$i = @($e);");
    ///     $a = $n();
    ///     include $f;
    ///     echo $e, ': ', $n() - $a, "\n";
    ///     unlink($f);
    /// }
    /// ```
    /// ```expect regime=jit-off exit=0
    /// -"78": 2
    /// !"ab": 2
    /// ~"st": 3
    /// ~"cd" . "e": 3
    /// "vw" && "yz": 2
    /// "ij" || "kl": 2
    /// "" || "mn": 2
    /// "op" && $q: 2
    /// ```
    pub(crate) fn side_folded(&mut self, side: tree::Expr) {
        let side = self.peel(side);
        if let Some(made) = self.claims.take_top(side) {
            self.strings.give_back(made);
        }
    }

    /// `zend_compile_binary_op`'s `ZEND_CONCAT` arm over two compiled
    /// sides it did not fold: each constant side converted to a string
    /// where it stands and `CONCAT` into a temporary. php converts
    /// before it emits, so what the opline reads is the string and
    /// never the number it was written as.
    ///
    /// Where both sides are constants the fold stands in its place
    /// ([`Self::operands_folded`]), and the strings the two sides were
    /// written as are freed with the fold, as `zend_compile_binary_op`
    /// destroys each side's zval, so neither joins opcache's buffer with
    /// the unit; a fold of a fold gives back the inner one's string too,
    /// and a comparison or an arithmetic fold its two strings:
    ///
    /// ```php
    /// $n = fn () => opcache_get_status()['interned_strings_usage']['number_of_strings'];
    /// foreach (['"ab" . "cd"', '"ef" | "gh"', '12 . "x"', '"ij" . "kl" . "mn"', '"op" == "qr"', '"34" * "56"', '"AB" . $u'] as $i => $e) {
    ///     $f = sys_get_temp_dir() . "/concat-$i-" . getmypid() . '.php';
    ///     file_put_contents($f, "<?php \$z$i = @($e);");
    ///     $a = $n();
    ///     include $f;
    ///     echo $e, ': ', $n() - $a, "\n";
    ///     unlink($f);
    /// }
    /// ```
    /// ```expect regime=jit-off exit=0
    /// "ab" . "cd": 3
    /// "ef" | "gh": 3
    /// 12 . "x": 3
    /// "ij" . "kl" . "mn": 3
    /// "op" == "qr": 2
    /// "34" * "56": 2
    /// "AB" . $u: 3
    /// ```
    fn concat(&mut self, left: Where, right: Where) -> Result<Where, CompileError> {
        let left = self.string_operand(left)?;
        let right = self.string_operand(right)?;
        Ok(self.emit_binary(Opcode::Concat, left, right, 0))
    }

    /// `zend_try_ct_eval_binary_op` over `bitwise_or_function`,
    /// `bitwise_xor_function` and `bitwise_and_function`, under the
    /// three arms `zend_binary_op_produces_error` has for them: an array
    /// on either side is refused, two strings are folded byte-wise and
    /// are the one pair the guard lets through whatever they hold, a
    /// string that is no number refuses, and an operand the long
    /// conversion would not be exact over refuses too, since php will
    /// not write the deprecation's answer into a constant. The string two
    /// strings fold to is the reader's alone ([`Operated::Made`]), which
    /// the node over it gives back once it stands, as
    /// `zend_eval_const_expr` destroys an operand's zval, and a long is
    /// kept; that string and the text a number is joined as
    /// ([`Self::stringified_made`]) are given back once read, so neither
    /// joins opcache's buffer with the unit that folded them:
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/bitwise-' . getmypid() . '.php';
    /// file_put_contents($f, '<?php class BW { const X = ("ab" | "cd") . "!"; const Y = 12 . "x"; const Z = 1.5 . "y"; }');
    /// $n = fn () => opcache_get_status()['interned_strings_usage']['number_of_strings'];
    /// $a = $n();
    /// include $f;
    /// echo $n() - $a, ' ', BW::X, ' ', BW::Y, ' ', BW::Z, "\n";
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 4 cf! 12x 1.5y
    /// ```
    fn bitwise_made(
        &mut self,
        op: BinaryOp,
        left: Zval,
        right: Zval,
    ) -> Result<Option<Operated>, CompileError> {
        if left.as_arr().is_some() || right.as_arr().is_some() {
            return Ok(None);
        }
        if let (Some(a), Some(b)) = (left.as_str(), right.as_str()) {
            let folded = bitwise_bytes(op, a.bytes(), b.bytes());
            return Ok(Some(Operated::Made(self.string_made(
                &folded,
                false,
                Sizing::Alloc,
            )?)));
        }
        let (Some(a), Some(b)) = (number(left), number(right)) else {
            return Ok(None);
        };
        let (Some(a), Some(b)) = (long_compatible(a), long_compatible(b)) else {
            return Ok(None);
        };
        Ok(Some(Operated::Kept(Zval::long(bitwise_longs(op, a, b)))))
    }

    /// `zend_compile_binary_op`'s comparison arms and `zend_compile_greater`
    /// over two compiled sides it did not fold (two constants
    /// [`Self::compare_fold`] folds, which `zend_binary_op_produces_error`
    /// never refuses for a comparison): a `===` or `!==` against a constant null, false or true as the
    /// `TYPE_CHECK` php emits in its place, `$a === null` being
    /// `is_null($a)`; and otherwise the opcode into a temporary, `>` and
    /// `>=` as `IS_SMALLER` and `IS_SMALLER_OR_EQUAL` with the operands
    /// swapped, which is the one shape php has for them.
    fn comparison(&mut self, op: BinaryOp, left: Where, right: Where) -> Where {
        if let Some((operand, mask)) = type_check(op, left, right) {
            return self.emit_unary(Opcode::TypeCheck, operand, mask);
        }
        match op {
            BinaryOp::Greater => self.emit_binary(Opcode::IsSmaller, right, left, 0),
            BinaryOp::GreaterOrEqual => self.emit_binary(Opcode::IsSmallerOrEqual, right, left, 0),
            _ => self.emit_binary(Self::opcode_of(op), left, right, 0),
        }
    }

    /// `zend_try_ct_eval_binary_op` over `is_smaller_function` and its
    /// kin, and `zend_ct_eval_greater`, which calls the same two with
    /// the constants swapped: the bool each comparison reads off
    /// [`php_heap::compare_constants`]' ordering, the long
    /// `compare_function` writes for `<=>`, and
    /// [`php_heap::identical_constants`] for `===` and `!==`, at the
    /// compile-time `precision` a double is rendered at against a string
    /// that is not numeric. A constant array is compared as the kernel
    /// compares it, since `zend_binary_op_produces_error` refuses no
    /// comparison:
    ///
    /// ```php
    /// var_dump([1] === [2], [1] == [2], [1, 2, 3] <=> [], [1] === [], [1] == [1], [1] < [2]);
    /// var_dump(['a' => 1, 'b' => 2] == ['b' => 2, 'a' => 1], ['a' => 1, 'b' => 2] === ['b' => 2, 'a' => 1], [1] == ['1'], [1] === ['1']);
    /// var_dump([] == false, [0] == true, [] < null, [1] > 5, 'x' < [], ['a' => 1] <=> ['b' => 1], [[1, 2]] < [[1, 3]]);
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// int(1)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// int(1)
    /// bool(true)
    /// ```
    fn compare_fold(&self, op: BinaryOp, left: Zval, right: Zval) -> Zval {
        let compare = |l, r| php_heap::compare_constants(l, r, self.precision);
        match op {
            BinaryOp::IsIdentical => Zval::bool(php_heap::identical_constants(left, right)),
            BinaryOp::IsNotIdentical => Zval::bool(!php_heap::identical_constants(left, right)),
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
            // An arithmetic operator is [`fold`]'s, an `xor`
            // [`Emit::bool_xor`]'s and a `.` [`Emit::concat_fold`]'s;
            // none of them reaches here.
            BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Mod
            | BinaryOp::Pow
            | BinaryOp::Sl
            | BinaryOp::Sr
            | BinaryOp::BoolXor
            | BinaryOp::BwOr
            | BinaryOp::BwXor
            | BinaryOp::BwAnd
            | BinaryOp::Concat => Zval::null(),
        }
    }

    /// `concat_function` over two constants as `fold` makes it: the one
    /// string they join into, or nothing where either is left to the
    /// opcode.
    pub(crate) fn concat_fold(
        &mut self,
        left: Zval,
        right: Zval,
        fold: ConcatFold,
    ) -> Result<Option<Zval>, CompileError> {
        Ok(self
            .concat_made(left, right, fold)?
            .map(|made| made.value()))
    }

    /// Whether php's parser folded the `.` at `id`, whose sides are
    /// `left` and `right`, before the compiler saw it:
    /// `zend_ast_create_concat_op` over two `ZEND_AST_ZVAL`s, each a
    /// written value or a `.` the parser folded. Asked once both sides
    /// have folded, so a side's answer is the set's, and a chain of `n`
    /// is `n` asks.
    fn parser_folds(
        &mut self,
        id: NodeId<tree::BinaryExpr>,
        left: tree::Expr,
        right: tree::Expr,
    ) -> ConcatFold {
        let zval = |this: &Self, side: tree::Expr| {
            this.is_written_value(side)
                || matches!(peel(this.parsed, side), tree::Expr::BinaryExpr(dot) if this.parser_dots.contains(&dot))
        };
        if zval(self, left) && zval(self, right) {
            self.parser_dots.insert(id);
            ConcatFold::Parser
        } else {
            ConcatFold::Compiler
        }
    }

    /// [`Self::concat_fold`]'s string as the claim of the one who asked
    /// ([`Made`]), which hands it back once it is read
    /// ([`Self::binary_constant`]) or keeps it by dropping the claim.
    ///
    /// The parser's fold joins into its left side, `concat_function(zv0,
    /// zv0, zv1)`, so a left side of the scanner's or a fold's own, two
    /// bytes or more, is grown where it stands by `zend_string_extend`,
    /// which php-8.5.12 sizes as `zend_string_finit`
    /// ([`Intern::intern_made_reallocated`]); the interned one-byte and
    /// empty strings, and every compiler's fold, which joins into a
    /// fresh zval, are `zend_string_alloc`'s. Under opcache, a
    /// fold's string in an `eval`'s class constant:
    ///
    /// ```php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('class W2 { const K = "zzzzzzzzzzzzzz" . 7; }');
    /// foreach (['"abcdefghij5"', '"abcdefghi" . 55', '"abcdefgh" . 5555555', '"a" . "bcdefghijkl"', '"ab" . "cdefghijkl"', '55 . "abcdefghi"', '"abcdefgh" . (5 . 5)'] as $i => $e) {
    ///     echo m("class F$i { const K = $e; }"), " ", $e, "\n";
    /// }
    /// ```
    /// ```expect regime=jit-off
    /// 520 "abcdefghij5"
    /// 528 "abcdefghi" . 55
    /// 528 "abcdefgh" . 5555555
    /// 520 "a" . "bcdefghijkl"
    /// 528 "ab" . "cdefghijkl"
    /// 528 55 . "abcdefghi"
    /// 528 "abcdefgh" . (5 . 5)
    /// ```
    fn concat_made(
        &mut self,
        left: Zval,
        right: Zval,
        fold: ConcatFold,
    ) -> Result<Option<Made>, CompileError> {
        // `zend_binary_op_produces_error`'s concatenation arm: an array
        // on either side is the warning left to the request, and no
        // conversion is made of the other side here.
        if left.as_arr().is_some() || right.as_arr().is_some() {
            return Ok(None);
        }
        let a = self.stringified_made(left)?;
        let b = match self.stringified_made(right) {
            Ok(b) => b,
            Err(refused) => {
                self.give_back_all([a, None]);
                return Err(refused);
            }
        };
        let joined = match (&a, &b) {
            (Some(a), Some(b)) => match (a.value().as_str(), b.value().as_str()) {
                (Some(a), Some(b)) => {
                    // `concat_function`'s `ZSTR_COPY_CONCAT_PROPERTIES_BOTH`,
                    // then the interning that keeps a string the table
                    // already holds as it is.
                    let utf8 = a.is_valid_utf8() && b.is_valid_utf8();
                    let joined = [a.bytes(), b.bytes()].concat();
                    let sizing = if fold == ConcatFold::Parser
                        && a.bytes().len() > 1
                        && !b.bytes().is_empty()
                    {
                        Sizing::Finit
                    } else {
                        Sizing::Alloc
                    };
                    Some(self.string_made(&joined, utf8, sizing))
                }
                _ => None,
            },
            _ => None,
        };
        // The text an operand was converted to goes once the two are
        // joined, as `concat_function` releases `op1_copy` and `op2_copy`.
        self.give_back_all([a, b]);
        joined.transpose()
    }

    /// Each claim among `operated` given back ([`Intern::give_back`]);
    /// a kept value stays where it stands.
    fn give_back_all<const N: usize>(&mut self, operated: [Option<Operated>; N]) {
        for operated in operated.into_iter().flatten() {
            if let Operated::Made(made) = operated {
                self.strings.give_back(made);
            }
        }
    }

    /// A constant operand of a `.` as the string `zend_compile_binary_op`
    /// converts it to, a constant array as the `CAST (string)` it emits
    /// for one, and any other operand as it is. The cast stands after
    /// both sides are compiled and before the `CONCAT`, so the array's
    /// warning is raised before an object on the other side throws:
    ///
    /// ```php
    /// set_error_handler(function ($n, $s) { echo "warning: $s\n"; });
    /// try { new stdClass . []; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { [] . new stdClass; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $a = 1; echo $a . [], "\n";
    /// ```
    /// ```expect
    /// warning: Array to string conversion
    /// Object of class stdClass could not be converted to string
    /// warning: Array to string conversion
    /// Object of class stdClass could not be converted to string
    /// warning: Array to string conversion
    /// 1Array
    /// ```
    fn string_operand(&mut self, value: Where) -> Result<Where, CompileError> {
        match value {
            Where::Constant(v) if v.as_arr().is_some() => Ok(self.emit_cast(value, CastTo::Str)),
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
        Ok(self.stringified_made(value)?.map(Operated::kept))
    }

    /// [`Self::stringified`] as its reader holds it: a number's text is
    /// made for the read alone ([`Operated::Made`]), as
    /// `zval_get_string_func` makes the string `concat_function` frees
    /// once the two are joined; a string, and the text of `null`,
    /// `false` and `true`, which no allocation makes, are kept.
    fn stringified_made(&mut self, value: Zval) -> Result<Option<Operated>, CompileError> {
        // `zend_long_to_str` and `zend_double_to_str` mark their text.
        if let Some(n) = value.as_long() {
            let text = self.string_made(n.to_string().as_bytes(), true, Sizing::Alloc)?;
            return Ok(Some(Operated::Made(text)));
        }
        if let Some(d) = value.as_double() {
            let text = double_to_string(d, self.precision, Exponent::Upper);
            let text = self.string_made(text.as_bytes(), true, Sizing::Alloc)?;
            if d.is_nan() {
                self.raise(Level::Warning, "unexpected NAN value was coerced to string");
            }
            return Ok(Some(Operated::Made(text)));
        }
        Ok(match value.kind() {
            ZvalType::String => Some(Operated::Kept(value)),
            ZvalType::True => Some(Operated::Kept(self.string(b"1")?)),
            ZvalType::Null | ZvalType::False => Some(Operated::Kept(self.string(b"")?)),
            _ => None,
        })
    }

    /// `zend_try_ct_eval_cast` (`Z/Zend/zend_compile.c:12045`) over one
    /// folded operand: the value the cast leaves where php folds it, and
    /// nothing where php leaves the `CAST` opline for the request. php
    /// folds no cast over a NAN, since casting one warns; `(int)` of a
    /// double past the long's range is refused, since
    /// `zend_dval_to_lval` answers it modularly and php will not write
    /// that into a constant; `(string)` of a double reads the request's
    /// `precision` and `(string)` of an array warns, so neither folds;
    /// and there is no `IS_OBJECT` arm at all, so `(object)` never
    /// folds.
    fn cast_fold(&mut self, to: CastTo, value: Zval) -> Result<Option<Operated>, CompileError> {
        if value.as_double().is_some_and(f64::is_nan) {
            return Ok(None);
        }
        if to == CastTo::Array {
            return self.array_of(value);
        }
        Ok(match to {
            CastTo::Bool => Some(Zval::bool(truth(value))),
            CastTo::Long => match value.as_double() {
                Some(d) if !fits_long(d) => None,
                _ => Some(Zval::long(long_of(value))),
            },
            CastTo::Double => Some(Zval::double(double_of(value))),
            CastTo::Str => match value.kind() {
                ZvalType::Array | ZvalType::Double => None,
                _ => self.stringified(value)?,
            },
            CastTo::Array | CastTo::Object => None,
        }
        .map(Operated::Kept))
    }

    /// `convert_to_array` over a folded constant: an array is itself,
    /// `null` is the empty array, and every other constant the
    /// one-element array holding it at `0` (`convert_scalar_to_array`),
    /// the cast's own claim.
    fn array_of(&mut self, value: Zval) -> Result<Option<Operated>, CompileError> {
        if value.as_arr().is_some() {
            return Ok(Some(Operated::Kept(value)));
        }
        let entries: &[(Key, Zval)] = match value.kind() {
            ZvalType::Null => &[],
            _ => &[(Key::Long(0), value)],
        };
        self.lay_array(entries, Vec::new(), entries.len())
            .map(|made| Some(Operated::Made(made)))
    }

    /// `zend_compile_unary_pm`: php folds unary `-` and `+` as the
    /// multiplication by -1 and by 1 it would otherwise emit, and emits
    /// that multiplication where the operand is not a constant.
    fn unary(&mut self, minus: bool, side: tree::Expr) -> Result<Where, CompileError> {
        let sign = Zval::long(if minus { -1 } else { 1 });
        let operand = self.expr(side)?;
        if let Where::Constant(value) = operand
            && let Some(folded) = fold(BinaryOp::Mul, value, sign)
        {
            self.side_folded(side);
            return Ok(Where::Constant(folded));
        }
        Ok(self.emit_binary(Opcode::Mul, operand, Where::Constant(sign), 0))
    }

    /// `zend_compile_unary_op` (`Z/Zend/zend_compile.c:10309`) for the
    /// one operator that reaches it here, `ZEND_BOOL_NOT`: the operand
    /// compiled, `zend_try_ct_eval_unary_op` over it where it is a
    /// constant, and `zend_emit_op_tmp` otherwise.
    ///
    /// The fold is refused over a double, which is
    /// `zend_unary_op_produces_error`'s one arm for this opcode, so
    /// `!1` and `!""` are bools in the op array and `!1.5` is the
    /// opline over `float(1.5)`, as the oracle's dump has it.
    fn bool_not(&mut self, side: tree::Expr) -> Result<Where, CompileError> {
        let operand = self.expr(side)?;
        if let Where::Constant(value) = operand
            && value.as_double().is_none()
        {
            let folded = Zval::bool(!truth(value));
            self.side_folded(side);
            return Ok(Where::Constant(folded));
        }
        Ok(self.emit_unary(Opcode::BoolNot, operand, 0))
    }

    /// `zend_compile_unary_op` for `ZEND_BW_NOT`: the operand compiled,
    /// `zend_try_ct_eval_unary_op` over it where it is a constant, and
    /// `zend_emit_op_tmp` otherwise.
    ///
    /// The fold is `zend_unary_op_produces_error`'s `ZEND_BW_NOT` arm
    /// (`Z/Zend/zend_compile.c:10041`): a string is folded as it stands,
    /// since `~` over one is the bytes negated and no number is read out
    /// of it; `null`, `false` and `true` are refused, php's `Z_TYPE_P(op)
    /// <= IS_TRUE`; and everything else is folded only where
    /// `zend_is_op_long_compatible` says the long conversion loses
    /// nothing, so `~1.0` is `int(-2)` in the op array and `~1.5` is the
    /// opline over `float(1.5)`, which raises php's deprecation where it
    /// runs. An array reaches none of those arms and is the opline too,
    /// which throws.
    ///
    /// The fold's own value is php's, which is what a program reads
    /// where no opline stands; the string arm is byte-wise and its
    /// bytes are what this pins, since they are no text.
    ///
    /// ```php
    /// var_dump(~6, ~"ab" === "\x9e\x9d", ~1.0, ~"a" === "\x9e");
    /// ```
    /// ```expect
    /// int(-7)
    /// bool(true)
    /// int(-2)
    /// bool(true)
    /// ```
    fn bw_not(
        &mut self,
        id: NodeId<tree::UnaryExpr>,
        side: tree::Expr,
    ) -> Result<Where, CompileError> {
        let operand = self.expr(side)?;
        if let Where::Constant(value) = operand
            && let Some(folded) = self.bw_not_made(value)?
        {
            self.side_folded(side);
            return Ok(Where::Constant(
                self.claimed(tree::Expr::UnaryExpr(id), folded),
            ));
        }
        Ok(self.emit_unary(Opcode::BwNot, operand, 0))
    }

    /// `bitwise_not_function` over a constant php's
    /// `zend_unary_op_produces_error` lets through: a long negated, a
    /// string negated byte by byte, the reader's alone
    /// ([`Operated::Made`]), and a double through the long it is exactly
    /// worth. Nothing for a constant the guard refuses.
    fn bw_not_made(&mut self, value: Zval) -> Result<Option<Operated>, CompileError> {
        if let Some(s) = value.as_str() {
            let negated: Vec<u8> = s.bytes().iter().map(|b| !b).collect();
            return self
                .string_made(&negated, false, Sizing::Alloc)
                .map(|made| Some(Operated::Made(made)));
        }
        if let Some(n) = value.as_long() {
            return Ok(Some(Operated::Kept(Zval::long(!n))));
        }
        if let Some(d) = value.as_double() {
            let long = long_compatible(Number::Double(d));
            return Ok(long.map(|l| Operated::Kept(Zval::long(!l))));
        }
        Ok(None)
    }

    /// `zend_compile_silence` (`Z/Zend/zend_compile.c:10896`):
    /// `BEGIN_SILENCE` into a temporary (`zend_emit_op_tmp`), the
    /// expression, and `END_SILENCE` over that temporary with nothing
    /// left. A variable as the expression, `$a` or `$this`, is
    /// `zend_compile_simple_var_no_cv` under `BP_VAR_R`: `FETCH_R` over
    /// the name's constant string, `ZEND_FETCH_LOCAL` in
    /// `extended_value`, into a temporary
    /// (`zend_adjust_for_fetch_type`), "otherwise the CV access will
    /// happen outside the silenced section"; `$_GET` and its kin are the
    /// same fetch with `ZEND_FETCH_GLOBAL`. The
    /// value is where the expression left it, and its `zend_do_free`
    /// steps back over the `END_SILENCE` ([`Self::last_that_left_a_value`]).
    ///
    /// ```php
    /// var_dump(@$nothing, @$_GET, @$_SERVER['nothing']);
    /// ```
    /// ```expect
    /// NULL
    /// array(0) {
    /// }
    /// NULL
    /// ```
    fn silence(&mut self, operand: tree::Expr) -> Result<Where, CompileError> {
        let silence = self.temporary();
        self.push(Opline {
            opcode: Opcode::BeginSilence,
            out_type: OperandKind::Tmp,
            out: silence,
            ..Opline::default()
        });
        let line = self.line_of(operand);
        let value = match self.peel(operand) {
            tree::Expr::SimpleVarExpr(id) => match self.simple_var_of(id, line)? {
                SimpleVar::Cv(name) => {
                    // `zend_compile_simple_var_no_cv` under `BP_VAR_R`,
                    // whose own deprecation asks nothing of the flag.
                    if name == HTTP_RESPONSE_HEADER {
                        self.raise(Level::Deprecated, HTTP_RESPONSE_HEADER_DEPRECATED);
                    }
                    self.fetch_by_name(name, Symbols::Local)?
                }
                SimpleVar::This => self.fetch_by_name(b"this", Symbols::Local)?,
                SimpleVar::Auto(global) => self.fetch_by_name(global.name(), Symbols::Global)?,
                // `zend_compile_simple_var_no_cv` itself, past the
                // `is_globals_fetch` arm: the name the table has not.
                SimpleVar::Globals => self.fetch_by_name(b"GLOBALS", Symbols::Global)?,
            },
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

    /// `zend_compile_simple_var_no_cv` under `BP_VAR_R` for a name
    /// written out: `FETCH_R` over the name's constant from `table` into
    /// a temporary.
    fn fetch_by_name(&mut self, name: &[u8], table: Symbols) -> Result<Where, CompileError> {
        let name = self.string(name)?;
        Ok(self.fetch_r(Where::Constant(name), table))
    }

    /// `zend_compile_simple_var_no_cv`'s opline: `FETCH_R` over the
    /// name, `ZEND_FETCH_LOCAL` or `ZEND_FETCH_GLOBAL` in
    /// `extended_value` by the table, into the temporary
    /// `zend_adjust_for_fetch_type` makes of it under `BP_VAR_R`.
    fn fetch_r(&mut self, name: Where, table: Symbols) -> Where {
        let (op1_type, op1) = self.operand(name);
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::FetchR,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            extended_value: table.fetch_type(),
            ..Opline::default()
        });
        Where::Temporary(out)
    }

    /// `zend_compile_simple_var` under `BP_VAR_R` for a `ZEND_AST_VAR`
    /// whose name is an expression, `"${expr}"`: where php's parser had
    /// the expression as one value ([`Self::written`]), a string
    /// `this` is `FETCH_THIS` (`is_this_fetch`), an auto-global's name
    /// is `FETCH_R (global)` over it (`zend_try_compile_cv` refuses it),
    /// and any other value is the CV its string names, so `${'a'}` and
    /// `${1}` are `$a` and `$1` with no opline. Otherwise
    /// `zend_compile_simple_var_no_cv`: the expression compiled, a
    /// constant converted to a string, and `FETCH_R (local)` over it
    /// into a temporary ([`by_name`]). `${'GLOBALS'}` is `$GLOBALS`,
    /// `FETCH_GLOBALS` into a temporary.
    ///
    /// ```php
    /// $a = 5; $ab = 7;
    /// echo "${'a'} ${1} ${'a' . 'b'}", PHP_EOL;
    /// ```
    /// ```expect
    /// Deprecated: Using ${expr} (variable variables) in strings is deprecated, use {${expr}} instead in %s on line 3
    ///
    /// Deprecated: Using ${expr} (variable variables) in strings is deprecated, use {${expr}} instead in %s on line 3
    ///
    /// Deprecated: Using ${expr} (variable variables) in strings is deprecated, use {${expr}} instead in %s on line 3
    ///
    /// Warning: Undefined variable $1 in %s on line 3
    /// 5  7
    /// ```
    fn var_var(&mut self, name: tree::Expr) -> Result<Where, CompileError> {
        Ok(match self.named_by_expr(name)? {
            by_name::Named::Cv(name) => Where::Cv(self.lookup_cv(&name)),
            by_name::Named::This => self.fetch_this(OperandKind::Tmp),
            by_name::Named::Globals => self.fetch_globals(OperandKind::Tmp),
            by_name::Named::Fetch { name, table } => self.fetch_r(name, table),
        })
    }

    /// The pieces of a `"…"` or a heredoc, php's `encaps_list`: the
    /// written runs as the strings the scanner made of them, and each
    /// variable as what it is, with how it was written, php's `attr` on
    /// the `encaps_var`, which [`Self::encaps`] reads for its two
    /// deprecations. Nothing is compiled here: a piece is read and
    /// [`Self::encaps`] compiles it where the rope wants it.
    fn pieces(
        &mut self,
        parts: &[tree::StringPart],
        holder: Holder<'_>,
        line: u32,
    ) -> Result<&'a [Piece<'a>], CompileError> {
        let mut pieces = self.arena.vec(parts.len());
        let mut bytes = Vec::new();
        let mut at = line;
        for part in parts {
            match *part {
                tree::StringPart::LiteralStringPart(id) => {
                    let node = &self.parsed.tree[id];
                    if bytes.is_empty() {
                        at = node.line();
                    }
                    if let Some(TokenValue::Bytes(text)) = node
                        .text
                        .and_then(|text| TokenValue::of(self.parsed, holder, text))
                    {
                        bytes.extend(text);
                    }
                }
                tree::StringPart::SimpleInterpolation(id) => {
                    let node = &self.parsed.tree[id];
                    let (var, suffix, var_line) = (node.var, node.suffix, node.line());
                    let name = self.variable_name(var);
                    let fetch = match suffix {
                        None => Fetched::Var(name),
                        Some(tree::SimpleInterpolationSuffix::SimpleDim(dim)) => {
                            Fetched::Dim(name, self.simple_dim(dim)?)
                        }
                        Some(tree::SimpleInterpolationSuffix::SimpleProp(prop)) => {
                            let prop = &self.parsed.tree[prop];
                            let nullsafe =
                                prop.op.map(|op| self.kind(op)) == Some(SyntaxKind::NULLSAFE_ARROW);
                            Fetched::Prop(name, self.text(prop.name), nullsafe)
                        }
                    };
                    self.piece(&mut pieces, &mut bytes, at, fetch, var_line, Written::Plain);
                }
                tree::StringPart::BracedInterpolation(id) => {
                    let node = &self.parsed.tree[id];
                    let (inner, inner_line) = (node.inner, node.line());
                    let Some(inner) = inner else {
                        return Err(walk::unread(inner_line, "an interpolated expression"));
                    };
                    self.piece(
                        &mut pieces,
                        &mut bytes,
                        at,
                        Fetched::Node(inner),
                        inner_line,
                        Written::Plain,
                    );
                }
                tree::StringPart::DollarBraceInterpolation(id) => {
                    let node = &self.parsed.tree[id];
                    let (name, inner, dim, inner_line) =
                        (node.name, node.inner, node.dim, node.line());
                    // The lexer's `T_STRING_VARNAME` is a label right after
                    // the `${` and right before its `}` or `[`; one with
                    // whitespace around it is an expression, a constant's
                    // name, which names the variable by its value.
                    let opened = node.range.start;
                    let name = name.filter(|name| {
                        self.parsed
                            .tokens
                            .get(name.0 as usize)
                            .is_some_and(|token| {
                                token.offset == opened + 2
                                    && matches!(
                                        self.parsed.source.get((token.offset + token.len) as usize),
                                        Some(b'}' | b'[')
                                    )
                            })
                    });
                    let (fetch, written) = match (name, inner) {
                        (Some(name), _) => {
                            let name = self.text(name);
                            let fetch = match dim {
                                Some(dim) => Fetched::Dim(name, self.simple_dim(dim)?),
                                None => Fetched::Var(name),
                            };
                            (fetch, Written::DollarCurly)
                        }
                        (None, Some(inner)) => (Fetched::VarVar(inner), Written::DollarCurlyVarVar),
                        (None, None) => {
                            return Err(walk::unread(inner_line, "an interpolated expression"));
                        }
                    };
                    self.piece(&mut pieces, &mut bytes, at, fetch, inner_line, written);
                }
            }
        }
        if !bytes.is_empty() {
            pieces.push(Piece {
                fetch: Fetched::Text(self.arena.copy(&bytes)),
                line: at,
                written: Written::Plain,
            });
        }
        Ok(pieces.into_slice())
    }

    /// One variable piece, after the written bytes before it as a piece
    /// of their own at the line those bytes began on.
    /// The written bytes are laid in the arena and the room they were
    /// gathered in kept for the next.
    fn piece(
        &self,
        pieces: &mut ArenaVec<'a, Piece<'a>>,
        bytes: &mut Vec<u8>,
        at: u32,
        fetch: Fetched<'a>,
        line: u32,
        written: Written,
    ) {
        if !bytes.is_empty() {
            pieces.push(Piece {
                fetch: Fetched::Text(self.arena.copy(bytes)),
                line: at,
                written: Written::Plain,
            });
            bytes.clear();
        }
        pieces.push(Piece {
            fetch,
            line,
            written,
        });
    }

    /// The `[…]` after a variable inside a string, php's
    /// `encaps_var_offset`: a bare word is the string, a number is
    /// `T_NUM_STRING`'s value ([`Holder::Offset`]) and `-` before it is
    /// `zend_negate_num_string`, a variable is itself.
    fn simple_dim(&mut self, id: NodeId<tree::SimpleDim>) -> Result<Offset<'a>, CompileError> {
        let node = &self.parsed.tree[id];
        let line = node.line();
        let Some(index) = node.index else {
            return Err(walk::unread(line, "a dimension in a string"));
        };
        match self.kind(index) {
            SyntaxKind::VARIABLE => Ok(Offset::Var(self.variable_name(index))),
            SyntaxKind::IDENT => {
                let text = self.text(index).to_vec();
                Ok(Offset::Value(self.string(&text)?))
            }
            SyntaxKind::ENCAPS_INT | SyntaxKind::INT => {
                let value = TokenValue::of(self.parsed, Holder::Offset(node), index);
                let value = match (value, node.neg.is_some()) {
                    (Some(TokenValue::Int(0)), true) => self.string(b"-0")?,
                    (Some(TokenValue::Int(n)), true) => Zval::long(-n),
                    (Some(TokenValue::Int(n)), false) => Zval::long(n),
                    (Some(TokenValue::Bytes(digits)), true) => {
                        let mut bytes = vec![b'-'];
                        bytes.extend(digits);
                        self.string(&bytes)?
                    }
                    (Some(TokenValue::Bytes(digits)), false) => self.string(&digits)?,
                    _ => return Err(walk::unread(line, "a dimension in a string")),
                };
                Ok(Offset::Value(value))
            }
            _ => Err(walk::unread(line, "a dimension in a string")),
        }
    }

    /// One piece compiled where the rope wants it: the written bytes as
    /// the constant they are, a variable as its CV, `$this` as
    /// `FETCH_THIS`, and the two suffixes the in-string grammar takes
    /// as the one `FETCH_DIM_R` or `FETCH_OBJ_R` over it, which is what
    /// `zend_compile_encaps_var_offset` and `zend_compile_prop` leave
    /// there; a property's `$this` is the receiver
    /// ([`Self::this_receiver`]), `THIS` inside a method.
    ///
    /// ```php
    /// class C {
    ///     public $p = 'p';
    ///     function m() { return "$this->p {$this->p} $this?->p"; }
    ///     static function s() { return "$this->p"; }
    /// }
    /// echo (new C)->m(), "\n";
    /// $f = function () { return "[$this->p]"; };
    /// echo Closure::bind($f, new C, C::class)(), "\n";
    /// try { C::s(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// p p p
    /// [p]
    /// Using $this when not in object context
    /// ```
    fn fetched(&mut self, fetch: &Fetched) -> Result<Where, CompileError> {
        match fetch {
            Fetched::Text(bytes) => Ok(Where::Constant(self.string(bytes)?)),
            Fetched::Var(name) => self.in_string_var(name),
            // `"$GLOBALS[k]"`: the fetch by the offset's name from
            // `EG(symbol_table)`, `zend_delayed_compile_dim`'s first arm.
            Fetched::Dim(name, index) if name == b"GLOBALS" => {
                let name = match index {
                    Offset::Value(value) => {
                        Where::Constant(self.stringified(*value)?.unwrap_or(*value))
                    }
                    Offset::Var(name) => self.in_string_var(name)?,
                };
                Ok(self.fetch_r(name, Symbols::Global))
            }
            Fetched::Dim(name, index) => {
                // `zend_compile_dim` under `BP_VAR_R`: an auto-global's
                // fetch by name held back while the offset compiles.
                let mut container = match AutoGlobal::of(name) {
                    Some(global) => {
                        self.arm(global);
                        let name = self.string(name)?;
                        self.held_by_name(Where::Constant(name), Symbols::Global)
                    }
                    None => Base::Settled(self.in_string_operand(name)?),
                };
                self.hold_base(&mut container);
                let index = match index {
                    Offset::Value(value) => Where::Constant(*value),
                    Offset::Var(name) => self.in_string_var(name)?,
                };
                let (kind, at) = self.dim_operand(index);
                let out = self.temporary();
                let link = DimLink {
                    kind,
                    at,
                    out,
                    held: self.held_back(),
                };
                match self.dim_fetches(container, &[link], Fetch::Read, Over::Dim) {
                    (OperandKind::Tmp, at) => Ok(Where::Temporary(at)),
                    _ => Err(self.fatal("a dimension read with no dimension")),
                }
            }
            // `"$a?->b"`, php's `ZEND_AST_NULLSAFE_PROP` inside
            // `zend_compile_encaps_list`, is its own chain: the
            // `JMP_NULL` over the variable and the commit after the fetch.
            // `"$this->b"` is `zend_delayed_compile_prop`'s receiver, which
            // needs no `JMP_NULL`, since it throws where `$this` is not.
            Fetched::Prop(name, prop, nullsafe) => {
                let checkpoint = self.short_circuits.len();
                let (op1_type, op1) = if name == b"this" {
                    self.this_receiver()
                } else {
                    let receiver = self.in_string_operand(name)?;
                    if *nullsafe {
                        self.jmp_null(receiver, false);
                    }
                    receiver
                };
                let name = self.string(prop)?;
                let name_at = self.literal(name);
                let out = self.temporary();
                let slots = self.cache_slots(3);
                // `zend_compile_prop`'s one held-back fetch, laid down at once.
                drop(self.held_back());
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
                let value = Where::Temporary(out);
                self.short_circuit_commit(checkpoint, value, chain::ChainEnd::Expr);
                Ok(value)
            }
            Fetched::Node(expr) => self.expr(*expr),
            Fetched::VarVar(expr) => self.var_var(*expr),
        }
    }

    /// A variable named inside a string: the CV, `$this` fetched by
    /// `FETCH_THIS`, and an auto-global `FETCH_R (global)` over its name.
    ///
    /// ```php
    /// $_SERVER['who'] = 'w';
    /// $k = 'who';
    /// echo "$_SERVER[who] {$_SERVER['who']} $_SERVER[$k] $_SERVER\n";
    /// ```
    /// ```expect
    /// Warning: Array to string conversion in %s on line 4
    /// w w w Array
    /// ```
    fn in_string_var(&mut self, name: &[u8]) -> Result<Where, CompileError> {
        if name == b"this" {
            return Ok(self.fetch_this(OperandKind::Tmp));
        }
        if name == b"GLOBALS" {
            return Ok(self.fetch_globals(OperandKind::Tmp));
        }
        if let Some(global) = AutoGlobal::of(name) {
            self.arm(global);
            return self.fetch_by_name(name, Symbols::Global);
        }
        Ok(Where::Cv(self.lookup_cv(name)))
    }

    /// The operand a variable named inside a string stands as under a
    /// fetch.
    fn in_string_operand(&mut self, name: &[u8]) -> Result<(OperandKind, u32), CompileError> {
        let value = self.in_string_var(name)?;
        Ok(self.operand(value))
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
    /// piece is the one string php's parser made of it, a `ZEND_AST_ZVAL`
    /// and no list: the node's own claim ([`Self::claimed`]), which a
    /// fold over it gives back ([`Self::operands_folded`]) and every
    /// other reader keeps.
    ///
    /// A dimension piece is `zend_compile_dim` under `BP_VAR_R` and a
    /// property piece `zend_compile_prop`, each piece's oplines before
    /// the `ROPE_ADD` that takes its value; the offset is what
    /// `encaps_var_offset` read, `0` an integer, `-1` its negation,
    /// `k` a string and `$i` the CV:
    ///
    /// ```php
    /// $a = [1, 'k' => 'v', -1 => 'm']; $o = new stdClass; $o->p = 'q'; $i = 0;
    /// echo "$a[0]-$a[k]-$a[-1]-$a[$i]-$o->p-{$a[0]}-{$o->p}-${a[0]}", PHP_EOL;
    /// ```
    /// ```expect
    /// Deprecated: Using ${var} in strings is deprecated, use {$var} instead in %s on line 3
    /// 1-v-m-1-q-1-q-1
    /// ```
    ///
    /// A name with whitespace around it inside `${ }` is no
    /// `T_STRING_VARNAME` but a constant, whose value names the variable:
    ///
    /// ```php
    /// define("FOO", "a"); $a = 1;
    /// echo "${ FOO }", PHP_EOL;
    /// ```
    /// ```expect
    /// Deprecated: Using ${expr} (variable variables) in strings is deprecated, use {${expr}} instead in %s on line 3
    /// 1
    /// ```
    fn encaps(
        &mut self,
        node: tree::Expr,
        parts: &[tree::StringPart],
        holder: Holder<'_>,
        line: u32,
    ) -> Result<Where, CompileError> {
        let pieces = self.pieces(parts, holder, line)?;
        if let [
            Piece {
                fetch: Fetched::Text(bytes),
                line: text_line,
                written: Written::Plain,
            },
        ] = pieces
        {
            self.line = *text_line;
            let made = self.string_made(bytes, false, Sizing::Alloc)?;
            return Ok(Where::Constant(self.claimed(node, Operated::Made(made))));
        }
        let mut last_const: Option<Zval> = None;
        let mut reserved: Option<usize> = None;
        let mut init_at: Option<usize> = None;
        let mut count = 0u32;
        // `zend_compile_encaps_list` deprecates a piece before it compiles
        // it, at the line the piece before it left, the list's own for
        // the first.
        let mut left = line;
        for piece in pieces {
            self.line = left;
            match piece.written {
                Written::Plain => {}
                Written::DollarCurly => self.raise(
                    Level::Deprecated,
                    "Using ${var} in strings is deprecated, use {$var} instead",
                ),
                Written::DollarCurlyVarVar => self.raise(
                    Level::Deprecated,
                    "Using ${expr} (variable variables) in strings is deprecated, use {${expr}} instead",
                ),
            }
            self.line = piece.line;
            let value = self.fetched(&piece.fetch)?;
            left = self.line;
            if let Where::Constant(value) = value {
                let value = self.stringified(value)?.unwrap_or(value);
                if value.as_str().is_some_and(|s| s.bytes().is_empty()) {
                    continue;
                }
                last_const = Some(if let Some(held) = last_const.take() {
                    self.concat_fold(held, value, ConcatFold::Compiler)?
                        .unwrap_or(value)
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

    /// `zend_compile_rope_add`: one more opline, `ROPE_INIT` for element
    /// 0 and `ROPE_ADD` after, taking `element` as element `num`, and
    /// where the opline stands, which the first element's caller keeps
    /// for [`Self::rope_finalize`].
    pub(crate) fn rope_add(&mut self, num: u32, element: Where) -> usize {
        let at = self.oplines.len();
        self.push(Opline::default());
        self.rope_add_at(at, num, element);
        at
    }

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
    pub(crate) fn rope_finalize(&mut self, count: u32, init_at: usize) -> Where {
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
    fn cast(&mut self, to: CastTo, operand: tree::Expr) -> Result<Where, CompileError> {
        let operand = self.expr(operand)?;
        Ok(self.emit_cast(operand, to))
    }

    /// `operand` converted to `to` into a temporary: `ZEND_BOOL` for a
    /// bool and `ZEND_CAST` with the type in `extended_value` for the
    /// rest. What `zend_compile_cast` emits, what `zend_compile_func_cast`
    /// emits for `boolval`, `intval`, `floatval` and `strval`, and what
    /// `zend_compile_func_sprintf` emits before a `%d` and an array's `%s`.
    pub(crate) fn emit_cast(&mut self, operand: Where, to: CastTo) -> Where {
        match to {
            CastTo::Bool => self.emit_unary(Opcode::Bool, operand, 0),
            _ => self.emit_unary(Opcode::Cast, operand, to.type_code()),
        }
    }

    /// `zend_emit_op_tmp(result, opcode, NULL, NULL)`: the opline with
    /// no operand and a temporary for what it leaves.
    pub(crate) fn emit_nullary(&mut self, opcode: Opcode) -> Where {
        let out = self.temporary();
        self.push(Opline {
            opcode,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        Where::Temporary(out)
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

    /// `zend_emit_op_tmp`: the opline with both operands,
    /// `extended_value` as the caller sets it after the emission, and a
    /// temporary for what it leaves, which is where the value is.
    pub(crate) fn emit_binary(
        &mut self,
        opcode: Opcode,
        left: Where,
        right: Where,
        extended_value: u32,
    ) -> Where {
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
            extended_value,
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
            BinaryOp::BoolXor => Opcode::BoolXor,
            BinaryOp::BwOr => Opcode::BwOr,
            BinaryOp::BwXor => Opcode::BwXor,
            BinaryOp::BwAnd => Opcode::BwAnd,
        }
    }

    /// php's constant expression: the value `zend_compile_expr` leaves as
    /// an `IS_CONST` operand, folding a number as written, a persistent
    /// constant (`zend_try_ct_eval_const`), arithmetic over two of them
    /// (`zend_try_ct_eval_binary_op`) and a call of a name in
    /// [`crate::special`]'s table over constants
    /// (`zend_try_compile_special_func`).
    ///
    /// A string with a variable in it folds nothing and lays no opline
    /// down: the array it stands in compiles it once, where the element
    /// wants it, and a rope in a loop writes its temporary afresh each
    /// time round.
    ///
    /// ```php
    /// function f(array $a, &$p) { return 1; }
    /// for ($i = 0; $i < 3; $i++) {
    ///     $r[] = f(["x", "-r", "a $i b $i"], $pipes);
    /// }
    /// echo "done\n";
    /// ```
    /// ```expect
    /// done
    /// ```
    ///
    /// Every fold of a nested expression passes through this frame once
    /// per level, as a compile passes through [`Self::expr_node`] and
    /// php's through `zend_eval_const_expr`, so it holds the dispatch and
    /// nothing else: each arm is one call whose answer is this function's.
    pub(crate) fn constant_expr(&mut self, expr: tree::Expr) -> Result<Zval, Unfolded> {
        if let Some(evaluated) = self.evaluated(expr) {
            return evaluated;
        }
        // `zend_eval_const_expr` checks the stack and leaves
        // `CG(zend_lineno)` where it stands: the fold emits no opline,
        // so the line the compiler is on is the one the opline that
        // does emit was given.
        let line = expr.line(self.parsed);
        self.check_stack()?;
        let eval_const_expr = self.fold == Fold::EvalConstExpr;
        let folded = match expr {
            tree::Expr::ParenExpr(id) => self.paren_constant(id),
            tree::Expr::LiteralExpr(id) => self.literal_constant(id),
            tree::Expr::MagicConstExpr(id) => Ok(self.magic_node(id)?),
            tree::Expr::NowdocExpr(id) => self.nowdoc_constant(id),
            tree::Expr::NameRef(token) => self.name_constant(token),
            tree::Expr::InterpolatedStringExpr(_) | tree::Expr::HeredocExpr(_) => {
                self.string_constant(expr)
            }
            // `zend_eval_const_expr` has no `ZEND_AST_SHELL_EXEC` case,
            // so the node stands and [`Self::expr`] compiles it. It is
            // no constant and no `cannot read yet`, so a constant
            // expression a declaration keeps ([`Self::const_expr`]) is
            // refused in php's own words by [`Self::const_refused`].
            tree::Expr::ShellExecExpr(_) => Err(Unfolded::NoConstant(
                "a backtick operator is no constant expression",
            )),
            tree::Expr::UnaryExpr(id) => self.unary_constant(id),
            // `zend_compile_cast` folds nothing, which [`Self::cast`]
            // copies: under a compile the cast is its opline, a `TMP`,
            // and no fold over it happens, so `-(float)"0"` is the `CAST`
            // and the `MUL` over it.
            tree::Expr::CastExpr(id) if eval_const_expr => self.cast_constant(id, line),
            tree::Expr::CastExpr(_) => Err(Unfolded::NoConstant("a cast compiled is its opline")),
            tree::Expr::BinaryExpr(id) => self.operator_constant(id),
            tree::Expr::ArrayExpr(id) => self.array_constant(id),
            tree::Expr::ListExpr(_) => Err(self
                .compile_error("Cannot use list() as standalone expression")
                .into()),
            // `zend_is_allowed_in_const_expr` allows `ZEND_AST_CALL`
            // for the first-class callable alone, `f(...)`, which folds
            // to nothing and is kept as the tree
            // ([`Self::const_callable`]).
            tree::Expr::CallExpr(_) if eval_const_expr => {
                Err(Unfolded::NoConstant("a call is no constant expression"))
            }
            tree::Expr::CallExpr(id) => self.call_constant(id, line),
            tree::Expr::CloneExpr(_) | tree::Expr::ExitExpr(_) => {
                Err(Unfolded::NoConstant("a call is no constant expression"))
            }
            tree::Expr::NewExpr(id) => self.new_constant(id),
            tree::Expr::SimpleVarExpr(_)
            | tree::Expr::VarVarExpr(_)
            | tree::Expr::DynamicVarExpr(_)
            | tree::Expr::AssignExpr(_)
            | tree::Expr::PrefixIncDecExpr(_)
            | tree::Expr::PostfixIncDecExpr(_) => {
                Err(Unfolded::NoConstant("a variable is no constant expression"))
            }
            // `zend_eval_const_expr` folds no property fetch, and
            // `zend_compile_prop` emits its fetch.
            tree::Expr::MemberAccessExpr(id) => self.member_constant(id),
            // `zend_eval_const_expr`'s `ZEND_AST_DIM` arm
            // ([`Self::dimension_fold`]); `zend_compile_dim` folds
            // nothing.
            tree::Expr::SubscriptExpr(id) if eval_const_expr => self.subscript_constant(id),
            tree::Expr::SubscriptExpr(_) => Err(Unfolded::NoConstant(
                "a dimension is read as the program serves",
            )),
            tree::Expr::ScopedAccessExpr(id) => self.scoped_constant(id, line),
            tree::Expr::MatchExpr(_) => {
                Err(Unfolded::NoConstant("a match is no constant expression"))
            }
            tree::Expr::ErrorSuppressExpr(_) => Err(Unfolded::NoConstant(
                "a silenced expression is no constant expression",
            )),
            tree::Expr::InstanceofExpr(_) => Err(Unfolded::NoConstant(
                "an instanceof is no constant expression",
            )),
            // `zend_compile_conditional` folds nothing.
            tree::Expr::TernaryExpr(id) if eval_const_expr => self.ternary_constant(id),
            tree::Expr::TernaryExpr(_) => Err(Unfolded::NoConstant(
                "a conditional is folded by no compile of it",
            )),
            // `zend_compile_throw` emits an opline whatever stands
            // around it, so a `throw` folds to nothing, and
            // `zend_compile_include_or_eval` emits its opline the same
            // way for a file and for an `eval`;
            // `zend_compile_isset_or_empty` emits one over the CV.
            tree::Expr::ThrowExpr(_) => {
                Err(Unfolded::NoConstant("a `throw` is no constant expression"))
            }
            tree::Expr::IncludeExpr(_) => Err(Unfolded::NoConstant(
                "an `include` is no constant expression",
            )),
            tree::Expr::IssetExpr(_) | tree::Expr::EmptyExpr(_) => {
                Err(Unfolded::NoConstant("an `isset` is no constant expression"))
            }
            tree::Expr::ClosureExpr(_) | tree::Expr::ArrowFnExpr(_) => {
                Err(Unfolded::NoConstant("a closure is no constant expression"))
            }
            // `zend_compile_print` emits its `ECHO` whatever stands
            // around it, so a `print` folds to nothing.
            tree::Expr::PrintExpr(_) => {
                Err(Unfolded::NoConstant("a `print` is no constant expression"))
            }
            tree::Expr::EvalExpr(_) => {
                Err(Unfolded::NoConstant("an `eval` is no constant expression"))
            }
            // `zend_compile_yield` emits its opline whatever stands
            // around it.
            tree::Expr::YieldExpr(_) | tree::Expr::YieldFromExpr(_) => {
                Err(Unfolded::NoConstant("a `yield` is no constant expression"))
            }
        };
        self.record_fold(expr, &folded);
        folded
    }

    /// `'(' expr ')'`, which php's grammar drops: the inner node folded.
    fn paren_constant(&mut self, id: NodeId<tree::ParenExpr>) -> Result<Zval, Unfolded> {
        match self.parsed.tree[id].inner {
            Some(inner) => self.constant_expr(inner),
            None => Err(self.unread("an expression inside parentheses").into()),
        }
    }

    /// `ZEND_AST_ZVAL` written as a number or a quoted string.
    fn literal_constant(&mut self, id: NodeId<tree::LiteralExpr>) -> Result<Zval, Unfolded> {
        let node = &self.parsed.tree[id];
        let Some(token) = node.value else {
            return Err(self.unread("a literal").into());
        };
        match TokenValue::of(self.parsed, Holder::Scalar(node), token) {
            Some(TokenValue::Int(n)) => Ok(Zval::long(n)),
            Some(TokenValue::Float(f)) => Ok(Zval::double(f)),
            // The string php's scanner made, which `zend_ast_destroy`
            // frees once a fold over it stands.
            Some(TokenValue::Bytes(bytes)) => {
                let made = self.string_made(&bytes, false, Sizing::Alloc)?;
                Ok(self.claimed(tree::Expr::LiteralExpr(id), Operated::Made(made)))
            }
            None => Err(self.unread("a literal").into()),
        }
    }

    /// `ZEND_AST_ZVAL` written as a nowdoc.
    fn nowdoc_constant(&mut self, id: NodeId<tree::NowdocExpr>) -> Result<Zval, Unfolded> {
        let node = &self.parsed.tree[id];
        let mut bytes = Vec::new();
        for token in self.list(node.text) {
            if let Some(TokenValue::Bytes(text)) =
                TokenValue::of(self.parsed, Holder::Nowdoc(node), *token)
            {
                bytes.extend(text);
            }
        }
        let made = self.string_made(&bytes, false, Sizing::Alloc)?;
        Ok(self.claimed(tree::Expr::NowdocExpr(id), Operated::Made(made)))
    }

    /// `ZEND_AST_CONST`: what [`Self::named_constant`] substitutes, and
    /// no constant where it substitutes nothing, since
    /// `zend_eval_const_expr` then leaves the node standing for the
    /// `FETCH_CONSTANT` the compile emits (`Z/Zend/zend_compile.c:12292`).
    fn name_constant(&mut self, token: TokenId) -> Result<Zval, Unfolded> {
        let folded = self.named_constant(token)?.ok_or(Unfolded::NoConstant(
            "a constant fetched as the program runs",
        ))?;
        Ok(self.claimed(tree::Expr::NameRef(token), folded))
    }

    /// A `"…"` or a heredoc with nothing interpolated is one string
    /// php's parser folded before the compiler saw it; one with a
    /// variable in it is `ZEND_AST_ENCAPS_LIST`, whose oplines no fold
    /// replaces and `zend_eval_const_expr` leaves standing uncompiled,
    /// its `default` arm. Only the folded kind is compiled here, which
    /// lays no opline down: compiling the other left its oplines behind
    /// once for every fold that asked, each a temporary written and
    /// never read. Its line is the parser's and no compile's, so the
    /// compile that reads it leaves `CG(zend_lineno)` where it stood.
    fn string_constant(&mut self, expr: tree::Expr) -> Result<Zval, Unfolded> {
        let tree = &self.parsed.tree;
        let (parts, holder): (&[tree::StringPart], Holder<'_>) = match expr {
            tree::Expr::InterpolatedStringExpr(id) => {
                (tree.list(tree[id].parts), Holder::DoubleQuoted(&tree[id]))
            }
            tree::Expr::HeredocExpr(id) => (tree.list(tree[id].parts), Holder::Heredoc(&tree[id])),
            _ => return Err(self.unread("a string").into()),
        };
        let mut bytes = Vec::new();
        for part in parts {
            let tree::StringPart::LiteralStringPart(id) = *part else {
                return Err(Unfolded::NoConstant(
                    "an interpolated string is no constant expression",
                ));
            };
            if let Some(TokenValue::Bytes(text)) =
                (tree[id].text).and_then(|text| TokenValue::of(self.parsed, holder, text))
            {
                bytes.extend(text);
            }
        }
        // The one string the parser made of it, which `zend_ast_destroy`
        // frees once a fold over it stands.
        let made = self.string_made(&bytes, false, Sizing::Alloc)?;
        Ok(self.claimed(expr, Operated::Made(made)))
    }

    /// `zend_try_ct_eval_unary_pm`: php folds unary `-` and `+` as the
    /// multiplication by -1 and by 1 it would otherwise emit. The
    /// `ZEND_AST_UNARY_OP` arm of `zend_eval_const_expr` folds the
    /// operand first, then `zend_try_ct_eval_unary_op`, which
    /// `zend_unary_op_produces_error` refuses over a double for
    /// `ZEND_BOOL_NOT`, since it warns casting a NAN, so `!1.5` is the
    /// opline php leaves standing and never the bool; the `ZEND_BW_NOT`
    /// arm refuses `null`, `false`, `true` and a double the long
    /// conversion would lose something of, and folds a string as it
    /// stands.
    fn unary_constant(&mut self, id: NodeId<tree::UnaryExpr>) -> Result<Zval, Unfolded> {
        let node = &self.parsed.tree[id];
        let operand = self.required(node.operand, "a unary operator")?;
        let Some(kind) = node.op.map(|op| self.kind(op)) else {
            return Err(self.unread("a unary operator").into());
        };
        self.fold_over(tree::Expr::UnaryExpr(id), [operand], |emit| {
            let value = emit.constant_expr(operand)?;
            emit.unary_operand(kind, value)
        })
    }

    /// The fold of [`Self::unary_constant`] over its folded operand:
    /// the string `~` makes of a string is the node's claim
    /// ([`Operated::Made`]), and every other value is kept.
    fn unary_operand(&mut self, kind: SyntaxKind, value: Zval) -> Result<Operated, Unfolded> {
        match kind {
            SyntaxKind::MINUS | SyntaxKind::PLUS => {
                let sign = Zval::long(if kind == SyntaxKind::MINUS { -1 } else { 1 });
                fold(BinaryOp::Mul, value, sign)
                    .map(Operated::Kept)
                    .ok_or(Unfolded::NoConstant(
                        "arithmetic this compiler cannot fold yet",
                    ))
            }
            SyntaxKind::BANG if value.as_double().is_some() => Err(Unfolded::NoConstant(
                "a `!` over a float is left to its opcode",
            )),
            SyntaxKind::BANG => Ok(Operated::Kept(Zval::bool(!truth(value)))),
            SyntaxKind::TILDE => self.bw_not_made(value)?.ok_or(Unfolded::NoConstant(
                "a `~` over this constant is left to its opcode",
            )),
            _ => Err(self.unread("a unary operator").into()),
        }
    }

    /// `zend_eval_const_expr`'s `ZEND_AST_CAST` arm: the operand folded,
    /// then `zend_try_ct_eval_cast` over it. `(unset)` is refused where
    /// it stands, as php's grammar puts it.
    fn cast_constant(&mut self, id: NodeId<tree::CastExpr>, line: u32) -> Result<Zval, Unfolded> {
        let node = &self.parsed.tree[id];
        let to = self.cast_to(node.op, line)?;
        let operand = self.required(node.operand, "a cast")?;
        self.fold_over(tree::Expr::CastExpr(id), [operand], |emit| {
            let value = emit.constant_expr(operand)?;
            emit.cast_fold(to, value)?
                .ok_or(Unfolded::NoConstant("a cast this compiler cannot fold"))
        })
    }

    /// `zend_eval_const_expr`'s `ZEND_AST_AND`, `ZEND_AST_OR`,
    /// `ZEND_AST_COALESCE` and `ZEND_AST_BINARY_OP` arms.
    fn operator_constant(&mut self, id: NodeId<tree::BinaryExpr>) -> Result<Zval, Unfolded> {
        let node = &self.parsed.tree[id];
        let left = node.lhs;
        let right = self.required(node.rhs, "a binary operator")?;
        match node.op.map(|op| self.kind(op)) {
            // `Z/Zend/zend_compile.c:12116`.
            Some(SyntaxKind::AMP_AMP | SyntaxKind::AND_KW) => {
                self.short_circuit_fold(id, false, left, right)
            }
            Some(SyntaxKind::PIPE_PIPE | SyntaxKind::OR_KW) => {
                self.short_circuit_fold(id, true, left, right)
            }
            Some(SyntaxKind::QUESTION_QUESTION) => self.coalesce_constant(id, left, right),
            Some(kind) => match binary_op(kind) {
                Some(op) => self.binary_constant(id, op, left, right),
                None => Err(self.unread("a binary operator").into()),
            },
            None => Err(self.unread("a binary operator").into()),
        }
    }

    /// `ZEND_AST_ARRAY` whose every element folds.
    fn array_constant(&mut self, id: NodeId<tree::ArrayExpr>) -> Result<Zval, Unfolded> {
        let elements = self.list(self.parsed.tree[id].array_elements);
        let made = self.constant_array(elements)?.ok_or(Unfolded::NoConstant(
            "an array expression that is no constant",
        ))?;
        Ok(self.claimed(tree::Expr::ArrayExpr(id), Operated::Made(made)))
    }

    /// A call of a name in [`crate::special`]'s table over constants
    /// (`zend_try_compile_special_func`), the arguments read before the
    /// callee.
    fn call_constant(&mut self, id: NodeId<tree::CallExpr>, line: u32) -> Result<Zval, Unfolded> {
        let node = &self.parsed.tree[id];
        let (callee, args) = (node.callee, node.args);
        let arguments = self.arguments(args, line)?;
        if let Args::CallableConvert = arguments {
            return Err(Unfolded::NoConstant(
                "a first-class callable folds to no constant",
            ));
        }
        let Some(arguments) = arguments.positional() else {
            return Err(walk::unread(line, "a named argument in a constant expression").into());
        };
        let tree::Expr::NameRef(name) = self.peel(callee) else {
            return Err(Unfolded::NoConstant("a call is no constant expression"));
        };
        let name = self.text(name).to_vec();
        let (resolved, runtime) = self.function_name(&name);
        let folded = if runtime {
            None
        } else {
            self.special_constant(self.arena.lowercase(resolved), &arguments)?
        };
        folded.ok_or(Unfolded::NoConstant("a call this compiler cannot fold"))
    }

    /// `zend_eval_const_expr`'s `ZEND_AST_DIM` arm.
    fn subscript_constant(&mut self, id: NodeId<tree::SubscriptExpr>) -> Result<Zval, Unfolded> {
        let node = &self.parsed.tree[id];
        let (base, index) = (node.base, node.index);
        let Some(index) = index else {
            return Err(self.compile_error("Cannot use [] for reading").into());
        };
        self.fold_over(tree::Expr::SubscriptExpr(id), [base, index], |emit| {
            emit.dimension_fold(base, index)
        })
    }

    /// What `zend_try_ct_eval_class_const` substitutes
    /// ([`Self::ct_class_constant`]), the node's claim where it is a
    /// string the request's constant holds, and any other constant
    /// fetched as the program serves; a class's property is a variable; and a
    /// class's name folds where
    /// `zend_try_compile_const_expr_resolve_class_name` answers.
    fn scoped_constant(
        &mut self,
        id: NodeId<tree::ScopedAccessExpr>,
        line: u32,
    ) -> Result<Zval, Unfolded> {
        match self.scoped_of(id, line)? {
            Scoped::Constant { class, name } => {
                let operand = self.evaluated_class(class)?;
                let folded = match self.folded_name(&name)? {
                    Some(name) => match self.constant_class(operand)? {
                        Some(class) => self.ct_class_constant(class, name)?,
                        None => None,
                    },
                    None => None,
                };
                let folded = folded.ok_or(Unfolded::NoConstant(
                    "a class constant is fetched at run time",
                ))?;
                Ok(self.claimed(tree::Expr::ScopedAccessExpr(id), folded))
            }
            Scoped::Property { .. } => {
                Err(Unfolded::NoConstant("a variable is no constant expression"))
            }
            Scoped::ClassName { class } => {
                let operand = self.decoded_class(class)?;
                self.resolved_class_name(operand)?
                    .map(Operated::kept)
                    .ok_or(Unfolded::NoConstant("a class name is fetched at run time"))
            }
        }
    }

    /// [`Self::constant_expr`]'s `ZEND_AST_BINARY_OP` arm, one fold per
    /// family: `zend_try_ct_eval_binary_op` over `concat_function`, over
    /// `bitwise_or_function` and its kin, over `zend_compare` and
    /// `zend_is_identical`, and over the arithmetic; each of the four
    /// refuses where php's opcode would throw or warn, which is the
    /// value left to the request. The steps are [`Self::fold_over`]'s
    /// spelled out: a closure here stands in the frame a left-nested
    /// chain repeats, 171 links under a 512K stack in a debug build where
    /// this reaches 206 (measured on #3240).
    fn binary_constant(
        &mut self,
        id: NodeId<tree::BinaryExpr>,
        op: BinaryOp,
        left: tree::Expr,
        right: tree::Expr,
    ) -> Result<Zval, Unfolded> {
        let mark = self.claims.mark();
        let folded = (self.constant_pair(left, right)).and_then(|(a, b)| {
            let fold = match op {
                BinaryOp::Concat => self.parser_folds(id, left, right),
                _ => ConcatFold::Compiler,
            };
            self.constant_operands(op, a, b, fold)
        });
        let claims = self.claims.since(mark);
        let folded = folded?;
        self.consume(claims, [left, right]);
        Ok(self.claimed(tree::Expr::BinaryExpr(id), folded))
    }

    /// What a node folded to, its string held as the node's own claim
    /// where the fold made it for the node alone ([`eval::Claims`]).
    fn claimed(&mut self, node: tree::Expr, folded: Operated) -> Zval {
        match folded {
            Operated::Kept(value) => value,
            Operated::Made(made) => {
                let value = made.value();
                self.claims.hold(node, made);
                value
            }
        }
    }

    /// `zend_try_ct_eval_binary_op` over two folded sides, the one fold
    /// `zend_eval_const_expr` ([`Self::binary_constant`]) and
    /// `zend_compile_binary_op` ([`Self::binary_operands`]) both make,
    /// out of the frame a left-nested chain stands in once per
    /// operator: the string a concatenation or a bitwise operator over
    /// two strings makes is the node's claim ([`Operated::Made`]), and
    /// every other value is kept. A `.` is folded as `fold` says, the
    /// parser's or the compiler's ([`Self::parser_folds`]).
    fn constant_operands(
        &mut self,
        op: BinaryOp,
        left: Zval,
        right: Zval,
        fold: ConcatFold,
    ) -> Result<Operated, Unfolded> {
        if op == BinaryOp::Concat {
            let made = self
                .concat_made(left, right, fold)?
                .ok_or(Unfolded::NoConstant(
                    "a concatenation this compiler cannot fold yet",
                ))?;
            return Ok(Operated::Made(made));
        }
        if op == BinaryOp::BoolXor {
            return Ok(Operated::Kept(self.bool_xor(left, right)));
        }
        if bitwise(op) {
            return self
                .bitwise_made(op, left, right)?
                .ok_or(Unfolded::NoConstant(
                    "a bitwise operator this compiler cannot fold yet",
                ));
        }
        if compares(op) {
            return Ok(Operated::Kept(self.compare_fold(op, left, right)));
        }
        self.arithmetic_fold(op, left, right)?
            .ok_or(Unfolded::NoConstant(
                "arithmetic this compiler cannot fold yet",
            ))
    }

    /// `boolean_xor_function` (`Z/Zend/zend_operators.c:1575`) under
    /// `zend_try_ct_eval_binary_op`: each side read through
    /// `zend_is_true` ([`Self::is_true`]), the left one first, whatever
    /// its kind. `zend_binary_op_produces_error` lets every pair
    /// through, so every pair of constants folds, and a `NAN` warns as
    /// the file compiles, once per side it stands on:
    ///
    /// ```php
    /// echo "start\n";
    /// const C = NAN xor 0;
    /// var_dump(NAN xor 1);
    /// var_dump(NAN xor NAN, [1] xor NAN, C);
    /// ```
    /// ```expect
    /// Warning: unexpected NAN value was coerced to bool in %SCRATCH%/witness.php on line 3
    ///
    /// Warning: unexpected NAN value was coerced to bool in %SCRATCH%/witness.php on line 4
    ///
    /// Warning: unexpected NAN value was coerced to bool in %SCRATCH%/witness.php on line 5
    ///
    /// Warning: unexpected NAN value was coerced to bool in %SCRATCH%/witness.php on line 5
    ///
    /// Warning: unexpected NAN value was coerced to bool in %SCRATCH%/witness.php on line 5
    /// start
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// ```
    fn bool_xor(&mut self, left: Zval, right: Zval) -> Zval {
        let left = self.is_true(Truth::of(left));
        let right = self.is_true(Truth::of(right));
        Zval::bool(left != right)
    }

    /// `zend_eval_const_expr`'s `ZEND_AST_DIM` arm
    /// (`Z/Zend/zend_compile.c:12225`), past the `[]` with nothing in it
    /// that [`Self::subscript_constant`] refuses as php's compile error:
    /// the container and the dimension folded, then an
    /// array's entry at an integer or a string key (`zend_symtable_find`,
    /// a decimal string read as the integer) and a string's byte at an
    /// integer offset inside it, as a one-byte string (`ZVAL_CHAR`). A key
    /// the array has not, an offset past the string and any other pair
    /// are left to the request, which raises what php raises for them.
    ///
    /// ```php
    /// class C { const A = ['k' => [1, 2]]['k'][1]; const B = 'abc'[1]; const D = [1][0] ?? 2; }
    /// var_dump(C::A, C::B, C::D, [[3, 4][1], 'xy'['1']]);
    /// ```
    /// ```expect
    /// int(2)
    /// string(1) "b"
    /// int(1)
    /// array(2) {
    ///   [0]=>
    ///   int(4)
    ///   [1]=>
    ///   string(1) "y"
    /// }
    /// ```
    ///
    /// The element is one more owner of what the array holds
    /// (`ZVAL_COPY_OR_DUP`), so an `eval`'s, whose array goes with its
    /// main op array, outlives it in the static that keeps it, the heap
    /// churned between:
    ///
    /// ```php
    /// eval('function f() { static $s = [[1, 2]][0]; return $s; }');
    /// eval('function g() { static $s = [["ab" . "cd", "k" => "efgh"]]["0"]["k"]; return $s; }');
    /// $junk = array_fill(0, 1000, str_repeat("x", 100));
    /// var_dump(f(), g());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    /// string(4) "efgh"
    /// ```
    fn dimension_fold(
        &mut self,
        base: tree::Expr,
        index: tree::Expr,
    ) -> Result<Operated, Unfolded> {
        let (container, dim) = self.constant_pair(base, index)?;
        let refused = || Unfolded::NoConstant("a dimension this compiler leaves to the request");
        if let Some(array) = container.as_arr() {
            let key = match (dim.as_long(), dim.as_str()) {
                (Some(n), _) => Key::Long(n),
                (None, Some(s)) => match php_heap::canonical_int(s.bytes()) {
                    Some(n) => Key::Long(n),
                    None => Key::Str(s),
                },
                (None, None) => return Err(refused()),
            };
            // `ZVAL_COPY_OR_DUP`: one more owner of the element, the
            // node's claim where it is a string or an array; else the
            // fetch.
            let element = array.get(key).ok_or_else(refused)?;
            if element.as_str().is_some() || element.as_arr().is_some() {
                return (self.strings.hold_made(element))
                    .map(Operated::Made)
                    .ok_or_else(refused);
            }
            return Ok(Operated::Kept(element));
        }
        if let Some(text) = container.as_str() {
            let offset = match (dim.as_long(), dim.as_str()) {
                (Some(n), _) => n,
                (None, Some(s)) => match numeric_prefix(s.bytes()) {
                    Some((Number::Long(n), _)) => n,
                    _ => return Err(refused()),
                },
                (None, None) => return Err(refused()),
            };
            let byte = usize::try_from(offset)
                .ok()
                .and_then(|at| text.bytes().get(at).copied());
            return match byte {
                Some(byte) => Ok(Operated::Kept(self.string(&[byte])?)),
                None => Err(refused()),
            };
        }
        Err(refused())
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
    ///
    /// The array is laid at the elements written, a repeated key among
    /// them, as `in_array`'s table is at the haystack's elements
    /// (`zend_new_array(zend_hash_num_elements(src))`):
    ///
    /// ```php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('$q1 = 1;'); m('function w1() {}');
    /// echo m('$GLOBALS["a"] = ["e1" => 1, "e2" => 2, "e3" => 3, "e4" => 4, "e5" => 5, "e6" => 6, "e7" => 7, "e8" => 8, "e1" => 9];'), " ";
    /// echo m('$GLOBALS["b"] = ["f1" => 1, "f2" => 2, "f3" => 3, "f4" => 4, "f5" => 5, "f6" => 6, "f7" => 7, "f8" => 8];'), "\n";
    /// echo m('function i9($x) { return in_array($x, ["g1", "g2", "g3", "g4", "g5", "g6", "g7", "g8", "g1"], true); }'), " ";
    /// echo m('function i8($x) { return in_array($x, ["h1", "h2", "h3", "h4", "h5", "h6", "h7", "h8"], true); }'), "\n";
    /// echo count($a), count($b), i9("g1") && i8("h8"), "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 1464 632
    /// 1304 984
    /// 881
    /// ```
    fn constant_array(
        &mut self,
        elements: &[NodeId<tree::ArrayElement>],
    ) -> Result<Option<Made>, CompileError> {
        let mark = self.claims.mark();
        let Some(entries) = self.interning(false, |this| this.constant_entries(elements))? else {
            return Ok(None);
        };
        let claims = (self.claims.since(mark).into_iter()).map(|(_, made)| made);
        (self.lay_array(&entries, claims.collect(), elements.len())).map(Some)
    }

    /// `zend_try_ct_eval_array` over an argument php's tree holds as
    /// `ZEND_AST_ARRAY`, an array expression under any parentheses: its
    /// entries as [`Self::constant_entries`] folds them, laid down
    /// nowhere, and nothing where the argument is any other node or
    /// folds to no constant. What `zend_compile_func_in_array` builds a
    /// table of its own from.
    pub(crate) fn array_entries(
        &mut self,
        expr: tree::Expr,
    ) -> Result<Option<Vec<(Key, Zval)>>, CompileError> {
        let tree::Expr::ArrayExpr(id) = self.peel(expr) else {
            return Ok(None);
        };
        let elements = self.list(self.parsed.tree[id].array_elements);
        self.interning(false, |this| this.constant_entries(elements))
    }

    /// [`Self::constant_array`]'s entries before they are laid down: the
    /// keys and values `zend_try_ct_eval_array` folds, in order, and
    /// nothing where it folds none. A `&` element is never constant, and
    /// a `...` is where what it unpacks folds to an array, whose string
    /// keys are written as they stand (`zend_hash_update`) and whose
    /// integer keys are dropped for the next index
    /// (`zend_hash_next_index_insert`); anything else unpacked is php's
    /// compile error.
    ///
    /// ```php
    /// const K = ['k' => 1];
    /// var_dump([...[5 => 'a', 'b'], ...K, 'k' => 2, ...['x' => 3, 9 => 'c']]);
    /// ```
    /// ```expect
    /// array(5) {
    ///   [0]=>
    ///   string(1) "a"
    ///   [1]=>
    ///   string(1) "b"
    ///   ["k"]=>
    ///   int(2)
    ///   ["x"]=>
    ///   int(3)
    ///   [2]=>
    ///   string(1) "c"
    /// }
    /// ```
    ///
    /// ```php
    /// var_dump([...1]);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Only arrays and Traversables can be unpacked, int given in %s on line 2
    /// ```
    fn constant_entries(
        &mut self,
        elements: &[NodeId<tree::ArrayElement>],
    ) -> Result<Option<Vec<(Key, Zval)>>, CompileError> {
        let elements = self.elements_of(elements)?;
        let mut folded: Vec<FoldedElement> = Vec::with_capacity(elements.len());
        let mut constant = true;
        for &element in elements {
            let (key, value, by_ref) = match element {
                Element::Value { key, value } => (key, value, false),
                Element::Ref { key, value } => (key, value, true),
                Element::Unpack(value) => {
                    match self.eval_const(value)? {
                        Some(value) => folded.push(FoldedElement::Unpack(value)),
                        None => constant = false,
                    }
                    continue;
                }
            };
            // `ZEND_AST_ARRAY_ELEM` holds the value first and the key
            // second, and php evaluates them in that order.
            let value = self.eval_const(value)?;
            let key = match key {
                Some(key) => {
                    let folded_key = self.eval_const(key)?;
                    constant &= folded_key.is_some();
                    folded_key
                }
                None => None,
            };
            match value {
                Some(value) if !by_ref => folded.push(FoldedElement::Pair(key, value)),
                _ => constant = false,
            }
        }
        if !constant {
            return Ok(None);
        }
        self.keyed_entries(folded)
    }

    /// `zend_try_ct_eval_array`'s second pass over elements whose every
    /// key and value folded: each key written as php writes it, a `...`
    /// spread, and nothing where a key leaves the array to the request.
    fn keyed_entries(
        &self,
        folded: Vec<FoldedElement>,
    ) -> Result<Option<Vec<(Key, Zval)>>, CompileError> {
        let mut entries: Vec<(Key, Zval)> = Vec::with_capacity(folded.len());
        let mut next_free = i64::MIN;
        for element in folded {
            let (key, value) = match element {
                FoldedElement::Unpack(value) => {
                    let Some(array) = value.as_arr() else {
                        return Err(self.compile_error(format!(
                            "Only arrays and Traversables can be unpacked, {} given",
                            constant_value_name(&value)
                        )));
                    };
                    for (key, held) in &array {
                        let key = match key {
                            Key::Str(s) => Key::Str(s),
                            Key::Long(_) => match next_index(&entries, next_free) {
                                Some(key) => key,
                                None => return Ok(None),
                            },
                        };
                        write_entry(&mut entries, &mut next_free, key, held);
                    }
                    continue;
                }
                FoldedElement::Pair(key, value) => (key, value),
            };
            let key = match key {
                None => match next_index(&entries, next_free) {
                    Some(key) => key,
                    None => return Ok(None),
                },
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
            write_entry(&mut entries, &mut next_free, key, value);
        }
        Ok(Some(entries))
    }

    /// One constant array laid down in the home the unit's strings are
    /// in, at the `size` php inits it with, the elements written
    /// (`array_init_size(result, list->children)`), as the claim of the
    /// one who asked ([`Intern::constant_array_made`]), and the fatal
    /// php's allocator raises where there is no memory for it. The array takes one owner of each key and value over, as
    /// `zend_try_ct_eval_array` moves each element's zval in: the claim
    /// among `claims` an entry was made with, else one more owner of it
    /// ([`Intern::hold`]); a claim no entry holds, a numeric key's string
    /// or an unpacked array, is given back. So an array a fold reads and
    /// gives back takes its strings with it, and none joins opcache's
    /// buffer with the unit (each file written where the cache keeps it):
    ///
    /// ```php
    /// $n = fn () => opcache_get_status()['interned_strings_usage']['number_of_strings'];
    /// foreach (['$z = ["a#"] == "a#";', '$z = ["a#"] + ["c#"];', 'class X { const A = ["a#"] == "a#"; }', '$z = [["a#"]] == 1;', '$z = [...["a#"]] == 1;', 'class D { const A = ["k#" => "l#"]["k#"]; }', '$z = [["a#"]][0];', '$z = ["a#"] ?? 2;', 'class U { const A = (["a#"] + ["c#", "e#"]) == 1; }', 'class V { const A = [[["a#"]][0]] == 1; }'] as $i => $c) {
    ///     $c = str_replace('#', "q$i", $c);
    ///     $f = sys_get_temp_dir() . "/w3258-lay-$i.php";
    ///     file_put_contents("$f.tmp", "<?php $c");
    ///     touch("$f.tmp", time() - 10);
    ///     rename("$f.tmp", $f);
    ///     $a = $n();
    ///     include $f;
    ///     echo $n() - $a, '  ', $c, "\n";
    ///     unlink($f);
    /// }
    /// echo json_encode([X::A, D::A, $z, U::A, V::A]), "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 1  $z = ["aq0"] == "aq0";
    /// 2  $z = ["aq1"] + ["cq1"];
    /// 1  class X { const A = ["aq2"] == "aq2"; }
    /// 1  $z = [["aq3"]] == 1;
    /// 1  $z = [...["aq4"]] == 1;
    /// 2  class D { const A = ["kq5" => "lq5"]["kq5"]; }
    /// 2  $z = [["aq6"]][0];
    /// 2  $z = ["aq7"] ?? 2;
    /// 1  class U { const A = (["aq8"] + ["cq8", "eq8"]) == 1; }
    /// 1  class V { const A = [[["aq9"]][0]] == 1; }
    /// [false,"lq5",["aq7"],false,false]
    /// ```
    pub(crate) fn lay_array(
        &mut self,
        entries: &[(Key, Zval)],
        mut claims: Vec<Made>,
        size: usize,
    ) -> Result<Made, CompileError> {
        let values = entries.iter().flat_map(|(key, value)| {
            let key = match key {
                Key::Str(s) => Some(Zval::string(*s)),
                Key::Long(_) => None,
            };
            key.into_iter().chain([*value])
        });
        for value in values.filter(|z| z.as_str().is_some() || z.as_arr().is_some()) {
            match claims
                .iter()
                .position(|made| made.value().is_same_value(value))
            {
                Some(at) => drop(claims.swap_remove(at)),
                None => drop(self.strings.hold(value)),
            }
        }
        for made in claims {
            self.strings.give_back(made);
        }
        (self.strings.constant_array_made(entries, size)).map_err(|fatal| self.fatal_of(fatal))
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
    /// `INIT_ARRAY` once a constant string key is seen. A `&` element's
    /// value is the variable fetched for writing ([`Self::element_ref`])
    /// and its opline carries `ZEND_ARRAY_ELEMENT_REF`; a `...` is
    /// `ADD_ARRAY_UNPACK` over the value, and where it is the first
    /// element an `INIT_ARRAY` with no operand and no count stands
    /// before it.
    ///
    /// The `array(…)` spelling is the same node over the same
    /// `array_pair_list` (`Z/Zend/zend_language_parser.y:1491`), with
    /// `ZEND_ARRAY_SYNTAX_LONG` in `attr` where `[…]` has
    /// `ZEND_ARRAY_SYNTAX_SHORT`; `zend_compile_array` reads `attr`
    /// only to refuse a `list()` where an array is meant, so nothing
    /// here differs by spelling. The word is matched whatever its
    /// case, and `array` without the parenthesis stays the type name:
    ///
    /// ```php
    /// $k = 'k';
    /// function f(array $a): array { return $a; }
    /// var_dump(array(1, 'k' => 2), ARRAY(1, $k => 2), [1, 'k' => 2], f(array()));
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   ["k"]=>
    ///   int(2)
    /// }
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   ["k"]=>
    ///   int(2)
    /// }
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   ["k"]=>
    ///   int(2)
    /// }
    /// array(0) {
    /// }
    /// ```
    fn array(&mut self, ids: &[NodeId<tree::ArrayElement>]) -> Result<Where, CompileError> {
        let elements = self.elements_of(ids)?;
        let count = u32::try_from(elements.len()).unwrap_or(u32::MAX);
        let mut init_at = 0usize;
        let mut out = 0u32;
        let mut packed = true;
        for (i, &element) in elements.iter().enumerate() {
            let (key, value, by_ref) = match element {
                Element::Unpack(value) => {
                    let value = self.expr(value)?;
                    let (op1_type, op1) = self.operand(value);
                    if i == 0 {
                        out = self.temporary();
                        init_at = self.oplines.len();
                        self.push(Opline {
                            opcode: Opcode::InitArray,
                            out_type: OperandKind::Tmp,
                            out,
                            ..Opline::default()
                        });
                    }
                    self.push(Opline {
                        opcode: Opcode::AddArrayUnpack,
                        op1_type,
                        op1,
                        out_type: OperandKind::Tmp,
                        out,
                        ..Opline::default()
                    });
                    continue;
                }
                Element::Value { key, value } => (key, value, false),
                Element::Ref { key, value } => (key, value, true),
            };
            let key = match key {
                Some(key) => {
                    let key = self.expr(key)?;
                    Some(Self::numeric_op(key))
                }
                None => None,
            };
            let (op1_type, op1) = if by_ref {
                self.element_ref(value)?
            } else {
                let value = self.expr(value)?;
                self.operand(value)
            };
            let (op2_type, op2) = match key {
                Some(key) => self.operand(key),
                None => (OperandKind::Unused, 0),
            };
            let reference = if by_ref { ARRAY_ELEMENT_REF } else { 0 };
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
                    extended_value: (count << ARRAY_SIZE_SHIFT) | reference,
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
                    extended_value: reference,
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

    /// The value of a `&` element, `zend_ensure_writable_variable` and
    /// then `zend_compile_var(…, BP_VAR_W, 1)`: what a call returned is
    /// php's compile error, a plain variable is its CV, `$this` is
    /// `FETCH_THIS` into a `VAR`, `$GLOBALS` is php's compile error, and
    /// a dimension, a property, a class's property and a variable
    /// fetched by name are the write fetch of the whole chain with the
    /// `&`'s own bit on its last opline ([`Self::var_for_reference`]).
    /// The array's entry and the variable are one cell from there:
    ///
    /// ```php
    /// $n = [1, 2]; $o = new stdClass; $o->p = 'p';
    /// $a = ['one' => &$n[0], &$o->p, &$fresh];
    /// $a['one'] = 10; $a[0] = 'q'; $fresh = 'f';
    /// var_dump($n[0], $o->p, $a);
    /// ```
    /// ```expect
    /// int(10)
    /// string(1) "q"
    /// array(3) {
    ///   ["one"]=>
    ///   &int(10)
    ///   [0]=>
    ///   &string(1) "q"
    ///   [1]=>
    ///   &string(1) "f"
    /// }
    /// ```
    ///
    /// ```php
    /// function f() { return 1; }
    /// $a = [&f()];
    /// ```
    /// ```expect exit=255
    /// Fatal error: Can't use function return value in write context in %s on line 3
    /// ```
    ///
    /// A variable fetched by name is bound where it stands, in the table
    /// it names:
    ///
    /// ```php
    /// $n = 'x';
    /// $a = [&$_SERVER['k'], &$GLOBALS['g'], &$$n, &$_GET];
    /// $a[0] = 1; $a[1] = 2; $a[2] = 3; $a[3] = [4];
    /// var_dump($_SERVER['k'], $g, $x, $_GET);
    /// ```
    /// ```expect
    /// int(1)
    /// int(2)
    /// int(3)
    /// array(1) {
    ///   [0]=>
    ///   int(4)
    /// }
    /// ```
    ///
    /// ```php
    /// $a = [&$GLOBALS];
    /// ```
    /// ```expect exit=255
    /// Fatal error: $GLOBALS can only be modified using the $GLOBALS[$name] = $value syntax in %s on line 2
    /// ```
    fn element_ref(&mut self, value: tree::Expr) -> Result<(OperandKind, u32), CompileError> {
        let line = self.line_of(value);
        match self.peel(value) {
            tree::Expr::CallExpr(id) => Err(self.call_written(id)),
            // A variable fetched by name, `$$x`, an auto-global or
            // `$GLOBALS['k']`: its `FETCH_W` into a `VAR`.
            node if self.is_by_name(node) => {
                self.line = line;
                self.var_for_reference(node)
            }
            tree::Expr::SimpleVarExpr(id) => match self.simple_var_of(id, line)? {
                SimpleVar::Cv(name) => {
                    self.cv_fetched(name, Fetch::Write);
                    Ok((OperandKind::Cv, self.lookup_cv(name)))
                }
                SimpleVar::This => {
                    let this = self.fetch_this(OperandKind::Var);
                    Ok(self.operand(this))
                }
                SimpleVar::Auto(_) => Err(self.fatal("an auto-global not fetched by name")),
                SimpleVar::Globals => Err(self.compile_error(
                    "$GLOBALS can only be modified using the $GLOBALS[$name] = $value syntax",
                )),
            },
            node @ (tree::Expr::SubscriptExpr(_) | tree::Expr::MemberAccessExpr(_)) => {
                self.line = line;
                self.var_for_reference(node)
            }
            node @ tree::Expr::ScopedAccessExpr(id)
                if matches!(self.scoped_of(id, line)?, Scoped::Property { .. }) =>
            {
                self.line = line;
                self.var_for_reference(node)
            }
            _ => Err(self.compile_error("Cannot use temporary expression in write context")),
        }
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

    /// `zend_compile_shell_exec` (`Z/Zend/zend_compile.c:10915`): the
    /// operator is deprecated at 8.5 before anything of it compiles,
    /// and what compiles is the call `shell_exec(…)` over the parts, so
    /// `` `…` `` has no opcode of its own and every semantic of it is
    /// the native's. The name php hands that call is a `ZEND_AST_ZVAL`
    /// whose `attr` is `ZEND_NAME_FQ`, so `zend_compile_function_name`
    /// prefixes no namespace and asks for no resolution as the program
    /// serves: the operator inside a namespace compiles to the same
    /// `INIT_FCALL "shell_exec"` it does outside one.
    ///
    /// The `INIT_FCALL` stands before the parts, as
    /// `zend_compile_call_common` emits it before `zend_compile_args`.
    /// The parts are [`Self::encaps`] under `Holder::ShellExec`, which
    /// leaves the joined constant where nothing is interpolated and the
    /// rope's temporary where something is, never a CV or a `VAR`, so
    /// the argument is sent by `SEND_VAL` where the table has the
    /// native and by `SEND_VAL_EX` where it has not.
    ///
    /// ```php
    /// $w = 'world';
    /// echo `echo hello $w`;
    /// echo `echo plain`;
    /// ```
    /// ```expect
    /// Deprecated: The backtick (`) operator is deprecated, use shell_exec() instead in %s on line 3
    ///
    /// Deprecated: The backtick (`) operator is deprecated, use shell_exec() instead in %s on line 4
    /// hello world
    /// plain
    /// ```
    ///
    /// `zend_is_allowed_in_const_expr` refuses the node before
    /// `zend_compile_shell_exec` is reached, so a declaration that
    /// keeps a constant expression refuses it and no deprecation is
    /// raised:
    ///
    /// ```php
    /// class A { const X = `echo x`; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Constant expression contains invalid operations in %s on line 2
    /// ```
    fn shell_exec(
        &mut self,
        id: NodeId<tree::ShellExecExpr>,
        line: u32,
    ) -> Result<Where, CompileError> {
        self.raise(
            Level::Deprecated,
            "The backtick (`) operator is deprecated, use shell_exec() instead",
        );
        let name = b"shell_exec".as_slice();
        let known = self.native_index(name).is_some();
        if known {
            let lowered = self.string(name)?;
            let lowered = self.literal(lowered);
            let slot = self.cache_slot();
            self.push(Opline {
                opcode: Opcode::InitFcall,
                op1: used_stack(1, None),
                op2_type: OperandKind::Const,
                op2: lowered,
                out: slot,
                extended_value: 1,
                ..Opline::default()
            });
        } else {
            let pair = self.func_name_literal(name)?;
            let slot = self.cache_slot();
            self.push(Opline {
                opcode: Opcode::InitFcallByName,
                op2_type: OperandKind::Const,
                op2: pair,
                out: slot,
                extended_value: 1,
                ..Opline::default()
            });
        }
        let parts = self.list(self.parsed.tree[id].parts);
        let holder = Holder::ShellExec(&self.parsed.tree[id]);
        let command = self.encaps(tree::Expr::ShellExecExpr(id), parts, holder, line)?;
        let (op1_type, op1) = self.operand(command);
        self.push(Opline {
            opcode: if known {
                Opcode::SendVal
            } else {
                Opcode::SendValEx
            },
            op1_type,
            op1,
            op2: 1,
            ..Opline::default()
        });
        let fbc = if known { Fbc::Native } else { Fbc::ByName };
        Ok(self.extended_call(self.get_call_op(fbc, false), line, false))
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
    /// `assert` is `zend_compile_assert`'s before any of that
    /// ([`Self::assert`]), by the name the table holds and by the bare
    /// name in a namespace alike.
    ///
    /// Every call is given a `VAR` for what it returns (`zend_emit_op`
    /// with a `result`, `zend_make_var_result`), numbered whether or not
    /// anything reads it; [`Self::do_free`] marks it `UNUSED` when
    /// nothing does, which is what the engine's `DO_ICALL` reads as
    /// `RETURN_VALUE_USED`.
    fn call(&mut self, name: &[u8], arguments: &Args, fetch: Fetch) -> Result<Where, CompileError> {
        let line = self.line;
        let is_callable_convert = matches!(arguments, Args::CallableConvert);
        let (resolved, runtime) = self.function_name(name);
        if runtime {
            // `zend_compile_assert` over the name as written, which the
            // program resolves.
            if name.eq_ignore_ascii_case(b"assert") && !is_callable_convert {
                return self.assert(resolved, arguments, None);
            }
            return self.ns_call(resolved, arguments, fetch);
        }
        let lowered = self.arena.lowercase(resolved);
        // "Special assert() handling should apply independently of
        // compiler flags."
        if lowered == b"assert"
            && !is_callable_convert
            && let Some(index) = self.native_index(lowered)
        {
            return self.assert(resolved, arguments, Some(index));
        }
        let (callee, plan, stack, call_op) = match self.native_index(lowered) {
            Some(index) => {
                // `zend_try_compile_special_func`, which a named argument
                // keeps a call out of.
                if let Some(positional) = arguments.positional() {
                    if let Some(served) = self.special(lowered, &positional, fetch)? {
                        return Ok(served);
                    }
                    // `find_frameless_function_info`: `type != BP_VAR_R`
                    // is the ordinary call.
                    if fetch == Fetch::Read
                        && let Some(served) = self.frameless_icall(index, &positional, None)?
                    {
                        return Ok(served);
                    }
                }
                let (parameters, flagged) = self
                    .functions
                    .rows()
                    .get(index)
                    .map(|decl| {
                        (
                            decl.parameters.as_slice(),
                            attributes::deprecated_or_no_discard(&decl.attributes),
                        )
                    })
                    .unwrap_or_default();
                let callee = Callee::of(parameters);
                let plan = Plan::of(self.arena, arguments, Some(&callee));
                let stack = used_stack(plan.count, None);
                let call_op = self.get_call_op(Fbc::Native, flagged);
                (callee, plan, stack, call_op)
            }
            None => match self.declared_function(lowered) {
                Some((decl, body)) => {
                    let callee = Callee::of(&decl.parameters);
                    let plan = Plan::of(self.arena, arguments, Some(&callee));
                    let stack = used_stack(plan.count, Some(body));
                    let flagged = attributes::deprecated_or_no_discard(&decl.attributes);
                    let call_op = self.get_call_op(Fbc::User, flagged);
                    (callee, plan, stack, call_op)
                }
                None => return self.dynamic_call(resolved, arguments),
            },
        };
        let lowered = self.string(lowered)?;
        let lowered = self.literal(lowered);
        let slot = self.cache_slot();
        self.push(Opline {
            opcode: Opcode::InitFcall,
            op1: stack,
            op2_type: OperandKind::Const,
            op2: lowered,
            out: slot,
            extended_value: plan.count,
            ..Opline::default()
        });
        self.call_common(&plan, arguments, Some(&callee), call_op, line)
    }

    /// `zend_hash_find_ptr(CG(function_table), lcname)` over the
    /// functions this file has declared so far, with `fbc_is_finalized`
    /// folded in: a body that has compiled, whose `used_stack` a call
    /// site can write; none for a native, which [`Self::native_index`]
    /// answers, and none for the function whose own body is being
    /// compiled, which is in php's table without `ZEND_ACC_DONE_PASS_TWO`
    /// and so compiles a call to itself by name.
    fn declared_function(&self, lowered: &[u8]) -> Option<(&FunctionDecl, &OpArray)> {
        self.functions_declared
            .iter()
            .find(|(decl, _)| decl.name.0.eq_ignore_ascii_case(lowered))
            .map(|(decl, body)| (decl, body))
    }

    /// `zend_compile_dynamic_call` for a name written as a constant
    /// string that `CG(function_table)` has not: `INIT_FCALL_BY_NAME`
    /// over the name's constant pair, the arguments sent for a callee
    /// nothing is known of, and `DO_FCALL_BY_NAME`
    /// (`zend_compile_call_common` with `fbc` null, whose `used_stack`
    /// is written for an `INIT_FCALL` alone, so `op1` stays unused).
    /// The engine resolves the pair's lowered name at the `INIT` and
    /// raises php's `Error` over the written one where nothing declares
    /// it. A name whose last `:` follows another, `'C::m'`, is the class
    /// and the method: `INIT_STATIC_METHOD_CALL` over the class's pair in
    /// `op1` and the method's in `op2`, with two cache slots, and
    /// `DO_FCALL`.
    fn dynamic_call(&mut self, name: &[u8], arguments: &Args) -> Result<Where, CompileError> {
        let line = self.line;
        // `zend_memrchr(str, ':')` with `colon > str && *(colon - 1) == ':'`.
        if let Some(colon) = name.iter().rposition(|&byte| byte == b':')
            && colon > 0
            && name.get(colon - 1) == Some(&b':')
        {
            let class = self.resolved_class_literal(&name[..colon - 1])?;
            let method = self.func_name_literal(&name[colon + 1..])?;
            let plan = Plan::of(self.arena, arguments, None);
            let slots = self.cache_slots(2);
            self.push(Opline {
                opcode: Opcode::InitStaticMethodCall,
                op1_type: OperandKind::Const,
                op1: class,
                op2_type: OperandKind::Const,
                op2: method,
                out: slots,
                extended_value: plan.count,
                ..Opline::default()
            });
            return self.call_common(&plan, arguments, None, Opcode::DoFcall, line);
        }
        let pair = self.func_name_literal(name)?;
        let plan = Plan::of(self.arena, arguments, None);
        let slot = self.cache_slot();
        self.push(Opline {
            opcode: Opcode::InitFcallByName,
            op2_type: OperandKind::Const,
            op2: pair,
            out: slot,
            extended_value: plan.count,
            ..Opline::default()
        });
        let call_op = self.get_call_op(Fbc::ByName, false);
        self.call_common(&plan, arguments, None, call_op, line)
    }

    /// `zend_compile_dynamic_call` over a callee that is no constant
    /// string (`Z/Zend/zend_compile.c:4044`): `INIT_DYNAMIC_CALL` with
    /// the callee in `op2` and the number of arguments in
    /// `extended_value`, no cache slot, since nothing about the callee
    /// is known until the opline runs; then the arguments sent for a
    /// callee nothing is known of and `DO_FCALL`, which is what
    /// `zend_get_call_op` picks with `fbc` null over an opline that is
    /// no `INIT_FCALL_BY_NAME`. A callee the compiler folds to a
    /// string is the name [`Self::dynamic_call`] calls by.
    ///
    /// A closure written where it is called is the value the call
    /// makes, and a value that is no callable is php's `Error` in the
    /// initialiser's words:
    ///
    /// ```php
    /// echo (function () { return 'a'; })(), (fn ($x) => $x)('b'), (static fn () => 'c')(), "\n";
    /// function f() { return 'strrev'; }
    /// echo f()('de'), ('strre' . 'v')('fgh'), "\n";
    /// $a = ['x' => fn () => 'y'];
    /// echo $a['x'](), "\n";
    /// try { (1.5)(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { ([1, 2, 3])(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { (new stdClass)(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// abc
    /// edhgf
    /// y
    /// Value of type float is not callable
    /// Array callback must have exactly two elements
    /// Object of type stdClass is not callable
    /// ```
    fn call_var(&mut self, callee: Source, arguments: &Args) -> Result<Where, CompileError> {
        let line = self.line;
        let value = self.source(callee)?;
        if let Where::Constant(name) = value
            && let Some(name) = name.as_str()
        {
            return self.dynamic_call(name.bytes(), arguments);
        }
        let (op2_type, op2) = self.operand(value);
        let plan = Plan::of(self.arena, arguments, None);
        self.push(Opline {
            opcode: Opcode::InitDynamicCall,
            op2_type,
            op2,
            extended_value: plan.count,
            ..Opline::default()
        });
        self.call_common(&plan, arguments, None, Opcode::DoFcall, line)
    }

    /// `zend_compile_new` (`Z/Zend/zend_compile.c:5410`): the class
    /// compiled as `zend_compile_class_ref` compiles it, then `NEW`
    /// with the class's constant pair in `op1` and a cache slot in
    /// `op2` for a class named at compile time, the fetch type of
    /// `self`, `parent` or `static` in an unused `op1`, or the `VAR` a
    /// `FETCH_CLASS` wrote; the number of arguments in `extended_value`
    /// and the object written into a `VAR`. Then the arguments sent for
    /// a callee nothing is known of, since the constructor is looked up
    /// as the program serves (`zend_compile_call_common` with `fbc`
    /// null), and the `DO_FCALL` that serves it, whose own `VAR`
    /// `zend_do_free` marks unused.
    ///
    /// ```php
    /// class A {
    ///     static function make() { return new static; }
    ///     static function mine() { return new self; }
    /// }
    /// class B extends A {
    ///     function up() { return new parent; }
    /// }
    /// var_dump(get_class(B::make()), get_class(B::mine()), get_class((new B)->up()));
    /// $name = 'B';
    /// var_dump(new $name instanceof A);
    /// ```
    /// ```expect
    /// string(1) "B"
    /// string(1) "A"
    /// string(1) "A"
    /// bool(true)
    /// ```
    fn new_expression(
        &mut self,
        class: ClassSpec,
        arguments: &Args,
    ) -> Result<Where, CompileError> {
        let line = self.line;
        let class = self.class_ref(class, FETCH_CLASS_EXCEPTION)?;
        self.new_of(&class, arguments, line)
    }

    /// [`Self::new_expression`] from its `NEW` on, over a class already
    /// compiled: one by name, the scope's, or the `VAR` a `FETCH_CLASS`
    /// or a `DECLARE_ANON_CLASS` ([`Self::anonymous_class`]) wrote. The
    /// `DO_FCALL` stands at `line`, the `new`'s own.
    fn new_of(
        &mut self,
        class: &ClassRef,
        arguments: &Args,
        line: u32,
    ) -> Result<Where, CompileError> {
        // `zend_compile_call_common` over the `NEW` it follows: a closure
        // is made of a call frame, and `new` has no function to make it of.
        if matches!(arguments, Args::CallableConvert) {
            return Err(self.compile_error("Cannot create Closure for new expression"));
        }
        let out = self.temporary();
        let (op1_type, op1) = self.class_operand(class)?;
        let op2 = if op1_type == OperandKind::Const {
            self.cache_slot()
        } else {
            0
        };
        let plan = Plan::of(self.arena, arguments, None);
        self.push(Opline {
            opcode: Opcode::New,
            op1_type,
            op1,
            op2,
            out_type: OperandKind::Var,
            out,
            extended_value: plan.count,
            ..Opline::default()
        });
        self.send_args(&plan, arguments, None)?;
        let served = self.extended_call(Opcode::DoFcall, line, plan.extra_named);
        self.do_free(served);
        Ok(Where::Returned(out))
    }

    /// `zend_compile_static_call` (`Z/Zend/zend_compile.c:5332`): the
    /// class compiled as `zend_compile_class_ref` compiles it, then
    /// `INIT_STATIC_METHOD_CALL` with the class in `op1`
    /// (`zend_set_class_name_op1`: its constant pair, the fetch type of
    /// `self`, `parent` or `static` in an unused `op1`, or the `VAR` a
    /// `FETCH_CLASS` wrote), the method's pair in `op2`, two cache slots
    /// in `result` and the number of arguments in `extended_value`;
    /// `__construct` is php's `CONSTRUCTOR`, `op2` unused and one cache
    /// slot where the class is a constant. The callee is known where the
    /// class is named at compile time or is `self` in a known scope, and
    /// the table holds a method of the name the scope may call
    /// (`zend_get_compatible_func_or_null`), which decides how the
    /// arguments are sent and whether the call is `DO_UCALL`.
    ///
    /// ```php
    /// class P {
    ///     public static $tag = "P";
    ///     function __construct() { echo "P\n"; }
    ///     static function who() { return static::$tag; }
    ///     function name() { return "P::name"; }
    /// }
    /// class C extends P {
    ///     public static $tag = "C";
    ///     function __construct() { parent::__construct(); echo "C\n"; }
    ///     function name() { return parent::name() . " via " . self::who(); }
    ///     static function s() { return parent::who(); }
    /// }
    /// $c = new C;
    /// var_dump($c->name(), C::s());
    /// $k = 'C';
    /// var_dump($k::who(), $c::who());
    /// ```
    /// ```expect
    /// P
    /// C
    /// string(13) "P::name via C"
    /// string(1) "C"
    /// string(1) "C"
    /// string(1) "C"
    /// ```
    ///
    /// A method the program names is `op2`'s `CV` or temporary, with one
    /// cache slot for a class named at compile time and none otherwise,
    /// and no callee known; the engine holds the name to be a string,
    /// php's `Method name must be a string`. `class` after `::` in a
    /// call is a method's name like any semi-reserved word.
    ///
    /// ```php
    /// class A {
    ///     static function hi($x) { return "hi $x"; }
    ///     function me() { $m = 'who'; return static::$m(); }
    ///     static function who() { return static::class; }
    ///     static function class() { return 'kw'; }
    /// }
    /// class B extends A {}
    /// $m = 'hi';
    /// $c = 'A';
    /// echo A::$m(1), ' ', $c::$m(2), ' ', A::{'h' . 'i'}(3), ' ', (new B)->me(), ' ', A::class(), "\n";
    /// $n = 5;
    /// try { A::$n(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { A::$undefined(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $m = 'nope';
    /// try { A::$m(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// hi 1 hi 2 hi 3 B kw
    /// Method name must be a string
    ///
    /// Warning: Undefined variable $undefined in %s on line 14
    /// Method name must be a string
    /// Call to undefined method A::nope()
    /// ```
    fn static_call(
        &mut self,
        class: ClassSpec,
        method: Member,
        arguments: &Args,
    ) -> Result<Where, CompileError> {
        let line = self.member_line(method);
        let class = self.class_link(class)?;
        let method = match self.method_name(method)? {
            MethodName::Constant(method) => method,
            MethodName::Computed((op2_type, op2)) => {
                let plan = Plan::of(self.arena, arguments, None);
                let (op1_type, op1) = self.class_operand(&class)?;
                let out = if op1_type == OperandKind::Const {
                    self.cache_slot()
                } else {
                    0
                };
                self.push(Opline {
                    opcode: Opcode::InitStaticMethodCall,
                    op1_type,
                    op1,
                    op2_type,
                    op2,
                    out,
                    extended_value: plan.count,
                    ..Opline::default()
                });
                return self.call_common(&plan, arguments, None, Opcode::DoFcall, line);
            }
        };
        let method: &[u8] = method;
        let constructor = method.eq_ignore_ascii_case(b"__construct");
        let known = match &class {
            _ if constructor => None,
            ClassRef::Named(name) => self.known_method_of(name, method),
            ClassRef::Scope(fetch) if fetch & 0x0f == FETCH_CLASS_SELF && self.scope_known() => {
                self.known_method_of_active(method)
            }
            ClassRef::Scope(_) | ClassRef::Fetched(_) => None,
        };
        let (call_op, callee) = match known {
            Some((Known::User { flagged }, callee)) => {
                (self.get_call_op(Fbc::User, flagged), Some(callee))
            }
            Some((Known::Native, callee)) => (Opcode::DoFcall, Some(callee)),
            None => (Opcode::DoFcall, None),
        };
        let plan = Plan::of(self.arena, arguments, callee.as_ref());
        let count = plan.count;
        let (op1_type, op1) = self.class_operand(&class)?;
        let opline = if constructor {
            let out = if op1_type == OperandKind::Const {
                self.cache_slot()
            } else {
                0
            };
            Opline {
                opcode: Opcode::InitStaticMethodCall,
                op1_type,
                op1,
                out,
                extended_value: count,
                ..Opline::default()
            }
        } else {
            let method_at = self.func_name_literal(method)?;
            Opline {
                opcode: Opcode::InitStaticMethodCall,
                op1_type,
                op1,
                op2_type: OperandKind::Const,
                op2: method_at,
                out: self.cache_slots(2),
                extended_value: count,
                ..Opline::default()
            }
        };
        self.push(opline);
        self.call_common(&plan, arguments, callee.as_ref(), call_op, line)
    }

    /// `zend_compile_method_call`: the receiver compiled,
    /// `INIT_METHOD_CALL` over it with the method's constant pair in
    /// `op2`, two cache slots in `result` and the number of arguments in
    /// `extended_value`, the arguments sent for a callee nothing is
    /// known of, and `DO_FCALL`, but for a method of `$this` the class
    /// being compiled pins down ([`Self::known_on_this`]). A method the
    /// program names is `op2`'s `CV` or temporary and takes no cache
    /// slot; the engine holds it to be a string before it reads the
    /// receiver, so `Method name must be a string` stands before what a
    /// receiver that is no object raises.
    ///
    /// ```php
    /// class C { function hi($x) { return "hi $x"; } }
    /// $o = new C;
    /// $m = 'hi';
    /// echo $o->$m(1), ' ', $o->{'h' . 'i'}(2), ' ', $o?->$m(3), "\n";
    /// $n = null;
    /// var_dump($n?->$m());
    /// try { $o->$n(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $n->$n(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $n->$m(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $o->{$m . 'x'}(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $r = &$m;
    /// echo $o->$r(4), "\n";
    /// ```
    /// ```expect
    /// hi 1 hi 2 hi 3
    /// NULL
    /// Method name must be a string
    /// Method name must be a string
    /// Call to a member function hi() on null
    /// Call to undefined method C::hix()
    /// hi 4
    /// ```
    fn method_call(
        &mut self,
        object: tree::Expr,
        method: Member,
        arguments: &Args,
        nullsafe: bool,
    ) -> Result<Where, CompileError> {
        let line = self.member_line(method);
        let (op1_type, op1) = self.receiver(object)?;
        // `zend_emit_jmp_null` over the receiver of a `?->`, but for
        // `$this`, which throws where it does not exist.
        if nullsafe && !self.is_this(object) {
            self.jmp_null((op1_type, op1), false);
        }
        let (op2_type, op2, out, known) = match self.method_name(method)? {
            MethodName::Constant(method) => {
                let at = self.func_name_literal(method)?;
                let known = self.known_on_this(op1_type, method);
                (OperandKind::Const, at, self.cache_slots(2), known)
            }
            MethodName::Computed((kind, at)) => (kind, at, 0, None),
        };
        let (call_op, callee) = match known {
            Some(method) => (
                self.get_call_op(Fbc::User, method.flagged),
                Some(method.callee),
            ),
            None => (Opcode::DoFcall, None),
        };
        let plan = Plan::of(self.arena, arguments, callee.as_ref());
        self.push(Opline {
            opcode: Opcode::InitMethodCall,
            op1_type,
            op1,
            op2_type,
            op2,
            out,
            extended_value: plan.count,
            ..Opline::default()
        });
        let served = self.call_common(&plan, arguments, callee.as_ref(), call_op, line)?;
        // `short_circuiting_checkpoint != zend_short_circuiting_checkpoint()`
        // once `zend_compile_call_common` made a closure: a `?->` the
        // receiver stands on would leave it null.
        if matches!(arguments, Args::CallableConvert)
            && (nullsafe || self.is_short_circuited(object))
        {
            return Err(
                self.compile_error("Cannot combine nullsafe operator with Closure creation")
            );
        }
        Ok(served)
    }

    /// `zend_compile_method_call`'s "known method on `$this`": where the
    /// receiver is the `IS_UNUSED` `$this` of a method whose scope is
    /// known, the method of the name the class being compiled has
    /// declared so far, its own among them, is the callee where it is
    /// private or final, since no class extending it can put another in
    /// its place. Then the call is `DO_UCALL` and its arguments are sent
    /// by the parameters it declares.
    ///
    /// ```php
    /// class A {
    ///     private function p(&$r, $n) { $r .= "A::p$n "; return $n ? $this->p($r, $n - 1) : $r; }
    ///     final function f(array $a) { return count($a); }
    ///     function run() { $s = ''; $this->p($s, 2); return $s . $this->f([1, 2]) . ' ' . self::d(); }
    ///     static function d() { return 'd'; }
    /// }
    /// class B extends A {
    ///     function p() { return 'B::p'; }
    /// }
    /// echo (new B)->run(), "\n";
    /// ```
    /// ```expect
    /// A::p2 A::p1 A::p0 2 d
    /// ```
    fn known_on_this(&self, receiver: OperandKind, method: &[u8]) -> Option<TableMethod> {
        if receiver != OperandKind::Unused || !self.scope_known() {
            return None;
        }
        let at = self.active_class.as_ref()?.at;
        self.table_method(at, method)
            .filter(|m| m.visibility == Visibility::Private || m.is_final)
    }

    /// The receiver of a method call or a property fetch: `$this` is
    /// `FETCH_THIS` into a `VAR` (`zend_delayed_compile_prop` and
    /// `zend_compile_method_call`, where `this_guaranteed_exists` is
    /// false outside a method's body, which is everywhere this compiler
    /// reads); anything else is the expression itself.
    fn receiver(&mut self, object: tree::Expr) -> Result<(OperandKind, u32), CompileError> {
        self.receiver_of(object)
    }

    /// `zend_try_compile_const_expr_resolve_class_name`
    /// (`Z/Zend/zend_compile.c:1784`): the name `C::class` folds to
    /// where it is known while the file compiles, a name written out as
    /// it resolves, `self` the class being compiled and `parent` the
    /// name it extends where the scope is known; nothing for `static`,
    /// for `self` and `parent` where the scope is not known, and for a
    /// class the program computes. `self`, `parent` and `static` are
    /// held to [`Self::ensure_valid_class_fetch_type`] first. A name
    /// written out is a string of its own, as php's scanner made one for
    /// the token, and `parent` the class's `parent_name`, a string of its
    /// own too; `self` is the class's name itself
    /// (`ZVAL_STR_COPY(zv, CG(active_class_entry)->name)`), a claim the
    /// caller keeps where it needs a string or gives back where the name
    /// stands for it. So an `eval`'d class constant naming its own class
    /// costs a string, its parent's a string, and `self::class` none (the
    /// file under the recipe's `TMPDIR`, so each `eval`'s name has one
    /// length):
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3268-resolved.inc';
    /// file_put_contents($f, <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// foreach ([
    ///     'class P10 {}',
    ///     'class B10 { const K = B10::class; }',
    ///     'class B12 { const K = self::class; }',
    ///     'class B13 extends P10 { const K = parent::class; }',
    ///     'class B16 { const K = B16::class, L = B16::class; }',
    ///     '$GLOBALS["a"] = [B10::K, B12::K, B13::K, B16::K, B16::L];',
    /// ] as $c) echo m($c), "  ", $c, "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 176  class P10 {}
    /// 528  class B10 { const K = B10::class; }
    /// 496  class B12 { const K = self::class; }
    /// 528  class B13 extends P10 { const K = parent::class; }
    /// 560  class B16 { const K = B16::class, L = B16::class; }
    /// 728  $GLOBALS["a"] = [B10::K, B12::K, B13::K, B16::K, B16::L];
    /// ```
    fn resolved_class_name(
        &mut self,
        operand: ClassOperand,
    ) -> Result<Option<Operated>, CompileError> {
        let written = match operand {
            ClassOperand::Name(token) => self.text(token).to_vec(),
            ClassOperand::Value(value) => self.value_class_name(value)?,
            ClassOperand::Expr(_) => return Ok(None),
        };
        let fetch = class_fetch_type(&written);
        self.ensure_valid_class_fetch_type(fetch)?;
        let name = match (fetch, operand) {
            (FETCH_CLASS_DEFAULT, ClassOperand::Value(_)) => self.fq_name(&written)?,
            (FETCH_CLASS_DEFAULT, _) => self.class_name(&written).to_vec(),
            (FETCH_CLASS_SELF, _) if self.scope_known() => match self.active_name() {
                Some(active) => active,
                None => return Ok(None),
            },
            (FETCH_CLASS_PARENT, _) if self.scope_known() => match self.active_parent.clone() {
                Some(parent) => parent,
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        Ok(Some(if fetch == FETCH_CLASS_SELF {
            Operated::Made(self.class_name_made()?)
        } else {
            Operated::Kept(self.string(&name)?)
        }))
    }

    /// `zend_compile_class_name` (`Z/Zend/zend_compile.c:11105`),
    /// `C::class`: the string [`Self::resolved_class_name`] folds to
    /// where it does; `FETCH_CLASS_NAME` with the fetch type in an
    /// unused `op1` for `static` and for `self` and `parent` where the
    /// scope is not known; and for an expression, `FETCH_CLASS_NAME` over
    /// its value, a constant being php's `Cannot use "::class" on`
    /// compile-time fatal. Each into a temporary.
    ///
    /// ```php
    /// namespace N;
    /// class Foo {
    ///     function names() { return [self::class, static::class, Foo::class]; }
    /// }
    /// class Bar extends Foo {
    ///     function up() { return parent::class; }
    /// }
    /// $o = new Bar;
    /// var_dump(Foo::class, \Foo::class, $o->names(), $o->up(), $o::class);
    /// ```
    /// ```expect
    /// string(5) "N\Foo"
    /// string(3) "Foo"
    /// array(3) {
    ///   [0]=>
    ///   string(5) "N\Foo"
    ///   [1]=>
    ///   string(5) "N\Bar"
    ///   [2]=>
    ///   string(5) "N\Foo"
    /// }
    /// string(5) "N\Foo"
    /// string(5) "N\Bar"
    /// ```
    ///
    /// ```php
    /// function f() { return static::class; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use "static" when no class scope is active in %s on line 2
    /// ```
    ///
    /// A number written where the class goes is no name at all,
    /// `zend_try_compile_const_expr_resolve_class_name`'s refusal of a
    /// `ZEND_AST_ZVAL` that is no string, while an expression that
    /// folds to one is the refusal after it compiled:
    ///
    /// ```php
    /// echo (1)::class;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Illegal class name in %s on line 2
    /// ```
    ///
    /// ```php
    /// echo (1 + 1)::class;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use "::class" on int in %s on line 2
    /// ```
    fn class_name_of(&mut self, class: ClassSpec) -> Result<Where, CompileError> {
        let operand = self.decoded_class(class)?;
        if let Some(value) = self.resolved_class_name(operand)? {
            return Ok(Where::Constant(value.kept()));
        }
        // `class_ast->kind == ZEND_AST_ZVAL`: the fetch type of the name.
        let (op1_type, op1) = match operand {
            ClassOperand::Name(token) => (OperandKind::Unused, class_fetch_type(self.text(token))),
            ClassOperand::Value(value) => {
                let name = self.value_class_name(value)?;
                (OperandKind::Unused, class_fetch_type(&name))
            }
            ClassOperand::Expr(expr) => match self.expr(expr)? {
                Where::Constant(value) => {
                    let shown = match value.kind() {
                        ZvalType::Null => "null",
                        ZvalType::False => "false",
                        ZvalType::True => "true",
                        ZvalType::Long => "int",
                        ZvalType::Double => "float",
                        ZvalType::String => "string",
                        ZvalType::Array => "array",
                        _ => "value",
                    };
                    return Err(self.compile_error(format!("Cannot use \"::class\" on {shown}")));
                }
                value => self.operand(value),
            },
        };
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::FetchClassName,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_class_const` (`Z/Zend/zend_compile.c:11064`): a
    /// constant `zend_try_ct_eval_class_const` substitutes is its value
    /// ([`Self::ct_class_constant`]): one of the class whose body is being
    /// compiled, named `self` where the scope is known or by the class's
    /// own name, where the class declared it before this point and it
    /// folded to a scalar, and, in a compile php runs without
    /// `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION`, one of a class the
    /// request's table holds; every other constant is fetched as the
    /// program serves, since opcache's option folds no other class's in
    /// a file it caches.
    /// The class is compiled as `zend_compile_class_ref` compiles it,
    /// then `FETCH_CLASS_CONSTANT` with the constant's name in `op2`, into
    /// a temporary (`zend_emit_op_tmp`), then the class in `op1`
    /// (`zend_set_class_name_op1`) and two cache slots in
    /// `extended_value`. php reads the constant's name before the
    /// class's, so the constants stand in that order, as they do under
    /// [`Self::static_prop`].
    ///
    /// ```php
    /// class P { const Y = 'py'; const Z = 'pz'; }
    /// class A extends P {
    ///     const X = 'ax';
    ///     const Z = 'az';
    ///     function f() { return [self::X, static::Z, parent::Z, A::X]; }
    /// }
    /// class B extends A { const Z = 'bz'; }
    /// $r = (new B)->f();
    /// echo $r[0], $r[1], $r[2], $r[3], "\n";
    /// $b = new B;
    /// var_dump($b::Z, ('A')::X);
    /// ```
    /// ```expect
    /// axbzpzax
    /// string(2) "bz"
    /// string(2) "ax"
    /// ```
    ///
    /// Outside a class, where the scope is known, `self` is php's
    /// compile-time fatal; in the main op array the scope is the
    /// includer's and the fetch raises the `Error` as the program serves:
    ///
    /// ```php
    /// function f() { return self::X; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use "self" when no class scope is active in %s on line 2
    /// ```
    ///
    /// ```php
    /// echo static::X;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Cannot access "static" when no class scope is active in %s:2
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { function f() { return parent::X; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use "parent" when current class scope has no parent in %s on line 2
    /// ```
    fn class_const(
        &mut self,
        id: NodeId<tree::ScopedAccessExpr>,
        class: ClassSpec,
        name: &ScopedName<'a>,
    ) -> Result<Where, CompileError> {
        // Both children evaluated, the class first; the class resolved
        // only where the name folded to a string.
        let operand = self.evaluated_class(class)?;
        if let Some(written) = self.folded_name(name)?
            && let Some(resolved) = self.constant_class(operand)?
            && let Some(value) = self.ct_class_constant(resolved, written)?
        {
            let value = self.claimed(tree::Expr::ScopedAccessExpr(id), value);
            return Ok(Where::Constant(value));
        }
        let class = self.class_ref_of(operand, FETCH_CLASS_EXCEPTION)?;
        let name = match name {
            ScopedName::Written(bytes) => Where::Constant(self.string(bytes)?),
            ScopedName::Computed(expr) => self.expr(*expr)?,
        };
        let (op2_type, op2) = self.operand(name);
        let out = self.temporary();
        let (op1_type, op1) = self.class_operand(&class)?;
        let slots = if op1_type == OperandKind::Const || op2_type == OperandKind::Const {
            self.cache_slots(2)
        } else {
            0
        };
        self.push(Opline {
            opcode: Opcode::FetchClassConstant,
            op1_type,
            op1,
            op2_type,
            op2,
            out_type: OperandKind::Tmp,
            out,
            extended_value: slots,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_eval_const_expr` over a name after `::`: the name written
    /// out, or the string a computed one folds to, so `C::{'X'}` and
    /// `C::{'X' . ''}` are `C::X`; nothing where it folds to no string or
    /// does not fold.
    fn folded_name(&mut self, name: &ScopedName<'a>) -> Result<Option<&'a [u8]>, CompileError> {
        Ok(match name {
            ScopedName::Written(bytes) => Some(*bytes),
            ScopedName::Computed(expr) => self
                .eval_const(*expr)?
                .and_then(|value| value.as_str().map(|s| self.arena.copy(s.bytes()))),
        })
    }

    /// `zend_try_ct_eval_class_const` (`Z/Zend/zend_compile.c:1863`): a
    /// constant of the class whose body is being compiled where the name
    /// refers to it ([`Self::active_class_constant`]), and otherwise, for
    /// a class named as written, one of a class the request's table
    /// holds ([`Self::request_class_constant`]); nothing under
    /// `ZEND_COMPILE_NO_PERSISTENT_CONSTANT_SUBSTITUTION`
    /// ([`Substitution::NoPersistent`], a parameter's default), and
    /// nothing for `parent`, `static`, or `self` where the scope is not
    /// known.
    fn ct_class_constant(
        &mut self,
        class: &[u8],
        name: &[u8],
    ) -> Result<Option<Operated>, CompileError> {
        if self.substitution == Substitution::NoPersistent {
            return Ok(None);
        }
        if self.refers_to_active_class(class) {
            return self.active_class_constant(name);
        }
        if class_fetch_type(class) != FETCH_CLASS_DEFAULT {
            return Ok(None);
        }
        Ok(self.request_class_constant(class, name))
    }

    /// `class_name_refers_to_active_ce` (`Z/Zend/zend_compile.c:1720`):
    /// `self` where the scope is known, or the name of the class whose
    /// body is being compiled, whatever its case.
    fn refers_to_active_class(&self, class: &[u8]) -> bool {
        let Some(active) = self.active_name() else {
            return false;
        };
        match class_fetch_type(class) {
            FETCH_CLASS_SELF => self.scope_known(),
            FETCH_CLASS_DEFAULT => class.eq_ignore_ascii_case(&active),
            _ => false,
        }
    }

    /// `zend_try_ct_eval_class_const`'s one arm opcache leaves standing,
    /// a name that refers to the class whose body is being compiled
    /// ([`Self::refers_to_active_class`]): a constant that class declared
    /// before the member being compiled and that folded to a scalar or
    /// to an array `array_is_const` takes, which is the constant's
    /// value; nothing for every other.
    ///
    /// A declaration's constant expression substitutes it too
    /// (`zend_eval_const_expr`'s `ZEND_AST_CLASS_CONST` arm), so a class
    /// whose table then holds nothing to evaluate is declared updated
    /// and its parent's constants are not evaluated for it:
    ///
    /// ```php
    /// class B { const X = UNRESOLVED; }
    /// class C extends B { const X = 2; const A = 1; const Y = self::A; public $p = [self::A]; }
    /// var_dump((new C)->p);
    /// ```
    /// ```expect
    /// array(1) {
    ///   [0]=>
    ///   int(1)
    /// }
    /// ```
    ///
    /// An array that holds it folds to the unit's immutable array, and
    /// one naming a constant declared below it, or in a parameter's
    /// default, does not:
    ///
    /// ```php
    /// class X { const K = [1]; public $c = [self::K]; } debug_zval_dump(new X);
    /// class Y { public $p = [self::Z]; const Z = 9; function f($a = self::Z, $b = [self::Z]) {} }
    /// debug_zval_dump(new Y);
    /// foreach ((new ReflectionMethod('Y', 'f'))->getParameters() as $p) echo $p, "\n";
    /// ```
    /// ```expect regime=immutable-literals
    /// object(X)#1 (1) refcount(1){
    ///   ["c"]=>
    ///   array(1) packed interned {
    ///     [0]=>
    ///     array(1) packed interned {
    ///       [0]=>
    ///       int(1)
    ///     }
    ///   }
    /// }
    /// object(Y)#1 (1) refcount(1){
    ///   ["p"]=>
    ///   array(1) packed refcount(2){
    ///     [0]=>
    ///     int(9)
    ///   }
    /// }
    /// Parameter #0 [ <optional> $a = self::Z ]
    /// Parameter #1 [ <optional> $b = [self::Z] ]
    /// ```
    fn active_class_constant(&mut self, name: &[u8]) -> Result<Option<Operated>, CompileError> {
        let Some(value) = self.foldable_constant(name) else {
            return Ok(None);
        };
        let value = value.clone();
        if let ConstExpr::UnitValue(held) = &value
            && held.value().as_arr().is_some()
            && !array_is_const(held.value())
        {
            return Ok(None);
        }
        self.operated_of(&value)
    }

    /// `zend_compile_static_prop` under `BP_VAR_R`, which delays nothing
    /// this compiler reads: the class compiled as `zend_compile_class_ref`
    /// compiles it, then `FETCH_STATIC_PROP_R` with the property's name
    /// as one constant in `op1` and three cache slots in
    /// `extended_value` (`zend_alloc_cache_slots(3)`), then the class in
    /// `op2`, its constant pair (`zend_add_class_name_literal`), the
    /// fetch type of `self`, `parent` or `static`, or the `VAR` a
    /// `FETCH_CLASS` wrote. The fetch is written into a temporary
    /// (`zend_adjust_for_fetch_type`). php reads the property's name
    /// before the class's, so the literals stand in that order.
    ///
    /// ```php
    /// class A {
    ///     public static $n = 0;
    ///     public static $list = [];
    ///     static function bump() { self::$n++; static::$list[] = self::$n; return self::$n; }
    /// }
    /// class B extends A {
    ///     static function twice() { parent::bump(); return parent::$n + static::$n; }
    /// }
    /// A::bump();
    /// var_dump(B::twice(), A::$list);
    /// echo self::$n;
    /// ```
    /// ```expect exit=255
    /// int(4)
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    ///
    /// Fatal error: Uncaught Error: Cannot access "self" when no class scope is active in %s:12
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 12
    /// ```
    fn static_prop(&mut self, class: ClassSpec, name: &ScopedName) -> Result<Where, CompileError> {
        let StaticOperands {
            name: (op1_type, op1),
            class: (op2_type, op2),
            slots,
        } = self.static_operands(class, name)?;
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::FetchStaticPropR,
            op1_type,
            op1,
            op2_type,
            op2,
            out_type: OperandKind::Tmp,
            out,
            extended_value: slots,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_static_prop`'s operands (`Z/Zend/zend_compile.c:3202`),
    /// which every opline over a class's property takes: the class
    /// compiled first (`zend_compile_class_ref`), then the name
    /// (`zend_compile_expr`), a name written out or one the compiler
    /// folds being the constant string `convert_to_string` leaves in
    /// `op1` with three cache slots, and any other the operand its
    /// expression left, with one slot where the class is a constant pair
    /// and none where the class is fetched; `op2` the class.
    ///
    /// ```php
    /// class A { public static $b = 1; public static $c = [0, 1]; }
    /// $b = 'b'; $c = 'c'; $n = 42;
    /// var_dump(A::$$b, A::${'b'}, A::${$b}, A::$$c[1], A::${'' . $b});
    /// try { var_dump(A::${$n}); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { var_dump(A::${42}); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(1)
    /// int(1)
    /// int(1)
    /// int(1)
    /// int(1)
    /// Access to undeclared static property A::$42
    /// Access to undeclared static property A::$42
    /// ```
    fn static_operands(
        &mut self,
        class: ClassSpec,
        name: &ScopedName,
    ) -> Result<StaticOperands, CompileError> {
        let class = self.class_link(class)?;
        let name = match name {
            ScopedName::Written(bytes) => Where::Constant(self.string(bytes)?),
            ScopedName::Computed(expr) => match self.expr(*expr)? {
                Where::Constant(value) => {
                    Where::Constant(self.stringified(value)?.unwrap_or(value))
                }
                computed => computed,
            },
        };
        let name = self.operand(name);
        let slots = match (name.0, &class) {
            (OperandKind::Const, _) => self.cache_slots(3),
            (_, ClassRef::Named(_)) => self.cache_slots(1),
            _ => 0,
        };
        let class = self.class_operand(&class)?;
        Ok(StaticOperands { name, class, slots })
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

    /// `zend_compile_prop` under `BP_VAR_R`: the chain the property
    /// stands on compiled delayed ([`Self::delayed_var`]), every name
    /// the program computes compiled where it stands, and the fetches
    /// laid down after them at `zend_delayed_compile_end`, each
    /// `FETCH_OBJ_R` over the one under it with the name in `op2`, three
    /// cache slots in `extended_value` for a constant one
    /// (`zend_alloc_cache_slots(3)`), written into a temporary
    /// (`zend_adjust_for_fetch_type`). So `$a->$b->{$c[1]}` reads
    /// `$c[1]` before it fetches `$a->$b`:
    ///
    /// ```php
    /// var_dump($a->$b->{$c[1]});
    /// ```
    /// ```expect
    /// Warning: Undefined variable $c in %s on line 2
    ///
    /// Warning: Trying to access array offset on null in %s on line 2
    ///
    /// Warning: Undefined variable $a in %s on line 2
    ///
    /// Warning: Undefined variable $b in %s on line 2
    ///
    /// Warning: Attempt to read property "" on null in %s on line 2
    ///
    /// Warning: Attempt to read property "" on null in %s on line 2
    /// NULL
    /// ```
    fn prop(&mut self, expr: tree::Expr) -> Result<Where, CompileError> {
        let held = self.delayed_var(expr, Fetch::Read)?;
        let (_, out) = self.base_fetch(held, Fetch::Read, Over::Obj);
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_instanceof`: a constant on the left is `false`
    /// where it stands, php's compile-time answer for what is no object;
    /// otherwise the class compiled as `zend_compile_class_ref` compiles
    /// it under `ZEND_FETCH_CLASS_NO_AUTOLOAD`, `_SILENT` and
    /// `_EXCEPTION`, then `INSTANCEOF` over the operand with the class in
    /// `op2`, a cache slot in `extended_value` where the class is a
    /// constant, into a temporary.
    ///
    /// ```php
    /// class A { function is($o) { return [$o instanceof self, $o instanceof static]; } }
    /// class B extends A {}
    /// $cls = 'A';
    /// var_dump((new B)->is(new A), new B instanceof $cls);
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   bool(true)
    ///   [1]=>
    ///   bool(false)
    /// }
    /// bool(true)
    /// ```
    fn instance_of(&mut self, expr: tree::Expr, class: ClassSpec) -> Result<Where, CompileError> {
        let value = self.expr(expr)?;
        if let Where::Constant(_) = value {
            return Ok(Where::Constant(Zval::bool(false)));
        }
        let class = self.class_ref(
            class,
            FETCH_CLASS_NO_AUTOLOAD | FETCH_CLASS_EXCEPTION | FETCH_CLASS_SILENT,
        )?;
        let (op1_type, op1) = self.operand(value);
        let out = self.temporary();
        let (op2_type, op2) = self.class_operand(&class)?;
        let extended_value = if op2_type == OperandKind::Const {
            self.cache_slot()
        } else {
            0
        };
        self.push(Opline {
            opcode: Opcode::Instanceof,
            op1_type,
            op1,
            op2_type,
            op2,
            out_type: OperandKind::Tmp,
            out,
            extended_value,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_add_class_name_literal` over `zend_resolve_class_name`: the
    /// name as written with its leading separator dropped, and its
    /// lowered form in the slot after it, which the engine looks the
    /// class up by (`Z_STR_P(RT_CONSTANT(opline, opline->op1) + 1)`).
    fn class_name_literal(&mut self, name: &[u8]) -> Result<u32, CompileError> {
        let resolved = self.class_name(name);
        self.resolved_class_literal(resolved)
    }

    /// `zend_add_class_name_literal` over a name already resolved, which
    /// is [`Self::func_name_literal`]'s pair.
    fn resolved_class_literal(&mut self, resolved: &[u8]) -> Result<u32, CompileError> {
        self.func_name_literal(resolved)
    }

    /// `zend_is_scope_known` (`Z/Zend/zend_compile.c:1696`): whether the
    /// class `self` names is the one being compiled. A closure can be
    /// rebound to another scope; outside a class a function's scope is
    /// none, known, and the main op array's is the includer's, not
    /// known.
    fn scope_known(&self) -> bool {
        if self.fn_flags.closure {
            return false;
        }
        match self.active_class {
            None => self.active_function.is_some(),
            Some(_) => !self.in_trait(),
        }
    }

    /// `zend_ensure_valid_class_fetch_type` (`Z/Zend/zend_compile.c:1768`):
    /// where the scope is known, `self`, `parent` and `static` outside a
    /// class and `parent` in a class that extends nothing are php's
    /// compile-time fatals.
    fn ensure_valid_class_fetch_type(&self, fetch: u32) -> Result<(), CompileError> {
        if fetch == FETCH_CLASS_DEFAULT || !self.scope_known() {
            return Ok(());
        }
        let word = match fetch {
            FETCH_CLASS_SELF => "self",
            FETCH_CLASS_PARENT => "parent",
            _ => "static",
        };
        if self.active_class.is_none() {
            return Err(self.compile_error(format!(
                "Cannot use \"{word}\" when no class scope is active"
            )));
        }
        if fetch == FETCH_CLASS_PARENT && self.active_parent.is_none() {
            return Err(
                self.compile_error("Cannot use \"parent\" when current class scope has no parent")
            );
        }
        Ok(())
    }

    /// `zend_compile_class_ref` (`Z/Zend/zend_compile.c:2819`): a name
    /// written with a leading `\` is the class of that name; `self`,
    /// `parent` and `static` are their fetch type and `flags` in an
    /// unused operand, held to [`Self::ensure_valid_class_fetch_type`];
    /// any other name is the class it resolves to. A value written where
    /// the class goes is the class it names as `ZEND_NAME_FQ`
    /// ([`Self::fq_class_name`]); any other expression is
    /// [`Self::computed_class_ref`]'s.
    fn class_ref(&mut self, class: ClassSpec, flags: u32) -> Result<ClassRef<'a>, CompileError> {
        let operand = self.decoded_class(class)?;
        self.class_ref_of(operand, flags)
    }

    /// [`Self::class_ref`] over the operand decoded.
    fn class_ref_of(
        &mut self,
        operand: ClassOperand,
        flags: u32,
    ) -> Result<ClassRef<'a>, CompileError> {
        let written = match operand {
            ClassOperand::Name(token) => self.text(token),
            ClassOperand::Value(value) => {
                return Ok(ClassRef::Named(
                    self.arena.copy(&self.fq_class_name(value)?),
                ));
            }
            ClassOperand::Expr(expr) => return self.computed_class_ref(expr, flags),
        };
        if written.starts_with(b"\\") {
            return Ok(ClassRef::Named(self.class_name(written)));
        }
        match class_fetch_type(written) {
            FETCH_CLASS_DEFAULT => Ok(ClassRef::Named(self.class_name(written))),
            fetch => {
                self.ensure_valid_class_fetch_type(fetch)?;
                Ok(ClassRef::Scope(fetch | flags))
            }
        }
    }

    /// [`Self::class_ref`] over a class operand that is no value: the
    /// expression compiled, where a constant string is read by its fetch
    /// type, a name of the default one resolved as `ZEND_NAME_FQ`
    /// ([`Self::fq_name`]), a constant of another type is php's `Illegal
    /// class name`, and anything else is `FETCH_CLASS` over the value
    /// into a `VAR`, `ZEND_FETCH_CLASS_DEFAULT` and `flags` in its `op1`.
    fn computed_class_ref(
        &mut self,
        expr: tree::Expr,
        flags: u32,
    ) -> Result<ClassRef<'a>, CompileError> {
        let value = match self.expr(expr)? {
            Where::Constant(value) => value,
            value => {
                let (op2_type, op2) = self.operand(value);
                let out = self.temporary();
                self.push(Opline {
                    opcode: Opcode::FetchClass,
                    op1: FETCH_CLASS_DEFAULT | flags,
                    op2_type,
                    op2,
                    out_type: OperandKind::Var,
                    out,
                    ..Opline::default()
                });
                return Ok(ClassRef::Fetched(out));
            }
        };
        let Some(name) = value.as_str() else {
            return Err(self.compile_error("Illegal class name"));
        };
        match class_fetch_type(name.bytes()) {
            FETCH_CLASS_DEFAULT => Ok(ClassRef::Named(
                self.arena.copy(&self.fq_name(name.bytes())?),
            )),
            fetch => {
                self.ensure_valid_class_fetch_type(fetch)?;
                Ok(ClassRef::Scope(fetch | flags))
            }
        }
    }

    /// `zend_short_circuiting_mark_inner(class_ast)` and then
    /// `zend_compile_class_ref` under `ZEND_FETCH_CLASS_EXCEPTION`, what
    /// `zend_compile_static_prop` and `zend_compile_static_call` do: a
    /// class written as a `?->` chain is a link of the chain the property
    /// or the call ends, so its `JMP_NULL` skips them too.
    ///
    /// ```php
    /// var_dump(null?->bar()::$baz, null?->bar()::baz());
    /// ```
    /// ```expect
    /// NULL
    /// NULL
    /// ```
    fn class_link(&mut self, class: ClassSpec) -> Result<ClassRef<'a>, CompileError> {
        if let ClassSpec::Expr(expr) = class {
            self.chain_inner = Some(self.peel(expr));
        }
        self.class_ref(class, FETCH_CLASS_EXCEPTION)
    }

    /// `zend_set_class_name_op1` and its kin: the operand a class
    /// reference is on the opline that reads it, the constant pair of a
    /// class named at compile time added here, the fetch type in an
    /// unused operand, or the `VAR` a `FETCH_CLASS` wrote.
    fn class_operand(&mut self, class: &ClassRef) -> Result<(OperandKind, u32), CompileError> {
        Ok(match class {
            ClassRef::Named(name) => (OperandKind::Const, self.resolved_class_literal(name)?),
            ClassRef::Scope(fetch) => (OperandKind::Unused, *fetch),
            ClassRef::Fetched(at) => (OperandKind::Var, *at),
        })
    }

    /// `zend_get_compatible_func_or_null` over `CG(class_table)`: whether
    /// the class named, one the engine declared ([`Options::classes`])
    /// or one this file has bound before the call, has a public method
    /// of the name, its own or one it inherited
    /// ([`Self::linked_method`]); a private or protected one is
    /// compatible only from the class itself while it compiles,
    /// `ce == CG(active_class_entry)`, which finds its own method too
    /// ([`Self::table_method`]). The name is resolved as the fetch
    /// resolves it ([`Self::class_name`]), which is the name a
    /// declaration of the file took under the same namespace.
    #[cfg(test)]
    fn known_method(&self, class: &[u8], method: &[u8]) -> Option<Known> {
        self.known_method_of(self.class_name(class), method)
            .map(|(known, _)| known)
    }

    /// [`Self::known_method`] over a name already resolved, with the
    /// method's parameters as `zend_compile_args` reads them.
    ///
    /// A method the class inherits is in its table once the early bind
    /// has linked it, so a call to it is the method's, `DO_UCALL` for a
    /// user's, with its arguments sent by its parameters:
    ///
    /// ```php
    /// class A { static function f(&$r) { $r = static::class; } }
    /// class B extends A {}
    /// B::f($x);
    /// echo $x, "\n";
    /// ```
    /// ```expect
    /// B
    /// ```
    fn known_method_of(&self, class: &[u8], method: &[u8]) -> Option<(Known, Callee)> {
        // `zend_hash_find_ptr(CG(class_table), lcname)`: a class the
        // engine declared or one of the file bound early, linked, and
        // never one a `DECLARE_CLASS` binds, which the table holds
        // under its key. The protected arm of
        // `zend_get_compatible_func_or_null` asks for a linked
        // `CG(active_class_entry)`, and no class is linked while its
        // body compiles, so only a public method is compatible here.
        if let Some(decl) = self.bound_class(class) {
            let (holder, found) = self.linked_method(decl, method)?;
            if found.visibility != Visibility::Public {
                return None;
            }
            let callee = Callee::of(&found.function.parameters);
            if self.known_classes.find(&holder.name.0).is_some() {
                return Some((Known::Native, callee));
            }
            let flagged = attributes::deprecated_or_no_discard(&found.function.attributes);
            return Some((Known::User { flagged }, callee));
        }
        // Else `CG(active_class_entry)` by its name.
        let active = self.active_class.as_ref()?;
        if !active.name.eq_ignore_ascii_case(class) {
            return None;
        }
        self.known_method_of_active(method)
    }

    /// [`Self::known_method_of`] over `CG(active_class_entry)`, the class
    /// whose body is compiling, which `self::` names whatever the table
    /// holds under its name, and which may call any method of its own.
    /// The methods `zend_enum_register_funcs` gives an enum are added at
    /// the link, so while the body compiles the table holds an enum's
    /// own `cases` alone, where it declares one.
    fn known_method_of_active(&self, method: &[u8]) -> Option<(Known, Callee)> {
        let at = self.active_class.as_ref()?.at;
        self.table_method(at, method)
            .map(|m| (Known::User { flagged: m.flagged }, m.callee))
    }

    /// `zend_hash_find_ptr(&ce->function_table, lcname)` over a class
    /// `CG(class_table)` holds linked: its own method of the name, else
    /// the one its parent's table holds, which `do_inherit_method` put
    /// in its own, with the class that declared it. A class bound while
    /// the file compiles implements no interface and uses no trait, and
    /// one the engine declared declares what it implements, so the
    /// parents' tables are the whole of it. The walk stops at a depth
    /// of 64, as the linker's own walks up a chain do, so a chain no
    /// link has refused yet cannot loop it.
    fn linked_method<'d>(
        &'d self,
        mut class: &'d ClassDecl,
        method: &[u8],
    ) -> Option<(&'d ClassDecl, &'d MethodDecl)> {
        for _ in 0..64 {
            if let Some(found) =
                (class.methods.iter()).find(|m| m.function.name.0.eq_ignore_ascii_case(method))
            {
                return Some((class, found));
            }
            class = self.bound_class(&class.parent.as_ref()?.0)?;
        }
        None
    }

    /// `zend_add_func_name_literal`: the name as written and its lowered
    /// form, two slots in a row whatever the table already holds, since
    /// the engine reads the second as the one after the first
    /// (`Z_STR_P(function_name+1)`).
    fn func_name_literal(&mut self, name: &[u8]) -> Result<u32, CompileError> {
        let written = self.string(name)?;
        let lowered = self.lowered(written, name)?;
        let at = self.literal(written);
        self.literal(lowered);
        Ok(at)
    }

    /// `zend_string_tolower` of the string `written` holds, whose bytes
    /// are `bytes`: a string of its own where a byte is upper case, and
    /// else `written` itself with one more owner, as `zend_string_tolower`
    /// copies a string it would not change. So a call's method name
    /// written lower case is one string in its two slots, where the
    /// class's name is two:
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3283-lowered.inc';
    /// file_put_contents($f, <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// m('class X9 { static function sm(&$r) { $r = 7; } static function sv($r) { } }');
    /// foreach ([
    ///     'function f13() { X9::sm($b); return $b; }',
    ///     'function f14() { X9::sv(1); }',
    ///     'function f15() { X9::sv($b); }',
    ///     'function f16() { Y9::sv($b); }',
    ///     'function f17() { x9::sv(1); }',
    ///     'function f18() { \\X9::sv(1); }',
    ///     'function f19() { foo_undefined(); }',
    ///     'function f20($o) { $o->method_name(); }',
    ///     'function f21($o) { return $o instanceof x9; }',
    /// ] as $c) echo m($c), "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 528
    /// 480
    /// 496
    /// 496
    /// 448
    /// 480
    /// 360
    /// 440
    /// 432
    /// ```
    fn lowered(&mut self, written: Zval, bytes: &[u8]) -> Result<Zval, CompileError> {
        let lowered = self.arena.lowercase(bytes);
        if lowered == bytes && self.strings.hold(written) {
            return Ok(written);
        }
        self.string(lowered)
    }

    /// `zend_compile_function_name` over `zend_resolve_function_name`:
    /// the name a call is resolved to through the file's imports
    /// ([`Self::resolve_non_class`]), and whether the resolution waits
    /// for the program to be served, which is php's
    /// `!is_fully_qualified && FC(current_namespace)`: an unqualified
    /// name no `use function` names, inside a namespace.
    fn function_name(&self, name: &[u8]) -> (&'a [u8], bool) {
        let (resolved, fully_qualified) = self.resolve_non_class(name, Symbol::Function);
        (resolved, !fully_qualified && self.namespace.is_some())
    }

    /// A call of a name written as such, `ZEND_AST_CALL` whose name is a
    /// `ZEND_AST_ZVAL` string: that name lowered as
    /// `zend_resolve_function_name` resolves it through the file's
    /// imports, and the arguments as written; nothing for a call of
    /// anything else, a parenthesised name among them, which php's
    /// grammar makes a constant, and nothing for anything but a call.
    /// What `zend_compile_func_array_slice` and `zend_compile_func_cufa`
    /// read of the call written as their argument.
    pub(crate) fn named_call(&self, expr: tree::Expr) -> Option<(Vec<u8>, &[tree::Arg])> {
        let tree::Expr::CallExpr(call) = self.peel(expr) else {
            return None;
        };
        let node = &self.parsed.tree[call];
        let tree::Expr::NameRef(name) = node.callee else {
            return None;
        };
        let (resolved, _) = self.function_name(self.text(name));
        Some((
            resolved.to_ascii_lowercase(),
            self.list(self.parsed.tree[node.args].args),
        ))
    }

    /// [`Self::function_name`] for a constant, `zend_resolve_const_name`,
    /// which reads the constant imports, whose aliases keep their case;
    /// a name that is not fully qualified, inside a namespace, is php's
    /// `IS_CONSTANT_UNQUALIFIED_IN_NAMESPACE`, fetched with the global
    /// fallback.
    fn constant_resolved(&self, name: &[u8]) -> ConstantName {
        let (resolved, fully_qualified) = self.resolve_non_class(name, Symbol::Const);
        let resolved = Name(resolved.into());
        if !fully_qualified && self.namespace.is_some() {
            ConstantName::UnqualifiedInNamespace(resolved)
        } else {
            ConstantName::Resolved(resolved)
        }
    }

    /// `zend_resolve_class_name`: `self`, `parent` and `static` written
    /// without the `\` are no class's name and resolve to themselves;
    /// every other name is [`Self::resolve_class`]'s, the file's imports
    /// read, with nothing waiting for the program, since a class name
    /// resolves at compile time.
    fn class_name(&self, name: &[u8]) -> &'a [u8] {
        if class_fetch_type(name) != FETCH_CLASS_DEFAULT {
            return self.arena.copy(name);
        }
        self.resolve_class(name)
    }

    /// `zend_prefix_with_ns`: the name a declaration takes, the current
    /// namespace, a `\` and the name as written where a namespace
    /// stands, and the name as written where none does.
    fn prefix_with_ns(&self, name: &[u8]) -> &'a [u8] {
        let Some(namespace) = &self.namespace else {
            return self.arena.copy(name);
        };
        self.arena.concat(&[namespace, b"\\", name])
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
    /// working directory, which `%s` stands for since each adapter's is
    /// a scratch directory of its own:
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
    ///
    /// and so it is where the class is declared inside a function, whose
    /// op array the constant is compiled in. A closure's scope and a
    /// function's written inside a method are none while the file
    /// compiles, so `__METHOD__` is the op array's own name in both, and
    /// the function has no class at all (`CG(active_class_entry) =
    /// NULL` for a function declaration):
    ///
    /// ```php
    /// function h() { class B { const X = __METHOD__, Y = __FUNCTION__; } }
    /// h();
    /// class C {
    ///     function m() {
    ///         $f = function () { return __METHOD__; };
    ///         function g() { return __CLASS__ . '|' . __METHOD__; }
    ///         return $f() . ' ' . g() . ' ' . __METHOD__;
    ///     }
    /// }
    /// echo '[', B::X, '] ', B::Y, ' ', (new C)->m(), "\n";
    /// ```
    /// ```expect
    /// [] h {closure:C::m():6} |g C::m
    /// ```
    ///
    /// `__PROPERTY__` is the property a declaration, its attributes, its
    /// default or one of its `get` and `set` bodies is compiled for:
    ///
    /// ```php
    /// #[Attribute] class A { function __construct(public $m) {} }
    /// class C { #[A(__PROPERTY__)] public $p = __PROPERTY__ { get => $this->p . __PROPERTY__; } }
    /// $r = new ReflectionProperty('C', 'p');
    /// echo $r->getAttributes()[0]->getArguments()[0], ' ', (new C)->p, "\n";
    /// ```
    /// ```expect
    /// p pp
    /// ```
    ///
    /// `__DIR__` is laid at the length of the whole file name and cut
    /// where it stands ([`Folded::Dir`]), and a unit compiled for the
    /// request alone takes the engine's string where opcache holds the
    /// bytes, as it holds the working directory once a relative path is
    /// keyed, `witness.php`'s: so an `eval` that program runs costs no
    /// string, where one a program in a directory under the recipe's
    /// `TMPDIR` runs, and an include written there during the request,
    /// costs a string of the file name's length, which is longer by a bin:
    ///
    /// ```php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// $fc = $fd = $fe = null; m('$GLOBALS["fz"] = 1;');
    /// echo m('$GLOBALS["fc"] = __DIR__;'), "\n";
    /// $d = sys_get_temp_dir() . '/w3324'; @mkdir($d);
    /// $put = function ($name, $src) use ($d) { $f = "$d/$name"; file_put_contents("$f.tmp", $src); touch("$f.tmp", time() - 10); rename("$f.tmp", $f); return $f; };
    /// $f = $put('dir.inc', <<<'PHP'
    /// <?php
    /// ini_set('opcache.file_update_protection', '2');
    /// function inc($name, $src) { $f = dirname(__FILE__) . '/' . str_pad($name, 40, '-') . '.inc'; file_put_contents($f, $src); $m = memory_get_usage(); include $f; $d = memory_get_usage() - $m; unlink($f); return $d; }
    /// function here($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// inc('w0', '<?php class W {} $q0 = 1;');
    /// echo inc('main', '<?php $GLOBALS["fd"] = __DIR__;'), "\n";
    /// echo inc('fn', '<?php function fa3() { return __DIR__; }'), "\n";
    /// echo here('$GLOBALS["fe"] = __DIR__;'), "\n";
    /// PHP);
    /// include $f;
    /// $junk = array_fill(0, 1000, str_repeat("x", 100));
    /// echo $fc === __DIR__, $fd === $d, fa3() === $d, $fe === $d, "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 0
    /// 256
    /// 400
    /// 112
    /// 1111
    /// ```
    fn magic_const(&self, which: Magic, line: u32) -> Folded {
        let text = match which {
            Magic::Line => return Folded::Line(i64::from(line)),
            Magic::File => return Folded::FileName,
            Magic::Dir => {
                let dirname = dirname(self.filename);
                return if dirname == b"." {
                    Folded::Dir(self.cwd.to_vec(), MAXPATHLEN)
                } else {
                    Folded::Dir(dirname, self.filename.len())
                };
            }
            Magic::Namespace => self.namespace.clone().unwrap_or_default(),
            Magic::Class if self.in_trait() => return Folded::UsingClass,
            Magic::Class => return Folded::ClassName,
            Magic::Property => self.active_property.clone().unwrap_or_default(),
            // `zend_compile_func_decl` names the op array; the main one
            // has no `function_name`, and a member's constant expression
            // stands directly inside the class.
            Magic::Function if self.active_function.is_some() => return Folded::FunctionName,
            // `__METHOD__` is `Class::method` inside a method, the op
            // array's own name inside a function or a closure, whose
            // scope the compiler leaves unset, and nothing directly
            // inside a class body, whatever op array the class stands in
            // (`zend_try_ct_eval_magic_const`'s `T_METHOD_C`).
            Magic::Method => match (
                self.active_name(),
                self.active_function.as_ref().map(|function| &function.name),
            ) {
                (Some(class), Some(function)) if self.fn_flags.method => {
                    let mut text = class;
                    text.extend_from_slice(b"::");
                    text.extend_from_slice(function);
                    text
                }
                (Some(_), Some(_)) if !self.fn_flags.closure => Vec::new(),
                (_, Some(_)) => return Folded::FunctionName,
                (_, None) => Vec::new(),
            },
            // The trait's own name inside one, and none elsewhere.
            Magic::Trait if self.in_trait() => return Folded::ClassName,
            Magic::Function | Magic::Trait => Vec::new(),
        };
        Folded::Text(text)
    }

    /// [`Self::named_constant`] read once and destroyed, `zval_ptr_dtor`
    /// of the copy, as `zend_compile_func_in_array` reads a `$strict`
    /// written as a constant: what `zend_is_true` reads off it, and
    /// nothing where the name is left to the fetch.
    pub(crate) fn named_constant_truth(&self, token: TokenId) -> Option<Truth> {
        if let Some(value) = self.constant_name(token) {
            return Some(Truth::of(value));
        }
        let resolved = self.constant_resolved(self.text(token));
        self.ct_found(&resolved.resolved().0).map(Found::truth)
    }

    /// `zend_try_ct_eval_const` over the name `zend_resolve_const_name`
    /// resolved ([`Self::ct_constant`]): a bare name inside a namespace is
    /// looked for under the namespace, as `zend_compile_const` asks for
    /// `resolved_name` whatever `is_fully_qualified` says, and its global
    /// fallback is the fetch's to make. A function's copy is one more
    /// owner of the constant's string, where `b`'s fetch takes none:
    ///
    /// ```php
    /// define('a\S', str_repeat('q', count($argv) + 9));
    /// eval('namespace a; function f() { return S; } function g() { return \strlen(S); }');
    /// eval('namespace b; function h() { return S; }');
    /// debug_zval_dump(\a\S);
    /// var_dump(\a\f(), \a\g());
    /// ```
    /// ```expect regime=jit-off
    /// string(10) "qqqqqqqqqq" refcount(3)
    /// string(10) "qqqqqqqqqq"
    /// int(10)
    /// ```
    fn const_name(&mut self, name: &[u8]) -> Result<Option<Operated>, CompileError> {
        let resolved = self.constant_resolved(name);
        self.ct_constant(&resolved.resolved().0)
    }

    /// Whether `zend_try_ct_eval_const` substitutes a name with no
    /// namespace in it ([`Self::ct_found`]), what
    /// `zend_compile_func_defined` folds to `true`: php destroys the copy
    /// it made at once (`zval_ptr_dtor`), so nothing is copied here.
    pub(crate) fn ct_substitutes(&self, name: &[u8]) -> bool {
        self.ct_found(name).is_some()
    }

    /// `zend_compile_const`'s second half (`Z/Zend/zend_compile.c:11048`),
    /// the name [`Self::const_name`] cannot fold: `FETCH_CONSTANT` into a
    /// temporary with the resolved name in `op2`, the bit
    /// `IS_CONSTANT_UNQUALIFIED_IN_NAMESPACE` in `op1` where the name
    /// stood unqualified inside a namespace, and the cache slot
    /// `zend_alloc_cache_slot` takes in `extended_value`. The resolution
    /// is [`Self::constant_resolved`]'s, `zend_resolve_const_name` over
    /// the constant imports: a leading `\`, a `\` inside the name and an
    /// imported alias are all fully qualified, and only a bare name no
    /// `use const` names, in a namespace, waits for the program.
    ///
    /// ```php
    /// define("W", 1);
    /// var_dump(W);
    /// ```
    /// ```expect
    /// int(1)
    /// ```
    fn const_fetch(&mut self, token: TokenId) -> Result<Where, CompileError> {
        let name = &self.text(token).to_vec();
        if let Some(offset) = self.halt.filter(|_| self.names_halt_offset(name)) {
            return Ok(Where::Constant(Zval::long(offset)));
        }
        if let Some(folded) = self.const_name(name)? {
            return Ok(Where::Constant(
                self.claimed(tree::Expr::NameRef(token), folded),
            ));
        }
        let resolved = self.constant_resolved(name);
        let op2 = self.const_name_literal(&resolved)?;
        let extended_value = self.cache_slot();
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::FetchConstant,
            op1: match resolved {
                ConstantName::UnqualifiedInNamespace(_) => UNQUALIFIED_IN_NAMESPACE,
                ConstantName::Resolved(_) => 0,
            },
            op2_type: OperandKind::Const,
            op2,
            out_type: OperandKind::Tmp,
            out,
            extended_value,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_const`'s first test (`Z/Zend/zend_compile.c:11024`):
    /// the name resolves to `__COMPILER_HALT_OFFSET__`, or is written so
    /// with no `namespace\` before it.
    fn names_halt_offset(&self, name: &[u8]) -> bool {
        const HALT: &[u8] = b"__COMPILER_HALT_OFFSET__";
        self.constant_resolved(name).resolved().0.as_ref() == HALT
            || (!name.starts_with(b"namespace\\") && name == HALT)
    }

    /// `zend_compile_halt_compiler` (`Z/Zend/zend_compile.c:9804`): no
    /// opline, the file's offset kept for the unit, which the engine
    /// registers as the file's `__COMPILER_HALT_OFFSET__`; refused inside
    /// a bracketed namespace.
    ///
    /// ```php
    /// function offset() { return __COMPILER_HALT_OFFSET__; }
    /// echo __COMPILER_HALT_OFFSET__, ' ', offset(), ' ', constant('__COMPILER_HALT_OFFSET__'), "\n";
    /// echo strlen(file_get_contents(__FILE__, false, null, __COMPILER_HALT_OFFSET__)), "\n";
    /// __halt_compiler(); ?>
    /// data after the halt
    /// ```
    /// ```expect
    /// 261 261 261
    /// 23
    /// ```
    pub(super) fn halt_compiler(
        &mut self,
        id: NodeId<tree::HaltCompilerStmt>,
    ) -> Result<(), CompileError> {
        if matches!(self.namespacing, Namespacing::Bracketed { open: true }) {
            return Err(
                self.compile_error("__HALT_COMPILER() can only be used from the outermost scope")
            );
        }
        // The offset the parser took from the scanner, in the script as
        // read (`zend_get_scanned_file_offset`), which [`Emit::halt`] holds.
        self.halted = Some(self.halt.unwrap_or_else(|| walk::halt_end(self.parsed, id)));
        Ok(())
    }

    /// `zend_add_const_name_literal` (`Z/Zend/zend_compile.c:664`): the
    /// resolved name, and after it the key php's constant table is
    /// written under, the same name with its namespace lowered
    /// (`zend_register_constant`), which is the one the handler looks
    /// the constant up by. Where the name stood unqualified in a
    /// namespace a third slot follows, the name without its namespace,
    /// which is the global spelling the handler falls back to. A name
    /// with no namespace in it takes its own spelling twice, so the slot
    /// after `op2` is the key whatever the name was. The answer is the
    /// slot of the name as resolved, which is the one the opline names
    /// and the one php's error text reads.
    fn const_name_literal(&mut self, resolved: &ConstantName) -> Result<u32, CompileError> {
        let name = resolved.resolved().0.as_ref();
        let written = self.string(name)?;
        let at = self.literal(written);
        let Some(slash) = name.iter().rposition(|byte| *byte == b'\\') else {
            let bare = self.string(name)?;
            self.literal(bare);
            return Ok(at);
        };
        let mut lowered = name.to_vec();
        if let Some(namespace) = lowered.get_mut(..slash) {
            namespace.make_ascii_lowercase();
        }
        let lowered = self.string(&lowered)?;
        self.literal(lowered);
        if let Some(global) = resolved.fallback() {
            let bare = self.string(global)?;
            self.literal(bare);
        }
        Ok(at)
    }

    /// `find_frameless_function_info` (`Z/Zend/zend_compile.c:4649`): the
    /// arity of the native's first frameless entry that serves a call
    /// of `count` arguments, none where none does. An entry serves the
    /// call when its arity is at least the count, the native's required
    /// count is at most the count, and a variadic native's entry is of
    /// the count exactly; a call of more than three arguments has none.
    fn frameless_arity(&self, index: usize, count: usize) -> Option<u32> {
        let decl = self.functions.rows().get(index)?;
        if count > 3 || self.hooked.natives {
            return None;
        }
        let required = decl
            .parameters
            .iter()
            .take_while(|p| p.is_required())
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
        arguments: &[Source],
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
                values.push(self.source(*argument)?);
                continue;
            }
            let default = self
                .functions
                .rows()
                .get(index)
                .and_then(|decl| decl.parameters.get(at))
                .and_then(|parameter| parameter.default.expr())
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
            ConstExpr::Long { value: n, .. } => Some(Zval::long(*n)),
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
    fn ns_call(
        &mut self,
        name: &[u8],
        arguments: &Args,
        fetch: Fetch,
    ) -> Result<Where, CompileError> {
        let line = self.line;
        let name_constants = self.ns_func_name_literal(name)?;
        let lowered = self.arena.lowercase(name);
        let unqualified = lowered
            .rsplit(|b| *b == b'\\')
            .next()
            .unwrap_or_default()
            .to_vec();
        let mut branch = None;
        // A named argument keeps the call out of the frameless branch
        // (`!zend_args_contain_unpack_or_named`).
        let positional = arguments.positional();
        if !self.in_jmp_frameless_branch
            && fetch == Fetch::Read
            && let Some(positional) = &positional
            && let Some(index) = self.native_index(&unqualified)
            && self.frameless_arity(index, positional.len()).is_some()
        {
            self.in_jmp_frameless_branch = true;
            let over = self.string(lowered)?;
            let op1 = self.literal(over);
            let at = self.push(Opline {
                opcode: Opcode::JmpFrameless,
                op1_type: OperandKind::Const,
                op1,
                ..Opline::default()
            });
            branch = Some((at, index));
        }
        let plan = Plan::of(self.arena, arguments, None);
        let slot = self.cache_slot();
        self.push(Opline {
            opcode: Opcode::InitNsFcallByName,
            op2_type: OperandKind::Const,
            op2: name_constants,
            out: slot,
            extended_value: plan.count,
            ..Opline::default()
        });
        let call_op = self.get_call_op(Fbc::ByName, false);
        let returned = self.call_common(&plan, arguments, None, call_op, line)?;
        if let Some((jmp_fl, index)) = branch
            && let Some(positional) = &positional
            && let Where::Returned(var) = returned
        {
            let jmp_end = self.push(Opline {
                opcode: Opcode::Jmp,
                ..Opline::default()
            });
            let target = self.next();
            self.frameless_icall(index, positional, Some(var))?;
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
        let unqualified = (self.arena.lowercase(name).rsplit(|b| *b == b'\\'))
            .next()
            .unwrap_or_default();
        let written = self.string(name)?;
        let lowered = self.lowered(written, name)?;
        let unqualified = self.string(unqualified)?;
        let at = self.literal(written);
        self.literal(lowered);
        self.literal(unqualified);
        Ok(at)
    }

    /// `zend_compile_call_common` past the `INIT_*` it follows: the
    /// arguments [`Plan`] placed sent and the call op, or, for the `...`
    /// of a first-class callable, nothing sent, the `INIT_*` counting no
    /// argument, and `CALLABLE_CONVERT` into a temporary, which makes a
    /// closure of the call the `INIT_*` began and serves nothing.
    ///
    /// ```php
    /// class C {
    ///     function m($x) { return "m$x"; }
    ///     static function s($x) { return "s$x"; }
    ///     function viaStatic() { return static::s(...); }
    /// }
    /// $o = new C;
    /// $name = 'm';
    /// $f = strlen(...);
    /// $g = $o->m(...);
    /// $h = $o->$name(...);
    /// $i = C::s(...);
    /// $j = $f(...);
    /// $k = (fn ($x) => "k$x")(...);
    /// var_dump($f('abc'), $g(1), $h(2), $i(3), $o->viaStatic()(4), $j === $f, $k(5));
    /// ```
    /// ```expect
    /// int(3)
    /// string(2) "m1"
    /// string(2) "m2"
    /// string(2) "s3"
    /// string(2) "s4"
    /// bool(true)
    /// string(2) "k5"
    /// ```
    ///
    /// `new` has no function to make a closure of, and a `?->` the
    /// receiver stands on would leave none to make it of, each php's
    /// compile error:
    ///
    /// ```php
    /// class C {}
    /// $c = new C(...);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot create Closure for new expression in %s on line 3
    /// ```
    ///
    /// ```php
    /// $o = null;
    /// $f = $o?->m(...);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot combine nullsafe operator with Closure creation in %s on line 3
    /// ```
    fn call_common(
        &mut self,
        plan: &Plan,
        arguments: &Args,
        callee: Option<&Callee>,
        opcode: Opcode,
        line: u32,
    ) -> Result<Where, CompileError> {
        if let Args::CallableConvert = arguments {
            let out = self.temporary();
            self.push(Opline {
                opcode: Opcode::CallableConvert,
                out_type: OperandKind::Tmp,
                out,
                ..Opline::default()
            });
            return Ok(Where::Temporary(out));
        }
        self.send_args(plan, arguments, callee)?;
        Ok(self.extended_call(opcode, line, plan.extra_named))
    }

    /// `zend_get_call_op` (`Z/Zend/zend_compile.c:3919-3949`): the
    /// opcode that passes a hook by only where the hook it passes is
    /// php's own ([`crate::Hooked`]), `DO_FCALL_BY_NAME` for a known
    /// callee `flagged` `#[\Deprecated]` or `#[\NoDiscard]` where nothing
    /// reads what it returns, and `DO_FCALL` for every other call.
    pub(crate) fn get_call_op(&self, fbc: Fbc, flagged: bool) -> Opcode {
        let hooked = self.hooked;
        match fbc {
            Fbc::Native if !hooked.natives && flagged => Opcode::DoFcallByName,
            Fbc::Native if !hooked.natives => Opcode::DoIcall,
            Fbc::User if !hooked.user && flagged => Opcode::DoFcallByName,
            Fbc::User if !hooked.user => Opcode::DoUcall,
            Fbc::ByName if hooked == crate::Hooked::NONE => Opcode::DoFcallByName,
            Fbc::Native | Fbc::User | Fbc::ByName => Opcode::DoFcall,
        }
    }

    /// The call op `zend_get_call_op` picked, with the `VAR` for what
    /// the call returns, and `ZEND_FCALL_MAY_HAVE_EXTRA_NAMED_PARAMS` in
    /// `extended_value` where a named argument may land in a variadic.
    fn call_op(&mut self, opcode: Opcode, line: u32, extra_named: bool) -> Where {
        // `opline->lineno = lineno` after `zend_emit_op`
        // (`Z/Zend/zend_compile.c:3999`): the call's own line, not the
        // line the last argument ended on.
        self.line = line;
        let out = self.temporary();
        self.push(Opline {
            opcode,
            out_type: OperandKind::Var,
            out,
            extended_value: u32::from(extra_named),
            ..Opline::default()
        });
        Where::Returned(out)
    }

    /// Whether the return type declared holds `never`,
    /// `ZEND_TYPE_CONTAINS_CODE(return_info->type, IS_NEVER)`.
    fn returns_never(&self) -> bool {
        self.return_type
            .as_ref()
            .is_some_and(|ty| ty.contains_code(NEVER_CODE))
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
        if let Some(ty) = self
            .return_type
            .clone()
            .filter(|_| !self.fn_flags.generator)
        {
            if ty.contains_code(NEVER_CODE) {
                self.push(Opline {
                    opcode: Opcode::VerifyNeverType,
                    ..Opline::default()
                });
                return Ok(());
            }
            self.return_type_check(Where::Constant(Zval::null()), false, true)?;
        }
        let value = self.literal(if return_one {
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

/// One variable a closure binds: its name, its slot in the closure's
/// `op_array->static_variables`, whether an `&` stood before it and
/// whether it is an arrow function's implicit capture. php holds the
/// two modes and the slot in one `extended_value`
/// ([`Bind::slot_and_mode`]).
struct Bind {
    name: Vec<u8>,
    slot: u32,
    by_ref: bool,
    implicit: bool,
    line: u32,
}

impl Bind {
    /// The `extended_value` of a `BIND_LEXICAL` or a `BIND_STATIC`: the
    /// slot in the low bits above the three binding bits php reserves,
    /// `ZEND_BIND_REF`, `ZEND_BIND_IMPLICIT` and `ZEND_BIND_EXPLICIT`
    /// (`Z/Zend/zend_compile.h:1193-1196`), which every reader masks
    /// off with `~(REF|IMPLICIT|EXPLICIT)`. php's slot is the bound
    /// value's byte offset in the table's `arData`, a multiple of the
    /// bucket size and so clear of those bits; nothing observes the
    /// number itself, so the slot stands where the offset stood.
    /// `ZEND_BIND_EXPLICIT` is the `BIND_STATIC`'s alone, which is
    /// `in_body`: `zend_compile_closure_binding` passes the `use`
    /// list's own attribute and `zend_compile_closure_uses` adds the
    /// bit.
    fn slot_and_mode(&self, in_body: bool) -> u32 {
        let mut mode = 0;
        if self.by_ref {
            mode |= 1;
        }
        if self.implicit {
            mode |= 2;
        }
        if in_body && !self.implicit {
            mode |= 4;
        }
        (self.slot << 3) | mode
    }
}

impl Emit<'_> {
    /// `find_implicit_binds_recursively` (`Z/Zend/zend_compile.c:8089`):
    /// every variable `expr` reads, in the order it reads them, each
    /// name once, php's walk over the children of every node in php's
    /// child order. `$this` and the names `zend_is_auto_global` answers
    /// for need no import and are left out; a `ZEND_AST_VAR` whose name
    /// is one string value is that name, and over anything else the
    /// walk goes on into the name, `varvars_used` being a bit php sets
    /// and never reads; a closure written inside contributes its `use`
    /// list and not its body, since its own body binds through that
    /// list; an arrow function inside contributes what its body reads.
    /// `ZEND_AST_ARRAY_ELEM` and `ZEND_AST_YIELD` are `(value, key)`, so
    /// the value's names come before the key's. A `.` over two literals
    /// is one value to php's parser and two nodes here, so `${'a' . 'b'}`
    /// binds nothing here.
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per node kind, which is what php's walk over the children of a node it makes no special case of comes to here: the length is the number of kinds `Expr` has, and there is nothing in it to shorten that would not be a second walk"
    )]
    fn implicit_uses(&self, names: &mut Vec<Vec<u8>>, expr: tree::Expr) {
        let tree = &self.parsed.tree;
        match expr {
            tree::Expr::SimpleVarExpr(id) => {
                if let Some(var) = tree[id].var {
                    note_var(names, self.variable_name(var));
                }
            }
            tree::Expr::VarVarExpr(id) => self.implicit_uses_of_name(names, tree[id].inner),
            tree::Expr::DynamicVarExpr(id) => self.implicit_uses_of_name(names, tree[id].inner),
            tree::Expr::ParenExpr(id) => self.implicit_uses_of_any(names, tree[id].inner),
            tree::Expr::LiteralExpr(_)
            | tree::Expr::MagicConstExpr(_)
            | tree::Expr::NowdocExpr(_)
            | tree::Expr::NameRef(_) => {}
            tree::Expr::InterpolatedStringExpr(id) => {
                self.implicit_uses_of_parts(names, tree.list(tree[id].parts));
            }
            tree::Expr::HeredocExpr(id) => {
                self.implicit_uses_of_parts(names, tree.list(tree[id].parts));
            }
            tree::Expr::ShellExecExpr(id) => {
                self.implicit_uses_of_parts(names, tree.list(tree[id].parts));
            }
            tree::Expr::ArrayExpr(id) => {
                self.implicit_uses_of_elements(names, tree.list(tree[id].array_elements));
            }
            tree::Expr::ListExpr(id) => {
                self.implicit_uses_of_elements(names, tree.list(tree[id].array_elements));
            }
            tree::Expr::UnaryExpr(id) => self.implicit_uses_of_any(names, tree[id].operand),
            tree::Expr::PrefixIncDecExpr(id) => self.implicit_uses_of_any(names, tree[id].operand),
            tree::Expr::PostfixIncDecExpr(id) => self.implicit_uses(names, tree[id].operand),
            tree::Expr::CastExpr(id) => self.implicit_uses_of_any(names, tree[id].operand),
            tree::Expr::ErrorSuppressExpr(id) => self.implicit_uses_of_any(names, tree[id].operand),
            tree::Expr::CloneExpr(id) => {
                let node = &tree[id];
                self.implicit_uses_of_any(names, node.operand);
                if let Some(args) = node.args {
                    self.implicit_uses_of_args(names, args);
                }
            }
            tree::Expr::PrintExpr(id) => self.implicit_uses_of_any(names, tree[id].operand),
            tree::Expr::ThrowExpr(id) => self.implicit_uses_of_any(names, tree[id].operand),
            tree::Expr::BinaryExpr(id) => {
                let node = &tree[id];
                self.implicit_uses(names, node.lhs);
                self.implicit_uses_of_any(names, node.rhs);
            }
            tree::Expr::InstanceofExpr(id) => {
                let node = &tree[id];
                self.implicit_uses(names, node.lhs);
                self.implicit_uses_of_any(names, node.rhs);
            }
            tree::Expr::AssignExpr(id) => {
                let node = &tree[id];
                self.implicit_uses(names, node.lhs);
                self.implicit_uses_of_any(names, node.rhs);
            }
            tree::Expr::TernaryExpr(id) => {
                let node = &tree[id];
                self.implicit_uses(names, node.cond);
                self.implicit_uses_of_any(names, node.then);
                self.implicit_uses_of_any(names, node.else_branch);
            }
            tree::Expr::MatchExpr(id) => {
                let node = &tree[id];
                self.implicit_uses_of_any(names, node.subject);
                for arm in self.list(node.match_arms) {
                    let arm = &tree[*arm];
                    if let Some(conds) = arm.conds {
                        for cond in tree.list(tree[conds].exprs) {
                            self.implicit_uses(names, *cond);
                        }
                    }
                    self.implicit_uses_of_any(names, arm.body);
                }
            }
            tree::Expr::NewExpr(id) => {
                let node = &tree[id];
                self.implicit_uses_of_any(names, node.ctor);
                if let Some(args) = node.args {
                    self.implicit_uses_of_args(names, args);
                }
            }
            tree::Expr::ExitExpr(id) => {
                if let Some(args) = tree[id].args {
                    self.implicit_uses_of_args(names, args);
                }
            }
            tree::Expr::YieldExpr(id) => {
                let node = &tree[id];
                self.implicit_uses_of_any(names, node.value);
                if let Some(key) = node.key {
                    self.implicit_uses(names, tree[key].key);
                }
            }
            tree::Expr::YieldFromExpr(id) => self.implicit_uses_of_any(names, tree[id].source),
            tree::Expr::IncludeExpr(id) => self.implicit_uses_of_any(names, tree[id].operand),
            tree::Expr::IssetExpr(id) => {
                for target in tree.list(tree[id].targets) {
                    self.implicit_uses(names, *target);
                }
            }
            tree::Expr::EmptyExpr(id) => self.implicit_uses_of_any(names, tree[id].target),
            tree::Expr::EvalExpr(id) => self.implicit_uses_of_any(names, tree[id].code),
            tree::Expr::CallExpr(id) => {
                let node = &tree[id];
                self.implicit_uses(names, node.callee);
                // `C::$m()` names its method by the variable, a
                // `ZEND_AST_VAR` where `C::$p` alone names a property.
                if let tree::Expr::ScopedAccessExpr(scoped) = node.callee
                    && let Some(tree::ScopedSelector::SimpleVarExpr(var)) = tree[scoped].member
                {
                    self.implicit_uses(names, tree::Expr::SimpleVarExpr(var));
                }
                self.implicit_uses_of_args(names, node.args);
            }
            tree::Expr::MemberAccessExpr(id) => {
                let node = &tree[id];
                self.implicit_uses(names, node.obj);
                match node.member {
                    Some(tree::MemberSelector::SimpleVarExpr(var)) => {
                        self.implicit_uses(names, tree::Expr::SimpleVarExpr(var));
                    }
                    Some(tree::MemberSelector::VarVarExpr(var)) => {
                        self.implicit_uses(names, tree::Expr::VarVarExpr(var));
                    }
                    Some(tree::MemberSelector::DynamicVarExpr(var)) => {
                        self.implicit_uses(names, tree::Expr::DynamicVarExpr(var));
                    }
                    Some(tree::MemberSelector::BracedMember(member)) => {
                        self.implicit_uses_of_any(names, tree[member].inner);
                    }
                    Some(tree::MemberSelector::MemberName(_)) | None => {}
                }
            }
            tree::Expr::ScopedAccessExpr(id) => {
                let node = &tree[id];
                self.implicit_uses(names, node.scope);
                // `C::$p` names the property as a value, not a variable,
                // so `zend_ast_get_lineno`'s walk finds no `ZEND_AST_VAR`
                // there; `C::$$p` and `C::{expr}` name it by one.
                match node.member {
                    Some(tree::ScopedSelector::VarVarExpr(var)) => {
                        self.implicit_uses(names, tree::Expr::VarVarExpr(var));
                    }
                    Some(tree::ScopedSelector::DynamicVarExpr(var)) => {
                        self.implicit_uses(names, tree::Expr::DynamicVarExpr(var));
                    }
                    Some(tree::ScopedSelector::BracedMember(member)) => {
                        self.implicit_uses_of_any(names, tree[member].inner);
                    }
                    Some(
                        tree::ScopedSelector::MemberName(_)
                        | tree::ScopedSelector::SimpleVarExpr(_)
                        | tree::ScopedSelector::ClassKw(_),
                    )
                    | None => {}
                }
            }
            tree::Expr::SubscriptExpr(id) => {
                let node = &tree[id];
                self.implicit_uses(names, node.base);
                self.implicit_uses_of_any(names, node.index);
            }
            tree::Expr::ClosureExpr(id) => {
                if let Some(clause) = tree[id].uses {
                    for used in tree.list(tree[clause].closure_use_vars) {
                        note(names, self.variable_name(tree[*used].var));
                    }
                }
            }
            tree::Expr::ArrowFnExpr(id) => self.implicit_uses_of_any(names, tree[id].body),
        }
    }

    /// `find_implicit_binds_recursively` over a child php may leave
    /// null, which it returns on at once.
    fn implicit_uses_of_any(&self, names: &mut Vec<Vec<u8>>, expr: Option<tree::Expr>) {
        if let Some(expr) = expr {
            self.implicit_uses(names, expr);
        }
    }

    /// The name under `${…}`: one string value is the variable's name,
    /// php's `ZEND_AST_VAR` over a `ZEND_AST_ZVAL`; anything else is
    /// walked for what it reads.
    fn implicit_uses_of_name(&self, names: &mut Vec<Vec<u8>>, inner: Option<tree::Expr>) {
        let Some(inner) = inner else {
            return;
        };
        if let tree::Expr::LiteralExpr(id) = inner {
            let node = &self.parsed.tree[id];
            if let Some(TokenValue::Bytes(name)) = node
                .value
                .and_then(|value| TokenValue::of(self.parsed, Holder::Scalar(node), value))
            {
                note_var(names, &name);
                return;
            }
        }
        self.implicit_uses(names, inner);
    }

    /// The arguments of a list, each one's expression.
    fn implicit_uses_of_args(&self, names: &mut Vec<Vec<u8>>, args: NodeId<tree::ArgList>) {
        let tree = &self.parsed.tree;
        for arg in tree.list(tree[args].args) {
            match arg {
                tree::Arg::Expr(expr) => self.implicit_uses(names, *expr),
                tree::Arg::NamedArg(arg) => self.implicit_uses_of_any(names, tree[*arg].value),
                tree::Arg::SpreadArg(spread) => {
                    self.implicit_uses_of_any(names, tree[*spread].value);
                }
                tree::Arg::VariadicPlaceholder(_) => {}
            }
        }
    }

    /// The elements of an array, each `ZEND_AST_ARRAY_ELEM` its value
    /// and then its key.
    fn implicit_uses_of_elements(
        &self,
        names: &mut Vec<Vec<u8>>,
        elements: &[NodeId<tree::ArrayElement>],
    ) {
        let tree = &self.parsed.tree;
        for element in elements {
            let element = &tree[*element];
            self.implicit_uses_of_any(names, element.value);
            if let Some(key) = element.key {
                self.implicit_uses(names, tree[key].key);
            }
        }
    }

    /// The pieces of an interpolated string, php's `ZEND_AST_ENCAPS_LIST`:
    /// the variable of each, the variable a `[…]` after it indexes by,
    /// and the expression under braces.
    fn implicit_uses_of_parts(&self, names: &mut Vec<Vec<u8>>, parts: &[tree::StringPart]) {
        let tree = &self.parsed.tree;
        for part in parts {
            match *part {
                tree::StringPart::LiteralStringPart(_) => {}
                tree::StringPart::SimpleInterpolation(id) => {
                    let node = &tree[id];
                    note_var(names, self.variable_name(node.var));
                    if let Some(tree::SimpleInterpolationSuffix::SimpleDim(dim)) = node.suffix {
                        self.implicit_uses_of_dim(names, dim);
                    }
                }
                tree::StringPart::BracedInterpolation(id) => {
                    self.implicit_uses_of_any(names, tree[id].inner);
                }
                tree::StringPart::DollarBraceInterpolation(id) => {
                    let node = &tree[id];
                    if let Some(name) = node.name {
                        note_var(names, self.text(name));
                        if let Some(dim) = node.dim {
                            self.implicit_uses_of_dim(names, dim);
                        }
                    } else {
                        self.implicit_uses_of_name(names, node.inner);
                    }
                }
            }
        }
    }

    /// The `[…]` after a variable inside a string: a variable there is
    /// one the string reads.
    fn implicit_uses_of_dim(&self, names: &mut Vec<Vec<u8>>, dim: NodeId<tree::SimpleDim>) {
        if let Some(index) = self.parsed.tree[dim].index
            && self.kind(index) == SyntaxKind::VARIABLE
        {
            note_var(names, self.variable_name(index));
        }
    }
}

/// One variable's name noted, unless it is `$this` or an auto-global,
/// which need no import.
fn note_var(names: &mut Vec<Vec<u8>>, name: &[u8]) {
    if name == b"this" || is_auto_global(name) {
        return;
    }
    note(names, name);
}

/// `zend_hash_add_empty_element(&info->uses, name)`: the name once, in
/// the order it was first read, which is the order the table hands its
/// keys back and so the order the slots are taken in.
fn note(names: &mut Vec<Vec<u8>>, name: &[u8]) {
    if !names.iter().any(|held| held == name) {
        names.push(name.to_vec());
    }
}

/// What a call site knows of its callee once the table has it
/// (`zend_get_call_op`): a native, served by `DO_ICALL` or `DO_FCALL`,
/// or a user function of this file, served by `DO_UCALL`, or by
/// `DO_FCALL_BY_NAME` where `flagged`, its `fn_flags` carrying
/// `ZEND_ACC_DEPRECATED` or `ZEND_ACC_NODISCARD`, which the one tests
/// and the other does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Known {
    Native,
    User { flagged: bool },
}

/// The binary operator a token is, php's `zend_compile_binary_op`
/// opcode by the token; none for `|>`, which the emitter does not read.
fn binary_op(kind: SyntaxKind) -> Option<BinaryOp> {
    Some(match kind {
        SyntaxKind::PLUS => BinaryOp::Add,
        SyntaxKind::MINUS => BinaryOp::Sub,
        SyntaxKind::STAR => BinaryOp::Mul,
        SyntaxKind::SLASH => BinaryOp::Div,
        SyntaxKind::PERCENT => BinaryOp::Mod,
        SyntaxKind::STAR_STAR => BinaryOp::Pow,
        SyntaxKind::SHL => BinaryOp::Sl,
        SyntaxKind::SHR => BinaryOp::Sr,
        SyntaxKind::DOT => BinaryOp::Concat,
        SyntaxKind::LT => BinaryOp::IsSmaller,
        SyntaxKind::LT_EQ => BinaryOp::IsSmallerOrEqual,
        SyntaxKind::GT => BinaryOp::Greater,
        SyntaxKind::GT_EQ => BinaryOp::GreaterOrEqual,
        SyntaxKind::EQ_EQ => BinaryOp::IsEqual,
        SyntaxKind::BANG_EQ | SyntaxKind::LT_GT => BinaryOp::IsNotEqual,
        SyntaxKind::EQ_EQ_EQ => BinaryOp::IsIdentical,
        SyntaxKind::BANG_EQ_EQ => BinaryOp::IsNotIdentical,
        SyntaxKind::SPACESHIP => BinaryOp::Spaceship,
        SyntaxKind::PIPE => BinaryOp::BwOr,
        SyntaxKind::CARET => BinaryOp::BwXor,
        SyntaxKind::AMP => BinaryOp::BwAnd,
        SyntaxKind::XOR_KW => BinaryOp::BoolXor,
        _ => return None,
    })
}

/// The operator of a compound assignment, `+=` and its kin.
fn compound_op(kind: SyntaxKind) -> Option<BinaryOp> {
    Some(match kind {
        SyntaxKind::PLUS_EQ => BinaryOp::Add,
        SyntaxKind::MINUS_EQ => BinaryOp::Sub,
        SyntaxKind::STAR_EQ => BinaryOp::Mul,
        SyntaxKind::SLASH_EQ => BinaryOp::Div,
        SyntaxKind::PERCENT_EQ => BinaryOp::Mod,
        SyntaxKind::STAR_STAR_EQ => BinaryOp::Pow,
        SyntaxKind::SHL_EQ => BinaryOp::Sl,
        SyntaxKind::SHR_EQ => BinaryOp::Sr,
        SyntaxKind::DOT_EQ => BinaryOp::Concat,
        SyntaxKind::AMP_EQ => BinaryOp::BwAnd,
        SyntaxKind::PIPE_EQ => BinaryOp::BwOr,
        SyntaxKind::CARET_EQ => BinaryOp::BwXor,
        _ => return None,
    })
}

/// `ZEND_FREE_VOID_CAST` (`Z/Zend/zend_compile.h:1106`): the
/// `extended_value` of the `FREE` a `(void)` cast leaves.
const FREE_VOID_CAST: u32 = 1 << 2;

/// Which constants php's compiler substitutes where they are named,
/// `CG(compiler_options)`'s bits over `zend_try_ct_eval_const`.
///
/// A parameter's default keeps the constant it names, and every other
/// constant expression is the persistent constant's value, an
/// attribute's argument among them:
///
/// ```php
/// function f($a = -PHP_INT_MAX, $b = [PHP_INT_SIZE], $c = PHP_INT_SIZE) {}
/// foreach ((new ReflectionFunction('f'))->getParameters() as $p) echo $p, "\n";
/// #[Attribute] class At { function __construct(public $v) {} }
/// #[At([PHP_INT_SIZE, PHP_EOL])] class B {}
/// echo (new ReflectionClass('B'))->getAttributes()[0];
/// ```
/// ```expect
/// Parameter #0 [ <optional> $a = -PHP_INT_MAX ]
/// Parameter #1 [ <optional> $b = [PHP_INT_SIZE] ]
/// Parameter #2 [ <optional> $c = PHP_INT_SIZE ]
/// Attribute [ At ] {
///   - Arguments [1] {
///     Argument #0 [ [8, '\n'] ]
///   }
/// }
/// ```
#[derive(Clone, Copy, PartialEq, Eq)]
enum Substitution {
    /// A persistent constant is its value where it is named, and so is
    /// one the request defined where the compile is handed the request's
    /// ([`Options::request_constants`]), as php compiles every constant
    /// expression but a parameter's default.
    Persistent,
    /// `ZEND_COMPILE_NO_PERSISTENT_CONSTANT_SUBSTITUTION` beside
    /// `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION`, which `zend_compile_params`
    /// sets while it compiles a parameter's default, so the default keeps
    /// the constant it names, whichever table holds it.
    NoPersistent,
}

/// `can_ct_eval_const`'s and `zend_try_ct_eval_class_const`'s test of
/// the value itself: a type below `IS_ARRAY`, a scalar or a string, or
/// an array `array_is_const` takes; an object, a resource and a constant
/// expression not yet evaluated are not substituted.
fn substitutable(value: Zval) -> bool {
    match value.kind() {
        ZvalType::Null
        | ZvalType::False
        | ZvalType::True
        | ZvalType::Long
        | ZvalType::Double
        | ZvalType::String => true,
        ZvalType::Array => array_is_const(value),
        _ => false,
    }
}

/// `array_is_const` (`Z/Zend/zend_compile.c:1644`): at most 50
/// elements, each a scalar. php's walk descends into the array itself
/// where an element is an array, not into that element, until its 50
/// checks are spent, so an array holding an array is never taken; the
/// walk is copied as it stands.
fn array_is_const(array: Zval) -> bool {
    fn walk(array: Zval, max_checks: &mut usize) -> bool {
        let Some(elements) = array.as_arr() else {
            return false;
        };
        if elements.len() > *max_checks {
            return false;
        }
        *max_checks -= elements.len();
        for (_, element) in &elements {
            if element.as_arr().is_some() {
                if !walk(array, max_checks) {
                    return false;
                }
            } else if element.kind() as u8 >= ZvalType::Array as u8 {
                return false;
            }
        }
        true
    }
    walk(array, &mut 50)
}

/// One element of an array whose key and value folded, before its key
/// is written ([`Emit::keyed_entries`]).
enum FoldedElement {
    Pair(Option<Zval>, Zval),
    Unpack(Zval),
}

/// `nNextFreeElement` over the entries a constant array folded so far:
/// one past the largest integer key, 0 before any, and none where the
/// next index is taken past `ZEND_LONG_MAX`, which leaves the array to
/// the request as php's `zend_hash_next_index_insert` failing does.
fn next_index(entries: &[(Key, Zval)], next_free: i64) -> Option<Key> {
    let index = if next_free == i64::MIN { 0 } else { next_free };
    if index == i64::MAX && entries.iter().any(|(k, _)| *k == Key::Long(index)) {
        return None;
    }
    Some(Key::Long(index))
}

/// One entry of a constant array written as `zend_hash_update` writes
/// it: a key already there keeps its place and takes the value, and an
/// integer key moves `nNextFreeElement` past it.
fn write_entry(entries: &mut Vec<(Key, Zval)>, next_free: &mut i64, key: Key, value: Zval) {
    if let Key::Long(n) = key
        && n >= *next_free
    {
        *next_free = n.saturating_add(1);
    }
    match entries.iter_mut().find(|(k, _)| *k == key) {
        Some(held) => held.1 = value,
        None => entries.push((key, value)),
    }
}

/// `zend_zval_value_name` over a folded constant that is no array.
fn constant_value_name(value: &Zval) -> &'static str {
    match value.kind() {
        ZvalType::Null => "null",
        ZvalType::False => "false",
        ZvalType::True => "true",
        ZvalType::Long => "int",
        ZvalType::Double => "float",
        ZvalType::String => "string",
        _ => "array",
    }
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

impl<'a> Emit<'a> {
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
        self.functions.find(lowered).map(|(at, _)| at)
    }

    /// `zend_try_ct_eval_const` past the three special names: the
    /// constant `EG(zend_constants)` holds under the resolved name, where
    /// `can_ct_eval_const` (`Z/Zend/zend_compile.c:1650`) lets it be its
    /// value where it is written ([`Self::ct_found`]), copied as php's
    /// `ZVAL_COPY_OR_DUP` copies it: a persistent constant's value
    /// written out ([`Found::Declared`]), and a constant the request
    /// defined one more owner of its own value ([`Self::request_constant`]).
    /// Nothing where the name is left to the `FETCH_CONSTANT` the program
    /// serves.
    fn ct_constant(&mut self, name: &[u8]) -> Result<Option<Operated>, CompileError> {
        Ok(match self.ct_found(name) {
            Some(Found::Declared(value)) => Some(Operated::Kept(match value {
                Scalar::Str(bytes) => self.interned_string(bytes)?,
                Scalar::Bool(b) => Zval::bool(b),
                Scalar::Long(n) => Zval::long(n),
                Scalar::Double(d) => Zval::double(d),
                Scalar::Null => Zval::null(),
            })),
            Some(Found::Defined(value)) => self.request_constant(value),
            None => None,
        })
    }

    /// The constant `zend_try_ct_eval_const` would copy for `name`, found
    /// and judged by `can_ct_eval_const` but not copied, no owner taken
    /// and no string made. A parameter's default substitutes none, since
    /// `zend_compile_params` sets both
    /// `ZEND_COMPILE_NO_PERSISTENT_CONSTANT_SUBSTITUTION` and
    /// `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION` around it
    /// ([`Substitution::NoPersistent`]). A constant's name is
    /// case-sensitive, and a leading `\` names the same global constant
    /// as none. The engine's persistent constants are looked for first
    /// ([`Options::persistent`]), `PHP_SAPI` the host's name in every
    /// compile, a file the cache keeps among them, as php registers it
    /// `CONST_PERSISTENT`: one `CONST_DEPRECATED` marks is refused
    /// before anything else is read, so the `FETCH_CONSTANT` stands and
    /// the deprecation is raised where the constant is read, the bit
    /// being the `#[\Deprecated]` attribute on the declaration; and a
    /// declared value that is no scalar is not folded, as php folds none
    /// whose value is not yet evaluated. Then the constants the request
    /// defined, which a compile php runs without
    /// `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION` is handed
    /// ([`Options::request_constants`]): one `CONST_DEPRECATED` marks is
    /// refused first, as a persistent one is; then a scalar or a string is
    /// its value, and an array where `array_is_const` takes it, and an
    /// object, a resource or a larger array is left to the fetch.
    ///
    /// `ext/standard`'s `INF` and `NAN` are two persistent ones, folded
    /// where they are written:
    ///
    /// ```php
    /// var_dump(INF, -NAN, \INF, -INF);
    /// ```
    /// ```expect
    /// float(INF)
    /// float(NAN)
    /// float(INF)
    /// float(-INF)
    /// ```
    ///
    /// A file the cache keeps substitutes `PHP_SAPI` too, where the
    /// request's own constants are not handed its compile: `RETURN
    /// string("cli")`, with no `FETCH_CONSTANT` and no cache slot, as
    /// opcache's dump of the file reads in php:
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3272-sapi.inc';
    /// file_put_contents("$f.tmp", '<?php function f1() { return PHP_SAPI; }');
    /// touch("$f.tmp", time() - 10);
    /// rename("$f.tmp", $f);
    /// $dump = shell_exec(escapeshellarg(PHP_BINARY) . ' -n -d opcache.enable_cli=1 -d opcache.jit=off -d opcache.opt_debug_level=0x10000 ' . escapeshellarg($f) . ' 2>&1');
    /// preg_match_all('/^\d{4} .*$/m', $dump, $ops);
    /// echo implode("\n", $ops[0]), "\n";
    /// unlink($f);
    /// ```
    /// ```expect process regime=jit-off
    /// 0000 RETURN int(1)
    /// 0000 RETURN string("cli")
    /// 0001 RETURN null
    /// ```
    fn ct_found(&self, name: &[u8]) -> Option<Found<'a>> {
        if self.substitution == Substitution::NoPersistent {
            return None;
        }
        let mut at = 0;
        while name.get(at) == Some(&b'\\') {
            at += 1;
        }
        let name = name.get(at..).unwrap_or(&[]);
        if name == b"PHP_SAPI"
            && let Some(host) = self.persistent.host
        {
            return Some(Found::Defined(host));
        }
        if let Some((_, declared)) = self.persistent.declared.find(name) {
            let deprecated = (declared.attributes.iter())
                .any(|one| one.name.0.eq_ignore_ascii_case(b"Deprecated"));
            return declared_scalar(&declared.value)
                .filter(|_| !deprecated)
                .map(Found::Declared);
        }
        let defined = self.request_constants.as_ref()?.find(name)?;
        (!defined.deprecated && substitutable(defined.value))
            .then_some(Found::Defined(defined.value))
    }

    /// `zend_try_ct_eval_class_const`'s arm for a class named as written,
    /// in a compile php runs without `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION`
    /// ([`Options::request_constants`]): the constant of the class
    /// `EG(class_table)` holds under the lowered name, a native's or one
    /// the request declared, where `zend_verify_ct_const_access` lets it
    /// through and its value is one `can_ct_eval_const` would take, copied
    /// as a request's constant is ([`Self::request_constant`]). The access
    /// refuses a `#[\Deprecated]` constant and one read directly off a
    /// trait, and lets a public one through; a private one only where the
    /// scope is the class that declared it and a protected one only where
    /// the scope is that class or one it descends from, and the scope is
    /// the class being compiled, which no class the table held before it
    /// is or descends from, so neither is ever substituted here. A
    /// constant whose value is still a constant expression is left to the
    /// fetch, as php reads `IS_CONSTANT_AST` past `IS_ARRAY`, and so is
    /// one a cached file's class declared as one even once evaluated,
    /// where an `eval`'d class's evaluated constant is its value. A class
    /// the same compile bound early is in the table from its declaration
    /// on ([`Self::bound_class_constant`]).
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3266-class-constants.inc';
    /// file_put_contents($f, <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// m('class B4 { const K = 1; const S = "abcdefghij"; private const P = 2; protected const Q = 3; #[\Deprecated] const D = 4; const L = UNDEFINED_YET; }');
    /// m('trait T4 { const K = 5; } enum E4 { case A; } class C4 extends B4 {}');
    /// class B7 { const L = LATE; }
    /// m('class B6 { const L = LATE; }');
    /// define('LATE', 7);
    /// $read = B6::L + B7::L;
    /// foreach ([
    ///     'function fu1() { return 1; }',
    ///     'function fu7() { return B4::K; }',
    ///     'function fu8() { return b4::S . "x"; }',
    ///     'function fu9() { return ArrayObject::ARRAY_AS_PROPS; }',
    ///     'function fu10() { return DateTimeInterface::ATOM; }',
    ///     'function fu20() { return PDO::PARAM_INT; }',
    ///     'function fu11() { return C4::K; }',
    ///     'function fu12() { return B4::P; }',
    ///     'function fu13() { return B4::Q; }',
    ///     'function fu14() { return B4::D; }',
    ///     'function fu15() { return T4::K; }',
    ///     'function fu16() { return E4::A; }',
    ///     'function fu17() { return B4::L; }',
    ///     'function fu18() { return B6::L; }',
    ///     'function fu19() { return B7::L; }',
    ///     'class X9 { const K = 1; } function f9() { return X9::K; }',
    ///     'class X7 { const S = "zyxwvutsrq"; } class X6 extends X7 {} function f7() { return X6::S; }',
    ///     'class B5 { const K = B4::K + 1; }',
    ///     '$GLOBALS["a"] = B5::K;',
    /// ] as $c) echo m($c), "  ", $c, "\n";
    /// echo fu7(), fu8(), fu9(), fu10(), fu11(), "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 256  function fu1() { return 1; }
    /// 256  function fu7() { return B4::K; }
    /// 296  function fu8() { return b4::S . "x"; }
    /// 256  function fu9() { return ArrayObject::ARRAY_AS_PROPS; }
    /// 256  function fu10() { return DateTimeInterface::ATOM; }
    /// 256  function fu20() { return PDO::PARAM_INT; }
    /// 256  function fu11() { return C4::K; }
    /// 384  function fu12() { return B4::P; }
    /// 384  function fu13() { return B4::Q; }
    /// 384  function fu14() { return B4::D; }
    /// 384  function fu15() { return T4::K; }
    /// 384  function fu16() { return E4::A; }
    /// 384  function fu17() { return B4::L; }
    /// 256  function fu18() { return B6::L; }
    /// 320  function fu19() { return B7::L; }
    /// 640  class X9 { const K = 1; } function f9() { return X9::K; }
    /// 1064  class X7 { const S = "zyxwvutsrq"; } class X6 extends X7 {} function f7() { return X6::S; }
    /// 496  class B5 { const K = B4::K + 1; }
    /// 512  $GLOBALS["a"] = B5::K;
    /// 1abcdefghijx2Y-m-d\TH:i:sP1
    /// ```
    fn request_class_constant(&mut self, class: &[u8], name: &[u8]) -> Option<Operated> {
        self.request_constants.as_ref()?;
        if let Some(at) = (self.bound.iter().copied()).find(|at| {
            (self.classes.get(*at)).is_some_and(|decl| decl.name.0.eq_ignore_ascii_case(class))
        }) {
            return self.bound_class_constant(at, name, 0);
        }
        let found =
            (self.request_constants.as_ref()?).class_constant(&class.to_ascii_lowercase(), name)?;
        let accessible =
            !found.deprecated && !found.in_trait && found.visibility == Visibility::Public;
        if !accessible || !substitutable(found.value) {
            return None;
        }
        self.request_constant(found.value)
    }

    /// The copy of a constant the request defined that `can_ct_eval_const`
    /// lets through ([`Self::ct_found`]): the value is the constant's own
    /// with one more owner,
    /// `ZVAL_COPY_OR_DUP`, not a copy of its bytes ([`Intern::hold`]): the
    /// unit keeps it where it lands, and a fold over a string gives the
    /// owner back once it stands, as `zend_eval_const_expr` destroys its
    /// operand ([`Intern::hold_made`]). A unit a cache keeps is compiled
    /// with no request constants, as opcache compiles a file under
    /// `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION`.
    ///
    /// So an `eval` naming a constant defined before it compiles no
    /// `FETCH_CONSTANT` and no cache slot for it, a class constant written
    /// as one is a value written out with no constant expression to copy,
    /// a fold over one folds, and the op array's copy of a string is one more owner of
    /// the constant's string, each in its own `eval`. Each `eval`'s name
    /// is a string of the request, so the file is written under the
    /// recipe's `TMPDIR`, a directory of one length on every machine
    /// ([`crate::Options::request_constants`]):
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3263-substituted.inc';
    /// file_put_contents($f, <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// const UC = 5;
    /// m('const EVALNAMED = 5;');
    /// define('DS', str_repeat('q', count($GLOBALS['argv']) + 19));
    /// const ARR = [1, 2];
    /// foreach ([
    ///     'function fu2() { return UC; }',
    ///     'class B2 { const K = EVALNAMED; }',
    ///     '$GLOBALS["a"] = B2::K;',
    ///     'class B3 { const K = "abcdefghij" . EVALNAMED; }',
    ///     '$GLOBALS["a"] = B3::K;',
    ///     'enum E1: int { case A = 1; const K = EVALNAMED; }',
    ///     '$GLOBALS["a"] = E1::from(1);',
    ///     'function fu3() { return DS; }',
    ///     'function fu6() { return ARR; }',
    ///     'function fu7() { return "x" . DS; }',
    ///     '$GLOBALS["d"] = defined("UC");',
    /// ] as $c) echo m($c), "  ", $c, "\n";
    /// debug_zval_dump(DS, fu3(), fu6(), fu7());
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 256  function fu2() { return UC; }
    /// 496  class B2 { const K = EVALNAMED; }
    /// 0  $GLOBALS["a"] = B2::K;
    /// 536  class B3 { const K = "abcdefghij" . EVALNAMED; }
    /// 0  $GLOBALS["a"] = B3::K;
    /// 1304  enum E1: int { case A = 1; const K = EVALNAMED; }
    /// 184  $GLOBALS["a"] = E1::from(1);
    /// 256  function fu3() { return DS; }
    /// 256  function fu6() { return ARR; }
    /// 304  function fu7() { return "x" . DS; }
    /// 0  $GLOBALS["d"] = defined("UC");
    /// string(20) "qqqqqqqqqqqqqqqqqqqq" refcount(4)
    /// string(20) "qqqqqqqqqqqqqqqqqqqq" refcount(4)
    /// array(2) packed interned {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    /// string(21) "xqqqqqqqqqqqqqqqqqqqq" refcount(2)
    /// ```
    ///
    /// A constant `#[\Deprecated]` marks and one holding an object are
    /// still fetched where they are read, the `FETCH_CONSTANT` and its
    /// cache slot standing where a substituted one has none, so the
    /// deprecation is raised when the function runs and not as the
    /// `eval` compiles:
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3263-still-fetched.inc';
    /// file_put_contents($f, <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// const UC = 5;
    /// #[\Deprecated] const DEP = 7;
    /// const OBJ = new stdClass;
    /// echo m('function fu2() { return UC; }'), "\n";
    /// echo m('function fu4() { return DEP; }'), "\n";
    /// echo m('function fu5() { return OBJ; }'), "\n";
    /// var_dump(fu4(), fu5() === OBJ);
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 256
    /// 320
    /// 320
    ///
    /// Deprecated: Constant DEP is deprecated in %s/w3263-still-fetched.inc(2) : eval()'d code on line 1
    /// int(7)
    /// bool(true)
    /// ```
    fn request_constant(&mut self, value: Zval) -> Option<Operated> {
        match value.as_str() {
            Some(_) => self.strings.hold_made(value).map(Operated::Made),
            None => self.strings.hold(value).then_some(Operated::Kept(value)),
        }
    }

    /// [`Self::request_class_constant`] over a class of the unit that
    /// was bound early, which `CG(class_table)` holds from its
    /// declaration on, so a name after it in the same compile substitutes
    /// its constant: its own, or what it inherits from its parent, looked
    /// for as any class's is. The constant is held to the access and the
    /// value test as any other, its string the one its declaration holds
    /// ([`UnitValue`]) or, interned, the one of its bytes. The walk up its
    /// parents stops at a depth of 64, as [`Self::linked_method`]'s does.
    ///
    /// `self::class` is the class's own name string
    /// (`zend_try_compile_const_expr_resolve_class_name`'s
    /// `ZVAL_STR_COPY`), which a name after the class substitutes with one
    /// more owner, no fetch and no cache slot (the file under the
    /// recipe's `TMPDIR`):
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3281-self-class.inc';
    /// file_put_contents($f, <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W {}'); m('$q0 = 1;');
    /// foreach ([
    ///     'class Q5y { const K = self::class; } function f5() { return Q5y::K; }',
    ///     'class Q6y { const K = "abcdefghijk"; } function f6() { return Q6y::K; }',
    ///     'class Q7y { const K = self::class; const L = self::K; }',
    ///     'class Q8y { const K = self::class; } $GLOBALS["q8"] = Q8y::K;',
    /// ] as $c) echo m($c), "\n";
    /// $junk = array_fill(0, 1000, str_repeat("x", 100));
    /// echo f5(), " ", f6(), " ", Q7y::L, " ", $q8, "\n";
    /// eval('debug_zval_dump(Q5y::K);');
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 640
    /// 680
    /// 432
    /// 1008
    /// Q5y abcdefghijk Q7y Q8y
    /// string(3) "Q5y" refcount(5)
    /// ```
    fn bound_class_constant(&mut self, at: usize, name: &[u8], depth: u32) -> Option<Operated> {
        let decl = self.classes.get(at)?;
        let Some(constant) = decl.constants.iter().find(|c| *c.name.0 == *name) else {
            let parent = decl.parent.as_ref()?.0.to_vec();
            if depth >= 64 {
                return None;
            }
            if let Some(parent_at) = (self.bound.iter().copied()).find(|at| {
                (self.classes.get(*at)).is_some_and(|d| d.name.0.eq_ignore_ascii_case(&parent))
            }) {
                return self.bound_class_constant(parent_at, name, depth + 1);
            }
            return self.request_class_constant(&parent, name);
        };
        let accessible = !constant.is_case
            && constant.visibility == Visibility::Public
            && !attributes::is_deprecated(&constant.attributes);
        if !accessible {
            return None;
        }
        let value = match &constant.value {
            ConstExpr::Null => Zval::null(),
            ConstExpr::Bool(b) => Zval::bool(*b),
            ConstExpr::Long { value: n, .. } => Zval::long(*n),
            ConstExpr::Double(d) => Zval::double(*d),
            ConstExpr::Str(bytes) => {
                let bytes = bytes.clone();
                self.string(&bytes).ok()?
            }
            ConstExpr::UnitValue(held) => held.value(),
            _ => return None,
        };
        if !substitutable(value) {
            return None;
        }
        self.request_constant(value)
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

/// `zval_is_true` over a folded constant, as [`constant_truth`] reads
/// one. No object, resource or reference is a constant a compiler
/// folds, so the truth it has none of is never asked.
pub(crate) fn truth(value: Zval) -> bool {
    constant_truth(value).unwrap_or(false)
}

/// `zval_get_long_func` over a folded constant: a null and a false are
/// zero and a true is one, a long is itself, a double is truncated,
/// which is all `zend_try_ct_eval_cast` lets through, a string is the
/// number `is_numeric_string` reads off it with `allow_errors` on, a
/// double among them capped as `zend_dval_to_lval_cap` caps it, and an
/// array is whether it holds anything.
#[expect(
    clippy::cast_possible_truncation,
    reason = "php's `(zend_long) d` under `ZEND_DOUBLE_FITS_LONG`, which the caller held"
)]
fn long_of(value: Zval) -> i64 {
    if let Some(n) = value.as_long() {
        return n;
    }
    if let Some(d) = value.as_double() {
        return d as i64;
    }
    if let Some(s) = value.as_str() {
        return match numeric_prefix(s.bytes()) {
            Some((Number::Long(n), _)) => n,
            Some((Number::Double(d), _)) => dval_to_lval_cap(d),
            None => 0,
        };
    }
    if let Some(array) = value.as_arr() {
        return i64::from(!array.is_empty());
    }
    i64::from(value.kind() == ZvalType::True)
}

/// `zval_get_double_func` over a folded constant: a null and a false are
/// zero and a true is one, a long widens, a double is itself, a string
/// is what `zend_strtod` reads off it and zero where it opens with no
/// number, and an array is whether it holds anything.
#[expect(
    clippy::cast_precision_loss,
    reason = "php's `(double) lval`, whose rounding is the behaviour being copied"
)]
fn double_of(value: Zval) -> f64 {
    if let Some(n) = value.as_long() {
        return n as f64;
    }
    if let Some(d) = value.as_double() {
        return d;
    }
    if let Some(s) = value.as_str() {
        return match numeric_prefix(s.bytes()) {
            Some((number, _)) => number.double(),
            None => 0.0,
        };
    }
    if let Some(array) = value.as_arr() {
        return f64::from(u8::from(!array.is_empty()));
    }
    f64::from(u8::from(value.kind() == ZvalType::True))
}

/// `ZEND_DOUBLE_FITS_LONG` on a 64-bit long: `(double) ZEND_LONG_MAX`
/// rounds up out of the range, so the upper edge is exclusive. Written
/// as php writes it, two comparisons a NAN fails neither of.
#[expect(
    clippy::cast_precision_loss,
    reason = "php's own `(double) ZEND_LONG_MIN` and `(double) ZEND_LONG_MAX`"
)]
fn fits_long(d: f64) -> bool {
    d >= i64::MIN as f64 && d < i64::MAX as f64
}

/// `zend_dval_to_lval_cap`: zero for what is not finite, and the long's
/// own edge past its range, which is `strtol`'s answer kept for a
/// float-string.
#[expect(
    clippy::cast_possible_truncation,
    reason = "php's `(zend_long) d` inside the range this guards"
)]
fn dval_to_lval_cap(d: f64) -> i64 {
    if !d.is_finite() {
        0
    } else if !fits_long(d) {
        if d > 0.0 { i64::MAX } else { i64::MIN }
    } else {
        d as i64
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
        // A `.` is [`Emit::concat_fold`]'s, an `xor`
        // [`Emit::bool_xor`]'s and a comparison [`Emit::compare_fold`]'s;
        // none reaches here.
        BinaryOp::Concat | BinaryOp::BoolXor => true,
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

/// Whether the operator is one of the three bitwise levels, which
/// `zend_compile_binary_op` folds through `bitwise_or_function` and its
/// kin and `zend_binary_op_produces_error` has its own arms for.
fn bitwise(op: BinaryOp) -> bool {
    matches!(op, BinaryOp::BwOr | BinaryOp::BwXor | BinaryOp::BwAnd)
}

/// `bitwise_or_function`, `bitwise_xor_function` and
/// `bitwise_and_function` over two strings, the one operand pair the
/// three read byte by byte: the shorter string's length of bytes joined
/// by the operator, and for an `|` the longer string's tail after them,
/// which is why an `|` is as long as the longer side and an `^` and an
/// `&` as long as the shorter.
fn bitwise_bytes(op: BinaryOp, left: &[u8], right: &[u8]) -> Vec<u8> {
    let (longer, shorter) = if left.len() >= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    let mut out: Vec<u8> = shorter
        .iter()
        .zip(longer)
        .map(|(a, b)| match op {
            BinaryOp::BwOr => a | b,
            BinaryOp::BwXor => a ^ b,
            _ => a & b,
        })
        .collect();
    if op == BinaryOp::BwOr {
        out.extend_from_slice(&longer[shorter.len()..]);
    }
    out
}

/// The same three over two longs.
fn bitwise_longs(op: BinaryOp, left: i64, right: i64) -> i64 {
    match op {
        BinaryOp::BwOr => left | right,
        BinaryOp::BwXor => left ^ right,
        _ => left & right,
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
/// The key a case or an arm condition adds to a jump table: its
/// integer, or its string as interned; nothing for a constant of
/// another type, which the type walk ruled out already.
fn jump_key(value: Zval) -> Option<Key> {
    match value.kind() {
        ZvalType::Long => value.as_long().map(Key::Long),
        ZvalType::String => value.as_str().map(Key::Str),
        _ => None,
    }
}

fn should_use_jumptable(cases: usize, kind: ZvalType) -> bool {
    if kind == ZvalType::Long {
        cases >= 5
    } else {
        cases >= 2
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

/// The scalar a persistent constant's declared value is, which is the
/// value `can_ct_eval_const` lets be written where its name is: a null,
/// a bool, a long, a double or a string. Nothing for a value not yet
/// evaluated, which php folds none of.
fn declared_scalar(value: &ConstExpr) -> Option<Scalar<'_>> {
    Some(match value {
        ConstExpr::Null => Scalar::Null,
        ConstExpr::Bool(b) => Scalar::Bool(*b),
        ConstExpr::Long { value, .. } => Scalar::Long(*value),
        ConstExpr::Double(d) => Scalar::Double(*d),
        ConstExpr::Str(bytes) => Scalar::Str(bytes),
        _ => return None,
    })
}

/// `FC(declarables)`' half this compiler reads, which is php's
/// `ZEND_ACC_STRICT_TYPES` by another name: which of the two typing
/// rules the file the emitter is compiling was declared under. php's
/// default, where the file declares nothing, is the weak one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Typing {
    /// `zend_verify_weak_scalar_type_hint` decides an argument, a
    /// return and a write to a typed property.
    Weak,
    /// `declare(strict_types=1)`: nothing converts but an int where a
    /// float is asked for.
    Strict,
}

impl Typing {
    /// The rule a caller's `ZEND_ACC_STRICT_TYPES` names, which is what
    /// `eval` and an `include` inherit ([`Options::strict_types`]).
    fn of(strict: bool) -> Typing {
        if strict { Typing::Strict } else { Typing::Weak }
    }
}

/// An expression under any parentheses around it, the node php's
/// parser makes of it (`(expr)` is `expr`).
pub(crate) fn peel(parsed: &Parsed, mut expr: tree::Expr) -> tree::Expr {
    while let tree::Expr::ParenExpr(id) = expr {
        match parsed.tree[id].inner {
            Some(inner) => expr = inner,
            None => return expr,
        }
    }
    expr
}

/// The `ZEND_AST_ZVAL` an expression is, or nothing where php's node
/// is another kind: a number or a `'…'` string is its token's value,
/// [`TokenValue::of`]; a `"…"`, a heredoc or a nowdoc with nothing
/// interpolated is one string as php's parser folds it. What the parser's
/// `declare` action holds an encoding to, and the compiler every other
/// directive.
pub(crate) fn written(parsed: &Parsed, expr: tree::Expr) -> Option<Literal> {
    let tree = &parsed.tree;
    match peel(parsed, expr) {
        tree::Expr::LiteralExpr(id) => {
            let node = &tree[id];
            match TokenValue::of(parsed, Holder::Scalar(node), node.value?)? {
                TokenValue::Int(n) => Some(Literal::Int(n)),
                TokenValue::Float(_) | TokenValue::Bytes(_) => Some(Literal::Other),
            }
        }
        tree::Expr::InterpolatedStringExpr(id) => tree
            .list(tree[id].parts)
            .iter()
            .all(|part| matches!(part, tree::StringPart::LiteralStringPart(_)))
            .then_some(Literal::Other),
        tree::Expr::HeredocExpr(id) => tree
            .list(tree[id].parts)
            .iter()
            .all(|part| matches!(part, tree::StringPart::LiteralStringPart(_)))
            .then_some(Literal::Other),
        tree::Expr::NowdocExpr(_) => Some(Literal::Other),
        _ => None,
    }
}

/// What the file's own statement list has held so far, as
/// `zend_is_first_statement` reads it for an `encoding` directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TopList {
    /// Nothing but `declare` statements.
    Declares,
    /// Some other statement, an empty one among them.
    Other,
}

/// What php's `ZEND_AST_ZVAL` is among the tree's expressions: one
/// token the scanner already made a zval of, or a string in any
/// spelling with nothing interpolated, which php's parser hands the
/// compiler as one value. `true`, `null` and `-1` are each a node of
/// their own, which is why `zend_compile_declare` refuses them before
/// it folds anything.
pub(crate) enum Literal {
    /// `T_LNUMBER`, the one a directive here reads the value of.
    Int(i64),
    /// `T_DNUMBER` and `T_CONSTANT_ENCAPSED_STRING`: one php takes,
    /// whose value no directive this compiler serves reads.
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_unit::decl::MethodDecl;

    /// An emitter as [`unit`] builds one, in namespace `A` and at the
    /// file's own statement list, which is where a class binds at
    /// compile time.
    fn in_namespace<'a>(
        arena: &'a Arena,
        parsed: &'a Parsed<'a>,
        strings: &'a mut php_heap::Strings,
        options: &'a Options<'a>,
    ) -> Emit<'a> {
        Emit {
            parsed,
            arena,
            strings,
            precision: options.precision,
            assertions: assert::Assertions::of(options.assertions),
            persistent: options.persistent,
            functions: options.functions,
            known_classes: options.classes,
            hooked: options.hooked,
            extended_info: options.extended_info,
            request_classes: options.request_classes,
            request_constants: options.request_constants,
            frameless: options.frameless,
            namespace: Some(b"A".to_vec()),
            namespacing: Namespacing::Unbracketed,
            imports: Imports::default(),
            active_class: None,
            folded_string: None,
            parser_dots: std::collections::HashSet::new(),
            active_parent: None,
            active_function: None,
            own_names: Vec::new(),
            file_own: None,
            this_in_scope: false,
            return_type: None,
            returns_by_ref: false,
            // `zend_compile_top_stmt`'s `toplevel`, which is where a
            // class binds at compile time.
            depth: 1,
            active_property: None,
            active_hook: None,
            in_jmp_frameless_branch: false,
            statements_seen: 0,
            top_list: TopList::Declares,
            typing: Typing::of(options.strict_types),
            ticks: None,
            stack: options.stack,
            oplines: Vec::new(),
            short_circuits: Vec::new(),
            chain_inner: None,
            memo: memo::Memo::default(),
            const_evaluation: Evaluation::Once,
            substitution: Substitution::Persistent,
            constants: Vec::new(),
            const_exprs: Vec::new(),
            vars: Vec::new(),
            http_response_header: ResponseHeader::Predefined,
            safepoints: Safepoints::new(),
            temporaries: 0,
            cache_slots: 0,
            line: 1,
            diagnostics: Vec::new(),
            classes: Vec::new(),
            bound: Vec::new(),
            top_level: Vec::new(),
            completed: Vec::new(),
            runtime_keys: Vec::new(),
            attributed_constants: Vec::new(),
            keys_before: 0,
            first_slot: 0,
            functions_declared: Vec::new(),
            landed: Vec::new(),
            dynamic: Vec::new(),
            cell: php_unit::UnitCell::default(),
            function_callables: 0,
            dynamic_func_defs: Vec::new(),
            nested: Vec::new(),
            static_vars: Vec::new(),
            fold: Fold::Compile,
            rewrites: eval::Rewrites::default(),
            claims: eval::Claims::default(),
            fn_flags: FnFlags::default(),
            numeric_dims: Vec::new(),
            methods: Vec::new(),
            hooks: Vec::new(),
            filename: options.filename,
            room_filename: None,
            cwd: options.cwd,
            loops: Vec::new(),
            current_loop: None,
            jumps: Vec::new(),
            labels: Vec::new(),
            try_catch: Vec::new(),
            unwind: Vec::new(),
            loop_vars: 0,
            loop_var_peak: 0,
            delayed: Rc::default(),
            fast_call_var: None,
            try_catch_offset: None,
            generators: Vec::new(),
            arms: Vec::new(),
            halt: None,
            halted: None,
        }
    }

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
        let parsed = Parsed::default();
        let arena = Arena::default();
        let mut emit = in_namespace(&arena, &parsed, &mut strings, &options);
        let read = Class {
            name: b"M".to_vec(),
            kind: ClassKind::Class,
            modifiers: walk::ClassModifiers::default(),
            backing: None,
            parent: None,
            interfaces: Vec::new(),
            traits: Vec::new(),
            attributes: Vec::new(),
            doc_comment: None,
            body: Vec::new(),
            statement_lines: Vec::new(),
            line: 1,
            end_line: 1,
        };
        assert!(emit.class(&read).is_ok());
        assert_eq!(emit.classes[0].name.0.as_ref(), b"A\\M".as_slice());
        emit.classes[0].methods.push(MethodDecl {
            function: php_unit::decl::native_function(b"f"),
            visibility: Visibility::Public,
            is_instance: false,
            is_abstract: false,
            is_final: false,
            tentative_return: false,
        });
        let user = Some(Known::User { flagged: false });
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

    /// #3215: a node `zend_eval_const_expr` leaves unfolded because a
    /// child it waits on is one this compiler cannot read yet answers
    /// the child's [`eval::Unfolded::Unread`], so a compile that needs
    /// the value refuses in the innermost sentence, the one the phpt lane
    /// counts; a child that is no constant leaves the node's own sentence.
    /// The `()` is a tree the parser refuses, read here past its
    /// diagnostic: no program reaches these arms over an unread child
    /// and then a compile that reads the answer, so this test holds the
    /// state where no witness can.
    #[test]
    fn a_node_over_a_child_not_read_yet_answers_the_child_s_refusal() {
        let answers = |source: &'static [u8]| {
            let mut strings = php_heap::Strings::new();
            let options = Options::default();
            let arena = Arena::default();
            let parsed = php_syntax::parse(&arena, source, &php_syntax::Options::default());
            let mut emit = in_namespace(&arena, &parsed, &mut strings, &options);
            emit.fold = Fold::EvalConstExpr;
            let file = parsed
                .tree
                .root()
                .and_then(php_syntax::tree::Element::id::<tree::SourceFile>);
            let stmt =
                file.and_then(|file| parsed.tree.list(parsed.tree[file].stmts).first().copied());
            let Some(Stmt::ExprStmt(id)) = stmt else {
                return format!("no expression statement in {source:?}");
            };
            match emit.constant_expr(parsed.tree[id].expr) {
                Err(eval::Unfolded::Unread(refused)) => format!("unread: {}", refused.message),
                Err(eval::Unfolded::NoConstant(why)) => format!("no constant: {why}"),
                Err(eval::Unfolded::Refused(refused)) => format!("refused: {}", refused.message),
                Ok(_) => "folded".to_owned(),
            }
        };
        let unread = "unread: an expression inside parentheses this compiler cannot read yet";
        assert_eq!(answers(b"<?php () ?? 1;"), unread);
        assert_eq!(answers(b"<?php () ? 1 : 2;"), unread);
        assert_eq!(answers(b"<?php true ? () : 2;"), unread);
        assert_eq!(answers(b"<?php () && 1;"), unread);
        assert_eq!(answers(b"<?php true && ();"), unread);
        assert_eq!(
            answers(b"<?php $a ?? 1;"),
            "no constant: a `??` over a side that does not fold"
        );
        assert_eq!(
            answers(b"<?php $a ? 1 : 2;"),
            "no constant: a conditional over a condition that does not fold"
        );
        assert_eq!(
            answers(b"<?php true ? $a : 2;"),
            "no constant: a node replaced by a side that does not fold"
        );
        assert_eq!(
            answers(b"<?php $a && 1;"),
            "no constant: a short circuit over a side that does not fold"
        );
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
                .map(|refused| (refused.level, refused.message.to_string()))
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

// ----- the `attr` php's parser puts on a node -----
//
// The enums php keeps in `zend_ast.attr`, which the walk reads from the
// token that decided it: which file construct an `include` is, which
// magic constant a `__LINE__` is, which operator a `+` is, which type a
// cast is to, which of the four step nodes a `++` is, and how a
// variable piece of an interpolated string was written (design §4.9).
/// Which of the five constructs of `ZEND_AST_INCLUDE_OR_EVAL` a node
/// is: php's `ZEND_EVAL`, `ZEND_INCLUDE`, `ZEND_INCLUDE_ONCE`,
/// `ZEND_REQUIRE` and `ZEND_REQUIRE_ONCE`
/// (`Z/Zend/zend_compile.h:1075-1079`), one bit each, which the parser
/// puts in the node's `attr` and `zend_compile_include_or_eval` copies
/// into the opline's `extended_value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncludeKind {
    Eval,
    Include,
    IncludeOnce,
    Require,
    RequireOnce,
}

impl IncludeKind {
    /// The bit php names the construct by.
    fn attr(self) -> u32 {
        match self {
            IncludeKind::Eval => 1,
            IncludeKind::Include => 1 << 1,
            IncludeKind::IncludeOnce => 1 << 2,
            IncludeKind::Require => 1 << 3,
            IncludeKind::RequireOnce => 1 << 4,
        }
    }
}

/// The nine magic constants, php's `T_LINE` through `T_PROPERTY_C`: the
/// lexer's tokens, matched whatever their case, so `__file__` is
/// `__FILE__`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Magic {
    Line,
    File,
    Dir,
    Function,
    Class,
    Trait,
    Method,
    Namespace,
    Property,
}

impl Magic {
    /// The magic constant this spelling is, by the token name
    /// `php_syntax` gives it; none for any other name, `\__FILE__`
    /// included, which php reads as a constant of that name.
    fn of(name: &[u8]) -> Option<Magic> {
        Some(match magic_const_name(name) {
            "T_LINE" => Magic::Line,
            "T_FILE" => Magic::File,
            "T_DIR" => Magic::Dir,
            "T_FUNC_C" => Magic::Function,
            "T_CLASS_C" => Magic::Class,
            "T_TRAIT_C" => Magic::Trait,
            "T_METHOD_C" => Magic::Method,
            "T_NS_C" => Magic::Namespace,
            "T_PROPERTY_C" => Magic::Property,
            _ => return None,
        })
    }
}

/// How a variable piece of an interpolated string was written: `$a`,
/// `{$a}` and the plain bytes carry no `attr`; `${a}` and `${a[0]}` are
/// php's `ZEND_ENCAPS_VAR_DOLLAR_CURLY`, and `${expr}` its
/// `ZEND_ENCAPS_VAR_DOLLAR_CURLY_VAR_VAR`, each deprecated in its own
/// words.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Written {
    Plain,
    DollarCurly,
    DollarCurlyVarVar,
}

/// Which of php's four step nodes stands over a target: whether the
/// value the step leaves is the one before it (`POST`) or after it
/// (`PRE`), and whether it counts up or down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// `++$x`, php's `ZEND_AST_PRE_INC`
    PreInc,
    /// `--$x`, php's `ZEND_AST_PRE_DEC`
    PreDec,
    /// `$x++`, php's `ZEND_AST_POST_INC`
    PostInc,
    /// `$x--`, php's `ZEND_AST_POST_DEC`
    PostDec,
}

/// The binary operators this reads, each the opcode php's
/// `zend_compile_binary_op` gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinaryOp {
    /// `ZEND_ADD`
    Add,
    /// `ZEND_SUB`
    Sub,
    /// `ZEND_MUL`
    Mul,
    /// `ZEND_DIV`
    Div,
    /// `ZEND_MOD`
    Mod,
    /// `ZEND_POW`
    Pow,
    /// `ZEND_SL`
    Sl,
    /// `ZEND_SR`
    Sr,
    /// `ZEND_CONCAT`
    Concat,
    /// `ZEND_IS_SMALLER`
    IsSmaller,
    /// `ZEND_IS_SMALLER_OR_EQUAL`
    IsSmallerOrEqual,
    /// `>`, php's `ZEND_AST_GREATER`, which `zend_compile_greater` emits
    /// as `ZEND_IS_SMALLER` with the operands swapped.
    Greater,
    /// `>=`, php's `ZEND_AST_GREATER_EQUAL`, `ZEND_IS_SMALLER_OR_EQUAL`
    /// the same way.
    GreaterOrEqual,
    /// `ZEND_IS_EQUAL`
    IsEqual,
    /// `ZEND_IS_NOT_EQUAL`, which `<>` is as well.
    IsNotEqual,
    /// `ZEND_IS_IDENTICAL`
    IsIdentical,
    /// `ZEND_IS_NOT_IDENTICAL`
    IsNotIdentical,
    /// `ZEND_SPACESHIP`
    Spaceship,
    /// `ZEND_BW_OR`
    BwOr,
    /// `ZEND_BW_XOR`
    BwXor,
    /// `ZEND_BW_AND`
    BwAnd,
    /// `ZEND_BOOL_XOR`, which `xor` is: the one word spelling
    /// `zend_compile_binary_op` serves, with no jump in it, since both
    /// sides are read whatever either answers.
    BoolXor,
}

impl BinaryOp {
    /// The operator as php's grammar spells it, what a constant
    /// expression's `Binary` node names it by (design §4.7).
    fn spelling(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            BinaryOp::Pow => "**",
            BinaryOp::Sl => "<<",
            BinaryOp::Sr => ">>",
            BinaryOp::Concat => ".",
            BinaryOp::IsSmaller => "<",
            BinaryOp::IsSmallerOrEqual => "<=",
            BinaryOp::Greater => ">",
            BinaryOp::GreaterOrEqual => ">=",
            BinaryOp::IsEqual => "==",
            BinaryOp::IsNotEqual => "!=",
            BinaryOp::IsIdentical => "===",
            BinaryOp::IsNotIdentical => "!==",
            BinaryOp::Spaceship => "<=>",
            BinaryOp::BwOr => "|",
            BinaryOp::BwXor => "^",
            BinaryOp::BwAnd => "&",
            BinaryOp::BoolXor => "xor",
        }
    }
}

/// The type a cast is to, php's `attr` on `ZEND_AST_CAST`: the scanner's
/// `T_INT_CAST`, `T_DOUBLE_CAST`, `T_STRING_CAST`, `T_ARRAY_CAST`,
/// `T_OBJECT_CAST` and `T_BOOL_CAST`, each scalar one with the canonical
/// spelling and the one 8.5 deprecates (`zend_language_scanner.l` at the
/// pin). `(unset)` is refused as the compile error `zend_compile_cast`
/// raises for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CastTo {
    /// `(int)` and `(integer)`: `IS_LONG`.
    Long,
    /// `(float)` and `(double)`: `IS_DOUBLE`.
    Double,
    /// `(string)` and `(binary)`: `IS_STRING`.
    Str,
    /// `(array)`: `IS_ARRAY`.
    Array,
    /// `(object)`: `IS_OBJECT`.
    Object,
    /// `(bool)` and `(boolean)`: `_IS_BOOL`, which php compiles to
    /// `ZEND_BOOL` and never to `ZEND_CAST`.
    Bool,
}

impl CastTo {
    /// php's type constant, `ZEND_CAST`'s `extended_value`.
    fn type_code(self) -> u32 {
        match self {
            CastTo::Long => 4,
            CastTo::Double => 5,
            CastTo::Str => 6,
            CastTo::Array => 7,
            CastTo::Object => 8,
            // `_IS_BOOL` is 18 and reaches no opline: `ZEND_BOOL` carries
            // nothing in `extended_value`.
            CastTo::Bool => 0,
        }
    }

    /// php's type constant as a `ZEND_AST_CAST` carries it in `attr`,
    /// `_IS_BOOL` for a bool, the constant expression's `to`.
    fn ast_code(self) -> u32 {
        match self {
            CastTo::Bool => 18,
            other => other.type_code(),
        }
    }
}

/// php's `allow_dynamic` over a constant expression
/// (`zend_const_expr_to_zval`): whether it may differ each time it is
/// evaluated. A parameter's default, an attribute's argument and a
/// `const` statement's value are evaluated each time, and admit `new`
/// and `(object)`; a class constant, a property's default and an enum
/// case's value are evaluated once, and refuse both.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Evaluation {
    #[default]
    Once,
    EachTime,
}

/// `zend_is_auto_global`: the names `php_startup_auto_globals` and
/// `zend_startup` register, which no CV holds.
fn is_auto_global(name: &[u8]) -> bool {
    name == b"GLOBALS" || AutoGlobal::of(name).is_some()
}

/// `zend_enum_register_props` (`Z/Zend/zend_enum.c:475`): an enum's
/// `public readonly string $name`, and a backed one's
/// `public readonly int|string $value` of its backing type, declared
/// before anything its body declares; nothing for any other kind.
fn enum_properties(kind: ClassKind, backing: EnumBacking) -> Vec<PropertyDecl> {
    if kind != ClassKind::Enum {
        return Vec::new();
    }
    let property = |name: &str, ty: Type| PropertyDecl {
        name: Name::from(name),
        visibility: Visibility::Public,
        set_visibility: None,
        is_instance: true,
        is_readonly: true,
        is_final: false,
        is_promoted: false,
        is_virtual: false,
        ty: Some(ty),
        default: None,
        hooks: None,
        attributes: Vec::new(),
        doc_comment: None,
    };
    let mut out = vec![property("name", Type::String)];
    match backing {
        EnumBacking::Int => out.push(property("value", Type::Int)),
        EnumBacking::String => out.push(property("value", Type::String)),
        EnumBacking::Pure => {}
    }
    out
}

/// `zend_mark_function_as_generator`'s test of a declared return type
/// (`Z/Zend/zend_compile.c:1537-1551`): any object admits a
/// `Generator`, and so does a name `is_generator_compatible_class_type`
/// answers for, `Traversable`, `Iterator` or `Generator` whatever the
/// case, alone or in a union or an intersection. `iterable` is
/// `Traversable|array` once php has compiled it.
fn generator_compatible(ty: &Type) -> bool {
    match ty {
        Type::Mixed | Type::Object | Type::Iterable => true,
        Type::Named(name) => {
            let lowered = name.0.to_ascii_lowercase();
            matches!(
                lowered.as_slice(),
                b"traversable" | b"iterator" | b"generator"
            )
        }
        Type::Nullable(inner) => generator_compatible(inner),
        Type::Union(members) | Type::Intersection(members) => {
            members.iter().any(generator_compatible)
        }
        _ => false,
    }
}
