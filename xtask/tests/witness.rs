//! `xtask witness`, the witness sensor (design §8; harness-design.md §4.3,
//! §4.5 conditions 1 and 2, §8): the fence pairs of a crate's rustdoc, each
//! snippet run under the oracle's recipe and held to what its fences pin,
//! and the kernels and natives without one. Every test here goes through
//! the instrument's `run` and reads what it wrote; how one witness is
//! judged, the notes and the digest, is held beside the judge.
//!
//! The tests that run a snippet through the engine's own binary run the
//! pinned oracle behind a shell file standing where that binary would,
//! since the binary that will be measured does not exist yet; the tests of
//! the rung below it build no binary at all and are served by the engine
//! linked into the harness (#119). They say so on stderr and return when
//! the pinned binary is not on this machine, unless the machine promised
//! it (#38); the rest, the coverage half and the readings of a run with
//! nothing to run, run anywhere.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use xtask::divergence::Registry;
use xtask::manifest::Manifest;
use xtask::oracle::Promise;
use xtask::reading::{Number, Reading, Status};
use xtask::witness::{self, Ask};

mod common;
use common::scratch_workspace;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn manifest() -> Manifest {
    Manifest::load(workspace()).unwrap()
}

/// The negative-control fixture of one manifest rule (harness §4.1).
fn fixture(rule: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/rules")
        .join(rule);
    assert!(dir.is_dir(), "no fixture at {}", dir.display());
    dir
}

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary (#38).
fn oracle() -> Option<PathBuf> {
    let pin = manifest().pin;
    Promise::Oracle
        .or_skip(pin.oracle.file().map(Path::to_path_buf))
        .unwrap()
}

fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}

/// A block whose snippet prints `1` and whose fences pin what `fences` says.
fn block(fences: &str) -> String {
    format!("/// ```php\n/// echo 1;\n/// ```\n{fences}pub fn native() {{}}\n")
}

/// A workspace of this test's own: a git repository holding the tracked
/// manifest, and one crate `ext/fixture` whose `src/lib.rs` is `text`.
/// With `engine`, a shell file at `target/debug/php` that hands every call
/// to the pinned oracle stands where the engine's binary would, so the
/// runner is an engine and not the oracle's own control; without it, no
/// engine is built here.
fn scratch(name: &str, text: &str, engine: bool) -> PathBuf {
    let root = std::env::temp_dir().join(format!("xtask-witness-{name}-{}", std::process::id()));
    scratch_workspace(workspace(), &root).unwrap();
    fs::create_dir_all(root.join("ext/fixture/src")).unwrap();
    fs::write(
        root.join("ext/fixture/Cargo.toml"),
        "[package]\nname = \"php_ext_fixture\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("ext/fixture/src/lib.rs"), text).unwrap();
    if engine {
        stand_in_engine(&root);
    }
    root
}

/// The shell file at `target/debug/php` that runs the pinned oracle; its
/// bytes are not the oracle's, so the runner reads it as the engine.
fn stand_in_engine(root: &Path) {
    let binary = oracle().unwrap();
    let dir = root.join("target/debug");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("php");
    fs::write(
        &file,
        format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", binary.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The manifest a run over the scratch workspace reads: one crate row for
/// the fixture under the tracked pin, and no rule, so nothing is counted
/// as a kernel or native that needs a witness.
fn fixture_manifest() -> Manifest {
    let mut manifest = Manifest::named("ext/fixture");
    manifest.pin = self::manifest().pin;
    manifest
}

/// The one reading of `ext/fixture` over `root`.
fn run(root: &Path, registry: &Registry, accept: bool) -> Reading {
    let readings = witness::run(
        root,
        &fixture_manifest(),
        registry,
        &Ask {
            crates: vec!["ext/fixture".to_owned()],
            accept,
            staged: false,
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    readings.into_iter().next().unwrap()
}

fn count(reading: &Reading, key: &str) -> i64 {
    match reading.numbers.get(key) {
        Some(Number::Int(n)) => *n,
        other => panic!("{key} is {other:?} in {reading:#?}"),
    }
}

/// The divergence key of a witness (harness §4.3): `witness:` and git's
/// blob id of the snippet, as `git hash-object` gives it.
fn witness_id(snippet: &str) -> String {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(snippet.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    format!("witness:{}", String::from_utf8(out.stdout).unwrap().trim())
}

#[test]
fn a_witness_whose_bytes_are_the_oracle_s_passes() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("pass", &block("/// ```expect\n/// 1\n/// ```\n"), true);
    let reading = run(&root, &registry(""), false);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "run"), 1);
    assert_eq!(count(&reading, "passed"), 1);
    assert_eq!(count(&reading, "failed"), 0);
    assert_eq!(count(&reading, "missing"), 0);
}

#[test]
fn a_witness_that_differs_is_a_difference_the_registry_can_accept() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("differs", &block("/// ```expect\n/// 2\n/// ```\n"), true);
    let reading = run(&root, &registry(""), false);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "failed"), 1);
    assert!(
        reading.message.contains("1 of 1 witnesses missed")
            && reading.message.contains("ext/fixture/src/lib.rs:1"),
        "{}",
        reading.message
    );

    let accepted = format!(
        "[[divergence]]\nid = \"{}\"\nkind = \"witness\"\nreason = \"A row a person wrote.\"\nstatus = \"accepted\"\nsince = \"2026-09-09\"\n",
        witness_id("echo 1;")
    );
    let reading = run(&root, &registry(&accepted), false);
    assert_eq!(
        reading.status,
        Status::Pass,
        "an accepted row is not a failure: {reading:#?}"
    );
    assert_eq!(count(&reading, "diverged"), 1);
    assert_eq!(count(&reading, "failed"), 0);
}

