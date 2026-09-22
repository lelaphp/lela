//! The walk over an expression node (design §4.9, #595): what [`Emit`]
//! reads each of the tree's expression kinds as, in the shape
//! [`crate::read`] gave the emitter, one `Child` per node at the node's
//! own line, php's `zend_ast.lineno`.
//!
//! A scalar's value is [`TokenValue::of`], php's scanner's reading of
//! the token, and nothing here spells a radix or an escape. A node the
//! emitter cannot emit yet is refused where it stands, in the one-line
//! shape the phpt lane counts as unread. The walk recurses on the tree,
//! one native frame per node, so the stack limit is checked at every
//! node as `zend_compile_expr` checks it (design §5.6): the chain
//! `stack.rs`'s witness refuses is refused here at the same line.

use super::{Emit, refused, unread};
use crate::CompileError;
use crate::read::{
    Arm, BinaryOp, CastTo, Child, Closure, Element, Expr as Ir, Function, IncludeKind, Magic,
    Piece, Stmt, Use, Written, is_auto_global,
};
use php_syntax::tree::{
    Arg, ArgList, ArrowFnExpr, AssignExpr, CallExpr, ClosureExpr, Expr, MemberAccessExpr,
    MemberSelector, Node, NodeId, ScopedAccessExpr, ScopedSelector, SimpleDim,
    SimpleInterpolationSuffix, StringPart, TokenId,
};
use php_syntax::{Holder, SyntaxKind, TextRange, TokenValue, cast_name};

