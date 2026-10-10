//! `MessageFormatter` where its pattern changes under a format or its
//! constructor fails, each a test held to the engine alone.
//!
//! The argument types are copied before any PHP code runs and the
//! formatter is read again for the call, so a format whose argument sets
//! the pattern runs with the pattern set
//! (`trace:intl-msgfmt-pattern-replaced-in-argument`).
//!
//! An ICU object is kept only where its status is a success, so a
//! constructor whose pattern ICU refuses leaves the object unconstructed,
//! and the program answers the same every time it is served.

use php_ext::test::{Capture, Engine};

#[test]
fn an_argument_that_replaces_the_pattern_is_formatted_with_the_new_one() {
    let engine = Engine::builder()
        .extensions(&[php_ext_intl::extension()])
        .build();
    let script = r#"<?php
$m = new MessageFormatter('en', '{0} {1,number}');
class S { function __toString(): string { global $m; $m->setPattern('{1,number} then {0}'); return "str"; } }
var_dump($m->format([new S, 5]), $m->getErrorMessage(), $m->getPattern());
"#;
    let served = engine.serve_capture(script, Capture::default());
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "string(10) \"5 then str\"\nstring(12) \"U_ZERO_ERROR\"\nstring(19) \"{1,number} then {0}\"\n"
    );
    assert_eq!(String::from_utf8_lossy(&served.stderr), "");
}
