//! Frame reads for extensions (#1827): an observer's handler reads the
//! frame it is handed, and the frames below it, through a borrowed
//! [`FrameView`], and keeps a frame past the handler as a [`FrameId`],
//! which [`Frames::of`] checks against the stacks that stand before
//! it reads: a frame that left them is refused, and never read as the
//! frame that stands at its old place now. A generator's frame is
//! taken off the stack at each yield and pushed where its next resumer
//! stands, and a fiber switch swaps the whole stack, so a bare
//! position read either way names another live frame.
//!
//! The extension below keeps the frame that calls `keep()`, and at
//! each call of `read()` writes what the kept frame reads as, between
//! the bytes the program echoes.

use crate::api::abi::{Beneath, Extension, ExtensionBuilder, Observed, ObserverHandlers, chain};
use crate::api::request::{ChainFrame, FrameId, FrameView, Request};
use crate::engine::Engine;
use crate::test::Capture;

/// The frame `keep()` was called from, as the extension's request state.
#[derive(Default)]
struct Kept(Option<FrameId>);

/// The function `view` serves, by its declared name, or the file's
/// base name for an included, evaluated or main file's frame.
fn name(req: &Request, view: &FrameView<'_>) -> String {
    if let Some(function) = view.function() {
        return req
            .symbols()
            .function_decl(function)
            .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned())
            .unwrap_or_default();
    }
    let file = view.filename();
    let base = file.rsplit(|&b| b == b'/').next().unwrap_or(file);
    String::from_utf8_lossy(base).into_owned()
}

fn init(observed: &mut Observed<'_>) -> ObserverHandlers {
    let Some(frame) = observed.frame() else {
        return ObserverHandlers::default();
    };
    let begin: fn(&mut Observed<'_>) = match name(observed, &frame).as_str() {
        "keep" => keep,
        "read" => read,
        _ => return ObserverHandlers::default(),
    };
    ObserverHandlers {
        begin: Some(begin),
        end: None,
    }
}

/// `keep()` begun: its caller's frame kept past the handler.
fn keep(observed: &mut Observed<'_>) {
    let caller = observed
        .caller()
        .map(|view| (view.id(), name(observed, &view)));
    let Some((id, named)) = caller else {
        observed.response_mut().write(b"[kept nothing]");
        return;
    };
    observed.state::<Kept>().0 = Some(id);
    observed
        .response_mut()
        .write(format!("[kept {named}]").as_bytes());
}

/// `read()` begun: the kept frame read through its identity, its
/// function and line, or refused.
fn read(observed: &mut Observed<'_>) {
    let kept = observed.state::<Kept>().0;
    let read = kept.and_then(|id| {
        let view = observed.frames().of(id)?;
        Some(format!("{}:{}", name(observed, &view), view.line()))
    });
    let read = read.unwrap_or_else(|| String::from("refused"));
    observed
        .response_mut()
        .write(format!("[read {read}]").as_bytes());
}

fn keeping() -> Extension {
    ExtensionBuilder::default()
        .request_state::<Kept>()
        .observer(init)
        .build("frame_reads")
}

