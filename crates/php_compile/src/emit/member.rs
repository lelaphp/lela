//! A member named after `->`, `?->` or `::`: php's `prop_ast` and
//! `method_ast`, which `zend_delayed_compile_prop`,
//! `zend_compile_method_call` and `zend_compile_static_call` compile with
//! `zend_compile_expr` like any other operand
//! (`Z/Zend/zend_compile.c:3178`, `:5271`, `:5352`). A name written out is
//! a constant in `op2`; `$o->$p`, `$o->{expr}`, `$o->$m()` and `A::$m()`
//! are the value the program computes, a `CV` or a temporary in `op2`,
//! which the engine converts where the opline runs.
//!
//! What the name is, is decided once, where the node is read
//! ([`Member`]); its operand is laid down where php compiles it
//! ([`Emit::prop_name`], [`Emit::method_name`]), after the object's own
//! oplines and before the fetch or the call that names it.

use php_syntax::tree::{self, NodeId};
use php_unit::{Level, OperandKind};

use super::{Emit, Where, walk};
use crate::CompileError;

/// A member's name as the tree holds it: an identifier written out, or
/// an expression whose value names it.
#[derive(Clone, Copy)]
pub(super) enum Member {
    Written(tree::TokenId),
    Computed(tree::Expr),
}

/// A property's name as the fetch names it in `op2`, php's `prop_node`
/// after `zend_delayed_compile_prop`: the operand, and the three cache
/// slots `zend_alloc_cache_slots(3)` takes for a constant name alone, `0`
/// where the name is computed.
#[derive(Clone, Copy)]
pub(super) struct PropName {
    pub(super) kind: OperandKind,
    pub(super) at: u32,
    pub(super) slots: u32,
}

/// A method's name as `zend_compile_method_call` and
/// `zend_compile_static_call` hold it once compiled: the name its
/// constant pair will carry, or the operand the program's value stands
/// in.
pub(super) enum MethodName<'a> {
    Constant(&'a [u8]),
    Computed((OperandKind, u32)),
}

impl<'a> Emit<'a> {
    /// The member after `->` or `?->`: the identifier, or `$p`, `$$p`,
    /// `${expr}` and `{expr}` as the expression that names it.
    pub(super) fn member_of(
        &self,
        id: NodeId<tree::MemberAccessExpr>,
    ) -> Result<Member, CompileError> {
        let refused = || walk::unread(self.line, "a property fetch");
        match self.parsed.tree[id].member {
            Some(tree::MemberSelector::MemberName(member)) => self.parsed.tree[member]
                .name
                .map(Member::Written)
                .ok_or_else(refused),
            Some(tree::MemberSelector::SimpleVarExpr(var)) => {
                Ok(Member::Computed(tree::Expr::SimpleVarExpr(var)))
            }
            Some(tree::MemberSelector::VarVarExpr(var)) => {
                Ok(Member::Computed(tree::Expr::VarVarExpr(var)))
            }
            Some(tree::MemberSelector::DynamicVarExpr(var)) => {
                Ok(Member::Computed(tree::Expr::DynamicVarExpr(var)))
            }
            Some(tree::MemberSelector::BracedMember(braced)) => self.parsed.tree[braced]
                .inner
                .map(Member::Computed)
                .ok_or_else(refused),
            None => Err(refused()),
        }
    }

    /// The method after `::` in a call, php's `member_name`: the
    /// identifier, or `$m`, `$$m`, `${expr}` and `{expr}` as the
    /// expression that names it, where outside a call `$m` would name a
    /// `static` property instead.
    pub(super) fn scoped_method_of(
        &self,
        id: NodeId<tree::ScopedAccessExpr>,
    ) -> Result<Member, CompileError> {
        let refused = || walk::unread(self.line, "a static call");
        match self.parsed.tree[id].member {
            Some(tree::ScopedSelector::MemberName(member)) => self.parsed.tree[member]
                .name
                .map(Member::Written)
                .ok_or_else(refused),
            Some(tree::ScopedSelector::SimpleVarExpr(var)) => {
                Ok(Member::Computed(tree::Expr::SimpleVarExpr(var)))
            }
            Some(tree::ScopedSelector::VarVarExpr(var)) => {
                Ok(Member::Computed(tree::Expr::VarVarExpr(var)))
            }
            Some(tree::ScopedSelector::DynamicVarExpr(var)) => {
                Ok(Member::Computed(tree::Expr::DynamicVarExpr(var)))
            }
            Some(tree::ScopedSelector::BracedMember(braced)) => self.parsed.tree[braced]
                .inner
                .map(Member::Computed)
                .ok_or_else(refused),
            // `C::class()`: the keyword is an identifier after `::` in a
            // call, php's semi-reserved `member_name`.
            Some(tree::ScopedSelector::ClassKw(token)) => Ok(Member::Written(token)),
            None => Err(refused()),
        }
    }

