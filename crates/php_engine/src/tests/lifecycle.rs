//! The Extension seam's lifecycle as `TestExt` records it (design §4.3,
//! §7.1; ADR-0005, ADR-0010), each property that holds today: a scope's
//! states are built in bundle order and dropped in its reverse, on every
//! request a request state is built for and at engine drop.

use super::test_ext::{Event, Events, Phase, Recording, TestExt, record};
use crate::api::abi::{
    Beneath, Extension, ExtensionBuilder, PostStartup as Answer, Refusal, Start, chain,
};
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::ClassId;
use crate::engine::{
    Engine, EngineBuilder, HostPage, HostServes, IniOrigin, Method, NotAlone, RequestDescription,
    WebRequest,
};
use crate::test::{Capture, handed_back};

use Event::{Ext, Mark};
use Phase::{
    Deactivate, EngineEnd, EngineStartRefused, EngineStateBuilt, EngineStateDropped, Error,
    PostDeactivate, PostStartup, ProcessStartRefused, ProcessStateBuilt, ProcessStateDropped,
    ProcessStateReached, RequestStateBuilt, RequestStateDropped, ResourceClosed, Rinit, Rshutdown,
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
    assert_eq!(served.stdout_text(), "ran");
}

/// `php_request_startup` and `php_request_shutdown` as an extension
/// sees them: request state built in bundle order, then each `rinit` in
/// bundle order; at the end each `rshutdown` in reverse, each
/// `deactivate` in reverse, then the resources the request left open
/// closed from the top down (`zend_close_rsrc_list`), which is reverse
/// again for resources opened in bundle order; then each
/// `post_deactivate` in reverse, after `shutdown_executor`
/// (`zend_post_deactivate_modules`); each request state dropped once,
/// after all of it, in reverse.
#[test]
fn a_request_starts_in_bundle_order_and_ends_in_reverse_with_its_resources_closed_last() {
    let rec = Recording::start();
    let engine = Engine::cli_builder().extensions(three()).build();
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
            Ext(2, PostDeactivate),
            Ext(1, PostDeactivate),
            Ext(0, PostDeactivate),
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
    let _engine = Engine::cli_builder().extensions(three()).build();
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
    let engine = Engine::cli_builder().extensions(three()).build();
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
    let ended = |engine| {
        [
            Ext(0, RequestStateBuilt),
            Ext(0, EngineEnd(Some(engine))),
            Ext(0, RequestStateDropped),
            Ext(0, EngineStateDropped(engine)),
        ]
    };
    assert_eq!(events.between("first dropped", "clone dropped").0, ended(0));
    assert_eq!(
        events.between("clone dropped", "second dropped").0,
        ended(1)
    );
}

