//! The request's end with `intl_error` messages standing, under the
//! debug build's request leak sensor, which every engine a test builds
//! arms: an object's message is freed with the object, the request's at
//! `RSHUTDOWN`, and one an object's `free_obj` sets after `RSHUTDOWN`
//! with the request's heap, so the sensor finds nothing the request
//! still holds.

use php_ext::test::{Capture, Engine};

#[test]
fn messages_standing_as_the_request_ends_are_freed_with_it() {
    let engine = Engine::builder()
        .extensions(&[php_ext_intl::extension()])
        .build();
    // `$twice` keeps the converter past the symbol table's destructors,
    // so it is freed with the object store, after `RSHUTDOWN`, and its
    // close callbacks set the request's message there. php's executor
    // serves no callback by then (`EG(active)`), which the engine's does.
    let script = r#"<?php
class C extends UConverter {
    function toUCallback($reason, $source, $codeUnits, &$error): string|int|array|null {
        return $reason === self::REASON_CLOSE ? [new stdClass] : null;
    }
}
$kept = new C('ascii', 'utf-8');
$twice = $kept;
$f = new NumberFormatter('en', NumberFormatter::DECIMAL);
$f->parse('x');
$g = $f;
echo $f->getErrorMessage(), "\n";
IntlChar::isalpha(0x110000);
echo intl_get_error_message(), "\n";
"#;
    let served = engine.serve_capture(script, Capture::default());
    let stdout = served.stdout_text();
    assert_eq!(
        stdout,
        "NumberFormatter::parse(): Number parsing failed: U_PARSE_ERROR\n\
         IntlChar::isalpha(): Codepoint out of range: U_ILLEGAL_ARGUMENT_ERROR\n"
    );
    assert_eq!(String::from_utf8_lossy(&served.stderr), "");
}
