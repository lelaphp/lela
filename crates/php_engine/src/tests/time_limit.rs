//! A handler that blocks with the time limit suspended (#1814): an
//! observer's begin handler for a function named `blocks` sleeps past
//! `max_execution_time` and `hard_timeout` both, as a debugger waits on
//! its client, holding [`crate::api::abi::Observed::suspend_time_limit`]'s
//! guard, and the request goes on with the time the limit had left.

use std::time::{Duration, Instant};

use crate::api::abi::{ExtensionBuilder, Observed, ObserverHandlers, ObserverInit};
use crate::engine::Engine;
use crate::test::{Capture, Served};

use super::test_ext::observed_name;

/// php's text for a limit of one second (`Z/Zend/zend_execute_API.c:1441`).
const TIMED_OUT: &str = "Maximum execution time of 1 second exceeded";

/// A loop the limit stops well before it is done, and the line it
/// prints when it was not stopped.
const SPINS: &str = "for ($i = 0; $i < 1000000000; $i++) {}\necho \"not stopped\\n\";";

/// An engine whose limit is one second, past which `hard` more end the
/// process, and whose one extension observes a function named `blocks`
/// with `begin` and declines every other frame.
fn engine(hard: &str, init: ObserverInit) -> Engine {
    Engine::cli_builder()
        .ini("max_execution_time", "1")
        .ini("memory_limit", "2M")
        .ini("hard_timeout", hard)
        .extensions(&[ExtensionBuilder::default().observer(init).build("blocker")])
        .build()
}

/// `begin` for a function named `blocks`, and nothing for another.
fn blocks(observed: &Observed<'_>, begin: fn(&mut Observed<'_>)) -> ObserverHandlers {
    ObserverHandlers {
        begin: (observed_name(observed) == "blocks").then_some(begin),
        end: None,
    }
}

/// `source` served, and the wall-clock time it took.
fn timed(engine: &Engine, source: &str) -> (Served, Duration) {
    let began = Instant::now();
    let served = engine.serve_capture(source, Capture::default());
    (served, began.elapsed())
}

/// The acceptance box: blocked two seconds and more under a limit of one
/// and a `hard_timeout` of one, which unsuspended would have raised the
/// timeout at one second and ended the process at two, the request goes
/// on past the handler; the limit then counts on from the resume with
/// the second it had left, so the loop after it is stopped by php's
/// fatal a second later, not at once and not never.
#[test]
fn a_handler_that_blocks_past_the_limit_with_it_suspended_does_not_end_the_request() {
    fn begin(observed: &mut Observed<'_>) {
        let _untimed = observed.suspend_time_limit();
        std::thread::sleep(Duration::from_millis(2200));
    }
    let engine = engine("1", |observed| blocks(observed, begin));
    let (served, took) = timed(
        &engine,
        &format!("<?php\nfunction blocks() {{}}\nblocks();\necho \"ran\\n\";\n{SPINS}"),
    );
    let stdout = served.stdout_text();
    assert_eq!(stdout.lines().next(), Some("ran"), "{stdout}");
    assert!(!stdout.contains("not stopped"), "{stdout}");
    assert!(
        served
            .fatal
            .as_deref()
            .is_some_and(|fatal| fatal.contains(TIMED_OUT))
    );
    assert!(
        took >= Duration::from_secs(3),
        "the second the limit had left counted from the resume, not {took:?}"
    );
}

/// The control: the same handler with no guard is timed out under, and
/// the fatal is raised as the loop reaches its next check, the callee's
/// activation, before the program goes on. A `hard_timeout` of zero, or
/// the hard stage would end this process.
#[test]
fn a_handler_that_blocks_past_the_limit_unsuspended_is_timed_out() {
    fn begin(_: &mut Observed<'_>) {
        std::thread::sleep(Duration::from_millis(1500));
    }
    let engine = engine("0", |observed| blocks(observed, begin));
    let (served, _) = timed(
        &engine,
        "<?php\nfunction blocks() {}\nblocks();\necho \"ran\\n\";\n",
    );
    assert!(!served.stdout_text().contains("ran"));
    assert!(
        served
            .fatal
            .as_deref()
            .is_some_and(|fatal| fatal.contains(TIMED_OUT))
    );
}

/// The pairing survives a fatal: one raised by PHP code the
/// handler calls under the guard leaves through the guard's drop, so
/// the shutdown function that follows is timed again, and the limit
/// stops it at the second the request had, rather than letting it count
/// its loop out, which takes seconds more.
#[test]
fn a_fatal_under_the_guard_resumes_the_limit() {
    fn begin(observed: &mut Observed<'_>) {
        let mut untimed = observed.suspend_time_limit();
        untimed.call_named(b"bails", &[]).ok();
    }
    let engine = engine("0", |observed| blocks(observed, begin));
    let (served, _) = timed(
        &engine,
        &format!(
            "<?php\nfunction bails() {{ $s = 'x'; for (;;) {{ $s .= $s; }} }}\n\
         function blocks() {{}}\n\
         register_shutdown_function(function () {{\n\
         {SPINS}\n}});\n\
         blocks();\necho \"after\\n\";\n"
        ),
    );
    let stdout = served.stdout_text();
    assert!(stdout.contains("Allowed memory size"), "{stdout}");
    assert!(!stdout.contains("after"), "{stdout}");
    assert!(!stdout.contains("not stopped"), "{stdout}");
    assert!(stdout.contains(TIMED_OUT), "{stdout}");
}
