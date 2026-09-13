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
//! **What a diagnostic says.** The engine has one voice (design §6.1), so
//! a site whose inherited wording had php's text beside it carries php's,
//! and the parser's own `unexpected …` is php's already. Four sites had
//! only Mahout's wording and keep it until #206 re-words the 74 to php's
//! at php's line: the close tag inside an unfinished construct, the stray
//! token where a closer was due, the stray closer no opener matches, and
//! the alternative-syntax terminator, wrong or missing.

pub(crate) mod ckw;
mod decl;
mod expr;
mod strings;
mod types;

use crate::SyntaxKind as K;
use crate::parser::{CompletedMarker, Parser, php_token_name};
use crate::source::{Diagnostic, Span};

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
        || p.peek_at(K::SEMICOLON)
        || p.peek_at(K::R_CURLY)
        || p.peek_at(K::CLOSE_TAG)
        || p.peek_at(K::INLINE_HTML)
        || p.peek_at(K::OPEN_TAG)
        || STMT_START.contains(&p.current())
}

/// A whole file into a `SOURCE_FILE` node.
pub(crate) fn source_file(p: &mut Parser<'_>) {
    let m = p.start();
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

fn statement_inner(p: &mut Parser<'_>, top_level: bool) {
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
        K::CONST_KW => const_stmt(p),
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
        // A lexer-error token already carries the lexer's diagnostic;
        // reporting it again would be a cascade. Absorbed quietly.
        K::ERROR => {
            let e = p.start();
            p.bump(K::ERROR);
            e.complete(p, K::ERROR);
        }
        K::ATTRIBUTE_OPEN => attributed_statement(p),
        K::IDENT if p.nth(1) == K::COLON => {
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
fn attributed_statement(p: &mut Parser<'_>) {
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
                // attributes on constants land; parsed into the statement
                // shape, and placement is the compiler's.
                _ => {
                    m.abandon(p);
                    statement(p, false);
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

/// `{ Stmt* }`, completing a `BLOCK` node.
pub(crate) fn block(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    if !p.eat(K::L_CURLY) {
        p.error_unexpected();
        return m.complete(p, K::BLOCK);
    }
    stmt_list_until(p, &[K::R_CURLY]);
    expect_closer(p, K::R_CURLY, "`{`");
    m.complete(p, K::BLOCK)
}

/// Statements until one of `enders`, or the end of the input, with the
/// stuck recovery.
fn stmt_list_until(p: &mut Parser<'_>, enders: &[K]) {
    while !p.at_eof() && !enders.contains(&p.current()) {
        if p.is_stuck() {
            let e = p.start();
            p.bump_any();
            e.complete(p, K::ERROR);
            continue;
        }
        statement(p, false);
    }
}

/// A `STMT_LIST` node for an alternative-syntax body, ending before any
/// of `enders`, the construct's `end…` keyword family.
fn alt_stmt_list(p: &mut Parser<'_>, enders: &[K]) {
    let m = p.start();
    stmt_list_until(p, enders);
    m.complete(p, K::STMT_LIST);
}

/// The zero-width span after the previous token, where a repair inserts.
fn after_previous(p: &Parser<'_>) -> Span {
    let insert_at = p.prev_span();
    Span {
        file: insert_at.file,
        start: insert_at.end(),
        len: 0,
    }
}

/// Expects a closing delimiter. On failure: a `?>` in the way is the close
/// tag ending an unfinished construct, the end of the input is php's
/// balance text through the parser, and anything else the stray token
/// where the closer was due. The parser's delimiter stack is what the
/// opener is read from.
pub(crate) fn expect_closer(p: &mut Parser<'_>, closer: K, opener_name: &str) {
    if p.eat(closer) {
        return;
    }
    let d = if p.peek_at(K::CLOSE_TAG) {
        Diagnostic::new(
            p.current_span(),
            format!("`?>` ends the script inside an unfinished {opener_name} construct"),
        )
    } else if p.at_eof() {
        // The end of the input inside an open delimiter: php's
        // `Unclosed '{' [on line N]`.
        p.unexpected_here()
    } else {
        Diagnostic::new(
            p.current_span(),
            format!("unclosed delimiter {opener_name}"),
        )
    };
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
pub(crate) fn semicolon_expecting(p: &mut Parser<'_>, expecting: Option<&str>) {
    if p.eat(K::SEMICOLON) {
        return;
    }
    // `?>` supplies the `;` (the close tag itself is consumed at list
    // level), and inline HTML likewise ends a statement. A bare end of
    // input does not: a statement that runs off the end of the file
    // without its `;` is a parse error (`eval('abc')` is `unexpected end
    // of file`).
    if p.peek_at(K::CLOSE_TAG) || p.peek_at(K::INLINE_HTML) {
        return;
    }
    let span = after_previous(p);
    // `{` counts as a statement start (a block) only across a line break:
    // on the same line it is far likelier a stray brace on the current
    // statement, and php reports it as the unexpected token (`const X = 1
    // {0};` is `unexpected token "{", expecting "," or ";"`, probed 8.5.7).
    let stmt_starts_here =
        p.newline_before_current() || (STMT_START.contains(&p.current()) && !p.peek_at(K::L_CURLY));
    if stmt_starts_here {
        // The statement already failed at or after this offset: the
        // earlier diagnostic is the story (php's wording is its
        // unexpected-token one: `function echo(){}` reports the
        // parameter list's `expecting "("`, `const return` the name
        // slot's `expecting identifier`). A zero-width repair here would
        // sort ahead of it.
        if p.has_error_at_or_after(span.start) {
            return;
        }
        // The high-confidence repair, worded as php words the missing
        // terminator: the bison parser names the token it choked on, with
        // no `expecting …` clause, since the statement-boundary expected
        // set is large (`$x = []\nFOO` is `unexpected identifier "FOO"`,
        // `$x = 1\n$y` is `unexpected variable "$y"`, a trailing-newline
        // end of input `unexpected end of file`, all pinned 8.5.7).
        let message = if p.at_eof() {
            "syntax error, unexpected end of file".to_owned()
        } else {
            format!(
                "syntax error, unexpected {}",
                php_token_name(p.current(), p.current_text())
            )
        };
        p.error(Diagnostic::new(span, message));
    } else {
        if let Some(expecting) = expecting {
            let d = p.unexpected_here_expecting(expecting);
            p.error(d);
        } else {
            p.error_unexpected();
        }
        recover_to_stmt_boundary(p);
    }
}

/// The missing-comma repair, shared by the argument and element lists: a
/// zero-width insertion after the previous token, worded as php words it.
/// `php_closer` is the list's close token as php spells it: after a
/// complete element php's expected set collapses to the closer alone
/// (`[1 2]` is `unexpected integer "2", expecting "]"`, `foo(1 2)`
/// `expecting ")"`, probed 8.5.7), so the text names the unexpected next
/// token with that clause.
pub(crate) fn missing_comma(p: &mut Parser<'_>, php_closer: &str) {
    let span = after_previous(p);
    let message = format!(
        "syntax error, unexpected {}, expecting {php_closer}",
        php_token_name(p.current(), p.current_text())
    );
    p.error(Diagnostic::new(span, message));
}

/// The level-2 and level-3 recovery: absorb tokens into an `ERROR` node
/// until a statement boundary, always making progress.
pub(crate) fn recover_to_stmt_boundary(p: &mut Parser<'_>) {
    if at_stmt_recovery(p) {
        // The boundary token itself belongs to the next construct,
        // except a stray `;`, which folds into the error region.
        if p.peek_at(K::SEMICOLON) {
            p.bump(K::SEMICOLON);
        }
        return;
    }
    let e = p.start();
    while !at_stmt_recovery(p) {
        p.bump_any();
    }
    e.complete(p, K::ERROR);
    if p.peek_at(K::SEMICOLON) {
        p.bump(K::SEMICOLON);
    }
}

// ---------------------------------------------------------------------------
// Simple statements
// ---------------------------------------------------------------------------

fn expr_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    if expr::expr(p) {
        semicolon(p);
        m.complete(p, K::EXPR_STMT);
    } else {
        // No expression could start here: one token absorbed for
        // progress, then synchronised. A stray closer gets php's
        // bracket-balance wording (`Unmatched '}'`, `Unclosed '{' does
        // not match ')'`) through the parser; a closer that matches its
        // opener but stands where no expression can, the stray-closer
        // wording; anything else the unexpected-token one.
        let d = if p.bracket_closer_message().is_some() {
            p.unexpected_here()
        } else if matches!(p.current(), K::R_PAREN | K::R_BRACK)
            || (p.peek_at(K::R_CURLY) && p.open_delim_span(K::R_CURLY).is_none())
        {
            Diagnostic::new(
                p.current_span(),
                format!(
                    "stray `{}` with no matching opener",
                    String::from_utf8_lossy(p.current_text())
                ),
            )
        } else {
            p.unexpected_here()
        };
        p.error(d);
        m.abandon(p);
        if !at_stmt_recovery(p) {
            let e = p.start();
            p.bump_any();
            e.complete(p, K::ERROR);
        } else if p.peek_at(K::SEMICOLON) {
            p.bump(K::SEMICOLON);
        }
    }
}

fn echo_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    if p.peek_at(K::OPEN_TAG_ECHO) {
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
    if !p.peek_at(K::SEMICOLON) && !p.peek_at(K::CLOSE_TAG) && !p.at_eof() {
        expr::expr(p);
    }
    semicolon(p);
    m.complete(p, K::RETURN_STMT);
}

fn break_continue_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    let kind = if p.peek_at(K::BREAK_KW) {
        p.bump(K::BREAK_KW);
        K::BREAK_STMT
    } else {
        p.bump(K::CONTINUE_KW);
        K::CONTINUE_STMT
    };
    if !p.peek_at(K::SEMICOLON) && !p.peek_at(K::CLOSE_TAG) && !p.at_eof() {
        expr::expr(p);
    }
    semicolon(p);
    m.complete(p, kind);
}

fn goto_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::GOTO_KW);
    if p.at(K::IDENT) {
        p.bump(K::IDENT);
    } else {
        // A `goto` target is a plain label identifier; php spells the
        // clause when it is missing (`goto exit`, `goto 5` and `goto ;`
        // are `expecting identifier`, probed 8.5.7).
        let d = p.unexpected_here_expecting("identifier");
        p.error(d);
    }
    semicolon(p);
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
            p.error_unexpected();
            break;
        }
        if !p.eat(K::COMMA) {
            break;
        }
    }
    // After a `global` variable php expects another `,`-separated simple
    // variable or the terminating `;`. A trailing token that is neither,
    // a `->`, `[` or `::` trailer on a `$$name`
    // (globalNonSimpleVariableError: `global $$foo->bar;`), is
    // `unexpected "->", expecting "," or ";"` (probed 8.5.7). The generic
    // `semicolon` repair path drops the clause, so the set is named here;
    // a plain missing `;` (a statement start or a newline) still takes
    // the repair.
    if p.peek_at(K::SEMICOLON)
        || p.peek_at(K::CLOSE_TAG)
        || p.peek_at(K::INLINE_HTML)
        || STMT_START.contains(&p.current())
        || p.newline_before_current()
    {
        semicolon(p);
    } else {
        let d = p.unexpected_expecting("\",\" or \";\"");
        p.error(d);
        recover_to_stmt_boundary(p);
    }
    m.complete(p, K::GLOBAL_STMT);
}

