//! Declarations (design §4.9): functions, class-likes, members, parameters
//! with promotion and property accessors, attributes, modifiers with 8.4's
//! asymmetric visibility, trait-use adaptations, and the call-site
//! argument grammar (named arguments, spreads, first-class callables).

use crate::SyntaxKind as K;
use crate::parser::{CompletedMarker, Marker, Parser};
use crate::source::Diagnostic;

use super::types::TypeSite;
use super::{block, ckw, expr, semicolon, types};

/// `#[…]` attribute groups, zero or more. A group takes the pending doc
/// comment at its `#[` and gives it back after its `]`, php's
/// `attribute: T_ATTRIBUTE backup_doc_comment … ']' { CG(doc_comment) =
/// $2; }`, so what a closure's `}` or a comment inside the group did to
/// it is undone for the declaration after.
pub(crate) fn attribute_groups(p: &mut Parser<'_>) {
    while p.at(K::ATTRIBUTE_OPEN) {
        let g = p.start();
        p.doc_comment();
        p.bump(K::ATTRIBUTE_OPEN);
        let mut after_comma = false;
        loop {
            let a = p.start();
            if !types::name_ref(p) {
                // After a comma the trailing comma has reduced and only
                // `]` may follow (`#[A, ;` is `expecting "]"`, probed
                // 8.5.9); at the first attribute the set is not spelled.
                let d = if after_comma {
                    p.unexpected_here_expecting("\"]\"")
                } else {
                    p.unexpected_here()
                };
                p.error(d);
                a.abandon(p);
                break;
            }
            if p.at(K::L_PAREN) {
                arg_list(p);
            }
            a.complete(p, K::ATTRIBUTE);
            if !p.eat(K::COMMA) {
                break;
            }
            after_comma = true;
            if p.at(K::R_BRACK) {
                break; // a trailing comma
            }
        }
        super::expect_closer(p, K::R_BRACK, Some("\"]\""));
        p.doc_comment_restore();
        g.complete(p, K::ATTRIBUTE_GROUP);
    }
}

/// The modifier list: visibility, `private(set)` included, `static`,
/// `abstract`, `final`, `readonly`, `var`.
pub(crate) fn modifiers(p: &mut Parser<'_>) {
    loop {
        match p.current() {
            K::PUBLIC_KW | K::PROTECTED_KW | K::PRIVATE_KW => {
                // `private(set)`: four tokens glued in modifier position
                // (8.4), as php's scanner fuses them
                // (`Parser::at_fused_set_visibility`).
                if p.at_fused_set_visibility() {
                    let m = p.start();
                    p.bump_any();
                    p.bump(K::L_PAREN);
                    p.bump(K::SET_KW);
                    p.bump(K::R_PAREN);
                    m.complete(p, K::SET_VISIBILITY);
                } else {
                    p.bump_any();
                }
            }
            K::STATIC_KW | K::ABSTRACT_KW | K::FINAL_KW | K::VAR_KW => p.bump_any(),
            // In member and parameter position `readonly (` is always the
            // modifier, the paren beginning a DNF type (gh9500, pinned
            // 8.5.6); the call reading exists in statement position only.
            K::READONLY_KW => p.bump(K::READONLY_KW),
            _ => break,
        }
    }
}

/// A `function` declaration; `m` already covers the leading attributes.
pub(crate) fn function_decl(p: &mut Parser<'_>, m: Marker) {
    p.bump(K::FUNCTION_KW);
    p.eat(K::AMP);
    // `function returns_ref function_name backup_doc_comment '('`: the
    // name is the lookahead `returns_ref` needed and `function_name`
    // reduces without one, so the take reads what stands before the
    // name and not what stands after it (probed 8.5.9: `function /** X */
    // f()` reads X, `function f /** X2 */ ()` reads false).
    p.doc_comment();
    // A function name reduces to the parameter-list state; php's clause
    // here is `"("`, not `identifier` (probed 8.5.7: `function die()`).
    name_token(p, NameSlot::Function);
    param_list(p);
    ret_type(p);
    body_block(p);
    m.complete(p, K::FUNCTION_DECL);
}

/// A function's or a closure's body, whose `{` is the one token php
/// spells after the head (`function f();` and `$f = function()
/// use ($x);` are `expecting "{"`, probed 8.5.9).
fn body_block(p: &mut Parser<'_>) {
    if p.at(K::L_CURLY) {
        block(p);
    } else {
        let d = p.unexpected_here_expecting("\"{\"");
        p.error(d);
    }
}

/// Which declaration a name slot belongs to, which is what php takes
/// there and the `, expecting …` clause it spells.
#[derive(Clone, Copy)]
enum NameSlot {
    /// `function_name: T_STRING | T_READONLY`: `readonly` is kept as a
    /// function's name since 8.1. The slot reduces to a state that can
    /// only go on to the parameter list, so php's clause is `"("` (probed
    /// 8.5.7: `function die`; 8.5.9: `function callable`).
    Function,
    /// A class, interface, trait or enum: `T_STRING` alone, so `readonly`
    /// is refused with the rest, and the clause is `identifier` (probed
    /// 8.5.7: `class static`, `interface 5`; 8.5.9: `class readonly`,
    /// `enum readonly`, `class callable`).
    ClassLike,
}

