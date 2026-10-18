//! The walk over the declarations of `php_syntax`'s tree (design §4.9):
//! what [`Emit`] reads a class, a function and their members as, and,
//! in [`expr`], a closure.
//!
//! `emit` walks the tree's `SourceFile`, every statement node in it and
//! every expression under them itself ([`Emit::statement`],
//! [`Emit::expr`]); what is here reads the declarations a statement
//! carries, each fact where the tree holds it: a name from its token, a
//! modifier from its token, a type from its `TypeExpr`, a doc comment
//! from the token index the builder recorded, a line from
//! [`Node::line`], which is php's `zend_ast.lineno`. A declaration is
//! handed to the emitter in one of the shapes at the foot of this
//! file, its body and its defaults the tree's own nodes, so every
//! construct's semantics stays in the one function that has it and
//! nothing here decides what an opline is (#595).
//!
//! **What is refused.** A construct the tree parses and the emitter
//! cannot emit yet is refused here, at the node's line, in the one-line
//! `this compiler cannot read yet` shape at `Level::Parse`, which the
//! phpt lane counts as unread; what the grammar does not parse is php's
//! parse error from `Parsed::diagnostics`, raised before this walk runs.

use php_syntax::tree::{
    Block, ClassBody, ClassMember, Modifier, Node, NodeId, ParamList, Stmt, TokenId, TypeExpr,
};
use php_syntax::{Parsed, SyntaxKind, TextRange, tree};
use php_unit::Level;
use php_unit::decl::{ClassKind, Visibility};

mod expr;
mod hooks;

pub(super) use hooks::{Hook, HookBody};

use super::Emit;
use crate::CompileError;

/// A group of a member's modifiers as the tree holds them, read once:
/// php's `member_modifier` list after `zend_modifier_list_to_flags`
/// folds it for its [`Target`]. The visibilities are read apart, since
/// php refuses two of either; the rest are the flags written.
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

/// What a modifier list is written on, php's `zend_modifier_target`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Property,
    Method,
    Constant,
    Parameter,
    Hook,
}

impl Target {
    /// The member `zend_modifier_token_to_flag` names in its refusal.
    fn member(self) -> &'static str {
        match self {
            Target::Property => "property",
            Target::Method => "method",
            Target::Constant => "class constant",
            Target::Parameter => "parameter",
            Target::Hook => "property hook",
        }
    }
}

/// One modifier token as `zend_modifier_token_to_flag` reads it, before
/// the target has taken it.
#[derive(Clone, Copy)]
enum Written {
    Visibility(Visibility),
    SetVisibility(Visibility),
    Flag(Flag),
}

impl Written {
    /// `zend_modifier_token_to_string`.
    fn word(self) -> &'static str {
        match self {
            Written::Visibility(Visibility::Public) => "public",
            Written::Visibility(Visibility::Protected) => "protected",
            Written::Visibility(Visibility::Private) => "private",
            Written::SetVisibility(Visibility::Public) => "public(set)",
            Written::SetVisibility(Visibility::Protected) => "protected(set)",
            Written::SetVisibility(Visibility::Private) => "private(set)",
            Written::Flag(Flag::Static) => "static",
            Written::Flag(Flag::Abstract) => "abstract",
            Written::Flag(Flag::Final) => "final",
            Written::Flag(Flag::Readonly) => "readonly",
            Written::Flag(Flag::Var) => "var",
        }
    }

    /// Whether `zend_modifier_token_to_flag` gives the token a bit on
    /// `target`. `var` is the grammar's, which writes it before a
    /// property alone.
    fn takes(self, target: Target) -> bool {
        match self {
            Written::Flag(Flag::Final | Flag::Var) => true,
            Written::Visibility(_) => target != Target::Hook,
            Written::SetVisibility(_) | Written::Flag(Flag::Readonly) => {
                matches!(target, Target::Property | Target::Parameter)
            }
            Written::Flag(Flag::Static | Flag::Abstract) => {
                matches!(target, Target::Property | Target::Method)
            }
        }
    }
}

impl Modifiers {
    fn has(&self, flag: Flag) -> bool {
        self.flags.contains(&flag)
    }
}

/// The modifiers of a class, php's `class_modifiers` as
/// `zend_add_class_modifier` folds them into `ZEND_ACC_EXPLICIT_ABSTRACT_CLASS`,
/// `ZEND_ACC_FINAL` and `ZEND_ACC_READONLY_CLASS`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // the three class modifiers, each one fact
pub(super) struct ClassModifiers {
    pub(super) is_abstract: bool,
    pub(super) is_final: bool,
    pub(super) is_readonly: bool,
}

