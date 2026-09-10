//! `xtask phpt`, the subset runner (harness-design.md §3, §7, §8): a subset
//! of the pinned corpus run through a binary under the oracle's recipe,
//! each test held to its `--EXPECT*--` section, and one reading per subset.
//!
//! The tests that spawn php run the pinned oracle as the binary under test,
//! which is the runner's own control: it measures the runner, since the
//! engine that will be measured does not exist yet. They say so on stderr
//! and return when the pinned binary is not on this machine; the rest run
//! anywhere.

use std::fs;
use std::path::{Path, PathBuf};

use xtask::divergence::Registry;
use xtask::manifest::{Crate, Manifest};
use xtask::oracle::Runner;
use xtask::phpt::{self, Ask, Dir, Judgement, Subset, Tally};
use xtask::reading::{Number, Status};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn manifest() -> Manifest {
    Manifest::load(workspace()).unwrap()
}

/// A manifest of these `[[crate]]` rows under the tracked pin: the rows a
/// person writes in `seams.toml`, resolved against the pinned tree without
/// the tracked manifest having to carry the crate they name.
fn manifest_of(rows: impl IntoIterator<Item = Crate>) -> Manifest {
    let mut manifest = Manifest::of(rows);
    manifest.pin = self::manifest().pin;
    manifest
}

