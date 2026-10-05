//! Seam 9, the observer, as `TestExt` records it (design §4.3 row 9,
//! revision 79): php's order and pairing of begin and end for user calls,
//! includes and evals, a function's slot resolved on its first call with
//! the choice to decline, and every open frame ended once by an unwind
//! and by a fatal. The sequences are php's, as `zend_test`'s observer
//! prints them in `Z/ext/zend_test/tests/observer_*.phpt`.

use std::path::{Path, PathBuf};

use super::test_ext::{Event, Events, Phase, Recording, TestExt};
use crate::engine::Engine;
use crate::test::{Capture, Served};

use Event::Ext;
use Phase::{Begin, End, Error, FiberDestroy, FiberInit, FiberSwitch, ObserverInit};

/// The observer's events alone, the fiber notices among them, and the
/// diagnostics `zend_error_cb` was handed among them, in the order
/// recorded.
fn observed(events: &Events) -> Vec<Event> {
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

/// A directory of its own for one test's files, under the target's
/// temporary directory, emptied first.
fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("php_engine_observer_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// `source` served as `main.php` in `dir`.
fn serve_main(engine: &Engine, dir: &Path, source: &str) -> Served {
    let main = dir.join("main.php");
    let _ = std::fs::write(&main, source);
    engine.serve_capture_in(
        dir,
        main.to_string_lossy().as_bytes(),
        source,
        Capture::default(),
    )
}

/// `observer_basic_01.phpt`'s order, two observers deep: a function's
/// handlers are resolved on its first call, every observer asked in
/// declaration order, and never again in the request; begin handlers
/// are called in declaration order and end handlers in reverse, each end
/// handed what the frame returned; the main file's frame is observed
/// as a function's is, and ends with the `1` its implicit return gives.
#[test]
fn calls_begin_in_declaration_order_and_end_in_reverse_with_what_they_returned() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing(), TestExt::<1>::observing()])
        .build();
    let dir = scratch("calls");
    rec.mark("built");
    let served = serve_main(
        &engine,
        &dir,
        "<?php\nfunction g($y) { return $y * 2; }\nfunction f($x) { return g($x) + 1; }\necho f(1), f(2);\n",
    );
    rec.mark("served");
    assert_eq!(String::from_utf8_lossy(&served.stdout), "35");
    let file = "file 'main.php'";
    assert_eq!(
        observed(&rec.events().between("built", "served")),
        [
            init(0, file),
            init(1, file),
            begin(0, file),
            begin(1, file),
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
            end(1, file, Some("1")),
            end(0, file, Some("1")),
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
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("declined");
    rec.mark("built");
    serve_main(
        &engine,
        &dir,
        "<?php\nfunction declined() { return 1; }\nclass C { static function m() { return declined(); } }\n\
         C::m(); C::m();\nset_error_handler(function () { return declined(); });\necho $undefined;\n",
    );
    rec.mark("served");
    let file = "file 'main.php'";
    let events = observed(&rec.events().between("built", "served"));
    assert_eq!(
        events.get(..8).unwrap_or_default(),
        [
            init(0, file),
            begin(0, file),
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
    assert_eq!(events.last(), Some(&end(0, file, Some("1"))));
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
    let dir = scratch("include");
    let _ = std::fs::write(
        dir.join("inc.php"),
        "<?php\nfunction h() { return 'h'; }\nreturn h();\n",
    );
    rec.mark("built");
    let served = serve_main(
        &engine,
        &dir,
        "<?php\necho include __DIR__ . '/inc.php';\necho eval('$v = 7; return $v;');\n",
    );
    rec.mark("served");
    assert_eq!(String::from_utf8_lossy(&served.stdout), "h7");
    let file = "file 'main.php'";
    let inc = "file 'inc.php'";
    let evaluated = "file 'main.php(3) : eval()'d code'";
    assert_eq!(
        observed(&rec.events().between("built", "served")),
        [
            init(0, file),
            begin(0, file),
            init(0, inc),
            begin(0, inc),
            init(0, "h"),
            begin(0, "h"),
            end(0, "h", Some("'h'")),
            end(0, inc, Some("'h'")),
            init(0, evaluated),
            begin(0, evaluated),
            end(0, evaluated, Some("7")),
            end(0, file, Some("1")),
        ]
    );
}

/// Begin runs before the parameters are received, as php's
/// `ZEND_OBSERVER_FCALL_BEGIN` stands in `ZEND_DO_UCALL` before the
/// callee's `RECV` runs: a `RECV` that throws ends the frame it began,
/// with no value, from the unwind, for a call from code and for one a
/// native makes alike, here the error handler `zend_error` calls.
#[test]
fn begin_runs_before_the_parameters_are_received() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("recv");
    rec.mark("built");
    serve_main(
        &engine,
        &dir,
        "<?php\nfunction t(array $a) { return $a; }\n\
         try { t(1); } catch (TypeError $e) { echo 'a'; }\n\
         set_error_handler('t');\ntry { echo $u; } catch (TypeError $e) { echo 'b'; }\n",
    );
    rec.mark("served");
    let file = "file 'main.php'";
    assert_eq!(
        observed(&rec.events().between("built", "served")),
        [
            init(0, file),
            begin(0, file),
            init(0, "t"),
            begin(0, "t"),
            end(0, "t", None),
            begin(0, "t"),
            end(0, "t", None),
            end(0, file, Some("1")),
        ]
    );
}

/// An unwind ends every frame it leaves once, with no value, from the
/// innermost out, where php's uncaught arm of
/// `zend_dispatch_try_catch_finally_helper` ends it: through a `catch`
/// further out, as the throwable leaves the main file uncaught
/// (`observer_exception_01.phpt`, where the file ends before the fatal
/// is rendered), and as `exit` unwinds.
#[test]
fn an_unwind_ends_every_frame_it_leaves_once() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("unwind");
    let file = "file 'main.php'";
    let nested = "<?php\nfunction b() { throw new Exception('x'); }\nfunction a() { b(); }\n";

    rec.mark("caught");
    serve_main(
        &engine,
        &dir,
        &format!("{nested}try {{ a(); }} catch (Exception $e) {{ echo 'c'; }}\n"),
    );
    rec.mark("uncaught");
    serve_main(&engine, &dir, &format!("{nested}a();\n"));
    rec.mark("exited");
    serve_main(
        &engine,
        &dir,
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
            end(0, "b", None),
            end(0, "a", None),
            end(0, file, Some("1"))
        ]
    );
    let uncaught = observed(&events.between("uncaught", "exited"));
    assert_eq!(
        unwound(&events.between("uncaught", "exited")),
        [end(0, "b", None), end(0, "a", None), end(0, file, None)]
    );
    let rendered = uncaught
        .iter()
        .position(|e| matches!(e, Ext(_, Error(m)) if m.starts_with("Uncaught Exception: x")));
    let file_ended = uncaught.iter().position(|e| *e == end(0, file, None));
    assert!(
        file_ended < rendered && rendered.is_some(),
        "the main file ends before the uncaught throwable is rendered: {uncaught:?}"
    );
    assert_eq!(
        unwound(&events.between("exited", "end")),
        [end(0, "b", None), end(0, "a", None), end(0, file, None)]
    );
}

