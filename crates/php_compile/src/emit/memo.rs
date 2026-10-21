//! `??=`, the one form php compiles a place twice for: once under
//! `BP_VAR_IS` to read it and once under `BP_VAR_W` to write it
//! (`zend_compile_assign_coalesce`, `Z/Zend/zend_compile.c:10597`). What
//! the place's chain computes on the way, a dimension, a call's value, a
//! class the program names, is computed once: php's
//! `CG(memoize_mode)` and `CG(memoized_exprs)` hold what each expression
//! left in the read for the write (`zend_compile_memoized_expr`,
//! `Z/Zend/zend_compile.c:2587`), which is [`Memo`] here, consulted by
//! [`Emit::expr`] and by nothing else.
//!
//! The read is [`Emit::silent_read`], the `??`'s own, and the write is
//! [`Emit::assign_to`] over the decoded [`Target`] with the value as a
//! node the compiler already holds, the one write path every assignment
//! takes; what stands here is the memo between them and the `FREE`s
//! after.

use php_syntax::tree;
use php_unit::{Opcode, OperandKind, Opline};

use super::target::Target;
use super::{Emit, Source, Where};
use crate::CompileError;

/// `zend_memoize_mode`: whether an expression compiled now is compiled
/// and held (`ZEND_MEMOIZE_COMPILE`), answered by what was held
/// (`ZEND_MEMOIZE_FETCH`), or compiled as any other (`ZEND_MEMOIZE_NONE`).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum MemoMode {
    #[default]
    None,
    Compile,
    Fetch,
}

/// `CG(memoize_mode)` beside `CG(memoized_exprs)`: each expression the
/// read of a `??=` compiled, by its node, with the copy the write reads,
/// in the order php's table holds them, which is the order the `FREE`s
/// after the write stand in.
#[derive(Default)]
pub(super) struct Memo {
    pub(super) mode: MemoMode,
    held: Vec<(tree::Expr, Where)>,
}

