//! Backpressure over a real socket (#2342), as Ranch's acceptors sleep
//! (research/production-server.r2.md M7): at the connection cap the
//! server stops accepting and new clients wait in the kernel's listen
//! queue, never refused; and whole requests wait in the bounded queue,
//! and on their connections past it, for a free request thread.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use std::time::Duration;

use common::{Ending, Request, Server, read_until_closed};

/// Long enough for any answer on a loaded machine.
const WAIT: Duration = Duration::from_secs(30);

/// Connections past the cap wait, open and unanswered, while the one the
/// cap allows stays open, and each is served once an earlier one closes:
/// none is refused or reset.
#[test]
fn connections_past_the_cap_wait_in_the_listen_queue_and_are_served_in_turn() {
    let root = common::root(
        "cap",
        &[("echo.php", "<?php echo 'served ', $_GET['n'] ?? '-';")],
    );
    let server = Server::with_args(&root, &["--threads", "2", "--limit", "connections=1"]);
    let held = server.connect();
    let mut waiting: Vec<_> = (1..=3)
        .map(|n| server.send(&Request::get(&format!("/echo.php?n={n}")).close()))
        .collect();
    for stream in &mut waiting {
        let early = read_until_closed(stream, Duration::from_millis(500));
        assert_eq!(early.ending, Ending::Open, "{}", early.text());
        assert!(early.bytes.is_empty(), "{}", early.text());
    }
    drop(held);
    for (n, stream) in waiting.iter_mut().enumerate() {
        let answer = read_until_closed(stream, WAIT);
        assert_eq!(answer.codes(), [200], "{}", answer.text());
        assert_eq!(
            String::from_utf8_lossy(&answer.response().body),
            format!("served {}", n + 1)
        );
        assert_eq!(answer.ending, Ending::Closed);
    }
}

/// With one request thread and a queue of one, requests sent together
/// are each served, one at a time and in the order they came, none
/// refused: while one runs, the next waits in the queue and the rest on
/// their connections for room in it. The queue's bound is not seen from
/// the socket; what is seen is that nothing is refused or run at once.
#[test]
fn requests_beyond_the_pool_wait_in_the_queue_and_are_served_in_turn() {
    let root = common::root(
        "queue",
        &[(
            "slow.php",
            "<?php $start = microtime(true); usleep(200000); \
             printf('%s %.6f %.6f', $_GET['n'], $start, microtime(true));",
        )],
    );
    let server = Server::with_args(&root, &["--threads", "1", "--queue", "1"]);
    let mut streams: Vec<_> = (1..=4)
        .map(|n| {
            let stream = server.send(&Request::get(&format!("/slow.php?n={n}")).close());
            // Each is in line before the next is sent, so their order is
            // the one they came in.
            std::thread::sleep(Duration::from_millis(50));
            stream
        })
        .collect();
    let mut spans = Vec::new();
    for (n, stream) in streams.iter_mut().enumerate() {
        let answer = read_until_closed(stream, WAIT);
        assert_eq!(answer.codes(), [200], "{}", answer.text());
        let body = String::from_utf8_lossy(&answer.response().body).into_owned();
        let words: Vec<&str> = body.split(' ').collect();
        assert_eq!(
            words.first().copied(),
            Some((n + 1).to_string().as_str()),
            "{body}"
        );
        let time = |at: usize| -> f64 {
            words
                .get(at)
                .and_then(|word| word.parse().ok())
                .unwrap_or_else(|| panic!("no time in {body}"))
        };
        spans.push((time(1), time(2)));
    }
    for pair in spans.windows(2) {
        let ((_, ended), (started, _)) = (pair[0], pair[1]);
        assert!(started >= ended, "served at once: {spans:?}");
    }
}
