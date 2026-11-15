//! php's line for a node, `zend_ast.lineno`, as the builder records it at
//! `Finish` (design §4.9, ledger 27). php's parser
//! produces the line before its compiler runs, and this file copies the
//! two rules it uses at 8.5.9, where `ZEND_AST_SPEC` is 1:
//!
//! 1. **A node with children takes its first non-NULL child's line, in
//!    php's child order** (`zend_ast_create_1..5`, `zend_ast_create_va`,
//!    `Z/Zend/zend_ast.c:160-316`; a list's, `zend_ast_create_list_1`,
//!    `:350-364`, is its first child's, and `zend_ast_list_add` never
//!    moves it). A child that is a token stands in php's tree as the zval
//!    node the scanner made of it, at the line the token starts on
//!    (`zend_ast_create_zval_with_lineno(zendlval, start_line)`,
//!    `zend_language_scanner.l:3194`); a child that is a node has its own
//!    line. php's tree holds no keyword and no punctuation, so those
//!    tokens are not children here either, and attributes are set onto a
//!    node after it is made (`zend_ast_with_attributes`), so an attribute
//!    group is not its first child.
//! 2. **A node php builds with no child takes the scanner's line at the
//!    reduction**, `CG(zend_lineno)` (`zend_ast_create_0`, the `else` arms
//!    of the constructors above, `zend_ast_create_zval*`). Which token
//!    the scanner had last returned then is a fact of the grammar's
//!    state, measured for every kind: the last token shifted, P,
//!    where the state reduces by default, and the lookahead after it, N,
//!    where the state reads one. The line is that token's last byte's,
//!    so the `;` that `?>` and a newline spell is on the `?>`'s line.
//!
//! The tree's kinds are not php's one for one, so the rule is applied
//! per kind below. Most kinds fall to rule 1 over their children as
//! written; the ones that do not are named in [`of`], each with the
//! grammar rule that decides it. What the compiler does with the line
//! once it holds it, the resets of `CG(zend_lineno)` in `zend_compile.c`,
//! is `emit`'s and not here.
//!
//! **What is not carried.** Two rows of that measurement are read but
//! not applied, both unobservable on an opline: a type keyword (`array`,
//! `callable`, `static`) directly after a class `const` takes the next
//! token's line, and the constant that follows it takes the same line
//! from its name, so the group's line does not move; and `static` as a
//! type is its own line, which rule 1 gives it.

use crate::SyntaxKind;
use crate::lexer::Token;
use crate::line_index::LineIndex;
use crate::source::TextRange;

use super::Element;

/// What the builder knows around one node as it closes it.
pub(super) struct Site<'a> {
    pub(super) tokens: &'a [Token],
    pub(super) lines: &'a LineIndex<'a>,
    /// The last significant token consumed before the node opened.
    pub(super) before: Option<usize>,
    /// The last significant token consumed: the node's last, or `before`
    /// for a node with no token.
    pub(super) last: Option<usize>,
    /// The next token after the node not yet passed over, from which
    /// [`Site::next`] finds the next significant one where a rule reads
    /// it.
    pub(super) raw: usize,
    /// The kind of the innermost open node that opened before this one,
    /// which is where the node stands: a chained parent that opened at
    /// the same token is the same place.
    pub(super) enclosing: Option<SyntaxKind>,
}

