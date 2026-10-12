//! Expressions (design §4.9): a Pratt core transcribing php 8.5's
//! precedence table, the normative source being `zend_language_parser.y`,
//! with a grammar function per special form.

use crate::SyntaxKind as K;
use crate::parser::{CompletedMarker, Marker, Parser};
use crate::source::{Diagnostic, Level};

use super::{ckw, decl, strings};

/// The Pratt left operand: a completed node, or a still-open marker over a
/// bare token (a name in expression position has no node of its own,
/// schema convention A1, so its marker stays open for wrapping). Two
/// flagged variants carry grammar facts the node kind alone cannot: php's
/// `new`-dereference and `variable`-production rules key on them.
enum Lhs {
    Done(CompletedMarker),
    Bare(Marker, K),
    /// A `new` expression with no constructor argument list and no
    /// anonymous class: not postfix-dereferenceable (`new A->p` is a parse
    /// error in php; `new A()->p` is fine, the 8.4 rule).
    NewBare(CompletedMarker),
    /// A scoped access whose selector is a constant, `class` or a braced
    /// fetch (`A::C`, `A::class`, `A::{expr}`): unlike `A::$p`, not one of
    /// Zend's `variable` productions, so no assignment, by-reference or
    /// unset target (`A::C = 1` is a parse error in php).
    ScopedConst(CompletedMarker),
}

impl Lhs {
    fn kind(&self) -> K {
        match self {
            Lhs::Done(cm) | Lhs::NewBare(cm) | Lhs::ScopedConst(cm) => cm.kind,
            Lhs::Bare(_, kind) => *kind,
        }
    }

    /// A marker positioned before this operand, ready to become its wrapper.
    fn wrap(self, p: &mut Parser<'_>) -> Marker {
        match self {
            Lhs::Done(cm) | Lhs::NewBare(cm) | Lhs::ScopedConst(cm) => cm.precede(p),
            Lhs::Bare(m, _) => m,
        }
    }

    fn finish(self, p: &mut Parser<'_>) {
        if let Lhs::Bare(m, _) = self {
            m.abandon(p);
        }
    }
}

/// One expression; `false`, emitting nothing, when no expression can start
/// here.
pub(crate) fn expr(p: &mut Parser<'_>) -> bool {
    match expr_bp(p, 0) {
        Some(lhs) => {
            lhs.finish(p);
            true
        }
        None => false,
    }
}

/// `Expr,+`: comma-separated expressions (`echo`, a `for` header).
pub(crate) fn expr_list(p: &mut Parser<'_>) {
    loop {
        if !expr(p) {
            p.error_unexpected();
            break;
        }
        if !p.eat(K::COMMA) {
            break;
        }
    }
}

/// `Expr,+` with an optional trailing comma before `closer`: the call-like
/// construct lists (`unset(…)`, `isset(…)`) take one since php 7.3.
pub(crate) fn expr_list_trailing(p: &mut Parser<'_>, closer: K) {
    let mut after_comma = false;
    loop {
        if !expr(p) {
            // After a comma php's clause collapses to the closer
            // (`isset($x, ;` is `expecting ")"`, probed 8.5.9).
            let d = if after_comma {
                p.unexpected_here_expecting(php_list_closer(closer))
            } else {
                p.unexpected_here()
            };
            p.error(d);
            break;
        }
        if !p.eat(K::COMMA) || p.at(closer) {
            break;
        }
        after_comma = true;
    }
}

// ---------------------------------------------------------------------------
// Binding powers: left-associative tiers are (n, n+1), right (n+1, n)
// ---------------------------------------------------------------------------

/// The Pratt binding powers an infix or postfix operator reports: `left`
/// and `right` are the left and right binding powers, and `non_assoc` is
/// `Some(tier)` for the `%nonassoc` families that reject same-tier
/// chaining (`1 == 2 == 3` is an error).
struct BindingPower {
    left: u8,
    right: u8,
    non_assoc: Option<u8>,
}

/// The infix or postfix [`BindingPower`] of `kind`.
fn infix_bp(kind: K) -> Option<BindingPower> {
    // The whole assignment family shares one tier, right-associative,
    // just above the yield and print tier; `is_assign_op` is the one
    // source for the operator list.
    if is_assign_op(kind) {
        return Some(BindingPower {
            left: 14,
            right: 13,
            non_assoc: None,
        });
    }
    let (left, right, non_assoc) = match kind {
        K::OR_KW => (1, 2, None),
        K::XOR_KW => (3, 4, None),
        K::AND_KW => (5, 6, None),
        K::QUESTION => (15, 16, None), // the ternary, special-cased
        K::QUESTION_QUESTION => (20, 19, None), // right
        K::PIPE_PIPE => (21, 22, None),
        K::AMP_AMP => (23, 24, None),
        K::PIPE => (25, 26, None),
        K::CARET => (27, 28, None),
        K::AMP => (29, 30, None),
        K::EQ_EQ | K::BANG_EQ | K::EQ_EQ_EQ | K::BANG_EQ_EQ | K::LT_GT | K::SPACESHIP => {
            (31, 32, Some(31))
        }
        K::LT | K::LT_EQ | K::GT | K::GT_EQ => (33, 34, Some(33)),
        // `|>` (8.5): left-associative, tighter than comparison, bitwise
        // and logical, looser than concatenation, shifts and arithmetic:
        // between `<` and `.` (probed 8.5.7 through the assert-rerender
        // paren drop: pipe_operator/ast, prec_005, precedence_comparison).
        // The concatenation block and everything tighter sit two up to
        // open this tier.
        K::PIPE_GT => (35, 36, None),
        K::DOT => (37, 38, None), // below the shifts since 8.0
        K::SHL | K::SHR => (39, 40, None),
        K::PLUS | K::MINUS => (41, 42, None),
        K::STAR | K::SLASH | K::PERCENT => (43, 44, None),
        // `%nonassoc T_INSTANCEOF` in php's table, yet its right operand
        // is a `class_name_reference`, not an `expr`, so the tier never
        // meets itself: `$x instanceof A instanceof B` is
        // `($x instanceof A) instanceof B` (probed 8.5.9).
        K::INSTANCEOF_KW => (47, 48, None),
        K::STAR_STAR => (53, 52, None), // right; tighter than unary
        _ => return None,
    };
    Some(BindingPower {
        left,
        right,
        non_assoc,
    })
}

// The prefix operators' right binding powers. `instanceof` sits above
// `!`, which sits above `* / %`: `!$a * $b` is `(!$a) * $b`, so `!`'s
// right binding power (45) is above `*`'s left (43) and below
// `instanceof`'s (47). `-$a ** $b` is `-($a ** $b)`: the unary right
// binding power (49) is below `**`'s left (53). `!$x instanceof Y` is
// `!($x instanceof Y)`: `instanceof`'s left (47) is at least 45, so it
// folds into the `!` operand.
const BP_NOT: u8 = 45;
/// `++ -- ~ (cast) @ - +`.
const BP_UNARY: u8 = 49;
/// The `clone` and `new` tier, above `**`.
const BP_CLONE: u8 = 55;
const BP_PRINT: u8 = 9;
const BP_YIELD: u8 = 11;
const BP_INCLUDE: u8 = 9;
const BP_THROW: u8 = 11;
/// Postfix `++` and `--`.
const BP_POSTFIX: u8 = 57;
/// Call, subscript and member trailers bind tightest.
const BP_TRAILER: u8 = 61;

