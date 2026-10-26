//! Seam 9, the observer, as `TestExt` records it (design §4.3 row 9,
//! revision 79): php's order and pairing of begin and end for user calls,
//! includes and evals, a function's slot resolved on its first call with
//! the choice to decline, and every open frame ended once by an unwind
//! and by a fatal. The sequences are php's, as `zend_test`'s observer
//! prints them in `Z/ext/zend_test/tests/observer_*.phpt`.

use crate::api::values::ClassId;
use php_macros::php_function;

use super::test_ext::{
    Event, Events, Phase, Recording, TestExt, fiber_name, observed_name, record, returned_name,
};
use crate::api::abi::{
    Beneath, Extension, ExtensionBuilder, Observed, ObservedFiber, ObserverHandlers, chain,
};
use crate::api::call::call;
use crate::api::coerce::callable_checked;
use crate::api::raising::{Level, Thrown, catch_fatal, leave_pending, throw};
use crate::api::request::Request;
use crate::api::values::{Owner, PhpString, Value};
use crate::engine::Engine;
use crate::test::{Capture, Files, Served};

use Event::Ext;
use Phase::{Begin, End, Error, FiberDestroy, FiberInit, FiberSwitch, ObserverInit};

/// The observer's events alone, the fiber notices among them, and the
/// diagnostics `zend_error_cb` was handed among them, in the order
/// recorded.
pub(super) fn observed(events: &Events) -> Vec<Event> {
    events
        .only(&[
            ObserverInit(String::new()),
            Begin(String::new()),
            End(String::new(), None),
            FiberInit(String::new()),
            FiberSwitch(String::new(), String::new()),
            FiberDestroy(String::new()),
            Error(String::new()),
        ])
        .0
}

fn init(n: u8, name: &str) -> Event {
    Ext(n, ObserverInit(name.to_owned()))
}

fn begin(n: u8, name: &str) -> Event {
    Ext(n, Begin(name.to_owned()))
}

fn end(n: u8, name: &str, returned: Option<&str>) -> Event {
    Ext(n, End(name.to_owned(), returned.map(str::to_owned)))
}

/// `source` served as `name` in `files`, written there first, the
/// request named for the file's whole path.
pub(super) fn serve_file(engine: &Engine, files: &Files, name: &str, source: &str) -> Served {
    files.write(name, source);
    let path = files.path().join(name);
    engine.serve_capture_in(
        files.path(),
        path.to_string_lossy().as_bytes(),
        source,
        Capture::default(),
    )
}

/// `source` served as `main.php` in `files` ([`serve_file`]).
pub(super) fn serve_main(engine: &Engine, files: &Files, source: &str) -> Served {
    serve_file(engine, files, "main.php", source)
}

/// The main file's frame as the observer names it, the file
/// [`serve_main`] serves.
pub(super) const MAIN: &str = "file 'main.php'";

/// `source` served as `main.php` ([`serve_main`]) in a fresh `Files`
/// named `name`, by an engine built with `extensions`, and what the
/// observer saw ([`observed`]) from the build on.
pub(super) fn observe(extensions: &[Extension], name: &str, source: &str) -> (Served, Vec<Event>) {
    let rec = Recording::start();
    let engine = Engine::cli_builder().extensions(extensions).build();
    let files = Files::new(name, &[]);
    rec.mark("built");
    let served = serve_main(&engine, &files, source);
    rec.mark("served");
    (served, observed(&rec.events().between("built", "served")))
}

/// `observer_basic_01.phpt`'s order, two observers deep: a function's
/// handlers are resolved on its first call, every observer asked in
/// declaration order, and never again in the request; begin handlers
/// are called in declaration order and end handlers in reverse, each end
/// handed what the frame returned; the main file's frame is observed
/// as a function's is, and ends with the `1` its implicit return gives.
#[test]
fn calls_begin_in_declaration_order_and_end_in_reverse_with_what_they_returned() {
    let (served, events) = observe(
        &[TestExt::<0>::observing(), TestExt::<1>::observing()],
        "observer-calls",
        "<?php\nfunction g($y) { return $y * 2; }\nfunction f($x) { return g($x) + 1; }\necho f(1), f(2);\n",
    );
    assert_eq!(served.stdout_text(), "35");
    assert_eq!(
        events,
        [
            init(0, MAIN),
            init(1, MAIN),
            begin(0, MAIN),
            begin(1, MAIN),
            init(0, "f"),
            init(1, "f"),
            begin(0, "f"),
            begin(1, "f"),
            init(0, "g"),
            init(1, "g"),
            begin(0, "g"),
            begin(1, "g"),
            end(1, "g", Some("2")),
            end(0, "g", Some("2")),
            end(1, "f", Some("3")),
            end(0, "f", Some("3")),
            begin(0, "f"),
            begin(1, "f"),
            begin(0, "g"),
            begin(1, "g"),
            end(1, "g", Some("4")),
            end(0, "g", Some("4")),
            end(1, "f", Some("5")),
            end(0, "f", Some("5")),
            end(1, MAIN, Some("1")),
            end(0, MAIN, Some("1")),
        ]
    );
}

/// php's `zend_observer_fcall_install` over an initialiser that answers
/// no handler: the function is asked of once and never begun or ended,
/// on its first call or after; a method is named by its class, and a
/// callback a native calls (`zend_call_function`), here the error
/// handler, is observed as a call from code is,
/// `observer_zend_call_function_01.phpt`.
#[test]
fn a_function_the_observer_declines_is_asked_of_once_and_never_begun() {
    let (_, events) = observe(
        &[TestExt::<0>::observing()],
        "observer-declined",
        "<?php\nfunction declined() { return 1; }\nclass C { static function m() { return declined(); } }\n\
         C::m(); C::m();\nset_error_handler(function () { return declined(); });\necho $undefined;\n",
    );
    assert_eq!(
        events
            .get(..8)
            .unwrap_or_else(|| panic!("eight events: {events:?}")),
        [
            init(0, MAIN),
            begin(0, MAIN),
            init(0, "C::m"),
            begin(0, "C::m"),
            init(0, "declined"),
            end(0, "C::m", Some("1")),
            begin(0, "C::m"),
            end(0, "C::m", Some("1")),
        ]
    );
    assert_eq!(
        events.iter().filter(|e| **e == init(0, "declined")).count(),
        1,
        "asked of once: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| *e == begin(0, "declined") || *e == end(0, "declined", Some("1"))),
        "never begun or ended: {events:?}"
    );
    assert_eq!(events.last(), Some(&end(0, MAIN, Some("1"))));
}

/// `observer_basic_03.phpt` and `observer_eval_01.phpt`: an included
/// file's frame and an evaluated string's are observed as a function's
/// are, each asked of on its activation, between the includer's begin
/// and end.
#[test]
fn an_include_and_an_eval_are_begun_and_ended_inside_their_includer() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let files = Files::new("observer-include", &[]);
    files.write(
        "inc.php",
        "<?php\nfunction h() { return 'h'; }\nreturn h();\n",
    );
    rec.mark("built");
    let served = serve_main(
        &engine,
        &files,
        "<?php\necho include __DIR__ . '/inc.php';\necho eval('$v = 7; return $v;');\n",
    );
    rec.mark("served");
    assert_eq!(served.stdout_text(), "h7");
    let inc = "file 'inc.php'";
    let evaluated = "file 'main.php(3) : eval()'d code'";
    assert_eq!(
        observed(&rec.events().between("built", "served")),
        [
            init(0, MAIN),
            begin(0, MAIN),
            init(0, inc),
            begin(0, inc),
            init(0, "h"),
            begin(0, "h"),
            end(0, "h", Some("'h'")),
            end(0, inc, Some("'h'")),
            init(0, evaluated),
            begin(0, evaluated),
            end(0, evaluated, Some("7")),
            end(0, MAIN, Some("1")),
        ]
    );
}