impl Site<'_> {
    /// The next significant token after the node, the one php's parser
    /// reads as its lookahead where a state needs one.
    fn next(&self) -> Option<usize> {
        (self.raw..self.tokens.len()).find(|&i| !self.tokens[i].kind.is_trivia())
    }

    /// The line the token's last byte is on, `CG(zend_lineno)` after the
    /// scanner returned it; `1` where there is no token, the scanner's
    /// line before the first (`zend_language_scanner.l:588`).
    fn end_line(&self, token: Option<usize>) -> u32 {
        let Some(token) = token.and_then(|i| self.tokens.get(i)) else {
            return 1;
        };
        self.lines
            .line_number(token.offset + token.len.saturating_sub(1))
    }

    /// The next significant token after the one at `index`: the
    /// lookahead php's parser has read where the state at that token
    /// reads one, rule 2's N. Whitespace and comments are passed over, as
    /// the scanner passes over them on its way to the next token, and
    /// they count under `end_line(N)` for that reason.
    fn after(&self, index: usize) -> Option<usize> {
        (index.saturating_add(1)..self.tokens.len()).find(|&i| !self.tokens[i].kind.is_trivia())
    }

    /// The line the token's first byte is on, `zendlex`'s `start_line`.
    fn start_line(&self, id: super::TokenId) -> Option<u32> {
        let token = self.tokens.get(id.0 as usize)?;
        Some(self.lines.line_number(token.offset))
    }

    /// The lexer's kind of the token, under the parser's remap.
    fn raw(&self, id: super::TokenId) -> Option<SyntaxKind> {
        self.tokens.get(id.0 as usize).map(|t| t.kind)
    }

    /// The lexer's kind of the token at `offset`, where a node begins.
    fn raw_at(&self, offset: u32) -> Option<SyntaxKind> {
        let i = self.tokens.partition_point(|t| t.offset < offset);
        self.tokens.get(i).map(|t| t.kind)
    }

    fn before_kind(&self) -> Option<SyntaxKind> {
        self.before.and_then(|i| self.tokens.get(i)).map(|t| t.kind)
    }

    /// Is the node the first thing after the `(` or a `,` of an argument
    /// list? There a keyword may also be a named argument's label, so
    /// php's parser reads the token after it before it reduces the
    /// keyword (states 443, 444, 452, 446, 438, 441).
    ///
    /// `__LINE__` is `ast->lineno` read back by
    /// `zend_try_ct_eval_magic_const`, so what it prints is the line this
    /// answers for: its own as a statement, the `)`'s as an argument
    /// (`2` then `8`):
    ///
    /// ```php
    /// echo __LINE__ /*
    ///
    /// */ ;
    /// function f($x) { return $x; }
    /// echo f(__LINE__ /*
    ///
    /// */ );
    /// ```
    /// ```expect
    /// 28
    /// ```
    fn in_argument_position(&self) -> bool {
        matches!(
            self.before_kind(),
            Some(SyntaxKind::L_PAREN | SyntaxKind::COMMA)
        ) && self.enclosing == Some(SyntaxKind::ARG_LIST)
    }
}

/// Does php's parser make a zval node of this token, at the line the
/// token starts on?
fn is_value(kind: SyntaxKind) -> bool {
    use SyntaxKind as K;
    matches!(
        kind,
        K::IDENT
            | K::MEMBER_IDENT
            | K::VARIABLE
            | K::NAME_QUALIFIED
            | K::NAME_FULLY_QUALIFIED
            | K::NAME_RELATIVE
            | K::INT
            | K::FLOAT
            | K::SINGLE_QUOTED
            | K::STRING_TEXT
            | K::INLINE_HTML
            | K::ENCAPS_INT
    )
}

/// Rule 1 over `children` as written: the first that stands in php's
/// tree, or `None` where php builds the node with no child.
fn first_child(children: &[Element], site: &Site<'_>) -> Option<u32> {
    children.iter().find_map(|child| child_line(*child, site))
}

/// The line one child gives its parent under rule 1, or `None` for a
/// child php's tree does not hold.
fn child_line(child: Element, site: &Site<'_>) -> Option<u32> {
    match child {
        // Attributes are set onto the node after it is made, and a
        // modifier is a flag on it, never a child.
        Element::Node {
            kind: SyntaxKind::ATTRIBUTE_GROUP | SyntaxKind::SET_VISIBILITY,
            ..
        } => None,
        Element::Node { line, .. } => Some(line),
        Element::Token { kind, id } if is_value(kind) => site.start_line(id),
        Element::Token { .. } => None,
    }
}

