//! What `parse` hands back as diagnostics (design §6.1): php's text at
//! php's line under php's level, in the order php raises them. Every
//! expectation here is what `php -l` printed under the pin at 8.5.9 for
//! the same source, read once; the fixture suites hold the tree, this
//! file holds the list.

// This file reads the diagnostics alone; the dump and the snapshot
// assertion are the fixture suites'.
#[allow(dead_code)]
mod support;

use php_syntax::{FileId, Level, LexOptions, Options, parse};

/// The diagnostics as `(line, level, message)`, in the order handed back.
fn diagnostics(src: &str) -> Vec<(u32, Level, String)> {
    support::parse_lossless(src)
        .diagnostics
        .iter()
        .map(|d| (d.line, d.level, d.message.clone()))
        .collect()
}

/// The first diagnostic's `(line, message)`: what php reports.
fn first(src: &str) -> (u32, String) {
    let all = diagnostics(src);
    let (line, _, message) = all
        .first()
        .cloned()
        .unwrap_or_else(|| (0, Level::Parse, String::new()));
    (line, message)
}

#[test]
fn the_first_diagnostic_is_the_one_php_reports() {
    // The scanner raises as it scans and the parser at the token it did
    // not want: the warning inside the string precedes the parser's word
    // on the string, and a deprecation on line 1 precedes a parse error
    // on line 2.
    assert_eq!(
        diagnostics("<?php echo 1 \"\\777\";"),
        [
            (
                1,
                Level::CompileWarning,
                "Octal escape sequence overflow \\777 is greater than \\377".into()
            ),
            (
                1,
                Level::Parse,
                "syntax error, unexpected double-quoted string \"\\777\", expecting \",\" or \";\""
                    .into()
            ),
        ]
    );
    assert_eq!(
        diagnostics("<?php (integer)$x;\nfoo(1 2);"),
        [
            (
                1,
                Level::Deprecated,
                "Non-canonical cast (integer) is deprecated, use the (int) cast instead".into()
            ),
            (
                2,
                Level::Parse,
                "syntax error, unexpected integer \"2\", expecting \")\"".into()
            ),
        ]
    );
    // php compiles nothing of a file its parser refused: the compile
    // error comes after every parse error, wherever it stands.
    assert_eq!(
        diagnostics("<?php (unset)$x; foo("),
        [
            (1, Level::Parse, "Unclosed '('".into()),
            (
                1,
                Level::CompileError,
                "The (unset) cast is no longer supported".into()
            ),
        ]
    );
    // A site two recovery layers reach is one diagnostic.
    assert_eq!(
        diagnostics("<?php unset($x + $y);"),
        [(
            1,
            Level::Parse,
            "syntax error, unexpected token \"+\", expecting \"->\" or \"?->\" or \"[\"".into()
        )]
    );
}

#[test]
fn the_line_is_the_scanners_after_the_unexpected_token() {
    // A token spanning lines reports its last; the end of the input
    // reports the last line; an unclosed comment the line it opened on.
    assert_eq!(first("<?php echo 1 'a\nb';").0, 2);
    assert_eq!(first("<?php echo 1 \"a\nb\";").0, 2);
    assert_eq!(first("<?php echo 1 <<<EOT\na\nEOT;").0, 2);
    assert_eq!(
        first("<?php foo(\n\n"),
        (3, "Unclosed '(' on line 1".into())
    );
    assert_eq!(
        first("<?php echo 1\n"),
        (
            2,
            "syntax error, unexpected end of file, expecting \",\" or \";\"".into()
        )
    );
    assert_eq!(
        first("<?php $x = /* a\n"),
        (1, "Unterminated comment starting line 1".into())
    );
    // php hands an unclosed single-quoted string back without counting
    // its newlines, so it reports the line it opened on.
    assert_eq!(
        first("<?php $x = 'a\n"),
        (1, "syntax error, unexpected string content \"a\"".into())
    );
}

