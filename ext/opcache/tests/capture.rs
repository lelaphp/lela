//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

/// php registers the extension as `Zend OPcache`, and the crate's name is
/// no name php answers to.
#[test]
fn the_extension_is_loaded_under_php_s_name() {
    let engine = Engine::builder()
        .extensions(&[php_ext_opcache::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture(
        "<?php var_dump(extension_loaded('Zend OPcache'), extension_loaded('opcache'));",
        sink,
    );
    assert_eq!(served.stdout, b"bool(true)\nbool(false)\n");
}