impl Emit<'_> {
    /// One expression node as the emitter reads it.
    ///
    /// `expr instanceof C` is `expr T_INSTANCEOF class_name_reference`
    /// with no associativity declared, so a second `instanceof` reads
    /// the first as its left side, php's `($o instanceof E) instanceof
    /// F`, which the grammar groups to the left as this walk reads it;
    /// the oracle at 8.5.9 compiles it to two `INSTANCEOF`s, the second
    /// over the first's temporary, and a bool on the left is no object,
    /// so the chain is `false`. The class is a name as written; `self`,
    /// `parent` and `static` are the emitter's to refuse, and a class
    /// held in a variable is not read.
    ///
    /// ```php
    /// $o = 1;
    /// var_dump($o instanceof E instanceof F);
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    #[allow(clippy::too_many_lines)]
    pub(super) fn walk_expr(&mut self, expr: Expr) -> Result<Child, CompileError> {
        let line = expr.line(self.parsed);
        self.stack.check(line)?;
        let tree = &self.parsed.tree;
        let ir = match expr {
            Expr::LiteralExpr(id) => {
                let node = &tree[id];
                let Some(token) = node.value else {
                    return Err(unread(line, "a literal"));
                };
                match TokenValue::of(self.parsed, Holder::Scalar(node), token) {
                    Some(TokenValue::Int(n)) => Ir::Int(n),
                    Some(TokenValue::Float(f)) => Ir::Float(f),
                    Some(TokenValue::Bytes(bytes)) => Ir::Str(bytes),
                    None => return Err(unread(line, "a literal")),
                }
            }
            Expr::MagicConstExpr(id) => {
                let Some(which) = tree[id].token.and_then(|token| Magic::of(self.text(token)))
                else {
                    return Err(unread(line, "a magic constant"));
                };
                Ir::Magic { which, line }
            }
            Expr::NameRef(token) => {
                let name = self.text(token);
                match name.to_ascii_lowercase().as_slice() {
                    b"true" => Ir::Bool(true),
                    b"false" => Ir::Bool(false),
                    b"null" => Ir::Null,
                    _ => Ir::Constant(name.to_vec()),
                }
            }
            Expr::SimpleVarExpr(id) => {
                let Some(var) = tree[id].var else {
                    return Err(unread(line, "a variable"));
                };
                self.simple_var(var, line)?
            }
            Expr::VarVarExpr(id) => {
                Ir::VarVar(self.walk_inner(tree[id].inner, line, "a variable")?)
            }
            Expr::DynamicVarExpr(id) => {
                Ir::VarVar(self.walk_inner(tree[id].inner, line, "a variable")?)
            }
            Expr::ParenExpr(id) => {
                let Some(inner) = tree[id].inner else {
                    return Err(unread(line, "an expression inside parentheses"));
                };
                let mut inner = self.walk_expr(inner)?;
                if let Ir::Ternary { parenthesised, .. } = &mut *inner {
                    *parenthesised = true;
                }
                return Ok(inner);
            }
            Expr::UnaryExpr(id) => {
                let node = &tree[id];
                let operand = self.walk_inner(node.operand, line, "a unary operator")?;
                match node.op.map(|op| self.kind(op)) {
                    Some(SyntaxKind::MINUS) => Ir::Unary {
                        minus: true,
                        operand,
                    },
                    Some(SyntaxKind::PLUS) => Ir::Unary {
                        minus: false,
                        operand,
                    },
                    Some(SyntaxKind::BANG) => Ir::Not(operand),
                    Some(SyntaxKind::TILDE) => Ir::BwNot(operand),
                    _ => return Err(unread(line, "a unary operator")),
                }
            }
            Expr::PrefixIncDecExpr(id) => {
                let node = &tree[id];
                let up = node.op.map(|op| self.kind(op)) == Some(SyntaxKind::PLUS_PLUS);
                let target = self.walk_inner(node.operand, line, "a `++`")?;
                step(target, up, true)?
            }
            Expr::PostfixIncDecExpr(id) => {
                let node = &tree[id];
                let up = node.op.map(|op| self.kind(op)) == Some(SyntaxKind::PLUS_PLUS);
                let target = self.walk_expr(node.operand)?;
                step(target, up, false)?
            }
            Expr::CastExpr(id) => {
                let node = &tree[id];
                let to = match node.op.map(|op| cast_name(self.text(op))) {
                    Some("T_INT_CAST") => CastTo::Long,
                    Some("T_DOUBLE_CAST") => CastTo::Double,
                    Some("T_STRING_CAST") => CastTo::Str,
                    Some("T_ARRAY_CAST") => CastTo::Array,
                    Some("T_OBJECT_CAST") => CastTo::Object,
                    Some("T_BOOL_CAST") => CastTo::Bool,
                    Some("T_UNSET_CAST") => {
                        return Err(refused(line, "The (unset) cast is no longer supported"));
                    }
                    _ => return Err(unread(line, "a cast")),
                };
                let operand = self.walk_inner(node.operand, line, "a cast")?;
                Ir::Cast { to, operand }
            }
            Expr::ErrorSuppressExpr(id) => {
                Ir::Silence(self.walk_inner(tree[id].operand, line, "an `@`")?)
            }
            Expr::CloneExpr(id) => {
                let node = &tree[id];
                let operand = match (node.operand, node.args) {
                    (Some(operand), _) => self.walk_expr(operand)?,
                    (None, Some(args)) => {
                        let mut arguments = self.walk_arguments(args, line)?;
                        if arguments.len() != 1 {
                            return Err(unread(line, "a `clone` with an argument list"));
                        }
                        arguments.remove(0)
                    }
                    (None, None) => return Err(unread(line, "a `clone`")),
                };
                Ir::Call {
                    name: b"\\clone".to_vec(),
                    arguments: vec![operand],
                }
            }
            Expr::PrintExpr(_) => return Err(unread(line, "a `print`")),
            Expr::ThrowExpr(id) => {
                Ir::Throw(self.walk_inner(tree[id].operand, line, "a `throw`")?)
            }
            Expr::BinaryExpr(id) => {
                let node = &tree[id];
                let left = self.walk_expr(node.lhs)?;
                let right = self.walk_inner(node.rhs, line, "a binary operator")?;
                match node.op.map(|op| self.kind(op)) {
                    Some(SyntaxKind::AMP_AMP | SyntaxKind::AND_KW) => Ir::And { left, right },
                    Some(SyntaxKind::PIPE_PIPE | SyntaxKind::OR_KW) => Ir::Or { left, right },
                    Some(SyntaxKind::QUESTION_QUESTION) => Ir::Coalesce { left, right },
                    Some(kind) => match binary_op(kind) {
                        Some(op) => Ir::Binary { op, left, right },
                        None => return Err(unread(line, "a binary operator")),
                    },
                    None => return Err(unread(line, "a binary operator")),
                }
            }
            Expr::InstanceofExpr(id) => {
                let node = &tree[id];
                let Some(Expr::NameRef(class)) = node.rhs else {
                    return Err(unread(line, "an `instanceof` of what is no class name"));
                };
                let class = self.text(class).to_vec();
                Ir::Instanceof {
                    expr: self.walk_expr(node.lhs)?,
                    class,
                }
            }
            Expr::AssignExpr(id) => self.walk_assignment(id, line)?,
            Expr::TernaryExpr(id) => {
                let node = &tree[id];
                let cond = self.walk_expr(node.cond)?;
                let then = match node.then {
                    Some(then) => Some(self.walk_expr(then)?),
                    None => None,
                };
                let otherwise = self.walk_inner(node.else_branch, line, "a `?`")?;
                Ir::Ternary {
                    cond,
                    then,
                    otherwise,
                    parenthesised: false,
                }
            }
            Expr::MatchExpr(id) => {
                let node = tree[id].clone();
                let expr = self.walk_inner(node.subject, line, "a `match`")?;
                let mut arms = Vec::with_capacity(node.match_arms.len());
                for arm in &node.match_arms {
                    let arm = self.parsed.tree[*arm].clone();
                    let conds = match arm.conds {
                        Some(conds) => {
                            let exprs = self.parsed.tree[conds].exprs.clone();
                            Some(self.walk_all(&exprs)?)
                        }
                        None => None,
                    };
                    let body = self.walk_inner(arm.body, arm.line(), "a `match` arm")?;
                    arms.push(Arm { conds, body });
                }
                Ir::Match { expr, arms }
            }
            Expr::NewExpr(id) => {
                let node = &tree[id];
                if node.anon.is_some() {
                    return Err(unread(line, "an anonymous class"));
                }
                let Some(Expr::NameRef(class)) = node.ctor else {
                    return Err(unread(line, "a `new` of what is no class name"));
                };
                let class = self.text(class).to_vec();
                let arguments = match node.args {
                    Some(args) => self.walk_arguments(args, line)?,
                    None => Vec::new(),
                };
                Ir::New { class, arguments }
            }
            Expr::ExitExpr(id) => {
                let arguments = match tree[id].args {
                    Some(args) => self.walk_arguments(args, line)?,
                    None => Vec::new(),
                };
                Ir::Call {
                    name: b"\\exit".to_vec(),
                    arguments,
                }
            }
            Expr::YieldExpr(_) | Expr::YieldFromExpr(_) => {
                return Err(unread(line, "a `yield`"));
            }
            Expr::IncludeExpr(id) => {
                let node = &tree[id];
                let kind = match node.op.map(|op| self.kind(op)) {
                    Some(SyntaxKind::INCLUDE_KW) => IncludeKind::Include,
                    Some(SyntaxKind::INCLUDE_ONCE_KW) => IncludeKind::IncludeOnce,
                    Some(SyntaxKind::REQUIRE_KW) => IncludeKind::Require,
                    Some(SyntaxKind::REQUIRE_ONCE_KW) => IncludeKind::RequireOnce,
                    _ => return Err(unread(line, "an `include`")),
                };
                let path = self.walk_inner(node.operand, line, "an `include`")?;
                Ir::Include { kind, path }
            }
            Expr::IssetExpr(id) => {
                let targets = tree[id].targets.clone();
                let [target] = targets[..] else {
                    return Err(unread(line, "an `isset` over more than one variable"));
                };
                self.walk_isset(target, false)?
            }
            Expr::EmptyExpr(id) => {
                let Some(target) = tree[id].target else {
                    return Err(unread(line, "an `empty`"));
                };
                self.walk_isset(target, true)?
            }
            Expr::EvalExpr(_) => return Err(unread(line, "an `eval`")),
            Expr::CallExpr(id) => self.walk_call(id, line)?,
            Expr::MemberAccessExpr(id) => {
                let (object, name) = self.walk_member_access(id, line)?;
                Ir::Prop { object, name }
            }
            Expr::ScopedAccessExpr(id) => self.walk_scoped_access(id, line)?,
            Expr::SubscriptExpr(id) => {
                let node = &tree[id];
                let container = self.walk_expr(node.base)?;
                let index = match node.index {
                    Some(index) => Some(self.walk_expr(index)?),
                    None => None,
                };
                Ir::Dim { container, index }
            }
            Expr::ArrayExpr(id) => {
                let elements = tree[id].array_elements.clone();
                let mut out = Vec::with_capacity(elements.len());
                for element in &elements {
                    let element = self.parsed.tree[*element].clone();
                    if element.spread.is_some() || element.byref.is_some() {
                        return Err(unread(element.line(), "an array expression"));
                    }
                    let Some(value) = element.value else {
                        return Err(unread(element.line(), "an array expression"));
                    };
                    let key = match element.key {
                        Some(key) => Some(self.walk_expr(self.parsed.tree[key].key)?),
                        None => None,
                    };
                    out.push(Element {
                        key,
                        value: self.walk_expr(value)?,
                    });
                }
                Ir::Array(out)
            }
            Expr::ListExpr(_) => return Err(unread(line, "a `list`")),
            Expr::ClosureExpr(id) => self.walk_closure(id, line)?,
            Expr::ArrowFnExpr(id) => self.walk_arrow_fn(id, line)?,
            Expr::InterpolatedStringExpr(id) => {
                let parts = tree[id].parts.clone();
                self.walk_interpolated(&parts, Holder::DoubleQuoted(&self.parsed.tree[id]), line)?
            }
            Expr::HeredocExpr(id) => {
                let parts = tree[id].parts.clone();
                self.walk_interpolated(&parts, Holder::Heredoc(&self.parsed.tree[id]), line)?
            }
            Expr::NowdocExpr(id) => {
                let node = &tree[id];
                let mut bytes = Vec::new();
                for token in &node.text {
                    if let Some(TokenValue::Bytes(text)) =
                        TokenValue::of(self.parsed, Holder::Nowdoc(node), *token)
                    {
                        bytes.extend(text);
                    }
                }
                Ir::Str(bytes)
            }
            Expr::ShellExecExpr(_) => return Err(unread(line, "a backtick operator")),
        };
        Ok(Child::at(line, ir))
    }

    /// Every expression of a list, in order.
    pub(super) fn walk_all(&mut self, exprs: &[Expr]) -> Result<Vec<Child>, CompileError> {
        exprs.iter().map(|expr| self.walk_expr(*expr)).collect()
    }

    /// A child the grammar requires, refused as `what` where recovery
    /// left none.
    fn walk_inner(
        &mut self,
        expr: Option<Expr>,
        line: u32,
        what: &str,
    ) -> Result<Child, CompileError> {
        match expr {
            Some(expr) => self.walk_expr(expr),
            None => Err(unread(line, what)),
        }
    }

    /// The tree's kind of a token.
    fn kind(&self, token: TokenId) -> SyntaxKind {
        self.parsed
            .tokens
            .get(token.0 as usize)
            .map_or(SyntaxKind::ERROR, |token| token.kind)
    }

    /// The kind of the first significant token inside `range`.
    fn first_kind(&self, range: TextRange) -> Option<SyntaxKind> {
        let at = self
            .parsed
            .tokens
            .partition_point(|token| token.offset < range.start);
        self.parsed.tokens[at..]
            .iter()
            .take_while(|token| token.offset < range.end())
            .map(|token| token.kind)
            .find(|kind| !kind.is_trivia())
    }

    /// The line of the first significant token after `range`, or the
    /// last line where none follows: where an arrow function's
    /// `end_lineno` stands (#601 row 18).
    fn line_after(&self, range: TextRange) -> u32 {
        let at = self
            .parsed
            .tokens
            .partition_point(|token| token.offset < range.end());
        self.parsed.tokens[at..]
            .iter()
            .find(|token| !token.kind.is_trivia())
            .map_or_else(
                || self.parsed.lines.line_number(self.parsed.lines.len()),
                |token| self.parsed.lines.line_number(token.offset),
            )
    }

    /// `$name`: the CV, `$this`, and an auto-global refused as the
    /// reader refused it.
    fn simple_var(&self, var: TokenId, line: u32) -> Result<Ir, CompileError> {
        let name = self.variable_name(var);
        if name == b"this" {
            return Ok(Ir::This);
        }
        if is_auto_global(&name) {
            return Err(unread(line, "a variable"));
        }
        Ok(Ir::Var(name))
    }

    /// The arguments of a call, each a plain expression; a named or an
    /// unpacked argument, and the `...` of a first-class callable, are
    /// refused.
    fn walk_arguments(
        &mut self,
        id: NodeId<ArgList>,
        line: u32,
    ) -> Result<Vec<Child>, CompileError> {
        let args = self.parsed.tree[id].args.clone();
        let mut out = Vec::with_capacity(args.len());
        for arg in &args {
            match arg {
                Arg::Expr(expr) => out.push(self.walk_expr(*expr)?),
                Arg::NamedArg(_) => return Err(unread(line, "a named argument")),
                Arg::SpreadArg(_) => return Err(unread(line, "an unpacked argument")),
                Arg::VariadicPlaceholder(_) => {
                    return Err(unread(line, "a first-class callable"));
                }
            }
        }
        Ok(out)
    }

    /// `zend_compile_assign`, `zend_compile_assign_ref` and
    /// `zend_compile_compound_assign` by the target's shape and the
    /// operator: a variable, an element, a property or a class's property.
    fn walk_assignment(&mut self, id: NodeId<AssignExpr>, line: u32) -> Result<Ir, CompileError> {
        let node = self.parsed.tree[id].clone();
        let op = node.op.map(|op| self.kind(op));
        let target = self.walk_expr(node.lhs)?;
        let Some(value) = node.rhs else {
            return Err(unread(line, "an assignment"));
        };
        if node.byref.is_some() {
            if op != Some(SyntaxKind::EQ) {
                return Err(unread(line, "a by-reference compound assignment"));
            }
            if !matches!(
                &*target,
                Ir::Var(_) | Ir::Dim { .. } | Ir::StaticProp { .. }
            ) {
                return Err(unread(
                    line,
                    "a by-reference assignment to what is no variable",
                ));
            }
            let source = self.walk_expr(value)?;
            if !matches!(&*source, Ir::Var(_) | Ir::Call { .. }) {
                return Err(unread(
                    line,
                    "a by-reference assignment from what is no variable",
                ));
            }
            return Ok(Ir::AssignRef { target, source });
        }
        if op == Some(SyntaxKind::QUESTION_QUESTION_EQ) {
            let Ir::Var(var) = target.take() else {
                return Err(unread(line, "a `??=` over what is no variable"));
            };
            let value = self.walk_expr(value)?;
            return Ok(Ir::AssignCoalesce { var, value });
        }
        if op == Some(SyntaxKind::EQ) {
            let ir = match target.take() {
                Ir::Var(var) => Ir::Assign {
                    var,
                    value: self.walk_expr(value)?,
                },
                dim @ Ir::Dim { .. } => Ir::AssignDim {
                    target: Child::at(line, dim),
                    value: self.walk_expr(value)?,
                },
                Ir::Prop { object, name } => Ir::AssignProp {
                    object,
                    name,
                    value: self.walk_expr(value)?,
                },
                Ir::StaticProp { class, name } => Ir::AssignStaticProp {
                    class,
                    name,
                    value: self.walk_expr(value)?,
                },
                _ => return Err(unread(line, "an assignment to what is no variable")),
            };
            return Ok(ir);
        }
        let Some(op) = op.and_then(compound_op) else {
            return Err(unread(line, "an assignment operator"));
        };
        Ok(match target.take() {
            Ir::Var(var) => Ir::AssignOp {
                var,
                op,
                value: self.walk_expr(value)?,
            },
            Ir::StaticProp { class, name } => Ir::AssignStaticPropOp {
                class,
                name,
                op,
                value: self.walk_expr(value)?,
            },
            dim @ Ir::Dim { .. } => Ir::AssignDimOp {
                target: Child::at(line, dim),
                op,
                value: self.walk_expr(value)?,
            },
            Ir::Prop { object, name } => Ir::AssignObjOp {
                object,
                name,
                op,
                value: self.walk_expr(value)?,
            },
            _ => {
                return Err(unread(line, "a compound assignment to what is no variable"));
            }
        })
    }

    /// `isset($v)` and `empty($v)` over a variable, an element or a
    /// property chain rooted at one, `Isset` with `empty` telling the
    /// two apart; `$this` and anything else are refused as the reader
    /// refused them.
    fn walk_isset(&mut self, target: Expr, empty: bool) -> Result<Ir, CompileError> {
        let word = if empty { "empty" } else { "isset" };
        let line = target.line(self.parsed);
        let var = self.walk_expr(target)?;
        let mut base = &var;
        loop {
            match &**base {
                Ir::Dim { container, .. } => base = container,
                Ir::Prop { object, .. } => base = object,
                Ir::Var(_) => break,
                Ir::This => {
                    return Err(unread(
                        line,
                        &format!("an `{word}` over $this, whose opcode is `ISSET_ISEMPTY_THIS`,"),
                    ));
                }
                _ => {
                    return Err(unread(
                        line,
                        &format!("an `{word}` over what is no variable"),
                    ));
                }
            }
        }
        Ok(Ir::Isset { var, empty })
    }

    /// A call by the callee's shape: a named function, a class's method
    /// through `::`, an object's method, or a variable holding a callable.
    fn walk_call(&mut self, id: NodeId<CallExpr>, line: u32) -> Result<Ir, CompileError> {
        let node = self.parsed.tree[id].clone();
        Ok(match node.callee {
            Expr::NameRef(name) => {
                let name = self.text(name).to_vec();
                let arguments = self.walk_arguments(node.args, line)?;
                Ir::Call { name, arguments }
            }
            Expr::ScopedAccessExpr(scoped) => {
                let scoped = self.parsed.tree[scoped].clone();
                let Expr::NameRef(class) = scoped.scope else {
                    return Err(unread(line, "a static call on what is no class name"));
                };
                let Some(ScopedSelector::MemberName(method)) = scoped.member else {
                    return Err(unread(
                        line,
                        "a static call whose method is named at run time",
                    ));
                };
                let Some(method) = self.parsed.tree[method].name else {
                    return Err(unread(line, "a static call"));
                };
                let class = self.text(class).to_vec();
                let method = self.text(method).to_vec();
                let arguments = self.walk_arguments(node.args, line)?;
                Ir::StaticCall {
                    class,
                    method,
                    arguments,
                }
            }
            Expr::MemberAccessExpr(access) => {
                let (object, method) = self.walk_member_access(access, line)?;
                let arguments = self.walk_arguments(node.args, line)?;
                Ir::MethodCall {
                    object,
                    method,
                    arguments,
                }
            }
            callee @ (Expr::SimpleVarExpr(_) | Expr::CallExpr(_)) => {
                let callee = self.walk_expr(callee)?;
                if !matches!(&*callee, Ir::Var(_) | Ir::CallVar { .. }) {
                    return Err(unread(line, "a call of what is no variable"));
                }
                let arguments = self.walk_arguments(node.args, line)?;
                Ir::CallVar { callee, arguments }
            }
            _ => return Err(unread(line, "a call of what is no name")),
        })
    }

    /// `$o->name`: the receiver and the property's name as written; a
    /// nullsafe `?->` and a name the program computes are refused.
    fn walk_member_access(
        &mut self,
        id: NodeId<MemberAccessExpr>,
        line: u32,
    ) -> Result<(Child, Vec<u8>), CompileError> {
        let node = self.parsed.tree[id].clone();
        if node.op.map(|op| self.kind(op)) == Some(SyntaxKind::NULLSAFE_ARROW) {
            return Err(unread(line, "a nullsafe property fetch"));
        }
        let Some(MemberSelector::MemberName(member)) = node.member else {
            return Err(unread(line, "a property named at run time"));
        };
        let Some(name) = self.parsed.tree[member].name else {
            return Err(unread(line, "a property fetch"));
        };
        let name = self.text(name).to_vec();
        Ok((self.walk_expr(node.obj)?, name))
    }

    /// `C::NAME` and `C::$name`: a class constant or a class's property
    /// (`ZEND_AST_STATIC_PROP`), the class named as written; `::class` and a scope that is no name
    /// are refused.
    fn walk_scoped_access(
        &mut self,
        id: NodeId<ScopedAccessExpr>,
        line: u32,
    ) -> Result<Ir, CompileError> {
        let node = self.parsed.tree[id].clone();
        let Expr::NameRef(class) = node.scope else {
            return Err(unread(line, "a class reference of the scope"));
        };
        let class = self.text(class).to_vec();
        Ok(match node.member {
            Some(ScopedSelector::MemberName(member)) => {
                let Some(name) = self.parsed.tree[member].name else {
                    return Err(unread(line, "a class constant"));
                };
                Ir::ClassConst {
                    class,
                    name: self.text(name).to_vec(),
                }
            }
            Some(ScopedSelector::SimpleVarExpr(var)) => {
                let Some(var) = self.parsed.tree[var].var else {
                    return Err(unread(line, "a static property"));
                };
                Ir::StaticProp {
                    class,
                    name: self.variable_name(var),
                }
            }
            Some(ScopedSelector::ClassKw(_)) => return Err(unread(line, "a `::class`")),
            Some(ScopedSelector::VarVarExpr(_) | ScopedSelector::DynamicVarExpr(_)) => {
                return Err(unread(line, "a static property named at run time"));
            }
            _ => return Err(unread(line, "a class member named at run time")),
        })
    }

    /// `function (…) use (…) { … }`, php's `ZEND_AST_CLOSURE`: the
    /// signature and body as a function's, the `use` list, and whether
    /// `static` stands before the keyword.
    fn walk_closure(&mut self, id: NodeId<ClosureExpr>, line: u32) -> Result<Ir, CompileError> {
        let node = self.parsed.tree[id].clone();
        if !node.attrs.is_empty() {
            return Err(unread(line, "an attribute before a closure"));
        }
        let is_static = self.first_kind(node.range) == Some(SyntaxKind::STATIC_KW);
        let mut lexical = Vec::new();
        if let Some(clause) = node.uses {
            for id in &self.parsed.tree[clause].closure_use_vars.clone() {
                let used = &self.parsed.tree[*id];
                lexical.push(Use {
                    name: self.variable_name(used.var),
                    by_ref: used.byref.is_some(),
                    line: used.line(),
                });
            }
        }
        let function = self.walk_function(
            Vec::new(),
            node.byref.is_some(),
            node.params,
            node.ret,
            node.body,
            node.doc_comment,
            line,
            node.range,
        )?;
        Ok(Ir::Closure(Box::new(Closure {
            function,
            uses: lexical,
            is_static,
            is_arrow: false,
        })))
    }

    /// `fn (…) => expr`, php's `ZEND_AST_ARROW_FUNC`: a closure whose
    /// body is the one `return` php makes of the expression
    /// (`zend_compile_func_decl`), and whose end line is the token after
    /// the body's (#601 row 18).
    fn walk_arrow_fn(&mut self, id: NodeId<ArrowFnExpr>, line: u32) -> Result<Ir, CompileError> {
        let node = self.parsed.tree[id].clone();
        if !node.attrs.is_empty() {
            return Err(unread(line, "an attribute before an arrow function"));
        }
        let is_static = self.first_kind(node.range) == Some(SyntaxKind::STATIC_KW);
        let parameters = self.walk_parameters(node.params)?;
        let return_type = match node.ret {
            Some(ret) => match self.parsed.tree[ret].ty {
                Some(ty) => Some(self.walk_type(ty, line)?),
                None => return Err(unread(line, "a return type")),
            },
            None => None,
        };
        let Some(body) = node.body else {
            return Err(unread(line, "an arrow function with no body"));
        };
        let returned = body.line(self.parsed);
        let value = self.walk_expr(body)?;
        Ok(Ir::Closure(Box::new(Closure {
            function: Function {
                name: Vec::new(),
                parameters,
                return_type,
                by_ref: node.byref.is_some(),
                body: vec![(Stmt::Return(Some(value)), returned)],
                doc_comment: self.doc_comment(node.doc_comment),
                line,
                end_line: self.line_after(node.range),
            },
            uses: Vec::new(),
            is_static,
            is_arrow: true,
        })))
    }

    /// A `"…"` or a heredoc: one string where nothing is interpolated,
    /// php's `ZEND_AST_ENCAPS_LIST` otherwise, its written and variable
    /// pieces in order, each variable piece with how it was written.
    fn walk_interpolated(
        &mut self,
        parts: &[StringPart],
        holder: Holder<'_>,
        line: u32,
    ) -> Result<Ir, CompileError> {
        let mut pieces: Vec<Piece> = Vec::new();
        let mut bytes = Vec::new();
        let mut interpolated = false;
        for part in parts {
            match *part {
                StringPart::LiteralStringPart(id) => {
                    let node = &self.parsed.tree[id];
                    if let Some(TokenValue::Bytes(text)) = node
                        .text
                        .and_then(|text| TokenValue::of(self.parsed, holder, text))
                    {
                        bytes.extend(text);
                    }
                }
                StringPart::SimpleInterpolation(id) => {
                    let node = self.parsed.tree[id].clone();
                    let var_line = node.line();
                    let var = Child::at(var_line, self.simple_var(node.var, var_line)?);
                    let expr = match node.suffix {
                        None => var,
                        Some(SimpleInterpolationSuffix::SimpleDim(dim)) => Child::at(
                            var_line,
                            Ir::Dim {
                                container: var,
                                index: Some(self.walk_simple_dim(dim)?),
                            },
                        ),
                        Some(SimpleInterpolationSuffix::SimpleProp(prop)) => {
                            let prop = &self.parsed.tree[prop];
                            if prop.op.map(|op| self.kind(op)) == Some(SyntaxKind::NULLSAFE_ARROW) {
                                return Err(unread(var_line, "a nullsafe property in a string"));
                            }
                            let name = self.text(prop.name).to_vec();
                            Child::at(var_line, Ir::Prop { object: var, name })
                        }
                    };
                    piece(&mut pieces, &mut bytes, expr, Written::Plain);
                    interpolated = true;
                }
                StringPart::BracedInterpolation(id) => {
                    let node = &self.parsed.tree[id];
                    let expr =
                        self.walk_inner(node.inner, node.line(), "an interpolated expression")?;
                    piece(&mut pieces, &mut bytes, expr, Written::Plain);
                    interpolated = true;
                }
                StringPart::DollarBraceInterpolation(id) => {
                    let node = self.parsed.tree[id].clone();
                    let at = node.line();
                    let (expr, written) = match (node.name, node.inner) {
                        (Some(name), _) => {
                            let var = Child::at(at, named_var(self.text(name).to_vec(), at)?);
                            let expr = match node.dim {
                                Some(dim) => Child::at(
                                    at,
                                    Ir::Dim {
                                        container: var,
                                        index: Some(self.walk_simple_dim(dim)?),
                                    },
                                ),
                                None => var,
                            };
                            (expr, Written::DollarCurly)
                        }
                        (None, Some(inner)) => (
                            Child::at(at, Ir::VarVar(self.walk_expr(inner)?)),
                            Written::DollarCurlyVarVar,
                        ),
                        (None, None) => {
                            return Err(unread(at, "an interpolated expression"));
                        }
                    };
                    piece(&mut pieces, &mut bytes, expr, written);
                    interpolated = true;
                }
            }
        }
        if !interpolated {
            return Ok(Ir::Str(bytes));
        }
        if !bytes.is_empty() {
            pieces.push(Piece {
                expr: Child::at(line, Ir::Str(bytes)),
                written: Written::Plain,
            });
        }
        Ok(Ir::Encaps(pieces))
    }

    /// The `[…]` after a variable inside a string, php's
    /// `encaps_var_offset`: a bare word is the string, a number is
    /// `T_NUM_STRING`'s value ([`Holder::Offset`]) and `-` before it is
    /// `zend_negate_num_string`, a variable is itself.
    fn walk_simple_dim(&self, id: NodeId<SimpleDim>) -> Result<Child, CompileError> {
        let node = &self.parsed.tree[id];
        let line = node.line();
        let Some(index) = node.index else {
            return Err(unread(line, "a dimension in a string"));
        };
        let ir = match self.kind(index) {
            SyntaxKind::VARIABLE => self.simple_var(index, line)?,
            SyntaxKind::IDENT => Ir::Str(self.text(index).to_vec()),
            SyntaxKind::ENCAPS_INT | SyntaxKind::INT => {
                let value = TokenValue::of(self.parsed, Holder::Offset(node), index);
                match (value, node.neg.is_some()) {
                    (Some(TokenValue::Int(0)), true) => Ir::Str(b"-0".to_vec()),
                    (Some(TokenValue::Int(n)), true) => Ir::Int(-n),
                    (Some(TokenValue::Int(n)), false) => Ir::Int(n),
                    (Some(TokenValue::Bytes(digits)), true) => {
                        let mut bytes = vec![b'-'];
                        bytes.extend(digits);
                        Ir::Str(bytes)
                    }
                    (Some(TokenValue::Bytes(digits)), false) => Ir::Str(digits),
                    _ => return Err(unread(line, "a dimension in a string")),
                }
            }
            _ => return Err(unread(line, "a dimension in a string")),
        };
        Ok(Child::at(line, ir))
    }
}

