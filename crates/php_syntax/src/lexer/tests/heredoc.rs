//! Heredoc and nowdoc fixtures: flexible indentation, nowdoc, and labels
//! that continue with label characters.

use super::*;

#[test]
fn basic_heredoc() {
    // T_START_HEREDOC's extent: `<<<LABEL` through the newline inclusive.
    check_php(
        "<<<EOT\nhello\nEOT;",
        &[
            (HEREDOC_START, "<<<EOT\n"),
            (STRING_TEXT, "hello\n"),
            (HEREDOC_END, "EOT"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn heredoc_intro_tolerates_spaces_and_quotes() {
    check_php(
        "<<< \t EOT\nx\nEOT;",
        &[
            (HEREDOC_START, "<<< \t EOT\n"),
            (STRING_TEXT, "x\n"),
            (HEREDOC_END, "EOT"),
            (SEMICOLON, ";"),
        ],
    );
    // Double-quoted label: still an interpolating heredoc.
    check_php(
        "<<<\"EOT\"\n$x\nEOT;",
        &[
            (HEREDOC_START, "<<<\"EOT\"\n"),
            (VARIABLE, "$x"),
            (STRING_TEXT, "\n"),
            (HEREDOC_END, "EOT"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn heredoc_b_prefix() {
    check_php(
        "b<<<EOT\nx\nEOT;",
        &[
            (HEREDOC_START, "b<<<EOT\n"),
            (STRING_TEXT, "x\n"),
            (HEREDOC_END, "EOT"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn nowdoc_never_interpolates() {
    check_php(
        "<<<'EOT'\na $x {$y}\nEOT;",
        &[
            (HEREDOC_START, "<<<'EOT'\n"),
            (STRING_TEXT, "a $x {$y}\n"),
            (HEREDOC_END, "EOT"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn nowdoc_unindented_body_is_one_text_run() {
    check_php(
        "<<<'EOT'\nline1\nline2\n\nline4\nEOT;",
        &[
            (HEREDOC_START, "<<<'EOT'\n"),
            (STRING_TEXT, "line1\nline2\n\nline4\n"),
            (HEREDOC_END, "EOT"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn heredoc_interpolation_forms() {
    check_php(
        "<<<EOT\na $x b $a[0] c $o->p {$e}\nEOT;",
        &[
            (HEREDOC_START, "<<<EOT\n"),
            (STRING_TEXT, "a "),
            (VARIABLE, "$x"),
            (STRING_TEXT, " b "),
            (VARIABLE, "$a"),
            (L_BRACK, "["),
            (ENCAPS_INT, "0"),
            (R_BRACK, "]"),
            (STRING_TEXT, " c "),
            (VARIABLE, "$o"),
            (ARROW, "->"),
            (IDENT, "p"),
            (STRING_TEXT, " "),
            (L_CURLY, "{"),
            (VARIABLE, "$e"),
            (R_CURLY, "}"),
            (STRING_TEXT, "\n"),
            (HEREDOC_END, "EOT"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn flexible_indentation_strips_per_line() {
    // Closing marker indented by two: each body line's first two bytes are
    // a HEREDOC_INDENT token, so the strip is in the stream (design §4.9).
    check_php(
        "<<<EOT\n    a\n  b\n  EOT;",
        &[
            (HEREDOC_START, "<<<EOT\n"),
            (HEREDOC_INDENT, "  "),
            (STRING_TEXT, "  a\n"),
            (HEREDOC_INDENT, "  "),
            (STRING_TEXT, "b\n"),
            (HEREDOC_INDENT, "  "),
            (HEREDOC_END, "EOT"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn flexible_indentation_empty_lines_exempt() {
    check_php(
        "<<<EOT\n  a\n\n  b\n  EOT;",
        &[
            (HEREDOC_START, "<<<EOT\n"),
            (HEREDOC_INDENT, "  "),
            (STRING_TEXT, "a\n"),
            (STRING_TEXT, "\n"),
            (HEREDOC_INDENT, "  "),
            (STRING_TEXT, "b\n"),
            (HEREDOC_INDENT, "  "),
            (HEREDOC_END, "EOT"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn flexible_indentation_with_interpolation() {
    check_php(
        "<<<EOT\n  a $x\n  EOT;",
        &[
            (HEREDOC_START, "<<<EOT\n"),
            (HEREDOC_INDENT, "  "),
            (STRING_TEXT, "a "),
            (VARIABLE, "$x"),
            (STRING_TEXT, "\n"),
            (HEREDOC_INDENT, "  "),
            (HEREDOC_END, "EOT"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn label_followed_by_label_chars_does_not_terminate() {
    // Body line `ENDING` does not close label `END` (the `I` is a label
    // char); `ENDS`-style prefixes are content.
    check_php(
        "<<<END\nENDING\nEND;",
        &[
            (HEREDOC_START, "<<<END\n"),
            (STRING_TEXT, "ENDING\n"),
            (HEREDOC_END, "END"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn terminator_label_is_case_sensitive() {
    check_php(
        "<<<END\nend\nEND;",
        &[
            (HEREDOC_START, "<<<END\n"),
            (STRING_TEXT, "end\n"),
            (HEREDOC_END, "END"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn terminator_followed_by_punctuation() {
    // 7.3 flexible rules: any non-label char may follow the closing label.
    check_php(
        "$a = [<<<E\nx\nE, 1];",
        &[
            (VARIABLE, "$a"),
            (WHITESPACE, " "),
            (EQ, "="),
            (WHITESPACE, " "),
            (L_BRACK, "["),
            (HEREDOC_START, "<<<E\n"),
            (STRING_TEXT, "x\n"),
            (HEREDOC_END, "E"),
            (COMMA, ","),
            (WHITESPACE, " "),
            (INT, "1"),
            (R_BRACK, "]"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn terminator_at_eof_without_newline() {
    check_php(
        "<<<E\nx\nE",
        &[
            (HEREDOC_START, "<<<E\n"),
            (STRING_TEXT, "x\n"),
            (HEREDOC_END, "E"),
        ],
    );
}

#[test]
fn empty_heredoc_body() {
    check_php(
        "<<<E\nE;",
        &[
            (HEREDOC_START, "<<<E\n"),
            (HEREDOC_END, "E"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn crlf_heredoc() {
    check_php(
        "<<<E\r\nx\r\nE;",
        &[
            (HEREDOC_START, "<<<E\r\n"),
            (STRING_TEXT, "x\r\n"),
            (HEREDOC_END, "E"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn close_tag_inside_heredoc_is_text() {
    check_php(
        "<<<E\n?>\nE;",
        &[
            (HEREDOC_START, "<<<E\n"),
            (STRING_TEXT, "?>\n"),
            (HEREDOC_END, "E"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn backslash_before_newline_keeps_line_accounting() {
    // Zend's ST_HEREDOC escape rule does not swallow newlines: the line
    // after `\` can still terminate the heredoc.
    check_php(
        "<<<E\nx\\\nE;",
        &[
            (HEREDOC_START, "<<<E\n"),
            (STRING_TEXT, "x\\\n"),
            (HEREDOC_END, "E"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn heredoc_intro_requires_newline() {
    // `<<<EOT` without a following newline is not a heredoc; operators win.
    check_php(
        "<<<EOT x",
        &[
            (SHL, "<<"),
            (LT, "<"),
            (IDENT, "EOT"),
            (WHITESPACE, " "),
            (IDENT, "x"),
        ],
    );
}

#[test]
fn flexible_indent_survives_dollar_curly_with_label_inside() {
    // A `${…}` interpolation that itself contains the heredoc's closing
    // label must not defeat indentation stripping. `${` is one opener:
    // counting it twice, once for `$` and once for the `{`, left the
    // textual brace depth above zero at the real closing marker, so
    // `ensure_heredoc_info` saw indent_len == 0 and emitted no
    // HEREDOC_INDENT (php-src flexible-heredoc-complex test1..4). The
    // leading "    " before `T` is the dedent.
    check_php(
        "$b = <<<FOO\n    T\n    ${\n     FOO\n    }\n    FOO;\n",
        &[
            (VARIABLE, "$b"),
            (WHITESPACE, " "),
            (EQ, "="),
            (WHITESPACE, " "),
            (HEREDOC_START, "<<<FOO\n"),
            (HEREDOC_INDENT, "    "),
            (STRING_TEXT, "T\n"),
            (HEREDOC_INDENT, "    "),
            (DOLLAR_L_CURLY, "${"),
            (WHITESPACE, "\n     "),
            (IDENT, "FOO"),
            (WHITESPACE, "\n    "),
            (R_CURLY, "}"),
            (STRING_TEXT, "\n"),
            (HEREDOC_INDENT, "    "),
            (HEREDOC_END, "FOO"),
            (SEMICOLON, ";"),
            (WHITESPACE, "\n"),
        ],
    );
}