/// The first token child, and its lexer kind.
fn first_token(children: &[Element], site: &Site<'_>) -> Option<(usize, SyntaxKind)> {
    children.iter().find_map(|child| match child {
        Element::Token { id, .. } => site.raw(*id).map(|kind| (id.0 as usize, kind)),
        Element::Node { .. } => None,
    })
}

/// The first node child of `kind`.
fn node_of(children: &[Element], kind: SyntaxKind) -> Option<(usize, TextRange, u32)> {
    children
        .iter()
        .enumerate()
        .find_map(|(i, child)| match *child {
            Element::Node {
                kind: k,
                range,
                line,
                ..
            } if k == kind => Some((i, range, line)),
            _ => None,
        })
}

/// php's line for a node of `kind` over `children`, closed at `site`.
///
/// What an opline shows of it. An assignment written over several lines
/// carries its target's line, since `zend_compile_assign` sets
/// `CG(zend_lineno)` back to `var_ast`'s before the `ASSIGN` it emits,
/// and the warning that opline raises names it:
///
/// ```php
/// $a
/// =
/// $b;
/// ```
/// ```expect
/// Warning: Undefined variable $b in %s on line 2
/// ```
///
/// An empty statement list is the scanner's line where its rule reduced,
/// the `:` of a `default` here, which `zend_compile_switch` reads for the
/// clause it refuses:
///
/// ```php
/// switch (1) { default: default:
/// /*
///
/// */ }
/// ```
/// ```expect exit=255
/// Fatal error: Switch statements may only contain one default clause in %s on line 2
/// ```
///
/// The same line through the deprecation `zend_compile_switch` raises
/// for the `;` spelling, the `;` being where the arm's empty statement
/// list reduced and the comments on either side of it uncounted:
///
/// ```php
/// switch (1) { default /*
///
/// */ ; /*
///
/// */ }
/// ```
/// ```expect
/// Deprecated: Case statements followed by a semicolon (;) are deprecated, use a colon (:) instead in %s on line 4
/// ```
///
/// An empty array is its `]`'s line, the lookahead its rule reduced on,
/// and the `ECHO` that converts it is emitted there:
///
/// ```php
/// echo [ /*
///
/// */ ];
/// ```
/// ```expect
/// Warning: Array to string conversion in %s on line 4
/// Array
/// ```
///
/// The same line through `zend_compile_args`, which raises after the
/// argument's `zend_compile_expr`:
///
/// ```php
/// function f(&$x) {}
/// f([ /*
///
/// */ ]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: f(): Argument #1 ($x) could not be passed by reference in %s:5
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 5
/// ```
///
/// A bare `return` is its `;`'s line and not its keyword's, the comment
/// after the `;` uncounted:
///
/// ```php
/// function f(): int { return
/// ; /*
///
/// */ }
/// ```
/// ```expect exit=255
/// Fatal error: A function with return type must return a value in %s on line 3
/// ```
///
/// The `;` that `?>` and a newline spell is on the `?>`'s line, since the
/// newline is counted at the next `zendlex`:
///
/// ```php
/// function f(): int { return ?>
/// <?php }
/// ```
/// ```expect exit=255
/// Fatal error: A function with return type must return a value in %s on line 2
/// ```
///
/// A bare `break` is its `;`'s line the same way:
///
/// ```php
/// break /*
///
/// */ ; /*
///
/// */
/// ```
/// ```expect exit=255
/// Fatal error: 'break' not in the 'loop' or 'switch' context in %s on line 4
/// ```
///
/// `exit`'s name zval is made when `T_EXIT ctor_arguments` reduces, after
/// the `)`, so the call is the `)`'s line:
///
/// ```php
/// exit(
/// []
/// );
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: exit(): Argument #1 ($status) must be of type string|int, array given in %s:4
/// Stack trace:
/// #0 %s(4): exit(Array)
/// #1 {main}
///   thrown in %s on line 4
/// ```
///
/// What is left of the measurement's programs is held to what the oracle
/// printed in `tests/line.rs`, each for a reason written beside it there:
/// the two programs that read a line through `declare(ticks=1)`, the
/// `yield` and the `static` the compiler refuses, and the `case ;`
/// deprecation it does not raise. The line a `for` loop's jump back is
/// left at is a witness beside `php_compile`'s `for`, where the mechanism
/// is.
pub(super) fn of(kind: SyntaxKind, children: &[Element], site: &Site<'_>) -> u32 {
    use SyntaxKind as K;
    let by_rule_1 = || first_child(children, site);
    match kind {
        // `top_statement_list: %empty` reduces in state 0, before any
        // token, and `zend_ast_list_add` never moves a list's line.
        K::SOURCE_FILE => 1,
        // `inner_statement_list: %empty`, `class_statement_list: %empty`
        // and `property_hook_list: %empty` reduce directly after the `{`
        // by default, and the list keeps that line however many
        // statements are added.
        K::BLOCK | K::CLASS_BODY | K::PROPERTY_HOOK_LIST => {
            site.end_line(first_token(children, site).map(|(i, _)| i).or(site.last))
        }
        // The same list after the `:` of an alternative-syntax body or a
        // `case`, or the `;` a `default ;` is written with.
        K::STMT_LIST => site.end_line(site.before),
        K::FUNCTION_DECL
        | K::METHOD_DECL
        | K::CLOSURE_EXPR
        | K::ARROW_FN_EXPR
        | K::CLASS_DECL
        | K::INTERFACE_DECL
        | K::TRAIT_DECL
        | K::ENUM_DECL
        | K::ANON_CLASS_DECL => declaration(kind, children, site),
        // `for_exprs: %empty` reduces on the lookahead, which is the `;`
        // or `)` after it.
        K::FOR_INIT | K::FOR_COND | K::FOR_STEP => {
            by_rule_1().unwrap_or_else(|| site.end_line(site.next()))
        }
        // `ZEND_AST_ARRAY_ELEM` is `(value, key)` (`:1646-1647`), and an
        // elided element is `NULL`, whose list takes the lookahead's line
        // (`possible_array_pair: %empty`, as the empty array's does);
        // `ZEND_AST_YIELD` is `(value, key)` too (`:1380`), and a bare
        // `yield` reduces on the lookahead.
        K::ARRAY_ELEMENT => value_before_key(children, K::ARRAY_KEY, site),
        K::YIELD_EXPR => value_before_key(children, K::YIELD_KEY, site),
        // `ZEND_AST_CLASS_CONST_GROUP` is `(list, attrs, type)` (`:992`):
        // the declarators' line, before the type written ahead of them.
        K::CLASS_CONST_DECL => node_of(children, K::CONST_DECLARATOR)
            .map(|(_, _, line)| line)
            .or_else(by_rule_1)
            .unwrap_or_else(|| site.end_line(site.last)),
        // A class constant named by a keyword is `semi_reserved '=' expr
        // backup_doc_comment`, whose zval is made when the whole rule
        // reduces, after the expression's lookahead (`:1188-1192`).
        K::CONST_DECLARATOR
            if site.enclosing == Some(K::CLASS_CONST_DECL)
                && node_of(children, K::MEMBER_NAME)
                    .and_then(|(_, range, _)| site.raw_at(range.start))
                    .is_some_and(SyntaxKind::is_keyword) =>
        {
            site.end_line(site.next())
        }
        // A named argument's label that is a keyword is reduced after the
        // `:` in argument position (`identifier: semi_reserved`).
        K::NAMED_ARG if first_token(children, site).is_some_and(|(_, kind)| kind.is_keyword()) => {
            site.end_line(first_token(children, site).and_then(|(at, _)| site.after(at)))
        }
        // `zend_ast_create_ex(ZEND_AST_MAGIC_CONST, …)` has no child: its
        // own line, or the lookahead's in argument position.
        K::MAGIC_CONST_EXPR if site.in_argument_position() => site.end_line(site.next()),
        // `exit`'s and `clone`'s name zval is made when `T_EXIT
        // ctor_arguments` or `T_CLONE clone_argument_list` reduces, after
        // the `)`, and for the bare and the operand forms after the
        // lookahead (`:1370`, `:1259-1268`).
        K::EXIT_EXPR | K::CLONE_EXPR => {
            if node_of(children, K::ARG_LIST).is_some() {
                site.end_line(site.last)
            } else {
                site.end_line(site.next())
            }
        }
        // `'(' T_ELLIPSIS ')'` is `zend_ast_create_fcc()` after the `)`.
        K::VARIADIC_PLACEHOLDER => site.end_line(site.next()),
        K::CALL_EXPR => call(children, site),
        K::SCOPED_ACCESS_EXPR | K::TRAIT_METHOD_REF => class_name(kind, children, site),
        // Rule 1, and the last token the scanner returned where php's
        // node has no child: `argument_list: '(' ')'`, the empty
        // `parameter_list`, array and heredoc, `return;`, `break;`, an
        // empty statement.
        _ => by_rule_1().unwrap_or_else(|| site.end_line(site.last)),
    }
}

