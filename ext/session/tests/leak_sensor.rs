//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_session::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// A cookie parameter given twice in `session_set_cookie_params`'
/// array keeps the last, and the first's string stays held to the end
/// of the request, as php overwrites it unreleased and
/// `memory_get_usage()` reads it, where the request gives it up (#2100).
#[test]
fn a_cookie_parameter_given_twice_is_given_up_at_the_request_s_end() {
    let stdout = armed(
        r#"<?php
session_set_cookie_params(['path' => 123457]);
$m = memory_get_usage(); session_set_cookie_params(['path' => 123456, 'PATH' => 654321]); echo memory_get_usage() - $m, "\n";
"#,
    );
    assert_eq!(stdout, "32\n");
}