impl Emit<'_> {
    /// `zend_compile_memoized_expr` (`Z/Zend/zend_compile.c:2587`), what
    /// [`Self::expr`] is while a `??=` compiles its place: under
    /// `ZEND_MEMOIZE_COMPILE` the node compiled as any other and a
    /// `COPY_TMP` of what it left in a temporary or a `VAR` held for the
    /// write, the original the read's own; under `ZEND_MEMOIZE_FETCH`
    /// the held copy, and no opline at all. A call of its own, so what
    /// it takes is off the stack of every other nested compile (#3159).
    #[inline(never)]
    pub(super) fn memoized(&mut self, node: tree::Expr) -> Result<Where, CompileError> {
        if self.memo.mode == MemoMode::Fetch {
            if self.chain_inner == Some(node) {
                self.chain_inner = None;
            }
            return match self.memo.held.iter().find(|(held, _)| *held == node) {
                Some(&(_, value)) => Ok(value),
                None => Err(self.fatal(
                    "InternalError: a `??=` wrote through an expression its read never compiled",
                )),
            };
        }
        self.memo.mode = MemoMode::None;
        let value = self.expr(node);
        self.memo.mode = MemoMode::Compile;
        let value = value?;
        let held = match value {
            Where::Returned(at) => {
                let out = self.temporary();
                self.copy_tmp(OperandKind::Var, at, out);
                Where::Returned(out)
            }
            Where::Temporary(at) => {
                let out = self.temporary();
                self.copy_tmp(OperandKind::Tmp, at, out);
                Where::Temporary(out)
            }
            Where::Constant(_) | Where::Cv(_) => value,
        };
        self.memo.held.push((node, held));
        Ok(value)
    }

    /// `ZEND_COPY_TMP`: `op1` copied into `out` of the same kind, one more
    /// owner of what it holds.
    fn copy_tmp(&mut self, kind: OperandKind, at: u32, out: u32) {
        self.push(Opline {
            opcode: Opcode::CopyTmp,
            op1_type: kind,
            op1: at,
            out_type: kind,
            out,
            ..Opline::default()
        });
    }

    /// `zend_compile_assign_coalesce` (`Z/Zend/zend_compile.c:10597`),
    /// `place ??= value`: `zend_ensure_writable_variable` and the
    /// `$this` check ([`Self::target`]), then the place compiled under
    /// `BP_VAR_IS` with every expression on the way held
    /// ([`Self::silent_read`] under [`MemoMode::Compile`]), `COALESCE`
    /// over what the read left into a temporary with the jump target
    /// learnt after the write, the value compiled as any other (with
    /// `zend_compile_expr_with_potential_assign_to_self` over a
    /// dimension), then the place compiled again under `BP_VAR_W` from
    /// what was held and its last fetch the write
    /// ([`Self::assign_to`] with the value as a [`Source::Node`]), and
    /// `QM_ASSIGN` of what the write left into the coalesce's temporary.
    /// Where a held expression left a temporary or a `VAR`, the write
    /// jumps over a `FREE` of each copy, which the path that found a
    /// value lands on (`need_frees`).
    ///
    /// The place is read once however many oplines the two fetches take,
    /// which is what the form is for, and a place nothing wrote raises
    /// nothing, since the read is `BP_VAR_IS`:
    ///
    /// ```php
    /// function f() { echo "f"; return 2; }
    /// $a = 1; $n = null;
    /// var_dump($a ??= f());
    /// var_dump($n ??= f());
    /// var_dump($undefined ??= 3, $undefined);
    /// function k() { echo "k"; return 'k'; }
    /// $arr = [];
    /// var_dump($arr[k()] ??= 1, $arr[k()] ??= 2, $arr['x']['y'] ??= f());
    /// $o = new stdClass; $o->q = new stdClass;
    /// var_dump($o->p ??= 3, $o->p ??= 4, $o->q->r ??= 5);
    /// class C { public static $s; }
    /// var_dump(C::$s ??= 6, C::$s ??= 7);
    /// $GLOBALS['x'] ??= 'x';
    /// $nm = 'y';
    /// $$nm ??= 1;
    /// $$nm ??= 2;
    /// ${'z' . 'z'} ??= 3;
    /// $_SERVER ??= f();
    /// $_GET['g'] ??= 4;
    /// $v = ($GLOBALS['w'] ??= 'w');
    /// var_dump($GLOBALS['x'], $y, $zz, is_array($_SERVER), $_GET['g'], $v, $w);
    /// ```
    /// ```expect
    /// int(1)
    /// fint(2)
    /// int(3)
    /// int(3)
    /// kkfint(1)
    /// int(1)
    /// int(2)
    /// int(3)
    /// int(3)
    /// int(5)
    /// int(6)
    /// int(6)
    /// string(1) "x"
    /// int(1)
    /// int(3)
    /// bool(true)
    /// int(4)
    /// string(1) "w"
    /// string(1) "w"
    /// ```
    pub(super) fn assign_coalesce(
        &mut self,
        var: tree::Expr,
        value: tree::Expr,
    ) -> Result<Where, CompileError> {
        let dim = match self.target(var)? {
            Target::List(_) => {
                return Err(self.compile_error("Cannot use list() as standalone expression"));
            }
            Target::Dim(_) => true,
            _ => false,
        };
        let outer = std::mem::replace(
            &mut self.memo,
            Memo {
                mode: MemoMode::Compile,
                held: Vec::new(),
            },
        );
        let written = self.coalesce_through(var, dim, value);
        let memo = std::mem::replace(&mut self.memo, outer);
        let (out, opnum) = written?;
        let frees: Vec<(OperandKind, u32)> = memo
            .held
            .into_iter()
            .filter_map(|(_, held)| match held {
                Where::Returned(at) => Some((OperandKind::Var, at)),
                Where::Temporary(at) => Some((OperandKind::Tmp, at)),
                Where::Constant(_) | Where::Cv(_) => None,
            })
            .collect();
        if frees.is_empty() {
            self.update_jump_target_to_next(opnum);
        } else {
            let jump = self.jump_forward();
            self.update_jump_target_to_next(opnum);
            for (kind, at) in frees {
                self.free(kind, at);
            }
            self.update_jump_target_to_next(jump);
        }
        Ok(Where::Temporary(out))
    }

    /// [`Self::assign_coalesce`] between the memo's opening and its
    /// `FREE`s: the read, the `COALESCE`, the value, the write and the
    /// `QM_ASSIGN`; the coalesce's temporary and the `COALESCE`'s number.
    fn coalesce_through(
        &mut self,
        var: tree::Expr,
        dim: bool,
        value: tree::Expr,
    ) -> Result<(u32, u32), CompileError> {
        let read = self.silent_read(var)?;
        let (op1_type, op1) = self.operand(read);
        let out = self.temporary();
        let opnum = self.push(Opline {
            opcode: Opcode::Coalesce,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        self.memo.mode = MemoMode::None;
        let value = if dim {
            self.assigned_value(var, value)?
        } else {
            self.expr(value)?
        };
        self.memo.mode = MemoMode::Fetch;
        let assigned = self.assign_to(var, Source::Node(value))?;
        self.memo.mode = MemoMode::None;
        let (op1_type, op1) = self.operand(assigned);
        self.push(Opline {
            opcode: Opcode::QmAssign,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        Ok((out, opnum))
    }
}
