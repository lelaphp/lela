//! The lane's controls each way, over `xtask/fixtures/hostile` (its
//! README says what each file is) and the corpus itself.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::corpus::{Corpus, Form};
use super::judge::{Came, Fault, Found, Judged, Said, seen};
use super::kept::{self, Base, Run, Verdict};
use crate::divergence::Registry;
use crate::oracle::{Ended, Observation};

fn fixture(name: &str) -> String {
    let path = crate::package_dir().join("fixtures/hostile").join(name);
    fs::read_to_string(&path).unwrap()
}

fn observed(stdout: &str, exit: Ended) -> Observation {
    Observation {
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
        exit,
        wall: Duration::ZERO,
    }
}

/// What the oracle's side of a program that printed `stdout` and exited 0
/// is held by.
fn theirs(stdout: &str) -> Vec<u8> {
    seen(&observed(stdout, Ended::Status(0)))
}

fn came(stdout: &str) -> Came {
    Came::Observed(observed(stdout, Ended::Status(0)))
}

/// The corpus the lane runs decodes, every program in one of the four
/// forms, more than a hundred of them and few enough for the lane's
/// seconds (Round 2, Q31 (a)).
#[test]
fn the_corpus_decodes_with_an_id_each() {
    let root = crate::package_dir().parent().unwrap();
    let corpus = Corpus::load(root).unwrap();
    assert!(
        (100..=200).contains(&corpus.programs.len()),
        "{}",
        corpus.programs.len()
    );
    for form in [Form::Grows, Form::Unsets, Form::Replaces, Form::Throws] {
        assert!(corpus.programs.iter().any(|p| p.form == form), "{form:?}");
    }
}

/// The fixture corpus decodes; one naming an id twice, one with a form
/// the corpus does not list and one with a misspelt id are each refused,
/// naming what is wrong.
#[test]
fn a_corpus_is_refused_for_a_twice_named_id_an_unknown_form_or_a_misspelt_id() {
    let corpus = Corpus::parse(&fixture("corpus.toml")).unwrap();
    let ids: Vec<&str> = corpus.programs.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["grows-one", "throws-two"]);
    assert_eq!(corpus.programs[1].form, Form::Throws);
    let refused = |name: &str| format!("{:#}", Corpus::parse(&fixture(name)).unwrap_err());
    assert!(refused("corpus-id-twice.toml").contains("\"same\" names two programs"));
    assert!(refused("corpus-unknown-form.toml").contains("unknown variant `frees`"));
    assert!(refused("corpus-misspelt-id.toml").contains("\"Grows_One\" is spelt"));
    assert!(Corpus::parse("").is_err());
}

/// A fault is a signal, the recipe's limit, or a line saying an
/// invariant failed that the oracle's side does not carry; an exit of
/// 255 with php's bytes is no fault, and neither is a program that prints
/// such a line where php prints it too.
#[test]
fn a_fault_is_a_signal_the_limit_or_a_failed_invariant_php_does_not_print() {
    let leak =
        "[x]\n\nFatal error: InternalError: the request ended with 1 block of its heap never freed";
    assert_eq!(
        Fault::of(&came(leak), &theirs("[x]")),
        Some(Fault::Internal(
            "Fatal error: InternalError: the request ended with 1 block of its heap never freed"
                .to_owned()
        ))
    );
    let segv = Came::Observed(observed("", Ended::Signal(11)));
    assert_eq!(Fault::of(&segv, &theirs("")), Some(Fault::Signal(11)));
    assert_eq!(
        Fault::of(&Came::PastLimit(60), &theirs("")),
        Some(Fault::PastLimit(60))
    );
    let fatal = Came::Observed(observed("Fatal error: Uncaught Error", Ended::Status(255)));
    assert_eq!(
        Fault::of(&fatal, &theirs("Fatal error: Uncaught Error")),
        None
    );
    let echoed = "InternalError: as the program wrote it";
    assert_eq!(Fault::of(&came(echoed), &theirs(echoed)), None);
    let mut panicked = observed("", Ended::Status(101));
    panicked.stderr = b"thread 'main' panicked at src/x.rs:1:1".to_vec();
    let fault = Fault::of(&Came::Observed(panicked), &theirs(""));
    assert!(matches!(fault, Some(Fault::Internal(line)) if line.contains("panicked at")));
}

