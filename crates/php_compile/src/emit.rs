//! Emission, after `zend_compile.c` (design §6.1): what the statements
//! read from a source file become as oplines, with php's operand kinds,
//! php's constant table and php's safepoint numbering (design §5.7).
//!
//! One op array is built at a time. Constants are added as
//! `zend_add_literal` adds them, the same value twice keeping one slot;
//! temporaries are numbered as `get_temporary_variable` numbers them, and
//! a temporary's slot on the frame is its number past the last CV.

use php_heap::{Str, Zval};
use php_unit::decl::Name;
use php_unit::{OpArray, Opcode, OperandKind, Opline, OplineIdx};

use crate::read::{Expr, Read, Stmt};

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
pub(crate) fn main_op_array(read: &Read, filename: &Name, lines: u32) -> OpArray {
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
        emit.statement(statement);
    }
    emit.final_return();
    OpArray {
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
    }
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

    fn statement(&mut self, statement: &Stmt) {
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
                    let value = self.expr(part, true);
                    self.push(Opline {
                        opcode: Opcode::Echo,
                        op1_type: value.kind(),
                        op1: value.field(),
                        ..Opline::default()
                    });
                }
            }
            Stmt::Effect(expr) => {
                self.expr(expr, false);
            }
        }
    }

    /// Compile `expr` and say where its value is. `used` is php's
    /// `result_used`: a call whose value nothing reads writes none.
    fn expr(&mut self, expr: &Expr, used: bool) -> Where {
        match expr {
            Expr::Null => Where::Constant(self.constant(Zval::null())),
            Expr::Bool(b) => Where::Constant(self.constant(Zval::bool(*b))),
            Expr::Int(n) => Where::Constant(self.constant(Zval::long(*n))),
            Expr::Float(f) => Where::Constant(self.constant(Zval::double(*f))),
            Expr::Str(bytes) => Where::Constant(self.constant(Zval::string(Str::intern(bytes)))),
            Expr::Call { name, arguments } => self.call(name, arguments, used),
        }
    }

    /// `zend_compile_call` for a name known at compile time, and
    /// `zend_compile_args` for its arguments. Every callable this engine
    /// has is a native, so the call op is `DO_ICALL`; the compile-time
    /// table that also answers `INIT_FCALL_BY_NAME` and `DO_UCALL` comes
    /// with the engine's declarations.
    fn call(&mut self, name: &[u8], arguments: &[Expr], used: bool) -> Where {
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
            let value = self.expr(argument, true);
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
        let out = if used { self.temporary() } else { 0 };
        self.push(Opline {
            opcode: Opcode::DoIcall,
            out,
            ..Opline::default()
        });
        Where::Returned(out)
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