/// One variable piece of an interpolated string, after the written
/// bytes before it, if any, as a piece of their own at the variable's
/// line.
fn piece(pieces: &mut Vec<Piece>, bytes: &mut Vec<u8>, expr: Child, written: Written) {
    if !bytes.is_empty() {
        pieces.push(Piece {
            expr: Child::at(expr.line(), Ir::Str(std::mem::take(bytes))),
            written: Written::Plain,
        });
    }
    pieces.push(Piece { expr, written });
}

/// The binary operator a token is, php's `zend_compile_binary_op`
/// opcode by the token; none for `|>`, which the emitter does not read.
fn binary_op(kind: SyntaxKind) -> Option<BinaryOp> {
    Some(match kind {
        SyntaxKind::PLUS => BinaryOp::Add,
        SyntaxKind::MINUS => BinaryOp::Sub,
        SyntaxKind::STAR => BinaryOp::Mul,
        SyntaxKind::SLASH => BinaryOp::Div,
        SyntaxKind::PERCENT => BinaryOp::Mod,
        SyntaxKind::STAR_STAR => BinaryOp::Pow,
        SyntaxKind::SHL => BinaryOp::Sl,
        SyntaxKind::SHR => BinaryOp::Sr,
        SyntaxKind::DOT => BinaryOp::Concat,
        SyntaxKind::LT => BinaryOp::IsSmaller,
        SyntaxKind::LT_EQ => BinaryOp::IsSmallerOrEqual,
        SyntaxKind::GT => BinaryOp::Greater,
        SyntaxKind::GT_EQ => BinaryOp::GreaterOrEqual,
        SyntaxKind::EQ_EQ => BinaryOp::IsEqual,
        SyntaxKind::BANG_EQ | SyntaxKind::LT_GT => BinaryOp::IsNotEqual,
        SyntaxKind::EQ_EQ_EQ => BinaryOp::IsIdentical,
        SyntaxKind::BANG_EQ_EQ => BinaryOp::IsNotIdentical,
        SyntaxKind::SPACESHIP => BinaryOp::Spaceship,
        SyntaxKind::PIPE => BinaryOp::BwOr,
        SyntaxKind::CARET => BinaryOp::BwXor,
        SyntaxKind::AMP => BinaryOp::BwAnd,
        SyntaxKind::XOR_KW => BinaryOp::BoolXor,
        _ => return None,
    })
}

