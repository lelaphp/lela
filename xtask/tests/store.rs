//! The reading and its store (harness-design.md §4.2): one file per
//! instrument and scope, overwritten each run, and an append-only log.

use std::path::PathBuf;

use xtask::reading::{Reading, Status, Store};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn reading(status: Status, failed: i64) -> Reading {
    let mut reading = Reading::new("witness", "ext/pcre");
    reading.rule = "witness.coverage".into();
    reading.status = status;
    reading.numbers.insert("run".into(), 212.into());
    reading.numbers.insert("failed".into(), failed.into());
    reading.message = "ext/pcre: 3 witnesses differ from the oracle".into();
    reading.adr = "0004".into();
    reading.tree = "9f3c".into();
    reading.commit = "0b62ae0".into();
    reading.at = "2026-09-07T01:05:49Z".parse().unwrap();
    reading
}

#[test]
fn a_reading_is_one_file_in_the_store_and_one_line_in_the_log() {
    let store = Store::at(scratch("one"));
    store.write(&reading(Status::Fail, 3)).unwrap();

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
    store.write(&reading(Status::Fail, 3)).unwrap();
    store.write(&reading(Status::Pass, 0)).unwrap();

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
