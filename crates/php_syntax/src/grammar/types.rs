//! Type expressions: nullable, union, intersection and the DNF parens. The
//! shape rules (no nested union, no `?` on a compound) are the compiler's
//! guards (design §6.1); the grammar parses the superset.

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

/// One type expression: `int`, `?T`, `A|B`, `A&B`, `(A&B)|C`.
pub(crate) fn type_expr(p: &mut Parser<'_>) {
    if !p.enter_depth() {
        super::absorb_balanced(p);
        return;
    }
    type_expr_inner(p);
    p.exit_depth();
}

fn type_expr_inner(p: &mut Parser<'_>) {
    if p.peek_at(K::QUESTION) {
        let m = p.start();
        p.bump(K::QUESTION);
        // `?` wraps one type in php; a compound under `?` is parsed and
        // the compiler rejects it.
        union_type(p);
        m.complete(p, K::NULLABLE_TYPE);
        return;
    }
    union_type(p);
}

/// `intersection ('|' intersection)*`: a flat `UnionType` when `|` appears.
fn union_type(p: &mut Parser<'_>) {
    let m = p.start();
    intersection_type(p);
    if p.peek_at(K::PIPE) {
        while p.eat(K::PIPE) {
            intersection_type(p);
        }
        m.complete(p, K::UNION_TYPE);
    } else {
        m.abandon(p);
    }
}

/// `atomic ('&' atomic)*`: a flat `IntersectionType` when `&` appears. An
/// `&` followed by a variable or `...` is a parameter's by-reference or
/// variadic marker, never an intersection: php 8.1's token split, read
/// here as lookahead.
fn intersection_type(p: &mut Parser<'_>) {
    let m = p.start();
    atomic_type(p);
    if p.peek_at(K::AMP) && !matches!(p.nth(1), K::VARIABLE | K::DOT_DOT_DOT) {
        while p.peek_at(K::AMP) && !matches!(p.nth(1), K::VARIABLE | K::DOT_DOT_DOT) {
            p.bump(K::AMP);
            atomic_type(p);
        }
        m.complete(p, K::INTERSECTION_TYPE);
    } else {
        m.abandon(p);
    }
}

/// A named type or a parenthesised (DNF) group.
fn atomic_type(p: &mut Parser<'_>) {
    if p.peek_at(K::L_PAREN) {
        let m = p.start();
        p.bump(K::L_PAREN);
        // php's DNF grammar takes only an intersection inside type parens:
        // a one-member group errors at its `)` with the `&`-token clause
        // (`private (set) mixed $v1;`, where the interior whitespace
        // defeats the asymmetric-visibility glue and `(set)` parses as a
        // type group; php reports `unexpected token ")", expecting token
        // "&"`, probed 8.5.7, gh18026). A wider mis-shape (`(A|B)`) is
        // parsed as the superset for the compiler's shape guard.
        if p.peek_at(K::L_PAREN) || p.peek_at(K::QUESTION) || p.nth(1) != K::R_PAREN {
            union_type(p);
        } else {
            let nm = p.start();
            if name_ref(p) {
                nm.complete(p, K::NAMED_TYPE);
                let d = p.unexpected_expecting("token \"&\"");
                p.error(d);
            } else {
                nm.abandon(p);
                union_type(p);
            }
        }
        super::expect_closer(p, K::R_PAREN, "`(`");
        m.complete(p, K::PAREN_TYPE);
        return;
    }
    let m = p.start();
    if name_ref(p) {
        m.complete(p, K::NAMED_TYPE);
    } else {
        p.error_unexpected();
        m.abandon(p);
    }
}