/// The declaration name: a plain identifier (a contextual-only keyword
/// remaps), never `callable`, which the lexer leaves an `IDENT` and php's
/// scanner does not. A reserved word is rejected as php rejects it, with
/// the slot's position-specific clause; the same clause covers a plain
/// end of input.
fn name_token(p: &mut Parser<'_>, slot: NameSlot) {
    let callable = ckw::is_callable_text(p.current(), p.current_text());
    if p.at(K::IDENT) && !callable {
        p.bump(K::IDENT);
    } else if ckw::is_contextual_only(p.current())
        || (matches!(slot, NameSlot::Function) && p.at(K::READONLY_KW))
    {
        p.bump_remap(K::IDENT);
    } else {
        // The name is missing or a reserved word (`class static {}`
        // reserves `static`, lsb_008 and lsb_009): php's
        // position-specific clause, still yielding to the bracket-balance
        // wording where that applies.
        let expecting = match slot {
            NameSlot::Function => "\"(\"",
            NameSlot::ClassLike => "identifier",
        };
        let d = p.unexpected_here_expecting(expecting);
        p.error(d);
    }
}

/// `abstract`, `final` or `readonly` modifiers, then a class declaration.
pub(crate) fn modifiers_then_class(p: &mut Parser<'_>, m: Marker) {
    modifiers(p);
    if p.at(K::CLASS_KW) {
        class_decl(p, m);
    } else {
        // After a class modifier php expects another modifier or `class`;
        // a `trait`, `interface`, `enum` or anything else reports the
        // fixed set `"abstract" or "final" or "readonly" or "class"`
        // (probed 8.5.7: `readonly enum`, `abstract function`, `final
        // function`).
        let d =
            p.unexpected_here_expecting("\"abstract\" or \"final\" or \"readonly\" or \"class\"");
        p.error(d);
        m.complete(p, K::ERROR);
        super::recover_to_stmt_boundary(p);
    }
}

/// A class takes the doc comment before its `{`: `T_STRING extends_from
/// implements_list backup_doc_comment '{'`, and both clauses need the
/// lookahead to know they are absent, so the `{` is read before the take
/// (probed 8.5.9: `class F extends /** D5 */ A {}` reads D5, `class F
/// extends A /** D6 */ {}` D6, `class F /** D6b */ {}` D6b). An
/// interface, an enum and an anonymous class read the same way.
pub(crate) fn class_decl(p: &mut Parser<'_>, m: Marker) {
    p.bump(K::CLASS_KW);
    name_token(p, NameSlot::ClassLike);
    extends_clause(p, false);
    implements_clause(p);
    p.doc_comment();
    class_body(p);
    m.complete(p, K::CLASS_DECL);
}

pub(crate) fn interface_decl(p: &mut Parser<'_>, m: Marker) {
    p.bump(K::INTERFACE_KW);
    name_token(p, NameSlot::ClassLike);
    extends_clause(p, true);
    p.doc_comment();
    class_body(p);
    m.complete(p, K::INTERFACE_DECL);
}

/// A trait takes before its name: `T_TRAIT T_STRING backup_doc_comment
/// '{'` has nothing between the name and the take that needs the
/// lookahead, so a comment between the name and `{` is not the trait's
/// (probed 8.5.9: `/** T */ trait T1 /** T2 */ {}` reads T).
pub(crate) fn trait_decl(p: &mut Parser<'_>, m: Marker) {
    p.bump(K::TRAIT_KW);
    p.doc_comment();
    name_token(p, NameSlot::ClassLike);
    class_body(p);
    m.complete(p, K::TRAIT_DECL);
}

pub(crate) fn enum_decl(p: &mut Parser<'_>, m: Marker) {
    p.bump(K::ENUM_KW);
    name_token(p, NameSlot::ClassLike);
    if p.at(K::COLON) {
        let b = p.start();
        p.bump(K::COLON);
        types::type_expr(p, TypeSite::WithStatic);
        b.complete(p, K::ENUM_BACKING);
    }
    // An enum takes no `extends`: php's parser refuses it as the body's
    // `expecting "{"` (probed 8.5.9), which `class_body` says.
    implements_clause(p);
    p.doc_comment();
    class_body(p);
    m.complete(p, K::ENUM_DECL);
}

/// `new class (…) extends … implements … { … }`.
pub(crate) fn anon_class_decl(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    attribute_groups(p);
    // Modifiers: `new readonly class` (8.3).
    modifiers(p);
    if !p.eat(K::CLASS_KW) {
        p.error_unexpected();
    }
    if p.at(K::L_PAREN) {
        arg_list(p);
    }
    extends_clause(p, false);
    implements_clause(p);
    p.doc_comment();
    class_body(p);
    m.complete(p, K::ANON_CLASS_DECL)
}

