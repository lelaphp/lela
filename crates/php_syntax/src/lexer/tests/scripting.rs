//! Scripting fixtures: keywords, names, magic constants, casts,
//! operators, comments, variables, the property-name remap, error bytes.

use super::*;

#[test]
fn keywords_case_insensitive() {
    check_php(
        "FUNCTION Foo",
        &[(FUNCTION_KW, "FUNCTION"), (WHITESPACE, " "), (IDENT, "Foo")],
    );
    check_php("MaTcH", &[(MATCH_KW, "MaTcH")]);
    check_php("exit", &[(EXIT_KW, "exit")]);
    check_php("DIE", &[(DIE_KW, "DIE")]);
}

#[test]
fn magic_constants_case_insensitive() {
    check_php("__LINE__", &[(MAGIC_CONST, "__LINE__")]);
    check_php("__file__", &[(MAGIC_CONST, "__file__")]);
    check_php("__Property__", &[(MAGIC_CONST, "__Property__")]);
    // Not magic: ordinary ident.
    check_php("__LINE", &[(IDENT, "__LINE")]);
}

#[test]
fn idents_with_high_bytes() {
    check_bytes(
        b"<?php \xC3\xA9t\xC3\xA9;",
        &[
            (OPEN_TAG, b"<?php "),
            (IDENT, b"\xC3\xA9t\xC3\xA9"),
            (SEMICOLON, b";"),
        ],
    );
}

#[test]
fn qualified_names_are_single_tokens() {
    check_php("A\\B", &[(NAME_QUALIFIED, "A\\B")]);
    check_php("\\A\\B", &[(NAME_FULLY_QUALIFIED, "\\A\\B")]);
    check_php("\\foo", &[(NAME_FULLY_QUALIFIED, "\\foo")]);
    check_php("namespace\\A", &[(NAME_RELATIVE, "namespace\\A")]);
    check_php("NAMESPACE\\a\\b", &[(NAME_RELATIVE, "NAMESPACE\\a\\b")]);
}

#[test]
fn qualified_name_scan_wins_over_keywords() {
    // php-src lexes {LABEL}("\\"{LABEL})+ by longest match, keyword-ness of
    // segments notwithstanding.
    check_php("list\\B", &[(NAME_QUALIFIED, "list\\B")]);
    check_php("A\\list", &[(NAME_QUALIFIED, "A\\list")]);
}

#[test]
fn trailing_backslash_is_not_part_of_a_name() {
    // T_NAME_QUALIFIED needs a label after the backslash.
    check_php(
        "A\\ ",
        &[(IDENT, "A"), (BACKSLASH, "\\"), (WHITESPACE, " ")],
    );
    check_php(
        "list\\ ",
        &[(LIST_KW, "list"), (BACKSLASH, "\\"), (WHITESPACE, " ")],
    );
    check_php("\\", &[(BACKSLASH, "\\")]);
}

#[test]
fn variables_and_dollars() {
    check_php("$x", &[(VARIABLE, "$x")]);
    check_php("$_9é", &[(VARIABLE, "$_9é")]);
    check_php("$$x", &[(DOLLAR, "$"), (VARIABLE, "$x")]);
    check_php("$ x", &[(DOLLAR, "$"), (WHITESPACE, " "), (IDENT, "x")]);
    check_php(
        "${x}",
        &[(DOLLAR, "$"), (L_CURLY, "{"), (IDENT, "x"), (R_CURLY, "}")],
    );
}