#[test]
fn a_bare_exit_fails_the_run_and_accept_is_what_writes_it() {
    if oracle().is_none() {
        return;
    }
    let text =
        "/// ```php\n/// exit(3);\n/// ```\n/// ```expect exit\n/// ```\npub fn native() {}\n";
    let root = scratch("accept", text, true);
    let file = root.join("ext/fixture/src/lib.rs");

    let reading = run(&root, &registry(""), false);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "unpinned"), 1);
    assert_eq!(count(&reading, "accepted"), 0);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        text,
        "a run that does not accept writes nothing"
    );

    let reading = run(&root, &registry(""), true);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "accepted"), 1);
    let written = fs::read_to_string(&file).unwrap();
    assert!(
        written.contains("```expect exit=3"),
        "the oracle's status is on the fence line: {written}"
    );
    // Nothing else of the file moved.
    assert_eq!(written.lines().count(), text.lines().count());
    assert!(written.contains("/// exit(3);"), "{written}");

    // The witness now pins the status, so a second run accepts nothing.
    let again = run(&root, &registry(""), true);
    assert_eq!(again.status, Status::Pass, "{again:#?}");
    assert_eq!(count(&again, "accepted"), 0);
    assert_eq!(fs::read_to_string(&file).unwrap(), written);
}

#[test]
fn accept_writes_the_bare_opt_in_alone_and_never_repins_a_status() {
    if oracle().is_none() {
        return;
    }
    // A fence that pins a status the oracle no longer gives, and one that
    // pins none while the oracle exits 3: both are differences for a person
    // to read, and `--accept` leaves both where they stand.
    for (name, fences) in [
        ("repin-pinned", "/// ```expect exit=0\n/// ```\n".to_owned()),
        ("repin-absent", "/// ```expect\n/// ```\n".to_owned()),
    ] {
        let text = format!("/// ```php\n/// exit(3);\n/// ```\n{fences}pub fn native() {{}}\n");
        let root = scratch(name, &text, true);
        let reading = run(&root, &registry(""), true);
        assert_eq!(reading.status, Status::Fail, "{reading:#?}");
        assert_eq!(count(&reading, "failed"), 1);
        assert_eq!(count(&reading, "accepted"), 0);
        assert_eq!(
            fs::read_to_string(root.join("ext/fixture/src/lib.rs")).unwrap(),
            text,
            "nothing is written"
        );
    }
}