/// The php 8.5 `clone` disambiguation. `clone` is a prefix keyword; the
/// with-properties call form (`clone($obj, [...])`) is taken only when the
/// parenthesised group directly after `clone` holds a top-level comma, so a
/// single parenthesised operand is the prefix form and trailers fold into
/// the operand. The call form is also taken for a leading `...`, the
/// first-class callable `clone(...)` and the spread `clone(...$args)`,
/// neither of which can begin a prefix operand. Assumes the current token
/// is `(`; scans the balanced group without consuming.
fn clone_call_form(p: &mut Parser<'_>) -> bool {
    // A `...` immediately after `(` is the callable or spread call form.
    if p.nth(1) == K::DOT_DOT_DOT {
        return true;
    }
    // An immediately empty group, `clone()`, is the zero-argument call
    // form: php parses it and raises the catchable ArgumentCountError
    // when it serves; the prefix form would reject it at parse time.
    if p.nth(1) == K::R_PAREN {
        return true;
    }
    // A single named argument, `clone(object: $x)`, is the call form too
    // (a bare `name :` at the argument start, `::` excluded). A multi-
    // argument named call carries a top-level comma the scan below finds.
    if ckw::is_member_name(p.nth(1)) && p.nth(2) == K::COLON && p.nth(3) != K::COLON {
        return true;
    }
    let mut depth: i32 = 0;
    let mut n = 0usize;
    loop {
        match p.nth(n) {
            K::L_PAREN | K::L_BRACK | K::L_CURLY => depth += 1,
            K::R_PAREN | K::R_BRACK | K::R_CURLY => {
                depth -= 1;
                if depth == 0 {
                    return false;
                }
            }
            K::COMMA if depth == 1 => return true,
            K::EOF => return false,
            _ => {}
        }
        n += 1;
    }
}

fn expr_bp(p: &mut Parser<'_>, min_bp: u8) -> Option<Lhs> {
    if !p.enter_depth() {
        // The flat-consumed ERROR node stands in as the operand so the
        // enclosing construct completes without a cascade.
        return Some(Lhs::Done(super::absorb_balanced(p)));
    }
    let lhs = expr_bp_inner(p, min_bp);
    p.exit_depth();
    lhs
}

#[allow(clippy::too_many_lines)] // one block per operator family; splitting hurts flow
fn expr_bp_inner(p: &mut Parser<'_>, min_bp: u8) -> Option<Lhs> {
    let mut lhs = prefix(p)?;
    // The tier of the binary node built by the previous iteration of this
    // loop (non-associative chaining can only involve the immediately
    // preceding wrap; anything parenthesised or nested arrives as a
    // different operand).
    let mut prev_tier: Option<u8> = None;

    loop {
        if p.is_stuck() {
            break;
        }
        let kind = p.current();

        // Postfix `++` and `--`.
        if matches!(kind, K::PLUS_PLUS | K::MINUS_MINUS) && BP_POSTFIX >= min_bp {
            let m = lhs.wrap(p);
            p.bump(kind);
            lhs = Lhs::Done(m.complete(p, K::POSTFIX_INC_DEC_EXPR));
            prev_tier = None;
            continue;
        }

        // Trailers: calls, subscripts, member and scoped access.
        if BP_TRAILER >= min_bp
            && let Some(trailer) = at_trailer(p, &lhs)
        {
            lhs = apply_trailer(p, lhs, trailer);
            prev_tier = None;
            continue;
        }

        let Some(BindingPower {
            left: lbp,
            right: rbp,
            non_assoc,
        }) = infix_bp(kind)
        else {
            break;
        };
        // Zend's assignment is the production `variable OP= expr`, not a
        // precedence level, with two consequences, both php's: (a)
        // whenever the operand just parsed is a variable production, an
        // assignment operator binds it whatever the surrounding
        // operator's precedence (`$a > $b[] = $y` parses in php as
        // `$a > ($b[] = $y)`, likewise through `&&`, ternary arms and
        // comparisons); (b) an assignment operator after an operand that
        // is not a variable never binds at all (`A::C = 1`, `new A() = 1`
        // and `'s' = 1` are parse errors at the operator). Destructuring
        // is the one carve-out: `[..]` and `list(..)` take a plain `=`
        // only.
        if is_assign_op(kind) {
            let destructuring = kind == K::EQ && matches!(lhs.kind(), K::ARRAY_EXPR | K::LIST_EXPR);
            if !is_variable_production(&lhs) && !destructuring {
                // Refuse to bind; the enclosing construct reports the
                // dangling operator, at the operator, as php does.
                break;
            }
        } else if lbp < min_bp {
            break;
        }

        // The ternary: `cond ? then? : else`.
        if kind == K::QUESTION {
            let m = lhs.wrap(p);
            p.bump(K::QUESTION);
            if !p.at(K::COLON) {
                // The full form: the middle is a complete expression.
                if let Some(then) = expr_bp(p, 0) {
                    then.finish(p);
                }
            }
            if !p.eat(K::COLON) {
                p.error_unexpected();
            }
            if let Some(els) = expr_bp(p, rbp) {
                els.finish(p);
            } else {
                p.error_unexpected();
            }
            lhs = Lhs::Done(m.complete(p, K::TERNARY_EXPR));
            prev_tier = None;
            continue;
        }

        // A non-associative tier rejects chaining at the same level, as
        // php's `%nonassoc` does (`1 == 2 == 3` is a parse error).
        if let Some(tier) = non_assoc
            && prev_tier == Some(tier)
        {
            // The tree is built anyway: recovery keeps the shape useful.
            p.error_unexpected();
        }

        // `instanceof` takes php's `class_name_reference` on the right,
        // the grammar `new` reads, not a tight expression. `is_assign_op`
        // is the one authority for the assignment family.
        let node = if kind == K::INSTANCEOF_KW {
            K::INSTANCEOF_EXPR
        } else if is_assign_op(kind) {
            K::ASSIGN_EXPR
        } else {
            K::BINARY_EXPR
        };

        let m = lhs.wrap(p);
        p.bump(kind);
        let mut took_ref = false;
        if node == K::ASSIGN_EXPR {
            // `$a = &$b`, the reference assignment: `&` is legal with `=`
            // alone, and the compound cases are the compiler's.
            took_ref = p.eat(K::AMP);
        }
        if took_ref && kind == K::EQ {
            // Zend's `variable '=' ampersand variable`: the `&` takes
            // exactly one variable production. Trailers extend it; a
            // binary operator after a complete variable belongs to the
            // enclosing expression (`$a = &$b + 1` is `($a = &$b) + 1` in
            // php); an operand that is not a variable (`&A::C`, `&new A`)
            // is a parse error at the first token that fails to extend it.
            ref_target(p);
        } else if node == K::INSTANCEOF_EXPR {
            // `$x instanceof;` and `$x instanceof 1;` are the bare
            // unexpected token in php, no `expecting` clause (probed
            // 8.5.9).
            if !class_name_reference(p) {
                p.error_unexpected();
            }
        } else if let Some(rhs) = expr_bp(p, rbp) {
            rhs.finish(p);
        } else {
            p.error_unexpected();
        }
        lhs = Lhs::Done(m.complete(p, node));
        prev_tier = non_assoc;
    }

    Some(lhs)
}