/// A declaration's line is `CG(zend_lineno)` after its keyword
/// (`function: T_FUNCTION { $$ = CG(zend_lineno); }` and `fn:`, `T_CLASS
/// { $<num>$ = CG(zend_lineno); }` and the interface, trait, enum and
/// anonymous-class rules alike, handed to `zend_ast_create_decl` as
/// `start_lineno`). In argument position a closure's keyword is reduced
/// after the lookahead, which is what Reflection reads back as the
/// closure's start line, `2`, `9` and `11` here:
///
/// ```php
/// $c = function /*
///
/// */ () {};
/// echo (new ReflectionFunction($c))->getStartLine(), "\n";
/// function f($c) { return $c; }
/// $d = f(function /*
///
/// */ () {});
/// echo (new ReflectionFunction($d))->getStartLine(), "\n";
/// $e = f(static function /*
///
/// */ () {});
/// echo (new ReflectionFunction($e))->getStartLine(), "\n";
/// ```
/// ```expect
/// 2
/// 9
/// 11
/// ```
///
/// The lookahead is the token after the keyword in the source, which is
/// the `(` of the parameter list and not the token after the closure:
/// `zend_ast_create_decl` is handed the mid-rule value, so a closure
/// whose `(` stands on the keyword's line is that line however far the
/// argument list runs past it.
///
/// ```php
/// function f($c) { return $c; }
/// $d = f(function () {
/// }
/// );
/// echo (new ReflectionFunction($d))->getStartLine(), "\n";
/// ```
/// ```expect
/// 3
/// ```
fn declaration(kind: SyntaxKind, children: &[Element], site: &Site<'_>) -> u32 {
    use SyntaxKind as K;
    let keyword = children
        .iter()
        .enumerate()
        .find_map(|(i, child)| match child {
            Element::Token { id, .. }
                if matches!(
                    site.raw(*id),
                    Some(
                        K::FUNCTION_KW
                            | K::FN_KW
                            | K::CLASS_KW
                            | K::INTERFACE_KW
                            | K::TRAIT_KW
                            | K::ENUM_KW
                    )
                ) =>
            {
                Some((i, id.0 as usize))
            }
            _ => None,
        });
    match keyword {
        Some((0, token))
            if matches!(kind, K::CLOSURE_EXPR | K::ARROW_FN_EXPR)
                && site.in_argument_position() =>
        {
            site.end_line(site.after(token))
        }
        Some((_, token)) => site.end_line(Some(token)),
        None => first_child(children, site).unwrap_or_else(|| site.end_line(site.last)),
    }
}

