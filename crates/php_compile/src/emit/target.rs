//! What stands on the left of a write, read once: `zend_compile_assign`'s
//! checks before it compiles anything and its switch over the target's
//! kind (`Z/Zend/zend_compile.c:3452`), and the value it writes, which is
//! a node of the tree or what the compiler already holds, php's
//! `ZEND_AST_ZNODE`.
//!
//! An assignment, a `foreach` target and each element of a `list()` are
//! one path here: php compiles a `list()` element and a `foreach` value
//! that is no plain variable by building an `ASSIGN` over a `ZNODE`
//! (`zend_emit_assign_znode`) and compiling it as the program's own, so
//! the same checks and the same oplines serve all three.

use php_syntax::tree::{self, NodeId};

use super::{ClassSpec, Emit, Member, Scoped, ScopedName, SimpleVar, Source, Where, walk};
use crate::CompileError;

/// A place a write lands, decoded once from the tree: php's
/// `var_ast->kind` after `zend_ensure_writable_variable`.
pub(super) enum Target<'a> {
    /// `$name`, the CV `zend_try_compile_cv` gives it.
    Cv(&'a [u8]),
    /// `$a[k]`, `ZEND_AST_DIM`: the whole chain, as the node written.
    Dim(tree::Expr),
    /// `$o->name` or `$o->$name`, `ZEND_AST_PROP`.
    Prop { object: tree::Expr, name: Member },
    /// `C::$name`, `ZEND_AST_STATIC_PROP`.
    StaticProp {
        class: ClassSpec,
        name: ScopedName<'a>,
    },
    /// `list(…)` or `[…]`, `ZEND_AST_ARRAY` as a target.
    List(List<'a>),
    /// `$$x` or `${expr}`, a `ZEND_AST_VAR` whose name is no `ZEND_AST_ZVAL`,
    /// or an auto-global, which no CV holds: the variable, fetched by its
    /// name.
    ByName(tree::Expr),
}

/// A `list()` or a `[…]` written as a target: its elements, and which of
/// the three spellings it is, php's `attr` on the `ZEND_AST_ARRAY`.
pub(super) struct List<'a> {
    pub(super) elements: &'a [NodeId<tree::ArrayElement>],
    pub(super) style: ListStyle,
}

/// `ZEND_ARRAY_SYNTAX_LIST`, `ZEND_ARRAY_SYNTAX_SHORT` and
/// `ZEND_ARRAY_SYNTAX_LONG` (`Z/Zend/zend_compile.h`): `list(…)`, `[…]`
/// and `array(…)`, the last of which is no target.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ListStyle {
    List,
    Short,
    Long,
}

