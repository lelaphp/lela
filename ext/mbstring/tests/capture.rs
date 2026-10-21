//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

mod support;

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let sink = Capture::default();
    let served = engine().serve_capture("<?php var_dump(extension_loaded('mbstring'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

fn engine() -> Engine {
    support::engine()
}

/// `mb_convert_variables` writes each converted element back under its
/// own key, the array keeping its owner of the key: a string of the
/// key's size allocated after is not handed the key's block.
#[test]
fn a_converted_array_keeps_its_string_keys() {
    let source = r#"<?php
$k = 'kkkkk'; $z = 'ZZZZZ';
$a = [$k . $k => "\xe9"];
mb_convert_variables('UTF-8', 'ISO-8859-1', $a);
$x = $z . $z;
foreach ($a as $key => $v) { echo $key, '=', $v, "\n"; }
"#;
    let served = engine().serve_capture(source, Capture::default());
    served.assert_clean("kkkkkkkkkk=é\n");
}

/// Two arrays converted with twenty strings of their keys' size
/// allocated between them: each keeps its key, and the request ends as
/// php's does.
#[test]
fn converting_two_arrays_around_reused_slots_keeps_both_keys() {
    let source = r#"<?php
$k = 'kkkkk'; $y = 'xxxxx';
$a = [$k . $k => "\xe9"];
mb_convert_variables('UTF-8', 'ISO-8859-1', $a);
$s = [];
for ($i = 0; $i < 20; $i++) { $s[] = $y . $y; }
$b = [$k . $k => "\xe9"];
mb_convert_variables('UTF-8', 'ISO-8859-1', $b);
foreach ([$a, $b] as $c) { foreach ($c as $key => $v) { echo $key, '=', $v, "\n"; } }
"#;
    let served = engine().serve_capture(source, Capture::default());
    served.assert_clean("kkkkkkkkkk=é\nkkkkkkkkkk=é\n");
}
