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
//! `2 * $a` and `2 == $a` read the CV first. A call of a name in
//! [`crate::special`]'s table is served as php's compiler serves it,
//! a constant or an opcode of its own (`ZEND_STRLEN`, `ZEND_TYPE_CHECK`,
//! `ZEND_DEFINED`, `ZEND_COUNT`), where the engine's function table
//! holds the name as a native ([`Options::functions`]); any other call
//! is `INIT_FCALL`, `SEND_*` and `DO_ICALL`.
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

use php_heap::{Fatal, Intern, Number, Scalar, Zval, ZvalType, double_to_string, numeric_prefix};
use php_unit::decl::{ConstDecl, ConstExpr, FunctionDecl, Name};
use php_unit::{Diagnostic, Level, OpArray, Opcode, OperandKind, Opline, OplineIdx, Unit};

use crate::read::{BinaryOp, Expr, Read, Stmt};
use crate::{CompileError, Options};

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
    oplines: Vec<Opline>,
    constants: Vec<Zval>,
    vars: Vec<Name>,
    safepoints: Vec<OplineIdx>,
    temporaries: u32,
    cache_slots: u32,
    line: u32,
    /// What `zend_error` raised so far, [`Unit::diagnostics`].
    diagnostics: Vec<Diagnostic>,
}

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
        oplines: Vec::new(),
        constants: Vec::new(),
        vars: Vec::new(),
        safepoints: vec![OplineIdx(0)],
        temporaries: 0,
        cache_slots: 0,
        line: 1,
        diagnostics: Vec::new(),
    };
    emit.statements(read)?;
    emit.final_return();
    swap_commutative(&mut emit.oplines);
    let num_cvs = u32::try_from(emit.vars.len()).unwrap_or(u32::MAX);
    Ok(Unit {
        main: OpArray {
            oplines: emit.oplines.into_boxed_slice(),
            literals: emit.constants.into_boxed_slice(),
            vars: emit.vars.into_boxed_slice(),
            num_cvs,
            num_tmps: emit.temporaries,
            try_catch: Box::new([]),
            live_ranges: Box::new([]),
            safepoints: emit.safepoints.into_boxed_slice(),
            filename: options.filename.clone(),
            line_start: 1,
            line_end: lines.max(1),
            cache_size: emit.cache_slots,
        },
        diagnostics: emit.diagnostics,
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

    pub(crate) fn cache_slot(&mut self) -> u32 {
        let at = self.cache_slots;
        self.cache_slots = self.cache_slots.saturating_add(1);
        at
    }

    /// The number of a new safepoint at `opline`; the entry is zero and
    /// is already there.
    fn safepoint(&mut self, at: u32) -> u32 {
        let number = u32::try_from(self.safepoints.len()).unwrap_or(0);
        self.safepoints.push(OplineIdx(at));
        number
    }

    /// The compile-time fatal for what stands on the statement's line.
    fn fatal(&self, message: impl Into<String>) -> CompileError {
        CompileError {
            message: message.into(),
            line: self.line,
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
        match statement {
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
        }
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
        let start = self.next();
        self.statements(body)?;
        self.line = line;
        let step = self.expr_list(step)?;
        self.do_free(step);
        self.update_jump_target_to_next(jump);
        let cond = self.expr_list(cond)?;
        self.emit_cond_jump(cond, start);
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
        let start = self.next();
        self.statements(body)?;
        self.line = line;
        self.update_jump_target_to_next(jump);
        let cond = self.expr(cond)?;
        self.emit_cond_jump(cond, start);
        Ok(())
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
    /// to reach the opline that left the value; none of the three is
    /// emitted here, so the last opline is the one.
    fn do_free(&mut self, value: Where) {
        match value {
            Where::Temporary(at) => {
                if let Some(last) = self.oplines.last_mut()
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
                let Some(last) = self.oplines.last_mut() else {
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

    /// `zend_update_jump_target_to_next`: the `JMP` at `jump` aimed at
    /// the opline about to be emitted.
    fn update_jump_target_to_next(&mut self, jump: u32) {
        let target = self.next();
        if let Some(opline) = self.oplines.get_mut(jump as usize) {
            opline.op1 = target;
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
        match expr {
            Expr::Call { name, arguments } => self.call(name, arguments),
            Expr::Unary { minus, operand } => self.unary(*minus, operand),
            Expr::Binary { op, left, right } => self.binary(*op, left, right),
            // `zend_compile_simple_var` through `zend_try_compile_cv`: the
            // variable is its CV, and no opline reads it into anything.
            Expr::Var(name) => Ok(Where::Cv(self.lookup_cv(name))),
            Expr::Assign { var, value } => self.assign(var, value),
            Expr::PreInc(var) => Ok(self.incdec(var, Opcode::PreInc)),
            Expr::PostInc(var) => Ok(self.incdec(var, Opcode::PostInc)),
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
        if let (Ok(a), Ok(b)) = (self.constant_expr(left), self.constant_expr(right))
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
        if let Ok(value) = self.constant_expr(operand)
            && let Some(folded) = fold(BinaryOp::Mul, value, sign)
        {
            return Ok(Where::Constant(folded));
        }
        let operand = self.expr(operand)?;
        Ok(self.emit_binary(Opcode::Mul, operand, Where::Constant(sign)))
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
        match expr {
            Expr::Null => Ok(Zval::null()),
            Expr::Bool(b) => Ok(Zval::bool(*b)),
            Expr::Int(n) => Ok(Zval::long(*n)),
            Expr::Float(f) => Ok(Zval::double(*f)),
            Expr::Str(bytes) => self.string(bytes),
            Expr::Constant(name) => self
                .persistent(name)?
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
            Expr::Call { name, arguments } => self
                .special_constant(&unqualified(name), arguments)?
                .ok_or_else(|| self.fatal("a call this compiler cannot fold")),
            Expr::Var(_) | Expr::Assign { .. } | Expr::PreInc(_) | Expr::PostInc(_) => {
                Err(self.fatal("a variable is no constant expression"))
            }
        }
    }

    /// `zend_compile_call` for a name known at compile time: what
    /// `zend_try_compile_special_func` makes of the call where the name
    /// is one of the engine's natives ([`Self::native`]) and in
    /// [`crate::special`]'s table, a constant or an opcode of its own,
    /// and the call otherwise, with `zend_compile_args` for its
    /// arguments. Every callable this engine has is a native, so the
    /// call op is `DO_ICALL`; a name the table has not is
    /// `INIT_FCALL_BY_NAME` in php (`zend_compile_dynamic_call`) and the
    /// same `INIT_FCALL` here until that opcode has a handler (#252), and
    /// `DO_UCALL` comes with user functions.
    ///
    /// Every call is given a `VAR` for what it returns (`zend_emit_op`
    /// with a `result`, `zend_make_var_result`), numbered whether or not
    /// anything reads it; [`Self::do_free`] marks it `UNUSED` when
    /// nothing does, which is what the engine's `DO_ICALL` reads as
    /// `RETURN_VALUE_USED`.
    fn call(&mut self, name: &[u8], arguments: &[Expr]) -> Result<Where, CompileError> {
        let lowered = unqualified(name);
        if let Some(served) = self.special(&lowered, arguments)? {
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
        for (at, argument) in arguments.iter().enumerate() {
            let value = self.expr(argument)?;
            let number = u32::try_from(at).unwrap_or(0) + 1;
            let (op1_type, op1) = self.operand(value);
            self.push(Opline {
                // php sends a VAR or a CV by `SEND_VAR` and anything else
                // by `SEND_VAL` (`zend_compile_args`, the not-by-reference
                // arms).
                opcode: match op1_type {
                    OperandKind::Var | OperandKind::Cv => Opcode::SendVar,
                    _ => Opcode::SendVal,
                },
                op1_type,
                op1,
                op2: number,
                out: number - 1,
                ..Opline::default()
            });
        }
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::DoIcall,
            out_type: OperandKind::Var,
            out,
            ..Opline::default()
        });
        Ok(Where::Returned(out))
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
        self.functions
            .iter()
            .any(|decl| decl.name.0.eq_ignore_ascii_case(lowered))
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

/// `zend_resolve_function_name` for a name in no namespace, lowered as
/// `zend_string_tolower` lowers it for `INIT_FCALL`'s operand: the
/// leading separator of `\var_dump` dropped and the rest as written.
fn unqualified(name: &[u8]) -> Vec<u8> {
    let mut at = 0;
    while name.get(at) == Some(&b'\\') {
        at += 1;
    }
    name[at..].to_ascii_lowercase()
}