    /// `zend_compile_expr(&prop_node, prop_ast)` and what
    /// `zend_delayed_compile_prop` does with a constant after it,
    /// `convert_to_string` and three cache slots: a name written out, or
    /// one the compiler folds (`$o->{'p'}`, `$o->{1}`), is the constant
    /// string, and any other is the operand the expression left, with no
    /// slot.
    ///
    /// ```php
    /// class C { public $a = 1; public $b = [2]; }
    /// $o = new C;
    /// $p = 'a';
    /// echo $o->$p, $o->{'b'}[0], $o->{"a"}, "\n";
    /// $o->$p = 5;
    /// $o->{'x' . 'y'} = 6;
    /// $o->$p += 2;
    /// $o->{$p}++;
    /// $n = 'b';
    /// $o->$n[] = 3;
    /// var_dump($o->a, $o->xy, $o->b, isset($o->$p), isset($o->{'q'}), empty($o->$n));
    /// unset($o->$p);
    /// var_dump(isset($o->a), $o->{1} ?? 'none');
    /// $o->{1} = 'one';
    /// var_dump($o->{'1'});
    /// ```
    /// ```expect
    /// 121
    ///
    /// Deprecated: Creation of dynamic property C::$xy is deprecated in %s on line 7
    /// int(8)
    /// int(6)
    /// array(2) {
    ///   [0]=>
    ///   int(2)
    ///   [1]=>
    ///   int(3)
    /// }
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// string(4) "none"
    ///
    /// Deprecated: Creation of dynamic property C::$1 is deprecated in %s on line 15
    /// string(3) "one"
    /// ```
    pub(super) fn prop_name(&mut self, member: Member) -> Result<PropName, CompileError> {
        let constant = match member {
            Member::Written(token) => {
                let name = self.text(token).to_vec();
                self.string(&name)?
            }
            Member::Computed(expr) => match self.expr(expr)? {
                // `convert_to_string` over the constant, whose warning
                // for an array is the compilation's.
                Where::Constant(value) => {
                    if let Some(name) = self.stringified(value)? {
                        name
                    } else {
                        self.raise(Level::Warning, "Array to string conversion");
                        self.string(b"Array")?
                    }
                }
                computed => {
                    let (kind, at) = self.operand(computed);
                    return Ok(PropName { kind, at, slots: 0 });
                }
            },
        };
        let at = self.literal(constant);
        let slots = self.cache_slots(3);
        Ok(PropName {
            kind: OperandKind::Const,
            at,
            slots,
        })
    }

    /// `zend_compile_expr(&method_node, method_ast)` and the check after
    /// it: a name written out, or folded to a string, is the name the
    /// constant pair `zend_add_func_name_literal` adds will carry; a
    /// folded constant that is no string is php's compile-time `Method
    /// name must be a string`; any other is the operand the expression
    /// left, which the engine holds to the same rule where the call is
    /// made.
    ///
    /// ```php
    /// $o = new stdClass;
    /// $o->{1}();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Method name must be a string in %s on line 3
    /// ```
    pub(super) fn method_name(&mut self, member: Member) -> Result<MethodName<'a>, CompileError> {
        let name = match member {
            Member::Written(token) => self.text(token),
            Member::Computed(expr) => match self.expr(expr)? {
                Where::Constant(value) => match value.as_str() {
                    Some(name) => self.arena.copy(name.bytes()),
                    None => return Err(self.compile_error("Method name must be a string")),
                },
                computed => return Ok(MethodName::Computed(self.operand(computed))),
            },
        };
        Ok(MethodName::Constant(name))
    }

    /// `zend_ast_get_lineno(method_ast)`: the line of a method's name,
    /// which php's `DO_FCALL` of a call to it carries, so a call whose
    /// receiver spans lines stands in a trace where its name is written.
    ///
    /// ```php
    /// class X { function __construct($f) {} function start() { echo debug_backtrace()[0]['line'], "\n"; } }
    /// (new X(function () {
    ///     return 1;
    /// }))->start();
    /// ```
    /// ```expect
    /// 5
    /// ```
    pub(super) fn member_line(&self, member: Member) -> u32 {
        match member {
            Member::Written(token) => (self.parsed.tokens.get(token.0 as usize))
                .map_or(self.line, |token| {
                    self.parsed.lines.line_number(token.offset)
                }),
            Member::Computed(expr) => self.line_of(expr),
        }
    }
}
