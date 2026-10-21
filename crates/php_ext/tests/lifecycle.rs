//! The recording extension through the facade (design §4.3, ADR-0004):
//! the engine tests' `TestExt`, its extension compiled here against
//! `php_ext` alone, so every phase it records is one an extension
//! reaches through this crate. The engine that hosts it is the test
//! face's, named through `php_engine`: `php_ext::test` re-exports it only
//! under this crate's own `test` feature, which `cargo test -p php_ext`
//! does not turn on.

use php_engine::test::{Capture, Engine};
use php_ext as api;

#[path = "../../php_engine/src/tests/test_ext.rs"]
#[allow(dead_code, reason = "the engine's own tests read the rest")]
mod test_ext;

use test_ext::Event::{Ext, Mark};
use test_ext::Phase::{
    Deactivate, EngineEnd, EngineStartRefused, EngineStateBuilt, EngineStateDropped, Error,
    PostDeactivate, RequestStateBuilt, RequestStateDropped, ResourceClosed, Rinit, Rshutdown,
};
use test_ext::{Recording, TestExt};

/// Two extensions declared through the facade, an engine built, one
/// request served and the engine dropped: the startup request, engine
/// state in bundle order, then the request's phases in bundle order and
/// in reverse, each request's states dropped in reverse, and at the drop
/// the shutdown request's engine end in reverse before the engine
/// states drop in reverse.
#[test]
fn an_extension_declared_through_the_facade_is_handed_every_phase_in_order() {
    let rec = Recording::start();
    let engine = Engine::builder()
        .extensions(&[TestExt::<0>::extension(), TestExt::<1>::extension()])
        .build();
    rec.mark("built");
    let served = engine.serve_capture("<?php echo 'ran';", Capture::default());
    rec.mark("served");
    drop(engine);
    rec.mark("dropped");
    assert_eq!(served.stdout_text(), "ran");
    assert_eq!(
        rec.events().0,
        [
            Ext(0, RequestStateBuilt),
            Ext(1, RequestStateBuilt),
            Ext(1, RequestStateDropped),
            Ext(0, RequestStateDropped),
            Ext(0, EngineStateBuilt(0)),
            Ext(1, EngineStateBuilt(0)),
            Mark("built"),
            Ext(0, RequestStateBuilt),
            Ext(1, RequestStateBuilt),
            Ext(0, Rinit(Some(0))),
            Ext(1, Rinit(Some(0))),
            Ext(1, Rshutdown),
            Ext(0, Rshutdown),
            Ext(1, Deactivate),
            Ext(0, Deactivate),
            Ext(1, ResourceClosed),
            Ext(0, ResourceClosed),
            Ext(1, PostDeactivate),
            Ext(0, PostDeactivate),
            Ext(1, RequestStateDropped),
            Ext(0, RequestStateDropped),
            Mark("served"),
            Ext(0, RequestStateBuilt),
            Ext(1, RequestStateBuilt),
            Ext(1, EngineEnd(Some(0))),
            Ext(0, EngineEnd(Some(0))),
            Ext(1, RequestStateDropped),
            Ext(0, RequestStateDropped),
            Ext(1, EngineStateDropped(0)),
            Ext(0, EngineStateDropped(0)),
            Mark("dropped"),
        ]
    );
}

/// An engine start declared through the facade that refuses, with the
/// host's name it was handed in its warning: the extension after it
/// never starts, and the one request asked for is the refusal, with no
/// request phase handed to either extension.
#[test]
fn an_engine_start_declared_through_the_facade_refuses_the_engine() {
    let rec = Recording::start();
    let engine = Engine::builder()
        .sapi_name("cli")
        .ini("html_errors", "0")
        .ini("fatal_error_backtraces", "0")
        .extensions(&[TestExt::<0>::refusing(), TestExt::<1>::extension()])
        .build();
    rec.mark("built");
    let served = engine.serve_capture("<?php echo 'ran';", Capture::default());
    rec.mark("served");
    assert_eq!(
        served.stdout_text(),
        "\nWarning: test_ext_0 refuses to start under 'cli' in Unknown on line 0\n\nFatal error: Unable to start test_ext_0 module in Unknown on line 0\n"
    );
    assert_eq!(served.exit_code, -2);
    // The error links' share of the sequence is #1953's: the extension
    // that never started is told the refusal too.
    assert_eq!(
        rec.events()
            .without(&RequestStateBuilt)
            .without(&RequestStateDropped)
            .without(&Error(String::new()))
            .0,
        [Ext(0, EngineStartRefused), Mark("built"), Mark("served")]
    );
}