/// A refusal from the last extension's `post_startup`, as opcache's
/// `accel_post_startup` refuses a configuration it cannot start under:
/// the request laid to ask builds every request state in bundle order,
/// serves nothing, and drops them in reverse, as a served one does.
/// The engine's drop then ends no extension, as no host of php's calls
/// `php_module_shutdown` once `php_module_startup` failed
/// (`Z/sapi/cli/php_cli.c:1341-1350`); its engine states only drop.
#[test]
fn a_refused_request_drops_its_request_state_in_reverse() {
    fn refuse<const N: u8>(_: &mut Request, _: Beneath<'_, chain::PostStartup>) -> Answer {
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
    drop(engine);
    rec.mark("dropped");
    assert_eq!(
        rec.events().between("refused", "dropped").0,
        [
            Ext(2, EngineStateDropped(0)),
            Ext(1, EngineStateDropped(0)),
            Ext(0, EngineStateDropped(0)),
        ]
    );
}

/// Dropping the last clone of an engine ends it: the shutdown request,
/// the mirror of the startup one, builds every request state in bundle
/// order, serves each engine-end function in reverse on them, reaching
/// the engine's own states, and drops the request states in reverse;
/// then each engine state drops once, in reverse bundle order, as php's
/// `zend_destroy_modules` walks the bundle back to front, each
/// `MSHUTDOWN` before that extension's state goes.
#[test]
fn an_engine_ends_on_a_shutdown_request_and_drops_its_engine_state_in_reverse() {
    let rec = Recording::start();
    let engine = Engine::cli_builder().extensions(three()).build();
    serve(&engine);
    rec.mark("served");
    drop(engine);
    rec.mark("dropped");
    assert_eq!(
        rec.events().between("served", "dropped").0,
        [
            Ext(0, RequestStateBuilt),
            Ext(1, RequestStateBuilt),
            Ext(2, RequestStateBuilt),
            Ext(2, EngineEnd(Some(0))),
            Ext(1, EngineEnd(Some(0))),
            Ext(0, EngineEnd(Some(0))),
            Ext(2, RequestStateDropped),
            Ext(1, RequestStateDropped),
            Ext(0, RequestStateDropped),
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
        served.stdout_text(),
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
        served.stdout_text(),
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
    let stdout = served.stdout_text();
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
    fn serve_on<const N: u8>(_: &mut Request, _: Beneath<'_, chain::PostStartup>) -> Answer {
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

/// A request-start function that throws, as session's `RINIT` leaves
/// pending what a session's data threw as it was unserialized.
fn throws<const N: u8>(req: &mut Request) -> Result<(), Thrown> {
    record(N, Rinit(Some(0)));
    Err(crate::api::raising::throw_error(
        req,
        ClassId::ERROR,
        &format!("thrown by {N}"),
    ))
}

/// `program` as the CLI host describes a program it read from stdin.
fn piped(program: &str) -> RequestDescription {
    RequestDescription {
        sapi_name: String::from("cli"),
        script_name: Some(b"Standard input code".to_vec()),
        source: Some(program.as_bytes().to_vec()),
        cwd: std::env::current_dir().expect("the working directory"),
        ..RequestDescription::default()
    }
}

/// An error from `rinit` is php's fatal inside `RINIT` (#1775): the
/// throwable is rendered as php renders one thrown with no frame
/// standing, at `[no active file]` on line zero, the extension after it
/// is handed no start, the program is not served, and no extension is
/// handed a request-end phase, since every php host but the built-in
/// server goes from the failure to `php_module_shutdown` without
/// `php_request_shutdown`. The request states drop, in reverse; the
/// resources the started extensions' `rinit` opened are not closed, as
/// php's `zend_close_rsrc_list` is `php_request_shutdown`'s. The host is
/// told the start failed, with the fatal's status.
#[test]
fn an_error_from_rinit_fails_the_request_start_with_no_request_end_phase() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[
            TestExt::<0>::extension(),
            TestExt::<1>::builder()
                .rinit(throws::<1>)
                .build("test_ext_1"),
            TestExt::<2>::extension(),
        ])
        .build();
    rec.mark("built");
    let mut sink = Capture::default();
    let observed = engine.serve_into(piped("<?php echo 'ran';"), &mut sink);
    rec.mark("served");
    assert_eq!(
        String::from_utf8_lossy(&sink.stdout),
        "\nFatal error: Uncaught Error: thrown by 1 in [no active file]:0\nStack trace:\n#0 {main}\n  thrown in [no active file] on line 0\n"
    );
    assert!(observed.start_failed);
    assert_eq!(observed.exit_code, 255);
    assert_eq!(
        rec.events()
            .between("built", "served")
            .without(&Error(String::new()))
            .0,
        [
            Ext(0, RequestStateBuilt),
            Ext(1, RequestStateBuilt),
            Ext(2, RequestStateBuilt),
            Ext(0, Rinit(Some(0))),
            Ext(1, Rinit(Some(0))),
            Ext(2, RequestStateDropped),
            Ext(1, RequestStateDropped),
            Ext(0, RequestStateDropped),
        ]
    );
}

/// A request-start function that sets a header, as session's `RINIT`
/// under `session.auto_start` sends its cookie.
#[allow(clippy::unnecessary_wraps)] // a `rinit`'s signature
fn sets_header<const N: u8>(req: &mut Request) -> Result<(), Thrown> {
    record(N, Rinit(Some(0)));
    req.response_mut().header_add(b"X-Rinit: 1", true, 0);
    Ok(())
}

/// `program` as php-fpm describes a `GET` of the web.
fn fetched(program: &str) -> RequestDescription {
    RequestDescription {
        sapi_name: String::from("fpm-fcgi"),
        web: Some(WebRequest {
            method: Some(Method::Get),
            response_code: 200,
            ..WebRequest::default()
        }),
        ..piped(program)
    }
}

/// A host that looks at its request once it has started, as php-fpm's
/// loop answers its ping page after `php_request_startup`
/// (`Z/sapi/fpm/fpm/fpm_main.c:1838-1852`), is asked after every
/// extension's `rinit`, and its page's lines replace those of their
/// names that `rinit` set; the code is the page's, the body its own, and
/// the program is not served.
#[test]
fn a_host_looks_at_its_request_after_every_rinit() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::builder()
            .rinit(sets_header::<0>)
            .build("test_ext_0")])
        .build();
    let sink = Box::new(Capture::default());
    rec.mark("built");
    let (observed, sink) = engine.serve(fetched("<?php echo 'ran';"), sink, |_| {
        rec.mark("looked");
        HostServes::Page(HostPage {
            code: 404,
            headers: vec![b"X-Rinit: 2".to_vec(), b"Content-Type: text/plain".to_vec()],
            body: b"pong".to_vec(),
        })
    });
    rec.mark("served");
    let sink: Capture = handed_back(sink).expect("the sink handed back");
    assert!(!observed.start_failed);
    assert_eq!(
        rec.events().between("built", "looked").0,
        [Ext(0, RequestStateBuilt), Ext(0, Rinit(Some(0)))]
    );
    assert_eq!(
        String::from_utf8_lossy(&sink.headers),
        "X-Powered-By: PHP/8.5.12\r\nX-Rinit: 2\r\nContent-type: text/plain;charset=UTF-8\r\n"
    );
    assert_eq!(String::from_utf8_lossy(&sink.stdout), "pong");
}

