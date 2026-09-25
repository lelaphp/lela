//! `?->`, php's short circuit: `zend_emit_jmp_null` before each nullsafe
//! member and `zend_short_circuiting_commit` where the chain ends
//! (`Z/Zend/zend_compile.c:2535-2585`). One mechanism serves every place
//! a chain is read: a property fetch, a method call, a chain of
//! dimensions over either, `isset` and `empty`, and a string's
//! `"$a?->b"`.
//!
//! Each `JMP_NULL` is pushed on [`Emit::short_circuits`], php's
//! `CG(short_circuiting_opnums)`. [`Emit::expr`] takes a checkpoint of
//! the stack before it compiles a node and, where the node is one a
//! chain can end in and no link above it marked it inner
//! ([`Emit::chain_link`], `zend_short_circuiting_mark_inner`), aims every
//! `JMP_NULL` pushed since at the opline after the chain and gives it the
//! chain's value, so a null receiver leaves `null` where the whole
//! chain's value would stand, and the arguments of a call after it are
//! never compiled into a path that runs.

use php_syntax::tree;
use php_unit::{Opcode, OperandKind, Opline};

use super::{Emit, Where};

/// `ZEND_SHORT_CIRCUITING_CHAIN_EXPR`, `_ISSET` and `_EMPTY`
/// (`Z/Zend/zend_compile.h:438`): what a `JMP_NULL` leaves in the
/// chain's value where it jumps, `null`, `false` or `true`.
#[derive(Clone, Copy)]
pub(super) enum ChainEnd {
    Expr = 0,
    Isset = 1,
    Empty = 2,
}

/// `ZEND_JMP_NULL_BP_VAR_IS` (`Z/Zend/zend_compile.h:444`): the chain is
/// read silently, so an undefined variable under the `?->` warns of
/// nothing.
const JMP_NULL_BP_VAR_IS: u32 = 4;

impl Emit<'_> {
    /// `zend_short_circuiting_mark_inner` and the compile after it: the
    /// object under a link of a chain, whose own end the link's commits.
    pub(super) fn chain_link(&mut self, object: tree::Expr) -> Result<Where, crate::CompileError> {
        self.chain_inner = Some(self.peel(object));
        self.expr(object)
    }

    /// `zend_emit_jmp_null` (`Z/Zend/zend_compile.c:2574`): `JMP_NULL` over
    /// the receiver, its target and its `result` left for the commit, and
    /// `ZEND_JMP_NULL_BP_VAR_IS` where the chain is read silently.
    pub(super) fn jmp_null(&mut self, (op1_type, op1): (OperandKind, u32), silent: bool) {
        let at = self.push(Opline {
            opcode: Opcode::JmpNull,
            op1_type,
            op1,
            extended_value: if silent { JMP_NULL_BP_VAR_IS } else { 0 },
            ..Opline::default()
        });
        self.short_circuits.push(at);
    }

    /// `zend_short_circuiting_commit` (`Z/Zend/zend_compile.c:2546`):
    /// every `JMP_NULL` pushed since `checkpoint` aimed at the next
    /// opline, writing the chain's value into its `result`, and marked with how the chain
    /// ends. The whole chain is skipped, the links after the `?->` and
    /// the arguments of every call among them, under `??` and over a
    /// dimension as over a property:
    ///
    /// ```php
    /// $a = null;
    /// var_dump($a?->b->c ?? 'd', $a?->b['k']);
    /// function f() { echo "f\n"; return 1; }
    /// $a?->m(f())->n(f());
    /// $o = new stdClass;
    /// $o->p = ['k' => 'v'];
    /// echo $o?->p['k'], "\n";
    /// ```
    /// ```expect
    /// string(1) "d"
    /// NULL
    /// v
    /// ```
    ///
    /// A chain is never a place to write:
    ///
    /// ```php
    /// $a?->b = 1;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Can't use nullsafe operator in write context in %s on line 2
    /// ```
    pub(super) fn short_circuit_commit(&mut self, checkpoint: usize, value: Where, end: ChainEnd) {
        let (out_type, out) = match value {
            Where::Temporary(at) => (OperandKind::Tmp, at),
            Where::Returned(at) => (OperandKind::Var, at),
            Where::Cv(at) => (OperandKind::Cv, at),
            Where::Constant(_) => (OperandKind::Unused, 0),
        };
        let next = self.next();
        while self.short_circuits.len() > checkpoint {
            let Some(at) = self.short_circuits.pop() else {
                break;
            };
            if let Some(opline) = self.oplines.get_mut(at as usize) {
                opline.op2 = next;
                opline.out_type = out_type;
                opline.out = out;
                opline.extended_value |= end as u32;
            }
        }
    }

    /// `zend_ast_kind_is_short_circuited` and the two `zend_short_circuiting_commit`
    /// adds: how a chain ending at `expr` ends, where it is one of the
    /// kinds a chain can end in.
    pub(super) fn chain_end(&self, expr: tree::Expr) -> Option<ChainEnd> {
        let tree = &self.parsed.tree;
        match expr {
            tree::Expr::SubscriptExpr(_) | tree::Expr::MemberAccessExpr(_) => Some(ChainEnd::Expr),
            tree::Expr::ScopedAccessExpr(id) => matches!(
                tree[id].member,
                Some(tree::ScopedSelector::SimpleVarExpr(_))
            )
            .then_some(ChainEnd::Expr),
            tree::Expr::CallExpr(id) => matches!(
                self.peel(tree[id].callee),
                tree::Expr::MemberAccessExpr(_) | tree::Expr::ScopedAccessExpr(_)
            )
            .then_some(ChainEnd::Expr),
            tree::Expr::IssetExpr(_) => Some(ChainEnd::Isset),
            tree::Expr::EmptyExpr(_) => Some(ChainEnd::Empty),
            _ => None,
        }
    }
}