/// Begin runs before the parameters are received, as php's
/// `ZEND_OBSERVER_FCALL_BEGIN` stands in `ZEND_DO_UCALL` before the
/// callee's `RECV` runs: a `RECV` that throws ends the frame it began,
/// with no value, from the unwind, for a call from code and for one a
/// native makes alike, here the error handler `zend_error` calls. The
/// native that sets the handler is observed as any native is.
#[test]
fn begin_runs_before_the_parameters_are_received() {
    let (_, events) = observe(
        &[TestExt::<0>::observing()],
        "observer-recv",
        "<?php\nfunction t(array $a) { return $a; }\n\
         try { t(1); } catch (TypeError $e) { echo 'a'; }\n\
         set_error_handler('t');\ntry { echo $u; } catch (TypeError $e) { echo 'b'; }\n",
    );
    assert_eq!(
        events,
        [
            init(0, MAIN),
            begin(0, MAIN),
            init(0, "t"),
            begin(0, "t"),
            end(0, "t", None),
            init(0, "set_error_handler"),
            begin(0, "set_error_handler"),
            end(0, "set_error_handler", Some("null")),
            begin(0, "t"),
            end(0, "t", None),
            end(0, MAIN, Some("1")),
        ]
    );
}

/// An unwind ends every frame it leaves once, with no value, from the
/// innermost out, where php's uncaught arm of
/// `zend_dispatch_try_catch_finally_helper` ends it: through a `catch`
/// further out, as the throwable leaves the main file uncaught
/// (`observer_exception_01.phpt`, where the file ends before the fatal
/// is rendered), and as `exit` unwinds. The natives the throw and the
/// `exit` call are ended first: `Exception::__construct` as it returns,
/// and `exit` as it throws, with no value, as `ZEND_DO_ICALL` ends a
/// native under `EG(exception)`; the uncaught throwable's `__toString`
/// after the file.
#[test]
fn an_unwind_ends_every_frame_it_leaves_once() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let files = Files::new("observer-unwind", &[]);
    let nested = "<?php\nfunction b() { throw new Exception('x'); }\nfunction a() { b(); }\n";

    rec.mark("caught");
    serve_main(
        &engine,
        &files,
        &format!("{nested}try {{ a(); }} catch (Exception $e) {{ echo 'c'; }}\n"),
    );
    rec.mark("uncaught");
    serve_main(&engine, &files, &format!("{nested}a();\n"));
    rec.mark("exited");
    serve_main(
        &engine,
        &files,
        "<?php\nfunction b() { exit(3); }\nfunction a() { b(); }\na();\n",
    );
    rec.mark("end");
    let unwound = |events: &Events| -> Vec<Event> {
        observed(events)
            .into_iter()
            .filter(|e| matches!(e, Ext(_, End(..))))
            .collect()
    };
    let events = rec.events();
    assert_eq!(
        unwound(&events.between("caught", "uncaught")),
        [
            end(0, "Exception::__construct", Some("null")),
            end(0, "b", None),
            end(0, "a", None),
            end(0, MAIN, Some("1"))
        ]
    );
    let uncaught = observed(&events.between("uncaught", "exited"));
    assert_eq!(
        unwound(&events.between("uncaught", "exited"))
            .get(..4)
            .expect("four frames unwound"),
        [
            end(0, "Exception::__construct", Some("null")),
            end(0, "b", None),
            end(0, "a", None),
            end(0, MAIN, None)
        ]
    );
    // `zend_exception_error` renders the throwable through its
    // `__toString`, which `zend_call_function` serves after the file has
    // ended, and which reaches `getTraceAsString` through a call of its
    // own (`Z/Zend/zend_exceptions.c:731-733`), as
    // `observer_exception_01.phpt` shows it.
    let file_ended = uncaught
        .iter()
        .position(|e| *e == end(0, MAIN, None))
        .unwrap_or_else(|| panic!("the file ends: {uncaught:?}"));
    let to_string = "Exception::__toString";
    let trace = "Exception::getTraceAsString";
    assert_eq!(
        uncaught[file_ended + 1..]
            .iter()
            .filter(|e| !matches!(e, Ext(_, Error(_))))
            .map(|e| match e {
                Ext(n, End(name, Some(_))) => Ext(*n, End(name.clone(), Some(String::new()))),
                other => other.clone(),
            })
            .collect::<Vec<_>>(),
        [
            init(0, to_string),
            begin(0, to_string),
            init(0, trace),
            begin(0, trace),
            end(0, trace, Some("")),
            end(0, to_string, Some("")),
        ],
        "{uncaught:?}"
    );
    let rendered = uncaught
        .iter()
        .position(|e| matches!(e, Ext(_, Error(m)) if m.starts_with("Uncaught Exception: x")));
    assert!(
        rendered.is_some_and(|rendered| file_ended < rendered),
        "the main file ends before the uncaught throwable is rendered: {uncaught:?}"
    );
    assert_eq!(
        unwound(&events.between("exited", "end")),
        [
            end(0, "exit", None),
            end(0, "b", None),
            end(0, "a", None),
            end(0, MAIN, None)
        ]
    );
}

/// A fatal ends every open frame once, with no value, the innermost
/// first, after the fatal is rendered, as php's `zend_observer_fcall_end_all`
/// does at `php_request_shutdown` (`observer_error_01.phpt`), and before
/// a shutdown function runs: a fatal the loop raises, the memory limit
/// a concatenation reaches, and one a native raises, `trigger_error`'s
/// `E_USER_ERROR`, inside the error handler `zend_error` called, where
/// the native is the innermost open call and ended first; and
/// the time limit reached by a function a generator's body called,
/// which the loop raises.
#[test]
fn a_bailout_ends_every_open_frame_once_after_the_fatal() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let files = Files::new("observer-bailout", &[]);

    rec.mark("memory");
    serve_main(
        &engine,
        &files,
        "<?php\nini_set('memory_limit', '2M');\n\
         function s() { echo 's'; }\nregister_shutdown_function('s');\n\
         function b() { $t = '.'; for ($i = 0; $i < 16; $i++) { $t .= $t; } $s = ''; while (true) { $s .= $t; } }\nfunction a() { return b(); }\na();\n",
    );
    rec.mark("user");
    serve_main(
        &engine,
        &files,
        "<?php\nfunction b($no, $str) { trigger_error('boom', E_USER_ERROR); }\n\
         function a() { set_error_handler('b'); echo $u; }\na();\n",
    );
    rec.mark("generator");
    serve_main(
        &engine,
        &files,
        "<?php\nset_time_limit(1);\n\
         function b() { while (true); }\n\
         function g() { b(); yield 1; }\nfunction a() { foreach (g() as $v) {} }\na();\n",
    );
    rec.mark("end");
    let events = rec.events();

    let memory = observed(&events.between("memory", "user"));
    let fatal = memory
        .iter()
        .position(|e| matches!(e, Ext(_, Error(m)) if m.starts_with("Allowed memory size")))
        .unwrap_or_else(|| panic!("the fatal is rendered: {memory:?}"));
    let after: Vec<_> = memory.iter().skip(fatal + 1).cloned().collect();
    assert_eq!(
        after,
        [
            end(0, "b", None),
            end(0, "a", None),
            end(0, MAIN, None),
            init(0, "s"),
            begin(0, "s"),
            end(0, "s", Some("null")),
        ],
        "every open frame ended once after the fatal, before the shutdown function: {memory:?}"
    );

    let user = observed(&events.between("user", "generator"));
    let fatal = user
        .iter()
        .position(|e| matches!(e, Ext(_, Error(m)) if m == "boom"))
        .unwrap_or_else(|| panic!("the fatal is rendered: {user:?}"));
    assert_eq!(
        user.iter().skip(fatal + 1).cloned().collect::<Vec<_>>(),
        [
            end(0, "trigger_error", None),
            end(0, "b", None),
            end(0, "a", None),
            end(0, MAIN, None)
        ],
        "every open frame ended once after the fatal: {user:?}"
    );
    for name in ["a", "b", "trigger_error", MAIN] {
        let begun = user.iter().filter(|e| **e == begin(0, name)).count();
        let ended = user
            .iter()
            .filter(|e| matches!(e, Ext(_, End(n, _)) if n == name))
            .count();
        assert_eq!((begun, ended), (1, 1), "{name}: {user:?}");
    }

    // A frame a generator's body called stands above the generator's
    // frame, which the resumption pops on its way out of the fatal: it
    // is ended before that, after the fatal is rendered, and the
    // generator's frame, begun at its resumption, after it.
    let resumed = observed(&events.between("generator", "end"));
    let fatal = resumed
        .iter()
        .position(|e| matches!(e, Ext(_, Error(m)) if m.starts_with("Maximum execution time")))
        .unwrap_or_else(|| panic!("the fatal is rendered: {resumed:?}"));
    assert_eq!(
        resumed.iter().skip(fatal + 1).cloned().collect::<Vec<_>>(),
        [
            end(0, "b", None),
            end(0, "g", None),
            end(0, "a", None),
            end(0, MAIN, None)
        ],
        "every open frame ended once after the fatal: {resumed:?}"
    );
}

