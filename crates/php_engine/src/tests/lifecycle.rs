//! The Extension seam's lifecycle as `TestExt` records it (design §4.3,
//! §7.1; ADR-0005), each property that holds today. Drop order is not
//! asserted: request state and engine state drop front to back where
//! ADR-0005 says reverse, and #1788 asserts the reverse as it fixes it.

use super::test_ext::{Event, Events, Phase, Recording, TestExt};
use crate::api::abi::Extension;
use crate::engine::Engine;
use crate::test::Capture;

use Event::{Ext, Mark};
use Phase::{
    Deactivate, EngineStartRefused, EngineStateBuilt, EngineStateDropped, RequestStateBuilt,
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
/// dropped once, after all of it.
#[test]
fn a_request_starts_in_bundle_order_and_ends_in_reverse_with_its_resources_closed_last() {
    let rec = Recording::start();
    let engine = Engine::cli_builder().extensions(&three()).build();
    rec.mark("built");
    serve(&engine);
    rec.mark("served");
    let request = rec.events().between("built", "served");
    let dropped = request.handed_each(&RequestStateDropped);
    assert_eq!(dropped, [0, 1, 2]);
    let (handed, last) = request.0.split_at(request.0.len() - dropped.len());
    assert_eq!(
        handed,
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
        ]
    );
    assert!(
        last.iter()
            .all(|e| matches!(e, Ext(_, RequestStateDropped)))
    );
}

/// Building an engine lays the startup request first, on which php's
/// `php_module_startup` registers the ini entries: it builds every
/// request state in bundle order and drops each before any engine state
/// exists. Then each engine state is built once, in bundle order, and
/// nothing else is handed to an extension until a request.
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
    let startup = Events(startup.to_vec());
    assert_eq!(
        startup.without(&RequestStateDropped).0,
        [
            Ext(0, RequestStateBuilt),
            Ext(1, RequestStateBuilt),
            Ext(2, RequestStateBuilt),
        ]
    );
    assert_eq!(startup.handed_each(&RequestStateDropped), [0, 1, 2]);
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

/// An engine start that refuses, php's `MINIT` returning `FAILURE`
/// (ADR-0010): the extension before it starts, it is handed its start
/// with the host's name and refuses, and the one after it never starts,
/// as `zend_startup_module_ex` ends `zend_startup_modules`. The engine
/// serves nothing: every request is the refusal, what the start raised
/// and then php's `Unable to start` fatal as `php -n` writes it, with
/// the status of php's `exit(-2)`, and no extension is handed a
/// request's start or end. The request states built are the rendering
/// request's, which the startup request builds too. The one engine
/// state built drops with the engine.
#[test]
fn an_engine_start_that_refuses_starts_no_later_extension_and_refuses_every_request() {
    let rec = Recording::start();
    rec.mark("start");
    let engine = Engine::cli_builder()
        .sapi_name("cli")
        .extensions(&[
            TestExt::<0>::extension(),
            TestExt::<1>::refusing(),
            TestExt::<2>::extension(),
        ])
        .build();
    rec.mark("built");
    let refused = "\nWarning: test_ext_1 refuses to start under 'cli' in Unknown on line 0\n\nFatal error: Unable to start test_ext_1 module in Unknown on line 0\nStack trace:\n#0 {main}\n";
    for _ in 0..2 {
        let served = engine.serve_capture("<?php echo 'ran';", Capture::default());
        assert_eq!(String::from_utf8_lossy(&served.stdout), refused);
        assert_eq!(served.exit_code, -2);
    }
    rec.mark("served");
    drop(engine);
    rec.mark("dropped");
    let events = rec.events();
    let requests = |from, to| {
        events
            .between(from, to)
            .without(&RequestStateBuilt)
            .without(&RequestStateDropped)
            .0
    };
    assert_eq!(
        requests("start", "built"),
        [Ext(0, EngineStateBuilt(0)), Ext(1, EngineStartRefused)]
    );
    assert_eq!(requests("built", "served"), []);
    assert_eq!(
        events.between("served", "dropped").0,
        [Ext(0, EngineStateDropped(0))]
    );
}
