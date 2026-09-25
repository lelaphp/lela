//! A variable the program computes the name of, `$$x` and `${expr}`, and
//! one php never gives a CV, an auto-global: `zend_compile_simple_var`'s
//! arm past `zend_try_compile_cv`, `zend_compile_simple_var_no_cv`
//! (`Z/Zend/zend_compile.c:2915`), which fetches the variable by its
//! name out of a symbol table, `FETCH_R`, `FETCH_W`, `FETCH_RW` or
//! `FETCH_IS` by the fetch the place is compiled under, and the
//! `global` statement's arm over such a name (`zend_compile_global_var`,
//! `:5441`).
//!
//! What the name is, is decided once ([`Named`]): a name written out is
//! the CV `zend_try_compile_cv` gives it, `$this` when it is `this`,
//! and a fetch by name from `EG(symbol_table)` when it is an
//! auto-global's ([`AutoGlobal`], which the unit arms); any other name is
//! the value the program computes, fetched by name from the frame's
//! own table. The table is [`Symbols`], decoded once where the name is.

use php_heap::Zval;
use php_syntax::tree;
use php_unit::{AutoGlobal, Opcode, OperandKind, Opline};

use super::target::Source;
use super::{Base, Emit, FETCH_GLOBAL, FETCH_LOCAL, Fetch, Over, SimpleVar, Where};
use crate::CompileError;

/// `ZEND_FETCH_GLOBAL_LOCK` (`Z/Zend/zend_compile.h:1084`): the `FETCH_W`
/// of a `global` over a name no CV has, which leaves the name's operand
/// for the fetch after it to read and free.
const FETCH_GLOBAL_LOCK: u32 = 1 << 3;

/// `ZEND_ISEMPTY` (`Z/Zend/zend_compile.h:1005`), the bit of an
/// `ISSET_ISEMPTY_*` opline's `extended_value` that says `empty`.
const ISEMPTY: u32 = 1 << 0;

/// The table a fetch by name reads, what `zend_get_target_symbol_table`
/// decodes from the opline: `ZEND_FETCH_GLOBAL`, `EG(symbol_table)`, for
/// an auto-global; `ZEND_FETCH_LOCAL`, the frame's own, for every other
/// name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Symbols {
    Local,
    Global,
}

impl Symbols {
    /// The fetch type the opline carries in `extended_value`, which the
    /// dump prints `(local)` or `(global)`.
    pub(super) const fn fetch_type(self) -> u32 {
        match self {
            Symbols::Local => FETCH_LOCAL,
            Symbols::Global => FETCH_GLOBAL,
        }
    }
}

/// What a variable's name makes of it, decided once: the CV
/// `zend_try_compile_cv` gives a name written out, `$this`
/// (`is_this_fetch`), or a fetch by the name's value from the table
/// [`Symbols`] names.
pub(super) enum Named {
    Cv(Vec<u8>),
    This,
    Fetch { name: Where, table: Symbols },
}

