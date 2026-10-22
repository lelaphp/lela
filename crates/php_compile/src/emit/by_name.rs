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
//! `$GLOBALS` when it is `GLOBALS` (`FETCH_GLOBALS`, a copy of the global
//! table), and a fetch by name from `EG(symbol_table)` when it is an
//! auto-global's ([`AutoGlobal`], which the unit arms); any other name is
//! the value the program computes, fetched by name from the frame's
//! own table. The table is [`Symbols`], decoded once where the name is.
//! `$GLOBALS['k']` is the fetch by name over `'k'` from the global table
//! (`is_global_var_fetch`, `Z/Zend/zend_compile.c:2974`), which php
//! compiles as the variable `$k` of that table wherever it stands.

use php_heap::Zval;
use php_syntax::tree::{self, NodeId};
use php_unit::{AutoGlobal, Opcode, OperandKind, Opline};

use super::{
    Base, Emit, FETCH_GLOBAL, FETCH_LOCAL, Fetch, HeldBack, Over, SimpleVar, Source, Where,
};
use crate::CompileError;

/// `ZEND_FETCH_GLOBAL_LOCK` (`Z/Zend/zend_compile.h:1084`): the `FETCH_W`
/// of a `global` over a name no CV has, which leaves the name's operand
/// for the fetch after it to read and free.
const FETCH_GLOBAL_LOCK: u32 = 1 << 3;

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
/// (`is_this_fetch`), `$GLOBALS` (`is_globals_fetch`), or a fetch by the
/// name's value from the table [`Symbols`] names.
pub(super) enum Named {
    Cv(Vec<u8>),
    This,
    Globals,
    Fetch { name: Where, table: Symbols },
}

