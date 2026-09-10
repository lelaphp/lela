//! How one witness is judged, held beside the judge: the notes, the digest
//! asserted once per run, the streams and the status held apart, and what
//! `--accept` captures. What a run of the instrument writes is held in
//! `tests/witness.rs`, through `run`.
//!
//! The tests that run a snippet run the pinned oracle standing in for the
//! engine, since the engine that will be measured does not exist yet. They
//! say so on stderr and return when the pinned binary is not on this
//! machine.

use std::fs;
use std::path::{Path, PathBuf};

use super::fence;
use super::{Ask, Judgement, Tally, capture, judge};
use crate::divergence::Registry;
use crate::manifest::Manifest;
use crate::oracle::Runner;

fn workspace() -> PathBuf {
    crate::built_root()
}

fn manifest() -> Manifest {
    Manifest::load(&workspace()).unwrap()
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
fn judge_text(text: &str, registry: &Registry) -> Tally {
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
        staged: false,
    };
    let manifest = manifest();
    let tally = judge(&manifest.pin.oracle(), registry, &runner, &ask, &found).unwrap();
    if accept {
        capture(&workspace(), &tally.captured).unwrap();
    }
    tally
}

fn only(tally: &Tally) -> (Judgement, String) {
    assert_eq!(tally.witnesses.judged.len(), 1, "{tally:#?}");
    let judged = &tally.witnesses.judged[0];
    (judged.judgement, judged.note.clone())
}

/// A block whose snippet prints `1` and whose fences pin what `fences` says.
fn block(fences: &str) -> String {
    format!("/// ```php\n/// echo 1;\n/// ```\n{fences}pub fn native() {{}}\n")
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
    let tally = judge(
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
    let err = judge(
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
    let tally = judge_text(&text, &registry(""));
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
}

#[test]
fn a_witness_that_differs_notes_the_difference_and_an_accepted_row_notes_its_reason() {
    if oracle().is_none() {
        return;
    }
    let text = block("/// ```expect\n/// 2\n/// ```\n");
    let tally = judge_text(&text, &registry(""));
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Fail, "{tally:#?}");
    assert!(note.contains("-2") && note.contains("+1"), "{note}");

    let id = fence::extract(Path::new("ext/fixture/src/lib.rs"), &text).witnesses[0].id();
    let accepted = format!(
        "[[divergence]]\nid = \"{id}\"\nkind = \"witness\"\nreason = \"A row a person wrote.\"\nstatus = \"accepted\"\nsince = \"2026-09-09\"\n"
    );
    let tally = judge_text(&text, &registry(&accepted));
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
    assert_eq!(only(&judge_text(&text, &registry(""))).0, Judgement::Pass);
    // The same run against a fence that pins no second stream: an absent
    // `stderr` fence asserts an empty one.
    let without = text.replace("/// ```stderr\n/// to the second\n/// ```\n", "");
    let (judgement, note) = only(&judge_text(&without, &registry("")));
    assert_eq!(judgement, Judgement::Fail, "{note}");
    assert!(note.contains("to the second"), "{note}");
}

#[test]
fn a_bare_exit_is_unpinned_until_accept_captures_the_status() {
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

    // The witness now pins the status, so a second run accepts nothing.
    let again = judged(&file, &written, &registry(""), true);
    assert!(again.captured.is_empty(), "{again:#?}");
    assert_eq!(only(&again).0, Judgement::Pass);
}

#[test]
fn the_declared_durations_are_summed_and_gate_nothing() {
    if oracle().is_none() {
        return;
    }
    let tally = judge_text(
        &block("/// ```expect wall=250ms\n/// 1\n/// ```\n"),
        &registry(""),
    );
    assert_eq!(only(&tally).0, Judgement::Pass);
    assert_eq!(tally.wall.as_millis(), 250);
}

#[test]
fn a_block_the_grammar_cannot_read_fails_the_crate_where_it_stands() {
    let found = fence::extract(
        Path::new("ext/fixture/src/lib.rs"),
        "/// ```php\n/// echo 1;\n/// ```\n/// prose\npub fn native() {}\n",
    );
    let tally = judge(
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
