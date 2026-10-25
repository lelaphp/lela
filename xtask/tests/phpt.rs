//! `xtask phpt`, the subset runner (harness-design.md §3, §7, §8): a subset
//! of the pinned corpus run through a binary under the oracle's recipe,
//! each test held to its `--EXPECT*--` section, and one reading per subset.
//! Every test here goes through the instrument's `run` and reads what it
//! wrote; how one test is judged, the notes and the subset's resolution,
//! is held beside the judge.
//!
//! The tests that spawn php run the pinned oracle behind a shell file
//! standing where the engine's binary would, since the engine that will be
//! measured does not exist yet: the runner reads the file as an engine,
//! since its bytes are not the oracle's. They say so on stderr and return
//! when the pinned binary is not on this machine, unless the machine
//! promised it (#38); the rest run anywhere.
#![cfg(tracked)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use xtask::divergence::Registry;
use xtask::manifest::{Crate, Manifest};
use xtask::oracle::{Pin, Promise};
use xtask::phpt::{self, Ask};
use xtask::reading::{Number, Reading, Status};
use xtask::scratch::Tree;

mod common {
    pub mod manifest;
    pub mod phpt_tree;
    pub mod registry;
    pub mod repo;
    pub mod run;
}
use common::manifest::{of_rows, pin};
use common::phpt_tree::fixture_tree;
use common::registry::{Row, registry, rows};
use common::repo::{Repo, scratch_workspace};
use common::run::Run;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// A manifest of `ext/pcre`'s row alone under the tracked pin: what the
/// tests that run a subset of the pinned tree judge by, the extension
/// being one whose tests stand there.
fn manifest() -> Manifest {
    of_rows([Crate::named("ext/pcre")])
}

/// A manifest of these `[[crate]]` rows under the tracked pin with its
/// tree the one given: the rows a person writes in `seams.toml`, resolved
/// against a tree without the tracked manifest having to carry the crate
/// they name or this machine the corpus. The caller holds the tree open
/// for as long as it reads the manifest.
fn manifest_of(rows: impl IntoIterator<Item = Crate>, tree: &Path) -> Manifest {
    let mut manifest = Manifest::of(rows);
    manifest.pin = Some(pin_over(tree));
    manifest
}

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary (#38).
fn oracle() -> Option<PathBuf> {
    let pin = pin();
    Promise::Oracle
        .or_skip(pin.oracle.file().map(Path::to_path_buf))
        .unwrap()
}

/// The tracked pin's php-src tree, when it is on this machine: what the
/// variable names, else the pin's path (harness-design.md §4.3). A runner
/// that sets neither has the corpus nowhere, and the tests that run a
/// subset of it say so and return, as the ones that spawn the oracle do;
/// where the runner promised the tree, they fail naming both places
/// tried (#38).
fn source() -> Option<PathBuf> {
    Promise::Tree.or_skip(pin().source.tree()).unwrap()
}

/// A workspace of this test's own: a git repository holding the tracked
/// manifest, one crate `ext/fixture`, a corpus tree under `corpus/` whose
/// `ext/fixture/tests` holds `tests`, and a shell file at `engine/php`
/// that hands every call to the pinned oracle.
struct Scratch {
    root: Tree,
    /// The manifest a run reads: the `ext/fixture` row under the tracked
    /// pin with its source tree pointed at `corpus/`, so the crate's subset
    /// by the design, `ext/fixture/tests`, resolves there.
    manifest: Manifest,
    engine: PathBuf,
    /// Where its run holds the conflict keys as locks: a directory of its
    /// own, not the machine's temporary directory, so a real run beside it
    /// holding `all` makes it wait for nothing (#1675).
    locks: Tree,
}

impl Scratch {
    fn new(name: &str, tests: &[(&str, &str)]) -> Scratch {
        let root = Tree::new(&format!("phpt-{name}")).unwrap();
        scratch_workspace(workspace(), &root);
        fs::create_dir_all(root.join("ext/fixture")).unwrap();
        fs::write(
            root.join("ext/fixture/Cargo.toml"),
            "[package]\nname = \"php_ext_fixture\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let corpus = root.join("corpus");
        let dir = corpus.join("ext/fixture/tests");
        fs::create_dir_all(&dir).unwrap();
        for (file, text) in tests {
            fs::write(dir.join(file), text).unwrap();
        }
        let engine = root.join("engine/php");
        fs::create_dir_all(engine.parent().unwrap()).unwrap();
        fs::write(
            &engine,
            format!(
                "#!/bin/sh\nexec \"{}\" \"$@\"\n",
                oracle().unwrap().display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut manifest = Manifest::named("ext/fixture");
        manifest.pin = Some(pin_over(&corpus));
        Scratch {
            root,
            manifest,
            engine,
            locks: Tree::new(&format!("phpt-{name}-locks")).unwrap(),
        }
    }

    /// The one reading of the fixture's subset, run through `engine`.
    fn run(&self, registry: &Registry, engine: &Path) -> Reading {
        self.run_among(registry, engine, self.locks.path())
    }

    /// The one reading of the fixture's subset, its keys held as locks in
    /// `locks`: where two scratch runs meet as two runs on the machine do.
    fn run_among(&self, registry: &Registry, engine: &Path, locks: &Path) -> Reading {
        let ask = Ask {
            subset: Some("ext/fixture".to_owned()),
            engine: phpt::Engine::Named(engine.to_path_buf()),
            locks: Some(locks.to_path_buf()),
            ..Ask::default()
        };
        let readings = phpt::run(&self.root, &self.manifest, registry, &ask).unwrap();
        assert_eq!(readings.len(), 1, "{readings:#?}");
        readings.into_iter().next().unwrap()
    }
}

/// The tracked pin with its tree at `corpus`, as if the variable named
/// it: the way a test points the pin at a tree of its own.
fn pin_over(corpus: &Path) -> Pin {
    let pin = pin().on_this_machine(Some(corpus.to_path_buf()));
    assert_eq!(
        pin.source.tree().unwrap(),
        corpus,
        "the tree is the scratch corpus"
    );
    pin
}

fn count(reading: &Reading, key: &str) -> i64 {
    match reading.numbers.get(key) {
        Some(Number::Int(n)) => *n,
        other => panic!("{key} is {other:?} in {reading:#?}"),
    }
}

#[test]
fn a_missed_expectation_fails_with_the_counts_and_the_reading_names_the_file() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new(
        "missed",
        &[(
            "missed.phpt",
            "--TEST--\nA wrong expectation\n--FILE--\n<?php echo \"four\";\n--EXPECT--\nfive\n",
        )],
    );
    let reading = scratch.run(&registry(""), &scratch.engine);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(reading.instrument, "phpt");
    assert_eq!(reading.scope, "ext/fixture");
    assert!(
        reading.message.contains("missed.phpt"),
        "{}",
        reading.message
    );
    assert_eq!(count(&reading, "failed"), 1);
    assert_eq!(count(&reading, "passed"), 0);
    assert_eq!(count(&reading, "tests"), 1);
    assert_eq!(count(&reading, "run"), 1);
}

/// `--unkept` (#1830): the runner's control runs only a subset whose kept
/// target does not stand. With none kept under the pin it runs and keeps
/// one; run again, the target holds a verdict for each of the subset's
/// tests and the subset is not run; a test added to the subset is one the
/// target predates, so it runs again and keeps the target over both. Asked
/// of a binary that is not the pinned php, it is refused, since only the
/// control keeps a target.
#[test]
fn the_control_with_unkept_runs_only_a_subset_whose_kept_target_does_not_stand() {
    let Some(php) = oracle() else {
        return;
    };
    let test = |name: &str| {
        format!("--TEST--\n{name}\n--FILE--\n<?php echo '{name}';\n--EXPECT--\n{name}\n")
    };
    let scratch = Scratch::new("unkept", &[("a.phpt", &test("a"))]);
    let ask = |engine: &Path| Ask {
        subset: Some("ext/fixture".to_owned()),
        engine: phpt::Engine::Named(engine.to_path_buf()),
        locks: Some(scratch.locks.path().to_path_buf()),
        unkept: true,
        ..Ask::default()
    };
    let run = || phpt::run(&scratch.root, &scratch.manifest, &registry(""), &ask(&php));
    let pin = scratch.manifest.pin().unwrap();
    let kept = scratch.root.join(format!(
        "target/harness/phpt-runs/pin-{}-{}/ext/fixture.json",
        pin.source.tag, pin.oracle.md5
    ));
    let verdicts = || -> Vec<String> {
        let run: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&kept).unwrap()).unwrap();
        run["verdicts"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };
    let id = |name: &str| format!("phpt:php-src/ext/fixture/tests/{name}");

    let readings = run().unwrap();
    assert_eq!(
        readings.len(),
        1,
        "no target kept, so the subset runs: {readings:#?}"
    );
    assert_eq!(readings[0].scope, "ext/fixture@control");
    assert_eq!(verdicts(), [id("a.phpt")], "and keeps its target");

    let readings = run().unwrap();
    assert!(
        readings.is_empty(),
        "the target stands, so nothing runs: {readings:#?}"
    );

    fs::write(
        scratch.root.join("corpus/ext/fixture/tests/b.phpt"),
        test("b"),
    )
    .unwrap();
    let readings = run().unwrap();
    assert_eq!(
        readings.len(),
        1,
        "the target predates b.phpt: {readings:#?}"
    );
    assert_eq!(
        verdicts(),
        [id("a.phpt"), id("b.phpt")],
        "kept again over both"
    );

    let refused = phpt::run(
        &scratch.root,
        &scratch.manifest,
        &registry(""),
        &ask(&scratch.engine),
    )
    .expect_err("a binary that is not the pin keeps no target");
    assert!(format!("{refused:#}").contains("--unkept"), "{refused:#}");
}

/// An extension's derived subset is every `.phpt` under `ext/<name>`, as
/// run-tests.php walks the directory, so a test in a directory beside
/// `tests/`, as `ext/intl/uchar/tests` stands beside `ext/intl/tests`, is
/// the extension's with no `phpt` key in its row (#1590, #1598).
#[test]
fn a_nested_test_directory_is_held_in_the_extensions_derived_subset() {
    if oracle().is_none() {
        return;
    }
    let pass = "--TEST--\nHeld\n--FILE--\n<?php echo \"ok\";\n--EXPECT--\nok\n";
    let scratch = Scratch::new("nested", &[("top.phpt", pass)]);
    let nested = scratch.root.join("corpus/ext/fixture/uchar/tests");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("nested.phpt"), pass).unwrap();
    let reading = scratch.run(&registry(""), &scratch.engine);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(reading.scope, "ext/fixture");
    assert_eq!(count(&reading, "tests"), 2, "{reading:#?}");
    assert_eq!(count(&reading, "files"), 2, "{reading:#?}");
    assert_eq!(count(&reading, "passed"), 2, "{reading:#?}");
}

#[test]
fn the_same_missed_expectation_passes_under_an_accepted_divergence_row() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new(
        "accepted",
        &[(
            "missed.phpt",
            "--TEST--\nA wrong expectation\n--FILE--\n<?php echo \"four\";\n--EXPECT--\nfive\n",
        )],
    );
    let accepted = rows(&[Row::accepted("phpt:php-src/ext/fixture/tests/missed.phpt")]);
    let reading = scratch.run(&accepted, &scratch.engine);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "diverged"), 1);
    assert_eq!(count(&reading, "failed"), 0);
}

