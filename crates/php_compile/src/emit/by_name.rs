//! A variable the program computes the name of, `$$x` and `${expr}`, and
//! one php never gives a CV, an auto-global: `zend_compile_simple_var`'s
//! arm past `zend_try_compile_cv`, `zend_compile_simple_var_no_cv`
//! (`Z/Zend/zend_compile.c:2915`), which fetches the variable by its
//! name out of a symbol table, `FETCH_R`, `FETCH_W`, `FETCH_RW`,
//! `FETCH_IS` or `FETCH_FUNC_ARG` by the fetch the place is compiled
//! under, and the
//! `global` statement's arm over such a name (`zend_compile_global_var`,
//! `:5441`).
//!
//! What the name is, is decided once ([`Named`]): a name written out is
//! the CV `zend_try_compile_cv` gives it, `$this` when it is `this`,
//! and a fetch by name when it is an auto-global's; any other name is
//! the value the program computes, fetched by name from the frame's
//! own table.

use php_heap::Zval;
use php_syntax::tree;
use php_unit::{Opcode, OperandKind, Opline};

use super::target::Source;
use super::{Base, Emit, FETCH_GLOBAL, FETCH_LOCAL, Fetch, Over, Where, is_auto_global};
use crate::CompileError;

/// `ZEND_FETCH_GLOBAL_LOCK` (`Z/Zend/zend_compile.h:1084`): the `FETCH_W`
/// of a `global` over a name no CV has, which leaves the name's operand
/// for the fetch after it to read and free.
const FETCH_GLOBAL_LOCK: u32 = 1 << 3;

/// What a variable's name makes of it, decided once: the CV
/// `zend_try_compile_cv` gives a name written out, `$this`
/// (`is_this_fetch`), or a fetch by the name's value from the table
/// `scope` names, `ZEND_FETCH_GLOBAL` for an auto-global and
/// `ZEND_FETCH_LOCAL` for every other.
pub(super) enum Named {
    Cv(Vec<u8>),
    This,
    Fetch { name: Where, scope: u32 },
}

