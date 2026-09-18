//! How one witness is judged, held beside the judge: the notes, the digest
//! asserted once per run, the streams and the status held apart, and what
//! `--accept` captures. What a run of the instrument writes is held in
//! `tests/witness.rs`, through `run`.
//!
//! The tests that run a snippet run it through the engine linked into the
//! harness, which is what the lane serves every witness through (harness
//! §7, revision 48; #401), with the pinned oracle beside it for the status
//! a fence pins. They return when the pinned binary is not on this
//! machine, unless the machine promised it (#38).

use std::fs;
use std::path::{Path, PathBuf};

use super::fence;
use super::{Ask, Judgement, Tally, capture, judge};
use crate::divergence::Registry;
use crate::manifest::Manifest;
use crate::oracle::Promise;
use crate::scratch::Tree;

fn workspace() -> PathBuf {
    crate::built_root()
}

fn manifest() -> Manifest {
    Manifest::load(&workspace()).unwrap()
}

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary (#38).
fn oracle() -> Option<PathBuf> {
    let pin = manifest().pin;
    Promise::Oracle
        .or_skip(pin.oracle.file().map(Path::to_path_buf))
        .unwrap()
}

/// A source file of this test's own, in a scratch tree of its own. The
/// tree comes back with the file: it is what holds it there, and the
/// caller binds it for as long as it reads or writes the file.
fn source(name: &str, text: &str) -> (Tree, PathBuf) {
    let dir = Tree::new(&format!("witness-{name}")).unwrap();
    let file = dir.join("lib.rs");
    fs::write(&file, text).unwrap();
    (dir, file)
}

fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}

/// The witnesses of one text, judged as the lane judges them.
fn judge_text(text: &str, registry: &Registry) -> Tally {
    judged(Path::new("ext/fixture/src/lib.rs"), text, registry, false)
}