#[test]
fn a_subset_where_nothing_met_an_expectation_is_a_skip_and_not_a_green_reading() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new(
        "nothing",
        &[(
            "skipped.phpt",
            "--TEST--\nSkipped\n--SKIPIF--\n<?php echo \"skip not here\\n\";\n--FILE--\n<?php echo \"never\";\n--EXPECT--\nnever\n",
        )],
    );
    let reading = scratch.run(&registry(""), &scratch.engine);
    assert_eq!(reading.status, Status::Skip, "{}", reading.message);
    assert!(
        reading.message.contains("no test of the subset ran"),
        "{}",
        reading.message
    );
    assert_eq!(count(&reading, "skipped"), 1);
    assert_eq!(count(&reading, "passed"), 0);
}

#[test]
fn a_file_that_is_not_a_test_borks_and_the_subset_fails_on_it() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("borked", &[("borked.phpt", "--NOTATEST--\nnothing\n")]);
    let reading = scratch.run(&registry(""), &scratch.engine);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "borked"), 1);
    assert!(
        reading.message.contains("borked.phpt"),
        "{}",
        reading.message
    );
}

#[test]
fn the_reading_carries_the_burndowns_one_number() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new(
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
    let reading = scratch.run(&registry(""), &scratch.engine);
    assert_eq!(reading.numbers["percent"], Number::Float(50.0));
    assert_eq!(count(&reading, "tests"), 2);
    assert_eq!(count(&reading, "passed"), 1);
    assert_eq!(count(&reading, "failed"), 1);
}

#[test]
fn the_pinned_oracle_as_the_binary_is_the_runners_control_and_never_a_reading_of_the_engine() {
    let Some(binary) = oracle() else { return };
    let scratch = Scratch::new(
        "control",
        &[(
            "pass.phpt",
            "--TEST--\nEcho\n--FILE--\n<?php echo \"one\";\n--EXPECT--\none\n",
        )],
    );
    let reading = scratch.run(&registry(""), &binary);
    assert_eq!(count(&reading, "passed"), 1, "{reading:#?}");
    assert_eq!(
        reading.status,
        Status::Skip,
        "a green control must not stand in for the engine: {}",
        reading.message
    );
    assert!(reading.message.contains("control"), "{}", reading.message);
    assert_eq!(
        reading.scope, "ext/fixture@control",
        "a control keeps a reading of its own"
    );
}

#[test]
fn with_no_engine_built_the_reading_is_a_skip_that_says_so_and_no_test_runs() {
    if source().is_none() {
        return;
    }
    let root = Tree::new("phpt-absent").unwrap();
    scratch_workspace(workspace(), &root);
    let manifest = manifest();
    let ask = Ask {
        subset: Some("pcre".to_owned()),
        ..Ask::default()
    };
    // The crate the subset is of has no Cargo.toml in the scratch: the
    // reading says so ahead of the engine's binary (#64).
    let readings = phpt::run(&root, &manifest, &registry(""), &ask).unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert_eq!(reading.scope, "ext/pcre");
    assert!(
        reading
            .message
            .starts_with("ext/pcre: not yet created; the subset holds "),
        "{}",
        reading.message
    );
    assert_eq!(count(reading, "run"), 0);
    assert!(
        count(reading, "tests") > 100,
        "the subset was still counted: {reading:#?}"
    );

    // With the crate laid down, what is absent is the engine's binary.
    fs::create_dir_all(root.join("ext/pcre/src")).unwrap();
    fs::write(
        root.join("ext/pcre/Cargo.toml"),
        "[package]\nname = \"pcre\"\n",
    )
    .unwrap();
    let readings = phpt::run(&root, &manifest, &registry(""), &ask).unwrap();
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert!(reading.message.contains("not built"), "{}", reading.message);
    assert!(
        !reading.message.contains("not yet created"),
        "{}",
        reading.message
    );
    assert_eq!(count(reading, "run"), 0);
}

#[test]
fn a_subset_the_pinned_tree_has_not_is_a_reading_that_says_so_and_not_a_silent_drop() {
    let tree = fixture_tree();
    let manifest = manifest_of(
        [Crate::named("php_engine").with_phpt(&["Zend/tests", "Zend/nope"])],
        &tree,
    );
    // `done` asks a crate for a phpt reading whenever its row names one,
    // and it never reads the tree; so a directory the tree has not must be
    // a reading a run can write, never a subset quietly dropped.
    let ask = Ask {
        subset: Some("php_engine".to_owned()),
        ..Ask::default()
    };
    let root = Tree::new("phpt-absent-dir").unwrap();
    scratch_workspace(workspace(), &root);
    let readings = phpt::run(&root, &manifest, &registry(""), &ask).unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading.message.contains("Zend/nope") && reading.message.contains("php-8.5.12"),
        "the row and the pin are both named: {}",
        reading.message
    );
}

#[test]
fn a_bare_run_is_the_per_commit_lane_composed_from_the_crate_rows() {
    let tree = fixture_tree();
    let manifest = manifest_of(
        [
            Crate::named("php_engine").with_phpt(&["Zend/tests", "tests"]),
            Crate::named("php_heap"),
            Crate::named("sapi/cli").with_phpt(&["sapi/cli/tests"]),
            Crate::named("ext/pcre"),
            Crate::named("ext/curl").with_feature("curl"),
        ],
        &tree,
    );
    // No engine is built here, so every subset of the lane is a skip that
    // says so; what is under test is which subsets the lane is. §7's
    // per-commit row: Zend/tests and tests/ with php_engine, sapi/cli with
    // its host, and the default extensions' own. The scratch workspace
    // has no binary crate, so it carries no feature and a crate behind
    // one is outside its lane (revision 63); one with no subset owns none.
    let root = Tree::new("phpt-lane").unwrap();
    scratch_workspace(workspace(), &root);
    let readings = phpt::run(&root, &manifest, &registry(""), &Ask::default()).unwrap();
    let scopes: Vec<&str> = readings.iter().map(|r| r.scope.as_str()).collect();
    assert_eq!(
        scopes,
        ["php_engine", "sapi/cli", "ext/pcre"],
        "{readings:#?}"
    );
}

#[test]
fn a_run_with_no_subset_runs_the_lane_rather_than_refusing() {
    let tree = fixture_tree();
    let manifest = manifest_of([Crate::named("ext/ctype")], &tree);
    // A scratch workspace, so a run over this corpus is kept in no
    // checkout's store, as one of none was in main's (#3143).
    let root = Tree::new("phpt-no-subset").unwrap();
    scratch_workspace(workspace(), &root);
    let readings = phpt::run(&root, &manifest, &registry(""), &Ask::default()).unwrap();
    let scopes: Vec<&str> = readings.iter().map(|r| r.scope.as_str()).collect();
    assert_eq!(scopes, ["ext/ctype"], "{readings:#?}");
}

#[test]
fn the_reading_of_an_extension_subset_is_stamped_over_the_crate_so_done_can_read_it() {
    let tree = fixture_tree();
    let mut manifest = manifest();
    manifest.pin = Some(pin_over(&tree));
    let ask = Ask {
        subset: Some("pcre".to_owned()),
        ..Ask::default()
    };
    let root = Tree::new("phpt-stamp").unwrap();
    scratch_workspace(workspace(), &root);
    let readings = phpt::run(&root, &manifest, &registry(""), &ask).unwrap();
    let reading = &readings[0];
    assert_eq!(reading.scope, "ext/pcre");
    assert_eq!(
        reading.key(),
        Reading::for_scope(
            "phpt",
            "ext/pcre",
            &manifest,
            &xtask::key::Snapshot::disk(&root).unwrap()
        )
        .unwrap()
        .key(),
        "done reads the reading as fresh only when the tree matches the crate's"
    );
    assert_eq!(
        reading.key(),
        Reading::for_scope(
            "phpt",
            "ext/pcre@control",
            &manifest,
            &xtask::key::Snapshot::disk(&root).unwrap()
        )
        .unwrap()
        .key(),
        "a control of the subset is stamped over the same crate"
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

/// `Pin::load` is where the variable is read, so this is the one test
/// that sets it: the binary, over a scratch clone whose pin names a tree
/// that is nowhere, resolves `Zend/tests` from the fixture the variable
/// names, and with the variable naming no directory refuses naming both.
#[test]
fn the_variable_names_the_tree_to_a_run_of_the_binary() {
    let root = Tree::new("phpt-variable").unwrap();
    scratch_workspace(workspace(), &root);
    // The scratch clone's pin, its `[source].path` pointed nowhere, so the
    // fallback is absent whichever machine this runs on.
    let pinned = root.join("nowhere-pinned");
    let pin = root.join("oracle.toml");
    let mut in_source = false;
    let text: Vec<String> = fs::read_to_string(&pin)
        .unwrap()
        .lines()
        .map(|line| {
            if line.starts_with('[') {
                in_source = line.trim() == "[source]";
            }
            if in_source && line.starts_with("path = ") {
                format!("path = {:?}", pinned.display().to_string())
            } else {
                line.to_owned()
            }
        })
        .collect();
    fs::write(&pin, text.join("\n")).unwrap();
    let tree = fixture_tree();
    let ran = Run::at(&root)
        .args(["phpt", "--subset", "Zend/tests"])
        .env(xtask::oracle::TREE_VARIABLE, &*tree)
        .go();
    assert!(
        ran.stdout.contains("Zend/tests") && !ran.stderr.contains("neither place"),
        "the subset resolved under the fixture: {ran}"
    );
    let stderr = Run::at(&root)
        .args(["phpt", "--subset", "Zend/tests"])
        .env(xtask::oracle::TREE_VARIABLE, root.join("nowhere"))
        .go()
        .failed();
    assert!(
        stderr.contains(&format!(
            "{}={}",
            xtask::oracle::TREE_VARIABLE,
            root.join("nowhere").display()
        )) && stderr.contains(&pinned.display().to_string()),
        "both places are named: {stderr}"
    );
}

/// A relative `--root` reaches the verdicts an absolute one does (#942):
/// the engine is found under the root and run in each test's own
/// directory, so the root is made absolute where the command line is
/// read. Before, `--root .` handed every test `./target/debug/php`, its
/// `-m` listed no extension, and a test naming one skipped.
#[test]
fn a_relative_root_reaches_the_verdicts_an_absolute_one_does() {
    let Some(oracle) = oracle() else { return };
    let root = Tree::new("phpt-relative-root").unwrap();
    scratch_workspace(workspace(), &root);
    fs::create_dir_all(root.join("ext/ctype")).unwrap();
    fs::write(
        root.join("ext/ctype/Cargo.toml"),
        "[package]\nname = \"php_ext_ctype\"\n",
    )
    .unwrap();
    let corpus = root.join("corpus");
    fs::create_dir_all(corpus.join("ext/ctype/tests")).unwrap();
    fs::write(
        corpus.join("ext/ctype/tests/named.phpt"),
        "--TEST--\nnamed\n--EXTENSIONS--\nctype\n--FILE--\n<?php var_dump(ctype_digit('1'));\n--EXPECT--\nbool(true)\n",
    )
    .unwrap();
    let engine = root.join("target/debug/php");
    fs::create_dir_all(engine.parent().unwrap()).unwrap();
    fs::write(
        &engine,
        format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", oracle.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let run = |given: &Path| {
        let stdout = Run::at(given)
            .args(["phpt", "--subset", "ext/ctype", "--jobs", "1"])
            .current_dir(&root)
            .env(xtask::oracle::TREE_VARIABLE, &corpus)
            .go()
            .stdout;
        // The verdicts, the reading's line up to what it was run by.
        let verdicts = stdout
            .lines()
            .find(|line| line.contains(" phpt ext/ctype: "))
            .and_then(|line| line.split(';').next())
            .map(str::to_owned);
        (verdicts, stdout)
    };
    let (absolute, said) = run(root.path());
    assert_eq!(
        absolute.as_deref(),
        Some("pass phpt ext/ctype: ext/ctype: 1 passed of 1 tests"),
        "{said}"
    );
    let (relative, said) = run(Path::new("."));
    assert_eq!(relative, absolute, "{said}");
}

/// A binary standing where the engine's would that prints `__FILE__` as
/// the engine prints it, the path the program was run by, taken as the
/// first argument ending in `.php`; or, with `bare`, that name alone, the
/// relative name the lane handed before #331.
fn echoing_engine(scratch: &Scratch, bare: bool) -> PathBuf {
    let name = if bare { "bare-name" } else { "as-given" };
    let path = scratch.root.join("engine").join(name);
    let printed = if bare { "$(basename \"$arg\")" } else { "$arg" };
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    *.php) file=\"{printed}\"; printf 'string(%s) \"%s\"\\n' \"${{#file}}\" \"$file\"; exit 0 ;;\n  esac\ndone\nexit 1\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// The fixture at `xtask/fixtures/phpt/FileName_basic.phpt` (#331): a
/// `--EXPECTF--` of `%sFileName_basic.php`, where `%s` is one character
/// or more, as the corpus pins `ReflectionClass::getFileName()` and every
/// error site. Upstream's runner hands php the program's absolute path,
/// and the engine prints `__FILE__` as the path it was run by, so the
/// fixture is met when the lane hands the absolute path and missed on the
/// bare name the lane handed before: the second engine is the fixture's
/// negative control, and the first is what the runner now does.
#[test]
fn a_program_runs_by_its_absolute_path_so_a_directory_before_its_name_is_matched() {
    if oracle().is_none() {
        return;
    }
    let fixture = xtask::package_dir().join("fixtures/phpt/FileName_basic.phpt");
    let text = fs::read_to_string(&fixture).unwrap();
    assert!(
        text.contains("\"%sFileName_basic.php\""),
        "the fixture expects a directory before the name: {text}"
    );
    let scratch = Scratch::new("absolute", &[("FileName_basic.phpt", &text)]);

    let bare = echoing_engine(&scratch, true);
    let reading = scratch.run(&registry(""), &bare);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading.message.contains("FileName_basic.phpt"),
        "the fixture fails on the relative name: {}",
        reading.message
    );

    let as_given = echoing_engine(&scratch, false);
    let reading = scratch.run(&registry(""), &as_given);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "passed"), 1);
    assert_eq!(count(&reading, "failed"), 0);
}

