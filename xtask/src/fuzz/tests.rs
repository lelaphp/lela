//! The lane's controls each way, over `xtask/fixtures/fuzz` (its README
//! says what it is) and the corpus the lane runs by default.

use super::{Ask, Corpus};

/// What a run asks for, `drawn` programs within `minutes`.
fn ask(drawn: usize, minutes: u64) -> Ask {
    Ask {
        corpus: None,
        seed: None,
        programs: drawn,
        minutes,
    }
}

/// The fixture's corpus.
fn fixture() -> Corpus {
    Corpus::load(&crate::package_dir().join("fixtures/fuzz/corpus")).unwrap()
}

/// One seed draws the same programs twice, another seed others; half
/// and half the grammar's and mutations of the corpus, each after the
/// corpus's own program as it stands, and each opening with php's tag.
#[test]
fn the_same_seed_draws_the_same_programs() {
    let corpus = fixture();
    let drawn = corpus.drawn(42, 24);
    assert_eq!(drawn, corpus.drawn(42, 24));
    assert_ne!(drawn, corpus.drawn(43, 24));
    assert_eq!(drawn.len(), 25);
    assert_eq!(drawn[0].label, "disagreed");
    assert_eq!(drawn[0].code, corpus.programs[0].1);
    let labels = |prefix: char| {
        drawn[1..]
            .iter()
            .filter(|d| d.label.starts_with(prefix))
            .count()
    };
    assert!(labels('g') > 0 && labels('m') > 0, "{drawn:#?}");
    assert_eq!(labels('g') + labels('m'), 24);
    assert!(
        drawn.iter().all(|d| d.code.starts_with("<?php\n")),
        "{drawn:#?}"
    );
    let generated: Vec<_> = drawn.iter().filter(|d| d.label.starts_with('g')).collect();
    assert!(
        generated
            .iter()
            .all(|d| d.code.ends_with("echo \"|end\";\n"))
    );
}

/// A directory with no `.php` program is no corpus, and the lane's own
/// corpus is one.
#[test]
fn a_corpus_holds_a_program() {
    let empty = crate::scratch::Tree::new("fuzz-empty").unwrap();
    let refused = format!("{:#}", Corpus::load(&empty).unwrap_err());
    assert!(refused.contains("holds no .php program"), "{refused}");
    let root = crate::package_dir().parent().unwrap();
    assert!(
        !Corpus::load(&root.join(super::CORPUS))
            .unwrap()
            .programs
            .is_empty()
    );
}

/// A run that held no program to php fails, saying it compared nothing,
/// so an oracle that completes nothing is never a pass; a run that held
/// one and found nothing passes.
#[test]
fn a_run_that_held_no_program_fails() {
    let unheld = super::Hunt {
        unheld: 3,
        ..super::Hunt::default()
    };
    assert_eq!(unheld.status(), crate::reading::Status::Fail);
    let message = unheld.message(std::path::Path::new("/"), 1, &fixture(), &ask(2, 30));
    assert!(
        message.starts_with("no program was held to php"),
        "{message}"
    );
    let met = super::Hunt {
        met: 1,
        ..super::Hunt::default()
    };
    assert_eq!(met.status(), crate::reading::Status::Pass);
}

/// A run that cut programs or kept findings whole says so first after its
/// findings, with the budget it was held to.
#[test]
fn a_run_cut_short_says_what_it_cut() {
    let hunt = super::Hunt {
        met: 2,
        cut: 5,
        whole: 1,
        ..super::Hunt::default()
    };
    let message = hunt.message(std::path::Path::new("/"), 1, &fixture(), &ask(6, 3));
    assert!(
        message.starts_with("cut short: 5 not run and 1 finding kept whole, a time-out's or past the run's 3 minutes"),
        "{message}"
    );
}