/// A fatal in a shutdown function or in a destructor the shutdown calls
/// ends none of the frames it leaves open, as php ends them only at
/// `php_request_shutdown`'s first step, which has passed (#2015); a
/// fatal in the program before them is ended there, before the
/// shutdown function runs. php's sequence, read from the oracle's
/// zend-test variant with `zend_test.observer.observe_all`: the file's
/// frames ended after the program's fatal, then `sd`, `inner2` and
/// `trigger_error` begun and the second fatal rendered, and nothing
/// after it; and a destructor's `inner` and `trigger_error` begun, the
/// fatal rendered, and nothing after it.
#[test]
fn a_fatal_in_the_shutdown_leaves_its_frames_open() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let files = Files::new("observer-shutdown-fatal", &[]);
    rec.mark("shutdown function");
    serve_main(
        &engine,
        &files,
        "<?php\nfunction inner() { trigger_error('in main', E_USER_ERROR); }\n\
         function outer() { inner(); }\nfunction sd() { inner2(); }\n\
         function inner2() { trigger_error('in shutdown', E_USER_ERROR); }\n\
         function sd2() { echo 'not reached'; }\n\
         register_shutdown_function('sd');\nregister_shutdown_function('sd2');\nouter();\n",
    );
    rec.mark("destructor");
    serve_main(
        &engine,
        &files,
        "<?php\nfunction inner() { trigger_error('in destructor', E_USER_ERROR); }\n\
         class D { function __destruct() { inner(); } }\n$d = new D;\n",
    );
    rec.mark("end");
    let events = rec.events();

    let shutdown = observed(&events.between("shutdown function", "destructor"));
    let fatal = shutdown
        .iter()
        .position(|e| matches!(e, Ext(_, Error(m)) if m == "in main"))
        .unwrap_or_else(|| panic!("{shutdown:?}"));
    assert_eq!(
        shutdown[fatal + 1..]
            .iter()
            .filter(|e| !matches!(e, Ext(_, Error(m)) if m.starts_with("Passing E_USER_ERROR")))
            .cloned()
            .collect::<Vec<_>>(),
        [
            end(0, "trigger_error", None),
            end(0, "inner", None),
            end(0, "outer", None),
            end(0, MAIN, None),
            init(0, "sd"),
            begin(0, "sd"),
            init(0, "inner2"),
            begin(0, "inner2"),
            begin(0, "trigger_error"),
            Ext(0, Error(String::from("in shutdown"))),
        ],
        "{shutdown:?}"
    );

    let destructor = observed(&events.between("destructor", "end"));
    let fatal = destructor
        .iter()
        .position(|e| matches!(e, Ext(_, Error(m)) if m == "in destructor"));
    assert_eq!(
        fatal.map(|at| at + 1),
        Some(destructor.len()),
        "nothing is ended after the destructor's fatal: {destructor:?}"
    );
    for name in ["D::__destruct", "inner", "trigger_error"] {
        assert!(destructor.contains(&begin(0, name)), "{destructor:?}");
    }
}

/// `frameless_probe(string $string): bool`, frameless for one argument:
/// whether `$string` is empty. Core declares no frameless entry this
/// engine's compiler emits (#1810's comment on `class_exists`), so the
/// observer's tests declare their own.
#[php_function(frameless(1))]
fn frameless_probe(string: PhpString) -> bool {
    string.as_bytes().is_empty()
}

/// An extension declaring [`frameless_probe`] and nothing else.
fn probe() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![frameless_probe::declared()])
        .build("observer_probe")
}

/// Natives are observed as php observes its internal functions: each
/// asked of on its first call in the request, begun before its entry and
/// ended after it with what it returned (`ZEND_DO_ICALL`,
/// `Z/Zend/zend_vm_def.h:4126`, `:4141`); a native method named by its
/// class; and the user function a native calls begun and ended inside
/// it, here the error handler `trigger_error` calls.
#[test]
fn natives_are_begun_before_their_entry_and_ended_after_it() {
    let (served, events) = observe(
        &[TestExt::<0>::observing()],
        "observer-natives",
        "<?php\nfunction f($s) { return strcmp($s, 'a'); }\n\
         function h($no, $str) { return f($str) === 0; }\n\
         echo f('b'), (new Exception('m'))->getMessage();\n\
         set_error_handler('h');\ntrigger_error('a');\n",
    );
    assert_eq!(served.stdout_text(), "1m");
    assert_eq!(
        events,
        [
            init(0, MAIN),
            begin(0, MAIN),
            init(0, "f"),
            begin(0, "f"),
            init(0, "strcmp"),
            begin(0, "strcmp"),
            end(0, "strcmp", Some("1")),
            end(0, "f", Some("1")),
            init(0, "Exception::__construct"),
            begin(0, "Exception::__construct"),
            end(0, "Exception::__construct", Some("null")),
            init(0, "Exception::getMessage"),
            begin(0, "Exception::getMessage"),
            end(0, "Exception::getMessage", Some("'m'")),
            init(0, "set_error_handler"),
            begin(0, "set_error_handler"),
            end(0, "set_error_handler", Some("null")),
            init(0, "trigger_error"),
            begin(0, "trigger_error"),
            init(0, "h"),
            begin(0, "h"),
            begin(0, "f"),
            begin(0, "strcmp"),
            end(0, "strcmp", Some("0")),
            end(0, "f", Some("0")),
            end(0, "h", Some("bool")),
            end(0, "trigger_error", Some("bool")),
            end(0, MAIN, Some("1")),
        ]
    );
}

/// A native that throws is ended at once, before the unwind ends the
/// frames it leaves, and is handed what the path that called it hands:
/// no value from a call opcode (`EG(exception) ? NULL : ret`,
/// `Z/Zend/zend_vm_def.h:4141`), also where what threw was the error
/// handler the native's notice called, and the `null` its return value
/// was laid down with from `zend_call_function`
/// (`Z/Zend/zend_execute_API.c:1039`), here that error handler, a
/// native, and from a frameless call, which php serves as a full call
/// where the callee is observed (`zend_frameless_observed_call`,
/// `Z/Zend/zend_execute.c:1597`).
#[test]
fn a_native_that_throws_is_ended_before_the_frames_the_throw_leaves() {
    let (served, events) = observe(
        &[TestExt::<0>::observing(), probe()],
        "observer-native-throw",
        "<?php\n$a = [];\nfunction f($s) { return strcmp($s, 'a'); }\n\
         try { f($a); } catch (TypeError $e) { echo 'c'; }\n\
         try { frameless_probe($a); } catch (TypeError $e) { echo 'd'; }\n\
         set_error_handler('strcmp');\n\
         try { trigger_error('x'); } catch (ArgumentCountError $e) { echo 'e'; }\n",
    );
    assert_eq!(served.stdout_text(), "cde");
    assert_eq!(
        events,
        [
            init(0, MAIN),
            begin(0, MAIN),
            init(0, "f"),
            begin(0, "f"),
            init(0, "strcmp"),
            begin(0, "strcmp"),
            end(0, "strcmp", None),
            end(0, "f", None),
            init(0, "frameless_probe"),
            begin(0, "frameless_probe"),
            end(0, "frameless_probe", Some("null")),
            init(0, "set_error_handler"),
            begin(0, "set_error_handler"),
            end(0, "set_error_handler", Some("null")),
            init(0, "trigger_error"),
            begin(0, "trigger_error"),
            begin(0, "strcmp"),
            end(0, "strcmp", Some("null")),
            end(0, "trigger_error", None),
            end(0, MAIN, Some("1")),
        ]
    );
}