// ---------------------------------------------------------------------------
// Prefix forms and primaries
// ---------------------------------------------------------------------------

/// `static function (){}` and `static fn () =>`: consume `static`, then
/// the closure or the arrow function. `m` already covers any leading
/// attributes, or is the freshly opened marker in the bare case.
fn static_closure_or_arrow(p: &mut Parser<'_>, m: Marker) -> CompletedMarker {
    p.bump(K::STATIC_KW);
    if p.at(K::FUNCTION_KW) {
        decl::closure_expr(p, Some(m))
    } else {
        decl::arrow_fn_expr(p, Some(m))
    }
}

/// The primary at the current token, or `None` when no expression can
/// start here, which the enclosing site then names. `(void)` is a primary
/// only where a grant says so (a statement's start, a `for` header):
///
/// ```php
/// $y = (void)$x;
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected token "(void)" in %s on line 2
/// ```
#[allow(clippy::too_many_lines)] // one arm per primary form: a dispatch table in match shape
fn prefix(p: &mut Parser<'_>) -> Option<Lhs> {
    // One-shot: only the immediately next primary may be an unassigned
    // `list(...)` (a `foreach` target, an array or list element); a
    // descent through another primary first revokes the grant.
    let list_target_ok = std::mem::take(&mut p.grants.list_target);
    let void_ok = std::mem::take(&mut p.grants.void);
    let kind = p.current();
    let done = |cm: CompletedMarker| Some(Lhs::Done(cm));
    match kind {
        K::PLUS | K::MINUS | K::BANG | K::TILDE => {
            let m = p.start();
            p.bump(kind);
            let rbp = if kind == K::BANG { BP_NOT } else { BP_UNARY };
            operand(p, rbp);
            done(m.complete(p, K::UNARY_EXPR))
        }
        K::PLUS_PLUS | K::MINUS_MINUS => {
            let m = p.start();
            p.bump(kind);
            operand(p, BP_UNARY);
            done(m.complete(p, K::PREFIX_INC_DEC_EXPR))
        }
        K::CAST | K::CAST_REMOVED => {
            let lower = p.current_text().to_ascii_lowercase();
            // `(void)` (8.5) is a statement form and a `for` header's,
            // never an operand (`T_VOID_CAST expr ';'`,
            // `non_empty_for_exprs`): where the grant is not given it is
            // the token php did not want, which the enclosing site names
            // (`$y = (void)$x;` is `unexpected token "(void)"`, `[(void)$y]`
            // adds `expecting "]"`, probed 8.5.9). Granted, it prefixes the
            // whole expression (`(void) f() + 1` casts the sum).
            let is_void = lower.windows(4).any(|w| w == b"void");
            if is_void && !void_ok {
                return None;
            }
            let m = p.start();
            if kind == K::CAST_REMOVED && lower.windows(5).any(|w| w == b"unset") {
                // `(unset)` parses as a cast and php's compiler refuses
                // it, `zend_compile_cast` at the pin: a compile error,
                // after every parse error in the file (design §6.1's
                // lexical fatals; `(real)` is the lexer's, since php's
                // scanner throws on it).
                let span = p.current_span();
                let line = p.line_here();
                p.error(Diagnostic::of(
                    Level::CompileError,
                    span,
                    line,
                    "The (unset) cast is no longer supported",
                ));
            }
            p.bump(kind);
            operand(p, if is_void { 0 } else { BP_UNARY });
            done(m.complete(p, K::CAST_EXPR))
        }
        K::AT => {
            let m = p.start();
            p.bump(K::AT);
            operand(p, BP_UNARY);
            done(m.complete(p, K::ERROR_SUPPRESS_EXPR))
        }
        K::CLONE_KW => {
            let m = p.start();
            p.bump(K::CLONE_KW);
            if p.at(K::L_PAREN) && clone_call_form(p) {
                // The 8.5 with-properties call form, `clone($obj, [...])`:
                // a full argument list (named arguments, `...`, a trailing
                // comma; pinned 8.5.6), taken only when the parenthesised
                // group holds a top-level comma.
                decl::arg_list(p);
            } else {
                // The prefix keyword form. `clone` binds looser than the
                // trailers, so a single parenthesised operand absorbs its
                // `->`, `[]` and `()` chain: `clone (new A())->m()` is
                // `clone((new A())->m())` and `clone($o)->v` is
                // `clone(($o)->v)` (probed 8.5.7: the latter is a
                // TypeError on the fetched scalar, not `(clone $o)->v`).
                operand(p, BP_CLONE);
            }
            done(m.complete(p, K::CLONE_EXPR))
        }
        K::PRINT_KW => {
            let m = p.start();
            p.bump(K::PRINT_KW);
            operand(p, BP_PRINT);
            done(m.complete(p, K::PRINT_EXPR))
        }
        K::THROW_KW => {
            let m = p.start();
            p.bump(K::THROW_KW);
            operand(p, BP_THROW);
            done(m.complete(p, K::THROW_EXPR))
        }
        K::YIELD_KW => done(yield_expr(p)),
        K::INCLUDE_KW | K::INCLUDE_ONCE_KW | K::REQUIRE_KW | K::REQUIRE_ONCE_KW => {
            let m = p.start();
            p.bump(kind);
            operand(p, BP_INCLUDE);
            done(m.complete(p, K::INCLUDE_EXPR))
        }
        K::NEW_KW => Some(new_expr(p)),
        K::MATCH_KW => done(match_expr(p)),
        K::ISSET_KW => done(isset_expr(p)),
        K::EMPTY_KW => done(empty_or_eval(p, K::EMPTY_KW, K::EMPTY_EXPR)),
        K::EVAL_KW => done(empty_or_eval(p, K::EVAL_KW, K::EVAL_EXPR)),
        K::EXIT_KW | K::DIE_KW => {
            let m = p.start();
            p.bump(kind);
            if p.at(K::L_PAREN) {
                decl::arg_list(p);
            }
            done(m.complete(p, K::EXIT_EXPR))
        }
        K::LIST_KW if p.nth(1) != K::L_PAREN => {
            // A bare `list` without its mandatory `(`: php reports the
            // next token, the end of the input included, with the bison
            // clause (probed 8.5.7: `list` at the end is `syntax error,
            // unexpected end of file, expecting "("`), never `unexpected
            // token "list"`.
            let m = p.start();
            p.bump(K::LIST_KW);
            let d = p.unexpected_here_expecting("\"(\"");
            p.error(d);
            done(m.complete(p, K::LIST_EXPR))
        }
        K::LIST_KW => {
            let m = p.start();
            p.bump(K::LIST_KW);
            p.bump(K::L_PAREN);
            array_elements(p, K::R_PAREN);
            super::expect_closer(p, K::R_PAREN, Some("\")\""));
            // Zend: `list(...)` exists only as an `=` target, a `foreach`
            // target, or nested in an enclosing destructuring pattern;
            // `var_dump(list($a))` is a parse error at the next token,
            // `expecting "="` (php-src list_011).
            if !list_target_ok && !p.at(K::EQ) {
                let d = p.unexpected_here_expecting("\"=\"");
                p.error(d);
            }
            done(m.complete(p, K::LIST_EXPR))
        }
        K::ARRAY_KW if p.nth(1) == K::L_PAREN => {
            let m = p.start();
            p.bump(K::ARRAY_KW);
            p.bump(K::L_PAREN);
            array_elements(p, K::R_PAREN);
            super::expect_closer(p, K::R_PAREN, Some("\")\""));
            done(m.complete(p, K::ARRAY_EXPR))
        }
        K::L_BRACK => {
            let m = p.start();
            p.bump(K::L_BRACK);
            array_elements(p, K::R_BRACK);
            super::expect_closer(p, K::R_BRACK, Some("\"]\""));
            done(m.complete(p, K::ARRAY_EXPR))
        }
        K::L_PAREN => {
            let m = p.start();
            p.bump(K::L_PAREN);
            if expr_bp(p, 0).map(|lhs| lhs.finish(p)).is_none() {
                p.error_unexpected();
            }
            // After a parenthesised expression the operator set is too
            // large to spell: no clause (`(1 ;`, probed 8.5.9).
            super::expect_closer(p, K::R_PAREN, None);
            done(m.complete(p, K::PAREN_EXPR))
        }
        K::INT | K::FLOAT | K::SINGLE_QUOTED => {
            let m = p.start();
            p.bump(kind);
            done(m.complete(p, K::LITERAL_EXPR))
        }
        K::MAGIC_CONST => {
            let m = p.start();
            p.bump(K::MAGIC_CONST);
            done(m.complete(p, K::MAGIC_CONST_EXPR))
        }
        K::VARIABLE => {
            let m = p.start();
            p.bump(K::VARIABLE);
            done(m.complete(p, K::SIMPLE_VAR_EXPR))
        }
        K::DOLLAR => done(variable_variable(p)),
        K::DOUBLE_QUOTE => done(strings::interpolated(
            p,
            K::DOUBLE_QUOTE,
            K::INTERPOLATED_STRING_EXPR,
        )),
        K::BACKQUOTE => done(strings::interpolated(p, K::BACKQUOTE, K::SHELL_EXEC_EXPR)),
        K::HEREDOC_START => done(strings::heredoc(p)),
        K::FUNCTION_KW => done(decl::closure_expr(p, None)),
        K::FN_KW => done(decl::arrow_fn_expr(p, None)),
        K::STATIC_KW if matches!(p.nth(1), K::FUNCTION_KW | K::FN_KW) => {
            let m = p.start();
            done(static_closure_or_arrow(p, m))
        }
        K::ATTRIBUTE_OPEN => {
            // Attributes in expression position belong to a closure, an
            // arrow function or an anonymous class.
            let m = p.start();
            decl::attribute_groups(p);
            match p.current() {
                K::FUNCTION_KW => done(decl::closure_expr(p, Some(m))),
                K::FN_KW => done(decl::arrow_fn_expr(p, Some(m))),
                K::STATIC_KW if matches!(p.nth(1), K::FUNCTION_KW | K::FN_KW) => {
                    done(static_closure_or_arrow(p, m))
                }
                _ => {
                    // Attributes with none of the three after them: an
                    // ERROR node, which is not an `Expr`, so it must not
                    // satisfy `expr()` (schema `ExprStmt.expr!`).
                    p.error_unexpected();
                    m.complete(p, K::ERROR);
                    None
                }
            }
        }
        // `static` as a class reference (`static::…`, `static` as a type):
        // in expression position `::` is the one continuation php can
        // spell (`static ;` is `expecting "::"`, probed 8.5.9); inside a
        // `class_name_reference` (`new static`, `$x instanceof static`)
        // it stands on its own, as `types::name_ref` reads it in a type.
        K::STATIC_KW => {
            let m = p.start();
            p.bump_remap(K::IDENT);
            if !p.at(K::COLON_COLON) && !p.grants.class_ref {
                let d = p.unexpected_here_expecting("\"::\"");
                p.error(d);
            }
            Some(Lhs::Bare(m, K::IDENT))
        }
        // A bare `callable` starts nothing: php's `T_CALLABLE` is a type
        // and a member name, never a constant, a call or a class
        // reference (`callable();`, `callable::x` are the keyword out of
        // place, probed 8.5.9). The site that wanted an expression
        // reports it.
        K::IDENT if ckw::is_callable_text(kind, p.current_text()) => None,
        K::IDENT | K::NAME_QUALIFIED | K::NAME_FULLY_QUALIFIED | K::NAME_RELATIVE => {
            let m = p.start();
            p.bump(kind);
            Some(Lhs::Bare(m, kind))
        }
        // A contextual-only keyword used as a plain name (`enum(1)`).
        k if ckw::is_contextual_only(k) => {
            let m = p.start();
            p.bump_remap(K::IDENT);
            Some(Lhs::Bare(m, K::IDENT))
        }
        // `readonly(…)` is a call to a function named `readonly`.
        K::READONLY_KW if p.nth(1) == K::L_PAREN => {
            let m = p.start();
            p.bump_remap(K::IDENT);
            Some(Lhs::Bare(m, K::IDENT))
        }
        _ => None,
    }
}

