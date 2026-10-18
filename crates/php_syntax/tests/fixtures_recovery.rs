//! Error-recovery fixtures (design §4.9): the targeted repairs, the
//! synchronisation to an anchor set, the bounded absorption into an
//! `ERROR` node and the forward-progress worst cases. Every fixture here
//! stays lossless: the harness holds the token stream to the source on
//! every parse. Read at Mahout `1b3104a8` and regenerated once against
//! the arena's dump.

mod support;

use expect_test::expect;
use support::{check, parse_fixture, parse_lossless};

// ---------------------------------------------------------------------------
// Targeted single-token repairs
// ---------------------------------------------------------------------------

#[test]
fn missing_semicolon_repair() {
    // A newline before the next statement: the high-confidence repair.
    check(
        "<?php $a = 1\n$b = 2;",
        expect![[r#"
            SOURCE_FILE@0..20
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..12
                ASSIGN_EXPR@6..12
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  EQ@9..10 "="
                  LITERAL_EXPR@11..12
                    INT@11..12 "1"
              EXPR_STMT@13..20
                ASSIGN_EXPR@13..19
                  SIMPLE_VAR_EXPR@13..15
                    VARIABLE@13..15 "$b"
                  EQ@16..17 "="
                  LITERAL_EXPR@18..19
                    INT@18..19 "2"
                SEMICOLON@19..20 ";"
            error@13..15 syntax error, unexpected variable "$b"
        "#]],
    );
}

#[test]
fn missing_comma_in_array() {
    check(
        "<?php [1 2];",
        expect![[r#"
            SOURCE_FILE@0..12
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..12
                ARRAY_EXPR@6..11
                  L_BRACK@6..7 "["
                  ARRAY_ELEMENT@7..8
                    LITERAL_EXPR@7..8
                      INT@7..8 "1"
                  ARRAY_ELEMENT@9..10
                    LITERAL_EXPR@9..10
                      INT@9..10 "2"
                  R_BRACK@10..11 "]"
                SEMICOLON@11..12 ";"
            error@9..10 syntax error, unexpected integer "2", expecting "]"
        "#]],
    );
}

#[test]
fn missing_comma_in_args() {
    check(
        "<?php f(1 2);",
        expect![[r#"
            SOURCE_FILE@0..13
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..13
                CALL_EXPR@6..12
                  IDENT@6..7 "f"
                  ARG_LIST@7..12
                    L_PAREN@7..8 "("
                    LITERAL_EXPR@8..9
                      INT@8..9 "1"
                    LITERAL_EXPR@10..11
                      INT@10..11 "2"
                    R_PAREN@11..12 ")"
                SEMICOLON@12..13 ";"
            error@10..11 syntax error, unexpected integer "2", expecting ")"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Unclosed delimiters
// ---------------------------------------------------------------------------

#[test]
fn unclosed_paren() {
    check(
        "<?php f(1;",
        expect![[r#"
            SOURCE_FILE@0..10
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..10
                CALL_EXPR@6..9
                  IDENT@6..7 "f"
                  ARG_LIST@7..9
                    L_PAREN@7..8 "("
                    LITERAL_EXPR@8..9
                      INT@8..9 "1"
                SEMICOLON@9..10 ";"
            error@9..10 syntax error, unexpected token ";", expecting ")"
        "#]],
    );
}

#[test]
fn unclosed_brace_eof_in_class_body() {
    check(
        "<?php class C { public $x = 1;",
        expect![[r#"
            SOURCE_FILE@0..30
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..30
                CLASS_KW@6..11 "class"
                IDENT@12..13 "C"
                CLASS_BODY@14..30
                  L_CURLY@14..15 "{"
                  PROPERTY_DECL@16..30
                    PUBLIC_KW@16..22 "public"
                    PROPERTY_DECLARATOR@23..29
                      VARIABLE@23..25 "$x"
                      EQ@26..27 "="
                      LITERAL_EXPR@28..29
                        INT@28..29 "1"
                    SEMICOLON@29..30 ";"
            error@30..30 Unclosed '{'
        "#]],
    );
}

#[test]
fn unclosed_bracket() {
    check(
        "<?php $a = [1, 2;",
        expect![[r#"
            SOURCE_FILE@0..17
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..17
                ASSIGN_EXPR@6..16
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  EQ@9..10 "="
                  ARRAY_EXPR@11..16
                    L_BRACK@11..12 "["
                    ARRAY_ELEMENT@12..13
                      LITERAL_EXPR@12..13
                        INT@12..13 "1"
                    COMMA@13..14 ","
                    ARRAY_ELEMENT@15..16
                      LITERAL_EXPR@15..16
                        INT@15..16 "2"
                SEMICOLON@16..17 ";"
            error@16..17 syntax error, unexpected token ";", expecting "]"
        "#]],
    );
}

#[test]
fn stray_closing_paren() {
    check(
        "<?php $a = 1; ) $b = 2;",
        expect![[r#"
            SOURCE_FILE@0..23
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..13
                ASSIGN_EXPR@6..12
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  EQ@9..10 "="
                  LITERAL_EXPR@11..12
                    INT@11..12 "1"
                SEMICOLON@12..13 ";"
              ERROR@14..15
                R_PAREN@14..15 ")"
              EXPR_STMT@16..23
                ASSIGN_EXPR@16..22
                  SIMPLE_VAR_EXPR@16..18
                    VARIABLE@16..18 "$b"
                  EQ@19..20 "="
                  LITERAL_EXPR@21..22
                    INT@21..22 "2"
                SEMICOLON@22..23 ";"
            error@14..15 Unmatched ')'
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Alternative-syntax terminator mismatch
// ---------------------------------------------------------------------------

#[test]
fn wrong_alt_terminator() {
    check(
        "<?php if (1): endwhile;",
        expect![[r#"
            SOURCE_FILE@0..23
              OPEN_TAG@0..6 "<?php "
              IF_STMT@6..23
                IF_KW@6..8 "if"
                L_PAREN@9..10 "("
                LITERAL_EXPR@10..11
                  INT@10..11 "1"
                R_PAREN@11..12 ")"
                COLON@12..13 ":"
                STMT_LIST@14..14
                ERROR@14..22
                  ENDWHILE_KW@14..22 "endwhile"
                SEMICOLON@22..23 ";"
            error@14..22 syntax error, unexpected token "endwhile", expecting "elseif" or "else" or "endif"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// `?>` inside an unfinished construct
// ---------------------------------------------------------------------------

#[test]
fn close_tag_inside_unfinished_call() {
    check(
        "<?php f( ?>",
        expect![[r#"
            SOURCE_FILE@0..11
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..8
                CALL_EXPR@6..8
                  IDENT@6..7 "f"
                  ARG_LIST@7..8
                    L_PAREN@7..8 "("
              CLOSE_TAG@9..11 "?>"
            error@9..11 syntax error, unexpected token ";"
        "#]],
    );
}

#[test]
fn inline_html_hard_anchor_recovery() {
    // Inline-HTML boundaries are hard anchors: the trailing segment
    // recovers per-segment by construction.
    check(
        "<?php broken( ?>HTML<?php echo 1;",
        expect![[r#"
            SOURCE_FILE@0..33
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..13
                CALL_EXPR@6..13
                  IDENT@6..12 "broken"
                  ARG_LIST@12..13
                    L_PAREN@12..13 "("
              CLOSE_TAG@14..16 "?>"
              INLINE_HTML_STMT@16..20
                INLINE_HTML@16..20 "HTML"
              OPEN_TAG@20..26 "<?php "
              ECHO_STMT@26..33
                ECHO_KW@26..30 "echo"
                LITERAL_EXPR@31..32
                  INT@31..32 "1"
                SEMICOLON@32..33 ";"
            error@14..16 syntax error, unexpected token ";"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Garbage and keyword soup
// ---------------------------------------------------------------------------

#[test]
fn garbage_mid_expression() {
    check(
        "<?php $a = 1 + @@ 2;",
        expect![[r#"
            SOURCE_FILE@0..20
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..20
                ASSIGN_EXPR@6..19
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  EQ@9..10 "="
                  BINARY_EXPR@11..19
                    LITERAL_EXPR@11..12
                      INT@11..12 "1"
                    PLUS@13..14 "+"
                    ERROR_SUPPRESS_EXPR@15..19
                      AT@15..16 "@"
                      ERROR_SUPPRESS_EXPR@16..19
                        AT@16..17 "@"
                        LITERAL_EXPR@18..19
                          INT@18..19 "2"
                SEMICOLON@19..20 ";"
        "#]],
    );
}

#[test]
fn keyword_soup_if_if_if() {
    check(
        "<?php if if if",
        expect![[r#"
            SOURCE_FILE@0..14
              OPEN_TAG@0..6 "<?php "
              IF_STMT@6..14
                IF_KW@6..8 "if"
                IF_STMT@9..14
                  IF_KW@9..11 "if"
                  IF_STMT@12..14
                    IF_KW@12..14 "if"
            error@9..11 syntax error, unexpected token "if", expecting "("
            error@12..14 syntax error, unexpected token "if", expecting "("
            error@14..14 syntax error, unexpected end of file, expecting "("
            error@14..14 syntax error, unexpected end of file
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Broken declarations
// ---------------------------------------------------------------------------

#[test]
fn class_missing_name() {
    // php rejects `class {}`; the CLASS_DECL shape is kept beside the
    // diagnostic.
    check(
        "<?php class { }",
        expect![[r#"
            SOURCE_FILE@0..15
              OPEN_TAG@0..6 "<?php "
              CLASS_DECL@6..15
                CLASS_KW@6..11 "class"
                CLASS_BODY@12..15
                  L_CURLY@12..13 "{"
                  R_CURLY@14..15 "}"
            error@12..13 syntax error, unexpected token "{", expecting identifier
        "#]],
    );
}

#[test]
fn function_without_body() {
    check(
        "<?php function f();",
        expect![[r#"
            SOURCE_FILE@0..19
              OPEN_TAG@0..6 "<?php "
              FUNCTION_DECL@6..18
                FUNCTION_KW@6..14 "function"
                IDENT@15..16 "f"
                PARAM_LIST@16..18
                  L_PAREN@16..17 "("
                  R_PAREN@17..18 ")"
              EMPTY_STMT@18..19
                SEMICOLON@18..19 ";"
            error@18..19 syntax error, unexpected token ";", expecting "{"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Unterminated tokens at EOF
// ---------------------------------------------------------------------------

#[test]
fn unterminated_string_at_eof() {
    check(
        r#"<?php $a = "abc"#,
        expect![[r#"
            SOURCE_FILE@0..15
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..15
                ASSIGN_EXPR@6..15
                  SIMPLE_VAR_EXPR@6..8
                    VARIABLE@6..8 "$a"
                  EQ@9..10 "="
                  INTERPOLATED_STRING_EXPR@11..15
                    DOUBLE_QUOTE@11..12 "\""
                    LITERAL_STRING_PART@12..15
                      STRING_TEXT@12..15 "abc"
            error@15..15 syntax error, unexpected end of file, expecting variable or "${" or "{$"
        "#]],
    );
}

// ---------------------------------------------------------------------------
// Forward-progress worst cases
// ---------------------------------------------------------------------------

#[test]
fn eight_open_parens() {
    check(
        "<?php ((((((((",
        expect![[r#"
            SOURCE_FILE@0..14
              OPEN_TAG@0..6 "<?php "
              EXPR_STMT@6..14
                PAREN_EXPR@6..14
                  L_PAREN@6..7 "("
                  PAREN_EXPR@7..14
                    L_PAREN@7..8 "("
                    PAREN_EXPR@8..14
                      L_PAREN@8..9 "("
                      PAREN_EXPR@9..14
                        L_PAREN@9..10 "("
                        PAREN_EXPR@10..14
                          L_PAREN@10..11 "("
                          PAREN_EXPR@11..14
                            L_PAREN@11..12 "("
                            PAREN_EXPR@12..14
                              L_PAREN@12..13 "("
                              PAREN_EXPR@13..14
                                L_PAREN@13..14 "("
            error@14..14 Unclosed '('
        "#]],
    );
}

/// The diagnostics of one parse, through the harness so the token stream
/// is held lossless on the way; the dump is not rendered, since a
/// 100,000-token `ERROR` node under the cap is a lot of lines for no
/// reading.
fn errors(src: &str) -> Vec<String> {
    parse_lossless(src)
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn deep_paren_nesting_terminates_losslessly() {
    // ~200 levels of `(`: must neither hang nor overflow the stack, and
    // must stay lossless (the harness holds it). The dump is enormous, so
    // assert shape properties instead of snapshotting it.
    let src = format!("<?php {}", "(".repeat(200));
    let dump = parse_fixture(&src);
    assert!(dump.starts_with("SOURCE_FILE@0..206\n"), "root spans input");
    assert!(
        dump.contains("Unclosed '('"),
        "EOF inside the open construct is reported"
    );
    assert!(
        !dump.contains("REQUIRED-VIOLATION"),
        "no required-children violations under deep nesting"
    );
}

#[test]
fn deep_brace_nesting_terminates_losslessly() {
    let src = format!("<?php {}", "{".repeat(200));
    let dump = parse_fixture(&src);
    assert!(dump.starts_with("SOURCE_FILE@0..206\n"), "root spans input");
    // Input ends inside the open construct: the unclosed opener is the
    // report, not a generic one.
    assert!(
        dump.contains("Unclosed '{'"),
        "EOF in open construct is reported"
    );
}

/// The grammar nesting cap: past it the construct is consumed flat, no
/// stack overflows at any input depth, the tree stays lossless and
/// exactly one diagnostic is raised, php's `memory exhausted`, which is
/// what bison's stack answers in php on the same axis
/// (`Zend/tests/bug64660.phpt`). The unit test beside the parser holds
/// the same on a thread sized for the cap; this one runs on the test
/// harness's own thread, so the cap is shown to refuse before the stack
/// runs out.
#[test]
fn nesting_depth_cap() {
    for depth in [600_usize, 10_000, 100_000] {
        let src = format!("<?php $x = {}1{};\n", "(".repeat(depth), ")".repeat(depth));
        assert_eq!(
            errors(&src),
            ["memory exhausted"],
            "exactly one refusal at depth {depth}"
        );
    }

    // Below the cap: silent.
    let src = format!("<?php $x = {}1{};\n", "(".repeat(100), ")".repeat(100));
    assert!(errors(&src).is_empty(), "depth 100 parses clean");

    // Deep blocks too (statement-side recursion).
    let src = format!(
        "<?php {} echo 1; {}\n",
        "{".repeat(5_000),
        "}".repeat(5_000)
    );
    assert_eq!(
        errors(&src),
        ["memory exhausted"],
        "deep blocks hit the cap"
    );
}

/// A chain of one prefix operator is taken flat, so it reaches php's
/// bison stack rather than the nesting cap: `stack_limit_013.phpt`'s 7,676
/// `~` parse clean, as php parses them, and a chain past `YYMAXDEPTH`
/// raises php's one `memory exhausted`.
#[test]
fn a_prefix_chain_reaches_yymaxdepth() {
    for (op, depth) in [("~", 7_676_usize), ("!", 9_000), ("-", 9_000)] {
        let src = format!("<?php $x = [{}1];\n", format!("{op} ").repeat(depth));
        assert!(errors(&src).is_empty(), "{depth} {op} parse clean");
    }
    let src = format!("<?php $x = {}1;\n", "~".repeat(10_001));
    assert_eq!(errors(&src), ["memory exhausted"], "past YYMAXDEPTH");
}

/// The cap bounds grammar recursion and nothing else: the Pratt core
/// wraps a left-nested chain iteratively, so `1+1+…+1` is grammar depth 1
/// and tree depth n, and the parser passes it whole, as php's bison does.
/// What a deep tree costs downstream is the compiler's to refuse, where
/// php refuses it (`zend_compile_expr` under the stack limit), not the
/// parser's.
#[test]
fn left_nested_chains_are_not_grammar_depth() {
    // Each shape is O(1) grammar depth and O(n) tree depth.
    for shape in [
        |n: usize| format!("<?php $y = 1{};", " || 1".repeat(n)),
        |n: usize| format!("<?php $y = 1{};", "+1".repeat(n)),
        |n: usize| format!("<?php $y = $a{};", "[0]".repeat(n)),
        |n: usize| format!("<?php $y = \"a\"{};", " . \"a\"".repeat(n)),
        |n: usize| format!("<?php $y = $c{}->e();", "->v(1)".repeat(n)),
    ] {
        // The dump of a 20,000-deep tree indents each line by its depth,
        // which is a lot of whitespace for no reading.
        let src = shape(20_000);
        let parsed = parse_lossless(&src);
        assert!(
            parsed.diagnostics.is_empty() && parsed.missing.is_empty(),
            "a 20,000-link chain is ordinary code for the parser: {} {:?}",
            &src[..24],
            parsed.diagnostics
        );
    }
}

/// A left-nested chain parses in time linear in its length. The Pratt
/// core opens every link of `1+1+…` at one `Start`, after one token, and
/// the builder read the kind that encloses a node by looking down the
/// open frames past every one that opened after the same token, so each
/// link cost the chain's length and 200,000 links took minutes in a debug
/// build. Held by growth, not by a clock: eight times the links costs
/// about eight times the time linearly and sixty-four times
/// quadratically, measured at 61× before the fix, and the bound sits
/// between. The time is the thread's CPU time, best of three: a wall
/// clock on a loaded machine charges the parse for the time it waited
/// to be scheduled, which a 40,000-link parse waits more often than a
/// 5,000-link one fits in between (#2205).
#[test]
fn a_left_nested_chain_parses_in_linear_time() {
    use nix::time::{ClockId, clock_gettime};
    use std::time::Duration;
    let cpu = || -> Duration {
        clock_gettime(ClockId::CLOCK_THREAD_CPUTIME_ID)
            .expect("the thread's CPU clock reads")
            .into()
    };
    let best = |n: usize| -> Duration {
        let src = format!("<?php $y = 1{};", "+1".repeat(n));
        (0..3)
            .map(|_| {
                let at = cpu();
                let arena = php_syntax::Arena::new();
                let parsed =
                    php_syntax::parse(&arena, src.as_bytes(), &php_syntax::Options::default());
                let took = cpu().saturating_sub(at);
                assert!(parsed.diagnostics.is_empty() && parsed.missing.is_empty());
                took
            })
            .min()
            .unwrap_or_default()
    };
    let (short, long) = (best(5_000), best(40_000));
    let ratio = long.as_secs_f64() / short.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 24.0,
        "8× the links took {ratio:.1}× the thread's CPU time ({short:?} for 5,000, {long:?} for 40,000); linear is 8×, quadratic 64×"
    );
}

/// `$$…$x` self-recurses in `variable_variable`, the one self-recursion in
/// `grammar/` outside the Pratt core and the statement loop. php 8.5
/// refuses the shape on the same axis (`Parse error: memory exhausted`,
/// near 10,000 `$`); the cap refuses it earlier, with php's text, and the
/// flat recovery must not swallow the statement's `;` and cascade.
#[test]
fn variable_variable_depth_is_guarded() {
    for n in [600_usize, 10_000, 100_000] {
        let src = format!("<?php $y = {}x;", "$".repeat(n));
        assert_eq!(
            errors(&src),
            ["memory exhausted"],
            "exactly one refusal at {n} dollars"
        );
    }

    // Under the cap: still an ordinary variable-variable.
    assert!(
        errors("<?php $y = $$$$x;").is_empty(),
        "`$$$$x` is legal PHP"
    );
}

/// `as` followed by garbage once built a `USE_ALIAS` without its required
/// name (schema convention A2). The stray `as` is an `ERROR` instead, and
/// the tree keeps every promise: the harness prints a broken one as
/// `REQUIRED-VIOLATION:`.
#[test]
fn use_alias_without_name_is_error() {
    for src in [
        "<?php use A\\B as ;",
        "<?php use A\\B as + 1;",
        "<?php use A\\B as \x0E\x7F; use C as D;",
    ] {
        let dump = parse_fixture(src);
        assert!(
            !dump.contains("REQUIRED-VIOLATION"),
            "every required child supplied for {src:?}:\n{dump}"
        );
    }
}