impl Emit<'_> {
    /// `$name`, `$$x`, `${expr}` or `$GLOBALS['k']` sorted as [`Named`],
    /// the name's own oplines laid down where it is not written out;
    /// `None` for what is no variable.
    pub(super) fn named(&mut self, expr: tree::Expr) -> Result<Option<Named>, CompileError> {
        let line = self.line_of(expr);
        if let Some(id) = self.global_var_fetch(expr) {
            let index = self.parsed.tree[id].index;
            return self.named_in_globals(index).map(Some);
        }
        let inner = match self.peel(expr) {
            tree::Expr::SimpleVarExpr(id) => {
                return Ok(Some(match self.simple_var_of(id, line)? {
                    SimpleVar::Cv(name) => Named::Cv(name.to_vec()),
                    SimpleVar::This => Named::This,
                    SimpleVar::Globals => Named::Globals,
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
    /// an auto-global written out, and `$GLOBALS['k']`.
    pub(super) fn is_by_name(&self, expr: tree::Expr) -> bool {
        match self.peel(expr) {
            tree::Expr::VarVarExpr(_) | tree::Expr::DynamicVarExpr(_) => true,
            tree::Expr::SimpleVarExpr(id) => self.parsed.tree[id]
                .var
                .is_some_and(|var| AutoGlobal::of(self.variable_name(var)).is_some()),
            expr => self.global_var_fetch(expr).is_some(),
        }
    }

    /// `is_global_var_fetch`: the `[…]` directly over `$GLOBALS`.
    pub(super) fn global_var_fetch(&self, expr: tree::Expr) -> Option<NodeId<tree::SubscriptExpr>> {
        let tree::Expr::SubscriptExpr(id) = self.peel(expr) else {
            return None;
        };
        self.is_globals(self.parsed.tree[id].base).then_some(id)
    }

    /// `$GLOBALS[]`, the append over `$GLOBALS`, which each construct
    /// refuses in words of its own.
    pub(super) fn is_globals_append(&self, expr: tree::Expr) -> bool {
        self.global_var_fetch(expr)
            .is_some_and(|id| self.parsed.tree[id].index.is_none())
    }

    /// `zend_delayed_compile_dim`'s first arm over `$GLOBALS[…]`: the
    /// index compiled, a constant converted to a string
    /// (`convert_to_string`), and the fetch by that name from
    /// `EG(symbol_table)`; the append is php's compile error. Every
    /// place a variable fetched by name stands, `$GLOBALS['k']` stands:
    /// read, written, stepped, compounded, bound, under `??`, `isset`
    /// and `unset`, and under a further dimension or property.
    ///
    /// ```php
    /// $a = 1;
    /// $GLOBALS['b'] = 2;
    /// $GLOBALS['a'] += 10;
    /// $GLOBALS['c'][] = 3;
    /// $GLOBALS[1] = 'one';
    /// $GLOBALS['a']++;
    /// $GLOBALS['o'] = new stdClass;
    /// $GLOBALS['o']->p = 'p';
    /// $r = &$GLOBALS['b'];
    /// $r = 20;
    /// function f() { $GLOBALS['q'] = 5; return $GLOBALS['q'] + $GLOBALS['a']; }
    /// var_dump($a, $b, $c, ${1}, $o->p, f(), $q, $GLOBALS['z'] ?? 'no', isset($GLOBALS['a']), empty($GLOBALS['z']));
    /// unset($GLOBALS['a']);
    /// var_dump(isset($a), "$GLOBALS[b] {$GLOBALS['q']}");
    /// ```
    /// ```expect
    /// int(12)
    /// int(20)
    /// array(1) {
    ///   [0]=>
    ///   int(3)
    /// }
    /// string(3) "one"
    /// string(1) "p"
    /// int(17)
    /// int(5)
    /// string(2) "no"
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// string(4) "20 5"
    /// ```
    ///
    /// ```php
    /// $GLOBALS[] = 1;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot append to $GLOBALS in %s on line 2
    /// ```
    fn named_in_globals(&mut self, index: Option<tree::Expr>) -> Result<Named, CompileError> {
        let Some(index) = index else {
            return Err(self.compile_error("Cannot append to $GLOBALS"));
        };
        let name = self.global_name(index)?;
        Ok(Named::Fetch {
            name,
            table: Symbols::Global,
        })
    }

    /// The index under `$GLOBALS[…]` as the name it fetches: compiled,
    /// and a constant converted to a string.
    fn global_name(&mut self, index: tree::Expr) -> Result<Where, CompileError> {
        Ok(match self.expr(index)? {
            Where::Constant(value) => Where::Constant(self.stringified(value)?.unwrap_or(value)),
            name => name,
        })
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

    /// The name under `$` or `${…}`: one written out, php's
    /// `ZEND_AST_ZVAL`, is what `is_this_fetch`, `is_globals_fetch` and
    /// `zend_try_compile_cv` make of it; any other expression is
    /// compiled (`zend_compile_simple_var_no_cv`) and fetched by the
    /// value, a constant one converted to a string first and never a
    /// CV, so `${true}` is the variable `$1` fetched by name while
    /// `${1}` and `${'1'}` are its CV.
    ///
    /// ```php
    /// ${true} = 42;
    /// var_dump(${true}, ${1}, ${'1'}, ${null} ?? 'unset', ${PHP_EOL} ?? 'unset');
    /// ${false} = 'e';
    /// var_dump(${''});
    /// ```
    /// ```expect
    /// int(42)
    /// int(42)
    /// int(42)
    /// string(5) "unset"
    /// string(5) "unset"
    /// string(1) "e"
    /// ```
    pub(super) fn named_by_expr(&mut self, inner: tree::Expr) -> Result<Named, CompileError> {
        if let Some(value) = self.written(inner)? {
            let name = self.name_string(value)?;
            return self.named_by(name);
        }
        let name = match self.expr(inner)? {
            Where::Constant(value) => {
                let name = self.name_string(value)?;
                return match AutoGlobal::of(&name) {
                    Some(global) => self.auto_global(global),
                    None => Ok(Named::Fetch {
                        name: Where::Constant(self.string(&name)?),
                        table: Symbols::Local,
                    }),
                };
            }
            name => name,
        };
        Ok(Named::Fetch {
            name,
            table: Symbols::Local,
        })
    }

    /// `convert_to_string` over a variable's name known while the file
    /// compiles.
    fn name_string(&mut self, value: Zval) -> Result<Vec<u8>, CompileError> {
        let value = self.stringified(value)?.unwrap_or(value);
        value
            .as_str()
            .map(|s| s.bytes().to_vec())
            .ok_or_else(|| self.fatal("a variable's name that is no string"))
    }

    /// A name written out: `this`, an auto-global's, or a CV's.
    fn named_by(&mut self, name: Vec<u8>) -> Result<Named, CompileError> {
        if name == b"this" {
            return Ok(Named::This);
        }
        // `is_globals_fetch`: `$GLOBALS` is `FETCH_GLOBALS`, no fetch by
        // name, and not read yet.
        if name == b"GLOBALS" {
            return Ok(Named::Globals);
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
        Base::ByName {
            name,
            table,
            out,
            held: HeldBack::default(),
        }
    }

    /// The fetch by name [`Self::base_fetch`] lays down: `FETCH_R` or
    /// `FETCH_IS` into a temporary, `FETCH_W`, `FETCH_RW` or
    /// `FETCH_FUNC_ARG` into a `VAR` (`zend_adjust_for_fetch_type`).
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
            Fetch::FuncArg => (Opcode::FetchFuncArg, OperandKind::Var),
            Fetch::Unset => (Opcode::FetchUnset, OperandKind::Var),
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

    /// `FETCH_GLOBALS` (`Z/Zend/zend_vm_def.h:9229`), `$GLOBALS` itself
    /// (`zend_compile_simple_var`'s `is_globals_fetch` arm): a copy of
    /// `EG(symbol_table)` into a temporary under a read and into a `VAR`
    /// under a write, which `isset` and `empty` never reach, since php
    /// answers them while it compiles.
    ///
    /// ```php
    /// $a = 1;
    /// function f() { return $GLOBALS; }
    /// $g = f();
    /// $g['a'] = 2;
    /// var_dump($a, $g['a'], isset($GLOBALS), empty($GLOBALS), is_array($GLOBALS));
    /// foreach ($GLOBALS as $name => $value) { if ($name === 'a') echo "a is $value\n"; }
    /// var_dump(count(${'GLOBALS'}) === count($GLOBALS), @$GLOBALS);
    /// ```
    /// ```expect
    /// int(1)
    /// int(2)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// a is 1
    /// bool(true)
    /// NULL
    /// ```
    pub(super) fn fetch_globals(&mut self, out_type: OperandKind) -> Where {
        let out = self.temporary();
        self.push(Opline {
            opcode: Opcode::FetchGlobals,
            out_type,
            out,
            ..Opline::default()
        });
        match out_type {
            OperandKind::Var => Where::Returned(out),
            _ => Where::Temporary(out),
        }
    }

    /// [`Self::named`] under `fetch` as `zend_delayed_compile_var` holds
    /// it: a CV settled, `$this` its `FETCH_THIS`, `$GLOBALS` its
    /// `FETCH_GLOBALS`, and a fetch by name held back. `None` for what is
    /// no variable.
    pub(super) fn delayed_named(
        &mut self,
        expr: tree::Expr,
        fetch: Fetch,
    ) -> Result<Option<Base>, CompileError> {
        Ok(match self.named(expr)? {
            None => None,
            Some(Named::Cv(name)) => {
                self.cv_fetched(&name, fetch);
                Some(Base::Settled((OperandKind::Cv, self.lookup_cv(&name))))
            }
            Some(Named::This) => {
                let kind = match fetch {
                    Fetch::Read | Fetch::Is => OperandKind::Tmp,
                    Fetch::Write | Fetch::Rw | Fetch::FuncArg | Fetch::Unset => OperandKind::Var,
                };
                let this = self.fetch_this(kind);
                Some(Base::Settled(self.operand(this)))
            }
            Some(Named::Globals) => {
                let kind = if matches!(fetch, Fetch::Read | Fetch::Is) {
                    OperandKind::Tmp
                } else {
                    OperandKind::Var
                };
                let globals = self.fetch_globals(kind);
                Some(Base::Settled(self.operand(globals)))
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
        let Some(mut held) = self.delayed_named(target, Fetch::Write)? else {
            return Err(self.fatal("an assignment by name to what is no variable"));
        };
        self.hold_base(&mut held);
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
        let Some(mut held) = self.delayed_named(target, Fetch::Rw)? else {
            return Err(self.fatal("a compound assignment by name to what is no variable"));
        };
        self.hold_base(&mut held);
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
}
