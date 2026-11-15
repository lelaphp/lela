//! The request leak sensor over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine};

/// `src` served by an armed engine with this extension alone, what it
/// wrote held to `expected`.
fn served(src: &str, expected: &str) {
    let engine = Engine::builder()
        .extensions(&[php_ext_reflection::extension()])
        .build();
    let served = engine.serve_capture(src, Capture::default());
    assert_eq!(served.stdout_text(), expected);
}

/// The closure `ReflectionFunction::getClosure()` makes of a function
/// holds the function, and the name it was looked up by is given up.
#[test]
fn a_function_s_closure_keeps_no_name() {
    served(
        "<?php echo (new ReflectionFunction('strlen'))->getClosure()('abc'), \"\\n\";",
        "3\n",
    );
}

/// An attribute's arguments are given up where one of them fails to
/// evaluate, as `getArguments` gives up `args`, and the flags of an
/// `#[Attribute]` marker that are no int are given up after the error
/// names them, `zval_ptr_dtor(&flags)`.
#[test]
fn an_attribute_s_arguments_are_given_up_where_they_fail() {
    served(
        "<?php\n\
         #[A(['k' => 1], Missing::C)] function f() {}\n\
         try { (new ReflectionFunction('f'))->getAttributes()[0]->getArguments(); } catch (Error $e) { echo $e->getMessage(), \"\\n\"; }\n\
         #[Attribute('foo' . PHP_EOL)] class A {}\n\
         #[A] function g() {}\n\
         try { (new ReflectionFunction('g'))->getAttributes()[0]->newInstance(); } catch (Error $e) { echo $e->getMessage(), \"\\n\"; }\n",
        "Class \"Missing\" not found\nAttribute::__construct(): Argument #1 ($flags) must be of type int, string given\n",
    );
}
