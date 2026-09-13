//! Declaration fixtures (design §4.9): functions, class-likes, members,
//! parameters with promotion and hooks, attributes on every legal target,
//! trait adaptations and the use, namespace and declare structure. Read at
//! Mahout `1b3104a8` and regenerated once against the arena's dump.
//!
//! Every source here parses under the pin at 8.5.9 (`php -n -l`); the
//! five php rejects are its compiler's fatals, named beside each, and
//! design §6.1 puts each of those guards at the compiler function that
//! owns the construct, so the tree carries no diagnostic for them.

mod support;

use expect_test::expect;
use support::check;

// ---------------------------------------------------------------------------
// Functions and parameters
// ---------------------------------------------------------------------------

#[test]
fn function_byref_variadic_default() {
    check(
        "<?php function &f(int $a = 1, ...$rest) {}",
        expect![[r#"
            SOURCE_FILE@0..42
              OPEN_TAG@0..6 "<?php "
              FUNCTION_DECL@6..42
                FUNCTION_KW@6..14 "function"
                AMP@15..16 "&"
                IDENT@16..17 "f"
                PARAM_LIST@17..39
                  L_PAREN@17..18 "("
                  PARAM@18..28
                    NAMED_TYPE@18..21
                      IDENT@18..21 "int"
                    VARIABLE@22..24 "$a"
                    EQ@25..26 "="
                    LITERAL_EXPR@27..28
                      INT@27..28 "1"
                  COMMA@28..29 ","
                  PARAM@30..38
                    DOT_DOT_DOT@30..33 "..."
                    VARIABLE@33..38 "$rest"
                  R_PAREN@38..39 ")"
                BLOCK@40..42
                  L_CURLY@40..41 "{"
                  R_CURLY@41..42 "}"
        "#]],
    );
}

#[test]
fn param_type_forms() {
    check(
        "<?php function f(?T $a, A|B $b, A&B $c, (A&B)|C $d) {}",
        expect![[r#"
            SOURCE_FILE@0..54
              OPEN_TAG@0..6 "<?php "
              FUNCTION_DECL@6..54
                FUNCTION_KW@6..14 "function"
                IDENT@15..16 "f"
                PARAM_LIST@16..51
                  L_PAREN@16..17 "("
                  PARAM@17..22
                    NULLABLE_TYPE@17..19
                      QUESTION@17..18 "?"
                      NAMED_TYPE@18..19
                        IDENT@18..19 "T"
                    VARIABLE@20..22 "$a"
                  COMMA@22..23 ","
                  PARAM@24..30
                    UNION_TYPE@24..27
                      NAMED_TYPE@24..25
                        IDENT@24..25 "A"
                      PIPE@25..26 "|"
                      NAMED_TYPE@26..27
                        IDENT@26..27 "B"
                    VARIABLE@28..30 "$b"
                  COMMA@30..31 ","
                  PARAM@32..38
                    INTERSECTION_TYPE@32..35
                      NAMED_TYPE@32..33
                        IDENT@32..33 "A"
                      AMP@33..34 "&"
                      NAMED_TYPE@34..35
                        IDENT@34..35 "B"
                    VARIABLE@36..38 "$c"
                  COMMA@38..39 ","
                  PARAM@40..50
                    UNION_TYPE@40..47
                      PAREN_TYPE@40..45
                        L_PAREN@40..41 "("
                        INTERSECTION_TYPE@41..44
                          NAMED_TYPE@41..42
                            IDENT@41..42 "A"
                          AMP@42..43 "&"
                          NAMED_TYPE@43..44
                            IDENT@43..44 "B"
                        R_PAREN@44..45 ")"
                      PIPE@45..46 "|"
                      NAMED_TYPE@46..47
                        IDENT@46..47 "C"
                    VARIABLE@48..50 "$d"
                  R_PAREN@50..51 ")"
                BLOCK@52..54
                  L_CURLY@52..53 "{"
                  R_CURLY@53..54 "}"
        "#]],
    );
}

#[test]
fn return_type_dnf() {
    check(
        "<?php function f(): (X&Y)|null {}",
        expect![[r#"
            SOURCE_FILE@0..33
              OPEN_TAG@0..6 "<?php "
              FUNCTION_DECL@6..33
                FUNCTION_KW@6..14 "function"
                IDENT@15..16 "f"
                PARAM_LIST@16..18
                  L_PAREN@16..17 "("
                  R_PAREN@17..18 ")"
                RET_TYPE@18..30
                  COLON@18..19 ":"
                  UNION_TYPE@20..30
                    PAREN_TYPE@20..25
                      L_PAREN@20..21 "("
                      INTERSECTION_TYPE@21..24
                        NAMED_TYPE@21..22
                          IDENT@21..22 "X"
                        AMP@22..23 "&"
                        NAMED_TYPE@23..24
                          IDENT@23..24 "Y"
                      R_PAREN@24..25 ")"
                    PIPE@25..26 "|"
                    NAMED_TYPE@26..30
                      IDENT@26..30 "null"
                BLOCK@31..33
                  L_CURLY@31..32 "{"
                  R_CURLY@32..33 "}"
        "#]],
    );
}

#[test]
fn byref_param_vs_intersection_amp() {
    // php -l ok: `A&B $x` intersection, `A &$y` by-ref, `A&...$z` by-ref
    // variadic, the 8.1 ampersand split, as parser lookahead.
    check(
        "<?php function f(A&B $x, A &$y, A&...$z) {}",
        expect![[r#"
            SOURCE_FILE@0..43
              OPEN_TAG@0..6 "<?php "
              FUNCTION_DECL@6..43
                FUNCTION_KW@6..14 "function"
                IDENT@15..16 "f"
                PARAM_LIST@16..40
                  L_PAREN@16..17 "("
                  PARAM@17..23
                    INTERSECTION_TYPE@17..20
                      NAMED_TYPE@17..18
                        IDENT@17..18 "A"
                      AMP@18..19 "&"
                      NAMED_TYPE@19..20
                        IDENT@19..20 "B"
                    VARIABLE@21..23 "$x"
                  COMMA@23..24 ","
                  PARAM@25..30
                    NAMED_TYPE@25..26
                      IDENT@25..26 "A"
                    AMP@27..28 "&"
                    VARIABLE@28..30 "$y"
                  COMMA@30..31 ","
                  PARAM@32..39
                    NAMED_TYPE@32..33
                      IDENT@32..33 "A"
                    AMP@33..34 "&"
                    DOT_DOT_DOT@34..37 "..."
                    VARIABLE@37..39 "$z"
                  R_PAREN@39..40 ")"
                BLOCK@41..43
                  L_CURLY@41..42 "{"
                  R_CURLY@42..43 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Classes
// ---------------------------------------------------------------------------

#[test]
fn class_extends_implements() {
    check(
        "<?php class C extends B implements I, J {}",
        expect![[r#"
            SOURCE_FILE@0..42
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..42
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                EXTENDS_CLAUSE@14..23
                  EXTENDS_KW@14..21 "extends"
                  IDENT@22..23 "B"
                IMPLEMENTS_CLAUSE@24..39
                  IMPLEMENTS_KW@24..34 "implements"
                  IDENT@35..36 "I"
                  COMMA@36..37 ","
                  IDENT@38..39 "J"
                CLASS_BODY@40..42
                  L_CURLY@40..41 "{"
                  R_CURLY@41..42 "}"
        "#]],
    );
}

#[test]
fn class_modifiers() {
    check(
        "<?php abstract class A {} final class F {} readonly class R {}",
        expect![[r#"
            SOURCE_FILE@0..62
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..25
                ABSTRACT_KW@6..14 "abstract"
                CLASS_KW@15..20 "class"
                IDENT@21..22 "A"
                CLASS_BODY@23..25
                  L_CURLY@23..24 "{"
                  R_CURLY@24..25 "}"
              CLASS_DECL@26..42
                FINAL_KW@26..31 "final"
                CLASS_KW@32..37 "class"
                IDENT@38..39 "F"
                CLASS_BODY@40..42
                  L_CURLY@40..41 "{"
                  R_CURLY@41..42 "}"
              CLASS_DECL@43..62
                READONLY_KW@43..51 "readonly"
                CLASS_KW@52..57 "class"
                IDENT@58..59 "R"
                CLASS_BODY@60..62
                  L_CURLY@60..61 "{"
                  R_CURLY@61..62 "}"
        "#]],
    );
}

#[test]
fn interface_multi_extends() {
    check(
        "<?php interface I extends A, B {}",
        expect![[r#"
            SOURCE_FILE@0..33
              OPEN_TAG@0..6 "<?php "
              INTERFACE_DECL@6..33
                INTERFACE_KW@6..15 "interface"
                IDENT@16..17 "I"
                EXTENDS_CLAUSE@18..30
                  EXTENDS_KW@18..25 "extends"
                  IDENT@26..27 "A"
                  COMMA@27..28 ","
                  IDENT@29..30 "B"
                CLASS_BODY@31..33
                  L_CURLY@31..32 "{"
                  R_CURLY@32..33 "}"
        "#]],
    );
}

#[test]
fn trait_decl() {
    check(
        "<?php trait T { public function m() {} }",
        expect![[r#"
            SOURCE_FILE@0..40
              OPEN_TAG@0..6 "<?php "
              TRAIT_DECL@6..40
                TRAIT_KW@6..11 "trait"
                IDENT@12..13 "T"
                CLASS_BODY@14..40
                  L_CURLY@14..15 "{"
                  METHOD_DECL@16..38
                    PUBLIC_KW@16..22 "public"
                    FUNCTION_KW@23..31 "function"
                    MEMBER_NAME@32..33
                      IDENT@32..33 "m"
                    PARAM_LIST@33..35
                      L_PAREN@33..34 "("
                      R_PAREN@34..35 ")"
                    BLOCK@36..38
                      L_CURLY@36..37 "{"
                      R_CURLY@37..38 "}"
                  R_CURLY@39..40 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

#[test]
fn enum_pure() {
    check(
        "<?php enum E { case A; case B; }",
        expect![[r#"
            SOURCE_FILE@0..32
              OPEN_TAG@0..6 "<?php "
              ENUM_DECL@6..32
                ENUM_KW@6..10 "enum"
                IDENT@11..12 "E"
                CLASS_BODY@13..32
                  L_CURLY@13..14 "{"
                  ENUM_CASE_DECL@15..22
                    CASE_KW@15..19 "case"
                    MEMBER_NAME@20..21
                      IDENT@20..21 "A"
                    SEMICOLON@21..22 ";"
                  ENUM_CASE_DECL@23..30
                    CASE_KW@23..27 "case"
                    MEMBER_NAME@28..29
                      IDENT@28..29 "B"
                    SEMICOLON@29..30 ";"
                  R_CURLY@31..32 "}"
        "#]],
    );
}

#[test]
fn enum_backed_full() {
    // php -l ok: cases with values, implements, use, a method, a const.
    check(
        "<?php enum E: string implements I { use T; case A = 'a'; const C = 1; public function m(): void {} }",
        expect![[r#"
            SOURCE_FILE@0..100
              OPEN_TAG@0..6 "<?php "
              ENUM_DECL@6..100
                ENUM_KW@6..10 "enum"
                IDENT@11..12 "E"
                ENUM_BACKING@12..20
                  COLON@12..13 ":"
                  NAMED_TYPE@14..20
                    IDENT@14..20 "string"
                IMPLEMENTS_CLAUSE@21..33
                  IMPLEMENTS_KW@21..31 "implements"
                  IDENT@32..33 "I"
                CLASS_BODY@34..100
                  L_CURLY@34..35 "{"
                  TRAIT_USE_DECL@36..42
                    USE_KW@36..39 "use"
                    IDENT@40..41 "T"
                    SEMICOLON@41..42 ";"
                  ENUM_CASE_DECL@43..56
                    CASE_KW@43..47 "case"
                    MEMBER_NAME@48..49
                      IDENT@48..49 "A"
                    EQ@50..51 "="
                    LITERAL_EXPR@52..55
                      SINGLE_QUOTED@52..55 "'a'"
                    SEMICOLON@55..56 ";"
                  CLASS_CONST_DECL@57..69
                    CONST_KW@57..62 "const"
                    CONST_DECLARATOR@63..68
                      MEMBER_NAME@63..64
                        IDENT@63..64 "C"
                      EQ@65..66 "="
                      LITERAL_EXPR@67..68
                        INT@67..68 "1"
                    SEMICOLON@68..69 ";"
                  METHOD_DECL@70..98
                    PUBLIC_KW@70..76 "public"
                    FUNCTION_KW@77..85 "function"
                    MEMBER_NAME@86..87
                      IDENT@86..87 "m"
                    PARAM_LIST@87..89
                      L_PAREN@87..88 "("
                      R_PAREN@88..89 ")"
                    RET_TYPE@89..95
                      COLON@89..90 ":"
                      NAMED_TYPE@91..95
                        IDENT@91..95 "void"
                    BLOCK@96..98
                      L_CURLY@96..97 "{"
                      R_CURLY@97..98 "}"
                  R_CURLY@99..100 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Anonymous classes
// ---------------------------------------------------------------------------

#[test]
fn new_readonly_anon_class() {
    // php -l ok: a readonly anonymous class (8.3); `new_expr` routes to
    // the anonymous-class grammar on a modifier as on `class` and `#[`.
    check(
        "<?php new readonly class {};",
        expect![[r#"
            SOURCE_FILE@0..28
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..28
                NEW_EXPR@6..27
                  NEW_KW@6..9 "new"
                  ANON_CLASS_DECL@10..27
                    READONLY_KW@10..18 "readonly"
                    CLASS_KW@19..24 "class"
                    CLASS_BODY@25..27
                      L_CURLY@25..26 "{"
                      R_CURLY@26..27 "}"
                SEMICOLON@27..28 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Attributes on every legal target
// ---------------------------------------------------------------------------

#[test]
fn attribute_args_and_groups() {
    check(
        "<?php #[A(1, name: 2)] #[B, C] class K {}",
        expect![[r##"
            SOURCE_FILE@0..41
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..41
                ATTRIBUTE_GROUP@6..22
                  ATTRIBUTE_OPEN@6..8 "#["
                  ATTRIBUTE@8..21
                    IDENT@8..9 "A"
                    ARG_LIST@9..21
                      L_PAREN@9..10 "("
                      LITERAL_EXPR@10..11
                        INT@10..11 "1"
                      COMMA@11..12 ","
                      NAMED_ARG@13..20
                        IDENT@13..17 "name"
                        COLON@17..18 ":"
                        LITERAL_EXPR@19..20
                          INT@19..20 "2"
                      R_PAREN@20..21 ")"
                  R_BRACK@21..22 "]"
                ATTRIBUTE_GROUP@23..30
                  ATTRIBUTE_OPEN@23..25 "#["
                  ATTRIBUTE@25..26
                    IDENT@25..26 "B"
                  COMMA@26..27 ","
                  ATTRIBUTE@28..29
                    IDENT@28..29 "C"
                  R_BRACK@29..30 "]"
                CLASS_KW@31..36 "class"
                IDENT@37..38 "K"
                CLASS_BODY@39..41
                  L_CURLY@39..40 "{"
                  R_CURLY@40..41 "}"
        "##]],
    );
}

#[test]
fn attributes_on_function_and_params() {
    check(
        "<?php #[A] function f(#[B] int $x) {}",
        expect![[r##"
            SOURCE_FILE@0..37
              OPEN_TAG@0..6 "<?php "
              FUNCTION_DECL@6..37
                ATTRIBUTE_GROUP@6..10
                  ATTRIBUTE_OPEN@6..8 "#["
                  ATTRIBUTE@8..9
                    IDENT@8..9 "A"
                  R_BRACK@9..10 "]"
                FUNCTION_KW@11..19 "function"
                IDENT@20..21 "f"
                PARAM_LIST@21..34
                  L_PAREN@21..22 "("
                  PARAM@22..33
                    ATTRIBUTE_GROUP@22..26
                      ATTRIBUTE_OPEN@22..24 "#["
                      ATTRIBUTE@24..25
                        IDENT@24..25 "B"
                      R_BRACK@25..26 "]"
                    NAMED_TYPE@27..30
                      IDENT@27..30 "int"
                    VARIABLE@31..33 "$x"
                  R_PAREN@33..34 ")"
                BLOCK@35..37
                  L_CURLY@35..36 "{"
                  R_CURLY@36..37 "}"
        "##]],
    );
}

#[test]
fn attributes_on_members() {
    check(
        "<?php class C { #[A] const X = 1; #[B] public $p; #[C] function m() {} }",
        expect![[r##"
            SOURCE_FILE@0..72
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..72
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..72
                  L_CURLY@14..15 "{"
                  CLASS_CONST_DECL@16..33
                    ATTRIBUTE_GROUP@16..20
                      ATTRIBUTE_OPEN@16..18 "#["
                      ATTRIBUTE@18..19
                        IDENT@18..19 "A"
                      R_BRACK@19..20 "]"
                    CONST_KW@21..26 "const"
                    CONST_DECLARATOR@27..32
                      MEMBER_NAME@27..28
                        IDENT@27..28 "X"
                      EQ@29..30 "="
                      LITERAL_EXPR@31..32
                        INT@31..32 "1"
                    SEMICOLON@32..33 ";"
                  PROPERTY_DECL@34..49
                    ATTRIBUTE_GROUP@34..38
                      ATTRIBUTE_OPEN@34..36 "#["
                      ATTRIBUTE@36..37
                        IDENT@36..37 "B"
                      R_BRACK@37..38 "]"
                    PUBLIC_KW@39..45 "public"
                    PROPERTY_DECLARATOR@46..48
                      VARIABLE@46..48 "$p"
                    SEMICOLON@48..49 ";"
                  METHOD_DECL@50..70
                    ATTRIBUTE_GROUP@50..54
                      ATTRIBUTE_OPEN@50..52 "#["
                      ATTRIBUTE@52..53
                        IDENT@52..53 "C"
                      R_BRACK@53..54 "]"
                    FUNCTION_KW@55..63 "function"
                    MEMBER_NAME@64..65
                      IDENT@64..65 "m"
                    PARAM_LIST@65..67
                      L_PAREN@65..66 "("
                      R_PAREN@66..67 ")"
                    BLOCK@68..70
                      L_CURLY@68..69 "{"
                      R_CURLY@69..70 "}"
                  R_CURLY@71..72 "}"
        "##]],
    );
}

#[test]
fn attributes_on_enum_case() {
    check(
        "<?php enum E { #[A] case X; }",
        expect![[r##"
            SOURCE_FILE@0..29
              OPEN_TAG@0..6 "<?php "
              ENUM_DECL@6..29
                ENUM_KW@6..10 "enum"
                IDENT@11..12 "E"
                CLASS_BODY@13..29
                  L_CURLY@13..14 "{"
                  ENUM_CASE_DECL@15..27
                    ATTRIBUTE_GROUP@15..19
                      ATTRIBUTE_OPEN@15..17 "#["
                      ATTRIBUTE@17..18
                        IDENT@17..18 "A"
                      R_BRACK@18..19 "]"
                    CASE_KW@20..24 "case"
                    MEMBER_NAME@25..26
                      IDENT@25..26 "X"
                    SEMICOLON@26..27 ";"
                  R_CURLY@28..29 "}"
        "##]],
    );
}

#[test]
fn attributes_on_closure_and_arrow_fn() {
    check(
        "<?php #[A] function () {}; #[B] fn() => 1;",
        expect![[r##"
            SOURCE_FILE@0..42
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..26
                CLOSURE_EXPR@6..25
                  ATTRIBUTE_GROUP@6..10
                    ATTRIBUTE_OPEN@6..8 "#["
                    ATTRIBUTE@8..9
                      IDENT@8..9 "A"
                    R_BRACK@9..10 "]"
                  FUNCTION_KW@11..19 "function"
                  PARAM_LIST@20..22
                    L_PAREN@20..21 "("
                    R_PAREN@21..22 ")"
                  BLOCK@23..25
                    L_CURLY@23..24 "{"
                    R_CURLY@24..25 "}"
                SEMICOLON@25..26 ";"
              EXPR_STMT@27..42
                ARROW_FN_EXPR@27..41
                  ATTRIBUTE_GROUP@27..31
                    ATTRIBUTE_OPEN@27..29 "#["
                    ATTRIBUTE@29..30
                      IDENT@29..30 "B"
                    R_BRACK@30..31 "]"
                  FN_KW@32..34 "fn"
                  PARAM_LIST@34..36
                    L_PAREN@34..35 "("
                    R_PAREN@35..36 ")"
                  FAT_ARROW@37..39 "=>"
                  LITERAL_EXPR@40..41
                    INT@40..41 "1"
                SEMICOLON@41..42 ";"
        "##]],
    );
}

#[test]
fn attributes_on_anon_class() {
    check(
        "<?php new #[A] class {};",
        expect![[r##"
            SOURCE_FILE@0..24
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..24
                NEW_EXPR@6..23
                  NEW_KW@6..9 "new"
                  ANON_CLASS_DECL@10..23
                    ATTRIBUTE_GROUP@10..14
                      ATTRIBUTE_OPEN@10..12 "#["
                      ATTRIBUTE@12..13
                        IDENT@12..13 "A"
                      R_BRACK@13..14 "]"
                    CLASS_KW@15..20 "class"
                    CLASS_BODY@21..23
                      L_CURLY@21..22 "{"
                      R_CURLY@22..23 "}"
                SEMICOLON@23..24 ";"
        "##]],
    );
}

// ---------------------------------------------------------------------------
// Constructor promotion and asymmetric visibility (8.4)
// ---------------------------------------------------------------------------

#[test]
fn constructor_promotion() {
    check(
        "<?php class C { function __construct(public int $a, private(set) readonly string $b) {} }",
        expect![[r#"
            SOURCE_FILE@0..89
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..89
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..89
                  L_CURLY@14..15 "{"
                  METHOD_DECL@16..87
                    FUNCTION_KW@16..24 "function"
                    MEMBER_NAME@25..36
                      IDENT@25..36 "__construct"
                    PARAM_LIST@36..84
                      L_PAREN@36..37 "("
                      PARAM@37..50
                        PUBLIC_KW@37..43 "public"
                        NAMED_TYPE@44..47
                          IDENT@44..47 "int"
                        VARIABLE@48..50 "$a"
                      COMMA@50..51 ","
                      PARAM@52..83
                        SET_VISIBILITY@52..64
                          PRIVATE_KW@52..59 "private"
                          L_PAREN@59..60 "("
                          SET_KW@60..63 "set"
                          R_PAREN@63..64 ")"
                        READONLY_KW@65..73 "readonly"
                        NAMED_TYPE@74..80
                          IDENT@74..80 "string"
                        VARIABLE@81..83 "$b"
                      R_PAREN@83..84 ")"
                    BLOCK@85..87
                      L_CURLY@85..86 "{"
                      R_CURLY@86..87 "}"
                  R_CURLY@88..89 "}"
        "#]],
    );
}

#[test]
fn asymmetric_visibility_property() {
    // php -l ok: asymmetric visibility (8.4).
    check(
        "<?php class C { public private(set) int $x = 1; }",
        expect![[r#"
            SOURCE_FILE@0..49
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..49
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..49
                  L_CURLY@14..15 "{"
                  PROPERTY_DECL@16..47
                    PUBLIC_KW@16..22 "public"
                    SET_VISIBILITY@23..35
                      PRIVATE_KW@23..30 "private"
                      L_PAREN@30..31 "("
                      SET_KW@31..34 "set"
                      R_PAREN@34..35 ")"
                    NAMED_TYPE@36..39
                      IDENT@36..39 "int"
                    PROPERTY_DECLARATOR@40..46
                      VARIABLE@40..42 "$x"
                      EQ@43..44 "="
                      LITERAL_EXPR@45..46
                        INT@45..46 "1"
                    SEMICOLON@46..47 ";"
                  R_CURLY@48..49 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Property hooks (8.4)
// ---------------------------------------------------------------------------

#[test]
fn hooks_plain_get_set() {
    // A bodiless hook on a concrete class is the compiler's fatal
    // (`Non-abstract property hook must have a body`, pinned 8.5.9);
    // the tree is the same as an abstract property's.
    check(
        "<?php class C { public $x { get; set; } }",
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
                        IDENT@28..31 "get"
                        SEMICOLON@31..32 ";"
                      PROPERTY_HOOK@33..37
                        IDENT@33..36 "set"
                        SEMICOLON@36..37 ";"
                      R_CURLY@38..39 "}"
                  R_CURLY@40..41 "}"
        "#]],
    );
}

#[test]
fn hooks_arrow_get() {
    // php -l ok: an arrow-bodied get hook (8.4).
    check(
        "<?php class C { public $x { get => 1; } }",
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
                      PROPERTY_HOOK@28..37
                        IDENT@28..31 "get"
                        HOOK_ARROW_BODY@32..37
                          FAT_ARROW@32..34 "=>"
                          LITERAL_EXPR@35..36
                            INT@35..36 "1"
                          SEMICOLON@36..37 ";"
                      R_CURLY@38..39 "}"
                  R_CURLY@40..41 "}"
        "#]],
    );
}

#[test]
fn hooks_byref_get_final_set() {
    // php -l ok: a by-reference get and a final set with the untyped
    // `$value` an untyped property's set hook takes (8.4).
    check(
        "<?php class C { public $x { &get { return $this->v; } final set($value) {} } }",
        expect![[r#"
            SOURCE_FILE@0..78
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..78
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..78
                  L_CURLY@14..15 "{"
                  PROPERTY_DECL@16..76
                    PUBLIC_KW@16..22 "public"
                    PROPERTY_DECLARATOR@23..25
                      VARIABLE@23..25 "$x"
                    PROPERTY_HOOK_LIST@26..76
                      L_CURLY@26..27 "{"
                      PROPERTY_HOOK@28..53
                        AMP@28..29 "&"
                        IDENT@29..32 "get"
                        BLOCK@33..53
                          L_CURLY@33..34 "{"
                          RETURN_STMT@35..51
                            RETURN_KW@35..41 "return"
                            MEMBER_ACCESS_EXPR@42..50
                              SIMPLE_VAR_EXPR@42..47
                                VARIABLE@42..47 "$this"
                              ARROW@47..49 "->"
                              MEMBER_NAME@49..50
                                IDENT@49..50 "v"
                            SEMICOLON@50..51 ";"
                          R_CURLY@52..53 "}"
                      PROPERTY_HOOK@54..74
                        FINAL_KW@54..59 "final"
                        IDENT@60..63 "set"
                        PARAM_LIST@63..71
                          L_PAREN@63..64 "("
                          PARAM@64..70
                            VARIABLE@64..70 "$value"
                          R_PAREN@70..71 ")"
                        BLOCK@72..74
                          L_CURLY@72..73 "{"
                          R_CURLY@73..74 "}"
                      R_CURLY@75..76 "}"
                  R_CURLY@77..78 "}"
        "#]],
    );
}

#[test]
fn hooks_on_promoted_param() {
    // A hook on a promoted parameter (8.4); this one is bodiless, so
    // the compiler's fatal (`Non-abstract property hook must have a
    // body`, pinned 8.5.9) and not the parser's.
    check(
        "<?php class C { function __construct(public $x { get; }) {} }",
        expect![[r#"
            SOURCE_FILE@0..61
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..61
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..61
                  L_CURLY@14..15 "{"
                  METHOD_DECL@16..59
                    FUNCTION_KW@16..24 "function"
                    MEMBER_NAME@25..36
                      IDENT@25..36 "__construct"
                    PARAM_LIST@36..56
                      L_PAREN@36..37 "("
                      PARAM@37..55
                        PUBLIC_KW@37..43 "public"
                        VARIABLE@44..46 "$x"
                        PROPERTY_HOOK_LIST@47..55
                          L_CURLY@47..48 "{"
                          PROPERTY_HOOK@49..53
                            IDENT@49..52 "get"
                            SEMICOLON@52..53 ";"
                          R_CURLY@54..55 "}"
                      R_PAREN@55..56 ")"
                    BLOCK@57..59
                      L_CURLY@57..58 "{"
                      R_CURLY@58..59 "}"
                  R_CURLY@60..61 "}"
        "#]],
    );
}

#[test]
fn hooks_in_interface() {
    // php -l ok: a hooked property in an interface (8.4).
    check(
        "<?php interface I { public int $x { get; } }",
        expect![[r#"
            SOURCE_FILE@0..44
              OPEN_TAG@0..6 "<?php "
              INTERFACE_DECL@6..44
                INTERFACE_KW@6..15 "interface"
                IDENT@16..17 "I"
                CLASS_BODY@18..44
                  L_CURLY@18..19 "{"
                  PROPERTY_DECL@20..42
                    PUBLIC_KW@20..26 "public"
                    NAMED_TYPE@27..30
                      IDENT@27..30 "int"
                    PROPERTY_DECLARATOR@31..33
                      VARIABLE@31..33 "$x"
                    PROPERTY_HOOK_LIST@34..42
                      L_CURLY@34..35 "{"
                      PROPERTY_HOOK@36..40
                        IDENT@36..39 "get"
                        SEMICOLON@39..40 ";"
                      R_CURLY@41..42 "}"
                  R_CURLY@43..44 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Trait use with adaptations
// ---------------------------------------------------------------------------

#[test]
fn trait_use_adaptations() {
    // php -l ok. `m as final;` is a modifier change, php's `trait_alias:
    // … T_AS member_modifier`, and `final` is the adaptation's modifier,
    // not its alias.
    check(
        "<?php class C { use A, B { A::m insteadof B; B::m as protected n; m as final; } }",
        expect![[r#"
            SOURCE_FILE@0..81
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..81
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..81
                  L_CURLY@14..15 "{"
                  TRAIT_USE_DECL@16..79
                    USE_KW@16..19 "use"
                    IDENT@20..21 "A"
                    COMMA@21..22 ","
                    IDENT@23..24 "B"
                    TRAIT_ADAPTATION_LIST@25..79
                      L_CURLY@25..26 "{"
                      INSTEADOF_ADAPTATION@27..44
                        TRAIT_METHOD_REF@27..31
                          IDENT@27..28 "A"
                          COLON_COLON@28..30 "::"
                          MEMBER_NAME@30..31
                            IDENT@30..31 "m"
                        INSTEADOF_KW@32..41 "insteadof"
                        IDENT@42..43 "B"
                        SEMICOLON@43..44 ";"
                      AS_ADAPTATION@45..65
                        TRAIT_METHOD_REF@45..49
                          IDENT@45..46 "B"
                          COLON_COLON@46..48 "::"
                          MEMBER_NAME@48..49
                            IDENT@48..49 "m"
                        AS_KW@50..52 "as"
                        PROTECTED_KW@53..62 "protected"
                        MEMBER_NAME@63..64
                          IDENT@63..64 "n"
                        SEMICOLON@64..65 ";"
                      AS_ADAPTATION@66..77
                        TRAIT_METHOD_REF@66..67
                          MEMBER_NAME@66..67
                            IDENT@66..67 "m"
                        AS_KW@68..70 "as"
                        FINAL_KW@71..76 "final"
                        SEMICOLON@76..77 ";"
                      R_CURLY@78..79 "}"
                  R_CURLY@80..81 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Class constants
// ---------------------------------------------------------------------------

#[test]
fn typed_class_const_multi_declarator() {
    // php -l ok: a typed constant (8.3) with two declarators.
    check(
        "<?php class C { const int|string X = 1, Y = 2; }",
        expect![[r#"
            SOURCE_FILE@0..48
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..48
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..48
                  L_CURLY@14..15 "{"
                  CLASS_CONST_DECL@16..46
                    CONST_KW@16..21 "const"
                    UNION_TYPE@22..32
                      NAMED_TYPE@22..25
                        IDENT@22..25 "int"
                      PIPE@25..26 "|"
                      NAMED_TYPE@26..32
                        IDENT@26..32 "string"
                    CONST_DECLARATOR@33..38
                      MEMBER_NAME@33..34
                        IDENT@33..34 "X"
                      EQ@35..36 "="
                      LITERAL_EXPR@37..38
                        INT@37..38 "1"
                    COMMA@38..39 ","
                    CONST_DECLARATOR@40..45
                      MEMBER_NAME@40..41
                        IDENT@40..41 "Y"
                      EQ@42..43 "="
                      LITERAL_EXPR@44..45
                        INT@44..45 "2"
                    SEMICOLON@45..46 ";"
                  R_CURLY@47..48 "}"
        "#]],
    );
}

#[test]
fn enum_case_outside_enum() {
    // The parser takes a case in any class-like; php's compiler is what
    // rejects it (`Case can only be used in enums`, pinned 8.5.9).
    check(
        "<?php class C { case A; }",
        expect![[r#"
            SOURCE_FILE@0..25
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..25
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..25
                  L_CURLY@14..15 "{"
                  ENUM_CASE_DECL@16..23
                    CASE_KW@16..20 "case"
                    MEMBER_NAME@21..22
                      IDENT@21..22 "A"
                    SEMICOLON@22..23 ";"
                  R_CURLY@24..25 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// use statements
// ---------------------------------------------------------------------------

#[test]
fn use_plain_and_aliased() {
    check(
        r"<?php use A\B; use C\D as E;",
        expect![[r#"
            SOURCE_FILE@0..28
              OPEN_TAG@0..6 "<?php "
              USE_STMT@6..14
                USE_KW@6..9 "use"
                USE_CLAUSE@10..13
                  NAME_QUALIFIED@10..13 "A\\B"
                SEMICOLON@13..14 ";"
              USE_STMT@15..28
                USE_KW@15..18 "use"
                USE_CLAUSE@19..27
                  NAME_QUALIFIED@19..22 "C\\D"
                  USE_ALIAS@23..27
                    AS_KW@23..25 "as"
                    IDENT@26..27 "E"
                SEMICOLON@27..28 ";"
        "#]],
    );
}

#[test]
fn use_function_and_const() {
    check(
        r"<?php use function a\b; use const C\D;",
        expect![[r#"
            SOURCE_FILE@0..38
              OPEN_TAG@0..6 "<?php "
              USE_STMT@6..23
                USE_KW@6..9 "use"
                FUNCTION_KW@10..18 "function"
                USE_CLAUSE@19..22
                  NAME_QUALIFIED@19..22 "a\\b"
                SEMICOLON@22..23 ";"
              USE_STMT@24..38
                USE_KW@24..27 "use"
                CONST_KW@28..33 "const"
                USE_CLAUSE@34..37
                  NAME_QUALIFIED@34..37 "C\\D"
                SEMICOLON@37..38 ";"
        "#]],
    );
}

#[test]
fn use_group_mixed_kinds() {
    check(
        r"<?php use A\{B, function c, const D as E};",
        expect![[r#"
            SOURCE_FILE@0..42
              OPEN_TAG@0..6 "<?php "
              USE_STMT@6..42
                USE_KW@6..9 "use"
                USE_GROUP@10..41
                  IDENT@10..11 "A"
                  BACKSLASH@11..12 "\\"
                  L_CURLY@12..13 "{"
                  USE_GROUP_ITEM@13..14
                    IDENT@13..14 "B"
                  COMMA@14..15 ","
                  USE_GROUP_ITEM@16..26
                    FUNCTION_KW@16..24 "function"
                    IDENT@25..26 "c"
                  COMMA@26..27 ","
                  USE_GROUP_ITEM@28..40
                    CONST_KW@28..33 "const"
                    IDENT@34..35 "D"
                    USE_ALIAS@36..40
                      AS_KW@36..38 "as"
                      IDENT@39..40 "E"
                  R_CURLY@40..41 "}"
                SEMICOLON@41..42 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Namespaces
// ---------------------------------------------------------------------------

#[test]
fn namespace_unbraced() {
    check(
        r"<?php namespace A\B; const X = 1;",
        expect![[r#"
            SOURCE_FILE@0..33
              OPEN_TAG@0..6 "<?php "
              NAMESPACE_DEF@6..20
                NAMESPACE_KW@6..15 "namespace"
                NAME_QUALIFIED@16..19 "A\\B"
                SEMICOLON@19..20 ";"
              CONST_STMT@21..33
                CONST_KW@21..26 "const"
                CONST_DECLARATOR@27..32
                  MEMBER_NAME@27..28
                    IDENT@27..28 "X"
                  EQ@29..30 "="
                  LITERAL_EXPR@31..32
                    INT@31..32 "1"
                SEMICOLON@32..33 ";"
        "#]],
    );
}

#[test]
fn namespace_braced() {
    check(
        "<?php namespace A { const X = 1; } namespace { }",
        expect![[r#"
            SOURCE_FILE@0..48
              OPEN_TAG@0..6 "<?php "
              NAMESPACE_DEF@6..34
                NAMESPACE_KW@6..15 "namespace"
                IDENT@16..17 "A"
                BLOCK@18..34
                  L_CURLY@18..19 "{"
                  CONST_STMT@20..32
                    CONST_KW@20..25 "const"
                    CONST_DECLARATOR@26..31
                      MEMBER_NAME@26..27
                        IDENT@26..27 "X"
                      EQ@28..29 "="
                      LITERAL_EXPR@30..31
                        INT@30..31 "1"
                    SEMICOLON@31..32 ";"
                  R_CURLY@33..34 "}"
              NAMESPACE_DEF@35..48
                NAMESPACE_KW@35..44 "namespace"
                BLOCK@45..48
                  L_CURLY@45..46 "{"
                  R_CURLY@47..48 "}"
        "#]],
    );
}

#[test]
fn namespace_form_mixing() {
    // Both forms parse; mixing them is the compiler's fatal (`Cannot mix
    // bracketed namespace declarations with unbracketed namespace
    // declarations`, pinned 8.5.9).
    check(
        "<?php namespace A; namespace B {}",
        expect![[r#"
            SOURCE_FILE@0..33
              OPEN_TAG@0..6 "<?php "
              NAMESPACE_DEF@6..18
                NAMESPACE_KW@6..15 "namespace"
                IDENT@16..17 "A"
                SEMICOLON@17..18 ";"
              NAMESPACE_DEF@19..33
                NAMESPACE_KW@19..28 "namespace"
                IDENT@29..30 "B"
                BLOCK@31..33
                  L_CURLY@31..32 "{"
                  R_CURLY@32..33 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// declare
// ---------------------------------------------------------------------------

#[test]
fn declare_strict_types_first() {
    check(
        "<?php declare(strict_types=1); echo 1;",
        expect![[r#"
            SOURCE_FILE@0..38
              OPEN_TAG@0..6 "<?php "
              DECLARE_STMT@6..30
                DECLARE_KW@6..13 "declare"
                L_PAREN@13..14 "("
                DECLARE_DIRECTIVE@14..28
                  IDENT@14..26 "strict_types"
                  EQ@26..27 "="
                  LITERAL_EXPR@27..28
                    INT@27..28 "1"
                R_PAREN@28..29 ")"
                SEMICOLON@29..30 ";"
              ECHO_STMT@31..38
                ECHO_KW@31..35 "echo"
                LITERAL_EXPR@36..37
                  INT@36..37 "1"
                SEMICOLON@37..38 ";"
        "#]],
    );
}

#[test]
fn declare_strict_types_misplaced() {
    // The declare parses wherever it stands; its place is the compiler's
    // fatal (`strict_types declaration must be the very first statement
    // in the script`, pinned 8.5.9).
    check(
        "<?php echo 1; declare(strict_types=1);",
        expect![[r#"
            SOURCE_FILE@0..38
              OPEN_TAG@0..6 "<?php "
              ECHO_STMT@6..13
                ECHO_KW@6..10 "echo"
                LITERAL_EXPR@11..12
                  INT@11..12 "1"
                SEMICOLON@12..13 ";"
              DECLARE_STMT@14..38
                DECLARE_KW@14..21 "declare"
                L_PAREN@21..22 "("
                DECLARE_DIRECTIVE@22..36
                  IDENT@22..34 "strict_types"
                  EQ@34..35 "="
                  LITERAL_EXPR@35..36
                    INT@35..36 "1"
                R_PAREN@36..37 ")"
                SEMICOLON@37..38 ";"
        "#]],
    );
}
