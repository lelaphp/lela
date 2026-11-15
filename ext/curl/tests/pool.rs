//! `curl_share_init_persistent`'s shares lent from the engine's pool
//! (ADR-0005's amendment): php keeps one `CURLSH` per set
//! of options in `CURL_G(persistent_curlsh)`, one table per php-fpm
//! worker, so the next request on the worker reuses the share and, with
//! `CURL_LOCK_DATA_CONNECT`, its connections. A server of the test's own
//! reports the connections it accepts and closes, so the test reads which
//! share each request held without looking inside libcurl.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use php_ext::test::{Capture, Engine};

/// What a connection tells the server's coordinator.
enum Event {
    Accepted,
    /// A request read, answered once the coordinator sends on the sender.
    Request(Sender<()>),
    Closed,
}

/// What the coordinator reports to the test.
#[derive(Debug, PartialEq)]
enum Seen {
    Accepted,
    Closed,
}

/// A keep-alive HTTP/1.1 server answering `ok` to every request on every
/// connection it accepts, until the client closes it. The first two
/// requests are answered as they come; the third and fourth only once
/// both have been read, which holds two transfers open at once.
fn serve(listener: TcpListener) -> Receiver<Seen> {
    let (events, inbox) = mpsc::channel();
    let (report, seen) = mpsc::channel();
    let accepting = events.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if accepting.send(Event::Accepted).is_err() {
                return;
            }
            let events = accepting.clone();
            std::thread::spawn(move || answer(stream, &events));
        }
    });
    drop(events);
    std::thread::spawn(move || {
        let (mut requests, mut held) = (0, Vec::new());
        for event in inbox {
            match event {
                Event::Accepted => report.send(Seen::Accepted).ok(),
                Event::Closed => report.send(Seen::Closed).ok(),
                Event::Request(go) => {
                    requests += 1;
                    held.push(go);
                    if requests <= 2 || held.len() == 2 {
                        for go in held.drain(..) {
                            go.send(()).ok();
                        }
                    }
                    Some(())
                }
            };
        }
    });
    seen
}

/// One connection's requests answered until it reaches its end.
fn answer(mut stream: TcpStream, events: &Sender<Event>) {
    let mut read = Vec::new();
    let mut buf = [0u8; 1024];
    'connection: loop {
        while let Some(end) = read.windows(4).position(|w| w == b"\r\n\r\n") {
            read.drain(..end + 4);
            let (go, wait) = mpsc::channel();
            events.send(Event::Request(go)).ok();
            let answered = wait.recv_timeout(Duration::from_secs(5)).is_ok()
                && stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .is_ok();
            if !answered {
                break 'connection;
            }
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => read.extend_from_slice(&buf[..n]),
        }
    }
    events.send(Event::Closed).ok();
}

/// What the server reported since the last look, waiting up to five
/// seconds for `at_least` reports to have come.
fn reports(seen: &Receiver<Seen>, at_least: usize) -> Vec<Seen> {
    let mut got: Vec<Seen> = seen.try_iter().collect();
    while got.len() < at_least {
        match seen.recv_timeout(Duration::from_secs(5)) {
            Ok(one) => got.push(one),
            Err(_) => break,
        }
    }
    got
}

/// One request's transfer over the persistent share of
/// `CURL_LOCK_DATA_CONNECT`: the body, then how many connections curl
/// opened for it.
fn transfer(port: u16) -> String {
    format!(
        "<?php $sh = curl_share_init_persistent([CURL_LOCK_DATA_CONNECT]); \
         $c = curl_init('http://127.0.0.1:{port}/'); \
         curl_setopt($c, CURLOPT_SHARE, $sh); curl_setopt($c, CURLOPT_RETURNTRANSFER, true); \
         echo curl_exec($c), ' ', curl_getinfo($c, CURLINFO_NUM_CONNECTS);"
    )
}

/// Two requests served in turn on one engine hold one share, the second
/// taking what the first gave back, and transfer over the one connection
/// the share keeps; two served at once hold two, as two php-fpm workers
/// do, one taking the idle share and the other making its own. Each
/// share's connection is closed as the engine ends, when the pool's
/// shares are cleaned up, and not before.
#[test]
fn a_persistent_share_is_lent_to_one_request_at_a_time() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
    let port = listener.local_addr().map_or(0, |a| a.port());
    let seen = serve(listener);
    let engine = Engine::builder()
        .extensions(&[php_ext_curl::extension()])
        .build();
    let source = transfer(port);

    let mut served = Vec::new();
    for _ in 0..2 {
        let out = engine.serve_capture(&source, Capture::default());
        assert_eq!(out.fatal, None);
        served.push(out.stdout_text());
    }
    assert_eq!(served, ["ok 1", "ok 0"]);
    assert_eq!(reports(&seen, 1), [Seen::Accepted]);

    let mut served: Vec<String> = std::thread::scope(|scope| {
        let both: Vec<_> = (0..2)
            .map(|_| {
                let (engine, source) = (&engine, &source);
                scope.spawn(move || engine.serve_capture(source, Capture::default()))
            })
            .collect();
        both.into_iter()
            .map(|one| {
                let out = one.join().unwrap_or_else(|_| panic!("a request panicked"));
                assert_eq!(out.fatal, None);
                out.stdout_text()
            })
            .collect()
    });
    served.sort();
    assert_eq!(served, ["ok 0", "ok 1"]);
    assert_eq!(reports(&seen, 1), [Seen::Accepted]);

    drop(engine);
    assert_eq!(reports(&seen, 2), [Seen::Closed, Seen::Closed]);
}
