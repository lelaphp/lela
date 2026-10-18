//! A node's `line` against what the pinned php printed for the same
//! program, for the programs the linked engine cannot serve as a witness
//! (design §4.9, ledger 27; #601, #603, #606). Each program is written as
//! the witness recipe runs one, `<?php` on line 1 and the code from line
//! 2, and the number asserted is the one the oracle printed on #601,
//! probed again at 8.5.9 on 2026-09-22. What the engine now serves is a
//! `php` fence beside the rule it exercises, in `src/tree/line.rs` and,
//! for the jump back of a `for`, in `php_compile`'s `emit`; what is left
//! here is left for one of four reasons, each named on the test that
//! carries it and each measured under the linked engine:
//!
//! 1. **`declare(ticks=1)`**, which the two programs that observe a line
//!    through `register_tick_function` need: the engine refuses the
//!    statement, "a `declare(ticks)` of a count this compiler cannot read
//!    yet", and #499 is where that work stands.
//! 2. **The line the execution timer names**, for the third of #601's
//!    three `for (;;)` programs: its loop holds a taken forward `JMPZ`
//!    beside the jump back, and php tests the interrupt after every
//!    jump that sets an opline where this engine tests it at a
//!    back-edge alone, so php names line 3 on ten runs of twelve and
//!    line 2 on the other two while the engine names line 2 on all
//!    twelve. Design §5.8 and ledger 28 say the line such a timeout
//!    names is not held. The op array itself is held to php's:
//!    `xtask compile-identity --file` reads all three programs as
//!    byte-identical with the oracle's dump.
//! 3. **A `yield`**, which the compiler refuses, "a `yield` this compiler
//!    cannot read yet"; #564 is where that work stands.
//! 4. **`static` as a class reference**, which it refuses too, "a class
//!    reference of the scope this compiler cannot read yet".

use php_syntax::tree::Element;
use php_syntax::{Options, SyntaxKind, parse};

/// The line of every node of `kind` in `src`, in source order.
fn lines(src: &str, kind: SyntaxKind) -> Vec<u32> {
    let arena = php_syntax::Arena::new();
    let parsed = parse(&arena, src.as_bytes(), &Options::default());
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    assert!(parsed.missing.is_empty(), "{:?}", parsed.missing);
    parsed
        .tree
        .elements()
        .iter()
        .filter_map(|e| match *e {
            Element::Node { kind: k, line, .. } if k == kind => Some(line),
            _ => None,
        })
        .collect()
}

/// The one node of `kind` in `src`.
fn line(src: &str, kind: SyntaxKind) -> u32 {
    let all = lines(src, kind);
    assert_eq!(all.len(), 1, "{kind:?} in {src:?}: {all:?}");
    all[0]
}

/// #601 pin 1, under `declare(ticks=1)` (reason 1): the lines printed are
/// 3, 5, 10, 15 and 21. An empty block is its `{`'s line, an `else` arm
/// its block's, a `finally` clause its block's, and an `else` arm over a
/// statement that statement's.
#[test]
fn an_empty_block_an_else_arm_and_a_finally_clause_under_ticks() {
    let src = "<?php\n\
        declare(ticks=1);\n\
        register_tick_function(function () { echo \"tick at \", (new Exception)->getTrace()[0]['line'], \"\\n\"; });\n\
        if (1)\n\
        { /*\n\
        \n\
        */\n\
        }\n\
        if (0) ; else\n\
        { /*\n\
        \n\
        */\n\
        }\n\
        try { throw new Exception; } catch (Exception $e) {} finally\n\
        { /*\n\
        \n\
        */\n\
        }\n\
        if (0) ; else\n\
        \n\
        \x20 echo \"\";\n";
    // The closure's body, the if's block, the else's, the try's, the
    // catch's and the finally's.
    assert_eq!(lines(src, SyntaxKind::BLOCK), [3, 5, 10, 14, 14, 15]);
    assert_eq!(lines(src, SyntaxKind::ELSE_CLAUSE), [10, 21]);
    assert_eq!(line(src, SyntaxKind::FINALLY_CLAUSE), 15);
}

/// The third of the three programs that read the line a `for` loop's jump
/// back is left at, which is the `else` arm's block's (reason 2). The
/// other two are witnesses beside `php_compile`'s `for`; this one no
/// lane pins, php naming the opline the interrupt fell on and this
/// engine the one the jump back lands on, which design §5.8 and ledger
/// 28 leave unheld. What the tree holds is the `else` arm's line.
#[test]
fn the_line_a_for_loops_jump_back_is_left_at_over_an_else_arm() {
    let arm = "<?php\nset_time_limit(1); $x = 0; for (;;) if ($x) ; else\n{ /*\n\n*/\n}\n";
    assert_eq!(line(arm, SyntaxKind::ELSE_CLAUSE), 3);
}

/// #601 pin 9 and the amendment's pin (reason 3): `The "yield" expression
/// can only be used inside a function in %s on line 4`, and `… on line 5`
/// for a `yield $k => $v` written over four lines, the value's line and
/// not the key's, since `ZEND_AST_YIELD` is `(value, key)`.
#[test]
fn a_yield_is_the_lookaheads_line_bare_and_its_values_line_keyed() {
    assert_eq!(line("<?php\nyield /*\n\n*/ ;\n", SyntaxKind::YIELD_EXPR), 4);
    assert_eq!(
        line("<?php\nyield\n$k\n=>\n$v;\n", SyntaxKind::YIELD_EXPR),
        5
    );
}

/// #601 pin 12, under `declare(ticks=1)` (reason 1): 5 is printed for the
/// empty heredoc, whose line is its closing label's, and 7 for the one
/// with a body, whose line is the body's first.
#[test]
fn a_heredoc_is_its_bodys_first_line_or_its_closing_labels() {
    let src = "<?php\n\
        declare(ticks=1);\n\
        register_tick_function(function () { echo \"tick at \", (new Exception)->getTrace()[0][\"line\"], \"\\n\"; });\n\
        echo <<<EOT\n\
        EOT;\n\
        echo <<<EOT\n\
        \n\
        x\n\
        EOT;\n";
    assert_eq!(lines(src, SyntaxKind::HEREDOC_EXPR), [5, 7]);
}

/// #601 pins 14 and 15 (reason 4): the constructor's `ArgumentCountError`
/// names `on line 2` for `new static` written over three lines, and the
/// method's names `on line 4` for `static::f()` written the same way:
/// `class_name: T_STATIC` reduces before the lookahead after `new` and
/// after it where a statement may begin.
#[test]
fn static_as_a_class_name_is_its_own_line_or_the_double_colons() {
    let new = "<?php\nclass A { function __construct($x) {} static function m() { new static /*\n\n*/ ; } }\nA::m();\n";
    assert_eq!(line(new, SyntaxKind::NEW_EXPR), 2);
    let scoped = "<?php\nclass A { static function f($x) {} static function m() { static /*\n\n*/ ::f(); } }\nA::m();\n";
    assert_eq!(lines(scoped, SyntaxKind::SCOPED_ACCESS_EXPR), [4, 5]);
}
