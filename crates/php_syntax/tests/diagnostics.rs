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
    let arena = php_syntax::Arena::new();
    let parsed = parse(
        &arena,
        b"<? echo 1 2;",
        &Options {
            file: FileId(7),
            lex: LexOptions {
                short_open_tag: false,
                ..LexOptions::default()
            },
        },
    );
    // Without short open tags the whole source is inline HTML: nothing
    // to refuse.
    assert_eq!(parsed.diagnostics, []);
    let parsed = parse(
        &arena,
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

#[test]
fn names_php_takes_as_t_string_alone_refuse_a_keyword() {
    // php's `use_declaration` alias, `function_name` and every class-like
    // name are `T_STRING` (`function_name` takes `T_READONLY` beside it),
    // and `callable` is `T_CALLABLE` to php's scanner where the lexer here
    // leaves it a name: each of these is a `ParseError` under the pin.
    for (src, message) in [
        (
            "<?php use A\\B as list;",
            "syntax error, unexpected token \"list\", expecting identifier",
        ),
        (
            "<?php use A\\B as callable;",
            "syntax error, unexpected token \"callable\", expecting identifier",
        ),
        (
            "<?php use A\\B as __CLASS__;",
            "syntax error, unexpected token \"__CLASS__\", expecting identifier",
        ),
        (
            "<?php function callable(){}",
            "syntax error, unexpected token \"callable\", expecting \"(\"",
        ),
        (
            "<?php callable();",
            "syntax error, unexpected token \"callable\", expecting end of file",
        ),
        (
            "<?php callable::x;",
            "syntax error, unexpected token \"callable\", expecting end of file",
        ),
        (
            "<?php echo callable();",
            "syntax error, unexpected token \"callable\"",
        ),
        (
            "<?php class readonly {}",
            "syntax error, unexpected token \"readonly\", expecting identifier",
        ),
        (
            "<?php interface readonly {}",
            "syntax error, unexpected token \"readonly\", expecting identifier",
        ),
        (
            "<?php trait readonly {}",
            "syntax error, unexpected token \"readonly\", expecting identifier",
        ),
        (
            "<?php enum readonly {}",
            "syntax error, unexpected token \"readonly\", expecting identifier",
        ),
        (
            "<?php class callable {}",
            "syntax error, unexpected token \"callable\", expecting identifier",
        ),
    ] {
        assert_eq!(first(src), (1, message.into()), "{src}");
    }
    // The line is the token's: php reports where `callable` stands.
    assert_eq!(
        first("<?php\n\nfunction callable(){}"),
        (
            3,
            "syntax error, unexpected token \"callable\", expecting \"(\"".into()
        )
    );
    // Where php takes the word, no diagnostic: `readonly` as a function's
    // name, `callable` after `::` and `->`, as a type and as a member.
    for src in [
        "<?php function readonly(){}",
        "<?php A::callable;",
        "<?php $x->callable;",
        "<?php function f(callable $c){}",
        "<?php function f(): ?callable {}",
        "<?php f(callable: 1);",
        "<?php class A { function callable(){} const callable = 1; }",
        "<?php use A\\B as enum;",
    ] {
        assert_eq!(diagnostics(src), [], "{src}");
    }
}

#[test]
fn a_fused_set_visibility_is_the_one_token_phps_scanner_fused() {
    // php's scanner returns `private(set)` as one `T_PRIVATE_SET`, and
    // its unexpected-token clause names the four characters; with a
    // space inside, the four are four and the clause names the first.
    assert_eq!(
        first("<?php private(set) int $x;"),
        (
            1,
            "syntax error, unexpected token \"private(set)\", expecting end of file".into()
        )
    );
    assert_eq!(
        first("<?php\nPROTECTED(SET) int $x;"),
        (
            2,
            "syntax error, unexpected token \"protected(set)\", expecting end of file".into()
        )
    );
    assert_eq!(
        first("<?php private (set) int $x;"),
        (
            1,
            "syntax error, unexpected token \"private\", expecting end of file".into()
        )
    );
}

/// A diagnostic that names the token it wanted as the token it did not
/// want: `unexpected token "X", expecting "X"`. No site can raise one;
/// the halt rule did (#265), taking `;` alone where php's scanner
/// returns `?>` as `';'`.
fn names_the_wanted_token_as_unwanted(message: &str) -> bool {
    let Some(rest) = message.strip_prefix("syntax error, unexpected token \"") else {
        return false;
    };
    let Some((unwanted, clause)) = rest.split_once("\", expecting ") else {
        return false;
    };
    clause
        .split(" or ")
        .any(|wanted| wanted == format!("\"{unwanted}\""))
}

#[test]
fn halt_compiler_takes_the_close_tag_as_its_semicolon() {
    // `T_HALT_COMPILER '(' ')' ';'` with the close tag as the `;`: a
    // whole program php accepts, with or without a trailer.
    assert_eq!(diagnostics("<?php __halt_compiler() ?>"), []);
    assert_eq!(diagnostics("<?php __halt_compiler() ?>\nrest"), []);
    assert_eq!(diagnostics("<?php __halt_compiler() ?>rest"), []);
}

#[test]
fn no_site_names_the_wanted_token_as_the_unwanted_one() {
    assert!(names_the_wanted_token_as_unwanted(
        "syntax error, unexpected token \";\", expecting \";\""
    ));
    assert!(names_the_wanted_token_as_unwanted(
        "syntax error, unexpected token \";\", expecting \",\" or \";\""
    ));
    assert!(!names_the_wanted_token_as_unwanted(
        "syntax error, unexpected token \";\", expecting \")\""
    ));
    // Every statement whose terminator is a `;`, with `?>` standing where
    // the `;` is due, followed by a trailer and by nothing; `php -l`
    // takes each clean (probed 8.5.9), and none may raise the sentence.
    let terminated = [
        "__halt_compiler()",
        "echo 1",
        "print 1",
        "$x = 1",
        "f()",
        "return",
        "return 1",
        "global $x",
        "static $x",
        "static $x = 1",
        "unset($x)",
        "const X = 1",
        "use A\\B",
        "use function A\\f",
        "namespace A",
        "a: throw $e",
        "do {} while (1)",
        "declare(strict_types=1)",
        "abstract class A { abstract function f(); }",
        "interface I { const X = 1; function f(); }",
        "class C { public $x; public int $y = 1; use T; }",
        "enum E: int { case A = 1; }",
        "$x = fn() => 1",
        "$x = function() {}",
        "$x = new class {}",
        "exit",
        "exit(1)",
        "list($a) = $b",
        "[$a] = $b",
    ];
    for stmt in terminated {
        for tail in ["", "\n", "\nrest", "rest"] {
            let src = format!("<?php {stmt} ?>{tail}");
            let diags = diagnostics(&src);
            assert_eq!(diags, [], "{src:?}");
        }
    }
}

#[test]
fn no_site_names_the_wanted_token_as_the_unwanted_one_on_a_stray_close_tag() {
    // Where the `?>` stands in for a `;` that a construct does not take,
    // php's word names the token it wanted, never the one it got, as
    // the one it wanted.
    let unterminated = [
        "if (1) ?>",
        "while (1) ?>",
        "for (;;) ?>",
        "foreach ($a as $b) ?>",
        "switch ($a) ?>",
        "function f() ?>",
        "class C ?>",
        "class C { function f() ?> }",
        "$x = ?>",
        "echo 1 + ?>",
        "f( ?>",
        "$a[ ?>",
        "[1, ?>",
        "\"{$a ?>",
        "namespace ?>",
        "use ?>",
        "const ?>",
        "const X ?>",
        "const X = ?>",
        "static ?>",
        "global ?>",
        "unset( ?>",
        "goto ?>",
        "try {} ?>",
        "try {} catch ?>",
        "match ($a) ?>",
        "$x = match ($a) { ?>",
        "fn() ?>",
        "function() use ?>",
        "declare ?>",
        "declare(ticks ?>",
        "enum E ?>",
        "enum E: ?>",
        "class C extends ?>",
        "class C implements ?>",
        "interface I extends ?>",
        "class C { use ?>",
        "class C { public ?>",
        "class C { const ?>",
        "class C { case ?>",
        "$x instanceof ?>",
        "new ?>",
        "clone ?>",
        "throw ?>",
        "yield from ?>",
        "$x?->",
        "$x->",
        "$x::",
        "A::",
        "#[A ?>",
        "#[A] ?>",
        "__halt_compiler ?>",
        "__halt_compiler( ?>",
    ];
    for stmt in unterminated {
        for tail in ["", "\nrest"] {
            let src = format!("<?php {stmt}{tail}");
            for (_, _, message) in diagnostics(&src) {
                assert!(
                    !names_the_wanted_token_as_unwanted(&message),
                    "{src:?} raised {message:?}"
                );
            }
        }
    }
}

#[test]
fn callable_is_refused_where_php_reads_a_class_reference_a_label_a_const_or_a_use_name() {
    // php's scanner hands the parser `T_CALLABLE`, and `class_name`,
    // `goto`'s label, `const_decl`'s name and `use_declaration`'s name are
    // `T_STRING` or a namespaced name: each is a `ParseError` under the
    // pin, with php's clause at that state.
    for (src, message) in [
        (
            "<?php new callable;",
            "syntax error, unexpected token \"callable\", expecting \"class\"",
        ),
        (
            "<?php new callable();",
            "syntax error, unexpected token \"callable\", expecting \"class\"",
        ),
        (
            "<?php echo new callable;",
            "syntax error, unexpected token \"callable\", expecting \"class\"",
        ),
        (
            "<?php $x instanceof callable;",
            "syntax error, unexpected token \"callable\"",
        ),
        (
            "<?php goto callable;",
            "syntax error, unexpected token \"callable\", expecting identifier",
        ),
        (
            "<?php const callable = 1;",
            "syntax error, unexpected token \"callable\", expecting identifier",
        ),
        (
            "<?php const A = 1, callable = 2;",
            "syntax error, unexpected token \"callable\", expecting identifier",
        ),
        (
            "<?php use callable;",
            "syntax error, unexpected token \"callable\"",
        ),
        (
            "<?php use callable as x;",
            "syntax error, unexpected token \"callable\"",
        ),
        (
            "<?php use function callable;",
            "syntax error, unexpected token \"callable\", expecting identifier or fully qualified name or namespaced name",
        ),
        (
            "<?php use const callable;",
            "syntax error, unexpected token \"callable\", expecting identifier or fully qualified name or namespaced name",
        ),
        (
            "<?php use A\\{callable};",
            "syntax error, unexpected token \"callable\", expecting identifier or namespaced name or \"function\" or \"const\"",
        ),
    ] {
        assert_eq!(first(src), (1, message.into()), "{src}");
    }
    // The line is the token's: php reports where `callable` stands.
    assert_eq!(
        first("<?php\n\nnew callable;"),
        (
            3,
            "syntax error, unexpected token \"callable\", expecting \"class\"".into()
        )
    );
    // Where php takes the word, no diagnostic: `callable` as a type, an
    // anonymous class, a class constant and a qualified `use` name whose
    // last segment is `callable` (that one is the compiler's fatal).
    for src in [
        "<?php function f(callable $c){}",
        "<?php function f(): ?callable {}",
        "<?php new class {};",
        "<?php $x instanceof A;",
        "<?php goto a; a: ;",
        "<?php const A = 1;",
        "<?php class A { const callable = 1; }",
        "<?php use A\\callable;",
        "<?php use A\\B;",
    ] {
        assert_eq!(diagnostics(src), [], "{src}");
    }
}
