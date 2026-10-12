//! The grammar (design §4.9, ADR-0003): the statement level, the recovery
//! ladder and the source-file loop, written against the parser's surface
//! and nothing else. Expressions live in [`expr`], declarations in
//! [`decl`], types in [`types`], interpolation in [`strings`] and the
//! contextual-keyword decisions in [`ckw`]. Read at Mahout `1b3104a8`
//! from `crates/mahout_parser/src/grammar/`, the six files of record §3.6,
//! with the hand-placed error sites where they were.
//!
//! **Recovery**, in order: (1) a targeted single-token repair in a
//! high-confidence position (a missing `;` or `,`); (2) synchronisation to
//! an anchor set: `;`, `}`, `?>`, an inline-HTML boundary, a declaration-
//! or statement-introducing keyword; (3) bounded absorption into an
//! `ERROR` node under the forward-progress invariant.
//!
//! **What a diagnostic says.** The engine has one voice (design §6.1):
//! every site words its diagnostic as php words it, `syntax error,
//! unexpected …` with the `, expecting …` clause bison prints at that
//! position where the site pins one, at php's line. A missing `;` or `,`
//! is the unexpected token that stood where it was due, named as such;
//! what the repair decides is only how the tree goes on. A token that
//! cannot start a statement takes the enclosing list's clause
//! ([`Parser::stmt_clause`]), which is how `const` inside a function,
//! `(void)` where php forbids it, a stray byte and an unclosed
//! single-quoted string all read as php reads them: as the token php did
//! not want there.

pub(crate) mod ckw;
mod decl;
mod expr;
mod strings;
mod types;

use crate::SyntaxKind as K;
use crate::parser::{CompletedMarker, Marker, Parser};
use crate::source::Span;

/// The tokens that introduce a statement: the sync anchors, and the
/// "next token starts a statement" half of the missing-`;` repair.
const STMT_START: &[K] = &[
    K::SEMICOLON,
    K::L_CURLY,
    K::IF_KW,
    K::WHILE_KW,
    K::DO_KW,
    K::FOR_KW,
    K::FOREACH_KW,
    K::SWITCH_KW,
    K::BREAK_KW,
    K::CONTINUE_KW,
    K::RETURN_KW,
    K::GOTO_KW,
    K::TRY_KW,
    K::ECHO_KW,
    K::GLOBAL_KW,
    K::UNSET_KW,
    K::CONST_KW,
    K::USE_KW,
    K::NAMESPACE_KW,
    K::DECLARE_KW,
    K::FUNCTION_KW,
    K::CLASS_KW,
    K::INTERFACE_KW,
    K::TRAIT_KW,
    K::ABSTRACT_KW,
    K::FINAL_KW,
    K::THROW_KW,
];

/// The hard recovery anchors at statement level. An inline-HTML boundary
/// is one by design: a template-heavy file recovers per segment.
fn at_stmt_recovery(p: &mut Parser<'_>) -> bool {
    p.at_eof()
        || p.at(K::SEMICOLON)
        || p.at(K::R_CURLY)
        || p.at(K::CLOSE_TAG)
        || p.at(K::INLINE_HTML)
        || p.at(K::OPEN_TAG)
        || STMT_START.contains(&p.current())
}

/// A whole file into a `SOURCE_FILE` node. The top level is where php's
/// clause for a token that starts no statement is `end of file` (probed
/// 8.5.9: `<?php =`).
pub(crate) fn source_file(p: &mut Parser<'_>) {
    let m = p.start();
    p.stmt_clause = Some("end of file");
    while !p.at_eof() {
        if p.is_stuck() {
            // The forward-progress backstop: never loop without consuming.
            let e = p.start();
            p.bump_any();
            e.complete(p, K::ERROR);
            continue;
        }
        statement(p, true);
    }
    m.complete(p, K::SOURCE_FILE);
}

/// One statement, or a declaration: declarations are statements in php.
pub(crate) fn statement(p: &mut Parser<'_>, top_level: bool) {
    if !p.enter_depth() {
        absorb_balanced(p);
        return;
    }
    statement_inner(p, top_level);
    p.exit_depth();
}

/// Consumes the construct at the cursor flat into one `ERROR` node:
/// delimiter balance is tracked iteratively, stopping at the enclosing
/// closer, a same-level `;` (consumed) or the end of the input. The
/// nesting-cap recovery: no recursion, forward progress, lossless.
pub(crate) fn absorb_balanced(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    let mut depth: u64 = 0;
    let mut consumed = false;
    while !p.at_eof() {
        match p.current() {
            K::L_PAREN | K::L_BRACK | K::L_CURLY | K::ATTRIBUTE_OPEN => depth += 1,
            K::R_PAREN | K::R_BRACK | K::R_CURLY => {
                if depth == 0 {
                    break; // the enclosing construct's closer
                }
                depth -= 1;
            }
            K::SEMICOLON if depth == 0 => {
                p.bump_any();
                consumed = true;
                break;
            }
            _ => {}
        }
        p.bump_any();
        consumed = true;
    }
    // Forward progress even when parked on a stray closer (the outer
    // recovery ladder owns it; the progress invariant owns this).
    if !consumed && !p.at_eof() {
        p.bump_any();
    }
    m.complete(p, K::ERROR)
}

