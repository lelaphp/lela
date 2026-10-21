//! php's constant-expression evaluator, `zend_eval_const_expr`
//! (`Z/Zend/zend_compile.c:12080`), as the tree it leaves behind.
//!
//! php evaluates in place. A node it folds becomes a `ZEND_AST_ZVAL`; a
//! `??` whose left side folds and a conditional whose condition folds
//! become the side they pick, folded or not; and every child of a node it
//! does not fold is evaluated all the same, the right side of an operator
//! whose left one did not fold included. The compile that follows reads
//! the tree as the evaluator left it, which is where the rewrite shows:
//! `static $v = null ?? A[1] ?? 1;` compiles `A[1] ?? 1`, and
//! `[true ? $a : 2]` is an `INIT_ARRAY` over `$a`.
//!
//! The tree here is immutable (ADR-0003), so the rewrite is held beside
//! it, one [`Rewrite`] per node php's would have replaced. The fold under
//! [`Fold::EvalConstExpr`] writes it ([`Emit::constant_expr`]), and every
//! compile and fold of a node after it reads it: [`Emit::peel`] steps to
//! the side a `??` or a conditional was replaced by, and a node replaced
//! by its value is that value wherever it is read. Each node is
//! evaluated once and compiled once, as php's is, so a warning the fold
//! raised stands once.

use std::collections::HashMap;

use php_heap::{Zval, ZvalType};
use php_syntax::tree::{self, NodeId};
use php_unit::Level;

use super::{Emit, Fold, truth};
use crate::{CompileError, Raise};

/// What a fold of a node answers where it has no value for it: the node
/// is no constant, php's `zend_eval_const_expr` leaving it standing; it
/// is one this compiler cannot read yet ([`crate::Raise::Unread`]); or
/// the compile refused. The first is the common answer, every variable
/// and call an array expression holds, so it is a state of its own and no
/// refusal: it allocates nothing, where a [`CompileError`] is a box
/// (#3208). `why` is the sentence a compile that needed the constant
/// refuses with ([`Emit::constant_value`]).
///
/// The first two leave the node unfolded, for the compile to meet as it
/// stands; a refusal is the answer, whatever its level, since php's
/// compile stops at it. A refusal is sorted into one of the two once,
/// where it becomes an answer of a fold ([`From`]), and every fold
/// matches the variant.
#[derive(Debug)]
pub(crate) enum Unfolded {
    NoConstant(&'static str),
    Unread(CompileError),
    Refused(CompileError),
}

impl From<CompileError> for Unfolded {
    fn from(refused: CompileError) -> Unfolded {
        match refused.raise {
            Raise::Unread => Unfolded::Unread(refused),
            Raise::Fatal | Raise::Thrown => Unfolded::Refused(refused),
        }
    }
}

impl Unfolded {
    /// What a node left unfolded because a child it waits on was answers:
    /// the child's own answer where this compiler cannot read the child,
    /// so the innermost cause is the one a compile that needs the value
    /// refuses with, and `why` where the child is no constant.
    fn of_node(self, why: &'static str) -> Unfolded {
        match self {
            Unfolded::NoConstant(_) => Unfolded::NoConstant(why),
            Unfolded::Unread(_) | Unfolded::Refused(_) => self,
        }
    }
}

/// A child's fold as the node that waits on it reads it: the value, or
/// the child left unfolded with why ([`Unfolded::NoConstant`] or
/// [`Unfolded::Unread`]). A refusal is not one: [`Emit::child`] answers
/// it as the node's own.
enum Child {
    Folded(Zval),
    Unfolded(Unfolded),
}

/// What `zend_eval_const_expr` replaced a node of php's tree with: the
/// `ZEND_AST_ZVAL` of the value it folded to, or another node, the side
/// a `??` or a conditional picked, which did not fold.
#[derive(Clone, Copy)]
pub(super) enum Rewrite {
    Value(Zval),
    Node(tree::Expr),
}

/// The nodes `zend_eval_const_expr` has replaced, by the node; the
/// table is made with the first, so a compile that folds no `??` or
/// conditional makes none.
#[derive(Default)]
pub(super) struct Rewrites(Option<HashMap<tree::Expr, Rewrite>>);

impl Rewrites {
    /// What the node was replaced with, if anything.
    pub(super) fn get(&self, expr: tree::Expr) -> Option<Rewrite> {
        self.0.as_ref()?.get(&expr).copied()
    }