/// The operator of a compound assignment, `+=` and its kin.
fn compound_op(kind: SyntaxKind) -> Option<BinaryOp> {
    Some(match kind {
        SyntaxKind::PLUS_EQ => BinaryOp::Add,
        SyntaxKind::MINUS_EQ => BinaryOp::Sub,
        SyntaxKind::STAR_EQ => BinaryOp::Mul,
        SyntaxKind::SLASH_EQ => BinaryOp::Div,
        SyntaxKind::PERCENT_EQ => BinaryOp::Mod,
        SyntaxKind::STAR_STAR_EQ => BinaryOp::Pow,
        SyntaxKind::SHL_EQ => BinaryOp::Sl,
        SyntaxKind::SHR_EQ => BinaryOp::Sr,
        SyntaxKind::DOT_EQ => BinaryOp::Concat,
        SyntaxKind::AMP_EQ => BinaryOp::BwAnd,
        SyntaxKind::PIPE_EQ => BinaryOp::BwOr,
        SyntaxKind::CARET_EQ => BinaryOp::BwXor,
        _ => return None,
    })
}

/// `++$a`, `$a--` and their kin over a variable or a class's property
/// (`ZEND_AST_STATIC_PROP`), php's four step nodes.
fn step(target: Child, up: bool, pre: bool) -> Result<Ir, CompileError> {
    let line = target.line();
    Ok(match target.take() {
        Ir::Var(name) => match (pre, up) {
            (true, true) => Ir::PreInc(name),
            (true, false) => Ir::PreDec(name),
            (false, true) => Ir::PostInc(name),
            (false, false) => Ir::PostDec(name),
        },
        Ir::StaticProp { class, name } => Ir::StaticPropStep {
            class,
            name,
            step: match (pre, up) {
                (true, true) => crate::read::Step::PreInc,
                (true, false) => crate::read::Step::PreDec,
                (false, true) => crate::read::Step::PostInc,
                (false, false) => crate::read::Step::PostDec,
            },
        },
        _ => {
            return Err(unread(
                line,
                if up {
                    "a `++` over what is no variable"
                } else {
                    "a `--` over what is no variable"
                },
            ));
        }
    })
}

/// A variable named inside `${…}`, by its name without a `$`.
fn named_var(name: Vec<u8>, line: u32) -> Result<Ir, CompileError> {
    if name == b"this" {
        return Ok(Ir::This);
    }
    if is_auto_global(&name) {
        return Err(unread(line, "a variable"));
    }
    Ok(Ir::Var(name))
}