/// One statement, `top_level` saying whether php's `top_statement`s
/// (`const`, `use`, `namespace`) may stand here.
///
/// ```php
/// function f() { const D = 2; }
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected token "const" in %s on line 2
/// ```
fn statement_inner(p: &mut Parser<'_>, top_level: bool) {
    // `const`, `use` and `namespace` are php's `top_statement`s: inside
    // a function, a block or a control-flow body they are the unexpected
    // token, with the enclosing list's clause (design §6.1's nested
    // `const`; probed 8.5.9: `function f(){ const D = 2; }`, `if (1):
    // const X = 1; endif;`). Parsed anyway, so the tree keeps the shape.
    if !top_level && matches!(p.current(), K::CONST_KW | K::USE_KW | K::NAMESPACE_KW) {
        let d = p.unexpected_at_stmt();
        p.error(d);
    }
    match p.current() {
        K::SEMICOLON => {
            let m = p.start();
            p.bump(K::SEMICOLON);
            m.complete(p, K::EMPTY_STMT);
        }
        K::L_CURLY => {
            block(p);
        }
        K::INLINE_HTML => {
            let m = p.start();
            p.bump(K::INLINE_HTML);
            m.complete(p, K::INLINE_HTML_STMT);
        }
        // An open or close tag is structural glue at list level (schema A6).
        K::OPEN_TAG | K::CLOSE_TAG => p.bump_any(),
        K::OPEN_TAG_ECHO | K::ECHO_KW => echo_stmt(p),
        K::IF_KW => if_stmt(p),
        K::WHILE_KW => while_stmt(p),
        K::DO_KW => do_while_stmt(p),
        K::FOR_KW => for_stmt(p),
        K::FOREACH_KW => foreach_stmt(p),
        K::SWITCH_KW => switch_stmt(p),
        K::BREAK_KW | K::CONTINUE_KW => break_continue_stmt(p),
        K::RETURN_KW => return_stmt(p),
        K::GOTO_KW => goto_stmt(p),
        K::TRY_KW => try_stmt(p),
        K::GLOBAL_KW => global_stmt(p),
        K::UNSET_KW => unset_stmt(p),
        K::CONST_KW => {
            let m = p.start();
            const_stmt(p, m);
        }
        K::USE_KW => use_stmt(p),
        K::NAMESPACE_KW => namespace_def(p),
        K::DECLARE_KW => declare_stmt(p),
        K::HALT_COMPILER_KW => halt_compiler_stmt(p, top_level),
        K::STATIC_KW => match ckw::static_in_stmt(p.nth(1)) {
            ckw::StaticReading::VarStmt => static_var_stmt(p),
            ckw::StaticReading::Closure | ckw::StaticReading::Expr => expr_stmt(p),
        },
        K::FUNCTION_KW if function_is_decl(p) => {
            let m = p.start();
            decl::function_decl(p, m);
        }
        K::CLASS_KW => {
            let m = p.start();
            decl::class_decl(p, m);
        }
        K::INTERFACE_KW => {
            let m = p.start();
            decl::interface_decl(p, m);
        }
        K::TRAIT_KW => {
            let m = p.start();
            decl::trait_decl(p, m);
        }
        K::ENUM_KW if ckw::enum_is_declaration(p.nth(1)) => {
            let m = p.start();
            decl::enum_decl(p, m);
        }
        K::ABSTRACT_KW | K::FINAL_KW => {
            let m = p.start();
            decl::modifiers_then_class(p, m);
        }
        K::READONLY_KW if !ckw::readonly_is_call(p.nth(1)) => {
            let m = p.start();
            decl::modifiers_then_class(p, m);
        }
        // A stray byte, which php's scanner hands its parser as
        // `T_BAD_CHARACTER`: `unexpected character 0x0E` with the list's
        // clause.
        K::ERROR => {
            let d = p.unexpected_at_stmt();
            p.error(d);
            let e = p.start();
            p.bump(K::ERROR);
            e.complete(p, K::ERROR);
        }
        K::ATTRIBUTE_OPEN => attributed_statement(p, top_level),
        // A label is `T_STRING ':'`; `callable:` is the keyword out of
        // place, as any bare `callable` at a statement's start.
        K::IDENT if p.nth(1) == K::COLON && !ckw::is_callable_text(K::IDENT, p.current_text()) => {
            let m = p.start();
            p.bump(K::IDENT);
            p.bump(K::COLON);
            m.complete(p, K::LABEL_STMT);
        }
        _ => expr_stmt(p),
    }
}

/// `#[…]` at statement position: a declaration's attributes, or an
/// expression statement whose closure carries them.
fn attributed_statement(p: &mut Parser<'_>, top_level: bool) {
    match peek_past_attributes(p) {
        K::FUNCTION_KW
        | K::CLASS_KW
        | K::INTERFACE_KW
        | K::TRAIT_KW
        | K::ENUM_KW
        | K::ABSTRACT_KW
        | K::FINAL_KW
        | K::READONLY_KW
        | K::CONST_KW => {
            let m = p.start();
            decl::attribute_groups(p);
            match p.current() {
                // `#[A] function (){};`, an attributed closure statement:
                // the attributes belong to the `ClosureExpr`, wrapped in
                // an `ExprStmt`.
                K::FUNCTION_KW if !function_is_decl(p) => {
                    let cm = decl::closure_expr(p, Some(m));
                    let stmt = cm.precede(p);
                    semicolon(p);
                    stmt.complete(p, K::EXPR_STMT);
                }
                K::FUNCTION_KW => decl::function_decl(p, m),
                K::CLASS_KW => decl::class_decl(p, m),
                K::INTERFACE_KW => decl::interface_decl(p, m),
                K::TRAIT_KW => decl::trait_decl(p, m),
                K::ENUM_KW => decl::enum_decl(p, m),
                K::ABSTRACT_KW | K::FINAL_KW | K::READONLY_KW => {
                    decl::modifiers_then_class(p, m);
                }
                // `#[A] const X = 1;` is legal at top level on 8.5, where
                // attributes on constants land: the groups are the
                // statement's, and placement is the compiler's.
                K::CONST_KW if top_level => const_stmt(p, m),
                _ => {
                    m.abandon(p);
                    statement(p, top_level);
                }
            }
        }
        // A closure, an arrow function or an anonymous class owns its
        // attributes.
        _ => expr_stmt(p),
    }
}

/// The kind after the (possibly several) `#[…]` attribute groups at the
/// cursor.
fn peek_past_attributes(p: &mut Parser<'_>) -> K {
    let mut n = 0;
    loop {
        if p.nth(n) != K::ATTRIBUTE_OPEN {
            return p.nth(n);
        }
        // Skip one balanced group, `#[` … `]`, with nesting.
        let mut depth = 0_u32;
        loop {
            let kind = p.nth(n);
            n += 1;
            match kind {
                K::ATTRIBUTE_OPEN | K::L_BRACK => depth += 1,
                K::R_BRACK => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                K::EOF => return K::EOF,
                _ => {}
            }
        }
    }
}

/// Whether `function` here introduces a declaration rather than a closure
/// expression: `function name` or `function &name`.
fn function_is_decl(p: &mut Parser<'_>) -> bool {
    let after_amp = usize::from(p.nth(1) == K::AMP);
    let name = p.nth(1 + after_amp);
    // `readonly` is kept as a function name since 8.1 (`php -l` accepts
    // `function readonly() {}`).
    name == K::IDENT || ckw::is_contextual_only(name) || name == K::READONLY_KW
}

/// `{ Stmt* }`, completing a `BLOCK` node: a statement list whose
/// statements are php's `inner_statement`s.
pub(crate) fn block(p: &mut Parser<'_>) -> CompletedMarker {
    block_of(p, false)
}

/// [`block`], or with `top_level` the braces of a `namespace { … }`,
/// whose statements are php's `top_statement`s. Inside braces a token
/// that starts no statement carries no clause (probed 8.5.9: `{ = }`,
/// `namespace A { = }`).
fn block_of(p: &mut Parser<'_>, top_level: bool) -> CompletedMarker {
    let m = p.start();
    if !p.eat(K::L_CURLY) {
        // Where a block is the one continuation php spells it (`try ;`,
        // `catch (E) ;`, `finally ;` are `expecting "{"`, probed 8.5.9).
        let d = p.unexpected_here_expecting("\"{\"");
        p.error(d);
        return m.complete(p, K::BLOCK);
    }
    stmt_list_until(p, &[K::R_CURLY], None, top_level);
    expect_closer(p, K::R_CURLY, None);
    m.complete(p, K::BLOCK)
}

/// Statements until one of `enders`, or the end of the input, with the
/// stuck recovery; `clause` is what a token that starts no statement
/// reports as expected inside this list, and the enclosing list's clause
/// stands again after it.
fn stmt_list_until(
    p: &mut Parser<'_>,
    enders: &[K],
    clause: Option<&'static str>,
    top_level: bool,
) {
    let outer = std::mem::replace(&mut p.stmt_clause, clause);
    while !p.at_eof() && !enders.contains(&p.current()) {
        if p.is_stuck() {
            let e = p.start();
            p.bump_any();
            e.complete(p, K::ERROR);
            continue;
        }
        statement(p, top_level);
    }
    p.stmt_clause = outer;
}