    /// The node replaced with `rewrite`.
    fn record(&mut self, expr: tree::Expr, rewrite: Rewrite) {
        self.0
            .get_or_insert_with(HashMap::new)
            .insert(expr, rewrite);
    }
}

impl Emit<'_> {
    /// [`Self::constant_expr`] where a compile needs the value: a node
    /// that is no constant refused at the line the compile stands on, as
    /// a sentence of this compiler's own ([`Emit::fatal`]).
    pub(crate) fn constant_value(&mut self, expr: tree::Expr) -> Result<Zval, CompileError> {
        self.constant_expr(expr).map_err(|unfolded| match unfolded {
            Unfolded::NoConstant(why) => self.fatal(why),
            Unfolded::Unread(refused) | Unfolded::Refused(refused) => refused,
        })
    }

    /// The node where it stands after `zend_eval_const_expr`: the side a
    /// `??` or a conditional was replaced by, followed to the node that
    /// replaced nothing further. A node replaced by a value stands as it
    /// is; [`Self::rewritten_value`] reads the value.
    pub(super) fn replaced(&self, mut expr: tree::Expr) -> tree::Expr {
        while let Some(Rewrite::Node(node)) = self.rewrites.get(expr) {
            expr = super::peel(self.parsed, node);
        }
        expr
    }

    /// The value `zend_eval_const_expr` replaced the node with, if it did.
    pub(super) fn rewritten_value(&self, expr: tree::Expr) -> Option<Zval> {
        match self.rewrites.get(expr) {
            Some(Rewrite::Value(value)) => Some(value),
            _ => None,
        }
    }

    /// What [`Self::constant_expr`] answers for a node already evaluated:
    /// its value, or the fold of the node that replaced it, and nothing
    /// for a node the evaluator left standing.
    pub(super) fn evaluated(&mut self, expr: tree::Expr) -> Option<Result<Zval, Unfolded>> {
        match self.rewrites.get(expr)? {
            Rewrite::Value(value) => Some(Ok(value)),
            Rewrite::Node(node) => Some(self.constant_expr(node)),
        }
    }

    /// The fold [`Self::constant_expr`] made of a node, kept where it is
    /// `zend_eval_const_expr`'s: the node is replaced by its value.
    pub(super) fn record_fold(&mut self, expr: tree::Expr, folded: &Result<Zval, Unfolded>) {
        // Only `zend_eval_const_expr` replaces a node, and a value php's
        // parser made, a number or a string written out, is the same
        // `ZEND_AST_ZVAL` before the evaluator and after it, so it is
        // kept nowhere and keeps the line it was written at: asked last,
        // since a compile's fold, which records nothing, would read the
        // literal a second time to learn it.
        if self.fold == Fold::EvalConstExpr
            && let Ok(value) = folded
            && super::written(self.parsed, expr).is_none()
        {
            self.rewrites.record(expr, Rewrite::Value(*value));
        }
    }

    /// Both children of a node folded, left then right: under
    /// `zend_eval_const_expr` the right one even where the left one did
    /// not fold, since php evaluates every child before it reads either,
    /// and under a compile's fold the right one only once the left one
    /// has, as `zend_compile_expr` would have emitted the left one's
    /// oplines. A refusal from either is the answer, the left one's
    /// first; a side left unfolded leaves the node unfolded.
    pub(super) fn constant_pair(
        &mut self,
        left: tree::Expr,
        right: tree::Expr,
    ) -> Result<(Zval, Zval), Unfolded> {
        if self.fold == Fold::Compile {
            return Ok((self.constant_expr(left)?, self.constant_expr(right)?));
        }
        let left = match self.constant_expr(left) {
            Err(refused @ Unfolded::Refused(_)) => return Err(refused),
            left => left,
        };
        match (left, self.constant_expr(right)) {
            (_, Err(refused @ Unfolded::Refused(_))) => Err(refused),
            (Ok(left), Ok(right)) => Ok((left, right)),
            (Err(unfolded), _) | (_, Err(unfolded)) => Err(unfolded),
        }
    }

    /// [`Self::constant_expr`] over a child the node waits on: what it
    /// folded to, or why it did not, and a refusal as the node's answer.
    fn child(&mut self, expr: tree::Expr) -> Result<Child, Unfolded> {
        match self.constant_expr(expr) {
            Ok(value) => Ok(Child::Folded(value)),
            Err(refused @ Unfolded::Refused(_)) => Err(refused),
            Err(unfolded) => Ok(Child::Unfolded(unfolded)),
        }
    }

