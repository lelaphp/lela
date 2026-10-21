//! Single- and double-quoted strings, backquote, and all five interpolation
//! forms: `b"…"` prefixes; `$var`, `$var[dim]`, `$var->prop`, `{$expr}`,
//! `${name}` and `${expr}`; `$arr[bare-string]` inside strings.

use super::*;

#[test]
fn single_quoted() {
    check_php("'abc'", &[(SINGLE_QUOTED, "'abc'")]);
    check_php("''", &[(SINGLE_QUOTED, "''")]);
    check_php(r"'a\'b'", &[(SINGLE_QUOTED, r"'a\'b'")]);
    check_php(r"'a\\'", &[(SINGLE_QUOTED, r"'a\\'")]);
    // `\q` does not escape the quote-terminator logic but `\` always
    // consumes the next byte (flex: ([^'\\]|\\[^])*).
    check_php(r"'\q'", &[(SINGLE_QUOTED, r"'\q'")]);
    check_php("'$x'", &[(SINGLE_QUOTED, "'$x'")]); // never interpolates
}

#[test]
fn single_quoted_b_prefix() {
    check_php("b'x'", &[(SINGLE_QUOTED, "b'x'")]);
    check_php("B'x'", &[(SINGLE_QUOTED, "B'x'")]);
}

#[test]
fn double_quoted_plain() {
    check_php(
        "\"abc\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (STRING_TEXT, "abc"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    check_php("\"\"", &[(DOUBLE_QUOTE, "\""), (DOUBLE_QUOTE, "\"")]);
}

