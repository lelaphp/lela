//! Zend's `variable`-production pins (design §4.9): the assignment,
//! by-reference and unset targets, `new`'s dereference rights, a
//! standalone `list`, and the group-`use` shapes. Every rejected case is a
//! `php -l` parse error pinned on 8.5.6; every accepted case is a legal
//! neighbour a sloppier rule would break. Read at Mahout `1b3104a8` and
//! regenerated once against the arena's dump.

mod support;

use expect_test::expect;
use support::check;

// ---------------------------------------------------------------------------
// new: class-reference trailer rights + 8.4 dereference rule
// ---------------------------------------------------------------------------

#[test]
fn new_bare_name_takes_no_subscript() {
    check(
        "<?php echo new A[0];",
        expect![[r#"
            SOURCE_FILE@0..20
              OPEN_TAG@0..6 "<?php "
              ECHO_STMT@6..20
                ECHO_KW@6..10 "echo"
                NEW_EXPR@11..16
                  NEW_KW@11..14 "new"
                  IDENT@15..16 "A"
                ERROR@16..19
                  L_BRACK@16..17 "["
                  INT@17..18 "0"
                  R_BRACK@18..19 "]"
                SEMICOLON@19..20 ";"
            error@16..17 syntax error, unexpected token "[", expecting "," or ";"
        "#]],
    );
}

#[test]
fn new_bare_name_takes_no_nullsafe() {
    check(
        "<?php new A?->p;",
        expect![[r#"
            SOURCE_FILE@0..16
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..16
                NEW_EXPR@6..11
                  NEW_KW@6..9 "new"
                  IDENT@10..11 "A"
                ERROR@11..15
                  NULLSAFE_ARROW@11..14 "?->"
                  IDENT@14..15 "p"
                SEMICOLON@15..16 ";"
            error@11..14 syntax error, unexpected token "?->"
        "#]],
    );
}

#[test]
fn new_scoped_selector_must_be_variable() {
    check(
        "<?php echo new A::C;",
        expect![[r#"
            SOURCE_FILE@0..20
              OPEN_TAG@0..6 "<?php "
              ECHO_STMT@6..20
                ECHO_KW@6..10 "echo"
                NEW_EXPR@11..19
                  NEW_KW@11..14 "new"
                  SCOPED_ACCESS_EXPR@15..19
                    IDENT@15..16 "A"
                    COLON_COLON@16..18 "::"
                    MEMBER_NAME@18..19
                      IDENT@18..19 "C"
                SEMICOLON@19..20 ";"
            error@18..19 syntax error, unexpected identifier "C", expecting variable or "$"
        "#]],
    );
}

#[test]
fn new_scoped_selector_must_be_variable_on_dynamic_prefix() {
    check(
        "<?php new $c::C;",
        expect![[r#"
            SOURCE_FILE@0..16
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..16
                NEW_EXPR@6..15
                  NEW_KW@6..9 "new"
                  SCOPED_ACCESS_EXPR@10..15
                    SIMPLE_VAR_EXPR@10..12
                      VARIABLE@10..12 "$c"
                    COLON_COLON@12..14 "::"
                    MEMBER_NAME@14..15
                      IDENT@14..15 "C"
                SEMICOLON@15..16 ";"
            error@14..15 syntax error, unexpected identifier "C", expecting variable or "$"
        "#]],
    );
}

#[test]
fn new_paren_prefix_takes_no_trailers() {
    check(
        "<?php new ('A')->m();",
        expect![[r#"
            SOURCE_FILE@0..21
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..21
                NEW_EXPR@6..15
                  NEW_KW@6..9 "new"
                  PAREN_EXPR@10..15
                    L_PAREN@10..11 "("
                    LITERAL_EXPR@11..14
                      SINGLE_QUOTED@11..14 "'A'"
                    R_PAREN@14..15 ")"
                ERROR@15..20
                  ARROW@15..17 "->"
                  IDENT@17..18 "m"
                  L_PAREN@18..19 "("
                  R_PAREN@19..20 ")"
                SEMICOLON@20..21 ";"
            error@15..17 syntax error, unexpected token "->"
        "#]],
    );
}

#[test]
fn new_static_prop_chain_is_legal() {
    check(
        "<?php new A::$cls->m();",
        expect![[r#"
            SOURCE_FILE@0..23
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..23
                NEW_EXPR@6..22
                  NEW_KW@6..9 "new"
                  MEMBER_ACCESS_EXPR@10..20
                    SCOPED_ACCESS_EXPR@10..17
                      IDENT@10..11 "A"
                      COLON_COLON@11..13 "::"
                      SIMPLE_VAR_EXPR@13..17
                        VARIABLE@13..17 "$cls"
                    ARROW@17..19 "->"
                    MEMBER_NAME@19..20
                      IDENT@19..20 "m"
                  ARG_LIST@20..22
                    L_PAREN@20..21 "("
                    R_PAREN@21..22 ")"
                SEMICOLON@22..23 ";"
        "#]],
    );
}

#[test]
fn new_with_args_dereferences() {
    check(
        "<?php new A()->p;",
        expect![[r#"
            SOURCE_FILE@0..17
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..17
                MEMBER_ACCESS_EXPR@6..16
                  NEW_EXPR@6..13
                    NEW_KW@6..9 "new"
                    IDENT@10..11 "A"
                    ARG_LIST@11..13
                      L_PAREN@11..12 "("
                      R_PAREN@12..13 ")"
                  ARROW@13..15 "->"
                  MEMBER_NAME@15..16
                    IDENT@15..16 "p"
                SEMICOLON@16..17 ";"
        "#]],
    );
}

#[test]
fn new_without_args_is_not_an_assignment_target() {
    check(
        "<?php new A() = 1;",
        expect![[r#"
            SOURCE_FILE@0..18
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..18
                NEW_EXPR@6..13
                  NEW_KW@6..9 "new"
                  IDENT@10..11 "A"
                  ARG_LIST@11..13
                    L_PAREN@11..12 "("
                    R_PAREN@12..13 ")"
                ERROR@14..17
                  EQ@14..15 "="
                  INT@16..17 "1"
                SEMICOLON@17..18 ";"
            error@14..15 syntax error, unexpected token "="
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Assignment binds only variable productions (+ `=` destructuring)
// ---------------------------------------------------------------------------

#[test]
fn class_const_is_not_an_assignment_target() {
    check(
        "<?php A::C = 2;",
        expect![[r#"
            SOURCE_FILE@0..15
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..15
                SCOPED_ACCESS_EXPR@6..10
                  IDENT@6..7 "A"
                  COLON_COLON@7..9 "::"
                  MEMBER_NAME@9..10
                    IDENT@9..10 "C"
                ERROR@11..14
                  EQ@11..12 "="
                  INT@13..14 "2"
                SEMICOLON@14..15 ";"
            error@11..12 syntax error, unexpected token "="
        "#]],
    );
}

#[test]
fn class_const_compound_assign_rejected() {
    check(
        "<?php A::C ??= 2;",
        expect![[r#"
            SOURCE_FILE@0..17
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..17
                SCOPED_ACCESS_EXPR@6..10
                  IDENT@6..7 "A"
                  COLON_COLON@7..9 "::"
                  MEMBER_NAME@9..10
                    IDENT@9..10 "C"
                ERROR@11..16
                  QUESTION_QUESTION_EQ@11..14 "??="
                  INT@15..16 "2"
                SEMICOLON@16..17 ";"
            error@11..14 syntax error, unexpected token "??="
        "#]],
    );
}

#[test]
fn literal_is_not_an_assignment_target() {
    check(
        "<?php 'str' = 1;",
        expect![[r#"
            SOURCE_FILE@0..16
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..16
                LITERAL_EXPR@6..11
                  SINGLE_QUOTED@6..11 "'str'"
                ERROR@12..15
                  EQ@12..13 "="
                  INT@14..15 "1"
                SEMICOLON@15..16 ";"
            error@12..13 syntax error, unexpected token "="
        "#]],
    );
}

#[test]
fn paren_expr_is_not_an_assignment_target() {
    check(
        "<?php ($a) = 1;",
        expect![[r#"
            SOURCE_FILE@0..15
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..15
                PAREN_EXPR@6..10
                  L_PAREN@6..7 "("
                  SIMPLE_VAR_EXPR@7..9
                    VARIABLE@7..9 "$a"
                  R_PAREN@9..10 ")"
                ERROR@11..14
                  EQ@11..12 "="
                  INT@13..14 "1"
                SEMICOLON@14..15 ";"
            error@11..12 syntax error, unexpected token "="
        "#]],
    );
}

#[test]
fn array_destructuring_takes_plain_eq_only() {
    check(
        "<?php [$a] += [1];",
        expect![[r#"
            SOURCE_FILE@0..18
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..18
                ARRAY_EXPR@6..10
                  L_BRACK@6..7 "["
                  ARRAY_ELEMENT@7..9
                    SIMPLE_VAR_EXPR@7..9
                      VARIABLE@7..9 "$a"
                  R_BRACK@9..10 "]"
                ERROR@11..17
                  PLUS_EQ@11..13 "+="
                  L_BRACK@14..15 "["
                  INT@15..16 "1"
                  R_BRACK@16..17 "]"
                SEMICOLON@17..18 ";"
            error@11..13 syntax error, unexpected token "+="
        "#]],
    );
}

#[test]
fn static_prop_and_subscripted_const_still_assign() {
    check(
        "<?php A::$p = 2; A::C[0] = 2;",
        expect![[r#"
            SOURCE_FILE@0..29
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..16
                ASSIGN_EXPR@6..15
                  SCOPED_ACCESS_EXPR@6..11
                    IDENT@6..7 "A"
                    COLON_COLON@7..9 "::"
                    SIMPLE_VAR_EXPR@9..11
                      VARIABLE@9..11 "$p"
                  EQ@12..13 "="
                  LITERAL_EXPR@14..15
                    INT@14..15 "2"
                SEMICOLON@15..16 ";"
              EXPR_STMT@17..29
                ASSIGN_EXPR@17..28
                  SUBSCRIPT_EXPR@17..24
                    SCOPED_ACCESS_EXPR@17..21
                      IDENT@17..18 "A"
                      COLON_COLON@18..20 "::"
                      MEMBER_NAME@20..21
                        IDENT@20..21 "C"
                    L_BRACK@21..22 "["
                    LITERAL_EXPR@22..23
                      INT@22..23 "0"
                    R_BRACK@23..24 "]"
                  EQ@25..26 "="
                  LITERAL_EXPR@27..28
                    INT@27..28 "2"
                SEMICOLON@28..29 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// `= &` takes exactly one variable production
// ---------------------------------------------------------------------------

#[test]
fn ref_of_class_const_rejected() {
    check(
        "<?php $a = &A::C;",
        expect![[r#"
            SOURCE_FILE@0..17
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..17
                ASSIGN_EXPR@6..16
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  EQ@9..10 "="
                  AMP@11..12 "&"
                  SCOPED_ACCESS_EXPR@12..16
                    IDENT@12..13 "A"
                    COLON_COLON@13..15 "::"
                    MEMBER_NAME@15..16
                      IDENT@15..16 "C"
                SEMICOLON@16..17 ";"
            error@16..17 syntax error, unexpected token ";", expecting "->" or "?->" or "["
        "#]],
    );
}

#[test]
fn ref_of_bare_new_rejected() {
    check(
        "<?php $r = &new A();",
        expect![[r#"
            SOURCE_FILE@0..20
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..20
                ASSIGN_EXPR@6..19
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$r"
                  EQ@9..10 "="
                  AMP@11..12 "&"
                  NEW_EXPR@12..19
                    NEW_KW@12..15 "new"
                    IDENT@16..17 "A"
                    ARG_LIST@17..19
                      L_PAREN@17..18 "("
                      R_PAREN@18..19 ")"
                SEMICOLON@19..20 ";"
            error@19..20 syntax error, unexpected token ";", expecting "->" or "?->" or "["
        "#]],
    );
}

#[test]
fn ref_binds_one_variable_then_yields_to_enclosing() {
    // Upstream shape: `($a = &$b) + 1`.
    check(
        "<?php $a = &$b + 1;",
        expect![[r#"
            SOURCE_FILE@0..19
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..19
                BINARY_EXPR@6..18
                  ASSIGN_EXPR@6..14
                    SIMPLE_VAR_EXPR@6..8
                      VARIABLE@6..8 "$a"
                    EQ@9..10 "="
                    AMP@11..12 "&"
                    SIMPLE_VAR_EXPR@12..14
                      VARIABLE@12..14 "$b"
                  PLUS@15..16 "+"
                  LITERAL_EXPR@17..18
                    INT@17..18 "1"
                SEMICOLON@18..19 ";"
        "#]],
    );
}

#[test]
fn ref_of_call_is_legal() {
    check(
        "<?php $a = &foo();",
        expect![[r#"
            SOURCE_FILE@0..18
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..18
                ASSIGN_EXPR@6..17
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  EQ@9..10 "="
                  AMP@11..12 "&"
                  CALL_EXPR@12..17
                    IDENT@12..15 "foo"
                    ARG_LIST@15..17
                      L_PAREN@15..16 "("
                      R_PAREN@16..17 ")"
                SEMICOLON@17..18 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// unset() arguments are variable productions
// ---------------------------------------------------------------------------

#[test]
fn unset_of_binary_expr_rejected() {
    check(
        "<?php unset($x + $y);",
        expect![[r#"
            SOURCE_FILE@0..21
              OPEN_TAG@0..6 "<?php "
              UNSET_STMT@6..21
                UNSET_KW@6..11 "unset"
                L_PAREN@11..12 "("
                SIMPLE_VAR_EXPR@12..14
                  VARIABLE@12..14 "$x"
                ERROR@15..20
                  PLUS@15..16 "+"
                  VARIABLE@17..19 "$y"
                  R_PAREN@19..20 ")"
                SEMICOLON@20..21 ";"
            error@15..16 syntax error, unexpected token "+", expecting "->" or "?->" or "["
        "#]],
    );
}

#[test]
fn unset_of_bare_new_rejected() {
    check(
        "<?php unset(new A());",
        expect![[r#"
            SOURCE_FILE@0..21
              OPEN_TAG@0..6 "<?php "
              UNSET_STMT@6..21
                UNSET_KW@6..11 "unset"
                L_PAREN@11..12 "("
                NEW_EXPR@12..19
                  NEW_KW@12..15 "new"
                  IDENT@16..17 "A"
                  ARG_LIST@17..19
                    L_PAREN@17..18 "("
                    R_PAREN@18..19 ")"
                R_PAREN@19..20 ")"
                SEMICOLON@20..21 ";"
            error@19..20 syntax error, unexpected token ")", expecting "->" or "?->" or "["
        "#]],
    );
}

#[test]
fn unset_variable_list_with_trailing_comma() {
    check(
        "<?php unset($a, $o->p, $b['k'],);",
        expect![[r#"
            SOURCE_FILE@0..33
              OPEN_TAG@0..6 "<?php "
              UNSET_STMT@6..33
                UNSET_KW@6..11 "unset"
                L_PAREN@11..12 "("
                SIMPLE_VAR_EXPR@12..14
                  VARIABLE@12..14 "$a"
                COMMA@14..15 ","
                MEMBER_ACCESS_EXPR@16..21
                  SIMPLE_VAR_EXPR@16..18
                    VARIABLE@16..18 "$o"
                  ARROW@18..20 "->"
                  MEMBER_NAME@20..21
                    IDENT@20..21 "p"
                COMMA@21..22 ","
                SUBSCRIPT_EXPR@23..30
                  SIMPLE_VAR_EXPR@23..25
                    VARIABLE@23..25 "$b"
                  L_BRACK@25..26 "["
                  LITERAL_EXPR@26..29
                    SINGLE_QUOTED@26..29 "'k'"
                  R_BRACK@29..30 "]"
                COMMA@30..31 ","
                R_PAREN@31..32 ")"
                SEMICOLON@32..33 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// list() exists only as a destructuring target
// ---------------------------------------------------------------------------

#[test]
fn list_in_call_argument_rejected() {
    check(
        "<?php var_dump(list($a));",
        expect![[r#"
            SOURCE_FILE@0..25
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..25
                CALL_EXPR@6..24
                  IDENT@6..14 "var_dump"
                  ARG_LIST@14..24
                    L_PAREN@14..15 "("
                    LIST_EXPR@15..23
                      LIST_KW@15..19 "list"
                      L_PAREN@19..20 "("
                      ARRAY_ELEMENT@20..22
                        SIMPLE_VAR_EXPR@20..22
                          VARIABLE@20..22 "$a"
                      R_PAREN@22..23 ")"
                    R_PAREN@23..24 ")"
                SEMICOLON@24..25 ";"
            error@23..24 syntax error, unexpected token ")", expecting "="
        "#]],
    );
}

#[test]
fn list_takes_plain_eq_only() {
    check(
        "<?php list($a) == $b;",
        expect![[r#"
            SOURCE_FILE@0..21
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..21
                BINARY_EXPR@6..20
                  LIST_EXPR@6..14
                    LIST_KW@6..10 "list"
                    L_PAREN@10..11 "("
                    ARRAY_ELEMENT@11..13
                      SIMPLE_VAR_EXPR@11..13
                        VARIABLE@11..13 "$a"
                    R_PAREN@13..14 ")"
                  EQ_EQ@15..17 "=="
                  SIMPLE_VAR_EXPR@18..20
                    VARIABLE@18..20 "$b"
                SEMICOLON@20..21 ";"
            error@15..17 syntax error, unexpected token "==", expecting "="
        "#]],
    );
}

#[test]
fn list_as_foreach_target_is_legal() {
    check(
        "<?php foreach ($x as $k => list($a)) {}",
        expect![[r#"
            SOURCE_FILE@0..39
              OPEN_TAG@0..6 "<?php "
              FOREACH_STMT@6..39
                FOREACH_KW@6..13 "foreach"
                L_PAREN@14..15 "("
                SIMPLE_VAR_EXPR@15..17
                  VARIABLE@15..17 "$x"
                AS_KW@18..20 "as"
                FOREACH_KEY@21..26
                  SIMPLE_VAR_EXPR@21..23
                    VARIABLE@21..23 "$k"
                  FAT_ARROW@24..26 "=>"
                FOREACH_TARGET@27..35
                  LIST_EXPR@27..35
                    LIST_KW@27..31 "list"
                    L_PAREN@31..32 "("
                    ARRAY_ELEMENT@32..34
                      SIMPLE_VAR_EXPR@32..34
                        VARIABLE@32..34 "$a"
                    R_PAREN@34..35 ")"
                R_PAREN@35..36 ")"
                BLOCK@37..39
                  L_CURLY@37..38 "{"
                  R_CURLY@38..39 "}"
        "#]],
    );
}

#[test]
fn nested_list_needs_no_own_eq() {
    check(
        "<?php list(list($a), $b) = $c;",
        expect![[r#"
            SOURCE_FILE@0..30
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..30
                ASSIGN_EXPR@6..29
                  LIST_EXPR@6..24
                    LIST_KW@6..10 "list"
                    L_PAREN@10..11 "("
                    ARRAY_ELEMENT@11..19
                      LIST_EXPR@11..19
                        LIST_KW@11..15 "list"
                        L_PAREN@15..16 "("
                        ARRAY_ELEMENT@16..18
                          SIMPLE_VAR_EXPR@16..18
                            VARIABLE@16..18 "$a"
                        R_PAREN@18..19 ")"
                    COMMA@19..20 ","
                    ARRAY_ELEMENT@21..23
                      SIMPLE_VAR_EXPR@21..23
                        VARIABLE@21..23 "$b"
                    R_PAREN@23..24 ")"
                  EQ@25..26 "="
                  SIMPLE_VAR_EXPR@27..29
                    VARIABLE@27..29 "$c"
                SEMICOLON@29..30 ";"
        "#]],
    );
}

#[test]
fn list_assignment_as_subexpression_is_legal() {
    check(
        "<?php f(list($a) = $b);",
        expect![[r#"
            SOURCE_FILE@0..23
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..23
                CALL_EXPR@6..22
                  IDENT@6..7 "f"
                  ARG_LIST@7..22
                    L_PAREN@7..8 "("
                    ASSIGN_EXPR@8..21
                      LIST_EXPR@8..16
                        LIST_KW@8..12 "list"
                        L_PAREN@12..13 "("
                        ARRAY_ELEMENT@13..15
                          SIMPLE_VAR_EXPR@13..15
                            VARIABLE@13..15 "$a"
                        R_PAREN@15..16 ")"
                      EQ@17..18 "="
                      SIMPLE_VAR_EXPR@19..21
                        VARIABLE@19..21 "$b"
                    R_PAREN@21..22 ")"
                SEMICOLON@22..23 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// group use: at least one name, prefix-relative names
// ---------------------------------------------------------------------------

#[test]
fn empty_group_use_rejected() {
    check(
        "<?php use Baz\\{};",
        expect![[r#"
            SOURCE_FILE@0..17
              OPEN_TAG@0..6 "<?php "
              USE_STMT@6..17
                USE_KW@6..9 "use"
                USE_GROUP@10..16
                  IDENT@10..13 "Baz"
                  BACKSLASH@13..14 "\\"
                  L_CURLY@14..15 "{"
                  R_CURLY@15..16 "}"
                SEMICOLON@16..17 ";"
            error@15..16 syntax error, unexpected token "}", expecting identifier or namespaced name or "function" or "const"
        "#]],
    );
}

#[test]
fn fully_qualified_name_inside_group_use_rejected() {
    check(
        "<?php use Foo\\{Baz, \\Qux};",
        expect![[r#"
            SOURCE_FILE@0..26
              OPEN_TAG@0..6 "<?php "
              USE_STMT@6..26
                USE_KW@6..9 "use"
                USE_GROUP@10..25
                  IDENT@10..13 "Foo"
                  BACKSLASH@13..14 "\\"
                  L_CURLY@14..15 "{"
                  USE_GROUP_ITEM@15..18
                    IDENT@15..18 "Baz"
                  COMMA@18..19 ","
                  USE_GROUP_ITEM@20..24
                    NAME_FULLY_QUALIFIED@20..24 "\\Qux"
                  R_CURLY@24..25 "}"
                SEMICOLON@25..26 ";"
            error@20..24 syntax error, unexpected fully qualified name "\Qux", expecting "}"
        "#]],
    );
}

#[test]
fn group_use_trailing_comma_is_legal() {
    check(
        "<?php use Foo\\{Baz,};",
        expect![[r#"
            SOURCE_FILE@0..21
              OPEN_TAG@0..6 "<?php "
              USE_STMT@6..21
                USE_KW@6..9 "use"
                USE_GROUP@10..20
                  IDENT@10..13 "Foo"
                  BACKSLASH@13..14 "\\"
                  L_CURLY@14..15 "{"
                  USE_GROUP_ITEM@15..18
                    IDENT@15..18 "Baz"
                  COMMA@18..19 ","
                  R_CURLY@19..20 "}"
                SEMICOLON@20..21 ";"
        "#]],
    );
}