/// The manifest a reading over the fixture's subset is stamped through:
/// the subset stands where `ext/fixture`'s would, so its crate is the
/// reading's scope and the manifest must name it (harness §4.5).
fn fixture_manifest() -> Manifest {
    manifest_of([Crate::named("ext/fixture")])
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

/// The tracked pin's php-src tree, when it is on this machine. The tree
/// the pin names is outside the clone (harness-design.md §5), so a runner
/// has the corpus nowhere and the tests that resolve a subset of it say so
/// and return, as the ones that spawn the oracle do.
fn source() -> Option<PathBuf> {
    let tree = manifest().pin.source.tree();
    if tree.is_dir() {
        Some(tree)
    } else {
        eprintln!("the pinned php-src is not at {}; skipping", tree.display());
        None
    }
}

/// A subset of one test, in a directory of this test's own, standing where
/// `php-src/ext/fixture/tests` would.
fn subset(name: &str, tests: &[(&str, &str)]) -> Subset {
    let dir = std::env::temp_dir().join(format!("xtask-phpt-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for (file, text) in tests {
        fs::write(dir.join(file), text).unwrap();
    }
    Subset {
        scope: "ext/fixture".to_owned(),
        dirs: vec![Dir {
            under: PathBuf::from("ext/fixture/tests"),
            dir,
        }],
    }
}

fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}

/// Run one subset with the pinned oracle standing in for the engine.
fn run(subset: &Subset, registry: &Registry) -> Tally {
    let binary = oracle().unwrap();
    let manifest = manifest();
    let oracle = manifest.pin.oracle();
    phpt::run_subset(&oracle, registry, &Runner::Engine(binary), None, subset).unwrap()
}

fn only(tally: &Tally) -> (Judgement, String) {
    assert_eq!(tally.judged.len(), 1, "{tally:#?}");
    let judged = &tally.judged[0];
    (judged.judgement, judged.note.clone())
}

#[test]
fn a_test_that_meets_its_expect_section_passes() {
    if oracle().is_none() {
        return;
    }
    let subset = subset(
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
    let subset = subset(
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
    let tally = phpt::run_subset(
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
    let subset = subset(
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
fn a_missed_expectation_fails_with_the_difference_and_the_reading_names_the_file() {
    if oracle().is_none() {
        return;
    }
    let subset = subset(
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

    let reading = phpt::reading(
        &tally,
        workspace(),
        &fixture_manifest(),
        &subset,
        &Runner::Engine(oracle().unwrap()),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.instrument, "phpt");
    assert_eq!(reading.scope, "ext/fixture");
    assert!(
        reading.message.contains("missed.phpt"),
        "{}",
        reading.message
    );
    assert_eq!(reading.numbers["failed"], Number::Int(1));
    assert_eq!(reading.numbers["passed"], Number::Int(0));
    assert_eq!(reading.numbers["tests"], Number::Int(1));
}

#[test]
fn the_same_missed_expectation_passes_under_an_accepted_divergence_row() {
    if oracle().is_none() {
        return;
    }
    let subset = subset(
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

    let reading = phpt::reading(
        &tally,
        workspace(),
        &fixture_manifest(),
        &subset,
        &Runner::Engine(oracle().unwrap()),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass);
    assert_eq!(reading.numbers["diverged"], Number::Int(1));
}

#[test]
fn a_skipif_that_says_skip_skips_with_its_reason() {
    if oracle().is_none() {
        return;
    }
    let subset = subset(
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
    let subset = subset(
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
    let subset = subset("borked", &[("borked.phpt", "--NOTATEST--\nnothing\n")]);
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
    let subset = subset(
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
    let subset = subset(
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
    let manifest = manifest();
    for name in ["pcre", "ext/pcre"] {
        let subset = Subset::resolve(&manifest, name).unwrap();
        assert_eq!(subset.scope, "ext/pcre");
        assert_eq!(subset.dirs.len(), 1, "{subset:#?}");
        assert_eq!(subset.dirs[0].under, Path::new("ext/pcre/tests"));
        assert_eq!(
            subset.dirs[0].dir,
            manifest.pin.source.tree().join("ext/pcre/tests"),
            "the subset is the pinned tree's"
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
    let subset = Subset::resolve(&manifest, "Zend/tests").unwrap();
    assert_eq!(subset.scope, "Zend/tests");
    assert_eq!(subset.dirs[0].under, Path::new("Zend/tests"));

    let error = Subset::resolve(&manifest, "nope").unwrap_err().to_string();
    assert!(
        error.contains("ext/nope") && error.contains("nope"),
        "{error}"
    );
}

#[test]
fn a_tier_lane_and_a_control_are_readings_of_their_own() {
    let subset = Subset::resolve(&manifest(), "pcre").unwrap();
    let engine = Runner::Engine(PathBuf::from("target/debug/php"));
    assert_eq!(subset.scope(&engine, None), "ext/pcre");
    assert_eq!(
        subset.scope(&engine, Some(phpt::Lane::Force)),
        "ext/pcre@force"
    );
    assert_eq!(
        subset.scope(&engine, Some(phpt::Lane::ExitFuzz)),
        "ext/pcre@exit-fuzz"
    );
    // A control of the runner keeps a reading of its own, so a green one
    // can never sit where the engine's reading belongs.
    let control = Runner::Control(PathBuf::from("php"));
    assert_eq!(subset.scope(&control, None), "ext/pcre@control");
}

#[test]
fn with_no_engine_built_the_reading_is_a_skip_that_says_so_and_no_test_runs() {
    if source().is_none() {
        return;
    }
    let root = std::env::temp_dir().join(format!("xtask-phpt-absent-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let manifest = manifest();
    let runner = Runner::resolve(&root, &manifest.pin, None).unwrap();
    assert!(matches!(runner, Runner::Absent(_)), "{runner:?}");

    let subset = Subset::resolve(&manifest, "pcre").unwrap();
    let oracle = manifest.pin.oracle();
    let tally = phpt::run_subset(&oracle, &registry(""), &runner, None, &subset).unwrap();
    assert!(tally.judged.is_empty());
    assert!(
        tally.found > 100,
        "the subset was still counted: {tally:#?}"
    );
    let reading = phpt::reading(&tally, workspace(), &manifest, &subset, &runner, None).unwrap();
    assert_eq!(reading.status, Status::Skip);
    assert_eq!(reading.scope, "ext/pcre");
    assert!(reading.message.contains("not built"), "{}", reading.message);
}

#[test]
fn the_pinned_oracle_as_the_binary_is_the_runners_control_and_never_a_reading_of_the_engine() {
    let Some(binary) = oracle() else { return };
    let manifest = fixture_manifest();
    let runner = Runner::resolve(workspace(), &manifest.pin, Some(&binary)).unwrap();
    assert!(matches!(runner, Runner::Control(_)), "{runner:?}");

    let subset = subset(
        "control",
        &[(
            "pass.phpt",
            "--TEST--\nEcho\n--FILE--\n<?php echo \"one\";\n--EXPECT--\none\n",
        )],
    );
    let oracle = manifest.pin.oracle();
    let tally = phpt::run_subset(&oracle, &registry(""), &runner, None, &subset).unwrap();
    assert_eq!(only(&tally).0, Judgement::Pass);
    let reading = phpt::reading(&tally, workspace(), &manifest, &subset, &runner, None).unwrap();
    assert_eq!(
        reading.status,
        Status::Skip,
        "a green control must not stand in for the engine: {}",
        reading.message
    );
    assert!(reading.message.contains("control"), "{}", reading.message);
}

#[test]
fn a_subset_where_nothing_met_an_expectation_is_a_skip_and_not_a_green_reading() {
    if oracle().is_none() {
        return;
    }
    let subset = subset(
        "nothing",
        &[(
            "skipped.phpt",
            "--TEST--\nSkipped\n--SKIPIF--\n<?php echo \"skip not here\\n\";\n--FILE--\n<?php echo \"never\";\n--EXPECT--\nnever\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(only(&tally).0, Judgement::Skip);
    let reading = phpt::reading(
        &tally,
        workspace(),
        &fixture_manifest(),
        &subset,
        &Runner::Engine(oracle().unwrap()),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Skip, "{}", reading.message);
    assert!(
        reading.message.contains("no test of the subset ran"),
        "{}",
        reading.message
    );
}

#[test]
fn the_reading_carries_the_burndowns_one_number() {
    if oracle().is_none() {
        return;
    }
    let subset = subset(
        "percent",
        &[
            (
                "one.phpt",
                "--TEST--\nOne\n--FILE--\n<?php echo \"one\";\n--EXPECT--\none\n",
            ),
            (
                "two.phpt",
                "--TEST--\nTwo\n--FILE--\n<?php echo \"two\";\n--EXPECT--\nnot two\n",
            ),
        ],
    );
    let tally = run(&subset, &registry(""));
    let reading = phpt::reading(
        &tally,
        workspace(),
        &fixture_manifest(),
        &subset,
        &Runner::Engine(oracle().unwrap()),
        None,
    )
    .unwrap();
    assert_eq!(reading.numbers["percent"], Number::Float(50.0));
}

#[test]
fn a_crate_resolves_to_every_subset_its_row_names() {
    let manifest = manifest_of([Crate::named("php_engine").with_phpt(&["Zend/tests", "tests"])]);
    let subset = Subset::resolve(&manifest, "php_engine").unwrap();
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
    let manifest = manifest_of([Crate::named("php_heap")]);
    let error = Subset::resolve(&manifest, "php_heap")
        .unwrap_err()
        .to_string();
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
fn a_subset_the_pinned_tree_has_not_is_a_reading_that_says_so_and_not_a_silent_drop() {
    if source().is_none() {
        return;
    }
    let manifest =
        manifest_of([Crate::named("php_engine").with_phpt(&["Zend/tests", "Zend/nope"])]);
    // `done` asks a crate for a phpt reading whenever its row names one,
    // and it never reads the tree; so a directory the tree has not must be
    // a reading a run can write, never a subset quietly dropped.
    let subset = Subset::resolve(&manifest, "php_engine").unwrap();
    assert_eq!(subset.absent(), [Path::new("Zend/nope")]);
    let reading = phpt::reading(
        &Tally::default(),
        workspace(),
        &manifest,
        &subset,
        &Runner::Absent("no engine".to_owned()),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading.message.contains("Zend/nope") && reading.message.contains("php-8.5.9"),
        "the row and the pin are both named: {}",
        reading.message
    );
}

#[test]
fn a_bare_run_is_the_per_commit_lane_composed_from_the_crate_rows() {
    let manifest = manifest_of([
        Crate::named("php_engine").with_phpt(&["Zend/tests", "tests"]),
        Crate::named("php_heap"),
        Crate::named("php_sapi").with_phpt(&["sapi/cli/tests"]),
        Crate::named("ext/pcre"),
        Crate::named("ext/curl").with_feature("curl"),
    ]);
    let lane: Vec<String> = Subset::lane(&manifest)
        .into_iter()
        .map(|subset| subset.scope)
        .collect();
    // §7's per-commit row: Zend/tests and tests/ with php_engine, sapi/cli
    // with php_sapi, and the default extensions' own. A crate behind a
    // feature is not in the default build, and one with no subset owns none.
    assert_eq!(lane, ["php_engine", "php_sapi", "ext/pcre"]);
}

#[test]
fn a_run_with_no_subset_runs_the_lane_rather_than_refusing() {
    let manifest = manifest_of([Crate::named("ext/ctype")]);
    // No engine is built here, so every subset of the lane is a skip that
    // says so; what is under test is that the lane composed at all.
    let readings = phpt::run(workspace(), &manifest, &registry(""), &Ask::default()).unwrap();
    let scopes: Vec<&str> = readings.iter().map(|r| r.scope.as_str()).collect();
    assert_eq!(scopes, ["ext/ctype"], "{readings:#?}");
}

#[test]
fn the_reading_of_an_extension_subset_is_stamped_over_the_crate_so_done_can_read_it() {
    let manifest = manifest();
    let subset = Subset::resolve(&manifest, "pcre").unwrap();
    let runner = Runner::Absent("no engine".to_owned());
    let reading = phpt::reading(
        &Tally::default(),
        workspace(),
        &manifest,
        &subset,
        &runner,
        None,
    )
    .unwrap();
    assert_eq!(reading.scope, "ext/pcre");
    assert_eq!(
        reading.tree(),
        xtask::git::tree_hash(workspace(), Path::new("ext/pcre")).unwrap(),
        "done reads the reading as fresh only when the tree matches the crate's"
    );
}

/// A directory of the pinned corpus no crate owns is a scope the mapping
/// knows only through the tree: its reading is stamped over the whole
/// workspace, since no crate's sources are what it measures, and `done`
/// never asks for it.
#[test]
fn a_corpus_directory_no_crate_owns_is_stamped_over_the_workspace() {
    if source().is_none() {
        return;
    }
    let manifest = manifest();
    assert_eq!(manifest.scope_path("Zend/tests").unwrap(), Path::new("."));
    assert_eq!(
        manifest.scope_path("Zend/tests@control").unwrap(),
        Path::new(".")
    );
    assert_eq!(manifest.scope_path("Zend/nowhere"), None);
}

#[test]
fn every_expectf_section_of_the_pinned_pcre_subset_compiles() {
    let manifest = manifest();
    let subset = Subset::resolve(&manifest, "pcre").unwrap();
    let Ok(tests) = subset.tests() else { return };
    if tests.is_empty() {
        eprintln!("the pinned php-src tree is not on this machine; skipping");
        return;
    }
    let mut seen = 0;
    for test in tests {
        let Ok(phpt) = phpt::file::Phpt::load(&test) else {
            continue;
        };
        for (section, expectation) in [
            (
                "EXPECTF",
                xtask::oracle::expect::Expectation::Format as fn(String) -> _,
            ),
            ("EXPECTREGEX", xtask::oracle::expect::Expectation::Pattern),
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

#[test]
fn the_full_corpus_is_a_flag_the_manifest_denies_at_the_tool_call() {
    // The rule's fixture is the negative control; this asserts the flag the
    // rule names is the flag the runner has.
    let ask = Ask {
        full: true,
        ..Ask::default()
    };
    assert!(ask.full);
    let rule = manifest()
        .rule("lanes.nightly")
        .expect("the manifest has the rule")
        .predicate
        .deny_pattern
        .clone()
        .expect("the rule denies a pattern");
    assert!(rule.contains("phpt"), "{rule}");
}