/// The negative control of the manifest's coverage rule (harness §4.1, §12
/// item 4): the fixture named by the rule's id must fail under it, with the
/// rule's own text at the failure. Coverage is a count over the source and
/// needs no php, so the same reading comes with the pinned binary absent
/// (#65), where the fixture's one witness is a snippet that did not run.
#[test]
fn a_kernel_or_native_without_a_witness_is_missing_and_fails_the_crate() {
    let tracked = manifest();
    let tracked = tracked
        .rule_checked_by("witness-coverage")
        .expect("the manifest carries the coverage rule");
    let root = fixture(&tracked.id);
    let manifest = Manifest::load(&root).unwrap();
    let rule = manifest
        .rule_checked_by("witness-coverage")
        .expect("the fixture carries the rule");
    for absent in [false, true] {
        let mut manifest = manifest.clone();
        if absent {
            manifest.pin.oracle.path = root.join("no-php-here");
        } else if oracle().is_none() {
            continue;
        }
        let readings = witness::run(
            &root,
            &manifest,
            &Registry::default(),
            &Ask {
                crates: vec!["ext/pcre".to_owned()],
                accept: false,
                staged: false,
            },
        )
        .unwrap();
        assert_eq!(readings.len(), 1, "{readings:#?}");
        let reading = &readings[0];
        assert_eq!(
            reading.status,
            Status::Fail,
            "absent={absent}: {reading:#?}"
        );
        assert_eq!(reading.rule, tracked.id);
        assert_eq!(count(reading, "missing"), 1);
        assert_eq!(count(reading, "subjects"), 2);
        let ran = i64::from(!absent);
        assert_eq!(count(reading, "run"), ran, "{reading:#?}");
        assert_eq!(count(reading, "not_run"), 1 - ran, "{reading:#?}");
        assert!(
            reading.message.contains("ctype_alpha"),
            "{}",
            reading.message
        );
        assert!(reading.message.contains(&rule.text), "{}", reading.message);
        if absent {
            assert!(
                reading.message.contains("1 witness did not run")
                    && reading.message.contains("no-php-here"),
                "the failure says what did not run and where the pin looked: {}",
                reading.message
            );
        }
    }
}

/// A witness that cannot run for want of the pinned binary is a snippet
/// that did not run, never a refusal: coverage is complete, and the reading
/// is a skip that counts what did not run (#65). Green would be a claim.
#[test]
fn a_crate_whose_witnesses_cannot_run_for_want_of_the_oracle_is_a_skip_never_a_pass() {
    let root = scratch(
        "witness-absent",
        &block("/// ```expect\n/// 1\n/// ```\n"),
        false,
    );
    let mut manifest = fixture_manifest();
    manifest.pin.oracle.path = root.join("no-php-here");
    let readings = witness::run(
        &root,
        &manifest,
        &registry(""),
        &Ask {
            crates: vec!["ext/fixture".to_owned()],
            accept: false,
            staged: false,
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert_eq!(count(reading, "run"), 0);
    assert_eq!(count(reading, "not_run"), 1);
    assert_eq!(count(reading, "missing"), 0);
    assert!(
        reading.message.contains("coverage complete, oracle absent")
            && reading.message.contains("1 witness did not run")
            && reading.message.contains("no-php-here"),
        "{}",
        reading.message
    );
}

#[test]
fn the_reading_carries_run_failed_and_missing_and_is_scoped_to_the_crate() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("scoped", &block("/// ```expect\n/// 1\n/// ```\n"), true);
    let reading = run(&root, &registry(""), false);
    assert_eq!(reading.instrument, "witness");
    assert_eq!(reading.scope, "ext/fixture");
    assert_eq!(reading.status, Status::Pass);
    assert_eq!(count(&reading, "run"), 1);
    assert_eq!(count(&reading, "failed"), 0);
    assert_eq!(count(&reading, "missing"), 0);
    assert!(!reading.tree().is_empty(), "the reading is stamped");
}

/// A checkout with no binary on either target path is still a reading of
/// the engine: the linked engine serves the snippet in this process, the
/// other adapter of design §8's `ScriptExec` seam, so the reading is a
/// pass on its own merits and not the skip that said the engine ran none
/// (#119).
#[test]
fn with_no_binary_built_the_linked_engine_serves_the_witness() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("linked", &block("/// ```expect\n/// 1\n/// ```\n"), false);
    for profile in ["release", "debug"] {
        let binary = root.join("target").join(profile).join("php");
        assert!(!binary.is_file(), "{} stands", binary.display());
    }
    let reading = run(&root, &registry(""), false);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "run"), 1);
    assert_eq!(count(&reading, "passed"), 1);
    assert_eq!(count(&reading, "failed"), 0);
    assert!(
        !reading.message.contains("the engine ran none"),
        "{}",
        reading.message
    );
}