/// `observer_frameless.phpt`: a frameless call's native is asked of on
/// its first call and not again, and, observed, is begun and ended on
/// every call as a full call is.
#[test]
fn a_frameless_call_is_observed_as_a_full_call() {
    let (served, events) = observe(
        &[TestExt::<0>::observing(), probe()],
        "observer-frameless",
        "<?php\nfunction f($s) { return \\frameless_probe($s); }\nf('a'); f('');\n",
    );
    assert_eq!(served.stdout_text(), "");
    assert_eq!(
        events,
        [
            init(0, MAIN),
            begin(0, MAIN),
            init(0, "f"),
            begin(0, "f"),
            init(0, "frameless_probe"),
            begin(0, "frameless_probe"),
            end(0, "frameless_probe", Some("bool")),
            end(0, "f", Some("bool")),
            begin(0, "f"),
            begin(0, "frameless_probe"),
            end(0, "frameless_probe", Some("bool")),
            end(0, "f", Some("bool")),
            end(0, MAIN, Some("1")),
        ]
    );
}

/// For each of sixteen frame sizes, a function of `$k` locals more than
/// the last: the depth at which its recursion first opens a second VM
/// stack page with no call at its bottom, found by halving, and with a
/// frameless call there, which is that depth or the one before it,
/// `k: alone called`.
const AT_THE_PAGE: &str = "<?php\n\
    function opens($f, $d, $call) { memory_reset_peak_usage(); $base = memory_get_usage(); $f($d, $call); return memory_get_peak_usage() - $base >= 262144; }\n\
    for ($k = 0; $k < 16; $k++) {\n\
    $locals = ''; for ($i = 0; $i < $k; $i++) { $locals .= \"\\$v$i = 0; \"; }\n\
    eval(\"function f$k(\\$n, \\$call) { $locals return \\$n ? f$k(\\$n - 1, \\$call) : (\\$call ? frameless_probe('a' . \\$n) : 0); }\");\n\
    $lo = 1; $hi = 4096;\n\
    while ($lo < $hi) { $mid = ($lo + $hi) >> 1; if (opens(\"f$k\", $mid, false)) { $hi = $mid; } else { $lo = $mid + 1; } }\n\
    echo $k, ': ', $lo, ' ', opens(\"f$k\", $lo - 1, true) ? $lo - 1 : $lo, \"\\n\"; }\n";

/// An observer of every function with a begin handler that does nothing.
fn watching(_: &mut Observed<'_>) -> ObserverHandlers {
    ObserverHandlers {
        begin: Some(|_| {}),
        end: None,
    }
}

/// An observer that declines every function.
fn declining(_: &mut Observed<'_>) -> ObserverHandlers {
    ObserverHandlers::default()
}

