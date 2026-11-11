//! The lane's controls each way, over `xtask/fixtures/fuzz` (its README
//! says what it is) and the corpus the lane runs by default.

use super::Corpus;

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
    let message = unheld.message(std::path::Path::new("/"), 1, &fixture(), 2);
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

    use super::super::{Hunt, Pass, Sides};
    use super::fixture;
    use crate::divergence::Registry;
    use crate::hostile::judge::Judged;
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

    /// The fixture's corpus with `drawn` programs beside it, hunted with
    /// the engine stood in by `engine`, and the tree its findings are in.
    fn hunted(engine: &str, drawn: usize) -> (Hunt, Tree) {
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
        };
        let programs = fixture().drawn(7, drawn);
        let hunt = sides.hunt(&programs, &tree.join("found")).unwrap();
        (hunt, tree)
    }

    /// The control that must fail: the engine made to differ on the
    /// corpus's program is a finding, kept minimised to the line it
    /// differs on, under the interpreter; the drawn programs, which do not
    /// carry the line, meet php.
    #[test]
    fn the_lane_fails_on_the_program_the_engine_is_made_to_differ_on() {
        let differs = "if grep -q 'the engine is made to differ here' \"$program\"; then echo differs; else echo same; fi";
        let (hunt, tree) = hunted(differs, 0);
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
        let message = hunt.message(Path::new("/"), 7, &fixture(), 0);
        assert!(message.starts_with("1 finding: "), "{message}");
    }

    /// With an engine that agrees, the same corpus and its drawn programs
    /// all meet php and nothing is kept.
    #[test]
    fn the_lane_passes_where_the_engine_agrees() {
        let (hunt, tree) = hunted("echo same", 6);
        assert!(hunt.findings.is_empty(), "{hunt:#?}");
        assert_eq!((hunt.met, hunt.unheld), (7, 0));
        assert!(!tree.join("found").exists());
    }
}