/// An operand at `rbp`, with the unexpected-token diagnostic when nothing
/// can start.
fn operand(p: &mut Parser<'_>, rbp: u8) {
    if let Some(lhs) = expr_bp(p, rbp) {
        lhs.finish(p);
    } else {
        p.error_unexpected();
    }
}

/// The assignment-family operators (Zend's `variable OP= expr` productions).
fn is_assign_op(kind: K) -> bool {
    matches!(
        kind,
        K::EQ
            | K::PLUS_EQ
            | K::MINUS_EQ
            | K::STAR_EQ
            | K::STAR_STAR_EQ
            | K::SLASH_EQ
            | K::DOT_EQ
            | K::PERCENT_EQ
            | K::AMP_EQ
            | K::PIPE_EQ
            | K::CARET_EQ
            | K::SHL_EQ
            | K::SHR_EQ
            | K::QUESTION_QUESTION_EQ
    )
}

/// Is the parsed operand one of Zend's `variable` productions, the only
/// legal assignment targets at the grammar level (`f() = x` parses in php
/// too, rejected at compile)? A scoped access counts only with a
/// `$`-selector (`A::$p` yes, `A::C` and `A::class` no, the
/// [`Lhs::ScopedConst`] variant), and a bare `new` is never a variable.
fn is_variable_production(lhs: &Lhs) -> bool {
    if matches!(lhs, Lhs::ScopedConst(_) | Lhs::NewBare(_)) {
        return false;
    }
    matches!(
        lhs.kind(),
        K::SIMPLE_VAR_EXPR
            | K::VAR_VAR_EXPR
            | K::DYNAMIC_VAR_EXPR
            | K::SUBSCRIPT_EXPR
            | K::MEMBER_ACCESS_EXPR
            | K::SCOPED_ACCESS_EXPR
            | K::CALL_EXPR
    )
}