fn serve(source: &str) -> String {
    let engine = Engine::cli_builder().extensions(&[keeping()]).build();
    let served = engine.serve_capture(source, Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// A generator's frame kept while it runs is refused while the
/// generator is suspended, where the frame of `read()` itself now
/// stands at the place it stood, and read again once a resumer pushes
/// it back, at another depth: the identity is php's `execute_data`,
/// which is one across a generator's resumptions. Once the generator
/// has returned, it is refused for good.
#[test]
fn a_kept_generator_frame_is_refused_while_it_is_suspended_and_after_it_returns() {
    let out = serve(
        "<?php\nfunction keep() {}\nfunction read() {}\n\
         function gen() {\n    keep();\n    read();\n    yield 1;\n    read();\n}\n\
         function deeper($g) { $g->next(); }\n\
         $g = gen();\n$g->current();\necho \"|\";\nread();\necho \"|\";\ndeeper($g);\necho \"|\";\nread();\n",
    );
    assert_eq!(
        out,
        "[kept gen][read gen:6]|[read refused]|[read gen:8]|[read refused]"
    );
}

/// A fiber's frame kept while the fiber runs is refused once it
/// suspends, where the resumer's frame stands at the same position of
/// its own stack, read again when the fiber is resumed, and refused
/// once the fiber has returned.
#[test]
fn a_kept_fiber_frame_is_refused_while_the_fiber_is_suspended() {
    let out = serve(
        "<?php\nfunction keep() {}\nfunction read() {}\n\
         function inner() {\n    keep();\n    read();\n    Fiber::suspend();\n    read();\n}\n\
         $fiber = new Fiber(function () { inner(); });\n\
         $fiber->start();\necho \"|\";\nread();\necho \"|\";\n$fiber->resume();\necho \"|\";\nread();\n",
    );
    assert_eq!(
        out,
        "[kept inner][read inner:6]|[read refused]|[read inner:8]|[read refused]"
    );
}

/// The resumer's frame, kept before a fiber starts, is read from inside
/// the fiber on the stack it stands on, not as the fiber's frame at the
/// same position, and refused once it has returned.
#[test]
fn a_kept_frame_below_a_fiber_is_read_on_its_own_stack() {
    let out = serve(
        "<?php\nfunction keep() {}\nfunction read() {}\n\
         function outer() {\n    keep();\n    $fiber = new Fiber(function () { read(); });\n    $fiber->start();\n}\n\
         outer();\necho \"|\";\nread();\n",
    );
    assert_eq!(out, "[kept outer][read outer:7]|[read refused]");
}

/// The frames below the observed one, walked by [`FrameView::caller`]:
/// php's `prev_execute_data` chain over user frames, through a fiber
/// into the stack that resumed it.
#[test]
fn a_frame_view_walks_its_callers_into_the_resumer_of_a_fiber() {
    fn walk(observed: &mut Observed<'_>) {
        let mut names = Vec::new();
        let mut at = observed.caller();
        while let Some(view) = at {
            names.push(format!("{}:{}", name(observed, &view), view.line()));
            at = view.caller();
        }
        let line = format!("[{}]", names.join(" < "));
        observed.response_mut().write(line.as_bytes());
    }
    fn init(observed: &mut Observed<'_>) -> ObserverHandlers {
        let walks = observed
            .frame()
            .is_some_and(|frame| name(observed, &frame) == "walk");
        ObserverHandlers {
            begin: walks.then_some(walk as fn(&mut Observed<'_>)),
            end: None,
        }
    }
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .observer(init)
            .build("frame_walk")])
        .build();
    let served = engine.serve_capture(
        "<?php\nfunction walk() {}\nfunction f() {\n    $fiber = new Fiber(function () {\n        walk();\n    });\n    $fiber->start();\n}\nf();\n",
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "[{closure:f():4}:5 < f:7 < Standard input code:9]"
    );
}

/// A native's call stands in no frame of the loop's, so
/// [`Observed::frame`] answers none for it, and [`Observed::caller`]
/// answers the user frame it was called from, which a kept identity
/// reads as any other.
#[test]
fn a_native_call_has_no_frame_and_its_caller_is_the_user_frame_it_stands_on() {
    fn native(observed: &mut Observed<'_>) {
        let framed = observed.frame().is_some();
        let caller = observed
            .caller()
            .map(|view| format!("{}:{}", name(observed, &view), view.line()));
        let line = format!(
            "[strcmp framed={framed} from {}]",
            caller.unwrap_or_default()
        );
        observed.response_mut().write(line.as_bytes());
    }
    fn init(observed: &mut Observed<'_>) -> ObserverHandlers {
        let named = observed.frame().is_none()
            && observed
                .function()
                .and_then(|function| observed.symbols().function_decl(function))
                .is_some_and(|decl| decl.name.0.as_ref() == b"strcmp");
        ObserverHandlers {
            begin: named.then_some(native as fn(&mut Observed<'_>)),
            end: None,
        }
    }
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .observer(init)
            .build("frame_native")])
        .build();
    let served = engine.serve_capture(
        "<?php\nfunction f($s) {\n    return strcmp($s, 'abc');\n}\necho f('abc');\n",
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "[strcmp framed=false from f:3]0"
    );
}

/// `zend_test.observer.show_opcode` (`observer_opline_01.phpt`): a begin
/// handler reads the opline its frame stands on, past the `RECV` of a
/// passed argument php skips before the begin, and an end handler the
/// `RETURN` that leaves it; the main file's frame begins at its first
/// opline, and a native's call stands in no frame of the loop's.
#[test]
fn a_handler_reads_the_opcode_its_frame_stands_on() {
    fn init(_: &mut Observed<'_>) -> ObserverHandlers {
        ObserverHandlers {
            begin: Some(begin),
            end: Some(end),
        }
    }
    fn show(observed: &mut Observed<'_>, edge: &str) {
        let req: &Request = observed;
        let Some(view) = observed.frame() else {
            return;
        };
        let line = format!("<{edge} {} {}>", name(req, &view), view.opcode());
        observed.response_mut().write(line.as_bytes());
    }
    fn begin(observed: &mut Observed<'_>) {
        show(observed, "begin");
    }
    fn end(observed: &mut Observed<'_>, _: Option<&crate::api::values::Value>) {
        show(observed, "end");
    }
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default().observer(init).build("showing")])
        .build();
    let dir = super::observer::scratch("opcode_read");
    let served = super::observer::serve_main(
        &engine,
        &dir,
        "<?php\nfunction f($a) {\n    echo $a;\n    return $a;\n}\necho f(1);\nstrlen('x');\n",
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "<begin main.php ZEND_INIT_FCALL><begin f ZEND_ECHO>1<end f ZEND_RETURN>1\
         <end main.php ZEND_RETURN>"
    );
}