#[test]
fn the_seven_lexical_fatals_are_the_parsers() {
    // design §6.1: `(real)` and `(unset)`, `$s{0}`, `(void)` placement,
    // nested `const`, octal overflow and the non-canonical cast.
    assert_eq!(
        first("<?php ( Real )$x;").1,
        "The (real) cast has been removed, use (float) instead"
    );
    assert_eq!(
        first("<?php echo 1 (real)$x;").1,
        "The (real) cast has been removed, use (float) instead"
    );
    assert_eq!(
        diagnostics("<?php ( Unset )$x;"),
        [(
            1,
            Level::CompileError,
            "The (unset) cast is no longer supported".into()
        )]
    );
    assert_eq!(
        first("<?php $s{0};").1,
        "syntax error, unexpected token \"{\""
    );
    assert_eq!(
        first("<?php foo($s{0});").1,
        "syntax error, unexpected token \"{\", expecting \")\""
    );
    assert_eq!(
        first("<?php const X = \"s\"{0};").1,
        "syntax error, unexpected token \"{\", expecting \",\" or \";\""
    );
    assert_eq!(
        diagnostics("<?php (void) f() + 1; for ((void)$a; (void)$b, 1; (void)$c) {}"),
        []
    );
    assert_eq!(
        first("<?php $y = (void)$x;").1,
        "syntax error, unexpected token \"(void)\""
    );
    assert_eq!(
        first("<?php $x = [(void)$y];").1,
        "syntax error, unexpected token \"(void)\", expecting \"]\""
    );
    assert_eq!(
        first("<?php return (void)$x;").1,
        "syntax error, unexpected token \"(void)\", expecting \";\""
    );
    assert_eq!(
        first("<?php for (;(void)$x;) {}").1,
        "syntax error, unexpected token \";\", expecting \",\""
    );
    assert_eq!(
        first("<?php function f() { const D = 2; }").1,
        "syntax error, unexpected token \"const\""
    );
    assert_eq!(
        first("<?php if (1): const X = 1; endif;").1,
        "syntax error, unexpected token \"const\", expecting \"elseif\" or \"else\" or \"endif\""
    );
    assert_eq!(diagnostics("<?php namespace A { const X = 1; }"), []);
    assert_eq!(
        diagnostics("<?php $x = \"\\400$y\n\\777\";"),
        [
            (
                1,
                Level::CompileWarning,
                "Octal escape sequence overflow \\400 is greater than \\377".into()
            ),
            (
                2,
                Level::CompileWarning,
                "Octal escape sequence overflow \\777 is greater than \\377".into()
            ),
        ]
    );
    assert_eq!(
        diagnostics("<?php (double)$x;"),
        [(
            1,
            Level::Deprecated,
            "Non-canonical cast (double) is deprecated, use the (float) cast instead".into()
        )]
    );
}

#[test]
fn a_token_that_starts_no_statement_takes_the_lists_clause() {
    assert_eq!(
        first("<?php =").1,
        "syntax error, unexpected token \"=\", expecting end of file"
    );
    assert_eq!(
        first("<?php echo 1; 'abc").1,
        "syntax error, unexpected string content \"abc\", expecting end of file"
    );
    assert_eq!(
        first("<?php { = }").1,
        "syntax error, unexpected token \"=\""
    );
    assert_eq!(
        first("<?php if (1): = endif;").1,
        "syntax error, unexpected token \"=\", expecting \"elseif\" or \"else\" or \"endif\""
    );
    assert_eq!(
        first("<?php if (1): else: = endif;").1,
        "syntax error, unexpected token \"=\""
    );
    assert_eq!(
        first("<?php while (1): endif;").1,
        "syntax error, unexpected token \"endif\""
    );
    assert_eq!(
        first("<?php switch (1) { = }").1,
        "syntax error, unexpected token \"=\", expecting \"case\" or \"default\" or \"}\""
    );
    assert_eq!(
        first("<?php switch (1): case 1: echo 1;").1,
        "syntax error, unexpected end of file, expecting \"endswitch\" or \"case\" or \"default\""
    );
    assert_eq!(
        first("<?php declare(ticks=1)").1,
        "syntax error, unexpected end of file"
    );
}

#[test]
fn what_the_scanner_hands_over_as_a_token_the_parser_names() {
    // A stray byte, an unclosed string, the identifier after a malformed
    // number: php's scanner raises nothing on them, its parser does.
    assert_eq!(
        first("<?php $x = 1 \u{0E};").1,
        "syntax error, unexpected character 0x0E"
    );
    assert_eq!(
        first("<?php \u{0E}").1,
        "syntax error, unexpected character 0x0E, expecting end of file"
    );
    assert_eq!(
        first("<?php $x = \"$b[\u{0E}]\";").1,
        "syntax error, unexpected character 0x0E, expecting \"-\" or identifier or variable or number"
    );
    assert_eq!(
        first("<?php echo 'abc").1,
        "syntax error, unexpected string content \"abc\""
    );
    assert_eq!(
        first("<?php '").1,
        "syntax error, unexpected string content \"\", expecting end of file"
    );
    assert_eq!(
        first("<?php echo \"abc").1,
        "syntax error, unexpected end of file, expecting variable or \"${\" or \"{$\""
    );
    assert_eq!(
        first("<?php \"").1,
        "syntax error, unexpected end of file, expecting variable or string content or \"${\" or \"{$\""
    );
    assert_eq!(
        first("<?php echo \"abc $x").1,
        "syntax error, unexpected end of file"
    );
    assert_eq!(
        first("<?php echo `abc $x").1,
        "syntax error, unexpected end of file, expecting \"`\""
    );
    assert_eq!(
        first("<?php echo <<<'EOT'\nabc").1,
        "syntax error, unexpected end of file, expecting variable or heredoc end or \"${\" or \"{$\""
    );
    assert_eq!(
        first("<?php echo \"$x[0").1,
        "syntax error, unexpected end of file, expecting \"]\""
    );
    assert_eq!(
        first("<?php $x = \"$a[]\";").1,
        "syntax error, unexpected token \"]\", expecting \"-\" or identifier or variable or number"
    );
    // A byte that pops `ST_VAR_OFFSET` is handed over as a zero-width
    // string content, so the token php names is empty and stands on the
    // line before the newline it pops on.
    assert_eq!(
        first("<?php $x = \"$a[ 1 ]\";"),
        (
            1,
            "syntax error, unexpected string content \"\", expecting \"-\" or identifier or variable or number".into()
        )
    );
    assert_eq!(
        first("<?php $x = \"$a[\n1]\";"),
        (
            1,
            "syntax error, unexpected string content \"\", expecting \"-\" or identifier or variable or number".into()
        )
    );
    assert_eq!(
        first("<?php echo 0x_1;").1,
        "syntax error, unexpected identifier \"x_1\", expecting \",\" or \";\""
    );
    assert_eq!(
        first("<?php $x = 1__2;").1,
        "syntax error, unexpected identifier \"__2\""
    );
}