/// `extends …`: one name for a class, a list for an interface; a
/// second name after a class's is the body's `expecting "{"` (probed
/// 8.5.9: `class A extends B, C`).
fn extends_clause(p: &mut Parser<'_>, several: bool) {
    if p.at(K::EXTENDS_KW) {
        let m = p.start();
        p.bump(K::EXTENDS_KW);
        loop {
            if !types::name_ref(p) {
                p.error_unexpected();
                break;
            }
            if !several || !p.eat(K::COMMA) {
                break;
            }
        }
        m.complete(p, K::EXTENDS_CLAUSE);
    }
}

fn implements_clause(p: &mut Parser<'_>) {
    if p.at(K::IMPLEMENTS_KW) {
        let m = p.start();
        p.bump(K::IMPLEMENTS_KW);
        loop {
            if !types::name_ref(p) {
                p.error_unexpected();
                break;
            }
            if !p.eat(K::COMMA) {
                break;
            }
        }
        m.complete(p, K::IMPLEMENTS_CLAUSE);
    }
}

// ---------------------------------------------------------------------------
// Class bodies and members
// ---------------------------------------------------------------------------

fn class_body(p: &mut Parser<'_>) {
    let m = p.start();
    if !p.eat(K::L_CURLY) {
        // php's expected set at the body-open position collapses to
        // exactly `{`: once the name and any `extends` or `implements`
        // clause has reduced, the class body is the only continuation,
        // so even a stray `,`, `implements` or trailing name reports
        // `expecting "{"` (probed 8.5.7: `trait Foo extends`, `class B
        // implements\A`, `interface I extends A B`). A bracket closer or
        // an end of input inside a delimiter still yields the balance
        // text through the same call.
        let d = p.unexpected_here_expecting("\"{\"");
        p.error(d);
        m.complete(p, K::CLASS_BODY);
        return;
    }
    while !p.at_eof() && !p.at(K::R_CURLY) {
        if p.is_stuck() {
            let e = p.start();
            p.bump_any();
            e.complete(p, K::ERROR);
            continue;
        }
        class_member(p);
    }
    super::expect_closer(p, K::R_CURLY, None);
    m.complete(p, K::CLASS_BODY);
}

/// One member of a class body. A method's body is `;` or a block, and
/// php names both where neither stands:
///
/// ```php
/// interface I { function f(): int }
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected token "}", expecting ";" or "{" in %s on line 2
/// ```
fn class_member(p: &mut Parser<'_>) {
    let m = p.start();
    attribute_groups(p);
    // A trait use first: `use` takes no modifiers.
    if p.at(K::USE_KW) {
        trait_use(p, m);
        return;
    }
    if p.at(K::CASE_KW) {
        enum_case(p, m);
        return;
    }
    let before = p.current_span().start;
    modifiers(p);
    let modified = p.current_span().start != before;
    match p.current() {
        K::CONST_KW => {
            p.bump(K::CONST_KW);
            // An optional type (8.3): a type is present when the
            // declarator shape `NAME =` does not immediately follow.
            if !(ckw::is_member_name(p.current()) && p.nth(1) == K::EQ) {
                types::type_expr(p, TypeSite::WithStatic);
            }
            const_declarators(p, true);
            super::semicolon_expecting(p, Some("\",\" or \";\""));
            m.complete(p, K::CLASS_CONST_DECL);
        }
        K::FUNCTION_KW => {
            p.bump(K::FUNCTION_KW);
            p.eat(K::AMP);
            // As a function's: before the name (`function_decl`).
            p.doc_comment();
            member_name(p);
            param_list(p);
            ret_type(p);
            if p.at(K::L_CURLY) {
                block(p);
            } else {
                // `method_body: ';' | '{' …`: php lists both where the
                // body was due (`function f(): ?A&B;`, `function f() x`,
                // probed 8.5.9).
                super::semicolon_expecting(p, Some("\";\" or \"{\""));
            }
            m.complete(p, K::METHOD_DECL);
        }
        K::VARIABLE => {
            property_tail(p, m);
        }
        K::R_CURLY | K::EOF => {
            p.error_unexpected();
            m.abandon(p);
        }
        // A typed property: anything that can start a type.
        _ if types::at_type_start(p) => {
            types::type_expr(p, TypeSite::WithoutStatic);
            property_tail(p, m);
        }
        _ => {
            // A token that starts no member: php spells `"function"` at
            // a member's start and `variable` after modifiers, where a
            // property is the one continuation it lists (`class A { = }`,
            // `class A { public ; }`, probed 8.5.9).
            let d = if modified {
                p.unexpected_here_expecting("variable")
            } else {
                p.unexpected_here_expecting("\"function\"")
            };
            p.error(d);
            m.abandon(p);
            let e = p.start();
            while !p.at_eof()
                && !p.at(K::R_CURLY)
                && !p.at(K::SEMICOLON)
                && !matches!(
                    p.current(),
                    K::FUNCTION_KW | K::CONST_KW | K::USE_KW | K::CASE_KW | K::VARIABLE
                )
            {
                p.bump_any();
            }
            p.eat(K::SEMICOLON);
            e.complete(p, K::ERROR);
        }
    }
}

