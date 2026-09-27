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

mod common;
use common::scratch_workspace;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn manifest() -> Manifest {
    Manifest::load(workspace()).unwrap()
}

/// A manifest of these `[[crate]]` rows under the tracked pin with its
/// tree the one given: the rows a person writes in `seams.toml`, resolved
/// against a tree without the tracked manifest having to carry the crate
/// they name or this machine the corpus. The caller holds the tree open
/// for as long as it reads the manifest.
fn manifest_of(rows: impl IntoIterator<Item = Crate>, tree: &Path) -> Manifest {
    let mut manifest = Manifest::of(rows);
    manifest.pin = pin_over(tree);
    manifest
}

/// A tree standing where the pinned php-src would, with the directories
/// the per-commit lane's rows name and no test in them.
fn fixture_tree() -> Tree {
    let tree = Tree::new("phpt-tree").unwrap();
    for under in ["Zend/tests", "tests", "sapi/cli/tests", "ext/pcre/tests"] {
        fs::create_dir_all(tree.join(under)).unwrap();
    }
    tree
}

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary (#38).
fn oracle() -> Option<PathBuf> {
    let pin = manifest().pin;
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
    Promise::Tree.or_skip(manifest().pin.source.tree()).unwrap()
}

fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
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
}

impl Scratch {
    fn new(name: &str, tests: &[(&str, &str)]) -> Scratch {
        let root = Tree::new(&format!("phpt-{name}")).unwrap();
        scratch_workspace(workspace(), &root).unwrap();
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
        manifest.pin = pin_over(&corpus);
        Scratch {
            root,
            manifest,
            engine,
        }
    }

    /// The one reading of the fixture's subset, run through `engine`.
    fn run(&self, registry: &Registry, engine: &Path) -> Reading {
        let ask = Ask {
            subset: Some("ext/fixture".to_owned()),
            engine: Some(engine.to_path_buf()),
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
    let pin = manifest().pin.on_this_machine(Some(corpus.to_path_buf()));
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
    let accepted = registry(
        "[[divergence]]\nid = \"phpt:php-src/ext/fixture/tests/missed.phpt\"\nkind = \"phpt\"\nreason = \"A person wrote why this one is allowed to differ.\"\nstatus = \"accepted\"\nsince = \"2026-09-08\"\n",
    );
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
    scratch_workspace(workspace(), &root).unwrap();
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
    let readings = phpt::run(workspace(), &manifest, &registry(""), &ask).unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading.message.contains("Zend/nope") && reading.message.contains("php-8.5.9"),
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
            Crate::named("php_sapi").with_phpt(&["sapi/cli/tests"]),
            Crate::named("ext/pcre"),
            Crate::named("ext/curl").with_feature("curl"),
        ],
        &tree,
    );
    // No engine is built here, so every subset of the lane is a skip that
    // says so; what is under test is which subsets the lane is. §7's
    // per-commit row: Zend/tests and tests/ with php_engine, sapi/cli with
    // php_sapi, and the default extensions' own. A crate behind a feature
    // is not in the default build, and one with no subset owns none.
    let root = Tree::new("phpt-lane").unwrap();
    scratch_workspace(workspace(), &root).unwrap();
    let readings = phpt::run(&root, &manifest, &registry(""), &Ask::default()).unwrap();
    let scopes: Vec<&str> = readings.iter().map(|r| r.scope.as_str()).collect();
    assert_eq!(
        scopes,
        ["php_engine", "php_sapi", "ext/pcre"],
        "{readings:#?}"
    );
}

#[test]
fn a_run_with_no_subset_runs_the_lane_rather_than_refusing() {
    let tree = fixture_tree();
    let manifest = manifest_of([Crate::named("ext/ctype")], &tree);
    let readings = phpt::run(workspace(), &manifest, &registry(""), &Ask::default()).unwrap();
    let scopes: Vec<&str> = readings.iter().map(|r| r.scope.as_str()).collect();
    assert_eq!(scopes, ["ext/ctype"], "{readings:#?}");
}