fn static_var_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::STATIC_KW);
    loop {
        let d = p.start();
        if p.at(K::VARIABLE) {
            p.bump(K::VARIABLE);
            if p.eat(K::EQ) {
                expr::expr(p);
            }
            d.complete(p, K::STATIC_VAR_DECLARATOR);
        } else {
            d.abandon(p);
            p.error_unexpected();
            break;
        }
        if !p.eat(K::COMMA) {
            break;
        }
    }
    semicolon(p);
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
        expect_closer(p, K::R_PAREN, "`(`");
    } else {
        // php's bison clause at the mandatory-paren keywords (probed
        // 8.5.7: `unset` at the end of the input is `… unexpected end of
        // file, expecting "("`).
        let d = p.unexpected_here_expecting("\"(\"");
        p.error(d);
    }
    semicolon(p);
    m.complete(p, K::UNSET_STMT);
}

fn const_stmt(p: &mut Parser<'_>) {
    let m = p.start();
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
        expect_closer(p, K::R_PAREN, "`(`");
    }
    p.eat(K::SEMICOLON);
    if p.peek_at(K::HALT_DATA) {
        p.bump(K::HALT_DATA);
    }
    m.complete(p, K::HALT_COMPILER_STMT);
}

// ---------------------------------------------------------------------------
// Control flow
// ---------------------------------------------------------------------------

