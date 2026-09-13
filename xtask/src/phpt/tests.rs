//! The judge of one test and the resolution of a subset, held from inside
//! the instrument: what a `.phpt` file comes to under the process adapter,
//! and which directories of the pinned tree a name resolves to. The
//! reading a subset ends as is held from outside, in `tests/phpt.rs`,
//! through `run`.
//!
//! The tests that spawn php run the pinned oracle as the binary under
//! test, which is the runner's own control: it measures the judge, since
//! the engine that will be measured does not exist yet. They say so on
//! stderr and return when the pinned binary is not on this machine, as
//! the ones that resolve a subset of the real corpus do when the pinned
//! tree is on neither place `Source::tree` looks, and fail there instead
//! where this machine promised the fact (#38); the rest run anywhere,
//! the subsets they resolve being a fixture tree's.

use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::divergence::Registry;
use crate::manifest::{Crate, Manifest};
use crate::oracle::{Promise, Runner};
use crate::scratch::Tree;

fn workspace() -> PathBuf {
    crate::built_root()
}

fn manifest() -> Manifest {
    Manifest::load(&workspace()).unwrap()
}

/// A manifest of these `[[crate]]` rows under the tracked pin, its tree
/// the one given, which the caller holds open for as long as it reads the
/// manifest.
fn manifest_of(rows: impl IntoIterator<Item = Crate>, tree: &Path) -> Manifest {
    let mut manifest = Manifest::of(rows);
    manifest.pin = self::manifest()
        .pin
        .on_this_machine(Some(tree.to_path_buf()));
    manifest
}

/// The tracked manifest with its tree the one given.
fn tracked_over_fixture(tree: &Path) -> Manifest {
    let mut manifest = manifest();
    manifest.pin = manifest
        .pin
        .clone()
        .on_this_machine(Some(tree.to_path_buf()));
    manifest
}

/// A tree standing where the pinned php-src would, with the directories
/// the per-commit lane's rows name and no test in them: what a subset
/// resolves to is held here, and what one runs to is held over the real
/// corpus by the tests that skip without it.
fn fixture_tree() -> Tree {
    let tree = Tree::new("phpt-tree").unwrap();
    for under in ["Zend/tests", "tests", "sapi/cli/tests", "ext/pcre/tests"] {
        fs::create_dir_all(tree.join(under)).unwrap();
    }
    tree
}

/// `Subset::resolve` over the tree the manifest's pin answers.
fn resolve(manifest: &Manifest, name: &str) -> Result<Subset> {
    Subset::resolve(manifest, &manifest.pin.source.tree()?, name)
}

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary (#38).
fn oracle() -> Option<PathBuf> {
    let pin = manifest().pin;
    Promise::Oracle
        .or_skip(pin.oracle.file().map(Path::to_path_buf))
        .unwrap()
}

/// The tracked pin's php-src tree, when it is on this machine; a failure
/// where this machine promised the tree (#38).
fn source() -> Option<PathBuf> {
    Promise::Tree.or_skip(manifest().pin.source.tree()).unwrap()
}

/// A subset of one directory of this test's own, standing where
/// `php-src/ext/fixture/tests` would.
fn subset(name: &str, tests: &[(&str, &str)]) -> (Tree, Subset) {
    let dir = Tree::new(&format!("phpt-judge-{name}")).unwrap();
    for (file, text) in tests {
        fs::write(dir.join(file), text).unwrap();
    }
    let subset = Subset {
        scope: "ext/fixture".to_owned(),
        dirs: vec![Dir {
            under: PathBuf::from("ext/fixture/tests"),
            dir: dir.path().to_path_buf(),
        }],
    };
    (dir, subset)
}

fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}

/// Run one subset with the pinned oracle standing in for the engine.
fn run(subset: &Subset, registry: &Registry) -> Tally {
    let binary = oracle().unwrap();
    let manifest = manifest();
    let oracle = manifest.pin.oracle();
    run_subset(&oracle, registry, &Runner::Engine(binary), None, subset).unwrap()
}

fn only(tally: &Tally) -> (Judgement, String) {
    assert_eq!(tally.judged.len(), 1, "{tally:#?}");
    let judged = &tally.judged[0];
    (judged.judgement, judged.note.clone())
}

