//! The reading and its store (harness-design.md §4.2): one file per
//! instrument and scope, overwritten each run, and an append-only log.

use std::path::{Path, PathBuf};

use xtask::manifest::Crate;
use xtask::reading::{self, Outcome, Reading, Status, Store};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A reading of the shape §4.2 draws, stamped through the constructor
/// over the crate; the commit and the time are pinned, since the lines
/// the tests read back are those, and the tree is whatever the crate's
/// sources hash to today.
fn reading(status: Status, failed: i64) -> Reading {
    let krate: Crate = toml::from_str("name = \"ext/pcre\"\nbudget = 6000").unwrap();
    let mut reading = Reading::for_crate("witness", &krate, workspace()).unwrap();
    reading.rule = "witness.coverage".into();
    reading.status = status;
    reading.numbers.insert("run".into(), 212.into());
    reading.numbers.insert("failed".into(), failed.into());
    reading.message = "ext/pcre: 3 witnesses differ from the oracle".into();
    reading.adr = "0004".into();
    reading.commit = "0b62ae0".into();
    reading.at = "2026-09-07T01:05:49Z".parse().unwrap();
    reading
}

#[test]
fn a_reading_is_one_file_in_the_store_and_one_line_in_the_log() {
    let store = Store::at(scratch("one"));
    store.record(&[reading(Status::Fail, 3)]).unwrap();

    let file = store.root().join("witness/ext/pcre.toml");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("instrument = \"witness\""), "{text}");
    assert!(text.contains("status = \"fail\""), "{text}");
    assert!(
        text.contains("at = 2026-09-07T01:05:49Z"),
        "the time is a TOML datetime: {text}"
    );
    assert!(text.contains("failed = 3"), "{text}");

    let log = std::fs::read_to_string(store.root().join("log.jsonl")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 1);
    let line: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(line["instrument"], "witness");
    assert_eq!(line["scope"], "ext/pcre");
    assert_eq!(line["numbers"]["failed"], 3);
    assert_eq!(line["at"], "2026-09-07T01:05:49Z");
}

#[test]
fn a_second_run_overwrites_the_file_and_appends_to_the_log() {
    let store = Store::at(scratch("two"));
    store.record(&[reading(Status::Fail, 3)]).unwrap();
    store.record(&[reading(Status::Pass, 0)]).unwrap();

    let back = store
        .read("witness", "ext/pcre")
        .unwrap()
        .expect("the reading is there");
    assert_eq!(back.status, Status::Pass);
    assert_eq!(back.numbers["failed"], 0.into());
    assert_eq!(back.at.to_string(), "2026-09-07T01:05:49Z");

    let log = std::fs::read_to_string(store.root().join("log.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 2);
    assert_eq!(
        store.readings().unwrap().len(),
        1,
        "one file per instrument and scope"
    );
}

#[test]
fn a_missing_reading_is_none_and_a_missing_store_has_no_readings() {
    let store = Store::at(scratch("none"));
    assert!(store.read("budget", "php_heap").unwrap().is_none());
    assert!(store.readings().unwrap().is_empty());
}

/// The exit-code rule (harness-design.md §4.2, §4.5): an instrument whose
/// readings all pass or are skipped exits 0, and one with any failing
/// reading exits 1, however many it wrote.
#[test]
fn a_failing_reading_is_exit_one_and_a_pass_or_a_skip_is_exit_zero() {
    let mut skipped = reading(Status::Skip, 0);
    skipped.scope = "ext/absent".into();
    assert_eq!(Outcome::of(&[]), Outcome::Met);
    assert_eq!(
        Outcome::of(&[reading(Status::Pass, 0), skipped.clone()]),
        Outcome::Met
    );
    assert_eq!(
        Outcome::of(&[reading(Status::Pass, 0), skipped, reading(Status::Fail, 3)]),
        Outcome::Unmet {
            failed: 1,
            written: 3
        }
    );
    assert!(Outcome::Met.exit().is_ok());
    let err = Outcome::Unmet {
        failed: 1,
        written: 3,
    }
    .exit()
    .unwrap_err();
    assert_eq!(err.to_string(), "1 of 3 readings fail");
}

/// One call records every reading, in order, and says what the exit is.
#[test]
fn a_set_of_readings_is_recorded_together_and_the_outcome_is_the_exit() {
    let store = Store::at(scratch("set"));
    let mut skipped = reading(Status::Skip, 0);
    skipped.scope = "ext/absent".into();
    let outcome = store
        .record(&[reading(Status::Pass, 0), skipped, reading(Status::Fail, 3)])
        .unwrap();
    assert_eq!(
        outcome,
        Outcome::Unmet {
            failed: 1,
            written: 3
        }
    );
    assert_eq!(
        store.readings().unwrap().len(),
        2,
        "two keys, one overwritten"
    );
    let log = store.log().unwrap();
    assert_eq!(log.len(), 3);
    assert_eq!(
        log.iter().map(|r| r.status).collect::<Vec<_>>(),
        [Status::Pass, Status::Skip, Status::Fail]
    );
}

/// A reading's `at` is written by the store's clock, UTC to the second,
/// and reads back as the seconds the silent window is measured in
/// (harness-design.md §7); the calendar is the reading's.
#[test]
fn the_time_is_utc_to_the_second_and_reads_back_as_epoch_seconds() {
    let now = reading::now().unwrap();
    let text = now.to_string();
    assert!(text.ends_with('Z'), "{text}");
    assert_eq!(text.len(), "2026-09-07T01:05:49Z".len(), "{text}");
    let at: toml::value::Datetime = "2026-09-07T01:05:49Z".parse().unwrap();
    assert_eq!(reading::epoch_seconds(&at).unwrap(), 1_788_743_149);
    let offset: toml::value::Datetime = "2026-09-07T03:05:49+02:00".parse().unwrap();
    assert_eq!(reading::epoch_seconds(&offset).unwrap(), 1_788_743_149);
    let leap: toml::value::Datetime = "2024-02-29T00:00:00Z".parse().unwrap();
    assert_eq!(reading::epoch_seconds(&leap).unwrap(), 19_782 * 86_400);
    assert!(reading::epoch_seconds(&now).unwrap() >= reading::epoch_seconds(&at).unwrap());
}

/// A crate not yet created is measured by nothing: its reading through the
/// constructor carries no stamp, so `done` finds it fresh for nothing.
#[test]
fn a_crate_not_yet_created_gets_a_reading_without_a_stamp() {
    let krate: Crate = toml::from_str("name = \"ext/absent\"\nbudget = 6000").unwrap();
    let reading = Reading::for_crate("budget", &krate, workspace()).unwrap();
    assert_eq!(reading.tree(), "");
    assert_eq!(reading.commit, "");
    assert_eq!(reading.at.to_string(), "1970-01-01T00:00:00Z");
    let existing: Crate = toml::from_str("name = \"xtask\"\nbudget = 14000").unwrap();
    let stamped = Reading::for_crate("budget", &existing, workspace()).unwrap();
    assert_eq!(stamped.tree().len(), 40);
    assert!(stamped.at.to_string().ends_with('Z'));
}
