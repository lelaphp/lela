//! The request leak sensor over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

mod support;

use php_ext::test::Capture;

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = support::engine();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// `mb_ereg_replace_callback` appends what the callback returns as its
/// string, `zval_try_get_string`, and gives that string up after,
/// whether the return was a string or a number converted.
#[test]
fn a_replacement_callback_s_answer_is_given_up() {
    let stdout = armed(
        r#"<?php
var_dump(mb_ereg_replace_callback("(b)", fn($m) => "<$m[1]>", "abcb"), mb_ereg_replace_callback("\\d", fn($m) => $m[0] * 2.5, "a1b2"));
"#,
    );
    assert_eq!(stdout, "string(8) \"a<b>c<b>\"\nstring(6) \"a2.5b5\"\n");
}

/// libmbfl on a request's path allocates on the request's heap, under the
/// lend `text::on_heap` makes, and frees there (`shim/zend.h`'s `emalloc`
/// and `efree`, both read off `php_heap_lend_standing()`): the armed
/// request ends with nothing of a conversion's buffer held, and under a
/// memory limit the buffer's growth is refused as php 8.5.12 refuses it
/// (`mbfl_encoding.h:171`).
#[test]
fn a_conversion_s_buffer_is_the_request_s_and_is_given_back() {
    let stdout = armed(
        r"<?php
$s = 'a'; for ($i = 0; $i < 17; $i++) { $s .= $s; }
var_dump(strlen(mb_convert_encoding($s, 'UTF-32', 'ISO-8859-1')));
",
    );
    assert_eq!(stdout, "int(524288)\n");
    let engine = php_ext::test::Engine::builder()
        .extensions(support::extensions())
        .ini("memory_limit", "4M")
        .build();
    let source = r#"<?php
$s = 'a'; for ($i = 0; $i < 20; $i++) { $s .= $s; }
mb_convert_encoding($s, 'UTF-32', 'ISO-8859-1');
echo "not reached\n";
"#;
    let stdout = engine
        .serve_capture(source, Capture::default())
        .stdout_text();
    assert!(
        stdout.contains("Allowed memory size of 4194304 bytes exhausted")
            && !stdout.contains("not reached"),
        "{stdout}"
    );
}

/// The engine's multibyte scanner converts a source file with no
/// request's heap lent: libmbfl allocates on the system allocator there
/// and frees there, by the same predicate, so the file compiles and runs
/// and the armed request ends clean.
#[test]
fn the_scanner_s_conversion_outside_a_lend_is_the_system_s() {
    let engine = php_ext::test::Engine::builder()
        .extensions(support::extensions())
        .ini("zend.multibyte", "1")
        .build();
    // "é" in UTF-8, two bytes, read as ISO-8859-1 and converted to the
    // internal UTF-8: four.
    let source = "<?php declare(encoding='ISO-8859-1'); var_dump(strlen('é'));";
    let served = engine.serve_capture(source, Capture::default());
    assert_eq!(served.stdout_text(), "int(4)\n");
}