/// A file's write time, set: what makes a stand-in binary older or newer
/// than the tree it stands over, without a sleep to answer by wall clock.
fn written_at(path: &Path, at: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at)
        .unwrap();
}

/// A binary the tree has moved past runs no test at all, and the reading
/// is a skip held to that cause: before this, whichever binary happened to
/// stand in `target` was the lane's engine, and one unchanged tree carried
/// two readings 37 tests apart (#475). It is not the absent binary `done`
/// excuses, so `engine_absent` stays at nought and `engine_stale` is the
/// number.
#[test]
fn a_binary_the_tree_has_moved_past_runs_nothing_and_the_skip_names_it() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new(
        "stale",
        &[(
            "pass.phpt",
            "--TEST--\nEcho\n--FILE--\n<?php echo \"one\";\n--EXPECT--\none\n",
        )],
    );
    written_at(&scratch.engine, SystemTime::now() - Duration::from_hours(1));
    let reading = scratch.run(&registry(""), &scratch.engine);
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert_eq!(count(&reading, "engine_stale"), 1);
    assert_eq!(count(&reading, "engine_absent"), 0);
    assert_eq!(count(&reading, "run"), 0, "no test ran under it");
    assert_eq!(count(&reading, "passed"), 0);
    assert!(
        reading.message.contains("was built before"),
        "{}",
        reading.message
    );
    assert!(
        reading
            .message
            .contains(&scratch.engine.display().to_string()),
        "{}",
        reading.message
    );
    assert!(
        reading
            .message
            .contains("cargo build --profile release-da -p php_sapi_cli -p php_sapi_cgi"),
        "{}",
        reading.message
    );
}

/// A binary the tree has not moved past is the lane's engine as before,
/// and the reading names it: the path and the md5 of its bytes, so two
/// readings of one tree can be told apart (#475).
#[test]
fn a_binary_the_tree_has_not_moved_past_writes_an_ordinary_reading_that_names_it() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new(
        "fresh",
        &[(
            "pass.phpt",
            "--TEST--\nEcho\n--FILE--\n<?php echo \"one\";\n--EXPECT--\none\n",
        )],
    );
    written_at(&scratch.engine, SystemTime::now());
    let reading = scratch.run(&registry(""), &scratch.engine);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "passed"), 1);
    assert_eq!(count(&reading, "engine_stale"), 0);
    let stamp = reading
        .message
        .split_once("; run by ")
        .unwrap_or_else(|| panic!("{}", reading.message))
        .1;
    let (path, digest) = stamp
        .split_once(" md5 ")
        .unwrap_or_else(|| panic!("{stamp}"));
    assert_eq!(path, scratch.engine.display().to_string());
    assert_eq!(digest.len(), 32, "{stamp}");
    assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()), "{stamp}");
}

/// Every file of the scratch workspace committed, `target/` ignored, and
/// the commit's id.
fn commit_all(root: &Path, message: &str) -> String {
    fs::write(root.join(".gitignore"), "target/\n").unwrap();
    Repo::at(root).commit(message)
}

/// A binary standing where the engine's would that writes `two` for the
/// program `planted.php` and hands every other call to the pinned oracle.
fn planting_engine(scratch: &Scratch) -> PathBuf {
    let path = scratch.root.join("engine/planting");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    *planted.php) echo two; exit 0 ;;\n  esac\ndone\nexec \"{}\" \"$@\"\n",
            oracle().unwrap().display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// One object `ipcs -a` lists, read by its section's header line as the
/// sweep reads it: its kind's letter, id, key, mode and the process that
/// made a segment or last sent to a queue.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ipc {
    kind: String,
    id: String,
    key: String,
    mode: String,
    pid: String,
}