/// Judging decides and writes nothing; `--accept` is the caller's write, as
/// it is in the instrument. The run of record is the engine linked into the
/// harness, the lane's one adapter. The root is the workspace, which leaves
/// a file named by an absolute path where it stands.
fn judged(file: &Path, text: &str, registry: &Registry, accept: bool) -> Tally {
    let found = fence::extract(file, text);
    let ask = Ask {
        crates: Vec::new(),
        accept,
        staged: false,
    };
    let manifest = manifest();
    let tally = judge(&manifest.pin.oracle(), registry, &ask, &found).unwrap();
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

/// What php writes on the first stream for an uncaught error at line 2,
/// which the engine writes with it, opted in to the status `--accept`
/// captures. The path is `%s`, since the oracle runs the snippet in a
/// scratch directory and the linked engine as `witness.php` (#293).
const FATAL: &str = "/// ```expect exit\n/// Fatal error: Uncaught Error: Call to undefined function nope() in %s:2\n/// Stack trace:\n/// #0 {main}\n///   thrown in %s on line 2\n/// ```\n";

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
    let manifest = manifest();
    assert_eq!(manifest.pin.digests_asserted(), 0);
    let oracle = manifest.pin.oracle();
    let tally = judge(&oracle, &registry(""), &Ask::default(), &found).unwrap();
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

/// Coverage first, the digest only before a snippet runs (#65): with no
/// binary at the pin's path the kernel without a witness still fails the
/// crate, the witness is a snippet that did not run rather than a
/// refusal, and the pin is never hashed.
#[test]
fn an_absent_oracle_leaves_the_coverage_half_judged_and_the_witness_unrun() {
    let text = format!(
        "{}/// A kernel without one.\npub fn kernel() {{}}\n",
        block("/// ```expect\n/// 1\n/// ```\n")
    );
    let found = fence::extract(Path::new("ext/fixture/src/lib.rs"), &text);
    assert_eq!(found.witnesses.len(), 1, "{found:#?}");
    assert_eq!(found.subjects.len(), 2, "{found:#?}");
    let mut manifest = manifest();
    manifest.pin.oracle.path = PathBuf::from("/no-php-here");
    let oracle = manifest.pin.oracle();
    let tally = judge(&oracle, &registry(""), &Ask::default(), &found).unwrap();
    assert!(tally.failed(), "{tally:#?}");
    assert_eq!(tally.missing.len(), 1, "{tally:#?}");
    assert!(tally.witnesses.judged.is_empty(), "{tally:#?}");
    assert_eq!(tally.witnesses.found, 1);
    assert_eq!(tally.witnesses.not_run(), 1);
    assert!(
        tally
            .oracle_absent
            .as_deref()
            .is_some_and(|why| why.contains("/no-php-here")),
        "{tally:#?}"
    );
    assert_eq!(
        manifest.pin.digests_asserted(),
        0,
        "nothing ran, so nothing hashed"
    );
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
    // recipe's preamble is for (design §8). The oracle runs the snippet as
    // `%SCRATCH%/witness.php` and the linked engine, which serves the run
    // of record, names it `witness.php` (#293), and `%s` is one character
    // at least (run-tests.php's `[^\r\n]+`), so the pin both meet is
    // `%s 2`, the one the witness beside `magic_const` in php_compile
    // writes (#364).
    let text = format!(
        "/// ```php\n/// echo __FILE__, ' ', __LINE__;\n/// ```\n{}pub fn native() {{}}\n",
        "/// ```expect\n/// %s 2\n/// ```\n"
    );
    let (judgement, note) = only(&judge_text(&text, &registry("")));
    assert_eq!(judgement, Judgement::Pass, "{note}");
}

#[test]
fn the_snippet_is_named_witness_php_as_the_recipe_names_it() {
    if oracle().is_none() {
        return;
    }
    // The linked adapter serves the snippet under the recipe's
    // `script_name`, so a diagnostic's `in %s on line %d` names the file
    // the fresh-process adapter runs it as, and not `Standard input
    // code`, `serve_capture`'s name for a piped source (#293). The name
    // is pinned literally, where every witness in the tree writes `%s`.
    let file = Path::new("ext/fixture/src/lib.rs");
    let text = format!(
        "/// ```php\n/// ini_set('display_errors', 'stderr');\n/// echo $u, 'to the first';\n/// ```\n{}pub fn native() {{}}\n",
        "/// ```expect\n/// to the first\n/// ```\n/// ```stderr\n/// Warning: Undefined variable $u in witness.php on line 3\n/// ```\n"
    );
    let tally = judged(file, &text, &registry(""), false);
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Pass, "{note}");
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
fn the_second_stream_is_handed_over_and_a_fence_is_held_apart_to_it() {
    if oracle().is_none() {
        return;
    }
    // A snippet whose diagnostic goes to the error stream under the
    // linked engine, which is `Capture`'s `stderr` handed over as the
    // fresh-process adapter hands php's fd 2 over (#248): a fence pinning
    // it passes, and one pinning nothing there fails on what was written,
    // an absent `stderr` fence asserting an empty second stream.
    let text = format!(
        "/// ```php\n/// ini_set('display_errors', 'stderr');\n/// echo $u, 'to the first';\n/// ```\n{}pub fn native() {{}}\n",
        "/// ```expect\n/// to the first\n/// ```\n/// ```stderr\n/// Warning: Undefined variable $u in %s on line 3\n/// ```\n"
    );
    let file = Path::new("ext/fixture/src/lib.rs");
    let tally = judged(file, &text, &registry(""), false);
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Pass, "{note}");
    let without = text.replace(
        "/// ```stderr\n/// Warning: Undefined variable $u in %s on line 3\n/// ```\n",
        "",
    );
    let tally = judged(file, &without, &registry(""), false);
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Fail, "{note}");
    assert!(note.contains("Undefined variable $u"), "{note}");
}

#[test]
fn a_bare_exit_is_unpinned_until_accept_captures_the_status() {
    if oracle().is_none() {
        return;
    }
    // A snippet the engine ends on a status of its own: php's uncaught
    // error, which the engine writes byte for byte and both end 255 on
    // (`exit` is a construct the compiler does not read yet, so the
    // snippet that ended 3 is one the run of record cannot serve).
    let text = format!("/// ```php\n/// nope();\n/// ```\n{FATAL}pub fn native() {{}}\n");
    let text = text.as_str();
    let (_tree, file) = source("accept", text);

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
        written.contains("```expect exit=255"),
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
