//! `xtask witness`, the witness sensor (design §8; harness-design.md §4.3,
//! §4.5 conditions 1 and 2, §8): the fence pairs of a crate's rustdoc, each
//! snippet run under the oracle's recipe and held to what its fences pin,
//! and the kernels and natives without one.
//!
//! The tests that run a snippet run the pinned oracle, since the engine
//! that will be measured does not exist yet. They say so on stderr and
//! return when the pinned binary is not on this machine; the rest, the
//! extraction and the coverage half, run anywhere.

use std::fs;
use std::path::{Path, PathBuf};

use xtask::divergence::Registry;
use xtask::manifest::Manifest;
use xtask::oracle::Runner;
use xtask::reading::{Number, Status};
use xtask::witness::{self, Ask, Judgement, Tally, fence};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn manifest() -> Manifest {
    Manifest::load(workspace()).unwrap()
}

/// The negative-control fixture of one manifest rule (harness §4.1).
fn fixture(rule: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/rules")
        .join(rule);
    assert!(dir.is_dir(), "no fixture at {}", dir.display());
    dir
}

/// The tracked pin's binary, when it is on this machine.
fn oracle() -> Option<PathBuf> {
    let pin = manifest().pin;
    if pin.oracle.path.is_file() {
        Some(pin.oracle.path)
    } else {
        eprintln!(
            "the pinned php is not at {}; skipping",
            pin.oracle.path.display()
        );
        None
    }
}

/// A source file of this test's own, in a directory of its own.
fn source(name: &str, text: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-witness-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("lib.rs");
    fs::write(&file, text).unwrap();
    file
}

fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}

/// The witnesses of one text, judged with the pinned oracle standing in for
/// the engine.
fn judge(text: &str, registry: &Registry) -> Tally {
    judged(Path::new("ext/fixture/src/lib.rs"), text, registry, false)
}

/// Judging decides and writes nothing; `--accept` is the caller's write, as
/// it is in the instrument. The root is the workspace, which leaves a file
/// named by an absolute path where it stands.
fn judged(file: &Path, text: &str, registry: &Registry, accept: bool) -> Tally {
    let found = fence::extract(file, text);
    let runner = Runner::Engine(oracle().unwrap());
    let ask = Ask {
        crates: Vec::new(),
        accept,
    };
    let manifest = manifest();
    let tally = witness::judge(&manifest.pin.oracle(), registry, &runner, &ask, &found).unwrap();
    if accept {
        witness::capture(workspace(), &tally.captured).unwrap();
    }
    tally
}

fn only(tally: &Tally) -> (Judgement, String) {
    assert_eq!(tally.witnesses.judged.len(), 1, "{tally:#?}");
    let judged = &tally.witnesses.judged[0];
    (judged.judgement, judged.note.clone())
}

/// A block whose snippet prints `1` and whose fences pin what `body` says.
fn block(fences: &str) -> String {
    format!("/// ```php\n/// echo 1;\n/// ```\n{fences}pub fn native() {{}}\n")
}

#[test]
fn a_witness_whose_bytes_are_the_oracle_s_passes() {
    if oracle().is_none() {
        return;
    }
    let tally = judge(&block("/// ```expect\n/// 1\n/// ```\n"), &registry(""));
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
    assert!(tally.missing.is_empty(), "{tally:#?}");
}

#[test]
fn the_digest_is_asserted_once_over_a_run_of_three_witnesses() {
    if oracle().is_none() {
        return;
    }
    let one = |n: u8| {
        format!(
            "/// ```php\n/// echo {n};\n/// ```\n/// ```expect\n/// {n}\n/// ```\npub fn native{n}() {{}}\n"
        )
    };
    let text = format!("{}{}{}", one(1), one(2), one(3));
    let found = fence::extract(Path::new("ext/fixture/src/lib.rs"), &text);
    assert_eq!(found.witnesses.len(), 3, "{found:#?}");
    let binary = oracle().unwrap();
    let manifest = manifest();
    assert_eq!(manifest.pin.digests_asserted(), 0);
    let oracle = manifest.pin.oracle();
    let tally = witness::judge(
        &oracle,
        &registry(""),
        &Runner::Engine(binary),
        &Ask::default(),
        &found,
    )
    .unwrap();
    assert_eq!(tally.witnesses.judged.len(), 3, "{tally:#?}");
    assert!(
        tally
            .witnesses
            .judged
            .iter()
            .all(|j| j.judgement == Judgement::Pass),
        "{tally:#?}"
    );
    assert_eq!(manifest.pin.digests_asserted(), 1);
}