impl<'a> Emit<'a> {
    /// `expr` as a target, with the checks `zend_compile_assign` and
    /// `zend_compile_assign_ref` make before they compile anything:
    /// `$this` is `Cannot re-assign $this`, and then
    /// `zend_ensure_writable_variable`'s refusals
    /// ([`Self::ensure_writable`]).
    pub(super) fn target(&mut self, expr: tree::Expr) -> Result<Target<'a>, CompileError> {
        if self.is_this(expr) {
            return Err(self.compile_error("Cannot re-assign $this"));
        }
        self.ensure_writable(expr)?;
        self.place(expr)
    }

    /// `expr` decoded as a [`Target`] with no check made: what an
    /// `unset` deletes, which makes `zend_ensure_writable_variable`'s
    /// checks and its own `$this` refusal before it
    /// (`zend_compile_unset`), and what [`Self::target`] decodes after
    /// an assignment's.
    pub(super) fn place(&mut self, expr: tree::Expr) -> Result<Target<'a>, CompileError> {
        let line = self.line_of(expr);
        let expr = self.peel(expr);
        let parsed = self.parsed;
        let tree = &parsed.tree;
        Ok(match expr {
            // `$$x`, `${expr}` and an auto-global, a `ZEND_AST_VAR` no CV
            // holds, and `$GLOBALS['k']`, which `zend_compile_assign`
            // treats as one: the variable fetched by its name.
            _ if self.is_by_name(expr) => Target::ByName(expr),
            tree::Expr::SimpleVarExpr(id) => match self.simple_var_of(id, line)? {
                SimpleVar::Cv(name) => Target::Cv(name),
                SimpleVar::This => return Err(self.compile_error("Cannot re-assign $this")),
                SimpleVar::Auto(_) => Target::ByName(expr),
                SimpleVar::Globals => return Err(self.fatal("`$GLOBALS` past its writable check")),
            },
            tree::Expr::SubscriptExpr(_) => Target::Dim(expr),
            tree::Expr::MemberAccessExpr(id) => {
                let name = self.member_of(id)?;
                Target::Prop {
                    object: tree[id].obj,
                    name,
                }
            }
            tree::Expr::ScopedAccessExpr(id) => match self.scoped_of(id, line)? {
                Scoped::Property { class, name } => Target::StaticProp { class, name },
                Scoped::Constant { .. } | Scoped::ClassName { .. } => {
                    return Err(walk::unread(line, "an assignment to what is no variable"));
                }
            },
            tree::Expr::ListExpr(id) => Target::List(List {
                elements: tree.list(tree[id].array_elements),
                style: ListStyle::List,
            }),
            tree::Expr::ArrayExpr(id) => Target::List(List {
                elements: tree.list(tree[id].array_elements),
                style: if tree[id].long.is_some() {
                    ListStyle::Long
                } else {
                    ListStyle::Short
                },
            }),
            _ => return Err(walk::unread(line, "an assignment to what is no variable")),
        })
    }

    /// `zend_ensure_writable_variable` (`Z/Zend/zend_compile.c:3388`):
    /// what a call returned, a chain a `?->` stands in and `$GLOBALS`
    /// itself are no place to write, each in php's words.
    pub(super) fn ensure_writable(&self, expr: tree::Expr) -> Result<(), CompileError> {
        let expr = self.peel(expr);
        if let tree::Expr::CallExpr(id) = expr {
            return Err(self.call_written(id));
        }
        // `ZEND_AST_PIPE`, a call as `zend_ensure_writable_variable` reads
        // it.
        if self.is_call(expr) {
            return Err(self.compile_error("Can't use function return value in write context"));
        }
        if self.is_short_circuited(expr) {
            return Err(self.compile_error("Can't use nullsafe operator in write context"));
        }
        if self.is_globals(expr) {
            return Err(self.compile_error(
                "$GLOBALS can only be modified using the $GLOBALS[$name] = $value syntax",
            ));
        }
        Ok(())
    }

    /// What a call returned, where a place to write is wanted: php's
    /// `zend_ensure_writable_variable`, by the callee as written, so
    /// `($o->m)()` is a function's value as `$o->m()` is a method's.
    pub(super) fn call_written(&self, id: NodeId<tree::CallExpr>) -> CompileError {
        self.compile_error(match self.parsed.tree[id].callee {
            tree::Expr::MemberAccessExpr(_) | tree::Expr::ScopedAccessExpr(_) => {
                "Can't use method return value in write context"
            }
            _ => "Can't use function return value in write context",
        })
    }

    /// `zend_ast_is_short_circuited` (`Z/Zend/zend_compile.c:2504`):
    /// whether a `?->` stands anywhere down the chain `expr` ends, through
    /// dimensions, properties, `static` properties and calls on an object
    /// or a class.
    pub(super) fn is_short_circuited(&self, expr: tree::Expr) -> bool {
        let tree = &self.parsed.tree;
        match self.peel(expr) {
            tree::Expr::SubscriptExpr(id) => self.is_short_circuited(tree[id].base),
            tree::Expr::MemberAccessExpr(id) => {
                self.is_nullsafe(id) || self.is_short_circuited(tree[id].obj)
            }
            tree::Expr::ScopedAccessExpr(id) => {
                self.names_property(id) && self.is_short_circuited(tree[id].scope)
            }
            // The callee as written: `($o?->f)()` calls a value, php's
            // `ZEND_AST_CALL`, which no `?->` short-circuits.
            tree::Expr::CallExpr(id) => match tree[id].callee {
                tree::Expr::MemberAccessExpr(access) => {
                    self.is_nullsafe(access) || self.is_short_circuited(tree[access].obj)
                }
                tree::Expr::ScopedAccessExpr(scoped) => self.is_short_circuited(tree[scoped].scope),
                _ => false,
            },
            _ => false,
        }
    }

    /// Whether the member access is written `?->`.
    pub(super) fn is_nullsafe(&self, id: NodeId<tree::MemberAccessExpr>) -> bool {
        self.parsed.tree[id].op.map(|op| self.kind(op))
            == Some(php_syntax::SyntaxKind::NULLSAFE_ARROW)
    }

    /// `is_globals_fetch`: `$GLOBALS` itself.
    pub(super) fn is_globals(&self, expr: tree::Expr) -> bool {
        match self.peel(expr) {
            tree::Expr::SimpleVarExpr(id) => self.parsed.tree[id]
                .var
                .is_some_and(|var| self.variable_name(var) == b"GLOBALS"),
            _ => false,
        }
    }

    /// `zend_is_variable` or `zend_is_call` (`Z/Zend/zend_compile.c:2718`),
    /// a pipe among the calls, so a list may name one and the assignment
    /// refuses it as a call's value:
    ///
    /// ```php
    /// list(y|>y)=y;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Can't use function return value in write context in %s on line 2
    /// ```
    ///
    /// a variable, an element, a property, a `static` property or a call.
    pub(super) fn is_variable_or_call(&self, expr: tree::Expr) -> bool {
        match self.peel(expr) {
            tree::Expr::SimpleVarExpr(_)
            | tree::Expr::VarVarExpr(_)
            | tree::Expr::DynamicVarExpr(_)
            | tree::Expr::SubscriptExpr(_)
            | tree::Expr::MemberAccessExpr(_)
            | tree::Expr::CallExpr(_) => true,
            tree::Expr::ScopedAccessExpr(id) => self.names_property(id),
            // `zend_is_call`'s `ZEND_AST_PIPE`.
            tree::Expr::BinaryExpr(_) => self.is_call(expr),
            _ => false,
        }
    }

    /// `zend_compile_assign` over any target (`Z/Zend/zend_compile.c:3452`):
    /// the target decoded, and the arm of its kind.
    pub(super) fn assign_to(
        &mut self,
        target: tree::Expr,
        value: Source,
    ) -> Result<Where, CompileError> {
        match self.target(target)? {
            Target::Cv(name) => self.assign(name, value),
            Target::Dim(dim) => self.assign_dim(dim, value),
            Target::Prop { object, name } => self.assign_prop(object, name, value),
            Target::StaticProp { class, name } => self.assign_static_prop(class, &name, value),
            Target::ByName(target) => self.assign_by_name(target, value),
            Target::List(list) => match value {
                Source::Expr(value) => self.list_assign_expr(&list, value),
                Source::Node(value) => self
                    .list_assign(&list, value, true)
                    .map(|held| held.unwrap_or(value)),
            },
        }
    }
}