/// A `STMT_LIST` node for an alternative-syntax body, ending before any
/// of `enders`, the construct's `end…` keyword family; `clause` is the
/// body's own (`"elseif" or "else" or "endif"` inside an `if:`; none
/// inside a `while:`, an `else:` or a `for:`; probed 8.5.9).
fn alt_stmt_list(p: &mut Parser<'_>, enders: &[K], clause: Option<&'static str>) {
    let m = p.start();
    stmt_list_until(p, enders, clause, false);
    m.complete(p, K::STMT_LIST);
}

/// The zero-width span after the previous token: where a missing `;` or
/// `,` was due, which is what the repairs ask the high-water mark about.
fn after_previous(p: &Parser<'_>) -> Span {
    let insert_at = p.prev_span();
    Span {
        file: insert_at.file,
        start: insert_at.end(),
        len: 0,
    }
}

/// Expects a closing delimiter. On failure the diagnostic is php's for
/// what stands there: the end of the input inside an open delimiter and
/// a closer that closes the wrong thing are the balance text, and any
/// other token, a `?>` (which php's scanner hands its parser as `;`)
/// included, is the unexpected token with `expecting`, the clause php
/// prints at this closer's position where it prints one (`")"` after a
/// complete argument, none after a parenthesised expression, whose
/// operator set is too large to spell). A site whose list already
/// reported the token at this offset says nothing more.
///
/// ```php
/// foo(1;
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected token ";", expecting ")" in %s on line 2
/// ```
pub(crate) fn expect_closer(p: &mut Parser<'_>, closer: K, expecting: Option<&str>) {
    if p.eat(closer) {
        return;
    }
    if !p.at_eof()
        && p.bracket_closer_message().is_none()
        && p.has_error_at_or_after(p.current_span().start)
    {
        return;
    }
    let d = p.unexpected_here_maybe_expecting(expecting);
    p.error(d);
}

/// The `;` (or the close-tag or end-of-input glue) terminating a simple
/// statement. A missing semicolon in a high-confidence position gets the
/// repair; otherwise the caller's recovery takes over.
pub(crate) fn semicolon(p: &mut Parser<'_>) {
    semicolon_expecting(p, None);
}

/// [`semicolon`] that pins php's terminator `, expecting …` clause: the
/// follow set php's LALR tables print at a simple statement's terminator
/// position, after a complete value, which a hand-written parser cannot
/// rebuild. The clause depends on the caller: `echo`, `global`, `static`
/// and `const` print `"," or ";"`, since another list item may follow; a
/// single-value terminator (`return`, `break`, `continue`, `goto`,
/// `do`-`while`, the `unset(...)` statement) prints `";"`; a statement
/// whose completed expression can still take a trailing operator
/// (`print`, `throw`, a bare expression, an assignment, `yield`) prints no
/// clause and passes `None` (probed 8.5.7). Only the plain
/// unexpected-token branch carries the clause; the missing-`;` repair (a
/// statement start or a newline follows) is untouched, and the end of the
/// input and a bracket imbalance defer to the balance wording through
/// [`Parser::unexpected_here_expecting`].
///
/// ```php
/// $x = 1
/// $y = 2;
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected variable "$y" in %s on line 3
/// ```
pub(crate) fn semicolon_expecting(p: &mut Parser<'_>, expecting: Option<&str>) {
    if p.eat(K::SEMICOLON) {
        return;
    }
    // `?>` supplies the `;` (the close tag itself is consumed at list
    // level), and inline HTML likewise ends a statement. A bare end of
    // input does not: a statement that runs off the end of the file
    // without its `;` is a parse error (`eval('abc')` is `unexpected end
    // of file`).
    if p.at(K::CLOSE_TAG) || p.at(K::INLINE_HTML) {
        return;
    }
    // The statement already failed at or after where the `;` was due:
    // the earlier diagnostic is the story (`function echo(){}` reports
    // the parameter list's `expecting "("`, `const return` the name
    // slot's `expecting identifier`). Otherwise the diagnostic is php's:
    // the token that stood where the `;` was due, with the terminator's
    // clause, at that token's line.
    if !p.has_error_at_or_after(after_previous(p).start) {
        let d = p.unexpected_here_maybe_expecting(expecting);
        p.error(d);
    }
    // What the repair decides is the tree: a statement start on the next
    // line, or any token across a line break, is far likelier the next
    // statement than a stray on this one, so the parse goes on from it
    // rather than skipping to a boundary. `{` counts as a statement start
    // only across a line break: on the same line it is far likelier a
    // stray brace on the current statement (`const X = 1 {0};`, probed
    // 8.5.7).
    let stmt_starts_here =
        p.newline_before_current() || (STMT_START.contains(&p.current()) && !p.at(K::L_CURLY));
    if !stmt_starts_here {
        recover_to_stmt_boundary(p);
    }
}

/// The missing-comma repair, shared by the argument and element lists:
/// the token that stood where the `,` was due, worded as php words it,
/// and the list goes on from it. `php_closer` is the list's close token
/// as php spells it: after a complete element php's expected set
/// collapses to the closer alone (`[1 2]` is `unexpected integer "2",
/// expecting "]"`, `foo(1 2)` `expecting ")"`, probed 8.5.7).
///
/// ```php
/// [1 2];
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected integer "2", expecting "]" in %s on line 2
/// ```
pub(crate) fn missing_comma(p: &mut Parser<'_>, php_closer: &str) {
    let d = p.unexpected_here_expecting(php_closer);
    p.error(d);
}

/// The level-2 and level-3 recovery: absorb tokens into an `ERROR` node
/// until a statement boundary, always making progress.
pub(crate) fn recover_to_stmt_boundary(p: &mut Parser<'_>) {
    if at_stmt_recovery(p) {
        // The boundary token itself belongs to the next construct,
        // except a stray `;`, which folds into the error region.
        if p.at(K::SEMICOLON) {
            p.bump(K::SEMICOLON);
        }
        return;
    }
    let e = p.start();
    while !at_stmt_recovery(p) {
        p.bump_any();
    }
    e.complete(p, K::ERROR);
    if p.at(K::SEMICOLON) {
        p.bump(K::SEMICOLON);
    }
}

// ---------------------------------------------------------------------------
// Simple statements
// ---------------------------------------------------------------------------

fn expr_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    // `(void) expr;` is a statement form of its own in php
    // (`T_VOID_CAST expr ';'`): the grant is for the primary at the
    // statement's start alone.
    p.grants.void = true;
    if expr::expr(p) {
        semicolon(p);
        m.complete(p, K::EXPR_STMT);
    } else {
        // No expression could start here: php's unexpected token, with
        // the enclosing list's clause, and the balance text where the
        // token is a closer (`Unmatched '}'`, `Unclosed '{' does not
        // match ')'`). One token absorbed for progress, then
        // synchronised.
        let d = p.unexpected_at_stmt();
        p.error(d);
        m.abandon(p);
        if !at_stmt_recovery(p) {
            let e = p.start();
            p.bump_any();
            e.complete(p, K::ERROR);
        } else if p.at(K::SEMICOLON) {
            p.bump(K::SEMICOLON);
        }
    }
}

