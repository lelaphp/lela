//! Frame writes and evaluation in a paused frame (#1828): a debugger
//! written as an extension stops where the program calls `pause()`, its
//! observer's begin handler standing in that call, writes a variable of
//! a frame it holds a [`FrameId`] of ([`Request::set_frame_var`]) and
//! evaluates code in that frame's scope ([`Request::eval_in_frame`]), as
//! xdebug's `property_set` and `eval` do in php through
//! `zend_eval_stringl` with `EG(current_execute_data)` at the frame.
//!
//! What the debugger does at each stop is the test's plan: which frame,
//! counted down from the caller of `pause()`, the variable to write and
//! the code to evaluate. Every expectation is what the oracle prints for
//! the same program with the write and the evaluation typed into the
//! paused frame by hand, where php's own code can reach that frame.

use std::cell::Cell;

use crate::api::abi::{ExtensionBuilder, Observed, ObserverHandlers};
use crate::api::raising::{Cause, Thrown};
use crate::api::request::{FrameId, Request};
use crate::api::values::{Owner, Value};
use crate::engine::Engine;
use crate::test::Capture;

/// What the debugger does at a stop.
#[derive(Clone, Copy)]
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

thread_local! {
    /// The plan of the test this thread serves.
    static PLAN: Cell<Option<Plan>> = const { Cell::new(None) };
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
                let class = req.class_name(throwable.class());
                let message = throwable
                    .prop(req, b"message")
                    .and_then(|message| message.as_str_bytes().map(<[u8]>::to_vec))
                    .unwrap_or_default();
                let trace = throwable
                    .prop(req, b"trace")
                    .and_then(|trace| trace.as_array().copied())
                    .filter(|_| traced)
                    .map(|trace| format!("\n{}", req.trace_to_string(trace, true)))
                    .unwrap_or_default();
                req.release(throwable.as_value());
                format!("{class}: {}{trace}", String::from_utf8_lossy(&message))
            }
            Cause::Exit => String::from("exit"),
            Cause::Fatal => String::from("fatal"),
        },
    }
}

/// `pause()` begun: the plan carried out where the program stands.
fn stop(observed: &mut Observed<'_>) {
    let Some(plan) = PLAN.get() else {
        return;
    };
    let Some(frame) = paused(observed, plan.depth) else {
        observed.write(b"[no frame]");
        return;
    };
    if let Some((name, value)) = plan.write {
        let value = Owner::take(observed, Value::int(value));
        let set = observed.set_frame_var(frame, name.as_bytes(), value);
        let line = format!("[set {}]", if set { "ok" } else { "refused" });
        observed.write(line.as_bytes());
    }
    let answered = observed.eval_in_frame(frame, plan.code.as_bytes(), b"debugger");
    let line = format!("[eval {}]", shown(observed, answered, plan.traced));
    observed.write(line.as_bytes());
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

fn serve(plan: Plan, source: &str) -> String {
    PLAN.set(Some(plan));
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default().observer(init).build("debugger")])
        .build();
    let served = engine.serve_capture(source, Capture::default());
    PLAN.set(None);
    String::from_utf8_lossy(&served.stdout).into_owned()
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

/// A frame below a fiber, on the stack that resumed it, is refused, as
/// its slots stand on a stack the fiber's does not reach, and it goes on
/// as it stood.
#[test]
fn a_frame_below_a_fiber_is_refused() {
    let out = serve(
        Plan {
            depth: 1,
            write: Some(("v", 9)),
            code: "$v",
            traced: false,
        },
        "<?php\nfunction pause() {}\nfunction outer() {\n    $v = 1;\n    $f = new Fiber(function () { pause(); });\n    $f->start();\n    echo $v, \"\\n\";\n}\nouter();\n",
    );
    assert_eq!(out, "[set refused][eval refused]1\n");
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