    /// `zend_eval_const_expr` over a child whose fold the node does not
    /// wait on, for the rewrite alone: what it folds to is kept, and a
    /// node it does not fold is no refusal of the parent's; a compile
    /// error is.
    pub(super) fn evaluate(&mut self, expr: tree::Expr) -> Result<(), CompileError> {
        self.constant_of(expr).map(|_| ())
    }

    /// `zend_eval_const_expr`'s `ZEND_AST_COALESCE` arm
    /// (`Z/Zend/zend_compile.c:12167`): the left side folded; one that
    /// does not fold leaves the `??` standing, the right side evaluated
    /// for its own rewrites; a `null` replaces the `??` with the right
    /// side, its fold or the node; anything else with the left side's
    /// value. Under a compile's fold a `??` folds to nothing, since
    /// `zend_compile_coalesce` emits its two oplines whatever stands on
    /// either side.
    ///
    /// ```php
    /// const A = [1 => [[]]];
    /// function f() { static $v = null ?? A[1]['undefined'] ?? 1; return $v; }
    /// function g() { static $v = null ?? 'n'; return $v; }
    /// var_dump(f(), g());
    /// ```
    /// ```expect
    /// int(1)
    /// string(1) "n"
    /// ```
    pub(super) fn coalesce_constant(
        &mut self,
        id: NodeId<tree::BinaryExpr>,
        left: tree::Expr,
        right: tree::Expr,
    ) -> Result<Zval, Unfolded> {
        if self.fold == Fold::Compile {
            return Err(Unfolded::NoConstant("a `??` is no constant expression"));
        }
        let value = match self.child(left)? {
            Child::Folded(value) => value,
            Child::Unfolded(unfolded) => {
                self.evaluate(right)?;
                return Err(unfolded.of_node("a `??` over a side that does not fold"));
            }
        };
        if value.kind() != ZvalType::Null {
            return Ok(value);
        }
        self.replaced_by(tree::Expr::BinaryExpr(id), right)
    }

    /// `zend_eval_const_expr`'s `ZEND_AST_CONDITIONAL` arm
    /// (`Z/Zend/zend_compile.c:12189`): the condition folded, then the
    /// branch `zend_is_true` picks in the conditional's place, the
    /// condition itself for `a ?: b`, folded where it folds and the node
    /// where it does not. A condition that does not fold leaves the
    /// conditional standing, both branches evaluated for their own
    /// rewrites.
    ///
    /// ```php
    /// function f($a) { static $v = [true ? $a : 2, 1 ? 2 : 3]; return $v; }
    /// var_dump(f(5));
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   int(5)
    ///   [1]=>
    ///   int(2)
    /// }
    /// ```
    ///
    /// The warning a condition of `NAN` raises is raised once, where the
    /// evaluator reads it, and the branch it picked compiles without it:
    ///
    /// ```php
    /// function f($a) { static $v = NAN ? $a : 1; return $v; }
    /// var_dump(f(2), [NAN ? $u : 1]);
    /// ```
    /// ```expect
    /// Warning: unexpected NAN value was coerced to bool in %s on line 2
    ///
    /// Warning: unexpected NAN value was coerced to bool in %s on line 3
    ///
    /// Warning: Undefined variable $u in %s on line 3
    /// int(2)
    /// array(1) {
    ///   [0]=>
    ///   NULL
    /// }
    /// ```
    pub(super) fn ternary_constant(
        &mut self,
        id: NodeId<tree::TernaryExpr>,
    ) -> Result<Zval, Unfolded> {
        let node = &self.parsed.tree[id];
        let (cond, then, otherwise) = (node.cond, node.then, node.else_branch);
        let otherwise = self.required(otherwise, "a conditional")?;
        let value = match self.child(cond)? {
            Child::Folded(value) => value,
            Child::Unfolded(unfolded) => {
                if let Some(then) = then {
                    self.evaluate(then)?;
                }
                self.evaluate(otherwise)?;
                return Err(unfolded.of_node("a conditional over a condition that does not fold"));
            }
        };
        if value.as_double().is_some_and(f64::is_nan) {
            self.raise(Level::Warning, "unexpected NAN value was coerced to bool");
        }
        match (truth(value), then) {
            (true, None) => Ok(value),
            (true, Some(then)) => self.replaced_by(tree::Expr::TernaryExpr(id), then),
            (false, _) => self.replaced_by(tree::Expr::TernaryExpr(id), otherwise),
        }
    }