#[test]
fn a_lane_names_the_engine_only_directive() {
    assert_eq!(Lane::Force.ini(), "jit.mode=force");
    assert_eq!(Lane::ExitFuzz.ini(), "jit.mode=exit-fuzz");
}

#[test]
fn a_test_that_meets_its_expect_section_passes() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "pass",
        &[(
            "pass.phpt",
            "--TEST--\nEcho\n--FILE--\n<?php echo \"one\\n\", 2 + 2;\n--EXPECT--\none\n4\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
}

#[test]
fn the_digest_is_asserted_once_over_a_run_of_three_tests() {
    if oracle().is_none() {
        return;
    }
    // Three tests, each with a SKIPIF and a CLEAN section, so the oracle
    // runs three times beside the binary under test and the pin is asked
    // for its digest before the first program only (harness §4.3).
    let test = |n: u8| {
        format!(
            "--TEST--\nn{n}\n--SKIPIF--\n<?php echo 'ok';\n--FILE--\n<?php echo {n};\n--CLEAN--\n<?php echo 'clean';\n--EXPECT--\n{n}\n"
        )
    };
    let tests = [test(1), test(2), test(3)];
    let (_tree, subset) = subset(
        "digest",
        &[
            ("one.phpt", &tests[0]),
            ("two.phpt", &tests[1]),
            ("three.phpt", &tests[2]),
        ],
    );
    let binary = oracle().unwrap();
    let manifest = manifest();
    assert_eq!(manifest.pin.digests_asserted(), 0);
    let oracle = manifest.pin.oracle();
    let tally = run_subset(
        &oracle,
        &registry(""),
        &Runner::Engine(binary),
        None,
        &subset,
    )
    .unwrap();
    assert_eq!(tally.judged.len(), 3, "{tally:#?}");
    assert!(
        tally.judged.iter().all(|j| j.judgement == Judgement::Pass),
        "{tally:#?}"
    );
    assert_eq!(manifest.pin.digests_asserted(), 1);
}

#[test]
fn the_wildcards_of_an_expectf_section_are_matched_and_the_streams_are_one() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "expectf",
        &[(
            "warn.phpt",
            "--TEST--\nA warning names the file and the line\n--FILE--\n<?php\nfwrite(STDERR, \"on the second stream\\n\");\necho $nope;\n--EXPECTF--\non the second stream\n\nWarning: Undefined variable $nope in %s on line %d\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
}

#[test]
fn a_missed_expectation_fails_with_the_difference() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "missed",
        &[(
            "missed.phpt",
            "--TEST--\nA wrong expectation\n--FILE--\n<?php echo \"four\";\n--EXPECT--\nfive\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Fail, "{tally:#?}");
    assert!(note.contains("-five") && note.contains("+four"), "{note}");
    assert_eq!(tally.against(), ["missed.phpt"]);
}

#[test]
fn the_same_missed_expectation_is_diverged_under_an_accepted_row() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "accepted",
        &[(
            "missed.phpt",
            "--TEST--\nA wrong expectation\n--FILE--\n<?php echo \"four\";\n--EXPECT--\nfive\n",
        )],
    );
    let accepted = registry(
        "[[divergence]]\nid = \"phpt:php-src/ext/fixture/tests/missed.phpt\"\nkind = \"phpt\"\nreason = \"A person wrote why this one is allowed to differ.\"\nstatus = \"accepted\"\nsince = \"2026-09-08\"\n",
    );
    let tally = run(&subset, &accepted);
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Diverged, "{tally:#?}");
    assert!(note.contains("A person wrote why"), "{note}");
    assert!(tally.against().is_empty());
}

#[test]
fn a_skipif_that_says_skip_skips_with_its_reason() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "skipif",
        &[(
            "skipped.phpt",
            "--TEST--\nSkipped\n--SKIPIF--\n<?php echo \"skip nothing to see here\\n\";\n--FILE--\n<?php echo \"never\";\n--EXPECT--\nnever\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(
        only(&tally),
        (Judgement::Skip, "nothing to see here".to_owned()),
        "{tally:#?}"
    );
}

#[test]
fn an_extension_the_binary_does_not_load_skips_in_upstreams_words() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "extensions",
        &[(
            "needs.phpt",
            "--TEST--\nNeeds an extension that is not there\n--EXTENSIONS--\nnot_an_extension\n--FILE--\n<?php echo \"never\";\n--EXPECT--\nnever\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(
        only(&tally),
        (
            Judgement::Skip,
            "Required extension missing: not_an_extension".to_owned()
        ),
        "{tally:#?}"
    );
}

#[test]
fn a_file_that_is_not_a_test_borks_and_the_subset_fails_on_it() {
    let (_tree, subset) = subset("borked", &[("borked.phpt", "--NOTATEST--\nnothing\n")]);
    if oracle().is_none() {
        return;
    }
    let tally = run(&subset, &registry(""));
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Bork);
    assert!(note.contains("must start with --TEST--"), "{note}");
    assert_eq!(tally.against(), ["borked.phpt"]);
}