/// The declarators, and the optional accessors, of a property
/// declaration, after any type. `m` covers the attributes, modifiers and
/// type. Each declarator takes the doc comment after its last token,
/// php's `property: T_VARIABLE backup_doc_comment | T_VARIABLE '=' expr
/// backup_doc_comment`, where the `=`, the operator or the `{` that may
/// follow is the lookahead already read (probed 8.5.9: `/** P */ public
/// $x, $y;` gives P to `$x` alone, `public $z /** P3 */;` P3 to `$z`,
/// `public $w = 1 /** P4 */;` P4 to `$w`). The constant declarators of
/// [`const_declarators`] take the same way; `tests/doc_comment.rs` holds
/// both to the measurement and the witness below holds the engine to
/// it end to end.
///
/// ```php
/// class M { /** P */ public $x, $y; /** K */ const A = 1, B = 2; } foreach (['x', 'y'] as $p) var_dump((new ReflectionProperty('M', $p))->getDocComment()); foreach (['A', 'B'] as $c) var_dump((new ReflectionClassConstant('M', $c))->getDocComment());
/// ```
/// ```expect
/// string(8) "/** P */"
/// bool(false)
/// string(8) "/** K */"
/// bool(false)
/// ```
fn property_tail(p: &mut Parser<'_>, m: Marker) {
    loop {
        let d = p.start();
        if p.at(K::VARIABLE) {
            p.bump(K::VARIABLE);
            if p.eat(K::EQ) && !expr::expr(p) {
                p.error_unexpected();
            }
            p.doc_comment();
            d.complete(p, K::PROPERTY_DECLARATOR);
        } else {
            d.abandon(p);
            // php's bison tables print the position-specific expected set
            // here (`public $a, int $b` and `public $a, }` are
            // `, expecting variable`, probed 8.5.7 typed_properties_025);
            // an end of input inside the class body yields to the
            // `Unclosed '{'` balance text through the same call.
            let diag = p.unexpected_here_expecting("variable");
            p.error(diag);
            break;
        }
        if !p.eat(K::COMMA) {
            break;
        }
    }
    if p.at(K::L_CURLY) {
        property_hook_list(p);
    } else {
        // Another declarator may follow (`public $x }` is `expecting ","
        // or ";"`, probed 8.5.9).
        super::semicolon_expecting(p, Some("\",\" or \";\""));
    }
    m.complete(p, K::PROPERTY_DECL);
}

/// `{ get; set => …; }`, a property's accessors (8.4).
pub(crate) fn property_hook_list(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(K::L_CURLY);
    while !p.at_eof() && !p.at(K::R_CURLY) {
        let h = p.start();
        attribute_groups(p);
        modifiers(p);
        p.eat(K::AMP);
        // `property_hook_modifiers returns_ref T_STRING backup_doc_comment`:
        // before the name, as a function's (probed 8.5.9: `{ /** HG */
        // get { … } }` gives HG to the accessor and nothing to the
        // property).
        p.doc_comment();
        if ckw::is_hook_name(p.current()) || p.current().is_keyword() {
            if p.at(K::IDENT) {
                p.bump(K::IDENT);
            } else {
                // `set` lexes as SET_KW; another keyword arrives only in
                // broken code: remapped, and the compiler rejects the
                // name.
                p.bump_remap(K::IDENT);
            }
        } else {
            // A hook is named by an identifier (`{ ; }` and `{ = }` are
            // `expecting identifier`, probed 8.5.9).
            let diag = p.unexpected_here_expecting("identifier");
            p.error(diag);
            h.abandon(p);
            let e = p.start();
            p.bump_any();
            e.complete(p, K::ERROR);
            continue;
        }
        if p.at(K::L_PAREN) {
            param_list(p);
        }
        if p.at(K::L_CURLY) {
            block(p);
        } else if p.at(K::FAT_ARROW) {
            let b = p.start();
            p.bump(K::FAT_ARROW);
            expr::expr(p);
            semicolon(p);
            b.complete(p, K::HOOK_ARROW_BODY);
        } else {
            // After the hook's name php lists its three continuations
            // (`{ get }` is `expecting "=>" or ";" or "{"`, probed 8.5.9).
            super::semicolon_expecting(p, Some("\"=>\" or \";\" or \"{\""));
        }
        h.complete(p, K::PROPERTY_HOOK);
    }
    super::expect_closer(p, K::R_CURLY, None);
    m.complete(p, K::PROPERTY_HOOK_LIST);
}

