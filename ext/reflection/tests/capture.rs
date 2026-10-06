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

/// An attribute's arguments are given up where one of them fails to
/// evaluate, as `getArguments` gives up `args`, and the flags of an
/// `#[Attribute]` marker that are no int are given up after the error
/// names them, `zval_ptr_dtor(&flags)` (#2078).
#[cfg(debug_assertions)]
#[test]
fn an_attribute_s_arguments_are_given_up_where_they_fail() {
    served_armed(
        "<?php\n\
         #[A(['k' => 1], Missing::C)] function f() {}\n\
         try { (new ReflectionFunction('f'))->getAttributes()[0]->getArguments(); } catch (Error $e) { echo $e->getMessage(), \"\\n\"; }\n\
         #[Attribute('foo' . PHP_EOL)] class A {}\n\
         #[A] function g() {}\n\
         try { (new ReflectionFunction('g'))->getAttributes()[0]->newInstance(); } catch (Error $e) { echo $e->getMessage(), \"\\n\"; }\n",
        "Class \"Missing\" not found\nAttribute::__construct(): Argument #1 ($flags) must be of type int, string given\n",
    );
}
