//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_date::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// `DateInterval::__unserialize` reads `days` as `DATE_A64I` reads it,
/// `strtoll` over the value's string, and gives the string up after
/// (#2100).
#[test]
fn the_string_days_is_read_through_is_given_up() {
    let stdout = armed(
        r#"<?php
$i = new DateInterval('P1D'); $i->__unserialize(['days' => 10]); echo $i->days, "\n";
"#,
    );
    assert_eq!(stdout, "10\n");
}
