//! The request leak sensor as a program meets it (#1992): after a clean
//! request a debug build's heap holds nothing, a deliberately faulty
//! native that drops an owner it made ends the request in the
//! `InternalError` naming what the heap still held, and a request that
//! ended in a fatal reports nothing, as php's debug heap reports nothing
//! after an unclean shutdown. The leak names the extension whose native
//! or lifecycle hook allocated it (#1993), and with allocations traced
//! the line that did. A debug engine is armed unless it is built with
//! the sensor off (`EngineBuilder::sense_leaks`); each engine here names
//! the sensor it is built with. A leak from an extension class's
//! handler names the extension that declared the class (#2113).

use php_macros::php_function;
use php_unit::decl::ClassKind;

use crate::api::__private::class;
use crate::api::abi::{CloneObj, Extension, ExtensionBuilder, ObjectHandlers, PropertyRead, Std};
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Owner, PhpObject, PhpString, Value};
use crate::engine::{Engine, LeakSensor};
use crate::test::Capture;

/// `dropped_owner(): void`, faulty on purpose: a string made, its one
/// owner the native's, and the owner handed on to a view nothing keeps,
/// so it is neither given up nor reported dropped. Seven bytes, so the
/// block is the 32-byte bin's.
#[php_function]
fn dropped_owner(req: &mut Request) {
    if let Ok(made) = PhpString::new(req, b"dropped") {
        let _leaked = made.hand_on();
    }
}

fn faulty() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![dropped_owner::declared()])
        .build("leak_sensor")
}

/// The request-end function of an extension, faulty as
/// [`dropped_owner`] is: a string made at `RSHUTDOWN` and its owner
/// dropped.
fn rshutdown_dropping_an_owner(req: &mut Request) {
    if let Ok(made) = PhpString::new(req, b"in hook") {
        let _leaked = made.hand_on();
    }
}

fn faulty_at_rshutdown() -> Extension {
    ExtensionBuilder::default()
        .rshutdown(rshutdown_dropping_an_owner)
        .build("leak_hooks")
}

/// What the sensor renders for the faulty native's one block, up to its
/// address, and after it.
const LEAKED: &str = "Fatal error: InternalError: the request ended with 1 block of its heap never freed, 32 bytes, the first of 32 bytes at 0x";
const BY_LEAK_SENSOR: &str = ", allocated by leak_sensor in Unknown on line 0\n";

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
        .sense_leaks(LeakSensor::Armed)
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
/// `InternalError` naming it and the extension whose native allocated
/// it, after everything the program wrote.
#[test]
fn a_dropped_owner_fails_the_request_as_an_internal_error() {
    let engine = Engine::cli_builder()
        .extensions(&[faulty()])
        .sense_leaks(LeakSensor::Armed)
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
    assert!(stdout.contains(BY_LEAK_SENSOR), "{stdout}");
    assert_eq!(served.exit_code, 255);
}

/// A debug engine built without a word about the sensor is armed, so
/// the faulty native's dropped owner fails its request, and one built
/// with the sensor off ends the request as a release engine does
/// (#1992's default).
#[test]
fn a_debug_engine_is_armed_unless_built_with_the_sensor_off() {
    let program = "<?php dropped_owner();\necho 'went on';\n";
    let armed = Engine::cli_builder().extensions(&[faulty()]).build();
    let served = armed.serve_capture(program, Capture::default());
    let stdout = String::from_utf8_lossy(&served.stdout);
    assert!(
        stdout.starts_with(&format!("went on\n{LEAKED}")),
        "{stdout}"
    );
    assert_eq!(served.exit_code, 255);
    let off = Engine::cli_builder()
        .extensions(&[faulty()])
        .sense_leaks(LeakSensor::Off)
        .build();
    let served = off.serve_capture(program, Capture::default());
    assert_eq!(String::from_utf8_lossy(&served.stdout), "went on");
    assert_eq!(served.exit_code, 0);
}

