//! Frame writes and evaluation in a paused frame (#1828): a debugger
//! written as an extension stops where the program calls `pause()`, its
//! observer's begin handler standing in that call, writes a variable of
//! a frame it holds a [`FrameId`] of ([`Frames::set_var`]) and
//! evaluates code in that frame's scope ([`Frames::eval_in_frame`]), as
//! xdebug's `property_set` and `eval` do in php through
//! `zend_eval_stringl` with `EG(current_execute_data)` at the frame.
//!
//! What the debugger does at each stop is the test's plan, which the
//! debugger registers with the engine it is built into: which frame,
//! counted down from the caller of `pause()`, the variable to write and
//! the code to evaluate. Every expectation is what the oracle prints for
//! the same program with the write and the evaluation typed into the
//! paused frame by hand, where php's own code can reach that frame.

use crate::api::abi::{EngineState, ExtensionBuilder, Observed, ObserverHandlers};
use crate::api::raising::{Cause, Thrown};
use crate::api::request::{FrameId, Request};
use crate::api::values::{Owner, Value};
use crate::engine::Engine;
use crate::test::Capture;

/// What the debugger does at every stop.
trait Stop: Copy + EngineState {
    fn stop(self, observed: &mut Observed<'_>);
}

/// What the debugger does at a stop: write and evaluate in the frame
/// it names.
#[derive(Clone, Copy, EngineState)]
struct Plan {
    /// How many frames below the caller of `pause()` the frame stands.
    depth: usize,
    /// The variable written, and the int written to it.
    write: Option<(&'static str, i64)>,
    /// The code evaluated after the write.
    code: &'static str,
    /// Whether what the evaluation throws is shown with its trace.
    traced: bool,
}

/// The frame the plan names.
fn paused(observed: &Observed<'_>, depth: usize) -> Option<FrameId> {
    let mut at = observed.caller()?;
    for _ in 0..depth {
        at = at.caller()?;
    }
    Some(at.id())
}

/// What an evaluation answered, as the debugger reports it: the value's
/// type and contents, or the class and message of what it threw.
fn shown(
    req: &mut Request,
    answered: Option<Result<Owner<Value>, Thrown>>,
    traced: bool,
) -> String {
    match answered {
        None => String::from("refused"),
        Some(Ok(value)) => {
            let text = if let Some(n) = value.as_int() {
                format!("int({n})")
            } else if let Some(bytes) = value.as_str_bytes() {
                format!("string(\"{}\")", String::from_utf8_lossy(bytes))
            } else {
                value.type_name().to_owned()
            };
            value.give_up(req);
            text
        }
        Some(Err(thrown)) => match thrown.cause(req) {
            Cause::Throwable(throwable) => {
                let throwable = *throwable;
                let class = String::from_utf8_lossy(req.symbols().class_name(throwable.class()))
                    .into_owned();
                let message = throwable
                    .prop(req, b"message")
                    .and_then(|message| message.as_str_bytes().map(<[u8]>::to_vec))
                    .unwrap_or_default();
                let trace = throwable
                    .prop(req, b"trace")
                    .and_then(|trace| trace.as_array())
                    .filter(|_| traced)
                    .map(|trace| {
                        let text = req.trace_to_string(trace, true);
                        format!("\n{}", String::from_utf8_lossy(&text))
                    })
                    .unwrap_or_default();
                req.release(throwable.as_value());
                format!("{class}: {}{trace}", String::from_utf8_lossy(&message))
            }
            Cause::Exit => String::from("exit"),
            Cause::Taken => String::from("taken"),
            Cause::Fatal => String::from("fatal"),
        },
    }
}

impl Stop for Plan {
    /// The plan carried out where the program stands.
    fn stop(self, observed: &mut Observed<'_>) {
        let Some(frame) = paused(observed, self.depth) else {
            observed.response_mut().write(b"[no frame]");
            return;
        };
        if let Some((name, value)) = self.write {
            let set = observed
                .frames_mut()
                .set_var(frame, name.as_bytes(), Owner::int(value));
            let line = format!("[set {}]", if set { "ok" } else { "refused" });
            observed.response_mut().write(line.as_bytes());
        }
        let answered =
            observed
                .frames_mut()
                .eval_in_frame(frame, self.code.as_bytes(), b"debugger");
        let line = format!("[eval {}]", shown(observed, answered, self.traced));
        observed.response_mut().write(line.as_bytes());
    }
}

/// `pause()` begun: the plan the debugger registered carried out.
fn stopped<P: Stop>(observed: &mut Observed<'_>) {
    let plan = observed.registered::<P>().next().copied();
    if let Some(plan) = plan {
        plan.stop(observed);
    }
}

fn init<P: Stop>(observed: &mut Observed<'_>) -> ObserverHandlers {
    let named = observed
        .function()
        .and_then(|function| observed.symbols().function_decl(function))
        .is_some_and(|decl| decl.name.0.as_ref() == b"pause");
    ObserverHandlers {
        begin: named.then_some(stopped::<P> as fn(&mut Observed<'_>)),
        end: None,
    }
}

/// `source` served with a debugger that carries `plan` out at every
/// stop.
fn serve<P: Stop>(plan: P, source: &str) -> String {
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .observer(init::<P>)
            .register(plan)
            .request_state::<Kept>()
            .build("debugger")])
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// A local of the paused frame changed, and an expression evaluated
/// there reads it changed; the program goes on with the new value.
#[test]
fn a_debugger_changes_a_local_and_evaluates_an_expression_in_the_paused_frame() {
    let out = serve(
        Plan {
            depth: 0,
            write: Some(("x", 42)),
            code: "$x + $y",
            traced: false,
        },
        "<?php\nfunction pause() {}\nfunction f() {\n    $x = 1;\n    $y = 3;\n    pause();\n    echo $x, \"\\n\";\n}\nf();\n",
    );
    assert_eq!(out, "[set ok][eval int(45)]42\n");
}

/// A frame below the innermost is evaluated in with its own `$this` and
/// class, a private property read, and what the evaluation assigns is
/// the frame's variable when it goes on.
#[test]
fn an_evaluation_in_a_frame_below_reads_its_this_and_writes_its_variables() {
    let out = serve(
        Plan {
            depth: 1,
            write: None,
            code: "$local = $this->p . $local",
            traced: false,
        },
        "<?php\nfunction pause() {}\nclass C {\n    private $p = 'secret';\n    function m() {\n        $local = 2;\n        g();\n        return $local;\n    }\n}\nfunction g() { pause(); }\necho (new C)->m(), \"\\n\";\n",
    );
    assert_eq!(out, "[eval string(\"secret2\")]secret2\n");
}

/// A variable the frame names nowhere is written into the symbol table
/// php's `zend_rebuild_symbol_table` builds for it, and the frame reads
/// it from there.
#[test]
fn a_variable_the_frame_does_not_name_is_written_into_its_table() {
    let out = serve(
        Plan {
            depth: 0,
            write: Some(("fresh", 7)),
            code: "$fresh * 2",
            traced: false,
        },
        "<?php\nfunction pause() {}\nfunction h() {\n    pause();\n    echo get_defined_vars()['fresh'], \"\\n\";\n}\nh();\n",
    );
    assert_eq!(out, "[set ok][eval int(14)]7\n");
}

/// What the evaluated code throws, and the compiler's `ParseError`, are
/// the `Err`, taken, and the program goes on.
#[test]
fn what_an_evaluation_throws_is_handed_back_and_the_program_goes_on() {
    let thrown = serve(
        Plan {
            depth: 0,
            write: None,
            code: "throw new Exception('no')",
            traced: false,
        },
        "<?php\nfunction pause() {}\nfunction f() { pause(); echo \"on\\n\"; }\nf();\n",
    );
    assert_eq!(thrown, "[eval Exception: no]on\n");
    let parse = serve(
        Plan {
            depth: 0,
            write: None,
            code: "1 +",
            traced: false,
        },
        "<?php\nfunction pause() {}\nfunction f() { pause(); echo \"on\\n\"; }\nf();\n",
    );
    assert_eq!(
        parse,
        "[eval ParseError: syntax error, unexpected token \";\"]on\n"
    );
}

/// A frame below a fiber, on the stack that resumed it, is written and
/// evaluated in, the request running on that stack for it, and the
/// frame reads the write when the fiber returns, as it reads the same
/// write typed into it by hand.
#[test]
fn a_frame_below_a_fiber_is_written_and_evaluated_in() {
    let out = serve(
        Plan {
            depth: 1,
            write: Some(("v", 9)),
            code: "$v * 2",
            traced: false,
        },
        "<?php\nfunction pause() {}\nfunction outer() {\n    $v = 1;\n    $f = new Fiber(function () { pause(); });\n    $f->start();\n    echo $v, \"\\n\";\n}\nouter();\n",
    );
    assert_eq!(out, "[set ok][eval int(18)]9\n");
}

/// The frame the first stop kept, for the stops after it: the
/// debugger's request state.
#[derive(Default)]
struct Kept(Option<FrameId>);

/// What a debugger does whose first stop keeps its caller's frame and
/// whose every stop after it writes `$x` of the kept frame and evaluates
/// `code` there, its throwable shown with its trace where `traced`.
#[derive(Clone, Copy, EngineState)]
struct Revisit {
    code: &'static str,
    traced: bool,
}

impl Stop for Revisit {
    fn stop(self, observed: &mut Observed<'_>) {
        let Some(frame) = observed.state::<Kept>().0 else {
            let caller = observed.caller().map(|caller| caller.id());
            observed.state::<Kept>().0 = caller;
            return;
        };
        let set = observed.frames_mut().set_var(frame, b"x", Owner::int(5));
        let line = format!("[set {}]", if set { "ok" } else { "refused" });
        observed.response_mut().write(line.as_bytes());
        let answered =
            observed
                .frames_mut()
                .eval_in_frame(frame, self.code.as_bytes(), b"debugger");
        let line = format!("[eval {}]", shown(observed, answered, self.traced));
        observed.response_mut().write(line.as_bytes());
    }
}

/// A frame inside a fiber, kept while the fiber ran, is written and
/// evaluated in while the fiber is suspended, on the stack the fiber
/// holds, and the fiber reads the write when it is resumed, as php's
/// `fiber->execute_data` reaches it; once the fiber has returned, the
/// frame is refused.
#[test]
fn a_suspended_fibers_frame_is_written_and_evaluated_in() {
    let out = serve(
        Revisit {
            code: "$x * 2",
            traced: false,
        },
        "<?php\nfunction pause() {}\nfunction f() {\n    $x = 1;\n    pause();\n    Fiber::suspend();\n    echo $x, \"\\n\";\n}\n\
         $fiber = new Fiber(function () { f(); });\n$fiber->start();\npause();\n$fiber->resume();\npause();\n",
    );
    assert_eq!(out, "[set ok][eval int(10)]5\n[set refused][eval refused]");
}

/// An evaluation in a suspended fiber's frame that gives up the last
/// owner of the fiber does not destroy it under the evaluation: the
/// serve holds the fiber until its frames are back with it, and the
/// fiber is destroyed once the evaluation is done, its `finally` blocks entered
/// there.
#[test]
fn a_suspended_fiber_whose_last_owner_an_evaluation_gives_up_is_destroyed_after_it() {
    let out = serve(
        Revisit {
            code: "$GLOBALS['fiber'] = null",
            traced: false,
        },
        "<?php\nfunction pause() {}\nfunction f() {\n    $x = 1;\n    pause();\n    try { Fiber::suspend(); } finally { echo \"[finally]\"; }\n}\n\
         $fiber = new Fiber(function () { f(); });\n$fiber->start();\npause();\necho \"[on]\\n\";\n",
    );
    assert_eq!(out, "[set ok][finally][eval null][on]\n");
}

/// What code evaluated in a suspended fiber's frame throws traces down
/// php's `prev_execute_data` chain from that frame, which ends at the
/// fiber's bottom frame, `zend_fiber_suspend` having cut what stood
/// under it: the fiber's callable is called from `[internal function]`,
/// and no row names the `start` that ran it or the frame that stops.
/// The evaluated code's own row is php's `unknown()` at the frame's
/// line, as `zend_fetch_debug_backtrace` names a code frame whose caller
/// stands at no `INCLUDE_OR_EVAL`; the rows under it are the oracle's
/// for a throw reached from `f()` while the fiber runs, cut where its
/// resumer's begin.
#[test]
fn what_an_evaluation_in_a_suspended_fibers_frame_throws_traces_to_its_bottom() {
    let out = serve(
        Revisit {
            code: "throw new Exception('no')",
            traced: true,
        },
        "<?php\nfunction pause() {}\nfunction f() {\n    $x = 1;\n    pause();\n    Fiber::suspend();\n}\n\
         $fiber = new Fiber(function () { f(); });\n$fiber->start();\npause();\n",
    );
    assert_eq!(
        out,
        "[set ok][eval Exception: no\n#0 Standard input code(6): unknown()\n\
         #1 Standard input code(8): f()\n#2 [internal function]: {closure:Standard input code:8}()\n#3 {main}]"
    );
}

/// A frame of a fiber suspended while it ran above another, kept on its
/// stack and not the one below it, is written and evaluated in once
/// both are suspended, and the inner fiber reads the write when it is
/// resumed.
#[test]
fn a_frame_of_a_fiber_suspended_above_another_is_written_and_evaluated_in() {
    let out = serve(
        Revisit {
            code: "$x * 2",
            traced: false,
        },
        "<?php\nfunction pause() {}\n\
         $outer = new Fiber(function () {\n\
             $inner = new Fiber(function () {\n        $x = 1;\n        pause();\n        Fiber::suspend();\n        echo $x, \"\\n\";\n    });\n\
             $inner->start();\n    Fiber::suspend();\n    $inner->resume();\n});\n\
         $outer->start();\npause();\n$outer->resume();\n",
    );
    assert_eq!(out, "[set ok][eval int(10)]5\n");
}

/// A suspended generator's frame, kept as a [`FrameId`] while it ran, is
/// written and evaluated in, stood where a resumption stands it, and
/// the generator reads the write when it is resumed; once the generator
/// has returned, the frame is refused.
#[test]
fn a_suspended_generators_frame_is_written_and_evaluated_in() {
    let out = serve(
        Revisit {
            code: "$x * 2",
            traced: false,
        },
        "<?php\nfunction pause() {}\nfunction g() {\n    $x = 1;\n    pause();\n    yield 1;\n    echo $x, \"\\n\";\n}\n\
         $g = g();\n$g->current();\npause();\n$g->next();\npause();\n",
    );
    assert_eq!(out, "[set ok][eval int(10)]5\n[set refused][eval refused]");
}

/// A fatal the evaluation ends in is rendered where it is raised, the
/// message as the oracle renders the same `eval`; the handler is handed
/// it as the `Err`, and the request ends once the handler returns, as
/// `zend_eval_stringl`'s `zend_bailout` leaves it to the request's own
/// `zend_try`, so nothing after it runs. The compile runs with
/// `EG(current_execute_data)` at `f()`, so the trace starts there and
/// passes the frame of `pause()` above it, as the oracle's does for the
/// same `eval` typed into `f()`.
#[test]
fn a_fatal_in_an_evaluation_ends_the_request() {
    let out = serve(
        Plan {
            depth: 0,
            write: None,
            code: "new class { const A = 1; const A = 2; }",
            traced: false,
        },
        "<?php\nfunction pause() {}\nfunction f() { pause(); echo \"on\\n\"; }\nf();\n",
    );
    assert_eq!(
        out,
        "\nFatal error: Cannot redefine class constant class@anonymous::A in debugger on line 1\n\
         Stack trace:\n#0 Standard input code(4): f()\n#1 {main}\n[eval fatal]"
    );
}

/// What code evaluated in a frame below the innermost throws traces
/// from that frame down, php's `prev_execute_data` chain from the
/// evaluated code: the frames above it, `pause()` and the `eval` it was
/// called from, are on no row. `f()` stands at an `eval` of its own, so
/// the chain is the one the oracle traces for that `eval` throwing:
/// `#0 Standard input code(4): eval()`, `#1 Standard input code(6): f()`.
#[test]
fn what_an_evaluation_below_the_innermost_throws_traces_from_its_frame() {
    let out = serve(
        Plan {
            depth: 1,
            write: None,
            code: "throw new Exception('no')",
            traced: true,
        },
        "<?php\nfunction pause() {}\nfunction f() {\n    eval('pause();');\n}\nf();\n",
    );
    assert_eq!(
        out,
        "[eval Exception: no\n#0 Standard input code(4): eval()\n\
         #1 Standard input code(6): f()\n#2 {main}]"
    );
}

/// An evaluation in a frame below one that has a symbol table of its
/// own gives the lower frame a table after the upper one's; the upper
/// frame's table is still freed when it returns, so what only its table
/// held is destroyed there, as php's `zend_clean_and_cache_symbol_table`
/// destroys it.
#[test]
fn a_table_built_below_another_leaves_the_upper_one_freed_at_its_return() {
    let out = serve(
        Plan {
            depth: 1,
            write: None,
            code: "$x",
            traced: false,
        },
        "<?php\nfunction pause() {}\nclass D { function __destruct() { echo \"[dtor]\"; } }\n\
         function g() {\n    $n = 'e';\n    $$n = new D;\n    pause();\n    echo \"[g]\";\n}\n\
         function f() {\n    $x = 1;\n    g();\n    echo \"[after]\\n\";\n}\nf();\n",
    );
    assert_eq!(out, "[eval int(1)][g][dtor][after]\n");
}

/// `{main}`'s frame is evaluated in over the global table: a global
/// written and read, and the program reads the write.
#[test]
fn the_main_frame_is_written_and_evaluated_in_over_the_global_table() {
    let out = serve(
        Plan {
            depth: 1,
            write: Some(("g", 5)),
            code: "$g + 1",
            traced: false,
        },
        "<?php\nfunction pause() {}\nfunction f() { pause(); }\n$g = 1;\nf();\necho $g, \"\\n\";\n",
    );
    assert_eq!(out, "[set ok][eval int(6)]5\n");
}