/// The processes that last operated on each semaphore of the set `id`, as
/// the sweep reads them, `ipcs` printing none.
fn semaphore_pids(id: &str) -> Vec<String> {
    let read = Command::new("perl")
        .args([
            "-e",
            "use IPC::SysV qw(GETPID); for (my $i = 0; defined(my $pid = semctl($ARGV[0], $i, GETPID, 0)); $i++) { print \"$pid\\n\" }",
            id,
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&read.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Every object `ipcs -a` lists now.
fn ipcs() -> Vec<Ipc> {
    let listed = Command::new("ipcs").arg("-a").output().unwrap();
    let mut header: Vec<String> = Vec::new();
    let mut objects = Vec::new();
    for line in String::from_utf8_lossy(&listed.stdout).lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.first() {
            Some(&"T") => header = fields.iter().map(|f| (*f).to_owned()).collect(),
            Some(kind) if ["m", "s", "q"].contains(kind) => {
                let column = |name: &str| {
                    header
                        .iter()
                        .position(|h| h == name)
                        .and_then(|at| fields.get(at))
                        .map(|v| (*v).to_owned())
                        .unwrap_or_default()
                };
                objects.push(Ipc {
                    kind: (*kind).to_owned(),
                    id: column("ID"),
                    key: column("KEY"),
                    mode: column("MODE"),
                    pid: if *kind == "m" {
                        column("CPID")
                    } else {
                        column("LSPID")
                    },
                });
            }
            _ => {}
        }
    }
    objects
}

/// Whether `ipcs -a` here prints the tables with a `T` header line, which
/// is all the sweep reads; why the test returns where it does not.
fn ipcs_readable() -> Option<&'static str> {
    let Ok(listed) = Command::new("ipcs").arg("-a").output() else {
        return Some("no ipcs on this machine");
    };
    let readable = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .any(|line| line.starts_with("T "));
    (!readable)
        .then_some("ipcs prints no table with a `T` header here, which is all the sweep reads")
}

/// Why the machine's System V tables have no room for what the System V
/// test makes (#1508), none where they have: two queues, two segments
/// and a set among the test's program and `perl`, against each table's
/// limit as `ipcs -Q`, `-M` and `-S` print it and the objects `ipcs -a`
/// lists. A table full of other programs' objects fails `msg_get_queue`
/// or `shmop_open` with "No space left on device", which says nothing of
/// the sweep. A limit `ipcs` does not print is not held.
fn no_room() -> Option<String> {
    let stood = ipcs();
    let needs = [
        ("q", "-Q", "msgmni", 2),
        ("m", "-M", "shmmni", 2),
        ("s", "-S", "semmni", 1),
    ];
    for (kind, flag, limit, need) in needs {
        let Ok(info) = Command::new("ipcs").arg(flag).output() else {
            continue;
        };
        let most = String::from_utf8_lossy(&info.stdout)
            .lines()
            .find_map(|line| {
                let rest = line.trim().strip_prefix(limit)?.strip_prefix(':')?;
                rest.split_whitespace().next()?.parse::<usize>().ok()
            });
        let Some(most) = most else {
            continue;
        };
        let standing = stood.iter().filter(|o| o.kind == kind).count();
        if standing + need > most {
            return Some(format!(
                "the machine's {limit} is {most} and {standing} stand, no room for the {need} the test makes"
            ));
        }
    }
    None
}

/// What the test's program made, read off the machine while it stands,
/// and what `perl` made beside it.
struct Made {
    /// The segment, the set and the keyed queue, each naming its process.
    named: Vec<Ipc>,
    /// The ids of `perl`'s segment and keyed queue, a space between.
    perl: String,
}

/// Wait for the test's program to print its pid at `started`, read
/// its objects off the machine, have `perl` make a keyed segment and a
/// keyed queue, and mark `made` so the program ends. The segment is keyed
/// since a private one whose creator is gone is any reclaim's on the
/// machine to take (`sysv::abandoned`, #2115), a System V subset's run
/// beside this one among them (#2448). The wait for
/// `started` is the recipe's limit, 60 s, past which the program is gone
/// anyway: it covers the run's start and the program's, which took 1.4 s
/// to 3.0 s at a load average of 70 and outgrew the 5 s it had beside the
/// file's other tests (#2280). The program waits for `made` 30 s, inside
/// the same limit.
fn made_while_it_runs(started: &Path, made: &Path, stood: &[Ipc]) -> Made {
    let mut printed = String::new();
    for _ in 0..6000 {
        printed = fs::read_to_string(started).unwrap_or_default();
        if !printed.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = printed.trim().to_owned();
    let keyed = format!(
        "0x{:08x}",
        0x5a00_0000 + pid.parse::<u64>().unwrap_or(u64::MAX - 0x5a00_0000)
    );
    let named = ipcs()
        .into_iter()
        .filter(|o| !stood.contains(o))
        .filter(|o| match o.kind.as_str() {
            "m" => o.pid == pid,
            "s" => o.key == "0x00000000" && semaphore_pids(&o.id).contains(&pid),
            _ => o.key == keyed,
        })
        .collect();
    let perl = Command::new("perl")
        .args([
            "-e",
            "use IPC::SysV qw(IPC_CREAT); print shmget(0x5b100000 + $$, 64, IPC_CREAT | 0600), ' ', msgget(0x5b000000 + $$, IPC_CREAT | 0600)",
        ])
        .output()
        .unwrap();
    fs::write(made, "").unwrap();
    Made {
        named,
        perl: String::from_utf8(perl.stdout).unwrap(),
    }
}

/// A run removes what its own tests leave of System V IPC (#1412): a
/// shared memory segment, a semaphore set and a message queue a test
/// creates and leaves are gone after the run, and the reading counts them;
/// while a segment another process creates while the test runs, `perl`
/// here, stands, and so does a keyed queue `perl` makes then as the
/// runner's own user, naming no process either. The subset holds no
/// claim, so it takes only what its own processes are named on, and its
/// count is those three whatever another process makes meanwhile (#2563);
/// an object that names no process is a System V subset's to take (#1482).
/// The test tells its program's objects apart by the pid it printed, which
/// names the segment and the set and is in the keyed queue's key.
///
/// A machine whose tables have no room for what the test makes skips it
/// and says so (#1508); a miss names what the program printed in the
/// run's `fail` line, which the failure prints.
#[test]
fn a_run_removes_the_system_v_objects_its_tests_leave_and_nothing_else() {
    if oracle().is_none() {
        return;
    }
    if let Some(why) = ipcs_readable() {
        eprintln!("skipped: {why}");
        return;
    }
    if let Some(why) = no_room() {
        eprintln!("skipped: {why}");
        return;
    }
    let marks = Tree::new("phpt-sysv-marks").unwrap();
    let (started, made) = (marks.join("started"), marks.join("made"));
    let leaves = format!(
        "--TEST--\nleaves a segment, a semaphore set and a queue\n--FILE--\n<?php\n$m = shmop_open(0, \"c\", 0604, 64);\n$s = sem_get(0, 1, 0604);\nsem_acquire($s);\n$q = msg_get_queue(0x5a000000 + getmypid(), 0604);\nmsg_send($q, 1, \"x\");\nfile_put_contents('{}', getmypid());\nfor ($i = 0; $i < 3000 && !file_exists('{}'); $i++) usleep(10000);\necho \"left\";\n--EXPECT--\nleft\n",
        started.display(),
        made.display()
    );
    let scratch = Scratch::new("sysv", &[("leaves.phpt", &leaves)]);
    let stood: Vec<Ipc> = ipcs();
    // While the test's program stands, its objects are read off the
    // machine by what it printed, and another process makes a segment and
    // a keyed queue: new since the subset began, nothing attached, and
    // their creator no process of the run.
    let outside = std::thread::spawn(move || made_while_it_runs(&started, &made, &stood));
    let reading = scratch.run(&registry(""), &scratch.engine);
    let made = outside.join().unwrap();
    let (segment, keyed) = made.perl.split_once(' ').unwrap_or((&made.perl, ""));
    let after = ipcs();
    Command::new("ipcrm").args(["-m", segment]).status().ok();
    Command::new("ipcrm").args(["-q", keyed]).status().ok();
    assert!(
        !segment.is_empty() && !keyed.is_empty(),
        "perl made {:?}",
        made.perl
    );
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    let mut kinds: Vec<&str> = made.named.iter().map(|o| o.kind.as_str()).collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        ["m", "q", "s"],
        "the test's program made a segment, a set and a keyed queue: {:#?}",
        made.named
    );
    let standing: Vec<&Ipc> = made
        .named
        .iter()
        .filter(|o| after.iter().any(|a| a.kind == o.kind && a.id == o.id))
        .collect();
    assert_eq!(
        standing,
        Vec::<&Ipc>::new(),
        "the run left its test's objects"
    );
    assert_eq!(
        count(&reading, "sysv_removed"),
        3,
        "the run removed the three objects that name its test's process and nothing else: {reading:#?}"
    );
    assert!(
        after.iter().any(|o| o.kind == "m" && o.id == segment),
        "the segment perl made, {segment}, was removed"
    );
    assert!(
        after.iter().any(|o| o.kind == "q" && o.id == keyed),
        "the keyed queue perl made, {keyed}, was removed"
    );
}

/// A test of `ext/sysvmsg` that makes a private queue of mode 0600 and
/// sends nothing to it, so it names no process, as php's
/// `msg_get_queue(0)` leaves one, writes its pid and the new private
/// queues `ipcs -q` lists to `started`, none where its own was refused, so
/// a queue another session made is never read as its (#3007), and goes on
/// with `then`, its expectation after it.
fn leaves_a_private_queue(started: &Path, then: &str) -> String {
    format!(
        "--TEST--\nleaves a private queue\n--FILE--\n<?php\n$private = fn() => preg_grep('/^q\\s+\\d+\\s+0x00000000\\s/', explode(\"\\n\", shell_exec('ipcs -q')));\n$stood = $private();\n$q = msg_get_queue(0, 0600);\n$new = $q ? array_map(fn($line) => preg_split('/\\s+/', $line)[1], array_diff($private(), $stood)) : [];\nfile_put_contents('{}', getmypid() . ' ' . implode(' ', $new));\n{then}",
        started.display()
    )
}

/// What a test wrote to `at`, waited for as long as the recipe's limit.
fn printed(at: &Path) -> String {
    for _ in 0..6000 {
        let printed = fs::read_to_string(at).unwrap_or_default();
        if !printed.is_empty() {
            return printed;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    String::new()
}

/// Held by each test here that makes a private queue naming no process,
/// for its length: two at once each read the other's queue among their
/// own and take it in their sweeps, the kernel keeping no maker for it.
static PRIVATE_QUEUES: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What names the claims directory a run's System V claims are laid
/// apart in ([`OneTree::claims`]), so the claims a test reads are its
/// runs' alone; such a run still holds back from what a claim held in the
/// machine's covers, as a run beside another run does (#2902), and says
/// it left what it did.
const CLAIMS: &str = "XTASK_SYSV_CLAIMS";

/// What a sweep says of a new object that names no process and that it
/// leaves, as it does where another run's claim covers its second.
const STOOD: &str = "no process and stand";

/// What a sweep says where another run held a claim as it settled its own,
/// which then stands for a later reclaim (#3007).
const CLAIM_STANDS: &str = "claim stands for a later reclaim";

/// [`PRIVATE_QUEUES`] held, whatever test panicked holding it before.
fn alone() -> std::sync::MutexGuard<'static, ()> {
    PRIVATE_QUEUES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// When the process under `pid` started, as `ps -o lstart=` prints it,
/// empty where none runs: what tells the process a test started from one
/// that took its number since.
fn started_at(pid: &str) -> String {
    let ps = Command::new("ps")
        .args(["-o", "lstart=", "-p", pid])
        .output()
        .unwrap();
    String::from_utf8_lossy(&ps.stdout).trim().to_owned()
}

/// A System V subset's run killed before its sweep leaves its claim, and
/// the next System V subset's run removes what the killed one's tests
/// left and names it in its reading (#2448): here a private queue of
/// mode 0600 nobody sends to, naming no process, as php's
/// `msg_get_queue(0)` leaves one, made by a test of `ext/sysvmsg` that
/// then sleeps past the recipe's limit. The run is killed outright, which
/// leaves the test's php running, and the next run kills it before it
/// sweeps (#2522); the same php still running then is killed by this
/// test, which fails. The runs lay their claims apart ([`CLAIMS`]), so
/// the claims directory is theirs alone and no other run's reclaim takes
/// the claim first; another run's sweep on the machine may still take the
/// queue itself, which this test then cannot name, and a claim another
/// run holds on the machine keeps the next run from it and the killed
/// run's claim standing, which the test says and returns on.
#[test]
fn a_system_v_subset_killed_mid_run_leaves_a_claim_the_next_run_takes() {
    if oracle().is_none() {
        return;
    }
    let _alone = alone();
    if let Some(why) = ipcs_readable() {
        eprintln!("skipped: {why}");
        return;
    }
    let tree = OneTree::new("sysv-killed");
    let dir = tree.corpus.join("ext/sysvmsg/tests");
    fs::create_dir_all(&dir).unwrap();
    let marks = Tree::new("phpt-sysv-killed-marks").unwrap();
    let started = marks.join("started");
    fs::write(
        dir.join("leaves.phpt"),
        leaves_a_private_queue(&started, "sleep(120);\n--EXPECT--\n"),
    )
    .unwrap();
    let claims = tree.claims();
    let checkout = OneTree::checkout("sysv-killed");
    let run = || tree.command(&checkout, "ext/sysvmsg");
    let mut killed = run().spawn().unwrap();
    let printed = printed(&started);
    killed.kill().unwrap();
    killed.wait().unwrap();
    let mut words = printed.split_whitespace();
    let pid = words.next().expect("the test printed its pid");
    let php = started_at(pid);
    assert!(!php.is_empty(), "the killed run's php runs on");
    let appeared: Vec<&str> = words.collect();
    let left: Vec<PathBuf> = fs::read_dir(&claims)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(left.len(), 1, "the killed run left its claim: {left:?}");
    // Its journal names each program as it started, the test's own among
    // them, and its last write bounds the window the next run takes.
    let journal = fs::read_to_string(&left[0]).unwrap();
    assert!(
        journal
            .lines()
            .any(|line| line.starts_with("started ") && line.ends_with(&format!(" {pid}"))),
        "the claim names the test's program {pid}: {journal}"
    );
    fs::write(
        dir.join("leaves.phpt"),
        "--TEST--\nquick\n--FILE--\n<?php echo 'quick';\n--EXPECT--\nquick\n",
    )
    .unwrap();
    let out = run().output().unwrap();
    // Killed by this test only where it is still the same process.
    let orphan = started_at(pid) == php;
    if orphan {
        Command::new("kill").args(["-9", pid]).status().unwrap();
    }
    assert!(
        !orphan,
        "the next run left the killed run's php {pid} running"
    );
    assert!(out.status.success(), "{out:?}");
    let reading = xtask::reading::Store::of_workspace(checkout.path())
        .read("phpt", "ext/sysvmsg")
        .unwrap()
        .expect("the second run's reading");
    let [queue] = appeared.as_slice() else {
        eprintln!("the queue is not told apart: {appeared:?}");
        return;
    };
    let left = stands(queue);
    if fs::read_dir(&claims).unwrap().next().is_some() {
        if left {
            Command::new("ipcrm").args(["-q", queue]).status().ok();
        }
        eprintln!(
            "skipped: a claim held on the machine kept the killed run's: {}",
            reading.message
        );
        return;
    }
    assert!(!left, "the queue {queue} the killed run's test left stands");
    let named = "before the run, of what runs cut off left, removed";
    if reading.message.contains(named) {
        assert!(
            reading.message.contains(queue) && count(&reading, "sysv_removed") >= 1,
            "{reading:#?}"
        );
    } else {
        eprintln!(
            "another run's sweep took the queue {queue} first: {}",
            reading.message
        );
    }
}

/// Whether the queue `queue` stands, as `ipcs -a` lists the queues.
fn stands(queue: &str) -> bool {
    ipcs().iter().any(|o| o.kind == "q" && o.id == queue)
}

/// Whether the run's own sweep's words name `queue` among what it
/// removed, beside any private queue a run going at once made in the
/// same second; one gone that they do not name another run's sweep took.
fn took(reading: &Reading, queue: &str) -> bool {
    let own = reading
        .message
        .split_once("of what its tests left, removed ")
        .and_then(|(_, words)| words.split(';').next())
        .unwrap_or_default();
    own.split([' ', '(', ')', ',']).any(|word| word == queue)
}

/// A System V subset's own sweep removes the private queue its test leaves
/// that nobody sent to, naming no process, and counts it, where no other
/// run holds a claim in the claims directory it reads (#1482, #2571); and
/// with a claim held there from before the queue was made, as a run going
/// beside it holds one (#2563), it leaves the queue standing and its own
/// claim for a later reclaim, says so, and this test removes the queue. The
/// runs lay their claims apart ([`CLAIMS`]), so the claims they read are
/// this test's alone but for those held on the machine, which keep the
/// first run from the queue too, and the test says so and returns; a
/// private queue another session's run makes in the same second as the
/// test's program may still be taken with it, as by any System V subset's
/// sweep.
///
/// The machine is every session's (#3007): another run's sweep may take the
/// queue before the first run's does, and a claim another run holds as the
/// first run settles keeps its claim standing, which the run says. So the
/// test holds the first run to what it said of its claim, removes the claim
/// it said stands, which the test's directory alone holds, before the
/// second run, and removes only a queue its test made that still stands.
/// On Linux, where `unshare` puts the subset in a private IPC namespace,
/// the queue goes with its program, and the test says so and returns.
#[test]
fn a_system_v_subsets_own_sweep_takes_the_private_queue_its_test_leaves_unless_a_claim_is_held() {
    if oracle().is_none() {
        return;
    }
    let _alone = alone();
    if let Some(why) = ipcs_readable() {
        eprintln!("skipped: {why}");
        return;
    }
    let tree = OneTree::new("sysv-own");
    let dir = tree.corpus.join("ext/sysvmsg/tests");
    fs::create_dir_all(&dir).unwrap();
    let marks = Tree::new("phpt-sysv-own-marks").unwrap();
    let started = marks.join("started");
    let then = "echo 'left';\n--EXPECT--\nleft\n";
    fs::write(
        dir.join("leaves.phpt"),
        leaves_a_private_queue(&started, then),
    )
    .unwrap();
    let claims = tree.claims();
    let checkout = OneTree::checkout("sysv-own");
    let run = || {
        fs::remove_file(&started).ok();
        let out = tree.command(&checkout, "ext/sysvmsg").output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let reading = xtask::reading::Store::of_workspace(checkout.path())
            .read("phpt", "ext/sysvmsg")
            .unwrap()
            .expect("the run's reading");
        let printed = printed(&started);
        let queue = match printed.split_whitespace().collect::<Vec<_>>()[..] {
            [_, queue] => Some(queue.to_owned()),
            _ => None,
        };
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            reading,
            queue,
        )
    };
    let (said, reading, queue) = run();
    if said.contains("private IPC namespace") {
        eprintln!("skipped: the subset ran in a private IPC namespace: {said}");
        return;
    }
    let Some(queue) = queue else {
        eprintln!("the queue is not told apart: {}", reading.message);
        return;
    };
    if stands(&queue) {
        Command::new("ipcrm").args(["-q", &queue]).status().ok();
        assert!(said.contains(STOOD), "the run left {queue}: {said}");
        eprintln!("skipped: a claim held on the machine kept the run from {queue}: {said}");
        return;
    }
    if took(&reading, &queue) {
        assert!(count(&reading, "sysv_removed") >= 1, "{reading:#?}");
    } else {
        eprintln!("another run's sweep took the queue {queue} first: {said}");
    }
    let stood: Vec<PathBuf> = fs::read_dir(&claims)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        stood.len(),
        usize::from(said.contains(CLAIM_STANDS)),
        "the run's claim stands where it says so, and goes where not: {stood:?} {said}"
    );
    for claim in &stood {
        fs::remove_file(claim).unwrap();
    }
    // A claim held from before the queue is made, as a run going beside
    // this one holds its own.
    let held = fs::File::create(claims.join("held")).unwrap();
    held.try_lock().unwrap();
    let (said, reading, queue) = run();
    let Some(queue) = queue else {
        eprintln!("the queue is not told apart: {}", reading.message);
        return;
    };
    if stands(&queue) {
        Command::new("ipcrm").args(["-q", &queue]).status().ok();
        assert!(said.contains(STOOD), "{said}");
    } else {
        eprintln!("another run's sweep took the queue {queue}: {said}");
    }
    assert!(
        !took(&reading, &queue),
        "the run took its test's queue {queue} while a claim was held: {reading:#?}"
    );
    assert!(said.contains(CLAIM_STANDS), "{said}");
    assert_eq!(
        fs::read_dir(&claims).unwrap().count(),
        2,
        "the run keeps its claim beside the held one"
    );
    drop(held);
}

/// The queues a test outside the four System V subsets leaves, removed
/// after it (#2811): `Zend/tests/arginfo_zpp_mismatch.phpt` calls every
/// function with nulls, and `msg_get_queue(null)`, `msg_get_queue(null,
/// null)` and `msg_queue_exists(null)` each make a private queue of mode 0
/// that nobody sends to, naming no process, three per php that runs it,
/// which filled the machine's 40 on #2808. A test of that name here makes
/// the three and writes their ids; once the subset's run ends none of them
/// stands, as `ipcs -q` reads them. It reads queues of mode 0 alone, so a
/// private queue of a mode another session's run made beside it is never
/// read as the test's, nor removed by this test (#3007).
#[test]
fn the_private_queues_the_zend_arginfo_test_leaves_are_removed_after_its_run() {
    if oracle().is_none() {
        return;
    }
    let _alone = alone();
    if let Some(why) = ipcs_readable() {
        eprintln!("skipped: {why}");
        return;
    }
    let tree = OneTree::new("sysv-arginfo");
    let dir = tree.corpus.join("Zend/tests");
    fs::create_dir_all(&dir).unwrap();
    let marks = Tree::new("phpt-sysv-arginfo-marks").unwrap();
    let made = marks.join("made");
    fs::write(
        dir.join("arginfo_zpp_mismatch.phpt"),
        format!(
            "--TEST--\nmakes three private queues\n--FILE--\n<?php\n$private = fn() => preg_grep('/^q\\s+\\d+\\s+0x00000000\\s+\\S\\S-{{9}}\\s/', explode(\"\\n\", shell_exec('ipcs -q')));\n$stood = $private();\nforeach ([fn() => msg_get_queue(null), fn() => msg_get_queue(null, null), fn() => msg_queue_exists(null)] as $f) {{ try {{ @$f(); }} catch (Throwable) {{}} }}\n$new = array_map(fn($line) => preg_split('/\\s+/', $line)[1], array_diff($private(), $stood));\nfile_put_contents('{}', implode(' ', $new) . ' ', FILE_APPEND);\necho \"===DONE===\";\n--EXPECT--\n===DONE===\n",
            made.display()
        ),
    )
    .unwrap();
    let checkout = OneTree::checkout("sysv-arginfo");
    let out = tree.command(&checkout, "Zend/tests").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stdout);
    let queues: Vec<String> = fs::read_to_string(&made)
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    assert!(
        queues.len() >= 3,
        "the test made its three queues: {queues:?} {said}"
    );
    let standing: Vec<String> = ipcs()
        .into_iter()
        .filter(|o| o.kind == "q" && queues.contains(&o.id))
        .map(|o| o.id)
        .collect();
    for queue in &standing {
        Command::new("ipcrm").args(["-q", queue]).status().ok();
    }
    if !standing.is_empty() && said.contains(STOOD) {
        eprintln!("skipped: a claim held on the machine kept the run from {standing:?}: {said}");
        return;
    }
    assert!(
        standing.is_empty(),
        "the run left {standing:?} of {queues:?}: {said}"
    );
}