/// A request whose start failed is never looked at: php-fpm's loop ends
/// the child before its ping page, its `Primary script unknown` and its
/// `Unable to open primary script` (`Z/sapi/fpm/fpm/fpm_main.c:1838-1843`),
/// and the host is handed the failure.
#[test]
fn a_request_whose_start_failed_is_not_looked_at() {
    let _rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::builder()
            .rinit(throws::<0>)
            .build("test_ext_0")])
        .build();
    let sink = Box::new(Capture::default());
    let mut looked = false;
    let (observed, _) = engine.serve(fetched("<?php echo 'ran';"), sink, |_| {
        looked = true;
        HostServes::Nothing
    });
    assert!(observed.start_failed);
    assert!(!looked);
}

/// What the engine start of [`starts_from_ini`] was handed for its two
/// entries.
static STARTED_WITH: std::sync::Mutex<Vec<Option<Vec<u8>>>> = std::sync::Mutex::new(Vec::new());

/// An `on_modify` that takes every value.
#[allow(clippy::unnecessary_wraps)] // an `on_modify`'s signature
fn takes_every_value(_: &mut Request, _: Option<&[u8]>) -> Result<bool, Thrown> {
    Ok(true)
}

/// An engine start that records what its startup ini holds for
/// `t.plain` and `t.handled` in [`STARTED_WITH`].
#[allow(clippy::unnecessary_wraps)] // an engine start's signature
fn records_startup_ini(start: &Start) -> Result<(), Refusal> {
    let value = |name: &[u8]| {
        start
            .ini()
            .entries
            .iter()
            .find(|(entry, _)| entry.as_slice() == name)
            .and_then(|(_, value)| value.clone())
    };
    let mut seen = STARTED_WITH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    seen.clear();
    seen.extend([value(b"t.plain"), value(b"t.handled")]);
    Ok(())
}

