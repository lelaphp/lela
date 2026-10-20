//! A kept frame inside a `Fiber` that a `_ZendTestFiber::suspend()`
//! passed through (#2765): a debugger written as an extension keeps the
//! frame that calls `pause()` while the fiber runs, and at the next stop,
//! with the fiber waiting, writes `$x` of that frame and evaluates code
//! there, as xdebug does with `EG(current_execute_data)` moved into the
//! fiber's frames. php's chain runs from there into the resumer's frames,
//! the test fiber's, which it holds suspended: php never ran
//! `zend_fiber_suspend` for the `Fiber`, so its bottom frame's
//! `prev_execute_data` still names the `start` that resumed it.

use php_ext::abi::{ExtensionBuilder, Observed, ObserverHandlers};
use php_ext::call::call_method;
use php_ext::raising::{Cause, Thrown, clear};
use php_ext::request::{FrameId, Request};
use php_ext::test::{Capture, Engine};
use php_ext::values::{Owner, Value};

/// The debugger's request state: the frame its first stop kept.
#[derive(Default)]
struct Kept {
    frame: Option<FrameId>,
}

/// What an evaluation answered: the int it made, or the class, message
/// and trace of what it threw.
fn shown(req: &mut Request, answered: Option<Result<Owner<Value>, Thrown>>) -> String {
    match answered {
        None => String::from("refused"),
        Some(Ok(value)) => {
            let text = value
                .as_int()
                .map_or_else(|| value.type_name().to_owned(), |n| format!("int({n})"));
            value.give_up(req);
            text
        }
        Some(Err(thrown)) => {
            let text = match thrown.cause(req) {
                Cause::Throwable(throwable) => {
                    let throwable = *throwable;
                    let class = req.class_name(throwable.class());
                    let message = throwable
                        .prop(req, b"message")
                        .and_then(|message| message.as_str_bytes().map(<[u8]>::to_vec))
                        .unwrap_or_default();
                    let trace = call_method(req, &throwable, b"getTraceAsString", &[])
                        .ok()
                        .map(|trace| {
                            let text = trace.as_str_bytes().map(<[u8]>::to_vec).unwrap_or_default();
                            trace.give_up(req);
                            text
                        })
                        .unwrap_or_default();
                    format!(
                        "{class}: {}\n{}",
                        String::from_utf8_lossy(&message),
                        String::from_utf8_lossy(&trace)
                    )
                }
                Cause::Exit => String::from("exit"),
                Cause::Fatal => String::from("fatal"),
            };
            clear(req, thrown).ok();
            text
        }
    }
}

/// `pause()` begun: the first stop keeps its caller's frame; the one
/// after writes `$x` of the kept frame, evaluates `$x * 2` there, then
/// code that throws.
fn stop(observed: &mut Observed<'_>) {
    let Some(frame) = observed.state::<Kept>().frame else {
        let kept = observed.caller().map(|caller| caller.id());
        observed.state::<Kept>().frame = kept;
        return;
    };
    let set = observed.set_frame_var(frame, b"x", Owner::int(5));
    let line = format!("[set {}]", if set { "ok" } else { "refused" });
    observed.response_mut().write(line.as_bytes());
    for code in ["$x * 2", "throw new Exception('no')"] {
        let answered = observed.eval_in_frame(frame, code.as_bytes(), b"debugger");
        let line = format!("[eval {}]", shown(observed, answered));
        observed.response_mut().write(line.as_bytes());
    }
}

