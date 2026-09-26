//! `$value |> callable`, the pipe (`zend_compile_pipe`,
//! `Z/Zend/zend_compile.c:6491`): the left side compiled to a value
//! first, then a call built over it, `f(...)`, `C::m(...)` and `$o->m(...)`
//! written as the call itself with the value its one argument, and any
//! other callable compiled and called.
//!
//! php hands the value to the call it builds as a `ZEND_AST_ZNODE`, a node
//! whose compile is the value the compiler already holds. Here that is
//! [`Emit::znodes`]: the left side's node beside the value, which
//! [`Emit::expr`] answers for the node and every reader of an argument's
//! shape takes as no call, no variable and no constant to fold.

use php_syntax::tree;
use php_unit::{Opcode, OperandKind, Opline};

use super::args::Args;
use super::{Emit, Where};
use crate::CompileError;

impl Emit<'_> {
    /// `zend_compile_pipe` (`Z/Zend/zend_compile.c:6491`). An arrow
    /// function on the right must stand in parentheses. The left side is
    /// compiled, and a CV or a `VAR` it left is copied into a temporary by
    /// `QM_ASSIGN`, so a callee that takes its parameter by reference is
    /// handed no variable to bind. Then the call: a name other than
    /// `assert`, a class's method or an object's method written with
    /// `(...)` is called with the value as its one argument
    /// (`zend_is_pipe_optimizable_callable_name`), and anything else is
    /// compiled where it stands and called as a callable is,
    /// `INIT_DYNAMIC_CALL`.
    ///
    /// ```php
    /// function inc($x) { return $x + 1; }
    /// class M { static function twice($x) { return $x * 2; } function neg($x) { return -$x; } }
    /// $m = new M;
    /// $f = 'inc';
    /// var_dump(1 |> inc(...) |> M::twice(...) |> $m->neg(...) |> $f |> (fn($x) => $x - 1));
    /// var_dump("abc" |> strlen(...), [3, 1] |> count(...), 2 |> 'M::twice', "ab" |> 'strtoupper');
    /// function r(&$x) { $x = 9; return $x; }
    /// $v = 1;
    /// try { $v |> r(...); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(-4)
    /// int(3)
    /// int(2)
    /// int(4)
    /// string(2) "AB"
    /// r(): Argument #1 ($x) could not be passed by reference
    /// ```
    ///
    /// ```php
    /// var_dump(1 |> fn($x) => $x);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Arrow functions on the right hand side of |> must be parenthesized in %s on line 2
    /// ```
    pub(super) fn pipe(
        &mut self,
        operand: tree::Expr,
        callable: tree::Expr,
    ) -> Result<Where, CompileError> {
        if matches!(callable, tree::Expr::ArrowFnExpr(_)) {
            return Err(self.compile_error(
                "Arrow functions on the right hand side of |> must be parenthesized",
            ));
        }
        let value = self.expr(operand)?;
        let value = match value {
            Where::Cv(_) | Where::Returned(_) => {
                let (op1_type, op1) = self.operand(value);
                let out = self.temporary();
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
            Where::Temporary(_) | Where::Constant(_) => value,
        };
        let operand = self.peel(operand);
        let held = self.znodes.len();
        self.znodes.push((operand, value));
        let called = self.pipe_call(operand, callable);
        self.znodes.truncate(held);
        called
    }

    /// The call [`Self::pipe`] builds over `operand`, the node standing
    /// for the value it holds.
    fn pipe_call(
        &mut self,
        operand: tree::Expr,
        callable: tree::Expr,
    ) -> Result<Where, CompileError> {
        let line = self.line;
        let arguments = Args::of(&[operand], line);
        let callable = self.peel(callable);
        if let tree::Expr::CallExpr(id) = callable
            && matches!(
                self.arguments(self.parsed.tree[id].args, line)?,
                Args::CallableConvert
            )
        {
            match self.peel(self.parsed.tree[id].callee) {
                tree::Expr::NameRef(token) => {
                    let name = self.text(token).to_vec();
                    let bare = name.strip_prefix(b"\\").unwrap_or(&name);
                    if !bare.eq_ignore_ascii_case(b"assert") {
                        return self.call(&name, &arguments);
                    }
                }
                callee @ (tree::Expr::SimpleVarExpr(_) | tree::Expr::CallExpr(_)) => {
                    return self.call_var(callee, &arguments);
                }
                tree::Expr::ScopedAccessExpr(scoped) => {
                    let class = self.class_spec(self.parsed.tree[scoped].scope);
                    let method = self.scoped_method_of(scoped)?;
                    return self.static_call(class, method, &arguments);
                }
                tree::Expr::MemberAccessExpr(access) if !self.is_nullsafe(access) => {
                    let method = self.member_of(access)?;
                    let object = self.parsed.tree[access].obj;
                    return self.method_call(object, method, &arguments, false);
                }
                _ => {}
            }
        }
        // `$foo |> $expr` is `($expr)($foo)`: the callable compiled, and
        // called as the value it left.
        let callee = self.expr(callable)?;
        self.znodes.push((callable, callee));
        self.call_var(callable, &arguments)
    }

    /// The value php's `ZEND_AST_ZNODE` holds for `expr`, where
    /// [`Self::pipe`] stood one in for it.
    pub(super) fn znode(&self, expr: tree::Expr) -> Option<Where> {
        let expr = self.peel(expr);
        self.znodes
            .iter()
            .rev()
            .find(|(node, _)| *node == expr)
            .map(|&(_, value)| value)
    }
}