/// A frame an uncaught throwable or an `exit` leaves stands at
/// `ZEND_HANDLE_EXCEPTION` for its end handler, php's `EG(exception_op)`,
/// as the oracle's `zend_test.observer.show_opcode` prints it at `b`'s,
/// `d`'s and the main file's ends of this program. `a`'s end comes from
/// the `FAST_RET` that takes the unwind up again past its `finally`,
/// which this engine reads as `ZEND_FAST_RET` and php as the stale
/// `ZEND_ECHO` its last saving handler left.
#[test]
fn an_end_handler_an_uncaught_throwable_calls_reads_the_exception_op() {
    fn init(_: &mut Observed<'_>) -> ObserverHandlers {
        ObserverHandlers {
            begin: None,
            end: Some(end),
        }
    }
    fn end(observed: &mut Observed<'_>, _: Option<&crate::api::values::Value>) {
        let req: &Request = observed;
        let Some(view) = observed.frame() else {
            return;
        };
        let line = format!("<end {} {}>", name(req, &view), view.opcode());
        observed.response_mut().write(line.as_bytes());
    }
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default().observer(init).build("showing")])
        .build();
    let dir = super::observer::scratch("opcode_unwound");
    let served = super::observer::serve_main(
        &engine,
        &dir,
        "<?php\nfunction b() { throw new Exception(\"x\"); }\n\
         function a() { try { b(); } finally { echo \"f\\n\"; } }\n\
         function c() { try { a(); } catch (Exception $e) { echo \"c\\n\"; } return 1; }\n\
         c();\nfunction d() { exit(0); }\nd();\n",
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "<end b ZEND_HANDLE_EXCEPTION>f\n<end a ZEND_FAST_RET>c\n<end c ZEND_RETURN>\
         <end d ZEND_HANDLE_EXCEPTION><end main.php ZEND_HANDLE_EXCEPTION>"
    );
}

