//! A periodic interrupt as an extension sees it (#1806; ADR-0010,
//! Threads): a sampling profiler's extension asks the request for an
//! interrupt every period, its interrupt hook is served at the loop's
//! next check and reads the periods elapsed, and nothing the engine ran
//! for it outlives the request.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::api::abi::ExtensionBuilder;
use crate::api::request::{Interrupt, Periodic};
use crate::engine::Engine;
use crate::test::Capture;

/// The period the sampler asks for.
const PERIOD: Duration = Duration::from_millis(10);

/// A program that keeps the loop busy until the execution timer stops
/// it, a second in, so the engine's one timer thread serves both.
const BUSY: &str = "<?php for (;;) {}";

/// The sampler's request state: the periodic interrupt its `rinit`
/// started.
#[derive(Default)]
struct Sampler(Option<Periodic>);

/// What one test's sampler saw: the periods each serving of its hook
/// read, the interrupt of the request it started on, and how many held
/// that interrupt when its `rshutdown` was served.
struct Seen {
    periods: Vec<u64>,
    interrupt: Option<Interrupt>,
    at_rshutdown: usize,
}

impl Seen {
    const NONE: Seen = Seen {
        periods: Vec::new(),
        interrupt: None,
        at_rshutdown: 0,
    };
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
                    let periodic = req.limits_mut().start_periodic(PERIOD, Some(PERIOD));
                    req.state::<Sampler>().0 = periodic;
                    noted(&$seen).interrupt = Some(req.limits().interrupt().clone());
                    Ok(())
                })
                .interrupt(|req, beneath| {
                    if let Some(periodic) = req.state::<Sampler>().0 {
                        match req.limits_mut().periods_elapsed(periodic) {
                            0 => {}
                            periods => noted(&$seen).periods.push(periods),
                        }
                    }
                    beneath.serve(req)
                })
                .rshutdown(|req| noted(&$seen).at_rshutdown = req.limits().interrupt().holders())
                .build("sampler")])
            .build()
    };
}

/// The sampler is served while the loop is busy, about once a period,
/// and the periods its hook reads add up to about the time the request
/// ran: none beyond it, and not fewer than three in ten, served at least
/// ten times, which leaves a loaded machine room to wake the timer late,
/// since a late wake counts the periods it missed.
#[test]
fn an_extension_is_interrupted_at_the_period_it_asked_for() {
    static SEEN: Mutex<Seen> = Mutex::new(Seen::NONE);
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
        seen.periods.len() >= 10 && seen.periods.len() as u64 <= periods,
        "served {} times for {periods} periods",
        seen.periods.len()
    );
}

/// ADR-0010: nothing of a periodic interrupt outlives the request that
/// asked for it, though the extension never stopped it: by the time
/// `rshutdown` is served the engine's schedule has let go of the
/// request's interrupt, leaving the request, its deadlines, its heap,
/// which raises it at a delayed OOM, and the sampler's own clone; once the request is gone the sampler's clone is
/// the only one left, and the engine's timer thread waits parked for the
/// next request with nothing scheduled (#2541), joined only before a
/// fork and at the engine's end.
#[test]
fn nothing_of_a_periodic_outlives_the_request_that_asked() {
    static SEEN: Mutex<Seen> = Mutex::new(Seen::NONE);
    let engine = sampling!(SEEN);
    engine.serve_capture(BUSY, Capture::default());
    let mut seen = noted(&SEEN);
    assert!(!seen.periods.is_empty(), "the sampler was served");
    assert_eq!(
        seen.at_rshutdown, 4,
        "the request, its deadlines, its heap, the sampler"
    );
    let holders = seen.interrupt.take().map(|interrupt| interrupt.holders());
    assert_eq!(holders, Some(1), "the sampler's clone alone");
    assert!(
        engine.shared.timer.parks_with_no_request(),
        "the timer's thread parked with nothing scheduled"
    );
}