/// A fatal ends every open frame once, with no value, the innermost
/// first, after the fatal is rendered, as php's `zend_observer_fcall_end_all`
/// does at `php_request_shutdown` (`observer_error_01.phpt`), and before
/// a shutdown function runs: a fatal the loop raises, the memory limit
/// a concatenation reaches, and one a native raises, `trigger_error`'s
/// `E_USER_ERROR`, inside the error handler `zend_error` called; and
/// the time limit reached by a function a generator's body called,
/// which the loop raises.
#[test]
fn a_bailout_ends_every_open_frame_once_after_the_fatal() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("bailout");
    let file = "file 'main.php'";

    rec.mark("memory");
    serve_main(
        &engine,
        &dir,
        "<?php\nini_set('memory_limit', '2M');\n\
         function s() { echo 's'; }\nregister_shutdown_function('s');\n\
         function b() { $s = '.'; while (true) { $s .= $s; } }\nfunction a() { return b(); }\na();\n",
    );
    rec.mark("user");
    serve_main(
        &engine,
        &dir,
        "<?php\nfunction b($no, $str) { trigger_error('boom', E_USER_ERROR); }\n\
         function a() { set_error_handler('b'); echo $u; }\na();\n",
    );
    rec.mark("generator");
    serve_main(
        &engine,
        &dir,
        "<?php\nset_time_limit(1);\n\
         function b() { while (true); }\n\
         function g() { b(); yield 1; }\nfunction a() { foreach (g() as $v) {} }\na();\n",
    );
    rec.mark("end");
    let events = rec.events();

    let memory = observed(&events.between("memory", "user"));
    let fatal = memory
        .iter()
        .position(|e| matches!(e, Ext(_, Error(m)) if m.starts_with("Allowed memory size")));
    assert!(fatal.is_some(), "the fatal is rendered: {memory:?}");
    let after: Vec<_> = memory
        .iter()
        .skip(fatal.unwrap_or_default() + 1)
        .cloned()
        .collect();
    assert_eq!(
        after,
        [
            end(0, "b", None),
            end(0, "a", None),
            end(0, file, None),
            init(0, "s"),
            begin(0, "s"),
            end(0, "s", Some("null")),
        ],
        "every open frame ended once after the fatal, before the shutdown function: {memory:?}"
    );

    let user = observed(&events.between("user", "generator"));
    let fatal = user
        .iter()
        .position(|e| matches!(e, Ext(_, Error(m)) if m == "boom"));
    assert!(fatal.is_some(), "the fatal is rendered: {user:?}");
    assert_eq!(
        user.iter()
            .skip(fatal.unwrap_or_default() + 1)
            .cloned()
            .collect::<Vec<_>>(),
        [end(0, "b", None), end(0, "a", None), end(0, file, None)],
        "every open frame ended once after the fatal: {user:?}"
    );
    for name in ["a", "b", file] {
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
        .position(|e| matches!(e, Ext(_, Error(m)) if m.starts_with("Maximum execution time")));
    assert!(fatal.is_some(), "the fatal is rendered: {resumed:?}");
    assert_eq!(
        resumed
            .iter()
            .skip(fatal.unwrap_or_default() + 1)
            .cloned()
            .collect::<Vec<_>>(),
        [
            end(0, "b", None),
            end(0, "g", None),
            end(0, "a", None),
            end(0, file, None)
        ],
        "every open frame ended once after the fatal: {resumed:?}"
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

/// A generator's frame is begun at each resumption and ended at each
/// suspension with what it yielded, and not at its call
/// (`observer_generator_01.phpt` to `_03`): the frame's slot is
/// resolved at its first resumption; a `yield from` another generator
/// ends the delegating frame with what it yielded last and resumes the
/// one delegated to, whose `return` ends it with what it returned, and
/// the delegating frame's own `return` ends it with its own.
#[test]
fn a_generator_is_begun_at_each_resumption_and_ended_at_each_yield() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("generator");
    rec.mark("built");
    let served = serve_main(
        &engine,
        &dir,
        "<?php\nfunction bar() { for ($i = 10; $i < 12; $i++) { yield $i; } }\n\
         function foo() { yield 0; yield from bar(); yield 42; return 1337; }\n\
         function run() { $g = foo(); foreach ($g as $v) { echo $v, ','; } return $g->getReturn(); }\n\
         echo run();\n",
    );
    rec.mark("served");
    assert_eq!(String::from_utf8_lossy(&served.stdout), "0,10,11,42,1337");
    let file = "file 'main.php'";
    assert_eq!(
        observed(&rec.events().between("built", "served")),
        [
            init(0, file),
            begin(0, file),
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
            end(0, "run", Some("1337")),
            end(0, file, Some("1")),
        ]
    );
}

/// A generator resumed by `current()`, `next()` and `send()` rather than
/// by `foreach` (`observer_generator_04.phpt`): its first read runs it
/// to its first `yield`, each `next()` and the `send()` is one
/// resumption, and a read of a generator not resumed again begins
/// nothing; and one that throws out of its body
/// (`observer_generator_05.phpt`) is ended with nothing, as every frame
/// the throwable leaves is.
#[test]
fn a_generator_resumed_by_hand_or_left_by_a_throwable_pairs_each_resumption() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("generator_by_hand");
    rec.mark("by_hand");
    let served = serve_main(
        &engine,
        &dir,
        "<?php\nfunction foo() { $i = 0; while (true) { if (yield $i++) break; } }\n\
         function run() { $g = foo();\n\
         while ($g->current() !== null) { echo $g->current(); if ($g->current() === 2) { $g->send('Boop'); } $g->next(); }\n\
         return 'Done'; }\necho run();\n",
    );
    rec.mark("thrown");
    serve_main(
        &engine,
        &dir,
        "<?php\nfunction foo() { yield 0; yield 1; throw new Exception('Oops!'); }\n\
         function run() { foreach (foo() as $v) { echo $v; } return 'unseen'; }\necho run();\n",
    );
    rec.mark("end");
    assert_eq!(String::from_utf8_lossy(&served.stdout), "012Done");
    let file = "file 'main.php'";
    let events = rec.events();
    assert_eq!(
        observed(&events.between("by_hand", "thrown")),
        [
            init(0, file),
            begin(0, file),
            init(0, "run"),
            begin(0, "run"),
            init(0, "foo"),
            begin(0, "foo"),
            end(0, "foo", Some("0")),
            begin(0, "foo"),
            end(0, "foo", Some("1")),
            begin(0, "foo"),
            end(0, "foo", Some("2")),
            begin(0, "foo"),
            end(0, "foo", Some("null")),
            end(0, "run", Some("'Done'")),
            end(0, file, Some("1")),
        ]
    );
    let thrown = observed(&events.between("thrown", "end"));
    let rendered = thrown
        .iter()
        .position(|e| matches!(e, Ext(_, Error(m)) if m.starts_with("Uncaught Exception: Oops!")));
    assert!(rendered.is_some(), "the throwable is rendered: {thrown:?}");
    assert_eq!(
        thrown[..rendered.unwrap_or_default()],
        [
            init(0, file),
            begin(0, file),
            init(0, "run"),
            begin(0, "run"),
            init(0, "foo"),
            begin(0, "foo"),
            end(0, "foo", Some("0")),
            begin(0, "foo"),
            end(0, "foo", Some("1")),
            begin(0, "foo"),
            end(0, "foo", None),
            end(0, "run", None),
            end(0, file, None),
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
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("fiber");
    rec.mark("built");
    let served = serve_main(
        &engine,
        &dir,
        "<?php\nfunction inner() { return Fiber::suspend() . 'i'; }\n\
         $fiber = new Fiber(function () { return inner(); });\n\
         $fiber->start();\n$fiber->resume('r');\necho $fiber->getReturn();\n",
    );
    rec.mark("served");
    assert_eq!(String::from_utf8_lossy(&served.stdout), "ri");
    let events = observed(&rec.events().between("built", "served"));
    let (fiber, closure) = one_named_in(&events);
    let fiber = |status: &str| format!("{fiber} {status}");
    let closure = closure.as_str();
    let file = "file 'main.php'";
    assert_eq!(
        events,
        [
            init(0, file),
            begin(0, file),
            fiber_init(0, &fiber("init")),
            switch(0, "main running", &fiber("init")),
            init(0, closure),
            begin(0, closure),
            init(0, "inner"),
            begin(0, "inner"),
            switch(0, &fiber("running"), "main suspended"),
            switch(0, "main running", &fiber("suspended")),
            end(0, "inner", Some("'ri'")),
            end(0, closure, Some("'ri'")),
            switch(0, &fiber("dead"), "main suspended"),
            destroy(0, &fiber("dead")),
            end(0, file, Some("1")),
        ]
    );
}

/// Each stack's open frames are its own (#1809's note on #1811): a
/// fiber suspended inside `inner`, its frame open at the same height on
/// the fiber's stack as `declined`'s on the request's, which the
/// observer declined and never begins, so `declined`'s return ends
/// nothing; `inner` is ended inside the fiber with what it returned.
/// A fiber inside a fiber switches with its resumer, not with the
/// request's own stack, and each is destroyed once its resumer is served
/// again (`observer_fiber_03.phpt`).
#[test]
fn a_frame_open_in_a_suspended_fiber_is_ended_in_that_fiber_and_nowhere_else() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("fiber_chain");
    rec.mark("same_height");
    let served = serve_main(
        &engine,
        &dir,
        "<?php\nfunction inner() { Fiber::suspend(); return 'i'; }\nfunction declined() { return 'd'; }\n\
         $fiber = new Fiber(function () { return inner(); });\n\
         $fiber->start();\necho declined();\n$fiber->resume();\necho $fiber->getReturn();\n",
    );
    rec.mark("nested");
    let nested = serve_main(
        &engine,
        &dir,
        "<?php\nfunction in2() { Fiber::suspend(); return 2; }\n\
         function in1() { $f = new Fiber(function () { return in2(); }); $f->start(); Fiber::suspend(); $f->resume(); return 1; }\n\
         $fiber = new Fiber(function () { return in1(); });\n\
         $fiber->start();\necho 'm';\n$fiber->resume();\necho $fiber->getReturn();\n",
    );
    rec.mark("end");
    assert_eq!(String::from_utf8_lossy(&served.stdout), "di");
    assert_eq!(String::from_utf8_lossy(&nested.stdout), "m1");
    let file = "file 'main.php'";
    let events = rec.events();

    let same_height = observed(&events.between("same_height", "nested"));
    let (fiber, closure) = one_named_in(&same_height);
    let fiber = |status: &str| format!("{fiber} {status}");
    let closure = closure.as_str();
    assert_eq!(
        same_height,
        [
            init(0, file),
            begin(0, file),
            fiber_init(0, &fiber("init")),
            switch(0, "main running", &fiber("init")),
            init(0, closure),
            begin(0, closure),
            init(0, "inner"),
            begin(0, "inner"),
            switch(0, &fiber("running"), "main suspended"),
            init(0, "declined"),
            switch(0, "main running", &fiber("suspended")),
            end(0, "inner", Some("'i'")),
            end(0, closure, Some("'i'")),
            switch(0, &fiber("dead"), "main suspended"),
            destroy(0, &fiber("dead")),
            end(0, file, Some("1")),
        ]
    );

    let nested = observed(&events.between("nested", "end"));
    let (fibers, closures) = named_in(&nested);
    assert!(
        fibers.len() == 2 && closures.len() == 2,
        "two fibers, two closures: {nested:?}"
    );
    let at = |names: &[String], i: usize| names.get(i).cloned().unwrap_or_default();
    let (outer, inner) = (at(&fibers, 0), at(&fibers, 1));
    let (outer_closure, inner_closure) = (at(&closures, 0), at(&closures, 1));
    let (outer_closure, inner_closure) = (outer_closure.as_str(), inner_closure.as_str());
    let outer = |status: &str| format!("{outer} {status}");
    let inner = |status: &str| format!("{inner} {status}");
    assert_eq!(
        nested,
        [
            init(0, file),
            begin(0, file),
            fiber_init(0, &outer("init")),
            switch(0, "main running", &outer("init")),
            init(0, outer_closure),
            begin(0, outer_closure),
            init(0, "in1"),
            begin(0, "in1"),
            fiber_init(0, &inner("init")),
            switch(0, &outer("running"), &inner("init")),
            init(0, inner_closure),
            begin(0, inner_closure),
            init(0, "in2"),
            begin(0, "in2"),
            switch(0, &inner("running"), &outer("suspended")),
            switch(0, &outer("running"), "main suspended"),
            switch(0, "main running", &outer("suspended")),
            switch(0, &outer("running"), &inner("suspended")),
            end(0, "in2", Some("2")),
            end(0, inner_closure, Some("2")),
            switch(0, &inner("dead"), &outer("suspended")),
            destroy(0, &inner("dead")),
            end(0, "in1", Some("1")),
            end(0, outer_closure, Some("1")),
            switch(0, &outer("dead"), "main suspended"),
            destroy(0, &outer("dead")),
            end(0, file, Some("1")),
        ]
    );
}

/// A fatal inside a fiber (`observer_fiber_functions_03.phpt`) ends the
/// fiber's open frames after it is rendered, the innermost first; the
/// switch out of the dead fiber and its destruction are noticed; then
/// the resumer's open frames are ended, as php's `zend_bailout()` raised
/// again on the resumer leaves them to `zend_observer_fcall_end_all`. A
/// fiber suspended meanwhile keeps its frames open, never ended, as php
/// leaves a suspended fiber's at a fatal.
#[test]
fn a_fatal_in_a_fiber_ends_its_frames_then_its_resumers_and_no_other_fibers() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("fiber_fatal");
    rec.mark("built");
    serve_main(
        &engine,
        &dir,
        "<?php\nfunction held() { Fiber::suspend(); }\n\
         $kept = new Fiber(function () { held(); });\n$kept->start();\n\
         function deep() { $s = '.'; while (true) { $s .= $s; } }\n\
         function resumer($f) { $f->resume(); }\n\
         $fiber = new Fiber(function () { Fiber::suspend(); deep(); });\n$fiber->start();\n\
         ini_set('memory_limit', '2M');\nresumer($fiber);\n",
    );
    rec.mark("served");
    let events = observed(&rec.events().between("built", "served"));
    let fatal = events
        .iter()
        .position(|e| matches!(e, Ext(_, Error(m)) if m.starts_with("Allowed memory size")));
    assert!(fatal.is_some(), "the fatal is rendered: {events:?}");
    let (fibers, closures) = named_in(&events);
    assert!(
        fibers.len() == 2 && closures.len() == 2,
        "two fibers, two closures: {events:?}"
    );
    let at = |names: &[String], i: usize| names.get(i).cloned().unwrap_or_default();
    let (fiber, kept_closure, closure) = (at(&fibers, 1), at(&closures, 0), at(&closures, 1));
    assert_eq!(
        events[fatal.unwrap_or_default() + 1..],
        [
            end(0, "deep", None),
            end(0, &closure, None),
            switch(0, &format!("{fiber} dead"), "main suspended"),
            destroy(0, &format!("{fiber} dead")),
            end(0, "resumer", None),
            end(0, "file 'main.php'", None),
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
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("fiber_destroyed");
    rec.mark("built");
    serve_main(
        &engine,
        &dir,
        "<?php\nfunction inner() { Fiber::suspend(); }\n\
         $fiber = new Fiber(function () { inner(); });\n$fiber->start();\n",
    );
    rec.mark("served");
    let events = observed(&rec.events().between("built", "served"));
    let (fiber, closure) = one_named_in(&events);
    let fiber = |status: &str| format!("{fiber} {status}");
    let closure = closure.as_str();
    let file = "file 'main.php'";
    assert_eq!(
        events,
        [
            init(0, file),
            begin(0, file),
            fiber_init(0, &fiber("init")),
            switch(0, "main running", &fiber("init")),
            init(0, closure),
            begin(0, closure),
            init(0, "inner"),
            begin(0, "inner"),
            switch(0, &fiber("running"), "main suspended"),
            end(0, file, Some("1")),
            switch(0, "main running", &fiber("suspended")),
            end(0, "inner", None),
            end(0, closure, None),
            switch(0, &fiber("dead"), "main suspended"),
            destroy(0, &fiber("dead")),
        ]
    );
}