/// Under the interpreter a program is met, differs, or is a fault; a
/// difference a `trace:` row covering its `hostile:` id accepts is
/// accepted, and a `witness:` row may not cover one.
#[test]
fn a_difference_is_accepted_by_a_trace_row_covering_its_hostile_id_alone() {
    let none = Registry::parse("").unwrap();
    assert_eq!(
        Judged::of(&none, "kept-met", &theirs("a"), &came("a")),
        Judged::Met
    );
    let differs = Judged::of(&none, "kept-differs", &theirs("a"), &came("b"));
    assert!(
        matches!(&differs, Judged::Differs(first) if first.contains("line 1")),
        "{differs:?}"
    );
    let leak = came("InternalError: a block never freed");
    assert!(matches!(
        Judged::of(&none, "x", &theirs(""), &leak),
        Judged::Fault(_)
    ));

    let rows = Registry::parse(&fixture("accepts.toml")).unwrap();
    let accepted = Judged::of(&rows, "kept-accepted", &theirs("a"), &came("b"));
    assert_eq!(
        accepted,
        Judged::Accepted("trace:fixture-hostile".to_owned())
    );
    assert_eq!(accepted.verdict(), Verdict::Accepted);
    let other = Judged::of(&rows, "kept-differs", &theirs("a"), &came("b"));
    assert_eq!(other.verdict(), Verdict::Differs);

    let refused = format!(
        "{:#}",
        Registry::parse(&fixture("witness-covers.toml")).unwrap_err()
    );
    assert!(refused.contains("hostile:kept-met"), "{refused}");
}

/// The base fixture's verdicts.
fn base() -> BTreeMap<String, Verdict> {
    let run: Run = serde_json::from_str(&fixture("base.json")).unwrap();
    run.verdicts
}

fn held(interpreter: Judged) -> Found {
    Found::Held {
        interpreter,
        tier: None,
    }
}

/// Round 1's Q5 (b), the control that must fail: a program that met php
/// at the base and differs now refuses the run, naming the base; one that
/// differed there, one accepted there, and one the base's run did not
/// hold, each differing now, are counted and refuse nothing.
#[test]
fn a_program_that_met_php_at_the_base_and_differs_now_refuses_the_run() {
    let verdicts = base();
    let differs = || held(Judged::Differs("at line 1".to_owned()));
    let found = [
        ("kept-met", differs()),
        ("kept-differs", differs()),
        ("kept-accepted", differs()),
        ("new-program", differs()),
    ];
    let said = Said::of(
        found.iter().map(|(id, f)| (*id, f)),
        Some(("0000000", &verdicts)),
    );
    assert_eq!(
        said.refusals,
        ["kept-met met php at 0000000 and differs now: at line 1"]
    );
    assert_eq!((said.differs, said.regressed, said.held_at_base), (4, 1, 3));

    let counted = Said::of(found.iter().map(|(id, f)| (*id, f)), None);
    assert!(counted.refusals.is_empty(), "{:?}", counted.refusals);
    assert_eq!(
        (counted.differs, counted.regressed, counted.held_at_base),
        (4, 0, 0)
    );
}

/// A fault refuses the run whatever the base held, under the interpreter
/// or under the compiled tier, as does a program the oracle did not
/// complete; a program met at the base and met now is a pass.
#[test]
fn a_fault_under_either_pass_refuses_the_run_whatever_the_base_held() {
    let verdicts = base();
    let found = [
        ("kept-met", held(Judged::Met)),
        ("kept-differs", held(Judged::Fault(Fault::Signal(11)))),
        (
            "kept-accepted",
            Found::Held {
                interpreter: Judged::Accepted("trace:fixture-hostile".to_owned()),
                tier: Some(Fault::Internal("InternalError: x".to_owned())),
            },
        ),
        ("unheld", Found::Unheld("on signal 6".to_owned())),
    ];
    let said = Said::of(
        found.iter().map(|(id, f)| (*id, f)),
        Some(("0000000", &verdicts)),
    );
    assert_eq!(
        said.refusals,
        [
            "kept-differs faults: signal 11 ended it",
            "kept-accepted faults under the compiled tier: InternalError: x",
            "unheld: the oracle did not complete it, on signal 6, and the corpus holds only programs it completes",
        ]
    );
    assert_eq!(
        (
            said.met,
            said.accepted,
            said.faults,
            said.tier_faults,
            said.unheld
        ),
        (1, 1, 1, 1, 1)
    );
    let met = [("kept-met", held(Judged::Met))];
    let said = Said::of(
        met.iter().map(|(id, f)| (*id, f)),
        Some(("0000000", &verdicts)),
    );
    assert!(said.refusals.is_empty());
}