    /// A node php replaces with one of its children, `*ast_ptr =
    /// child`: the child's fold where it folds, and otherwise the node
    /// replaced by the child, which the compile reads in its place.
    fn replaced_by(&mut self, node: tree::Expr, child: tree::Expr) -> Result<Zval, Unfolded> {
        match self.child(child)? {
            Child::Folded(value) => Ok(value),
            Child::Unfolded(unfolded) => {
                self.rewrites.record(node, Rewrite::Node(child));
                Err(unfolded.of_node("a node replaced by a side that does not fold"))
            }
        }
    }

    /// `zend_eval_const_expr`'s `ZEND_AST_AND` and `ZEND_AST_OR` arm
    /// (`Z/Zend/zend_compile.c:12116`): both sides evaluated, then where
    /// the left side's truth is the operator's answer on its own, that
    /// bool; else the right side's truth, since the left one has settled
    /// nothing and the operator answers what the right one is. A side
    /// that does not fold where it is read leaves the whole expression
    /// unfolded, which is php's `return` out of the arm. Under a
    /// compile's fold the right side is not read where the left one
    /// settles it, as `zend_compile_short_circuiting` does not compile
    /// it.
    pub(super) fn short_circuit_fold(
        &mut self,
        or: bool,
        left: tree::Expr,
        right: tree::Expr,
    ) -> Result<Zval, Unfolded> {
        if self.fold == Fold::EvalConstExpr {
            let left = self.child(left)?;
            let right = self.child(right)?;
            return match (left, right) {
                (Child::Folded(held), _) if truth(held) == or => Ok(Zval::bool(truth(held))),
                (Child::Folded(_), Child::Folded(right)) => Ok(Zval::bool(truth(right))),
                (Child::Unfolded(unfolded), _) | (Child::Folded(_), Child::Unfolded(unfolded)) => {
                    Err(unfolded.of_node("a short circuit over a side that does not fold"))
                }
            };
        }
        let held = truth(self.constant_expr(left)?);
        if held == or {
            return Ok(Zval::bool(held));
        }
        Ok(Zval::bool(truth(self.constant_expr(right)?)))
    }

    /// `zend_eval_const_expr`'s `ZEND_AST_NEW` arm and its
    /// `ZEND_AST_ARG_LIST` and `ZEND_AST_NAMED_ARG` ones: the class, where
    /// it is an expression, and each argument but a `...` evaluated for
    /// their rewrites, and the `new` itself folds to nothing.
    pub(super) fn new_constant(&mut self, id: NodeId<tree::NewExpr>) -> Result<Zval, Unfolded> {
        let node = &self.parsed.tree[id];
        let (ctor, args) = (node.ctor, node.args);
        if self.fold == Fold::EvalConstExpr {
            if let Some(ctor) = ctor
                && !matches!(ctor, tree::Expr::NameRef(_))
            {
                self.evaluate(ctor)?;
            }
            if let Some(args) = args {
                for arg in self.list(self.parsed.tree[args].args).iter().copied() {
                    match arg {
                        tree::Arg::Expr(value) => self.evaluate(value)?,
                        tree::Arg::NamedArg(named) => {
                            if let Some(value) = self.parsed.tree[named].value {
                                self.evaluate(value)?;
                            }
                        }
                        tree::Arg::SpreadArg(_) | tree::Arg::VariadicPlaceholder(_) => {}
                    }
                }
            }
        }
        Err(Unfolded::NoConstant("a call is no constant expression"))
    }

    /// `zend_eval_const_expr`'s `ZEND_AST_PROP` and
    /// `ZEND_AST_NULLSAFE_PROP` arm: the object and a name in braces
    /// evaluated for their rewrites, and the fetch folds to nothing.
    pub(super) fn member_constant(
        &mut self,
        id: NodeId<tree::MemberAccessExpr>,
    ) -> Result<Zval, Unfolded> {
        if self.fold == Fold::EvalConstExpr {
            let node = &self.parsed.tree[id];
            let (object, member) = (node.obj, node.member);
            self.evaluate(object)?;
            if let Some(tree::MemberSelector::BracedMember(braced)) = member
                && let Some(inner) = self.parsed.tree[braced].inner
            {
                self.evaluate(inner)?;
            }
        }
        Err(Unfolded::NoConstant(
            "a property fetch is read as the program serves",
        ))
    }
}
