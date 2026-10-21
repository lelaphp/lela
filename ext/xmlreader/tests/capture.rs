//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_libxml::extension(),
            php_ext_lexbor::extension(),
            php_ext_dom::extension(),
            php_ext_standard::extension(),
            php_ext_uri::extension(),
            php_ext_xmlreader::extension(),
        ])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('xmlreader'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}
