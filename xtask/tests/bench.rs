//! The negative control of `bench` (harness-design.md §12 item 4, §4.2):
//! the A/A control under `xtask/fixtures/bench/`, whose second arm sleeps
//! 500 ms longer than its first, run through the instrument's library entry.
//! A control the instrument cannot fail is a control that says nothing, so
//! the fixture is the sensor on the sensor.
#![cfg(tracked)]

mod common {
    pub mod repo;
}

use std::path::PathBuf;

use xtask::bench::{self, Label, Machine};
use xtask::reading::{Status, Store};

fn workspace() -> PathBuf {
    xtask::package_dir()
        .parent()
        .expect("xtask sits in the workspace")
        .to_path_buf()
}

/// A repository of its own for one test, so the readings a fixture
/// provokes land in its store and not the workspace's.
fn scratch(name: &str) -> PathBuf {
    let dir = workspace().join("target").join(name);
    common::repo::scratch_workspace(&workspace(), &dir);
    dir
}

/// The A/A control the fixture breaks: one arm sleeps 500 ms longer than
/// the other, so every paired delta clears zero, the sign resolves, and the
/// reading is `fail`. The exit is the other half of the criterion: a
/// measurement owes none, so the run returns while its reading says fail.
#[test]
fn the_control_whose_arms_differ_by_a_known_amount_reads_fail() {
    let root = scratch("bench-negative-control");
    let store = Store::of_workspace(&root);
    let fixture = workspace().join("xtask/fixtures/bench/aa-differs.toml");
    bench::run(
        &root,
        &store,
        None,
        Some(&fixture),
        (6, Machine::Shared),
        (Label::Smoke, ""),
        None,
    )
    .expect("a measurement owes no exit, whatever the reading says");

    let reading = store
        .read("bench", "aa-differs")
        .expect("the store")
        .expect("one reading per bar");
    assert_eq!(reading.status, Status::Fail, "{}", reading.message);
    assert!(reading.message.contains("resolved"), "{}", reading.message);
    // The protocol is read with the standing and the load beside the
    // number, so the reading carries both (§7, §8).
    assert!(reading.message.contains("SMOKE"), "{}", reading.message);
    assert!(reading.message.contains("load "), "{}", reading.message);
    assert!(
        reading.numbers.contains_key("load_1"),
        "{:?}",
        reading.numbers
    );
    // Six measured reps after the two the protocol discards, and the arms
    // wrote nothing, which is the vacuous value of §5.
    assert_eq!(reading.numbers["reps"], 6.into());
    assert_eq!(reading.numbers["warmup_discarded"], 2.into());
    assert!(reading.message.contains("vacuous"), "{}", reading.message);
    assert!(
        reading.numbers["delta_median_ms"].as_f64() < -10.0,
        "{:?}",
        reading.numbers
    );
}

/// §8: a standing with no written reservation is a word and not a label,
/// so the two labels that owe one refuse the run rather than record it.
#[test]
fn a_label_that_owes_a_reservation_refuses_without_one() {
    let root = scratch("bench-unlabelled");
    let store = Store::of_workspace(&root);
    let fixture = workspace().join("xtask/fixtures/bench/aa-differs.toml");
    let refused = bench::run(
        &root,
        &store,
        None,
        Some(&fixture),
        (4, Machine::Shared),
        (Label::BaselineOfRecord, "  "),
        None,
    )
    .expect_err("a label that owes a note refuses without one");
    assert!(
        format!("{refused:#}").contains("written reservation"),
        "{refused:#}"
    );
    assert!(
        store
            .read("bench", "aa-differs")
            .expect("the store")
            .is_none(),
        "a refused run measures nothing and writes nothing"
    );
}