/// [`AT_THE_PAGE`] served beside `observer`: each frame size's two
/// depths.
fn depths_at_the_page(observer: fn(&mut Observed<'_>) -> ObserverHandlers) -> Vec<(u32, u32)> {
    let engine = Engine::cli_builder()
        .extensions(&[
            ExtensionBuilder::default()
                .observer(observer)
                .build("observer_room"),
            probe(),
        ])
        .build();
    let served = engine.serve_capture(AT_THE_PAGE, Capture::default());
    served
        .stdout_text()
        .lines()
        .filter_map(|line| {
            let (_, depths) = line.split_once(": ")?;
            let (alone, called) = depths.split_once(' ')?;
            Some((alone.parse().ok()?, called.parse().ok()?))
        })
        .collect()
}

/// A frameless call an observer watches is php's full call
/// (`zend_frameless_observed_call`), its frame pushed on the VM stack,
/// so where the recursion leaves less room on the page than that frame
/// takes it opens the next page a depth sooner than the recursion
/// alone; one every observer declined takes no room at any frame size,
/// as with no observer at all.
#[test]
fn an_observed_frameless_call_takes_a_full_calls_room_and_a_declined_one_none() {
    let declined = depths_at_the_page(declining);
    assert_eq!(declined.len(), 16, "{declined:?}");
    assert!(
        declined.iter().all(|(alone, called)| alone == called),
        "{declined:?}"
    );
    let watched = depths_at_the_page(watching);
    assert_eq!(watched.len(), 16, "{watched:?}");
    assert!(
        watched.iter().all(|(alone, called)| called <= alone),
        "{watched:?}"
    );
    assert!(
        watched.iter().any(|(alone, called)| called < alone),
        "{watched:?}"
    );
}

/// A native's room under a registered observer is php's, its arguments
/// and the temporary `zend_observer_post_startup` gives every native
/// (`++zif->T`), so a recursion one slot short of a page's end
/// that ends in a native call opens the next page where php's does
/// (#2755); an observer its start does not register adds none, as with
/// none declared.
#[test]
fn a_natives_room_takes_the_observers_temporary_where_one_is_registered() {
    let rooms: [(crate::api::abi::ObserverRegistered, usize); 2] = [(|_| true, 3), (|_| false, 2)];
    for (registered, room) in rooms {
        let engine = Engine::cli_builder()
            .extensions(&[ExtensionBuilder::default()
                .observer_when(declining, registered)
                .build("observer_room")])
            .build();
        let req = Request::new(
            std::sync::Arc::clone(&engine.shared),
            crate::engine::RequestDescription::default(),
        );
        let native = req.function_named(php_unit::decl::Bare::resolved(b"strlen"));
        assert_eq!(native.map(|f| req.call_room(f, 2).0), Some(room));
    }
}

/// A user frame under a registered observer takes the temporary php's
/// `pass_two` reserves for it too (`op_array->T += ZEND_OBSERVER_ENABLED`,
/// #3065): a recursion opens its next VM stack page at a lesser depth
/// than beside an observer its start does not register, which takes no
/// room, as none declared takes none.
#[test]
fn a_user_frame_takes_the_observers_temporary_where_one_is_registered() {
    const DEPTH: &str = "<?php\n\
        function f($n) { return $n ? f($n - 1) : 0; }\n\
        function opens($d) { memory_reset_peak_usage(); $base = memory_get_usage(); f($d); return memory_get_peak_usage() - $base >= 262144; }\n\
        $lo = 1; $hi = 8192;\n\
        while ($lo < $hi) { $mid = ($lo + $hi) >> 1; if (opens($mid)) { $hi = $mid; } else { $lo = $mid + 1; } }\n\
        echo $lo;";
    let depth = |registered: crate::api::abi::ObserverRegistered| {
        let engine = Engine::cli_builder()
            .extensions(&[ExtensionBuilder::default()
                .observer_when(declining, registered)
                .build("observer_room")])
            .build();
        let served = engine.serve_capture(DEPTH, Capture::default());
        served.stdout_text().parse::<u32>().ok()
    };
    let (registered, declined) = (depth(|_| true), depth(|_| false));
    assert!(
        registered.zip(declined).is_some_and(|(on, off)| on < off),
        "{registered:?} {declined:?}"
    );
}

/// An end handler that records whether a throwable stands pending as it
/// is called, under extension number 9.
fn pending_init(_: &mut Observed<'_>) -> ObserverHandlers {
    ObserverHandlers {
        begin: None,
        end: Some(pending_end),
    }
}

fn pending_end(observed: &mut Observed<'_>, returned: Option<&Value>) {
    let pending = observed.is_pending();
    record(
        9,
        End(
            format!("{} pending {pending}", observed_name(observed)),
            returned.map(returned_name),
        ),
    );
}

/// The throwable a native left stands pending while its end handlers
/// are called, as it stands in `EG(exception)` at php's
/// `ZEND_OBSERVER_FCALL_END` (`Z/Zend/zend_vm_def.h:4141`), and as it
/// stands for the frames the unwind then ends; it is still the caller's
/// to catch after.
#[test]
fn a_natives_throwable_stands_pending_while_its_end_handlers_are_called() {
    let (served, events) = observe(
        &[ExtensionBuilder::default()
            .observer(pending_init)
            .build("observer_pending")],
        "observer-pending",
        "<?php\nfunction f($s) { return strcmp($s, 'a'); }\n\
         try { f([]); } catch (TypeError $e) { echo 'c'; }\n",
    );
    assert_eq!(served.stdout_text(), "c");
    assert_eq!(
        events,
        [
            Ext(9, End(String::from("strcmp pending true"), None)),
            Ext(9, End(String::from("f pending true"), None)),
            Ext(
                9,
                End(
                    String::from("file 'main.php' pending false"),
                    Some(String::from("1"))
                )
            ),
        ]
    );
}

/// The fibers' names as the creation notices render them, without
/// their status, and the closures' as the observer was asked of them,
/// each in the order they came: the names a test's expectation is
/// written over.
fn named_in(events: &[Event]) -> (Vec<String>, Vec<String>) {
    let fibers = events
        .iter()
        .filter_map(|e| match e {
            Ext(_, FiberInit(name)) => name.strip_suffix(" init").map(str::to_owned),
            _ => None,
        })
        .collect();
    let closures = events
        .iter()
        .filter_map(|e| match e {
            Ext(_, ObserverInit(name)) if name.starts_with("{closure") => Some(name.clone()),
            _ => None,
        })
        .collect();
    (fibers, closures)
}

/// The one fiber and the one closure [`named_in`] finds, held to be
/// one each.
fn one_named_in(events: &[Event]) -> (String, String) {
    let (fibers, closures) = named_in(events);
    assert!(
        fibers.len() == 1 && closures.len() == 1 && fibers.iter().all(|f| f.starts_with('#')),
        "one fiber and one closure: {events:?}"
    );
    (fibers.concat(), closures.concat())
}

fn fiber_init(n: u8, context: &str) -> Event {
    Ext(n, FiberInit(context.to_owned()))
}

fn switch(n: u8, from: &str, to: &str) -> Event {
    Ext(n, FiberSwitch(from.to_owned(), to.to_owned()))
}

fn destroy(n: u8, context: &str) -> Event {
    Ext(n, FiberDestroy(context.to_owned()))
}

/// What a program observed from its file's begin to `Fiber::start`'s
/// end, where it makes a fiber over a closure that calls `inner`, which
/// suspends at once; `fiber` names the fiber in a status.
fn fiber_started(file: &str, fiber: &impl Fn(&str) -> String, closure: &str) -> Vec<Event> {
    vec![
        init(0, file),
        begin(0, file),
        init(0, "Fiber::__construct"),
        begin(0, "Fiber::__construct"),
        end(0, "Fiber::__construct", Some("null")),
        init(0, "Fiber::start"),
        begin(0, "Fiber::start"),
        fiber_init(0, &fiber("init")),
        switch(0, "main running", &fiber("init")),
        init(0, closure),
        begin(0, closure),
        init(0, "inner"),
        begin(0, "inner"),
        init(0, "Fiber::suspend"),
        begin(0, "Fiber::suspend"),
        switch(0, &fiber("running"), "main suspended"),
        end(0, "Fiber::start", Some("null")),
    ]
}

/// A generator's frame is begun at each resumption and ended at each
/// suspension with what it yielded, and not at its call
/// (`observer_generator_01.phpt` to `_03`): the frame's slot is
/// resolved at its first resumption; a `yield from` another generator
/// ends the delegating frame with what it yielded last and resumes the
/// one delegated to, whose `return` ends it with what it returned, and
/// the delegating frame's own `return` ends it with its own.
#[test]
fn a_generator_is_begun_at_each_resumption_and_ended_at_each_yield() {
    let (served, events) = observe(
        &[TestExt::<0>::observing()],
        "observer-generator",
        "<?php\nfunction bar() { for ($i = 10; $i < 12; $i++) { yield $i; } }\n\
         function foo() { yield 0; yield from bar(); yield 42; return 1337; }\n\
         function run() { $g = foo(); foreach ($g as $v) { echo $v, ','; } return $g->getReturn(); }\n\
         echo run();\n",
    );
    assert_eq!(served.stdout_text(), "0,10,11,42,1337");
    assert_eq!(
        events,
        [
            init(0, MAIN),
            begin(0, MAIN),
            init(0, "run"),
            begin(0, "run"),
            init(0, "foo"),
            begin(0, "foo"),
            end(0, "foo", Some("0")),
            begin(0, "foo"),
            end(0, "foo", Some("0")),
            init(0, "bar"),
            begin(0, "bar"),
            end(0, "bar", Some("10")),
            begin(0, "bar"),
            end(0, "bar", Some("11")),
            begin(0, "bar"),
            end(0, "bar", Some("null")),
            begin(0, "foo"),
            end(0, "foo", Some("42")),
            begin(0, "foo"),
            end(0, "foo", Some("1337")),
            init(0, "Generator::getReturn"),
            begin(0, "Generator::getReturn"),
            end(0, "Generator::getReturn", Some("1337")),
            end(0, "run", Some("1337")),
            end(0, MAIN, Some("1")),
        ]
    );
}

/// A generator resumed by `current()`, `next()` and `send()` rather than
/// by `foreach` (`observer_generator_04.phpt`): its first read runs it
/// to its first `yield`, each `next()` and the `send()` is one
/// resumption, and a read of a generator not resumed again begins
/// nothing but the read.
#[test]
fn a_generator_resumed_by_hand_pairs_each_resumption() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let files = Files::new("observer-generator-by-hand", &[]);
    rec.mark("by_hand");
    let served = serve_main(
        &engine,
        &files,
        "<?php\nfunction foo() { $i = 0; while (true) { if (yield $i++) break; } }\n\
         function run() { $g = foo();\n\
         while ($g->current() !== null) { echo $g->current(); if ($g->current() === 2) { $g->send('Boop'); } $g->next(); }\n\
         return 'Done'; }\necho run();\n",
    );
    rec.mark("end");
    assert_eq!(served.stdout_text(), "012Done");
    let events = rec.events();
    // A native read of the generator, begun and ended with what it gave.
    let read = |name: &str, gave: &str| [begin(0, name), end(0, name, Some(gave))];
    let current = |gave: &str| read("Generator::current", gave);
    let mut expected = vec![
        init(0, MAIN),
        begin(0, MAIN),
        init(0, "run"),
        begin(0, "run"),
        init(0, "Generator::current"),
        begin(0, "Generator::current"),
        init(0, "foo"),
        begin(0, "foo"),
        end(0, "foo", Some("0")),
        end(0, "Generator::current", Some("0")),
    ];
    expected.extend(current("0"));
    expected.extend(current("0"));
    expected.extend([
        init(0, "Generator::next"),
        begin(0, "Generator::next"),
        begin(0, "foo"),
        end(0, "foo", Some("1")),
        end(0, "Generator::next", Some("null")),
    ]);
    for _ in 0..3 {
        expected.extend(current("1"));
    }
    expected.extend([
        begin(0, "Generator::next"),
        begin(0, "foo"),
        end(0, "foo", Some("2")),
        end(0, "Generator::next", Some("null")),
    ]);
    for _ in 0..3 {
        expected.extend(current("2"));
    }
    expected.extend([
        init(0, "Generator::send"),
        begin(0, "Generator::send"),
        begin(0, "foo"),
        end(0, "foo", Some("null")),
        end(0, "Generator::send", Some("null")),
    ]);
    expected.extend(read("Generator::next", "null"));
    expected.extend(current("null"));
    expected.extend([end(0, "run", Some("'Done'")), end(0, MAIN, Some("1"))]);
    assert_eq!(observed(&events.between("by_hand", "end")), expected);
}

