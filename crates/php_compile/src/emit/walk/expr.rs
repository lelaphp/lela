//! The walk over a closure written inside an expression of
//! `php_syntax`'s tree (design §4.9): what [`Emit`] reads
//! `function (…) use (…) {…}` and `fn (…) => e` as, in the [`Closure`]
//! shape that stands beside the other declarations', since a closure
//! is a declaration and reaches the emitter as they do.
//!
//! The body is the tree's own statement nodes, which the emitter walks
//! where it compiles them, and an arrow function's is the expression
//! after `=>`, the one `return` php makes of it
//! (`zend_compile_func_decl`). A doc comment is the token index the
//! builder recorded on the node; the `static` before the keyword is the
//! first significant token of its range, php's `ZEND_ACC_STATIC`. The
//! attributes written before either are the closure's own, compiled at
//! its declaration as a function's are (`zend_compile_func_decl`).

use super::{Closure, Emit, Function, Use, unread};
use crate::CompileError;
use php_syntax::tree::{ArrowFnExpr, ClosureExpr, Expr, Node, NodeId};
use php_syntax::{SyntaxKind, TextRange};

impl Emit<'_> {
    /// One closure or arrow function as the emitter reads it: php's
    /// `ZEND_AST_CLOSURE` and `ZEND_AST_ARROW_FUNC`.
    pub(in crate::emit) fn walk_closure(
        &mut self,
        expr: Expr,
        line: u32,
    ) -> Result<Closure, CompileError> {
        match expr {
            Expr::ClosureExpr(id) => self.closure_expr(id, line),
            Expr::ArrowFnExpr(id) => self.arrow_fn(id, line),
            _ => Err(self.internal("an expression that is no closure")),
        }
    }

    /// `function (…) use (…) { … }`, php's `ZEND_AST_CLOSURE`: the
    /// signature and body as a function's, the `use` list, and whether
    /// `static` stands before the keyword.
    fn closure_expr(
        &mut self,
        id: NodeId<ClosureExpr>,
        line: u32,
    ) -> Result<Closure, CompileError> {
        let node = self.parsed.tree[id];
        let attributes = self.walk_attributes(self.list(node.attrs))?;
        let is_static = self.first_kind(node.range) == Some(SyntaxKind::STATIC_KW);
        let mut lexical = Vec::new();
        if let Some(clause) = node.uses {
            for id in self.list(self.parsed.tree[clause].closure_use_vars) {
                let used = &self.parsed.tree[*id];
                lexical.push(Use {
                    name: self.variable_name(used.var).to_vec(),
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
            false,
            node.doc_comment,
            line,
            node.range,
        )?;
        Ok(Closure {
            function: Function {
                attributes,
                ..function
            },
            uses: lexical,
            is_static,
            arrow: None,
        })
    }

    /// `fn (…) => expr`, php's `ZEND_AST_ARROW_FUNC`: a closure whose
    /// body is the one `return` php makes of the expression
    /// (`zend_compile_func_decl`), and whose end line is the token after
    /// the body's.
    fn arrow_fn(&mut self, id: NodeId<ArrowFnExpr>, line: u32) -> Result<Closure, CompileError> {
        let node = self.parsed.tree[id];
        let attributes = self.walk_attributes(self.list(node.attrs))?;
        let is_static = self.first_kind(node.range) == Some(SyntaxKind::STATIC_KW);
        let parameters = self.walk_parameters(node.params)?;
        let return_type = match node.ret {
            Some(ret) => match self.parsed.tree[ret].ty {
                Some(ty) => Some(ty),
                None => return Err(unread(line, "a return type")),
            },
            None => None,
        };
        let Some(body) = node.body else {
            return Err(unread(line, "an arrow function with no body"));
        };
        Ok(Closure {
            function: Function {
                name: Vec::new(),
                parameters,
                return_type,
                by_ref: node.byref.is_some(),
                body: Vec::new(),
                attributes,
                doc_comment: self.doc_comment(node.doc_comment),
                line,
                end_line: self.line_after(node.range),
                generator: self.is_generator_at(node.range.start),
                void_return: false,
            },
            uses: Vec::new(),
            is_static,
            arrow: Some(body),
        })
    }

    /// The kind of the first significant token inside `range`.
    pub(in crate::emit) fn first_kind(&self, range: TextRange) -> Option<SyntaxKind> {
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
    /// `end_lineno` stands.
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
}