impl Emit<'_> {
    /// `$name`, `$$x` or `${expr}` sorted as [`Named`], the name's own
    /// oplines laid down where it is not written out; `None` for what is
    /// no variable.
    pub(super) fn named(&mut self, expr: tree::Expr) -> Result<Option<Named>, CompileError> {
        let line = self.line_of(expr);
        let inner = match self.peel(expr) {
            tree::Expr::SimpleVarExpr(id) => {
                let Some(var) = self.parsed.tree[id].var else {
                    return Err(self.unread("a variable"));
                };
                let name = self.variable_name(var);
                return Ok(Some(self.named_by(name)?));
            }
            tree::Expr::VarVarExpr(id) => self.parsed.tree[id].inner,
            tree::Expr::DynamicVarExpr(id) => self.parsed.tree[id].inner,
            _ => return Ok(None),
        };
        let Some(inner) = inner else {
            return Err(super::walk::unread(line, "a variable"));
        };
        self.named_by_expr(inner).map(Some)
    }

    /// The name under `$` or `${…}`: one written out, folded, or the
    /// value the program computes.
    pub(super) fn named_by_expr(&mut self, inner: tree::Expr) -> Result<Named, CompileError> {
        if let Some(value) = self.written(inner)? {
            return self.named_by_value(value);
        }
        let name = match self.expr(inner)? {
            // `convert_to_string` over a name the compiler folded.
            Where::Constant(value) => return self.named_by_value(value),
            name => name,
        };
        Ok(Named::Fetch {
            name,
            scope: FETCH_LOCAL,
        })
    }

    /// A name known while the file compiles, as a string.
    fn named_by_value(&mut self, value: Zval) -> Result<Named, CompileError> {
        let value = self.stringified(value)?.unwrap_or(value);
        let Some(name) = value.as_str().map(|s| s.bytes().to_vec()) else {
            return Err(self.fatal("a variable's name that is no string"));
        };
        self.named_by(name)
    }

    /// A name written out: `this`, an auto-global's, or a CV's.
    fn named_by(&mut self, name: Vec<u8>) -> Result<Named, CompileError> {
        if name == b"this" {
            return Ok(Named::This);
        }
        // `is_globals_fetch`: `$GLOBALS` is `FETCH_GLOBALS`, no fetch by
        // name, and not read yet.
        if name == b"GLOBALS" {
            return Err(self.unread("a variable"));
        }
        if is_auto_global(&name) {
            let name = self.string(&name)?;
            return Ok(Named::Fetch {
                name: Where::Constant(name),
                scope: FETCH_GLOBAL,
            });
        }
        Ok(Named::Cv(name))
    }

    /// `zend_compile_simple_var_no_cv` with `delayed`: the name's operand
    /// settled and the `VAR` or temporary numbered now, the opline itself
    /// held back for [`Self::base_fetch`] to lay down, which is how php
    /// numbers `$$x = f()`'s fetch before the call's.
    pub(super) fn held_by_name(&mut self, name: Where, scope: u32) -> Base {
        let name = self.operand(name);
        let out = self.temporary();
        Base::ByName { name, scope, out }
    }

    /// The fetch by name [`Self::base_fetch`] lays down: `FETCH_R` or
    /// `FETCH_IS` into a temporary, `FETCH_W`, `FETCH_RW` or
    /// `FETCH_FUNC_ARG` into a `VAR` (`zend_adjust_for_fetch_type`).
    pub(super) fn lay_fetch_by_name(
        &mut self,
        (op1_type, op1): (OperandKind, u32),
        scope: u32,
        out: u32,
        fetch: Fetch,
    ) -> (OperandKind, u32) {
        let (opcode, out_type) = match fetch {
            Fetch::Read => (Opcode::FetchR, OperandKind::Tmp),
            Fetch::Write => (Opcode::FetchW, OperandKind::Var),
            Fetch::Rw => (Opcode::FetchRw, OperandKind::Var),
            Fetch::Is => (Opcode::FetchIs, OperandKind::Tmp),
            Fetch::FuncArg => (Opcode::FetchFuncArg, OperandKind::Var),
        };
        self.push(Opline {
            opcode,
            op1_type,
            op1,
            out_type,
            out,
            extended_value: scope,
            ..Opline::default()
        });
        (out_type, out)
    }

    /// [`Self::named`] under `fetch` as `zend_delayed_compile_var` holds
    /// it: a CV settled, `$this` its `FETCH_THIS`, and a fetch by name
    /// held back. `None` for what is no variable.
    pub(super) fn delayed_named(
        &mut self,
        expr: tree::Expr,
        fetch: Fetch,
    ) -> Result<Option<Base>, CompileError> {
        Ok(match self.named(expr)? {
            None => None,
            Some(Named::Cv(name)) => Some(Base::Settled((OperandKind::Cv, self.lookup_cv(&name)))),
            Some(Named::This) => {
                let kind = match fetch {
                    Fetch::Read | Fetch::Is => OperandKind::Tmp,
                    Fetch::Write | Fetch::Rw | Fetch::FuncArg => OperandKind::Var,
                };
                let this = self.fetch_this(kind);
                Some(Base::Settled(self.operand(this)))
            }
            Some(Named::Fetch { name, scope }) => Some(self.held_by_name(name, scope)),
        })
    }

    /// `zend_compile_assign`'s `ZEND_AST_VAR` arm over a variable the
    /// program names: the fetch held back (`FETCH_W`), the value, the fetch
    /// laid down, and `ASSIGN` over the `VAR` it left into a temporary.
    ///
    /// ```php
    /// $x = 'a';
    /// $$x = 1;
    /// ${'b' . 'c'}['k'] = 2;
    /// function name() { echo "name\n"; return 'd'; }
    /// function value() { echo "value\n"; return 3; }
    /// ${name()} = value();
    /// var_dump($a, $bc, $d);
    /// ```
    /// ```expect
    /// name
    /// value
    /// int(1)
    /// array(1) {
    ///   ["k"]=>
    ///   int(2)
    /// }
    /// int(3)
    /// ```
    pub(super) fn assign_by_name(
        &mut self,
        target: tree::Expr,
        value: Source,
    ) -> Result<Where, CompileError> {
        let line = self.line;
        let Some(held) = self.delayed_named(target, Fetch::Write)? else {
            return Err(self.fatal("an assignment by name to what is no variable"));
        };
        let value = self.source(value)?;
        let (op2_type, op2) = self.operand(value);
        let (op1_type, op1) = self.base_fetch(held, Fetch::Write, Over::Dim);
        let out = self.temporary();
        self.line = line;
        self.push(Opline {
            opcode: Opcode::Assign,
            op1_type,
            op1,
            op2_type,
            op2,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_compound_assign`'s `ZEND_AST_VAR` arm over a
    /// variable the program names: `FETCH_RW` held back, the value, and
    /// `ASSIGN_OP` over the `VAR`.
    pub(super) fn assign_op_by_name(
        &mut self,
        target: tree::Expr,
        opcode: u32,
        value: tree::Expr,
    ) -> Result<Where, CompileError> {
        let Some(held) = self.delayed_named(target, Fetch::Rw)? else {
            return Err(self.fatal("a compound assignment by name to what is no variable"));
        };
        let value = self.expr(value)?;
        let (op2_type, op2) = self.operand(value);
        let (op1_type, op1) = self.base_fetch(held, Fetch::Rw, Over::Dim);
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::AssignOp,
            op1_type,
            op1,
            op2_type,
            op2,
            out_type: OperandKind::Tmp,
            out,
            extended_value: opcode,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `++` or `--` over a variable the program names,
    /// `zend_compile_pre_incdec` and `zend_compile_post_incdec` over
    /// `zend_compile_var(…, BP_VAR_RW)`: `FETCH_RW` into a `VAR` and the
    /// step over it into a temporary.
    ///
    /// ```php
    /// $x = 'n';
    /// $$x++;
    /// ++$$x;
    /// var_dump($n, ${'n'}--);
    /// ```
    /// ```expect
    /// Warning: Undefined variable $n in %s on line 3
    /// int(2)
    /// int(2)
    /// ```
    pub(super) fn step_by_name(
        &mut self,
        target: tree::Expr,
        opcode: Opcode,
    ) -> Result<Where, CompileError> {
        let Some(held) = self.delayed_named(target, Fetch::Rw)? else {
            return Err(self.fatal("a step by name over what is no variable"));
        };
        let (op1_type, op1) = self.base_fetch(held, Fetch::Rw, Over::Dim);
        let out = self.temporary();
        self.push(Opline {
            opcode,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_global_var`'s second arm (`Z/Zend/zend_compile.c:5441`),
    /// a `global` over a name no CV has, an auto-global or one the
    /// program computes: `FETCH_W (global+lock)` over the name into a
    /// `VAR`, the global's place, then the `ASSIGN_REF` php builds over
    /// a fetch by the same name (`zend_emit_assign_ref_znode`), `FETCH_W`
    /// of the local variable, or of the global for an auto-global, bound
    /// to it, its value unused.
    ///
    /// ```php
    /// $x = ['test', '55'];
    /// global ${$x[0]};
    /// ${$x[0]} = $x[1];
    /// echo "Test: $test\n";
    /// function sf($n) { echo "sf\n"; return $n; }
    /// function f() { global ${sf('g')}; $g++; }
    /// $g = 5;
    /// f();
    /// var_dump($g);
    /// try { global ${new stdClass}; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Test: 55
    /// sf
    /// int(6)
    /// Object of class stdClass could not be converted to string
    /// ```
    pub(super) fn global_by_name(&mut self, name: Where, scope: u32) {
        let name = self.operand(name);
        let global = self.temporary();
        self.push(Opline {
            opcode: Opcode::FetchW,
            op1_type: name.0,
            op1: name.1,
            out_type: OperandKind::Var,
            out: global,
            extended_value: FETCH_GLOBAL_LOCK,
            ..Opline::default()
        });
        let local = self.temporary();
        let (op1_type, op1) = self.lay_fetch_by_name(name, scope, local, Fetch::Write);
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::AssignRef,
            op1_type,
            op1,
            op2_type: OperandKind::Var,
            op2: global,
            out_type: OperandKind::Var,
            out,
            ..Opline::default()
        });
        self.do_free(Where::Returned(out));
    }
}
