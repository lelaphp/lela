//! The running execution's trace number (#2738; `jit-design.md` §4.9,
//! revisions 23 and NEXT; design §5.11, revision 52): php's
//! `EG(jit_trace_num)`, which the tier reads and writes through
//! [`tier::trace_num`] and [`tier::set_trace_num`] and the engine keeps
//! with the execution state. A program reaches it here through two
//! natives of the test's own, `trace_num()` and `set_trace_num()`, as
//! the tier around a trace it executes would, so each test reads the
//! number on whichever execution the program stands on.
//!
//! What each expectation follows is php's code, since no PHP program
//! reads the number: a fiber switch saves and restores it with the rest
//! of `zend_fiber_vm_state` and a fiber's body starts at
//! [`TraceNum::NONE`], which the natives show as zero
//! (`Z/Zend/zend_fibers.c:105-155`, `:599`), a generator's body runs at
//! `NONE` with its resumer's restored after it
//! (`Z/Zend/zend_generators.c:793-865`), and phpdbg serving a frame below
//! a fiber moves `EG(current_execute_data)` alone
//! (`Z/sapi/phpdbg/phpdbg_frame.c:116`).

use php_macros::php_function;

use crate::api::abi::{Extension, ExtensionBuilder, Observed, ObserverHandlers};
use crate::api::request::Request;
use crate::engine::Engine;
use crate::test::Capture;
use crate::tier::{self, TraceNum};

/// `trace_num(): int`: the running execution's trace number, as the
/// tier reads it.
#[php_function]
fn trace_num(req: &mut Request) -> i64 {
    i64::from(tier::trace_num(req).raw())
}

/// `set_trace_num(int $n): void`: the running execution's trace number
/// written, as the tier writes it around a trace it executes.
#[php_function]
fn set_trace_num(req: &mut Request, n: i64) {
    tier::set_trace_num(
        req,
        TraceNum::from_raw(u32::try_from(n).unwrap_or(u32::MAX)),
    );
}

/// `pause()` begun: `trace_num()` evaluated in the frame below the
/// caller, as a debugger serves a frame below a fiber, and what it read
/// written out.
fn eval_below_caller(observed: &mut Observed<'_>) {
    let Some(frame) = observed
        .caller()
        .and_then(|at| at.caller())
        .map(|at| at.id())
    else {
        observed.response_mut().write(b"[no frame]\n");
        return;
    };
    let read = observed
        .eval_in_frame(frame, b"trace_num()", b"debugger")
        .and_then(Result::ok)
        .and_then(|value| {
            let n = value.as_int();
            value.give_up(observed);
            n
        });
    let line = format!(
        "debugger {}\n",
        read.map_or_else(|| String::from("refused"), |n| n.to_string())
    );
    observed.response_mut().write(line.as_bytes());
}

fn init(observed: &mut Observed<'_>) -> ObserverHandlers {
    let named = observed
        .function()
        .and_then(|function| observed.function_decl(function))
        .is_some_and(|decl| decl.name.0.as_ref() == b"pause");
    ObserverHandlers {
        begin: named.then_some(eval_below_caller as fn(&mut Observed<'_>)),
        end: None,
    }
}

/// The two natives, and the debugger that evaluates at `pause()`, which
/// only the program that declares a `pause()` meets.
fn traced() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![trace_num::declared(), set_trace_num::declared()])
        .observer(init)
        .build("traced")
}