/// On Linux, a run where no private IPC namespace is in play still removes
/// what its test leaves (#1643): the fixture's subset is no System V
/// subset, so its program runs in the machine's own namespace, as every
/// subset's does where `unshare` is refused, and the keyed queue its test
/// sends to and leaves, naming the test's process, is gone from
/// `/proc/sysvipc/msg` after the run. Before #1643 the sweep read
/// `ipcs -a` by a `T` header util-linux never prints, and the queue stood.
/// It returns elsewhere, where there is no `/proc/sysvipc`, and where the
/// oracle has no sysvmsg.
#[cfg(target_os = "linux")]
#[test]
fn on_linux_a_run_outside_a_namespace_removes_the_queue_its_test_leaves() {
    if oracle().is_none() {
        return;
    }
    let keys = || -> Vec<String> {
        fs::read_to_string("/proc/sysvipc/msg")
            .unwrap_or_default()
            .lines()
            .skip(1)
            .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
            .collect()
    };
    if fs::metadata("/proc/sysvipc/msg").is_err() {
        eprintln!("skipped: no /proc/sysvipc here");
        return;
    }
    let marks = Tree::new("phpt-sysv-linux").unwrap();
    let pid = marks.join("pid");
    let leaves = format!(
        "--TEST--\nleaves a queue\n--SKIPIF--\n<?php if (!function_exists('msg_get_queue')) die('skip no sysvmsg');\n--FILE--\n<?php\n$q = msg_get_queue(0x5c000000 + getmypid(), 0600);\nmsg_send($q, 1, \"x\");\nfile_put_contents('{}', getmypid());\necho \"left\";\n--EXPECT--\nleft\n",
        pid.display()
    );
    let scratch = Scratch::new("sysv-linux", &[("leaves.phpt", &leaves)]);
    let reading = scratch.run(&registry(""), &scratch.engine);
    let Ok(pid) = fs::read_to_string(&pid) else {
        eprintln!("skipped: the oracle has no sysvmsg: {}", reading.message);
        return;
    };
    let key = (0x5c00_0000 + pid.trim().parse::<i64>().unwrap()).to_string();
    let stood = keys().contains(&key);
    if stood {
        let _ = Command::new("ipcrm")
            .args(["-Q", &format!("0x{:08x}", key.parse::<i64>().unwrap())])
            .status();
    }
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert!(!stood, "the run left the queue of key {key}: {reading:#?}");
    assert!(count(&reading, "sysv_removed") >= 1, "{reading:#?}");
}

/// Two runs on one machine keep apart on what upstream's conflict keys
/// name (#1313): a subset whose `CONFLICTS` names `all`, as
/// `sapi/fpm/tests` does for `FPM\Tester`'s ports, whose every test binds
/// one fixed port for a while, run twice at once from two workspaces, and
/// each run passes every test; the waits are on a reading. The control: a
/// run that holds its keys within itself alone binds the port while the
/// other run's test holds it, and that test misses.
///
/// The port is found free and let go before the tests bind it, so another
/// process on the machine can take it first (#2773). Then every test of
/// both runs misses, which keys held apart never cause, since one test
/// of each run binds the port; the exchange is run again on a fresh port,
/// three at most.
#[test]
fn two_runs_at_once_over_a_fixed_port_each_read_what_a_lone_run_reads() {
    if oracle().is_none() {
        return;
    }
    let free = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let readings = std::iter::repeat_with(free)
        .take(3)
        .find_map(|port| {
            let readings = over_a_fixed_port(port);
            let bound =
                |r: &Reading| !matches!(r.numbers.get("passed"), None | Some(Number::Int(0)));
            readings.iter().any(bound).then_some(readings)
        })
        .expect("another process took each of three ports before the tests bound it");
    for reading in &readings {
        assert_eq!(reading.status, Status::Pass, "{reading:#?}");
        assert_eq!(count(reading, "passed"), 3, "{reading:#?}");
    }
    let waited: i64 = readings
        .iter()
        .map(|r| match r.numbers.get("waited") {
            Some(Number::Int(n)) => *n,
            _ => 0,
        })
        .sum();
    assert!(waited > 0, "one run waited for the other: {readings:#?}");
    assert!(
        readings.iter().any(|r| {
            let (said, stamp) = r.message.rsplit_once("; ").unwrap();
            said.contains("waited") && said.contains("conflict key") && stamp.starts_with("run by ")
        }),
        "the wait is said before the engine's stamp, which stays last: {readings:#?}"
    );
}

