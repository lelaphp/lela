//! Type expressions: nullable, union, intersection and the DNF parens. The
//! shape rules php's parser holds (`(A&B)` only inside a union, `&` and
//! `?` never before a group, only `&` inside one, `?` over one name and
//! nothing compound, `static` only where [`TypeSite::WithStatic`]) are
//! parse errors here with php's text; what php's compiler holds (a
//! nested union, a redundant member) is the compiler's guard (design
//! §6.1).

use crate::SyntaxKind as K;
use crate::parser::Parser;

use super::ckw;

/// A name token usable as a type or class reference: `IDENT`, a qualified
/// name, or a remappable contextual keyword (`array`, `static` and the
/// contextual-only set arrive as keywords). Consumes one token; `false`,
/// consuming nothing, when the current token cannot be a name.
pub(crate) fn name_ref(p: &mut Parser<'_>) -> bool {
    match p.current() {
        K::IDENT | K::NAME_QUALIFIED | K::NAME_FULLY_QUALIFIED | K::NAME_RELATIVE => {
            p.bump_any();
            true
        }
        // Type-position keywords that are names in the tree: `array`,
        // `static` (a return type) and the contextual-only set.
        K::ARRAY_KW | K::STATIC_KW => {
            p.bump_remap(K::IDENT);
            true
        }
        kind if ckw::is_contextual_only(kind) => {
            p.bump_remap(K::IDENT);
            true
        }
        _ => false,
    }
}

/// Can the current position start a type?
pub(crate) fn at_type_start(p: &mut Parser<'_>) -> bool {
    match p.current() {
        K::QUESTION
        | K::L_PAREN
        | K::IDENT
        | K::NAME_QUALIFIED
        | K::NAME_FULLY_QUALIFIED
        | K::NAME_RELATIVE
        | K::ARRAY_KW
        | K::STATIC_KW => true,
        kind => ckw::is_contextual_only(kind),
    }
}

/// Which of php's two type rules a position takes: `type_expr`, where
/// `static` is a type (a return type, a class constant's type, an enum's
/// backing), or `type_expr_without_static` (a parameter's and a
/// property's), where it is no name and so the unexpected token
/// (`public ?static $x;`, `function f(int|static $x)`, probed 8.5.9).
///
/// ```php
/// class Test { public ?static $foo; }
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected token "static" in %s on line 2
/// ```
///
/// ```php
/// class Test { function f(): static|array { return $this; } }
/// var_dump((new Test)->f());
/// ```
/// ```expect
/// object(Test)#1 (0) {
/// }
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TypeSite {
    WithStatic,
    WithoutStatic,
}

/// One type expression: `int`, `?T`, `A|B`, `A&B`, `(A&B)|C`.
///
/// ```php
/// function foo(): ?Countable&Iterator {}
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected token "&", expecting "{" in %s on line 2
/// ```
///
/// ```php
/// class T { public ?int|string $x; }
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected token "|", expecting variable in %s on line 2
/// ```
pub(crate) fn type_expr(p: &mut Parser<'_>, site: TypeSite) {
    if !p.enter_depth() {
        super::absorb_balanced(p);
        return;
    }
    type_expr_inner(p, site);
    p.exit_depth();
}

fn type_expr_inner(p: &mut Parser<'_>, site: TypeSite) {
    if p.at(K::QUESTION) {
        let m = p.start();
        p.bump(K::QUESTION);
        // `'?' type`: one name, never a group (`?(A&B)`) and never a
        // compound, whose `|` or `&` is then what the position after
        // the type finds unexpected (`?A&B $x` is `unexpected token
        // "&", expecting variable`, probed 8.5.9).
        named_type(p, site);
        m.complete(p, K::NULLABLE_TYPE);
        return;
    }
    union_type(p, site);
}

/// `intersection ('|' intersection)*`: a flat `UnionType` when `|` appears.
/// A DNF group stands only inside a union, so after one that no `|`
/// precedes or follows php wants the `|` (`(A&B) $x` is `unexpected
/// variable "$x", expecting "|"`, probed 8.5.9).
fn union_type(p: &mut Parser<'_>, site: TypeSite) {
    let m = p.start();
    let grouped = intersection_type(p, site);
    if p.at(K::PIPE) {
        while p.eat(K::PIPE) {
            intersection_type(p, site);
        }
        m.complete(p, K::UNION_TYPE);
    } else {
        if grouped && !p.has_error_at_or_after(p.current_span().start) {
            let d = p.unexpected_here_expecting("\"|\"");
            p.error(d);
        }
        m.abandon(p);
    }
}