fn echo_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    if p.at(K::OPEN_TAG_ECHO) {
        p.bump(K::OPEN_TAG_ECHO);
    } else {
        p.bump(K::ECHO_KW);
    }
    expr::expr_list(p);
    // php's echo-list terminator prints both continuations (`echo 1 2` is
    // `expecting "," or ";"`, probed 8.5.7): another `,` item may follow.
    semicolon_expecting(p, Some("\",\" or \";\""));
    m.complete(p, K::ECHO_STMT);
}

fn return_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::RETURN_KW);
    single_value_tail(p);
    m.complete(p, K::RETURN_STMT);
}

fn break_continue_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    let kind = if p.at(K::BREAK_KW) {
        p.bump(K::BREAK_KW);
        K::BREAK_STMT
    } else {
        p.bump(K::CONTINUE_KW);
        K::CONTINUE_STMT
    };
    single_value_tail(p);
    m.complete(p, kind);
}

/// The optional value and the `;` of a `return`, `break` or `continue`:
/// php's clause at the value slot and after it is `";"` alone (`return
/// ,;` and `return 1 foo` are `expecting ";"`, probed 8.5.9).
fn single_value_tail(p: &mut Parser<'_>) {
    if !p.at(K::SEMICOLON) && !p.at(K::CLOSE_TAG) && !p.at_eof() && !expr::expr(p) {
        let d = p.unexpected_here_expecting("\";\"");
        p.error(d);
        recover_to_stmt_boundary(p);
        return;
    }
    semicolon_expecting(p, Some("\";\""));
}

fn goto_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::GOTO_KW);
    if p.at(K::IDENT) && !ckw::is_callable_text(K::IDENT, p.current_text()) {
        p.bump(K::IDENT);
    } else {
        // A `goto` target is a plain label identifier, `T_STRING`, so
        // `callable` is refused with the reserved words; php spells the
        // clause when it is missing (`goto exit`, `goto 5` and `goto ;`
        // are `expecting identifier`, probed 8.5.7; `goto callable`,
        // 8.5.9).
        let d = p.unexpected_here_expecting("identifier");
        p.error(d);
    }
    semicolon_expecting(p, Some("\";\""));
    m.complete(p, K::GOTO_STMT);
}

fn global_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::GLOBAL_KW);
    loop {
        if p.at(K::VARIABLE) {
            let v = p.start();
            p.bump(K::VARIABLE);
            v.complete(p, K::SIMPLE_VAR_EXPR);
        } else if p.at(K::DOLLAR) {
            // `global $$name;` and `global ${expr};`: Zend's
            // `simple_variable`.
            expr::variable_variable(p);
        } else {
            // `global ;` and `global $x, ;` are `expecting variable or
            // "$"` (probed 8.5.9).
            let d = p.unexpected_here_expecting("variable or \"$\"");
            p.error(d);
            break;
        }
        if !p.eat(K::COMMA) {
            break;
        }
    }
    // After a `global` variable php expects another `,`-separated simple
    // variable or the terminating `;`: a trailer on a `$$name`
    // (globalNonSimpleVariableError: `global $$foo->bar;`) is
    // `unexpected "->", expecting "," or ";"` (probed 8.5.7).
    semicolon_expecting(p, Some("\",\" or \";\""));
    m.complete(p, K::GLOBAL_STMT);
}

fn static_var_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::STATIC_KW);
    loop {
        let d = p.start();
        if p.at(K::VARIABLE) {
            p.bump(K::VARIABLE);
            if p.eat(K::EQ) && !expr::expr(p) {
                p.error_unexpected();
            }
            d.complete(p, K::STATIC_VAR_DECLARATOR);
        } else {
            d.abandon(p);
            // `static $x, ;` is `expecting variable` (probed 8.5.9).
            let diag = p.unexpected_here_expecting("variable");
            p.error(diag);
            break;
        }
        if !p.eat(K::COMMA) {
            break;
        }
    }
    semicolon_expecting(p, Some("\",\" or \";\""));
    m.complete(p, K::STATIC_VAR_STMT);
}

fn unset_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::UNSET_KW);
    if p.eat(K::L_PAREN) {
        // Zend's `unset_variables`: each argument is a `variable`
        // production (`unset($x + $y)` errors at `+`, `unset(new A())`
        // at `)`).
        expr::unset_operands(p);
        expect_closer(p, K::R_PAREN, Some(expr::AFTER_VARIABLE));
    } else {
        // php's bison clause at the mandatory-paren keywords (probed
        // 8.5.7: `unset` at the end of the input is `… unexpected end of
        // file, expecting "("`).
        let d = p.unexpected_here_expecting("\"(\"");
        p.error(d);
    }
    semicolon_expecting(p, Some("\";\""));
    m.complete(p, K::UNSET_STMT);
}

fn const_stmt(p: &mut Parser<'_>, m: Marker) {
    p.bump(K::CONST_KW);
    // A top-level constant's name is a plain identifier; the
    // semi-reserved set applies to class constants alone (php-src
    // `const_list` against `class_const_list`; `const FOR = 1;` is
    // rejected in php).
    decl::const_declarators(p, false);
    // Another declarator may follow, so php's terminator clause here is
    // `"," or ";"` (probed 8.5.7: `const X = "s"{0};`).
    semicolon_expecting(p, Some("\",\" or \";\""));
    m.complete(p, K::CONST_STMT);
}

fn halt_compiler_stmt(p: &mut Parser<'_>, top_level: bool) {
    // A `__halt_compiler()` that is not outermost is a compile-time fatal,
    // and a compiler fact rather than a parse error: php parses right past
    // it and reports any parse failure first (bug60099: a halt inside an
    // unclosed braced namespace reports `Unclosed '{'`, not the
    // outermost-scope fatal). The lexer only swallows the trailing bytes
    // for the outermost-frame form, so the enclosing braces stay visible
    // here; the compiler owns the scope fatal.
    let _ = top_level;
    let m = p.start();
    p.bump(K::HALT_COMPILER_KW);
    if p.eat(K::L_PAREN) {
        expect_closer(p, K::R_PAREN, Some("\")\""));
    } else {
        let d = p.unexpected_here_expecting("\"(\"");
        p.error(d);
    }
    // The close tag is the `;` here as at every terminator site (Zend
    // returns `?>` as `';'`); it is taken into the statement because the
    // trailer follows it, and the lexer has already stopped.
    if !p.eat(K::SEMICOLON)
        && !p.eat(K::CLOSE_TAG)
        && !p.has_error_at_or_after(p.current_span().start)
    {
        // `__halt_compiler() foo` is `expecting ";"` (probed 8.5.9).
        let d = p.unexpected_here_expecting("\";\"");
        p.error(d);
    }
    if p.at(K::HALT_DATA) {
        p.bump(K::HALT_DATA);
    }
    m.complete(p, K::HALT_COMPILER_STMT);
}