fn paren_cond(p: &mut Parser<'_>) {
    if p.eat(K::L_PAREN) {
        expr::expr(p);
        expect_closer(p, K::R_PAREN, "`(`");
    } else {
        p.error_unexpected();
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
        alt_stmt_list(p, ENDERS);
        while p.peek_at(K::ELSEIF_KW) || (p.peek_at(K::ELSE_KW) && p.nth(1) == K::IF_KW) {
            let c = p.start();
            if p.peek_at(K::ELSEIF_KW) {
                p.bump(K::ELSEIF_KW);
            } else {
                // `else if` in the alternative syntax: parsed as an
                // `elseif` clause; the compiler rejects it.
                p.bump(K::ELSE_KW);
                p.bump(K::IF_KW);
            }
            paren_cond(p);
            if !p.eat(K::COLON) {
                p.error_unexpected();
            }
            alt_stmt_list(p, ENDERS);
            c.complete(p, K::ALT_ELSEIF_CLAUSE);
        }
        if p.peek_at(K::ELSE_KW) {
            let c = p.start();
            p.bump(K::ELSE_KW);
            if !p.eat(K::COLON) {
                p.error_unexpected();
            }
            alt_stmt_list(p, ALT_ENDERS);
            c.complete(p, K::ALT_ELSE_CLAUSE);
        }
        alt_terminator(p, K::ENDIF_KW, "endif");
    } else {
        statement(p, false);
        while p.peek_at(K::ELSEIF_KW) {
            let c = p.start();
            p.bump(K::ELSEIF_KW);
            paren_cond(p);
            statement(p, false);
            c.complete(p, K::ELSEIF_CLAUSE);
        }
        if p.peek_at(K::ELSE_KW) {
            let c = p.start();
            p.bump(K::ELSE_KW);
            statement(p, false);
            c.complete(p, K::ELSE_CLAUSE);
        }
    }
    m.complete(p, K::IF_STMT);
}

