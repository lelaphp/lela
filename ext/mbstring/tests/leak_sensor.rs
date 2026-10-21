//! The request leak sensor (#1992) over this extension's natives: each
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
/// whether the return was a string or a number converted (#2100).
#[test]
fn a_replacement_callback_s_answer_is_given_up() {
    let stdout = armed(
        r#"<?php
var_dump(mb_ereg_replace_callback("(b)", fn($m) => "<$m[1]>", "abcb"), mb_ereg_replace_callback("\\d", fn($m) => $m[0] * 2.5, "a1b2"));
"#,
    );
    assert_eq!(stdout, "string(8) \"a<b>c<b>\"\nstring(6) \"a2.5b5\"\n");
}