/// Does `lead` begin one of php's `dereferenceable` primaries, the
/// operands after which php's `variable`-position error spells the
/// dereference set `"->" / "?->" / "["` when the operand is complete but
/// not dereferenced (`unset(new A())`, `unset('s')`, `unset([1])`,
/// `unset(A::C)`, `unset(($x))` and `unset(foo)` all carry that clause,
/// probed 8.5.7)? The node kind alone cannot decide it: `'s'` and `5` are
/// both `LITERAL_EXPR`, and only the string is dereferenceable, so the
/// leading token discriminates. Not dereferenceable, where php errors at
/// the token with no clause: an integer or float, a heredoc or backtick,
/// and every operator-prefixed form (`unset(-5)`, `unset(match…)`). A
/// bare `new X` is classified apart at the call site; its clause is `"("`.
fn starts_dereferenceable(lead: K) -> bool {
    matches!(
        lead,
        K::SINGLE_QUOTED
            | K::DOUBLE_QUOTE
            | K::L_BRACK
            | K::ARRAY_KW
            | K::L_PAREN
            | K::MAGIC_CONST
            | K::IDENT
            | K::NAME_QUALIFIED
            | K::NAME_FULLY_QUALIFIED
            | K::NAME_RELATIVE
            | K::STATIC_KW
            // `new X()` and `new class …` (with arguments or a body) are
            // dereferenceable; the argument-less `new X` is peeled off as
            // `Lhs::NewBare` first.
            | K::NEW_KW
    ) || ckw::is_contextual_only(lead)
}

/// The operand of `= &` or an `unset()` argument: a single Zend `variable`
/// production, a primary plus dereference trailers and no binary operator.
/// php sub-cases the `variable`-position `expecting …` clause by the
/// primary's shape (probed 8.5.7): a bare `new X` (no constructor parens)
/// wants its argument list, `"("`; a dereferenceable primary that is not
/// dereferenced wants the dereference set `"->" or "?->" or "["`; a
/// scalar that is not dereferenceable is the unexpected token itself.
fn variable_only(p: &mut Parser<'_>) -> Option<K> {
    variable_or_pattern(p, false)
}

/// [`variable_only`], or with `patterns` also a destructuring pattern.
fn variable_or_pattern(p: &mut Parser<'_>, patterns: bool) -> Option<K> {
    // php errors at the token that starts what is no variable at all
    // (`unset(1)` and `foreach ($a as 1` are `unexpected integer "1"`,
    // probed 8.5.9); the diagnostic is built here, before the primary is
    // consumed, and raised only then.
    let lead = p.current();
    let at_lead = p.unexpected_here();
    let Some(mut lhs) = prefix(p) else {
        p.error(at_lead);
        return None;
    };
    while let Some(trailer) = at_trailer(p, &lhs) {
        lhs = apply_trailer(p, lhs, trailer);
    }
    let pattern = patterns && matches!(lhs.kind(), K::LIST_EXPR | K::ARRAY_EXPR);
    if !is_variable_production(&lhs) && !pattern {
        if matches!(lhs, Lhs::NewBare(_)) {
            let d = p.unexpected_here_expecting("\"(\"");
            p.error(d);
        } else if starts_dereferenceable(lead) {
            let d = p.unexpected_here_expecting(AFTER_VARIABLE);
            p.error(d);
        } else {
            p.error(at_lead);
        }
    }
    let kind = lhs.kind();
    lhs.finish(p);
    Some(kind)
}

/// The `= &` reference-assignment target (Zend: `'=' ampersand variable`).
fn ref_target(p: &mut Parser<'_>) {
    variable_only(p);
}

/// php's clause at the closer after a `variable` production: the
/// dereference set, which is what `unset($x ;` and `foreach ($a as $b ;`
/// print (probed 8.5.9).
pub(crate) const AFTER_VARIABLE: &str = "\"->\" or \"?->\" or \"[\"";

/// The `unset(...)` arguments: each a `variable` production (php's
/// `unset_variables`); a trailing comma is legal.
pub(crate) fn unset_operands(p: &mut Parser<'_>) {
    let mut after_comma = false;
    // `?>` is the `;` php's scanner makes of it, unexpected here with
    // no clause, and left for the enclosing construct.
    if p.at(K::CLOSE_TAG) {
        p.error_unexpected();
    }
    // `unset_variables` is `unset_variable | unset_variables ','
    // unset_variable`, so it takes one operand at least and the empty
    // list is nobody's to reduce: php is at the closer with no rule for
    // it, `unexpected token ")"` with no clause (probed 8.5.9). Without
    // this the list walks straight out and the statement stands with an
    // `UNSET_STMT` carrying nothing.
    if p.at(K::R_PAREN) {
        p.error_unexpected();
    }
    while !p.at_eof() && !p.at(K::R_PAREN) && !p.at(K::CLOSE_TAG) {
        // After a comma php's clause collapses to the closer
        // (`unset($x, ;` is `expecting ")"`, probed 8.5.9); at the first
        // argument the set is too large to spell.
        if after_comma && !at_expr_start(p) {
            let d = p.unexpected_here_expecting("\")\"");
            p.error(d);
            break;
        }
        variable_only(p);
        if !p.eat(K::COMMA) {
            break;
        }
        after_comma = true;
    }
}

/// A `foreach` key or target: one Zend `foreach_variable`, which is a
/// `variable` production or a `list(…)` or `[…]` pattern (the grant is
/// the caller's). The completed operand's kind, for the closer's clause.
pub(crate) fn variable_target(p: &mut Parser<'_>) -> Option<K> {
    variable_or_pattern(p, true)
}

/// `$$x` and `${expr}`, the variable variables. Under php 7's uniform
/// variable syntax `$$x[0]` is `($$x)[0]`: the inner position takes only
/// a variable form, never a trailer. Also entered from a `global`
/// statement (Zend's `simple_variable`).
pub(crate) fn variable_variable(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    p.bump(K::DOLLAR);
    match p.current() {
        K::L_CURLY => {
            p.bump(K::L_CURLY);
            operand(p, 0);
            super::expect_closer(p, K::R_CURLY, None);
            m.complete(p, K::DYNAMIC_VAR_EXPR)
        }
        K::VARIABLE => {
            let v = p.start();
            p.bump(K::VARIABLE);
            v.complete(p, K::SIMPLE_VAR_EXPR);
            m.complete(p, K::VAR_VAR_EXPR)
        }
        K::DOLLAR => {
            // `$$$$…$x` is one native frame per `$`, and php refuses the
            // same shape with `Parse error: memory exhausted`, bisected
            // on 8.5.7 to 9 987 `$` passing and 10 044 failing, so the
            // depth guard moves this parser toward the oracle: the same
            // refusal, only the threshold differing. Past the cap the `$`
            // run is consumed flat.
            if p.enter_depth() {
                variable_variable(p);
                p.exit_depth();
            } else {
                // The rest of the `$` run, flat. Not `absorb_balanced`,
                // which scans to the statement's `;` and eats it, and
                // that cascades a second diagnostic onto the one the cap
                // already reported.
                let e = p.start();
                while p.at(K::DOLLAR) {
                    p.bump_any();
                }
                if p.at(K::VARIABLE) {
                    p.bump_any();
                }
                e.complete(p, K::ERROR);
            }
            m.complete(p, K::VAR_VAR_EXPR)
        }
        _ => {
            // php's clause at Zend's `simple_variable` slot is the fixed
            // set `variable or "{" or "$"` (`$"…";` is `unexpected
            // double-quoted string "…", expecting variable or "{" or
            // "$"`, probed 8.5.7, bug70430).
            let d = p.unexpected_here_expecting("variable or \"{\" or \"$\"");
            p.error(d);
            m.complete(p, K::VAR_VAR_EXPR)
        }
    }
}

