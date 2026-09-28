//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

/// What the engine wrote and what ended it, for one source, under an
/// engine of this extension alone.
fn served(source: &str) -> (String, Option<String>) {
    let engine = Engine::builder()
        .extensions(&[php_ext_standard::extension()])
        .build();
    let served = engine.serve_capture(source, Capture::default());
    (
        String::from_utf8_lossy(&served.stdout).into_owned(),
        served.fatal,
    )
}

#[test]
fn the_extension_is_loaded() {
    let (stdout, _) = served("<?php var_dump(extension_loaded('standard'));");
    assert_eq!(stdout, "bool(true)\n");
}

/// `str_repeat` reaches the engine's table through the bundle's
/// declaration, and a 3 MB string is one value it hands back whole.
#[test]
fn str_repeat_is_declared_and_builds_a_string_past_two_mebibytes() {
    let (stdout, fatal) = served("<?php echo str_repeat('ab', 2), str_repeat('x', 3000000);");
    assert_eq!(fatal, None);
    assert!(stdout.starts_with("ababx"), "{}", &stdout[..8]);
    assert_eq!(stdout.len(), 4 + 3_000_000);
}

/// php's own text, from the pinned oracle, held here without it; the
/// trace the first of these prints, arguments and all, is the witness
/// beside `str_repeat`:
///
/// ```text
/// ValueError: str_repeat(): Argument #2 ($times) must be greater than or equal to 0
/// ArgumentCountError: str_repeat() expects exactly 2 arguments, 1 given
/// ArgumentCountError: str_repeat() expects exactly 2 arguments, 3 given
/// ```
///
/// The argument's name in the `ValueError` is #175's; what is held here
/// is the class, the position and the tail.
#[test]
fn the_count_below_zero_and_the_arity_are_php_s() {
    for (source, class, text) in [
        ("<?php str_repeat('x', -1);", "ValueError", "Argument #2"),
        (
            "<?php str_repeat('x', -1);",
            "ValueError",
            "must be greater than or equal to 0",
        ),
        (
            "<?php str_repeat('x');",
            "ArgumentCountError",
            "str_repeat() expects exactly 2 arguments, 1 given",
        ),
        (
            "<?php str_repeat('x', 1, 2);",
            "ArgumentCountError",
            "str_repeat() expects exactly 2 arguments, 3 given",
        ),
    ] {
        let (_, fatal) = served(source);
        let fatal = fatal.unwrap_or_default();
        assert!(
            fatal.contains(class) && fatal.contains(text),
            "{source} came to {fatal:?}"
        );
    }
}

/// `zend_string_safe_alloc`'s guard, before a byte is asked for: php's
/// fatal for a product that overflows, with the header's 32 bytes in the
/// text as php prints them.
#[test]
fn a_product_that_overflows_is_php_s_fatal_and_no_allocation() {
    let (_, fatal) = served("<?php str_repeat('xxx', 9223372036854775807);");
    let fatal = fatal.unwrap_or_default();
    assert!(
        fatal.contains(
            "Possible integer overflow in memory allocation (3 * 9223372036854775807 + 32)"
        ),
        "{fatal:?}"
    );
}

/// The walks php leaves unguarded, which recurse in php until the
/// thread's stack runs out and fault, are held to the thread's floor
/// here (`php_ext::raising::call_stack_end`): each over a nest deeper
/// than the test harness's 2 MiB thread is the catchable `Error`, and
/// the request goes on to the next.
#[test]
fn an_unguarded_walk_deeper_than_the_thread_is_refused_at_its_floor() {
    let source = r#"<?php
$a = []; for ($i = 0; $i < 200000; $i++) { $a = ['k' => $a]; }
$b = $a;
$walks = [
    fn () => print_r($a, true),
    fn () => var_export($a, true),
    function () use ($a) { ob_start(); try { debug_zval_dump($a); } finally { ob_end_clean(); } },
    function () use ($a) { $c = $a; array_walk_recursive($c, fn () => null); },
    fn () => array_merge_recursive($a, $b),
    fn () => array_replace_recursive($a, $b),
    fn () => count($a, COUNT_RECURSIVE),
    fn () => compact([$a]),
    fn () => unserialize(str_repeat('a:1:{i:0;', 200000) . 'i:0;' . str_repeat('}', 200000), ['max_depth' => 0]),
];
foreach ($walks as $walk) {
    try { $walk(); echo "whole\n"; } catch (Error $e) { echo strstr($e->getMessage(), ' of ', true), "\n"; }
}
echo 'done';
"#;
    let (stdout, fatal) = served(source);
    assert_eq!(fatal, None);
    assert_eq!(
        stdout,
        format!("{}done", "Maximum call stack size\n".repeat(9))
    );
}