#[test]
fn a_digest_mismatch_refuses_the_run_and_is_not_a_bork_per_witness() {
    if oracle().is_none() {
        return;
    }
    let found = fence::extract(
        Path::new("ext/fixture/src/lib.rs"),
        &block("/// ```expect\n/// 1\n/// ```\n"),
    );
    let mut manifest = manifest();
    manifest.pin.oracle.md5 = "0".repeat(32);
    let err = witness::judge(
        &manifest.pin.oracle(),
        &registry(""),
        &Runner::Absent("no engine".to_owned()),
        &Ask::default(),
        &found,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("the pin says"), "{err}");
}

#[test]
fn the_snippet_runs_as_witness_php_with_the_code_on_line_two() {
    if oracle().is_none() {
        return;
    }
    // php's own error text names the file and the line, which is what the
    // recipe's preamble is for (design §8).
    let text = format!(
        "/// ```php\n/// echo __FILE__, ' ', __LINE__;\n/// ```\n{}pub fn native() {{}}\n",
        "/// ```expect\n/// %switness.php 2\n/// ```\n"
    );
    let tally = judged(
        Path::new("ext/fixture/src/lib.rs"),
        &text,
        &registry(""),
        false,
    );
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
}

#[test]
fn a_witness_that_differs_is_a_difference_the_registry_can_accept() {
    if oracle().is_none() {
        return;
    }
    let text = block("/// ```expect\n/// 2\n/// ```\n");
    let tally = judge(&text, &registry(""));
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Fail, "{tally:#?}");
    assert!(note.contains("-2") && note.contains("+1"), "{note}");

    let id = fence::extract(Path::new("ext/fixture/src/lib.rs"), &text).witnesses[0].id();
    let accepted = format!(
        "[[divergence]]\nid = \"{id}\"\nkind = \"witness\"\nreason = \"A row a person wrote.\"\nstatus = \"accepted\"\nsince = \"2026-09-09\"\n"
    );
    let tally = judge(&text, &registry(&accepted));
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Diverged, "{tally:#?}");
    assert_eq!(note, "A row a person wrote.");
    assert!(!tally.failed(), "an accepted row is not a failure");
}

#[test]
fn the_pinned_second_stream_and_the_status_are_held_apart_from_the_first() {
    if oracle().is_none() {
        return;
    }
    let text = format!(
        "/// ```php\n/// fwrite(STDERR, 'to the second');\n/// echo 'to the first';\n/// ```\n{}pub fn native() {{}}\n",
        "/// ```expect\n/// to the first\n/// ```\n/// ```stderr\n/// to the second\n/// ```\n"
    );
    assert_eq!(only(&judge(&text, &registry(""))).0, Judgement::Pass);
    // The same run against a fence that pins no second stream: an absent
    // `stderr` fence asserts an empty one.
    let without = text.replace("/// ```stderr\n/// to the second\n/// ```\n", "");
    let (judgement, note) = only(&judge(&without, &registry("")));
    assert_eq!(judgement, Judgement::Fail, "{note}");
    assert!(note.contains("to the second"), "{note}");
}

#[test]
fn a_bare_exit_fails_the_run_and_accept_is_what_writes_it() {
    if oracle().is_none() {
        return;
    }
    let text =
        "/// ```php\n/// exit(3);\n/// ```\n/// ```expect exit\n/// ```\npub fn native() {}\n";
    let file = source("accept", text);

    let (judgement, note) = only(&judged(&file, text, &registry(""), false));
    assert_eq!(judgement, Judgement::Unpinned, "{note}");
    assert!(note.contains("--accept"), "{note}");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        text,
        "a run that does not accept writes nothing"
    );

    let tally = judged(&file, text, &registry(""), true);
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
    assert_eq!(tally.captured.len(), 1, "{tally:#?}");
    let written = fs::read_to_string(&file).unwrap();
    assert!(
        written.contains(&format!("```expect exit={}", 3)),
        "the oracle's status is on the fence line: {written}"
    );
    // Nothing else of the file moved.
    assert_eq!(written.lines().count(), text.lines().count());
    assert!(written.contains("/// exit(3);"), "{written}");

    // The witness now pins the status, so a second run accepts nothing.
    let again = judged(&file, &written, &registry(""), true);
    assert!(again.captured.is_empty(), "{again:#?}");
    assert_eq!(only(&again).0, Judgement::Pass);
}