/// `T_CASE backup_doc_comment identifier`: the take reduces once `case`
/// is shifted, before the name is read, so a comment between `case` and
/// the name is the next case's (probed 8.5.9: `case /** C2 */ B = 1; case
/// C = 2;` gives false to B and C2 to C).
fn enum_case(p: &mut Parser<'_>, m: Marker) {
    p.doc_comment();
    p.bump(K::CASE_KW);
    member_name(p);
    if p.eat(K::EQ) && !expr::expr(p) {
        p.error_unexpected();
    }
    super::semicolon_expecting(p, Some("\";\""));
    m.complete(p, K::ENUM_CASE_DECL);
}

fn trait_use(p: &mut Parser<'_>, m: Marker) {
    p.bump(K::USE_KW);
    loop {
        if !types::name_ref(p) {
            p.error_unexpected();
            break;
        }
        if !p.eat(K::COMMA) {
            break;
        }
    }
    if p.at(K::L_CURLY) {
        let list = p.start();
        p.bump(K::L_CURLY);
        while !p.at_eof() && !p.at(K::R_CURLY) {
            // Zend maps `?>` to `;` everywhere in scripting, so a close
            // tag is a legal adaptation terminator (`x as y?><?= as z;`,
            // the short_echo_as_identifier shape): consumed as glue, and
            // the next entry begins at the reopened tag.
            if p.at(K::CLOSE_TAG) || p.at(K::INLINE_HTML) || p.at(K::OPEN_TAG) {
                p.bump_any();
                continue;
            }
            trait_adaptation(p);
        }
        super::expect_closer(p, K::R_CURLY, None);
        list.complete(p, K::TRAIT_ADAPTATION_LIST);
    } else {
        // Another trait, the terminator or the adaptations may follow
        // (`use T }` is `expecting "," or ";" or "{"`, probed 8.5.9).
        super::semicolon_expecting(p, Some("\",\" or \";\" or \"{\""));
    }
    m.complete(p, K::TRAIT_USE_DECL);
}

fn trait_adaptation(p: &mut Parser<'_>) {
    let m = p.start();
    // `TraitMethodRef`: `(Name '::')? member`.
    let r = p.start();
    let r_start = p.current_span().start;
    if (p.at(K::IDENT)
        || p.at(K::NAME_QUALIFIED)
        || p.at(K::NAME_FULLY_QUALIFIED)
        || p.at(K::NAME_RELATIVE)
        // `static::m`: `static` is a keyword, not an IDENT (`self` and
        // `parent` already match as IDENT). Accepting it lets the
        // compiler reject the reserved trait name with php's wording
        // rather than a stray `unexpected "::"` here.
        || p.at(K::STATIC_KW))
        && p.nth(1) == K::COLON_COLON
    {
        p.bump_any();
        p.bump(K::COLON_COLON);
    }
    let qualified = p.current_span().start != r_start;
    member_name(p);
    r.complete(p, K::TRAIT_METHOD_REF);
    if p.at(K::INSTEADOF_KW) && !qualified {
        // `insteadof` takes a qualified method (`f insteadof B` is
        // `unexpected token "insteadof", expecting "::"`, probed 8.5.9).
        let diag = p.unexpected_here_expecting("\"::\"");
        p.error(diag);
    }
    if p.at(K::INSTEADOF_KW) {
        p.bump(K::INSTEADOF_KW);
        loop {
            if !types::name_ref(p) {
                p.error_unexpected();
                break;
            }
            if !p.eat(K::COMMA) {
                break;
            }
        }
        super::semicolon_expecting(p, Some("\";\""));
        m.complete(p, K::INSTEADOF_ADAPTATION);
    } else if p.at(K::AS_KW) {
        p.bump(K::AS_KW);
        // php-src `member_modifier`: a visibility, or `static`, `abstract`,
        // `final` or `readonly` (`m as final;` changes the modifier, no
        // alias).
        if matches!(
            p.current(),
            K::PUBLIC_KW
                | K::PROTECTED_KW
                | K::PRIVATE_KW
                | K::STATIC_KW
                | K::ABSTRACT_KW
                | K::FINAL_KW
                | K::READONLY_KW
        ) && !(p.nth(1) == K::SEMICOLON && p.current() == K::IDENT)
        {
            p.bump_any();
            if ckw::is_member_name(p.current()) {
                member_name(p);
            }
        } else if ckw::is_member_name(p.current()) {
            member_name(p);
        } else {
            // `f as ;` wants a modifier or an alias (probed 8.5.9).
            p.error_unexpected();
        }
        super::semicolon_expecting(p, Some("\";\""));
        m.complete(p, K::AS_ADAPTATION);
    } else {
        // After a bare method php still expects its `::`, after a
        // qualified one its `as` (`{ f ;` is `expecting "::"`, `{ T::f
        // ;` `expecting "as"`, probed 8.5.9).
        let diag = if p.has_error_at_or_after(p.current_span().start) {
            None
        } else if qualified {
            Some(p.unexpected_here_expecting("\"as\""))
        } else {
            Some(p.unexpected_here_expecting("\"::\""))
        };
        if let Some(diag) = diag {
            p.error(diag);
        }
        // Consumed to `;` or `}` for progress.
        let e = p.start();
        while !p.at_eof() && !p.at(K::SEMICOLON) && !p.at(K::R_CURLY) {
            p.bump_any();
        }
        p.eat(K::SEMICOLON);
        e.complete(p, K::ERROR);
        m.complete(p, K::INSTEADOF_ADAPTATION);
    }
}