#[test]
fn a_cgi_test_is_skipped_because_this_runner_hosts_the_command_line() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "cgi",
        &[(
            "cgi.phpt",
            "--TEST--\nA CGI test\n--CGI--\n--FILE--\n<?php echo \"never\";\n--EXPECT--\nnever\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Skip);
    assert!(note.contains("CGI"), "{note}");
}

#[test]
fn the_test_s_own_ini_reaches_the_run() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "ini",
        &[(
            "ini.phpt",
            "--TEST--\nThe INI section is passed after the recipe's\n--INI--\nprecision=3\n--FILE--\n<?php echo 1/3;\n--EXPECT--\n0.333\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
}

#[test]
fn the_subset_of_an_extension_is_its_tests_at_the_pinned_tag_and_the_scope_is_the_crate() {
    let tree = fixture_tree();
    let manifest = tracked_over_fixture(&tree);
    for name in ["pcre", "ext/pcre"] {
        let subset = resolve(&manifest, name).unwrap();
        assert_eq!(subset.scope, "ext/pcre");
        assert_eq!(subset.dirs.len(), 1, "{subset:#?}");
        assert_eq!(subset.dirs[0].under, Path::new("ext/pcre/tests"));
        assert_eq!(
            subset.dirs[0].dir,
            tree.join("ext/pcre/tests"),
            "the subset is the tree's the variable names"
        );
        assert_eq!(
            subset.id(&subset.dirs[0].dir.join("preg_match_basic.phpt")),
            "phpt:php-src/ext/pcre/tests/preg_match_basic.phpt",
            "the divergence key names the corpus, not this machine"
        );
    }
}

#[test]
fn a_corpus_directory_is_a_subset_and_an_unknown_name_names_both_forms_tried() {
    if source().is_none() {
        return;
    }
    let manifest = manifest();
    let subset = resolve(&manifest, "Zend/tests").unwrap();
    assert_eq!(subset.scope, "Zend/tests");
    assert_eq!(subset.dirs[0].under, Path::new("Zend/tests"));

    let error = resolve(&manifest, "nope").unwrap_err().to_string();
    assert!(
        error.contains("ext/nope") && error.contains("nope"),
        "{error}"
    );
}

#[test]
fn a_subset_resolves_under_the_tree_the_pin_was_handed_ahead_of_the_pins_path() {
    // The fixture is what the variable names; the pin's path, the
    // maintainer's tree or nothing, is not consulted while it is there.
    let tree = fixture_tree();
    let manifest = manifest_of([Crate::named("ext/pcre")], &tree);
    assert_eq!(manifest.pin.source.tree().unwrap(), *tree);
    let corpus = resolve(&manifest, "Zend/tests").unwrap();
    assert_eq!(corpus.scope, "Zend/tests");
    assert_eq!(corpus.dirs[0].dir, tree.join("Zend/tests"));
    let extension = resolve(&manifest, "pcre").unwrap();
    assert_eq!(extension.dirs[0].dir, tree.join("ext/pcre/tests"));
    assert_eq!(extension.absent(), Vec::<&Path>::new(), "{extension:#?}");
}

#[test]
fn a_tier_lane_and_a_control_are_readings_of_their_own() {
    let tree = fixture_tree();
    let subset = resolve(&tracked_over_fixture(&tree), "pcre").unwrap();
    let engine = Runner::Engine(PathBuf::from("target/debug/php"));
    assert_eq!(subset.scope(&engine, None), "ext/pcre");
    assert_eq!(subset.scope(&engine, Some(Lane::Force)), "ext/pcre@force");
    assert_eq!(
        subset.scope(&engine, Some(Lane::ExitFuzz)),
        "ext/pcre@exit-fuzz"
    );
    // A control of the runner keeps a reading of its own, so a green one
    // can never sit where the engine's reading belongs.
    let control = Runner::Control(PathBuf::from("php"));
    assert_eq!(subset.scope(&control, None), "ext/pcre@control");
}

#[test]
fn a_crate_resolves_to_every_subset_its_row_names() {
    let tree = fixture_tree();
    let manifest = manifest_of(
        [Crate::named("php_engine").with_phpt(&["Zend/tests", "tests"])],
        &tree,
    );
    let subset = resolve(&manifest, "php_engine").unwrap();
    assert_eq!(subset.scope, "php_engine");
    let under: Vec<&Path> = subset.dirs.iter().map(|d| d.under.as_path()).collect();
    assert_eq!(under, [Path::new("Zend/tests"), Path::new("tests")]);
    assert_eq!(
        subset.id(&subset.dirs[1].dir.join("basic/lambda.phpt")),
        "phpt:php-src/tests/basic/lambda.phpt",
        "the key names where the test stands in the corpus, whichever directory it came from"
    );
}

#[test]
fn a_crate_whose_row_names_no_subset_says_so_rather_than_looking_for_a_directory() {
    let tree = fixture_tree();
    let manifest = manifest_of([Crate::named("php_heap")], &tree);
    let error = resolve(&manifest, "php_heap").unwrap_err().to_string();
    assert!(
        error.contains("php_heap") && error.contains("no phpt subset"),
        "{error}"
    );
    assert!(
        !error.contains("php-src"),
        "the row is the answer, not the pinned tree: {error}"
    );
}

#[test]
fn a_directory_the_pinned_tree_has_not_is_kept_and_named_absent() {
    let tree = fixture_tree();
    let manifest = manifest_of(
        [Crate::named("php_engine").with_phpt(&["Zend/tests", "Zend/nope"])],
        &tree,
    );
    let subset = resolve(&manifest, "php_engine").unwrap();
    assert_eq!(subset.dirs.len(), 2, "{subset:#?}");
    assert_eq!(subset.absent(), [Path::new("Zend/nope")]);
}

#[test]
fn the_per_commit_lane_is_composed_from_the_crate_rows() {
    let tree = fixture_tree();
    let manifest = manifest_of(
        [
            Crate::named("php_engine").with_phpt(&["Zend/tests", "tests"]),
            Crate::named("php_heap"),
            Crate::named("php_sapi").with_phpt(&["sapi/cli/tests"]),
            Crate::named("ext/pcre"),
            Crate::named("ext/curl").with_feature("curl"),
        ],
        &tree,
    );
    let lane: Vec<String> = Subset::lane(&manifest, &tree)
        .into_iter()
        .map(|subset| subset.scope)
        .collect();
    // §7's per-commit row: Zend/tests and tests/ with php_engine, sapi/cli
    // with php_sapi, and the default extensions' own. A crate behind a
    // feature is not in the default build, and one with no subset owns none.
    assert_eq!(lane, ["php_engine", "php_sapi", "ext/pcre"]);
}

#[test]
fn every_expectf_section_of_the_pinned_pcre_subset_compiles() {
    use crate::oracle::expect::Expectation;
    let Some(tree) = source() else { return };
    let subset = Subset::resolve(&manifest(), &tree, "pcre").unwrap();
    let tests = subset.tests().unwrap();
    assert!(!tests.is_empty(), "the pinned tree has ext/pcre/tests");
    let mut seen = 0;
    for test in tests {
        let Ok(phpt) = file::Phpt::load(&test) else {
            continue;
        };
        for (section, expectation) in [
            ("EXPECTF", Expectation::Format as fn(String) -> _),
            ("EXPECTREGEX", Expectation::Pattern),
        ] {
            let Some(body) = phpt.section(section) else {
                continue;
            };
            seen += 1;
            let expectation = expectation(String::from_utf8_lossy(body).trim().to_owned());
            // The one corpus body that needs PCRE-only syntax is a
            // divergence row, not a reason to carry a backtracker (§8);
            // every other one must compile here.
            if let Err(e) = expectation.met(b"") {
                assert!(section == "EXPECTREGEX", "{}: {e:#}", test.display());
            }
        }
    }
    assert!(seen > 30, "the subset carried {seen} patterns");
}
