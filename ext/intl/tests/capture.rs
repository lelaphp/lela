//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[php_ext_intl::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('intl'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

/// A `fromUCallback` that answers one byte more in the conversion's
/// second pass than in its preflight: the engine returns the bytes php
/// returns, in a string grown to hold them, which `memory_get_usage()`
/// reads a bin larger, 40 where php reads 32
/// (`trace:intl-convert-second-pass-one-byte-longer`).
#[test]
fn a_callback_one_byte_past_the_preflight_grows_the_string() {
    let engine = Engine::builder()
        .extensions(&[php_ext_intl::extension()])
        .build();
    let served = engine.serve_capture(
        r#"<?php
class G extends UConverter {
  public $n = 0;
  function fromUCallback($reason, $source, $codePoint, &$error): string|int|array|null {
    if ($reason != UConverter::REASON_UNASSIGNED) return null;
    $error = U_ZERO_ERROR;
    return $this->n++ ? 'yy' : 'x';
  }
}
$c = new G('ASCII', 'UTF-8');
$m = memory_get_usage();
$s = $c->convert("abcdef\u{100}");
var_dump($s, memory_get_usage() - $m);
"#,
        Capture::default(),
    );
    assert_eq!(served.stdout_text(), "string(8) \"abcdefyy\"\nint(40)\n");
    assert_eq!(served.exit_code, 0);
}
