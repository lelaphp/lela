//! The walk over a property's hook list (design §4.9): what
//! [`Emit`] reads `{ get => …; set(T $v) { … } }` as, php's
//! `property_hook_list` of `ZEND_AST_PROPERTY_HOOK` declarations, in the
//! [`Hook`] shape the emitter compiles each one from
//! (`zend_compile_property_hooks`).
//!
//! What is read here is what the grammar wrote: the name as written,
//! which the emitter holds to `get` and `set`; the modifiers under
//! `ZEND_MODIFIER_TARGET_PROPERTY_HOOK`, which take `final` and nothing
//! else; the parameter list where one is written; and the body, a
//! block, the `=>` short form or none. Whether a body names the backing
//! value, `zend_property_hook_uses_property`, is read off the tree here
//! too, since it is a fact of the tree's shape.

use super::{Attr, Emit, Flag, Parameter, Target, unread};
use crate::CompileError;
use php_syntax::SyntaxKind;
use php_syntax::tree::{self, Element, Node, NodeId, Stmt};

/// One hook as written, php's `ZEND_AST_PROPERTY_HOOK`: the name as the
/// source spells it, `&` before it (`ZEND_ACC_RETURN_REFERENCE`),
/// `final`, the parameter list where one is written, the body, the
/// attributes before it, the doc comment after its name, and the lines
/// of its name and of its last token, `start_lineno` and `end_lineno`.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // the ZEND_ACC_* bits a hook is written with, and two facts of its body
pub(in crate::emit) struct Hook {
    pub(in crate::emit) name: Vec<u8>,
    pub(in crate::emit) by_ref: bool,
    pub(in crate::emit) is_final: bool,
    pub(in crate::emit) parameters: Option<Vec<Parameter>>,
    pub(in crate::emit) body: HookBody,
    pub(in crate::emit) attributes: Vec<Attr>,
    pub(in crate::emit) doc_comment: Option<Vec<u8>>,
    pub(in crate::emit) line: u32,
    pub(in crate::emit) end_line: u32,
    /// `ZEND_ACC_GENERATOR`: a `yield` stands in the body itself.
    pub(in crate::emit) generator: bool,
    /// `zend_property_hook_uses_property`: the body reads or writes
    /// `$this->name` of the property it hooks, outside any function or
    /// class written inside it, or it is a `set` hook's short form,
    /// which assigns it.
    pub(in crate::emit) uses_property: bool,
}

/// A hook's body as written, php's `property_hook_body`.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::emit) enum HookBody {
    /// `;`, an abstract hook's, `NULL`.
    None,
    /// `{ … }`, the statements.
    Block(Vec<Stmt>),
    /// `=> expr;`, php's `ZEND_AST_PROPERTY_HOOK_SHORT_BODY`, which a
    /// `get` hook returns and a `set` hook assigns to the backing value.
    Short(tree::Expr),
}

impl HookBody {
    /// Whether a body stands, php's `stmt_ast != NULL`.
    pub(in crate::emit) fn is_some(&self) -> bool {
        !matches!(self, HookBody::None)
    }
}