#[test]
fn accept_writes_the_bare_opt_in_alone_and_never_repins_a_status() {
    if oracle().is_none() {
        return;
    }
    // A fence that pins a status the oracle no longer gives, and one that
    // pins none while the oracle exits 3: both are differences for a person
    // to read, and `--accept` leaves both where they stand.
    for fences in [
        format!(
            "/// ```expect exit={}
/// ```
",
            0
        ),
        "/// ```expect
/// ```
"
        .to_owned(),
    ] {
        let text = format!(
            "/// ```php
/// exit(3);
/// ```
{fences}pub fn native() {{}}
"
        );
        let file = source("repin", &text);
        let tally = judged(&file, &text, &registry(""), true);
        assert!(tally.captured.is_empty(), "{tally:#?}");
        assert_eq!(only(&tally).0, Judgement::Fail, "{tally:#?}");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            text,
            "nothing is written"
        );
    }
}

#[test]
fn the_declared_durations_are_summed_and_gate_nothing() {
    if oracle().is_none() {
        return;
    }
    let tally = judge(
        &block("/// ```expect wall=250ms\n/// 1\n/// ```\n"),
        &registry(""),
    );
    assert_eq!(only(&tally).0, Judgement::Pass);
    assert_eq!(tally.wall.as_millis(), 250);
}