/// The two runs of the test above, at once, over `port`.
fn over_a_fixed_port(port: u16) -> [Reading; 2] {
    let binds = |name: &str| {
        (
            format!("{name}.phpt"),
            format!(
                "--TEST--\n{name}\n--FILE--\n<?php\n$s = @stream_socket_server('tcp://127.0.0.1:{port}', $errno, $errstr);\nif (!$s) {{ echo \"held elsewhere: $errstr\\n\"; exit; }}\nusleep(300000);\necho \"bound\\n\";\n--EXPECT--\nbound\n"
            ),
        )
    };
    let tests: Vec<(String, String)> = ["one", "two", "three"].map(binds).into();
    let mut files: Vec<(&str, &str)> = tests
        .iter()
        .map(|(f, t)| (f.as_str(), t.as_str()))
        .collect();
    files.push(("CONFLICTS", "all\n"));
    let (first, second) = (
        Scratch::new("port-one", &files),
        Scratch::new("port-two", &files),
    );
    let locks = Tree::new("phpt-across-locks").unwrap();
    let locks = locks.path();
    std::thread::scope(|scope| {
        let runs = [&first, &second].map(|scratch| {
            scope.spawn(move || scratch.run_among(&registry(""), &scratch.engine, locks))
        });
        runs.map(|run| run.join().unwrap())
    })
}

/// A test naming `all` in one run runs beside no test of another key in a
/// second run (#1559), as upstream's `all` runs beside nothing
/// (run-tests.php:1365-1370): one run's subset names `all` and its one
/// test sleeps a while, the other's names a key of its own and its three
/// tests sleep a while each, both run at once from two workspaces, and
/// every test writes when it started and ended. No keyed test's span
/// meets the `all` test's. Before #1559 `all` was a lock named `all`
/// beside the other key's, and the spans met.
#[test]
fn a_test_naming_all_runs_beside_no_other_runs_keyed_test() {
    fn borrowed(files: &[(String, String)]) -> Vec<(&str, &str)> {
        files
            .iter()
            .map(|(f, t)| (f.as_str(), t.as_str()))
            .collect()
    }
    if oracle().is_none() {
        return;
    }
    let marks = Tree::new("phpt-all-across").unwrap();
    let log = marks.join("spans");
    let spans = |name: &str, micros: u32| {
        (
            format!("{name}.phpt"),
            format!(
                "--TEST--\n{name}\n--FILE--\n<?php\n$from = microtime(true);\nusleep({micros});\nfile_put_contents('{}', sprintf(\"{name} %.6f %.6f\\n\", $from, microtime(true)), FILE_APPEND | LOCK_EX);\necho \"slept\\n\";\n--EXPECT--\nslept\n",
                log.display()
            ),
        )
    };
    let alone = [spans("alone", 1_500_000)];
    let keyed = ["one", "two", "three"].map(|name| spans(name, 400_000));
    let files = |tests: &[(String, String)], conflicts: &str| -> Vec<(String, String)> {
        let mut files = tests.to_vec();
        files.push(("CONFLICTS".to_owned(), conflicts.to_owned()));
        files
    };
    let key = format!("keyed-{}\n", std::process::id());
    let (alone, keyed) = (files(&alone, "all\n"), files(&keyed, &key));
    let (first, second) = (
        Scratch::new("all-alone", &borrowed(&alone)),
        Scratch::new("all-keyed", &borrowed(&keyed)),
    );
    let locks = Tree::new("phpt-across-locks").unwrap();
    let locks = locks.path();
    let readings = std::thread::scope(|scope| {
        let runs = [&first, &second].map(|scratch| {
            scope.spawn(move || scratch.run_among(&registry(""), &scratch.engine, locks))
        });
        runs.map(|run| run.join().unwrap())
    });
    for reading in &readings {
        assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    }
    let written = fs::read_to_string(&log).unwrap();
    let spans: Vec<(&str, f64, f64)> = written
        .lines()
        .map(|line| {
            let mut words = line.split(' ');
            let name = words.next().unwrap();
            let mut at = || words.next().unwrap().parse::<f64>().unwrap();
            (name, at(), at())
        })
        .collect();
    assert_eq!(spans.len(), 4, "{written}");
    let (_, from, to) = *spans.iter().find(|(name, ..)| *name == "alone").unwrap();
    for (name, f, t) in spans.iter().filter(|(name, ..)| *name != "alone") {
        assert!(
            *t <= from || *f >= to,
            "{name} ran {f}..{t}, beside alone's {from}..{to}: {written}"
        );
    }
}

/// `--against <commit>` (#664): the run a checkout of the merge base kept
/// is found by the commit alone, and a test met there that fails here is
/// named by a reading that fails, while the subset's own reading is
/// written beside it. The controls: the head against itself passes, a
/// binary the tree has moved past gives no verdict, and so does a commit
/// with no run kept, whose subset still runs and keeps its own reading
/// (#1435): a runner that runs nothing there fails this.
#[test]
fn a_test_met_at_the_merge_base_and_failing_here_fails_the_comparison() {
    if oracle().is_none() {
        return;
    }
    let echo =
        |name: &str| format!("--TEST--\n{name}\n--FILE--\n<?php echo \"one\";\n--EXPECT--\none\n");
    let scratch = Scratch::new(
        "against",
        &[
            ("kept.phpt", &echo("kept")),
            ("planted.phpt", &echo("planted")),
        ],
    );
    let run = |engine: &Path, against: Option<&str>| {
        let ask = Ask {
            subset: Some("ext/fixture".to_owned()),
            engine: phpt::Engine::Named(engine.to_path_buf()),
            against: against.map(str::to_owned),
            ..Ask::default()
        };
        phpt::run(&scratch.root, &scratch.manifest, &registry(""), &ask).unwrap()
    };

    // The merge base: both tests pass, and the run is kept.
    let base = commit_all(&scratch.root, "base");
    let readings = run(&scratch.engine, None);
    assert_eq!(readings.len(), 1, "{readings:#?}");
    assert_eq!(readings[0].status, Status::Pass, "{readings:#?}");

    // The head: the engine's sources moved and planted.phpt fails.
    fs::create_dir_all(scratch.root.join("ext/fixture/src")).unwrap();
    fs::write(scratch.root.join("ext/fixture/src/lib.rs"), "// head\n").unwrap();
    let head = commit_all(&scratch.root, "head");
    let planting = planting_engine(&scratch);
    written_at(&planting, SystemTime::now() + Duration::from_mins(1));
    let readings = run(&planting, Some(&base));
    let scopes: Vec<&str> = readings.iter().map(|r| r.scope.as_str()).collect();
    assert_eq!(scopes, ["ext/fixture", "ext/fixture@against"]);
    let against = &readings[1];
    assert_eq!(against.status, Status::Fail, "{against:#?}");
    assert!(
        against.message.contains("ext/fixture/tests/planted.phpt"),
        "{}",
        against.message
    );
    assert!(
        !against.message.contains("kept.phpt"),
        "{}",
        against.message
    );
    assert_eq!(count(against, "regressed"), 1);
    assert_eq!(count(against, "held"), 2);

    // The head against its own kept run: nothing met there fails here.
    let readings = run(&planting, Some(&head));
    assert_eq!(readings[1].status, Status::Pass, "{readings:#?}");

    // A binary the tree has moved past ran nothing: no verdict.
    written_at(&planting, SystemTime::now() - Duration::from_hours(1));
    let readings = run(&planting, Some(&base));
    assert_eq!(readings[1].status, Status::Skip, "{readings:#?}");
    assert!(
        readings[1].message.contains("measured no engine"),
        "{}",
        readings[1].message
    );

    // No run kept at the commit: no verdict, and the subset runs anyway,
    // its own reading kept beside the skip, which carries its counts.
    fs::remove_dir_all(scratch.root.join("target/harness/phpt-runs")).unwrap();
    written_at(&scratch.engine, SystemTime::now() + Duration::from_mins(1));
    let readings = run(&scratch.engine, Some(&base));
    let scopes: Vec<&str> = readings.iter().map(|r| r.scope.as_str()).collect();
    assert_eq!(scopes, ["ext/fixture", "ext/fixture@against"]);
    assert_eq!(readings[0].status, Status::Pass, "{readings:#?}");
    assert_eq!(count(&readings[0], "run"), 2, "{readings:#?}");
    assert_eq!(readings[1].status, Status::Skip, "{readings:#?}");
    for words in [
        "no run of ext/fixture is kept",
        "the subset ran, and its own reading stands beside this one: ext/fixture: 2 passed",
    ] {
        assert!(
            readings[1].message.contains(words),
            "{words:?} is not in {}",
            readings[1].message
        );
    }
}

/// A subset run on a pool of four writes the reading and keeps the run a
/// pool of one does, word for word, whatever order its tests finished in
/// (#938): the status, every number and the message, and the kept run's
/// bytes, which `--against` reads.
#[test]
fn a_pooled_run_writes_the_reading_and_keeps_the_run_a_run_of_one_does() {
    if oracle().is_none() {
        return;
    }
    let slow = |n: u8, said: &str| {
        format!(
            "--TEST--\nn{n}\n--FILE--\n<?php usleep({}); echo {n};\n--EXPECT--\n{said}\n",
            u32::from(9 - n) * 30_000
        )
    };
    let tests: Vec<(String, String)> = (1..=8)
        .map(|n| {
            let said = if n % 3 == 0 { "wrong".to_owned() } else { n.to_string() };
            (format!("t{n}.phpt"), slow(n, &said))
        })
        .chain([(
            "t9.phpt".to_owned(),
            "--TEST--\nskipped\n--SKIPIF--\n<?php echo 'skip here';\n--FILE--\n<?php\n--EXPECT--\n\n".to_owned(),
        )])
        .collect();
    let tests: Vec<(&str, &str)> = tests
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let scratch = Scratch::new("pooled", &tests);
    commit_all(&scratch.root, "pooled");
    written_at(&scratch.engine, SystemTime::now() + Duration::from_mins(1));
    let kept = |jobs: usize| {
        let ask = Ask {
            subset: Some("ext/fixture".to_owned()),
            engine: phpt::Engine::Named(scratch.engine.clone()),
            jobs: std::num::NonZeroUsize::new(jobs),
            ..Ask::default()
        };
        let readings = phpt::run(&scratch.root, &scratch.manifest, &registry(""), &ask).unwrap();
        let mut runs = Vec::new();
        files_under(&scratch.root.join("target/harness/phpt-runs"), &mut runs);
        assert_eq!(runs.len(), 1, "{runs:?}");
        let run = fs::read(&runs[0]).unwrap();
        let reading = readings.into_iter().next().unwrap();
        (reading.status, reading.numbers, reading.message, run)
    };
    let one = kept(1);
    assert_eq!(one.0, Status::Fail, "{one:?}");
    assert_eq!(kept(4), one);
}

