//! String interpolation (design §4.9): the lexer has already emitted the
//! structured sub-token runs of a double-quoted string, a heredoc and a
//! backtick; this shapes them into the schema's `StringPart` nodes.

use crate::SyntaxKind as K;
use crate::parser::{CompletedMarker, Parser};
use crate::source::Diagnostic;

use super::expr;

/// `"…"` and `` `…` ``: delimiter, parts, delimiter. An unclosed string
/// runs to the end of the input, where php's parser, not its scanner,
/// says so: after text a double-quoted string can go on only with an
/// interpolation (`expecting variable or "${" or "{$"`), after nothing
/// with text too, after an interpolation with too much to spell; a
/// backtick expects its closer alone (probed 8.5.9).
///
/// ```php
/// echo "abc
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected end of file, expecting variable or "${" or "{$" in %s on line 2
/// ```
pub(crate) fn interpolated(p: &mut Parser<'_>, delim: K, node: K) -> CompletedMarker {
    let m = p.start();
    p.bump(delim);
    let last = parts(p, &[delim]);
    if !p.eat(delim) {
        let clause = if delim == K::BACKQUOTE {
            Some("\"`\"")
        } else {
            match last {
                Parts::None => Some("variable or string content or \"${\" or \"{$\""),
                Parts::TextOnly => Some("variable or \"${\" or \"{$\""),
                Parts::Interpolated => None,
            }
        };
        let d = p.unexpected_here_maybe_expecting(clause);
        p.error(d);
    }
    m.complete(p, node)
}

/// What a string's parts amounted to, for the clause an unclosed one
/// carries: php's `encaps_list` wants an interpolation after a lone
/// text, and once one has been seen the closer may follow too, which
/// makes the set too large to spell (`"abc` against `"abc $x` and
/// `"abc {$x} def`, probed 8.5.9).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Parts {
    None,
    TextOnly,
    Interpolated,
}

/// A heredoc or a nowdoc, told apart by the opener's quoting (`<<<'X'` is
/// a nowdoc, whose body is one stretch of text).
pub(crate) fn heredoc(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    let nowdoc = p.current_text().contains(&b'\'');
    p.bump(K::HEREDOC_START);
    let parts = if nowdoc {
        // An indented nowdoc carries one STRING_TEXT per body line, the
        // indentation between them not significant: consume the run.
        let mut seen = Parts::None;
        while p.at(K::STRING_TEXT) {
            p.bump(K::STRING_TEXT);
            seen = Parts::TextOnly;
        }
        seen
    } else {
        parts(p, &[K::HEREDOC_END])
    };
    if !p.eat(K::HEREDOC_END) {
        // As for a double-quoted string, with the heredoc's own closer in
        // the set, a nowdoc's body being the same token to php (probed
        // 8.5.7, flexible-heredoc-error6/7: a body of zero bytes reports
        // the bare `unexpected end of file`).
        let clause = match parts {
            Parts::TextOnly => Some("variable or heredoc end or \"${\" or \"{$\""),
            Parts::None | Parts::Interpolated => None,
        };
        let d = p.unexpected_here_maybe_expecting(clause);
        p.error(d);
    }
    m.complete(
        p,
        if nowdoc {
            K::NOWDOC_EXPR
        } else {
            K::HEREDOC_EXPR
        },
    )
}