/// A generator that throws out of its body (`observer_generator_05.phpt`)
/// is ended with nothing, as every frame the throwable leaves is, after
/// the native that made the throwable.
#[test]
fn a_generator_left_by_a_throwable_is_ended_with_nothing() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let files = Files::new("observer-generator-thrown", &[]);
    rec.mark("thrown");
    serve_main(
        &engine,
        &files,
        "<?php\nfunction foo() { yield 0; yield 1; throw new Exception('Oops!'); }\n\
         function run() { foreach (foo() as $v) { echo $v; } return 'unseen'; }\necho run();\n",
    );
    rec.mark("end");
    let events = rec.events();
    // Up to the main file's end; what renders the throwable after it,
    // `Exception::__toString`, is the natives' (#1810).
    let thrown = observed(&events.between("thrown", "end"));
    let rendered = thrown
        .iter()
        .position(|e| matches!(e, Ext(_, Error(m)) if m.starts_with("Uncaught Exception: Oops!")));
    assert!(rendered.is_some(), "the throwable is rendered: {thrown:?}");
    let file_ended = thrown
        .iter()
        .position(|e| *e == end(0, MAIN, None))
        .map_or(0, |at| at + 1);
    assert_eq!(
        thrown[..file_ended],
        [
            init(0, MAIN),
            begin(0, MAIN),
            init(0, "run"),
            begin(0, "run"),
            init(0, "foo"),
            begin(0, "foo"),
            end(0, "foo", Some("0")),
            begin(0, "foo"),
            end(0, "foo", Some("1")),
            begin(0, "foo"),
            init(0, "Exception::__construct"),
            begin(0, "Exception::__construct"),
            end(0, "Exception::__construct", Some("null")),
            end(0, "foo", None),
            end(0, "run", None),
            end(0, MAIN, None),
        ]
    );
}

/// php's fiber notices (`observer_fiber_01.phpt`,
/// `observer_fiber_functions_01.phpt`): the creation as the start lays
/// the stack down, each switch handed the fiber left, running or dead,
/// and the fiber entered, new or suspended, and the destruction once
/// the dead fiber's resumer is served again; and a frame begun inside
/// the fiber is ended inside it, after the switch back into it.
#[test]
fn a_fiber_is_noticed_at_its_creation_each_switch_and_its_destruction() {
    let (served, events) = observe(
        &[TestExt::<0>::observing()],
        "observer-fiber",
        "<?php\nfunction inner() { return Fiber::suspend() . 'i'; }\n\
         $fiber = new Fiber(function () { return inner(); });\n\
         $fiber->start();\n$fiber->resume('r');\necho $fiber->getReturn();\n",
    );
    assert_eq!(served.stdout_text(), "ri");
    let (fiber, closure) = one_named_in(&events);
    let fiber = |status: &str| format!("{fiber} {status}");
    let closure = closure.as_str();
    assert_eq!(
        events,
        [
            fiber_started(MAIN, &fiber, closure),
            vec![
                init(0, "Fiber::resume"),
                begin(0, "Fiber::resume"),
                switch(0, "main running", &fiber("suspended")),
                end(0, "Fiber::suspend", Some("'r'")),
                end(0, "inner", Some("'ri'")),
                end(0, closure, Some("'ri'")),
                switch(0, &fiber("dead"), "main suspended"),
                destroy(0, &fiber("dead")),
                end(0, "Fiber::resume", Some("null")),
                init(0, "Fiber::getReturn"),
                begin(0, "Fiber::getReturn"),
                end(0, "Fiber::getReturn", Some("'ri'")),
                end(0, MAIN, Some("1")),
            ],
        ]
        .concat()
    );
}

/// Each stack's open frames are its own (#1809's note on #1811): a
/// fiber suspended inside `inner`, its frame open at the same height on
/// the fiber's stack as `declined`'s on the request's, which the
/// observer declined and never begins, so `declined`'s return ends
/// nothing; `inner` is ended inside the fiber with what it returned, and
/// the `Fiber::suspend` it waits in with what the resume sent.
#[test]
fn a_frame_open_in_a_suspended_fiber_is_ended_in_that_fiber_and_nowhere_else() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let files = Files::new("observer-fiber-chain", &[]);
    rec.mark("same_height");
    let served = serve_main(
        &engine,
        &files,
        "<?php\nfunction inner() { Fiber::suspend(); return 'i'; }\nfunction declined() { return 'd'; }\n\
         $fiber = new Fiber(function () { return inner(); });\n\
         $fiber->start();\necho declined();\n$fiber->resume();\necho $fiber->getReturn();\n",
    );
    rec.mark("end");
    assert_eq!(served.stdout_text(), "di");
    let same_height = observed(&rec.events().between("same_height", "end"));
    let (fiber, closure) = one_named_in(&same_height);
    let fiber = |status: &str| format!("{fiber} {status}");
    let closure = closure.as_str();
    assert_eq!(
        same_height,
        [
            fiber_started(MAIN, &fiber, closure),
            vec![
                init(0, "declined"),
                init(0, "Fiber::resume"),
                begin(0, "Fiber::resume"),
                switch(0, "main running", &fiber("suspended")),
                end(0, "Fiber::suspend", Some("null")),
                end(0, "inner", Some("'i'")),
                end(0, closure, Some("'i'")),
                switch(0, &fiber("dead"), "main suspended"),
                destroy(0, &fiber("dead")),
                end(0, "Fiber::resume", Some("null")),
                init(0, "Fiber::getReturn"),
                begin(0, "Fiber::getReturn"),
                end(0, "Fiber::getReturn", Some("'i'")),
                end(0, MAIN, Some("1")),
            ],
        ]
        .concat()
    );
}

/// A fiber inside a fiber switches with its resumer, not with the
/// request's own stack, each `Fiber::start`, `suspend` and `resume` is
/// ended on the stack it was begun on, and each fiber is destroyed once
/// its resumer is served again (`observer_fiber_03.phpt`).
#[test]
fn a_fiber_inside_a_fiber_switches_with_its_resumer() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let files = Files::new("observer-fiber-nested", &[]);
    rec.mark("nested");
    let nested = serve_main(
        &engine,
        &files,
        "<?php\nfunction in2() { Fiber::suspend(); return 2; }\n\
         function in1() { $f = new Fiber(function () { return in2(); }); $f->start(); Fiber::suspend(); $f->resume(); return 1; }\n\
         $fiber = new Fiber(function () { return in1(); });\n\
         $fiber->start();\necho 'm';\n$fiber->resume();\necho $fiber->getReturn();\n",
    );
    rec.mark("end");
    assert_eq!(nested.stdout_text(), "m1");
    let nested = observed(&rec.events().between("nested", "end"));
    let (fibers, closures) = named_in(&nested);
    assert!(
        fibers.len() == 2 && closures.len() == 2,
        "two fibers, two closures: {nested:?}"
    );
    let at = |names: &[String], i: usize| names[i].clone();
    let (outer, inner) = (at(&fibers, 0), at(&fibers, 1));
    let (outer_closure, inner_closure) = (at(&closures, 0), at(&closures, 1));
    let (outer_closure, inner_closure) = (outer_closure.as_str(), inner_closure.as_str());
    let outer = |status: &str| format!("{outer} {status}");
    let inner = |status: &str| format!("{inner} {status}");
    assert_eq!(
        nested,
        [
            init(0, MAIN),
            begin(0, MAIN),
            init(0, "Fiber::__construct"),
            begin(0, "Fiber::__construct"),
            end(0, "Fiber::__construct", Some("null")),
            init(0, "Fiber::start"),
            begin(0, "Fiber::start"),
            fiber_init(0, &outer("init")),
            switch(0, "main running", &outer("init")),
            init(0, outer_closure),
            begin(0, outer_closure),
            init(0, "in1"),
            begin(0, "in1"),
            begin(0, "Fiber::__construct"),
            end(0, "Fiber::__construct", Some("null")),
            begin(0, "Fiber::start"),
            fiber_init(0, &inner("init")),
            switch(0, &outer("running"), &inner("init")),
            init(0, inner_closure),
            begin(0, inner_closure),
            init(0, "in2"),
            begin(0, "in2"),
            init(0, "Fiber::suspend"),
            begin(0, "Fiber::suspend"),
            switch(0, &inner("running"), &outer("suspended")),
            end(0, "Fiber::start", Some("null")),
            begin(0, "Fiber::suspend"),
            switch(0, &outer("running"), "main suspended"),
            end(0, "Fiber::start", Some("null")),
            init(0, "Fiber::resume"),
            begin(0, "Fiber::resume"),
            switch(0, "main running", &outer("suspended")),
            end(0, "Fiber::suspend", Some("null")),
            begin(0, "Fiber::resume"),
            switch(0, &outer("running"), &inner("suspended")),
            end(0, "Fiber::suspend", Some("null")),
            end(0, "in2", Some("2")),
            end(0, inner_closure, Some("2")),
            switch(0, &inner("dead"), &outer("suspended")),
            destroy(0, &inner("dead")),
            end(0, "Fiber::resume", Some("null")),
            end(0, "in1", Some("1")),
            end(0, outer_closure, Some("1")),
            switch(0, &outer("dead"), "main suspended"),
            destroy(0, &outer("dead")),
            end(0, "Fiber::resume", Some("null")),
            init(0, "Fiber::getReturn"),
            begin(0, "Fiber::getReturn"),
            end(0, "Fiber::getReturn", Some("1")),
            end(0, MAIN, Some("1")),
        ]
    );
}

