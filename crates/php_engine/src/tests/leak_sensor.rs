//! The request leak sensor as a program meets it (#1992): after a clean
//! request a debug build's heap holds nothing, a deliberately faulty
//! native that drops an owner it made ends the request in the
//! `InternalError` naming what the heap still held, and a request that
//! ended in a fatal reports nothing, as php's debug heap reports nothing
//! after an unclean shutdown. Each engine here arms the sensor
//! (`EngineBuilder::sense_leaks`), which is off by default until the
//! leaks the corpus has are fixed (#2057).

use php_macros::php_function;

use crate::api::abi::{Extension, ExtensionBuilder};
use crate::api::request::Request;
use crate::api::values::PhpString;
use crate::engine::Engine;
use crate::test::Capture;

/// `dropped_owner(): void`, faulty on purpose: a string made, its one
/// owner the native's, and the owner dropped, neither handed on nor
/// given up. Seven bytes, so the block is the 32-byte bin's.
#[php_function]
fn dropped_owner(req: &mut Request) {
    let _dropped = PhpString::new(req, b"dropped");
}

fn faulty() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![dropped_owner::declared()])
        .build("leak_sensor")
}

/// What the sensor renders for the faulty native's one block.
const LEAKED: &str = "Fatal error: InternalError: the request ended with 1 block of its heap never freed, 32 bytes, the first of 32 bytes at 0x";

/// A program that leaves something in every place the teardown frees by
/// name: a cycle the shutdown collection takes, a `static` variable, a
/// constant and a static property holding objects, an enum case, an
/// evaluated class constant and default property, a constant array of
/// a unit the request alone compiled, a closure and a rebuilt symbol
/// table. The sensor finds the heap empty and says nothing.
#[test]
fn after_a_clean_request_the_debug_heap_is_empty() {
    let engine = Engine::cli_builder()
        .extensions(&[faulty()])
        .sense_leaks(true)
        .build();
    let served = engine.serve_capture(
        r#"<?php
class Node { public $next; public $tag = [1, 2]; const LABEL = 'n' . 'ode'; public static $kept; }
enum Suit: string { case Hearts = 'h'; }
$a = new Node; $b = new Node; $a->next = $b; $b->next = $a; unset($a, $b);
function counter() { static $seen = []; $seen[] = new Node; return $seen; }
counter();
define('HELD', new Node);
Node::$kept = [new Node];
$suit = Suit::Hearts;
echo Node::LABEL, "\n";
eval('$literal = [1, "k" => [2, 3]];');
$closure = function () use ($literal) { return $literal; };
function rebuilt() { $local = 1; return get_defined_vars(); }
rebuilt();
echo "done\n";
"#,
        Capture::default(),
    );
    let stdout = String::from_utf8_lossy(&served.stdout);
    assert_eq!(stdout, "node\ndone\n");
    assert_eq!(served.exit_code, 0);
}

/// The faulty native's dropped owner is the one block the heap holds
/// where the request ends, and the sensor ends the request in the
/// `InternalError` naming it, after everything the program wrote.
#[test]
fn a_dropped_owner_fails_the_request_as_an_internal_error() {
    let engine = Engine::cli_builder()
        .extensions(&[faulty()])
        .sense_leaks(true)
        .build();
    let served = engine.serve_capture(
        "<?php echo 'a';\ndropped_owner();\necho 'went on';\n",
        Capture::default(),
    );
    let stdout = String::from_utf8_lossy(&served.stdout);
    assert!(
        stdout.starts_with(&format!("awent on\n{LEAKED}")),
        "{stdout}"
    );
    assert!(stdout.contains(" in Unknown on line 0\n"), "{stdout}");
    assert_eq!(served.exit_code, 255);
}

/// The same dropped owner in a request that then ends in a fatal: the
/// fatal is rendered and the sensor stays silent, since the frames
/// `zend_bailout` left were never unwound.
#[test]
fn a_request_ending_in_a_fatal_reports_nothing() {
    let engine = Engine::cli_builder()
        .extensions(&[faulty()])
        .sense_leaks(true)
        .build();
    for (fatal, rendered) in [
        ("undefined_function();", "Call to undefined function"),
        ("trigger_error('boom', E_USER_ERROR);", "Fatal error: boom"),
    ] {
        let served = engine.serve_capture(
            &format!("<?php dropped_owner();\n{fatal}\n"),
            Capture::default(),
        );
        let stdout = String::from_utf8_lossy(&served.stdout);
        assert!(stdout.contains(rendered), "{fatal}: {stdout}");
        assert!(!stdout.contains("InternalError"), "{fatal}: {stdout}");
        assert_eq!(served.exit_code, 255, "{fatal}");
    }
}