/// And a fail is a fail there: what the linked engine wrote is held to
/// what the fences pin, with no binary standing anywhere (#119).
#[test]
fn with_no_binary_built_a_witness_the_linked_engine_misses_fails() {
    if oracle().is_none() {
        return;
    }
    let root = scratch(
        "linked-differs",
        &block("/// ```expect\n/// 2\n/// ```\n"),
        false,
    );
    let reading = run(&root, &registry(""), false);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "failed"), 1);
    assert!(
        reading.message.contains("1 of 1 witnesses missed"),
        "{}",
        reading.message
    );
}

/// Nothing to run is complete only with the pinned oracle here to have
/// run it: green means the oracle agreed, and without the oracle the pass
/// would be a claim (#60). The engine's absence is another matter: the
/// coverage half was measured whatever stands in for it.
#[test]
fn a_crate_with_no_witness_and_nothing_that_needs_one_passes_with_the_oracle() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("nothing", "pub fn native() {}\n", false);
    let reading = run(&root, &registry(""), false);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "run"), 0);
    assert!(
        reading.message.contains("no witness"),
        "{}",
        reading.message
    );
}

#[test]
fn a_crate_with_no_witness_and_the_oracle_absent_is_a_skip_that_says_so() {
    let root = scratch("nothing-absent", "pub fn native() {}\n", false);
    let mut manifest = fixture_manifest();
    manifest.pin.oracle.path = root.join("no-php-here");
    let readings = witness::run(
        &root,
        &manifest,
        &registry(""),
        &Ask {
            crates: vec!["ext/fixture".to_owned()],
            accept: false,
            staged: false,
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert_eq!(count(reading, "run"), 0);
    assert_eq!(count(reading, "missing"), 0);
    assert!(
        reading.message.contains("coverage complete, oracle absent"),
        "{}",
        reading.message
    );
    assert!(
        reading.message.contains("no-php-here"),
        "the skip names where the pin looked: {}",
        reading.message
    );
}

/// The skip is for a binary that is not here. One that is here and is not
/// the pin's refuses the run, nothing to run or not (§4.3).
#[test]
fn a_crate_with_no_witness_under_a_drifted_oracle_refuses_the_run() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("nothing-drifted", "pub fn native() {}\n", false);
    let mut manifest = fixture_manifest();
    manifest.pin.oracle.md5 = "0".repeat(32);
    let err = witness::run(
        &root,
        &manifest,
        &registry(""),
        &Ask {
            crates: vec!["ext/fixture".to_owned()],
            accept: false,
            staged: false,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("the pin says"), "{err}");
}

/// A kernel without a witness fails the crate whether or not the oracle
/// is here: the coverage half is measured without it, and nothing asserts
/// a pin whose binary is not at the path (#60, #65).
#[test]
fn a_kernel_without_a_witness_fails_whether_or_not_the_oracle_is_here() {
    let root = scratch("uncovered-absent", "pub fn native() {}\n", false);
    let mut manifest = fixture_manifest();
    manifest.rules = self::manifest().rules;
    manifest.pin.oracle.path = root.join("no-php-here");
    let rule = manifest
        .rule_checked_by("witness-coverage")
        .expect("the manifest carries the coverage rule")
        .clone();
    let readings = witness::run(
        &root,
        &manifest,
        &registry(""),
        &Ask {
            crates: vec!["ext/fixture".to_owned()],
            accept: false,
            staged: false,
        },
    )
    .unwrap();
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(reading, "missing"), 1);
    assert_eq!(count(reading, "run"), 0);
    assert!(
        reading.message.contains("native") && reading.message.contains(&rule.text),
        "{}",
        reading.message
    );
}

#[test]
fn a_block_the_grammar_cannot_read_fails_the_crate_where_it_stands() {
    let root = scratch(
        "unread-block",
        "/// ```php\n/// echo 1;\n/// ```\n/// prose\npub fn native() {}\n",
        false,
    );
    let reading = run(&root, &registry(""), false);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "unread"), 1);
    assert!(
        reading.message.contains("ext/fixture/src/lib.rs:1"),
        "{}",
        reading.message
    );
}