/// The command `done` names for a stale `fuzz` reading of each crate the
/// lane owes, so the `[[owed]]` row a person lands needs nothing beside
/// it: `cargo xtask fuzz`, which the command line parses.
#[test]
fn done_names_the_command_that_takes_the_lane_s_reading() {
    use clap::Parser;
    for krate in super::OWING {
        let command = crate::done::command_taking(super::INSTRUMENT, krate);
        assert_eq!(command.as_deref(), Some("cargo xtask fuzz"), "{krate}");
    }
    let parsed = crate::cli::Cli::try_parse_from(["xtask", "fuzz"]);
    assert!(parsed.is_ok(), "{}", parsed.unwrap_err());
}

/// The row the lane is to be owed by decodes now, so the parser stands
/// before a person lands the row, and covers each crate the run's verdict
/// is written over and no other.
#[test]
fn the_owed_row_the_lane_waits_on_decodes_and_covers_the_crates_that_owe_it() {
    let crates: Vec<String> = super::OWING.iter().map(|c| format!("{c:?}")).collect();
    let row = format!(
        "instrument = \"fuzz\"\nheld_at = \"merge\"\ncrates = [{}]\n",
        crates.join(", ")
    );
    let owed: crate::manifest::Owed = toml::from_str(&row).unwrap();
    assert_eq!(owed.instrument.name(), super::INSTRUMENT);
    for name in super::OWING {
        assert!(owed.covers(name), "{name}");
    }
    assert!(!owed.covers("ext/standard"));
}

#[cfg(tracked)]
mod stood_in {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::super::{Hunt, Pass, Sides};
    use super::{ask, fixture};
    use crate::divergence::Registry;
    use crate::hostile::judge::{Fault, Judged};
    use crate::oracle::Pin;
    use crate::scratch::Tree;

    /// The tracked pin, whose recipe both stand-ins run under.
    fn pin() -> Pin {
        let root = crate::package_dir().join("..");
        Pin::parse(&fs::read_to_string(root.join("oracle.toml")).unwrap()).unwrap()
    }

