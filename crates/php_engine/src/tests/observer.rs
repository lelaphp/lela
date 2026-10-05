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
use Phase::{Begin, End, Error, ObserverInit};

/// The observer's events alone, and the diagnostics `zend_error_cb`
/// was handed among them, in the order recorded.
pub(super) fn observed(events: &Events) -> Vec<Event> {
    events
        .only(&[
            ObserverInit(String::new()),
            Begin(String::new()),
            End(String::new(), None),
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
pub(super) fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("php_engine_observer_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// `source` served as `main.php` in `dir`.
pub(super) fn serve_main(engine: &Engine, dir: &Path, source: &str) -> Served {
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
    // is ended before that, after the fatal is rendered. The generator's
    // own frame is not begun at its call (#1811).
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
        [end(0, "b", None), end(0, "a", None), end(0, file, None)],
        "every open frame ended once after the fatal: {resumed:?}"
    );
}