impl Emit<'_> {
    /// A hook list as written, for the property named `property`.
    ///
    /// Only `final` may stand before a hook, as
    /// `zend_modifier_token_to_flag` has it under
    /// `ZEND_MODIFIER_TARGET_PROPERTY_HOOK`:
    ///
    /// ```php
    /// class A { public $a { public get => 1; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use the public modifier on a property hook in %s on line 2
    /// ```
    ///
    /// ```php
    /// class A { public $a { final final get => 1; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Multiple final modifiers are not allowed in %s on line 2
    /// ```
    pub(in crate::emit) fn walk_hooks(
        &mut self,
        list: NodeId<tree::PropertyHookList>,
        property: &[u8],
    ) -> Result<Vec<Hook>, CompileError> {
        let hooks = self.list(self.parsed.tree[list].property_hooks);
        let mut out = Vec::with_capacity(hooks.len());
        for id in hooks {
            let node = self.parsed.tree[*id];
            let line = self.token_line(node.name).unwrap_or_else(|| node.line());
            let modifiers = self.walk_modifiers(self.list(node.modifiers), line, Target::Hook)?;
            let attributes = self.walk_attributes(self.list(node.attrs))?;
            let name = self.text(node.name).to_vec();
            let parameters = match node.params {
                Some(params) => Some(self.walk_parameters(params)?),
                None => None,
            };
            let body = match (node.body, node.arrow) {
                (Some(block), _) => HookBody::Block(
                    self.parsed
                        .tree
                        .list(self.parsed.tree[block].stmts)
                        .to_vec(),
                ),
                (None, Some(arrow)) => match self.parsed.tree[arrow].value {
                    Some(value) => HookBody::Short(value),
                    None => return Err(unread(line, "a property hook's short body")),
                },
                (None, None) => HookBody::None,
            };
            let is_set = name.eq_ignore_ascii_case(b"set");
            let uses_property = match &body {
                HookBody::None => false,
                HookBody::Short(_) if is_set => true,
                HookBody::Short(_) | HookBody::Block(_) => self.names_backing_value(*id, property),
            };
            out.push(Hook {
                name,
                by_ref: node.byref.is_some(),
                is_final: modifiers.has(Flag::Final),
                parameters,
                body,
                attributes,
                doc_comment: self.doc_comment(node.doc_comment),
                line,
                end_line: self.end_line(node.range()),
                generator: self.is_generator_at(node.range().start),
                uses_property,
            });
        }
        Ok(out)
    }

    /// `zend_property_hook_find_property_usage` over the hook's body:
    /// whether `$this->property` or `$this?->property` stands in it,
    /// the name written as an identifier, a string interpolation's
    /// `"$this->property"` included, without looking inside a closure,
    /// an arrow function or a class written there, which
    /// `zend_ast_is_special` keeps the walk out of.
    fn names_backing_value(&self, hook: NodeId<tree::PropertyHook>, property: &[u8]) -> bool {
        let elements = self.parsed.tree.elements();
        let Some(start) = elements.iter().position(|element| {
            matches!(element, Element::Node { kind: SyntaxKind::PROPERTY_HOOK, index, .. } if *index == hook.index())
        }) else {
            return false;
        };
        let Some(Element::Node { descendants, .. }) = elements.get(start) else {
            return false;
        };
        let end = start.saturating_add(*descendants as usize);
        let mut at = start.saturating_add(1);
        while at <= end {
            let Some(element) = elements.get(at) else {
                break;
            };
            let Element::Node {
                kind, descendants, ..
            } = *element
            else {
                at += 1;
                continue;
            };
            match kind {
                SyntaxKind::CLOSURE_EXPR
                | SyntaxKind::ARROW_FN_EXPR
                | SyntaxKind::ANON_CLASS_DECL
                | SyntaxKind::FUNCTION_DECL
                | SyntaxKind::CLASS_DECL
                | SyntaxKind::INTERFACE_DECL
                | SyntaxKind::TRAIT_DECL
                | SyntaxKind::ENUM_DECL => {
                    at = at.saturating_add(descendants as usize).saturating_add(1);
                    continue;
                }
                SyntaxKind::MEMBER_ACCESS_EXPR => {
                    if let Some(id) = element.id::<tree::MemberAccessExpr>()
                        && self.is_this_property(id, property)
                    {
                        return true;
                    }
                }
                SyntaxKind::SIMPLE_INTERPOLATION => {
                    if let Some(id) = element.id::<tree::SimpleInterpolation>()
                        && self.is_this_interpolated(id, property)
                    {
                        return true;
                    }
                }
                _ => {}
            }
            at += 1;
        }
        false
    }

    /// `$this->property` as `zend_property_hook_find_property_usage`
    /// matches it: a plain `$this` before the arrow and the name as a
    /// constant after it.
    fn is_this_property(&self, id: NodeId<tree::MemberAccessExpr>, property: &[u8]) -> bool {
        let node = &self.parsed.tree[id];
        let tree::Expr::SimpleVarExpr(var) = node.obj else {
            return false;
        };
        if self.parsed.tree[var]
            .var
            .is_none_or(|var| self.text(var) != b"$this")
        {
            return false;
        }
        match node.member {
            Some(tree::MemberSelector::MemberName(name)) => self.parsed.tree[name]
                .name
                .is_some_and(|name| self.text(name) == property),
            _ => false,
        }
    }

    /// `"$this->property"`, which php's scanner hands the parser as the
    /// same `ZEND_AST_PROP`.
    fn is_this_interpolated(&self, id: NodeId<tree::SimpleInterpolation>, property: &[u8]) -> bool {
        let node = &self.parsed.tree[id];
        if self.text(node.var) != b"$this" {
            return false;
        }
        match node.suffix {
            Some(tree::SimpleInterpolationSuffix::SimpleProp(prop)) => {
                self.text(self.parsed.tree[prop].name) == property
            }
            _ => false,
        }
    }
}