/// The `FAST_RET` that takes an uncaught throwable up again past `a`'s
/// `finally` is where `a`'s frame stands for userland too: the
/// foreach's iterated object, freed there as the unwind leaves the
/// loop, reads `a`'s line in its destructor's backtrace as line 12,
/// the `finally`'s last statement, where the `FAST_RET` stands. php
/// reads line 10, the `echo`, the last handler before it that saved
/// its opline: line 11's `JMPZ` takes its fast path without saving
/// one, and `ZEND_FAST_RET` dispatches to
/// `zend_dispatch_try_catch_finally_helper` without `SAVE_OPLINE`.
/// `docs/divergence.toml`'s `trace:fast-ret-frame-opline` row records
/// the difference as php's. The program is #2478's, its `ArrayIterator`
/// a plain object with one property, which this crate serves without
/// ext/spl; the pinned php prints line 10 for either.
#[test]
fn a_backtrace_the_unwind_past_a_finally_calls_reads_its_fast_ret_line() {
    let engine = Engine::cli_builder().build();
    let dir = super::observer::scratch("fast_ret_line");
    let served = super::observer::serve_main(
        &engine,
        &dir,
        r#"<?php
class D { public $p = 1; function __destruct() { echo "dtor sees a() at line ", debug_backtrace()[0]['line'], "\n"; } }
function b() { throw new Exception("x"); }
function a() {
    $t = true;
    foreach (new D as $v) {
        try {
            b();
        } finally {
            echo "f\n";                  // line 10: ECHO saves the opline
            if (!$t) { $u = 2; }       // line 11: JMPZ's fast path saves none
            $t;
        }
    }
}
function c() { try { a(); } catch (Exception $e) { echo "c\n"; } }
c();
"#,
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "f\ndtor sees a() at line 12\nc\n"
    );
}

/// One frame of a walk as `observer_show_init_backtrace` writes it:
/// `C::m`, `f`, or `{main}` and the file's base name, `[no active file]`
/// where it has none; then, for a user frame standing on a stack, the
/// line its [`FrameView`] reads.
fn walked(req: &Request, frame: &ChainFrame<'_>) -> String {
    let named = named(req, frame);
    match frame.frame() {
        Some(view) => format!("{named}:{}", view.line()),
        None => named,
    }
}

fn named(req: &Request, frame: &ChainFrame<'_>) -> String {
    let Some(function) = frame.function() else {
        let file = frame.filename().map_or_else(
            || String::from("[no active file]"),
            |file| {
                let base = file.rsplit(|&b| b == b'/').next().unwrap_or(file);
                String::from_utf8_lossy(base).into_owned()
            },
        );
        return format!("{{main}} {file}");
    };
    let name = req
        .symbols()
        .function_decl(function)
        .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned())
        .unwrap_or_default();
    match frame.function_scope() {
        Some(class) => format!("{}::{name}", req.symbols().class_name(class)),
        None => name,
    }
}

