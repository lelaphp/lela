//! The request leak sensor over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_ffi::extension()])
        .ini("ffi.enable", "1")
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// A type `FFI::new` takes from a `FFI\CType` it does not solely own is
/// remembered in `FFI_G(weak_types)` until the request ends, where
/// `RSHUTDOWN` destroys the table and frees what it remembered.
#[test]
fn a_remembered_type_is_freed_at_the_request_s_end() {
    let stdout = armed(
        r#"<?php
$ffi = FFI::cdef(); $t = $ffi->type("int[3]");
$x = $ffi->new($t); $y = $ffi->new($t); $x[2] = 7;
echo $x[2], ' ', $y[0], "\n";
"#,
    );
    assert_eq!(stdout, "7 0\n");
}
