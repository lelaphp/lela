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
//! `PRE_INC CV0($i)`. What an opline leaves is written in its
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

use std::cmp::Ordering;

use php_heap::{
    Fatal, Intern, Key, Number, Scalar, Zval, ZvalType, double_to_string, numeric_prefix,
};
use php_unit::decl::{
    ClassConstDecl, ClassDecl, ClassFlags, ClassKind, ConstDecl, ConstExpr, EnumBacking,
    FunctionDecl, Name, PropertyDecl, Visibility,
};
use php_unit::{
    Diagnostic, FramelessEntry, Level, OpArray, Opcode, OperandKind, Opline, OplineIdx, Unit,
    frameless_table,
};

use crate::read::{Arm, BinaryOp, Case, CastTo, Class, Expr, Read, Stmt};
use crate::{CompileError, Options, StackLimit};

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
    /// `zend_compile_class_decl` bound them.
    classes: Vec<ClassDecl>,
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
    var: Option<(OperandKind, u32)>,
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
/// `ZEND_FREE_SWITCH`, the bit `zend_compile.h` sets on the `FREE` of a
/// `switch` or `match` subject at its end.
const FREE_SWITCH: u32 = 1 << 1;

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
        filename: options.filename.clone(),
        loops: Vec::new(),
        current_loop: None,
        breaks: Vec::new(),
    };
    // A fatal leaves the file with what `zend_error` had raised before
    // it already rendered, so those travel with it.
    if let Err(mut refused) = emit.statements(read) {
        refused.raised = emit.diagnostics;
        return Err(refused);
    }
    emit.final_return();
    emit.resolve_breaks();
    swap_commutative(&mut emit.oplines);
    let live_ranges = crate::live::live_ranges(&mut emit.oplines, emit.temporaries);
    let num_cvs = u32::try_from(emit.vars.len()).unwrap_or(u32::MAX);
    Ok(Unit {
        main: OpArray {
            oplines: emit.oplines.into_boxed_slice(),
            literals: emit.constants.into_boxed_slice(),
            vars: emit.vars.into_boxed_slice(),
            num_cvs,
            num_tmps: emit.temporaries,
            try_catch: Box::new([]),
            live_ranges,
            safepoints: emit.safepoints.into_boxed_slice(),
            filename: options.filename.clone(),
            line_start: 1,
            line_end: lines.max(1),
            cache_size: emit.cache_slots,
        },
        diagnostics: emit.diagnostics,
        classes: emit.classes,
        ..Unit::default()
    })
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
            Stmt::Switch { expr, cases } => self.switch(expr, cases)?,
            Stmt::Break(depth) => self.break_continue(depth.as_ref(), true)?,
            Stmt::Continue(depth) => self.break_continue(depth.as_ref(), false)?,
            Stmt::Class(class) => self.class(class)?,
        }
        Ok(())
    }

    /// `zend_compile_class_decl` for a top-level class with no parent
    /// and no interface: the declaration goes into the unit's classes
    /// and no opline is emitted, since php binds such a class at
    /// compile time (`zend_hash_add_ptr(CG(class_table), …)` under
    /// `toplevel`), and the oracle's dump of `class M {} echo 1;` is the
    /// `ECHO` and the `RETURN` alone. The name a second declaration of
    /// the same file repeats fails that bind in php, which then emits a
    /// `DECLARE_CLASS` for the runtime fatal; that opline is not emitted
    /// here, so the repeat is refused as not yet read. The members are
    /// `zend_compile_prop_decl` and `zend_compile_class_const_decl`: a
    /// name declared twice is php's fatal in php's words, at the
    /// declarator's line, and a default or a value is kept as the
    /// constant expression it was written as (design §4.7).
    fn class(&mut self, class: &Class) -> Result<(), CompileError> {
        let wanted = class.name.to_ascii_lowercase();
        if self
            .classes
            .iter()
            .any(|declared| declared.name.0.to_ascii_lowercase() == wanted)
        {
            return Err(self.fatal("a class declared twice this compiler cannot read yet"));
        }
        let line = self.line;
        let name = Name(class.name.clone().into_boxed_slice());
        let mut decl = ClassDecl {
            name,
            kind: ClassKind::Class,
            flags: ClassFlags::default(),
            parent: None,
            interfaces: Vec::new(),
            traits: Vec::new(),
            backing: EnumBacking::Pure,
            constants: Vec::new(),
            properties: Vec::new(),
            methods: Vec::new(),
            attributes: Vec::new(),
            doc_comment: class.doc_comment.clone().map(Vec::into_boxed_slice),
            file: self.filename.clone(),
            line,
        };
        for constant in &class.constants {
            self.line = constant.line;
            if decl.constants.iter().any(|c| *c.name.0 == *constant.name) {
                return Err(self.compile_error(format!(
                    "Cannot redefine class constant {}::{}",
                    String::from_utf8_lossy(&class.name),
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
        for property in &class.properties {
            self.line = property.line;
            if decl.properties.iter().any(|p| *p.name.0 == *property.name) {
                return Err(self.compile_error(format!(
                    "Cannot redeclare {}::${}",
                    String::from_utf8_lossy(&class.name),
                    String::from_utf8_lossy(&property.name)
                )));
            }
            let default = match &property.default {
                Some(expr) => Some(self.const_expr(expr)?),
                None => None,
            };
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
        self.line = line;
        self.classes.push(decl);
        Ok(())
    }

    /// A constant expression as a declaration keeps it (design §4.7):
    /// `zend_const_expr_to_zval` folds what `zend_eval_const_expr` can,
    /// so `1 + 2` is `3` and a number or a string as written is itself, and what names a
    /// constant stays the tree it was written as, for the engine to
    /// evaluate on first use. `zend_is_allowed_in_const_expr` refuses a
    /// variable, a call, an assignment and an increment in php's words;
    /// a cast or a `match`, which php allows or refuses on its own
    /// terms, is not read yet.
    fn const_expr(&mut self, expr: &Expr) -> Result<ConstExpr, CompileError> {
        self.check_stack()?;
        match expr {
            Expr::Null => Ok(ConstExpr::Null),
            Expr::Bool(b) => Ok(ConstExpr::Bool(*b)),
            Expr::Int(n) => Ok(ConstExpr::Long(*n)),
            Expr::Float(f) => Ok(ConstExpr::Double(*f)),
            Expr::Str(bytes) => Ok(ConstExpr::Str(bytes.clone().into_boxed_slice())),
            Expr::Constant(name) => Ok(ConstExpr::Constant(Name(name.clone().into_boxed_slice()))),
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
            | Expr::Call { .. }
            | Expr::PreInc(_)
            | Expr::PostInc(_)
            | Expr::PreDec(_)
            | Expr::PostDec(_)
            | Expr::StaticCall { .. }
            | Expr::MethodCall { .. } => {
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
        for (statement, line) in body {
            self.line = *line;
            self.statement(statement)?;
        }
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
        self.begin_loop(None, false);
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
        self.begin_loop(None, false);
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

    /// `zend_begin_loop`: one more element of `brk_cont_array`, the
    /// innermost from here, holding the subject's temporary when a
    /// `switch` has one to free.
    fn begin_loop(&mut self, var: Option<Where>, is_switch: bool) {
        let var = match var {
            Some(Where::Temporary(at)) => Some((OperandKind::Tmp, at)),
            Some(Where::Returned(at)) => Some((OperandKind::Var, at)),
            _ => None,
        };
        self.loops.push(Loop {
            parent: self.current_loop,
            is_switch,
            brk: 0,
            cont: 0,
            var,
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
            if let Some((kind, var)) = var {
                self.push(Opline {
                    opcode: Opcode::Free,
                    op1_type: kind,
                    op1: var,
                    extended_value: FREE_ON_RETURN,
                    ..Opline::default()
                });
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
            let target = if jump.is_break {
                target.brk
            } else {
                target.cont
            };
            if let Some(opline) = self.oplines.get_mut(jump.at as usize) {
                opline.op1 = target;
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
        self.begin_loop(Some(subject), true);
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
    /// php steps back over `END_SILENCE`, `EXT_FCALL_END` and `OP_DATA`
    /// to reach the opline that left the value; of the three only
    /// `OP_DATA` is emitted here, after a `FRAMELESS_ICALL_3`.
    fn do_free(&mut self, value: Where) {
        match value {
            Where::Temporary(at) => {
                if let Some(last) = self.last_but_op_data()
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
                let Some(last) = self.last_but_op_data() else {
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

    /// The last opline that is no `OP_DATA`, which is the one that left
    /// the value `zend_do_free` is given.
    fn last_but_op_data(&mut self) -> Option<&mut Opline> {
        self.oplines
            .iter_mut()
            .rev()
            .find(|opline| opline.opcode != Opcode::OpData)
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
    /// target at or before the opline is a back-edge, and carries its
    /// safepoint number as [`Self::final_return`]'s carries its own.
    fn emit_cond_jump(&mut self, cond: Where, target: u32) {
        let (op1_type, op1) = self.operand(cond);
        let at = self.next();
        let out = if target <= at { self.safepoint(at) } else { 0 };
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
            known => Ok(Where::Constant(self.constant_expr(known)?)),
        }
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
        let receiver = self.receiver(base)?;
        let (mut op1_type, mut op1) = self.operand(receiver);
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
            Expr::Var(_)
            | Expr::This
            | Expr::Prop { .. }
            | Expr::Assign { .. }
            | Expr::AssignProp { .. }
            | Expr::PreInc(_)
            | Expr::PostInc(_)
            | Expr::PreDec(_)
            | Expr::PostDec(_) => Err(self.fatal("a variable is no constant expression")),
            Expr::Match { .. } => Err(self.fatal("a match is no constant expression")),
            Expr::New { .. } | Expr::StaticCall { .. } | Expr::MethodCall { .. } => {
                Err(self.fatal("a call is no constant expression"))
            }
            Expr::Instanceof { .. } => Err(self.fatal("an instanceof is no constant expression")),
            // `zend_compile_expr` folds no cast; only a constant
            // expression's walk does (`zend_eval_const_expr`), and none is
            // read yet.
            Expr::Cast { .. } => Err(self.fatal("a cast is no constant expression here")),
        }
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
        let Some(index) = self.native_index(&lowered) else {
            return self.dynamic_call(&resolved, arguments);
        };
        if let Some(served) = self.special(&lowered, arguments)? {
            return Ok(served);
        }
        if let Some(served) = self.frameless_icall(index, arguments, None)? {
            return Ok(served);
        }
        let lowered = self.string(&lowered)?;
        let lowered = self.constant(lowered);
        let count = u32::try_from(arguments.len()).unwrap_or(0);
        let slot = self.cache_slot();
        self.push(Opline {
            opcode: Opcode::InitFcall,
            op1: used_stack(count),
            op2_type: OperandKind::Const,
            op2: lowered,
            out: slot,
            extended_value: count,
            ..Opline::default()
        });
        self.args(arguments, true)?;
        Ok(self.call_op(Opcode::DoIcall))
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
        Ok(self.call_op(Opcode::DoFcall))
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
        let receiver = self.receiver(object)?;
        let (op1_type, op1) = self.operand(receiver);
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
    fn receiver(&mut self, object: &Expr) -> Result<Where, CompileError> {
        match object {
            Expr::This => Ok(self.fetch_this(OperandKind::Var)),
            other => self.expr(other),
        }
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
        let receiver = self.receiver(object)?;
        let (op1_type, op1) = self.operand(receiver);
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
    /// inside the class, which nothing here is.
    fn known_method(&self, class: &[u8], method: &[u8]) -> bool {
        let class = self.class_name(class).to_ascii_lowercase();
        let method = method.to_ascii_lowercase();
        self.known_classes
            .iter()
            .chain(self.classes.iter())
            .find(|decl| decl.name.0.to_ascii_lowercase() == class)
            .is_some_and(|decl| {
                decl.methods.iter().any(|m| {
                    m.function.name.0.to_ascii_lowercase() == method
                        && m.visibility == Visibility::Public
                })
            })
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

    /// `zend_compile_const` over `zend_resolve_const_name` and
    /// `zend_try_ct_eval_const`: the value of the constant `name` names
    /// where php substitutes it, and nothing where php emits
    /// `FETCH_CONSTANT`. A leading `\` names the global constant; an
    /// unqualified name in a namespace names the namespace's own, which
    /// nothing declares, so php fetches it as the program serves and
    /// falls back to the global one then; outside a namespace it is
    /// the persistent table's.
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

    /// `zend_emit_final_return(true)`: the main op array returns the
    /// constant `1`, and the return carries its safepoint number.
    fn final_return(&mut self) {
        let one = self.constant(Zval::long(1));
        let at = self.next();
        let number = self.safepoint(at);
        self.push(Opline {
            opcode: Opcode::Return,
            op1_type: OperandKind::Const,
            op1: one,
            out: number,
            extended_value: u32::MAX,
            ..Opline::default()
        });
    }
}

/// `zend_vm_calc_used_stack` for a native callee, the number
/// `zend_compile_call_common` writes into an `INIT_FCALL`'s `op1`
/// (`Z/Zend/zend_compile.c:3983`): the frame header's slots
/// (`ZEND_CALL_FRAME_SLOT`, five zvals on a 64-bit build), one slot per
/// argument, and the callee's temporaries, of which a native has none
/// while nothing observes it (`Z/Zend/zend_API.c:2988-2995`), in bytes.
/// php's own VM sizes its stack by this number; here it is what the
/// dump prints, `INIT_FCALL 1 96 string("set_time_limit")`, and the
/// engine's stack is sized by the engine.
fn used_stack(arguments: u32) -> u32 {
    const CALL_FRAME_SLOT: u32 = 5;
    let zval = u32::try_from(std::mem::size_of::<Zval>()).unwrap_or(16);
    CALL_FRAME_SLOT
        .saturating_add(arguments)
        .saturating_mul(zval)
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
