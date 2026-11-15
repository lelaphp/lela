//! Expression fixtures (design §4.9): literals, names, variables, the
//! Pratt table's precedence shape, the special expression forms and the
//! access chains. Read at Mahout `1b3104a8` and regenerated once against
//! the arena's dump.

mod support;

use expect_test::expect;
use support::check;

// ---------------------------------------------------------------------------
// Literals and names
// ---------------------------------------------------------------------------

#[test]
fn int_literal() {
    check(
        "<?php 1;",
        expect![[r#"
        SOURCE_FILE@0..8
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..8
            LITERAL_EXPR@6..7
              INT@6..7 "1"
            SEMICOLON@7..8 ";"
    "#]],
    );
}

#[test]
fn float_literal() {
    check(
        "<?php 1.5e3;",
        expect![[r#"
        SOURCE_FILE@0..12
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..12
            LITERAL_EXPR@6..11
              FLOAT@6..11 "1.5e3"
            SEMICOLON@11..12 ";"
    "#]],
    );
}

#[test]
fn single_quoted_literal() {
    check(
        r"<?php 'it\'s';",
        expect![[r#"
        SOURCE_FILE@0..14
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..14
            LITERAL_EXPR@6..13
              SINGLE_QUOTED@6..13 "'it\\'s'"
            SEMICOLON@13..14 ";"
    "#]],
    );
}

#[test]
fn magic_const() {
    check(
        "<?php __LINE__;",
        expect![[r#"
        SOURCE_FILE@0..15
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..15
            MAGIC_CONST_EXPR@6..14
              MAGIC_CONST@6..14 "__LINE__"
            SEMICOLON@14..15 ";"
    "#]],
    );
}

#[test]
fn bare_name_constant_fetch() {
    check(
        "<?php FOO;",
        expect![[r#"
        SOURCE_FILE@0..10
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..10
            IDENT@6..9 "FOO"
            SEMICOLON@9..10 ";"
    "#]],
    );
}

#[test]
fn qualified_name() {
    check(
        r"<?php A\B;",
        expect![[r#"
        SOURCE_FILE@0..10
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..10
            NAME_QUALIFIED@6..9 "A\\B"
            SEMICOLON@9..10 ";"
    "#]],
    );
}

#[test]
fn fully_qualified_name() {
    check(
        r"<?php \A\B;",
        expect![[r#"
        SOURCE_FILE@0..11
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..11
            NAME_FULLY_QUALIFIED@6..10 "\\A\\B"
            SEMICOLON@10..11 ";"
    "#]],
    );
}

#[test]
fn relative_name() {
    check(
        r"<?php namespace\A;",
        expect![[r#"
        SOURCE_FILE@0..18
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..18
            NAME_RELATIVE@6..17 "namespace\\A"
            SEMICOLON@17..18 ";"
    "#]],
    );
}

// ---------------------------------------------------------------------------
// Variables
// ---------------------------------------------------------------------------

#[test]
fn simple_variable() {
    check(
        "<?php $x;",
        expect![[r#"
        SOURCE_FILE@0..9
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..9
            SIMPLE_VAR_EXPR@6..8
              VARIABLE@6..8 "$x"
            SEMICOLON@8..9 ";"
    "#]],
    );
}

#[test]
fn variable_variable() {
    check(
        "<?php $$x;",
        expect![[r#"
        SOURCE_FILE@0..10
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..10
            VAR_VAR_EXPR@6..9
              DOLLAR@6..7 "$"
              SIMPLE_VAR_EXPR@7..9
                VARIABLE@7..9 "$x"
            SEMICOLON@9..10 ";"
    "#]],
    );
}

#[test]
fn dynamic_variable() {
    check(
        "<?php ${'a'};",
        expect![[r#"
        SOURCE_FILE@0..13
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..13
            DYNAMIC_VAR_EXPR@6..12
              DOLLAR@6..7 "$"
              L_CURLY@7..8 "{"
              LITERAL_EXPR@8..11
                SINGLE_QUOTED@8..11 "'a'"
              R_CURLY@11..12 "}"
            SEMICOLON@12..13 ";"
    "#]],
    );
}

// `$$x[0]` parses as `($$x)[0]`: php 7's uniform variable syntax.
#[test]
fn varvar_subscript_groups_left() {
    check(
        "<?php $$x[0];",
        expect![[r#"
            SOURCE_FILE@0..13
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..13
                SUBSCRIPT_EXPR@6..12
                  VAR_VAR_EXPR@6..9
                    DOLLAR@6..7 "$"
                    SIMPLE_VAR_EXPR@7..9
                      VARIABLE@7..9 "$x"
                  L_BRACK@9..10 "["
                  LITERAL_EXPR@10..11
                    INT@10..11 "0"
                  R_BRACK@11..12 "]"
                SEMICOLON@12..13 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Binary operators: precedence shape probes
// ---------------------------------------------------------------------------

#[test]
fn mul_binds_tighter_than_add() {
    check(
        "<?php 1 + 2 * 3;",
        expect![[r#"
            SOURCE_FILE@0..16
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..16
                BINARY_EXPR@6..15
                  LITERAL_EXPR@6..7
                    INT@6..7 "1"
                  PLUS@8..9 "+"
                  BINARY_EXPR@10..15
                    LITERAL_EXPR@10..11
                      INT@10..11 "2"
                    STAR@12..13 "*"
                    LITERAL_EXPR@14..15
                      INT@14..15 "3"
                SEMICOLON@15..16 ";"
        "#]],
    );
}

#[test]
fn pow_right_assoc_with_unary_rhs() {
    // php -r 'var_dump(2 ** -3 ** 2);' => 0.001953125 — `2 ** (-(3 ** 2))`.
    check(
        "<?php 2 ** -3 ** 2;",
        expect![[r#"
            SOURCE_FILE@0..19
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..19
                BINARY_EXPR@6..18
                  LITERAL_EXPR@6..7
                    INT@6..7 "2"
                  STAR_STAR@8..10 "**"
                  UNARY_EXPR@11..18
                    MINUS@11..12 "-"
                    BINARY_EXPR@12..18
                      LITERAL_EXPR@12..13
                        INT@12..13 "3"
                      STAR_STAR@14..16 "**"
                      LITERAL_EXPR@17..18
                        INT@17..18 "2"
                SEMICOLON@18..19 ";"
        "#]],
    );
}

#[test]
fn pow_binds_tighter_than_unary_minus() {
    // php -r 'var_dump(-2 ** 2);' => -4 — `-(2 ** 2)`.
    check(
        "<?php -2 ** 2;",
        expect![[r#"
            SOURCE_FILE@0..14
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..14
                UNARY_EXPR@6..13
                  MINUS@6..7 "-"
                  BINARY_EXPR@7..13
                    LITERAL_EXPR@7..8
                      INT@7..8 "2"
                    STAR_STAR@9..11 "**"
                    LITERAL_EXPR@12..13
                      INT@12..13 "2"
                SEMICOLON@13..14 ";"
        "#]],
    );
}

#[test]
fn concat_below_shift() {
    // php -r 'var_dump(1 . 2 << 3);' => "116" — `1 . (2 << 3)` since 8.0.
    check(
        "<?php 1 . 2 << 3;",
        expect![[r#"
            SOURCE_FILE@0..17
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..17
                BINARY_EXPR@6..16
                  LITERAL_EXPR@6..7
                    INT@6..7 "1"
                  DOT@8..9 "."
                  BINARY_EXPR@10..16
                    LITERAL_EXPR@10..11
                      INT@10..11 "2"
                    SHL@12..14 "<<"
                    LITERAL_EXPR@15..16
                      INT@15..16 "3"
                SEMICOLON@16..17 ";"
        "#]],
    );
}

#[test]
fn coalesce_right_nesting() {
    check(
        "<?php $a ?? $b ?? $c;",
        expect![[r#"
            SOURCE_FILE@0..21
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..21
                BINARY_EXPR@6..20
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  QUESTION_QUESTION@9..11 "??"
                  BINARY_EXPR@12..20
                    SIMPLE_VAR_EXPR@12..14
                      VARIABLE@12..14 "$b"
                    QUESTION_QUESTION@15..17 "??"
                    SIMPLE_VAR_EXPR@18..20
                      VARIABLE@18..20 "$c"
                SEMICOLON@20..21 ";"
        "#]],
    );
}

#[test]
fn pipe_operator() {
    // `|>` is 8.5; its tier is the Pratt table's, between `<` and `.`.
    check(
        "<?php $a |> strlen(...);",
        expect![[r#"
            SOURCE_FILE@0..24
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..24
                BINARY_EXPR@6..23
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  PIPE_GT@9..11 "|>"
                  CALL_EXPR@12..23
                    IDENT@12..18 "strlen"
                    ARG_LIST@18..23
                      L_PAREN@18..19 "("
                      VARIADIC_PLACEHOLDER@19..22
                        DOT_DOT_DOT@19..22 "..."
                      R_PAREN@22..23 ")"
                SEMICOLON@23..24 ";"
        "#]],
    );
}

#[test]
fn not_with_instanceof() {
    // php -r 'var_dump(!true instanceof stdClass);' => true — `!($x instanceof Y)`.
    check(
        "<?php !$x instanceof Y;",
        expect![[r#"
            SOURCE_FILE@0..23
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..23
                UNARY_EXPR@6..22
                  BANG@6..7 "!"
                  INSTANCEOF_EXPR@7..22
                    SIMPLE_VAR_EXPR@7..9
                      VARIABLE@7..9 "$x"
                    INSTANCEOF_KW@10..20 "instanceof"
                    IDENT@21..22 "Y"
                SEMICOLON@22..23 ";"
        "#]],
    );
}

#[test]
fn instanceof_static() {
    // php's `class_name_reference` takes `T_STATIC`: the late static
    // binding stands on the right as the name `static`.
    check(
        "<?php $x instanceof static;",
        expect![[r#"
            SOURCE_FILE@0..27
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..27
                INSTANCEOF_EXPR@6..26
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$x"
                  INSTANCEOF_KW@9..19 "instanceof"
                  IDENT@20..26 "static"
                SEMICOLON@26..27 ";"
        "#]],
    );
}

#[test]
fn instanceof_static_scoped_variable() {
    // A `new_variable` on the right: `static::$c` is a scoped access.
    check(
        "<?php $x instanceof static::$c;",
        expect![[r#"
            SOURCE_FILE@0..31
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..31
                INSTANCEOF_EXPR@6..30
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$x"
                  INSTANCEOF_KW@9..19 "instanceof"
                  SCOPED_ACCESS_EXPR@20..30
                    IDENT@20..26 "static"
                    COLON_COLON@26..28 "::"
                    SIMPLE_VAR_EXPR@28..30
                      VARIABLE@28..30 "$c"
                SEMICOLON@30..31 ";"
        "#]],
    );
}

#[test]
fn cast_with_pow() {
    // php -r 'var_dump((int)2.9 ** 2);' => 8 — `(int)($x ** 2)`.
    check(
        "<?php (int)$x ** 2;",
        expect![[r#"
            SOURCE_FILE@0..19
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..19
                CAST_EXPR@6..18
                  CAST@6..11 "(int)"
                  BINARY_EXPR@11..18
                    SIMPLE_VAR_EXPR@11..13
                      VARIABLE@11..13 "$x"
                    STAR_STAR@14..16 "**"
                    LITERAL_EXPR@17..18
                      INT@17..18 "2"
                SEMICOLON@18..19 ";"
        "#]],
    );
}

#[test]
fn logical_word_operators() {
    check(
        "<?php $a and $b xor $c or $d;",
        expect![[r#"
            SOURCE_FILE@0..29
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..29
                BINARY_EXPR@6..28
                  BINARY_EXPR@6..22
                    BINARY_EXPR@6..15
                      SIMPLE_VAR_EXPR@6..8
                        VARIABLE@6..8 "$a"
                      AND_KW@9..12 "and"
                      SIMPLE_VAR_EXPR@13..15
                        VARIABLE@13..15 "$b"
                    XOR_KW@16..19 "xor"
                    SIMPLE_VAR_EXPR@20..22
                      VARIABLE@20..22 "$c"
                  OR_KW@23..25 "or"
                  SIMPLE_VAR_EXPR@26..28
                    VARIABLE@26..28 "$d"
                SEMICOLON@28..29 ";"
        "#]],
    );
}

#[test]
fn boolean_symbol_operators() {
    check(
        "<?php $a && $b || $c;",
        expect![[r#"
            SOURCE_FILE@0..21
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..21
                BINARY_EXPR@6..20
                  BINARY_EXPR@6..14
                    SIMPLE_VAR_EXPR@6..8
                      VARIABLE@6..8 "$a"
                    AMP_AMP@9..11 "&&"
                    SIMPLE_VAR_EXPR@12..14
                      VARIABLE@12..14 "$b"
                  PIPE_PIPE@15..17 "||"
                  SIMPLE_VAR_EXPR@18..20
                    VARIABLE@18..20 "$c"
                SEMICOLON@20..21 ";"
        "#]],
    );
}

#[test]
fn bitwise_tiers() {
    check(
        "<?php $a | $b ^ $c & $d;",
        expect![[r#"
            SOURCE_FILE@0..24
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..24
                BINARY_EXPR@6..23
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  PIPE@9..10 "|"
                  BINARY_EXPR@11..23
                    SIMPLE_VAR_EXPR@11..13
                      VARIABLE@11..13 "$b"
                    CARET@14..15 "^"
                    BINARY_EXPR@16..23
                      SIMPLE_VAR_EXPR@16..18
                        VARIABLE@16..18 "$c"
                      AMP@19..20 "&"
                      SIMPLE_VAR_EXPR@21..23
                        VARIABLE@21..23 "$d"
                SEMICOLON@23..24 ";"
        "#]],
    );
}

#[test]
fn spaceship() {
    check(
        "<?php 1 <=> 2;",
        expect![[r#"
            SOURCE_FILE@0..14
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..14
                BINARY_EXPR@6..13
                  LITERAL_EXPR@6..7
                    INT@6..7 "1"
                  SPACESHIP@8..11 "<=>"
                  LITERAL_EXPR@12..13
                    INT@12..13 "2"
                SEMICOLON@13..14 ";"
        "#]],
    );
}

#[test]
fn comparison_non_assoc_rejected() {
    // A parse error in php; here the diagnostic, and the tree still built.
    check(
        "<?php 1 == 2 == 3;",
        expect![[r#"
            SOURCE_FILE@0..18
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..18
                BINARY_EXPR@6..17
                  BINARY_EXPR@6..12
                    LITERAL_EXPR@6..7
                      INT@6..7 "1"
                    EQ_EQ@8..10 "=="
                    LITERAL_EXPR@11..12
                      INT@11..12 "2"
                  EQ_EQ@13..15 "=="
                  LITERAL_EXPR@16..17
                    INT@16..17 "3"
                SEMICOLON@17..18 ";"
            error@13..15 syntax error, unexpected token "=="
        "#]],
    );
}

#[test]
fn comparison_different_tiers_chain() {
    // `<` and `==` are different tiers: `1 < 2 == 3` is legal in php.
    check(
        "<?php 1 < 2 == 3;",
        expect![[r#"
            SOURCE_FILE@0..17
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..17
                BINARY_EXPR@6..16
                  BINARY_EXPR@6..11
                    LITERAL_EXPR@6..7
                      INT@6..7 "1"
                    LT@8..9 "<"
                    LITERAL_EXPR@10..11
                      INT@10..11 "2"
                  EQ_EQ@12..14 "=="
                  LITERAL_EXPR@15..16
                    INT@15..16 "3"
                SEMICOLON@16..17 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Assignment
// ---------------------------------------------------------------------------

#[test]
fn assignment_chains_right() {
    check(
        "<?php $a = $b = 1;",
        expect![[r#"
            SOURCE_FILE@0..18
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..18
                ASSIGN_EXPR@6..17
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  EQ@9..10 "="
                  ASSIGN_EXPR@11..17
                    SIMPLE_VAR_EXPR@11..13
                      VARIABLE@11..13 "$b"
                    EQ@14..15 "="
                    LITERAL_EXPR@16..17
                      INT@16..17 "1"
                SEMICOLON@17..18 ";"
        "#]],
    );
}

#[test]
fn assignment_by_reference() {
    check(
        "<?php $a = &$b;",
        expect![[r#"
            SOURCE_FILE@0..15
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..15
                ASSIGN_EXPR@6..14
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  EQ@9..10 "="
                  AMP@11..12 "&"
                  SIMPLE_VAR_EXPR@12..14
                    VARIABLE@12..14 "$b"
                SEMICOLON@14..15 ";"
        "#]],
    );
}

#[test]
fn compound_assignments() {
    check(
        "<?php $a += 1; $a .= 'x'; $a **= 2; $a ??= 3;",
        expect![[r#"
            SOURCE_FILE@0..45
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..14
                ASSIGN_EXPR@6..13
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  PLUS_EQ@9..11 "+="
                  LITERAL_EXPR@12..13
                    INT@12..13 "1"
                SEMICOLON@13..14 ";"
              EXPR_STMT@15..25
                ASSIGN_EXPR@15..24
                  SIMPLE_VAR_EXPR@15..17
                    VARIABLE@15..17 "$a"
                  DOT_EQ@18..20 ".="
                  LITERAL_EXPR@21..24
                    SINGLE_QUOTED@21..24 "'x'"
                SEMICOLON@24..25 ";"
              EXPR_STMT@26..35
                ASSIGN_EXPR@26..34
                  SIMPLE_VAR_EXPR@26..28
                    VARIABLE@26..28 "$a"
                  STAR_STAR_EQ@29..32 "**="
                  LITERAL_EXPR@33..34
                    INT@33..34 "2"
                SEMICOLON@34..35 ";"
              EXPR_STMT@36..45
                ASSIGN_EXPR@36..44
                  SIMPLE_VAR_EXPR@36..38
                    VARIABLE@36..38 "$a"
                  QUESTION_QUESTION_EQ@39..42 "??="
                  LITERAL_EXPR@43..44
                    INT@43..44 "3"
                SEMICOLON@44..45 ";"
        "#]],
    );
}

#[test]
fn assignment_binds_above_and() {
    // `$a and $b = 1` is `$a and ($b = 1)`.
    check(
        "<?php $a and $b = 1;",
        expect![[r#"
            SOURCE_FILE@0..20
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..20
                BINARY_EXPR@6..19
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  AND_KW@9..12 "and"
                  ASSIGN_EXPR@13..19
                    SIMPLE_VAR_EXPR@13..15
                      VARIABLE@13..15 "$b"
                    EQ@16..17 "="
                    LITERAL_EXPR@18..19
                      INT@18..19 "1"
                SEMICOLON@19..20 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Ternary
// ---------------------------------------------------------------------------

#[test]
fn ternary_full() {
    check(
        "<?php $a ? 1 : 2;",
        expect![[r#"
            SOURCE_FILE@0..17
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..17
                TERNARY_EXPR@6..16
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  QUESTION@9..10 "?"
                  LITERAL_EXPR@11..12
                    INT@11..12 "1"
                  COLON@13..14 ":"
                  LITERAL_EXPR@15..16
                    INT@15..16 "2"
                SEMICOLON@16..17 ";"
        "#]],
    );
}

#[test]
fn ternary_short() {
    check(
        "<?php $a ?: 2;",
        expect![[r#"
            SOURCE_FILE@0..14
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..14
                TERNARY_EXPR@6..13
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  QUESTION@9..10 "?"
                  COLON@10..11 ":"
                  LITERAL_EXPR@12..13
                    INT@12..13 "2"
                SEMICOLON@13..14 ";"
        "#]],
    );
}

#[test]
fn short_ternary_chain_allowed() {
    // php -r 'var_dump(0 ?: 0 ?: 3);' prints 3: legal.
    check(
        "<?php $a ?: $b ?: $c;",
        expect![[r#"
            SOURCE_FILE@0..21
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..21
                TERNARY_EXPR@6..20
                  TERNARY_EXPR@6..14
                    SIMPLE_VAR_EXPR@6..8
                      VARIABLE@6..8 "$a"
                    QUESTION@9..10 "?"
                    COLON@10..11 ":"
                    SIMPLE_VAR_EXPR@12..14
                      VARIABLE@12..14 "$b"
                  QUESTION@15..16 "?"
                  COLON@16..17 ":"
                  SIMPLE_VAR_EXPR@18..20
                    VARIABLE@18..20 "$c"
                SEMICOLON@20..21 ";"
        "#]],
    );
}

#[test]
fn nested_ternary_needs_parens() {
    check(
        "<?php $a ? 1 : $b ? 2 : 3;",
        expect![[r#"
            SOURCE_FILE@0..26
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..26
                TERNARY_EXPR@6..25
                  TERNARY_EXPR@6..17
                    SIMPLE_VAR_EXPR@6..8
                      VARIABLE@6..8 "$a"
                    QUESTION@9..10 "?"
                    LITERAL_EXPR@11..12
                      INT@11..12 "1"
                    COLON@13..14 ":"
                    SIMPLE_VAR_EXPR@15..17
                      VARIABLE@15..17 "$b"
                  QUESTION@18..19 "?"
                  LITERAL_EXPR@20..21
                    INT@20..21 "2"
                  COLON@22..23 ":"
                  LITERAL_EXPR@24..25
                    INT@24..25 "3"
                SEMICOLON@25..26 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Unary, incdec, casts, error suppression
// ---------------------------------------------------------------------------

#[test]
fn postfix_increment() {
    check(
        "<?php $x++;",
        expect![[r#"
        SOURCE_FILE@0..11
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..11
            POSTFIX_INC_DEC_EXPR@6..10
              SIMPLE_VAR_EXPR@6..8
                VARIABLE@6..8 "$x"
              PLUS_PLUS@8..10 "++"
            SEMICOLON@10..11 ";"
    "#]],
    );
}

#[test]
fn prefix_decrement() {
    check(
        "<?php --$x;",
        expect![[r#"
        SOURCE_FILE@0..11
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..11
            PREFIX_INC_DEC_EXPR@6..10
              MINUS_MINUS@6..8 "--"
              SIMPLE_VAR_EXPR@8..10
                VARIABLE@8..10 "$x"
            SEMICOLON@10..11 ";"
    "#]],
    );
}

#[test]
fn cast_simple() {
    check(
        "<?php (bool)$x;",
        expect![[r#"
        SOURCE_FILE@0..15
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..15
            CAST_EXPR@6..14
              CAST@6..12 "(bool)"
              SIMPLE_VAR_EXPR@12..14
                VARIABLE@12..14 "$x"
            SEMICOLON@14..15 ";"
    "#]],
    );
}

#[test]
fn cast_interior_whitespace() {
    check(
        "<?php ( int )$x;",
        expect![[r#"
        SOURCE_FILE@0..16
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..16
            CAST_EXPR@6..15
              CAST@6..13 "( int )"
              SIMPLE_VAR_EXPR@13..15
                VARIABLE@13..15 "$x"
            SEMICOLON@15..16 ";"
    "#]],
    );
}

#[test]
fn error_suppress() {
    check(
        "<?php @$x->load();",
        expect![[r#"
        SOURCE_FILE@0..18
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..18
            ERROR_SUPPRESS_EXPR@6..17
              AT@6..7 "@"
              CALL_EXPR@7..17
                MEMBER_ACCESS_EXPR@7..15
                  SIMPLE_VAR_EXPR@7..9
                    VARIABLE@7..9 "$x"
                  ARROW@9..11 "->"
                  MEMBER_NAME@11..15
                    IDENT@11..15 "load"
                ARG_LIST@15..17
                  L_PAREN@15..16 "("
                  R_PAREN@16..17 ")"
            SEMICOLON@17..18 ";"
    "#]],
    );
}

// ---------------------------------------------------------------------------
// clone / print / throw / yield / include
// ---------------------------------------------------------------------------

#[test]
fn clone_expr() {
    check(
        "<?php clone $x;",
        expect![[r#"
            SOURCE_FILE@0..15
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..15
                CLONE_EXPR@6..14
                  CLONE_KW@6..11 "clone"
                  SIMPLE_VAR_EXPR@12..14
                    VARIABLE@12..14 "$x"
                SEMICOLON@14..15 ";"
        "#]],
    );
}

#[test]
fn print_expr() {
    check(
        "<?php print 'a' . 'b';",
        expect![[r#"
            SOURCE_FILE@0..22
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..22
                PRINT_EXPR@6..21
                  PRINT_KW@6..11 "print"
                  BINARY_EXPR@12..21
                    LITERAL_EXPR@12..15
                      SINGLE_QUOTED@12..15 "'a'"
                    DOT@16..17 "."
                    LITERAL_EXPR@18..21
                      SINGLE_QUOTED@18..21 "'b'"
                SEMICOLON@21..22 ";"
        "#]],
    );
}

#[test]
fn throw_expr_in_coalesce() {
    check(
        "<?php $a ?? throw new E();",
        expect![[r#"
            SOURCE_FILE@0..26
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..26
                BINARY_EXPR@6..25
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  QUESTION_QUESTION@9..11 "??"
                  THROW_EXPR@12..25
                    THROW_KW@12..17 "throw"
                    NEW_EXPR@18..25
                      NEW_KW@18..21 "new"
                      IDENT@22..23 "E"
                      ARG_LIST@23..25
                        L_PAREN@23..24 "("
                        R_PAREN@24..25 ")"
                SEMICOLON@25..26 ";"
        "#]],
    );
}

#[test]
fn yield_bare() {
    check(
        "<?php function g() { yield; }",
        expect![[r#"
            SOURCE_FILE@0..29
              OPEN_TAG@0..6 "<?php "
              FUNCTION_DECL@6..29
                FUNCTION_KW@6..14 "function"
                IDENT@15..16 "g"
                PARAM_LIST@16..18
                  L_PAREN@16..17 "("
                  R_PAREN@17..18 ")"
                BLOCK@19..29
                  L_CURLY@19..20 "{"
                  EXPR_STMT@21..27
                    YIELD_EXPR@21..26
                      YIELD_KW@21..26 "yield"
                    SEMICOLON@26..27 ";"
                  R_CURLY@28..29 "}"
        "#]],
    );
}

#[test]
fn yield_value() {
    check(
        "<?php function g() { yield 1; }",
        expect![[r#"
            SOURCE_FILE@0..31
              OPEN_TAG@0..6 "<?php "
              FUNCTION_DECL@6..31
                FUNCTION_KW@6..14 "function"
                IDENT@15..16 "g"
                PARAM_LIST@16..18
                  L_PAREN@16..17 "("
                  R_PAREN@17..18 ")"
                BLOCK@19..31
                  L_CURLY@19..20 "{"
                  EXPR_STMT@21..29
                    YIELD_EXPR@21..28
                      YIELD_KW@21..26 "yield"
                      LITERAL_EXPR@27..28
                        INT@27..28 "1"
                    SEMICOLON@28..29 ";"
                  R_CURLY@30..31 "}"
        "#]],
    );
}

#[test]
fn yield_key_value() {
    check(
        "<?php function g() { yield $k => $v; }",
        expect![[r#"
            SOURCE_FILE@0..38
              OPEN_TAG@0..6 "<?php "
              FUNCTION_DECL@6..38
                FUNCTION_KW@6..14 "function"
                IDENT@15..16 "g"
                PARAM_LIST@16..18
                  L_PAREN@16..17 "("
                  R_PAREN@17..18 ")"
                BLOCK@19..38
                  L_CURLY@19..20 "{"
                  EXPR_STMT@21..36
                    YIELD_EXPR@21..35
                      YIELD_KW@21..26 "yield"
                      YIELD_KEY@27..32
                        SIMPLE_VAR_EXPR@27..29
                          VARIABLE@27..29 "$k"
                        FAT_ARROW@30..32 "=>"
                      SIMPLE_VAR_EXPR@33..35
                        VARIABLE@33..35 "$v"
                    SEMICOLON@35..36 ";"
                  R_CURLY@37..38 "}"
        "#]],
    );
}

#[test]
fn yield_from() {
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
fn include_require_forms() {
    check(
        "<?php include 'a'; include_once 'b'; require 'c'; require_once 'd';",
        expect![[r#"
            SOURCE_FILE@0..67
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..18
                INCLUDE_EXPR@6..17
                  INCLUDE_KW@6..13 "include"
                  LITERAL_EXPR@14..17
                    SINGLE_QUOTED@14..17 "'a'"
                SEMICOLON@17..18 ";"
              EXPR_STMT@19..36
                INCLUDE_EXPR@19..35
                  INCLUDE_ONCE_KW@19..31 "include_once"
                  LITERAL_EXPR@32..35
                    SINGLE_QUOTED@32..35 "'b'"
                SEMICOLON@35..36 ";"
              EXPR_STMT@37..49
                INCLUDE_EXPR@37..48
                  REQUIRE_KW@37..44 "require"
                  LITERAL_EXPR@45..48
                    SINGLE_QUOTED@45..48 "'c'"
                SEMICOLON@48..49 ";"
              EXPR_STMT@50..67
                INCLUDE_EXPR@50..66
                  REQUIRE_ONCE_KW@50..62 "require_once"
                  LITERAL_EXPR@63..66
                    SINGLE_QUOTED@63..66 "'d'"
                SEMICOLON@66..67 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// isset / empty / eval / exit / die
// ---------------------------------------------------------------------------

#[test]
fn isset_multi_arg() {
    check(
        "<?php isset($a, $b[0]);",
        expect![[r#"
            SOURCE_FILE@0..23
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..23
                ISSET_EXPR@6..22
                  ISSET_KW@6..11 "isset"
                  L_PAREN@11..12 "("
                  SIMPLE_VAR_EXPR@12..14
                    VARIABLE@12..14 "$a"
                  COMMA@14..15 ","
                  SUBSCRIPT_EXPR@16..21
                    SIMPLE_VAR_EXPR@16..18
                      VARIABLE@16..18 "$b"
                    L_BRACK@18..19 "["
                    LITERAL_EXPR@19..20
                      INT@19..20 "0"
                    R_BRACK@20..21 "]"
                  R_PAREN@21..22 ")"
                SEMICOLON@22..23 ";"
        "#]],
    );
}

#[test]
fn empty_expr() {
    check(
        "<?php empty($a);",
        expect![[r#"
        SOURCE_FILE@0..16
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..16
            EMPTY_EXPR@6..15
              EMPTY_KW@6..11 "empty"
              L_PAREN@11..12 "("
              SIMPLE_VAR_EXPR@12..14
                VARIABLE@12..14 "$a"
              R_PAREN@14..15 ")"
            SEMICOLON@15..16 ";"
    "#]],
    );
}

#[test]
fn eval_expr() {
    check(
        "<?php eval($code);",
        expect![[r#"
        SOURCE_FILE@0..18
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..18
            EVAL_EXPR@6..17
              EVAL_KW@6..10 "eval"
              L_PAREN@10..11 "("
              SIMPLE_VAR_EXPR@11..16
                VARIABLE@11..16 "$code"
              R_PAREN@16..17 ")"
            SEMICOLON@17..18 ";"
    "#]],
    );
}

#[test]
fn exit_bare_and_with_args() {
    check(
        "<?php exit; exit(1);",
        expect![[r#"
            SOURCE_FILE@0..20
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..11
                EXIT_EXPR@6..10
                  EXIT_KW@6..10 "exit"
                SEMICOLON@10..11 ";"
              EXPR_STMT@12..20
                EXIT_EXPR@12..19
                  EXIT_KW@12..16 "exit"
                  ARG_LIST@16..19
                    L_PAREN@16..17 "("
                    LITERAL_EXPR@17..18
                      INT@17..18 "1"
                    R_PAREN@18..19 ")"
                SEMICOLON@19..20 ";"
        "#]],
    );
}

#[test]
fn die_bare_and_with_args() {
    check(
        "<?php die; die('msg');",
        expect![[r#"
            SOURCE_FILE@0..22
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..10
                EXIT_EXPR@6..9
                  DIE_KW@6..9 "die"
                SEMICOLON@9..10 ";"
              EXPR_STMT@11..22
                EXIT_EXPR@11..21
                  DIE_KW@11..14 "die"
                  ARG_LIST@14..21
                    L_PAREN@14..15 "("
                    LITERAL_EXPR@15..20
                      SINGLE_QUOTED@15..20 "'msg'"
                    R_PAREN@20..21 ")"
                SEMICOLON@21..22 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Arrays and destructuring
// ---------------------------------------------------------------------------

#[test]
fn array_short() {
    check(
        "<?php [1, 2];",
        expect![[r#"
            SOURCE_FILE@0..13
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..13
                ARRAY_EXPR@6..12
                  L_BRACK@6..7 "["
                  ARRAY_ELEMENT@7..8
                    LITERAL_EXPR@7..8
                      INT@7..8 "1"
                  COMMA@8..9 ","
                  ARRAY_ELEMENT@10..11
                    LITERAL_EXPR@10..11
                      INT@10..11 "2"
                  R_BRACK@11..12 "]"
                SEMICOLON@12..13 ";"
        "#]],
    );
}

#[test]
fn array_long() {
    check(
        "<?php array(1);",
        expect![[r#"
        SOURCE_FILE@0..15
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..15
            ARRAY_EXPR@6..14
              ARRAY_KW@6..11 "array"
              L_PAREN@11..12 "("
              ARRAY_ELEMENT@12..13
                LITERAL_EXPR@12..13
                  INT@12..13 "1"
              R_PAREN@13..14 ")"
            SEMICOLON@14..15 ";"
    "#]],
    );
}

#[test]
fn array_keys_spread_byref() {
    check(
        "<?php ['a' => 1, ...$s, &$r];",
        expect![[r#"
            SOURCE_FILE@0..29
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..29
                ARRAY_EXPR@6..28
                  L_BRACK@6..7 "["
                  ARRAY_ELEMENT@7..15
                    ARRAY_KEY@7..13
                      LITERAL_EXPR@7..10
                        SINGLE_QUOTED@7..10 "'a'"
                      FAT_ARROW@11..13 "=>"
                    LITERAL_EXPR@14..15
                      INT@14..15 "1"
                  COMMA@15..16 ","
                  ARRAY_ELEMENT@17..22
                    DOT_DOT_DOT@17..20 "..."
                    SIMPLE_VAR_EXPR@20..22
                      VARIABLE@20..22 "$s"
                  COMMA@22..23 ","
                  ARRAY_ELEMENT@24..27
                    AMP@24..25 "&"
                    SIMPLE_VAR_EXPR@25..27
                      VARIABLE@25..27 "$r"
                  R_BRACK@27..28 "]"
                SEMICOLON@28..29 ";"
        "#]],
    );
}

#[test]
fn destructuring_nested() {
    check(
        "<?php [$a, [$b, $c]] = $x;",
        expect![[r#"
            SOURCE_FILE@0..26
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..26
                ASSIGN_EXPR@6..25
                  ARRAY_EXPR@6..20
                    L_BRACK@6..7 "["
                    ARRAY_ELEMENT@7..9
                      SIMPLE_VAR_EXPR@7..9
                        VARIABLE@7..9 "$a"
                    COMMA@9..10 ","
                    ARRAY_ELEMENT@11..19
                      ARRAY_EXPR@11..19
                        L_BRACK@11..12 "["
                        ARRAY_ELEMENT@12..14
                          SIMPLE_VAR_EXPR@12..14
                            VARIABLE@12..14 "$b"
                        COMMA@14..15 ","
                        ARRAY_ELEMENT@16..18
                          SIMPLE_VAR_EXPR@16..18
                            VARIABLE@16..18 "$c"
                        R_BRACK@18..19 "]"
                    R_BRACK@19..20 "]"
                  EQ@21..22 "="
                  SIMPLE_VAR_EXPR@23..25
                    VARIABLE@23..25 "$x"
                SEMICOLON@25..26 ";"
        "#]],
    );
}

#[test]
fn destructuring_list_keyed() {
    check(
        "<?php list('k' => $v) = $x;",
        expect![[r#"
            SOURCE_FILE@0..27
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..27
                ASSIGN_EXPR@6..26
                  LIST_EXPR@6..21
                    LIST_KW@6..10 "list"
                    L_PAREN@10..11 "("
                    ARRAY_ELEMENT@11..20
                      ARRAY_KEY@11..17
                        LITERAL_EXPR@11..14
                          SINGLE_QUOTED@11..14 "'k'"
                        FAT_ARROW@15..17 "=>"
                      SIMPLE_VAR_EXPR@18..20
                        VARIABLE@18..20 "$v"
                    R_PAREN@20..21 ")"
                  EQ@22..23 "="
                  SIMPLE_VAR_EXPR@24..26
                    VARIABLE@24..26 "$x"
                SEMICOLON@26..27 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// match
// ---------------------------------------------------------------------------

#[test]
fn match_with_default_multicond_trailing_comma() {
    check(
        "<?php match ($x) { 1, 2 => 'a', default => 'b', };",
        expect![[r#"
            SOURCE_FILE@0..50
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..50
                MATCH_EXPR@6..49
                  MATCH_KW@6..11 "match"
                  L_PAREN@12..13 "("
                  SIMPLE_VAR_EXPR@13..15
                    VARIABLE@13..15 "$x"
                  R_PAREN@15..16 ")"
                  L_CURLY@17..18 "{"
                  MATCH_ARM@19..30
                    MATCH_CONDS@19..23
                      LITERAL_EXPR@19..20
                        INT@19..20 "1"
                      COMMA@20..21 ","
                      LITERAL_EXPR@22..23
                        INT@22..23 "2"
                    FAT_ARROW@24..26 "=>"
                    LITERAL_EXPR@27..30
                      SINGLE_QUOTED@27..30 "'a'"
                  COMMA@30..31 ","
                  MATCH_ARM@32..46
                    DEFAULT_KW@32..39 "default"
                    FAT_ARROW@40..42 "=>"
                    LITERAL_EXPR@43..46
                      SINGLE_QUOTED@43..46 "'b'"
                  COMMA@46..47 ","
                  R_CURLY@48..49 "}"
                SEMICOLON@49..50 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// new
// ---------------------------------------------------------------------------

#[test]
fn new_bare() {
    check(
        "<?php new Foo;",
        expect![[r#"
            SOURCE_FILE@0..14
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..14
                NEW_EXPR@6..13
                  NEW_KW@6..9 "new"
                  IDENT@10..13 "Foo"
                SEMICOLON@13..14 ";"
        "#]],
    );
}

#[test]
fn new_with_args() {
    check(
        "<?php new Foo(1);",
        expect![[r#"
            SOURCE_FILE@0..17
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..17
                NEW_EXPR@6..16
                  NEW_KW@6..9 "new"
                  IDENT@10..13 "Foo"
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
fn new_dynamic_class() {
    check(
        "<?php new $class;",
        expect![[r#"
            SOURCE_FILE@0..17
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..17
                NEW_EXPR@6..16
                  NEW_KW@6..9 "new"
                  SIMPLE_VAR_EXPR@10..16
                    VARIABLE@10..16 "$class"
                SEMICOLON@16..17 ";"
        "#]],
    );
}

#[test]
fn new_parenthesized_expression() {
    check(
        "<?php new (expr())();",
        expect![[r#"
            SOURCE_FILE@0..21
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..21
                NEW_EXPR@6..20
                  NEW_KW@6..9 "new"
                  PAREN_EXPR@10..18
                    L_PAREN@10..11 "("
                    CALL_EXPR@11..17
                      IDENT@11..15 "expr"
                      ARG_LIST@15..17
                        L_PAREN@15..16 "("
                        R_PAREN@16..17 ")"
                    R_PAREN@17..18 ")"
                  ARG_LIST@18..20
                    L_PAREN@18..19 "("
                    R_PAREN@19..20 ")"
                SEMICOLON@20..21 ";"
        "#]],
    );
}

#[test]
fn new_anon_class_with_args_extends() {
    check(
        "<?php new class(1) extends B {};",
        expect![[r#"
            SOURCE_FILE@0..32
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..32
                NEW_EXPR@6..31
                  NEW_KW@6..9 "new"
                  ANON_CLASS_DECL@10..31
                    CLASS_KW@10..15 "class"
                    ARG_LIST@15..18
                      L_PAREN@15..16 "("
                      LITERAL_EXPR@16..17
                        INT@16..17 "1"
                      R_PAREN@17..18 ")"
                    EXTENDS_CLAUSE@19..28
                      EXTENDS_KW@19..26 "extends"
                      IDENT@27..28 "B"
                    CLASS_BODY@29..31
                      L_CURLY@29..30 "{"
                      R_CURLY@30..31 "}"
                SEMICOLON@31..32 ";"
        "#]],
    );
}

#[test]
fn new_member_chain_84() {
    // 8.4+: `new Foo()->bar` — postfix chain requires the arg parens.
    check(
        "<?php new Foo()->bar;",
        expect![[r#"
            SOURCE_FILE@0..21
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..21
                MEMBER_ACCESS_EXPR@6..20
                  NEW_EXPR@6..15
                    NEW_KW@6..9 "new"
                    IDENT@10..13 "Foo"
                    ARG_LIST@13..15
                      L_PAREN@13..14 "("
                      R_PAREN@14..15 ")"
                  ARROW@15..17 "->"
                  MEMBER_NAME@17..20
                    IDENT@17..20 "bar"
                SEMICOLON@20..21 ";"
        "#]],
    );
}

// `new Foo->bar` is a parse error in php: `->` is never a class-reference
// trailer on a bare name, and an argument-less `new` is not
// dereferenceable (the 8.4 rule), so the dangling `->` is rejected
// exactly at php's error site.
#[test]
fn new_member_chain_without_parens_is_error() {
    check(
        "<?php new Foo->bar;",
        expect![[r#"
            SOURCE_FILE@0..19
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..19
                NEW_EXPR@6..13
                  NEW_KW@6..9 "new"
                  IDENT@10..13 "Foo"
                ERROR@13..18
                  ARROW@13..15 "->"
                  IDENT@15..18 "bar"
                SEMICOLON@18..19 ";"
            error@13..15 syntax error, unexpected token "->"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Closures and arrow functions
// ---------------------------------------------------------------------------

#[test]
fn closure_with_use_byref_and_ret() {
    check(
        "<?php function () use (&$x): int {};",
        expect![[r#"
            SOURCE_FILE@0..36
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..36
                CLOSURE_EXPR@6..35
                  FUNCTION_KW@6..14 "function"
                  PARAM_LIST@15..17
                    L_PAREN@15..16 "("
                    R_PAREN@16..17 ")"
                  CLOSURE_USE_CLAUSE@18..27
                    USE_KW@18..21 "use"
                    L_PAREN@22..23 "("
                    CLOSURE_USE_VAR@23..26
                      AMP@23..24 "&"
                      VARIABLE@24..26 "$x"
                    R_PAREN@26..27 ")"
                  RET_TYPE@27..32
                    COLON@27..28 ":"
                    NAMED_TYPE@29..32
                      IDENT@29..32 "int"
                  BLOCK@33..35
                    L_CURLY@33..34 "{"
                    R_CURLY@34..35 "}"
                SEMICOLON@35..36 ";"
        "#]],
    );
}

#[test]
fn arrow_fn_simple() {
    check(
        "<?php fn($x) => $x + 1;",
        expect![[r#"
            SOURCE_FILE@0..23
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..23
                ARROW_FN_EXPR@6..22
                  FN_KW@6..8 "fn"
                  PARAM_LIST@8..12
                    L_PAREN@8..9 "("
                    PARAM@9..11
                      VARIABLE@9..11 "$x"
                    R_PAREN@11..12 ")"
                  FAT_ARROW@13..15 "=>"
                  BINARY_EXPR@16..22
                    SIMPLE_VAR_EXPR@16..18
                      VARIABLE@16..18 "$x"
                    PLUS@19..20 "+"
                    LITERAL_EXPR@21..22
                      INT@21..22 "1"
                SEMICOLON@22..23 ";"
        "#]],
    );
}

#[test]
fn arrow_fn_body_extent_in_args() {
    // Body binds lowest but stops at the argument comma.
    check(
        "<?php f(fn() => 1, 2);",
        expect![[r#"
            SOURCE_FILE@0..22
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..22
                CALL_EXPR@6..21
                  IDENT@6..7 "f"
                  ARG_LIST@7..21
                    L_PAREN@7..8 "("
                    ARROW_FN_EXPR@8..17
                      FN_KW@8..10 "fn"
                      PARAM_LIST@10..12
                        L_PAREN@10..11 "("
                        R_PAREN@11..12 ")"
                      FAT_ARROW@13..15 "=>"
                      LITERAL_EXPR@16..17
                        INT@16..17 "1"
                    COMMA@17..18 ","
                    LITERAL_EXPR@19..20
                      INT@19..20 "2"
                    R_PAREN@20..21 ")"
                SEMICOLON@21..22 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Calls: FCC, named args, spreads
// ---------------------------------------------------------------------------

#[test]
fn first_class_callable() {
    check(
        "<?php strlen(...);",
        expect![[r#"
        SOURCE_FILE@0..18
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..18
            CALL_EXPR@6..17
              IDENT@6..12 "strlen"
              ARG_LIST@12..17
                L_PAREN@12..13 "("
                VARIADIC_PLACEHOLDER@13..16
                  DOT_DOT_DOT@13..16 "..."
                R_PAREN@16..17 ")"
            SEMICOLON@17..18 ";"
    "#]],
    );
}

#[test]
fn named_argument() {
    check(
        "<?php f(name: 1);",
        expect![[r#"
            SOURCE_FILE@0..17
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..17
                CALL_EXPR@6..16
                  IDENT@6..7 "f"
                  ARG_LIST@7..16
                    L_PAREN@7..8 "("
                    NAMED_ARG@8..15
                      IDENT@8..12 "name"
                      COLON@12..13 ":"
                      LITERAL_EXPR@14..15
                        INT@14..15 "1"
                    R_PAREN@15..16 ")"
                SEMICOLON@16..17 ";"
        "#]],
    );
}

#[test]
fn spread_argument() {
    check(
        "<?php f(...$args);",
        expect![[r#"
        SOURCE_FILE@0..18
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..18
            CALL_EXPR@6..17
              IDENT@6..7 "f"
              ARG_LIST@7..17
                L_PAREN@7..8 "("
                SPREAD_ARG@8..16
                  DOT_DOT_DOT@8..11 "..."
                  SIMPLE_VAR_EXPR@11..16
                    VARIABLE@11..16 "$args"
                R_PAREN@16..17 ")"
            SEMICOLON@17..18 ";"
    "#]],
    );
}

// ---------------------------------------------------------------------------
// Member / scoped access chains and subscripts
// ---------------------------------------------------------------------------

#[test]
fn access_chain_mixed() {
    check(
        "<?php $a->b?->c::$d::CONST_A;",
        expect![[r#"
        SOURCE_FILE@0..29
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..29
            SCOPED_ACCESS_EXPR@6..28
              SCOPED_ACCESS_EXPR@6..19
                MEMBER_ACCESS_EXPR@6..15
                  MEMBER_ACCESS_EXPR@6..11
                    SIMPLE_VAR_EXPR@6..8
                      VARIABLE@6..8 "$a"
                    ARROW@8..10 "->"
                    MEMBER_NAME@10..11
                      IDENT@10..11 "b"
                  NULLSAFE_ARROW@11..14 "?->"
                  MEMBER_NAME@14..15
                    IDENT@14..15 "c"
                COLON_COLON@15..17 "::"
                SIMPLE_VAR_EXPR@17..19
                  VARIABLE@17..19 "$d"
              COLON_COLON@19..21 "::"
              MEMBER_NAME@21..28
                IDENT@21..28 "CONST_A"
            SEMICOLON@28..29 ";"
    "#]],
    );
}

#[test]
fn class_constant_fetch() {
    check(
        "<?php Foo::class;",
        expect![[r#"
        SOURCE_FILE@0..17
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..17
            SCOPED_ACCESS_EXPR@6..16
              IDENT@6..9 "Foo"
              COLON_COLON@9..11 "::"
              CLASS_KW@11..16 "class"
            SEMICOLON@16..17 ";"
    "#]],
    );
}

#[test]
fn braced_member_access() {
    check(
        "<?php $obj->{$expr};",
        expect![[r#"
        SOURCE_FILE@0..20
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..20
            MEMBER_ACCESS_EXPR@6..19
              SIMPLE_VAR_EXPR@6..10
                VARIABLE@6..10 "$obj"
              ARROW@10..12 "->"
              BRACED_MEMBER@12..19
                L_CURLY@12..13 "{"
                SIMPLE_VAR_EXPR@13..18
                  VARIABLE@13..18 "$expr"
                R_CURLY@18..19 "}"
            SEMICOLON@19..20 ";"
    "#]],
    );
}

#[test]
fn scoped_braced_call() {
    check(
        "<?php Foo::{'m'}();",
        expect![[r#"
        SOURCE_FILE@0..19
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..19
            CALL_EXPR@6..18
              SCOPED_ACCESS_EXPR@6..16
                IDENT@6..9 "Foo"
                COLON_COLON@9..11 "::"
                BRACED_MEMBER@11..16
                  L_CURLY@11..12 "{"
                  LITERAL_EXPR@12..15
                    SINGLE_QUOTED@12..15 "'m'"
                  R_CURLY@15..16 "}"
              ARG_LIST@16..18
                L_PAREN@16..17 "("
                R_PAREN@17..18 ")"
            SEMICOLON@18..19 ";"
    "#]],
    );
}

#[test]
fn subscript_chain() {
    check(
        "<?php $a[1][2];",
        expect![[r#"
        SOURCE_FILE@0..15
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..15
            SUBSCRIPT_EXPR@6..14
              SUBSCRIPT_EXPR@6..11
                SIMPLE_VAR_EXPR@6..8
                  VARIABLE@6..8 "$a"
                L_BRACK@8..9 "["
                LITERAL_EXPR@9..10
                  INT@9..10 "1"
                R_BRACK@10..11 "]"
              L_BRACK@11..12 "["
              LITERAL_EXPR@12..13
                INT@12..13 "2"
              R_BRACK@13..14 "]"
            SEMICOLON@14..15 ";"
    "#]],
    );
}

#[test]
fn subscript_push_write() {
    check(
        "<?php $a[] = 1;",
        expect![[r#"
            SOURCE_FILE@0..15
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..15
                ASSIGN_EXPR@6..14
                  SUBSCRIPT_EXPR@6..10
                    SIMPLE_VAR_EXPR@6..8
                      VARIABLE@6..8 "$a"
                    L_BRACK@8..9 "["
                    R_BRACK@9..10 "]"
                  EQ@11..12 "="
                  LITERAL_EXPR@13..14
                    INT@13..14 "1"
                SEMICOLON@14..15 ";"
        "#]],
    );
}

#[test]
fn legacy_curly_offset() {
    check(
        "<?php $s{0};",
        expect![[r#"
            SOURCE_FILE@0..12
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..8
                SIMPLE_VAR_EXPR@6..8
                  VARIABLE@6..8 "$s"
              BLOCK@8..11
                L_CURLY@8..9 "{"
                EXPR_STMT@9..10
                  LITERAL_EXPR@9..10
                    INT@9..10 "0"
                R_CURLY@10..11 "}"
              EMPTY_STMT@11..12
                SEMICOLON@11..12 ";"
            error@8..9 syntax error, unexpected token "{"
            error@10..11 syntax error, unexpected token "}"
        "#]],
    );
}
