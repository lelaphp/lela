//! String interpolation (design §4.9): the lexer has already emitted the
//! structured sub-token runs of a double-quoted string, a heredoc and a
//! backtick; this shapes them into the schema's `StringPart` nodes.

use crate::SyntaxKind as K;
use crate::parser::{CompletedMarker, Parser};

use super::expr;

/// `"…"` and `` `…` ``: delimiter, parts, delimiter.
pub(crate) fn interpolated(p: &mut Parser<'_>, delim: K, node: K) -> CompletedMarker {
    let m = p.start();
    p.bump(delim);
    parts(p, &[delim]);
    // Unterminated: the lexer reported it; the node closes here.
    p.eat(delim);
    m.complete(p, node)
}

/// A heredoc or a nowdoc, told apart by the opener's quoting (`<<<'X'` is
/// a nowdoc, whose body is one stretch of text).
pub(crate) fn heredoc(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    let nowdoc = p.current_text().contains(&b'\'');
    p.bump(K::HEREDOC_START);
    if nowdoc {
        // An indented nowdoc carries one STRING_TEXT per body line, the
        // indentation between them not significant: consume the run.
        while p.peek_at(K::STRING_TEXT) {
            p.bump(K::STRING_TEXT);
        }
        p.eat(K::HEREDOC_END);
        m.complete(p, K::NOWDOC_EXPR)
    } else {
        parts(p, &[K::HEREDOC_END]);
        p.eat(K::HEREDOC_END);
        m.complete(p, K::HEREDOC_EXPR)
    }
}

/// `StringPart*` until one of `enders`, or the end of the input (an
/// unterminated string is the lexer's diagnostic).
fn parts(p: &mut Parser<'_>, enders: &[K]) {
    while !p.at_eof() && !enders.contains(&p.current()) {
        match p.current() {
            K::STRING_TEXT => {
                let m = p.start();
                p.bump(K::STRING_TEXT);
                m.complete(p, K::LITERAL_STRING_PART);
            }
            K::VARIABLE => simple_interpolation(p),
            K::L_CURLY => {
                // `{$…}`: the lexer guarantees `{$`, and the interior is
                // the full scripting grammar.
                let m = p.start();
                p.bump(K::L_CURLY);
                if !expr::expr(p) {
                    p.error_unexpected();
                }
                super::expect_closer(p, K::R_CURLY, "`{`");
                m.complete(p, K::BRACED_INTERPOLATION);
            }
            K::DOLLAR_L_CURLY => dollar_brace(p),
            _ => {
                // Anything else inside a string run is a lexer defect;
                // absorbed for progress, losslessly.
                let e = p.start();
                p.bump_any();
                e.complete(p, K::ERROR);
            }
        }
    }
}

/// `$name`, `$name[dim]`, `$name->prop`: the restricted in-string forms.
fn simple_interpolation(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::VARIABLE);
    if p.peek_at(K::L_BRACK) {
        simple_dim(p);
    } else if matches!(p.current(), K::ARROW | K::NULLSAFE_ARROW) && p.nth(1) == K::IDENT {
        let s = p.start();
        p.bump_any();
        p.bump(K::IDENT);
        s.complete(p, K::SIMPLE_PROP);
    }
    m.complete(p, K::SIMPLE_INTERPOLATION);
}

/// The in-string offset grammar, `[ident]`, `[$var]`, `[123]`, `[-123]`,
/// is restricted on purpose, and php's bison prints a fixed clause at the
/// offset position: `expecting "-" or identifier or variable or number`
/// (`"$arr['foo']"`, Bug #21820).
const OFFSET_EXPECTED: &str = "\"-\" or identifier or variable or number";

fn simple_dim(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::L_BRACK);
    match p.current() {
        // INT beside ENCAPS_INT: inside `${name[…]}` the offset travels
        // through scripting mode, so a plain INT arrives.
        K::IDENT | K::VARIABLE | K::ENCAPS_INT | K::INT => p.bump_any(),
        K::MINUS => {
            p.bump(K::MINUS);
            if p.at(K::ENCAPS_INT) || p.at(K::INT) {
                p.bump_any();
            } else {
                let d = p.unexpected_here_expecting(OFFSET_EXPECTED);
                p.error(d);
            }
        }
        _ => {
            let d = p.unexpected_here_expecting(OFFSET_EXPECTED);
            p.error(d);
        }
    }
    if !p.eat(K::R_BRACK) {
        p.error_unexpected();
    }
    m.complete(p, K::SIMPLE_DIM);
}

/// `${name}`, `${name[dim]}`, `${expr}`: deprecated since 8.2, which the
/// compiler says on the node.
fn dollar_brace(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::DOLLAR_L_CURLY);
    if p.peek_at(K::IDENT) && matches!(p.nth(1), K::R_CURLY | K::L_BRACK) {
        p.bump(K::IDENT);
        if p.peek_at(K::L_BRACK) {
            simple_dim(p);
        }
    } else if !expr::expr(p) {
        p.error_unexpected();
    }
    super::expect_closer(p, K::R_CURLY, "`${`");
    m.complete(p, K::DOLLAR_BRACE_INTERPOLATION);
}