/// An extension of two system entries, `t.plain` and `t.handled`, the
/// second with an `on_modify` that takes every value, whose engine start
/// records what its startup ini holds for each.
fn starts_from_ini() -> Extension {
    let entry = |name: &'static str| php_unit::decl::IniDecl {
        name: name.as_bytes(),
        default: Some(b"default"),
        user: false,
        per_dir: false,
        system: true,
    };
    ExtensionBuilder::default()
        .ini(vec![entry("t.plain"), entry("t.handled")])
        .on_modify(b"t.handled", takes_every_value)
        .engine_state(records_startup_ini)
        .build("starts_from_ini")
}

/// A host's own entries are laid over the started engine at
/// `ZEND_INI_STAGE_ACTIVATE` ([`Engine::alter_master_ini`]), as php-fpm's
/// child applies its pool's `php_value` and `php_admin_value` to the
/// engine whose `MINIT` ran once in the master: an extension's engine
/// start saw the startup ini alone, the `-d` value where one was given
/// and the default where none was, and the master value every request
/// starts with is the host's after, whether or not the entry has an
/// `on_modify` (#1944, #2273). With a clone of the engine standing,
/// nothing is laid.
#[test]
fn a_host_s_entries_are_laid_over_the_started_engine() {
    let _rec = Recording::start();
    let mut engine = Engine::cli_builder()
        .extensions(&[starts_from_ini()])
        .ini(b"t.plain", b"line")
        .build();
    let seen = STARTED_WITH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(seen, [Some(b"line".to_vec()), Some(b"default".to_vec())]);
    let pool = |name: &[u8], mode| (name.to_vec(), b"pool".to_vec(), mode);
    let clone = engine.clone();
    assert_eq!(
        engine.alter_master_ini(vec![pool(b"t.plain", IniOrigin::Admin)]),
        Err(NotAlone)
    );
    assert_eq!(engine.ini_get(b"t.plain"), Some(b"line".to_vec()));
    drop(clone);
    let written = engine.alter_master_ini(vec![
        pool(b"t.plain", IniOrigin::Admin),
        pool(b"t.handled", IniOrigin::Value),
    ]);
    assert_eq!(written, Ok(Vec::new()));
    assert_eq!(engine.ini_get(b"t.plain"), Some(b"pool".to_vec()));
    assert_eq!(engine.ini_get(b"t.handled"), Some(b"pool".to_vec()));
}

/// The built-in server is the one host that shuts a request whose start
/// failed down (`php_cli_server_dispatch`,
/// `Z/sapi/cli/php_cli_server.c:2307-2310`): the program is not served,
/// and every request-end phase is served after the fatal, the resources
/// closed among them; no response code is read for its log.
#[test]
fn the_built_in_server_ends_a_request_whose_start_failed() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[
            TestExt::<0>::extension(),
            TestExt::<1>::builder()
                .rinit(throws::<1>)
                .build("test_ext_1"),
        ])
        .build();
    rec.mark("built");
    let sink = Box::new(Capture::default());
    let (observed, dispatched, sink) = engine.dispatch(piped("<?php echo 'ran';"), None, sink);
    rec.mark("served");
    let sink: Capture = handed_back(sink).expect("the sink handed back");
    assert!(observed.start_failed);
    assert_eq!(dispatched.program, None);
    assert!(!String::from_utf8_lossy(&sink.stdout).contains("ran"));
    assert_eq!(
        rec.events()
            .between("built", "served")
            .without(&Error(String::new()))
            .without(&RequestStateBuilt)
            .without(&RequestStateDropped)
            .0,
        [
            Ext(0, Rinit(Some(0))),
            Ext(1, Rinit(Some(0))),
            Ext(1, Rshutdown),
            Ext(0, Rshutdown),
            Ext(1, Deactivate),
            Ext(0, Deactivate),
            Ext(0, ResourceClosed),
            Ext(1, PostDeactivate),
            Ext(0, PostDeactivate),
        ]
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
/// state built drops with the engine. The `error` link of the extension
/// that started is told both diagnostics, as php's `php_error_cb` is the
/// error callback a link saved at `MINIT` while php starts; the refused
/// extension's and the never-started one's are told nothing, since no
/// `MINIT` of theirs installed them (#1953), so the sequence is read
/// without the error links and the links are read apart.
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
        assert_eq!(served.stdout_text(), refused);
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
            .without(&Error(String::new()))
            .0
    };
    assert_eq!(
        requests("start", "built"),
        [Ext(0, EngineStateBuilt(0)), Ext(1, EngineStartRefused)]
    );
    assert_eq!(requests("built", "served"), []);
    let told = |n: u8| -> Vec<String> {
        (events.between("built", "served").0.into_iter())
            .filter_map(|e| match e {
                Ext(at, Error(message)) if at == n => Some(message),
                _ => None,
            })
            .collect()
    };
    let warning = "test_ext_1 refuses to start under 'cli'";
    let fatal = "Unable to start test_ext_1 module";
    assert_eq!(told(0), [warning, fatal, warning, fatal]);
    assert_eq!(told(1), [] as [String; 0]);
    assert_eq!(told(2), [] as [String; 0]);
    assert_eq!(
        events.between("served", "dropped").0,
        [Ext(0, EngineStateDropped(0))]
    );
}