/// `Fiber::getCurrent()` served from a fiber switch handler reads
/// php's `EG(active_fiber)` as the switch's notice finds it, which
/// `zend_fiber_resume_internal` sets to the fiber before the switch into
/// it and restores only once the switch back has returned
/// (`Z/Zend/zend_fibers.c:680-696`): the fiber, on each switch into it
/// and each switch out of it, its last among them (#2014).
#[test]
fn a_switch_handler_reads_the_fiber_switched_into_and_out_of_as_the_current_one() {
    let (served, events) = observe(
        &[TestExt::<0>::builder()
            .fiber_switch(current_at_switch)
            .build("test_ext_0")],
        "observer-fiber-current",
        "<?php\n$fiber = new Fiber(function () { Fiber::suspend(); });\n\
         $fiber->start();\n$fiber->resume();\necho 'done';\n",
    );
    assert_eq!(served.stdout_text(), "done");
    // The fiber as the first switch, the one into it, names it.
    let fiber = match events.first() {
        Some(Ext(_, FiberSwitch(_, to))) => to
            .split_once(' ')
            .map_or(to.as_str(), |(name, _)| name)
            .to_owned(),
        _ => panic!("the first event is the switch into the fiber: {events:?}"),
    };
    let current = |to: &str| format!("{to}; current {fiber}");
    assert_eq!(
        events,
        [
            switch(0, "main running", &current(&format!("{fiber} init"))),
            switch(0, &format!("{fiber} running"), &current("main suspended")),
            switch(0, "main running", &current(&format!("{fiber} suspended"))),
            switch(0, &format!("{fiber} dead"), &current("main suspended")),
        ]
    );
}

/// A switch handler that records the switch with what
/// `Fiber::getCurrent()` answers there: `#n` for a fiber, `null` for
/// none.
fn current_at_switch(req: &mut Request, from: ObservedFiber, to: ObservedFiber) {
    let current = match PhpString::declared(req, b"Fiber::getCurrent")
        .ok()
        .and_then(|name| {
            let callable = callable_checked(req, &Value::string(*name));
            name.give_up(req);
            callable.ok()
        })
        .map(|callable| call(req, &callable, &[]))
    {
        Some(Ok(answer)) => {
            let named = answer.as_object().map_or_else(
                || String::from("null"),
                |fiber| format!("#{}", fiber.index()),
            );
            answer.give_up(req);
            named
        }
        Some(Err(_)) => String::from("thrown"),
        None => String::from("uncallable"),
    };
    record(
        0,
        FiberSwitch(
            fiber_name(from),
            format!("{}; current {current}", fiber_name(to)),
        ),
    );
}

/// A fatal inside a fiber (`observer_fiber_functions_03.phpt`) ends the
/// fiber's open frames after it is rendered, the innermost first; the
/// switch out of the dead fiber and its destruction are noticed; then
/// the resumer's open frames are ended, as php's `zend_bailout()` raised
/// again on the resumer leaves them to `zend_observer_fcall_end_all`. A
/// fiber suspended meanwhile keeps its frames open, never ended, as php
/// leaves a suspended fiber's at a fatal. The fatal here is the memory
/// limit's, which php 8.5.12 raises at the loop's next interrupt check
/// and unwinds by the unwind exit (`zend_handle_delayed_oom`), so the
/// fiber dies having thrown, as `zend_fiber_execute` marks a fiber not
/// being destroyed that an unwind exit leaves (`Z/Zend/zend_fibers.c:616`).
#[test]
fn a_fatal_in_a_fiber_ends_its_frames_then_its_resumers_and_no_other_fibers() {
    let (_, events) = observe(
        &[TestExt::<0>::observing()],
        "observer-fiber-fatal",
        "<?php\nfunction held() { Fiber::suspend(); }\n\
         $kept = new Fiber(function () { held(); });\n$kept->start();\n\
         function deep() { $t = '.'; for ($i = 0; $i < 16; $i++) { $t .= $t; } $s = ''; while (true) { $s .= $t; } }\n\
         function resumer($f) { $f->resume(); }\n\
         $fiber = new Fiber(function () { Fiber::suspend(); deep(); });\n$fiber->start();\n\
         ini_set('memory_limit', '2M');\nresumer($fiber);\n",
    );
    let fatal = events
        .iter()
        .position(|e| matches!(e, Ext(_, Error(m)) if m.starts_with("Allowed memory size")))
        .unwrap_or_else(|| panic!("the fatal is rendered: {events:?}"));
    let (fibers, closures) = named_in(&events);
    assert!(
        fibers.len() == 2 && closures.len() == 2,
        "two fibers, two closures: {events:?}"
    );
    let at = |names: &[String], i: usize| names[i].clone();
    let (fiber, kept_closure, closure) = (at(&fibers, 1), at(&closures, 0), at(&closures, 1));
    assert_eq!(
        events[fatal + 1..],
        [
            end(0, "deep", None),
            end(0, &closure, None),
            switch(0, &format!("{fiber} dead threw"), "main suspended"),
            destroy(0, &format!("{fiber} dead threw")),
            end(0, "Fiber::resume", None),
            end(0, "resumer", None),
            end(0, MAIN, None),
        ],
        "{events:?}"
    );
    for name in ["held", kept_closure.as_str()] {
        assert!(
            events.contains(&begin(0, name))
                && !events
                    .iter()
                    .any(|e| matches!(e, Ext(_, End(n, _)) if n == name)),
            "{name} is begun and never ended: {events:?}"
        );
    }
}

/// A fiber still suspended as the request ends
/// (`observer_fiber_functions_02.phpt`): the main file has ended, the
/// fiber is destroyed by switching into it, every frame it left open is
/// ended with nothing as the graceful exit unwinds it, and the switch
/// out of it and its destruction are noticed.
#[test]
fn a_fiber_destroyed_while_suspended_ends_its_frames_inside_it() {
    let (_, events) = observe(
        &[TestExt::<0>::observing()],
        "observer-fiber-destroyed",
        "<?php\nfunction inner() { Fiber::suspend(); }\n\
         $fiber = new Fiber(function () { inner(); });\n$fiber->start();\n",
    );
    let (fiber, closure) = one_named_in(&events);
    let fiber = |status: &str| format!("{fiber} {status}");
    let closure = closure.as_str();
    assert_eq!(
        events,
        [
            fiber_started(MAIN, &fiber, closure),
            vec![
                end(0, MAIN, Some("1")),
                switch(0, "main running", &fiber("suspended destroyed")),
                end(0, "Fiber::suspend", None),
                end(0, "inner", None),
                end(0, closure, None),
                switch(0, &fiber("dead destroyed"), "main suspended"),
                destroy(0, &fiber("dead destroyed")),
            ],
        ]
        .concat()
    );
}