/// The alternative-syntax terminator family.
const ALT_ENDERS: &[K] = &[
    K::ENDIF_KW,
    K::ENDWHILE_KW,
    K::ENDFOR_KW,
    K::ENDFOREACH_KW,
    K::ENDSWITCH_KW,
    K::ENDDECLARE_KW,
];

/// Consumes the `end…;` of an alternative-syntax construct, with a
/// diagnostic when the terminator is wrong or missing.
fn alt_terminator(p: &mut Parser<'_>, ender: K, name: &str) {
    if p.eat(ender) {
        semicolon(p);
        return;
    }
    let message = format!("expected `{name}` to close the alternative syntax");
    if ALT_ENDERS.contains(&p.current()) {
        // The wrong `end…` keyword: consumed, with its `;`, as the
        // construct's end, the one reading that makes the construct
        // close.
        p.error(Diagnostic::new(p.current_span(), message));
        let e = p.start();
        p.bump_any();
        e.complete(p, K::ERROR);
        p.eat(K::SEMICOLON);
        return;
    }
    // Missing entirely: where it was expected.
    p.error(Diagnostic::new(after_previous(p), message));
    recover_to_stmt_boundary(p);
}

fn while_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::WHILE_KW);
    paren_cond(p);
    if p.eat(K::COLON) {
        alt_stmt_list(p, ALT_ENDERS);
        alt_terminator(p, K::ENDWHILE_KW, "endwhile");
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
        p.error_unexpected();
    }
    semicolon(p);
    m.complete(p, K::DO_WHILE_STMT);
}

fn for_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::FOR_KW);
    if p.eat(K::L_PAREN) {
        let w = p.start();
        if !p.peek_at(K::SEMICOLON) {
            expr::expr_list(p);
        }
        w.complete(p, K::FOR_INIT);
        p.eat(K::SEMICOLON);
        let w = p.start();
        if !p.peek_at(K::SEMICOLON) {
            expr::expr_list(p);
        }
        w.complete(p, K::FOR_COND);
        p.eat(K::SEMICOLON);
        let w = p.start();
        if !p.peek_at(K::R_PAREN) {
            expr::expr_list(p);
        }
        w.complete(p, K::FOR_STEP);
        expect_closer(p, K::R_PAREN, "`(`");
    } else {
        p.error_unexpected();
    }
    if p.eat(K::COLON) {
        alt_stmt_list(p, ALT_ENDERS);
        alt_terminator(p, K::ENDFOR_KW, "endfor");
    } else {
        statement(p, false);
    }
    m.complete(p, K::FOR_STMT);
}