/// The process scope (ADR-0010): two engines built one after the other
/// in one process build the process state once, as the first engine
/// carrying the extension starts it, in bundle order and before that
/// extension's engine start, which is handed it; the second engine's
/// engine start is handed the same one, and no engine's drop drops it,
/// since no host ends the process scope. Its `N` is this test's alone,
/// since the tests of this binary share the process.
#[test]
fn two_engines_in_one_process_build_the_process_state_once_and_never_drop_it() {
    let rec = Recording::start();
    let bundle = || [TestExt::<0>::extension(), TestExt::<100>::with_process()];
    rec.mark("start");
    let first = Engine::cli_builder().extensions(bundle()).build();
    rec.mark("first built");
    let second = Engine::cli_builder().extensions(bundle()).build();
    rec.mark("second built");
    serve(&first);
    serve(&second);
    drop(first);
    drop(second);
    rec.mark("dropped");
    let events = rec.events();
    let starts = |from, to| {
        events
            .between(from, to)
            .without(&RequestStateBuilt)
            .without(&RequestStateDropped)
            .0
    };
    assert_eq!(
        starts("start", "first built"),
        [
            Ext(0, EngineStateBuilt(0)),
            Ext(100, ProcessStateBuilt),
            Ext(100, ProcessStateReached(Some(0))),
            Ext(100, EngineStateBuilt(0)),
        ]
    );
    assert_eq!(
        starts("first built", "second built"),
        [
            Ext(0, EngineStateBuilt(1)),
            Ext(100, ProcessStateReached(Some(0))),
            Ext(100, EngineStateBuilt(1)),
        ]
    );
    let after = events.between("second built", "dropped");
    assert_eq!(
        after.handed(&EngineStateDropped(0)),
        [100, 0],
        "the engines do drop their engine states"
    );
    assert!(
        after
            .only(&[ProcessStateBuilt, ProcessStateDropped])
            .0
            .is_empty()
    );
}

/// Two engines built at once on two threads of one process still build
/// the process state once: the second waits on the first's build and is
/// handed what it built.
#[test]
fn two_engines_built_at_once_build_the_process_state_once() {
    let rec = Recording::start();
    std::thread::scope(|threads| {
        let build = || {
            Engine::cli_builder()
                .extensions(&[TestExt::<101>::with_process()])
                .build()
        };
        let one = threads.spawn(build);
        let other = threads.spawn(build);
        drop((one.join(), other.join()));
    });
    let events = rec.events();
    assert_eq!(events.handed(&ProcessStateBuilt), [101]);
    assert_eq!(events.handed(&ProcessStateReached(Some(0))), [101, 101]);
}