/// A repository with `crates/php_engine/a.rs` committed on `main`.
fn repository(name: &str) -> crate::scratch::Tree {
    let tree = crate::scratch::Tree::new(&format!("hostile-{name}")).unwrap();
    crate::git::git(&tree, &["init", "-q", "-b", "main"]).unwrap();
    fs::create_dir_all(tree.join("crates/php_engine")).unwrap();
    fs::write(tree.join("crates/php_engine/a.rs"), "a\n").unwrap();
    crate::git::git(&tree, &["add", "--", "crates"]).unwrap();
    crate::scratch::committing(&tree, &["commit", "-q", "-m", "scratch"]);
    tree
}

/// A run kept under the engine tree a commit hashes its paths to is the
/// one found at that commit, under its pin, from the workspace that kept
/// it; under another pin it is stale; at a commit whose engine moved
/// there is none, and a commit that leaves the engine where it was finds
/// it still.
#[test]
fn the_run_kept_at_the_bases_engine_tree_is_the_one_a_run_is_held_to() {
    let tree = repository("kept");
    let root: &Path = &tree;
    let built_from = vec![PathBuf::from("crates/php_engine")];
    let snapshot =
        crate::key::Snapshot::commit(root, &crate::track::rev(root, "main").unwrap()).unwrap();
    let key = snapshot.tracked_over(&built_from).unwrap();
    let run = Run {
        built_from,
        ..serde_json::from_str(&fixture("base.json")).unwrap()
    };
    let file = run.keep(root, &key).unwrap();
    assert!(
        file.starts_with(root.join(kept::RUNS)),
        "{}",
        file.display()
    );
    assert_eq!(
        kept::at(root, "main", "fixture").unwrap(),
        Base::Kept(run.clone())
    );
    assert_eq!(kept::at(root, "main", "another").unwrap(), Base::Stale);

    fs::write(tree.join("b.txt"), "b\n").unwrap();
    crate::git::git(&tree, &["add", "--", "b.txt"]).unwrap();
    crate::scratch::committing(&tree, &["commit", "-q", "-m", "beside"]);
    assert_eq!(kept::at(root, "main", "fixture").unwrap(), Base::Kept(run));

    fs::write(tree.join("crates/php_engine/a.rs"), "b\n").unwrap();
    crate::git::git(&tree, &["add", "--", "crates"]).unwrap();
    crate::scratch::committing(&tree, &["commit", "-q", "-m", "moved"]);
    assert_eq!(kept::at(root, "main", "fixture").unwrap(), Base::None);
    assert!(matches!(
        kept::at(root, "main~1", "fixture").unwrap(),
        Base::Kept(_)
    ));
}

/// The row box 4 of #4265 lands by `steer` decodes now, so the parser
/// stands before the row (`deny_unknown_fields`): it names the lane, held
/// at the merge as Round 2's Q31 (a) ruled, and covers each crate that
/// owes it and no other.
#[test]
fn the_owed_row_the_lane_waits_on_decodes_and_covers_the_crates_that_owe_it() {
    let crates: Vec<String> = super::OWING.iter().map(|c| format!("{c:?}")).collect();
    let row = format!(
        "instrument = \"hostile\"\nheld_at = \"merge\"\ncrates = [{}]\n",
        crates.join(", ")
    );
    let owed: crate::manifest::Owed = toml::from_str(&row).unwrap();
    assert_eq!(owed.instrument.name(), super::INSTRUMENT);
    assert_eq!(owed.held_at, Some(crate::manifest::HeldAt::Merge));
    for name in super::OWING {
        assert!(owed.covers(name), "{name}");
    }
    assert!(!owed.covers("ext/standard"));
}