/// Every file under `dir`.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// One tree standing where the pinned php-src would, a stand-in engine
/// over the pinned oracle, and the checkouts that run `xtask phpt` over
/// the tree as processes of their own, as two builders' lanes do: what the
/// tests of runs at once share (#990, #1097), each run in a copy of the
/// tree of its own (#1183).
struct OneTree {
    corpus: Tree,
    engine: PathBuf,
    bin: Tree,
}

impl OneTree {
    fn new(name: &str) -> OneTree {
        let corpus = Tree::new(&format!("phpt-{name}-corpus")).unwrap();
        let bin = Tree::new(&format!("phpt-{name}-engine")).unwrap();
        let engine = bin.join("php");
        fs::write(
            &engine,
            format!(
                "#!/bin/sh\nexec \"{}\" \"$@\"\n",
                oracle().unwrap().display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
        }
        written_at(&engine, SystemTime::now() + Duration::from_mins(1));
        OneTree {
            corpus,
            engine,
            bin,
        }
    }

    /// Where the runs over the tree lay their System V claims apart
    /// ([`CLAIMS`]), so a test of what a sweep takes never waits on them.
    fn claims(&self) -> PathBuf {
        self.bin.join("xtask-sysv-claims")
    }

    /// A checkout of its own, committed, as a builder's worktree is.
    fn checkout(name: &str) -> Tree {
        let root = Tree::new(&format!("phpt-apart-{name}")).unwrap();
        scratch_workspace(workspace(), &root);
        commit_all(&root, name);
        root
    }

    /// `xtask phpt` over `subset` of the tree from the checkout at `root`,
    /// a process of its own on a pool of two.
    fn run(&self, root: &Tree, subset: &str) -> std::process::Child {
        self.command(root, subset).spawn().unwrap()
    }

    /// The command [`OneTree::run`] spawns, for a run that needs one more
    /// variable set.
    fn command(&self, root: &Tree, subset: &str) -> Command {
        let mut command = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")));
        command
            .arg("--root")
            .arg(root.path())
            .args(["phpt", "--subset", subset, "--jobs", "2", "--engine"])
            .arg(&self.engine)
            .env(xtask::oracle::TREE_VARIABLE, self.corpus.path())
            .env(CLAIMS, self.claims())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command
    }
}

/// The run the checkout at `root` kept once `child` is done, with its
/// commit taken out, the one field two checkouts of one tree tell apart.
fn kept(root: &Tree, child: std::process::Child) -> serde_json::Value {
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let mut runs = Vec::new();
    files_under(&root.join("target/harness/phpt-runs"), &mut runs);
    assert_eq!(runs.len(), 1, "{runs:?}");
    let mut run: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&runs[0]).unwrap()).unwrap();
    run.as_object_mut().unwrap().remove("commit").unwrap();
    run
}

/// Two runs over one subset at once, from two checkouts, each a process of
/// its own over the one tree the variable names (#990): each reads every
/// test as a lone run does, and keeps the lone run's bytes. Each test
/// writes a file named for its `__FILE__` beside itself, sleeps, and reads
/// it back, so a run whose test stood beside the other's same test would
/// read `theirs`, or bork on the other's program still standing; and each
/// prints `__FILE__` and `__DIR__` by name and a sibling included by
/// `__DIR__` and by the include path, which are run-tests.php's. The lone
/// run first is the control the two are held to, and the tree holds only
/// what it held before any of them, each run being in a copy (#1183).
#[test]
fn two_runs_over_one_subset_at_once_each_read_as_a_lone_run_does() {
    if oracle().is_none() {
        return;
    }
    let tree = OneTree::new("apart");
    let dir = tree.corpus.join("Zend/tests");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("sibling.inc"), "<?php return 'sibling';\n").unwrap();
    for n in 1..=4 {
        fs::write(
            dir.join(format!("t{n}.phpt")),
            format!(
                "--TEST--\nt{n}\n--FILE--\n<?php\n$mark = __FILE__ . '.mark';\n\
                 file_put_contents($mark, getmypid());\nusleep(400000);\n\
                 $by_dir = include __DIR__ . '/sibling.inc';\n$by_path = include 'sibling.inc';\n\
                 $whose = file_get_contents($mark) == getmypid() ? 'mine' : 'theirs';\n\
                 unlink($mark);\n\
                 echo basename(__FILE__), ' ', basename(__DIR__), ' ', $by_dir, ' ', $by_path, ' ', $whose, \"\\n\";\n\
                 --EXPECT--\nt{n}.php tests sibling sibling mine\n"
            ),
        )
        .unwrap();
    }
    let lone = OneTree::checkout("lone");
    let alone = kept(&lone, tree.run(&lone, "Zend/tests"));
    let verdicts = alone["verdicts"].as_object().unwrap();
    assert!(
        verdicts.len() == 4 && verdicts.values().all(|v| v == "pass"),
        "the lone run passes all four: {alone}"
    );
    let (one, two) = (OneTree::checkout("one"), OneTree::checkout("two"));
    let (first, second) = (tree.run(&one, "Zend/tests"), tree.run(&two, "Zend/tests"));
    assert_eq!(kept(&one, first), alone);
    assert_eq!(kept(&two, second), alone);
    let mut left = Vec::new();
    files_under(&dir, &mut left);
    left.sort();
    assert_eq!(
        left,
        [
            dir.join("sibling.inc"),
            dir.join("t1.phpt"),
            dir.join("t2.phpt"),
            dir.join("t3.phpt"),
            dir.join("t4.phpt")
        ],
        "nothing is left beside the tests"
    );
}

/// Two runs over one directory at once, from two checkouts, each a process
/// of its own over one tree (#1097). The directory's two tests each make
/// and remove `relative_temp` relative to the tree's root, where every
/// program runs, as ext/zlib's `readgzfile_basic.phpt` and
/// `readgzfile_basic2.phpt` make `readgzfile_temp` (#1091): a run judges the
/// two in one batch, one after the other, and the one run's first met the
/// other's second on `mkdir(): File exists` while both ran in the tree
/// itself. Each run's root is its own copy's (#1183), so both keep the
/// lone run's verdicts, on twenty runs of twenty. The lone run first is
/// the control they are held to.
#[test]
fn two_runs_over_one_directory_at_once_never_meet_in_it() {
    if oracle().is_none() {
        return;
    }
    let tree = OneTree::new("directory");
    let dir = tree.corpus.join("Zend/tests");
    fs::create_dir_all(&dir).unwrap();
    for name in ["a-temp", "b-temp"] {
        fs::write(
            dir.join(format!("{name}.phpt")),
            format!(
                "--TEST--\n{name}\n--FILE--\n<?php\nvar_dump(mkdir('relative_temp'));\n\
                 usleep(200000);\nvar_dump(rmdir('relative_temp'));\n\
                 --EXPECT--\nbool(true)\nbool(true)\n"
            ),
        )
        .unwrap();
    }
    let lone = OneTree::checkout("directory-lone");
    let alone = kept(&lone, tree.run(&lone, "Zend/tests"));
    let verdicts = alone["verdicts"].as_object().unwrap();
    assert!(
        verdicts.len() == 2 && verdicts.values().all(|v| v == "pass"),
        "the lone run passes both: {alone}"
    );
    let (one, two) = (
        OneTree::checkout("directory-one"),
        OneTree::checkout("directory-two"),
    );
    for run in 0..20 {
        let (first, second) = (tree.run(&one, "Zend/tests"), tree.run(&two, "Zend/tests"));
        assert_eq!(kept(&one, first), alone, "run {run}, the first");
        assert_eq!(kept(&two, second), alone, "run {run}, the second");
    }
}

/// A run killed mid-subset leaves the pinned tree as it stood (#1183):
/// its one test, standing in the run's copy, writes beside itself and
/// sleeps until the run is killed, and the tree holds only what it held
/// before. The copy it leaves is a dead run's, swept by the next run: that
/// run passes, and the dead one's copy is gone.
///
/// The copy stands under `/tmp` whatever `TMPDIR` says (#2626), where the
/// first scratch tree any process on the machine lays there sweeps a dead
/// run's (#1473): another session's instrument, a test binary starting.
/// So whether the copy still stands between the kill and the next run is
/// the machine's, not the test's; another process's run goes between them,
/// so the sweep in the window is made, not met by chance, and the copy is
/// gone after it whoever swept it. The test's php, sleeping past the
/// recipe's limit, outlives the killed run, and that other run, in a
/// `TMPDIR` of its own, stops it as it starts where no other process's
/// run has stopped it first: the dead copy's lock came, and in the one
/// directory every session shares the lock is the only guard (#2663); a
/// copy whose lock a live run holds is left alone, which `scratch`'s own
/// test holds. A php still running after the next run is killed by this
/// test, which fails.
#[test]
fn a_run_killed_mid_subset_leaves_the_pinned_tree_as_it_stood() {
    use std::io::{BufRead, BufReader};
    if oracle().is_none() {
        return;
    }
    let tree = OneTree::new("killed");
    let dir = tree.corpus.join("Zend/tests");
    fs::create_dir_all(&dir).unwrap();
    let signal = Tree::new("phpt-killed-signal").unwrap();
    let held = signal.join("held");
    let temporary = Tree::new("phpt-killed-tmp").unwrap();
    fs::write(
        dir.join("a-held.phpt"),
        format!(
            "--TEST--\nheld\n--FILE--\n<?php\nfile_put_contents(__DIR__ . '/beside', 1);\n\
             file_put_contents({:?}, getmypid());\nsleep(120);\n--EXPECT--\n",
            held.display().to_string()
        ),
    )
    .unwrap();
    let before = listing(tree.corpus.path());
    let one = OneTree::checkout("killed-one");
    let mut first = tree
        .command(&one, "Zend/tests")
        .env("TMPDIR", temporary.path())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(first.stdout.take().unwrap()).lines();
    let copy = loop {
        let line = lines.next().expect("the run named no copy").unwrap();
        if let Some((_, copy)) = line.split_once(" files, is ") {
            break PathBuf::from(copy);
        }
    };
    // Read while the run lives and holds its copy's lock, which no sweep
    // passes: once it is killed, any process's run may stop its php (#2942).
    // The file stands empty between php's open and its write, which a read
    // under load met (#3007), so the pid is what is waited for.
    let pid = printed(&held);
    assert!(!pid.is_empty(), "the killed run's test never started");
    assert!(
        copy.join("Zend/tests/beside").exists(),
        "the test ran in {copy:?}"
    );
    let pid = pid.trim();
    let php = started_at(pid);
    assert!(!php.is_empty(), "the run's php runs");
    first.kill().unwrap();
    first.wait().unwrap();

    // Another process's run, in the machine's temporary directory, whose
    // first scratch tree sweeps every dead run's standing there.
    let other = OneTree::new("killed-other");
    fs::create_dir_all(other.corpus.join("Zend/tests")).unwrap();
    fs::write(
        other.corpus.join("Zend/tests/quick.phpt"),
        "--TEST--\nquick\n--FILE--\n<?php\necho \"ok\\n\";\n--EXPECT--\nok\n",
    )
    .unwrap();
    let swept = other.run(&one, "Zend/tests").wait_with_output().unwrap();
    // A run in another temporary directory stops the killed one's php,
    // if no other process's has: its copy stood in /tmp with its lock let go.
    let stopped_by_other = started_at(pid) != php;
    assert!(swept.status.success(), "{swept:?}");

    assert_eq!(listing(tree.corpus.path()), before, "the pinned tree moved");
    assert!(!copy.exists(), "the sweep in the window left {copy:?}");

    fs::remove_file(dir.join("a-held.phpt")).unwrap();
    fs::write(
        dir.join("b-quick.phpt"),
        "--TEST--\nquick\n--FILE--\n<?php\necho \"ok\\n\";\n--EXPECT--\nok\n",
    )
    .unwrap();
    let two = OneTree::checkout("killed-two");
    let second = tree
        .command(&two, "Zend/tests")
        .env("TMPDIR", temporary.path())
        .spawn()
        .unwrap();
    let run = kept(&two, second);
    // Killed by this test only where it is still the same process.
    let orphan = started_at(pid) == php;
    if orphan {
        Command::new("kill").args(["-9", pid]).status().unwrap();
    }
    assert!(
        stopped_by_other,
        "a run in another temporary directory left the dead run's php {pid}"
    );
    assert!(
        !orphan,
        "the next run left the killed run's php {pid} running"
    );
    assert_eq!(
        run["verdicts"]
            .as_object()
            .unwrap()
            .values()
            .collect::<Vec<_>>(),
        ["pass"],
        "{run}"
    );
}

