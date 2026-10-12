//! The comparison from inside: two kept runs diffed and the reading they
//! come to, over the fixture at `xtask/fixtures/phpt/against/`, which
//! plants one regression. Nothing here spawns php, so the negative control
//! runs on every machine; the run kept and found by a commit is held from
//! outside, in `tests/phpt.rs`.

use std::fs;
use std::path::Path;

use super::*;
use crate::scratch::{Tree, committing};

fn fixture(name: &str) -> Run {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/phpt/against")
        .join(name);
    serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap()
}

fn reading_of(diff: Result<Diff, String>) -> Reading {
    compared(
        Reading::new(INSTRUMENT, "ext/fixture@against"),
        "882bbb2",
        diff,
    )
    .unwrap()
}

fn count(reading: &Reading, key: &str) -> i64 {
    match reading.numbers.get(key) {
        Some(crate::reading::Number::Int(n)) => *n,
        other => panic!("{key} is {other:?} in {reading:#?}"),
    }
}

#[test]
fn the_planted_regression_fails_the_reading_and_is_named() {
    let diff = diff(&fixture("base.json"), &fixture("head.json"), None).unwrap();
    assert_eq!(
        diff.regressed,
        [(
            "phpt:php-src/ext/fixture/tests/planted.phpt".to_owned(),
            Judgement::Fail
        )]
    );
    // kept, planted and diverged were met at the base; the skip met
    // nothing there, so its fail here is no regression.
    assert_eq!(diff.held, 3);
    assert_eq!(diff.fixed, 1);
    let reading = reading_of(Ok(diff));
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading
            .message
            .contains("1 fail here: ext/fixture/tests/planted.phpt"),
        "{}",
        reading.message
    );
    assert_eq!(count(&reading, "regressed"), 1);
    assert_eq!(count(&reading, "held"), 3);
    assert_eq!(count(&reading, "fixed"), 1);
}

#[test]
fn the_same_run_against_itself_passes() {
    let base = fixture("base.json");
    let reading = reading_of(diff(&base, &base, None));
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert!(
        reading.message.contains("none fails here"),
        "{}",
        reading.message
    );
    assert_eq!(count(&reading, "regressed"), 0);
}

/// Neither a test the compiler no longer reads nor one that skips here
/// meets a test met at the base (#725).
#[test]
fn a_test_the_compiler_no_longer_reads_or_that_skips_is_a_regression() {
    let base = fixture("base.json");
    for now in [Judgement::Unread, Judgement::Skip] {
        let mut head = base.clone();
        head.verdicts
            .insert("phpt:php-src/ext/fixture/tests/kept.phpt".to_owned(), now);
        let diff = diff(&base, &head, None).unwrap();
        assert_eq!(diff.regressed.len(), 1, "{diff:?}");
        assert_eq!(diff.regressed[0].1, now);
    }
}

/// Every way two runs fail to compare is a skip that says why, and never
/// the pass "no regressions" would be.
#[test]
fn runs_that_do_not_compare_give_no_verdict() {
    let base = fixture("base.json");

    let mut other_corpus = fixture("head.json");
    other_corpus.corpus = "php-8.5.10 0123abcd".to_owned();
    let mut fewer = fixture("head.json");
    fewer
        .verdicts
        .remove("phpt:php-src/ext/fixture/tests/planted.phpt");
    let mut none_met = base.clone();
    for verdict in none_met.verdicts.values_mut() {
        *verdict = Judgement::Fail;
    }

    for (head, why) in [
        (&other_corpus, "over the corpus"),
        (&fewer, "not the same tests"),
        (&none_met, "nothing is held"),
    ] {
        let was = if why == "nothing is held" {
            head
        } else {
            &base
        };
        let reading = reading_of(diff(was, head, None));
        assert_eq!(reading.status, Status::Skip, "{reading:#?}");
        assert!(
            reading.message.contains("no verdict"),
            "{}",
            reading.message
        );
        assert!(reading.message.contains(why), "{}", reading.message);
        assert!(reading.numbers.is_empty(), "{reading:#?}");
    }
}

#[test]
fn the_exit_is_the_comparisons_whatever_the_subsets_say() {
    let mut subset = Reading::new(INSTRUMENT, "ext/fixture");
    subset.status = Status::Fail;
    let mut against = Reading::new(INSTRUMENT, "ext/fixture@against");
    against.status = Status::Pass;
    assert!(answer(&[subset.clone(), against.clone()]).is_ok());
    for status in [Status::Fail, Status::Skip] {
        against.status = status;
        let err = answer(&[subset.clone(), against.clone()]).unwrap_err();
        assert!(err.to_string().contains("ext/fixture@against"), "{err:#}");
    }
}

#[test]
fn a_kept_run_is_spelled_as_the_fixture_is() {
    let base = fixture("base.json");
    let text = serde_json::to_string_pretty(&base).unwrap();
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/phpt/against/base.json");
    assert_eq!(text.trim(), fs::read_to_string(file).unwrap().trim());
}

/// A run kept in a linked worktree at the base, where the skip asks for one
/// to be taken, is found by the commit from main's checkout (#1869). Before
/// it is kept, the skip names each worktree it looked in once, the one it
/// runs in first; from the linked one, main comes after it.
#[test]
fn a_run_kept_in_a_linked_worktree_at_the_base_is_found_from_main() {
    let main = Tree::new("against-main").unwrap();
    let beside = Tree::new("against-beside").unwrap();
    committing(&main, &["init", "-q", "-b", "main"]);
    fs::write(main.join("Cargo.toml"), "[workspace]\n").unwrap();
    committing(&main, &["add", "--", "Cargo.toml"]);
    committing(&main, &["commit", "-q", "-m", "base"]);
    let linked = beside.join("base");
    let linked_str = linked.to_str().unwrap();
    committing(
        &main,
        &["worktree", "add", "-q", "--detach", linked_str, "HEAD"],
    );

    let base = Base::at(&main, "HEAD").unwrap();
    let looked = roots(&main);
    assert_eq!(looked, [main.path(), &linked], "{looked:?}");
    assert_eq!(roots(&linked), [&linked, main.path()]);

    let why = base.run(&looked, "ext/fixture").unwrap().unwrap_err();
    let main_named = format!("{}, ", main.display());
    assert_eq!(why.matches(&main_named).count(), 1, "{why}");
    assert_eq!(why.matches(linked_str).count(), 1, "{why}");
    assert!(
        why.contains(&format!(
            "`cargo xtask phpt --base {} --subset ext/fixture`",
            base.commit
        )),
        "{why}"
    );

    let run = fixture("base.json");
    let file = path(&linked, &base.tree, "ext/fixture");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, serde_json::to_string_pretty(&run).unwrap()).unwrap();
    assert_eq!(base.run(&roots(&main), "ext/fixture").unwrap(), Ok(run));

    // Another spelling of the checkout is the same root, and a worktree
    // git still lists whose directory is gone is none.
    assert_eq!(roots(&main.join(".")), looked);
    fs::remove_dir_all(&linked).unwrap();
    assert_eq!(roots(&main), [main.path()]);
}