#[test]
fn the_options_are_the_file_and_the_lexers() {
    let parsed = parse(
        b"<? echo 1 2;",
        &Options {
            file: FileId(7),
            lex: LexOptions {
                short_open_tag: false,
            },
        },
    );
    // Without short open tags the whole source is inline HTML: nothing
    // to refuse.
    assert_eq!(parsed.diagnostics, []);
    let parsed = parse(
        b"<?php echo 1 2;",
        &Options {
            file: FileId(7),
            ..Options::default()
        },
    );
    assert_eq!(parsed.diagnostics[0].span.file, FileId(7));
    assert_eq!(
        parsed.lines.line_number(parsed.diagnostics[0].span.start),
        1
    );
}

#[test]
fn instanceof_takes_a_class_name_reference() {
    // php's `class_name_reference`: `class_name` is `T_STATIC | name`, so
    // `static` stands alone on the right of `instanceof` (#262), and a
    // `new_variable` or a parenthesised expression stands there too.
    for src in [
        "<?php $x instanceof static;",
        "<?php $x instanceof static::$y;",
        "<?php $x instanceof static::$b->c;",
        "<?php $x instanceof namespace\\A;",
        "<?php $x instanceof $y->z;",
        "<?php $x instanceof $y[0]->z::$w;",
        "<?php $x instanceof A::$b[0];",
        "<?php $x instanceof (A);",
        "<?php $x instanceof static ?? 1;",
        // `%nonassoc` on a tier whose right operand is not an `expr`:
        // the chain is legal, `($x instanceof A) instanceof B`.
        "<?php $x instanceof A instanceof B;",
    ] {
        assert_eq!(diagnostics(src), [], "{src}");
    }
    // What is not a class reference is the bare unexpected token; after
    // `::` php spells the selector it wants; a trailer after the class
    // reference dangles into the statement, which reports it as it
    // reports one after `new A`.
    for (src, message) in [
        (
            "<?php $x instanceof 1;",
            "syntax error, unexpected integer \"1\"",
        ),
        (
            "<?php $x instanceof \"A\";",
            "syntax error, unexpected double-quoted string \"A\"",
        ),
        (
            "<?php $x instanceof new A;",
            "syntax error, unexpected token \"new\"",
        ),
        (
            "<?php $x instanceof;",
            "syntax error, unexpected token \";\"",
        ),
        (
            "<?php $x instanceof A::C;",
            "syntax error, unexpected identifier \"C\", expecting variable or \"$\"",
        ),
        (
            "<?php $x instanceof static::C;",
            "syntax error, unexpected identifier \"C\", expecting variable or \"$\"",
        ),
        (
            "<?php $x instanceof A->p;",
            "syntax error, unexpected token \"->\"",
        ),
        (
            "<?php $x instanceof A[0];",
            "syntax error, unexpected token \"[\"",
        ),
        (
            "<?php $x instanceof (A)->b;",
            "syntax error, unexpected token \"->\"",
        ),
        (
            "<?php $x instanceof $y();",
            "syntax error, unexpected token \"(\"",
        ),
        (
            "<?php echo $x instanceof A[0];",
            "syntax error, unexpected token \"[\", expecting \",\" or \";\"",
        ),
        // `new` reads the same production, and a literal there is the
        // unexpected token with `new`'s own clause.
        (
            "<?php new 1;",
            "syntax error, unexpected integer \"1\", expecting \"class\"",
        ),
        (
            "<?php new \"A\";",
            "syntax error, unexpected double-quoted string \"A\", expecting \"class\"",
        ),
    ] {
        assert_eq!(first(src), (1, message.into()), "{src}");
    }
}