/// A block a lifecycle hook allocated and dropped is named with the
/// extension whose hook it was, and not the one whose native ran last.
#[test]
fn a_leak_from_a_lifecycle_hook_names_its_extension() {
    let engine = Engine::cli_builder()
        .extensions(&[faulty(), faulty_at_rshutdown()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture("<?php echo strlen('a');\n", Capture::default());
    let stdout = String::from_utf8_lossy(&served.stdout);
    assert!(stdout.starts_with(&format!("1\n{LEAKED}")), "{stdout}");
    assert!(
        stdout.contains(", allocated by leak_hooks in Unknown on line 0\n"),
        "{stdout}"
    );
}

/// With allocations traced the leak names the line of the faulty native
/// that allocated the block, beside its extension.
#[test]
fn the_backtrace_mode_names_the_leaking_line() {
    let engine = Engine::cli_builder()
        .extensions(&[faulty()])
        .sense_leaks(LeakSensor::Traced)
        .build();
    let served = engine.serve_capture("<?php dropped_owner();\n", Capture::default());
    let stdout = String::from_utf8_lossy(&served.stdout);
    let source = include_str!("leak_sensor.rs");
    let line = source
        .lines()
        .position(|line| line.contains("if let Ok(made) = PhpString::new(req, b\"dropped\") {"))
        .map_or(0, |at| at + 1);
    assert!(
        stdout.contains(&format!("leak_sensor.rs:{line}:")),
        "line {line}: {stdout}"
    );
    assert!(
        stdout.contains(", allocated by leak_sensor at:\n"),
        "{stdout}"
    );
    assert_eq!(served.exit_code, 255);
}

/// The same dropped owner in a request that then ends in a fatal: the
/// fatal is rendered and the sensor stays silent, since the frames
/// `zend_bailout` left were never unwound. `E_USER_ERROR`, the memory
/// limit and a throwable no frame stood to take, which php renders and
/// then bails out of, are each such a fatal.
#[test]
fn a_request_ending_in_a_fatal_reports_nothing() {
    let engine = Engine::cli_builder()
        .extensions(&[faulty()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    for (fatal, rendered) in [
        ("trigger_error('boom', E_USER_ERROR);", "Fatal error: boom"),
        (
            "ini_set('memory_limit', '2M'); $s = 'x'; while (true) { $s .= $s; }",
            "Fatal error: Allowed memory size of 2097152 bytes exhausted",
        ),
        (
            "register_shutdown_function(function () { throw new Exception('late'); });",
            "Fatal error: Uncaught Exception: late",
        ),
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

/// A program that ends in an uncaught throwable is not one php bails out
/// of: `zend_exception_error` renders it `E_DONT_BAIL`, so the shutdown
/// is clean and a debug php reports the request's leaks. The faulty
/// native's block is reported after the uncaught fatal (#2058).
#[test]
fn a_request_ending_in_an_uncaught_throwable_reports_its_leak() {
    let engine = Engine::cli_builder()
        .extensions(&[faulty()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    for uncaught in ["throw new Exception('x');", "undefined_function();"] {
        let served = engine.serve_capture(
            &format!("<?php dropped_owner();\n{uncaught}\n"),
            Capture::default(),
        );
        let stdout = String::from_utf8_lossy(&served.stdout);
        let (rendered, sensed) = stdout.split_once(LEAKED).unwrap_or_default();
        assert!(
            rendered.contains("Fatal error: Uncaught "),
            "{uncaught}: {stdout}"
        );
        assert!(sensed.contains(BY_LEAK_SENSOR), "{uncaught}: {stdout}");
        assert_eq!(served.exit_code, 255, "{uncaught}");
    }
}

/// What a handler slot answers is the caller's one owner, and what a
/// slot takes it keeps or gives up (#2102): `WeakMap`'s `offsetGet`
/// returns the owner its `read_dimension` answered, where it took a
/// second one to return and leaked the first, and its `offsetSet`, its
/// `InternalIterator`, its clone and its dump, a closure's clone and
/// dump, and an `ArrayAccess` read and written through the standard
/// slots each leave the heap empty.
#[test]
fn what_a_handler_slot_answers_is_the_callers_owner() {
    let engine = Engine::cli_builder().sense_leaks(LeakSensor::Armed).build();
    let served = engine.serve_capture(
        r#"<?php
$s = 'a'; $s .= 'aaaa';
$m = new WeakMap; $k = new stdClass; $m[$k] = [$s];
echo $m->offsetGet($k)[0], ' ', $m[$k][0], "\n";
$s .= 'b'; $m->offsetSet($k, [$s]);
foreach ($m->getIterator() as $key => $value) echo get_class($key), ' ', $value[0], "\n";
$c = clone $m; var_dump($c[$k][0]); unset($c);
$f = function ($x) use ($s) { return $x; }; $g = clone $f; var_dump($g);
class A implements ArrayAccess {
    public $rows = [];
    function offsetGet($o): mixed { return $this->rows[$o] ?? null; }
    function offsetSet($o, $v): void { $this->rows[$o] = $v; }
    function offsetExists($o): bool { return isset($this->rows[$o]); }
    function offsetUnset($o): void { unset($this->rows[$o]); }
}
$a = new A; $a['x'] = $s; $a['x'] .= 'd'; echo $a['x'], ' ', $a['y'] ?? 'none', "\n";
"#,
        Capture::default(),
    );
    let stdout = String::from_utf8_lossy(&served.stdout);
    assert_eq!(
        stdout,
        "aaaaa aaaaa\nstdClass aaaaab\nstring(6) \"aaaaab\"\nobject(Closure)#4 (5) {\n  [\"name\"]=>\n  string(31) \"{closure:Standard input code:8}\"\n  [\"file\"]=>\n  string(19) \"Standard input code\"\n  [\"line\"]=>\n  int(8)\n  [\"static\"]=>\n  array(1) {\n    [\"s\"]=>\n    string(6) \"aaaaab\"\n  }\n  [\"parameter\"]=>\n  array(1) {\n    [\"$x\"]=>\n    string(10) \"<required>\"\n  }\n}\naaaaabd none\n"
    );
    assert_eq!(served.exit_code, 0);
}

/// `Leaky`'s table, faulty on purpose in two slots: `read_property`
/// makes a string and drops its owner before it reads as `Std` reads,
/// and the function its `clone_obj` answers does the same before it
/// copies the members.
struct LeakyHandlers;

impl ObjectHandlers for LeakyHandlers {
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        if let Ok(made) = PhpString::new(req, b"in read") {
            let _leaked = made.hand_on();
        }
        Std.read_property(req, object, name, read)
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(leaky_clone)
    }
}

/// The `clone_obj` function `Leaky`'s table answers.
fn leaky_clone(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    if let Ok(made) = PhpString::new(req, b"in copy") {
        let _leaked = made.hand_on();
    }
    req.clone_members(object)
}

static LEAKY: LeakyHandlers = LeakyHandlers;

fn faulty_class() -> Extension {
    ExtensionBuilder::default()
        .class_with(class(b"Leaky", ClassKind::Class), Vec::new(), &LEAKY)
        .build("leak_class")
}

/// A block an extension class's handler allocated is named with the
/// extension that declared the class, though the dispatch loop called
/// the slot from a user program while Core's code was served (#2113):
/// the slot that serves code, `read_property`, and the function a slot
/// answers, the `clone_obj` that `clone` calls.
#[test]
fn a_leak_from_an_extension_class_s_handler_names_its_extension() {
    let engine = Engine::cli_builder()
        .extensions(&[faulty(), faulty_class()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    for program in [
        "<?php $o = new Leaky; echo $o->p ?? 1, ' served';\n",
        "<?php $o = new Leaky; $c = clone $o; echo 1, ' served';\n",
    ] {
        let served = engine.serve_capture(program, Capture::default());
        let stdout = String::from_utf8_lossy(&served.stdout);
        assert!(
            stdout.starts_with(&format!("1 served\n{LEAKED}")),
            "{stdout}"
        );
        assert!(
            stdout.contains(", allocated by leak_class in Unknown on line 0\n"),
            "{stdout}"
        );
    }
}

/// An error link of `zend_error_cb`'s chain, faulty as [`dropped_owner`]
/// is: a string made and its owner dropped, then the one beneath served.
fn error_dropping_an_owner(
    req: &mut Request,
    beneath: crate::api::abi::Beneath<'_, crate::api::abi::chain::Error>,
) {
    if let Ok(made) = PhpString::new(req, b"in link") {
        let _leaked = made.hand_on();
    }
    beneath.serve(req);
}

/// An extension's hook served at each `ZEND_TICKS`, faulty the same way.
#[allow(clippy::unnecessary_wraps)] // `TicksHook`'s signature
fn tick_dropping_an_owner(req: &mut Request) -> Result<(), Thrown> {
    if let Ok(made) = PhpString::new(req, b"in tick") {
        let _leaked = made.hand_on();
    }
    Ok(())
}

/// A block an extension's link of a chained kind allocated and dropped
/// is named with that extension, as the chain serves each link as the
/// extension that supplied it, not the native or the compile the
/// diagnostic was raised in (#2627).
#[test]
fn a_leak_from_an_error_link_names_its_extension() {
    let linked = ExtensionBuilder::default()
        .error(error_dropping_an_owner)
        .build("leak_error");
    let engine = Engine::cli_builder()
        .extensions(&[linked])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(
        "<?php trigger_error('x', E_USER_NOTICE);\n",
        Capture::default(),
    );
    let stdout = String::from_utf8_lossy(&served.stdout);
    assert!(
        stdout.contains(", allocated by leak_error in Unknown on line 0\n"),
        "{stdout}"
    );
}

/// A block an extension's `ZEND_TICKS` hook allocated and dropped is
/// named with that extension, as each listed hook is served as its own
/// (#2627).
#[test]
fn a_leak_from_a_tick_function_names_its_extension() {
    let ticking = ExtensionBuilder::default()
        .ticks(tick_dropping_an_owner)
        .build("leak_tick");
    let engine = Engine::cli_builder()
        .extensions(&[ticking])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture("<?php declare(ticks=1); $a = 1;\n", Capture::default());
    let stdout = String::from_utf8_lossy(&served.stdout);
    assert!(
        stdout.contains(", allocated by leak_tick in Unknown on line 0\n"),
        "{stdout}"
    );
}
