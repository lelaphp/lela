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
//! php emits `ZEND_ADD` for what it cannot fold; no handler serves an
//! arithmetic opcode yet, so what php would leave to one is a
//! [`CompileError`] rather than something else compiled in its place.

use php_heap::{Str, Zval};
use php_unit::decl::Name;
use php_unit::{OpArray, Opcode, OperandKind, Opline, OplineIdx};

use crate::CompileError;
use crate::read::{BinaryOp, Expr, Read, Stmt};

/// One op array under construction.
pub(crate) struct Emit {
    oplines: Vec<Opline>,
    constants: Vec<Zval>,
    safepoints: Vec<OplineIdx>,
    temporaries: u32,
    cache_slots: u32,
    line: u32,
}

/// Where an expression left its value: a constant's slot, or a temporary
/// holding what a call returned (php's `IS_VAR`).
#[derive(Debug, Clone, Copy)]
enum Where {
    Constant(u32),
    Returned(u32),
}

impl Where {
    fn kind(self) -> OperandKind {
        match self {
            Where::Constant(_) => OperandKind::Const,
            Where::Returned(_) => OperandKind::Var,
        }
    }

    fn field(self) -> u32 {
        match self {
            Where::Constant(at) | Where::Returned(at) => at,
        }
    }
}

/// The main op array of one source file: its statements, then php's
/// implicit final `RETURN 1` (`zend_emit_final_return`, with
/// `extended_value` at -1).
pub(crate) fn main_op_array(
    read: &Read,
    filename: &Name,
    lines: u32,
) -> Result<OpArray, CompileError> {
    let mut emit = Emit {
        oplines: Vec::new(),
        constants: Vec::new(),
        safepoints: vec![OplineIdx(0)],
        temporaries: 0,
        cache_slots: 0,
        line: 1,
    };
    for (statement, line) in read {
        emit.line = *line;
        emit.statement(statement)?;
    }
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

impl Emit {
    fn push(&mut self, opline: Opline) -> u32 {
        let at = u32::try_from(self.oplines.len()).unwrap_or(u32::MAX);
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

    fn statement(&mut self, statement: &Stmt) -> Result<(), CompileError> {
        match statement {
            Stmt::Inline(text) => {
                let at = self.constant(Zval::string(Str::intern(text)));
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
                    self.push(Opline {
                        opcode: Opcode::Echo,
                        op1_type: value.kind(),
                        op1: value.field(),
                        ..Opline::default()
                    });
                }
            }
            Stmt::Effect(expr) => {
                self.expr(expr)?;
            }
        }
        Ok(())
    }

    /// Compile `expr` and say where its value is. Everything but a call
    /// is a constant expression here, and its slot in the constant table
    /// is where it is.
    fn expr(&mut self, expr: &Expr) -> Result<Where, CompileError> {
        match expr {
            Expr::Call { name, arguments } => self.call(name, arguments),
            known => {
                let value = self.constant_expr(known)?;
                Ok(Where::Constant(self.constant(value)))
            }
        }
    }

    /// php's constant expression: the value `zend_compile_expr` leaves as
    /// an `IS_CONST` operand, folding a number as written, a persistent constant
    /// (`zend_try_ct_eval_const`) and arithmetic over two of them
    /// (`zend_try_ct_eval_binary_op`).
    fn constant_expr(&self, expr: &Expr) -> Result<Zval, CompileError> {
        match expr {
            Expr::Null => Ok(Zval::null()),
            Expr::Bool(b) => Ok(Zval::bool(*b)),
            Expr::Int(n) => Ok(Zval::long(*n)),
            Expr::Float(f) => Ok(Zval::double(*f)),
            Expr::Str(bytes) => Ok(Zval::string(Str::intern(bytes))),
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
        let lowered = self.constant(Zval::string(Str::intern(&unqualified(name))));
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
            self.push(Opline {
                // php sends a VAR by `SEND_VAR` and anything else by
                // `SEND_VAL` (`zend_compile_args`, the not-by-reference arms).
                opcode: match value.kind() {
                    OperandKind::Var => Opcode::SendVar,
                    _ => Opcode::SendVal,
                },
                op1_type: value.kind(),
                op1: value.field(),
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
        let at = u32::try_from(self.oplines.len()).unwrap_or(u32::MAX);
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
/// opcode. php's guard is `zend_binary_op_produces_error`, which for the
/// four operators here is a division whose right side is zero as a
/// double; that one throws `DivisionByZeroError` and is never folded.
/// php folds an operand it would convert too, a numeric string or a
/// bool; this folds two numbers and refuses the rest, since the
/// conversion is not made yet.
fn fold(op: BinaryOp, left: Zval, right: Zval) -> Option<Zval> {
    match (number(left)?, number(right)?) {
        (Number::Long(a), Number::Long(b)) => longs(op, a, b),
        (a, b) => doubles(op, a.double(), b.double()),
    }
}

/// The long arms of `add_function`, `sub_function`, `mul_function` and
/// `div_function`: the value php's `fast_long_*` macros leave, a double
/// where the operation overflowed and, for a division, where it did not
/// come out whole.
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
        // `zend_binary_op_produces_error`: a division by zero throws,
        // and is left to the opcode.
        BinaryOp::Div if b == 0 => None,
        // php's own case, "prevent overflow error/crash".
        BinaryOp::Div if a == i64::MIN && b == -1 => Some(Zval::double(a as f64 / -1.0)),
        BinaryOp::Div if a % b == 0 => Some(Zval::long(a / b)),
        BinaryOp::Div => Some(Zval::double(a as f64 / b as f64)),
    }
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