/// Rule 1 with php's `(value, key)` order: the value's line, the key's
/// where the node has no value, and the lookahead's where it has
/// neither.
fn value_before_key(children: &[Element], key_kind: SyntaxKind, site: &Site<'_>) -> u32 {
    let key = node_of(children, key_kind);
    let value = children
        .iter()
        .enumerate()
        .filter(|(i, _)| key.is_none_or(|(at, _, _)| *i > at))
        .find_map(|(_, child)| child_line(*child, site));
    value
        .or(key.map(|(_, _, line)| line))
        .unwrap_or_else(|| site.end_line(site.next()))
}

/// `ZEND_AST_CALL` over a name takes the name's line; over a
/// `callable_expr` it takes `CG(zend_lineno)` at the `(` (`:1457`); a
/// method call or a `::` call takes its receiver's, as rule 1 gives it;
/// `readonly(…)`'s name zval is made after the `)` (`:1449`).
///
/// The call through a variable, whose line the `ArgumentCountError`
/// names:
///
/// ```php
/// function g($x) {}
/// $f = "g";
/// $f /*
///
/// */ (
/// );
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: Too few arguments to function g(), 0 passed in %s on line 6 and exactly 1 expected in %s:2
/// Stack trace:
/// #0 %s(6): g()
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// `readonly` as a function name, whose zval is made after the `)`:
///
/// ```php
/// function readonly($x) {}
/// readonly(
///
/// );
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: Too few arguments to function readonly(), 0 passed in %s on line 5 and exactly 1 expected in %s:2
/// Stack trace:
/// #0 %s(5): readonly()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn call(children: &[Element], site: &Site<'_>) -> u32 {
    use SyntaxKind as K;
    match children.first() {
        Some(Element::Token { id, .. }) if site.raw(*id) == Some(K::READONLY_KW) => {
            site.end_line(site.last)
        }
        Some(Element::Token { kind, id }) if is_value(*kind) => site
            .start_line(*id)
            .unwrap_or_else(|| site.end_line(site.last)),
        Some(Element::Node {
            kind: K::MEMBER_ACCESS_EXPR | K::SCOPED_ACCESS_EXPR,
            line,
            ..
        }) => *line,
        _ => node_of(children, K::ARG_LIST)
            .map(|(_, range, _)| site.lines.line_number(range.start))
            .or_else(|| first_child(children, site))
            .unwrap_or_else(|| site.end_line(site.last)),
    }
}