// ---------------------------------------------------------------------------
// Control flow
// ---------------------------------------------------------------------------

/// The parenthesised condition of `if`, `elseif`, `while`, `do … while`
/// and `switch`: the `(` is the one token php spells after the keyword
/// (`if if` is `expecting "("`), and after the condition the operator set is
/// too large to spell (`if ($x ;` has no clause; probed 8.5.9).
fn paren_cond(p: &mut Parser<'_>) {
    if p.eat(K::L_PAREN) {
        if !expr::expr(p) {
            p.error_unexpected();
        }
        expect_closer(p, K::R_PAREN, None);
    } else {
        let d = p.unexpected_here_expecting("\"(\"");
        p.error(d);
    }
}

fn if_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::IF_KW);
    paren_cond(p);
    if p.eat(K::COLON) {
        // The alternative syntax: `if: … (elseif: …)* (else: …)? endif;`.
        // The list stops at any `end…` keyword so a wrong terminator
        // reaches `alt_terminator` instead of recovery garbage.
        const ENDERS: &[K] = &[
            K::ELSEIF_KW,
            K::ELSE_KW,
            K::ENDIF_KW,
            K::ENDWHILE_KW,
            K::ENDFOR_KW,
            K::ENDFOREACH_KW,
            K::ENDSWITCH_KW,
            K::ENDDECLARE_KW,
        ];
        // Inside an `if:` or `elseif:` body php spells the three
        // continuations; after `else:` only `endif` can follow and the
        // clause is gone (probed 8.5.9).
        const IF_CLAUSE: Option<&str> = Some("\"elseif\" or \"else\" or \"endif\"");
        alt_stmt_list(p, ENDERS, IF_CLAUSE);
        let mut clause = IF_CLAUSE;
        while p.at(K::ELSEIF_KW) || (p.at(K::ELSE_KW) && p.nth(1) == K::IF_KW) {
            let c = p.start();
            if p.at(K::ELSEIF_KW) {
                p.bump(K::ELSEIF_KW);
            } else {
                // `else if` in the alternative syntax: parsed as an
                // `elseif` clause; the compiler rejects it (`else if
                // (2):` is `unexpected token "if", expecting ":"`).
                p.bump(K::ELSE_KW);
                let d = p.unexpected_here_expecting("\":\"");
                p.error(d);
                p.bump(K::IF_KW);
            }
            paren_cond(p);
            if !p.eat(K::COLON) {
                // In the alternative syntax the `:` is the one
                // continuation (`elseif (2) ;` and `else ;` are
                // `expecting ":"`, probed 8.5.9).
                let d = p.unexpected_here_expecting("\":\"");
                p.error(d);
            }
            alt_stmt_list(p, ENDERS, IF_CLAUSE);
            c.complete(p, K::ALT_ELSEIF_CLAUSE);
        }
        if p.at(K::ELSE_KW) {
            let c = p.start();
            p.bump(K::ELSE_KW);
            if !p.eat(K::COLON) {
                let d = p.unexpected_here_expecting("\":\"");
                p.error(d);
            }
            alt_stmt_list(p, ALT_ENDERS, None);
            clause = None;
            c.complete(p, K::ALT_ELSE_CLAUSE);
        }
        alt_terminator(p, K::ENDIF_KW, clause);
    } else {
        statement(p, false);
        while p.at(K::ELSEIF_KW) {
            let c = p.start();
            p.bump(K::ELSEIF_KW);
            paren_cond(p);
            statement(p, false);
            c.complete(p, K::ELSEIF_CLAUSE);
        }
        if p.at(K::ELSE_KW) {
            let c = p.start();
            p.bump(K::ELSE_KW);
            statement(p, false);
            c.complete(p, K::ELSE_CLAUSE);
        }
    }
    m.complete(p, K::IF_STMT);
}

/// php's clause inside an alternative-syntax switch body, the arm
/// starters beside the closer in the body's own order (probed 8.5.9).
const ALT_SWITCH_CLAUSE: &str = "\"endswitch\" or \"case\" or \"default\"";

/// The alternative-syntax terminator family.
const ALT_ENDERS: &[K] = &[
    K::ENDIF_KW,
    K::ENDWHILE_KW,
    K::ENDFOR_KW,
    K::ENDFOREACH_KW,
    K::ENDSWITCH_KW,
    K::ENDDECLARE_KW,
];

/// Consumes the `end…;` of an alternative-syntax construct. A wrong
/// `end…` keyword, or the end of the input, is the token php did not
/// want after the body, with the body's clause (`if (1): endwhile;` is
/// `unexpected token "endwhile", expecting "elseif" or "else" or
/// "endif"`, `while (1): echo 1;` at the end is `unexpected end of
/// file`; probed 8.5.9); after the keyword only `;` can follow.
///
/// ```php
/// if (1): endwhile;
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected token "endwhile", expecting "elseif" or "else" or "endif" in %s on line 2
/// ```
fn alt_terminator(p: &mut Parser<'_>, ender: K, clause: Option<&str>) {
    if p.eat(ender) {
        semicolon_expecting(p, Some("\";\""));
        return;
    }
    let d = p.unexpected_here_maybe_expecting(clause);
    p.error(d);
    if ALT_ENDERS.contains(&p.current()) {
        // The wrong `end…` keyword: consumed, with its `;`, as the
        // construct's end, the one reading that makes the construct
        // close.
        let e = p.start();
        p.bump_any();
        e.complete(p, K::ERROR);
        p.eat(K::SEMICOLON);
        return;
    }
    recover_to_stmt_boundary(p);
}

fn while_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::WHILE_KW);
    paren_cond(p);
    if p.eat(K::COLON) {
        alt_stmt_list(p, ALT_ENDERS, None);
        alt_terminator(p, K::ENDWHILE_KW, None);
    } else {
        statement(p, false);
    }
    m.complete(p, K::WHILE_STMT);
}

fn do_while_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::DO_KW);
    statement(p, false);
    if p.at(K::WHILE_KW) {
        p.bump(K::WHILE_KW);
        paren_cond(p);
    } else {
        // After the body only `while` can follow (`do ;` is `unexpected
        // end of file, expecting "while"`, probed 8.5.9).
        let d = p.unexpected_here_expecting("\"while\"");
        p.error(d);
    }
    semicolon_expecting(p, Some("\";\""));
    m.complete(p, K::DO_WHILE_STMT);
}

fn for_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::FOR_KW);
    if p.eat(K::L_PAREN) {
        let w = p.start();
        if !p.at(K::SEMICOLON) {
            for_exprs(p, false);
        }
        w.complete(p, K::FOR_INIT);
        for_semicolon(p);
        let w = p.start();
        if !p.at(K::SEMICOLON) {
            for_exprs(p, true);
        }
        w.complete(p, K::FOR_COND);
        for_semicolon(p);
        let w = p.start();
        if !p.at(K::R_PAREN) && !p.at(K::SEMICOLON) {
            for_exprs(p, false);
        }
        w.complete(p, K::FOR_STEP);
        expect_closer(p, K::R_PAREN, Some("\")\""));
    } else {
        let d = p.unexpected_here_expecting("\"(\"");
        p.error(d);
    }
    if p.eat(K::COLON) {
        alt_stmt_list(p, ALT_ENDERS, None);
        alt_terminator(p, K::ENDFOR_KW, None);
    } else {
        statement(p, false);
    }
    m.complete(p, K::FOR_STMT);
}

