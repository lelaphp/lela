//! The Extension seam's lifecycle as `TestExt` records it (design §4.3,
//! §7.1; ADR-0005, ADR-0010), each property that holds today: a scope's
//! states are built in bundle order and dropped in its reverse, on every
//! request a request state is built for and at engine drop.

use super::test_ext::{Event, Events, Phase, Recording, TestExt, record};
use crate::api::abi::{Extension, PostStartup as Answer};
use crate::api::request::Request;
use crate::engine::Engine;
use crate::test::Capture;

use Event::{Ext, Mark};
use Phase::{
    Deactivate, EngineStateBuilt, EngineStateDropped, PostStartup, RequestStateBuilt,
    RequestStateDropped, ResourceClosed, Rinit, Rshutdown,
};

/// Three recording extensions, in bundle order.
fn three() -> [Extension; 3] {
    [
        TestExt::<0>::extension(),
        TestExt::<1>::extension(),
        TestExt::<2>::extension(),
    ]
}

/// Whether no engine state was built or dropped in `events`, whichever
/// engine's.
fn builds_or_drops_no_engine_state(events: &Events) -> bool {
    events
        .without(&EngineStateBuilt(0))
        .without(&EngineStateDropped(0))
        == *events
}

fn serve(engine: &Engine) {
    let served = engine.serve_capture("<?php echo 'ran';", Capture::default());
    assert_eq!(served.stdout, b"ran");
}

/// `php_request_startup` and `php_request_shutdown` as an extension
/// sees them: request state built in bundle order, then each `rinit` in
/// bundle order; at the end each `rshutdown` in reverse, each
/// `deactivate` in reverse, then the resources the request left open
/// closed from the top down (`zend_close_rsrc_list`), which is reverse
/// again for resources opened in bundle order; each request state
/// dropped once, after all of it, in reverse.
#[test]
fn a_request_starts_in_bundle_order_and_ends_in_reverse_with_its_resources_closed_last() {
    let rec = Recording::start();
    let engine = Engine::cli_builder().extensions(&three()).build();
    rec.mark("built");
    serve(&engine);
    rec.mark("served");
    assert_eq!(
        rec.events().between("built", "served").0,
        [
            Ext(0, RequestStateBuilt),
            Ext(1, RequestStateBuilt),
            Ext(2, RequestStateBuilt),
            Ext(0, Rinit(Some(0))),
            Ext(1, Rinit(Some(0))),
            Ext(2, Rinit(Some(0))),
            Ext(2, Rshutdown),
            Ext(1, Rshutdown),
            Ext(0, Rshutdown),
            Ext(2, Deactivate),
            Ext(1, Deactivate),
            Ext(0, Deactivate),
            Ext(2, ResourceClosed),
            Ext(1, ResourceClosed),
            Ext(0, ResourceClosed),
            Ext(2, RequestStateDropped),
            Ext(1, RequestStateDropped),
            Ext(0, RequestStateDropped),
        ]
    );
}

/// Building an engine lays the startup request first, on which php's
/// `php_module_startup` registers the ini entries: it builds every
/// request state in bundle order and drops each in reverse before any
/// engine state exists. Then each engine state is built once, in bundle
/// order, and nothing else is handed to an extension until a request.
#[test]
fn an_engine_builds_engine_state_in_bundle_order_once_the_startup_request_is_gone() {
    let rec = Recording::start();
    let _engine = Engine::cli_builder().extensions(&three()).build();
    rec.mark("built");
    let events = rec.events().0;
    let engine_start = events
        .iter()
        .position(|e| matches!(e, Ext(_, EngineStateBuilt(_))))
        .unwrap_or(events.len());
    let (startup, after) = events.split_at(engine_start);
    assert_eq!(
        startup,
        [
            Ext(0, RequestStateBuilt),
            Ext(1, RequestStateBuilt),
            Ext(2, RequestStateBuilt),
            Ext(2, RequestStateDropped),
            Ext(1, RequestStateDropped),
            Ext(0, RequestStateDropped),
        ]
    );
    assert_eq!(
        after,
        [
            Ext(0, EngineStateBuilt(0)),
            Ext(1, EngineStateBuilt(0)),
            Ext(2, EngineStateBuilt(0)),
            Mark("built"),
        ]
    );
}

/// Two requests on one engine: each builds request state of its own and
/// is handed the same phases, both reach the one engine state the
/// engine built, and neither builds or drops an engine state.
#[test]
fn two_requests_on_one_engine_build_their_own_request_state_and_share_its_engine_state() {
    let rec = Recording::start();
    let engine = Engine::cli_builder().extensions(&three()).build();
    rec.mark("built");
    serve(&engine);
    rec.mark("first");
    serve(&engine);
    rec.mark("second");
    let events = rec.events();
    let first = events.between("built", "first");
    assert_eq!(first, events.between("first", "second"));
    assert_eq!(first.handed(&RequestStateBuilt), [0, 1, 2]);
    assert_eq!(first.handed(&Rinit(Some(0))), [0, 1, 2]);
    assert!(builds_or_drops_no_engine_state(&first));
}

