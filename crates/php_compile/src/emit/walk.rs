//! The walk over `php_syntax`'s tree (design §4.9): what [`Emit`]
//! reads a statement node as, and, in [`expr`], an expression node.
//!
//! `emit` walks the tree's `SourceFile`, every statement node in it,
//! every declaration a statement carries and every expression under
//! them, reading each fact where the tree holds it: a name from its
//! token, a modifier from its token, a type from its `TypeExpr`, a
//! scalar's value from [`php_syntax::TokenValue::of`], a doc comment
//! from the token index the builder recorded, a line from
//! [`Node::line`], which is php's `zend_ast.lineno`. What a node holds
//! is handed to the emitter in the shape [`crate::read`] gave it, so
//! every construct's semantics stays in the one function that had it
//! and nothing here decides what an opline is; each family of those
//! shapes goes as the emitter learns to read its nodes directly (#595).
//!
//! **What is refused.** A construct the tree parses and the emitter
//! cannot emit yet is refused here, at the node's line, in the one-line
//! `this compiler cannot read yet` shape at `Level::Parse`, which the
//! phpt lane counts as unread; what the grammar does not parse is php's
//! parse error from `Parsed::diagnostics`, raised before this walk runs.

use php_syntax::tree::{
    Block, CatchClause, ClassBody, ClassMember, Modifier, Node, NodeId, ParamList, Stmt, StmtList,
    TokenId, TypeExpr,
};
use php_syntax::{SyntaxKind, TextRange, tree};
use php_unit::Level;
use php_unit::decl::{ClassKind, Name, Type, Visibility};

mod expr;

use super::Emit;
use crate::CompileError;
use crate::read::{
    self, Attr, Case, Catch, Child, Class, ConstElem, Constant, Expr, Function, IfArm, Method,
    Parameter, Promoted, Property, Read, Stmt as Ir, Target, Unset,
};

/// A group of a member's modifiers as the tree holds them, read once:
/// php's `member_modifier` list before `zend_add_member_modifier`
/// folds it. The visibilities are read apart, since php refuses two of
/// either; the rest are the flags written.
#[derive(Default)]
struct Modifiers {
    visibility: Option<Visibility>,
    set_visibility: Option<Visibility>,
    flags: Vec<Flag>,
}

/// A modifier that is no visibility.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flag {
    Static,
    Abstract,
    Final,
    Readonly,
    Var,
}

impl Modifiers {
    fn has(&self, flag: Flag) -> bool {
        self.flags.contains(&flag)
    }

    /// Whether any of `flags` was written.
    fn any(&self, flags: &[Flag]) -> bool {
        flags.iter().any(|flag| self.has(*flag))
    }
}