/// `atomic ('&' atomic)*`: a flat `IntersectionType` when `&` appears. An
/// `&` followed by a variable or `...` is a parameter's by-reference or
/// variadic marker, never an intersection: php 8.1's token split, read
/// here as lookahead. A group after `&` is the unexpected token
/// (`A&(B&C)`, probed 8.5.9). Whether what was parsed is one DNF group.
fn intersection_type(p: &mut Parser<'_>, site: TypeSite) -> bool {
    let m = p.start();
    let grouped = p.at(K::L_PAREN);
    atomic_type(p, site);
    if grouped && p.at(K::AMP) {
        // A group joins a union alone (`(A&B)&C` is `unexpected token
        // "&", expecting "|"`, probed 8.5.9).
        let d = p.unexpected_here_expecting("\"|\"");
        p.error(d);
    }
    if p.at(K::AMP) && !matches!(p.nth(1), K::VARIABLE | K::DOT_DOT_DOT) {
        while p.at(K::AMP) && !matches!(p.nth(1), K::VARIABLE | K::DOT_DOT_DOT) {
            p.bump(K::AMP);
            if p.at(K::L_PAREN) {
                p.error_unexpected();
            }
            atomic_type(p, site);
        }
        m.complete(p, K::INTERSECTION_TYPE);
        return false;
    }
    m.abandon(p);
    grouped
}

/// A named type or a parenthesised (DNF) group, which php's grammar
/// takes as `name (& name)+` and nothing else: inside the parens any
/// token but `&` after a name is unexpected with the `&`-token clause
/// (`(A)`, `(A|B)`; `private (set) mixed $v1;`, where the interior
/// whitespace defeats the asymmetric-visibility glue and `(set)` parses
/// as a type group, probed 8.5.7, gh18026), and at the closer the clause
/// is `token "&"` after one member and `token "&" or ")"` after more
/// (probed 8.5.9). The group's own `(` and `)` are on the parser's
/// delimiter stack, so the balance text stands where it applies.
fn atomic_type(p: &mut Parser<'_>, site: TypeSite) {
    if p.at(K::L_PAREN) {
        let m = p.start();
        p.bump(K::L_PAREN);
        let group = p.start();
        let mut members = 0;
        loop {
            let nm = p.start();
            refuse_static(p, site);
            if name_ref(p) {
                nm.complete(p, K::NAMED_TYPE);
                members += 1;
            } else {
                nm.abandon(p);
                p.error_unexpected();
                break;
            }
            if p.at(K::AMP) {
                p.bump(K::AMP);
                continue;
            }
            if !p.at(K::R_PAREN) && !p.at_eof() && p.bracket_closer_message().is_none() {
                let d = if members > 1 {
                    p.unexpected_expecting("token \"&\" or \")\"")
                } else {
                    p.unexpected_expecting("token \"&\"")
                };
                p.error(d);
            }
            break;
        }
        if members > 1 {
            group.complete(p, K::INTERSECTION_TYPE);
        } else {
            group.abandon(p);
            if members == 1 && p.at(K::R_PAREN) {
                let d = p.unexpected_expecting("token \"&\"");
                p.error(d);
            }
        }
        let closer = if members > 1 {
            "token \"&\" or \")\""
        } else {
            "token \"&\""
        };
        super::expect_closer(p, K::R_PAREN, Some(closer));
        m.complete(p, K::PAREN_TYPE);
        return;
    }
    named_type(p, site);
}

/// One name as a type, `static` among them where the site takes it.
fn named_type(p: &mut Parser<'_>, site: TypeSite) {
    let m = p.start();
    refuse_static(p, site);
    if name_ref(p) {
        m.complete(p, K::NAMED_TYPE);
    } else {
        p.error_unexpected();
        m.abandon(p);
    }
}

/// `static` where the rule is `type_without_static`: the unexpected
/// token, left for [`name_ref`] to take so the tree keeps its shape.
fn refuse_static(p: &mut Parser<'_>, site: TypeSite) {
    if site == TypeSite::WithoutStatic && p.at(K::STATIC_KW) {
        p.error_unexpected();
    }
}