/// Two engines in one process: each builds engine state of its own, a
/// request reaches its own engine's whichever engine was built first or
/// served last, and dropping an engine drops its state alone, once, when
/// its last clone goes.
#[test]
fn two_engines_in_one_process_each_build_reach_and_drop_their_own_engine_state() {
    let rec = Recording::start();
    rec.mark("start");
    let first = Engine::cli_builder()
        .extensions(&[TestExt::<0>::extension()])
        .build();
    rec.mark("first built");
    let second = Engine::cli_builder()
        .extensions(&[TestExt::<0>::extension()])
        .build();
    rec.mark("second built");
    serve(&second);
    serve(&first);
    serve(&second);
    rec.mark("served");
    let clone = first.clone();
    drop(first);
    rec.mark("first dropped");
    drop(clone);
    rec.mark("clone dropped");
    drop(second);
    rec.mark("second dropped");
    let events = rec.events();
    let built = |from, to| {
        let between = events.between(from, to);
        [0, 1].map(|engine| between.handed(&EngineStateBuilt(engine)))
    };
    assert_eq!(built("start", "first built"), [vec![0], vec![]]);
    assert_eq!(built("first built", "second built"), [vec![], vec![0]]);
    let served = events.between("second built", "served");
    assert!(builds_or_drops_no_engine_state(&served));
    let reached: Vec<Event> = served
        .0
        .into_iter()
        .filter(|e| matches!(e, Ext(_, Rinit(_))))
        .collect();
    assert_eq!(
        reached,
        [
            Ext(0, Rinit(Some(1))),
            Ext(0, Rinit(Some(0))),
            Ext(0, Rinit(Some(1))),
        ]
    );
    assert!(events.between("served", "first dropped").0.is_empty());
    assert_eq!(
        events.between("first dropped", "clone dropped").0,
        [Ext(0, EngineStateDropped(0))]
    );
    assert_eq!(
        events.between("clone dropped", "second dropped").0,
        [Ext(0, EngineStateDropped(1))]
    );
}

/// A refusal from the last extension's `post_startup`, as opcache's
/// `accel_post_startup` refuses a configuration it cannot start under:
/// the request laid to ask builds every request state in bundle order,
/// serves nothing, and drops them in reverse, as a served one does.
#[test]
fn a_refused_request_drops_its_request_state_in_reverse() {
    fn refuse<const N: u8>(_: &mut Request) -> Answer {
        record(N, PostStartup);
        Answer::Refuse { status: 3 }
    }
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[
            TestExt::<0>::extension(),
            TestExt::<1>::extension(),
            TestExt::<2>::builder()
                .post_startup(refuse::<2>)
                .build("test_ext_2"),
        ])
        .build();
    rec.mark("built");
    let served = engine.serve_capture("<?php echo 'ran';", Capture::default());
    rec.mark("refused");
    assert_eq!((served.stdout.as_slice(), served.exit_code), (&b""[..], 3));
    assert_eq!(
        rec.events().between("built", "refused").0,
        [
            Ext(0, RequestStateBuilt),
            Ext(1, RequestStateBuilt),
            Ext(2, RequestStateBuilt),
            Ext(2, PostStartup),
            Ext(2, RequestStateDropped),
            Ext(1, RequestStateDropped),
            Ext(0, RequestStateDropped),
        ]
    );
}

/// Dropping the last clone of an engine drops each engine state once,
/// in reverse bundle order, as php's `zend_destroy_modules` walks its
/// modules back to front; nothing else is handed to an extension.
#[test]
fn an_engine_drops_its_engine_state_in_reverse_bundle_order() {
    let rec = Recording::start();
    let engine = Engine::cli_builder().extensions(&three()).build();
    serve(&engine);
    rec.mark("served");
    drop(engine);
    rec.mark("dropped");
    assert_eq!(
        rec.events().between("served", "dropped").0,
        [
            Ext(2, EngineStateDropped(0)),
            Ext(1, EngineStateDropped(0)),
            Ext(0, EngineStateDropped(0)),
        ]
    );
}

/// A request-end function that raises a fatal, as an extension's
/// `RSHUTDOWN` may through `zend_error_noreturn`.
fn fatal<const N: u8>(req: &mut Request) {
    record(N, Rshutdown);
    let _ = crate::api::raising::fatal(req, &format!("raised by {N}"));
}