/// `StringPart*` until one of `enders`, or the end of the input; what
/// they amounted to, for the clause an unclosed string carries.
fn parts(p: &mut Parser<'_>, enders: &[K]) -> Parts {
    let mut seen = Parts::None;
    while !p.at_eof() && !enders.contains(&p.current()) {
        seen = match (seen, p.current()) {
            (Parts::None, K::STRING_TEXT) => Parts::TextOnly,
            _ => Parts::Interpolated,
        };
        match p.current() {
            K::STRING_TEXT => {
                let m = p.start();
                p.bump(K::STRING_TEXT);
                m.complete(p, K::LITERAL_STRING_PART);
            }
            K::VARIABLE => simple_interpolation(p),
            K::L_CURLY => {
                // `{$…}`: the lexer guarantees `{$`, and the interior is
                // the full scripting grammar, of which php takes a
                // `variable` production: at the closer the clause is the
                // dereference set (`"{$a ;` is `expecting "->" or "?->"
                // or "["`, probed 8.5.9).
                let m = p.start();
                p.bump(K::L_CURLY);
                if !expr::expr(p) {
                    p.error_unexpected();
                }
                super::expect_closer(p, K::R_CURLY, Some(expr::AFTER_VARIABLE));
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
    seen
}

/// `$name`, `$name[dim]`, `$name->prop`: the restricted in-string forms.
fn simple_interpolation(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::VARIABLE);
    if p.at(K::L_BRACK) {
        simple_dim(p, false);
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

/// `in_scripting` says the offset came through the scripting state, inside
/// `${name[…]}`, where php's parser takes a whole expression and spells
/// no clause at the `]`; inside `"$a[…]"` the scanner's `ST_VAR_OFFSET`
/// enters no bracket nesting, so an unclosed `[` there is the parser's
/// `unexpected end of file, expecting …`, never `Unclosed '['` (probed
/// 8.5.9).
fn simple_dim(p: &mut Parser<'_>, in_scripting: bool) {
    let m = p.start();
    if in_scripting {
        p.bump(K::L_BRACK);
    } else {
        p.bump_unnested(K::L_BRACK);
    }
    match p.current() {
        // INT beside ENCAPS_INT: inside `${name[…]}` the offset travels
        // through scripting mode, so a plain INT arrives.
        K::IDENT | K::VARIABLE | K::ENCAPS_INT | K::INT => p.bump_any(),
        K::MINUS => {
            p.bump(K::MINUS);
            if p.at(K::ENCAPS_INT) || p.at(K::INT) {
                p.bump_any();
            } else {
                // After the `-` only a number can follow (`"$a[-"`
                // is `expecting number`, probed 8.5.9).
                let d = offset_unexpected(p, in_scripting, "number");
                p.error(d);
            }
        }
        _ => {
            let d = offset_unexpected(p, in_scripting, OFFSET_EXPECTED);
            p.error(d);
        }
    }
    if p.at(K::R_BRACK) {
        if in_scripting {
            p.bump(K::R_BRACK);
        } else {
            p.bump_unnested(K::R_BRACK);
        }
    } else if !p.has_error_at_or_after(p.current_span().start) {
        // After the offset only `]` can follow (`"$a[0 "` is
        // `expecting "]"`, probed 8.5.9).
        let d = if in_scripting {
            p.unexpected_here()
        } else {
            offset_unexpected(p, in_scripting, "\"]\"")
        };
        p.error(d);
    }
    m.complete(p, K::SIMPLE_DIM);
}

/// The unexpected token inside a string's `$a[…]`, where a stray `]` is
/// the token and not a balance error, php's `ST_VAR_OFFSET` tracking no
/// nesting (`"$a[]"` is `unexpected token "]", expecting …`, probed
/// 8.5.9); inside `${name[…]}` the balance text stands.
fn offset_unexpected(p: &mut Parser<'_>, in_scripting: bool, expecting: &str) -> Diagnostic {
    if in_scripting || p.at_eof() {
        p.unexpected_here_expecting(expecting)
    } else {
        p.unexpected_expecting(expecting)
    }
}

/// `${name}`, `${name[dim]}`, `${expr}`: deprecated since 8.2, which the
/// compiler says on the node.
fn dollar_brace(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::DOLLAR_L_CURLY);
    if p.at(K::IDENT) && matches!(p.nth(1), K::R_CURLY | K::L_BRACK) {
        p.bump(K::IDENT);
        if p.at(K::L_BRACK) {
            simple_dim(p, true);
        }
    } else if !expr::expr(p) {
        p.error_unexpected();
    }
    super::expect_closer(p, K::R_CURLY, None);
    m.complete(p, K::DOLLAR_BRACE_INTERPOLATION);
}