impl Emit<'_> {
    /// `$name`, `$$x` or `${expr}` sorted as [`Named`], the name's own
    /// oplines laid down where it is not written out; `None` for what is
    /// no variable.
    pub(super) fn named(&mut self, expr: tree::Expr) -> Result<Option<Named>, CompileError> {
        let line = self.line_of(expr);
        let inner = match self.peel(expr) {
            tree::Expr::SimpleVarExpr(id) => {
                return Ok(Some(match self.simple_var_of(id, line)? {
                    SimpleVar::Cv(name) => Named::Cv(name),
                    SimpleVar::This => Named::This,
                    SimpleVar::Auto(global) => self.auto_global(global)?,
                }));
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

    /// Whether `expr` is a variable fetched by name: `$$x` and `${expr}`,
    /// and an auto-global written out.
    pub(super) fn is_by_name(&self, expr: tree::Expr) -> bool {
        match self.peel(expr) {
            tree::Expr::VarVarExpr(_) | tree::Expr::DynamicVarExpr(_) => true,
            tree::Expr::SimpleVarExpr(id) => self.parsed.tree[id]
                .var
                .is_some_and(|var| AutoGlobal::of(&self.variable_name(var)).is_some()),
            _ => false,
        }
    }

    /// An auto-global the compiler met, `zend_is_auto_global`: armed by
    /// the unit ([`php_unit::Unit::arms`]) and fetched by its name from
    /// `EG(symbol_table)` under whatever the place is compiled under,
    /// `FETCH_W (global)` under an element's write, `FETCH_RW (global)`
    /// under a compound assignment's or a step's, `FETCH_IS (global)`
    /// under `??`, and the variable itself assigned, bound and iterated
    /// into as any variable fetched by name is.
    ///
    /// ```php
    /// $_SERVER['x'] = 1;
    /// $_SERVER['n'] = 'b';
    /// $_SERVER['n'] .= "a";
    /// $_SERVER['c'] = 0;
    /// $_SERVER['c']++;
    /// ++$_SERVER['c'];
    /// $_ENV['e'] = &$_SERVER['x'];
    /// $_ENV['e'] = 5;
    /// var_dump($_SERVER['x'], $_SERVER['n'], $_SERVER['c'], "$_SERVER[n]-{$_SERVER['c']}", $_POST['p'] ?? 'none');
    /// function g($v) { return $v; }
    /// var_dump(count(array_keys($_FILES)), g($_GET), later($_SERVER['n']));
    /// function later($v) { return "later $v"; }
    /// $_GET = ['a' => 1];
    /// [$_POST['one'], $_POST['two']] = [1, 2];
    /// foreach ([3] as $_COOKIE['k']) {}
    /// var_dump($_GET, $_POST, $_COOKIE);
    /// ```
    /// ```expect
    /// int(5)
    /// string(2) "ba"
    /// int(2)
    /// string(4) "ba-2"
    /// string(4) "none"
    /// int(0)
    /// array(0) {
    /// }
    /// string(8) "later ba"
    /// array(1) {
    ///   ["a"]=>
    ///   int(1)
    /// }
    /// array(2) {
    ///   ["one"]=>
    ///   int(1)
    ///   ["two"]=>
    ///   int(2)
    /// }
    /// array(1) {
    ///   ["k"]=>
    ///   int(3)
    /// }
    /// ```
    fn auto_global(&mut self, global: AutoGlobal) -> Result<Named, CompileError> {
        self.arm(global);
        let name = self.string(global.name())?;
        Ok(Named::Fetch {
            name: Where::Constant(name),
            table: Symbols::Global,
        })
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
            table: Symbols::Local,
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
        if let Some(global) = AutoGlobal::of(&name) {
            return self.auto_global(global);
        }
        Ok(Named::Cv(name))
    }

    /// `zend_compile_simple_var_no_cv` with `delayed`: the name's operand
    /// settled and the `VAR` or temporary numbered now, the opline itself
    /// held back for [`Self::base_fetch`] to lay down, which is how php
    /// numbers `$$x = f()`'s fetch before the call's.
    pub(super) fn held_by_name(&mut self, name: Where, table: Symbols) -> Base {
        let name = self.operand(name);
        let out = self.temporary();
        Base::ByName { name, table, out }
    }

    /// The fetch by name [`Self::base_fetch`] lays down: `FETCH_R` or
    /// `FETCH_IS` into a temporary, `FETCH_W` or `FETCH_RW` into a `VAR`
    /// (`zend_adjust_for_fetch_type`).
    pub(super) fn lay_fetch_by_name(
        &mut self,
        (op1_type, op1): (OperandKind, u32),
        table: Symbols,
        out: u32,
        fetch: Fetch,
    ) -> (OperandKind, u32) {
        let (opcode, out_type) = match fetch {
            Fetch::Read => (Opcode::FetchR, OperandKind::Tmp),
            Fetch::Write => (Opcode::FetchW, OperandKind::Var),
            Fetch::Rw => (Opcode::FetchRw, OperandKind::Var),
            Fetch::Is => (Opcode::FetchIs, OperandKind::Tmp),
        };
        self.push(Opline {
            opcode,
            op1_type,
            op1,
            out_type,
            out,
            extended_value: table.fetch_type(),
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
                    Fetch::Write | Fetch::Rw => OperandKind::Var,
                };
                let this = self.fetch_this(kind);
                Some(Base::Settled(self.operand(this)))
            }
            Some(Named::Fetch { name, table }) => Some(self.held_by_name(name, table)),
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
    pub(super) fn global_by_name(&mut self, name: Where, table: Symbols) {
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
        let (op1_type, op1) = self.lay_fetch_by_name(name, table, local, Fetch::Write);
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

    /// `zend_compile_isset_or_empty`'s arm over a variable fetched by
    /// name: `zend_compile_simple_var_no_cv` under `BP_VAR_IS` with its
    /// opcode made `ISSET_ISEMPTY_VAR`, the name in `op1`, the table's
    /// fetch type and `ZEND_ISEMPTY` for `empty` in `extended_value`, the
    /// answer in a temporary. The bucket is asked, never fetched, so
    /// nothing is warned of.
    ///
    /// ```php
    /// $a = 1; $z = 0;
    /// $names = ['a', 'z', 'none'];
    /// foreach ($names as $name) { var_dump(isset($$name), empty(${$name})); }
    /// var_dump(isset($_SERVER), empty($_FILES), isset($_ENV['nothing']), empty($_GET));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// ```
    pub(super) fn isset_by_name(
        &mut self,
        var: tree::Expr,
        empty: bool,
    ) -> Result<Where, CompileError> {
        let Some(Named::Fetch { name, table }) = self.named(var)? else {
            return Err(self.fatal("an `isset` by name over what is no fetch by name"));
        };
        let (op1_type, op1) = self.operand(name);
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::IssetIsemptyVar,
            op1_type,
            op1,
            out_type: OperandKind::Tmp,
            out,
            extended_value: table.fetch_type() | if empty { ISEMPTY } else { 0 },
            ..Opline::default()
        });
        Ok(Where::Temporary(out))
    }

    /// `zend_compile_unset`'s `ZEND_AST_VAR` arm past `zend_try_compile_cv`:
    /// `zend_compile_simple_var_no_cv` under `BP_VAR_UNSET` with its
    /// opcode made `UNSET_VAR`, the name in `op1` and the table's fetch
    /// type in `extended_value`, nothing left.
    ///
    /// ```php
    /// $a = 1; $b = 2; $n = 'a';
    /// unset($$n);
    /// var_dump(isset($a), $b);
    /// $_ENV['k'] = 1;
    /// unset($_ENV);
    /// var_dump(isset($_ENV));
    /// ```
    /// ```expect
    /// bool(false)
    /// int(2)
    /// bool(false)
    /// ```
    pub(super) fn unset_by_name(&mut self, var: tree::Expr) -> Result<(), CompileError> {
        let Some(Named::Fetch { name, table }) = self.named(var)? else {
            return Err(self.fatal("an `unset` by name over what is no fetch by name"));
        };
        let (op1_type, op1) = self.operand(name);
        self.push(Opline {
            opcode: Opcode::UnsetVar,
            op1_type,
            op1,
            extended_value: table.fetch_type(),
            ..Opline::default()
        });
        Ok(())
    }

    /// `zend_compile_dim` under `BP_VAR_UNSET` over one dimension of a
    /// variable fetched by name, its last opline made `UNSET_DIM`: the
    /// fetch held back (`zend_delayed_compile_var`), the dimension, then
    /// `FETCH_UNSET` into a `VAR` and `UNSET_DIM` over it.
    ///
    /// ```php
    /// $_SERVER['k'] = 1;
    /// $_SERVER['l'] = 2;
    /// unset($_SERVER['k']);
    /// $n = 'a';
    /// $a = ['x' => 1, 'y' => 2];
    /// $m = 'none';
    /// unset($$n['x'], $$n['nope'], $$m['z']);
    /// var_dump(isset($_SERVER['k']), $_SERVER['l'], $a);
    /// ```
    /// ```expect
    /// bool(false)
    /// int(2)
    /// array(1) {
    ///   ["y"]=>
    ///   int(2)
    /// }
    /// ```
    pub(super) fn unset_dim_by_name(
        &mut self,
        container: tree::Expr,
        index: tree::Expr,
    ) -> Result<(), CompileError> {
        let Some(Named::Fetch { name, table }) = self.named(container)? else {
            return Err(self.fatal("an `unset` by name over what is no fetch by name"));
        };
        let name = self.operand(name);
        let out = self.temporary();
        let dim = self.expr(index)?;
        let (op2_type, op2) = self.dim_operand(dim);
        self.push(Opline {
            opcode: Opcode::FetchUnset,
            op1_type: name.0,
            op1: name.1,
            out_type: OperandKind::Var,
            out,
            extended_value: table.fetch_type(),
            ..Opline::default()
        });
        self.push(Opline {
            opcode: Opcode::UnsetDim,
            op1_type: OperandKind::Var,
            op1: out,
            op2_type,
            op2,
            ..Opline::default()
        });
        Ok(())
    }
}