fn foreach_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::FOREACH_KW);
    if p.eat(K::L_PAREN) {
        expr::expr(p);
        if !p.eat(K::AS_KW) {
            p.error_unexpected();
        }
        // `key => target` or just `target`, either by reference. A
        // leading `&` before the first expression is usually the
        // by-reference value target, but php also parses the illegal
        // by-reference key form `&$k => $v`: the grammar shapes that as
        // a `FOREACH_KEY` carrying the `&`, and the compiler rejects it
        // (`Key element cannot be a reference`). A target is a Zend
        // `foreach_variable`: `list(...)` destructures here without an
        // `=` (the one-shot grant).
        if p.peek_at(K::AMP) {
            let t = p.start();
            p.bump(K::AMP);
            p.list_target_ok = true;
            expr::expr(p);
            if p.peek_at(K::FAT_ARROW) {
                // `&$k => …`: what was parsed was the by-reference key,
                // completed as `FOREACH_KEY` with the `&` inside it for
                // the compiler; then the value target, itself optionally
                // by reference.
                t.complete(p, K::FOREACH_KEY);
                p.bump(K::FAT_ARROW);
                let v = p.start();
                p.eat(K::AMP);
                p.list_target_ok = true;
                expr::expr(p);
                v.complete(p, K::FOREACH_TARGET);
            } else {
                t.complete(p, K::FOREACH_TARGET);
            }
        } else {
            let first = p.start();
            p.list_target_ok = true;
            let has_expr = expr::expr(p);
            if has_expr && p.peek_at(K::FAT_ARROW) {
                // What was parsed was the key; `=>` lives inside
                // `ForeachKey`.
                p.bump(K::FAT_ARROW);
                first.complete(p, K::FOREACH_KEY);
                let t = p.start();
                p.eat(K::AMP);
                p.list_target_ok = true;
                expr::expr(p);
                t.complete(p, K::FOREACH_TARGET);
            } else if has_expr {
                first.complete(p, K::FOREACH_TARGET);
            } else {
                // No target expression at all: the schema promises a
                // `ForeachKey` is only ever built around a parsed key.
                first.abandon(p);
                p.error_unexpected();
            }
        }
        expect_closer(p, K::R_PAREN, "`(`");
    } else {
        p.error_unexpected();
    }
    if p.eat(K::COLON) {
        alt_stmt_list(p, ALT_ENDERS);
        alt_terminator(p, K::ENDFOREACH_KW, "endforeach");
    } else {
        statement(p, false);
    }
    m.complete(p, K::FOREACH_STMT);
}

fn switch_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::SWITCH_KW);
    paren_cond(p);
    if p.eat(K::L_CURLY) {
        switch_arms(p, &[K::R_CURLY]);
        expect_closer(p, K::R_CURLY, "`{`");
    } else if p.peek_at(K::COLON) {
        p.bump(K::COLON);
        switch_arms(p, &[K::ENDSWITCH_KW]);
        alt_terminator(p, K::ENDSWITCH_KW, "endswitch");
    } else {
        p.error_unexpected();
    }
    m.complete(p, K::SWITCH_STMT);
}

fn switch_arms(p: &mut Parser<'_>, enders: &[K]) {
    while !p.at_eof() && !enders.contains(&p.current()) {
        if p.peek_at(K::CASE_KW) || p.peek_at(K::DEFAULT_KW) {
            let arm = p.start();
            if p.peek_at(K::CASE_KW) {
                p.bump(K::CASE_KW);
                expr::expr(p);
            } else {
                p.bump(K::DEFAULT_KW);
            }
            // A `;` after `case` or `default` is legal, archaic, and in
            // the corpus.
            if !p.eat(K::COLON) && !p.eat(K::SEMICOLON) {
                p.error_unexpected();
            }
            let body = p.start();
            while !p.at_eof()
                && !p.peek_at(K::CASE_KW)
                && !p.peek_at(K::DEFAULT_KW)
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
            p.error_unexpected();
            let e = p.start();
            p.bump_any();
            e.complete(p, K::ERROR);
        }
    }
}