/// One of a `for` header's three expression lists, php's
/// `non_empty_for_exprs`: every element may be a `(void)` cast, except
/// the last of the condition list, whose value is the test
/// (`for_cond_exprs`); there php wants another element (`for (;(void)$x;)`
/// is `unexpected token ";", expecting ","`, probed 8.5.9).
fn for_exprs(p: &mut Parser<'_>, cond: bool) {
    loop {
        let voided = p.at(K::CAST)
            && p.current_text()
                .to_ascii_lowercase()
                .windows(4)
                .any(|w| w == b"void");
        p.grants.void = true;
        if !expr::expr(p) {
            p.error_unexpected();
            break;
        }
        if !p.eat(K::COMMA) {
            if cond && voided && p.at(K::SEMICOLON) {
                let d = p.unexpected_here_expecting("\",\"");
                p.error(d);
            }
            break;
        }
    }
}

/// A `;` between a `for` header's lists: after a complete list php
/// expects it or another element (`for (1 2;;)` is `unexpected integer
/// "2", expecting ";"`, probed 8.5.9); a stray closer is the balance
/// text.
fn for_semicolon(p: &mut Parser<'_>) {
    if !p.eat(K::SEMICOLON) && !p.has_error_at_or_after(p.current_span().start) {
        let d = p.unexpected_here_expecting("\";\"");
        p.error(d);
    }
}

fn foreach_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::FOREACH_KW);
    if p.eat(K::L_PAREN) {
        if !expr::expr(p) {
            p.error_unexpected();
        }
        if !p.eat(K::AS_KW) {
            p.error_unexpected();
        }
        // `key => target` or just `target`, either by reference, each a
        // Zend `foreach_variable`: a variable production, or a
        // `list(...)` or `[...]` pattern, which destructures here without
        // an `=` (the one-shot grant). A leading `&` before the first
        // expression is usually the by-reference value target, but php
        // also parses the illegal by-reference key form `&$k => $v`: the
        // grammar shapes that as a `FOREACH_KEY` carrying the `&`, and
        // the compiler rejects it (`Key element cannot be a reference`).
        // At the closer php's clause is the dereference set after a
        // variable or a `[…]` pattern, and `"=>" or ")"` after a
        // `list(…)` that may yet be the key (probed 8.5.9).
        let mut closer = Some(expr::AFTER_VARIABLE);
        if p.at(K::AMP) {
            let t = p.start();
            p.bump(K::AMP);
            p.grants.list_target = true;
            expr::variable_target(p);
            if p.at(K::FAT_ARROW) {
                // `&$k => …`: what was parsed was the by-reference key,
                // completed as `FOREACH_KEY` with the `&` inside it for
                // the compiler; then the value target, itself optionally
                // by reference.
                t.complete(p, K::FOREACH_KEY);
                p.bump(K::FAT_ARROW);
                let v = p.start();
                p.eat(K::AMP);
                p.grants.list_target = true;
                expr::variable_target(p);
                v.complete(p, K::FOREACH_TARGET);
            } else {
                t.complete(p, K::FOREACH_TARGET);
            }
        } else {
            let first = p.start();
            p.grants.list_target = true;
            let target = expr::variable_target(p);
            if target.is_some() && p.at(K::FAT_ARROW) {
                // What was parsed was the key; `=>` lives inside
                // `ForeachKey`.
                p.bump(K::FAT_ARROW);
                first.complete(p, K::FOREACH_KEY);
                let t = p.start();
                p.eat(K::AMP);
                p.grants.list_target = true;
                expr::variable_target(p);
                t.complete(p, K::FOREACH_TARGET);
            } else if target.is_some() {
                first.complete(p, K::FOREACH_TARGET);
                if target == Some(K::LIST_EXPR) {
                    closer = Some("\"=>\" or \")\"");
                }
            } else {
                // No target expression at all: the schema promises a
                // `ForeachKey` is only ever built around a parsed key.
                first.abandon(p);
            }
        }
        expect_closer(p, K::R_PAREN, closer);
    } else {
        let d = p.unexpected_here_expecting("\"(\"");
        p.error(d);
    }
    if p.eat(K::COLON) {
        alt_stmt_list(p, ALT_ENDERS, None);
        alt_terminator(p, K::ENDFOREACH_KW, None);
    } else {
        statement(p, false);
    }
    m.complete(p, K::FOREACH_STMT);
}

fn switch_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::SWITCH_KW);
    paren_cond(p);
    // Inside a switch body php spells the arm starters beside the
    // closer, in the body's own order (probed 8.5.9).
    if p.eat(K::L_CURLY) {
        switch_arms(p, &[K::R_CURLY], "\"case\" or \"default\" or \"}\"");
        expect_closer(p, K::R_CURLY, None);
    } else if p.at(K::COLON) {
        p.bump(K::COLON);
        switch_arms(p, &[K::ENDSWITCH_KW], ALT_SWITCH_CLAUSE);
        alt_terminator(p, K::ENDSWITCH_KW, Some(ALT_SWITCH_CLAUSE));
    } else {
        let d = p.unexpected_here_expecting("\":\" or \"{\"");
        p.error(d);
    }
    m.complete(p, K::SWITCH_STMT);
}

fn switch_arms(p: &mut Parser<'_>, enders: &[K], clause: &'static str) {
    let outer = p.stmt_clause.replace(clause);
    // One `;` may open the list (`switch_case_list: '{' ';' case_list
    // '}'`), an archaism php's grammar keeps.
    if p.at(K::SEMICOLON) {
        let e = p.start();
        p.bump(K::SEMICOLON);
        e.complete(p, K::EMPTY_STMT);
    }
    while !p.at_eof() && !enders.contains(&p.current()) {
        if p.at(K::CASE_KW) || p.at(K::DEFAULT_KW) {
            let arm = p.start();
            let is_default = p.at(K::DEFAULT_KW);
            if is_default {
                p.bump(K::DEFAULT_KW);
            } else {
                p.bump(K::CASE_KW);
                if !expr::expr(p) {
                    p.error_unexpected();
                }
            }
            // A `;` after `case` or `default` is legal, archaic, and in
            // the corpus; php spells both after `default` (`default }`
            // is `expecting ":" or ";"`, probed 8.5.9), where after a
            // case's expression the operator set is too large.
            if !p.eat(K::COLON)
                && !p.eat(K::SEMICOLON)
                && !p.has_error_at_or_after(p.current_span().start)
            {
                let d = if is_default {
                    p.unexpected_here_expecting("\":\" or \";\"")
                } else {
                    p.unexpected_here()
                };
                p.error(d);
            }
            let body = p.start();
            while !p.at_eof()
                && !p.at(K::CASE_KW)
                && !p.at(K::DEFAULT_KW)
                && !enders.contains(&p.current())
            {
                if p.is_stuck() {
                    let e = p.start();
                    p.bump_any();
                    e.complete(p, K::ERROR);
                    continue;
                }
                statement(p, false);
            }
            body.complete(p, K::STMT_LIST);
            arm.complete(p, K::SWITCH_ARM);
        } else {
            let d = p.unexpected_at_stmt();
            p.error(d);
            let e = p.start();
            p.bump_any();
            e.complete(p, K::ERROR);
        }
    }
    p.stmt_clause = outer;
}

