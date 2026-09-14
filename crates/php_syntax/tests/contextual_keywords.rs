//! The enumerated contextual-keyword file (design §4.9): the decision
//! procedure is one place, `grammar::ckw`, and this file enumerates every
//! case, one keyword-or-name decision per fixture with php's verdict in a
//! comment. Under test: `is_member_name`, `is_name_start`,
//! `is_contextual_only`, `enum_is_declaration`, `readonly_is_call`,
//! `static_in_stmt` and `is_hook_name`. Read at Mahout `1b3104a8` and
//! regenerated once against the arena's dump.

mod support;

use expect_test::expect;
use support::check;

// ---------------------------------------------------------------------------
// Semi-reserved member names
// ---------------------------------------------------------------------------

#[test]
fn member_list_method_call() {
    // php -l ok: semi-reserved after `->`.
    check(
        "<?php $x->list();",
        expect![[r#"
        SOURCE_FILE@0..17
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..17
            CALL_EXPR@6..16
              MEMBER_ACCESS_EXPR@6..14
                SIMPLE_VAR_EXPR@6..8
                  VARIABLE@6..8 "$x"
                ARROW@8..10 "->"
                MEMBER_NAME@10..14
                  IDENT@10..14 "list"
              ARG_LIST@14..16
                L_PAREN@14..15 "("
                R_PAREN@15..16 ")"
            SEMICOLON@16..17 ";"
    "#]],
    );
}

#[test]
fn member_for_property() {
    // php -l ok: keyword as property name.
    check(
        "<?php $x->for;",
        expect![[r#"
        SOURCE_FILE@0..14
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..14
            MEMBER_ACCESS_EXPR@6..13
              SIMPLE_VAR_EXPR@6..8
                VARIABLE@6..8 "$x"
              ARROW@8..10 "->"
              MEMBER_NAME@10..13
                IDENT@10..13 "for"
            SEMICOLON@13..14 ";"
    "#]],
    );
}

#[test]
fn scoped_list_method_call() {
    // php -l ok: semi-reserved after `::`.
    check(
        "<?php Foo::list();",
        expect![[r#"
        SOURCE_FILE@0..18
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..18
            CALL_EXPR@6..17
              SCOPED_ACCESS_EXPR@6..15
                IDENT@6..9 "Foo"
                COLON_COLON@9..11 "::"
                MEMBER_NAME@11..15
                  IDENT@11..15 "list"
              ARG_LIST@15..17
                L_PAREN@15..16 "("
                R_PAREN@16..17 ")"
            SEMICOLON@17..18 ";"
    "#]],
    );
}

#[test]
fn scoped_match_constant() {
    // php -l ok: `match` is reserved as a NAME but fine as a member.
    check(
        "<?php Foo::match;",
        expect![[r#"
        SOURCE_FILE@0..17
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..17
            SCOPED_ACCESS_EXPR@6..16
              IDENT@6..9 "Foo"
              COLON_COLON@9..11 "::"
              MEMBER_NAME@11..16
                IDENT@11..16 "match"
            SEMICOLON@16..17 ";"
    "#]],
    );
}

#[test]
fn class_const_semi_reserved_name() {
    // php -l ok: `const FOR = 1;` inside a class (member-name grammar).
    check(
        "<?php class C { const FOR = 1; }",
        expect![[r#"
            SOURCE_FILE@0..32
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..32
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..32
                  L_CURLY@14..15 "{"
                  CLASS_CONST_DECL@16..30
                    CONST_KW@16..21 "const"
                    CONST_DECLARATOR@22..29
                      MEMBER_NAME@22..25
                        IDENT@22..25 "FOR"
                      EQ@26..27 "="
                      LITERAL_EXPR@28..29
                        INT@28..29 "1"
                    SEMICOLON@29..30 ";"
                  R_CURLY@31..32 "}"
        "#]],
    );
}

// A top-level `const FOR = 1;` is a parse error in php (`unexpected token
// "for", expecting identifier`): the semi-reserved set is the class
// constant's alone.
#[test]
fn top_level_const_keyword_name_rejected() {
    check(
        "<?php const FOR = 1;",
        expect![[r#"
            SOURCE_FILE@0..20
              OPEN_TAG@0..6 "<?php "
              CONST_STMT@6..11
                CONST_KW@6..11 "const"
              FOR_STMT@12..17
                FOR_KW@12..15 "FOR"
                ERROR@16..17
                  EQ@16..17 "="
              EXPR_STMT@18..20
                LITERAL_EXPR@18..19
                  INT@18..19 "1"
                SEMICOLON@19..20 ";"
            error@12..15 syntax error, unexpected token "for", expecting identifier
            error@16..17 syntax error, unexpected token "=", expecting "("
            error@16..17 syntax error, unexpected token "=", expecting end of file
        "#]],
    );
}

#[test]
fn methods_named_keywords() {
    // php -l ok: `function list/for/match() {}` as METHODS.
    check(
        "<?php class C { function list() {} function for() {} function match() {} }",
        expect![[r#"
            SOURCE_FILE@0..74
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..74
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..74
                  L_CURLY@14..15 "{"
                  METHOD_DECL@16..34
                    FUNCTION_KW@16..24 "function"
                    MEMBER_NAME@25..29
                      IDENT@25..29 "list"
                    PARAM_LIST@29..31
                      L_PAREN@29..30 "("
                      R_PAREN@30..31 ")"
                    BLOCK@32..34
                      L_CURLY@32..33 "{"
                      R_CURLY@33..34 "}"
                  METHOD_DECL@35..52
                    FUNCTION_KW@35..43 "function"
                    MEMBER_NAME@44..47
                      IDENT@44..47 "for"
                    PARAM_LIST@47..49
                      L_PAREN@47..48 "("
                      R_PAREN@48..49 ")"
                    BLOCK@50..52
                      L_CURLY@50..51 "{"
                      R_CURLY@51..52 "}"
                  METHOD_DECL@53..72
                    FUNCTION_KW@53..61 "function"
                    MEMBER_NAME@62..67
                      IDENT@62..67 "match"
                    PARAM_LIST@67..69
                      L_PAREN@67..68 "("
                      R_PAREN@68..69 ")"
                    BLOCK@70..72
                      L_CURLY@70..71 "{"
                      R_CURLY@71..72 "}"
                  R_CURLY@73..74 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// enum contextuality
// ---------------------------------------------------------------------------

#[test]
fn enum_declaration() {
    check(
        "<?php enum E {}",
        expect![[r#"
            SOURCE_FILE@0..15
              OPEN_TAG@0..6 "<?php "
              ENUM_DECL@6..15
                ENUM_KW@6..10 "enum"
                IDENT@11..12 "E"
                CLASS_BODY@13..15
                  L_CURLY@13..14 "{"
                  R_CURLY@14..15 "}"
        "#]],
    );
}

#[test]
fn enum_call() {
    // php -l ok: call to a function named enum.
    check(
        "<?php enum(1);",
        expect![[r#"
        SOURCE_FILE@0..14
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..14
            CALL_EXPR@6..13
              IDENT@6..10 "enum"
              ARG_LIST@10..13
                L_PAREN@10..11 "("
                LITERAL_EXPR@11..12
                  INT@11..12 "1"
                R_PAREN@12..13 ")"
            SEMICOLON@13..14 ";"
    "#]],
    );
}

#[test]
fn enum_member_access() {
    // php -l ok: `enum->prop;` — constant fetch then property access.
    check(
        "<?php enum->prop;",
        expect![[r#"
        SOURCE_FILE@0..17
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..17
            MEMBER_ACCESS_EXPR@6..16
              IDENT@6..10 "enum"
              ARROW@10..12 "->"
              MEMBER_NAME@12..16
                IDENT@12..16 "prop"
            SEMICOLON@16..17 ";"
    "#]],
    );
}

#[test]
fn enum_bare_constant() {
    // php -l ok: `$x = enum;` — `enum` not followed by a name is a
    // plain constant fetch (the php-src lexer hack, as parser lookahead).
    check(
        "<?php $x = enum;",
        expect![[r#"
            SOURCE_FILE@0..16
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..16
                ASSIGN_EXPR@6..15
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$x"
                  EQ@9..10 "="
                  IDENT@11..15 "enum"
                SEMICOLON@15..16 ";"
        "#]],
    );
}

#[test]
fn enum_named_enum() {
    // php -l ok: `enum enum {}` — contextual keyword as the enum's name.
    check(
        "<?php enum enum {}",
        expect![[r#"
            SOURCE_FILE@0..18
              OPEN_TAG@0..6 "<?php "
              ENUM_DECL@6..18
                ENUM_KW@6..10 "enum"
                IDENT@11..15 "enum"
                CLASS_BODY@16..18
                  L_CURLY@16..17 "{"
                  R_CURLY@17..18 "}"
        "#]],
    );
}

#[test]
fn enum_named_reserved_word_rejected() {
    // php -l: "unexpected token \"list\", expecting identifier" — reserved
    // words are not class-like names; both sides reject.
    check(
        "<?php enum list {}",
        expect![[r#"
            SOURCE_FILE@0..18
              OPEN_TAG@0..6 "<?php "
              ENUM_DECL@6..11
                ENUM_KW@6..10 "enum"
                CLASS_BODY@11..11
              EXPR_STMT@11..15
                LIST_EXPR@11..15
                  LIST_KW@11..15 "list"
              BLOCK@16..18
                L_CURLY@16..17 "{"
                R_CURLY@17..18 "}"
            error@11..15 syntax error, unexpected token "list", expecting identifier
            error@11..15 syntax error, unexpected token "list", expecting "{"
            error@16..17 syntax error, unexpected token "{", expecting "("
        "#]],
    );
}

// ---------------------------------------------------------------------------
// readonly contextuality
// ---------------------------------------------------------------------------

#[test]
fn readonly_class_modifier() {
    check(
        "<?php readonly class C {}",
        expect![[r#"
            SOURCE_FILE@0..25
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..25
                READONLY_KW@6..14 "readonly"
                CLASS_KW@15..20 "class"
                IDENT@21..22 "C"
                CLASS_BODY@23..25
                  L_CURLY@23..24 "{"
                  R_CURLY@24..25 "}"
        "#]],
    );
}

#[test]
fn readonly_call() {
    // `php -l` accepts `readonly(1);`: a call, php keeping the functions
    // named readonly (its scanner emits T_STRING before `(`).
    check(
        "<?php readonly(1);",
        expect![[r#"
        SOURCE_FILE@0..18
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..18
            CALL_EXPR@6..17
              IDENT@6..14 "readonly"
              ARG_LIST@14..17
                L_PAREN@14..15 "("
                LITERAL_EXPR@15..16
                  INT@15..16 "1"
                R_PAREN@16..17 ")"
            SEMICOLON@17..18 ";"
    "#]],
    );
}

// `php -l` accepts `function readonly() {}`: the `readonly(`-is-a-name
// rule covers the declaration too.
#[test]
fn function_named_readonly() {
    check(
        "<?php function readonly() {}",
        expect![[r#"
            SOURCE_FILE@0..28
              OPEN_TAG@0..6 "<?php "
              FUNCTION_DECL@6..28
                FUNCTION_KW@6..14 "function"
                IDENT@15..23 "readonly"
                PARAM_LIST@23..25
                  L_PAREN@23..24 "("
                  R_PAREN@24..25 ")"
                BLOCK@26..28
                  L_CURLY@26..27 "{"
                  R_CURLY@27..28 "}"
        "#]],
    );
}

#[test]
fn readonly_property_modifier() {
    check(
        "<?php class C { public readonly int $x; }",
        expect![[r#"
            SOURCE_FILE@0..41
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..41
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..41
                  L_CURLY@14..15 "{"
                  PROPERTY_DECL@16..39
                    PUBLIC_KW@16..22 "public"
                    READONLY_KW@23..31 "readonly"
                    NAMED_TYPE@32..35
                      IDENT@32..35 "int"
                    PROPERTY_DECLARATOR@36..38
                      VARIABLE@36..38 "$x"
                    SEMICOLON@38..39 ";"
                  R_CURLY@40..41 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// static: the four readings
// ---------------------------------------------------------------------------

#[test]
fn static_var_declaration() {
    check(
        "<?php static $x = 1;",
        expect![[r#"
            SOURCE_FILE@0..20
              OPEN_TAG@0..6 "<?php "
              STATIC_VAR_STMT@6..20
                STATIC_KW@6..12 "static"
                STATIC_VAR_DECLARATOR@13..19
                  VARIABLE@13..15 "$x"
                  EQ@16..17 "="
                  LITERAL_EXPR@18..19
                    INT@18..19 "1"
                SEMICOLON@19..20 ";"
        "#]],
    );
}

#[test]
fn static_scoped_call() {
    check(
        "<?php static::foo();",
        expect![[r#"
            SOURCE_FILE@0..20
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..20
                CALL_EXPR@6..19
                  SCOPED_ACCESS_EXPR@6..17
                    IDENT@6..12 "static"
                    COLON_COLON@12..14 "::"
                    MEMBER_NAME@14..17
                      IDENT@14..17 "foo"
                  ARG_LIST@17..19
                    L_PAREN@17..18 "("
                    R_PAREN@18..19 ")"
                SEMICOLON@19..20 ";"
        "#]],
    );
}

#[test]
fn static_closure() {
    check(
        "<?php static function () {};",
        expect![[r#"
            SOURCE_FILE@0..28
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..28
                CLOSURE_EXPR@6..27
                  STATIC_KW@6..12 "static"
                  FUNCTION_KW@13..21 "function"
                  PARAM_LIST@22..24
                    L_PAREN@22..23 "("
                    R_PAREN@23..24 ")"
                  BLOCK@25..27
                    L_CURLY@25..26 "{"
                    R_CURLY@26..27 "}"
                SEMICOLON@27..28 ";"
        "#]],
    );
}

#[test]
fn static_arrow_fn() {
    check(
        "<?php static fn() => 1;",
        expect![[r#"
            SOURCE_FILE@0..23
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..23
                ARROW_FN_EXPR@6..22
                  STATIC_KW@6..12 "static"
                  FN_KW@13..15 "fn"
                  PARAM_LIST@15..17
                    L_PAREN@15..16 "("
                    R_PAREN@16..17 ")"
                  FAT_ARROW@18..20 "=>"
                  LITERAL_EXPR@21..22
                    INT@21..22 "1"
                SEMICOLON@22..23 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// match is fully reserved
// ---------------------------------------------------------------------------

#[test]
fn function_named_match_rejected() {
    // php -l: "unexpected token \"match\", expecting \"(\"" — `match` is
    // fully reserved (8.0); must error here too.
    check(
        "<?php function match() {}",
        expect![[r#"
            SOURCE_FILE@0..25
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..22
                CLOSURE_EXPR@6..15
                  FUNCTION_KW@6..14 "function"
                  PARAM_LIST@15..15
                ERROR@15..22
                  MATCH_KW@15..20 "match"
                  L_PAREN@20..21 "("
                  R_PAREN@21..22 ")"
              BLOCK@23..25
                L_CURLY@23..24 "{"
                R_CURLY@24..25 "}"
            error@15..20 syntax error, unexpected token "match", expecting "("
            error@15..20 syntax error, unexpected token "match", expecting "{"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Property-hook names (8.4): get/set, case-insensitive
// ---------------------------------------------------------------------------

#[test]
fn hook_names_case_insensitive() {
    // 8.4: accessor names match case-insensitively, as php's identifiers
    // do. A bodiless accessor on a concrete class is the compiler's fatal
    // (`Non-abstract property hook must have a body`, pinned 8.5.6).
    check(
        "<?php class C { public $x { GET; SET; } }",
        expect![[r#"
            SOURCE_FILE@0..41
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..41
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..41
                  L_CURLY@14..15 "{"
                  PROPERTY_DECL@16..39
                    PUBLIC_KW@16..22 "public"
                    PROPERTY_DECLARATOR@23..25
                      VARIABLE@23..25 "$x"
                    PROPERTY_HOOK_LIST@26..39
                      L_CURLY@26..27 "{"
                      PROPERTY_HOOK@28..32
                        IDENT@28..31 "GET"
                        SEMICOLON@31..32 ";"
                      PROPERTY_HOOK@33..37
                        IDENT@33..36 "SET"
                        SEMICOLON@36..37 ";"
                      R_CURLY@38..39 "}"
                  R_CURLY@40..41 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// private(set) gluing vs `set` as a plain name
// ---------------------------------------------------------------------------

#[test]
fn private_set_no_space() {
    // 8.4+: not verifiable against local php 8.3.
    check(
        "<?php class C { private(set) int $x = 1; }",
        expect![[r#"
            SOURCE_FILE@0..42
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..42
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..42
                  L_CURLY@14..15 "{"
                  PROPERTY_DECL@16..40
                    SET_VISIBILITY@16..28
                      PRIVATE_KW@16..23 "private"
                      L_PAREN@23..24 "("
                      SET_KW@24..27 "set"
                      R_PAREN@27..28 ")"
                    NAMED_TYPE@29..32
                      IDENT@29..32 "int"
                    PROPERTY_DECLARATOR@33..39
                      VARIABLE@33..35 "$x"
                      EQ@36..37 "="
                      LITERAL_EXPR@38..39
                        INT@38..39 "1"
                    SEMICOLON@39..40 ";"
                  R_CURLY@41..42 "}"
        "#]],
    );
}

#[test]
fn private_set_with_spaces() {
    // php fuses `private(set)` in its scanner with no interior
    // whitespace, so `(set)` here is a type group and the diagnostics
    // are php's (8.5.9: `unexpected token ")", expecting token "&"`).
    check(
        "<?php class C { private (set) int $x = 1; }",
        expect![[r#"
            SOURCE_FILE@0..43
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..43
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..43
                  L_CURLY@14..15 "{"
                  PROPERTY_DECL@16..41
                    PRIVATE_KW@16..23 "private"
                    PAREN_TYPE@24..29
                      L_PAREN@24..25 "("
                      NAMED_TYPE@25..28
                        IDENT@25..28 "set"
                      R_PAREN@28..29 ")"
                    ERROR@30..40
                      IDENT@30..33 "int"
                      VARIABLE@34..36 "$x"
                      EQ@37..38 "="
                      INT@39..40 "1"
                    SEMICOLON@40..41 ";"
                  R_CURLY@42..43 "}"
            error@28..29 syntax error, unexpected token ")", expecting token "&"
            error@30..33 syntax error, unexpected identifier "int", expecting "|"
            error@30..33 syntax error, unexpected identifier "int", expecting variable
        "#]],
    );
}

#[test]
fn set_method_call() {
    // php -l ok: `set` as a member name.
    check(
        "<?php $x->set(1);",
        expect![[r#"
        SOURCE_FILE@0..17
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..17
            CALL_EXPR@6..16
              MEMBER_ACCESS_EXPR@6..13
                SIMPLE_VAR_EXPR@6..8
                  VARIABLE@6..8 "$x"
                ARROW@8..10 "->"
                MEMBER_NAME@10..13
                  IDENT@10..13 "set"
              ARG_LIST@13..16
                L_PAREN@13..14 "("
                LITERAL_EXPR@14..15
                  INT@14..15 "1"
                R_PAREN@15..16 ")"
            SEMICOLON@16..17 ";"
    "#]],
    );
}

#[test]
fn set_global_call() {
    // php -l ok: call to a global function named set.
    check(
        "<?php set(1);",
        expect![[r#"
        SOURCE_FILE@0..13
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..13
            CALL_EXPR@6..12
              IDENT@6..9 "set"
              ARG_LIST@9..12
                L_PAREN@9..10 "("
                LITERAL_EXPR@10..11
                  INT@10..11 "1"
                R_PAREN@11..12 ")"
            SEMICOLON@12..13 ";"
    "#]],
    );
}

// ---------------------------------------------------------------------------
// yield from vs `from` as a name
// ---------------------------------------------------------------------------

#[test]
fn yield_from_delegation() {
    check(
        "<?php function g() { yield from $gen; }",
        expect![[r#"
            SOURCE_FILE@0..39
              OPEN_TAG@0..6 "<?php "
              FUNCTION_DECL@6..39
                FUNCTION_KW@6..14 "function"
                IDENT@15..16 "g"
                PARAM_LIST@16..18
                  L_PAREN@16..17 "("
                  R_PAREN@17..18 ")"
                BLOCK@19..39
                  L_CURLY@19..20 "{"
                  EXPR_STMT@21..37
                    YIELD_FROM_EXPR@21..36
                      YIELD_KW@21..26 "yield"
                      FROM_KW@27..31 "from"
                      SIMPLE_VAR_EXPR@32..36
                        VARIABLE@32..36 "$gen"
                    SEMICOLON@36..37 ";"
                  R_CURLY@38..39 "}"
        "#]],
    );
}

#[test]
fn from_global_call() {
    // php -l ok: `from(1);` — `from` is only a keyword after `yield`.
    check(
        "<?php from(1);",
        expect![[r#"
        SOURCE_FILE@0..14
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..14
            CALL_EXPR@6..13
              IDENT@6..10 "from"
              ARG_LIST@10..13
                L_PAREN@10..11 "("
                LITERAL_EXPR@11..12
                  INT@11..12 "1"
                R_PAREN@12..13 ")"
            SEMICOLON@13..14 ";"
    "#]],
    );
}

#[test]
fn yield_calling_from2() {
    // php -l ok: `from2` is a single identifier; `yield from2()` yields
    // the call's value (no delegation).
    check(
        "<?php function g() { $from = yield from2(); }",
        expect![[r#"
            SOURCE_FILE@0..45
              OPEN_TAG@0..6 "<?php "
              FUNCTION_DECL@6..45
                FUNCTION_KW@6..14 "function"
                IDENT@15..16 "g"
                PARAM_LIST@16..18
                  L_PAREN@16..17 "("
                  R_PAREN@17..18 ")"
                BLOCK@19..45
                  L_CURLY@19..20 "{"
                  EXPR_STMT@21..43
                    ASSIGN_EXPR@21..42
                      SIMPLE_VAR_EXPR@21..26
                        VARIABLE@21..26 "$from"
                      EQ@27..28 "="
                      YIELD_EXPR@29..42
                        YIELD_KW@29..34 "yield"
                        CALL_EXPR@35..42
                          IDENT@35..40 "from2"
                          ARG_LIST@40..42
                            L_PAREN@40..41 "("
                            R_PAREN@41..42 ")"
                    SEMICOLON@42..43 ";"
                  R_CURLY@44..45 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Qualified name with a keyword segment
// ---------------------------------------------------------------------------

#[test]
fn qualified_name_keyword_segment() {
    // php -l ok: `A\list` is one qualified-name token (8.0 lexing);
    // keywords are plain label characters inside qualified names.
    check(
        r"<?php A\list();",
        expect![[r#"
        SOURCE_FILE@0..15
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..15
            CALL_EXPR@6..14
              NAME_QUALIFIED@6..12 "A\\list"
              ARG_LIST@12..14
                L_PAREN@12..13 "("
                R_PAREN@13..14 ")"
            SEMICOLON@14..15 ";"
    "#]],
    );
}