#[test]
fn the_instrument_writes_one_reading_per_crate_and_names_an_unknown_one() {
    let manifest = manifest();
    let readings = witness::run(
        workspace(),
        &manifest,
        &Registry::default(),
        &Ask {
            crates: vec!["php_proto".to_owned()],
            accept: false,
            staged: false,
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    assert_eq!(readings[0].scope, "php_proto");

    let unknown = witness::run(
        workspace(),
        &manifest,
        &Registry::default(),
        &Ask {
            crates: vec!["php_nope".to_owned()],
            accept: false,
            staged: false,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(unknown.contains("php_nope"), "{unknown}");
}

#[test]
fn the_harness_crate_carries_no_witness_of_its_own() {
    let manifest = manifest();
    let readings = witness::run(
        workspace(),
        &manifest,
        &Registry::default(),
        &Ask {
            crates: vec!["xtask".to_owned()],
            accept: false,
            staged: false,
        },
    )
    .unwrap();
    assert_eq!(readings[0].status, Status::Skip);
    assert!(
        readings[0].message.contains("carries no witness"),
        "{:#?}",
        readings[0]
    );
}

/// A crate whose sources cannot be read is a failing reading naming the
/// crate and the file, never a crate quietly skipped: `done` asks it for a
/// witness reading, and a pass over sources the grammar never saw would
/// say the crate was measured when it was not.
#[test]
fn a_crate_whose_sources_cannot_be_read_fails_and_names_the_crate() {
    let root = std::env::temp_dir().join(format!("xtask-witness-unread-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("ext/pcre/src")).unwrap();
    fs::write(
        root.join("ext/pcre/Cargo.toml"),
        "[package]\nname = \"php_ext_pcre\"\n",
    )
    .unwrap();
    // Two bytes no text is: the file is there and is not one the fence
    // grammar can read.
    fs::write(root.join("ext/pcre/src/lib.rs"), [0xff, 0xfe]).unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["add", "-A"],
        vec![
            "-c",
            "user.name=x",
            "-c",
            "user.email=x@x",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "scratch",
        ],
    ] {
        let status = Command::new("git")
            .args(&args)
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
    let readings = witness::run(
        &root,
        &manifest(),
        &Registry::default(),
        &Ask {
            crates: vec!["ext/pcre".to_owned()],
            accept: false,
            staged: false,
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading.message.starts_with("ext/pcre:") && reading.message.contains("src/lib.rs"),
        "{}",
        reading.message
    );
}

/// `--staged` is pre-commit's call (harness §6): the crates with a staged
/// change and no other, one reading each, and no reading when no crate is
/// touched, as `clippy --staged` answers.
#[test]
fn staged_narrows_the_run_to_the_crates_with_a_staged_change() {
    let root = scratch("staged", "pub fn native() {}\n", false);
    let staged = |crates: Vec<String>| {
        witness::run(
            &root,
            &fixture_manifest(),
            &Registry::default(),
            &Ask {
                crates,
                accept: false,
                staged: true,
            },
        )
        .unwrap()
    };
    assert!(staged(Vec::new()).is_empty(), "nothing is staged yet");

    let added = Command::new("git")
        .args(["add", "--", "ext/fixture"])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(added.success(), "git add ext/fixture");
    let readings = staged(Vec::new());
    assert_eq!(readings.len(), 1, "{readings:#?}");
    assert_eq!(readings[0].scope, "ext/fixture");
    assert_ne!(readings[0].status, Status::Fail, "{:#?}", readings[0]);
}