fn try_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::TRY_KW);
    block(p);
    while p.at(K::CATCH_KW) {
        let c = p.start();
        p.bump(K::CATCH_KW);
        if p.eat(K::L_PAREN) {
            let list = p.start();
            loop {
                if !types::name_ref(p) {
                    p.error_unexpected();
                    break;
                }
                if !p.eat(K::PIPE) {
                    break;
                }
            }
            list.complete(p, K::CATCH_TYPE_LIST);
            p.eat(K::VARIABLE); // a capture-less catch is 8.0
            expect_closer(p, K::R_PAREN, Some("\")\""));
        } else {
            let d = p.unexpected_here_expecting("\"(\"");
            p.error(d);
        }
        block(p);
        c.complete(p, K::CATCH_CLAUSE);
    }
    if p.at(K::FINALLY_KW) {
        let c = p.start();
        p.bump(K::FINALLY_KW);
        block(p);
        c.complete(p, K::FINALLY_CLAUSE);
    }
    m.complete(p, K::TRY_STMT);
}

// ---------------------------------------------------------------------------
// Namespaces, use, declare
// ---------------------------------------------------------------------------

/// `namespace N;`, `namespace N { … }` or `namespace { … }`. Each of
/// php's three rules runs `RESET_DOC_COMMENT()`: the bracketed ones in
/// a mid-rule action that needs the `{` as its lookahead, the other
/// after its `;` with none, so the take stands before the `{` and
/// before the `;`, and a doc comment written before the statement
/// reaches no declaration after it:
///
/// ```php
/// /** D */
/// namespace A {
///     class X {}
///     var_dump((new \ReflectionClass(X::class))->getDocComment());
/// }
/// ```
/// ```expect
/// bool(false)
/// ```
///
/// ```php
/// /** D */
/// namespace A; /** E */ class X {}
/// var_dump((new \ReflectionClass(X::class))->getDocComment());
/// ```
/// ```expect
/// string(8) "/** E */"
/// ```
fn namespace_def(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::NAMESPACE_KW);
    if p.at(K::IDENT) || p.at(K::NAME_QUALIFIED) {
        p.bump_any();
    } else if p.at(K::NAME_RELATIVE) || p.at(K::NAME_FULLY_QUALIFIED) {
        // A namespace declaration's name is a plain or qualified name. A
        // namespace-relative (`namespace\x`) or fully qualified (`\x`)
        // name cannot start one; php reports it with `expecting "{"`,
        // the other production after `namespace` being the anonymous
        // `namespace { … }` block (php-src
        // namespace_name_namespace_start, probed 8.5.7).
        let d = p.unexpected_expecting("\"{\"");
        p.error(d);
        p.bump_any();
    } else if ckw::is_member_name(p.current()) && !p.at(K::L_CURLY) {
        // A reserved word is a legal namespace name since the 8.0
        // namespaced-names RFC (`namespace fn;`, `namespace enum {}`;
        // php-src namespace_name_reserved_keywords.php, pinned 8.5.6).
        p.bump_remap(K::IDENT);
    } else if !p.at(K::L_CURLY) {
        // `namespace ;` is `expecting "{"` (probed 8.5.9).
        let d = p.unexpected_here_expecting("\"{\"");
        p.error(d);
    }
    // `RESET_DOC_COMMENT()`: the node takes what is pending and keeps
    // nothing of it (the docs above).
    p.doc_comment();
    if p.at(K::L_CURLY) {
        block_of(p, true);
    } else {
        // After the name only `;` or the braces can follow (`namespace A
        // foo` is `expecting "{"`, probed 8.5.9: bison lists the one
        // shift, the `;` being a default reduction's).
        semicolon_expecting(p, Some("\"{\""));
    }
    m.complete(p, K::NAMESPACE_DEF);
}

fn use_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::USE_KW);
    // The statement-level kind: `use function …` or `use const …`. A
    // plain `use` is a mixed import whose group items may each carry
    // their own `function` or `const`; a statement-level kind forbids a
    // per-item kind, a distinction php's expected-set clause reflects
    // when a group item is malformed (`… expecting … or "function" or
    // "const"` for the mixed form alone).
    let mixed = !(p.at(K::FUNCTION_KW) || p.at(K::CONST_KW));
    if !mixed {
        p.bump_any();
    }
    let mut after_comma = false;
    let mut grouped = false;
    loop {
        // The group form, `Prefix\{ … }`: the prefix may be fully
        // qualified (`use function \Foo\{…}`, php-src ns_095).
        if (p.at(K::NAME_QUALIFIED) || p.at(K::IDENT) || p.at(K::NAME_FULLY_QUALIFIED))
            && p.nth(1) == K::BACKSLASH
            && p.nth(2) == K::L_CURLY
        {
            use_group(p, mixed);
            grouped = true;
        } else {
            let c = p.start();
            if !use_name_ref(p) {
                // A statement-level kind, or a comma, reduces to the
                // name slot (`use function ;` and `use A, ;` are
                // `expecting identifier or fully qualified name or
                // namespaced name`); a bare `use ;` spells no clause
                // (`use ;`, `use callable;`, probed 8.5.9).
                let d = if mixed && !after_comma {
                    p.unexpected_here()
                } else {
                    p.unexpected_here_expecting(
                        "identifier or fully qualified name or namespaced name",
                    )
                };
                p.error(d);
                c.abandon(p);
                break;
            }
            // `A\` with nothing after the separator is a group whose
            // brace is missing (`use A\ ;` is `expecting "{"`).
            if p.at(K::BACKSLASH) {
                p.bump(K::BACKSLASH);
                let d = p.unexpected_here_expecting("\"{\"");
                p.error(d);
            }
            use_alias(p);
            c.complete(p, K::USE_CLAUSE);
        }
        if !p.eat(K::COMMA) {
            break;
        }
        after_comma = true;
    }
    // After a group only `;` can follow; after a plain clause another
    // may (`use A\{B} foo` is `expecting ";"`, `use A foo` `expecting ","
    // or ";"`, probed 8.5.9).
    if grouped {
        semicolon_expecting(p, Some("\";\""));
    } else {
        semicolon_expecting(p, Some("\",\" or \";\""));
    }
    m.complete(p, K::USE_STMT);
}

