//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[php_ext_reflection::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('reflection'));", sink);
    assert_eq!(served.stdout, b"bool(true)\n");
}

/// `src` served by an engine with this crate alone and the leak sensor
/// armed (#1992): what it wrote, held to `expected`, so an owner one of
/// its natives kept fails the test by the `InternalError` it ends in.
#[cfg(debug_assertions)]
fn served_armed(src: &str, expected: &str) {
    let engine = Engine::builder()
        .extensions(&[php_ext_reflection::extension()])
        .sense_leaks(php_ext::test::LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(src, Capture::default());
    assert_eq!(String::from_utf8_lossy(&served.stdout), expected);
}

/// The closure `ReflectionFunction::getClosure()` makes of a function
/// holds the function, and the name it was looked up by is given up
/// (#2062).
#[cfg(debug_assertions)]
#[test]
fn a_function_s_closure_keeps_no_name() {
    served_armed(
        "<?php echo (new ReflectionFunction('strlen'))->getClosure()('abc'), \"\\n\";",
        "3\n",
    );
}
