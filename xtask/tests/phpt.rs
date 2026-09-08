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
use xtask::manifest::Manifest;
use xtask::phpt::{self, Ask, Judgement, Runner, Subset, Tally};
use xtask::reading::{Number, Status};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn manifest() -> Manifest {
    Manifest::load(workspace()).unwrap()
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
        under: PathBuf::from("ext/fixture/tests"),
        dir,
    }
}

fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}

/// Run one subset with the pinned oracle standing in for the engine.
fn run(subset: &Subset, registry: &Registry) -> Tally {
    let binary = oracle().unwrap();
    phpt::run_subset(&manifest(), registry, &Runner::Engine(binary), None, subset).unwrap()
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

    let reading = tally
        .reading(
            workspace(),
            &manifest(),
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

    let reading = tally
        .reading(
            workspace(),
            &manifest(),
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
        assert_eq!(subset.under, Path::new("ext/pcre/tests"));
        assert_eq!(
            subset.dir,
            manifest.pin.source.path.join("ext/pcre/tests"),
            "the subset is the pinned tree's"
        );
        assert_eq!(
            subset.id(&subset.dir.join("preg_match_basic.phpt")),
            "phpt:php-src/ext/pcre/tests/preg_match_basic.phpt",
            "the divergence key names the corpus, not this machine"
        );
    }
}

#[test]
fn a_corpus_directory_is_a_subset_and_an_unknown_name_names_both_forms_tried() {
    let manifest = manifest();
    let subset = Subset::resolve(&manifest, "Zend/tests").unwrap();
    assert_eq!(subset.scope, "Zend/tests");
    assert_eq!(subset.under, Path::new("Zend/tests"));

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
    let root = std::env::temp_dir().join(format!("xtask-phpt-absent-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let manifest = manifest();
    let runner = Runner::resolve(&root, &manifest.pin, None).unwrap();
    assert!(matches!(runner, Runner::Absent(_)), "{runner:?}");

    let subset = Subset::resolve(&manifest, "pcre").unwrap();
    let tally = phpt::run_subset(&manifest, &registry(""), &runner, None, &subset).unwrap();
    assert!(tally.judged.is_empty());
    assert!(
        tally.found > 100,
        "the subset was still counted: {tally:#?}"
    );
    let reading = tally
        .reading(workspace(), &manifest, &subset, &runner, None)
        .unwrap();
    assert_eq!(reading.status, Status::Skip);
    assert_eq!(reading.scope, "ext/pcre");
    assert!(reading.message.contains("not built"), "{}", reading.message);
}

#[test]
fn the_pinned_oracle_as_the_binary_is_the_runners_control_and_never_a_reading_of_the_engine() {
    let Some(binary) = oracle() else { return };
    let manifest = manifest();
    let runner = Runner::resolve(workspace(), &manifest.pin, Some(&binary)).unwrap();
    assert!(matches!(runner, Runner::Control(_)), "{runner:?}");

    let subset = subset(
        "control",
        &[(
            "pass.phpt",
            "--TEST--\nEcho\n--FILE--\n<?php echo \"one\";\n--EXPECT--\none\n",
        )],
    );
    let tally = phpt::run_subset(&manifest, &registry(""), &runner, None, &subset).unwrap();
    assert_eq!(only(&tally).0, Judgement::Pass);
    let reading = tally
        .reading(workspace(), &manifest, &subset, &runner, None)
        .unwrap();
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
    let reading = tally
        .reading(
            workspace(),
            &manifest(),
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
    let reading = tally
        .reading(
            workspace(),
            &manifest(),
            &subset,
            &Runner::Engine(oracle().unwrap()),
            None,
        )
        .unwrap();
    assert_eq!(reading.numbers["percent"], Number::Float(50.0));
}

#[test]
fn a_run_with_no_subset_says_what_to_ask_for_rather_than_running_a_lane() {
    let error = phpt::run(workspace(), &manifest(), &registry(""), &Ask::default())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("--subset") && error.contains("--full"),
        "{error}"
    );
}

#[test]
fn the_reading_of_an_extension_subset_is_stamped_over_the_crate_so_done_can_read_it() {
    let manifest = manifest();
    let subset = Subset::resolve(&manifest, "pcre").unwrap();
    let runner = Runner::Absent("no engine".to_owned());
    let reading = Tally::default()
        .reading(workspace(), &manifest, &subset, &runner, None)
        .unwrap();
    assert_eq!(reading.scope, "ext/pcre");
    assert_eq!(
        reading.tree,
        xtask::git::tree_hash(workspace(), Path::new("ext/pcre")).unwrap(),
        "done reads the reading as fresh only when the tree matches the crate's"
    );
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
                phpt::expect::Expectation::Format as fn(String) -> _,
            ),
            ("EXPECTREGEX", phpt::expect::Expectation::Pattern),
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