/// The group form of a `use` clause, `Prefix\{ Item, … }`, entered once
/// the caller has confirmed the `Prefix\{` shape. `mixed` is true for a
/// plain `use Baz\{…}`, where each item may carry its own `function` or
/// `const`, and false for a statement-level `use function` or `use const`
/// group, where a per-item kind is forbidden. Every item-list parse error
/// is php's: its `expecting …` clause is the LALR expected set php's bison
/// tables carry at that position (php-src `ns_trailing_comma_error_01`
/// to `08`, `ns_094`, `ns_096`, and the nested-brace `ns_088`).
fn use_group(p: &mut Parser<'_>, mixed: bool) {
    let g = p.start();
    p.bump_any();
    p.bump(K::BACKSLASH);
    p.bump(K::L_CURLY);
    // php's expected-set clause at an item start (empty braces, a leading
    // comma) wants a name, plus `function` and `const` for a mixed group.
    let item_start_expecting = if mixed {
        "identifier or namespaced name or \"function\" or \"const\""
    } else {
        "identifier or namespaced name"
    };
    // Empty group braces are a parse error in php: at least one name is
    // required (`use Baz\{};`).
    if p.at(K::R_CURLY) {
        let d = p.unexpected_expecting(item_start_expecting);
        p.error(d);
    }
    let mut parsed_item = false;
    loop {
        if p.at(K::R_CURLY) || p.at_eof() {
            break;
        }
        // After a name and a comma the trailing comma has reduced to
        // `possible_comma`, so only `}` may follow; at an item start a
        // name (plus the kind keywords for a mixed group) is expected.
        let expecting = if parsed_item {
            "\"}\""
        } else {
            item_start_expecting
        };
        let item = p.start();
        if p.at(K::FUNCTION_KW) || p.at(K::CONST_KW) {
            if mixed {
                // A plain `use Baz\{…}` group may carry a per-item kind
                // (`use Baz\{function f, const K}`).
                p.bump_any();
            } else {
                // A statement-level `use function` or `use const` group
                // forbids a per-item kind: php reports the keyword as an
                // unexpected token, not the kind-set-on-both fatal
                // (ns_094).
                let d = p.unexpected_expecting(expecting);
                p.error(d);
                item.abandon(p);
                let e = p.start();
                p.bump_any();
                e.complete(p, K::ERROR);
                continue;
            }
        }
        // A name inside the braces is relative to the prefix: a leading
        // `\` is a parse error in php (`use A\{\B}`, php-src ns_096),
        // legal on the prefix alone; after a comma the clause is the
        // closer's, as for any item (`use Foo\{Baz, \Qux}` is
        // `expecting "}"`, probed 8.5.9).
        if p.at(K::NAME_FULLY_QUALIFIED) {
            let d = p.unexpected_expecting(expecting);
            p.error(d);
            // `name_ref` still consumes it: one diagnostic, and the tree
            // keeps its shape.
        }
        if !use_name_ref(p) {
            let d = p.unexpected_expecting(expecting);
            p.error(d);
            item.abandon(p);
            let e = p.start();
            p.bump_any();
            e.complete(p, K::ERROR);
            continue;
        }
        use_alias(p);
        item.complete(p, K::USE_GROUP_ITEM);
        parsed_item = true;
        if p.eat(K::COMMA) {
            continue;
        }
        // The list is done. Only `}` closes it; any other token is php's
        // `unexpected <tok>, expecting "}"` (php-src ns_088, a nested
        // group brace `B { … }`). Recovered to the group's closing `}`,
        // skipping nested braces, before breaking, so `expect_closer`
        // and the statement's `;` recovery do not emit competing
        // diagnostics: a later missing-`;` would anchor at an earlier
        // span, the end of the last item, and sort ahead of this one.
        if !p.at(K::R_CURLY) && !p.at_eof() {
            let d = p.unexpected_expecting("\"}\"");
            p.error(d);
            let e = p.start();
            let mut depth = 0u32;
            while !p.at_eof() {
                if p.at(K::R_CURLY) {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                } else if p.at(K::L_CURLY) {
                    depth += 1;
                } else if p.at(K::SEMICOLON) && depth == 0 {
                    break;
                }
                p.bump_any();
            }
            e.complete(p, K::ERROR);
        }
        break;
    }
    expect_closer(p, K::R_CURLY, Some("\"}\""));
    g.complete(p, K::USE_GROUP);
}

/// The name a `use` clause or group item imports: php's `namespace_name`
/// is `T_STRING` or a namespaced name, and `callable` is `T_CALLABLE` to
/// its scanner, so the bare word is refused where `types::name_ref`
/// would take it as a type (`use callable;`, `use function callable;`
/// and `use A\{callable};` are parse errors; `use A\callable;` is a name
/// and the compiler's fatal, probed 8.5.9). Consumes one token, or
/// nothing on `false`.
fn use_name_ref(p: &mut Parser<'_>) -> bool {
    if ckw::is_callable_text(p.current(), p.current_text()) {
        return false;
    }
    types::name_ref(p)
}

fn use_alias(p: &mut Parser<'_>) {
    if p.at(K::AS_KW) {
        let a = p.start();
        p.bump(K::AS_KW);
        // php's alias is `T_STRING` alone (`unprefixed_use_declaration`,
        // `use_declaration`): a plain identifier or a contextual-only
        // keyword, never a reserved word, `callable` or a magic constant
        // (`use A\B as list;`, `as callable`, `as __CLASS__` are
        // `expecting identifier`, probed 8.5.9).
        if p.at(K::IDENT) && !ckw::is_callable_text(K::IDENT, p.current_text()) {
            p.bump(K::IDENT);
            a.complete(p, K::USE_ALIAS);
        } else if ckw::is_contextual_only(p.current()) {
            p.bump_remap(K::IDENT);
            a.complete(p, K::USE_ALIAS);
        } else {
            // `UseAlias.name` is a promise (`!`): without a name the node
            // may not exist, so the stray `as` becomes an `ERROR`. php
            // spells the slot (`use A as ;` is `expecting identifier`).
            let d = p.unexpected_here_expecting("identifier");
            p.error(d);
            a.complete(p, K::ERROR);
        }
    }
}

fn declare_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::DECLARE_KW);
    // php's clauses down the directive list (probed 8.5.9): `declare(`
    // wants `identifier`, the name wants `"="`, the value nothing it can
    // spell, and the complete directive `"," or ")"`.
    if p.eat(K::L_PAREN) {
        loop {
            let d = p.start();
            if p.at(K::IDENT) {
                p.bump(K::IDENT);
                if p.eat(K::EQ) {
                    if !expr::expr(p) {
                        p.error_unexpected();
                    }
                } else {
                    let diag = p.unexpected_here_expecting("\"=\"");
                    p.error(diag);
                }
                d.complete(p, K::DECLARE_DIRECTIVE);
            } else {
                d.abandon(p);
                let diag = p.unexpected_here_expecting("identifier");
                p.error(diag);
                break;
            }
            if !p.eat(K::COMMA) {
                break;
            }
        }
        expect_closer(p, K::R_PAREN, Some("\",\" or \")\""));
    } else {
        let d = p.unexpected_here_expecting("\"(\"");
        p.error(d);
    }
    if p.eat(K::COLON) {
        alt_stmt_list(p, ALT_ENDERS, None);
        alt_terminator(p, K::ENDDECLARE_KW, None);
    } else if p.at(K::SEMICOLON) {
        p.bump(K::SEMICOLON);
    } else {
        // php's `declare_statement` is any statement: `declare(ticks=1)
        // foo();` runs `foo()` under the directive (probed 8.5.9).
        statement(p, false);
    }
    m.complete(p, K::DECLARE_STMT);
}
