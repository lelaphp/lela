//! Emission, after `zend_compile.c` (design §6.1): what the statements
//! read from a source file become as oplines, with php's operand kinds,
//! php's constant table and php's safepoint numbering (design §5.7).
//!
//! One op array is built at a time. Constants are added as
//! `zend_add_literal` adds them, the same value twice keeping one slot;
//! temporaries are numbered as `get_temporary_variable` numbers them, and
//! a temporary's slot on the frame is its number past the last CV.
//!
//! Arithmetic over values known here is folded as php folds it
//! (`zend_try_ct_eval_binary_op`), so `1/3` is one constant and no opline.
//! What it cannot fold becomes the opcode php emits, into a temporary as
//! `zend_emit_op_tmp` leaves it: `ZEND_ADD`, `ZEND_SUB`, `ZEND_MUL` and
//! `ZEND_DIV`, whose kernels the engine serves. `%` and `**` fold here
//! and no handler serves their opcodes, so what does not fold is a
//! [`CompileError`] rather than an opline that would fatal under the loop.
//!
//! A loop is emitted as `zend_compile_for` and `zend_compile_while` emit
//! one: a `JMP` forward over the body to the condition, and the condition
//! as a `JMPNZ` back to the body's first opline, which is the back-edge
//! and carries its safepoint number (design §5.7). php folds nothing
//! here, so `for (;;)` is a `JMPNZ` over the constant `true` and not a
//! `JMP`, which is the shape the compile-identity lane holds this to.

use php_heap::{Fatal, Intern, Zval};
use php_unit::decl::Name;
use php_unit::{OpArray, Opcode, OperandKind, Opline, OplineIdx};

use crate::CompileError;
use crate::read::{BinaryOp, Expr, Read, Stmt};

/// One op array under construction, and the home its constant strings
/// are interned into (design §5.2): the engine's table when the unit is
/// bound for the unit cache, the request's own otherwise, and the
/// compiler never learns which (`Z/Zend/zend_compile.c`'s
/// `zend_add_literal_string`, which calls `zend_new_interned_string`
/// through whichever handler is installed).
pub(crate) struct Emit<'a> {
    strings: &'a mut dyn Intern,
    oplines: Vec<Opline>,
    constants: Vec<Zval>,
    safepoints: Vec<OplineIdx>,
    temporaries: u32,
    cache_slots: u32,
    line: u32,
}

/// Where an expression left its value, php's `znode`: the value itself
/// for a constant, which reaches the table only when the opline that
/// names it is emitted (`zend_add_literal`); a temporary holding what a
/// call returned (php's `IS_VAR`); or a temporary an operator wrote
/// (php's `IS_TMP_VAR`).
#[derive(Debug, Clone, Copy)]
enum Where {
    Constant(Zval),
    Returned(u32),
    Temporary(u32),
}