/// A process start that refuses is php's `MINIT` failing on its library
/// (sodium's `sodium_init()`), which php meets once per process and
/// answers by ending the process: here the refusal is remembered for
/// the process, so every later engine carrying the extension is refused
/// with the same diagnostics, and the process start is not called again.
/// No later extension starts, in either engine.
#[test]
fn a_refused_process_start_is_remembered_for_every_later_engine() {
    let rec = Recording::start();
    let bundle = || {
        [
            TestExt::<0>::extension(),
            TestExt::<102>::process_refusing(),
            TestExt::<2>::extension(),
        ]
    };
    let refused = "\nWarning: test_ext_102 refuses to start its process in Unknown on line 0\n\nFatal error: Unable to start test_ext_102 module in Unknown on line 0\nStack trace:\n#0 {main}\n";
    rec.mark("start");
    for _ in 0..2 {
        let engine = Engine::cli_builder()
            .sapi_name("cli")
            .extensions(bundle())
            .build();
        let served = engine.serve_capture("<?php echo 'ran';", Capture::default());
        assert_eq!(served.stdout_text(), refused);
        assert_eq!(served.exit_code, -2);
    }
    rec.mark("end");
    let started = rec.events().between("start", "end").only(&[
        EngineStateBuilt(0),
        ProcessStartRefused,
        ProcessStateBuilt,
    ]);
    assert_eq!(
        started.0,
        [
            Ext(0, EngineStateBuilt(0)),
            Ext(102, ProcessStartRefused),
            Ext(0, EngineStateBuilt(1)),
        ]
    );
}

/// Two extensions that declare process states of one type are each
/// started on their own, the process state held by the extension's name
/// beside its type, and neither is handed the other's start.
#[test]
fn two_extensions_with_one_process_state_type_are_each_started() {
    let rec = Recording::start();
    let named = |name: &str| TestExt::<103>::with_process_named(name);
    let _engine = Engine::cli_builder()
        .extensions(&[named("test_ext_103_a"), named("test_ext_103_b")])
        .build();
    let events = rec.events();
    assert_eq!(events.handed(&ProcessStateBuilt), [103, 103]);
}

/// `ZEND_MOD_REQUIRED(name)`, a row of an extension's `deps` table.
fn required(name: &str) -> crate::api::abi::Dependency {
    crate::api::abi::Dependency {
        name: name.to_owned(),
        kind: crate::api::abi::DependencyKind::Required,
        rel: None,
        version: None,
    }
}

/// An extension named `name` declaring one empty class, `class`, and
/// requiring each of `requires`.
fn declaring(name: &str, class: &str, requires: &[&str]) -> Extension {
    use php_unit::decl::{ClassDecl, ClassFlags, ClassKind, EnumBacking, Name};
    let decl = ClassDecl {
        name: Name(class.as_bytes().into()),
        kind: ClassKind::Class,
        flags: ClassFlags::default(),
        parent: None,
        interfaces: Vec::new(),
        implementors: Vec::new(),
        aliases: Vec::new(),
        traits: Vec::new(),
        adaptations: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        properties: Vec::new(),
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name::default(),
        line: 0,
        end_line: 0,
        object_size: 0,
        object_beside: Box::new([]),
        attribute_validator: None,
    };
    ExtensionBuilder::default()
        .dependencies(requires.iter().map(|name| required(name)).collect())
        .classes(vec![(decl, Vec::new())])
        .build(name)
}