impl Emit<'_> {
    /// The file's statements in order, `zend_compile_top_stmt` over
    /// the `SourceFile`'s list: each one read from the tree and emitted
    /// before the next is read, so a refusal stands where php's
    /// compiler would have stood when it raised.
    pub(super) fn source_file(&mut self) -> Result<(), CompileError> {
        let Some(root) = self.parsed.tree.root() else {
            return Ok(());
        };
        let Some(file) = root.id::<tree::SourceFile>() else {
            return Err(self.internal("a tree whose root is no SourceFile"));
        };
        if let Some(missing) = self.parsed.missing.first() {
            return Err(self.internal(format!(
                "a tree with a required child absent: {}",
                missing.field
            )));
        }
        self.depth = self.depth.saturating_add(1);
        for stmt in &self.parsed.tree[file].stmts {
            if let Some((statement, line)) = self.walk_stmt(*stmt)? {
                self.line = line;
                self.statement(&statement)?;
            }
        }
        self.depth = self.depth.saturating_sub(1);
        Ok(())
    }

    /// `InternalError`: the tree is not what the grammar promised
    /// (design §10).
    fn internal(&self, what: impl AsRef<str>) -> CompileError {
        CompileError {
            level: Level::Error,
            message: format!("InternalError: {}", what.as_ref()),
            line: self.line,
            raised: Vec::new(),
        }
    }

    /// A token's text in the source.
    fn text(&self, id: TokenId) -> &[u8] {
        let Some(token) = self.parsed.tokens.get(id.0 as usize) else {
            return &[];
        };
        self.parsed
            .source
            .get(token.offset as usize..(token.offset + token.len) as usize)
            .unwrap_or_default()
    }

    /// A `<variable>` token's name, without its `$`.
    fn variable_name(&self, id: TokenId) -> Vec<u8> {
        self.text(id)
            .strip_prefix(b"$")
            .unwrap_or_default()
            .to_vec()
    }

    /// The doc comment a declaration took, by the token index the
    /// builder recorded.
    fn doc_comment(&self, id: Option<TokenId>) -> Option<Vec<u8>> {
        id.map(|id| self.text(id).to_vec())
    }

    /// The line a node's last byte stands on: a declaration's
    /// `end_lineno`, the line of its closing brace.
    fn end_line(&self, range: TextRange) -> u32 {
        self.parsed
            .lines
            .line_number(range.end().saturating_sub(1).max(range.start))
    }

    /// One statement read from the tree, with php's line for it, or
    /// nothing for the empty statement, which php's `statement: ';'`
    /// reduces to an empty list.
    #[allow(clippy::too_many_lines)]
    fn walk_stmt(&mut self, stmt: Stmt) -> Result<Option<(Ir, u32)>, CompileError> {
        let line = stmt.line(self.parsed);
        let tree = &self.parsed.tree;
        let statement = match stmt {
            Stmt::EmptyStmt(_) => return Ok(None),
            Stmt::InlineHtmlStmt(id) => {
                let text = tree[id].body.map(|body| self.text(body).to_vec());
                Ir::Inline(text.unwrap_or_default())
            }
            Stmt::EchoStmt(id) => Ir::Echo(self.walk_all(&tree[id].exprs)?),
            Stmt::ExprStmt(id) => Ir::Effect(self.walk_expr(tree[id].expr)?),
            Stmt::Block(id) => Ir::Block(self.walk_block(id)?),
            Stmt::IfStmt(id) => Ir::If(self.walk_if(id)?),
            Stmt::WhileStmt(id) => {
                let node = &tree[id];
                let cond = self.walk_cond(node.cond, line, "a `while`")?;
                let body = self.walk_body(node.body, node.stmt_list)?;
                Ir::While { cond, body }
            }
            Stmt::DoWhileStmt(id) => {
                let node = &tree[id];
                let body = self.walk_body(node.body, None)?;
                let cond = self.walk_cond(node.cond, line, "a `do`")?;
                Ir::DoWhile { body, cond }
            }
            Stmt::ForStmt(id) => {
                let node = &tree[id];
                let init = match node.init {
                    Some(init) => self.walk_all(&tree[init].exprs)?,
                    None => Vec::new(),
                };
                let cond = match node.cond {
                    Some(cond) => self.walk_all(&tree[cond].exprs)?,
                    None => Vec::new(),
                };
                let step = match node.step {
                    Some(step) => self.walk_all(&tree[step].exprs)?,
                    None => Vec::new(),
                };
                let body = self.walk_body(node.body, node.stmt_list)?;
                Ir::For {
                    init,
                    cond,
                    step,
                    body,
                }
            }
            Stmt::ForeachStmt(id) => {
                let node = &tree[id];
                let expr = self.walk_cond(node.iterable, line, "a `foreach`")?;
                let key = match node.key {
                    Some(key) => Some(Target {
                        expr: self.walk_expr(tree[key].key)?,
                        by_ref: tree[key].byref.is_some(),
                    }),
                    None => None,
                };
                let Some(target) = node.target else {
                    return Err(unread(line, "a `foreach` whose target"));
                };
                let target = &tree[target];
                let Some(written) = target.target else {
                    return Err(unread(line, "a `foreach` whose target"));
                };
                let value = Target {
                    expr: self.walk_expr(written)?,
                    by_ref: target.byref.is_some(),
                };
                let body = self.walk_body(node.body, node.stmt_list)?;
                Ir::Foreach {
                    expr,
                    value,
                    key,
                    body,
                }
            }
            Stmt::SwitchStmt(id) => {
                let node = &tree[id];
                let expr = self.walk_cond(node.subject, line, "a `switch`")?;
                let mut cases = Vec::with_capacity(node.switch_arms.len());
                for arm in &node.switch_arms {
                    let arm = &tree[*arm];
                    let cond = match arm.value {
                        Some(value) => Some(self.walk_expr(value)?),
                        None => None,
                    };
                    let body = self.walk_stmt_list(arm.stmt_list)?;
                    cases.push(Case {
                        cond,
                        body,
                        line: arm.line(),
                    });
                }
                Ir::Switch { expr, cases }
            }
            Stmt::BreakStmt(id) => Ir::Break(self.walk_optional(tree[id].level)?),
            Stmt::ContinueStmt(id) => Ir::Continue(self.walk_optional(tree[id].level)?),
            Stmt::ReturnStmt(id) => Ir::Return(self.walk_optional(tree[id].value)?),
            Stmt::GotoStmt(id) => {
                let Some(label) = tree[id].label else {
                    return Err(unread(line, "a `goto` naming no label"));
                };
                Ir::Goto(self.text(label).to_vec())
            }
            Stmt::LabelStmt(id) => Ir::Label(self.text(tree[id].name).to_vec()),
            Stmt::TryStmt(id) => {
                let node = &tree[id];
                let body = self.walk_block(node.body)?;
                let mut catches = Vec::with_capacity(node.catch_clauses.len());
                for clause in &node.catch_clauses {
                    catches.push(self.walk_catch(*clause)?);
                }
                let finally = match node.finally_clause {
                    Some(clause) => {
                        let clause = &tree[clause];
                        let Some(block) = clause.body else {
                            return Err(unread(clause.line(), "a `finally` with no block"));
                        };
                        Some((self.walk_block(block)?, tree[block].line()))
                    }
                    None => None,
                };
                Ir::Try {
                    body,
                    catches,
                    finally,
                }
            }
            Stmt::UnsetStmt(id) => {
                let targets = tree[id].targets.clone();
                let mut out = Vec::with_capacity(targets.len());
                for target in &targets {
                    let written = self.walk_expr(*target)?;
                    out.push(unset_target(written, target.line(self.parsed))?);
                }
                Ir::Unset(out)
            }
            Stmt::ConstStmt(id) => {
                if !tree[id].attrs.is_empty() {
                    return Err(unread(line, "an attribute before this declaration"));
                }
                let declarators = tree[id].const_declarators.clone();
                let mut elements = Vec::with_capacity(declarators.len());
                for declarator in &declarators {
                    elements.push(self.walk_const_elem(*declarator)?);
                }
                Ir::Const(elements)
            }
            Stmt::NamespaceDef(id) => {
                let node = &tree[id];
                let Some(name) = node.name else {
                    return Err(unread(line, "a namespace declaration naming nothing"));
                };
                if node.body.is_some() {
                    return Err(unread(line, "a braced namespace declaration"));
                }
                let name = self.text(name);
                if name.starts_with(b"\\") {
                    return Err(unread(line, "a namespace declaration"));
                }
                Ir::Namespace(name.to_vec())
            }
            Stmt::DeclareStmt(id) => {
                let node = &tree[id];
                if node.stmt_list.is_some() {
                    return Err(unread(line, "a `declare` in the alternative syntax"));
                }
                let directives = node.declare_directives.clone();
                let mut elements = Vec::with_capacity(directives.len());
                for directive in &directives {
                    let directive = &tree[*directive];
                    let Some(value) = directive.value else {
                        return Err(unread(directive.line(), "a `declare` directive"));
                    };
                    elements.push(ConstElem {
                        name: self.text(directive.name).to_vec(),
                        value: self.walk_expr(value)?,
                        line: directive.line(),
                    });
                }
                let body = match node.body {
                    Some(body) => {
                        let at = body.line(self.parsed);
                        self.walk_stmt(body)?
                            .map(|(statement, _)| vec![(statement, at)])
                    }
                    None => None,
                };
                Ir::Declare {
                    directives: elements,
                    body,
                }
            }
            Stmt::FunctionDecl(id) => {
                let node = &tree[id];
                if !node.attrs.is_empty() {
                    return Err(unread(line, "an attribute before this declaration"));
                }
                let name = self.text(node.name).to_vec();
                Ir::Function(self.walk_function(
                    name,
                    node.byref.is_some(),
                    node.params,
                    node.ret,
                    node.body,
                    node.doc_comment,
                    line,
                    node.range(),
                )?)
            }
            Stmt::ClassDecl(id) => {
                let node = &tree[id];
                if !node.modifiers.is_empty() {
                    return Err(unread(line, "a class with a modifier"));
                }
                let Some(name) = node.name else {
                    return Err(unread(line, "a class"));
                };
                let parent = match node.extends {
                    Some(extends) => tree[extends]
                        .types
                        .first()
                        .map(|id| self.text(*id).to_vec()),
                    None => None,
                };
                let interfaces = match node.implements {
                    Some(implements) => tree[implements]
                        .types
                        .iter()
                        .map(|id| self.text(*id).to_vec())
                        .collect(),
                    None => Vec::new(),
                };
                let attrs = node.attrs.clone();
                let attributes = self.walk_attributes(&attrs)?;
                Ir::Class(self.walk_class(
                    ClassKind::Class,
                    name,
                    parent,
                    interfaces,
                    attributes,
                    node.doc_comment,
                    node.body,
                    line,
                    node.range(),
                )?)
            }
            Stmt::InterfaceDecl(id) => {
                let node = &tree[id];
                let Some(name) = node.name else {
                    return Err(unread(line, "an interface"));
                };
                let interfaces = match node.extends {
                    Some(extends) => tree[extends]
                        .types
                        .iter()
                        .map(|id| self.text(*id).to_vec())
                        .collect(),
                    None => Vec::new(),
                };
                let attrs = node.attrs.clone();
                let attributes = self.walk_attributes(&attrs)?;
                Ir::Class(self.walk_class(
                    ClassKind::Interface,
                    name,
                    None,
                    interfaces,
                    attributes,
                    node.doc_comment,
                    node.body,
                    line,
                    node.range(),
                )?)
            }
            Stmt::TraitDecl(_) => return Err(unread(line, "a trait")),
            Stmt::EnumDecl(_) => return Err(unread(line, "an enum")),
            Stmt::GlobalStmt(_) => return Err(unread(line, "a `global` statement")),
            Stmt::StaticVarStmt(_) => return Err(unread(line, "a `static` variable")),
            Stmt::UseStmt(_) => return Err(unread(line, "a `use` statement")),
            Stmt::HaltCompilerStmt(_) => return Err(unread(line, "a `__halt_compiler`")),
        };
        Ok(Some((statement, line)))
    }

    /// A statement list from the tree, each statement read in order.
    fn walk_stmts(&mut self, stmts: &[Stmt]) -> Result<Read, CompileError> {
        let mut out = Read::with_capacity(stmts.len());
        for stmt in stmts {
            if let Some(read) = self.walk_stmt(*stmt)? {
                out.push(read);
            }
        }
        Ok(out)
    }

    /// A block's statements.
    fn walk_block(&mut self, id: NodeId<Block>) -> Result<Read, CompileError> {
        let stmts = self.parsed.tree[id].stmts.clone();
        self.walk_stmts(&stmts)
    }

    /// An alternative-syntax body's statements, empty where the tree
    /// has none.
    fn walk_stmt_list(&mut self, id: Option<NodeId<StmtList>>) -> Result<Read, CompileError> {
        match id {
            Some(id) => {
                let stmts = self.parsed.tree[id].stmts.clone();
                self.walk_stmts(&stmts)
            }
            None => Ok(Read::new()),
        }
    }

    /// The body of a loop or an arm: a block's statements themselves,
    /// one statement alone, or the alternative syntax's list, which php's
    /// grammar hands the compiler as one `statement` either way.
    fn walk_body(
        &mut self,
        body: Option<Stmt>,
        stmt_list: Option<NodeId<StmtList>>,
    ) -> Result<Read, CompileError> {
        match body {
            Some(Stmt::Block(block)) => self.walk_block(block),
            Some(stmt) => Ok(self.walk_stmt(stmt)?.into_iter().collect()),
            None => self.walk_stmt_list(stmt_list),
        }
    }

    /// A condition the grammar requires, refused where recovery left
    /// none.
    fn walk_cond(
        &mut self,
        cond: Option<tree::Expr>,
        line: u32,
        what: &str,
    ) -> Result<Child, CompileError> {
        match cond {
            Some(cond) => self.walk_expr(cond),
            None => Err(unread(line, &format!("{what} with no condition"))),
        }
    }

    /// An expression written or not.
    fn walk_optional(&mut self, expr: Option<tree::Expr>) -> Result<Option<Child>, CompileError> {
        match expr {
            Some(expr) => Some(self.walk_expr(expr)).transpose(),
            None => Ok(None),
        }
    }

    /// `zend_compile_if`'s arms in written order: the `if`, each
    /// `elseif`, the `else`, in either syntax; an arm's line is its
    /// node's, the condition's for an arm with one and the statements'
    /// for the `else`.
    fn walk_if(&mut self, id: NodeId<tree::IfStmt>) -> Result<Vec<IfArm>, CompileError> {
        let node = self.parsed.tree[id].clone();
        let line = node.line();
        let alt = node.stmt_list.is_some();
        let cond = self.walk_cond(node.cond, line, "an `if`")?;
        let body = self.walk_body(node.then, node.stmt_list)?;
        let mut arms = vec![IfArm {
            cond: Some(cond),
            body,
            line,
        }];
        if alt {
            for clause in &node.alt_elseif_clauses {
                let clause = self.parsed.tree[*clause].clone();
                let cond = self.walk_cond(clause.cond, clause.line(), "an `elseif`")?;
                let body = self.walk_stmt_list(clause.stmt_list)?;
                arms.push(IfArm {
                    cond: Some(cond),
                    body,
                    line: clause.line(),
                });
            }
            if let Some(clause) = node.alt_else_clause {
                let clause = self.parsed.tree[clause].clone();
                let body = self.walk_stmt_list(clause.stmt_list)?;
                arms.push(IfArm {
                    cond: None,
                    body,
                    line: clause.line(),
                });
            }
        } else {
            for clause in &node.elseif_clauses {
                let clause = self.parsed.tree[*clause].clone();
                let cond = self.walk_cond(clause.cond, clause.line(), "an `elseif`")?;
                let body = self.walk_body(clause.body, None)?;
                arms.push(IfArm {
                    cond: Some(cond),
                    body,
                    line: clause.line(),
                });
            }
            if let Some(clause) = node.else_clause {
                let clause = self.parsed.tree[clause].clone();
                let body = self.walk_body(clause.body, None)?;
                arms.push(IfArm {
                    cond: None,
                    body,
                    line: clause.line(),
                });
            }
        }
        Ok(arms)
    }

    /// One `catch (A|B $e) { … }`: the class names as written, the
    /// variable, the block, and the clause's line.
    fn walk_catch(&mut self, id: NodeId<CatchClause>) -> Result<Catch, CompileError> {
        let clause = self.parsed.tree[id].clone();
        let line = clause.line();
        let Some(types) = clause.types else {
            return Err(unread(line, "a `catch` naming no class"));
        };
        let types: Vec<Vec<u8>> = self.parsed.tree[types]
            .types
            .iter()
            .map(|id| self.text(*id).to_vec())
            .collect();
        if types.is_empty() {
            return Err(unread(line, "a `catch` naming no class"));
        }
        let var = clause.var.map(|id| self.variable_name(id));
        let Some(block) = clause.body else {
            return Err(unread(line, "a `catch` with no block"));
        };
        let body = self.walk_block(block)?;
        Ok(Catch {
            types,
            var,
            body,
            line,
        })
    }

    /// One `name = value` declarator of a `const` statement or a class
    /// constant, at the name's line.
    fn walk_const_elem(
        &mut self,
        id: NodeId<tree::ConstDeclarator>,
    ) -> Result<ConstElem, CompileError> {
        let declarator = self.parsed.tree[id].clone();
        let line = declarator.line();
        let Some(name) = self.parsed.tree[declarator.name].name else {
            return Err(unread(line, "a `const` whose name is no identifier"));
        };
        let Some(value) = declarator.value else {
            return Err(unread(line, "a `const` declarator with no value"));
        };
        Ok(ConstElem {
            name: self.text(name).to_vec(),
            value: self.walk_expr(value)?,
            line,
        })
    }

    /// The attributes before a class, `#[A(1), B] #[C]`, php's
    /// `attributed_statement` groups, each attribute with its arguments;
    /// a named or unpacked argument is refused.
    ///
    /// ```php
    /// #[A] #[B(1), C] class M { public $x = 1; }
    /// var_dump((new M)->x);
    /// ```
    /// ```expect
    /// int(1)
    /// ```
    fn walk_attributes(
        &mut self,
        groups: &[NodeId<tree::AttributeGroup>],
    ) -> Result<Vec<Attr>, CompileError> {
        let mut out = Vec::new();
        for group in groups {
            let attributes = self.parsed.tree[*group].attributes.clone();
            for attribute in &attributes {
                let attribute = self.parsed.tree[*attribute].clone();
                let line = attribute.line();
                let arguments = match attribute.args {
                    Some(args) => self.plain_arguments(args, line, "an attribute's")?,
                    None => Vec::new(),
                };
                out.push(Attr {
                    name: self.text(attribute.name).to_vec(),
                    arguments,
                    line,
                });
            }
        }
        Ok(out)
    }

    /// The arguments of a list that are plain expressions;
    /// anything else refused as what `whose` cannot carry yet.
    fn plain_arguments(
        &mut self,
        id: NodeId<tree::ArgList>,
        line: u32,
        whose: &str,
    ) -> Result<Vec<Child>, CompileError> {
        let args = self.parsed.tree[id].args.clone();
        let mut out = Vec::with_capacity(args.len());
        for arg in &args {
            match arg {
                tree::Arg::Expr(expr) => out.push(self.walk_expr(*expr)?),
                tree::Arg::NamedArg(_) => {
                    return Err(unread(line, &format!("{whose} named argument")));
                }
                tree::Arg::SpreadArg(_) | tree::Arg::VariadicPlaceholder(_) => {
                    return Err(unread(line, &format!("{whose} argument list")));
                }
            }
        }
        Ok(out)
    }

    /// A declared type from the tree, in the shape the unit holds: the
    /// scalar and special names by their keyword, any other name as
    /// written without a leading `\`, and `?T`, `A|B` and `A&B` over
    /// those; a DNF type is refused.
    fn walk_type(&self, ty: TypeExpr, line: u32) -> Result<Type, CompileError> {
        let tree = &self.parsed.tree;
        match ty {
            TypeExpr::NamedType(id) => Ok(type_name(self.text(tree[id].name))),
            TypeExpr::NullableType(id) => match tree[id].ty {
                Some(TypeExpr::NamedType(inner)) => Ok(Type::Nullable(Box::new(type_name(
                    self.text(tree[inner].name),
                )))),
                _ => Err(unread(line, "a type")),
            },
            TypeExpr::UnionType(id) => Ok(Type::Union(self.type_members(&tree[id].types, line)?)),
            TypeExpr::IntersectionType(id) => Ok(Type::Intersection(
                self.type_members(&tree[id].types, line)?,
            )),
            TypeExpr::ParenType(_) => Err(unread(line, "a DNF type")),
        }
    }

    /// The members of a union or an intersection, each a name.
    fn type_members(&self, members: &[TypeExpr], line: u32) -> Result<Vec<Type>, CompileError> {
        members
            .iter()
            .map(|member| match member {
                TypeExpr::NamedType(id) => Ok(type_name(self.text(self.parsed.tree[*id].name))),
                TypeExpr::ParenType(_) => Err(unread(line, "a DNF type")),
                _ => Err(unread(line, "a type")),
            })
            .collect()
    }

    /// A function's or a method's signature and body from the tree:
    /// the parameters, the return type, the block, and the lines of
    /// the keyword and the closing brace.
    #[allow(clippy::too_many_arguments)]
    fn walk_function(
        &mut self,
        name: Vec<u8>,
        by_ref: bool,
        params: NodeId<ParamList>,
        ret: Option<NodeId<tree::RetType>>,
        body: Option<NodeId<Block>>,
        doc_comment: Option<TokenId>,
        line: u32,
        range: TextRange,
    ) -> Result<Function, CompileError> {
        let parameters = self.walk_parameters(params)?;
        let return_type = match ret {
            Some(ret) => match self.parsed.tree[ret].ty {
                Some(ty) => Some(self.walk_type(ty, line)?),
                None => return Err(unread(line, "a return type")),
            },
            None => None,
        };
        let Some(body) = body else {
            return Err(unread(line, "a function whose body is no block"));
        };
        let doc_comment = self.doc_comment(doc_comment);
        let body = self.walk_block(body)?;
        Ok(Function {
            name,
            parameters,
            return_type,
            by_ref,
            body,
            doc_comment,
            line,
            end_line: self.end_line(range),
        })
    }

    /// The parameters of a list, php's `ZEND_AST_PARAM` each: the type,
    /// the `&`, the `...`, the name, the default and the promotion.
    fn walk_parameters(&mut self, id: NodeId<ParamList>) -> Result<Vec<Parameter>, CompileError> {
        let params = self.parsed.tree[id].params.clone();
        let mut out = Vec::with_capacity(params.len());
        for param in &params {
            let param = self.parsed.tree[*param].clone();
            let line = param.line();
            if !param.attrs.is_empty() {
                return Err(unread(line, "an attribute"));
            }
            if param.hooks.is_some() {
                return Err(unread(line, "a property accessor list"));
            }
            let modifiers = self.walk_modifiers(&param.modifiers, line)?;
            if modifiers.set_visibility.is_some() {
                return Err(unread(line, "an asymmetric visibility"));
            }
            if modifiers.any(&[Flag::Static, Flag::Final, Flag::Abstract, Flag::Var]) {
                return Err(unread(line, "a parameter"));
            }
            let is_readonly = modifiers.has(Flag::Readonly);
            let promote = (modifiers.visibility.is_some() || is_readonly).then(|| Promoted {
                visibility: modifiers.visibility.unwrap_or_default(),
                is_readonly,
            });
            let ty = match param.ty {
                Some(ty) => Some(self.walk_type(ty, line)?),
                None => None,
            };
            let default = self.walk_optional(param.default)?;
            out.push(Parameter {
                name: self.variable_name(param.var),
                ty,
                by_ref: param.byref.is_some(),
                variadic: param.variadic.is_some(),
                default,
                promote,
            });
        }
        Ok(out)
    }

    /// A modifier list read once, php's `member_modifier` list:
    /// `zend_add_member_modifier` refuses a second visibility in its
    /// own words, and a second set visibility in the same words under
    /// its own arm (`Z/Zend/zend_compile.c:1043`).
    ///
    /// ```php
    /// class Foo { public private(set) protected(set) int $bar; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Multiple access type modifiers are not allowed in %s on line 2
    /// ```
    ///
    /// The word is read whatever its case, as every keyword is:
    ///
    /// ```php
    /// class Foo { public PRIVATE(SET) int $bar = 7; }
    /// var_dump((new Foo)->bar, (new ReflectionProperty('Foo', 'bar'))->isPrivateSet());
    /// ```
    /// ```expect
    /// int(7)
    /// bool(true)
    /// ```
    fn walk_modifiers(&self, modifiers: &[Modifier], line: u32) -> Result<Modifiers, CompileError> {
        let mut out = Modifiers::default();
        for modifier in modifiers {
            let visibility = match modifier {
                Modifier::PublicKw(_) => Some(Visibility::Public),
                Modifier::ProtectedKw(_) => Some(Visibility::Protected),
                Modifier::PrivateKw(_) => Some(Visibility::Private),
                Modifier::VarKw(_) => {
                    out.flags.push(Flag::Var);
                    Some(Visibility::Public)
                }
                Modifier::SetVisibility(id) => {
                    let seen = match self.parsed.tree[*id].vis.map(|vis| {
                        self.parsed
                            .tokens
                            .get(vis.0 as usize)
                            .map(|token| token.kind)
                    }) {
                        Some(Some(SyntaxKind::PROTECTED_KW)) => Visibility::Protected,
                        Some(Some(SyntaxKind::PRIVATE_KW)) => Visibility::Private,
                        _ => Visibility::Public,
                    };
                    if out.set_visibility.is_some() {
                        return Err(refused(
                            line,
                            "Multiple access type modifiers are not allowed",
                        ));
                    }
                    out.set_visibility = Some(seen);
                    continue;
                }
                Modifier::StaticKw(_) => {
                    out.flags.push(Flag::Static);
                    continue;
                }
                Modifier::AbstractKw(_) => {
                    out.flags.push(Flag::Abstract);
                    continue;
                }
                Modifier::FinalKw(_) => {
                    out.flags.push(Flag::Final);
                    continue;
                }
                Modifier::ReadonlyKw(_) => {
                    out.flags.push(Flag::Readonly);
                    continue;
                }
            };
            if out.visibility.is_some() {
                return Err(refused(
                    line,
                    "Multiple access type modifiers are not allowed",
                ));
            }
            out.visibility = visibility;
        }
        Ok(out)
    }

    /// A class or an interface from the tree: its members as php's
    /// `ZEND_AST_PROP_GROUP`, `ZEND_AST_CLASS_CONST_GROUP` and
    /// `ZEND_AST_METHOD` hold them, one declarator each.
    #[allow(clippy::too_many_arguments)]
    fn walk_class(
        &mut self,
        kind: ClassKind,
        name: TokenId,
        parent: Option<Vec<u8>>,
        interfaces: Vec<Vec<u8>>,
        attributes: Vec<Attr>,
        doc_comment: Option<TokenId>,
        body: NodeId<ClassBody>,
        line: u32,
        range: TextRange,
    ) -> Result<Class, CompileError> {
        let name = self.text(name).to_vec();
        if read::is_reserved_class_name(&name) {
            return Err(refused(
                line,
                format!(
                    "Cannot use \"{}\" as {} as it is reserved",
                    String::from_utf8_lossy(&name),
                    match kind {
                        ClassKind::Interface => "an interface name",
                        _ => "a class name",
                    }
                ),
            ));
        }
        let mut class = Class {
            name,
            kind,
            parent,
            interfaces,
            attributes,
            doc_comment: self.doc_comment(doc_comment),
            properties: Vec::new(),
            constants: Vec::new(),
            methods: Vec::new(),
            line,
            end_line: self.end_line(range),
        };
        let members = self.parsed.tree[body].class_members.clone();
        for member in &members {
            self.walk_member(&mut class, *member)?;
        }
        Ok(class)
    }

    /// One member of a class body.
    fn walk_member(&mut self, class: &mut Class, member: ClassMember) -> Result<(), CompileError> {
        match member {
            ClassMember::ClassConstDecl(id) => self.walk_constants(class, id),
            ClassMember::PropertyDecl(id) => self.walk_properties(class, id),
            ClassMember::MethodDecl(id) => self.walk_method(class, id),
            ClassMember::TraitUseDecl(id) => {
                Err(unread(self.parsed.tree[id].line(), "a `use` of a trait"))
            }
            ClassMember::EnumCaseDecl(id) => {
                Err(unread(self.parsed.tree[id].line(), "an enum case"))
            }
        }
    }

    /// One `const A = 1, B = 2;` of a class body, php's
    /// `ZEND_AST_CLASS_CONST_GROUP`: the group's modifiers beside each
    /// declarator, whose doc comment and line are its own.
    fn walk_constants(
        &mut self,
        class: &mut Class,
        id: NodeId<tree::ClassConstDecl>,
    ) -> Result<(), CompileError> {
        let node = self.parsed.tree[id].clone();
        let line = node.line();
        if !node.attrs.is_empty() {
            return Err(unread(line, "an attribute before a class member"));
        }
        if node.ty.is_some() {
            return Err(unread(line, "a typed class constant"));
        }
        let modifiers = self.walk_modifiers(&node.modifiers, line)?;
        if modifiers.any(&[Flag::Static, Flag::Abstract, Flag::Readonly, Flag::Var])
            || modifiers.set_visibility.is_some()
        {
            return Err(unread(line, "a class constant"));
        }
        for declarator in &node.const_declarators {
            let element = self.walk_const_elem(*declarator)?;
            let doc_comment = self.doc_comment(self.parsed.tree[*declarator].doc_comment);
            class.constants.push(Constant {
                name: element.name,
                visibility: modifiers.visibility.unwrap_or_default(),
                is_final: modifiers.has(Flag::Final),
                value: element.value,
                doc_comment,
                line: element.line,
            });
        }
        Ok(())
    }

    /// One `public int $x = 1, $y;` of a class body, php's
    /// `ZEND_AST_PROP_GROUP`: the group's modifiers and type beside each
    /// declarator.
    fn walk_properties(
        &mut self,
        class: &mut Class,
        id: NodeId<tree::PropertyDecl>,
    ) -> Result<(), CompileError> {
        let node = self.parsed.tree[id].clone();
        let line = node.line();
        if !node.attrs.is_empty() {
            return Err(unread(line, "an attribute before a class member"));
        }
        if node.hooks.is_some() {
            return Err(unread(line, "a property accessor list"));
        }
        let modifiers = self.walk_modifiers(&node.modifiers, line)?;
        if modifiers.any(&[Flag::Final, Flag::Abstract, Flag::Readonly]) {
            return Err(unread(line, "a property"));
        }
        let ty = match node.ty {
            Some(ty) => Some(self.walk_type(ty, line)?),
            None => None,
        };
        for declarator in &node.property_declarators {
            let declarator = self.parsed.tree[*declarator].clone();
            let default = self.walk_optional(declarator.init)?;
            class.properties.push(Property {
                name: self.variable_name(declarator.var),
                visibility: modifiers.visibility.unwrap_or_default(),
                set_visibility: modifiers.set_visibility,
                ty: ty.clone(),
                is_static: modifiers.has(Flag::Static),
                default,
                doc_comment: self.doc_comment(declarator.doc_comment),
                line: declarator.line(),
            });
        }
        Ok(())
    }

    /// One method of a class body, php's `ZEND_AST_METHOD`: a function
    /// with the modifiers beside it.
    fn walk_method(
        &mut self,
        class: &mut Class,
        id: NodeId<tree::MethodDecl>,
    ) -> Result<(), CompileError> {
        let node = self.parsed.tree[id].clone();
        let line = node.line();
        if !node.attrs.is_empty() {
            return Err(unread(line, "an attribute before a class member"));
        }
        let modifiers = self.walk_modifiers(&node.modifiers, line)?;
        if modifiers.any(&[Flag::Abstract, Flag::Readonly, Flag::Var])
            || modifiers.set_visibility.is_some()
        {
            return Err(unread(line, "a class member"));
        }
        let Some(name) = node.name.and_then(|name| self.parsed.tree[name].name) else {
            return Err(unread(line, "a method"));
        };
        let name = self.text(name).to_vec();
        let function = self.walk_function(
            name,
            node.byref.is_some(),
            node.params,
            node.ret,
            node.body,
            node.doc_comment,
            line,
            node.range(),
        )?;
        class.methods.push(Method {
            function,
            visibility: modifiers.visibility.unwrap_or_default(),
            is_static: modifiers.has(Flag::Static),
            is_final: modifiers.has(Flag::Final),
        });
        Ok(())
    }
}