#[test]
fn double_quoted_b_prefix_joins_opening_delimiter() {
    // The opening token's text is `b"`, the closing one's `"`.
    check_php(
        "b\"x\"",
        &[
            (DOUBLE_QUOTE, "b\""),
            (STRING_TEXT, "x"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
}

#[test]
fn double_quoted_escapes_stay_in_text() {
    check_php(
        r#""a\"b\n\t""#,
        &[
            (DOUBLE_QUOTE, "\""),
            (STRING_TEXT, r#"a\"b\n\t"#),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    // `\$` suppresses interpolation; `\\` does not suppress the `$` after it.
    check_php(
        r#""\$x""#,
        &[
            (DOUBLE_QUOTE, "\""),
            (STRING_TEXT, r"\$x"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    check_php(
        r#""\\$x""#,
        &[
            (DOUBLE_QUOTE, "\""),
            (STRING_TEXT, r"\\"),
            (VARIABLE, "$x"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
}

#[test]
fn simple_variable_interpolation() {
    check_php(
        "\"a $x b\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (STRING_TEXT, "a "),
            (VARIABLE, "$x"),
            (STRING_TEXT, " b"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
}

#[test]
fn lone_dollar_and_brace_are_text() {
    check_php(
        "\"$ {\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (STRING_TEXT, "$ {"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    check_php(
        "\"$\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (STRING_TEXT, "$"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    // `$$x`: first `$` is text, second interpolates.
    check_php(
        "\"$$x\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (STRING_TEXT, "$"),
            (VARIABLE, "$x"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    // `$1` is text: labels cannot start with a digit.
    check_php(
        "\"$1\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (STRING_TEXT, "$1"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
}

#[test]
fn var_offset_integer_dims() {
    check_php(
        "\"$a[0]\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (VARIABLE, "$a"),
            (L_BRACK, "["),
            (ENCAPS_INT, "0"),
            (R_BRACK, "]"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    // Negative index: `-` is its own token (php: φ), digits are T_NUM_STRING.
    check_php(
        "\"$a[-3]\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (VARIABLE, "$a"),
            (L_BRACK, "["),
            (MINUS, "-"),
            (ENCAPS_INT, "3"),
            (R_BRACK, "]"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    // Hex-shaped offsets are still T_NUM_STRING ("treated as a string").
    check_php(
        "\"$a[0x1F]\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (VARIABLE, "$a"),
            (L_BRACK, "["),
            (ENCAPS_INT, "0x1F"),
            (R_BRACK, "]"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
}

#[test]
fn var_offset_bare_word_and_variable() {
    // `$arr[bare-string]` inside strings.
    check_php(
        "\"$a[key]\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (VARIABLE, "$a"),
            (L_BRACK, "["),
            (IDENT, "key"),
            (R_BRACK, "]"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    check_php(
        "\"$a[$k]\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (VARIABLE, "$a"),
            (L_BRACK, "["),
            (VARIABLE, "$k"),
            (R_BRACK, "]"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    // After the closing `]`, interpolation does not chain: `[1]` is text.
    check_php(
        "\"$a[0][1]\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (VARIABLE, "$a"),
            (L_BRACK, "["),
            (ENCAPS_INT, "0"),
            (R_BRACK, "]"),
            (STRING_TEXT, "[1]"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
}

#[test]
fn var_offset_whitespace_falls_back_to_text() {
    // Zend's [ \n\r\t\\'#] rule pops ST_VAR_OFFSET with `yyless(0)` and
    // returns a zero-width T_ENCAPSED_AND_WHITESPACE at the byte; the
    // rest is text, and php's parse error on it is the parser's to say.
    check_php(
        "\"$a[ 0]\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (VARIABLE, "$a"),
            (L_BRACK, "["),
            (STRING_TEXT, ""),
            (STRING_TEXT, " 0]"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    // Each of the seven bytes pops the same way, in a heredoc and a
    // backquote as in `"…"`.
    for byte in [" ", "\n", "\r", "\t", "\\", "'", "#"] {
        check_php(
            &format!("\"$a[{byte}]\""),
            &[
                (DOUBLE_QUOTE, "\""),
                (VARIABLE, "$a"),
                (L_BRACK, "["),
                (STRING_TEXT, ""),
                (STRING_TEXT, &format!("{byte}]")),
                (DOUBLE_QUOTE, "\""),
            ],
        );
    }
    check_php(
        "`$a[ 0]`",
        &[
            (BACKQUOTE, "`"),
            (VARIABLE, "$a"),
            (L_BRACK, "["),
            (STRING_TEXT, ""),
            (STRING_TEXT, " 0]"),
            (BACKQUOTE, "`"),
        ],
    );
}

#[test]
fn var_offset_zero_width_token_stands_at_the_byte_it_pops_on() {
    // The empty token's offset is the popping byte's, and the text token
    // after it starts at the same offset: `yyless(0)` consumed nothing.
    let input = "<?php \"$a[ 0]\"";
    let lexed = lex(F, input.as_bytes(), &LexOptions::default());
    let empty: Vec<usize> = (0..lexed.tokens.len())
        .filter(|&i| lexed.tokens[i].len == 0)
        .collect();
    assert_eq!(empty, [4], "one zero-width token, after the `[`");
    let token = lexed.tokens[4];
    assert_eq!((token.kind, token.offset), (STRING_TEXT, 10));
    assert_eq!(&input.as_bytes()[10..11], b" ");
    let next = lexed.tokens[5];
    assert_eq!((next.kind, next.offset, next.len), (STRING_TEXT, 10, 3));
}

#[test]
fn simple_property_interpolation() {
    check_php(
        "\"$x->prop\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (VARIABLE, "$x"),
            (ARROW, "->"),
            (IDENT, "prop"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    // Keywords lex as IDENT in property position, inside strings too.
    check_php(
        "\"$x->for\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (VARIABLE, "$x"),
            (ARROW, "->"),
            (IDENT, "for"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    // Single hop only: the second `->` is text.
    check_php(
        "\"$x->a->b\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (VARIABLE, "$x"),
            (ARROW, "->"),
            (IDENT, "a"),
            (STRING_TEXT, "->b"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
}

#[test]
fn property_interpolation_needs_label_start() {
    // `$x->3` and `$x->` stay text; `$x?->p` interpolates, as it does in
    // php since 8.0, and `?->3` stays text like `->3`.
    check_php(
        "\"$x->3\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (VARIABLE, "$x"),
            (STRING_TEXT, "->3"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    check_php(
        "\"$x?->p\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (VARIABLE, "$x"),
            (NULLSAFE_ARROW, "?->"),
            (IDENT, "p"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    check_php(
        "\"$x?->3\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (VARIABLE, "$x"),
            (STRING_TEXT, "?->3"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
}

#[test]
fn braced_interpolation_is_scripting() {
    check_php(
        "\"{$x}\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (L_CURLY, "{"),
            (VARIABLE, "$x"),
            (R_CURLY, "}"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    check_php(
        "\"{$a['k']}\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (L_CURLY, "{"),
            (VARIABLE, "$a"),
            (L_BRACK, "["),
            (SINGLE_QUOTED, "'k'"),
            (R_BRACK, "]"),
            (R_CURLY, "}"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    // Nested braces: depth is stacked Scripting states.
    check_php(
        "\"{$a->{$b}}\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (L_CURLY, "{"),
            (VARIABLE, "$a"),
            (ARROW, "->"),
            (L_CURLY, "{"),
            (VARIABLE, "$b"),
            (R_CURLY, "}"),
            (R_CURLY, "}"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    // `{` not followed by `$` is text; `{{$` triggers on the second brace.
    check_php(
        "\"{{$x}}\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (STRING_TEXT, "{"),
            (L_CURLY, "{"),
            (VARIABLE, "$x"),
            (R_CURLY, "}"),
            (STRING_TEXT, "}"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
}

#[test]
fn dollar_brace_varname_form() {
    // `${name}`: IDENT is php's T_STRING_VARNAME; the 8.2 deprecation is
    // the compiler's to raise.
    check_php(
        "\"${x}\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (DOLLAR_L_CURLY, "${"),
            (IDENT, "x"),
            (R_CURLY, "}"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    // `${name[expr]}`: after the varname, scripting grammar until `}`.
    check_php(
        "\"${x[0]}\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (DOLLAR_L_CURLY, "${"),
            (IDENT, "x"),
            (L_BRACK, "["),
            (INT, "0"),
            (R_BRACK, "]"),
            (R_CURLY, "}"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
}

#[test]
fn dollar_brace_expr_form() {
    // `${expr}`: ST_LOOKING_FOR_VARNAME falls through to full scripting.
    check_php(
        "\"${$x}\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (DOLLAR_L_CURLY, "${"),
            (VARIABLE, "$x"),
            (R_CURLY, "}"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    check_php(
        "\"${'a' . 'b'}\"",
        &[
            (DOUBLE_QUOTE, "\""),
            (DOLLAR_L_CURLY, "${"),
            (SINGLE_QUOTED, "'a'"),
            (WHITESPACE, " "),
            (DOT, "."),
            (WHITESPACE, " "),
            (SINGLE_QUOTED, "'b'"),
            (R_CURLY, "}"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
}

#[test]
fn unicode_escapes_stay_in_text() {
    check_php(
        r#""\u{1F600}""#,
        &[
            (DOUBLE_QUOTE, "\""),
            (STRING_TEXT, r"\u{1F600}"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
    // `\u` without `{` is literal, no diagnostic.
    check_php(
        r#""\u 1""#,
        &[
            (DOUBLE_QUOTE, "\""),
            (STRING_TEXT, r"\u 1"),
            (DOUBLE_QUOTE, "\""),
        ],
    );
}

#[test]
fn backquote_interpolates_like_double_quotes() {
    check_php(
        "`ls $x`",
        &[
            (BACKQUOTE, "`"),
            (STRING_TEXT, "ls "),
            (VARIABLE, "$x"),
            (BACKQUOTE, "`"),
        ],
    );
    check_php(
        "`a{$b}c`",
        &[
            (BACKQUOTE, "`"),
            (STRING_TEXT, "a"),
            (L_CURLY, "{"),
            (VARIABLE, "$b"),
            (R_CURLY, "}"),
            (STRING_TEXT, "c"),
            (BACKQUOTE, "`"),
        ],
    );
    // A double quote inside backticks is plain text.
    check_php(
        "`a\"b`",
        &[(BACKQUOTE, "`"), (STRING_TEXT, "a\"b"), (BACKQUOTE, "`")],
    );
}