/// `zend_startup_module_ex` (`Z/Zend/zend_API.c:2417-2447`): an
/// extension whose `MODULE_DEP_REQUIRED` row names no extension started
/// before it, compared case-insensitively, is not started and leaves
/// `module_registry`, with php's `E_CORE_WARNING` among what the
/// startup raised; so its classes never register, and an extension
/// requiring it is refused in turn. `pdo` requires `spl`; with `SPL`
/// beside it, the negative control, both start and nothing is raised.
/// The warning stands where php raises it, after the dynamic-library
/// warnings of `php_ini_register_extensions` and before
/// `zend_disable_functions`' own.
#[test]
fn an_extension_whose_required_dependency_is_absent_is_not_started() {
    let pdo = || declaring("PDO", "PDO", &["spl"]);
    let pdo_driver = || declaring("pdo_driver", "PdoDriver", &["pdo"]);
    let probe =
        "<?php var_dump(class_exists('PDO'), extension_loaded('pdo'), class_exists('PdoDriver'));";
    let control = Engine::cli_builder()
        .extensions(&[declaring("SPL", "ArrayObject", &[]), pdo(), pdo_driver()])
        .build();
    let served = control.serve_capture(probe, Capture::default());
    assert_eq!(served.stdout_text(), "bool(true)\nbool(true)\nbool(true)\n");
    let engine = Engine::cli_builder()
        .ini("extension_dir", "/nope")
        .ini("extension", "x")
        .ini("disable_functions", "exit")
        .extensions(&[pdo(), pdo_driver()])
        .build();
    let served = engine.serve_capture(probe, Capture::default());
    let stdout = served.stdout_text();
    let refused = [
        "\nWarning: Cannot load module \"PDO\" because required module \"spl\" is not loaded in Unknown on line 0\n",
        "\nWarning: Cannot load module \"pdo_driver\" because required module \"pdo\" is not loaded in Unknown on line 0\n",
    ]
    .concat();
    let at = stdout.find(&refused);
    assert!(at.is_some(), "{stdout}");
    let library = stdout.find("Unable to load dynamic library 'x'");
    let disabled = stdout.find("Cannot disable function exit()");
    assert!(library < at && at < disabled, "{stdout}");
    assert!(
        stdout.ends_with("bool(false)\nbool(false)\nbool(false)\n"),
        "{stdout}"
    );
}

/// What a host leaves with its engine for the engine end, kept where the
/// test reads it once the engine is gone: the body and the log, each as
/// written.
#[derive(Clone, Default)]
struct Left {
    body: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    log: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl Left {
    fn read(kept: &std::sync::Mutex<Vec<u8>>) -> String {
        let kept = kept
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&kept).into_owned()
    }
}

impl crate::engine::OutputSink for Left {
    fn write(&mut self, bytes: &[u8]) -> Result<(), crate::engine::Aborted> {
        let mut body = (self.body.lock()).unwrap_or_else(std::sync::PoisonError::into_inner);
        body.extend_from_slice(bytes);
        Ok(())
    }

    fn flush(&mut self) {}

    fn send_headers(
        &mut self,
        _: &crate::engine::ResponseHead,
    ) -> Result<(), crate::engine::Aborted> {
        Ok(())
    }

    fn log(&mut self, entry: &[u8]) {
        let mut log = (self.log.lock()).unwrap_or_else(std::sync::PoisonError::into_inner);
        log.extend_from_slice(entry);
        log.push(b'\n');
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        let mut log = (self.log.lock()).unwrap_or_else(std::sync::PoisonError::into_inner);
        log.extend_from_slice(bytes);
    }
}

/// An engine-end function that raises a warning, as an `MSHUTDOWN` may
/// through `php_error_docref`.
fn warns_at_end<const N: u8>(req: &mut Request) {
    record(N, EngineEnd(None));
    crate::api::raising::docref(req, crate::api::raising::Level::Warning, "left unfinished");
}

