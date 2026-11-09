//! The reading and its store (harness-design.md §4.2): one file per
//! instrument and scope, overwritten each run, an index of one file per
//! key, and an append-only log of one file per instrument.

use std::path::Path;

use xtask::manifest::Crate;
use xtask::reading::{self, Log, Outcome, Reading, Status, Store};
use xtask::scratch::Tree;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn scratch(name: &str) -> Tree {
    Tree::new(&format!("store-{name}")).unwrap()
}

/// A reading of the shape §4.2 draws, stamped through the constructor
/// over the crate; the commit and the time are pinned, since the lines
/// the tests read back are those, and the tree is whatever the crate's
/// sources hash to today. It names a session of its own, which the writer
/// keeps, so a reading read back is the one handed in whatever session
/// the test runs in.
fn reading(status: Status, failed: i64) -> Reading {
    static NOW: std::sync::OnceLock<xtask::key::Snapshot> = std::sync::OnceLock::new();
    let krate = Crate::named("ext/pcre");
    let now = NOW.get_or_init(|| xtask::key::Snapshot::disk(workspace()).unwrap());
    let mut reading = Reading::for_crate("witness", &krate, now).unwrap();
    reading.rule = "witness.coverage".into();
    reading.status = status;
    reading.numbers.insert("run".into(), 212.into());
    reading.numbers.insert("failed".into(), failed.into());
    reading.message = "ext/pcre: 3 witnesses differ from the oracle".into();
    reading.adr = "0004".into();
    reading.commit = "0b62ae0".into();
    reading.at = "2026-09-07T01:05:49Z".parse().unwrap();
    reading.session = "s-witness".into();
    reading
}

#[test]
fn a_reading_is_one_file_in_the_store_and_one_line_in_the_log() {
    let dir = scratch("one");
    let store = Store::at(dir.path());
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

    let log = std::fs::read_to_string(store.root().join("log/witness.jsonl")).unwrap();
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
    let dir = scratch("two");
    let store = Store::at(dir.path());
    store.record(&[reading(Status::Fail, 3)]).unwrap();
    store.record(&[reading(Status::Pass, 0)]).unwrap();

    let back = store
        .read("witness", "ext/pcre")
        .unwrap()
        .expect("the reading is there");
    assert_eq!(back.status, Status::Pass);
    assert_eq!(back.numbers["failed"], 0.into());
    assert_eq!(back.at.to_string(), "2026-09-07T01:05:49Z");

    let log = std::fs::read_to_string(store.root().join("log/witness.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 2);
    assert_eq!(
        store.readings().unwrap().readings.len(),
        1,
        "one file per instrument and scope"
    );
}

#[test]
fn a_missing_reading_is_none_and_a_missing_store_has_no_readings() {
    let dir = scratch("none");
    // Under the scratch tree and not the tree itself: the store is a
    // directory that was never written, which is what a workspace no
    // instrument has run in has.
    let store = Store::at(dir.join("store"));
    assert!(store.read("budget", "php_heap").unwrap().is_none());
    let files = store.readings().unwrap();
    assert!(files.readings.is_empty() && files.unreadable.is_empty());
}

/// The exit-code rule (harness-design.md §4.2, §4.5): an instrument whose
/// readings all pass or are skipped exits 0, and one with any failing
/// reading exits 1, however many it wrote. A measurement alone stands
/// outside it (§4.4, #61): a bench's reading keeps `fail` against its
/// bar, so `status` shows it, and the writer owes no exit for it, since a
/// bench number is a claim a person reads with the A/A control.
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

    // The exception: a measurement's exit is not its status.
    let dir = scratch("measurement");
    let store = Store::at(dir.path());
    let missed = reading(Status::Fail, 3);
    store
        .record_measurement(&[reading(Status::Pass, 0), missed.clone()])
        .unwrap();
    assert_eq!(
        store.read("witness", "ext/pcre").unwrap().as_ref(),
        Some(&missed),
        "the reading records fail and its numbers"
    );
    assert_eq!(store.history("witness").unwrap().readings.len(), 2);
}

/// One call records every reading, in order, and says what the exit is.
#[test]
fn a_set_of_readings_is_recorded_together_and_the_outcome_is_the_exit() {
    let dir = scratch("set");
    let store = Store::at(dir.path());
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
        store.readings().unwrap().readings.len(),
        2,
        "two keys, one overwritten"
    );
    let log = store.history("witness").unwrap().readings;
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
    let krate = Crate::named("ext/absent");
    let reading = Reading::for_crate(
        "budget",
        &krate,
        &xtask::key::Snapshot::disk(workspace()).unwrap(),
    )
    .unwrap();
    assert_eq!(reading.key(), "");
    assert_eq!(reading.commit, "");
    assert_eq!(reading.at.to_string(), "1970-01-01T00:00:00Z");
    let existing = Crate::named("xtask");
    let stamped = Reading::for_crate(
        "budget",
        &existing,
        &xtask::key::Snapshot::disk(workspace()).unwrap(),
    )
    .unwrap();
    assert_eq!(stamped.key().len(), 40);
    assert!(stamped.at.to_string().ends_with('Z'));
}