/// What `source` wrote, served with [`traced`] loaded.
fn serve(source: &str) -> String {
    let engine = Engine::cli_builder().extensions(&[traced()]).build();
    let served = engine.serve_capture(source, Capture::default());
    assert_eq!(
        served.exit_code,
        0,
        "{}",
        String::from_utf8_lossy(&served.stdout)
    );
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// A fiber's body reads [`TraceNum::NONE`] whatever its resumer's
/// execution holds; a number set on each execution before a suspend
/// reads again after the resume on that execution and not on the other,
/// and the fiber's last write stays with it when its body returns. A
/// fiber left suspended with its number set leaves its resumer's as it
/// was, which is why the exclusion cannot stick on a request (revision
/// 9).
#[test]
fn a_fiber_switch_saves_each_executions_trace_number_and_a_body_starts_at_none() {
    let out = serve(
        "<?php\nset_trace_num(7);\n\
         $f = new Fiber(function () {\n\
             echo 'body ', trace_num(), \"\\n\";\n\
             set_trace_num(5);\n\
             Fiber::suspend();\n\
             echo 'resumed ', trace_num(), \"\\n\";\n\
             set_trace_num(6);\n\
         });\n\
         $f->start();\n\
         echo 'main ', trace_num(), \"\\n\";\n\
         set_trace_num(9);\n\
         $f->resume();\n\
         echo 'main ', trace_num(), \"\\n\";\n\
         $left = new Fiber(function () { set_trace_num(4); Fiber::suspend(); });\n\
         $left->start();\n\
         echo 'main ', trace_num(), \"\\n\";\n",
    );
    assert_eq!(out, "body 0\nmain 7\nresumed 5\nmain 9\nmain 9\n");
}

/// A fiber started inside a fiber starts at `NONE`, and each of the three
/// executions keeps its own across the suspends between them.
#[test]
fn nested_fibers_each_keep_their_own_trace_number() {
    let out = serve(
        "<?php\nset_trace_num(1);\n\
         $outer = new Fiber(function () {\n\
             set_trace_num(2);\n\
             $inner = new Fiber(function () {\n\
                 echo 'inner ', trace_num(), \"\\n\";\n\
                 set_trace_num(3);\n\
                 Fiber::suspend();\n\
                 echo 'inner ', trace_num(), \"\\n\";\n\
             });\n\
             $inner->start();\n\
             echo 'outer ', trace_num(), \"\\n\";\n\
             Fiber::suspend();\n\
             $inner->resume();\n\
             echo 'outer ', trace_num(), \"\\n\";\n\
         });\n\
         $outer->start();\n\
         echo 'main ', trace_num(), \"\\n\";\n\
         $outer->resume();\n\
         echo 'main ', trace_num(), \"\\n\";\n",
    );
    assert_eq!(out, "inner 0\nouter 2\nmain 1\ninner 3\nouter 2\nmain 1\n");
}

/// A generator's body runs at zero at every resume, and its resumer
/// reads its own number again after each: after a yield, after a
/// `yield from` that hands out a value of the array it delegates to
/// without running the body (`:818`), through a delegating generator,
/// and inside a fiber, where the resumer is the fiber's execution.
#[test]
fn a_generator_resume_restores_the_resumers_trace_number() {
    let out = serve(
        "<?php\n\
         function g() {\n\
             echo 'gen ', trace_num(), \"\\n\";\n\
             set_trace_num(11);\n\
             yield 1;\n\
             echo 'gen ', trace_num(), \"\\n\";\n\
             set_trace_num(12);\n\
             yield 2;\n\
         }\n\
         function from_array() { set_trace_num(13); yield from [1, 2]; }\n\
         function outer() { set_trace_num(14); yield from g(); }\n\
         set_trace_num(3);\n\
         $g = g();\n\
         $g->current();\n\
         echo 'caller ', trace_num(), \"\\n\";\n\
         $g->next();\n\
         echo 'caller ', trace_num(), \"\\n\";\n\
         $a = from_array();\n\
         $a->current();\n\
         $a->next();\n\
         echo 'caller ', trace_num(), \"\\n\";\n\
         $o = outer();\n\
         $o->current();\n\
         $o->next();\n\
         echo 'caller ', trace_num(), \"\\n\";\n\
         $f = new Fiber(function () {\n\
             set_trace_num(5);\n\
             $g = g();\n\
             $g->current();\n\
             echo 'fiber ', trace_num(), \"\\n\";\n\
         });\n\
         $f->start();\n\
         echo 'caller ', trace_num(), \"\\n\";\n",
    );
    assert_eq!(
        out,
        "gen 0\ncaller 3\ngen 0\ncaller 3\ncaller 3\ngen 0\ngen 0\ncaller 3\ngen 0\nfiber 5\ncaller 3\n"
    );
}

/// A generator whose body suspends the fiber it runs in: the number the
/// body set goes with the fiber and comes back at its resume, and the
/// resumer's number the resume backed up waits on the fiber's machine
/// stack through the switch, as php's `original_jit_trace_num` does, so
/// the fiber reads its own when the generator yields.
#[test]
fn a_generator_body_that_suspends_its_fiber_restores_the_fibers_trace_number() {
    let out = serve(
        "<?php\n\
         function g() {\n\
             echo 'gen ', trace_num(), \"\\n\";\n\
             set_trace_num(21);\n\
             Fiber::suspend();\n\
             echo 'gen ', trace_num(), \"\\n\";\n\
             yield 1;\n\
         }\n\
         set_trace_num(3);\n\
         $f = new Fiber(function () {\n\
             set_trace_num(5);\n\
             $g = g();\n\
             $g->current();\n\
             echo 'fiber ', trace_num(), \"\\n\";\n\
         });\n\
         $f->start();\n\
         echo 'main ', trace_num(), \"\\n\";\n\
         set_trace_num(8);\n\
         $f->resume();\n\
         echo 'main ', trace_num(), \"\\n\";\n",
    );
    assert_eq!(out, "gen 0\nmain 3\ngen 21\nfiber 5\nmain 8\n");
}

/// A debugger evaluating in a frame below a fiber serves it on that
/// frame's stack with the running fiber's trace number, as phpdbg moves
/// `EG(current_execute_data)` and nothing else; both executions read their
/// own after it.
#[test]
fn an_evaluation_below_a_fiber_reads_the_running_fibers_trace_number() {
    let out = serve(
        "<?php\nfunction pause() {}\n\
         function outer() {\n\
             set_trace_num(7);\n\
             $f = new Fiber(function () { set_trace_num(5); pause(); echo 'fiber ', trace_num(), \"\\n\"; });\n\
             $f->start();\n\
             echo 'outer ', trace_num(), \"\\n\";\n\
         }\n\
         outer();\n",
    );
    assert_eq!(out, "debugger 5\nfiber 5\nouter 7\n");
}