    /// A stand-in binary: `body` as a shell program, which finds the
    /// program it runs as its last word.
    fn binary(tree: &Tree, name: &str, body: &str) -> PathBuf {
        let text = format!("#!/bin/sh\nfor word; do program=$word; done\n{body}\n");
        let path = tree.write(name, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Both stand-ins in a tree, the engine's `engine`, and the sides
    /// they make with a budget ending at `deadline`, handed to `with`.
    fn stood_in<T>(engine: &str, deadline: Instant, with: impl FnOnce(&Sides, &Tree) -> T) -> T {
        let tree = Tree::new("fuzz-stood-in").unwrap();
        let php = binary(&tree, "php", "echo same");
        let engine = binary(&tree, "engine", engine);
        let pin = pin();
        let oracle = pin.oracle();
        let registry = Registry::default();
        let sides = Sides {
            oracle: &oracle,
            php: &php,
            engine: &engine,
            registry: &registry,
            deadline,
        };
        with(&sides, &tree)
    }

    /// The fixture's corpus with `drawn` programs beside it, hunted with
    /// the engine stood in by `engine` within `budget`, and the tree its
    /// findings are in.
    fn hunted(engine: &str, drawn: usize, budget: Duration) -> (Hunt, Tree) {
        let tree = Tree::new("fuzz-found").unwrap();
        let hunt = stood_in(engine, Instant::now() + budget, |sides, _| {
            sides
                .hunt(&fixture().drawn(7, drawn), &tree.join("found"))
                .unwrap()
        });
        (hunt, tree)
    }

    /// The engine that differs on the line the fixture's program carries.
    const DIFFERS: &str = "if grep -q 'the engine is made to differ here' \"$program\"; then echo differs; else echo same; fi";

    /// A run whose budget is spent starts no program, so it held none and
    /// fails, saying how many it did not run.
    #[test]
    fn a_run_past_its_budget_starts_no_program() {
        let (hunt, tree) = hunted(DIFFERS, 4, Duration::ZERO);
        assert_eq!(
            (hunt.cut, hunt.met, hunt.findings.len()),
            (5, 0, 0),
            "{hunt:#?}"
        );
        assert_eq!(hunt.status(), crate::reading::Status::Fail);
        let message = hunt.message(Path::new("/"), 7, &fixture(), &ask(4, 0));
        assert!(message.contains("cut short: 5 not run"), "{message}");
        assert!(!tree.join("found").exists());
    }

    /// A time-out is kept whole: minimising it would run each trial to the
    /// recipe's limit. So is a finding met with the budget spent; within
    /// it, a difference is minimised.
    #[test]
    fn a_time_out_or_a_finding_past_the_budget_is_kept_whole() {
        let code = "<?php\n$a = 1;\n// the engine is made to differ here\n";
        let timed_out = Judged::Fault(Fault::PastLimit(60));
        let differs = Judged::Differs("at line 1".to_owned());
        let later = Instant::now() + Duration::from_mins(10);
        stood_in(DIFFERS, later, |sides, _| {
            assert_eq!(
                sides
                    .minimised(code, Pass::Interpreter, &timed_out)
                    .unwrap(),
                None
            );
            let kept = sides.minimised(code, Pass::Interpreter, &differs).unwrap();
            assert_eq!(
                kept.as_deref(),
                Some("<?php\n// the engine is made to differ here\n")
            );
        });
        stood_in(DIFFERS, Instant::now(), |sides, _| {
            assert_eq!(
                sides.minimised(code, Pass::Interpreter, &differs).unwrap(),
                None
            );
        });
    }

    /// The control that must fail: the engine made to differ on the
    /// corpus's program is a finding, kept minimised to the line it
    /// differs on, under the interpreter; the drawn programs, which do not
    /// carry the line, meet php.
    #[test]
    fn the_lane_fails_on_the_program_the_engine_is_made_to_differ_on() {
        let (hunt, tree) = hunted(DIFFERS, 0, Duration::from_mins(10));
        let [finding] = &hunt.findings[..] else {
            panic!("{hunt:#?}")
        };
        assert_eq!(finding.pass, Pass::Interpreter);
        assert_eq!(finding.path, tree.join("found/disagreed.php"));
        assert!(
            matches!(&finding.judged, Judged::Differs(first) if first.contains("line 1")),
            "{finding:#?}"
        );
        let kept = fs::read_to_string(&finding.path).unwrap();
        assert_eq!(kept, "<?php\n// the engine is made to differ here\n");
        let message = hunt.message(Path::new("/"), 7, &fixture(), &ask(0, 10));
        assert!(message.starts_with("1 finding: "), "{message}");
    }

    /// With an engine that agrees, the same corpus and its drawn programs
    /// all meet php and nothing is kept.
    #[test]
    fn the_lane_passes_where_the_engine_agrees() {
        let (hunt, tree) = hunted("echo same", 6, Duration::from_mins(10));
        assert!(hunt.findings.is_empty(), "{hunt:#?}");
        assert_eq!((hunt.met, hunt.unheld), (7, 0));
        assert!(!tree.join("found").exists());
    }
}

/// `permit` allows the command `done` names for a stale `fuzz` reading,
/// in main's checkout and in a ticketed one, as `done`'s own test asks of
/// every landed `[[owed]]` row; a rule that keeps the lane off a local
/// machine would deny it, and the row and the rule are one person's
/// choice (#4487).
#[cfg(tracked)]
#[test]
fn permit_allows_the_command_done_names_for_the_lane() {
    let root = crate::package_dir().parent().unwrap();
    let manifest = crate::manifest::Manifest::load(root).unwrap();
    let command = crate::done::command_taking(super::INSTRUMENT, "php_engine").unwrap();
    for ticket in [None, Some("4358")] {
        let tree = crate::scratch::Tree::new("fuzz-permit").unwrap();
        crate::git::git(tree.path(), &["init", "-q", "-b", "main"]).unwrap();
        if let Some(ticket) = ticket {
            crate::git::git(tree.path(), &["config", "branch.main.ticket", ticket]).unwrap();
        }
        let call = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": command, "description": "take the reading" }
        });
        let call = crate::permit::Call::parse(&call.to_string()).unwrap();
        let judgement = crate::permit::judge(&manifest, tree.path(), &call).unwrap();
        assert_eq!(
            judgement.denial().map(|f| f.rule.clone()),
            None,
            "{ticket:?}"
        );
    }
}