/// A reading reaches the log whole or not at all (#56): two instruments
/// appending at once, which pre-commit and a track's worktrees do, each
/// hand a line to the file in one write, so a few hundred readings from two
/// writers over one log leave every line parseable and every one present.
/// The lines are longer than macOS's `PIPE_BUF` of 512 bytes, so this
/// exercises the case the writer's rustdoc calls not covered on paper,
/// and shows what the platform does in practice rather than proving it.
#[test]
fn two_writers_over_one_log_tear_no_line() {
    let dir = scratch("two-writers");
    let store = Store::at(dir.path());
    let per_writer: usize = 200;
    let writers: Vec<_> = (0..2)
        .map(|w| {
            let store = store.clone();
            std::thread::spawn(move || {
                for i in 0..per_writer {
                    let mut reading = reading(Status::Fail, i64::try_from(i).unwrap());
                    reading.scope = format!("ext/writer-{w}");
                    reading.message = format!("writer {w} reading {i}: {}", "x".repeat(600));
                    store.record(std::slice::from_ref(&reading)).unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }

    let text = std::fs::read_to_string(store.root().join("log/witness.jsonl")).unwrap();
    assert_eq!(text.lines().count(), 2 * per_writer);
    for (i, line) in text.lines().enumerate() {
        Reading::from_json(line).unwrap_or_else(|e| panic!("line {} is torn: {e:#}", i + 1));
    }
    let log = store.history("witness").unwrap();
    assert_eq!(log.unreadable, Vec::<usize>::new());
    assert_eq!(log.readings.len(), 2 * per_writer);
    for w in 0..2 {
        let of: Vec<usize> = log
            .readings
            .iter()
            .filter(|r| r.scope == format!("ext/writer-{w}"))
            .map(|r| match r.numbers["failed"] {
                reading::Number::Int(n) => usize::try_from(n).unwrap(),
                reading::Number::Float(_) => usize::MAX,
            })
            .collect();
        assert_eq!(
            of,
            (0..per_writer).collect::<Vec<_>>(),
            "writer {w} in order"
        );
    }
}

/// A torn line, which a write the kernel split leaves behind, is counted
/// by its number and named, and the rest of the log is read: the fixture
/// holds the shape found in the maintainer's log, a line that is the head
/// of one reading with another whole inside it, then the tail.
#[test]
fn a_torn_line_is_counted_and_named_and_the_rest_is_read() {
    let dir = scratch("torn");
    let store = Store::at(dir.path());
    let fixture = xtask::package_dir().join("fixtures/store/torn.jsonl");
    std::fs::create_dir_all(store.root().join("log")).unwrap();
    std::fs::copy(&fixture, store.root().join("log/permit.jsonl")).unwrap();

    let log = store.history("permit").unwrap();
    assert_eq!(log.unreadable, vec![2, 3]);
    assert_eq!(log.readings.len(), 2, "{log:#?}");
    assert_eq!(log.readings[0].rule, "harness.self");
    assert_eq!(log.readings[1].rule, "lanes.nightly");
    assert_eq!(
        log.unreadable_note().as_deref(),
        Some("log: 2 lines unreadable at 2, 3.")
    );

    let one = Log {
        readings: Vec::new(),
        unreadable: vec![10_019],
    };
    assert_eq!(
        one.unreadable_note().as_deref(),
        Some("log: 1 line unreadable at 10019.")
    );
    assert_eq!(Log::default().unreadable_note(), None);
    let many = Log {
        readings: Vec::new(),
        unreadable: (1..=7).collect(),
    };
    assert_eq!(
        many.unreadable_note().as_deref(),
        Some("log: 7 lines unreadable at 1, 2, 3, 4, 5 and 2 more.")
    );
}

/// Another store's log, the retire's, is landed one line per write through
/// the same append a reading takes: each reading in its instrument's log
/// and in the index, a line the worktree tore kept as it was in the log's
/// `unread` file, where it tears nothing beside it, and a last line
/// without its newline given one. The other store's log is emptied, so a
/// second landing lands nothing (#2164).
#[test]
fn another_stores_log_is_landed_and_emptied() {
    let dir = scratch("land");
    let store = Store::at(dir.path());
    store.record(&[reading(Status::Pass, 0)]).unwrap();
    let from_dir = scratch("land-from");
    let from = Store::at(from_dir.path());
    let fixture = xtask::package_dir().join("fixtures/store/torn.jsonl");
    let mut foreign = std::fs::read(&fixture).unwrap();
    foreign.pop();
    assert!(!foreign.ends_with(b"\n"), "the last line has no newline");
    let log = from.root().join(reading::LOG);
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(log.join("permit.jsonl"), &foreign).unwrap();

    assert_eq!(store.land(&from).unwrap(), 4);
    store.record(&[reading(Status::Fail, 3)]).unwrap();
    assert_eq!(
        store.land(&from).unwrap(),
        0,
        "the other store's log is emptied"
    );

    let permit = store.history("permit").unwrap();
    assert_eq!(permit.unreadable, Vec::<usize>::new());
    assert_eq!(permit.readings.len(), 2);
    let unread = std::fs::read_to_string(store.root().join("log/unread.jsonl")).unwrap();
    assert_eq!(unread.lines().count(), 2, "{unread}");
    let witness = store.history("witness").unwrap();
    assert_eq!(witness.readings.len(), 2);
    assert_eq!(witness.readings[1].status, Status::Fail);
    let landed = &permit.readings[0];
    let entry = store
        .root()
        .join("permit")
        .join(&landed.scope)
        .join(format!("{}.toml", landed.key()));
    assert!(
        entry.is_file(),
        "the landed reading is in the index: {}",
        entry.display()
    );
}

/// A reading's file is replaced, never rewritten in place (#1235): the
/// new reading goes to a file beside it and is renamed over the path, so
/// a reader holding the old file, here a hard link to it, still reads the
/// old reading whole, where a truncate and a write would have cut it
/// short under the reader; and nothing is left beside the file. A disk
/// that fills mid-write leaves the old file as it stood for the same
/// reason.
#[test]
fn a_reading_is_written_beside_its_file_and_renamed_over_it() {
    let dir = scratch("replace");
    let store = Store::at(dir.path());
    store.record(&[reading(Status::Fail, 3)]).unwrap();
    let file = store.root().join("witness/ext/pcre.toml");
    let held = dir.join("held.toml");
    std::fs::hard_link(&file, &held).unwrap();
    let before = std::fs::read_to_string(&held).unwrap();

    store.record(&[reading(Status::Pass, 0)]).unwrap();
    assert_eq!(
        std::fs::read_to_string(&held).unwrap(),
        before,
        "the old file was written in place"
    );
    let back = store.read("witness", "ext/pcre").unwrap().unwrap();
    assert_eq!(back.status, Status::Pass);
    let beside: Vec<String> = std::fs::read_dir(file.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.path().is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(beside, ["pcre.toml"], "nothing is left beside the file");
}

/// A reading is written under the reading's version, `v = 2`, with the
/// scope's key spelled `key` (harness-design.md §4.2, #1719): in the
/// scope's file, its index entry and its line of the log alike.
#[cfg(tracked)]
#[test]
fn a_reading_is_written_under_its_version_with_its_key() {
    let dir = scratch("v2");
    let store = Store::at(dir.path());
    let written = reading(Status::Pass, 0);
    store.record(std::slice::from_ref(&written)).unwrap();
    assert_eq!(written.key().len(), 40);

    let file = store.root().join("witness/ext/pcre.toml");
    let entry = store
        .root()
        .join("witness/ext/pcre")
        .join(format!("{}.toml", written.key()));
    for path in [&file, &entry] {
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains(&format!("v = {}\n", reading::V)), "{text}");
        assert!(
            text.contains(&format!("key = \"{}\"\n", written.key())),
            "{text}"
        );
        assert!(!text.contains("tree"), "{text}");
    }
    let log = std::fs::read_to_string(store.root().join("log/witness.jsonl")).unwrap();
    let line: serde_json::Value = serde_json::from_str(log.trim_end()).unwrap();
    assert_eq!(line["v"], reading::V);
    assert_eq!(line["key"], written.key());
    assert_eq!(line.get("tree"), None);
    assert_eq!(store.read("witness", "ext/pcre").unwrap(), Some(written));
}

/// A reading written before #1719 carries no `v` and spells its key
/// `tree`. The store reads it as version 1 wherever it stands: a scope's
/// file, a line of the log, and a log another store landed here, whose
/// reading the index then keeps under its key, written again as version 2.
#[test]
fn a_reading_of_the_first_version_spelling_its_key_tree_still_reads() {
    const KEY: &str = "dd29800cbb457eb99968a70299edce74213a45b1";
    let dir = scratch("v1");
    let store = Store::at(dir.path());
    let file = store.root().join("budget/xtask.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        format!(
            "instrument = \"budget\"\nscope = \"xtask\"\nrule = \"\"\nstatus = \"pass\"\nmessage = \"\"\nadr = \"\"\ntree = \"{KEY}\"\ncommit = \"2ebc652\"\nat = 2026-10-03T11:57:08Z\nsession = \"\"\n\n[numbers]\nlines = 30050\n"
        ),
    )
    .unwrap();
    let read = store.read("budget", "xtask").unwrap().unwrap();
    assert_eq!(read.key(), KEY);
    assert_eq!(read.numbers["lines"], 30050.into());

    let line = format!(
        r#"{{"adr":"","at":"2026-10-03T11:57:08Z","commit":"2ebc652","instrument":"budget","message":"","numbers":{{"lines":30050}},"rule":"","scope":"xtask","session":"","status":"pass","tree":"{KEY}"}}"#
    );
    assert_eq!(Reading::from_json(&line).unwrap(), read);

    let from_dir = scratch("v1-from");
    let from = Store::at(from_dir.path());
    std::fs::create_dir_all(from.root().join(reading::LOG)).unwrap();
    std::fs::write(from.root().join("log/budget.jsonl"), format!("{line}\n")).unwrap();
    let other = scratch("v1-into");
    let into = Store::at(other.path());
    assert_eq!(into.land(&from).unwrap(), 1);
    assert_eq!(into.history("budget").unwrap().readings, [read]);
    let entry = into.root().join(format!("budget/xtask/{KEY}.toml"));
    let text = std::fs::read_to_string(&entry).unwrap();
    assert!(
        text.contains(&format!("v = {}\n", reading::V))
            && text.contains(&format!("key = \"{KEY}\"")),
        "{text}"
    );
}

/// A reading whose `v` this harness does not know is refused, from a file
/// and from a line of the log alike, as a `--format jsonl` row's reader
/// refuses one (harness-design.md §4.6): its fields may mean what this
/// build cannot say.
#[test]
fn a_reading_of_a_version_this_harness_does_not_know_is_refused() {
    let dir = scratch("v-unknown");
    let store = Store::at(dir.path());
    let written = reading(Status::Pass, 0);
    let mut line = written.to_json().unwrap();
    line["v"] = (reading::V + 1).into();
    let refused = Reading::from_json(&line.to_string()).unwrap_err();
    assert!(
        format!("{refused:#}").contains(&format!("v = {}", reading::V + 1)),
        "{refused:#}"
    );
    store.record(std::slice::from_ref(&written)).unwrap();
    let file = store.root().join("witness/ext/pcre.toml");
    let text = std::fs::read_to_string(&file).unwrap().replace(
        &format!("v = {}\n", reading::V),
        &format!("v = {}\n", reading::V + 1),
    );
    std::fs::write(&file, text).unwrap();
    assert!(store.read("witness", "ext/pcre").is_err());
}

/// A file of the store that is no reading, one a crash left empty or cut
/// short, is named by its path and skipped, and every other reading is
/// read (#1235), as the log's torn line is.
#[test]
fn an_unreadable_file_is_named_and_the_rest_are_read() {
    let dir = scratch("unreadable");
    let store = Store::at(dir.path());
    store.record(&[reading(Status::Pass, 0)]).unwrap();
    let empty = store.root().join("unsafe-audit/ext/calendar.toml");
    std::fs::create_dir_all(empty.parent().unwrap()).unwrap();
    std::fs::write(&empty, "").unwrap();
    let cut = store.root().join("budget/php_heap.toml");
    std::fs::create_dir_all(cut.parent().unwrap()).unwrap();
    std::fs::write(&cut, "instrument = \"bud").unwrap();

    let files = store.readings().unwrap();
    assert_eq!(files.readings.len(), 1, "{files:#?}");
    assert_eq!(files.readings[0].scope, "ext/pcre");
    assert_eq!(files.unreadable, [cut, empty]);
    assert_eq!(
        files.unreadable_note(store.root()).unwrap(),
        "target/harness: 2 readings unreadable at budget/php_heap.toml, unsafe-audit/ext/calendar.toml; the next run of its instrument writes it again."
    );
}

/// A store's log is `log/<instrument>.jsonl` alone (#2934, #2973): a
/// `log.jsonl` from before revision 83 is no log of the store's, read by
/// no history and landed by no retire, and left where it stands.
#[test]
fn a_log_from_before_revision_83_is_not_read() {
    let dir = scratch("legacy");
    let store = Store::at(dir.path());
    let from_dir = scratch("legacy-from");
    let from = Store::at(from_dir.path());
    let line = format!("{}\n", reading(Status::Pass, 0).to_json().unwrap());
    for one in [&store, &from] {
        std::fs::create_dir_all(one.root()).unwrap();
        std::fs::write(one.root().join("log.jsonl"), &line).unwrap();
    }

    assert!(store.history("witness").unwrap().readings.is_empty());
    assert_eq!(store.land(&from).unwrap(), 0);
    assert!(store.history_all().unwrap().readings.is_empty());
    assert_eq!(
        std::fs::read_to_string(from.root().join("log.jsonl")).unwrap(),
        line,
        "left where it stands"
    );
}