#[test]
fn the_reading_of_an_extension_subset_is_stamped_over_the_crate_so_done_can_read_it() {
    let tree = fixture_tree();
    let mut manifest = manifest();
    manifest.pin = pin_over(&tree);
    let ask = Ask {
        subset: Some("pcre".to_owned()),
        ..Ask::default()
    };
    let root = Tree::new("phpt-stamp").unwrap();
    scratch_workspace(workspace(), &root).unwrap();
    let readings = phpt::run(&root, &manifest, &registry(""), &ask).unwrap();
    let reading = &readings[0];
    assert_eq!(reading.scope, "ext/pcre");
    assert_eq!(
        reading.tree(),
        Reading::for_scope("phpt", "ext/pcre", &manifest, &root)
            .unwrap()
            .tree(),
        "done reads the reading as fresh only when the tree matches the crate's"
    );
    assert_eq!(
        reading.tree(),
        Reading::for_scope("phpt", "ext/pcre@control", &manifest, &root)
            .unwrap()
            .tree(),
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
    scratch_workspace(workspace(), &root).unwrap();
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
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(&root)
        .args(["phpt", "--subset", "Zend/tests"])
        .env(xtask::oracle::TREE_VARIABLE, &tree)
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stdout.contains("Zend/tests") && !stderr.contains("neither place"),
        "the subset resolved under the fixture:\n{stdout}\n{stderr}"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(&root)
        .args(["phpt", "--subset", "Zend/tests"])
        .env(xtask::oracle::TREE_VARIABLE, root.join("nowhere"))
        .output()
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success());
    assert!(
        stderr.contains(&format!(
            "{}={}",
            xtask::oracle::TREE_VARIABLE,
            root.join("nowhere").display()
        )) && stderr.contains(&pinned.display().to_string()),
        "both places are named: {stderr}"
    );
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
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/phpt/FileName_basic.phpt");
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
        reading.message.contains("cargo build -p php"),
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
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=xtask", "-c", "user.email=xtask@scratch"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", message]);
    git(&["rev-parse", "HEAD"])
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

/// `--against <commit>` (#664): the run a checkout of the merge base kept
/// is found by the commit alone, and a test met there that fails here is
/// named by a reading that fails, while the subset's own reading is
/// written beside it. The controls: the head against itself passes, a
/// binary the tree has moved past gives no verdict, and so does a commit
/// with no run kept, whose subset is then not run at all.
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
            engine: Some(engine.to_path_buf()),
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

    // No run kept at the commit: no verdict, and the subset is not run.
    fs::remove_dir_all(scratch.root.join("target/phpt-runs")).unwrap();
    let readings = run(&scratch.engine, Some(&base));
    assert_eq!(readings.len(), 1, "{readings:#?}");
    assert_eq!(readings[0].scope, "ext/fixture@against");
    assert_eq!(readings[0].status, Status::Skip, "{readings:#?}");
    assert!(
        readings[0]
            .message
            .contains("no run of ext/fixture is kept"),
        "{}",
        readings[0].message
    );
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
            engine: Some(scratch.engine.clone()),
            jobs: std::num::NonZeroUsize::new(jobs),
            ..Ask::default()
        };
        let readings = phpt::run(&scratch.root, &scratch.manifest, &registry(""), &ask).unwrap();
        let mut runs = Vec::new();
        files_under(&scratch.root.join("target/phpt-runs"), &mut runs);
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

/// Two runs over one subset at once, from two checkouts, each a process of
/// its own over the one tree the variable names (#990): each reads every
/// test as a lone run does, and keeps the lone run's bytes. Each test
/// writes a file named for its `__FILE__` beside itself, sleeps, and reads
/// it back, so a run whose test stood beside the other's same test would
/// read `theirs`, or bork on the other's program still standing; and each
/// prints `__FILE__` and `__DIR__` by name and a sibling included by
/// `__DIR__` and by the include path, which are run-tests.php's. The lone
/// run first is the control the two are held to.
#[test]
fn two_runs_over_one_subset_at_once_each_read_as_a_lone_run_does() {
    if oracle().is_none() {
        return;
    }
    let corpus = Tree::new("phpt-apart-corpus").unwrap();
    let dir = corpus.join("Zend/tests");
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
    let engine = Tree::new("phpt-apart-engine").unwrap();
    let engine = engine.join("php");
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
    let checkout = |name: &str| {
        let root = Tree::new(&format!("phpt-apart-{name}")).unwrap();
        scratch_workspace(workspace(), &root).unwrap();
        commit_all(&root, name);
        root
    };
    let run = |root: &Tree| {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .arg("--root")
            .arg(root.path())
            .args(["phpt", "--subset", "Zend/tests", "--jobs", "2", "--engine"])
            .arg(&engine)
            .env(xtask::oracle::TREE_VARIABLE, corpus.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    // The kept run with its commit taken out, the one field two checkouts
    // of one tree tell apart.
    let kept = |root: &Tree, child: std::process::Child| {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let mut runs = Vec::new();
        files_under(&root.join("target/phpt-runs"), &mut runs);
        assert_eq!(runs.len(), 1, "{runs:?}");
        let mut run: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&runs[0]).unwrap()).unwrap();
        run.as_object_mut().unwrap().remove("commit").unwrap();
        run
    };
    let lone = checkout("lone");
    let alone = kept(&lone, run(&lone));
    let verdicts = alone["verdicts"].as_object().unwrap();
    assert!(
        verdicts.len() == 4 && verdicts.values().all(|v| v == "pass"),
        "the lone run passes all four: {alone}"
    );
    let (one, two) = (checkout("one"), checkout("two"));
    let (first, second) = (run(&one), run(&two));
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