/// The negative control of the manifest's coverage rule (harness §4.1, §12
/// item 4): the fixture named by the rule's id must fail under it, with the
/// rule's own text at the failure.
#[test]
fn a_kernel_or_native_without_a_witness_is_missing_and_fails_the_crate() {
    let tracked = manifest();
    let tracked = witness::coverage_rule(&tracked).expect("the manifest carries the coverage rule");
    let root = fixture(&tracked.id);
    let manifest = Manifest::load(&root).unwrap();
    let rule = witness::coverage_rule(&manifest).expect("the fixture carries the rule");
    let krate = manifest.crate_named("ext/pcre").unwrap();
    let found = witness::extract(&root, krate, Some(rule)).unwrap();
    assert_eq!(found.subjects.len(), 2, "{found:#?}");

    let tally = witness::judge(
        &manifest.pin.oracle(),
        &Registry::default(),
        &Runner::Absent("no engine".to_owned()),
        &Ask::default(),
        &fence::Extracted {
            witnesses: Vec::new(),
            ..found
        },
    )
    .unwrap();
    assert_eq!(tally.missing.len(), 1, "{tally:#?}");
    assert!(tally.missing[0].contains("ctype_alpha"), "{tally:#?}");

    let reading = witness::reading(
        &tally,
        &root,
        krate,
        Some(rule),
        &Runner::Absent("no engine".into()),
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(reading.rule, tracked.id);
    assert_eq!(reading.numbers["missing"], Number::Int(1));
    assert_eq!(reading.numbers["subjects"], Number::Int(2));
    assert!(reading.message.contains(&rule.text), "{}", reading.message);
}

#[test]
fn the_reading_carries_run_failed_and_missing_and_is_scoped_to_the_crate() {
    if oracle().is_none() {
        return;
    }
    let manifest = manifest();
    let krate = manifest.crate_named("ext/pcre").unwrap();
    let text = block("/// ```expect\n/// 1\n/// ```\n");
    let tally = judge(&text, &registry(""));
    let reading = witness::reading(
        &tally,
        workspace(),
        krate,
        None,
        &Runner::Engine(oracle().unwrap()),
    )
    .unwrap();
    assert_eq!(reading.instrument, "witness");
    assert_eq!(reading.scope, "ext/pcre");
    assert_eq!(reading.status, Status::Pass);
    assert_eq!(reading.numbers["run"], Number::Int(1));
    assert_eq!(reading.numbers["failed"], Number::Int(0));
    assert_eq!(reading.numbers["missing"], Number::Int(0));
    assert!(!reading.tree.is_empty(), "the reading is stamped");
}

#[test]
fn without_an_engine_the_reading_is_a_skip_that_says_so() {
    if oracle().is_none() {
        return;
    }
    let manifest = manifest();
    let krate = manifest.crate_named("ext/pcre").unwrap();
    let tally = judge(&block("/// ```expect\n/// 1\n/// ```\n"), &registry(""));
    let absent = Runner::Absent("the engine's binary is not built".to_owned());
    let reading = witness::reading(&tally, workspace(), krate, None, &absent).unwrap();
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert!(
        reading.message.contains("the engine ran none"),
        "{}",
        reading.message
    );
    assert!(reading.message.contains("not built"), "{}", reading.message);
}

#[test]
fn a_crate_with_no_witness_and_nothing_that_needs_one_passes() {
    let manifest = manifest();
    let krate = manifest.crate_named("php_proto").unwrap();
    let reading = witness::reading(
        &Tally::default(),
        workspace(),
        krate,
        None,
        &Runner::Absent("no engine".to_owned()),
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(reading.numbers["run"], Number::Int(0));
}

#[test]
fn a_block_the_grammar_cannot_read_fails_the_crate_where_it_stands() {
    let found = fence::extract(
        Path::new("ext/fixture/src/lib.rs"),
        "/// ```php\n/// echo 1;\n/// ```\n/// prose\npub fn native() {}\n",
    );
    let tally = witness::judge(
        &manifest().pin.oracle(),
        &Registry::default(),
        &Runner::Absent("no engine".to_owned()),
        &Ask::default(),
        &found,
    )
    .unwrap();
    assert!(tally.failed(), "{tally:#?}");
    assert_eq!(tally.problems.len(), 1, "{tally:#?}");
    assert!(
        tally.problems[0].contains("ext/fixture/src/lib.rs:1"),
        "{tally:#?}"
    );
}

#[test]
fn the_instrument_writes_one_reading_per_crate_and_names_an_unknown_one() {
    let manifest = manifest();
    let readings = witness::run(
        workspace(),
        &manifest,
        &Registry::default(),
        &Ask {
            crates: vec!["php_proto".to_owned()],
            accept: false,
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    assert_eq!(readings[0].scope, "php_proto");

    let unknown = witness::run(
        workspace(),
        &manifest,
        &Registry::default(),
        &Ask {
            crates: vec!["php_nope".to_owned()],
            accept: false,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(unknown.contains("php_nope"), "{unknown}");
}

#[test]
fn the_harness_crate_carries_no_witness_of_its_own() {
    let manifest = manifest();
    let readings = witness::run(
        workspace(),
        &manifest,
        &Registry::default(),
        &Ask {
            crates: vec!["xtask".to_owned()],
            accept: false,
        },
    )
    .unwrap();
    assert_eq!(readings[0].status, Status::Skip);
    assert!(
        readings[0].message.contains("carries no witness"),
        "{:#?}",
        readings[0]
    );
}

/// A crate whose sources cannot be read is a failing reading naming the
/// crate and the file, never a crate quietly skipped: `done` asks it for a
/// witness reading, and a pass over sources the grammar never saw would
/// say the crate was measured when it was not.
#[test]
fn a_crate_whose_sources_cannot_be_read_fails_and_names_the_crate() {
    let root = std::env::temp_dir().join(format!("xtask-witness-unread-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ext/pcre/src")).unwrap();
    fs::write(
        root.join("ext/pcre/Cargo.toml"),
        "[package]\nname = \"php_ext_pcre\"\n",
    )
    .unwrap();
    // Two bytes no text is: the file is there and is not one the fence
    // grammar can read.
    fs::write(root.join("ext/pcre/src/lib.rs"), [0xff, 0xfe]).unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["add", "-A"],
        vec![
            "-c",
            "user.name=x",
            "-c",
            "user.email=x@x",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "scratch",
        ],
    ] {
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
    let readings = witness::run(
        &root,
        &manifest(),
        &Registry::default(),
        &Ask {
            crates: vec!["ext/pcre".to_owned()],
            accept: false,
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading.message.starts_with("ext/pcre:") && reading.message.contains("src/lib.rs"),
        "{}",
        reading.message
    );
}
