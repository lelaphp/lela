//! Statement fixtures (design §4.9): control flow in brace and all six
//! alternative forms, simple statements, tags and inline HTML, and
//! `__halt_compiler`. Read at Mahout `1b3104a8` and regenerated once
//! against the arena's dump.

mod support;

use expect_test::expect;
use support::check;

// ---------------------------------------------------------------------------
// if / elseif / else
// ---------------------------------------------------------------------------

#[test]
fn if_elseif_else() {
    check(
        "<?php if ($a) {} elseif ($b) {} else {}",
        expect![[r#"
            SOURCE_FILE@0..39
              OPEN_TAG@0..6 "<?php "
              IF_STMT@6..39
                IF_KW@6..8 "if"
                L_PAREN@9..10 "("
                SIMPLE_VAR_EXPR@10..12
                  VARIABLE@10..12 "$a"
                R_PAREN@12..13 ")"
                BLOCK@14..16
                  L_CURLY@14..15 "{"
                  R_CURLY@15..16 "}"
                ELSEIF_CLAUSE@17..31
                  ELSEIF_KW@17..23 "elseif"
                  L_PAREN@24..25 "("
                  SIMPLE_VAR_EXPR@25..27
                    VARIABLE@25..27 "$b"
                  R_PAREN@27..28 ")"
                  BLOCK@29..31
                    L_CURLY@29..30 "{"
                    R_CURLY@30..31 "}"
                ELSE_CLAUSE@32..39
                  ELSE_KW@32..36 "else"
                  BLOCK@37..39
                    L_CURLY@37..38 "{"
                    R_CURLY@38..39 "}"
        "#]],
    );
}

#[test]
fn else_if_two_words_nests() {
    // `else if` is ELSE_CLAUSE { IF_STMT }, php's own shape, no diagnostic.
    check(
        "<?php if ($a) {} else if ($b) {}",
        expect![[r#"
            SOURCE_FILE@0..32
              OPEN_TAG@0..6 "<?php "
              IF_STMT@6..32
                IF_KW@6..8 "if"
                L_PAREN@9..10 "("
                SIMPLE_VAR_EXPR@10..12
                  VARIABLE@10..12 "$a"
                R_PAREN@12..13 ")"
                BLOCK@14..16
                  L_CURLY@14..15 "{"
                  R_CURLY@15..16 "}"
                ELSE_CLAUSE@17..32
                  ELSE_KW@17..21 "else"
                  IF_STMT@22..32
                    IF_KW@22..24 "if"
                    L_PAREN@25..26 "("
                    SIMPLE_VAR_EXPR@26..28
                      VARIABLE@26..28 "$b"
                    R_PAREN@28..29 ")"
                    BLOCK@30..32
                      L_CURLY@30..31 "{"
                      R_CURLY@31..32 "}"
        "#]],
    );
}

#[test]
fn if_alt_form() {
    check(
        "<?php if ($a): elseif ($b): else: endif;",
        expect![[r#"
            SOURCE_FILE@0..40
              OPEN_TAG@0..6 "<?php "
              IF_STMT@6..40
                IF_KW@6..8 "if"
                L_PAREN@9..10 "("
                SIMPLE_VAR_EXPR@10..12
                  VARIABLE@10..12 "$a"
                R_PAREN@12..13 ")"
                COLON@13..14 ":"
                STMT_LIST@15..15
                ALT_ELSEIF_CLAUSE@15..28
                  ELSEIF_KW@15..21 "elseif"
                  L_PAREN@22..23 "("
                  SIMPLE_VAR_EXPR@23..25
                    VARIABLE@23..25 "$b"
                  R_PAREN@25..26 ")"
                  COLON@26..27 ":"
                  STMT_LIST@28..28
                ALT_ELSE_CLAUSE@28..34
                  ELSE_KW@28..32 "else"
                  COLON@32..33 ":"
                  STMT_LIST@34..34
                ENDIF_KW@34..39 "endif"
                SEMICOLON@39..40 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// The six alternative forms
// ---------------------------------------------------------------------------

#[test]
fn while_alt_form() {
    check(
        "<?php while ($a): break; endwhile;",
        expect![[r#"
            SOURCE_FILE@0..34
              OPEN_TAG@0..6 "<?php "
              WHILE_STMT@6..34
                WHILE_KW@6..11 "while"
                L_PAREN@12..13 "("
                SIMPLE_VAR_EXPR@13..15
                  VARIABLE@13..15 "$a"
                R_PAREN@15..16 ")"
                COLON@16..17 ":"
                STMT_LIST@18..24
                  BREAK_STMT@18..24
                    BREAK_KW@18..23 "break"
                    SEMICOLON@23..24 ";"
                ENDWHILE_KW@25..33 "endwhile"
                SEMICOLON@33..34 ";"
        "#]],
    );
}

#[test]
fn for_alt_form() {
    check(
        "<?php for (;;): break; endfor;",
        expect![[r#"
            SOURCE_FILE@0..30
              OPEN_TAG@0..6 "<?php "
              FOR_STMT@6..30
                FOR_KW@6..9 "for"
                L_PAREN@10..11 "("
                FOR_INIT@11..11
                SEMICOLON@11..12 ";"
                FOR_COND@12..12
                SEMICOLON@12..13 ";"
                FOR_STEP@13..13
                R_PAREN@13..14 ")"
                COLON@14..15 ":"
                STMT_LIST@16..22
                  BREAK_STMT@16..22
                    BREAK_KW@16..21 "break"
                    SEMICOLON@21..22 ";"
                ENDFOR_KW@23..29 "endfor"
                SEMICOLON@29..30 ";"
        "#]],
    );
}

#[test]
fn foreach_alt_form() {
    check(
        "<?php foreach ($a as $v): endforeach;",
        expect![[r#"
            SOURCE_FILE@0..37
              OPEN_TAG@0..6 "<?php "
              FOREACH_STMT@6..37
                FOREACH_KW@6..13 "foreach"
                L_PAREN@14..15 "("
                SIMPLE_VAR_EXPR@15..17
                  VARIABLE@15..17 "$a"
                AS_KW@18..20 "as"
                FOREACH_TARGET@21..23
                  SIMPLE_VAR_EXPR@21..23
                    VARIABLE@21..23 "$v"
                R_PAREN@23..24 ")"
                COLON@24..25 ":"
                STMT_LIST@26..26
                ENDFOREACH_KW@26..36 "endforeach"
                SEMICOLON@36..37 ";"
        "#]],
    );
}

#[test]
fn switch_alt_form() {
    check(
        "<?php switch ($a): case 1: break; endswitch;",
        expect![[r#"
            SOURCE_FILE@0..44
              OPEN_TAG@0..6 "<?php "
              SWITCH_STMT@6..44
                SWITCH_KW@6..12 "switch"
                L_PAREN@13..14 "("
                SIMPLE_VAR_EXPR@14..16
                  VARIABLE@14..16 "$a"
                R_PAREN@16..17 ")"
                COLON@17..18 ":"
                SWITCH_ARM@19..33
                  CASE_KW@19..23 "case"
                  LITERAL_EXPR@24..25
                    INT@24..25 "1"
                  COLON@25..26 ":"
                  STMT_LIST@27..33
                    BREAK_STMT@27..33
                      BREAK_KW@27..32 "break"
                      SEMICOLON@32..33 ";"
                ENDSWITCH_KW@34..43 "endswitch"
                SEMICOLON@43..44 ";"
        "#]],
    );
}

#[test]
fn declare_alt_form() {
    check(
        "<?php declare(ticks=1): enddeclare;",
        expect![[r#"
            SOURCE_FILE@0..35
              OPEN_TAG@0..6 "<?php "
              DECLARE_STMT@6..35
                DECLARE_KW@6..13 "declare"
                L_PAREN@13..14 "("
                DECLARE_DIRECTIVE@14..21
                  IDENT@14..19 "ticks"
                  EQ@19..20 "="
                  LITERAL_EXPR@20..21
                    INT@20..21 "1"
                R_PAREN@21..22 ")"
                COLON@22..23 ":"
                STMT_LIST@24..24
                ENDDECLARE_KW@24..34 "enddeclare"
                SEMICOLON@34..35 ";"
        "#]],
    );
}

#[test]
fn nested_mixed_alt_and_brace() {
    // Legal in php: an alternative `if` wrapping a braced `while`.
    check(
        "<?php if ($a): while ($b) { echo 1; } endif;",
        expect![[r#"
            SOURCE_FILE@0..44
              OPEN_TAG@0..6 "<?php "
              IF_STMT@6..44
                IF_KW@6..8 "if"
                L_PAREN@9..10 "("
                SIMPLE_VAR_EXPR@10..12
                  VARIABLE@10..12 "$a"
                R_PAREN@12..13 ")"
                COLON@13..14 ":"
                STMT_LIST@15..37
                  WHILE_STMT@15..37
                    WHILE_KW@15..20 "while"
                    L_PAREN@21..22 "("
                    SIMPLE_VAR_EXPR@22..24
                      VARIABLE@22..24 "$b"
                    R_PAREN@24..25 ")"
                    BLOCK@26..37
                      L_CURLY@26..27 "{"
                      ECHO_STMT@28..35
                        ECHO_KW@28..32 "echo"
                        LITERAL_EXPR@33..34
                          INT@33..34 "1"
                        SEMICOLON@34..35 ";"
                      R_CURLY@36..37 "}"
                ENDIF_KW@38..43 "endif"
                SEMICOLON@43..44 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Loops
// ---------------------------------------------------------------------------

#[test]
fn do_while() {
    check(
        "<?php do {} while ($a);",
        expect![[r#"
            SOURCE_FILE@0..23
              OPEN_TAG@0..6 "<?php "
              DO_WHILE_STMT@6..23
                DO_KW@6..8 "do"
                BLOCK@9..11
                  L_CURLY@9..10 "{"
                  R_CURLY@10..11 "}"
                WHILE_KW@12..17 "while"
                L_PAREN@18..19 "("
                SIMPLE_VAR_EXPR@19..21
                  VARIABLE@19..21 "$a"
                R_PAREN@21..22 ")"
                SEMICOLON@22..23 ";"
        "#]],
    );
}

#[test]
fn for_empty_clauses() {
    check(
        "<?php for (;;) {}",
        expect![[r#"
            SOURCE_FILE@0..17
              OPEN_TAG@0..6 "<?php "
              FOR_STMT@6..17
                FOR_KW@6..9 "for"
                L_PAREN@10..11 "("
                FOR_INIT@11..11
                SEMICOLON@11..12 ";"
                FOR_COND@12..12
                SEMICOLON@12..13 ";"
                FOR_STEP@13..13
                R_PAREN@13..14 ")"
                BLOCK@15..17
                  L_CURLY@15..16 "{"
                  R_CURLY@16..17 "}"
        "#]],
    );
}

#[test]
fn for_multiple_clauses() {
    check(
        "<?php for ($i=0, $j=1; $i<5; $i++, $j--) {}",
        expect![[r#"
            SOURCE_FILE@0..43
              OPEN_TAG@0..6 "<?php "
              FOR_STMT@6..43
                FOR_KW@6..9 "for"
                L_PAREN@10..11 "("
                FOR_INIT@11..21
                  ASSIGN_EXPR@11..15
                    SIMPLE_VAR_EXPR@11..13
                      VARIABLE@11..13 "$i"
                    EQ@13..14 "="
                    LITERAL_EXPR@14..15
                      INT@14..15 "0"
                  COMMA@15..16 ","
                  ASSIGN_EXPR@17..21
                    SIMPLE_VAR_EXPR@17..19
                      VARIABLE@17..19 "$j"
                    EQ@19..20 "="
                    LITERAL_EXPR@20..21
                      INT@20..21 "1"
                SEMICOLON@21..22 ";"
                FOR_COND@23..27
                  BINARY_EXPR@23..27
                    SIMPLE_VAR_EXPR@23..25
                      VARIABLE@23..25 "$i"
                    LT@25..26 "<"
                    LITERAL_EXPR@26..27
                      INT@26..27 "5"
                SEMICOLON@27..28 ";"
                FOR_STEP@29..39
                  POSTFIX_INC_DEC_EXPR@29..33
                    SIMPLE_VAR_EXPR@29..31
                      VARIABLE@29..31 "$i"
                    PLUS_PLUS@31..33 "++"
                  COMMA@33..34 ","
                  POSTFIX_INC_DEC_EXPR@35..39
                    SIMPLE_VAR_EXPR@35..37
                      VARIABLE@35..37 "$j"
                    MINUS_MINUS@37..39 "--"
                R_PAREN@39..40 ")"
                BLOCK@41..43
                  L_CURLY@41..42 "{"
                  R_CURLY@42..43 "}"
        "#]],
    );
}

#[test]
fn foreach_value() {
    check(
        "<?php foreach ($a as $v) {}",
        expect![[r#"
            SOURCE_FILE@0..27
              OPEN_TAG@0..6 "<?php "
              FOREACH_STMT@6..27
                FOREACH_KW@6..13 "foreach"
                L_PAREN@14..15 "("
                SIMPLE_VAR_EXPR@15..17
                  VARIABLE@15..17 "$a"
                AS_KW@18..20 "as"
                FOREACH_TARGET@21..23
                  SIMPLE_VAR_EXPR@21..23
                    VARIABLE@21..23 "$v"
                R_PAREN@23..24 ")"
                BLOCK@25..27
                  L_CURLY@25..26 "{"
                  R_CURLY@26..27 "}"
        "#]],
    );
}

#[test]
fn foreach_key_value() {
    check(
        "<?php foreach ($a as $k => $v) {}",
        expect![[r#"
            SOURCE_FILE@0..33
              OPEN_TAG@0..6 "<?php "
              FOREACH_STMT@6..33
                FOREACH_KW@6..13 "foreach"
                L_PAREN@14..15 "("
                SIMPLE_VAR_EXPR@15..17
                  VARIABLE@15..17 "$a"
                AS_KW@18..20 "as"
                FOREACH_KEY@21..26
                  SIMPLE_VAR_EXPR@21..23
                    VARIABLE@21..23 "$k"
                  FAT_ARROW@24..26 "=>"
                FOREACH_TARGET@27..29
                  SIMPLE_VAR_EXPR@27..29
                    VARIABLE@27..29 "$v"
                R_PAREN@29..30 ")"
                BLOCK@31..33
                  L_CURLY@31..32 "{"
                  R_CURLY@32..33 "}"
        "#]],
    );
}

#[test]
fn foreach_byref_value() {
    check(
        "<?php foreach ($a as &$v) {}",
        expect![[r#"
            SOURCE_FILE@0..28
              OPEN_TAG@0..6 "<?php "
              FOREACH_STMT@6..28
                FOREACH_KW@6..13 "foreach"
                L_PAREN@14..15 "("
                SIMPLE_VAR_EXPR@15..17
                  VARIABLE@15..17 "$a"
                AS_KW@18..20 "as"
                FOREACH_TARGET@21..24
                  AMP@21..22 "&"
                  SIMPLE_VAR_EXPR@22..24
                    VARIABLE@22..24 "$v"
                R_PAREN@24..25 ")"
                BLOCK@26..28
                  L_CURLY@26..27 "{"
                  R_CURLY@27..28 "}"
        "#]],
    );
}

#[test]
fn foreach_key_byref_value() {
    check(
        "<?php foreach ($a as $k => &$v) {}",
        expect![[r#"
            SOURCE_FILE@0..34
              OPEN_TAG@0..6 "<?php "
              FOREACH_STMT@6..34
                FOREACH_KW@6..13 "foreach"
                L_PAREN@14..15 "("
                SIMPLE_VAR_EXPR@15..17
                  VARIABLE@15..17 "$a"
                AS_KW@18..20 "as"
                FOREACH_KEY@21..26
                  SIMPLE_VAR_EXPR@21..23
                    VARIABLE@21..23 "$k"
                  FAT_ARROW@24..26 "=>"
                FOREACH_TARGET@27..30
                  AMP@27..28 "&"
                  SIMPLE_VAR_EXPR@28..30
                    VARIABLE@28..30 "$v"
                R_PAREN@30..31 ")"
                BLOCK@32..34
                  L_CURLY@32..33 "{"
                  R_CURLY@33..34 "}"
        "#]],
    );
}

#[test]
fn foreach_destructuring_target() {
    check(
        "<?php foreach ($a as [$x, $y]) {}",
        expect![[r#"
            SOURCE_FILE@0..33
              OPEN_TAG@0..6 "<?php "
              FOREACH_STMT@6..33
                FOREACH_KW@6..13 "foreach"
                L_PAREN@14..15 "("
                SIMPLE_VAR_EXPR@15..17
                  VARIABLE@15..17 "$a"
                AS_KW@18..20 "as"
                FOREACH_TARGET@21..29
                  ARRAY_EXPR@21..29
                    L_BRACK@21..22 "["
                    ARRAY_ELEMENT@22..24
                      SIMPLE_VAR_EXPR@22..24
                        VARIABLE@22..24 "$x"
                    COMMA@24..25 ","
                    ARRAY_ELEMENT@26..28
                      SIMPLE_VAR_EXPR@26..28
                        VARIABLE@26..28 "$y"
                    R_BRACK@28..29 "]"
                R_PAREN@29..30 ")"
                BLOCK@31..33
                  L_CURLY@31..32 "{"
                  R_CURLY@32..33 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// switch
// ---------------------------------------------------------------------------

#[test]
fn switch_case_semicolon_and_default() {
    // `case 1;` (the semicolon form) is legal, archaic and in the corpus;
    // its 8.5 deprecation is the compiler's to raise (design §6.1), and
    // the tree is the same.
    check(
        "<?php switch ($x) { case 1; echo 1; default: echo 2; }",
        expect![[r#"
            SOURCE_FILE@0..54
              OPEN_TAG@0..6 "<?php "
              SWITCH_STMT@6..54
                SWITCH_KW@6..12 "switch"
                L_PAREN@13..14 "("
                SIMPLE_VAR_EXPR@14..16
                  VARIABLE@14..16 "$x"
                R_PAREN@16..17 ")"
                L_CURLY@18..19 "{"
                SWITCH_ARM@20..35
                  CASE_KW@20..24 "case"
                  LITERAL_EXPR@25..26
                    INT@25..26 "1"
                  SEMICOLON@26..27 ";"
                  STMT_LIST@28..35
                    ECHO_STMT@28..35
                      ECHO_KW@28..32 "echo"
                      LITERAL_EXPR@33..34
                        INT@33..34 "1"
                      SEMICOLON@34..35 ";"
                SWITCH_ARM@36..52
                  DEFAULT_KW@36..43 "default"
                  COLON@43..44 ":"
                  STMT_LIST@45..52
                    ECHO_STMT@45..52
                      ECHO_KW@45..49 "echo"
                      LITERAL_EXPR@50..51
                        INT@50..51 "2"
                      SEMICOLON@51..52 ";"
                R_CURLY@53..54 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// break / continue / return / goto
// ---------------------------------------------------------------------------

#[test]
fn break_continue_levels() {
    // `break 2` outside two loops is php's compile error, `Cannot 'break'
    // 2 levels`, which is the compiler's to raise (design §6.1); the tree
    // is the same.
    check(
        "<?php while (1) { break; continue; break 2; continue 2; }",
        expect![[r#"
            SOURCE_FILE@0..57
              OPEN_TAG@0..6 "<?php "
              WHILE_STMT@6..57
                WHILE_KW@6..11 "while"
                L_PAREN@12..13 "("
                LITERAL_EXPR@13..14
                  INT@13..14 "1"
                R_PAREN@14..15 ")"
                BLOCK@16..57
                  L_CURLY@16..17 "{"
                  BREAK_STMT@18..24
                    BREAK_KW@18..23 "break"
                    SEMICOLON@23..24 ";"
                  CONTINUE_STMT@25..34
                    CONTINUE_KW@25..33 "continue"
                    SEMICOLON@33..34 ";"
                  BREAK_STMT@35..43
                    BREAK_KW@35..40 "break"
                    LITERAL_EXPR@41..42
                      INT@41..42 "2"
                    SEMICOLON@42..43 ";"
                  CONTINUE_STMT@44..55
                    CONTINUE_KW@44..52 "continue"
                    LITERAL_EXPR@53..54
                      INT@53..54 "2"
                    SEMICOLON@54..55 ";"
                  R_CURLY@56..57 "}"
        "#]],
    );
}

#[test]
fn return_with_and_without_value() {
    check(
        "<?php function f() { return 1; return; }",
        expect![[r#"
            SOURCE_FILE@0..40
              OPEN_TAG@0..6 "<?php "
              FUNCTION_DECL@6..40
                FUNCTION_KW@6..14 "function"
                IDENT@15..16 "f"
                PARAM_LIST@16..18
                  L_PAREN@16..17 "("
                  R_PAREN@17..18 ")"
                BLOCK@19..40
                  L_CURLY@19..20 "{"
                  RETURN_STMT@21..30
                    RETURN_KW@21..27 "return"
                    LITERAL_EXPR@28..29
                      INT@28..29 "1"
                    SEMICOLON@29..30 ";"
                  RETURN_STMT@31..38
                    RETURN_KW@31..37 "return"
                    SEMICOLON@37..38 ";"
                  R_CURLY@39..40 "}"
        "#]],
    );
}

#[test]
fn goto_and_label() {
    check(
        "<?php goto end; end: echo 1;",
        expect![[r#"
            SOURCE_FILE@0..28
              OPEN_TAG@0..6 "<?php "
              GOTO_STMT@6..15
                GOTO_KW@6..10 "goto"
                IDENT@11..14 "end"
                SEMICOLON@14..15 ";"
              LABEL_STMT@16..20
                IDENT@16..19 "end"
                COLON@19..20 ":"
              ECHO_STMT@21..28
                ECHO_KW@21..25 "echo"
                LITERAL_EXPR@26..27
                  INT@26..27 "1"
                SEMICOLON@27..28 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// try / catch / finally
// ---------------------------------------------------------------------------

#[test]
fn try_catch_union_finally() {
    check(
        "<?php try {} catch (A|B $e) {} finally {}",
        expect![[r#"
            SOURCE_FILE@0..41
              OPEN_TAG@0..6 "<?php "
              TRY_STMT@6..41
                TRY_KW@6..9 "try"
                BLOCK@10..12
                  L_CURLY@10..11 "{"
                  R_CURLY@11..12 "}"
                CATCH_CLAUSE@13..30
                  CATCH_KW@13..18 "catch"
                  L_PAREN@19..20 "("
                  CATCH_TYPE_LIST@20..23
                    IDENT@20..21 "A"
                    PIPE@21..22 "|"
                    IDENT@22..23 "B"
                  VARIABLE@24..26 "$e"
                  R_PAREN@26..27 ")"
                  BLOCK@28..30
                    L_CURLY@28..29 "{"
                    R_CURLY@29..30 "}"
                FINALLY_CLAUSE@31..41
                  FINALLY_KW@31..38 "finally"
                  BLOCK@39..41
                    L_CURLY@39..40 "{"
                    R_CURLY@40..41 "}"
        "#]],
    );
}

#[test]
fn try_catch_captureless() {
    check(
        "<?php try {} catch (E) {}",
        expect![[r#"
            SOURCE_FILE@0..25
              OPEN_TAG@0..6 "<?php "
              TRY_STMT@6..25
                TRY_KW@6..9 "try"
                BLOCK@10..12
                  L_CURLY@10..11 "{"
                  R_CURLY@11..12 "}"
                CATCH_CLAUSE@13..25
                  CATCH_KW@13..18 "catch"
                  L_PAREN@19..20 "("
                  CATCH_TYPE_LIST@20..21
                    IDENT@20..21 "E"
                  R_PAREN@21..22 ")"
                  BLOCK@23..25
                    L_CURLY@23..24 "{"
                    R_CURLY@24..25 "}"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// echo and tags
// ---------------------------------------------------------------------------

#[test]
fn echo_multi_expr() {
    check(
        "<?php echo 1, $x, 'a';",
        expect![[r#"
            SOURCE_FILE@0..22
              OPEN_TAG@0..6 "<?php "
              ECHO_STMT@6..22
                ECHO_KW@6..10 "echo"
                LITERAL_EXPR@11..12
                  INT@11..12 "1"
                COMMA@12..13 ","
                SIMPLE_VAR_EXPR@14..16
                  VARIABLE@14..16 "$x"
                COMMA@16..17 ","
                LITERAL_EXPR@18..21
                  SINGLE_QUOTED@18..21 "'a'"
                SEMICOLON@21..22 ";"
        "#]],
    );
}

#[test]
fn open_tag_echo() {
    check(
        "<?= $x ?>",
        expect![[r#"
            SOURCE_FILE@0..9
              ECHO_STMT@0..6
                OPEN_TAG_ECHO@0..3 "<?="
                SIMPLE_VAR_EXPR@4..6
                  VARIABLE@4..6 "$x"
              CLOSE_TAG@7..9 "?>"
        "#]],
    );
}

#[test]
fn inline_html_interleaved_alt_if() {
    check(
        "<?php if (1): ?>html<?php endif; ?>",
        expect![[r#"
            SOURCE_FILE@0..35
              OPEN_TAG@0..6 "<?php "
              IF_STMT@6..32
                IF_KW@6..8 "if"
                L_PAREN@9..10 "("
                LITERAL_EXPR@10..11
                  INT@10..11 "1"
                R_PAREN@11..12 ")"
                COLON@12..13 ":"
                STMT_LIST@14..26
                  CLOSE_TAG@14..16 "?>"
                  INLINE_HTML_STMT@16..20
                    INLINE_HTML@16..20 "html"
                  OPEN_TAG@20..26 "<?php "
                ENDIF_KW@26..31 "endif"
                SEMICOLON@31..32 ";"
              CLOSE_TAG@33..35 "?>"
        "#]],
    );
}

#[test]
fn leading_inline_html() {
    check(
        "before<?php echo 1;",
        expect![[r#"
            SOURCE_FILE@0..19
              INLINE_HTML_STMT@0..6
                INLINE_HTML@0..6 "before"
              OPEN_TAG@6..12 "<?php "
              ECHO_STMT@12..19
                ECHO_KW@12..16 "echo"
                LITERAL_EXPR@17..18
                  INT@17..18 "1"
                SEMICOLON@18..19 ";"
        "#]],
    );
}

#[test]
fn close_tag_acts_as_semicolon() {
    check(
        "<?php echo 1 ?>after",
        expect![[r#"
            SOURCE_FILE@0..20
              OPEN_TAG@0..6 "<?php "
              ECHO_STMT@6..12
                ECHO_KW@6..10 "echo"
                LITERAL_EXPR@11..12
                  INT@11..12 "1"
              CLOSE_TAG@13..15 "?>"
              INLINE_HTML_STMT@15..20
                INLINE_HTML@15..20 "after"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// global / static / unset / const
// ---------------------------------------------------------------------------

#[test]
fn global_stmt() {
    check(
        "<?php global $a, $b;",
        expect![[r#"
            SOURCE_FILE@0..20
              OPEN_TAG@0..6 "<?php "
              GLOBAL_STMT@6..20
                GLOBAL_KW@6..12 "global"
                SIMPLE_VAR_EXPR@13..15
                  VARIABLE@13..15 "$a"
                COMMA@15..16 ","
                SIMPLE_VAR_EXPR@17..19
                  VARIABLE@17..19 "$b"
                SEMICOLON@19..20 ";"
        "#]],
    );
}

#[test]
fn static_var_stmt_with_init() {
    check(
        "<?php static $x = 1, $y;",
        expect![[r#"
            SOURCE_FILE@0..24
              OPEN_TAG@0..6 "<?php "
              STATIC_VAR_STMT@6..24
                STATIC_KW@6..12 "static"
                STATIC_VAR_DECLARATOR@13..19
                  VARIABLE@13..15 "$x"
                  EQ@16..17 "="
                  LITERAL_EXPR@18..19
                    INT@18..19 "1"
                COMMA@19..20 ","
                STATIC_VAR_DECLARATOR@21..23
                  VARIABLE@21..23 "$y"
                SEMICOLON@23..24 ";"
        "#]],
    );
}

#[test]
fn unset_stmt() {
    check(
        "<?php unset($a, $b[0]);",
        expect![[r#"
            SOURCE_FILE@0..23
              OPEN_TAG@0..6 "<?php "
              UNSET_STMT@6..23
                UNSET_KW@6..11 "unset"
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

/// php's `unset_variables` takes one operand at least, so `unset()` is
/// its parse error and not a statement that reads.
#[test]
fn unset_stmt_with_no_operand() {
    check(
        "<?php unset();",
        expect![[r#"
        SOURCE_FILE@0..14
          OPEN_TAG@0..6 "<?php "
          UNSET_STMT@6..14
            UNSET_KW@6..11 "unset"
            L_PAREN@11..12 "("
            R_PAREN@12..13 ")"
            SEMICOLON@13..14 ";"
        error@12..13 syntax error, unexpected token ")"
    "#]],
    );
}

#[test]
fn const_stmt_multi_declarator() {
    check(
        "<?php const A = 1, B = 2;",
        expect![[r#"
            SOURCE_FILE@0..25
              OPEN_TAG@0..6 "<?php "
              CONST_STMT@6..25
                CONST_KW@6..11 "const"
                CONST_DECLARATOR@12..17
                  MEMBER_NAME@12..13
                    IDENT@12..13 "A"
                  EQ@14..15 "="
                  LITERAL_EXPR@16..17
                    INT@16..17 "1"
                COMMA@17..18 ","
                CONST_DECLARATOR@19..24
                  MEMBER_NAME@19..20
                    IDENT@19..20 "B"
                  EQ@21..22 "="
                  LITERAL_EXPR@23..24
                    INT@23..24 "2"
                SEMICOLON@24..25 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// halt_compiler, empty statements, blocks
// ---------------------------------------------------------------------------

#[test]
fn halt_compiler_with_trailing_data() {
    check(
        "<?php __halt_compiler(); binary )(",
        expect![[r#"
        SOURCE_FILE@0..34
          OPEN_TAG@0..6 "<?php "
          HALT_COMPILER_STMT@6..34
            HALT_COMPILER_KW@6..21 "__halt_compiler"
            L_PAREN@21..22 "("
            R_PAREN@22..23 ")"
            SEMICOLON@23..24 ";"
            HALT_DATA@24..34 " binary )("
    "#]],
    );
}

#[test]
fn halt_compiler_with_the_close_tag_as_its_semicolon() {
    // Zend returns `?>` as `';'`, so the close tag is the halt statement's
    // terminator and the trailer begins after the newline it swallowed
    // (probed 8.5.9: `__COMPILER_HALT_OFFSET__` is that byte).
    check(
        "<?php __halt_compiler() ?>\nrest",
        expect![[r#"
        SOURCE_FILE@0..31
          OPEN_TAG@0..6 "<?php "
          HALT_COMPILER_STMT@6..31
            HALT_COMPILER_KW@6..21 "__halt_compiler"
            L_PAREN@21..22 "("
            R_PAREN@22..23 ")"
            CLOSE_TAG@24..27 "?>\n"
            HALT_DATA@27..31 "rest"
    "#]],
    );
    check(
        "<?php __halt_compiler() ?>",
        expect![[r#"
        SOURCE_FILE@0..26
          OPEN_TAG@0..6 "<?php "
          HALT_COMPILER_STMT@6..26
            HALT_COMPILER_KW@6..21 "__halt_compiler"
            L_PAREN@21..22 "("
            R_PAREN@22..23 ")"
            CLOSE_TAG@24..26 "?>"
    "#]],
    );
}

#[test]
fn empty_statements() {
    check(
        "<?php ;;",
        expect![[r#"
        SOURCE_FILE@0..8
          OPEN_TAG@0..6 "<?php "
          EMPTY_STMT@6..7
            SEMICOLON@6..7 ";"
          EMPTY_STMT@7..8
            SEMICOLON@7..8 ";"
    "#]],
    );
}

#[test]
fn nested_blocks() {
    check(
        "<?php { { echo 1; } }",
        expect![[r#"
            SOURCE_FILE@0..21
              OPEN_TAG@0..6 "<?php "
              BLOCK@6..21
                L_CURLY@6..7 "{"
                BLOCK@8..19
                  L_CURLY@8..9 "{"
                  ECHO_STMT@10..17
                    ECHO_KW@10..14 "echo"
                    LITERAL_EXPR@15..16
                      INT@15..16 "1"
                    SEMICOLON@16..17 ";"
                  R_CURLY@18..19 "}"
                R_CURLY@20..21 "}"
        "#]],
    );
}