/// php serves the request-end functions with nothing executing
/// (`zend_deactivate_modules` empties `EG(current_execute_data)`), so a
/// fatal raised in one is rendered at `Unknown` on line zero, with the
/// trace `zend_fetch_debug_backtrace` takes from no frame, and the
/// request ends in its status.
#[test]
fn a_fatal_in_a_request_end_function_is_raised_where_nothing_is_executing() {
    let _rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::builder()
            .rshutdown(fatal::<0>)
            .build("test_ext_0")])
        .build();
    let served = engine.serve_capture("<?php echo 'ran';", Capture::default());
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "ran\nFatal error: raised by 0 in Unknown on line 0\nStack trace:\n#0 {main}\n"
    );
    assert_eq!(served.exit_code, 255);
}

/// Two request-end functions that each raise a fatal: php serves each
/// in a `zend_try` of its own (`zend_deactivate_modules`,
/// `Z/Zend/zend_API.c:3441-3448`), so both fatals are rendered, the
/// first extension's function is still served after them, and every
/// later phase of the request still runs.
#[test]
fn each_request_end_function_is_guarded_on_its_own() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[
            TestExt::<0>::extension(),
            TestExt::<1>::builder()
                .rshutdown(fatal::<1>)
                .build("test_ext_1"),
            TestExt::<2>::builder()
                .rshutdown(fatal::<2>)
                .build("test_ext_2"),
        ])
        .build();
    rec.mark("built");
    let served = engine.serve_capture("<?php echo 'ran';", Capture::default());
    rec.mark("served");
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "ran\nFatal error: raised by 2 in Unknown on line 0\nStack trace:\n#0 {main}\n\
         \nFatal error: raised by 1 in Unknown on line 0\nStack trace:\n#0 {main}\n"
    );
    assert_eq!(served.exit_code, 255);
    let events = rec.events().between("built", "served");
    assert_eq!(events.handed(&Rshutdown), [2, 1, 0]);
    assert_eq!(events.handed(&Deactivate), [2, 1, 0]);
    assert_eq!(events.handed(&ResourceClosed), [2, 1, 0]);
    assert_eq!(events.handed(&RequestStateDropped), [2, 1, 0]);
}

/// The same guard on `deactivate`, which ADR-0010 gives every end
/// phase: two extensions' `deactivate` each raising a fatal, both
/// rendered, the first extension's still served, and the resources
/// still closed after them.
#[test]
fn each_deactivate_function_is_guarded_on_its_own() {
    fn fatal_deactivate<const N: u8>(req: &mut Request) {
        record(N, Deactivate);
        let _ = crate::api::raising::fatal(req, &format!("raised by {N}"));
    }
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[
            TestExt::<0>::extension(),
            TestExt::<1>::builder()
                .deactivate(fatal_deactivate::<1>)
                .build("test_ext_1"),
            TestExt::<2>::builder()
                .deactivate(fatal_deactivate::<2>)
                .build("test_ext_2"),
        ])
        .build();
    rec.mark("built");
    let served = engine.serve_capture("<?php echo 'ran';", Capture::default());
    rec.mark("served");
    let stdout = String::from_utf8_lossy(&served.stdout);
    assert!(stdout.contains("Fatal error: raised by 2 in Unknown on line 0"));
    assert!(stdout.contains("Fatal error: raised by 1 in Unknown on line 0"));
    let events = rec.events().between("built", "served");
    assert_eq!(events.handed(&Deactivate), [2, 1, 0]);
    assert_eq!(events.handed(&ResourceClosed), [2, 1, 0]);
}

/// However a request ends, its request states drop in reverse: the
/// request laid to ask `post_startup` when it answers serve, a program
/// that calls `exit`, and one that ends in a fatal.
#[test]
fn a_request_drops_its_request_state_in_reverse_however_it_ends() {
    fn serve_on<const N: u8>(_: &mut Request) -> Answer {
        record(N, PostStartup);
        Answer::Serve
    }
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[
            TestExt::<0>::extension(),
            TestExt::<1>::extension(),
            TestExt::<2>::builder()
                .post_startup(serve_on::<2>)
                .build("test_ext_2"),
        ])
        .build();
    rec.mark("built");
    let exited = engine.serve_capture("<?php exit(4);", Capture::default());
    rec.mark("exited");
    let failed = engine.serve_capture("<?php undefined();", Capture::default());
    rec.mark("failed");
    assert_eq!((exited.exit_code, failed.exit_code), (4, 255));
    let events = rec.events();
    let asked_and_exited = events.between("built", "exited");
    assert_eq!(asked_and_exited.handed(&PostStartup), [2]);
    assert_eq!(
        asked_and_exited.handed(&RequestStateDropped),
        [2, 1, 0, 2, 1, 0]
    );
    assert_eq!(
        events
            .between("exited", "failed")
            .handed(&RequestStateDropped),
        [2, 1, 0]
    );
}
