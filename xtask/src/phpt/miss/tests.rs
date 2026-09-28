//! A miss decoded from an expectation and what ran: the first lines where
//! the two part, and the signature they come to.

use super::*;

fn format(expected: &str) -> Expectation {
    Expectation::Format(expected.to_owned())
}

fn exact(expected: &str) -> Expectation {
    Expectation::Exact(expected.as_bytes().to_vec())
}

/// A `%s` that stands for a path is no difference: the first line the
/// engine wrote of its own is the one after it, where the textual diff
/// would have named the path's line.
#[test]
fn a_line_an_expectf_wildcard_matches_is_no_difference() {
    let miss = Miss::of(
        &format("Warning: Undefined variable $x in %s on line %d\nNULL\nint(1)"),
        b"Warning: Undefined variable $x in /tmp/x.php on line 3\nNULL\nint(2)",
    );
    assert_eq!(miss.expected.as_deref(), Some("int(1)"));
    assert_eq!(miss.actual.as_deref(), Some("int(2)"));
    assert_eq!(miss.signature, Signature::Differs);
    assert_eq!(miss.signature.to_string(), "output differs");
}

/// The engine's first line of its own names what it lacks, whichever of
/// the four php words it in.
#[test]
fn a_missing_symbol_is_named() {
    let named = |line: &str| {
        Miss::of(&exact("ok"), line.as_bytes())
            .signature
            .to_string()
    };
    assert_eq!(
        named("Fatal error: Uncaught Error: Call to undefined function array_any() in /t/a.php:3"),
        "undefined function array_any"
    );
    assert_eq!(
        named("Fatal error: Uncaught Error: Call to undefined method Foo\\Bar::baz() in x"),
        "undefined method Foo\\Bar::baz"
    );
    assert_eq!(
        named("Fatal error: Uncaught Error: Interface \"Stringable2\" not found in x"),
        "Interface \"Stringable2\" not found"
    );
    assert_eq!(
        named("Fatal error: Uncaught Error: Undefined constant \"PHP_FOO\" in /t/a.php:2"),
        "undefined constant PHP_FOO"
    );
}

/// A diagnostic that names nothing missing is masked of its strings, its
/// paths and its numbers, so two tests that part at it group together.
#[test]
fn a_diagnostic_is_masked() {
    let one = Miss::of(
        &exact("done"),
        b"Warning: str_repeat(): Argument #2 ($times) must be greater than or equal to 0 in /tmp/phpt-tree/a.php on line 12\ndone",
    );
    let two = Miss::of(
        &exact("done"),
        b"Warning: str_repeat(): Argument #2 ($times) must be greater than or equal to 0 in /tmp/phpt-tree/b.php on line 7\ndone",
    );
    assert_eq!(one.signature, two.signature);
    assert_eq!(
        one.signature.to_string(),
        "Warning: str_repeat(): Argument #_ ($times) must be greater than or equal to _ in _ on line _"
    );
    assert_eq!(one.expected, None);
    let long = format!("Notice: {}", "x".repeat(200));
    let Signature::Diagnostic(masked) = Miss::of(&exact(""), long.as_bytes()).signature else {
        panic!("a notice is a diagnostic");
    };
    assert_eq!(masked.chars().count(), MASKED);
}

/// Where the engine wrote nothing the test does not expect, the cause is
/// the first diagnostic php wrote and the engine did not, its wildcards
/// masked as what they stand for is.
#[test]
fn what_php_alone_wrote_is_php_only() {
    let miss = Miss::of(
        &format(
            "start\nFatal error: Cannot redeclare function f() (previously declared in %s:%d) in %s on line %d",
        ),
        b"start",
    );
    assert_eq!(
        miss.expected.as_deref(),
        Some(
            "Fatal error: Cannot redeclare function f() (previously declared in %s:%d) in %s on line %d"
        )
    );
    assert_eq!(miss.actual, None);
    assert_eq!(
        miss.signature.to_string(),
        "php only: Fatal error: Cannot redeclare function f() (previously declared in _:_) in _ on line _"
    );
    let plain = Miss::of(&exact("a\nb\nc"), b"a\nc");
    assert_eq!(plain.expected.as_deref(), Some("b"));
    assert_eq!(plain.signature, Signature::PhpOnly(None));
    assert_eq!(plain.signature.to_string(), "php only: no diagnostic");
}

/// A line that may stand for many takes the lines after it that nothing
/// else stands for, so a trace php elides with `%A` is no difference.
#[test]
fn a_line_that_spans_takes_the_lines_after_it() {
    let miss = Miss::of(
        &format("Fatal error: Uncaught Exception: boom in %s:%d\nStack trace:\n%A\ndone"),
        b"Fatal error: Uncaught Exception: boom in /t/a.php:3\nStack trace:\n#0 {main}\n  thrown in /t/a.php on line 3",
    );
    assert_eq!(miss.actual, None);
    assert_eq!(miss.expected.as_deref(), Some("done"));
    assert_eq!(miss.signature, Signature::PhpOnly(None));
}

/// An engine line inserted before what both wrote is the first of its
/// own, and the expected line it displaced is none: nothing php wrote is
/// missing.
#[test]
fn an_inserted_line_is_the_engines_own() {
    let miss = Miss::of(
        &exact("a\nb"),
        b"a\nDeprecated: Implicit conversion from float 1.5 to int loses precision in /t/x.php on line 4\nb",
    );
    assert_eq!(miss.expected, None);
    assert_eq!(
        miss.signature.to_string(),
        "Deprecated: Implicit conversion from float _._ to int loses precision in _ on line _"
    );
}

/// The misses that keep no lines: the compiler's refusal by its own
/// sentence, the recipe's limit, and a test the runner could not read.
#[test]
fn a_miss_without_lines_keeps_its_signature() {
    let unread = Miss::unread("a `declare(encoding)` this compiler cannot read yet");
    assert_eq!(
        unread.signature.to_string(),
        "a `declare(encoding)` this compiler cannot read yet"
    );
    assert_eq!((unread.expected, unread.actual), (None, None));
    assert_eq!(Miss::past_limit().signature, Signature::PastLimit);
    assert_eq!(
        Miss::bork("a file already stands where the test's program goes, /t/Zend/tests/a.php")
            .signature
            .to_string(),
        "bork: a file already stands where the test's program goes, _"
    );
}

/// A kept miss reads back as it was written, and one without lines
/// writes none.
#[test]
fn a_miss_round_trips_through_the_kept_run() {
    let miss = Miss::of(
        &exact("ok"),
        b"Fatal error: Uncaught Error: Class \"Foo\" not found",
    );
    let text = serde_json::to_string(&miss).unwrap();
    assert_eq!(serde_json::from_str::<Miss>(&text).unwrap(), miss);
    assert_eq!(
        serde_json::to_string(&Miss::past_limit()).unwrap(),
        r#"{"signature":"past_limit"}"#
    );
}

/// A blank line is passed over for the first line after it that says
/// something, on either side, and is the first only where nothing else
/// differs.
#[test]
fn a_blank_line_is_the_first_only_where_nothing_else_differs() {
    let miss = Miss::of(
        &format("#1 {main}\n\nNext TypeError: bad in %s:%d\nthrown"),
        b"#1 {main}\nthrown",
    );
    assert_eq!(
        miss.expected.as_deref(),
        Some("Next TypeError: bad in %s:%d")
    );
    assert_eq!(miss.signature, Signature::PhpOnly(None));
    let blank = Miss::of(&exact("a\nb"), b"a\n\nb");
    assert_eq!(blank.actual.as_deref(), Some(""));
    assert_eq!(blank.signature, Signature::Differs);
}