/// A process a test started that leaves its program's group, as a
/// php-fpm master a test starts with `$daemonize = true` calls `setsid()`
/// (#2663), is stopped as its subset ends: the test's `perl` forks, its
/// parent exits, and the child takes a session of its own, writes its pid
/// and sleeps past the recipe's limit with the test's directory, in the
/// run's copy of the pinned tree, on its command line, as the fpm master's
/// title names its ini there. The run ends, and that process is gone; one
/// still running is killed by this test, which fails.
#[test]
fn a_process_a_test_started_that_left_its_group_is_stopped_as_the_run_ends() {
    if oracle().is_none() {
        return;
    }
    let tree = OneTree::new("left-group");
    let dir = tree.corpus.join("Zend/tests");
    fs::create_dir_all(&dir).unwrap();
    let marks = Tree::new("phpt-left-group-marks").unwrap();
    let started = marks.join("started");
    // The parent waits for the child's mark, so the child has its own
    // session before the program ends and its group is killed.
    let test = r"--TEST--
leaves its group
--FILE--
<?php
$perl = <<<'PERL'
my $m = $ARGV[1];
if (fork) { select(undef, undef, undef, 0.05) until -s $m; exit }
POSIX::setsid();
open my $f, '>', $m; print $f $$; close $f;
sleep 120;
PERL;
exec('perl -MPOSIX -e ' . escapeshellarg($perl) . ' ' . escapeshellarg(__DIR__) . ' MARK > /dev/null 2>&1');
echo 'left';
--EXPECT--
left
";
    let test = test.replace("MARK", &started.display().to_string());
    fs::write(dir.join("leaves.phpt"), test).unwrap();
    let checkout = OneTree::checkout("left-group");
    let out = tree.command(&checkout, "Zend/tests").output().unwrap();
    let pid = printed(&started);
    assert!(!pid.is_empty(), "the test's process never started: {out:?}");
    let left = !started_at(&pid).is_empty();
    if left {
        Command::new("kill").args(["-9", &pid]).status().unwrap();
    }
    assert!(
        !left,
        "the run left {pid}, which left its group, running: {out:?}"
    );
    assert!(out.status.success(), "{out:?}");
}

/// Every path under `dir` with its bytes, a directory's as none, sorted:
/// what a tree holds, to hold it to what it held before a run.
fn listing(dir: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.push((path.clone(), None));
                walk(&path, out);
            } else {
                out.push((path.clone(), Some(fs::read(&path).unwrap())));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

/// The fixture at `xtask/fixtures/phpt/writes` (#1183), run over a pinned
/// tree that is a checkout of its own with the fixture committed and an
/// untracked `stray.php` standing where `stray.phpt`'s program goes, as a
/// killed run's program stood in the pinned tree. Every test passes, the
/// stray borking none since the run's copy holds what git tracks, and the
/// tree holds, path for path and byte for byte, what it held before. A
/// runner that writes into the pinned tree fails here: the fixture's tests
/// leave six paths behind them there.
#[test]
fn a_run_writes_nothing_into_the_pinned_tree() {
    if oracle().is_none() {
        return;
    }
    let fixture = xtask::package_dir().join("fixtures/phpt/writes");
    let tests: Vec<(String, String)> = fs::read_dir(&fixture)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "phpt"))
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name, fs::read_to_string(&path).unwrap())
        })
        .collect();
    assert_eq!(tests.len(), 4, "{tests:?}");
    let tests: Vec<(&str, &str)> = tests
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let mut scratch = Scratch::new("writes", &tests);
    let corpus = scratch.root.join("corpus");
    Repo::init(&corpus);
    scratch.manifest.pin.as_mut().unwrap().source.commit = commit_all(&corpus, "the pin");
    fs::write(
        corpus.join("ext/fixture/tests/stray.php"),
        "<?php echo 'a stray';\n",
    )
    .unwrap();
    let before = listing(&corpus);
    let reading = scratch.run(&registry(""), &scratch.engine);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "passed"), 4, "{reading:#?}");
    assert_eq!(count(&reading, "borked"), 0, "{reading:#?}");
    assert_eq!(listing(&corpus), before, "the pinned tree moved");
}

/// The tests of a fixture directory, by file name and text.
fn fixture_tests(dir: &str) -> Vec<(String, String)> {
    let fixture = xtask::package_dir().join(dir);
    let mut tests: Vec<(String, String)> = fs::read_dir(&fixture)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "phpt"))
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name, fs::read_to_string(&path).unwrap())
        })
        .collect();
    tests.sort();
    tests
}

/// The fixture at `xtask/fixtures/phpt/environment` (#1186): a test's
/// programs get run-tests.php's environment and pipes for their streams,
/// so under the pinned php every one of its tests passes, the `SKIPIF`
/// that skips on anything but a pipe included. Run again through a binary
/// that hands its program `/dev/null` on stdin and a file on stdout, what
/// the runner handed before, the three tests of the streams fail and are
/// named: the fixture's negative control.
#[test]
fn a_test_gets_run_tests_environment_and_pipes_for_its_streams() {
    if oracle().is_none() {
        return;
    }
    let tests = fixture_tests("fixtures/phpt/environment");
    assert_eq!(tests.len(), 5, "{tests:?}");
    let tests: Vec<(&str, &str)> = tests
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let scratch = Scratch::new("environment", &tests);
    let reading = scratch.run(&registry(""), &scratch.engine);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "passed"), 5, "{reading:#?}");

    let nulled = scratch.root.join("engine/nulled");
    fs::write(
        &nulled,
        format!(
            "#!/bin/sh\nout=$(mktemp)\n\"{}\" \"$@\" </dev/null >\"$out\" 2>&1\ncat \"$out\"\nrm -f \"$out\"\n",
            oracle().unwrap().display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&nulled, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let reading = scratch.run(&registry(""), &nulled);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "failed"), 3, "{reading:#?}");
    for name in ["stdin-pipe.phpt", "stdin-section.phpt", "stdout-pipe.phpt"] {
        assert!(reading.message.contains(name), "{}", reading.message);
    }
}

/// The target (#1184): the runner's control keeps the oracle's verdict on
/// every test under the pin's key, and an engine's reading of the subset
/// then carries met-of-target. The oracle misses `oracle-miss.phpt`, whose
/// expectation no php meets, and skips `oracle-skip.phpt`, so both are
/// outside the target, each named with why the oracle left it out (#2033),
/// and the engine's fail on the miss is counted there and not against the
/// target; the engine meets the one test the oracle meets. Before the
/// control has run, the engine's reading says how to take a target and
/// carries no such number.
#[test]
fn the_control_keeps_the_target_and_the_engines_reading_is_met_of_target() {
    let Some(binary) = oracle() else { return };
    let scratch = Scratch::new(
        "target",
        &[
            (
                "met.phpt",
                "--TEST--\nEcho\n--FILE--\n<?php echo \"one\";\n--EXPECT--\none\n",
            ),
            (
                "oracle-miss.phpt",
                "--TEST--\nEcho\n--FILE--\n<?php echo \"one\";\n--EXPECT--\ntwo\n",
            ),
            (
                "oracle-skip.phpt",
                "--TEST--\nSkip\n--SKIPIF--\n<?php die(\"skip For Linux only\"); ?>\n--FILE--\n<?php echo \"one\";\n--EXPECT--\none\n",
            ),
        ],
    );
    let reading = scratch.run(&registry(""), &scratch.engine);
    assert!(!reading.numbers.contains_key("target"), "{reading:#?}");
    assert!(
        reading.message.contains("no target is kept under pin-"),
        "{}",
        reading.message
    );

    let control = scratch.run(&registry(""), &binary);
    assert!(
        control
            .message
            .contains("kept as the subset's target under pin-"),
        "{}",
        control.message
    );
    let pin = scratch.manifest.pin().unwrap();
    let kept = scratch.root.join(format!(
        "target/harness/phpt-runs/pin-{}-{}/ext/fixture.json",
        pin.source.tag, pin.oracle.md5
    ));
    let kept: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&kept).unwrap()).unwrap();
    assert_eq!(
        kept["verdicts"]["phpt:php-src/ext/fixture/tests/oracle-miss.phpt"],
        "fail"
    );
    assert_eq!(
        kept["verdicts"]["phpt:php-src/ext/fixture/tests/met.phpt"],
        "pass"
    );
    assert_eq!(
        kept["skips"]["phpt:php-src/ext/fixture/tests/oracle-skip.phpt"],
        "For Linux only"
    );

    let reading = scratch.run(&registry(""), &scratch.engine);
    assert_eq!(count(&reading, "target"), 1, "{reading:#?}");
    assert_eq!(count(&reading, "met"), 1, "{reading:#?}");
    assert_eq!(count(&reading, "outside"), 2, "{reading:#?}");
    assert!(
        reading.message.contains(
            "1 of 1 target tests met (ext/fixture/tests/met.phpt), 2 outside (missed by the \
             oracle, output differs: ext/fixture/tests/oracle-miss.phpt; skipped by the oracle \
             \"For Linux only\": ext/fixture/tests/oracle-skip.phpt)"
        ),
        "{}",
        reading.message
    );
}

/// The tests whose subject is the tracked workspace itself: each reads the
/// `seams.toml`, `oracle.toml`, registry or sources standing in the
/// workspace, so a person's edit there is what may turn one red (#3091).
/// Every other test here judges by a manifest it builds, or by a
/// scratch's byte-for-byte copy of the tracked one where the binary's
/// hook or `permit` refuses any other (`built.rs`).
mod tracked {
    use super::*;

    #[test]
    fn the_full_corpus_is_a_flag_the_manifest_denies_at_the_tool_call() {
        // The rule's fixture is the negative control; this asserts the flag the
        // rule names, among the instrument's argv (#1860), is the flag the
        // runner has.
        let ask = Ask {
            full: true,
            ..Ask::default()
        };
        assert!(ask.full);
        let manifest = Manifest::load(workspace()).unwrap();
        let rule = manifest
            .rule("lanes.nightly-flags")
            .expect("the manifest has the rule");
        let flags = rule
            .predicate
            .deny_argv
            .as_ref()
            .expect("the rule denies an argv");
        assert!(flags.iter().any(|f| f == "--full"), "{flags:?}");
        assert!(
            rule.predicate.with.iter().any(|w| w == "xtask phpt"),
            "{:?}",
            rule.predicate.with
        );
    }
}
