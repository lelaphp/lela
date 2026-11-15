//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

mod support;

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[php_ext_zlib::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('zlib'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

/// `zlib.output_compression` turned on by `ini_set` asks whether its
/// handler is on the stack as `php_output_handler_started` does, by
/// reading the stack, and keeps nothing: armed, the request ends with no
/// block of its heap held.
#[cfg(debug_assertions)]
#[test]
fn turning_output_compression_on_keeps_nothing_of_the_output_layer_s() {
    let engine = support::engine();
    let served = engine.serve_capture(
        "<?php var_dump(ini_set('zlib.output_compression', '1'), ini_get('zlib.output_compression'));",
        Capture::default(),
    );
    served.assert_clean("string(1) \"0\"\nstring(1) \"1\"\n");
}