/// The main op array of one source file: its statements, then php's
/// implicit final `RETURN 1` (`zend_emit_final_return`, with
/// `extended_value` at -1).
pub(crate) fn main_op_array(
    read: &Read,
    filename: &Name,
    lines: u32,
    strings: &mut dyn Intern,
) -> Result<OpArray, CompileError> {
    let mut emit = Emit {
        strings,
        oplines: Vec::new(),
        constants: Vec::new(),
        safepoints: vec![OplineIdx(0)],
        temporaries: 0,
        cache_slots: 0,
        line: 1,
    };
    emit.statements(read)?;
    emit.final_return();
    Ok(OpArray {
        oplines: emit.oplines.into_boxed_slice(),
        literals: emit.constants.into_boxed_slice(),
        vars: Box::new([]),
        num_cvs: 0,
        num_tmps: emit.temporaries,
        try_catch: Box::new([]),
        live_ranges: Box::new([]),
        safepoints: emit.safepoints.into_boxed_slice(),
        filename: filename.clone(),
        line_start: 1,
        line_end: lines.max(1),
        cache_size: emit.cache_slots,
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

    fn cache_slot(&mut self) -> u32 {
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

    /// The constant string for these bytes, interned into whichever home
    /// this compilation was given: php's `zend_new_interned_string` over
    /// the string a `CONST` operand holds. A home with no memory left
    /// refuses, and the refusal is the compile-time fatal php raises
    /// where it runs out (`Z/Zend/zend_alloc.c:1059`).
    fn string(&mut self, bytes: &[u8]) -> Result<Zval, CompileError> {
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
                self.expr(expr)?;
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
        self.expr_list(init)?;
        let jump = self.push(Opline {
            opcode: Opcode::Jmp,
            ..Opline::default()
        });
        let start = self.next();
        self.statements(body)?;
        self.line = line;
        self.expr_list(step)?;
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
    /// and where the last one left its value; the constant `true` for a
    /// list with nothing in it, which is what `for (;;)` tests.
    fn expr_list(&mut self, exprs: &[Expr]) -> Result<Where, CompileError> {
        let mut last = Where::Constant(Zval::bool(true));
        for expr in exprs {
            last = self.expr(expr)?;
        }
        Ok(last)
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
        }
    }

    /// `zend_compile_expr`: compile `expr` and say where its value is.
    fn expr(&mut self, expr: &Expr) -> Result<Where, CompileError> {
        match expr {
            Expr::Call { name, arguments } => self.call(name, arguments),
            Expr::Unary { minus, operand } => self.unary(*minus, operand),
            Expr::Binary { op, left, right } => self.binary(*op, left, right),
            known => Ok(Where::Constant(self.constant_expr(known)?)),
        }
    }

    /// `zend_compile_binary_op`: the value where
    /// `zend_try_ct_eval_binary_op` folds the operator over two constants,
    /// and the opcode into a temporary where it does not
    /// (`zend_emit_op_tmp`).
    fn binary(&mut self, op: BinaryOp, left: &Expr, right: &Expr) -> Result<Where, CompileError> {
        if let (Ok(a), Ok(b)) = (self.constant_expr(left), self.constant_expr(right))
            && let Some(folded) = fold(op, a, b)
        {
            return Ok(Where::Constant(folded));
        }
        let opcode = self.opcode_of(op)?;
        let left = self.expr(left)?;
        let right = self.expr(right)?;
        Ok(self.emit_binary(opcode, left, right))
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
            out,
            ..Opline::default()
        });
        Where::Temporary(out)
    }

    /// The opcode `zend_compile_binary_op` gives each operator. php has
    /// one for `%` and `**` too and this engine serves neither, so an
    /// expression that did not fold is refused where it stands rather
    /// than compiled into an opline the loop would fatal on.
    fn opcode_of(&self, op: BinaryOp) -> Result<Opcode, CompileError> {
        match op {
            BinaryOp::Add => Ok(Opcode::Add),
            BinaryOp::Sub => Ok(Opcode::Sub),
            BinaryOp::Mul => Ok(Opcode::Mul),
            BinaryOp::Div => Ok(Opcode::Div),
            BinaryOp::Mod | BinaryOp::Pow => {
                Err(self.fatal("arithmetic this engine serves no opcode for yet"))
            }
        }
    }

    /// php's constant expression: the value `zend_compile_expr` leaves as
    /// an `IS_CONST` operand, folding a number as written, a persistent constant
    /// (`zend_try_ct_eval_const`) and arithmetic over two of them
    /// (`zend_try_ct_eval_binary_op`).
    fn constant_expr(&mut self, expr: &Expr) -> Result<Zval, CompileError> {
        match expr {
            Expr::Null => Ok(Zval::null()),
            Expr::Bool(b) => Ok(Zval::bool(*b)),
            Expr::Int(n) => Ok(Zval::long(*n)),
            Expr::Float(f) => Ok(Zval::double(*f)),
            Expr::Str(bytes) => self.string(bytes),
            Expr::Constant(name) => persistent(name)
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
            Expr::Binary { op, left, right } => {
                let left = self.constant_expr(left)?;
                let right = self.constant_expr(right)?;
                fold(*op, left, right)
                    .ok_or_else(|| self.fatal("arithmetic this compiler cannot fold yet"))
            }
            Expr::Call { .. } => {
                Err(self.fatal("arithmetic over a call this compiler cannot fold yet"))
            }
        }
    }

    /// `zend_compile_call` for a name known at compile time, and
    /// `zend_compile_args` for its arguments. Every callable this engine
    /// has is a native, so the call op is `DO_ICALL`; the compile-time
    /// table that also answers `INIT_FCALL_BY_NAME` and `DO_UCALL` comes
    /// with the engine's declarations.
    ///
    /// Every call is given a temporary for what it returns. php leaves
    /// the field unused on a call nothing reads, and an [`Opline`] has
    /// nowhere to say so: the design's twenty-four bytes carry
    /// `op1_type` and `op2_type` and no third (design §5.7).
    fn call(&mut self, name: &[u8], arguments: &[Expr]) -> Result<Where, CompileError> {
        let lowered = self.string(&unqualified(name))?;
        let lowered = self.constant(lowered);
        let count = u32::try_from(arguments.len()).unwrap_or(0);
        let slot = self.cache_slot();
        self.push(Opline {
            opcode: Opcode::InitFcall,
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
                // php sends a VAR by `SEND_VAR` and anything else by
                // `SEND_VAL` (`zend_compile_args`, the not-by-reference arms).
                opcode: match op1_type {
                    OperandKind::Var => Opcode::SendVar,
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

/// `zend_try_ct_eval_const`: php substitutes a persistent constant's
/// value where it stands, so `INF` and `NAN` reach no opline. php reads
/// them from the table `ext/standard` registers
/// (`REGISTER_DOUBLE_CONSTANT("INF", ZEND_INFINITY, CONST_PERSISTENT)`,
/// `Z/ext/standard/basic_functions_arginfo.h:3580`); the engine's table
/// does not reach the compiler yet, so the two a float needs are here. A
/// constant's name is case-sensitive, and a leading `\` names the same
/// global constant as none.
fn persistent(name: &[u8]) -> Option<Zval> {
    let mut at = 0;
    while name.get(at) == Some(&b'\\') {
        at += 1;
    }
    match name.get(at..)? {
        b"INF" => Some(Zval::double(f64::INFINITY)),
        b"NAN" => Some(Zval::double(f64::NAN)),
        _ => None,
    }
}

/// One operand of php's arithmetic: the two number types `add_function`
/// and its kin switch on.
#[derive(Clone, Copy)]
enum Number {
    Long(i64),
    Double(f64),
}

impl Number {
    /// The operand as the double php's mixed arms convert it to.
    #[expect(
        clippy::cast_precision_loss,
        reason = "php's `(double) lval`, whose rounding is the behaviour being copied"
    )]
    fn double(self) -> f64 {
        match self {
            Number::Long(n) => n as f64,
            Number::Double(d) => d,
        }
    }
}

/// The number a constant holds, or `None` for a value php's arithmetic
/// would convert: a string, a bool or null, whose conversion this
/// compiler does not make yet.
fn number(value: Zval) -> Option<Number> {
    value
        .as_long()
        .map(Number::Long)
        .or_else(|| value.as_double().map(Number::Double))
}

/// `zend_try_ct_eval_binary_op`: the value php folds an arithmetic
/// operator over two constants to, or `None` where php leaves it to the
/// opcode. php's guard is `zend_binary_op_produces_error`: a division or
/// a modulo by zero, a modulo over a double, and `0 ** (<0)`, each of
/// which throws where it stands. php folds an operand it would convert
/// too, a numeric string, a bool or null; this folds two numbers and
/// leaves the rest to the opcode, whose kernel makes php's conversion.
fn fold(op: BinaryOp, left: Zval, right: Zval) -> Option<Zval> {
    match (number(left)?, number(right)?) {
        (Number::Long(a), Number::Long(b)) => longs(op, a, b),
        (a, b) => doubles(op, a.double(), b.double()),
    }
}

/// The long arms of `add_function`, `sub_function`, `mul_function`,
/// `div_function`, `mod_function` and `pow_function`: the value php's
/// `fast_long_*` macros leave, a double where the operation overflowed
/// and, for a division, where it did not come out whole.
#[expect(
    clippy::cast_precision_loss,
    reason = "php's `(double) lval`, whose rounding is the behaviour being copied"
)]
fn longs(op: BinaryOp, a: i64, b: i64) -> Option<Zval> {
    let overflowed = |n: Option<i64>, d: f64| Some(n.map_or_else(|| Zval::double(d), Zval::long));
    match op {
        BinaryOp::Add => overflowed(a.checked_add(b), a as f64 + b as f64),
        BinaryOp::Sub => overflowed(a.checked_sub(b), a as f64 - b as f64),
        BinaryOp::Mul => overflowed(a.checked_mul(b), a as f64 * b as f64),
        // `zend_binary_op_produces_error`: a division or a modulo by
        // zero throws, and both are left to the opcode.
        BinaryOp::Div | BinaryOp::Mod if b == 0 => None,
        // php's own case, "prevent overflow error/crash".
        BinaryOp::Div if a == i64::MIN && b == -1 => Some(Zval::double(a as f64 / -1.0)),
        BinaryOp::Div if a % b == 0 => Some(Zval::long(a / b)),
        BinaryOp::Div => Some(Zval::double(a as f64 / b as f64)),
        // php's own case, `% -1` being zero where the division overflows.
        BinaryOp::Mod if b == -1 => Some(Zval::long(0)),
        BinaryOp::Mod => Some(Zval::long(a % b)),
        BinaryOp::Pow => Some(power(a, b)),
    }
}

/// `pow_function_base`'s long arm: php's square-and-multiply, a long
/// while every step fits and the double php computes from `pow()` once
/// one overflows. A negative exponent is php's `pow()` from the start.
#[expect(
    clippy::cast_precision_loss,
    reason = "php's `(double) lval`, whose rounding is the behaviour being copied"
)]
fn power(a: i64, b: i64) -> Zval {
    if b < 0 {
        return Zval::double((a as f64).powf(b as f64));
    }
    if b == 0 {
        return Zval::long(1);
    }
    if a == 0 {
        return Zval::long(0);
    }
    let (mut l1, mut l2, mut i) = (1_i64, a, b);
    while i >= 1 {
        if i % 2 == 1 {
            i -= 1;
            let Some(product) = l1.checked_mul(l2) else {
                // php's `dval * pow(l2, i)`, the multiply having overflowed.
                return Zval::double(l1 as f64 * l2 as f64 * (l2 as f64).powf(i as f64));
            };
            l1 = product;
        } else {
            i /= 2;
            let Some(square) = l2.checked_mul(l2) else {
                // php's `(double) l1 * pow(dval, i)`, `dval` being the
                // square that overflowed.
                let squared = l2 as f64 * l2 as f64;
                return Zval::double(l1 as f64 * squared.powf(i as f64));
            };
            l2 = square;
        }
        if i == 0 {
            return Zval::long(l1);
        }
    }
    Zval::long(l1)
}

/// The double arms of the same four, where php converts both operands
/// and works in doubles.
fn doubles(op: BinaryOp, a: f64, b: f64) -> Option<Zval> {
    match op {
        BinaryOp::Add => Some(Zval::double(a + b)),
        BinaryOp::Sub => Some(Zval::double(a - b)),
        BinaryOp::Mul => Some(Zval::double(a * b)),
        // php's guard is `zval_get_double(op2) == 0.0`, which -0.0
        // meets as well.
        BinaryOp::Div if b == 0.0 => None,
        BinaryOp::Div => Some(Zval::double(a / b)),
        // A modulo casts both sides to a long, which php refuses to fold
        // where either is a double that is not a whole number in range;
        // this leaves every double modulo to the opcode.
        BinaryOp::Mod => None,
        // `0 ** (<0)` throws `DivisionByZeroError` where it stands.
        BinaryOp::Pow if a == 0.0 && b < 0.0 => None,
        BinaryOp::Pow => Some(Zval::double(a.powf(b))),
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
