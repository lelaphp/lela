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
//! handed to the emitter in the shape [`crate::read`] gave it, its
//! body and its defaults the tree's own nodes, so every construct's
//! semantics stays in the one function that had it and nothing here
//! decides what an opline is; those shapes go as the emitter learns to
//! read the declarations directly (#595).
//!
//! **What is refused.** A construct the tree parses and the emitter
//! cannot emit yet is refused here, at the node's line, in the one-line
//! `this compiler cannot read yet` shape at `Level::Parse`, which the
//! phpt lane counts as unread; what the grammar does not parse is php's
//! parse error from `Parsed::diagnostics`, raised before this walk runs.

use php_syntax::tree::{
    Block, ClassBody, ClassMember, Modifier, Node, NodeId, ParamList, Stmt, TokenId, TypeExpr,
};
use php_syntax::{SyntaxKind, TextRange, tree};
use php_unit::Level;
use php_unit::decl::{ClassKind, Name, Type, Visibility};

mod expr;

use super::Emit;
use crate::CompileError;
use crate::read::{self, Attr, Class, Constant, Function, Method, Parameter, Promoted, Property};

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
        let stmts = &self.parsed.tree[file].stmts;
        self.statements(stmts)
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

    /// The line a node's last byte stands on: a declaration's
    /// `end_lineno`, the line of its closing brace.
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
                let attributes = self.walk_attributes(&node.attrs)?;
                self.walk_class(
                    ClassKind::Class,
                    name,
                    parent,
                    interfaces,
                    attributes,
                    node.doc_comment,
                    node.body,
                    line,
                    node.range(),
                )
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
                let attributes = self.walk_attributes(&node.attrs)?;
                self.walk_class(
                    ClassKind::Interface,
                    name,
                    None,
                    interfaces,
                    attributes,
                    node.doc_comment,
                    node.body,
                    line,
                    node.range(),
                )
            }
            _ => Err(self.internal("a statement that is no class declaration")),
        }
    }

    /// A `function` declaration read from the tree, in the shape the
    /// emitter's `function_decl` takes.
    pub(super) fn walk_function_decl(
        &mut self,
        id: NodeId<tree::FunctionDecl>,
    ) -> Result<Function, CompileError> {
        let node = &self.parsed.tree[id];
        let line = node.line();
        if !node.attrs.is_empty() {
            return Err(unread(line, "an attribute before this declaration"));
        }
        let name = self.text(node.name).to_vec();
        self.walk_function(
            name,
            node.byref.is_some(),
            node.params,
            node.ret,
            node.body,
            node.doc_comment,
            line,
            node.range(),
        )
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
    ) -> Result<Vec<tree::Expr>, CompileError> {
        let args = self.parsed.tree[id].args.clone();
        let mut out = Vec::with_capacity(args.len());
        for arg in &args {
            match arg {
                tree::Arg::Expr(expr) => out.push(*expr),
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
        let body = self.parsed.tree[body].stmts.to_vec();
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
            out.push(Parameter {
                name: self.variable_name(param.var),
                ty,
                by_ref: param.byref.is_some(),
                variadic: param.variadic.is_some(),
                default: param.default,
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
            let (name, value, line) = self.const_declarator(*declarator)?;
            let doc_comment = self.doc_comment(self.parsed.tree[*declarator].doc_comment);
            class.constants.push(Constant {
                name,
                visibility: modifiers.visibility.unwrap_or_default(),
                is_final: modifiers.has(Flag::Final),
                value,
                doc_comment,
                line,
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
            class.properties.push(Property {
                name: self.variable_name(declarator.var),
                visibility: modifiers.visibility.unwrap_or_default(),
                set_visibility: modifiers.set_visibility,
                ty: ty.clone(),
                is_static: modifiers.has(Flag::Static),
                default: declarator.init,
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
pub(super) fn unread(line: u32, what: &str) -> CompileError {
    CompileError {
        level: Level::Parse,
        message: format!("{what} this compiler cannot read yet"),
        line,
        raised: Vec::new(),
    }
}

/// php's compile error at the node's own line.
pub(super) fn refused(line: u32, message: impl Into<String>) -> CompileError {
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
