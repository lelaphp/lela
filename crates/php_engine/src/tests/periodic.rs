//! A periodic interrupt as an extension sees it (#1806; ADR-0010,
//! Threads): a sampling profiler's extension asks the request for an
//! interrupt every period, its interrupt hook is served at the loop's
//! next check and reads the periods elapsed, and nothing the engine ran
//! for it outlives the request.

use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::api::abi::ExtensionBuilder;
use crate::api::request::{Interrupt, Periodic};
use crate::engine::Engine;
use crate::test::Capture;

/// The period the sampler asks for.
const PERIOD: Duration = Duration::from_millis(10);

/// A program that keeps the loop busy until the execution timer stops
/// it, a second in, so the request's one timer thread serves both.
const BUSY: &str = "<?php for (;;) {}";

/// The sampler's request state: the periodic interrupt its `rinit`
/// started.
#[derive(Default)]
struct Sampler(Option<Periodic>);

/// What one test's sampler saw: the periods each serving of its hook
/// read, and the interrupt of the request it started on.
#[derive(Default)]
struct Seen {
    periods: Vec<u64>,
    interrupt: Option<Interrupt>,
}

/// The record behind `seen`, whatever a failed test left behind.
fn noted(seen: &Mutex<Seen>) -> MutexGuard<'_, Seen> {
    seen.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An engine whose one extension starts a periodic interrupt at `rinit`
/// and never stops it, and notes into the static `seen` the interrupt
/// it started on and what its hook reads, as a sampling profiler takes
/// one sample per serving weighted by the periods it stands for. A
/// macro, since a hook is a function and names its record by path.
macro_rules! sampling {
    ($seen:path) => {
        Engine::cli_builder()
            .ini("max_execution_time", "1")
            .extensions(&[ExtensionBuilder::default()
                .request_state::<Sampler>()
                .rinit(|req| {
                    let periodic = req.start_periodic(PERIOD, PERIOD);
                    req.state::<Sampler>().0 = periodic;
                    noted(&$seen).interrupt = Some(req.interrupt().clone());
                    Ok(())
                })
                .interrupt(|req| {
                    if let Some(periodic) = req.state::<Sampler>().0 {
                        match req.periods_elapsed(periodic) {
                            0 => {}
                            periods => noted(&$seen).periods.push(periods),
                        }
                    }
                    Ok(())
                })
                .build("sampler")])
            .build()
    };
}

/// The sampler is served about once a period while the loop is busy,
/// and the periods its hook reads add up to about the time the request
/// ran: none beyond it, and not fewer than a third of it, which leaves a
/// loaded machine room to wake the timer late, since a late wake counts
/// the periods it missed.
#[test]
fn an_extension_is_interrupted_at_the_period_it_asked_for() {
    static SEEN: Mutex<Seen> = Mutex::new(Seen {
        periods: Vec::new(),
        interrupt: None,
    });
    let engine = sampling!(SEEN);
    let began = Instant::now();
    let served = engine.serve_capture(BUSY, Capture::default());
    let ran = began.elapsed();
    assert_eq!(
        served.exit_code, 255,
        "the execution timer's fatal ended it"
    );
    let seen = noted(&SEEN);
    let periods: u64 = seen.periods.iter().sum();
    let at_most = u64::try_from(ran.as_millis() / PERIOD.as_millis()).unwrap_or(u64::MAX);
    assert!(
        (30..=at_most).contains(&periods),
        "{periods} periods of {PERIOD:?} read over {ran:?}"
    );
    assert!(
        !seen.periods.is_empty() && seen.periods.len() as u64 <= periods,
        "served {} times for {periods} periods",
        seen.periods.len()
    );
}

/// ADR-0010: the engine joins every thread it runs, and the periodic
/// interrupt's does not outlive the request that asked, though the
/// extension never stopped it: once the request has ended, nothing
/// raises its interrupt again over many periods.
#[test]
fn no_thread_outlives_the_request_that_asked() {
    static SEEN: Mutex<Seen> = Mutex::new(Seen {
        periods: Vec::new(),
        interrupt: None,
    });
    let engine = sampling!(SEEN);
    engine.serve_capture(BUSY, Capture::default());
    assert!(!noted(&SEEN).periods.is_empty(), "the sampler was served");
    let interrupt = noted(&SEEN).interrupt.take().unwrap_or_default();
    interrupt.raised().store(false, Ordering::Relaxed);
    std::thread::sleep(PERIOD * 10);
    assert!(
        !interrupt.raised().load(Ordering::Relaxed),
        "raised after the request ended"
    );
}