/// The refusal for a construct the emitter cannot emit yet, at the
/// node's own line: the `Level::Parse` sentence the phpt lane reads as
/// unread.
fn unread(line: u32, what: &str) -> CompileError {
    CompileError {
        level: Level::Parse,
        message: format!("{what} this compiler cannot read yet"),
        line,
        raised: Vec::new(),
    }
}

/// php's compile error at the node's own line.
fn refused(line: u32, message: impl Into<String>) -> CompileError {
    CompileError {
        level: Level::CompileError,
        message: message.into(),
        line,
        raised: Vec::new(),
    }
}

/// One type name as php's `zend_compile_single_typename` sorts it: a
/// scalar or special type by its keyword whatever the case, `static`, `self` and
/// `parent`, and any other name as a class name without its leading `\`.
fn type_name(name: &[u8]) -> Type {
    match name.to_ascii_lowercase().as_slice() {
        b"mixed" => Type::Mixed,
        b"void" => Type::Void,
        b"never" => Type::Never,
        b"null" => Type::Null,
        b"bool" => Type::Bool,
        b"false" => Type::False,
        b"true" => Type::True,
        b"int" => Type::Int,
        b"float" => Type::Float,
        b"string" => Type::String,
        b"array" => Type::Array,
        b"object" => Type::Object,
        b"iterable" => Type::Iterable,
        b"callable" => Type::Callable,
        b"static" => Type::CalledClass,
        b"self" => Type::SelfClass,
        b"parent" => Type::ParentClass,
        _ => Type::Named(Name(name.strip_prefix(b"\\").unwrap_or(name).into())),
    }
}

/// One operand of `unset`, as `zend_compile_unset` sorts them: a
/// variable, one element of one, one property of one; `$this` and
/// the append refused as php refuses them.
fn unset_target(written: Child, line: u32) -> Result<Unset, CompileError> {
    match written.take() {
        Expr::This => Err(refused(line, "Cannot unset $this")),
        Expr::Var(name) => Ok(Unset::Var(name)),
        Expr::Dim { container, index } => {
            let Expr::Var(var) = container.take() else {
                return Err(unread(
                    line,
                    "an `unset` over an element of what is no variable",
                ));
            };
            let Some(dim) = index else {
                return Err(refused(line, "Cannot use [] for unsetting"));
            };
            Ok(Unset::Dim { var, dim })
        }
        Expr::Prop { object, name } => {
            if !matches!(&*object, Expr::Var(_) | Expr::This) {
                return Err(unread(
                    line,
                    "an `unset` over a property of what is no variable",
                ));
            }
            Ok(Unset::Prop { object, name })
        }
        _ => Err(unread(line, "an `unset` over what is no variable")),
    }
}
