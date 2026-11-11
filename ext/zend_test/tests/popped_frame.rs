//! `ReflectionFiber` over a `Fiber` resumed since its own suspend and
//! now waiting under a suspend that passed through it: the divergence
//! `trace:fiber-view-under-pass-through-suspend` (`docs/divergence.toml`,
//! cause `php-ub`). php's answer there is not held, so no witness pins
//! the oracle's. The engine reads the
//! innermost frame on the fiber's own stack, and the values below are
//! the engine's, reasoned from the frames live at that point: `g()`'s,
//! at its line that calls `_ZendTestFiber::suspend()`, which the
//! closure's call of `g()` stands under. Only `_ZendTestFiber` makes a
//! suspend that passes through a `Fiber`, and `php_engine` reaches
//! neither it nor `ReflectionFiber`, so the test lives with the test
//! fiber and takes ext/reflection as a dev-dependency.

use php_ext::test::{Capture, Engine};

/// The row's sequence, its lines numbered as the program's: the `Fiber`
/// suspends on its own at line 7; the test fiber's closure resumes it
/// at line 12; and it calls `g()` at line 8 and waits under `g()`'s
/// `_ZendTestFiber::suspend()` at line 3, which passes through it,
/// while the main code reads it through `ReflectionFiber`.
const PROGRAM: &str = "<?php\n\
function g() {\n\
    _ZendTestFiber::suspend();\n\
}\n\
$test = new _ZendTestFiber(function () {\n\
    $f = new Fiber(function () {\n\
        Fiber::suspend();\n\
        g();\n\
    });\n\
    $GLOBALS['f'] = $f;\n\
    $f->start();\n\
    $f->resume();\n\
});\n\
$test->start();\n\
$r = new ReflectionFiber($f);\n\
var_dump($f->isRunning(), $r->getExecutingLine(), $r->getExecutingFile());\n\
foreach ($r->getTrace() as $i => $row) {\n\
    echo '#', $i, ' ', $row['class'] ?? '', $row['type'] ?? '', $row['function'], ' ', $row['line'] ?? '-', \"\\n\";\n\
}\n\
$test->resume();\n\
echo \"done\\n\";\n";

/// What [`PROGRAM`] writes on an engine with this crate and
/// `ReflectionFiber`'s, nothing else.
fn served() -> String {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_zend_test::extension(),
            php_ext_reflection::extension(),
        ])
        .build();
    engine
        .serve_capture(PROGRAM, Capture::default())
        .stdout_text()
}

/// The `Fiber` waits running, and `ReflectionFiber` reads the innermost
/// frame of its stack: the executing line and file are `g()`'s
/// call of the test fiber's suspend, line 3, and the trace runs from that
/// call through `g()`'s, at line 8, down to the fiber's closure, which a
/// native, the `Fiber`'s start, called. Neither the start's bottom frame
/// (the resumer's line 11 and no row) nor line 7 is read.
#[test]
fn a_fiber_resumed_since_its_own_suspend_is_read_at_its_innermost_frame() {
    assert_eq!(
        served(),
        "bool(true)\n\
         int(3)\n\
         string(19) \"Standard input code\"\n\
         #0 _ZendTestFiber::suspend 3\n\
         #1 g 8\n\
         #2 {closure:{closure:Standard input code:5}:6} -\n\
         done\n"
    );
}
