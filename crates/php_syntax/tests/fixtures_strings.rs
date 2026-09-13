//! String fixtures (design §4.9): single-quoted, double-quoted with every
//! interpolation form, heredoc (plain, interpolating, flexible
//! indentation), nowdoc, backquote shell-exec, the `b"…"` prefixes and
//! the no-interpolation edges. Read at Mahout `1b3104a8` and regenerated
//! once against the arena's dump.

mod support;

use expect_test::expect;
use support::check;

// ---------------------------------------------------------------------------
// Single-quoted
// ---------------------------------------------------------------------------

#[test]
fn single_quoted_with_escapes() {
    check(
        r"<?php 'a\'b\\c';",
        expect![[r#"
        SOURCE_FILE@0..16
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..16
            LITERAL_EXPR@6..15
              SINGLE_QUOTED@6..15 "'a\\'b\\\\c'"
            SEMICOLON@15..16 ";"
    "#]],
    );
}

// ---------------------------------------------------------------------------
// Double-quoted interpolation: the five forms
// ---------------------------------------------------------------------------

#[test]
fn interp_simple_var() {
    check(
        r#"<?php "a $v b";"#,
        expect![[r#"
        SOURCE_FILE@0..15
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..15
            INTERPOLATED_STRING_EXPR@6..14
              DOUBLE_QUOTE@6..7 "\""
              LITERAL_STRING_PART@7..9
                STRING_TEXT@7..9 "a "
              SIMPLE_INTERPOLATION@9..11
                VARIABLE@9..11 "$v"
              LITERAL_STRING_PART@11..13
                STRING_TEXT@11..13 " b"
              DOUBLE_QUOTE@13..14 "\""
            SEMICOLON@14..15 ";"
    "#]],
    );
}

#[test]
fn interp_var_dim_int() {
    check(
        r#"<?php "$v[0]";"#,
        expect![[r#"
        SOURCE_FILE@0..14
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..14
            INTERPOLATED_STRING_EXPR@6..13
              DOUBLE_QUOTE@6..7 "\""
              SIMPLE_INTERPOLATION@7..12
                VARIABLE@7..9 "$v"
                SIMPLE_DIM@9..12
                  L_BRACK@9..10 "["
                  ENCAPS_INT@10..11 "0"
                  R_BRACK@11..12 "]"
              DOUBLE_QUOTE@12..13 "\""
            SEMICOLON@13..14 ";"
    "#]],
    );
}

#[test]
fn interp_var_dim_bareword() {
    // php -r verified: `"$arr[k]"` — bare-word key, no quotes, in-string only.
    check(
        r#"<?php "$v[key]";"#,
        expect![[r#"
        SOURCE_FILE@0..16
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..16
            INTERPOLATED_STRING_EXPR@6..15
              DOUBLE_QUOTE@6..7 "\""
              SIMPLE_INTERPOLATION@7..14
                VARIABLE@7..9 "$v"
                SIMPLE_DIM@9..14
                  L_BRACK@9..10 "["
                  IDENT@10..13 "key"
                  R_BRACK@13..14 "]"
              DOUBLE_QUOTE@14..15 "\""
            SEMICOLON@15..16 ";"
    "#]],
    );
}

#[test]
fn interp_var_dim_variable() {
    check(
        r#"<?php "$v[$i]";"#,
        expect![[r#"
        SOURCE_FILE@0..15
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..15
            INTERPOLATED_STRING_EXPR@6..14
              DOUBLE_QUOTE@6..7 "\""
              SIMPLE_INTERPOLATION@7..13
                VARIABLE@7..9 "$v"
                SIMPLE_DIM@9..13
                  L_BRACK@9..10 "["
                  VARIABLE@10..12 "$i"
                  R_BRACK@12..13 "]"
              DOUBLE_QUOTE@13..14 "\""
            SEMICOLON@14..15 ";"
    "#]],
    );
}

#[test]
fn interp_var_dim_negative() {
    // php -l verified: `"$a[-1]"` negative in-string offset.
    check(
        r#"<?php "$v[-1]";"#,
        expect![[r#"
        SOURCE_FILE@0..15
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..15
            INTERPOLATED_STRING_EXPR@6..14
              DOUBLE_QUOTE@6..7 "\""
              SIMPLE_INTERPOLATION@7..13
                VARIABLE@7..9 "$v"
                SIMPLE_DIM@9..13
                  L_BRACK@9..10 "["
                  MINUS@10..11 "-"
                  ENCAPS_INT@11..12 "1"
                  R_BRACK@12..13 "]"
              DOUBLE_QUOTE@13..14 "\""
            SEMICOLON@14..15 ";"
    "#]],
    );
}

#[test]
fn interp_var_prop() {
    check(
        r#"<?php "$v->prop";"#,
        expect![[r#"
        SOURCE_FILE@0..17
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..17
            INTERPOLATED_STRING_EXPR@6..16
              DOUBLE_QUOTE@6..7 "\""
              SIMPLE_INTERPOLATION@7..15
                VARIABLE@7..9 "$v"
                SIMPLE_PROP@9..15
                  ARROW@9..11 "->"
                  IDENT@11..15 "prop"
              DOUBLE_QUOTE@15..16 "\""
            SEMICOLON@16..17 ";"
    "#]],
    );
}

#[test]
fn interp_braced_expr() {
    check(
        r#"<?php "{$expr}";"#,
        expect![[r#"
        SOURCE_FILE@0..16
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..16
            INTERPOLATED_STRING_EXPR@6..15
              DOUBLE_QUOTE@6..7 "\""
              BRACED_INTERPOLATION@7..14
                L_CURLY@7..8 "{"
                SIMPLE_VAR_EXPR@8..13
                  VARIABLE@8..13 "$expr"
                R_CURLY@13..14 "}"
              DOUBLE_QUOTE@14..15 "\""
            SEMICOLON@15..16 ";"
    "#]],
    );
}

#[test]
fn interp_braced_subscript() {
    check(
        r#"<?php "{$a['k']}";"#,
        expect![[r#"
        SOURCE_FILE@0..18
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..18
            INTERPOLATED_STRING_EXPR@6..17
              DOUBLE_QUOTE@6..7 "\""
              BRACED_INTERPOLATION@7..16
                L_CURLY@7..8 "{"
                SUBSCRIPT_EXPR@8..15
                  SIMPLE_VAR_EXPR@8..10
                    VARIABLE@8..10 "$a"
                  L_BRACK@10..11 "["
                  LITERAL_EXPR@11..14
                    SINGLE_QUOTED@11..14 "'k'"
                  R_BRACK@14..15 "]"
                R_CURLY@15..16 "}"
              DOUBLE_QUOTE@16..17 "\""
            SEMICOLON@17..18 ";"
    "#]],
    );
}

#[test]
fn interp_dollar_brace_name() {
    check(
        r#"<?php "${name}";"#,
        expect![[r#"
            SOURCE_FILE@0..16
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..16
                INTERPOLATED_STRING_EXPR@6..15
                  DOUBLE_QUOTE@6..7 "\""
                  DOLLAR_BRACE_INTERPOLATION@7..14
                    DOLLAR_L_CURLY@7..9 "${"
                    IDENT@9..13 "name"
                    R_CURLY@13..14 "}"
                  DOUBLE_QUOTE@14..15 "\""
                SEMICOLON@15..16 ";"
        "#]],
    );
}

// `"${name[0]}"` is legal and deprecated in php (8.2); the offset arrives
// as a scripting-mode INT, which `simple_dim` takes beside ENCAPS_INT.
#[test]
fn interp_dollar_brace_name_dim() {
    check(
        r#"<?php "${name[0]}";"#,
        expect![[r#"
            SOURCE_FILE@0..19
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..19
                INTERPOLATED_STRING_EXPR@6..18
                  DOUBLE_QUOTE@6..7 "\""
                  DOLLAR_BRACE_INTERPOLATION@7..17
                    DOLLAR_L_CURLY@7..9 "${"
                    IDENT@9..13 "name"
                    SIMPLE_DIM@13..16
                      L_BRACK@13..14 "["
                      INT@14..15 "0"
                      R_BRACK@15..16 "]"
                    R_CURLY@16..17 "}"
                  DOUBLE_QUOTE@17..18 "\""
                SEMICOLON@18..19 ";"
        "#]],
    );
}

#[test]
fn interp_dollar_brace_expr() {
    check(
        r#"<?php "${expr()}";"#,
        expect![[r#"
            SOURCE_FILE@0..18
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..18
                INTERPOLATED_STRING_EXPR@6..17
                  DOUBLE_QUOTE@6..7 "\""
                  DOLLAR_BRACE_INTERPOLATION@7..16
                    DOLLAR_L_CURLY@7..9 "${"
                    CALL_EXPR@9..15
                      IDENT@9..13 "expr"
                      ARG_LIST@13..15
                        L_PAREN@13..14 "("
                        R_PAREN@14..15 ")"
                    R_CURLY@15..16 "}"
                  DOUBLE_QUOTE@16..17 "\""
                SEMICOLON@17..18 ";"
        "#]],
    );
}

#[test]
fn interp_adjacent_forms() {
    // php -r verified: "1CD" — three adjacent forms, no separators.
    check(
        r#"<?php "$a[0]$b->c{$d}";"#,
        expect![[r#"
        SOURCE_FILE@0..23
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..23
            INTERPOLATED_STRING_EXPR@6..22
              DOUBLE_QUOTE@6..7 "\""
              SIMPLE_INTERPOLATION@7..12
                VARIABLE@7..9 "$a"
                SIMPLE_DIM@9..12
                  L_BRACK@9..10 "["
                  ENCAPS_INT@10..11 "0"
                  R_BRACK@11..12 "]"
              SIMPLE_INTERPOLATION@12..17
                VARIABLE@12..14 "$b"
                SIMPLE_PROP@14..17
                  ARROW@14..16 "->"
                  IDENT@16..17 "c"
              BRACED_INTERPOLATION@17..21
                L_CURLY@17..18 "{"
                SIMPLE_VAR_EXPR@18..20
                  VARIABLE@18..20 "$d"
                R_CURLY@20..21 "}"
              DOUBLE_QUOTE@21..22 "\""
            SEMICOLON@22..23 ";"
    "#]],
    );
}

// ---------------------------------------------------------------------------
// No-interpolation edges
// ---------------------------------------------------------------------------

#[test]
fn escaped_dollar_no_interpolation() {
    check(
        r#"<?php "\$not";"#,
        expect![[r#"
        SOURCE_FILE@0..14
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..14
            INTERPOLATED_STRING_EXPR@6..13
              DOUBLE_QUOTE@6..7 "\""
              LITERAL_STRING_PART@7..12
                STRING_TEXT@7..12 "\\$not"
              DOUBLE_QUOTE@12..13 "\""
            SEMICOLON@13..14 ";"
    "#]],
    );
}

#[test]
fn brace_space_dollar_still_interpolates_var() {
    // php -r verified: "{ $a }" => "{ 5 }" — the brace is literal text,
    // the `$a` still interpolates (only `{$` opens a braced form).
    check(
        r#"<?php "{ $a }";"#,
        expect![[r#"
        SOURCE_FILE@0..15
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..15
            INTERPOLATED_STRING_EXPR@6..14
              DOUBLE_QUOTE@6..7 "\""
              LITERAL_STRING_PART@7..9
                STRING_TEXT@7..9 "{ "
              SIMPLE_INTERPOLATION@9..11
                VARIABLE@9..11 "$a"
              LITERAL_STRING_PART@11..13
                STRING_TEXT@11..13 " }"
              DOUBLE_QUOTE@13..14 "\""
            SEMICOLON@14..15 ";"
    "#]],
    );
}

// ---------------------------------------------------------------------------
// Heredoc / nowdoc
// ---------------------------------------------------------------------------

#[test]
fn heredoc_plain() {
    check(
        "<?php $x = <<<EOT\nplain\nEOT;",
        expect![[r#"
            SOURCE_FILE@0..28
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..28
                ASSIGN_EXPR@6..27
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$x"
                  EQ@9..10 "="
                  HEREDOC_EXPR@11..27
                    HEREDOC_START@11..18 "<<<EOT\n"
                    LITERAL_STRING_PART@18..24
                      STRING_TEXT@18..24 "plain\n"
                    HEREDOC_END@24..27 "EOT"
                SEMICOLON@27..28 ";"
        "#]],
    );
}

#[test]
fn heredoc_interpolating() {
    check(
        "<?php $x = <<<EOT\na $v b {$w}\nEOT;",
        expect![[r#"
            SOURCE_FILE@0..34
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..34
                ASSIGN_EXPR@6..33
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$x"
                  EQ@9..10 "="
                  HEREDOC_EXPR@11..33
                    HEREDOC_START@11..18 "<<<EOT\n"
                    LITERAL_STRING_PART@18..20
                      STRING_TEXT@18..20 "a "
                    SIMPLE_INTERPOLATION@20..22
                      VARIABLE@20..22 "$v"
                    LITERAL_STRING_PART@22..25
                      STRING_TEXT@22..25 " b "
                    BRACED_INTERPOLATION@25..29
                      L_CURLY@25..26 "{"
                      SIMPLE_VAR_EXPR@26..28
                        VARIABLE@26..28 "$w"
                      R_CURLY@28..29 "}"
                    LITERAL_STRING_PART@29..30
                      STRING_TEXT@29..30 "\n"
                    HEREDOC_END@30..33 "EOT"
                SEMICOLON@33..34 ";"
        "#]],
    );
}

#[test]
fn heredoc_flexible_indentation() {
    // 7.3 flexible rules: indented closing marker, indentation stripped
    // as trivia structure (G2-lossless).
    check(
        "<?php $x = <<<EOT\n    a\n    b\n    EOT;",
        expect![[r#"
            SOURCE_FILE@0..38
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..38
                ASSIGN_EXPR@6..37
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$x"
                  EQ@9..10 "="
                  HEREDOC_EXPR@11..37
                    HEREDOC_START@11..18 "<<<EOT\n"
                    LITERAL_STRING_PART@22..24
                      STRING_TEXT@22..24 "a\n"
                    LITERAL_STRING_PART@28..30
                      STRING_TEXT@28..30 "b\n"
                    HEREDOC_END@34..37 "EOT"
                SEMICOLON@37..38 ";"
        "#]],
    );
}

#[test]
fn nowdoc() {
    check(
        "<?php $x = <<<'EOT'\nno $interp\nEOT;",
        expect![[r#"
            SOURCE_FILE@0..35
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..35
                ASSIGN_EXPR@6..34
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$x"
                  EQ@9..10 "="
                  NOWDOC_EXPR@11..34
                    HEREDOC_START@11..20 "<<<'EOT'\n"
                    STRING_TEXT@20..31 "no $interp\n"
                    HEREDOC_END@31..34 "EOT"
                SEMICOLON@34..35 ";"
        "#]],
    );
}

#[test]
fn nowdoc_indented_closer() {
    check(
        "<?php $x = <<<'EOT'\n  text\n  EOT;",
        expect![[r#"
            SOURCE_FILE@0..33
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..33
                ASSIGN_EXPR@6..32
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$x"
                  EQ@9..10 "="
                  NOWDOC_EXPR@11..32
                    HEREDOC_START@11..20 "<<<'EOT'\n"
                    STRING_TEXT@22..27 "text\n"
                    HEREDOC_END@29..32 "EOT"
                SEMICOLON@32..33 ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Backquote and b-prefix
// ---------------------------------------------------------------------------

#[test]
fn backquote_shell_exec_with_interpolation() {
    check(
        "<?php `ls $dir`;",
        expect![[r#"
            SOURCE_FILE@0..16
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..16
                SHELL_EXEC_EXPR@6..15
                  BACKQUOTE@6..7 "`"
                  LITERAL_STRING_PART@7..10
                    STRING_TEXT@7..10 "ls "
                  SIMPLE_INTERPOLATION@10..14
                    VARIABLE@10..14 "$dir"
                  BACKQUOTE@14..15 "`"
                SEMICOLON@15..16 ";"
        "#]],
    );
}

#[test]
fn b_prefix_double_quoted() {
    check(
        r#"<?php b"bytes $x";"#,
        expect![[r#"
        SOURCE_FILE@0..18
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..18
            INTERPOLATED_STRING_EXPR@6..17
              DOUBLE_QUOTE@6..8 "b\""
              LITERAL_STRING_PART@8..14
                STRING_TEXT@8..14 "bytes "
              SIMPLE_INTERPOLATION@14..16
                VARIABLE@14..16 "$x"
              DOUBLE_QUOTE@16..17 "\""
            SEMICOLON@17..18 ";"
    "#]],
    );
}

#[test]
fn b_prefix_single_quoted() {
    check(
        "<?php b'bytes';",
        expect![[r#"
        SOURCE_FILE@0..15
          OPEN_TAG@0..6 "<?php "
          EXPR_STMT@6..15
            LITERAL_EXPR@6..14
              SINGLE_QUOTED@6..14 "b'bytes'"
            SEMICOLON@14..15 ";"
    "#]],
    );
}