/// Two fibers still suspended as the request ends, one held by the
/// other's closure (`observer_fiber_05.phpt`): the outer is destroyed,
/// and the inner as the outer's frame gives it up, each switched into
/// marked destroyed, and each switched out of dead in the reverse order,
/// php's `destroying`, `destroying`, `destroyed`, `destroyed`; and a
/// fiber `Fiber::throw()` resumed that does not catch
/// (`observer_fiber_06.phpt`) leaves dead marked as having thrown, php's `threw`.
#[test]
fn a_fiber_destroyed_or_thrown_into_is_noticed_as_such() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let files = Files::new("observer-fiber-flags", &[]);
    rec.mark("nested");
    serve_main(
        &engine,
        &files,
        "<?php\n$fiber = new Fiber(function () {\n\
         Fiber::suspend();\n$fiber = new Fiber(function () { Fiber::suspend(); });\n\
         $fiber->start();\nFiber::suspend(); });\n$fiber->start();\n$fiber->resume();\n",
    );
    rec.mark("thrown");
    serve_main(
        &engine,
        &files,
        "<?php\n$fiber = new Fiber(function () { Fiber::suspend(); });\n$fiber->start();\n\
         try { $fiber->throw(new Exception); } catch (Exception $e) {}\n",
    );
    rec.mark("end");
    let switched = |events: &Events| -> Vec<Event> {
        observed(events)
            .into_iter()
            .filter(|e| matches!(e, Ext(_, FiberSwitch(..) | FiberDestroy(_))))
            .collect()
    };
    let events = rec.events();

    let nested = observed(&events.between("nested", "thrown"));
    let (fibers, _) = named_in(&nested);
    assert_eq!(fibers.len(), 2, "two fibers: {nested:?}");
    let at = |i: usize| fibers[i].clone();
    let (outer, inner) = (at(0), at(1));
    let outer = |status: &str| format!("{outer} {status}");
    let inner = |status: &str| format!("{inner} {status}");
    assert_eq!(
        switched(&events.between("nested", "thrown")),
        [
            switch(0, "main running", &outer("init")),
            switch(0, &outer("running"), "main suspended"),
            switch(0, "main running", &outer("suspended")),
            switch(0, &outer("running"), &inner("init")),
            switch(0, &inner("running"), &outer("suspended")),
            switch(0, &outer("running"), "main suspended"),
            switch(0, "main running", &outer("suspended destroyed")),
            switch(
                0,
                &outer("running destroyed"),
                &inner("suspended destroyed")
            ),
            switch(0, &inner("dead destroyed"), &outer("suspended destroyed")),
            destroy(0, &inner("dead destroyed")),
            switch(0, &outer("dead destroyed"), "main suspended"),
            destroy(0, &outer("dead destroyed")),
        ]
    );

    let thrown = observed(&events.between("thrown", "end"));
    let (fiber, _) = one_named_in(&thrown);
    let fiber = |status: &str| format!("{fiber} {status}");
    assert_eq!(
        switched(&events.between("thrown", "end")),
        [
            switch(0, "main running", &fiber("init")),
            switch(0, &fiber("running"), "main suspended"),
            switch(0, "main running", &fiber("suspended")),
            switch(0, &fiber("dead threw"), "main suspended"),
            destroy(0, &fiber("dead threw")),
        ]
    );
}

/// The resumptions a generator is served by other than `foreach`, each
/// a begin and an end as php's `zend_generator_resume` makes them: a
/// `yield from` over an array, ended at the `YIELD_FROM` with what it
/// yielded last and its values served unobserved; `Generator::throw()`
/// into it, caught inside; a generator destroyed suspended in a `try`,
/// whose `finally` is served by a resumption that is ended with nothing,
/// as php's forced close leaves it through
/// `zend_dispatch_try_catch_finally_helper`'s uncaught arm; and a
/// generator resumed inside a fiber, begun and ended there, and given up
/// suspended outside any `try`, which resumes nothing.
#[test]
fn a_generator_is_observed_through_yield_from_throw_close_and_a_fiber() {
    let (served, events) = observe(
        &[TestExt::<0>::observing()],
        "observer-generator-paths",
        "<?php\nfunction foo() { yield 0; yield from [1, 2];\n\
         try { yield 3; } catch (Exception $e) { echo 'c'; }\n\
         try { yield 4; } finally { echo 'f'; } }\n\
         function run() { $g = foo(); foreach ($g as $v) { echo $v; if ($v === 3) { $g->throw(new Exception); echo $g->current(); break; } } }\n\
         run();\n\
         $fiber = new Fiber(function () { $g = foo(); echo $g->current(); });\n$fiber->start();\n",
    );
    assert_eq!(served.stdout_text(), "0123c4f0");
    let foo: Vec<Event> = events
        .iter()
        .filter(|e| matches!(e, Ext(_, Begin(n) | End(n, _)) if n == "foo"))
        .cloned()
        .collect();
    assert_eq!(
        foo,
        [
            begin(0, "foo"),
            end(0, "foo", Some("0")),
            begin(0, "foo"),
            end(0, "foo", Some("0")),
            begin(0, "foo"),
            end(0, "foo", Some("3")),
            begin(0, "foo"),
            end(0, "foo", Some("4")),
            begin(0, "foo"),
            end(0, "foo", None),
            begin(0, "foo"),
            end(0, "foo", Some("0")),
        ]
    );
}

/// A trait's method in each class that uses it reads a runtime cache of
/// its own, `function_add_ref`'s fresh map pointer over
/// `zend_add_trait_method`'s copy, so the observer is asked of it once
/// per class, `A::f` and `B::f`, and not again for `A::f`, as
/// `zend_test`'s observer prints it. A class a held file's link
/// publishes takes an engine map pointer for its copy instead; the
/// witness on `CacheOf` serves that through opcache.
#[test]
fn a_trait_method_is_asked_of_once_per_class_that_uses_it() {
    let (_, events) = observe(
        &[TestExt::<0>::observing()],
        "observer-trait-copies",
        "<?php\ntrait T { function f() { return 1; } }\nclass A { use T; }\nclass B { use T; }\n\
         (new A)->f(); (new B)->f(); (new A)->f();\n",
    );
    let methods: Vec<Event> = events
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                Ext(0, ObserverInit(name) | Begin(name)) if name.ends_with("::f")
            )
        })
        .collect();
    assert_eq!(
        methods,
        [
            init(0, "A::f"),
            begin(0, "A::f"),
            init(0, "B::f"),
            begin(0, "B::f"),
            begin(0, "A::f"),
        ]
    );
}

/// `zend_create_closure_ex`'s runtime cache, as `zend_test`'s observer
/// prints it under the oracle: a closure bound to the scope its op
/// array's cache was laid down for shares that cache and is asked of
/// once (`$f`, `$h`, the second `make()`), one bound to another scope
/// takes a cache of its own and is asked of again (`$g`, `$o`), one
/// made from a closure with a cache of its own takes another (`$i`),
/// and a fake closure of a method already called, in the method's own
/// scope, shares the method's (`$p`).
#[test]
fn a_closure_bound_to_another_scope_is_asked_of_again() {
    let (served, events) = observe(
        &[TestExt::<0>::observing()],
        "observer-rebound",
        "<?php\nclass A { function make() { return function () { return 1; }; } }\nclass B {}\n\
         $f = function () { return 1; };\n$f(); $f();\n\
         $g = Closure::bind($f, null, A::class);\n$g(); $g();\n\
         $h = Closure::bind($f, null, null);\n$h();\n\
         $i = $g->bindTo(null, A::class);\n$i();\n\
         $m = (new A)->make();\n$m();\n$n = (new A)->make();\n$n();\n\
         $o = Closure::bind($m, null, B::class);\n$o();\n\
         $p = Closure::fromCallable([new A, 'make']);\n$p();\necho \"done\\n\";\n",
    );
    assert_eq!(served.stdout_text(), "done\n");
    let asked: Vec<String> = events
        .into_iter()
        .filter_map(|event| match event {
            Ext(0, ObserverInit(name)) if name.contains("{closure") || name == "A::make" => Some(
                name.split_once("{closure")
                    .map_or(name.as_str(), |(head, _)| head)
                    .to_owned()
                    + "*",
            ),
            _ => None,
        })
        .collect();
    assert_eq!(
        asked,
        ["*", "A::*", "A::*", "A::make*", "A::*", "B::*"],
        "{asked:?}"
    );
}
