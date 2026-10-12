//! `$value |> callable`, the pipe (`zend_compile_pipe`,
//! `Z/Zend/zend_compile.c:6491`): the left side compiled to a value
//! first, then a call built over it, `f(...)`, `C::m(...)` and `$o->m(...)`
//! written as the call itself with the value its one argument, and any
//! other callable compiled and called.
//!
//! php hands the value to the call it builds as a `ZEND_AST_ZNODE`, a node
//! whose compile is the value the compiler already holds. Here that is
//! the argument's [`Source::Node`], which every reader of an argument's
//! shape takes as no call, no variable and no constant to fold.

use php_syntax::tree;
use php_unit::{Opcode, OperandKind, Opline};

use super::args::{Arg, Args};
use super::{ClassSpec, Emit, Source, Where};
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
        let compiled = self.expr(operand)?;
        let value = match compiled {
            Where::Cv(_) | Where::Returned(_) => {
                let (op1_type, op1) = self.operand(compiled);
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
            Where::Temporary(_) | Where::Constant(_) => compiled,
        };
        self.pipe_call(value, callable, compiled)
    }

    /// The call [`Self::pipe`] builds over `value`, the one argument it
    /// sends, and before it the `EXT_STMT` over `compiled`, what the left
    /// side left before any `QM_ASSIGN` copied it
    /// (`zend_do_extended_stmt(&operand_result)`), after the callable
    /// where one is compiled ([`super::extended`]).
    fn pipe_call(
        &mut self,
        value: Where,
        callable: tree::Expr,
        compiled: Where,
    ) -> Result<Where, CompileError> {
        let line = self.line;
        let arguments = Args::Written {
            list: vec![Arg::Positional(Source::Node(value))],
            line,
        };
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
                        self.ext_stmt_over(Some(compiled));
                        return self.call(&name, &arguments, super::Fetch::Read);
                    }
                }
                callee @ (tree::Expr::SimpleVarExpr(_) | tree::Expr::CallExpr(_)) => {
                    self.ext_stmt_over(Some(compiled));
                    return self.call_var(Source::Expr(callee), &arguments);
                }
                tree::Expr::ScopedAccessExpr(scoped) => {
                    let class = ClassSpec::of(self.parsed.tree[scoped].scope);
                    let method = self.scoped_method_of(scoped)?;
                    self.ext_stmt_over(Some(compiled));
                    return self.static_call(class, method, &arguments);
                }
                tree::Expr::MemberAccessExpr(access) if !self.is_nullsafe(access) => {
                    let method = self.member_of(access)?;
                    let object = self.parsed.tree[access].obj;
                    self.ext_stmt_over(Some(compiled));
                    return self.method_call(object, method, &arguments, false);
                }
                _ => {}
            }
        }
        // `$foo |> $expr` is `($expr)($foo)`: the callable compiled, and
        // called as the value it left.
        let callee = self.expr(callable)?;
        self.ext_stmt_over(Some(compiled));
        self.call_var(Source::Node(callee), &arguments)
    }
}