/// A member name: an identifier or a semi-reserved keyword, remapped.
pub(crate) fn member_name(p: &mut Parser<'_>) {
    let m = p.start();
    if p.at(K::IDENT) {
        p.bump(K::IDENT);
    } else if ckw::is_member_name(p.current()) {
        p.bump_remap(K::IDENT);
    } else {
        // `<?=` where an identifier is required gets Zend's targeted
        // `zend_lex_tstring` text, `Cannot use "<?=" as an identifier`, a
        // ParseError rendered verbatim (probed 8.5.7,
        // short_echo_as_identifier). Everything else keeps the
        // unexpected-token wording.
        let d = if p.at(K::OPEN_TAG_ECHO) {
            let (span, line) = (p.current_span(), p.line_here());
            Diagnostic::new(span, line, "Cannot use \"<?=\" as an identifier")
        } else {
            p.unexpected_here()
        };
        p.error(d);
        m.abandon(p);
        return;
    }
    m.complete(p, K::MEMBER_NAME);
}

/// The `const` declarators, `NAME = expr (, NAME = expr)*`. A class
/// constant takes the semi-reserved set; a top-level constant takes a
/// plain identifier only (php-src `const_list` against
/// `class_const_list`: `const FOR = 1;` is a parse error in php). Each
/// declarator takes the doc comment after its expression, php's
/// `class_const_decl` and `const_decl`, both `… '=' expr
/// backup_doc_comment` (probed 8.5.9: `/** K */ const C = 1, D = 2;`
/// gives K to C alone and `const E = 1 /** K3 */, F = 2;` K3 to E).
pub(crate) fn const_declarators(p: &mut Parser<'_>, semi_reserved: bool) {
    loop {
        let d = p.start();
        // A top-level name is `T_STRING`, which `callable` is not to
        // php's scanner (`const callable = 1;` is `expecting
        // identifier`, probed 8.5.9); a class constant takes it through
        // `semi_reserved`.
        let acceptable = if semi_reserved {
            ckw::is_member_name(p.current())
        } else {
            (p.at(K::IDENT) && !ckw::is_callable_text(K::IDENT, p.current_text()))
                || ckw::is_contextual_only(p.current())
        };
        if acceptable {
            member_name(p);
            if !p.eat(K::EQ) {
                // `const X ;` is `unexpected token ";"` (probed 8.5.9).
                p.error_unexpected();
            } else if !expr::expr(p) {
                p.error_unexpected();
            }
            p.doc_comment();
            d.complete(p, K::CONST_DECLARATOR);
        } else {
            d.abandon(p);
            // A top-level `const` name is a plain identifier, and php
            // spells the clause when it is missing (`const exit`, `const
            // 5` and `const for` are `expecting identifier`, probed
            // 8.5.7). A class constant takes the semi-reserved set, whose
            // expected phrase php does not print (`class C { const 5 = 1;
            // }` carries no clause), so that path keeps the bare wording.
            if semi_reserved {
                p.error_unexpected();
            } else {
                let d = p.unexpected_here_expecting("identifier");
                p.error(d);
            }
            break;
        }
        if !p.eat(K::COMMA) {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Parameters, return types, closures
// ---------------------------------------------------------------------------

pub(crate) fn param_list(p: &mut Parser<'_>) {
    let m = p.start();
    if !p.eat(K::L_PAREN) {
        // A function, closure, arrow-function or method head reduces to
        // the state whose sole continuation is the parameter list, so php
        // reports `"("` here (probed 8.5.7: `function die()`, `function
        // foo bar`, `fn 5 =>`, `$f = function die {}`; a plain end of
        // input spells the clause too). A bad member name already errored
        // one token back without consuming it, so this stands beside
        // that earlier, clause-free diagnostic at the same span, the
        // first emitted first, which keeps `public function 5()`'s bare
        // `unexpected integer "5"` in front.
        let d = p.unexpected_here_expecting("\"(\"");
        p.error(d);
        m.complete(p, K::PARAM_LIST);
        return;
    }
    if p.at(K::CLOSE_TAG) {
        let d = p.unexpected_here_expecting("variable");
        p.error(d);
    }
    while !p.at_eof() && !p.at(K::R_PAREN) && !p.at(K::CLOSE_TAG) {
        let param = p.start();
        attribute_groups(p);
        modifiers(p); // promotion (8.0), set-visibility (8.4)
        if types::at_type_start(p) {
            types::type_expr(p, TypeSite::WithoutStatic);
        }
        // `&` (by reference) and `...` (variadic): `&` here is by
        // reference because an intersection's `&` was consumed inside
        // the type. php's scanner splits `&` by what follows, and one
        // that no variable or `...` follows is itself the unexpected
        // token (`function f(& ;` is `unexpected token "&", expecting
        // variable`, probed 8.5.9).
        if p.at(K::AMP) && matches!(p.nth(1), K::VARIABLE | K::DOT_DOT_DOT) {
            p.bump(K::AMP);
        }
        p.eat(K::DOT_DOT_DOT);
        // `… T_VARIABLE backup_doc_comment` in both `parameter` rules:
        // the take reduces once the variable is shifted, before what
        // follows is read, so it is what stands before the variable
        // (probed 8.5.9: `fn(/** A */ $x) => 1` takes A and `fn($x /** B
        // */) => 1` leaves B for the next declaration).
        p.doc_comment();
        if p.at(K::VARIABLE) {
            p.bump(K::VARIABLE);
        } else {
            // A parameter is named by a variable, and php spells it
            // whatever stood before (`function f( ;`, `function f(int ;`,
            // `function f($x, ;` are `expecting variable`, probed 8.5.9).
            let d = p.unexpected_here_expecting("variable");
            p.error(d);
            param.abandon(p);
            // Progress: one token consumed unless at a list boundary.
            if !p.at(K::R_PAREN) && !p.at(K::COMMA) && !p.at_eof() {
                let e = p.start();
                p.bump_any();
                e.complete(p, K::ERROR);
            }
            if !p.eat(K::COMMA) {
                break;
            }
            continue;
        }
        if p.eat(K::EQ) && !expr::expr(p) {
            p.error_unexpected();
        }
        if p.at(K::L_CURLY) {
            // The accessors of a promoted parameter's property (8.4).
            property_hook_list(p);
        }
        param.complete(p, K::PARAM);
        if !p.eat(K::COMMA) {
            // After a complete parameter php's expected set collapses to
            // `)` (the `,` continuation and the closer reduce together;
            // probed 8.5.7: `function f($a string)`, `function f($a/)`).
            // The clause is pinned at the close position; it stands
            // beside the `expect_closer` or body-open diagnostic at the
            // same token, so the first carries php's clause.
            if !p.at(K::R_PAREN) && !p.at_eof() && !p.at(K::CLOSE_TAG) {
                let d = p.unexpected_here_expecting("\")\"");
                p.error(d);
            }
            break;
        }
    }
    super::expect_closer(p, K::R_PAREN, Some("\")\""));
    m.complete(p, K::PARAM_LIST);
}

pub(crate) fn ret_type(p: &mut Parser<'_>) {
    if p.at(K::COLON) {
        let m = p.start();
        p.bump(K::COLON);
        types::type_expr(p, TypeSite::WithStatic);
        m.complete(p, K::RET_TYPE);
    }
}

/// `function (…) use (…): T { … }`; `m` covers `static` or the attributes
/// when present.
pub(crate) fn closure_expr(p: &mut Parser<'_>, m: Option<Marker>) -> CompletedMarker {
    let m = m.unwrap_or_else(|| p.start());
    p.bump(K::FUNCTION_KW);
    p.eat(K::AMP);
    // `function returns_ref backup_doc_comment '('`: the `(` is the
    // lookahead `returns_ref` needed (probed 8.5.9: `function /** F */ ()
    // {}` reads F, `function () /** G */ {}` false).
    p.doc_comment();
    param_list(p);
    if p.at(K::USE_KW) {
        let u = p.start();
        p.bump(K::USE_KW);
        if p.eat(K::L_PAREN) {
            let mut after_comma = false;
            while !p.at_eof() && !p.at(K::R_PAREN) {
                let v = p.start();
                let by_ref = p.eat(K::AMP);
                if p.at(K::VARIABLE) {
                    p.bump(K::VARIABLE);
                    v.complete(p, K::CLOSURE_USE_VAR);
                } else {
                    // php's clause at the first use variable is the
                    // three-way set, both `&` spellings included (`use
                    // ( ;` is `expecting variable or "&" or token "&"`);
                    // after a comma the closer alone (probed 8.5.9).
                    let diag = if after_comma {
                        p.unexpected_here_expecting("\")\"")
                    } else if by_ref {
                        p.unexpected_here_expecting("variable")
                    } else {
                        p.unexpected_here_expecting("variable or \"&\" or token \"&\"")
                    };
                    p.error(diag);
                    v.abandon(p);
                    break;
                }
                if !p.eat(K::COMMA) {
                    break;
                }
                after_comma = true;
            }
            super::expect_closer(p, K::R_PAREN, Some("\")\""));
        } else {
            let diag = p.unexpected_here_expecting("\"(\"");
            p.error(diag);
        }
        u.complete(p, K::CLOSURE_USE_CLAUSE);
    }
    ret_type(p);
    body_block(p);
    m.complete(p, K::CLOSURE_EXPR)
}

/// `fn (…) => expr`: the body binds lowest.
pub(crate) fn arrow_fn_expr(p: &mut Parser<'_>, m: Option<Marker>) -> CompletedMarker {
    let m = m.unwrap_or_else(|| p.start());
    p.bump(K::FN_KW);
    p.eat(K::AMP);
    // As a closure's: `fn returns_ref backup_doc_comment '('`.
    p.doc_comment();
    param_list(p);
    ret_type(p);
    if !p.eat(K::FAT_ARROW) {
        // `fn() ;` is `expecting "=>"` (probed 8.5.9).
        let d = p.unexpected_here_expecting("\"=>\"");
        p.error(d);
    }
    if !expr::expr(p) {
        p.error_unexpected();
    }
    m.complete(p, K::ARROW_FN_EXPR)
}

// ---------------------------------------------------------------------------
// Call-site arguments
// ---------------------------------------------------------------------------

/// A `( … )` argument list: named arguments, spreads, the first-class
/// callable `(...)` placeholder, plain expressions. The placeholder is
/// php's `'(' T_ELLIPSIS ')'`, the whole list, so after an argument the
/// `...` is a spread whose expression is missing:
///
/// ```php
/// max(1, ...);
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected token ")" in %s on line 2
/// ```
pub(crate) fn arg_list(p: &mut Parser<'_>) {
    let m = p.start();
    if !p.eat(K::L_PAREN) {
        let d = p.unexpected_here_expecting("\"(\"");
        p.error(d);
        m.complete(p, K::ARG_LIST);
        return;
    }
    // After a complete argument php's expected set at a dangling or
    // trailing comma collapses to `)` (`foo($a,,)` and `foo($a,,$b)` are
    // `expecting ")"`), but at the first-argument position it prints no
    // clause (`foo(,)`, `foo(*)`: the argument-start set is too large to
    // spell). Whether an argument has been consumed decides whether the
    // plain-expression failure pins php's clause.
    let mut after_comma = false;
    // `?>` is never consumed as an argument: it is the `;` php's scanner
    // makes of it, and at the first argument's position php spells no
    // clause (`f( ?>`, probed 8.5.9).
    if p.at(K::CLOSE_TAG) {
        p.error_unexpected();
    }
    while !p.at_eof() && !p.at(K::R_PAREN) && !p.at(K::CLOSE_TAG) {
        if p.at(K::DOT_DOT_DOT) {
            if p.nth(1) == K::R_PAREN && !after_comma {
                // The first-class callable `f(...)` (8.1), which is the
                // whole list or nothing: `f($a, ...)` is php's `T_ELLIPSIS
                // expr` missing its expression, `unexpected token ")"`.
                let a = p.start();
                p.bump(K::DOT_DOT_DOT);
                a.complete(p, K::VARIADIC_PLACEHOLDER);
            } else {
                let a = p.start();
                p.bump(K::DOT_DOT_DOT);
                if !expr::expr(p) {
                    p.error_unexpected();
                }
                a.complete(p, K::SPREAD_ARG);
            }
        } else if ckw::is_member_name(p.current()) && p.nth(1) == K::COLON && p.nth(2) != K::COLON {
            // A named argument `name: expr` (the `name::` case is a scoped
            // access, excluded by the third-token test above).
            let a = p.start();
            if p.at(K::IDENT) {
                p.bump(K::IDENT);
            } else {
                p.bump_remap(K::IDENT);
            }
            p.bump(K::COLON);
            if !expr::expr(p) {
                p.error_unexpected();
            }
            a.complete(p, K::NAMED_ARG);
        } else if !expr::expr(p) {
            // A dangling comma (`foo($a,,`) reports `expecting ")"`; the
            // first-argument position does not (php spells no clause there).
            if after_comma {
                let d = p.unexpected_here_expecting("\")\"");
                p.error(d);
            } else {
                p.error_unexpected();
            }
            // Progress, then resynchronised at `,` or `)`.
            if !p.at(K::R_PAREN) && !p.at(K::COMMA) && !p.at_eof() {
                let e = p.start();
                p.bump_any();
                e.complete(p, K::ERROR);
            }
        }
        if !p.eat(K::COMMA) {
            // A missing `,` between arguments: the repair, when the next
            // token can begin another argument.
            if !p.at(K::R_PAREN) && !p.at_eof() && expr::at_expr_start(p) {
                super::missing_comma(p, "\")\"");
                after_comma = true;
                continue;
            }
            break;
        }
        after_comma = true;
    }
    super::expect_closer(p, K::R_PAREN, Some("\")\""));
    m.complete(p, K::ARG_LIST);
}