fn yield_expr(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    p.bump(K::YIELD_KW);
    if p.at(K::FROM_KW) {
        p.bump(K::FROM_KW);
        operand(p, BP_YIELD);
        return m.complete(p, K::YIELD_FROM_EXPR);
    }
    // A bare `yield`, `yield $v`, or `yield $k => $v`.
    if let Some(first) = expr_bp(p, BP_YIELD) {
        if p.at(K::FAT_ARROW) {
            // What was parsed was the key (`=>` lives inside `YieldKey`).
            let key_wrap = first.wrap(p);
            p.bump(K::FAT_ARROW);
            key_wrap.complete(p, K::YIELD_KEY);
            if let Some(value) = expr_bp(p, BP_YIELD) {
                value.finish(p);
            } else {
                p.error_unexpected();
            }
        } else {
            first.finish(p);
        }
    }
    m.complete(p, K::YIELD_EXPR)
}

/// `new` with its restricted class-reference grammar; an anonymous class
/// is `decl`'s. Answers an [`Lhs`] because dereferenceability is a grammar
/// fact: only `new C(args)` and an anonymous class take postfix trailers
/// (the 8.4 rule).
fn new_expr(p: &mut Parser<'_>) -> Lhs {
    let m = p.start();
    p.bump(K::NEW_KW);
    // Anonymous classes: `new class`, `new #[A] class`, and the modifier
    // forms (`new readonly class`, 8.3). The modifier run may be longer
    // than one token (`new readonly readonly class`, `new final abstract
    // class`: `decl::anon_class_decl` consumes the whole run and the
    // compiler raises the duplicate or invalid modifier), so a contiguous
    // run of class modifiers is scanned to the `class` rather than one
    // token peeked.
    let modifiers_then_class = {
        let mut k = 0;
        while matches!(p.nth(k), K::READONLY_KW | K::FINAL_KW | K::ABSTRACT_KW) {
            k += 1;
        }
        k > 0 && p.nth(k) == K::CLASS_KW
    };
    if p.at(K::CLASS_KW) || p.at(K::ATTRIBUTE_OPEN) || modifiers_then_class {
        decl::anon_class_decl(p);
        // An anonymous class is always dereferenceable.
        return Lhs::Done(m.complete(p, K::NEW_EXPR));
    }
    if !class_name_reference(p) {
        // A bare `namespace` keyword that is not the start of a relative
        // name (`new namespace::$foo`) reports `expecting ":"` in the
        // call-argument position bug43343 pins (probed 8.5.7). The
        // statement-position form spells `expecting "class"` instead, a
        // bison-state nuance with no corpus row; the pinned clause stands.
        // Elsewhere php spells `"class"`, the anonymous class being the
        // one keyword continuation (`new ;`, probed 8.5.9).
        let d = if p.at(K::NAMESPACE_KW) {
            p.unexpected_here_expecting("\":\"")
        } else {
            p.unexpected_here_expecting("\"class\"")
        };
        p.error(d);
    }
    let has_args = p.at(K::L_PAREN);
    if has_args {
        decl::arg_list(p);
    }
    let cm = m.complete(p, K::NEW_EXPR);
    if has_args {
        Lhs::Done(cm)
    } else {
        Lhs::NewBare(cm)
    }
}