#[test]
fn operators_longest_match() {
    check_php("<=>", &[(SPACESHIP, "<=>")]);
    check_php("<<=", &[(SHL_EQ, "<<=")]);
    check_php("<<", &[(SHL, "<<")]);
    check_php("<=", &[(LT_EQ, "<=")]);
    check_php("<>", &[(LT_GT, "<>")]);
    check_php("?->", &[(NULLSAFE_ARROW, "?->")]);
    check_php("??=", &[(QUESTION_QUESTION_EQ, "??=")]);
    check_php("??", &[(QUESTION_QUESTION, "??")]);
    check_php("|>", &[(PIPE_GT, "|>")]);
    check_php("...", &[(DOT_DOT_DOT, "...")]);
    check_php(".=", &[(DOT_EQ, ".=")]);
    check_php("**=", &[(STAR_STAR_EQ, "**=")]);
    check_php("**", &[(STAR_STAR, "**")]);
    check_php(">>=", &[(SHR_EQ, ">>=")]);
    check_php(">>", &[(SHR, ">>")]);
    check_php("===", &[(EQ_EQ_EQ, "===")]);
    check_php("==", &[(EQ_EQ, "==")]);
    check_php("=>", &[(FAT_ARROW, "=>")]);
    check_php("!==", &[(BANG_EQ_EQ, "!==")]);
    check_php("!=", &[(BANG_EQ, "!=")]);
    check_php("::", &[(COLON_COLON, "::")]);
    check_php("++", &[(PLUS_PLUS, "++")]);
    check_php("--", &[(MINUS_MINUS, "--")]);
    check_php("@~", &[(AT, "@"), (TILDE, "~")]);
}

#[test]
fn failed_heredoc_intro_falls_back_to_shift() {
    check_php(
        "1 <<< 2",
        &[
            (INT, "1"),
            (WHITESPACE, " "),
            (SHL, "<<"),
            (LT, "<"),
            (WHITESPACE, " "),
            (INT, "2"),
        ],
    );
}

#[test]
fn ampersand_is_always_one_kind() {
    // The 8.1 two-flavour split is parser lookahead (the lexer's doc says why).
    check_php("&", &[(AMP, "&")]);
    check_php("& $x", &[(AMP, "&"), (WHITESPACE, " "), (VARIABLE, "$x")]);
    check_php("&&", &[(AMP_AMP, "&&")]);
    check_php("&=", &[(AMP_EQ, "&=")]);
    check_php("A&B", &[(IDENT, "A"), (AMP, "&"), (IDENT, "B")]);
}

#[test]
fn attribute_open_is_not_a_comment() {
    check_php(
        "#[Attr] # c",
        &[
            (ATTRIBUTE_OPEN, "#["),
            (IDENT, "Attr"),
            (R_BRACK, "]"),
            (WHITESPACE, " "),
            (LINE_COMMENT, "# c"),
        ],
    );
}

#[test]
fn line_comments_end_before_their_newline() {
    // php >= 8.0 puts the newline in T_WHITESPACE, not T_COMMENT
    // (adjudicated on the 8.3 reference; token_map pin P1 updated).
    check_php(
        "// x\n1;",
        &[
            (LINE_COMMENT, "// x"),
            (WHITESPACE, "\n"),
            (INT, "1"),
            (SEMICOLON, ";"),
        ],
    );
    check_php(
        "# x\r\n1;",
        &[
            (LINE_COMMENT, "# x"),
            (WHITESPACE, "\r\n"),
            (INT, "1"),
            (SEMICOLON, ";"),
        ],
    );
    check_php("// at eof", &[(LINE_COMMENT, "// at eof")]);
}

#[test]
fn block_and_doc_comments() {
    check_php("/* x */", &[(BLOCK_COMMENT, "/* x */")]);
    check_php("/** doc */", &[(DOC_COMMENT, "/** doc */")]);
    check_php("/**\n * d\n */", &[(DOC_COMMENT, "/**\n * d\n */")]);
    // Zend: doc requires whitespace after `/**`.
    check_php("/**/", &[(BLOCK_COMMENT, "/**/")]);
    check_php("/**x*/", &[(BLOCK_COMMENT, "/**x*/")]);
    check_php("/***/", &[(BLOCK_COMMENT, "/***/")]);
}

#[test]
fn casts_single_token_with_interior_whitespace() {
    check_php("(int)", &[(CAST, "(int)")]);
    check_php("( int )", &[(CAST, "( int )")]);
    check_php("(\tINT\t)", &[(CAST, "(\tINT\t)")]);
    for c in [
        "(integer)",
        "(bool)",
        "(boolean)",
        "(float)",
        "(double)",
        "(string)",
        "(binary)",
        "(array)",
        "(object)",
        "(OBJECT)",
    ] {
        check_php(c, &[(CAST, c)]);
    }
}

