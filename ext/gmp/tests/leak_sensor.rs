//! The request leak sensor over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_gmp::extension()])
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// `GMP::__unserialize` writes each member it is handed, under an
/// integer key and under a string one, with one owner of the value the
/// table's: the property sinks take an owner, and the native takes the
/// one it hands them.
#[test]
fn the_members_gmp_unserializes_are_the_table_s() {
    let stdout = armed(
        "<?php $a = 'a'; $g = gmp_init(1); @$g->__unserialize(['5', [7 => $a . $a, 'p' => $a . 'x']]); echo gmp_strval($g); foreach ($g as $k => $v) echo $k, $v; echo \"\\n\";",
    );
    assert_eq!(stdout, "57aapax\n");
}
