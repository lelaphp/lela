//! The stream layer's idle persistent sockets close first at engine end,
//! as `zend_shutdown` destroys `EG(persistent_list)` before any
//! `MSHUTDOWN` (`Z/Zend/zend.c:1166-1170`): the pool is the layer's
//! engine state, and the engine serves the layer's engine end on the
//! shutdown request before any extension's. A tripwire extension
//! registered after ext/standard, so its engine end is served first of
//! the extensions', reads the peer of a `pfsockopen` socket a request
//! left idle, and finds it closed.

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::request::Request;
use php_ext::test::{Capture, Engine};

/// The accepted end of the pooled connection, which the tripwire reads.
static PEER: Mutex<Option<TcpStream>> = Mutex::new(None);
/// What the tripwire found: whether the peer read the connection's end.
static CLOSED_BEFORE: Mutex<Option<bool>> = Mutex::new(None);

/// The tripwire's engine end: the peer reads the end of the stream where
/// the layer's socket is closed, and times out where it is still open.
fn tripwire_end(_req: &mut Request) {
    let closed = PEER.lock().ok().and_then(|mut peer| {
        let peer = peer.as_mut()?;
        peer.set_read_timeout(Some(Duration::from_millis(500)))
            .ok()?;
        let mut byte = [0u8; 1];
        Some(matches!(peer.read(&mut byte), Ok(0)))
    });
    if let Ok(mut found) = CLOSED_BEFORE.lock() {
        *found = closed;
    }
}

fn tripwire() -> Extension {
    ExtensionBuilder::default()
        .engine_end(tripwire_end)
        .build("persistent_end_tripwire")
}

#[test]
fn the_layers_idle_persistent_sockets_close_before_any_extensions_engine_end() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
    let port = listener.local_addr().map_or(0, |a| a.port());
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
            tripwire(),
        ])
        .build();
    let served = engine.serve_capture(
        &format!("<?php $s = pfsockopen('127.0.0.1', {port}); fwrite($s, 'a');"),
        Capture::default(),
    );
    assert_eq!(served.fatal, None);
    let (mut peer, _) = listener.accept().unwrap_or_else(|e| panic!("{e}"));
    let mut sent = [0u8; 1];
    peer.read_exact(&mut sent).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(&sent, b"a");
    // The request gave the socket back idle: the peer reads nothing yet.
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(peer.read(&mut sent).is_err(), "the pooled socket is open");
    *PEER.lock().unwrap_or_else(|e| panic!("{e}")) = Some(peer);

    drop(engine);
    assert_eq!(
        *CLOSED_BEFORE.lock().unwrap_or_else(|e| panic!("{e}")),
        Some(true),
        "the tripwire's engine end found the layer's idle socket still open"
    );
}
