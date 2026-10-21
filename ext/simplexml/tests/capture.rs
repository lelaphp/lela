//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_libxml::extension(),
            php_ext_lexbor::extension(),
            php_ext_json::extension(),
            php_ext_standard::extension(),
            php_ext_spl::extension(),
            php_ext_uri::extension(),
            php_ext_simplexml::extension(),
        ])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('simplexml'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

/// `current()`, `key()` and the movers over an element read the table
/// the `(array)` cast builds of its children, as php reads the one
/// `sxe_get_properties` keeps on the object, and give it up once read:
/// armed, the request ends with no block of its heap held (#2081).
#[cfg(debug_assertions)]
#[test]
fn the_pointer_natives_over_an_element_give_up_the_table_they_read() {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_json::extension(),
            php_ext_spl::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
            php_ext_libxml::extension(),
            php_ext_simplexml::extension(),
        ])
        .sense_leaks(php_ext::test::LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(
        r#"<?php $x = simplexml_load_string('<a><b>1</b><c>2</c></a>');
echo @current($x), @key($x), @next($x), @reset($x), @end($x), @prev($x), "\n";"#,
        Capture::default(),
    );
    served.assert_clean("1b212\n");
}