/// php's `class_name_reference`, the right operand of `instanceof` and
/// what `new` names: `class_name` (`static` or a name), a `new_variable`
/// or a parenthesised expression. The trailer rights depend on the prefix
/// shape (pinned 8.5.6): a bare name takes only `::` and a `$`-variable
/// (then the chain is a `new_variable` with the full trailer set); a
/// variable prefix takes the full set from the start; a parenthesised
/// expression takes none. `->` and `[` directly after a bare name are not
/// part of the class reference: they dangle and the enclosing construct
/// reports them, exactly where php errors on `new A->p`, `new A[0]` and
/// `$x instanceof A->p`. `false`, consuming nothing, when no prefix can
/// start here; the site names what it expected.
///
/// ```php
/// $x instanceof A::C;
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected identifier "C", expecting variable or "$" in %s on line 2
/// ```
fn class_name_reference(p: &mut Parser<'_>) -> bool {
    // What can start one: `class_name` (`static`, a name), a
    // `simple_variable` (`$v`, `$$v`, `${e}`) or `(`. A literal, `new`
    // or a string is the unexpected token (`$x instanceof 1;`, probed
    // 8.5.9); `prefix` would read it as an operand. `callable` is a
    // type and never a `class_name`: `new callable` is `expecting
    // "class"` and `$x instanceof callable` the bare token (probed
    // 8.5.9), the site's word as for any token that starts nothing.
    let starts = match p.current() {
        K::IDENT if ckw::is_callable_text(K::IDENT, p.current_text()) => false,
        K::IDENT
        | K::NAME_QUALIFIED
        | K::NAME_FULLY_QUALIFIED
        | K::NAME_RELATIVE
        | K::STATIC_KW
        | K::VARIABLE
        | K::DOLLAR
        | K::L_PAREN => true,
        kind => ckw::is_contextual_only(kind),
    };
    if !starts {
        return false;
    }
    p.grants.class_ref = true;
    let class_ref = prefix(p);
    p.grants.class_ref = false;
    let Some(mut lhs) = class_ref else {
        return false;
    };
    let mut bare_name = matches!(lhs, Lhs::Bare(..));
    let paren_prefix = lhs.kind() == K::PAREN_EXPR;
    if !paren_prefix {
        loop {
            match p.current() {
                K::COLON_COLON => {
                    let sm = lhs.wrap(p);
                    p.bump(K::COLON_COLON);
                    if !matches!(p.current(), K::VARIABLE | K::DOLLAR) {
                        // php: `new A::C` is `expecting variable or "$"`.
                        let d = p.unexpected_expecting("variable or \"$\"");
                        p.error(d);
                    }
                    member_selector(p, true);
                    lhs = Lhs::Done(sm.complete(p, K::SCOPED_ACCESS_EXPR));
                    bare_name = false;
                }
                K::L_BRACK | K::ARROW | K::NULLSAFE_ARROW if !bare_name => {
                    if let Some(trailer) = at_trailer(p, &lhs) {
                        lhs = apply_trailer(p, lhs, trailer);
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
    }
    lhs.finish(p);
    true
}

fn match_expr(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    p.bump(K::MATCH_KW);
    if p.eat(K::L_PAREN) {
        operand(p, 0);
        super::expect_closer(p, K::R_PAREN, None);
    } else {
        let d = p.unexpected_here_expecting("\"(\"");
        p.error(d);
    }
    if p.eat(K::L_CURLY) {
        while !p.at_eof() && !p.at(K::R_CURLY) {
            let arm = p.start();
            if p.at(K::DEFAULT_KW) {
                p.bump(K::DEFAULT_KW);
                // `default , => …`: the trailing comma is grammar-legal
                // here too (php-src match/041, pinned 8.5.6).
                if !p.at(K::FAT_ARROW) {
                    p.eat(K::COMMA);
                }
            } else {
                let conds = p.start();
                // The conditions allow a trailing comma before `=>`
                // (php-src `match_arm_cond_list`). At an arm's start
                // php's clause is the closer's (`{ , }` and `{ 1 => 2,
                // => 3 }` are `expecting "}"`, probed 8.5.9).
                let mut first = true;
                loop {
                    if !expr(p) {
                        let d = if first {
                            p.unexpected_here_expecting("\"}\"")
                        } else {
                            p.unexpected_here_expecting("\"=>\"")
                        };
                        p.error(d);
                        break;
                    }
                    first = false;
                    if !p.eat(K::COMMA) || p.at(K::FAT_ARROW) {
                        break;
                    }
                }
                conds.complete(p, K::MATCH_CONDS);
            }
            if !p.eat(K::FAT_ARROW) {
                // After the conditions only `=>` can follow (`{ 1 ;`,
                // `{ default ;`, probed 8.5.9); an arm that failed at its
                // first token already said so.
                if !p.has_error_at_or_after(p.current_span().start) {
                    let d = p.unexpected_here_expecting("\"=>\"");
                    p.error(d);
                }
                arm.complete(p, K::MATCH_ARM);
                // Recovery: skip to `,` or `}`.
                while !p.at_eof() && !p.at(K::COMMA) && !p.at(K::R_CURLY) {
                    p.bump_any();
                }
                if !p.eat(K::COMMA) {
                    break;
                }
                continue;
            }
            operand(p, 0);
            arm.complete(p, K::MATCH_ARM);
            if !p.eat(K::COMMA) {
                break;
            }
        }
        super::expect_closer(p, K::R_CURLY, Some("\"}\""));
    } else {
        let d = p.unexpected_here_expecting("\"{\"");
        p.error(d);
    }
    m.complete(p, K::MATCH_EXPR)
}

fn isset_expr(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    p.bump(K::ISSET_KW);
    if p.eat(K::L_PAREN) {
        expr_list_trailing(p, K::R_PAREN);
        super::expect_closer(p, K::R_PAREN, Some("\")\""));
    } else {
        // php's bison clause at the mandatory-paren keywords (probed
        // 8.5.7: `isset` and `isset;` are `… expecting "("`, the end of
        // the input and a token alike).
        let d = p.unexpected_here_expecting("\"(\"");
        p.error(d);
    }
    m.complete(p, K::ISSET_EXPR)
}

fn empty_or_eval(p: &mut Parser<'_>, kw: K, node: K) -> CompletedMarker {
    let m = p.start();
    p.bump(kw);
    if p.eat(K::L_PAREN) {
        operand(p, 0);
        super::expect_closer(p, K::R_PAREN, None);
    } else {
        // php's bison clause (probed 8.5.7, require_parse_exception:
        // `empty` at the end of the input is `syntax error, unexpected
        // end of file, expecting "("`).
        let d = p.unexpected_here_expecting("\"(\"");
        p.error(d);
    }
    m.complete(p, node)
}

/// The `[a, &$b, ...$c, k => v, ,]` element list of an array, a `list()`
/// and a destructuring pattern.
pub(crate) fn array_elements(p: &mut Parser<'_>, closer: K) {
    // `?>` is a hard recovery anchor: never consumed as a stray element;
    // it is the `;` php's scanner makes of it, reported here with the
    // element position's clause and left for the enclosing construct.
    if p.at(K::CLOSE_TAG) {
        let d = p.unexpected_here_expecting(php_list_closer(closer));
        p.error(d);
    }
    while !p.at_eof() && !p.at(closer) && !p.at(K::CLOSE_TAG) {
        let el = p.start();
        if p.at(K::COMMA) {
            // An elision, a fully empty element, legal only in
            // destructuring, which the compiler holds.
            el.complete(p, K::ARRAY_ELEMENT);
            p.bump(K::COMMA);
            continue;
        }
        if p.at(K::DOT_DOT_DOT) {
            p.bump(K::DOT_DOT_DOT);
            operand(p, 0);
            el.complete(p, K::ARRAY_ELEMENT);
        } else {
            let byref_first = p.at(K::AMP);
            if byref_first {
                p.bump(K::AMP);
            }
            let first = p.start();
            // An element position is a Zend `array_pair`: a nested
            // `list(...)` here needs no `=` of its own (and a list key
            // parses in php too; its rejection is the compiler's).
            p.grants.list_target = true;
            if let Some(parsed) = expr_bp(p, 0) {
                if !byref_first && p.at(K::FAT_ARROW) {
                    // It was the key: wrapped as `ArrayKey`, with the `=>`.
                    let key = parsed.wrap(p);
                    p.bump(K::FAT_ARROW);
                    key.complete(p, K::ARRAY_KEY);
                    first.abandon(p);
                    p.eat(K::AMP);
                    if p.at(K::DOT_DOT_DOT) {
                        // `k => ...$v` is invalid; parsed, and the
                        // compiler rejects the spread.
                        p.bump(K::DOT_DOT_DOT);
                    }
                    p.grants.list_target = true;
                    operand(p, 0);
                } else {
                    parsed.finish(p);
                    first.abandon(p);
                }
            } else {
                first.abandon(p);
                // php's expected set after `[1, ` collapses to the closer
                // when nothing can start an element (`[1, FOREACH]` is
                // `unexpected token "foreach", expecting "]"`; the
                // `array(` and `list(` forms spell `")"`, probed 8.5.7).
                let d = p.unexpected_here_expecting(php_list_closer(closer));
                p.error(d);
                el.complete(p, K::ARRAY_ELEMENT);
                break;
            }
            el.complete(p, K::ARRAY_ELEMENT);
        }
        if !p.eat(K::COMMA) {
            if !p.at(closer) && !p.at_eof() && at_expr_start(p) {
                // A missing comma between elements: the repair.
                super::missing_comma(p, php_list_closer(closer));
                continue;
            }
            break;
        }
    }
}

/// How php spells `closer` in an array or list element clause (see
/// [`array_elements`]).
fn php_list_closer(closer: K) -> &'static str {
    if closer == K::R_BRACK {
        "\"]\""
    } else {
        "\")\""
    }
}

/// Can the current token begin an expression? Conservative: it gates the
/// missing-comma repair and nothing else.
pub(crate) fn at_expr_start(p: &mut Parser<'_>) -> bool {
    matches!(
        p.current(),
        K::INT
            | K::FLOAT
            | K::SINGLE_QUOTED
            | K::DOUBLE_QUOTE
            | K::HEREDOC_START
            | K::BACKQUOTE
            | K::VARIABLE
            | K::DOLLAR
            | K::IDENT
            | K::NAME_QUALIFIED
            | K::NAME_FULLY_QUALIFIED
            | K::NAME_RELATIVE
            | K::MAGIC_CONST
            | K::L_BRACK
            | K::L_PAREN
            | K::BANG
            | K::PLUS
            | K::MINUS
            | K::TILDE
            | K::AT
            | K::PLUS_PLUS
            | K::MINUS_MINUS
            | K::CAST
            | K::CAST_REMOVED
            | K::NEW_KW
            | K::CLONE_KW
            | K::MATCH_KW
            | K::FUNCTION_KW
            | K::FN_KW
            | K::STATIC_KW
            | K::ISSET_KW
            | K::EMPTY_KW
            | K::EVAL_KW
            | K::EXIT_KW
            | K::DIE_KW
            | K::LIST_KW
            | K::ARRAY_KW
            | K::YIELD_KW
            | K::THROW_KW
            | K::PRINT_KW
            | K::ATTRIBUTE_OPEN
    )
}

// ---------------------------------------------------------------------------
// Trailers
// ---------------------------------------------------------------------------

/// One postfix trailer, read off the current token by [`at_trailer`] and
/// applied by [`apply_trailer`], so the two agree on the set by type.
#[derive(Clone, Copy)]
enum Trailer {
    /// `(args)`.
    Call,
    /// `[index]`.
    Subscript,
    /// `->member` or `?->member`.
    Member,
    /// `::member`.
    Scoped,
}

/// The postfix trailer the current token begins for this operand, if any.
/// The curly offset 8.0 removed begins none:
///
/// ```php
/// $s{0};
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected token "{" in %s on line 2
/// ```
///
/// Nor does anything after a `clone`, 8.5's argument list included:
///
/// ```php
/// $o = new stdClass;
/// var_dump(clone($o, [])->p);
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected token "->", expecting ")" in %s on line 3
/// ```
fn at_trailer(p: &mut Parser<'_>, lhs: &Lhs) -> Option<Trailer> {
    // The 8.4 new-dereference rule: `new A()->p` and an anonymous class
    // chain, but a bare `new A` takes no trailer; the dangling `->`, `[`
    // or `::` is the enclosing construct's parse error, exactly where php
    // reports it.
    if matches!(lhs, Lhs::NewBare(_)) {
        return None;
    }
    // An `instanceof` whose class reference took what it may: the `->`,
    // `[` or `(` after it is the enclosing construct's (`$x instanceof
    // A->p;` and `$x instanceof $y();` are the bare unexpected token,
    // probed 8.5.9). A `clone` is none of php's `dereferenceable`
    // productions either: the keyword form's operand took every trailer
    // it may, and 8.5's `clone($o, [...])` is a whole `expr`, so
    // `clone($o, [])->p` is the unexpected `->`.
    if let Lhs::Done(cm) = lhs
        && matches!(cm.kind, K::INSTANCEOF_EXPR | K::CLONE_EXPR)
    {
        return None;
    }
    Some(match p.current() {
        K::L_PAREN => Trailer::Call,
        K::L_BRACK => Trailer::Subscript,
        K::ARROW | K::NULLSAFE_ARROW => Trailer::Member,
        K::COLON_COLON => Trailer::Scoped,
        // `$s{0}`, the curly offset 8.0 removed, is no trailer: php has no
        // such production, so the `{` is the token the enclosing site did
        // not want (`$s{0};` is `unexpected token "{"`, `foo($s{0})` adds
        // `expecting ")"`, probed 8.5.9; design §6.1's lexical fatals).
        _ => return None,
    })
}

/// Applies the trailer [`at_trailer`] found.
fn apply_trailer(p: &mut Parser<'_>, lhs: Lhs, trailer: Trailer) -> Lhs {
    match trailer {
        Trailer::Call => {
            let m = lhs.wrap(p);
            decl::arg_list(p);
            Lhs::Done(m.complete(p, K::CALL_EXPR))
        }
        Trailer::Subscript => {
            let m = lhs.wrap(p);
            p.bump(K::L_BRACK);
            // `$x[]` is legal (a push), so at `[` php spells the closer
            // beside the operand (`$x[ ;` is `expecting "]"`, probed
            // 8.5.9).
            if !p.at(K::R_BRACK) && expr_bp(p, 0).map(|e| e.finish(p)).is_none() {
                let d = p.unexpected_here_expecting("\"]\"");
                p.error(d);
            }
            super::expect_closer(p, K::R_BRACK, Some("\"]\""));
            Lhs::Done(m.complete(p, K::SUBSCRIPT_EXPR))
        }
        Trailer::Member => {
            let m = lhs.wrap(p);
            p.bump_any();
            member_selector(p, false);
            Lhs::Done(m.complete(p, K::MEMBER_ACCESS_EXPR))
        }
        Trailer::Scoped => {
            let m = lhs.wrap(p);
            p.bump(K::COLON_COLON);
            // `A::$p` is a variable production; `A::C`, `A::class` and
            // `A::{e}` are constant fetches, and the distinction gates
            // the assignment, by-reference and unset targets (Zend's
            // `variable`).
            let var_selector = matches!(p.current(), K::VARIABLE | K::DOLLAR);
            member_selector(p, true);
            let cm = m.complete(p, K::SCOPED_ACCESS_EXPR);
            if var_selector {
                Lhs::Done(cm)
            } else {
                Lhs::ScopedConst(cm)
            }
        }
    }
}

/// `->member` and `::member`: a name (semi-reserved allowed), `$var`,
/// `{expr}`, or, scoped only, `class`.
fn member_selector(p: &mut Parser<'_>, scoped: bool) {
    match p.current() {
        K::VARIABLE => {
            let m = p.start();
            p.bump(K::VARIABLE);
            m.complete(p, K::SIMPLE_VAR_EXPR);
        }
        K::DOLLAR => {
            variable_variable(p);
        }
        K::L_CURLY => {
            let m = p.start();
            p.bump(K::L_CURLY);
            operand(p, 0);
            super::expect_closer(p, K::R_CURLY, None);
            m.complete(p, K::BRACED_MEMBER);
        }
        K::CLASS_KW if scoped => {
            // `Foo::class`: the keyword token is the selector (the
            // schema's `ScopedSelector` `class` variant).
            p.bump(K::CLASS_KW);
        }
        kind if ckw::is_member_name(kind) => {
            let m = p.start();
            if kind == K::IDENT {
                p.bump(K::IDENT);
            } else {
                p.bump_remap(K::IDENT);
            }
            m.complete(p, K::MEMBER_NAME);
        }
        _ => {
            // `$x-> ;` is `expecting identifier or variable or "{" or "$"`;
            // after `::` the set is too large to spell (probed 8.5.9).
            let d = if scoped {
                p.unexpected_here()
            } else {
                p.unexpected_here_expecting("identifier or variable or \"{\" or \"$\"")
            };
            p.error(d);
        }
    }
}
