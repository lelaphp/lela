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
    Deactivate, EngineEnd, EngineStateBuilt, EngineStateDropped, PostDeactivate, RequestStateBuilt,
    RequestStateDropped, ResourceClosed, Rinit, Rshutdown,
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
    assert_eq!(served.stdout, b"ran");
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
