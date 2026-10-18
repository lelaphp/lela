//! `list()` and `[…]` as targets: `zend_compile_assign`'s
//! `ZEND_AST_ARRAY` arm and `zend_compile_list_assign`
//! (`Z/Zend/zend_compile.c:3298`), one `FETCH_LIST_R` per element, or
//! `FETCH_LIST_W` and `MAKE_REF` for an element taken by reference, each
//! assigned as its own `ASSIGN` over what the fetch left.

use php_heap::Zval;
use php_syntax::tree;
use php_unit::{Opcode, OperandKind, Opline};

use super::target::{List, ListStyle};
use super::{Emit, SimpleVar, Source, Where};
use crate::CompileError;

impl<'a> Emit<'a> {
    /// `zend_compile_assign`'s `ZEND_AST_ARRAY` arm: the right side
    /// compiled for the list to read. Where an element anywhere in the
    /// list is taken by reference (`zend_propagate_list_refs`) it is
    /// `zend_compile_var` under `BP_VAR_W` and a `MAKE_REF` over it, so
    /// every element binds to the right side's own cells; a plain
    /// variable is copied into a temporary first, so `[$a, $b] = $a`
    /// reads the old `$a` for both; anything else is the expression.
    ///
    /// ```php
    /// $a = [1, 2];
    /// [$a, $b] = $a;
    /// var_dump($a, $b);
    /// $x = ['a' => [1], 'k' => 2];
    /// ['a' => [&$r], 'k' => &$s] = $x;
    /// $r = 'r'; $s = 's';
    /// var_dump($x);
    /// ```
    /// ```expect
    /// int(1)
    /// int(2)
    /// array(2) {
    ///   ["a"]=>
    ///   array(1) {
    ///     [0]=>
    ///     &string(1) "r"
    ///   }
    ///   ["k"]=>
    ///   &string(1) "s"
    /// }
    /// ```
    ///
    /// php's four refusals of a list's shape are its compile errors:
    ///
    /// ```php
    /// [] = [1];
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use empty list in %s on line 2
    /// ```
    ///
    /// ```php
    /// [$a, 'k' => $b] = [1];
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot mix keyed and unkeyed array entries in assignments in %s on line 2
    /// ```
    ///
    /// ```php
    /// $x = [list($a)];
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use list() as standalone expression in %s on line 2
    /// ```
    ///
    /// ```php
    /// [...$a] = [1];
    /// ```
    /// ```expect exit=255
    /// Fatal error: Spread operator is not supported in assignments in %s on line 2
    /// ```
    pub(super) fn list_assign_expr(
        &mut self,
        list: &List,
        value: tree::Expr,
    ) -> Result<Where, CompileError> {
        let line = self.line;
        let written = self.peel(value);
        let value = if self.list_has_refs(list) {
            if !self.is_variable_or_call(written) {
                return Err(
                    self.compile_error("Cannot assign reference to non referenceable value")
                );
            }
            if self.is_short_circuited(written) {
                return Err(self.compile_error("Cannot take reference of a nullsafe chain"));
            }
            let (kind, at) = self.var_by_ref(written)?;
            let out = self.temporary();
            self.push(Opline {
                opcode: Opcode::MakeRef,
                op1_type: kind,
                op1: at,
                out_type: OperandKind::Var,
                out,
                ..Opline::default()
            });
            Where::Returned(out)
        } else {
            match written {
                tree::Expr::SimpleVarExpr(id) => match self.simple_var_of(id, line)? {
                    SimpleVar::Cv(name) => {
                        let cv = self.lookup_cv(name);
                        let out = self.temporary();
                        self.push(Opline {
                            opcode: Opcode::QmAssign,
                            op1_type: OperandKind::Cv,
                            op1: cv,
                            out_type: OperandKind::Tmp,
                            out,
                            ..Opline::default()
                        });
                        Where::Temporary(out)
                    }
                    SimpleVar::This | SimpleVar::Auto(_) => self.expr(written)?,
                    // `zend_try_compile_cv` fails and
                    // `zend_compile_simple_var_no_cv` fetches the name,
                    // which `EG(symbol_table)` has not: php's own warning.
                    SimpleVar::Globals => self.fetch_by_name(b"GLOBALS", super::Symbols::Global)?,
                },
                _ => self.expr(value)?,
            }
        };
        self.line = line;
        Ok(self.list_assign(list, value, true)?.unwrap_or(value))
    }