fn try_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::TRY_KW);
    block(p);
    while p.peek_at(K::CATCH_KW) {
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
            expect_closer(p, K::R_PAREN, "`(`");
        } else {
            p.error_unexpected();
        }
        block(p);
        c.complete(p, K::CATCH_CLAUSE);
    }
    if p.peek_at(K::FINALLY_KW) {
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
    }
    if p.peek_at(K::L_CURLY) {
        block(p);
    } else {
        semicolon(p);
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
    let mixed = !((p.peek_at(K::FUNCTION_KW) || p.peek_at(K::CONST_KW))
        && (ckw::is_name_start(p.nth(1)) || p.nth(1) == K::BACKSLASH));
    if !mixed {
        p.bump_any();
    }
    loop {
        // The group form, `Prefix\{ … }`: the prefix may be fully
        // qualified (`use function \Foo\{…}`, php-src ns_095).
        if (p.peek_at(K::NAME_QUALIFIED)
            || p.peek_at(K::IDENT)
            || p.peek_at(K::NAME_FULLY_QUALIFIED))
            && p.nth(1) == K::BACKSLASH
            && p.nth(2) == K::L_CURLY
        {
            use_group(p, mixed);
        } else {
            let c = p.start();
            if !types::name_ref(p) {
                p.error_unexpected();
                c.abandon(p);
                break;
            }
            use_alias(p);
            c.complete(p, K::USE_CLAUSE);
        }
        if !p.eat(K::COMMA) {
            break;
        }
    }
    semicolon(p);
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
    if p.peek_at(K::R_CURLY) {
        let d = p.unexpected_expecting(item_start_expecting);
        p.error(d);
    }
    let mut parsed_item = false;
    loop {
        if p.peek_at(K::R_CURLY) || p.at_eof() {
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
        if p.peek_at(K::FUNCTION_KW) || p.peek_at(K::CONST_KW) {
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
        // legal on the prefix alone.
        if p.peek_at(K::NAME_FULLY_QUALIFIED) {
            let d = p.unexpected_expecting(item_start_expecting);
            p.error(d);
            // `name_ref` still consumes it: one diagnostic, and the tree
            // keeps its shape.
        }
        if !types::name_ref(p) {
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
        if !p.peek_at(K::R_CURLY) && !p.at_eof() {
            let d = p.unexpected_expecting("\"}\"");
            p.error(d);
            let e = p.start();
            let mut depth = 0u32;
            while !p.at_eof() {
                if p.peek_at(K::R_CURLY) {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                } else if p.peek_at(K::L_CURLY) {
                    depth += 1;
                } else if p.peek_at(K::SEMICOLON) && depth == 0 {
                    break;
                }
                p.bump_any();
            }
            e.complete(p, K::ERROR);
        }
        break;
    }
    expect_closer(p, K::R_CURLY, "`{`");
    g.complete(p, K::USE_GROUP);
}

fn use_alias(p: &mut Parser<'_>) {
    if p.peek_at(K::AS_KW) {
        let a = p.start();
        p.bump(K::AS_KW);
        if p.at(K::IDENT) {
            p.bump(K::IDENT);
            a.complete(p, K::USE_ALIAS);
        } else if ckw::is_member_name(p.current()) {
            p.bump_remap(K::IDENT);
            a.complete(p, K::USE_ALIAS);
        } else {
            // `UseAlias.name` is a promise (`!`): without a name the node
            // may not exist, so the stray `as` becomes an `ERROR`.
            p.error_unexpected();
            a.complete(p, K::ERROR);
        }
    }
}

fn declare_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::DECLARE_KW);
    if p.eat(K::L_PAREN) {
        loop {
            let d = p.start();
            if p.at(K::IDENT) {
                p.bump(K::IDENT);
                if p.eat(K::EQ) {
                    expr::expr(p);
                }
                d.complete(p, K::DECLARE_DIRECTIVE);
            } else {
                d.abandon(p);
                p.error_unexpected();
                break;
            }
            if !p.eat(K::COMMA) {
                break;
            }
        }
        expect_closer(p, K::R_PAREN, "`(`");
    } else {
        p.error_unexpected();
    }
    if p.eat(K::COLON) {
        alt_stmt_list(p, ALT_ENDERS);
        alt_terminator(p, K::ENDDECLARE_KW, "enddeclare");
    } else if p.peek_at(K::L_CURLY) {
        statement(p, false);
    } else {
        semicolon(p);
    }
    m.complete(p, K::DECLARE_STMT);
}