/// Where every generator the file declares starts: the offset of each
/// function, method, closure and arrow function whose own body holds a
/// `yield` or a `yield from`, ascending. php's parser marks the
/// declaration as it reduces the `yield` (`CG(extra_fn_flags) |=
/// ZEND_ACC_GENERATOR`, `Z/Zend/zend_language_parser.y:1378-1381`),
/// the flags saved and restored around every nested declaration, so a
/// `yield` belongs to the innermost function-like around it and to no
/// class body: one walk of the tree's shape with a stack of the
/// declarations open, which is what `zend_compile_func_decl` reads
/// before it compiles the body and emits `GENERATOR_CREATE` after the
/// parameters.
pub(super) fn generators(tree: &tree::Tree) -> Vec<u32> {
    let mut open: Vec<(usize, Option<u32>)> = Vec::new();
    let mut found = Vec::new();
    for (at, element) in tree.elements().iter().enumerate() {
        let tree::Element::Node {
            kind,
            range,
            descendants,
            ..
        } = *element
        else {
            continue;
        };
        while open.last().is_some_and(|(end, _)| *end < at) {
            open.pop();
        }
        let end = at.saturating_add(descendants as usize);
        match kind {
            SyntaxKind::FUNCTION_DECL
            | SyntaxKind::METHOD_DECL
            | SyntaxKind::CLOSURE_EXPR
            | SyntaxKind::ARROW_FN_EXPR
            | SyntaxKind::PROPERTY_HOOK => open.push((end, Some(range.start))),
            SyntaxKind::CLASS_DECL
            | SyntaxKind::INTERFACE_DECL
            | SyntaxKind::TRAIT_DECL
            | SyntaxKind::ENUM_DECL
            | SyntaxKind::ANON_CLASS_DECL => open.push((end, None)),
            SyntaxKind::YIELD_EXPR | SyntaxKind::YIELD_FROM_EXPR => {
                if let Some((_, Some(start))) = open.last() {
                    found.push(*start);
                }
            }
            _ => {}
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

impl Emit<'_> {
    /// Whether the function-like node starting at `start` is a
    /// generator ([`generators`]).
    pub(super) fn is_generator_at(&self, start: u32) -> bool {
        self.generators.binary_search(&start).is_ok()
    }

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
        let stmts = self.list(self.parsed.tree[file].stmts);
        self.depth = self.depth.saturating_add(1);
        let compiled = self.top_statements(stmts);
        self.depth = self.depth.saturating_sub(1);
        compiled
    }

    /// `zend_compile_top_stmt` over a list of top statements, the
    /// file's own, a bracketed namespace's or a block's among them: each
    /// at its own line, a function or a class declaration leaving the
    /// line at its closing brace (`CG(zend_lineno) = decl->end_lineno`),
    /// and after each but a `namespace` and a `__halt_compiler` php's
    /// `zend_verify_namespace`. An empty statement is php's `NULL`
    /// child, which the walk returns from before either.
    ///
    /// A function or a class in a block among the top statements is
    /// bound before the line above the block runs, as php binds it:
    ///
    /// ```php
    /// var_dump(function_exists('f'), class_exists('C'));
    /// { function f() {} class C {} echo 1, "\n"; }
    /// var_dump(f());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// 1
    /// NULL
    /// ```
    pub(super) fn top_statements(&mut self, stmts: &[Stmt]) -> Result<(), CompileError> {
        let mut after = 0;
        for stmt in stmts {
            // A `?>` between two statements of the list is php's `;`, the
            // `NULL` child, as much as an empty statement is.
            let range = stmt.range(self.parsed);
            let glue = self.top_list == super::TopList::Declares
                && self.parsed.tokens.iter().any(|token| {
                    token.kind == SyntaxKind::CLOSE_TAG
                        && token.offset >= after
                        && token.offset < range.start
                });
            after = range.end();
            if glue || !matches!(stmt, Stmt::DeclareStmt(_)) {
                self.top_list = super::TopList::Other;
            }
            if matches!(stmt, Stmt::EmptyStmt(_)) {
                continue;
            }
            // A block is `zend_compile_top_stmt`'s `ZEND_AST_STMT_LIST`
            // arm: each statement inside is a top statement, a function
            // or a class among them bound at compile time, and the block
            // itself takes no line and no `zend_verify_namespace`. It is
            // still a statement `zend_is_first_statement` finds ahead of
            // a `namespace` or a `strict_types` declaration.
            if let Stmt::Block(block) = stmt {
                self.statements_seen = self.statements_seen.saturating_add(1);
                self.top_statements(self.list(self.parsed.tree[*block].stmts))?;
                continue;
            }
            self.line = stmt.line(self.parsed);
            // A function or a class is `zend_compile_func_decl` or
            // `zend_compile_class_decl` straight from the top statement,
            // which ticks nothing after it; every other goes through
            // `zend_compile_stmt`, `__halt_compiler` among them.
            match stmt {
                Stmt::HaltCompilerStmt(id) => {
                    self.ext_stmt();
                    self.halt_compiler(*id)?;
                    self.tick();
                }
                Stmt::FunctionDecl(_)
                | Stmt::ClassDecl(_)
                | Stmt::InterfaceDecl(_)
                | Stmt::TraitDecl(_)
                | Stmt::EnumDecl(_) => self.statement(*stmt)?,
                _ => self.stmt(*stmt)?,
            }
            match stmt {
                Stmt::NamespaceDef(_) | Stmt::HaltCompilerStmt(_) => continue,
                Stmt::FunctionDecl(_)
                | Stmt::ClassDecl(_)
                | Stmt::InterfaceDecl(_)
                | Stmt::TraitDecl(_)
                | Stmt::EnumDecl(_) => self.line = self.end_line(stmt.range(self.parsed)),
                _ => {}
            }
            self.verify_namespace()?;
        }
        Ok(())
    }

    /// A `namespace` statement as `zend_compile_namespace` reads it: the
    /// name in `child[0]`, none for `namespace { … }`, and the braces'
    /// statement list in `child[1]`, none for the `;` form.
    pub(super) fn walk_namespace(
        &self,
        id: NodeId<tree::NamespaceDef>,
    ) -> Result<Namespace, CompileError> {
        let node = &self.parsed.tree[id];
        let name = node.name.map(|name| self.text(name).to_vec());
        match (name, node.body) {
            // What only a statement the parser refused leaves.
            (Some(name), _) if name.starts_with(b"\\") => {
                Err(unread(node.line(), "a namespace declaration"))
            }
            (None, None) => Err(unread(node.line(), "a namespace declaration")),
            (name, Some(body)) => Ok(Namespace::Bracketed {
                name,
                body: self.list(self.parsed.tree[body].stmts).to_vec(),
            }),
            (Some(name), None) => Ok(Namespace::Unbracketed(name)),
        }
    }

    /// `InternalError`: the tree is not what the grammar promised
    /// (design §10).
    fn internal(&self, what: impl AsRef<str>) -> CompileError {
        CompileError {
            level: Level::Error,
            message: format!("InternalError: {}", what.as_ref()),
            line: self.line,
            raised: Vec::new(),
            thrown: false,
            stage: crate::Stage::Compiling,
        }
    }

    /// The line a node's last byte stands on: a declaration's
    /// `end_lineno`, the line of its closing brace.
    ///
    /// A declaration's other line, `start_lineno`, is not read from the
    /// range: `zend_ast_decl` writes it where `zend_ast.lineno` stands
    /// (`Z/Zend/zend_ast.c:137-158`), so it is the node's own `line()`,
    /// which the builder puts on the keyword and, in argument position,
    /// on the token after it (`php_syntax`'s `tree::line`, #601 row 17).
    /// Reading it from the range would name the `static` or the
    /// attribute group written ahead of the keyword instead.
    fn end_line(&self, range: TextRange) -> u32 {
        self.parsed
            .lines
            .line_number(range.end().saturating_sub(1).max(range.start))
    }

    /// A `class` or an `interface` declaration read from the tree, in
    /// the shape the emitter's `class` takes: the name, the parent, the
    /// interfaces, the attributes, the doc comment and the members.
    pub(super) fn walk_class_decl(&mut self, stmt: Stmt) -> Result<Class, CompileError> {
        let line = stmt.line(self.parsed);
        let tree = &self.parsed.tree;
        match stmt {
            Stmt::ClassDecl(id) => {
                let node = &tree[id];
                let modifiers = self.walk_class_modifiers(self.list(node.modifiers), false)?;
                let Some(name) = node.name else {
                    return Err(unread(line, "a class"));
                };
                let parent = match node.extends {
                    Some(extends) => tree
                        .list(tree[extends].types)
                        .first()
                        .map(|id| self.text(*id).to_vec()),
                    None => None,
                };
                let interfaces = match node.implements {
                    Some(implements) => tree
                        .list(tree[implements].types)
                        .iter()
                        .map(|id| self.text(*id).to_vec())
                        .collect(),
                    None => Vec::new(),
                };
                let attributes = self.walk_attributes(self.list(node.attrs))?;
                self.walk_class(
                    ClassKind::Class,
                    Some(name),
                    parent,
                    interfaces,
                    attributes,
                    node.doc_comment,
                    node.body,
                    line,
                    node.range(),
                )
                .map(|class| Class { modifiers, ..class })
            }
            Stmt::InterfaceDecl(id) => {
                let node = &tree[id];
                let Some(name) = node.name else {
                    return Err(unread(line, "an interface"));
                };
                let interfaces = match node.extends {
                    Some(extends) => tree
                        .list(tree[extends].types)
                        .iter()
                        .map(|id| self.text(*id).to_vec())
                        .collect(),
                    None => Vec::new(),
                };
                let attributes = self.walk_attributes(self.list(node.attrs))?;
                self.walk_class(
                    ClassKind::Interface,
                    Some(name),
                    None,
                    interfaces,
                    attributes,
                    node.doc_comment,
                    node.body,
                    line,
                    node.range(),
                )
            }
            Stmt::TraitDecl(id) => {
                let node = &tree[id];
                let Some(name) = node.name else {
                    return Err(unread(line, "a trait"));
                };
                let attributes = self.walk_attributes(self.list(node.attrs))?;
                self.walk_class(
                    ClassKind::Trait,
                    Some(name),
                    None,
                    Vec::new(),
                    attributes,
                    node.doc_comment,
                    node.body,
                    line,
                    node.range(),
                )
            }
            Stmt::EnumDecl(id) => self.walk_enum_decl(id, line),
            _ => Err(self.internal("a statement that is no class declaration")),
        }
    }

    /// An `enum` declaration read from the tree, php's `zend_ast_decl`
    /// under `ZEND_ACC_ENUM`: the name, the backing type after the `:`,
    /// the interfaces, the attributes and the members, cases among them.
    fn walk_enum_decl(
        &mut self,
        id: NodeId<tree::EnumDecl>,
        line: u32,
    ) -> Result<Class, CompileError> {
        let tree = &self.parsed.tree;
        let node = &tree[id];
        let Some(name) = node.name else {
            return Err(unread(line, "an enum"));
        };
        let interfaces = match node.implements {
            Some(implements) => tree
                .list(tree[implements].types)
                .iter()
                .map(|id| self.text(*id).to_vec())
                .collect(),
            None => Vec::new(),
        };
        let backing = match node.backing {
            Some(backing) => match tree[backing].ty {
                Some(ty) => Some(ty),
                None => return Err(unread(line, "an enum's backing type")),
            },
            None => None,
        };
        let attributes = self.walk_attributes(self.list(node.attrs))?;
        let (doc_comment, body, range) = (node.doc_comment, node.body, node.range());
        let mut class = self.walk_class(
            ClassKind::Enum,
            Some(name),
            None,
            interfaces,
            attributes,
            doc_comment,
            body,
            line,
            range,
        )?;
        class.backing = backing;
        Ok(class)
    }

    /// The class of `new class (...) extends P implements I { ... }`,
    /// php's `anonymous_class` rule: a `zend_ast_decl` under
    /// `ZEND_ACC_ANON_CLASS` with its modifiers, its parent and its
    /// interfaces as a class's, its line the `class` keyword's, and no
    /// name written ([`Emit::anonymous_class`] makes it). The arguments
    /// are the `new`'s, which the caller sends.
    pub(super) fn walk_anon_class(
        &mut self,
        id: NodeId<tree::AnonClassDecl>,
    ) -> Result<Class, CompileError> {
        let tree = &self.parsed.tree;
        let node = &tree[id];
        let line = node.line;
        let modifiers = self.walk_class_modifiers(self.list(node.modifiers), true)?;
        let parent = node
            .extends
            .and_then(|extends| tree.list(tree[extends].types).first())
            .map(|id| self.text(*id).to_vec());
        let interfaces = match node.implements {
            Some(implements) => tree
                .list(tree[implements].types)
                .iter()
                .map(|id| self.text(*id).to_vec())
                .collect(),
            None => Vec::new(),
        };
        let attributes = self.walk_attributes(self.list(node.attrs))?;
        let (doc_comment, body, range) = (node.doc_comment, node.body, node.range());
        let class = self.walk_class(
            ClassKind::Class,
            None,
            parent,
            interfaces,
            attributes,
            doc_comment,
            body,
            line,
            range,
        )?;
        Ok(Class { modifiers, ..class })
    }

    /// A `function` declaration read from the tree, in the shape the
    /// emitter's `function_decl` takes.
    pub(super) fn walk_function_decl(
        &mut self,
        id: NodeId<tree::FunctionDecl>,
    ) -> Result<Function, CompileError> {
        let node = self.parsed.tree[id];
        let line = node.line();
        let attributes = self.walk_attributes(self.list(node.attrs))?;
        let name = self.text(node.name).to_vec();
        let function = self.walk_function(
            name,
            node.byref.is_some(),
            node.params,
            node.ret,
            node.body,
            false,
            node.doc_comment,
            line,
            node.range(),
        )?;
        Ok(Function {
            attributes,
            ..function
        })
    }

    /// The attributes before a declaration, `#[A(1), B] #[C]`, php's
    /// `attributed_statement` groups, each attribute with its arguments,
    /// positional, named and unpacked as written.
    ///
    /// ```php
    /// #[A] #[B(1), C] class M { public $x = 1; }
    /// var_dump((new M)->x);
    /// ```
    /// ```expect
    /// int(1)
    /// ```
    pub(super) fn walk_attributes(
        &mut self,
        groups: &[NodeId<tree::AttributeGroup>],
    ) -> Result<Vec<Attr>, CompileError> {
        let mut out = Vec::new();
        for group in groups {
            let attributes = self.list(self.parsed.tree[*group].attributes);
            for attribute in attributes {
                let attribute = self.parsed.tree[*attribute];
                let line = attribute.line();
                let arguments = match attribute.args {
                    Some(args) => self.attribute_arguments(args, line)?,
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

    /// The arguments of an attribute's list as written, which
    /// `zend_compile_attributes` holds to its rules, a first-class
    /// callable's `...` among them.
    fn attribute_arguments(
        &mut self,
        id: NodeId<tree::ArgList>,
        line: u32,
    ) -> Result<Vec<AttrArgument>, CompileError> {
        let args = self.list(self.parsed.tree[id].args);
        let mut out = Vec::with_capacity(args.len());
        for arg in args {
            out.push(match arg {
                tree::Arg::Expr(expr) => AttrArgument::Positional(*expr),
                tree::Arg::NamedArg(named) => {
                    let named = &self.parsed.tree[*named];
                    let Some(value) = named.value else {
                        return Err(unread(line, "an attribute's named argument"));
                    };
                    AttrArgument::Named(self.text(named.name).to_vec(), value)
                }
                tree::Arg::SpreadArg(_) => AttrArgument::Unpack,
                tree::Arg::VariadicPlaceholder(_) => AttrArgument::CallableConvert,
            });
        }
        Ok(out)
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
        bodyless: bool,
        doc_comment: Option<TokenId>,
        line: u32,
        range: TextRange,
    ) -> Result<Function, CompileError> {
        let parameters = self.walk_parameters(params)?;
        let return_type = match ret {
            Some(ret) => match self.parsed.tree[ret].ty {
                Some(ty) => Some(ty),
                None => return Err(unread(line, "a return type")),
            },
            None => None,
        };
        let body = match body {
            Some(body) => self.list(self.parsed.tree[body].stmts).to_vec(),
            // A trait's method written with no block, which
            // `zend_begin_method_decl` holds to its `abstract`; its op
            // array is the prologue and the final return alone.
            None if bodyless => Vec::new(),
            None => return Err(unread(line, "a function whose body is no block")),
        };
        let doc_comment = self.doc_comment(doc_comment);
        Ok(Function {
            name,
            parameters,
            return_type,
            by_ref,
            body,
            attributes: Vec::new(),
            doc_comment,
            line,
            end_line: self.end_line(range),
            generator: self.is_generator_at(range.start),
            void_return: false,
        })
    }

    /// The parameters of a list, php's `ZEND_AST_PARAM` each: the type,
    /// the `&`, the `...`, the name, the default and the promotion.
    fn walk_parameters(&mut self, id: NodeId<ParamList>) -> Result<Vec<Parameter>, CompileError> {
        let params = self.list(self.parsed.tree[id].params);
        let mut out = Vec::with_capacity(params.len());
        for param in params {
            let param = self.parsed.tree[*param];
            let line = param.line();
            let attributes = self.walk_attributes(self.list(param.attrs))?;
            let modifiers =
                self.walk_modifiers(self.list(param.modifiers), line, Target::Parameter)?;
            let name = self.variable_name(param.var).to_vec();
            let hooks = match param.hooks {
                Some(list) => Some(self.walk_hooks(list, &name)?),
                None => None,
            };
            if modifiers.has(Flag::Var) {
                return Err(self.internal("a parameter the grammar wrote `var` before"));
            }
            // `promotion_flags`, or a hook list: any of them promotes
            // (`is_promoted = property_flags || hooks_ast`).
            let is_readonly = modifiers.has(Flag::Readonly);
            let is_final = modifiers.has(Flag::Final);
            let promote = (modifiers.visibility.is_some()
                || modifiers.set_visibility.is_some()
                || is_readonly
                || is_final
                || hooks.is_some())
            .then(|| Promoted {
                visibility: modifiers.visibility.unwrap_or_default(),
                set_visibility: modifiers.set_visibility,
                is_readonly,
                is_final,
            });
            out.push(Parameter {
                name,
                ty: param.ty,
                line,
                by_ref: param.byref.is_some(),
                variadic: param.variadic.is_some(),
                default: param.default,
                promote,
                hooks,
                attributes,
                doc_comment: self.doc_comment(param.doc_comment),
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
    ///
    /// ```php
    /// class A { final final function f() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Multiple final modifiers are not allowed in %s on line 2
    /// ```
    ///
    /// A modifier the target takes no bit for is
    /// `zend_modifier_token_to_flag`'s refusal, and `abstract` beside
    /// `final` is `zend_add_member_modifier`'s, both before a repeat is
    /// refused:
    ///
    /// ```php
    /// class A { static const X = 1; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use the static modifier on a class constant in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { readonly function f() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use the readonly modifier on a method in %s on line 2
    /// ```
    ///
    /// ```php
    /// abstract class A { final abstract function f(); }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use the final modifier on an abstract method in %s on line 2
    /// ```
    fn walk_modifiers(
        &self,
        modifiers: &[Modifier],
        line: u32,
        target: Target,
    ) -> Result<Modifiers, CompileError> {
        let mut out = Modifiers::default();
        for modifier in modifiers {
            let written = match modifier {
                Modifier::PublicKw(_) => Written::Visibility(Visibility::Public),
                Modifier::ProtectedKw(_) => Written::Visibility(Visibility::Protected),
                Modifier::PrivateKw(_) => Written::Visibility(Visibility::Private),
                Modifier::VarKw(_) => Written::Flag(Flag::Var),
                Modifier::SetVisibility(id) => Written::SetVisibility(
                    match self.parsed.tree[*id].vis.map(|vis| {
                        self.parsed
                            .tokens
                            .get(vis.0 as usize)
                            .map(|token| token.kind)
                    }) {
                        Some(Some(SyntaxKind::PROTECTED_KW)) => Visibility::Protected,
                        Some(Some(SyntaxKind::PRIVATE_KW)) => Visibility::Private,
                        _ => Visibility::Public,
                    },
                ),
                Modifier::StaticKw(_) => Written::Flag(Flag::Static),
                Modifier::AbstractKw(_) => Written::Flag(Flag::Abstract),
                Modifier::FinalKw(_) => Written::Flag(Flag::Final),
                Modifier::ReadonlyKw(_) => Written::Flag(Flag::Readonly),
            };
            if !written.takes(target) {
                return Err(thrown(
                    line,
                    format!(
                        "Cannot use the {} modifier on a {}",
                        written.word(),
                        target.member()
                    ),
                ));
            }
            // `zend_add_member_modifier`, then the repeat
            // `zend_modifier_list_to_flags` holds back until it has run.
            let multiple_access = || thrown(line, "Multiple access type modifiers are not allowed");
            let repeated = match written {
                Written::Visibility(visibility) => {
                    if out.visibility.is_some() {
                        return Err(multiple_access());
                    }
                    out.visibility = Some(visibility);
                    false
                }
                Written::Flag(Flag::Var) => {
                    // `var` is `ZEND_ACC_PUBLIC` to the grammar.
                    if out.visibility.is_some() {
                        return Err(multiple_access());
                    }
                    out.visibility = Some(Visibility::Public);
                    out.flags.push(Flag::Var);
                    false
                }
                Written::SetVisibility(visibility) => {
                    if out.set_visibility.is_some() {
                        return Err(multiple_access());
                    }
                    out.set_visibility = Some(visibility);
                    false
                }
                Written::Flag(flag) => {
                    let repeated = out.has(flag);
                    out.flags.push(flag);
                    repeated
                }
            };
            if out.has(Flag::Abstract) && out.has(Flag::Final) {
                match target {
                    Target::Method => {
                        return Err(thrown(
                            line,
                            "Cannot use the final modifier on an abstract method",
                        ));
                    }
                    Target::Property => {
                        return Err(thrown(
                            line,
                            "Cannot use the final modifier on an abstract property",
                        ));
                    }
                    Target::Constant | Target::Parameter | Target::Hook => {}
                }
            }
            if repeated {
                return Err(thrown(
                    line,
                    format!("Multiple {} modifiers are not allowed", written.word()),
                ));
            }
        }
        Ok(out)
    }

    /// A class's modifiers read once, php's `class_modifiers`: each one
    /// after the first goes through `zend_add_class_modifier`, which
    /// refuses a repeat and `abstract` beside `final` at the line of the
    /// modifier that made it.
    ///
    /// ```php
    /// abstract
    /// final class A {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use the final modifier on an abstract class in %s on line 3
    /// ```
    ///
    /// ```php
    /// readonly final readonly class A {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Multiple readonly modifiers are not allowed in %s on line 2
    /// ```
    ///
    /// ```php
    /// final abstract class A {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use the final modifier on an abstract class in %s on line 2
    /// ```
    ///
    /// What php takes, it takes, each modifier once in any order:
    ///
    /// ```php
    /// abstract class A {} final class F {} readonly final class R { public int $x; }
    /// var_dump((new ReflectionClass('A'))->isAbstract(), (new ReflectionClass('F'))->isFinal(),
    ///     (new ReflectionClass('R'))->isReadOnly(), (new ReflectionClass('R'))->isFinal());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// ```
    ///
    /// An anonymous class's go through `zend_add_anonymous_class_modifier`
    /// instead, which takes `readonly` alone:
    ///
    /// ```php
    /// echo "never\n";
    /// $o = new final class {};
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use the final modifier on an anonymous class in %s on line 3
    /// ```
    ///
    /// ```php
    /// $o = new abstract class {};
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use the abstract modifier on an anonymous class in %s on line 2
    /// ```
    ///
    /// ```php
    /// $o = new readonly readonly class {};
    /// ```
    /// ```expect exit=255
    /// Fatal error: Multiple readonly modifiers are not allowed in %s on line 2
    /// ```
    fn walk_class_modifiers(
        &self,
        modifiers: &[Modifier],
        anonymous: bool,
    ) -> Result<ClassModifiers, CompileError> {
        let mut out = ClassModifiers::default();
        for modifier in modifiers {
            let (token, flag, word) = match modifier {
                Modifier::AbstractKw(token) => (*token, &mut out.is_abstract, "abstract"),
                Modifier::FinalKw(token) => (*token, &mut out.is_final, "final"),
                Modifier::ReadonlyKw(token) => (*token, &mut out.is_readonly, "readonly"),
                Modifier::PublicKw(_)
                | Modifier::ProtectedKw(_)
                | Modifier::PrivateKw(_)
                | Modifier::VarKw(_)
                | Modifier::StaticKw(_)
                | Modifier::SetVisibility(_) => {
                    return Err(self.internal("a class modifier the grammar does not take"));
                }
            };
            let line = self
                .parsed
                .tokens
                .get(token.0 as usize)
                .map_or(self.line, |token| {
                    self.parsed.lines.line_number(token.offset)
                });
            if anonymous && word != "readonly" {
                return Err(thrown(
                    line,
                    format!("Cannot use the {word} modifier on an anonymous class"),
                ));
            }
            if *flag {
                return Err(thrown(
                    line,
                    format!("Multiple {word} modifiers are not allowed"),
                ));
            }
            *flag = true;
            if out.is_abstract && out.is_final {
                return Err(thrown(
                    line,
                    "Cannot use the final modifier on an abstract class",
                ));
            }
        }
        Ok(out)
    }

    /// A class or an interface from the tree: its members as php's
    /// `ZEND_AST_PROP_GROUP`, `ZEND_AST_CLASS_CONST_GROUP` and
    /// `ZEND_AST_METHOD` hold them, one declarator each.
    ///
    /// A class may not take a name `reserved_class_names` lists, and one
    /// called `_` is php's 8.4 deprecation:
    ///
    /// ```php
    /// class _ {}
    /// echo "ok\n";
    /// ```
    /// ```expect exit=0
    /// Deprecated: Using "_" as a class name is deprecated since 8.4 in %s on line 2
    /// ok
    /// ```
    ///
    /// An anonymous class has no name written, and none is held.
    #[allow(clippy::too_many_arguments)]
    fn walk_class(
        &mut self,
        kind: ClassKind,
        name: Option<TokenId>,
        parent: Option<Vec<u8>>,
        interfaces: Vec<Vec<u8>>,
        attributes: Vec<Attr>,
        doc_comment: Option<TokenId>,
        body: NodeId<ClassBody>,
        line: u32,
        range: TextRange,
    ) -> Result<Class, CompileError> {
        let name = name.map_or_else(Vec::new, |name| self.text(name).to_vec());
        if super::types::is_reserved_class_name(&name) {
            return Err(refused(
                line,
                format!(
                    "Cannot use \"{}\" as {} as it is reserved",
                    String::from_utf8_lossy(&name),
                    class_name_kind(kind)
                ),
            ));
        }
        if name == b"_" {
            self.diagnostics.push(php_unit::Diagnostic {
                level: Level::Deprecated,
                line,
                message: format!(
                    "Using \"_\" as {} is deprecated since 8.4",
                    class_name_kind(kind)
                )
                .as_str()
                .into(),
            });
        }
        let mut class = Class {
            name,
            kind,
            modifiers: ClassModifiers::default(),
            backing: None,
            parent,
            interfaces,
            traits: Vec::new(),
            attributes,
            doc_comment: self.doc_comment(doc_comment),
            body: Vec::new(),
            statement_lines: Vec::new(),
            line,
            end_line: self.end_line(range),
        };
        let members = self.list(self.parsed.tree[body].class_members);
        for member in members {
            let line = match member {
                ClassMember::PropertyDecl(id) => Some(self.parsed.tree[*id].line()),
                ClassMember::EnumCaseDecl(id) => Some(self.parsed.tree[*id].line()),
                _ => None,
            };
            class.statement_lines.extend(line);
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
            ClassMember::TraitUseDecl(id) => self.walk_trait_use(class, id),
            ClassMember::EnumCaseDecl(id) => self.walk_case(class, id),
        }
    }

    /// One `use A, B { ... }` of a class body, php's `ZEND_AST_USE_TRAIT`:
    /// the names as written and each adaptation of its list, the method
    /// references, the names an `insteadof` excludes, and an `as`'s
    /// modifier and alias.
    fn walk_trait_use(
        &mut self,
        class: &mut Class,
        id: NodeId<tree::TraitUseDecl>,
    ) -> Result<(), CompileError> {
        let node = self.parsed.tree[id];
        let names = self
            .list(node.traits)
            .iter()
            .map(|t| self.text(*t).to_vec())
            .collect();
        let mut adaptations = Vec::new();
        if let Some(list) = node.adaptations {
            let list = self.list(self.parsed.tree[list].trait_adaptations);
            for adaptation in list {
                adaptations.push(self.walk_adaptation(*adaptation)?);
            }
        }
        class.traits.push(TraitUse {
            names,
            adaptations,
            line: node.line(),
        });
        Ok(())
    }

    /// One adaptation of a trait `use`, `ZEND_AST_TRAIT_PRECEDENCE` or
    /// `ZEND_AST_TRAIT_ALIAS`, its modifier read as
    /// `zend_modifier_token_to_flag` reads a method's.
    fn walk_adaptation(
        &self,
        adaptation: tree::TraitAdaptation,
    ) -> Result<Adaptation, CompileError> {
        let tree = &self.parsed.tree;
        let (reference, line) = match adaptation {
            tree::TraitAdaptation::InsteadofAdaptation(id) => (tree[id].r#ref, tree[id].line()),
            tree::TraitAdaptation::AsAdaptation(id) => (tree[id].r#ref, tree[id].line()),
        };
        let reference = &tree[reference];
        let Some(method) = reference.method.and_then(|m| tree[m].name) else {
            return Err(unread(line, "a trait adaptation naming no method"));
        };
        // `static::m` is no name to the tree, which reads `static` as a
        // keyword; php resolves it as a trait name and refuses it as
        // reserved, which the compile does from its spelling.
        let range = reference.range();
        let written = self
            .parsed
            .source
            .get(range.start as usize..(range.start + range.len) as usize)
            .unwrap_or_default();
        let trait_name = match reference.r#trait {
            Some(t) => Some(self.text(t).to_vec()),
            None if written.len() > 6
                && written[..6].eq_ignore_ascii_case(b"static")
                && written[6..].trim_ascii_start().starts_with(b"::") =>
            {
                Some(written[..6].to_vec())
            }
            None => None,
        };
        let method = self.text(method).to_vec();
        match adaptation {
            tree::TraitAdaptation::InsteadofAdaptation(id) => {
                let Some(trait_name) = trait_name else {
                    return Err(unread(line, "an `insteadof` naming no trait"));
                };
                Ok(Adaptation::Insteadof {
                    trait_name,
                    method,
                    excluded: tree
                        .list(tree[id].excluded)
                        .iter()
                        .map(|t| self.text(*t).to_vec())
                        .collect(),
                    line,
                })
            }
            tree::TraitAdaptation::AsAdaptation(id) => {
                let node = &tree[id];
                let modifier = match node
                    .vis
                    .and_then(|vis| self.parsed.tokens.get(vis.0 as usize))
                {
                    None => None,
                    Some(token) => Some(match token.kind {
                        SyntaxKind::PUBLIC_KW => AsModifier::Visibility(Visibility::Public),
                        SyntaxKind::PROTECTED_KW => AsModifier::Visibility(Visibility::Protected),
                        SyntaxKind::PRIVATE_KW => AsModifier::Visibility(Visibility::Private),
                        SyntaxKind::FINAL_KW => AsModifier::Final,
                        SyntaxKind::STATIC_KW => AsModifier::Static,
                        SyntaxKind::ABSTRACT_KW => AsModifier::Abstract,
                        SyntaxKind::READONLY_KW => AsModifier::Readonly,
                        _ => {
                            return Err(
                                self.internal("an `as` modifier that is no member modifier")
                            );
                        }
                    }),
                };
                let alias = match node.alias {
                    Some(alias) => match tree[alias].name {
                        Some(name) => Some(self.text(name).to_vec()),
                        None => return Err(unread(line, "a trait alias")),
                    },
                    None => None,
                };
                Ok(Adaptation::As {
                    trait_name,
                    method,
                    modifier,
                    alias,
                    line,
                })
            }
        }
    }

    /// One `case A;` or `case A = expr;`, php's `ZEND_AST_ENUM_CASE`,
    /// which `zend_compile_enum_case` refuses outside an enum.
    ///
    /// ```php
    /// class C { case A; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Case can only be used in enums in %s on line 2
    /// ```
    fn walk_case(
        &mut self,
        class: &mut Class,
        id: NodeId<tree::EnumCaseDecl>,
    ) -> Result<(), CompileError> {
        let node = self.parsed.tree[id];
        let line = node.line();
        if class.kind != ClassKind::Enum {
            return Err(refused(line, "Case can only be used in enums"));
        }
        let attributes = self.walk_attributes(self.list(node.attrs))?;
        let Some(name) = node.name.and_then(|name| self.parsed.tree[name].name) else {
            return Err(unread(line, "an enum case whose name is no identifier"));
        };
        class.body.push(ClassStmt::Constant(Constant {
            name: self.text(name).to_vec(),
            visibility: Visibility::Public,
            is_final: false,
            ty: None,
            value: node.value,
            is_case: true,
            attributes,
            doc_comment: self.doc_comment(node.doc_comment),
            line,
        }));
        Ok(())
    }

    /// One `const A = 1, B = 2;` of a class body, php's
    /// `ZEND_AST_CLASS_CONST_GROUP`: the group's modifiers beside each
    /// declarator, whose doc comment and line are its own.
    fn walk_constants(
        &mut self,
        class: &mut Class,
        id: NodeId<tree::ClassConstDecl>,
    ) -> Result<(), CompileError> {
        let node = self.parsed.tree[id];
        let line = node.line();
        let modifiers = self.walk_modifiers(self.list(node.modifiers), line, Target::Constant)?;
        if modifiers.has(Flag::Var) {
            return Err(self.internal("a class constant the grammar wrote `var` before"));
        }
        for declarator in self.list(node.const_declarators) {
            let (name, value, line) = self.const_declarator(*declarator)?;
            let attributes = self.walk_attributes(self.list(node.attrs))?;
            let doc_comment = self.doc_comment(self.parsed.tree[*declarator].doc_comment);
            class.body.push(ClassStmt::Constant(Constant {
                name,
                visibility: modifiers.visibility.unwrap_or_default(),
                is_final: modifiers.has(Flag::Final),
                ty: node.ty,
                value: Some(value),
                is_case: false,
                attributes,
                doc_comment,
                line,
            }));
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
        let node = self.parsed.tree[id];
        let line = node.line();
        let modifiers = self.walk_modifiers(self.list(node.modifiers), line, Target::Property)?;
        if let Some(list) = node.hooks
            && node.property_declarators.len() > 1
        {
            // php's `hooked_property` is one `T_VARIABLE`, so a list
            // after a second declarator is the parser's.
            let at = self.parsed.tree[list].line();
            return Err(CompileError {
                level: Level::Parse,
                message: String::from(
                    "syntax error, unexpected token \"{\", expecting \",\" or \";\"",
                ),
                line: at,
                raised: Vec::new(),
                thrown: false,
                stage: crate::Stage::Compiling,
            });
        }
        for declarator in self.list(node.property_declarators) {
            let declarator = self.parsed.tree[*declarator];
            let attributes = self.walk_attributes(self.list(node.attrs))?;
            let name = self.variable_name(declarator.var).to_vec();
            let hooks = match node.hooks {
                Some(list) => Some(self.walk_hooks(list, &name)?),
                None => None,
            };
            class.body.push(ClassStmt::Property(Property {
                name,
                hooks,
                visibility: modifiers.visibility.unwrap_or_default(),
                set_visibility: modifiers.set_visibility,
                ty: node.ty,
                is_static: modifiers.has(Flag::Static),
                is_final: modifiers.has(Flag::Final),
                is_abstract: modifiers.has(Flag::Abstract),
                is_readonly: modifiers.has(Flag::Readonly),
                attributes,
                default: declarator.init,
                doc_comment: self.doc_comment(declarator.doc_comment),
                line: declarator.line(),
            }));
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
        let node = self.parsed.tree[id];
        let line = node.line();
        let modifiers = self.walk_modifiers(self.list(node.modifiers), line, Target::Method)?;
        if modifiers.has(Flag::Var) {
            return Err(self.internal("a method the grammar wrote `var` before"));
        }
        let attributes = self.walk_attributes(self.list(node.attrs))?;
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
            true,
            node.doc_comment,
            line,
            node.range(),
        )?;
        class.body.push(ClassStmt::Method(Method {
            function: Function {
                attributes,
                ..function
            },
            visibility: modifiers.visibility.unwrap_or_default(),
            is_static: modifiers.has(Flag::Static),
            is_abstract: modifiers.has(Flag::Abstract),
            has_body: node.body.is_some(),
            is_final: modifiers.has(Flag::Final),
        }));
        Ok(())
    }
}

/// The refusal for a construct the emitter cannot emit yet, at the
/// node's own line: the `Level::Parse` sentence the phpt lane reads as
/// unread.
pub(super) fn unread(line: u32, what: &str) -> CompileError {
    CompileError {
        level: Level::Parse,
        message: format!("{what} this compiler cannot read yet"),
        line,
        raised: Vec::new(),
        thrown: false,
        stage: crate::Stage::Compiling,
    }
}

/// What `zend_assert_valid_class_name` calls a declaration's name in
/// its refusals, by the kind being declared.
fn class_name_kind(kind: ClassKind) -> &'static str {
    match kind {
        ClassKind::Interface => "an interface name",
        ClassKind::Enum => "an enum name",
        ClassKind::Trait => "a trait name",
        ClassKind::Class => "a class name",
    }
}

/// php's compile error at the node's own line.
pub(super) fn refused(line: u32, message: impl Into<String>) -> CompileError {
    CompileError {
        level: Level::CompileError,
        message: message.into(),
        line,
        raised: Vec::new(),
        thrown: false,
        stage: crate::Stage::Compiling,
    }
}

/// The refusal php throws as a `CompileError`
/// (`zend_throw_exception(zend_ce_compile_error)`), which a `try`
/// around the `eval` or the `include` catches: every refusal of a
/// modifier list, `zend_modifier_token_to_flag`'s,
/// `zend_add_member_modifier`'s, `zend_add_class_modifier`'s and the
/// repeat `zend_modifier_list_to_flags` holds back; and, built beside
/// it, a nested `__HALT_COMPILER()` and `zend_handle_encoding_declaration`'s
/// refusal of an encoding the parser cannot read as it stands.
///
/// ```php
/// foreach (['class C { final final function f() {} }', 'class D { private protected $x; }', 'abstract final class E {}', 'if (1) { __HALT_COMPILER(); }', 'declare(encoding=1+1);'] as $code) {
///     try { eval($code); } catch (CompileError $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// CompileError: Multiple final modifiers are not allowed
/// CompileError: Multiple access type modifiers are not allowed
/// CompileError: Cannot use the final modifier on an abstract class
/// CompileError: __HALT_COMPILER() can only be used from the outermost scope
/// CompileError: Encoding must be a literal
/// ```
pub(super) fn thrown(line: u32, message: impl Into<String>) -> CompileError {
    CompileError {
        thrown: true,
        ..refused(line, message)
    }
}

// ----- the shapes a declaration reaches the emitter in -----
//
// What the walk above reads a class, a function, a closure and the
// members and parameters under them into, each named after the tree
// node it stands for and the `zend_ast` kind that node is, with the
// expressions under it the tree's own nodes (design §4.9, #595).

/// One `namespace` statement, php's `ZEND_AST_NAMESPACE`, in the form
/// `zend_compile_namespace` tells apart by whether `child[1]` stands.
#[derive(Debug, PartialEq)]
pub(super) enum Namespace {
    /// `namespace N;`: what follows is in `N`, up to the next one.
    Unbracketed(Vec<u8>),
    /// `namespace N { … }`, and `namespace { … }`, whose name is none
    /// and whose statements are the global namespace's: the braces'
    /// statements are top statements, compiled as the file's own are.
    Bracketed {
        name: Option<Vec<u8>>,
        body: Vec<Stmt>,
    },
}

/// One class declaration, php's `zend_ast_decl` for a class: the name as
/// written, the doc comment taken before its `{` (`class_declaration_statement`
/// reduces `backup_doc_comment` after `implements_list`), and its
/// members as php's `ZEND_AST_PROP_GROUP` and `ZEND_AST_CLASS_CONST_GROUP`
/// hold them, one declarator each so the doc comment and the line are
/// the declarator's own.
///
/// `extends_from` is the parent of a class, `child[0]`; `implements_list`
/// is `child[1]`, and an interface's own `extends` list is the same
/// child, since `interface_declaration_statement` hands its
/// `interface_extends_list` to `zend_ast_create_decl` where a class
/// hands its `implements_list`.
#[derive(Debug, PartialEq)]
pub(super) struct Class {
    pub(super) name: Vec<u8>,
    pub(super) kind: ClassKind,
    /// `abstract`, `final` and `readonly` as a class's `class_modifiers`
    /// wrote them; none on every other kind.
    pub(super) modifiers: ClassModifiers,
    /// An enum's backing type as written after the `:`, php's
    /// `decl->child[4]`; none for a pure enum and for every other kind.
    pub(super) backing: Option<TypeExpr>,
    pub(super) parent: Option<Vec<u8>>,
    pub(super) interfaces: Vec<Vec<u8>>,
    /// Each `use` of a trait the body writes, in the order written.
    pub(super) traits: Vec<TraitUse>,
    pub(super) attributes: Vec<Attr>,
    pub(super) doc_comment: Option<Vec<u8>>,
    /// The constants, the enum cases, the properties and the methods,
    /// in the order written, which is the order
    /// `zend_compile_class_decl` compiles them in (`zend_compile_stmt`
    /// over the member list), so each sees in the class's tables what
    /// stands above it and nothing below.
    pub(super) body: Vec<ClassStmt>,
    /// The line of each member `zend_compile_stmt` compiles as a
    /// statement of the op array the class is declared in, in written
    /// order: a property group and an enum case, the members
    /// `zend_is_unticked_stmt` does not name
    /// ([`Emit::member_statements`]).
    pub(super) statement_lines: Vec<u32>,
    /// The line the name is on, `ce->info.user.line_start`, which is
    /// where php raises what `zend_compile_attributes` refuses and not
    /// where the attribute itself was written.
    pub(super) line: u32,
    /// The line of the closing brace, `ce->info.user.line_end`.
    pub(super) end_line: u32,
}

/// One member of a class body that declares something in the class's
/// tables: a constant or an enum case, one property declarator, or a
/// method.
#[derive(Debug, PartialEq)]
pub(super) enum ClassStmt {
    Constant(Constant),
    Property(Property),
    Method(Method),
}

/// One `use` of traits in a class body, php's `ZEND_AST_USE_TRAIT`: the
/// names as written, before `zend_resolve_const_class_name_reference`
/// resolves them, the adaptations of its list, and the line of the
/// `use`, where `zend_compile_use_trait` raises.
#[derive(Debug, PartialEq)]
pub(super) struct TraitUse {
    pub(super) names: Vec<Vec<u8>>,
    pub(super) adaptations: Vec<Adaptation>,
    pub(super) line: u32,
}

/// One adaptation as written, php's `ZEND_AST_TRAIT_PRECEDENCE` or
/// `ZEND_AST_TRAIT_ALIAS`, with the trait names still unresolved.
#[derive(Debug, PartialEq)]
pub(super) enum Adaptation {
    Insteadof {
        trait_name: Vec<u8>,
        method: Vec<u8>,
        excluded: Vec<Vec<u8>>,
        line: u32,
    },
    As {
        trait_name: Option<Vec<u8>>,
        method: Vec<u8>,
        modifier: Option<AsModifier>,
        alias: Option<Vec<u8>>,
        line: u32,
    },
}

/// The modifier token an `as` wrote, before
/// `zend_check_trait_alias_modifiers` refuses the ones a trait alias
/// may not take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AsModifier {
    Visibility(Visibility),
    Final,
    Static,
    Abstract,
    Readonly,
}

/// One attribute as written, php's `ZEND_AST_ATTRIBUTE`: the class name
/// before it is resolved, and the arguments of its list, still
/// expressions. `zend_compile_attributes` is what turns a group of
/// these into a declaration's, so a group is not a thing that outlives
/// the reader: `#[A] #[B]` and `#[A, B]` are the same two attributes.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Attr {
    pub(super) name: Vec<u8>,
    pub(super) arguments: Vec<AttrArgument>,
    pub(super) line: u32,
}

/// One argument of an attribute's list as written, php's
/// `ZEND_AST_ARG_LIST` child: an expression, `name: expression`, or
/// `...expression`, which `zend_compile_attributes` refuses, or the
/// `...` that makes the list php's `ZEND_AST_CALLABLE_CONVERT`, which it
/// refuses too.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum AttrArgument {
    Positional(php_syntax::tree::Expr),
    Named(Vec<u8>, php_syntax::tree::Expr),
    Unpack,
    CallableConvert,
}

/// One function declaration, php's `zend_ast_decl` for a function or a
/// method: the name as written, the parameters, the return type where
/// one is written after the `)`, whether an `&` before the name returns
/// it by reference (`ZEND_ACC_RETURN_REFERENCE`), the body's statement
/// nodes, which the emitter walks from the tree, the doc comment taken
/// before the `function` keyword, and the lines of the keyword and of
/// the closing brace, `start_lineno` and `end_lineno`.
#[derive(Debug, PartialEq)]
pub(super) struct Function {
    pub(super) name: Vec<u8>,
    pub(super) parameters: Vec<Parameter>,
    /// The return type as written, which the emitter compiles where
    /// `zend_compile_params` does, before the first parameter.
    pub(super) return_type: Option<TypeExpr>,
    pub(super) by_ref: bool,
    pub(super) body: Vec<php_syntax::tree::Stmt>,
    /// The attributes written before it, `decl->child[4]`, which the
    /// emitter compiles for the function or the method target.
    pub(super) attributes: Vec<Attr>,
    pub(super) doc_comment: Option<Vec<u8>>,
    pub(super) line: u32,
    pub(super) end_line: u32,
    /// `ZEND_ACC_GENERATOR`: a `yield` stands in the body itself
    /// ([`generators`]).
    pub(super) generator: bool,
    /// The `void` `zend_compile_property_hooks` writes as a `set`
    /// hook's return type, where `return_type` is none.
    pub(super) void_return: bool,
}

/// One parameter, php's `ZEND_AST_PARAM`: the declared type where one is
/// written, `ZEND_PARAM_REF` for an `&` and `ZEND_PARAM_VARIADIC` for a
/// `...` before the variable, the name, the default, and the modifiers
/// that promote it to a property in a constructor.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Parameter {
    pub(super) name: Vec<u8>,
    /// The type as written, compiled by the emitter.
    pub(super) ty: Option<TypeExpr>,
    /// The parameter's own line, `param_ast->lineno`, which
    /// `zend_compile_params` sets `CG(zend_lineno)` to before it.
    pub(super) line: u32,
    pub(super) by_ref: bool,
    pub(super) variadic: bool,
    pub(super) default: Option<php_syntax::tree::Expr>,
    pub(super) promote: Option<Promoted>,
    /// The hook list after it, `param_ast->child[5]`, which alone
    /// promotes it too; none where no list is written.
    pub(super) hooks: Option<Vec<Hook>>,
    /// The attributes written before it, `param_ast->child[3]`.
    pub(super) attributes: Vec<Attr>,
    /// The doc comment before the parameter, `param_ast->child[4]`,
    /// which `zend_compile_params` hands the property a promoted
    /// parameter declares.
    pub(super) doc_comment: Option<Vec<u8>>,
}

/// The modifiers that make a constructor's parameter a property,
/// php's `promotion_flags` in `zend_compile_params`: the visibility, the
/// set visibility as written, and the `readonly` and `final` bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Promoted {
    pub(super) visibility: Visibility,
    pub(super) set_visibility: Option<Visibility>,
    pub(super) is_readonly: bool,
    pub(super) is_final: bool,
}

/// One method, php's `ZEND_AST_METHOD`: a function with the group's
/// modifiers beside it.
#[allow(clippy::struct_excessive_bools)] // the ZEND_ACC_* bits as written, and whether a block follows
#[derive(Debug, PartialEq)]
pub(super) struct Method {
    pub(super) function: Function,
    pub(super) visibility: Visibility,
    pub(super) is_static: bool,
    /// `ZEND_ACC_ABSTRACT` as written, and whether a block follows the
    /// signature, the two facts `zend_begin_method_decl` holds against
    /// each other.
    pub(super) is_abstract: bool,
    pub(super) has_body: bool,
    pub(super) is_final: bool,
}

/// One closure as written, php's `ZEND_AST_CLOSURE` and its
/// `ZEND_AST_ARROW_FUNC`: the function itself, whose `name` is empty
/// because `zend_begin_func_decl` builds `{closure:...}` from where it
/// stands; the `use` list as written, empty for an arrow
/// function, whose captures are found in its body
/// (`find_implicit_binds`); whether `static` stands before the
/// keyword, php's `ZEND_ACC_STATIC`, which keeps `$this` out of the
/// body; and for an arrow function the expression after `=>`, the one
/// `return` php makes its body of (`zend_compile_func_decl`), the
/// function's own statement list being empty.
#[derive(Debug, PartialEq)]
pub(super) struct Closure {
    pub(super) function: Function,
    pub(super) uses: Vec<Use>,
    pub(super) is_static: bool,
    pub(super) arrow: Option<php_syntax::tree::Expr>,
}

/// One `use` variable of a closure, php's `lexical_var`: the name
/// without its `$`, whether an `&` stands before it
/// (`ZEND_BIND_REF`), and the line the name is written on, which
/// `zend_compile_closure_binding` sets `CG(zend_lineno)` to before the
/// `BIND_LEXICAL` it emits.
#[derive(Debug, PartialEq)]
pub(super) struct Use {
    pub(super) name: Vec<u8>,
    pub(super) by_ref: bool,
    pub(super) line: u32,
}

/// One property declarator, php's `ZEND_AST_PROP_ELEM` with the group's
/// modifiers and type beside it: `public $x`, `static $y = 1`,
/// `public private(set) int $z`. Its doc comment is what was pending
/// after its last token, php's `property: T_VARIABLE
/// backup_doc_comment | T_VARIABLE '=' expr backup_doc_comment`.
/// `set_visibility` is the group's `public(set)`, `protected(set)` or
/// `private(set)` as written, before `zend_declare_typed_property`
/// holds it to the read visibility.
#[derive(Debug, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // the ZEND_ACC_* bits of a property as written, each one fact
pub(super) struct Property {
    pub(super) name: Vec<u8>,
    /// The hook list, `prop_ast->child[3]`: none where the declarator
    /// ends in `;`, and an empty list for `{}`, which
    /// `zend_compile_property_hooks` refuses.
    pub(super) hooks: Option<Vec<Hook>>,
    pub(super) visibility: Visibility,
    pub(super) set_visibility: Option<Visibility>,
    /// The group's type as written, compiled by the emitter once per
    /// declarator, as `zend_compile_prop_decl` compiles it.
    pub(super) ty: Option<TypeExpr>,
    pub(super) is_static: bool,
    /// `ZEND_ACC_FINAL`, `ZEND_ACC_ABSTRACT` and `ZEND_ACC_READONLY` as
    /// the group wrote them, before `zend_compile_prop_decl` holds them
    /// to the class and to each other.
    pub(super) is_final: bool,
    pub(super) is_abstract: bool,
    pub(super) is_readonly: bool,
    /// The group's attributes, compiled for each declarator.
    pub(super) attributes: Vec<Attr>,
    pub(super) default: Option<php_syntax::tree::Expr>,
    pub(super) doc_comment: Option<Vec<u8>>,
    pub(super) line: u32,
}

/// One class constant declarator, php's `ZEND_AST_CONST_ELEM` with the
/// group's modifiers beside it: `const A = 1`, `final protected const B = 2`.
/// Its doc comment is taken after its value, `class_const_decl: identifier
/// '=' expr backup_doc_comment`.
#[derive(Debug, PartialEq)]
pub(super) struct Constant {
    pub(super) name: Vec<u8>,
    pub(super) visibility: Visibility,
    pub(super) is_final: bool,
    /// The group's type as written, compiled by the emitter once per
    /// declarator, as `zend_compile_class_const_decl` compiles it.
    pub(super) ty: Option<TypeExpr>,
    /// The value as written; a `case` of a pure enum has none.
    pub(super) value: Option<php_syntax::tree::Expr>,
    /// `case A;` or `case A = 1;` in an enum's body, php's
    /// `ZEND_AST_ENUM_CASE`, which stands in the constants table beside
    /// the enum's own constants in the order written.
    pub(super) is_case: bool,
    /// The attributes written before a case, `ast->child[3]` of
    /// `ZEND_AST_ENUM_CASE`, or before a constant's group, compiled for
    /// each declarator.
    pub(super) attributes: Vec<Attr>,
    pub(super) doc_comment: Option<Vec<u8>>,
    pub(super) line: u32,
}

/// The offset `__COMPILER_HALT_OFFSET__` compiles to in a file whose top
/// statements end with `__halt_compiler();`, as `zend_compile_const`
/// finds that statement at the end of `CG(ast)`; none in any other.
pub(crate) fn halt_offset(parsed: &Parsed) -> Option<i64> {
    let file = parsed.tree.root()?.id::<tree::SourceFile>()?;
    match parsed.tree.list(parsed.tree[file].stmts).last()? {
        Stmt::HaltCompilerStmt(id) => Some(halt_end(parsed, *id)),
        _ => None,
    }
}

/// `zend_get_scanned_file_offset()` as the parser reduces
/// `T_HALT_COMPILER '(' ')' ';'`: the byte after the `;`, or after the
/// `?>` that stands for it and the newline it swallowed, which is where
/// the lexer's `HALT_DATA` trailer begins, and the end of the source
/// where there is none.
pub(super) fn halt_end(parsed: &Parsed, id: NodeId<tree::HaltCompilerStmt>) -> i64 {
    let data = parsed.tree[id]
        .data
        .and_then(|t| parsed.tokens.get(usize::try_from(t.0).ok()?))
        .map(|t| i64::from(t.offset));
    data.unwrap_or_else(|| i64::try_from(parsed.source.len()).unwrap_or(i64::MAX))
}