    /// `zend_compile_list_assign` (`Z/Zend/zend_compile.c:3298`): each
    /// element in order, its dimension the key written or its position,
    /// a `FETCH_LIST_R` of it from `value` into a `VAR`, or for an
    /// element taken by reference a `FETCH_LIST_W` (`FETCH_DIM_W` over a
    /// CV) and a `MAKE_REF`, then the element's own target assigned from
    /// that `VAR`: a nested list the same way, a variable, an element or
    /// a property by the `ASSIGN` or `ASSIGN_REF` php builds over it
    /// (`zend_emit_assign_znode`). An empty slot is skipped where the list
    /// is not keyed. `value` is handed back where the assignment's value
    /// is read, and freed otherwise.
    ///
    /// Each key is compiled where its element stands, so the keys and
    /// the reads interleave in the order written:
    ///
    /// ```php
    /// ['b' => $b, 'a' => [, $c]] = ['a' => [1, 2], 'b' => 3];
    /// function k($n) { echo "k$n "; return $n; }
    /// [k(1) => $d, k(0) => $e] = ['x', 'y'];
    /// var_dump($b, $c, $d, $e);
    /// ```
    /// ```expect
    /// k1 k0 int(3)
    /// int(2)
    /// string(1) "y"
    /// string(1) "x"
    /// ```
    pub(super) fn list_assign(
        &mut self,
        list: &List,
        value: Where,
        result: bool,
    ) -> Result<Option<Where>, CompileError> {
        let tree = &self.parsed.tree;
        // `list_is_keyed`: the first element that is there says.
        let keyed = list
            .elements
            .iter()
            .map(|id| &tree[*id])
            .find(|element| !is_elision(element))
            .is_some_and(|element| element.spread.is_none() && element.key.is_some());
        let mut has_elems = false;
        for (at, id) in list.elements.iter().enumerate() {
            let element = &self.parsed.tree[*id];
            if is_elision(element) {
                if keyed {
                    return Err(self.compile_error(
                        "Cannot use empty array entries in keyed array assignment",
                    ));
                }
                continue;
            }
            if element.spread.is_some() {
                return Err(self.compile_error("Spread operator is not supported in assignments"));
            }
            let key = element.key.map(|key| self.parsed.tree[key].key);
            let Some(var) = element.value else {
                return Err(self.unread("a list element"));
            };
            has_elems = true;
            let dim = match (keyed, key) {
                (true, Some(key)) => self.expr(key)?,
                (false, None) => Where::Constant(Zval::long(i64::try_from(at).unwrap_or(0))),
                _ => {
                    return Err(self.compile_error(
                        "Cannot mix keyed and unkeyed array entries in assignments",
                    ));
                }
            };
            self.verify_list_target(var, list.style)?;
            let by_ref = self.element_by_ref(*id);
            let (op1_type, op1) = self.operand(value);
            let (op2_type, op2) = self.dim_operand(dim);
            let out = self.temporary();
            self.push(Opline {
                opcode: match (by_ref, value) {
                    (false, _) => Opcode::FetchListR,
                    (true, Where::Cv(_)) => Opcode::FetchDimW,
                    (true, _) => Opcode::FetchListW,
                },
                op1_type,
                op1,
                op2_type,
                op2,
                out_type: OperandKind::Var,
                out,
                ..Opline::default()
            });
            let mut fetched = Where::Returned(out);
            if by_ref {
                let made = self.temporary();
                self.push(Opline {
                    opcode: Opcode::MakeRef,
                    op1_type: OperandKind::Var,
                    op1: out,
                    out_type: OperandKind::Var,
                    out: made,
                    ..Opline::default()
                });
                fetched = Where::Returned(made);
            }
            if let Some(nested) = self.list_of(var) {
                self.list_assign(&nested, fetched, false)?;
            } else {
                // `zend_compile_expr` over the `ASSIGN` php builds: the
                // line is the target's.
                self.line = self.line_of(var);
                let assigned = if by_ref {
                    self.assign_ref(var, Source::Node(fetched))?
                } else {
                    self.assign_to(var, Source::Node(fetched))?
                };
                self.do_free(assigned);
            }
        }
        if !has_elems {
            return Err(self.compile_error("Cannot use empty list"));
        }
        if result {
            return Ok(Some(value));
        }
        self.do_free(value);
        Ok(None)
    }

    /// `zend_verify_list_assign_target` (`Z/Zend/zend_compile.c:3249`):
    /// a nested list spelled as its parent is, never `array(…)`, and any
    /// other element a place php can write (`zend_can_write_to_variable`).
    fn verify_list_target(&self, var: tree::Expr, style: ListStyle) -> Result<(), CompileError> {
        if let Some(nested) = self.list_of(var) {
            if nested.style == ListStyle::Long {
                return Err(self.compile_error("Cannot assign to array(), use [] instead"));
            }
            if nested.style != style {
                return Err(self.compile_error("Cannot mix [] and list()"));
            }
            return Ok(());
        }
        let mut base = self.peel(var);
        loop {
            base = match base {
                tree::Expr::SubscriptExpr(id) => self.peel(self.parsed.tree[id].base),
                tree::Expr::MemberAccessExpr(id) if !self.is_nullsafe(id) => {
                    self.peel(self.parsed.tree[id].obj)
                }
                _ => break,
            };
        }
        if !self.is_variable_or_call(base) || self.is_short_circuited(base) {
            return Err(self.compile_error("Assignments can only happen to writable values"));
        }
        Ok(())
    }

    /// The list `expr` is, where it is one.
    pub(super) fn list_of(&self, expr: tree::Expr) -> Option<List<'a>> {
        let tree = &self.parsed.tree;
        match self.peel(expr) {
            tree::Expr::ListExpr(id) => Some(List {
                elements: tree.list(tree[id].array_elements),
                style: ListStyle::List,
            }),
            tree::Expr::ArrayExpr(id) => Some(List {
                elements: tree.list(tree[id].array_elements),
                style: if tree[id].long.is_some() {
                    ListStyle::Long
                } else {
                    ListStyle::Short
                },
            }),
            _ => None,
        }
    }

    /// `zend_propagate_list_refs` (`Z/Zend/zend_compile.c:3266`): whether
    /// any element of the list, at any depth, is taken by reference.
    pub(super) fn list_has_refs(&self, list: &List) -> bool {
        list.elements.iter().any(|id| self.element_by_ref(*id))
    }

    /// An element taken by reference: written `&`, or a nested list with
    /// such an element in it, which php marks on the element itself.
    fn element_by_ref(&self, id: tree::NodeId<tree::ArrayElement>) -> bool {
        let element = &self.parsed.tree[id];
        if is_elision(element) || element.spread.is_some() {
            return false;
        }
        match element.value.and_then(|value| self.list_of(value)) {
            Some(nested) => self.list_has_refs(&nested),
            None => element.byref.is_some(),
        }
    }
}

/// An empty slot, `[, $b]`: an element with nothing written in it.
fn is_elision(element: &tree::ArrayElement) -> bool {
    element.value.is_none() && element.spread.is_none() && element.key.is_none()
}