fn init(observed: &mut Observed<'_>) -> ObserverHandlers {
    let named = observed
        .function()
        .and_then(|function| observed.function_decl(function))
        .is_some_and(|decl| decl.name.0.as_ref() == b"pause");
    ObserverHandlers {
        begin: named.then_some(stop as fn(&mut Observed<'_>)),
        end: None,
    }
}

/// `f()`, which keeps its frame at `pause()` and suspends the test fiber
/// from there, and reads `$x` once it is resumed: the program's lines
/// 2 to 9.
const F: &str = "<?php\nfunction pause() {}\nfunction f() {\n    $x = 1;\n    pause();\n    \
     _ZendTestFiber::suspend();\n    echo $x, \"\\n\";\n}\n";

/// `F` and `main` served with `_ZendTestFiber` and the debugger, whose
/// second stop is the main frame's while the fibers wait.
fn served(main: &str) -> String {
    let debugger = ExtensionBuilder::default()
        .request_state::<Kept>()
        .observer(init)
        .build("debugger");
    let engine = Engine::builder()
        .extensions(&[php_ext_zend_test::extension(), debugger])
        .build();
    let served = engine.serve_capture(&format!("{F}{main}"), Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// `f()` in a `Fiber` the test fiber started.
const ONE_DEEP: &str = "$test = new _ZendTestFiber(function () {\n    \
     $fiber = new Fiber(function () { f(); });\n    $fiber->start();\n});\n\
     $test->start();\npause();\n$test->resume();\n";

/// `f()` in a `Fiber` another `Fiber` started inside the test fiber.
const TWO_DEEP: &str = "$test = new _ZendTestFiber(function () {\n    \
     $a = new Fiber(function () {\n        $b = new Fiber(function () { f(); });\n        \
     $b->start();\n    });\n    $a->start();\n});\n$test->start();\npause();\n$test->resume();\n";

/// A frame of a `Fiber` waiting under the test fiber's suspend is written
/// and evaluated in, and the fiber reads the write once the test fiber is
/// resumed.
#[test]
fn a_frame_of_a_fiber_a_test_fibers_suspend_passed_through_is_written_and_evaluated_in() {
    for main in [ONE_DEEP, TWO_DEEP] {
        let out = served(main);
        assert!(out.starts_with("[set ok][eval int(10)]"), "{out}");
        assert!(out.ends_with("]5\n"), "{out}");
    }
}

/// What code evaluated there throws traces down php's `prev_execute_data`
/// chain from the frame, into the test fiber's frames that resumed the
/// `Fiber`, and on to the test fiber's bottom. The evaluated code's own row
/// is php's `unknown()` at the frame's line; the rows under it are the
/// zend-test oracle's for a throw from `f()` while the `Fiber` runs, the
/// same chain, numbered by this program's lines:
///
/// ```text
/// #0 Standard input code(10): f()
/// #1 [internal function]: {closure:{closure:Standard input code:9}:10}()
/// #2 Standard input code(11): Fiber->start()
/// #3 [internal function]: {closure:Standard input code:9}()
/// #4 {main}
/// ```
#[test]
fn what_an_evaluation_there_throws_traces_on_into_the_test_fibers_frames() {
    let out = served(ONE_DEEP);
    assert!(
        out.contains(
            "[eval Exception: no\n#0 Standard input code(6): unknown()\n\
             #1 Standard input code(10): f()\n\
             #2 [internal function]: {closure:{closure:Standard input code:9}:10}()\n\
             #3 Standard input code(11): Fiber->start()\n\
             #4 [internal function]: {closure:Standard input code:9}()\n#5 {main}]"
        ),
        "{out}"
    );
}

/// Two `Fiber`s deep, the chain runs from the inner one's frame through
/// the outer one's into the test fiber's, as the zend-test oracle's trace
/// of a throw from `f()` while the inner one runs reads it (the witness on
/// `_ZendTestFiber::suspend` pins that trace), numbered by this program's
/// lines:
///
/// ```text
/// #0 Standard input code(11): f()
/// #1 [internal function]: {closure:{closure:{closure:Standard input code:9}:10}:11}()
/// #2 Standard input code(12): Fiber->start()
/// #3 [internal function]: {closure:{closure:Standard input code:9}:10}()
/// #4 Standard input code(14): Fiber->start()
/// #5 [internal function]: {closure:Standard input code:9}()
/// #6 {main}
/// ```
#[test]
fn the_trace_runs_through_every_fiber_the_suspend_passed_through() {
    let out = served(TWO_DEEP);
    assert!(
        out.contains(
            "[eval Exception: no\n#0 Standard input code(6): unknown()\n\
             #1 Standard input code(11): f()\n\
             #2 [internal function]: {closure:{closure:{closure:Standard input code:9}:10}:11}()\n\
             #3 Standard input code(12): Fiber->start()\n\
             #4 [internal function]: {closure:{closure:Standard input code:9}:10}()\n\
             #5 Standard input code(14): Fiber->start()\n\
             #6 [internal function]: {closure:Standard input code:9}()\n#7 {main}]"
        ),
        "{out}"
    );
}