#[test]
fn removed_casts() {
    check_php("(real)", &[(CAST_REMOVED, "(real)")]);
    check_php("(unset)", &[(CAST_REMOVED, "(unset)")]);
    check_php("( Real )", &[(CAST_REMOVED, "( Real )")]);
}

#[test]
fn non_casts_are_parens() {
    check_php("(intx)", &[(L_PAREN, "("), (IDENT, "intx"), (R_PAREN, ")")]);
    check_php(
        "(int x)",
        &[
            (L_PAREN, "("),
            (IDENT, "int"),
            (WHITESPACE, " "),
            (IDENT, "x"),
            (R_PAREN, ")"),
        ],
    );
    check_php("()", &[(L_PAREN, "("), (R_PAREN, ")")]);
}

#[test]
fn cast_applies_anywhere_in_scripting() {
    // Zend's cast rule is purely lexical; even `$a(int)` fuses the cast.
    check_php("$a(int)", &[(VARIABLE, "$a"), (CAST, "(int)")]);
}

#[test]
fn property_name_remap_after_arrow() {
    // Zend ST_LOOKING_FOR_PROPERTY: keywords lex as plain identifiers.
    check_php(
        "$x->list",
        &[(VARIABLE, "$x"), (ARROW, "->"), (IDENT, "list")],
    );
    check_php(
        "$x ?-> for",
        &[
            (VARIABLE, "$x"),
            (WHITESPACE, " "),
            (NULLSAFE_ARROW, "?->"),
            (WHITESPACE, " "),
            (IDENT, "for"),
        ],
    );
    check_php(
        "$x->__CLASS__",
        &[(VARIABLE, "$x"), (ARROW, "->"), (IDENT, "__CLASS__")],
    );
}

#[test]
fn arrow_followed_by_non_label_pops_silently() {
    check_php(
        "$x->$y",
        &[(VARIABLE, "$x"), (ARROW, "->"), (VARIABLE, "$y")],
    );
    check_php("$x->3", &[(VARIABLE, "$x"), (ARROW, "->"), (INT, "3")]);
    check_php(
        "$x->{$y}",
        &[
            (VARIABLE, "$x"),
            (ARROW, "->"),
            (L_CURLY, "{"),
            (VARIABLE, "$y"),
            (R_CURLY, "}"),
        ],
    );
}

#[test]
fn comment_exits_property_mode_like_zend() {
    // Zend's ST_LOOKING_FOR_PROPERTY has no comment rule: the {ANY_CHAR}
    // fallback pops, so a label after the comment lexes in scripting mode
    // and keyword-ness returns. Conformance-relevant; see CONFORMANCE.md.
    check_php(
        "$x-> /*c*/ class",
        &[
            (VARIABLE, "$x"),
            (ARROW, "->"),
            (WHITESPACE, " "),
            (BLOCK_COMMENT, "/*c*/"),
            (WHITESPACE, " "),
            (CLASS_KW, "class"),
        ],
    );
}

#[test]
fn yield_from_is_two_tokens() {
    // php-src fuses T_YIELD_FROM; the stream keeps two tokens, and
    // `ext/tokenizer` fuses them the way token_get_all does (`php_names`
    // says why the fuse is its and not the tables').
    check_php(
        "yield from",
        &[(YIELD_KW, "yield"), (WHITESPACE, " "), (FROM_KW, "from")],
    );
}

#[test]
fn stray_close_brace_keeps_scripting() {
    // Zend pops only with a non-empty state stack: `}` at top level must
    // not fall back to HTML mode.
    check_php(
        "} 1;",
        &[
            (R_CURLY, "}"),
            (WHITESPACE, " "),
            (INT, "1"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn error_bytes_one_token_per_run() {
    check_bytes(
        b"<?php \x00\x01\x02;",
        &[
            (OPEN_TAG, b"<?php "),
            (ERROR, b"\x00\x01\x02"),
            (SEMICOLON, b";"),
        ],
    );
}
