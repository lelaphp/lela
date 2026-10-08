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
    assert_eq!(served.stdout, b"bool(true)\n");
}

/// A callback that replaces the converter ICU is converting with
/// (`setSourceEncoding()` inside `toUCallback()`) leaves its close to the
/// conversion's end (ADR-0013): the conversion goes on with the old
/// converter and the next one uses the new. php frees the old one under
/// ICU and dies on a signal there, which the witness lane cannot pin
/// (no status), so the snippet is held here.
#[test]
fn a_converter_freed_inside_its_own_callback_is_closed_as_the_conversion_ends() {
    let engine = Engine::builder()
        .extensions(&[php_ext_intl::extension()])
        .build();
    let script = r#"<?php
class Swap extends UConverter {
    public $swapped = 0;
    public function toUCallback($reason, $source, $codeUnits, &$error): string|int|array|null {
        echo 'to ', UConverter::reasonText($reason), "\n";
        if ($reason == UConverter::REASON_ILLEGAL && !$this->swapped++) {
            var_dump($this->setSourceEncoding('latin1'), $this->getSourceEncoding());
        }
        $error = U_ZERO_ERROR;
        return '?';
    }
}
$c = new Swap('utf-8', 'ascii');
var_dump($c->convert("a\x80b\x81c"));
var_dump($c->getSourceEncoding(), $c->convert("\xE4"));
$c = null;
echo "done\n";
"#;
    let served = engine.serve_capture(script, Capture::default());
    let expect = "to REASON_RESET\nto REASON_ILLEGAL\nto REASON_CLOSE\nbool(true)\nstring(10) \"ISO-8859-1\"\n\
                  to REASON_ILLEGAL\nto REASON_RESET\nto REASON_ILLEGAL\nto REASON_ILLEGAL\n\
                  string(5) \"a?b?c\"\nto REASON_RESET\nto REASON_RESET\n\
                  string(10) \"ISO-8859-1\"\nstring(2) \"\u{e4}\"\n\
                  to REASON_CLOSE\nto REASON_CLOSE\ndone\n";
    assert_eq!(String::from_utf8_lossy(&served.stdout), expect);
}