/// Serve `source` under an observer that writes, as each `walk()` or
/// `strcmp()` call begins, the walk from it: every frame php's
/// `prev_execute_data` links, innermost first.
fn walks(source: &str) -> String {
    fn begin(observed: &mut Observed<'_>) {
        let walk: Vec<String> = observed
            .walk()
            .map(|frame| walked(observed, &frame))
            .collect();
        let line = format!("[{}]", walk.join(" < "));
        observed.response_mut().write(line.as_bytes());
    }
    fn init(observed: &mut Observed<'_>) -> ObserverHandlers {
        let walks = observed
            .function()
            .and_then(|function| observed.symbols().function_decl(function))
            .is_some_and(|decl| matches!(decl.name.0.as_ref(), b"walk" | b"strcmp"));
        ObserverHandlers {
            begin: walks.then_some(begin as fn(&mut Observed<'_>)),
            end: None,
        }
    }
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .observer(init)
            .build("frame_chain")])
        .build();
    let served = engine.serve_capture(source, Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// The walk meets the natives between two user frames, `Closure::call()`
/// under the closure it calls and `Generator::current()` under the
/// generator it resumes, which [`FrameView::caller`] steps over:
/// `observer_backtrace_01.phpt`'s chain.
#[test]
fn a_walk_meets_the_natives_between_user_frames() {
    let out = walks(
        "<?php\nfunction walk() {}\n\
         class C { function m() { $c = function () { walk(); return 1; }; return $c->call($this); } }\n\
         function gen() { yield (new C)->m(); }\n\
         function f() { return gen()->current(); }\n\
         f();\n",
    );
    assert_eq!(
        out,
        "[walk:2 < C::{closure:C::m():3}:3 < Closure::call < C::m:3 < gen:4 < Generator::current < f:5 < {main} Standard input code:6]"
    );
}

/// A native's call is where its walk begins, the frames it stands on
/// below it.
#[test]
fn a_walk_from_a_native_call_begins_at_the_call() {
    let out = walks(
        "<?php\nclass C {}\nfunction f() { $c = function () { return strcmp('a', 'b'); }; return $c->call(new C); }\nf();\n",
    );
    assert_eq!(
        out,
        "[strcmp < C::{closure:f():3}:3 < Closure::call < f:3 < {main} Standard input code:4]"
    );
}

/// A destructor a delegated generator's frame runs as it is freed
/// stands on the delegating generator's frame, as php links it, a frame
/// the generator keeps and no stack holds, so it has no [`FrameView`]:
/// `gh16514.phpt`'s chain.
#[test]
fn a_walk_from_a_delegated_generator_meets_the_delegating_one() {
    let out = walks(
        "<?php\nfunction walk() {}\n\
         class Foo { function __destruct() { walk(); } }\n\
         function bar() { yield from foo(); }\n\
         function foo() { $foo = new Foo(); yield; }\n\
         foreach (bar() as $dummy);\n",
    );
    assert_eq!(
        out,
        "[walk:2 < Foo::__destruct:3 < bar < {main} Standard input code:6]"
    );
}

/// Below a fiber's first frame the walk meets the fiber's bottom frame,
/// which names no function and no file, then the stack that resumed
/// the fiber: `observer_fiber_backtrace_crash.phpt`'s chain.
#[test]
fn a_walk_meets_a_fibers_bottom_frame_and_its_resumer() {
    let out = walks(
        "<?php\nfunction walk() {}\n\
         $fiber = new Fiber(function () { walk(); });\n\
         $fiber->start();\n",
    );
    assert_eq!(
        out,
        "[walk:2 < {closure:Standard input code:3}:3 < {main} [no active file] < Fiber::start < {main} Standard input code:4]"
    );
}

/// The trace the `thrown` link below read at the first throw, the
/// tracer's request state, read back by the test once the request ended
/// ([`crate::tier::serve_kept`]).
#[derive(Default)]
struct Traced(Option<String>);

/// A `thrown` link that reads the trace where it is told, as xdebug's
/// develop setting prints one from `zend_throw_exception_hook`.
fn traced_at_throw(req: &mut Request, beneath: Beneath<'_, chain::Throw>) {
    if req.state::<Traced>().0.is_none()
        && let Ok(trace) = req.frames_mut().backtrace_here(0, 0)
    {
        let rendered = req.trace_to_string(*trace, true);
        trace.give_up(req);
        req.state::<Traced>().0 = Some(rendered);
    }
    beneath.serve(req);
}

/// `Generator::throw()` tells the links with the generator's frame
/// current and the caller's below it, as php switches
/// `EG(current_execute_data)` before `zend_throw_exception_object`: the
/// trace a link reads has the generator on top, under no file since the
/// frame below it is the native's, as the oracle's trace of a
/// destructor served at the same place reads (`Generator::throw`'s witness).
#[test]
fn a_thrown_link_at_a_generator_throw_reads_the_generators_frame_on_top() {
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .thrown(traced_at_throw)
            .request_state::<Traced>()
            .build("tracer")])
        .build();
    let (served, traced) = crate::tier::serve_kept::<Traced>(
        &engine,
        &std::env::current_dir().unwrap_or_default(),
        b"Standard input code",
        "<?php\nfunction g() { try { yield 1; } catch (Exception $e) { echo \"caught\\n\"; } }\n\
         $g = g();\n$g->current();\n$g->throw(new Exception(\"e\"));\n",
    );
    assert_eq!(String::from_utf8_lossy(&served.stdout), "caught\n");
    assert_eq!(
        traced.and_then(|traced| traced.0).as_deref(),
        Some(
            "#0 [internal function]: g()\n\
             #1 Standard input code(5): Generator->throw(Object(Exception))\n#2 {main}"
        )
    );
}