/// `class_name: T_STATIC` makes its zval at the reduction: before the
/// lookahead where only a class reference can stand (`new static`),
/// after it where a statement or an expression may begin, so the `::`'s
/// line, and in a trait adaptation, where `static` may also be a
/// method's name. Any other class name is rule 1.
fn class_name(kind: SyntaxKind, children: &[Element], site: &Site<'_>) -> u32 {
    use SyntaxKind as K;
    match children.first() {
        Some(Element::Token { id, .. })
            if site.raw(*id) == Some(K::STATIC_KW)
                && !(kind == K::SCOPED_ACCESS_EXPR && static_stands_alone(site)) =>
        {
            site.end_line(site.after(id.0 as usize))
        }
        _ => first_child(children, site).unwrap_or_else(|| site.end_line(site.last)),
    }
}

/// Is `static` here reduced before the lookahead (state 151)? After
/// `new`, `instanceof`, `++` and `--`, and where only a variable can
/// follow: a `foreach` target, an `unset` operand, and a reference taken
/// by `&` outside a binary expression.
fn static_stands_alone(site: &Site<'_>) -> bool {
    use SyntaxKind as K;
    match site.before_kind() {
        Some(K::NEW_KW | K::INSTANCEOF_KW | K::PLUS_PLUS | K::MINUS_MINUS) => true,
        Some(K::AS_KW | K::FAT_ARROW) => site.enclosing == Some(K::FOREACH_STMT),
        Some(K::L_PAREN | K::COMMA) => site.enclosing == Some(K::UNSET_STMT),
        Some(K::AMP) => site.enclosing != Some(K::BINARY_EXPR),
        _ => false,
    }
}
