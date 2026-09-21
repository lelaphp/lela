//! A node's `line` against what the pinned php printed for the same
//! program, for the programs the linked engine cannot yet serve as a
//! witness (design §4.9, ledger 27; #601, #603). Each program is written
//! as the witness recipe runs one, `<?php` on line 1 and the code from
//! line 2, and the number asserted is the one the oracle printed on
//! #601, probed again at 8.5.9 on 2026-09-21; `line.rs`'s own witnesses
//! hold the two the engine serves through the oracle.

use php_syntax::tree::Element;
use php_syntax::{Options, SyntaxKind, parse};

/// The line of every node of `kind` in `src`, in source order.
fn lines(src: &str, kind: SyntaxKind) -> Vec<u32> {
    let parsed = parse(src.as_bytes(), &Options::default());
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

/// `ASSIGN` is emitted at the target's line: `Warning: Undefined
/// variable $b in %s on line 2`.
#[test]
fn an_assignment_is_its_targets_line() {
    assert_eq!(line("<?php\n$a\n=\n$b;\n", SyntaxKind::ASSIGN_EXPR), 2);
}

/// #601 pin 1, under `declare(ticks=1)`: the lines printed are 3, 5,
/// 10, 15 and 21. An empty block is its `{`'s line,
/// an `else` arm its block's, a `finally` clause its block's, and an
/// `else` arm over a statement that statement's.
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

/// The same three lines as the execution timer finds them, on the jump
/// back of a `for` with no condition and no step, which is emitted at
/// the line the body left: `Maximum execution time of 1 second exceeded
/// in %s on line 3` for each (probed 8.5.9). These are the programs a
/// witness in `line.rs` runs once the engine's compiler leaves the line
/// where php leaves it.
#[test]
fn the_line_a_for_loops_jump_back_is_left_at() {
    let block = "<?php\nset_time_limit(1); for (;;)\n{ /*\n\n*/\n}\n";
    assert_eq!(lines(block, SyntaxKind::BLOCK), [3]);
    let finally = "<?php\nset_time_limit(1); for (;;) try {} finally\n{ /*\n\n*/\n}\n";
    assert_eq!(line(finally, SyntaxKind::FINALLY_CLAUSE), 3);
    let arm = "<?php\nset_time_limit(1); $x = 0; for (;;) if ($x) ; else\n{ /*\n\n*/\n}\n";
    assert_eq!(line(arm, SyntaxKind::ELSE_CLAUSE), 3);
}

/// #601 pins 2 and 3: `Switch statements may only contain one default
/// clause in %s on line 2`, and `Case statements followed by a semicolon
/// (;) are deprecated … on line 4`. A `default`'s line is its empty
/// statement list's, the `:`'s or the `;`'s, comments on both sides
/// uncounted.
#[test]
fn a_switch_arms_empty_body_is_its_separators_line() {
    let two = "<?php\nswitch (1) { default: default:\n/*\n\n*/ }\n";
    assert_eq!(lines(two, SyntaxKind::SWITCH_ARM), [2, 2]);
    let semi = "<?php\nswitch (1) { default /*\n\n*/ ; /*\n\n*/ }\n";
    assert_eq!(line(semi, SyntaxKind::SWITCH_ARM), 4);
}

/// #601 pins 4 and 5: `Array to string conversion in %s on line 4`, and
/// `could not be passed by reference in %s:5`. An empty array is its
/// `]`'s line, the lookahead its rule reduced on.
#[test]
fn an_empty_array_is_its_closing_brackets_line() {
    assert_eq!(
        line("<?php\necho [ /*\n\n*/ ];\n", SyntaxKind::ARRAY_EXPR),
        4
    );
    assert_eq!(
        line(
            "<?php\nfunction f(&$x) {}\nf([ /*\n\n*/ ]);\n",
            SyntaxKind::ARRAY_EXPR
        ),
        5
    );
}

/// #601 pins 6, 7 and 8: `must return a value in %s on line 3`, `… on
/// line 2` for `return ?>` with a newline, whose `;` ends on the `?>`'s
/// line, and `'break' not in the 'loop' or 'switch' context in %s on
/// line 4`. A bare `return` or `break` is its `;`'s line.
#[test]
fn a_bare_return_or_break_is_its_semicolons_line() {
    assert_eq!(
        line(
            "<?php\nfunction f(): int { return\n; /*\n\n*/ }\n",
            SyntaxKind::RETURN_STMT
        ),
        3
    );
    assert_eq!(
        line(
            "<?php\nfunction f(): int { return ?>\n<?php }\n",
            SyntaxKind::RETURN_STMT
        ),
        2
    );
    assert_eq!(
        line("<?php\nbreak /*\n\n*/ ; /*\n\n*/\n", SyntaxKind::BREAK_STMT),
        4
    );
}

/// #601 pin 9 and the amendment's pin: `The "yield" expression can only
/// be used inside a function in %s on line 4`, and `… on line 5` for a
/// `yield $k => $v` written over four lines, the value's line and not
/// the key's, since `ZEND_AST_YIELD` is `(value, key)`.
#[test]
fn a_yield_is_the_lookaheads_line_bare_and_its_values_line_keyed() {
    assert_eq!(line("<?php\nyield /*\n\n*/ ;\n", SyntaxKind::YIELD_EXPR), 4);
    assert_eq!(
        line("<?php\nyield\n$k\n=>\n$v;\n", SyntaxKind::YIELD_EXPR),
        5
    );
}

/// #601 pins 10 and 11: `exit(): Argument #1 ($status) must be of type
/// string|int, array given in %s:4`, and `Too few arguments to function
/// readonly(), 0 passed in %s on line 5`. Both name zvals are made after
/// the `)`.
#[test]
fn exit_and_readonly_as_calls_are_their_closing_parens_line() {
    assert_eq!(line("<?php\nexit(\n[]\n);\n", SyntaxKind::EXIT_EXPR), 4);
    assert_eq!(
        line(
            "<?php\nfunction readonly($x) {}\nreadonly(\n\n);\n",
            SyntaxKind::CALL_EXPR
        ),
        5
    );
}

/// #601 pin 12, under `declare(ticks=1)`: 5 is printed for the empty
/// heredoc, whose line is its closing label's, and 7 for the one with a
/// body, whose line is the body's first.
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

/// #601 pin 13: `2` then `8`. `__LINE__` is its own line as a statement
/// and the `)`'s as an argument, where the keyword may be a named
/// argument's label and the parser reads past it before it reduces.
#[test]
fn a_magic_constant_is_the_lookaheads_line_in_argument_position() {
    let src = "<?php\n\
        echo __LINE__ /*\n\
        \n\
        */ ;\n\
        function f($x) { return $x; }\n\
        echo f(__LINE__ /*\n\
        \n\
        */ );\n";
    assert_eq!(lines(src, SyntaxKind::MAGIC_CONST_EXPR), [2, 8]);
}

/// #601 pins 14 and 15: the constructor's `ArgumentCountError` names
/// `on line 2` for `new static` written over three lines, and the
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

/// #601 pin 16: `Too few arguments to function g(), 0 passed in %s on
/// line 6`. A call through a variable is the `(`'s line.
#[test]
fn a_call_through_a_variable_is_its_opening_parens_line() {
    let src = "<?php\nfunction g($x) {}\n$f = \"g\";\n$f /*\n\n*/ (\n);\n";
    assert_eq!(line(src, SyntaxKind::CALL_EXPR), 6);
}

/// #601 pin 17, and one more probed 8.5.9: `2`, `9`, then `2` again for
/// `static function` in argument position, whose `static` took the
/// lookahead so the `function` after it reduces on its own line.
#[test]
fn a_closures_line_is_its_keywords_or_the_next_tokens_in_argument_position() {
    let src = "<?php\n\
        $c = function /*\n\
        \n\
        */ () {};\n\
        echo (new ReflectionFunction($c))->getStartLine(), \"\\n\";\n\
        function f($c) { return $c; }\n\
        $d = f(function /*\n\
        \n\
        */ () {});\n\
        echo (new ReflectionFunction($d))->getStartLine(), \"\\n\";\n\
        $e = f(static function /*\n\
        \n\
        */ () {});\n\
        echo (new ReflectionFunction($e))->getStartLine(), \"\\n\";\n";
    assert_eq!(lines(src, SyntaxKind::CLOSURE_EXPR), [2, 9, 11]);
}