/// `php_module_shutdown` serves `MSHUTDOWN` with `php_output` not activated: a
/// diagnostic one raises is displayed through `php_output_direct`, which
/// writes the process's stdout (`Z/main/output.c:250`), and, under
/// `log_errors`, logged through `sapi_module.log_message`, since
/// `module_initialized` still holds, labelled `PHP Shutdown`
/// (`php_verror`, `Z/main/main.c:1088-1089`). Here the host leaves the
/// engine a sink for its end ([`EngineBuilder::ends_into`]), as
/// `php_sapi::start` leaves its process's streams and log, and the
/// warning reaches it as php's CLI writes it, displayed and logged;
/// without one, the negative control, it reaches nothing.
#[test]
fn a_diagnostic_raised_at_engine_end_is_displayed_and_logged_where_the_host_left() {
    let rec = Recording::start();
    let warning = || {
        TestExt::<0>::builder()
            .engine_end(warns_at_end::<0>)
            .build("test_ext_0")
    };
    let left = Left::default();
    let engine = Engine::cli_builder()
        .ini("log_errors", "1")
        .extensions(&[warning()])
        .ends_into(Box::new(left.clone()))
        .build();
    serve(&engine);
    drop(engine);
    assert_eq!(
        Left::read(&left.body),
        "\nWarning: PHP Shutdown: left unfinished in Unknown on line 0\n"
    );
    assert_eq!(
        Left::read(&left.log),
        "PHP Warning:  PHP Shutdown: left unfinished in Unknown on line 0\n"
    );
    let unheard = Engine::cli_builder().extensions(&[warning()]).build();
    serve(&unheard);
    drop(unheard);
    assert_eq!(rec.events().handed(&EngineEnd(None)), [0, 0]);
}

/// A tier whose state is `test_ext_3`'s: its request and engine states,
/// its lifecycle and its ini entry `test_ext_3.observe_allocations`.
struct Declaring;

impl crate::tier::Tier for Declaring {
    type State = ();

    fn safepoint(
        _: &mut Request,
        _: php_heap::FrameRef,
        _: crate::tier::Site,
        _: crate::tier::Held<'_>,
    ) -> crate::tier::Flow {
        crate::tier::Flow::Continue
    }

    fn extension() -> Option<Extension> {
        Some(TestExt::<3>::observing_allocations())
    }
}

/// What a program reads of the loaded extensions and of `test_ext_3`'s ini entry
/// on an engine the builder makes, and what `test_ext_3` was handed from
/// the engine's build to its drop.
fn lived(builder: EngineBuilder) -> (String, Events) {
    let rec = Recording::start();
    let engine = builder.build();
    let served = engine.serve_capture(
        "<?php foreach (get_loaded_extensions() as $m) { echo $m, ','; }\n\
         $ini = ini_get('test_ext_3.observe_allocations');\n\
         echo '|', extension_loaded('test_ext_3') ? 'loaded' : 'not loaded', '|', \
         $ini === false ? 'no entry' : $ini;",
        Capture::default(),
    );
    drop(engine);
    (served.stdout_text(), rec.events())
}

/// The tier declares its state as an extension declares it and is no
/// extension (`jit-design.md` §2.3, §4; #2988): an engine bound to a
/// tier whose extension is named `test_ext_3` lists the bundle's
/// extensions and no other, in `get_loaded_extensions()` and `extension_loaded()`,
/// while the tier's ini entry answers `ini_get`, and its request and
/// engine states are built, reached and dropped as the same extension's
/// are in the bundle.
#[test]
fn a_tiers_extension_is_served_as_the_bundles_are_and_is_no_module() {
    let (plain, _) = lived(Engine::cli_builder());
    let (bundled, as_bundled) =
        lived(Engine::cli_builder().extensions(&[TestExt::<3>::observing_allocations()]));
    let (tiered, as_tier) = lived(Engine::cli_builder().tier::<Declaring>());
    let (loaded, _) = plain
        .split_once('|')
        .expect("the loaded extensions, then the rest");
    assert_eq!(plain, format!("{loaded}|not loaded|no entry"));
    assert_eq!(bundled, format!("{loaded}test_ext_3,|loaded|0"));
    assert_eq!(tiered, format!("{loaded}|not loaded|0"));
    assert!(
        as_tier.handed(&EngineStateBuilt(0)) == [3]
            && as_tier.handed(&Rinit(Some(0))) == [3]
            && as_tier.handed(&EngineStateDropped(0)) == [3],
        "{as_tier:?}"
    );
    assert_eq!(as_tier, as_bundled);
}
