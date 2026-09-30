//! Open/close tags, inline HTML segmentation, BOM, short-tag option
//! (Appendix C: `<?=`; short open tags; BOM before `<?php`; file with no
//! PHP at all; `?>` eats one newline; comment terminated by `?>`).

use super::*;

#[test]
fn empty_input_no_tokens() {
    check("", &[]);
}

#[test]
fn no_php_at_all_is_one_inline_html() {
    check(
        "hello <b>world</b>\n",
        &[(INLINE_HTML, "hello <b>world</b>\n")],
    );
}

#[test]
fn bom_before_open_tag_is_inline_html() {
    check_bytes(
        b"\xEF\xBB\xBF<?php 1;",
        &[
            (INLINE_HTML, b"\xEF\xBB\xBF"),
            (OPEN_TAG, b"<?php "),
            (INT, b"1"),
            (SEMICOLON, b";"),
        ],
    );
}

#[test]
fn open_tag_includes_one_whitespace_char() {
    // php-src T_OPEN_TAG: `<?php` plus exactly one space/tab/newline.
    check(
        "<?php $a;",
        &[(OPEN_TAG, "<?php "), (VARIABLE, "$a"), (SEMICOLON, ";")],
    );
    check(
        "<?php\n$a;",
        &[(OPEN_TAG, "<?php\n"), (VARIABLE, "$a"), (SEMICOLON, ";")],
    );
    check(
        "<?php\t$a;",
        &[(OPEN_TAG, "<?php\t"), (VARIABLE, "$a"), (SEMICOLON, ";")],
    );
    // \r\n counts as the one whitespace "char" (both bytes in the token).
    check(
        "<?php\r\n$a;",
        &[(OPEN_TAG, "<?php\r\n"), (VARIABLE, "$a"), (SEMICOLON, ";")],
    );
    // Two newlines: only the first joins the tag.
    check(
        "<?php\n\n1;",
        &[
            (OPEN_TAG, "<?php\n"),
            (WHITESPACE, "\n"),
            (INT, "1"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn open_tag_at_eof_without_whitespace() {
    check("<?php", &[(OPEN_TAG, "<?php")]);
}

#[test]
fn open_tag_case_insensitive() {
    check(
        "<?PHP 1;",
        &[(OPEN_TAG, "<?PHP "), (INT, "1"), (SEMICOLON, ";")],
    );
}

#[test]
fn open_tag_echo_never_consumes_whitespace() {
    check(
        "<?= $x ?>",
        &[
            (OPEN_TAG_ECHO, "<?="),
            (WHITESPACE, " "),
            (VARIABLE, "$x"),
            (WHITESPACE, " "),
            (CLOSE_TAG, "?>"),
        ],
    );
}

#[test]
fn open_tag_echo_works_with_short_tags_off() {
    check_opts(
        "<?=1;",
        &no_short_tags(),
        &[(OPEN_TAG_ECHO, "<?="), (INT, "1"), (SEMICOLON, ";")],
    );
}

#[test]
fn short_open_tag_on() {
    check(
        "<? 1;",
        &[
            (OPEN_TAG, "<?"),
            (WHITESPACE, " "),
            (INT, "1"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn short_open_tag_off_is_html() {
    check_opts("<? 1;", &no_short_tags(), &[(INLINE_HTML, "<? 1;")]);
}

#[test]
fn glued_open_tag_is_short_tag_plus_ident() {
    // `<?phpinfo()` — the famous one: `<?php` requires whitespace, so the
    // short tag wins and `phpinfo` is an identifier (Zend longest match).
    check(
        "<?phpinfo();",
        &[
            (OPEN_TAG, "<?"),
            (IDENT, "phpinfo"),
            (L_PAREN, "("),
            (R_PAREN, ")"),
            (SEMICOLON, ";"),
        ],
    );
    // With short tags off the failed `<?php` match falls back to HTML.
    check_opts(
        "<?phpinfo();",
        &no_short_tags(),
        &[(INLINE_HTML, "<?phpinfo();")],
    );
}

#[test]
fn close_tag_eats_one_newline() {
    check(
        "<?php ?>\nhtml",
        &[
            (OPEN_TAG, "<?php "),
            (CLOSE_TAG, "?>\n"),
            (INLINE_HTML, "html"),
        ],
    );
    check(
        "<?php ?>\r\nhtml",
        &[
            (OPEN_TAG, "<?php "),
            (CLOSE_TAG, "?>\r\n"),
            (INLINE_HTML, "html"),
        ],
    );
    // Only one: the second newline is HTML.
    check(
        "<?php ?>\n\n",
        &[
            (OPEN_TAG, "<?php "),
            (CLOSE_TAG, "?>\n"),
            (INLINE_HTML, "\n"),
        ],
    );
    check("<?php ?>", &[(OPEN_TAG, "<?php "), (CLOSE_TAG, "?>")]);
}

#[test]
fn html_php_html_sandwich() {
    check(
        "a<?php 1; ?>b<?php 2;",
        &[
            (INLINE_HTML, "a"),
            (OPEN_TAG, "<?php "),
            (INT, "1"),
            (SEMICOLON, ";"),
            (WHITESPACE, " "),
            (CLOSE_TAG, "?>"),
            (INLINE_HTML, "b"),
            (OPEN_TAG, "<?php "),
            (INT, "2"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn html_lt_question_without_tag_stays_html_when_short_off() {
    check_opts(
        "<x><?xml version=\"1.0\"?>",
        &no_short_tags(),
        &[(INLINE_HTML, "<x><?xml version=\"1.0\"?>")],
    );
}

#[test]
fn lone_short_tag_at_eof() {
    check("html<?", &[(INLINE_HTML, "html"), (OPEN_TAG, "<?")]);
}

#[test]
fn line_comment_terminated_by_close_tag() {
    // The classic gotcha: `//` ends at `?>` WITHOUT consuming it.
    check(
        "<?php // c ?>\nx",
        &[
            (OPEN_TAG, "<?php "),
            (LINE_COMMENT, "// c "),
            (CLOSE_TAG, "?>\n"),
            (INLINE_HTML, "x"),
        ],
    );
    check(
        "<?php # c ?>",
        &[
            (OPEN_TAG, "<?php "),
            (LINE_COMMENT, "# c "),
            (CLOSE_TAG, "?>"),
        ],
    );
}

#[test]
fn close_tag_inside_string_is_text() {
    check(
        "<?php \"?>\";",
        &[
            (OPEN_TAG, "<?php "),
            (DOUBLE_QUOTE, "\""),
            (STRING_TEXT, "?>"),
            (DOUBLE_QUOTE, "\""),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn close_tag_inside_braces_still_closes() {
    // Zend: `?>` does BEGIN(INITIAL) even with pushed scripting states;
    // the reopened `<?php` continues and `}` pops the surviving entry.
    check(
        "<?php { ?>h<?php }",
        &[
            (OPEN_TAG, "<?php "),
            (L_CURLY, "{"),
            (WHITESPACE, " "),
            (CLOSE_TAG, "?>"),
            (INLINE_HTML, "h"),
            (OPEN_TAG, "<?php "),
            (R_CURLY, "}"),
        ],
    );
}

/// `compile_string`'s position for `eval`: the first byte is code, a
/// `<?php` there is no open tag but the `<` and `?` php's scanner reads
/// in code, and `?>` leaves for inline HTML as it does in a file.
#[test]
fn after_the_open_tag_the_first_byte_is_code() {
    let code = LexOptions {
        position: Position::AfterOpenTag,
        ..LexOptions::default()
    };
    check_opts(
        "$a = 1; ?>h<?php }",
        &code,
        &[
            (VARIABLE, "$a"),
            (WHITESPACE, " "),
            (EQ, "="),
            (WHITESPACE, " "),
            (INT, "1"),
            (SEMICOLON, ";"),
            (WHITESPACE, " "),
            (CLOSE_TAG, "?>"),
            (INLINE_HTML, "h"),
            (OPEN_TAG, "<?php "),
            (R_CURLY, "}"),
        ],
    );
    check_opts(
        "\n\necho",
        &code,
        &[(WHITESPACE, "\n\n"), (ECHO_KW, "echo")],
    );
    check("echo", &[(INLINE_HTML, "echo")]);
}

/// `CG(skip_shebang)`'s position: a first line that opens with `#!` is
/// one token the parser passes over, through its line end, a `\r` where
/// no `\n` follows, and inline HTML after it; anything else is the file
/// as it stands.
#[test]
fn at_the_shebang_a_first_hash_bang_line_is_passed_over() {
    let shebang = LexOptions {
        position: Position::AtShebang,
        ..LexOptions::default()
    };
    check_opts(
        "#!/usr/bin/env php\nola\n<?php 1;",
        &shebang,
        &[
            (SHEBANG, "#!/usr/bin/env php\n"),
            (INLINE_HTML, "ola\n"),
            (OPEN_TAG, "<?php "),
            (INT, "1"),
            (SEMICOLON, ";"),
        ],
    );
    check_opts("#!php\r", &shebang, &[(SHEBANG, "#!php\r")]);
    check_opts("#!php", &shebang, &[(INLINE_HTML, "#!php")]);
    check_opts(" #!php\n", &shebang, &[(INLINE_HTML, " #!php\n")]);
    check("#!php\n", &[(INLINE_HTML, "#!php\n")]);
}
