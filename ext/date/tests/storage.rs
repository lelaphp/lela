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

#[test]
fn tight_limit_failure_records_the_transition_counterexample_not_a_new_contract() {
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
    let served = engine.serve_capture_as(
        b"witness.php",
        include_str!("storage/limit.php"),
        Capture::default(),
    );
    assert_eq!(served.exit_code, 255);
    let directory = std::env::current_dir().unwrap_or_default();
    let stdout =
        String::from_utf8_lossy(&served.stdout).replace(&format!("{}/", directory.display()), "");
    assert_eq!(stdout, include_str!("storage/limit.transition"));
    assert_ne!(stdout, include_str!("storage/limit.oracle"));
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
