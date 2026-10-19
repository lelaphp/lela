//! Retained date storage observations for the Rust replacement proof.
//! The oracle receipts use oracle.toml's recipe and contain no timings.
//! These exercise the existing native bindings, not their Rust replacement.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

#[test]
fn zone_cache_growth_clone_and_individual_release_match_the_oracle() {
    compare(
        include_str!("storage/zones.php"),
        include_bytes!("storage/zones.oracle"),
    );
}

#[test]
fn dates_intervals_periods_and_retained_diagnostics_match_the_oracle() {
    compare(
        include_str!("storage/operations.php"),
        include_bytes!("storage/operations.oracle"),
    );
}

#[test]
fn diagnostic_reentry_ends_borrows_and_preserves_the_replacement_state() {
    compare(
        include_str!("storage/reentry.php"),
        include_bytes!("storage/reentry.oracle"),
    );
}

/// The limit's fatal mid-loop releases every date the array held, and
/// the engine serves the next request as the first. The size the fatal
/// asks for is the oracle's only from php's start, every extension's
/// request state laid, so it is pinned by the witness on
/// `DateTime::__construct` under `[regime.jit-off]` (#2854); this engine
/// lays ext/date's alone and is held to the site, the limit and the exit.
#[test]
fn tight_limit_fatal_releases_every_date_and_serves_again() {
    let engine = Engine::builder()
        .extensions(&[php_ext_date::extension()])
        .ini("date.timezone", "UTC")
        .ini("html_errors", "0")
        .ini("display_errors", "1")
        .ini("log_errors", "0")
        .ini("fatal_error_backtraces", "Off")
        .ini("memory_limit", "2M")
        .sense_leaks(LeakSensor::Armed)
        .build();
    let mut first = None;
    for _ in 0..2 {
        let served = engine.serve_capture_as(
            b"witness.php",
            include_str!("storage/limit.php"),
            Capture::default(),
        );
        assert_eq!(served.exit_code, 255);
        let stdout = String::from_utf8_lossy(&served.stdout).into_owned();
        assert!(
            stdout.starts_with(
                "\nFatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate "
            ),
            "{stdout}"
        );
        assert!(stdout.ends_with("witness.php on line 5\n"), "{stdout}");
        assert_eq!(first.get_or_insert_with(|| stdout.clone()), &stdout);
    }
}

fn compare(source: &str, oracle: &[u8]) {
    let engine = Engine::builder()
        .extensions(&[php_ext_date::extension()])
        .ini("date.timezone", "UTC")
        .ini("html_errors", "0")
        .ini("display_errors", "1")
        .ini("log_errors", "0")
        .ini("fatal_error_backtraces", "Off")
        .ini("memory_limit", "128M")
        .sense_leaks(LeakSensor::Armed)
        .build();
    for _ in 0..2 {
        let served = engine.serve_capture_as(b"witness.php", source, Capture::default());
        assert_eq!(served.fatal, None);
        assert_eq!(served.exit_code, 0);
        assert_eq!(served.stderr, b"");
        assert_eq!(
            String::from_utf8_lossy(&served.stdout),
            String::from_utf8_lossy(oracle)
        );
    }
}
